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
use crate::domain::{EventFilter, EventId, EventKind};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64},
    },
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
            self, DownOptions, Ports as LifecyclePorts, QueuePaths, RepositoryPaths, UpEnvironment,
            UpOptions,
        },
        planner::{self, PlannerProbes, PlannerWrapper},
        prompt,
        rebind::{self as rebinding, Rebind, RebindTarget},
        recording::RecordingBackend,
        review::{self as reviewing, Review},
        session::{self as wrapper, OwnWorkspace, Session, WrapperStart},
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
            claude_trusts_repository, executable, free_disk_bytes, host_versions, is_dagq_source,
            load_average, main_checkout_of, path_text,
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
            ShellVerifier, load_conflict_config, load_disk_config, load_exit_config,
            load_kpi_settings, load_resume_config, load_stall_config, load_supervisor_config,
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
    /// A shell command in place of the e2e the build passes before it is
    /// put in place (ADR-t963-1 decision 1; tests).
    pub e2e_command: Option<String>,
    /// How long that e2e may run.
    pub e2e_timeout: Duration,
    /// What an in-cmux supervisor started again uses.
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
    /// Explicit operator policy: never start Claude; unsupported roles wait for manual handling.
    pub no_claude: bool,
    /// Retry an unreadable review once (the production default). Tests
    /// unrelated to review retries can skip the second headless job.
    /// No CLI or repository setting overrides this policy.
    pub retry_unreadable_review: bool,
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
    /// Start the throughput reviews of each hour, day and ISO week
    /// (ADR-t996-1): off unless asked for (`supervise --throughput-review`,
    /// on by default in the CLI but for `--once`).
    pub throughput_review: bool,
    /// The host's time zone at a unix second; tests fix it.
    pub utc_offset: fn(i64) -> i64,
    /// Pause between two passes over the active runs; tests shorten it.
    pub tick: Duration,
    /// Pause between two looks for claimable work while no run is active;
    /// tests shorten it (`supervise --idle-poll-ms`).
    pub idle_poll: Duration,
    /// The longest a pass keeps the landing branch's resolution while its
    /// inputs look unchanged ([`supervisor::LANDING_BRANCH_RECHECK`]);
    /// tests lengthen it to see a change caught by the inputs alone
    /// (task 1078).
    pub landing_recheck: Duration,
    /// How often the supervisor heartbeats its registration and leases;
    /// tests shorten it (`supervise --heartbeat-interval-ms`, task 1048).
    pub heartbeat_interval: Duration,
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
    /// decision 12); a person's planners do not count. `None` follows
    /// `[supervisor] runtime_planners` as `parallel` does, else 1 (task
    /// 941).
    pub runtime_planners: Option<usize>,
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
    /// The e2e the supervisor runs of a run after its review (ADR-t1233-2).
    pub run_e2e: RunE2eOptions,
    /// `supervise --max-load` (task 327): no new run is claimed while the
    /// 1-minute load average is above it; `None` (the default here) holds
    /// for no load.
    pub max_load: Option<f64>,
    /// The seconds between new claims while `max_load` holds
    /// (ADR-t1479-1); 0 spaces none. `None` follows `[supervisor]
    /// claim_spacing` as `parallel` does, else 180. The CLI has no flag
    /// for it.
    pub claim_spacing: Option<usize>,
    /// Reads the 1-minute load average; tests set it.
    pub load_average: fn() -> Option<f64>,
    /// Reads `[conflicts]` of the `dagq.toml` in the main checkout, at the
    /// start and again each pass when `conflicts` is `None` (ADR-0080);
    /// tests set it to meet the same error at both (ADR-t775-1).
    pub load_conflicts: fn(&Path) -> Result<Option<crate::domain::stats::ConflictConfig>>,
    /// How much free disk space a claim and a landing need (task 377);
    /// `None` reads `[disk]` of the main checkout's `dagq.toml`.
    pub disk: Option<crate::domain::disk::DiskConfig>,
    /// The limit of a run's conflict-only attempts (ADR-0047 decision
    /// 24); `None` reads `[resume]` of the main checkout's `dagq.toml`.
    pub resume: Option<crate::domain::resume::ResumeConfig>,
    /// The retries of a `/exit` a session held back (ADR-0047 decision
    /// 25); `None` reads `[exit]` of the main checkout's `dagq.toml`.
    pub exit: Option<crate::domain::exit::ExitConfig>,
    /// Reads the free bytes of the file system of a path; tests set it.
    pub free_space: fn(&Path) -> Option<u64>,
    /// The directories Claude Code keeps the sessions' scratchpads under,
    /// whose ended runs' scratchpads the supervisor removes (task 1100);
    /// `None` is the host's, looked for at each cleanup
    /// ([`crate::infrastructure::run_files::claude_scratchpad_roots`]),
    /// which the CLI gives; a caller of the library (the tests) looks
    /// under none unless it gives its own.
    pub scratchpad_roots: Option<Vec<PathBuf>>,
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
    /// The host's processes the supervisor lists and signals; `None` is
    /// the system's. Tests set it (their sessions run inside the test
    /// process, which a supervisor never counts as a run's).
    pub processes: Option<ProcessesPort>,
    /// Reads crates.io's sparse index for the release check (ADR-t618-1);
    /// `None` is `curl`. Tests set it.
    pub release_index: Option<ReleaseIndexPort>,
    /// The build identifier the release check takes as the supervisor's;
    /// `None` is this binary's. Tests set it (a release build looks).
    pub release_current: Option<String>,
    /// The Codex CLI a Codex worker starts (`supervise --codex`, ADR-t813-2):
    /// resolved on PATH when found; a supervisor without it runs no Codex
    /// worker and no Codex job.
    pub codex: PathBuf,
    /// Codex's home, whose rollouts name the model of a Codex job
    /// (ADR-t1063-1); `None` is Codex's own (`$CODEX_HOME`, else
    /// `~/.codex`). Tests point it at a stub's; nothing there is written.
    pub codex_home: Option<PathBuf>,
    /// Record the host's load under `<queue dir>/host/` (task 516): off
    /// unless asked for (`supervise --host-metrics-interval`, 30 seconds
    /// by default in the CLI).
    pub host_metrics: Option<HostMetricsSettings>,
    /// The ports of the queue's resource broker and how often its health
    /// is looked at (ADR-t827-3); `None` is podman, the health over
    /// loopback and [`crate::application::supervise::BROKER_HEALTH_INTERVAL`].
    /// Only a mode other than `disabled` uses them. Tests set them.
    pub broker: Option<BrokerOptions>,
    /// Keep the queue's service running (ADR-t1233-4 decision 2): the
    /// supervisor `up` starts gets it; `None` (a `--once` pass, the tests)
    /// keeps none and holds nothing for it.
    pub queue_service: Option<QueueServiceOptions>,
    /// Counts the supervisor loop's passes, one at the top of each; tests
    /// keep a clone and wait for passes past a threshold (task 1046).
    pub passes: Arc<AtomicU64>,
    /// Keep the host's sccache server when `[run.env]`'s `RUSTC_WRAPPER`
    /// is sccache (ADR-t1215-1): the CLI's supervisor does; `None` (the
    /// tests unless they ask) looks at no server.
    pub sccache: Option<SccacheOptions>,
}

/// How the supervisor keeps the sccache server
/// ([`SuperviseOptions::sccache`]).
#[derive(Debug, Clone)]
pub struct SccacheOptions {
    /// How long a start may take before it is taken to have failed.
    pub start_timeout: Duration,
}

impl Default for SccacheOptions {
    fn default() -> Self {
        Self {
            start_timeout: crate::infrastructure::sccache::START_TIMEOUT,
        }
    }
}

/// How the supervisor keeps the queue's service ([`SuperviseOptions::queue_service`]).
#[derive(Debug, Clone)]
pub struct QueueServiceOptions {
    /// The binary the service runs: the supervisor's own.
    pub executable: PathBuf,
    /// The cmux a new ask notifies the inbox through.
    pub cmux: PathBuf,
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

/// What [`SuperviseOptions::broker`] gives the supervisor's broker.
#[derive(Clone)]
pub struct BrokerOptions {
    pub ports: crate::infrastructure::broker_queue::BrokerPorts,
    pub health_interval: Duration,
    /// How long a start or a restart waits for the health.
    pub health_timeout: Duration,
    /// The broker's client a worker is given instead of the one next to
    /// this dagq; `None` resolves that one.
    pub client: Option<PathBuf>,
}

impl std::fmt::Debug for BrokerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokerOptions")
            .field("health_interval", &self.health_interval)
            .field("health_timeout", &self.health_timeout)
            .finish_non_exhaustive()
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

/// The index [`SuperviseOptions::release_index`] gives the release check.
#[derive(Clone)]
pub struct ReleaseIndexPort(pub Arc<dyn crate::application::release_update::ReleaseIndex>);

impl std::fmt::Debug for ReleaseIndexPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReleaseIndexPort")
    }
}

/// The processes [`SuperviseOptions::processes`] gives the supervisor.
#[derive(Clone)]
pub struct ProcessesPort(pub Arc<dyn ProcessControl + Send + Sync>);

