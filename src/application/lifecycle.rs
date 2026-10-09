//! `up`, `down` and `inbox`: the cold start and the stop of one queue's
//! runtime, and the inbox a person opens in their own terminal. `up` makes
//! sure a supervisor is resident (as a launchd LaunchAgent, restarted
//! after any exit) and reports the queue's open work; it opens no inbox
//! and calls no cmux (ADR-t2159-1 decision 3): `dagq inbox` starts the
//! inbox's agent in the foreground of the terminal it is typed in
//! (decision 2). Planners are not resident: the runtime opens one when
//! there is planning to do ([`super::planner`], ADR-t1394-1). `down`
//! unloads the agent so the supervisor drains and is not restarted. Both
//! are idempotent: a second `up` reuses what the first one started.
//!
//! The in-cmux mode is retired (ADR-t1433-4): `up --in-cmux` is refused,
//! and a supervisor some earlier binary registered in that mode is still
//! handed over, replaced and stopped (its SIGINT) until a person moves it
//! to launchd with `down --wait` and `up`. Its cmux workspace, and the
//! inbox's an earlier `up` opened, are a person's to close: dagq only
//! forgets their records (ADR-t2159-1 decision 6).
//!
//! A supervisor is only reused while it runs this binary's own build.
//! Every registration carries the `binary_version` its process recorded,
//! and `up` hands a live supervisor of any other build over to this binary
//! without waiting for its sessions (the supervisor execs it under its own
//! pid and token), or drains one that cannot take a handoff before
//! starting one of its own in its place (ADR-0045 decisions 10, 15).
//!
//! The use cases reach the queue, launchd, processes, Claude Code and the
//! files through [`Ports`]; the entry points in [`crate::compose`] build
//! the adapters.
use super::{
    AgentProvider, CONFIG_HOME_ENV, Clock, CommandSpec, LaunchAgent, ProcessControl, QueueOpener,
    RunCoordination, RunFiles, RunLog, RunRecovery, SessionRegistry, SupervisorEnvironment,
    SupervisorRegistry,
    actor_executor::{
        ActorExecutionSpec, ActorExecutor, ActorProgram, HostActorExecutor, WorkspaceAccess,
    },
    path_text,
    prompt::inbox_prompt,
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
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

/// Set in the environment of every session of a queue: the role the
/// session plays, so that `dagq inbox` typed inside the inbox opens no
/// second one (ADR-t2159-1 decision 2), and the plugin's hook knows the
/// session however it was started.
pub const ROLE_ENV: &str = crate::domain::actor::ROLE_ENV;
/// The queue database the session belongs to.
pub const QUEUE_ENV: &str = "DAGQ_QUEUE";
/// `DAGQ_ROLE` of a run's session wrapper (and of its resumes').
pub const WORKER_ROLE: &str = ActorRole::Worker.as_str();
/// `DAGQ_ROLE` of a planner session, which writes goals and tasks and
/// submits them as a proposal. The runtime starts one as a background
/// wrapper with no workspace (ADR-t1433-2); `up` opens none, and `dagq
/// plan` no longer opens one a person talks with (ADR-t1394-1).
pub const PLANNER_ROLE: &str = ActorRole::Planner.as_str();
pub use crate::domain::actor::{PLANNER_ID_ENV, PLANNER_ORIGIN_ENV, SESSION_KIND_ENV};
/// The cmux workspace a session runs in, set by cmux in every terminal:
/// a person's planner's workspace owns the proposals it submits (a planner
/// of the runtime's has none: its record's background handle owns them).
pub const CMUX_WORKSPACE_ENV: &str = "CMUX_WORKSPACE_ID";
/// `DAGQ_ROLE` of the session where a person answers the queue's asks: a
/// person opens it in their own terminal with `dagq inbox`.
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

/// What `up --in-cmux` is refused with (ADR-t1433-4 decision 2): the mode
/// is retired, and how a supervisor registered in it moves to launchd.
pub const IN_CMUX_RETIRED: &str = "the in-cmux mode is retired: the supervisor calls no cmux and \
runs only under launchd, so `up --in-cmux` starts nothing. Run `up` without --in-cmux (with the \
other flags you give it); a supervisor already running in a cmux workspace moves to launchd with \
`down --wait` and then `up` without --in-cmux";

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

/// Check the Claude Code at `executable` (`agent`) before `dagq <command>`
/// starts what loads the plugin from `cwd`: the agent's own preflight,
/// then, unless `plugin_dir` is given or `check_plugin` is off,
/// [`require_installed_plugin`] for the inbox. `up` and `dagq inbox` run
/// the same check (ADR-t617-2 decisions 1, 4).
pub fn require_agent(
    agent: &dyn AgentProvider,
    executable: &Path,
    plugin_dir: Option<&Path>,
    cwd: &Path,
    command: &str,
    check_plugin: bool,
) -> Result<()> {
    agent.preflight()?;
    if check_plugin && plugin_dir.is_none() {
        require_installed_plugin(agent, executable, cwd, "inbox", command)?;
    }
    Ok(())
}

/// Where the queue `up` and `down` work on lives: its database, its hash,
/// and the LaunchAgent label, plist and log directory of its supervisor.
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

/// What `up` and `down` reach the outside through. `queues` opens the
/// queue at a database path as their ports (once for the use case);
/// `inspect_repository` finds the repository containing a checkout;
/// `trusts_repository(config, root)` reads Claude Code's global config for
/// the folder trust of `root`. Neither reaches cmux (ADR-t2159-1 decision 1).
pub struct Ports<'a> {
    pub launchd: &'a dyn LaunchAgent,
    pub processes: &'a dyn ProcessControl,
    pub files: &'a dyn RunFiles,
    pub clock: &'a dyn Clock,
    pub queues: &'a dyn Fn(&Path) -> Arc<dyn QueueOpener<dyn LifecycleQueue + Send>>,
    pub inspect_repository: &'a dyn Fn(&Path) -> Result<RepositoryPaths>,
    pub trusts_repository: &'a dyn Fn(&Path, &Path) -> Result<bool>,
    /// `run_env_programs(checkout, db, path)` checks the programs the
    /// `[run.env]` of the `dagq.toml` in `checkout` names on `path`
    /// (ADR-0049 decision 9).
    pub run_env_programs: &'a dyn Fn(&Path, &Path, &str) -> Result<RunEnvCheck>,
    /// `ci_watch_preflight(checkout, path)`: with `[ci_watch]` in the
    /// `dagq.toml` in `checkout`, why `gh` on `path` cannot read the CI
    /// (ADR-t1920-1 decision 2); `None` when it can or there is no table.
    pub ci_watch_preflight: &'a dyn Fn(&Path, &str) -> Result<Option<String>>,
    /// `resolve_language(checkout, user_config)`: the language of the
    /// `dagq.toml` in `checkout` over the user's `config.toml`, or the
    /// mistake in either (ADR-t616-2).
    pub resolve_language: &'a ResolveLanguage,
    /// `queue_service(db, executable)`: the control of the queue's
    /// service, started from `executable` (ADR-t1233-4 decision 1). The
    /// service calls no cmux (ADR-t1433-1).
    pub queue_service: &'a QueueServiceControls,
}

