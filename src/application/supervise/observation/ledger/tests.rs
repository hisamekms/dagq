use std::cell::{Cell, RefCell};

use super::*;
use crate::application::{LedgerStore, NodeSampleStore, SessionOwners, SessionStepStore};
use crate::domain::host_metrics::{HOST_NODE, HostSample, NodeSample, SampleSource};
use crate::domain::ledger::settle::SETTLE_GRACE_MS;
use crate::domain::ledger::testkit::{Log, T0, ms, run_id};
use crate::domain::ledger::{LEDGER_V, LedgerKind, LedgerRow, node};
use crate::domain::session_step::{
    RETENTION_MS, SeqRange, SessionOwner, SessionStep, Step, StepKind, StepTarget, StepsRecorded,
    TargetKind,
};
use crate::domain::{RunEvent, RunId, TaskId};

const BUILT: &str = "2030-01-01T00:00:00.000Z";

/// The stores in memory: the events, the node samples, the steps and the rows, the
/// rows written as `ledger_rows` writes them (a legacy row keeps its
/// values, its records and their range follow), how many writes, and how
/// many events were read and how often all of them.
#[derive(Default)]
struct Memory {
    events: RefCell<Vec<RunEvent>>,
    samples: RefCell<Vec<NodeSample>>,
    steps: RefCell<Vec<SessionStep>>,
    /// The last step id given: ids go on past the steps removed.
    step_id: Cell<i64>,
    owners: RefCell<BTreeMap<String, SessionOwner>>,
    rows: RefCell<BTreeMap<(LedgerKind, String), LedgerRow>>,
    writes: Cell<usize>,
    read: Cell<usize>,
    read_all: Cell<usize>,
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
}

/// Another builder's pass, run on the store between a read and a write.
type Between = Box<dyn FnOnce(&Memory)>;

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
    fn ledger_row(&self, kind: LedgerKind, key: &str) -> Result<Option<LedgerRow>> {
        let row = (kind, key.to_owned());
        ensure!(
            self.broken.borrow().as_ref() != Some(&row),
            "the stored row does not parse"
        );
        Ok(self.rows.borrow().get(&row).cloned())
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
        let between = self.before_steps.borrow_mut().take();
        if let Some(between) = between {
            between(self);
        }
        Ok(self
            .steps
            .borrow()
            .iter()
            .filter(|step| step.id > after)
            .take(limit)
            .cloned()
            .collect())
    }
    fn target_steps(&self, target: &StepTarget) -> Result<Vec<SessionStep>> {
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
        // As the SQLite store: an owner registered before it whose session
        // keeps no step goes too.
        self.owners.borrow_mut().retain(|session, owner| {
            owner.registered_at >= before || held.iter().any(|step| &step.session_id == session)
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

/// One pass at `secs` after [`T0`].
fn pass(memory: &Memory, builder: &mut Builder, secs: i64) -> Built {
    build_pass(memory, builder, ms(secs), BUILT, Retain::default())
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
    let built = build_pass(&memory, &mut builder, ms(0), BUILT, retain);
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
    let removed = build_pass(&memory, &mut builder, ms(1001), BUILT, retain);
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
    let first = build_pass(&memory, &mut builder, ms(LATER), BUILT, retain);
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
    assert_eq!(unwritten, 1);
    let mut built = build_pass(&memory, &mut builder, ms(LATER + 1), BUILT, retain);
    while built.backlog > 0 {
        built = build_pass(&memory, &mut builder, ms(LATER + 2), BUILT, retain);
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
        let other = build_pass(memory, &mut Builder::default(), ms(1000), BUILT, retain);
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
                ms(1002),
                BUILT,
                Retain::default(),
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
    assert_eq!(flow(&from_rows), flow(all));
    assert_eq!(repairs(&from_rows), repairs(all));
    assert_eq!(routes(&from_rows), routes(all));
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
/// stops no other: the pass that fails keeps it in its turn and the next
/// pass starts at the targets after it, so those are written while it
/// stays marked.
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
        assert_eq!(built.backlog, if secs == 10 { 3 } else { 1 }, "{secs}");
    }
    assert_eq!(memory.count(LedgerKind::Task), 2);
    assert!(
        !memory
            .rows
            .borrow()
            .contains_key(&(LedgerKind::Task, "1".to_owned()))
    );
}

mod queue_waits;
mod stream_waits;
mod waits;
