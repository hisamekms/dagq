//! The summaries of the pushes [`Kept`] holds (the runs waiting on a push,
//! where each run's last failed push went, the push that closed each run's
//! pending push, and the runs and tasks not settled that read each push
//! kept), made again from the rows the store holds as they were at a
//! position ([`PushesHeld::at`]), so that a builder that goes on from
//! there ([`super::Builder::restore_pushes`]) reads back by id the push a
//! settled run's late event folds it on, as the fold of every event does.
//!
//! They are a fold of the runs' own `push_failed` and `run_phase_changed`
//! and of every `push_finished` ([`Kept::wait_on_push`]), each a record of
//! its run's row, or of its task's or the queue's for a push of no run
//! ([`super::super::owner`]); only where a push went (`remote`, `branch`)
//! and a phase's name are read of them ([`cut`]). A run on its way has no
//! row, and the row of one that went on its way since it was written
//! (from a pending push, closed or not) records none of its later events:
//! the records of either are its events, read whatever its tasks, and
//! whether it was settled at the position is its fold up to it
//! ([`PushesHeld::events_settled`]).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use super::{Kept, Target, Waiting};
use crate::domain::environment::RunResource;
use crate::domain::ledger::fold::PushedTo;
use crate::domain::ledger::{LEDGER_V, LedgerHead, LedgerKind, LedgerRow, window};
use crate::domain::pre_claim::PreClaimInterval;
use crate::domain::{EventId, EventKind, RunEvent, TaskId};

pub mod page;

/// Whether `event` is one the summaries of the pushes are a fold of: any
/// `push_finished`, a run's or not, and a run's `push_failed` or
/// `run_phase_changed`.
pub fn is_push_record(event: &RunEvent) -> bool {
    event.kind == EventKind::PushFinished
        || (event.run_id.is_some()
            && (event.kind == EventKind::PushFailed || event.kind == EventKind::RunPhaseChanged))
}

/// `event` with only what the summaries read of its payload: where a push
/// went (`remote` and `branch`, as [`PushedTo`] reads them), and a phase's
/// name ([`crate::domain::run_phase::Recorded`]).
pub fn cut(event: &RunEvent) -> RunEvent {
    let keys: &[&str] = if event.kind == EventKind::RunPhaseChanged {
        &["phase"]
    } else {
        &["remote", "branch"]
    };
    let payload: Map<String, Value> = keys
        .iter()
        .filter_map(|key| Some(((*key).to_owned(), event.payload.get(*key)?.clone())))
        .collect();
    RunEvent {
        id: event.id,
        task_id: event.task_id,
        goal_id: event.goal_id,
        run_id: event.run_id.clone(),
        kind: event.kind.clone(),
        payload: Value::Object(payload),
        created_at: event.created_at.clone(),
        actor: None,
    }
}

/// Whether the run or task row of `head` was settled at `through` and the
/// cursor `resources` of the runs' windows measured from outside, as the
/// store holds it: settled by today's fold ([`LEDGER_V`]), and no input it
/// folded (its records, its events, a life's last event, a wait's reach, a
/// window) came after them ([`reaches_past`]). Whether one an input after
/// them reaches was settled at them only its fold up to them says
/// ([`PushesHeld::settled_folded`]), and so of a `legacy` row: the store
/// keeps its mark as first written, and a run of it may record its phases
/// since, which the fold reads as a run of today's form.
pub fn settled_at(head: &LedgerHead, legacy: bool, through: EventId, resources: i64) -> bool {
    !legacy && head.is_final && head.ledger_v == LEDGER_V && !reaches_past(head, through, resources)
}

/// Whether an input the row of `head` folded came after `through`, or a
/// window of its run's resources after the cursor `resources` (a run's
/// settling waits on the supervisor's last measure of them): its mark of
/// settled says nothing of it at them, and only a fold up to them does
/// ([`PushesHeld::settled_folded`]).
fn reaches_past(head: &LedgerHead, through: EventId, resources: i64) -> bool {
    let inputs = &head.inputs_through;
    inputs.events > through
        || inputs.resources.is_some_and(|id| id > resources)
        || inputs.supervisor.is_some_and(|id| id > through)
        || inputs
            .life_ended
            .iter()
            .any(|(start, reach)| *start > through || *reach > through)
        || head.records.iter().any(|id| *id > through)
}

/// The summaries of the pushes [`Kept`] made of the events up to
/// `through`, made again from the store's rows ([`Self::at`]).
#[derive(Debug, Clone, Default)]
pub struct PushesHeld {
    pub through: EventId,
    pub(super) waiting_push: BTreeMap<String, Waiting>,
    pub(super) failed_push: BTreeMap<String, PushedTo>,
    pub(super) closing: BTreeMap<String, EventId>,
    pub(super) push_readers: BTreeMap<EventId, BTreeSet<Target>>,
    /// The pushes that closed a pending push, cut, which a fold of a
    /// reader up to `through` reads ([`Self::settled_folded`]); not a
    /// summary, and dropped once it settled.
    pub(super) closings: Vec<RunEvent>,
}