/// How `up` and `down` reach the queue's service: see
/// [`Ports::queue_service`].
pub type QueueServiceControls =
    dyn Fn(&Path, &Path) -> Box<dyn crate::application::queue_service::QueueServiceControl>;

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
    /// `up --in-cmux`, the retired mode: `up` refuses it before anything
    /// else ([`IN_CMUX_RETIRED`]).
    pub in_cmux: bool,
    /// Refuse to wait for a supervisor of another version to drain. Only a
    /// replacement reads it (ADR-0014): with runs in flight `up` stops
    /// without touching anything, and with none it replaces the supervisor
    /// but bounds the drain by `startup_timeout` rather than waiting for a
    /// supervisor that turns out not to stop.
    pub no_wait: bool,
    /// Passed to the supervisor, whose planners load it with `--plugin-dir`.
    pub plugin_dir: Option<PathBuf>,
    /// Resolved executables; the agent runs the supervisor with these. The
    /// cmux is only passed on for the supervisor's older command line.
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

/// Ensure the supervisor exists and report the queue's open work.
/// Preflight first ([`require_agent`]: claude and the installed plugin
/// without `--plugin-dir`; Claude Code's trust of the repository root, an
/// initialized queue, the repository), then prune registrations whose
/// process is gone, start the agent only when no live registration of
/// this binary's version remains — draining and replacing a live
/// supervisor of any other version. `up` opens no inbox (ADR-t2159-1
/// decision 3): its result says to open it with `dagq inbox`. The
/// workspaces an earlier binary recorded (the inbox's, the retired
/// maintainer's and resident planner's) are forgotten without cmux; the
/// workspaces themselves are left for a person to close.
///
/// `claude` is the Claude Code the supervisor's sessions and the inbox
/// start, whose preflight `up` runs.
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
    ensure!(!options.in_cmux, IN_CMUX_RETIRED);
    let (launchd, processes) = (ports.launchd, ports.processes);
    let db = ports
        .files
        .canonicalize(&location.db)
        .context("queue must already be initialized")?;
    let repository = (ports.inspect_repository)(repo)?;
    // A restart by the runtime is not a person's `up`, and starts what ran
    // before without the plugin's check.
    if !options.no_claude {
        require_agent(
            claude,
            &options.claude,
            options.plugin_dir.as_deref(),
            &repository.root,
            "up",
            !environment.restart,
        )
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
    // So must the GitHub CLI `[ci_watch]` reads the CI with: without it the
    // supervisor would claim and land nothing (ADR-t1920-1 decision 2).
    if let Some(message) = (ports.ci_watch_preflight)(trust_root, &environment.path)? {
        bail!("{message}; the supervisor was not started");
    }
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
    // The supervisor's planners load it; one that is not there stops `up`.
    if let Some(dir) = &options.plugin_dir {
        ports
            .files
            .canonicalize(dir)
            .with_context(|| format!("plugin directory {}", dir.display()))?;
    }
    let queue = (ports.queues)(&db).open()?;
    let queue = &*queue;
    // The queue's service before the supervisor, of this binary's build
    // (ADR-t1233-4 decision 1): one that does not start stops `up`.
    let queue_service = if options.queue_service {
        let control = (ports.queue_service)(&db, &environment.current_exe);
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
    let up = Up {
        location,
        db: &db,
        repository: &repository,
        queue,
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
    // others serve under this build, and they get `--auto-update` all the
    // same before `up` fails naming the ones that did not.
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
        None => start_under_launchd(&up, &existing)?,
    };
    let supervisor = set_auto_update(queue, supervisor, &live, options.auto_update)?;

    // The workspaces an earlier `up` opened are a person's to close; their
    // records go without a call to cmux (ADR-t2159-1 decision 6).
    let retired_sessions = queue.forget_retired_session_workspaces()?;
    let inbox = if options.no_claude {
        json!({"outcome": "skipped", "reason": "provider_disabled", "next": "use a manually opened inbox"})
    } else {
        json!({"outcome": "not_opened", "next": INBOX_NEXT})
    };

    let mut report = json!({
        "supervisor": supervisor,
        "inbox": inbox,
        "retired_sessions": retired_sessions,
        "pruned_supervisors": pruned,
        "doctor": open_work(queue, processes, ports.clock)?,
        "repository": landing,
        "language": language,
    });
    // What the preflight found, only for a repository with a dagq.toml.
    if run_env.config {
        report["run_env"] = serde_json::to_value(&run_env)?;
    }
    if let Some(queue_service) = queue_service {
        report["queue_service"] = queue_service;
    }
    match handoff_failure {
        None => Ok(report),
        Some(message) => Err(PartialHandoff { message, report }.into()),
    }
}

/// What `up` says of the inbox it does not open (ADR-t2159-1 decision 3).
pub const INBOX_NEXT: &str = "open the inbox with `dagq inbox` in your own terminal (any terminal \
or IDE, in the repository); it starts the inbox's agent there in the foreground";

/// The error of an `up` whose handoff some or all of the supervisors did
/// not take: `up` still set `--auto-update`, and
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

/// The queue's ports `up`, `down` and `dagq inbox` reach: the
/// supervisors' registrations and handoffs, the runs they lease, the
/// events, and the workspaces an earlier binary recorded.
pub trait LifecycleQueue:
    SupervisorRegistry + RunCoordination + RunRecovery + RunLog + SessionRegistry
{
}

impl<T: SupervisorRegistry + RunCoordination + RunRecovery + RunLog + SessionRegistry + ?Sized>
    LifecycleQueue for T
{
}

/// One `up`'s settled inputs, shared by the ways it starts a supervisor.
struct Up<'a, Q: ?Sized> {
    location: &'a QueuePaths,
    db: &'a Path,
    repository: &'a RepositoryPaths,
    queue: &'a Q,
    launchd: &'a dyn LaunchAgent,
    processes: &'a dyn ProcessControl,
    clock: &'a dyn Clock,
    environment: &'a UpEnvironment,
    options: &'a UpOptions,
}

