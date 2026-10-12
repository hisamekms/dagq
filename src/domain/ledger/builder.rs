//! What builds the ledger decides (ADR-t1662-1 (ii) and D7,
//! docs/design/measurement.md "台帳を作る係"), pure: which rows to fold
//! again, the fold of each and whether it is written and settled. The
//! supervisor's pass of 観測と分析 holds a [`Builder`], reads the inputs
//! that came in after its cursors, and writes what it says.
//!
//! - **Marks**: each input after a cursor (an EventStore event id, a
//!   NodeSampleStore id) marks what it is an input of
//!   ([`Builder::mark_events`], [`Builder::mark_samples`]): its run, its
//!   task, the session and queue rows, its node and hour. A `push_finished`
//!   marks the runs this builder left pending on their push and their tasks
//!   (another run's push may close them). No row is scanned to find a dirty
//!   one. A builder starts with its cursors at 0, so a new process marks
//!   every run and task, those before the switch too, and catches up.
//! - **Which are written**: a run whose life ended (its push finished or
//!   was skipped, it ended without a landing, it was canceled), one pending
//!   on its push, and a legacy run; a task that ended, or whose run is
//!   pending on its push, and a legacy task; every session, queue and node
//!   row ([`build`]). A run or task on its way is not written: its next
//!   event marks it again. A row is written when the store has none, when
//!   its fold is older than [`LEDGER_V`] (a legacy row aside, whose version
//!   the store keeps as first written), when the new fold read an input
//!   past the stored one's `inputs_through` (dirty), and when it is now
//!   settled ([`writes`]). The same inputs fold to the same row, which is
//!   not written again.
//! - **Settled** (`final`): a run or task once it ended (a pending push
//!   has not) and [`SETTLE_GRACE_MS`] passed ([`run_settles`]); a session,
//!   queue or node row by [`super::settle`]. A settled row an input reaches
//!   later is folded and written again, and settled again after its grace.
//!   A row ended and not settled yet is marked again when its grace is
//!   over ([`Builder::due`]).
//! - **Bounds**: at most [`WRITES_PER_PASS`] rows written and
//!   [`CHECKS_PER_PASS`] marks looked at per pass; the rest stay marked for
//!   the next pass.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    LEDGER_V, LedgerKind, LedgerRow, fold,
    node::{self, BUCKET_SECS},
    owner::{self, Owner},
    queue, session,
    settle::{self, SETTLE_GRACE_MS},
};
use crate::domain::host_metrics::NodeSample;
use crate::domain::pre_claim::{PreClaimInterval, pre_claim_intervals};
use crate::domain::supervisor_life::supervisor_of;
use crate::domain::{EventId, RunEvent, TaskId, event_kind};

/// How often the supervisor's pass builds the ledger.
pub const INTERVAL_SECS: u64 = 60;

/// How many rows one pass writes at most: a backfill of every run and
/// task before the switch takes several passes, and no pass holds the
/// queue long.
pub const WRITES_PER_PASS: usize = 200;

/// How many marks one pass looks at at most (a mark of a row the store
/// already holds up to date is looked at and not written).
pub const CHECKS_PER_PASS: usize = 2_000;

/// What a mark is of.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Target {
    Run(String),
    Task(TaskId),
    /// A session or queue row, by its kind and key.
    Row(LedgerKind, String),
    /// A node's hour, by the node and the hour's first unix second.
    Node(String, i64),
}

impl Target {
    /// The kind and key of the row it is of.
    pub fn row_key(&self) -> (LedgerKind, String) {
        match self {
            Self::Run(run) => (LedgerKind::Run, run.clone()),
            Self::Task(task) => (LedgerKind::Task, task.to_string()),
            Self::Row(kind, key) => (*kind, key.clone()),
            Self::Node(node, start) => (LedgerKind::Node, node::key(node, *start)),
        }
    }
}

