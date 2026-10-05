//! `up` and `down`: the cold start and the stop of one queue's runtime. `up`
//! makes sure a supervisor is resident (as a launchd LaunchAgent, restarted
//! after any exit) and that the inbox's Claude session has a cmux
//! workspace, and reports the queue's open work. Planners are not resident:
//! the runtime opens one when there is planning to do ([`super::planner`],
//! ADR-t1394-1). `down` unloads the agent so the
//! supervisor drains and is not restarted. Both are idempotent: a second
//! `up` reuses what the first one started.
//!
//! A launchd-started supervisor is not a child of a cmux terminal, and cmux
//! admits such a process only by socket password. `up` therefore proves the
//! connection from outside cmux (a `ping` with the agent's environment, run
//! outside cmux's process tree) before it writes the agent, or launchd would
//! keep restarting a supervisor that fails its own preflight forever. Where
//! that password is not configured, `up --in-cmux` starts the supervisor
//! inside the cmux workspace `[<repo>]supervisor` instead, with no
//! launchd involved and so nothing to restart it (ADR-0011).
//!
//! A supervisor is only reused while it runs this binary's own build.
//! Every registration carries the `binary_version` its process recorded,
//! and `up` hands a live supervisor of any other build over to this binary
//! without waiting for its sessions (the supervisor execs it under its own
//! pid and token), or drains one that cannot take a handoff before
//! starting one of its own in its place (ADR-0045 decisions 10, 15).
//!
//! The use cases reach the queue, cmux, launchd, processes, Claude Code and
//! the files through [`Ports`]; the entry points in [`crate::compose`]
//! build the adapters.
use super::{
    AgentProvider, CONFIG_HOME_ENV, Clock, DetachedRefusal, LaunchAgent, ProcessControl, Queue,
    QueueOpener, RunFiles, SOCKET_PASSWORD_ENV, SupervisorEnvironment, WorkspaceBackend,
    WorkspaceTags,
    actor_executor::{
        ActorExecutionSpec, ActorExecutor, ActorProgram, HostActorExecutor, WorkspaceAccess,
        WorkspaceCommand, actor_env,
    },
    naming::{
        inbox_workspace_name, shell_join, supervisor_workspace_name, workspace_description,
        workspace_group_name,
    },
    path_text,
    prompt::inbox_prompt,
    recording::RecordingBackend,
};
use crate::domain::EventKind;
use crate::domain::LeaseToken;
use crate::domain::language::{Language, with_instruction};
use crate::{
    VERSION,
    domain::{
        ActorContext, ActorRole, HEARTBEAT_TIMEOUT_SECS, RunStatus, SessionRole, SupervisorMode,
        SupervisorRegistration, recovery::ProcessInfo, run_env::RunEnvCheck,
    },
};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    cell::{OnceCell, RefCell},
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

/// Set in the environment of every workspace of a queue (`--env`, which
/// every shell of the workspace inherits): the role the workspace plays, so
/// that `up`, run from inside the inbox session (the plugin skill calls
/// it), does not open a second one, and the plugin's hook knows the session
/// however it was started (ADR-0026).
pub const ROLE_ENV: &str = crate::domain::actor::ROLE_ENV;
/// The queue database the workspace belongs to.
pub const QUEUE_ENV: &str = "DAGQ_QUEUE";
/// `DAGQ_ROLE` of a run's session wrapper (and of its resumes').
pub const WORKER_ROLE: &str = ActorRole::Worker.as_str();
/// `DAGQ_ROLE` of a planner session, which writes goals and tasks and
/// submits them as a proposal. The runtime opens one in a workspace
/// `[<repo>]planner#<id>`; `up` opens none, and `dagq plan` no longer opens
/// one a person talks with (ADR-t1394-1).
pub const PLANNER_ROLE: &str = ActorRole::Planner.as_str();
pub use crate::domain::actor::{PLANNER_ID_ENV, PLANNER_ORIGIN_ENV, SESSION_KIND_ENV};
/// The cmux workspace a session runs in, set by cmux in every terminal:
/// the planner workspace that owns the proposals it submits.
pub const CMUX_WORKSPACE_ENV: &str = "CMUX_WORKSPACE_ID";
/// `DAGQ_ROLE` of the session where a person answers the queue's asks. `up`
/// opens its workspace `[<repo>]inbox`.
pub const INBOX_ROLE: &str = ActorRole::Inbox.as_str();
/// `DAGQ_ROLE` of the periodic observer job (ADR-0024 decision 4). The CLI
/// refuses every command that changes queue state from this environment,
/// except notes, draft goals and the draft tasks of a draft goal.
pub const OBSERVER_ROLE: &str = ActorRole::Observer.as_str();
/// `DAGQ_ROLE` of the supervisor's headless review of a run (ADR-0027). The
/// CLI allows it, like every headless job, only commands that read the
/// queue.
pub const REVIEW_JOB_ROLE: &str = ActorRole::ReviewJob.as_str();
/// `DAGQ_ROLE` of the recovery (triage) job of a run (ADR-0047).
pub const RECOVERY_JOB_ROLE: &str = ActorRole::RecoveryJob.as_str();
/// `DAGQ_ROLE` of the plan review of a proposal.
pub const PLAN_REVIEW_JOB_ROLE: &str = ActorRole::PlanReviewJob.as_str();
/// `DAGQ_ROLE` of the goal review of a goal.
pub const GOAL_REVIEW_JOB_ROLE: &str = ActorRole::GoalReviewJob.as_str();
/// `DAGQ_ROLE` every headless job ran under before the four roles above
/// (ADR-t728-1 decision 2): still read, as a read-only job.
pub const REVIEWER_ROLE: &str = crate::domain::actor::LEGACY_REVIEWER_ROLE;
/// The id of the actor a session is (ADR-t728-1 decision 4).
pub const ACTOR_ID_ENV: &str = crate::domain::actor::ACTOR_ID_ENV;
/// The run a worker works on.
pub const RUN_ID_ENV: &str = crate::domain::actor::RUN_ID_ENV;
/// File under the queue's log directory that launchd appends the
/// supervisor's stdout and stderr to.
pub const LAUNCHD_LOG_NAME: &str = "launchd.log";

/// What `up` reads from the process that runs it.
#[derive(Debug, Clone)]
pub struct UpEnvironment {
    /// `DAGQ_ROLE`, if set.
    pub role: Option<String>,
    /// `DAGQ_QUEUE`, if set.
    pub queue: Option<PathBuf>,
    /// `PATH`, copied into the agent so the supervisor finds what this shell finds.
    pub path: String,
    /// `CMUX_SOCKET_PASSWORD`, if this shell exported it (non-empty); it is
    /// then copied into the agent too.
    pub socket_password: Option<String>,
    /// `XDG_CONFIG_HOME`, if this shell exported it (non-empty); it is then
    /// copied into the agent too, so the launchd-run supervisor reads the
    /// `config.toml` and `host.toml` this `up` read.
    pub config_home: Option<String>,
    /// The binary launchd runs: this one, by absolute path.
    pub current_exe: PathBuf,
    /// Claude Code's global config, which records the folder trust of each
    /// repository (`claude_global_config`); `None` trusts nothing.
    pub claude_config: Option<PathBuf>,
    /// The user's `config.toml` (`$XDG_CONFIG_HOME/dagq/config.toml`) the
    /// language comes from under the repository's `dagq.toml`
    /// (ADR-t616-2); `None` reads none.
    pub user_config: Option<PathBuf>,
    /// The runtime runs this `up` to start a supervisor again after an
    /// `install --allow-breaking` drain or an automatic update
    /// ([`UP_RESTART_ENV`]): it does not check the installed plugin, as a
    /// handoff does not (ADR-t617-2 decision 1).
    pub restart: bool,
}

/// Set by the runtime in the environment of the `up` it runs to start a
/// supervisor again ([`UpEnvironment::restart`]); a binary that does not
/// know it ignores it.
pub const UP_RESTART_ENV: &str = "DAGQ_UP_RESTART";

/// What `up` says when cmux does not admit a process from outside its
/// terminals. The remedies are the operator's: cmux's CLI takes the password
/// saved in its Settings on its own, or `CMUX_SOCKET_PASSWORD` from the
/// shell that runs `up` (stored in the agent then).
pub const DETACHED_CMUX_HINT: &str = "cmux refused a connection from outside its own terminals, \
so the supervisor launchd starts could not reach it. Either save a socket password in cmux \
Settings (its CLI uses it on its own), or export CMUX_SOCKET_PASSWORD before `up` (it is then \
written into the LaunchAgent); or run `up --in-cmux`, which starts the supervisor inside a cmux \
workspace without launchd and without any automatic restart";

/// What `up` says when Claude Code has not trusted the repository: every
/// run session would stop at the folder trust dialog, since run worktrees
/// take their trust from the repository root.
pub fn untrusted_repository_hint(root: &Path, config: Option<&Path>) -> String {
    format!(
        "Claude Code has not trusted the repository {root}, so every run session would stop at \
its folder trust dialog (run worktrees take their trust from the repository root). Start `claude` \
once in {root} and accept \"Yes, I trust this folder\", then run `up` again; {config} must then \
record projects[\"{root}\"].hasTrustDialogAccepted = true",
        root = root.display(),
        config = config.map_or_else(|| "~/.claude.json".into(), |c| c.display().to_string()),
    )
}

/// The plugin whose skills the inbox's and the planners' prompts name.
pub const DAGQ_PLUGIN: &str = "claude-dagq";
/// The official commands that install [`DAGQ_PLUGIN`] (ADR-t617-1).
pub const PLUGIN_INSTALL_COMMANDS: [&str; 2] = [
    "claude plugin marketplace add hisamekms/dagq",
    "claude plugin install claude-dagq@dagq",
];
/// The arguments of `claude` that bring the installed [`DAGQ_PLUGIN`] to
/// the newest release its marketplace names, in order (ADR-t618-2): read
/// the marketplace again, then update the plugin.
pub const PLUGIN_UPDATE_ARGUMENTS: [&[&str]; 2] = [
    &["plugin", "marketplace", "update", "dagq"],
    &["plugin", "update", "claude-dagq@dagq"],
];