/// The actor of the session `role` has a single instance of (the inbox):
/// its id is the role's name.
pub fn session_actor(role: SessionRole) -> ActorContext {
    ActorContext::new(role.actor_role(), role.as_str())
}

/// Drain every live supervisor of another build and start one of this
/// binary's version in its place (ADR-0014), so that updating the fixed
/// binary is `up` and nothing else. The stop is `down --wait`'s: unload the
/// LaunchAgent (whose bootout carries the SIGTERM, and whose `KeepAlive`
/// would otherwise restart the old binary at once), SIGINT a supervisor an
/// earlier binary registered in the retired in-cmux mode, SIGTERM one
/// launchd did not signal, then wait for each registration to go — the
/// supervisor stops claiming, finishes the runs it holds and deregisters —
/// and forget the records of the in-cmux ones' workspaces, which a person
/// closes (ADR-t2159-1 decision 6). The one started in their place runs
/// under launchd, whatever mode they had (ADR-t1433-4).
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
fn replace_supervisors(
    up: &Up<impl LifecycleQueue + ?Sized>,
    live: &[SupervisorRegistration],
) -> Result<Value> {
    let Up {
        location,
        queue,
        launchd,
        processes,
        options,
        ..
    } = *up;
    // The version reported as replaced is an outdated one, not merely the
    // first: a mixed set is drained whole, but naming a version that
    // matched would read as if nothing had been out of date.
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
    // rows go the way `down --force` drops them.
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
    let left = forget_supervisor_workspaces(queue, live);
    // Whatever survived the drain (an alive-but-silent supervisor `up`
    // neither reuses nor kills) belongs to another process, not to the one
    // started below.
    let existing: HashSet<LeaseToken> = queue
        .supervisors()?
        .into_iter()
        .map(|registration| registration.token)
        .collect();
    let mut started = start_under_launchd(up, &existing)?;
    let object = started
        .as_object_mut()
        .expect("a started supervisor is a JSON object");
    object.insert("outcome".into(), json!("restarted"));
    object.insert("previous_version".into(), json!(previous_version));
    object.insert("replaced".into(), json!(replaced));
    object.insert("supervisor_workspaces".into(), json!(left));
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
fn takes_handoff(
    up: &Up<impl LifecycleQueue + ?Sized>,
    files: &dyn RunFiles,
    live: &[SupervisorRegistration],
) -> bool {
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
    up: &Up<impl LifecycleQueue + ?Sized>,
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
    queue: &(impl SupervisorRegistry + RunLog + ?Sized),
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
    queue: &(impl SupervisorRegistry + RunLog + ?Sized),
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
fn stopping_supervisors(queue: &(impl RunLog + ?Sized)) -> Result<HashSet<LeaseToken>> {
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
        queue: &(impl RunLog + ?Sized),
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
    queue: &(impl SupervisorRegistry + RunLog + ?Sized),
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

/// Write and load the LaunchAgent, the one way `up` starts a supervisor:
/// the supervisor calls no cmux, so nothing about cmux is proven for it
/// (ADR-t1433-4 decision 1).
fn start_under_launchd(
    up: &Up<impl LifecycleQueue + ?Sized>,
    existing: &HashSet<LeaseToken>,
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

/// `<this binary> --db <db> supervise [--parallel N] --log-dir <queue logs>
/// --cmux <resolved> --claude <resolved> --codex <resolved or given> --mode
/// launchd [--plugin-dir <dir>]`:
/// what the LaunchAgent keeps going. The executables are absolute so
/// launchd's PATH does not decide which ones run; the plugin directory is
/// what the planners the runtime opens load. `--mode` is what its start
/// mark records (ADR-0051 decision 10): `up` writes the registration's
/// mode only after it sees the registration.
fn supervise_arguments(
    location: &QueuePaths,
    db: &Path,
    environment: &UpEnvironment,
    options: &UpOptions,
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
        SupervisorMode::Launchd.as_str().into(),
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
fn prune_supervisor(
    queue: &(impl SupervisorRegistry + ?Sized),
    registration: &SupervisorRegistration,
) -> Result<()> {
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
    queue: &(impl SupervisorRegistry + ?Sized),
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

/// Which of `registrations` is the supervisor `up` has just started: the
/// first that is fresh at `now` (its pid `alive`, its heartbeat within
/// [`HEARTBEAT_TIMEOUT_SECS`]) and whose token is not in `existing`.
/// Tokens in `existing` were already registered when `up` looked, so they
/// belong to another process: one of them may start heartbeating again
/// mid-wait (an alive but silent supervisor `up` neither reuses nor kills),
/// and taking it for ours would stamp this start's mode onto a supervisor
/// `up` did not start.
fn started_registration<'a>(
    registrations: &'a [SupervisorRegistration],
    existing: &HashSet<LeaseToken>,
    alive: &dyn Fn(u32) -> bool,
    now: i64,
) -> Option<&'a SupervisorRegistration> {
    registrations.iter().find(|registration| {
        !existing.contains(&registration.token)
            && alive(registration.pid)
            && now - registration.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS
    })
}

/// Wait until the supervisor `up` has just started registers itself
/// ([`started_registration`]), up to `startup_timeout`.
fn wait_for_registration(
    up: &Up<impl LifecycleQueue + ?Sized>,
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
        let registrations = queue.supervisors()?;
        if let Some(registration) = started_registration(
            &registrations,
            existing,
            &|pid| processes.alive(pid),
            up.clock.now(),
        ) {
            return Ok(registration.clone());
        }
        ensure!(Instant::now() < deadline, "no live supervisor registration");
        thread::sleep(options.poll);
    }
}

/// The agent definition: this binary running `supervise` on this queue from
/// the repository root, with the caller's PATH (and exported
/// `XDG_CONFIG_HOME`) and the queue's log directory.
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
        program_arguments: supervise_arguments(location, db, environment, options)?,
        working_directory: path_text(repo_root)?,
        environment: SupervisorEnvironment {
            path: environment.path.clone(),
            config_home: environment.config_home.clone(),
        },
        log: path_text(&location.log_dir.join(LAUNCHD_LOG_NAME))?,
    })
}

