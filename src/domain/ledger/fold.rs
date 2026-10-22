//! The one fold of a run's and a task's events into intervals that do not
//! overlap and cover their whole life (ADR-t1662-1 decisions 1, 2, 6 and
//! 16, docs/design/measurement.md "畳む関数"). A run's boundaries are its
//! `run_phase_changed` records only: no other event moves them. A task's
//! life before and between its runs is read from its status moves, the
//! `ready_at` its claims recorded and its runs' own folds, its holds
//! taking its time ready they cover, and the waits of the supervisors
//! before a claim laid over the rest of that time. Time no
//! record accounts for is an `unattributed` interval. Reads no state: the
//! events, the queue's pushes, the waits before a claim and `now_ms` are
//! all, and the same input gives the same output.

use crate::domain::EventKind;
use std::borrow::Borrow;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::domain::after_claim::{AfterClaim, AfterClaimTrack};
use crate::domain::claim_facts::ATTRIBUTES;
use crate::domain::claim_hold;
use crate::domain::pre_claim::{IntervalKind, PreClaimInterval};
use crate::domain::run::payload::typed;
use crate::domain::run_phase::{Attempt, Blocker, Holds, PUSHED, Phase};
use crate::domain::stats::timestamp_millis;
use crate::domain::{EventId, RunEvent, TaskId};

/// The phase name of time no record accounts for.
pub const UNATTRIBUTED: &str = "unattributed";
/// A task's time ready, after its prerequisites were done.
pub const READY: &str = "ready";
/// A task's time ready, before the last of its prerequisites was done
/// (the next claim's `ready_at`).
pub const DEPENDENCY_WAIT: &str = "dependency_wait";
/// A task's time submitted, waiting for or under plan review.
pub const PLAN_REVIEW: &str = "plan_review";
/// A task's time as a draft.
pub const DRAFT: &str = "draft";
/// The key of a task row's detail that holds when its status `ready`
/// began, while that is its status now and it has not ended
/// ([`TaskLife::ready_since`]).
pub const READY_SINCE: &str = "ready_since";
/// The key of a task row's detail that holds `true` while, not ready now,
/// a wait still open bears on its time ready that ended
/// ([`TaskLife::waits_open`]).
pub const WAITS_OPEN: &str = "waits_open";
/// A task's time ready under its hold (`task_held` to `task_released`,
/// ADR-t1879-1): the run's own phase of the hold's wait, laid over the
/// task's time before a claim with the same tags.
pub const HELD: &str = Phase::Held.name();

const ENDED: &str = Phase::Ended.name();
/// The phase a run whose push failed waits in.
pub const PUSH_PENDING: &str = Phase::PushPending.name();

/// One interval of a life: its phase and tags (none for
/// [`UNATTRIBUTED`]), its start and end in unix milliseconds, what moved
/// the run into it, the run that held the landing slot meanwhile, and in a
/// task's life the run it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interval {
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<Blocker>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holds: Option<Holds>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<Attempt>,
    pub start: i64,
    pub end: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

impl Interval {
    fn tagged(phase: &str, blocker: Blocker, holds: Holds, start: i64, end: i64) -> Self {
        Self {
            phase: phase.to_owned(),
            blocker: Some(blocker),
            holds: Some(holds),
            attempt: None,
            start,
            end,
            cause: None,
            blocked_by: None,
            run_id: None,
        }
    }

    fn unattributed(start: i64, end: i64) -> Self {
        Self {
            blocker: None,
            holds: None,
            ..Self::tagged(UNATTRIBUTED, Blocker::Runtime, Holds::None, start, end)
        }
    }

    /// Its length in milliseconds.
    pub fn millis(&self) -> i64 {
        self.end - self.start
    }

    /// Whether it holds a slot: the second measure (ADR-t1662-1 decision 5).
    pub fn holds_slot(&self) -> bool {
        matches!(self.holds, Some(Holds::WorkerSlot | Holds::LandingSlot))
    }

    /// Whether no record accounts for it.
    pub fn is_unattributed(&self) -> bool {
        self.phase == UNATTRIBUTED
    }
}

