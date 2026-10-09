//! The ports of host運用 (docs/design/architecture.md, section
//! "portのmodule"): the connections to the queue, the service manager,
//! the session wrappers and cmux, sccache, processes, the installed plugin,
//! the headless jobs, and the supervisors' registrations and the repository
//! binding.

use super::shared::{Queue, StdinUnprepared};
use crate::domain::{
    EventKind, GoalId, LeaseToken, ProposalId, RunId, SupervisorMode, SupervisorRegistration,
};
use anyhow::Result;
use std::{
    io,
    path::{Path, PathBuf},
};

/// Opens connections to the queue: the supervisor's own, and one for each
/// thread that works beside its loop (the heartbeat, validations,
/// landings).
pub trait QueueOpener: Send + Sync {
    fn open(&self) -> Result<Box<dyn Queue + Send>>;
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

/// The variable that moves the user's `config.toml` and the host-wide
/// `host.toml` away from `~/.config/dagq/`.
pub const CONFIG_HOME_ENV: &str = "XDG_CONFIG_HOME";

/// The environment variables the LaunchAgent gives the supervisor, which
/// is all a launchd-started process keeps of the shell that ran `up`: its
/// PATH and, only when that shell exported it, `XDG_CONFIG_HOME`. The
/// supervisor calls no cmux, so no cmux variable is among them
/// (ADR-t1433-4).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SupervisorEnvironment {
    pub path: String,
    /// `XDG_CONFIG_HOME` as exported (non-empty) by the invoking shell, so
    /// the supervisor reads the same `config.toml` and `host.toml` as the
    /// `up` that checked them; unset, both are under `~/.config/dagq/`.
    pub config_home: Option<String>,
}

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

/// The session wrappers of the headless runs and the runtime's planners,
/// started without a workspace as background processes of this host
/// (ADR-t1404-1): the port of the supervisor, the runtime's planners and
/// the reads that judge a planner (`planners`, `planner request`), which
/// call no cmux (ADR-t1433-1 decision 1; the inbox's workspace is
/// [`WorkspaceBackend`]'s). A wrapper is named by the handle its start
/// returned, the pid and start time of its process (ADR-t1404-1 decision
/// 2), recorded where a run or a planner records its session; any other
/// session ID (a workspace an older binary opened) names no wrapper. Beside
/// the start, the stop and the liveness, it carries the limits of one call
/// and the waits the supervisor gives a wrapper. Implemented by
/// `infrastructure::adapters::BackgroundSessions` over `BackgroundWrappers`;
/// [`crate::application::recording::RecordingSessions`] records its failures
/// and stops (`wrapper_stopped`).
pub trait SessionWrappers {
    /// Start the session wrapper `command` (a shell command line) in `cwd`
    /// as a process detached from this one, with `env` in its environment
    /// and its output in `log` (ADR-t1404-1 decision 1), and return the
    /// [`BackgroundHandle`](crate::domain::background_wrapper::BackgroundHandle)
    /// the run or the planner records as its session.
    fn launch_background(
        &self,
        cwd: &std::path::Path,
        command: &str,
        env: &[(String, String)],
        log: &std::path::Path,
    ) -> Result<String>;
    /// Stop the background wrapper `handle` names and what it started, and
    /// say how it ended: SIGTERM, SIGKILL or already gone, and the SIGKILLs
    /// sent to what it started (task 1657). `route` is the path of the
    /// runtime that stops it, which the recording records with the stop as
    /// `wrapper_stopped`; the adapter does not read it. A port that does
    /// not tell how a stop ended says `None`, and nothing is recorded.
    fn stop_background(
        &self,
        handle: &str,
        route: crate::domain::background_wrapper::StopRoute,
    ) -> Result<Option<crate::domain::background_wrapper::WrapperStop>>;
    /// Whether the wrapper `handle` names still runs: its pid is alive with
    /// the start time the handle recorded. Any other ID is not open.
    fn exists(&self, handle: &str) -> Result<bool>;
    /// How long one call may run before it is given up as failed; recorded
    /// with every `backend_call_failed`.
    fn call_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(30)
    }
    /// How many times in all a call that timed out is made when making it
    /// again is safe (a read; task 326).
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
    /// How long the session's wrapper may take to register after its start
    /// before the supervisor gives the run up.
    fn registration_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(45)
    }
    /// How long after an attempt to open a headless run's lost session
    /// again the supervisor makes the next one (task 1372).
    fn reopen_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(60)
    }
    /// How long a resumed session may work on the resolution request
    /// without going idle before the supervisor asks it to exit.
    fn resume_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(3600)
    }
}