/// The inbox's first message: `inbox_prompt` with the instruction of
/// `language` (ADR-t616-2). The role and queue are the environment of the
/// command `dagq inbox` starts, not a prefix of it.
pub fn inbox_session_prompt(db: &Path, language: Option<&Language>) -> Result<String> {
    Ok(with_instruction(inbox_prompt(db)?, language))
}

/// What `dagq inbox` is refused with inside the inbox (ADR-t2159-1
/// decision 2), whatever queue `DAGQ_QUEUE` names: an inbox does not open
/// another inbox in its terminal.
pub const INBOX_INSIDE_REFUSED: &str = "dagq inbox is refused inside an inbox (DAGQ_ROLE=inbox): \
an inbox does not open another one in its terminal. Run `dagq inbox` in a terminal without \
DAGQ_ROLE; to open this queue's inbox again, end this one first";

/// What `dagq inbox` reaches the outside through: the queue's events at a
/// database path (the one port it records `inbox_opened` through), the
/// files, the repository containing a checkout, and the language of its
/// `dagq.toml` over the user's `config.toml`.
pub struct InboxPorts<'a> {
    pub files: &'a dyn RunFiles,
    pub queues: &'a dyn Fn(&Path) -> Arc<dyn QueueOpener<dyn RunLog + Send>>,
    pub inspect_repository: &'a dyn Fn(&Path) -> Result<RepositoryPaths>,
    pub resolve_language: &'a ResolveLanguage,
}

