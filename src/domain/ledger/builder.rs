//! What builds the ledger decides (ADR-t1662-1 (ii) and D7,
//! docs/design/measurement.md "台帳を作る係"), pure: which rows to fold
//! again, the fold of each and whether it is written and settled. The
//! supervisor's pass of 観測と分析 holds a [`Builder`], reads the inputs
//! that came in after its cursors, and writes what it says.
//!
//! - **Marks**: each input after a cursor (an EventStore event id, a
//!   NodeSampleStore id) marks what it is an input of
//!   ([`Builder::mark_events`], [`Builder::mark_samples`]): its run, its
//!   task, the session and queue rows it changes (any event's: a claim
//!   ends a deferral, a run's event moves the landing slot), its node and
//!   hour. A `push_finished`
//!   marks the runs this builder left pending on their push and their tasks
//!   (another run's push may close them). No row is scanned to find a dirty
//!   one. A builder starts with its cursors at 0, so a new process marks
//!   every run and task, those before the switch too, and catches up.
//! - **Which are written**: a run whose life ended (its push finished or
//!   was skipped, it ended without a landing, it was canceled), one pending
//!   on its push, and a legacy run; a task that ended, or whose last run is
//!   pending on its push or was until another run's push closed it, and a
//!   legacy task; every session, queue and node
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
//! - **What a pass reads**: the events after its cursor, [`EVENTS_PER_READ`]
//!   at a time, and the events of each run and task it folds. What the
//!   folds of the queue as a whole read of the rest is [`Kept`] between
//!   passes, so no pass reads the whole EventStore again
//!   ([`Inputs::of`]).
//! - **What a pass makes**: each event is read into [`Kept`] once as it
//!   comes in (the supervisors' lives, the waits before a claim, the
//!   landing slot's attempts, which session or queue row it is a record
//!   of), and a pass folds of it only the session and queue rows those
//!   events may have changed and those marked
//!   ([`Builder::rows_to_build`]); a task's fold reads the pushes and the
//!   waits where they are kept. Nothing a pass makes grows with the events
//!   before its new ones.

use crate::domain::EventKind;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::{
    LEDGER_V, LedgerKind, LedgerRow, fold,
    node::{self, BUCKET_SECS},
    owner::{self, Owner},
    queue::{self, QueueBook},
    session::SessionBook,
    settle::{self, SETTLE_GRACE_MS},
};
use crate::domain::host_metrics::NodeSample;
use crate::domain::pre_claim::PreClaimBook;
use crate::domain::supervisor_life::{Lives, supervisor_of};
use crate::domain::{EventId, RunEvent, TaskId};

/// How often the supervisor's pass builds the ledger.
pub const INTERVAL_SECS: u64 = 60;

/// How many rows one pass writes at most: a backfill of every run and
/// task before the switch takes several passes, and no pass holds the
/// queue long.
pub const WRITES_PER_PASS: usize = 200;

/// How many marks one pass looks at at most (a mark of a row the store
/// already holds up to date is looked at and not written).
pub const CHECKS_PER_PASS: usize = 2_000;

/// How many events after the cursor a pass reads at a time: a new process
/// reads every event once, a page at a time.
pub const EVENTS_PER_READ: usize = 5_000;

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

/// What the folds of the queue as a whole (the session and queue rows,
/// the waits before a claim, the pushes) read of the EventStore up to the
/// builder's cursor, kept between passes so that a pass reads only what
/// came in after it: the events a session or queue row owns and the
/// `push_finished`, whole, and what the folds made of every event so far,
/// each read once as it came in ([`Self::add`]): the supervisors' lives,
/// the waits before a claim and the holds on the landings, the landing
/// slot's attempts, and which session or queue row each event is a record
/// of. A pass folds of them only the rows it looks at
/// ([`Inputs::of`]); the events of a run or a task folded are read whole
/// for its fold ([`Inputs::read`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Kept {
    /// The last event read.
    last: EventId,
    /// Ascending id.
    sessions: Vec<RunEvent>,
    queue: Vec<RunEvent>,
    pushes: Vec<RunEvent>,
    lives: Lives,
    pre_claim: PreClaimBook,
    session_rows: SessionBook,
    queue_rows: QueueBook,
}

impl Kept {
    /// The kept of `events` (ascending id).
    pub fn of(events: &[RunEvent]) -> Self {
        let mut kept = Self::default();
        kept.add(events, &mut BTreeSet::new());
        kept
    }

    /// Keep what the folds of the queue as a whole read of `new`
    /// (ascending id), and add to `touched` the session and queue rows
    /// whose fold it changes; one at or before the last kept is kept
    /// already.
    fn add(&mut self, new: &[RunEvent], touched: &mut BTreeSet<(LedgerKind, String)>) {
        for event in new {
            if event.id <= self.last {
                continue;
            }
            self.last = event.id;
            self.lives.add(event);
            let moved = self.pre_claim.add(event);
            let owned = owner::owner(event);
            self.queue_rows
                .add(event, owned == Owner::Queue, moved, touched);
            match owned {
                Owner::Session => {
                    self.session_rows.add(event, touched);
                    self.sessions.push(event.clone());
                }
                Owner::Queue => self.queue.push(event.clone()),
                Owner::Run | Owner::Task => {}
            }
            if event.kind == EventKind::PushFinished {
                self.pushes.push(event.clone());
            }
        }
    }

