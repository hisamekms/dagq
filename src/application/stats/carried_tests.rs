//! The judgments of now on the events a supervisor carries across its
//! passes ([`carried_events`]) against the whole history.

use std::cell::{Cell, RefCell};
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{Value, json};

use super::*;
use crate::application::memory_files::MemoryFiles;
use crate::application::{EndedRunWorkspace, EndedRunWorktree, IdleHook};
use crate::domain::claim_defer::claim_deferrals;
use crate::domain::claim_hold::claim_holds;
use crate::domain::live_alerts::judge;
use crate::domain::{
    CommitSha, EventFilter, EventId, EventKind, GoalId, Provider, RunEvent, RunRecord, TaskId,
};

/// A queue's runs and events that counts its reads of the events: the
/// calls of `all_events`, and the events every other read answered.
struct EventLog {
    runs: Vec<TaskRun>,
    events: RefCell<Vec<RunEvent>>,
    all_events: Cell<usize>,
    read: Cell<usize>,
}

impl EventLog {
    fn new(runs: Vec<TaskRun>, events: Vec<RunEvent>) -> Self {
        Self {
            runs,
            events: RefCell::new(events),
            all_events: Cell::new(0),
            read: Cell::new(0),
        }
    }

    fn answered(&self, events: Vec<RunEvent>) -> Vec<RunEvent> {
        self.read.set(self.read.get() + events.len());
        events
    }
}

#[allow(unused_variables)]
impl RunReads for EventLog {
    fn active_runs(&self) -> Result<Vec<TaskRun>> {
        unreachable!("active_runs is not what the test reaches")
    }
    fn all_runs(&self) -> Result<Vec<TaskRun>> {
        Ok(self.runs.clone())
    }
    fn run(&self, id: &RunId) -> Result<TaskRun> {
        unreachable!("run is not what the test reaches")
    }
    fn runs_with_status(&self, status: RunStatus) -> Result<Vec<TaskRun>> {
        unreachable!("runs_with_status is not what the test reaches")
    }
    fn next_awaiting_integration(&self) -> Result<Option<TaskRun>> {
        unreachable!("next_awaiting_integration is not what the test reaches")
    }
    fn ended_run_workspaces(&self) -> Result<Vec<EndedRunWorkspace>> {
        unreachable!("ended_run_workspaces is not what the test reaches")
    }
    fn ended_run_worktrees(&self) -> Result<Vec<EndedRunWorktree>> {
        unreachable!("ended_run_worktrees is not what the test reaches")
    }
    fn ended_run_worktree(&self, id: &RunId) -> Result<Option<EndedRunWorktree>> {
        unreachable!("ended_run_worktree is not what the test reaches")
    }
    fn latest_runs_in_progress(&self) -> Result<Vec<TaskRun>> {
        unreachable!("latest_runs_in_progress is not what the test reaches")
    }
    fn runs_with_pending_push(&self) -> Result<Vec<TaskRun>> {
        unreachable!("runs_with_pending_push is not what the test reaches")
    }
    fn run_in_workspace(&self, workspace_id: &str) -> Result<Option<RunId>> {
        unreachable!("run_in_workspace is not what the test reaches")
    }
}

