//! Read-only views the store assembles and the CLI prints, and the
//! outcomes that travel between the runtime's steps. They are
//! not aggregates, so their fields are public.

use serde::{Deserialize, Serialize};

use super::{
    CommitSha, EventId, Goal, GoalId, GoalStatus, GoalVerdict, PushReport, RunId, SupervisorMode,
    Task, TaskId, TaskRun, TaskStatus,
};

/// Number of a goal's tasks in each status; progress is derived from these.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStatusCounts {
    pub total: usize,
    pub draft: usize,
    /// Waiting for plan review (ADR-0041 decision 8).
    #[serde(default)]
    pub submitted: usize,
    pub ready: usize,
    pub in_progress: usize,
    pub completed: usize,
    pub canceled: usize,
}

impl TaskStatusCounts {
    pub fn count(&mut self, status: TaskStatus, n: usize) {
        self.total += n;
        *match status {
            TaskStatus::Draft => &mut self.draft,
            TaskStatus::Submitted => &mut self.submitted,
            TaskStatus::Ready => &mut self.ready,
            TaskStatus::InProgress => &mut self.in_progress,
            TaskStatus::Completed => &mut self.completed,
            TaskStatus::Canceled => &mut self.canceled,
        } += n;
    }
}

/// One row of `goal list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalSummary {
    pub id: GoalId,
    pub title: String,
    pub status: GoalStatus,
    pub closed: bool,
    pub verdict: Option<GoalVerdict>,
    pub tasks: TaskStatusCounts,
}

/// A task as `goal show` lists it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalTask {
    pub id: TaskId,
    pub title: String,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct GoalDetail {
    pub goal: Goal,
    pub closed: bool,
    pub tasks: Vec<GoalTask>,
    /// Unfinished tasks that depend on this goal (ADR-0038), ascending.
    pub dependents: Vec<GoalTask>,
    pub events: Vec<RunEvent>,
}

/// A direct dependency of a task as the worker's prompt describes it: the
/// predecessor and the run that landed it on `main`. A claimed task's
/// predecessors are all completed, so the run is absent only when the task
/// was completed by hand or its integrated run is gone.
#[derive(Debug, Clone, Serialize)]
pub struct Predecessor {
    pub task: Task,
    pub integrated_run: Option<TaskRun>,
}

/// A goal a task depends on (ADR-0038) as the worker's prompt describes it:
/// the goal and its completed tasks in ID order, each with the run that
/// landed it. A claimed task's goal dependencies are all closed as achieved.
#[derive(Debug, Clone, Serialize)]
pub struct GoalPredecessor {
    pub goal: Goal,
    pub tasks: Vec<Predecessor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunEvent {
    pub id: EventId,
    /// Absent only for goal-level events (`goal_created`, `goal_updated`, `goal_closed`).
    pub task_id: Option<TaskId>,
    pub goal_id: Option<GoalId>,
    pub run_id: Option<RunId>,
    pub kind: String,
    pub payload: serde_json::Value,
    pub created_at: String,
    /// Who wrote it (ADR-t728-1 decision 4); `None` for an event written
    /// before the queue recorded actors, or by an older binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<EventActor>,
}

/// The actor recorded on an event: its role as ADR-t728-1 decision 2
/// spells it (read as text, so a role of a newer binary still reads), its
/// id, and the id of the headless job whose verdict the supervisor applied
/// when the event came of one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventActor {
    pub role: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<String>,
}

/// Which run events `events` reads (ADR-0044 decision 22): every field
/// that is set narrows them. `since` / `until` are queue timestamps
/// (`since <= created_at < until`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventFilter {
    pub kinds: Option<Vec<String>>,
    pub run: Option<RunId>,
    pub task: Option<TaskId>,
    pub goal: Option<GoalId>,
    pub since: Option<String>,
    pub until: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskDetail {
    pub task: Task,
    pub dependencies: Vec<TaskId>,
    /// Goals the task depends on (ADR-0038), ascending.
    pub goal_dependencies: Vec<GoalId>,
    /// The task this one was canceled as a duplicate of (ADR-0046 decision 5).
    pub duplicate_of: Option<TaskId>,
    /// The canceled tasks recorded as duplicates of this one, ascending.
    pub duplicates: Vec<TaskId>,
    pub runs: Vec<TaskRun>,
    pub events: Vec<RunEvent>,
    pub processes: Vec<RunProcess>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunProcess {
    pub run_id: RunId,
    pub role: String,
    pub pid: u32,
    pub heartbeat_at: i64,
    pub exited_at: Option<i64>,
    pub exit_code: Option<i32>,
}

/// A supervisor's ownership of one executing run. The row exists while the
/// supervisor watches the run and heartbeats it; it is deleted when the run
/// comes to rest, when the supervisor gives the run up, or by `recover`.
/// `token` is the owning process's token, shared with its
/// [`SupervisorRegistration`] when the owner is a resident `supervise`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunLease {
    pub run_id: RunId,
    pub token: String,
    pub pid: u32,
    pub heartbeat_at: i64,
}

