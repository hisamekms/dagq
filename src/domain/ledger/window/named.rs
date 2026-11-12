//! What a read from a time names of the rows it leaves out
//! (docs/plans/ledger-window-earlier-visibility-spike.md, "読みの約束" 3 to
//! 5 as "追試 (i) earlier" updates them): the ids the rows it reads and the
//! queue's records give, and which of the events a store seeks by them go
//! among the earlier records. No read here: a store seeks the events by
//! what [`Named`] gives and keeps those these functions take, so that what
//! it reads is in proportion to what the range names and to the runs left
//! open, not to the settled history outside the range.

use std::collections::BTreeSet;

use serde_json::Value;

use super::EARLIER_RUN_KINDS;
use crate::domain::forecast::score::TASK_MARK_KINDS;
use crate::domain::ledger::owner::{Owner, owner};
use crate::domain::ledger::{LedgerKind, LedgerRow};
use crate::domain::{EventKind, GoalSubmitted, RunEvent};

/// The kinds of a task's end a store reads of a task named only as a
/// member of a proposal the range names ([`member_end`]), each by its task
/// and kind.
pub const MEMBER_END_KINDS: [EventKind; 5] = [
    EventKind::TaskCreated,
    EventKind::AttributesBaseline,
    EventKind::TaskGoalChanged,
    EventKind::TaskStatusChanged,
    EventKind::TaskReopened,
];

/// The kinds a store seeks by the proposal they name, to find a proposal's
/// tasks ([`Named::member_of`]).
pub const MEMBER_KINDS: [EventKind; 2] = [EventKind::TaskSubmitted, EventKind::TaskReopened];

/// The kinds of a task's records that name a proposal and that only a
/// task of the proposal holds (its anchor's): read of the member tasks by
/// their task and kind, kept when they name a proposal named.
pub const PROPOSAL_KINDS: [EventKind; 3] = [
    EventKind::ProposalWithdrawn,
    EventKind::PlanDecided,
    EventKind::ProposalSettled,
];

/// The kinds by whose goal a store seeks a goal's tasks
/// ([`goal_task`]): the creation and the baseline (both restore its goal,
/// `attributes::apply_task`) by `goal_id`, a change of goal by `to`.
pub const GOAL_TASK_KINDS: [(EventKind, &str); 3] = [
    (EventKind::TaskCreated, "goal_id"),
    (EventKind::AttributesBaseline, "goal_id"),
    (EventKind::TaskGoalChanged, "to"),
];

/// What the range names: the ids the rows a read from a time reads and
/// the queue's records give.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Named {
    tasks: BTreeSet<i64>,
    reviews: BTreeSet<i64>,
    /// The plan reviews named, as the text of the id (a session row's key
    /// holds it so; a payload gives it as text or as a number).
    review_keys: BTreeSet<String>,
    planners: BTreeSet<i64>,
    proposals: BTreeSet<i64>,
    /// The proposals a finding of the queue's records proposed links.
    linked: BTreeSet<i64>,
    /// The tasks of each forecast's snapshot of the queue's records.
    snapshots: Vec<Vec<i64>>,
}

fn field(event: &RunEvent, key: &str) -> Option<i64> {
    event.payload.get(key).and_then(Value::as_i64)
}

impl Named {
    /// Note what the run, task or session row `row` the read takes names:
    /// its task, and the tasks, plan reviews, planners (by `planner_id` and
    /// by an actor `planner:<id>`) and proposals its records name.
    pub fn note_row(&mut self, row: &LedgerRow) {
        if matches!(row.kind, LedgerKind::Queue | LedgerKind::Node) {
            return;
        }
        self.tasks.extend(row.task_id.map(|task| task.as_i64()));
        for event in &row.records {
            self.tasks.extend(event.task_id.map(|task| task.as_i64()));
            self.reviews.extend(field(event, "plan_review_id"));
            self.planners.extend(field(event, "planner_id"));
            self.proposals.extend(field(event, "proposal_id"));
            self.note_ids(event);
        }
    }

    /// Note what the queue's row `row` names: the planner and plan review
    /// of each record of the time `from_ms` on (an ask a planner opened
    /// names it by its actor), the tasks of each forecast's snapshot and
    /// the proposals a finding proposed links.
    pub fn note_queue_row(&mut self, row: &LedgerRow, from_ms: i64) {
        for event in &row.records {
            match event.kind.known() {
                Some(EventKind::ForecastRecorded) => self.snapshots.push(
                    event.payload["tasks"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|task| task["id"].as_i64())
                        .collect(),
                ),
                Some(EventKind::FindingStatusChanged) if event.payload["to"] == "proposed" => {
                    self.linked.extend(field(event, "proposal_id"));
                }
                _ => {}
            }
            if crate::domain::stats::timestamp_millis(&event.created_at)
                .is_some_and(|at| at >= from_ms)
            {
                self.note_ids(event);
            }
        }
    }

