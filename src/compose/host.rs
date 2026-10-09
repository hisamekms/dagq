//! The wiring of host運用 (docs/design/architecture.md): `up`, `down`,
//! `install` and the update jobs, `rebind`, the queue service's commands,
//! `init` and `migrate`, the inbox's command, and the ports of the
//! supervisor's release check, host metrics, queue service and sccache.
//! Each entry builds the adapters and calls the use case; what is decided
//! is in `application` and `domain`.

use super::{OneShot, queue_service_view};
use crate::domain::LeaseToken;
use crate::infrastructure::location::runs_dir;
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
        WorkspaceBackend,
        install::{self as installation, Binaries, InstallOptions},
        lifecycle::{
            self, DownOptions, InboxAgent, InboxOptions, InboxPorts, LifecycleQueue,
            Ports as LifecyclePorts, QueuePaths, RepositoryPaths, UpEnvironment, UpOptions,
        },
        rebind::{self as rebinding, Rebind, RebindTarget},
        recording::RecordingBackend,
        update,
    },
    domain::SupervisorRegistration,
    infrastructure::{
        adapters::{
            ClaudeCode, ClaudePlugin, Cmux, GitRepository, SystemProcesses,
            claude_trusts_repository, load_average, path_text,
        },
        binaries::LocalBinaries,
        clock,
        location::{QueueLocation, REPOSITORY_FILE_NAME, data_home},
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
    pub cmux: PathBuf,
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

/// The arguments of the `up` that starts a supervisor registered in the
/// retired in-cmux mode again after an update's job, beside `--db` and its
/// own flags.
fn restart_arguments(
    cmux: &Path,
    claude: &Path,
    codex: &Path,
    plugin_dir: Option<&Path>,
) -> Result<Vec<String>> {
    let mut arguments = vec![
        "--cmux".to_owned(),
        path_text(cmux)?,
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

/// Starts a supervisor an update's job found gone with the binary in
/// place, as [`update::restart`] decides: cmux closes the workspace of one
/// registered in the retired in-cmux mode and `binary` runs its `up`.
fn restarter<'a>(
    db: &'a Path,
    cmux: &'a Path,
    binary: &'a Path,
    arguments: &'a [String],
) -> impl Fn(&SupervisorRegistration) -> Result<Value> + 'a {
    move |registration: &SupervisorRegistration| -> Result<Value> {
        let cmux = Cmux {
            executable: cmux.to_path_buf(),
        };
        update::restart(
            registration,
            &path_text(db)?,
            arguments,
            &|workspace| cmux.close(workspace),
            &|up| LocalBinaries.run(binary, up),
        )
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
    pub cmux: PathBuf,
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

/// How the supervisor records the host's load (task 516).
#[derive(Debug, Clone)]
pub struct HostMetricsSettings {
    /// How often a sample is taken.
    pub interval: Duration,
    /// How many local days of files are kept, today's included; zero
    /// removes none.
    pub retention_days: u32,
    /// Takes one sample at a unix second; tests set it.
    pub sample: fn(i64) -> crate::domain::host_metrics::HostSample,
    /// Reads the space of the filesystem of a path, put in each sample for
    /// the queue's `runs/` (task 1371); tests set it.
    pub disk: fn(&Path) -> Option<crate::domain::host_metrics::DiskSpace>,
}

impl HostMetricsSettings {
    /// The host's own sample every `interval`, kept `retention_days`.
    pub fn new(interval: Duration, retention_days: u32) -> Self {
        Self {
            interval,
            retention_days,
            sample: crate::infrastructure::host_metrics::sample,
            disk: crate::infrastructure::adapters::disk_space,
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

/// The runtime's own constructor of the cmux wrapper `up`, `down` and the
/// tests use: failures are recorded in the queue at `db`, and `token` is
/// the supervisor whose slots are reported (`None` reports all of them).
impl<'a> RecordingBackend<'a> {
    pub fn new(inner: &'a dyn WorkspaceBackend, db: PathBuf, token: Option<LeaseToken>) -> Self {
        Self::over(
            inner,
            Arc::new(SqliteOpener {
                db,
                generators: clock::system(),
                actor: None,
            }),
            token,
            load_average,
        )
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
    .inbox_command(&prompt, plugin_dir, db.parent().unwrap_or(Path::new(".")))?;
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
/// report as JSON, or the error that stopped it.
fn trace_command(command: &str, result: &Result<Value>) {
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
/// again (`--cmux`, and `--claude`, `--codex`, `--plugin-dir` when given):
/// a provider's path as given, a link kept and a gone one found again by
/// its name (ADR-t2079-1); a Codex not found is passed as given.
pub fn install_restart_arguments(
    cmux: &Path,
    claude: Option<&Path>,
    codex: Option<&Path>,
    plugin_dir: Option<&Path>,
) -> Result<Vec<String>> {
    let mut restart = vec!["--cmux".to_owned(), path_text(cmux)?];
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

/// `dagq service status`: [`queue_service_view`], read without changing
/// anything.
pub fn queue_service_status(db: &Path) -> Value {
    let queue = SqliteQueue::open_read_only(db).ok();
    queue_service_view(db, queue.as_ref())
}

/// `dagq service start`: see
/// [`crate::application::queue_service::start_by_hand`], with `executable`.
pub fn queue_service_start(db: &Path, executable: &Path) -> Result<Value> {
    crate::application::queue_service::start_by_hand(
        &SqliteQueue::open(db)?,
        &crate::infrastructure::queue_service::SystemQueueService::new(db, executable),
        crate::application::lifecycle::QUEUE_SERVICE_START_TIMEOUT,
    )
}

/// `dagq service stop`: see
/// [`crate::application::queue_service::stop_by_hand`].
pub fn queue_service_stop(db: &Path) -> Result<Value> {
    crate::application::queue_service::stop_by_hand(
        &SqliteQueue::open(db)?,
        &crate::infrastructure::queue_service::SystemQueueService::new(db, Path::new("dagq")),
        crate::application::lifecycle::QUEUE_SERVICE_START_TIMEOUT,
    )
}

/// `init`: create the queue at `db`, or open the one there.
pub fn init_queue(db: &Path) -> Result<SqliteQueue> {
    SqliteQueue::init(db)
}

/// `migrate --check`: the schema of the queue at `db`, read-only.
pub fn queue_schema(db: &Path) -> Result<crate::infrastructure::sqlite::SchemaState> {
    SqliteQueue::schema(db)
}

/// `migrate`: bring the queue at `db` to this binary's schema, with the
/// live processes judged on this host.
pub fn migrate_queue(
    db: &Path,
    now: i64,
) -> Result<crate::infrastructure::sqlite::MigrationReport> {
    SqliteQueue::migrate(
        db,
        Some(&crate::infrastructure::adapters::process_alive),
        now,
    )
}

impl OneShot {
    /// `rebind`: bind the queue at `db` to the repository containing `repo`
    /// (see [`rebinding::rebind`]).
    pub fn rebind(&self, db: &Path, repo: &Path) -> Result<Value> {
        let db = db
            .canonicalize()
            .context("queue must already be initialized")?;
        let mut queue = self.open(&db)?;
        let repository = GitRepository::inspect(repo)?;
        let common_dir = path_text(&repository.common_dir)?;
        let location = QueueLocation::explicit(&db);
        let repository_queue_dir = data_home()
            .ok()
            .map(|home| QueueLocation::for_repository(&repository.common_dir, &home).queue_dir);
        let result = rebinding::rebind(
            Rebind {
                queue: &mut queue,
                repository: &repository,
                files: &LocalRunFiles,
                processes: &SystemProcesses,
                clock: &*self.generators.clock,
            },
            RebindTarget {
                repository_file: location.queue_dir.join(REPOSITORY_FILE_NAME),
                db,
                common_dir,
                queue_dir: location.queue_dir,
                log_dir: location.log_dir,
                repository_queue_dir,
            },
        );
        trace_command("rebind", &result);
        result
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
        let migrated = self.migrate_compatible(&location.db, processes)?;
        let queues = |db: &Path| self.lifecycle_queues(db);
        let result = lifecycle::up(
            &self.lifecycle_ports(launchd, processes, &queues),
            &claude,
            &queue_paths(location),
            repo,
            environment,
            options,
        )
        .map_err(|mut error| {
            // A handoff some supervisors failed reports the whole of `up`.
            if let Some(partial) = error.downcast_mut::<lifecycle::PartialHandoff>() {
                partial.report["migrated"] = serde_json::to_value(&migrated).unwrap_or(Value::Null);
            }
            error
        })
        .and_then(|mut value| {
            value["migrated"] = serde_json::to_value(&migrated)?;
            Ok(value)
        });
        trace_command("up", &result);
        result
    }

    /// [`SqliteQueue::migrate_compatible`] of the queue at `db` on these
    /// generators' now, with the live processes judged by `processes`.
    pub fn migrate_compatible(
        &self,
        db: &Path,
        processes: &dyn ProcessControl,
    ) -> Result<Option<crate::infrastructure::sqlite::MigrationReport>> {
        let alive = |pid| processes.alive(pid);
        SqliteQueue::migrate_compatible(db, Some(&alive), self.generators.clock.now())
    }

    /// `down`: see [`lifecycle::down`].
    pub fn down(
        &self,
        location: &QueueLocation,
        launchd: &dyn LaunchAgent,
        processes: &dyn ProcessControl,
        options: &DownOptions,
    ) -> Result<Value> {
        let queues = |db: &Path| self.lifecycle_queues(db);
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
        let queues = |db: &Path| self.event_queues(db);
        lifecycle::inbox(
            &InboxPorts {
                files: &LocalRunFiles,
                queues: &queues,
                inspect_repository: &inspect_repository,
                resolve_language: &|checkout, user_config| {
                    crate::infrastructure::language::resolve_language(Some(checkout), user_config)
                },
            },
            &InboxAgent {
                provider: crate::domain::Provider::Claude,
                agent: &claude,
            },
            &location.db,
            repo,
            environment,
            &InboxOptions {
                agent: claude.executable.clone(),
                ..options.clone()
            },
        )
    }

    /// `install`: replace the fixed binary and hand the queue's supervisor
    /// over to it (see [`installation::install`]), then watch the
    /// supervisors heartbeat on under it for `watch_timeout` and, when they
    /// do not, put the old binary back, start them again and ask the inbox
    /// (see [`update::install_watched`], ADR-0073 decisions 13 and 14).
    /// `executable` is the cmux the `up` that starts one again is given.
    pub fn install(
        &self,
        location: &QueueLocation,
        executable: &Path,
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
        let restart = restarter(&db, executable, &options.target, &options.restart);
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
        let restart_arguments = restart_arguments(
            &job.cmux,
            &job.claude,
            &job.codex,
            job.plugin_dir.as_deref(),
        )?;
        let restart = restarter(&db, &job.cmux, &job.target, &restart_arguments);
        let paths = update::UpdatePaths::under(&location.queue_dir);
        let e2e = installation::E2eSettings {
            command: job.e2e_command.clone(),
            timeout: job.e2e_timeout,
            cmux: Some(job.cmux.clone()),
            // The runtime reads `[run.env]` from the main checkout's
            // `dagq.toml`, as for a run.
            run_env_root: Some(job.repository.clone()),
            queue_dir: Some(location.queue_dir.clone()),
            scratch: paths.e2e.clone(),
            log: e2e_log(&job.log),
            utc_offset_secs: clock::local_utc_offset(self.generators.clock.now()),
            lock: installation::e2e_lock_path(&location.queue_dir),
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
        let restart_arguments = restart_arguments(
            &job.cmux,
            &job.claude,
            &job.codex,
            job.plugin_dir.as_deref(),
        )?;
        let restart = restarter(&db, &job.cmux, &job.target, &restart_arguments);
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
        executable: &Path,
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
            executable,
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

    /// The queue at a path as the ports `up` and `down` take.
    fn lifecycle_queues(&self, db: &Path) -> Arc<dyn QueueOpener<dyn LifecycleQueue + Send>> {
        Arc::new(SqlitePorts {
            opener: self.opener(db),
            keep: |queue| -> Box<dyn LifecycleQueue + Send> { Box::new(queue) },
        })
    }

    /// The queue at a path as its events, the one port `dagq inbox` takes.
    fn event_queues(&self, db: &Path) -> Arc<dyn QueueOpener<dyn RunLog + Send>> {
        Arc::new(SqlitePorts {
            opener: self.opener(db),
            keep: |queue| -> Box<dyn RunLog + Send> { Box::new(queue) },
        })
    }

    /// The queue at a path as the ports `install` and the update jobs take.
    fn host_queues(&self, db: &Path) -> Arc<dyn QueueOpener<dyn HostOpsQueue + Send>> {
        Arc::new(SqlitePorts {
            opener: self.opener(db),
            keep: |queue| -> Box<dyn HostOpsQueue + Send> { Box::new(queue) },
        })
    }

    /// The adapters `up` and `down` run on: the queue at a path through
    /// `SqliteQueue` with these generators, Git for the repository, the
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
/// under the queue's `host/`, each with the space of the filesystem of the
/// run worktrees and their builds, the one the disk checks read (task
/// 1371): `runs/`, else the queue's dir.
pub(super) fn host_metrics_port(
    db: &Path,
    settings: HostMetricsSettings,
) -> crate::application::supervise::HostMetricsPort {
    let queue_dir = db.parent().unwrap_or(Path::new(".")).to_path_buf();
    let dir = queue_dir.join(crate::domain::host_metrics::HOST_DIR);
    let runs = runs_dir(db);
    let interval = settings.interval;
    crate::application::supervise::HostMetricsPort {
        interval,
        record: Arc::new(move |now| {
            let disk = (settings.disk)(&runs).or_else(|| (settings.disk)(&queue_dir));
            let sample = (settings.sample)(now).with_disk(disk);
            crate::infrastructure::host_metrics::record(
                &dir,
                &sample,
                clock::local_utc_offset(now),
                interval,
                settings.retention_days,
            )
            .map(|recorded| recorded.removed)
        }),
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
