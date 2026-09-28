//! The inbox's notification path (ADR-0016, ADR-0024 decision 6):
//! `events --after` that reads `run_events` past a cursor, and `watch` that
//! blocks until an attention event arrives or the supervisors' health
//! changes. `events`' filters and `--full` and `timeline RUN` read the same
//! events for people and the jobs (ADR-0044 decision 22). The attention `status` derives from the queue as it is now is
//! [`crate::application::health::attention`]. Everything here reads the
//! queue and writes nothing; which transition is an attention is decided
//! by `domain`.
pub use crate::application::health::compact_event;
use crate::{
    application::health::{for_role, pulses, supervisors},
    domain::{
        ATTENTION_KINDS, EventFilter, EventId, RunEvent, RunId, SessionRole, UPDATE_EVENT_KINDS,
        event_attention,
        timeline::{self, Gap},
    },
    infrastructure::{adapters::SystemProcesses, inbox_watchers, sqlite::SqliteQueue},
};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

/// At most this many attention events come back from one `watch`; the
/// cursor then points at the last one returned.
const WATCH_LIMIT: usize = 100;

/// At most this many steps of the automatic update show in one `timeline`.
const TIMELINE_UPDATES_LIMIT: usize = 200;

/// An event as `events` and `timeline` print it: compact (ADR-0016) or,
/// with `full`, every field with its whole payload.
fn shown(event: &RunEvent, full: bool) -> Value {
    if full {
        json!(event)
    } else {
        compact_event(event)
    }
}

/// Up to `query.limit` events with `query.after < id <= upto` that its
/// filter keeps, oldest first, and the cursor to continue from: the last
/// event returned when the limit was reached, `upto` otherwise. Without
/// `all` and without kinds in the filter, only the attention events for
/// `role`.
fn read_events(
    queue: &SqliteQueue,
    query: &EventsQuery,
    upto: EventId,
    role: Option<SessionRole>,
) -> Result<(Vec<Value>, EventId)> {
    let (after, limit) = (query.after, query.limit.max(1));
    let attention = !query.all && query.filter.kinds.is_none();
    let filter = EventFilter {
        kinds: if attention {
            Some(
                ATTENTION_KINDS
                    .iter()
                    .map(|&kind| kind.to_owned())
                    .collect(),
            )
        } else {
            query.filter.kinds.clone()
        },
        ..query.filter.clone()
    };
    let mut events = Vec::new();
    let mut cursor = after;
    loop {
        // An attention kind can still be dropped by its payload, so pages
        // are read until the limit is filled or the range is exhausted.
        let page = queue.events_between(cursor, upto, &filter, limit)?;
        if page.is_empty() {
            return Ok((events, upto));
        }
        for event in page {
            cursor = event.id;
            if !attention
                || (event_attention(&event.kind, &event.payload).is_some() && for_role(role))
            {
                events.push(shown(&event, query.full));
                if events.len() == limit {
                    return Ok((events, cursor));
                }
            }
        }
    }
}

/// What `events` reads and how it prints it.
#[derive(Debug, Clone)]
pub struct EventsQuery {
    pub after: EventId,
    pub limit: usize,
    /// Every kind, not only attention.
    pub all: bool,
    /// Every field and the whole payload instead of the compact form.
    pub full: bool,
    pub filter: EventFilter,
}

/// `events --after`: the events after `after`, oldest first, in compact
/// form with no filter.
pub fn events(db: &Path, after: EventId, limit: usize, all: bool) -> Result<Value> {
    events_matching(
        db,
        &EventsQuery {
            after,
            limit,
            all,
            full: false,
            filter: EventFilter::default(),
        },
    )
}

/// `events` with its filters and `--full`: the events after `query.after`
/// that the query keeps, oldest first.
pub fn events_matching(db: &Path, query: &EventsQuery) -> Result<Value> {
    events_in(&SqliteQueue::open_read_only(db)?, query)
}

