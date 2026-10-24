//! What builds the ledger decides (ADR-t1662-1 (ii) and D7,
//! docs/design/measurement.md "台帳を作る係"), pure: which rows to fold
//! again, the fold of each and whether it is written and settled. The
//! supervisor's pass of 観測と分析 holds a [`Builder`], reads the inputs
//! that came in after its cursors, and writes what it says.
//!
//! - **Marks**: each input after a cursor (an EventStore event id, a
//!   NodeSampleStore id, a SessionStepStore id, a run's resources' id)
//!   marks what it is an input of ([`Builder::mark_events`],
//!   [`Builder::mark_samples`], [`Builder::mark_steps`],
//!   [`Builder::mark_resources`]): its run, its task, the session and queue
//!   rows it changes (any event's: a claim ends a deferral, a run's event
//!   moves the landing slot), its node and hour; a step or a window of
//!   resources its run only, and one of a target of no run nothing. A `push_finished`
//!   marks the runs this builder left pending on their push and their tasks
//!   (another run's push may close them). No row is scanned to find a dirty
//!   one. A builder starts with its cursors at 0, so a new process marks
//!   every run and task, those before the switch too, and catches up.
//! - **Which are written**: a run whose life ended (its push finished or
//!   was skipped, it ended without a landing, it was canceled), one pending
//!   on its push, and a legacy run; every task from its `task_created`
//!   (or its first record), ended or not, so that what a task not ended
//!   records (a draft's registration and settling, an ask of no run) is
//!   some row's; every session, queue and node row ([`build`]). A run on
//!   its way is not written: its next event marks it again. A task not
//!   ended is written not settled, its intervals up to the pass's time,
//!   and each event of it (or of its runs) marks it again, until it ends
//!   and settles. The waits before a claim that bear on its time ready are
//!   inputs of it (their last event,
//!   [`crate::domain::pre_claim::PreClaimInterval::reach`], and the starts
//!   of those a supervisor's life ended, [`fold::fold_task`]), so that a
//!   fold over fewer events (their times in their order) is [`behind`] one
//!   over more, and while its row
//!   is not settled and has time ready the waits that move mark it, so
//!   that its overlays follow: a pass that finds the life of the supervisor
//!   of a slots full not freed ended (its silence or stop,
//!   [`Builder::due`]) and an event naming it after that end mark it;
//!   while it is ready now or a wait still open bears on its time ready
//!   that ended, so does an event that moves a wait (starts or ends one,
//!   names such a supervisor). A time ready that ended with no wait open on
//!   it reads no other move but one timed before an event read before it.
//!   A row is written when the store has none, when
//!   its fold is older than [`LEDGER_V`] (a legacy row aside, whose version
//!   the store keeps as first written), when the new fold read an input
//!   past the stored one's `inputs_through` (dirty), and when it is now
//!   settled ([`writes`]). The same inputs fold to the same row, which is
//!   not written again. A fold that read less of some input than the
//!   stored row (another supervisor's builder, its cursors ahead or its
//!   pass at a later time, wrote it: a task's time is an input, past a
//!   silence that ends its waits) is not written and stays marked until
//!   its cursors and its time pass it ([`behind`]).
//! - **Steps**: a run's row folds the run's steps up to the step cursor
//!   onto what its stored row folded of them ([`RunSteps`],
//!   [`super::steps::StepFold::fold_onto`]), so the steps the retention
//!   removed stay in it and a late one that fills a gap is added. A step
//!   that came in after the row was written is an input past its
//!   `inputs_through` (dirty); a step sent again adds nothing. The fold
//!   keeps its sessions' starts, so a late step of a session whose owner
//!   the retention removed is placed too, and the retention keeps a run's
//!   steps while it is marked and no row the store holds of it holds them
//!   ([`Builder::steps_held_since`]).
//! - **Resources**: a run's row folds into its intervals the windows of
//!   its resources up to the resource cursors, those the supervisor
//!   measured from outside, else those its executors measured from inside
//!   ([`RunResources`], [`crate::domain::environment::fold`]). A window
//!   recorded after the row was written, a late one after its `final`
//!   too, is an input past its `inputs_through` (dirty).
//! - **Settled** (`final`): a run or task once it ended (a pending push
//!   has not) and [`SETTLE_GRACE_MS`] passed ([`run_settles`]), a run
//!   only once its telemetry token's revocation is recorded too
//!   ([`telemetry_revoked`]: while it is not, the run waits on the event
//!   that records it, not on its grace) and the supervisor's last measure
//!   of its resources is recorded
//!   ([`crate::domain::environment::resources_recorded`]: while it is
//!   not, the run waits on that window, which marks it); a session,
//!   queue or node row by [`super::settle`]. A settled row an input reaches
//!   later is folded and written again, and settled again after its grace;
//!   for a task, that input is a later event of the task or of its runs,
//!   and the waits that move over its time ready after it settled leave it
//!   as it was ([`keeps_settled`]). A row ended and not settled yet is
//!   marked again when its grace is over ([`Builder::due`]).
//! - **Bounds**: at most [`WRITES_PER_PASS`] rows written and
//!   [`CHECKS_PER_PASS`] marks looked at per pass; the rest stay marked for
//!   the next pass, which starts at them and goes round ([`Builder::take`]),
//!   so that no mark waits behind those marked again each pass (more tasks
//!   ready under a slots full than a pass writes, which each event of its
//!   supervisor marks and makes dirty).
//! - **What a pass reads**: the events after its cursor, [`EVENTS_PER_READ`]
//!   at a time, and the events of each run and task it folds. It folds at
//!   its clock's time, or at the latest event's it read when that is later
//!   ([`Builder::folds_at`]). What the
//!   folds of the queue as a whole read of the rest is [`Kept`] between
//!   passes, so no pass reads the whole EventStore again
//!   ([`Inputs::of`]). It drops the records of the session and queue rows
//!   the store holds settled and the pushes no fold of a row not settled
//!   reads, so what it keeps does not grow with the rows settled; a
//!   settled row a later event reaches gets its records back from the row
//!   the store holds ([`Builder::to_restore`]), a settled run or task its
//!   push by id ([`Builder::pushes_to_read`]). A new process's first pass
//!   drops those of the rows the store holds settled as it ends
//!   ([`Builder::sweep`]), not after the marks before them. How much it keeps
//!   ([`KeptSize`]) goes to the supervisor's log.
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
use std::ops::Bound;
use std::sync::Arc;

use super::fold::PushedTo;
use super::steps::{self, StepFold};
use super::{
    LEDGER_V, LedgerKind, LedgerRow, fold,
    node::{self, BUCKET_SECS},
    owner::{self, Owner},
    queue::{self, QueueBook},
    session::SessionBook,
    settle::{self, SETTLE_GRACE_MS},
};
use crate::domain::environment::{self, CountedSample, Folded, RunResource, Window};
use crate::domain::host_metrics::NodeSample;
use crate::domain::pre_claim::{PreClaimBook, Started};
use crate::domain::run_phase::{Phase, Recorded};
use crate::domain::session_step::{SessionResource, SessionStep};
use crate::domain::stats::timestamp_millis;
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

/// How many steps after the step cursor a pass reads at a time.
pub const STEPS_PER_READ: usize = 5_000;

