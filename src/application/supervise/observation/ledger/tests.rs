use std::cell::{Cell, RefCell};

use super::*;
use crate::application::{
    LedgerStore, NodeSampleStore, RunResourceStore, SessionOwners, SessionResourceStore,
    SessionStepStore,
};
use crate::domain::environment::{Resources, RunResource};
use crate::domain::host_metrics::{HOST_NODE, HostSample, NodeSample, SampleSource};
use crate::domain::ledger::builder::HOLDING_R_PER_PASS;
use crate::domain::ledger::queue::QueueBook;
use crate::domain::ledger::read::read;
use crate::domain::ledger::session::SessionBook;
use crate::domain::ledger::session::written::{
    Cursors, Marks, RowsWritten, SessionsWritten, written_through,
};
use crate::domain::ledger::settle::SETTLE_GRACE_MS;
use crate::domain::ledger::testkit::{
    Log, T0, ms, pushes_across, queue_across, run_id, sessions_across,
};
use crate::domain::ledger::{LEDGER_V, LedgerKind, LedgerRow, node, provider_use};
use crate::domain::pre_claim::PreClaimBook;
use crate::domain::session_step::{
    RETENTION_MS, ResourceSample, SeqRange, SessionOwner, SessionResource, SessionStep, Step,
    StepKind, StepTarget, StepsRecorded, TargetKind,
};
use crate::domain::supervisor_life::Lives;
use crate::domain::{RunEvent, RunId, TaskId};

const BUILT: &str = "2030-01-01T00:00:00.000Z";

/// The stores in memory: the events, the node samples, the steps and the rows, the
/// rows written as `ledger_rows` writes them (a legacy row keeps its
/// values, its records and their range follow), how many writes, and how
/// many events were read and how often all of them.
#[derive(Default)]
struct Memory {
    callers: RefCell<Vec<(&'static str, Option<&'static str>)>>,
    events: RefCell<Vec<RunEvent>>,
    samples: RefCell<Vec<NodeSample>>,
    steps: RefCell<Vec<SessionStep>>,
    /// The last step id given: ids go on past the steps removed.
    step_id: Cell<i64>,
    owners: RefCell<BTreeMap<String, SessionOwner>>,
    /// The runs' windows of resources measured from outside, and the
    /// samples executors measured from inside, ids from 1 in order.
    run_resources: RefCell<Vec<RunResource>>,
    inside: RefCell<Vec<SessionResource>>,
    /// The last sample id given: ids go on past the samples removed.
    inside_id: Cell<i64>,
    rows: RefCell<BTreeMap<(LedgerKind, String), LedgerRow>>,
    writes: Cell<usize>,
    read: Cell<usize>,
    read_all: Cell<usize>,
    row_reads: Cell<usize>,
    /// The runs whose events were read whole, and the events read one by
    /// one by id, in order.
    runs_read: RefCell<Vec<RunId>>,
    read_one: RefCell<Vec<RunEvent>>,
    /// The keys of the queue rows whose records were read, and the runs
    /// whose windows of resources were read, in order.
    queue_read: RefCell<Vec<String>>,
    resources_read: RefCell<Vec<RunId>>,
    /// The other reads of the events, the rows, the steps, the samples and
    /// the resources, by name.
    other_reads: RefCell<Vec<&'static str>>,
    /// The queue rows of waits given to make the books again and to fold
    /// a task over the waits settled ([`LedgerStore::ledger_wait_rows`],
    /// [`LedgerStore::ledger_settled_waits`]).
    wait_rows: Cell<usize>,
    /// Gives no wait settled to a task's fold: a builder that reads only
    /// the book made again from the rows.
    no_settled_waits: Cell<bool>,
    /// Answers [`LedgerStore::ledger_rows_of`] with one
    /// [`LedgerStore::ledger_row`] a key, as a store of no such read does.
    rows_one_by_one: Cell<bool>,
    /// Refuses every write of a row.
    refuses: Cell<bool>,
    /// The row whose read fails, as a stored row that does not parse.
    broken: RefCell<Option<(LedgerKind, String)>>,
    /// Runs once, as the next run's steps are read: another builder's pass
    /// between a fold's read of the row and its write.
    between: RefCell<Option<Between>>,
    /// Runs once, as the steps after a cursor are next read: another
    /// builder's pass between a pass's read of the events and of the steps.
    before_steps: RefCell<Option<Between>>,
    /// The mark of where the session rows record every session event up
    /// to ([`LedgerStore::sessions_written`]).
    mark: RefCell<Option<SessionsWritten>>,
    /// How many of the next writes of the session rows fail part way.
    batch_fails: Cell<usize>,
    /// How many writes of the session rows were committed.
    batches: Cell<usize>,
    /// How many writes of the session rows were tried, and which of them
    /// (counted from 0) fails part way.
    batch_calls: Cell<usize>,
    batch_fails_at: Cell<Option<usize>>,
    /// The bytes of the rows' text of each write of the session rows
    /// committed, and whether it held the mark.
    batch_bytes: RefCell<Vec<(usize, bool)>>,
    /// The mark R written with the session rows' mark
    /// ([`LedgerStore::ledger_marks`]).
    rows_mark: RefCell<Option<RowsWritten>>,
    /// How many writes of the marks alone were committed
    /// ([`LedgerStore::write_ledger_marks`]).
    marks_alone: Cell<usize>,
    /// The reads besides the rows' heads a cut of the queue rows to a
    /// position makes ([`books_at`]).
    cut_reads: Cell<CutReads>,
    /// How many heads [`LedgerStore::ledger_queue_heads_at`] gave at most
    /// at once.
    heads_given: Cell<usize>,
    /// Each read of an input after a cursor (not by id): which cursor, the
    /// position it read after, and how many it gave.
    scans: RefCell<Vec<(&'static str, i64, usize)>>,
    /// The keys of the rows written, in order.
    written_keys: RefCell<Vec<(LedgerKind, String)>>,
}

/// How many times each read a cut of the queue rows makes was called, and
/// how many events the reads of a row's records and a run's events gave.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CutReads {
    rows: usize,
    row_events: usize,
    runs: usize,
    run_events: usize,
    naming: usize,
    since_at: usize,
}

impl Memory {
    fn count_cut(&self, count: impl FnOnce(&mut CutReads)) {
        let mut reads = self.cut_reads.get();
        count(&mut reads);
        self.cut_reads.set(reads);
    }
}

/// Another builder's pass, run on the store between a read and a write.
type Between = Box<dyn FnOnce(&Memory)>;

impl Memory {
    fn of(log: &Log) -> Self {
        let memory = Self::default();
        memory.events.replace(log.events.clone());
        memory
    }

    /// Forget the reads [`assert_reads_allowed`] looks at.
    fn forget_reads(&self) {
        self.runs_read.borrow_mut().clear();
        self.read_one.borrow_mut().clear();
        self.queue_read.borrow_mut().clear();
        self.resources_read.borrow_mut().clear();
        self.other_reads.borrow_mut().clear();
    }

    /// Note the read `name`, none of those [`assert_reads_allowed`] allows.
    fn other_read(&self, name: &'static str) {
        self.callers.borrow_mut().push((
            name,
            crate::application::supervisor_service::caller::current(),
        ));
        self.other_reads.borrow_mut().push(name);
    }

    fn row(&self, kind: LedgerKind, key: &str) -> LedgerRow {
        self.rows.borrow()[&(kind, key.to_owned())].clone()
    }

    /// The events `keep` keeps, counted as read.
    fn read(&self, keep: impl Fn(&RunEvent) -> bool) -> Vec<RunEvent> {
        let read: Vec<RunEvent> = self
            .events
            .borrow()
            .iter()
            .filter(|event| keep(event))
            .cloned()
            .collect();
        self.read.set(self.read.get() + read.len());
        read
    }

    fn count(&self, kind: LedgerKind) -> usize {
        self.rows
            .borrow()
            .keys()
            .filter(|(k, _)| *k == kind)
            .count()
    }
}

#[allow(unused_variables)]
impl EventStore for Memory {
    fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        unreachable!()
    }
    fn all_events(&self) -> Result<Vec<RunEvent>> {
        self.other_read("all_events");
        self.read_all.set(self.read_all.get() + 1);
        Ok(self.read(|_| true))
    }
    fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
        unreachable!()
    }
    fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
        self.runs_read.borrow_mut().push(id.clone());
        Ok(self.read(|event| event.run_id.as_ref() == Some(id)))
    }
    fn task_events(&self, task: TaskId) -> Result<Vec<RunEvent>> {
        self.other_read("task_events");
        Ok(self.read(|event| event.task_id == Some(task)))
    }
    fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool> {
        unreachable!()
    }
    fn record_runtime_event(&self, id: &RunId, kind: EventKind, payload: Value) -> Result<()> {
        unreachable!()
    }
    fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
        unreachable!()
    }
    fn latest_event_id(&self) -> Result<EventId> {
        Ok(self
            .events
            .borrow()
            .last()
            .map_or(EventId::new(0), |event| event.id))
    }
    fn record_backend_failure(&self, run: Option<&RunId>, payload: Value) -> Result<()> {
        unreachable!()
    }
    fn record_queue_event(&self, kind: EventKind, payload: Value) -> Result<EventId> {
        unreachable!()
    }
    fn latest_event_of(&self, kind: &str) -> Result<Option<RunEvent>> {
        unreachable!()
    }
    fn latest_events_of(&self, kind: &str, limit: usize) -> Result<Vec<RunEvent>> {
        unreachable!()
    }
    fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>> {
        unreachable!()
    }
    fn latest_events_by_supervisor(
        &self,
        kinds: &[&str],
        supervisors: &[&str],
    ) -> Result<Vec<RunEvent>> {
        unreachable!()
    }
    fn events_of_between(
        &self,
        kinds: &[&str],
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        unreachable!()
    }
    fn events_between(
        &self,
        after: EventId,
        upto: EventId,
        filter: &EventFilter,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        self.callers.borrow_mut().push((
            "events_between",
            crate::application::supervisor_service::caller::current(),
        ));
        assert_eq!(filter, &EventFilter::default());
        let read: Vec<RunEvent> = self
            .events
            .borrow()
            .iter()
            .filter(|event| event.id > after && event.id <= upto)
            .take(limit)
            .cloned()
            .collect();
        self.read.set(self.read.get() + read.len());
        if limit != 1 {
            self.scans
                .borrow_mut()
                .push(("events", after.as_i64(), read.len()));
        }
        if after.as_i64() + 1 != upto.as_i64() {
            self.other_read("events_between");
        }
        if after.as_i64() + 1 == upto.as_i64() {
            self.read_one.borrow_mut().extend(read.iter().cloned());
        }
        Ok(read)
    }
}

impl Memory {
    /// The row of `kind` and `key`, counted as one row read.
    fn held_row(&self, kind: LedgerKind, key: &str) -> Result<Option<LedgerRow>> {
        self.row_reads.set(self.row_reads.get() + 1);
        let row = (kind, key.to_owned());
        ensure!(
            self.broken.borrow().as_ref() != Some(&row),
            "the stored row does not parse"
        );
        let held = self.rows.borrow().get(&row).cloned();
        if kind == LedgerKind::Queue && held.is_some() {
            self.queue_read.borrow_mut().push(key.to_owned());
        }
        self.count_cut(|reads| {
            reads.rows += 1;
            reads.row_events += held.as_ref().map_or(0, |row| row.records.len());
        });
        Ok(held)
    }
}

impl LedgerStore for Memory {
    fn ledger_run_events(&self, run: &RunId) -> Result<Vec<RunEvent>> {
        let events = EventStore::run_events(self, run)?;
        self.count_cut(|reads| {
            reads.runs += 1;
            reads.run_events += events.len();
        });
        Ok(events)
    }
    fn ledger_legacy_runs_without_row(&self) -> Result<Vec<RunId>> {
        let rows = self.rows.borrow();
        let legacy: BTreeSet<TaskId> = rows
            .values()
            .filter(|row| row.kind == LedgerKind::Task && row.is_legacy())
            .filter_map(|row| row.task_id)
            .collect();
        let runs: BTreeSet<RunId> = self
            .read(|event| {
                event.kind == EventKind::RunClaimed
                    && event.task_id.is_some_and(|task| legacy.contains(&task))
            })
            .into_iter()
            .filter_map(|event| event.run_id)
            .filter(|run| !rows.contains_key(&(LedgerKind::Run, run.to_string())))
            .collect();
        Ok(runs.into_iter().collect())
    }
    fn ledger_runs_claimed(&self, task: TaskId) -> Result<Vec<RunId>> {
        let runs: BTreeSet<RunId> = self
            .read(|event| event.kind == EventKind::RunClaimed && event.task_id == Some(task))
            .into_iter()
            .filter_map(|event| event.run_id)
            .collect();
        Ok(runs.into_iter().collect())
    }
    fn ledger_runs_past_their_rows(&self, through: EventId) -> Result<Vec<RunId>> {
        let rows = self.rows.borrow();
        let runs: BTreeSet<RunId> = self
            .events
            .borrow()
            .iter()
            .filter(|event| event.id <= through)
            .filter_map(|event| {
                let run = event.run_id.as_ref()?;
                let row = rows.get(&(LedgerKind::Run, run.to_string()))?;
                (event.id > row.inputs_through.events).then(|| run.clone())
            })
            .collect();
        Ok(runs.into_iter().collect())
    }
    fn ledger_tasks_open_since_at(&self, at: EventId) -> Result<pre_claim::rows::Since> {
        self.other_read("ledger_tasks_open_since_at");
        self.count_cut(|reads| reads.since_at += 1);
        let rows: Vec<LedgerRow> = self
            .ledger_rows(LedgerKind::Task)?
            .into_iter()
            .filter(|row| row.inputs_from <= at)
            .map(|row| LedgerRow {
                is_final: row.is_final && row.inputs_through.last() <= at,
                ..row
            })
            .collect();
        Ok(pre_claim::rows::Since::of(&rows))
    }
    fn last_event_naming_supervisor(
        &self,
        supervisor: &str,
        upto: EventId,
    ) -> Result<Option<RunEvent>> {
        self.other_read("last_event_naming_supervisor");
        self.count_cut(|reads| reads.naming += 1);
        Ok(self
            .events
            .borrow()
            .iter()
            .filter(|event| {
                event.id <= upto
                    && crate::domain::supervisor_life::supervisor_of(event) == Some(supervisor)
            })
            .max_by_key(|event| event.id)
            .cloned())
    }

    fn write_ledger_row(&self, row: &LedgerRow) -> Result<bool> {
        ensure!(!self.refuses.get(), "attempt to write a readonly database");
        self.writes.set(self.writes.get() + 1);
        let mut rows = self.rows.borrow_mut();
        let key = (row.kind, row.key.clone());
        self.written_keys.borrow_mut().push(key.clone());
        let written = match rows.get(&key).filter(|stored| stored.is_legacy()) {
            Some(stored) => LedgerRow {
                records: row.records.clone(),
                inputs_from: row.inputs_from,
                inputs_through: row.inputs_through.clone(),
                built_at: row.built_at.clone(),
                detail: row.detail.clone(),
                ..stored.clone()
            },
            None => row.clone(),
        };
        let fresh = !rows.contains_key(&key) || !rows[&key].is_legacy();
        rows.insert(key, written);
        Ok(fresh)
    }
    fn write_ledger_row_over(
        &self,
        row: &LedgerRow,
        read: Option<&LedgerRow>,
    ) -> Result<Option<bool>> {
        let held = self.ledger_row(row.kind, &row.key)?;
        if held.as_ref() != read {
            return Ok(None);
        }
        self.write_ledger_row(row).map(Some)
    }
    /// All or none, as one transaction: a refused write or a batch set to
    /// fail ([`Memory::batch_fails`]) writes no row and no mark.
    fn write_session_rows_over(
        &self,
        rows: &[(LedgerRow, Option<LedgerRow>)],
        mark: Option<&SessionsWritten>,
    ) -> Result<bool> {
        ensure!(!self.refuses.get(), "attempt to write a readonly database");
        let call = self.batch_calls.get();
        self.batch_calls.set(call + 1);
        if self.batch_fails_at.get() == Some(call) {
            anyhow::bail!("the transaction failed part way");
        }
        if self.batch_fails.get() > 0 {
            self.batch_fails.set(self.batch_fails.get() - 1);
            anyhow::bail!("the transaction failed part way");
        }
        for (row, read) in rows {
            if self.ledger_row(row.kind, &row.key)?.as_ref() != read.as_ref() {
                return Ok(false);
            }
        }
        for (row, _) in rows {
            self.write_ledger_row(row)?;
        }
        if let Some(mark) = mark {
            self.mark.replace(Some(*mark));
        }
        self.batches.set(self.batches.get() + 1);
        let bytes = rows
            .iter()
            .map(|(row, _)| serde_json::to_vec(row).unwrap().len())
            .sum();
        self.batch_bytes.borrow_mut().push((bytes, mark.is_some()));
        Ok(true)
    }
    fn sessions_written(&self) -> Result<Option<SessionsWritten>> {
        Ok(*self.mark.borrow())
    }
    /// As [`Self::write_session_rows_over`], R with the session rows' mark.
    fn write_session_rows_marked(
        &self,
        rows: &[(LedgerRow, Option<LedgerRow>)],
        marks: Option<&Marks>,
    ) -> Result<bool> {
        let written = self.write_session_rows_over(rows, marks.map(|marks| &marks.sessions))?;
        if written && let Some(rows) = marks.and_then(|marks| marks.rows) {
            self.rows_mark.replace(Some(rows));
        }
        Ok(written)
    }
    /// The marks alone, counted apart from the writes of the session rows.
    fn write_ledger_marks(&self, marks: &Marks) -> Result<()> {
        ensure!(!self.refuses.get(), "attempt to write a readonly database");
        self.mark.replace(Some(marks.sessions));
        if let Some(rows) = marks.rows {
            self.rows_mark.replace(Some(rows));
        }
        self.marks_alone.set(self.marks_alone.get() + 1);
        Ok(())
    }
    fn ledger_marks(&self) -> Result<Option<Marks>> {
        Ok(self.mark.borrow().map(|sessions| Marks {
            sessions,
            rows: *self.rows_mark.borrow(),
        }))
    }
    fn ledger_row(&self, kind: LedgerKind, key: &str) -> Result<Option<LedgerRow>> {
        self.callers.borrow_mut().push((
            "ledger_row",
            crate::application::supervisor_service::caller::current(),
        ));
        self.held_row(kind, key)
    }
    /// One request for every key, unless [`Memory::rows_one_by_one`]: then
    /// one [`LedgerStore::ledger_row`] a key, as a store of no such read.
    fn ledger_rows_of(&self, keys: &[(LedgerKind, String)]) -> Result<Vec<LedgerRow>> {
        if !self.rows_one_by_one.get() {
            self.callers.borrow_mut().push((
                "ledger_rows_of",
                crate::application::supervisor_service::caller::current(),
            ));
        }
        let mut rows = Vec::new();
        for (kind, key) in keys {
            rows.extend(if self.rows_one_by_one.get() {
                self.ledger_row(*kind, key)?
            } else {
                self.held_row(*kind, key)?
            });
        }
        Ok(rows)
    }
    fn ledger_rows(&self, kind: LedgerKind) -> Result<Vec<LedgerRow>> {
        if matches!(kind, LedgerKind::Session | LedgerKind::Node) {
            self.other_read("ledger_rows");
        }
        let rows: Vec<LedgerRow> = self
            .rows
            .borrow()
            .values()
            .filter(|row| row.kind == kind)
            .cloned()
            .collect();
        if kind == LedgerKind::Queue {
            self.queue_read
                .borrow_mut()
                .extend(rows.iter().map(|row| row.key.clone()));
        }
        Ok(rows)
    }
    /// As SQLite gives them: no row read, so none counted.
    fn ledger_older_rows_after(
        &self,
        after: Option<&(LedgerKind, String)>,
        limit: usize,
    ) -> Result<Vec<(LedgerKind, String)>> {
        let place = |(kind, key): &(LedgerKind, String)| (kind.as_str(), key.clone());
        let mut older: Vec<(LedgerKind, String)> = self
            .rows
            .borrow()
            .iter()
            .filter(|(_, row)| !row.is_legacy() && row.ledger_v < LEDGER_V)
            .map(|(key, _)| key.clone())
            .collect();
        older.sort_by_key(place);
        let after = after.map(place);
        Ok(older
            .into_iter()
            .filter(|row| after.as_ref().is_none_or(|after| place(row) > *after))
            .take(limit)
            .collect())
    }
    /// As SQLite gives them: no row's records read, so no key counted.
    fn ledger_queue_heads(&self) -> Result<Vec<crate::domain::ledger::queue::QueueHead>> {
        Ok(self
            .rows
            .borrow()
            .values()
            .filter(|row| row.kind == LedgerKind::Queue)
            .map(crate::domain::ledger::queue::QueueHead::of)
            .collect())
    }
    fn ledger_queue_heads_at(
        &self,
        at: EventId,
        since: pre_claim::rows::Since,
    ) -> Result<Vec<crate::domain::ledger::queue::QueueHead>> {
        let heads = self.ledger_queue_heads_at_after(at, since, None, usize::MAX)?;
        self.heads_given
            .set(self.heads_given.get().max(heads.len()));
        Ok(heads)
    }
    fn ledger_wait_rows(&self, since: pre_claim::rows::Since) -> Result<Vec<LedgerRow>> {
        self.other_read("ledger_wait_rows");
        let rows: Vec<LedgerRow> = self
            .ledger_rows(LedgerKind::Queue)?
            .into_iter()
            .filter(|row| pre_claim::rows::holds(row, since))
            .collect();
        self.wait_rows.set(self.wait_rows.get() + rows.len());
        Ok(rows)
    }
    fn ledger_settled_waits(
        &self,
        held: pre_claim::rows::Since,
        from: pre_claim::rows::Since,
        until_ms: i64,
    ) -> Result<Vec<LedgerRow>> {
        self.other_read("ledger_settled_waits");
        if self.no_settled_waits.get() {
            return Ok(Vec::new());
        }
        let rows: Vec<LedgerRow> = self
            .ledger_rows(LedgerKind::Queue)?
            .into_iter()
            .filter(|row| pre_claim::rows::lacks(row, held, from, until_ms))
            .collect();
        self.wait_rows.set(self.wait_rows.get() + rows.len());
        Ok(rows)
    }
    fn remove_node_rows(&self, before_day: i64) -> Result<usize> {
        let mut rows = self.rows.borrow_mut();
        let before = rows.len();
        let kept: BTreeMap<_, _> = std::mem::take(&mut *rows)
            .into_iter()
            .filter_map(|(key, row)| {
                if row.kind == LedgerKind::Node {
                    node::retained(&row, before_day).map(|row| (key, row))
                } else {
                    Some((key, row))
                }
            })
            .collect();
        *rows = kept;
        Ok(before - rows.len())
    }
}

#[allow(unused_variables)]
impl NodeSampleStore for Memory {
    fn record_node_sample(
        &self,
        node: &str,
        day: i64,
        sample: &HostSample,
        resources: &crate::domain::environment::NodeRate,
        source: SampleSource,
        apart_secs: i64,
    ) -> Result<bool> {
        unreachable!()
    }
    fn import_node_sample_part(
        &self,
        node: &str,
        file: &str,
        day: i64,
        samples: &[HostSample],
        mark: Option<crate::domain::host_metrics::ImportMark>,
    ) -> Result<usize> {
        unreachable!()
    }
    fn imported_files(&self, node: &str) -> Result<BTreeMap<String, u64>> {
        unreachable!()
    }
    fn remove_node_samples(&self, before_day: i64) -> Result<usize> {
        unreachable!()
    }
    fn node_samples(&self, node: &str, from: i64, until: i64) -> Result<Vec<NodeSample>> {
        self.other_read("node_samples");
        Ok(self
            .samples
            .borrow()
            .iter()
            .filter(|s| s.node_id == node && (from..=until).contains(&s.sample.unix))
            .cloned()
            .collect())
    }
    fn node_samples_after(&self, after: i64) -> Result<Vec<NodeSample>> {
        self.other_read("node_samples_after");
        let read: Vec<NodeSample> = self
            .samples
            .borrow()
            .iter()
            .filter(|s| s.id > after)
            .cloned()
            .collect();
        self.scans.borrow_mut().push(("samples", after, read.len()));
        Ok(read)
    }
}

#[allow(unused_variables)]
impl SessionStepStore for Memory {
    fn record_steps(
        &self,
        target: &StepTarget,
        session_id: &str,
        steps: &[Step],
        received_at: i64,
    ) -> Result<StepsRecorded> {
        let mut held = self.steps.borrow_mut();
        for step in steps {
            let id = self.step_id.get() + 1;
            self.step_id.set(id);
            held.push(SessionStep {
                id,
                target: target.clone(),
                session_id: session_id.to_owned(),
                received_at,
                step: step.clone(),
            });
        }
        Ok(StepsRecorded {
            added: steps.len(),
            duplicates: 0,
        })
    }
    fn session_gaps(&self, session_id: &str) -> Result<Vec<SeqRange>> {
        unreachable!()
    }
    fn steps_after(&self, after: i64, limit: usize) -> Result<Vec<SessionStep>> {
        self.other_read("steps_after");
        let between = self.before_steps.borrow_mut().take();
        if let Some(between) = between {
            between(self);
        }
        let read: Vec<SessionStep> = self
            .steps
            .borrow()
            .iter()
            .filter(|step| step.id > after)
            .take(limit)
            .cloned()
            .collect();
        self.scans.borrow_mut().push(("steps", after, read.len()));
        Ok(read)
    }
    fn target_steps(&self, target: &StepTarget) -> Result<Vec<SessionStep>> {
        self.other_read("target_steps");
        let between = self.between.borrow_mut().take();
        if let Some(between) = between {
            between(self);
        }
        Ok(self
            .steps
            .borrow()
            .iter()
            .filter(|step| step.target == *target)
            .cloned()
            .collect())
    }
    fn remove_steps(&self, before: i64) -> Result<usize> {
        let mut held = self.steps.borrow_mut();
        let count = held.len();
        held.retain(|step| step.received_at >= before);
        // As the SQLite store: the samples from inside go with the steps,
        // and an owner registered before it whose session keeps no step
        // and no sample goes too.
        let mut inside = self.inside.borrow_mut();
        inside.retain(|sample| sample.received_at >= before);
        self.owners.borrow_mut().retain(|session, owner| {
            owner.registered_at >= before
                || held.iter().any(|step| &step.session_id == session)
                || inside.iter().any(|sample| &sample.session_id == session)
        });
        Ok(count - held.len())
    }
}

impl SessionOwners for Memory {
    fn register_session(
        &self,
        target: &StepTarget,
        session_id: &str,
        registered_at: i64,
    ) -> Result<()> {
        self.owners.borrow_mut().insert(
            session_id.to_owned(),
            SessionOwner {
                session_id: session_id.to_owned(),
                target: target.clone(),
                registered_at,
            },
        );
        Ok(())
    }
    fn session_owner(&self, session_id: &str) -> Result<Option<SessionOwner>> {
        Ok(self.owners.borrow().get(session_id).cloned())
    }
}

#[allow(unused_variables)]
impl SessionResourceStore for Memory {
    fn record_resources(
        &self,
        target: &StepTarget,
        session_id: &str,
        samples: &[ResourceSample],
        received_at: i64,
    ) -> Result<StepsRecorded> {
        let mut held = self.inside.borrow_mut();
        let mut duplicates = 0;
        for sample in samples {
            if held
                .iter()
                .any(|kept| kept.session_id == session_id && kept.sample.seq == sample.seq)
            {
                duplicates += 1;
                continue;
            }
            let id = self.inside_id.get() + 1;
            self.inside_id.set(id);
            held.push(SessionResource {
                id,
                target: target.clone(),
                session_id: session_id.to_owned(),
                received_at,
                sample: sample.clone(),
            });
        }
        Ok(StepsRecorded {
            added: samples.len() - duplicates,
            duplicates,
        })
    }
    fn target_resources(&self, target: &StepTarget) -> Result<Vec<SessionResource>> {
        self.other_read("target_resources");
        Ok(self
            .inside
            .borrow()
            .iter()
            .filter(|sample| sample.target == *target)
            .cloned()
            .collect())
    }
    fn resources_after(&self, after: i64, limit: usize) -> Result<Vec<SessionResource>> {
        self.other_read("resources_after");
        let read: Vec<SessionResource> = self
            .inside
            .borrow()
            .iter()
            .filter(|sample| sample.id > after)
            .take(limit)
            .cloned()
            .collect();
        self.scans
            .borrow_mut()
            .push(("inside_resources", after, read.len()));
        Ok(read)
    }
}

impl RunResourceStore for Memory {
    fn record_run_resource(&self, row: &RunResource) -> Result<i64> {
        let mut held = self.run_resources.borrow_mut();
        let id = i64::try_from(held.len()).unwrap() + 1;
        held.push(RunResource { id, ..row.clone() });
        Ok(id)
    }
    fn run_resources(&self, run: &RunId) -> Result<Vec<RunResource>> {
        self.resources_read.borrow_mut().push(run.clone());
        Ok(self
            .run_resources
            .borrow()
            .iter()
            .filter(|window| window.run_id == run.as_str())
            .cloned()
            .collect())
    }
    fn run_resources_after(&self, after: i64, limit: usize) -> Result<Vec<RunResource>> {
        let read: Vec<RunResource> = self
            .run_resources
            .borrow()
            .iter()
            .filter(|window| window.id > after)
            .take(limit)
            .cloned()
            .collect();
        self.scans
            .borrow_mut()
            .push(("resources", after, read.len()));
        Ok(read)
    }
    fn open_attachments(&self) -> Result<Vec<(RunId, i64)>> {
        unreachable!("the ledger does not close attachments")
    }
}

/// One pass at `secs` after [`T0`].
fn pass(memory: &Memory, builder: &mut Builder, secs: i64) -> Built {
    build_pass(memory, builder, (ms(secs), BUILT), Retain::default(), None)
}

/// How many passes [`drain`] runs at most.
const DRAIN_PASSES: usize = 10;

