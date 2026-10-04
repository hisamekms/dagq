//! The traits the use cases reach the queue, Git, the agent, cmux, the
//! service manager, processes, time and IDs through. The infrastructure
//! implements them and the entry points inject the implementations.

use anyhow::{Context, Result};
use serde::Serialize;
use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fmt, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use super::{GraphInput, TaskPage, TaskQuery, timestamp, unix_seconds};
use crate::domain::{
    Ask, AskId, AskKind, AskOutcome, ClaimOutcome, CommitSha, DraftOrigin, DraftTarget, EventId,
    EventKind, Finding, FindingId, FindingStatus, FindingView, Goal, GoalDetail, GoalEdit, GoalId,
    GoalPredecessor, GoalSummary, GoalVerdict, LeaseToken, LintInput, NewAsk, NewGoal, NewNote,
    NewTask, NotePage, NoteQuery, PlanReviewCandidate, PlanReviewDecision, PlanReviewVerdict,
    PlannerId, PlannerOrigin, PlannerSession, Predecessor, Priority, Proposal, ProposalId, Reason,
    RunEvent, RunId, RunLease, RunPlan, RunProcess, RunStatus, SessionRole, StrandedDependency,
    Submission, SupervisorMode, SupervisorRegistration, Task, TaskAction, TaskChange, TaskDetail,
    TaskEdit, TaskId, TaskRun, TaskStatus,
    goal_review::{GoalReviewDecision, GoalReviewVerdict},
    related::RelatedPage,
    search::{SearchPage, SearchQuery},
};

pub trait TaskStore {
    fn add(&mut self, task: NewTask) -> Result<Task>;
    /// One page of tasks matching `query`, newest first.
    fn list(&self, query: &TaskQuery) -> Result<TaskPage>;
    fn show(&mut self, task_id: TaskId) -> Result<TaskDetail>;
    fn transition(&mut self, task_id: TaskId, action: TaskAction) -> Result<Task>;
    /// Cancel a task as a duplicate of another (ADR-0046 decision 5),
    /// recording which in its `task_status_changed`. The other task must
    /// exist, differ and not be canceled.
    fn cancel_duplicate(&mut self, task_id: TaskId, duplicate_of: TaskId) -> Result<Task>;
    fn add_dependency(&mut self, task_id: TaskId, predecessor_id: TaskId) -> Result<()>;
    fn remove_dependency(&mut self, task_id: TaskId, predecessor_id: TaskId) -> Result<()>;
    /// Make a draft or ready task wait until `goal_id` is closed as achieved
    /// (ADR-0038); never its own goal, never a cycle.
    fn add_goal_dependency(&mut self, task_id: TaskId, goal_id: GoalId) -> Result<()>;
    fn remove_goal_dependency(&mut self, task_id: TaskId, goal_id: GoalId) -> Result<()>;
    /// Dependency-ready tasks in claim order (ADR-0040 decision 4); each
    /// task is limited to one unfinished run.
    fn candidates(&self) -> Result<Vec<Task>>;
    /// The unfinished tasks with their direct predecessors and the IDs of
    /// `candidates`, read in one snapshot.
    fn graph_input(&self) -> Result<GraphInput>;
    /// Reserve one run atomically, without a lease. Does not start a process or validate Git objects.
    fn claim(&mut self, base_commit: &CommitSha) -> Result<ClaimOutcome>;
    /// Direct predecessors of a task, each with the run that landed it, in ID order.
    fn predecessors(&self, task_id: TaskId) -> Result<Vec<Predecessor>>;
    /// Goals a task depends on, in ID order, each with its completed tasks
    /// and the runs that landed them.
    fn goal_predecessors(&self, task_id: TaskId) -> Result<Vec<GoalPredecessor>>;
    /// Tasks that are `in_progress` right now, in ID order.
    fn tasks_in_progress(&self) -> Result<Vec<Task>>;
    fn add_goal(&mut self, goal: NewGoal) -> Result<Goal>;
    /// Every goal in ID order with its task counts by status.
    fn list_goals(&self) -> Result<Vec<GoalSummary>>;
    fn show_goal(&mut self, goal_id: GoalId) -> Result<GoalDetail>;
    /// Replace the given fields; running runs keep their prompt snapshot.
    fn edit_goal(&mut self, goal_id: GoalId, edit: GoalEdit) -> Result<Goal>;
    /// Record the verdict once. `achieved` is refused while a task is not
    /// completed or canceled; `abandoned` while a task is in progress.
    fn close_goal(&mut self, goal_id: GoalId, verdict: GoalVerdict) -> Result<Goal>;
    /// Move a draft or ready task to an open goal, or to none.
    fn set_goal(&mut self, task_id: TaskId, goal_id: Option<GoalId>) -> Result<Task>;
    /// Replace the globs of the paths a draft or ready task may change
    /// (ADR-0029); an empty list removes the limit.
    fn set_paths(&mut self, task_id: TaskId, paths: Vec<String>) -> Result<Task>;
    /// Replace the given fields of a draft task (ADR-0041 decision 9),
    /// recording `task_edited` with the fields that changed; running runs
    /// keep their prompt snapshot. `authorized` is the status the caller
    /// authorized the edit with; a task whose status differs in the
    /// transaction is refused unchanged (ADR-t883-1).
    fn edit_task(
        &mut self,
        task_id: TaskId,
        edit: TaskEdit,
        authorized: TaskStatus,
    ) -> Result<Task>;
    /// Give a draft or ready task another priority (ADR-0040 decision 4);
    /// it takes effect at the next claim.
    fn set_priority(&mut self, task_id: TaskId, priority: Priority) -> Result<Task>;
    /// Bundle draft tasks, the draft tasks of the given goals and those
    /// goals into a proposal and submit it for plan review (ADR-0041
    /// decisions 7, 8): the tasks become `submitted`, which no claim takes.
    /// With a proposal ID, submit that proposal again after a revise,
    /// with the drafts it holds. A task or goal of another active proposal
    /// is refused.
    fn submit(&mut self, submission: Submission) -> Result<Proposal>;
    /// The plan-review path to `ready` (ADR-0041 decisions 8, 11): the
    /// submitted proposal is accepted, its submitted tasks become ready and
    /// its draft goals open.
    fn approve_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal>;
    /// Plan review sends the submitted proposal back to its planner: its
    /// submitted tasks return to draft.
    fn send_back_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal>;
    /// Withdraw a submitted or revising proposal: it ends as canceled, its
    /// submitted tasks return to draft, and its tasks and goals are free to
    /// join another proposal.
    fn withdraw_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal>;
    fn show_proposal(&self, proposal_id: ProposalId) -> Result<Proposal>;
    /// The submitted and revising proposals, oldest submission first; with
    /// `all`, every proposal.
    fn proposals(&self, all: bool) -> Result<Vec<Proposal>>;
    /// What `lint` checks `tasks` against (ADR-0041 decision 10), read in
    /// one snapshot: the tasks in the order given, every task's status and
    /// dependencies, and every goal's verdict. A missing task is an error.
    fn lint_input(&self, tasks: &[TaskId]) -> Result<LintInput>;
    /// Open a draft goal so its tasks become candidates (ADR-0024 decision 5).
    fn ready_goal(&mut self, goal_id: GoalId) -> Result<Goal>;
    /// Record a note as an `observation` run event on its task, run or goal.
    fn add_note(&mut self, note: NewNote) -> Result<RunEvent>;
    /// One page of notes, oldest first.
    fn notes(&self, query: &NoteQuery) -> Result<NotePage>;
}

/// The directory under a run's directory the runtime gives a Codex
/// worker's turns as their `TMPDIR` (task 1290), and removes with the
/// run's leftovers once its task is over.
pub const RUN_TMP_DIR: &str = "tmp";

/// A process to start: its program, arguments, environment changes and
/// working directory, built like a command and started by a [`Spawner`],
/// which also decides where its standard streams go.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandSpec {
    program: OsString,
    args: Vec<OsString>,
    /// In the order given; `None` removes the variable.
    envs: Vec<(OsString, Option<OsString>)>,
    current_dir: Option<PathBuf>,
    /// Start it in a session and process group of its own, so neither a
    /// signal to the starter's group nor the close of its terminal reaches
    /// it (the automatic update's job, ADR-0045 decision 13).
    new_session: bool,
    /// What the process reads on its standard input, whatever the
    /// [`Streams`] say: a headless job's prompt, which on the command line
    /// could pass the system's limit on the arguments (`E2BIG`, task 1560).
    stdin: Option<String>,
}

impl CommandSpec {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            ..Self::default()
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.arg(arg);
        }
        self
    }

    /// Add options `args` before the `--` that ends the options given so
    /// far, or after them all when there is none.
    pub fn option_args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let at = self
            .args
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(self.args.len());
        let rest = self.args.split_off(at);
        self.args(args);
        self.args.extend(rest);
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.envs
            .push((key.as_ref().to_owned(), Some(value.as_ref().to_owned())));
        self
    }

    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        for (key, value) in vars {
            self.env(key, value);
        }
        self
    }

    /// Take out the first two consecutive arguments `first` `second`;
    /// whether they were there.
    pub fn remove_arg_pair(&mut self, first: impl AsRef<OsStr>, second: impl AsRef<OsStr>) -> bool {
        let (first, second) = (first.as_ref(), second.as_ref());
        let at = self
            .args
            .windows(2)
            .position(|pair| pair[0] == first && pair[1] == second);
        if let Some(at) = at {
            self.args.drain(at..at + 2);
        }
        at.is_some()
    }

    /// The process does not inherit `key`.
    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.envs.push((key.as_ref().to_owned(), None));
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.current_dir = Some(dir.as_ref().to_owned());
        self
    }

    /// See [`Self::get_new_session`].
    pub fn new_session(&mut self) -> &mut Self {
        self.new_session = true;
        self
    }

    /// Whether the process starts in a session of its own.
    pub fn get_new_session(&self) -> bool {
        self.new_session
    }

    /// See [`Self::get_stdin`].
    pub fn stdin(&mut self, text: impl Into<String>) -> &mut Self {
        self.stdin = Some(text.into());
        self
    }

    /// What the process reads on its standard input instead of the input
    /// its [`Streams`] give; `None` leaves it to them.
    pub fn get_stdin(&self) -> Option<&str> {
        self.stdin.as_deref()
    }

    pub fn get_program(&self) -> &OsStr {
        &self.program
    }

    pub fn get_args(&self) -> impl Iterator<Item = &OsStr> {
        self.args.iter().map(OsString::as_os_str)
    }

    /// Every change to the environment in the order given; `None` removes.
    pub fn get_envs(&self) -> impl Iterator<Item = (&OsStr, Option<&OsStr>)> {
        self.envs
            .iter()
            .map(|(key, value)| (key.as_os_str(), value.as_deref()))
    }

    pub fn get_current_dir(&self) -> Option<&Path> {
        self.current_dir.as_deref()
    }
}

/// The standard input of a process ([`CommandSpec::get_stdin`]) could not
/// be prepared (its file not made, written or removed): the starter's
/// environment (its `TMPDIR` missing, unwritable or full), not the program,
/// so [`job_start_failure`] takes it for no provider's (task 1560). `what`
/// says which step and file, `source` why.
#[derive(Debug)]
pub struct StdinUnprepared {
    pub what: String,
    pub source: io::Error,
}

impl fmt::Display for StdinUnprepared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.what)
    }
}

impl std::error::Error for StdinUnprepared {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Where the standard streams of a started process go.
#[derive(Debug, Clone, Copy)]
pub enum Streams<'a> {
    /// The starting process's own: the session wrapper's terminal.
    Inherit,
    /// Nowhere.
    Null,
    /// No input (unless the command has its own,
    /// [`CommandSpec::get_stdin`]); stdout and stderr to these files,
    /// created or truncated.
    Files { stdout: &'a Path, stderr: &'a Path },
    /// No input; stdout and stderr both to this one file, created or
    /// truncated.
    Log(&'a Path),
}

/// How a process ended: `description` as the operating system words it
/// (`exit status: 1`), `code` absent when a signal ended it, and `signal`
/// the signal then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exit {
    pub success: bool,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub description: String,
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.description)
    }
}

/// A process a [`Spawner`] started.
pub trait Spawned: Send {
    fn id(&self) -> u32;
    /// `None` while it runs.
    fn try_wait(&mut self) -> Result<Option<Exit>>;
    fn kill(&mut self) -> Result<()>;
    fn wait(&mut self) -> Result<Exit>;
    /// Kill it with every process in its group, for one started in a
    /// session of its own ([`CommandSpec::new_session`]): a headless turn
    /// is stopped with what it runs (ADR-t813-1 decision 3). One started
    /// otherwise is killed alone.
    fn kill_group(&mut self) -> Result<()> {
        self.kill()
    }
}

/// Starts processes: the agent under the session wrapper, the headless
/// review and triage jobs, and the observer.
pub trait Spawner: Send + Sync {
    fn spawn(&self, command: &CommandSpec, streams: Streams<'_>) -> Result<Box<dyn Spawned>>;
}

/// The files of the runs (the run directory, its prompt, the receipt, the
/// idle marker and `review.md`) and of the queue directory (`rebind`'s log
/// and `repository` file) as the use cases read and write them. Errors are
/// the operating system's, unchanged.
pub trait RunFiles: Send + Sync {
    /// Create `dir` and every missing parent.
    fn create_dir_all(&self, dir: &Path) -> io::Result<()>;
    /// Create `dir`, which must not exist yet.
    fn create_new_dir(&self, dir: &Path) -> io::Result<()>;
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()>;
    fn copy(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// The bytes of `path` from `offset` on (none past its end): what a
    /// process appended since the last read.
    fn read_from(&self, path: &Path, offset: u64) -> io::Result<Vec<u8>> {
        let bytes = self.read(path)?;
        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        Ok(bytes.get(offset..).unwrap_or_default().to_vec())
    }
    /// At most `len` bytes of `path` from `offset` on (none past its end):
    /// a bounded part of a file that may be larger than one read takes,
    /// such as a background session's log.
    fn read_range(&self, path: &Path, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let mut bytes = self.read_from(path, offset)?;
        bytes.truncate(usize::try_from(len).unwrap_or(usize::MAX));
        Ok(bytes)
    }
    /// The last `bytes` bytes of `path` (all of a shorter file), read
    /// without the whole of a large file such as an agent's debug log.
    fn read_tail(&self, path: &Path, bytes: u64) -> io::Result<Vec<u8>> {
        let all = self.read(path)?;
        let from = all
            .len()
            .saturating_sub(usize::try_from(bytes).unwrap_or(usize::MAX));
        Ok(all[from..].to_vec())
    }
    /// How many bytes `path` holds now, without reading them.
    fn size(&self, path: &Path) -> io::Result<u64> {
        self.read(path).map(|bytes| bytes.len() as u64)
    }
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    /// Take an exclusive lock on `path` (created if missing) without
    /// waiting, held across processes until the returned guard drops;
    /// `None` while another holder has it. A store with nothing to share
    /// between processes always grants it.
    fn try_lock(&self, path: &Path) -> io::Result<Option<Box<dyn std::any::Any + Send>>> {
        let _ = path;
        Ok(Some(Box::new(())))
    }
    /// When the file was last written.
    fn modified(&self, path: &Path) -> io::Result<SystemTime>;
    /// The modification time and the bytes of one open file, so both
    /// belong to the same write; `None` when there is no file.
    fn read_stamped(&self, path: &Path) -> Result<Option<(SystemTime, Vec<u8>)>>;
    fn is_file(&self, path: &Path) -> bool;
    fn is_dir(&self, path: &Path) -> bool;
    fn exists(&self, path: &Path) -> bool;
    /// The paths of the entries of `dir`, in no particular order.
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// The bytes the directory `dir` takes on disk, each file counted once
    /// however many links it has, without following links; `None` when
    /// `dir` is not a directory (missing, a file or a link).
    fn tree_size(&self, dir: &Path) -> io::Result<Option<u64>>;
    /// Remove the directory `dir` and everything under it.
    fn remove_dir_all(&self, dir: &Path) -> io::Result<()>;
    /// Append `line` and a newline to `path`, creating it if missing.
    fn append_line(&self, path: &Path, line: &str) -> io::Result<()>;
    /// The absolute path with every link resolved; an error when it does
    /// not exist.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    /// Write `text`, then the bytes of the file `body` as a block fenced
    /// with one backtick more than its longest backtick run (three at
    /// least) and labelled `info`, to a new file at `path`, and sync it.
    /// `body` is read in chunks, never whole. Local run material is limited
    /// to 64 MiB; links and special files are refused, and the destination
    /// is published by atomic rename.
    fn write_fenced(&self, path: &Path, text: &str, info: &str, body: &Path) -> Result<()>;
    /// The wall clock that stamps the files: a time compared with a
    /// file's modification time is read here, not from the [`Clock`].
    fn now(&self) -> SystemTime;
}

/// What an entry of an agent's directory is, without following a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Link,
    /// A FIFO, a socket, a device.
    Other,
}

impl EntryKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "a regular file",
            Self::Dir => "a directory",
            Self::Link => "a symbolic link",
            Self::Other => "a special file",
        }
    }
}

/// Opens connections to the queue: the supervisor's own, and one for each
/// thread that works beside its loop (the heartbeat, validations,
/// landings).
pub trait QueueOpener: Send + Sync {
    fn open(&self) -> Result<Box<dyn Queue + Send>>;
}

/// How long a connection to the queue waits for another connection's
/// write lock before its statement fails as busy.
pub const QUEUE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The context of an error a write met because another connection held the
/// queue's lock past [`QUEUE_BUSY_TIMEOUT`] (SQLite's busy or locked): a
/// passing condition, which the same write may try again (task 1119).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueBusy;

impl fmt::Display for QueueBusy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the queue database is locked by another writer")
    }
}

impl QueueBusy {
    /// Whether `error` is one the queue met as busy.
    pub fn is(error: &anyhow::Error) -> bool {
        error.downcast_ref::<QueueBusy>().is_some()
    }
}

/// What one heartbeat wrote: whether the process's supervisor registration
/// was there to refresh, and how many run leases it refreshed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatWrite {
    pub registered: bool,
    pub leases: usize,
}

/// Whether an agent's sessions load a plugin (ADR-t617-2 decision 4), as
/// [`AgentProvider::installed_plugin`] finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginState {
    /// Installed and enabled.
    Enabled,
    /// Installed, but disabled: the IDs it is installed under.
    Disabled(Vec<String>),
    /// Not installed.
    Missing,
}

/// One command of an update of the installed dagq plugin and what it
/// printed (ADR-t618-2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCommandRun {
    /// The command as a person would type it.
    pub command: String,
    /// Its stdout and stderr, or why it could not run.
    pub output: String,
    pub succeeded: bool,
}