impl std::fmt::Debug for ProcessesPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProcessesPort")
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
            no_claude: false,
            retry_unreadable_review: true,
            parallel: Some(parallel),
            max_waiting: None,
            once,
            stop: Arc::new(AtomicBool::new(false)),
            observe_interval: Duration::ZERO,
            observe_daily: false,
            throughput_review: false,
            utc_offset: clock::local_utc_offset,
            tick: TICK,
            idle_poll: IDLE_POLL,
            landing_recheck: supervisor::LANDING_BRANCH_RECHECK,
            heartbeat_interval: supervisor::HEARTBEAT_INTERVAL,
            sweep_interval: SWEEP_INTERVAL,
            generators: clock::system(),
            stall: None,
            conflicts: None,
            runtime_planners: None,
            claim_spacing: None,
            planner_timeout: PLANNER_TIMEOUT,
            plugin_dir: None,
            handoff_token: None,
            mode: None,
            update: UpdateSettings::default(),
            run_e2e: RunE2eOptions::default(),
            // The CLI's `--max-load` has a default; a caller of the library
            // (the tests) holds for no load unless it asks to.
            max_load: None,
            load_average,
            load_conflicts: load_conflict_config,
            disk: None,
            resume: None,
            exit: None,
            free_space: free_disk_bytes,
            scratchpad_roots: Some(Vec::new()),
            report_daily: false,
            forecast_snapshots: false,
            forecast_check: crate::application::supervise::FORECAST_CHECK,
            host_config: None,
            diagram_path: None,
            user_config: None,
            push_retry: crate::domain::kpi::push::RETRY_DELAYS_SECS.map(Duration::from_secs),
            files: None,
            processes: None,
            release_index: None,
            release_current: None,
            codex: PathBuf::from("codex"),
            codex_home: None,
            host_metrics: None,
            broker: None,
            queue_service: None,
            passes: Arc::new(AtomicU64::new(0)),
            sccache: None,
        }
    }

    /// The flags `parallel`, `max_waiting`, `runtime_planners` and
    /// `claim_spacing` stand for.
    fn slot_flags(&self) -> SlotFlags {
        SlotFlags {
            parallel: self.parallel,
            max_waiting: self.max_waiting,
            runtime_planners: self.runtime_planners,
            claim_spacing: self.claim_spacing,
        }
    }

    fn settings(
        &self,
        stall: StallConfig,
        conflicts: ConflictConfigReport,
        disk: crate::domain::disk::DiskConfig,
        resume: crate::domain::resume::ResumeConfig,
        exit: crate::domain::exit::ExitConfig,
        limits: SlotLimits,
    ) -> LoopSettings {
        LoopSettings {
            no_claude: self.no_claude,
            retry_unreadable_review: self.retry_unreadable_review,
            limits,
            slot_flags: self.slot_flags(),
            once: self.once,
            stop: self.stop.clone(),
            observe_interval: self.observe_interval,
            observe_daily: self.observe_daily,
            throughput_review: self.throughput_review,
            utc_offset: self.utc_offset,
            tick: self.tick,
            idle_poll: self.idle_poll,
            landing_recheck: self.landing_recheck,
            heartbeat_interval: self.heartbeat_interval,
            sweep_interval: self.sweep_interval,
            stall,
            conflicts,
            conflicts_error: None,
            planner_timeout: self.planner_timeout,
            handoff_token: self.handoff_token.clone(),
            mode: self.mode,
            update: self.update.clone(),
            max_load: self.max_load,
            disk,
            resume,
            exit,
            passes: self.passes.clone(),
        }
    }
}

/// How the supervisor runs the e2e of a run after its review (ADR-t1233-2):
/// in dagq's source `cargo test --locked --test e2e -- --ignored` in the
/// run's worktree, with the cmux the supervisor uses, the `[run.env]` of
/// the main checkout, podman checked for the broker's tests, the host's e2e
/// lock and [`installation::E2E_TIMEOUT`]. Elsewhere there is none unless
/// `command` gives one.
#[derive(Debug, Clone, Default)]
pub struct RunE2eOptions {
    /// A shell command in place of the e2e (tests): it runs in any
    /// repository, with no cmux to ping and no podman to check unless
    /// `cmux` names one.
    pub command: Option<String>,
    /// How long it may run; `None` is [`installation::E2E_TIMEOUT`].
    pub timeout: Option<Duration>,
    /// The cmux it pings and cleans up after, in place of the supervisor's.
    pub cmux: Option<PathBuf>,
    /// How long a run waits before its e2e that could not run is tried
    /// again; `None` is [`crate::domain::run_e2e::RETRY_SECS`] (tests
    /// shorten it).
    pub retry: Option<Duration>,
    /// The host's e2e lock it takes, in place of
    /// [`installation::e2e_lock_path`] of the queue (which a `command`
    /// does not take unless it is given here).
    pub lock: Option<PathBuf>,
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
    // the values in use (ADR-0080). The error warned of here is not warned
    // of again by the first read again (ADR-t775-1).
    let (conflicts, conflicts_error) =
        crate::application::supervise::read_conflicts_at_start(options.conflicts, || {
            (options.load_conflicts)(&main_checkout)
        });
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
    // The retries of a `/exit` the session held back (ADR-0047 decision
    // 25): an `[exit]` that cannot be read leaves the defaults.
    let exit = options.exit.clone().unwrap_or_else(|| {
        load_exit_config(&main_checkout)
            .unwrap_or_else(|error| {
                tracing::warn!(error = %format_args!("{error:#}"), "[exit] of dagq.toml not read: {error:#}; using the defaults");
                None
            })
            .unwrap_or_default()
    });
    // `parallel`, `max_waiting`, `runtime_planners` and `claim_spacing`
    // the flags did not give (task 698, task 941, ADR-t1479-1): a
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
        .map(|executable| Codex {
            executable,
            // Codex's own home unless the options name one (tests).
            home: options.codex_home.clone(),
        })
        .filter(|codex| match codex.preflight() {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(error = %format_args!("{error:#}"), "codex {} does not run: {error:#}; no Codex worker", codex.executable.display());
                false
            }
        });
    let workers = worker_adapters(&agent, &transcripts, codex_agent.as_ref());
    let mut providers = provider_checks(claude, &codex, &workers);
    if options.no_claude {
        for provider in &mut providers {
            if provider.provider == Provider::Claude {
                provider.modes.clear();
                provider.error = Some("provider_disabled".into());
            }
        }
    }
    let layout = Layout {
        runs_dir: runs_dir(&db),
        queue_hash: QueueLocation::explicit(&db).hash(),
        repo_root: repository.root.clone(),
        main_checkout: main_checkout.clone(),
        common_dir: repository.common_dir.clone(),
        claude: claude.into(),
        cmux: options.update.cmux.clone().unwrap_or_else(|| "cmux".into()),
        codex: codex.clone(),
        codex_home: options.codex_home.clone(),
        providers,
        runner: runner.into(),
        pid,
        version: crate::VERSION.to_owned(),
        // The observe command's environment drops the supervisor's actor
        // variables, and the supervisor sets its own when it starts it
        // (`supervisor:<pid>`); its agent is the observer. It drops a
        // client-mode `dagq`'s variables too: the command opens the queue it
        // names (a supervisor started from a worker's tests inherits them).
        observer_env_remove: crate::domain::actor::ACTOR_ENV
            .into_iter()
            .chain(crate::domain::queue_service::CLIENT_ENV)
            .map(str::to_owned)
            .collect(),
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
    let review_material = move |task_id: TaskId, range: Option<&reviewing::ReviewRange>| {
        review_in(&review_db, task_id, range)
    };
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
        let load = options.load_conflicts;
        Arc::new(move || load(&checkout)) as crate::application::supervise::ConflictsFile
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
        // The filesystem of the run worktrees and their builds, the one
        // the disk checks read (task 1371): `runs/`, else the queue's dir.
        let runs = runs_dir(&db);
        let queue_dir = db.parent().unwrap_or(Path::new(".")).to_path_buf();
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
    });
    // The resource broker (ADR-t827-3 decision 2): `[broker]` of dagq.toml
    // lowered by host.toml; `disabled` gives no port and calls no podman,
    // only revoking the tokens an earlier mode left (task 1125), and
    // `required` does not start (Phase 2).
    let (broker, broker_leftovers) = {
        let queue_dir = db.parent().unwrap_or(Path::new(".")).to_path_buf();
        let host_wide = options
            .host_config
            .clone()
            .or_else(crate::infrastructure::kpi_config::host_wide_file);
        let setup = load_broker_setup(Some(&main_checkout), &queue_dir, host_wide.as_deref())?;
        for warning in &setup.host.warnings {
            tracing::warn!("[broker] of host.toml: {warning}");
        }
        if let Some(reason) = setup.mode.unsupported() {
            bail!("{reason}; the supervisor was not started");
        }
        let tokens: Arc<dyn crate::application::broker_run::RunTokens> =
            Arc::new(crate::infrastructure::broker_token::QueueRunTokens {
                queue_dir: queue_dir.clone(),
            });
        match setup.mode {
            crate::domain::broker::BrokerMode::Disabled => (None, Some(tokens)),
            mode => {
                let settings = options.broker.clone();
                let ports = match &settings {
                    Some(settings) => settings.ports.clone(),
                    None => crate::infrastructure::broker_queue::system_ports(
                        setup.host.config.podman.as_deref().map(Path::new),
                        &crate::infrastructure::broker_podman::machine_lock_home()?,
                    ),
                };
                let mut control = queue_broker(
                    queue_dir.clone(),
                    layout.runs_dir.clone(),
                    layout.queue_hash.clone(),
                    Some(layout.common_dir.clone()),
                    &setup,
                    ports,
                );
                if let Some(settings) = &settings {
                    control.health_timeout = settings.health_timeout;
                    control.health_interval = settings.health_interval.min(control.health_interval);
                }
                // The client next to this dagq, of its build (ADR-t827-1
                // decisions 5 and 7), unless the options name one.
                let client = match settings
                    .as_ref()
                    .and_then(|settings| settings.client.clone())
                {
                    Some(client) => Ok(client),
                    None => {
                        let dagq =
                            std::env::current_exe().unwrap_or_else(|_| PathBuf::from("dagq"));
                        crate::application::actor_executor::HostActorExecutor::new(&db)
                            .broker_client(
                                &dagq,
                                &crate::infrastructure::broker_podman::client_version,
                            )
                    }
                };
                if let Err(failure) = &client {
                    tracing::warn!(
                        code = failure.code.as_str(),
                        "workers get no broker tools: {failure}"
                    );
                }
                let port = crate::application::supervise::BrokerPort {
                    mode,
                    tokens,
                    client,
                    control: Arc::new(control),
                    health_interval: settings.map_or(
                        crate::application::supervise::BROKER_HEALTH_INTERVAL,
                        |settings| settings.health_interval,
                    ),
                };
                (Some(port), None)
            }
        }
    };
    let run_e2e = {
        let queue_dir = db.parent().unwrap_or(Path::new(".")).to_path_buf();
        let command = options.run_e2e.command.clone();
        let stub = command.is_some();
        if stub || crate::infrastructure::adapters::is_dagq_source(&main_checkout) {
            Some(crate::application::supervise::RunE2ePort {
                settings: installation::E2eSettings {
                    command,
                    timeout: options.run_e2e.timeout.unwrap_or(installation::E2E_TIMEOUT),
                    cmux: options
                        .run_e2e
                        .cmux
                        .clone()
                        .or_else(|| (!stub).then(|| layout.cmux.clone())),
                    run_env_root: Some(main_checkout.clone()),
                    queue_dir: Some(queue_dir.clone()),
                    scratch: queue_dir.join("e2e"),
                    // The run's, set for each e2e.
                    log: queue_dir.join("e2e.log"),
                    // The broker's e2e, as the automatic update's gate
                    // checks it (ADR-t1162-1); a command in place of the
                    // e2e has none.
                    podman: if stub {
                        None
                    } else {
                        Some(crate::infrastructure::e2e_gate::podman_check()?)
                    },
                    utc_offset_secs: 0,
                    lock: options.run_e2e.lock.clone().or_else(|| {
                        (!stub)
                            .then(|| installation::e2e_lock_path(&queue_dir))
                            .flatten()
                    }),
                },
                run: Arc::new(|worktree, settings| {
                    crate::infrastructure::e2e_gate::run(worktree, None, settings)
                }),
                retry: options
                    .run_e2e
                    .retry
                    .unwrap_or(Duration::from_secs(crate::domain::run_e2e::RETRY_SECS)),
            })
        } else {
            None
        }
    };
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
        // The Codex a role's jobs run on (ADR-t1063-1): the one the Codex
        // workers run, found and running.
        codex_jobs: codex_agent
            .as_ref()
            .map(|codex| codex as &dyn AgentProvider),
        spawner: &LocalSpawner,
        service_access: &crate::infrastructure::queue_service::SystemServiceAccess,
        files: options.files.as_ref().map_or_else(
            || Arc::new(LocalRunFiles) as Arc<dyn RunFiles>,
            |port| port.0.clone(),
        ),
        processes: options.processes.as_ref().map_or_else(
            || Arc::new(SystemProcesses) as Arc<dyn ProcessControl + Send + Sync>,
            |port| port.0.clone(),
        ),
        generators,
        review_material: &review_material,
        load_average: options.load_average,
        free_space: options.free_space,
        scratchpad_roots: options.scratchpad_roots.clone().map_or_else(
            || {
                Arc::new(crate::infrastructure::run_files::claude_scratchpad_roots)
                    as crate::application::supervise::ScratchpadRoots
            },
            |roots| {
                Arc::new(move || roots.clone()) as crate::application::supervise::ScratchpadRoots
            },
        ),
        host_versions,
        reports,
        max_improvement_proposals,
        conflicts_file,
        supervisor_file,
        forecasts,
        release: Some(release),
        host_metrics,
        broker,
        broker_leftovers,
        queue_service: options.queue_service.as_ref().map(|settings| {
            crate::application::supervise::QueueServicePort {
                control: settings.control.as_ref().map_or_else(
                    || {
                        Arc::new(
                            crate::infrastructure::queue_service::SystemQueueService::new(
                                &db,
                                &settings.executable,
                                &settings.cmux,
                            ),
                        )
                            as Arc<dyn crate::application::queue_service::QueueServiceControl>
                    },
                    |port| port.0.clone(),
                ),
                interval: settings.interval,
                start_timeout: settings.start_timeout,
            }
        }),
        run_e2e,
        sccache: options.sccache.as_ref().map(|settings| {
            crate::application::supervise::SccachePort(Arc::new(
                crate::infrastructure::sccache::SystemSccache::new(
                    db.parent().unwrap_or(Path::new(".")),
                    settings.start_timeout,
                ),
            ))
        }),
        layout,
    };
    let settings = LoopSettings {
        conflicts_error,
        ..options.settings(stall, conflicts, disk, resume, exit, limits)
    };
    supervisor::supervise(&ports, &settings)
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
    /// The ports of the queue's resource broker `down` stops; `None` is
    /// podman. Tests set them.
    pub broker: Option<BrokerOptions>,
    /// The host-wide `host.toml` `up` and `down` read `[broker]` from;
    /// `None` is `$XDG_CONFIG_HOME/dagq/host.toml`. Tests set it.
    pub host_config: Option<PathBuf>,
}