/// Make sure the `claude` at `executable` (`agent`) loads [`DAGQ_PLUGIN`]
/// in sessions started in `cwd`, before `dagq <command>` opens the
/// `session` there without `--plugin-dir` (ADR-t617-2 decision 4): a
/// session without it has none of the skills its prompt names. Anything
/// but an enabled install, a failed or unreadable listing included, is an
/// error that says how to install it.
pub fn require_installed_plugin(
    agent: &dyn AgentProvider,
    executable: &Path,
    cwd: &Path,
    session: &str,
    command: &str,
) -> Result<()> {
    let enable;
    let reason = match agent.installed_plugin(cwd, DAGQ_PLUGIN) {
        Ok(super::PluginState::Enabled) => return Ok(()),
        Ok(super::PluginState::Disabled(ids)) => {
            enable = format!(
                " (or enable it with {})",
                ids.iter()
                    .map(|id| format!("`claude plugin enable {id}`"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            );
            format!("the {DAGQ_PLUGIN} plugin is installed but disabled")
        }
        Ok(super::PluginState::Missing) => {
            enable = String::new();
            format!("the {DAGQ_PLUGIN} plugin is not installed")
        }
        Err(error) => {
            enable = String::new();
            format!(
                "whether the {DAGQ_PLUGIN} plugin is installed could not be checked ({})",
                format!("{error:#}").trim()
            )
        }
    };
    bail!(
        "{reason} in the Claude Code at {claude}, so the {session} session would start without \
the dagq skills its prompt names. Install it with `{add}` and `{install}`{enable}, then run \
`dagq {command}` again; to use a plugin checkout instead, pass --plugin-dir",
        claude = executable.display(),
        add = PLUGIN_INSTALL_COMMANDS[0],
        install = PLUGIN_INSTALL_COMMANDS[1],
    )
}

/// Where the queue `up` and `down` work on lives: its database, its hash
/// (the external ID of its workspace group), and the LaunchAgent label,
/// plist and log directory of its supervisor.
#[derive(Debug, Clone)]
pub struct QueuePaths {
    pub db: PathBuf,
    pub hash: String,
    pub label: String,
    pub launch_agent: PathBuf,
    pub log_dir: PathBuf,
}

/// The repository `up` starts the supervisor for: the checkout it was
/// inspected from, its Git common directory, and its landing branch and
/// push (ADR-t615-1) or why they did not resolve.
#[derive(Debug, Clone)]
pub struct RepositoryPaths {
    pub root: PathBuf,
    pub common_dir: PathBuf,
    /// The main checkout, whose `dagq.toml` `up` reads and whose folder
    /// Claude Code trusts, or why the repository has none.
    pub checkout: std::result::Result<PathBuf, String>,
    pub landing: std::result::Result<crate::domain::landing_branch::RepositorySettings, String>,
    /// Whether the repository is dagq's source (ADR-t614-1), which
    /// `--auto-update` needs.
    pub dagq_source: bool,
}

/// What `up` and `down` reach the outside through. `queues` opens the queue at a database
/// path (once for the use case, and again for each recorded cmux failure);
/// `inspect_repository` finds the repository containing a checkout;
/// `trusts_repository(config, root)` reads Claude Code's global config for
/// the folder trust of `root`.
pub struct Ports<'a> {
    pub cmux: &'a dyn WorkspaceBackend,
    pub launchd: &'a dyn LaunchAgent,
    pub processes: &'a dyn ProcessControl,
    pub files: &'a dyn RunFiles,
    pub clock: &'a dyn Clock,
    pub queues: &'a dyn Fn(&Path) -> Arc<dyn QueueOpener>,
    pub inspect_repository: &'a dyn Fn(&Path) -> Result<RepositoryPaths>,
    pub trusts_repository: &'a dyn Fn(&Path, &Path) -> Result<bool>,
    /// `run_env_programs(checkout, db, path)` checks the programs the
    /// `[run.env]` of the `dagq.toml` in `checkout` names on `path`
    /// (ADR-0049 decision 9).
    pub run_env_programs: &'a dyn Fn(&Path, &Path, &str) -> Result<RunEnvCheck>,
    /// `resolve_language(checkout, user_config)`: the language of the
    /// `dagq.toml` in `checkout` over the user's `config.toml`, or the
    /// mistake in either (ADR-t616-2).
    pub resolve_language: &'a ResolveLanguage,
    pub load_average: fn() -> Option<f64>,
    /// The agent of the sessions `up` opens (the inbox), given `--claude`.
    pub agent: &'a dyn Fn(&Path) -> Box<dyn AgentProvider>,
    /// The queue's resource broker: `up`'s preflight and `down`'s stop
    /// (ADR-t827-3 decision 2).
    pub broker: &'a dyn BrokerLifecycle,
    /// `queue_service(db, executable, cmux)`: the control of the queue's
    /// service, started from `executable` (ADR-t1233-4 decision 1).
    pub queue_service: &'a QueueServiceControls,
}

/// How `up` and `down` reach the queue's service: see
/// [`Ports::queue_service`].
pub type QueueServiceControls =
    dyn Fn(&Path, &Path, &Path) -> Box<dyn crate::application::queue_service::QueueServiceControl>;

/// What `up` and `down` do about the queue's resource broker (ADR-t827-3
/// decision 2). With the mode `disabled` (the default) neither does
/// anything, and no podman is looked for or run.
pub trait BrokerLifecycle {
    /// `up`'s preflight for the supervisor it starts, with the `[broker]`
    /// of the `dagq.toml` in `checkout` and the queue's `host.toml`: the
    /// mode, and for a mode other than `disabled` the podman executable on
    /// `path`. `None` for `disabled`; an error (no podman, `required`) keeps
    /// the supervisor from starting.
    fn preflight(&self, checkout: &Path, db: &Path, path: &str) -> Result<Option<Value>>;
    /// `down` after the drain (`drained`): stop the queue's container and
    /// then dagq's machine when no container runs on it, as `dagq broker
    /// stop` does. Before it (a `down` that returns while the supervisor
    /// drains), only what is left to do. `None` for `disabled`.
    fn after_drain(&self, db: &Path, drained: bool) -> Result<Option<Value>>;
    /// Before `down` signals `supervisors`: ask them to stop the queue's
    /// broker once their drain ends (ADR-t827-3 decisions 2 and 5), so a
    /// `down` that does not wait for the drain stops it too. `false` for
    /// `disabled`, which records nothing. `up`'s replacement never asks.
    fn request_stop(&self, db: &Path, supervisors: &[LeaseToken]) -> Result<bool>;
}

/// How `up` resolves the language: `(checkout, user_config)` to the
/// language in force, or the mistake in either file (ADR-t616-2).
pub type ResolveLanguage = dyn Fn(&Path, Option<&Path>) -> Result<Option<Language>>;

#[derive(Debug, Clone)]
pub struct UpOptions {
    /// Explicit operator policy: never start Claude; unsupported roles wait for manual handling.
    pub no_claude: bool,
    /// The supervisor's `--parallel`, passed only when given: without it
    /// the supervisor follows `[supervisor]` of `dagq.toml` (task 698).
    pub parallel: Option<u16>,
    /// The supervisor's `--max-waiting` (ADR-0062 decision 7), passed only
    /// when given, as `parallel` is.
    pub max_waiting: Option<u16>,
    /// The supervisor's `--runtime-planners` (ADR-0041 decision 12),
    /// passed only when given, as `parallel` is (task 941).
    pub runtime_planners: Option<u16>,
    /// The supervisor's `--max-load` (task 327): no new run is claimed
    /// while the 1-minute load average is above it; 0 or below disables
    /// the hold. Passed only when given: without it the supervisor holds
    /// at twice the host's logical cores (task 623).
    pub max_load: Option<f64>,
    /// Start the supervisor inside the cmux workspace `[<repo>]supervisor`
    /// instead of as a LaunchAgent: no launchd, no automatic restart, and no
    /// out-of-cmux preflight to pass.
    pub in_cmux: bool,
    /// Refuse to wait for a supervisor of another version to drain. Only a
    /// replacement reads it (ADR-0014): with runs in flight `up` stops
    /// without touching anything, and with none it replaces the supervisor
    /// but bounds the drain by `startup_timeout` rather than waiting for a
    /// supervisor that turns out not to stop.
    pub no_wait: bool,
    /// Passed to the inbox's `claude` as `--plugin-dir`.
    pub plugin_dir: Option<PathBuf>,
    /// Resolved executables; the agent runs the supervisor with these, and
    /// the inbox workspace starts this `claude`.
    pub cmux: PathBuf,
    pub claude: PathBuf,
    /// The Codex CLI the supervisor's Codex workers start (ADR-t813-2),
    /// resolved when it was found; a missing one does not stop `up`: the
    /// supervisor then claims no Codex task, and `status` says so.
    pub codex: PathBuf,
    /// How long a started supervisor may take to register before `up` fails.
    pub startup_timeout: Duration,
    /// How long a supervisor asked to hand off may take to come back under
    /// this binary: its wait for the validation or landing in progress and
    /// the exec (ADR-0045 decision 10).
    pub handoff_timeout: Duration,
    /// Have the supervisor update its own binary on every landing that
    /// changes the runtime (ADR-0045 decision 17): the started one runs
    /// `supervise --auto-update`, and the registration of one reused or
    /// handed over gets it; `false` turns it off on those.
    pub auto_update: bool,
    /// Start (or reuse, or replace) the queue's service before the
    /// supervisor (ADR-t1233-4 decision 1); the command line's `up` does,
    /// a test of `up`'s other steps need not.
    pub queue_service: bool,
    pub poll: Duration,
}

/// How long `up` waits for the queue's service to answer.
pub const QUEUE_SERVICE_START_TIMEOUT: Duration = Duration::from_secs(10);

/// Ensure the supervisor and the inbox workspace exist and report the queue's open work. Preflight first (cmux, claude, the installed plugin without `--plugin-dir`, Claude Code's trust of
/// the repository root, an initialized queue, the repository), then prune registrations whose process is gone, start
/// the agent only when no live registration of this binary's version
/// remains (after proving that cmux admits a process with the agent's
/// environment) — draining and replacing a live supervisor of any other
/// version — and open the inbox workspace only outside that session itself.
/// A workspace recorded for a role `up` no longer opens (the maintainer
/// ADR-0024 retired, the resident planner ADR-0041 decision 6 retired) is
/// forgotten; the workspace itself is left for a person to close.
///
/// `claude` is the Claude Code the inbox session starts, whose preflight
/// `up` runs.
pub fn up(
    ports: &Ports,
    claude: &dyn AgentProvider,
    location: &QueuePaths,
    repo: &Path,
    environment: &UpEnvironment,
    options: &UpOptions,
) -> Result<Value> {
    ensure!(
        options.parallel.is_none_or(|parallel| parallel >= 1),
        "parallel must be at least 1"
    );
    let (cmux, launchd, processes) = (ports.cmux, ports.launchd, ports.processes);
    let db = ports
        .files
        .canonicalize(&location.db)
        .context("queue must already be initialized")?;
    let repository = (ports.inspect_repository)(repo)?;
    cmux.preflight()?;
    if !options.no_claude {
        claude.preflight()?;
    }
    // Without --plugin-dir the inbox loads the plugin the user installed
    // (ADR-t617-2 decisions 1, 4). A restart by the runtime is not a
    // person's `up`, and starts what ran before.
    if !options.no_claude && options.plugin_dir.is_none() && !environment.restart {
        require_installed_plugin(claude, &options.claude, &repository.root, "inbox", "up")
            .map_err(|error| anyhow::anyhow!("{error:#}; the supervisor was not started"))?;
    }
    // Claude Code keys trust by the main checkout even for a linked
    // worktree, and `up` may run from any worktree of the repository; the
    // supervisor reads every setting from its `dagq.toml`.
    let trust_root = match &repository.checkout {
        Ok(checkout) => checkout.as_path(),
        Err(error) => bail!("{error}; the supervisor was not started"),
    };
    let trusted = if options.no_claude {
        true
    } else {
        match environment.claude_config.as_deref() {
            Some(config) => (ports.trusts_repository)(config, trust_root)?,
            None => false,
        }
    };
    ensure!(
        trusted,
        untrusted_repository_hint(trust_root, environment.claude_config.as_deref())
    );
    // The supervisor `up` starts gets this PATH, and a worker's workspace
    // one from the same login shell: a program [run.env] names that is not
    // on it would fail every run's cargo (ADR-0049 decision 9).
    let run_env = (ports.run_env_programs)(trust_root, &db, &environment.path)?;
    if let Some(message) = run_env.missing_message() {
        bail!("{message}; the supervisor was not started");
    }
    // The supervisor keeps the broker of a mode other than `disabled`
    // with podman on this PATH (ADR-t827-3 decision 2).
    let broker = ports
        .broker
        .preflight(trust_root, &db, &environment.path)
        .map_err(|error| anyhow::anyhow!("{error:#}; the supervisor was not started"))?;
    // A mistake in the language is caught before anything starts; later it
    // only leaves the prompts without the instruction (ADR-t616-2).
    let language = (ports.resolve_language)(trust_root, environment.user_config.as_deref())
        .map_err(|error| anyhow::anyhow!("{error:#}; the supervisor was not started"))?;
    // Every claim and landing reads the landing branch: one that does not
    // resolve would hold them all, and a configured push remote that is
    // missing would fail every push (ADR-t615-1).
    let landing = match &repository.landing {
        Ok(landing) => landing.clone(),
        Err(error) => bail!("{error}; the supervisor was not started"),
    };
    // The automatic update builds dagq from this repository (ADR-t614-1).
    if options.auto_update && !repository.dagq_source {
        bail!(
            "{}; the supervisor was not started",
            auto_update_refused(trust_root)
        );
    }
    let plugin_dir = options
        .plugin_dir
        .as_deref()
        .map(|dir| {
            ports
                .files
                .canonicalize(dir)
                .with_context(|| format!("plugin directory {}", dir.display()))
        })
        .transpose()?;
    let queues = (ports.queues)(&db);
    let queue = queues.open()?;
    let queue: &dyn Queue = &*queue;
    // The queue's service before the supervisor, of this binary's build
    // (ADR-t1233-4 decision 1): one that does not start stops `up`.
    let queue_service = if options.queue_service {
        let control = (ports.queue_service)(&db, &environment.current_exe, &options.cmux);
        let report =
            crate::application::queue_service::ensure(&*control, QUEUE_SERVICE_START_TIMEOUT)
                .map_err(|error| {
                    anyhow::anyhow!(
                        "{error:#}; up stopped before it started or reused a supervisor"
                    )
                })?;
        if report["outcome"] != "reused" {
            queue.record_queue_event(
                crate::domain::EventKind::QueueServiceStarted,
                json!({
                    "by": "up",
                    "pid": report["service"]["pid"],
                    "build": report["service"]["build"],
                    "api_version": report["service"]["api_version"],
                    "socket": report["service"]["socket"],
                    "restart": false,
                    "replaced": report["replaced"],
                }),
            )?;
        }
        Some(report)
    } else {
        None
    };
    // Every workspace call from here on is recorded when it fails; none of
    // them is for a run.
    let recording = RecordingBackend::over(cmux, queues, None, ports.load_average);
    let cmux: &dyn WorkspaceBackend = &recording;
    let workspaces = QueueWorkspaces::new(cmux, &db, location.hash.clone(), &repository.root);
    let up = Up {
        location,
        db: &db,
        repository: &repository,
        queue,
        workspaces: &workspaces,
        launchd,
        processes,
        clock: ports.clock,
        environment,
        options,
    };

    let mut pruned = Vec::new();
    let mut live = Vec::new();
    // Registrations that survive this pass are not the supervisor `up` is
    // about to start, so the wait below must not mistake one for it.
    let mut existing = HashSet::new();
    let now = ports.clock.now();
    let mut listing = None;
    for registration in queue.supervisors()? {
        if !processes.alive(registration.pid) {
            prune_supervisor(queue, &registration)?;
            pruned.push(json!({"token": registration.token, "pid": registration.pid}));
            continue;
        }
        if pid_taken_over(&registration, processes, now, &mut listing) {
            prune_supervisor(queue, &registration)?;
            pruned.push(json!({
                "token": registration.token,
                "pid": registration.pid,
                "reason": "pid_reused",
            }));
            continue;
        }
        let no_claude = registration.claude_disabled();
        ensure!(
            no_claude == options.no_claude,
            "the live supervisor has a different --no-claude policy; drain it with down --wait before up"
        );
        existing.insert(registration.token.clone());
        if fresh(&registration, processes, now) {
            live.push(registration);
        }
        // Alive but silent: not ours to kill; it shows up as stale in doctor.
    }

    // A supervisor of another build is not ours to reuse: it would keep
    // serving this queue with the code the operator has just replaced, and
    // it would migrate the schema of a queue the new binary owns (ADR-0014).
    let outdated = live
        .iter()
        .any(|registration| registration.binary_version.as_deref() != Some(VERSION));
    // A handoff some supervisors failed does not stop `up` short: the
    // others serve under this build, and they get `--auto-update` and the
    // inbox all the same before `up` fails naming the ones that did not.
    let mut handoff_failure = None;
    let supervisor = match live.first() {
        // Whoever started it recorded the mode; a supervisor started by
        // hand has none, and `up` does not claim one for it.
        Some(registration) if !outdated => json!({
            "outcome": "reused",
            "mode": registration.mode.map(SupervisorMode::as_str),
            "version": registration.binary_version,
            "pid": registration.pid,
            "token": registration.token,
            "workspace_id": registration.workspace_id,
            "plist": location.launch_agent,
            "log_dir": location.log_dir,
        }),
        Some(_) if takes_handoff(&up, ports.files, &live) => {
            let (supervisor, failure) = hand_off_supervisors(&up, &live)?;
            handoff_failure = failure;
            supervisor
        }
        Some(_) => replace_supervisors(&up, &live)?,
        None => start_supervisor(&up, &existing, false)?,
    };
    let supervisor = set_auto_update(queue, supervisor, &live, options.auto_update)?;

    let sessions = Sessions {
        queue,
        workspaces: &workspaces,
        environment,
        files: ports.files,
        db: &db,
        root: &repository.root,
        agent: &*(ports.agent)(&options.claude),
        plugin_dir: plugin_dir.as_deref(),
    };
    let retired_sessions = queue.forget_retired_session_workspaces()?;
    let inbox = if options.no_claude {
        json!({"outcome": "skipped", "reason": "provider_disabled", "next": "use a manually opened inbox"})
    } else {
        sessions.open(
            SessionRole::Inbox,
            inbox_workspace_name(&repository.root),
            || inbox_session_prompt(&db, language.as_ref()),
        )?
    };

    let mut report = json!({
        "supervisor": supervisor,
        "inbox": inbox,
        "retired_sessions": retired_sessions,
        "pruned_supervisors": pruned,
        "warnings": workspaces.take_warnings(),
        "doctor": open_work(queue, processes, ports.clock)?,
        "repository": landing,
        "language": language,
    });
    // What the preflight found, only for a repository with a dagq.toml.
    if run_env.config {
        report["run_env"] = serde_json::to_value(&run_env)?;
    }
    if let Some(broker) = broker {
        report["broker"] = broker;
    }
    if let Some(queue_service) = queue_service {
        report["queue_service"] = queue_service;
    }
    match handoff_failure {
        None => Ok(report),
        Some(message) => Err(PartialHandoff { message, report }.into()),
    }
}

/// The error of an `up` whose handoff some or all of the supervisors did
/// not take: `up` still set `--auto-update` and opened the inbox, and
/// `report` is what it reports, with the supervisor's outcome
/// `partially_handed_off` (or `handoff_failed` when none took it) and each
/// supervisor's result in `replaced`, for the command to print beside the
/// error. `up` replaces no file, so nothing was put back.
#[derive(Debug)]
pub struct PartialHandoff {
    pub message: String,
    pub report: Value,
}

impl std::fmt::Display for PartialHandoff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PartialHandoff {}

impl PartialHandoff {
    /// The [`PartialHandoff`] `error` is or wraps.
    pub fn of(error: &anyhow::Error) -> Option<&Self> {
        error.chain().find_map(|cause| cause.downcast_ref::<Self>())
    }
}

/// One `up`'s settled inputs, shared by the ways it starts a supervisor.
struct Up<'a> {
    location: &'a QueuePaths,
    db: &'a Path,
    repository: &'a RepositoryPaths,
    queue: &'a dyn Queue,
    workspaces: &'a QueueWorkspaces<'a>,
    launchd: &'a dyn LaunchAgent,
    processes: &'a dyn ProcessControl,
    clock: &'a dyn Clock,
    environment: &'a UpEnvironment,
    options: &'a UpOptions,
}

