//! The supervisor (ADR-0024 decision 1): execute claimed tasks in parallel,
//! validate their receipts, review accepted runs headless, land them on
//! main one at a time, triage failed ones and resume parked ones. One run's
//! state machine is unchanged from the single-run supervisor; the loop
//! multiplexes independent slots and isolates failures. A run whose
//! supervisor died while its session lives on is adopted by a supervisor
//! with a free slot instead of being rerun (ADR-0012).
//!
//! Everything outside the process reaches the loop through ports: the
//! queue ([`Queue`], a connection per thread from [`QueueOpener`]), Git
//! ([`Repository`], [`MainRemote`]), the verification commands
//! ([`Verifier`]), cmux ([`WorkspaceBackend`]), the agent
//! ([`AgentProvider`]) and what it shows and writes ([`AgentSignals`]),
//! the processes it starts ([`Spawner`]) and checks ([`ProcessControl`]),
//! the run files ([`RunFiles`]) and the time and IDs ([`Generators`]).
//! Progress and diagnostics are `tracing` events with `run_id` /
//! `task_id` / `ask_id` / `error` fields; the entry point picks their
//! subscriber (ADR-0033).
//!
//! This module holds the loop and the state machine of a slot (`Phase`,
//! `step`); each phase's watch and the supervisor's methods for it are in
//! the submodules: `session` (the worker's session), `exit` (its `/exit`),
//! `jobs` (the headless review and triage), `landing` (the review verdict
//! and the landing), `revise`, `resume`, `triage`, `adopt` and `idle` (the
//! idle marker). The prompts and requests are in [`super::prompt`].

use crate::domain::LeaseToken;
use crate::domain::language::with_instruction;
use crate::domain::slot_limits::{SlotFlags, SlotLimits, SupervisorConfig};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tracing::{error, info, warn};

use super::{
    AgentProvider, AgentSignals, AskQuery, CommandSpec, Exhaustion, Generators, IdleHook,
    InputSource, LeasedRun, MainRemote, ProcessControl, Queue, QueueOpener, Repository,
    ResumeCandidate, RunFiles, Spawned, Spawner, Streams, TRIAGE_ASKER, TriageAction, Validation,
    Verifier, WorkerAdapters, WorkspaceBackend,
    actor_executor::{
        ActorExecutionSpec, ActorExecutor, ActorProgram, HeadlessProgram, HostActorExecutor,
        WorkspaceAccess,
    },
    ask, dependency_graph,
    health::{lease_health, run_health},
    integrate::{self as integration, Integration, IntegrationRequest, Integrator, check_receipt},
    naming::{
        resume_workspace_description, shell_join, workspace_description, workspace_group_name,
    },
    or_none, path_text,
    prompt::{
        GoalPredecessorSummary, HEADLESS_NEVER, Inheritance, PredecessorSummary, RecoveryMaterial,
        ResumeKind, ResumeRequest, TRIAGE_TOOLS, answer_text, continue_text, ended_run_material,
        prompt, recovery_instruction, recovery_prompt, resume_request, review_prompt,
        revise_mismatch_request, revise_request, siblings_in_progress, stale_receipt_nudge,
        stall_nudge,
    },
    recording::{
        RecordingBackend, exit_unsent, reason_of_error, text_on_screen, timed_out_maybe_sent,
    },
    tail, unix_seconds,
};
use crate::domain::{
    ABANDON_EXIT_FAILED, ABANDON_EXIT_REQUESTED_BEFORE, ABANDON_EXIT_SENT, ActorContext,
    AfterValidation, AskId, AskKind, AskReason, ClaimOutcome, CommitSha, ConflictDecision, EventId,
    EvidenceCheck, HEARTBEAT_TIMEOUT_SECS, HOLD_OPTIONS, IntegrationOutcome, LANDING_OPTIONS,
    MAX_RESUME_ATTEMPTS, MAX_REVISE_ATTEMPTS, NewAsk, NewHold, Predecessor, Reason, ReasonCode,
    Receipt, ReceiptResult, ResumedSession, ReviewDecision, ReviewVerdict, ReviseDecision,
    RunHistory, RunId, RunLease, RunPaths, RunPlan, RunProcess, RunStatus, SessionRole,
    TRIAGE_OPTIONS, TRIAGE_RETRY_FAILURES, TaskAction, TaskId, TaskRun, TaskStatus, TriageState,
    after_validation,
    claim_hold::{self, ClaimHold, HoldInputs},
    decide_conflict, decide_revise, event_kind, headless_job, heartbeat_stale,
    kpi::{CANDIDATES_SAMPLED, CandidatesSample},
    marks::{RUN_ENV_CHANGED, SUPERVISOR_STARTED, SUPERVISOR_STOPPED, run_env_digest},
    measure::{ClaimAttributes, HostVersions, LoadSummary, LoadWindow},
    recovery::{RecoveryAlert, RecoveryDecision, RecoveryVerdict, STUCK_EXIT_ACTIONS},
    resume::{ResumeConfig, ResumeCount, inherits_on_exhaustion, is_inherit_retry},
    run_env::RUN_ENV_PROGRAM_KINDS,
    stall::{BackgroundTask, STALL_CONFIG_LOADED, StallConfig},
    triage_state,
    worker::Worker,
    worker_model::{WorkerSession, WorkerTrial},
};

mod adopt;
mod claim_defer;
mod cleanup;
mod deliver;
mod dialog;
mod disk;
mod draft_planner;
mod exit;
mod finding_planner;
mod forecast;
mod goal_review;
mod handoff;
mod headless;
mod host_metrics;
mod idle;
mod jobs;
mod landing;
mod plan_review;
mod push;
mod queue_hold;
mod recheck;
mod recovery;
mod release;
mod report;
mod resume;
mod revise;
mod session;
mod slot_limits;
mod stale;
mod stall;
mod stall_recovery;
mod sweep;
mod triage;
mod update;
mod waiting;

pub use self::forecast::{FORECAST_CHECK, ForecastPort};
pub use self::handoff::SUPERVISOR_HANDED_OFF;
pub use self::host_metrics::HostMetricsPort;
pub use self::release::{RELEASE_LOOK, ReleasePort};
pub use self::report::ReportPort;
pub use self::update::{UPDATE_INTERVAL, UpdateSettings};
use self::{
    deliver::*, dialog::*, exit::*, headless::*, idle::*, jobs::*, recovery::*, resume::*,
    revise::*, session::*, stale::*, stall::*, sweep::*, waiting::*,
};

/// How often the supervisor records the finished transcript turns of the
/// session spans still open (ADR-0048 decision 8).
pub const SESSION_TURNS_INTERVAL: Duration = Duration::from_secs(600);

/// The snapshot of the binary a run's session wrapper runs from, in the
/// run directory: copied for each session (a resume copies it again) so
/// rebuilding the binary does not change a running one, and removed once
/// the run ended and nobody leases it ([`cleanup`]).
pub(super) const RUN_RUNNER_FILE: &str = "runner";

/// How far back the daily observation reads.
pub const DAILY_WINDOW_SECS: i64 = 24 * 60 * 60;

/// The two observations: the hourly one reads what finished since the last
/// one (its cursor), the daily one the last 24 hours for trends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserveMode {
    Hourly,
    Daily,
}

impl ObserveMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hourly => "hourly",
            Self::Daily => "daily",
        }
    }
}

/// How the supervisor loop is driven. `stop` is the graceful drain switch
/// (SIGINT in the CLI): no more claims, exit once every active run rests.
#[derive(Debug, Clone)]
pub struct LoopSettings {
    /// Upper bound on runs executing at once (`parallel`), and on the runs
    /// waiting for a person outside the slots (`max_waiting`, ADR-0062
    /// decision 7; zero keeps every run in its slot), with where each
    /// comes from (task 698).
    pub limits: SlotLimits,
    /// The flags `limits` was resolved from: a value not given is read
    /// again from `[supervisor]` each pass.
    pub slot_flags: SlotFlags,
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
    /// Least time between two sweeps of the workspaces of ended runs; the
    /// first pass sweeps at once.
    pub sweep_interval: Duration,
    /// The thresholds of the stalled-session checks (ADR-0043 decision 4),
    /// recorded as `stall_config_loaded` when the loop starts.
    pub stall: StallConfig,
    /// The thresholds of the `conflict_hotspot` alert, for the files the
    /// plan review is told conflict often.
    pub conflicts: crate::domain::stats::ConflictConfigReport,
    /// Upper bound on the planners the runtime has open at once (ADR-0041
    /// decision 12), apart from the run slots; planners a person opened
    /// do not count.
    pub runtime_planners: usize,
    /// How long a planner a revise went to may take to submit its proposal
    /// again before the inbox is told (ADR-0041 decision 13).
    pub planner_timeout: Duration,
    /// The token of the supervisor this process continues after an exec
    /// (ADR-0045 decision 10); `None` registers a new one.
    pub handoff_token: Option<LeaseToken>,
    /// How `up` started this process (`supervise --mode`): what its start
    /// mark records, since `up` writes the registration's mode only after
    /// it sees the registration (ADR-0051 decision 10).
    pub mode: Option<crate::domain::SupervisorMode>,
    /// The automatic update of this supervisor's binary (ADR-0045
    /// decision 17).
    pub update: UpdateSettings,
    /// `--max-load`: no new run is claimed while the 1-minute load average
    /// is above it (task 327); `None` holds for no load.
    pub max_load: Option<f64>,
    /// How much free disk space a claim and a landing need (task 377).
    pub disk: crate::domain::disk::DiskConfig,
    /// `[resume]`: the limit of a run's conflict-only attempts (ADR-0047
    /// decision 24).
    pub resume: ResumeConfig,
}

/// Where the supervisor works and what it starts: the queue database and
/// its run directory, the repository, the binaries a session and the
/// observer run, this process, and the environment of the processes it
/// starts. Paths only; nothing here is read or written by the use case.
#[derive(Debug, Clone)]
pub struct Layout {
    /// The queue database, canonical.
    pub db: PathBuf,
    /// Where the run directories are made (`<queue dir>/runs`).
    pub runs_dir: PathBuf,
    /// The queue hash: the external ID of the queue's workspace group and
    /// part of every run workspace's description (ADR-0026).
    pub queue_hash: String,
    /// The checkout `supervise` was given, and its Git common directory.
    pub repo_root: PathBuf,
    /// The repository's main checkout, which names it in notifications.
    pub main_checkout: PathBuf,
    pub common_dir: PathBuf,
    /// The `claude` the run sessions start.
    pub claude: PathBuf,
    /// The `codex` a Codex worker starts (`supervise --codex`), resolved
    /// when it was found.
    pub codex: PathBuf,
    /// Each provider's executable as resolved at the start, recorded on
    /// the registration for `status` and `doctor`.
    pub providers: Vec<crate::domain::worker::ProviderCheck>,
    /// The runtime binary the sessions and the observer run (snapshotted
    /// into each run directory).
    pub runner: PathBuf,
    /// This process and its binary's version, recorded on the registration.
    pub pid: u32,
    pub version: String,
    /// Variables the observer's process does not inherit (its role and
    /// actor).
    pub observer_env_remove: Vec<String>,
    /// `planners/` of the queue, where the planners the runtime opens keep
    /// their files, and the plugin directory they load.
    pub planners_dir: PathBuf,
    pub plugin_dir: Option<PathBuf>,
    /// `plan-reviews/` of the queue: one directory per plan review job.
    pub plan_reviews_dir: PathBuf,
    /// `goal-reviews/` of the queue: one directory per goal review job.
    pub goal_reviews_dir: PathBuf,
}

impl Layout {
    /// The supervisor as an actor: `supervisor:<pid>`, what its own events
    /// and the commands it starts for itself record.
    pub fn supervisor_actor(&self) -> ActorContext {
        ActorContext::instance(crate::domain::ActorRole::Supervisor, self.pid)
    }
}