/// The dagq plugin installed in the host's Claude Code, which the release
/// update brings to the release of the binary (ADR-t618-2): `claude` on the
/// host, a stub in tests.
pub trait InstalledPlugin: Send + Sync {
    /// The version of the installed plugin (the enabled install, else any);
    /// `None` when it is not installed, an error when it cannot tell.
    fn version(&self) -> Result<Option<String>>;
    /// Read the marketplace again and update the plugin, in that order,
    /// stopping at the first command that fails: the commands run.
    fn update(&self) -> Vec<PluginCommandRun>;
}

/// Provider-specific CLI construction is kept outside supervisor orchestration.
pub trait AgentProvider {
    fn preflight(&self) -> Result<()>;
    /// Whether the agent's sessions started in `cwd` load the plugin
    /// `name` without a `--plugin-dir` (ADR-t617-2 decision 4); an error
    /// when it cannot tell. A provider without plugins cannot tell.
    fn installed_plugin(&self, cwd: &std::path::Path, name: &str) -> Result<PluginState> {
        let _ = (cwd, name);
        anyhow::bail!("this provider has no plugins")
    }
    fn command(&self, run: &crate::domain::TaskRun, prompt: &str) -> Result<CommandSpec>;
    /// The same session reopened for a `needs_session` run (ADR-0019): the
    /// run's own settings and idle marker, without a prompt; the supervisor
    /// sends the resolution request to the terminal once it is up.
    fn resume_command(&self, run: &crate::domain::TaskRun) -> Result<CommandSpec>;
    /// A headless run of the agent for a job without a workspace (ADR-0024
    /// decision 2): `prompt` in `cwd`, allowed what `access` says and
    /// nothing else that needs permission (ADR-t1063-1 decision 2). The
    /// provider turns `access` into its own mechanism, and gives the job
    /// its own settings or none. The caller sets the environment and where
    /// the output goes, and reads the job's reply through
    /// [`AgentProvider::job_reply`]. A provider without one refuses.
    fn headless_command(
        &self,
        cwd: &std::path::Path,
        prompt: &str,
        access: crate::domain::headless_job::JobAccess,
    ) -> Result<CommandSpec> {
        let _ = (cwd, prompt, access);
        anyhow::bail!("this provider has no headless execution")
    }
    /// The final reply of a headless job (from
    /// [`AgentProvider::headless_command`] or
    /// [`AgentProvider::review_command`]) in what its process wrote to
    /// `stdout` (ADR-t1063-1 decision 2): the text the job reads its
    /// verdict or result from, whatever the provider's output looks like.
    /// A provider whose job prints only its reply gives `stdout` back.
    fn job_reply(&self, stdout: &str) -> String {
        stdout.to_owned()
    }
    /// What the output of a headless job that ended (its `stdout`, the job
    /// started at `since`, unix milliseconds) says of its session
    /// (ADR-t1063-1 decision 6): `None` for a provider whose session the
    /// runtime names ahead and whose transcript gives the model (Claude
    /// Code, ADR-0048 decision 4); Codex names its thread in its output and
    /// the model in its rollout.
    fn job_session(
        &self,
        stdout: &str,
        since: Option<i64>,
    ) -> Option<crate::domain::headless_job::JobSession> {
        let _ = (stdout, since);
        None
    }
    /// Why a headless job of this provider failed, from its `stdout` and
    /// `stderr`, in the classes shared by every provider (ADR-t1063-1
    /// decision 4). Claude Code's jobs are read by its
    /// [`AgentSignals::job_failure`] (task 438); a provider that cannot
    /// tell says `other`.
    fn job_failure(&self, stdout: &str, stderr: &str) -> crate::domain::headless_job::JobFailure {
        let _ = (stdout, stderr);
        crate::domain::headless_job::JobFailure::Other
    }
    /// The agent of a planner session (ADR-0041 decisions 1, 6): an
    /// interactive agent in `planner.cwd` with `planner.prompt` as its first
    /// message, whose `Stop` hook writes [`PlannerCommand::idle_marker`] the
    /// way a worker's does, and that loads `planner.plugin_dir`. A provider
    /// without one refuses.
    fn planner_command(&self, planner: &PlannerCommand<'_>) -> Result<CommandSpec> {
        let _ = planner;
        anyhow::bail!("this provider has no planner session")
    }
    /// How often the session wrapper checks the agent for its exit and
    /// heartbeats; tests shorten it.
    fn wait_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(1)
    }
    /// The headless review of an accepted run (ADR-0023 decision 2,
    /// ADR-0027). Kept apart from [`AgentProvider::headless_command`]: a
    /// review belongs to a run, and needs that run's directory (settings
    /// without the worker's `Stop` hook, so the live session's idle marker
    /// is not written, a debug file, `--add-dir`) and tools denied as well
    /// as allowed, since the live worker session owns the worktree; the
    /// observer's job has no run.
    ///
    /// A non-interactive agent in the run's worktree with settings of the
    /// run directory and `prompt` as its only input, allowed what `access`
    /// says (the review reads files only), whose reply
    /// ([`AgentProvider::job_reply`]) is the verdict JSON. It must not
    /// touch the worker session's idle marker. The runtime wires stdin,
    /// stdout and stderr, waits at most [`AgentProvider::review_timeout`]
    /// and reads stdout.
    fn review_command(
        &self,
        run: &crate::domain::TaskRun,
        prompt: &str,
        access: crate::domain::headless_job::JobAccess,
    ) -> Result<CommandSpec>;
    /// Whether this provider's review job can run the review's required
    /// subagents (ADR-t1453-1 decision 8): one that cannot is not started
    /// for a review that requires them. None can by default.
    fn runs_review_subagents(&self) -> bool {
        false
    }
    /// Hand the review `command` ([`AgentProvider::review_command`]) the
    /// definitions of its required subagents, allowed only the review's
    /// reads, and keep it from loading agents or settings of the
    /// worktree. A provider that cannot ([`AgentProvider::runs_review_subagents`])
    /// refuses.
    fn review_subagents(
        &self,
        command: &mut CommandSpec,
        agents: &[crate::domain::review_subagents::AgentDefinition],
    ) -> Result<()> {
        let _ = (command, agents);
        anyhow::bail!("this provider cannot run the review's subagents")
    }
    /// The agent of the inbox (ADR-0022): an interactive agent with
    /// `prompt` as its first message that loads `plugin_dir`, run as the
    /// command of its workspace. Its settings, if the provider has a way
    /// to give them, are only the inbox's denials (ADR-t1228-2 decision 3),
    /// written under `queue_dir`: no hook, no idle marker, the prompt
    /// suggestions as they are (a person works in it). A provider without
    /// one refuses.
    fn inbox_command(
        &self,
        prompt: &str,
        plugin_dir: Option<&std::path::Path>,
        queue_dir: &std::path::Path,
    ) -> Result<CommandSpec> {
        let _ = (prompt, plugin_dir, queue_dir);
        anyhow::bail!("this provider has no inbox session")
    }
    /// The settings file [`AgentProvider::inbox_command`] writes under
    /// `queue_dir` and starts the inbox with, which refuse raw `cmux`
    /// (ADR-t1228-2 decisions 3 and 6); `None` for a provider whose inbox
    /// has no such guardrail. `up` records whether there is one, and
    /// `status` and `doctor` show it.
    fn inbox_settings(&self, queue_dir: &std::path::Path) -> Option<std::path::PathBuf> {
        let _ = queue_dir;
        None
    }
    /// Start the worker session `command` (from [`AgentProvider::command`]
    /// or [`AgentProvider::resume_command`]) with `model` at `effort`
    /// (ADR-0079 decision 3): given explicitly, so neither the provider's
    /// default nor the user's settings decide them. A provider without
    /// models leaves it as it is.
    fn select_model(&self, command: &mut CommandSpec, model: &str, effort: &str) {
        let _ = (command, model, effort);
    }
    /// Start the session of a role other than the worker (`command`) the
    /// way `launch` says (ADR-0079 decision 7): with its model and effort
    /// through [`AgentProvider::select_model`] when it gives them, else as
    /// it is (the provider's default, as before).
    fn apply_launch(
        &self,
        command: &mut CommandSpec,
        launch: &crate::domain::actor_model::ActorLaunch,
    ) {
        if let Some((model, effort)) = launch.arguments() {
            self.select_model(command, model, effort);
        }
    }
    /// Start the session of a headless job (`command`, from
    /// [`AgentProvider::review_command`] or
    /// [`AgentProvider::headless_command`]) with `session_id`, so that its
    /// span and transcript are known before it ends (ADR-0048 decision 4).
    /// A provider that cannot name its session leaves it as it is.
    fn assign_session_id(&self, command: &mut CommandSpec, session_id: &str) {
        let _ = (command, session_id);
    }
    /// Start a headless job (`command`, from
    /// [`AgentProvider::headless_command`]) without any MCP server: the
    /// observer's job reads the queue through its CLI only (ADR-0044), and
    /// loading the user's servers costs every observation their start. A
    /// provider without MCP leaves it as it is.
    fn without_mcp(&self, command: &mut CommandSpec) {
        let _ = command;
    }
    /// Give a worker's agent (`command`, from [`AgentProvider::command`],
    /// [`AgentProvider::resume_command`] or [`AgentProvider::turn_command`])
    /// the resource broker's tools: the MCP configuration at `config`
    /// (`<run dir>/broker/mcp.json`) and the permission to use its server
    /// (ADR-t827-4 decision 1). Whether it did: a provider whose MCP the
    /// runtime does not pass yet (Codex) leaves the command as it is.
    fn broker_tools(&self, command: &mut CommandSpec, config: &std::path::Path) -> bool {
        let _ = (command, config);
        false
    }
    /// Let the agent of `command` (a worker's turn or session, or a
    /// headless job) reach the queue service at `socket`, which its
    /// client-mode `dagq` calls (ADR-t1233-5 decision 4): a provider whose
    /// sandbox would refuse the connection allows that socket and nothing
    /// more. One without a sandbox (Claude Code's agents) leaves the
    /// command as it is.
    fn reach_queue_service(&self, command: &mut CommandSpec, socket: &std::path::Path) {
        let _ = (command, socket);
    }
    /// How long the headless review may take before it counts as failed.
    fn review_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(600)
    }
    /// One turn of a headless worker (ADR-t813-1 decision 1): a
    /// non-interactive call in the run's worktree with `prompt` as its only
    /// input, starting a new session (named as `session` says, for a
    /// provider that takes the name) or going on with the session of that
    /// id, under the run's settings. Its stdout is the turn's output for
    /// [`AgentProvider::turn_reader`]. The caller closes stdin, says where
    /// the output goes and starts it in a process group of its own. A
    /// provider without one refuses.
    ///
    /// `target` says where the turn runs: a run's worktree and directory,
    /// or a headless planner's checkout and directory (ADR-t1394-2
    /// decision 6: the provider has no branch of the planner's own).
    fn turn_command(
        &self,
        target: &TurnTarget<'_>,
        prompt: &str,
        session: crate::domain::turn::TurnSession<'_>,
    ) -> Result<CommandSpec> {
        let _ = (target, prompt, session);
        anyhow::bail!("this provider has no headless worker")
    }
    /// Whether the agent names a new session itself, in the turn's output
    /// (Codex's `thread.started`), rather than taking the run's id for it
    /// (Claude's `--session-id`): the wrapper then records the id it reads
    /// and resumes that one.
    fn turn_session_from_output(&self) -> bool {
        false
    }
    /// Whether the agent keeps a session named `name` in `cwd` (the
    /// turn's working directory) already (its transcript exists), so that
    /// the next turn resumes it rather than starting one of that name
    /// again: a turn that failed before its model answered (a login that
    /// ran out) may have left one.
    fn turn_session_exists(&self, cwd: &std::path::Path, name: &str) -> bool {
        let _ = (cwd, name);
        false
    }
    /// The reader of a headless turn's output, one per turn.
    fn turn_reader(&self) -> Result<Box<dyn TurnReader>> {
        anyhow::bail!("this provider has no headless worker")
    }
    /// The permission mode a headless turn must say it started in; one
    /// that says another is stopped as started otherwise than asked
    /// (ADR-t813-1 decision 8). `None` when the provider says none.
    fn turn_permission_mode(&self) -> Option<&'static str> {
        None
    }
}

/// Why a headless job's process could not be started, in the classes
/// shared by every provider (ADR-t1063-1 decision 4): an executable that is
/// not there is `executable_missing`, any other refusal of the start
/// `launch_failed`, except arguments or an environment past the system's
/// limit (`E2BIG`, os error 7), or a standard input that could not be
/// prepared ([`StdinUnprepared`], whatever its io error): that is the job's
/// own input or the starter's environment, not its provider, so it is
/// `other`, a failure of the job alone that holds no provider (task 1560).
pub fn job_start_failure(error: &anyhow::Error) -> crate::domain::headless_job::JobFailure {
    use crate::domain::headless_job::JobFailure;
    if error
        .chain()
        .any(|cause| cause.downcast_ref::<StdinUnprepared>().is_some())
    {
        return JobFailure::Other;
    }
    let kind = |kind: io::ErrorKind| {
        error.chain().any(|cause| {
            cause
                .downcast_ref::<io::Error>()
                .is_some_and(|io| io.kind() == kind)
        })
    };
    if kind(io::ErrorKind::NotFound) {
        JobFailure::ExecutableMissing
    } else if kind(io::ErrorKind::ArgumentListTooLong) {
        JobFailure::Other
    } else {
        JobFailure::LaunchFailed
    }
}

/// Reads the output of one headless turn into what the runtime acts on,
/// whatever the provider (ADR-t813-1): the one place that knows the shape
/// of a provider's JSONL.
pub trait TurnReader: Send {
    /// One line of the turn's stdout, without its line break.
    fn line(&mut self, line: &str) -> Vec<crate::domain::turn::TurnSignal>;
    /// The lines read next were read at `at` (unix milliseconds): a reader
    /// whose output has no times takes them as its items' (Codex's, for
    /// [`crate::domain::turn::TurnResult::commands`]).
    fn stamp(&mut self, _at: i64) {}
    /// Whether the output goes on while the agent works (a heartbeat), so
    /// that a silence means the turn is stuck.
    fn heartbeats(&self) -> bool;
    /// How the turn ended, from the lines read, how its process ended
    /// (`None` when it was stopped) and its stderr.
    fn finish(&mut self, exit: Option<&Exit>, stderr: &str) -> crate::domain::turn::TurnResult;
}

/// Where the transcript of a Claude session span is: the span's session id,
/// the directory the session ran in and the path recorded for it, if any.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranscriptSource {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub transcript_path: Option<String>,
}

/// The transcripts of the agent's sessions (ADR-0048 decision 9): the one
/// place that knows where they are and what they hold. The runtime reads a
/// span's active time from it, and task 199 its token counts. A transcript
/// that cannot be read (missing, of a format this build does not know, of
/// another session) is `Err`, and nothing is recorded from it.
pub trait Transcripts {
    fn read(
        &self,
        source: &TranscriptSource,
    ) -> std::result::Result<
        crate::domain::transcript::Transcript,
        crate::domain::transcript::Unreadable,
    >;
}

/// The adapters a worker of one provider in one mode runs through
/// (ADR-0004, ADR-t813-2): the agent that starts its sessions, the signals
/// the supervisor reads of them, and their transcripts.
#[derive(Clone, Copy)]
pub struct WorkerAdapter<'a> {
    pub agent: &'a dyn AgentProvider,
    pub signals: &'a dyn AgentSignals,
    pub transcripts: &'a dyn Transcripts,
}

/// The table of [`WorkerAdapter`]s by worker (provider and mode) that the
/// composition fills: a worker without an entry is one this binary cannot
/// run, and the supervisor does not claim its tasks.
#[derive(Clone, Default)]
pub struct WorkerAdapters<'a> {
    entries: Vec<(crate::domain::worker::Worker, WorkerAdapter<'a>)>,
}

impl<'a> WorkerAdapters<'a> {
    /// The table with `adapter` for `worker`, in place of any it had.
    pub fn with(
        mut self,
        worker: crate::domain::worker::Worker,
        adapter: WorkerAdapter<'a>,
    ) -> Self {
        self.entries.retain(|(entry, _)| *entry != worker);
        self.entries.push((worker, adapter));
        self
    }

    /// The adapters of `worker`, if this binary runs it.
    pub fn get(&self, worker: crate::domain::worker::Worker) -> Option<WorkerAdapter<'a>> {
        self.entries
            .iter()
            .find(|(entry, _)| *entry == worker)
            .map(|(_, adapter)| *adapter)
    }

    /// The workers the table runs, in the order they were added.
    pub fn workers(&self) -> Vec<crate::domain::worker::Worker> {
        self.entries.iter().map(|(worker, _)| *worker).collect()
    }

    /// The modes the table runs `provider` in.
    pub fn modes(
        &self,
        provider: crate::domain::Provider,
    ) -> Vec<crate::domain::worker::WorkerMode> {
        self.entries
            .iter()
            .filter(|(worker, _)| worker.provider == provider)
            .map(|(worker, _)| worker.mode)
            .collect()
    }
}

/// Where a headless turn runs (ADR-t813-1, ADR-t1394-2): the actor it
/// runs for (a worker, or a planner of the runtime's), the directory of its
/// requests, settings and turns (the run's or the planner's), the directory
/// it works in (the run's worktree, or the repository's checkout), its
/// agent's debug log (a provider that writes one refuses a target without
/// it), and the plugin directory it loads (a planner's).
#[derive(Debug, Clone, Copy)]
pub struct TurnTarget<'a> {
    pub role: crate::domain::ActorRole,
    pub dir: &'a std::path::Path,
    pub cwd: &'a std::path::Path,
    pub debug_log: Option<&'a std::path::Path>,
    pub plugin_dir: Option<&'a std::path::Path>,
}

impl<'a> TurnTarget<'a> {
    /// The turn of `run`'s worker: in its worktree, with its run directory
    /// and log.
    pub fn of_run(run: &'a crate::domain::TaskRun) -> Result<Self> {
        Ok(Self {
            role: crate::domain::ActorRole::Worker,
            dir: std::path::Path::new(run.run_dir().context("missing run directory")?),
            cwd: std::path::Path::new(run.worktree_path().context("missing worktree")?),
            debug_log: run.log_path().map(std::path::Path::new),
            plugin_dir: None,
        })
    }
}

/// What the agent of a planner session is started with: the planner's
/// directory (its prompt, settings, log and idle marker), the directory it
/// works in (the repository's checkout), its first message and the plugin
/// directory it loads, and who opened it (a person's planner keeps the
/// session settings a person works with).
#[derive(Debug, Clone, Copy)]
pub struct PlannerCommand<'a> {
    pub origin: PlannerOrigin,
    pub dir: &'a std::path::Path,
    pub cwd: &'a std::path::Path,
    pub prompt: &'a str,
    pub plugin_dir: Option<&'a std::path::Path>,
}