impl OneShot {
    pub fn new(generators: Generators) -> Self {
        Self {
            generators,
            user_config: None,
            disk: None,
            free_space: free_disk_bytes,
            verification_timeout: VERIFICATION_TIMEOUT,
            broker: None,
            host_config: None,
        }
    }

    /// The host-wide `host.toml` `up` and `down` read.
    fn host_wide(&self) -> Option<PathBuf> {
        self.host_config
            .clone()
            .or_else(crate::infrastructure::kpi_config::host_wide_file)
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
            supervisor::HeartbeatPolicy::leases(supervisor::HEARTBEAT_INTERVAL),
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
        if matches!(role, None | Some(SessionRole::Inbox)) {
            status["inbox_guardrail"] = inbox_guardrail(queue)?;
            status["inbox_watcher"] = self.inbox_watcher(db);
            status["broker"] = status_broker(db, queue);
            status["queue_service"] = queue_service_view(db, Some(queue));
        }
        Ok(status)
    }

    /// Whether the inbox has a watcher now (ADR-t906-1), from the records
    /// the inbox's watches left under the queue's directory and the
    /// processes under their pids (task 927).
    fn inbox_watcher(&self, db: &Path) -> Value {
        crate::application::inbox_watcher::judge_with(
            &crate::infrastructure::inbox_watchers::read(
                &crate::infrastructure::inbox_watchers::dir(db),
            ),
            &SystemProcesses,
            self.generators.clock.now(),
        )
        .to_json()
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
        // The broker's mode and health, from a queue that can be read.
        let mut broker_view = None;
        let mut service_view = None;
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
                broker_view = Some(broker_queue_view(db, &queue));
                service_view = Some(queue_service_view(db, Some(&queue)));
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
                report["roles"] = doctor_roles(&queue);
                // Whether the recorded inbox refuses raw cmux (ADR-t1228-2
                // decision 4).
                report["inbox_guardrail"] = inbox_guardrail(&queue)?;
                report["language"] = serde_json::to_value(self.language_report(&queue)?)?;
                report
            }
        };
        report["schema"] = serde_json::to_value(schema)?;
        // The inbox's watcher (ADR-t906-1).
        report["inbox_watcher"] = self.inbox_watcher(db);
        // The resource broker's podman and last recorded state (ADR-t827-3).
        report["broker"] = doctor_broker(db, broker_view.as_ref());
        // The queue service (ADR-t1233-4): whether it answers, and its
        // attention from a queue that can be read.
        report["queue_service"] = service_view.unwrap_or_else(|| queue_service_view(db, None));
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
            Codex::new(PathBuf::from("codex")),
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
            areas: &area_reader(checkout.as_deref())?,
            utc_offset_secs: clock::local_utc_offset(now),
            dagq_source: checkout.as_deref().is_some_and(is_dagq_source),
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
            &setup.areas,
            query,
            setup
                .host_metrics
                .as_ref()
                .map(|read| crate::domain::kpi::HostReader(read.0.as_ref())),
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
                &setup.areas,
                &KpiQuery {
                    period,
                    last,
                    ..KpiQuery::default()
                },
                None,
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
            // A substitute command has no broker e2e and must not touch podman.
            podman: match job.e2e_command {
                Some(_) => None,
                None => Some(crate::infrastructure::e2e_gate::podman_check()?),
            },
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
                source: installation::Source::Built(binary),
                ..options.clone()
            },
        )?;
        report["release"] = json!(version);
        report["log"] = json!(log);
        Ok(report)
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
                screen_idle_threshold: stall.screen_idle(),
                screen_idle: crate::application::screen_idle::ScreenIdle::Peek,
            },
            all,
        )?;
        Ok(serde_json::json!({ "planners": views }))
    }

    /// `planner request ID`: hand `words` to the headless planner `planner`
    /// of `queue` (at `db`) as its next turn, the planner judged as
    /// [`Self::planners_of`] judges it ([`planner_request::request_planner`]).
    pub fn request_planner(
        &self,
        queue: &mut SqliteQueue,
        db: &Path,
        cmux: &dyn WorkspaceBackend,
        planner: PlannerId,
        words: &str,
    ) -> Result<Value> {
        let signals = ClaudeCode {
            executable: PathBuf::from("claude"),
        };
        let stall = match bound_checkout(queue) {
            Ok(Some(checkout)) => load_stall_config(&checkout).ok().flatten(),
            _ => None,
        }
        .unwrap_or_default();
        let probes = PlannerProbes {
            cmux,
            processes: &SystemProcesses,
            files: &LocalRunFiles,
            signals: &signals,
            clock: &*self.generators.clock,
            planners_dir: &planners_dir(db),
            screen_idle_threshold: stall.screen_idle(),
            screen_idle: crate::application::screen_idle::ScreenIdle::Peek,
        };
        crate::application::planner_request::request_planner(queue, &probes, planner, words)
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
            broker: self,
            queue_service: &|db, executable, cmux| {
                Box::new(
                    crate::infrastructure::queue_service::SystemQueueService::new(
                        db, executable, cmux,
                    ),
                )
            },
        }
    }
}

