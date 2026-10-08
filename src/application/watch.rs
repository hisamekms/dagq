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
    application::{
        AskStore, Clock, EventReads, ProcessControl, RunCoordination, RunLog, SessionRegistry,
        SupervisorRegistry, WorkspaceBackend,
    },
    domain::{
        ATTENTION_KINDS, EventFilter, EventId, RunEvent, RunId, SessionRole, UPDATE_EVENT_KINDS,
        event_attention,
        event_kind::ASK_OPENED,
        timeline::{self, Gap},
        wakes_inbox,
    },
};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{
    thread,
    time::{Duration, Instant},
};

/// At most this many attention events come back from one `watch` without
/// a role or for the planner; the cursor then points at the last one
/// returned. The inbox's watch has no limit: it comes back with every
/// attention past the cursor, the notices that did not wake it included,
/// so none waits past the wake for another (ADR-t1418-1).
const WATCH_LIMIT: usize = 100;

/// How many events one read of the queue takes at most.
const PAGE: usize = 100;

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

/// Whether an attention event wakes a `watch` for `role`: for the inbox, not
/// the notices [`wakes_inbox`] leaves for its next wake (ADR-t1418-1); for
/// any other role or none, every one.
fn wakes(role: Option<SessionRole>, event: &RunEvent) -> bool {
    role != Some(SessionRole::Inbox) || wakes_inbox(&event.kind, &event.payload)
}

/// The events [`read_events`] read: those kept, the cursor to continue
/// from, whether one kept wakes a `watch` for the role, and the
/// `ask_opened` among those kept, which the inbox's watch notifies.
struct Read {
    events: Vec<Value>,
    cursor: EventId,
    woken: bool,
    opened: Vec<RunEvent>,
}

/// Up to `query.limit` events with `query.after < id <= upto` that its
/// filter keeps, oldest first, and the cursor to continue from: the last
/// event returned when the limit was reached, `upto` otherwise. Without
/// `all` and without kinds in the filter, only the attention events for
/// `role`.
fn read_events(
    queue: &dyn EventReads,
    query: &EventsQuery,
    upto: EventId,
    role: Option<SessionRole>,
) -> Result<Read> {
    let (after, limit) = (query.after, query.limit.max(1));
    let attention = !query.all && query.filter.kinds.is_none();
    let filter = EventFilter {
        kinds: if attention {
            Some(attention_kinds())
        } else {
            query.filter.kinds.clone()
        },
        ..query.filter.clone()
    };
    let mut events = Vec::new();
    let mut woken = false;
    let mut opened = Vec::new();
    let mut cursor = after;
    loop {
        // An attention kind can still be dropped by its payload, so pages
        // are read until the limit is filled or the range is exhausted.
        let page = queue.events_between(cursor, upto, &filter, limit.min(PAGE))?;
        if page.is_empty() {
            return Ok(Read {
                events,
                cursor: upto,
                woken,
                opened,
            });
        }
        for event in page {
            cursor = event.id;
            if !attention
                || (event_attention(&event.kind, &event.payload).is_some() && for_role(role))
            {
                woken |= wakes(role, &event);
                events.push(shown(&event, query.full));
                if event.kind == ASK_OPENED {
                    opened.push(event);
                }
                if events.len() == limit {
                    return Ok(Read {
                        events,
                        cursor,
                        woken,
                        opened,
                    });
                }
            }
        }
    }
}

