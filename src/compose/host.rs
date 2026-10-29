//! The wiring of host運用 (docs/design/architecture.md): `up`, `down`,
//! `install` and the update jobs, `rebind`, the queue service's commands,
//! `init` and `migrate`, the inbox's command, and the ports of the
//! supervisor's release check, host metrics, queue service and sccache.
//! Each entry builds the adapters and calls the use case; what is decided
//! is in `application` and `domain`.

use super::OneShot;
use crate::application::queue_commands::QueueCommand;
use crate::application::queue_service::QueueServiceControl;
use crate::application::supervisor_service::{SupervisorQueue, SupervisorQueues};
use crate::domain::LeaseToken;
use crate::domain::queue_service::{ControlCall, ControlCommand};
use crate::infrastructure::location::runs_dir;
use crate::infrastructure::queue_service as service;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use crate::{
    application::{
        AgentProvider, CommandSpec, HostOpsQueue, LaunchAgent, ProcessControl, QueueOpener, RunLog,
        install::{self as installation, Binaries, InstallOptions},
        lifecycle::{
            self, DownOptions, InboxAgents, InboxOptions, InboxPorts, LifecycleQueue,
            Ports as LifecyclePorts, QueuePaths, RepositoryPaths, UpEnvironment, UpOptions,
        },
        rebind::{self as rebinding, Rebind, RebindTarget},
        update,
    },
    domain::SupervisorRegistration,
    infrastructure::{
        adapters::{
            ClaudeCode, ClaudePlugin, GitRepository, SystemProcesses, claude_trusts_repository,
            path_text,
        },
        binaries::LocalBinaries,
        clock,
        location::{QueueLocation, REPOSITORY_FILE_NAME, data_home},
        queue_binary::{self, QueueBinary},
        run_files::LocalRunFiles,
        runtime_store::{SqliteOpener, SqlitePorts},
        sqlite::SqliteQueue,
    },
};

/// The automatic update's job as the supervisor starts it (`auto-update`,
/// ADR-0045 decision 17).
#[derive(Debug, Clone)]
pub struct AutoUpdateJob {
    pub commit: String,
    /// The supervisor that started it.
    pub token: LeaseToken,
    /// The fixed binary to replace.
    pub target: PathBuf,
    /// A checkout of the repository.
    pub repository: PathBuf,
    /// Where the build's output is appended.
    pub log: PathBuf,
    pub build_command: Option<String>,
    /// A shell command in place of the e2e the build passes before it is
    /// put in place (ADR-t963-1 decision 1; tests).
    pub e2e_command: Option<String>,
    /// How long that e2e may run.
    pub e2e_timeout: Duration,
    /// What a supervisor started again by `up` uses (see [`restarter`]).
    pub claude: PathBuf,
    /// The Codex CLI of the supervisor that started the job (ADR-t813-2).
    pub codex: PathBuf,
    pub plugin_dir: Option<PathBuf>,
    pub handoff_timeout: Duration,
    pub watch_timeout: Duration,
    /// Between two looks at the handoff and at the new supervisor's
    /// heartbeat (`auto-update --poll-ms`, 500 ms by default).
    pub poll: Duration,
}

/// The log of the automatic update's e2e next to its build's:
/// `update-<time>-<commit>.e2e.log` for `update-<time>-<commit>.build.log`.
fn e2e_log(build_log: &Path) -> PathBuf {
    let name = build_log
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = name.strip_suffix(".build.log").unwrap_or(&name);
    build_log.with_file_name(format!("{stem}.e2e.log"))
}

/// The arguments of the `up` that starts a supervisor an earlier binary
/// registered in the retired in-cmux mode again after an update's job,
/// beside `--db` and its own flags: no `--cmux` (ADR-t2159-1 decision 6).
fn restart_arguments(
    claude: &Path,
    codex: &Path,
    plugin_dir: Option<&Path>,
) -> Result<Vec<String>> {
    let mut arguments = vec![
        "--claude".to_owned(),
        path_text(claude)?,
        "--codex".to_owned(),
        path_text(codex)?,
    ];
    if let Some(dir) = plugin_dir {
        arguments.extend(["--plugin-dir".to_owned(), path_text(dir)?]);
    }
    Ok(arguments)
}

/// Starts a supervisor a person's `install` found gone with the binary in
/// place, as [`update::restart`] decides: `binary` runs the `up` of one
/// registered in the retired in-cmux mode, as the install's caller.
fn restarter<'a>(
    db: &'a Path,
    binary: &'a Path,
    arguments: &'a [String],
) -> impl Fn(&SupervisorRegistration) -> Result<Value> + 'a {
    move |registration: &SupervisorRegistration| -> Result<Value> {
        update::restart(registration, &path_text(db)?, arguments, &|up| {
            LocalBinaries.run(binary, up)
        })
    }
}

/// The restart of an update's job ([`update::restart_by_update`]): it runs
/// no `up`.
fn update_restarter<'a>(
    db: &'a Path,
    arguments: &'a [String],
) -> impl Fn(&SupervisorRegistration) -> Result<Value> + 'a {
    move |registration: &SupervisorRegistration| -> Result<Value> {
        update::restart_by_update(registration, &path_text(db)?, arguments)
    }
}

/// The release update's job as the supervisor starts it (`release-update`,
/// ADR-t618-1 decision 5).
#[derive(Debug, Clone)]
pub struct ReleaseUpdateJob {
    /// The release to install.
    pub version: String,
    /// The supervisor that started it.
    pub token: LeaseToken,
    /// The binary to replace.
    pub target: PathBuf,
    /// Where cargo's output is appended.
    pub log: PathBuf,
    /// The cargo that installs the release.
    pub cargo: PathBuf,
    /// What a supervisor started again by `up` uses (see [`restarter`]).
    pub claude: PathBuf,
    /// The Codex CLI of the supervisor that started the job (ADR-t813-2).
    pub codex: PathBuf,
    pub plugin_dir: Option<PathBuf>,
    pub handoff_timeout: Duration,
    pub watch_timeout: Duration,
    /// Only bring the installed plugin to the release (ADR-t618-2
    /// decision 4).
    pub plugin_only: bool,
}

/// How the supervisor keeps the sccache server
/// ([`SuperviseOptions::sccache`](super::SuperviseOptions::sccache)).
#[derive(Debug, Clone)]
pub struct SccacheOptions {
    /// Process query programs; defaults use the host PATH.
    pub lsof: PathBuf,
    pub ps: PathBuf,
    /// How long a start may take before it is taken to have failed.
    pub start_timeout: Duration,
}

impl Default for SccacheOptions {
    fn default() -> Self {
        Self {
            start_timeout: crate::infrastructure::sccache::START_TIMEOUT,
            lsof: "lsof".into(),
            ps: "ps".into(),
        }
    }
}

/// How the supervisor keeps the queue's service ([`SuperviseOptions::queue_service`](super::SuperviseOptions::queue_service)).
#[derive(Debug, Clone)]
pub struct QueueServiceOptions {
    /// The binary the service runs: the supervisor's own.
    pub executable: PathBuf,
    pub interval: Duration,
    pub start_timeout: Duration,
    /// Starts, looks at and stops the service instead of the system's
    /// control of `executable`; tests set it.
    pub control: Option<QueueServiceControlPort>,
}

/// The control [`QueueServiceOptions::control`] gives the supervisor.
#[derive(Clone)]
pub struct QueueServiceControlPort(
    pub Arc<dyn crate::application::queue_service::QueueServiceControl>,
);

impl std::fmt::Debug for QueueServiceControlPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("QueueServiceControlPort")
    }
}

/// How the supervisor keeps the queue's broker
/// ([`SuperviseOptions::queue_broker`](super::SuperviseOptions::queue_broker)).
#[derive(Debug, Clone)]
pub struct QueueBrokerOptions {
    /// The binary the broker runs: the supervisor's own.
    pub executable: PathBuf,
    pub interval: Duration,
    pub start_timeout: Duration,
    /// Starts, looks at and stops the broker instead of the system's
    /// control of `executable`; tests set it.
    pub control: Option<QueueBrokerControlPort>,
}