impl PlannerCommand<'_> {
    /// Where the agent's `Stop` hook writes its input when the agent stops,
    /// in the planner's directory like a run's `idle.json`.
    pub fn idle_marker(&self) -> std::path::PathBuf {
        planner_idle_marker(self.dir)
    }
}

/// The idle marker of the planner whose directory is `dir`.
pub fn planner_idle_marker(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("idle.json")
}

/// What the supervisor reads of the agent of a live session: its screen,
/// for a dialog that holds it (ADR-0019 decision 6), and the idle marker
/// its hook writes when it stops (ADR-0016). Both formats are the agent's
/// own (for Claude Code, its TUI and its `Stop` hook input), so the
/// provider's adapter implements this; the supervisor decides what a dialog
/// or an idle agent means for the run.
pub trait AgentSignals {
    /// The kind of dialog at the bottom of `screen` that holds the session
    /// (recorded as `prompt` of `prompt_waiting`), or `None` while it works.
    fn detect_prompt(&self, screen: &str) -> Option<&'static str>;
    /// Whether the bottom of `screen` shows the agent stopped at a login
    /// that ran out (ADR-0047 decision 42): only a person can log in again.
    fn auth_required(&self, _screen: &str) -> bool {
        false
    }
    /// Whether the bottom of `screen` shows the agent stopped at its usage
    /// limit (ADR-0047 decision 42, task 438): only a person decides on
    /// the cost.
    fn usage_limited(&self, _screen: &str) -> bool {
        false
    }
    /// The wall only a person moves that `screen` shows the agent stopped
    /// at: a login that ran out, or the usage limit.
    fn screen_wall(&self, screen: &str) -> Option<crate::domain::queue_hold::Wall> {
        use crate::domain::queue_hold::Wall;
        if self.auth_required(screen) {
            Some(Wall::Authentication)
        } else if self.usage_limited(screen) {
            Some(Wall::UsageLimit)
        } else {
            None
        }
    }
    /// Why a headless job that failed did, from its output (its stdout and
    /// stderr), in the classes shared by every provider (ADR-t1063-1
    /// decision 4): a login that ran out and the usage limit are the walls
    /// of task 438. A provider that cannot tell says `other`.
    fn job_failure(&self, _output: &str) -> crate::domain::headless_job::JobFailure {
        crate::domain::headless_job::JobFailure::Other
    }
    /// The inputs that switch a live worker session from `from` to `to`
    /// (ADR-0079 decision 5), each typed and submitted in turn before a
    /// request; `None` when the agent cannot switch inside a session.
    fn model_switch(
        &self,
        _from: &crate::domain::worker_model::WorkerSession,
        _to: &crate::domain::worker_model::WorkerSession,
    ) -> Option<Vec<String>> {
        None
    }
    /// The last lines of `screen` an ask and `prompt_waiting` carry.
    fn screen_excerpt(&self, screen: &str) -> String;
    /// What the idle marker's content says. A content the adapter cannot
    /// read still marks a stop.
    fn idle_hook(&self, content: &[u8]) -> IdleHook;
    /// Who gave the input the input marker's content records (the
    /// `prompt-submit.json` the agent's hook writes each time the session
    /// takes an input, ADR-0043 decision 2). A provider whose hook writes
    /// no such marker never has one to read.
    fn input_source(&self, _content: &[u8]) -> InputSource {
        InputSource::Unknown
    }
    /// The text of the input the input marker's content records, when it
    /// says: matched against the texts the supervisor typed.
    fn input_text(&self, _content: &[u8]) -> Option<String> {
        None
    }
    /// Whether the agent's input box is drawn with no dialog over it: text
    /// typed now reaches the agent (a booting session drops it).
    fn input_ready(&self, screen: &str) -> bool;
    /// Whether the input box still holds `text` after it was submitted.
    fn input_pending(&self, screen: &str, text: &str) -> bool;
    /// Whether the input box is drawn and holds nothing a person typed
    /// (ADR-t906-1 decision 1 (3)): the supervisor types into a session a
    /// person may use only then. `false` by default, which never types.
    fn input_empty(&self, _screen: &str) -> bool {
        false
    }
    /// Whether the screen shows the agent at work on a turn.
    fn working(&self, screen: &str) -> bool;
    /// Whether `screen` shows background work the agent keeps running
    /// (Claude Code's count of background shells under its input box): a
    /// `/exit` sent now stops at the agent's own dialog. `None` from a
    /// provider whose screen does not tell, which counts as none.
    fn screen_background(&self, _screen: &str) -> Option<bool> {
        None
    }
    /// The part of `screen` only the agent's work changes (its
    /// transcript), compared for a sign of work after a submit: what the
    /// TUI redraws by itself (a clock, a cost, a notification) is left
    /// out. The whole screen by default.
    fn transcript(&self, screen: &str) -> String {
        screen.to_owned()
    }
    /// The dialog of the fixed list the supervisor answers by rule
    /// (ADR-0047 decision 29) at the bottom of `screen`, with the keys that
    /// answer it, or `None` for any other screen, however much a dialog.
    fn known_dialog(&self, _screen: &str) -> Option<DialogAnswer> {
        None
    }
    /// The line of `log`, the end of the agent's debug log, that says its
    /// idle hook failed to write the marker (ADR-t803-1), the latest; a
    /// provider that logs none never has one.
    fn idle_hook_failure(&self, _log: &str) -> Option<String> {
        None
    }
}

/// A dialog of the agent's TUI the supervisor answers by a fixed rule once
/// its safety conditions hold (ADR-0047 decision 29). Any other dialog gets
/// no key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownDialog {
    /// The confirmation a `/exit` gets while background work runs
    /// ("Background work is running"): answered with "Exit and stop tasks"
    /// only after the supervisor's `/exit`, with the worktree clean and the
    /// receipt's commit at HEAD.
    BackgroundWork,
    /// The Settings panel (`/status`, `/usage`, ...) left open over the
    /// input box: closed with Esc at any stage.
    SettingsPanel,
}

impl KnownDialog {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BackgroundWork => "background_work",
            Self::SettingsPanel => "settings_panel",
        }
    }
}

/// A known dialog found on a screen and the keys that answer it, in the
/// backend's key names ([`WorkspaceBackend::send_key`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogAnswer {
    pub dialog: KnownDialog,
    pub keys: Vec<&'static str>,
}

/// Who gave an input the session took, as [`AgentSignals::input_source`]
/// reads its input marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputSource {
    /// Text typed into the session: a person's, or the supervisor's (told
    /// apart by the time of the supervisor's sends).
    Typed,
    /// Text the agent put in by itself, such as the notice that its
    /// background work ended.
    Agent,
    /// The marker does not say.
    Unknown,
}

/// The content of an idle marker, as [`AgentSignals::idle_hook`] read it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IdleHook {
    /// Background work the agent left running when it stopped: a `/exit`
    /// sent now stops at the agent's own dialog.
    pub background_running: bool,
    /// The background tasks still `running` when it stopped.
    pub background_tasks: Vec<crate::domain::stall::BackgroundTask>,
    /// The fields of the hook input recorded with the evidence of a stop,
    /// by name.
    pub evidence: Vec<(&'static str, serde_json::Value)>,
}

/// The Git remote `integrate` pushes the landing branch to (ADR-0019
/// decision 3, ADR-t615-1), replaceable in tests.
pub trait MainRemote {
    /// `[repository]` of `dagq.toml`: which remote to push to and whether
    /// to push; the default is `origin`, pushed.
    fn push_config(&self) -> Result<crate::domain::landing_branch::RepositoryConfig> {
        Ok(Default::default())
    }
    /// Whether the repository has a remote named `remote`.
    fn has_remote(&self, remote: &str) -> Result<bool>;
    /// Push `branch`, the landing branch resolved when the landing began,
    /// to the same branch of `remote`. An error is the failed push, with
    /// Git's message. Only the [`super::integrate::Integrator`] holds the
    /// [`super::integrate::PushGrant`] it takes (ADR-t728-2).
    fn push_main(
        &self,
        grant: &super::integrate::PushGrant,
        remote: &str,
        branch: &crate::domain::landing_branch::LandingBranch,
    ) -> Result<()>;
    /// After a failed push, whether the remote branch already contains the
    /// commit landed by this run. An error leaves the push failed.
    fn contains_landed_commit(
        &self,
        remote: &str,
        branch: &crate::domain::landing_branch::LandingBranch,
        commit: &crate::domain::CommitSha,
    ) -> Result<bool>;
}

/// The one `CMUX_*` variable a detached process may carry: cmux's CLI
/// reads its socket password from it.
pub const SOCKET_PASSWORD_ENV: &str = "CMUX_SOCKET_PASSWORD";

/// The variable that moves the user's `config.toml` and the host-wide
/// `host.toml` away from `~/.config/dagq/`.
pub const CONFIG_HOME_ENV: &str = "XDG_CONFIG_HOME";

/// The environment variables the LaunchAgent gives the supervisor, which
/// is all a launchd-started process keeps of the shell that ran `up`: its
/// PATH and, only when that shell exported them, the cmux socket password
/// and `XDG_CONFIG_HOME`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SupervisorEnvironment {
    pub path: String,
    /// `CMUX_SOCKET_PASSWORD` as exported by the invoking shell; a password
    /// saved in cmux's Settings is never read or stored here.
    pub socket_password: Option<String>,
    /// `XDG_CONFIG_HOME` as exported (non-empty) by the invoking shell, so
    /// the supervisor reads the same `config.toml` and `host.toml` as the
    /// `up` that checked them; unset, both are under `~/.config/dagq/`.
    pub config_home: Option<String>,
}

/// cmux answered the detached ping and did not admit it (its message is
/// `reason`), as opposed to not answering at all: only this failure has
/// the socket password as its remedy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedRefusal {
    pub reason: String,
}

impl std::fmt::Display for DetachedRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for DetachedRefusal {}

/// What a workspace carries besides its title and command (ADR-0026).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceTags {
    /// Environment every shell of the workspace inherits: `DAGQ_ROLE` and
    /// `DAGQ_QUEUE`, which the workspace keeps however its session is
    /// started again.
    pub env: Vec<(String, String)>,
    /// One machine-readable line for people; never read back.
    pub description: Option<String>,
    /// The queue's workspace group, when it could be made.
    pub group: Option<String>,
}

pub trait WorkspaceBackend {
    fn preflight(&self) -> Result<()>;
    /// Check that cmux accepts a connection from a process that is not a
    /// child of one of its terminals, the way the launchd-run supervisor
    /// connects: `ping` run outside cmux's process tree with `environment`
    /// and none of the `CMUX_*` variables a cmux session inherits (cmux
    /// admits such a process only by socket password). A refusal is a
    /// [`DetachedRefusal`]; any other error means cmux could not be asked.
    fn preflight_detached(&self, environment: &SupervisorEnvironment) -> Result<()>;
    /// Open the workspace a run's session works in. `task` is the run's
    /// task; the backend names the workspace after it (ADR-0018) and gives
    /// it `tags`.
    fn create(
        &self,
        task: &crate::domain::Task,
        run: &crate::domain::TaskRun,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String>;
    /// Open the workspace a resumed session of a `needs_session` run works
    /// in, in the run's worktree; the backend names it like the run's worker
    /// workspace (ADR-0028; display only, ADR-0026) and gives it `tags`.
    fn create_resume(
        &self,
        task: &crate::domain::Task,
        run: &crate::domain::TaskRun,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String>;
    /// Type one line at the session's prompt and submit it: the resolution
    /// request to a resumed session, the only text besides `/exit` the
    /// supervisor sends (ADR-0019). A long text is given time to be pasted
    /// before the Enter. Whether it was submitted is the caller's to read
    /// from the screen (task 285).
    fn send_text(&self, workspace_id: &str, text: &str) -> Result<()>;
    /// Press Enter alone: a text or `/exit` left in the input box after its
    /// submit is submitted again without being typed twice (task 285).
    fn send_enter(&self, workspace_id: &str) -> Result<()>;
    /// Press one key (`down`, `up`, `enter`, `escape`): the answer to a
    /// known dialog (ADR-0047 decision 29), the only keys the supervisor
    /// sends besides Enter. A backend without keys refuses.
    fn send_key(&self, workspace_id: &str, key: &str) -> Result<()> {
        let _ = (workspace_id, key);
        anyhow::bail!("this workspace backend sends no keys")
    }
    fn capture(&self, workspace_id: &str) -> Result<String>;
    /// Close the workspace; the worktree and branch are not touched. A
    /// pinned workspace is unpinned first, since cmux refuses to close one
    /// (ADR-0031); every close dagq makes goes through here.
    fn close(&self, workspace_id: &str) -> Result<()>;
    /// Give the workspace a sidebar color: a cmux color name or `#RRGGBB`.
    fn set_color(&self, workspace_id: &str, color: &str) -> Result<()>;
    /// Show the status pill `key` with `value` and `icon` on the
    /// workspace's sidebar entry, replacing the pill under the same key.
    fn set_status(&self, workspace_id: &str, key: &str, value: &str, icon: &str) -> Result<()>;
    /// Pin the workspace in the sidebar; pinning a pinned one is a no-op.
    fn pin(&self, workspace_id: &str) -> Result<()>;
    /// Ask the agent session to end the way a person would, without killing it.
    fn send_exit(&self, workspace_id: &str) -> Result<()>;
    /// [`send_exit`](Self::send_exit), made again after a timeout while
    /// `unsent` says from a screen read that the `/exit` did not get there
    /// (task 354). A backend that retries nothing sends it once.
    fn send_exit_when(&self, workspace_id: &str, unsent: &dyn Fn(&str) -> bool) -> Result<()> {
        let _ = unsent;
        self.send_exit(workspace_id)
    }
    /// Whether the workspace with this stable ID is still open. Workspaces
    /// are found by the ID the queue recorded, never by their title, which
    /// people may rename (ADR-0026).
    fn exists(&self, workspace_id: &str) -> Result<bool>;
    /// The stable IDs of every workspace cmux lists, in all its windows:
    /// one listing for many checks, as the supervisor's sweep of ended
    /// runs' workspaces makes.
    fn listed_workspace_ids(&self) -> Result<Vec<String>>;
    /// The stable IDs of the workspaces cmux lists, in all its windows,
    /// whose description is exactly `description`: a workspace a create
    /// reported failed may have been made all the same (a create that
    /// timed out, task 806), and its description is all that finds it. A
    /// backend that keeps no descriptions finds none.
    fn workspaces_described(&self, description: &str) -> Result<Vec<String>> {
        let _ = description;
        Ok(Vec::new())
    }
    /// Open a workspace that is not tied to a run (the inbox and planner
    /// sessions, the in-cmux supervisor) and return its stable ID.
    fn create_named(
        &self,
        name: &str,
        cwd: &std::path::Path,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String>;
    /// Start the session wrapper `command` (a shell command line) in `cwd`
    /// without a workspace, as a process detached from this one with `env`
    /// in its environment and its output in `log` (ADR-t1404-1 decision
    /// 1); the [`BackgroundHandle`](crate::domain::background_wrapper::BackgroundHandle)
    /// the run records in place of a workspace ID. Every other call of the
    /// backend takes that handle as a workspace: `exists` says whether the
    /// wrapper runs, `close` stops it and what it started, and the screen
    /// and the keys are refused. A backend without background processes
    /// refuses.
    fn launch_background(
        &self,
        cwd: &std::path::Path,
        command: &str,
        env: &[(String, String)],
        log: &std::path::Path,
    ) -> Result<String> {
        let _ = (cwd, command, env, log);
        anyhow::bail!("this workspace backend starts no background wrapper")
    }
    /// The handle of the workspace group whose external ID is
    /// `external_id`, created under `name` when there is none yet; asking
    /// again returns the same group.
    fn ensure_group(&self, external_id: &str, name: &str) -> Result<String>;
    /// Tell a person that something waits for them: a notification, never
    /// keystrokes into a terminal. `workspace` is the workspace it belongs
    /// to; `None` sends it without one. Only `ask` sends one, for a new
    /// ask, aimed at the inbox (ADR-0022 decision 5); the supervisor sends
    /// none.
    fn notify(&self, title: &str, body: &str, workspace: Option<&str>) -> Result<()>;
    /// How long one call may run before the backend gives it up as failed;
    /// recorded with every `backend_call_failed`.
    fn call_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(30)
    }
    /// How many times in all a call that timed out is made when making it
    /// again is safe (a read, or a text that did not reach the screen;
    /// task 326).
    fn call_attempts(&self) -> u32 {
        3
    }
    /// The backoff before the first retry of a call that timed out,
    /// doubled before each next one.
    fn retry_backoff(&self) -> std::time::Duration {
        std::time::Duration::from_secs(2)
    }
    /// How long the session may take to exit after the request before the
    /// supervisor stops waiting and leaves the run to a human.
    fn exit_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(120)
    }
    /// How long the session's wrapper may take to register after the
    /// workspace opens before the supervisor gives the run up.
    fn registration_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(45)
    }
    /// How long after an attempt to open a headless run's lost session
    /// again the supervisor makes the next one (task 1372): what stopped
    /// the wrapper may be cmux itself, which needs time to come back.
    fn reopen_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(60)
    }
    /// How long a session may run with neither a receipt nor an idle marker
    /// before the supervisor starts reading its screen for a dialog.
    fn prompt_wait(&self) -> std::time::Duration {
        std::time::Duration::from_secs(90)
    }
    /// How long a resumed session's agent may take, after it registered, to
    /// be ready for the resolution request.
    fn resume_prompt_delay(&self) -> std::time::Duration {
        std::time::Duration::from_secs(5)
    }
    /// How long a resumed session may work on the resolution request
    /// without going idle before the supervisor asks it to exit.
    fn resume_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(3600)
    }
    /// How long after a submit (and between the Enters sent again) the
    /// screen is read for the text left in the input box.
    fn submit_check_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(1)
    }
}

/// What the service manager had under a label when `uninstall` ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentState {
    pub loaded: bool,
    /// The agent's process, when it was loaded and running.
    pub pid: Option<u32>,
}

/// The service manager that keeps the supervisor resident (launchd on
/// macOS). `up` writes the agent's definition and loads it; `down` unloads
/// it, which is what makes the supervisor stop without being restarted.
pub trait LaunchAgent {
    /// Write `contents` to `path` and (re)load the agent under `label`: an
    /// agent already loaded is unloaded first, and its process waited for,
    /// so the new definition takes.
    fn install(&self, label: &str, path: &std::path::Path, contents: &str) -> Result<()>;
    /// Unload the agent without waiting for its process (which drains) and
    /// remove its definition so it does not come back at the next login.
    fn uninstall(&self, label: &str, path: &std::path::Path) -> Result<AgentState>;
}