/// How many windows of resources after a resource cursor a pass reads at a
/// time.
pub const RESOURCES_PER_READ: usize = 5_000;

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

/// What the events read moved of the waits before a claim
/// ([`crate::domain::pre_claim::Moved`]), which the tasks with time ready
/// read ([`Builder::mark_ready`]): a wait's reach, a slots full its
/// supervisor's life had ended opened again, and a wait started or ended.
#[derive(Debug, Clone, Copy, Default)]
struct WaitsMoved {
    reached: bool,
    reopened: bool,
    started_or_ended: bool,
}

/// What the folds of the queue as a whole (the session and queue rows,
/// the waits before a claim, the pushes) read of the EventStore up to the
/// builder's cursor, kept between passes so that a pass reads only what
/// came in after it: what the folds made of every event so far, each read
/// once as it came in ([`Self::add`]): the supervisors' lives, the waits
/// before a claim and the holds on the landings, the landing slot's
/// attempts, and which session or queue row each event is a record of (by
/// id); and of the events themselves only those a later fold may read. A
/// pass folds of them only the rows it looks at ([`Inputs::of`]); the
/// events of a run or a task folded are read whole for its fold
/// ([`Inputs::read`]).
///
/// What it does not keep, by the rules that settle a row today:
///
/// - **A settled session or queue row's events**: once the store holds the
///   row settled (`final`), its records are dropped ([`Builder::track`],
///   at the next [`Builder::due`]) and only their ids stay in the book. A
///   settled row is folded again only when a later event reaches it
///   ([`super::settle`]: a late record of a closed session or an ended
///   job, an event of the supervisor whose life ended its interval);
///   then [`Builder::to_restore`] names it and its records come back from
///   the row the store holds, which owns exactly them
///   ([`Builder::restore`]), so its fold is that of every event. A row not
///   settled keeps its events: its grace or a later event folds it again.
///   A builder that starts at its cursors' start (a new process) reads
///   every event and marks every row behind the marks of every run and
///   task; so that it does not keep every settled row's events for the
///   passes those marks take, its first pass that builds the rows sweeps
///   those still marked past [`CHECKS_PER_PASS`] ([`Builder::sweep`]): one
///   the store holds settled and its fold would not write is settled and
///   its records dropped as that pass ends ([`Builder::swept`]).
/// - **A `push_finished` no fold not settled reads**: a run's or a
///   task's fold reads of the pushes only the first after the run's last
///   `push_pending` to the remote and branch of its last failed push
///   ([`super::fold::closing_push`]). A push that arrives while no run
///   waits on one it matches is that of none, and a run that records its
///   `push_pending` later reads only later pushes; so a push is kept only
///   when it closed a pending push as it came, and only until the rows of
///   the runs it closed and of their tasks are settled ([`Builder::track`],
///   at the next [`Builder::due`]). Of each run whose pending push one
///   closed, the id of that push stays ([`Builder::pushes_to_read`]): a
///   settled run or task folded again reads it back by id
///   ([`Inputs::read_pushes`]), so its fold is that of every push.
///
/// What it keeps of the pushes besides is a summary per run: where the
/// last failed push of each run that failed one went ([`PushedTo`], for a
/// later `push_pending` of it), the runs waiting on a push, and the push
/// that closed each run's last pending push. They are as many as the runs
/// that failed a push, and stay while the process does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Kept {
    /// The last event read.
    last: EventId,
    /// Ascending id: the records of the session and queue rows not
    /// dropped, and the pushes a fold not settled reads.
    sessions: Vec<RunEvent>,
    queue: Vec<RunEvent>,
    pushes: Vec<RunEvent>,
    /// The runs whose last `run_phase_changed` is `push_pending` and that
    /// no push closed yet.
    waiting_push: BTreeMap<String, Waiting>,
    /// Where the last `push_failed` of each run that recorded one went.
    failed_push: BTreeMap<String, PushedTo>,
    /// The push that closed each run's last pending push.
    closing: BTreeMap<String, EventId>,
    /// The runs and tasks not settled whose folds read each kept push.
    push_readers: BTreeMap<EventId, BTreeSet<Target>>,
    lives: Lives,
    pre_claim: PreClaimBook,
    session_rows: SessionBook,
    queue_rows: QueueBook,
    /// The session and queue rows whose records are dropped.
    dropped: BTreeSet<(LedgerKind, String)>,
    /// Roughly how many bytes the events kept hold outside their vectors
    /// ([`event_heap_bytes`]).
    event_heap: usize,
}

/// A run waiting on a push: the id of its `push_pending`, where its last
/// failed push before it went, and its task.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Waiting {
    after: EventId,
    to: PushedTo,
    task: Option<TaskId>,
}

/// How much [`Kept`] holds, for the supervisor's log: the events it keeps
/// of each kind, the elements of each book, and roughly how many bytes
/// all of them hold (the room of the events' vectors, used or not, the
/// events' texts and payloads, and the books' entries; not what an
/// allocator adds).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeptSize {
    pub sessions: usize,
    pub queue: usize,
    pub pushes: usize,
    /// The supervisors with a life.
    pub lives: usize,
    /// The waits before a claim and the holds on the landings.
    pub pre_claim: usize,
    pub session_rows: usize,
    pub queue_rows: usize,
    /// The session and queue rows whose records are dropped.
    pub dropped_rows: usize,
    /// The runs of the summaries of the pushes: those that failed one,
    /// wait on one, or had a pending push closed.
    pub push_runs: usize,
    pub bytes: usize,
}

impl KeptSize {
    /// The events it keeps.
    pub const fn events(&self) -> usize {
        self.sessions + self.queue + self.pushes
    }
}

/// Roughly how many bytes `event` holds outside the vector it is in: its
/// texts and its payload.
fn event_heap_bytes(event: &RunEvent) -> usize {
    fn value(of: &serde_json::Value) -> usize {
        use serde_json::Value;
        size_of::<Value>()
            + match of {
                Value::String(text) => text.len(),
                Value::Array(items) => items.iter().map(value).sum(),
                Value::Object(map) => map
                    .iter()
                    .map(|(key, item)| size_of::<String>() + key.len() + value(item))
                    .sum(),
                Value::Null | Value::Bool(_) | Value::Number(_) => 0,
            }
    }
    event.created_at.len()
        + event.run_id.as_ref().map_or(0, |run| run.as_str().len())
        + value(&event.payload)
}

