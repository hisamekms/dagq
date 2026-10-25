//! Read-only views the store assembles and the CLI prints, and the
//! outcomes that travel between the runtime's steps. They are
//! not aggregates, so their fields are public.

use serde::{Deserialize, Serialize};

use super::{
    Ask, CommitSha, EventId, Goal, GoalId, GoalStatus, GoalTag, GoalVerdict, LeaseToken, Priority,
    PrioritySource, PushReport, RecordedKind, RunId, SupervisorMode, Task, TaskId, TaskRun,
    TaskStatus,
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
    /// Ended by a replanning that replaced it with child tasks
    /// (ADR-t1521-2 decision 5); shown only when there is one.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub replaced: usize,
}

const fn is_zero(n: &usize) -> bool {
    *n == 0
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
            TaskStatus::Replaced => &mut self.replaced,
        } += n;
    }
}

/// One row of `goal list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalSummary {
    pub id: GoalId,
    pub title: String,
    pub status: GoalStatus,
    /// What its tasks without one of their own inherit (ADR-t1639-1
    /// decision 1).
    pub priority: Priority,
    /// Its tags (ADR-t1639-1 decision 6).
    pub tags: Vec<GoalTag>,
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
    /// Its base priority and where it comes from (ADR-t1639-1 decision 2).
    pub priority: Priority,
    pub priority_source: PrioritySource,
    /// Who set it (ADR-t1975-1 decision 2): `human` for a person's.
    pub priority_by: super::plan_request::PriorityBy,
}

#[derive(Debug, Clone, Serialize)]
pub struct GoalDetail {
    /// The version of the goal's acceptance, raised by one only when its
    /// text changes; a membership judgement keeps the version it was made
    /// against.
    pub acceptance_version: i64,
    /// Each follow_up whose source goal is this goal, or that belongs to
    /// it or was judged against it (moved ones included): its current goal
    /// and status, its registration material, its `follow_up_depth` and
    /// its judgements as in [`TaskDetail::membership_judgements`].
    pub follow_up_memberships: Vec<serde_json::Value>,
    pub goal: Goal,
    pub closed: bool,
    pub tasks: Vec<GoalTask>,
    /// Unfinished tasks that depend on this goal (ADR-0038), ascending.
    pub dependents: Vec<GoalTask>,
    pub events: Vec<RunEvent>,
}