/// The sccache server of a `[run.env]` whose `RUSTC_WRAPPER` is sccache
/// (ADR-t1215-1), looked at and started from outside any sandbox.
pub trait SccacheServer {
    /// Whether something listens on the loopback `port`, looked at without
    /// starting anything: no sccache client runs (a client starts the
    /// server it does not find, with the caller's environment).
    fn listening(&self, port: u16) -> Result<bool>;
    /// Start the server with `program --start-server` and `env` beside this
    /// process's (the caller puts `SCCACHE_IDLE_TIMEOUT=0` in it), wait
    /// until it listens on `port`, and read the pid of the process that
    /// listens there.
    fn start(&self, program: &Path, env: &[(String, String)], port: u16) -> Result<ServerPid>;
}

/// The pid of the sccache server [`SccacheServer::start`] started, or why
/// it could not be read (the server runs either way).
pub type ServerPid = std::result::Result<u32, String>;

/// Liveness and signals for the supervisor's PID, replaceable in tests.
pub trait ProcessControl {
    fn alive(&self, pid: u32) -> bool;
    /// Ask the process to drain (SIGTERM); used when no agent is loaded for it.
    fn terminate(&self, pid: u32) -> Result<()>;
    /// Ask the process to drain the way Ctrl-C in its terminal would
    /// (SIGINT); used for the supervisor of an in-cmux workspace, which no
    /// service manager can signal for us.
    fn interrupt(&self, pid: u32) -> Result<()>;
    /// End the process immediately (SIGKILL).
    fn kill(&self, pid: u32) -> Result<()>;
    /// End the process group `leader` leads at once (SIGKILL to the
    /// group); a group that is gone is no error. A process control that
    /// signals no groups refuses.
    fn kill_group(&self, leader: u32) -> Result<()> {
        let _ = leader;
        anyhow::bail!("this process control signals no process groups")
    }
    /// Collect the exit of `pid` if it is an ended child of this process,
    /// so it no longer counts as alive: a child the process started before
    /// it exec'd another binary (the automatic update's job, ADR-0045
    /// decision 17) has no other reaper. Nothing for any other process.
    fn reap(&self, pid: u32) {
        let _ = pid;
    }
    /// This user's processes with their parents, ages, commands and
    /// working directories, for the recovery job (ADR-0047 decision 39).
    fn list(&self) -> Result<Vec<crate::domain::recovery::ProcessInfo>> {
        anyhow::bail!("this process control cannot list processes")
    }
    /// When the process `pid` started, as the system prints it, to tell a
    /// headless job's process from another that took its pid later (task
    /// 443); `None` when there is no such process or it cannot be read.
    fn start_identity(&self, pid: u32) -> Option<String> {
        let _ = pid;
        None
    }
    /// When the process `pid` started, in unix seconds, to tell an inbox
    /// watch's process from another that took its pid later (task 927);
    /// `None` when there is no such process or its start cannot be read.
    fn started_at(&self, pid: u32) -> Option<i64> {
        let _ = pid;
        None
    }
    /// The processes `pid` started, and theirs, from [`Self::list`]; none
    /// when the processes cannot be listed.
    fn descendants(&self, pid: u32) -> Vec<u32> {
        self.list()
            .map(|all| crate::domain::headless_job::descendants(&all, pid))
            .unwrap_or_default()
    }
}

/// The current time, injected so a use case reads it through this port and
/// a test can fix it (ADR-0013 policy 7). One operation reads it once and
/// passes the value on, so its steps share one reference time.
pub trait Clock: Send + Sync {
    fn system_time(&self) -> SystemTime;

    /// Unix seconds, the form of `heartbeat_at`, `closed_at` and the other
    /// INTEGER times.
    fn now(&self) -> i64 {
        unix_seconds(self.system_time())
    }

    /// `%Y-%m-%dT%H:%M:%fZ` in UTC (RFC 3339 with milliseconds), the form
    /// SQLite's `strftime` gives `created_at` / `updated_at`.
    fn timestamp(&self) -> String {
        timestamp(self.system_time())
    }
}

/// New identifiers: run IDs, supervisor tokens and integrate tokens, each
/// a UUID string.
pub trait IdGenerator: Send + Sync {
    fn uuid(&self) -> String;

    /// A new supervisor or integrate token.
    fn lease_token(&self) -> LeaseToken {
        LeaseToken::new(self.uuid())
    }
}

/// The clock and the ID generator a use case is given.
#[derive(Clone)]
pub struct Generators {
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGenerator>,
}

impl fmt::Debug for Generators {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Generators").finish_non_exhaustive()
    }
}

/// A run some other process leases, with its wrapper registration: what a
/// supervisor with a free slot judges for adoption (ADR-0012).
#[derive(Debug, Clone)]
pub struct LeasedRun {
    pub run: TaskRun,
    pub lease: RunLease,
    pub wrapper: Option<RunProcess>,
}

/// What `integrate` put on `main`: the squash `commit` whose tree is that of
/// `source_commit` (the rebased run head kept under `history_ref`), on top of
/// `main_before`. `verification_skipped` is always false: every landing
/// runs the verification commands (ADR-0023 decision 1); the field keeps the
/// `run_integrated` payload's shape.
#[derive(Debug, serde::Serialize)]
pub struct Landing {
    pub commit: CommitSha,
    pub source_commit: CommitSha,
    pub main_before: CommitSha,
    pub history_ref: String,
    pub message: String,
    pub verification_skipped: bool,
}

pub use crate::domain::validation::Validation;

/// A workspace an ended run opened (the worker's or a resume's), which the
/// supervisor's sweep closes while cmux still lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndedRunWorkspace {
    pub run_id: RunId,
    pub status: RunStatus,
    pub workspace_id: String,
}

/// The worktree of a run that nobody leases and that either ended or
/// belongs to a task that is over, for the supervisor's clean-up of the
/// disk (task 376), or of a run left in the middle that nobody works on
/// (task 1289, [`WorktreeCleanup`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndedRunWorktree {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub status: RunStatus,
    pub task_status: TaskStatus,
    pub worktree: String,
    pub branch: Option<String>,
    pub cleanup: WorktreeCleanup,
}

/// Why a run's worktree is a candidate for the cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeCleanup {
    /// The run ended (`integrated`, `succeeded`, `failed`, `interrupted`)
    /// or its task is over (task 376).
    Ended,
    /// An `awaiting_integration` or `needs_session` run of a task that goes
    /// on, with no lease and no live session, waiting for a person's
    /// answer to this ask, its oldest one open and unanswered (task 1289):
    /// its build outputs go on every cleanup.
    AwaitingAnswer(AskId),
    /// Such a run with no ask waiting for an answer (queued to land, or
    /// waiting for a resume): its build outputs go only in a cleanup for
    /// disk space (task 1289).
    Idle,
}

/// A `needs_session` run as the supervisor judges it for a resume.
#[derive(Debug, Clone)]
pub struct ResumeCandidate {
    pub run: TaskRun,
    pub lease: Option<RunLease>,
    /// The latest session's wrapper registration.
    pub wrapper: Option<RunProcess>,
    /// Its `resume_started` events so far, counted as ADR-0047 decision
    /// 24 says.
    pub resumes: crate::domain::resume::ResumeCount,
}

pub use crate::domain::resume::Exhaustion;

/// What a recovery round of a `failed` or `interrupted` run does to it
/// (ADR-0047 decision 40), recorded as `triage_finished`'s `action`: the
/// job's action once its preconditions held, or the ask it escalated to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriageAction {
    /// The task goes back to `ready`; the next claim makes a new run.
    Retry,
    /// The task goes back to `ready`, and its next run carries this run's
    /// `branch` over from `head` (ADR-0047 decision 24).
    RetryInherit {
        branch: Option<String>,
        head: CommitSha,
    },
    /// The run becomes `needs_session` with `instruction` as `last_error`,
    /// and the supervisor resumes it (ADR-0019 decision 1).
    Resume { instruction: String },
    /// Nothing moves; the job runs again from `recheck_at` (unix seconds)
    /// if the run is still where it was.
    Wait { recheck_at: i64 },
    /// The run stays; the `decide` ask `ask_id` waits for a person.
    Ask { ask_id: AskId },
}

impl TriageAction {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::RetryInherit { .. } => crate::domain::resume::RETRY_INHERIT,
            Self::Resume { .. } => "resume",
            Self::Wait { .. } => "wait",
            Self::Ask { .. } => "ask",
        }
    }
}

/// `asked_by` of the triage's `decide` asks: the supervisor that triaged.
pub const TRIAGE_ASKER: &str = "supervisor";