/// The summaries alone: the pushes it reads as it settles are not.
impl PartialEq for PushesHeld {
    fn eq(&self, other: &Self) -> bool {
        (
            self.through,
            &self.waiting_push,
            &self.failed_push,
            &self.closing,
            &self.push_readers,
        ) == (
            other.through,
            &other.waiting_push,
            &other.failed_push,
            &other.closing,
            &other.push_readers,
        )
    }
}

impl Eq for PushesHeld {}

impl PushesHeld {
    /// The summaries of `records` up to `through`: the records of the run
    /// rows the store holds ([`is_push_record`], cut or whole), the pushes
    /// of no run (records of a task's row or the queue's), and the events of
    /// the runs whose rows do not record every event of theirs up to it
    /// (those on their way with no row, and those that went on their way
    /// since their row was written, which keeps that row); those after
    /// `through` and any other are not read. Every run and task a push
    /// closed reads it until [`Self::settle`] says which are settled.
    pub fn at(records: impl IntoIterator<Item = RunEvent>, through: EventId) -> Self {
        let mut records: Vec<RunEvent> = records
            .into_iter()
            .filter(|event| event.id <= through && is_push_record(event))
            .collect();
        records.sort_by_key(|event| event.id);
        records.dedup_by_key(|event| event.id);
        let mut kept = Kept::default();
        for event in &records {
            kept.wait_on_push(event);
        }
        Self {
            through,
            waiting_push: kept.waiting_push,
            failed_push: kept.failed_push,
            closing: kept.closing,
            push_readers: kept.push_readers,
            closings: kept.pushes,
        }
    }

    /// Every run and task that reads a push, settled or not.
    pub fn readers(&self) -> BTreeSet<Target> {
        self.push_readers.values().flatten().cloned().collect()
    }

    /// Whether the run or task of `row`, the store's row of a reader not
    /// [`settled_at`] `through`, was settled at `through` and `now_ms`, as
    /// a build of every event up to it tracked it (a row written before the
    /// push that closed its pending push is behind though it records every
    /// event of its run): its fold over `events`, its events up to it (a
    /// task's, its own records and its runs'), on the pushes that closed a
    /// pending push.
    /// A run waits on its token's revocation as recorded by then and on
    /// the supervisor's last measure of its resources among `windows` (its
    /// windows up to the cursor the position has of them,
    /// [`crate::domain::environment::resources_recorded`]), not on what
    /// `row` holds of them; the waits before a claim, which do not move an
    /// end, are not read. A fold that is legacy (a task with a run from
    /// before the switch) is settled as the build writes it
    /// ([`super::run_settles`]); a row of an older version is not, but a
    /// legacy one, whose version the store keeps as first written.
    pub fn settled_folded(
        &self,
        row: &LedgerRow,
        events: Vec<RunEvent>,
        windows: &[RunResource],
        now_ms: i64,
    ) -> bool {
        // The store keeps a legacy row's version as first written, and its
        // records follow every write.
        if row.ledger_v != LEDGER_V && !row.is_legacy() {
            return false;
        }
        let mut events: Vec<RunEvent> = events
            .into_iter()
            .filter(|event| event.id <= self.through)
            .collect();
        events.sort_by_key(|event| event.id);
        events.dedup_by_key(|event| event.id);
        if events.is_empty() {
            return false;
        }
        match target_of(&LedgerHead::of(row)) {
            Some(Target::Run(run)) => {
                let folded = super::super::run_row(&run, &events, &self.closings, now_ms, "");
                run_folded_settles(&folded, windows, now_ms)
            }
            Some(Target::Task(task)) => {
                let folded = super::super::task_row(
                    task,
                    &events,
                    &self.closings,
                    std::iter::empty::<PreClaimInterval>(),
                    now_ms,
                    "",
                );
                super::run_settles(&folded, now_ms)
            }
            _ => false,
        }
    }

    /// Whether the run `run`, whose row in the store does not record its
    /// events up to `through` (none: on its way where the rows were
    /// written; or the one a run that went on its way since keeps), was
    /// settled at `through` and `now_ms` as a build
    /// of every event up to it tracked it: its fold over `events` (its
    /// events) up to it, on the pushes that closed a pending push, waiting
    /// on its token's revocation as recorded by then and on the
    /// supervisor's last measure of its resources among `windows` (its
    /// windows recorded by then,
    /// [`crate::domain::environment::resources_recorded`]), or, a legacy
    /// fold, as the build writes it.
    pub fn events_settled(
        &self,
        run: &str,
        events: &[RunEvent],
        windows: &[RunResource],
        now_ms: i64,
    ) -> bool {
        let mut events: Vec<RunEvent> = events
            .iter()
            .filter(|event| event.id <= self.through)
            .cloned()
            .collect();
        if events.is_empty() {
            return false;
        }
        events.sort_by_key(|event| event.id);
        events.dedup_by_key(|event| event.id);
        let folded = super::super::run_row(run, &events, &self.closings, now_ms, "");
        run_folded_settles(&folded, windows, now_ms)
    }