/// What builds the ledger keeps between passes: its cursors over each
/// input, the marks not written yet, and the rows it waits on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Builder {
    /// The last EventStore event read.
    pub events: EventId,
    /// The last NodeSampleStore id read.
    pub samples: i64,
    marks: BTreeSet<Target>,
    /// Session and queue rows with an input after this event are to be
    /// looked at: set by an event of theirs (or naming a supervisor, whose
    /// life may end their intervals) and taken by [`Self::mark_rows`].
    rows_after: Option<EventId>,
    /// Rows ended and not settled, by when their grace is over (unix ms).
    unsettled: BTreeMap<Target, i64>,
    /// The runs left pending on their push, each with its task.
    pending_pushes: BTreeMap<String, Option<TaskId>>,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            events: EventId::new(0),
            samples: 0,
            marks: BTreeSet::new(),
            rows_after: None,
            unsettled: BTreeMap::new(),
            pending_pushes: BTreeMap::new(),
        }
    }
}

impl Builder {
    /// Mark what the events `new` (ascending id, after [`Self::events`])
    /// are inputs of, and move the cursor past them.
    pub fn mark_events(&mut self, new: &[RunEvent]) {
        for event in new {
            if let Some(run) = &event.run_id {
                self.marks.insert(Target::Run(run.to_string()));
            }
            if let Some(task) = event.task_id {
                self.marks.insert(Target::Task(task));
            }
            let owned = owner::owner(event);
            if matches!(owned, Owner::Session | Owner::Queue) || supervisor_of(event).is_some() {
                self.rows_after.get_or_insert(self.events);
            }
            if event.kind == event_kind::PUSH_FINISHED {
                for (run, task) in &self.pending_pushes {
                    self.marks.insert(Target::Run(run.clone()));
                    if let Some(task) = task {
                        self.marks.insert(Target::Task(*task));
                    }
                }
            }
            self.events = self.events.max(event.id);
        }
    }

    /// Mark the node hours the samples `new` (after [`Self::samples`]) are
    /// of, and move the cursor past them.
    pub fn mark_samples(&mut self, new: &[NodeSample]) {
        for sample in new {
            self.marks.insert(Target::Node(
                sample.node_id.clone(),
                node::bucket_of(sample.sample.unix),
            ));
            self.samples = self.samples.max(sample.id);
        }
    }

    /// Whether the session and queue rows are to be built this pass: an
    /// input of theirs came in, or a mark of one waits (its grace is over,
    /// a pass ran out of its bound, a write failed).
    pub fn rows_marked(&self) -> bool {
        self.rows_after.is_some()
            || self
                .marks
                .iter()
                .any(|target| matches!(target, Target::Row(..)))
    }

    /// Mark the session and queue rows of `rows` (all of them, as [`build`]
    /// folds them) that read an input after the event the marks started at.
    pub fn mark_rows(&mut self, rows: &BTreeMap<(LedgerKind, String), LedgerRow>) {
        let Some(after) = self.rows_after.take() else {
            return;
        };
        for ((kind, key), row) in rows {
            if row.inputs_through.last() > after {
                self.marks.insert(Target::Row(*kind, key.clone()));
            }
        }
    }

    /// Mark again the rows whose grace is over at `now_ms`.
    pub fn due(&mut self, now_ms: i64) {
        let due: Vec<Target> = self
            .unsettled
            .iter()
            .filter(|(_, at)| **at <= now_ms)
            .map(|(target, _)| target.clone())
            .collect();
        for target in due {
            self.unsettled.remove(&target);
            self.marks.insert(target);
        }
    }

    /// Whether anything is marked.
    pub fn marked(&self) -> bool {
        !self.marks.is_empty() || self.rows_after.is_some()
    }

    /// Take up to `limit` marks, runs first.
    pub fn take(&mut self, limit: usize) -> Vec<Target> {
        let taken: Vec<Target> = self.marks.iter().take(limit).cloned().collect();
        for target in &taken {
            self.marks.remove(target);
        }
        taken
    }