/// The run aggregate saved as it moves (ADR-0032's first kind): its claim,
/// provisioning, supervision, validation, landing decisions, integration
/// and clean-up. Every transition that takes a `token` is refused unless
/// that token holds the run's lease, so two processes never move one run
/// at once; the refusal is an error.
pub trait RunTransitions {
    /// Reserve the next dependency-ready task for the supervisor `token`:
    /// the run and its lease are created together.
    fn claim_for_supervisor(
        &mut self,
        base_commit: &CommitSha,
        token: &LeaseToken,
    ) -> Result<ClaimOutcome>;
    /// Record a runtime error and give the lease up, leaving the status;
    /// `session` is what became of the run's live session, if it had one.
    fn abandon_run(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        message: &str,
        reason: &Reason,
        session: Option<&serde_json::Value>,
    ) -> Result<TaskRun>;
    /// Take the single integration slot for `id` under `token`.
    fn begin_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        main: &CommitSha,
    ) -> Result<TaskRun>;
    /// Leave the integrating run to a session (`needs_session`).
    fn defer_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        detail: serde_json::Value,
    ) -> Result<TaskRun>;
    /// Leave the integrating run awaiting integration for a person
    /// (`integration_held`): a verification command failed on the host
    /// again after its retry (task 639).
    fn hold_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        detail: serde_json::Value,
    ) -> Result<TaskRun>;
    /// End the integrating run as failed, as its rewritten receipt says.
    fn fail_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        receipt: serde_json::Value,
    ) -> Result<TaskRun>;
    /// Give the slot back before `main` moved; the run returns to `revert_to`.
    fn abort_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        revert_to: &str,
        message: &str,
        reason: &Reason,
    ) -> Result<TaskRun>;
    /// Record the landing: the run is integrated and its task completed.
    fn finish_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        landing: &Landing,
        common_dir: &str,
    ) -> Result<(Task, TaskRun)>;
    fn record_cleanup_failure(&mut self, id: &RunId, message: &str, reason: &Reason) -> Result<()>;
    fn workspace_closed(&mut self, id: &RunId, token: &LeaseToken) -> Result<TaskRun>;
    fn cleanup_failed(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        message: &str,
        reason: &Reason,
    ) -> Result<TaskRun>;
    /// [`RunTransitions::claim_for_supervisor`], taking the first task of `order`
    /// that is still claimable, with `attributes` (an object) in its
    /// `run_claimed`, and the worker session `trial` chooses for the task
    /// there (ADR-0079 decisions 3 and 4). Only a task whose worker is one
    /// of `workers` is claimed (ADR-t813-2).
    #[allow(clippy::too_many_arguments)]
    fn claim_for_supervisor_in_order(
        &mut self,
        base_commit: &CommitSha,
        token: &LeaseToken,
        order: &[TaskId],
        attributes: Option<&serde_json::Value>,
        trial: &crate::domain::worker_model::WorkerTrial,
        routes: &[crate::domain::provider_switch::WorkerRoute],
    ) -> Result<ClaimOutcome>;
    /// Move the run's worker to `worker`, the other provider's, under the
    /// lease `token` (ADR-t813-2 decision 4): recorded as
    /// `provider_switched` with `payload`.
    fn switch_provider(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        worker: crate::domain::worker::Worker,
        payload: serde_json::Value,
    ) -> Result<TaskRun>;
    /// Record a runtime error on the run without changing its status.
    fn record_runtime_error(&mut self, id: &RunId, message: &str, reason: &Reason) -> Result<()>;
    /// Save the paths a claimed run is provisioned at.
    fn plan_run(&mut self, id: &RunId, token: &LeaseToken, plan: &RunPlan) -> Result<()>;
    fn workspace_created(&mut self, id: &RunId, token: &LeaseToken, workspace: &str) -> Result<()>;
    /// The session's wrapper exited: the run moves on by its exit code.
    fn finish_supervision(&mut self, id: &RunId, token: &LeaseToken) -> Result<TaskRun>;
    /// End a running run's lost session that could not be opened again
    /// (task 1372) as if its wrapper exited with `exit_code`.
    fn finish_lost_session(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        exit_code: i32,
    ) -> Result<TaskRun>;
    /// The session went idle after its receipt and stays open: validating.
    fn finish_supervision_live(&mut self, id: &RunId, token: &LeaseToken) -> Result<TaskRun>;
    fn finish_validation(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        validation: &Validation,
    ) -> Result<TaskRun>;
    /// Validate a rewritten receipt again.
    fn restart_validation(&mut self, id: &RunId, token: &LeaseToken) -> Result<TaskRun>;
    /// Apply a person's answer to an `approve_landing` ask.
    fn decide_landing(
        &mut self,
        id: &RunId,
        status: RunStatus,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<TaskRun>;
    /// Park a running run leased to `token` whose live session a recovery
    /// job's `resume` sends back to a session of its own (task 442): it
    /// becomes `needs_session` with `reason`, recorded as
    /// `recovery_parked` with `payload`; the lease stays for the session's
    /// exit.
    fn park_live(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<TaskRun>;
    /// Park a run awaiting integration leased to `token` whose session's
    /// workspace an adopter found gone while it could not land without that
    /// session (task 960): it becomes `needs_session` with `reason`,
    /// recorded as `session_gone_parked` with `payload`; the lease stays
    /// until the supervisor gives it back.
    fn park_gone_session(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<TaskRun>;
    /// Park a run awaiting integration leased to `token` whose e2e after
    /// its review failed (ADR-t1233-2 decision 3): it becomes
    /// `needs_session` with `reason`, recorded as `run_e2e_failed` with
    /// `payload`; the lease stays until the supervisor gives it back.
    fn park_e2e_failed(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<TaskRun>;
    /// Park a run awaiting integration that the landing recheck found no
    /// longer landing on main (ADR-0068 decision 3), leased to `token` or
    /// to nobody; `None` when it is not so any more.
    fn park_rechecked(
        &mut self,
        id: &RunId,
        token: Option<&LeaseToken>,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<Option<TaskRun>>;
    /// Record `workspace_closed` (`payload` and the `workspace_id`) of a
    /// workspace the triage or the supervisor's sweep closed.
    fn record_workspace_closed(
        &mut self,
        id: &RunId,
        workspace_id: &str,
        payload: serde_json::Value,
    ) -> Result<()>;
}

/// The run aggregate moved by recovery: the triage of ended runs, the
/// resumes of `needs_session` runs and the adoption of runs a stale
/// supervisor leased. The same lease rule as [`RunTransitions`] applies.
pub trait RunRecovery {
    /// Adoptable runs whose lease carries a token other than `token`.
    fn runs_leased_by_others(&self, token: &LeaseToken) -> Result<Vec<LeasedRun>>;
    /// Every run whose lease carries `token`, oldest first.
    fn runs_leased_by(&self, token: &LeaseToken) -> Result<Vec<TaskRun>>;
    /// The unclosed asks of the run, answered or not, oldest first.
    fn unclosed_run_asks(&self, run_id: &RunId) -> Result<Vec<crate::domain::Ask>>;
    /// Take over the stale lease `previous_token` holds on `id`; `None`
    /// when another process got there first or the lease is fresh again.
    fn adopt_run(
        &mut self,
        id: &RunId,
        previous_token: &LeaseToken,
        token: &LeaseToken,
        pid: u32,
        wrapper: serde_json::Value,
    ) -> Result<Option<TaskRun>>;
    /// Lease an `awaiting_integration` run nobody holds (or whose lease
    /// is stale) to `token` for its review; `None` when another process
    /// took it or it changed meanwhile.
    fn lease_for_review(&mut self, id: &RunId, token: &LeaseToken) -> Result<Option<TaskRun>>;
    /// [`Self::lease_for_review`] for the e2e a run queued to land runs
    /// first (ADR-t1233-2).
    fn lease_for_e2e(&mut self, id: &RunId, token: &LeaseToken) -> Result<Option<TaskRun>>;
    /// Recover an orphaned run whose `checked_processes` registered
    /// processes the caller found dead.
    fn recover_run(
        &mut self,
        id: &RunId,
        checked_processes: usize,
        report: serde_json::Value,
    ) -> Result<TaskRun>;
    /// The latest `failed` / `interrupted` run of every task in progress.
    fn runs_to_triage(&self) -> Result<Vec<TaskRun>>;
    /// Take the run's lease for a recovery round, recording `request` as
    /// its `recovery_requested` first when there is one; the round, or
    /// `None` when another process has it.
    /// `launch` is what its job is started with (ADR-0079 decision 7),
    /// recorded as the `launch` of `triage_started`.
    fn begin_triage(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        request: Option<serde_json::Value>,
        launch: &crate::domain::actor_model::ActorLaunch,
    ) -> Result<Option<(TaskRun, usize)>>;
    /// Act on the round's outcome and record `triage_finished`, then each
    /// of `also` (kind, payload), in one transaction.
    fn finish_triage(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        action: &TriageAction,
        payload: serde_json::Value,
        also: Vec<(EventKind, serde_json::Value)>,
    ) -> Result<TaskRun>;
    /// Apply a person's answer to the triage's `decide` ask.
    fn decide_triage(
        &mut self,
        id: &RunId,
        ask_id: AskId,
        answer: &str,
        reason: &str,
    ) -> Result<TaskRun>;
    fn runs_needing_session(&self) -> Result<Vec<ResumeCandidate>>;
    /// Take the lease of a `needs_session` run for a resume; the attempt.
    fn begin_resume(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        main: &CommitSha,
        reason: Option<&str>,
        config: crate::domain::resume::ResumeConfig,
    ) -> Result<Option<(TaskRun, usize)>>;
    fn finish_resume(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        status: Option<RunStatus>,
        reason: Option<&str>,
        keep_lease: bool,
        payload: serde_json::Value,
    ) -> Result<TaskRun>;
    /// Move a run an earlier resume resolved on without a session.
    fn skip_resume(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        head: &CommitSha,
        main: &CommitSha,
        approved: bool,
    ) -> Result<Option<TaskRun>>;
    /// Fail a run whose resumes are used up, naming the ask for a person
    /// or retrying its task with its branch carried over.
    fn exhaust_resumes(
        &mut self,
        id: &RunId,
        exhaustion: &Exhaustion,
        reason: &str,
        config: crate::domain::resume::ResumeConfig,
    ) -> Result<Option<TaskRun>>;
}

/// The state processes coordinate through (ADR-0032's second kind): run
/// leases and heartbeats, supervisor registrations and their handoffs, the
/// wrapper and agent processes of runs, the backend's slots and the
/// repository the queue is bound to.
pub trait RunCoordination {
    /// Refresh every lease `token` holds; how many there were.
    fn heartbeat_leases(&self, token: &LeaseToken) -> Result<usize>;
    fn register_supervisor(
        &mut self,
        token: &LeaseToken,
        pid: u32,
        parallel: u32,
        binary_version: &str,
    ) -> Result<SupervisorRegistration>;
    /// Whether a registration under `token` was removed.
    fn deregister_supervisor(&self, token: &LeaseToken) -> Result<bool>;
    /// Remove the registration under `token` and record the queue event of
    /// `kind` with the payload `stopped` builds from the removed row, in one
    /// transaction; `false`, recording nothing, when there was no row.
    fn prune_supervisor(
        &self,
        token: &LeaseToken,
        kind: EventKind,
        stopped: &dyn Fn(&SupervisorRegistration) -> serde_json::Value,
    ) -> Result<bool>;
    /// Every registered supervisor, oldest first, alive or not.
    fn supervisors(&self) -> Result<Vec<SupervisorRegistration>>;
    fn release_lease(&mut self, id: &RunId, token: &LeaseToken) -> Result<()>;
    /// Mark `token`'s registration as one that takes a handoff.
    fn accept_handoff(&self, token: &LeaseToken) -> Result<()>;
    /// Ask the supervisor `token` to exec `binary`; `false` when it is not
    /// registered or does not take a handoff (ADR-0045 decision 10).
    fn request_handoff(&self, token: &LeaseToken, binary: &str) -> Result<bool>;
    /// The binary the supervisor `token` was asked to exec, if any.
    fn handoff_request(&self, token: &LeaseToken) -> Result<Option<String>>;
    /// Atomically take the matching request immediately before an exec.
    /// A withdrawal or replacement wins if it reached the queue first.
    fn take_handoff(&self, token: &LeaseToken, binary: &str) -> Result<bool>;
    /// Withdraw a request to exec `binary` not taken yet.
    fn cancel_handoff(&self, token: &LeaseToken, binary: &str) -> Result<bool>;
    /// Take `token`'s registration back under `binary_version` after an
    /// exec, clearing the request.
    fn resume_registration(
        &mut self,
        token: &LeaseToken,
        pid: u32,
        binary_version: &str,
    ) -> Result<SupervisorRegistration>;
    /// Turn the automatic update of the supervisor `token` on or off
    /// (ADR-0045 decision 17).
    fn set_auto_update(&self, token: &LeaseToken, enabled: bool) -> Result<()>;
    /// Record the supervisor `token`'s `parallel`, `max_waiting`,
    /// `runtime_planners` and `claim_spacing` in use (ADR-0062 decision 7,
    /// ADR-t1479-1) and where each comes from (task 698).
    fn set_slot_limits(
        &self,
        token: &LeaseToken,
        limits: crate::domain::slot_limits::SlotLimits,
    ) -> Result<()>;
    /// Record the supervisor `token`'s `--max-load`, `None` when its load
    /// hold is off (ADR-t1479-1: `status` reads it for the claim spacing).
    fn set_max_load(&self, token: &LeaseToken, max_load: Option<f64>) -> Result<()>;
    /// Record the executables of the supervisor `token`'s providers as it
    /// resolved them at its start (ADR-t813-2).
    fn set_supervisor_providers(
        &self,
        token: &LeaseToken,
        providers: &[crate::domain::worker::ProviderCheck],
    ) -> Result<()>;
    fn holds_lease(&self, id: &RunId, token: &LeaseToken) -> Result<bool>;
    fn run_leases(&self) -> Result<Vec<RunLease>>;
    fn run_lease(&self, id: &RunId) -> Result<Option<RunLease>>;
    /// Point the queue at `common_dir` whatever it was bound to, and return
    /// the previous binding (`rebind`, ADR-0020).
    fn rebind_repository(&mut self, common_dir: &str) -> Result<Option<String>>;
    /// Git common directory the queue is bound to, if any.
    fn repository_binding(&self) -> Result<Option<String>>;
    fn bind_repository(&mut self, common_dir: &str) -> Result<()>;
    fn assert_repository(&self, common_dir: &str) -> Result<()>;
    /// One heartbeat of the process `token`: its registration and every
    /// lease it holds; how many leases there were.
    fn heartbeat(&mut self, token: &LeaseToken) -> Result<HeartbeatWrite>;
    /// The processes registered for the run (its wrapper and agent).
    fn processes(&self, id: &RunId) -> Result<Vec<RunProcess>>;
    /// Record how `up` started the supervisor `token`.
    fn set_supervisor_mode(
        &self,
        token: &LeaseToken,
        mode: SupervisorMode,
        workspace_id: Option<&str>,
    ) -> Result<()>;
    fn register_wrapper(&mut self, id: &RunId, token: &LeaseToken, pid: u32) -> Result<()>;
    fn register_resume_wrapper(&mut self, id: &RunId, token: &LeaseToken, pid: u32) -> Result<()>;
    /// Forget the processes of a running run whose session was lost, so
    /// that a new wrapper may register for it (task 1372); refused when a
    /// wrapper other than the `lost` pid has not recorded its exit.
    fn clear_lost_session(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        lost: Option<u32>,
    ) -> Result<()>;
    /// The lost session of a running run was opened again in `workspace`
    /// (its `attempt`): the run's workspace, `workspace_created` and
    /// `auto_repaired` with `repaired`.
    fn session_reopened(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        workspace: &str,
        attempt: u64,
        repaired: serde_json::Value,
    ) -> Result<()>;
    fn register_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32) -> Result<()>;
    fn register_resume_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32)
    -> Result<()>;
    /// Make the process of a later turn of a headless session the run's
    /// agent, in place of the earlier turn's.
    fn register_turn_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32) -> Result<()>;
    fn heartbeat_wrapper(&self, id: &RunId, pid: u32) -> Result<()>;
    fn wrapper_exited(&mut self, id: &RunId, pid: u32, exit_code: i32) -> Result<()>;
    /// The leases `token` holds (every lease with `None`) and the
    /// `parallel` it registered (null without a supervisor).
    fn backend_slots(&self, token: Option<&LeaseToken>) -> Result<(i64, Option<i64>)>;
}

/// The sessions around runs: the workspaces `up` opened, the planner
/// sessions and the Claude session spans the hooks and transcripts report.
pub trait SessionRegistry {
    /// The workspace `up` recorded for `role`.
    fn session_workspace(&self, role: SessionRole) -> Result<Option<String>>;
    /// Record the cmux workspace `up` opened for `role`, replacing any
    /// earlier one (ADR-0026).
    fn register_session_workspace(&self, role: SessionRole, workspace_id: &str) -> Result<()>;
    /// Forget the workspace of `role`; `false` when none was recorded.
    fn remove_session_workspace(&self, role: SessionRole) -> Result<bool>;
    /// Forget the workspaces recorded for a role `up` no longer opens.
    fn forget_retired_session_workspaces(&self) -> Result<usize>;
    /// Record a new planner session (ADR-0041 decisions 1, 6) before its
    /// workspace opens.
    fn open_planner(
        &self,
        origin: PlannerOrigin,
        proposal: Option<ProposalId>,
    ) -> Result<PlannerSession>;
    /// The UUID of the workspace cmux opened for the planner.
    fn planner_workspace_created(&self, id: PlannerId, workspace_id: &str) -> Result<()>;
    /// Give the planner up; the first close and its error are kept.
    fn close_planner(&self, id: PlannerId, error: Option<&str>) -> Result<PlannerSession>;
    /// Close the planner's row as the runtime ends it and record
    /// `planner_closed` with `payload` (ADR-t1300-1), once: `false` when the
    /// row was closed already.
    fn end_planner(&self, id: PlannerId, payload: &serde_json::Value) -> Result<bool>;
    fn planner(&self, id: PlannerId) -> Result<PlannerSession>;
    /// The planners not closed, oldest first; with `all`, every planner.
    fn planners(&self, all: bool) -> Result<Vec<PlannerSession>>;
    /// Record the route the planner's agent runs on (ADR-t1394-2), before
    /// its session starts.
    fn set_planner_route(&self, id: PlannerId, route: crate::domain::PlannerRoute) -> Result<()>;
    /// The `turn_*` events of headless planner `id` and its
    /// `provider_waiting`, oldest first.
    fn planner_turn_events(&self, id: PlannerId) -> Result<Vec<RunEvent>>;
    /// The planner's session wrapper registers itself, once.
    fn register_planner_wrapper(&self, id: PlannerId, pid: u32) -> Result<()>;
    fn register_planner_agent(&self, id: PlannerId, wrapper_pid: u32, agent: u32) -> Result<()>;
    fn heartbeat_planner(&self, id: PlannerId, wrapper_pid: u32) -> Result<()>;
    fn planner_exited(&self, id: PlannerId, wrapper_pid: u32, exit_code: i32) -> Result<()>;
    /// Record `planner_unresponsive` about a planner of the runtime's
    /// nothing was seen of within the planner timeout (task 805), once per
    /// planner: `payload` names it by `planner_id` with `subject:
    /// "planner"`. `false` when it was recorded before.
    fn planner_silent(&self, id: PlannerId, payload: serde_json::Value) -> Result<bool>;
    /// The planners not closed that such a `planner_unresponsive` names,
    /// each with that event: the inbox's attention.
    fn silent_planners(&self) -> Result<Vec<(PlannerSession, RunEvent)>>;
    /// Record the finished transcript turns of the Claude session spans
    /// still open, and finalize closed hook spans awaiting intake (ADR-0048
    /// decision 8); returns how many spans got turns or final measurements.
    fn record_session_turns(&self) -> Result<usize>;
    /// Record what the plugin's hook reported of an inbox or planner
    /// session (ADR-0048 decision 6): its span opened, gone on with or
    /// closed. Only the spans are written.
    fn record_session_hook(
        &self,
        hook: &crate::domain::sessions::SessionHook,
    ) -> Result<serde_json::Value>;
    /// The open spans the hook recorded, each with its workspace.
    fn hook_session_workspaces(&self) -> Result<Vec<(EventId, String)>>;
    /// Close, as `inferred`, the spans among `gone` the hook recorded that
    /// are still open: their workspace is gone (ADR-0048 decision 7).
    /// Returns how many it closed.
    fn close_gone_sessions(&self, gone: &[EventId]) -> Result<usize>;
    /// Close the run's review span still open, as `job_finished` now: its
    /// headless job ended (or could not start) without a verdict, and its
    /// `review_failed` waits for the session's `/exit` (task 541); returns
    /// how many spans it closed. `session` is what the job's output said
    /// of its session when its provider names it itself (Codex,
    /// ADR-t1063-1 decision 6): the span takes its thread and model, as
    /// `review_failed` records them.
    fn close_review_session(
        &self,
        id: &RunId,
        session: Option<&crate::domain::headless_job::JobSession>,
    ) -> Result<usize>;
}

/// A successful lookup found no run; storage/read errors remain distinct.
#[derive(Debug)]
pub struct RunNotFound(pub RunId);

impl fmt::Display for RunNotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "run {} does not exist", self.0)
    }
}

impl std::error::Error for RunNotFound {}