    /// Note the plan review's id of `event` as text and the planner that
    /// asked it: its actor, or with none (an older binary's ask) its
    /// payload's `asked_by`, as `stats`' `planner_routes` takes the asker.
    fn note_ids(&mut self, event: &RunEvent) {
        let review = &event.payload["plan_review_id"];
        self.review_keys.extend(
            review
                .as_str()
                .map(str::to_owned)
                .or_else(|| review.as_i64().map(|id| id.to_string())),
        );
        let asker = event
            .actor
            .as_ref()
            .map(|actor| actor.id.as_str())
            .or_else(|| event.payload["asked_by"].as_str());
        self.planners.extend(
            asker
                .and_then(|asker| asker.strip_prefix("planner:"))
                .and_then(|id| id.parse::<i64>().ok()),
        );
    }

    /// The tasks named, whose records a store reads by their task.
    pub const fn tasks(&self) -> &BTreeSet<i64> {
        &self.tasks
    }

    /// The plan reviews named, whose records a store reads by their id.
    pub const fn reviews(&self) -> &BTreeSet<i64> {
        &self.reviews
    }

    /// The proposals named, whose tasks a store finds by them
    /// ([`MEMBER_KINDS`]).
    pub const fn proposals(&self) -> &BTreeSet<i64> {
        &self.proposals
    }

    /// The proposals whose goals' tasks a store finds
    /// (`goal_submitted` naming them): those named and those a finding
    /// links.
    pub fn goal_proposals(&self) -> BTreeSet<i64> {
        self.proposals.union(&self.linked).copied().collect()
    }

    /// The tasks of the forecasts' snapshots that name a task named: their
    /// plan's changes ([`TASK_MARK_KINDS`]) count on a snapshot's samples.
    pub fn snapshot_tasks(&self) -> BTreeSet<i64> {
        self.snapshots
            .iter()
            .filter(|tasks| tasks.iter().any(|task| self.tasks.contains(task)))
            .flatten()
            .copied()
            .collect()
    }

    /// The ranges of the session rows' keys (`[from, to)`) of the planners
    /// and plan reviews named (`{kind}/{id}/{what}/{round}`): `'0'` follows
    /// `'/'`, so each range holds the keys of its id alone.
    pub fn session_key_ranges(&self) -> Vec<(String, String)> {
        let planners = self
            .planners
            .iter()
            .map(|id| ("runtime_planner", id.to_string()));
        let reviews = self
            .review_keys
            .iter()
            .filter(|key| !key.is_empty() && !key.contains('/'))
            .map(|key| ("plan_review", key.clone()));
        planners
            .chain(reviews)
            .map(|(kind, id)| (format!("{kind}/{id}/"), format!("{kind}/{id}0")))
            .collect()
    }

    /// Whether `event` names what the range names: its task, a task, plan
    /// review, planner or proposal of its payload, or a task of a snapshot
    /// by a change of its plan.
    pub fn names(&self, event: &RunEvent) -> bool {
        let task = event.task_id.map(|task| task.as_i64());
        let mark = event
            .kind
            .known()
            .is_some_and(|kind| TASK_MARK_KINDS.contains(&kind));
        let of_snapshot = |task: i64| {
            self.snapshots.iter().any(|tasks| {
                tasks.contains(&task) && tasks.iter().any(|of| self.tasks.contains(of))
            })
        };
        if mark && task.is_some_and(of_snapshot) {
            return true;
        }
        task.is_some_and(|task| self.tasks.contains(&task))
            || field(event, "task_id").is_some_and(|task| self.tasks.contains(&task))
            || field(event, "plan_review_id").is_some_and(|id| self.reviews.contains(&id))
            || field(event, "planner_id").is_some_and(|id| self.planners.contains(&id))
            || field(event, "proposal_id").is_some_and(|id| self.proposals.contains(&id))
    }

    /// Whether `event`, sought by what is named, goes among the earlier
    /// records: a task's record that names what the range names, or a
    /// run's of the kinds read past its end ([`EARLIER_RUN_KINDS`]) that
    /// names it, but none of a run's claim, end, asks and sessions (those
    /// come of the runs left open) and none of a session's or the queue's
    /// (their rows are read whole or by key).
    pub fn takes(&self, event: &RunEvent) -> bool {
        let of_run = match owner(event) {
            Owner::Task => false,
            Owner::Run => true,
            Owner::Session | Owner::Queue => return false,
        };
        if of_run
            && !event.kind.known().is_some_and(|kind| {
                EARLIER_RUN_KINDS.contains(&kind)
                    && !matches!(
                        kind,
                        EventKind::RunClaimed
                            | EventKind::RunIntegrated
                            | EventKind::RunSatisfiedClosed
                            | EventKind::AskOpened
                            | EventKind::AskAnswered
                            | EventKind::SessionOpened
                            | EventKind::SessionClosed
                    )
            })
        {
            return false;
        }
        self.names(event)
    }

