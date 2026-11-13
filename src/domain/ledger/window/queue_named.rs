//! What a read from a time names of the queue rows of the kinds it reads
//! by their range with the store's views trusted
//! ([`super::queue_from::BY_VIEWS`]), and which of the events a store seeks
//! by those names go among the window's records
//! (docs/plans/ledger-window-earlier-visibility-spike.md, "追試 (ii) の残り
//! （Spike 4811）" "判定" (B) to (F)). No read here: a store seeks by what
//! [`QueueWants`] gives and keeps what these functions take, so that what
//! it reads of before the time is in proportion to what the range names,
//! not to the settled history outside it.
//!
//! - Main's record: its summary up to `queue_before` or before it (the
//!   state of main and CI there, [`MainSummary`]) and its events after the
//!   summary, and of before it the commits the range names by their sha
//!   (those its runs landed, and those its records revert), the commits a
//!   base completed after the summary applies, the commits whose time a
//!   window from the time holds, the commits that revert a landed one and
//!   CI's turns to red on a landed one.
//! - A goal named (by a record of the rows read, a queue record of the
//!   range, or `--goal`): its records down the ids to the last that
//!   replaces it ([`replaces_goal`]) before the time, a baseline by its
//!   group's mark before the time too, and that mark.
//! - Each baseline's latest mark before the time.
//! - A finding that links a proposal named (its own, or one a
//!   `goal_submitted` of a goal named names): its row, by the finding.

use std::collections::BTreeSet;

use serde_json::Value;

use super::super::attributes::{self, replaces_goal};
use super::super::owner::{Owner, owner};
use super::super::post_landing::landed_commit;
use super::super::queue_views::MainSummary;
use super::super::{LedgerKind, LedgerRow, findings};
use crate::domain::stats::timestamp_millis;
use crate::domain::{EventKind, GoalSubmitted, RunEvent};

/// The subject kinds of main's record whose events a summary folds, read
/// after the summary by the store's view of them.
pub const SUMMARIZED_KINDS: [EventKind; 6] = [
    EventKind::MainCommitsRecorded,
    EventKind::MainPathsRecorded,
    EventKind::MainObserved,
    EventKind::MainReadFailed,
    EventKind::MainReadRecovered,
    EventKind::CiChecked,
];

/// The kinds of a goal's own records (a queue row each), which a store
/// takes of a goal named as it walks its records down.
pub const GOAL_KINDS: [EventKind; 6] = [
    EventKind::GoalCreated,
    EventKind::GoalUpdated,
    EventKind::GoalStatusChanged,
    EventKind::GoalClosed,
    EventKind::GoalReopened,
    EventKind::AttributesBaseline,
];

/// What the rows a read takes name of the queue rows read by the views:
/// the goals, the commits its runs landed, the commits its main records
/// revert and the heads of its bases of main's paths.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueNames {
    goals: BTreeSet<i64>,
    landed: BTreeSet<String>,
    shas: BTreeSet<String>,
    heads: BTreeSet<(crate::domain::EventId, String)>,
    closes: BTreeSet<(i64, i64)>,
}

/// What one seek of a store asks: the goals whose records it walks, the
/// commits it reads the records of by their sha, the landed commits whose
/// reverts and turns to red it reads, the proposals whose findings it
/// reads, the goals whose first close after a forecast's snapshot it
/// reads (`closes`, each the goal and the snapshot's event), and whether it
/// reads the baselines' latest marks; all before the time `from_ms` (S).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueWants {
    pub from_ms: i64,
    pub goals: BTreeSet<i64>,
    pub shas: BTreeSet<String>,
    pub landed: BTreeSet<String>,
    pub proposals: BTreeSet<i64>,
    pub closes: BTreeSet<(i64, i64)>,
    pub marks: bool,
}

impl QueueWants {
    /// Whether it asks nothing.
    pub fn is_empty(&self) -> bool {
        self.goals.is_empty()
            && self.shas.is_empty()
            && self.landed.is_empty()
            && self.proposals.is_empty()
            && self.closes.is_empty()
            && !self.marks
    }

    /// These wants less those `sought` asked already.
    #[must_use]
    pub fn less(&self, sought: &Self) -> Self {
        Self {
            from_ms: self.from_ms,
            goals: self.goals.difference(&sought.goals).copied().collect(),
            shas: self.shas.difference(&sought.shas).cloned().collect(),
            landed: self.landed.difference(&sought.landed).cloned().collect(),
            proposals: self
                .proposals
                .difference(&sought.proposals)
                .copied()
                .collect(),
            closes: self.closes.difference(&sought.closes).copied().collect(),
            marks: self.marks && !sought.marks,
        }
    }