/// Runs and their events as read (ADR-0032's third kind), and the events
/// recorded outside a run transition.
pub trait RunLog {
    /// The latest `limit` steps of the automatic update (its `update_*`
    /// queue events), newest first.
    fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>>;
    /// The latest `limit` events of the e2e gates, newest first: the
    /// automatic update's (`update_e2e_passed`, `update_failed`) and the
    /// runtime's e2e of the runs (`run_e2e_finished`, `run_e2e_failed`,
    /// ADR-t1233-2 decision 5), for a marked test failing in a row.
    fn e2e_gate_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        self.update_events(limit)
    }
    fn active_runs(&self) -> Result<Vec<TaskRun>>;
    /// Every run of the queue, oldest first.
    fn all_runs(&self) -> Result<Vec<TaskRun>>;
    /// Every run event, oldest first, for `stats`.
    fn all_events(&self) -> Result<Vec<RunEvent>>;
    /// Per task, its newest event of one of `kinds` (ADR-0069).
    fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>>;
    /// Returns [`RunNotFound`] only when the lookup succeeded with no row.
    fn run(&self, id: &RunId) -> Result<TaskRun>;
    fn runs_with_status(&self, status: RunStatus) -> Result<Vec<TaskRun>>;
    /// The run awaiting integration longest, by validation time.
    fn next_awaiting_integration(&self) -> Result<Option<TaskRun>>;
    fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>>;
    fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool>;
    fn record_runtime_event(
        &self,
        id: &RunId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<()>;
    /// The workspaces of the ended runs the triage does not take, for the
    /// supervisor's sweep.
    fn ended_run_workspaces(&self) -> Result<Vec<EndedRunWorkspace>>;
    /// The worktrees of the runs nobody leases that ended or whose task is
    /// `completed` / `canceled`, for the supervisor's clean-up of the disk.
    fn ended_run_worktrees(&self) -> Result<Vec<EndedRunWorktree>>;
    /// Run `id` as [`Self::ended_run_worktrees`] would list it, read alone
    /// (task 1586); `None` when it would not be listed.
    fn ended_run_worktree(&self, id: &RunId) -> Result<Option<EndedRunWorktree>>;
    /// When an observation of `mode` last started or finished.
    fn last_observe(&self, mode: &str) -> Result<Option<i64>>;
    /// The newest `run_events` id, 0 for an empty queue.
    fn latest_event_id(&self) -> Result<EventId>;
    /// The latest run of every `in_progress` task, oldest first.
    fn latest_runs_in_progress(&self) -> Result<Vec<TaskRun>>;
    /// The `integrated` runs whose push of `main` failed after the latest
    /// successful push, oldest first.
    fn runs_with_pending_push(&self) -> Result<Vec<TaskRun>>;
    /// The run whose workspace is `workspace_id`, the latest one first.
    fn run_in_workspace(&self, workspace_id: &str) -> Result<Option<RunId>>;
    /// Record `backend_call_failed`, on `run` when the call was for one.
    fn record_backend_failure(&self, run: Option<&RunId>, payload: serde_json::Value)
    -> Result<()>;
    /// Record an event of the queue itself, on no task, goal or run.
    fn record_queue_event(&self, kind: EventKind, payload: serde_json::Value) -> Result<EventId>;
    /// Record `inbox_nudged` with `payload` unless one with the same
    /// `absent_since` and `attempt` is recorded (ADR-t906-1 decision 1
    /// (3)), in one write transaction: `false` when another supervisor
    /// recorded it first. Only the claimer nudges the inbox.
    fn claim_inbox_nudge(&self, payload: serde_json::Value) -> Result<bool>;
    /// Record `kind` (`inbox_watcher_absent` or `inbox_watcher_returned`)
    /// with `payload` unless the latest of the two is already `kind`, in
    /// one write transaction (task 1021): `false` when the state did not
    /// change or another supervisor recorded the change first.
    fn record_inbox_watcher_change(
        &self,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<bool>;
    /// The newest event of `kind`, on whatever task, goal or run.
    fn latest_event_of(&self, kind: &str) -> Result<Option<RunEvent>>;
    /// The newest `limit` events of `kind`, on whatever task, goal or run,
    /// newest first.
    fn latest_events_of(&self, kind: &str, limit: usize) -> Result<Vec<RunEvent>>;
    /// The newest event of the queue itself (on no run) of one of `kinds`.
    fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>>;
    /// The events of one of `kinds` with `after < id <= upto`, oldest
    /// first, at most `limit`.
    fn events_of_between(
        &self,
        kinds: &[&str],
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>>;
    /// Record `requester`, a headless job whose verdict the caller
    /// applies, as `requested_by` on the events written until it is
    /// replaced (ADR-t728-1 decision 1, task 730), and return
    /// the `requested_by` it replaces, for the caller to put back with
    /// [`RunLog::restore_request`] so a nested request does not clear the
    /// outer one (task 783). A store that records no actors ignores it and
    /// returns `None`.
    fn request_as(
        &self,
        _requester: Option<&crate::domain::actor::ActorContext>,
    ) -> Option<String> {
        None
    }
    /// Put back the `requested_by` [`RunLog::request_as`] returned. A store
    /// that records no actors ignores it.
    fn restore_request(&self, _previous: Option<String>) {}
    /// Record `actor` as the actor of the events written from now on and
    /// return the one it replaces, for the [`super::integrate::Integrator`]
    /// to write the landing as itself (ADR-t728-2). A store that records no
    /// actors ignores it and returns `None`.
    fn act_as(
        &self,
        _actor: crate::domain::actor::ActorContext,
    ) -> Option<crate::domain::actor::ActorContext> {
        None
    }
}

/// The events `events`, `timeline` and `watch` read past a cursor.
pub trait EventReads {
    /// Events with `after < id <= upto` that `filter` keeps, oldest first,
    /// at most `limit`. A pure read.
    fn events_between(
        &self,
        after: EventId,
        upto: EventId,
        filter: &crate::domain::EventFilter,
        limit: usize,
    ) -> Result<Vec<RunEvent>>;
}

/// What one role wrote in a window ([`ObserverLog::written_by`]): finding
/// ids it recorded, updated and closed (resolved or dismissed), its
/// asks' ids, and the findings it left without an ask.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WrittenBy {
    pub recorded: Vec<i64>,
    pub updated: Vec<i64>,
    pub closed: Vec<i64>,
    pub asks: Vec<i64>,
    /// The findings it recorded or updated and did not close that have
    /// no `blocked` ask: none it opened in the window and none open now
    /// (ADR-t451-1 decision 2, the reading it kept to the finding).
    pub without_ask: Vec<i64>,
}

/// What the observer reads of the queue's record of its observations and
/// of what it wrote (ADR-0044).
pub trait ObserverLog {
    /// The id of the last event recorded before `unix` (seconds), 0 when
    /// there is none: a cursor that reads everything from that time on.
    fn event_id_before(&self, unix: i64) -> Result<EventId>;
    /// The newest ask's ID: the mark [`Self::written_by`] counts past.
    fn ask_high_water(&self) -> Result<AskId>;
    /// What `role` wrote after the marks: the findings it recorded,
    /// updated and closed after `event_id` and its asks after `ask_id`.
    fn written_by(&self, role: &str, event_id: EventId, ask_id: AskId) -> Result<WrittenBy>;
    /// The last observation of `mode` that ran its agent: the id and the
    /// payload of its `observe_finished` (a skipped one is not).
    fn last_observation(&self, mode: &str) -> Result<Option<(EventId, serde_json::Value)>>;
    /// How many events after `after` the observer did not write itself
    /// (ADR-0044), its own spans (`span_kind`) excluded.
    fn events_besides(&self, role: &str, span_kind: &str, after: EventId) -> Result<i64>;
    /// The newest `limit` observations, newest first: each
    /// `observe_finished` with the `observe_started` of the same directory
    /// when there is one (a skipped observation has none).
    fn observations(&self, limit: usize) -> Result<Vec<(RunEvent, Option<RunEvent>)>>;
}

/// What the mark commands read and record (ADR-0051 decision 12): the
/// queue's events, to resolve `--at` and list the marks, and a new event
/// of the queue itself. Every [`RunLog`] is one.
pub trait MarkLog {
    /// Every event of the queue, oldest first.
    fn events(&self) -> Result<Vec<RunEvent>>;
    /// Record an event of the queue itself, on no task, goal or run.
    fn record_event(&self, kind: EventKind, payload: serde_json::Value) -> Result<EventId>;
}

impl<T: RunLog + ?Sized> MarkLog for T {
    fn events(&self) -> Result<Vec<RunEvent>> {
        self.all_events()
    }

    fn record_event(&self, kind: EventKind, payload: serde_json::Value) -> Result<EventId> {
        self.record_queue_event(kind, payload)
    }
}

/// What reports read and record of the queue as a whole: the written
/// reports, KPI breaches, forecasts and the lookups `stats` joins runs with.
pub trait QueueRecords {
    /// The commits that landed the `limit` completed tasks most related to
    /// `task` (`dagq related`, ADR-0046), for the files it is expected to
    /// touch when it declares no paths (ADR-0069).
    fn related_landed_commits(&self, task: TaskId, limit: usize) -> Result<Vec<String>>;
    /// The `limit` tasks most related to `task`, best first, with their
    /// clues, kept to `statuses` (empty: any status; `dagq related`,
    /// ADR-0046 decision 4).
    fn related_tasks(&self, task: TaskId, statuses: &[String], limit: usize)
    -> Result<RelatedPage>;
    /// The documents matching `query`, best first (`dagq search`, ADR-0046).
    fn search_documents(&self, query: &SearchQuery) -> Result<SearchPage>;
    /// The goal of every task, for `stats`.
    fn task_goals(&self) -> Result<HashMap<TaskId, Option<GoalId>>>;
    /// The title of every task, for `stats`.
    fn task_titles(&self) -> Result<HashMap<TaskId, String>>;
    /// The change of every task (none for a task without one, ADR-t980-1),
    /// for `stats`, `kpi` and `forecast`.
    fn task_changes(&self) -> Result<HashMap<TaskId, Option<TaskChange>>>;
    /// The findings `query` lists, larger impact first (`findings`), for
    /// the KPI report's open findings.
    fn findings(&self, query: &crate::domain::FindingQuery) -> Result<Vec<FindingView>>;
    /// The reports recorded as written (`report_written`, ADR-0051
    /// decision 20): each (period, label).
    fn reports_written(&self) -> Result<std::collections::HashSet<(String, String)>>;
    /// Record `report_written` with `payload` (its `period` and `label`)
    /// unless the same report is recorded; `false` when it is.
    fn record_report_written(&self, payload: serde_json::Value) -> Result<bool>;
    /// The KPI breaches started and not resolved (ADR-0051 decision 18),
    /// each its `kpi_breach_started` payload.
    fn kpi_breaches_open(&self) -> Result<Vec<serde_json::Value>>;
    /// Record a breach's start or end unless it already stands so; a
    /// start is marked `pushed` while `push_day` (the local day, the
    /// day's limit) allows. Returns the payload recorded.
    fn record_kpi_breach(
        &self,
        kind: EventKind,
        payload: serde_json::Value,
        push_day: Option<(i64, usize)>,
    ) -> Result<Option<serde_json::Value>>;
    /// Record `kpi_push_abandoned` unless one stands since the latest
    /// push that succeeded (ADR-0051 decision 23); `false` when it does.
    fn record_kpi_push_abandoned(&self, payload: serde_json::Value) -> Result<bool>;
    /// Where every draft the runtime or a job registered came from, for
    /// `stats`' `draft_flow`.
    fn draft_origins(&self) -> Result<HashMap<TaskId, DraftOrigin>>;
    /// Record a forecast snapshot (`forecast_recorded`, ADR-0070 decision
    /// 3) unless another was recorded after `previous`; its ID, or `None`.
    fn record_forecast(
        &self,
        payload: serde_json::Value,
        previous: Option<EventId>,
    ) -> Result<Option<EventId>>;
}

/// The questions the runtime and its sessions put to a person (ADR-0022).
pub trait AskStore {
    /// Open the ask of the automatic update of `kind` (`update_failed`,
    /// `approve_update` or `approve_release`), closing an older open one of
    /// it (ADR-0073 decision 17); `subject` is the release an
    /// `approve_release` is about, and the keys of `details` (an object, or
    /// null) are written into its `ask_opened` (the release update's
    /// `plugin_only`).
    fn open_update_ask(
        &mut self,
        kind: crate::domain::AskKind,
        question: &str,
        options: &[&str],
        asked_by: &str,
        subject: Option<&str>,
        details: serde_json::Value,
    ) -> Result<crate::domain::Ask>;
    /// The payload of the `ask_opened` of the ask `id`; null when there is
    /// none.
    fn ask_opened_payload(&self, id: crate::domain::AskId) -> Result<serde_json::Value>;
    /// Whether the host's `[update]` of this queue looks for releases
    /// (`release` other than `off`, ADR-t618-1): whether a supervisor of a
    /// release build applies the release update's answers.
    fn release_updates_on(&self) -> bool;
    /// The answered, unclosed asks of the automatic update of `kind`,
    /// oldest first.
    fn update_answers(&self, kind: &crate::domain::AskKind) -> Result<Vec<crate::domain::Ask>>;
    /// Asks matching `query`, oldest first.
    fn asks(&self, query: AskQuery) -> Result<Vec<Ask>>;
    /// Whether the run has an ask of `kind` nobody closed, answered or not.
    fn has_unclosed_ask(&self, run_id: &RunId, kind: AskKind) -> Result<bool>;
    /// Register an ask, or return the open one it repeats.
    fn ask(&mut self, ask: NewAsk) -> Result<AskOutcome>;
    /// Open the authentication or cost ask of the hold with its run, or add
    /// the run to the open one (ADR-0047 decision 42).
    fn hold(&mut self, hold: crate::domain::NewHold) -> Result<crate::domain::HoldOutcome>;
    /// The open `queue_hold` ask that holds the run, if any.
    fn hold_of(&self, run_id: &RunId) -> Result<Option<Ask>>;
    /// Whether a `queue_hold` ask that holds the run is still unclosed:
    /// open, or answered and not yet applied by a supervisor.
    fn hold_unclosed(&self, run_id: &RunId) -> Result<bool>;
    /// Close the `queue_hold` asks of `reason` and `subject` nobody
    /// closed, answering the open ones `answer` (task 377).
    fn close_hold_asks(
        &mut self,
        reason: crate::domain::AskReason,
        subject: Option<&str>,
        answer: &str,
    ) -> Result<Vec<Ask>>;
    fn read_ask(&self, id: AskId) -> Result<Ask>;
    /// Write the answer of an open ask, given by `answerer` (its
    /// `answered_by` and the authority it carries).
    fn answer_as(
        &mut self,
        id: AskId,
        text: &str,
        answerer: crate::domain::Answerer,
    ) -> Result<Ask>;
    fn close_ask(&mut self, id: AskId) -> Result<Ask>;
    /// Answered `approve_landing` asks nobody closed.
    fn landing_answers(&self) -> Result<Vec<Ask>>;
    /// Answered `decide` asks of the triage nobody closed.
    fn triage_answers(&self) -> Result<Vec<Ask>>;
    /// Answered `worker_question` asks of the run not yet delivered.
    fn undelivered_answers(&self, run_id: &RunId) -> Result<Vec<Ask>>;
    fn ask_delivered(&mut self, id: AskId, workspace_id: &str) -> Result<Ask>;
    fn has_unclosed_worker_question(&self, run_id: &RunId) -> Result<bool>;
    /// Whether the run has a `worker_question` nobody closed that was
    /// created at or after `created_from` (unix seconds).
    fn has_unclosed_worker_question_since(&self, run_id: &RunId, created_from: i64)
    -> Result<bool>;
    /// When the run's `worker_question` closed last (unix seconds).
    fn last_worker_question_closed(&self, run_id: &RunId) -> Result<Option<i64>>;
    /// Close the run's `stuck_exit` asks nobody closed, with `answer`.
    fn close_stuck_exit_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>>;
    /// Close the run's `answer_prompt` asks nobody closed, with `answer`.
    fn close_answer_prompt_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>>;
    /// The run's `stalled` ask nobody closed, answered or not.
    fn unclosed_stalled_ask(&self, run_id: &RunId) -> Result<Option<Ask>>;
    /// Close the run's `stalled` asks nobody closed, with `answer`.
    fn close_stalled_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>>;
    /// Close the run's `stalled` asks nobody closed, with `answer`, and
    /// record `stall_resolved` (outcome `run_ended`) for each of its
    /// stalled detections with no end recorded yet, once.
    fn end_stalled_detections(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>>;
    /// Add `note` as a paragraph to the question of every ask of the run
    /// nobody closed, recording `ask_updated` with `why`; the asks noted.
    fn note_on_asks(&mut self, run_id: &RunId, note: &str, why: &str) -> Result<Vec<Ask>>;
    /// Close the run's `approve_landing` asks nobody closed, with `answer`.
    fn close_approve_landing_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>>;
    /// Close the `blocked` asks of the run, or of its task with no run
    /// named, nobody closed, with `answer`.
    fn close_blocked_asks(
        &mut self,
        run_id: &RunId,
        task_id: TaskId,
        answer: &str,
    ) -> Result<Vec<Ask>>;
}

/// Which asks [`AskStore::asks`] lists. By default the ones nobody closed;
/// `all` adds the closed ones, `open` keeps only the unanswered ones, and
/// `role` keeps those that wait for that role ([`Ask::waits_for`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct AskQuery {
    pub all: bool,
    pub open: bool,
    pub role: Option<SessionRole>,
}

/// How [`DraftPlannerStore::open_draft_planner`] ended.
#[derive(Debug, Clone)]
pub enum DraftPlannerStart {
    /// A planner of the runtime's is recorded for the bundle of drafts of
    /// `key` (ADR-t807-1): each member with which planner this is for it
    /// (1-based), oldest first; the caller opens its workspace. `exhausted`
    /// are the drafts left out as below.
    Opened {
        planner: Box<PlannerSession>,
        key: crate::domain::BundleKey,
        members: Vec<(DraftTarget, usize)>,
        exhausted: Vec<TaskId>,
    },
    /// [`crate::domain::MAX_DRAFT_PLANNERS`] planners ended without
    /// deciding each of these drafts: `draft_planner_exhausted` is
    /// recorded and a person decides.
    Exhausted { drafts: Vec<TaskId> },
    /// Not now: the draft moved on, or another planner took it.
    Skipped,
}

/// How [`DraftPlannerStore::open_finding_planner`] ended.
#[derive(Debug, Clone)]
pub enum FindingPlannerStart {
    /// A planner of the runtime's is recorded for the finding, its
    /// `attempt`-th since the finding was marked; the caller opens its
    /// workspace.
    Opened {
        planner: Box<PlannerSession>,
        finding: Box<Finding>,
        attempt: usize,
    },
    /// [`crate::domain::MAX_FINDING_PLANNERS`] planners ended without
    /// deciding the finding: `finding_planner_exhausted` is recorded and a
    /// person decides.
    Exhausted { attempts: usize },
    /// The improvements running reached the limit (ADR-0051 decision 25):
    /// the finding waits, `open`, for one to end.
    AtLimit(crate::domain::ImprovementLimit),
    /// Not now: the finding moved on, or another planner took it.
    Skipped,
}

/// How [`PlanRequestStore::open_request_planner`] ended.
#[derive(Debug, Clone)]
pub enum RequestPlannerStart {
    /// A planner of the runtime's is recorded for the request, its
    /// `attempt`-th for it; the caller hands it the request and opens its
    /// workspace.
    Opened {
        planner: Box<PlannerSession>,
        request: Box<crate::domain::plan_request::PlanRequest>,
        attempt: usize,
    },
    /// [`crate::domain::plan_request::MAX_REQUEST_PLANNERS`] planners ended
    /// without deciding the request: it is `exhausted`, with
    /// `request_planner_exhausted`, and the inbox decides.
    Exhausted { attempts: usize },
    /// Not now: the request moved on, or another planner took it.
    Skipped,
}

/// The planning requests the inbox records for planners of the runtime's
/// (ADR-t1394-1): the ones waiting for a planner, the planner opened for
/// each, and what its prompt reads of what a request refers to.
pub trait PlanRequestStore {
    /// The `open` requests waiting for a planner of the runtime's, oldest
    /// first: none open for it, no `planner_question` about it nobody
    /// closed.
    fn planner_requests(&self) -> Result<Vec<crate::domain::plan_request::PlanRequest>>;
    /// Record a planner of the runtime's for `request` after re-checking it
    /// in the same write transaction (`request_planner_opened`); with
    /// `answer`, one that carries that answered `planner_question` about
    /// it. Past [`crate::domain::plan_request::MAX_REQUEST_PLANNERS`]
    /// planners (without `answer`), the request is made `exhausted`
    /// instead.
    fn open_request_planner(
        &mut self,
        request: crate::domain::RequestId,
        answer: Option<AskId>,
    ) -> Result<RequestPlannerStart>;
    fn plan_request(
        &self,
        request: crate::domain::RequestId,
    ) -> Result<crate::domain::plan_request::PlanRequest>;
    /// The asks about `request`, oldest first: its planners' questions.
    fn request_asks(&self, request: crate::domain::RequestId) -> Result<Vec<Ask>>;
    /// The event `id`, when there is one: what a request refers to.
    fn event_by_id(&self, id: EventId) -> Result<Option<RunEvent>>;
}

/// Where the answer of a `planner_question` goes (ADR-0041 decision 13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannerAnswerRoute {
    /// Typed into the workspace of this planner of the runtime's, which
    /// works on the ask's task (its draft, or its proposal).
    Planner(Box<PlannerSession>),
    /// The draft's planner is gone: a new one is opened with the answer.
    NewPlanner,
    /// The draft moved on (submitted, canceled) with no planner left: the
    /// supervisor closes the ask.
    Close,
    /// None of these: a person delivers it through the inbox.
    Person,
}

/// The drafts the runtime or a job registered (ADR-0041 decision 16):
/// where each came from, the planner of the runtime's opened for each, and
/// the `planner_question` answers the supervisor types into a planner.
pub trait DraftPlannerStore {
    /// Register every still-unrecorded entry of one receipt atomically,
    /// including skipped entries and their `follow_up_registered` events.
    /// Snapshot the source task's goal and state in the same transaction;
    /// callers cannot supply registration-time goal facts.
    fn register_follow_ups(
        &mut self,
        run: &RunId,
        entries: Vec<FollowUpRegistration>,
        depth: i64,
    ) -> Result<Vec<crate::domain::RegisteredFollowUp>>;
    /// Record where a draft the runtime or a job registered came from, and
    /// what its planner is shown about it. A draft has one origin: a second
    /// call for it is refused.
    fn record_draft_origin(
        &mut self,
        task: TaskId,
        origin: DraftOrigin,
        material: &serde_json::Value,
    ) -> Result<()>;
    /// Where the draft came from, if the runtime or a job registered it.
    fn draft_origin(&self, task: TaskId) -> Result<Option<(DraftOrigin, serde_json::Value)>>;
    /// The drafts waiting for a planner of the runtime's, by ID: `draft`,
    /// in no proposal, with an origin, no planner open for it, no
    /// `planner_question` about it nobody closed, not kept as a draft by
    /// an answer and not exhausted.
    fn planner_drafts(&self) -> Result<Vec<DraftTarget>>;
    /// Record a planner of the runtime's for the bundle of `drafts`
    /// (ADR-t807-1: `draft_bundles`, `draft_bundle_members`, and
    /// `draft_planner_opened` on each) after re-checking them in the same
    /// write transaction: a draft that no longer waits, or whose bundle key
    /// is not the first one's, is left out. With `answer`, the planner
    /// carries that answered `planner_question` about the first draft,
    /// whose planner is gone, instead of that draft being a target.
    fn open_draft_planner(
        &mut self,
        drafts: &[TaskId],
        answer: Option<AskId>,
    ) -> Result<DraftPlannerStart>;
    /// The drafts a planner of the runtime's works on (its bundle's).
    fn planner_draft_tasks(&self, planner: PlannerId) -> Result<Vec<TaskId>>;
    /// The bundle a planner of the runtime's was opened for, if any.
    fn draft_bundle(&self, planner: PlannerId) -> Result<Option<crate::domain::DraftBundleView>>;
    /// Answered `planner_question` asks nobody closed, oldest first.
    fn planner_answers(&self) -> Result<Vec<Ask>>;
    /// Where the answer of an answered `planner_question` goes.
    fn planner_answer_route(&self, ask: &Ask) -> Result<PlannerAnswerRoute>;
    /// Claim the typing of the answer of `ask` into `planner`'s
    /// `workspace` (`planner_answer_claimed`), in one write transaction:
    /// `false` when another process claimed it, the ask was closed, or its
    /// answer no longer goes to that planner. Only the claimer types it.
    fn claim_planner_answer(
        &mut self,
        ask: AskId,
        planner: PlannerId,
        workspace: &str,
    ) -> Result<bool>;
    /// Close an answered `planner_question` nobody needs any more (its
    /// draft moved on), recording `planner_answer_closed` with `why`.
    fn close_planner_answer(&mut self, ask: AskId, why: &str) -> Result<()>;
    /// Record an event of a task that has no run (a draft's).
    fn record_task_event(
        &mut self,
        task: TaskId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<()>;
    /// Drafts still `draft` whose planners were used up
    /// (`draft_planner_exhausted`), by ID.
    fn exhausted_drafts(&self) -> Result<Vec<Task>>;
    /// The `follow_up_depth` of a task (ADR-0037 decision 6).
    fn follow_up_depth(&self, task: TaskId) -> Result<i64>;
    fn set_follow_up_depth(&mut self, task: TaskId, depth: i64) -> Result<()>;
    /// The findings waiting for a planner of the runtime's (ADR-0044
    /// decision 19): `open`, marked for a proposal, no planner open for
    /// it, no `planner_question` about it nobody closed and its planners
    /// since the mark not used up; the oldest mark first.
    fn planner_findings(&self) -> Result<Vec<Finding>>;
    /// Record a planner of the runtime's for `finding`
    /// (`finding_planner_opened`) after re-checking it in the same write
    /// transaction. With `answer`, the planner carries that answered
    /// `planner_question` about the finding, whose planner is gone.
    /// Without `answer`, none is opened while the improvements running
    /// reach `limit` (ADR-0051 decision 25).
    fn open_finding_planner(
        &mut self,
        finding: FindingId,
        answer: Option<AskId>,
        limit: usize,
    ) -> Result<FindingPlannerStart>;
    /// The improvement proposals running against `limit`.
    fn improvements(&self, limit: usize) -> Result<crate::domain::ImprovementLimit>;
    /// One finding as `findings ID --full` shows it.
    fn finding_view(&self, finding: FindingId) -> Result<FindingView>;
    /// The asks about the finding, oldest first.
    fn finding_asks(&self, finding: FindingId) -> Result<Vec<Ask>>;
    /// End the `proposed` findings whose proposal ended: `resolved`, or
    /// `open` again without the mark.
    fn settle_findings(&mut self) -> Result<Vec<(FindingId, FindingStatus)>>;
    /// Findings whose planners were used up (`finding_planner_exhausted`)
    /// and that still wait, by ID.
    fn exhausted_findings(&self) -> Result<Vec<Finding>>;
    /// Record an event on the finding's target (on nothing for the queue).
    fn record_finding_event(
        &mut self,
        finding: FindingId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<()>;
    /// Whether the answer of `ask` was typed into `workspace`.
    fn ask_delivered_to(&self, ask: AskId, workspace: &str) -> Result<bool>;
    /// When the latest claim of the typing of the answer of `ask` into
    /// `workspace` was taken (`planner_answer_claimed`): a time before the
    /// typing, unlike the ask's close after it. `None` without a claim (an
    /// answer a new planner carried in its prompt).
    fn answer_claimed_at(&self, ask: AskId, workspace: &str) -> Result<Option<i64>>;
    /// Whether typing the answer of `ask` ever failed.
    fn ask_delivery_failed(&self, ask: AskId) -> Result<bool>;
}

/// One receipt entry prepared by integrate for atomic registration.
pub struct FollowUpRegistration {
    pub index: usize,
    pub entry: serde_json::Value,
    pub category: String,
    /// The worker's membership proposal as written, or null (ADR-t1504-2
    /// decision 11).
    pub membership_proposal: serde_json::Value,
    pub draft: Option<NewTask>,
    pub skipped: Option<&'static str>,
}

/// A planner of the runtime's that holds a place under
/// `--runtime-planners` while a revise with no planner waits (task 884):
/// its state and why the supervisor does not end it (`busy`, empty when it
/// is about to be asked to exit).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannerHold {
    pub planner_id: PlannerId,
    pub state: String,
    pub busy: Vec<&'static str>,
    pub proposal_id: Option<ProposalId>,
    pub draft_task_id: Option<TaskId>,
    pub finding_id: Option<FindingId>,
    pub request_id: Option<crate::domain::RequestId>,
}

/// A plan review job the queue recorded (ADR-0041 decision 11): the
/// proposal it reviews, its attempt at that proposal, the first task of the
/// proposal (where its events are recorded) and its directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanReviewJob {
    pub id: i64,
    pub proposal_id: ProposalId,
    pub attempt: usize,
    pub anchor: TaskId,
    pub dir: PathBuf,
    /// The job's session id, given to it by the runtime (ADR-0048
    /// decision 4); `None` for a provider that names its session itself
    /// (Codex, whose thread its end records).
    pub session_id: Option<String>,
}