/// The Claude sessions `up` keeps a workspace open for: the inbox (ADR-0022,
/// ADR-0041 decision 6). Each is opened the same way: skipped
/// when `up` runs inside that very session of this queue (its `DAGQ_ROLE`
/// and `DAGQ_QUEUE`), reused while its recorded UUID is still listed, and
/// otherwise created and recorded in `session_workspaces`.
struct Sessions<'a> {
    queue: &'a dyn Queue,
    workspaces: &'a QueueWorkspaces<'a>,
    environment: &'a UpEnvironment,
    files: &'a dyn RunFiles,
    db: &'a Path,
    root: &'a Path,
    /// The agent of the sessions, on `up`'s `--claude`.
    agent: &'a dyn AgentProvider,
    plugin_dir: Option<&'a Path>,
}

impl Sessions<'_> {
    /// Open the workspace of `role`'s agent with `prompt` as its first
    /// message, through the actor executor like every AI actor.
    fn open(
        &self,
        role: SessionRole,
        name: String,
        prompt: impl FnOnce() -> Result<String>,
    ) -> Result<Value> {
        let inside = self.environment.role.as_deref() == Some(role.as_str())
            && self
                .environment
                .queue
                .as_deref()
                .and_then(|queue| self.files.canonicalize(queue).ok())
                .is_some_and(|queue| queue == self.db);
        let cmux = self.workspaces.cmux;
        if inside {
            // The session `up` runs in is most likely the recorded one; an
            // `up` from it is how an older binary's workspace gets its look.
            if let Some(id) = self.queue.session_workspace(role)?
                && matches!(cmux.exists(&id), Ok(true))
            {
                self.mark(role, &id);
            }
            return Ok(json!({"outcome": "skipped", "workspace_id": Value::Null, "name": name}));
        }
        if let Some(id) = recorded_workspace(self.queue, cmux, role)? {
            self.mark(role, &id);
            let mut report = json!({"outcome": "reused", "workspace_id": id, "name": name});
            if role == SessionRole::Inbox {
                // `up` does not start a reused inbox again (its
                // conversation stays): it keeps the guardrail it was opened
                // with, or none (ADR-t1228-2 decision 4).
                let opened = self
                    .queue
                    .latest_event_of(EventKind::InboxOpened.as_str())?;
                let view = super::inbox_guardrail::judge(
                    Some(&id),
                    opened.as_ref().map(|event| &event.payload),
                );
                report["guardrail"] = view["guardrail"].clone();
                if let Some(next) = view.get("next") {
                    report["next"] = next.clone();
                }
            }
            return Ok(report);
        }
        let id = HostActorExecutor::new(self.db)
            .with_workspaces(cmux)
            .with_provider(self.agent)
            .spawn(ActorExecutionSpec::new(
                session_actor(role),
                WorkspaceAccess::Write(self.root.to_path_buf()),
                ActorProgram::NamedWorkspace {
                    name: &name,
                    cwd: self.root,
                    command: WorkspaceCommand::Agent {
                        prompt: prompt()?,
                        plugin_dir: self.plugin_dir,
                    },
                    planner: None,
                    launch: None,
                    description: self.workspaces.description(role),
                    group: self.workspaces.group(),
                    background: None,
                },
            ))?
            .workspace()?;
        self.queue.register_session_workspace(role, &id)?;
        let mut report = json!({"outcome": "created", "workspace_id": id, "name": name});
        if role == SessionRole::Inbox {
            // Whether this inbox refuses raw cmux (ADR-t1228-2 decision 4):
            // `status` and `doctor` judge the recorded inbox by it.
            let settings = self
                .agent
                .inbox_settings(self.db.parent().unwrap_or(Path::new(".")));
            let guardrail = settings.is_some();
            self.queue.record_queue_event(
                EventKind::InboxOpened,
                json!({"workspace_id": id, "guardrail": guardrail, "settings": settings}),
            )?;
            report["guardrail"] = json!(guardrail);
        }
        self.mark(role, &id);
        Ok(report)
    }

    /// Color the workspace, put the role's status pill on it and pin it
    /// (ADR-0031). Every `up` does it again, so a workspace an older `up`
    /// opened gets it too. None of it is worth failing `up` for: what cmux
    /// refuses is a warning in `up`'s result.
    fn mark(&self, role: SessionRole, id: &str) {
        let Some((color, icon)) = session_look(role) else {
            return;
        };
        let cmux = self.workspaces.cmux;
        for (what, result) in [
            ("color", cmux.set_color(id, color)),
            (
                "status pill",
                cmux.set_status(id, ROLE_STATUS_KEY, role.as_str(), icon),
            ),
            ("pin", cmux.pin(id)),
        ] {
            if let Err(error) = result {
                self.workspaces.warnings.borrow_mut().push(format!(
                    "cmux could not set the {what} of the {} workspace {id}: {error:#}",
                    role.as_str()
                ));
            }
        }
    }
}

/// The key of the status pill a session workspace carries: dagq's own, so
/// it never replaces another tool's pill (Claude Code's `claude_code`).
pub const ROLE_STATUS_KEY: &str = "dagq_role";

/// How the sidebar tells the inbox and the planners apart at a glance
/// (ADR-0031): the workspace's cmux color and the SF Symbol of its role
/// pill (cmux workspaces have no icon of their own). Amber for the inbox,
/// where things wait for a person, Blue for a planner. Other roles keep
/// cmux's defaults.
pub fn session_look(role: SessionRole) -> Option<(&'static str, &'static str)> {
    match role {
        SessionRole::Inbox => Some(("Amber", "tray")),
        SessionRole::Planner => Some(("Blue", "map")),
        _ => None,
    }
}