/// A task's landed commit (its `run_integrated`'s `result_commit`, the
/// `landed_commits` row): what `TaskReads::build_waits` gives the claim's
/// wait for a build that contains the dependencies' landings (ADR-t1632-1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Landing {
    pub task_id: TaskId,
    pub commit: String,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEvent {
    pub id: EventId,
    /// Absent only for goal-level events (`goal_created`, `goal_updated`, `goal_closed`).
    pub task_id: Option<TaskId>,
    pub goal_id: Option<GoalId>,
    pub run_id: Option<RunId>,
    pub kind: RecordedKind,
    pub payload: serde_json::Value,
    pub created_at: String,
    /// Who wrote it (ADR-t728-1 decision 4); `None` for an event written
    /// before the queue recorded actors, or by an older binary.
    /// `events --full`, `show --full` and `show`'s summary of the events
    /// carry it; the summary of `events` and `watch` do not.
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
/// (`since <= created_at < until`). Without `kinds` and without `--all`,
/// `events` sets `kinds` to the attention kinds, so the other fields narrow
/// the attention events only.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EventFilter {
    /// These kinds, attention or not.
    pub kinds: Option<Vec<String>>,
    /// The run's events and those naming it as their `source_run_id`.
    pub run: Option<RunId>,
    pub task: Option<TaskId>,
    /// The goal's events and those of its tasks and their runs.
    pub goal: Option<GoalId>,
    pub since: Option<String>,
    pub until: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskDetail {
    /// The task's follow-up membership judgements, oldest first (the last
    /// is the current one), each with the source goal's current acceptance
    /// version and `needs_recheck`, true when that differs from the version
    /// the judgement was made against.
    pub membership_judgements: Vec<serde_json::Value>,
    pub task: Task,
    pub dependencies: Vec<TaskId>,
    /// Goals the task depends on (ADR-0038), ascending.
    pub goal_dependencies: Vec<GoalId>,
    /// The task that holds this canceled one's content: the one it
    /// duplicates (ADR-0046 decision 5) or that took it (ADR-t947-4
    /// decision 2).
    pub duplicate_of: Option<TaskId>,
    /// The reason its cancel recorded (ADR-t947-4), read off
    /// `duplicate_of` for one recorded before reasons; null for a task not
    /// canceled or one whose cancel recorded neither.
    pub cancel_reason: Option<String>,
    /// The canceled tasks whose `duplicate_of` is this one, ascending.
    pub duplicates: Vec<TaskId>,
    /// The reason of each of `duplicates`: only `duplicate` and
    /// `already_done` make it a duplicate (ADR-t947-4 decision 2).
    pub duplicate_reasons: std::collections::BTreeMap<TaskId, String>,
    pub runs: Vec<TaskRun>,
    pub events: Vec<RunEvent>,
    pub processes: Vec<RunProcess>,
    /// Where the draft came from when the runtime or a job made it
    /// (ADR-t807-1): null for a task a person added.
    pub origin: Option<TaskOrigin>,
    /// The follow_up drafts the receipts and the reviews of this task's runs
    /// proposed, with their status now (ADR-t807-1, ADR-t2352-1).
    pub follow_up_drafts: Vec<FollowUpDraft>,
    /// The draft's revisit time (ADR-t1540-1): when the runtime opens a
    /// planner for it again, or when it did (`opened_at`); null without one.
    /// A draft kept by a `keep_draft` answer without a revisit time is not
    /// shown in `status`: it has this null while `asks` holds that answer.
    pub revisit: Option<crate::domain::DraftRevisit>,
    /// The asks about the task (`task_id`) or one of its runs (`run_id`),
    /// open and closed, oldest first, with the asking AI's recommendation
    /// and confidence (ADR-t451-1 decision 1).
    pub asks: Vec<Ask>,
    /// The replanning operations on its runs (ADR-t1521-2), oldest first,
    /// with where each is and why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub replans: Vec<crate::domain::task_replan::ReplanOperation>,
    /// Its open hold (ADR-t1879-1), if any: why, until when and by whom.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hold: Option<crate::domain::task_hold::TaskHold>,
}

/// Where a draft the runtime or a job made came from (ADR-t807-1): its
/// origin and material, for a follow_up the task and run whose receipt
/// proposed it and which of its follow_ups it was, the key of the bundle it
/// is planned in and the bundles (planners of the runtime's) that took it.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct TaskOrigin {
    pub origin: crate::domain::DraftOrigin,
    pub material: serde_json::Value,
    pub source_task_id: Option<TaskId>,
    pub source_run_id: Option<String>,
    pub index: Option<i64>,
    pub bundle_key: crate::domain::BundleKey,
    pub bundles: Vec<DraftBundleView>,
}

/// A bundle of drafts one planner of the runtime's was opened for
/// (ADR-t807-1), with its drafts and what became of each.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct DraftBundleView {
    pub planner_id: crate::domain::PlannerId,
    pub origin: String,
    pub key_kind: String,
    pub key_value: String,
    pub created_at: i64,
    pub members: Vec<DraftBundleMember>,
}

/// One draft of a bundle: which planner of the runtime's the bundle's is
/// for it, what became of it when the planner ended (null while it works)
/// and its status now.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct DraftBundleMember {
    pub task_id: TaskId,
    pub attempt: i64,
    pub outcome: Option<String>,
    pub proposal_id: Option<i64>,
    pub duplicate_of: Option<TaskId>,
    pub status: String,
}