    /// Put `target` back to be looked at next pass (its write failed, or
    /// the pass ran out of its bound).
    pub fn keep(&mut self, target: Target) {
        self.marks.insert(target);
    }

    /// Note what the store holds of `target` now, `row`: wait on its grace
    /// while it ended and is not settled, and on a later push while it is
    /// pending on its push.
    pub fn track(&mut self, target: &Target, row: &LedgerRow) {
        match settles_at(row) {
            Some(at) if !row.is_final => {
                self.unsettled.insert(target.clone(), at);
            }
            _ => {
                self.unsettled.remove(target);
            }
        }
        if let Target::Run(run) = target {
            if pending_push(row) {
                self.pending_pushes.insert(run.clone(), row.task_id);
            } else {
                self.pending_pushes.remove(run);
            }
        }
    }

    /// How many marks wait.
    pub fn backlog(&self) -> usize {
        self.marks.len()
    }
}

/// When `row`, ended, settles once no later input reaches it: its end and
/// its grace. None for a row not ended (or a legacy one, settled as it is
/// written).
fn settles_at(row: &LedgerRow) -> Option<i64> {
    if row.is_legacy() {
        return None;
    }
    let end = row.ended_at?;
    let grace = match row.kind {
        LedgerKind::Session | LedgerKind::Queue => settle::grace(row),
        _ => SETTLE_GRACE_MS,
    };
    Some(end + grace)
}

/// Whether `row` waits on a later push: not ended, its last interval
/// `push_pending`.
pub fn pending_push(row: &LedgerRow) -> bool {
    row.ended_at.is_none()
        && row
            .intervals
            .last()
            .is_some_and(|last| last.phase == fold::PUSH_PENDING)
}

/// Whether the run or task row `row` is settled at `now_ms`: a legacy row
/// is (a run from before the switch ended long ago); a pending push is
/// not; else once it ended and [`SETTLE_GRACE_MS`] passed.
pub fn run_settles(row: &LedgerRow, now_ms: i64) -> bool {
    row.is_legacy()
        || row
            .ended_at
            .is_some_and(|end| now_ms - end >= SETTLE_GRACE_MS)
}

/// Whether the fold `built` is written over `stored` (none when the store
/// has none): a missing row, one of an older [`LEDGER_V`] (a newer binary's is left as it
/// is; not a legacy one, whose version the store keeps), one that read fewer inputs than
/// `built` (dirty: a later event, the push that closed its pending push, a
/// later sample, a supervisor's later event), and one settled now. A
/// stored row that holds the same inputs is not written again, so the same
/// inputs read twice leave it as it is.
pub fn writes(stored: Option<&LedgerRow>, built: &LedgerRow) -> bool {
    let Some(stored) = stored else {
        return true;
    };
    (!stored.is_legacy() && stored.ledger_v < LEDGER_V)
        || built.inputs_through.last() > stored.inputs_through.last()
        || built.inputs_through.node_samples > stored.inputs_through.node_samples
        || (built.is_final && !stored.is_final && !stored.is_legacy())
}

/// What a pass folds its marks of: the whole EventStore (ascending id),
/// the queue's pushes and waits before a claim, and the session and queue
/// rows folded of them.
#[derive(Debug, Clone)]
pub struct Inputs {
    pub now_ms: i64,
    pub built_at: String,
    runs: BTreeMap<String, Vec<RunEvent>>,
    tasks: BTreeMap<TaskId, Vec<RunEvent>>,
    pushes: Vec<RunEvent>,
    pre_claim: Vec<PreClaimInterval>,
    /// The session and queue rows, built only when they are marked.
    pub rows: BTreeMap<(LedgerKind, String), LedgerRow>,
}