/// The ports the supervisor works through, and where it works.
pub struct Ports<'a> {
    pub queues: Arc<dyn QueueOpener>,
    pub repository: Arc<dyn Repository + Send + Sync>,
    pub remote: Arc<dyn MainRemote + Send + Sync>,
    pub verifier: Arc<dyn Verifier + Send + Sync>,
    pub cmux: &'a dyn WorkspaceBackend,
    /// The adapters of each worker (provider and mode) this binary runs
    /// (ADR-t813-2): every agent is checked before anything is claimed, the
    /// signals of Claude's interactive sessions read the run screens, and a
    /// task whose worker has none is not claimed.
    pub workers: WorkerAdapters<'a>,
    /// Starts the headless review and triage (ADR-0027, ADR-0024).
    pub reviewer: &'a dyn AgentProvider,
    pub spawner: &'a dyn Spawner,
    pub files: Arc<dyn RunFiles>,
    pub processes: Arc<dyn ProcessControl + Send + Sync>,
    pub generators: Generators,
    /// Writes a task's review material (`review`) and reports its path.
    pub review_material: &'a dyn Fn(TaskId) -> Result<Value>,
    /// The log of this start, given the registration's `started_at`.
    /// The 1-minute load average recorded with a failed cmux call, at a
    /// claim and over each interval of a run.
    pub load_average: fn() -> Option<f64>,
    /// The free bytes of the file system of a path (task 377).
    pub free_space: fn(&Path) -> Option<u64>,
    /// The versions of Claude Code (given `--claude`) and of the host's
    /// `rustc` (run in the given checkout) a claim records (task 197).
    pub host_versions: fn(&Path, &Path) -> HostVersions,
    /// Writes the daily KPI reports (ADR-0051 decision 20); `None` writes
    /// none.
    pub reports: Option<ReportPort>,
    /// `[kpi]`'s `max_improvement_proposals` of the main checkout's
    /// `dagq.toml` (ADR-0051 decision 25), read again each time.
    pub max_improvement_proposals: Arc<dyn Fn() -> Result<usize> + Send + Sync>,
    /// `[conflicts]` of the main checkout's `dagq.toml` (`None` for no
    /// file), read again each pass (ADR-0080); `None` when the options
    /// set the thresholds, which are then never read again.
    pub conflicts_file: Option<ConflictsFile>,
    /// `[supervisor]` of the main checkout's `dagq.toml` (`None` for no
    /// file), read again each pass for the values the flags did not give
    /// (task 698); `None` reads nothing.
    pub supervisor_file: Option<SupervisorFile>,
    /// Records the forecast snapshots (ADR-0070 decision 3); `None`
    /// records none.
    pub forecasts: Option<ForecastPort>,
    /// Looks for a new release on crates.io (ADR-t618-1); `None` looks
    /// for none.
    pub release: Option<ReleasePort>,
    /// Records the host's load under `<queue dir>/host/` (task 516);
    /// `None` records none.
    pub host_metrics: Option<HostMetricsPort>,
    pub layout: Layout,
}

/// Reads `[conflicts]` of the main checkout's `dagq.toml` (ADR-0080).
pub type ConflictsFile =
    Arc<dyn Fn() -> Result<Option<crate::domain::stats::ConflictConfig>> + Send + Sync>;

/// Reads `[supervisor]` of the main checkout's `dagq.toml` (task 698).
pub type SupervisorFile = Arc<dyn Fn() -> Result<Option<SupervisorConfig>> + Send + Sync>;

/// Spawn a thread that reports its `tracing` events to the subscriber of
/// the spawning thread, so a supervisor run under a scoped subscriber (the
/// tests) keeps the events of its heartbeat, validations and landings.
pub fn spawn_traced<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> thread::JoinHandle<T> {
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    thread::spawn(move || tracing::dispatcher::with_default(&dispatch, work))
}

/// One process heartbeats its registration (a resident supervisor) and every
/// lease it holds with a single token.
pub struct Heartbeat {
    stop: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
    failed: Arc<AtomicBool>,
}