/// The inbox's cmux workspace, which the inbox and `up` / `down` own
/// (docs/design/architecture.md, "host運用"; ADR-t1433-1 decision 1): `up`
/// opens, looks up, marks and closes the workspaces it keeps, and the
/// inbox's `watch --role inbox` notifies a person. The session wrappers of
/// the runs and the runtime's planners are [`SessionWrappers`]', which call
/// no cmux. Implemented by `infrastructure::adapters::Cmux`, which calls
/// cmux's CLI; [`crate::application::recording::RecordingBackend`] records
/// its failures.
pub trait WorkspaceBackend {
    fn preflight(&self) -> Result<()>;
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
    /// Whether the workspace with this stable ID is still open. Workspaces
    /// are found by the ID the queue recorded, never by their title, which
    /// people may rename (ADR-0026).
    fn exists(&self, workspace_id: &str) -> Result<bool>;
    /// Open a workspace that is not tied to a run (the inbox's session) and
    /// return its stable ID. A run's session has no workspace: its wrapper
    /// starts in the background ([`SessionWrappers::launch_background`],
    /// ADR-t1433-3).
    fn create_named(
        &self,
        name: &str,
        cwd: &std::path::Path,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String>;
    /// The handle of the workspace group whose external ID is
    /// `external_id`, created under `name` when there is none yet; asking
    /// again returns the same group.
    fn ensure_group(&self, external_id: &str, name: &str) -> Result<String>;
    /// Tell a person that something waits for them: a notification, never
    /// keystrokes into a terminal. `workspace` is the workspace it belongs
    /// to; `None` sends it without one. Only the inbox's `watch --role
    /// inbox` sends one, for each new ask it returns, aimed at the inbox
    /// (ADR-0022 decision 5, ADR-t1433-1 decision 2); the supervisor, the
    /// queue service and the observer send none.
    fn notify(&self, title: &str, body: &str, workspace: Option<&str>) -> Result<()>;
    /// How long one call may run before the backend gives it up as failed;
    /// recorded with every `backend_call_failed`.
    fn call_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(30)
    }
    /// How many times in all a call that timed out is made when making it
    /// again is safe (a read; task 326).
    fn call_attempts(&self) -> u32 {
        3
    }
    /// The backoff before the first retry of a call that timed out,
    /// doubled before each next one.
    fn retry_backoff(&self) -> std::time::Duration {
        std::time::Duration::from_secs(2)
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
    fn process(&self, port: u16) -> Result<Option<crate::domain::sccache::ServerProcess>>;
    /// Never call a stats client when the port is absent.
    fn stats(
        &self,
        program: &Path,
        env: &[(String, String)],
        port: u16,
    ) -> Result<Option<crate::domain::sccache::ServerStats>>;
    fn stop(&self, program: &Path, env: &[(String, String)], port: u16) -> Result<()>;
    /// Start the server with `program --start-server` and `env` beside this
    /// process's (the caller puts `SCCACHE_IDLE_TIMEOUT=0` in it), wait
    /// until it listens on `port`, and read the pid of the process that
    /// listens there.
    fn start(&self, program: &Path, env: &[(String, String)], port: u16) -> Result<ServerPid>;
    /// Make in `dir` the guard a process given `[run.env]` is given as
    /// `RUSTC_WRAPPER` (ADR-t2086-1), and return its path; one
    /// that cannot be made is an error, and the process then runs without
    /// `RUSTC_WRAPPER`.
    fn guard(&self, dir: &Path) -> Result<PathBuf> {
        anyhow::bail!("no guard is made for {}", dir.display())
    }
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
    /// (SIGINT); used for a supervisor an earlier binary started in a cmux
    /// workspace (the retired in-cmux mode), which no service manager can
    /// signal for us.
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
    /// This user's processes with their parents and the paths of their
    /// executables, for the cleanup of an ended run's worktree (task
    /// 1590); no working directories are read.
    fn executables(&self) -> Result<Vec<crate::domain::disk::ProcessExecutable>> {
        anyhow::bail!("this process control cannot list executables")
    }
    /// The processes `pid` started, and theirs, from [`Self::list`]; none
    /// when the processes cannot be listed.
    fn descendants(&self, pid: u32) -> Vec<u32> {
        self.list()
            .map(|all| crate::domain::headless_job::descendants(&all, pid))
            .unwrap_or_default()
    }
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
    /// The provider an agent job runs on (`headless_jobs.provider`);
    /// `None` for a program job, recorded as
    /// [`crate::domain::headless_job::NO_PROVIDER`].
    pub provider: Option<crate::domain::Provider>,
    pub pid: u32,
    /// [`ProcessControl::start_identity`] of `pid` just after the start.
    pub process_start: Option<String>,
    pub supervisor_token: LeaseToken,
    /// The file its stdout goes to, where a supervisor that stops it after
    /// its own is gone reads its agent's Execution (ADR-t1486-1).
    pub stdout: Option<std::path::PathBuf>,
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
    /// before the column was, `none` for a program job).
    pub provider: String,
    pub pid: u32,
    pub process_start: Option<String>,
    pub supervisor_token: LeaseToken,
    /// The pid of that supervisor's registration, when it has one left.
    pub supervisor_pid: Option<u32>,
    pub started_at: i64,
    /// [`NewHeadlessJob::stdout`]; `None` for a row written without it.
    pub stdout: Option<std::path::PathBuf>,
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

/// The records of the inbox's watcher and of the backstop for an inbox
/// without one, which host運用's `supervise::inbox_nudge` writes: queue
/// events (on no task, goal or run) whose comparison and write share one
/// write transaction, so a second supervisor and the process after an exec
/// make neither twice (docs/design/architecture.md, "host運用").
pub trait InboxWatchLog {
    /// Record `inbox_nudged` with `payload` unless one with the same
    /// `absent_since` and `attempt` is recorded (ADR-t1433-5 decision 1
    /// (3)), in one write transaction: `false` when another supervisor
    /// recorded it first. Only the claimer nudges the inbox.
    fn claim_inbox_nudge(&self, payload: serde_json::Value) -> Result<bool>;
    /// Record `kind` (`inbox_watcher_absent` or `inbox_watcher_returned`;
    /// any other is refused) with `payload` unless the latest of the two
    /// is already `kind` or its `at` is later than `payload`'s, in one
    /// write transaction (task 1021): `false` when the state did not
    /// change, another supervisor recorded the change first, or the
    /// judgment is older than the recorded one.
    fn record_inbox_watcher_change(
        &self,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<bool>;
}

/// The supervisors' registrations, their handoffs and settings, and the
/// repository the queue is bound to: the coordination state of host運用
/// (ADR-0032's second kind). Its reads (`supervisors`, `handoff_request`,
/// `repository_binding`, `assert_repository`) are published to every
/// context and the rest to 実行と着地's loop (docs/design/architecture.md).
pub trait SupervisorRegistry {
    /// Register a supervisor with its slot limits and their sources in one
    /// write, so `status` and a reader of the registration never see it
    /// without them.
    fn register_supervisor(
        &mut self,
        token: &LeaseToken,
        pid: u32,
        limits: crate::domain::slot_limits::SlotLimits,
        binary_version: &str,
    ) -> Result<SupervisorRegistration>;
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
    /// Point the queue at `common_dir` whatever it was bound to, and return
    /// the previous binding (`rebind`, ADR-0020).
    fn rebind_repository(&mut self, common_dir: &str) -> Result<Option<String>>;
    /// Git common directory the queue is bound to, if any.
    fn repository_binding(&self) -> Result<Option<String>>;
    fn bind_repository(&mut self, common_dir: &str) -> Result<()>;
    fn assert_repository(&self, common_dir: &str) -> Result<()>;
    /// Record how `up` started the supervisor `token`.
    fn set_supervisor_mode(
        &self,
        token: &LeaseToken,
        mode: SupervisorMode,
        workspace_id: Option<&str>,
    ) -> Result<()>;
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