/// Give back the room of `events` once it is more than twice what they
/// take: a drop after the events read at a process's start leaves no
/// vector the size of them all.
fn shed(events: &mut Vec<RunEvent>) {
    if events.capacity() > 2 * events.len() {
        events.shrink_to_fit();
    }
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
    /// already. What they moved of the waits before a claim.
    fn add(
        &mut self,
        new: &[RunEvent],
        touched: &mut BTreeSet<(LedgerKind, String)>,
    ) -> WaitsMoved {
        let mut waits = WaitsMoved::default();
        for event in new {
            if event.id <= self.last {
                continue;
            }
            self.last = event.id;
            self.lives.add(event);
            let moved = self.pre_claim.add(event);
            waits.reached |= moved.reached;
            waits.reopened |= moved.reopened;
            waits.started_or_ended |=
                matches!(moved.started, Some(Started::PreClaim(_))) || moved.ended[0].is_some();
            let owned = owner::owner(event);
            self.queue_rows
                .add(event, owned == Owner::Queue, moved, touched);
            match owned {
                Owner::Session => {
                    self.session_rows.add(event, touched);
                    self.event_heap += event_heap_bytes(event);
                    self.sessions.push(event.clone());
                }
                Owner::Queue => {
                    self.event_heap += event_heap_bytes(event);
                    self.queue.push(event.clone());
                }
                Owner::Run | Owner::Task => {}
            }
            self.wait_on_push(event);
        }
        waits
    }

    /// Follow the runs waiting on a push, and keep `event` when it is a
    /// push that closes one ([`Kept`]).
    fn wait_on_push(&mut self, event: &RunEvent) {
        if event.kind == EventKind::PushFinished {
            let closed: Vec<(String, Option<TaskId>)> = self
                .waiting_push
                .iter()
                .filter(|(_, waiting)| waiting.to.closed_by(event, waiting.after))
                .map(|(run, waiting)| (run.clone(), waiting.task))
                .collect();
            if !closed.is_empty() {
                let mut readers = BTreeSet::new();
                for (run, task) in closed {
                    self.waiting_push.remove(&run);
                    self.closing.insert(run.clone(), event.id);
                    readers.insert(Target::Run(run));
                    readers.extend(task.map(Target::Task));
                }
                self.push_readers.insert(event.id, readers);
                self.event_heap += event_heap_bytes(event);
                self.pushes.push(event.clone());
            }
        }
        let Some(run) = &event.run_id else {
            return;
        };
        if event.kind == EventKind::PushFailed {
            self.failed_push
                .insert(run.to_string(), PushedTo::of(event));
        } else if let Some(recorded) = Recorded::of_event(event) {
            if recorded.is(Phase::PushPending) {
                let waiting = Waiting {
                    after: event.id,
                    to: self
                        .failed_push
                        .get(run.as_str())
                        .cloned()
                        .unwrap_or_default(),
                    task: event.task_id,
                };
                self.waiting_push.insert(run.to_string(), waiting);
            } else {
                self.waiting_push.remove(run.as_str());
            }
        } else if event.kind == EventKind::RunPhaseChanged {
            // One with no phase is unattributed, as the fold reads it.
            self.waiting_push.remove(run.as_str());
        }
    }

    /// Note the runs and tasks of `settled` settled, and drop the pushes
    /// no fold not settled reads any more.
    fn settle_pushes(&mut self, settled: &BTreeSet<Target>) {
        if settled.is_empty() {
            return;
        }
        let mut unread = BTreeSet::new();
        for (push, readers) in &mut self.push_readers {
            readers.retain(|reader| !settled.contains(reader));
            if readers.is_empty() {
                unread.insert(*push);
            }
        }
        if unread.is_empty() {
            return;
        }
        for push in &unread {
            self.push_readers.remove(push);
        }
        let mut freed = 0;
        self.pushes.retain(|push| {
            let drop = unread.contains(&push.id);
            if drop {
                freed += event_heap_bytes(push);
            }
            !drop
        });
        shed(&mut self.pushes);
        self.event_heap -= freed;
    }

    /// The pushes the folds of the runs of `events` read that it no longer
    /// keeps: each run's that closed its last pending push.
    fn pushes_to_read(&self, events: &[RunEvent]) -> Vec<EventId> {
        let runs: BTreeSet<&str> = events
            .iter()
            .filter_map(|event| event.run_id.as_ref().map(|run| run.as_str()))
            .collect();
        let pushes: BTreeSet<EventId> = runs
            .into_iter()
            .filter_map(|run| self.closing.get(run).copied())
            .filter(|push| {
                self.pushes
                    .binary_search_by_key(push, |event| event.id)
                    .is_err()
            })
            .collect();
        pushes.into_iter().collect()
    }

    /// The ids of the records of the session or queue row of `kind` and
    /// `key`; none for a key of no row.
    fn records_of(&self, kind: LedgerKind, key: &str) -> Option<&[EventId]> {
        match kind {
            LedgerKind::Session => self.session_rows.records_of(key),
            LedgerKind::Queue => self.queue_rows.records_of(key),
            _ => None,
        }
    }

    /// Drop the records of the session and queue rows of `rows`, and note
    /// them dropped.
    fn drop_rows(&mut self, rows: impl IntoIterator<Item = (LedgerKind, String)>) {
        let (mut sessions, mut queue) = (BTreeSet::new(), BTreeSet::new());
        for (kind, key) in rows {
            let Some(records) = self.records_of(kind, &key).filter(|ids| !ids.is_empty()) else {
                continue;
            };
            match kind {
                LedgerKind::Session => sessions.extend(records.iter().copied()),
                _ => queue.extend(records.iter().copied()),
            }
            self.dropped.insert((kind, key));
        }
        let mut freed = 0;
        for (events, ids) in [(&mut self.sessions, sessions), (&mut self.queue, queue)] {
            if ids.is_empty() {
                continue;
            }
            events.retain(|event| {
                let drop = ids.contains(&event.id);
                if drop {
                    freed += event_heap_bytes(event);
                }
                !drop
            });
            shed(events);
        }
        self.event_heap -= freed;
    }

    /// Give the dropped row of `kind` and `key` back those of its records
    /// among `events`; the ids of those still missing. Once none is, the
    /// row is not dropped.
    fn restore(&mut self, kind: LedgerKind, key: &str, events: Vec<RunEvent>) -> Vec<EventId> {
        let row = (kind, key.to_owned());
        if !self.dropped.contains(&row) {
            return Vec::new();
        }
        let ids: BTreeSet<EventId> = self
            .records_of(kind, key)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        let kept = match kind {
            LedgerKind::Session => &mut self.sessions,
            _ => &mut self.queue,
        };
        let mut missing: BTreeSet<EventId> = ids
            .iter()
            .filter(|id| kept.binary_search_by_key(*id, |event| event.id).is_err())
            .copied()
            .collect();
        for event in events {
            if missing.remove(&event.id) {
                self.event_heap += event_heap_bytes(&event);
                kept.push(event);
            }
        }
        kept.sort_by_key(|event| event.id);
        if missing.is_empty() {
            self.dropped.remove(&row);
        }
        missing.into_iter().collect()
    }

    /// How much it holds.
    pub fn size(&self) -> KeptSize {
        let to = |to: &PushedTo| {
            size_of::<PushedTo>()
                + to.remote.as_ref().map_or(0, String::len)
                + to.branch.as_ref().map_or(0, String::len)
        };
        let runs = self
            .waiting_push
            .iter()
            .map(|(run, waiting)| size_of::<(String, Waiting)>() + run.len() + to(&waiting.to))
            .chain(
                self.failed_push
                    .iter()
                    .map(|(run, failed)| size_of::<String>() + run.len() + to(failed)),
            )
            .chain(
                self.closing
                    .keys()
                    .map(|run| size_of::<(String, EventId)>() + run.len()),
            )
            .sum::<usize>()
            + self
                .push_readers
                .values()
                .map(|readers| size_of::<EventId>() + readers.len() * size_of::<Target>())
                .sum::<usize>();
        let dropped: usize = self
            .dropped
            .iter()
            .map(|(_, key)| size_of::<(LedgerKind, String)>() + key.len())
            .sum();
        KeptSize {
            sessions: self.sessions.len(),
            queue: self.queue.len(),
            pushes: self.pushes.len(),
            lives: self.lives.supervisors(),
            pre_claim: self.pre_claim.waits(),
            session_rows: self.session_rows.row_count(),
            queue_rows: self.queue_rows.row_count(),
            dropped_rows: self.dropped.len(),
            push_runs: self.waiting_push.len() + self.failed_push.len() + self.closing.len(),
            bytes: (self.sessions.capacity() + self.queue.capacity() + self.pushes.capacity())
                * size_of::<RunEvent>()
                + self.event_heap
                + runs
                + dropped
                + self.lives.approx_bytes()
                + self.pre_claim.approx_bytes()
                + self.session_rows.approx_bytes()
                + self.queue_rows.approx_bytes(),
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
    /// The latest time (unix ms) of an event read: a pass folds at no
    /// earlier time ([`Self::folds_at`]).
    read_ms: i64,
    /// The last NodeSampleStore id read.
    pub samples: i64,
    /// The last SessionStepStore id read (the order the steps came in).
    pub steps: i64,
    /// The last id read of the runs' resources measured from outside.
    pub resources: i64,
    /// The last id read of the samples executors measured from inside.
    pub inside_resources: i64,
    marks: BTreeSet<Target>,
    /// Where the next [`Self::take`] starts: the first mark the last pass
    /// left past its bounds (not taken, or put back, [`Self::put_back`]).
    /// None: at the first.
    next: Option<Target>,
    /// Session and queue rows with an input after this event are to be
    /// looked at: set by an event of theirs, one that changed one of them
    /// or one naming a supervisor (whose life may end their intervals),
    /// and taken by [`Self::mark_rows`].
    rows_after: Option<EventId>,
    /// Rows ended and not settled, by when their grace is over (unix ms).
    unsettled: BTreeMap<Target, i64>,
    /// The runs left pending on their push, each with its task.
    pending_pushes: BTreeMap<String, Option<TaskId>>,
    /// The tasks whose row the store holds not settled and with time ready,
    /// each with whether every move of a wait may change it
    /// ([`waits_on_ready`]): the waits that move mark them
    /// ([`Self::mark_ready`]).
    ready_tasks: BTreeMap<TaskId, bool>,
    /// The runs with steps or samples from inside read that no row the
    /// store holds of them holds yet, each with when the earliest of those
    /// came in (unix ms): while such a run is marked the retention keeps
    /// the steps and the samples from then on ([`Self::steps_held_since`]).
    unsaved_steps: BTreeMap<String, i64>,
    /// The session and queue rows an event read since the rows were last
    /// built may have changed ([`Kept::add`]).
    touched: BTreeSet<(LedgerKind, String)>,
    /// The session and queue rows the store holds settled, whose records
    /// [`Self::due`] drops from [`Kept`] unless an event reached them since.
    settled: BTreeSet<(LedgerKind, String)>,
    /// The runs and tasks the store holds settled, which [`Self::due`]
    /// notes so in [`Kept`] unless marked again since.
    settled_folds: BTreeSet<Target>,
    /// Shared with the pass's [`Inputs`], which it drops before the next
    /// events come in: an [`Inputs`] kept past them would copy it whole.
    kept: Arc<Kept>,
    /// The version of the rules it last marked the runs by
    /// ([`Self::mark_rules`]); none before any.
    rules: Option<String>,
    /// Whether a pass swept the session and queue rows it built from its
    /// cursors' start ([`Self::sweep`]).
    swept: bool,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            events: EventId::new(0),
            read_ms: i64::MIN,
            samples: 0,
            steps: 0,
            resources: 0,
            inside_resources: 0,
            marks: BTreeSet::new(),
            next: None,
            rows_after: None,
            unsettled: BTreeMap::new(),
            pending_pushes: BTreeMap::new(),
            ready_tasks: BTreeMap::new(),
            unsaved_steps: BTreeMap::new(),
            touched: BTreeSet::new(),
            settled: BTreeSet::new(),
            settled_folds: BTreeSet::new(),
            kept: Arc::default(),
            rules: None,
            swept: false,
        }
    }
}