/// Passes at `secs` until one fails or leaves no mark, at most
/// [`DRAIN_PASSES`]: the last pass.
fn passes_to_drain(memory: &Memory, builder: &mut Builder, secs: i64) -> Built {
    let mut built = pass(memory, builder, secs);
    for _ in 1..DRAIN_PASSES {
        if built.backlog == 0 || built.failure.is_some() {
            break;
        }
        built = pass(memory, builder, secs);
    }
    built
}

/// [`passes_to_drain`], whose last pass neither failed nor left a mark: a
/// mark left after them (a fold kept behind, a write that fails) fails the
/// test rather than keeping it running.
#[track_caller]
fn drain(memory: &Memory, builder: &mut Builder, secs: i64) -> Built {
    let built = passes_to_drain(memory, builder, secs);
    assert_eq!(
        (built.backlog, &built.failure),
        (0, &None),
        "marks left after at most {DRAIN_PASSES} passes at {secs}"
    );
    built
}

/// The retention runs once a local day: the steps past
/// [`RETENTION_MS`] whether or not the host's load is kept, and the node
/// rows only when it is.
#[test]
fn the_retention_is_due_once_a_day_for_the_steps_and_the_kept_node_rows() {
    let now = 200 * 86_400;
    let today = 200;
    let steps = Some(now * 1000 - RETENTION_MS);
    assert_eq!(
        Retain::due(now, today, None, None),
        Retain {
            node_rows: None,
            steps
        }
    );
    let both = Retain::due(now, today, Some(today - 1), Some(30));
    assert_eq!(both.steps, steps);
    assert!(both.node_rows.is_some());
    assert!(both.any());
    let done = Retain::due(now, today, Some(today), Some(30));
    assert_eq!(done, Retain::default());
    assert!(!done.any());
}

/// A pass with the steps' retention removes the steps that came in
/// before it, whatever their session, keeps the others, and reports how
/// many it removed.
#[test]
fn a_pass_removes_the_steps_past_their_retention() {
    let memory = Memory::default();
    let run = StepTarget::run(&RunId::new("r-steps").unwrap());
    let step = Step::new(1, StepKind::ModelTurn, 0, 10);
    memory
        .record_steps(&run, "old", std::slice::from_ref(&step), 1_000)
        .unwrap();
    memory
        .record_steps(&run, "both", std::slice::from_ref(&step), 1_000)
        .unwrap();
    let later = Step::new(2, StepKind::Wait, 20, 10);
    memory.record_steps(&run, "both", &[later], 5_000).unwrap();
    memory.record_steps(&run, "new", &[step], 5_000).unwrap();
    let mut builder = Builder::default();
    let retain = Retain {
        node_rows: None,
        steps: Some(2_000),
    };
    let built = build_pass(&memory, &mut builder, (ms(0), BUILT), retain, None);
    assert_eq!(built.failure, None);
    assert_eq!(built.steps_removed, 2);
    let kept: Vec<(String, i64)> = memory
        .steps
        .borrow()
        .iter()
        .map(|step| (step.session_id.clone(), step.step.seq))
        .collect();
    assert_eq!(kept, vec![("both".to_owned(), 2), ("new".to_owned(), 1)]);
}

/// A long time after every run here, past every grace.
const LATER: i64 = 1_000_000;

/// The rules alone fold a settled run again: on the same builder, with no
/// input come in since, a pass by rules of another version classifies its
/// command steps by them; passes by the same rules again write nothing,
/// and neither does a new builder's (a new process).
#[test]
fn a_settled_run_is_classified_again_when_only_the_rules_change() {
    use crate::domain::command_rules::{
        CommandClassifier, CommandRule, TelemetrySettings, effective_rules,
    };
    use crate::domain::command_shape::{DigestKey, Subcommands, shape_value_with};
    use crate::domain::ledger::steps::StepFold;
    use crate::domain::toolchain::Toolchains;
    let a = run_id(1);
    let mut log = Log::default();
    log.landed(1, &a, 0);
    let memory = Memory::of(&log);
    let target = StepTarget::run(&RunId::new(&a).unwrap());
    memory.register_session(&target, "s", ms(0)).unwrap();
    let known = Subcommands::default().keyed(Some(DigestKey::test()));
    let mut step = Step::new(1, StepKind::ToolCall, 0, 10);
    step.tool = Some("Bash".into());
    step.command_shape = shape_value_with("make test", &known);
    memory.record_steps(&target, "s", &[step], 1_000).unwrap();
    let rules = |class: &str| {
        let toolchains = Toolchains::none().with_telemetry(TelemetrySettings {
            commands: vec![CommandRule::new(class, "make")],
            ..TelemetrySettings::default()
        });
        CommandClassifier::new(effective_rules(&toolchains), known.clone())
    };
    let (first, second) = (rules("build"), rules("test"));
    let pass_by = |builder: &mut Builder, classifier: &CommandClassifier| {
        build_pass(
            &memory,
            builder,
            (ms(LATER), BUILT),
            Retain::default(),
            Some(classifier),
        )
    };
    let classed = || {
        let row = memory.row(LedgerKind::Run, &a);
        let classes = StepFold::of_detail(&row.detail).unwrap().classes.unwrap();
        let class: Vec<Option<String>> = classes
            .steps
            .iter()
            .map(|step| step.class.clone())
            .collect();
        (row.is_final, classes.rules, class)
    };
    let mut builder = Builder::default();
    while pass_by(&mut builder, &first).backlog > 0 {}
    assert_eq!(
        classed(),
        (
            true,
            first.rules().version(),
            vec![Some("build".to_owned())]
        )
    );
    assert_eq!(pass_by(&mut builder, &first).written, 0);
    // Only the rules change.
    let built = pass_by(&mut builder, &second);
    assert_eq!((built.written, built.failure), (1, None));
    assert_eq!(
        classed(),
        (
            true,
            second.rules().version(),
            vec![Some("test".to_owned())]
        )
    );
    assert_eq!(pass_by(&mut builder, &second).written, 0);
    let writes = memory.writes.get();
    let mut again = Builder::default();
    while pass_by(&mut again, &second).backlog > 0 {}
    assert_eq!(memory.writes.get(), writes);
}

/// Every ended run and task is written once, the passes catching up on
/// more than one pass writes; what came in while no pass ran is caught up
/// on the next one; passes on the same inputs write nothing more, and a new
/// builder reading every input again (a new process) leaves the rows as
/// they are.
#[test]
fn the_ledger_catches_up_once_and_the_same_inputs_leave_it_as_it_is() {
    let mut log = Log::default();
    let runs = WRITES_PER_PASS * 3 / 4;
    for n in 0..runs {
        let n = i64::try_from(n).unwrap();
        log.landed(
            n + 1,
            &format!("{:08x}-2222-4222-8222-222222222222", n),
            n * 10,
        );
    }
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    let first = pass(&memory, &mut builder, LATER);
    assert_eq!(first.written, WRITES_PER_PASS);
    assert!(first.backlog > 0, "{first:?}");
    // Each landed run has a landing slot's row too, marked by its run's
    // events: the run, the task and the slot, three rows a run.
    let second = pass(&memory, &mut builder, LATER);
    assert_eq!(second.written, WRITES_PER_PASS);
    let third = pass(&memory, &mut builder, LATER);
    assert_eq!(third.written, runs * 3 - 2 * WRITES_PER_PASS);
    assert_eq!(third.backlog, 0);
    assert_eq!(memory.count(LedgerKind::Run), runs);
    assert_eq!(memory.count(LedgerKind::Task), runs);
    assert!(memory.rows.borrow().values().all(|row| row.is_final));

    // Stopped meanwhile: two more runs land before the next pass.
    let mut more = Log {
        events: memory.events.borrow().clone(),
    };
    more.landed(9001, &run_id(1), 20_000)
        .landed(9002, &run_id(2), 20_000);
    memory.events.replace(more.events);
    let caught = pass(&memory, &mut builder, LATER);
    // Each run's, task's and landing slot's.
    assert_eq!(caught.written, 6);
    assert_eq!(memory.count(LedgerKind::Run), runs + 2);

    let rows = memory.rows.borrow().clone();
    let writes = memory.writes.get();
    assert_eq!(pass(&memory, &mut builder, LATER).written, 0);
    let mut again = Builder::default();
    while pass(&memory, &mut again, LATER).backlog > 0 {}
    assert_eq!(memory.writes.get(), writes);
    assert_eq!(*memory.rows.borrow(), rows);
}

/// A row of an older fold is built again by the next builder (a new
/// binary); a legacy row is not, its values as first written.
#[test]
fn a_new_fold_builds_the_rows_again_but_a_legacy_one() {
    let (a, d) = (run_id(1), run_id(4));
    let mut log = Log::default();
    log.landed(1, &a, 0).legacy(4, &d, 0);
    let memory = Memory::of(&log);
    pass(&memory, &mut Builder::default(), LATER);
    let legacy = memory.row(LedgerKind::Run, &d);
    assert!(legacy.is_legacy());
    for row in memory.rows.borrow_mut().values_mut() {
        row.ledger_v = LEDGER_V - 1;
    }
    let built = pass(&memory, &mut Builder::default(), LATER);
    // The landing slot's rows of a and of the legacy d too: queue rows,
    // of the older fold as well.
    assert_eq!(built.written, 4, "run a, its task and the landing slots");
    assert_eq!(memory.row(LedgerKind::Run, &a).ledger_v, LEDGER_V);
    let kept = memory.row(LedgerKind::Run, &d);
    assert_eq!(kept.ledger_v, LEDGER_V - 1);
    assert_eq!(kept.legacy, legacy.legacy);
}

/// A legacy row an older fold wrote without the use of providers its
/// records hold is written again by the next builder with it, its frozen
/// values and its version kept, and not again after.
#[test]
fn a_legacy_row_takes_the_use_of_providers_once_its_values_kept() {
    let (a, d) = (run_id(1), run_id(4));
    let mut log = Log::default();
    log.landed(1, &a, 0).legacy(4, &d, 0).add(
        Some(4),
        Some(&d),
        "provider_waiting",
        serde_json::json!({"turn": 1, "reason": "usage_limit"}),
        100,
    );
    let memory = Memory::of(&log);
    pass(&memory, &mut Builder::default(), LATER);
    let legacy = memory.row(LedgerKind::Run, &d);
    assert!(legacy.is_legacy());
    for row in memory.rows.borrow_mut().values_mut() {
        row.ledger_v = LEDGER_V - 1;
        if let Some(detail) = row.detail.as_object_mut() {
            detail.remove(provider_use::DETAIL_KEY);
        }
    }
    pass(&memory, &mut Builder::default(), LATER);
    let kept = memory.row(LedgerKind::Run, &d);
    assert_eq!(kept.ledger_v, LEDGER_V - 1);
    assert_eq!(kept.legacy, legacy.legacy);
    assert_eq!(kept.totals, legacy.totals);
    let uses = read(&kept).provider_use.expect("written again");
    assert_eq!(uses.worker_waits.unwrap()["usage_limit"].count, 1);

    let rows = memory.rows.borrow().clone();
    let writes = memory.writes.get();
    let mut again = Builder::default();
    while pass(&memory, &mut again, LATER).backlog > 0 {}
    assert_eq!(memory.writes.get(), writes);
    assert_eq!(*memory.rows.borrow(), rows);
}

/// A later event of a settled run makes it dirty and it is folded again;
/// another run's push closes a pending push, whose row is folded again
/// ended; the same event read twice does not write it twice. A legacy row
/// a later event reaches follows with its records, its values kept.
#[test]
fn later_inputs_fold_a_row_again_and_a_legacy_one_keeps_its_values() {
    let (a, b, c, d) = (run_id(1), run_id(2), run_id(3), run_id(4));
    let mut log = Log::default();
    log.landed(1, &a, 0)
        .worked(3, &c, 0)
        .integrated(3, &c, 700)
        .push_failed(3, &c, 910)
        .legacy(4, &d, 0);
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    pass(&memory, &mut builder, LATER);
    let settled = memory.row(LedgerKind::Run, &a);
    assert!(settled.is_final);
    let pending = memory.row(LedgerKind::Run, &c);
    assert!(!pending.is_final && pending.ended_at.is_none());
    let legacy = memory.row(LedgerKind::Run, &d);

    log.add(Some(1), Some(&a), "review_finished", json!({}), 5000)
        .add(Some(4), Some(&d), "review_finished", json!({}), 5000)
        .landed(2, &b, 6000);
    memory.events.replace(log.events.clone());
    pass(&memory, &mut builder, LATER);
    let again = memory.row(LedgerKind::Run, &a);
    assert!(again.inputs_through.events > settled.inputs_through.events);
    let closed = memory.row(LedgerKind::Run, &c);
    assert_eq!(closed.ended_at, Some(ms(6930)));
    assert_eq!(closed.detail["closed_by"], json!(b));
    let followed = memory.row(LedgerKind::Run, &d);
    assert!(followed.inputs_through.events > legacy.inputs_through.events);
    assert_eq!(followed.legacy, legacy.legacy);
    assert_eq!(followed.totals, legacy.totals);

    let writes = memory.writes.get();
    builder.mark_events(&log.events);
    pass(&memory, &mut builder, LATER);
    assert_eq!(memory.writes.get(), writes);
}

/// A run is settled only after its end and the grace, by the pass that
/// finds the grace over though nothing came in; a pending push never is.
#[test]
fn a_run_is_settled_after_its_end_and_grace_and_a_pending_push_never() {
    let (a, c) = (run_id(1), run_id(3));
    let mut log = Log::default();
    log.landed(1, &a, 0)
        .worked(3, &c, 0)
        .integrated(3, &c, 700)
        .push_failed(3, &c, 910);
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    let grace = SETTLE_GRACE_MS / 1000;
    pass(&memory, &mut builder, 1000);
    assert!(!memory.row(LedgerKind::Run, &a).is_final);
    pass(&memory, &mut builder, 930 + grace - 1);
    assert!(!memory.row(LedgerKind::Run, &a).is_final);
    pass(&memory, &mut builder, 930 + grace);
    assert!(memory.row(LedgerKind::Run, &a).is_final);
    assert!(memory.row(LedgerKind::Task, "1").is_final);
    pass(&memory, &mut builder, LATER);
    assert!(!memory.row(LedgerKind::Run, &c).is_final);
    assert!(!memory.row(LedgerKind::Task, "3").is_final);
}

/// A run on its way has no row: its end writes it.
#[test]
fn a_run_on_its_way_is_written_once_it_ends() {
    let a = run_id(1);
    let mut log = Log::default();
    log.worked(1, &a, 0);
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    pass(&memory, &mut builder, 700);
    assert_eq!(memory.count(LedgerKind::Run), 0);
    log.integrated(1, &a, 700).pushed(1, &a, 930);
    memory.events.replace(log.events.clone());
    pass(&memory, &mut builder, 1000);
    assert_eq!(memory.count(LedgerKind::Run), 1);
}

/// A store that cannot be written writes nothing and keeps every mark: the
/// next pass that can writes them all.
#[test]
fn a_store_that_cannot_be_written_keeps_the_marks_for_the_next_pass() {
    let mut log = Log::default();
    log.landed(1, &run_id(1), 0).landed(2, &run_id(2), 0);
    let memory = Memory::of(&log);
    memory.refuses.set(true);
    let mut builder = Builder::default();
    let failed = pass(&memory, &mut builder, LATER);
    assert!(failed.failure.unwrap().contains("readonly"));
    // Each run's, task's and landing slot's, marked by the run's events.
    assert_eq!(failed.backlog, 6);
    assert!(memory.rows.borrow().is_empty());
    memory.refuses.set(false);
    assert_eq!(pass(&memory, &mut builder, LATER).written, 6);
}

/// The session and queue rows are written once; a later event of a
/// session folds its row again; the same inputs write nothing more.
#[test]
fn session_and_queue_rows_are_written_once_and_again_when_dirty() {
    let mut log = Log::default();
    log.add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 5, "supervisor": "s1"}),
        0,
    )
    .add(None, None, "claim_held", json!({"supervisor": "s1"}), 10)
    .add(None, None, "claim_resumed", json!({"supervisor": "s1"}), 20);
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    pass(&memory, &mut builder, 100);
    let (sessions, queue) = (
        memory.count(LedgerKind::Session),
        memory.count(LedgerKind::Queue),
    );
    assert!(sessions >= 1 && queue >= 1, "{:?}", memory.rows.borrow());
    let writes = memory.writes.get();
    pass(&memory, &mut builder, 100);
    pass(&memory, &mut Builder::default(), 100);
    assert_eq!(memory.writes.get(), writes);

    let session = memory
        .rows
        .borrow()
        .values()
        .find(|row| row.kind == LedgerKind::Session)
        .cloned()
        .unwrap();
    log.add(
        None,
        None,
        "session_closed",
        json!({"kind": "runtime_planner", "planner_id": 5, "opened_event_id": 1}),
        50,
    );
    memory.events.replace(log.events.clone());
    pass(&memory, &mut builder, 100);
    let again = memory.row(LedgerKind::Session, &session.key);
    assert!(again.inputs_through.last() > session.inputs_through.last());
    assert_eq!(memory.count(LedgerKind::Session), sessions);
}

fn sample(id: i64, unix: i64) -> NodeSample {
    NodeSample {
        resources: Default::default(),
        id,
        node_id: HOST_NODE.to_owned(),
        local_day: unix.div_euclid(86_400),
        source: SampleSource::Sampled,
        sample: HostSample::new(unix),
    }
}

/// A node's hour is one row however often it is read; a sample that comes
/// in after its row was settled makes it dirty and it is folded again.
#[test]
fn a_node_row_is_one_per_hour_and_a_late_sample_folds_it_again() {
    let start = node::bucket_of(T0);
    let memory = Memory::default();
    memory
        .samples
        .replace(vec![sample(1, start + 10), sample(2, start + 40)]);
    let mut builder = Builder::default();
    let after = start - T0 + 3600 + SETTLE_GRACE_MS / 1000;
    pass(&memory, &mut builder, after);
    pass(&memory, &mut Builder::default(), after);
    assert_eq!(memory.count(LedgerKind::Node), 1);
    let key = node::key(HOST_NODE, start);
    let settled = memory.row(LedgerKind::Node, &key);
    assert!(settled.is_final);
    memory.samples.borrow_mut().push(sample(3, start + 70));
    pass(&memory, &mut builder, after);
    let again = memory.row(LedgerKind::Node, &key);
    assert_eq!(again.inputs_through.node_samples, Some(3));
    assert_eq!(memory.count(LedgerKind::Node), 1);
}

/// A session or queue row written before its grace is over is settled by
/// the pass that finds the grace over, though no event came in; and one a
/// pass's bound or a failed write left marked is written by a later pass
/// that reads no new event either.
#[test]
fn session_and_queue_rows_left_marked_are_written_by_a_later_pass() {
    let mut log = Log::default();
    log.add(None, None, "claim_held", json!({"supervisor": "s1"}), 0)
        .add(None, None, "claim_resumed", json!({"supervisor": "s1"}), 10);
    let memory = Memory::of(&log);
    memory.refuses.set(true);
    let mut builder = Builder::default();
    assert!(pass(&memory, &mut builder, 20).failure.is_some());
    memory.refuses.set(false);
    pass(&memory, &mut builder, 20);
    let rows: Vec<LedgerRow> = memory.rows.borrow().values().cloned().collect();
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|row| !row.is_final), "{rows:?}");
    pass(&memory, &mut builder, 10 + SETTLE_GRACE_MS / 1000);
    let settled: Vec<LedgerRow> = memory.rows.borrow().values().cloned().collect();
    assert!(
        settled
            .iter()
            .filter(|row| row.ended_at.is_some())
            .all(|row| row.is_final),
        "{settled:?}"
    );
    assert!(settled.iter().any(|row| row.is_final));
}

/// A queue with every kind of row: landed runs, one pending on its push
/// that a later push closes, a legacy one, a landing its run left for a
/// session (a status on an event that ends no landing by its kind), a
/// session, the waits before a claim, a deferral a claim ends, a hold, the
/// supervisors' lives and a node's hour; the run events carry payloads the
/// folds of the queue as a whole do not read.
fn every_kind_of_row() -> (Log, Vec<NodeSample>) {
    let (a, b, c, d, e) = (run_id(1), run_id(2), run_id(3), run_id(4), run_id(5));
    let mut log = Log::default();
    log.add(
        None,
        None,
        "supervisor_started",
        json!({"supervisor": "s1"}),
        0,
    )
    .add(
        None,
        None,
        "slots_full_started",
        json!({"supervisor": "s1", "class": "heavy"}),
        1,
    )
    .add(
        Some(5),
        None,
        "claim_deferred",
        json!({"supervisor": "s1", "reason": "paths"}),
        2,
    )
    .landed(1, &a, 0)
    .add(
        Some(1),
        Some(&a),
        "review_finished",
        json!({"verdict": "pass", "body": "x".repeat(512)}),
        300,
    )
    .add(
        None,
        None,
        "slots_full_ended",
        json!({"supervisor": "s1", "class": "heavy"}),
        400,
    )
    .add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 5, "supervisor": "s1"}),
        500,
    )
    .add(None, None, "claim_held", json!({"supervisor": "s1"}), 510)
    .add(
        None,
        None,
        "claim_resumed",
        json!({"supervisor": "s1"}),
        520,
    )
    .worked(3, &c, 600)
    .integrated(3, &c, 1300)
    .push_failed(3, &c, 1510)
    .legacy(4, &d, 0)
    .add(
        Some(5),
        Some(&e),
        "run_claimed",
        json!({"supervisor": "s1", "provider": "claude"}),
        1600,
    )
    .add(
        Some(5),
        Some(&e),
        "integration_started",
        json!({"supervisor": "s1"}),
        1700,
    )
    .add(
        Some(5),
        Some(&e),
        "validation_finished",
        json!({"status": "needs_session", "error": "conflict"}),
        1750,
    )
    .add(
        None,
        None,
        "session_closed",
        json!({"kind": "runtime_planner", "planner_id": 5, "opened_event_id": 1}),
        1800,
    )
    .landed(2, &b, 2000)
    .add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s2"}),
        3000,
    )
    .add(
        None,
        None,
        "supervisor_stopped",
        json!({"supervisor": "s1", "last_heartbeat_at": T0 + 3100}),
        3200,
    );
    let start = node::bucket_of(T0);
    (log, vec![sample(1, start + 10), sample(2, start + 40)])
}

/// The target a stored row is of.
fn target_of(kind: LedgerKind, key: &str) -> Target {
    match kind {
        LedgerKind::Run => Target::Run(key.to_owned()),
        LedgerKind::Task => Target::Task(TaskId::new(key.parse().unwrap())),
        LedgerKind::Session | LedgerKind::Queue => Target::Row(kind, key.to_owned()),
        LedgerKind::Node => {
            let start = node::bucket_of(T0);
            assert_eq!(key, node::key(HOST_NODE, start));
            Target::Node(HOST_NODE.to_owned(), start)
        }
    }
}

/// The passes, reading the new events a page at a time and each run's and
/// task's events for its fold, write the same queue, run, task, session
/// and node rows as folding every target on the whole EventStore at once.
#[test]
fn the_passes_write_the_rows_a_fold_of_the_whole_event_store_does() {
    let (log, samples) = every_kind_of_row();
    let memory = Memory::of(&log);
    memory.samples.replace(samples.clone());
    let mut builder = Builder::default();
    while pass(&memory, &mut builder, LATER).backlog > 0 {}
    assert_eq!(memory.read_all.get(), 0);

    let whole = Inputs::new(&log.events, ms(LATER), BUILT, true);
    let rows = memory.rows.borrow().clone();
    for kind in [
        LedgerKind::Run,
        LedgerKind::Task,
        LedgerKind::Session,
        LedgerKind::Queue,
        LedgerKind::Node,
    ] {
        assert!(rows.keys().any(|(k, _)| *k == kind), "no {kind:?} row");
    }
    for ((kind, key), row) in &rows {
        let built = build(&target_of(*kind, key), &whole, &samples);
        assert_eq!(built.as_ref(), Some(row), "{kind:?} {key}");
    }
    for (kind, key) in whole.rows.keys() {
        assert!(rows.contains_key(&(*kind, key.clone())), "{kind:?} {key}");
    }
}

/// A pass reads the events after its cursor and those of the runs and
/// tasks it folds, never all of them: after one more landing and a
/// supervisor's event, a pass reads as many events however many the queue
/// held before.
#[test]
fn a_pass_reads_what_came_in_and_what_it_folds_whatever_the_queue_holds() {
    let read_after_one_more = |runs: i64| {
        let mut log = Log::default();
        for n in 0..runs {
            log.landed(
                n + 1,
                &format!("{n:08x}-2222-4222-8222-222222222222"),
                n * 10,
            );
        }
        let pages = log.events.len().div_ceil(EVENTS_PER_READ);
        let memory = Memory::of(&log);
        let mut builder = Builder::default();
        while pass(&memory, &mut builder, LATER).backlog > 0 {}
        assert_eq!(builder.events, log.last(), "{pages} pages");
        let caught_up = memory.read.get();
        assert!(caught_up >= log.events.len(), "{caught_up}");

        let before = log.events.len();
        log.landed(9001, &run_id(1), 20_000).add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "s"}),
            21_000,
        );
        let new = log.events.len() - before;
        memory.events.replace(log.events.clone());
        memory.read.set(0);
        pass(&memory, &mut builder, LATER);
        assert!(
            memory
                .rows
                .borrow()
                .contains_key(&(LedgerKind::Run, run_id(1)))
        );
        assert!(memory.count(LedgerKind::Queue) > 0);
        assert_eq!(memory.read_all.get(), 0);
        (memory.read.get(), new, pages)
    };
    let (few, new, _) = read_after_one_more(3);
    let (many, _, pages) = read_after_one_more(300);
    assert!(pages > 1, "the catch-up reads more than one page");
    assert_eq!(few, many);
    // The new events, and the run's and the task's of them for their folds.
    assert!(few <= new * 3, "{few} of {new}");
}

/// The events of every kind of row of the queue as a whole, a run's and a
/// task's, each interval ended by an event of its own, so that every row
/// a later event changes reads it.
fn in_time_order() -> Log {
    let (a, b, c) = (run_id(1), run_id(2), run_id(3));
    let mut log = Log::default();
    log.add(
        None,
        None,
        "supervisor_started",
        json!({"supervisor": "s1"}),
        0,
    )
    .add(
        None,
        None,
        "slots_full_started",
        json!({"supervisor": "s1", "class": "heavy"}),
        1,
    )
    .add(
        Some(5),
        None,
        "claim_deferred",
        json!({"supervisor": "s1", "reason": "paths"}),
        2,
    )
    .add(
        None,
        None,
        "live_alert_baseline",
        json!({"supervisor": "s1", "version": 1}),
        3,
    )
    .landed(1, &a, 10)
    .add(
        None,
        None,
        "slots_full_ended",
        json!({"supervisor": "s1", "class": "heavy"}),
        1000,
    )
    .add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 5, "supervisor": "s1"}),
        1010,
    )
    .add(None, None, "claim_held", json!({"supervisor": "s1"}), 1020)
    .add(
        None,
        None,
        "live_alert_input_changed",
        json!({"supervisor": "s1", "version": 2}),
        1030,
    )
    .add(
        None,
        None,
        "claim_resumed",
        json!({"supervisor": "s2"}),
        1040,
    )
    .add(
        Some(5),
        None,
        "claim_deferral_ended",
        json!({"supervisor": "s1", "why": "free"}),
        1050,
    )
    .add(
        None,
        None,
        "session_closed",
        json!({"kind": "runtime_planner", "planner_id": 5, "opened_event_id": 7}),
        1060,
    )
    .landed(3, &c, 1100)
    .add(None, None, "mark_added", json!({"mark": "m"}), 2100)
    .add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s2"}),
        2200,
    )
    .landed(2, &b, 2300)
    .add(
        None,
        None,
        "supervisor_stopped",
        json!({"supervisor": "s1"}),
        3300,
    );
    log
}

/// Passes that each read a few events more, folding only the session and
/// queue rows those reached, end with the queue, run, task and session
/// rows of the ledger's fold of every event at once
/// ([`crate::domain::ledger::build`]), settled as the builder says, for
/// every row the builder writes.
#[test]
fn passes_on_a_few_events_each_write_the_rows_of_the_fold_of_every_event() {
    let log = in_time_order();
    let memory = Memory::default();
    let mut builder = Builder::default();
    for read in (1..=log.events.len()).step_by(3).chain([log.events.len()]) {
        memory.events.replace(log.events[..read].to_vec());
        while pass(&memory, &mut builder, LATER).backlog > 0 {}
    }
    let whole: BTreeMap<(LedgerKind, String), LedgerRow> =
        crate::domain::ledger::build(&log.events, ms(LATER), BUILT)
            .into_iter()
            .map(|row| ((row.kind, row.key.clone()), row))
            .collect();
    let rows = memory.rows.borrow();
    for kind in [
        LedgerKind::Run,
        LedgerKind::Task,
        LedgerKind::Session,
        LedgerKind::Queue,
    ] {
        assert!(rows.keys().any(|(k, _)| *k == kind), "no {kind:?} row");
    }
    let at_once = Inputs::new(&log.events, ms(LATER), BUILT, true);
    for ((kind, key), row) in rows.iter() {
        // A row its later inputs put on its way again is not written.
        let Some(built) = build(&target_of(*kind, key), &at_once, &[]) else {
            continue;
        };
        assert_eq!(row, &built, "{kind:?} {key}");
        let mut expected = whole[&(*kind, key.clone())].clone();
        expected.is_final = row.is_final;
        assert_eq!(row, &expected, "{kind:?} {key}");
    }
    // Every session row and interval or event of the queue is written; a
    // landing slot's is written by the passes that build the rows while
    // it reads a new input, as any row they mark.
    for ((kind, key), row) in &whole {
        let slot = row
            .subject
            .as_ref()
            .is_some_and(|s| s.kind == "landing_slot");
        if matches!(kind, LedgerKind::Session | LedgerKind::Queue) && !slot {
            assert!(rows.contains_key(&(*kind, key.clone())), "{kind:?} {key}");
        }
    }
}

