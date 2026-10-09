//! The ports of 実行と着地 (docs/design/architecture.md, section
//! "portのmodule"): the runs and their transitions, recovery, leases and
//! sessions, the run log, the agent and its turns, the run's files, Git
//! and the verification.

use super::shared::{CommandSpec, EventStore, Exit};
use crate::domain::{
    AskId, ClaimOutcome, CommitSha, EventId, EventKind, LeaseToken, PlannerId, PlannerOrigin,
    PlannerSession, ProposalId, Reason, RunEvent, RunId, RunLease, RunPlan, RunProcess, RunStatus,
    SessionRole, Task, TaskId, TaskRun, TaskStatus,
};
use anyhow::{Context, Result};
use serde::Serialize;
use std::{
    fmt, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

/// The directory under a run's directory the runtime gives a Codex
/// worker's turns as their `TMPDIR` (task 1290), and removes with the
/// run's leftovers once its task is over.
pub const RUN_TMP_DIR: &str = "tmp";

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
    /// Take an exclusive lock on `path` (created if missing) as
    /// [`Self::try_lock`] does, but wait while another holder has it. A
    /// store with nothing to share between processes grants it at once.
    fn lock(&self, path: &Path) -> io::Result<Box<dyn std::any::Any + Send>> {
        let _ = path;
        Ok(Box::new(()))
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
    ///
    /// [`Clock`]: super::shared::Clock
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

/// The launch of an agent job (ADR-t1895-1, ADR-t1728-1 decision 5): one
/// agent's own headless job, whose prompt carries its definition and asks
/// for one verdict of the shape of a review agent's result. Built by
/// [`crate::application::agent_job::build`] for the eval of an agent; a
/// run's review starts the same job for each of its agents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentJobLaunch {
    /// Where the job runs and reads the change's files: the tree of the
    /// change (an eval case's worktree).
    pub cwd: std::path::PathBuf,
    /// The job's own directory, outside `cwd`: its settings and its debug
    /// file.
    pub dir: std::path::PathBuf,
    /// The material file the job reads (the commits and the full diff),
    /// outside `cwd`.
    pub material: std::path::PathBuf,
    /// The prompt, given on standard input, never as an argument.
    pub prompt: String,
    /// The tools the definition declares, or the role's default
    /// (ADR-t1728-2), which the provider turns into its own: Claude Code's
    /// `--allowedTools` and `--disallowedTools`, Codex's features on top of
    /// its read-only sandbox.
    pub tools: crate::domain::review_subagents::AgentTools,
}

/// The rounds of the eval as supervisors own them (ADR-t1728-1 decision
/// 9): one round runs per queue at once, and each running round has one
/// owner, the supervisor whose token its latest `agent_eval_started` or
/// `agent_eval_taken_up` names. Each change is one write transaction, so
/// two supervisors on one queue (ADR-0054 decision 3) never both start or
/// take up a round. Internal to execution and landing: only the
/// supervisor's rounds use it.
pub trait EvalRounds {
    /// Record `kind`, `agent_eval_started` (its `payload` names the owner
    /// as `supervisor`) or `agent_eval_refused`, for the waiting round
    /// `eval_id`, unless it was started or refused already or, for a
    /// start, another round of the queue started and has not finished:
    /// `false` then, and nothing is recorded.
    fn settle_eval_round(
        &self,
        eval_id: i64,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<bool>;
    /// Make `token` the owner of the running round `eval_id`
    /// (`agent_eval_taken_up`), only while it has not finished and its
    /// owner is gone or `token` itself
    /// ([`crate::domain::agent_eval::round::owner_gone`]; `alive` says
    /// whether a registered owner's process lives): `false` otherwise.
    fn take_up_eval_round(
        &self,
        eval_id: i64,
        token: &str,
        alive: &dyn Fn(u32) -> bool,
    ) -> Result<bool>;
}

/// Provider-specific CLI construction is kept outside supervisor orchestration.
pub trait AgentProvider {
    fn preflight(&self) -> Result<()>;
    /// The agent's executable found again by the provider's name on PATH
    /// when the path it was given is not there (a version the provider's
    /// update removed, ADR-t2079-1); `None` when that path is there or
    /// nothing is found by the name. The executor starts an agent whose
    /// start found no executable once more with it.
    fn relocated_executable(&self) -> Option<std::path::PathBuf> {
        None
    }
    /// Whether the agent's sessions started in `cwd` load the plugin
    /// `name` without a `--plugin-dir` (ADR-t617-2 decision 4); an error
    /// when it cannot tell. A provider without plugins cannot tell.
    fn installed_plugin(&self, cwd: &std::path::Path, name: &str) -> Result<PluginState> {
        let _ = (cwd, name);
        anyhow::bail!("this provider has no plugins")
    }
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
    /// (ADR-t1063-1 decision 6) and of the job's tokens, one Execution
    /// (ADR-t1486-1): Codex names its thread in its output and the model
    /// in its rollout; Claude Code, whose session the runtime names ahead
    /// and whose transcript gives the model (ADR-0048 decision 4), gives
    /// the tokens only. `None` for a provider whose output says neither.
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
    /// The agent of a planner session in a terminal (ADR-0041 decisions 1,
    /// 6): only a person's planner, opened before `dagq plan` was abolished
    /// (ADR-t1394-1), runs one; a planner of the runtime's runs headless
    /// turns ([`AgentProvider::turn_command`], ADR-t1433-2 decision 3). An
    /// interactive agent in `planner.cwd` with `planner.prompt` as its
    /// first message, whose `Stop` hook writes
    /// [`PlannerCommand::idle_marker`] the way a worker's does, and that
    /// loads `planner.plugin_dir`. A provider without one refuses.
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
    /// An agent job ([`AgentJobLaunch`]): a non-interactive agent in
    /// `job.cwd` with `job.prompt` on its standard input, allowed only the
    /// reads of [`crate::domain::headless_job::JobAccess::ReadFiles`]
    /// narrowed to the declared tools, loading nothing of the tree it runs
    /// in (ADR-t1470-1, ADR-t1570-1), whose reply
    /// ([`AgentProvider::job_reply`]) is the verdict JSON. No subagent is
    /// given it. A provider without one refuses.
    fn agent_job_command(&self, job: &AgentJobLaunch) -> Result<CommandSpec> {
        let _ = job;
        anyhow::bail!("this provider has no agent job")
    }
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
    /// `prompt` as its first message that loads `plugin_dir`, which `dagq
    /// inbox` starts in the foreground of a person's terminal (ADR-t2159-1
    /// decision 2). Its settings, if the provider has a way
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
    /// `queue_dir` and starts the inbox with, the inbox's denials
    /// (ADR-t2159-1 decision 5); `None` for a provider whose inbox has no
    /// such guardrail. `dagq inbox` records whether there is one, and
    /// `status` and `doctor` show it.
    fn inbox_settings(&self, queue_dir: &std::path::Path) -> Option<std::path::PathBuf> {
        let _ = queue_dir;
        None
    }
    /// Start the agent of `command` (a turn from
    /// [`AgentProvider::turn_command`], or another role's through
    /// [`AgentProvider::apply_launch`]) with `model` at `effort` (ADR-0079
    /// decision 3): given explicitly, so neither the provider's
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

/// Reads the output of one headless turn into what the runtime acts on,
/// whatever the provider (ADR-t813-1): the one place that knows the shape
/// of a provider's JSONL.
pub trait TurnReader: Send {
    /// One line of the turn's stdout, without its line break.
    fn line(&mut self, line: &str) -> Vec<crate::domain::turn::TurnSignal>;
    /// The lines read next were read at `at` (unix milliseconds): a reader
    /// whose output has no times takes them as its items' (Codex's, for
    /// [`crate::domain::turn::TurnResult::commands`]).
    /// The wrapper reads about once per [`AgentProvider::wait_interval`],
    /// so such a time is late by up to that interval, and the lines of one
    /// read share it: a command shorter than the interval may read as 0
    /// or 1 second.
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
/// it) and the plugin directory it loads (a planner's).
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

/// What the agent of a person's planner session is started with: the
/// planner's directory (its prompt, settings, log and idle marker), the
/// directory it works in (the repository's checkout), its first message
/// and the plugin directory it loads.
#[derive(Debug, Clone, Copy)]
pub struct PlannerCommand<'a> {
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

/// What a provider's agent leaves for the runtime to read: its idle
/// marker's content and a headless job's output.
pub trait AgentSignals {
    /// Why a headless job that failed did, from its output (its stdout and
    /// stderr), in the classes shared by every provider (ADR-t1063-1
    /// decision 4): a login that ran out and the usage limit are the walls
    /// of task 438. A provider that cannot tell says `other`.
    fn job_failure(&self, _output: &str) -> crate::domain::headless_job::JobFailure {
        crate::domain::headless_job::JobFailure::Other
    }
    /// What the idle marker's content says. A content the adapter cannot
    /// read still marks a stop.
    fn idle_hook(&self, content: &[u8]) -> IdleHook;
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
    /// Git's message. Only the [`crate::application::integrate::Integrator`] holds the
    /// [`crate::application::integrate::PushGrant`] it takes (ADR-t728-2).
    fn push_main(
        &self,
        grant: &crate::application::integrate::PushGrant,
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

/// A session an ended run opened (the worker's or a resume's), whose
/// background wrapper the supervisor's sweep stops while it still runs; a
/// workspace from before ADR-t1433-3 is left to a person.
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
    /// A run the supervisor itself leases, waiting outside its slots
    /// (ADR-0071) for a person's answer to this ask, its oldest one the
    /// wait holds open, with no turn running: no listing of the queue
    /// returns it, the supervisor offers it to a cleanup for disk space
    /// alone, which removes its build outputs while the wait holds still.
    Waiting(AskId),
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
    /// The session's wrapper exited: the run moves on by its exit code and
    /// whether its `receipt` was written (ADR-t1594-1).
    fn finish_supervision(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        receipt: bool,
    ) -> Result<TaskRun>;
    /// End a running run's lost session that could not be opened again
    /// (task 1372) as if its wrapper exited with `exit_code`.
    fn finish_lost_session(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        exit_code: i32,
        receipt: bool,
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

/// The state runs coordinate through (ADR-0032's second kind): run leases
/// and heartbeats, the wrapper and agent processes of runs and the
/// backend's slots. The supervisors' registrations are host運用's
/// [`SupervisorRegistry`](super::host::SupervisorRegistry), except that
/// [`heartbeat`](Self::heartbeat) refreshes the registration with the
/// leases in one write (T11 of docs/design/architecture.md). Its reads
/// (`run_leases`, `run_lease`, `processes`) are published to every context;
/// the rest is 実行と着地's own (a handoff gives its slots' leases back in
/// 実行と着地's `supervise::handoff_slots`).
pub trait RunCoordination {
    /// Refresh every lease `token` holds; how many there were.
    fn heartbeat_leases(&self, token: &LeaseToken) -> Result<usize>;
    fn release_lease(&mut self, id: &RunId, token: &LeaseToken) -> Result<()>;
    fn holds_lease(&self, id: &RunId, token: &LeaseToken) -> Result<bool>;
    fn run_leases(&self) -> Result<Vec<RunLease>>;
    fn run_lease(&self, id: &RunId) -> Result<Option<RunLease>>;
    /// One heartbeat of the process `token`: its registration and every
    /// lease it holds; how many leases there were.
    fn heartbeat(&mut self, token: &LeaseToken) -> Result<HeartbeatWrite>;
    /// The processes registered for the run (its wrapper and agent).
    fn processes(&self, id: &RunId) -> Result<Vec<RunProcess>>;
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
    /// Forget the workspaces recorded for a role `up` no longer opens (all
    /// but the retired in-cmux supervisor's), without a call to cmux.
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
    /// Mark planner `id` of the runtime's as asked to exit because only a
    /// person's answer to its `planner_question`s `asks` is left, with
    /// `planner_answer_wait` (`payload`), once and only while none of them
    /// is answered or closed (ADR-t1704-1 decisions 1 and 2): `false`, with
    /// nothing written, otherwise.
    fn planner_answer_wait(
        &self,
        id: PlannerId,
        asks: &[AskId],
        payload: &serde_json::Value,
    ) -> Result<bool>;
    /// Mark planner `id` of the runtime's as asked to exit because the
    /// answer of `ask` could not be sent to it, as [`Self::planner_answer_wait`]
    /// marks one, so the answer goes to a new planner once its row is
    /// closed; once and only while the ask is answered and not closed:
    /// `false`, with nothing written, otherwise.
    fn planner_answer_undelivered(
        &self,
        id: PlannerId,
        ask: AskId,
        payload: &serde_json::Value,
    ) -> Result<bool>;
    /// What the planner that ended for the answer of `ask` alone left for
    /// the next (ADR-t1704-1 decision 3), if one did.
    fn planner_handover(&self, ask: AskId) -> Result<Option<crate::domain::PlannerHandover>>;
    /// The planner's session wrapper registers itself, once.
    fn register_planner_wrapper(&self, id: PlannerId, pid: u32) -> Result<()>;
    fn register_planner_agent(&self, id: PlannerId, wrapper_pid: u32, agent: u32) -> Result<()>;
    fn heartbeat_planner(&self, id: PlannerId, wrapper_pid: u32) -> Result<()>;
    fn planner_exited(&self, id: PlannerId, wrapper_pid: u32, exit_code: i32) -> Result<()>;
    /// Record `planner_unresponsive` about a planner of the runtime's whose
    /// turn its wrapper stopped at the turn's limit (`[stall]`, the
    /// supervisor's `tell_of_stopped_planner_turns`; before task 1441 also
    /// one nothing was seen of within the planner timeout, task 805), once
    /// per planner: `payload` names it by `planner_id` with `subject:
    /// "planner"`. `false` when it was recorded before.
    fn planner_silent(&self, id: PlannerId, payload: serde_json::Value) -> Result<bool>;
    /// The planners not closed that such a `planner_unresponsive` names,
    /// each with that event: the inbox's attention.
    fn silent_planners(&self) -> Result<Vec<(PlannerSession, RunEvent)>>;
    /// Record the finished transcript turns of the Claude session spans
    /// still open, and finalize closed hook spans awaiting intake (ADR-0048
    /// decision 8); returns how many spans got turns or final measurements.
    fn record_session_turns(&self) -> Result<usize>;
    /// Cut the tokens of the inbox and person's planner spans that are due
    /// a cut: hourly while open, once at their close (ADR-t1486-1 decision
    /// 3); returns how many cuts were recorded.
    fn record_session_tokens(&self) -> Result<usize>;
    /// Record what the plugin's hook reported of an inbox or planner
    /// session (ADR-0048 decision 6): its span opened, gone on with or
    /// closed. Only the spans are written.
    fn record_session_hook(
        &self,
        hook: &crate::domain::sessions::SessionHook,
    ) -> Result<serde_json::Value>;
    /// The open spans the hook recorded (inbox and planner sessions'),
    /// oldest first.
    fn open_hook_session_spans(&self) -> Result<Vec<crate::domain::sessions::OpenSpan>>;
    /// Close, as `inferred`, the hook's spans among `ended` still open:
    /// their session is over without a `SessionEnd` (ADR-t2022-1). Each
    /// takes in its transcript and ends at its last record. Returns how
    /// many it closed.
    fn close_inferred_sessions(&self, ended: &[EventId]) -> Result<usize>;
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

/// The runs as read (ADR-0032's third kind): the run rows of the
/// [`StateStore`](super::shared::StateStore), which a run's transitions
/// change with their events in one transaction.
pub trait RunReads {
    fn active_runs(&self) -> Result<Vec<TaskRun>>;
    /// Every run of the queue, oldest first.
    fn all_runs(&self) -> Result<Vec<TaskRun>>;
    /// Returns [`RunNotFound`] only when the lookup succeeded with no row.
    fn run(&self, id: &RunId) -> Result<TaskRun>;
    fn runs_with_status(&self, status: RunStatus) -> Result<Vec<TaskRun>>;
    /// The run awaiting integration longest, by validation time.
    fn next_awaiting_integration(&self) -> Result<Option<TaskRun>>;
    /// The sessions of the ended runs the triage does not take, for the
    /// supervisor's sweep.
    fn ended_run_workspaces(&self) -> Result<Vec<EndedRunWorkspace>>;
    /// The worktrees of the runs nobody leases that ended or whose task is
    /// `completed` / `canceled`, for the supervisor's clean-up of the disk.
    fn ended_run_worktrees(&self) -> Result<Vec<EndedRunWorktree>>;
    /// Run `id` as [`Self::ended_run_worktrees`] would list it, read alone
    /// (task 1586); `None` when it would not be listed.
    fn ended_run_worktree(&self, id: &RunId) -> Result<Option<EndedRunWorktree>>;
    /// The latest run of every `in_progress` task, oldest first.
    fn latest_runs_in_progress(&self) -> Result<Vec<TaskRun>>;
    /// The `integrated` runs whose push of `main` failed after the latest
    /// successful push, oldest first.
    fn runs_with_pending_push(&self) -> Result<Vec<TaskRun>>;
    /// The run whose workspace is `workspace_id`, the latest one first.
    fn run_in_workspace(&self, workspace_id: &str) -> Result<Option<RunId>>;
}

/// The runs and the events together: a name for [`RunReads`] and the
/// [`EventStore`], kept so the use cases that took the run log need not be
/// rewritten at once (docs/design/architecture.md, section "portのmodule").
/// It adds no method; a use case that reads only events takes
/// [`EventStore`], and one that reads only runs takes [`RunReads`].
pub trait RunLog: RunReads + EventStore {}

impl<T: RunReads + EventStore + ?Sized> RunLog for T {}

/// The ports of the queue the recording of the session wrappers
/// ([`crate::application::recording::RecordingSessions`]) reaches: the run
/// a session belongs to and its events, the slots held when a call failed,
/// and the planners whose turns a wrapper that died left running.
/// [`super::shared::Queue`] has it as a supertrait, so a connection of the
/// whole queue upcasts to it.
pub trait RecordingQueue: RunLog + RunCoordination + SessionRegistry {}

impl<T: RunLog + RunCoordination + SessionRegistry + ?Sized> RecordingQueue for T {}

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

    /// The blob ID of each file at or under `paths` (files, or directories
    /// and everything under them) in `commit`'s tree, by its path; a path
    /// the tree lacks gives none.
    fn blobs_in(&self, _commit: &str, _paths: &[&str]) -> Result<Vec<(String, String)>> {
        anyhow::bail!("this repository cannot list committed files")
    }

    /// Main's first-parent history since `since` (unix seconds) and the
    /// paths it has now, for `conflict_hotspots`; a repository that cannot
    /// tell has none.
    fn main_history(&self, since: i64) -> Result<crate::domain::stats::conflicts::MainHistory> {
        let _ = since;
        anyhow::bail!("this repository keeps no history")
    }
    /// The commits of main's first-parent line at `head` after `after`
    /// (none `after` reaches) from `since` (unix seconds), oldest first,
    /// each with its ID, its committer's time and the paths it changed:
    /// what the supervisor records of main's history.
    fn main_commits(
        &self,
        head: &str,
        after: Option<&str>,
        since: i64,
    ) -> Result<Vec<crate::domain::stats::conflicts::MainCommit>> {
        let _ = (head, after, since);
        anyhow::bail!("this repository keeps no history")
    }
    /// Every file path of `commit`'s tree.
    fn tree_paths(&self, commit: &str) -> Result<Vec<String>> {
        let _ = commit;
        anyhow::bail!("this repository cannot list committed files")
    }
    /// Whether the repository has the commit `commit`.
    fn has_commit(&self, commit: &str) -> Result<bool> {
        let _ = commit;
        anyhow::bail!("this repository keeps no history")
    }
    /// Whether `commit` is on the first-parent line of `head`.
    fn on_first_parent_line(&self, commit: &str, head: &str) -> Result<bool> {
        let _ = (commit, head);
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
    /// Paths that differ between two commits, or trees (the landing
    /// recheck's merged tree against main, ADR-t2032-1).
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
    /// The tree of an eval's case (ADR-t1728-1 decision 4) at `path`, a
    /// worktree without a branch: `base` checked out detached, the patch
    /// file `patch` applied and committed on top; the commit's SHA, the
    /// head of the change `<base>...<head>`. A tree left at `path` is
    /// removed first. No hook of the repository runs. `hidden` (relative
    /// paths, one `*` segment matching a directory's entries) is then
    /// removed from the working tree, not from the commit: what the job
    /// must not read there (the eval's own cases and patches).
    fn add_case_tree(
        &self,
        path: &Path,
        base: &str,
        patch: &Path,
        hidden: &[&str],
    ) -> Result<CommitSha> {
        let _ = (path, base, patch, hidden);
        anyhow::bail!("this repository cannot make an eval case's tree")
    }
    /// Remove the case's tree at `path` ([`Self::add_case_tree`]); one
    /// already gone is not an error.
    fn remove_case_tree(&self, path: &Path) -> Result<()> {
        let _ = path;
        anyhow::bail!("this repository cannot remove an eval case's tree")
    }
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
    /// `[recheck]` of `dagq.toml`: the command the landing recheck runs on
    /// main's tree with a waiting run merged in (ADR-0068 decision 2; none
    /// checks the merge only) and the paths a run's diff must touch for it
    /// to run (ADR-t2032-1; none runs it on every run).
    fn recheck_config(&self) -> Result<crate::domain::recheck::RecheckConfig> {
        Ok(crate::domain::recheck::RecheckConfig::default())
    }
    /// The command the landing runs in place of some of a task's
    /// verification commands (`[landing_verification]` of `dagq.toml`,
    /// ADR-t1925-1 decision 4); none by default, which runs them as
    /// registered.
    fn landing_verification(
        &self,
    ) -> Result<Option<crate::domain::landing_verification::LandingVerification>> {
        Ok(None)
    }
    /// `[ci_watch]` of `dagq.toml` (ADR-t1920-1), whose branch the known
    /// failures handed to the landing's command are read for; none by
    /// default.
    fn ci_watch_config(&self) -> Result<Option<crate::domain::ci_watch::CiWatchConfig>> {
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
    /// The program reviews (`[review.programs.<name>]`, ADR-t1895-2) of
    /// `text`, a whole `dagq.toml` as committed; `Err` when it cannot be
    /// parsed. None by default.
    fn review_programs_in(
        &self,
        text: &str,
    ) -> Result<Vec<crate::domain::review_programs::ReviewProgram>> {
        let _ = text;
        Ok(Vec::new())
    }
    /// `[headless] wrapper` as `dagq.toml` writes it, `None` without the
    /// key: only to tell that a worker and a planner of the runtime's
    /// ignore `"workspace"` (ADR-t1433-3 decision 2, ADR-t1433-2 decision
    /// 3): their wrappers always start in the background.
    fn headless_wrapper_setting(
        &self,
    ) -> Result<Option<crate::domain::background_wrapper::HeadlessWrapper>> {
        Ok(None)
    }
    /// `[eval]` and `[eval.providers.<provider>]` of `dagq.toml`
    /// (ADR-t1728-1), each key's default where unset; the default without
    /// the file.
    fn eval_config(&self) -> Result<crate::domain::agent_eval::round::EvalConfig> {
        Ok(Default::default())
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

/// Where a run review's program jobs run (ADR-t1895-2 decision 5), one
/// implementation per backend of the review's actor: the command that runs
/// a program against the run's worktree. Today the host
/// (`infrastructure::review_programs::HostPrograms`); a backend decided but
/// not implemented is an error to start, never the host instead.
pub trait ReviewProgramBackend: Send + Sync {
    /// The command that runs `program`'s script with the run's `worktree`
    /// as its working directory and an environment narrowed to what a
    /// read-only check needs, no credential among it (decision 6), whose
    /// `PATH` finds no program in the worktree, `run_dir` (the run's
    /// directory) or the main checkout (decision 2). The script's
    /// committed text is written first under `scratch`, a directory of the
    /// attempt the runtime owns, outside the worktree and the run's
    /// directory, which the worker can write.
    fn command(
        &self,
        program: &super::super::review_programs::SnapshotProgram,
        worktree: &Path,
        run_dir: &Path,
        scratch: &Path,
    ) -> Result<CommandSpec>;
}