/// The actor of the session `role` has a single workspace for (the inbox,
/// the in-cmux supervisor): its id is the role's name.
pub fn session_actor(role: SessionRole) -> ActorContext {
    ActorContext::new(role.actor_role(), role.as_str())
}

/// What every workspace `up` opens for a queue carries (ADR-0026): its role
/// and queue in the environment, the description line, and the queue's
/// workspace group. The group is made when the first workspace needs it
/// (cmux opens an anchor workspace with it), so an `up` that reuses
/// everything touches no group. A group cmux cannot make is a warning in
/// `up`'s result, and the workspace opens outside it.
pub struct QueueWorkspaces<'a> {
    cmux: &'a dyn WorkspaceBackend,
    db: &'a Path,
    hash: String,
    group_name: String,
    group: OnceCell<Option<String>>,
    warnings: RefCell<Vec<String>>,
}

impl<'a> QueueWorkspaces<'a> {
    pub fn new(
        cmux: &'a dyn WorkspaceBackend,
        db: &'a Path,
        hash: String,
        repo_root: &Path,
    ) -> Self {
        Self {
            cmux,
            db,
            hash,
            group_name: workspace_group_name(repo_root),
            group: OnceCell::new(),
            warnings: RefCell::new(Vec::new()),
        }
    }

    /// The tags of a workspace of `role` that belongs to no run: the
    /// in-cmux supervisor's, which is not an AI actor the executor starts,
    /// with the environment every actor's workspace has
    /// ([`actor_env`]), its actor id being the role's.
    pub fn tags(&self, role: SessionRole) -> Result<WorkspaceTags> {
        Ok(WorkspaceTags {
            env: actor_env(self.db, &session_actor(role), None, None)?,
            description: self.description(role),
            group: self.group(),
        })
    }

    /// The description line of a workspace of `role` that belongs to no run.
    pub fn description(&self, role: SessionRole) -> Option<String> {
        Some(workspace_description(role, &self.hash, None, None))
    }

    /// What cmux refused so far (the group), for the caller's result.
    pub fn take_warnings(&self) -> Vec<String> {
        self.warnings.take()
    }

    /// The queue's workspace group, made on the first ask.
    pub fn group(&self) -> Option<String> {
        self.group
            .get_or_init(
                || match self.cmux.ensure_group(&self.hash, &self.group_name) {
                    Ok(group) => Some(group),
                    Err(error) => {
                        self.warnings.borrow_mut().push(format!(
                            "cmux workspace group {:?} (external ID {}) could not be made, so the \
workspace opens outside it: {error:#}",
                            self.group_name, self.hash
                        ));
                        None
                    }
                },
            )
            .clone()
    }
}

/// The workspace the queue recorded for `role`, while cmux still lists it.
/// A recorded UUID cmux no longer lists (the workspace was closed, or cmux
/// restarted) is forgotten, so the caller opens a new one. The title is
/// never consulted: people rename workspaces (ADR-0026).
fn recorded_workspace(
    queue: &dyn Queue,
    cmux: &dyn WorkspaceBackend,
    role: SessionRole,
) -> Result<Option<String>> {
    let Some(id) = queue.session_workspace(role)? else {
        return Ok(None);
    };
    if cmux.exists(&id)? {
        return Ok(Some(id));
    }
    queue.remove_session_workspace(role)?;
    Ok(None)
}

/// Start one supervisor in the mode this `up` was asked for. The mode of a
/// supervisor being replaced does not decide it: `up --in-cmux` moves a
/// launchd queue into a workspace and a plain `up` moves it back, and
/// either way the old one has already been drained and its agent unloaded.
fn start_supervisor(
    up: &Up,
    existing: &HashSet<LeaseToken>,
    detached_proven: bool,
) -> Result<Value> {
    if up.options.in_cmux {
        start_in_cmux(up, existing)
    } else {
        start_under_launchd(up, existing, detached_proven)
    }
}

/// Drain every live supervisor of another build and start one of this
/// binary's version in its place (ADR-0014), so that updating the fixed
/// binary is `up` and nothing else. The stop is `down --wait`'s: unload the
/// LaunchAgent (whose bootout carries the SIGTERM, and whose `KeepAlive`
/// would otherwise restart the old binary at once), SIGINT an in-cmux
/// supervisor, SIGTERM one launchd did not signal, then wait for each
/// registration to go — the supervisor stops claiming, finishes the runs it
/// holds and deregisters — and close the workspaces of the in-cmux ones
/// before a new one could want the same name.
///
/// The drain is unbounded because a run is a Claude session: `--no-wait` is
/// the way to ask for the replacement only if nothing is in flight, and it
/// bounds the drain too.
///
/// Only live registrations are replaced. A supervisor that is alive but no
/// longer heartbeating is one `up` neither reuses nor kills, so an
/// old-binary one in that state is left running beside the new supervisor
/// and reported `stale`; stopping it stays a person's call
/// (ADR-0014's Consequences).
fn replace_supervisors(up: &Up, live: &[SupervisorRegistration]) -> Result<Value> {
    let Up {
        location,
        queue,
        workspaces,
        launchd,
        processes,
        environment,
        options,
        ..
    } = *up;
    // The version reported as replaced is an outdated one, not merely the
    // first: a mixed set is drained whole, but naming a version that
    // matched would read as if nothing had been out of date.
    let cmux = workspaces.cmux;
    let previous_version = live
        .iter()
        .find(|registration| registration.binary_version.as_deref() != Some(VERSION))
        .and_then(|registration| registration.binary_version.clone());
    if options.no_wait {
        // Read before anything is signalled, so a refusal leaves the old
        // supervisor serving the queue exactly as it was. What the drain
        // waits for is what the replaced supervisors lease: a run keeps its
        // lease through the review, a revise, the e2e and the landing
        // (ADR-0054 decision 6), and a resume or a recovery round takes it
        // again, so `active_runs` alone would miss most of it.
        let mut held = Vec::new();
        for registration in live {
            held.extend(
                queue
                    .runs_leased_by(&registration.token)?
                    .into_iter()
                    .map(|run| (run.id().to_string(), run.status())),
            );
        }
        if let Some(refusal) = no_wait_refusal(previous_version.as_deref(), &held) {
            bail!(refusal);
        }
    }
    // Settle what can refuse the new supervisor before the old one is
    // touched: draining a working supervisor and then failing to start its
    // replacement would leave the queue with nothing serving it. For
    // launchd that is the out-of-cmux connection (not asked again below);
    // for `--in-cmux` it is the recorded supervisor workspace.
    if options.in_cmux {
        ensure_supervisor_workspace_free(queue, cmux, live)?;
    } else {
        let spec = launch_agent_spec(location, up.db, &up.repository.root, environment, options)?;
        prove_detached_cmux(cmux, &spec)?;
    }
    let replaced: Vec<Value> = live
        .iter()
        .map(|registration| {
            json!({
                "token": registration.token,
                "pid": registration.pid,
                "mode": registration.mode.map(SupervisorMode::as_str),
                "workspace_id": registration.workspace_id,
                "version": registration.binary_version,
            })
        })
        .collect();
    let agent = launchd.uninstall(&location.label, &location.launch_agent)?;
    for registration in live {
        if registration.mode == Some(SupervisorMode::InCmux) {
            processes.interrupt(registration.pid)?;
        } else if (!agent.loaded || agent.pid.is_some()) && Some(registration.pid) != agent.pid {
            // A second SIGTERM would end a draining supervisor at once, so
            // the one bootout delivered is never repeated here.
            processes.terminate(registration.pid)?;
        }
    }
    // `--no-wait` promised not to sit through a drain. The runs were the
    // reason a drain is long, and there were none, but a supervisor can
    // still fail to stop (a loop wedged on a hung cmux or git call keeps
    // its row while its heartbeat thread runs on, and a run claimed
    // between that check and the signal is a session again), so the wait
    // is bounded there instead of unbounded.
    let deadline = options
        .no_wait
        .then(|| Instant::now() + options.startup_timeout);
    loop {
        let remaining: Vec<String> = queue
            .supervisors()?
            .into_iter()
            .filter(|registration| {
                live.iter().any(|l| l.token == registration.token)
                    && processes.alive(registration.pid)
            })
            .map(|registration| format!("{} (pid {})", registration.token, registration.pid))
            .collect();
        if remaining.is_empty() {
            break;
        }
        if let Some(deadline) = deadline {
            ensure!(
                Instant::now() < deadline,
                "--no-wait: the supervisor did not stop within {}s of the signal; still \
registered: {}. It has been asked to drain and its LaunchAgent is unloaded, so run `up` again \
once `status` shows it gone",
                options.startup_timeout.as_secs(),
                remaining.join(", "),
            );
        }
        thread::sleep(options.poll);
    }
    // The drain can also end because the process died with its row intact
    // (launchd's `ExitTimeOut` SIGKILL, or the heartbeat failure that keeps
    // the row on purpose because the database may be unreachable). Those
    // rows go the way `down --force` drops them, so none is left pointing
    // at the workspace closed just below.
    let surviving: Vec<LeaseToken> = queue
        .supervisors()?
        .into_iter()
        .map(|registration| registration.token)
        .collect();
    for registration in live {
        if surviving.contains(&registration.token) {
            prune_supervisor(queue, registration)?;
        }
    }
    // The drain is over, so every workspace of a replaced supervisor is
    // ours to close; one left open would stop the next in-cmux supervisor
    // from opening its own.
    let closed = close_supervisor_workspaces(queue, cmux, live, live, Stop::SeenThrough);
    // Whatever survived the drain (an alive-but-silent supervisor `up`
    // neither reuses nor kills) belongs to another process, not to the one
    // started below.
    let existing: HashSet<LeaseToken> = queue
        .supervisors()?
        .into_iter()
        .map(|registration| registration.token)
        .collect();
    let mut started = start_supervisor(up, &existing, !options.in_cmux)?;
    let object = started
        .as_object_mut()
        .expect("a started supervisor is a JSON object");
    object.insert("outcome".into(), json!("restarted"));
    object.insert("previous_version".into(), json!(previous_version));
    object.insert("replaced".into(), json!(replaced));
    object.insert("supervisor_workspaces".into(), json!(closed));
    Ok(started)
}

/// Why `up --no-wait` refuses to drain the supervisors it replaces: the
/// runs they still lease (`held`, id and status), whatever stage each is
/// at. `None` when they lease none. A run nobody leases (its history keeps
/// the `supervisor_token` of whoever drove it last) or one another process
/// leases (a supervisor `up` does not replace, a `dagq integrate` by hand)
/// is not waited for by the drain, so it does not refuse it either.
fn no_wait_refusal(previous_version: Option<&str>, held: &[(String, RunStatus)]) -> Option<String> {
    if held.is_empty() {
        return None;
    }
    Some(format!(
        "refusing to replace the supervisor of version {} with {VERSION} without waiting: {} \
run(s) are still in flight on the supervisors being replaced ({}); run `up` without --no-wait \
to drain them, or wait for them to finish",
        previous_version.unwrap_or("(unrecorded)"),
        held.len(),
        held.iter()
            .map(|(id, status)| format!("{id} {}", status.as_str()))
            .collect::<Vec<_>>()
            .join(", "),
    ))
}

/// Whether every live supervisor can be handed over to this binary rather
/// than drained: each one takes a handoff (a supervisor of a binary before
/// ADR-0045 does not), and a launchd one would keep running this binary
/// after its next restart too — its agent starts this very path. An exec
/// does not change the agent's `ProgramArguments`, so a supervisor whose
/// agent names another binary is drained and started again by `up`, which
/// rewrites the agent (ADR-0045 decision 12).
fn takes_handoff(up: &Up, files: &dyn RunFiles, live: &[SupervisorRegistration]) -> bool {
    if !live
        .iter()
        .all(|registration| registration.handoff_accepted)
    {
        return false;
    }
    if live
        .iter()
        .all(|registration| registration.mode != Some(SupervisorMode::Launchd))
    {
        return true;
    }
    let Ok(current) = path_text(&up.environment.current_exe) else {
        return false;
    };
    files
        .read_to_string(&up.location.launch_agent)
        .is_ok_and(|plist| {
            plist.contains(&format!(
                "<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{}</string>",
                escape(&current)
            ))
        })
}