/// The control [`QueueBrokerOptions::control`] gives the supervisor.
#[derive(Clone)]
pub struct QueueBrokerControlPort(
    pub Arc<dyn crate::application::queue_broker::QueueBrokerControl>,
);

impl std::fmt::Debug for QueueBrokerControlPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("QueueBrokerControlPort")
    }
}

/// How the supervisor keeps the relays of the runs' containers
/// ([`SuperviseOptions::broker_relay`](super::SuperviseOptions::broker_relay)).
#[derive(Debug, Clone)]
pub struct QueueRelayOptions {
    pub interval: Duration,
    /// Looks at and starts the relays instead of podman's in the dagq
    /// machine; tests set it.
    pub control: Option<QueueRelayControlPort>,
}

/// The control [`QueueRelayOptions::control`] gives the supervisor.
#[derive(Clone)]
pub struct QueueRelayControlPort(pub Arc<dyn crate::application::broker_relay::RelayControl>);

impl std::fmt::Debug for QueueRelayControlPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("QueueRelayControlPort")
    }
}

/// How the supervisor records the host's load (task 516).
#[derive(Debug, Clone)]
pub struct HostMetricsSettings {
    /// How often a sample is taken.
    pub interval: Duration,
    /// How many local days of files are kept, today's included; zero
    /// removes none.
    pub retention_days: u32,
    /// Takes one sample at a unix second, its processes split by the kinds
    /// of the profiles given (those the main checkout's `dagq.toml`
    /// declares, read again for each sample); tests set it.
    pub sample:
        fn(i64, &crate::domain::toolchain::Toolchains) -> crate::domain::host_metrics::HostSample,
    /// Reads the space of the filesystem of a path, put in each sample for
    /// the queue's `runs/` (task 1371); tests set it.
    pub disk: fn(&Path) -> Option<crate::domain::host_metrics::DiskSpace>,
    /// How often the build outputs of the runs that wait are measured into
    /// a sample (task 1382); tests set it.
    pub held_targets_interval: Duration,
}

impl HostMetricsSettings {
    /// The host's own sample every `interval`, kept `retention_days`.
    pub fn new(interval: Duration, retention_days: u32) -> Self {
        Self {
            interval,
            retention_days,
            sample: crate::infrastructure::host_metrics::sample,
            disk: crate::infrastructure::adapters::disk_space,
            held_targets_interval: Duration::from_secs(
                crate::domain::host_metrics::DEFAULT_HELD_TARGETS_INTERVAL_SECS,
            ),
        }
    }
}

/// The index [`SuperviseOptions::release_index`](super::SuperviseOptions::release_index) gives the release check.
#[derive(Clone)]
pub struct ReleaseIndexPort(pub Arc<dyn crate::application::release_update::ReleaseIndex>);

impl std::fmt::Debug for ReleaseIndexPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReleaseIndexPort")
    }
}

/// The inbox's command `dagq inbox` starts, on one line: `claude`
/// with the inbox's settings (written under the queue's directory), its
/// prompt (see [`lifecycle::inbox_session_prompt`]) and the plugin
/// directory, made by the provider the executor starts the inbox with.
pub fn inbox_command(
    db: &Path,
    claude: &Path,
    plugin_dir: Option<&Path>,
    language: Option<&crate::domain::language::Language>,
) -> Result<String> {
    let prompt = lifecycle::inbox_session_prompt(db, language)?;
    let command = ClaudeCode {
        executable: claude.to_owned(),
    }
    .inbox_command(
        &prompt,
        plugin_dir,
        db.parent().unwrap_or(Path::new(".")),
        Path::new("."),
    )?;
    crate::application::actor_executor::command_line(&command)
}

/// Where the queue of `location` lives, as `up` and `down` take it.
fn queue_paths(location: &QueueLocation) -> QueuePaths {
    QueuePaths {
        db: location.db.clone(),
        hash: location.hash(),
        label: location.label.clone(),
        launch_agent: location.launch_agent.clone(),
        log_dir: location.log_dir.clone(),
    }
}

/// The Git common directory a repository queue at `location` is bound to,
/// which the service checks the queue's binding against before it answers,
/// as the command line on the DB checked it; `None` for a `--db` queue.
fn binding_of(location: &QueueLocation) -> Result<Option<String>> {
    location
        .git_common_dir
        .as_deref()
        .map(path_text)
        .transpose()
}

fn inspect_repository(repo: &Path) -> Result<RepositoryPaths> {
    let repository = GitRepository::inspect(repo)?;
    let landing = repository
        .repository_settings()
        .map_err(|error| format!("{error:#}"));
    let dagq_source = repository.is_dagq_source();
    let checkout = repository
        .checkout()
        .map(Path::to_path_buf)
        .map_err(|error| format!("{error:#}"));
    Ok(RepositoryPaths {
        dagq_source,
        checkout,
        root: repository.root,
        common_dir: repository.common_dir,
        landing,
    })
}

/// The tracing target of the record [`trace_command`] writes: under
/// [`telemetry::FILE_ONLY_TARGET`], so the record goes to the process's
/// JSON Lines file and not to stderr, where the command's own output is
/// unchanged.
///
/// [`telemetry::FILE_ONLY_TARGET`]: crate::infrastructure::telemetry::FILE_ONLY_TARGET
pub const COMMAND_TARGET: &str = "dagq::telemetry::command";

/// Record what `rebind`, `up` or `down` did through tracing (ADR-0033
/// decision 2), next to `rebind.jsonl`, which `rebind` keeps writing: its
/// `outcome` (for `up`, the supervisor's and the inbox's) and its whole
/// report as JSON, or the error that stopped it (a refused `rebind` too).
/// A failure before the use case (inspecting the repository, reaching
/// `rebind`'s queue) returns early, leaving only
/// the `dagq::telemetry::exit` record `main` writes. The host's `rebind`
/// through the queue service records its answer, a refusal or a failure
/// of the service's included, as the service does in its own log.
pub fn trace_command(command: &str, result: &Result<Value>) {
    match result {
        Ok(report) => {
            let outcome = report["outcome"]
                .as_str()
                .or_else(|| report["supervisor"]["outcome"].as_str())
                .unwrap_or("finished");
            let inbox = report["inbox"]["outcome"].as_str();
            tracing::info!(
                target: COMMAND_TARGET,
                command,
                outcome,
                inbox,
                report = %report,
                "dagq {command} finished: {outcome}"
            );
        }
        Err(error) => tracing::warn!(
            target: COMMAND_TARGET,
            command,
            error = %format_args!("{error:#}"),
            "dagq {command} failed: {error:#}"
        ),
    }
}

/// `up` on the system clock: see [`OneShot::up`].
pub fn up(
    location: &QueueLocation,
    repo: &Path,
    launchd: &dyn LaunchAgent,
    processes: &dyn ProcessControl,
    environment: &UpEnvironment,
    options: &UpOptions,
) -> Result<Value> {
    OneShot::system().up(location, repo, launchd, processes, environment, options)
}

/// `down` on the system clock: see [`OneShot::down`].
pub fn down(
    location: &QueueLocation,
    launchd: &dyn LaunchAgent,
    processes: &dyn ProcessControl,
    options: &DownOptions,
) -> Result<Value> {
    OneShot::system().down(location, launchd, processes, options)
}

/// `dagq inbox` on the system clock: see [`OneShot::inbox`].
pub fn inbox(
    location: &QueueLocation,
    repo: &Path,
    environment: &UpEnvironment,
    options: &InboxOptions,
) -> Result<CommandSpec> {
    OneShot::system().inbox(location, repo, environment, options)
}

