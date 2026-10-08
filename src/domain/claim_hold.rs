//! Holding new claims (task 327): the supervisor claims no new run while a
//! reason holds, and leaves the runs in flight alone. One judgement names
//! the first reason that holds; the supervisor records `claim_held` when
//! the hold starts (or its reason changes) and `claim_resumed` when it
//! ends, as queue events, so `status` shows a hold in progress and `stats`
//! the time spent held apart from the `idle_slots` alert. The reasons are
//! the free disk space below what a run needs (task 377) and the 1-minute
//! load average above `supervise --max-load`; later reasons join
//! [`HoldReason`] and [`ClaimHold::judge`].
//!
//! Landings are held the same way (task 377): the supervisor starts no
//! landing's verification while the free disk space is below what it
//! needs, and records `landing_held` / `landing_resumed` with the same
//! payloads, so `status` and `stats` show them in the same shape
//! ([`LANDINGS`]).

use super::EventKind;
use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Value, json};

use super::{EventId, LeaseToken, RunEvent, TaskId, stats::timestamp_millis};

/// Recorded when the supervisor starts holding its claims, or holds them
/// for another reason (`reason`, `value`, `threshold`, `message`,
/// `supervisor`).
pub const CLAIM_HELD: &str = crate::domain::event_kind::EventKind::ClaimHeld.as_str();
/// Recorded when the hold ends and claims resume (`reason` of the hold
/// that ended, `supervisor`).
pub const CLAIM_RESUMED: &str = crate::domain::event_kind::EventKind::ClaimResumed.as_str();
/// The two kinds, for reading the latest of them.
pub const CLAIM_HOLD_KINDS: [&str; 2] = [CLAIM_HELD, CLAIM_RESUMED];
/// Recorded when the supervisor starts holding the landings' verification
/// (task 377), with the payload of `claim_held`.
pub const LANDING_HELD: &str = crate::domain::event_kind::EventKind::LandingHeld.as_str();
/// Recorded when landings resume, with the payload of `claim_resumed`.
pub const LANDING_RESUMED: &str = crate::domain::event_kind::EventKind::LandingResumed.as_str();
/// The two kinds, for reading the latest of them.
pub const LANDING_HOLD_KINDS: [&str; 2] = [LANDING_HELD, LANDING_RESUMED];

/// What a hold holds: new claims or the landings' verification, each
/// recorded by its pair of queue events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldKinds {
    pub held: EventKind,
    pub resumed: EventKind,
    /// The hold is of the supervisor that recorded it, not of the host:
    /// another live supervisor does not end it (a landing waits in one
    /// supervisor's slot).
    pub own: bool,
}

impl HoldKinds {
    /// Both kinds, for reading the latest of them.
    pub const fn kinds(self) -> [&'static str; 2] {
        [self.held.as_str(), self.resumed.as_str()]
    }
}

/// The holds on new claims.
pub const CLAIMS: HoldKinds = HoldKinds {
    held: EventKind::ClaimHeld,
    resumed: EventKind::ClaimResumed,
    own: false,
};
/// The holds on the landings' verification.
pub const LANDINGS: HoldKinds = HoldKinds {
    held: EventKind::LandingHeld,
    resumed: EventKind::LandingResumed,
    own: true,
};

/// The load average per logical core above which `supervise --max-load`
/// holds claims by default. On the 8-core host this queue was tuned on,
/// the `backend_call_failed` events by load band (`stats --full` of
/// 2026-09-26) were 1 at 0-4, 1 at 8-16, 56 at 16-32, 257 at 32-64 and
/// 116 above 64: cmux's timeouts start past twice the cores (task 623).
pub const DEFAULT_LOAD_PER_CORE: f64 = 2.0;

/// The default of `supervise --max-load` when the host's logical cores
/// cannot be read: twice the 8 cores of the host it was tuned on.
pub const FALLBACK_MAX_LOAD: f64 = 16.0;

/// The default of `supervise --max-load`: [`DEFAULT_LOAD_PER_CORE`] times
/// the host's logical cores, or [`FALLBACK_MAX_LOAD`] when they are
/// unknown.
pub fn default_max_load(logical_cores: Option<usize>) -> f64 {
    logical_cores
        .filter(|&cores| cores > 0)
        .map_or(FALLBACK_MAX_LOAD, |cores| {
            DEFAULT_LOAD_PER_CORE * cores as f64
        })
}