/// When a run first reached each point its events name, in unix
/// milliseconds: not boundaries, but what the names `work`, `validate`,
/// `wait_to_land` and `startup` measure between.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Milestones {
    pub claimed: Option<i64>,
    pub agent_started: Option<i64>,
    pub first_commit: Option<i64>,
    pub receipt: Option<i64>,
    pub validated: Option<i64>,
    pub integrated: Option<i64>,
}

impl Milestones {
    fn observe(&mut self, event: &RunEvent, at: i64) {
        let slot = match event.kind.known() {
            Some(EventKind::RunClaimed) => &mut self.claimed,
            Some(EventKind::AgentStarted) => &mut self.agent_started,
            Some(EventKind::FirstCommitObserved) => &mut self.first_commit,
            Some(EventKind::ReceiptObserved) => &mut self.receipt,
            Some(EventKind::ValidationFinished) => &mut self.validated,
            Some(EventKind::RunIntegrated) => &mut self.integrated,
            _ => return,
        };
        slot.get_or_insert(at);
    }
}

/// A run's life folded: from its claim, intervals up to its end, or up to
/// `now_ms` while it is not settled (`end` none: it runs, or its push is
/// pending with no later push of the same branch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunLife {
    pub start: i64,
    pub intervals: Vec<Interval>,
    pub end: Option<i64>,
    /// The `cause` of its end (`pushed`, `push_skipped`, a failed status,
    /// `canceled`, ...).
    pub outcome: Option<String>,
    /// The run whose `push_finished` closed its `push_pending`.
    pub closed_by: Option<String>,
    pub milestones: Milestones,
    /// The claim's attributes as `run_claimed` recorded them, whatever
    /// their keys.
    pub attributes: Map<String, Value>,
    pub after_claim: AfterClaim,
    /// The last event folded in: the run's own, or the push that closed it.
    pub inputs_through: EventId,
}

/// One `run_phase_changed` as the fold reads it.
struct Record<'e> {
    event: &'e RunEvent,
    at: i64,
}

impl Record<'_> {
    fn phase(&self) -> &str {
        self.event.payload["phase"].as_str().unwrap_or(UNATTRIBUTED)
    }

    fn interval(&self, start: i64, end: i64) -> Interval {
        let payload = &self.event.payload;
        fn read<T: serde::de::DeserializeOwned>(payload: &Value, key: &str) -> Option<T> {
            serde_json::from_value(payload[key].clone()).ok()
        }
        let text = |key: &str| payload[key].as_str().map(str::to_owned);
        let (blocker, holds): (Option<Blocker>, Option<Holds>) =
            (read(payload, "blocker"), read(payload, "holds"));
        if blocker.is_none() || holds.is_none() {
            // A record without tags it can read accounts for nothing.
            return Interval {
                cause: text("cause"),
                ..Interval::unattributed(start, end)
            };
        }
        Interval {
            phase: self.phase().to_owned(),
            blocker,
            holds,
            attempt: read(payload, "attempt"),
            start,
            end,
            cause: text("cause"),
            blocked_by: text("blocked_by"),
            run_id: None,
        }
    }
}

fn at(event: &RunEvent) -> Option<i64> {
    timestamp_millis(&event.created_at)
}