/// The programs the `[run.env]` of the `dagq.toml` in `checkout` names,
/// resolved on `path`, the PATH of `up` and of the supervisor it starts
/// (ADR-0049 decision 9).
fn up_run_env_programs(
    checkout: &Path,
    db: &Path,
    path: &str,
) -> Result<crate::domain::run_env::RunEnvCheck> {
    crate::infrastructure::run_env::check_run_env_programs(
        checkout,
        db.parent().context("queue database has no directory")?,
        None,
        Some(std::ffi::OsStr::new(path)),
    )
}

/// The arguments `install` gives the `up` that starts a drained supervisor
/// again (`--claude`, `--codex`, `--plugin-dir` when given, and no `--cmux`,
/// ADR-t2159-1 decision 6): a provider's path as given, a link kept and a
/// gone one found again by its name (ADR-t2079-1); a Codex not found is
/// passed as given.
pub fn install_restart_arguments(
    claude: Option<&Path>,
    codex: Option<&Path>,
    plugin_dir: Option<&Path>,
) -> Result<Vec<String>> {
    let mut restart = Vec::new();
    if let Some(claude) = claude {
        restart.extend([
            "--claude".to_owned(),
            path_text(&crate::infrastructure::adapters::claude_at_entry(claude)?)?,
        ]);
    }
    if let Some(codex) = codex {
        let codex = crate::infrastructure::codex::codex_at_entry(codex)
            .unwrap_or_else(|_| codex.to_owned());
        restart.extend(["--codex".to_owned(), path_text(&codex)?]);
    }
    if let Some(plugin_dir) = plugin_dir {
        restart.extend(["--plugin-dir".to_owned(), path_text(plugin_dir)?]);
    }
    Ok(restart)
}

/// `rebind` on the system clock: see [`OneShot::rebind`].
pub fn rebind(db: &Path, repo: &Path) -> Result<Value> {
    OneShot::system().rebind(db, repo)
}

/// The Git common directory of the repository containing `repo`, which
/// `rebind` binds the queue to: read by the caller, as no Git runs in the
/// queue's operation (ADR-t3102-3 decision 5).
pub fn repository_common_dir(repo: &Path) -> Result<PathBuf> {
    Ok(GitRepository::inspect(repo)?.common_dir)
}

/// The queue directory the data home of this process gives a queue of the
/// repository whose Git common directory is `common_dir`, as a queue is
/// found from a checkout; none without a data home.
pub fn repository_queue_dir(common_dir: &Path) -> Option<PathBuf> {
    data_home()
        .ok()
        .map(|home| QueueLocation::for_repository(common_dir, &home).queue_dir)
}

/// The answer of `rebind`'s queue part with the run worktrees it lists
/// repaired from the repository containing `repo`, by the caller after the
/// queue is bound ([`rebinding::repair_worktrees`]).
pub fn repair_rebound_worktrees(repo: &Path, answer: Value) -> Result<Value> {
    let repository = GitRepository::inspect(repo)?;
    Ok(rebinding::repair_worktrees(&repository, answer))
}

/// `dagq service status` of the queue at `db`, which opens no queue
/// ([`host_status`](crate::application::queue_service::host_status)): the
/// service's record and socket under its directory, read through `client`.
pub fn queue_service_status(db: &Path, client: &service::Client) -> Value {
    crate::application::queue_service::host_status(&service::HostServiceStatus {
        queue_dir: db.parent().unwrap_or(Path::new(".")),
        client,
    })
}

/// `dagq service start`: see
/// [`crate::application::queue_service::start_by_hand`], with `executable`:
/// its record goes to the service it started, as the principal the
/// environment `env` names (the user with none), and the queue at `db`,
/// bound to `binding`, is not opened.
pub fn queue_service_start(
    db: &Path,
    executable: &Path,
    binding: Option<&str>,
    env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
) -> Result<Value> {
    let log = lifecycle_socket_opener(db, binding, ControlCommand::ServiceStart, env, keep_log);
    crate::application::queue_service::start_by_hand(
        &*log,
        &crate::infrastructure::queue_service::SystemQueueService::new(db, executable),
        crate::application::lifecycle::QUEUE_SERVICE_START_TIMEOUT,
    )
}

/// `dagq service stop`: see
/// [`crate::application::queue_service::stop_by_hand`], its record sent to
/// the service before it stops as [`queue_service_start`]'s.
pub fn queue_service_stop(
    db: &Path,
    binding: Option<&str>,
    env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
) -> Result<Value> {
    let log = lifecycle_socket_opener(db, binding, ControlCommand::ServiceStop, env, keep_log);
    crate::application::queue_service::stop_by_hand(
        &*log,
        &crate::infrastructure::queue_service::SystemQueueService::new(db, Path::new("dagq")),
        crate::application::lifecycle::QUEUE_SERVICE_START_TIMEOUT,
    )
}

/// Where `up`, `down`, `dagq inbox` and `service start|stop` send the
/// operations of the queue they make ([`OneShot::lifecycle_route`]).
#[derive(Debug, Clone, Copy)]
pub enum LifecycleRoute {
    /// The queue service's socket, each operation a step of the command
    /// ([`UseCase::Control`](crate::domain::queue_service::UseCase::Control))
    /// as the principal of the token the environment read by the function
    /// names (`DAGQ_SERVICE_CREDENTIAL_FILE`, `DAGQ_ROLE`), or the user's
    /// with none: the process opens no queue, and a service that does not
    /// answer is the command's failure (fail closed).
    Socket(fn(&str) -> Option<String>),
    /// The service's use cases run in this process, each step made and
    /// read back as on the socket, as the user's: a test's command that
    /// keeps no service.
    InProcess,
}

/// The queue of a lifecycle command's steps as the ports `P`, made again at
/// each open: the steps' caller is made only when the queue is first
/// needed, so `up` and `service start` reach the service they started.
struct LifecycleOpener<P: ?Sized> {
    queues: Box<dyn Fn() -> Result<SupervisorQueues> + Send + Sync>,
    keep: fn(SupervisorQueue) -> Box<P>,
}

impl<P: ?Sized> QueueOpener<P> for LifecycleOpener<P> {
    fn open(&self) -> Result<Box<P>> {
        Ok((self.keep)((self.queues)()?.queue()))
    }
}

fn keep_log(queue: SupervisorQueue) -> Box<dyn RunLog + Send> {
    Box::new(queue)
}

fn keep_lifecycle(queue: SupervisorQueue) -> Box<dyn LifecycleQueue + Send> {
    Box::new(queue)
}

/// The step of `command` this process makes.
fn lifecycle_call(command: ControlCommand) -> ControlCall {
    ControlCall {
        command,
        task: None,
        yes: false,
        pid: std::process::id(),
    }
}

/// The queue of `command`'s steps on the socket of the service of the queue
/// at `db`, bound to `binding`, as the principal `env` names
/// ([`LifecycleRoute::Socket`]).
fn lifecycle_socket_opener<P: ?Sized + 'static>(
    db: &Path,
    binding: Option<&str>,
    command: ControlCommand,
    env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    keep: fn(SupervisorQueue) -> Box<P>,
) -> Arc<dyn QueueOpener<P>> {
    let (db, binding) = (db.to_path_buf(), binding.map(str::to_owned));
    Arc::new(LifecycleOpener {
        queues: Box::new(move || {
            super::lifecycle_socket(
                service::Client::host(&db, binding.as_deref(), &env),
                lifecycle_call(command),
            )
        }),
        keep,
    })
}

/// `init`: create the queue at `db`, naming the binary at `executable`
/// its binary ([`queue_binary`]), or open the one there.
pub fn init_queue(db: &Path, executable: &Path) -> Result<SqliteQueue> {
    let created = !db.is_file();
    let queue = SqliteQueue::init(db)?;
    if created {
        queue_binary::write(db, &QueueBinary::new(executable))?;
    }
    Ok(queue)
}