/// The load threshold `supervise` holds claims at: the given `--max-load`,
/// else [`default_max_load`]; 0 or below disables the hold (`None`).
pub fn resolve_max_load(given: Option<f64>, logical_cores: Option<usize>) -> Option<f64> {
    let max_load = given.unwrap_or_else(|| default_max_load(logical_cores));
    (max_load > 0.0).then_some(max_load)
}

/// Why new claims are held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldReason {
    /// An `authentication` `queue_hold` ask is open: Claude Code's login
    /// ran out (task 437, ADR-0047 decision 42).
    Authentication,
    /// A `cost` `queue_hold` ask about the usage limit is open (task 437).
    UsageLimit,
    /// The free disk space of the queue's directory is below what a run
    /// needs (task 377, [`super::disk`]).
    DiskSpace,
    /// The 1-minute load average is above `--max-load`.
    LoadAverage,
}

impl HoldReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::UsageLimit => "usage_limit",
            Self::DiskSpace => "disk_space",
            Self::LoadAverage => "load_average",
        }
    }
}

/// What the supervisor judges before it claims.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HoldInputs {
    /// The 1-minute load average; `None` when it could not be read.
    pub load_average: Option<f64>,
    /// `--max-load`; `None` holds for no load.
    pub max_load: Option<f64>,
    /// The free bytes of the queue's directory; `None` when they could not
    /// be read.
    pub free_bytes: Option<u64>,
    /// The free bytes a new run (or a landing) needs; `None` holds for no
    /// disk space.
    pub needed_bytes: Option<u64>,
    /// The open authentication or usage-limit ask that holds the queue's
    /// work (task 437), if any.
    pub queue_hold: Option<QueueHold>,
}

/// An open authentication or usage-limit `queue_hold` ask (ADR-0047
/// decision 42): while it is open no new run is claimed and no headless
/// job starts (task 437).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueHold {
    /// [`HoldReason::Authentication`] or [`HoldReason::UsageLimit`].
    pub reason: HoldReason,
    pub ask_id: i64,
    /// How many runs the ask holds.
    pub affected: usize,
}

/// A hold on new claims: its reason, and the value that crossed the
/// threshold.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClaimHold {
    pub reason: HoldReason,
    pub value: f64,
    pub threshold: f64,
    /// The `queue_hold` ask of an authentication or usage-limit hold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_id: Option<i64>,
}

impl ClaimHold {
    /// The first reason that holds, or `None` when claims may go on: an
    /// open authentication or usage-limit ask (every run would stop at it;
    /// `value` is the number of runs it holds, `threshold` 0), the free
    /// disk space below the bytes needed (the disk fills whatever the
    /// load), then the load above `--max-load`. A value that could not be
    /// read holds nothing.
    pub fn judge(inputs: &HoldInputs) -> Option<Self> {
        if let Some(hold) = inputs.queue_hold {
            return Some(Self {
                reason: hold.reason,
                value: hold.affected as f64,
                threshold: 0.0,
                ask_id: Some(hold.ask_id),
            });
        }
        if let (Some(free), Some(needed)) = (inputs.free_bytes, inputs.needed_bytes)
            && free < needed
        {
            return Some(Self {
                reason: HoldReason::DiskSpace,
                value: free as f64,
                threshold: needed as f64,
                ask_id: None,
            });
        }
        match (inputs.load_average, inputs.max_load) {
            (Some(value), Some(threshold)) if value > threshold => Some(Self {
                reason: HoldReason::LoadAverage,
                value,
                threshold,
                ask_id: None,
            }),
            _ => None,
        }
    }

    /// Why nothing is claimed, for the log and the event.
    pub fn message(&self) -> String {
        let ask = self.ask_id.unwrap_or_default();
        match self.reason {
            HoldReason::Authentication => format!(
                "the authentication ask {ask} is open (Claude Code's login ran out): no new run is claimed and no headless job (review, recovery, plan review, goal review, observer) starts until a person logs in and answers it; the runs in flight keep their leases"
            ),
            HoldReason::UsageLimit => format!(
                "the usage-limit ask {ask} is open (Claude Code's usage limit was reached): no new run is claimed and no headless job (review, recovery, plan review, goal review, observer) starts until a person answers it; the runs in flight keep their leases"
            ),
            HoldReason::DiskSpace => format!(
                "the free disk space {} of the queue's directory is below the {} a new run needs: no new run is claimed until the ended runs' worktrees are cleaned or a person frees the disk; the runs in flight go on",
                super::disk::gib(self.value),
                super::disk::gib(self.threshold)
            ),
            HoldReason::LoadAverage => format!(
                "the 1-minute load average {:.2} is above --max-load {:.2}: no new run is claimed until it falls back; the runs in flight go on",
                self.value, self.threshold
            ),
        }
    }

