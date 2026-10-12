//! Which kind of ledger row an event of the EventStore is a record of.
//! Every event is one row's, so that the rows of a window give back the
//! events the functions of `stats`, `kpi` and `report` read
//! ([`super::window`]); which row of the kind is the fold's
//! ([`super::session`], [`super::queue`], [`super::run_row`],
//! [`super::task_row`]).

use super::{attributes, findings};
use crate::domain::{
    EventKind, RunEvent, claim_defer, claim_hold,
    event_kind::{self, SLOTS_FULL_ENDED, SLOTS_FULL_STARTED},
    headless_job::JobRef,
    live_alerts,
    sessions::{SESSION_CLOSED, SESSION_OPENED, SESSION_TOKENS, SESSION_TURNS, SpanRef},
    stats::main_log::MAIN_LOG_KINDS,
    supervisor_life,
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
fn queue_wide(kind: &str) -> bool {
    live_alerts::RECORD_KINDS.contains(&kind)
        || supervisor_life::KINDS.contains(&kind)
        || [SLOTS_FULL_STARTED, SLOTS_FULL_ENDED].contains(&kind)
        || claim_hold::CLAIM_HOLD_KINDS.contains(&kind)
        || claim_hold::LANDING_HOLD_KINDS.contains(&kind)
        || [
            claim_defer::CLAIM_DEFERRED,
            claim_defer::CLAIM_DEFERRAL_ENDED,
        ]
        .contains(&kind)
        || MAIN_LOG_KINDS.contains(&kind)
        || findings::KINDS.contains(&kind)
}

/// Whether an event of no run is a session's or a job's: a span's, a
/// runtime planner's (it names its `planner_id`), a plan review's or a
/// goal review's (they name their id), the observer's or a throughput
/// review's, or a job of no run its supervisor stopped.
fn of_session(event: &RunEvent) -> bool {
    let kind = event.kind.as_str();
    let job = JobRef::of(&event.payload);
    [
        SESSION_OPENED,
        SESSION_CLOSED,
        SESSION_TURNS,
        SESSION_TOKENS,
    ]
    .contains(&kind)
        || SpanRef::of(event).planner_id.is_some()
        || job.plan_review_id.is_some()
        || job.goal_review_id.is_some()
        || [
            EventKind::ObserveStarted.as_str(),
            EventKind::ObserveFinished.as_str(),
            EventKind::ThroughputReviewStarted.as_str(),
            EventKind::ThroughputReviewFinished.as_str(),
            EventKind::ThroughputReviewReported.as_str(),
        ]
        .contains(&kind)
        || kind == event_kind::GOAL_REVIEW_REARMED
        || kind == event_kind::HEADLESS_JOB_STOPPED
}

/// The kind of row `event` is a record of: the queue's for its own kinds,
/// else its run's; a task's or a goal's attribute record its task's or the
/// queue's; else a session's, its task's, or the queue's.
pub fn owner(event: &RunEvent) -> Owner {
    let kind = event.kind.as_str();
    if queue_wide(kind) {
        return Owner::Queue;
    }
    if event.run_id.is_some() {
        return Owner::Run;
    }
    if attributes::KINDS.contains(&kind) || kind == event_kind::ATTRIBUTES_BASELINE {
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