/// Fold one run's `events` (ascending id). `pushes` are the queue's
/// `push_finished` (any run's, ascending id): a later one to the same
/// remote and branch closes the run's `push_pending`. `None` for a run
/// that finished with no `run_phase_changed`: a run from before the
/// switch, which is never folded into intervals (ADR-t1662-1 decision
/// 18). A run still on its way with none yet (its first record comes
/// after the claim, or failed and waits for the next move) is all
/// unattributed up to now, not legacy.
pub fn fold_run(events: &[RunEvent], pushes: &[RunEvent], now_ms: i64) -> Option<RunLife> {
    let records: Vec<Record> = events
        .iter()
        .filter(|event| event.kind == EventKind::RunPhaseChanged)
        .filter_map(|event| {
            Some(Record {
                event,
                at: at(event)?,
            })
        })
        .collect();
    if records.is_empty() && finished(events) {
        return None;
    }
    let mut milestones = Milestones::default();
    let mut after_claim = AfterClaimTrack::default();
    let mut attributes = Map::new();
    for event in events {
        after_claim.observe(event);
        if let Some(at) = at(event) {
            milestones.observe(event, at);
        }
        if event.kind == EventKind::RunClaimed
            && attributes.is_empty()
            && let Some(Value::Object(set)) = event.payload.get(ATTRIBUTES)
        {
            attributes = set.clone();
        }
    }
    let first_at = records.first().map_or(now_ms, |first| first.at);
    let start = milestones
        .claimed
        .or_else(|| events.first().and_then(at))
        .unwrap_or(first_at)
        .min(first_at);
    let mut life = RunLife {
        start,
        intervals: Vec::new(),
        end: None,
        outcome: None,
        closed_by: None,
        milestones,
        attributes,
        after_claim: after_claim.facts(),
        inputs_through: events
            .iter()
            .map(|event| event.id)
            .max()
            .unwrap_or(EventId::new(0)),
    };
    if first_at > start {
        life.intervals.push(Interval::unattributed(start, first_at));
    }
    let mut cursor = start;
    for (index, record) in records.iter().enumerate() {
        let from = record.at.max(cursor);
        if let Some(next) = records.get(index + 1) {
            let to = next.at.max(from);
            life.intervals.push(record.interval(from, to));
            cursor = to;
            continue;
        }
        if record.phase() == ENDED {
            life.end = Some(from);
            life.outcome = record.event.payload["cause"].as_str().map(str::to_owned);
        } else if record.phase() == PUSH_PENDING
            && let Some(push) = later_push(events, pushes, record.event.id)
        {
            let to = at(push).unwrap_or(from).max(from);
            life.intervals.push(record.interval(from, to));
            life.end = Some(to);
            life.outcome = Some(PUSHED.to_owned());
            life.closed_by = push.run_id.as_ref().map(ToString::to_string);
            life.inputs_through = life.inputs_through.max(push.id);
        } else {
            life.intervals.push(record.interval(from, now_ms.max(from)));
        }
    }
    Some(life)
}

/// Whether the run's `events` say it finished: landed, or a status of
/// its end.
fn finished(events: &[RunEvent]) -> bool {
    events.iter().any(|event| {
        event.kind == EventKind::RunIntegrated
            || matches!(
                event.payload["status"].as_str(),
                Some("failed" | "interrupted" | "succeeded" | "integrated")
            )
    })
}

/// The first `push_finished` of `pushes` after the event `after` to the
/// remote and branch of the run's last failed push (to any, when that did
/// not record them).
fn later_push<'p>(
    events: &[RunEvent],
    pushes: &'p [RunEvent],
    after: EventId,
) -> Option<&'p RunEvent> {
    let failed = events
        .iter()
        .rev()
        .find(|event| event.kind == EventKind::PushFailed && event.id < after);
    closing_push(failed, pushes, after)
}

/// The remote and branch a push records (`push_failed`, `push_finished`),
/// each absent when its payload does not record it as text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct PushedTo {
    #[serde(default, deserialize_with = "typed")]
    pub remote: Option<String>,
    #[serde(default, deserialize_with = "typed")]
    pub branch: Option<String>,
}

impl PushedTo {
    /// What `event` records of where it pushed.
    pub fn of(event: &RunEvent) -> Self {
        Self::deserialize(&event.payload).unwrap_or_default()
    }

    /// Whether `push`, a `push_finished` after the event `after`, closes a
    /// `push_pending` whose failed push was to this: of the remote and the
    /// branch, each this records is the same.
    pub fn closed_by(&self, push: &RunEvent, after: EventId) -> bool {
        if push.kind != EventKind::PushFinished || push.id <= after {
            return false;
        }
        let to = Self::of(push);
        [(&self.remote, &to.remote), (&self.branch, &to.branch)]
            .iter()
            .all(|(failed, pushed)| failed.is_none() || failed == pushed)
    }
}

/// The first `push_finished` of `pushes` after the event `after` to the
/// remote and branch of the push that failed, `failed` (to any, when there
/// is none or it did not record them).
pub(super) fn closing_push<'p>(
    failed: Option<&RunEvent>,
    pushes: impl IntoIterator<Item = &'p RunEvent>,
    after: EventId,
) -> Option<&'p RunEvent> {
    let failed = failed.map(PushedTo::of).unwrap_or_default();
    pushes
        .into_iter()
        .find(|push| failed.closed_by(push, after))
}