    /// The task `event` (sought by a proposal, [`MEMBER_KINDS`]) adds as a
    /// member of a proposal named, when no other record names it: such a
    /// task's ends alone are read ([`member_end`]).
    pub fn member_of(&self, event: &RunEvent) -> Option<i64> {
        let kind = event.kind.known()?;
        let task = event.task_id?.as_i64();
        (MEMBER_KINDS.contains(&kind)
            && field(event, "proposal_id").is_some_and(|id| self.proposals.contains(&id))
            && !self.tasks.contains(&task))
        .then_some(task)
    }

    /// The goal `event` (a `goal_submitted` sought by a proposal) puts in
    /// a proposal of [`Self::goal_proposals`].
    pub fn goal_of(&self, event: &RunEvent) -> Option<i64> {
        let (goal, submitted) = GoalSubmitted::of_event(event)?;
        self.goal_proposals()
            .contains(&submitted.proposal_id.as_i64())
            .then_some(goal.as_i64())
    }

    /// These names with the proposals a finding links that a
    /// `goal_submitted` among `events` puts a goal in: such a proposal
    /// runs on its goal's tasks though nothing of the range names it, and
    /// its own tasks (their ends, its owner's origin) and its cancellation
    /// ([`PROPOSAL_KINDS`], its anchor's) decide whether it does, so they
    /// are sought as a named proposal's. A proposal a finding links that
    /// holds no goal needs none of them: every task of it ended before the
    /// range (a task to be done keeps its row in the range, which names
    /// it), so it runs in neither read.
    pub fn with_goal_proposals_of(&self, events: &[RunEvent]) -> Self {
        let mut named = self.clone();
        named.proposals.extend(
            events
                .iter()
                .filter_map(GoalSubmitted::of_event)
                .map(|(_, submitted)| submitted.proposal_id.as_i64())
                .filter(|proposal| self.linked.contains(proposal)),
        );
        named
    }

    /// These names with the planners and plan reviews of the sessions'
    /// records among `events` that name what these name: their session
    /// rows hold what the whole read reads of them (a planner's revision
    /// of a proposal named, say), and are read by their keys.
    pub fn with_sessions_of(&self, events: &[RunEvent]) -> Self {
        let mut named = self.clone();
        for event in events {
            if owner(event) == Owner::Session && self.names(event) {
                named.planners.extend(field(event, "planner_id"));
                named.note_ids(event);
            }
        }
        named
    }

    /// Whether a member task `task` is read whole instead (named itself).
    pub fn names_task(&self, task: i64) -> bool {
        self.tasks.contains(&task)
    }
}

/// The task `event` (sought by its goal, [`GOAL_TASK_KINDS`]) holds in one
/// of `goals`.
pub fn goal_task(event: &RunEvent, goals: &BTreeSet<i64>) -> Option<i64> {
    let kind = event.kind.known()?;
    let (_, key) = GOAL_TASK_KINDS.iter().find(|(of, _)| *of == kind)?;
    let goal = field(event, key)?;
    goals
        .contains(&goal)
        .then(|| event.task_id.map(|task| task.as_i64()))?
}

/// Whether `event` is among a member task's ends, of its `records` (all
/// of [`MEMBER_END_KINDS`] of it a store reads): its creation and baseline,
/// its last change of goal, its last change of status and the reopenings
/// after it. A store reads these by the task and the kind, the last by id
/// alone: what `improvement_proposals` takes of a task's membership and
/// whether it ended.
pub fn member_end(event: &RunEvent, records: &[RunEvent]) -> bool {
    let last = |kind: EventKind| {
        records
            .iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.id)
            .max()
    };
    match event.kind.known() {
        Some(EventKind::TaskCreated | EventKind::AttributesBaseline) => true,
        Some(EventKind::TaskGoalChanged) => last(EventKind::TaskGoalChanged) == Some(event.id),
        Some(EventKind::TaskStatusChanged) => last(EventKind::TaskStatusChanged) == Some(event.id),
        Some(EventKind::TaskReopened) => {
            last(EventKind::TaskStatusChanged).is_none_or(|status| event.id > status)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests;