#[allow(unused_variables)]
impl EventStore for EventLog {
    fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        unreachable!("update_events is not what the test reaches")
    }
    fn all_events(&self) -> Result<Vec<RunEvent>> {
        self.all_events.set(self.all_events.get() + 1);
        Ok(self.answered(self.events.borrow().clone()))
    }
    fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
        unreachable!("latest_task_events is not what the test reaches")
    }
    fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
        let events = self.events.borrow();
        let of_run = events
            .iter()
            .filter(|event| event.run_id.as_ref() == Some(id))
            .cloned()
            .collect();
        Ok(self.answered(of_run))
    }
    fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool> {
        unreachable!("has_run_event is not what the test reaches")
    }
    fn record_runtime_event(&self, id: &RunId, kind: EventKind, payload: Value) -> Result<()> {
        unreachable!("record_runtime_event is not what the test reaches")
    }
    fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
        unreachable!("last_observe is not what the test reaches")
    }
    fn latest_event_id(&self) -> Result<EventId> {
        Ok(self
            .events
            .borrow()
            .last()
            .map_or(EventId::new(0), |event| event.id))
    }
    fn record_backend_failure(&self, run: Option<&RunId>, payload: Value) -> Result<()> {
        unreachable!("record_backend_failure is not what the test reaches")
    }
    fn record_queue_event(&self, kind: EventKind, payload: Value) -> Result<EventId> {
        unreachable!("record_queue_event is not what the test reaches")
    }
    fn latest_event_of(&self, kind: &str) -> Result<Option<RunEvent>> {
        unreachable!("latest_event_of is not what the test reaches")
    }
    fn latest_events_of(&self, kind: &str, limit: usize) -> Result<Vec<RunEvent>> {
        unreachable!("latest_events_of is not what the test reaches")
    }
    fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>> {
        unreachable!("latest_queue_event is not what the test reaches")
    }
    fn latest_events_by_supervisor(
        &self,
        kinds: &[&str],
        supervisors: &[&str],
    ) -> Result<Vec<RunEvent>> {
        unreachable!("latest_events_by_supervisor is not what the test reaches")
    }
    fn events_of_between(
        &self,
        kinds: &[&str],
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        unreachable!("events_of_between is not what the test reaches")
    }
    fn events_between(
        &self,
        after: EventId,
        upto: EventId,
        filter: &EventFilter,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        assert_eq!(filter, &EventFilter::default());
        let page = self
            .events
            .borrow()
            .iter()
            .filter(|event| event.id > after && event.id <= upto)
            .take(limit)
            .cloned()
            .collect();
        Ok(self.answered(page))
    }
}

/// No process runs: every background wrapper is gone.
struct NoProcesses;

impl ProcessControl for NoProcesses {
    fn alive(&self, _: u32) -> bool {
        false
    }
    fn terminate(&self, _: u32) -> Result<()> {
        unreachable!()
    }
    fn interrupt(&self, _: u32) -> Result<()> {
        unreachable!()
    }
    fn kill(&self, _: u32) -> Result<()> {
        unreachable!()
    }
    fn start_identity(&self, _: u32) -> Option<String> {
        None
    }
}

/// An agent whose marker lists no background work.
struct Signals;

impl AgentSignals for Signals {
    fn idle_hook(&self, _: &[u8]) -> IdleHook {
        IdleHook::default()
    }
}

/// 2026-10-30T00:00:00Z, the day the events are on, in unix seconds.
const DAY: i64 = 1_793_318_400;

