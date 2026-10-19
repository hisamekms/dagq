use std::cell::{Cell, RefCell};

use super::*;
use crate::application::{LedgerStore, NodeSampleStore};
use crate::domain::host_metrics::{HOST_NODE, HostSample, NodeSample, SampleSource};
use crate::domain::ledger::settle::SETTLE_GRACE_MS;
use crate::domain::ledger::testkit::{Log, T0, ms, run_id};
use crate::domain::ledger::{LEDGER_V, LedgerKind, LedgerRow, node};
use crate::domain::{RunEvent, RunId, TaskId};

const BUILT: &str = "2030-01-01T00:00:00.000Z";

/// The stores in memory: the events, the node samples and the rows, the
/// rows written as `ledger_rows` writes them (a legacy row keeps its
/// values, its records and their range follow), how many writes, and how
/// many events were read and how often all of them.
#[derive(Default)]
struct Memory {
    events: RefCell<Vec<RunEvent>>,
    samples: RefCell<Vec<NodeSample>>,
    rows: RefCell<BTreeMap<(LedgerKind, String), LedgerRow>>,
    writes: Cell<usize>,
    read: Cell<usize>,
    read_all: Cell<usize>,
    /// Refuses every write of a row.
    refuses: Cell<bool>,
}

impl Memory {
    fn of(log: &Log) -> Self {
        let memory = Self::default();
        memory.events.replace(log.events.clone());
        memory
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
        self.read_all.set(self.read_all.get() + 1);
        Ok(self.read(|_| true))
    }
    fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
        unreachable!()
    }
    fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
        Ok(self.read(|event| event.run_id.as_ref() == Some(id)))
    }
    fn task_events(&self, task: TaskId) -> Result<Vec<RunEvent>> {
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
        Ok(read)
    }
}

impl LedgerStore for Memory {
    fn write_ledger_row(&self, row: &LedgerRow) -> Result<bool> {
        ensure!(!self.refuses.get(), "attempt to write a readonly database");
        self.writes.set(self.writes.get() + 1);
        let mut rows = self.rows.borrow_mut();
        let key = (row.kind, row.key.clone());
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
    fn ledger_row(&self, kind: LedgerKind, key: &str) -> Result<Option<LedgerRow>> {
        Ok(self.rows.borrow().get(&(kind, key.to_owned())).cloned())
    }
    fn ledger_rows(&self, kind: LedgerKind) -> Result<Vec<LedgerRow>> {
        Ok(self
            .rows
            .borrow()
            .values()
            .filter(|row| row.kind == kind)
            .cloned()
            .collect())
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
        source: SampleSource,
        apart_secs: i64,
    ) -> Result<bool> {
        unreachable!()
    }
    fn import_node_samples(
        &self,
        node: &str,
        file: &str,
        day: i64,
        samples: &[HostSample],
        bytes: u64,
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
        Ok(self
            .samples
            .borrow()
            .iter()
            .filter(|s| s.node_id == node && (from..=until).contains(&s.sample.unix))
            .cloned()
            .collect())
    }
    fn node_samples_after(&self, after: i64) -> Result<Vec<NodeSample>> {
        Ok(self
            .samples
            .borrow()
            .iter()
            .filter(|s| s.id > after)
            .cloned()
            .collect())
    }
}

/// One pass at `secs` after [`T0`].
fn pass(memory: &Memory, builder: &mut Builder, secs: i64) -> Built {
    build_pass(memory, builder, ms(secs), BUILT, None)
}

/// A long time after every run here, past every grace.
const LATER: i64 = 1_000_000;

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
    let second = pass(&memory, &mut builder, LATER);
    assert_eq!(second.written, runs * 2 - WRITES_PER_PASS);
    assert_eq!(second.backlog, 0);
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
    assert_eq!(caught.written, 4);
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
    assert_eq!(built.written, 2, "run a and its task");
    assert_eq!(memory.row(LedgerKind::Run, &a).ledger_v, LEDGER_V);
    let kept = memory.row(LedgerKind::Run, &d);
    assert_eq!(kept.ledger_v, LEDGER_V - 1);
    assert_eq!(kept.legacy, legacy.legacy);
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
    assert_eq!(failed.backlog, 4);
    assert!(memory.rows.borrow().is_empty());
    memory.refuses.set(false);
    assert_eq!(pass(&memory, &mut builder, LATER).written, 4);
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