    /// Of the runs and tasks that read a push, those not settled at
    /// `through` (`open`: the targets of the heads of the rows not
    /// [`settled_at`] it, but those [`Self::settled_folded`] or
    /// [`Self::events_settled`] says were, and the runs read of their
    /// events not [`Self::events_settled`] at it) read it on; a push none
    /// reads is not kept.
    pub fn settle(&mut self, open: &BTreeSet<Target>) {
        for readers in self.push_readers.values_mut() {
            readers.retain(|reader| open.contains(reader));
        }
        self.push_readers.retain(|_, readers| !readers.is_empty());
        self.closings = Vec::new();
    }

    /// The pushes kept, whose events [`super::Builder::restore_pushes`]
    /// takes (read by id).
    pub fn to_read(&self) -> Vec<EventId> {
        self.push_readers.keys().copied().collect()
    }

    /// Whether it holds a push that closed a pending push of `run`.
    #[cfg(test)]
    pub(crate) fn closed(&self, run: &str) -> bool {
        self.closing.contains_key(run)
    }

    /// Where it and `other` differ, one line each: each key of a summary
    /// either holds otherwise, with both values.
    #[cfg(test)]
    pub(crate) fn differences(&self, other: &Self) -> Vec<String> {
        fn of<K: Ord + std::fmt::Debug, V: PartialEq + std::fmt::Debug>(
            name: &str,
            mine: &BTreeMap<K, V>,
            theirs: &BTreeMap<K, V>,
            out: &mut Vec<String>,
        ) {
            let keys: BTreeSet<&K> = mine.keys().chain(theirs.keys()).collect();
            for key in keys {
                if mine.get(key) != theirs.get(key) {
                    out.push(format!(
                        "{name} {key:?}: {:?} / {:?}",
                        mine.get(key),
                        theirs.get(key)
                    ));
                }
            }
        }
        let mut out = Vec::new();
        of("waiting", &self.waiting_push, &other.waiting_push, &mut out);
        of("failed", &self.failed_push, &other.failed_push, &mut out);
        of("closing", &self.closing, &other.closing, &mut out);
        of("readers", &self.push_readers, &other.push_readers, &mut out);
        out
    }

    /// Whether it holds a push that closed a pending push of any run.
    #[cfg(test)]
    pub(crate) fn closed_any(&self) -> bool {
        !self.closing.is_empty()
    }

    /// Whether it holds where the last failed push of `run` went.
    #[cfg(test)]
    pub(crate) fn failed(&self, run: &str) -> bool {
        self.failed_push.contains_key(run)
    }

    /// The runs waiting on a push, each with its task: those the builder
    /// left pending on their push ([`super::Builder::track`]), whose rows a
    /// later push marks.
    pub(super) fn pending(&self) -> BTreeMap<String, Option<TaskId>> {
        self.waiting_push
            .iter()
            .map(|(run, waiting)| (run.clone(), waiting.task))
            .collect()
    }
}

/// Whether the run's row `folded` (its fold up to a position) is settled
/// at `now_ms` as the build writes it: ended past its grace, its token's
/// revocation recorded, and, but for a legacy one (none folded), the
/// supervisor's last measure of its resources among `windows`.
fn run_folded_settles(folded: &LedgerRow, windows: &[RunResource], now_ms: i64) -> bool {
    super::run_settles(folded, now_ms)
        && super::telemetry_revoked(&folded.records)
        && (folded.is_legacy()
            || crate::domain::environment::resources_recorded(&folded.records, windows))
}

/// The runs the task row `row` names: those of today's fold
/// ([`window::runs_named`]) and those a legacy row names (`legacy.runs`,
/// as the store keeps them since it first wrote the row); none for
/// another kind.
pub fn task_runs(row: &LedgerRow) -> Vec<String> {
    let mut runs = window::runs_named(row);
    if row.kind == LedgerKind::Task {
        runs.extend(
            row.legacy
                .as_ref()
                .and_then(|legacy| legacy.get("runs"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned),
        );
    }
    runs
}

/// The run or task of `head`'s row; none for another kind or a key that
/// does not parse.
pub fn target_of(head: &LedgerHead) -> Option<Target> {
    match head.kind {
        LedgerKind::Run => Some(Target::Run(head.key.clone())),
        LedgerKind::Task => head
            .key
            .parse()
            .ok()
            .map(|id| Target::Task(TaskId::new(id))),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