/// `up`'s replacement without a drain (ADR-0045 decision 15): every live
/// supervisor is asked to exec this binary, and `up` reports once each of
/// them is back under this build with its pid and token. `up` replaces no
/// file, so nothing is put back when one fails: it waits for every one of
/// them (ADR-t632-1) and returns, beside the report, the error naming the
/// ones that did not take it, for `up` to fail with once it has done the
/// rest.
fn hand_off_supervisors(
    up: &Up,
    live: &[SupervisorRegistration],
) -> Result<(Value, Option<String>)> {
    let handed = hand_off(
        up.queue,
        up.processes,
        up.clock,
        live,
        &up.environment.current_exe,
        VERSION,
        up.options.handoff_timeout,
        up.options.poll,
    )?;
    let took = handed.iter().filter(|h| h.error.is_none()).count();
    let failure = handoff_failures(&handed).map(|error| {
        format!(
            "{:#}",
            error.context(format!(
                "{took} of the {} supervisors took the handoff to {VERSION}; the ones that did \
not go on with the binary they had: `down --force` and `up` start them with this one",
                handed.len()
            ))
        )
    });
    let outcome = match (&failure, took) {
        (None, _) => "restarted",
        (Some(_), 0) => "handoff_failed",
        (Some(_), _) => "partially_handed_off",
    };
    let first = &handed[0];
    let previous_version = live
        .iter()
        .find(|registration| registration.binary_version.as_deref() != Some(VERSION))
        .and_then(|registration| registration.binary_version.clone());
    let report = json!({
        "outcome": outcome,
        "handoff": true,
        "mode": first.registration.mode.map(SupervisorMode::as_str),
        "version": VERSION,
        "previous_version": previous_version,
        "pid": first.registration.pid,
        "token": first.now.as_ref().unwrap_or(&first.registration.token),
        "workspace_id": first.registration.workspace_id,
        "plist": up.location.launch_agent,
        "log_dir": up.location.log_dir,
        "replaced": handed.iter().map(Handed::report).collect::<Vec<_>>(),
        "supervisor_workspaces": [],
    });
    Ok((report, failure))
}

/// What became of one supervisor asked to exec a binary: the registration
/// it had, and either the token it serves under now or why it did not take
/// the handoff.
#[derive(Debug, Clone)]
pub struct Handed {
    pub registration: SupervisorRegistration,
    /// The token it took back under the new build: its own, or the one its
    /// pid registered under again.
    pub now: Option<LeaseToken>,
    pub error: Option<String>,
    /// It did not take the handoff because it drains for a stop request
    /// (its `supervisor_draining`, task 1277): a stop wins over the handoff.
    pub stopping: bool,
}

impl Handed {
    /// The supervisor as the reports show it: `token` is the one it serves
    /// under now (`previous_token` the one it was asked under) when it took
    /// the handoff, and the one it had with the `error` when it did not.
    pub fn report(&self) -> Value {
        let registration = &self.registration;
        let mut value = json!({
            "token": self.now.as_ref().unwrap_or(&registration.token),
            "pid": registration.pid,
            "mode": registration.mode.map(SupervisorMode::as_str),
            "workspace_id": registration.workspace_id,
            "version": registration.binary_version,
        });
        match &self.error {
            Some(error) => value["error"] = json!(error),
            None => value["previous_token"] = json!(registration.token),
        }
        if self.stopping {
            value["stopping"] = json!(true);
        }
        value
    }
}

/// The failures of a handoff as one error naming each supervisor that did
/// not take it; `None` when every one did.
pub fn handoff_failures(handed: &[Handed]) -> Option<anyhow::Error> {
    let errors: Vec<&str> = handed.iter().filter_map(|h| h.error.as_deref()).collect();
    (!errors.is_empty()).then(|| anyhow::anyhow!("{}", errors.join("; ")))
}

/// The longest `hand_off` looks again at a supervisor that took its
/// request just as the wait ran out (the withdrawal found no request left,
/// as its new binary clears it by registering again): one whose row is
/// gone gets this much more, at most the wait itself, for its pid to
/// register again under the new build.
pub const HANDOFF_GRACE: Duration = Duration::from_secs(30);

/// Ask each supervisor in `live` to exec `binary` (ADR-0045 decision 10)
/// and wait until each of them has either taken its registration back
/// under `version`, with the same pid, or failed to, within one `timeout`
/// for them all. A supervisor fails when it cannot take a handoff, stops
/// heartbeating before it did (an exec'd binary that failed to start),
/// deregistered without its pid registering again under `version` (which
/// takes its place, under its new token), came back under another build (an
/// exec that failed, after which the old binary goes on) or is still asked
/// at the timeout. One failing does not stop the wait for the others
/// (ADR-t632-1): the result names what became of each, and the request of
/// each one that failed is withdrawn. One still asked at the timeout whose
/// request is gone by the withdrawal took it just then: it is looked at
/// again for up to [`HANDOFF_GRACE`] (at most `timeout`), and takes the
/// handoff when it registers again under `version` meanwhile. An error is
/// only a queue that could not be read or written.
#[allow(clippy::too_many_arguments)]
pub fn hand_off(
    queue: &dyn Queue,
    processes: &dyn ProcessControl,
    clock: &dyn Clock,
    live: &[SupervisorRegistration],
    binary: &Path,
    version: &str,
    timeout: Duration,
    poll: Duration,
) -> Result<Vec<Handed>> {
    let binary_text = path_text(binary)?;
    // A registration of the same pid counts as a successor only when it was
    // made after the handoff was asked for: an older one is a stale row of
    // a pid the system reused.
    let asked_at = clock.now();
    let waited = match wait_for_handoff(
        queue,
        processes,
        clock,
        live,
        asked_at,
        &binary_text,
        version,
        timeout,
        poll,
    ) {
        Ok(waited) => waited,
        Err(error) => {
            for registration in live {
                let _ = queue.cancel_handoff(&registration.token, &binary_text);
            }
            return Err(error);
        }
    };
    let mut handed = Vec::with_capacity(waited.len());
    let mut taken_late = Vec::new();
    for (index, (one, timed_out)) in waited.into_iter().enumerate() {
        if one.error.is_some() {
            // A request left behind would have the supervisor exec that path
            // later, after the caller put another binary there.
            let withdrawn = queue.cancel_handoff(&one.registration.token, &binary_text);
            // Nothing left to withdraw from one still asked a moment ago:
            // its new binary cleared the request by registering again (or
            // its row went away while it exec'd), so it took the handoff
            // just then.
            if timed_out && matches!(withdrawn, Ok(false)) {
                taken_late.push(index);
            }
        }
        handed.push(one);
    }
    look_again(
        queue,
        processes,
        clock,
        &mut handed,
        taken_late,
        asked_at,
        &binary_text,
        version,
        HANDOFF_GRACE.min(timeout),
        poll,
    )?;
    Ok(handed)
}

/// What became of each supervisor in `live`, with whether its failure is
/// the timeout (it was still asked when the wait ran out).
#[allow(clippy::too_many_arguments)]
fn wait_for_handoff(
    queue: &dyn Queue,
    processes: &dyn ProcessControl,
    clock: &dyn Clock,
    live: &[SupervisorRegistration],
    asked_at: i64,
    binary_text: &str,
    version: &str,
    timeout: Duration,
    poll: Duration,
) -> Result<Vec<(Handed, bool)>> {
    let mut handed = Vec::with_capacity(live.len());
    for registration in live {
        let error = (!queue.request_handoff(&registration.token, binary_text)?).then(|| {
            format!(
                "supervisor {} (pid {}) cannot take a handoff; stop it with `down --wait` and \
run `up`",
                registration.token, registration.pid
            )
        });
        handed.push((
            Handed {
                registration: registration.clone(),
                now: None,
                error,
                stopping: false,
            },
            false,
        ));
    }
    let deadline = Instant::now() + timeout;
    loop {
        let stops = Stops::around(queue, || {
            let now = clock.now();
            Ok((now, queue.supervisors()?))
        })?;
        let (now, registrations) = (stops.now, &stops.registrations);
        let expired = Instant::now() >= deadline;
        for (handed, timed_out) in handed
            .iter_mut()
            .filter(|(h, _)| h.now.is_none() && h.error.is_none())
        {
            match look_at_handoff(
                registrations,
                &handed.registration,
                processes,
                now,
                asked_at,
                binary_text,
                version,
            ) {
                Ok(Some(token)) => handed.now = Some(token),
                // A stop wins over the handoff, also when its drain ended
                // (deregistered) before this look.
                Ok(None) if stops.before.contains(&handed.registration.token) => {
                    stopped_instead(handed, binary_text);
                }
                Err(_) if stops.after.contains(&handed.registration.token) => {
                    stopped_instead(handed, binary_text);
                }
                Ok(None) if expired => {
                    handed.error = Some(format!(
                        "supervisor {} (pid {}) did not take the handoff to {binary_text} within \
{}s; it still finishes its validation or landing in progress, so check `status` again",
                        handed.registration.token,
                        handed.registration.pid,
                        timeout.as_secs()
                    ));
                    *timed_out = true;
                }
                Ok(None) => {}
                Err(error) => handed.error = Some(format!("{error:#}")),
            }
        }
        if handed
            .iter()
            .all(|(h, _)| h.now.is_some() || h.error.is_some())
        {
            return Ok(handed);
        }
        thread::sleep(poll);
    }
}

/// How many of the latest `supervisor_draining` events the handoff reads
/// each look: more than the supervisors of one queue ever drain at once.
const DRAINING_LOOKED_AT: usize = 64;

/// The tokens of the supervisors that recorded a stop request
/// (`supervisor_draining`, task 1277). A stop wins over a handoff and a
/// draining supervisor ends without exec'ing, so one that recorded it,
/// before or after it was asked, never takes the handoff.
fn stopping_supervisors(queue: &dyn Queue) -> Result<HashSet<LeaseToken>> {
    Ok(queue
        .latest_events_of(EventKind::SupervisorDraining.as_str(), DRAINING_LOOKED_AT)?
        .iter()
        .filter_map(|event| event.payload["supervisor"].as_str())
        .map(LeaseToken::new)
        .collect())
}

/// One look's registrations, read between two reads of the stop requests
/// (task 1277). A supervisor still asked in the registrations is stopping
/// only by the stops read `before` them: one that registered again under
/// the new build and was stopped after that read took the handoff. One
/// that failed (deregistered, no longer heartbeating) is stopping by the
/// stops read `after` them too: a supervisor records its stop before its
/// drain ends and it deregisters, so one gone from the registrations
/// after a stop the first read missed shows it in the second.
struct Stops {
    before: HashSet<LeaseToken>,
    now: i64,
    registrations: Vec<SupervisorRegistration>,
    after: HashSet<LeaseToken>,
}

impl Stops {
    fn around(
        queue: &dyn Queue,
        read: impl FnOnce() -> Result<(i64, Vec<SupervisorRegistration>)>,
    ) -> Result<Self> {
        let before = stopping_supervisors(queue)?;
        let (now, registrations) = read()?;
        let after = stopping_supervisors(queue)?;
        Ok(Self {
            before,
            now,
            registrations,
            after,
        })
    }
}

/// Fail `handed`'s handoff for the stop request its supervisor drains for.
fn stopped_instead(handed: &mut Handed, binary_text: &str) {
    handed.error = Some(format!(
        "supervisor {} (pid {}) is stopping (a stop request wins over the handoff) and was not \
handed off to {binary_text}; it drains its runs in progress under its old binary and \
deregisters, then `up` starts it with the binary in place",
        handed.registration.token, handed.registration.pid
    ));
    handed.stopping = true;
}

