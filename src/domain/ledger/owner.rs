//! Which kind of ledger row an event of the EventStore is a record of.
//! Events are records of their row, except for the queue's events omitted
//! after C, the coverage's start ([`super::queue::Unwritten`]): those are
//! no row's records. The common window read ([`super::window`]) supplements
//! the rows' records with the coverage's summaries and reads near the
//! edges from the EventStore, so that `stats`, `kpi` and `report` keep the
//! window's edges. The omission's rules are in [`super::queue`] and
//! [`super::builder`], the summaries in [`super::session::written::omitted`]
//! and the supplementing read in [`super::window`]. Which row of the kind
//! owns a record is the fold's ([`super::session`], [`super::queue`],
//! [`super::run_row`], [`super::task_row`]).

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

/// The kinds of no run that are a session's or a job's whatever they name
/// ([`of_session`]): a span's, the observer's, a throughput review's, a
/// goal review's rearming and a job of no run its supervisor stopped.
pub const SESSION_KINDS: [EventKind; 11] = [
    EventKind::SessionOpened,
    EventKind::SessionClosed,
    EventKind::SessionTurns,
    EventKind::SessionTokens,
    EventKind::ObserveStarted,
    EventKind::ObserveFinished,
    EventKind::ThroughputReviewStarted,
    EventKind::ThroughputReviewFinished,
    EventKind::ThroughputReviewReported,
    EventKind::GoalReviewRearmed,
    EventKind::HeadlessJobStopped,
];

/// The payload keys an event of no run of any other kind is a session's
/// or a job's by ([`of_session`]), when one is there and not null: a
/// runtime planner's, a plan review's and a goal review's id. A store
/// seeks those of no run that may be a session's by these and
/// [`SESSION_KINDS`], and keeps the ones [`owner`] gives to one.
pub const SESSION_KEYS: [&str; 3] = ["planner_id", "plan_review_id", "goal_review_id"];

/// Whether an event of no run is a session's or a job's: a span's, a
/// runtime planner's (it names its `planner_id`), a plan review's or a
/// goal review's (they name their id), the observer's or a throughput
/// review's, or a job of no run its supervisor stopped.
fn of_session(event: &RunEvent) -> bool {
    let job = JobRef::of(&event.payload);
    event.kind.is_in(&SESSION_KINDS)
        || SpanRef::of(event).planner_id.is_some()
        || job.plan_review_id.is_some()
        || job.goal_review_id.is_some()
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