/// A wait of the supervisors laid over a task's time ready, cut to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overlay {
    /// `slots_full`, `claim_hold` or `claim_deferral`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub start: i64,
    pub end: i64,
}

/// A task's life folded: from `task_created` to its end (`completed` at
/// the end of its landed run's push, or when it completed with no landed
/// run, as a closed `already_satisfied` claim does; or `canceled`), or to
/// `now_ms` while it is not settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskLife {
    pub start: i64,
    pub intervals: Vec<Interval>,
    pub end: Option<i64>,
    /// `completed` or `canceled`.
    pub outcome: Option<String>,
    /// The `ready_at` of its last claim, and its first claim.
    pub ready_at: Option<i64>,
    pub first_claimed: Option<i64>,
    /// Its runs, in the order they were claimed.
    pub runs: Vec<String>,
    /// The waits laid over its time ready, and per kind the time they
    /// cover (each kind's overlays merged, so supervisors are not added).
    pub overlays: Vec<Overlay>,
    pub overlay_millis: BTreeMap<String, i64>,
    pub inputs_through: EventId,
    /// Of the waits that bear on its time ready ([`overlays`]: one a
    /// silence ended that starts by the end of that time, whenever it
    /// ended), those their supervisor's life ended, by their start, each
    /// with its [`PreClaimInterval::reach`]: a time past a silence ends one
    /// with no event, which changes them, and so does a later event naming
    /// its supervisor that opens it again, though a silence ends it again.
    pub life_ended: Vec<(EventId, EventId)>,
    /// While it has not ended and its status now is `ready`, when that
    /// status began (at most the time folded to, so a fold at the time it
    /// became ready has it too, though it has no time ready yet).
    pub ready_since: Option<i64>,
    /// While its status now is not `ready`, whether a wait still open at
    /// the time folded to bears on a time ready that ended: its last event,
    /// which moves while it is open (each event naming a slots full's
    /// supervisor), is an input of the row ([`overlays`]).
    pub waits_open: bool,
}

/// Of the waits that bear on a task's time ready ([`overlays`]), the
/// greatest [`PreClaimInterval::reach`] (`waits`), whether one is open on
/// a time ready that ended while not ready now (`open`), and the start and
/// reach of those their supervisor's life ended (`life_ended`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Reach {
    waits: Option<EventId>,
    open: bool,
    life_ended: Vec<(EventId, EventId)>,
}

/// What [`fold_task`] makes of a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskFold {
    Life(Box<TaskLife>),
    /// A run of it is from before the switch ([`fold_run`] gives none):
    /// the task is not folded either.
    Legacy {
        runs: Vec<String>,
    },
}