/// Whatever many events each pass reads (1, 2 or 3 more, or all at once),
/// after each pass the store holds the queue, run, task, session and node
/// rows of the ledger's fold of the events read so far
/// ([`crate::domain::ledger::build`]), settled as the builder says: a
/// row ended by an event it does not record (a deferral a claim ends), the
/// landing slot of a run with no supervisor's event, a task left pending
/// on its push that a later push closes among them. A run on its way is
/// not written, and one a later input put on its way again is left as it
/// was; a task on its way is written, not settled; a legacy row keeps the
/// values first written. The node samples come in one more a pass, and the
/// node rows are those of the samples so far ([`node::node_rows`]).
#[test]
fn passes_on_any_number_of_events_each_write_the_rows_of_the_fold_so_far() {
    let (every_kind, samples) = every_kind_of_row();
    let mut landed = Log::default();
    landed.landed(1, &run_id(1), 0);
    for (name, log) in [("every kind", every_kind), ("landed", landed)] {
        let all = log.events.len();
        for step in [1, 2, 3, all] {
            let memory = Memory::default();
            let mut builder = Builder::default();
            let mut read = 0;
            let mut passes = 0;
            while read < all {
                read = (read + step).min(all);
                passes += 1;
                let events = &log.events[..read];
                let samples = &samples[..passes.min(samples.len())];
                memory.events.replace(events.to_vec());
                memory.samples.replace(samples.to_vec());
                while pass(&memory, &mut builder, LATER).backlog > 0 {}
                let at = format!("{name}, {step} a pass, {read} read");
                let nodes = node::node_rows(samples, BUILT);
                assert_eq!(memory.count(LedgerKind::Node), nodes.len(), "{at}");
                for mut expected in nodes {
                    let stored = memory.row(LedgerKind::Node, &expected.key);
                    expected.is_final = stored.is_final;
                    assert_eq!(stored, expected, "{at}: node {}", expected.key);
                }
                let whole = crate::domain::ledger::build(events, ms(LATER), BUILT);
                let rows = memory.rows.borrow();
                let at_once = Inputs::new(events, ms(LATER), BUILT, true);
                for expected in whole {
                    let key = (expected.kind, expected.key.clone());
                    let target = target_of(expected.kind, &expected.key);
                    let stored = rows.get(&key);
                    // Every queue and session row is written, and the
                    // task another run's push closed the pending push of.
                    let required = matches!(expected.kind, LedgerKind::Queue | LedgerKind::Session)
                        || (name == "every kind"
                            && read == all
                            && key == (LedgerKind::Task, "3".to_owned()));
                    if !required && build(&target, &at_once, &[]).is_none() {
                        // On its way: not written, or left as it was.
                        continue;
                    }
                    let Some(stored) = stored else {
                        panic!("{at}: no {key:?}");
                    };
                    let mut expected = expected;
                    expected.is_final = stored.is_final;
                    if expected.is_legacy() {
                        // The store keeps a legacy row's values as first
                        // written; its records follow.
                        expected.legacy.clone_from(&stored.legacy);
                    }
                    assert_eq!(stored, &expected, "{at}: {key:?}");
                }
            }
        }
    }
}

/// The worker's session of `run`, registered to it at `secs` after [`T0`]:
/// its steps lie on the supervisor's clock from then.
fn session_of(memory: &Memory, run: &str, session: &str, secs: i64) -> StepTarget {
    let target = StepTarget::run(&RunId::new(run).unwrap());
    memory.register_session(&target, session, ms(secs)).unwrap();
    target
}

/// `steps` of `session` of `target`, received at `received_at` (unix ms).
fn steps_in(memory: &Memory, target: &StepTarget, session: &str, steps: &[Step], received_at: i64) {
    memory
        .record_steps(target, session, steps, received_at)
        .unwrap();
}

/// What `row` holds of its steps under its detail.
fn steps_of(row: &LedgerRow) -> &Value {
    &row.detail[crate::domain::ledger::steps::DETAIL_KEY]
}

/// The worker interval's kinds and coverage of `row`'s steps.
fn worker_parent(row: &LedgerRow) -> &Value {
    steps_of(row)["parents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|parent| parent["phase"] == "worker")
        .unwrap()
}

/// A run's row holds its steps folded per interval: in the worker's
/// (10 s to 600 s after its claim) each kind's time, the share any step
/// covers and the rest, the steps' sessions placed by when they were
/// registered to the run; and how far into the steps it read.
#[test]
fn a_runs_row_holds_its_steps_by_kind_and_each_intervals_coverage() {
    let a = run_id(1);
    let mut log = Log::default();
    log.landed(1, &a, 0);
    let memory = Memory::of(&log);
    let target = session_of(&memory, &a, "s-1", 10);
    // From 10 s: a turn 10..30, a subagent 30..90 with a call 40..60 in it,
    // a wait 100..130 (seconds after the claim).
    steps_in(
        &memory,
        &target,
        "s-1",
        &[
            Step::new(1, StepKind::ModelTurn, 0, 20_000),
            Step::new(2, StepKind::Subagent, 20_000, 60_000),
            Step::new(3, StepKind::ToolCall, 30_000, 20_000),
            Step::new(4, StepKind::Wait, 90_000, 30_000),
        ],
        ms(200),
    );
    let mut builder = Builder::default();
    pass(&memory, &mut builder, 1000);
    let row = memory.row(LedgerKind::Run, &a);
    assert_eq!(row.inputs_through.steps, Some(4));
    let worker = worker_parent(&row);
    assert_eq!(
        worker["by_kind"],
        json!({"model_turn": 20_000, "tool_call": 20_000, "subagent": 60_000, "wait": 30_000})
    );
    // 10..90 and 100..130 of the worker's 590 s.
    assert_eq!(worker["covered_ms"], 110_000);
    assert_eq!(worker["uncovered_ms"], 480_000);
    let coverage = worker["coverage"].as_f64().unwrap();
    assert!((coverage - 110.0 / 590.0).abs() < 1e-9, "{coverage}");
    assert_eq!(steps_of(&row)["outside"], 0);
    assert_eq!(steps_of(&row)["missing"], 0);
    // A run with no step holds none.
    let mut other = Log::default();
    other.landed(2, &run_id(2), 0);
    let none = Memory::of(&other);
    pass(&none, &mut Builder::default(), 1000);
    let row = none.row(LedgerKind::Run, &run_id(2));
    assert!(
        row.detail
            .get(crate::domain::ledger::steps::DETAIL_KEY)
            .is_none()
    );
    assert_eq!(row.inputs_through.steps, None);
}

/// A step that comes in after the run's row was written, before it is
/// settled, makes it dirty: the next pass folds it again with the step.
/// A step sent again (here under a new id, as once the retention removed
/// its session) leaves the row as it is, and nothing marks it again.
#[test]
fn a_step_after_the_row_folds_it_again_and_one_sent_again_leaves_it() {
    let a = run_id(1);
    let mut log = Log::default();
    log.landed(1, &a, 0);
    let memory = Memory::of(&log);
    let target = session_of(&memory, &a, "s-1", 10);
    steps_in(
        &memory,
        &target,
        "s-1",
        &[Step::new(1, StepKind::ModelTurn, 0, 20_000)],
        ms(100),
    );
    let mut builder = Builder::default();
    pass(&memory, &mut builder, 1000);
    let first = memory.row(LedgerKind::Run, &a);
    assert!(!first.is_final);
    assert_eq!(steps_of(&first)["fold"]["counts"], json!({"model_turn": 1}));
    let writes = memory.writes.get();
    steps_in(
        &memory,
        &target,
        "s-1",
        &[Step::new(2, StepKind::ToolCall, 20_000, 10_000)],
        ms(1000),
    );
    pass(&memory, &mut builder, 1001);
    assert_eq!(memory.writes.get(), writes + 1);
    let second = memory.row(LedgerKind::Run, &a);
    assert_eq!(second.inputs_through.steps, Some(2));
    assert_eq!(
        steps_of(&second)["fold"]["counts"],
        json!({"model_turn": 1, "tool_call": 1})
    );
    assert_eq!(worker_parent(&second)["covered_ms"], 30_000);
    // Both sent again: the row stays as it is.
    steps_in(
        &memory,
        &target,
        "s-1",
        &[
            Step::new(1, StepKind::Wait, 0, 99_000),
            Step::new(2, StepKind::ToolCall, 20_000, 10_000),
        ],
        ms(1002),
    );
    pass(&memory, &mut builder, 1003);
    assert_eq!(memory.writes.get(), writes + 1);
    assert_eq!(memory.row(LedgerKind::Run, &a), second);
    let read = memory.read.get();
    pass(&memory, &mut builder, 1004);
    assert_eq!(memory.read.get(), read, "nothing is marked again");
}

/// A run's row is settled only once its telemetry token's revocation is
/// recorded after its last issue: past its grace with the token still
/// held (its revocation failed, so none is recorded) it is not, and no
/// pass reads it again until the revocation's event comes in. A resume's
/// revocation before the next issue does not settle it.
#[test]
fn a_run_is_settled_only_after_its_telemetry_tokens_revocation_is_recorded() {
    let a = run_id(1);
    let mut log = Log::default();
    log.worked(1, &a, 0)
        .add(Some(1), Some(&a), "telemetry_token_issued", json!({}), 0)
        .add(
            Some(1),
            Some(&a),
            "telemetry_token_revoked",
            json!({"reason": "reissued"}),
            300,
        )
        .add(Some(1), Some(&a), "telemetry_token_issued", json!({}), 300)
        .integrated(1, &a, 700)
        .pushed(1, &a, 930);
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    pass(&memory, &mut builder, LATER);
    let held = memory.row(LedgerKind::Run, &a);
    assert!(held.ended_at.is_some());
    assert!(!held.is_final);
    // Its task settles on its own grace.
    assert!(memory.row(LedgerKind::Task, "1").is_final);
    let read = memory.read.get();
    assert_eq!(pass(&memory, &mut builder, LATER + 1).backlog, 0);
    assert_eq!(memory.read.get(), read, "the held token waits on its event");
    log.add(
        Some(1),
        Some(&a),
        "telemetry_token_revoked",
        json!({"reason": "run_ended"}),
        LATER,
    );
    memory.events.replace(log.events.clone());
    pass(&memory, &mut builder, LATER + 2);
    assert!(memory.row(LedgerKind::Run, &a).is_final);
}

/// Once the retention removed a run's steps and its session's owner, a
/// pass a later event or step marks folds the row again over what it
/// held: the kinds and the coverage do not go down, and a late step that
/// fills an earlier gap is added (the gap closes), placed by the session's
/// start the row kept.
#[test]
fn a_fold_after_the_retention_keeps_the_steps_and_adds_a_late_one() {
    let a = run_id(1);
    let mut log = Log::default();
    log.landed(1, &a, 0);
    let memory = Memory::of(&log);
    let target = session_of(&memory, &a, "s-1", 10);
    steps_in(
        &memory,
        &target,
        "s-1",
        &[
            Step::new(1, StepKind::ModelTurn, 0, 20_000),
            Step::new(3, StepKind::ToolCall, 40_000, 20_000),
        ],
        1_000,
    );
    let mut builder = Builder::default();
    pass(&memory, &mut builder, 1000);
    let first = memory.row(LedgerKind::Run, &a);
    assert_eq!(steps_of(&first)["missing"], 1);
    assert_eq!(worker_parent(&first)["covered_ms"], 40_000);
    let retain = Retain {
        node_rows: None,
        steps: Some(ms(500)),
    };
    let removed = build_pass(&memory, &mut builder, (ms(1001), BUILT), retain, None);
    assert_eq!(removed.steps_removed, 2);
    assert!(memory.steps.borrow().is_empty());
    assert_eq!(memory.session_owner("s-1").unwrap(), None);
    // A later event of the run folds it again: what it held stays.
    log.add(Some(1), Some(&a), "review_finished", json!({}), 1002);
    memory.events.replace(log.events.clone());
    pass(&memory, &mut builder, 1003);
    let again = memory.row(LedgerKind::Run, &a);
    assert!(again.inputs_through.events > first.inputs_through.events);
    assert_eq!(steps_of(&again), steps_of(&first));
    assert_eq!(again.inputs_through.steps, first.inputs_through.steps);
    // Seq 2 comes in late, and seq 3 is sent again.
    steps_in(
        &memory,
        &target,
        "s-1",
        &[
            Step::new(2, StepKind::Wait, 20_000, 20_000),
            Step::new(3, StepKind::ToolCall, 40_000, 20_000),
        ],
        ms(1004),
    );
    pass(&memory, &mut builder, 1005);
    let late = memory.row(LedgerKind::Run, &a);
    assert_eq!(steps_of(&late)["missing"], 0);
    assert_eq!(
        steps_of(&late)["fold"]["counts"],
        json!({"model_turn": 1, "tool_call": 1, "wait": 1})
    );
    assert_eq!(worker_parent(&late)["covered_ms"], 60_000);
    assert_eq!(
        worker_parent(&late)["by_kind"],
        json!({"model_turn": 20_000, "tool_call": 20_000, "subagent": 0, "wait": 20_000})
    );
    assert_eq!(steps_of(&late)["fold"]["unplaced"], json!([]));
}

/// A pass that writes up to its bound leaves the other runs marked, and
/// the steps' retention of that pass keeps their steps: a later pass
/// writes those runs with their steps, and only then are they removed.
#[test]
fn the_retention_keeps_the_steps_of_the_runs_a_pass_did_not_write_yet() {
    let runs: Vec<String> = (1..=u8::try_from(WRITES_PER_PASS + 1).unwrap())
        .map(run_id)
        .collect();
    let mut log = Log::default();
    for (task, run) in (1..).zip(&runs) {
        log.landed(task, run, 0);
    }
    let memory = Memory::of(&log);
    for run in &runs {
        let session = format!("s-{run}");
        let target = session_of(&memory, run, &session, 10);
        steps_in(
            &memory,
            &target,
            &session,
            &[Step::new(1, StepKind::ModelTurn, 0, 20_000)],
            1_000,
        );
    }
    let retain = Retain {
        node_rows: None,
        steps: Some(2_000),
    };
    let mut builder = Builder::default();
    let first = build_pass(&memory, &mut builder, (ms(LATER), BUILT), retain, None);
    assert_eq!(first.failure, None);
    assert_eq!(first.written, WRITES_PER_PASS);
    assert!(first.backlog > 0);
    assert_eq!(first.steps_removed, 0, "a run not written keeps its steps");
    let unwritten = runs
        .iter()
        .filter(|run| {
            memory
                .rows
                .borrow()
                .get(&(LedgerKind::Run, (*run).clone()))
                .is_none()
        })
        .count();
    // The tasks taken first ([`HOLDING_R_PER_PASS`]) are among the writes.
    assert!(
        (1..=1 + HOLDING_R_PER_PASS).contains(&unwritten),
        "{unwritten}"
    );
    let mut built = build_pass(&memory, &mut builder, (ms(LATER + 1), BUILT), retain, None);
    while built.backlog > 0 {
        built = build_pass(&memory, &mut builder, (ms(LATER + 2), BUILT), retain, None);
    }
    for run in &runs {
        let row = memory.row(LedgerKind::Run, run);
        assert_eq!(
            steps_of(&row)["fold"]["counts"],
            json!({"model_turn": 1}),
            "{run}"
        );
    }
    assert!(memory.steps.borrow().is_empty());
}

/// As the steps: the samples from inside of a run a pass did not write
/// yet (past its bound) are kept by the retention, which removes them
/// with the steps, until the run's row holds them; then they go, and the
/// owners of their sessions with them.
#[test]
fn the_retention_keeps_the_samples_from_inside_of_the_runs_a_pass_did_not_write_yet() {
    let runs: Vec<String> = (1..=u8::try_from(WRITES_PER_PASS + 1).unwrap())
        .map(run_id)
        .collect();
    let mut log = Log::default();
    for (task, run) in (1..).zip(&runs) {
        log.landed(task, run, 0);
    }
    let memory = Memory::of(&log);
    for run in &runs {
        let session = format!("s-{run}");
        // Registered before the retention's bound: its owner goes with its
        // last sample.
        let target = StepTarget::run(&RunId::new(run).unwrap());
        memory.register_session(&target, &session, 500).unwrap();
        let mut sample = ResourceSample::new(1, 0, 100_000);
        sample.cpu_seconds = Some(40.0);
        memory
            .record_resources(&target, &session, &[sample], 1_000)
            .unwrap();
    }
    let retain = Retain {
        node_rows: None,
        steps: Some(2_000),
    };
    let mut builder = Builder::default();
    let first = build_pass(&memory, &mut builder, (ms(LATER), BUILT), retain, None);
    assert_eq!(first.failure, None);
    assert_eq!(first.written, WRITES_PER_PASS);
    assert!(first.backlog > 0);
    assert_eq!(
        memory.inside.borrow().len(),
        runs.len(),
        "a run not written keeps its samples"
    );
    assert_eq!(memory.owners.borrow().len(), runs.len());
    let mut built = build_pass(&memory, &mut builder, (ms(LATER + 1), BUILT), retain, None);
    while built.backlog > 0 {
        built = build_pass(&memory, &mut builder, (ms(LATER + 2), BUILT), retain, None);
    }
    for run in &runs {
        let row = memory.row(LedgerKind::Run, run);
        assert_eq!(resources_of(&row)["source"], "inside", "{run}");
        assert_eq!(
            resources_of(&row)["total"]["cpu_seconds"],
            json!(40.0),
            "{run}"
        );
    }
    assert!(memory.inside.borrow().is_empty());
    assert!(memory.owners.borrow().is_empty());
}

/// Two builders on one queue (two supervisors): while one folds a run
/// whose row it read as missing, the other writes the row with the run's
/// steps and its retention removes them. The first one's write finds the
/// row written since its read, folds again over it, and the row keeps the
/// steps; a write over a row read before another write writes nothing.
#[test]
fn a_fold_over_a_row_another_builder_wrote_since_its_read_folds_again_and_keeps_the_steps() {
    let a = run_id(1);
    let mut log = Log::default();
    log.landed(1, &a, 0);
    let memory = Memory::of(&log);
    let target = session_of(&memory, &a, "s-1", 10);
    steps_in(
        &memory,
        &target,
        "s-1",
        &[
            Step::new(1, StepKind::ModelTurn, 0, 20_000),
            Step::new(2, StepKind::ToolCall, 20_000, 10_000),
        ],
        1_000,
    );
    memory.between.replace(Some(Box::new(|memory: &Memory| {
        let retain = Retain {
            node_rows: None,
            steps: Some(ms(500)),
        };
        let other = build_pass(
            memory,
            &mut Builder::default(),
            (ms(1000), BUILT),
            retain,
            None,
        );
        assert_eq!(other.failure, None);
        assert_eq!(other.steps_removed, 2);
    })));
    let mut builder = Builder::default();
    let built = pass(&memory, &mut builder, 1000);
    assert_eq!(built.failure, None);
    assert!(memory.between.borrow().is_none(), "the other pass ran");
    assert!(memory.steps.borrow().is_empty());
    let row = memory.row(LedgerKind::Run, &a);
    assert_eq!(
        steps_of(&row)["fold"]["counts"],
        json!({"model_turn": 1, "tool_call": 1})
    );
    assert_eq!(worker_parent(&row)["covered_ms"], 30_000);
    assert_eq!(row.inputs_through.steps, Some(2));
    // A write over the row as it was before (none) writes nothing.
    let mut stale = row.clone();
    stale.detail = json!({});
    assert_eq!(memory.write_ledger_row_over(&stale, None).unwrap(), None);
    assert_eq!(memory.row(LedgerKind::Run, &a), row);
}

/// Two builders whose event cursors differ: one read the events before
/// the other wrote a run's row with a later event and its first step, then
/// reads the run's second step. Its fold, behind the stored row on the
/// events though ahead on the steps, is not written (the later event stays
/// the row's) and stays marked; its next pass, past that event, writes the
/// row with both steps.
#[test]
fn a_fold_behind_the_stored_rows_events_is_not_written_and_folds_again_once_caught_up() {
    let a = run_id(1);
    let mut log = Log::default();
    log.landed(1, &a, 0);
    let memory = Memory::of(&log);
    let target = session_of(&memory, &a, "s-1", 10);
    steps_in(
        &memory,
        &target,
        "s-1",
        &[Step::new(1, StepKind::ModelTurn, 0, 20_000)],
        ms(100),
    );
    log.add(Some(1), Some(&a), "review_finished", json!({}), 1001);
    let later = log.events.clone();
    let late_event = later.last().unwrap().id;
    let second = target.clone();
    memory
        .before_steps
        .replace(Some(Box::new(move |memory: &Memory| {
            memory.events.replace(later);
            let other = build_pass(
                memory,
                &mut Builder::default(),
                (ms(1002), BUILT),
                Retain::default(),
                None,
            );
            assert_eq!(other.failure, None);
            steps_in(
                memory,
                &second,
                "s-1",
                &[Step::new(2, StepKind::ToolCall, 20_000, 10_000)],
                ms(1002),
            );
        })));
    let mut builder = Builder::default();
    let behind = pass(&memory, &mut builder, 1003);
    assert_eq!(behind.failure, None);
    assert!(memory.before_steps.borrow().is_none(), "the other pass ran");
    assert!(builder.events < late_event);
    let held = memory.row(LedgerKind::Run, &a);
    assert_eq!(held.inputs_through.events, late_event);
    assert_eq!(held.records.last().map(|event| event.id), Some(late_event));
    assert_eq!(held.inputs_through.steps, Some(1));
    assert!(behind.backlog > 0, "the run stays marked");
    pass(&memory, &mut builder, 1004);
    let caught_up = memory.row(LedgerKind::Run, &a);
    assert_eq!(caught_up.inputs_through.events, late_event);
    assert_eq!(caught_up.records, held.records);
    assert_eq!(caught_up.inputs_through.steps, Some(2));
    assert_eq!(
        steps_of(&caught_up)["fold"]["counts"],
        json!({"model_turn": 1, "tool_call": 1})
    );
}

/// The steps of a target of no run (a planner's session) move the step
/// cursor and mark nothing: no run's or task's row is written or read for
/// them.
#[test]
fn the_steps_of_a_target_of_no_run_mark_no_run_or_task() {
    let a = run_id(1);
    let mut log = Log::default();
    log.landed(1, &a, 0);
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    pass(&memory, &mut builder, LATER);
    let (writes, read) = (memory.writes.get(), memory.read.get());
    let planner = StepTarget {
        kind: TargetKind::Planner,
        id: "planner-1".to_owned(),
    };
    memory.register_session(&planner, "p-1", ms(10)).unwrap();
    steps_in(
        &memory,
        &planner,
        "p-1",
        &[Step::new(1, StepKind::ModelTurn, 0, 20_000)],
        ms(20),
    );
    let built = pass(&memory, &mut builder, LATER + 1);
    assert_eq!(builder.steps, 1);
    assert_eq!(built.backlog, 0);
    assert_eq!((memory.writes.get(), memory.read.get()), (writes, read));
    let row = memory.row(LedgerKind::Run, &a);
    assert!(
        row.detail
            .get(crate::domain::ledger::steps::DETAIL_KEY)
            .is_none()
    );
}

/// A pass drops the records of the session and queue rows the store holds
/// settled, and once a later event reaches one (a late turn of a closed
/// session) gives it back its records from the row the store holds: the
/// rows are those of the fold of every event, and the builder keeps none
/// of the settled rows' events after.
#[test]
fn a_pass_gives_a_settled_row_a_later_event_reaches_its_dropped_records_back() {
    let mut log = Log::default();
    log.add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 7}),
        0,
    )
    .add(
        None,
        None,
        "session_closed",
        json!({"kind": "runtime_planner", "planner_id": 7, "opened_event_id": 1}),
        10,
    )
    .add(None, None, "mark_added", json!({"mark": "m"}), 20);
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    while pass(&memory, &mut builder, LATER).backlog > 0 {}
    // The next pass's due drops what the last one found settled.
    let built = pass(&memory, &mut builder, LATER);
    assert_eq!((built.kept.events(), built.kept.dropped_rows), (0, 2));

    log.add(
        None,
        None,
        "session_turns",
        json!({"kind": "runtime_planner", "planner_id": 7, "opened_event_id": 1}),
        LATER + 10,
    );
    memory.events.replace(log.events.clone());
    while pass(&memory, &mut builder, LATER + 20).backlog > 0 {}
    let whole = Inputs::new(&log.events, ms(LATER + 20), BUILT, true);
    let rows = memory.rows.borrow().clone();
    for ((kind, key), row) in &rows {
        let built = build(&target_of(*kind, key), &whole, &[]).unwrap();
        assert_eq!(built.records, row.records, "{kind:?} {key}");
        assert_eq!(built.ended_at, row.ended_at, "{kind:?} {key}");
        assert_eq!(built.detail, row.detail, "{kind:?} {key}");
    }
    let session = memory.row(LedgerKind::Session, "runtime_planner/7/session/1");
    assert_eq!(session.records.len(), 3);
    let later = 2 * LATER;
    while pass(&memory, &mut builder, later).backlog > 0 {}
    assert_eq!(pass(&memory, &mut builder, later).kept.events(), 0);
}

/// A queue of `settled` runtime planner sessions opened and closed, each
/// with a mark of the queue, one landing, and one session still open:
/// every row but the open session's is settled after the grace.
fn settled_sessions(settled: i64) -> Log {
    let mut log = Log::default();
    log.landed(1, &run_id(1), 0);
    for planner in 0..settled {
        let at = 100 + 30 * planner;
        let opened = log.last().as_i64() + 1;
        log.add(
            None,
            None,
            "session_opened",
            json!({"kind": "runtime_planner", "planner_id": planner}),
            at,
        )
        .add(
            None,
            None,
            "session_closed",
            json!({"kind": "runtime_planner", "planner_id": planner, "opened_event_id": opened}),
            at + 10,
        )
        .add(None, None, "mark_added", json!({"mark": planner}), at + 20);
    }
    log.add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": settled}),
        100 + 30 * settled,
    );
    log
}

/// A new builder (a supervisor started again) catches up from the cursors'
/// start over a store that holds the session and queue rows settled: after
/// its first pass, which reads every event, it keeps as many events with a
/// few settled rows as with more than that pass looks at
/// ([`CHECKS_PER_PASS`]), those of the rows not settled only, and the
/// passes leave the rows as the fold of every event makes them.
#[test]
fn a_new_builder_keeps_no_event_of_the_rows_the_store_holds_settled_after_its_first_pass() {
    let kept_after_restart = |settled: i64| {
        let log = settled_sessions(settled);
        let memory = Memory::of(&log);
        let mut before = Builder::default();
        while pass(&memory, &mut before, LATER).backlog > 0 {}
        let held = memory.rows.borrow().clone();
        assert!(
            held.iter()
                .filter(|((kind, _), _)| matches!(kind, LedgerKind::Session | LedgerKind::Queue))
                .filter(|(_, row)| row.is_final)
                .count()
                >= usize::try_from(settled).unwrap()
        );

        let mut builder = Builder::default();
        let first = pass(&memory, &mut builder, LATER);
        assert_eq!(first.failure, None);
        while pass(&memory, &mut builder, LATER).backlog > 0 {}
        let whole = Inputs::new(&log.events, ms(LATER), BUILT, true);
        let rows = memory.rows.borrow().clone();
        assert_eq!(rows, held, "the passes again leave the rows as they were");
        for ((kind, key), row) in &rows {
            let built = build(&target_of(*kind, key), &whole, &[]);
            assert_eq!(built.as_ref(), Some(row), "{kind:?} {key}");
        }
        first.kept
    };
    let more = i64::try_from(CHECKS_PER_PASS).unwrap();
    let (few, many) = (kept_after_restart(3), kept_after_restart(more));
    assert_eq!(few.events(), many.events());
    assert_eq!((few.sessions, few.queue), (1, 0));
    assert_eq!(
        many.dropped_rows,
        few.dropped_rows + 2 * usize::try_from(more - 3).unwrap()
    );
}

/// The log says what the builder keeps the first time, an hour after it
/// last did, and when its bytes changed by half of those logged or more.
#[test]
fn the_kept_is_logged_hourly_or_when_it_changes_by_half() {
    assert!(logs_kept(None, 0));
    let minute = Duration::from_secs(60);
    let last = |bytes| Some((minute, bytes));
    assert!(!logs_kept(last(1000), 1000));
    assert!(!logs_kept(last(1000), 1499));
    assert!(!logs_kept(last(1000), 501));
    assert!(logs_kept(last(1000), 1500));
    assert!(logs_kept(last(1000), 500));
    assert!(!logs_kept(Some((KEPT_LOG_INTERVAL - minute, 1000)), 1000));
    assert!(logs_kept(Some((KEPT_LOG_INTERVAL, 1000)), 1000));
    // An empty queue's is said once an hour.
    assert!(!logs_kept(last(0), 0));
    assert!(logs_kept(last(0), 1));
}

/// Passes drop a push that closed a pending push once the run it closed
/// and its task are settled, and a later event of the run folds them on
/// the push read back by id: the rows are those of the fold of every
/// event.
#[test]
fn a_pass_reads_back_a_dropped_closing_push_for_a_settled_run_folded_again() {
    let (a, b) = (run_id(1), run_id(2));
    let mut log = Log::default();
    log.worked(1, &a, 0)
        .integrated(1, &a, 700)
        .push_failed(1, &a, 910)
        .add(
            Some(1),
            None,
            "task_status_changed",
            json!({"from": "in_progress", "to": "completed"}),
            910,
        )
        .landed(2, &b, 2000);
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    while pass(&memory, &mut builder, LATER).backlog > 0 {}
    // The next pass's due drops what the last one found settled.
    assert_eq!(pass(&memory, &mut builder, LATER).kept.pushes, 0);

    log.add(Some(1), Some(&a), "note_added", json!({}), LATER + 10);
    memory.events.replace(log.events.clone());
    while pass(&memory, &mut builder, LATER + 20).backlog > 0 {}
    let whole = Inputs::new(&log.events, ms(LATER + 20), BUILT, true);
    for key in [
        (LedgerKind::Run, a.clone()),
        (LedgerKind::Task, "1".to_owned()),
    ] {
        let row = memory.row(key.0, &key.1);
        assert!(row.ended_at.is_some(), "{key:?}");
        assert_eq!(
            build(&target_of(key.0, &key.1), &whole, &[]).as_ref(),
            Some(&row),
            "{key:?}"
        );
    }
}