impl Inputs {
    /// The inputs of `events` (the whole EventStore) at `now_ms`, built at
    /// `built_at`; with `rows`, the session and queue rows too.
    pub fn new(events: &[RunEvent], now_ms: i64, built_at: &str, rows: bool) -> Self {
        let mut runs: BTreeMap<String, Vec<RunEvent>> = BTreeMap::new();
        let mut tasks: BTreeMap<TaskId, Vec<RunEvent>> = BTreeMap::new();
        let (mut sessions, mut queue) = (Vec::new(), Vec::new());
        for event in events {
            if let Some(run) = &event.run_id {
                runs.entry(run.to_string()).or_default().push(event.clone());
            }
            if let Some(task) = event.task_id {
                tasks.entry(task).or_default().push(event.clone());
            }
            if rows {
                match owner::owner(event) {
                    Owner::Session => sessions.push(event.clone()),
                    Owner::Queue => queue.push(event.clone()),
                    Owner::Run | Owner::Task => {}
                }
            }
        }
        let mut built = BTreeMap::new();
        if rows {
            for row in session::session_rows(&sessions, built_at)
                .into_iter()
                .chain(queue::queue_rows(&queue, events, now_ms, built_at))
            {
                built.insert((row.kind, row.key.clone()), row);
            }
        }
        Self {
            now_ms,
            built_at: built_at.to_owned(),
            runs,
            tasks,
            pushes: events
                .iter()
                .filter(|event| event.kind == event_kind::PUSH_FINISHED)
                .cloned()
                .collect(),
            pre_claim: pre_claim_intervals(events, now_ms),
            rows: built,
        }
    }
}

/// The row of `target` as it is written now, settled or not: none for a
/// run or task on its way (or with no events), a session or queue row not
/// among `inputs.rows`, or a node hour with no sample left. `samples` are
/// the NodeSampleStore's of a node target's hour.
pub fn build(target: &Target, inputs: &Inputs, samples: &[NodeSample]) -> Option<LedgerRow> {
    let now = inputs.now_ms;
    match target {
        Target::Run(run) => {
            let events = inputs.runs.get(run)?;
            let mut row = super::run_row(run, events, &inputs.pushes, now, &inputs.built_at);
            if !(row.is_legacy() || row.ended_at.is_some() || pending_push(&row)) {
                return None;
            }
            row.is_final = run_settles(&row, now);
            Some(row)
        }
        Target::Task(task) => {
            let events = inputs.tasks.get(task)?;
            // As the ledger's build: a task with a record of its own or a run.
            if !events
                .iter()
                .any(|event| owner::owner(event) == Owner::Task || event.run_id.is_some())
            {
                return None;
            }
            let mut row = super::task_row(
                *task,
                events,
                &inputs.pushes,
                &inputs.pre_claim,
                now,
                &inputs.built_at,
            );
            if !(row.is_legacy() || row.ended_at.is_some() || pending_push(&row)) {
                return None;
            }
            row.is_final = run_settles(&row, now);
            Some(row)
        }
        Target::Row(kind, key) => {
            let mut row = inputs.rows.get(&(*kind, key.clone()))?.clone();
            row.is_final = settle::settlement(&row, row.inputs_through.last(), now).settles;
            Some(row)
        }
        Target::Node(node_id, start) => {
            let of: Vec<&NodeSample> = samples
                .iter()
                .filter(|sample| {
                    sample.node_id == *node_id && node::bucket_of(sample.sample.unix) == *start
                })
                .collect();
            if of.is_empty() {
                return None;
            }
            let mut row = node::node_row(node_id, *start, &of, &inputs.built_at);
            row.is_final =
                settle::node_settlement(&row, row.inputs_through.node_samples, now).settles;
            Some(row)
        }
    }
}

/// The unix seconds of a node target's hour, both inclusive.
pub const fn hour(start: i64) -> (i64, i64) {
    (start, start + BUCKET_SECS - 1)
}

#[cfg(test)]
mod tests;