/// Fold one task's `events` (ascending id; its runs' events among them).
/// `pushes` are as for [`fold_run`]; `pre_claim` are the waits before a
/// claim ([`crate::domain::pre_claim::pre_claim_intervals`]) to lay over
/// its time ready: every supervisor's slots full and the queue's holds,
/// and the task's own deferrals.
pub fn fold_task(
    task: TaskId,
    events: &[RunEvent],
    pushes: &[RunEvent],
    pre_claim: impl IntoIterator<Item = impl Borrow<PreClaimInterval>>,
    now_ms: i64,
) -> TaskFold {
    let mut runs: Vec<(String, Option<i64>, Option<RunLife>)> = Vec::new();
    for claim in events
        .iter()
        .filter(|event| event.kind == EventKind::RunClaimed)
    {
        let Some(run) = &claim.run_id else { continue };
        let own: Vec<RunEvent> = events
            .iter()
            .filter(|event| event.run_id.as_ref() == Some(run))
            .cloned()
            .collect();
        let ready_at = claim.payload["ready_at"]
            .as_str()
            .and_then(timestamp_millis);
        runs.push((run.to_string(), ready_at, fold_run(&own, pushes, now_ms)));
    }
    if runs.iter().any(|(_, _, life)| life.is_none()) {
        return TaskFold::Legacy {
            runs: runs.into_iter().map(|(run, _, _)| run).collect(),
        };
    }
    let runs: Vec<(String, Option<i64>, RunLife)> = runs
        .into_iter()
        .filter_map(|(run, ready_at, life)| Some((run, ready_at, life?)))
        .collect();
    let start = events
        .iter()
        .find(|event| event.kind == EventKind::TaskCreated)
        .or(events.first())
        .and_then(at)
        .unwrap_or(now_ms);
    let statuses = statuses(events, start);
    let (end, outcome) = match statuses.last().map(|(at, status)| (*at, status.as_str())) {
        Some((at, "canceled")) => (Some(at), Some("canceled")),
        Some((at, "completed")) => {
            let landed = runs
                .iter()
                .rev()
                .find(|(_, _, life)| life.milestones.integrated.is_some());
            match landed {
                Some((_, _, life)) => (life.end, life.end.map(|_| "completed")),
                None => (Some(at), Some("completed")),
            }
        }
        _ => (None, None),
    };
    let horizon = end.unwrap_or(now_ms).max(start);
    let mut fold = Composer {
        statuses: &statuses,
        holds: crate::domain::task_hold::task_hold_spans(events)
            .into_iter()
            .map(|(start, end)| (start, end.unwrap_or(i64::MAX)))
            .collect(),
        claims: runs
            .iter()
            .map(|(_, ready_at, life)| (life.start, *ready_at))
            .collect(),
        intervals: Vec::new(),
    };
    let mut cursor = start;
    for (run, _, life) in &runs {
        let run_start = life.start.clamp(cursor, horizon);
        fold.statuses_between(cursor, run_start);
        for interval in &life.intervals {
            let (from, to) = (interval.start.max(run_start), interval.end.min(horizon));
            if to > from {
                fold.intervals.push(Interval {
                    start: from,
                    end: to,
                    run_id: Some(run.clone()),
                    ..interval.clone()
                });
            }
        }
        let run_end = life.intervals.last().map_or(life.start, |last| last.end);
        cursor = cursor.max(run_end.min(horizon));
    }
    fold.statuses_between(cursor, horizon);
    let intervals = fold.intervals;
    // Ready now: its last status, not ended, read from the statuses and
    // not the intervals, which have none of no length.
    let ready_since = statuses
        .last()
        .filter(|(_, status)| end.is_none() && status == "ready")
        .map(|(at, _)| (*at).clamp(start, horizon));
    let open_ready = ready_since.map(|since| (since, horizon));
    let (overlays, reach) = overlays(task, &intervals, pre_claim, now_ms, open_ready);
    let mut overlay_millis = BTreeMap::new();
    for kind in ["slots_full", "claim_hold", "claim_deferral"] {
        let covered = merged_millis(overlays.iter().filter(|overlay| overlay.kind == kind));
        if covered > 0 {
            overlay_millis.insert(kind.to_owned(), covered);
        }
    }
    TaskFold::Life(Box::new(TaskLife {
        start,
        intervals,
        end,
        outcome: outcome.map(str::to_owned),
        ready_at: runs.iter().rev().find_map(|(_, ready_at, _)| *ready_at),
        first_claimed: runs.first().map(|(_, _, life)| life.start),
        runs: runs.iter().map(|(run, _, _)| run.clone()).collect(),
        overlays,
        overlay_millis,
        inputs_through: events
            .iter()
            .map(|event| event.id)
            .chain(runs.iter().map(|(_, _, life)| life.inputs_through))
            .chain(reach.waits)
            .max()
            .unwrap_or(EventId::new(0)),
        life_ended: reach.life_ended,
        ready_since,
        waits_open: reach.open,
    }))
}

/// The task's statuses from `start`, each with when it began: the `from`
/// of its first move (none known when it never moved), then the `to` of
/// each `task_status_changed` and `run_claimed`.
fn statuses(events: &[RunEvent], start: i64) -> Vec<(i64, String)> {
    let moves: Vec<(i64, &Value)> = events
        .iter()
        .filter(|event| {
            matches!(
                event.kind.known(),
                Some(EventKind::TaskStatusChanged | EventKind::RunClaimed)
            )
        })
        .filter_map(|event| Some((at(event)?, &event.payload)))
        .collect();
    let first = moves
        .first()
        .and_then(|(_, payload)| payload["from"].as_str())
        .unwrap_or(UNATTRIBUTED);
    // A move timed before the one recorded ahead of it starts when that
    // one does, so the statuses never overlap.
    let mut began = start;
    std::iter::once((start, first.to_owned()))
        .chain(moves.iter().filter_map(|(at, payload)| {
            let to = payload["to"].as_str()?.to_owned();
            began = began.max(*at);
            Some((began, to))
        }))
        .collect()
}