    /// Why no landing starts its verification, for the log and the event.
    pub fn landing_message(&self) -> String {
        format!(
            "the free disk space {} of the queue's directory is below the {} a landing's verification needs: no run lands until the ended runs' worktrees are cleaned or a person frees the disk; the runs stay awaiting integration",
            super::disk::gib(self.value),
            super::disk::gib(self.threshold)
        )
    }

    /// The message of a hold of `kinds`.
    pub fn message_for(&self, kinds: HoldKinds) -> String {
        if kinds == LANDINGS {
            self.landing_message()
        } else {
            self.message()
        }
    }
}

/// The event the supervisor `token` records when its judgement (`hold`)
/// differs from the latest [`CLAIM_HOLD_KINDS`] event on the queue
/// (`last`): `claim_held` when it holds and the last one is not a hold in
/// place for the same reason, `claim_resumed` when it holds nothing and the
/// last one is a hold, else none. A hold is in place while the supervisor
/// that recorded it runs (`live` of its token; this supervisor's own always
/// is): the load is the host's, so two supervisors on one queue do not
/// record it in turns, while one started after the holder stopped or died
/// records the hold anew under its own token, which `status` and `stats`
/// show; a hold nobody runs any more ends with the next `claim_resumed`.
pub fn transition(
    hold: Option<&ClaimHold>,
    last: Option<&RunEvent>,
    token: &LeaseToken,
    live: impl Fn(&str) -> bool,
) -> Option<(EventKind, Value)> {
    transition_of(CLAIMS, hold, last, token, live)
}

/// [`transition`] for the holds of `kinds`: `last` is the latest of its
/// two events.
pub fn transition_of(
    kinds: HoldKinds,
    hold: Option<&ClaimHold>,
    last: Option<&RunEvent>,
    token: &LeaseToken,
    live: impl Fn(&str) -> bool,
) -> Option<(EventKind, Value)> {
    let held = last.filter(|event| event.kind == kinds.held);
    let held_reason = held
        .filter(|event| {
            text(event, "supervisor").is_some_and(|holder| holder == token.as_str() || live(holder))
        })
        .and_then(|event| text(event, "reason"));
    match hold {
        Some(hold) if held_reason != Some(hold.reason.as_str()) => {
            let mut payload = json!({
                "reason": hold.reason,
                "value": hold.value,
                "threshold": hold.threshold,
                "message": hold.message_for(kinds),
                "supervisor": token,
            });
            if let Some(ask_id) = hold.ask_id {
                payload["ask_id"] = json!(ask_id);
            }
            Some((kinds.held, payload))
        }
        // Another live supervisor's own hold is its to end.
        None if kinds.own
            && held
                .and_then(|event| text(event, "supervisor"))
                .is_some_and(|holder| holder != token.as_str() && live(holder)) =>
        {
            None
        }
        None if held.is_some() => Some((
            kinds.resumed,
            json!({"reason": held.and_then(|event| text(event, "reason")), "supervisor": token}),
        )),
        _ => None,
    }
}

/// A hold of one supervisor's claims kept outside [`ClaimHold::judge`]
/// (the CI watch's, [`super::ci_watch::CI_WATCH_HOLD`]; the landing
/// branch's, [`super::landing_branch::LANDING_BRANCH_HOLD`]), recorded by
/// its pair of queue events only where it changes, so that `status` and
/// `candidates`' `held` read it. Each supervisor's records stand apart:
/// one's hold is read from its own latest record, which another's do not
/// end or repeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnHold {
    pub held: EventKind,
    pub resumed: EventKind,
}

impl OwnHold {
    /// Both kinds, for reading the latest of them.
    pub const fn kinds(self) -> [&'static str; 2] {
        [self.held.as_str(), self.resumed.as_str()]
    }

