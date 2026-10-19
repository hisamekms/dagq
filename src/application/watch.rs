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
        Clock, CoordinationReads, EventStore, ProcessControl, RunLog, SupervisorReads,
        push::{self, PushOutcome, PushRequest, PushTarget},
    },
    domain::{
        ATTENTION_KINDS, EventFilter, EventId, RunEvent, RunId, SessionRole, UPDATE_EVENT_KINDS,
        event_attention,
        event_kind::ASK_OPENED,
        kpi::push::{PushConfig, ask_opened_message},
        timeline::{self, Gap},
        wakes_inbox,
    },
};
use anyhow::{Result, anyhow, bail, ensure};
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
    queue: &dyn EventStore,
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
pub fn events_in(queue: &impl EventStore, query: &EventsQuery) -> Result<Value> {
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
    queue: &(impl RunLog + ?Sized),
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
    /// It is a `run_events` id whatever the role, so watches of different
    /// roles may share it.
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
/// written does not stop the watch. It also tells the person of the
/// `ask_opened` it returns, once, through `notifier` (ADR-t2159-1 decision
/// 4): the watch runs in the inbox's session, so the notification needs
/// nothing of the supervisor's. One that fails stops nothing; the output's
/// `ask_push` says how it went ([`notify_opened`]).
pub fn watch(
    queue: &(impl RunLog + CoordinationReads + SupervisorReads),
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
            let pushed = if woken || changed {
                notify_opened(options.role, &opened, notifier)
            } else {
                None
            };
            let mut value = json!({
                "events": events,
                "supervisors_changed": changed,
                "supervisors": supervisors(&registrations, &queue.run_leases()?, now, control),
                "cursor": cursor,
            });
            if let Some(pushed) = pushed {
                value["ask_push"] = pushed;
            }
            return Ok(value);
        }
        thread::sleep(remaining.map_or(options.interval, |r| options.interval.min(r)));
    }
}

/// How the inbox's watch tells the person of new asks (ADR-t2159-1
/// decision 4): a message out of the host, never keystrokes nor a wake of
/// a session.
pub trait AskNotifier {
    /// Tell the person that `opened`, the `ask_opened` one watch returns
    /// (never empty), opened that many asks.
    fn notify(&self, opened: &[RunEvent]) -> Result<()>;
}

/// Tell the person of `opened`, the `ask_opened` a watch returns, in one
/// notification and only for the inbox: a watch of another role, one with
/// no notifier (no `[push]`) or one that returns no new ask tells nobody,
/// and the next watch reads past these asks. What was done, for the
/// output's `ask_push`: `asks` and `sent`, with the `error` of one that
/// failed, which is also warned of and stops nothing; `None` when nothing
/// was tried.
fn notify_opened(
    role: Option<SessionRole>,
    opened: &[RunEvent],
    notifier: Option<&dyn AskNotifier>,
) -> Option<Value> {
    let notifier = notifier.filter(|_| role == Some(SessionRole::Inbox))?;
    if opened.is_empty() {
        return None;
    }
    Some(match notifier.notify(opened) {
        Ok(()) => json!({"asks": opened.len(), "sent": true}),
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "the person could not be told of {} new ask(s): {error:#}", opened.len());
            json!({"asks": opened.len(), "sent": false, "error": format!("{error:#}")})
        }
    })
}

/// What a [`PushNotifier`] says when the host's `[push]` could not be
/// read: the reading's own error is not shown, since its text may quote
/// the command's arguments.
pub const PUSH_UNREADABLE: &str = "host.toml's [push] could not be read or is not valid";

/// The notifier of new asks a watch of `role` has (ADR-t2159-1 decision
/// 4): only the inbox's, and only when `load` (the host's `[push]` as it
/// reads now, read only for the inbox) finds one, sending through
/// `run_push` about `target` (made only then). A `[push]` that cannot be
/// read still makes one, whose notifications fail with
/// [`PUSH_UNREADABLE`], so the watch goes on and its output says why.
pub fn push_notifier(
    role: Option<SessionRole>,
    load: impl FnOnce() -> Result<Option<PushConfig>>,
    target: impl FnOnce() -> PushTarget,
    run_push: fn(&PushRequest) -> PushOutcome,
) -> Option<PushNotifier> {
    if role != Some(SessionRole::Inbox) {
        return None;
    }
    let config = match load() {
        Ok(None) => return None,
        Ok(Some(config)) => Ok(config),
        Err(_) => Err(PUSH_UNREADABLE.to_owned()),
    };
    Some(PushNotifier {
        config,
        target: target(),
        run_push,
    })
}

/// The inbox's [`AskNotifier`] (ADR-t2159-1 decision 4): one message of
/// kind `ask_opened` ([`ask_opened_message`]) through the host's `[push]`
/// command, with the title and the count of the asks and nothing of their
/// content. [`push_notifier`] makes it.
pub struct PushNotifier {
    /// The host's `[push]`, or why it could not be read (then every
    /// notification fails with that).
    pub config: std::result::Result<PushConfig, String>,
    /// The queue's name and database the message is about.
    pub target: PushTarget,
    /// Runs the push command once.
    pub run_push: fn(&PushRequest) -> PushOutcome,
}

