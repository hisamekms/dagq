//! The inbox's notification path (ADR-0016, ADR-0024 decision 6):
//! `events --after` that reads `run_events` past a cursor, and `watch` that
//! blocks until an attention event arrives or the supervisors' health
//! changes. `events`' filters and `--full` and `timeline RUN` read the same
//! events for people and the jobs (ADR-0044 decision 22). The attention `status` derives from the queue as it is now is
//! [`crate::application::health::attention`]. Everything here reads the
//! queue and writes nothing; which transition is an attention is decided
//! by `domain`.
pub use crate::application::health::compact_event;
use crate::domain::EventKind;
use crate::{
    application::health::{for_role, pulses, supervisors},
    application::{
        Clock, CoordinationReads, EventStore, ProcessControl, RunLog, SupervisorReads,
        push::{self, PushOutcome, PushRequest, PushTarget},
    },
    domain::{
        ATTENTION_KINDS, EventFilter, EventId, RunEvent, RunId, SessionRole, UPDATE_EVENT_KINDS,
        event_attention,
        kpi::push::{PushConfig, ask_opened_message},
        timeline::{self, Gap},
        wakes_inbox,
    },
};
use anyhow::{Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{thread, time::Duration};

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
                if event.kind == EventKind::AskOpened {
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
                    kinds: Some(
                        UPDATE_EVENT_KINDS
                            .iter()
                            .map(|kind| kind.as_str().to_owned())
                            .collect(),
                    ),
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

/// One look of a `watch` at the queue past its cursor: the attention for
/// its role up to the newest event now, and the supervisors' health
/// (ADR-t3102-3 decision 2). The queue service answers one as `watch_poll`
/// for a client-mode watch, which waits between its looks itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchLook {
    /// The cursor looked past: the one asked for, or the newest event when
    /// none was.
    pub after: EventId,
    /// The events kept for the role, oldest first, as `watch` prints them.
    pub events: Vec<Value>,
    /// The cursor to continue from.
    pub cursor: EventId,
    /// Whether one kept wakes a `watch` for the role ([`wakes_inbox`]).
    pub woken: bool,
    /// The `ask_opened` among those kept, which the inbox's watch notifies.
    pub opened: Vec<RunEvent>,
    /// The registered supervisors' health as `watch` compares it: the set
    /// of tokens, their PIDs, `alive` and `stale`.
    pub pulses: Value,
    /// The supervisors as `watch` prints them.
    pub supervisors: Value,
    /// The Git common directory the queue is bound to, which the inbox's
    /// notification names it after wherever the watch runs; `None` for a
    /// queue bound to none, and from a service that does not give it.
    #[serde(default)]
    pub binding: Option<String>,
}

/// One look at `queue` past `after` (`None`: the newest event now) for
/// `role`: what [`watch`] reads at each of its reads of the queue.
pub fn look(
    queue: &(impl RunLog + CoordinationReads + SupervisorReads),
    clock: &dyn Clock,
    control: &dyn ProcessControl,
    after: Option<EventId>,
    role: Option<SessionRole>,
) -> Result<WatchLook> {
    let upto = queue.latest_event_id()?;
    let after = after.unwrap_or(upto);
    let query = EventsQuery {
        after,
        limit: if role == Some(SessionRole::Inbox) {
            usize::MAX
        } else {
            WATCH_LIMIT
        },
        all: false,
        full: false,
        filter: EventFilter::default(),
    };
    let Read {
        events,
        cursor,
        woken,
        opened,
    } = read_events(queue, &query, upto.max(after), role)?;
    let registrations = queue.supervisors()?;
    let now = clock.now();
    Ok(WatchLook {
        after,
        events,
        cursor,
        woken,
        opened,
        pulses: serde_json::to_value(pulses(&registrations, now, control))?,
        supervisors: serde_json::to_value(supervisors(
            &registrations,
            &queue.run_leases()?,
            now,
            control,
        ))?,
        // A binding that cannot be read names the queue as one bound to
        // none, and stops no look.
        binding: queue.repository_binding().ok().flatten(),
    })
}

/// Where a `watch` takes its looks: the queue it opened, or the queue
/// service, whose `watch_poll` a client-mode watch calls. An error ends
/// the watch at once: a service that does not answer is returned as that,
/// never as a timeout or no events, but the same recorded outage only once
/// ([`OutageNotices`]).
pub trait WatchSource {
    /// One look past `after` (`None`: the newest event now) for `role`,
    /// taking `limit` at most: a service that has not answered by then
    /// fails the look as one that does not answer ([`look_limit`]).
    fn look(
        &self,
        after: Option<EventId>,
        role: Option<SessionRole>,
        limit: Duration,
    ) -> Result<WatchLook>;

    /// What the source knows of the outages of the queue's service a look
    /// meets ([`OutageNotices`]); `None` for one that has no service, the
    /// queue a watch opened.
    fn outages(&self) -> Option<&dyn OutageNotices> {
        None
    }

    /// Wait `duration` before the next look while a watch waits out an
    /// outage a watch of its role returned already.
    fn wait_out(&self, duration: Duration) {
        thread::sleep(duration);
    }
}

/// The outages of the queue's service a host's watch meets, as the
/// supervisor recorded them outside the DB, and which of them a watch of
/// each role returned already (ADR-t3102-3 decisions 1 and 2).
pub trait OutageNotices {
    /// The start (unix seconds) of the outage the supervisor recorded,
    /// when `error`, a look's, says the queue's service did not answer and
    /// one stands; `None` for any other error, or no record.
    fn recorded(&self, error: &anyhow::Error) -> Option<i64>;
    /// Whether a watch of `role` returned the outage that began at `since`.
    fn told(&self, role: Option<SessionRole>, since: i64) -> bool;
    /// That a watch of `role` returns the outage that began at `since` now.
    fn tell(&self, role: Option<SessionRole>, since: i64) -> Result<()>;
}

/// The queue a `watch` opened, as its [`WatchSource`].
pub struct QueueWatch<'a, Q> {
    pub queue: &'a Q,
    pub clock: &'a dyn Clock,
    pub control: &'a dyn ProcessControl,
}