/// What `dagq inbox` is given: the plugin directory the inbox loads
/// (`--plugin-dir`; without it the installed plugin, which is checked) and
/// the executable of its agent as given (`--claude`).
#[derive(Debug, Clone)]
pub struct InboxOptions {
    pub plugin_dir: Option<PathBuf>,
    pub agent: PathBuf,
}

/// The agent `dagq inbox` starts: its provider, which `inbox_opened`
/// records, and the adapter that makes its command.
pub struct InboxAgent<'a> {
    pub provider: crate::domain::Provider,
    pub agent: &'a dyn AgentProvider,
}

/// `dagq inbox` (ADR-t2159-1 decision 2): the command that runs the
/// inbox's agent, for the caller to start in the foreground of its own
/// terminal (it execs it). Refused inside an inbox ([`INBOX_INSIDE_REFUSED`])
/// before anything is read or recorded. Then the same check of the agent
/// and the plugin as `up` ([`require_agent`]), the provider's
/// [`AgentProvider::inbox_command`] (its settings, the plugin directory and
/// [`inbox_session_prompt`]) through the actor executor, in the
/// repository's main checkout with the inbox's `DAGQ_ROLE` and
/// `DAGQ_QUEUE`; and, before it is returned, `inbox_opened` with whether
/// the settings went with it, which `status` and `doctor` judge the
/// inbox's guardrail by ([`super::inbox_guardrail::judge`]). Its payload
/// is `guardrail` (whether the provider has the settings,
/// [`AgentProvider::inbox_settings`]), `settings` (their path or null) and
/// `provider`; an inbox opened this way has no workspace.
pub fn inbox(
    ports: &InboxPorts,
    started: &InboxAgent,
    db: &Path,
    repo: &Path,
    environment: &UpEnvironment,
    options: &InboxOptions,
) -> Result<CommandSpec> {
    ensure!(
        environment.role.as_deref() != Some(INBOX_ROLE),
        INBOX_INSIDE_REFUSED
    );
    let agent = started.agent;
    let db = ports
        .files
        .canonicalize(db)
        .context("queue must already be initialized")?;
    let repository = (ports.inspect_repository)(repo)?;
    let checkout = match &repository.checkout {
        Ok(checkout) => checkout.clone(),
        Err(error) => bail!("{error}; the inbox was not opened"),
    };
    require_agent(
        agent,
        &options.agent,
        options.plugin_dir.as_deref(),
        &checkout,
        "inbox",
        true,
    )
    .map_err(|error| anyhow::anyhow!("{error:#}; the inbox was not opened"))?;
    let language = (ports.resolve_language)(&checkout, environment.user_config.as_deref())
        .map_err(|error| anyhow::anyhow!("{error:#}; the inbox was not opened"))?;
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
    let queue = (ports.queues)(&db).open()?;
    let command = HostActorExecutor::new(&db)
        .with_provider(agent)
        .spawn(ActorExecutionSpec::new(
            session_actor(SessionRole::Inbox),
            WorkspaceAccess::Write(checkout.clone()),
            ActorProgram::Foreground {
                cwd: &checkout,
                prompt: inbox_session_prompt(&db, language.as_ref())?,
                plugin_dir: plugin_dir.as_deref(),
                launch: None,
            },
        ))?
        .foreground()?;
    let settings = agent.inbox_settings(db.parent().unwrap_or(Path::new(".")));
    queue.record_queue_event(
        EventKind::InboxOpened,
        json!({
            "guardrail": settings.is_some(),
            "settings": settings,
            "provider": started.provider.as_str(),
        }),
    )?;
    Ok(command)
}