    /// The latest of `records` (of the two kinds, in any order) that the
    /// supervisor `token` recorded.
    pub fn latest_of<'e>(records: &'e [RunEvent], token: &str) -> Option<&'e RunEvent> {
        records
            .iter()
            .filter(|event| text(event, "supervisor") == Some(token))
            .max_by_key(|event| event.id)
    }

    /// Whether `latest` (a supervisor's latest record) is a hold.
    pub fn holds(self, latest: Option<&RunEvent>) -> bool {
        latest.is_some_and(|event| event.kind == self.held.as_str())
    }

    /// The event the supervisor `token` records when its hold (`hold`: the
    /// reason and the rest of the payload, `None` while it holds nothing)
    /// differs from its own latest record (`last`, [`Self::latest_of`]):
    /// `held` when it holds and the last one is not a hold for the same
    /// reason, `resumed` (with the reason that ended) when it holds nothing
    /// and the last one is a hold, else none.
    pub fn transition(
        self,
        hold: Option<(&str, Value)>,
        last: Option<&RunEvent>,
        token: &LeaseToken,
    ) -> Option<(EventKind, Value)> {
        let held = last.filter(|event| self.holds(Some(event)));
        let held_reason = held.and_then(|event| text(event, "reason"));
        match hold {
            Some((reason, _)) if held_reason == Some(reason) => None,
            Some((reason, mut payload)) => {
                if !payload.is_object() {
                    payload = json!({});
                }
                payload["reason"] = json!(reason);
                payload["supervisor"] = json!(token);
                Some((self.held, payload))
            }
            None if held.is_some() => Some((
                self.resumed,
                json!({"reason": held_reason, "supervisor": token}),
            )),
            None => None,
        }
    }
}

/// One reason's holds in a window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReasonHolds {
    pub count: i64,
    pub secs: i64,
}

/// The hold in progress: the latest `claim_held` no `claim_resumed` or
/// stop of its supervisor followed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpenHold {
    pub reason: String,
    pub supervisor: Option<String>,
    pub since: String,
    pub value: Option<f64>,
    pub threshold: Option<f64>,
}

/// The holds on new claims (`stats`' `claim_holds`): those that started in
/// the window, by reason, with the seconds each lasted (one still open
/// lasts to the window's end), and the hold in progress now.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ClaimHolds {
    pub count: i64,
    pub secs: i64,
    pub by_reason: BTreeMap<String, ReasonHolds>,
    pub held: Option<OpenHold>,
}

/// Aggregate the holds of `events` (ascending id) that started with
/// `after < id <= upto`; `end_ms` ends one still open. A hold ends at the
/// next `claim_held` or `claim_resumed`, or at the `supervisor_stopped` of
/// its supervisor. Holds belong to no task, so they count only when
/// `counts` accepts no task (no `--goal`); `held` is the queue's now
/// either way.
pub fn claim_holds(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    end_ms: i64,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> ClaimHolds {
    holds_of(CLAIMS, events, after, upto, end_ms, counts)
}

/// [`claim_holds`] for the holds of `kinds` (`stats`' `landing_holds` for
/// [`LANDINGS`]).
pub fn holds_of(
    kinds: HoldKinds,
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    end_ms: i64,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> ClaimHolds {
    let mut stats = ClaimHolds::default();
    for span in hold_spans(kinds, events) {
        let event = span.start;
        let start = span.start_ms.unwrap_or(end_ms);
        let end = span
            .end
            .as_ref()
            .map_or(end_ms, |end| end.at_ms.unwrap_or(end_ms));
        if span.end.is_none() {
            let number = |key: &str| event.payload.get(key).and_then(Value::as_f64);
            stats.held = Some(OpenHold {
                reason: text(event, "reason").unwrap_or("unknown").to_owned(),
                supervisor: text(event, "supervisor").map(str::to_owned),
                since: event.created_at.clone(),
                value: number("value"),
                threshold: number("threshold"),
            });
        }
        if event.id <= after || event.id > upto || !counts(event.task_id) {
            continue;
        }
        // A hold that ends after the window counts to the window's end.
        let secs = (end.min(end_ms) - start).max(0) / 1000;
        let reason = text(event, "reason").unwrap_or("unknown").to_owned();
        let entry = stats.by_reason.entry(reason).or_default();
        entry.count += 1;
        entry.secs += secs;
        stats.count += 1;
        stats.secs += secs;
    }
    stats
}

/// One hold of `kinds` as [`hold_spans`] reads it: the `held` event that
/// started it and its time, and how it ended (none while it is open).
#[derive(Debug, Clone)]
pub struct HoldSpan<'e> {
    pub start: &'e RunEvent,
    /// The time of `start`, `None` when it cannot be read.
    pub start_ms: Option<i64>,
    pub end: Option<SpanEnd<'e>>,
}

/// How a span ended: the event that ended it, its time (`None` when it
/// cannot be read) and why ([`ENDED_REPLACED`], [`ENDED_RESUMED`],
/// [`ENDED_SUPERVISOR_GONE`], or for a deferral the ends of
/// [`super::claim_defer::deferral_spans`]).
#[derive(Debug, Clone)]
pub struct SpanEnd<'e> {
    pub event: &'e RunEvent,
    pub at_ms: Option<i64>,
    pub why: String,
}