/// Lays a task's statuses into intervals between its runs.
struct Composer<'s> {
    statuses: &'s [(i64, String)],
    /// The task's holds, an open one to the end of time.
    holds: Vec<(i64, i64)>,
    /// Each run's claim and the `ready_at` it recorded.
    claims: Vec<(i64, Option<i64>)>,
    intervals: Vec<Interval>,
}

impl Composer<'_> {
    fn statuses_between(&mut self, from: i64, to: i64) {
        for (index, (began, status)) in self.statuses.iter().enumerate() {
            let until = self.statuses.get(index + 1).map_or(i64::MAX, |next| next.0);
            let (a, b) = ((*began).max(from), until.min(to));
            if b > a {
                self.status(status, a, b);
            }
        }
    }

    fn status(&mut self, status: &str, from: i64, to: i64) {
        let (phase, blocker) = match status {
            "draft" => (DRAFT, Blocker::Human),
            "submitted" => (PLAN_REVIEW, Blocker::Ai),
            "ready" => {
                // Ready before the last prerequisite was done is a wait for
                // the dependencies, as the next claim recorded it.
                let ready_at = self
                    .claims
                    .iter()
                    .find(|(claimed, _)| *claimed >= from)
                    .and_then(|(_, ready_at)| *ready_at)
                    .map(|ready_at| ready_at.clamp(from, to));
                if let Some(ready_at) = ready_at.filter(|ready_at| *ready_at > from) {
                    self.push_ready(DEPENDENCY_WAIT, from, ready_at);
                    if to > ready_at {
                        self.push_ready(READY, ready_at, to);
                    }
                    return;
                }
                return self.push_ready(READY, from, to);
            }
            // In progress with no run on it (between a run's end and a
            // person's next move), or a status the fold does not know.
            _ => return self.push(Interval::unattributed(from, to)),
        };
        self.push(Interval::tagged(phase, blocker, Holds::None, from, to));
    }

    /// The time `phase` of a task ready from `from` to `to`, its parts
    /// under a hold [`HELD`] (a person releases it), the rest `phase`.
    fn push_ready(&mut self, phase: &str, from: i64, to: i64) {
        let mut cursor = from;
        let mut holds: Vec<(i64, i64)> = self
            .holds
            .iter()
            .map(|(start, end)| ((*start).max(from), (*end).min(to)))
            .filter(|(start, end)| end > start)
            .collect();
        holds.sort_unstable();
        for (start, end) in holds {
            let start = start.max(cursor);
            if end <= start {
                continue;
            }
            if start > cursor {
                self.push(Interval::tagged(
                    phase,
                    Blocker::Queue,
                    Holds::None,
                    cursor,
                    start,
                ));
            }
            let tags = Phase::Held.tags();
            self.push(Interval::tagged(HELD, tags.blocker, tags.holds, start, end));
            cursor = end;
        }
        if to > cursor {
            self.push(Interval::tagged(
                phase,
                Blocker::Queue,
                Holds::None,
                cursor,
                to,
            ));
        }
    }

    fn push(&mut self, interval: Interval) {
        self.intervals.push(interval);
    }
}