impl Builder {
    /// Mark what the events `new` (ascending id, after [`Self::events`])
    /// are inputs of, keep what the folds of the queue as a whole read of
    /// them, and move the cursor past them. One that starts or ends a wait
    /// before a claim, timed before an event read before it (a clock set back where
    /// it was written, or a write late by more than that) marks every task
    /// with time ready ([`Self::mark_ready`]): the wait it starts or ends
    /// may fall on a time ready that ended, which reads no other move.
    pub fn mark_events(&mut self, new: &[RunEvent]) {
        let kept = Arc::make_mut(&mut self.kept);
        let mut waits = WaitsMoved::default();
        // A move of a wait timed before an event read before it.
        let mut out_of_order = false;
        for event in new {
            let mut touched = BTreeSet::new();
            let moved = kept.add(std::slice::from_ref(event), &mut touched);
            waits.reached |= moved.reached;
            waits.reopened |= moved.reopened;
            out_of_order |= moved.started_or_ended
                && timestamp_millis(&event.created_at).is_some_and(|at| at < self.read_ms);
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
            if let Some(at) = timestamp_millis(&event.created_at) {
                self.read_ms = self.read_ms.max(at);
            }
        }
        if waits.reopened || out_of_order {
            self.mark_ready(true);
        } else if waits.reached {
            self.mark_ready(false);
        }
    }

    /// The time a pass whose clock reads `now_ms` folds at, and is due at
    /// ([`Self::due`]): no earlier than the latest event read, which came
    /// by then though the pass took its time before it read it (a second's
    /// clock, the time a read takes). A fold at a time before an event it
    /// reads would cut what that event starts (its time ready) to the time
    /// folded at, and a fold at a later time over the same events would
    /// read less of the waits than it ([`behind`]).
    pub fn folds_at(&self, now_ms: i64) -> i64 {
        now_ms.max(self.read_ms)
    }

