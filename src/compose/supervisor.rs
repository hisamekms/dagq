//! The supervisor loop's wiring ([`supervise`]): it reads the settings
//! of the main checkout's `dagq.toml`, builds the adapters and takes the
//! port of each context from that context's module, and runs
//! [`supervisor::supervise`]. Unlike the context modules it names them
//! all, since the loop drives every context.

use super::{
    execution::{self, RunE2eOptions, review_in},
    host::{self, HostMetricsSettings, QueueServiceOptions, ReleaseIndexPort, SccacheOptions},
    observation::{self, CiWatchOptions},
    planning::PLANNER_TIMEOUT,
    provider_checks, worker_adapters,
};
use crate::domain::LeaseToken;
use crate::domain::worker::ProviderCheck;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64},
    },
    time::Duration,
};

use crate::{
    application::{
        AgentProvider, Generators, ProcessControl, RunFiles, SessionWrappers,
        review::{self as reviewing},
        supervise::{self as supervisor, Layout, LoopSettings, Ports, UpdateSettings},
    },
    domain::{
        Provider, SupervisorMode, TaskId,
        slot_limits::{SlotFlags, SlotLimits},
        stall::StallConfig,
        stats::ConflictConfigReport,
    },
    infrastructure::{
        adapters::{
            ClaudeCode, GitRepository, SystemProcesses, VERIFICATION_TIMEOUT, free_disk_bytes,
            host_versions, load_average,
        },
        clock,
        codex::Codex,
        location::{QueueLocation, goal_reviews_dir, plan_reviews_dir, planners_dir, runs_dir},
        process::LocalSpawner,
        run_env::{
            ShellVerifier, load_conflict_config, load_disk_config, load_fresh_session,
            load_provider_fallback, load_resume_config, load_stall_config, load_supervisor_config,
        },
        run_files::LocalRunFiles,
        runtime_store::SqliteOpener,
        transcripts::ClaudeTranscripts,
    },
};

const IDLE_POLL: Duration = Duration::from_secs(2);

const TICK: Duration = Duration::from_secs(1);

/// How often the supervisor sweeps the workspaces of ended runs.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// How the supervisor loop is driven. `stop` is the graceful drain switch
/// (SIGINT in the CLI): no more claims, exit once every active run rests.
#[derive(Debug, Clone)]
pub struct SuperviseOptions {
    /// Explicit operator policy: never start Claude; unsupported roles wait for manual handling.
    pub no_claude: bool,
    /// Retry once a review whose verdict cannot be read or whose job exited
    /// non-zero (the production default). Tests
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
    /// Least time from the end of a cleanup for room to the next one
    /// ([`supervisor::CLEANUP_INTERVAL`]); tests shorten it (task
    /// 1627).
    pub disk_cleanup_interval: Duration,
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
    /// Reads the free bytes of the file system of a path; tests set it.
    pub free_space: fn(&Path) -> Option<u64>,
    /// The Claude Code configuration directory whose
    /// `plugins/installed_plugins.json` names the plugin a Claude worker
    /// loads, hashed once per claim pass (goal 113). The CLI gives the
    /// host's ([`crate::infrastructure::adapters::claude_config_dir`]); a
    /// caller of the library (the tests) reads none, and records the
    /// plugin `unknown`, unless it gives its own.
    pub claude_config_dir: Option<PathBuf>,
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
    /// The build identifier the supervisor names itself by (registration,
    /// log, the claim's wait for a build, ADR-t1632-1); `None` is this
    /// binary's. Tests set it (a build that lacks or contains a landing).
    pub build: Option<String>,
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
    /// Watch the landing branch's CI when `[ci_watch]` is set
    /// (ADR-t1920-1): the CLI's supervisor does but for `--once`; `None`
    /// (the tests unless they ask) watches nothing.
    pub ci_watch: Option<CiWatchOptions>,
}

/// The processes [`SuperviseOptions::processes`](super::SuperviseOptions::processes) gives the supervisor.
#[derive(Clone)]
pub struct ProcessesPort(pub Arc<dyn ProcessControl + Send + Sync>);

impl std::fmt::Debug for ProcessesPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProcessesPort")
    }
}

/// The run files [`SuperviseOptions::files`](super::SuperviseOptions::files) gives the supervisor.
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
            disk_cleanup_interval: supervisor::CLEANUP_INTERVAL,
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
            free_space: free_disk_bytes,
            claude_config_dir: None,
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
            build: None,
            codex: PathBuf::from("codex"),
            codex_home: None,
            host_metrics: None,
            queue_service: None,
            passes: Arc::new(AtomicU64::new(0)),
            sccache: None,
            ci_watch: None,
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
        limits: SlotLimits,
    ) -> LoopSettings {
        LoopSettings {
            no_claude: self.no_claude,
            retry_unreadable_review: self.retry_unreadable_review,
            limits,
            light_changes: Default::default(),
            provider_fallback: Default::default(),
            fresh_session: Default::default(),
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
            disk_cleanup_interval: self.disk_cleanup_interval,
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
            passes: self.passes.clone(),
        }
    }
}

/// Run and monitor tasks until the loop ends (see
/// [`supervisor::supervise`]). Accepted runs are reviewed headless by
/// `claude` itself (ADR-0027).
pub fn supervise(
    db: &Path,
    repo: &Path,
    sessions: &dyn SessionWrappers,
    claude: &Path,
    runner: &Path,
    options: &SuperviseOptions,
) -> Result<Value> {
    let reviewer = ClaudeCode {
        executable: claude.into(),
    };
    supervise_with_reviewer(db, repo, sessions, claude, &reviewer, runner, options)
}