impl<Q> WatchSource for QueueWatch<'_, Q>
where
    Q: RunLog + CoordinationReads + SupervisorReads,
{
    fn look(
        &self,
        after: Option<EventId>,
        role: Option<SessionRole>,
        _limit: Duration,
    ) -> Result<WatchLook> {
        look(self.queue, self.clock, self.control, after, role)
    }
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
    record: Option<&mut dyn WatchRecord>,
    notifier: Option<&dyn AskNotifier>,
    options: &WatchOptions,
) -> Result<Value> {
    watch_through(
        &QueueWatch {
            queue,
            clock,
            control,
        },
        clock,
        record,
        notifier,
        options,
    )
}

/// [`watch`] with its looks taken from `source`: the supervisors' health
/// of its first look is the one later looks are compared with. A look
/// that meets an outage of the queue's service ends the watch with that
/// failure once per outage and role; a later watch of the role waits it
/// out ([`look_through`]).
pub fn watch_through(
    source: &dyn WatchSource,
    clock: &dyn Clock,
    mut record: Option<&mut dyn WatchRecord>,
    notifier: Option<&dyn AskNotifier>,
    options: &WatchOptions,
) -> Result<Value> {
    let started = clock.monotonic();
    let first = match look_through(source, clock, &mut record, options, started, options.after)? {
        Looked::Look(look) => look,
        Looked::WaitedOut { since } => {
            return Ok(waited_out(&mut record, clock, options.after, since));
        }
    };
    let after = first.after;
    let baseline = first.pulses.clone();
    let mut next = Some(first);
    loop {
        let look = match next.take() {
            Some(look) => look,
            None => {
                match look_through(source, clock, &mut record, options, started, Some(after))? {
                    Looked::Look(look) => look,
                    Looked::WaitedOut { since } => {
                        return Ok(waited_out(&mut record, clock, Some(after), since));
                    }
                }
            }
        };
        let now = clock.now();
        if let Some(record) = record.as_deref_mut() {
            let _ = record.heartbeat(now);
        }
        let changed = for_role(options.role) && look.pulses != baseline;
        let remaining = options
            .timeout
            .map(|timeout| super::time_left(clock.since(started), timeout));
        let woken = look.woken;
        if woken || changed || remaining.is_some_and(|r| r.is_zero()) {
            // A timeout with nothing that woke it keeps the notices for the
            // next watch.
            let (events, cursor) = if woken || changed {
                (look.events, look.cursor)
            } else {
                (Vec::new(), after)
            };
            if let Some(record) = record.as_deref_mut() {
                let _ = record.end(now);
            }
            let pushed = if woken || changed {
                notify_opened(
                    options.role,
                    &look.opened,
                    look.binding.as_deref(),
                    notifier,
                )
            } else {
                None
            };
            let mut value = json!({
                "events": events,
                "supervisors_changed": changed,
                "supervisors": look.supervisors,
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

/// A look of a watch, or the timeout that passed while it waited out an
/// outage of the queue's service that began at `since`.
enum Looked {
    Look(WatchLook),
    WaitedOut { since: i64 },
}

/// The longest one look of a watch, or the host's `status`, may take when
/// its timeout leaves longer or it has none: a service that takes the call
/// and does not answer is not answering after it (ADR-t3102-3 decision 2).
pub const LOOK_LIMIT: Duration = Duration::from_secs(30);

/// The least one look is given: a watch's last look starts as its
/// timeout passes (and `--timeout 0`'s only one at once), and a service
/// that answers it in a moment is not taken for one that does not answer.
/// A watch of a service that does not answer ends this long past its
/// timeout at most.
pub const LOOK_FLOOR: Duration = Duration::from_secs(3);

/// How long one look may take with `remaining` of the watch's timeout
/// left (`None`: it has none): what is left, within [`LOOK_FLOOR`] and
/// [`LOOK_LIMIT`].
pub fn look_limit(remaining: Option<Duration>) -> Duration {
    remaining.map_or(LOOK_LIMIT, |remaining| {
        remaining.clamp(LOOK_FLOOR, LOOK_LIMIT)
    })
}

/// What a watch does with a look its queue's service did not answer
/// (ADR-t3102-3 decision 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailedLook {
    /// End the watch with the failure: no outage is recorded.
    Fail,
    /// End the watch with the failure, and note that a watch of its role
    /// returned the outage that began at `since`.
    Tell { since: i64 },
    /// Look again after this long: a watch of its role returned the
    /// outage already.
    Wait(Duration),
    /// The timeout passed while the watch waited out the outage that began
    /// at `since`.
    WaitedOut { since: i64 },
}

/// The step after a failed look: `recorded` is the start of the outage the
/// supervisor recorded (`None`: none), `told` whether a watch of the role
/// returned it, `remaining` the time left of the watch's timeout (`None`:
/// it has none), and `interval` its wait between looks.
fn failed_look(
    recorded: Option<i64>,
    told: bool,
    remaining: Option<Duration>,
    interval: Duration,
) -> FailedLook {
    let Some(since) = recorded else {
        return FailedLook::Fail;
    };
    if !told {
        return FailedLook::Tell { since };
    }
    match remaining {
        Some(remaining) if remaining.is_zero() => FailedLook::WaitedOut { since },
        Some(remaining) => FailedLook::Wait(interval.min(remaining)),
        None => FailedLook::Wait(interval),
    }
}

/// One look of `source` past `after` for the watch of `options` started
/// at `started`, and what [`failed_look`] says to do with one the queue's
/// service does not answer: end the watch with the failure (noting the
/// recorded outage as told the first time a watch of its role meets it),
/// or wait it out, looking every interval, until the service answers or
/// another outage is recorded, and be [`Looked::WaitedOut`] once its
/// timeout passed meanwhile.
fn look_through(
    source: &dyn WatchSource,
    clock: &dyn Clock,
    record: &mut Option<&mut dyn WatchRecord>,
    options: &WatchOptions,
    started: std::time::Instant,
    after: Option<EventId>,
) -> Result<Looked> {
    let remaining = || {
        options
            .timeout
            .map(|timeout| super::time_left(clock.since(started), timeout))
    };
    loop {
        let error = match source.look(after, options.role, look_limit(remaining())) {
            Ok(look) => return Ok(Looked::Look(look)),
            Err(error) => error,
        };
        let outages = source.outages();
        let recorded = outages.and_then(|outages| outages.recorded(&error));
        let told = outages
            .zip(recorded)
            .is_some_and(|(outages, since)| outages.told(options.role, since));
        match failed_look(recorded, told, remaining(), options.interval) {
            FailedLook::Fail => return Err(error),
            FailedLook::Tell { since } => {
                if let Some(Err(told)) = outages.map(|outages| outages.tell(options.role, since)) {
                    tracing::warn!(error = %format_args!("{told:#}"), "that the watch returned the queue service's outage could not be noted: {told:#}");
                }
                return Err(error);
            }
            FailedLook::WaitedOut { since } => return Ok(Looked::WaitedOut { since }),
            FailedLook::Wait(wait) => {
                if let Some(record) = record.as_deref_mut() {
                    let _ = record.heartbeat(clock.now());
                }
                source.wait_out(wait);
            }
        }
    }
}

/// The output of a watch whose timeout passed while it waited out the
/// outage that began at `since`: no events, its cursor unchanged, and the
/// outage it waited on.
fn waited_out(
    record: &mut Option<&mut dyn WatchRecord>,
    clock: &dyn Clock,
    cursor: Option<EventId>,
    since: i64,
) -> Value {
    if let Some(record) = record.as_deref_mut() {
        let _ = record.end(clock.now());
    }
    json!({
        "events": [],
        "supervisors_changed": false,
        "supervisors": null,
        "cursor": cursor,
        "outage_since": since,
    })
}

/// How the inbox's watch tells the person of new asks (ADR-t2159-1
/// decision 4): a message out of the host, never keystrokes nor a wake of
/// a session.
pub trait AskNotifier {
    /// Tell the person that `opened`, the `ask_opened` one watch returns
    /// (never empty), opened that many asks in the queue bound to
    /// `binding` ([`WatchLook::binding`]).
    fn notify(&self, opened: &[RunEvent], binding: Option<&str>) -> Result<()>;
}

/// Tell the person of `opened`, the `ask_opened` a watch returns of the
/// queue bound to `binding`, in one notification and only for the inbox:
/// a watch of another role, one with no notifier (no `[push]`) or one that
/// returns no new ask tells nobody, and the next watch reads past these
/// asks. What was done, for the output's `ask_push`: `asks` and `sent`,
/// with the `error` of one that failed, which is also warned of and stops
/// nothing; `None` when nothing was tried.
fn notify_opened(
    role: Option<SessionRole>,
    opened: &[RunEvent],
    binding: Option<&str>,
    notifier: Option<&dyn AskNotifier>,
) -> Option<Value> {
    let notifier = notifier.filter(|_| role == Some(SessionRole::Inbox))?;
    if opened.is_empty() {
        return None;
    }
    Some(match notifier.notify(opened, binding) {
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
/// `run_push` about what `target` makes of the queue's binding at each
/// notification. A `[push]` that cannot be read still makes one, whose
/// notifications fail with [`PUSH_UNREADABLE`], so the watch goes on and
/// its output says why.
pub fn push_notifier<'a>(
    role: Option<SessionRole>,
    load: impl FnOnce() -> Result<Option<PushConfig>>,
    target: impl Fn(Option<&str>) -> PushTarget + 'a,
    run_push: fn(&PushRequest) -> PushOutcome,
) -> Option<PushNotifier<'a>> {
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
        target: Box::new(target),
        run_push,
    })
}

/// What a [`PushNotifier`]'s message is about, made at each notification
/// from the Git common directory the queue is bound to as the look that
/// returned the asks read it ([`WatchLook::binding`]).
pub type TargetOfBinding<'a> = Box<dyn Fn(Option<&str>) -> PushTarget + 'a>;

/// The inbox's [`AskNotifier`] (ADR-t2159-1 decision 4): one message of
/// kind `ask_opened` ([`ask_opened_message`]) through the host's `[push]`
/// command, with the title and the count of the asks and nothing of their
/// content. [`push_notifier`] makes it.
pub struct PushNotifier<'a> {
    /// The host's `[push]`, or why it could not be read (then every
    /// notification fails with that).
    pub config: std::result::Result<PushConfig, String>,
    /// The queue's name and database the message is about.
    pub target: TargetOfBinding<'a>,
    /// Runs the push command once.
    pub run_push: fn(&PushRequest) -> PushOutcome,
}

impl AskNotifier for PushNotifier<'_> {
    fn notify(&self, opened: &[RunEvent], binding: Option<&str>) -> Result<()> {
        let config = self
            .config
            .as_ref()
            .map_err(|error| anyhow!("[push] could not be read: {error}"))?;
        let target = (self.target)(binding);
        let through = opened.last().map_or(0, |event| event.id.as_i64());
        let message = ask_opened_message(&target.name, &target.queue, through, opened.len());
        let outcome = (self.run_push)(&push::request(config, &target, &message));
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
                        .is_none_or(|kinds| kinds.iter().any(|kind| kind == event.kind.as_str()))
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
            kind: kind.into(),
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

    /// A notifier that records each notification's asks and the binding
    /// it was given, failing when `fail`.
    struct Told(std::sync::Mutex<Vec<(Vec<i64>, Option<String>)>>, bool);

    impl AskNotifier for Told {
        fn notify(&self, opened: &[RunEvent], binding: Option<&str>) -> Result<()> {
            let ids = opened
                .iter()
                .map(|event| event.payload["ask_id"].as_i64().unwrap())
                .collect();
            self.0
                .lock()
                .unwrap()
                .push((ids, binding.map(str::to_owned)));
            ensure!(!self.1, "the push command failed");
            Ok(())
        }
    }

    /// The inbox's watch tells the person of the `ask_opened` it returns in
    /// one notification, and of nothing else (ADR-t2159-1 decision 4), with
    /// the binding of the queue the look read; one
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
            notify_opened(
                Some(SessionRole::Inbox),
                &read.opened,
                Some("/repo/.git"),
                Some(&told)
            ),
            Some(json!({"asks": 2, "sent": true}))
        );
        assert_eq!(
            *told.0.lock().unwrap(),
            [(vec![7, 8], Some("/repo/.git".to_owned()))]
        );
        let failing = Told(Default::default(), true);
        assert_eq!(
            notify_opened(Some(SessionRole::Inbox), &read.opened, None, Some(&failing)),
            Some(json!({"asks": 2, "sent": false, "error": "the push command failed"}))
        );
        let other = Told(Default::default(), false);
        assert_eq!(
            notify_opened(Some(SessionRole::Planner), &read.opened, None, Some(&other)),
            None
        );
        assert_eq!(notify_opened(None, &read.opened, None, Some(&other)), None);
        assert!(other.0.lock().unwrap().is_empty());
        assert_eq!(
            notify_opened(Some(SessionRole::Inbox), &read.opened, None, None),
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
            notify_opened(Some(SessionRole::Inbox), &past.opened, None, Some(&other)),
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
    /// with the count and the queue, titled with the name its target makes
    /// of the binding it is given, and nothing of the asks (their ids
    /// are not in it either); a command that fails, or a `[push]` that
    /// could not be read, fails the notification with why, the command's
    /// arguments masked.
    #[test]
    fn the_push_notifier_sends_one_ask_opened_message_without_the_asks() {
        let target = |binding: Option<&str>| PushTarget {
            name: binding.map_or("unbound", |_| "bound").to_owned(),
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
            target: Box::new(target),
            run_push: pushed_ok,
        };
        notifier.notify(&opened, Some("/repo/.git")).unwrap();
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
        assert_eq!(body["title"], "bound: 2 new ask(s) for the inbox");
        let text = String::from_utf8(request.stdin).unwrap();
        assert!(!text.contains("secret") && !text.contains("70"), "{text}");
        notifier.notify(&opened, None).unwrap();
        let request = PUSHED.lock().unwrap().take().unwrap();
        let body: Value = serde_json::from_slice(&request.stdin).unwrap();
        assert_eq!(body["title"], "unbound: 2 new ask(s) for the inbox");

        let failing = PushNotifier {
            run_push: pushed_failing,
            ..notifier
        };
        let error = format!("{:#}", failing.notify(&opened, None).unwrap_err());
        assert!(error.contains("exit code 4"), "{error}");
        assert!(!error.contains("hooks.example"), "{error}");
        let unreadable = PushNotifier {
            config: Err("bad host.toml".into()),
            target: Box::new(target),
            run_push: pushed_ok,
        };
        let error = format!("{:#}", unreadable.notify(&opened, None).unwrap_err());
        assert!(error.contains("bad host.toml"), "{error}");
        assert!(PUSHED.lock().unwrap().is_none());
    }

    /// Only the inbox's watch has a notifier, and only with a `[push]`:
    /// another role's or none reads no `[push]`, a missing one makes none,
    /// and one that cannot be read makes one that fails without the
    /// reading's error, which may quote the command. None names its target
    /// before it notifies.
    #[test]
    fn only_the_inbox_with_a_push_has_a_notifier() {
        use std::cell::Cell;
        let loads = Cell::new(0);
        let targets = Cell::new(0);
        let target = |_: Option<&str>| {
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
        assert_eq!(targets.get(), 0);
        assert_eq!((made.target)(None).queue, "/q/queue.db");

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

    /// A look takes what is left of the watch's timeout, never less than
    /// the floor nor more than the limit, and the limit with no timeout.
    #[test]
    fn a_look_takes_what_is_left_of_the_timeout_within_the_floor_and_the_limit() {
        let second = Duration::from_secs(1);
        assert_eq!(look_limit(None), LOOK_LIMIT);
        assert_eq!(look_limit(Some(10 * second)), 10 * second);
        assert_eq!(look_limit(Some(Duration::ZERO)), LOOK_FLOOR);
        assert_eq!(look_limit(Some(LOOK_FLOOR / 2)), LOOK_FLOOR);
        assert_eq!(look_limit(Some(LOOK_LIMIT * 10)), LOOK_LIMIT);
    }

    /// ADR-t3102-3 decision 2: a look the service did not answer ends the
    /// watch when no outage is recorded, and the first time a watch of its
    /// role meets a recorded one (to be noted as told); after that the
    /// watch waits it out, the interval or what is left of its timeout,
    /// until the timeout passes, and with no timeout it waits on.
    #[test]
    fn a_recorded_outage_is_returned_once_per_role_and_waited_out_after() {
        let second = Duration::from_secs(1);
        let interval = Duration::from_secs(5);
        for told in [false, true] {
            assert_eq!(
                failed_look(None, told, Some(second), interval),
                FailedLook::Fail
            );
        }
        assert_eq!(
            failed_look(Some(100), false, Some(Duration::ZERO), interval),
            FailedLook::Tell { since: 100 }
        );
        assert_eq!(
            failed_look(Some(100), false, None, interval),
            FailedLook::Tell { since: 100 }
        );
        assert_eq!(
            failed_look(Some(100), true, Some(60 * second), interval),
            FailedLook::Wait(interval)
        );
        assert_eq!(
            failed_look(Some(100), true, Some(second), interval),
            FailedLook::Wait(second)
        );
        assert_eq!(
            failed_look(Some(100), true, None, interval),
            FailedLook::Wait(interval)
        );
        assert_eq!(
            failed_look(Some(100), true, Some(Duration::ZERO), interval),
            FailedLook::WaitedOut { since: 100 }
        );
    }

    /// A clock stopped at its instant: a watch with no timeout never reads
    /// time passing.
    struct Stopped(std::time::Instant);

    impl Clock for Stopped {
        fn system_time(&self) -> std::time::SystemTime {
            std::time::UNIX_EPOCH + Duration::from_secs(1_790_000_000)
        }
        fn monotonic(&self) -> std::time::Instant {
            self.0
        }
    }

    /// A look the queue's service did not answer while the outage that
    /// began at its `since` stood.
    #[derive(Debug)]
    struct Down(i64);

    impl std::fmt::Display for Down {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "the queue service did not answer")
        }
    }

    impl std::error::Error for Down {}

    /// A service whose looks answer in turn as `looks` says (`Some(since)`:
    /// not answered in the outage that began at `since`; `None`: answered),
    /// the outage a watch of the inbox returned, and the waits between the
    /// looks, recorded rather than slept.
    struct Outages {
        looks: std::sync::Mutex<std::collections::VecDeque<Option<i64>>>,
        told: std::sync::Mutex<Option<i64>>,
        waits: std::sync::Mutex<Vec<Duration>>,
    }

    impl Outages {
        fn new(told: i64, looks: &[Option<i64>]) -> Self {
            Self {
                looks: std::sync::Mutex::new(looks.iter().copied().collect()),
                told: std::sync::Mutex::new(Some(told)),
                waits: std::sync::Mutex::default(),
            }
        }

        fn left(&self) -> usize {
            self.looks.lock().unwrap().len()
        }
    }

    impl WatchSource for Outages {
        fn look(
            &self,
            after: Option<EventId>,
            _role: Option<SessionRole>,
            _limit: Duration,
        ) -> Result<WatchLook> {
            let next = self.looks.lock().unwrap().pop_front().expect("a look");
            if let Some(since) = next {
                return Err(Down(since).into());
            }
            let after = after.unwrap_or(EventId::new(0));
            Ok(WatchLook {
                after,
                events: Vec::new(),
                cursor: after,
                woken: false,
                opened: Vec::new(),
                pulses: Value::Null,
                supervisors: Value::Null,
                binding: None,
            })
        }

        fn outages(&self) -> Option<&dyn OutageNotices> {
            Some(self)
        }

        fn wait_out(&self, duration: Duration) {
            self.waits.lock().unwrap().push(duration);
        }
    }

    impl OutageNotices for Outages {
        fn recorded(&self, error: &anyhow::Error) -> Option<i64> {
            error.downcast_ref::<Down>().map(|down| down.0)
        }
        fn told(&self, role: Option<SessionRole>, since: i64) -> bool {
            role == Some(SessionRole::Inbox) && *self.told.lock().unwrap() == Some(since)
        }
        fn tell(&self, role: Option<SessionRole>, since: i64) -> Result<()> {
            assert_eq!(role, Some(SessionRole::Inbox));
            *self.told.lock().unwrap() = Some(since);
            Ok(())
        }
    }

    /// ADR-t3102-3 decision 2: a watch of the inbox that returned an
    /// outage waits the same outage out, looking again each interval, and
    /// returns once the service answers again, or at once with the failure
    /// when another outage is recorded (noted as told).
    #[test]
    fn a_told_outage_is_waited_out_until_the_service_answers_or_another_is_recorded() {
        let options = WatchOptions {
            after: Some(EventId::new(7)),
            timeout: None,
            interval: Duration::from_secs(5),
            role: Some(SessionRole::Inbox),
        };
        let clock = Stopped(std::time::Instant::now());
        let looked = |source: &Outages| {
            look_through(source, &clock, &mut None, &options, clock.0, options.after)
        };

        let back = Outages::new(100, &[Some(100), Some(100), None]);
        match looked(&back).unwrap() {
            Looked::Look(look) => assert_eq!(look.cursor, EventId::new(7)),
            Looked::WaitedOut { since } => panic!("waited out {since}"),
        }
        assert_eq!(back.left(), 0);
        assert_eq!(*back.waits.lock().unwrap(), [options.interval; 2]);

        let another = Outages::new(100, &[Some(100), Some(160), None]);
        let failed = looked(&another).map(|_| ()).unwrap_err();
        assert_eq!(failed.downcast_ref::<Down>().map(|down| down.0), Some(160));
        assert_eq!(*another.told.lock().unwrap(), Some(160));
        assert_eq!(another.left(), 1);
        assert_eq!(*another.waits.lock().unwrap(), [options.interval]);
    }
}