/// A task not ended has a row from its `task_created`, not settled: the
/// passes write the rows of the open follow-up drafts and of the task
/// whose planner asked with no run, and the window of the stored rows
/// gives `draft_flow`, `auto_repairs` and `planner_routes` as `stats`
/// makes them of every event. Once the task ends, a pass folds its row
/// again and settles it after the grace.
#[test]
fn a_task_not_ended_has_a_row_whose_records_give_the_drafts_and_its_asks() {
    use crate::domain::EventId;
    use crate::domain::ledger::window::{WindowQuery, select};
    use crate::domain::stats::{auto_repairs, drafts, planner_routes};
    use std::collections::HashMap;

    let a = run_id(1);
    let mut log = Log::default();
    log.add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 5, "route": "headless"}),
        0,
    )
    .landed(1, &a, 0)
    .add(
        Some(1),
        Some(&a),
        "follow_up_registered",
        json!({"task_id": 7}),
        940,
    )
    .add(
        Some(1),
        Some(&a),
        "follow_up_registered",
        json!({"task_id": 8}),
        940,
    )
    .add(
        Some(7),
        None,
        "task_created",
        json!({"status": "draft"}),
        940,
    )
    .add(
        Some(8),
        None,
        "task_created",
        json!({"status": "draft"}),
        940,
    )
    .add(
        Some(8),
        None,
        "task_status_changed",
        json!({"from": "draft", "to": "ready"}),
        950,
    )
    .add(
        Some(8),
        None,
        "ask_opened",
        json!({
            "kind": "planner_question",
            "asked_by": "planner:5",
            "reason_category": "scope",
            "ask_id": 1,
        }),
        960,
    );
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    drain(&memory, &mut builder, 1000);
    for task in ["7", "8"] {
        let row = memory.row(LedgerKind::Task, task);
        assert!(!row.is_final && row.ended_at.is_none(), "{task}: {row:?}");
    }

    let now = ms(1000);
    let rows: Vec<LedgerRow> = memory.rows.borrow().values().cloned().collect();
    let window = select(
        rows,
        &WindowQuery {
            now_ms: now,
            ..WindowQuery::default()
        },
    );
    let from_rows = window.events();
    let all = &log.events;
    let (after, upto) = (EventId::new(0), log.last());
    let flow = |events: &[RunEvent]| {
        drafts::draft_flow(events, &HashMap::new(), after, upto, now, |_| true)
    };
    let repairs = |events: &[RunEvent]| auto_repairs::auto_repairs(events, after, upto, |_| true);
    let routes =
        |events: &[RunEvent]| planner_routes::planner_routes(events, after, upto, |_| true, None);
    assert_eq!(flow(from_rows), flow(all));
    assert_eq!(repairs(from_rows), repairs(all));
    assert_eq!(routes(from_rows), routes(all));
    let flow = flow(all);
    assert_eq!(
        (flow.all.registered, flow.all.adopted, flow.all.backlog),
        (2, 1, 1)
    );
    assert_eq!(routes(all)["headless"].planner_questions, 1);
    assert_eq!(
        repairs(all)
            .by_day
            .values()
            .map(|day| day.asks_opened)
            .sum::<i64>(),
        1
    );

    log.add(
        Some(8),
        None,
        "task_status_changed",
        json!({"from": "ready", "to": "canceled"}),
        1100,
    );
    memory.events.replace(log.events.clone());
    pass(&memory, &mut builder, 1200);
    let ended = memory.row(LedgerKind::Task, "8");
    assert_eq!(ended.ended_at, Some(ms(1100)));
    assert!(!ended.is_final);
    pass(&memory, &mut builder, 1100 + SETTLE_GRACE_MS / 1000);
    assert!(memory.row(LedgerKind::Task, "8").is_final);
    assert!(!memory.row(LedgerKind::Task, "7").is_final);
}

/// More tasks ready under a slots full than a pass writes, each made
/// dirty again by an event of its supervisor before every pass, leave no
/// other mark waiting: every pass writes up to its bound, and the passes
/// go round the marks, so the rows past those tasks (a draft, a task whose
/// planner asked with no run, the planner's session) are written too.
#[test]
fn tasks_ready_marked_again_past_the_bound_each_pass_leave_no_other_row_unwritten() {
    let ready = i64::try_from(WRITES_PER_PASS).unwrap() + 50;
    let (draft, asked) = (ready + 1, ready + 2);
    let mut log = Log::default();
    log.add(
        None,
        None,
        "slots_full_started",
        json!({"supervisor": "s", "class": "heavy"}),
        0,
    );
    for task in (1..=ready).chain([asked]) {
        log.add(
            Some(task),
            None,
            "task_created",
            json!({"status": "draft"}),
            1,
        )
        .add(
            Some(task),
            None,
            "task_status_changed",
            json!({"from": "draft", "to": "ready"}),
            2,
        );
    }
    log.add(
        Some(draft),
        None,
        "task_created",
        json!({"status": "draft"}),
        3,
    )
    .add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 5, "route": "headless"}),
        3,
    )
    .add(
        Some(asked),
        None,
        "ask_opened",
        json!({
            "kind": "planner_question",
            "asked_by": "planner:5",
            "reason_category": "scope",
            "ask_id": 1,
        }),
        4,
    );
    let memory = Memory::default();
    let mut builder = Builder::default();
    for secs in 10..14 {
        log.add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "s"}),
            secs,
        );
        memory.events.replace(log.events.clone());
        let built = pass(&memory, &mut builder, secs);
        assert_eq!(built.written, WRITES_PER_PASS, "{secs}: {built:?}");
    }
    for task in 1..=asked {
        let rows = memory.rows.borrow();
        let row = rows.get(&(LedgerKind::Task, task.to_string()));
        assert!(row.is_some_and(|row| !row.is_final), "{task}");
    }
    let asks = memory.row(LedgerKind::Task, &asked.to_string());
    assert!(
        asks.records
            .iter()
            .any(|record| record.kind == EventKind::AskOpened),
        "{asks:?}"
    );
    assert_eq!(memory.count(LedgerKind::Session), 1);
}

/// A target whose write fails each pass (its stored row does not read)
/// stops no other: taken first as it holds R, the pass goes on past it to
/// the others, and keeps it in its turn of the round, so those are written
/// while it stays marked.
#[test]
fn a_target_that_fails_each_pass_leaves_the_others_written() {
    let mut log = Log::default();
    for task in 1..=3 {
        log.add(
            Some(task),
            None,
            "task_created",
            json!({"status": "draft"}),
            task,
        );
    }
    let memory = Memory::of(&log);
    memory
        .broken
        .replace(Some((LedgerKind::Task, "1".to_owned())));
    let mut builder = Builder::default();
    for secs in 10..13 {
        let built = pass(&memory, &mut builder, secs);
        assert!(built.failure.is_some(), "{secs}: {built:?}");
        assert_eq!(built.backlog, 1, "{secs}");
    }
    assert_eq!(memory.count(LedgerKind::Task), 2);
    assert!(
        !memory
            .rows
            .borrow()
            .contains_key(&(LedgerKind::Task, "1".to_owned()))
    );
}

/// `run` attached to the host at `secs` (its `environment_attached`); the
/// event's id.
fn attach(log: &mut Log, run: &str, secs: i64) -> i64 {
    let payload = crate::domain::environment::EnvironmentAttached {
        env_kind: crate::domain::environment::EnvKind::Host,
        node_id: HOST_NODE.to_owned(),
        image_digest: None,
        cpu_limit: Some(8.0),
        mem_limit: Some(1 << 34),
        arch: "aarch64".to_owned(),
    }
    .payload();
    log.add(Some(1), Some(run), "environment_attached", payload, secs);
    log.last().as_i64()
}

/// A window of `run`'s resources the supervisor measured from outside
/// under `attachment`, from `from` to `until` (seconds after [`T0`]).
fn measured(
    memory: &Memory,
    run: &str,
    attachment: i64,
    (from, until): (i64, i64),
    cpu_seconds: Option<f64>,
    last: bool,
) {
    memory
        .record_run_resource(&RunResource {
            id: 0,
            run_id: run.to_owned(),
            attachment,
            node_id: HOST_NODE.to_owned(),
            from_ms: ms(from),
            until_ms: ms(until),
            resources: Resources {
                cpu_seconds,
                memory_max_bytes: cpu_seconds.map(|_| 1 << 20),
                ..Resources::default()
            },
            last,
        })
        .unwrap();
}

/// What `row` holds of its resources under its detail.
fn resources_of(row: &LedgerRow) -> &Value {
    &row.detail[crate::domain::environment::DETAIL_KEY]
}

/// The CPU seconds `row` holds for its interval of `phase`.
fn cpu_in(row: &LedgerRow, phase: &str) -> Option<f64> {
    let index = row.intervals.iter().position(|i| i.phase == phase)?;
    resources_of(row)["intervals"][index]["cpu_seconds"].as_f64()
}

/// A run's row folds the windows the supervisor measured of it from
/// outside into its intervals, in the common units: the worker's 10 s to
/// 600 s takes the two windows in it, the last window (600 s to 900 s)
/// goes to the validation, the wait and the landing in their shares. The
/// row waits on the last window to settle: before it, it is written but
/// not final however long after its end and its token's revocation; the
/// last window marks it dirty and it settles. A window recorded after it
/// settled marks it dirty again: it is folded again with it, and settles
/// again.
#[test]
fn a_runs_resources_fold_by_interval_and_it_settles_after_their_last_window() {
    let a = run_id(1);
    let mut log = Log::default();
    log.landed(1, &a, 0);
    let attachment = attach(&mut log, &a, 0);
    let memory = Memory::of(&log);
    measured(&memory, &a, attachment, (10, 300), Some(30.0), false);
    measured(&memory, &a, attachment, (300, 600), Some(60.0), false);
    let mut builder = Builder::default();
    pass(&memory, &mut builder, LATER);
    let row = memory.row(LedgerKind::Run, &a);
    assert!(row.ended_at.is_some());
    assert!(!row.is_final, "it waits on the last window");
    assert_eq!(row.detail["resources_pending"], json!(true));
    assert_eq!(row.inputs_through.resources, Some(2));
    assert_eq!(resources_of(&row)["source"], "outside");
    assert_eq!(resources_of(&row)["total"]["cpu_seconds"], json!(90.0));
    assert_eq!(cpu_in(&row, "worker"), Some(90.0));
    assert_eq!(cpu_in(&row, "landing"), None);
    // Nothing new: the row waits, nothing is read again.
    assert_eq!(pass(&memory, &mut builder, LATER + 1).written, 0);

    measured(&memory, &a, attachment, (600, 900), Some(30.0), true);
    assert_eq!(pass(&memory, &mut builder, LATER + 2).written, 1);
    let row = memory.row(LedgerKind::Run, &a);
    assert!(row.is_final);
    assert!(row.detail.get("resources_pending").is_none());
    assert_eq!(row.inputs_through.resources, Some(3));
    assert_eq!(resources_of(&row)["total"]["cpu_seconds"], json!(120.0));
    let validating = cpu_in(&row, "validating").unwrap();
    let landing = cpu_in(&row, "landing").unwrap();
    assert!((validating - 6.0).abs() < 1e-9, "{validating}");
    assert!((landing - 20.0).abs() < 1e-9, "{landing}");
    assert_eq!(
        resources_of(&row)["intervals"][1]["memory_max_bytes"],
        json!(1 << 20)
    );

    // Late, after it settled: folded again.
    measured(&memory, &a, attachment, (900, 960), Some(3.0), false);
    assert_eq!(pass(&memory, &mut builder, LATER + 3).written, 1);
    let row = memory.row(LedgerKind::Run, &a);
    assert_eq!(row.inputs_through.resources, Some(4));
    assert_eq!(resources_of(&row)["total"]["cpu_seconds"], json!(123.0));
    assert!(row.is_final);
    assert_eq!(pass(&memory, &mut builder, LATER + 4).written, 0);
}

/// A run attached to an environment that measured nothing of it from
/// outside (its windows hold no value) folds what its executor measured
/// from inside, placed from when its session was registered; a sample
/// from inside that came in after the row was written marks it dirty. A
/// run never attached (one from before the record) waits on no window.
#[test]
fn a_run_measured_from_inside_only_folds_its_executors_samples() {
    let a = run_id(1);
    let mut log = Log::default();
    log.landed(1, &a, 0);
    let attachment = attach(&mut log, &a, 0);
    let memory = Memory::of(&log);
    let target = session_of(&memory, &a, "s-1", 10);
    let mut sample = ResourceSample::new(1, 0, 100_000);
    sample.cpu_seconds = Some(40.0);
    sample.throttled_seconds = Some(2.0);
    memory
        .record_resources(&target, "s-1", &[sample], ms(200))
        .unwrap();
    measured(&memory, &a, attachment, (10, 600), None, false);
    measured(&memory, &a, attachment, (600, 930), None, true);
    let mut builder = Builder::default();
    pass(&memory, &mut builder, LATER);
    let row = memory.row(LedgerKind::Run, &a);
    assert!(row.is_final);
    assert_eq!(resources_of(&row)["source"], "inside");
    assert_eq!(cpu_in(&row, "worker"), Some(40.0));
    assert_eq!(resources_of(&row)["total"]["throttled_seconds"], json!(2.0));
    assert_eq!(row.inputs_through.inside_resources, Some(1));

    let mut late = ResourceSample::new(2, 100_000, 50_000);
    late.cpu_seconds = Some(5.0);
    memory
        .record_resources(&target, "s-1", &[late], ms(LATER))
        .unwrap();
    assert_eq!(pass(&memory, &mut builder, LATER + 1).written, 1);
    let row = memory.row(LedgerKind::Run, &a);
    assert_eq!(row.inputs_through.inside_resources, Some(2));
    assert_eq!(resources_of(&row)["total"]["cpu_seconds"], json!(45.0));

    // The retention removes the first sample; a later one folds onto what
    // the row counted, the removed one's 40 s kept, each counted once.
    memory
        .inside
        .borrow_mut()
        .retain(|sample| sample.sample.seq != 1);
    let mut later = ResourceSample::new(3, 150_000, 10_000);
    later.cpu_seconds = Some(1.0);
    memory
        .record_resources(&target, "s-1", &[later], ms(LATER + 1))
        .unwrap();
    assert_eq!(pass(&memory, &mut builder, LATER + 2).written, 1);
    let row = memory.row(LedgerKind::Run, &a);
    assert_eq!(row.inputs_through.inside_resources, Some(3));
    assert_eq!(resources_of(&row)["total"]["cpu_seconds"], json!(46.0));
    assert_eq!(resources_of(&row)["total"]["throttled_seconds"], json!(2.0));
    assert_eq!(cpu_in(&row, "worker"), Some(46.0));
    assert_eq!(resources_of(&row)["inside"].as_array().unwrap().len(), 3);

    let b = run_id(2);
    let mut other = Log::default();
    other.landed(2, &b, 0);
    let none = Memory::of(&other);
    pass(&none, &mut Builder::default(), LATER);
    let row = none.row(LedgerKind::Run, &b);
    assert!(row.is_final);
    assert!(row.detail.get("resources").is_none());
    assert_eq!(row.inputs_through.resources, None);
}

mod frozen;
mod oldest_first;
mod pages;
mod pushes_mixed;
mod queue_waits;
mod start;
mod stream_waits;
mod sweep_reads;
mod waits;

/// The source `row` holds for its interval of `phase`.
fn source_in(row: &LedgerRow, phase: &str) -> Value {
    let index = row.intervals.iter().position(|i| i.phase == phase).unwrap();
    resources_of(row)["sources"][index].clone()
}

/// A run the supervisor measured from outside only after its worker (the
/// worker's window holds no value) takes its executor's samples for the
/// worker and the values from outside for the rest, each interval saying
/// which it took and the row that it is mixed. The same sample sent again
/// adds none and leaves the row; a sample of a planner's session marks no
/// run or task.
#[test]
fn a_run_takes_its_executors_samples_only_for_the_intervals_measured_from_outside_with_nothing() {
    let a = run_id(1);
    let mut log = Log::default();
    log.landed(1, &a, 0);
    let attachment = attach(&mut log, &a, 0);
    let memory = Memory::of(&log);
    let target = session_of(&memory, &a, "s-1", 10);
    let mut sample = ResourceSample::new(1, 0, 590_000);
    sample.cpu_seconds = Some(40.0);
    memory
        .record_resources(&target, "s-1", std::slice::from_ref(&sample), ms(200))
        .unwrap();
    measured(&memory, &a, attachment, (10, 600), None, false);
    measured(&memory, &a, attachment, (600, 930), Some(33.0), true);
    let mut builder = Builder::default();
    pass(&memory, &mut builder, LATER);
    let row = memory.row(LedgerKind::Run, &a);
    assert!(row.is_final);
    assert_eq!(resources_of(&row)["source"], "mixed");
    assert_eq!(source_in(&row, "worker"), "inside");
    assert_eq!(cpu_in(&row, "worker"), Some(40.0));
    assert_eq!(source_in(&row, "landing"), "outside");
    assert!(cpu_in(&row, "landing").unwrap() > 0.0);
    assert_eq!(resources_of(&row)["total"]["cpu_seconds"], json!(73.0));

    // Sent again: the store adds none, nothing is written.
    let again = memory
        .record_resources(&target, "s-1", &[sample], ms(LATER))
        .unwrap();
    assert_eq!((again.added, again.duplicates), (0, 1));
    assert_eq!(pass(&memory, &mut builder, LATER + 1).written, 0);
    assert_eq!(memory.row(LedgerKind::Run, &a), row);

    // A planner's sample: no run and no task row changes.
    let rows = memory.rows.borrow().clone();
    let planner = StepTarget {
        kind: TargetKind::Planner,
        id: "planner-1".to_owned(),
    };
    memory.register_session(&planner, "p-1", ms(10)).unwrap();
    let mut theirs = ResourceSample::new(1, 0, 10_000);
    theirs.cpu_seconds = Some(9.0);
    memory
        .record_resources(&planner, "p-1", &[theirs], ms(LATER + 1))
        .unwrap();
    let built = pass(&memory, &mut builder, LATER + 2);
    assert_eq!(built.written, 0);
    assert_eq!(builder.inside_resources, 2);
    assert_eq!(*memory.rows.borrow(), rows);
}

/// A run's row an older fold settled (its resources from outside for the
/// whole run, no interval's source and no session's start kept) is folded
/// again by a builder that starts again over the same inputs, none new:
/// its worker takes the executor's sample, each interval says where its
/// resources come from and the row keeps the session's start, so once the
/// retention removed the sample and the session's owner a late sample is
/// still placed.
#[test]
fn a_row_of_an_older_fold_keeps_each_intervals_resources_their_sources_and_starts_on_no_new_input()
{
    let a = run_id(1);
    let mut log = Log::default();
    log.landed(1, &a, 0);
    let attachment = attach(&mut log, &a, 0);
    let memory = Memory::of(&log);
    let target = session_of(&memory, &a, "s-1", 10);
    let mut sample = ResourceSample::new(1, 0, 590_000);
    sample.cpu_seconds = Some(40.0);
    memory
        .record_resources(&target, "s-1", &[sample], ms(200))
        .unwrap();
    measured(&memory, &a, attachment, (10, 600), None, false);
    measured(&memory, &a, attachment, (600, 930), Some(33.0), true);
    pass(&memory, &mut Builder::default(), LATER);
    let current = memory.row(LedgerKind::Run, &a);
    assert!(current.is_final);

    // As the older fold left it.
    let worker = current
        .intervals
        .iter()
        .position(|i| i.phase == "worker")
        .unwrap();
    {
        let mut rows = memory.rows.borrow_mut();
        let old = rows.get_mut(&(LedgerKind::Run, a.clone())).unwrap();
        old.ledger_v = LEDGER_V - 1;
        let resources = &mut old.detail[crate::domain::environment::DETAIL_KEY];
        resources["source"] = json!("outside");
        resources["intervals"][worker] = json!({});
        resources["total"]["cpu_seconds"] = json!(33.0);
        let fold = resources.as_object_mut().unwrap();
        fold.remove("sources");
        fold.remove("starts");
    }

    let mut builder = Builder::default();
    assert_eq!(pass(&memory, &mut builder, LATER + 1).written, 1);
    let row = memory.row(LedgerKind::Run, &a);
    assert_eq!(row.ledger_v, LEDGER_V);
    assert!(row.is_final);
    assert_eq!(resources_of(&row), resources_of(&current));
    assert_eq!(resources_of(&row)["source"], "mixed");
    assert_eq!(source_in(&row, "worker"), "inside");
    assert_eq!(cpu_in(&row, "worker"), Some(40.0));
    assert_eq!(resources_of(&row)["starts"]["s-1"], json!(ms(10)));

    // The retention removes the sample and the session's owner; a late
    // sample is placed by the start the row kept.
    memory.remove_steps(ms(LATER)).unwrap();
    assert!(memory.owners.borrow().is_empty());
    assert!(memory.inside.borrow().is_empty());
    let mut late = ResourceSample::new(2, 100_000, 10_000);
    late.cpu_seconds = Some(5.0);
    memory
        .record_resources(&target, "s-1", &[late], ms(LATER + 2))
        .unwrap();
    assert_eq!(pass(&memory, &mut builder, LATER + 2).written, 1);
    let row = memory.row(LedgerKind::Run, &a);
    assert_eq!(cpu_in(&row, "worker"), Some(45.0));
    assert_eq!(source_in(&row, "worker"), "inside");
    assert_eq!(resources_of(&row)["total"]["cpu_seconds"], json!(78.0));
}

/// A builder that goes on from a position with the session book made again
/// from the rows the store holds ([`session_book`]), what else it keeps
/// carried from the passes up to there, writes the rows of the fold of
/// every event: the planner's next session its next round, a late record
/// of a closed session to that session's settled row, the records of none
/// to the one row of round 0, a job's model from a close before the
/// position or after it ([`sessions_across`]). A book of the events after
/// the position alone writes others.
#[test]
fn a_builder_going_on_from_the_session_rows_held_writes_the_rows_of_every_event() {
    let (log, position) = sessions_across();
    let rows_going_on = |book: Option<SessionBook>| {
        let memory = Memory::default();
        memory.events.replace(log.events[..position].to_vec());
        let mut builder = Builder::default();
        drain(&memory, &mut builder, LATER);
        assert!(memory.rows.borrow().values().any(|row| row.is_final));
        let made = session_book(&memory).unwrap();
        assert_eq!(&made, builder.kept().session_rows());
        builder.restore_session_rows(book.unwrap_or(made));
        memory.events.replace(log.events.clone());
        passes_to_drain(&memory, &mut builder, LATER + 1);
        memory.rows.into_inner()
    };
    let rows = rows_going_on(None);
    let whole = crate::domain::ledger::build(&log.events, ms(LATER + 1), BUILT);
    let sessions: Vec<&LedgerRow> = whole
        .iter()
        .filter(|row| row.kind == LedgerKind::Session)
        .collect();
    assert!(sessions.len() >= 7);
    for expected in &sessions {
        let stored = &rows[&(expected.kind, expected.key.clone())];
        let mut expected = (*expected).clone();
        expected.is_final = stored.is_final;
        assert_eq!(stored, &expected, "{}", expected.key);
    }
    assert_eq!(
        rows.keys()
            .filter(|(kind, _)| *kind == LedgerKind::Session)
            .count(),
        sessions.len()
    );

    let after_alone = rows_going_on(Some(SessionBook::default()));
    assert_ne!(after_alone, rows);
}

/// The session book is made again from the rows the store holds alone:
/// with a few settled sessions or more than a pass looks at, it reads no
/// event of the EventStore, and it is the book of every event.
#[test]
fn the_session_book_made_again_reads_no_event_however_many_sessions_settled() {
    let read_making_it = |settled: i64| {
        let log = settled_sessions(settled);
        let memory = Memory::of(&log);
        let mut builder = Builder::default();
        while pass(&memory, &mut builder, LATER).backlog > 0 {}
        assert!(
            memory
                .rows
                .borrow()
                .values()
                .filter(|row| row.kind == LedgerKind::Session && row.is_final)
                .count()
                >= usize::try_from(settled).unwrap()
        );
        memory.read.set(0);
        let book = session_book(&memory).unwrap();
        assert_eq!(&book, builder.kept().session_rows());
        (memory.read.get(), memory.read_all.get(), book.row_count())
    };
    let more = i64::try_from(CHECKS_PER_PASS).unwrap();
    let (few, many) = (read_making_it(3), read_making_it(more));
    assert_eq!((few.0, few.1), (0, 0));
    assert_eq!((many.0, many.1), (0, 0));
    assert!(many.2 > few.2);
}

/// A builder that goes on from a position with the queue book made again
/// from the rows the store holds ([`queue_book`]), what else it keeps
/// carried from the passes up to there and none of the queue's events,
/// writes the rows of the fold of every event ([`queue_across`]): a
/// finding's next record, an alert's end and a stream's next baseline to
/// the rows open across the position, an event timed before a landing's
/// end to that landing's row, the lives of the supervisors of a slots full
/// and of a hold their stop ended to those, a claim to the deferral open.
/// A book of the events after the position alone writes others.
#[test]
fn a_builder_going_on_from_the_queue_rows_held_writes_the_rows_of_every_event() {
    let (log, position) = queue_across();
    let rows_going_on = |book: Option<QueueBook>| {
        let memory = Memory::default();
        memory.events.replace(log.events[..position].to_vec());
        let mut builder = Builder::default();
        drain(&memory, &mut builder, LATER);
        assert!(
            memory
                .rows
                .borrow()
                .iter()
                .any(|((kind, _), row)| *kind == LedgerKind::Queue && row.is_final)
        );
        let made = queue_book(&memory, &builder).unwrap();
        assert!(builder.restore_queue_rows(book.unwrap_or(made)));
        memory.events.replace(log.events.clone());
        passes_to_drain(&memory, &mut builder, LATER + 1);
        memory.rows.into_inner()
    };
    let rows = rows_going_on(None);
    let whole = crate::domain::ledger::build(&log.events, ms(LATER + 1), BUILT);
    let queue: Vec<&LedgerRow> = whole
        .iter()
        .filter(|row| row.kind == LedgerKind::Queue)
        .collect();
    assert!(queue.len() >= 10);
    for expected in &queue {
        let stored = &rows[&(expected.kind, expected.key.clone())];
        let mut expected = (*expected).clone();
        expected.is_final = stored.is_final;
        assert_eq!(stored, &expected, "{}", expected.key);
    }
    assert_eq!(
        rows.keys()
            .filter(|(kind, _)| *kind == LedgerKind::Queue)
            .count(),
        queue.len()
    );

    let after_alone = rows_going_on(Some(QueueBook::default()));
    assert_ne!(after_alone, rows);
}

/// The queue book is made again from the rows the store holds alone: with
/// a few settled queue rows or more than a pass looks at, it reads no
/// event of the EventStore, and it holds as many rows: the settled marks
/// of the queue no later event reaches are not among them, the landing is.
#[test]
fn the_queue_book_made_again_reads_no_event_however_many_queue_rows_settled() {
    let read_making_it = |settled: i64| {
        let log = settled_sessions(settled);
        let memory = Memory::of(&log);
        let mut builder = Builder::default();
        while pass(&memory, &mut builder, LATER).backlog > 0 {}
        let held = memory
            .rows
            .borrow()
            .values()
            .filter(|row| row.kind == LedgerKind::Queue && row.is_final)
            .count();
        assert!(held > usize::try_from(settled).unwrap());
        memory.read.set(0);
        let book = queue_book(&memory, &builder).unwrap();
        (
            memory.read.get(),
            memory.read_all.get(),
            held,
            book.row_count(),
        )
    };
    let more = i64::try_from(CHECKS_PER_PASS).unwrap();
    let (few, many) = (read_making_it(3), read_making_it(more));
    assert_eq!((few.0, few.1), (0, 0));
    assert_eq!((many.0, many.1), (0, 0));
    assert!(many.2 > few.2);
    assert_eq!((few.3, many.3), (1, 1));
}

/// A builder with a row not written yet does not take a queue book made
/// again from the store's rows: they miss events it read.
#[test]
fn a_builder_with_rows_not_written_keeps_its_queue_book() {
    let (log, position) = queue_across();
    let memory = Memory::default();
    memory.events.replace(log.events[..position].to_vec());
    let mut builder = Builder::default();
    builder.mark_events(&log.events[..position]);
    let kept = builder.kept().clone();
    assert!(!builder.restore_queue_rows(QueueBook::default()));
    assert_eq!(builder.kept(), &kept);
}

/// A builder that goes on from a position with the supervisors' lives and
/// the waits before a claim made again from the queue rows the store holds
/// ([`pre_claim_books`]), what else it keeps carried from the passes up to
/// there, writes the rows of the fold of every event ([`waits_across`]):
/// supervisors alive across the position whose silences end their slots
/// full after it (one with an event after it, one with none), the event of
/// a stopped supervisor and of a silent one after the position that open
/// again the slots full their lives ended (and
/// reach the hold the stop ended), the holds and the deferral open across
/// it, the overlays of a task ready across it, which read them all and a
/// slots full freed over its time ready, and one freed before the task
/// whose row settles after the position. A book of the events after the
/// position alone writes others.
#[test]
fn a_builder_going_on_from_the_wait_rows_held_writes_the_rows_of_every_event() {
    use crate::domain::ledger::testkit::waits_across;
    let (log, position, at) = waits_across();
    // Before the grace of d's slots full, freed before the task, is over.
    let at = at - 100;
    let rows_going_on = |books: Option<(Lives, PreClaimBook)>| {
        let memory = Memory::default();
        memory.events.replace(log.events[..position].to_vec());
        let mut builder = Builder::default();
        drain(&memory, &mut builder, at);
        assert!(!memory.row(LedgerKind::Queue, "1").is_final);
        let made = pre_claim_books(&memory, ms(at)).unwrap();
        assert_eq!(made.0.supervisors(), 4);
        let (lives, book) = books.unwrap_or(made);
        builder.restore_pre_claim(lives, book);
        memory.events.replace(log.events.clone());
        drain(&memory, &mut builder, 1_500);
        passes_to_drain(&memory, &mut builder, LATER + 1);
        memory.rows.into_inner()
    };
    let rows = rows_going_on(None);
    let whole = crate::domain::ledger::build(&log.events, ms(LATER + 1), BUILT);
    assert!(whole.iter().any(|row| row.kind == LedgerKind::Task));
    for expected in &whole {
        let stored = &rows[&(expected.kind, expected.key.clone())];
        let mut expected = expected.clone();
        // Every queue row has ended and settled by then, as passes from
        // the first event settle them.
        assert_eq!(
            stored.is_final,
            expected.kind == LedgerKind::Queue,
            "{:?} {}",
            expected.kind,
            expected.key
        );
        expected.is_final = stored.is_final;
        assert_eq!(stored, &expected, "{:?} {}", expected.kind, expected.key);
    }
    assert_eq!(rows.len(), whole.len());

    let after_alone = rows_going_on(Some((Lives::default(), PreClaimBook::default())));
    assert_ne!(after_alone, rows);
}