/// [`supervise`] with the provider of the headless review given apart
/// from the `claude` the run sessions start (a test double in tests).
pub fn supervise_with_reviewer(
    db: &Path,
    repo: &Path,
    sessions: &dyn SessionWrappers,
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
    // `parallel`, `max_waiting`, `runtime_planners` and `claim_spacing`
    // the flags did not give (task 698, task 941, ADR-t1479-1): a
    // `[supervisor]` that cannot be read at the start leaves the defaults,
    // as `[disk]` does; later reads keep the values in use.
    let slot_flags = options.slot_flags();
    // Read even with every flag given: `light_changes` has no flag
    // (ADR-t1591-1).
    let supervisor_config = load_supervisor_config(&main_checkout)
        .unwrap_or_else(|error| {
            tracing::warn!(error = %format_args!("{error:#}"), "[supervisor] of dagq.toml not read: {error:#}; using the defaults");
            None
        })
        .unwrap_or_default();
    let limits = SlotLimits::resolve(slot_flags, &supervisor_config);
    let light_changes = supervisor_config.light_changes();
    // Read each pass even with every flag given: `light_changes` has no
    // flag (ADR-t1591-1).
    let supervisor_file = Some({
        let checkout = main_checkout.clone();
        Arc::new(move || load_supervisor_config(&checkout))
            as crate::application::supervise::SupervisorFile
    });
    // `[provider_fallback]` (ADR-t1857-1): a table that cannot be read at
    // the start leaves the default (on); later reads keep the value in
    // use. Read again each pass.
    let provider_fallback = load_provider_fallback(&main_checkout)
        .unwrap_or_else(|error| {
            tracing::warn!(error = %format_args!("{error:#}"), "[provider_fallback] of dagq.toml not read: {error:#}; using the default");
            None
        })
        .unwrap_or_default();
    let provider_fallback_file = Some({
        let checkout = main_checkout.clone();
        Arc::new(move || load_provider_fallback(&checkout))
            as crate::application::supervise::ProviderFallbackFile
    });
    // `[fresh_session]` (ADR-t2080-1), read as `[provider_fallback]` is:
    // a table that cannot be read at the start starts no new session;
    // later reads keep the value in use.
    let fresh_session = load_fresh_session(&main_checkout)
        .unwrap_or_else(|error| {
            tracing::warn!(error = %format_args!("{error:#}"), "[fresh_session] of dagq.toml not read: {error:#}; starting no new session");
            None
        })
        .unwrap_or_default();
    let fresh_session_file = Some({
        let checkout = main_checkout.clone();
        Arc::new(move || load_fresh_session(&checkout))
            as crate::application::supervise::FreshSessionFile
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
    let found_codex = crate::infrastructure::codex::codex_at_entry(&options.codex).ok();
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
        ProviderCheck::disable(&mut providers, Provider::Claude);
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
        version: options
            .build
            .clone()
            .unwrap_or_else(|| crate::VERSION.to_owned()),
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
    let host_wide = options
        .host_config
        .clone()
        .or_else(crate::infrastructure::kpi_config::host_wide_file);
    let reports = options.report_daily.then(|| {
        observation::report_port(
            &db,
            &main_checkout,
            host_wide.clone(),
            options.diagram_path.clone(),
            options.push_retry,
        )
    });
    // Read again each pass (ADR-0080), unless the options set the
    // thresholds.
    let conflicts_file = options.conflicts.is_none().then(|| {
        let checkout = main_checkout.clone();
        let load = options.load_conflicts;
        Arc::new(move || load(&checkout)) as crate::application::supervise::ConflictsFile
    });
    let limit_checkout = main_checkout.clone();
    let max_improvement_proposals =
        Arc::new(move || observation::max_improvement_proposals(&limit_checkout));
    let forecasts = options.forecast_snapshots.then(|| {
        observation::forecast_port(
            &db,
            &main_checkout,
            host_wide.clone(),
            options.forecast_check,
        )
    });
    let release = host::release_port(
        &db,
        &main_checkout,
        claude,
        host_wide.clone(),
        // The plugin a `--plugin-dir` supervisor loads is not touched
        // (ADR-t618-2 decision 3).
        options.plugin_dir.is_none(),
        options.release_index.as_ref(),
        options.release_current.clone(),
    );
    let host_metrics = options
        .host_metrics
        .clone()
        .map(|settings| host::host_metrics_port(&db, settings));
    let run_e2e = execution::run_e2e_port(&db, &main_checkout, &options.run_e2e, &layout.cmux);
    let ci_watch = options
        .ci_watch
        .as_ref()
        .map(|settings| observation::ci_watch_port(&db, &main_checkout, settings));
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
        sessions,
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
        worker_plugin: {
            let config_dir = options.claude_config_dir.clone();
            Arc::new(move |project: &Path| {
                crate::infrastructure::adapters::worker_plugin_hash(
                    config_dir.as_deref(),
                    crate::application::lifecycle::DAGQ_PLUGIN,
                    project,
                )
            })
        },
        reports,
        max_improvement_proposals,
        conflicts_file,
        supervisor_file,
        provider_fallback_file,
        fresh_session_file,
        forecasts,
        release: Some(release),
        host_metrics,
        queue_service: options
            .queue_service
            .as_ref()
            .map(|settings| host::queue_service_port(&db, settings)),
        run_e2e,
        sccache: options
            .sccache
            .as_ref()
            .map(|settings| host::sccache_port(&db, settings, runner)),
        ci_watch,
        layout,
    };
    let settings = LoopSettings {
        conflicts_error,
        light_changes,
        provider_fallback,
        fresh_session,
        ..options.settings(stall, conflicts, disk, resume, limits)
    };
    supervisor::supervise(&ports, &settings)
}
