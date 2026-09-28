//! The composition root: each entry point opens the queue and the
//! repository, builds the adapters of the ports (`SqliteQueue`,
//! `GitRepository`, `Cmux`-backed recording, `ClaudeCode`, `Launchctl`'s
//! port, `SystemProcesses`, `LocalRunFiles`, the system clock and IDs) and
//! calls the use case in `application` (ADR-0013). `main` resolves the
//! queue location, assembles the clock and IDs once ([`OneShot`] and
//! [`SuperviseOptions::generators`]), parses the CLI and prints what these
//! return; `runtime` and `lifecycle` re-export them under the names the
//! tests use.

use crate::domain::LeaseToken;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use crate::{
    application::{
        AgentProvider, Generators, LaunchAgent, MainRemote, ProcessControl, QueueOpener,
        Repository, RunFiles, Spawner, Verifier, WorkerAdapter, WorkerAdapters, WorkspaceBackend,
        health,
        install::{self as installation, Binaries, InstallOptions},
        integrate::{self as integration, IntegrateTarget, Integration, Integrator},
        lifecycle::{
            self, DownOptions, Ports as LifecyclePorts, QueuePaths, ROLE_ENV, RepositoryPaths,
            UpEnvironment, UpOptions,
        },
        planner::{self, PlannerLaunch, PlannerProbes, PlannerWrapper},
        prompt,
        rebind::{self as rebinding, Rebind, RebindTarget},
        recording::RecordingBackend,
        review::{self as reviewing, Review},
        session::{self as wrapper, OwnWorkspace, Session},
        stats::{self as statistics, StatsSources, WorkspaceListing},
        supervise::{self as supervisor, Heartbeat, Layout, LoopSettings, Ports, UpdateSettings},
        update,
    },
    domain::{
        IntegrationOutcome, NewAsk, PlannerId, Provider, RunId, SessionRole, SupervisorMode,
        SupervisorRegistration, TaskDetail, TaskId, TaskRun,
        slot_limits::{SlotFlags, SlotLimits, SupervisorConfig},
        stall::StallConfig,
        stats::{ConflictConfigReport, StatsQuery},
        worker::{ProviderCheck, Worker, WorkerMode},
    },
    infrastructure::{
        adapters::{
            ClaudeCode, ClaudePlugin, Cmux, GitRepository, SystemProcesses, VERIFICATION_TIMEOUT,
            claude_trusts_repository, executable, free_disk_bytes, host_versions, load_average,
            main_checkout_of, path_text,
        },
        binaries::LocalBinaries,
        clock,
        codex::Codex,
        location::{
            QueueLocation, REPOSITORY_FILE_NAME, data_home, goal_reviews_dir, plan_reviews_dir,
            planners_dir, runs_dir,
        },
        process::LocalSpawner,
        run_env::{
            ShellVerifier, load_conflict_config, load_disk_config, load_kpi_settings,
            load_resume_config, load_stall_config, load_supervisor_config,
        },
        run_files::LocalRunFiles,
        runtime_store::SqliteOpener,
        sqlite::{ReadOnlyQueue, SqliteQueue},
        transcripts::ClaudeTranscripts,
    },
};

const IDLE_POLL: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_secs(1);
/// How often the supervisor sweeps the workspaces of ended runs.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// The default planner timeout: an hour, like a run's resume timeout
/// (ADR-0041 decision 13).
pub const PLANNER_TIMEOUT: Duration = Duration::from_secs(3600);

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
    /// What an in-cmux supervisor started again uses.
    pub cmux: PathBuf,
    pub claude: PathBuf,
    /// The Codex CLI of the supervisor that started the job (ADR-t813-2).
    pub codex: PathBuf,
    pub plugin_dir: Option<PathBuf>,
    pub handoff_timeout: Duration,
    pub watch_timeout: Duration,
}

/// The arguments of the `up` that starts an in-cmux supervisor again after
/// an update's job, beside `--db`, `--in-cmux` and its own flags.
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
/// place: launchd does for its own; an in-cmux one by `up --in-cmux`, with
/// `--auto-update` only when it had it (a supervisor of a release has not,
/// and outside dagq's source `up` refuses it, ADR-t614-1).
fn restarter<'a>(
    db: &'a Path,
    cmux: &'a Path,
    binary: &'a Path,
    arguments: &'a [String],
) -> impl Fn(&SupervisorRegistration) -> Result<Value> + 'a {
    move |registration: &SupervisorRegistration| -> Result<Value> {
        match registration.mode {
            Some(SupervisorMode::Launchd) => Ok(json!({
                "by": "launchd",
                "note": "its LaunchAgent starts the binary in place again",
            })),
            Some(SupervisorMode::InCmux) => {
                if let Some(id) = &registration.workspace_id {
                    let _ = Cmux {
                        executable: cmux.to_path_buf(),
                    }
                    .close(id);
                }
                let mut up = vec![
                    "--db".to_owned(),
                    path_text(db)?,
                    "up".to_owned(),
                    "--in-cmux".to_owned(),
                ];
                if registration.auto_update {
                    up.push("--auto-update".to_owned());
                }
                // Only the values it took from flags (task 698).
                up.extend(registration.flag_arguments());
                up.extend(arguments.iter().cloned());
                let started = LocalBinaries.run(binary, &up)?;
                Ok(json!({"by": "up --in-cmux", "up": started["supervisor"]}))
            }
            None => bail!(
                "it was started by hand rather than by `up`, so it is not started again; start it the same way"
            ),
        }
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
    /// What an in-cmux supervisor started again uses.
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

/// How the supervisor loop is driven. `stop` is the graceful drain switch
/// (SIGINT in the CLI): no more claims, exit once every active run rests.
#[derive(Debug, Clone)]
pub struct SuperviseOptions {
    /// Upper bound on runs executing at once (`supervise --parallel`);
    /// `None` follows `[supervisor] parallel` of the main checkout's
    /// `dagq.toml`, read again each pass, else 4 (task 698).
    pub parallel: Option<usize>,
    /// Upper bound on the runs waiting for a person outside the slots
    /// (ADR-0062 decision 7); zero keeps every run in its slot. `None`
    /// follows `[supervisor] max_waiting` as `parallel` does.
    pub max_waiting: Option<usize>,
    /// Exit when no run is active and no task can be claimed, instead of
    /// polling for new work.
    pub once: bool,
    pub stop: Arc<AtomicBool>,
    /// Start the observer job when this long passed since the last one
    /// started or finished (ADR-0024 decision 4); zero disables the
    /// observer, the daily one included.
    pub observe_interval: Duration,
    /// Also run the daily observation once every 24 hours.
    pub observe_daily: bool,
    /// Pause between two passes over the active runs; tests shorten it.
    pub tick: Duration,
    /// Pause between two looks for claimable work while no run is active.
    pub idle_poll: Duration,
    /// Least time between two sweeps of the workspaces of ended runs; tests
    /// shorten it.
    pub sweep_interval: Duration,
    /// The clock and IDs of everything the supervisor records; tests fix them.
    pub generators: Generators,
    /// The thresholds of the stalled-session checks; `None` reads `[stall]`
    /// from the `dagq.toml` of the repository's main checkout (ADR-0043
    /// decision 4). Tests set them.
    pub stall: Option<StallConfig>,
    /// The `[conflicts]` thresholds and the limit of a deferred claim
    /// (ADR-0069); `None` reads `[conflicts]` of the main checkout's
    /// `dagq.toml`.
    pub conflicts: Option<crate::domain::stats::ConflictConfig>,
    /// Upper bound on the planners the runtime has open at once (ADR-0041
    /// decision 12); a person's planners do not count.
    pub runtime_planners: usize,
    /// How long a planner may take to submit a proposal sent back to it
    /// before the inbox is told (ADR-0041 decision 13).
    pub planner_timeout: Duration,
    /// The plugin directory the planners the runtime opens load.
    pub plugin_dir: Option<PathBuf>,
    /// The token of the supervisor this process takes over after it exec'd
    /// this binary (ADR-0045 decision 10): its registration and leases are
    /// kept, not registered anew. `None` registers a new supervisor.
    pub handoff_token: Option<LeaseToken>,
    /// How `up` started this process (`supervise --mode`), for its start
    /// mark; `None` for a supervisor started by hand.
    pub mode: Option<SupervisorMode>,
    /// The automatic update of the supervisor's binary (ADR-0045 decision
    /// 17): off unless `supervise --auto-update`.
    pub update: UpdateSettings,
    /// `supervise --max-load` (task 327): no new run is claimed while the
    /// 1-minute load average is above it; `None` (the default here) holds
    /// for no load.
    pub max_load: Option<f64>,
    /// Reads the 1-minute load average; tests set it.
    pub load_average: fn() -> Option<f64>,
    /// How much free disk space a claim and a landing need (task 377);
    /// `None` reads `[disk]` of the main checkout's `dagq.toml`.
    pub disk: Option<crate::domain::disk::DiskConfig>,
    /// The limit of a run's conflict-only attempts (ADR-0047 decision
    /// 24); `None` reads `[resume]` of the main checkout's `dagq.toml`.
    pub resume: Option<crate::domain::resume::ResumeConfig>,
    /// Reads the free bytes of the file system of a path; tests set it.
    pub free_space: fn(&Path) -> Option<u64>,
    /// Write the KPI reports of each day and week under `<queue dir>/reports/`
    /// (ADR-0051 decision 20): off unless asked for (`supervise
    /// --report-daily`, on by default in the CLI).
    pub report_daily: bool,
    /// Record the forecast snapshots at their triggers (ADR-0070 decision
    /// 3): off unless asked for (`supervise --forecast-snapshots`, on by
    /// default in the CLI).
    pub forecast_snapshots: bool,
    /// How often the triggers of a snapshot are looked for; tests shorten
    /// it.
    pub forecast_check: Duration,
    /// The host-wide `host.toml` the supervisor reads (`[kpi]`, `[report]`,
    /// `[push]`); `None` is `$XDG_CONFIG_HOME/dagq/host.toml`. Tests set it.
    pub host_config: Option<PathBuf>,
    /// The PATH the reports' dependency diagram finds d2 and TALA on
    /// (ADR-0077 decision 4); `None` is the supervisor's. Tests set it.
    pub diagram_path: Option<std::ffi::OsString>,
    /// The user's `config.toml` the supervisor reads `[language]` from
    /// (ADR-t616-2); `None` reads none. The CLI gives
    /// `$XDG_CONFIG_HOME/dagq/config.toml`.
    pub user_config: Option<PathBuf>,
    /// The delays before the second and the third attempt of a KPI push
    /// (ADR-0051 decision 23: 1 and 5 minutes); tests shorten them.
    pub push_retry: [Duration; 2],
    /// The run files the supervisor works with; `None` is the local file
    /// system. Tests set it (a slow removal, task 405).
    pub files: Option<RunFilesPort>,
    /// Reads crates.io's sparse index for the release check (ADR-t618-1);
    /// `None` is `curl`. Tests set it.
    pub release_index: Option<ReleaseIndexPort>,
    /// The build identifier the release check takes as the supervisor's;
    /// `None` is this binary's. Tests set it (a release build looks).
    pub release_current: Option<String>,
    /// The Codex CLI a Codex worker starts (`supervise --codex`, ADR-t813-2):
    /// resolved on PATH when found; a supervisor without it runs no Codex
    /// worker.
    pub codex: PathBuf,
    /// Record the host's load under `<queue dir>/host/` (task 516): off
    /// unless asked for (`supervise --host-metrics-interval`, 30 seconds
    /// by default in the CLI).
    pub host_metrics: Option<HostMetricsSettings>,
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
}

impl HostMetricsSettings {
    /// The host's own sample every `interval`, kept `retention_days`.
    pub fn new(interval: Duration, retention_days: u32) -> Self {
        Self {
            interval,
            retention_days,
            sample: crate::infrastructure::host_metrics::sample,
        }
    }
}

/// The index [`SuperviseOptions::release_index`] gives the release check.
#[derive(Clone)]
pub struct ReleaseIndexPort(pub Arc<dyn crate::application::release_update::ReleaseIndex>);

impl std::fmt::Debug for ReleaseIndexPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReleaseIndexPort")
    }
}

