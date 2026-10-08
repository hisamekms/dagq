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
//! ([`Verifier`]), the session wrappers ([`SessionWrappers`]), the agent
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
//! idle marker). The state of 観測と分析 and host運用 is apart from the
//! loop's, in [`contexts`]. The prompts and requests are in
//! [`super::prompt`].

use crate::domain::EventKind;
use crate::domain::LeaseToken;
use crate::domain::Priority;
use crate::domain::background_wrapper::StopRoute;
use crate::domain::light_slots::{self, ClaimRoom};
use crate::domain::slot_limits::{SlotFlags, SlotLimits, SupervisorConfig};
use crate::domain::slot_order::{SlotCandidate, SlotKind, slot_order};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tracing::{error, info, warn};

use super::{
    AgentProvider, AgentSignals, AskQuery, CommandSpec, Exhaustion, Generators, IdleHook,
    LeasedRun, MainRemote, ProcessControl, Queue, QueueOpener, Repository, ResumeCandidate,
    RunFiles, RunLog, SessionWrappers, Spawned, Spawner, Streams, TRIAGE_ASKER, TriageAction,
    Validation, Verifier, WorkerAdapters,
    actor_executor::{
        ActorExecutionSpec, ActorExecutor, ActorProgram, HeadlessProgram, HostActorExecutor,
        WorkspaceAccess,
    },
    ask, dependency_graph,
    health::{lease_health, run_health},
    integrate::{self as integration, Integration, IntegrationRequest, Integrator, check_receipt},
    naming::shell_join,
    or_none, path_text,
    prompt::{
        FittedPrompt, GoalPredecessorSummary, HEADLESS_NEVER, Inheritance, LandedTask,
        PredecessorSummary, PromptBytes, RESUME_REQUEST_LIMIT, REVISE_REQUEST_LIMIT,
        RecoveryMaterial, ResumeKind, ResumeRequest, TRIAGE_ACCESS, answer_text,
        closed_question_notice, continue_text, ended_run_material, prompt, recovery_instruction,
        recovery_prompt, restored_request, resume_request, review_prompt, revise_findings,
        revise_mismatch_request, revise_request, siblings_in_progress, stale_receipt_nudge,
        stall_nudge,
    },
    recording::{RecordingSessions, reason_of_error},
    tail, unix_seconds,
};
use crate::domain::{
    ABANDON_EXIT_FAILED, ABANDON_EXIT_REQUESTED_BEFORE, ABANDON_EXIT_SENT, ActorContext,
    AfterValidation, AskId, AskKind, AskReason, ClaimOutcome, CommitSha, ConflictDecision, EventId,
    EvidenceCheck, HEARTBEAT_TIMEOUT_SECS, IntegrationOutcome, LANDING_OPTIONS,
    MAX_RESUME_ATTEMPTS, MAX_REVISE_ATTEMPTS, NewAsk, NewHold, Provider, Reason, ReasonCode,
    Receipt, ReceiptResult, ResumedSession, ReviewDecision, ReviewVerdict, ReviseDecision,
    RunEvent, RunHistory, RunId, RunLease, RunPaths, RunPlan, RunProcess, RunStatus, SessionRole,
    TRIAGE_OPTIONS, TRIAGE_RETRY_FAILURES, TaskAction, TaskId, TaskRun, TaskStatus, TriageState,
    after_validation,
    claim_hold::{self, ClaimHold, HoldInputs},
    claim_spacing, decide_conflict, decide_revise, event_kind, headless_job, heartbeat_stale,
    instructions::{self, InstructionVersions},
    kpi::CandidatesSample,
    marks::{RUN_ENV_CHANGED, run_env_digest},
    measure::{ClaimAttributes, ClaimSpacing, HostVersions, LoadSummary, LoadWindow},
    queue_hold::{HoldJob, Wall},
    recovery::{RecoveryAlert, RecoveryDecision, RecoveryVerdict},
    resume::{
        KILL_ONLY_RESUME_LIMIT, ResumeConfig, ResumeCount, inherits_on_exhaustion,
        uses_automatic_inherit,
    },
    run_env::RUN_ENV_PROGRAM_KINDS,
    stall::{BackgroundTask, StallConfig},
    triage_state,
    worker::Worker,
    worker_model::{WorkerSession, WorkerTrial},
};
use contexts::{HostOpsState, ObservationState, PassEnv};

mod adopt;
mod background;
mod ci_watch;
mod claim_defer;
mod cleanup;
mod contexts;
mod deliver;
mod disk;
mod draft_planner;
mod e2e;
mod exit;
mod file_time;
mod finding_planner;
mod forecast;
mod goal_review;
mod handoff;
mod headless;
mod host_metrics;
mod idle;
mod inbox_nudge;
mod jobs;
mod landing;
mod observer;
mod plan_review;
mod planner_turns;
mod provider;
mod push;
mod queue_hold;
mod queue_service;
mod recheck;
mod recovery;
mod release;
mod reopen;
mod report;
mod request_planner;
mod resume;
mod revise;
mod sccache;
mod session;
mod slot_limits;
mod stale;
mod stall;
mod stall_recovery;
mod sweep;
mod throughput_review;
mod triage;
mod update;
mod waiting;

pub(crate) use self::background::{
    left_planner_turn, left_turn, stop_left_turn, stop_run_session, stop_session,
};
pub use self::ci_watch::{CiSourceMaker, CiWatchFile, CiWatchPort};
pub use self::claim_defer::read_conflicts_at_start;
pub(crate) use self::deliver::{Input, Submission};
pub use self::disk::CLEANUP_INTERVAL;
pub use self::e2e::RunE2ePort;
pub use self::forecast::{FORECAST_CHECK, ForecastPort};
pub use self::handoff::SUPERVISOR_HANDED_OFF;
pub(crate) use self::headless::{lock_waiting, provider_failure, write_request};
pub use self::host_metrics::HostMetricsPort;
pub use self::queue_service::{
    QUEUE_SERVICE_INTERVAL, QUEUE_SERVICE_RESTART_WINDOW, QUEUE_SERVICE_RESTARTS,
    QUEUE_SERVICE_START_TIMEOUT, QueueServicePort,
};
pub use self::release::{RELEASE_LOOK, ReleasePort};
pub use self::report::ReportPort;
pub use self::sccache::SccachePort;
pub use self::update::{UPDATE_INTERVAL, UpdateSettings};
use self::{
    deliver::*, exit::*, headless::*, idle::*, jobs::*, provider::*, recovery::*, resume::*,
    revise::*, session::*, stale::*, stall::*, sweep::*, waiting::*,
};

/// What a candidate in the line of a fill pass starts (ADR-t1850-1).
enum LineItem {
    Resume(Box<ResumeCandidate>),
    Recovery(Box<TaskRun>),
    /// The claim loop of the candidate's priority (it reads its own order).
    Claim,
}

/// What the claims of one fill pass carry from one priority to the next.
struct ClaimPass {
    /// Since when a claim waits for the claim spacing (ADR-t1479-1).
    spaced_since: Option<i64>,
    host: Option<HostVersions>,
    /// The hash of the plugin a Claude worker loads, read once per pass.
    plugin: Option<String>,
    trial: WorkerTrial,
}

/// How a claim loop of a fill pass ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimStep {
    /// The candidates of its priority ran out; a lower one may claim.
    Next,
    /// No further claim may start in this pass.
    Stop,
}

/// How often the supervisor records the finished transcript turns of the
/// session spans still open (ADR-0048 decision 8).
pub const SESSION_TURNS_INTERVAL: Duration = Duration::from_secs(600);

/// The snapshot of the binary a run's session wrapper runs from, in the
/// run directory: copied for each session (a resume copies it again) so
/// rebuilding the binary does not change a running one, and removed once
/// the run ended and nobody leases it ([`cleanup`]).
pub(super) const RUN_RUNNER_FILE: &str = "runner";

/// The longest a pass keeps the landing branch's last resolution while the
/// stamp of its inputs stays the same (task 1078): a bound for a change the
/// stamp misses, such as an edit in place that keeps the size, the inode
/// and a coarse modification time.
pub const LANDING_BRANCH_RECHECK: Duration = Duration::from_secs(5);

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
    /// Explicit operator policy: never start Claude; unsupported roles wait for manual handling.
    pub no_claude: bool,
    /// Retry once a review with an unreadable verdict or a job that exited
    /// non-zero; tests may disable the retry.
    pub retry_unreadable_review: bool,
    /// Upper bound on runs executing at once (`parallel`), on the runs
    /// waiting for a person outside the slots (`max_waiting`, ADR-0062
    /// decision 7; zero keeps every run in its slot), and on the planners
    /// the runtime has open at once (`runtime_planners`, ADR-0041 decision
    /// 12; apart from the run slots, and planners a person opened do not
    /// count), with where each comes from (task 698, task 941).
    pub limits: SlotLimits,
    /// The flags `limits` was resolved from: a value not given is read
    /// again from `[supervisor]` each pass.
    pub slot_flags: SlotFlags,
    /// `[supervisor] light_changes` at the start (ADR-t1591-1), read again
    /// each pass.
    pub light_changes: crate::domain::light_slots::LightChanges,
    /// `[provider_fallback]` at the start (ADR-t1857-1), read again each
    /// pass.
    pub provider_fallback: crate::domain::provider_switch::ProviderFallback,
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
    /// (ADR-t996-1).
    pub throughput_review: bool,
    /// The host's time zone at a unix second, seconds east of UTC: where the
    /// hours, days and weeks of the throughput reviews begin.
    pub utc_offset: fn(i64) -> i64,
    /// Pause between two passes over the active runs; tests shorten it.
    pub tick: Duration,
    /// Pause between two looks for claimable work while no run is active.
    pub idle_poll: Duration,
    /// The longest a pass keeps the landing branch's resolution while its
    /// inputs look unchanged ([`LANDING_BRANCH_RECHECK`]).
    pub landing_recheck: Duration,
    /// How often the registration and the leases are heartbeat
    /// ([`HEARTBEAT_INTERVAL`]; tests shorten it).
    pub heartbeat_interval: Duration,
    /// Least time between two sweeps of the workspaces of ended runs; the
    /// first pass sweeps at once.
    pub sweep_interval: Duration,
    /// Least time from the end of a cleanup for room (the rest of one
    /// another job took on included) to the next one
    /// ([`CLEANUP_INTERVAL`]; tests shorten it, task 1627).
    pub disk_cleanup_interval: Duration,
    /// The thresholds of the stalled-session checks (ADR-0043 decision 4),
    /// recorded as `stall_config_loaded` when the loop starts.
    pub stall: StallConfig,
    /// The thresholds of the `conflict_hotspot` alert, for the files the
    /// plan review is told conflict often.
    pub conflicts: crate::domain::stats::ConflictConfigReport,
    /// The error `[conflicts]` could not be read with at the start, warned
    /// of then: the first read again does not warn of it once more
    /// (ADR-t775-1).
    pub conflicts_error: Option<String>,
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
    /// Counts the loop's passes, one at the top of each; only read by
    /// tests, which wait for passes after a threshold instead of a fixed
    /// sleep (task 1046).
    pub passes: Arc<AtomicU64>,
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
    /// The cmux of `supervise --cmux`: the supervisor calls none
    /// (ADR-t1433-1); only the e2e it runs before a landing pings it, as
    /// the e2e gate's precondition (ADR-t1233-2 decision 3).
    pub cmux: PathBuf,
    /// The `codex` a Codex worker starts (`supervise --codex`), resolved
    /// when it was found.
    pub codex: PathBuf,
    /// Codex's home given to the supervisor (tests), passed on to the
    /// throughput review's command; `None` is Codex's own.
    pub codex_home: Option<PathBuf>,
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
    /// The session wrappers of the runs and the runtime's planners, which
    /// the supervisor starts, stops and checks without cmux (ADR-t1433-1).
    pub sessions: &'a dyn SessionWrappers,
    /// The adapters of each worker (provider and mode) this binary runs
    /// (ADR-t813-2): every agent is checked before anything is claimed, the
    /// signals of Claude's headless adapter read every idle marker and the
    /// jobs' failures, and a task whose provider has no headless worker
    /// here is not claimed on it.
    pub workers: WorkerAdapters<'a>,
    /// Starts the headless review and triage (ADR-0027, ADR-0024).
    pub reviewer: &'a dyn AgentProvider,
    /// Starts the headless jobs whose role `[roles.<role>]` puts on Codex
    /// (ADR-t1063-1); `None` when this supervisor found no Codex that runs.
    pub codex_jobs: Option<&'a dyn AgentProvider>,
    pub spawner: &'a dyn Spawner,
    /// The queue service's tokens and socket, which the workers and the
    /// jobs are given instead of the queue's path (goal 82's stage (3)).
    pub service_access: &'a dyn super::queue_service::ServiceAccess,
    pub files: Arc<dyn RunFiles>,
    pub processes: Arc<dyn ProcessControl + Send + Sync>,
    pub generators: Generators,
    /// Writes a task's review material (`review`) and reports its path.
    /// `review.md` of a task's run, over the attempt's range when one is
    /// fixed already.
    pub review_material:
        &'a dyn Fn(TaskId, Option<&crate::application::review::ReviewRange>) -> Result<Value>,
    /// The log of this start, given the registration's `started_at`.
    /// The 1-minute load average recorded with a failed cmux call, at a
    /// claim and over each interval of a run.
    pub load_average: fn() -> Option<f64>,
    /// The free bytes of the file system of a path (task 377).
    pub free_space: fn(&Path) -> Option<u64>,
    /// The directories Claude Code keeps the sessions' scratchpads under,
    /// read at each cleanup (task 1100).
    pub scratchpad_roots: ScratchpadRoots,
    /// The versions of Claude Code (given `--claude`) and of the host's
    /// `rustc` (run in the given checkout; none without one) a claim
    /// records (task 197).
    pub host_versions: fn(&Path, Option<&Path>, Option<&Path>) -> HostVersions,
    /// The hash of the plugin a Claude worker session started in the
    /// given checkout loads (goal 113), or
    /// [`NO_PLUGIN`](crate::domain::instructions::NO_PLUGIN) /
    /// [`UNKNOWN`](crate::domain::instructions::UNKNOWN).
    pub worker_plugin: Arc<dyn Fn(&Path) -> String + Send + Sync>,
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
    /// `[provider_fallback]` of the main checkout's `dagq.toml` (`None`
    /// for no file), read again each pass (ADR-t1857-1); `None` reads
    /// nothing.
    pub provider_fallback_file: Option<ProviderFallbackFile>,
    /// Records the forecast snapshots (ADR-0070 decision 3); `None`
    /// records none.
    pub forecasts: Option<ForecastPort>,
    /// Looks for a new release on crates.io (ADR-t618-1); `None` looks
    /// for none.
    pub release: Option<ReleasePort>,
    /// Records the host's load under `<queue dir>/host/` (task 516);
    /// `None` records none.
    pub host_metrics: Option<HostMetricsPort>,
    /// Keeps the queue's service running (ADR-t1233-4 decision 2); `None`
    /// keeps none and holds nothing for it (a `--once` pass, the tests).
    pub queue_service: Option<QueueServicePort>,
    /// Runs the e2e of a run that needs it after its review (ADR-t1233-2);
    /// `None` when the repository has no e2e the runtime knows, and such a
    /// run lands without one (`not_configured`).
    pub run_e2e: Option<RunE2ePort>,
    /// Looks at and starts the host's sccache server when `[run.env]`'s
    /// `RUSTC_WRAPPER` is sccache (ADR-t1215-1); `None` looks at none.
    pub sccache: Option<SccachePort>,
    /// Watches the landing branch's CI when `[ci_watch]` is set
    /// (ADR-t1920-1); `None` watches nothing.
    pub ci_watch: Option<CiWatchPort>,
    pub layout: Layout,
}