/// A queue of `settled` slots full of one supervisor, each freed long
/// before the pass, and a supervisor alive with a slots full open: the time
/// (seconds) of the pass after which only the open one is not settled.
fn settled_waits(settled: i64) -> (Log, i64) {
    let mut log = Log::default();
    let heavy = |supervisor: &str| json!({"supervisor": supervisor, "class": "heavy"});
    for k in 0..settled {
        log.add(None, None, "slots_full_started", heavy("old"), 10 * k)
            .add(None, None, "slots_full_ended", heavy("old"), 10 * k + 5);
    }
    let alive = 10 * settled + SETTLE_GRACE_MS / 1000 + 100;
    log.add(None, None, "slots_full_started", heavy("a"), alive)
        .add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "a"}),
            alive + 10,
        );
    (log, alive + 20)
}

/// The lives and the waits before a claim are made again from the rows
/// the store holds alone but the last event of the life that goes on: with
/// a few waits settled or many, they read the same one event of the
/// EventStore and hold as many waits and lives, while the book of every
/// event grows with them.
#[test]
fn the_wait_books_made_again_read_as_many_events_however_many_waits_settled() {
    let read_making_them = |settled: i64| {
        let (log, at) = settled_waits(settled);
        let memory = Memory::of(&log);
        let mut builder = Builder::default();
        while pass(&memory, &mut builder, at).backlog > 0 {}
        let settled_rows = memory
            .rows
            .borrow()
            .values()
            .filter(|row| {
                row.is_final && row.subject.as_ref().is_some_and(|s| s.kind == "slots_full")
            })
            .count();
        assert_eq!(settled_rows, usize::try_from(settled).unwrap());
        memory.read.set(0);
        let (lives, book) = pre_claim_books(&memory, ms(at)).unwrap();
        (
            (memory.read.get(), memory.read_all.get()),
            (book.waits(), lives.supervisors()),
            builder.kept().pre_claim().waits(),
        )
    };
    let (few, many) = (read_making_them(3), read_making_them(300));
    assert_eq!(few.0, (1, 0));
    assert_eq!(many.0, few.0);
    assert_eq!(many.1, few.1);
    assert_eq!(few.1, (1, 1));
    assert!(many.2 > few.2);
}

/// The books made again at a position from the queue rows the store
/// holds written later ([`books_at`]) read, besides the rows' heads and the
/// stops, as many times and as many events however many rows settled
/// before: a history and the same with more settled rows of their own (a
/// mark, a deferral a claim ended) between an attempt's end and an event
/// timed before it and in a supervisor's silence, cut at the same events
/// of each. Each kind of read is made. At the position the rows were
/// written at, the books are those of the rows as written.
#[test]
fn the_reads_of_the_books_cut_to_a_position_do_not_grow_with_the_rows_settled() {
    use crate::domain::ledger::testkit::{base_ats, cut_log};
    let reads_cutting = |fill: usize| {
        let log = cut_log(fill);
        let secs = (crate::domain::stats::timestamp_millis(&log.events.last().unwrap().created_at)
            .unwrap()
            / 1000)
            - T0
            + 1;
        let memory = Memory::of(&log);
        let mut builder = Builder::default();
        drain(&memory, &mut builder, secs);
        // At the position the rows were written at, the books of the rows
        // as written.
        let p = log.last();
        let (queue, lives, book) = books_at(&memory, p, ms(secs)).unwrap();
        let (as_lives, as_book) = pre_claim_books(&memory, ms(secs)).unwrap();
        assert_eq!((&lives, &book), (&as_lives, &as_book));
        let heads = memory.ledger_queue_heads().unwrap();
        assert_eq!(queue, QueueBook::from_heads(&heads, &as_book, p));
        let mut total = CutReads::default();
        let mut most = 0;
        for at in base_ats(&log) {
            memory.cut_reads.set(CutReads::default());
            books_at(&memory, at, ms(secs)).unwrap();
            let reads = memory.cut_reads.get();
            most = most.max(reads.rows + reads.runs + reads.naming + reads.since_at);
            total.rows += reads.rows;
            total.row_events += reads.row_events;
            total.runs += reads.runs;
            total.run_events += reads.run_events;
            total.naming += reads.naming;
            total.since_at += reads.since_at;
        }
        (total, most)
    };
    let (small, large) = (reads_cutting(1), reads_cutting(50));
    assert_eq!(small, large);
    let (total, _) = small;
    assert!(total.rows > 0 && total.row_events > 0, "{total:?}");
    assert!(total.runs > 0 && total.run_events > 0, "{total:?}");
    assert!(total.naming > 0 && total.since_at > 0, "{total:?}");
}

/// The rows `rows` and the events `events` a queue service holds, as the
/// supervisor's queue reads them through it ([`SupervisorQueues`]): it
/// answers the reads the waits and the lives are made again by as the
/// store's own [`LedgerStore`] gives them, and what else a service answers
/// to the events between two ids; any other operation (the rows whole
/// among them) fails the test. What each answer held is kept, by the
/// operation's name: how many rows, stops or events (an aggregate as one).
struct WaitRowsService {
    rows: Vec<LedgerRow>,
    events: Vec<RunEvent>,
    received: std::sync::Mutex<Vec<(String, usize)>>,
    callers: std::sync::Mutex<Vec<(String, Option<&'static str>)>>,
    /// The bytes of a page of the push records' answer past which it is
    /// refused as too large to answer.
    largest: usize,
    /// Of each page of the push records asked, the run rows the store read
    /// for it, counted as it read them, and whether the page was refused
    /// as past `largest` (its rows read all the same).
    push_pages: std::sync::Mutex<Vec<(usize, bool)>>,
}

/// The rows of a store, read through [`LedgerStore`]'s own readings of
/// them, counting the rows each read gives.
struct HeldRows<'a>(&'a [LedgerRow], &'a std::sync::atomic::AtomicUsize);

impl HeldRows<'_> {
    fn count(&self, rows: usize) {
        self.1.fetch_add(rows, std::sync::atomic::Ordering::Relaxed);
    }
}

impl LedgerStore for HeldRows<'_> {
    fn ledger_run_events(&self, _: &RunId) -> Result<Vec<RunEvent>> {
        unreachable!("no run's events here")
    }
    fn ledger_legacy_runs_without_row(&self) -> Result<Vec<RunId>> {
        unreachable!("no claims here")
    }
    fn ledger_runs_claimed(&self, _: TaskId) -> Result<Vec<RunId>> {
        unreachable!("no claims here")
    }
    fn ledger_runs_past_their_rows(&self, _: EventId) -> Result<Vec<RunId>> {
        unreachable!("no run's events here")
    }
    fn last_event_naming_supervisor(&self, _: &str, _: EventId) -> Result<Option<RunEvent>> {
        unreachable!("no event naming a supervisor here")
    }
    fn write_ledger_row(&self, _: &LedgerRow) -> Result<bool> {
        unreachable!("no write here")
    }
    fn write_ledger_row_over(&self, _: &LedgerRow, _: Option<&LedgerRow>) -> Result<Option<bool>> {
        unreachable!("no write here")
    }
    fn write_session_rows_over(
        &self,
        _: &[(LedgerRow, Option<LedgerRow>)],
        _: Option<&SessionsWritten>,
    ) -> Result<bool> {
        unreachable!("no write here")
    }
    fn sessions_written(&self) -> Result<Option<SessionsWritten>> {
        unreachable!("no mark here")
    }
    fn ledger_row(&self, kind: LedgerKind, key: &str) -> Result<Option<LedgerRow>> {
        let row = self
            .0
            .iter()
            .find(|row| row.kind == kind && row.key == key)
            .cloned();
        self.count(usize::from(row.is_some()));
        Ok(row)
    }
    fn ledger_rows(&self, kind: LedgerKind) -> Result<Vec<LedgerRow>> {
        let rows: Vec<LedgerRow> = self
            .0
            .iter()
            .filter(|row| row.kind == kind)
            .cloned()
            .collect();
        self.count(rows.len());
        Ok(rows)
    }
    fn ledger_rows_after(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LedgerRow>> {
        let mut rows: Vec<LedgerRow> = self
            .0
            .iter()
            .filter(|row| row.kind == kind)
            .filter(|row| after.is_none_or(|after| row.key.as_str() > after))
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.key.cmp(&b.key));
        rows.truncate(limit);
        self.count(rows.len());
        Ok(rows)
    }
    fn remove_node_rows(&self, _: i64) -> Result<usize> {
        unreachable!("no retention here")
    }
}

impl WaitRowsService {
    fn of(memory: &Memory) -> std::sync::Arc<Self> {
        Self::paged(memory, usize::MAX)
    }

    /// [`Self::of`], refusing a page of the push records whose answer is
    /// past `largest` bytes.
    fn paged(memory: &Memory, largest: usize) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            rows: memory.rows.borrow().values().cloned().collect(),
            events: memory.events.borrow().clone(),
            received: std::sync::Mutex::default(),
            callers: std::sync::Mutex::default(),
            largest,
            push_pages: std::sync::Mutex::default(),
        })
    }

    fn answer(&self, op: &str, args: &[serde_json::Value]) -> Result<(serde_json::Value, usize)> {
        let arg = |n: usize| args[n].clone();
        let read_here = std::sync::atomic::AtomicUsize::new(0);
        let store = HeldRows(&self.rows, &read_here);
        Ok(match op {
            "ledger_tasks_open_since" => {
                (serde_json::to_value(store.ledger_tasks_open_since()?)?, 1)
            }
            "ledger_wait_rows" => {
                let after: Option<String> = serde_json::from_value(arg(1))?;
                let read = store.ledger_wait_rows_after(
                    serde_json::from_value(arg(0))?,
                    after.as_deref(),
                    serde_json::from_value(arg(2))?,
                )?;
                (serde_json::to_value(&read)?, read.len())
            }
            "ledger_queue_heads_at" => {
                let after: Option<String> = serde_json::from_value(arg(2))?;
                let read = store.ledger_queue_heads_at_after(
                    serde_json::from_value(arg(0))?,
                    serde_json::from_value(arg(1))?,
                    after.as_deref(),
                    serde_json::from_value(arg(3))?,
                )?;
                (serde_json::to_value(&read)?, read.len())
            }
            "ledger_supervisor_stops" => {
                let after: Option<(String, i64)> = serde_json::from_value(arg(0))?;
                let read = store.ledger_supervisor_stops_after(
                    after.as_ref().map(|(key, index)| (key.as_str(), *index)),
                    serde_json::from_value(arg(1))?,
                )?;
                (serde_json::to_value(&read)?, read.len())
            }
            "events_between" => {
                let (after, upto): (EventId, EventId) = (
                    serde_json::from_value(arg(0))?,
                    serde_json::from_value(arg(1))?,
                );
                let page: usize = serde_json::from_value(arg(3))?;
                let read: Vec<&RunEvent> = self
                    .events
                    .iter()
                    .filter(|event| event.id > after && event.id <= upto)
                    .take(page)
                    .collect();
                (serde_json::to_value(&read)?, read.len())
            }
            "ledger_push_records" | "ledger_runless_pushes" => {
                let after: Option<EventId> = serde_json::from_value(arg(0))?;
                let page: usize = serde_json::from_value(arg(1))?;
                let read: Vec<RunEvent> = if op == "ledger_push_records" {
                    store.ledger_push_records()?
                } else {
                    store.ledger_runless_pushes()?
                }
                .into_iter()
                .filter(|push| after.is_none_or(|after| push.id > after))
                .take(page)
                .collect();
                (serde_json::to_value(&read)?, read.len())
            }
            "ledger_push_records_page" => {
                let page = store.ledger_push_records_page(&serde_json::from_value(arg(0))?)?;
                let answer = serde_json::to_value(&page)?;
                let refused = serde_json::to_vec(&answer)?.len() > self.largest;
                self.push_pages.lock().unwrap().push((
                    read_here.load(std::sync::atomic::Ordering::Relaxed),
                    refused,
                ));
                if refused {
                    anyhow::bail!("the page is past the answer's limit (response_too_large)");
                }
                (answer, page.records.len())
            }
            "ledger_heads_open_at" => {
                let kind: LedgerKind = serde_json::from_value(arg(0))?;
                let after: Option<String> = serde_json::from_value(arg(3))?;
                let page: usize = serde_json::from_value(arg(4))?;
                let read: Vec<crate::domain::ledger::LedgerHead> = store
                    .ledger_heads_open_at(
                        serde_json::from_value(arg(1))?,
                        serde_json::from_value(arg(2))?,
                    )?
                    .into_iter()
                    .filter(|head| head.kind == kind)
                    .filter(|head| after.as_ref().is_none_or(|after| head.key > *after))
                    .take(page)
                    .collect();
                (serde_json::to_value(&read)?, read.len())
            }
            "ledger_runs_without_row" => {
                let read = store.ledger_runs_without_row()?;
                (serde_json::to_value(&read)?, read.len())
            }
            "ledger_legacy_runs_without_row" | "ledger_runs_past_their_rows" => {
                let memory = Memory::default();
                memory.events.replace(self.events.clone());
                memory.rows.replace(
                    self.rows
                        .iter()
                        .map(|row| ((row.kind, row.key.clone()), row.clone()))
                        .collect(),
                );
                let read = if op == "ledger_runs_past_their_rows" {
                    memory.ledger_runs_past_their_rows(serde_json::from_value(arg(0))?)?
                } else {
                    memory.ledger_legacy_runs_without_row()?
                };
                (serde_json::to_value(&read)?, read.len())
            }
            "ledger_rows_of" => {
                let keys: Vec<(LedgerKind, String)> = serde_json::from_value(arg(0))?;
                let read = store.ledger_rows_of(&keys)?;
                (serde_json::to_value(&read)?, read.len())
            }
            "ledger_row" => {
                let key: String = serde_json::from_value(arg(1))?;
                let read = store.ledger_row(serde_json::from_value(arg(0))?, &key)?;
                (serde_json::to_value(&read)?, 1)
            }
            "run_events" => {
                let run: RunId = serde_json::from_value(arg(0))?;
                let read: Vec<&RunEvent> = self
                    .events
                    .iter()
                    .filter(|event| event.run_id.as_ref() == Some(&run))
                    .collect();
                (serde_json::to_value(&read)?, read.len())
            }
            "run_resources" => (json!([]), 0),
            op => panic!("{op} is not read here"),
        })
    }
}