/// The run files [`SuperviseOptions::files`] gives the supervisor.
#[derive(Clone)]
pub struct RunFilesPort(pub Arc<dyn RunFiles>);

impl std::fmt::Debug for RunFilesPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RunFilesPort")
    }
}

impl SuperviseOptions {
    pub fn new(parallel: usize, once: bool) -> Self {
        Self {
            parallel: Some(parallel),
            max_waiting: None,
            once,
            stop: Arc::new(AtomicBool::new(false)),
            observe_interval: Duration::ZERO,
            observe_daily: false,
            tick: TICK,
            idle_poll: IDLE_POLL,
            sweep_interval: SWEEP_INTERVAL,
            generators: clock::system(),
            stall: None,
            conflicts: None,
            runtime_planners: 1,
            planner_timeout: PLANNER_TIMEOUT,
            plugin_dir: None,
            handoff_token: None,
            mode: None,
            update: UpdateSettings::default(),
            // The CLI's `--max-load` has a default; a caller of the library
            // (the tests) holds for no load unless it asks to.
            max_load: None,
            load_average,
            disk: None,
            resume: None,
            free_space: free_disk_bytes,
            report_daily: false,
            forecast_snapshots: false,
            forecast_check: crate::application::supervise::FORECAST_CHECK,
            host_config: None,
            diagram_path: None,
            user_config: None,
            push_retry: crate::domain::kpi::push::RETRY_DELAYS_SECS.map(Duration::from_secs),
            files: None,
            release_index: None,
            release_current: None,
            codex: PathBuf::from("codex"),
            host_metrics: None,
        }
    }

    /// The flags `parallel` and `max_waiting` stand for.
    fn slot_flags(&self) -> SlotFlags {
        SlotFlags {
            parallel: self.parallel,
            max_waiting: self.max_waiting,
        }
    }

    fn settings(
        &self,
        stall: StallConfig,
        conflicts: ConflictConfigReport,
        disk: crate::domain::disk::DiskConfig,
        resume: crate::domain::resume::ResumeConfig,
        limits: SlotLimits,
    ) -> LoopSettings {
        LoopSettings {
            limits,
            slot_flags: self.slot_flags(),
            once: self.once,
            stop: self.stop.clone(),
            observe_interval: self.observe_interval,
            observe_daily: self.observe_daily,
            tick: self.tick,
            idle_poll: self.idle_poll,
            sweep_interval: self.sweep_interval,
            stall,
            conflicts,
            runtime_planners: self.runtime_planners,
            planner_timeout: self.planner_timeout,
            handoff_token: self.handoff_token.clone(),
            mode: self.mode,
            update: self.update.clone(),
            max_load: self.max_load,
            disk,
            resume,
        }
    }
}

/// Run and monitor tasks until the loop ends (see
/// [`supervisor::supervise`]). Accepted runs are reviewed headless by
/// `claude` itself (ADR-0027).
pub fn supervise(
    db: &Path,
    repo: &Path,
    cmux: &dyn WorkspaceBackend,
    claude: &Path,
    runner: &Path,
    options: &SuperviseOptions,
) -> Result<Value> {
    let reviewer = ClaudeCode {
        executable: claude.into(),
    };
    supervise_with_reviewer(db, repo, cmux, claude, &reviewer, runner, options)
}