    /// The session or queue row of `kind` and `key` at `now_ms`, built at
    /// `built_at`; none for a key of no row.
    fn row(&self, kind: LedgerKind, key: &str, now_ms: i64, built_at: &str) -> Option<LedgerRow> {
        match kind {
            LedgerKind::Session => self.session_rows.row(key, &self.sessions, built_at),
            LedgerKind::Queue => self.queue_rows.row(
                key,
                &queue::At {
                    owned: &self.queue,
                    pre_claim: &self.pre_claim,
                    lives: &self.lives,
                    now_ms,
                    built_at,
                },
            ),
            _ => None,
        }
    }

    /// The keys of every session and queue row.
    fn keys(&self) -> impl Iterator<Item = (LedgerKind, String)> + '_ {
        self.session_rows
            .keys()
            .map(|key| (LedgerKind::Session, key.to_owned()))
            .chain(self.queue_rows.keys().map(|key| (LedgerKind::Queue, key)))
    }
}

/// What builds the ledger keeps between passes: its cursors over each
/// input, the marks not written yet, the rows it waits on, and what the
/// folds of the queue as a whole read of the events up to its cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Builder {
    /// The last EventStore event read.
    pub events: EventId,
    /// The last NodeSampleStore id read.
    pub samples: i64,
    marks: BTreeSet<Target>,
    /// Session and queue rows with an input after this event are to be
    /// looked at: set by an event of theirs, one that changed one of them
    /// or one naming a supervisor (whose life may end their intervals),
    /// and taken by [`Self::mark_rows`].
    rows_after: Option<EventId>,
    /// Rows ended and not settled, by when their grace is over (unix ms).
    unsettled: BTreeMap<Target, i64>,
    /// The runs left pending on their push, each with its task.
    pending_pushes: BTreeMap<String, Option<TaskId>>,
    /// The session and queue rows an event read since the rows were last
    /// built may have changed ([`Kept::add`]).
    touched: BTreeSet<(LedgerKind, String)>,
    /// Shared with the pass's [`Inputs`], which it drops before the next
    /// events come in: an [`Inputs`] kept past them would copy it whole.
    kept: Arc<Kept>,
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
            touched: BTreeSet::new(),
            kept: Arc::default(),
        }
    }
}