/// A hold ended by the next hold (another reason, or another supervisor's).
pub const ENDED_REPLACED: &str = "replaced";
/// A hold ended by its `resumed` event, whoever recorded it.
pub const ENDED_RESUMED: &str = "resumed";
/// A hold ended by the stop of the supervisor that recorded it.
pub const ENDED_SUPERVISOR_GONE: &str = "supervisor_gone";

/// The holds of `kinds` in `events` (ascending id), in the order they
/// started: one ends at the next `held` (it is replaced) or `resumed` of
/// `kinds`, whichever supervisor recorded it, or at the
/// `supervisor_stopped` of the supervisor that recorded it. The one rule
/// `stats` ([`holds_of`]) and the waits before a claim
/// ([`super::pre_claim::pre_claim_intervals`]) read the holds by.
pub fn hold_spans(kinds: HoldKinds, events: &[RunEvent]) -> Vec<HoldSpan<'_>> {
    let mut spans: Vec<HoldSpan<'_>> = Vec::new();
    let mut open: Option<HoldSpan<'_>> = None;
    for event in events {
        let at_ms = timestamp_millis(&event.created_at);
        let why = match event.kind.as_str() {
            kind if kind == kinds.held => ENDED_REPLACED,
            kind if kind == kinds.resumed => ENDED_RESUMED,
            "supervisor_stopped"
                if open.as_ref().is_some_and(|held| {
                    text(held.start, "supervisor") == text(event, "supervisor")
                }) =>
            {
                ENDED_SUPERVISOR_GONE
            }
            _ => continue,
        };
        if let Some(mut held) = open.take() {
            held.end = Some(SpanEnd {
                event,
                at_ms,
                why: why.to_owned(),
            });
            spans.push(held);
        }
        if event.kind == kinds.held {
            open = Some(HoldSpan {
                start: event,
                start_ms: at_ms,
                end: None,
            });
        }
    }
    spans.extend(open);
    spans
}