/// What a person looks at first after `up`: unfinished runs with whether their
/// lease still has a live, heartbeating owner, and the runs that wait for
/// review (`awaiting_integration`) or a resumed session (`needs_session`).
fn open_work(
    queue: &(impl RunCoordination + RunLog + ?Sized),
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
/// without being restarted), which delivers SIGTERM to the agent's own
/// process; a supervisor started by hand gets the SIGTERM from here, and an
/// `in_cmux` one an earlier binary registered (ADR-t1433-4 decision 3)
/// gets a SIGINT, the signal its terminal would send. The runtime drains
/// on either.
///
/// `down` closes no cmux workspace (ADR-t2159-1 decision 3): the one an
/// `in_cmux` supervisor runs in is reported `left_open` for a person to
/// close, and its record in `session_workspaces` is forgotten without a
/// call to cmux. The inbox is the person's own terminal.
pub fn down(ports: &Ports, location: &QueuePaths, options: &DownOptions) -> Result<Value> {
    let (launchd, processes) = (ports.launchd, ports.processes);
    let queue = (ports.queues)(&location.db).open()?;
    let queue = &*queue;
    // Every registration's workspace is reported, whichever path this
    // `down` takes: a queue can hold supervisors of both modes at once.
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
    // The queue's service stops after the supervisors (ADR-t1233-4
    // decision 1): `down` stops it once they are gone, and asks the ones
    // about to drain to stop it when their drain ends.
    let service = (ports.queue_service)(&location.db, Path::new("dagq"));
    let service_running =
        service.probe().state != crate::domain::queue_service::ServiceState::Stopped;
    if !live.is_empty() && !options.force {
        let tokens: Vec<LeaseToken> = live.iter().map(|r| r.token.clone()).collect();
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
            "supervisor_workspaces": forget_supervisor_workspaces(queue, &registrations),
        });
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
        // The dead ones go too.
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
            "supervisor_workspaces": forget_supervisor_workspaces(queue, &registrations),
        });
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
            "supervisor_workspaces": forget_supervisor_workspaces(queue, &registrations),
        });
        if !pruned.is_empty() {
            report["pruned_supervisors"] = json!(pruned);
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
        "supervisor_workspaces": forget_supervisor_workspaces(queue, &registrations),
    });
    if !pruned.is_empty() {
        report["pruned_supervisors"] = json!(pruned);
    }
    if let Some(queue_service) = stop_service(false) {
        report["queue_service"] = queue_service;
    }
    Ok(report)
}