/// Lists the directories Claude Code keeps the sessions' scratchpads
/// under (task 1100).
pub type ScratchpadRoots = Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync>;

/// Reads `[conflicts]` of the main checkout's `dagq.toml` (ADR-0080).
pub type ConflictsFile =
    Arc<dyn Fn() -> Result<Option<crate::domain::stats::ConflictConfig>> + Send + Sync>;

/// Reads `[supervisor]` of the main checkout's `dagq.toml` (task 698).
pub type SupervisorFile = Arc<dyn Fn() -> Result<Option<SupervisorConfig>> + Send + Sync>;

/// Reads `[provider_fallback]` of the main checkout's `dagq.toml`
/// (ADR-t1857-1).
pub type ProviderFallbackFile =
    Arc<dyn Fn() -> Result<Option<crate::domain::provider_switch::ProviderFallback>> + Send + Sync>;

/// Spawn a thread that reports its `tracing` events to the subscriber of
/// the spawning thread, so a supervisor run under a scoped subscriber (the
/// tests) keeps the events of its heartbeat, validations and landings.
pub fn spawn_traced<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> thread::JoinHandle<T> {
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    thread::spawn(move || tracing::dispatcher::with_default(&dispatch, work))
}

/// How often a process heartbeats its registration and leases. The
/// handoff's watch waits for the new supervisor's next heartbeat, so tests
/// shorten it (`supervise --heartbeat-interval-ms`, task 1048).
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

/// How a [`Heartbeat`] writes and when it gives up (task 1119).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatPolicy {
    /// The time between two writes.
    pub interval: Duration,
    /// The age at which the leases the heartbeat keeps go stale, so another
    /// supervisor may take its runs over (`HEARTBEAT_TIMEOUT_SECS`).
    pub stale_after: Duration,
    /// How long one write may wait for another connection's lock
    /// ([`super::QUEUE_BUSY_TIMEOUT`]).
    pub attempt: Duration,
    /// Whether the token is a resident supervisor's registration, whose row
    /// must be there at every write.
    pub registered: bool,
}

impl HeartbeatPolicy {
    /// A resident supervisor's: its registration and its leases.
    pub fn supervisor(interval: Duration) -> Self {
        Self {
            interval,
            stale_after: Duration::from_secs(HEARTBEAT_TIMEOUT_SECS.unsigned_abs()),
            attempt: super::QUEUE_BUSY_TIMEOUT,
            registered: true,
        }
    }

    /// A process with leases and no registration (`integrate`).
    pub fn leases(interval: Duration) -> Self {
        Self {
            registered: false,
            ..Self::supervisor(interval)
        }
    }

    /// Whether a write that failed as busy, `since` the last one written
    /// (the failed write's wait included), is tried again: only while the
    /// next write, waiting out the interval and the lock, still lands before
    /// the leases go stale.
    pub fn retries(&self, busy: bool, since: Duration) -> bool {
        busy && since + self.interval + self.attempt < self.stale_after
    }

    /// Why a write that went through shows the process no longer holds what
    /// it heartbeats, if it does: its registration is gone (removed as
    /// stale by `up` or `down --force`, so its runs are another's to take).
    pub fn lost(&self, write: &super::HeartbeatWrite) -> Option<&'static str> {
        (self.registered && !write.registered)
            .then_some("the supervisor's registration is gone from the queue")
    }
}

/// Writes that failed as busy since the last one written.
struct BusyStreak {
    failures: u32,
    since: Instant,
    error: String,
}

/// One process heartbeats its registration (a resident supervisor) and every
/// lease it holds with a single token, every `interval`. A write that meets
/// another connection's lock is tried again at the next interval, and only a
/// lost registration, a failure that does not pass or busy writes that would
/// let the leases go stale stop it (task 1119).
pub struct Heartbeat {
    stop: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
    failed: Arc<std::sync::Mutex<Option<String>>>,
}

impl Heartbeat {
    pub fn start(queues: Arc<dyn QueueOpener>, token: LeaseToken, policy: HeartbeatPolicy) -> Self {
        let (stop, recv) = mpsc::channel();
        let failed = Arc::new(std::sync::Mutex::new(None));
        let flag = failed.clone();
        let worker = spawn_traced(move || {
            if let Err(error) = beat(&*queues, &token, policy, &recv) {
                error!(error = %format_args!("{error:#}"), "supervisor heartbeat failed: {error:#}");
                *flag.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("{error:#}"));
            }
        });
        Self {
            stop,
            worker: Some(worker),
            failed,
        }
    }

    /// Stop writing, before the process removes its registration: a beat
    /// after it would find the row gone and report a lost lease.
    pub fn stop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    pub fn check(&self) -> Result<()> {
        match &*self.failed.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(error) => {
                bail!("supervisor heartbeat failed; preserving runs for inspection: {error}")
            }
            None => Ok(()),
        }
    }
}