/// A follow_up draft a receipt or a review of the task's runs proposed
/// (ADR-t807-1, ADR-t2352-1).
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct FollowUpDraft {
    pub run_id: Option<String>,
    /// Its entry's position in the receipt's `follow_ups`; `None` for a
    /// review's, which its document's path and section make one of its run.
    pub index: Option<i64>,
    pub task_id: TaskId,
    pub title: String,
    pub status: String,
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
    pub token: LeaseToken,
    pub pid: u32,
    pub heartbeat_at: i64,
}

/// A resident `supervise` process as it registered itself, whether or not it
/// holds any lease. The row is heartbeated with the leases and deleted on a
/// graceful exit; a row left by a killed supervisor stays until `up`,
/// `down` or a live supervisor of the queue prunes it
/// ([`super::supervisor_prune`]), or a person deals with it (`down --force`). `mode`, `workspace_id` and
/// `binary_version` describe the process itself, so they share the row's
/// lifetime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorRegistration {
    pub token: LeaseToken,
    pub pid: u32,
    pub parallel: u32,
    pub started_at: i64,
    pub heartbeat_at: i64,
    /// Written by the `up` that started this process, once it registered;
    /// `None` for a supervisor started by hand.
    pub mode: Option<SupervisorMode>,
    /// The cmux workspace `supervise` runs in, in [`SupervisorMode::InCmux`]
    /// only (written by an earlier binary; `up` writes none now); `down`
    /// closes it when the supervisor is gone.
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
    /// The limit on the planners its runtime opens at once (ADR-0041
    /// decision 12, task 941); `None` for a supervisor of an older binary,
    /// whose value is not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_planners: Option<u32>,
    /// Where `runtime_planners` comes from; `None` as for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_planners_source: Option<super::slot_limits::SettingSource>,
    /// The seconds it spaces new claims while its load hold is on
    /// (ADR-t1479-1) and where they come from; `None` for a supervisor of
    /// an older binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_spacing: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_spacing_source: Option<super::slot_limits::SettingSource>,
    /// The Spikes that may hold its slots at once (ADR-t1487-2), `None`
    /// for no limit, and where that comes from; the source is `None` for a
    /// supervisor of an older binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_spikes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_spikes_source: Option<super::slot_limits::SettingSource>,
    /// The seconds after which a candidate goes ahead of the other
    /// execution class's (ADR-t1487-2) and where they come from; `None`
    /// for a supervisor of an older binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_aging: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_aging_source: Option<super::slot_limits::SettingSource>,
    /// Its `--max-load` (task 327); `None` when its load hold is off, or
    /// for a supervisor of an older binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_load: Option<f64>,
    /// Where `max_load` comes from: [`SettingSource::Flag`] for
    /// `--max-load` (its 0 or below is `max_load` `None`) or
    /// [`SettingSource::Default`]; `None` for a supervisor of an older
    /// binary, whose source is not known.
    ///
    /// [`SettingSource::Flag`]: super::slot_limits::SettingSource::Flag
    /// [`SettingSource::Default`]: super::slot_limits::SettingSource::Default
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_load_source: Option<super::slot_limits::SettingSource>,
    /// Each provider's executable as the supervisor resolved it at its
    /// start (ADR-t813-2), and the worker modes it runs it in; `None` for
    /// a supervisor of an older binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<Vec<super::worker::ProviderCheck>>,
}

impl SupervisorRegistration {
    /// An explicit provider policy, distinct from missing executables and timed holds.
    pub fn claude_disabled(&self) -> bool {
        self.providers.as_ref().is_some_and(|providers| {
            providers.iter().any(|p| {
                p.provider == super::Provider::Claude
                    && p.error.as_deref() == Some("provider_disabled")
            })
        })
    }