/// Look again, for up to `grace`, at the supervisors of `handed` at
/// `pending` that took their request just as the wait ran out: one that
/// registers again under `version` (its token, or its pid's successor)
/// took the handoff; one back under another build or no longer
/// heartbeating, or not back by then, did not, and its error says it took
/// the request.
#[allow(clippy::too_many_arguments)]
fn look_again(
    queue: &dyn Queue,
    processes: &dyn ProcessControl,
    clock: &dyn Clock,
    handed: &mut [Handed],
    mut pending: Vec<usize>,
    asked_at: i64,
    binary_text: &str,
    version: &str,
    grace: Duration,
    poll: Duration,
) -> Result<()> {
    let deadline = Instant::now() + grace;
    while !pending.is_empty() {
        let stops = Stops::around(queue, || {
            let now = clock.now();
            Ok((now, queue.supervisors()?))
        })?;
        let (now, registrations) = (stops.now, &stops.registrations);
        let expired = Instant::now() >= deadline;
        pending.retain(|&index| {
            let one = &mut handed[index];
            let registration = &one.registration;
            let name = format!(
                "supervisor {} (pid {})",
                registration.token, registration.pid
            );
            // While it execs, its row can be gone before its pid registers
            // again: only a row it serves under settles it early.
            let found = successor(
                registrations,
                &registration.token,
                registration.pid,
                version,
                asked_at,
            )
            .is_some();
            let error = match look_at_handoff(
                registrations,
                registration,
                processes,
                now,
                asked_at,
                binary_text,
                version,
            ) {
                Ok(Some(token)) => {
                    one.now = Some(token);
                    one.error = None;
                    return false;
                }
                // A stop wins over the handoff: it drains under its old
                // binary and will not come back under this one.
                Ok(None) if stops.before.contains(&registration.token) => {
                    stopped_instead(one, binary_text);
                    return false;
                }
                Err(_) if stops.after.contains(&registration.token) => {
                    stopped_instead(one, binary_text);
                    return false;
                }
                Err(error) if found => format!("{error:#}"),
                _ if expired => format!(
                    "not back {}s after the wait ran out; see its log, then `status`",
                    grace.as_secs_f64()
                ),
                _ => return true,
            };
            one.error = Some(format!(
                "{name} took the handoff to {binary_text} just as the wait ran out, but did not \
come back under {version}: {error}"
            ));
            false
        });
        if !pending.is_empty() {
            thread::sleep(poll);
        }
    }
    Ok(())
}

/// The registration a supervisor asked to hand off serves under: the one
/// of its `token`, or, once that is gone, the one its `pid` made again under
/// `version` since `asked_at` (the latest such when there are several). A
/// row of that pid and build started before the handoff was asked for is a
/// stale one of a reused pid, not the successor.
pub(crate) fn successor<'a>(
    registrations: &'a [SupervisorRegistration],
    token: &LeaseToken,
    pid: u32,
    version: &str,
    asked_at: i64,
) -> Option<&'a SupervisorRegistration> {
    registrations
        .iter()
        .find(|r| r.token == *token)
        .or_else(|| {
            registrations
                .iter()
                .filter(|r| {
                    r.pid == pid
                        && r.binary_version.as_deref() == Some(version)
                        && r.started_at >= asked_at
                })
                .max_by_key(|r| r.started_at)
        })
}

/// One look at `registration`'s handoff: the token it serves under once it
/// took it, `None` while it is still asked, an error when it cannot take
/// it any more.
fn look_at_handoff(
    registrations: &[SupervisorRegistration],
    registration: &SupervisorRegistration,
    processes: &dyn ProcessControl,
    now: i64,
    asked_at: i64,
    binary_text: &str,
    version: &str,
) -> Result<Option<LeaseToken>> {
    let name = format!(
        "supervisor {} (pid {})",
        registration.token, registration.pid
    );
    // A token that deregistered is taken back by the same pid registering
    // again under the new build.
    let Some(current) = successor(
        registrations,
        &registration.token,
        registration.pid,
        version,
        asked_at,
    ) else {
        bail!("{name} deregistered instead of taking the handoff to {binary_text}");
    };
    if current.handoff_binary.is_some() {
        ensure!(
            fresh(current, processes, now),
            "{name} stopped heartbeating before it took the handoff to {binary_text}; see its log, \
then `down --force` and `up`"
        );
        return Ok(None);
    }
    // Taken just before its exec (task 824): `resume_registration` accepts
    // handoffs again once the exec'd binary, or the old one after a failed
    // exec, has the registration back. A row of its pid under a new token
    // is a registration after the exec, whatever it accepts yet.
    if current.token == registration.token && !current.handoff_accepted {
        ensure!(
            fresh(current, processes, now),
            "{name} took the handoff to {binary_text} but stopped before it registered again; see \
its log, then `down --force` and `up`"
        );
        return Ok(None);
    }
    ensure!(
        current.pid == registration.pid
            && current.binary_version.as_deref() == Some(version)
            && fresh(current, processes, now),
        "{name} came back as {} (pid {}) instead of {version}: the exec of {binary_text} failed \
and it goes on with its binary; see its log",
        current.binary_version.as_deref().unwrap_or("(unrecorded)"),
        current.pid
    );
    Ok(Some(current.token.clone()))
}

/// Refuse, before anything is stopped, when the queue's recorded
/// supervisor workspace is still open and this replacement will not close
/// it. `up` prunes a dead registration without closing its workspace and
/// cmux keeps a workspace open after its command exits, so the workspace of
/// a crashed supervisor that is no longer registered at all can still be
/// open. Finding that only after the drain would cost a working supervisor
/// and leave the queue with nothing serving it.
fn ensure_supervisor_workspace_free(
    queue: &dyn Queue,
    cmux: &dyn WorkspaceBackend,
    live: &[SupervisorRegistration],
) -> Result<()> {
    let Some(id) = recorded_workspace(queue, cmux, SessionRole::Supervisor)? else {
        return Ok(());
    };
    ensure!(
        live.iter().any(|registration| {
            registration.mode == Some(SupervisorMode::InCmux)
                && registration.workspace_id.as_deref() == Some(id.as_str())
        }),
        "cmux workspace {id}, recorded as this queue's supervisor workspace, is still open but \
belongs to no supervisor this `up` would drain, so the replacement could not open its own; read \
its screen, then close it (`cmux workspace close {id}`) and run `up --in-cmux` again"
    );
    Ok(())
}

/// Ask cmux whether it admits a process carrying the agent's environment
/// from outside its process tree, which is how the launchd-started
/// supervisor will connect. Kept apart from the start so a replacement can
/// settle the question before it drains a supervisor that works.
fn prove_detached_cmux(cmux: &dyn WorkspaceBackend, spec: &LaunchAgentSpec) -> Result<()> {
    cmux.preflight_detached(&spec.environment).map_err(|error| {
        // Only a refusal has the password (or `--in-cmux`) as its remedy; a
        // ping that could not be run or did not answer is its own error.
        if error.is::<DetachedRefusal>() {
            error.context(DETACHED_CMUX_HINT)
        } else {
            error.context(
                "cmux could not be asked whether it admits a connection from outside its own terminals",
            )
        }
    })
}

/// Write and load the LaunchAgent, after proving that cmux admits a
/// process carrying its environment from outside cmux's process tree. A
/// refusal stops `up` before launchd ever sees the agent, because
/// `KeepAlive` would otherwise restart a supervisor that cannot work.
fn start_under_launchd(
    up: &Up,
    existing: &HashSet<LeaseToken>,
    detached_proven: bool,
) -> Result<Value> {
    let Up {
        location,
        queue,
        launchd,
        options,
        ..
    } = *up;
    let spec = launch_agent_spec(
        location,
        up.db,
        &up.repository.root,
        up.environment,
        options,
    )?;
    if !detached_proven {
        prove_detached_cmux(up.workspaces.cmux, &spec)?;
    }
    launchd.install(&spec.label, &spec.plist, &spec.xml())?;
    let registration = wait_for_registration(up, existing).with_context(|| {
        format!(
            "supervisor did not register within {}s; see {}",
            options.startup_timeout.as_secs(),
            spec.log
        )
    })?;
    queue.set_supervisor_mode(&registration.token, SupervisorMode::Launchd, None)?;
    Ok(json!({
        "outcome": "started",
        "mode": SupervisorMode::Launchd.as_str(),
        "version": VERSION,
        "pid": registration.pid,
        "token": registration.token,
        "workspace_id": Value::Null,
        "plist": spec.plist,
        "log_dir": location.log_dir,
    }))
}

/// Run `supervise` inside its own cmux workspace instead: nothing about
/// launchd is touched, and the supervisor is a child of a cmux terminal, so
/// no socket password is needed. Nothing restarts it either.
///
/// cmux keeps a workspace open after its command exits, so a leftover
/// `[<repo>]supervisor` may belong to a supervisor that crashed, or to
/// one that is alive but no longer heartbeating (which `up` never reuses
/// and never kills). Either way it is a person's to close, and `up`
/// stops rather than open a second one or interfere with the first.
fn start_in_cmux(up: &Up, existing: &HashSet<LeaseToken>) -> Result<Value> {
    let Up {
        location,
        repository,
        queue,
        workspaces,
        options,
        ..
    } = *up;
    let cmux = workspaces.cmux;
    let name = supervisor_workspace_name(&repository.root);
    if let Some(id) = recorded_workspace(queue, cmux, SessionRole::Supervisor)? {
        bail!(
            "cmux workspace {id}, recorded as this queue's supervisor workspace, is still open \
but no supervisor of this queue is registered and heartbeating; read its screen, then close it \
(`cmux workspace close {id}`) and run `up --in-cmux` again"
        );
    }
    let command = supervise_command(location, up.db, up.environment, options)?;
    let workspace_id = cmux.create_named(
        &name,
        &repository.root,
        &command,
        &workspaces.tags(SessionRole::Supervisor)?,
    )?;
    // Recorded before the wait, so a supervisor that never registers still
    // leaves its workspace where the next `up` finds it.
    queue.register_session_workspace(SessionRole::Supervisor, &workspace_id)?;
    let registration = wait_for_registration(up, existing).with_context(|| {
        format!(
            "supervisor did not register within {}s; read workspace {workspace_id} ({name:?}) \
and close it before trying again",
            options.startup_timeout.as_secs(),
        )
    })?;
    queue
        .set_supervisor_mode(
            &registration.token,
            SupervisorMode::InCmux,
            Some(&workspace_id),
        )
        .with_context(|| {
            format!("supervisor started in workspace {workspace_id} ({name:?}); close it by hand")
        })?;
    Ok(json!({
        "outcome": "started",
        "mode": SupervisorMode::InCmux.as_str(),
        "version": VERSION,
        "pid": registration.pid,
        "token": registration.token,
        "workspace_id": workspace_id,
        "name": name,
        "plist": Value::Null,
        "log_dir": location.log_dir,
    }))
}

/// The supervisor workspace's command: this binary running `supervise` on
/// this queue, with the same arguments the LaunchAgent would carry. cmux
/// types it into a login shell, so every argument is quoted on its own. The
/// environment is the cmux terminal's, so nothing is set here.
pub fn supervise_command(
    location: &QueuePaths,
    db: &Path,
    environment: &UpEnvironment,
    options: &UpOptions,
) -> Result<String> {
    Ok(shell_join(&supervise_arguments(
        location,
        db,
        environment,
        options,
        SupervisorMode::InCmux,
    )?))
}

/// `<this binary> --db <db> supervise [--parallel N] --log-dir <queue logs>
/// --cmux <resolved> --claude <resolved> --codex <resolved or given> --mode
/// <mode> [--plugin-dir <dir>]`:
/// what keeps a supervisor of this queue going, in either mode. The
/// executables are absolute so neither launchd's PATH nor the terminal's
/// decides which ones run; the plugin directory is what the planners the
/// runtime opens load. `--mode` is what its start mark records (ADR-0051
/// decision 10): `up` writes the registration's mode only after it sees the
/// registration.
fn supervise_arguments(
    location: &QueuePaths,
    db: &Path,
    environment: &UpEnvironment,
    options: &UpOptions,
    mode: SupervisorMode,
) -> Result<Vec<String>> {
    let mut arguments = vec![
        path_text(&environment.current_exe)?,
        "--db".into(),
        path_text(db)?,
        "supervise".into(),
        "--log-dir".into(),
        path_text(&location.log_dir)?,
        "--cmux".into(),
        path_text(&options.cmux)?,
        "--claude".into(),
        path_text(&options.claude)?,
        "--codex".into(),
        path_text(&options.codex)?,
        "--mode".into(),
        mode.as_str().into(),
    ];
    if options.no_claude {
        arguments.push("--no-claude".into());
    }
    if let Some(dir) = &options.plugin_dir {
        arguments.push("--plugin-dir".into());
        arguments.push(path_text(
            &dir.canonicalize().unwrap_or_else(|_| dir.clone()),
        )?);
    }
    // Only a value given is passed, so one not given follows `[supervisor]`
    // of `dagq.toml` rather than a default baked in (task 698).
    if let Some(parallel) = options.parallel {
        arguments.push("--parallel".into());
        arguments.push(parallel.to_string());
    }
    if let Some(max_waiting) = options.max_waiting {
        arguments.push("--max-waiting".into());
        arguments.push(max_waiting.to_string());
    }
    if let Some(runtime_planners) = options.runtime_planners {
        arguments.push("--runtime-planners".into());
        arguments.push(runtime_planners.to_string());
    }
    // 0 or below disables the hold; it is passed as 0, since a negative
    // value would read as a flag.
    if let Some(max_load) = options.max_load {
        arguments.push("--max-load".into());
        arguments.push(max_load.max(0.0).to_string());
    }
    if options.auto_update {
        arguments.push("--auto-update".into());
    }
    Ok(arguments)
}