/// [`events_matching`] on a queue the caller already opened, so a command
/// opens it once.
pub fn events_in(queue: &SqliteQueue, query: &EventsQuery) -> Result<Value> {
    let upto = queue.latest_event_id()?;
    let (events, cursor) = read_events(queue, query, upto, None)?;
    Ok(json!({"events": events, "cursor": cursor}))
}

/// A `--since` / `--until` of `events` as a queue timestamp: `YYYY-MM-DD`
/// (its midnight) or `YYYY-MM-DDTHH:MM:SS[.fff][Z]`, in UTC.
pub fn event_time(text: &str) -> Result<String> {
    let text = text.trim();
    let time = if text.len() == 10 {
        format!("{text}T00:00:00Z")
    } else if text.ends_with('Z') {
        text.to_owned()
    } else {
        format!("{text}Z")
    };
    if !well_formed(&time) {
        bail!("not a UTC time (YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS[.fff]Z): {text}");
    }
    Ok(time)
}

/// Whether `time` is `YYYY-MM-DDTHH:MM:SS[.f+]Z` with every field in range,
/// the form SQLite's `julianday` reads (a malformed one would compare as
/// NULL and match nothing).
fn well_formed(time: &str) -> bool {
    let shape = b"dddd-dd-ddTdd:dd:dd";
    if time.len() <= shape.len()
        || !time.ends_with('Z')
        || !shape
            .iter()
            .zip(time.bytes())
            .all(|(&want, got)| match want {
                b'd' => got.is_ascii_digit(),
                _ => got == want,
            })
    {
        return false;
    }
    let fraction = &time[shape.len()..time.len() - 1];
    let number = |range: std::ops::Range<usize>| time[range].parse::<u32>().unwrap_or(99);
    (fraction.is_empty()
        || (fraction.len() > 1
            && fraction.starts_with('.')
            && fraction[1..].bytes().all(|b| b.is_ascii_digit())))
        && (1..=12).contains(&number(5..7))
        && (1..=31).contains(&number(8..10))
        && number(11..13) < 24
        && number(14..16) < 60
        && number(17..19) < 60
}

/// `timeline RUN`: the run's events oldest first and its gaps of at least
/// `gap_secs` with their reasons (`domain::timeline`).
pub fn timeline(db: &Path, run: &RunId, gap_secs: i64, full: bool) -> Result<Value> {
    timeline_in(&SqliteQueue::open_read_only(db)?, run, gap_secs, full)
}

/// [`timeline`] on a queue the caller already opened, so a command opens
/// it once.
pub fn timeline_in(queue: &SqliteQueue, run: &RunId, gap_secs: i64, full: bool) -> Result<Value> {
    let task_run = queue.run(run)?;
    let events = queue.run_events(run)?;
    // The latest run of an unfinished task still moves on, even `failed` or
    // `interrupted` while it waits for triage: the time since its last event
    // is a gap too.
    let moving = queue
        .latest_runs_in_progress()?
        .iter()
        .any(|latest| latest.id() == run);
    let now_ms = moving.then(|| queue.generators().clock.now() * 1000);
    let gaps: Vec<Gap> = timeline::gaps(&events, gap_secs, now_ms);
    let waited: i64 = gaps.iter().map(|gap| gap.secs).sum();
    // The steps of the automatic update from the run's first event to its
    // last (to now while it moves on) show among its events: a handoff
    // explains a pause of the supervisor. The gaps are the run's own.
    let shown_events = match (events.first(), events.last()) {
        (Some(first), Some(last)) => {
            let upto = if moving {
                queue.latest_event_id()?
            } else {
                last.id
            };
            let updates = queue.events_between(
                EventId::new(first.id.as_i64() - 1),
                upto,
                &EventFilter {
                    kinds: Some(UPDATE_EVENT_KINDS.iter().map(|&k| k.to_owned()).collect()),
                    ..EventFilter::default()
                },
                TIMELINE_UPDATES_LIMIT,
            )?;
            timeline::merged(&events, &updates)
        }
        _ => events.clone(),
    };
    Ok(json!({
        "run_id": run,
        "task_id": task_run.task_id(),
        "status": task_run.status(),
        // The providers the run asked for and ran on, and its moves
        // between them (ADR-t813-2).
        "requested_provider": task_run.requested_provider(),
        "actual_provider": task_run.actual_provider(),
        "worker_mode": task_run.worker_mode(),
        "provider_switches": timeline::provider_switches(&events),
        "gap_secs": gap_secs,
        "events": shown_events.iter().map(|event| shown(event, full)).collect::<Vec<_>>(),
        "gaps": gaps,
        "gap_total_secs": waited,
        "commands": timeline::heavy_commands(&events),
    }))
}