impl AskNotifier for PushNotifier {
    fn notify(&self, opened: &[RunEvent]) -> Result<()> {
        let config = self
            .config
            .as_ref()
            .map_err(|error| anyhow!("[push] could not be read: {error}"))?;
        let through = opened.last().map_or(0, |event| event.id.as_i64());
        let message =
            ask_opened_message(&self.target.name, &self.target.queue, through, opened.len());
        let outcome = (self.run_push)(&push::request(config, &self.target, &message));
        ensure!(
            outcome.success,
            "the push command failed on the ask_opened message: {}",
            push::failure_reason(config, &outcome)
        );
        Ok(())
    }
}

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
    use crate::domain::EventKind;

    /// Events in memory, read as the queue reads them: in the range, of
    /// the filter's kinds, at most `limit`.
    struct Events(Vec<RunEvent>);

    #[allow(unused_variables)]
    impl EventStore for Events {
        fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
            unreachable!("update_events is not what the test reaches")
        }
        fn all_events(&self) -> Result<Vec<RunEvent>> {
            unreachable!("all_events is not what the test reaches")
        }
        fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
            unreachable!("latest_task_events is not what the test reaches")
        }
        fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
            unreachable!("run_events is not what the test reaches")
        }
        fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool> {
            unreachable!("has_run_event is not what the test reaches")
        }
        fn record_runtime_event(
            &self,
            id: &RunId,
            kind: EventKind,
            payload: serde_json::Value,
        ) -> Result<()> {
            unreachable!("record_runtime_event is not what the test reaches")
        }
        fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
            unreachable!("last_observe is not what the test reaches")
        }
        fn latest_event_id(&self) -> Result<EventId> {
            unreachable!("latest_event_id is not what the test reaches")
        }
        fn record_backend_failure(
            &self,
            run: Option<&RunId>,
            payload: serde_json::Value,
        ) -> Result<()> {
            unreachable!("record_backend_failure is not what the test reaches")
        }
        fn record_queue_event(
            &self,
            kind: EventKind,
            payload: serde_json::Value,
        ) -> Result<EventId> {
            unreachable!("record_queue_event is not what the test reaches")
        }
        fn latest_event_of(&self, kind: &str) -> Result<Option<RunEvent>> {
            unreachable!("latest_event_of is not what the test reaches")
        }
        fn latest_events_of(&self, kind: &str, limit: usize) -> Result<Vec<RunEvent>> {
            unreachable!("latest_events_of is not what the test reaches")
        }
        fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>> {
            unreachable!("latest_queue_event is not what the test reaches")
        }
        fn latest_events_by_supervisor(
            &self,
            kinds: &[&str],
            supervisors: &[&str],
        ) -> Result<Vec<RunEvent>> {
            unreachable!("latest_events_by_supervisor is not what the test reaches")
        }
        fn events_of_between(
            &self,
            kinds: &[&str],
            after: EventId,
            upto: EventId,
            limit: usize,
        ) -> Result<Vec<RunEvent>> {
            unreachable!("events_of_between is not what the test reaches")
        }
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

    /// A notifier that records each notification's asks, failing when
    /// `fail`.
    struct Told(std::sync::Mutex<Vec<Vec<i64>>>, bool);

    impl AskNotifier for Told {
        fn notify(&self, opened: &[RunEvent]) -> Result<()> {
            let ids = opened
                .iter()
                .map(|event| event.payload["ask_id"].as_i64().unwrap())
                .collect();
            self.0.lock().unwrap().push(ids);
            ensure!(!self.1, "the push command failed");
            Ok(())
        }
    }

    /// The inbox's watch tells the person of the `ask_opened` it returns in
    /// one notification, and of nothing else (ADR-t2159-1 decision 4); one
    /// that fails is reported in `ask_push` and stops nothing; a watch of
    /// another role, or none, or without a notifier (no `[push]`), tells
    /// nobody; and a read past an ask does not keep it for the next watch.
    #[test]
    fn the_inbox_watch_notifies_the_new_asks_it_returns_once() {
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
        let told = Told(Default::default(), false);
        assert_eq!(
            notify_opened(Some(SessionRole::Inbox), &read.opened, Some(&told)),
            Some(json!({"asks": 2, "sent": true}))
        );
        assert_eq!(*told.0.lock().unwrap(), [vec![7, 8]]);
        let failing = Told(Default::default(), true);
        assert_eq!(
            notify_opened(Some(SessionRole::Inbox), &read.opened, Some(&failing)),
            Some(json!({"asks": 2, "sent": false, "error": "the push command failed"}))
        );
        let other = Told(Default::default(), false);
        assert_eq!(
            notify_opened(Some(SessionRole::Planner), &read.opened, Some(&other)),
            None
        );
        assert_eq!(notify_opened(None, &read.opened, Some(&other)), None);
        assert!(other.0.lock().unwrap().is_empty());
        assert_eq!(
            notify_opened(Some(SessionRole::Inbox), &read.opened, None),
            None
        );
        let past = read_events(
            &events,
            &query(3, 10, false),
            upto,
            Some(SessionRole::Inbox),
        )
        .unwrap();
        assert!(past.opened.is_empty());
        assert_eq!(
            notify_opened(Some(SessionRole::Inbox), &past.opened, Some(&other)),
            None
        );
        assert!(other.0.lock().unwrap().is_empty());
    }

    /// The command's run of the last [`PushNotifier`] notification.
    static PUSHED: std::sync::Mutex<Option<PushRequest>> = std::sync::Mutex::new(None);

    fn pushed_ok(request: &PushRequest) -> PushOutcome {
        *PUSHED.lock().unwrap() = Some(request.clone());
        PushOutcome {
            success: true,
            ..PushOutcome::default()
        }
    }

    fn pushed_failing(_: &PushRequest) -> PushOutcome {
        PushOutcome {
            exit_code: Some(4),
            stderr: "posting to https://hooks.example/secret failed".into(),
            ..PushOutcome::default()
        }
    }

    /// [`PushNotifier`] hands `[push]`'s command one `ask_opened` message
    /// with the count and the queue, and nothing of the asks (their ids
    /// are not in it either); a command that fails, or a `[push]` that
    /// could not be read, fails the notification with why, the command's
    /// arguments masked.
    #[test]
    fn the_push_notifier_sends_one_ask_opened_message_without_the_asks() {
        let target = PushTarget {
            name: "dagq".into(),
            queue: "/q/queue.db".into(),
        };
        let command = vec![
            "/bin/push".to_owned(),
            "https://hooks.example/secret".to_owned(),
        ];
        let opened = [
            event(
                5,
                "ask_opened",
                json!({"ask_id": 70, "question": "secret?"}),
            ),
            event(
                9,
                "ask_opened",
                json!({"ask_id": 80, "question": "secret?"}),
            ),
        ];
        let notifier = PushNotifier {
            config: Ok(PushConfig::new(command.clone())),
            target: target.clone(),
            run_push: pushed_ok,
        };
        notifier.notify(&opened).unwrap();
        let request = PUSHED.lock().unwrap().take().unwrap();
        assert_eq!(request.command, command);
        assert!(
            request
                .env
                .contains(&("DAGQ_PUSH_KIND".to_owned(), "ask_opened".to_owned()))
        );
        let body: Value = serde_json::from_slice(&request.stdin).unwrap();
        assert_eq!(body["kind"], "ask_opened");
        assert_eq!(body["new_asks"], 2);
        assert_eq!(body["queue"], "/q/queue.db");
        assert_eq!(body["period"], "through event 9");
        let text = String::from_utf8(request.stdin).unwrap();
        assert!(!text.contains("secret") && !text.contains("70"), "{text}");

        let failing = PushNotifier {
            run_push: pushed_failing,
            ..notifier
        };
        let error = format!("{:#}", failing.notify(&opened).unwrap_err());
        assert!(error.contains("exit code 4"), "{error}");
        assert!(!error.contains("hooks.example"), "{error}");
        let unreadable = PushNotifier {
            config: Err("bad host.toml".into()),
            target,
            run_push: pushed_ok,
        };
        let error = format!("{:#}", unreadable.notify(&opened).unwrap_err());
        assert!(error.contains("bad host.toml"), "{error}");
        assert!(PUSHED.lock().unwrap().is_none());
    }

    /// Only the inbox's watch has a notifier, and only with a `[push]`:
    /// another role's or none reads no `[push]`, a missing one makes none
    /// (and names no target), and one that cannot be read makes one that
    /// fails without the reading's error, which may quote the command.
    #[test]
    fn only_the_inbox_with_a_push_has_a_notifier() {
        use std::cell::Cell;
        let loads = Cell::new(0);
        let targets = Cell::new(0);
        let target = || {
            targets.set(targets.get() + 1);
            PushTarget {
                name: "dagq".into(),
                queue: "/q/queue.db".into(),
            }
        };
        let config = || PushConfig::new(vec!["/bin/push".to_owned()]);
        for role in [None, Some(SessionRole::Planner)] {
            let made = push_notifier(
                role,
                || {
                    loads.set(loads.get() + 1);
                    Ok(Some(config()))
                },
                target,
                pushed_ok,
            );
            assert!(made.is_none());
        }
        assert_eq!((loads.get(), targets.get()), (0, 0));

        let none = push_notifier(Some(SessionRole::Inbox), || Ok(None), target, pushed_ok);
        assert!(none.is_none());
        assert_eq!(targets.get(), 0);

        let made = push_notifier(
            Some(SessionRole::Inbox),
            || Ok(Some(config())),
            target,
            pushed_ok,
        )
        .unwrap();
        assert_eq!(made.config, Ok(config()));
        assert_eq!(made.target.queue, "/q/queue.db");

        let unreadable = push_notifier(
            Some(SessionRole::Inbox),
            || {
                Err(anyhow!(
                    "bad [push]: command = [\"https://hooks.example/secret\"]"
                ))
            },
            target,
            pushed_ok,
        )
        .unwrap();
        assert_eq!(unreadable.config, Err(PUSH_UNREADABLE.to_owned()));
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