fn at(secs: i64) -> String {
    format!(
        "2026-10-30T{:02}:{:02}:{:02}.000Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

fn run_id(n: usize) -> RunId {
    RunId::new(format!("00000000-0000-4000-8000-{n:012}")).unwrap()
}

fn task_run(n: usize, task: i64, status: RunStatus, run_dir: Option<&str>) -> TaskRun {
    TaskRun::restore(RunRecord {
        id: run_id(n),
        task_id: TaskId::new(task),
        status,
        requested_provider: Provider::Claude,
        actual_provider: Provider::Claude,
        worker_mode: crate::domain::worker::WorkerMode::Headless,
        base_commit: CommitSha::parse("0123456789abcdef0123456789abcdef01234567", "base").unwrap(),
        branch: None,
        worktree_path: None,
        workspace_id: None,
        receipt_path: None,
        log_path: None,
        result_commit: None,
        repo_path: None,
        run_dir: run_dir.map(str::to_owned),
        last_error: None,
        workspace_closed_at: None,
        created_at: String::new(),
    })
    .unwrap()
}

/// Events numbered as they are pushed.
#[derive(Default)]
struct Events(Vec<RunEvent>);

impl Events {
    fn push(&mut self, secs: i64, kind: &str, payload: Value, on: (Option<usize>, Option<i64>)) {
        self.0.push(RunEvent {
            id: EventId::new(self.0.len() as i64 + 1),
            task_id: on.1.map(TaskId::new),
            goal_id: None,
            run_id: on.0.map(run_id),
            kind: kind.into(),
            payload,
            created_at: at(secs),
            actor: None,
        });
    }

    fn queue(&mut self, secs: i64, kind: &str, payload: Value) {
        self.push(secs, kind, payload, (None, None));
    }

    fn task(&mut self, secs: i64, task: i64, kind: &str, payload: Value) {
        self.push(secs, kind, payload, (None, Some(task)));
    }

    fn run(&mut self, secs: i64, run: usize, task: i64, kind: &str, payload: Value) {
        self.push(secs, kind, payload, (Some(run), Some(task)));
    }
}

/// The live run, on task 1, and the other run not finished, on task 2.
const LIVE: usize = 1;
const WAITING: usize = 2;

/// A queue with `finished` runs that landed, on tasks from 100 of the live
/// run's goal, behind the history every judgment of now reads: a claim hold
/// that ended and one open, a deferral that a claim ended and one open, an
/// ask answered, a `blocked` ask an integration closed and a `stalled` one
/// open, two thresholds of the supervisors, and the live run's background
/// session and its idle marker without a receipt.
fn queue(finished: usize) -> (Vec<TaskRun>, Vec<RunEvent>) {
    let mut events = Events::default();
    let mut runs = Vec::new();
    for n in 0..finished {
        let (run, task) = (10 + n, 100 + n as i64);
        let start = 60 + n as i64 % 600;
        events.run(start, run, task, "run_claimed", json!({}));
        events.run(
            start + 60 + n as i64 % 7,
            run,
            task,
            "receipt_observed",
            json!({}),
        );
        events.run(start + 70, run, task, "run_integrated", json!({}));
        runs.push(task_run(run, task, RunStatus::Succeeded, None));
    }
    events.queue(
        700,
        "stall_config_loaded",
        json!({"idle_without_receipt_secs": 100}),
    );
    events.queue(
        710,
        "claim_held",
        json!({"supervisor": "a", "reason": "load_average"}),
    );
    events.queue(720, "claim_resumed", json!({"supervisor": "a"}));
    events.task(
        730,
        1,
        "claim_deferred",
        json!({"reason": "hotspot", "files": ["src/a.rs"]}),
    );
    events.task(
        735,
        3,
        "claim_deferred",
        json!({"reason": "hotspot", "files": ["src/b.rs"]}),
    );
    events.task(
        740,
        100,
        "ask_opened",
        json!({"ask_id": 7, "kind": "blocked"}),
    );
    events.run(3600, LIVE, 1, "run_claimed", json!({}));
    events.run(3605, LIVE, 1, "agent_started", json!({}));
    let handle =
        crate::domain::background_wrapper::BackgroundHandle::new(4242, "Fri Oct 30 01:00:06 2026")
            .to_string();
    events.run(
        3606,
        LIVE,
        1,
        "wrapper_launched",
        json!({"workspace_id": handle}),
    );
    events.run(
        3610,
        LIVE,
        1,
        "ask_opened",
        json!({"ask_id": 8, "kind": "worker_question"}),
    );
    events.run(3620, LIVE, 1, "ask_answered", json!({"ask_id": 8}));
    events.run(3630, 10, 100, "run_integrated", json!({}));
    events.run(3640, WAITING, 2, "run_claimed", json!({}));
    events.queue(
        3650,
        "claim_held",
        json!({"supervisor": "b", "reason": "disk_space"}),
    );
    events.queue(
        3660,
        "stall_config_loaded",
        json!({"idle_without_receipt_secs": 1}),
    );
    events.run(
        3700,
        LIVE,
        1,
        "ask_opened",
        json!({"ask_id": 9, "kind": "stalled"}),
    );
    runs.insert(0, task_run(LIVE, 1, RunStatus::Running, Some("/runs/live")));
    runs.push(task_run(WAITING, 2, RunStatus::NeedsSession, None));
    (runs, events.0)
}

fn goals(runs: &[TaskRun]) -> HashMap<TaskId, Option<GoalId>> {
    runs.iter()
        .map(|run| (run.task_id(), Some(GoalId::new(1))))
        .collect()
}

/// What the judgments of now give on `events`: the inputs (its slots set
/// here, as they are not read off the events) and the judgments, `outliers`
/// giving the thresholds of the runs not finished.
fn judged(
    all: &[TaskRun],
    events: &[RunEvent],
    outliers: impl FnOnce(&[LiveRun]) -> HashMap<RunId, i64>,
) -> (LiveInputs, crate::domain::live_alerts::Judged) {
    let files = MemoryFiles::default();
    files.put(
        Path::new("/runs/live/idle.json"),
        UNIX_EPOCH + Duration::from_secs((DAY + 3800) as u64),
        "{}",
    );
    let no_file = || Ok(None);
    let sources = LiveSources {
        files: &files,
        signals: &Signals,
        config_file: &no_file,
    };
    let (runs, positions) = live_runs(all, &NoProcesses, events, &sources).unwrap();
    let inputs = LiveInputs {
        slots: SlotSnapshot {
            free_slots: 2,
            candidates: 1,
            ready: 1,
        },
        config: stall_config(events, &no_file).unwrap(),
        outliers: outliers(&runs),
        runs,
        positions,
    };
    let judged = judge(events, DAY + 7200, &inputs);
    (inputs, judged)
}

/// Folded on passes that each came at another event, the events the
/// judgments read give the inputs, the claim hold and deferrals, the
/// running alerts (idle, outlier and wrapper gone), the workspace check
/// and the outlier thresholds that the whole history gives.
#[test]
fn the_carried_events_judge_as_the_whole_history() {
    let (runs, all) = queue(5);
    let goals = goals(&runs);
    let (whole_inputs, whole) = judged(&runs, &all, |live| outlier_thresholds(&all, &goals, live));
    let kinds: Vec<&str> = whole
        .running_alerts
        .iter()
        .map(|alert| alert.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            "idle_without_receipt",
            "running_outlier",
            "workspace_mismatch"
        ],
        "{whole:?}"
    );
    assert_eq!(whole.slot_alerts[0].kind, "claim_held");
    assert!(!whole_inputs.outliers.is_empty());
    let now_ms = (DAY + 7200) * 1000;
    let (first, last) = (EventId::new(0), EventId::new(i64::MAX));
    let deferred = |events: &[RunEvent]| claim_deferrals(events, first, last, now_ms, |_| true);
    let held = |events: &[RunEvent]| claim_holds(events, first, last, now_ms, |_| true).held;
    assert_eq!(deferred(&all).deferred.len(), 1);
    for cuts in [
        vec![],
        vec![1],
        vec![17, 30],
        vec![all.len() - 1],
        vec![9, 21, 26],
    ] {
        let log = EventLog::new(runs.clone(), Vec::new());
        let mut history = LiveHistory::default();
        for cut in cuts {
            *log.events.borrow_mut() = all[..cut].to_vec();
            carried_events(&log, &mut history).unwrap();
        }
        *log.events.borrow_mut() = all.clone();
        let (read_runs, carried) = carried_events(&log, &mut history).unwrap();
        assert_eq!(read_runs.len(), runs.len());
        assert!(carried.len() < all.len());
        let (inputs, judgments) = judged(&runs, &carried, |live| {
            history.outlier_thresholds(&goals, live)
        });
        assert_eq!(inputs, whole_inputs);
        assert_eq!(judgments, whole);
        assert_eq!(judgments.keys(), whole.keys());
        assert_eq!(held(&carried), held(&all));
        assert_eq!(deferred(&carried).deferred, deferred(&all).deferred);
        assert_eq!(log.all_events.get(), 0);
    }
}

/// After the first pass caught up, a pass reads the events that came
/// since and those of the runs not finished: as many on a queue with few
/// finished runs as on one with many, and never every event.
#[test]
fn a_pass_reads_as_many_events_whatever_the_finished_history() {
    let reads = |finished: usize| {
        let (runs, events) = queue(finished);
        let total = events.len();
        let log = EventLog::new(runs, events);
        let mut history = LiveHistory::default();
        carried_events(&log, &mut history).unwrap();
        // The first pass reads every event once, and the runs not finished.
        let live: usize = log
            .events
            .borrow()
            .iter()
            .filter(|event| matches!(event.run_id.as_ref(), Some(id) if *id == run_id(LIVE) || *id == run_id(WAITING)))
            .count();
        assert_eq!(log.read.get(), total + live);
        let mut more = Events(std::mem::take(&mut *log.events.borrow_mut()));
        more.run(3900, LIVE, 1, "stall_nudged", json!({}));
        more.queue(3910, "claim_resumed", json!({"supervisor": "b"}));
        more.run(3920, WAITING, 2, "resume_started", json!({}));
        *log.events.borrow_mut() = more.0;
        log.read.set(0);
        carried_events(&log, &mut history).unwrap();
        assert_eq!(log.all_events.get(), 0);
        log.read.get()
    };
    let (few, many) = (reads(2), reads(EVENTS_PER_READ / 3 + 10));
    assert_eq!(few, many);
    // The three that came, and the two runs' nine.
    assert_eq!(few, 3 + 9);
}