/// What the runtime makes of a plan review's verdict before it is applied:
/// the decision it acts on (a `revise` past [`crate::domain::MAX_PLAN_REVISES`]
/// is a `concern`, `overridden` saying why), the reasons a revise carries to
/// the planner (the verdict's, then the precedents it named), and the
/// `approve_plan` ask a concern opens.
#[derive(Debug, Clone)]
pub struct PlanReviewApply {
    pub verdict: PlanReviewVerdict,
    pub decision: PlanReviewDecision,
    pub overridden: Option<String>,
    pub revise_reasons: Vec<String>,
    pub ask: Option<NewAsk>,
    /// What the runtime made of a `concern` verdict (ADR-t451-1 decision
    /// 4), recorded as `plan_concern_decided`; `None` for any other.
    pub concern: Option<crate::domain::plan_review::PlanConcernDecision>,
    pub duration_secs: u64,
    /// The session the job's output names (Codex's thread and model,
    /// ADR-t1063-1 decision 6); `None` for a Claude job.
    pub session: Option<crate::domain::headless_job::JobSession>,
    /// What the job's prompt took, recorded as `prompt_bytes` (task 1561).
    pub prompt_bytes: Option<crate::application::prompt::PromptBytes>,
}

/// Why a plan review job failed (`plan_review_failed`).
#[derive(Debug, Clone, Default)]
pub struct PlanReviewFailure {
    pub error: String,
    pub duration_secs: u64,
    /// The session the job's output names, as in [`PlanReviewApply`].
    pub session: Option<crate::domain::headless_job::JobSession>,
    /// The job's provider could not be used, and why (ADR-t1063-1
    /// decision 4): its row ends `interrupted` and the proposal is not
    /// held, so it is reviewed again at once, on the other provider unless
    /// both are held.
    pub unusable: Option<(
        crate::domain::Provider,
        crate::domain::provider_switch::SwitchReason,
    )>,
    /// What the job's prompt took, recorded as `prompt_bytes`; `None` when
    /// no prompt was written (task 1561).
    pub prompt_bytes: Option<crate::application::prompt::PromptBytes>,
}

/// A ready task a verdict took back to submitted, with the proposal of its
/// own a planner fixes it in (ADR-0041 decision 14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReopenedTask {
    pub task_id: TaskId,
    pub proposal_id: ProposalId,
}

/// What applying a verdict did. `stale`: nothing, because the job's row or
/// its proposal moved on meanwhile (the job is finished as `interrupted`).
#[derive(Debug, Clone, Default)]
pub struct PlanReviewApplied {
    pub stale: bool,
    pub reopened: Vec<ReopenedTask>,
    /// The `approve_plan` ask a concern opened; `created: false` when an
    /// open one already stood.
    pub ask: Option<AskOutcome>,
}

/// A proposal sent back to its planner (`revising`), with where its revise
/// stands: the reasons, when and to which planner they went (`None`: the
/// supervisor still has to deliver them), and when the inbox was told the
/// planner did not answer.
#[derive(Debug, Clone)]
pub struct RevisingProposal {
    pub proposal: Proposal,
    pub reasons: Vec<String>,
    /// Since when the revise waits (Unix seconds).
    pub revised_at: Option<i64>,
    pub sent_at: Option<i64>,
    pub planner_id: Option<PlannerId>,
    pub unresponsive_at: Option<i64>,
}

/// A proposal that waits for a person outside an ask: its plan review
/// failed (`plan_review_failed`), or its planner did not answer a revise
/// (`planner_unresponsive`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanReviewHold {
    pub proposal_id: ProposalId,
    pub anchor: TaskId,
    pub kind: &'static str,
    pub error: Option<String>,
}

/// What a person's `approve_plan` answer did to the proposal.
#[derive(Debug, Clone)]
pub struct PlanDecided {
    pub proposal: Proposal,
    pub answer: String,
}

/// Plan review (ADR-0041 decisions 11-15, 17): the submitted proposals it
/// takes, its one job at a time, the verdicts and answers the runtime
/// applies (each in one transaction), and the revises it delivers.
pub trait PlanReviewStore {
    /// Close unfinished reviews stopped for an exec handoff, owned only by
    /// `token`, including their session spans. Call before starting jobs.
    fn interrupt_plan_reviews_for_handoff(&mut self, token: &LeaseToken) -> Result<()>;
    /// Submitted proposals plan review may take now: not held, with a
    /// submitted task, and whether one of those has the interrupt priority.
    fn plan_review_candidates(&self) -> Result<Vec<PlanReviewCandidate>>;
    /// Record the start of a plan review of `proposal` by `token`, in the
    /// directory named by its row's ID under `plan_reviews_dir`
    /// (`plan_review_started`, with `cwd`, the checkout the job runs in, for
    /// its Claude session: ADR-0048). Rows of gone supervisors are finished
    /// as `interrupted` first. `None`: another supervisor's job runs, or the
    /// proposal is no longer a candidate. `launch` is what the job is
    /// started with (ADR-0079 decision 7), recorded as the event's `launch`.
    fn begin_plan_review(
        &mut self,
        proposal: ProposalId,
        token: &LeaseToken,
        plan_reviews_dir: &Path,
        cwd: &Path,
        launch: &crate::domain::actor_model::ActorLaunch,
    ) -> Result<Option<PlanReviewJob>>;
    /// Apply what the runtime made of the verdict and finish the job
    /// (`plan_review_finished`); an action the job may not take is an
    /// error, and nothing is applied.
    fn finish_plan_review(
        &mut self,
        job: &PlanReviewJob,
        token: &LeaseToken,
        apply: &PlanReviewApply,
    ) -> Result<PlanReviewApplied>;
    /// Record the job's failure (`plan_review_failed`, the inbox's) and
    /// hold the proposal as `failed`, unless the job moved on or its
    /// provider could not be used ([`PlanReviewFailure::unusable`]).
    fn fail_plan_review(
        &mut self,
        job: &PlanReviewJob,
        token: &LeaseToken,
        failure: &PlanReviewFailure,
    ) -> Result<()>;
    /// Every proposal sent back to its planner, oldest first.
    fn revising_proposals(&self) -> Result<Vec<RevisingProposal>>;
    /// Claim the delivery of the revise of `proposal` (it records when):
    /// `false` when another process claimed it, or it is no longer waiting.
    fn claim_revise(&mut self, proposal: ProposalId) -> Result<bool>;
    /// The claimed revise of `proposal` went to `planner`
    /// (`plan_revise_sent`); `opened` is what the planner's agent started
    /// with when the runtime opened that planner for it (its effort raised
    /// one step, ADR-0079 decision 7 (c)), `None` when it went to a live
    /// planner, whose effort is not raised.
    fn revise_sent(
        &mut self,
        proposal: ProposalId,
        planner: PlannerId,
        workspace: &str,
        opened: Option<&crate::domain::actor_model::ActorLaunch>,
    ) -> Result<()>;
    /// Take the revise of `proposal` back for another delivery: the
    /// planner it went to is gone before it submitted again
    /// (`plan_revise_lost`), or the delivery failed (`planner` `None`).
    fn revise_lost(
        &mut self,
        proposal: ProposalId,
        planner: Option<PlannerId>,
        why: &str,
    ) -> Result<()>;
    /// No planner submitted the proposal again within the timeout
    /// (`planner_unresponsive`, the inbox's), once per revise; `planner`
    /// is `None` when none took the revise yet, and `holders` are then the
    /// runtime's planners that fill the limit it waits on (task 884).
    fn planner_unresponsive(
        &mut self,
        proposal: ProposalId,
        planner: Option<PlannerId>,
        waited_secs: i64,
        holders: &[PlannerHold],
    ) -> Result<()>;
    /// End the submitted proposals none of whose tasks waits for plan
    /// review any more (a person readied them with the bypass or canceled
    /// them): `accepted` when a task is left, `canceled` otherwise
    /// (`proposal_settled`).
    fn settle_proposals(&mut self) -> Result<Vec<(ProposalId, crate::domain::ProposalStatus)>>;
    /// Answered `approve_plan` asks nobody closed, oldest first.
    fn plan_answers(&self) -> Result<Vec<Ask>>;
    /// Apply a person's answer to an `approve_plan` ask and close it.
    /// `None` when the answer is not one the runtime applies (left to the
    /// inbox), or the proposal no longer waits for it (the ask is closed).
    fn decide_plan(&mut self, ask: AskId) -> Result<Option<PlanDecided>>;
    /// Whether the supervisor applies the answer the `approve_plan` ask has.
    fn applies_plan_answer(&self, ask: &Ask) -> Result<bool>;
    /// The proposals held for a person outside an ask.
    fn plan_review_holds(&self) -> Result<Vec<PlanReviewHold>>;
    /// The tasks of closed goals left unfinished that other tasks wait on
    /// (task 421), held for a person outside an ask.
    fn stranded_dependencies(&self) -> Result<Vec<StrandedDependency>>;
    /// Asks a person answered, newest first, at most `limit`: the
    /// precedents plan review may cite.
    fn answered_asks(&self, limit: usize) -> Result<Vec<Ask>>;
}

/// A goal review job the queue recorded (ADR-0047 decision 43): the goal
/// it reviews, its attempt at that goal, the goal's first task (where its
/// `approve_goal` ask is), its directory, and how many `gaps` verdicts the
/// goal got in a row before it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GoalReviewJob {
    pub id: i64,
    pub goal_id: GoalId,
    pub attempt: usize,
    pub anchor: TaskId,
    pub dir: PathBuf,
    pub gaps_in_a_row: usize,
    /// The job's session id, given to it by the runtime (ADR-0048
    /// decision 4); `None` for a provider that names its session itself
    /// (Codex, whose thread its end records).
    pub session_id: Option<String>,
}

/// A finished goal review of a goal, for the next one's prompt.
#[derive(Debug, Clone, Serialize)]
pub struct GoalReviewRecord {
    pub id: i64,
    pub attempt: usize,
    pub outcome: String,
    pub verdict: Option<serde_json::Value>,
    pub error: Option<String>,
}

/// What the runtime makes of a goal review's verdict before it is applied:
/// the decision it acts on (a `gaps` past
/// [`crate::domain::goal_review::MAX_GOAL_GAPS`] in a row is an `ask`,
/// `overridden` saying why) and the `approve_goal` ask an `ask` opens.
#[derive(Debug, Clone)]
pub struct GoalReviewApply {
    pub verdict: GoalReviewVerdict,
    pub decision: GoalReviewDecision,
    pub overridden: Option<String>,
    pub ask: Option<NewAsk>,
    pub duration_secs: u64,
    /// The session the job's output names (Codex's thread and model,
    /// ADR-t1063-1 decision 6); `None` for a Claude job.
    pub session: Option<crate::domain::headless_job::JobSession>,
    /// What the job's prompt took (task 1571), recorded as
    /// `goal_review_finished`'s `prompt_bytes`.
    pub prompt_bytes: Option<crate::application::prompt::PromptBytes>,
}

/// Why a goal review job failed (`goal_review_failed`).
#[derive(Debug, Clone, Default)]
pub struct GoalReviewFailure {
    pub error: String,
    pub duration_secs: u64,
    /// The session the job's output names, as in [`GoalReviewApply`].
    pub session: Option<crate::domain::headless_job::JobSession>,
    /// The job's provider could not be used, and why (ADR-t1063-1
    /// decision 4): its row ends `interrupted` rather than `failed`, so
    /// the goal is reviewed again at once, on the other provider unless
    /// both are held.
    pub unusable: Option<(
        crate::domain::Provider,
        crate::domain::provider_switch::SwitchReason,
    )>,
    /// What the job's prompt took, when it was written (task 1571):
    /// `goal_review_failed`'s `prompt_bytes`.
    pub prompt_bytes: Option<crate::application::prompt::PromptBytes>,
}

/// What applying a goal review's verdict did. `stale`: nothing, because
/// the job's row moved on or the goal's tasks changed meanwhile.
#[derive(Debug, Clone, Default)]
pub struct GoalReviewApplied {
    pub stale: bool,
    pub closed: bool,
    /// The drafts a `gaps` verdict registered.
    pub gap_tasks: Vec<TaskId>,
    pub ask: Option<AskOutcome>,
}

/// A goal whose goal review failed and that waits for a person
/// (`goal_review_failed`) until its tasks change or `goal review ID`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalReviewHold {
    pub goal_id: GoalId,
    pub anchor: Option<TaskId>,
    pub error: Option<String>,
}

/// What a person's `approve_goal` answer did to the goal.
#[derive(Debug, Clone)]
pub struct GoalDecided {
    pub goal_id: GoalId,
    pub answer: String,
    pub closed: Option<GoalVerdict>,
    pub gap_tasks: Vec<TaskId>,
}

/// Goal review (ADR-0047 decision 43): the open goals whose tasks all
/// ended, its one job at a time, the verdicts and answers the runtime
/// applies (each in one transaction).
pub trait GoalReviewStore {
    /// Close unfinished reviews stopped for an exec handoff, owned only by
    /// `token`, including their session spans. Call before starting jobs.
    fn interrupt_goal_reviews_for_handoff(&mut self, token: &LeaseToken) -> Result<()>;
    /// Open goals a goal review may take now, in ID order: at least one
    /// task, each completed or canceled with one completed, no unclosed
    /// `approve_goal` ask, and tasks that changed since the goal's last
    /// review (or a person rearmed it).
    fn goal_review_candidates(&self) -> Result<Vec<GoalId>>;
    /// Record the start of a goal review of `goal` by `token`, in the
    /// directory named by its row's ID under `goal_reviews_dir`
    /// (`goal_review_started`, with the session id it gives the job, the
    /// checkout `cwd` it runs in and its `launch`). Rows of gone
    /// supervisors are finished as `interrupted` first. `None`: another
    /// supervisor's job runs, or the goal is no longer a candidate.
    fn begin_goal_review(
        &mut self,
        goal: GoalId,
        token: &LeaseToken,
        goal_reviews_dir: &Path,
        cwd: &Path,
        launch: &crate::domain::actor_model::ActorLaunch,
    ) -> Result<Option<GoalReviewJob>>;
    /// The finished reviews of `goal`, oldest first.
    fn goal_reviews(&self, goal: GoalId) -> Result<Vec<GoalReviewRecord>>;
    /// Apply the verdict in one transaction (`goal_review_finished`).
    fn finish_goal_review(
        &mut self,
        job: &GoalReviewJob,
        token: &LeaseToken,
        apply: &GoalReviewApply,
    ) -> Result<GoalReviewApplied>;
    /// Record the job's failure (`goal_review_failed`, the inbox's); the
    /// goal is not reviewed again until its tasks change or a person
    /// rearms it, unless its provider could not be used
    /// ([`GoalReviewFailure::unusable`]).
    fn fail_goal_review(
        &mut self,
        job: &GoalReviewJob,
        token: &LeaseToken,
        failure: &GoalReviewFailure,
    ) -> Result<()>;
    /// Answered `approve_goal` asks nobody closed whose answer the runtime
    /// took to apply when it was given (`runtime_delivers`), oldest first;
    /// an answer left to the inbox is never applied later by itself.
    fn goal_answers(&self) -> Result<Vec<Ask>>;
    /// Apply a person's answer to an `approve_goal` ask and close it.
    /// `None` when the answer is not one the runtime applies now (left
    /// open for the inbox), or the goal no longer waits for it (the ask is
    /// closed).
    fn decide_goal(&mut self, ask: AskId) -> Result<Option<GoalDecided>>;
    /// Whether the supervisor applies the answer the `approve_goal` ask has.
    fn applies_goal_answer(&self, ask: &Ask) -> Result<bool>;
    /// Answered `correct_goal` asks (ADR-t1504-2 decision 9) nobody closed
    /// whose answer the runtime took to apply when it was given
    /// (`runtime_delivers`), oldest first.
    fn correction_answers(&self) -> Result<Vec<Ask>>;
    /// Apply a person's answer to a `correct_goal` ask and close it
    /// (`goal_correction_decided`, and `goal_reopened` for `reopen`); the
    /// event's payload, or `None` when the answer is not one the runtime
    /// applies now (left open for the inbox) or the goal is no longer
    /// closed as achieved (the ask is closed).
    fn decide_correction(&mut self, ask: AskId) -> Result<Option<serde_json::Value>>;
    /// Whether the supervisor applies the answer the `correct_goal` ask has.
    fn applies_correction_answer(&self, ask: &Ask) -> Result<bool>;
    /// The goals whose review failed, held for a person.
    fn goal_review_holds(&self) -> Result<Vec<GoalReviewHold>>;
    /// `goal review ID`: let the supervisor review the open goal again
    /// although its tasks did not change (`goal_review_rearmed`).
    fn rearm_goal_review(&mut self, goal: GoalId) -> Result<serde_json::Value>;
}