impl crate::application::supervisor_service::ServiceCaller for WaitRowsService {
    fn call(
        &self,
        _: crate::domain::queue_service::UseCase,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let op = params["op"].as_str().unwrap().to_owned();
        self.callers.lock().unwrap().push((
            op.clone(),
            crate::application::supervisor_service::caller::current(),
        ));
        let args = params["args"].as_array().cloned().unwrap_or_default();
        let (answer, held) = self.answer(&op, &args)?;
        self.received.lock().unwrap().push((op, held));
        Ok(answer)
    }
    fn with_token(
        &self,
        _: &std::path::Path,
    ) -> Result<std::sync::Arc<dyn crate::application::supervisor_service::ServiceCaller>> {
        unreachable!("no landing here")
    }
    fn sign_in(&self, _: &crate::domain::LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn take_back(&self, _: &crate::domain::LeaseToken, _: u32) -> Result<()> {
        Ok(())
    }
    fn hand_over(&self) {}
    fn sign_out(&self) {}
    fn credential(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// The supervisor's queue of `service`, as the queue service's path gives
/// it.
fn through(
    service: std::sync::Arc<WaitRowsService>,
) -> crate::application::supervisor_service::SupervisorQueue {
    crate::application::supervisor_service::SupervisorQueues::new(
        service,
        crate::domain::ActorContext::instance(crate::domain::ActorRole::Supervisor, 7),
    )
    .queue()
}

/// The lives and the waits before a claim made again through the queue
/// service ([`pre_claim_books`] over the supervisor's queue) are those of
/// the store read directly, and the service is asked only the start of the
/// tasks not settled, the rows the waits are made again from, the stops
/// and the last event of each life: with a few waits settled or many,
/// what crosses from it is the same, rows and events alike.
#[test]
fn the_wait_books_made_again_through_the_service_receive_as_much_however_many_waits_settled() {
    let received = |settled: i64| {
        let (log, at) = settled_waits(settled);
        let memory = Memory::of(&log);
        let mut builder = Builder::default();
        while pass(&memory, &mut builder, at).backlog > 0 {}
        let direct = pre_claim_books(&memory, ms(at)).unwrap();
        let service = WaitRowsService::of(&memory);
        assert_eq!(
            pre_claim_books(&through(service.clone()), ms(at)).unwrap(),
            direct
        );
        service.received.lock().unwrap().clone()
    };
    let (few, many) = (received(3), received(300));
    assert_eq!(many, few);
    let ops: Vec<&str> = few.iter().map(|(op, _)| op.as_str()).collect();
    assert_eq!(
        ops,
        [
            "ledger_tasks_open_since",
            "ledger_wait_rows",
            "ledger_supervisor_stops",
            "events_between"
        ]
    );
}

/// Through the queue service, the books made again from the rows of the
/// waits across a position ([`waits_across`]: waits open, ended by a
/// supervisor's life or ended since a task not settled, a stream not
/// replaced and the stops) are those of the store read directly.
#[test]
fn the_wait_books_across_a_position_through_the_service_are_the_stores() {
    use crate::domain::ledger::testkit::waits_across;
    let (log, position, at) = waits_across();
    let at = at - 100;
    let memory = Memory::default();
    memory.events.replace(log.events[..position].to_vec());
    let mut builder = Builder::default();
    drain(&memory, &mut builder, at);
    let direct = pre_claim_books(&memory, ms(at)).unwrap();
    assert_eq!(direct.0.supervisors(), 4);
    let service = WaitRowsService::of(&memory);
    assert_eq!(
        pre_claim_books(&through(service.clone()), ms(at)).unwrap(),
        direct
    );
    let received = service.received.lock().unwrap();
    assert!(
        received
            .iter()
            .any(|(op, held)| op == "ledger_supervisor_stops" && *held > 0),
        "{received:?}"
    );
}

/// The rows of a store a builder drained the first `upto` events of into,
/// at `LATER` (the sessions closed before then settled).
fn drained_to(log: &Log, upto: usize) -> Memory {
    drained_at(log, upto, LATER)
}

/// [`drained_to`] at `secs`.
fn drained_at(log: &Log, upto: usize, secs: i64) -> Memory {
    let memory = Memory::default();
    memory.events.replace(log.events[..upto].to_vec());
    drain(&memory, &mut Builder::default(), secs);
    memory
}

/// The id of the last of the first `upto` events of `log`.
fn id_at(log: &Log, upto: usize) -> EventId {
    log.events[upto - 1].id
}

/// The session rows a new builder writes on `rows` going on from the
/// `from`th event of `log` (its cursor) to the last, after `restore` made
/// what it keeps of the sessions from the store's rows.
fn going_on(
    rows: &BTreeMap<(LedgerKind, String), LedgerRow>,
    log: &Log,
    from: usize,
    restore: impl FnOnce(&Memory, &mut Builder),
) -> BTreeMap<(LedgerKind, String), LedgerRow> {
    let memory = Memory::default();
    memory.rows.replace(rows.clone());
    memory.events.replace(log.events.clone());
    let mut builder = Builder::default();
    builder.events = id_at(log, from);
    restore(&memory, &mut builder);
    drain(&memory, &mut builder, LATER + 1);
    memory
        .rows
        .into_inner()
        .into_iter()
        .filter(|((kind, _), _)| *kind == LedgerKind::Session)
        .collect()
}

/// The session rows a builder from its cursors' start writes of every
/// event of `log` on a store with none, at the time the builders going on
/// write theirs.
fn rows_of_every_event(log: &Log) -> BTreeMap<(LedgerKind, String), LedgerRow> {
    let memory = Memory::of(log);
    drain(&memory, &mut Builder::default(), LATER + 1);
    memory
        .rows
        .into_inner()
        .into_iter()
        .filter(|((kind, _), _)| *kind == LedgerKind::Session)
        .collect()
}

/// Whether `rows` are the session rows a builder from its cursors' start
/// writes of every event of `log` ([`rows_of_every_event`]), each whole:
/// its records, its range of inputs, its mark of settled and its version.
fn are_of_every_event(rows: &BTreeMap<(LedgerKind, String), LedgerRow>, log: &Log) -> bool {
    let whole = rows_of_every_event(log);
    assert!(whole.len() >= 7);
    *rows == whole
}

/// A new builder that takes at its cursor the session rows the store holds
/// as they were there ([`restore_sessions`]: the book, the records of the
/// rows not settled, the rows settled as dropped) and goes on writes the
/// session rows of every event: a late record of a closed session settled
/// gets its row's records back from the store, and one of a job not ended
/// is folded with the records kept. Taking the book alone leaves the
/// settled row's records out and writes over it with the late record
/// alone.
#[test]
fn a_new_builder_going_on_from_the_sessions_held_writes_the_rows_of_every_event() {
    let (log, position) = sessions_across();
    let held = drained_to(&log, position).rows.into_inner();
    let closed = (
        LedgerKind::Session,
        "runtime_planner/1/session/1".to_owned(),
    );
    let job = (LedgerKind::Session, "plan_review/9/job/1".to_owned());
    assert!(held[&closed].is_final);
    assert!(!held[&job].is_final);

    let rows = going_on(&held, &log, position, |memory, builder| {
        restore_sessions(memory, builder).unwrap();
        let wanted: BTreeSet<(LedgerKind, String)> = [closed.clone(), job.clone()].into();
        assert_eq!(builder.to_restore(&wanted), std::slice::from_ref(&closed));
        assert_eq!(
            builder.kept().size().sessions,
            held[&job].records.len()
                + held
                    .values()
                    .filter(|row| row.kind == LedgerKind::Session
                        && !row.is_final
                        && row.key != job.1)
                    .map(|row| row.records.len())
                    .sum::<usize>()
        );
    });
    assert!(are_of_every_event(&rows, &log));
    assert_eq!(rows[&closed].records.len(), held[&closed].records.len() + 1);
    assert!(rows[&job].records.len() > held[&job].records.len());

    let book_alone = going_on(&held, &log, position, |memory, builder| {
        builder.restore_session_rows(session_book(memory).unwrap());
    });
    assert!(!are_of_every_event(&book_alone, &log));
    assert_eq!(book_alone[&closed].records.len(), 1);
}

/// When the store's session rows lag the position P a builder read up to
/// (a pass's writes ran out: the planner's closed session lacks its late
/// record and its next session has no row), a builder going on from an
/// earlier position W the rows record every session event up to writes
/// the rows of every event, the rows past W cut back to it; going on from
/// P misses the late record and the session with no row; and going on
/// from W with the rows taken as they were at P (even rows that record
/// every event up to P) assigns the events after W twice, the planner's
/// next session a round again.
#[test]
fn going_on_from_where_the_lagging_rows_record_every_event_writes_the_rows_of_every_event() {
    let (log, w) = sessions_across();
    let p = w + 3;
    let at_p = drained_to(&log, p).rows.into_inner();
    let at_w = drained_to(&log, w).rows.into_inner();
    let closed = (
        LedgerKind::Session,
        "runtime_planner/1/session/1".to_owned(),
    );
    let next = (
        LedgerKind::Session,
        "runtime_planner/1/session/2".to_owned(),
    );
    assert!(
        at_p[&next]
            .records
            .iter()
            .all(|event| event.id > id_at(&log, w))
    );
    let mut lagging = at_p.clone();
    lagging.insert(closed.clone(), at_w[&closed].clone());
    lagging.remove(&next);
    assert_ne!(lagging[&closed], at_p[&closed]);

    let restore = |memory: &Memory, builder: &mut Builder| {
        restore_sessions(memory, builder).unwrap();
    };
    assert!(are_of_every_event(
        &going_on(&lagging, &log, w, restore),
        &log
    ));
    assert!(are_of_every_event(&going_on(&at_p, &log, w, restore), &log));

    let from_p = going_on(&lagging, &log, p, restore);
    assert!(!are_of_every_event(&from_p, &log));
    assert!(!from_p.contains_key(&next));
    assert_eq!(from_p[&closed].records.len(), at_w[&closed].records.len());

    let twice = going_on(&at_p, &log, w, |memory, builder| {
        let held = SessionsHeld::at(
            &memory.ledger_heads(LedgerKind::Session).unwrap(),
            &memory.ledger_session_anchors().unwrap(),
            id_at(&log, p),
        );
        let records = held
            .open
            .iter()
            .flat_map(|key| memory.row(LedgerKind::Session, key).records)
            .collect();
        builder.restore_sessions(held, records);
    });
    assert!(!are_of_every_event(&twice, &log));
    assert!(twice.contains_key(&(
        LedgerKind::Session,
        "runtime_planner/1/session/3".to_owned()
    )));
}

/// A session that closed in its grace at the cursor (not settled in the
/// store's row, and no later event reaches it) settles once its grace is
/// over, as in the fold of every event: the rows not settled the builder
/// takes are marked, so it tracks their grace.
#[test]
fn a_session_in_its_grace_at_the_cursor_settles_after_going_on() {
    let (log, position) = sessions_across();
    let held = drained_at(&log, position, 45).rows.into_inner();
    let review = (LedgerKind::Session, "plan_review/9/session/1".to_owned());
    assert!(held[&review].ended_at.is_some());
    assert!(!held[&review].is_final);
    let rows = going_on(&held, &log, position, |memory, builder| {
        restore_sessions(memory, builder).unwrap();
    });
    assert!(are_of_every_event(&rows, &log));
    assert!(rows[&review].is_final);
}

/// A row the store holds settled by an older version that no later event
/// reaches is folded again by this version once a builder goes on from
/// the rows, as from its cursors' start.
#[test]
fn a_row_settled_by_an_older_version_is_folded_again_after_going_on() {
    let (log, position) = sessions_across();
    let mut held = drained_to(&log, position).rows.into_inner();
    let review = (LedgerKind::Session, "plan_review/9/session/1".to_owned());
    let old = held.get_mut(&review).unwrap();
    assert!(old.is_final);
    old.ledger_v = LEDGER_V - 1;
    assert!(log.events[position..].iter().all(|event| {
        !held[&review]
            .records
            .iter()
            .any(|record| record.id == event.id)
    }));
    let rows = going_on(&held, &log, position, |memory, builder| {
        restore_sessions(memory, builder).unwrap();
    });
    assert_eq!(rows[&review].ledger_v, LEDGER_V);
    assert!(are_of_every_event(&rows, &log));
}

/// What a builder writes of the task rows, and how many wait rows and
/// events it read, going on from `position` of `log` with the lives and
/// the waits before a claim made again from the store's rows at the pass
/// at `at` (seconds), what else it keeps carried from the passes up to
/// there; `lag` drops the row of that task from the store before (a
/// builder whose writes lag its cursor), and `bare` gives its folds no
/// wait settled. Then the passes at `later` and [`LATER`] + 1.
struct GoingOn {
    tasks: BTreeMap<String, LedgerRow>,
    wait_rows: usize,
    events: usize,
}

fn tasks_going_on(
    log: &Log,
    position: usize,
    (at, later): (i64, i64),
    lag: Option<&str>,
    bare: bool,
) -> GoingOn {
    let memory = Memory::default();
    memory.events.replace(log.events[..position].to_vec());
    let mut builder = Builder::default();
    drain(&memory, &mut builder, at);
    if let Some(task) = lag {
        memory
            .rows
            .borrow_mut()
            .remove(&(LedgerKind::Task, task.to_owned()));
    }
    memory.read.set(0);
    memory.wait_rows.set(0);
    memory.no_settled_waits.set(bare);
    let (lives, book) = pre_claim_books(&memory, ms(at)).unwrap();
    builder.restore_pre_claim(lives, book);
    memory.events.replace(log.events.clone());
    drain(&memory, &mut builder, later);
    passes_to_drain(&memory, &mut builder, LATER + 1);
    let tasks = memory
        .rows
        .borrow()
        .iter()
        .filter(|((kind, _), _)| *kind == LedgerKind::Task)
        .map(|((_, key), row)| (key.clone(), row.clone()))
        .collect();
    GoingOn {
        tasks,
        wait_rows: memory.wait_rows.get(),
        events: memory.read.get(),
    }
}

/// The task rows of a builder from the first event that passes at `at`
/// over the events up to `position`, then at `later` and [`LATER`] + 1
/// over them all; task 1's is that of the fold of every event at `later`,
/// the last pass an event of it marks it at.
fn tasks_of_every_event(
    log: &Log,
    position: usize,
    (at, later): (i64, i64),
) -> BTreeMap<String, LedgerRow> {
    let memory = Memory::default();
    memory.events.replace(log.events[..position].to_vec());
    let mut builder = Builder::default();
    drain(&memory, &mut builder, at);
    memory.events.replace(log.events.clone());
    drain(&memory, &mut builder, later);
    passes_to_drain(&memory, &mut builder, LATER + 1);
    let whole = crate::domain::ledger::build(&log.events, ms(later), BUILT);
    let rows = memory.rows.borrow();
    for expected in whole
        .iter()
        .filter(|row| row.kind == LedgerKind::Task && row.key == "1")
    {
        let stored = &rows[&(LedgerKind::Task, expected.key.clone())];
        assert_eq!(
            LedgerRow {
                is_final: stored.is_final,
                ..expected.clone()
            },
            *stored
        );
    }
    rows.iter()
        .filter(|((kind, _), _)| *kind == LedgerKind::Task)
        .map(|((_, key), row)| (key.clone(), row.clone()))
        .collect()
}

fn heavy(supervisor: &str) -> Value {
    json!({"supervisor": supervisor, "class": "heavy"})
}

/// `old` slots full of a supervisor freed long before, each settled by the
/// pass at the position, over no time of a task.
fn old_waits(log: &mut Log, old: i64) {
    for k in 0..old {
        log.add(None, None, "slots_full_started", heavy("old"), 10 * k)
            .add(None, None, "slots_full_ended", heavy("old"), 10 * k + 5);
    }
}

/// Case (a), after `old` waits ([`old_waits`]): task 1 ready, a slots full
/// freed over its time ready, then task 1 canceled; task 2 ready after
/// their grace, so the book made again holds the waits from its start on.
/// The pass at the position settles task 1 and the slots full. After it, a
/// later event of task 1 (ready again) folds its settled row again.
fn settled_task_folded_again(old: i64) -> (Log, usize, (i64, i64)) {
    let mut log = Log::default();
    old_waits(&mut log, old);
    let t = 10 * old + 100;
    let grace = SETTLE_GRACE_MS / 1000;
    log.add(Some(1), None, "task_created", json!({"status": "draft"}), t)
        .add(
            Some(1),
            None,
            "task_status_changed",
            json!({"from": "draft", "to": "ready"}),
            t + 10,
        )
        .add(None, None, "slots_full_started", heavy("a"), t + 20)
        .add(None, None, "slots_full_ended", heavy("a"), t + 30)
        .add(
            Some(1),
            None,
            "task_status_changed",
            json!({"from": "ready", "to": "canceled"}),
            t + 40,
        )
        .add(
            Some(2),
            None,
            "task_created",
            json!({"status": "draft"}),
            t + 40 + grace + 10,
        )
        .add(
            Some(2),
            None,
            "task_status_changed",
            json!({"from": "draft", "to": "ready"}),
            t + 40 + grace + 20,
        );
    let position = log.events.len();
    let at = t + 40 + 2 * grace + 100;
    log.add(
        Some(1),
        None,
        "task_status_changed",
        json!({"from": "canceled", "to": "ready"}),
        at + 10,
    );
    (log, position, (at, at + 20))
}

/// Case (b): task 1 ready over a slots full freed, not ended; the store
/// has no row of it at the position, so the book made again does not hold
/// the slots full. After it, task 1 is canceled.
fn task_of_no_row() -> (Log, usize, (i64, i64)) {
    let mut log = Log::default();
    let grace = SETTLE_GRACE_MS / 1000;
    log.add(Some(1), None, "task_created", json!({"status": "draft"}), 0)
        .add(
            Some(1),
            None,
            "task_status_changed",
            json!({"from": "draft", "to": "ready"}),
            10,
        )
        .add(None, None, "slots_full_started", heavy("a"), 20)
        .add(None, None, "slots_full_ended", heavy("a"), 30);
    let position = log.events.len();
    let at = grace + 100;
    log.add(
        Some(1),
        None,
        "task_status_changed",
        json!({"from": "ready", "to": "canceled"}),
        at + 10,
    );
    (log, position, (at, at + 20))
}

/// Case (c): a slots full freed and settled by the pass at the position;
/// after it, task 1 created and ready at the very time the slots full
/// ended, its events written late.
fn ready_as_a_wait_ended() -> (Log, usize, (i64, i64)) {
    let mut log = Log::default();
    let grace = SETTLE_GRACE_MS / 1000;
    log.add(None, None, "slots_full_started", heavy("a"), 0)
        .add(None, None, "slots_full_ended", heavy("a"), 50);
    let position = log.events.len();
    let at = grace + 100;
    log.add(
        Some(1),
        None,
        "task_created",
        json!({"status": "draft"}),
        50,
    )
    .add(
        Some(1),
        None,
        "task_status_changed",
        json!({"from": "draft", "to": "ready"}),
        50,
    );
    (log, position, (at, at + 20))
}

/// A builder that goes on from a position with the books of the waits
/// made again from the store's rows writes the task rows of the fold of
/// every event, though the book holds no wait settled before the tasks
/// not settled there: a settled task row its later event folds again over
/// a slots full freed before (a), a task with no row at the position (b),
/// and one ready at the very time a wait ended before (c) read the waits
/// settled over their time from the store's rows. Without them (a) and (b)
/// write another row; in (c) the wait bears on the task by its reach
/// alone, which its own events after the position pass.
#[test]
fn a_builder_going_on_from_the_books_made_again_writes_the_task_rows_of_every_event() {
    for (case, (log, position, at), lag) in [
        ("a", settled_task_folded_again(0), None),
        ("b", task_of_no_row(), Some("1")),
        ("c", ready_as_a_wait_ended(), None),
    ] {
        let whole = tasks_of_every_event(&log, position, at);
        assert!(whole.contains_key("1"), "{case}");
        let went_on = tasks_going_on(&log, position, at, lag, false);
        assert_eq!(went_on.tasks, whole, "{case}");
        let bare = tasks_going_on(&log, position, at, lag, true);
        // The rows a task's fold read of the waits settled.
        assert!(went_on.wait_rows > bare.wait_rows, "{case}");
        assert_eq!(bare.tasks["1"] == whole["1"], case == "c", "{case}");
    }
}

/// The fold of a task again reads of the waits settled only those over its
/// time: with a few waits settled before it or many, making the books
/// again and folding the task again read as many wait rows and as many
/// events.
#[test]
fn folding_a_task_again_reads_as_many_wait_rows_however_many_waits_settled_before_it() {
    let read = |old: i64| {
        let (log, position, at) = settled_task_folded_again(old);
        assert_eq!(
            tasks_going_on(&log, position, at, None, false).tasks,
            tasks_of_every_event(&log, position, at)
        );
        let went_on = tasks_going_on(&log, position, at, None, false);
        (went_on.wait_rows, went_on.events)
    };
    let few = read(3);
    assert_eq!(read(300), few);
    assert!(few.0 > 0);
}

/// The planners' sessions of `planners` planners, each opened and closed,
/// and `tasks` tasks created, every one at once: more session rows and
/// more task rows than a pass writes of the bounded kinds.
fn sessions_and_tasks(planners: i64, tasks: i64) -> Log {
    let mut log = Log::default();
    for task in 1..=tasks {
        log.add(
            Some(task),
            None,
            "task_created",
            json!({"status": "draft"}),
            1,
        );
    }
    for planner in 1..=planners {
        log.add(
            None,
            None,
            "session_opened",
            json!({"kind": "runtime_planner", "planner_id": planner}),
            2,
        );
        let opened = log.last().as_i64();
        log.add(
            None,
            None,
            "session_closed",
            json!({"kind": "runtime_planner", "planner_id": planner,
                   "opened_event_id": opened, "session_id": format!("p-{planner}")}),
            3,
        );
    }
    log
}

/// The session rows a pass changed are written that pass, past
/// [`WRITES_PER_PASS`], [`SESSION_ROWS_PER_WRITE`] a write, the last with
/// the mark of the pass's position; the task rows keep their bound and their order, and are
/// written by the next passes, which write no mark again while no event
/// came in.
#[test]
fn the_session_rows_a_pass_changed_are_written_that_pass_past_the_bound_with_the_mark() {
    let bound = i64::try_from(WRITES_PER_PASS).unwrap();
    let log = sessions_and_tasks(bound + 50, bound + 30);
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    let built = pass(&memory, &mut builder, LATER);
    assert_eq!(built.failure, None);
    assert_eq!(built.sessions, WRITES_PER_PASS + 50);
    assert_eq!(memory.count(LedgerKind::Session), WRITES_PER_PASS + 50);
    assert_eq!(built.written, WRITES_PER_PASS);
    assert_eq!(memory.count(LedgerKind::Task), WRITES_PER_PASS);
    // The first tasks by their order.
    assert!(
        memory
            .rows
            .borrow()
            .contains_key(&(LedgerKind::Task, "1".into()))
    );
    let batches = (WRITES_PER_PASS + 50).div_ceil(SESSION_ROWS_PER_WRITE);
    assert!(batches > 1);
    assert_eq!(memory.batches.get(), batches);
    assert_eq!(*memory.mark.borrow(), Some(SessionsWritten::at(log.last())));
    let next = drain(&memory, &mut builder, LATER);
    assert_eq!(next.sessions, 0);
    assert_eq!(memory.count(LedgerKind::Task), WRITES_PER_PASS + 30);
    assert_eq!(memory.batches.get(), batches);
    assert!(are_of_every_event(
        &rows_of_kind(&memory, LedgerKind::Session),
        &log
    ));
}

/// The rows of `kind` `memory` holds.
fn rows_of_kind(memory: &Memory, kind: LedgerKind) -> BTreeMap<(LedgerKind, String), LedgerRow> {
    memory
        .rows
        .borrow()
        .iter()
        .filter(|((of, _), _)| *of == kind)
        .map(|(key, row)| (key.clone(), row.clone()))
        .collect()
}

/// A write of the session rows that fails part way writes none of them and
/// no mark, and leaves them marked: the next pass writes them all with
/// the mark.
#[test]
fn a_write_of_the_session_rows_that_fails_writes_none_and_the_next_pass_writes_them() {
    let log = sessions_and_tasks(30, 0);
    let memory = Memory::of(&log);
    memory.batch_fails.set(1);
    let mut builder = Builder::default();
    let failed = pass(&memory, &mut builder, LATER);
    assert!(failed.failure.is_some());
    assert_eq!(memory.count(LedgerKind::Session), 0);
    assert_eq!(*memory.mark.borrow(), None);
    assert!(failed.backlog >= 30);
    let built = drain(&memory, &mut builder, LATER);
    assert_eq!(built.failure, None);
    assert_eq!(memory.count(LedgerKind::Session), 30);
    assert_eq!(*memory.mark.borrow(), Some(SessionsWritten::at(log.last())));
}

/// The events of [`sessions_across`] after [`WRITES_PER_PASS`] and more
/// tasks created, their ids and the events they name moved past the
/// tasks', and where the events after its position start: a pass over the
/// first of them leaves the task rows behind its bound.
fn sessions_behind_tasks() -> (Log, usize) {
    let tasks = WRITES_PER_PASS + 40;
    let (sessions, position) = sessions_across();
    let mut log = sessions_and_tasks(0, i64::try_from(tasks).unwrap());
    let shift = log.last().as_i64();
    for event in sessions.events {
        let mut payload = event.payload.clone();
        if let Some(opened) = payload["opened_event_id"].as_i64() {
            payload["opened_event_id"] = json!(opened + shift);
        }
        log.events.push(RunEvent {
            id: EventId::new(event.id.as_i64() + shift),
            payload,
            ..event
        });
    }
    (log, tasks + position)
}

/// On a store the new writes wrote, whose task rows lag the session rows
/// behind [`WRITES_PER_PASS`], the position the mark gives
/// ([`sessions_written_through`]) is the pass's, and a builder going on
/// from it (the rows held there, [`restore_sessions`]) writes the session
/// rows of every event: whether a new session opens and a closed one gets
/// a late record after it, or before it, or the position is the last.
#[test]
fn going_on_from_the_position_of_the_mark_writes_the_session_rows_of_every_event() {
    let (log, position) = sessions_behind_tasks();
    let closed = (
        LedgerKind::Session,
        "runtime_planner/1/session/1".to_owned(),
    );
    let next = (
        LedgerKind::Session,
        "runtime_planner/1/session/2".to_owned(),
    );
    for upto in [
        position - 2,
        position,
        position + 1,
        position + 2,
        position + 3,
        log.events.len(),
    ] {
        let memory = Memory::default();
        memory.events.replace(log.events[..upto].to_vec());
        let built = pass(&memory, &mut Builder::default(), LATER);
        assert_eq!(built.failure, None, "{upto}");
        assert!(built.backlog > 0, "{upto}: the task rows lag");
        assert!(memory.count(LedgerKind::Task) < WRITES_PER_PASS + 40);
        let w = sessions_written_through(&memory).unwrap();
        assert_eq!(w, Some(id_at(&log, upto)), "{upto}");
        // Before the position, after it, or neither.
        let rows = rows_of_kind(&memory, LedgerKind::Session);
        assert_eq!(rows.contains_key(&next), upto > position + 1, "{upto}");
        assert!(rows.contains_key(&closed));

        memory.events.replace(log.events.clone());
        let mut builder = Builder::default();
        builder.events = w.unwrap();
        restore_sessions(&memory, &mut builder).unwrap();
        drain(&memory, &mut builder, LATER + 1);
        assert!(
            are_of_every_event(&rows_of_kind(&memory, LedgerKind::Session), &log),
            "{upto}"
        );
        assert_eq!(
            *memory.mark.borrow(),
            Some(SessionsWritten::at(log.last())),
            "{upto}"
        );
    }
}

/// The store of [`going_on_from_where_the_lagging_rows_record_every_event_writes_the_rows_of_every_event`]
/// (written one row at a time: the planner's next session has no row and
/// its closed one lacks its late record), `p` its position and the rows
/// there, with no mark.
fn lagging_store(log: &Log, w: usize) -> (Memory, usize) {
    let p = w + 3;
    let at_w = drained_to(log, w).rows.into_inner();
    let mut lagging = drained_to(log, p).rows.into_inner();
    let closed = (
        LedgerKind::Session,
        "runtime_planner/1/session/1".to_owned(),
    );
    lagging.insert(closed.clone(), at_w[&closed].clone());
    lagging.remove(&(
        LedgerKind::Session,
        "runtime_planner/1/session/2".to_owned(),
    ));
    let memory = Memory::default();
    memory.rows.replace(lagging);
    memory.events.replace(log.events[..p].to_vec());
    (memory, p)
}

/// A store with no mark, the session rows written one at a time and
/// lagging (a new session with no row, a closed one without its late
/// record), gives no position: not the largest `inputs_through` of its
/// rows, from which going on misses both.
#[test]
fn a_store_of_lagging_rows_with_no_mark_gives_no_position() {
    let (log, w) = sessions_across();
    let (memory, p) = lagging_store(&log, w);
    let largest = memory
        .rows
        .borrow()
        .values()
        .filter(|row| row.kind == LedgerKind::Session)
        .map(|row| row.inputs_through.last())
        .max()
        .unwrap();
    assert!(largest >= id_at(&log, w));
    assert_eq!(*memory.mark.borrow(), None);
    assert_eq!(sessions_written_through(&memory).unwrap(), None);
    let heads = memory.ledger_heads(LedgerKind::Session).unwrap();
    assert_eq!(written_through(None, &heads), None);
    assert!(p <= log.events.len());
}

/// A builder from its cursors' start on a store with no mark and lagging
/// rows fills what they lack and writes the mark, decided by the store
/// alone; going on from it then writes the rows of every event. Stopped
/// part way: (i) a first write that fails leaves no mark, and a builder
/// from its cursors' start again finishes; (ii) once a mark is written, a
/// later write that fails leaves it, and a builder going on from it after
/// a start again writes the rows of every event; (iii) a builder made
/// again from the rows' heads at a position not the mark's writes no mark.
#[test]
fn a_store_with_no_mark_is_filled_and_marked_by_a_builder_from_its_cursors_start() {
    let (log, w) = sessions_across();
    let (memory, p) = lagging_store(&log, w);
    let to_p = Log {
        events: log.events[..p].to_vec(),
    };
    // (i)
    memory.batch_fails.set(1);
    assert!(
        pass(&memory, &mut Builder::default(), LATER + 1)
            .failure
            .is_some()
    );
    assert_eq!(sessions_written_through(&memory).unwrap(), None);
    let filled = rows_of_every_event(&to_p);
    assert_ne!(rows_of_kind(&memory, LedgerKind::Session), filled);
    drain(&memory, &mut Builder::default(), LATER + 1);
    assert_eq!(rows_of_kind(&memory, LedgerKind::Session), filled);
    let w1 = sessions_written_through(&memory).unwrap();
    assert_eq!(w1, Some(id_at(&log, p)));

    // (ii)
    memory.events.replace(log.events.clone());
    memory.batch_fails.set(1);
    let mut builder = Builder::default();
    builder.events = w1.unwrap();
    restore_sessions(&memory, &mut builder).unwrap();
    assert!(pass(&memory, &mut builder, LATER + 1).failure.is_some());
    assert_eq!(sessions_written_through(&memory).unwrap(), w1);
    drop(builder);
    let mut started_again = Builder::default();
    started_again.events = w1.unwrap();
    restore_sessions(&memory, &mut started_again).unwrap();
    drain(&memory, &mut started_again, LATER + 1);
    assert!(are_of_every_event(
        &rows_of_kind(&memory, LedgerKind::Session),
        &log
    ));
    assert_eq!(sessions_written_through(&memory).unwrap(), Some(log.last()));

    // (iii)
    let marked = drained_to(&log, w);
    let at_w = Some(id_at(&log, w));
    assert_eq!(sessions_written_through(&marked).unwrap(), at_w);
    marked.events.replace(log.events.clone());
    let mut elsewhere = Builder::default();
    elsewhere.events = id_at(&log, w + 3);
    restore_sessions(&marked, &mut elsewhere).unwrap();
    drain(&marked, &mut elsewhere, LATER + 1);
    assert_eq!(sessions_written_through(&marked).unwrap(), at_w);
    let mut book_alone = Builder::default();
    book_alone.events = id_at(&log, w);
    book_alone.restore_session_rows(session_book(&marked).unwrap());
    // Its folds of the rows the one before wrote past it stay behind them.
    let built = passes_to_drain(&marked, &mut book_alone, LATER + 1);
    assert_eq!(built.failure, None);
    assert_eq!(sessions_written_through(&marked).unwrap(), at_w);
}

/// A write of the session rows that fails stops no other row: the task
/// rows are written in their bound that pass, and the session rows wait
/// aside, none written alone, for the next pass's write of them all.
#[test]
fn a_failed_write_of_the_session_rows_stops_no_other_row() {
    let log = sessions_and_tasks(30, 10);
    let memory = Memory::of(&log);
    memory.batch_fails.set(1);
    let mut builder = Builder::default();
    let failed = pass(&memory, &mut builder, LATER);
    assert!(failed.failure.is_some());
    assert_eq!(memory.count(LedgerKind::Task), 10);
    assert_eq!(memory.count(LedgerKind::Session), 0);
    assert_eq!(*memory.mark.borrow(), None);
    let built = drain(&memory, &mut builder, LATER);
    assert_eq!(built.failure, None);
    assert_eq!(memory.count(LedgerKind::Session), 30);
    assert_eq!(memory.batches.get(), 1);
    assert_eq!(*memory.mark.borrow(), Some(SessionsWritten::at(log.last())));
}

/// A builder whose cursor was set past its session book's (not read up to,
/// nor gone on from the rows there) writes no mark: its book lacks the
/// session events before its cursor.
#[test]
fn a_builder_whose_cursor_was_set_past_its_book_writes_no_mark() {
    let (log, position) = sessions_across();
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    builder.events = id_at(&log, position);
    let built = passes_to_drain(&memory, &mut builder, LATER);
    assert_eq!(built.failure, None);
    assert_eq!(*memory.mark.borrow(), None);
}

/// A write of a later chunk of the session rows that fails leaves the
/// chunks written before it written and the mark held from before as it
/// was: the rows written only add records, and a builder going on from
/// that mark after a start again writes the session rows of every event.
#[test]
fn a_later_chunk_that_fails_leaves_the_mark_from_before_true() {
    let planners = i64::try_from(SESSION_ROWS_PER_WRITE * 2).unwrap();
    let log = sessions_and_tasks(planners, 0);
    let memory = Memory::default();
    memory.events.replace(log.events[..20].to_vec());
    drain(&memory, &mut Builder::default(), LATER);
    let before = sessions_written_through(&memory).unwrap();
    assert_eq!(before, Some(id_at(&log, 20)));
    let mut builder = Builder::default();
    builder.events = before.unwrap();
    restore_sessions(&memory, &mut builder).unwrap();
    memory.events.replace(log.events.clone());
    memory
        .batch_fails_at
        .set(Some(memory.batch_calls.get() + 1));
    let failed = pass(&memory, &mut builder, LATER);
    assert!(failed.failure.is_some());
    let written = memory.count(LedgerKind::Session);
    assert!(
        written > 10 && written < SESSION_ROWS_PER_WRITE * 2,
        "{written}"
    );
    assert_eq!(sessions_written_through(&memory).unwrap(), before);
    drop(builder);
    let mut started_again = Builder::default();
    started_again.events = before.unwrap();
    restore_sessions(&memory, &mut started_again).unwrap();
    drain(&memory, &mut started_again, LATER + 1);
    assert_eq!(
        rows_of_kind(&memory, LedgerKind::Session),
        rows_of_every_event(&log)
    );
    assert_eq!(sessions_written_through(&memory).unwrap(), Some(log.last()));
}

/// The session rows a pass writes are cut by the bytes of their text as
/// well as by [`SESSION_ROWS_PER_WRITE`]: rows each of which crosses alone,
/// fewer than a write holds by their number but more than its bytes
/// together, go in several writes each within the bytes (a row past them
/// alone a write of its own), every row is written, and the mark goes with
/// the last of them only.
#[test]
fn the_session_rows_are_cut_by_their_bytes_too() {
    let log = sessions_and_tasks(12, 0);
    let whole = Memory::of(&log);
    drain(&whole, &mut Builder::default(), LATER);
    let sizes: Vec<usize> = rows_of_kind(&whole, LedgerKind::Session)
        .values()
        .map(|row| serde_json::to_vec(row).unwrap().len())
        .collect();
    let largest = *sizes.iter().max().unwrap();
    let bound = largest * 3;
    assert!(sizes.iter().sum::<usize>() > bound);
    assert!(sizes.len() < SESSION_ROWS_PER_WRITE);

    let memory = Memory::of(&log);
    let built = build_pass_bounded(
        &memory,
        &mut Builder::default(),
        (ms(LATER), BUILT),
        Retain::default(),
        None,
        (EVENTS_PER_READ, bound),
    );
    assert_eq!(built.failure, None);
    assert_eq!(built.sessions, 12);
    let batches = memory.batch_bytes.borrow().clone();
    assert!(batches.len() > 1, "{batches:?}");
    assert!(
        batches.iter().all(|(bytes, _)| *bytes <= bound),
        "{batches:?}"
    );
    let marked: Vec<bool> = batches.iter().map(|(_, mark)| *mark).collect();
    assert_eq!(marked.iter().filter(|mark| **mark).count(), 1);
    assert_eq!(marked.last(), Some(&true));
    assert_eq!(
        rows_of_kind(&memory, LedgerKind::Session),
        rows_of_kind(&whole, LedgerKind::Session)
    );

    // A row past the bytes alone is a write of its own.
    let alone = Memory::of(&log);
    build_pass_bounded(
        &alone,
        &mut Builder::default(),
        (ms(LATER), BUILT),
        Retain::default(),
        None,
        (EVENTS_PER_READ, 1),
    );
    assert_eq!(alone.batch_bytes.borrow().len(), 12);
    assert_eq!(alone.count(LedgerKind::Session), 12);
}

/// The mark R the store holds ([`rows_written`]).
fn r_of(memory: &Memory) -> Cursors {
    rows_written(memory).unwrap().expect("the store holds R")
}

/// A pass that leaves task rows past [`WRITES_PER_PASS`] writes R before
/// the event of the first task left, its other cursors the pass's, with W
/// at the pass's position; the pass that writes the last of them moves R
/// to its cursors with W as it was, the marks alone. A pass whose write of
/// the session rows and the marks fails leaves R and W as they were, and
/// the next pass moves them.
#[test]
fn r_stays_before_a_row_left_past_the_bound_and_reaches_the_pass_once_written() {
    let bound = WRITES_PER_PASS;
    let mut log = sessions_and_tasks(0, i64::try_from(bound + 30).unwrap());
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    let built = pass(&memory, &mut builder, LATER);
    assert_eq!(built.failure, None);
    assert_eq!(memory.count(LedgerKind::Task), bound);
    let w = sessions_written_through(&memory).unwrap();
    assert_eq!(w, Some(log.last()));
    let r = r_of(&memory);
    // Task bound + 1 is created by the event after the bound'th.
    assert_eq!(r.events, log.events[bound - 1].id);
    assert!(Some(r.events) <= w);
    assert_eq!(
        Cursors {
            events: builder.events,
            ..r
        },
        builder.cursors()
    );

    drain(&memory, &mut builder, LATER);
    assert_eq!(memory.count(LedgerKind::Task), bound + 30);
    assert_eq!(r_of(&memory), builder.cursors());
    assert_eq!(r_of(&memory).events, log.last());
    assert_eq!(memory.marks_alone.get(), 1);
    assert_eq!(memory.batches.get(), 1, "W written once");

    // More tasks past the bound and a session: its write fails.
    let (r, w) = (r_of(&memory), sessions_written_through(&memory).unwrap());
    let start = i64::try_from(bound).unwrap() + 31;
    for task in start..start + i64::try_from(bound).unwrap() + 10 {
        log.add(Some(task), None, "task_created", json!({}), 4);
    }
    log.add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 1}),
        5,
    );
    memory.events.replace(log.events.clone());
    memory.batch_fails.set(1);
    let failed = pass(&memory, &mut builder, LATER);
    assert!(failed.failure.is_some());
    assert_eq!(failed.written, bound, "the other rows are written");
    assert_eq!(r_of(&memory), r);
    assert_eq!(sessions_written_through(&memory).unwrap(), w);
    drain(&memory, &mut builder, LATER);
    assert_eq!(r_of(&memory), builder.cursors());
    assert_eq!(sessions_written_through(&memory).unwrap(), Some(log.last()));
}

/// A builder whose cursor was set at W (its session rows gone on from
/// there, [`restore_sessions`]) moves W as it goes on, and leaves R as the
/// store holds it: its marks do not follow the inputs before its cursor.
#[test]
fn a_builder_whose_marks_do_not_follow_every_input_leaves_r() {
    let mut log = sessions_and_tasks(2, 3);
    let memory = Memory::of(&log);
    drain(&memory, &mut Builder::default(), LATER);
    let r = r_of(&memory);
    assert_eq!(r.events, log.last());
    let w = sessions_written_through(&memory).unwrap().unwrap();
    log.add(Some(9), None, "task_created", json!({}), 4).add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 9}),
        5,
    );
    memory.events.replace(log.events.clone());
    let mut builder = Builder::default();
    builder.events = w;
    restore_sessions(&memory, &mut builder).unwrap();
    drain(&memory, &mut builder, LATER);
    assert_eq!(sessions_written_through(&memory).unwrap(), Some(log.last()));
    assert_eq!(r_of(&memory), r);
}

/// On a store a pass left task rows past [`WRITES_PER_PASS`] with R
/// before W (R.events < W), a new builder going on from R
/// ([`Builder::go_on_from_rows`]) with the session rows held there
/// ([`restore_sessions`]) keeps the fold of every session event: it writes
/// the session rows of every event (the session events between R and W
/// read again give no round twice, and a late record after W is not
/// missed) and, once caught up, moves W and R to its cursors. One whose
/// rows are held past W writes no mark.
#[test]
fn going_on_from_r_before_w_writes_the_rows_of_every_event_and_moves_both_marks() {
    let (log, position) = sessions_behind_tasks();
    for upto in [position, position + 2, log.events.len()] {
        let memory = Memory::default();
        memory.events.replace(log.events[..upto].to_vec());
        let built = pass(&memory, &mut Builder::default(), LATER);
        assert_eq!(built.failure, None, "{upto}");
        assert!(built.backlog > 0, "{upto}: the task rows lag");
        let w = sessions_written_through(&memory).unwrap().unwrap();
        assert_eq!(w, id_at(&log, upto), "{upto}");
        let r = r_of(&memory);
        assert!(r.events < w, "{upto}");
        // Session events lie between R and W.
        assert!(
            log.events
                .iter()
                .any(|event| event.id > r.events && event.id <= w && event.task_id.is_none()),
            "{upto}"
        );

        memory.events.replace(log.events.clone());
        let mut builder = Builder::default();
        builder.go_on_from_rows(r);
        restore_sessions(&memory, &mut builder).unwrap();
        assert_eq!(builder.events, r.events);
        let built = drain(&memory, &mut builder, LATER + 1);
        assert_eq!(built.failure, None, "{upto}");
        assert!(
            are_of_every_event(&rows_of_kind(&memory, LedgerKind::Session), &log),
            "{upto}"
        );
        assert_eq!(
            sessions_written_through(&memory).unwrap(),
            Some(log.last()),
            "{upto}"
        );
        assert_eq!(r_of(&memory), builder.cursors(), "{upto}");
        assert_eq!(r_of(&memory).events, log.last(), "{upto}");
    }

    // Held past W: no mark moves.
    let upto = position;
    let memory = Memory::default();
    memory.events.replace(log.events[..upto].to_vec());
    pass(&memory, &mut Builder::default(), LATER);
    let w = sessions_written_through(&memory).unwrap();
    let r = r_of(&memory);
    memory.events.replace(log.events.clone());
    let mut past = Builder::default();
    past.go_on_from_rows(Cursors {
        events: id_at(&log, upto + 2),
        ..r
    });
    restore_sessions(&memory, &mut past).unwrap();
    let built = passes_to_drain(&memory, &mut past, LATER + 1);
    assert_eq!(built.failure, None);
    assert_eq!(sessions_written_through(&memory).unwrap(), w);
    assert_eq!(r_of(&memory), r);
}