    /// These wants with those of `other`.
    pub fn extend(&mut self, other: &Self) {
        self.goals.extend(other.goals.iter().copied());
        self.shas.extend(other.shas.iter().cloned());
        self.landed.extend(other.landed.iter().cloned());
        self.proposals.extend(other.proposals.iter().copied());
        self.closes.extend(other.closes.iter().copied());
        self.marks |= other.marks;
    }
}

/// What a store's seeks give of [`QueueWants`]: the queue's records of
/// before the time they found (each a queue row's of one event, or of the
/// goals walked) and the finding rows read whole.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueueSought {
    pub records: Vec<RunEvent>,
    pub rows: Vec<LedgerRow>,
}

/// What a store gives of main's record before `queue_before`: its latest
/// summary there or before it, and the rows of the summarized kinds after
/// it ([`SUMMARIZED_KINDS`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MainBefore {
    pub summary: Option<MainSummary>,
    /// All summary rows the store read, including those a clock bound
    /// rejected, so the read's measurement counts the actual seek results.
    pub summaries_read: usize,
    pub rows: Vec<LedgerRow>,
}

fn field(event: &RunEvent, key: &str) -> Option<i64> {
    event.payload.get(key).and_then(Value::as_i64)
}

impl QueueNames {
    /// Note what the run, task or session row `row` names: the goals of its
    /// records (an event's own, a payload's `goal_id`, a change of a task's
    /// goal from and to, the goals a task's creation or baseline waits for)
    /// and, of a run row, the commit it landed.
    pub fn note_row(&mut self, row: &LedgerRow) {
        if matches!(row.kind, LedgerKind::Queue | LedgerKind::Node) {
            return;
        }
        for event in &row.records {
            self.note_goals(event);
            if row.kind == LedgerKind::Run && event.kind == EventKind::RunIntegrated {
                self.landed.extend(landed_commit(&event.payload));
            }
        }
    }

    /// Note what the queue row `row` of the range names: the goal of each
    /// record, the commits its main records revert and the heads of its
    /// bases of main's paths.
    pub fn note_queue_row(&mut self, row: &LedgerRow) {
        for event in &row.records {
            self.note_queue_record(event);
        }
    }

    /// [`Self::note_queue_row`] of one record.
    pub fn note_queue_record(&mut self, event: &RunEvent) {
        self.goals.extend(event.goal_id.map(|goal| goal.as_i64()));
        self.note_revert_targets(event);
        if event.kind == EventKind::MainPathsRecorded {
            self.heads.extend(
                event.payload["head"]
                    .as_str()
                    .map(|head| (event.id, head.to_owned())),
            );
        }
    }

    /// Every revert this retained record exposes needs its target's record,
    /// even before S and even when another commit in the same sought event
    /// was the reason for reading it. This also closes transitive targets.
    pub fn note_revert_targets(&mut self, event: &RunEvent) {
        let key = match event.kind.known() {
            Some(EventKind::MainCommitsRecorded) => "commits",
            Some(EventKind::MainRevertsImported) => "reverts",
            _ => return,
        };
        for commit in event.payload[key].as_array().into_iter().flatten() {
            self.shas
                .extend(commit["reverts"].as_str().map(str::to_owned));
        }
    }

    /// Note the goals of the forecast's snapshot `event` (a
    /// `forecast_recorded`, of any time): a forecast's score takes each
    /// one's first close after the snapshot, which may stand before the
    /// time ([`QueueWants::closes`]).
    pub fn note_snapshot(&mut self, event: &RunEvent) {
        if event.kind != EventKind::ForecastRecorded {
            return;
        }
        let goals = event.payload["goals"].as_array().into_iter().flatten();
        self.closes.extend(
            goals
                .filter_map(|goal| goal["id"].as_i64())
                .map(|goal| (goal, event.id.as_i64())),
        );
    }

    /// Note `goal` as named (`--goal`'s).
    pub fn note_goal(&mut self, goal: i64) {
        self.goals.insert(goal);
    }

    fn note_goals(&mut self, event: &RunEvent) {
        self.goals.extend(event.goal_id.map(|goal| goal.as_i64()));
        self.goals.extend(field(event, "goal_id"));
        if event.kind == EventKind::TaskGoalChanged {
            self.goals.extend(field(event, "to"));
            self.goals.extend(field(event, "from"));
        }
        // A task's creation or baseline names the goals it waits for, whose
        // verdicts tell whether it can be claimed.
        if event.task_id.is_some()
            && matches!(
                event.kind.known(),
                Some(EventKind::TaskCreated | EventKind::AttributesBaseline)
            )
        {
            let goals = event.payload["goal_dependencies"].as_array();
            self.goals
                .extend(goals.into_iter().flatten().filter_map(Value::as_i64));
        }
    }