impl lifecycle::BrokerLifecycle for OneShot {
    fn preflight(&self, checkout: &Path, db: &Path, path: &str) -> Result<Option<Value>> {
        let queue_dir = db.parent().context("queue database has no directory")?;
        let setup = load_broker_setup(Some(checkout), queue_dir, self.host_wide().as_deref())?;
        if setup.mode == crate::domain::broker::BrokerMode::Disabled {
            return Ok(None);
        }
        if let Some(reason) = setup.mode.unsupported() {
            bail!("{reason}");
        }
        // The supervisor runs podman from this PATH (ADR-t827-3 decision
        // 2); a person installs it, dagq does not.
        let wanted = setup.host.config.podman.as_deref().unwrap_or("podman");
        let podman = crate::infrastructure::run_env::resolve_program(
            wanted,
            Some(std::ffi::OsStr::new(path)),
        )
        .with_context(|| {
            format!(
                "[broker] mode = \"{}\" of dagq.toml needs podman, and {wanted} is not found (PATH: {path}); \
a person installs it (brew install podman), or sets [broker] mode = \"disabled\" in the queue's host.toml",
                setup.mode.as_str()
            )
        })?;
        Ok(Some(json!({
            "mode": setup.mode,
            "podman": podman,
            "warnings": setup.host.warnings,
        })))
    }

    fn after_drain(&self, db: &Path, drained: bool) -> Result<Option<Value>> {
        let queue = self.open_read_only(db)?;
        let checkout = bound_checkout(&queue)?;
        let queue_dir = db.parent().context("queue database has no directory")?;
        let setup = load_broker_setup(checkout.as_deref(), queue_dir, self.host_wide().as_deref())?;
        if setup.mode == crate::domain::broker::BrokerMode::Disabled {
            return Ok(None);
        }
        if !drained {
            return Ok(Some(json!({
                "stopped": false,
                "reason": "the supervisor still drains: it stops the broker once its drain ends",
            })));
        }
        let location = QueueLocation::explicit(&db.canonicalize()?);
        let common_dir = queue.repository_binding()?.map(PathBuf::from);
        drop(queue);
        let ports = match &self.broker {
            Some(options) => options.ports.clone(),
            None => crate::infrastructure::broker_queue::system_ports(
                setup.host.config.podman.as_deref().map(Path::new),
                &crate::infrastructure::broker_podman::machine_lock_home()?,
            ),
        };
        let broker = queue_broker(
            location.queue_dir.clone(),
            location.runs_dir.clone(),
            location.hash(),
            common_dir,
            &setup,
            ports,
        );
        let report = broker.stop()?;
        // The supervisor may have stopped it at the end of its drain.
        let gvproxy_acted = report
            .gvproxy
            .as_ref()
            .is_some_and(crate::application::broker::GvproxyCleanup::acted);
        if report.container_stopped || report.machine_stopped || gvproxy_acted {
            self.open(db)?.record_queue_event(
                EventKind::BrokerStopped,
                json!({
                    "container": broker.container(),
                    "container_stopped": report.container_stopped,
                    "machine_stopped": report.machine_stopped,
                    "gvproxy": report.gvproxy,
                    "by": "down",
                }),
            )?;
        }
        Ok(Some(json!({"stopped": true, "stop": report})))
    }

    fn request_stop(&self, db: &Path, supervisors: &[LeaseToken]) -> Result<bool> {
        let queue = self.open_read_only(db)?;
        let checkout = bound_checkout(&queue)?;
        drop(queue);
        let queue_dir = db.parent().context("queue database has no directory")?;
        let setup = load_broker_setup(checkout.as_deref(), queue_dir, self.host_wide().as_deref())?;
        if setup.mode == crate::domain::broker::BrokerMode::Disabled {
            return Ok(false);
        }
        self.open(db)?.record_queue_event(
            EventKind::BrokerStopRequested,
            json!({"supervisors": supervisors, "by": "down"}),
        )?;
        Ok(true)
    }
}

/// The inbox workspace's command as `up` opens it: `claude` with the
/// inbox's settings (written under the queue's directory, ADR-t1228-2
/// decision 3), its prompt (see [`lifecycle::inbox_session_prompt`]) and
/// the plugin directory, made by the provider the executor starts the
/// inbox with.
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

/// Whether the inbox `up` recorded was opened with the guardrail that
/// refuses raw `cmux` ([`crate::application::inbox_guardrail::judge`]), by
/// its newest `inbox_opened`.
fn inbox_guardrail(queue: &SqliteQueue) -> Result<Value> {
    let recorded = queue.session_workspace(SessionRole::Inbox)?;
    let opened = queue.latest_event_of(EventKind::InboxOpened.as_str())?;
    Ok(crate::application::inbox_guardrail::judge(
        recorded.as_deref(),
        opened.as_ref().map(|event| &event.payload),
    ))
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

/// The provider, model and effort each role other than the worker's starts
/// with, and where its provider comes from (`dagq.toml` or `default`), as
/// the `[roles.*]` of the `dagq.toml` of the bound main checkout says
/// (ADR-t1063-1 decisions 1 and 6), keyed by role. A file that cannot be
/// read adds `error`, and every role starts as before meanwhile.
fn doctor_roles(queue: &SqliteQueue) -> serde_json::Value {
    use crate::domain::actor_model::{ModelRole, RoleModels};
    let read = match bound_checkout(queue) {
        Ok(Some(checkout)) => crate::infrastructure::run_env::load_role_models(&checkout),
        Ok(None) => Ok(RoleModels::default()),
        Err(error) => Err(error),
    };
    let (models, error) = match read {
        Ok(models) => (models, None),
        Err(error) => (RoleModels::default(), Some(format!("{error:#}"))),
    };
    let mut roles = serde_json::Map::new();
    for role in ModelRole::ALL {
        let (provider, source) = models.provider(role);
        let launch = models.launch(role);
        let mut entry = serde_json::json!({
            "provider": provider,
            "source": source,
            "model": launch.model,
            "effort": launch.effort,
        });
        // The route the runtime's planners open on, and where it comes
        // from (ADR-t1394-2 decision 1).
        if role == ModelRole::RuntimePlanner {
            let (route, route_source) = models.planner_route();
            entry["route"] = serde_json::json!(route);
            entry["route_source"] = serde_json::json!(route_source);
        }
        roles.insert(role.as_str().to_owned(), entry);
    }
    if let Some(error) = error {
        roles.insert("error".to_owned(), error.into());
    }
    serde_json::Value::Object(roles)
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
            dagq_source: checkout.is_some_and(is_dagq_source),
        },
        config: crate::domain::kpi::KpiConfig::merge(repository.as_ref(), host.as_ref()),
        keep: load_host_report(queue_dir, host_wide)?,
        build: crate::VERSION.to_owned(),
        diagram: d2_renderer(std::env::var_os("PATH")),
        areas: area_reader(checkout)?,
        host_metrics: Some(host_metrics_reader(
            queue_dir.join(crate::domain::host_metrics::HOST_DIR),
        )),
    })
}

/// The `[tasks] changes` of the `dagq.toml` of the main checkout `queue`
/// is bound to (ADR-t980-1), which `add`, `edit`, `submit` and `lint` hold
/// the tasks to; none for a queue bound to no checkout or without them.
pub fn task_changes(queue: &SqliteQueue) -> Result<Option<crate::domain::ChangeSet>> {
    match bound_checkout(queue)? {
        Some(checkout) => crate::infrastructure::run_env::load_change_set(&checkout),
        None => Ok(None),
    }
}

/// The `[areas]` of `checkout`'s `dagq.toml` and its Git for the landed
/// commits' changes (ADR-t980-1); no checkout or no `[areas]` gives no run
/// areas.
fn area_reader(checkout: Option<&Path>) -> Result<crate::application::areas::AreaReader> {
    use crate::application::areas::AreaReader;
    let Some(checkout) = checkout else {
        return Ok(AreaReader::none());
    };
    let map = crate::infrastructure::run_env::load_area_map(checkout)?;
    let checkout = checkout.to_path_buf();
    Ok(AreaReader {
        map,
        changed: Arc::new(move |commits: &[String]| {
            GitRepository::inspect(&checkout)?.landed_changes(commits)
        }),
    })
}