/// Refuse a binary at `executable` that is not the one changing the
/// schema of the queue at `db` ([`queue_binary::check`]) when it would
/// apply a migration; a queue it cannot read is left to the migration's
/// own error.
fn check_schema_changer(db: &Path, executable: &Path) -> Result<()> {
    match SqliteQueue::schema(db) {
        Ok(state) if !state.pending.is_empty() => queue_binary::check(
            db,
            executable,
            &crate::infrastructure::adapters::process_alive,
        ),
        _ => Ok(()),
    }
}

/// `migrate --check`: the schema of the queue at `db`, read-only.
pub fn queue_schema(db: &Path) -> Result<crate::infrastructure::sqlite::SchemaState> {
    SqliteQueue::schema(db)
}

/// `migrate`: bring the queue at `db` to this binary's schema, with the
/// live processes judged on this host, when the binary at `executable` is
/// the one changing its schema ([`queue_binary`]).
pub fn migrate_queue(
    db: &Path,
    executable: &Path,
    now: i64,
) -> Result<crate::infrastructure::sqlite::MigrationReport> {
    check_schema_changer(db, executable)?;
    SqliteQueue::migrate(
        db,
        Some(&crate::infrastructure::adapters::process_alive),
        now,
    )
}

impl OneShot {
    /// `rebind`: bind the queue at `db` to the repository containing `repo`
    /// (see [`rebinding::rebind`]), then repair the run worktrees it lists.
    pub fn rebind(&self, db: &Path, repo: &Path) -> Result<Value> {
        let result = (|| {
            let db = db
                .canonicalize()
                .context("queue must already be initialized")?;
            let common_dir = repository_common_dir(repo)?;
            let mut queue = self.open(&db)?;
            let answer = self.rebind_on(
                &mut queue,
                &db,
                &common_dir,
                repository_queue_dir(&common_dir),
            )?;
            repair_rebound_worktrees(repo, answer)
        })();
        trace_command("rebind", &result);
        result
    }

    /// The queue's part of [`Self::rebind`] on `queue`, the queue at `db`,
    /// opened already and written as its actor: bound to the repository
    /// whose Git common directory is `common_dir`. It runs no Git; its
    /// caller repairs the worktrees it lists ([`repair_rebound_worktrees`]).
    fn rebind_on(
        &self,
        queue: &mut SqliteQueue,
        db: &Path,
        common_dir: &Path,
        repository_queue_dir: Option<PathBuf>,
    ) -> Result<Value> {
        let db = db
            .canonicalize()
            .context("queue must already be initialized")?;
        let location = QueueLocation::explicit(&db);
        rebinding::rebind(
            Rebind {
                queue,
                files: &LocalRunFiles,
                processes: &SystemProcesses,
                clock: &*self.generators.clock,
            },
            RebindTarget {
                repository_file: location.queue_dir.join(REPOSITORY_FILE_NAME),
                db,
                common_dir: path_text(common_dir)?,
                queue_dir: location.queue_dir,
                log_dir: location.log_dir,
                repository_queue_dir,
            },
        )
    }

    /// `up`: see [`lifecycle::up`]. `repo` is any checkout of the repository.
    #[allow(clippy::too_many_arguments)]
    pub fn up(
        &self,
        location: &QueueLocation,
        repo: &Path,
        launchd: &dyn LaunchAgent,
        processes: &dyn ProcessControl,
        environment: &UpEnvironment,
        options: &UpOptions,
    ) -> Result<Value> {
        let claude = ClaudeCode {
            executable: options.claude.clone(),
        };
        let binding = binding_of(location)?;
        let queues = |db: &Path| {
            self.lifecycle_opener(
                db,
                binding.as_deref(),
                ControlCommand::Up,
                Some(&environment.current_exe),
                keep_lifecycle,
            )
        };
        // The service `up` starts refuses a queue bound to another
        // repository before it takes over or migrates: `up` opens none to
        // look.
        let bound = binding.clone();
        let service = move |db: &Path, executable: &Path| -> Box<dyn QueueServiceControl> {
            Box::new(
                crate::infrastructure::queue_service::SystemQueueService::new(db, executable)
                    .bound_to(bound.as_deref()),
            )
        };
        let mut ports = self.lifecycle_ports(launchd, processes, &queues);
        ports.queue_service = &service;
        let result = lifecycle::up(
            &ports,
            &claude,
            &queue_paths(location),
            repo,
            environment,
            options,
        );
        trace_command("up", &result);
        result
    }

    /// `down`: see [`lifecycle::down`].
    pub fn down(
        &self,
        location: &QueueLocation,
        launchd: &dyn LaunchAgent,
        processes: &dyn ProcessControl,
        options: &DownOptions,
    ) -> Result<Value> {
        let binding = binding_of(location)?;
        let queues = |db: &Path| {
            self.lifecycle_opener(
                db,
                binding.as_deref(),
                ControlCommand::Down,
                None,
                keep_lifecycle,
            )
        };
        let result = lifecycle::down(
            &self.lifecycle_ports(launchd, processes, &queues),
            &queue_paths(location),
            options,
        );
        trace_command("down", &result);
        result
    }

    /// `dagq inbox`: see [`lifecycle::inbox`], with Claude Code at
    /// `options.agent` (found again by its name when the path given is
    /// gone) as the inbox's agent. Returns the command for the caller to
    /// exec.
    pub fn inbox(
        &self,
        location: &QueueLocation,
        repo: &Path,
        environment: &UpEnvironment,
        options: &InboxOptions,
    ) -> Result<CommandSpec> {
        let claude = ClaudeCode {
            executable: crate::infrastructure::adapters::claude_at_entry(&options.agent)
                .unwrap_or_else(|_| options.agent.clone()),
        };
        let codex = crate::infrastructure::codex::Codex::new(
            crate::infrastructure::codex::codex_at_entry(&options.codex)
                .unwrap_or_else(|_| options.codex.clone()),
        );
        let binding = binding_of(location)?;
        let queues = |db: &Path| {
            self.lifecycle_opener(
                db,
                binding.as_deref(),
                ControlCommand::Inbox,
                None,
                keep_log,
            )
        };
        lifecycle::inbox(
            &InboxPorts {
                files: &LocalRunFiles,
                queues: &queues,
                inspect_repository: &inspect_repository,
                resolve_language: &|checkout, user_config| {
                    crate::infrastructure::language::resolve_language(Some(checkout), user_config)
                },
                queue_service: &crate::infrastructure::queue_service::RequestedServiceAccess::user(
                    &location.db,
                    environment.role.as_deref(),
                ),
                session: {
                    use crate::application::ProcessControl;
                    let pid = std::process::id();
                    crate::infrastructure::adapters::SystemProcesses
                        .session_start(pid)
                        .map(|start| crate::domain::queue_service::SessionProcess { pid, start })
                },
                roles: &crate::infrastructure::run_env::load_inbox_role,
                codex_settings: &crate::infrastructure::codex_inbox::check_this_user_config,
                codex_usable: &crate::infrastructure::codex_inbox::check_this_codex,
                claude_forbidden: &|db: &Path| {
                    let supervisors = self
                        .lifecycle_opener(
                            db,
                            binding.as_deref(),
                            ControlCommand::Inbox,
                            None,
                            keep_lifecycle,
                        )
                        .open()?
                        .supervisors()?;
                    Ok(lifecycle::claude_forbidden(
                        &supervisors,
                        &crate::infrastructure::adapters::SystemProcesses,
                    ))
                },
            },
            &InboxAgents {
                claude: &claude,
                codex: &codex,
            },
            &location.db,
            repo,
            environment,
            &InboxOptions {
                agent: claude.executable.clone(),
                codex: codex.executable.clone(),
                ..options.clone()
            },
        )
    }