    /// What a seek asks of these names, before `from_ms`, with `proposals`
    /// (those the range names) and `summary` (main's summary taken, whose
    /// commits past a base's head and of a window's times are read by
    /// their sha).
    pub fn wants(
        &self,
        from_ms: i64,
        proposals: &BTreeSet<i64>,
        summary: Option<&MainSummary>,
    ) -> QueueWants {
        let mut shas: BTreeSet<String> = self.shas.union(&self.landed).cloned().collect();
        if let Some(summary) = summary {
            for state in summary.states() {
                for (id, head) in &self.heads {
                    // A prefix's paths record is already folded by its
                    // state at E, even if a future timestamp put its raw
                    // row in the range. Only a later-id base can need the
                    // prefix's commit changes replayed after its head.
                    if summary.through.is_none_or(|through| *id > through) {
                        shas.extend(state.commits_past(head).iter().cloned());
                    }
                }
                shas.extend(state.commits_from(from_ms).into_iter().map(str::to_owned));
            }
        }
        QueueWants {
            from_ms,
            goals: self.goals.clone(),
            shas,
            landed: self.landed.clone(),
            proposals: proposals.clone(),
            closes: self.closes.clone(),
            marks: true,
        }
    }
}

/// Whether `event`, a goal's of the store's walk down its ids, is one of
/// its records a window takes: one of the queue's of [`GOAL_KINDS`] (a
/// task's comes with its row).
pub fn goal_record(event: &RunEvent) -> bool {
    owner(event) == Owner::Queue
        && event.task_id.is_none()
        && event
            .kind
            .known()
            .is_some_and(|kind| GOAL_KINDS.contains(&kind))
}

/// Whether `at`, a record's time, is before `from_ms`: every window of the
/// read holds it, by its time and by its id (a queue record before the
/// time is at or before `queue_before`).
pub fn before(event: &RunEvent, from_ms: i64) -> bool {
    timestamp_millis(&event.created_at).is_some_and(|at| at < from_ms)
}

/// Whether the walk down a goal's records stops at `event`: it replaces
/// the goal ([`replaces_goal`]) before `from_ms`, a baseline with its
/// group's mark (`mark`, the first after it) before it too, so that every
/// window of the read takes it and none of the goal's records before it
/// bears on the goal.
pub fn stops_goal_walk(event: &RunEvent, mark: Option<&RunEvent>, from_ms: i64) -> bool {
    if !replaces_goal(event) || !before(event, from_ms) {
        return false;
    }
    event.kind != EventKind::AttributesBaseline || mark.is_some_and(|mark| before(mark, from_ms))
}

/// Whether `event` is the queue's mark of the baselines of `kind`'s fold:
/// the attributes' ([`attributes::is_mark`]) or the findings'
/// ([`findings::is_mark`]).
pub fn is_mark(kind: EventKind, event: &RunEvent) -> bool {
    match kind {
        EventKind::AttributesBaseline => attributes::is_mark(event),
        EventKind::FindingBaseline => findings::is_mark(event),
        _ => false,
    }
}

/// The proposals the `goal_submitted` among `events` name.
pub fn submitted_proposals(events: &[RunEvent]) -> BTreeSet<i64> {
    events
        .iter()
        .filter_map(GoalSubmitted::of_event)
        .map(|(_, submitted)| submitted.proposal_id.as_i64())
        .collect()
}

/// The finding `event` links to one of `proposals`: a
/// `finding_status_changed` to `proposed` naming it.
pub fn linked_finding(event: &RunEvent, proposals: &BTreeSet<i64>) -> Option<i64> {
    (event.kind == EventKind::FindingStatusChanged
        && event.payload["to"] == "proposed"
        && field(event, "proposal_id").is_some_and(|id| proposals.contains(&id)))
    .then(|| field(event, "finding_id"))
    .flatten()
}

/// Of the records of the rows that record main's commits (a
/// `main_commits_recorded` each), those that record one of `shas` or one
/// reverting one of `landed`.
pub fn records_commit(
    event: &RunEvent,
    shas: &BTreeSet<String>,
    landed: &BTreeSet<String>,
) -> bool {
    event.kind == EventKind::MainCommitsRecorded
        && event.payload["commits"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|commit| {
                commit["sha"].as_str().is_some_and(|sha| shas.contains(sha))
                    || commit["reverts"]
                        .as_str()
                        .is_some_and(|target| landed.contains(target))
            })
}

#[cfg(test)]
mod tests;