/// Summarizes the host's load records under `dir` (task 872).
fn host_metrics_reader(dir: PathBuf) -> crate::application::report::HostMetrics {
    crate::application::report::HostMetrics(std::sync::Arc::new(move |from, until| {
        crate::infrastructure::host_metrics::summary(&dir, from, until)
    }))
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
        .with(Worker::CLAUDE_INTERACTIVE, adapter)
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
/// when the run records no such workspace (task 806). Started in the
/// background (`background`, ADR-t1404-1), it needs no terminal and has no
/// workspace of its own ([`wrapper_entry`]).
#[allow(clippy::too_many_arguments)]
pub fn session(
    db: &Path,
    id: &RunId,
    token: &LeaseToken,
    claude: &Path,
    codex: &Path,
    resume: bool,
    cmux: &Path,
    background: bool,
) -> Result<Value> {
    let start = wrapper_entry(background)?;
    let provider = ClaudeCode {
        executable: claude.into(),
    };
    let transcripts = ClaudeTranscripts::from_env();
    let codex = Codex::new(codex.into());
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
    // The other provider's headless turns, for a run the supervisor moves
    // there when its own provider cannot be used (ADR-t813-2).
    let other = workers
        .get(Worker {
            provider: worker.provider.other(),
            mode: WorkerMode::Headless,
        })
        .map(|adapter| adapter.agent);
    let cmux = Cmux {
        executable: cmux.into(),
    };
    // A headless turn outlives a wrapper killed with its workspace, or sent
    // SIGTERM in the background, unless the wrapper stops it (ADR-t813-1
    // decision 3, ADR-t1404-1 decision 3).
    crate::infrastructure::process::stop_groups_on_exit_signals();
    run_session(
        db,
        id,
        token,
        adapter.agent,
        other,
        &LocalSpawner,
        resume,
        match start {
            WrapperStart::Workspace => own_workspace(&cmux),
            WrapperStart::Background => None,
        },
        start,
        // The workspace's [run.env] is this process's environment, which
        // each turn inherits (ADR-t1215-1).
        crate::domain::sccache::SccacheTarget::of_pairs(
            &std::env::vars_os()
                .filter_map(|(key, value)| {
                    Some((key.into_string().ok()?, value.into_string().ok()?))
                })
                .collect::<Vec<_>>(),
        ),
    )
}

/// The entry of a session wrapper: one started in a workspace needs its
/// terminal (stdin and stdout), which the agent of an interactive session
/// takes. One started in the background (ADR-t1404-1) has none: it reads
/// nothing, its output goes to its log, and it leads a session and a
/// process group of its own, which the supervisor's signals stop (a wrapper
/// that leads one already keeps it). A planner's wrapper is entered the
/// same way.
pub fn wrapper_entry(background: bool) -> Result<WrapperStart> {
    let start = WrapperStart::of_flag(background);
    match start {
        WrapperStart::Workspace => ensure!(
            std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
            "interactive Claude wrapper requires a terminal"
        ),
        // SAFETY: setsid(2) touches no memory; it fails only for a process
        // that leads its group already.
        WrapperStart::Background => {
            unsafe { libc::setsid() };
        }
    }
    Ok(start)
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
    run_session(
        db,
        id,
        token,
        provider,
        None,
        spawner,
        false,
        None,
        WrapperStart::Workspace,
        None,
    )
}

/// The wrapper (`resume` for `session --resume`) with `provider`'s agent
/// and, for a headless run moved to the other provider (ADR-t813-2),
/// `other`'s, started by `spawner`.
pub fn session_with_providers(
    db: &Path,
    id: &RunId,
    token: &LeaseToken,
    provider: &dyn AgentProvider,
    other: Option<&dyn AgentProvider>,
    spawner: &dyn Spawner,
    resume: bool,
) -> Result<Value> {
    run_session(
        db,
        id,
        token,
        provider,
        other,
        spawner,
        resume,
        None,
        WrapperStart::Workspace,
        None,
    )
}

/// [`session_with_providers`] started in the background (ADR-t1404-1):
/// without a terminal or a workspace of its own, it registers once the
/// supervisor recorded its start (`wrapper_launched`) with this process's
/// pid.
pub fn session_in_background(
    db: &Path,
    id: &RunId,
    token: &LeaseToken,
    provider: &dyn AgentProvider,
    other: Option<&dyn AgentProvider>,
    spawner: &dyn Spawner,
    resume: bool,
) -> Result<Value> {
    run_session(
        db,
        id,
        token,
        provider,
        other,
        spawner,
        resume,
        None,
        WrapperStart::Background,
        None,
    )
}

/// [`session_with_providers`] whose environment names `sccache` as
/// `RUSTC_WRAPPER` (ADR-t1215-1): a Codex turn runs without it unless its
/// server listens on the loopback port just before.
#[allow(clippy::too_many_arguments)]
pub fn session_with_sccache(
    db: &Path,
    id: &RunId,
    token: &LeaseToken,
    provider: &dyn AgentProvider,
    other: Option<&dyn AgentProvider>,
    spawner: &dyn Spawner,
    resume: bool,
    sccache: crate::domain::sccache::SccacheTarget,
) -> Result<Value> {
    run_session(
        db,
        id,
        token,
        provider,
        other,
        spawner,
        resume,
        None,
        WrapperStart::Workspace,
        Some(sccache),
    )
}

/// The wrapper of a resumed session: `session --resume`.
pub fn resume_session_with_provider(
    db: &Path,
    id: &RunId,
    token: &LeaseToken,
    provider: &dyn AgentProvider,
    spawner: &dyn Spawner,
) -> Result<Value> {
    run_session(
        db,
        id,
        token,
        provider,
        None,
        spawner,
        true,
        None,
        WrapperStart::Workspace,
        None,
    )
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
    run_session(
        db,
        id,
        token,
        provider,
        None,
        spawner,
        resume,
        Some(own),
        WrapperStart::Workspace,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_session(
    db: &Path,
    id: &RunId,
    token: &LeaseToken,
    provider: &dyn AgentProvider,
    other: Option<&dyn AgentProvider>,
    spawner: &dyn Spawner,
    resume: bool,
    own_workspace: Option<OwnWorkspace<'_>>,
    start: WrapperStart,
    sccache: Option<crate::domain::sccache::SccacheTarget>,
) -> Result<Value> {
    // The wrapper's events are its own, not the worker's (ADR-t728-1).
    let mut queue = SqliteQueue::open(db)?.with_actor(
        crate::domain::actor::ActorContext::instance(crate::domain::actor::ActorRole::Wrapper, id),
    );
    // The wrapper only looks at the server; the supervisor starts it.
    let looker = crate::infrastructure::sccache::SystemSccache {
        log: PathBuf::new(),
        start_timeout: Duration::ZERO,
    };
    wrapper::run_session(
        Session {
            queue: &mut queue,
            db,
            provider,
            other,
            spawner,
            queue_service: &crate::infrastructure::queue_service::SystemServiceAccess,
            processes: &SystemProcesses,
            files: &LocalRunFiles,
            pid: std::process::id(),
            own_workspace,
            start,
            sccache: sccache
                .map(|target| (target, &looker as &dyn crate::application::SccacheServer)),
        },
        id,
        token,
        resume,
    )
}

/// `review`: see [`reviewing::review`]. The run's checkout is opened as a
/// Git repository.
pub fn review(db: &Path, task_id: TaskId) -> Result<Value> {
    review_in(db, task_id, None)
}

/// [`review`] over `range` when it is given: the range a review attempt
/// fixed when it selected its required agents.
pub fn review_in(
    db: &Path,
    task_id: TaskId,
    range: Option<&reviewing::ReviewRange>,
) -> Result<Value> {
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
        range,
    )
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
    entry: PlannerEntry,
) -> Result<Value> {
    // A headless planner's agent has no terminal (ADR-t1394-2); one started
    // in the background leads a session of its own (ADR-t1404-1).
    if entry.headless {
        wrapper_entry(entry.background)?;
    } else {
        ensure!(
            std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
            "interactive Claude wrapper requires a terminal"
        );
    }
    let provider = ClaudeCode {
        executable: claude.into(),
    };
    let cmux = Cmux {
        executable: cmux.into(),
    };
    let own = if entry.background {
        None
    } else {
        own_workspace(&cmux)
    };
    planner_session_with_provider(
        db,
        id,
        &provider,
        plugin_dir,
        model,
        own,
        &mut std::io::stderr(),
    )
}

/// How a planner's wrapper was started: for a headless planner
/// (`--headless`, ADR-t1394-2), and then in the background
/// (`--background`, ADR-t1404-1 decision 8).
#[derive(Debug, Clone, Copy, Default)]
pub struct PlannerEntry {
    pub headless: bool,
    pub background: bool,
}

/// [`planner_session`] with any provider, in the working directory, in the
/// workspace `own` (`None` knows none), writing what it tells the
/// workspace's terminal to `terminal`.
pub fn planner_session_with_provider(
    db: &Path,
    id: PlannerId,
    provider: &dyn AgentProvider,
    plugin_dir: Option<&Path>,
    model: Option<(&str, &str)>,
    own: Option<OwnWorkspace<'_>>,
    terminal: &mut dyn std::io::Write,
) -> Result<Value> {
    let mut queue =
        SqliteQueue::open(db)?.with_actor(crate::domain::actor::ActorContext::instance(
            crate::domain::actor::ActorRole::Wrapper,
            format_args!("planner:{id}"),
        ));
    let cwd = std::env::current_dir().context("working directory is unavailable")?;
    planner::run_planner_session(
        PlannerWrapper {
            queue: &mut queue,
            db,
            provider,
            spawner: &LocalSpawner,
            files: &LocalRunFiles,
            processes: &SystemProcesses,
            pid: std::process::id(),
            own_workspace: own,
            terminal,
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
    use crate::domain::marks::{self};
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
    let id = queue.record_queue_event(EventKind::MarkRecorded, payload)?;
    recorded_mark(queue, id)
}

/// `dagq mark --retract <id>`: record that the mark `target` was no change
/// (ADR-0051 decision 12); the mark stays, retracted.
pub fn retract_mark(
    queue: &SqliteQueue,
    target: crate::domain::EventId,
    by: &str,
) -> Result<Value> {
    use crate::domain::marks::{self};
    let payload =
        marks::retraction_payload(&queue.all_events()?, target, by).map_err(anyhow::Error::msg)?;
    let id = queue.record_queue_event(EventKind::MarkRetracted, payload)?;
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

/// The near-term dependency diagram of `graph --format d2|svg`
/// (ADR-0077): its d2 source, or the SVG the host's d2 draws from it, and
/// the tasks it shows.
pub fn graph_diagram(
    queue: &SqliteQueue,
    goal_id: Option<crate::domain::GoalId>,
    format: &str,
) -> Result<(String, Vec<TaskId>)> {
    use crate::application::{TaskStore, dependency_graph};
    let input = queue.graph_input()?;
    let graph = dependency_graph(input.clone(), goal_id);
    let titles = queue
        .list_goals()?
        .into_iter()
        .map(|goal| (goal.id, goal.title))
        .collect();
    let diagram = match goal_id {
        // The goal's prerequisites and critical steps outside it are
        // drawn too (ADR-0077 decision 1).
        Some(goal) => crate::application::diagram::near_term_in_goal(
            &dependency_graph(input, None),
            &graph,
            goal,
            &titles,
        ),
        None => crate::application::diagram::near_term(&graph, &titles),
    };
    let source = diagram.to_d2();
    let text = if format == "svg" {
        crate::infrastructure::d2::render_svg(
            &source,
            std::env::var_os("PATH").as_deref(),
            crate::infrastructure::d2::RENDER_TIMEOUT,
        )?
    } else {
        source
    };
    Ok((text, diagram.task_ids()))
}

/// A read of the queue (`list`, `events`, `stats`, `kpi`, `goal show`,
/// ...) as the command line prints it, and as the queue service answers
/// its read use case ([`QueueRead`], ADR-t1233-5 decision 1): both call
/// this, so a read gives the same JSON either way. `cmux` lists the
/// workspaces for `stats`' `workspace_mismatch` (unjudged without).
///
/// [`QueueRead`]: crate::application::queue_reads::QueueRead
pub fn read_queue(
    queue: &mut SqliteQueue,
    db: &Path,
    one_shot: &OneShot,
    cmux: Option<&Cmux>,
    read: &crate::application::queue_reads::QueueRead,
) -> Result<Value> {
    use crate::application::queue_reads::QueueRead;
    use crate::application::{
        AskQuery, StatusFilter, TaskQuery, TaskStore, claim_candidates, dependency_graph,
    };
    use crate::domain::{
        EventFilter, EventId, FindingId, FindingQuery, FindingTarget, GoalId, NoteQuery,
        ProposalId, TaskStatus, search,
    };
    let goal = |id: Option<i64>| id.map(GoalId::new);
    let role = |role: &Option<String>| -> Result<Option<SessionRole>> {
        Ok(role.as_deref().map(str::parse).transpose()?)
    };
    Ok(match read {
        QueueRead::List(read) => {
            let status = if read.all {
                StatusFilter::Any
            } else if read.status.is_empty() {
                StatusFilter::Open
            } else {
                StatusFilter::Only(
                    read.status
                        .iter()
                        .map(|value| value.trim().parse::<TaskStatus>())
                        .collect::<Result<_, _>>()?,
                )
            };
            serde_json::to_value(queue.list(&TaskQuery {
                status,
                goal_id: goal(read.goal),
                limit: usize::try_from(read.limit)?,
                before: read.before.map(TaskId::new),
                full: read.full,
            })?)?
        }
        QueueRead::Candidates => {
            let graph = dependency_graph(queue.graph_input()?, None);
            serde_json::to_value(claim_candidates(queue.candidates()?, &graph))?
        }
        QueueRead::Graph(read) => {
            if read.format == "json" {
                serde_json::to_value(dependency_graph(queue.graph_input()?, goal(read.goal)))?
            } else {
                let (text, _) = graph_diagram(queue, goal(read.goal), &read.format)?;
                json!({ crate::view::RAW_STDOUT: text })
            }
        }
        QueueRead::Status(read) => one_shot.status_of(db, queue, role(&read.role)?)?,
        QueueRead::Asks(read) => json!({"asks": queue.asks(AskQuery {
            all: read.all,
            open: read.open,
            role: role(&read.role)?,
        })?}),
        QueueRead::Events(read) => crate::application::watch::events_in(
            queue,
            &crate::application::watch::EventsQuery {
                after: EventId::new(read.after),
                limit: read.limit as usize,
                all: read.all,
                full: read.full,
                filter: EventFilter {
                    kinds: (!read.kind.is_empty()).then(|| read.kind.clone()),
                    run: read.run.clone().map(RunId::new).transpose()?,
                    task: read.task.map(TaskId::new),
                    goal: goal(read.goal),
                    since: read
                        .since
                        .as_deref()
                        .map(crate::application::watch::event_time)
                        .transpose()?,
                    until: read
                        .until
                        .as_deref()
                        .map(crate::application::watch::event_time)
                        .transpose()?,
                },
            },
        )?,
        QueueRead::Timeline(read) => {
            timeline_in(queue, &RunId::new(read.run.clone())?, read.gap, read.full)?
        }
        QueueRead::Stats(read) => one_shot.stats_of(
            queue,
            db,
            &StatsQuery {
                since: read.since,
                until: read.until,
                goal_id: goal(read.goal),
                full: read.full,
            },
            cmux.map(|cmux| cmux as &dyn WorkspaceListing),
        )?,
        QueueRead::Kpi(read) => one_shot.kpi_of(
            queue,
            db,
            &crate::domain::kpi::KpiQuery {
                period: read.period.parse().map_err(anyhow::Error::msg)?,
                last: usize::from(read.last),
                at: read.at,
                since: read.since,
                until: read.until,
                changes: read.changes.clone(),
                areas: read.areas.clone(),
                by: read
                    .by
                    .iter()
                    .map(|axis| axis.parse())
                    .collect::<Result<_, String>>()
                    .map_err(anyhow::Error::msg)?,
                cross: read.cross,
                compare: read.compare,
                window_days: read.window,
                goal_id: goal(read.goal),
            },
        )?,
        QueueRead::Forecast(read) => one_shot.forecast_of(
            queue,
            db,
            &crate::application::forecast::ForecastQuery {
                task_id: read.task.map(TaskId::new),
                goal_id: goal(read.goal),
                parallel: read.parallel.map(usize::from),
                trials: read.trials as usize,
            },
        )?,
        QueueRead::Notes(read) => serde_json::to_value(queue.notes(&NoteQuery {
            goal_id: goal(read.goal),
            task_id: read.task.map(TaskId::new),
            since: read.since.map(EventId::new),
            limit: usize::try_from(read.limit)?,
        })?)?,
        QueueRead::Marks(read) => marks(queue, read.since, read.until)?,
        QueueRead::Findings(read) => {
            let target = match (read.task, &read.run, read.goal) {
                (Some(task), _, _) => Some(FindingTarget::Task(TaskId::new(task))),
                (_, Some(run), _) => Some(FindingTarget::Run(RunId::new(run.clone())?)),
                (_, _, Some(id)) => Some(FindingTarget::Goal(GoalId::new(id))),
                _ => read.queue.then_some(FindingTarget::Queue),
            };
            let findings = queue.findings(&FindingQuery {
                id: read.id.map(FindingId::new),
                all: read.all,
                statuses: read
                    .status
                    .iter()
                    .map(|value| value.parse())
                    .collect::<Result<_, _>>()?,
                kinds: read.kinds.clone(),
                target,
                full: read.full,
            })?;
            // The limit's settings not reading does not hide the findings.
            let improvements = one_shot
                .improvements_of(queue)
                .unwrap_or_else(|error| json!({"error": format!("{error:#}")}));
            json!({"findings": findings, "improvements": improvements})
        }
        QueueRead::Search(read) => serde_json::to_value(
            queue.search(&search::SearchQuery {
                terms: read.query.clone(),
                kinds: read
                    .kinds
                    .iter()
                    .map(|kind| kind.parse())
                    .collect::<Result<_, _>>()?,
                statuses: read
                    .status
                    .iter()
                    .map(|value| search::parse_status(value))
                    .collect::<Result<_, _>>()?,
                goal_id: goal(read.goal),
                limit: usize::try_from(read.limit)?,
                full: read.full,
            })?,
        )?,
        QueueRead::Related(read) => {
            let statuses = read
                .status
                .iter()
                .map(|status| Ok(status.trim().parse::<TaskStatus>()?.as_str().to_owned()))
                .collect::<Result<Vec<_>>>()?;
            serde_json::to_value(queue.related(
                read.task,
                &statuses,
                usize::try_from(read.limit)?,
            )?)?
        }
        QueueRead::GoalList => serde_json::to_value(queue.list_goals()?)?,
        QueueRead::GoalShow(read) => {
            let detail = queue.show_goal(GoalId::new(read.id))?;
            if read.full {
                serde_json::to_value(detail)?
            } else {
                crate::view::goal_detail(&detail)
            }
        }
        QueueRead::Lint(read) => {
            let mut targets: Vec<TaskId> = read.tasks.iter().copied().map(TaskId::new).collect();
            for id in &read.proposals {
                targets.extend_from_slice(queue.show_proposal(ProposalId::new(*id))?.task_ids());
            }
            let mut seen = std::collections::HashSet::new();
            targets.retain(|id| seen.insert(*id));
            // The repository's set of changes holds the tasks lint checks
            // (ADR-t980-1).
            let mut input = queue.lint_input(&targets)?;
            input.changes = task_changes(queue)?;
            json!({"tasks": targets, "violations": crate::domain::lint::lint(&input)})
        }
        QueueRead::ObserveHistory(read) => {
            crate::application::observer::history(queue, read.limit)?
        }
        QueueRead::ObserveInput(read) => crate::infrastructure::observer::read_input(
            db,
            &read.observation,
            read.section.as_deref(),
            read.offset,
            read.limit,
        )?,
    })
}

/// What `dagq broker start` is given.
#[derive(Debug, Clone, Default)]
pub struct BrokerStartOptions {
    /// The port on `127.0.0.1`; else the one the queue used last, else a
    /// free one.
    pub port: Option<u16>,
    /// The podman executable; else `podman` on `PATH`.
    pub podman: Option<PathBuf>,
    /// The working directory, for the repository's `dagq.toml`.
    pub cwd: PathBuf,
}

/// What the queue's broker is set to: the mode in force, the
/// repository's `[broker]` and the host's.
#[derive(Debug, Clone)]
pub struct BrokerSetup {
    pub mode: crate::domain::broker::BrokerMode,
    pub repository: crate::domain::broker::BrokerConfig,
    pub host: crate::infrastructure::broker_config::LoadedHostBroker,
}

/// `[broker]` of the `dagq.toml` in `checkout` (none without a checkout)
/// and of the queue's `host.toml` over the host-wide one (ADR-t827-4
/// decision 4). A `dagq.toml` that cannot be read is an error.
pub fn load_broker_setup(
    checkout: Option<&Path>,
    queue_dir: &Path,
    host_wide: Option<&Path>,
) -> Result<BrokerSetup> {
    let repository = match checkout {
        Some(checkout) => crate::infrastructure::run_env::load_broker_config(checkout)?,
        None => crate::domain::broker::BrokerConfig::default(),
    };
    let host = crate::infrastructure::broker_config::load_host_broker(queue_dir, host_wide);
    Ok(BrokerSetup {
        mode: crate::domain::broker::resolve_mode(repository.mode, &host.config),
        repository,
        host,
    })
}

/// The queue's broker with `setup`'s resources and limits.
fn queue_broker(
    queue_dir: PathBuf,
    runs_dir: PathBuf,
    queue_hash: String,
    git_common_dir: Option<PathBuf>,
    setup: &BrokerSetup,
    ports: crate::infrastructure::broker_queue::BrokerPorts,
) -> crate::infrastructure::broker_queue::QueueBroker {
    use crate::application::broker::{ContainerLimits, MachineSpec};
    let host = &setup.host.config;
    crate::infrastructure::broker_queue::QueueBroker {
        machine: MachineSpec::with_host(host),
        limits: ContainerLimits::with_host(host),
        serve_limits: setup.repository.serve_args(),
        port: host.fixed_port(),
        ..crate::infrastructure::broker_queue::QueueBroker::new(
            queue_dir,
            runs_dir,
            queue_hash,
            git_common_dir,
            ports,
        )
    }
}

/// The queue's broker for `dagq broker` and `down`: `setup`, the podman
/// given (else `host.toml`'s, else on `PATH`), and the image from the
/// material this binary embeds.
fn location_broker(
    location: &QueueLocation,
    setup: &BrokerSetup,
    podman: Option<&Path>,
) -> Result<crate::infrastructure::broker_queue::QueueBroker> {
    use crate::infrastructure::broker_queue::system_ports;
    let podman = podman
        .map(Path::to_path_buf)
        .or_else(|| setup.host.config.podman.as_ref().map(PathBuf::from));
    let ports = system_ports(
        podman.as_deref(),
        &crate::infrastructure::broker_podman::machine_lock_home()?,
    );
    Ok(queue_broker(
        location.queue_dir.clone(),
        location.runs_dir.clone(),
        location.hash(),
        location.git_common_dir.clone(),
        setup,
        ports,
    ))
}

/// The setup with only the host's `[broker]`, for a `dagq.toml` that
/// cannot be read.
fn host_setup(location: &QueueLocation) -> BrokerSetup {
    BrokerSetup {
        mode: crate::domain::broker::BrokerMode::Disabled,
        repository: crate::domain::broker::BrokerConfig::default(),
        host: crate::infrastructure::broker_config::load_host_broker(
            &location.queue_dir,
            crate::infrastructure::kpi_config::host_wide_file().as_deref(),
        ),
    }
}

/// The podman given, else `host.toml`'s, else on `PATH`; `podman_missing`
/// when there is none.
fn podman_of(
    podman: Option<&Path>,
    setup: &BrokerSetup,
) -> std::result::Result<PathBuf, crate::application::broker::BrokerFailure> {
    let configured = podman
        .map(Path::to_path_buf)
        .or_else(|| setup.host.config.podman.as_ref().map(PathBuf::from));
    crate::infrastructure::broker_podman::PodmanCli::resolve(configured.as_deref())
        .map(|podman| podman.executable)
}

/// The broker's setup for the queue of `location`, its `dagq.toml` read
/// from the main checkout of `cwd` (none outside a repository).
fn location_setup(location: &QueueLocation, cwd: &Path) -> Result<BrokerSetup> {
    let checkout = main_checkout_of(cwd).ok();
    load_broker_setup(
        checkout.as_deref(),
        &location.queue_dir,
        crate::infrastructure::kpi_config::host_wide_file().as_deref(),
    )
}

/// `dagq broker start`: make the queue's broker run in dagq's Podman
/// machine and answer health on `127.0.0.1` ([`crate::application::broker::start`]).
/// The queue's lock keeps two starts of one queue apart; the machine has
/// its own host-wide lock. A failure is recorded in `state.json` and
/// returned as the [`crate::application::broker::BrokerFailure`]. It
/// starts the broker whatever the mode: a person or a test asks for it.
pub fn broker_start(location: &QueueLocation, options: &BrokerStartOptions) -> Result<Value> {
    let setup = location_setup(location, &options.cwd)?;
    // No podman is its own failure before anything else.
    podman_of(options.podman.as_deref(), &setup)?;
    let mut broker = location_broker(location, &setup, options.podman.as_deref())?;
    broker.port = options.port.or(broker.port);
    let report = broker.start()?;
    let port = report.port;
    Ok(json!({
        "state": "running",
        "machine": broker.machine,
        "start": report,
        "url": format!("http://127.0.0.1:{port}"),
    }))
}

/// `dagq broker stop`: stop the queue's container, then dagq's machine when
/// no container runs on it.
pub fn broker_stop(location: &QueueLocation, podman: Option<&Path>) -> Result<Value> {
    // Only host.toml's podman and machine matter to a stop: a dagq.toml
    // that cannot be read does not keep it from stopping.
    let setup = location_setup(location, &std::env::current_dir()?)
        .unwrap_or_else(|_| host_setup(location));
    podman_of(podman, &setup)?;
    let broker = location_broker(location, &setup, podman)?;
    let report = broker.stop()?;
    Ok(json!({"state": "stopped", "stop": report}))
}

/// `dagq broker status`: the machine, the image, the container and the
/// health, read without changing anything. No podman is a state
/// (`podman_missing`), not an error.
pub fn broker_status(location: &QueueLocation, podman: Option<&Path>) -> Result<Value> {
    use crate::infrastructure::broker_podman::{BrokerState, PodmanCli};
    let state = BrokerState::read(&location.queue_dir);
    let setup = location_setup(location, &std::env::current_dir()?);
    let mode = match &setup {
        Ok(setup) => json!(setup.mode),
        Err(error) => json!({"error": format!("{error:#}")}),
    };
    let setup = setup.unwrap_or_else(|_| host_setup(location));
    let podman = podman
        .map(Path::to_path_buf)
        .or_else(|| setup.host.config.podman.as_ref().map(PathBuf::from));
    let unavailable = |failure: crate::application::broker::BrokerFailure| {
        json!({
            "mode": mode,
            "state": failure.code.as_str(),
            "error": failure.to_json(),
            "recorded": state,
        })
    };
    let executable = match PodmanCli::resolve(podman.as_deref()) {
        Ok(podman) => podman.executable,
        Err(failure) => return Ok(unavailable(failure)),
    };
    let broker = location_broker(location, &setup, podman.as_deref())?;
    let report = match broker.status() {
        Ok(report) => report,
        Err(failure) => return Ok(unavailable(failure)),
    };
    let mut value = serde_json::to_value(report)?;
    value["mode"] = mode;
    value["client"] = broker_client_report();
    value["podman"] = json!(executable);
    value["recorded"] = serde_json::to_value(state)?;
    Ok(value)
}

/// The queue service as `status`, `doctor` and `dagq service status` show
/// it (ADR-t1233-4): what a look at its record and its socket found
/// ([`crate::infrastructure::queue_service::probe`]), the API version this
/// binary speaks, and the attention its latest event leaves standing
/// (`queue_service_down`) when `queue` can be read.
fn queue_service_view(db: &Path, queue: Option<&SqliteQueue>) -> Value {
    use crate::domain::queue_service::{API_VERSION, QUEUE_SERVICE_ATTENTION_KINDS};
    let queue_dir = db.parent().unwrap_or(Path::new("."));
    let mut view = serde_json::to_value(crate::infrastructure::queue_service::probe(queue_dir))
        .unwrap_or(Value::Null);
    view["client_api_version"] = json!(API_VERSION);
    view["attention"] =
        match queue.map(|queue| queue.latest_queue_event(&QUEUE_SERVICE_ATTENTION_KINDS)) {
            None => Value::Null,
            Some(Ok(latest)) => json!(latest.is_some_and(|event| {
                crate::domain::queue_service::attention_stands(Some(event.kind.as_str()))
            })),
            Some(Err(error)) => json!({"error": format!("{error:#}")}),
        };
    view
}

/// `dagq service status`: [`queue_service_view`], read without changing
/// anything.
pub fn queue_service_status(db: &Path) -> Value {
    let queue = SqliteQueue::open_read_only(db).ok();
    queue_service_view(db, queue.as_ref())
}

/// `dagq service start`: [`crate::application::queue_service::ensure`] with
/// `executable`, recorded as `queue_service_started` (`by: service start`)
/// unless one of this build already answered.
pub fn queue_service_start(db: &Path, executable: &Path, cmux: &Path) -> Result<Value> {
    let queue = SqliteQueue::open(db)?;
    let control =
        crate::infrastructure::queue_service::SystemQueueService::new(db, executable, cmux);
    let report = crate::application::queue_service::ensure(
        &control,
        crate::application::lifecycle::QUEUE_SERVICE_START_TIMEOUT,
    )?;
    if report["outcome"] != "reused" {
        crate::application::RunLog::record_queue_event(
            &queue,
            EventKind::QueueServiceStarted,
            json!({
                "by": "service start",
                "pid": report["service"]["pid"],
                "build": report["service"]["build"],
                "api_version": report["service"]["api_version"],
                "socket": report["service"]["socket"],
                "restart": false,
                "replaced": report["replaced"],
            }),
        )?;
    }
    Ok(report)
}

/// `dagq service stop`: stop the queue's service, recorded as
/// `queue_service_stopped` (`by: service stop`) when one ran. A supervisor
/// at work starts it again at its next look.
pub fn queue_service_stop(db: &Path) -> Result<Value> {
    use crate::application::queue_service::QueueServiceControl;
    let queue = SqliteQueue::open(db)?;
    let control = crate::infrastructure::queue_service::SystemQueueService::new(
        db,
        Path::new("dagq"),
        Path::new("cmux"),
    );
    Ok(
        match control.stop(crate::application::lifecycle::QUEUE_SERVICE_START_TIMEOUT)? {
            Some(pid) => {
                crate::application::RunLog::record_queue_event(
                    &queue,
                    EventKind::QueueServiceStopped,
                    json!({"pid": pid, "by": "service stop"}),
                )?;
                json!({"outcome": "stopped", "pid": pid})
            }
            None => json!({"outcome": "not_running"}),
        },
    )
}

/// `dagq broker logs`: the last `tail` lines of the queue's container's
/// `podman logs` ([`crate::application::broker_admin::logs`]). It only
/// reads: no podman, a missing or stopped machine and a missing container
/// are returned as the [`crate::application::broker::BrokerFailure`].
pub fn broker_logs(location: &QueueLocation, podman: Option<&Path>, tail: u32) -> Result<Value> {
    use crate::application::broker::{MACHINE, container_name};
    use crate::infrastructure::broker_podman::PodmanCli;
    let podman = PodmanCli::resolve(podman)?;
    let report = crate::application::broker_admin::logs(
        &podman,
        MACHINE,
        &container_name(&location.hash()),
        tail,
    )?;
    Ok(serde_json::to_value(report)?)
}

/// `dagq broker audit`: the lines of `<queue dir>/broker/audit` that
/// `query` keeps ([`crate::application::broker_admin::audit`]), read from
/// the files without taking them into the queue DB.
pub fn broker_audit(
    location: &QueueLocation,
    query: &crate::application::broker_admin::AuditQuery,
) -> Result<Value> {
    let dir = crate::application::broker::broker_dir(&location.queue_dir).join("audit");
    let report = crate::application::broker_admin::audit(&dir, query)
        .with_context(|| format!("read the broker's audit in {}", dir.display()))?;
    Ok(serde_json::to_value(report)?)
}

/// The broker's `mode` (`[broker]` of the bound checkout's `dagq.toml`
/// lowered by `host.toml`, or the `error` reading them), its `health`
/// from the supervisor's latest broker event, and the `active_tokens`
/// the runs hold, for `status` and `doctor`. It runs no podman command.
fn broker_queue_view(db: &Path, queue: &SqliteQueue) -> Value {
    let queue_dir = db.parent().unwrap_or(Path::new("."));
    let mode = bound_checkout(queue).and_then(|checkout| {
        load_broker_setup(
            checkout.as_deref(),
            queue_dir,
            crate::infrastructure::kpi_config::host_wide_file().as_deref(),
        )
    });
    let mode = match mode {
        Ok(setup) => json!(setup.mode),
        Err(error) => json!({"error": format!("{error:#}")}),
    };
    let health = match queue.latest_queue_event(&crate::domain::broker::BROKER_ATTENTION_KINDS) {
        Ok(latest) => crate::domain::broker::health_report(latest.as_ref().map(|event| {
            (
                event.kind.as_str(),
                &event.payload,
                event.created_at.as_str(),
            )
        })),
        Err(error) => json!({"error": format!("{error:#}")}),
    };
    let tokens = crate::infrastructure::broker_token::QueueRunTokens {
        queue_dir: queue_dir.to_path_buf(),
    };
    let active_tokens = match crate::application::broker_run::RunTokens::held(&tokens) {
        Ok(held) => json!(held.len()),
        Err(error) => json!({"error": format!("{error:#}")}),
    };
    json!({"mode": mode, "health": health, "active_tokens": active_tokens})
}

/// The `broker` of `doctor`: the mode and health ([`broker_queue_view`],
/// `null` for a queue that cannot be read), the podman executable (or why
/// there is none) and what `dagq broker` last recorded. It runs no podman
/// command.
fn doctor_broker(db: &Path, view: Option<&Value>) -> Value {
    use crate::infrastructure::broker_podman::{BrokerState, PodmanCli};
    let queue_dir = db.parent().unwrap_or(Path::new("."));
    let podman = PodmanCli::resolve(None);
    json!({
        "mode": view.map_or(Value::Null, |view| view["mode"].clone()),
        "health": view.map_or(Value::Null, |view| view["health"].clone()),
        "active_tokens": view.map_or(Value::Null, |view| view["active_tokens"].clone()),
        "podman": podman.as_ref().ok().map(|podman| &podman.executable),
        "error": podman.as_ref().err().map(|failure| failure.to_json()),
        "machine": crate::application::broker::MACHINE,
        "build": crate::VERSION,
        "image": crate::infrastructure::broker_image::image(),
        "client": broker_client_report(),
        "recorded": BrokerState::read(queue_dir),
    })
}

/// The `broker` of `status`: the build the client and the broker's image
/// must name, the image of that build and the one `dagq broker` last ran,
/// and the client next to this dagq. It runs no podman command.
fn status_broker(db: &Path, queue: &SqliteQueue) -> Value {
    use crate::infrastructure::broker_podman::BrokerState;
    let recorded = BrokerState::read(db.parent().unwrap_or(Path::new(".")));
    let image = crate::infrastructure::broker_image::image();
    let view = broker_queue_view(db, queue);
    json!({
        "mode": view["mode"],
        "health": view["health"],
        "active_tokens": view["active_tokens"],
        "state": recorded.state,
        "port": recorded.port,
        "build": crate::VERSION,
        "image": image,
        "running_image": recorded.image,
        "image_matches": recorded.image.as_ref().map(|running| *running == image),
        "client": broker_client_report(),
    })
}

/// The worker's client next to this dagq, its build and whether dagq uses
/// it (ADR-t827-1 decisions 5 and 7), for `doctor` and `broker status`.
fn broker_client_report() -> Value {
    let dagq = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("dagq"));
    serde_json::to_value(crate::application::broker::client_report(
        &dagq,
        crate::VERSION,
        &crate::infrastructure::broker_podman::client_version,
    ))
    .unwrap_or(Value::Null)
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

/// Open the queue at `db` for the CLI's commands.
pub fn open_queue(db: &Path) -> Result<SqliteQueue> {
    SqliteQueue::open(db)
}

/// Open the queue at `db` read-only for the CLI's reads.
pub fn open_queue_read_only(db: &Path) -> Result<SqliteQueue> {
    SqliteQueue::open_read_only(db)
}

/// `observe`: one observation of the queue at `db`
/// ([`crate::application::observer::observe`]) with the repository's reads,
/// the configured cmux and the local host. `signals` must belong to the
/// provider that starts it.
pub fn observe(
    db: &Path,
    provider: &dyn AgentProvider,
    signals: &dyn crate::application::AgentSignals,
    options: &crate::application::observer::ObserveOptions,
) -> Result<Value> {
    let db = db
        .canonicalize()
        .context("queue must already be initialized")?;
    let mut queue = SqliteQueue::open(&db)?;
    let generators = queue.generators().clone();
    // The configured cmux lists the workspaces for `workspace_mismatch`;
    // without one, only that alert is left unjudged.
    let cmux = options
        .cmux
        .as_deref()
        .and_then(|path| executable(path).ok())
        .map(|executable| Cmux { executable });
    let sources = ObserverReads {
        one_shot: OneShot::new(generators.clone()),
        cmux: cmux.as_ref(),
    };
    crate::application::observer::observe(
        &mut queue,
        &db,
        provider,
        signals,
        options,
        &crate::application::observer::ObserverEnvironment {
            sources: &sources,
            host: &crate::infrastructure::observer::LocalObserver,
            generators: &generators,
        },
    )
}

/// The observer's reads that [`OneShot`] gives on the opened queue.
struct ObserverReads<'a> {
    one_shot: OneShot,
    cmux: Option<&'a Cmux>,
}

impl crate::application::observer::ObserverSources<SqliteQueue> for ObserverReads<'_> {
    fn stats(&self, queue: &SqliteQueue, db: &Path, query: &StatsQuery) -> Result<Value> {
        self.one_shot.stats_of(
            queue,
            db,
            query,
            self.cmux.map(|cmux| cmux as &dyn WorkspaceListing),
        )
    }
    fn kpi(&self, queue: &SqliteQueue, db: &Path) -> Result<Value> {
        self.one_shot.observer_kpi(queue, db)
    }
    fn improvements(&self, queue: &SqliteQueue) -> Result<Value> {
        self.one_shot.improvements_of(queue)
    }
    fn checkout(&self, queue: &SqliteQueue) -> Result<Option<PathBuf>> {
        bound_checkout(queue)
    }
    fn cmux(&self) -> Option<&dyn WorkspaceBackend> {
        self.cmux.map(|cmux| cmux as &dyn WorkspaceBackend)
    }
}

/// `events --after`: the events after `after`, oldest first, in compact
/// form with no filter.
pub fn events(db: &Path, after: EventId, limit: usize, all: bool) -> Result<Value> {
    events_matching(
        db,
        &crate::application::watch::EventsQuery {
            after,
            limit,
            all,
            full: false,
            filter: EventFilter::default(),
        },
    )
}

/// `events` with its filters and `--full` on the queue at `db`.
pub fn events_matching(db: &Path, query: &crate::application::watch::EventsQuery) -> Result<Value> {
    crate::application::watch::events_in(&SqliteQueue::open_read_only(db)?, query)
}

/// `timeline RUN` on the queue at `db`.
pub fn timeline(db: &Path, run: &RunId, gap_secs: i64, full: bool) -> Result<Value> {
    timeline_in(&SqliteQueue::open_read_only(db)?, run, gap_secs, full)
}

/// [`timeline`] on a queue the caller already opened, so a command opens
/// it once.
pub fn timeline_in(queue: &SqliteQueue, run: &RunId, gap_secs: i64, full: bool) -> Result<Value> {
    crate::application::watch::timeline_in(
        queue,
        run,
        gap_secs,
        full,
        queue.generators().clock.as_ref(),
    )
}

/// `watch` on the queue at `db` ([`crate::application::watch::watch`]); a
/// `watch --role inbox` keeps its record in a file under the queue's
/// directory, never in the queue (ADR-t906-1).
pub fn watch(db: &Path, options: &crate::application::watch::WatchOptions) -> Result<Value> {
    use crate::infrastructure::inbox_watchers;
    let queue = SqliteQueue::open(db)?;
    let clock = queue.generators().clock.clone();
    let mut record = (options.role == Some(SessionRole::Inbox))
        .then(|| {
            let now = clock.system_time();
            inbox_watchers::WatcherFile::start(
                &inbox_watchers::dir(db),
                std::process::id(),
                clock.now(),
                now.duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as i64),
                options
                    .timeout
                    .map(|timeout| i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX / 4)),
                i64::try_from(options.interval.as_secs()).unwrap_or(i64::MAX / 4),
            )
            .inspect_err(|error| {
                tracing::warn!(error = %format_args!("{error:#}"), "the inbox watcher's record could not be written: {error:#}");
            })
            .ok()
        })
        .flatten();
    crate::application::watch::watch(
        &queue,
        clock.as_ref(),
        &SystemProcesses,
        record
            .as_mut()
            .map(|record| record as &mut dyn crate::application::watch::WatchRecord),
        options,
    )
}