impl Builder {
    /// Mark what the events `new` (ascending id, after [`Self::events`])
    /// are inputs of, keep what the folds of the queue as a whole read of
    /// them, and move the cursor past them.
    pub fn mark_events(&mut self, new: &[RunEvent]) {
        let kept = Arc::make_mut(&mut self.kept);
        for event in new {
            let mut touched = BTreeSet::new();
            kept.add(std::slice::from_ref(event), &mut touched);
            if let Some(run) = &event.run_id {
                self.marks.insert(Target::Run(run.to_string()));
            }
            if let Some(task) = event.task_id {
                self.marks.insert(Target::Task(task));
            }
            // A row it changed reads it (a run's event too, that ends a
            // deferral or moves the landing slot); a supervisor's life may
            // end rows.
            if matches!(owner::owner(event), Owner::Session | Owner::Queue)
                || !touched.is_empty()
                || supervisor_of(event).is_some()
            {
                self.rows_after.get_or_insert(self.events);
            }
            self.touched.append(&mut touched);
            if event.kind == EventKind::PushFinished {
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

    /// The session and queue rows a pass builds when [`Self::rows_marked`]:
    /// those an event read since they were last built may have changed,
    /// the landing slot's open one (its inputs reach every event) and
    /// those marked; none otherwise. Every row that read an input after
    /// the event the marks started at is among them.
    pub fn rows_to_build(&self, kept: &Kept) -> BTreeSet<(LedgerKind, String)> {
        if !self.rows_marked() {
            return BTreeSet::new();
        }
        let mut rows = self.touched.clone();
        rows.extend(
            kept.queue_rows
                .open_slot()
                .map(|key| (LedgerKind::Queue, key)),
        );
        rows.extend(self.marks.iter().filter_map(|target| match target {
            Target::Row(kind, key) => Some((*kind, key.clone())),
            _ => None,
        }));
        rows
    }

    /// Mark the session and queue rows of `rows` (those
    /// [`Self::rows_to_build`] names, as [`build`] folds them) that read an
    /// input after the event the marks started at.
    pub fn mark_rows(&mut self, rows: &BTreeMap<(LedgerKind, String), LedgerRow>) {
        self.touched.clear();
        let Some(after) = self.rows_after.take() else {
            return;
        };
        for ((kind, key), row) in rows {
            if row.inputs_through.last() > after {
                self.marks.insert(Target::Row(*kind, key.clone()));
            }
        }
    }

    /// Mark again the rows whose grace is over at `now_ms`, and close where
    /// they are kept the slots full whose supervisor's life ended by then,
    /// so that no read of them this pass makes them again: of the kept
    /// ends, only those that came by then ([`PreClaimBook::close_lives`]).
    pub fn due(&mut self, now_ms: i64) {
        let kept = Arc::make_mut(&mut self.kept);
        kept.pre_claim.close_lives(&kept.lives, now_ms);
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

    /// What the folds of the queue as a whole read, up to [`Self::events`].
    pub const fn kept(&self) -> &Arc<Kept> {
        &self.kept
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

/// Whether the task row `row`'s last run waits on a push or waited on one
/// another run's push closed (the task not ended after it): its last
/// interval of a run is `push_pending`. The row it was written as while
/// pending is folded again once the push closes it.
fn run_waited_on_push(row: &LedgerRow) -> bool {
    row.intervals
        .iter()
        .rev()
        .find(|interval| interval.run_id.is_some())
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

/// What a pass folds its marks of: the events of each run and task
/// (ascending id), what the folds of the queue as a whole read ([`Kept`]:
/// the pushes and the waits before a claim, read where they are kept),
/// and the session and queue rows folded of them.
#[derive(Debug, Clone)]
pub struct Inputs {
    pub now_ms: i64,
    pub built_at: String,
    runs: BTreeMap<String, Vec<RunEvent>>,
    tasks: BTreeMap<TaskId, Vec<RunEvent>>,
    kept: Arc<Kept>,
    /// The session and queue rows built.
    pub rows: BTreeMap<(LedgerKind, String), LedgerRow>,
}

impl Inputs {
    /// The inputs of `events` (the whole EventStore) at `now_ms`, built at
    /// `built_at`, every run's and task's events among them; with `rows`,
    /// every session and queue row too.
    pub fn new(events: &[RunEvent], now_ms: i64, built_at: &str, rows: bool) -> Self {
        let mut runs: BTreeMap<String, Vec<RunEvent>> = BTreeMap::new();
        let mut tasks: BTreeMap<TaskId, Vec<RunEvent>> = BTreeMap::new();
        for event in events {
            if let Some(run) = &event.run_id {
                runs.entry(run.to_string()).or_default().push(event.clone());
            }
            if let Some(task) = event.task_id {
                tasks.entry(task).or_default().push(event.clone());
            }
        }
        let kept = Arc::new(Kept::of(events));
        let keys: BTreeSet<(LedgerKind, String)> = if rows {
            kept.keys().collect()
        } else {
            BTreeSet::new()
        };
        let mut inputs = Self::of(&kept, now_ms, built_at, &keys);
        inputs.runs = runs;
        inputs.tasks = tasks;
        inputs
    }

    /// The inputs of what `kept` holds at `now_ms`, built at `built_at`,
    /// the session and queue rows of `rows` among them (a key of no row
    /// builds none). No run's or task's events are among them until
    /// [`Self::read`] gives them.
    pub fn of(
        kept: &Arc<Kept>,
        now_ms: i64,
        built_at: &str,
        rows: &BTreeSet<(LedgerKind, String)>,
    ) -> Self {
        let built = rows
            .iter()
            .filter_map(|(kind, key)| {
                let row = kept.row(*kind, key, now_ms, built_at)?;
                Some(((*kind, key.clone()), row))
            })
            .collect();
        Self {
            now_ms,
            built_at: built_at.to_owned(),
            runs: BTreeMap::new(),
            tasks: BTreeMap::new(),
            kept: Arc::clone(kept),
            rows: built,
        }
    }

    /// Give the run or task `target` its events, `events` (ascending id,
    /// the EventStore's on it up to the builder's cursor), for [`build`]
    /// to fold; none
    /// leaves it with no events. Another target takes none.
    pub fn read(&mut self, target: &Target, events: Vec<RunEvent>) {
        if events.is_empty() {
            return;
        }
        match target {
            Target::Run(run) => {
                self.runs.insert(run.clone(), events);
            }
            Target::Task(task) => {
                self.tasks.insert(*task, events);
            }
            Target::Row(..) | Target::Node(..) => {}
        }
    }

    /// Drop the events [`Self::read`] gave `target`, once folded.
    pub fn forget(&mut self, target: &Target) {
        match target {
            Target::Run(run) => {
                self.runs.remove(run);
            }
            Target::Task(task) => {
                self.tasks.remove(task);
            }
            Target::Row(..) | Target::Node(..) => {}
        }
    }
}

/// The row of `target` as it is written now, settled or not: none for a
/// run or task on its way (or with no events), a session or queue row not
/// among `inputs.rows`, or a node hour with no sample left. `samples` are
/// the NodeSampleStore's of a node target's hour.
pub fn build(target: &Target, inputs: &Inputs, samples: &[NodeSample]) -> Option<LedgerRow> {
    let now = inputs.now_ms;
    let kept = &*inputs.kept;
    match target {
        Target::Run(run) => {
            let events = inputs.runs.get(run)?;
            let mut row = super::run_row(run, events, &kept.pushes, now, &inputs.built_at);
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
                &kept.pushes,
                kept.pre_claim.intervals(&kept.lives, now),
                now,
                &inputs.built_at,
            );
            if !(row.is_legacy() || row.ended_at.is_some() || run_waited_on_push(&row)) {
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