    /// `install`: replace the fixed binary and hand the queue's supervisor
    /// over to it (see [`installation::install`]), then watch the
    /// supervisors heartbeat on under it for `watch_timeout` and, when they
    /// do not, put the old binary back, start them again and ask the inbox
    /// (see [`update::install_watched`], ADR-0073 decisions 13 and 14).
    pub fn install(
        &self,
        location: &QueueLocation,
        launchd: &dyn LaunchAgent,
        options: &InstallOptions,
        watch_timeout: Duration,
    ) -> Result<Value> {
        let queues = |db: &Path| self.host_queues(db);
        // A supervisor gone after a failed watch is started again with the
        // binary in place, as after the automatic update's.
        let db = location
            .db
            .canonicalize()
            .unwrap_or_else(|_| location.db.clone());
        let restart = restarter(&db, &options.target, &options.restart);
        let down = || {
            self.down(
                location,
                launchd,
                &SystemProcesses,
                &DownOptions {
                    wait: true,
                    force: false,
                    poll: Duration::from_secs(2),
                },
            )
        };
        update::install_watched(
            &update::JobPorts {
                binaries: &LocalBinaries,
                files: &LocalRunFiles,
                processes: &SystemProcesses,
                clock: &*self.generators.clock,
                queues: &queues,
                restart: &restart,
                is_ancestor: &update::no_ancestry,
            },
            &down,
            Some(&location.db),
            options,
            watch_timeout,
        )
    }

    /// The automatic update's job (see [`update::run`]): build
    /// `job.commit` in the queue's update checkout and put it in place of
    /// `job.target`, handing the supervisor `job.token` over to it. A
    /// supervisor registered in the retired in-cmux mode that is gone
    /// afterwards is started again with the binary in place, through `up
    /// --auto-update` under launchd; launchd restarts one of its own.
    pub fn auto_update(&self, location: &QueueLocation, job: &AutoUpdateJob) -> Result<Value> {
        let db = location
            .db
            .canonicalize()
            .context("queue must already be initialized")?;
        let queues = |db: &Path| self.host_queues(db);
        let restart_arguments =
            restart_arguments(&job.claude, &job.codex, job.plugin_dir.as_deref())?;
        let restart = update_restarter(&db, &restart_arguments);
        let paths = update::UpdatePaths::under(&location.queue_dir);
        let e2e = installation::E2eSettings {
            command: job.e2e_command.clone(),
            timeout: job.e2e_timeout,
            // The runtime reads `[run.env]` from the main checkout's
            // `dagq.toml`, as for a run.
            run_env_root: Some(job.repository.clone()),
            queue_dir: Some(location.queue_dir.clone()),
            scratch: paths.e2e.clone(),
            log: e2e_log(&job.log),
            utc_offset_secs: clock::local_utc_offset(self.generators.clock.now()),
            lock: installation::e2e_lock_path(&location.queue_dir),
            git_env: Vec::new(),
            ended: None,
        };
        update::run(
            &update::JobPorts {
                binaries: &LocalBinaries,
                files: &LocalRunFiles,
                processes: &SystemProcesses,
                clock: &*self.generators.clock,
                queues: &queues,
                restart: &restart,
                is_ancestor: &|ancestor, descendant| {
                    GitRepository::inspect(&job.repository)?.is_ancestor(ancestor, descendant)
                },
            },
            &db,
            &update::JobOptions {
                commit: job.commit.clone(),
                token: job.token.clone(),
                target: job.target.clone(),
                repository: job.repository.clone(),
                paths,
                log: job.log.clone(),
                build_command: job.build_command.clone(),
                e2e,
                restart: restart_arguments.clone(),
                handoff_timeout: job.handoff_timeout,
                watch_timeout: job.watch_timeout,
                poll: job.poll,
                pid: std::process::id(),
            },
        )
    }

    /// The release update's job (see [`update::run_release`]): install
    /// release `job.version` with cargo under the queue's update directory
    /// and put it in place of `job.target`, handing the supervisor
    /// `job.token` over to it; a supervisor gone afterwards is started
    /// again as [`Self::auto_update`] does.
    pub fn release_update(
        &self,
        location: &QueueLocation,
        job: &ReleaseUpdateJob,
    ) -> Result<Value> {
        let db = location
            .db
            .canonicalize()
            .context("queue must already be initialized")?;
        let queues = |db: &Path| self.host_queues(db);
        let restart_arguments =
            restart_arguments(&job.claude, &job.codex, job.plugin_dir.as_deref())?;
        let restart = update_restarter(&db, &restart_arguments);
        // Where the supervisor started it: its sessions' repository.
        let plugin = job.plugin_dir.is_none().then(|| ClaudePlugin {
            executable: job.claude.clone(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        });
        update::run_release(
            &update::JobPorts {
                binaries: &LocalBinaries,
                files: &LocalRunFiles,
                processes: &SystemProcesses,
                clock: &*self.generators.clock,
                queues: &queues,
                restart: &restart,
                is_ancestor: &update::no_ancestry,
            },
            &crate::infrastructure::binaries::CargoInstaller {
                program: job.cargo.clone(),
            },
            plugin
                .as_ref()
                .map(|plugin| plugin as &dyn crate::application::InstalledPlugin),
            &db,
            &update::ReleaseJobOptions {
                version: job.version.clone(),
                token: job.token.clone(),
                target: job.target.clone(),
                paths: update::UpdatePaths::under(&location.queue_dir),
                log: job.log.clone(),
                restart: restart_arguments.clone(),
                handoff_timeout: job.handoff_timeout,
                watch_timeout: job.watch_timeout,
                poll: Duration::from_millis(500),
                pid: std::process::id(),
                plugin_only: job.plugin_only,
            },
        )
    }

    /// `install --release [<version>]` (ADR-t618-1 decision 6): the
    /// release (`requested`, else the newest crates.io lists), installed
    /// with `cargo` under the queue's `update/release` unless `options.target`
    /// is that release already, then [`Self::install`] from it.
    #[allow(clippy::too_many_arguments)]
    pub fn install_release(
        &self,
        location: &QueueLocation,
        launchd: &dyn LaunchAgent,
        index: &dyn crate::application::release_update::ReleaseIndex,
        cargo: &Path,
        requested: Option<&str>,
        options: &InstallOptions,
        watch_timeout: Duration,
    ) -> Result<Value> {
        let version = crate::application::release_update::resolve_version(index, requested)?;
        let paths = update::UpdatePaths::under(&location.queue_dir);
        let log = location
            .queue_dir
            .join("logs")
            .join(format!("install-release-{version}.log"));
        // The queue's update lock, held while cargo uses `paths.target`.
        let binary = update::hold_target(&LocalRunFiles, &paths)
            .and_then(|_target| {
                installation::release_binary(
                    &LocalBinaries,
                    &crate::infrastructure::binaries::CargoInstaller {
                        program: cargo.to_path_buf(),
                    },
                    &version,
                    &options.target,
                    &paths.release,
                    &paths.target,
                    &log,
                )
            })
            .with_context(|| format!("install release {version}"))?;
        let mut report = self.install(
            location,
            launchd,
            &InstallOptions {
                source: installation::Source::Binary(binary),
                ..options.clone()
            },
            watch_timeout,
        )?;
        report["release"] = json!(version);
        report["log"] = json!(log);
        Ok(report)
    }

    /// The queue at a path, writing through these generators.
    fn opener(&self, db: &Path) -> SqliteOpener {
        SqliteOpener {
            db: db.to_path_buf(),
            generators: self.generators.clone(),
            actor: None,
        }
    }

    /// The queue at `db`, bound to `binding`, as the ports `P` of
    /// `command`'s steps, on [`Self::lifecycle_route`] (in this process, as
    /// a command that runs `executable` when given): the queue `up`,
    /// `down` and `dagq inbox` take, which they reach only through the
    /// queue service's use cases.
    fn lifecycle_opener<P: ?Sized + 'static>(
        &self,
        db: &Path,
        binding: Option<&str>,
        command: ControlCommand,
        executable: Option<&Path>,
        keep: fn(SupervisorQueue) -> Box<P>,
    ) -> Arc<dyn QueueOpener<P>> {
        match self.lifecycle_route {
            LifecycleRoute::Socket(env) => lifecycle_socket_opener(db, binding, command, env, keep),
            LifecycleRoute::InProcess => {
                let (db, generators) = (db.to_path_buf(), self.generators.clone());
                let executable = executable.map(Path::to_path_buf);
                Arc::new(LifecycleOpener {
                    queues: Box::new(move || {
                        super::lifecycle_in_process(
                            &db,
                            generators.clone(),
                            lifecycle_call(command),
                            executable.as_deref(),
                        )
                    }),
                    keep,
                })
            }
        }
    }