impl Heartbeat {
    pub fn start(queues: Arc<dyn QueueOpener>, token: LeaseToken) -> Self {
        let (stop, recv) = mpsc::channel();
        let failed = Arc::new(AtomicBool::new(false));
        let flag = failed.clone();
        let worker = spawn_traced(move || {
            let result = (|| -> Result<()> {
                let mut queue = queues.open()?;
                loop {
                    queue.heartbeat(&token)?;
                    match recv.recv_timeout(Duration::from_secs(2)) {
                        Err(mpsc::RecvTimeoutError::Timeout) => (),
                        _ => break,
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                error!(error = %format_args!("{error:#}"), "supervisor heartbeat failed: {error:#}");
                flag.store(true, Ordering::SeqCst);
            }
        });
        Self {
            stop,
            worker: Some(worker),
            failed,
        }
    }

    pub fn check(&self) -> Result<()> {
        ensure!(
            !self.failed.load(Ordering::SeqCst),
            "supervisor heartbeat failed; preserving runs for inspection"
        );
        Ok(())
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// A run this supervisor gave up on; it keeps its status, lease-less, with
/// the message in `last_error`.
#[derive(Debug, Clone, Serialize)]
pub struct RunError {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub message: String,
}

/// Run and monitor tasks until the loop ends: with `once`, when nothing is
/// active or claimable; otherwise on `stop`, or after a provisioning failure
/// has drained the active runs (an error). Every task unblocked by `integrate`
/// is picked up on a later pass with the then-current `main` as its base.
/// Accepted runs are reviewed headless by the reviewer (ADR-0027).
pub fn supervise(ports: &Ports<'_>, settings: &LoopSettings) -> Result<Value> {
    ensure!(
        settings.limits.parallel.value >= 1,
        "parallel must be at least 1"
    );
    let layout = &ports.layout;
    ensure!(
        !layout.db.starts_with(&layout.repo_root) || layout.db.starts_with(&layout.common_dir),
        "keep the queue outside the worktree or under its Git common directory"
    );
    ports.cmux.preflight()?;
    // The screens and idle markers are those of the interactive sessions.
    let interactive = ports
        .workers
        .get(Worker::DEFAULT)
        .context("no adapters for the interactive Claude worker")?;
    for worker in ports.workers.workers() {
        if let Some(adapter) = ports.workers.get(worker) {
            adapter.agent.preflight()?;
        }
    }
    let mut queue = ports.queues.open()?;
    queue.bind_repository(&path_text(&layout.common_dir)?)?;
    let parallel = u32::try_from(settings.limits.parallel.value)
        .context("parallel does not fit a registration")?;
    let pid = layout.pid;
    let mut previous_version = None;
    let token = match &settings.handoff_token {
        // This process exec'd this binary under the registration it had
        // (ADR-0045 decision 10): the same token, pid, mode and leases.
        Some(token) => {
            previous_version = queue
                .supervisors()?
                .into_iter()
                .find(|registration| &registration.token == token)
                .and_then(|registration| registration.binary_version);
            queue.resume_registration(token, pid, &layout.version)?;
            let previous = &previous_version;
            info!(
                "supervisor {token} handed off: version {} (was {}), pid {pid}, parallel {parallel}, db {}, repository {}",
                layout.version,
                previous.as_deref().unwrap_or("unrecorded"),
                layout.db.display(),
                layout.repo_root.display()
            );
            token.clone()
        }
        None => {
            let token = ports.generators.ids.lease_token();
            // Registered before the first heartbeat so the loop is visible
            // to `status` from its first second, runs or not.
            queue.register_supervisor(&token, pid, parallel, &layout.version)?;
            queue.accept_handoff(&token)?;
            if settings.update.register {
                queue.set_auto_update(&token, true)?;
            }
            info!(
                "supervisor {token} started: version {}, pid {pid}, parallel {parallel}, db {}, repository {}",
                layout.version,
                layout.db.display(),
                layout.repo_root.display()
            );
            token
        }
    };
    // Next to `parallel` on the registration, for `status` (ADR-0062
    // decision 7), with where each comes from (task 698); written again by
    // the process an exec continues.
    queue.set_slot_limits(&token, settings.limits)?;
    queue.set_supervisor_providers(&token, &layout.providers)?;
    let mut config = serde_json::to_value(settings.stall)?;
    config["supervisor"] = json!(token);
    queue.record_queue_event(STALL_CONFIG_LOADED, config)?;
    // The mark of this start or handoff (ADR-0051 decision 10): the mode
    // `up` passed, else the registration's (a handoff keeps it).
    let registration = queue
        .supervisors()?
        .into_iter()
        .find(|registration| registration.token == token);
    queue.record_queue_event(
        SUPERVISOR_STARTED,
        json!({
            "supervisor": token,
            "dagq_version": layout.version,
            "parallel": parallel,
            "mode": settings
                .mode
                .or(registration.as_ref().and_then(|r| r.mode))
                .map(|mode| mode.as_str()),
            "auto_update": registration.as_ref().is_some_and(|r| r.auto_update),
            "handoff": settings.handoff_token.is_some(),
            "previous_version": previous_version,
        }),
    )?;
    let heartbeat = Heartbeat::start(ports.queues.clone(), token.clone());
    let cmux = RecordingBackend::over(
        ports.cmux,
        ports.queues.clone(),
        Some(token.clone()),
        ports.load_average,
    );
    let mut supervisor = Supervisor {
        queue,
        queues: ports.queues.clone(),
        layout,
        repository: ports.repository.clone(),
        remote: ports.remote.clone(),
        verifier: ports.verifier.clone(),
        cmux: &cmux,
        reviewer: ports.reviewer,
        signals: interactive.signals,
        workers: ports.workers.workers(),
        spawner: ports.spawner,
        files: ports.files.clone(),
        processes: ports.processes.clone(),
        review_material: ports.review_material,
        token,
        heartbeat,
        slots: Vec::new(),
        parallel: settings.limits.parallel.value,
        max_waiting: settings.limits.max_waiting.value,
        limits: settings.limits,
        slot_flags: settings.slot_flags,
        supervisor_file: ports.supervisor_file.clone(),
        supervisor_error: None,
        finished: Vec::new(),
        errors: Vec::new(),
        claiming: true,
        provisioning_error: None,
        observer: None,
        observers_launched: Vec::new(),
        last_sweep: None,
        last_turns: None,
        process_sample: None,
        sweep_failures: Vec::new(),
        triaged: Vec::new(),
        generators: ports.generators.clone(),
        stall: settings.stall,
        conflicts: settings.conflicts,
        conflicts_file: ports.conflicts_file.clone(),
        conflicts_error: None,
        plan_review: None,
        goal_review: None,
        job_ends: JobEnds::default(),
        jobs_swept: false,
        planner_exits: Vec::new(),
        screen_spans: Default::default(),
        screen_probes: Default::default(),
        handoff: None,
        exec: None,
        run_env_missing: false,
        candidates: None,
        landing_unresolved: false,
        queue_hold: None,
        hold_continue: HashMap::new(),
        draining: false,
        update: update::UpdateWatch::default(),
        rechecks: recheck::Rechecks::default(),
        max_load: settings.max_load,
        load_average: ports.load_average,
        host_versions: ports.host_versions,
        reports: ports.reports.clone(),
        max_improvement_proposals: ports.max_improvement_proposals.clone(),
        report: report::ReportWatch::default(),
        forecasts: ports.forecasts.clone(),
        forecast: forecast::ForecastWatch::default(),
        release_port: ports.release.clone(),
        release: release::ReleaseWatch::default(),
        host_metrics_port: ports.host_metrics.clone(),
        host_metrics: host_metrics::HostMetricsWatch::default(),
        push: push::PushWatch::default(),
        loads: HashMap::new(),
        defer: claim_defer::DeferWatch::default(),
        disk_config: settings.disk,
        resume_config: settings.resume,
        free_space: ports.free_space,
        disk: disk::DiskWatch::default(),
        free: None,
        cleanup: cleanup::CleanupWatch::default(),
    };
    // Before any job starts again: the jobs a gone supervisor left, and
    // after an exec those the previous binary of this process started.
    supervisor.tend_headless_jobs();
    if settings.handoff_token.is_some() {
        // A review rebuilt under an open hold ask waits for it (task 437).
        supervisor.check_queue_hold()?;
        supervisor.rebuild_own_runs(previous_version.as_deref())?;
    }
    let result = supervisor.run_loop(settings);
    match &result {
        Ok(value) => info!("supervisor {} exiting: {value}", supervisor.token),
        Err(error) => {
            error!(error = %format_args!("{error:#}"), "supervisor {} failed: {error:#}", supervisor.token)
        }
    }
    result
}

/// A listing of this user's processes with the wall time it was taken.
type ProcessSample = (SystemTime, Vec<crate::domain::recovery::ProcessInfo>);

struct Supervisor<'a> {
    queue: Box<dyn Queue + Send>,
    /// A connection for each thread beside the loop.
    queues: Arc<dyn QueueOpener>,
    layout: &'a Layout,
    repository: Arc<dyn Repository + Send + Sync>,
    remote: Arc<dyn MainRemote + Send + Sync>,
    verifier: Arc<dyn Verifier + Send + Sync>,
    cmux: &'a dyn WorkspaceBackend,
    /// Starts the headless review of accepted runs (ADR-0027).
    reviewer: &'a dyn AgentProvider,
    /// Reads the screen and the idle marker of the run sessions.
    signals: &'a dyn AgentSignals,
    /// The workers this supervisor runs: a candidate whose worker is not
    /// one of them is not claimed (ADR-t813-2).
    workers: Vec<Worker>,
    spawner: &'a dyn Spawner,
    files: Arc<dyn RunFiles>,
    processes: Arc<dyn ProcessControl + Send + Sync>,
    review_material: &'a dyn Fn(TaskId) -> Result<Value>,
    token: LeaseToken,
    heartbeat: Heartbeat,
    slots: Vec<Slot>,
    /// `--parallel`: the slots in use (the runs not waiting) are held under
    /// it, apart from a run a person moved (ADR-0062 decision 10).
    parallel: usize,
    /// `--max-waiting` (ADR-0062 decision 7).
    max_waiting: usize,
    /// `parallel` and `max_waiting` with where each comes from, as last
    /// resolved (task 698).
    limits: SlotLimits,
    /// The flags given; a value not given follows `[supervisor]`.
    slot_flags: SlotFlags,
    /// Reads `[supervisor]` again each pass; `None` keeps `limits`.
    supervisor_file: Option<SupervisorFile>,
    /// The error the last read of `[supervisor]` failed with, warned of
    /// once until it changes or a read succeeds.
    supervisor_error: Option<String>,
    finished: Vec<TaskRun>,
    errors: Vec<RunError>,
    /// Cleared after a provisioning failure so an unavailable cmux or Git
    /// does not burn through every candidate.
    claiming: bool,
    provisioning_error: Option<String>,
    /// The observer job running now: one at a time, outside the run slots.
    observer: Option<(ObserveMode, Box<dyn Spawned>)>,
    /// When this process last launched each observation, so one that dies
    /// before it records anything is not relaunched on every pass.
    observers_launched: Vec<(ObserveMode, Instant)>,
    /// When this process last swept the workspaces of ended runs
    /// (`LoopSettings::sweep_interval`); `None` until the first pass sweeps.
    last_sweep: Option<Instant>,
    /// When this process last recorded the transcript turns of the open
    /// session spans (ADR-0048 decision 8); `None` until the first pass.
    last_turns: Option<Instant>,
    /// The latest listing of this user's processes for the `idle_process`
    /// alert (task 469): when it was taken, and the listing with its wall
    /// time, `None` when it failed. One listing serves every run.
    process_sample: Option<(Instant, Option<ProcessSample>)>,
    /// The workspaces the sweep could not close: retried on every sweep,
    /// their `cleanup_failed` recorded once per process.
    sweep_failures: Vec<String>,
    /// The runs this process triaged, with where each one went.
    triaged: Vec<Value>,
    /// The clock and IDs `queue` also uses.
    generators: Generators,
    /// The thresholds of the stalled-session checks (ADR-0043 decision 4).
    stall: StallConfig,
    /// The `[conflicts]` thresholds the plan review's hotspots and the
    /// claims deferred on them are judged by, as last read (ADR-0080).
    conflicts: crate::domain::stats::ConflictConfigReport,
    /// Reads `[conflicts]` again each pass (ADR-0080); `None` keeps
    /// `conflicts` as the options set it.
    conflicts_file: Option<ConflictsFile>,
    /// The error the last read of `[conflicts]` failed with, warned of
    /// once until it changes or a read succeeds.
    conflicts_error: Option<String>,
    /// The plan review job running now (ADR-0041 decision 11): one at a
    /// time, queue-wide, outside the run slots.
    plan_review: Option<plan_review::PlanReviewWatch>,
    /// The goal review job running now: one at a time, queue-wide.
    goal_review: Option<goal_review::GoalReviewWatch>,
    /// The ends of this process's headless jobs still to be written to
    /// `headless_jobs` (task 443).
    job_ends: JobEnds,
    /// Whether this process looked for the jobs a gone supervisor left,
    /// its own token's included (after an exec), already.
    jobs_swept: bool,
    /// The runtime's planners this process asked to `/exit`, and when.
    planner_exits: Vec<(crate::domain::PlannerId, Instant)>,
    /// The screen's idle spans of the sessions without a fresh idle marker
    /// (ADR-t803-1), kept here so a disk that takes no file loses none.
    screen_spans: crate::application::screen_idle::Spans,
    /// The last capture of each worker session judged by its screen, and
    /// what it inferred, so a session is captured at most once per
    /// [`idle::probe_interval`] ([`Supervisor::session_idle`]).
    screen_probes: idle::ScreenProbes,
    /// The binary a handoff asked this process to exec (ADR-0045 decision
    /// 10): no new work starts, and the loop ends once every slot rests at
    /// a point the next process rebuilds it from.
    handoff: Option<String>,
    /// Set when the loop ended for that exec: the registration stays.
    exec: Option<String>,
    /// A program `[run.env]` names did not resolve on this process's PATH
    /// at the last claim pass (ADR-0049 decision 9): nothing is claimed and
    /// no passed run lands until it does.
    run_env_missing: bool,
    /// The `candidates_sampled` this process recorded last (ADR-0051
    /// decision 3); `None` until its first claim pass records one.
    candidates: Option<CandidatesSample>,
    /// The landing branch did not resolve at the top of this pass
    /// (ADR-t615-1): nothing is claimed and no passed run lands until it
    /// does.
    landing_unresolved: bool,
    /// The open authentication or usage-limit ask read at the top of this
    /// pass (task 437): no new run is claimed and no headless job starts
    /// while it holds.
    queue_hold: Option<claim_hold::QueueHold>,
    /// The held runs whose session gets the fixed text to go on, with the
    /// ask a person answered `done` (task 437).
    hold_continue: HashMap<RunId, AskId>,
    /// This pass drains (a stop, a handoff, or claiming stopped after a
    /// provisioning failure): nothing may wait for the program to appear.
    draining: bool,
    /// The automatic update's look at main (ADR-0045 decision 17).
    update: update::UpdateWatch,
    /// The landing recheck running and the one due (ADR-0068).
    rechecks: recheck::Rechecks,
    /// `--max-load` (task 327).
    max_load: Option<f64>,
    /// The 1-minute load average, and the host's versions a claim records.
    load_average: fn() -> Option<f64>,
    host_versions: fn(&Path, &Path) -> HostVersions,
    /// Writes the daily KPI reports; `None` writes none.
    reports: Option<ReportPort>,
    /// Records the host's load; `None` records none (task 516).
    host_metrics_port: Option<HostMetricsPort>,
    /// The sample job of the host's load.
    host_metrics: host_metrics::HostMetricsWatch,
    /// Reads the limit on the improvement proposals running.
    max_improvement_proposals: Arc<dyn Fn() -> Result<usize> + Send + Sync>,
    /// The report job and the day the reports were last found written.
    report: report::ReportWatch,
    /// Takes the forecast snapshots; `None` takes none.
    forecasts: Option<ForecastPort>,
    /// The snapshot job and where the last look for triggers left off.
    forecast: forecast::ForecastWatch,
    /// Looks for a new release; `None` looks for none.
    release_port: Option<ReleasePort>,
    /// The release look running and when the last one started.
    release: release::ReleaseWatch,
    /// The KPI push's messages waiting and the one being sent.
    push: push::PushWatch,
    /// The load samples of each held run's current interval (task 197).
    loads: HashMap<RunId, LoadWindow>,
    /// The claims deferred on conflict hotspots (ADR-0069).
    defer: claim_defer::DeferWatch,
    /// `[disk]`: how much free disk space a claim and a landing need
    /// (task 377).
    disk_config: crate::domain::disk::DiskConfig,
    /// `[resume]`: the limit of a run's conflict-only attempts (ADR-0047
    /// decision 24).
    resume_config: ResumeConfig,
    /// Reads the free bytes of the file system of a path.
    free_space: fn(&Path) -> Option<u64>,
    /// The disk between passes (task 377).
    disk: disk::DiskWatch,
    /// The free bytes of the queue's directory read this pass.
    free: Option<u64>,
    /// The cleanup of ended runs' worktrees off the loop (task 405).
    cleanup: cleanup::CleanupWatch,
}

/// One executing run between provisioning and rest.
struct Slot {
    run: TaskRun,
    phase: Phase,
    /// The run waits for a person outside the slots, or waits to go back
    /// to one (ADR-0062).
    waiting: Option<Waiting>,
    /// The asks of the run's waits that ended: none starts another.
    consumed: Vec<AskId>,
    /// The asks `run_waiting_deferred` was recorded for.
    deferred: Vec<AskId>,
}

enum Phase {
    Session(SessionWatch),
    /// Receipt validation runs off the loop; the loop only joins the result.
    /// The run's session (if it still has a workspace) stays open through
    /// validation and review (ADR-0027 decision 1).
    Validating(
        Option<thread::JoinHandle<Result<Validation>>>,
        Option<SessionRef>,
    ),
    /// The headless review of an accepted run (ADR-0023 decision 2).
    Review(ReviewWatch),
    /// An accepted run whose review waits for the authentication or
    /// usage-limit ask that holds the queue's jobs (task 437); its session
    /// stays open, and the review starts once nothing holds.
    ReviewHeld(Option<SessionRef>),
    /// The live session fixes what a `revise` verdict named (ADR-0027
    /// decision 2).
    Revise(ReviseWatch),
    /// The session is asked to `/exit` and its workspace closed before the
    /// run moves on (a landing, an ask, a failed review, or rest).
    Exiting(ExitWatch),
    /// A resumed session of a `needs_session` run (ADR-0019).
    Resume(ResumeWatch),
    /// A run to land (a passed review, or an approved resolved resume)
    /// waits for the single integration slot, keeping its lease.
    AwaitingSlot,
    /// The run lands off the loop, like validation; the landing releases
    /// the lease itself.
    Landing(Option<thread::JoinHandle<Result<IntegrationOutcome>>>),
    /// The recovery job of a `failed` or `interrupted` run (ADR-0047
    /// decision 39), under a lease of its own.
    Recovery(EndedRecovery),
}

/// The session of a run the supervisor keeps open through validation,
/// review and revise (ADR-0027): the worker's own workspace, or the one of
/// the resume that reopened the session (ADR-0019).
#[derive(Debug, Clone)]
struct SessionRef {
    workspace: String,
    /// The resume attempt that opened the workspace; `None` for the
    /// worker's workspace (`task_runs.workspace_id`).
    resume: Option<usize>,
}

/// What the supervisor does once the session exited and its workspace
/// closed.
enum AfterExit {
    /// Wait for the integration slot and land (a passed review, or an
    /// approved resolved resume).
    Land,
    /// Open the `approve_landing` ask (a `concern`, or a `revise` past its
    /// limit) and give the lease back.
    Ask {
        decision: ReviewDecision,
        reasons: Vec<String>,
        summary: String,
        /// Why a `revise` verdict became a question for a person.
        why: Option<String>,
    },
    /// Record `review_failed` and give the lease back.
    ReviewFailed {
        attempt: usize,
        error: String,
        duration_secs: u64,
        /// The attempt whose job ran and wrote `review-N.out` / `.err`:
        /// this one when it ran, the one before when this one could not
        /// start after it (task 426), `None` when no job ran.
        output: Option<usize>,
    },
    /// Give the lease back: a run parked for evidence (its workspace is
    /// closed) or a failed one (its workspace is kept for inspection).
    Rest { close: bool },
}

enum Step {
    Continue,
    Done(Box<TaskRun>),
    /// The triage of a run ended (its verdict acted on, or it failed).
    Triaged(Box<TaskRun>),
    /// The lease now carries another token (an adopter took the run, or
    /// `recover` released it): this process must not touch the run again.
    Disowned,
}
impl Supervisor<'_> {
    /// What a session of `role` starts with (ADR-0079 decision 7):
    /// `[roles.<role>]` of `dagq.toml`, read at each start so a change takes
    /// effect without a restart; none, or a file that cannot be read, starts
    /// it as before.
    pub(super) fn actor_launch(
        &self,
        role: crate::domain::actor_model::ModelRole,
    ) -> crate::domain::actor_model::ActorLaunch {
        match self.verifier.role_models() {
            Ok(models) => models.launch(role),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "[roles.{}] could not be read; starting it as before: {error:#}", role.as_str());
                crate::domain::actor_model::ActorLaunch::default_of(role)
            }
        }
    }

    /// Drive the loop, then remove this process's registration: it is about
    /// to exit, whether it drained its runs, ran out of work, or failed on
    /// a claim or provisioning. Only a heartbeat failure keeps the row (the
    /// database may be unreachable), and it goes stale with the leases.
    fn run_loop(&mut self, options: &LoopSettings) -> Result<Value> {
        let result = self.drive(options);
        self.finish_host_metrics();
        // The jobs the loop stopped last (a handoff stops them all).
        self.write_job_ends();
        // A loop that ended on an error lets the cleanup job end after its
        // current worktree, and records what it did (task 405).
        if result.is_err() {
            self.poll_cleanup(true);
            self.finish_cleanup();
        }
        if self.exec.is_none() {
            // Only a loop that ended on an error leaves one running.
            self.stop_observer("with the supervisor");
        }
        if self.exec.is_none() && self.heartbeat.check().is_ok() {
            // The mark of the stop (ADR-0051 decision 10); an exec leaves it
            // to the next process's handoff mark.
            let stopped = json!({
                "supervisor": self.token,
                "dagq_version": self.layout.version,
                "outcome": if result.is_ok() { "stopped" } else { "failed" },
            });
            if let Err(error) = self.queue.record_queue_event(SUPERVISOR_STOPPED, stopped) {
                warn!(error = %format_args!("{error:#}"), "the supervisor's stop could not be recorded: {error:#}");
            }
            if let Err(error) = self.queue.deregister_supervisor(&self.token) {
                warn!(error = %format_args!("{error:#}"), "supervisor registration could not be removed: {error:#}");
            }
        }
        result
    }
    fn drive(&mut self, options: &LoopSettings) -> Result<Value> {
        loop {
            if let Err(error) = self.heartbeat.check() {
                // Supervisor-level failure: note it on every run and keep the
                // leases and the registration; they go stale once this
                // process is gone.
                for slot in &self.slots {
                    let _ = self.queue.record_runtime_error(
                        slot.run.id(),
                        &format!("{error:#}"),
                        &ReasonCode::LeaseLost.into(),
                    );
                }
                return Err(error);
            }
            let stopping = options.stop.load(Ordering::SeqCst);
            // Every pass, draining or handing off too (task 516).
            self.host_metrics_pass();
            // Before any job starts: none runs twice (task 443).
            self.tend_headless_jobs();
            // What the cleanup job removed is recorded before the disk is
            // read (task 405).
            self.poll_cleanup(stopping || self.handoff.is_some());
            // Every pass, draining or not, so a hold on landings ends as soon
            // as the program is found (ADR-0049 decision 9).
            self.check_run_env_programs()?;
            self.check_landing_branch();
            self.mark_run_env_change()?;
            // Every pass, before any claim: a change of `[conflicts]` takes
            // effect without a restart (ADR-0080).
            self.reread_conflicts()?;
            // And `[supervisor]`: a change of `parallel` or `max_waiting`
            // takes effect without a restart (task 698).
            self.reread_slot_limits()?;
            // Every pass too, so a hold on landings ends as soon as there
            // is room (task 377).
            self.check_disk()?;
            // Every pass too: the answer of an authentication or usage-limit
            // ask is applied and the hold read before any work starts (task
            // 437).
            self.check_queue_hold()?;
            self.draining = stopping || !self.claiming || self.handoff.is_some();
            // Before any new work, draining or not: a drain waits for them
            // (ADR-0062 decision 8).
            self.return_waiting_runs();
            // A stop wins over a handoff: the drain goes on as before.
            if !stopping {
                if self.handoff.is_none() {
                    self.handoff = self.queue.handoff_request(&self.token)?;
                    if let Some(binary) = &self.handoff {
                        info!(
                            "supervisor {} asked to hand off to {binary}: no new work starts; it execs once the validations and landings in progress are done",
                            self.token
                        );
                    }
                }
                if let Some(binary) = self.handoff.clone() {
                    // A landing recheck in progress is waited for: its
                    // command would go on in the scratch worktree the next
                    // process uses (ADR-0068). None starts meanwhile (this
                    // pass drains). So is the cleanup job, which ends after
                    // its current worktree (task 405).
                    self.recheck_pass();
                    if !self.rechecks.running()
                        && !self.cleanup.running()
                        && !self.push.running()
                        && !self.report.running()
                        && !self.host_metrics.running()
                        && !self.forecast.running()
                        && !self.release.running()
                        && self.slots.iter().all(|slot| slot.phase.rebuildable())
                    {
                        let runs = self.prepare_handoff();
                        info!(
                            "supervisor {} execs {binary}, handing over {runs} run(s)",
                            self.token
                        );
                        self.exec = Some(binary.clone());
                        return Ok(json!({
                            "outcome": "handoff",
                            "binary": binary,
                            "token": self.token,
                            "runs": self.finished,
                            "handed_over": runs,
                            "errors": self.errors,
                            "triaged": self.triaged,
                        }));
                    }
                    self.draining = true;
                    self.poll_observer();
                    self.report_pass(false);
                    self.forecast_pass(false);
                    self.release_pass(false);
                    self.push_pass(false);
                    self.tick(true);
                    thread::sleep(options.tick);
                    continue;
                }
            }
            if self.claiming && !stopping {
                self.fill_slots(self.parallel, options.sweep_interval)?;
                self.sample_candidates(self.parallel);
            }
            self.poll_observer();
            self.record_session_turns(false);
            let rechecked = self.recheck_pass();
            // A supervisor that stopped claiming is draining, not observing
            // nor starting plan reviews, nor updating itself.
            // Reaped on every pass, started only by a supervisor at work.
            self.report_pass(!stopping && self.claiming);
            // Looked for by a supervisor at work only: the triggers stay in
            // the queue for the next look (ADR-0070 decision 3).
            self.forecast_pass(!stopping && self.claiming);
            // Looked for by a supervisor at work only (ADR-t618-1).
            self.release_pass(!stopping && self.claiming);
            // A message waiting is sent while the supervisor does not stop.
            self.push_pass(!stopping);
            if !stopping && self.claiming {
                if self.queue_hold.is_none() {
                    self.start_observer_when_due(options);
                }
                self.auto_update_pass(options);
                self.release_update_pass(options);
            }
            // A plan review that just readied tasks is followed by one more
            // pass, which claims them.
            // A login or usage limit that holds the queue starts no job
            // (task 437); one in progress is followed.
            let starting = !stopping && self.claiming && self.queue_hold.is_none();
            let mut progressed = self.plan_review_pass(options, starting);
            progressed |= self.goal_review_pass(starting);
            if self.slots.is_empty() {
                // A running observer, KPI report job, plan review, landing recheck or
                // cleanup for disk space or one a triage or resume waits
                // for is waited for like a run: it is
                // bounded by its own timeout, its command or its worktrees.
                // A recheck just applied is followed by one more pass, which
                // resumes the runs it parked; a cleanup for room by one
                // that claims if there is room now. Any other cleanup is
                // joined once the loop ends.
                let job = self.observer.is_some()
                    || self.report.running()
                    || self.forecast.running()
                    || self.release.running()
                    // A message being sent is bounded by the command's
                    // timeout; `--once` also waits for those still to be
                    // tried, a stop does not.
                    || self.push.running()
                    || (options.once && !stopping && self.push.busy())
                    || self.plan_review.is_some()
                    || self.goal_review.is_some()
                    || self.rechecks.running()
                    || self.cleanup.for_disk()
                    || self.cleanup.deferred
                    || rechecked;
                if !job && !progressed && (options.once || stopping || !self.claiming) {
                    break;
                }
                thread::sleep(if job { options.tick } else { options.idle_poll });
                continue;
            }
            self.tick(false);
            thread::sleep(options.tick);
        }
        self.finish_cleanup();
        if let Some(message) = &self.provisioning_error {
            bail!(
                "{message}; claiming stopped and {} active run(s) were drained; inspect doctor before recovery",
                self.finished.len()
            );
        }
        let outcome = if options.stop.load(Ordering::SeqCst) {
            "stopped"
        } else {
            "finished"
        };
        Ok(json!({
            "outcome": outcome,
            "runs": self.finished,
            "errors": self.errors,
            "triaged": self.triaged,
        }))
    }
    /// Adopt the runs other supervisors left behind, then claim and
    /// provision candidates until every slot is taken or nothing is
    /// claimable. `main` is reread per claim so a task released by
    /// `integrate` starts from the main that contains its predecessor.
    fn fill_slots(&mut self, parallel: usize, sweep_interval: Duration) -> Result<()> {
        // A run that waits for a person is adopted without a free slot
        // (ADR-0062 decision 11): the check is per run.
        self.adopt_stale_runs(parallel)?;
        // Takes no slot: a dead run goes to the triage below.
        self.recover_dead_runs()?;
        if self.used_slots() < parallel {
            self.apply_landing_answers(parallel)?;
        }
        self.apply_triage_answers()?;
        // Resumes and triage read the landing branch: they wait with the
        // claims until it resolves (ADR-t615-1). A resumed session gets
        // `[run.env]` like a claimed run, so it waits with the claims for a
        // missing program too (task 303).
        if self.used_slots() < parallel && !self.landing_unresolved && !self.run_env_missing {
            self.resume_parked_runs(parallel)?;
        }
        if self.used_slots() < parallel && !self.landing_unresolved {
            self.triage_runs(parallel)?;
        }
        // Takes no slot: only closes and frees what ended runs left.
        if let Err(error) = self.sweep_ended_runs(sweep_interval) {
            warn!(error = %format_args!("{error:#}"), "the workspaces and worktrees of ended runs could not all be swept: {error:#}");
        }
        // A run claimed now would fail every cargo command (ADR-0049
        // decision 9; checked at the top of the pass); the runs in flight
        // and their reviews go on (resumes wait above).
        if self.run_env_missing || self.landing_unresolved {
            return Ok(());
        }
        // The runs in flight go on; only new claims wait (task 327).
        if self.hold_claims()? {
            return Ok(());
        }
        let mut host: Option<HostVersions> = None;
        // Read once per pass, so turning the trial on or off takes effect
        // without a restart; a file that cannot be read claims outside it
        // (provisioning reports the file's error).
        let trial = self.verifier.worker_trial().unwrap_or_else(|error| {
            warn!(error = %format_args!("{error:#}"), "[worker.trial] could not be read; claiming without the trial: {error:#}");
            WorkerTrial::default()
        });
        while self.used_slots() < parallel {
            // Highest effective priority, then most-releasing, then lowest
            // ID (ADR-0040 decision 4); `candidates` and `graph` show the
            // same order, so it is not recorded.
            // Less the candidates deferred on a conflict hotspot (ADR-0069).
            let graph = dependency_graph(self.queue.graph_input()?, None);
            let order = self.claimable(&graph)?;
            if order.is_empty() {
                break;
            }
            let base = self.repository.main_head()?;
            // Read once per pass: `rustc -vV` takes a moment on a loaded host.
            let host = host.get_or_insert_with(|| {
                (self.host_versions)(&self.layout.claude, &self.layout.repo_root)
            });
            let attributes = self.claim_attributes(parallel, host.clone());
            let run = match self.queue.claim_for_supervisor_in_order(
                &base,
                &self.token,
                &order,
                Some(&serde_json::to_value(&attributes)?),
                &trial,
                &self.workers,
            )? {
                ClaimOutcome::Claimed { run } => *run,
                ClaimOutcome::NoReadyTask => break,
            };
            self.defer.claimed();
            // The work interval starts at the claim, with its sample.
            let mut window = LoadWindow::default();
            window.add(attributes.load_avg);
            self.loads.insert(run.id().clone(), window);
            match self.provision(&run) {
                Ok(watch) => {
                    let run = self.queue.run(run.id())?;
                    self.slots.push(Slot::new(run, Phase::Session(watch)));
                }
                Err(error) => {
                    let message = format!("run {} provisioning failed: {error:#}", run.id());
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "{message}; no further tasks will be claimed");
                    self.abandon(
                        &run,
                        message.clone(),
                        &reason_of_error(&error, ReasonCode::Other),
                    );
                    self.claiming = false;
                    self.provisioning_error = Some(message);
                    break;
                }
            }
        }
        Ok(())
    }
    /// Record `candidates_sampled` (ADR-0051 decision 3) when this pass's
    /// claimable ready tasks (`graph`'s `candidates`), free slots or ready
    /// tasks differ from the sample this process recorded last, and on its
    /// first claim pass. A failure is logged: the sample is bookkeeping for
    /// `kpi`, and the next pass tries again.
    fn sample_candidates(&mut self, parallel: usize) {
        let sample = self.queue.graph_input().map(|input| {
            let graph = dependency_graph(input, None);
            CandidatesSample {
                candidates: graph.candidates.len(),
                free_slots: parallel.saturating_sub(self.used_slots()),
                ready: graph
                    .tasks
                    .iter()
                    .filter(|node| node.status == TaskStatus::Ready)
                    .count(),
            }
        });
        let result = sample.and_then(|sample| {
            if let Some(mut payload) = sample.transition(self.candidates.as_ref()) {
                payload["supervisor"] = json!(self.token);
                self.queue.record_queue_event(CANDIDATES_SAMPLED, payload)?;
                self.candidates = Some(sample);
            }
            Ok(())
        });
        if let Err(error) = result {
            warn!(error = %format_args!("{error:#}"), "the candidates could not be sampled: {error:#}");
        }
    }
    /// Judge whether new claims are held now ([`ClaimHold::judge`]: the
    /// free disk space this pass read against what a claim needs (task
    /// 377), and the load) and record `claim_held` or `claim_resumed` when
    /// the answer differs from the hold in place on the queue (task 327).
    /// Returns whether they are held.
    fn hold_claims(&mut self) -> Result<bool> {
        let needed = self.disk_needs()?.claim;
        // Short while a cleanup for room runs: wait for it without a hold
        // (task 405).
        if self.disk.cleaning
            && matches!((self.free, needed), (Some(free), Some(need)) if free < need)
        {
            return Ok(true);
        }
        let hold = ClaimHold::judge(&HoldInputs {
            load_average: (self.load_average)(),
            max_load: self.max_load,
            free_bytes: self.free,
            needed_bytes: needed,
            queue_hold: self.queue_hold,
        });
        self.record_hold(claim_hold::CLAIMS, hold.as_ref())
    }
    /// Check the programs `[run.env]` names on this process's PATH
    /// (ADR-0049 decision 9) and record `run_env_program_missing` or
    /// `run_env_program_found` when the answer differs from the latest one
    /// on the queue, so a restarted supervisor does not repeat it. A
    /// `dagq.toml` that cannot be read leaves the last answer: provisioning
    /// and `integrate` report that file themselves.
    fn check_run_env_programs(&mut self) -> Result<()> {
        let check = match self.verifier.run_env_programs(None) {
            Ok(check) => check,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the programs of [run.env] could not be checked: {error:#}");
                return Ok(());
            }
        };
        let last = self.queue.latest_queue_event(&RUN_ENV_PROGRAM_KINDS)?;
        if let Some((kind, mut payload)) = check.transition(last.as_ref().map(|e| e.kind.as_str()))
        {
            payload["supervisor"] = json!(self.token);
            self.queue.record_queue_event(kind, payload)?;
            match check.missing_message() {
                Some(message) => {
                    warn!("{message}; no task is claimed and no run lands until it is found")
                }
                None => {
                    info!("the programs of [run.env] are found again; claiming and landing resume")
                }
            }
        }
        self.run_env_missing = !check.missing().is_empty();
        Ok(())
    }
    /// Resolve the landing branch (ADR-t615-1) and hold claims and
    /// landings while it does not resolve, warning when that changes.
    fn check_landing_branch(&mut self) {
        match self.repository.landing_branch() {
            Ok(branch) => {
                if self.landing_unresolved {
                    info!(branch = %branch.name, "the landing branch resolves again to {}; claiming and landing resume", branch.name);
                }
                self.landing_unresolved = false;
            }
            Err(error) => {
                if !self.landing_unresolved {
                    warn!(error = %format_args!("{error:#}"), "{error:#}; no task is claimed and no run lands until it resolves");
                }
                self.landing_unresolved = true;
            }
        }
    }
    /// Record `run_env_changed` when the normalized `[run.env]` of the main
    /// checkout hashes differently from the latest one on the queue
    /// (ADR-0051 decision 11), so a restarted supervisor does not repeat
    /// it. A `dagq.toml` that cannot be read records nothing: provisioning
    /// and `integrate` report that file themselves. Neither does a missing
    /// one, which may only be a checkout rewriting it; an empty `[run.env]`
    /// is the change that removes the table.
    fn mark_run_env_change(&mut self) -> Result<()> {
        let table = match self.verifier.run_env_table() {
            Ok(Some(table)) => table,
            Ok(None) => return Ok(()),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "[run.env] could not be read for its change mark: {error:#}");
                return Ok(());
            }
        };
        let salt = match self.verifier.run_env_salt() {
            Ok(salt) => salt,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the salt of the [run.env] hashes could not be read: {error:#}");
                return Ok(());
            }
        };
        let last = self.queue.latest_queue_event(&[RUN_ENV_CHANGED])?;
        if let Some(mut payload) =
            run_env_digest(&table, &salt).transition(last.as_ref().map(|event| &event.payload))
        {
            info!(
                "[run.env] changed ({}): recorded as a change mark",
                payload["changed"]
            );
            payload["supervisor"] = json!(self.token);
            self.queue.record_queue_event(RUN_ENV_CHANGED, payload)?;
        }
        Ok(())
    }
    /// What `run_claimed` records of a claim made now (task 197): this
    /// binary's build identifier, the host's versions, `parallel`, the
    /// slots held before the claim (runs waiting for a person hold none,
    /// ADR-0062) and the load average.
    fn claim_attributes(&self, parallel: usize, host: HostVersions) -> ClaimAttributes {
        ClaimAttributes {
            dagq_version: self.layout.version.clone(),
            host,
            parallel,
            slots: self.used_slots(),
            load_avg: (self.load_average)(),
        }
    }
    /// Sample the load average once for every slot's current interval
    /// (task 197); the windows of runs no slot holds any more are dropped.
    fn sample_load(&mut self) {
        let load = (self.load_average)();
        let held: HashSet<&RunId> = self.slots.iter().map(|slot| slot.run.id()).collect();
        self.loads.retain(|id, _| held.contains(id));
        for id in held {
            self.loads.entry(id.clone()).or_default().add(load);
        }
    }
    /// The load over the run's interval that ends now, and start the next.
    pub(super) fn take_load(&mut self, id: &RunId) -> LoadSummary {
        self.loads.entry(id.clone()).or_default().take()
    }
    /// One pass over the slots; with `unsettled_only`, over the slots a
    /// handoff waits for (their validation or landing in progress) only.
    fn tick(&mut self, unsettled_only: bool) {
        self.sample_load();
        if !unsettled_only {
            self.start_waits();
        }
        let mut index = 0;
        while index < self.slots.len() {
            if unsettled_only && self.slots[index].phase.rebuildable() {
                index += 1;
                continue;
            }
            let mut slot = self.slots.remove(index);
            let stepped = if slot.out_of_slot() {
                self.watch_waiting(&mut slot)
            } else {
                self.step(&mut slot)
            };
            match stepped {
                Ok(Step::Continue) => {
                    self.slots.insert(index, slot);
                    index += 1;
                }
                Ok(Step::Done(run)) => {
                    info!(run_id = %run.id(), "run {} is {}", run.id(), run.status().as_str());
                    // Main moved: the runs that wait to land are checked
                    // against it (ADR-0068 decision 1).
                    if run.status() == RunStatus::Integrated {
                        self.note_landed(&run);
                    }
                    // A landed run needs none of its workspaces; a failed
                    // one keeps them for its triage.
                    if matches!(run.status(), RunStatus::Integrated | RunStatus::Succeeded)
                        && let Err(error) =
                            self.close_open_workspaces(&run, WorkspaceCloser::Supervisor)
                    {
                        warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: its workspaces could not all be closed: {error:#}", run.id());
                    }
                    self.clean_task_worktrees(run.task_id());
                    self.finished.push(*run);
                }
                Ok(Step::Triaged(run)) => self.note_triaged(&run),
                Ok(Step::Disowned) => {
                    stop_job(&mut slot);
                    self.disown(&slot)
                }
                // A lease-guarded write that failed because the lease
                // changed hands mid-step (this process was stalled and
                // adopted from) is the other owner's run to describe.
                Err(_)
                    if !self
                        .queue
                        .holds_lease(slot.run.id(), &self.token)
                        .unwrap_or(true) =>
                {
                    self.disown(&slot)
                }
                Err(error) if matches!(slot.phase, Phase::Recovery(_)) => {
                    stop_job(&mut slot);
                    let (round, alert, attempt) = match &slot.phase {
                        Phase::Recovery(watch) => (watch.round, watch.alert, watch.attempt),
                        _ => unreachable!("matched a recovery"),
                    };
                    self.fail_recovery(&slot.run, round, alert, attempt, format!("{error:#}"), 0);
                    let run = self.queue.run(slot.run.id()).unwrap_or(slot.run);
                    self.note_triaged(&run);
                }
                Err(error) if matches!(slot.phase, Phase::AwaitingSlot) => {
                    // The resume already recorded its `resume_finished`;
                    // only the lease it kept for the landing goes.
                    let message = format!("landing could not start: {error:#}");
                    warn!(run_id = %slot.run.id(), "run {}: {message}", slot.run.id());
                    if let Err(error) = self.queue.release_lease(slot.run.id(), &self.token) {
                        warn!(run_id = %slot.run.id(), error = %format_args!("{error:#}"), "run {}: could not release the lease: {error:#}", slot.run.id());
                    }
                    self.errors.push(RunError {
                        run_id: slot.run.id().clone(),
                        task_id: slot.run.task_id(),
                        message,
                    });
                }
                Err(error) if matches!(slot.phase, Phase::Resume(_)) => {
                    // Nobody reads the verdict of its recovery jobs now.
                    stop_recovery(&mut slot);
                    let message = format!("{error:#}");
                    warn!(run_id = %slot.run.id(), "run {} resume stopped: {message}; its workspace is kept for inspection", slot.run.id());
                    let (attempt, workspace) = match &slot.phase {
                        Phase::Resume(watch) => (watch.attempt, Some(watch.workspace.clone())),
                        _ => unreachable!("matched a resume"),
                    };
                    self.give_up_resume(
                        &slot.run,
                        attempt,
                        workspace.as_deref(),
                        message,
                        &reason_of_error(&error, ReasonCode::Other),
                    );
                }
                Err(error) => {
                    // Creation/communication failures can be ambiguous: the
                    // session may be alive. Disown the run, delete nothing,
                    // and keep serving the other slots. A headless review in
                    // progress is stopped: nobody would read its verdict.
                    // A session kept open through validation, review, revise
                    // or its `/exit` is asked to end, since nothing would
                    // watch it any more (task 237).
                    stop_job(&mut slot);
                    let session = self.exit_abandoned_session(&slot);
                    let message = format!("{error:#}");
                    warn!(run_id = %slot.run.id(), "run {} retained for inspection: {message}; see show {} and doctor", slot.run.id(), slot.run.task_id());
                    self.abandon_with_session(
                        &slot.run,
                        message,
                        &reason_of_error(&error, ReasonCode::Other),
                        session.as_ref(),
                    );
                }
            }
        }
    }
    /// The observation due now, if any: the daily one when it has not run
    /// for 24 hours, else the hourly one when the interval passed since the
    /// last one started or finished (from the queue, whichever supervisor
    /// ran it) and since this process last launched it.
    fn due_observation(&self, options: &LoopSettings) -> Result<Option<ObserveMode>> {
        if options.observe_interval.is_zero() {
            return Ok(None);
        }
        let now = self.generators.clock.now();
        let mut modes = vec![(
            ObserveMode::Hourly,
            i64::try_from(options.observe_interval.as_secs())?,
        )];
        if options.observe_daily {
            modes.insert(0, (ObserveMode::Daily, DAILY_WINDOW_SECS));
        }
        for (mode, every) in modes {
            let recorded = self
                .queue
                .last_observe(mode.as_str())?
                .is_some_and(|last| now - last < every);
            let launched = self.observers_launched.iter().any(|(launched, at)| {
                *launched == mode && at.elapsed().as_secs() < every.unsigned_abs()
            });
            if !recorded && !launched {
                return Ok(Some(mode));
            }
        }
        Ok(None)
    }
    /// Launch `dagq observe` as a child process when an observation is due
    /// and none is running. It takes no run slot. A failure to launch is
    /// logged and retried after the interval.
    fn start_observer_when_due(&mut self, options: &LoopSettings) {
        if self.observer.is_some() {
            return;
        }
        let mode = match self.due_observation(options) {
            Ok(Some(mode)) => mode,
            Ok(None) => return,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "observer schedule could not be read: {error:#}");
                return;
            }
        };
        // The observer reads the active time of the spans still open too.
        self.record_session_turns(true);
        self.observers_launched
            .retain(|(launched, _)| *launched != mode);
        self.observers_launched.push((mode, Instant::now()));
        let mut command = CommandSpec::new(&self.layout.runner);
        command
            .arg("--db")
            .arg(&self.layout.db)
            .arg("observe")
            .arg("--claude")
            .arg(&self.layout.claude)
            .current_dir(&self.layout.repo_root);
        for name in &self.layout.observer_env_remove {
            command.env_remove(name);
        }
        // The observe command is the supervisor's; its agent is the
        // observer (ADR-t728-1 decision 4).
        command.envs(self.layout.supervisor_actor().env());
        if mode == ObserveMode::Daily {
            command.arg("--daily");
        }
        match self.spawner.spawn(&command, Streams::Null) {
            Ok(child) => {
                info!("observer ({}) started: pid {}", mode.as_str(), child.id());
                self.observer = Some((mode, child));
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "observer ({}) could not start: {error:#}", mode.as_str())
            }
        }
    }
    /// Apply what the headless job `job` returned: the events written
    /// meanwhile record the supervisor as their actor and the job as
    /// `requested_by` (ADR-t728-1 decision 1, task 730).
    pub(super) fn for_job<T>(
        &mut self,
        job: &ActorContext,
        apply: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        self.queue.request_as(Some(job));
        let result = apply(self);
        self.queue.request_as(None);
        result
    }
    /// Close the review span of `run` when its job ended without a verdict
    /// or could not start: `review_failed` waits for the session's `/exit`,
    /// which is no time of the review (task 541). A failure is logged only
    /// (ADR-0048 decision 10); the span then closes with `review_failed`.
    pub(super) fn close_review_session(&mut self, run: &TaskRun) {
        if let Err(error) = self.queue.close_review_session(run.id()) {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: the span of its failed review could not be closed: {error:#}", run.id());
        }
    }
    /// Record the finished transcript turns of the open session spans when
    /// [`SESSION_TURNS_INTERVAL`] passed since the last time (at once with
    /// `now`). A failure is logged only: it changes no run (ADR-0048
    /// decision 10), and the next time reads the transcripts again.
    fn record_session_turns(&mut self, now: bool) {
        if !now
            && self
                .last_turns
                .is_some_and(|last| last.elapsed() < SESSION_TURNS_INTERVAL)
        {
            return;
        }
        self.last_turns = Some(Instant::now());
        self.close_gone_sessions();
        match self.queue.record_session_turns() {
            Ok(0) => {}
            Ok(spans) => info!("recorded the transcript turns of {spans} open session span(s)"),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "transcript turns could not be recorded: {error:#}")
            }
        }
    }
    /// Close, as `inferred`, the inbox and planner spans whose workspace cmux
    /// no longer lists: their `SessionEnd` never came (ADR-0048 decision 7).
    /// The spans are read before the listing, so a span opened after it is
    /// not taken for gone. A failure is logged only (decision 10).
    fn close_gone_sessions(&mut self) {
        let spans = match self.queue.hook_session_workspaces() {
            Ok(spans) if spans.is_empty() => return,
            Ok(spans) => spans,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the open inbox and planner session spans could not be read: {error:#}");
                return;
            }
        };
        let listed = match self.cmux.listed_workspace_ids() {
            Ok(listed) => listed,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the workspaces of the open inbox and planner session spans could not be listed: {error:#}");
                return;
            }
        };
        let gone: Vec<EventId> = spans
            .into_iter()
            .filter(|(_, workspace)| !listed.iter().any(|id| id.eq_ignore_ascii_case(workspace)))
            .map(|(span, _)| span)
            .collect();
        if gone.is_empty() {
            return;
        }
        match self.queue.close_gone_sessions(&gone) {
            Ok(0) => {}
            Ok(closed) => {
                info!("closed {closed} inbox or planner session span(s) whose workspace is gone")
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the spans of gone inbox and planner sessions could not be closed: {error:#}")
            }
        }
    }
    /// Kill the observer still running and the processes it started (its
    /// agent and that agent's Bash), so none outlives this supervisor or
    /// runs on unwatched after its exec; `why` ends the log line.
    pub(super) fn stop_observer(&mut self, why: &str) {
        let Some((mode, mut child)) = self.observer.take() else {
            return;
        };
        // Listed before the kill: once `observe` is gone, its agent is no
        // longer its descendant.
        let descendants = self.processes.descendants(child.id());
        let _ = child.kill();
        let _ = child.wait();
        for pid in &descendants {
            let _ = self.processes.kill(*pid);
        }
        info!(
            "observer ({}) stopped {why}: pid {} and {} descendant(s) killed",
            mode.as_str(),
            child.id(),
            descendants.len()
        );
    }
    /// Reap the observer once it exited; its own `observe_finished` is the
    /// record.
    fn poll_observer(&mut self) {
        let Some((mode, child)) = self.observer.as_mut() else {
            return;
        };
        let mode = *mode;
        match child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                info!("observer ({}) exited: {status}", mode.as_str());
                self.observer = None;
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "observer ({}) could not be waited for: {error:#}", mode.as_str());
                self.observer = None;
            }
        }
    }
    /// Drop a slot whose lease another process holds now, writing nothing
    /// about the run: the new owner's record is the record.
    fn disown(&mut self, slot: &Slot) {
        let mut message = format!(
            "lease of run {} is held by another process; this supervisor stopped watching it",
            slot.run.id()
        );
        if matches!(slot.phase, Phase::Validating(Some(_), _)) {
            // Its checks finish on their own; the new owner runs its own.
            message.push_str("; a validation already in progress runs to completion unrecorded");
        }
        warn!(run_id = %slot.run.id(), "{}", message);
        self.errors.push(RunError {
            run_id: slot.run.id().clone(),
            task_id: slot.run.task_id(),
            message,
        });
    }
    fn abandon(&mut self, run: &TaskRun, message: String, reason: &Reason) {
        self.abandon_with_session(run, message, reason, None);
    }
    /// [`abandon`](Self::abandon) a run whose live session the supervisor
    /// kept open, recording what became of it (`exit_abandoned_session`).
    fn abandon_with_session(
        &mut self,
        run: &TaskRun,
        message: String,
        reason: &Reason,
        session: Option<&Value>,
    ) {
        if let Err(error) = self
            .queue
            .abandon_run(run.id(), &self.token, &message, reason, session)
        {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: could not record the error: {error:#}", run.id());
        }
        self.errors.push(RunError {
            run_id: run.id().clone(),
            task_id: run.task_id(),
            message,
        });
    }
    /// Ask the live session of a slot being given up on to end (task 237):
    /// the session kept open through validation, review, revise or its
    /// `/exit` would otherwise be left open with nobody watching it, the
    /// run without a lease. `None` when the slot holds no such session, it
    /// ended already, its workspace is gone, or the lease is not known to
    /// be this supervisor's; else the `session` of the `runtime_error`:
    /// `exit` is `sent`, `requested_before` (the `/exit` was typed already
    /// and is never typed twice) or `failed` with the `error`, which
    /// leaves the session to a person (`AttentionNext::ExitSession`).
    fn exit_abandoned_session(&mut self, slot: &Slot) -> Option<Value> {
        let (workspace, requested) = open_session(&slot.phase)?;
        let workspace = workspace.to_owned();
        // Nothing is typed into a session this supervisor may no longer own
        // (the lease check itself failed), nor into one that ended already.
        if !matches!(self.queue.holds_lease(slot.run.id(), &self.token), Ok(true))
            || matches!(session_alive(self, slot.run.id()), Ok(false))
            || matches!(self.cmux.exists(&workspace), Ok(false))
        {
            return None;
        }
        let (exit, error) = if requested {
            (ABANDON_EXIT_REQUESTED_BEFORE, None)
        } else {
            let failed = |why: String| (ABANDON_EXIT_FAILED, Some(why));
            match submit(self, &slot.run, &workspace, Input::Exit, "/exit") {
                Ok(Submission::Submitted(_) | Submission::Queued) => (ABANDON_EXIT_SENT, None),
                Ok(Submission::Dialog(_)) => {
                    failed("a dialog is on the screen, so /exit was not submitted".into())
                }
                Ok(Submission::Stuck(_)) => failed("/exit stayed in the input box".into()),
                Ok(Submission::Unsent) => failed("/exit did not get to the session".into()),
                Err(error) => failed(format!("{error:#}")),
            }
        };
        match &error {
            Some(error) => {
                warn!(run_id = %slot.run.id(), "run {}: /exit could not be sent to its session in {workspace}: {error}; the session is left to a person", slot.run.id());
            }
            None => {
                info!(run_id = %slot.run.id(), "run {}: its session in {workspace} is asked to exit ({exit})", slot.run.id());
            }
        }
        let mut session = json!({"workspace_id": workspace, "exit": exit});
        if let Some(error) = error {
            session["error"] = json!(error);
        }
        Some(session)
    }
    /// A resume that failed in itself (not the session's verdict): record
    /// `resume_finished` with outcome `error` and give the lease back; the
    /// run stays `needs_session` with its reason, and the attempt counts.
    /// The resume workspace is closed only when no session of this resume
    /// can be alive: its wrapper never registered (and, the lease gone, no
    /// longer can) or already exited. `workspace` is `None` when opening it
    /// failed before cmux returned its ID; workspaces are never looked up
    /// by title (ADR-0026).
    fn give_up_resume(
        &mut self,
        run: &TaskRun,
        attempt: usize,
        workspace: Option<&str>,
        message: String,
        reason: &Reason,
    ) {
        let workspace = workspace.map(str::to_owned);
        // The workspace is recorded so a later pass knows it for this run's
        // own once its session has ended (`close_left_resume_workspaces`).
        let payload = reason.on(json!({
            "attempt": attempt,
            "outcome": "error",
            "error": message,
            "workspace_id": workspace,
            "exhausted": resumes_exhausted(&*self.queue, run.id(), self.resume_config),
        }));
        if let Err(error) =
            self.queue
                .finish_resume(run.id(), &self.token, None, None, false, payload)
        {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: could not record the resume error: {error:#}", run.id());
        }
        let session_may_live = self.queue.processes(run.id()).map_or(true, |processes| {
            processes
                .iter()
                .any(|p| p.role == "wrapper" && p.exited_at.is_none())
        });
        if let Some(workspace) = workspace {
            if session_may_live {
                info!(run_id = %run.id(), "run {}: resume workspace {workspace} is kept; its session may still run", run.id());
            } else if let Err(error) = self.cmux.close(&workspace) {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: resume workspace {workspace} could not be closed: {error:#}", run.id());
            }
        }
        self.errors.push(RunError {
            run_id: run.id().clone(),
            task_id: run.task_id(),
            message,
        });
    }
    fn step(&mut self, slot: &mut Slot) -> Result<Step> {
        // A landing releases the lease itself when it ends, so it is joined
        // before the lease is checked.
        if let Phase::Landing(handle) = &mut slot.phase {
            if !handle.as_ref().is_some_and(|h| h.is_finished()) {
                return Ok(Step::Continue);
            }
            let landed = handle
                .take()
                .context("landing already joined")?
                .join()
                .map_err(|_| anyhow!("landing thread panicked"));
            match landed.and_then(|result| result) {
                Ok(outcome) => {
                    let outcome = serde_json::to_value(&outcome)?;
                    info!(run_id = %slot.run.id(), "run {} landing: {}", slot.run.id(), outcome["outcome"]);
                }
                Err(error) => {
                    let message = format!("landing failed: {error:#}");
                    warn!(run_id = %slot.run.id(), "run {}: {message}", slot.run.id());
                    self.errors.push(RunError {
                        run_id: slot.run.id().clone(),
                        task_id: slot.run.task_id(),
                        message,
                    });
                }
            }
            return Ok(Step::Done(Box::new(self.queue.run(slot.run.id())?)));
        }
        if !self.queue.holds_lease(slot.run.id(), &self.token)? {
            return Ok(Step::Disowned);
        }
        match &mut slot.phase {
            Phase::Resume(watch) => {
                let Some(verdict) = watch.poll(self, &slot.run)? else {
                    return Ok(Step::Continue);
                };
                let attempt = watch.attempt;
                let workspace = watch.workspace.clone();
                self.finish_resumed_session(slot, attempt, &workspace, verdict)
            }
            Phase::AwaitingSlot => {
                // Its verification would fail on the missing program: the
                // run stays awaiting integration, leased, and the
                // integration slot stays free (ADR-0049 decision 9). A
                // supervisor that drains or hands off cannot wait for it:
                // it gives the lease back and leaves the run awaiting
                // integration for a person (`review and integrate`).
                // So would it, short of free disk space (task 377): it
                // starts no verification until there is room.
                if self.run_env_missing || self.landing_unresolved || self.disk.landing_short {
                    if !self.draining {
                        return Ok(Step::Continue);
                    }
                    let why = if self.run_env_missing {
                        "a program [run.env] names is missing"
                    } else if self.landing_unresolved {
                        "the landing branch does not resolve"
                    } else {
                        "the free disk space is short of what its verification needs"
                    };
                    warn!(run_id = %slot.run.id(), "run {} is left awaiting integration: {why} and this supervisor stops", slot.run.id());
                    self.queue.release_lease(slot.run.id(), &self.token)?;
                    return Ok(Step::Done(Box::new(self.queue.run(slot.run.id())?)));
                }
                if !self
                    .queue
                    .runs_with_status(RunStatus::Integrating)?
                    .is_empty()
                {
                    return Ok(Step::Continue);
                }
                let current = self.queue.run(slot.run.id())?;
                let previous = current.status();
                let main = self.repository.main_head()?;
                // A head the landing recheck found not landing on this main
                // is parked for a resume instead (ADR-0068 decision 3).
                if let Some(parked) = self.park_held_by_recheck(&current, &main)? {
                    return Ok(Step::Done(Box::new(parked)));
                }
                let run = match self
                    .queue
                    .begin_integration(slot.run.id(), &self.token, &main)
                {
                    Ok(run) => run,
                    // An `integrate` took the slot since the check: try again later.
                    Err(_)
                        if !self
                            .queue
                            .runs_with_status(RunStatus::Integrating)?
                            .is_empty() =>
                    {
                        return Ok(Step::Continue);
                    }
                    Err(error) => return Err(error),
                };
                info!(run_id = %run.id(), "run {} lands onto main {main} ({})", run.id(), previous.as_str());
                slot.phase =
                    Phase::Landing(Some(self.spawn_landing(run.clone(), previous, main)?));
                slot.run = run;
                Ok(Step::Continue)
            }
            Phase::Landing(_) => unreachable!("joined above"),
            Phase::Recovery(watch) => {
                let Some(outcome) = watch.poll(&*self.files)? else {
                    return Ok(Step::Continue);
                };
                let (round, alert, attempt) = (watch.round, watch.alert, watch.attempt);
                let duration_secs = watch.job.started.elapsed().as_secs();
                let run = self.queue.run(slot.run.id())?;
                let acted = match outcome {
                    Ok(verdict) => {
                        let job = ActorContext::recovery_job(run.id(), alert.as_str(), attempt);
                        self.for_job(&job, |sv| {
                            sv.act_on_recovery(&run, round, alert, attempt, duration_secs, verdict)
                        })
                    }
                    Err(error) => Err(anyhow!("{error}")),
                };
                if let Err(error) = acted {
                    // Another process took the run's lease meanwhile: its
                    // round is the record.
                    if !self.queue.holds_lease(run.id(), &self.token)? {
                        return Ok(Step::Disowned);
                    }
                    self.fail_recovery(
                        &run,
                        round,
                        alert,
                        attempt,
                        format!("{error:#}"),
                        duration_secs,
                    );
                }
                Ok(Step::Triaged(Box::new(self.queue.run(run.id())?)))
            }
            Phase::Session(watch) => {
                let Some(run) = watch.poll(self, &slot.run)? else {
                    return Ok(Step::Continue);
                };
                // Parked by its recovery job's `resume` (task 442): the
                // session is asked to exit and its workspace closed, as for
                // a run parked by validation, and the lease goes.
                if run.status() == RunStatus::NeedsSession {
                    let session = SessionRef {
                        workspace: watch.workspace.clone(),
                        resume: None,
                    };
                    slot.run = run;
                    slot.phase = Phase::Exiting(ExitWatch::new(
                        Some(session),
                        AfterExit::Rest { close: true },
                    ));
                    return Ok(Step::Continue);
                }
                if run.status() != RunStatus::Validating {
                    self.queue.release_lease(run.id(), &self.token)?;
                    return Ok(Step::Done(Box::new(run)));
                }
                let session = SessionRef {
                    workspace: watch.workspace.clone(),
                    resume: None,
                };
                let handle = self.validate(run.clone());
                slot.run = run;
                slot.phase = Phase::Validating(Some(handle), Some(session));
                Ok(Step::Continue)
            }
            Phase::Validating(handle, session) => {
                if !handle.as_ref().is_some_and(|h| h.is_finished()) {
                    return Ok(Step::Continue);
                }
                let mut validation = handle
                    .take()
                    .context("validation already joined")?
                    .join()
                    .map_err(|_| anyhow!("validation thread panicked"))??;
                validation.load = self.take_load(slot.run.id());
                let run = self
                    .queue
                    .finish_validation(slot.run.id(), &self.token, &validation)?;
                // Kept in the phase until it is replaced, so an error before
                // that still finds the session to ask to exit (task 237).
                let session = session.clone();
                let events = self.queue.run_events(run.id())?;
                let history = RunHistory::from_events(&events);
                // A landing recheck resumed the run without waiting for the
                // answer to its approve_landing ask (ADR-0068 decision 4).
                let awaits_landing_answer = run.status() == RunStatus::AwaitingIntegration
                    && !history.approved()
                    && crate::domain::resume::parked_by_recheck(history.events())
                    && self
                        .queue
                        .has_unclosed_ask(run.id(), AskKind::ApproveLanding)?;
                slot.phase =
                    match after_validation(run.status(), history.approved(), awaits_landing_answer)
                    {
                        AfterValidation::Land => {
                            Phase::Exiting(ExitWatch::new(session, AfterExit::Land))
                        }
                        AfterValidation::Review => self.start_review(&run, session)?,
                        AfterValidation::Rest { close } => {
                            Phase::Exiting(ExitWatch::new(session, AfterExit::Rest { close }))
                        }
                    };
                slot.run = run;
                Ok(Step::Continue)
            }
            Phase::ReviewHeld(session) => {
                if self.queue_hold.is_none() {
                    let session = session.take();
                    let run = self.queue.run(slot.run.id())?;
                    slot.phase = self.start_review(&run, session)?;
                    slot.run = run;
                }
                Ok(Step::Continue)
            }
            Phase::Review(watch) => {
                let Some(outcome) = watch.poll(&*self.files)? else {
                    return Ok(Step::Continue);
                };
                let attempt = watch.attempt;
                let duration_secs = watch.job.started.elapsed().as_secs();
                // Kept in the phase until it is replaced (task 237).
                let session = watch.session.clone();
                let run = self.queue.run(slot.run.id())?;
                slot.phase = match outcome {
                    ReviewEnd::Verdict(verdict) => {
                        let job = ActorContext::review_job(run.id(), attempt);
                        self.for_job(&job, |sv| {
                            sv.queue.record_runtime_event(
                                run.id(),
                                event_kind::REVIEW_FINISHED,
                                json!({
                                    "verdict": verdict.verdict,
                                    "reasons": verdict.reasons,
                                    "summary": verdict.summary,
                                    "duration_secs": duration_secs,
                                    "attempt": attempt,
                                }),
                            )?;
                            info!(run_id = %run.id(), "run {} review {attempt}: {} ({})", run.id(), verdict.verdict.as_str(), verdict.summary);
                            sv.act_on_verdict(&run, session, verdict)
                        })?
                    }
                    ReviewEnd::Unreadable(error) if !watch.retried => {
                        self.retry_review(&run, session, attempt, &error)?
                    }
                    ReviewEnd::Unreadable(error) | ReviewEnd::Failed(error) => {
                        warn!(run_id = %run.id(), error = %error, "run {} review {attempt} failed: {error}; a person is asked", run.id());
                        self.close_review_session(&run);
                        Phase::Exiting(ExitWatch::new(
                            session,
                            AfterExit::ReviewFailed {
                                attempt,
                                error,
                                duration_secs,
                                output: Some(attempt),
                            },
                        ))
                    }
                };
                slot.run = run;
                Ok(Step::Continue)
            }
            Phase::Revise(watch) => {
                let Some(outcome) = watch.poll(self, &slot.run)? else {
                    return Ok(Step::Continue);
                };
                let session = watch.session.clone();
                let label = watch.fix.label(watch.attempt);
                match outcome {
                    ReviseOutcome::Rewritten(head) => {
                        let kind = match watch.fix {
                            Fix::Revise(_) => event_kind::REVISE_FINISHED,
                            Fix::Conflict(_) => event_kind::CONFLICT_RESOLVED,
                        };
                        self.queue.record_runtime_event(
                            slot.run.id(),
                            kind,
                            json!({"attempt": watch.attempt, "head": head}),
                        )?;
                        info!(run_id = %slot.run.id(), "run {} rewrote its receipt for {label} (head {head}); validating again", slot.run.id());
                        let run = self.queue.restart_validation(slot.run.id(), &self.token)?;
                        let handle = self.validate(run.clone());
                        slot.run = run;
                        slot.phase = Phase::Validating(Some(handle), Some(session));
                    }
                    ReviseOutcome::Mismatch(code, why) => {
                        let message = with_instruction(
                            revise_mismatch_request(&slot.run, &label, &why)?,
                            self.verifier.language().as_ref(),
                        );
                        // Only what the session writes after this counts.
                        let sent_at = self.files.now();
                        let run = slot.run.clone();
                        match submit(
                            self,
                            &run,
                            &session.workspace,
                            Input::Text(&message),
                            "receipt fix request",
                        ) {
                            Ok(submission) => {
                                watch.requested(
                                    sent_at,
                                    StartCheck::new(
                                        "receipt fix request",
                                        &message,
                                        sent_at,
                                        &submission,
                                    ),
                                );
                                let kind = match watch.fix {
                                    Fix::Revise(_) => event_kind::REVISE_RECEIPT_REJECTED,
                                    Fix::Conflict(_) => event_kind::CONFLICT_RECEIPT_REJECTED,
                                };
                                self.queue.record_runtime_event(
                                    slot.run.id(),
                                    kind,
                                    json!({"code": code, "attempt": watch.attempt, "reason": why}),
                                )?;
                                info!(run_id = %slot.run.id(), "run {}: {why}; asked the session to fix it ({label})", slot.run.id());
                            }
                            Err(error) => {
                                let why = format!(
                                    "{why}, and the request to fix it could not be sent: {error:#}"
                                );
                                warn!(run_id = %slot.run.id(), "run {}: {why}", slot.run.id());
                                let then = watch.fix.ask(why.clone(), why);
                                slot.phase = Phase::Exiting(ExitWatch::new(Some(session), then));
                            }
                        }
                    }
                    ReviseOutcome::Ended(why) => {
                        info!(run_id = %slot.run.id(), "run {}: the session {why} after {label}; asking a person", slot.run.id());
                        let then = watch.fix.ask(
                            format!("the session {why}"),
                            format!("the session {why} after {label}"),
                        );
                        let mut exit = ExitWatch::new(Some(session), then);
                        // A silence the revise's wait recorded is not
                        // recorded twice.
                        exit.silent = watch.live.silent && why.starts_with("went silent");
                        slot.phase = Phase::Exiting(exit);
                    }
                }
                Ok(Step::Continue)
            }
            Phase::Exiting(watch) => {
                if !watch.poll(self, &slot.run)? {
                    return Ok(Step::Continue);
                }
                let session = watch.session.take();
                let then = std::mem::replace(&mut watch.then, AfterExit::Rest { close: false });
                let mut run = self.queue.run(slot.run.id())?;
                let close = !matches!(then, AfterExit::Rest { close: false });
                if close && let Some(session) = &session {
                    run = self.close_session(&run, session)?;
                }
                match then {
                    AfterExit::Land => {
                        self.queue_landing(&run, "exit");
                        slot.run = run;
                        slot.phase = Phase::AwaitingSlot;
                        Ok(Step::Continue)
                    }
                    AfterExit::Ask {
                        decision,
                        reasons,
                        summary,
                        why,
                    } => {
                        let ask = self.open_landing_ask(
                            &run,
                            decision,
                            &reasons,
                            &summary,
                            why.as_deref(),
                        )?;
                        info!(run_id = %run.id(), "run {} waits for a person in ask {ask}", run.id());
                        self.queue.release_lease(run.id(), &self.token)?;
                        Ok(Step::Done(Box::new(self.queue.run(run.id())?)))
                    }
                    AfterExit::ReviewFailed {
                        attempt,
                        error,
                        duration_secs,
                        output,
                    } => {
                        // The ask goes with the failure (task 328): the
                        // person answers it rather than finding the run in
                        // the attention. When it cannot be opened, the
                        // failure is the attention (review by hand).
                        let ask = match self.open_failed_review_ask(&run, attempt, output, &error) {
                            Ok(ask) => {
                                info!(run_id = %run.id(), "run {} waits for a person in ask {ask} after its failed review", run.id());
                                Some(ask)
                            }
                            Err(ask_error) => {
                                warn!(run_id = %run.id(), error = %format_args!("{ask_error:#}"), "run {}: the approve_landing ask of its failed review could not be opened: {ask_error:#}; it waits for a review by hand", run.id());
                                None
                            }
                        };
                        let mut payload = json!({
                            "code": ReasonCode::JobFailed,
                            "attempt": attempt,
                            "error": error,
                            "duration_secs": duration_secs,
                            "status": run.status().as_str(),
                        });
                        if let Some(ask) = ask {
                            payload["ask_id"] = json!(ask);
                        }
                        self.queue.record_runtime_event(
                            run.id(),
                            event_kind::REVIEW_FAILED,
                            payload,
                        )?;
                        self.queue.release_lease(run.id(), &self.token)?;
                        Ok(Step::Done(Box::new(self.queue.run(run.id())?)))
                    }
                    AfterExit::Rest { .. } => {
                        self.queue.release_lease(run.id(), &self.token)?;
                        Ok(Step::Done(Box::new(run)))
                    }
                }
            }
        }
    }
}

/// Kill the headless job (a review or a recovery job) of a slot the
/// supervisor stops watching.
fn stop_job(slot: &mut Slot) {
    match &mut slot.phase {
        Phase::Review(watch) => watch.job.stop(),
        Phase::Recovery(watch) => watch.job.stop(),
        _ => stop_recovery(slot),
    }
}

/// The live session a slot keeps open through validation, review, revise
/// or its `/exit` (ADR-0027), and whether the `/exit` was typed into it
/// already.
fn open_session(phase: &Phase) -> Option<(&str, bool)> {
    match phase {
        Phase::Validating(_, Some(session)) => Some((&session.workspace, false)),
        Phase::Review(watch) => watch
            .session
            .as_ref()
            .map(|session| (session.workspace.as_str(), false)),
        Phase::Revise(watch) => Some((&watch.session.workspace, false)),
        Phase::Exiting(watch) => watch.session.as_ref().map(|session| {
            // A `/exit` that never got there (task 354) was not typed.
            let typed = watch.requested.is_some() && watch.unsent.is_none();
            (session.workspace.as_str(), typed)
        }),
        _ => None,
    }
}

/// Kill the recovery job of a live session's alert the slot's watch runs.
fn stop_recovery(slot: &mut Slot) {
    match &mut slot.phase {
        Phase::Session(watch) => watch.recovery.stop_job(),
        Phase::Exiting(watch) => watch.recovery.stop_job(),
        Phase::Resume(watch) => {
            watch.recovery.stop_job();
            watch.live.recovery.stop_job();
        }
        Phase::Revise(watch) => watch.live.recovery.stop_job(),
        _ => {}
    }
}

/// Whether the run's resumes are used up (ADR-0047 decision 24: its
/// counted resumes or its conflict-only ones, up to `config`'s limit);
/// unreadable counts as used up.
fn resumes_exhausted(queue: &dyn Queue, run_id: &RunId, config: ResumeConfig) -> bool {
    queue
        .run_events(run_id)
        .map_or(true, |events| ResumeCount::of(&events).exhausted(config))
}

/// Whether the run's session wrapper is registered, has not exited and has
/// not died without recording its exit (its heartbeat expired and its
/// process is gone): a session that died is one that ended (task 236).
fn session_alive(sv: &Supervisor<'_>, run_id: &RunId) -> Result<bool> {
    let now = sv.generators.clock.now();
    Ok(sv
        .queue
        .processes(run_id)?
        .iter()
        .any(|p| p.role == "wrapper" && p.exited_at.is_none() && !wrapper_dead(sv, p, now)))
}

/// Whether a wrapper that has not recorded its exit died: its heartbeat is
/// older than `HEARTBEAT_TIMEOUT_SECS` and its process is gone.
fn wrapper_dead(sv: &Supervisor<'_>, wrapper: &RunProcess, now: i64) -> bool {
    now - wrapper.heartbeat_at > HEARTBEAT_TIMEOUT_SECS && !sv.processes.alive(wrapper.pid)
}

/// The `reasons` of the run's latest review: those of a `review_finished`,
/// or the error of a `review_failed` after it.
fn latest_review_reasons(queue: &dyn Queue, run_id: &RunId) -> Result<Vec<String>> {
    Ok(queue
        .run_events(run_id)?
        .into_iter()
        .rev()
        .find(|e| {
            matches!(
                e.kind.as_str(),
                event_kind::REVIEW_FINISHED | event_kind::REVIEW_FAILED
            )
        })
        .and_then(|e| match e.kind.as_str() {
            event_kind::REVIEW_FAILED => e.payload["error"]
                .as_str()
                .map(|error| vec![format!("the headless review failed: {error}")]),
            _ => serde_json::from_value(e.payload["reasons"].clone()).ok(),
        })
        .unwrap_or_default())
}

/// Cross-check the agent's receipt against Git on a thread with its own
/// connection. The task's verification commands do not run here: `integrate`
/// runs them once, after its rebase (ADR-0023 decision 1). Rejections become a
/// `Validation` that is not accepted; only errors in the checks themselves
/// propagate, leaving the run in `validating`.
fn spawn_validation(
    queues: Arc<dyn QueueOpener>,
    repository: Arc<dyn Repository + Send + Sync>,
    files: Arc<dyn RunFiles>,
    run: TaskRun,
) -> thread::JoinHandle<Result<Validation>> {
    spawn_traced(move || {
        let mut queue = queues.open()?;
        let task = queue.show(run.task_id())?.task;
        let checked = check_receipt(&*repository, &*files, &task, &run)?;
        Ok(match checked {
            Ok((receipt, commit)) => Validation {
                accepted: true,
                result_commit: Some(commit),
                reason: None,
                code: None,
                receipt: serde_json::to_value(receipt)?,
                evidence_missing: Vec::new(),
                scope_violation: Vec::new(),
                allowed_paths: Vec::new(),
                load: LoadSummary::default(),
            },
            Err(rejection) => {
                warn!(run_id = %run.id(), "run {} rejected: {}", run.id(), rejection.reason);
                Validation {
                    accepted: false,
                    result_commit: rejection.commit,
                    reason: Some(rejection.reason),
                    code: Some(rejection.code),
                    receipt: rejection
                        .receipt
                        .map(serde_json::to_value)
                        .transpose()?
                        .unwrap_or(Value::Null),
                    evidence_missing: rejection.evidence_missing,
                    allowed_paths: if rejection.scope_violation.is_empty() {
                        Vec::new()
                    } else {
                        task.paths().to_vec()
                    },
                    scope_violation: rejection.scope_violation,
                    load: LoadSummary::default(),
                }
            }
        })
    })
}

/// Close the cmux workspace of an accepted run. The worktree and branch stay
/// until integration. A close failure is recorded but does not change the run
/// status; `workspace_closed_at` stays null so nothing treats it as cleaned.
fn close_workspace(
    queue: &mut dyn Queue,
    cmux: &dyn WorkspaceBackend,
    token: &LeaseToken,
    run: &TaskRun,
) -> Result<TaskRun> {
    let workspace = run.workspace_id().context("missing workspace")?;
    match cmux.close(workspace) {
        Ok(()) => queue.workspace_closed(run.id(), token),
        Err(error) => {
            let message = format!("workspace {workspace} could not be closed: {error:#}");
            warn!(run_id = %run.id(), "run {}: {message}", run.id());
            queue.cleanup_failed(
                run.id(),
                token,
                &message,
                &reason_of_error(&error, ReasonCode::BackendFailed),
            )
        }
    }
}