    /// `--parallel N`, `--max-waiting N` and `--runtime-planners N` for an
    /// `up` that starts this supervisor again: each value it took from a
    /// flag, or that a registration of an older binary recorded without a
    /// source; one it took from `dagq.toml` or the default is left for the
    /// started one to resolve again (task 698, task 941).
    pub fn flag_arguments(&self) -> Vec<String> {
        use super::slot_limits::SettingSource;
        let flagged = |source: Option<SettingSource>| {
            source.is_none_or(|source| source == SettingSource::Flag)
        };
        let mut arguments = Vec::new();
        if self.claude_disabled() {
            arguments.push("--no-claude".into());
        }
        if flagged(self.parallel_source) {
            arguments.extend(["--parallel".to_owned(), self.parallel.to_string()]);
        }
        if let Some(max_waiting) = self
            .max_waiting
            .filter(|_| flagged(self.max_waiting_source))
        {
            arguments.extend(["--max-waiting".to_owned(), max_waiting.to_string()]);
        }
        if let Some(runtime_planners) = self
            .runtime_planners
            .filter(|_| flagged(self.runtime_planners_source))
        {
            arguments.extend([
                "--runtime-planners".to_owned(),
                runtime_planners.to_string(),
            ]);
        }
        arguments
    }

    /// The `--max-load` for an `up` that starts this supervisor again: the
    /// value it took from the flag, 0 when the flag disabled its hold.
    /// `None` for the default, which the started one resolves again on its
    /// host, and for a registration with no source (an older binary's, or
    /// one written before the column), whose `max_load` may be the default
    /// as well.
    pub fn flag_max_load(&self) -> Option<f64> {
        (self.max_load_source == Some(super::slot_limits::SettingSource::Flag))
            .then(|| self.max_load.unwrap_or(0.0))
    }

    /// Whether this is a live supervisor that updates its binary (ADR-0045
    /// decision 17): `auto_update`, its pid `alive` and its heartbeat no
    /// older than [`super::HEARTBEAT_TIMEOUT_SECS`]. The one rule by which
    /// an answer to an `update_failed` ask of a build is the runtime's to
    /// apply, both when it is recorded and when `status` reports it; that of
    /// a release's job is [`Self::applies_releases`]'.
    pub fn applies_updates(&self, now: i64, alive: impl Fn(u32) -> bool) -> bool {
        self.auto_update
            && alive(self.pid)
            && now - self.heartbeat_at <= super::HEARTBEAT_TIMEOUT_SECS
    }

    /// Whether this is a live supervisor that installs dagq's releases
    /// (ADR-t618-1 decision 4): it runs a release build (`X.Y.Z`), the
    /// host's `release` is not `off` (`releases_on`), its pid `alive` and
    /// its heartbeat no older than [`super::HEARTBEAT_TIMEOUT_SECS`]. The
    /// rule by which an answer to an `approve_release` ask, or to the
    /// `update_failed` ask of a release's job, is the runtime's to apply; a
    /// development build never applies one.
    pub fn applies_releases(
        &self,
        now: i64,
        alive: impl Fn(u32) -> bool,
        releases_on: bool,
    ) -> bool {
        releases_on
            && self.runs_release()
            && alive(self.pid)
            && now - self.heartbeat_at <= super::HEARTBEAT_TIMEOUT_SECS
    }