/// The heartbeat's loop: a write every interval until `stop` says so, or an
/// error once the process can no longer keep what it holds.
fn beat(
    queues: &dyn QueueOpener,
    token: &LeaseToken,
    policy: HeartbeatPolicy,
    stop: &mpsc::Receiver<()>,
) -> Result<()> {
    let mut queue: Option<Box<dyn Queue + Send>> = None;
    let mut written = Instant::now();
    let mut streak: Option<BusyStreak> = None;
    loop {
        let started = Instant::now();
        let attempt = match &mut queue {
            Some(queue) => queue.heartbeat(token),
            None => queues
                .open()
                .and_then(|opened| queue.insert(opened).heartbeat(token)),
        };
        match attempt {
            Ok(write) => {
                if let Some(lost) = policy.lost(&write) {
                    bail!("{lost}");
                }
                written = started;
                if let Some(streak) = streak.take() {
                    let secs = streak.since.elapsed().as_secs_f64();
                    info!(
                        "supervisor heartbeat written again after {} busy failures over {secs:.1}s",
                        streak.failures
                    );
                    let payload = json!({
                        "token": token,
                        "failures": streak.failures,
                        "secs": (secs * 10.0).round() / 10.0,
                        "error": streak.error,
                    });
                    if let Some(queue) = &queue
                        && let Err(error) =
                            queue.record_queue_event(EventKind::SupervisorHeartbeatRetried, payload)
                    {
                        warn!(error = %format_args!("{error:#}"), "the heartbeat's retries could not be recorded: {error:#}");
                    }
                }
            }
            Err(error) => {
                // From the last write to this failure, the attempt's wait
                // for the lock included.
                let since = written.elapsed();
                let streak = streak.get_or_insert_with(|| BusyStreak {
                    failures: 0,
                    since: started,
                    error: String::new(),
                });
                streak.failures += 1;
                streak.error = format!("{error:#}");
                if !policy.retries(super::QueueBusy::is(&error), since) {
                    return Err(error.context(format!(
                        "{} heartbeat writes failed over {:.1}s since the last one written",
                        streak.failures,
                        since.as_secs_f64()
                    )));
                }
                warn!(
                    error = %format_args!("{error:#}"),
                    "supervisor heartbeat busy ({} failures, {:.1}s since the last write); trying again: {error:#}",
                    streak.failures,
                    since.as_secs_f64()
                );
            }
        }
        match stop.recv_timeout(policy.interval) {
            Err(mpsc::RecvTimeoutError::Timeout) => (),
            _ => return Ok(()),
        }
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.stop();
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
    // No cmux is pinged: the supervisor calls none (ADR-t1433-1); the
    // backend it holds starts and stops the background wrappers. The idle
    // markers and the jobs' failures are read with the headless Claude
    // adapter's signals.
    let claude = ports
        .workers
        .get(Worker::CLAUDE_HEADLESS)
        .context("no adapters for the headless Claude worker")?;
    for worker in ports.workers.workers() {
        if settings.no_claude && worker.provider == crate::domain::Provider::Claude {
            continue;
        }
        if let Some(adapter) = ports.workers.get(worker) {
            adapter.agent.preflight()?;
        }
    }
    let mut queue = ports.queues.open()?;
    for registration in queue.supervisors()? {
        if ports.processes.alive(registration.pid) {
            ensure!(
                registration.claude_disabled() == settings.no_claude,
                "the live supervisor has a different --no-claude policy; drain it before switching"
            );
        }
    }
    if settings.no_claude {
        for run in queue.latest_runs_in_progress()? {
            if run.actual_provider() == crate::domain::Provider::Claude {
                ensure!(
                    !queue
                        .processes(run.id())?
                        .iter()
                        .any(|p| p.exited_at.is_none() && ports.processes.alive(p.pid)),
                    "Claude run {} is still alive; drain it before --no-claude",
                    run.id()
                );
            }
        }
    }

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
            // The limits this process resolved, over the ones the
            // registration had before the exec.
            queue.set_slot_limits(token, settings.limits)?;
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
            // to `status` from its first second, runs or not, with the
            // limits in use and where each comes from (ADR-0062 decision 7,
            // task 698) in the same write.
            queue.register_supervisor(&token, pid, settings.limits, &layout.version)?;
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
    // For the time of the next claim `status` shows (ADR-t1479-1).
    queue.set_max_load(&token, settings.max_load)?;
    queue.set_supervisor_providers(&token, &layout.providers)?;
    let mut config = serde_json::to_value(settings.stall)?;
    config["supervisor"] = json!(token);
    queue.record_queue_event(EventKind::StallConfigLoaded, config)?;
    // The mark of this start or handoff (ADR-0051 decision 10): the mode
    // `up` passed, else the registration's (a handoff keeps it).
    let registration = queue
        .supervisors()?
        .into_iter()
        .find(|registration| registration.token == token);
    queue.record_queue_event(
        EventKind::SupervisorStarted,
        json!({
            "supervisor": token,
            "dagq_version": layout.version,
            "parallel": parallel,
            "mode": settings
                .mode
                .or(registration.as_ref().and_then(|r| r.mode))
                .map(|mode| mode.as_str()),
            "auto_update": registration.as_ref().is_some_and(|r| r.auto_update),
            "no_claude": settings.no_claude,
            "handoff": settings.handoff_token.is_some(),
            "previous_version": previous_version,
        }),
    )?;
    let heartbeat = Heartbeat::start(
        ports.queues.clone(),
        token.clone(),
        HeartbeatPolicy::supervisor(settings.heartbeat_interval),
    );
    let sessions = RecordingSessions::over(
        ports.sessions,
        ports.queues.clone(),
        Some(token.clone()),
        ports.load_average,
    )
    .stopping_left_turns(ports.processes.clone());
    let mut supervisor = Supervisor {
        no_claude: settings.no_claude,
        queue,
        queues: ports.queues.clone(),
        layout,
        repository: ports.repository.clone(),
        remote: ports.remote.clone(),
        verifier: ports.verifier.clone(),
        sessions: &sessions,
        reviewer: ports.reviewer,
        codex_jobs: ports.codex_jobs,
        signals: claude.signals,
        workers: ports
            .workers
            .workers()
            .into_iter()
            .filter(|w| !settings.no_claude || w.provider != crate::domain::Provider::Claude)
            .collect(),
        spawner: ports.spawner,
        service_access: ports.service_access,
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
        light_changes: settings.light_changes.clone(),
        fallback: settings.provider_fallback,
        fallback_file: ports.provider_fallback_file.clone(),
        fallback_error: None,
        finished: Vec::new(),
        errors: Vec::new(),
        claiming: true,
        provisioning_error: None,
        last_sweep: None,
        last_turns: None,
        process_sample: None,
        live_job_ends: Vec::new(),
        sweep_failures: Vec::new(),
        triaged: Vec::new(),
        generators: ports.generators.clone(),
        stall: settings.stall,
        wrapper_setting_warned: false,
        route_setting_warned: std::sync::atomic::AtomicBool::new(false),
        conflicts: settings.conflicts,
        conflicts_file: ports.conflicts_file.clone(),
        conflicts_error: settings.conflicts_error.clone(),
        plan_review: None,
        goal_review: None,
        job_ends: JobEnds::default(),
        jobs_swept: false,
        planner_exits: Vec::new(),
        handoff: None,
        exec: None,
        run_env_missing: false,
        landing_unresolved: false,
        landing_recorded: None,
        landing_stamp: None,
        queue_hold: None,
        provider_holds: Vec::new(),
        timer_finishes_held: Vec::new(),
        moved: HashMap::new(),
        hold_continue: HashMap::new(),
        reopens: HashMap::new(),
        notice_failures: HashMap::new(),
        draining: false,
        stop_recorded: false,
        utc_offset: settings.utc_offset,
        rechecks: recheck::Rechecks::default(),
        max_load: settings.max_load,
        spaced_since: None,
        load_average: ports.load_average,
        host_versions: ports.host_versions,
        worker_plugin: ports.worker_plugin.clone(),
        max_improvement_proposals: ports.max_improvement_proposals.clone(),
        loads: HashMap::new(),
        defer: claim_defer::DeferWatch::default(),
        resume_config: settings.resume,
        retry_unreadable_review: settings.retry_unreadable_review,
        run_e2e: ports.run_e2e.clone(),
        e2e: e2e::E2eWaits::default(),

        observation: contexts::ObservationState::new(ports),
        host: contexts::HostOpsState::new(ports, settings),
    };
    // Before any job starts again: the jobs a gone supervisor left, and
    // after an exec those the previous binary of this process started.
    supervisor.tend_headless_jobs();
    if settings.handoff_token.is_some() {
        // A review rebuilt under an open hold ask waits for it (task 437).
        // An answer waits for the rebuilt slots (task 754).
        supervisor.check_queue_hold(false)?;
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

/// The supervisor's loop and the state of the contexts that are not split
/// off yet (docs/design/architecture.md, "`Supervisor`の状態"). Which
/// context owns each field, so that a submodule changes only its own
/// context's fields and reads or calls the others (rule C3):
///
/// - shared: `queue`, `queues`, `generators`, `layout`, `processes` and
///   `utc_offset`, the connection, clocks and places every context reads.
/// - planning: `plan_review`, `goal_review`, `planner_exits`,
///   `route_setting_warned` and `max_improvement_proposals`.
/// - execution and landing: the slots and phases (`workers`, `slots`,
///   `parallel`, `max_waiting`, `limits`, `slot_flags`, `light_changes`,
///   `spaced_since`, `finished`, `errors`, `claiming`, `provisioning_error`,
///   `triaged`, `stall`, `conflicts*`, `job_ends`, `live_job_ends`,
///   `last_turns`, `run_env_missing`, `landing_*`,
///   `run_e2e`, `e2e`, `queue_hold`, `provider_holds`,
///   `timer_finishes_held`, `fallback*`, `moved`, `hold_continue`,
///   `reopens`, `notice_failures`, `rechecks`, `defer`, `loads`,
///   `resume_config`, `retry_unreadable_review`, `review_material`,
///   `wrapper_setting_warned`) and the adapters it uses (`repository`,
///   `remote`, `verifier`, `reviewer`, `codex_jobs`, `signals`, `spawner`,
///   `files`).
/// - observation and analysis: `observation`
///   ([`contexts::ObservationState`]), changed only by its submodules
///   through their passes.
/// - host operation: `host` ([`contexts::HostOpsState`]), changed only by
///   its submodules through their passes; and on the loop, the
///   registration and handoff (`token`, `heartbeat`, `supervisor_file`,
///   `supervisor_error`, `exec`, `handoff`, `draining`, `stop_recorded`),
///   the sweep (`last_sweep`, `sweep_failures`), the host's load
///   (`max_load`, `load_average`, `host_versions`), `service_access`,
///   `no_claude` and `sessions`.
struct Supervisor<'a> {
    no_claude: bool,
    queue: Box<dyn Queue + Send>,
    /// A connection for each thread beside the loop.
    queues: Arc<dyn QueueOpener>,
    layout: &'a Layout,
    repository: Arc<dyn Repository + Send + Sync>,
    remote: Arc<dyn MainRemote + Send + Sync>,
    verifier: Arc<dyn Verifier + Send + Sync>,
    sessions: &'a dyn SessionWrappers,
    /// Starts the headless review of accepted runs (ADR-0027).
    reviewer: &'a dyn AgentProvider,
    /// Starts the headless jobs a role puts on Codex (ADR-t1063-1).
    codex_jobs: Option<&'a dyn AgentProvider>,
    /// Reads the idle marker of the run sessions, and the screen of the
    /// inbox (no worker screen since task 1437, no planner's screen since
    /// ADR-t1433-2).
    signals: &'a dyn AgentSignals,
    /// The workers this supervisor runs: a candidate whose worker is not
    /// one of them is not claimed (ADR-t813-2).
    workers: Vec<Worker>,
    spawner: &'a dyn Spawner,
    service_access: &'a dyn super::queue_service::ServiceAccess,
    files: Arc<dyn RunFiles>,
    processes: Arc<dyn ProcessControl + Send + Sync>,
    review_material:
        &'a dyn Fn(TaskId, Option<&crate::application::review::ReviewRange>) -> Result<Value>,
    token: LeaseToken,
    heartbeat: Heartbeat,
    slots: Vec<Slot>,
    /// `--parallel`: the slots in use (the runs not waiting) are held under
    /// it, apart from a run a person moved (ADR-0062 decision 10).
    parallel: usize,
    /// `--max-waiting` (ADR-0062 decision 7).
    max_waiting: usize,
    /// `parallel`, `max_waiting` and `runtime_planners` with where each
    /// comes from, as last resolved (task 698, task 941).
    limits: SlotLimits,
    /// The flags given; a value not given follows `[supervisor]`.
    slot_flags: SlotFlags,
    /// Reads `[supervisor]` again each pass; `None` keeps `limits`.
    supervisor_file: Option<SupervisorFile>,
    /// The error the last read of `[supervisor]` failed with, warned of
    /// once until it changes or a read succeeds.
    supervisor_error: Option<String>,
    /// `[supervisor] light_changes` as last read (ADR-t1591-1): the tasks
    /// claimed in the room the landing queue leaves.
    light_changes: crate::domain::light_slots::LightChanges,
    /// `[provider_fallback]` as last read (ADR-t1857-1): whether a worker
    /// moves off a provider it cannot use.
    fallback: crate::domain::provider_switch::ProviderFallback,
    /// Reads `[provider_fallback]` again each pass; `None` keeps
    /// `fallback`.
    fallback_file: Option<ProviderFallbackFile>,
    /// The error the last read of `[provider_fallback]` failed with,
    /// warned of once until it changes or a read succeeds.
    fallback_error: Option<String>,
    finished: Vec<TaskRun>,
    errors: Vec<RunError>,
    /// Cleared after a provisioning failure so an unavailable cmux or Git
    /// does not burn through every candidate.
    claiming: bool,
    provisioning_error: Option<String>,
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
    /// The ends of the live sessions' recovery jobs whose verdict a person
    /// is asked about (run, alert, attempt), until the escalation records
    /// its `recovery_finished` ([`recovery::Escalation::record`]).
    live_job_ends: Vec<(RunId, RecoveryAlert, usize, recovery::JobEnd)>,
    /// The workspaces the sweep could not close: retried on every sweep,
    /// their `cleanup_failed` recorded once per process.
    sweep_failures: Vec<String>,
    /// The runs this process triaged, with where each one went.
    triaged: Vec<Value>,
    /// The clock and IDs `queue` also uses.
    generators: Generators,
    /// The host's time zone ([`LoopSettings::utc_offset`]): the local day
    /// the e2e marks of a worker's prompt are judged on.
    utc_offset: fn(i64) -> i64,
    /// The thresholds of the stalled-session checks (ADR-0043 decision 4).
    stall: StallConfig,
    /// Whether `[headless] wrapper = "workspace"`, which a worker ignores,
    /// was warned of (ADR-t1433-3 decision 2).
    wrapper_setting_warned: bool,
    /// Whether `[roles.runtime_planner] route`, which the runtime's planners
    /// ignore, was warned of (ADR-t1433-2 decision 3). Set where a planner
    /// opens, which reads `[roles]` through `&self`.
    route_setting_warned: std::sync::atomic::AtomicBool,
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
    /// The runtime's planners this process asked to exit (their exit
    /// request), and when.
    planner_exits: Vec<(crate::domain::PlannerId, Instant)>,
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
    /// The landing branch did not resolve at the top of this pass
    /// (ADR-t615-1): nothing is claimed and no passed run lands until it
    /// does.
    landing_unresolved: bool,
    /// The reason of the landing branch's hold this process last recorded
    /// or found recorded (`Some(None)`: none); `None` before its first look
    /// at the queue, or after a record that failed.
    landing_recorded: Option<Option<&'static str>>,
    /// The stamp of the landing branch's inputs taken before its last
    /// resolution, and when (task 1078): a pass whose stamp is the same,
    /// within [`LANDING_BRANCH_RECHECK`], keeps that resolution without
    /// starting Git.
    landing_stamp: Option<(crate::application::LandingBranchStamp, Instant)>,
    /// The open authentication or usage-limit ask read at the top of this
    /// pass (task 437): no new run is claimed and no headless job starts
    /// while it holds.
    queue_hold: Option<claim_hold::QueueHold>,
    /// The providers held for the workers without an ask (ADR-t813-2
    /// decision 6): Codex for any reason, Claude for an agent that did not
    /// start. Their tasks run on the other provider meanwhile.
    provider_holds: Vec<crate::domain::provider_switch::ProviderHold>,
    /// The latest finishes of jobs on 観測と分析's timer whose provider
    /// this supervisor held, so that none is held twice
    /// ([`crate::domain::throughput_review::finishes_to_hold`]).
    timer_finishes_held: Vec<EventId>,
    /// The runs whose worker moved to the other provider in this step: the
    /// slot's copy takes the new worker once the step returns.
    moved: HashMap<RunId, Worker>,
    /// The held runs whose session gets the fixed text to go on, with the
    /// ask a person answered `done` (task 437).
    hold_continue: HashMap<RunId, AskId>,
    /// The headless sessions lost during a wait that are opened again
    /// (task 1372), by run.
    reopens: HashMap<RunId, reopen::ReopenWatch>,
    /// The notices of a question closed without its answer that could not
    /// be sent, by run: tried again a bounded number of times (task 1372).
    notice_failures: HashMap<RunId, stall::NoticeFailure>,
    /// This pass drains (a stop, a handoff, or claiming stopped after a
    /// provisioning failure): nothing may wait for the program to appear.
    draining: bool,
    /// Whether this process recorded `supervisor_draining` for its stop
    /// request (task 1277): once, on the first pass that saw it.
    stop_recorded: bool,
    /// The landing recheck running and the one due (ADR-0068).
    rechecks: recheck::Rechecks,
    /// `--max-load` (task 327).
    max_load: Option<f64>,
    /// Since when, in Unix milliseconds, a claim this process would make
    /// waits for the spacing after the queue's latest claim (ADR-t1479-1);
    /// `None` while none waits.
    spaced_since: Option<i64>,
    /// The 1-minute load average, and the host's versions a claim records.
    load_average: fn() -> Option<f64>,
    host_versions: fn(&Path, Option<&Path>, Option<&Path>) -> HostVersions,
    worker_plugin: Arc<dyn Fn(&Path) -> String + Send + Sync>,
    /// Reads the limit on the improvement proposals running.
    max_improvement_proposals: Arc<dyn Fn() -> Result<usize> + Send + Sync>,
    /// The load samples of each held run's current interval (task 197).
    loads: HashMap<RunId, LoadWindow>,
    /// The claims deferred on conflict hotspots (ADR-0069).
    defer: claim_defer::DeferWatch,
    /// `[resume]`: the limit of a run's conflict-only attempts (ADR-0047
    /// decision 24).
    resume_config: ResumeConfig,
    retry_unreadable_review: bool,
    /// Runs the e2e of the runs after their review (ADR-t1233-2).
    run_e2e: Option<RunE2ePort>,
    /// The runs waiting for the e2e, and when one that could not run is
    /// tried again.
    e2e: e2e::E2eWaits,
    /// 観測と分析's state.
    observation: contexts::ObservationState,
    /// host運用's state.
    host: contexts::HostOpsState,
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
    /// In `AwaitingSlot`, the run's review passed and its e2e is done, and
    /// it waits only for its landing turn (ADR-t1591-1): its last look
    /// found another run landing.
    landing_turn: bool,
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
    /// stays open, and the review starts once nothing holds. `retried` is
    /// whether the review that waits is the one retry of the review
    /// ([`ReviewWatch::retried`]): it stays one across the wait.
    ReviewHeld {
        session: Option<SessionRef>,
        retried: bool,
    },
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
    /// A passed run waits for its e2e to start (ADR-t1233-2): another
    /// run's e2e runs, one that could not run waits to be tried again, or a
    /// handoff starts none. Unlike `AwaitingSlot` nothing is about to start,
    /// so a handoff need not wait for it: the next process rebuilds the run
    /// as an adopted one, which waits for the slot again.
    AwaitingE2e,
    /// The e2e of a passed run runs on the host off the loop before it
    /// lands (ADR-t1233-2); the run keeps its lease and slot.
    E2e(e2e::E2eWatch),
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
        /// The review job's recommendation, confidence and reason the ask
        /// carries (ADR-t451-1 decision 3): those of a `concern` it did not
        /// apply. A `discard` asks for that reason rather than `scope`.
        recommendation: Option<crate::domain::concern::LandingRecommendation>,
        confidence: Option<crate::domain::AskConfidence>,
        reason_category: Option<crate::domain::concern::ConcernReason>,
        /// The review job whose verdict this ask acts on (a `concern`, a
        /// `revise` that cannot be sent back, the conflict a `pass` ran
        /// into): the ask's events record it as `requested_by` (task 798).
        /// `None` when the ask follows what the live session did after
        /// the verdict was acted on (a revise or a conflict request it
        /// did not carry out), which is the supervisor's own step, or when
        /// an adopter cannot read the job's attempt from the history.
        requested_by: Option<ActorContext>,
        /// A `send_back` the runtime applied on a `concern` that the
        /// session did not fix: the ask records the escalation
        /// (`concern_send_back_escalated`, task 1392).
        sent_back: Option<SentBackEscalation>,
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
        /// What the job's output said of its session (Codex's thread and
        /// model, ADR-t1063-1 decision 6; the job's tokens, ADR-t1486-1);
        /// `None` when no job ran.
        session: Option<crate::domain::headless_job::JobSession>,
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
        self.role_models(role).launch(role)
    }

    /// `[roles.*]` of `dagq.toml` as read now, or none (every role started
    /// as before) when it cannot be read, warned of for `role`.
    pub(super) fn role_models(
        &self,
        role: crate::domain::actor_model::ModelRole,
    ) -> crate::domain::actor_model::RoleModels {
        self.verifier.role_models().unwrap_or_else(|error| {
            warn!(error = %format_args!("{error:#}"), "[roles.{}] could not be read; starting it as before: {error:#}", role.as_str());
            crate::domain::actor_model::RoleModels::default()
        })
    }

    /// The loop's shared parts and adapters ([`PassEnv`]), borrowed apart
    /// from 観測と分析's and host運用's states, with those states.
    fn split(
        &mut self,
    ) -> (
        PassEnv<'_>,
        &mut contexts::ObservationState,
        &mut contexts::HostOpsState,
    ) {
        (
            PassEnv {
                queue: &mut *self.queue,
                queues: &self.queues,
                generators: &self.generators,
                layout: self.layout,
                processes: &self.processes,
                files: &self.files,
                repository: &self.repository,
                verifier: &self.verifier,
                spawner: self.spawner,
                token: &self.token,
            },
            &mut self.observation,
            &mut self.host,
        )
    }

    /// Run `pass` of 観測と分析 on its state.
    fn on_observation<R>(
        &mut self,
        pass: impl FnOnce(&mut contexts::ObservationState, &mut PassEnv<'_>) -> R,
    ) -> R {
        let (mut env, observation, _) = self.split();
        pass(observation, &mut env)
    }

    /// Run `pass` of host運用 on its state.
    fn on_host<R>(
        &mut self,
        pass: impl FnOnce(&mut contexts::HostOpsState, &mut PassEnv<'_>) -> R,
    ) -> R {
        let (mut env, _, host) = self.split();
        pass(host, &mut env)
    }

    /// Run `call` with the loop's shared parts only.
    fn with_env<R>(&mut self, call: impl FnOnce(&mut PassEnv<'_>) -> R) -> R {
        let (mut env, _, _) = self.split();
        call(&mut env)
    }

    /// The runs the slots hold, which a cleanup leaves alone.
    fn held_runs(&self) -> Vec<RunId> {
        self.slots
            .iter()
            .map(|slot| slot.run.id().clone())
            .collect()
    }

    /// Ask host運用 for the worktrees of ended runs to be cleaned, every
    /// such run's or only `task`'s ([`contexts::HostOpsState::request_cleanup`]).
    pub(super) fn request_cleanup(&mut self, task: Option<TaskId>) {
        let held = self.held_runs();
        self.on_host(|host, env| host.request_cleanup(env, &held, task, None));
    }

    /// A handoff withdrawn while this process drained: the cleanup goes
    /// back to normal ([`contexts::HostOpsState::resume_cleanup`]).
    fn resume_cleanup(&mut self) {
        let held = self.held_runs();
        self.on_host(|host, env| host.resume_cleanup(env, &held));
    }

    /// host運用's look at the sccache server before a process given
    /// `[run.env]` starts ([`contexts::HostOpsState::ensure_sccache`]).
    pub(super) fn ensure_sccache(
        &mut self,
        reason: crate::domain::sccache::CheckReason,
    ) -> crate::domain::sccache::ServerCheck {
        self.on_host(|host, env| host.ensure_sccache(env, reason))
    }

    /// The look and the guard in `dir` before a process given `[run.env]`
    /// starts ([`contexts::HostOpsState::sccache_look`]).
    pub(super) fn sccache_look(
        &mut self,
        reason: crate::domain::sccache::CheckReason,
        dir: &Path,
    ) -> crate::domain::sccache::GuardLook {
        self.on_host(|host, env| host.sccache_look(env, reason, dir))
    }

    /// Record `sccache_wrapper_removed` on `run` ([`sccache::record_wrapper_removed`]).
    pub(super) fn record_wrapper_removed(
        &mut self,
        run: &RunId,
        look: &crate::domain::sccache::GuardLook,
        fields: Value,
    ) {
        self.with_env(|env| sccache::record_wrapper_removed(env, run, look, fields));
    }

    /// The handoff request this process drains for was replaced by one for
    /// `now`: the drain goes on for the new binary (task 1286).
    fn replace_handoff(&mut self, binary: &str, now: String) {
        info!(
            "supervisor {} handoff to {binary} was replaced by a handoff to {now}: no new work starts; it execs {now} once the validations and landings in progress are done",
            self.token
        );
        self.handoff = Some(now);
    }

    /// Drive the loop, then remove this process's registration: it is about
    /// to exit, whether it drained its runs, ran out of work, or failed on
    /// a claim or provisioning. Only a heartbeat failure keeps the row (the
    /// database may be unreachable), and it goes stale with the leases.
    fn run_loop(&mut self, options: &LoopSettings) -> Result<Value> {
        let result = self.drive(options);
        self.host.host_metrics.finish();
        // The jobs the loop stopped last (a handoff stops them all).
        self.write_job_ends();
        // A loop that ended on an error lets the cleanup job end after its
        // current worktree (all candidates for disk space), recording its work.
        if result.is_err() {
            let held = self.held_runs();
            self.on_host(|host, env| {
                host.poll_cleanup(env, &held, true);
                host.finish_cleanup(env, &held);
            });
        }
        if self.exec.is_none() {
            // Only a loop that ended on an error leaves one running.
            self.on_observation(|observation, env| {
                observation.stop_observer(env, "with the supervisor");
                observation.stop_throughput_review(env, "with the supervisor");
            });
        }
        if self.exec.is_none() && self.heartbeat.check().is_ok() {
            // The mark of the stop (ADR-0051 decision 10); an exec leaves it
            // to the next process's handoff mark.
            let stopped = json!({
                "supervisor": self.token,
                "dagq_version": self.layout.version,
                "outcome": if result.is_ok() { "stopped" } else { "failed" },
            });
            if let Err(error) = self
                .queue
                .record_queue_event(EventKind::SupervisorStopped, stopped)
            {
                warn!(error = %format_args!("{error:#}"), "the supervisor's stop could not be recorded: {error:#}");
            }
            self.heartbeat.stop();
            if let Err(error) = self.queue.deregister_supervisor(&self.token) {
                warn!(error = %format_args!("{error:#}"), "supervisor registration could not be removed: {error:#}");
            }
            // After the deregistration, so the last of the supervisors a
            // `down` stops sees no other one left.
            if result.is_ok() && options.stop.load(Ordering::SeqCst) {
                // The queue service after the supervisor (ADR-t1233-4
                // decision 1).
                self.on_host(|host, env| host.stop_queue_service_after_down(env));
            }
        }
        result
    }
    /// Record, once, that this process drains for a stop request (SIGINT /
    /// SIGTERM, from `down`, the drain of `up` or `install
    /// --allow-breaking`, or launchd's bootout) (task 1277): a handoff
    /// waiting for it fails at once instead of at its timeout, and the
    /// observer reads the stop behind the claims and resumes it holds.
    /// A failed write is only logged and tried again on the next pass.
    fn record_stop_request(&mut self) {
        let handoff = match &self.handoff {
            Some(binary) => Some(binary.clone()),
            None => self.queue.handoff_request(&self.token).ok().flatten(),
        };
        let runs: Vec<&RunId> = self.slots.iter().map(|slot| slot.run.id()).collect();
        let payload = json!({
            "supervisor": self.token,
            "pid": std::process::id(),
            "build": self.layout.version,
            "reason": "stop_requested",
            "handoff_binary": handoff,
            "runs": runs,
        });
        match self
            .queue
            .record_queue_event(EventKind::SupervisorDraining, payload)
        {
            Ok(_) => {
                self.stop_recorded = true;
                info!(
                    "supervisor {} asked to stop: it drains the runs in progress; a stop wins over any handoff",
                    self.token
                );
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the supervisor's stop request could not be recorded: {error:#}");
            }
        }
    }
    fn drive(&mut self, options: &LoopSettings) -> Result<Value> {
        loop {
            options.passes.fetch_add(1, Ordering::SeqCst);
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
            if stopping && !self.stop_recorded {
                self.record_stop_request();
            }
            // Every pass, draining or handing off too (task 516).
            let handing_off = self.handoff.is_some();
            let (host, generators) = (&mut self.host, &self.generators);
            host.host_metrics.pass(
                host.host_metrics_port.as_ref(),
                handing_off,
                || generators.clock.now(),
                Instant::now(),
            );
            // Before any job starts: none runs twice (task 443).
            self.tend_headless_jobs();
            // What the cleanup job removed is recorded before the disk is
            // read (task 405).
            let ending = stopping || self.handoff.is_some();
            let held = self.held_runs();
            self.on_host(|host, env| host.poll_cleanup(env, &held, ending));
            // Every pass, draining or not, so a hold on landings ends as soon
            // as the program is found (ADR-0049 decision 9).
            self.check_run_env_programs()?;
            // And the sccache server, which no process given [run.env]
            // may start (ADR-t1215-1, ADR-t2086-1).
            self.on_host(|host, env| host.sccache_pass(env));
            self.check_landing_branch(options.landing_recheck);
            self.mark_run_env_change()?;
            // Every pass, before any claim: a change of `[conflicts]` takes
            // effect without a restart (ADR-0080).
            self.reread_conflicts()?;
            // And `[supervisor]`: a change of `parallel`, `max_waiting` or
            // `runtime_planners` takes effect without a restart (task 698,
            // task 941).
            self.reread_slot_limits()?;
            // And `[provider_fallback]`: turning the workers' fallback on
            // or off takes effect without a restart (ADR-t1857-1).
            self.reread_provider_fallback();
            // And `[ci_watch]`, whose check is reaped and started off the
            // loop, draining and handing off too (ADR-t1920-1).
            self.on_observation(|observation, env| observation.ci_watch_pass(env));
            // Every pass too, so a hold on landings ends as soon as there
            // is room (task 377).
            let landings: Vec<RunId> = self
                .slots
                .iter()
                .filter(|slot| matches!(slot.phase, Phase::AwaitingSlot))
                .map(|slot| slot.run.id().clone())
                .collect();
            let held = self.held_runs();
            self.on_host(|host, env| {
                host.check_disk(env, options.disk_cleanup_interval, &landings, &held)
            })?;
            // Every pass too: the answer of an authentication or usage-limit
            // ask is applied and the hold read before any work starts (task
            // 437), and Codex's hold ends once its time is up (ADR-t813-2).
            self.check_queue_hold(true)?;
            self.check_provider_holds()?;
            // A request may have been withdrawn while this process drained:
            // return to claims on this pass, without waiting for the slots
            // that had kept the handoff pending. One replaced by another
            // binary goes on draining for the new one, with no claim
            // between (task 1286).
            let mut handoff_withdrawn = false;
            if let Some(binary) = self.handoff.clone() {
                match self.queue.handoff_request(&self.token)? {
                    Some(now) if now == binary => {}
                    Some(now) => self.replace_handoff(&binary, now),
                    None => {
                        info!(
                            "supervisor {} handoff to {binary} was withdrawn; resuming normal work",
                            self.token
                        );
                        self.handoff = None;
                        handoff_withdrawn = true;
                        // The cleanup goes back to normal with the claims;
                        // a stop keeps it ending (task 1427).
                        if !stopping {
                            self.resume_cleanup();
                        }
                    }
                }
            }
            self.draining = stopping || !self.claiming || self.handoff.is_some();
            // Before any new work, draining or not: a drain waits for them
            // (ADR-0062 decision 8).
            self.return_waiting_runs();
            // A stop wins over a handoff: the drain goes on as before.
            if !stopping {
                if self.handoff.is_none() && !handoff_withdrawn {
                    self.handoff = self.queue.handoff_request(&self.token)?;
                    if let Some(binary) = &self.handoff {
                        info!(
                            "supervisor {} asked to hand off to {binary}: no new work starts; it execs once the validations and landings in progress are done",
                            self.token
                        );
                        // Read after the cleanup was polled: this pass
                        // drains on it already.
                        self.host.end_cleanup_for_handoff();
                    }
                }
                if let Some(binary) = self.handoff.clone() {
                    // A landing recheck in progress is waited for: its
                    // command would go on in the scratch worktree the next
                    // process uses (ADR-0068). None starts meanwhile (this
                    // pass drains). So is the cleanup job, which ends after
                    // its current worktree, or all candidates for disk space (task 648),
                    // and the rest of a cleanup for room it took on (task 1426).
                    self.recheck_pass();
                    if !self.rechecks.running()
                        && !self.host.handoff_waits()
                        && !self.observation.handoff_waits()
                        && self.slots.iter().all(|slot| slot.phase.rebuildable())
                    {
                        if self.queue.take_handoff(&self.token, &binary)? {
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
                        // Not taken: withdrawn, or replaced by another
                        // binary, which the next pass execs (task 1286).
                        match self.queue.handoff_request(&self.token)? {
                            Some(now) if now != binary => self.replace_handoff(&binary, now),
                            // The same binary asked again meanwhile: taken
                            // on the next pass.
                            Some(_) => {}
                            None => {
                                info!(
                                    "supervisor {} handoff to {binary} was withdrawn before exec; resuming normal work",
                                    self.token
                                );
                                self.handoff = None;
                                self.draining = !self.claiming;
                                self.resume_cleanup();
                            }
                        }
                    }
                    if self.handoff.is_some() {
                        self.draining = true;
                        self.poll_observer();
                        self.reap_throughput_reviews(options);
                        self.on_observation(|observation, env| {
                            observation.report_pass(env, false);
                            observation.forecast_pass(env, false);
                        });
                        self.on_host(|host, env| host.release_pass(env, false));
                        self.on_observation(|observation, env| observation.push_pass(env, false));
                        // A plan or goal review that ended meanwhile is
                        // reaped and its verdict applied, none started: a
                        // verdict left to the exec would be thrown away
                        // (task 1425). A task it readies waits for the
                        // next process's claim.
                        self.plan_review_pass(options, false);
                        self.goal_review_pass(false);
                        self.tick(true);
                        thread::sleep(options.tick);
                        continue;
                    }
                }
            }
            // The queue service looked at, started again or replaced
            // (ADR-t1233-4 decision 2); while it is down no new run and no
            // queue's job starts.
            let working = !stopping && self.claiming;
            self.on_host(|host, env| host.queue_service_pass(env, working));
            if self.claiming && !stopping {
                self.fill_slots(self.parallel, options.sweep_interval)?;
                self.sample_candidates(self.parallel);
            } else {
                // A draining supervisor still frees the integration slot a
                // dead landing holds: its own runs waiting to land, and so
                // the drain, would wait for it forever (task 1118).
                self.release_dead_landings()?;
            }
            self.poll_observer();
            self.record_session_turns(false);
            let rechecked = self.recheck_pass();
            // A supervisor that stopped claiming is draining, not observing
            // nor starting plan reviews, nor updating itself.
            let working = !stopping && self.claiming;
            self.on_observation(|observation, env| {
                // Reaped on every pass, started only by a supervisor at work.
                observation.report_pass(env, working);
                // Looked for by a supervisor at work only: the triggers stay
                // in the queue for the next look (ADR-0070 decision 3).
                observation.forecast_pass(env, working);
            });
            // Looked for by a supervisor at work only (ADR-t618-1).
            self.on_host(|host, env| host.release_pass(env, working));
            // A message waiting is sent while the supervisor does not stop.
            self.on_observation(|observation, env| observation.push_pass(env, !stopping));
            // An inbox without a watcher while asks wait for it is recorded
            // and told through `[push]` (ADR-t1433-5 decision 1 (3)),
            // draining or not: a drain waits for their answers. The
            // watcher's changes are recorded either way (task 1021).
            self.inbox_nudge_pass();
            // Reaped on every pass, started only by a supervisor at work.
            // Its route decides on `--no-claude` and the hold: a login or
            // usage limit that holds Claude starts no Claude review, but
            // one whose role names its provider may run on Codex
            // (ADR-t1063-1 decision 5, ADR-t1204-1, task 1220).
            self.reap_throughput_reviews(options);
            self.start_throughput_review(
                options,
                !stopping && self.claiming && self.host.service_up,
            );
            if !stopping && self.claiming {
                // Its route decides on `--no-claude` and the hold as the
                // throughput review's does (task 1223).
                if self.host.service_up {
                    self.start_observer_when_due(options);
                }
                self.on_host(|host, env| {
                    host.auto_update_pass(env, options);
                    host.release_update_pass(env, options);
                });
            }
            // A plan review that just readied tasks is followed by one more
            // pass, which claims them.
            // The plan and goal reviews decide on the hold itself: a login
            // or usage limit that holds the queue starts no Claude job (task
            // 437), but one whose role names its provider may run on Codex
            // while Claude is held (ADR-t1063-1 decision 5). One in progress
            // is followed.
            let starting = !stopping && self.claiming && self.host.service_up;
            let mut progressed = self.plan_review_pass(options, starting);
            progressed |= self.goal_review_pass(starting);
            if self.slots.is_empty() {
                // A running observer, KPI report job, plan review, landing recheck or
                // cleanup for disk space (with the rest of one another job
                // took on) or one a triage or resume waits
                // for is waited for like a run: it is
                // bounded by its own timeout, its command or its worktrees.
                // A recheck just applied is followed by one more pass, which
                // resumes the runs it parked; a cleanup for room by one
                // that claims if there is room now. Any other cleanup is
                // joined once the loop ends.
                // A message being sent is bounded by the command's timeout;
                // `--once` also waits for those still to be tried, a stop
                // does not.
                let job = self.observation.busy(options.once && !stopping)
                    || self.host.release.running()
                    || self.plan_review.is_some()
                    || self.goal_review.is_some()
                    || self.rechecks.running()
                    || self.host.cleanup.for_disk()
                    || self.host.cleanup.deferred()
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
        let held = self.held_runs();
        self.on_host(|host, env| host.finish_cleanup(env, &held));
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
        // Whatever the slots: a `land` answer only queues the run, which
        // lands below, before new claims, once there is room (task 949).
        self.apply_landing_answers()?;
        // Like a landing answer before: an error (an `integrate` that took
        // the integration slot since the check, a main that did not
        // resolve) is noted, and the run stays queued for a later pass.
        if let Err(error) = self.start_approved_landings(parallel) {
            warn!(error = %format_args!("{error:#}"), "an approved run could not start its landing: {error:#}");
        }
        // A run recovered from a landing it may not land again is reviewed
        // as one just validated (task 1118).
        if let Err(error) = self.review_recovered_runs(parallel) {
            warn!(error = %format_args!("{error:#}"), "a run recovered from its landing could not start its review: {error:#}");
        }
        self.apply_triage_answers()?;
        // Resumes and triage read the landing branch: they wait with the
        // claims until it resolves (ADR-t615-1). A resumed session gets
        // `[run.env]` like a claimed run, so it waits with the claims for a
        // missing program too (task 303).
        let resumes = if self.used_slots() < parallel
            && !self.landing_unresolved
            && !self.run_env_missing
            && !self.observation.ci.held()
        {
            self.resume_candidates()?
        } else {
            Vec::new()
        };
        let recoveries = if self.used_slots() < parallel && !self.landing_unresolved {
            self.triage_candidates()?
        } else {
            Vec::new()
        };
        // The resumes, the recovery jobs and the claims stand in one line
        // by the effective priority of their task (ADR-t1850-1); a held
        // kind only leaves the line, and holds no slot for itself. Read
        // after the candidates: a run out of resumes may have made its task
        // ready again for a claim in this pass.
        let graph = dependency_graph(self.queue.graph_input()?, None);
        let effective: HashMap<TaskId, Priority> = graph
            .tasks
            .iter()
            .map(|node| (node.id, node.effective_priority))
            .collect();
        let mut line = Vec::new();
        for candidate in resumes {
            let priority = self.line_priority(&effective, candidate.run.task_id())?;
            line.push(SlotCandidate {
                kind: SlotKind::Resume,
                priority,
                item: LineItem::Resume(Box::new(candidate)),
            });
        }
        for run in recoveries {
            let priority = self.line_priority(&effective, run.task_id())?;
            line.push(SlotCandidate {
                kind: SlotKind::Recovery,
                priority,
                item: LineItem::Recovery(Box::new(run)),
            });
        }
        // Takes no slot: only closes and frees what ended runs left.
        if let Err(error) = self.sweep_ended_runs(sweep_interval) {
            warn!(error = %format_args!("{error:#}"), "the workspaces and worktrees of ended runs could not all be swept: {error:#}");
        }
        // A wait for the claim spacing goes on only from one claim pass to
        // the next that reaches the spacing again (ADR-t1479-1): a hold or
        // a pass with no free slot or candidate ends it.
        let spaced_since = self.spaced_since.take();
        // A run claimed now would fail every cargo command (ADR-0049
        // decision 9; checked at the top of the pass); the runs in flight
        // and their reviews go on (resumes wait above). A queue service
        // that is down holds the new claims; the runs in flight go on
        // (ADR-t1233-4 decision 2).
        let mut claims = None;
        // Nor while the CI cannot be read (ADR-t1920-1 decision 2).
        if !(self.run_env_missing || self.observation.ci.held() || self.landing_unresolved)
            && self.host.service_up
        {
            // The runs in flight go on; only new claims wait (task 327).
            if self.hold_claims()? {
                // Neither provider can take a worker: the candidates are
                // deferred for it too (ADR-t813-2 decision 6).
                if self.routes().is_empty() {
                    self.claimable(&graph)?;
                }
            } else {
                // Read once per pass, so turning the trial on or off takes
                // effect without a restart; a file that cannot be read
                // claims outside it (provisioning reports the file's error).
                let trial = self.verifier.worker_trial().unwrap_or_else(|error| {
                    warn!(error = %format_args!("{error:#}"), "[worker.trial] could not be read; claiming without the trial: {error:#}");
                    WorkerTrial::default()
                });
                claims = Some(ClaimPass {
                    spaced_since,
                    host: None,
                    plugin: None,
                    trial,
                });
                // Only while there is room for a claim, as the claim loop
                // reads the candidates only then.
                if self.claim_room(parallel) != ClaimRoom::None {
                    for id in self.claimable(&graph)? {
                        let priority = self.line_priority(&effective, id)?;
                        line.push(SlotCandidate {
                            kind: SlotKind::Claim,
                            priority,
                            item: LineItem::Claim,
                        });
                    }
                }
            }
        }
        // The claims of one priority run together: the claim loop takes
        // every candidate of that priority or higher, in its own order.
        let mut claimed_down_to: Option<Priority> = None;
        for candidate in slot_order(line) {
            match candidate.item {
                // A resume and a recovery job use the normal room only.
                LineItem::Resume(resume) => {
                    if self.used_slots() < parallel {
                        self.resume_run(*resume)?;
                    }
                }
                LineItem::Recovery(run) => {
                    if self.used_slots() < parallel {
                        self.triage_run(*run)?;
                    }
                }
                LineItem::Claim => {
                    let Some(pass) = claims.as_mut() else {
                        continue;
                    };
                    if claimed_down_to.is_some_and(|floor| floor <= candidate.priority) {
                        continue;
                    }
                    claimed_down_to = Some(candidate.priority);
                    if self.claim_tasks(parallel, candidate.priority, pass)? == ClaimStep::Stop {
                        claims = None;
                    }
                }
            }
        }
        Ok(())
    }
    /// The room for the next claim (ADR-t1591-1).
    fn claim_room(&self, parallel: usize) -> ClaimRoom {
        light_slots::claim_room(
            self.used_slots(),
            self.landing_queue(),
            parallel,
            self.returning_runs(),
            !self.light_changes.is_empty(),
        )
    }
    /// The effective priority of `task_id` in the line of the fill pass
    /// (ADR-t1850-1): the graph's, else (a task the graph read before it
    /// does not hold) its own.
    fn line_priority(
        &mut self,
        effective: &HashMap<TaskId, Priority>,
        task_id: TaskId,
    ) -> Result<Priority> {
        match effective.get(&task_id) {
            Some(priority) => Ok(*priority),
            None => Ok(self.queue.show(task_id)?.task.priority()),
        }
    }
    /// Claim the ready tasks of effective priority `floor` or higher while
    /// there is room, in the claim order (ADR-0040 decision 4).
    /// [`ClaimStep::Stop`] when no further claim may start in this pass
    /// (no room, the claim spacing, the landing branch, a worker that could
    /// not be provisioned), [`ClaimStep::Next`] when only the candidates of
    /// this priority ran out.
    fn claim_tasks(
        &mut self,
        parallel: usize,
        floor: Priority,
        pass: &mut ClaimPass,
    ) -> Result<ClaimStep> {
        loop {
            // A heavy task needs a slot with the landing queue in it, as
            // always; the room the landing queue leaves takes only a light
            // task (ADR-t1591-1).
            let room = self.claim_room(parallel);
            if room == ClaimRoom::None {
                return Ok(ClaimStep::Stop);
            }
            // Highest effective priority, then most-releasing, then lowest
            // ID (ADR-0040 decision 4); `candidates` and `graph` show the
            // same order, so it is not recorded.
            // Less the candidates deferred on a conflict hotspot (ADR-0069).
            let graph = dependency_graph(self.queue.graph_input()?, None);
            let mut order = self.claimable(&graph)?;
            // The candidates of a lower priority wait for the resumes and
            // recovery jobs before them in the line (ADR-t1850-1).
            let effective: HashMap<TaskId, Priority> = graph
                .tasks
                .iter()
                .map(|node| (node.id, node.effective_priority))
                .collect();
            order.retain(|id| effective.get(id).is_some_and(|priority| *priority >= floor));
            // In the light room, only the light tasks, in the same order,
            // whatever the priority of the others.
            if room == ClaimRoom::LightOnly {
                let light: HashSet<TaskId> = self
                    .queue
                    .candidates()?
                    .into_iter()
                    .filter(|task| self.light_changes.admits(task.change(), task.paths()))
                    .map(|task| task.id())
                    .collect();
                order.retain(|id| light.contains(id));
            }
            if order.is_empty() {
                return Ok(ClaimStep::Next);
            }
            // While the load hold is on, a claim waits for the spacing
            // after the queue's latest claim, a light one too, and the next
            // pass judges the load again before it (ADR-t1479-1). The hold
            // itself returned above, before any claim.
            let spacing = claim_spacing::in_effect(self.max_load, self.limits.claim_spacing.value);
            let now_ms = crate::application::unix_millis(self.generators.clock.system_time());
            if spacing.is_some() {
                let last = self
                    .queue
                    .latest_event_of(crate::domain::event_kind::RUN_CLAIMED)?
                    .and_then(|event| crate::domain::stats::timestamp_millis(&event.created_at));
                if light_slots::gated(room, false, spacing, last, now_ms) == ClaimRoom::None {
                    if pass.spaced_since.is_none() {
                        info!(
                            event = "claim_spaced",
                            "the next claim waits for the claim spacing of {}s after the latest claim",
                            self.limits.claim_spacing.value
                        );
                    }
                    self.spaced_since = Some(pass.spaced_since.unwrap_or(now_ms));
                    return Ok(ClaimStep::Stop);
                }
            }
            let spacing = spacing.map(|secs| ClaimSpacing {
                claim_spacing: secs,
                claim_spacing_wait_secs: pass
                    .spaced_since
                    .take()
                    .map_or(0, |since| claim_spacing::waited_secs(since, now_ms)),
            });
            // The landing branch was read at the top of the pass; one that
            // stopped resolving since holds the claims as it would have then
            // (ADR-t615-1), and any other error ends the loop as before.
            let base = match self.repository.main_head() {
                Ok(base) => base,
                Err(error) => {
                    self.resolve_landing_branch();
                    if self.landing_unresolved {
                        info!(
                            event = "claim_landing_branch_unresolved",
                            "claim held after the landing branch stopped resolving during the pass"
                        );
                        return Ok(ClaimStep::Stop);
                    }
                    return Err(error);
                }
            };
            // Read once per pass: `rustc -vV` takes a moment on a loaded host.
            let host = pass.host.get_or_insert_with(|| {
                // Codex's version only when this supervisor runs Codex.
                let codex = self
                    .workers
                    .iter()
                    .any(|worker| worker.provider == crate::domain::Provider::Codex)
                    .then_some(self.layout.codex.as_path());
                // The toolchain only for dagq's source (ADR-t614-1).
                let rustc_in = self
                    .repository
                    .is_dagq_source()
                    .then_some(self.layout.repo_root.as_path());
                (self.host_versions)(&self.layout.claude, codex, rustc_in)
            });
            let host = host.clone();
            let plugin = pass
                .plugin
                .get_or_insert_with(|| (self.worker_plugin)(&self.layout.main_checkout))
                .clone();
            let attributes = ClaimAttributes {
                spacing,
                light_room: (room == ClaimRoom::LightOnly).then_some(true),
                instructions: self.instruction_versions(&base, &plugin),
                ..self.claim_attributes(parallel, host)
            };
            let run = match self.queue.claim_for_supervisor_in_order(
                &base,
                &self.token,
                &order,
                Some(&serde_json::to_value(&attributes)?),
                &pass.trial,
                &self.routes(),
            )? {
                ClaimOutcome::Claimed { run } => *run,
                ClaimOutcome::NoReadyTask => return Ok(ClaimStep::Next),
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
                    return Ok(ClaimStep::Stop);
                }
            }
        }
    }
    /// Record `candidates_sampled` (ADR-0051 decision 3) when this pass's
    /// claimable ready tasks (`graph`'s `candidates`), free slots or ready
    /// tasks differ from the sample this process recorded last, and on its
    /// first claim pass ([`contexts::ObservationState::record_candidates`]).
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
        self.on_observation(|observation, env| observation.record_candidates(env, sample));
    }
    /// Judge whether new claims are held now ([`ClaimHold::judge`]: the
    /// free disk space this pass read against what a claim needs (task
    /// 377), and the load) and record `claim_held` or `claim_resumed` when
    /// the answer differs from the hold in place on the queue (task 327).
    /// Returns whether they are held.
    fn hold_claims(&mut self) -> Result<bool> {
        let needed = self.on_host(|host, env| host.disk_needs(env))?.claim;
        // Short while a cleanup for room runs: wait for it without a hold
        // (task 405).
        if self.host.disk.cleaning
            && matches!((self.host.free, needed), (Some(free), Some(need)) if free < need)
        {
            return Ok(true);
        }
        let hold = ClaimHold::judge(&HoldInputs {
            load_average: (self.load_average)(),
            max_load: self.max_load,
            free_bytes: self.host.free,
            needed_bytes: needed,
            // Claude's hold ask holds the claims only while no worker can
            // run on the other provider either (ADR-t813-2 decision 6).
            queue_hold: self.queue_hold.filter(|_| self.routes().is_empty()),
        });
        self.with_env(|env| env.record_hold(claim_hold::CLAIMS, hold.as_ref()))
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
    /// Resolve the landing branch (ADR-t615-1) when it did not resolve at
    /// the last resolution, its inputs changed since then, or `recheck`
    /// passed,
    /// and hold claims and landings while it does not resolve (task 1078).
    /// The stamp is taken before resolving, so a change during the
    /// resolution differs from it at the next pass.
    fn check_landing_branch(&mut self, recheck: Duration) {
        let stamp = self.repository.landing_branch_stamp();
        // Only a resolution that succeeded is kept: one that failed, maybe
        // for a moment (a Git that did not start), is tried again at the
        // next pass, so the claims resume there once it resolves.
        if let (Some(stamp), Some((last, at))) = (&stamp, &self.landing_stamp)
            && !self.landing_unresolved
            && stamp == last
            && at.elapsed() < recheck
        {
            // A resolution kept after a record of its end that failed.
            if self.landing_recorded != Some(None) {
                self.record_landing_hold(None);
            }
            return;
        }
        self.landing_stamp = stamp.map(|stamp| (stamp, Instant::now()));
        self.resolve_landing_branch();
    }
    /// Resolve the landing branch now and hold claims and landings while it
    /// does not resolve, warning when that changes, and record
    /// `landing_branch_unresolved` / `landing_branch_resolved` where the
    /// hold differs from this supervisor's latest record, so `status` and
    /// `candidates` read it, and a pass that resolves it again records
    /// nothing.
    fn resolve_landing_branch(&mut self) {
        let error = match self.repository.landing_branch() {
            Ok(branch) => {
                if self.landing_unresolved {
                    info!(branch = %branch.name, "the landing branch resolves again to {}; claiming and landing resume", branch.name);
                }
                self.landing_unresolved = false;
                None
            }
            Err(error) => {
                if !self.landing_unresolved {
                    warn!(error = %format_args!("{error:#}"), "{error:#}; no task is claimed and no run lands until it resolves");
                }
                self.landing_unresolved = true;
                Some(format!("{error:#}"))
            }
        };
        self.record_landing_hold(error);
    }
    /// Record the landing branch's hold (`error` while it does not
    /// resolve) where it differs from the one this process last recorded.
    fn record_landing_hold(&mut self, error: Option<String>) {
        use crate::domain::landing_branch::{LANDING_BRANCH_HOLD, UNRESOLVED_REASON};
        let reason = error.is_some().then_some(UNRESOLVED_REASON);
        if self.landing_recorded == Some(reason) {
            return;
        }
        let hold = error.map(|error| (UNRESOLVED_REASON, json!({"error": error})));
        self.landing_recorded = self
            .with_env(|env| env.record_own_hold(LANDING_BRANCH_HOLD, hold))
            .then_some(reason);
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
            self.queue
                .record_queue_event(EventKind::RunEnvChanged, payload)?;
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
            spacing: None,
            light_room: None,
            instructions: BTreeMap::new(),
        }
    }
    /// The versions of the instructions a worker on each of this
    /// supervisor's providers reads in a run from `base` (goal 113), with
    /// `plugin` the hash of a Claude worker's plugin
    /// ([`instructions::by_provider`]).
    fn instruction_versions(
        &self,
        base: &CommitSha,
        plugin: &str,
    ) -> BTreeMap<String, InstructionVersions> {
        instructions::by_provider(
            self.workers.iter().map(|worker| worker.provider),
            |provider| crate::application::prompt::worker_template(provider).ok(),
            plugin,
            self.repository
                .blobs_in(base.as_str(), &instructions::REPOSITORY_INSTRUCTIONS)
                .ok(),
        )
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
            if let Some(worker) = self.moved.remove(slot.run.id()) {
                slot.run = slot.run.clone().running_on(worker);
            }
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
                    self.close_ended_landing_asks(Some(run.task_id()));
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
                    self.fail_recovery(
                        &slot.run,
                        round,
                        alert,
                        attempt,
                        format!("{error:#}"),
                        0,
                        &recovery::JobEnd::default(),
                    );
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
    /// Launch `dagq observe` when an observation is due and none is
    /// running ([`contexts::ObservationState::observer_due`]), on the route
    /// 実行と着地's holds give it. The observer reads the active time of the
    /// spans still open too, recorded first.
    fn start_observer_when_due(&mut self, options: &LoopSettings) {
        let mode = match self
            .on_observation(|observation, env| observation.observer_due(env, options))
        {
            Ok(Some(mode)) => mode,
            Ok(None) => return,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "observer schedule could not be read: {error:#}");
                return;
            }
        };
        let Some(route) = self.job_start_route(crate::domain::actor_model::ModelRole::Observer)
        else {
            return;
        };
        // Its finish is the first `observe_finished` past this mark.
        let mark = match self.queue.latest_event_id() {
            Ok(mark) => mark,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "observer schedule could not be read: {error:#}");
                return;
            }
        };
        self.record_session_turns(true);
        let fallback_jobs = self.fallback.jobs;
        self.on_observation(|observation, env| {
            observation.start_observer(env, mode, route, mark, fallback_jobs);
        });
    }
    /// Start the throughput review due, on the route 実行と着地's holds give
    /// it, when `start` and none runs.
    fn start_throughput_review(&mut self, options: &LoopSettings, start: bool) {
        let Some(due) = self.on_observation(|observation, env| {
            observation.throughput_review_due(env, options, start)
        }) else {
            return;
        };
        let Some(route) =
            self.job_start_route(crate::domain::actor_model::ModelRole::ThroughputReview)
        else {
            return;
        };
        let fallback_jobs = self.fallback.jobs;
        self.on_observation(|observation, env| {
            observation.start_throughput_review(env, due, route, fallback_jobs);
        });
    }
    /// Reap the throughput reviews and have the providers their finishes
    /// say could not be used held.
    fn reap_throughput_reviews(&mut self, options: &LoopSettings) {
        let unusable = self
            .on_observation(|observation, env| observation.reap_throughput_reviews(env, options));
        self.hold_timer_jobs(unusable);
    }
    /// Apply what the headless job `job` returned: the events written
    /// meanwhile record the supervisor as their actor and the job as
    /// `requested_by` (ADR-t728-1 decision 1, task 730). On the way out,
    /// whether `apply` failed or not, `requested_by` goes back to what it
    /// was, so a nested `for_job` leaves the outer job's (task 783).
    pub(super) fn for_job<T>(
        &mut self,
        job: &ActorContext,
        apply: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        requested_by_job(self, |sv| &*sv.queue, job, apply)
    }
    /// Close the review span of `run` when its job ended without a verdict
    /// or could not start: `review_failed` waits for the session's `/exit`,
    /// which is no time of the review (task 541). A failure is logged only
    /// (ADR-0048 decision 10); the span then closes with `review_failed`.
    pub(super) fn close_review_session(
        &mut self,
        run: &TaskRun,
        session: Option<&crate::domain::headless_job::JobSession>,
    ) {
        if let Err(error) = self.queue.close_review_session(run.id(), session) {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: the span of its failed review could not be closed: {error:#}", run.id());
        }
    }
    /// Record the finished transcript turns of the open session spans, and
    /// the remaining turns and measurements of the hook spans closed
    /// without them (ADR-t655-1), and the token cuts of the inbox and
    /// person's planner spans that are due one (ADR-t1486-1 decision 3:
    /// due by the time of their last cut, so the cuts stay hourly across
    /// restarts), when [`SESSION_TURNS_INTERVAL`] passed
    /// since the last time (at once with `now`). A failure is logged only: it changes no run (ADR-0048
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
        self.close_ended_sessions();
        match self.queue.record_session_turns() {
            Ok(0) => {}
            Ok(spans) => info!("recorded the transcript turns of {spans} session span(s)"),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "transcript turns could not be recorded: {error:#}")
            }
        }
        match self.queue.record_session_tokens() {
            Ok(0) => {}
            Ok(cuts) => info!("recorded {cuts} token cut(s) of interactive session spans"),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the tokens of interactive session spans could not be cut: {error:#}")
            }
        }
    }
    /// Close, as `inferred`, the inbox and planner spans the hook recorded
    /// whose session is over without its `SessionEnd`
    /// ([`crate::domain::sessions::inferred_hook_closes`], ADR-t2022-1),
    /// judged from the queue's planner rows and the processes of their
    /// background wrappers; no cmux is asked. A failure is logged only
    /// (ADR-0048 decision 10).
    fn close_ended_sessions(&mut self) {
        let spans = match self.queue.open_hook_session_spans() {
            Ok(spans) if spans.is_empty() => return,
            Ok(spans) => spans,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the open inbox and planner session spans could not be read: {error:#}");
                return;
            }
        };
        // Unread, a person's planner counts as open: only the spans that
        // name no planner wait for the next pass.
        let person_planners_open = match self.queue.planners(false) {
            Ok(planners) => planners.iter().any(|planner| planner.person_retired()),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the planners of the open session spans could not be read: {error:#}");
                true
            }
        };
        let mut over = std::collections::HashMap::new();
        for id in spans
            .iter()
            .filter_map(|span| span.payload["planner_id"].as_i64())
        {
            if over.contains_key(&id) {
                continue;
            }
            // A row that cannot be read is not taken for over.
            let ended = self
                .queue
                .planner(crate::domain::PlannerId::new(id))
                .is_ok_and(|planner| {
                    planner.session_over(
                        |pid| self.processes.alive(pid),
                        |pid| self.processes.start_identity(pid),
                    )
                });
            over.insert(id, ended);
        }
        let ended: Vec<EventId> = crate::domain::sessions::inferred_hook_closes(
            &spans,
            |id| over.get(&id).copied().unwrap_or(false),
            person_planners_open,
        )
        .into_iter()
        .map(|span| span.opened_event_id)
        .collect();
        if ended.is_empty() {
            return;
        }
        match self.queue.close_inferred_sessions(&ended) {
            Ok(0) => {}
            Ok(closed) => {
                info!("closed {closed} inbox or planner session span(s) whose session is over")
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the spans of ended inbox and planner sessions could not be closed: {error:#}")
            }
        }
    }
    /// Kill the observer still running and the processes it started;
    /// `why` ends the log line.
    pub(super) fn stop_observer(&mut self, why: &str) {
        self.on_observation(|observation, env| observation.stop_observer(env, why));
    }
    /// Have 実行と着地, which owns the holds, hold the providers the
    /// finishes 観測と分析 reaped (`unusable`) say could not be used
    /// ([`Self::hold_timer_jobs_unusable`]), and make the jobs whose
    /// provider is held now due again ([`Self::timer_job_due_again`]),
    /// before either job's next start reads the holds.
    fn hold_timer_jobs(&mut self, unusable: Vec<observer::UnusableTimerJob>) {
        if unusable.is_empty() {
            return;
        }
        for job in self.hold_timer_jobs_unusable(unusable) {
            self.observation.timer_job_due_again(job);
        }
    }
    /// Reap the observer once it exited; one that found its provider
    /// unusable has that provider held and is due again
    /// ([`contexts::ObservationState::poll_observer`],
    /// [`Self::hold_timer_jobs`]).
    fn poll_observer(&mut self) {
        let unusable = self.on_observation(|observation, env| observation.poll_observer(env));
        self.hold_timer_jobs(unusable.into_iter().collect());
    }
    /// Whether this process still drives `id` in a slot. Such a run whose
    /// lease another token holds now, stale or not, is adopted, resumed or
    /// triaged by none of the claims of a pass: its slot is dropped on its
    /// next step ([`Self::disown`]), and only a later pass may take the run
    /// like any other, never into a second slot (task 1361).
    fn in_slot(&self, id: &RunId) -> bool {
        self.slots.iter().any(|slot| slot.run.id() == id)
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
    /// ended already, its background wrapper is gone, or the lease is not
    /// known to be this supervisor's; else the `session` of the
    /// `runtime_error`:
    /// `exit` is `sent`, `requested_before` (the `/exit` was typed already
    /// and is never typed twice) or `failed` with the `error`, which
    /// leaves the session to a person (`AttentionNext::ExitSession`).
    fn exit_abandoned_session(&mut self, slot: &Slot) -> Option<Value> {
        let (workspace, requested) = open_session(&slot.phase)?;
        let workspace = workspace.to_owned();
        // Nothing is asked of a session this supervisor may no longer own
        // (the lease check itself failed), nor of one that ended already; a
        // workspace from before ADR-t1433-3 is not asked of cmux, and its
        // wrapper's registration alone says whether it ended
        // (`run_session_gone`).
        if !matches!(self.queue.holds_lease(slot.run.id(), &self.token), Ok(true))
            || matches!(session_alive(self, slot.run.id()), Ok(false))
            || matches!(self.run_session_gone(&workspace), Ok(true))
        {
            return None;
        }
        let (exit, error) = if requested {
            (ABANDON_EXIT_REQUESTED_BEFORE, None)
        } else {
            let failed = |why: String| (ABANDON_EXIT_FAILED, Some(why));
            match submit(self, &slot.run, &workspace, Input::Exit, "/exit") {
                Ok(_) => (ABANDON_EXIT_SENT, None),
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
            } else if let Err(error) =
                stop_run_session(self.sessions, &workspace, StopRoute::Resume)
            {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: the resume's session {workspace} could not be stopped: {error:#}", run.id());
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
                // Judged again on each look: only the landing of another run
                // keeps it in the landing queue (ADR-t1591-1).
                slot.landing_turn = false;
                // Its verification would fail on the missing program, an
                // unreadable CI, an unresolved landing branch or short disk:
                // the run stays awaiting integration, leased, and the
                // integration slot stays free. A supervisor that drains or
                // hands off gives the lease back for a hold that does not
                // end by itself, leaving the run awaiting integration for a
                // person (`review and integrate`)
                // ([`crate::domain::landing_hold::judge`]).
                match crate::domain::landing_hold::judge(
                    crate::domain::landing_hold::LandingHoldInputs {
                        run_env_missing: self.run_env_missing,
                        ci_held: self.observation.ci.held(),
                        ci_unreadable: self.observation.ci.unreadable(),
                        landing_unresolved: self.landing_unresolved,
                        landing_short: self.host.disk.landing_short,
                        disk_cleaning: self.host.disk.cleaning,
                        draining: self.draining,
                    },
                ) {
                    crate::domain::landing_hold::LandingHold::Proceed => {}
                    crate::domain::landing_hold::LandingHold::Wait => return Ok(Step::Continue),
                    crate::domain::landing_hold::LandingHold::HandBack(why) => {
                        warn!(run_id = %slot.run.id(), "run {} is left awaiting integration: {why} and this supervisor stops", slot.run.id());
                        self.queue.release_lease(slot.run.id(), &self.token)?;
                        return Ok(Step::Done(Box::new(self.queue.run(slot.run.id())?)));
                    }
                }
                // An e2e that could not run waits to be tried again; a
                // supervisor that stops cannot wait for it, and leaves the
                // run awaiting integration as above (one that hands off
                // leaves it to the next process, `AwaitingE2e`).
                if self.draining && self.handoff.is_none() && self.e2e_retry_pending(&slot.run) {
                    warn!(run_id = %slot.run.id(), "run {} is left awaiting integration: its e2e could not run and this supervisor stops", slot.run.id());
                    self.queue.release_lease(slot.run.id(), &self.token)?;
                    return Ok(Step::Done(Box::new(self.queue.run(slot.run.id())?)));
                }
                // The e2e a passed run needs runs before it lands
                // (ADR-t1233-2), whatever the integration slot; a head the
                // landing recheck found not landing is parked first, as
                // below, rather than tested.
                if self.e2e_due(&slot.run)?.is_some() {
                    let current = self.queue.run(slot.run.id())?;
                    let main = self.repository.main_head()?;
                    if let Some(parked) = self.park_held_by_recheck(&current, &main)? {
                        return Ok(Step::Done(Box::new(parked)));
                    }
                }
                if let Some(phase) = self.e2e_before_landing(&slot.run)? {
                    slot.phase = phase;
                    return Ok(Step::Continue);
                }
                if !self
                    .queue
                    .runs_with_status(RunStatus::Integrating)?
                    .is_empty()
                {
                    // Reviewed and its e2e done, it waits only for its
                    // landing turn (ADR-t1591-1).
                    slot.landing_turn = true;
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
                        slot.landing_turn = true;
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
            // Decided again as a run awaiting the slot.
            Phase::AwaitingE2e => {
                slot.phase = Phase::AwaitingSlot;
                Ok(Step::Continue)
            }
            Phase::E2e(watch) => {
                if !watch.finished() {
                    return Ok(Step::Continue);
                }
                let Phase::E2e(watch) = std::mem::replace(&mut slot.phase, Phase::AwaitingSlot)
                else {
                    unreachable!("matched above")
                };
                self.finish_e2e(slot, watch)
            }
            Phase::Landing(_) => unreachable!("joined above"),
            Phase::Recovery(watch) => {
                // The job's reply, session and failure are read by the
                // provider it ran on (ADR-t1063-1 decisions 2, 4 and 6).
                let agent = self.job_agent(watch.job.provider).unwrap_or(self.reviewer);
                let Some(outcome) = watch.poll(&*self.files, agent)? else {
                    return Ok(Step::Continue);
                };
                let (round, alert, attempt) = (watch.round, watch.alert, watch.attempt);
                let duration_secs = watch.job.started.elapsed().as_secs();
                let run = self.queue.run(slot.run.id())?;
                // A job that failed: stopped at a wall only a person moves
                // (Claude's), it joins the hold ask, whose `done` starts it
                // again, and its `triage_failed` is no attention meanwhile
                // (task 438). When `[roles.recovery]` names its provider
                // and that provider could not be used, the provider is held
                // and the round's `triage_failed` carries
                // `provider_unusable`: no attention, and the next round
                // starts on the other provider (ADR-t1063-1 decision 4), or,
                // with `[provider_fallback] jobs` off, on the same one once
                // its hold ends (ADR-t1857-1).
                // Only under `--no-claude`, with no provider left, does the
                // next round go to a person told why, never to Claude.
                let end = self.recovery_job_end(
                    run.id(),
                    &watch.job,
                    agent,
                    watch.switchable,
                    outcome.as_ref().err().map(String::as_str),
                );
                let acted = match outcome {
                    Ok(verdict) => {
                        let job = ActorContext::recovery_job(run.id(), alert.as_str(), attempt);
                        self.for_job(&job, |sv| {
                            sv.act_on_recovery(
                                &run,
                                round,
                                alert,
                                attempt,
                                duration_secs,
                                verdict,
                                &end,
                            )
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
                        &end,
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
            Phase::ReviewHeld { session, retried } => {
                if !matches!(self.review_route(), landing::ReviewRoute::Wait(_)) {
                    let (session, retried) = (session.take(), *retried);
                    let run = self.queue.run(slot.run.id())?;
                    slot.phase = self.resume_review(&run, session, retried)?;
                    slot.run = run;
                }
                Ok(Step::Continue)
            }
            Phase::Review(watch) => {
                let provider = self.job_agent(watch.job.provider).with_context(|| {
                    format!("no {} runs on this supervisor", watch.job.provider.as_str())
                })?;
                let Some(outcome) = watch.poll(&*self.files, provider)? else {
                    return Ok(Step::Continue);
                };
                let attempt = watch.attempt;
                let duration_secs = watch.job.started.elapsed().as_secs();
                // A provider that names its session itself (Codex) says its
                // thread and model in its output and rollout: the review's
                // end records them, as a goal or plan review's does
                // (ADR-t1063-1 decision 6); Claude's output gives the job's
                // tokens only (ADR-t1486-1).
                let job_session = provider.job_session(
                    &self
                        .files
                        .read_to_string(&watch.job.stdout)
                        .unwrap_or_default(),
                    watch.job.started_at,
                );
                // Kept in the phase until it is replaced (task 237).
                let session = watch.session.clone();
                let run = self.queue.run(slot.run.id())?;
                // A review stopped at a login that ran out or the usage
                // limit is no failure of the run: it joins the hold ask and
                // waits, its session open, until the hold ends (task 438).
                let wall = outcome.error().and_then(|_| self.job_wall(&watch.job));
                if let Some(error) = outcome.error()
                    && let Some(wall) = wall
                    && self.raise_job_wall(wall, &HoldJob::Review(run.id().clone()), error)
                {
                    info!(run_id = %run.id(), "run {} review {attempt} stopped at the {} wall; it waits for the hold ask with its session open", run.id(), wall.as_str());
                    let retried = watch.retried;
                    slot.phase = Phase::ReviewHeld { session, retried };
                    slot.run = run;
                    return Ok(Step::Continue);
                }
                // Another provider that cannot be used (Codex's login, usage
                // limit or start) is held, and a review whose role names its
                // provider starts again where the route sends it: on Claude
                // when Claude may run (ADR-t1063-1 decisions 4 and 5). Under
                // `--no-claude` it fails to the person below (ADR-t1207-1).
                let retried = watch.retried;
                let mut session = session;
                // Whether the job's provider was found unusable and the
                // review did not move off it (`move_review` gave it back, or
                // the wall's hold could not be written).
                let mut not_moved = wall.is_some();
                if let Some(error) = outcome.error()
                    && watch.job.provider != Provider::Claude
                {
                    let stdout = self
                        .files
                        .read_to_string(&watch.job.stdout)
                        .unwrap_or_default();
                    let said = format!("{error}\n{stdout}");
                    let failure = self.job_failure(&watch.job);
                    if let Some(unusable) = self.job_provider_failed(
                        watch.job.provider,
                        failure,
                        (error, &said),
                        &HoldJob::Review(run.id().clone()),
                        watch.switchable,
                    ) {
                        match self.move_review(
                            &run,
                            session,
                            attempt,
                            (error, job_session.as_ref()),
                            unusable,
                            retried,
                        )? {
                            Ok(phase) => {
                                slot.phase = phase;
                                slot.run = run;
                                return Ok(Step::Continue);
                            }
                            Err(back) => {
                                session = back;
                                not_moved = true;
                            }
                        }
                    }
                }
                // An unreadable verdict or a non-zero exit is reviewed once
                // more (tasks 328 and 1984); with no provider left to review
                // it again (`--no-claude`) it fails as it is, its output
                // named.
                let retry = landing::retries_review(
                    &outcome,
                    self.retry_unreadable_review,
                    retried,
                    matches!(self.review_route(), landing::ReviewRoute::Manual(_)),
                    not_moved,
                );
                slot.phase = match outcome {
                    ReviewEnd::Verdict(verdict) => {
                        let job = ActorContext::review_job(run.id(), attempt);
                        let mut finished = json!({
                            "verdict": verdict.verdict,
                            "reasons": verdict.reasons,
                            "reason_codes": verdict.recorded_codes(),
                            "primary_code": verdict.primary_code(),
                            "summary": verdict.summary,
                            "duration_secs": duration_secs,
                            "attempt": attempt,
                            "recommendation": verdict.recommendation,
                            "confidence": verdict.confidence,
                            "reason_category": verdict.reason_category,
                        });
                        // Each required agent's result, and where they and
                        // the verdict send the run (ADR-t1453-1 decisions
                        // 5 and 7); a review that requires none records as
                        // before.
                        if !verdict.agents.is_empty() {
                            let revise_left = self.revise_left(run.id())?;
                            finished["agents"] = json!(verdict.agents);
                            finished["route"] = verdict.route(revise_left).event_value();
                        }
                        if let Some(job_session) = &job_session {
                            job_session.record(&mut finished);
                        }
                        self.for_job(&job, |sv| {
                            sv.queue.record_runtime_event(
                                run.id(),
                                EventKind::ReviewFinished,
                                finished,
                            )?;
                            info!(run_id = %run.id(), "run {} review {attempt}: {} ({})", run.id(), verdict.verdict.as_str(), verdict.summary);
                            sv.act_on_verdict(&run, session, verdict, &job, attempt)
                        })?
                    }
                    outcome @ (ReviewEnd::Unreadable(_) | ReviewEnd::Failed(_))
                        if let Some(cause) = retry =>
                    {
                        let error = outcome.error().unwrap_or_default();
                        self.retry_review(
                            &run,
                            session,
                            attempt,
                            (error, cause),
                            job_session.as_ref(),
                        )?
                    }
                    ReviewEnd::Unreadable(error)
                    | ReviewEnd::Failed(JobFailed::Exited(error) | JobFailed::TimedOut(error)) => {
                        warn!(run_id = %run.id(), error = %error, "run {} review {attempt} failed: {error}; a person is asked", run.id());
                        self.close_review_session(&run, job_session.as_ref());
                        Phase::Exiting(ExitWatch::new(
                            session,
                            AfterExit::ReviewFailed {
                                attempt,
                                error,
                                duration_secs,
                                output: Some(attempt),
                                session: job_session,
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
                            Fix::Revise { .. } => EventKind::ReviseFinished,
                            Fix::Conflict(_) => EventKind::ConflictResolved,
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
                        let message = revise_mismatch_request(&slot.run, &label, &why)?
                            .with_language(self.verifier.language().as_ref());
                        // Only what the session writes after this counts.
                        let sent_at = self.files.now();
                        let run = slot.run.clone();
                        match submit(
                            self,
                            &run,
                            &session.workspace,
                            Input::from(&message),
                            "receipt fix request",
                        ) {
                            Ok(_submission) => {
                                watch.requested(sent_at, sent_at);
                                let kind = match watch.fix {
                                    Fix::Revise { .. } => EventKind::ReviseReceiptRejected,
                                    Fix::Conflict(_) => EventKind::ConflictReceiptRejected,
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
                                let then = watch.fix.ask(why.clone(), why, None);
                                slot.phase = Phase::Exiting(ExitWatch::new(Some(session), then));
                            }
                        }
                    }
                    ReviseOutcome::Ended(why) => {
                        info!(run_id = %slot.run.id(), "run {}: the session {why} after {label}; asking a person", slot.run.id());
                        let then = watch.fix.ask(
                            format!("the session {why}"),
                            format!("the session {why} after {label}"),
                            None,
                        );
                        let exit = ExitWatch::new(Some(session), then);
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
                        recommendation,
                        confidence,
                        reason_category,
                        requested_by,
                        sent_back,
                    } => {
                        let open = |sv: &mut Self| {
                            sv.open_landing_ask(
                                &run,
                                decision,
                                &reasons,
                                &summary,
                                why.as_deref(),
                                (recommendation, confidence, reason_category),
                            )
                        };
                        // Only the ask's own events carry the job: the
                        // lease given back after it is the supervisor's.
                        let ask = match &requested_by {
                            Some(job) => self.for_job(job, open)?,
                            None => open(self)?,
                        };
                        if let Some(sent_back) = sent_back {
                            self.queue.record_runtime_event(
                                run.id(),
                                EventKind::ConcernSendBackEscalated,
                                json!({"attempt": sent_back.attempt, "why": sent_back.why, "ask_id": ask}),
                            )?;
                        }
                        info!(run_id = %run.id(), "run {} waits for a person in ask {ask}", run.id());
                        self.queue.release_lease(run.id(), &self.token)?;
                        Ok(Step::Done(Box::new(self.queue.run(run.id())?)))
                    }
                    AfterExit::ReviewFailed {
                        attempt,
                        error,
                        duration_secs,
                        output,
                        session,
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
                            "code": if error.starts_with(landing::REVIEW_PROVIDER_DISABLED) {
                                "provider_disabled"
                            } else {
                                ReasonCode::JobFailed.as_str()
                            },
                            "attempt": attempt,
                            "error": error,
                            "duration_secs": duration_secs,
                            "status": run.status().as_str(),
                        });
                        if let Some(ask) = ask {
                            payload["ask_id"] = json!(ask);
                        }
                        if let Some(session) = &session {
                            session.record(&mut payload);
                        }
                        self.queue.record_runtime_event(
                            run.id(),
                            EventKind::ReviewFailed,
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
            // Whether the wrapper has already been asked to exit.
            let typed = watch.requested;
            (session.workspace.as_str(), typed)
        }),
        _ => None,
    }
}

/// Kill the recovery job of a live session's alert the slot's watch runs.
fn stop_recovery(slot: &mut Slot) {
    match &mut slot.phase {
        Phase::Session(watch) => watch.recovery.stop_job(),
        Phase::Exiting(_) => (),
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
/// older than `HEARTBEAT_TIMEOUT_SECS` and its process is gone (for a
/// background wrapper, no process shows its pid with its recorded start).
fn wrapper_dead(sv: &Supervisor<'_>, wrapper: &RunProcess, now: i64) -> bool {
    now - wrapper.heartbeat_at > HEARTBEAT_TIMEOUT_SECS && !sv.wrapper_lives(wrapper)
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
    e2e_paths: Vec<String>,
) -> thread::JoinHandle<Result<Validation>> {
    spawn_traced(move || {
        let mut queue = queues.open()?;
        let task = queue.show(run.task_id())?.task;
        let checked = check_receipt(&*repository, &*files, &task, &run, &e2e_paths)?;
        Ok(match checked {
            Ok(accepted) => Validation {
                accepted: true,
                result_commit: Some(accepted.commit),
                reason: None,
                code: None,
                receipt: serde_json::to_value(accepted.receipt)?,
                evidence_missing: Vec::new(),
                scope_violation: Vec::new(),
                allowed_paths: Vec::new(),
                e2e_requirement: Some(accepted.e2e_requirement),
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
                    e2e_requirement: rejection.e2e_requirement,
                    load: LoadSummary::default(),
                }
            }
        })
    })
}

/// Stop the session wrapper of an accepted run ([`stop_run_session`]). The
/// worktree and branch stay until integration. A failure is recorded but
/// does not change the run status; `workspace_closed_at` stays null so
/// nothing treats it as cleaned.
fn close_workspace(
    queue: &mut dyn Queue,
    sessions: &dyn SessionWrappers,
    token: &LeaseToken,
    run: &TaskRun,
) -> Result<TaskRun> {
    let workspace = run.workspace_id().context("missing workspace")?;
    match stop_run_session(sessions, workspace, StopRoute::AfterReview) {
        Ok(()) => queue.workspace_closed(run.id(), token),
        Err(error) => {
            let message = format!("session {workspace} could not be stopped: {error:#}");
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

/// Run `apply` on `state` with `job` as the `requested_by` of the events
/// its queue (`log`) writes, and put back the `requested_by` it replaced
/// afterwards, whatever `apply` returned (task 783).
fn requested_by_job<S: ?Sized, T>(
    state: &mut S,
    log: impl Fn(&S) -> &dyn RunLog,
    job: &ActorContext,
    apply: impl FnOnce(&mut S) -> Result<T>,
) -> Result<T> {
    let previous = log(state).request_as(Some(job));
    let result = apply(state);
    log(state).restore_request(previous);
    result
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::json;

    use super::*;
    use crate::{
        application::{EndedRunWorkspace, EndedRunWorktree},
        domain::actor::ActorContext,
    };

    /// A run log that keeps the requester [`RunLog::request_as`] sets, the
    /// calls in their order, and the kind of each queue event with the
    /// requester it was written under. That the SQLite store writes the
    /// requester as the event's `actor.requested_by` is the store's own test
    /// (`a_restored_request_is_written_as_the_requester_of_later_events`).
    #[derive(Default)]
    struct Requests {
        requested_by: RefCell<Option<String>>,
        calls: RefCell<Vec<String>>,
        recorded: RefCell<Vec<(String, Option<String>)>>,
    }

    #[allow(unused_variables)]
    impl RunLog for Requests {
        fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn active_runs(&self) -> Result<Vec<TaskRun>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn all_runs(&self) -> Result<Vec<TaskRun>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn all_events(&self) -> Result<Vec<RunEvent>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn run(&self, id: &RunId) -> Result<TaskRun> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn runs_with_status(&self, status: RunStatus) -> Result<Vec<TaskRun>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn next_awaiting_integration(&self) -> Result<Option<TaskRun>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn record_runtime_event(
            &self,
            id: &RunId,
            kind: EventKind,
            payload: serde_json::Value,
        ) -> Result<()> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn ended_run_workspaces(&self) -> Result<Vec<EndedRunWorkspace>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn ended_run_worktrees(&self) -> Result<Vec<EndedRunWorktree>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn ended_run_worktree(&self, _: &RunId) -> Result<Option<EndedRunWorktree>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn latest_event_id(&self) -> Result<EventId> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn latest_runs_in_progress(&self) -> Result<Vec<TaskRun>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn runs_with_pending_push(&self) -> Result<Vec<TaskRun>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn run_in_workspace(&self, workspace_id: &str) -> Result<Option<RunId>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn record_backend_failure(
            &self,
            run: Option<&RunId>,
            payload: serde_json::Value,
        ) -> Result<()> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn record_queue_event(
            &self,
            kind: EventKind,
            payload: serde_json::Value,
        ) -> Result<EventId> {
            let mut recorded = self.recorded.borrow_mut();
            recorded.push((kind.as_str().to_owned(), self.requested_by.borrow().clone()));
            Ok(EventId::new(recorded.len() as i64))
        }
        fn latest_event_of(&self, kind: &str) -> Result<Option<RunEvent>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn latest_events_of(&self, kind: &str, limit: usize) -> Result<Vec<RunEvent>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn latest_events_by_supervisor(
            &self,
            kinds: &[&str],
            supervisors: &[&str],
        ) -> Result<Vec<RunEvent>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn claim_inbox_nudge(&self, payload: serde_json::Value) -> Result<bool> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn record_inbox_watcher_change(
            &self,
            kind: EventKind,
            payload: serde_json::Value,
        ) -> Result<bool> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn events_of_between(
            &self,
            kinds: &[&str],
            after: EventId,
            upto: EventId,
            limit: usize,
        ) -> Result<Vec<RunEvent>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn request_as(&self, requester: Option<&ActorContext>) -> Option<String> {
            self.calls.borrow_mut().push(format!(
                "request_as {:?}",
                requester.map(ActorContext::actor_id)
            ));
            self.requested_by
                .replace(requester.map(|actor| actor.actor_id().to_owned()))
        }
        fn restore_request(&self, previous: Option<String>) {
            self.calls
                .borrow_mut()
                .push(format!("restore_request {previous:?}"));
            self.requested_by.replace(previous);
        }
    }

    #[test]
    fn a_nested_job_request_gives_the_outer_one_back() {
        let mut log = Requests::default();
        let outer = ActorContext::review_job(&RunId::new("r1").unwrap(), 1);
        let inner = ActorContext::review_job(&RunId::new("r2").unwrap(), 3);
        fn run_log(log: &Requests) -> &dyn RunLog {
            log
        }
        let result: Result<()> = requested_by_job(&mut log, run_log, &outer, |log| {
            let inner_result: Result<()> = requested_by_job(log, run_log, &inner, |log| {
                log.record_queue_event(EventKind::ObserveStarted, json!({}))?;
                anyhow::bail!("the inner apply failed")
            });
            assert!(inner_result.is_err());
            log.record_queue_event(EventKind::ObserveFinished, json!({}))?;
            Ok(())
        });
        result.unwrap();
        log.record_queue_event(EventKind::BackendCallFailed, json!({}))
            .unwrap();
        assert_eq!(
            log.calls.into_inner(),
            [
                "request_as Some(\"review-job:r1:1\")",
                "request_as Some(\"review-job:r2:3\")",
                "restore_request Some(\"review-job:r1:1\")",
                "restore_request None",
            ]
        );
        assert_eq!(
            log.recorded.into_inner(),
            [
                (
                    "observe_started".to_owned(),
                    Some("review-job:r2:3".to_owned())
                ),
                (
                    "observe_finished".to_owned(),
                    Some("review-job:r1:1".to_owned())
                ),
                ("backend_call_failed".to_owned(), None),
            ]
        );
    }

    #[test]
    fn a_failed_apply_still_gives_the_previous_requester_back() {
        let mut log = Requests::default();
        let job = ActorContext::review_job(&RunId::new("r1").unwrap(), 2);
        fn run_log(log: &Requests) -> &dyn RunLog {
            log
        }
        let result: Result<()> = requested_by_job(&mut log, run_log, &job, |_| {
            anyhow::bail!("the apply failed")
        });
        assert_eq!(result.unwrap_err().to_string(), "the apply failed");
        assert_eq!(*log.requested_by.borrow(), None);
        assert_eq!(
            log.calls.into_inner(),
            [
                "request_as Some(\"review-job:r1:2\")",
                "restore_request None"
            ]
        );
    }
}