/// A resident `supervise` process as it registered itself, whether or not it
/// holds any lease. The row is heartbeated with the leases and deleted on a
/// graceful exit; a row left by a killed supervisor stays until `up` prunes
/// it or a person deals with it (`down --force`). `mode`, `workspace_id` and
/// `binary_version` describe the process itself, so they share the row's
/// lifetime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorRegistration {
    pub token: String,
    pub pid: u32,
    pub parallel: u32,
    pub started_at: i64,
    pub heartbeat_at: i64,
    /// Written by the `up` that started this process, once it registered;
    /// `None` for a supervisor started by hand.
    pub mode: Option<SupervisorMode>,
    /// The cmux workspace `supervise` runs in, in [`SupervisorMode::InCmux`]
    /// only; `down` closes it when the supervisor is gone.
    pub workspace_id: Option<String>,
    /// The `dagq` version of the process, written by that process
    /// itself when it registers. `None` is a supervisor that registered
    /// before the column existed; `up` treats it as a version that is not
    /// its own (ADR-0014).
    pub binary_version: Option<String>,
    /// The process takes a handoff (ADR-0045 decision 10): written by a
    /// supervisor that execs another binary when asked to, so `up` and
    /// `install` ask it instead of draining it. `false` for a supervisor of
    /// an older binary.
    #[serde(default)]
    pub handoff_accepted: bool,
    /// The binary this supervisor was asked to exec and has not exec'd yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_binary: Option<String>,
    /// The supervisor builds and installs the runtime of every landing that
    /// changes it (ADR-0045 decision 17): written by `up --auto-update` or a
    /// `supervise --auto-update` that registers, cleared by a plain `up`.
    #[serde(default)]
    pub auto_update: bool,
    /// The limit on the runs it keeps waiting for a person outside its
    /// slots (ADR-0062 decision 7); `None` for a supervisor of an older
    /// binary, which waits in its slots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_waiting: Option<u32>,
    /// Where `parallel` comes from (task 698); `None` for a supervisor of
    /// an older binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_source: Option<super::slot_limits::SettingSource>,
    /// Where `max_waiting` comes from; `None` as for `parallel_source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_waiting_source: Option<super::slot_limits::SettingSource>,
}

impl SupervisorRegistration {
    /// `--parallel N` and `--max-waiting N` for an `up` that starts this
    /// supervisor again: each value it took from a flag, or that a
    /// registration of an older binary recorded without a source; one it
    /// took from `dagq.toml` or the default is left for the started one to
    /// resolve again (task 698).
    pub fn flag_arguments(&self) -> Vec<String> {
        use super::slot_limits::SettingSource;
        let flagged = |source: Option<SettingSource>| {
            source.is_none_or(|source| source == SettingSource::Flag)
        };
        let mut arguments = Vec::new();
        if flagged(self.parallel_source) {
            arguments.extend(["--parallel".to_owned(), self.parallel.to_string()]);
        }
        if let Some(max_waiting) = self
            .max_waiting
            .filter(|_| flagged(self.max_waiting_source))
        {
            arguments.extend(["--max-waiting".to_owned(), max_waiting.to_string()]);
        }
        arguments
    }

    /// Whether this is a live supervisor that updates its binary (ADR-0045
    /// decision 17): `auto_update`, its pid `alive` and its heartbeat no
    /// older than [`super::HEARTBEAT_TIMEOUT_SECS`]. The one rule by which
    /// an answer to an `update_failed` ask is the runtime's to apply, both
    /// when it is recorded and when `status` reports it.
    pub fn applies_updates(&self, now: i64, alive: impl Fn(u32) -> bool) -> bool {
        self.auto_update
            && alive(self.pid)
            && now - self.heartbeat_at <= super::HEARTBEAT_TIMEOUT_SECS
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ClaimOutcome {
    Claimed { run: Box<TaskRun> },
    NoReadyTask,
}

/// Result of one `integrate` invocation. `Integrated` landed the run on
/// `main` (`run.result_commit` is the landed commit); its
/// `verification_skipped` is always false since the verification commands
/// run on every landing (ADR-0023 decision 1), and is kept for the output's
/// shape.
/// `NeedsSession` parked the run for a session to resolve; `Failed` ended it
/// because its rewritten receipt reported `failed`. `NoRunAwaiting` is
/// `--next` on an empty queue. `Integrated` also reports the push of the
/// landed `main` (ADR-0019 decision 3); a failed push leaves the landing as it is.
/// Its `follow_ups` are the draft tasks registered from the landed receipt's
/// `follow_ups` (ADR-0019 decision 4).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum IntegrationOutcome {
    Integrated {
        task: Box<Task>,
        run: Box<TaskRun>,
        #[serde(default)]
        verification_skipped: bool,
        #[serde(default)]
        push: Box<PushReport>,
        #[serde(default)]
        follow_ups: Vec<RegisteredFollowUp>,
    },
    NeedsSession {
        run: Box<TaskRun>,
        main: CommitSha,
        reason: String,
    },
    Failed {
        run: Box<TaskRun>,
        reason: String,
    },
    /// A verification command failed on the host (a full disk, a kill, a
    /// timeout) again after its retry (task 639): the run is back
    /// awaiting integration for a person, and no resume is used.
    Held {
        run: Box<TaskRun>,
        main: CommitSha,
        reason: String,
    },
    NoRunAwaiting,
}

/// A draft task `integrate` registered from one of the landed receipt's
/// `follow_ups` (ADR-0019 decision 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredFollowUp {
    pub task_id: TaskId,
    pub title: String,
}

/// Where a run's files live: `<runs dir>/<run id>/` holds the worktree, the
/// receipt and the provider log. The layout is fixed, so these paths are
/// derived from the run ID and the queue's current `runs/` directory rather
/// than trusted from the database (ADR-0017).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPaths {
    pub run_dir: std::path::PathBuf,
    pub worktree: std::path::PathBuf,
    pub receipt: std::path::PathBuf,
    pub log: std::path::PathBuf,
}

impl RunPaths {
    pub fn new(runs_dir: &std::path::Path, run_id: &RunId) -> Self {
        let run_dir = runs_dir.join(run_id.as_str());
        Self {
            worktree: run_dir.join("worktree"),
            receipt: run_dir.join("receipt.json"),
            log: run_dir.join("claude.debug.log"),
            run_dir,
        }
    }
}