#[derive(Debug, Clone)]
pub struct WatchOptions {
    /// Cursor to wait past; `None` is the newest event when `watch` starts.
    pub after: Option<EventId>,
    /// How long to wait before returning with no events; `None`
    /// (`--until-attention`) waits until something comes, however long.
    pub timeout: Option<Duration>,
    pub interval: Duration,
    /// Only the attention addressed to this role (ADR-0022); `None` is all.
    pub role: Option<SessionRole>,
}

/// Block until an attention event past the cursor exists or the health of
/// the registered supervisors (the set of tokens, their PIDs, `alive` and
/// `stale`) differs from what it was when `watch` started, reading the queue
/// every `interval`. With a `role`, only the attention events addressed to
/// it count, the supervisors' health included. A timeout
/// returns no events and the cursor unchanged; without one
/// (`--until-attention`) it returns only when something came. An error
/// reading the queue ends it at once, never retried. Never writes and never
/// integrates.
///
/// A `watch --role inbox` also leaves the record of itself that says the
/// inbox has a watcher (ADR-t906-1): written when it starts, its heartbeat
/// renewed at every read of the queue, its end written when it returns, in
/// a file under the queue's directory, never in the queue. A record that
/// cannot be written does not stop the watch.
pub fn watch(db: &Path, options: &WatchOptions) -> Result<Value> {
    let queue = SqliteQueue::open(db)?;
    let clock = queue.generators().clock.clone();
    let mut record = (options.role == Some(SessionRole::Inbox))
        .then(|| {
            let now = clock.system_time();
            inbox_watchers::WatcherFile::start(
                &inbox_watchers::dir(db),
                std::process::id(),
                clock.now(),
                now.duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as i64),
                options
                    .timeout
                    .map(|timeout| i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX / 4)),
                i64::try_from(options.interval.as_secs()).unwrap_or(i64::MAX / 4),
            )
            .inspect_err(|error| {
                tracing::warn!(error = %format_args!("{error:#}"), "the inbox watcher's record could not be written: {error:#}");
            })
            .ok()
        })
        .flatten();
    let after = match options.after {
        Some(after) => after,
        None => queue.latest_event_id()?,
    };
    let baseline = pulses(
        &queue.supervisors()?,
        queue.generators().clock.now(),
        &SystemProcesses,
    );
    let query = EventsQuery {
        after,
        limit: WATCH_LIMIT,
        all: false,
        full: false,
        filter: EventFilter::default(),
    };
    let deadline = options.timeout.map(|timeout| Instant::now() + timeout);
    loop {
        let upto = queue.latest_event_id()?;
        let (events, cursor) = read_events(&queue, &query, upto, options.role)?;
        let registrations = queue.supervisors()?;
        let now = queue.generators().clock.now();
        if let Some(record) = record.as_mut() {
            let _ = record.heartbeat(now);
        }
        let changed =
            for_role(options.role) && pulses(&registrations, now, &SystemProcesses) != baseline;
        let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        if !events.is_empty() || changed || remaining.is_some_and(|r| r.is_zero()) {
            let cursor = if events.is_empty() && !changed {
                after
            } else {
                cursor
            };
            if let Some(record) = record.as_mut() {
                let _ = record.end(now);
            }
            return Ok(json!({
                "events": events,
                "supervisors_changed": changed,
                "supervisors": supervisors(&registrations, &queue.run_leases()?, now, &SystemProcesses),
                "cursor": cursor,
            }));
        }
        thread::sleep(remaining.map_or(options.interval, |r| options.interval.min(r)));
    }
}