/// The waits of `pre_claim` that fall on `task`'s time [`READY`], cut to
/// it: slots full and holds of any supervisor, deferrals of `task` only;
/// and the [`Reach`] of those that bear on its time ready, which a later
/// event that moves one of them (its end, its supervisor's life) or starts
/// another, and a time past its supervisor's silence, change:
///
/// - its reach, for a wait that starts by the end of `open_ready`, its time
///   ready up to now while it has not ended (from when its status became
///   `ready` to the time folded to, both included, so of no length at the
///   time it became ready), and ends at or after its start (an open one at
///   the time folded to, so one starting then too) or was ended by its
///   supervisor's life; and for one that overlaps a time [`READY`], or
///   starts before its end and was ended by its supervisor's life. Once a
///   wait bore on a time, it does while the inputs grow (their times in the
///   order of their events), so the reach read never goes back, and a fold
///   over fewer events reads less ([`super::builder::behind`]). While not
///   ready now, also whether one on a time [`READY`] is open, whose reach
///   moves with it (each event naming a slots full's supervisor);
/// - its start and reach, for a wait its supervisor's life ended that
///   starts before the end of a time [`READY`] (or by that of
///   `open_ready`), as those its reach is read of: one its supervisor's
///   silence ended, whenever that was, as a later event naming the
///   supervisor opens it again over every time since; one its stop ended,
///   when that end falls on that time (on `open_ready`, any end by now,
///   so that the time folded to does not change them). An open wait is cut
///   to the end of a time ready past, so of the ends only these move its
///   overlay there.
fn overlays(
    task: TaskId,
    intervals: &[Interval],
    pre_claim: impl IntoIterator<Item = impl Borrow<PreClaimInterval>>,
    now_ms: i64,
    open_ready: Option<(i64, i64)>,
) -> (Vec<Overlay>, Reach) {
    let mut out = Vec::new();
    let mut reach = Reach::default();
    for wait in pre_claim {
        let wait = wait.borrow();
        let kind = match wait.kind {
            IntervalKind::SlotsFull => "slots_full",
            IntervalKind::ClaimHold => "claim_hold",
            IntervalKind::ClaimDeferral if wait.task_id == Some(task) => "claim_deferral",
            IntervalKind::ClaimDeferral => continue,
        };
        let Some(start) = timestamp_millis(&wait.started_at) else {
            continue;
        };
        let end = wait
            .ended_at
            .as_deref()
            .and_then(timestamp_millis)
            .unwrap_or(now_ms);
        let life_ended = wait.end.as_deref() == Some(claim_hold::ENDED_SUPERVISOR_GONE);
        // A silence's end names no supervisor; a stop's names it.
        let silent = life_ended && wait.ended_by.is_none();
        let on_ready = intervals
            .iter()
            .filter(|interval| interval.phase == READY)
            .any(|ready| start < ready.end && (end > ready.start || life_ended));
        if on_ready
            || open_ready.is_some_and(|(from, to)| start <= to && (end >= from || life_ended))
        {
            reach.waits = reach.waits.max(Some(wait.reach));
        }
        // Ready now, every move marks it: the flag would only grow with the
        // time folded to.
        reach.open |= open_ready.is_none() && on_ready && wait.ended_at.is_none();
        // Up to now, any end by then falls on it, whatever the time folded
        // to; on a time past, one before its end.
        let up_to_now = |(from, to): (i64, i64)| start <= to && (silent || end >= from);
        let past = |ready: &Interval| {
            start < ready.end && (silent || (end >= ready.start && end < ready.end))
        };
        let moves = life_ended
            && (open_ready.is_some_and(up_to_now)
                || intervals
                    .iter()
                    .filter(|interval| interval.phase == READY)
                    .any(past));
        if moves {
            reach.life_ended.push((wait.id, wait.reach));
        }
        for ready in intervals.iter().filter(|interval| interval.phase == READY) {
            let (from, to) = (start.max(ready.start), end.min(ready.end));
            if to > from {
                out.push(Overlay {
                    kind: kind.to_owned(),
                    supervisor: wait.supervisor.clone(),
                    class: wait.class.map(|class| class.as_str().to_owned()),
                    reason: wait.reason.clone(),
                    start: from,
                    end: to,
                });
            }
        }
    }
    (out, reach)
}

/// The time `overlays` cover together.
fn merged_millis<'o>(overlays: impl Iterator<Item = &'o Overlay>) -> i64 {
    let mut spans: Vec<(i64, i64)> = overlays.map(|o| (o.start, o.end)).collect();
    spans.sort_unstable();
    let mut total = 0;
    let mut reach = i64::MIN;
    for (start, end) in spans {
        let from = start.max(reach);
        if end > from {
            total += end - from;
        }
        reach = reach.max(end);
    }
    total
}