fn text<'e>(event: &'e RunEvent, key: &str) -> Option<&'e str> {
    event.payload.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: i64, kind: &str, payload: Value, at: &str) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: format!("2026-09-26T01:{at}.000Z"),
            actor: None,
        }
    }

    /// An own hold is recorded once where it starts, again only for
    /// another reason, and closed once; each supervisor reads its own
    /// latest record, which another's records do not change.
    #[test]
    fn an_own_hold_records_only_its_changes() {
        use super::super::ci_watch::{CI_WATCH_HELD, CI_WATCH_HOLD as HOLD, CI_WATCH_RESUMED};
        let me = LeaseToken::new("me");
        let record = |id: i64, kind: &str, reason: &str, by: &str| {
            event(
                id,
                kind,
                json!({"reason": reason, "supervisor": by}),
                "00:00",
            )
        };
        let pending = || Some(("pending", json!({"workflow": "ci.yml"})));
        let (kind, payload) = HOLD.transition(pending(), None, &me).unwrap();
        assert_eq!(kind, EventKind::CiWatchHeld);
        assert_eq!(
            payload,
            json!({"reason": "pending", "workflow": "ci.yml", "supervisor": "me"})
        );
        let mine = record(1, CI_WATCH_HELD, "pending", "me");
        assert!(HOLD.transition(pending(), Some(&mine), &me).is_none());
        let (kind, payload) = HOLD
            .transition(Some(("unreadable", json!({}))), Some(&mine), &me)
            .unwrap();
        assert_eq!(
            (kind, payload["reason"].as_str()),
            (EventKind::CiWatchHeld, Some("unreadable"))
        );
        let (kind, payload) = HOLD.transition(None, Some(&mine), &me).unwrap();
        assert_eq!(kind, EventKind::CiWatchResumed);
        assert_eq!(payload, json!({"reason": "pending", "supervisor": "me"}));
        let resumed = record(2, CI_WATCH_RESUMED, "pending", "me");
        assert!(HOLD.transition(None, Some(&resumed), &me).is_none());
        assert!(HOLD.transition(None, None, &me).is_none());

        // Another supervisor's records, before and after, are not this one's
        // latest: neither repeats its hold nor ends it.
        let records = [
            record(3, CI_WATCH_HELD, "pending", "other"),
            mine.clone(),
            record(4, CI_WATCH_RESUMED, "pending", "other"),
        ];
        let latest = OwnHold::latest_of(&records, "me");
        assert_eq!(latest.map(|event| event.id), Some(mine.id));
        assert!(HOLD.holds(latest));
        assert!(HOLD.transition(pending(), latest, &me).is_none());
        assert!(!HOLD.holds(OwnHold::latest_of(&records, "other")));
        assert!(OwnHold::latest_of(&records, "gone").is_none());
    }

    fn load(value: Option<f64>, max: Option<f64>) -> Option<ClaimHold> {
        ClaimHold::judge(&HoldInputs {
            load_average: value,
            max_load: max,
            ..HoldInputs::default()
        })
    }

    fn disk(free: Option<u64>, needed: Option<u64>, load: Option<f64>) -> Option<ClaimHold> {
        ClaimHold::judge(&HoldInputs {
            load_average: load,
            max_load: Some(16.0),
            free_bytes: free,
            needed_bytes: needed,
            queue_hold: None,
        })
    }

    /// Without `--max-load` the threshold is twice the logical cores (16
    /// on the unknown host); a given value wins, and 0 or below turns the
    /// hold off whatever the cores (task 623).
    #[test]
    fn the_default_max_load_is_twice_the_logical_cores() {
        assert_eq!(resolve_max_load(None, Some(8)), Some(16.0));
        assert_eq!(resolve_max_load(None, Some(4)), Some(8.0));
        assert_eq!(resolve_max_load(None, Some(12)), Some(24.0));
        assert_eq!(resolve_max_load(None, None), Some(FALLBACK_MAX_LOAD));
        assert_eq!(resolve_max_load(None, Some(0)), Some(FALLBACK_MAX_LOAD));
        assert_eq!(resolve_max_load(Some(8.5), Some(4)), Some(8.5));
        assert_eq!(resolve_max_load(Some(16.0), Some(12)), Some(16.0));
        assert_eq!(resolve_max_load(Some(0.0), Some(8)), None);
        assert_eq!(resolve_max_load(Some(-1.0), None), None);
    }

    #[test]
    fn an_open_authentication_or_usage_limit_ask_holds_before_the_disk() {
        let inputs = |reason| HoldInputs {
            load_average: Some(40.0),
            max_load: Some(16.0),
            free_bytes: Some(1),
            needed_bytes: Some(2),
            queue_hold: Some(QueueHold {
                reason,
                ask_id: 7,
                affected: 2,
            }),
        };
        let hold = ClaimHold::judge(&inputs(HoldReason::Authentication)).unwrap();
        assert_eq!(hold.reason, HoldReason::Authentication);
        assert_eq!(
            (hold.value, hold.threshold, hold.ask_id),
            (2.0, 0.0, Some(7))
        );
        assert!(
            hold.message().contains("authentication ask 7"),
            "{}",
            hold.message()
        );
        assert!(hold.message().contains("no headless job"));
        let (kind, payload) =
            transition(Some(&hold), None, &LeaseToken::new("s"), |_| true).unwrap();
        assert_eq!(kind, EventKind::ClaimHeld);
        assert_eq!(payload["reason"], json!("authentication"));
        assert_eq!(payload["ask_id"], json!(7));
        let limit = ClaimHold::judge(&inputs(HoldReason::UsageLimit)).unwrap();
        assert!(
            limit.message().contains("usage-limit ask 7"),
            "{}",
            limit.message()
        );
        assert_eq!(HoldReason::UsageLimit.as_str(), "usage_limit");
        assert_eq!(HoldReason::Authentication.as_str(), "authentication");
        // A disk or load hold carries no ask.
        let (_, payload) = transition(
            disk(Some(1), Some(2), None).as_ref(),
            None,
            &LeaseToken::new("s"),
            |_| true,
        )
        .unwrap();
        assert!(payload.get("ask_id").is_none());
    }

    #[test]
    fn the_disk_holds_below_the_bytes_needed_before_the_load() {
        const GIB: u64 = 1 << 30;
        let hold = disk(Some(GIB), Some(4 * GIB), Some(40.0)).unwrap();
        assert_eq!(hold.reason, HoldReason::DiskSpace);
        assert_eq!((hold.value, hold.threshold), (GIB as f64, 4.0 * GIB as f64));
        assert!(hold.message().contains("1.0 GiB"), "{}", hold.message());
        assert!(hold.message().contains("4.0 GiB a new run needs"));
        assert!(hold.landing_message().contains("no run lands"));
        assert_eq!(hold.message_for(LANDINGS), hold.landing_message());
        assert_eq!(hold.message_for(CLAIMS), hold.message());
        assert_eq!(HoldReason::DiskSpace.as_str(), "disk_space");
        // Enough space, or nothing to judge by: the load decides.
        assert_eq!(
            disk(Some(4 * GIB), Some(4 * GIB), Some(40.0))
                .unwrap()
                .reason,
            HoldReason::LoadAverage
        );
        assert_eq!(disk(None, Some(4 * GIB), Some(1.0)), None);
        assert_eq!(disk(Some(GIB), None, Some(1.0)), None);
    }

    #[test]
    fn landings_are_held_and_summed_by_their_own_kinds() {
        let hold = disk(Some(1), Some(2), None).unwrap();
        let (kind, payload) =
            transition_of(LANDINGS, Some(&hold), None, &LeaseToken::new("s"), |_| true).unwrap();
        assert_eq!(kind, EventKind::LandingHeld);
        assert_eq!(payload["reason"], json!("disk_space"));
        assert!(
            payload["message"]
                .as_str()
                .unwrap()
                .contains("no run lands")
        );
        // A claim hold is no landing hold.
        let claim = event(1, CLAIM_HELD, payload.clone(), "00:00");
        assert_eq!(
            transition_of(LANDINGS, None, Some(&claim), &LeaseToken::new("s"), |_| {
                true
            }),
            None
        );
        let held = event(2, LANDING_HELD, payload, "00:00");
        assert_eq!(
            transition_of(
                LANDINGS,
                Some(&hold),
                Some(&held),
                &LeaseToken::new("s"),
                |_| true
            ),
            None
        );
        let (kind, _) =
            transition_of(LANDINGS, None, Some(&held), &LeaseToken::new("s"), |_| true).unwrap();
        assert_eq!(kind, EventKind::LandingResumed);
        // Another live supervisor with no landing waiting leaves it; one
        // that stopped does not hold it any more.
        assert_eq!(
            transition_of(LANDINGS, None, Some(&held), &LeaseToken::new("t"), |_| true),
            None
        );
        let (kind, _) = transition_of(LANDINGS, None, Some(&held), &LeaseToken::new("t"), |_| {
            false
        })
        .unwrap();
        assert_eq!(kind, EventKind::LandingResumed);
        let events = [claim, held, event(3, LANDING_RESUMED, json!({}), "00:30")];
        let end = timestamp_millis("2026-09-26T01:03:00.000Z").unwrap();
        let landings = holds_of(
            LANDINGS,
            &events,
            EventId::new(0),
            EventId::new(3),
            end,
            |_| true,
        );
        assert_eq!((landings.count, landings.secs), (1, 30));
        assert_eq!(landings.by_reason["disk_space"].count, 1);
        assert_eq!(landings.held, None);
        assert_eq!(LANDINGS.kinds(), LANDING_HOLD_KINDS);
        assert_eq!(CLAIMS.kinds(), CLAIM_HOLD_KINDS);
    }

    #[test]
    fn the_load_holds_above_the_threshold_only() {
        let hold = load(Some(20.5), Some(16.0)).unwrap();
        assert_eq!(hold.reason, HoldReason::LoadAverage);
        assert_eq!((hold.value, hold.threshold), (20.5, 16.0));
        assert!(hold.message().contains("20.50"), "{}", hold.message());
        assert!(hold.message().contains("--max-load 16.00"));
        assert_eq!(load(Some(16.0), Some(16.0)), None);
        assert_eq!(load(Some(3.0), Some(16.0)), None);
        assert_eq!(load(None, Some(16.0)), None);
        assert_eq!(load(Some(99.0), None), None);
        assert_eq!(HoldReason::LoadAverage.as_str(), "load_average");
    }

    #[test]
    fn a_hold_is_recorded_when_it_starts_and_when_it_ends() {
        let hold = load(Some(20.0), Some(16.0)).unwrap();
        let (kind, payload) =
            transition(Some(&hold), None, &LeaseToken::new("s"), |_| true).unwrap();
        assert_eq!(kind, EventKind::ClaimHeld);
        assert_eq!(payload["reason"], json!("load_average"));
        assert_eq!(payload["value"], json!(20.0));
        assert_eq!(payload["threshold"], json!(16.0));
        assert_eq!(payload["supervisor"], json!("s"));
        let held = event(1, CLAIM_HELD, payload, "00:00");
        // Held already: nothing more, however high the load goes.
        let higher = load(Some(40.0), Some(16.0)).unwrap();
        assert_eq!(
            transition(Some(&higher), Some(&held), &LeaseToken::new("s"), |_| true),
            None
        );
        // Another supervisor's hold is the host's too: held already, and
        // it ends that hold when the load falls.
        assert_eq!(
            transition(Some(&hold), Some(&held), &LeaseToken::new("t"), |_| true),
            None
        );
        let (kind, payload) =
            transition(None, Some(&held), &LeaseToken::new("t"), |_| true).unwrap();
        assert_eq!(kind, EventKind::ClaimResumed);
        assert_eq!(payload["supervisor"], json!("t"));
        // A holder that stopped or died holds nothing: the supervisor
        // started after it records the hold anew under its own token, and
        // ends it when the load falls.
        let stopped = |holder: &str| holder != "s";
        let (kind, payload) =
            transition(Some(&hold), Some(&held), &LeaseToken::new("t"), stopped).unwrap();
        assert_eq!(kind, EventKind::ClaimHeld);
        assert_eq!(payload["supervisor"], json!("t"));
        assert_eq!(
            transition(Some(&hold), Some(&held), &LeaseToken::new("s"), |_| false),
            None
        );
        let (kind, payload) =
            transition(None, Some(&held), &LeaseToken::new("t"), stopped).unwrap();
        assert_eq!(kind, EventKind::ClaimResumed);
        assert_eq!(payload["reason"], json!("load_average"));
        // The load fell: resumed, naming the hold's reason.
        let (kind, payload) =
            transition(None, Some(&held), &LeaseToken::new("s"), |_| true).unwrap();
        assert_eq!(kind, EventKind::ClaimResumed);
        assert_eq!(
            payload,
            json!({"reason": "load_average", "supervisor": "s"})
        );
        let resumed = event(2, CLAIM_RESUMED, payload, "00:10");
        assert_eq!(
            transition(None, Some(&resumed), &LeaseToken::new("s"), |_| true),
            None
        );
        assert_eq!(
            transition(None, None, &LeaseToken::new("s"), |_| true),
            None
        );
        assert_eq!(
            transition(Some(&hold), Some(&resumed), &LeaseToken::new("s"), |_| true)
                .unwrap()
                .0,
            EventKind::ClaimHeld
        );
    }

    #[test]
    fn holds_are_summed_by_reason_and_the_open_one_is_the_hold_now() {
        let held = |id, at, who: &str| {
            event(
                id,
                CLAIM_HELD,
                json!({"reason": "load_average", "value": 20.0, "threshold": 16.0, "supervisor": who}),
                at,
            )
        };
        let events = [
            held(1, "00:00", "s"),
            event(2, "run_claimed", json!({}), "00:05"),
            event(3, CLAIM_RESUMED, json!({"reason": "load_average"}), "00:30"),
            held(4, "01:00", "s"),
            // Another supervisor's stop ends nothing; its own does.
            event(5, "supervisor_stopped", json!({"supervisor": "t"}), "01:10"),
            event(6, "supervisor_stopped", json!({"supervisor": "s"}), "01:20"),
            held(7, "02:00", "u"),
            event(8, CLAIM_HELD, json!({"supervisor": "u"}), "02:05"),
        ];
        let end = timestamp_millis("2026-09-26T01:03:00.000Z").unwrap();
        let all = claim_holds(&events, EventId::new(0), EventId::new(8), end, |_| true);
        assert_eq!(all.count, 4);
        assert_eq!(all.secs, 30 + 20 + 5 + 55);
        assert_eq!(
            all.by_reason["load_average"],
            ReasonHolds { count: 3, secs: 55 }
        );
        assert_eq!(all.by_reason["unknown"], ReasonHolds { count: 1, secs: 55 });
        let now = all.held.unwrap();
        assert_eq!(now.reason, "unknown");
        assert_eq!(now.supervisor.as_deref(), Some("u"));
        assert_eq!(now.since, "2026-09-26T01:02:05.000Z");

        // Only the holds that started in the window; the hold now either way.
        let later = claim_holds(&events, EventId::new(3), EventId::new(6), end, |_| true);
        assert_eq!(later.count, 1);
        assert_eq!(later.secs, 20);
        assert!(later.held.is_some());
        // A hold that ends after the window counts to the window's end.
        let upto = timestamp_millis("2026-09-26T01:00:10.000Z").unwrap();
        let cut = claim_holds(&events, EventId::new(0), EventId::new(2), upto, |_| true);
        assert_eq!((cut.count, cut.secs), (1, 10));
        let goal = claim_holds(&events, EventId::new(0), EventId::new(8), end, |task| {
            task.is_some()
        });
        assert_eq!(goal.count, 0);
        assert!(goal.held.is_some());
        let resumed = claim_holds(&events[..3], EventId::new(0), EventId::new(3), end, |_| {
            true
        });
        assert_eq!(resumed.held, None);
    }
}