/// A headless job whose process just started (task 443).
#[derive(Debug, Clone)]
pub struct NewHeadlessJob {
    /// One of [`crate::domain::headless_job`]'s kinds.
    pub kind: &'static str,
    /// What else tells the job (the recovery job's alert).
    pub label: Option<String>,
    pub run_id: Option<RunId>,
    pub proposal_id: Option<ProposalId>,
    pub goal_id: Option<GoalId>,
    pub attempt: usize,
    /// The provider the job runs on (`headless_jobs.provider`).
    pub provider: crate::domain::Provider,
    pub pid: u32,
    /// [`ProcessControl::start_identity`] of `pid` just after the start.
    pub process_start: Option<String>,
    pub supervisor_token: LeaseToken,
}

/// An unfinished `headless_jobs` row of a supervisor that is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessJobRecord {
    pub id: i64,
    pub kind: String,
    pub label: Option<String>,
    pub run_id: Option<RunId>,
    pub proposal_id: Option<ProposalId>,
    pub goal_id: Option<GoalId>,
    pub attempt: usize,
    /// The provider it ran on, as stored (`claude` for a row written
    /// before the column was).
    pub provider: String,
    pub pid: u32,
    pub process_start: Option<String>,
    pub supervisor_token: LeaseToken,
    /// The pid of that supervisor's registration, when it has one left.
    pub supervisor_pid: Option<u32>,
    pub started_at: i64,
}

/// The processes of the supervisor's headless jobs (task 443).
pub trait HeadlessJobStore {
    /// Record the start of a job's process; the row's ID.
    fn record_headless_job(&self, job: &NewHeadlessJob) -> Result<i64>;
    /// Record that the job ended with `outcome`; whether this call ended
    /// it (a row ended already keeps its outcome).
    fn end_headless_job(&self, id: i64, outcome: &str) -> Result<bool>;
    /// The unfinished rows of supervisors other than `token` that are gone
    /// (no longer registered, or whose heartbeat is older than
    /// [`crate::domain::HEARTBEAT_TIMEOUT_SECS`]), oldest first; with
    /// `own`, `token`'s unfinished rows too (a process after its exec,
    /// which knows none of them).
    fn orphaned_headless_jobs(
        &self,
        token: &LeaseToken,
        own: bool,
    ) -> Result<Vec<HeadlessJobRecord>>;
}

/// The queue a use case works on: its tasks and goals, its runs and its
/// asks. A use case that needs only some of it takes those ports instead.
pub trait Queue:
    TaskStore
    + RunTransitions
    + RunRecovery
    + RunCoordination
    + SessionRegistry
    + RunLog
    + QueueRecords
    + AskStore
    + DraftPlannerStore
    + PlanRequestStore
    + PlanReviewStore
    + GoalReviewStore
    + HeadlessJobStore
{
}

impl<
    T: TaskStore
        + RunTransitions
        + RunRecovery
        + RunCoordination
        + SessionRegistry
        + RunLog
        + QueueRecords
        + AskStore
        + DraftPlannerStore
        + PlanRequestStore
        + PlanReviewStore
        + GoalReviewStore
        + HeadlessJobStore
        + ?Sized,
> Queue for T
{
}

/// One file's identity and change times as [`Repository::landing_branch_stamp`]
/// reads them; `None` in [`LandingBranchStamp`] is a file that is not there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp {
    pub modified: Option<SystemTime>,
    pub len: u64,
    /// The inode and the status change time (unix), which a rename of a
    /// lock file or an edit in place within the modification time's
    /// granularity still changes.
    pub inode: u64,
    pub changed: (i64, i64),
}

/// The files the landing branch is resolved from, each with its
/// [`FileStamp`] (task 1078).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandingBranchStamp(pub Vec<(PathBuf, Option<FileStamp>)>);

/// The Git operations `integrate` and the supervisor use on the repository
/// the queue is bound to and on its run worktrees. Commits are named by
/// their full SHA; a failed Git command is an error with Git's message.
pub trait Repository {
    /// The branch runs land on (ADR-t615-1), resolved again on every call;
    /// a repository that does not resolve one lands on `main`.
    fn landing_branch(&self) -> Result<crate::domain::landing_branch::LandingBranch> {
        Ok(crate::domain::landing_branch::LandingBranch::main())
    }
    /// A stamp of the files [`Self::landing_branch`] reads, taken without
    /// starting Git (task 1078): equal stamps mean the resolution would
    /// read the same inputs. `None` when the repository cannot tell, and
    /// the caller resolves every time.
    fn landing_branch_stamp(&self) -> Option<LandingBranchStamp> {
        None
    }
    /// `[repository]` of the main checkout's `dagq.toml`, read again on
    /// every call; a repository without one has the default (`origin`,
    /// pushed). `integrate --no-push` reads it only to record the remote
    /// it did not push to.
    fn repository_config(&self) -> Result<crate::domain::landing_branch::RepositoryConfig> {
        Ok(Default::default())
    }
    /// Whether the repository is dagq's source (ADR-t614-1), judged again
    /// on every call; what only dagq's own development needs runs only
    /// there.
    fn is_dagq_source(&self) -> bool;
    /// The landing branch's current commit, read again on every call.
    fn main_head(&self) -> Result<CommitSha>;
    /// Read a UTF-8 blob in a commit's tree; `None` if the path is absent.
    fn file_in(&self, _commit: &str, _path: &str) -> Result<Option<String>> {
        anyhow::bail!("this repository cannot read committed files")
    }

    /// Main's first-parent history since `since` (unix seconds) and the
    /// paths it has now, for `conflict_hotspots`; a repository that cannot
    /// tell has none.
    fn main_history(&self, since: i64) -> Result<crate::domain::stats::conflicts::MainHistory> {
        let _ = since;
        anyhow::bail!("this repository keeps no history")
    }
    /// Symbolic HEAD of a worktree (`refs/heads/...`), `None` when detached.
    fn current_branch(&self, worktree: &Path) -> Result<Option<String>>;
    fn head(&self, worktree: &Path) -> Result<CommitSha>;
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool>;
    fn merge_base(&self, a: &str, b: &str) -> Result<Option<CommitSha>>;
    /// `git status --porcelain` of the worktree; blank when clean.
    fn status(&self, worktree: &Path) -> Result<String>;
    fn rebase_in_progress(&self, worktree: &Path) -> Result<bool>;
    fn rebase_abort(&self, worktree: &Path) -> Result<()>;
    /// Rebase the worktree's branch onto `onto`; `Ok(Err(output))` is a
    /// rebase that stopped (a conflict), left in progress.
    fn rebase(&self, worktree: &Path, onto: &str) -> Result<std::result::Result<(), String>>;
    fn conflicted_files(&self, worktree: &Path) -> Result<Vec<String>>;
    /// Paths that differ between two commits.
    fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<String>>;
    /// Paths `to` adds over `from`.
    fn added_paths(&self, from: &str, to: &str) -> Result<Vec<String>>;
    /// Paths of the files directly in `directory` of `commit`'s tree; none
    /// when it has no such directory.
    fn paths_in(&self, commit: &str, directory: &str) -> Result<Vec<String>>;
    /// Which of `paths` contain the text `needle` in `commit`'s tree.
    fn paths_containing(&self, commit: &str, needle: &str, paths: &[String])
    -> Result<Vec<String>>;
    /// Move `from` to `to` in the clean `worktree` and commit the rename on
    /// its branch, `paragraphs` its message; the new head. `Ok(Err(output))`
    /// is a commit Git refused, with the rename undone and the worktree clean
    /// at the head it had; `Err` when the move or the undoing failed.
    fn rename_and_commit(
        &self,
        worktree: &Path,
        from: &str,
        to: &str,
        paragraphs: &[String],
    ) -> Result<std::result::Result<CommitSha, String>>;
    fn tree_of(&self, commit: &str) -> Result<String>;
    /// One commit with `tree` on top of `parent`, `paragraphs` its message.
    fn commit_tree(&self, tree: &str, parent: &str, paragraphs: &[String]) -> Result<CommitSha>;
    fn update_ref(&self, name: &str, value: &str) -> Result<()>;
    /// Fast-forward `branch`, the landing branch resolved when the landing
    /// began, from `from` to `to`.
    fn advance_main(
        &self,
        branch: &crate::domain::landing_branch::LandingBranch,
        from: &str,
        to: &str,
    ) -> Result<()>;
    /// Point the repository's record of a moved worktree at it again.
    fn repair_worktree(&self, worktree: &Path) -> Result<()>;
    /// Forget the worktrees whose directory is gone (`git worktree
    /// prune`); a repository without worktrees has none to forget.
    fn prune_worktrees(&self) -> Result<()> {
        Ok(())
    }
    /// Remove the worktree and its branch; a branch already gone is not
    /// an error.
    fn remove_worktree_and_branch(&self, worktree: &Path, branch: &str) -> Result<()>;
    /// The local branches, by their short name (`dagq/<run-id>`).
    fn branches(&self) -> Result<Vec<String>>;
    /// Delete the local branch (`git branch -D`); one already gone is not
    /// an error.
    fn delete_branch(&self, branch: &str) -> Result<()>;
    /// Whether Git tracks any file at or under `path` (relative to the
    /// worktree) in `worktree`.
    fn tracks(&self, worktree: &Path, path: &str) -> Result<bool>;
    /// The worktree that has the landing branch checked out, if any.
    fn main_checkout(&self) -> Result<Option<std::path::PathBuf>>;
    /// Add the run's worktree on its new branch from its base commit; Git's
    /// output.
    fn create_worktree(&self, run: &TaskRun) -> Result<String>;
    /// The paths `git merge-tree` finds conflicting between two commits,
    /// without touching a worktree; empty when they merge cleanly.
    fn merge_conflicts(&self, main: &str, head: &str) -> Result<Vec<String>>;
    /// The tree of `head` merged with `main` without touching a worktree:
    /// `Ok(tree)` when they merge cleanly, `Err(paths)` when they conflict.
    fn merged_tree(
        &self,
        main: &str,
        head: &str,
    ) -> Result<std::result::Result<String, Vec<String>>> {
        let _ = (main, head);
        anyhow::bail!("this repository cannot merge trees")
    }
    /// Check `commit` out detached in the scratch worktree at `path`,
    /// adding it when missing and clearing what its last use left.
    fn checkout_scratch(&self, path: &Path, commit: &str) -> Result<()> {
        let _ = (path, commit);
        anyhow::bail!("this repository keeps no scratch worktree")
    }
    /// The tasks landed between two commits, oldest first, from their
    /// `Dagq-Task` trailers.
    fn landed_task_ids(&self, base: &str, head: &str) -> Result<Vec<TaskId>>;
    /// The commit of `<base>..<head>`'s first-parent history that landed
    /// run `run` (its `Dagq-Run` trailer), with its first parent; `None`
    /// when none did. A repository that cannot tell finds none.
    fn landed_run_commit(
        &self,
        base: &str,
        head: &str,
        run: &str,
    ) -> Result<Option<(CommitSha, CommitSha)>> {
        let _ = (base, head, run);
        Ok(None)
    }
    /// `git log --oneline <base>..<head>`.
    fn log_oneline(&self, base: &str, head: &str) -> Result<String>;
    /// `git diff --stat <base>...<head>`.
    fn diff_stat(&self, base: &str, head: &str) -> Result<String>;
    /// The size of the diff `<base>...<head>`.
    fn diff_numbers(&self, base: &str, head: &str) -> Result<DiffNumbers>;
    /// Write the full diff `<base>...<head>` to a new file at `path`, as
    /// Git's raw bytes and never through memory.
    fn diff_to_file(&self, base: &str, head: &str, path: &Path) -> Result<()>;
}

/// The size of a diff, as `git diff --numstat` counts it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DiffNumbers {
    pub files_changed: u64,
    pub insertions: u64,
    pub deletions: u64,
}

/// Runs a task's verification commands for `integrate` (ADR-0023
/// decision 1) with the `[run.env]` of the repository (ADR-0023 decision 3).
pub trait Verifier {
    /// The environment of the run whose directory is `run_dir`.
    fn run_env(&self, run_dir: &Path) -> Result<Vec<(String, String)>>;
    /// Whether the programs that environment names resolve on this
    /// process's `PATH` (ADR-0049 decision 9); `None` is before any run.
    fn run_env_programs(
        &self,
        run_dir: Option<&Path>,
    ) -> Result<crate::domain::run_env::RunEnvCheck>;
    /// `[run.env]` of the main checkout's `dagq.toml` as written (values
    /// unexpanded), `None` without the file: what the supervisor hashes for
    /// the `run_env_changed` mark (ADR-0051 decision 11).
    fn run_env_table(&self) -> Result<Option<Vec<(String, String)>>>;
    /// The queue's secret salt of the `[run.env]` hashes, made on first use
    /// and kept outside the events (ADR-0051 decision 11).
    fn run_env_salt(&self) -> Result<String>;
    /// The command the landing recheck runs on main's tree with a waiting
    /// run merged in (`[recheck] command` of `dagq.toml`, ADR-0068 decision
    /// 2); `None` checks the merge only.
    fn recheck_command(&self) -> Result<Option<String>> {
        Ok(None)
    }
    /// The limited trial of the worker's model (`[worker.trial]` of
    /// `dagq.toml`, ADR-0079 decision 4); off by default.
    fn worker_trial(&self) -> Result<crate::domain::worker_model::WorkerTrial> {
        Ok(crate::domain::worker_model::WorkerTrial::default())
    }
    /// The paths whose change requires `e2e` of a run (`[e2e] paths` of
    /// `dagq.toml`, ADR-t963-1 decision 2), as globs; none by default,
    /// which leaves `e2e` to the task's `required_evidence`.
    fn e2e_paths(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    /// The review's subagents (`[review.subagents.<agent>]`, ADR-t1453-1
    /// decision 1) of `text`, a whole `dagq.toml` as committed; `Err`
    /// when it cannot be parsed. None by default.
    fn review_subagents_in(
        &self,
        text: &str,
    ) -> Result<Vec<crate::domain::review_subagents::ReviewSubagent>> {
        let _ = text;
        Ok(Vec::new())
    }
    /// Where a headless session's wrapper runs (`[headless] wrapper` of
    /// `dagq.toml`, ADR-t1404-1 decision 7); a workspace by default.
    fn headless_wrapper(&self) -> Result<crate::domain::background_wrapper::HeadlessWrapper> {
        Ok(crate::domain::background_wrapper::HeadlessWrapper::default())
    }
    /// The model and effort of the roles other than the worker
    /// (`[roles.<role>]` of `dagq.toml`, ADR-0079 decision 7); none by
    /// default, which starts every role as before.
    fn role_models(&self) -> Result<crate::domain::actor_model::RoleModels> {
        Ok(crate::domain::actor_model::RoleModels::default())
    }
    /// The language AI writes in for people (`[language]` of `dagq.toml`
    /// over the user's `config.toml`, ADR-t616-2), resolved now; `None`
    /// when unset or unreadable, which adds no instruction to a prompt.
    fn language(&self) -> Option<crate::domain::language::Language> {
        None
    }
    /// Run `command` in a shell in `cwd` with `env`, its output in `log`.
    fn run_to_log(
        &self,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
        log: &Path,
    ) -> Result<Exit>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::headless_job::JobFailure;

    #[test]
    fn a_job_that_did_not_start_is_classed_by_why() {
        let missing =
            anyhow::Error::new(io::Error::from(io::ErrorKind::NotFound)).context("launch agent");
        assert_eq!(job_start_failure(&missing), JobFailure::ExecutableMissing);
        let refused = anyhow::Error::new(io::Error::from(io::ErrorKind::PermissionDenied))
            .context("launch agent");
        assert_eq!(job_start_failure(&refused), JobFailure::LaunchFailed);
        assert_eq!(
            job_start_failure(&anyhow::anyhow!("no provider")),
            JobFailure::LaunchFailed
        );
        // Arguments past the system's limit are the job's input: a failure
        // of the job alone, which names no reason to hold its provider.
        let too_long =
            anyhow::Error::new(io::Error::from_raw_os_error(libc::E2BIG)).context("launch agent");
        assert_eq!(job_start_failure(&too_long), JobFailure::Other);
        assert_eq!(JobFailure::Other.switch_reason(), None);
        // A standard input that could not be prepared is the starter's
        // environment, even when its io error says `NotFound` (a `TMPDIR`
        // that is gone) or anything else (a full disk).
        for kind in [io::ErrorKind::NotFound, io::ErrorKind::StorageFull] {
            let unprepared = anyhow::Error::new(StdinUnprepared {
                what: "create the standard input /gone/dagq-stdin-1".into(),
                source: io::Error::from(kind),
            })
            .context("launch agent");
            assert_eq!(
                job_start_failure(&unprepared),
                JobFailure::Other,
                "{kind:?}"
            );
            assert!(format!("{unprepared:#}").contains("/gone/dagq-stdin-1"));
        }
    }
}