    /// The queue at a path as the ports `install` and the update jobs take.
    fn host_queues(&self, db: &Path) -> Arc<dyn QueueOpener<dyn HostOpsQueue + Send>> {
        Arc::new(SqlitePorts {
            opener: self.opener(db),
            keep: |queue| -> Box<dyn HostOpsQueue + Send> { Box::new(queue) },
        })
    }

    /// The adapters `up` and `down` run on: the queue at a path through
    /// the queue service (`queues`), Git for the repository, the
    /// local files and Claude Code's global config for the folder trust.
    fn lifecycle_ports<'a>(
        &'a self,
        launchd: &'a dyn LaunchAgent,
        processes: &'a dyn ProcessControl,
        queues: &'a dyn Fn(&Path) -> Arc<dyn QueueOpener<dyn LifecycleQueue + Send>>,
    ) -> LifecyclePorts<'a> {
        LifecyclePorts {
            launchd,
            processes,
            files: &LocalRunFiles,
            clock: &*self.generators.clock,
            queues,
            inspect_repository: &inspect_repository,
            trusts_repository: &claude_trusts_repository,
            run_env_programs: &up_run_env_programs,
            ci_watch_preflight: &|checkout, path| {
                crate::infrastructure::ci_watch::preflight(checkout, Some(path.into()))
            },
            resolve_language: &|checkout, user_config| {
                crate::infrastructure::language::resolve_language(Some(checkout), user_config)
            },
            queue_service: &|db, executable| {
                Box::new(
                    crate::infrastructure::queue_service::SystemQueueService::new(db, executable),
                )
            },
            queue_broker: &|db, executable| {
                Box::new(crate::infrastructure::queue_broker::SystemQueueBroker::new(
                    db, executable,
                ))
            },
            revoke_ended: &|db, principal| {
                crate::infrastructure::queue_service::RequestedServiceAccess::of_caller(db)
                    .revoke_ended(principal)
            },
        }
    }
}

/// The release check's port of the supervisor (ADR-t618-1): the host's
/// `[update]` of the queue at `db` (its `host.toml` over `host_wide`) read
/// again at each look, never from dagq.toml (decision 3); crates.io's index
/// through `index` (`curl` without one); the plugin `claude` installed for
/// `checkout` when `plugin` (a `--plugin-dir` supervisor's is not touched);
/// and `current` as the supervisor's build (this binary's without one).
pub(super) fn release_port(
    db: &Path,
    checkout: &Path,
    claude: &Path,
    host_wide: Option<PathBuf>,
    plugin: bool,
    index: Option<&ReleaseIndexPort>,
    current: Option<String>,
) -> crate::application::supervise::ReleasePort {
    let queue_dir = db.parent().unwrap_or(Path::new(".")).to_path_buf();
    crate::application::supervise::ReleasePort {
        config: Arc::new(move || {
            crate::infrastructure::release_update::load_host_update(
                &queue_dir,
                host_wide.as_deref(),
            )
            .config
        }),
        index: index.map_or_else(
            || {
                Arc::new(crate::infrastructure::release_update::CurlIndex::default())
                    as Arc<dyn crate::application::release_update::ReleaseIndex>
            },
            |port| port.0.clone(),
        ),
        plugin: plugin.then(|| {
            Arc::new(ClaudePlugin {
                executable: claude.to_path_buf(),
                cwd: checkout.to_path_buf(),
            }) as Arc<dyn crate::application::InstalledPlugin>
        }),
        current: current.unwrap_or_else(|| crate::VERSION.to_owned()),
    }
}

/// The host's load records' port of the supervisor (task 516): samples
/// in the queue's NodeSampleStore (opened by `store`) and under the
/// queue's `host/`, each with the space of the filesystem of the run
/// worktrees and their builds, the one the disk checks read (task 1371):
/// `runs/`, else the queue's dir; the processes split by the profiles
/// `checkout`'s `dagq.toml` declares, resolved against `profiles`
/// ([`crate::infrastructure::run_env::toolchains_in_use`]), and in the
/// common units as `environment` measures the node; the build outputs of
/// the runs that wait are those same profiles' (task 1382).
pub(super) fn host_metrics_port(
    db: &Path,
    checkout: &Path,
    profiles: &'static crate::domain::toolchain::ProfileRegistry,
    settings: HostMetricsSettings,
    store: Arc<dyn QueueOpener<dyn crate::application::NodeSampleStore + Send>>,
    environment: Arc<dyn crate::application::ExecutionEnvironment>,
) -> crate::application::supervise::HostMetricsPort {
    let queue_dir = db.parent().unwrap_or(Path::new(".")).to_path_buf();
    let dir = queue_dir.join(crate::domain::host_metrics::HOST_DIR);
    let runs = runs_dir(db);
    let (take, disk) = (settings.sample, settings.disk);
    let checkout = checkout.to_path_buf();
    let held_checkout = checkout.clone();
    crate::application::supervise::HostMetricsPort {
        interval: settings.interval,
        retention_days: settings.retention_days,
        sample: Arc::new(move |now| {
            let toolchains = crate::infrastructure::run_env::toolchains_in_use(&checkout, profiles);
            take(now, &toolchains).with_disk(disk(&runs).or_else(|| disk(&queue_dir)))
        }),
        held_targets: Some(crate::application::supervise::HeldTargetsPort {
            interval: settings.held_targets_interval,
            // A `dagq.toml` that does not read names no directory: unknown,
            // not 0 bytes.
            measure: Arc::new(move |worktree| {
                let toolchains =
                    crate::infrastructure::run_env::load_toolchains(&held_checkout, profiles)
                        .ok()?;
                crate::infrastructure::host_metrics::build_outputs_bytes(worktree, &toolchains)
            }),
        }),
        utc_offset: clock::local_utc_offset,
        files: Arc::new(crate::infrastructure::host_metrics::HostMetricsDir(dir)),
        store,
        environment,
    }
}

/// The queue service's port of the supervisor (ADR-t1233-4 decision 2):
/// `settings`' control, else the system's control of its executable.
pub(super) fn queue_service_port(
    db: &Path,
    settings: &QueueServiceOptions,
) -> crate::application::supervise::QueueServicePort {
    crate::application::supervise::QueueServicePort {
        control: settings.control.as_ref().map_or_else(
            || {
                Arc::new(
                    crate::infrastructure::queue_service::SystemQueueService::new(
                        db,
                        &settings.executable,
                    ),
                ) as Arc<dyn crate::application::queue_service::QueueServiceControl>
            },
            |port| port.0.clone(),
        ),
        interval: settings.interval,
        start_timeout: settings.start_timeout,
    }
}

/// The control of the queue's broker: `settings`' control, else the
/// system's control of its executable (of `dagq` without settings: one
/// that is only looked at, never started).
pub(super) fn queue_broker_control(
    db: &Path,
    settings: Option<&QueueBrokerOptions>,
) -> Arc<dyn crate::application::queue_broker::QueueBrokerControl> {
    match settings {
        Some(QueueBrokerOptions {
            control: Some(port),
            ..
        }) => port.0.clone(),
        Some(settings) => Arc::new(crate::infrastructure::queue_broker::SystemQueueBroker::new(
            db,
            &settings.executable,
        )),
        None => Arc::new(crate::infrastructure::queue_broker::SystemQueueBroker::new(
            db,
            Path::new("dagq"),
        )),
    }
}