/// Why `--auto-update` is refused in `checkout`, a repository that is not
/// dagq's source (ADR-t614-1), and how to update dagq there instead.
pub fn auto_update_refused(checkout: &Path) -> String {
    format!(
        "--auto-update builds dagq from the repository's sources, and {} is not dagq's source \
(its Cargo.toml has no [package] named {}); leave --auto-update off and update dagq with \
`cargo install dagq`, or with `dagq install --from <binary or checkout>`",
        checkout.display(),
        crate::domain::source_repository::PACKAGE
    )
}

/// Remove the registration of a supervisor that did not remove its own
/// (dead, killed, or gone silent), and record its stop (ADR-0051 decision
/// 10) with the row's last heartbeat, which `kpi` ends its life at: the
/// process could not record its own, and the row that kept the heartbeat
/// is gone after this. Both are one transaction, so a failed record keeps
/// the row for the next `up` or `down` to prune again. A row the process
/// removed meanwhile (it recorded its own stop) is not recorded again. The
/// heartbeat is read from the row as it is removed, as `registration` may
/// be from before a drain.
fn prune_supervisor(queue: &dyn Queue, registration: &SupervisorRegistration) -> Result<()> {
    queue.prune_supervisor(
        &registration.token,
        EventKind::SupervisorStopped,
        &|removed| {
            json!({
                "supervisor": removed.token,
                "dagq_version": removed.binary_version,
                "outcome": "pruned",
                "last_heartbeat_at": removed.heartbeat_at,
            })
        },
    )?;
    Ok(())
}

/// How much later than its registration's `started_at` a process at the
/// registered pid may seem to have started and still be the one that
/// registered: `ps` prints the age to the second.
const PID_START_SLACK_SECS: i64 = 5;

/// Whether the pid of a registration that stopped heartbeating now belongs
/// to another process (task 330): the supervisor died without removing its
/// row and the system gave its pid to a process of another user, or to one
/// of this user's that started after the registration, which the process
/// that registered cannot be (an exec keeps the start). Such a row is as
/// dead as one whose pid runs nothing, while an alive-but-silent
/// supervisor, which started before its registration, is kept. `listing`
/// caches this user's processes across registrations; when they cannot be
/// listed, nothing is taken over.
fn pid_taken_over(
    registration: &SupervisorRegistration,
    processes: &dyn ProcessControl,
    now: i64,
    listing: &mut Option<Option<Vec<ProcessInfo>>>,
) -> bool {
    if now - registration.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS {
        return false;
    }
    let Some(listed) = listing.get_or_insert_with(|| processes.list().ok()) else {
        return false;
    };
    match listed
        .iter()
        .find(|process| process.pid == registration.pid)
    {
        // Alive, yet not among this user's processes, which every
        // supervisor of this queue is.
        None => true,
        Some(process) => {
            let started = now - i64::try_from(process.elapsed_secs).unwrap_or(i64::MAX);
            started > registration.started_at + PID_START_SLACK_SECS
        }
    }
}

fn fresh(registration: &SupervisorRegistration, processes: &dyn ProcessControl, now: i64) -> bool {
    processes.alive(registration.pid) && now - registration.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS
}

/// Write `up`'s `--auto-update` (or its absence) on the registrations of
/// the supervisors it leaves serving the queue (ADR-0045 decision 17): the
/// live ones it reused, the ones it handed over (under the token each
/// serves under now, which is a new one when its pid registered again, and
/// also the ones that did not take the handoff and go on under their old
/// token while they are still registered), or the one it started (which
/// registered with it already, from `supervise --auto-update`). Reported
/// as the supervisor's `auto_update`.
fn set_auto_update(
    queue: &dyn Queue,
    mut supervisor: Value,
    live: &[SupervisorRegistration],
    enabled: bool,
) -> Result<Value> {
    let tokens: Vec<LeaseToken> = if supervisor["handoff"] == true {
        let registered: HashSet<LeaseToken> = queue
            .supervisors()?
            .into_iter()
            .map(|registration| registration.token)
            .collect();
        supervisor["replaced"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|handed| handed["token"].as_str().map(LeaseToken::new))
            .filter(|token| registered.contains(token))
            .collect()
    } else if supervisor["outcome"] == "reused" {
        live.iter()
            .map(|registration| registration.token.clone())
            .collect()
    } else {
        supervisor["token"]
            .as_str()
            .map(LeaseToken::new)
            .into_iter()
            .collect()
    };
    for token in &tokens {
        queue.set_auto_update(token, enabled)?;
    }
    supervisor["auto_update"] = json!(enabled);
    Ok(supervisor)
}

/// The registration the supervisor `up` has just started writes for
/// itself. Tokens in `existing` were already registered when `up` looked,
/// so they belong to another process: one of them may start heartbeating
/// again mid-wait (an alive but silent supervisor `up` neither reuses nor
/// kills), and taking it for ours would stamp this start's mode and
/// workspace onto a supervisor that never ran in it.
fn wait_for_registration(
    up: &Up,
    existing: &HashSet<LeaseToken>,
) -> Result<SupervisorRegistration> {
    let Up {
        queue,
        processes,
        options,
        ..
    } = *up;
    let deadline = Instant::now() + options.startup_timeout;
    loop {
        let now = up.clock.now();
        if let Some(registration) = queue.supervisors()?.into_iter().find(|registration| {
            !existing.contains(&registration.token) && fresh(registration, processes, now)
        }) {
            return Ok(registration);
        }
        ensure!(Instant::now() < deadline, "no live supervisor registration");
        thread::sleep(options.poll);
    }
}

/// The agent definition: this binary running `supervise` on this queue from
/// the repository root, with the caller's PATH (and exported socket
/// password) and the queue's log directory.
pub fn launch_agent_spec(
    location: &QueuePaths,
    db: &Path,
    repo_root: &Path,
    environment: &UpEnvironment,
    options: &UpOptions,
) -> Result<LaunchAgentSpec> {
    Ok(LaunchAgentSpec {
        label: location.label.clone(),
        plist: location.launch_agent.clone(),
        program_arguments: supervise_arguments(
            location,
            db,
            environment,
            options,
            SupervisorMode::Launchd,
        )?,
        working_directory: path_text(repo_root)?,
        environment: SupervisorEnvironment {
            path: environment.path.clone(),
            socket_password: environment.socket_password.clone(),
            config_home: environment.config_home.clone(),
        },
        log: path_text(&location.log_dir.join(LAUNCHD_LOG_NAME))?,
    })
}

/// The inbox's first message: `inbox_prompt` with the instruction of
/// `language` (ADR-t616-2). The role and queue are the workspace's own
/// `--env` (ADR-0026), not a prefix of its command, so a `claude` started
/// again in that workspace still has them.
pub fn inbox_session_prompt(db: &Path, language: Option<&Language>) -> Result<String> {
    Ok(with_instruction(inbox_prompt(db)?, language))
}

/// What a person looks at first after `up`: unfinished runs with whether their
/// lease still has a live, heartbeating owner, and the runs that wait for
/// review (`awaiting_integration`) or a resumed session (`needs_session`).
fn open_work(
    queue: &dyn Queue,
    processes: &dyn ProcessControl,
    clock: &dyn Clock,
) -> Result<Value> {
    let now = clock.now();
    let leases = queue.run_leases()?;
    let unfinished: Vec<Value> = queue
        .active_runs()?
        .into_iter()
        .map(|run| {
            let lease_stale = leases.iter().find(|l| l.run_id == *run.id()).map(|lease| {
                !processes.alive(lease.pid) || now - lease.heartbeat_at > HEARTBEAT_TIMEOUT_SECS
            });
            json!({
                "run_id": run.id(),
                "task_id": run.task_id(),
                "status": run.status(),
                "lease_stale": lease_stale,
            })
        })
        .collect();
    let brief = |status: RunStatus| -> Result<Vec<Value>> {
        Ok(queue
            .runs_with_status(status)?
            .into_iter()
            .map(|run| {
                json!({"run_id": run.id(), "task_id": run.task_id(), "last_error": run.last_error()})
            })
            .collect())
    };
    Ok(json!({
        "unfinished_runs": unfinished,
        "awaiting_integration": brief(RunStatus::AwaitingIntegration)?,
        "needs_session": brief(RunStatus::NeedsSession)?,
    }))
}

#[derive(Debug, Clone)]
pub struct DownOptions {
    /// Block until the supervisor's registration is gone or its process died.
    pub wait: bool,
    /// SIGKILL after the unload and drop the registration rows.
    pub force: bool,
    pub poll: Duration,
}