    /// Mark the tasks with time ready whose overlays or inputs a wait that
    /// moved may change ([`fold::fold_task`]): those any move may (ready
    /// now, or a wait still open bears on their time ready that ended), and
    /// with `all` the others too. Those read only the waits a supervisor's
    /// life ended: one ended as it passed a silence or after a stop
    /// ([`Self::due`]), or opened again by an event naming the supervisor.
    fn mark_ready(&mut self, all: bool) {
        self.marks.extend(
            self.ready_tasks
                .iter()
                .filter(|(_, every_move)| all || **every_move)
                .map(|(task, _)| Target::Task(*task)),
        );
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

    /// Mark the runs the steps `new` (after [`Self::steps`]) are of, and
    /// move the cursor past them; the retention keeps them until the run's
    /// row holds them ([`Self::steps_held_since`]). A step of a target of
    /// no run marks nothing: no run's or task's row holds it.
    pub fn mark_steps(&mut self, new: &[SessionStep]) {
        for step in new {
            if let Some(run) = step.target.run_id() {
                self.marks.insert(Target::Run(run.to_string()));
                let since = self
                    .unsaved_steps
                    .entry(run.to_string())
                    .or_insert(step.received_at);
                *since = (*since).min(step.received_at);
            }
            self.steps = self.steps.max(step.id);
        }
    }

    /// Whether the rules that classify the runs' command steps now are of
    /// another `version` (none without rules) than those it last marked
    /// the runs by ([`Self::mark_rules`]): a pass then reads the run rows
    /// the store holds, a new builder's first pass too.
    pub fn rules_moved(&self, version: Option<&str>) -> bool {
        self.rules.as_deref() != version
    }

    /// Mark each run of `rows`, the run rows the store holds, whose steps
    /// were folded and not classified by the rules of `version`: so a run
    /// that settled long ago is classified again when only the rules
    /// change, with no input of it come in. Without rules it marks none
    /// (no rule read leaves the classes as they were).
    pub fn mark_rules(&mut self, version: Option<&str>, rows: &[LedgerRow]) {
        if let Some(version) = version {
            for row in rows {
                if row.kind == LedgerKind::Run
                    && row.detail.get(steps::DETAIL_KEY).is_some()
                    && steps::rules_of(&row.detail) != Some(version)
                {
                    self.marks.insert(Target::Run(row.key.clone()));
                }
            }
        }
        self.rules = version.map(str::to_owned);
    }

    /// Mark the runs the windows of resources `outside` (after
    /// [`Self::resources`], the supervisor's) and `inside` (after
    /// [`Self::inside_resources`], the executors') are of, and move the
    /// cursors past them; the retention, which removes the samples from
    /// inside with the steps, keeps them until the run's row holds them
    /// ([`Self::steps_held_since`]). One of a target of no run marks
    /// nothing.
    pub fn mark_resources(&mut self, outside: &[RunResource], inside: &[SessionResource]) {
        for window in outside {
            self.marks.insert(Target::Run(window.run_id.clone()));
            self.resources = self.resources.max(window.id);
        }
        for sample in inside {
            if let Some(run) = sample.target.run_id() {
                self.marks.insert(Target::Run(run.to_string()));
                let since = self
                    .unsaved_steps
                    .entry(run.to_string())
                    .or_insert(sample.received_at);
                *since = (*since).min(sample.received_at);
            }
            self.inside_resources = self.inside_resources.max(sample.id);
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

    /// Mark again the rows whose grace is over at `now_ms`, close where
    /// they are kept the slots full whose supervisor's life ended by then,
    /// so that no read of them this pass makes them again: of the kept
    /// ends, only those that came by then ([`PreClaimBook::close_lives`]);
    /// and drop the records of the rows the store holds settled that no
    /// event read since reached ([`Kept`]). When it closed any, mark their
    /// queue rows, whose end it is, and the tasks with time ready
    /// ([`Self::mark_ready`]), whose overlays read that end. Mark too the
    /// open streams whose supervisor's life ended by then
    /// ([`QueueBook::close_lives`]), slots full or not.
    pub fn due(&mut self, now_ms: i64) {
        let kept = Arc::make_mut(&mut self.kept);
        // A silence ends a wait with no event: its row and the tasks ready
        // may read it.
        let closed = kept.pre_claim.close_lives(&kept.lives, now_ms);
        if !closed.is_empty() {
            self.marks.extend(
                closed
                    .iter()
                    .map(|start| Target::Row(LedgerKind::Queue, start.to_string())),
            );
            self.mark_ready(true);
        }
        let kept = Arc::make_mut(&mut self.kept);
        // So does a stream, whether or not its supervisor had a slots full.
        let streams = kept.queue_rows.close_lives(&kept.lives, now_ms);
        self.marks.extend(
            streams
                .iter()
                .map(|start| Target::Row(LedgerKind::Queue, start.to_string())),
        );
        self.drop_settled();
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

    /// Drop from [`Kept`] the records of the session and queue rows the
    /// store holds settled that no event read since reached, and the
    /// pushes of the runs and tasks it holds settled not marked again.
    fn drop_settled(&mut self) {
        let kept = Arc::make_mut(&mut self.kept);
        // One an event reached since stays noted until its fold is tracked.
        let (reached, settled): (BTreeSet<_>, BTreeSet<_>) = std::mem::take(&mut self.settled)
            .into_iter()
            .partition(|(kind, key)| {
                self.touched.contains(&(*kind, key.clone()))
                    || self.marks.contains(&Target::Row(*kind, key.clone()))
            });
        kept.drop_rows(settled);
        self.settled = reached;
        let (marked, settled): (BTreeSet<_>, BTreeSet<_>) = std::mem::take(&mut self.settled_folds)
            .into_iter()
            .partition(|target| self.marks.contains(target));
        kept.settle_pushes(&settled);
        self.settled_folds = marked;
    }

    /// The session and queue rows of `rows` (those [`Self::rows_to_build`]
    /// names, as [`build`] folds them) a pass sweeps once it looked at its
    /// marks: until a pass swept, each of them marked; none after.
    pub fn rows_to_sweep(&self, rows: &BTreeMap<(LedgerKind, String), LedgerRow>) -> Vec<Target> {
        if self.swept {
            return Vec::new();
        }
        rows.keys()
            .map(|(kind, key)| Target::Row(*kind, key.clone()))
            .filter(|target| self.marks.contains(target))
            .collect()
    }

    /// Sweep the session or queue row of `target`, `built` its fold now
    /// and `stored` the row the store holds of it: one the store holds
    /// settled that the fold would not write ([`writes`]) is tracked
    /// settled and no longer marked, as a pass that looked at it would.
    /// Another stays marked for its turn.
    pub fn sweep(
        &mut self,
        target: &Target,
        stored: Option<&LedgerRow>,
        built: Option<&LedgerRow>,
    ) {
        let (Some(stored), Some(built)) = (stored, built) else {
            return;
        };
        if matches!(target, Target::Row(..)) && stored.is_final && !writes(Some(stored), built) {
            self.marks.remove(target);
            self.track(target, stored);
        }
    }

    /// Note the rows swept ([`Self::rows_to_sweep`]) and, the first time,
    /// drop the records of those found settled, once no [`Inputs`] of the
    /// pass holds [`Kept`]; a later pass drops at its [`Self::due`].
    pub fn swept(&mut self) {
        if !self.swept {
            self.swept = true;
            self.drop_settled();
        }
    }

    /// Whether anything is marked.
    pub fn marked(&self) -> bool {
        !self.marks.is_empty() || self.rows_after.is_some()
    }

    /// Take up to `limit` marks in their order (runs first), from where the
    /// last pass left off and round: the marks a pass left past its bounds
    /// are taken before those it looked at, which may be marked again
    /// before the next pass.
    pub fn take(&mut self, limit: usize) -> Vec<Target> {
        let taken: Vec<Target> = match self.next.take() {
            Some(from) => self
                .marks
                .range(&from..)
                .chain(self.marks.range(..&from))
                .take(limit)
                .cloned()
                .collect(),
            None => self.marks.iter().take(limit).cloned().collect(),
        };
        for target in &taken {
            self.marks.remove(target);
        }
        // Those not taken go on after the last taken (none after it: at
        // the first).
        self.next = taken.last().and_then(|last| {
            self.marks
                .range((Bound::Excluded(last), Bound::Unbounded))
                .next()
                .cloned()
        });
        taken
    }

    /// Put back the marks a pass took and did not look at (past its bound
    /// of writes, or after the write before them failed), `first` and then
    /// `rest` in the order taken: the next pass starts at `first`.
    pub fn put_back(&mut self, first: Target, rest: impl IntoIterator<Item = Target>) {
        self.marks.insert(first.clone());
        self.marks.extend(rest);
        self.next = Some(first);
    }

    /// Put `target` back to be looked at next pass in its turn (another
    /// writer kept writing its row, or its cursors are behind the stored
    /// row's).
    pub fn keep(&mut self, target: Target) {
        self.marks.insert(target);
    }

    /// Note what the store holds of `target` now, `row`, folded on every
    /// input read (a run's steps too): wait on its grace while it ended and
    /// is not settled, and on a later push while it is pending on its push.
    pub fn track(&mut self, target: &Target, row: &LedgerRow) {
        match target {
            Target::Row(kind, key) if row.is_final => {
                self.settled.insert((*kind, key.clone()));
            }
            Target::Row(kind, key) => {
                self.settled.remove(&(*kind, key.clone()));
            }
            Target::Run(_) | Target::Task(_) if row.is_final => {
                self.settled_folds.insert(target.clone());
            }
            Target::Run(_) | Target::Task(_) => {
                self.settled_folds.remove(target);
            }
            Target::Node(..) => {}
        }
        match settles_at(row) {
            Some(at) if !row.is_final => {
                self.unsettled.insert(target.clone(), at);
            }
            _ => {
                self.unsettled.remove(target);
            }
        }
        if let Target::Task(task) = target {
            match waits_on_ready(row) {
                Some(now) => self.ready_tasks.insert(*task, now),
                None => self.ready_tasks.remove(task),
            };
        }
        if let Target::Run(run) = target {
            self.unsaved_steps.remove(run);
            if pending_push(row) {
                self.pending_pushes.insert(run.clone(), row.task_id);
            } else {
                self.pending_pushes.remove(run);
            }
        }
    }

    /// Since when (unix ms) the retention keeps the steps and the samples
    /// from inside: the earliest step or sample read of a run still marked
    /// (past a pass's bound, or its write failed) whose row the store does
    /// not hold them in yet. None when no marked run waits on them. A run folded to no row (on its way)
    /// holds nothing: its next input marks it, and steps it received more
    /// than the retention before it ends are not in its row.
    pub fn steps_held_since(&self) -> Option<i64> {
        self.unsaved_steps
            .iter()
            .filter(|(run, _)| self.marks.contains(&Target::Run((*run).clone())))
            .map(|(_, since)| *since)
            .min()
    }

    /// How many marks wait.
    pub fn backlog(&self) -> usize {
        self.marks.len()
    }

    /// What the folds of the queue as a whole read, up to [`Self::events`].
    pub const fn kept(&self) -> &Arc<Kept> {
        &self.kept
    }

    /// The rows of `rows` (those [`Self::rows_to_build`] names) whose
    /// records [`Kept`] dropped: before they are folded, each is given its
    /// records back ([`Self::restore`]).
    pub fn to_restore(&self, rows: &BTreeSet<(LedgerKind, String)>) -> Vec<(LedgerKind, String)> {
        rows.iter()
            .filter(|row| self.kept.dropped.contains(*row))
            .cloned()
            .collect()
    }

    /// The pushes the fold of a run or task whose events are `events` reads
    /// that [`Kept`] no longer keeps (its runs' rows were settled): each is
    /// read by id and given to the fold ([`Inputs::read_pushes`]).
    pub fn pushes_to_read(&self, events: &[RunEvent]) -> Vec<EventId> {
        self.kept.pushes_to_read(events)
    }

    /// Give the dropped row of `kind` and `key` back its records among
    /// `events` (the records of the row the store holds of it, or events
    /// read by id); the ids of those still missing.
    pub fn restore(&mut self, kind: LedgerKind, key: &str, events: Vec<RunEvent>) -> Vec<EventId> {
        Arc::make_mut(&mut self.kept).restore(kind, key, events)
    }
}

/// When `row`, ended, settles once no later input reaches it: its end and
/// its grace. None for a row not ended (or a legacy one, settled as it is
/// written), and for a run whose telemetry token's revocation is not
/// recorded: the event that records it marks the run.
fn settles_at(row: &LedgerRow) -> Option<i64> {
    if row.is_legacy()
        || (row.kind == LedgerKind::Run
            && (!telemetry_revoked(&row.records) || resources_pending(row)))
    {
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

/// Whether the task row `row`, not settled, has time ready, and whether
/// every move of a wait may change it: it is ready up to the time it was
/// folded at (its status `ready` then, [`fold::READY_SINCE`], though it
/// became ready at that very time and has no time ready yet), or a wait
/// still open bears on its time ready that ended ([`fold::WAITS_OPEN`]),
/// whose last event is its input. The waits before a claim on that time
/// may move (end, or their supervisor's life end, before the end of a time
/// ready past too) and others start on the time ready up to now, which its
/// overlays read ([`fold::fold_task`]) though no event of the task marks
/// it. None for a settled row or one never ready.
fn waits_on_ready(row: &LedgerRow) -> Option<bool> {
    if row.is_final {
        return None;
    }
    let every_move = row
        .detail
        .get(fold::READY_SINCE)
        .is_some_and(serde_json::Value::is_i64)
        || row
            .detail
            .get(fold::WAITS_OPEN)
            .and_then(serde_json::Value::as_bool)
            == Some(true);
    (every_move
        || row
            .intervals
            .iter()
            .any(|interval| interval.phase == fold::READY))
    .then_some(every_move)
}

/// Whether the telemetry token of the run whose events are `events`
/// (ascending id) is revoked, as the run's `final` waits on (ADR-t1662-1
/// decision 17): none was issued, or the last issue or revocation recorded
/// is a revocation. A revocation is recorded only once it succeeded, so
/// one that failed leaves the run's last record an issue until a later
/// sweep revokes it. A resume, an adoption or a handoff revokes the token
/// before it issues another, recorded in that order.
pub fn telemetry_revoked(events: &[RunEvent]) -> bool {
    events
        .iter()
        .rev()
        .find(|event| {
            event.kind == EventKind::TelemetryTokenIssued
                || event.kind == EventKind::TelemetryTokenRevoked
        })
        .is_none_or(|last| last.kind == EventKind::TelemetryTokenRevoked)
}

/// The key of a run row's detail that says the supervisor's last measure
/// of its resources is not recorded yet
/// ([`crate::domain::environment::resources_recorded`]).
pub const RESOURCES_PENDING: &str = "resources_pending";

/// Whether the run row `row` waits on the supervisor's last measure of its
/// resources.
pub fn resources_pending(row: &LedgerRow) -> bool {
    row.detail.get(RESOURCES_PENDING) == Some(&serde_json::Value::Bool(true))
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
/// later sample, a later step, a later window of its resources, a
/// supervisor's later event, its command steps classified by other
/// rules), a task's whose
/// waits a supervisor's life ended are others (the time passed a silence;
/// one folded at an earlier time is [`behind`]), a queue row's that its
/// supervisor's life ended over the events the stored one read open (the
/// time passed a silence; one that read a later event of the supervisor
/// that opens it again reads past it, [`queue`]), and one settled now. A
/// stored row that holds the same inputs is not written again, so the same
/// inputs read twice leave it as it is.
pub fn writes(stored: Option<&LedgerRow>, built: &LedgerRow) -> bool {
    let Some(stored) = stored else {
        return true;
    };
    (!stored.is_legacy() && stored.ledger_v < LEDGER_V)
        || built.inputs_through.last() > stored.inputs_through.last()
        || built.inputs_through.life_ended != stored.inputs_through.life_ended
        || built.inputs_through.supervisor > stored.inputs_through.supervisor
        || built.inputs_through.node_samples > stored.inputs_through.node_samples
        || built.inputs_through.steps > stored.inputs_through.steps
        || built.inputs_through.resources > stored.inputs_through.resources
        || built.inputs_through.inside_resources > stored.inputs_through.inside_resources
        || (built.is_final && !stored.is_final && !stored.is_legacy())
        || steps::rules_of(&built.detail) != steps::rules_of(&stored.detail)
}

/// Whether `stored`, a task's row the store holds settled (of this
/// [`LEDGER_V`] or a newer one, not a legacy one), stays as it was settled
/// over a fold whose events of the task (its own and its runs', up to the
/// builder's cursor) are `events`: none is past the events the stored fold
/// read. Each event of the task up to there was read by it, so no event of
/// the task came since it settled, and the rest of what a task's fold
/// reads moves after its end only by the waits before a claim over its
/// time ready (a supervisor that records again after its silence opens a
/// slots full its silence ended), which a settled row does not follow
/// (ADR-t1662-1 decision 17). A builder that reads the task's end after
/// such a move, or one that starts again, leaves it as it was.
pub fn keeps_settled(stored: &LedgerRow, events: &[RunEvent]) -> bool {
    stored.kind == LedgerKind::Task
        && stored.is_final
        && !stored.is_legacy()
        && stored.ledger_v >= LEDGER_V
        && events
            .iter()
            .all(|event| event.id <= stored.inputs_through.events)
}

/// Whether the fold `built` read less of some input than `stored`, the row
/// the store holds: another writer (another supervisor's builder, its
/// cursors ahead, or its pass at a later time) wrote it. Written over it,
/// it would take back what that one read (a later event, its intervals and
/// its `final`, a wait a silence ended) though it read more of another
/// input (a later step), so it is not written and folds again once its
/// cursors and its time passed the stored row's.
///
/// The time is an input of a task's row with no mark of its own: over the
/// same events, folded at no earlier time than any of them
/// ([`Builder::folds_at`]), the waits a supervisor's life ended it reads
/// (`life_ended`) only grow with the time folded at, as a silence passes.
/// So a fold of a task that read the events the stored row read and lacks
/// one of those its waits was folded at an earlier time (a pass that began
/// before the silence and writes after the one past it, whose builder
/// closed that wait and marks the task no more, or a clock set back). One
/// that read a later event is written: the builder ahead in time reads that
/// event too, which marks the task, and folds it again.
pub fn behind(stored: &LedgerRow, built: &LedgerRow) -> bool {
    let (held, read) = (&stored.inputs_through, &built.inputs_through);
    read.events < held.events
        || read.last() < held.last()
        || read.node_samples < held.node_samples
        || read.steps < held.steps
        || read.resources < held.resources
        || read.inside_resources < held.inside_resources
        || (read.events == held.events
            && held
                .life_ended
                .iter()
                .any(|wait| !read.life_ended.contains(wait)))
}

/// What the row of a run folds of its session steps: its steps up to the
/// builder's step cursor, the starts of their sessions on the supervisor's
/// clock (when the control side registered each to the run, unix ms; a
/// session with none keeps its steps unplaced until a fold knows it), and
/// what the row the store holds folded of them, which the new fold goes on
/// from ([`StepFold::fold_onto`]): the steps the retention removed stay
/// counted and placed, and the largest step id it counted stays its
/// `inputs_through`. A step it counted before, sent again under a new id
/// (once the retention removed its whole session), moves nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunSteps {
    pub steps: Vec<SessionStep>,
    pub starts: BTreeMap<String, i64>,
    stored: StepFold,
    stored_through: Option<i64>,
    /// The rules that classify its command steps now
    /// ([`StepFold::classify`]); none classifies none.
    classifier: Option<crate::domain::command_rules::CommandClassifier>,
}

impl RunSteps {
    /// The run's `steps` over what `stored`, the row the store holds of the
    /// run, folded of them; no start known yet.
    pub fn new(steps: Vec<SessionStep>, stored: Option<&LedgerRow>) -> Self {
        Self {
            steps,
            starts: BTreeMap::new(),
            stored: stored
                .and_then(|row| StepFold::of_detail(&row.detail))
                .unwrap_or_default(),
            stored_through: stored.and_then(|row| row.inputs_through.steps),
            classifier: None,
        }
    }

    /// The same, its command steps classified by `classifier`.
    #[must_use]
    pub fn classified_by(
        mut self,
        classifier: Option<crate::domain::command_rules::CommandClassifier>,
    ) -> Self {
        self.classifier = classifier;
        self
    }

    /// The sessions whose start the fold places steps by: those of the
    /// steps, and those of the steps the stored fold holds unplaced.
    pub fn sessions(&self) -> BTreeSet<String> {
        self.steps
            .iter()
            .map(|step| step.session_id.clone())
            .chain(
                self.stored
                    .unplaced
                    .iter()
                    .map(|step| step.session_id.clone()),
            )
            .collect()
    }

    /// Fold them into the run's `row`: under [`steps::DETAIL_KEY`] of its
    /// detail once any step is counted, and the largest id of a step
    /// counted into its `inputs_through`.
    fn fold_into(&self, row: &mut LedgerRow) {
        let mut fold = self.stored.fold_onto(&self.starts, &self.steps);
        fold.classify(self.classifier.as_ref(), &self.steps);
        row.inputs_through.steps = self
            .steps
            .iter()
            .filter(|step| !self.stored.holds(&step.session_id, step.step.seq))
            .map(|step| step.id)
            .max()
            .max(self.stored_through);
        if fold.count() == 0 {
            return;
        }
        if let serde_json::Value::Object(detail) = &mut row.detail {
            detail.insert(steps::DETAIL_KEY.to_owned(), fold.detail(&row.intervals));
        }
    }
}

/// What the row of a run folds of its resources: the windows the
/// supervisor measured from outside up to the builder's cursor, the
/// samples its executors measured from inside up to the other cursor with
/// the starts of their sessions on the supervisor's clock (as the steps'),
/// and what the row the store holds folded: the samples from inside it
/// counted are counted again with those it did not, so what the retention
/// removed from the store stays in the fold, the starts it kept place a
/// later sample of a session whose owner the retention removed, and the
/// largest id of a sample from inside it read stays its `inputs_through`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunResources {
    pub outside: Vec<RunResource>,
    pub inside: Vec<SessionResource>,
    pub starts: BTreeMap<String, i64>,
    stored: Option<Folded>,
    stored_inside_through: Option<i64>,
}

impl RunResources {
    /// The run's windows `outside` and samples `inside` over what `stored`,
    /// the row the store holds of the run, folded of them; the starts known
    /// are those it kept.
    pub fn new(
        outside: Vec<RunResource>,
        inside: Vec<SessionResource>,
        stored: Option<&LedgerRow>,
    ) -> Self {
        let stored_fold = stored.and_then(|row| Folded::of_detail(&row.detail));
        Self {
            outside,
            inside,
            starts: stored_fold
                .as_ref()
                .map(|fold| fold.starts.clone())
                .unwrap_or_default(),
            stored: stored_fold,
            stored_inside_through: stored.and_then(|row| row.inputs_through.inside_resources),
        }
    }

    /// The sessions whose start places the samples from inside.
    pub fn sessions(&self) -> BTreeSet<String> {
        self.inside
            .iter()
            .map(|sample| sample.session_id.clone())
            .collect()
    }

    /// Fold them into the run's `row`: under
    /// [`environment::DETAIL_KEY`] of its detail once a window holds a
    /// value, [`RESOURCES_PENDING`] while the supervisor's last measure
    /// is not recorded, and the largest ids read into its
    /// `inputs_through`. A sample from inside whose session has no start
    /// known is left out until a fold knows it.
    fn fold_into(&self, row: &mut LedgerRow) {
        let outside: Vec<Window> = self.outside.iter().map(Window::of_run).collect();
        let placed: Vec<CountedSample> = self
            .inside
            .iter()
            .filter_map(|sample| {
                let start = self.starts.get(&sample.session_id)?;
                Some(CountedSample::of_sample(sample, *start))
            })
            .collect();
        let stored = self
            .stored
            .as_ref()
            .map_or(&[][..], |fold| &fold.inside[..]);
        let counted = environment::counted_inside(stored, &placed);
        let inside: Vec<Window> = counted.iter().map(CountedSample::window).collect();
        row.inputs_through.resources = self.outside.iter().map(|window| window.id).max();
        row.inputs_through.inside_resources = self
            .inside
            .iter()
            .map(|sample| sample.id)
            .max()
            .max(self.stored_inside_through);
        let pending = !environment::resources_recorded(&row.records, &self.outside);
        let starts = self
            .starts
            .iter()
            .filter(|(session, _)| counted.iter().any(|sample| &sample.session_id == *session))
            .map(|(session, start)| (session.clone(), *start))
            .collect();
        let folded = environment::fold(&row.intervals, &outside, &inside)
            .map(|folded| Folded {
                inside: counted,
                starts,
                ..folded
            })
            .or_else(|| self.stored.clone());
        if let serde_json::Value::Object(detail) = &mut row.detail {
            if let Some(folded) = folded {
                detail.insert(environment::DETAIL_KEY.to_owned(), folded.detail());
            }
            if pending {
                detail.insert(RESOURCES_PENDING.to_owned(), serde_json::Value::Bool(true));
            }
        }
    }
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
    /// The session steps of each run read ([`Self::read_steps`]).
    steps: BTreeMap<String, RunSteps>,
    /// The pushes read for a run's or task's fold ([`Self::read_pushes`]).
    pushes: BTreeMap<Target, Vec<RunEvent>>,
    /// The resources of each run read ([`Self::read_resources`]).
    resources: BTreeMap<String, RunResources>,
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
            steps: BTreeMap::new(),
            pushes: BTreeMap::new(),
            resources: BTreeMap::new(),
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

    /// Give the run `run` its session steps, for [`build`] to fold into its
    /// row.
    pub fn read_steps(&mut self, run: &str, steps: RunSteps) {
        self.steps.insert(run.to_owned(), steps);
    }

    /// Give the fold of the run or task `target` the pushes `pushes` it
    /// reads that [`Kept`] no longer keeps ([`Builder::pushes_to_read`]).
    pub fn read_pushes(&mut self, target: &Target, pushes: Vec<RunEvent>) {
        if !pushes.is_empty() {
            self.pushes.insert(target.clone(), pushes);
        }
    }

    /// The pushes the fold of `target` reads: those kept, and those read
    /// for it, ascending id.
    fn pushes_of(&self, target: &Target) -> std::borrow::Cow<'_, [RunEvent]> {
        match self.pushes.get(target) {
            None => std::borrow::Cow::Borrowed(&self.kept.pushes),
            Some(read) => {
                let mut pushes: Vec<RunEvent> =
                    self.kept.pushes.iter().chain(read).cloned().collect();
                pushes.sort_by_key(|push| push.id);
                pushes.dedup_by_key(|push| push.id);
                std::borrow::Cow::Owned(pushes)
            }
        }
    }

    /// Give the run `run` its resources, for [`build`] to fold into its
    /// row.
    pub fn read_resources(&mut self, run: &str, resources: RunResources) {
        self.resources.insert(run.to_owned(), resources);
    }

    /// Drop the events [`Self::read`], the steps [`Self::read_steps`],
    /// the pushes [`Self::read_pushes`] and the resources
    /// [`Self::read_resources`] gave `target`, once folded.
    pub fn forget(&mut self, target: &Target) {
        self.pushes.remove(target);
        match target {
            Target::Run(run) => {
                self.runs.remove(run);
                self.steps.remove(run);
                self.resources.remove(run);
            }
            Target::Task(task) => {
                self.tasks.remove(task);
            }
            Target::Row(..) | Target::Node(..) => {}
        }
    }
}

/// The row of `target` as it is written now, settled or not: none for a
/// run on its way, a run or task with no events (or a task with no record
/// of its own and no run), a session or queue row not
/// among `inputs.rows`, or a node hour with no sample left. A run's row
/// holds its steps and resources given ([`Inputs::read_steps`],
/// [`Inputs::read_resources`]), a legacy one none.
/// `samples` are the NodeSampleStore's of a node target's hour.
pub fn build(target: &Target, inputs: &Inputs, samples: &[NodeSample]) -> Option<LedgerRow> {
    let now = inputs.now_ms;
    let kept = &*inputs.kept;
    match target {
        Target::Run(run) => {
            let events = inputs.runs.get(run)?;
            let pushes = inputs.pushes_of(target);
            let mut row = super::run_row(run, events, &pushes, now, &inputs.built_at);
            if !(row.is_legacy() || row.ended_at.is_some() || pending_push(&row)) {
                return None;
            }
            if !row.is_legacy()
                && let Some(steps) = inputs.steps.get(run)
            {
                steps.fold_into(&mut row);
            }
            if !row.is_legacy() {
                inputs
                    .resources
                    .get(run)
                    .cloned()
                    .unwrap_or_default()
                    .fold_into(&mut row);
            }
            row.is_final = run_settles(&row, now)
                && telemetry_revoked(&row.records)
                && !resources_pending(&row);
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
                &inputs.pushes_of(target),
                kept.pre_claim.intervals(&kept.lives, now),
                now,
                &inputs.built_at,
            );
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