/// The container runtime's parts behind the queue broker: the runs'
/// relays as the supervisor recorded them, the Claude credential the
/// broker sends a worker's requests with (ADR-t2177-1 decision 3), the
/// dagq podman machine as the queue at `db` uses it (only looked at: a
/// container's start makes it run), the image as `main_checkout` decides
/// it (only looked at: a container's start builds it), then the parts not
/// built yet. The relays' attention, the image's records and the runs that
/// own users of the machine are read and made on `records` (the queue
/// service's, for the supervisor).
pub(super) fn container_parts(
    db: &Path,
    main_checkout: &Path,
    clock: Arc<dyn crate::application::Clock>,
    records: &super::ContainerRecords,
) -> crate::application::broker_relay::RelayGate<
    crate::infrastructure::claude_credential::ClaudeCredentialGate<
        crate::application::podman_machine::MachineGate<
            crate::application::container_image::ImageGate<
                crate::infrastructure::container_runtime::ContainerExecutors,
            >,
        >,
    >,
> {
    let queue_dir = db.parent().unwrap_or(Path::new("."));
    crate::application::broker_relay::RelayGate {
        records: records.relays.clone(),
        container: crate::infrastructure::claude_credential::ClaudeCredentialGate::of_queue(
            queue_dir,
            crate::application::podman_machine::MachineGate {
                podman: Arc::new(crate::infrastructure::podman_machine::PodmanCli::default()),
                required: crate::domain::podman_machine::MachineMounts::of_queue(queue_dir),
                room: Arc::new(
                    crate::infrastructure::podman_machine::HostMemoryRoom::under(
                        queue_dir.parent().unwrap_or(Path::new(".")),
                        crate::infrastructure::kpi_config::host_wide_file(),
                    )
                    .with_owner_runs(records.owner_runs.clone()),
                ),
                container: crate::application::container_image::ImageGate {
                    specs: Arc::new(crate::infrastructure::container_image::CheckoutImages::new(
                        main_checkout,
                        queue_dir,
                    )),
                    records: records.images.clone(),
                    now: Arc::new(move || clock.now()),
                    container: crate::infrastructure::container_runtime::ContainerExecutors,
                },
            },
        ),
    }
}

/// The relays of the runs' containers as the supervisor keeps them:
/// `settings`' control, else podman's in the dagq machine for the queue at
/// `db`, towards the queue's broker `broker`, the runs that own users of
/// the machine read through `owner_runs` (the queue service's).
pub(super) fn broker_relay_port(
    db: &Path,
    settings: &QueueRelayOptions,
    broker: Arc<dyn crate::application::queue_broker::QueueBrokerControl>,
    owner_runs: Option<Arc<dyn crate::infrastructure::podman_machine::OwnerRuns>>,
) -> crate::application::supervise::QueueRelayPort {
    crate::application::supervise::QueueRelayPort {
        relays: match &settings.control {
            Some(port) => port.0.clone(),
            None => Arc::new(
                crate::infrastructure::broker_relay::PodmanRelays::of_queue(
                    crate::infrastructure::podman_machine::PodmanCli::default(),
                    db,
                    crate::infrastructure::kpi_config::host_wide_file().as_deref(),
                )
                .with_owner_runs(owner_runs),
            ),
        },
        broker,
        interval: settings.interval,
    }
}

/// The sccache server's port of the supervisor (ADR-t1215-1): its start
/// logged under the queue's directory, and the guard of a process it gives
/// `[run.env]` the `dagq` its sessions run, `runner` (ADR-t2086-1).
pub(super) fn sccache_port(
    db: &Path,
    settings: &SccacheOptions,
    runner: &Path,
) -> crate::application::supervise::SccachePort {
    crate::application::supervise::SccachePort(Arc::new(
        crate::infrastructure::sccache::SystemSccache {
            log: db
                .parent()
                .unwrap_or(Path::new("."))
                .join("sccache-start.log"),
            start_timeout: settings.start_timeout,
            lsof: settings.lsof.clone(),
            ps: settings.ps.clone(),
            dagq: Some(runner.to_path_buf()),
        },
    ))
}

/// One of the host's dialogue and operation commands (`answer`, `hold`,
/// `rebind`, `run log`, `report`, ...) on `queue`, the queue at `db`,
/// written as its actor: how the command line runs it on the DB and how the
/// queue service runs its use case ([`service_commands`]), so its answer is
/// the same either way (ADR-t3102-2 decision 6 (4)). The runtime
/// operations and the exports are authorized before it, as
/// [`QueueCommand::authorization`] says; the dialogue, the holds and the
/// replans by the application as they run.
pub fn run_command(queue: &mut SqliteQueue, db: &Path, command: &QueueCommand) -> Result<Value> {
    use crate::application::commands::{
        dialogue::Dialogue, holds::Holds, planning::Planning, replans::Replans,
    };
    use crate::application::queue_commands::QueueCommand as C;
    use crate::application::screen::{self, RunTarget};
    use crate::domain::{AskId, EventId, PlannerId, RunId, StaticPolicy, TaskId};
    let actor = queue.actor();
    let one_shot = OneShot {
        user_config: crate::infrastructure::language::user_config_file(),
        ..OneShot::new(queue.generators().clone())
    };
    let hold_target = |target: &str| -> Result<crate::domain::task_hold::HoldTarget> {
        use crate::domain::task_hold::HoldTarget;
        let target = target.trim();
        Ok(match target.parse::<i64>() {
            Ok(id) => HoldTarget::Task(TaskId::new(id)),
            Err(_) => HoldTarget::Run(RunId::new(target)?),
        })
    };
    let planners_dir = crate::infrastructure::location::planners_dir(db);
    Ok(match command {
        // The user's own answer or the inbox's delegated one, which the
        // application records from the actor (ADR-t728-3 decision 2).
        C::Answer(params) => {
            let mut store = crate::infrastructure::dialogue::DialogueQueue { queue };
            serde_json::to_value(
                Dialogue::new(&mut store, &actor, &StaticPolicy)
                    .answer(AskId::new(params.id), &params.text)?,
            )?
        }
        C::AskClose(params) => {
            let mut store = crate::infrastructure::dialogue::DialogueQueue { queue };
            serde_json::to_value(
                Dialogue::new(&mut store, &actor, &StaticPolicy).close(AskId::new(params.id))?,
            )?
        }
        C::Hold(params) => serde_json::to_value(Holds::new(queue, &actor, &StaticPolicy).hold(
            &hold_target(&params.target)?,
            &params.reason,
            params.until,
        )?)?,
        // The work a release carries over is read and kept by the caller
        // with Git between the two (`prepare_command`); the release's
        // transaction checks the read again.
        C::ReleaseRead(params) => {
            let read = Holds::new(queue, &actor, &StaticPolicy)
                .release_read(&hold_target(&params.target)?, &release_choice(params)?)?;
            serde_json::to_value(crate::application::queue_commands::ReleaseToRead {
                read,
                repository: queue.repository_binding()?,
            })?
        }
        C::ReleaseImages(params) => {
            let now = crate::application::unix_seconds(queue.generators().clock.system_time());
            serde_json::to_value(Holds::new(queue, &actor, &StaticPolicy).record_images(
                &hold_target(&params.target)?,
                &params.records,
                now,
            )?)?
        }
        C::Release(params) => serde_json::to_value(
            Holds::new(queue, &actor, &StaticPolicy).release_read_by_caller(
                &hold_target(&params.target)?,
                release_choice(params)?,
                params.work.clone().map(|work| (work.at, work.read)),
            )?,
        )?,
        C::ReplanRequest(params) => {
            serde_json::to_value(Replans::new(queue, &actor, &StaticPolicy).request(
                &crate::domain::task_replan::NewReplan {
                    run: RunId::new(&params.run)?,
                    reason: params.reason.clone(),
                    evidence: params.evidence.iter().copied().map(EventId::new).collect(),
                },
            )?)?
        }
        C::Rebind(params) => one_shot.rebind_on(
            queue,
            db,
            params
                .git_common_dir
                .as_deref()
                .context("rebind is sent with the Git common directory its caller read")?,
            params.repository_queue_dir.clone(),
        )?,
        C::Recover(params) => crate::application::health::recover(
            queue,
            &SystemProcesses,
            &LocalRunFiles,
            &crate::infrastructure::queue_broker::SystemVerificationTokens {
                db: db.to_path_buf(),
            },
            &*one_shot.generators.clock,
            &RunId::new(&params.run)?,
        )?,
        // Neither builds an agent adapter: they read only the queue
        // (task 1437).
        C::RunScreen(params) => screen::run_screen(queue, &RunTarget::parse(&params.run)?)?,
        C::RunSend(params) => screen::run_send(queue, &RunTarget::parse(&params.run)?)?,
        C::RunLog(params) => json!({
            "session": crate::application::session_log::run_log(
                queue,
                &RunTarget::parse(&params.run)?,
            )?
            .session,
        }),
        // Neither reads a screen nor types (ADR-t1433-2).
        C::PlannerScreen(params) => {
            screen::planner_screen(queue, &planners_dir, PlannerId::new(params.planner))?
        }
        C::PlannerSend(params) => screen::planner_send(queue, PlannerId::new(params.planner))?,
        C::PlannerLog(params) => json!({
            "session": crate::application::session_log::planner_log(
                queue,
                &planners_dir,
                PlannerId::new(params.planner),
            )?
            .session,
        }),
        C::PlannerRequest(params) => one_shot.request_planner(
            queue,
            db,
            &crate::infrastructure::adapters::BackgroundSessions,
            PlannerId::new(params.planner),
            &params.text,
        )?,
        C::Report(params) => one_shot.report_of(queue, db, params)?,
        C::ReportInputs(params) => one_shot.report_inputs_of(queue, db, params)?,
        // The head is chosen and kept by the caller with Git between the
        // two (`ready_inherit`); the record's transaction checks the read
        // again.
        C::ReadyInheritRead(params) => {
            let read = Planning::new(queue, &actor, &StaticPolicy)
                .ready_inherit_read(TaskId::new(params.id), &params.reason)?;
            serde_json::to_value(crate::application::queue_commands::ReadyInheritToRead {
                read,
                repository: queue.repository_binding()?,
                queue: db.to_str().map(str::to_owned),
            })?
        }
        C::ReadyInherit(params) => {
            let work = params
                .work
                .clone()
                .context("ready --inherit is sent with the head its client chose")?;
            serde_json::to_value(
                Planning::new(queue, &actor, &StaticPolicy).ready_inherit_by_caller(
                    TaskId::new(params.id),
                    &params.reason,
                    work.at,
                    work.head,
                )?,
            )?
        }
        C::ReadyInheritImages(params) => {
            let now = crate::application::unix_seconds(queue.generators().clock.system_time());
            json!({"recorded": Planning::new(queue, &actor, &StaticPolicy)
                .ready_inherit_images(TaskId::new(params.id), &params.records, now)?})
        }
        C::GraphExport(params) => {
            let (source, tasks) = crate::application::queue_reads::planning::graph_diagram(
                queue,
                params.goal.map(crate::domain::GoalId::new),
            )?;
            json!({"source": source, "tasks": tasks})
        }
    })
}