/// What became of the cmux workspace of every `in_cmux` registration among
/// `registrations`: each is `left_open` for a person to close, since dagq
/// closes no cmux workspace (ADR-t2159-1 decisions 3, 6), and the
/// supervisor's record in `session_workspaces` is forgotten without a call
/// to cmux. Forgetting it is tidiness only, so a failure is not raised:
/// the supervisor's stop is what was asked for.
fn forget_supervisor_workspaces(
    queue: &(impl SessionRegistry + ?Sized),
    registrations: &[SupervisorRegistration],
) -> Vec<Value> {
    let _ = queue.remove_session_workspace(SessionRole::Supervisor);
    registrations
        .iter()
        .filter(|registration| registration.mode == Some(SupervisorMode::InCmux))
        .filter_map(|registration| {
            let id = registration.workspace_id.as_deref()?;
            Some(json!({
                "workspace_id": id,
                "outcome": "left_open",
                "reason": "dagq closes no cmux workspace: a person closes it",
            }))
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
    /// `claude` and the tools of a run) and `XDG_CONFIG_HOME` when that
    /// shell exported it.
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

    /// A handoff reads the stop requests from the events alone: the
    /// supervisors that recorded `supervisor_draining` before the
    /// registrations were read, and those that recorded it while they were
    /// read, apart; events of other kinds are not stops.
    #[test]
    fn the_stops_around_a_look_are_the_draining_events_before_and_after_it() {
        let queue = crate::application::port_fakes::SupervisorsAndEvents::default();
        queue
            .record_queue_event(
                EventKind::SupervisorDraining,
                json!({"supervisor": "early"}),
            )
            .unwrap();
        queue
            .record_queue_event(
                EventKind::SupervisorStarted,
                json!({"supervisor": "started"}),
            )
            .unwrap();
        let stops = Stops::around(&queue, || {
            queue
                .record_queue_event(EventKind::SupervisorDraining, json!({"supervisor": "late"}))?;
            Ok((7, vec![registration("late", 1, 7)]))
        })
        .unwrap();
        assert_eq!(stops.before, HashSet::from([LeaseToken::new("early")]));
        assert_eq!(
            stops.after,
            HashSet::from([LeaseToken::new("early"), LeaseToken::new("late")])
        );
        assert_eq!(stops.now, 7);
        assert_eq!(stops.registrations.len(), 1);
    }

    fn registration(token: &str, pid: u32, heartbeat_at: i64) -> SupervisorRegistration {
        SupervisorRegistration {
            token: LeaseToken::new(token),
            pid,
            parallel: 2,
            started_at: 0,
            heartbeat_at,
            mode: None,
            workspace_id: None,
            handoff_accepted: true,
            handoff_binary: None,
            auto_update: false,
            max_waiting: None,
            parallel_source: None,
            max_waiting_source: None,
            runtime_planners: None,
            runtime_planners_source: None,
            claim_spacing: None,
            claim_spacing_source: None,
            max_load: None,
            providers: None,
            binary_version: Some(VERSION.into()),
        }
    }

    /// The supervisor `up` started is the first fresh registration it had
    /// not seen before the start: a registration that was there already
    /// (an alive but silent supervisor heartbeating again mid-wait) is
    /// never taken for it, however fresh and early, and neither is a new
    /// one whose pid is dead or whose heartbeat is stale.
    #[test]
    fn the_started_supervisor_is_the_first_fresh_registration_not_seen_before() {
        const NOW: i64 = 1_000_000;
        let alive = |pid: u32| pid != 3;
        let existing: HashSet<LeaseToken> = [LeaseToken::new("silent")].into();
        let registrations = [
            // First in `started_at` order and fresh again, but seen before.
            registration("silent", 1, NOW),
            registration("dead", 3, NOW),
            registration("stale", 4, NOW - HEARTBEAT_TIMEOUT_SECS - 1),
            registration("started", 5, NOW - HEARTBEAT_TIMEOUT_SECS),
            registration("later", 6, NOW),
        ];
        let found = started_registration(&registrations, &existing, &alive, NOW).unwrap();
        assert_eq!(found.token, "started");
        // Only the one seen before: nothing was started yet.
        assert!(started_registration(&registrations[..1], &existing, &alive, NOW).is_none());
        // Without it in `existing`, the silent one would be taken.
        let found = started_registration(&registrations, &HashSet::new(), &alive, NOW).unwrap();
        assert_eq!(found.token, "silent");
    }
}