/// Stop the queue's supervisors, whichever mode started them. The launchd
/// agent is always unloaded (that is what makes a `launchd` supervisor stop
/// without being restarted, and it also clears an agent left over from a
/// queue that has since moved to `--in-cmux`), which delivers SIGTERM to
/// the agent's own process; a supervisor started by hand gets the SIGTERM
/// from here, and an `in_cmux` one gets a SIGINT, the signal its terminal
/// would send. The runtime drains on either.
///
/// The cmux workspace of an `in_cmux` supervisor is closed once that
/// supervisor's process is gone: after the drain under `--wait`, after the
/// kill under `--force`, or straight away when it had already exited.
/// Closing it earlier would cut the drain short, so the default (which
/// returns while the supervisor drains) leaves it open and says so; a
/// person closes it or runs `down --wait`. The inbox and planner
/// workspaces are never touched.
pub fn down(ports: &Ports, location: &QueuePaths, options: &DownOptions) -> Result<Value> {
    let (launchd, processes) = (ports.launchd, ports.processes);
    let queues = (ports.queues)(&location.db);
    let queue = queues.open()?;
    let queue: &dyn Queue = &*queue;
    let recording = RecordingBackend::over(ports.cmux, queues, None, ports.load_average);
    let cmux: &dyn WorkspaceBackend = &recording;
    // Every registration is considered for the workspace close, whichever
    // path this `down` takes: the rule is the same for all of them, and a
    // queue can hold supervisors of both modes at once.
    let registrations = queue.supervisors()?;
    let mut live = Vec::new();
    let mut dead = Vec::new();
    let mut pruned = Vec::new();
    let now = ports.clock.now();
    let mut listing = None;
    for registration in &registrations {
        if !processes.alive(registration.pid) {
            dead.push(registration.clone());
        } else if pid_taken_over(registration, processes, now, &mut listing) {
            prune_supervisor(queue, registration)?;
            pruned.push(json!({
                "token": registration.token,
                "pid": registration.pid,
                "reason": "pid_reused",
            }));
        } else {
            live.push(registration.clone());
        }
    }
    // The supervisors about to drain stop the broker when their drain ends
    // (ADR-t827-3 decision 2); asked before any signal, the unload's
    // included. A `--force` kill stops it below instead.
    // The queue's service stops after the supervisors (ADR-t1233-4
    // decision 1): `down` stops it once they are gone, and asks the ones
    // about to drain to stop it when their drain ends.
    let service = (ports.queue_service)(&location.db, Path::new("dagq"), Path::new("cmux"));
    let service_running =
        service.probe().state != crate::domain::queue_service::ServiceState::Stopped;
    if !live.is_empty() && !options.force {
        let tokens: Vec<LeaseToken> = live.iter().map(|r| r.token.clone()).collect();
        if let Err(error) = ports.broker.request_stop(&location.db, &tokens) {
            tracing::warn!(error = %format_args!("{error:#}"), "the broker's stop after the drain could not be asked for: {error:#}");
        }
        if service_running
            && let Err(error) = queue.record_queue_event(
                crate::domain::EventKind::QueueServiceStopRequested,
                json!({"supervisors": tokens, "by": "down"}),
            )
        {
            tracing::warn!(error = %format_args!("{error:#}"), "the queue service's stop after the drain could not be asked for: {error:#}");
        }
    }
    // An agent whose supervisor never registered (a crash loop) is still
    // unloaded, or it would keep restarting.
    let agent = launchd.uninstall(&location.label, &location.launch_agent)?;
    let unloaded = agent.loaded;
    // After the drain the queue's broker is stopped, as `dagq broker stop`
    // does; a failure is reported, not raised: the supervisor is stopped.
    let broker = |drained: bool| -> Option<Value> {
        match ports.broker.after_drain(&location.db, drained) {
            Ok(report) => report,
            Err(error) => Some(json!({"error": format!("{error:#}")})),
        }
    };
    // After the drain the service is stopped; while the supervisors drain
    // it is left to the last of them. Nothing is said of a service that
    // did not run.
    let stop_service = |drained: bool| -> Option<Value> {
        if !service_running {
            return None;
        }
        if !drained {
            return Some(json!({"outcome": "left_to_the_drain"}));
        }
        match service.stop(QUEUE_SERVICE_START_TIMEOUT) {
            Ok(Some(pid)) => {
                if let Err(error) = queue.record_queue_event(
                    crate::domain::EventKind::QueueServiceStopped,
                    json!({"pid": pid, "by": "down"}),
                ) {
                    tracing::warn!(error = %format_args!("{error:#}"), "the queue service's stop could not be recorded: {error:#}");
                }
                Some(json!({"outcome": "stopped", "pid": pid}))
            }
            // The last supervisor stopped it at the end of its drain.
            Ok(None) => Some(json!({"outcome": "not_running"})),
            Err(error) => Some(json!({"outcome": "failed", "error": format!("{error:#}")})),
        }
    };
    if live.is_empty() {
        if options.force {
            for registration in &dead {
                prune_supervisor(queue, registration)?;
                pruned.push(json!({"token": registration.token, "pid": registration.pid}));
            }
        }
        let mut report = json!({
            "outcome": "not_running",
            "launch_agent_unloaded": unloaded,
            "pruned_supervisors": pruned,
            "supervisor_workspaces": close_supervisor_workspaces(
                queue,
                cmux,
                &registrations,
                &live,
                // Nothing is alive to drain, so every workspace is ours.
                Stop::SeenThrough,
            ),
        });
        if let Some(broker) = broker(true) {
            report["broker"] = broker;
        }
        if let Some(queue_service) = stop_service(true) {
            report["queue_service"] = queue_service;
        }
        return Ok(report);
    }
    // launchd's bootout delivers the SIGTERM to the agent's own process; a
    // supervisor started by hand gets it from here. A second SIGTERM would
    // end a draining supervisor at once, so when the agent is loaded but
    // its pid is unknown nothing is signalled.
    for registration in &live {
        if registration.mode == Some(SupervisorMode::InCmux) {
            processes.interrupt(registration.pid)?;
        } else if (!agent.loaded || agent.pid.is_some()) && Some(registration.pid) != agent.pid {
            processes.terminate(registration.pid)?;
        }
    }
    let pids: Vec<u32> = live.iter().map(|r| r.pid).collect();
    let pid = pids[0];
    if options.force {
        for registration in &live {
            processes.kill(registration.pid)?;
            prune_supervisor(queue, registration)?;
        }
        // The dead ones go too, so no row is left pointing at a workspace
        // this call has just closed.
        for registration in &dead {
            prune_supervisor(queue, registration)?;
            pruned.push(json!({"token": registration.token, "pid": registration.pid}));
        }
        let mut report = json!({
            "outcome": "killed",
            "pid": pid,
            "pids": pids,
            "launch_agent_unloaded": unloaded,
            "pruned_supervisors": pruned,
            "supervisor_workspaces": close_supervisor_workspaces(
                queue,
                cmux,
                &registrations,
                &live,
                Stop::SeenThrough,
            ),
        });
        if let Some(broker) = broker(true) {
            report["broker"] = broker;
        }
        if let Some(queue_service) = stop_service(true) {
            report["queue_service"] = queue_service;
        }
        return Ok(report);
    }
    if options.wait {
        loop {
            let remaining = queue
                .supervisors()?
                .into_iter()
                .filter(|registration| {
                    live.iter().any(|l| l.token == registration.token)
                        && processes.alive(registration.pid)
                })
                .count();
            if remaining == 0 {
                break;
            }
            thread::sleep(options.poll);
        }
        let mut report = json!({
            "outcome": "stopped",
            "pid": pid,
            "pids": pids,
            "launch_agent_unloaded": unloaded,
            "supervisor_workspaces": close_supervisor_workspaces(
                queue,
                cmux,
                &registrations,
                &live,
                Stop::SeenThrough,
            ),
        });
        if !pruned.is_empty() {
            report["pruned_supervisors"] = json!(pruned);
        }
        if let Some(broker) = broker(true) {
            report["broker"] = broker;
        }
        if let Some(queue_service) = stop_service(true) {
            report["queue_service"] = queue_service;
        }
        return Ok(report);
    }
    let mut report = json!({
        "outcome": "draining",
        "pid": pid,
        "pids": pids,
        "launch_agent_unloaded": unloaded,
        "supervisor_workspaces": close_supervisor_workspaces(
            queue,
            cmux,
            &registrations,
            &live,
            Stop::Pending,
        ),
    });
    if !pruned.is_empty() {
        report["pruned_supervisors"] = json!(pruned);
    }
    if let Some(broker) = broker(false) {
        report["broker"] = broker;
    }
    if let Some(queue_service) = stop_service(false) {
        report["queue_service"] = queue_service;
    }
    Ok(report)
}

/// Whether this `down` saw the stop through, which decides what may be
/// done to an `in_cmux` supervisor's workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// `--wait` waited for the drain, or `--force` killed it: the
    /// supervisor is not going to do any more work, so its workspace is
    /// closed whatever its pid still looks like.
    SeenThrough,
    /// The default `down` returned while the supervisor drains. Closing
    /// the workspace would end that drain, so only one that had already
    /// exited before `down` ran is closed.
    Pending,
}

/// Close the cmux workspace of every `in_cmux` registration this `down` is
/// done with, and report the ones left to a running drain. A close that
/// cmux refuses is reported, not raised: the supervisor is already
/// stopped, which is what `down` was asked to do.
///
/// The `Pending` case uses the registrations classified as live before
/// anything was signalled. After a SIGKILL a PID check would be useless:
/// `kill(2)` returns before the target is reaped, so `kill(pid, 0)` still
/// succeeds for a process that is already dying.
fn close_supervisor_workspaces(
    queue: &dyn Queue,
    cmux: &dyn WorkspaceBackend,
    registrations: &[SupervisorRegistration],
    live: &[SupervisorRegistration],
    stop: Stop,
) -> Vec<Value> {
    registrations
        .iter()
        .filter(|registration| registration.mode == Some(SupervisorMode::InCmux))
        .filter_map(|registration| {
            let id = registration.workspace_id.as_deref()?;
            if stop == Stop::Pending && live.iter().any(|r| r.token == registration.token) {
                return Some(json!({
                    "workspace_id": id,
                    "outcome": "left_open",
                    "reason": format!(
                        "supervisor pid {} is still draining; `down --wait` closes it",
                        registration.pid
                    ),
                }));
            }
            Some(match cmux.close(id) {
                Ok(()) => {
                    // The record goes with the workspace. Forgetting it is
                    // tidiness only: `up` drops a UUID cmux no longer lists.
                    if queue
                        .session_workspace(SessionRole::Supervisor)
                        .ok()
                        .flatten()
                        .as_deref()
                        == Some(id)
                    {
                        let _ = queue.remove_session_workspace(SessionRole::Supervisor);
                    }
                    json!({"workspace_id": id, "outcome": "closed"})
                }
                Err(error) => json!({
                    "workspace_id": id,
                    "outcome": "close_failed",
                    "reason": format!("{error:#}"),
                }),
            })
        })
        .collect()
}

/// How long launchd waits after SIGTERM before it kills the supervisor. A
/// drain waits for the active runs, which can take as long as their
/// sessions, so the default (20 s) is far too short.
pub const EXIT_TIMEOUT_SECS: u32 = 86_400;

/// Everything that goes into the plist, kept as data so tests can check it
/// without parsing XML.
#[derive(Debug, Clone, Serialize)]
pub struct LaunchAgentSpec {
    pub label: String,
    pub plist: PathBuf,
    pub program_arguments: Vec<String>,
    pub working_directory: String,
    /// PATH of the shell that ran `up` (launchd's own is too small for
    /// `cmux` and `claude`) and the socket password when that shell
    /// exported it.
    pub environment: SupervisorEnvironment,
    pub log: String,
}

impl LaunchAgentSpec {
    /// The plist as launchd reads it: `KeepAlive` and `RunAtLoad` so the
    /// supervisor starts now and restarts after any exit, stdout and stderr
    /// appended to one file, and a long `ExitTimeOut` for the drain.
    pub fn xml(&self) -> String {
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n",
        );
        xml.push_str(&format!(
            "\t<key>Label</key>\n\t<string>{}</string>\n",
            escape(&self.label)
        ));
        xml.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
        for argument in &self.program_arguments {
            xml.push_str(&format!("\t\t<string>{}</string>\n", escape(argument)));
        }
        xml.push_str("\t</array>\n");
        xml.push_str(&format!(
            "\t<key>WorkingDirectory</key>\n\t<string>{}</string>\n",
            escape(&self.working_directory)
        ));
        xml.push_str(&format!(
            "\t<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>PATH</key>\n\t\t<string>{}</string>\n",
            escape(&self.environment.path)
        ));
        if let Some(password) = &self.environment.socket_password {
            xml.push_str(&format!(
                "\t\t<key>{SOCKET_PASSWORD_ENV}</key>\n\t\t<string>{}</string>\n",
                escape(password)
            ));
        }
        if let Some(config_home) = &self.environment.config_home {
            xml.push_str(&format!(
                "\t\t<key>{CONFIG_HOME_ENV}</key>\n\t\t<string>{}</string>\n",
                escape(config_home)
            ));
        }
        xml.push_str("\t</dict>\n");
        xml.push_str("\t<key>KeepAlive</key>\n\t<true/>\n");
        xml.push_str("\t<key>RunAtLoad</key>\n\t<true/>\n");
        xml.push_str(&format!(
            "\t<key>ExitTimeOut</key>\n\t<integer>{EXIT_TIMEOUT_SECS}</integer>\n"
        ));
        xml.push_str(&format!(
            "\t<key>StandardOutPath</key>\n\t<string>{log}</string>\n\
             \t<key>StandardErrorPath</key>\n\t<string>{log}</string>\n",
            log = escape(&self.log)
        ));
        xml.push_str("</dict>\n</plist>\n");
        xml
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_wait_refuses_only_while_a_replaced_supervisor_leases_a_run() {
        assert_eq!(no_wait_refusal(Some("0.0.1"), &[]), None);
        let held = [
            ("r1".to_string(), RunStatus::AwaitingIntegration),
            ("r2".to_string(), RunStatus::NeedsSession),
        ];
        let refusal = no_wait_refusal(Some("0.0.1"), &held).unwrap();
        assert!(
            refusal.contains("2 run(s) are still in flight"),
            "{refusal}"
        );
        assert!(
            refusal.contains("r1 awaiting_integration, r2 needs_session"),
            "{refusal}"
        );
        assert!(
            refusal.contains("0.0.1") && refusal.contains(VERSION),
            "{refusal}"
        );
        let unrecorded = no_wait_refusal(None, &held[..1]).unwrap();
        assert!(unrecorded.contains("version (unrecorded)"), "{unrecorded}");
        assert!(unrecorded.contains("1 run(s)"), "{unrecorded}");
    }
}