/// The provider and model `release`'s params choose.
fn release_choice(
    params: &crate::application::queue_commands::ReleaseParams,
) -> Result<crate::domain::task_hold::ReleaseChoice> {
    Ok(crate::domain::task_hold::ReleaseChoice {
        provider: params.provider.as_deref().map(str::parse).transpose()?,
        model: params.model.clone(),
    })
}

/// `release`'s params as its caller sends them
/// ([`crate::application::commands::holds::prepare_release`]), from what
/// `call` answers (the service, or [`run_command`] on the DB of the queue
/// at `db`), with the host's Git for the main checkout and the run's work.
/// A caller of the queue at `db` runs a facts-only turn of a run in a
/// container in the queue's containers, which keep the image's records for
/// it to send; one that knows no queue (`db` is `None`: a client-mode
/// `dagq`) reads the run's work with no container of the queue
/// ([`UnknownQueueContainers`](crate::infrastructure::run_container::UnknownQueueContainers)).
pub(super) fn prepare_release(
    params: &mut crate::application::queue_commands::ReleaseParams,
    db: Option<&Path>,
    call: impl FnMut(&QueueCommand) -> Result<Value>,
) -> Result<()> {
    use crate::application::commands::holds::{self, ReleaseClient};
    use crate::infrastructure::adapters::RunRepositories;
    holds::prepare_release(
        params,
        &ReleaseClient {
            main_checkout: &crate::infrastructure::adapters::main_checkout_of,
            branches: &|main, read| match db {
                Some(db) => {
                    let kept =
                        Arc::new(crate::application::container_image::KeptImageRecords::default());
                    let containers = super::run_containers_keeping(db, main, Some(kept.clone()));
                    read(&RunRepositories {
                        containers: &containers,
                    });
                    kept.take()
                }
                None => {
                    read(&RunRepositories {
                        containers: &crate::infrastructure::run_container::UnknownQueueContainers,
                    });
                    Vec::new()
                }
            },
        },
        call,
    )
}

/// A person's `ready --inherit` of `params` as its caller runs it
/// ([`crate::application::commands::planning::ready_inherit`]), asking
/// `call` (the service, or [`run_command`] on the DB of the queue at `db`),
/// with the host's Git for the main checkout and the run's head. A
/// facts-only turn of a run in a container runs in the containers of the
/// queue the read answered for (or the one at `db`), which keep the
/// image's records for it to send: a client-mode `dagq`, which knows no
/// queue path, runs it too and opens no DB.
pub(super) fn ready_inherit(
    params: &crate::application::queue_commands::ReadyInheritParams,
    db: Option<&Path>,
    call: impl FnMut(&QueueCommand) -> Result<Value>,
    refused: impl Fn(&anyhow::Error) -> bool,
) -> Result<Value> {
    use crate::application::commands::planning::{self, InheritClient};
    planning::ready_inherit(
        params,
        &InheritClient {
            main_checkout: &crate::infrastructure::adapters::main_checkout_of,
            git: &|main, queue, carry| match queue.as_deref().or(db) {
                Some(db) => {
                    let kept =
                        Arc::new(crate::application::container_image::KeptImageRecords::default());
                    let containers = super::run_containers_keeping(db, main, Some(kept.clone()));
                    carry(
                        &crate::infrastructure::adapters::RunRepositories {
                            containers: &containers,
                        },
                        &|| kept.take(),
                    );
                }
                None => carry(
                    &crate::infrastructure::adapters::RunRepositories {
                        containers: &crate::infrastructure::run_container::UnknownQueueContainers,
                    },
                    &Vec::new,
                ),
            },
        },
        call,
        refused,
    )
}

/// How the queue service runs the host's dialogue and operation commands:
/// [`run_command`], as the command line runs them.
pub fn service_commands() -> crate::infrastructure::queue_service::ServiceCommands {
    Arc::new(run_command)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `up` an update's job or `install` starts a supervisor again with
    /// is given its providers and plugin directory, and no `--cmux`
    /// (ADR-t2159-1 decision 6).
    #[test]
    fn a_restarts_up_is_given_the_providers_and_no_cmux() {
        assert_eq!(
            restart_arguments(
                Path::new("/opt/bin/claude"),
                Path::new("/opt/bin/codex"),
                Some(Path::new("/plugin")),
            )
            .unwrap(),
            [
                "--claude",
                "/opt/bin/claude",
                "--codex",
                "/opt/bin/codex",
                "--plugin-dir",
                "/plugin",
            ]
        );
        assert_eq!(
            install_restart_arguments(None, None, Some(Path::new("/plugin"))).unwrap(),
            ["--plugin-dir", "/plugin"]
        );
        assert!(
            install_restart_arguments(None, None, None)
                .unwrap()
                .is_empty()
        );
    }
}