/// [`supervise`] with the provider of the headless review given apart
/// from the `claude` the run sessions start (a test double in tests).
pub fn supervise_with_reviewer(
    db: &Path,
    repo: &Path,
    cmux: &dyn WorkspaceBackend,
    claude: &Path,
    reviewer: &dyn AgentProvider,
    runner: &Path,
    options: &SuperviseOptions,
) -> Result<Value> {
    ensure!(
        options.parallel.is_none_or(|parallel| parallel >= 1),
        "parallel must be at least 1"
    );
    let db = db
        .canonicalize()
        .context("queue must already be initialized")?;
    let repository = GitRepository::inspect(repo)?;
    // Every setting is read from it: a repository without one stops here.
    let main_checkout = repository.checkout()?.to_path_buf();
    // Every claim and landing reads it (ADR-t615-1).
    repository.landing_branch()?;
    let stall = match options.stall {
        Some(stall) => stall,
        None => load_stall_config(&main_checkout)?.unwrap_or_default(),
    };
    // For the plan review's hotspots and the claims deferred on them
    // (ADR-0069): a `[conflicts]` that cannot be read at the start leaves
    // the defaults rather than stopping the supervisor; later reads keep
    // the values in use (ADR-0080).
    let conflicts = statistics::conflict_config(options.conflicts.or_else(|| {
        load_conflict_config(&main_checkout).unwrap_or_else(|error| {
            tracing::warn!(error = %format_args!("{error:#}"), "[conflicts] of dagq.toml not read: {error:#}; using the defaults");
            None
        })
    }));
    // The free disk space a claim and a landing need (ADR-0047 decision
    // 44): a `[disk]` that cannot be read leaves the defaults, as
    // `[conflicts]` does.
    let disk = options.disk.unwrap_or_else(|| {
        load_disk_config(&main_checkout)
            .unwrap_or_else(|error| {
                tracing::warn!(error = %format_args!("{error:#}"), "[disk] of dagq.toml not read: {error:#}; using the defaults");
                None
            })
            .unwrap_or_default()
    });
    // The limit of the conflict-only attempts (ADR-0047 decision 24): a
    // `[resume]` that cannot be read leaves the default, as `[disk]` does.
    let resume = options.resume.unwrap_or_else(|| {
        load_resume_config(&main_checkout)
            .unwrap_or_else(|error| {
                tracing::warn!(error = %format_args!("{error:#}"), "[resume] of dagq.toml not read: {error:#}; using the defaults");
                None
            })
            .unwrap_or_default()
    });
    // `parallel` and `max_waiting` the flags did not give (task 698): a
    // `[supervisor]` that cannot be read at the start leaves the defaults,
    // as `[disk]` does; later reads keep the values in use.
    let slot_flags = options.slot_flags();
    let limits = SlotLimits::resolve(
        slot_flags,
        if slot_flags.complete() {
            SupervisorConfig::default()
        } else {
            load_supervisor_config(&main_checkout)
                .unwrap_or_else(|error| {
                    tracing::warn!(error = %format_args!("{error:#}"), "[supervisor] of dagq.toml not read: {error:#}; using the defaults");
                    None
                })
                .unwrap_or_default()
        },
    );
    let supervisor_file = (!slot_flags.complete()).then(|| {
        let checkout = main_checkout.clone();
        Arc::new(move || load_supervisor_config(&checkout))
            as crate::application::supervise::SupervisorFile
    });
    let pid = std::process::id();
    let generators = options.generators.clone();
    let agent = ClaudeCode {
        executable: claude.into(),
    };
    let transcripts = ClaudeTranscripts::from_env();
    // A Codex that is not found, or does not run, runs no worker: its
    // tasks are deferred, and the supervisor starts all the same
    // (ADR-t813-2).
    let found_codex = crate::infrastructure::codex::executable(&options.codex).ok();
    let codex = found_codex.clone().unwrap_or_else(|| options.codex.clone());
    let codex_agent = found_codex
        .map(|executable| Codex { executable })
        .filter(|codex| match codex.preflight() {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(error = %format_args!("{error:#}"), "codex {} does not run: {error:#}; no Codex worker", codex.executable.display());
                false
            }
        });
    let workers = worker_adapters(&agent, &transcripts, codex_agent.as_ref());
    let layout = Layout {
        runs_dir: runs_dir(&db),
        queue_hash: QueueLocation::explicit(&db).hash(),
        repo_root: repository.root.clone(),
        main_checkout: main_checkout.clone(),
        common_dir: repository.common_dir.clone(),
        claude: claude.into(),
        codex: codex.clone(),
        providers: provider_checks(claude, &codex, &workers),
        runner: runner.into(),
        pid,
        version: crate::VERSION.to_owned(),
        // The observe command's environment drops the supervisor's actor
        // variables, and the supervisor sets its own when it starts it
        // (`supervisor:<pid>`); its agent is the observer.
        observer_env_remove: [
            ROLE_ENV,
            crate::domain::actor::ACTOR_ID_ENV,
            crate::domain::actor::RUN_ID_ENV,
            crate::domain::actor::TASK_ID_ENV,
        ]
        .map(str::to_owned)
        .to_vec(),
        planners_dir: planners_dir(&db),
        plugin_dir: options
            .plugin_dir
            .as_deref()
            .map(|dir| {
                dir.canonicalize()
                    .with_context(|| format!("plugin directory {}", dir.display()))
            })
            .transpose()?,
        plan_reviews_dir: plan_reviews_dir(&db),
        goal_reviews_dir: goal_reviews_dir(&db),
        db: db.clone(),
    };
    let review_db = db.clone();
    let review_material = move |task_id: TaskId| review(&review_db, task_id);
    let reports = options.report_daily.then(|| {
        let (db, checkout) = (db.clone(), main_checkout.clone());
        let host_wide = options
            .host_config
            .clone()
            .or_else(crate::infrastructure::kpi_config::host_wide_file);
        let name = checkout.file_name().map_or_else(
            || "dagq".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        let push_target = crate::application::push::PushTarget {
            name,
            queue: db.display().to_string(),
        };
        let (setup_db, setup_wide) = (db.clone(), host_wide.clone());
        let queue_dir = db.parent().unwrap_or(Path::new(".")).to_path_buf();
        let diagram_path = options.diagram_path.clone();
        crate::application::supervise::ReportPort {
            utc_offset: clock::local_utc_offset,
            setup: Arc::new(move |now| {
                let mut setup =
                    report_setup(&setup_db, Some(&checkout), None, setup_wide.as_deref(), now)?;
                if let Some(path) = &diagram_path {
                    setup.diagram = d2_renderer(Some(path.clone()));
                }
                Ok(setup)
            }),
            push_config: Arc::new(move || {
                crate::infrastructure::push::load_host_push(&queue_dir, host_wide.as_deref())
            }),
            run_push: crate::infrastructure::push::run_push,
            push_retry: options.push_retry,
            push_target,
        }
    });
    // Read again each pass (ADR-0080), unless the options set the
    // thresholds.
    let conflicts_file = options.conflicts.is_none().then(|| {
        let checkout = main_checkout.clone();
        Arc::new(move || load_conflict_config(&checkout))
            as crate::application::supervise::ConflictsFile
    });
    let limit_checkout = main_checkout.clone();
    let max_improvement_proposals = Arc::new(move || max_improvement_proposals(&limit_checkout));
    let forecasts = options.forecast_snapshots.then(|| {
        let (db, checkout) = (db.clone(), main_checkout.clone());
        let host_wide = options
            .host_config
            .clone()
            .or_else(crate::infrastructure::kpi_config::host_wide_file);
        crate::application::supervise::ForecastPort {
            utc_offset: clock::local_utc_offset,
            min_samples: Arc::new(move |now| {
                report_setup(&db, Some(&checkout), None, host_wide.as_deref(), now)
                    .map(|setup| setup.config.min_samples)
            }),
            check: options.forecast_check,
        }
    });
    // Read again at each look, never from dagq.toml (ADR-t618-1 decision
    // 3).
    let release = {
        let queue_dir = db.parent().unwrap_or(Path::new(".")).to_path_buf();
        let host_wide = options
            .host_config
            .clone()
            .or_else(crate::infrastructure::kpi_config::host_wide_file);
        crate::application::supervise::ReleasePort {
            config: Arc::new(move || {
                crate::infrastructure::release_update::load_host_update(
                    &queue_dir,
                    host_wide.as_deref(),
                )
                .config
            }),
            index: options.release_index.as_ref().map_or_else(
                || {
                    Arc::new(crate::infrastructure::release_update::CurlIndex::default())
                        as Arc<dyn crate::application::release_update::ReleaseIndex>
                },
                |port| port.0.clone(),
            ),
            // The plugin a `--plugin-dir` supervisor loads is not touched
            // (ADR-t618-2 decision 3).
            plugin: options.plugin_dir.is_none().then(|| {
                Arc::new(crate::infrastructure::adapters::ClaudePlugin {
                    executable: claude.to_path_buf(),
                    cwd: main_checkout.clone(),
                }) as Arc<dyn crate::application::InstalledPlugin>
            }),
            current: options
                .release_current
                .clone()
                .unwrap_or_else(|| crate::VERSION.to_owned()),
        }
    };
    let host_metrics = options.host_metrics.clone().map(|settings| {
        let dir = db
            .parent()
            .unwrap_or(Path::new("."))
            .join(crate::domain::host_metrics::HOST_DIR);
        let interval = settings.interval;
        crate::application::supervise::HostMetricsPort {
            interval,
            record: Arc::new(move |now| {
                let sample = (settings.sample)(now);
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
    });
    let ports = Ports {
        // The supervisor's own transitions (ADR-t728-1 decision 4).
        queues: Arc::new(SqliteOpener {
            db: db.clone(),
            generators: generators.clone(),
            actor: Some(crate::domain::actor::ActorContext::instance(
                crate::domain::actor::ActorRole::Supervisor,
                pid,
            )),
        }),
        verifier: Arc::new(ShellVerifier {
            checkout: main_checkout.clone(),
            db: db.clone(),
            user_config: options.user_config.clone(),
            verification_timeout: VERIFICATION_TIMEOUT,
        }),
        remote: Arc::new(repository.clone()),
        repository: Arc::new(repository),
        cmux,
        workers,
        reviewer,
        spawner: &LocalSpawner,
        files: options.files.as_ref().map_or_else(
            || Arc::new(LocalRunFiles) as Arc<dyn RunFiles>,
            |port| port.0.clone(),
        ),
        processes: Arc::new(SystemProcesses),
        generators,
        review_material: &review_material,
        load_average: options.load_average,
        free_space: options.free_space,
        host_versions,
        reports,
        max_improvement_proposals,
        conflicts_file,
        supervisor_file,
        forecasts,
        release: Some(release),
        host_metrics,
        layout,
    };
    supervisor::supervise(
        &ports,
        &options.settings(stall, conflicts, disk, resume, limits),
    )
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

/// The one-shot entry points (`integrate`, `status`, `doctor`, `recover`,
/// `stats`, `rebind`, `up` and `down`) on the clock and IDs `main`
/// assembled once: the queue each of them opens reads the time and creates
/// IDs through `generators`, and so does the use case, instead of the
/// queue's own default (ADR-0013 policy 7). Tests fix them.
#[derive(Debug, Clone)]
pub struct OneShot {
    pub generators: Generators,
    /// The user's `config.toml` `doctor` and `status --role` read the
    /// language from (ADR-t616-2); `None` reads none. The CLI gives
    /// `$XDG_CONFIG_HOME/dagq/config.toml`.
    pub user_config: Option<PathBuf>,
    /// How much free disk space `integrate` needs before it lands (task
    /// 638); `None` reads `[disk]` of the main checkout's `dagq.toml`.
    pub disk: Option<crate::domain::disk::DiskConfig>,
    /// Reads the free bytes of the file system of a path; tests set it.
    pub free_space: fn(&Path) -> Option<u64>,
    /// How long one verification command of `integrate` may run in all
    /// ([`VERIFICATION_TIMEOUT`]; tests shorten it, task 639).
    pub verification_timeout: std::time::Duration,
}

impl OneShot {
    pub fn new(generators: Generators) -> Self {
        Self {
            generators,
            user_config: None,
            disk: None,
            free_space: free_disk_bytes,
            verification_timeout: VERIFICATION_TIMEOUT,
        }
    }

    /// The wall clock and random UUIDs, what the binary runs with.
    pub fn system() -> Self {
        Self::new(clock::system())
    }

    /// The queue at `db`, writing through these generators.
    fn open(&self, db: &Path) -> Result<SqliteQueue> {
        Ok(SqliteQueue::open(db)?.with_generators(self.generators.clone()))
    }

    /// The queue at `db` on a read-only connection, for a command that only
    /// reads (ADR-0045 decision 18, [`SqliteQueue::open_read_only`]).
    fn open_read_only(&self, db: &Path) -> Result<SqliteQueue> {
        Ok(SqliteQueue::open_read_only(db)?.with_generators(self.generators.clone()))
    }

    /// Land one validated run on `main`: take the single integration slot,
    /// rebase the run worktree onto the current `refs/heads/main`, re-validate
    /// (receipt, descent from main, clean tree) and run the verification
    /// commands, the only run of them for the commit (ADR-0023), squash
    /// the tree into one commit with `Dagq-Task` / `Dagq-Run` trailers and
    /// fast-forward `main` to it. Never a merge commit, never a fast-forward of
    /// the run branch itself. A conflict or a failed re-validation parks the run
    /// as `needs_session` for a resumed session to fix; a rewritten receipt that
    /// reports `failed` ends the run. `repo` is any checkout of the repository
    /// the queue is bound to. After a landing, `main` is pushed to `origin`
    /// through `remote` (ADR-0019 decision 3); `None` is `--no-push`. The push
    /// never changes the landing: its outcome is an event and the `push` of the
    /// result. The landed receipt's `follow_ups` become draft tasks
    /// (ADR-0019 decision 4), listed as the result's `follow_ups`.
    ///
    /// The use case is [`Integrator::approve`] and [`Integrator::land`], at
    /// the request of this process's actor (ADR-t728-2); this entry point
    /// opens the queue and the repository and keeps the lease alive in
    /// between. The slot's token
    /// is one of these generators' IDs.
    pub fn integrate(
        &self,
        db: &Path,
        target: IntegrateTarget,
        repo: &Path,
        remote: Option<&dyn MainRemote>,
    ) -> Result<Value> {
        let db = db
            .canonicalize()
            .context("queue must already be initialized")?;
        let mut queue = self.open(&db)?;
        // The requester is who this process acts as (ADR-t728-2 decision 2):
        // a person, or the inbox at a person's word.
        let requester = queue.actor();
        let repository = GitRepository::inspect(repo)?;
        let main_checkout = repository.checkout()?.to_path_buf();
        let common_dir = path_text(&repository.common_dir)?;
        let verifier = ShellVerifier {
            checkout: main_checkout.clone(),
            db: db.clone(),
            user_config: None,
            verification_timeout: self.verification_timeout,
        };
        // The free disk space a landing's verification needs, as the
        // supervisor reads it (task 638): a `[disk]` that cannot be read
        // leaves the defaults.
        let disk = self.disk.unwrap_or_else(|| {
            load_disk_config(&main_checkout)
                .unwrap_or_else(|error| {
                    tracing::warn!(error = %format_args!("{error:#}"), "[disk] of dagq.toml not read: {error:#}; using the defaults");
                    None
                })
                .unwrap_or_default()
        });
        let read_free =
            || (self.free_space)(&runs_dir(&db)).or_else(|| db.parent().and_then(self.free_space));
        let free = read_free();
        let mut integration = Integration {
            queue: &mut queue,
            repository: &repository,
            verifier: &verifier,
            remote,
            files: &LocalRunFiles,
            common_dir: &common_dir,
            clock: &*self.generators.clock,
            ids: &*self.generators.ids,
            processes: &SystemProcesses,
            pid: std::process::id(),
            load_average,
            disk: Some(integration::DiskRoom { config: disk, free }),
            retry_disk: Some(integration::RetryDisk {
                config: disk,
                free: &read_free,
            }),
        };
        let integrator = Integrator::of_process(std::process::id());
        let Some(request) = integrator.approve(&mut integration, &requester, target, repo)? else {
            return Ok(serde_json::to_value(IntegrationOutcome::NoRunAwaiting)?);
        };
        let heartbeat = Heartbeat::start(
            Arc::new(SqliteOpener {
                db: db.clone(),
                generators: self.generators.clone(),
                actor: None,
            }),
            request.token.clone(),
        );
        let outcome = integrator.land(&mut integration, &request)?;
        drop(heartbeat); // Stops the lease heartbeat before this process reports.
        Ok(serde_json::to_value(outcome)?)
    }

    /// `status --role`: see [`health::status`], measured to these
    /// generators' now.
    pub fn status_for(&self, db: &Path, role: Option<SessionRole>) -> Result<Value> {
        self.status_of(db, &self.open_read_only(db)?, role)
    }

    /// [`Self::status_for`] on a queue the caller already opened, so a
    /// command opens it once.
    /// For the inbox and a planner it adds the `language` their prompt
    /// and the `SessionStart` hook carry (ADR-t616-2). `release_update` is
    /// the release check of the host's `[update]` (ADR-t618-1).
    pub fn status_of(
        &self,
        db: &Path,
        queue: &SqliteQueue,
        role: Option<SessionRole>,
    ) -> Result<Value> {
        let mut status = health::status(queue, &SystemProcesses, &*self.generators.clock, role)?;
        let live_builds: Vec<String> = status["supervisors"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|supervisor| supervisor["alive"] == true && supervisor["stale"] != true)
            .filter_map(|supervisor| supervisor["binary_version"].as_str().map(str::to_owned))
            .collect();
        status["release_update"] = crate::application::release_update::status(
            queue,
            &host_update(db).config,
            &live_builds,
        )?;
        if matches!(role, Some(SessionRole::Inbox | SessionRole::Planner)) {
            status["language"] = serde_json::to_value(self.language_report(queue)?)?;
        }
        Ok(status)
    }

    /// The language of the checkout `queue` is bound to over the user's
    /// (ADR-t616-2), with where it came from and any mistake.
    fn language_report(
        &self,
        queue: &SqliteQueue,
    ) -> Result<crate::infrastructure::language::LanguageReport> {
        Ok(crate::infrastructure::language::language_report(
            bound_checkout(queue)?.as_deref(),
            self.user_config.as_deref(),
        ))
    }

    /// `doctor`: see [`health::doctor`], with the queue's `schema` as
    /// `migrate --check` reports it. The schema is reported even for a
    /// queue that needs `migrate` or refuses this binary (ADR-0045 decision
    /// 5); a queue that refuses it has no `supervisors` or `runs`, and
    /// `error` says why. `common_dir`, when given, is the repository the
    /// queue must be bound to, checked either way.
    pub fn doctor(&self, db: &Path, full: bool, common_dir: Option<&str>) -> Result<Value> {
        let (schema, queue) = SqliteQueue::inspect_read_only(db)?;
        let mut report = match queue {
            ReadOnlyQueue::Refused { binding, error } => {
                if let Some(common_dir) = common_dir {
                    binding.assert_repository(common_dir)?;
                }
                serde_json::json!({
                    "checked_at": self.generators.clock.now(),
                    "error": format!("{error:#}"),
                })
            }
            ReadOnlyQueue::Readable(queue) => {
                let queue = queue.with_generators(self.generators.clone());
                if let Some(common_dir) = common_dir {
                    queue.assert_repository(common_dir)?;
                }
                let run_env = doctor_run_env(&queue, db).map_err(|error| format!("{error:#}"));
                let mut report = health::doctor(
                    &queue,
                    &SystemProcesses,
                    &LocalRunFiles,
                    &*self.generators.clock,
                    full,
                    run_env,
                )?;
                if let Some(repository) = doctor_repository(&queue)? {
                    report["repository"] = repository;
                }
                report["language"] = serde_json::to_value(self.language_report(&queue)?)?;
                report
            }
        };
        report["schema"] = serde_json::to_value(schema)?;
        // The resource broker's podman and last recorded state (ADR-t827-3).
        report["broker"] = doctor_broker(db);
        // The host's `[update]`, with what was taken as its default
        // (ADR-t618-1 decision 3).
        report["release_update"] = serde_json::to_value(host_update(db))?;
        // What `graph --format svg` draws with (ADR-0077 decision 4).
        report["d2"] = serde_json::to_value(crate::infrastructure::d2::Tools::on(
            std::env::var_os("PATH").as_deref(),
        ))?;
        // The worker providers `claude` and `codex` resolve to on this PATH
        // and the modes this binary runs them in (ADR-t813-2); what each
        // supervisor fixed is on its entry in `supervisors`.
        let (claude, transcripts, codex) = (
            ClaudeCode {
                executable: PathBuf::from("claude"),
            },
            ClaudeTranscripts::from_env(),
            Codex {
                executable: PathBuf::from("codex"),
            },
        );
        report["providers"] = serde_json::to_value(provider_checks(
            Path::new("claude"),
            Path::new("codex"),
            &worker_adapters(&claude, &transcripts, Some(&codex)),
        ))?;
        Ok(report)
    }

    /// `recover`: see [`health::recover`].
    pub fn recover(&self, db: &Path, id: &RunId) -> Result<Value> {
        let mut queue = self.open(db)?;
        health::recover(
            &mut queue,
            &SystemProcesses,
            &LocalRunFiles,
            &*self.generators.clock,
            id,
        )
    }

    /// `stats`: see [`statistics::stats`], measured to these generators'
    /// now. The run directories are read from disk as Claude Code writes
    /// them, `[stall]` and `[conflicts]` from the `dagq.toml` of the main
    /// checkout of the repository the queue is bound to, main's history
    /// from that checkout, and the workspaces from
    /// `workspaces` (`None`: `workspace_mismatch` is not judged).
    pub fn stats(
        &self,
        db: &Path,
        query: &StatsQuery,
        workspaces: Option<&dyn WorkspaceListing>,
    ) -> Result<Value> {
        self.stats_of(&self.open_read_only(db)?, db, query, workspaces)
    }

    /// [`Self::stats`] on `queue`, the queue at `db` the caller already
    /// opened, so a command opens it once.
    pub fn stats_of(
        &self,
        queue: &SqliteQueue,
        db: &Path,
        query: &StatsQuery,
        workspaces: Option<&dyn WorkspaceListing>,
    ) -> Result<Value> {
        let now = self.generators.clock.now();
        let checkout = bound_checkout(queue)?;
        let config_file = || match &checkout {
            Some(checkout) => load_stall_config(checkout),
            None => Ok(None),
        };
        let conflicts_file = || match &checkout {
            Some(checkout) => load_conflict_config(checkout),
            None => Ok(None),
        };
        let history = |since| match &checkout {
            Some(checkout) => GitRepository::inspect(checkout)?.main_history(since),
            None => anyhow::bail!("the queue is bound to no repository checkout"),
        };
        let signals = ClaudeCode {
            executable: PathBuf::from("claude"),
        };
        let queue_hash = QueueLocation::explicit(db).hash();
        let host_dir = db
            .parent()
            .unwrap_or(Path::new("."))
            .join(crate::domain::host_metrics::HOST_DIR);
        let host_metrics =
            |from, until| crate::infrastructure::host_metrics::summary(&host_dir, from, until);
        let sources = StatsSources {
            files: &LocalRunFiles,
            signals: &signals,
            workspaces,
            queue_hash: &queue_hash,
            config_file: &config_file,
            conflicts_file: &conflicts_file,
            history: &history,
            host_metrics: Some(&host_metrics),
        };
        Ok(serde_json::to_value(statistics::stats(
            queue,
            &SystemProcesses,
            now,
            query,
            &sources,
        )?)?)
    }

    /// `kpi` (ADR-0051 decision 9): see [`crate::domain::kpi::kpi`],
    /// measured to these generators' now in the host's time zone, judged by
    /// the `[kpi]` of the main checkout's `dagq.toml` with the host's
    /// `host.toml` (the queue's, over the host-wide one) over it.
    pub fn kpi_of(
        &self,
        queue: &SqliteQueue,
        db: &Path,
        query: &crate::domain::kpi::KpiQuery,
    ) -> Result<Value> {
        let now = self.generators.clock.now();
        let host_wide = crate::infrastructure::kpi_config::host_wide_file();
        let setup = report_setup(
            db,
            bound_checkout(queue)?.as_deref(),
            None,
            host_wide.as_deref(),
            now,
        )?;
        Ok(serde_json::to_value(crate::application::kpi::kpi(
            queue,
            now,
            setup.host,
            &setup.config,
            query,
        )?)?)
    }

    /// `forecast` (ADR-0070 decision 2): see
    /// [`crate::application::forecast::forecast`], at these generators' now,
    /// with the `min_samples` of the `[kpi]` settings `kpi` judges by.
    pub fn forecast_of(
        &self,
        queue: &SqliteQueue,
        db: &Path,
        query: &crate::application::forecast::ForecastQuery,
    ) -> Result<Value> {
        let now = self.generators.clock.now();
        let host_wide = crate::infrastructure::kpi_config::host_wide_file();
        let setup = report_setup(
            db,
            bound_checkout(queue)?.as_deref(),
            None,
            host_wide.as_deref(),
            now,
        )?;
        Ok(serde_json::to_value(
            crate::application::forecast::forecast(
                queue,
                &SystemProcesses,
                now,
                setup.config.min_samples,
                query,
            )?,
        )?)
    }

    /// What the observer reads of the KPIs (ADR-0051 decision 24): `kpi`
    /// of the last 7 days and the last 4 weeks as [`Self::kpi_of`] judges
    /// them, with the open breaches' events
    /// ([`crate::domain::kpi::observe::observer_input`]).
    pub fn observer_kpi(&self, queue: &SqliteQueue, db: &Path) -> Result<serde_json::Value> {
        use crate::domain::kpi::{KpiQuery, Period, observe};
        let now = self.generators.clock.now();
        let host_wide = crate::infrastructure::kpi_config::host_wide_file();
        let setup = report_setup(
            db,
            bound_checkout(queue)?.as_deref(),
            None,
            host_wide.as_deref(),
            now,
        )?;
        let of = |period, last| {
            crate::application::kpi::kpi(
                queue,
                now,
                setup.host,
                &setup.config,
                &KpiQuery {
                    period,
                    last,
                    ..KpiQuery::default()
                },
            )
        };
        let (day, week) = (of(Period::Day, 7)?, of(Period::Week, 4)?);
        Ok(observe::observer_input(
            &day,
            &week,
            &queue.kpi_breach_events_open()?,
        ))
    }

    /// The improvement proposals running against `[kpi]`'s
    /// `max_improvement_proposals` of the main checkout's `dagq.toml`
    /// (ADR-0051 decision 25), and when they reached it, the findings
    /// waiting for a planner because of it.
    pub fn improvements_of(&self, queue: &SqliteQueue) -> Result<serde_json::Value> {
        let limit = match bound_checkout(queue)? {
            Some(checkout) => max_improvement_proposals(&checkout)?,
            None => crate::domain::kpi::config::DEFAULT_MAX_IMPROVEMENT_PROPOSALS,
        };
        let improvements = queue.improvements(limit)?;
        let waiting: Vec<serde_json::Value> = if improvements.reached() {
            queue
                .planner_findings()?
                .iter()
                .map(|finding| {
                    serde_json::json!({"finding_id": finding.id, "reason": "improvement_limit"})
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(serde_json::json!({
            "running": improvements.running,
            "limit": improvements.limit,
            "reached": improvements.reached(),
            "waiting": waiting,
        }))
    }

    /// `report` (ADR-0051 decision 21): the KPI report of the `period` that
    /// holds `at` (now without; today's and this week's are partial),
    /// made as the supervisor makes it and written under `out` (the
    /// queue's `reports/` without), or with `print` only returned.
    pub fn report_of(
        &self,
        queue: &SqliteQueue,
        db: &Path,
        period: crate::domain::kpi::Period,
        at: Option<crate::domain::stats::Cursor>,
        out: Option<&Path>,
        print: bool,
    ) -> Result<Value> {
        use crate::application::report;
        let now = self.generators.clock.now();
        let host_wide = crate::infrastructure::kpi_config::host_wide_file();
        let setup = report_setup(
            db,
            bound_checkout(queue)?.as_deref(),
            out,
            host_wide.as_deref(),
            now,
        )?;
        let made = report::make(queue, &setup, now, period, at)?;
        if print {
            return Ok(serde_json::to_value(made)?);
        }
        Ok(serde_json::to_value(report::write(
            &LocalRunFiles,
            &setup,
            &made,
            period,
            now,
        )?)?)
    }

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
        cmux: &dyn WorkspaceBackend,
        launchd: &dyn LaunchAgent,
        processes: &dyn ProcessControl,
        environment: &UpEnvironment,
        options: &UpOptions,
    ) -> Result<Value> {
        let claude = ClaudeCode {
            executable: options.claude.clone(),
        };
        let migrated = self.migrate_compatible(&location.db, processes)?;
        let queues = |db: &Path| self.queues(db);
        let result = lifecycle::up(
            &self.lifecycle_ports(cmux, launchd, processes, &queues),
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

    /// Apply the migrations this binary knows and the queue at `db` lacks
    /// when every one of them is compatible, as the start of `up` and
    /// `install` does (ADR-0045 decisions 5, 15): a supervisor and the
    /// wrappers of an older binary go on with the migrated queue. A breaking
    /// one is refused with the way to it; `None` when nothing was pending
    /// (or there is no queue yet, which the use case reports).
    pub fn migrate_compatible(
        &self,
        db: &Path,
        processes: &dyn ProcessControl,
    ) -> Result<Option<crate::infrastructure::sqlite::MigrationReport>> {
        if !db.is_file() {
            return Ok(None);
        }
        let state = SqliteQueue::schema(db)?;
        if state.pending.is_empty() {
            return Ok(None);
        }
        let breaking: Vec<String> = state
            .pending
            .iter()
            .filter(|migration| !migration.compatible)
            .map(|migration| migration.version.to_string())
            .collect();
        ensure!(
            breaking.is_empty(),
            "the queue needs breaking migration(s) {} before this binary can run it, and a \
supervisor or run of the older binary could not open it afterwards: stop the supervisor \
(`down --wait`), run `dagq migrate`, then `up`; or let `dagq install --allow-breaking` do the \
same in one step",
            breaking.join(", ")
        );
        let alive = |pid| processes.alive(pid);
        Ok(Some(SqliteQueue::migrate(
            db,
            Some(&alive),
            self.generators.clock.now(),
        )?))
    }

    /// `down`: see [`lifecycle::down`].
    pub fn down(
        &self,
        location: &QueueLocation,
        cmux: &dyn WorkspaceBackend,
        launchd: &dyn LaunchAgent,
        processes: &dyn ProcessControl,
        options: &DownOptions,
    ) -> Result<Value> {
        let queues = |db: &Path| self.queues(db);
        let result = lifecycle::down(
            &self.lifecycle_ports(cmux, launchd, processes, &queues),
            &queue_paths(location),
            options,
        );
        trace_command("down", &result);
        result
    }

    /// `install`: replace the fixed binary and hand the queue's supervisor
    /// over to it (see [`installation::install`]). `cmux` stops an in-cmux
    /// supervisor when a breaking migration needs the drain.
    pub fn install(
        &self,
        location: &QueueLocation,
        cmux: &dyn WorkspaceBackend,
        launchd: &dyn LaunchAgent,
        options: &InstallOptions,
    ) -> Result<Value> {
        let queues = |db: &Path| self.queues(db);
        let down = || {
            self.down(
                location,
                cmux,
                launchd,
                &SystemProcesses,
                &DownOptions {
                    wait: true,
                    force: false,
                    poll: Duration::from_secs(2),
                },
            )
        };
        installation::install(
            &installation::Ports {
                binaries: &LocalBinaries,
                files: &LocalRunFiles,
                processes: &SystemProcesses,
                clock: &*self.generators.clock,
                queues: &queues,
                down: &down,
            },
            Some(&location.db),
            options,
        )
    }

    /// The automatic update's job (see [`update::run`]): build
    /// `job.commit` in the queue's update checkout and put it in place of
    /// `job.target`, handing the supervisor `job.token` over to it. An
    /// in-cmux supervisor that is gone afterwards is started again with the
    /// binary in place, through its `up --in-cmux --auto-update`; launchd
    /// restarts one of its own.
    pub fn auto_update(&self, location: &QueueLocation, job: &AutoUpdateJob) -> Result<Value> {
        let db = location
            .db
            .canonicalize()
            .context("queue must already be initialized")?;
        let queues = |db: &Path| self.queues(db);
        let restart_arguments = restart_arguments(
            &job.cmux,
            &job.claude,
            &job.codex,
            job.plugin_dir.as_deref(),
        )?;
        let restart = restarter(&db, &job.cmux, &job.target, &restart_arguments);
        update::run(
            &update::JobPorts {
                binaries: &LocalBinaries,
                files: &LocalRunFiles,
                processes: &SystemProcesses,
                clock: &*self.generators.clock,
                queues: &queues,
                restart: &restart,
            },
            &db,
            &update::JobOptions {
                commit: job.commit.clone(),
                token: job.token.clone(),
                target: job.target.clone(),
                repository: job.repository.clone(),
                paths: update::UpdatePaths::under(&location.queue_dir),
                log: job.log.clone(),
                build_command: job.build_command.clone(),
                restart: restart_arguments.clone(),
                handoff_timeout: job.handoff_timeout,
                watch_timeout: job.watch_timeout,
                poll: Duration::from_millis(500),
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
        let queues = |db: &Path| self.queues(db);
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
        cmux: &dyn WorkspaceBackend,
        launchd: &dyn LaunchAgent,
        index: &dyn crate::application::release_update::ReleaseIndex,
        cargo: &Path,
        requested: Option<&str>,
        options: &InstallOptions,
    ) -> Result<Value> {
        let version = crate::application::release_update::resolve_version(index, requested)?;
        let paths = update::UpdatePaths::under(&location.queue_dir);
        let log = location
            .queue_dir
            .join("logs")
            .join(format!("install-release-{version}.log"));
        let binary = installation::release_binary(
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
        .with_context(|| format!("install release {version}"))?;
        let mut report = self.install(
            location,
            cmux,
            launchd,
            &InstallOptions {
                source: installation::Source::Binary(binary),
                ..options.clone()
            },
        )?;
        report["release"] = json!(version);
        report["log"] = json!(log);
        Ok(report)
    }

    /// Open a planner a person talks with (`dagq plan`, ADR-0041 decision
    /// 6): preflight cmux and `options.claude`, then open a new workspace
    /// next to any planner still open (see
    /// [`planner::open_person_planner`]). `repo` is the checkout the
    /// planner works in.
    pub fn plan(
        &self,
        location: &QueueLocation,
        repo: &Path,
        cmux: &dyn WorkspaceBackend,
        options: &PlanOptions,
    ) -> Result<Value> {
        let db = location
            .db
            .canonicalize()
            .context("queue must already be initialized")?;
        let repository = GitRepository::inspect(repo)?;
        cmux.preflight()?;
        let claude = ClaudeCode {
            executable: options.claude.clone(),
        };
        claude.preflight()?;
        // Without --plugin-dir the planner loads the plugin the user
        // installed (ADR-t617-2 decisions 1, 4).
        if options.plugin_dir.is_none() {
            lifecycle::require_installed_plugin(
                &claude,
                &options.claude,
                &repository.root,
                "planner",
                "plan",
            )
            .map_err(|error| anyhow::anyhow!("{error:#}; no planner was opened"))?;
        }
        let plugin_dir = options
            .plugin_dir
            .as_deref()
            .map(|dir| {
                dir.canonicalize()
                    .with_context(|| format!("plugin directory {}", dir.display()))
            })
            .transpose()?;
        let queue = self.open(&db)?;
        let recording = RecordingBackend::over(cmux, self.queues(&db), None, load_average);
        // With no supervisor, the rows of planners whose workspace and
        // wrapper are gone are closed here; a listing that fails is a warning.
        let swept = planner::close_abandoned_planners(
            &queue,
            &recording,
            &SystemProcesses,
            &*self.generators.clock,
        );
        // And the runners of the planners nothing runs any more go.
        let runners = planner::remove_unused_planner_runners(
            &queue,
            &SystemProcesses,
            &LocalRunFiles,
            &*self.generators.clock,
            &planners_dir(&db),
        );
        // `dagq plan` reads `[roles.planner]` of the main checkout's
        // `dagq.toml` (ADR-0079 decision 7); a file it cannot read starts
        // the planner as before, with a warning.
        let roles = repository
            .checkout()
            .and_then(crate::infrastructure::run_env::load_role_models)
            .map_err(|error| format!("{error:#}"));
        let mut opened = planner::open_person_planner(&PlannerLaunch {
            queue: &queue,
            cmux: &recording,
            files: &LocalRunFiles,
            db: &db,
            queue_hash: &QueueLocation::explicit(&db).hash(),
            planners_dir: &planners_dir(&db),
            repo_root: &repository.root,
            runner: &options.runner,
            claude: &options.claude,
            plugin_dir: plugin_dir.as_deref(),
            language: crate::infrastructure::language::language_for_prompt(
                repository.checkout().ok(),
                options.user_config.as_deref(),
            ),
            roles: roles.clone().unwrap_or_default(),
        })?;
        if let Err(error) = &roles {
            opened.warnings.push(format!(
                "[roles.planner] could not be read; the planner starts as before: {error}"
            ));
        }
        for error in [swept.err(), runners.err()].into_iter().flatten() {
            opened.warnings.push(format!("{error:#}"));
        }
        Ok(serde_json::to_value(opened)?)
    }

    /// `planners`: every planner not closed (with `all`, every one), with
    /// its state judged by [`planner::planner_views`].
    pub fn planners(&self, db: &Path, cmux: &dyn WorkspaceBackend, all: bool) -> Result<Value> {
        self.planners_of(&self.open_read_only(db)?, db, cmux, all)
    }

    /// [`Self::planners`] on `queue`, the queue at `db` the caller already
    /// opened, so a command opens it once.
    pub fn planners_of(
        &self,
        queue: &SqliteQueue,
        db: &Path,
        cmux: &dyn WorkspaceBackend,
        all: bool,
    ) -> Result<Value> {
        // Claude Code's signals only read what its hook and screen show.
        let signals = ClaudeCode {
            executable: PathBuf::from("claude"),
        };
        // The screen stands in for a missing marker as the supervisor
        // judges it, without keeping the capture (ADR-t803-1).
        let stall = match bound_checkout(queue) {
            Ok(Some(checkout)) => load_stall_config(&checkout).ok().flatten(),
            _ => None,
        }
        .unwrap_or_default();
        let views = planner::planner_views(
            queue,
            &PlannerProbes {
                cmux,
                processes: &SystemProcesses,
                files: &LocalRunFiles,
                signals: &signals,
                clock: &*self.generators.clock,
                planners_dir: &planners_dir(db),
                screen_idle_secs: stall.screen_idle_secs,
                screen_idle: crate::application::screen_idle::ScreenIdle::Peek,
            },
            all,
        )?;
        Ok(serde_json::json!({ "planners": views }))
    }

    /// The queue at a path, as `up` and `down` open it, writing through
    /// these generators.
    fn queues(&self, db: &Path) -> Arc<dyn QueueOpener> {
        Arc::new(SqliteOpener {
            db: db.to_path_buf(),
            generators: self.generators.clone(),
            actor: None,
        })
    }

    /// The adapters `up` and `down` run on: the queue at a path through
    /// `SqliteQueue` with these generators, Git for the repository, the
    /// local files and Claude Code's global config for the folder trust.
    fn lifecycle_ports<'a>(
        &'a self,
        cmux: &'a dyn WorkspaceBackend,
        launchd: &'a dyn LaunchAgent,
        processes: &'a dyn ProcessControl,
        queues: &'a dyn Fn(&Path) -> Arc<dyn QueueOpener>,
    ) -> LifecyclePorts<'a> {
        LifecyclePorts {
            cmux,
            launchd,
            processes,
            files: &LocalRunFiles,
            clock: &*self.generators.clock,
            queues,
            inspect_repository: &inspect_repository,
            trusts_repository: &claude_trusts_repository,
            run_env_programs: &up_run_env_programs,
            resolve_language: &|checkout, user_config| {
                crate::infrastructure::language::resolve_language(Some(checkout), user_config)
            },
            load_average,
            agent: &|claude| {
                Box::new(ClaudeCode {
                    executable: claude.to_owned(),
                })
            },
        }
    }
}

/// The inbox workspace's command as `up` opens it: `claude` with the
/// inbox's prompt (see [`lifecycle::inbox_session_prompt`]) and the plugin
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
    .inbox_command(&prompt, plugin_dir)?;
    crate::application::actor_executor::command_line(&command)
}

/// `integrate` on the system clock and IDs: see [`OneShot::integrate`].
pub fn integrate(
    db: &Path,
    target: IntegrateTarget,
    repo: &Path,
    remote: Option<&dyn MainRemote>,
) -> Result<Value> {
    OneShot::system().integrate(db, target, repo, remote)
}

/// `status`: see [`status_for`], with all of the attention.
pub fn status(db: &Path) -> Result<Value> {
    status_for(db, None)
}

/// The host's `[update]` for the queue at `db`: its `host.toml` over the
/// host-wide one.
fn host_update(db: &Path) -> crate::infrastructure::release_update::HostUpdate {
    crate::infrastructure::release_update::load_host_update(
        db.parent().unwrap_or(Path::new(".")),
        crate::infrastructure::kpi_config::host_wide_file().as_deref(),
    )
}

/// `status --role` on the system clock: see [`OneShot::status_for`].
pub fn status_for(db: &Path, role: Option<SessionRole>) -> Result<Value> {
    OneShot::system().status_for(db, role)
}

/// `doctor` on the system clock: see [`OneShot::doctor`].
pub fn doctor(db: &Path, full: bool) -> Result<Value> {
    OneShot::system().doctor(db, full, None)
}

/// `recover` on the system clock: see [`OneShot::recover`].
pub fn recover(db: &Path, id: &RunId) -> Result<Value> {
    OneShot::system().recover(db, id)
}

/// `ask`: register an ask and, when it is new, tell a person with one
/// `cmux notify` aimed at the inbox workspace `up` recorded (see
/// [`crate::application::ask::ask`]).
pub fn ask(db: &Path, checkout: &Path, ask: NewAsk, cmux: &dyn WorkspaceBackend) -> Result<Value> {
    let mut queue = SqliteQueue::open(db)?;
    let binding = queue.repository_binding()?.map(PathBuf::from);
    let checkout = crate::infrastructure::adapters::naming_checkout(binding.as_deref(), checkout);
    crate::application::ask::ask(&mut queue, &checkout, ask, cmux)
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
    cmux: &dyn WorkspaceBackend,
    launchd: &dyn LaunchAgent,
    processes: &dyn ProcessControl,
    environment: &UpEnvironment,
    options: &UpOptions,
) -> Result<Value> {
    OneShot::system().up(
        location,
        repo,
        cmux,
        launchd,
        processes,
        environment,
        options,
    )
}

/// `down` on the system clock: see [`OneShot::down`].
pub fn down(
    location: &QueueLocation,
    cmux: &dyn WorkspaceBackend,
    launchd: &dyn LaunchAgent,
    processes: &dyn ProcessControl,
    options: &DownOptions,
) -> Result<Value> {
    OneShot::system().down(location, cmux, launchd, processes, options)
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

/// The programs the `[run.env]` of the main checkout of the repository the
/// queue is bound to names, resolved on this process's PATH (ADR-0049
/// decision 9).
fn doctor_run_env(queue: &SqliteQueue, db: &Path) -> Result<crate::domain::run_env::RunEnvCheck> {
    // A queue no supervisor or `up` bound has no repository to read, and a
    // repository without a main checkout (`repository` says why) no
    // `dagq.toml`.
    let Some(checkout) = bound_checkout(queue)? else {
        return Ok(crate::domain::run_env::RunEnvCheck::default());
    };
    ShellVerifier {
        checkout,
        db: db.to_path_buf(),
        user_config: None,
        verification_timeout: VERIFICATION_TIMEOUT,
    }
    .run_env_programs(None)
}

/// The landing branch and push of the repository the queue is bound to,
/// as `up`'s preflight resolves them (ADR-t615-1), or `error` with why
/// they do not resolve; `None` for a queue bound to no checkout.
fn doctor_repository(queue: &SqliteQueue) -> Result<Option<serde_json::Value>> {
    let Some(checkout) = bound_main_checkout(queue)? else {
        return Ok(None);
    };
    let resolved = checkout
        .and_then(|checkout| GitRepository::inspect(&checkout))
        .and_then(|repository| repository.resolve_repository_settings());
    Ok(Some(match resolved {
        // A configured push remote that is missing keeps the fields.
        Ok(settings) => {
            let mut value = serde_json::to_value(&settings)?;
            if let Err(error) = settings.push.check() {
                value["error"] = format!("{error:#}").into();
            }
            value
        }
        Err(error) => serde_json::json!({"error": format!("{error:#}")}),
    }))
}

/// What the KPIs and their reports of the queue at `db` are made with at
/// `now`: the `[kpi]` of `checkout`'s `dagq.toml` with the host's
/// `host.toml` (the queue's, over the host-wide one) over it, the host's
/// time zone and cores, the `[report]` retention of `host.toml`, the
/// reports' root (`out`, else `<queue dir>/reports/`), and the host's d2
/// and TALA on this process's PATH for the dependency diagram.
fn report_setup(
    db: &Path,
    checkout: Option<&Path>,
    out: Option<&Path>,
    host_wide: Option<&Path>,
    now: i64,
) -> Result<crate::application::report::ReportSetup> {
    use crate::infrastructure::{kpi_config::load_host_kpi, report_config::load_host_report};
    let repository = match checkout {
        Some(checkout) => load_kpi_settings(checkout)?,
        None => None,
    };
    let queue_dir = db.parent().unwrap_or(Path::new("."));
    let host = load_host_kpi(queue_dir, host_wide)?;
    Ok(crate::application::report::ReportSetup {
        root: out.map_or_else(|| queue_dir.join(REPORTS_DIR), Path::to_path_buf),
        host: crate::application::kpi::Host {
            utc_offset_secs: clock::local_utc_offset(now),
            cores: clock::logical_cores(),
        },
        config: crate::domain::kpi::KpiConfig::merge(repository.as_ref(), host.as_ref()),
        keep: load_host_report(queue_dir, host_wide)?,
        build: crate::VERSION.to_owned(),
        diagram: d2_renderer(std::env::var_os("PATH")),
    })
}

/// Draws the reports' dependency diagram with d2 and TALA on `path_var`
/// (ADR-0077 decisions 4 and 5).
fn d2_renderer(
    path_var: Option<std::ffi::OsString>,
) -> crate::application::report::DiagramRenderer {
    crate::application::report::DiagramRenderer(Arc::new(move |source| {
        crate::infrastructure::d2::render_svg(
            source,
            path_var.as_deref(),
            crate::infrastructure::d2::RENDER_TIMEOUT,
        )
    }))
}

/// `[kpi]`'s `max_improvement_proposals` of `checkout`'s `dagq.toml`, or
/// its default (ADR-0051 decision 25; the host's settings do not change
/// it).
fn max_improvement_proposals(checkout: &Path) -> Result<usize> {
    Ok(load_kpi_settings(checkout)?
        .and_then(|settings| settings.max_improvement_proposals)
        .unwrap_or(crate::domain::kpi::config::DEFAULT_MAX_IMPROVEMENT_PROPOSALS))
}

/// The KPI reports' directory in the queue's (ADR-0051 decision 20).
pub const REPORTS_DIR: &str = "reports";

/// The main checkout of the repository the queue is bound to; `None` for a
/// queue bound to none, or to one without a main checkout.
pub(crate) fn bound_checkout(queue: &SqliteQueue) -> Result<Option<PathBuf>> {
    Ok(bound_main_checkout(queue)?.and_then(Result::ok))
}

/// The main checkout of the repository the queue is bound to
/// ([`main_checkout_of`]), or why it has none; `None` for a queue bound to
/// no repository.
fn bound_main_checkout(queue: &SqliteQueue) -> Result<Option<Result<PathBuf>>> {
    Ok(queue
        .repository_binding()?
        .map(|common_dir| main_checkout_of(Path::new(&common_dir))))
}

/// The adapters of each worker this binary runs (ADR-t813-2), by provider
/// and mode: Claude Code's interactive session and its headless turns
/// (`claude -p`, ADR-t813-1), and Codex's headless turns (`codex exec`,
/// ADR-t813-3) when `codex` is given. A headless run has no screen, so
/// Codex's signals are never read; its spans are looked for as Claude's
/// and are not found (no active time or tokens are recorded for them).
pub fn worker_adapters<'a>(
    claude: &'a ClaudeCode,
    transcripts: &'a ClaudeTranscripts,
    codex: Option<&'a Codex>,
) -> WorkerAdapters<'a> {
    let adapter = WorkerAdapter {
        agent: claude,
        signals: claude,
        transcripts,
    };
    let workers = WorkerAdapters::default()
        .with(Worker::DEFAULT, adapter)
        .with(
            Worker {
                provider: Provider::Claude,
                mode: WorkerMode::Headless,
            },
            adapter,
        );
    match codex {
        Some(codex) => workers.with(
            Worker {
                provider: Provider::Codex,
                mode: WorkerMode::Headless,
            },
            WorkerAdapter {
                agent: codex,
                ..adapter
            },
        ),
        None => workers,
    }
}

/// Each provider's executable as `claude` and `codex` resolve, with the
/// modes `workers` runs it in: what a supervisor records on its
/// registration and `doctor` shows.
pub fn provider_checks(
    claude: &Path,
    codex: &Path,
    workers: &WorkerAdapters<'_>,
) -> Vec<ProviderCheck> {
    [(Provider::Claude, claude), (Provider::Codex, codex)]
        .into_iter()
        .map(|(provider, path)| {
            let resolved = match provider {
                Provider::Codex => crate::infrastructure::codex::executable(path),
                Provider::Claude => executable(path),
            };
            ProviderCheck {
                provider,
                executable: resolved.as_ref().map_or_else(
                    |_| path.display().to_string(),
                    |path| path.display().to_string(),
                ),
                found: resolved.is_ok(),
                error: resolved.err().map(|error| format!("{error:#}")),
                modes: workers.modes(provider),
            }
        })
        .collect()
}

/// What the recovery job of a run that ended reads about it (see
/// [`prompt::ended_run_material`]), its files read from `dir`.
pub fn ended_run_material(
    detail: &TaskDetail,
    run: &TaskRun,
    resumes: crate::domain::resume::ResumeCount,
    config: crate::domain::resume::ResumeConfig,
    dir: &Path,
) -> String {
    prompt::ended_run_material(&LocalRunFiles, detail, run, resumes, config, dir)
}

/// Run from cmux, not from a pipe; stdout must remain a terminal for Claude.
/// `resume` reopens the session of a `needs_session` run the supervisor is
/// resuming (ADR-0019) instead of starting the worker. A wrapper the run
/// refuses closes its own workspace (`CMUX_WORKSPACE_ID`) through `cmux`
/// when the run records no such workspace (task 806).
pub fn session(
    db: &Path,
    id: &RunId,
    token: &LeaseToken,
    claude: &Path,
    codex: &Path,
    resume: bool,
    cmux: &Path,
) -> Result<Value> {
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "interactive Claude wrapper requires a terminal"
    );
    let provider = ClaudeCode {
        executable: claude.into(),
    };
    let transcripts = ClaudeTranscripts::from_env();
    let codex = Codex {
        executable: codex.into(),
    };
    let workers = worker_adapters(&provider, &transcripts, Some(&codex));
    // The run's worker picks the adapters (ADR-t813-2); the supervisor
    // claims no task whose worker this binary has none for.
    let run = SqliteQueue::open(db)?.run(id)?;
    let worker = Worker::new(run.actual_provider(), run.worker_mode())?;
    let adapter = workers.get(worker).with_context(|| {
        format!(
            "run {id}: this binary has no adapters for a {} {} worker",
            worker.provider.as_str(),
            worker.mode.as_str()
        )
    })?;
    let cmux = Cmux {
        executable: cmux.into(),
    };
    // A headless turn outlives a wrapper killed with its workspace unless
    // the wrapper stops it (ADR-t813-1 decision 3).
    crate::infrastructure::process::stop_groups_on_exit_signals();
    run_session(
        db,
        id,
        token,
        adapter.agent,
        &LocalSpawner,
        resume,
        own_workspace(&cmux),
    )
}

/// The workspace this wrapper runs in, from cmux's `CMUX_WORKSPACE_ID`,
/// closed through `cmux` when its session refuses the wrapper.
fn own_workspace(cmux: &dyn WorkspaceBackend) -> Option<OwnWorkspace<'_>> {
    std::env::var(lifecycle::CMUX_WORKSPACE_ENV)
        .ok()
        .filter(|id| !id.trim().is_empty())
        .map(|id| OwnWorkspace { backend: cmux, id })
}

/// The wrapper with `provider`'s agent started by `spawner`.
pub fn session_with_provider(
    db: &Path,
    id: &RunId,
    token: &LeaseToken,
    provider: &dyn AgentProvider,
    spawner: &dyn Spawner,
) -> Result<Value> {
    run_session(db, id, token, provider, spawner, false, None)
}

/// The wrapper of a resumed session: `session --resume`.
pub fn resume_session_with_provider(
    db: &Path,
    id: &RunId,
    token: &LeaseToken,
    provider: &dyn AgentProvider,
    spawner: &dyn Spawner,
) -> Result<Value> {
    run_session(db, id, token, provider, spawner, true, None)
}

/// The wrapper (`resume` for `session --resume`) running in the workspace
/// `own`, which it closes when the run refuses it and records no such
/// workspace (task 806).
pub fn session_in_workspace(
    db: &Path,
    id: &RunId,
    token: &LeaseToken,
    provider: &dyn AgentProvider,
    spawner: &dyn Spawner,
    resume: bool,
    own: OwnWorkspace<'_>,
) -> Result<Value> {
    run_session(db, id, token, provider, spawner, resume, Some(own))
}

fn run_session(
    db: &Path,
    id: &RunId,
    token: &LeaseToken,
    provider: &dyn AgentProvider,
    spawner: &dyn Spawner,
    resume: bool,
    own_workspace: Option<OwnWorkspace<'_>>,
) -> Result<Value> {
    // The wrapper's events are its own, not the worker's (ADR-t728-1).
    let mut queue = SqliteQueue::open(db)?.with_actor(
        crate::domain::actor::ActorContext::instance(crate::domain::actor::ActorRole::Wrapper, id),
    );
    wrapper::run_session(
        Session {
            queue: &mut queue,
            db,
            provider,
            spawner,
            files: &LocalRunFiles,
            pid: std::process::id(),
            own_workspace,
        },
        id,
        token,
        resume,
    )
}

/// `review`: see [`reviewing::review`]. The run's checkout is opened as a
/// Git repository.
pub fn review(db: &Path, task_id: TaskId) -> Result<Value> {
    let mut queue = SqliteQueue::open(db)?;
    let open_repository = |checkout: &Path| -> Result<Box<dyn Repository>> {
        Ok(Box::new(GitRepository::inspect(checkout)?))
    };
    reviewing::review(
        Review {
            queue: &mut queue,
            files: &LocalRunFiles,
            open_repository: &open_repository,
            pid: std::process::id(),
        },
        task_id,
    )
}

/// What `dagq plan` opens a planner with: the resolved Claude Code
/// executable, the plugin directory its session loads, and the binary its
/// workspace runs as the session wrapper (this one).
#[derive(Debug, Clone)]
pub struct PlanOptions {
    pub claude: PathBuf,
    pub plugin_dir: Option<PathBuf>,
    pub runner: PathBuf,
    /// The user's `config.toml` the planner's language comes from under
    /// the repository's `dagq.toml` (ADR-t616-2); `None` reads none.
    pub user_config: Option<PathBuf>,
}

/// `plan` on the system clock: see [`OneShot::plan`].
pub fn plan(
    location: &QueueLocation,
    repo: &Path,
    cmux: &dyn WorkspaceBackend,
    options: &PlanOptions,
) -> Result<Value> {
    OneShot::system().plan(location, repo, cmux, options)
}

/// `planners` on the system clock: see [`OneShot::planners`].
pub fn planners(db: &Path, cmux: &dyn WorkspaceBackend, all: bool) -> Result<Value> {
    OneShot::system().planners(db, cmux, all)
}

/// The session wrapper of a planner (`planner-session`), run from its cmux
/// workspace: stdout must remain a terminal for Claude. Refused, it closes
/// its own workspace (`CMUX_WORKSPACE_ID`) through `cmux` when the planner
/// records no such workspace (task 806).
pub fn planner_session(
    db: &Path,
    id: PlannerId,
    claude: &Path,
    plugin_dir: Option<&Path>,
    model: Option<(&str, &str)>,
    cmux: &Path,
) -> Result<Value> {
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "interactive Claude wrapper requires a terminal"
    );
    let provider = ClaudeCode {
        executable: claude.into(),
    };
    let cmux = Cmux {
        executable: cmux.into(),
    };
    planner_session_with_provider(db, id, &provider, plugin_dir, model, own_workspace(&cmux))
}

/// [`planner_session`] with any provider, in the working directory, in the
/// workspace `own` (`None` knows none).
pub fn planner_session_with_provider(
    db: &Path,
    id: PlannerId,
    provider: &dyn AgentProvider,
    plugin_dir: Option<&Path>,
    model: Option<(&str, &str)>,
    own: Option<OwnWorkspace<'_>>,
) -> Result<Value> {
    let queue = SqliteQueue::open(db)?.with_actor(crate::domain::actor::ActorContext::instance(
        crate::domain::actor::ActorRole::Wrapper,
        format_args!("planner:{id}"),
    ));
    let cwd = std::env::current_dir().context("working directory is unavailable")?;
    planner::run_planner_session(
        PlannerWrapper {
            queue: &queue,
            db,
            provider,
            spawner: &LocalSpawner,
            files: &LocalRunFiles,
            pid: std::process::id(),
            own_workspace: own,
        },
        id,
        &planner::planner_dir(&planners_dir(db), id),
        &cwd,
        plugin_dir,
        model,
    )
}

/// `rebind` on the system clock: see [`OneShot::rebind`].
pub fn rebind(db: &Path, repo: &Path) -> Result<Value> {
    OneShot::system().rebind(db, repo)
}

/// `stats` on the system clock without cmux: see [`OneShot::stats`].
pub fn stats(db: &Path, query: &StatsQuery) -> Result<Value> {
    OneShot::system().stats(db, query, None)
}

/// `dagq mark <label>`: record a person's or a planner's change mark
/// (ADR-0051 decision 12), `at` the time the change took effect when it is
/// marked afterwards. Returns the mark as `marks` lists it.
pub fn record_mark(
    queue: &SqliteQueue,
    label: &str,
    note: Option<&str>,
    at: Option<crate::domain::stats::Cursor>,
    by: &str,
) -> Result<Value> {
    use crate::domain::marks::{self, MARK_RECORDED};
    let events = queue.all_events()?;
    let at = at
        .map(|cursor| {
            marks::cursor_time(cursor, &events)
                .context("--at names an event id this queue does not have")
        })
        .transpose()?;
    if let Some(at) = &at {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis();
        ensure!(
            crate::domain::stats::timestamp_millis(at)
                .is_some_and(|at| i128::from(at) <= now as i128),
            "--at {at} is in the future; a mark stands for a change already made"
        );
    }
    let payload = marks::mark_payload(label, note, at, by).map_err(anyhow::Error::msg)?;
    let id = queue.record_queue_event(MARK_RECORDED, payload)?;
    recorded_mark(queue, id)
}

/// `dagq mark --retract <id>`: record that the mark `target` was no change
/// (ADR-0051 decision 12); the mark stays, retracted.
pub fn retract_mark(
    queue: &SqliteQueue,
    target: crate::domain::EventId,
    by: &str,
) -> Result<Value> {
    use crate::domain::marks::{self, MARK_RETRACTED};
    let payload =
        marks::retraction_payload(&queue.all_events()?, target, by).map_err(anyhow::Error::msg)?;
    let id = queue.record_queue_event(MARK_RETRACTED, payload)?;
    recorded_mark(queue, id)
}

fn recorded_mark(queue: &SqliteQueue, id: crate::domain::EventId) -> Result<Value> {
    let mark = crate::domain::marks::marks(&queue.all_events()?, None, None)
        .into_iter()
        .find(|mark| mark.id == Some(id))
        .context("the recorded mark is not listed")?;
    Ok(serde_json::to_value(mark)?)
}

/// `dagq marks`: the recorded and derived change marks that took effect
/// after `since` and at or before `until`, oldest first.
pub fn marks(
    queue: &SqliteQueue,
    since: Option<crate::domain::stats::Cursor>,
    until: Option<crate::domain::stats::Cursor>,
) -> Result<Value> {
    let marks = crate::domain::marks::marks(&queue.all_events()?, since, until);
    Ok(json!({ "marks": marks }))
}

/// What `dagq broker start` is given.
#[derive(Debug, Clone, Default)]
pub struct BrokerStartOptions {
    /// A dagq checkout to build the image from; else the checkout this
    /// binary was built from, else the working directory's main checkout.
    pub source: Option<PathBuf>,
    /// The port on `127.0.0.1`; else the one the queue used last, else a
    /// free one.
    pub port: Option<u16>,
    /// The podman executable; else `podman` on `PATH`.
    pub podman: Option<PathBuf>,
    /// The working directory, for the source's fallback.
    pub cwd: PathBuf,
}

fn broker_container(
    location: &QueueLocation,
    port: u16,
) -> std::result::Result<
    crate::application::broker::ContainerSpec,
    crate::application::broker::BrokerFailure,
> {
    use crate::application::broker::{
        BrokerFailure, ContainerLimits, ContainerSpec, FailureCode, MACHINE, container_name,
        image_name,
    };
    let git_common_dir = location.git_common_dir.clone().ok_or_else(|| {
        BrokerFailure::new(
            FailureCode::RepositoryUnknown,
            "run from inside the repository the queue belongs to: the broker mounts its Git common dir",
        )
    })?;
    Ok(ContainerSpec {
        machine: MACHINE.to_owned(),
        name: container_name(&location.hash()),
        image: image_name(crate::VERSION),
        host_port: port,
        queue_dir: location.queue_dir.clone(),
        runs_dir: location.runs_dir.clone(),
        git_common_dir,
        limits: ContainerLimits::default(),
    })
}

/// `dagq broker start`: make the queue's broker run in dagq's Podman
/// machine and answer health on `127.0.0.1` ([`crate::application::broker::start`]).
/// The queue's lock keeps two starts of one queue apart; the machine has
/// its own host-wide lock. A failure is recorded in `state.json` and
/// returned as the [`crate::application::broker::BrokerFailure`].
pub fn broker_start(location: &QueueLocation, options: &BrokerStartOptions) -> Result<Value> {
    use crate::application::broker::{
        self as broker, BrokerFailure, FailureCode, HEALTH_TIMEOUT, HostLock, MachineSpec,
    };
    use crate::infrastructure::broker_podman::{
        BrokerState, CheckoutSource, FileLock, HttpHealth, PodmanCli, free_port, tokens_active,
    };
    let podman = PodmanCli::resolve(options.podman.as_deref())?;
    let queue_dir = &location.queue_dir;
    let _queue = FileLock::queue(queue_dir).hold()?;
    let mut state = BrokerState::read(queue_dir);
    let port = match options.port.or(state.port) {
        Some(port) => port,
        None => free_port()?,
    };
    let container = broker_container(location, port)?;
    let source = match &options.source {
        Some(checkout) => CheckoutSource {
            checkout: checkout.clone(),
        },
        None => CheckoutSource::of_this_build()
            .or_else(|| {
                main_checkout_of(&options.cwd)
                    .ok()
                    .filter(|checkout| CheckoutSource::is_source(checkout))
                    .map(|checkout| CheckoutSource { checkout })
            })
            .ok_or_else(|| {
                BrokerFailure::new(
                    FailureCode::ImageSourceMissing,
                    "no dagq checkout to build the broker's image from: pass --source",
                )
            })?,
    };
    // What the container mounts must exist before podman mounts it.
    crate::infrastructure::broker_token::ensure_key(queue_dir)?;
    for dir in [
        location.runs_dir.clone(),
        container.active(),
        container.audit(),
        container.git_common_dir.join("hooks"),
    ] {
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let machine = MachineSpec::default();
    let host_lock = FileLock::machine(&data_home()?);
    let health = HttpHealth::default();
    let ports = broker::Ports {
        podman: &podman,
        host_lock: &host_lock,
        health: &health,
    };
    let scratch = broker::broker_dir(queue_dir).join("build-context");
    let started = broker::start(
        &ports,
        &broker::StartRequest {
            machine: &machine,
            container: &container,
            source: &source,
            scratch: &scratch,
            in_use: tokens_active(queue_dir),
            health_timeout: HEALTH_TIMEOUT,
            health_interval: Duration::from_millis(500),
        },
    );
    let _ = std::fs::remove_dir_all(&scratch);
    state.port = Some(port);
    state.container = Some(container.name.clone());
    // A container kept for the runs that still hold tokens runs the image
    // recorded before.
    if !matches!(&started, Ok(report) if report.container_outcome.kept_stale) {
        state.image = Some(container.image.clone());
    }
    state.state = Some(match &started {
        Ok(_) => "running".to_owned(),
        Err(failure) => failure.code.as_str().to_owned(),
    });
    state.write(queue_dir)?;
    let report = started?;
    Ok(json!({
        "state": "running",
        "machine": machine,
        "start": report,
        "url": format!("http://127.0.0.1:{port}"),
    }))
}

/// `dagq broker stop`: stop the queue's container, then dagq's machine when
/// no container runs on it.
pub fn broker_stop(location: &QueueLocation, podman: Option<&Path>) -> Result<Value> {
    use crate::application::broker::{self as broker, HostLock, MACHINE, container_name};
    use crate::infrastructure::broker_podman::{BrokerState, FileLock, HttpHealth, PodmanCli};
    let podman = PodmanCli::resolve(podman)?;
    let queue_dir = &location.queue_dir;
    let _queue = FileLock::queue(queue_dir).hold()?;
    let host_lock = FileLock::machine(&data_home()?);
    let health = HttpHealth::default();
    let ports = broker::Ports {
        podman: &podman,
        host_lock: &host_lock,
        health: &health,
    };
    let container = container_name(&location.hash());
    let report = broker::stop(&ports, MACHINE, &container)?;
    let mut state = BrokerState::read(queue_dir);
    state.container = Some(container);
    state.state = Some("stopped".to_owned());
    state.write(queue_dir)?;
    Ok(json!({"state": "stopped", "stop": report}))
}

/// `dagq broker status`: the machine, the image, the container and the
/// health, read without changing anything. No podman is a state
/// (`podman_missing`), not an error.
pub fn broker_status(location: &QueueLocation, podman: Option<&Path>) -> Result<Value> {
    use crate::application::broker;
    use crate::infrastructure::broker_podman::{BrokerState, FileLock, HttpHealth, PodmanCli};
    let state = BrokerState::read(&location.queue_dir);
    let podman = match PodmanCli::resolve(podman) {
        Ok(podman) => podman,
        Err(failure) => {
            return Ok(json!({
                "state": failure.code.as_str(),
                "error": failure.to_json(),
                "recorded": state,
            }));
        }
    };
    let container = match broker_container(location, state.port.unwrap_or(0)) {
        Ok(container) => container,
        Err(failure) => {
            return Ok(json!({
                "state": failure.code.as_str(),
                "error": failure.to_json(),
                "recorded": state,
            }));
        }
    };
    let host_lock = FileLock::machine(&data_home()?);
    let health = HttpHealth::default();
    let ports = broker::Ports {
        podman: &podman,
        host_lock: &host_lock,
        health: &health,
    };
    let report = broker::status(&ports, &container, state.port);
    let mut value = serde_json::to_value(report)?;
    value["podman"] = json!(podman.executable);
    value["recorded"] = serde_json::to_value(state)?;
    Ok(value)
}

/// The `broker` of `doctor`: the podman executable (or why there is none)
/// and what `dagq broker` last recorded. It runs no podman command.
fn doctor_broker(db: &Path) -> Value {
    use crate::infrastructure::broker_podman::{BrokerState, PodmanCli};
    let queue_dir = db.parent().unwrap_or(Path::new("."));
    let podman = PodmanCli::resolve(None);
    json!({
        "mode": "disabled",
        "podman": podman.as_ref().ok().map(|podman| &podman.executable),
        "error": podman.as_ref().err().map(|failure| failure.to_json()),
        "machine": crate::application::broker::MACHINE,
        "recorded": BrokerState::read(queue_dir),
    })
}