/// The rows a fold of every input writes at `secs` ([`drain`] from the
/// cursors' start) of the events, node samples and windows of `memory`.
fn rows_of_every_input(memory: &Memory, secs: i64) -> BTreeMap<(LedgerKind, String), LedgerRow> {
    let whole = Memory::of(&Log {
        events: memory.events.borrow().clone(),
    });
    whole.samples.replace(memory.samples.borrow().clone());
    whole
        .run_resources
        .replace(memory.run_resources.borrow().clone());
    whole.steps.replace(memory.steps.borrow().clone());
    whole.step_id.set(memory.step_id.get());
    whole.owners.replace(memory.owners.borrow().clone());
    whole.inside.replace(memory.inside.borrow().clone());
    whole.inside_id.set(memory.inside_id.get());
    drain(&whole, &mut Builder::default(), secs);
    whole.rows.into_inner()
}

/// Every row of `whole` (the fold of every input) `memory` lacks or holds
/// otherwise has an input after `r` in some cursor, so that a builder
/// going on from `r` reads it again and marks the row: the kinds of those
/// rows.
#[track_caller]
fn lagging_rows_have_an_input_after(
    memory: &Memory,
    whole: &BTreeMap<(LedgerKind, String), LedgerRow>,
    r: Cursors,
) -> BTreeSet<LedgerKind> {
    let rows = memory.rows.borrow();
    let mut lagging = BTreeSet::new();
    for (key, row) in whole {
        if rows.get(key) == Some(row) {
            continue;
        }
        lagging.insert(key.0);
        let through = &row.inputs_through;
        assert!(
            through.last() > r.events
                || through.node_samples.is_some_and(|id| id > r.samples)
                || through.steps.is_some_and(|id| id > r.steps)
                || through.resources.is_some_and(|id| id > r.resources)
                || through
                    .inside_resources
                    .is_some_and(|id| id > r.inside_resources),
            "{key:?} lags with no input after {r:?}: {through:?}"
        );
    }
    lagging
}

/// On a store a pass left task, queue and node rows unwritten past
/// [`WRITES_PER_PASS`] (the session rows and their mark written, no input
/// after), each row the fold of every input has that the store lacks has
/// an input after R; the node row's is a sample, which R's NodeSampleStore
/// cursor stops before. Once drained, the store holds every row and R is
/// the cursors.
#[test]
fn every_row_a_store_lacks_has_an_input_after_r() {
    let (mut log, mut samples) = every_kind_of_row();
    one_more_hour(&mut samples);
    for task in 1_000..1_000 + i64::try_from(WRITES_PER_PASS).unwrap() {
        log.add(Some(task), None, "task_created", json!({}), 3_300);
    }
    // More queue rows than a pass takes first ([`HOLDING_R_PER_PASS`]):
    // all hold R from where the page's rows were marked, and those past
    // the marks taken first wait in the round past the bound.
    for _ in 0..HOLDING_R_PER_PASS + 3 {
        log.add(None, None, "authorization_denied", json!({}), 3_300);
    }
    let memory = Memory::of(&log);
    memory.samples.replace(samples);
    let whole = rows_of_every_input(&memory, LATER);
    let mut builder = Builder::default();
    let built = pass(&memory, &mut builder, LATER);
    assert_eq!(built.failure, None);
    assert!(built.backlog > 0);
    let r = r_of(&memory);
    assert!(r.samples < builder.samples);
    let lagging = lagging_rows_have_an_input_after(&memory, &whole, r);
    for kind in [LedgerKind::Task, LedgerKind::Queue, LedgerKind::Node] {
        assert!(lagging.contains(&kind), "{kind:?}: {lagging:?}");
    }
    let node = whole
        .iter()
        .find(|((kind, _), _)| *kind == LedgerKind::Node)
        .unwrap()
        .1;
    assert_eq!(node.inputs_through.events, EventId::new(0));

    drain(&memory, &mut builder, LATER);
    assert!(lagging_rows_have_an_input_after(&memory, &whole, r_of(&memory)).is_empty());
    assert_eq!(r_of(&memory), builder.cursors());
}

/// `samples` with one more of the node's next hour: a pass takes first
/// the hour holding R's samples earliest ([`HOLDING_R_PER_PASS`]), so this
/// one waits in the round, past the bound of writes when more rows than
/// it are marked before it.
fn one_more_hour(samples: &mut Vec<NodeSample>) {
    let last = samples.last().expect("a sample").clone();
    samples.push(sample(last.id + 1, last.sample.unix + 3600));
}

/// A run id of `n`, past what [`run_id`] gives.
fn many_run_id(n: usize) -> String {
    format!("{n:08x}-1111-4111-8111-111111111111")
}

/// Runs landed and settled, then a window of resources of each recorded
/// late, more than a pass writes: the pass leaves the last runs' rows
/// without their window, and R's cursor of the runs' resources stops
/// before the first of those windows while its events are the pass's.
/// Each run row the store holds otherwise than the fold of every input has
/// a window after R.
#[test]
fn a_run_left_without_its_late_window_holds_r_back_by_the_resources_cursor() {
    let runs = WRITES_PER_PASS + 5;
    let mut log = Log::default();
    let mut attachments = Vec::new();
    for n in 1..=runs {
        let run = many_run_id(n);
        let secs = i64::try_from(n).unwrap() * 1_000;
        log.landed(i64::try_from(n).unwrap(), &run, secs);
        attachments.push((run.clone(), attach(&mut log, &run, secs), secs));
    }
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    drain(&memory, &mut builder, LATER);
    for (run, attachment, secs) in &attachments {
        measured(
            &memory,
            run,
            *attachment,
            (secs + 10, secs + 300),
            Some(30.0),
            true,
        );
    }
    let whole = rows_of_every_input(&memory, LATER);
    let built = pass(&memory, &mut builder, LATER);
    assert_eq!(built.failure, None);
    assert_eq!(built.written, WRITES_PER_PASS);
    let r = r_of(&memory);
    assert_eq!(r.events, log.last());
    assert_eq!(r.resources, i64::try_from(WRITES_PER_PASS).unwrap());
    assert_eq!(builder.resources, i64::try_from(runs).unwrap());
    let lagging = lagging_rows_have_an_input_after(&memory, &whole, r);
    assert_eq!(lagging, BTreeSet::from([LedgerKind::Run]));

    drain(&memory, &mut builder, LATER);
    assert!(lagging_rows_have_an_input_after(&memory, &whole, r_of(&memory)).is_empty());
    assert_eq!(r_of(&memory).resources, i64::try_from(runs).unwrap());
}

/// Runs landed on their own tasks, more than a pass writes: the first
/// pass from the cursors' start writes the run rows and leaves the task
/// rows of the completed tasks unwritten, with W written and no input
/// after; each settled task row the store lacks has an event after R.
/// Once drained, steps of each run come in late, more than a pass writes:
/// the pass leaves the last runs' rows stale, without their steps, and R's
/// SessionStepStore cursor stops before the first of those steps while its
/// events are the pass's; each of those rows has a step after R.
#[test]
fn completed_tasks_left_unwritten_and_runs_stale_by_their_steps_have_an_input_after_r() {
    let runs = WRITES_PER_PASS + 5;
    let mut log = Log::default();
    for n in 1..=runs {
        let secs = i64::try_from(n).unwrap() * 1_000;
        log.landed(i64::try_from(n).unwrap(), &many_run_id(n), secs);
    }
    let memory = Memory::of(&log);
    let whole = rows_of_every_input(&memory, LATER);
    let mut builder = Builder::default();
    let built = pass(&memory, &mut builder, LATER);
    assert_eq!(built.failure, None);
    // Only those holding R taken first ([`HOLDING_R_PER_PASS`]) are
    // written of the tasks.
    assert!(memory.count(LedgerKind::Task) <= HOLDING_R_PER_PASS);
    assert_eq!(sessions_written_through(&memory).unwrap(), Some(log.last()));
    let r = r_of(&memory);
    assert!(r.events < log.last());
    let lagging = lagging_rows_have_an_input_after(&memory, &whole, r);
    assert!(lagging.contains(&LedgerKind::Task), "{lagging:?}");
    // The tasks left are completed: their rows settled in the fold of
    // every input.
    let settled = whole
        .iter()
        .filter(|((kind, _), row)| *kind == LedgerKind::Task && row.is_final)
        .count();
    assert_eq!(settled, runs);
    drain(&memory, &mut builder, LATER);
    assert!(lagging_rows_have_an_input_after(&memory, &whole, r_of(&memory)).is_empty());
    assert_eq!(r_of(&memory), builder.cursors());

    // Late steps of every run, in their order.
    for n in 1..=runs {
        let run = many_run_id(n);
        let session = format!("s-{n}");
        let secs = i64::try_from(n).unwrap() * 1_000;
        let target = session_of(&memory, &run, &session, secs);
        steps_in(
            &memory,
            &target,
            &session,
            &[Step::new(1, StepKind::ModelTurn, 20_000, 10_000)],
            ms(secs + 20),
        );
    }
    let whole = rows_of_every_input(&memory, LATER);
    let built = pass(&memory, &mut builder, LATER);
    assert_eq!(built.failure, None);
    assert_eq!(built.written, WRITES_PER_PASS);
    let r = r_of(&memory);
    assert_eq!(r.events, log.last());
    assert_eq!(r.steps, i64::try_from(WRITES_PER_PASS).unwrap());
    assert_eq!(builder.steps, i64::try_from(runs).unwrap());
    let stale: Vec<_> = whole
        .iter()
        .filter(|(key, row)| memory.rows.borrow().get(*key) != Some(*row))
        .map(|(key, _)| key.clone())
        .collect();
    assert_eq!(stale.len(), 5, "{stale:?}");
    // Stale, not unwritten.
    assert!(
        stale
            .iter()
            .all(|key| memory.rows.borrow().contains_key(key))
    );
    let lagging = lagging_rows_have_an_input_after(&memory, &whole, r);
    assert_eq!(lagging, BTreeSet::from([LedgerKind::Run]));
    drain(&memory, &mut builder, LATER);
    assert!(lagging_rows_have_an_input_after(&memory, &whole, r_of(&memory)).is_empty());
    assert_eq!(r_of(&memory), builder.cursors());
}

/// The port's own reading of the start of the task rows for a position
/// ([`LedgerStore::ledger_tasks_open_since_at`]) is no later than the one
/// of the rows folded up to that position, and at the position the rows
/// were written at is the start of those not settled; the fake's last
/// event naming a supervisor is the last up to the id naming it.
#[test]
fn the_ports_start_of_the_tasks_for_a_position_is_no_later_than_the_one_there() {
    use crate::domain::ledger::testkit::{built_and_settled, waits_across};
    use crate::domain::pre_claim::rows::Since;
    let (log, position, at) = waits_across();
    let events = &log.events[..position];
    let rows = built_and_settled(events, ms(at));
    let read = std::sync::atomic::AtomicUsize::new(0);
    let store = HeldRows(&rows, &read);
    let p = events.last().unwrap().id;
    assert_eq!(
        store.ledger_tasks_open_since_at(p).unwrap(),
        store.ledger_tasks_open_since().unwrap()
    );
    for at_id in 0..=p.as_i64() {
        let at_id = EventId::new(at_id);
        let read = store.ledger_tasks_open_since_at(at_id).unwrap();
        let upto: Vec<RunEvent> = events.iter().filter(|e| e.id <= at_id).cloned().collect();
        let exact = Since::of(&built_and_settled(&upto, ms(at)));
        assert!(
            read.ms <= exact.ms && read.id <= exact.id,
            "{at_id}: {read:?} {exact:?}"
        );
    }

    let memory = Memory::default();
    memory.events.borrow_mut().extend(events.iter().cloned());
    for upto in 0..=p.as_i64() {
        let upto = EventId::new(upto);
        let expected = events
            .iter()
            .rfind(|e| {
                e.id <= upto && crate::domain::supervisor_life::supervisor_of(e) == Some("d")
            })
            .cloned();
        assert_eq!(
            memory.last_event_naming_supervisor("d", upto).unwrap(),
            expected
        );
    }
}

/// The id of the `push_failed` at `at` (the event just before it).
fn run_failed(log: &Log, at: usize) -> crate::domain::EventId {
    log.events[at - 1].id
}

/// The runs with no row in `store` that a task row names, or that a claim
/// of the task of a legacy task row names (its row keeps the runs it named
/// as first written): on their way where the rows were written, whose
/// events the summaries of the pushes read whatever their tasks.
fn rowless_in(store: &Memory) -> BTreeSet<String> {
    let rows = store.rows.borrow();
    let task_rows = || rows.values().filter(|row| row.kind == LedgerKind::Task);
    let legacy: BTreeSet<TaskId> = task_rows()
        .filter(|row| row.is_legacy())
        .filter_map(|row| row.task_id)
        .collect();
    let claimed: Vec<String> = store
        .events
        .borrow()
        .iter()
        .filter(|event| {
            event.kind == EventKind::RunClaimed
                && event.task_id.is_some_and(|task| legacy.contains(&task))
        })
        .filter_map(|event| event.run_id.as_ref().map(ToString::to_string))
        .collect();
    task_rows()
        .flat_map(crate::domain::ledger::builder::pushes::task_runs)
        .chain(claimed)
        .filter(|run| !rows.contains_key(&(LedgerKind::Run, run.clone())))
        .collect()
}

/// Whether `row` is a queue row of a push alone (its subject a
/// `push_finished`): the queue rows the summaries of the pushes read.
fn a_push_alone(row: &LedgerRow) -> bool {
    row.kind == LedgerKind::Queue
        && row
            .subject
            .as_ref()
            .is_some_and(|subject| subject.kind == EventKind::PushFinished.as_str())
}

/// That making the summaries of the pushes again on `store` for the
/// cursors of `builder` read only what the rows allow (`what`: where):
/// the events whole of the runs with no row a task row or a legacy task's
/// claim names ([`rowless_in`]) and of those with an event up to the cursor
/// after the inputs of their rows; of the queue rows only those of a push
/// alone; the windows of resources only of those runs and of those whose
/// row is not settled at the cursors ([`pushes::settled_at`]): their
/// settling is folded; by id only pushes; and nothing else
/// ([`Memory::other_read`]).
#[track_caller]
fn assert_reads_allowed(store: &Memory, builder: &Builder, what: &str) {
    use crate::domain::ledger::LedgerHead;
    use crate::domain::ledger::builder::pushes;
    let mut allowed = rowless_in(store);
    let rows = store.rows.borrow();
    allowed.extend(
        store
            .events
            .borrow()
            .iter()
            .filter(|event| event.id <= builder.events)
            .filter_map(|event| {
                let run = event.run_id.as_ref()?.to_string();
                rows.get(&(LedgerKind::Run, run.clone()))
                    .filter(|row| event.id > row.inputs_through.events)
                    .map(|_| run)
            }),
    );
    for run in store.runs_read.borrow().iter() {
        assert!(
            allowed.contains(run.as_str()),
            "{what}: the events of {run} read"
        );
    }
    for event in store.read_one.borrow().iter() {
        assert_eq!(
            event.kind,
            EventKind::PushFinished,
            "{what}: {} read by id",
            event.id
        );
    }
    assert_eq!(*store.other_reads.borrow(), [] as [&str; 0], "{what}");
    for key in store.queue_read.borrow().iter() {
        assert!(
            rows.get(&(LedgerKind::Queue, key.clone()))
                .is_some_and(a_push_alone),
            "{what}: the queue row {key} read"
        );
    }
    for run in store.resources_read.borrow().iter() {
        let open = rows
            .get(&(LedgerKind::Run, run.to_string()))
            .is_some_and(|row| {
                !pushes::settled_at(
                    &LedgerHead::of(row),
                    row.is_legacy(),
                    builder.events,
                    builder.resources,
                )
            });
        assert!(
            open || allowed.contains(run.as_str()),
            "{what}: the windows of {run} read"
        );
    }
}

/// The check of what making the summaries of the pushes again reads allows
/// no more than the rows do: the reads the making does pass it, and the
/// events whole of a run whose row was settled, the queue rows of a
/// subject other than a push, the windows of a run whose row was settled,
/// a task's events, every event, a range of events and the session rows
/// each fail it.
#[test]
fn a_read_outside_those_the_rows_allow_fails_the_check_of_the_reads() {
    use crate::domain::ledger::LedgerHead;
    use crate::domain::ledger::builder::pushes::settled_at;
    let (log, now) = pushes_across(2);
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    drain(&memory, &mut builder, now);
    pass(&memory, &mut builder, now);
    let mut made = builder.clone();
    memory.forget_reads();
    restore_pushes(&memory, &mut made, ms(now)).unwrap();
    assert_reads_allowed(&memory, &made, "made again");
    let settled = memory
        .rows
        .borrow()
        .values()
        .find(|row| {
            row.kind == LedgerKind::Run
                && settled_at(&LedgerHead::of(row), false, made.events, made.resources)
        })
        .map(|row| RunId::new(&row.key).unwrap())
        .unwrap();
    assert!(
        memory
            .rows
            .borrow()
            .values()
            .any(|row| row.kind == LedgerKind::Queue && !a_push_alone(row))
    );
    let fails = |read: &dyn Fn()| {
        memory.forget_reads();
        read();
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_reads_allowed(&memory, &made, "outside");
        }))
        .is_err()
    };
    assert!(fails(&|| {
        memory.ledger_run_events(&settled).unwrap();
    }));
    assert!(fails(&|| {
        memory.ledger_rows(LedgerKind::Queue).unwrap();
    }));
    assert!(fails(&|| {
        memory.run_resources(&settled).unwrap();
    }));
    assert!(fails(&|| {
        memory.task_events(TaskId::new(1)).unwrap();
    }));
    assert!(fails(&|| {
        memory.all_events().unwrap();
    }));
    assert!(fails(&|| {
        memory
            .events_between(
                crate::domain::EventId::new(0),
                made.events,
                &EventFilter::default(),
                10,
            )
            .unwrap();
    }));
    assert!(fails(&|| {
        memory.ledger_rows(LedgerKind::Session).unwrap();
    }));
}

/// Made again through the queue service, as the supervisor's store does,
/// the summaries of the pushes are those of the store read directly, and no
/// run or task row crosses whole but the readers' not settled: the reads
/// the store picks from the rows are its own operations. With a few runs
/// settled or many, as many rows, runs' events and windows cross; only the
/// push records and the settled pushes' ids grow.
#[test]
fn the_pushes_made_again_through_the_service_cross_no_row_of_a_settled_run() {
    let received = |settled: usize| {
        let (log, now) = pushes_across(settled);
        let memory = Memory::of(&log);
        let mut builder = Builder::default();
        drain(&memory, &mut builder, now);
        pass(&memory, &mut builder, now);
        let service = WaitRowsService::of(&memory);
        let mut made = builder.clone();
        restore_pushes(&through(service.clone()), &mut made, ms(now)).unwrap();
        let mut direct = builder.clone();
        restore_pushes(&memory, &mut direct, ms(now)).unwrap();
        assert_eq!(pushes_of(&made), pushes_of(&direct));
        assert_eq!(pushes_of(&made).0, pushes_of(&builder).0);
        let ops = service.received.lock().unwrap().clone();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for (op, _) in ops
            .iter()
            .filter(|(op, _)| op != "ledger_push_records_page")
        {
            *counts.entry(op.clone()).or_default() += 1;
        }
        counts
    };
    let (few, many) = (received(3), received(60));
    assert_eq!(few, many);
    for op in few.keys() {
        assert!(
            [
                "ledger_heads_open_at",
                "ledger_runless_pushes",
                "ledger_runs_without_row",
                "ledger_legacy_runs_without_row",
                "ledger_runs_past_their_rows",
                "ledger_row",
                "run_events",
                "run_resources",
                "events_between",
            ]
            .contains(&op.as_str()),
            "{op}"
        );
    }
}

/// Made again through the queue service in pages of the run rows, the
/// push records are read with every run row read once by the pages
/// answered, whatever their number: with a few runs settled or four times
/// as many, a page of more than two rows' answer refused as past the
/// answer's limit, the pages grow with the rows, and the rows they read add
/// up to the run rows, not to the run rows a page. Every read counts, the
/// pages refused too (the store reads their rows before the answer is
/// found too large): they are the steps of asking again with a quarter of
/// the rows, at most five from 1,000 rows to one, so what they read again
/// is a few times the rows at most and does not grow with the pages. The
/// summaries are those of the store read directly.
#[test]
fn the_push_records_made_again_in_pages_read_each_run_row_once() {
    use crate::domain::ledger::builder::pushes::page::PushRecordsPage;
    let read = |settled: usize| {
        let (log, now) = pushes_across(settled);
        let memory = Memory::of(&log);
        let mut builder = Builder::default();
        drain(&memory, &mut builder, now);
        pass(&memory, &mut builder, now);
        let runs: Vec<LedgerRow> = memory
            .rows
            .borrow()
            .values()
            .filter(|row| row.kind == LedgerKind::Run)
            .cloned()
            .collect();
        let row_bytes = runs
            .iter()
            .map(|row| {
                serde_json::to_vec(&PushRecordsPage::of_rows(std::slice::from_ref(row)))
                    .unwrap()
                    .len()
            })
            .max()
            .unwrap();
        let service = WaitRowsService::paged(&memory, 2 * row_bytes);
        let mut made = builder.clone();
        restore_pushes(&through(service.clone()), &mut made, ms(now)).unwrap();
        let mut direct = builder.clone();
        restore_pushes(&memory, &mut direct, ms(now)).unwrap();
        assert_eq!(pushes_of(&made), pushes_of(&direct));
        let pages = service.push_pages.lock().unwrap().clone();
        (runs.len(), pages)
    };
    let (few, many) = (read(10), read(40));
    assert!(many.0 >= 3 * few.0, "{} {}", few.0, many.0);
    let mut answered_pages = Vec::new();
    for (runs, pages) in [&few, &many] {
        let answered: Vec<usize> = pages.iter().filter(|(_, r)| !r).map(|(n, _)| *n).collect();
        let refused: Vec<usize> = pages.iter().filter(|(_, r)| *r).map(|(n, _)| *n).collect();
        assert!(answered.len() >= 2, "{runs} rows: {pages:?}");
        assert_eq!(
            answered.iter().sum::<usize>(),
            *runs,
            "{runs} rows: {pages:?}"
        );
        assert!(refused.len() <= 5, "{runs} rows: {pages:?}");
        assert!(refused.iter().all(|n| n <= runs), "{runs} rows: {pages:?}");
        let total: usize = pages.iter().map(|(n, _)| n).sum();
        assert!(total <= 6 * runs, "{runs} rows: {total} read, {pages:?}");
        answered_pages.push(answered.len());
    }
    assert!(answered_pages[1] > answered_pages[0], "{answered_pages:?}");
    // A page that read every row again would read the rows once a page.
    let (runs, pages) = &many;
    let total: usize = pages.iter().map(|(n, _)| n).sum();
    assert!(
        total * 2 < answered_pages[1] * runs,
        "{runs} rows: {total} read, {pages:?}"
    );
}

/// The summaries of the pushes `builder` holds, its pushes kept and the
/// runs it left pending on their push.
fn pushes_of(
    builder: &Builder,
) -> (
    crate::domain::ledger::builder::pushes::PushesHeld,
    Vec<crate::domain::EventId>,
    BTreeMap<String, Option<TaskId>>,
) {
    (
        builder.kept().pushes_held(),
        builder.kept().pushes_kept(),
        builder.pending_pushes().clone(),
    )
}

/// The summaries of the pushes made again from the run and task rows a
/// build of every event wrote up to a position P, as they were at any
/// position at or before it, are those of a build of every event up to
/// that position: runs pending whose push no push closed, one pending
/// again after it moved on, closing pushes of runs settled (as many before
/// as kept), and of the runs and tasks not settled the pushes they read.
/// At a run's `push_failed` before its `push_pending` it is not pending
/// yet but its failed push is, whether P has no row of it (on its way,
/// read of its events) or its row; so is one that went on its way from a
/// pending push, whose row is of that push; and one on its way with no row
/// whose task had ended (canceled) at the position, read of its events
/// too. A builder that forgets them holds none.
#[test]
fn the_pushes_made_again_from_the_rows_at_a_position_are_those_of_every_event_up_to_it() {
    let (log, now) = pushes_across(2);
    let end = Memory::of(&log);
    drain(&end, &mut Builder::default(), now);
    // The rows as passes write them while the events come in.
    let (going, mut going_builder) = (Memory::default(), Builder::default());
    let (mut on_its_way, mut readers_open, mut stale_with_task_ended) = (0, 0, 0);
    let mut rowless_of_ended_task = 0;
    for at in 0..=log.events.len() {
        going.events.replace(log.events[..at].to_vec());
        drain(&going, &mut going_builder, now);
        let memory = Memory::default();
        memory.events.replace(log.events[..at].to_vec());
        let mut whole = Builder::default();
        drain(&memory, &mut whole, now);
        let every = pushes_of(&whole);
        readers_open += usize::from(!every.1.is_empty());
        let expected = every.clone();
        if at > 0 && log.events[at - 1].kind == EventKind::PushFailed {
            let run = log.events[at - 1].run_id.clone().unwrap();
            assert!(expected.0.failed(run.as_str()));
            assert!(!expected.2.contains_key(run.as_str()));
            let stale = going.ledger_row(LedgerKind::Run, run.as_str()).unwrap();
            on_its_way += usize::from(stale.is_none());
            // Run 4's row is that of its first pending push, its task ended.
            if let Some(stale) = stale {
                assert_eq!(run.as_str(), run_id(4));
                assert!(
                    stale.ended_at.is_none()
                        && stale.records.last().unwrap().id < run_failed(&log, at)
                );
                assert!(going.row(LedgerKind::Task, "4").ended_at.is_some());
                stale_with_task_ended += 1;
            }
            // Run 7 is on its way at the end: no row.
            assert_eq!(
                end.ledger_row(LedgerKind::Run, run.as_str())
                    .unwrap()
                    .is_some(),
                run.as_str() != run_id(7)
            );
        }
        for (rows, name) in [
            (&memory, "P = at"),
            (&going, "P = at, written as it came"),
            (&end, "P = end"),
        ] {
            let mut restored = whole.clone();
            restored.restore_pushes(PushesHeld::default(), Vec::new());
            if expected.0.through.as_i64() > 0 && !expected.2.is_empty() {
                assert_ne!(pushes_of(&restored), expected, "{name} at {at}");
            }
            rows.forget_reads();
            restore_pushes(rows, &mut restored, ms(now)).unwrap();
            assert_reads_allowed(rows, &restored, &format!("{name} at {at}"));
            // Run 4 on its way with no row, its task canceled at the
            // position: read too, as every event folds it.
            let d = run_id(4);
            if rowless_in(rows).contains(&d)
                && log.events[..at]
                    .iter()
                    .any(|event| event.run_id.as_ref().map(RunId::as_str) == Some(d.as_str()))
                && crate::domain::ledger::build(&log.events[..at], ms(now), "")
                    .iter()
                    .any(|row| {
                        row.kind == LedgerKind::Task && row.key == "4" && row.ended_at.is_some()
                    })
            {
                assert!(rows.runs_read.borrow().iter().any(|run| run.as_str() == d));
                rowless_of_ended_task += 1;
            }
            let made = pushes_of(&restored);
            assert_eq!(
                (&made.0, &made.1),
                (&expected.0, &expected.1),
                "{name} at {at}"
            );
            // The runs waiting on a push; one that went on its way from a
            // pending push stays among the whole build's until it is
            // tracked again, and a later push marks it for nothing.
            assert!(
                made.2
                    .iter()
                    .all(|(run, task)| expected.2.get(run) == Some(task))
            );
        }
    }
    // Every run's failure but run 4's second, after its row of its first
    // pending push was written: that row records none of its later events.
    assert_eq!(on_its_way, 2 + 5);
    // Run 7's row, written as the events came, is of its pending push: the
    // rows' records alone say run 6's push closed it, which went back to
    // its landing before; the build of every event says not, and so do the
    // summaries made again, which read its events (past its row).
    let g = run_id(7);
    assert!(crate::domain::ledger::builder::pending_push(
        &going.row(LedgerKind::Run, &g)
    ));
    let rows_alone = PushesHeld::at(going.ledger_push_records().unwrap(), log.last());
    assert!(rows_alone.closed(&g));
    let mut whole = Builder::default();
    drain(&end, &mut whole, now);
    assert!(!whole.kept().pushes_held().closed(&g));
    assert_eq!(stale_with_task_ended, 1);
    assert!(readers_open > 0);
    // Run 4 went on its way from its pending push after its task was
    // canceled: at those positions of the rows built up to each, it has no
    // row there.
    assert!(rowless_of_ended_task > 0);
}

/// A builder that goes on from the summaries of the pushes made again from
/// the rows reads back by id, for a later event of a settled run whose
/// pending push another run's push closed, the push the build of every
/// event reads, and writes the run and task rows it writes; one that
/// forgot them reads none and writes others (the run pending on its push).
#[test]
fn a_builder_going_on_from_the_pushes_made_again_reads_a_settled_runs_push_as_every_event() {
    let (mut log, now) = pushes_across(2);
    let position = log.events.len();
    let settled = [run_id(1), "00000000-3333-4333-8333-333333333333".to_owned()];
    log.add(
        Some(1),
        Some(&settled[0]),
        "note_added",
        json!({}),
        now + 10,
    )
    .add(
        Some(100),
        Some(&settled[1]),
        "note_added",
        json!({}),
        now + 10,
    );
    let going_on = |made: bool| {
        let memory = Memory::default();
        memory.events.replace(log.events[..position].to_vec());
        let mut builder = Builder::default();
        drain(&memory, &mut builder, now);
        if made {
            builder.restore_pushes(PushesHeld::default(), Vec::new());
            restore_pushes(&memory, &mut builder, ms(now)).unwrap();
        }
        memory.events.replace(log.events.clone());
        let read: Vec<Vec<crate::domain::EventId>> = settled
            .iter()
            .map(|run| {
                let events = memory
                    .read(|event| event.run_id.as_ref().map(RunId::as_str) == Some(run.as_str()));
                builder.pushes_to_read(&events)
            })
            .collect();
        drain(&memory, &mut builder, now + 20);
        let rows: BTreeMap<_, _> = memory
            .rows
            .into_inner()
            .into_iter()
            .filter(|((kind, _), _)| matches!(kind, LedgerKind::Run | LedgerKind::Task))
            .collect();
        (read, rows)
    };
    // The build of every event, the one going on from the summaries made
    // again, and one that forgot them.
    let whole = going_on(false);
    assert!(whole.0.iter().all(|read| read.len() == 1), "{:?}", whole.0);
    let made = going_on(true);
    assert_eq!(made.0, whole.0);
    assert_eq!(made.1, whole.1);
    let forgot = {
        let memory = Memory::default();
        memory.events.replace(log.events[..position].to_vec());
        let mut builder = Builder::default();
        drain(&memory, &mut builder, now);
        builder.restore_pushes(PushesHeld::default(), Vec::new());
        memory.events.replace(log.events.clone());
        passes_to_drain(&memory, &mut builder, now + 20);
        memory.row(LedgerKind::Run, &settled[0])
    };
    assert!(forgot.ended_at.is_none());
    assert_ne!(
        Some(&forgot),
        whole.1.get(&(LedgerKind::Run, settled[0].clone()))
    );
}

