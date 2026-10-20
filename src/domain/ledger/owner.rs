//! Which kind of ledger row an event of the EventStore is a record of.
//! Every event is one row's, so that the rows of a window give back the
//! events the functions of `stats`, `kpi` and `report` read
//! ([`super::window`]); which row of the kind is the fold's
//! ([`super::session`], [`super::queue`], [`super::run_row`],
//! [`super::task_row`]).

use super::{attributes, findings};
use crate::domain::{
    EventKind, RecordedKind, RunEvent, claim_hold, headless_job::JobRef, live_alerts,
    sessions::SpanRef, stats::main_log::MAIN_LOG_KINDS, supervisor_life,
};

/// The kind of row an event is a record of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// Its run's.
    Run,
    /// Its task's: a task's event of no run.
    Task,
    /// A session's or a job's of no run.
    Session,
    /// The queue's as a whole.
    Queue,
}

/// The kinds of the queue as a whole whatever ids they carry: the waits
/// before a claim and the landings' holds (intervals of a supervisor or the
/// queue), the record of the judgments of now, main's history, the
/// supervisors' lives and the findings.
fn queue_wide(kind: &RecordedKind) -> bool {
    kind.is_in(&live_alerts::RECORD_KINDS)
        || kind.is_in(&supervisor_life::KINDS)
        || kind.is_in(&[EventKind::SlotsFullStarted, EventKind::SlotsFullEnded])
        || kind.is_in(&claim_hold::CLAIM_HOLD_KINDS)
        || kind.is_in(&claim_hold::LANDING_HOLD_KINDS)
        || kind.is_in(&[EventKind::ClaimDeferred, EventKind::ClaimDeferralEnded])
        || kind.is_in(&MAIN_LOG_KINDS)
        || kind.is_in(findings::KINDS)
}

/// Whether an event of no run is a session's or a job's: a span's, a
/// runtime planner's (it names its `planner_id`), a plan review's or a
/// goal review's (they name their id), the observer's or a throughput
/// review's, or a job of no run its supervisor stopped.
fn of_session(event: &RunEvent) -> bool {
    let kind = &event.kind;
    let job = JobRef::of(&event.payload);
    kind.is_in(&[
        EventKind::SessionOpened,
        EventKind::SessionClosed,
        EventKind::SessionTurns,
        EventKind::SessionTokens,
    ]) || SpanRef::of(event).planner_id.is_some()
        || job.plan_review_id.is_some()
        || job.goal_review_id.is_some()
        || kind.is_in(&[
            EventKind::ObserveStarted,
            EventKind::ObserveFinished,
            EventKind::ThroughputReviewStarted,
            EventKind::ThroughputReviewFinished,
            EventKind::ThroughputReviewReported,
        ])
        || *kind == EventKind::GoalReviewRearmed
        || *kind == EventKind::HeadlessJobStopped
}

/// The kind of row `event` is a record of: the queue's for its own kinds,
/// else its run's; a task's or a goal's attribute record its task's or the
/// queue's; else a session's, its task's, or the queue's.
pub fn owner(event: &RunEvent) -> Owner {
    let kind = &event.kind;
    if queue_wide(kind) {
        return Owner::Queue;
    }
    if event.run_id.is_some() {
        return Owner::Run;
    }
    if kind.is_in(attributes::KINDS) || *kind == EventKind::AttributesBaseline {
        return if event.task_id.is_some() {
            Owner::Task
        } else {
            Owner::Queue
        };
    }
    if of_session(event) {
        return Owner::Session;
    }
    if event.task_id.is_some() {
        Owner::Task
    } else {
        Owner::Queue
    }
}