    fn runs_release(&self) -> bool {
        self.binary_version
            .as_deref()
            .is_some_and(super::release_update::is_release_build)
    }
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
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
    /// A handoff's drain handed the landing over before its verification
    /// was done again (ADR-t3344-1): the run waits for the landing slot
    /// again, its lease kept, for the next process to land.
    HandedOver {
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

/// The last component of the path of the worktree of a run made before
/// the worktrees had names of their own: every such run's is `worktree`.
pub const LEGACY_WORKTREE_NAME: &str = "worktree";

/// The last component of the path of run `run_id`'s worktree,
/// `worktree-<run id>`. Git names a worktree's record in `.git/worktrees`
/// after it, numbering it on when the name is taken: were every run's
/// called the same, the runs would share `worktree`, `worktree1`… and an
/// add would take the name of a record that went while its directory
/// stayed, pointing that directory's `.git` at another run's record.
pub fn worktree_name(run_id: &str) -> String {
    format!("worktree-{run_id}")
}

/// The last component of the path of the landing recheck's scratch
/// worktree of the queue whose directory is `queue_dir` (its full path,
/// links resolved): `worktree-recheck-<16 hex digits of its SHA-256>`, so
/// the scratches of two queues of one repository have names of their own
/// even when their directories are named alike, and no run's worktree
/// ([`worktree_name`]) has it.
pub fn recheck_scratch_name(queue_dir: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(queue_dir.to_string_lossy().as_bytes());
    let key: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{RECHECK_SCRATCH_PREFIX}{key}")
}

/// How every [`recheck_scratch_name`] starts.
pub const RECHECK_SCRATCH_PREFIX: &str = "worktree-recheck-";

/// Whether `worktree` is a worktree of the run directory `run_dir` (named
/// by its run's ID) by its layout: `<run dir>/worktree-<run id>`, or
/// `<run dir>/worktree` for a run made before.
pub fn is_run_worktree(run_dir: &std::path::Path, worktree: &std::path::Path) -> bool {
    let (Some(parent), Some(name), Some(run)) = (
        worktree.parent(),
        worktree.file_name(),
        run_dir.file_name().and_then(|run| run.to_str()),
    ) else {
        return false;
    };
    parent == run_dir
        && (name == LEGACY_WORKTREE_NAME || name.to_str() == Some(&worktree_name(run)))
}

impl RunPaths {
    pub fn new(runs_dir: &std::path::Path, run_id: &RunId) -> Self {
        let run_dir = runs_dir.join(run_id.as_str());
        Self {
            worktree: run_dir.join(worktree_name(run_id.as_str())),
            receipt: run_dir.join("receipt.json"),
            log: run_dir.join("claude.debug.log"),
            run_dir,
        }
    }

    /// These paths with the worktree in the layout of `stored`, the
    /// worktree's path the queue recorded: `<run dir>/worktree` for a run
    /// made before the worktrees had names of their own, so it is still
    /// found where it was made. Only the last component is read from it,
    /// and only to choose between the two layouts.
    pub fn with_layout_of(self, stored: Option<&str>) -> Self {
        let legacy = stored.is_some_and(|stored| {
            std::path::Path::new(stored).file_name() == Some(LEGACY_WORKTREE_NAME.as_ref())
        });
        if !legacy {
            return self;
        }
        Self {
            worktree: self.run_dir.join(LEGACY_WORKTREE_NAME),
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run's worktree is `<run dir>/worktree-<run id>`, and one made
    /// before, `<run dir>/worktree`, is the run's too; no other name or
    /// directory is.
    #[test]
    fn a_runs_worktree_is_named_by_its_run_or_made_before() {
        let runs = std::path::Path::new("/q/runs");
        let run = RunId::new("r1").unwrap();
        let paths = RunPaths::new(runs, &run);
        assert_eq!(paths.worktree, runs.join("r1/worktree-r1"));
        let run_dir = runs.join("r1");
        assert!(is_run_worktree(&run_dir, &paths.worktree));
        assert!(is_run_worktree(&run_dir, &run_dir.join("worktree")));
        for other in [
            "r1/worktree-r2",
            "r2/worktree-r1",
            "r1/worktree-r1/sub",
            "r1",
        ] {
            assert!(!is_run_worktree(&run_dir, &runs.join(other)), "{other}");
        }
        assert_eq!(
            RunPaths::new(runs, &run)
                .with_layout_of(Some("/old/q/runs/r1/worktree"))
                .worktree,
            run_dir.join("worktree")
        );
        let (a, b) = (
            recheck_scratch_name(std::path::Path::new("/a/queue")),
            recheck_scratch_name(std::path::Path::new("/b/queue")),
        );
        assert!(a.starts_with(RECHECK_SCRATCH_PREFIX), "{a}");
        assert_ne!(a, b);
        assert_eq!(a, recheck_scratch_name(std::path::Path::new("/a/queue")));
        for stored in [
            None,
            Some("/q/runs/r1/worktree-r1"),
            Some("/q/runs/r1/tree"),
        ] {
            assert_eq!(
                RunPaths::new(runs, &run).with_layout_of(stored).worktree,
                paths.worktree,
                "{stored:?}"
            );
        }
    }
}