/// What making the summaries of the pushes again reads of the events (the
/// runs on their way and the pushes kept, by id) is as much whether few or
/// many runs whose pending push a push closed settled before, though the
/// summaries hold more of them; no read of every event.
#[test]
fn the_pushes_made_again_read_as_many_events_however_many_runs_settled() {
    let read_making_them = |settled: usize, of_open_task: usize| {
        let (log, now) = crate::domain::ledger::testkit::pushes_across_with(settled, of_open_task);
        let memory = Memory::of(&log);
        let mut builder = Builder::default();
        drain(&memory, &mut builder, now);
        // Its due drops the pushes of the rows the last pass settled.
        pass(&memory, &mut builder, now);
        if of_open_task > 0 {
            // Task 90 is not settled; its runs are.
            let open = memory
                .ledger_heads_open_at(builder.events, builder.resources)
                .unwrap();
            assert!(
                open.iter()
                    .any(|head| head.kind == LedgerKind::Task && head.key == "90")
            );
            let task = memory.row(LedgerKind::Task, "90");
            assert_eq!(
                crate::domain::ledger::window::runs_named(&task).len(),
                of_open_task
            );
        }
        memory.read.set(0);
        memory.read_all.set(0);
        memory.forget_reads();
        let mut made = builder.clone();
        restore_pushes(&memory, &mut made, ms(now)).unwrap();
        assert_reads_allowed(
            &memory,
            &made,
            &format!("{settled} and {of_open_task} settled"),
        );
        assert_eq!(pushes_of(&made).0, pushes_of(&builder).0);
        (
            (memory.read.get(), memory.read_all.get()),
            builder.kept().pushes_kept().len(),
            builder.kept().size().push_runs,
        )
    };
    // More runs settled whose pending push a push closed, each of its own
    // task, and more runs settled of a task not settled.
    let (few, many) = (read_making_them(3, 0), read_making_them(60, 0));
    assert_eq!(few.0, many.0);
    assert_eq!(few.0.1, 0);
    assert_eq!(few.1, 1);
    assert_eq!(many.1, few.1);
    assert!(many.2 > few.2);
    let (few_of_task, many_of_task) = (read_making_them(0, 3), read_making_them(0, 60));
    assert_eq!(few_of_task.0, many_of_task.0);
    assert_eq!(few_of_task.0, read_making_them(0, 0).0);
}

/// A reader on its way with no row (a run whose pending push another run's
/// push closed and that went back to its landing) reads the push on, as in
/// the build of every event, which writes no row of it and so never
/// settles it; so does it when its task was canceled and settled, its
/// events read as well.
#[test]
fn a_reader_on_its_way_with_no_row_reads_its_push_on_as_every_event() {
    for canceled in [false, true] {
        let (a, b) = (run_id(1), run_id(2));
        let mut log = Log::default();
        log.worked(1, &a, 0)
            .integrated(1, &a, 700)
            .push_failed(1, &a, 910)
            .landed(2, &b, 1000)
            .phase(1, &a, 2000, crate::domain::run_phase::Phase::Landing, "x");
        if canceled {
            log.add(
                Some(1),
                None,
                "task_status_changed",
                json!({"from": "in_progress", "to": "canceled"}),
                2000,
            );
        }
        let now = 2000 + SETTLE_GRACE_MS / 1000 + 100;
        let memory = Memory::of(&log);
        let mut whole = Builder::default();
        drain(&memory, &mut whole, now);
        pass(&memory, &mut whole, now);
        assert!(memory.ledger_row(LedgerKind::Run, &a).unwrap().is_none());
        assert_eq!(memory.row(LedgerKind::Task, "1").is_final, canceled);
        let every = pushes_of(&whole);
        assert_eq!(every.1.len(), 1, "canceled {canceled}");
        let mut made = whole.clone();
        made.restore_pushes(PushesHeld::default(), Vec::new());
        memory.forget_reads();
        restore_pushes(&memory, &mut made, ms(now)).unwrap();
        assert_reads_allowed(&memory, &made, &format!("canceled {canceled}"));
        let restored = pushes_of(&made);
        assert_eq!(*memory.runs_read.borrow(), [RunId::new(&a).unwrap()]);
        assert!(every.0.failed(&a), "canceled {canceled}");
        assert_eq!(
            (&restored.0, &restored.1),
            (&every.0, &every.1),
            "canceled {canceled}"
        );
    }
}

/// A task that completed while its landed run's push was pending has not
/// ended (the push has no end), though its own records alone say it
/// completed: at every position, against the rows a build of every event
/// wrote at the end (the run on its way back to its landing, with no row),
/// the run is read once and its failed push and its wait on it are made
/// again as every event makes them.
#[test]
fn a_task_completed_while_its_rowless_runs_push_is_pending_had_not_ended() {
    let a = run_id(1);
    let mut log = Log::default();
    log.worked(1, &a, 0)
        .integrated(1, &a, 700)
        .push_failed(1, &a, 910)
        .completed(1, 920)
        .phase(1, &a, 2000, crate::domain::run_phase::Phase::Landing, "x");
    let now = 2000 + SETTLE_GRACE_MS / 1000 + 100;
    let end = Memory::of(&log);
    drain(&end, &mut Builder::default(), now);
    assert!(end.ledger_row(LedgerKind::Run, &a).unwrap().is_none());
    assert!(end.row(LedgerKind::Task, "1").ended_at.is_none());
    let mut completed_and_waiting = 0;
    for at in 0..=log.events.len() {
        let upto = &log.events[..at];
        let memory = Memory::default();
        memory.events.replace(upto.to_vec());
        let mut whole = Builder::default();
        drain(&memory, &mut whole, now);
        let every = pushes_of(&whole);
        let completed = upto
            .iter()
            .any(|event| event.kind == EventKind::TaskStatusChanged);
        if completed && every.2.contains_key(a.as_str()) {
            assert!(every.0.failed(&a), "at {at}");
            completed_and_waiting += 1;
        }
        let mut made = whole.clone();
        made.restore_pushes(PushesHeld::default(), Vec::new());
        end.forget_reads();
        restore_pushes(&end, &mut made, ms(now)).unwrap();
        assert_reads_allowed(&end, &made, &format!("at {at}"));
        if upto.iter().any(|event| event.run_id.is_some()) {
            assert_eq!(
                *end.runs_read.borrow(),
                [RunId::new(&a).unwrap()],
                "at {at}"
            );
        }
        let made = pushes_of(&made);
        assert_eq!(made, every, "at {at}");
    }
    // After the completion and before the run went back to its landing.
    assert_eq!(completed_and_waiting, 1);
}

/// A push of a run with no row where the rows were written (stopped
/// between its push and its end) closes the pending push of another task's
/// run, whose task had completed: at a position after that push the run
/// and its task had ended and, past the grace, settled, though the run
/// went back to its landing later (no row either) and its task has not
/// ended where the rows were written. At every position the summaries made
/// again from the rows written at the end fold that push first, so they
/// are those of every event up to it, its readers settled.
#[test]
fn a_push_of_a_run_with_no_row_settles_another_tasks_run_as_every_event() {
    let (a, b) = (run_id(1), run_id(2));
    let settled_by = 1000 + SETTLE_GRACE_MS / 1000 + 10;
    let mut log = Log::default();
    log.worked(1, &a, 0)
        .integrated(1, &a, 700)
        .push_failed(1, &a, 910)
        .completed(1, 920)
        .worked(2, &b, 0)
        .integrated(2, &b, 800)
        .add(
            Some(2),
            Some(&b),
            "push_finished",
            json!({"remote": "origin", "branch": "main", "commit": "c"}),
            1000,
        )
        .phase(
            1,
            &a,
            settled_by + 50,
            crate::domain::run_phase::Phase::Landing,
            "x",
        );
    let now = settled_by + 100;
    let end = Memory::of(&log);
    drain(&end, &mut Builder::default(), now);
    assert!(end.ledger_row(LedgerKind::Run, &a).unwrap().is_none());
    assert!(end.ledger_row(LedgerKind::Run, &b).unwrap().is_none());
    assert!(end.row(LedgerKind::Task, "1").ended_at.is_none());
    let mut closed_and_settled = 0;
    for at in 0..=log.events.len() {
        let upto = &log.events[..at];
        let memory = Memory::default();
        memory.events.replace(upto.to_vec());
        let mut whole = Builder::default();
        drain(&memory, &mut whole, now);
        let every = pushes_of(&whole);
        if every.0.closed(&a) && every.1.is_empty() {
            closed_and_settled += 1;
        }
        let mut made = whole.clone();
        made.restore_pushes(PushesHeld::default(), Vec::new());
        end.forget_reads();
        restore_pushes(&end, &mut made, ms(now)).unwrap();
        assert_reads_allowed(&end, &made, &format!("at {at}"));
        let made = pushes_of(&made);
        assert_eq!((&made.0, &made.1), (&every.0, &every.1), "at {at}");
        assert!(
            made.2
                .iter()
                .all(|(run, task)| every.2.get(run) == Some(task)),
            "at {at}"
        );
    }
    // After the push and before the run went back to its landing.
    assert_eq!(closed_and_settled, 1);
}

/// A run whose row was written waiting on its push goes on its way after
/// another run's push closed that pending push, and its task, completed,
/// gets a note after that: the task's row written at the end reaches past
/// every earlier position, and whether it had ended there is folded on the
/// run's events up to it, not on that row of the run's pending push, which
/// with the closing push says the run and the task ended. At every
/// position, against the rows of a pass after the pending push and one at
/// the end, and against those of a build up to the position or to the
/// end alone, the summaries made again are those of every event up to it,
/// the task reading the push on once the run went on its way.
#[test]
fn a_task_whose_run_went_on_its_way_from_a_closed_pending_push_had_not_ended_as_every_event() {
    let (a, b) = (run_id(1), run_id(2));
    let mut log = Log::default();
    log.worked(1, &a, 0)
        .integrated(1, &a, 700)
        .push_failed(1, &a, 910)
        .completed(1, 920);
    let pending = log.events.len();
    log.landed(2, &b, 1000)
        .phase(1, &a, 1950, crate::domain::run_phase::Phase::Landing, "x");
    let back = log.events.len();
    log.add(Some(1), None, "note_added", json!({}), 1960);
    let now = 1960 + SETTLE_GRACE_MS / 1000 + 100;
    let two_passes = Memory::default();
    let mut builder = Builder::default();
    two_passes.events.replace(log.events[..pending].to_vec());
    drain(&two_passes, &mut builder, now);
    two_passes.events.replace(log.events.clone());
    drain(&two_passes, &mut builder, now);
    assert!(crate::domain::ledger::builder::pending_push(
        &two_passes.row(LedgerKind::Run, &a)
    ));
    let task = two_passes.row(LedgerKind::Task, "1");
    assert!(task.ended_at.is_none());
    assert!(task.inputs_through.events > log.events[back - 1].id);
    let end = Memory::of(&log);
    drain(&end, &mut Builder::default(), now);
    let task = Target::Task(TaskId::new(1));
    let mut read_by_task = 0;
    for at in 0..=log.events.len() {
        let memory = Memory::default();
        memory.events.replace(log.events[..at].to_vec());
        let mut whole = Builder::default();
        drain(&memory, &mut whole, now);
        let every = pushes_of(&whole);
        if at >= back {
            assert!(every.0.readers().contains(&task), "at {at}");
            read_by_task += 1;
        }
        for (rows, name) in [
            (&two_passes, "two passes"),
            (&memory, "P = at"),
            (&end, "P = end"),
        ] {
            assert_made_again_as(rows, &whole, &every, now, &format!("{name} at {at}"));
        }
    }
    assert_eq!(read_by_task, 2);
}

/// That the summaries of the pushes made again on `store` for the cursors
/// of `whole` are `every`, those of `whole`, reading only what is allowed
/// (`what`: where).
#[track_caller]
fn assert_made_again_as(
    store: &Memory,
    whole: &Builder,
    every: &(
        PushesHeld,
        Vec<crate::domain::EventId>,
        BTreeMap<String, Option<TaskId>>,
    ),
    now: i64,
    what: &str,
) {
    let mut made = whole.clone();
    made.restore_pushes(PushesHeld::default(), Vec::new());
    store.forget_reads();
    restore_pushes(store, &mut made, ms(now)).unwrap();
    assert_reads_allowed(store, &made, what);
    let made = pushes_of(&made);
    assert_eq!((&made.0, &made.1), (&every.0, &every.1), "{what}");
    assert!(
        made.2
            .iter()
            .all(|(run, task)| every.2.get(run) == Some(task)),
        "{what}"
    );
}

/// A run whose pending push another run's push closed ended and, with no
/// environment attached by then, settled at a position after that push;
/// an attachment of it after the position, whose last window no store
/// holds, makes its row written at the end wait on its resources (not
/// settled). Made again at every position, whether it had settled is that
/// of its events up to the position and its windows up to the builder's
/// cursor of them, not the row's wait: the summaries are those of every
/// event up to it, the push it read dropped once it and its task settled.
#[test]
fn a_run_attached_after_the_position_had_settled_there_as_every_event() {
    let (a, b) = (run_id(1), run_id(2));
    let settled_by = 1930 + SETTLE_GRACE_MS / 1000 + 10;
    let mut log = Log::default();
    log.worked(1, &a, 0)
        .integrated(1, &a, 700)
        .push_failed(1, &a, 910)
        .completed(1, 920)
        .landed(2, &b, 1000);
    attach(&mut log, &a, settled_by + 50);
    let now = settled_by + 100;
    let end = Memory::of(&log);
    drain(&end, &mut Builder::default(), now);
    let row = end.row(LedgerKind::Run, &a);
    assert!(crate::domain::ledger::builder::resources_pending(&row) && !row.is_final);
    let mut settled_there = 0;
    for at in 0..=log.events.len() {
        let memory = Memory::default();
        memory.events.replace(log.events[..at].to_vec());
        let mut whole = Builder::default();
        drain(&memory, &mut whole, now);
        let every = pushes_of(&whole);
        settled_there += usize::from(every.0.closed(&a) && every.1.is_empty());
        assert_made_again_as(&end, &whole, &every, now, &format!("at {at}"));
    }
    assert!(settled_there > 0);
}

/// A run attached to the host whose pending push another run's push closed
/// ended at a position, its settling waiting on the supervisor's last
/// measure of it: a build whose cursor of the windows has not reached that
/// window holds the push the run read, one whose cursor has drops it. The
/// rows written at the end, past that window (the run's row settled), give
/// the same at every position for either cursor of the builder made again,
/// which reads the run's windows up to its cursor.
#[test]
fn a_run_settled_by_a_window_after_the_cursor_had_not_settled_there_as_every_event() {
    let (a, b) = (run_id(1), run_id(2));
    let mut log = Log::default();
    log.worked(1, &a, 0);
    let attachment = attach(&mut log, &a, 100);
    log.integrated(1, &a, 700)
        .push_failed(1, &a, 910)
        .completed(1, 920)
        .landed(2, &b, 1000);
    let now = 1930 + SETTLE_GRACE_MS / 1000 + 100;
    let window = |memory: &Memory| measured(memory, &a, attachment, (100, 1930), Some(1.0), true);
    let end = Memory::of(&log);
    window(&end);
    drain(&end, &mut Builder::default(), now);
    let row = end.row(LedgerKind::Run, &a);
    assert!(row.is_final && row.inputs_through.resources == Some(1));
    let (mut held_short, mut dropped_past) = (0, 0);
    for with_window in [false, true] {
        for at in 0..=log.events.len() {
            let memory = Memory::default();
            memory.events.replace(log.events[..at].to_vec());
            if with_window {
                window(&memory);
            }
            let mut whole = Builder::default();
            drain(&memory, &mut whole, now);
            assert_eq!(whole.resources, i64::from(with_window));
            let every = pushes_of(&whole);
            if every.0.closed(&a) {
                if with_window {
                    dropped_past += usize::from(every.1.is_empty());
                } else {
                    held_short += usize::from(!every.1.is_empty());
                }
            }
            let what = format!("at {at}, the window read {with_window}");
            assert_made_again_as(&end, &whole, &every, now, &what);
        }
    }
    assert!(held_short > 0 && dropped_past > 0);
}

/// A push of no run (a record of its task's row, or of the queue's with no
/// task) that closes a run's pending push is followed as in the build of
/// every event: at every position, whether the rows are built up to it or
/// to the end, the summaries made again are those of every event up to it,
/// and a later event of the run, settled, reads that push back by id as
/// that build does and writes the rows it writes.
#[test]
fn a_push_of_no_run_closes_a_pending_push_as_every_event() {
    for task in [Some(2), None] {
        let (a, c) = (run_id(1), run_id(3));
        let mut log = Log::default();
        log.worked(1, &a, 0)
            .integrated(1, &a, 700)
            .push_failed_to(1, &a, "late", 910)
            .completed(1, 910)
            .worked(3, &c, 0)
            .integrated(3, &c, 800)
            .push_failed_to(3, &c, "other", 950)
            .add(
                task,
                None,
                "push_finished",
                json!({"remote": "origin", "branch": "late", "commit": "c"}),
                1000,
            );
        let now = 1000 + SETTLE_GRACE_MS / 1000 + 100;
        let what = format!("task {task:?}");
        let end = Memory::of(&log);
        drain(&end, &mut Builder::default(), now);
        let push = log.last();
        assert!(
            end.ledger_runless_pushes()
                .unwrap()
                .iter()
                .any(|event| event.id == push)
        );
        let mut closed = 0;
        for at in 0..=log.events.len() {
            let memory = Memory::default();
            memory.events.replace(log.events[..at].to_vec());
            let mut whole = Builder::default();
            drain(&memory, &mut whole, now);
            let expected = pushes_of(&whole);
            closed += usize::from(expected.0.closed(&a));
            assert!(!expected.0.closed(&c));
            for (rows, name) in [(&memory, "P = at"), (&end, "P = end")] {
                let mut restored = whole.clone();
                restored.restore_pushes(PushesHeld::default(), Vec::new());
                rows.forget_reads();
                restore_pushes(rows, &mut restored, ms(now)).unwrap();
                let where_ = format!("{what}, {name} at {at}");
                assert_reads_allowed(rows, &restored, &where_);
                let made = pushes_of(&restored);
                assert_eq!((&made.0, &made.1), (&expected.0, &expected.1), "{where_}");
            }
        }
        assert_eq!(closed, 1, "{what}");
        // The port's own read of the pushes of no run reads by key the
        // queue row of that push alone, and no other queue row.
        end.forget_reads();
        assert!(
            end.ledger_runless_pushes()
                .unwrap()
                .iter()
                .any(|event| event.id == push)
        );
        assert_eq!(
            !end.queue_read.borrow().is_empty(),
            task.is_none(),
            "{what}"
        );
        assert_reads_allowed(&end, &Builder::default(), &what);
        // A later event of run 1, settled.
        let position = log.events.len();
        log.add(Some(1), Some(&a), "note_added", json!({}), now + 10);
        let going_on = |made: bool| {
            let memory = Memory::default();
            memory.events.replace(log.events[..position].to_vec());
            let mut builder = Builder::default();
            drain(&memory, &mut builder, now);
            pass(&memory, &mut builder, now);
            if made {
                builder.restore_pushes(PushesHeld::default(), Vec::new());
                restore_pushes(&memory, &mut builder, ms(now)).unwrap();
            }
            memory.events.replace(log.events.clone());
            let events =
                memory.read(|event| event.run_id.as_ref().map(RunId::as_str) == Some(a.as_str()));
            let read = builder.pushes_to_read(&events);
            drain(&memory, &mut builder, now + 20);
            let rows: BTreeMap<_, _> = memory
                .rows
                .into_inner()
                .into_iter()
                .filter(|((kind, _), _)| matches!(kind, LedgerKind::Run | LedgerKind::Task))
                .collect();
            (read, rows)
        };
        let whole = going_on(false);
        assert_eq!(whole.0, [push], "{what}");
        let made = going_on(true);
        assert_eq!(made.0, whole.0, "{what}");
        assert_eq!(made.1, whole.1, "{what}");
    }
}

/// A run and a task settled at the position whose rows an input after it
/// reaches (a late note of run 1 and task 1, written at P) are settled as
/// the build of every event up to the position has them, by their folds
/// up to it: the push that closed run 1's pending push is not kept, and
/// the late note reads it back by id as that build does and writes its
/// rows.
#[test]
fn readers_settled_at_the_position_that_a_later_input_reaches_are_settled_as_every_event() {
    let (mut log, now) = pushes_across(2);
    let position = log.events.len();
    let run = run_id(1);
    log.add(Some(1), Some(&run), "note_added", json!({}), now + 10);
    let later = Memory::of(&log);
    drain(&later, &mut Builder::default(), now + 20);
    for target in [
        (LedgerKind::Run, run.clone()),
        (LedgerKind::Task, "1".to_owned()),
    ] {
        let row = later.row(target.0, &target.1);
        assert!(row.is_final && row.inputs_through.events > log.events[position - 1].id);
    }
    let going_on = |made: bool| {
        let memory = Memory::default();
        memory.events.replace(log.events[..position].to_vec());
        let mut builder = Builder::default();
        drain(&memory, &mut builder, now);
        if made {
            builder.restore_pushes(PushesHeld::default(), Vec::new());
            restore_pushes(&later, &mut builder, ms(now)).unwrap();
        }
        let held = pushes_of(&builder);
        memory.events.replace(log.events.clone());
        let events = memory.read(|event| event.run_id.as_ref().map(RunId::as_str) == Some(&run));
        let read = builder.pushes_to_read(&events);
        drain(&memory, &mut builder, now + 20);
        let rows: BTreeMap<_, _> = memory
            .rows
            .into_inner()
            .into_iter()
            .filter(|((kind, _), _)| matches!(kind, LedgerKind::Run | LedgerKind::Task))
            .collect();
        ((held.0, held.1), read, rows)
    };
    let whole = going_on(false);
    assert_eq!(whole.1.len(), 1);
    let made = going_on(true);
    assert_eq!(made.0, whole.0);
    assert_eq!(made.1, whole.1);
    assert_eq!(made.2, whole.2);
}

mod diagnostics;

#[test]
fn finish_wait_bounds_a_pass_without_an_end_deadline() {
    assert_eq!(
        finish_wait(false, Duration::ZERO, None),
        FinishWait::Wait(Duration::from_millis(20))
    );
    assert_eq!(
        finish_wait(false, FINISH_WAIT - Duration::from_millis(1), None),
        FinishWait::Wait(Duration::from_millis(1))
    );
    assert_eq!(finish_wait(false, FINISH_WAIT, None), FinishWait::Leave);
    assert_eq!(
        finish_wait(false, FINISH_WAIT + Duration::from_secs(1), None),
        FinishWait::Leave
    );
}

#[test]
fn finish_wait_obeys_an_earlier_or_new_end_deadline() {
    assert_eq!(
        finish_wait(false, Duration::ZERO, Some(Duration::from_millis(1))),
        FinishWait::Wait(Duration::from_millis(1))
    );
    assert_eq!(
        finish_wait(false, Duration::from_secs(1), Some(Duration::ZERO)),
        FinishWait::Leave
    );
    assert_eq!(
        finish_wait(false, FINISH_WAIT, Some(Duration::from_secs(60))),
        FinishWait::Leave
    );
}

#[test]
fn finish_wait_reaps_completed_passes_even_at_the_boundary() {
    for elapsed in [
        Duration::ZERO,
        FINISH_WAIT,
        FINISH_WAIT + Duration::from_secs(1),
    ] {
        for end in [None, Some(Duration::ZERO)] {
            assert_eq!(finish_wait(true, elapsed, end), FinishWait::Reap);
        }
    }
}

/// Port fakes observe the label at the actual read, rather than inferring
/// it from the operation, including nested restore reads and push records.
#[test]
fn ledger_reads_name_the_work_that_issued_them() {
    let (log, now) = pushes_across(3);
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    drain(&memory, &mut builder, now);
    let _restarted = start_builder(&memory, ms(now)).unwrap();
    read_by_id(&memory, &[log.events[0].id]).unwrap();
    let (sessions, position) = sessions_across();
    let session_memory = drained_to(&sessions, position);
    let mut resumed = Builder::default();
    resumed.events = id_at(&sessions, position);
    restore_sessions(&session_memory, &mut resumed).unwrap();
    let wanted = [(
        LedgerKind::Session,
        "runtime_planner/1/session/1".to_owned(),
    )]
    .into();
    super::restore(&session_memory, &mut resumed, &wanted).unwrap();
    let session_seen = session_memory.callers.borrow();
    assert!(
        session_seen
            .iter()
            .any(|(op, c)| *op == "ledger_row" && *c == Some("ledger.restore")),
        "{session_seen:?}"
    );
    assert!(
        session_seen
            .iter()
            .any(|(op, c)| *op == "ledger_row" && *c == Some("ledger.restore_sessions")),
        "{session_seen:?}"
    );
    let seen = memory.callers.borrow();
    let row_callers: BTreeSet<_> = seen
        .iter()
        .filter(|(op, _)| *op == "ledger_row")
        .filter_map(|(_, caller)| *caller)
        .collect();
    assert!(
        row_callers.contains("ledger.fold_and_write"),
        "{row_callers:?}"
    );
    assert!(
        seen.iter()
            .any(|(op, c)| *op == "ledger_rows_of" && *c == Some("ledger.fold_rows")),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|(_, c)| *c == Some("ledger.start_builder_timed"))
    );
    assert!(
        seen.iter()
            .any(|(op, c)| *op == "events_between" && *c == Some("ledger.read_by_id"))
    );
    drop(seen);
    let service = WaitRowsService::of(&memory);
    restore_pushes(&through(service.clone()), &mut builder, ms(now)).unwrap();
    let seen = service.callers.lock().unwrap();
    assert!(
        seen.iter()
            .any(|(op, c)| op == "ledger_push_records_page" && *c == Some("ledger.restore_pushes")),
        "{seen:?}"
    );
    assert_eq!(
        crate::application::supervisor_service::caller::current(),
        None
    );
}

/// Check only the reads made by the operation under test, after fixture
/// setup: an empty path or an inherited enclosing label cannot pass.
#[track_caller]
fn assert_ledger_row_callers(memory: &Memory, expected: &[&str]) {
    let seen = memory.callers.borrow();
    let callers: BTreeSet<_> = seen
        .iter()
        .filter(|(op, _)| *op == "ledger_row")
        .map(|(_, caller)| *caller)
        .collect();
    assert_eq!(
        callers,
        expected.iter().map(|caller| Some(*caller)).collect(),
        "{seen:?}"
    );
}

#[test]
fn session_folds_name_their_ledger_row_reads() {
    let (log, position) = sessions_across();
    let memory = drained_to(&log, position);
    let mut builder = Builder::default();
    builder.events = id_at(&log, position);
    restore_sessions(&memory, &mut builder).unwrap();
    memory.callers.borrow_mut().clear();
    let folded = fold_sessions(
        &memory,
        &builder,
        (ms(LATER), BUILT),
        &[Target::Row(
            LedgerKind::Session,
            "runtime_planner/1/session/1".into(),
        )],
    )
    .unwrap();
    assert_eq!(
        folded.write.len() + folded.held.len() + folded.behind.len(),
        1
    );
    assert_ledger_row_callers(&memory, &["ledger.fold_sessions"]);
}

#[test]
fn older_wait_folds_name_their_ledger_row_reads() {
    let mut log = Log::default();
    let supervisor = json!({"supervisor": "s", "class": "heavy"});
    log.add(None, None, "slots_full_started", supervisor.clone(), 0)
        .add(None, None, "slots_full_ended", supervisor, 10);
    let memory = Memory::of(&log);
    drain(&memory, &mut Builder::default(), LATER);
    for row in memory.rows.borrow_mut().values_mut() {
        if row.kind == LedgerKind::Queue {
            row.ledger_v = LEDGER_V - 1;
        }
    }
    let mut builder = Builder::default();
    builder.events = log.last();
    assert!(builder.restore_queue_rows(queue_book(&memory, &builder).unwrap()));
    assert!(!builder.older_waits().is_empty());
    memory.callers.borrow_mut().clear();
    fold_older_waits(&memory, &mut builder).unwrap();
    assert_ledger_row_callers(&memory, &["ledger.fold_older_waits"]);
}

#[test]
fn queue_book_cuts_name_their_ledger_row_reads() {
    let (log, position) = queue_across();
    let memory = Memory::of(&log);
    drain(&memory, &mut Builder::default(), LATER);
    memory.callers.borrow_mut().clear();
    // The stored finding has a record after this position, so cutting
    // its head must read its row's records through the port.
    books_at(&memory, id_at(&log, position), ms(LATER)).unwrap();
    assert_ledger_row_callers(&memory, &["ledger.books_of_heads"]);
}

#[test]
fn push_restoration_names_its_own_and_task_runs_ledger_row_reads() {
    let (log, now) = pushes_across(3);
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    drain(&memory, &mut builder, now);
    let expected = pushes_of(&builder);
    builder.restore_pushes(PushesHeld::default(), Vec::new());
    memory.callers.borrow_mut().clear();
    restore_pushes(&memory, &mut builder, ms(now)).unwrap();
    assert_eq!(pushes_of(&builder), expected);
    assert_ledger_row_callers(
        &memory,
        &["ledger.restore_pushes", "ledger.run_and_task_records"],
    );
}