fn attention_kinds() -> Vec<String> {
    ATTENTION_KINDS
        .iter()
        .map(|&kind| kind.to_owned())
        .collect()
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

/// `events` with its filters and `--full`: the events after `query.after`
/// that the query keeps, oldest first, and the cursor to continue from.
pub fn events_in(queue: &(impl RunLog + EventReads), query: &EventsQuery) -> Result<Value> {
    let upto = queue.latest_event_id()?;
    let Read { events, cursor, .. } = read_events(queue, query, upto, None)?;
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
/// `gap_secs` with their reasons (`domain::timeline`), the time since the
/// last event of a run that still moves on read from `clock`.
pub fn timeline_in(
    queue: &(impl RunLog + EventReads),
    run: &RunId,
    gap_secs: i64,
    full: bool,
    clock: &dyn Clock,
) -> Result<Value> {
    let task_run = queue.run(run)?;
    let events = queue.run_events(run)?;
    // The latest run of an unfinished task still moves on, even `failed` or
    // `interrupted` while it waits for triage: the time since its last event
    // is a gap too.
    let moving = queue
        .latest_runs_in_progress()?
        .iter()
        .any(|latest| latest.id() == run);
    let now_ms = moving.then(|| clock.now() * 1000);
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
/// it count, the supervisors' health included. For the inbox, the notices
/// that do not wake it ([`wakes_inbox`]: `update_installed` and the hourly
/// review, ADR-t1418-1) do not end the wait on their own; when something
/// else does, they come back among the events, oldest first, all of them
/// past the cursor with no limit. A timeout
/// returns no events and the cursor unchanged; without one
/// (`--until-attention`) it returns only when something came. An error
/// reading the queue ends it at once, never retried. Never writes and never
/// integrates.
///
/// A `watch --role inbox` also leaves the record of itself that says the
/// inbox has a watcher (ADR-t906-1, ADR-t1433-5 decision 1 (1)): `record`,
/// started by the caller, whose heartbeat is renewed at every read of the
/// queue and whose end is written when it returns. A record that cannot be
/// written does not stop the watch. It also tells the person of each
/// `ask_opened` it returns, once, through `notifier` (ADR-t1433-1 decision
/// 2): the watch runs in the inbox's session, so the notification needs
/// nothing of the supervisor's. One that fails is only warned of.
pub fn watch(
    queue: &(impl RunLog + RunCoordination + SupervisorRegistry + EventReads),
    clock: &dyn Clock,
    control: &dyn ProcessControl,
    mut record: Option<&mut dyn WatchRecord>,
    notifier: Option<&dyn AskNotifier>,
    options: &WatchOptions,
) -> Result<Value> {
    let after = match options.after {
        Some(after) => after,
        None => queue.latest_event_id()?,
    };
    let baseline = pulses(&queue.supervisors()?, clock.now(), control);
    let query = EventsQuery {
        after,
        limit: if options.role == Some(SessionRole::Inbox) {
            usize::MAX
        } else {
            WATCH_LIMIT
        },
        all: false,
        full: false,
        filter: EventFilter::default(),
    };
    let deadline = options.timeout.map(|timeout| Instant::now() + timeout);
    loop {
        let upto = queue.latest_event_id()?;
        let Read {
            events,
            cursor,
            woken,
            opened,
        } = read_events(queue, &query, upto, options.role)?;
        let registrations = queue.supervisors()?;
        let now = clock.now();
        if let Some(record) = record.as_deref_mut() {
            let _ = record.heartbeat(now);
        }
        let changed = for_role(options.role) && pulses(&registrations, now, control) != baseline;
        let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        if woken || changed || remaining.is_some_and(|r| r.is_zero()) {
            // A timeout with nothing that woke it keeps the notices for the
            // next watch.
            let (events, cursor) = if woken || changed {
                (events, cursor)
            } else {
                (Vec::new(), after)
            };
            if let Some(record) = record.as_deref_mut() {
                let _ = record.end(now);
            }
            if woken || changed {
                notify_opened(options.role, &opened, notifier);
            }
            return Ok(json!({
                "events": events,
                "supervisors_changed": changed,
                "supervisors": supervisors(&registrations, &queue.run_leases()?, now, control),
                "cursor": cursor,
            }));
        }
        thread::sleep(remaining.map_or(options.interval, |r| options.interval.min(r)));
    }
}

/// How the inbox's watch tells the person of a new ask (ADR-t1433-1
/// decision 2): a notification aimed at the inbox, never keystrokes. The
/// implementation is the inbox's port, the cmux of the inbox's session.
pub trait AskNotifier {
    /// Tell the person of the ask that `opened` (an `ask_opened`) opened.
    fn notify(&self, opened: &RunEvent) -> Result<()>;
}

/// Tell the person of each of `opened`, the `ask_opened` a watch returns,
/// once and only for the inbox: a watch of another role, or none, tells
/// nobody. A failed notification is warned of and stops nothing.
fn notify_opened(
    role: Option<SessionRole>,
    opened: &[RunEvent],
    notifier: Option<&dyn AskNotifier>,
) {
    let Some(notifier) = notifier.filter(|_| role == Some(SessionRole::Inbox)) else {
        return;
    };
    for event in opened {
        if let Err(error) = notifier.notify(event) {
            tracing::warn!(error = %format_args!("{error:#}"), "the person could not be notified of ask {}: {error:#}", event.payload["ask_id"]);
        }
    }
}

/// The inbox's [`AskNotifier`] (ADR-t1433-1 decision 2): one notification
/// of the ask as the queue holds it, aimed at the inbox's workspace `up`
/// recorded (at none when there is none), sent through `backend`, the cmux
/// of the inbox's session. `checkout` names the repository in the title.
pub struct InboxNotifier<'a, Q> {
    pub queue: &'a Q,
    pub backend: &'a dyn WorkspaceBackend,
    pub checkout: std::path::PathBuf,
}

impl<Q: AskStore + SessionRegistry> AskNotifier for InboxNotifier<'_, Q> {
    fn notify(&self, opened: &RunEvent) -> Result<()> {
        let Some(id) = opened.payload["ask_id"].as_i64() else {
            bail!("ask_opened names no ask");
        };
        let ask = self.queue.read_ask(crate::domain::AskId::new(id))?;
        let (title, body) = ask_notice(&self.checkout, &ask);
        let inbox = self.queue.session_workspace(SessionRole::Inbox)?;
        self.backend.notify(&title, &body, inbox.as_deref())
    }
}

/// The title and body of the notification of `ask` (ADR-0022 decision 5):
/// `[<repo>] ask #<id> <kind>`, and its question cut at
/// [`NOTIFY_QUESTION_CHARS`] with its task and run.
pub fn ask_notice(repo_root: &std::path::Path, ask: &crate::domain::Ask) -> (String, String) {
    let mut body = super::health::truncate(&ask.question, NOTIFY_QUESTION_CHARS)
        .unwrap_or_else(|| ask.question.clone());
    // An observer's blocked ask may belong to no task (and then no run).
    if let Some(task_id) = ask.task_id {
        body.push_str(&format!("\ntask {task_id}"));
        if let Some(run_id) = &ask.run_id {
            body.push_str(&format!(" run {run_id}"));
        }
    }
    (super::naming::ask_notification_title(repo_root, ask), body)
}

/// Characters of an ask's question the notification keeps before `…`.
const NOTIFY_QUESTION_CHARS: usize = 200;

/// The record a `watch --role inbox` keeps of itself (ADR-t906-1).
pub trait WatchRecord {
    /// Renew the heartbeat at `now` (unix seconds).
    fn heartbeat(&mut self, now: i64) -> Result<()>;
    /// Write the end at `now` (unix seconds).
    fn end(&mut self, now: i64) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Events in memory, read as the queue reads them: in the range, of
    /// the filter's kinds, at most `limit`.
    struct Events(Vec<RunEvent>);

    impl EventReads for Events {
        fn events_between(
            &self,
            after: EventId,
            upto: EventId,
            filter: &EventFilter,
            limit: usize,
        ) -> Result<Vec<RunEvent>> {
            Ok(self
                .0
                .iter()
                .filter(|event| event.id > after && event.id <= upto)
                .filter(|event| {
                    filter
                        .kinds
                        .as_ref()
                        .is_none_or(|kinds| kinds.contains(&event.kind))
                })
                .take(limit)
                .cloned()
                .collect())
        }
    }

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: "2026-10-04T00:00:00Z".to_owned(),
            actor: None,
        }
    }

    fn query(after: i64, limit: usize, all: bool) -> EventsQuery {
        EventsQuery {
            after: EventId::new(after),
            limit,
            all,
            full: false,
            filter: EventFilter::default(),
        }
    }

    fn ids(read: &Read) -> Vec<i64> {
        read.events
            .iter()
            .map(|event| event["id"].as_i64().unwrap())
            .collect()
    }

    /// Without `all`, only the attention events: an attention kind its
    /// payload drops (a failed review with its ask) and another kind are
    /// read past, and the cursor stops at the last one kept when the limit
    /// is reached, at `upto` otherwise.
    #[test]
    fn the_attention_events_are_read_past_what_the_payload_drops() {
        let events = Events(vec![
            event(1, "triage_failed", json!({})),
            event(2, "review_failed", json!({"ask_id": 9})),
            event(3, "note_added", json!({})),
            event(4, "triage_failed", json!({})),
            event(5, "triage_failed", json!({})),
        ]);
        let upto = EventId::new(5);
        let read = read_events(&events, &query(0, 2, false), upto, None).unwrap();
        assert_eq!(ids(&read), [1, 4]);
        assert_eq!(read.cursor, EventId::new(4));
        assert!(read.woken);
        let read = read_events(&events, &query(0, 10, false), upto, None).unwrap();
        assert_eq!(ids(&read), [1, 4, 5]);
        assert_eq!(read.cursor, upto);
        let read = read_events(&events, &query(0, 2, true), upto, None).unwrap();
        assert_eq!(ids(&read), [1, 2]);
        assert_eq!(read.cursor, EventId::new(2));
        let read = read_events(&events, &query(5, 2, false), upto, None).unwrap();
        assert!(ids(&read).is_empty());
        assert_eq!(read.cursor, upto);
        assert!(!read.woken);
    }

    /// A notifier that records the asks it was told of, failing on `fail`.
    struct Told(std::sync::Mutex<Vec<i64>>, Option<i64>);

    impl AskNotifier for Told {
        fn notify(&self, opened: &RunEvent) -> Result<()> {
            let id = opened.payload["ask_id"].as_i64().unwrap();
            self.0.lock().unwrap().push(id);
            if self.1 == Some(id) {
                bail!("cmux is gone");
            }
            Ok(())
        }
    }

    /// The inbox's watch tells the person of each `ask_opened` it returns,
    /// once, and of nothing else (ADR-t1433-1 decision 2); one that fails
    /// does not stop the others; a watch of another role, or none, tells
    /// nobody; and a read past an ask does not keep it for the next watch.
    #[test]
    fn the_inbox_watch_notifies_each_ask_opened_it_returns_once() {
        let events = Events(vec![
            event(1, "ask_opened", json!({"ask_id": 7, "kind": "decide"})),
            event(2, "triage_failed", json!({})),
            event(3, "ask_opened", json!({"ask_id": 8, "kind": "stalled"})),
        ]);
        let upto = EventId::new(3);
        let read = read_events(
            &events,
            &query(0, 10, false),
            upto,
            Some(SessionRole::Inbox),
        )
        .unwrap();
        let ids = |opened: &[RunEvent]| -> Vec<i64> {
            opened
                .iter()
                .map(|event| event.payload["ask_id"].as_i64().unwrap())
                .collect()
        };
        assert_eq!(ids(&read.opened), [7, 8]);
        let told = Told(Default::default(), Some(7));
        notify_opened(Some(SessionRole::Inbox), &read.opened, Some(&told));
        assert_eq!(*told.0.lock().unwrap(), [7, 8]);
        let other = Told(Default::default(), None);
        notify_opened(Some(SessionRole::Planner), &read.opened, Some(&other));
        notify_opened(None, &read.opened, Some(&other));
        assert!(other.0.lock().unwrap().is_empty());
        notify_opened(Some(SessionRole::Inbox), &read.opened, None);
        let past = read_events(
            &events,
            &query(3, 10, false),
            upto,
            Some(SessionRole::Inbox),
        )
        .unwrap();
        assert!(past.opened.is_empty());
    }

    #[test]
    fn an_ask_notice_names_the_repository_and_cuts_the_question() {
        use crate::domain::{Ask, AskKind};
        let mut ask: Ask = serde_json::from_value(json!({
            "id": 4, "kind": AskKind::Blocked, "task_id": null, "run_id": null,
            "question": "slots idle", "options": [], "answer": null, "asked_by": "observer",
            "reason_category": "scope", "created_at": 0, "answered_at": null, "closed_at": null
        }))
        .unwrap();
        let (title, body) = ask_notice(std::path::Path::new("/src/dagq"), &ask);
        assert_eq!(title, "[dagq] ask #4 blocked");
        assert_eq!(body, "slots idle");
        ask.task_id = Some(crate::domain::TaskId::new(1));
        ask.question = "長".repeat(250);
        let (_, body) = ask_notice(std::path::Path::new("/src/dagq"), &ask);
        assert_eq!(body, format!("{}…\ntask 1", "長".repeat(200)));
    }

    #[test]
    fn an_event_time_is_a_utc_timestamp_or_an_error() {
        assert_eq!(event_time("2026-10-04").unwrap(), "2026-10-04T00:00:00Z");
        assert_eq!(
            event_time("2026-10-04T01:02:03").unwrap(),
            "2026-10-04T01:02:03Z"
        );
        assert!(event_time("2026-13-04").is_err());
        assert!(event_time("yesterday").is_err());
    }
}
