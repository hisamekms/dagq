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
//! This module holds the states of a slot (`Phase`) and the types of the
//! loop, whose passes are in `run_loop` (`run_loop`, `drive` and the
//! heartbeat's stop) and whose look at a slot is in `slot_step` (`step`,
//! the look at a run waiting for the landing slot); each phase's watch
//! and the supervisor's methods for it are in the submodules: `session`
//! (the worker's session),
//! `exit` (its `/exit`),
//! `jobs` (the headless review and triage), `landing` (the review verdict
//! and the landing), `revise`, `resume`, `triage`, `adopt`, `idle` (the
//! idle marker), `claim` (filling the slots: the line of a fill pass,
//! the claims and what holds them), `slot_end` (giving a slot up:
//! disowning it, abandoning its run, giving up a resume, and the end of
//! the first session's supervision), `sweeps` (the runs' telemetry
//! tokens and sessions, and the sweeps of the held revocations, the kept
//! steps, the ended runs' worktrees and the tokens left behind) and
//! `tick` (a pass over the slots and the periodic jobs: the observer, the
//! throughput reviews and the record of the session turns). The
//! state of 計画管理, 観測と分析 and host運用 is in their modules ([`super::planning`], [`super::observation`],
//! [`super::host_ops`]), whose fields the loop cannot see: it reads and
//! changes that state through its methods. The prompts and requests are
//! in [`crate::application::prompt`].

use super::*;

mod adopt;
mod agent_cases;
mod agent_eval;
pub(super) mod background;
mod claim;
pub(super) mod claim_defer;
mod container;
mod container_sccache;
mod deliver;
mod e2e;
mod eval_gate;
mod exit;
mod file_time;
mod handoff_slots;
pub(super) mod headless;
mod idle;
mod jobs;
mod landing;
pub(super) mod provider;
mod queue_hold;
pub(super) mod recheck;
mod reconcile;
mod recovery;
mod reopen;
mod replan_diagnosis;
mod replan_park;
mod resume;
mod revise;
mod run_loop;
mod satisfied;
mod satisfied_review;
mod session;
mod slot_end;
mod slot_limits;
mod slot_step;
mod stages;
mod stale;
mod stall;
mod stall_recovery;
mod stranded;
mod sweeps;
mod task_hold;
mod tick;
mod triage;
mod waiting;

pub use crate::application::supervisor_service::end_bound::EndDeadline;
use crate::application::telemetry_tokens;
use crate::domain::run::review_findings;

pub(super) use self::background::WorkspaceCloser;
use self::background::{close_ended_landing_asks, close_open_workspaces};
pub(crate) use self::background::{
    left_planner_turn, left_turn, stop_left_turn, stop_run_session, stop_session,
};
pub use self::claim_defer::read_conflicts_at_start;
pub(crate) use self::deliver::{Input, Submission};
pub use self::e2e::RunE2ePort;
use self::handoff_slots::Pass;
pub(super) use self::headless::send_to_planner;
pub(crate) use self::headless::{lock_waiting, provider_failure, write_request};
pub use self::jobs::{
    HeadlessJob, JobEnds, JobFailed, JobPorts, JobSubject, PROGRAM_OUTPUT_TAIL, ProgramEnd,
    record_job, start_program_job, start_review_program,
};
pub(super) use self::jobs::{JobDesk, again_on};
#[cfg(test)]
pub(super) use self::jobs::{job_failure_route, job_unusable_of};
use self::{
    deliver::*, exit::*, headless::*, idle::*, jobs::*, provider::*, recovery::*, resume::*,
    revise::*, session::*, stale::*, stall::*, waiting::*,
};

/// How often the supervisor records the finished transcript turns of the
/// session spans still open (ADR-0048 decision 8).
pub const SESSION_TURNS_INTERVAL: Duration = Duration::from_secs(600);

/// The snapshot of the binary a run's session wrapper runs from, in the
/// run directory: copied for each session (a resume copies it again) so
/// rebuilding the binary does not change a running one, and removed once
/// the run ended and nobody leases it ([`cleanup`]).
pub(in crate::application) const RUN_RUNNER_FILE: &str = "runner";

/// The longest a pass keeps the landing branch's last resolution while the
/// stamp of its inputs stays the same (task 1078): a bound for a change the
/// stamp misses, such as an edit in place that keeps the size, the inode
/// and a coarse modification time, and for what it does not read (where a
/// branch ref that is itself symbolic points, `config.worktree` and an
/// included config). While the branch does not resolve, every pass resolves
/// it again, so a hold ends at the first pass it resolves at.
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
    /// `[fresh_session]` at the start (ADR-t2080-1), read again each pass.
    pub fresh_session: crate::domain::fresh_session::FreshSessionConfig,
    /// `[review.jobs]` at the start (ADR-t1895-1 decision 1): the timeout
    /// of each kind of job of a run's review.
    pub review_jobs: crate::domain::headless_job::ReviewJobs,
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
    /// Least time between two observations of `stats`' judgments of now
    /// ([`crate::domain::live_alerts::OBSERVE_INTERVAL_SECS`]; tests
    /// shorten it).
    pub live_alert_interval: Duration,
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
    /// How this process was started (`supervise --mode`): the mode of the
    /// registration it makes and what its start mark records (ADR-0051
    /// decision 10). launchd passes it from the agent's arguments whether
    /// `up` loaded the agent or launchd started it on its own (at login,
    /// after a restart, by `KeepAlive`); `None` leaves a new registration
    /// without a mode, as one started by hand, and a handoff keeps the
    /// registration's.
    pub mode: Option<crate::domain::SupervisorMode>,
    /// The automatic update of this supervisor's binary (ADR-0045
    /// decision 17).
    pub update: UpdateSettings,
    /// `--max-load`: no new run is claimed while the 1-minute load average
    /// is above it (task 327); `None` holds for no load.
    pub max_load: Option<f64>,
    /// Where `max_load` comes from: `--max-load` or the default.
    pub max_load_source: crate::domain::slot_limits::SettingSource,
    /// How much free disk space a claim and a landing need (task 377).
    pub disk: crate::domain::disk::DiskConfig,
    /// `[resume]`: the limit of a run's conflict-only attempts (ADR-0047
    /// decision 24).
    pub resume: ResumeConfig,
    /// The process this supervisor stops with
    /// ([`crate::domain::queue_service::OWNER_PID_ENV`]): a test's, so a
    /// supervisor a test started does not outlive it however it ends (a
    /// SIGKILL included). Once it is gone, the loop stops as on a stop
    /// request, but lets its runs go instead of waiting for them
    /// (`let_go_for_owner`) and exits within `owner_gone_grace`, the waits
    /// of the loop's end included; `supervisor_draining` and
    /// `supervisor_stopped` record the reason `owner_gone`. One already
    /// gone at the start registers nothing. An exec keeps the variable, so
    /// the next binary stops with the same owner. None in production,
    /// where it runs until it is stopped.
    pub owner: Option<u32>,
    /// How long a supervisor whose owner is gone may still wait before it
    /// exits ([`OWNER_GONE_GRACE`]; tests shorten it).
    pub owner_gone_grace: Duration,
    /// How long a supervisor whose heartbeat stopped may still wait before
    /// it exits ([`LEASE_LOST_GRACE`]; tests shorten it).
    pub lease_lost_grace: Duration,
    /// Counts the loop's passes, one at the top of each. Tests read it to
    /// wait for passes after a threshold instead of a fixed sleep (task
    /// 1046); a `--once` call names its records by it
    /// ([`run_loop::note_once_goes_on`]).
    pub passes: Arc<AtomicU64>,
    /// Counts the cleanup jobs of ended runs' worktrees the loop started
    /// and joined. Tests read it between passes to wait for a cleanup
    /// to end instead of a fixed sleep.
    pub cleanup_jobs: Arc<CleanupJobs>,
}

/// How many cleanup jobs of ended runs' worktrees the loop started and
/// joined ([`LoopSettings::cleanup_jobs`]). The loop alone moves both
/// counts, so between two of its passes they say whether a job runs.
#[derive(Debug, Default)]
pub struct CleanupJobs {
    started: AtomicU64,
    joined: AtomicU64,
}

impl CleanupJobs {
    /// Every job the loop started it also joined, so what each settled is
    /// cached and none reads the queue now. Read between two passes: a
    /// request no job took yet starts one in the same call.
    pub fn all_joined(&self) -> bool {
        self.started.load(Ordering::SeqCst) == self.joined.load(Ordering::SeqCst)
    }
    /// The loop started a job.
    pub(in crate::application::supervise) fn started(&self) {
        self.started.fetch_add(1, Ordering::SeqCst);
    }
    /// The loop joined a job.
    pub(in crate::application::supervise) fn joined(&self) {
        self.joined.fetch_add(1, Ordering::SeqCst);
    }
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
    /// `agent-evals/` of the queue: one directory per round of the eval of
    /// an agent (ADR-t1728-1).
    pub agent_evals_dir: PathBuf,
    /// `agent-cases/` of the queue: one directory per production set of an
    /// agent's cases (ADR-t1728-1 (g)).
    pub agent_cases_dir: PathBuf,
    /// `review-programs/` of the queue: one directory per run, where the
    /// program reviews of its review write the scripts they run, outside
    /// what its worker can write (ADR-t1895-2 decision 2).
    pub review_programs_dir: PathBuf,
    /// `satisfied/` of the queue: one scratch worktree per run, where the
    /// snapshot its `already_satisfied` claim is verified on is checked
    /// out, outside what its worker can write
    /// ([`crate::application::integrate::satisfied_scratch`]).
    pub satisfied_dir: PathBuf,
    /// `replans/` of the queue: the snapshot of each run parked for a
    /// replanning, outside what its worker can write (ADR-t1521-2).
    pub replans_dir: PathBuf,
    /// The file of the supervisor principal's token once it signed in
    /// ([`crate::application::supervisor_service::ServiceCaller::credential`]):
    /// the commands it starts (`observe`, `throughput-review`) are given it
    /// to ask the queue service for their jobs' tokens as the supervisor
    /// (ADR-t3102-1 decision 2).
    pub service_credential: std::sync::OnceLock<PathBuf>,
}

impl Layout {
    /// The supervisor as an actor: `supervisor:<pid>`, what its own events
    /// and the commands it starts for itself record.
    pub fn supervisor_actor(&self) -> ActorContext {
        ActorContext::instance(crate::domain::ActorRole::Supervisor, self.pid)
    }
}

#[cfg(test)]
impl Layout {
    /// The layout of the queue `<root>/queue.db` in a unit test: the runtime
    /// binary `/opt/bin/dagq` with Claude Code and Codex beside it, the
    /// repository `/repo` and the plugin directory `/plugin`.
    pub(in crate::application) fn for_test(root: &Path) -> Self {
        Self {
            db: root.join("queue.db"),
            runs_dir: root.join("runs"),
            queue_hash: "queue-hash".into(),
            repo_root: "/repo".into(),
            main_checkout: "/repo".into(),
            common_dir: "/repo/.git".into(),
            claude: "/opt/bin/claude".into(),
            codex: "/opt/bin/codex".into(),
            codex_home: None,
            providers: Vec::new(),
            runner: "/opt/bin/dagq".into(),
            pid: 42,
            version: "0.0.0-test".into(),
            observer_env_remove: Vec::new(),
            planners_dir: root.join("planners"),
            plugin_dir: Some("/plugin".into()),
            plan_reviews_dir: root.join("plan-reviews"),
            goal_reviews_dir: root.join("goal-reviews"),
            agent_evals_dir: root.join("agent-evals"),
            agent_cases_dir: root.join("agent-cases"),
            review_programs_dir: root.join("review-programs"),
            satisfied_dir: root.join("satisfied"),
            replans_dir: root.join("replans"),
            service_credential: std::sync::OnceLock::new(),
        }
    }
}

/// A point where a test pauses a supervisor ([`Ports::pauses`]) to change
/// the queue meanwhile, as another supervisor or a person would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PausePoint {
    /// A replanning's park, before it looks at the run's writer (and stops
    /// one past its limit).
    ParkLook,
    /// The delivery of an answer, before it is written as the session's
    /// next turn.
    AnswerSend,
    /// A request of a headless session, once written to its `turns/` and
    /// before `turn_requested` records it: the session may take it, and
    /// its turn end, before the record.
    RequestWritten,
    /// A resume, once its session's earlier requests were set aside,
    /// before its wrapper starts.
    ResumeLaunch,
    /// A receipt's validation that ended, before the supervisor acts on
    /// its result.
    ValidationEnd,
    /// A landing recheck that found a run no longer landing, before the
    /// supervisor records the finding and parks the run.
    RecheckFailure,
    /// A diagnosis job's verdict decided on, before its move or ask is
    /// written: another supervisor may settle the same verdict meanwhile.
    VerdictDecided,
    /// The restore of a container run's work handed to its executor (the
    /// request written, the clone made again), before the restore's lease
    /// is given back for the resume.
    RestoreHanded,
    /// The end of the supervisor's loop, once the loop ended, before it
    /// looks at its heartbeat and sends what it holds: of no run.
    LoopEnded,
    /// The end of the supervisor's loop, before its heartbeat stops for the
    /// last look at it and its registration goes: of no run.
    HeartbeatEnding,
    /// The supervisor's end, once its registration is removed, before it
    /// stops the queue's broker and service after a `down`'s drain and
    /// gives its token back: of no run.
    Deregistered,
}

/// What a supervisor calls at each [`PausePoint`] with the run's ID, `None`
/// at a point of no run.
pub type Pauses = Arc<dyn Fn(PausePoint, Option<&RunId>) + Send + Sync>;

/// The ports the supervisor works through, and where it works.
pub struct Ports<'a> {
    /// The supervisor's queue: the claim, the leases, the runs' transitions
    /// and events and the reads it decides on through the queue service
    /// ([`crate::application::supervisor_service::SupervisorQueues`]).
    pub queues: Arc<dyn QueueOpener>,
    /// How `queues` reaches the service, which the supervisor signs in to
    /// as its registration before anything else.
    pub service: Arc<dyn crate::application::supervisor_service::ServiceCaller>,
    /// The connections of 観測と分析's jobs off the loop, as its ports.
    pub observation_queues: Arc<dyn QueueOpener<dyn crate::application::ObservationQueue + Send>>,
    /// The connections of host運用's jobs off the loop, as its ports.
    pub host_queues: Arc<dyn QueueOpener<dyn crate::application::HostOpsQueue + Send>>,
    /// The connections the recording of the session wrappers records the
    /// failed calls and the stops through, as its ports.
    pub recording_queues: Arc<dyn QueueOpener<dyn crate::application::RecordingQueue + Send>>,
    pub repository: Arc<dyn Repository + Send + Sync>,
    pub remote: Arc<dyn MainRemote + Send + Sync>,
    pub verifier: Arc<dyn Verifier + Send + Sync>,
    /// Whether the container runtime can run what `dagq.toml` sets to it
    /// (goal 38): asked every pass for each such target.
    pub container: Arc<dyn crate::application::ContainerRuntime + Send + Sync>,
    /// The containers of the runs whose worker works in a clone (goal 38):
    /// a stop of such a run's session or processes removes its container.
    pub run_containers: Arc<dyn crate::application::run_container::RunContainers>,
    /// The tokens of the landings' verifications (ADR-t2114-1 decision 2).
    pub verification_tokens: Arc<dyn crate::application::queue_broker::VerificationTokens>,
    /// The containers of the landings' verifications `dagq.toml` sets to
    /// the container (ADR-t2113-3 decision 1).
    pub verification_containers: Arc<dyn crate::application::run_container::VerificationContainers>,
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
    /// Where the program jobs of a review run, by the backend of the
    /// review's actor (ADR-t1895-2 decision 5): an eval's case's program
    /// reviews (ADR-t1728-1 (i)).
    pub programs: crate::application::review_programs::ProgramBackends<'a>,
    /// The host's program backend and spawner, owned, that the landings'
    /// threads run the landing's program reviews with (ADR-t3749-1), on
    /// [`Self::programs`]' configuration.
    pub landing_programs: crate::application::review_programs::ProgramHost,
    /// The queue service's tokens and socket, which the workers and the
    /// jobs are given instead of the queue's path (goal 82's stage (3)).
    pub service_access: &'a dyn crate::application::queue_service::ServiceAccess,
    /// The telemetry tokens of the runs' executors, which the supervisor
    /// issues at the claim, the resume and an executor's start and revokes
    /// at the run's end and a lost lease (ADR-t1662-3 decision 11).
    pub telemetry_tokens: &'a dyn crate::application::queue_service::TelemetryTokens,
    /// Where the landings' and the e2e's verifications, the runs'
    /// executors too, send the steps of the commands they run
    /// (ADR-t1662-3 decision 1).
    pub telemetry_sinks: Arc<dyn crate::application::TelemetrySinks>,
    /// The deadline of the end of a supervisor whose heartbeat stopped,
    /// which bounds every call of `service` and every report of
    /// `telemetry_sinks` from then ([`EndDeadline`]): the same one given to
    /// both, set by the heartbeat's thread as it stops.
    pub end: Arc<EndDeadline>,
    /// Where tests pause the supervisor ([`PausePoint`]); none outside them.
    pub pauses: Option<Pauses>,
    pub files: Arc<dyn RunFiles>,
    pub processes: Arc<dyn ProcessControl + Send + Sync>,
    pub generators: Generators,
    /// Writes a task's review material (`review`) and reports its path.
    /// `review.md` of a task's run, over the attempt's range when one is
    /// fixed already.
    pub review_material:
        &'a dyn Fn(TaskId, Option<&crate::application::review::ReviewRange>) -> Result<Value>,
    /// The log of this start, given the registration's `started_at`.
    /// The 1-minute load average recorded with a failed session wrapper
    /// call, at a claim and over each interval of a run.
    pub load_average: fn() -> Option<f64>,
    /// The free bytes of the file system of a path (task 377).
    pub free_space: fn(&Path) -> Option<u64>,
    /// The size of the file system of a path, the reserve's share of which
    /// a claim and a landing keep free (ADR-t4473-1).
    pub total_space: fn(&Path) -> Option<u64>,
    /// The directories Claude Code keeps the sessions' scratchpads under,
    /// read at each cleanup (task 1100).
    pub scratchpad_roots: ScratchpadRoots,
    /// Measures the bytes on disk of a tree as
    /// [`RunFiles::tree_size`] does, for the sizes of the runs moving the
    /// claims count off the loop (ADR-t4542-1).
    pub measure_tree: super::host_ops::moving_sizes::MeasureTree,
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
    /// Where the verdicts of plan and goal reviews that ended are kept
    /// while the queue does not take them, for this supervisor and the
    /// next (ADR-t3102-3 decision 5).
    pub held_verdicts: Arc<dyn crate::application::supervisor_service::resend::HeldRecords>,
    /// `[conflicts]` of the repository's `dagq.toml` (`None` for no
    /// file), read again each pass (ADR-0080); `None` when the options
    /// set the thresholds, which are then never read again.
    pub conflicts_file: Option<ConflictsFile>,
    /// `[supervisor]` of the repository's `dagq.toml` (`None` for no
    /// file), read again each pass for the values the flags did not give
    /// (task 698); `None` reads nothing.
    pub supervisor_file: Option<SupervisorFile>,
    /// Deployment startup tables, retried until installed before claims.
    pub repository_startup_file: Option<super::repository_config::RepositoryStartupFile>,
    /// `[provider_fallback]` of the repository's `dagq.toml` (`None`
    /// for no file), read again each pass (ADR-t1857-1); `None` reads
    /// nothing.
    pub provider_fallback_file: Option<ProviderFallbackFile>,
    /// `[fresh_session]` of the repository's `dagq.toml` (`None` for no
    /// file), read again each pass (ADR-t2080-1); `None` reads nothing.
    pub fresh_session_file: Option<FreshSessionFile>,
    /// `[replan]` of the repository's `dagq.toml` (`None` for no file,
    /// which detects and diagnoses nothing), read again each pass; `None`
    /// reads nothing.
    pub replan_file: Option<ReplanFile>,
    /// `[[recurring]]` of the repository's `dagq.toml` (ADR-0072
    /// decision 1), read again each pass; `None` registers no recurring
    /// draft.
    pub recurring_file: Option<RecurringFile>,
    /// Records the forecast snapshots (ADR-0070 decision 3); `None`
    /// records none.
    pub forecasts: Option<ForecastPort>,
    /// How often the watch of the adopted agent definitions looks
    /// (ADR-t1728-1 (g)); `None` watches none.
    pub agent_watch: Option<Duration>,
    /// Looks for a new release on crates.io (ADR-t618-1); `None` looks
    /// for none.
    pub release: Option<ReleasePort>,
    /// Records the host's load under `<queue dir>/host/` (task 516);
    /// `None` records none.
    pub host_metrics: Option<HostMetricsPort>,
    /// Keeps the queue's service running (ADR-t1233-4 decision 2), started
    /// before the registration when it does not answer; `None` keeps none
    /// and holds nothing for it (the tests).
    pub queue_service: Option<QueueServicePort>,
    /// Keeps the queue's broker running (ADR-t2114-1 decision 1); `None`
    /// keeps none (a `--once` pass, the tests).
    pub queue_broker: Option<QueueBrokerPort>,
    /// Keeps the relays of the runs' containers running (ADR-t2114-3
    /// decision 4); `None` keeps none (a `--once` pass, the tests).
    pub broker_relay: Option<QueueRelayPort>,
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
    /// Builds the ledger (docs/design/measurement.md "台帳を作る係");
    /// `None` builds none.
    pub ledger: Option<LedgerPort>,
    /// The environment the runs execute in (ADR-t1662-3 decision 3): a
    /// run's claim, resume and adoption record what it is attached to
    /// (`environment_attached`); `None` records none.
    pub environment: Option<Arc<dyn crate::application::ExecutionEnvironment>>,
    /// Measures the runs' resources from outside
    /// (docs/design/measurement-environment.md "runの資源"); `None` measures none.
    pub resources: Option<ResourcesPort>,
    pub layout: Layout,
}

/// Lists the directories Claude Code keeps the sessions' scratchpads
/// under (task 1100).
pub type ScratchpadRoots = Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync>;

/// Reads `[conflicts]` of the repository's `dagq.toml` (ADR-0080).
pub type ConflictsFile =
    Arc<dyn Fn() -> Result<Option<crate::domain::stats::ConflictConfig>> + Send + Sync>;

/// Reads `[supervisor]` of repository rules: the main checkout locally,
/// main through the runner App in deployment. A `ConfigUnavailable` error
/// means no deployment snapshot has ever been read and holds new claims.
pub type SupervisorFile = Arc<dyn Fn() -> Result<Option<SupervisorConfig>> + Send + Sync>;

/// Reads `[provider_fallback]` of the repository's `dagq.toml`
/// (ADR-t1857-1).
pub type ProviderFallbackFile =
    Arc<dyn Fn() -> Result<Option<crate::domain::provider_switch::ProviderFallback>> + Send + Sync>;

/// Reads `[fresh_session]` of the repository's `dagq.toml`
/// (ADR-t2080-1).
pub type FreshSessionFile =
    Arc<dyn Fn() -> Result<Option<crate::domain::fresh_session::FreshSessionConfig>> + Send + Sync>;

/// Reads `[replan]` of the repository's `dagq.toml`
/// (docs/plans/task-replanning.md).
pub type ReplanFile =
    Arc<dyn Fn() -> Result<Option<crate::domain::replan_detect::ReplanConfig>> + Send + Sync>;

/// Reads `[[recurring]]` of the repository's `dagq.toml` (ADR-0072).
pub type RecurringFile =
    Arc<dyn Fn() -> Result<Vec<crate::domain::recurring::RecurringDraft>> + Send + Sync>;

/// Spawn a thread that reports its `tracing` events to the subscriber of
/// the spawning thread, so a supervisor run under a scoped subscriber (the
/// tests) keeps the events of its heartbeat, validations and landings.
pub fn spawn_traced<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> thread::JoinHandle<T> {
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    thread::spawn(move || tracing::dispatcher::with_default(&dispatch, work))
}

/// When the end of the loop stops waiting ([`finished_by`]) and sending
/// ([`handoff::send_within`]): the deadline of a supervisor whose owner is
/// gone, set as the loop ends, and that of one whose heartbeat stopped,
/// set the moment it stopped ([`EndDeadline`]), read again at each look so
/// a heartbeat that stops while the end waits bounds that wait too; the
/// earlier of the two ([`handoff::end_at`]), none while neither is.
#[derive(Debug, Clone)]
pub(in crate::application) struct EndBy {
    owner_gone: Option<Instant>,
    lease_lost: Arc<EndDeadline>,
}

impl EndBy {
    /// The deadline now, if there is one.
    pub(in crate::application) fn at(&self) -> Option<Instant> {
        handoff::end_at(self.owner_gone, self.lease_lost.at())
    }

    /// What is left of it at `now`, if there is one.
    pub(in crate::application) fn left(&self, now: Instant) -> Option<Duration> {
        self.at().map(|at| at.saturating_duration_since(now))
    }
}

/// Wait for `job` to finish, until `deadline` on `clock`'s monotonic clock
/// when there is one ([`EndBy`], read again at each look): whether
/// it finished, so that joining it does not wait. The end of the loop of
/// a supervisor whose owner is gone or whose heartbeat stopped waits for
/// its jobs so ([`OWNER_GONE_GRACE`], [`LEASE_LOST_GRACE`]); every other
/// end waits for them to finish. What
/// to do next is [`handoff::job_wait`]'s; this reads the clock and sleeps.
pub(in crate::application) fn finished_by<T>(
    job: &thread::JoinHandle<T>,
    deadline: &EndBy,
    clock: &dyn crate::application::Clock,
) -> bool {
    loop {
        let left = deadline.left(clock.monotonic());
        match handoff::job_wait(job.is_finished(), left) {
            handoff::JobWait::Join => return true,
            handoff::JobWait::Leave => return false,
            handoff::JobWait::Wait => thread::sleep(Duration::from_millis(20)),
        }
    }
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
    /// ([`crate::application::QUEUE_BUSY_TIMEOUT`]).
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
            attempt: crate::application::QUEUE_BUSY_TIMEOUT,
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

    /// Whether a write that failed as busy (or met no queue service),
    /// `since` the last one written
    /// (the failed write's wait included), is tried again: only while the
    /// next write, waiting out the interval and the lock, still lands before
    /// the leases go stale.
    pub fn retries(&self, busy: bool, since: Duration) -> bool {
        busy && since + self.interval + self.attempt < self.stale_after
    }

    /// Why a write that went through shows the process no longer holds what
    /// it heartbeats, if it does: its registration is gone (removed as
    /// stale by `up` or `down --force`, so its runs are another's to take).
    pub fn lost(&self, write: &crate::application::HeartbeatWrite) -> Option<&'static str> {
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

/// Why a [`Heartbeat`] stopped, as the loop records it on the queue
/// (`supervisor_heartbeat_failed`, [`Self::payload`]), so a supervisor that
/// ended on a queue that stayed busy is told from one that ended otherwise.
#[derive(Debug, Clone, PartialEq)]
pub struct HeartbeatFailure {
    /// What stopped it ([`HeartbeatFailure::cause_of`]).
    pub cause: &'static str,
    /// The writes that failed in a row before it stopped, the last one
    /// included; none for a registration found gone.
    pub failures: u32,
    /// The seconds from the first of those failures to the last one's end.
    pub secs: f64,
    /// The last failure's error, with how many failed over how long since
    /// the last write.
    pub error: String,
}

impl HeartbeatFailure {
    /// The cause of a heartbeat that stopped on a write that failed:
    /// `busy` when the queue stayed locked until the leases would go
    /// stale, `unreachable` when the queue service did not answer as long,
    /// and `failed` for a failure that is not tried again.
    pub const fn cause_of(busy: bool, unreachable: bool) -> &'static str {
        if busy {
            "busy"
        } else if unreachable {
            "unreachable"
        } else {
            "failed"
        }
    }

    /// The cause of a heartbeat that stopped on a write that went through
    /// and found the registration gone ([`HeartbeatPolicy::lost`]).
    pub const REGISTRATION_GONE: &'static str = "registration_gone";

    /// The payload of `supervisor_heartbeat_failed` of the supervisor
    /// `token`.
    pub fn payload(&self, token: &LeaseToken) -> Value {
        json!({
            "supervisor": token,
            "cause": self.cause,
            "failures": self.failures,
            "secs": (self.secs * 10.0).round() / 10.0,
            "error": self.error,
        })
    }
}

/// One process heartbeats its registration (a resident supervisor) and every
/// lease it holds with a single token, every `interval`. A write that meets
/// another connection's lock is tried again at the next interval, and only a
/// lost registration, a failure that does not pass or busy writes that would
/// let the leases go stale stop it (task 1119).
pub struct Heartbeat {
    stop: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
    failed: Arc<std::sync::Mutex<Option<HeartbeatFailure>>>,
}

impl Heartbeat {
    pub fn start(queues: Arc<dyn QueueOpener>, token: LeaseToken, policy: HeartbeatPolicy) -> Self {
        Self::start_with(queues, token, policy, None)
    }

    /// [`Self::start`], `stopped` called on the heartbeat's thread as it
    /// stops on a failure, before the failure can be read
    /// ([`Self::check`]).
    pub fn start_with(
        queues: Arc<dyn QueueOpener>,
        token: LeaseToken,
        policy: HeartbeatPolicy,
        stopped: Option<Box<dyn FnOnce() + Send>>,
    ) -> Self {
        let (stop, recv) = mpsc::channel();
        let failed = Arc::new(std::sync::Mutex::new(None));
        let flag = failed.clone();
        let worker = spawn_traced(move || {
            if let Err(failure) = beat(&*queues, &token, policy, &recv) {
                let error = &failure.error;
                error!(
                    error = %error,
                    cause = failure.cause,
                    failures = failure.failures,
                    "supervisor heartbeat failed: {error}"
                );
                if let Some(stopped) = stopped {
                    stopped();
                }
                *flag.lock().unwrap_or_else(|e| e.into_inner()) = Some(failure);
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

    /// The failure that stopped the heartbeat, if any; the loop records it
    /// on every run as a `runtime_error` with the reason `lease_lost`, and
    /// its cause on the queue ([`HeartbeatFailure`]).
    /// When writes failed, its message carries how many failed in a row
    /// and over how many seconds since the last one written (`beat`).
    pub fn check(&self) -> Result<()> {
        match &*self.failed.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(failure) => {
                bail!(
                    "supervisor heartbeat failed; preserving runs for inspection: {}",
                    failure.error
                )
            }
            None => Ok(()),
        }
    }

    /// Why the heartbeat stopped, if it did ([`HeartbeatFailure`]).
    pub fn failure(&self) -> Option<HeartbeatFailure> {
        self.failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// The heartbeat's loop: a write every interval until `stop` says so, or an
/// error once the process can no longer keep what it holds.
#[expect(
    clippy::disallowed_methods,
    reason = "L5: the heartbeat thread's thin loop; alive_due decides on the seconds it hands over"
)]
fn beat(
    queues: &dyn QueueOpener,
    token: &LeaseToken,
    policy: HeartbeatPolicy,
    stop: &mpsc::Receiver<()>,
) -> Result<(), HeartbeatFailure> {
    let mut queue: Option<Box<dyn Queue + Send>> = None;
    let mut written = Instant::now();
    let mut streak: Option<BusyStreak> = None;
    // The evidence of life before the first `supervisor_alive` is the
    // supervisor's start, recorded just before.
    let mut alive = Instant::now();
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
                    return Err(HeartbeatFailure {
                        cause: HeartbeatFailure::REGISTRATION_GONE,
                        failures: 0,
                        secs: 0.0,
                        error: lost.to_owned(),
                    });
                }
                written = started;
                // A resident supervisor's evidence of life on the events
                // (ADR-t1662-1 decision 6); one that fails is tried again
                // after the interval, and stops neither the heartbeat nor
                // the supervisor.
                if policy.registered
                    && crate::domain::supervisor_life::alive_due(
                        i64::try_from(alive.elapsed().as_secs()).unwrap_or(i64::MAX),
                    )
                    && let Some(queue) = &queue
                {
                    match queue.record_queue_event(
                        EventKind::SupervisorAlive,
                        json!({"supervisor": token}),
                    ) {
                        Ok(_) => alive = started,
                        Err(error) => {
                            alive = started;
                            warn!(error = %format_args!("{error:#}"), "the supervisor's evidence of life could not be recorded: {error:#}");
                        }
                    }
                }
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
                // A queue service that does not answer (being replaced, or
                // started again) passes as a busy queue does: tried again
                // while the leases hold (ADR-t3102-3 decision 5).
                let busy = crate::application::QueueBusy::is(&error);
                let unreachable = crate::application::supervisor_service::unreachable(&error);
                if !policy.retries(busy || unreachable, since) {
                    let error = error.context(format!(
                        "{} heartbeat writes failed over {:.1}s since the last one written",
                        streak.failures,
                        since.as_secs_f64()
                    ));
                    return Err(HeartbeatFailure {
                        cause: HeartbeatFailure::cause_of(busy, unreachable),
                        failures: streak.failures,
                        secs: streak.since.elapsed().as_secs_f64(),
                        error: format!("{error:#}"),
                    });
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
///
/// The start, in this order: an owner already gone starts nothing; the
/// queue must be outside the worktree or under its Git common directory (a
/// queue in the user's directory always is); every worker's agent is
/// preflighted (Claude's not under `--no-claude`), and a live supervisor of
/// the other `--no-claude` policy is refused; the queue is bound to the
/// repository, and one bound to another is refused, though nothing else is
/// exclusive: several supervisors may share a queue. Then the supervisor
/// registers with a new token, the slot limits and their sources in the
/// same write, and accepts handoffs; a process exec'd by a handoff takes
/// its registration back under the same token and pid instead and writes
/// the limits again. Only after the registration it records
/// `stall_config_loaded` and then the mark `supervisor_started`, and starts
/// the heartbeat. Its runs' worktrees are made from the checkout it was
/// given, recorded as each run's `repo_path`.
pub fn supervise(ports: &Ports<'_>, settings: &LoopSettings) -> Result<Value> {
    // A supervisor that ends gives its principal's token back through the
    // service, whatever ended it, but for a handoff's exec, whose next image
    // takes it back (ADR-t3102-1 decision 2). Through the caller the loop
    // uses, as every other request of the supervisor.
    struct SignedIn(Arc<dyn crate::application::supervisor_service::ServiceCaller>);
    impl Drop for SignedIn {
        fn drop(&mut self) {
            crate::application::supervisor_service::sign_out(&*self.0);
        }
    }

    ensure!(
        settings.limits.parallel.value >= 1,
        "parallel must be at least 1"
    );
    // An owner already gone starts nothing, not even a preflight that
    // could fail and be restarted, and registers nothing; after an
    // exec the registration exists, and the loop drains it instead.
    if settings.handoff_token.is_none()
        && handoff::owner_gone(settings.owner, |pid| ports.processes.alive(pid))
    {
        info!(
            "the supervisor did not start: its owner (pid {}) is gone",
            settings.owner.unwrap_or_default()
        );
        return Ok(json!({"outcome": "owner_gone", "owner": settings.owner}));
    }
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
    let pid = layout.pid;
    // The registration this process runs under: the one it had before the
    // exec of a handoff, else a new one. The queue service answers it as
    // that registration's supervisor only (ADR-t3102-1 decisions 2 and 3),
    // so it signs in before it reads or writes anything of the queue, once
    // the service answers (ADR-t3102-3 decision 5).
    let token = settings
        .handoff_token
        .clone()
        .unwrap_or_else(|| ports.generators.ids.lease_token());
    // The image a handoff exec'd into takes back the token the one before
    // kept first, so a start that goes no further still revokes it.
    let handed = if settings.handoff_token.is_some() {
        ports.service.take_back(&token, pid)?;
        Some(SignedIn(ports.service.clone()))
    } else {
        None
    };
    let mut started = None;
    if let Some(unreached) = super::host_ops::queue_service::reach_at_start(ports, &mut started)? {
        return Ok(unreached);
    }
    // A token the image before kept that is gone is asked for anew, as a
    // first start's is, once the service answers.
    let _signed_in = match handed {
        Some(signed_in) if ports.service.credential().is_some() => signed_in,
        handed => {
            ports.service.sign_in(&token, pid)?;
            handed.unwrap_or_else(|| SignedIn(ports.service.clone()))
        }
    };
    if let Some(file) = ports.service.credential() {
        let _ = layout.service_credential.set(file);
    }
    let mut queue = ports.queues.open()?;
    // A live supervisor of the other `--no-claude` policy is neither
    // reused nor taken over: this one refuses to start until a person
    // drains it (ADR-t1204-1).
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
            // The limits and providers this process resolved, over the ones
            // the registration had before the exec, in the same write.
            queue.resume_registration(
                token,
                pid,
                settings.limits,
                &layout.providers,
                &layout.version,
            )?;
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
            // Registered before the first heartbeat so the loop is visible
            // to `status` from its first second, runs or not, with the
            // limits in use and where each comes from (ADR-0062 decision 7,
            // task 698) and the providers' executables (ADR-t813-2) in the
            // same write.
            queue.register_supervisor(
                &token,
                pid,
                settings.limits,
                &layout.providers,
                &layout.version,
            )?;
            if let Some(mode) = settings.mode {
                queue.set_supervisor_mode(&token, mode, None)?;
            }
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
    queue.set_max_load(&token, settings.max_load, Some(settings.max_load_source))?;
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
    // The heartbeat that stops sets the end's deadline at once, whatever
    // the loop does then.
    let heartbeat = {
        let (end, clock, grace) = (
            ports.end.clone(),
            ports.generators.clock.clone(),
            settings.lease_lost_grace,
        );
        Heartbeat::start_with(
            ports.queues.clone(),
            token.clone(),
            HeartbeatPolicy::supervisor(settings.heartbeat_interval),
            Some(Box::new(move || end.set_by(clock.monotonic() + grace))),
        )
    };
    let sessions = RecordingSessions::over(
        ports.sessions,
        ports.recording_queues.clone(),
        Some(token.clone()),
        ports.load_average,
    )
    .stopping_left_turns(ports.processes.clone())
    .removing_containers(ports.run_containers.clone());
    let mut supervisor = Supervisor {
        no_claude: settings.no_claude,
        service: ports.service.clone(),
        queue,
        queues: ports.queues.clone(),
        layout,
        repository: ports.repository.clone(),
        remote: ports.remote.clone(),
        verifier: ports.verifier.clone(),
        container: ports.container.clone(),
        run_containers: ports.run_containers.clone(),
        container_servers: Default::default(),
        verification_tokens: ports.verification_tokens.clone(),
        verification_containers: ports.verification_containers.clone(),
        sessions: &sessions,
        reviewer: ports.reviewer,
        codex_jobs: ports.codex_jobs,
        signals: claude.signals,
        spawner: ports.spawner,
        programs: &ports.programs,
        landing_programs: ports.landing_programs.clone(),
        service_access: ports.service_access,
        telemetry_tokens: ports.telemetry_tokens,
        environment: ports.environment.clone(),
        telemetry_sinks: ports.telemetry_sinks.clone(),
        end: ports.end.clone(),
        unsent_steps: Arc::default(),
        handoff_drain: Arc::default(),
        stop: settings.stop.clone(),
        telemetry_retries: telemetry_tokens::Retries::default(),
        telemetry_sessions: std::collections::HashSet::new(),
        pauses: ports.pauses.clone(),
        files: ports.files.clone(),
        processes: ports.processes.clone(),
        review_material: ports.review_material,
        registration: handoff::Registration::new(token, heartbeat),
        fresh_session: settings.fresh_session,
        review_jobs: settings.review_jobs,
        fresh_session_file: ports.fresh_session_file.clone(),
        fresh_session_error: None,
        replan: crate::domain::replan_detect::ReplanConfig::default(),
        replan_file: ports.replan_file.clone(),
        replan_error: None,
        replan_seen: (None, HashMap::new()),
        replan_diagnosis: None,
        replan_unrecorded: Vec::new(),
        finished: Vec::new(),
        errors: Vec::new(),
        last_turns: None,
        generators: ports.generators.clone(),
        stall: settings.stall,
        repository_startup_file: ports.repository_startup_file.clone(),
        wrapper_setting_warned: false,
        agent_eval: None,
        agent_cases: None,
        job_ends: JobEnds::default(),
        run_children: ports
            .resources
            .as_ref()
            .map(|port| port.children.clone())
            .unwrap_or_default(),
        jobs_swept: false,
        held_left: false,
        queue_hold: None,
        hold_continue: HashMap::new(),
        unanswered: reconcile::Unanswered::default(),
        utc_offset: settings.utc_offset,
        worker_plugin: ports.worker_plugin.clone(),
        claim: stages::ClaimState {
            slots: stages::SlotTable::default(),
            workers: ports
                .workers
                .workers()
                .into_iter()
                .filter(|w| !settings.no_claude || w.provider != crate::domain::Provider::Claude)
                .collect(),
            parallel: settings.limits.parallel.value,
            max_waiting: settings.limits.max_waiting.value,
            limits: settings.limits,
            slot_flags: settings.slot_flags,
            supervisor_file: ports.supervisor_file.clone(),
            supervisor_error: None,
            repository_config_missing: false,
            light_changes: settings.light_changes.clone(),
            spaced_since: None,
            claiming: true,
            provisioning_error: None,
            loads: HashMap::new(),
            defer: claim_defer::DeferWatch::default(),
            conflicts: settings.conflicts,
            slots_full: None,
            conflicts_file: ports.conflicts_file.clone(),
            conflicts_error: settings.conflicts_error.clone(),
        },
        landing: stages::LandingState {
            retry_unreadable_review: settings.retry_unreadable_review,
            ..stages::LandingState::default()
        },
        resume: stages::ResumeState {
            config: settings.resume,
            reopens: HashMap::new(),
        },
        triage: stages::TriageState::default(),
        provider: stages::ProviderState {
            fallback: settings.provider_fallback,
            fallback_file: ports.provider_fallback_file.clone(),
            fallback_error: None,
            holds: Vec::new(),
            timer_finishes_held: Vec::new(),
            moved: HashMap::new(),
        },
        e2e: e2e::E2eWaits::new(ports.run_e2e.clone()),

        notice_failures: HashMap::new(),
        planning: PlanningState::new(ports, pid),
        observation: ObservationState::new(ports),
        host: HostOpsState::new(ports, settings),
    };
    if let Some(started) = started {
        supervisor.host.started_queue_service(started);
    }
    // The records a previous supervisor kept go first, its jobs' ends among
    // them, so a job it saw end is not taken over as gone, nor a run rebuilt
    // without them (ADR-t3102-3 decision 5): while the queue stays busy for
    // them, after a wait, which a stop or an owner gone ends; what the
    // service does not answer waits for a pass, which sends it first.
    supervisor.held_left = loop {
        match supervisor.service.send_held() {
            Ok(false)
                if supervisor.service.holds_for_busy_queue()
                    && !settings.stop.load(Ordering::SeqCst)
                    && !handoff::owner_gone(settings.owner, |pid| {
                        supervisor.processes.alive(pid)
                    }) =>
            {
                info!(
                    "the records kept for the queue service go first: this supervisor starts once the queue takes them"
                );
                thread::sleep(settings.idle_poll);
            }
            Ok(all) => break !all,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the records kept for the queue service wait for the first pass: {error:#}");
                break true;
            }
        }
    };
    // Before any job starts again: the jobs a gone supervisor left, and
    // after an exec those the previous binary of this process started.
    supervisor.tend_headless_jobs();
    if settings.handoff_token.is_some() {
        // A review rebuilt under an open hold ask waits for it (task 437).
        // An answer waits for the rebuilt slots (task 754).
        supervisor.check_queue_hold(false)?;
        supervisor.take_telemetry_retries();
        supervisor.rebuild_own_runs(previous_version.as_deref())?;
    }
    // `_signed_in` goes after `supervisor`, so the tokens its jobs' handles
    // revoke as they go are revoked while it is still signed in.
    let result = supervisor.run_loop(settings);
    match &result {
        Ok(value) => info!(
            "supervisor {} exiting: {value}",
            supervisor.registration.token()
        ),
        Err(error) => {
            error!(error = %format_args!("{error:#}"), "supervisor {} failed: {error:#}", supervisor.registration.token())
        }
    }
    // The image this process execs into takes the token back, so a job
    // command started with it goes on recording across the exec.
    drop(supervisor);
    if result
        .as_ref()
        .is_ok_and(|value| value["outcome"] == "handoff")
    {
        ports.service.hand_over();
    }
    result
}

/// How many times the end of the loop sends the records it holds for a
/// queue service that does not answer, a poll apart.
const HELD_RECORD_TRIES: u32 = 15;

/// A listing of this user's processes with the wall time it was taken.
type ProcessSample = (SystemTime, Vec<crate::domain::recovery::ProcessInfo>);

/// The supervisor's loop, with each context's state apart. Which context
/// owns each field, so that a submodule changes only its own context's
/// state and reads or calls the others (rule C3 of
/// docs/design/architecture.md):
///
/// - shared: `queue`, `queues`, `generators`, `layout`, `processes` and
///   `utc_offset`, the connection, clocks and places every context reads,
///   and the adapters and flags every context reads and none changes
///   (`sessions`, `service_access`, `no_claude`).
/// - planning: `planning` ([`PlanningState`]), changed only by
///   its submodules through their passes.
/// - execution and landing: by stage ([`stages`]), the slot assignment
///   `claim` (its slots change only through [`stages::SlotTable`]'s
///   operations), `landing`, `resume`, `triage`, `provider` and `e2e`
///   ([`e2e::E2eWaits`]), each changed by its stage's submodules; and on
///   the loop, the steps of the slots' phases (each slot's [`Phase`] is
///   held in the [`stages::SlotTable`]) and what spans the stages
///   (`finished`, `errors`, `stall`, `notice_failures`, `job_ends`,
///   `jobs_swept`, `last_turns`, `fresh_session*`, `review_jobs`,
///   `agent_eval`,
///   `queue_hold`, `hold_continue`, `review_material`, `wrapper_setting_warned`), with
///   the adapters it uses (`repository`, `remote`, `verifier`, `reviewer`,
///   `codex_jobs`, `signals`, `spawner`, `files`). The providers' holds and
///   `queue_hold` are lent to 計画管理's jobs through [`JobDesk`].
/// - observation and analysis: `observation`
///   ([`ObservationState`]), changed only by its submodules
///   through their passes.
/// - host operation: `host` ([`HostOpsState`], the sweep and the
///   load among it), changed only by its submodules through their passes,
///   and `registration` ([`handoff::Registration`]), changed only by its
///   own operations, which the loop calls on its steps of a handoff and a
///   stop.
///
/// The fields of `Supervisor` and of 実行と着地's stages are private to
/// this module, and those of `planning`, `observation`, `host` and
/// `registration` to their context's module, so the compiler holds the
/// rule.
struct Supervisor<'a> {
    no_claude: bool,
    /// How the queues reach the service, which holds the records of results
    /// it did not answer until the next pass sends them.
    service: Arc<dyn crate::application::supervisor_service::ServiceCaller>,
    queue: Box<dyn Queue + Send>,
    /// A connection for each thread beside the loop.
    queues: Arc<dyn QueueOpener>,
    layout: &'a Layout,
    repository: Arc<dyn Repository + Send + Sync>,
    remote: Arc<dyn MainRemote + Send + Sync>,
    verifier: Arc<dyn Verifier + Send + Sync>,
    /// Whether the container runtime can be used ([`Ports::container`]).
    container: Arc<dyn crate::application::ContainerRuntime + Send + Sync>,
    /// The runs' containers ([`Ports::run_containers`]).
    run_containers: Arc<dyn crate::application::run_container::RunContainers>,
    /// The answers to the requests of the runs' containers for their
    /// sccache server under way.
    container_servers: container_sccache::ContainerServers,
    /// The tokens of the landings' verifications
    /// ([`Ports::verification_tokens`]).
    verification_tokens: Arc<dyn crate::application::queue_broker::VerificationTokens>,
    /// The containers of the landings' verifications
    /// ([`Ports::verification_containers`]).
    verification_containers: Arc<dyn crate::application::run_container::VerificationContainers>,
    sessions: &'a dyn SessionWrappers,
    /// Starts the headless review of accepted runs (ADR-0027).
    reviewer: &'a dyn AgentProvider,
    /// Starts the headless jobs a role puts on Codex (ADR-t1063-1).
    codex_jobs: Option<&'a dyn AgentProvider>,
    /// Reads the idle marker of the run sessions, and the screen of the
    /// inbox (no worker screen since task 1437, no planner's screen since
    /// ADR-t1433-2).
    signals: &'a dyn AgentSignals,
    spawner: &'a dyn Spawner,
    /// Where the program jobs of a review run ([`Ports::programs`]).
    programs: &'a crate::application::review_programs::ProgramBackends<'a>,
    /// What the landings run their program reviews with
    /// ([`Ports::landing_programs`]).
    landing_programs: crate::application::review_programs::ProgramHost,
    service_access: &'a dyn crate::application::queue_service::ServiceAccess,
    /// The runs' executors' telemetry tokens ([`Ports::telemetry_tokens`]).
    telemetry_tokens: &'a dyn crate::application::queue_service::TelemetryTokens,
    /// The environment the runs execute in ([`Ports::environment`]).
    environment: Option<Arc<dyn crate::application::ExecutionEnvironment>>,
    /// Where the verifications send their commands' steps
    /// ([`Ports::telemetry_sinks`]).
    telemetry_sinks: Arc<dyn crate::application::TelemetrySinks>,
    /// The deadline of the end once the heartbeat stopped ([`Ports::end`]).
    end: Arc<EndDeadline>,
    /// The steps its executors (the landings' and the e2e's verifications,
    /// its headless jobs) could not send by their end: sent again on each
    /// sweep while their target has its token ([`Self::sweep_telemetry`]).
    unsent_steps: Arc<crate::application::step_report::UnsentSteps>,
    /// Whether this process drains for a handoff, a stop's drain not
    /// (ADR-t3344-1): set on each pass, and read by a landing in progress
    /// before it verifies again, which hands the run over instead.
    handoff_drain: Arc<AtomicBool>,
    /// The stop of [`LoopSettings::stop`], read by a landing in progress
    /// with [`Self::handoff_drain`]: a stop that came after the pass set
    /// the handoff's flag still wins (ADR-t3344-1 decision 3).
    stop: Arc<AtomicBool>,
    /// The revocations of the runs' telemetry tokens this process could
    /// not make yet and no sweep would make for it: tried again on each
    /// sweep ([`Self::sweep_telemetry`]).
    telemetry_retries: telemetry_tokens::Retries,
    /// The runs' sessions this process registered to their run
    /// ([`telemetry_tokens::register_run_sessions`]), so each is asked of
    /// the queue once.
    telemetry_sessions: std::collections::HashSet<String>,
    /// Where tests pause it ([`Ports::pauses`]).
    pauses: Option<Pauses>,
    files: Arc<dyn RunFiles>,
    processes: Arc<dyn ProcessControl + Send + Sync>,
    review_material:
        &'a dyn Fn(TaskId, Option<&crate::application::review::ReviewRange>) -> Result<Value>,
    /// host運用's registration of this process, its handoff and its stop.
    registration: handoff::Registration,
    /// `[fresh_session]` as last read (ADR-t2080-1): past what context a
    /// worker's send-back or resume starts a new session.
    fresh_session: crate::domain::fresh_session::FreshSessionConfig,
    /// `[review.jobs]` as read at the start: the timeout of each kind of
    /// job of a run's review ([`Self::job_timeout`]).
    review_jobs: crate::domain::headless_job::ReviewJobs,
    /// Reads `[fresh_session]` again each pass; `None` keeps
    /// `fresh_session`.
    fresh_session_file: Option<FreshSessionFile>,
    /// The error the last read of `[fresh_session]` failed with, warned of
    /// once until it changes or a read succeeds.
    fresh_session_error: Option<String>,
    /// `[replan]` as last read: whether the runtime detects candidates
    /// and diagnoses the replanning operations, and its limits.
    replan: crate::domain::replan_detect::ReplanConfig,
    /// Reads `[replan]` again each pass; `None` keeps `replan`.
    replan_file: Option<ReplanFile>,
    /// The error the last read of `[replan]` failed with, warned of once.
    replan_error: Option<String>,
    /// Per task, the newest of its events the detection counts that it
    /// looked at, under the `[replan]` it read then: a task is looked at
    /// again only once a newer one comes, or the settings change.
    replan_seen: (
        Option<crate::domain::replan_detect::ReplanConfig>,
        HashMap<TaskId, EventId>,
    ),
    /// The diagnosis job running now (one at a time, queue-wide).
    replan_diagnosis: Option<Box<replan_diagnosis::DiagnosisJob>>,
    /// The records of diagnosis jobs' ends that could not be written,
    /// written again at each pass.
    replan_unrecorded: Vec<replan_diagnosis::UnrecordedRecord>,
    /// The runs that came to rest in this process's slots (a step ended
    /// them in a status the loop no longer drives); the result's `runs`.
    finished: Vec<TaskRun>,
    /// The runs this process gave up on; the result's `errors`.
    errors: Vec<RunError>,
    /// When this process last recorded the transcript turns of the open
    /// session spans (ADR-0048 decision 8); `None` until the first pass.
    last_turns: Option<Instant>,
    /// The clock and IDs `queue` also uses.
    generators: Generators,
    /// The host's time zone ([`LoopSettings::utc_offset`]): the local day
    /// the e2e marks of a worker's prompt are judged on.
    utc_offset: fn(i64) -> i64,
    /// The thresholds of the stalled-session checks (ADR-0043 decision 4).
    stall: StallConfig,
    repository_startup_file: Option<super::repository_config::RepositoryStartupFile>,
    /// Whether `[headless] wrapper = "workspace"`, which a worker ignores,
    /// was warned of (ADR-t1433-3 decision 2).
    wrapper_setting_warned: bool,
    /// The round of the eval of an agent this process runs (ADR-t1728-1
    /// decision 9): one at a time, queue-wide, outside the run slots.
    agent_eval: Option<agent_eval::EvalRound>,
    /// The production set of an agent's cases this process labels
    /// (ADR-t1728-1 (g)): one at a time, queue-wide, outside the run slots.
    agent_cases: Option<agent_cases::CaseLabeling>,
    /// The ends of this process's headless jobs still to be written: the
    /// outcomes of their `headless_jobs` rows (task 443), and the jobs
    /// abandoned, written as `headless_job_stopped` with their Execution.
    job_ends: JobEnds,
    /// The processes this process started for its runs outside their
    /// workers' trees (their headless jobs), which the measure of the runs'
    /// resources counts with them; those of the measure's port when it
    /// measures.
    run_children: crate::domain::environment::RunChildren,
    /// Whether this process looked for the jobs a gone supervisor left,
    /// its own token's included (after an exec), already.
    jobs_swept: bool,
    /// Whether records the queue service did not take are still kept, as
    /// the last [`ServiceCaller::send_held`] said: a kept job's end among
    /// them, no job is taken over as gone meanwhile.
    ///
    /// [`ServiceCaller::send_held`]: crate::application::supervisor_service::ServiceCaller::send_held
    held_left: bool,
    /// The open authentication or usage-limit ask read at the top of this
    /// pass (task 437): no new run is claimed and no headless job starts
    /// while it holds.
    queue_hold: Option<claim_hold::QueueHold>,
    /// The held runs whose session gets the fixed text to go on, with the
    /// ask a person answered `done` (task 437).
    hold_continue: HashMap<RunId, AskId>,
    /// What the queue service may have done without its answer reaching
    /// this process, judged against the queue once it answers again.
    unanswered: reconcile::Unanswered,
    worker_plugin: Arc<dyn Fn(&Path) -> String + Send + Sync>,
    /// The notices of a question closed without its answer that could not
    /// be sent, by run: tried again a bounded number of times (task 1372).
    notice_failures: HashMap<RunId, stall::NoticeFailure>,
    /// 実行と着地's state, by stage ([`stages`]).
    claim: stages::ClaimState,
    landing: stages::LandingState,
    resume: stages::ResumeState,
    triage: stages::TriageState,
    provider: stages::ProviderState,
    /// The e2e stage's state: its port, the runs waiting for the e2e, and
    /// when one that could not run is tried again.
    e2e: e2e::E2eWaits,
    /// 計画管理's state.
    planning: PlanningState,
    /// 観測と分析's state.
    observation: ObservationState,
    /// host運用's state.
    host: HostOpsState,
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
    /// In the landing turn, the run its last look found holding the
    /// landing slot, which its record of the landing queue names.
    blocked_by: Option<RunId>,
    /// What the slot keeps of its run's recorded phases.
    track: stages::PhaseTrack,
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
    /// The program reviews of an accepted run, one program job at a time
    /// before its agents' review (ADR-t1895-2); its session stays open.
    ReviewPrograms(ProgramsWatch),
    /// The headless review of an accepted run (ADR-0023 decision 2).
    Review(ReviewWatch),
    /// An accepted run whose review waits as a whole: none of its jobs can
    /// start, its whole review's waiting for the authentication or
    /// usage-limit ask that holds the queue's jobs (task 437) or for a held
    /// provider, and so each agent's that names its own provider. Its
    /// session stays open, and the review starts once one of them can.
    /// It comes only after the program reviews all passed: they use no
    /// provider and run through the hold, and do not run again when the
    /// agents' review starts (a handoff or an adoption that rebuilds an
    /// awaiting run with no review runs them again). It writes no
    /// `review_started` while it waits, and its wait adds it to no hold
    /// ask's `affected`: `cancel_affected` gives it up only when the ask
    /// listed the run already (`queue_hold::hold_outcome`).
    /// `retried` is
    /// whether the review that waits is the one retry of the review
    /// ([`ReviewWatch::retried`]): it stays one across the wait.
    ReviewHeld {
        session: Option<SessionRef>,
        retried: bool,
        /// The providers its selected agents name, read when it was held:
        /// the wait ends once the job of one of them can start, as well as
        /// once its whole review's can.
        providers: Vec<Provider>,
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
    /// Missing run.env programs keep the lease here without taking the
    /// integration slot; a later pass finding them starts landing. Drain or
    /// stopped claims release the lease instead of waiting, for adoption;
    /// handoff retains it for the next process. An approve_landing land
    /// answer still closes its ask and queues the run immediately; only
    /// start_approved_landings waits for the programs.
    AwaitingSlot,
    /// A passed run that changes an agent's definition waits, its session
    /// open and its lease and slot kept, for the dev of the landing
    /// branch's cases before its precheck (ADR-t1728-1 decision 10).
    AgentEval(eval_gate::EvalGateWatch),
    /// A passed run waits for its e2e to start (ADR-t1233-2): another
    /// run's e2e runs, one that could not run waits to be tried again, or a
    /// handoff starts none. Like `AwaitingSlot`, a handoff need not wait
    /// for it: the next process rebuilds the run as an adopted one, which
    /// waits for the slot again.
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
    /// The verification of an `already_satisfied` claim on a snapshot of
    /// main, off the loop (ADR-t1818-1); the run stays `validating` and
    /// keeps its lease and slot until it ends.
    Satisfied(satisfied::SatisfiedWatch),
    /// The review's judgment of that claim on the snapshot whose
    /// verification passed, one headless job (ADR-t1818-1 decision 4); the
    /// run stays `validating` and keeps its lease and slot until the job
    /// ended and its ask opened.
    SatisfiedReview(satisfied_review::ClaimReviewWatch),
}

/// Whether a slot's run in `phase` is offered to a cleanup for disk space
/// as one waiting for the landing slot: only in `AwaitingSlot`, in its
/// slot, whose landing, e2e, review, resume and session are not running
/// (each a phase of its own), while no landing recheck runs (`recheck`)
/// and the process does not drain (`draining`).
fn offered_for_room(phase: &Phase, out_of_slot: bool, recheck: bool, draining: bool) -> bool {
    matches!(phase, Phase::AwaitingSlot) && !out_of_slot && !recheck && !draining
}

impl Phase {
    /// Whether this phase holds work in flight that a rebuild from the
    /// queue would start again (a validation, an e2e, a verification, a
    /// landing, a review's, a claim judgment's or a recovery's job, the
    /// eval's rounds): a step of it that did nothing on the queue keeps it
    /// ([`reconcile`]). Each of their steps reads what it needs before it
    /// takes the work's result out of the slot, so a read the service does
    /// not answer leaves the result in the slot for the next look.
    fn holds_work(&self) -> bool {
        match self {
            Phase::Validating(handle, _) => handle.is_some(),
            Phase::Satisfied(watch) => watch.handle.is_some(),
            Phase::E2e(_)
            | Phase::Landing(_)
            | Phase::Review(_)
            | Phase::ReviewPrograms(_)
            | Phase::SatisfiedReview(_)
            | Phase::Recovery(_)
            | Phase::AgentEval(_) => true,
            Phase::Session(_)
            | Phase::ReviewHeld { .. }
            | Phase::Revise(_)
            | Phase::Exiting(_)
            | Phase::Resume(_)
            | Phase::AwaitingSlot
            | Phase::AwaitingE2e => false,
        }
    }
    /// The phase `run_phase_changed` records for this one, whose tags are
    /// [`crate::domain::run_phase::Phase::tags`]: a phase added here does
    /// not compile until it names one.
    fn recorded(&self) -> crate::domain::run_phase::Phase {
        use crate::domain::run_phase::Phase as Recorded;
        match self {
            // A first session, a revise or a resume stopped at its own
            // question waits for a person in its slot when the waiting
            // limit keeps it there.
            Phase::Session(watch) => stages::in_slot_wait(Recorded::Worker, watch.awaits_answer),
            Phase::Validating(..) => Recorded::Validating,
            // The judgment is the claim's step after its verification,
            // not a run review's.
            Phase::Satisfied(_) | Phase::SatisfiedReview(_) => Recorded::Verifying,
            Phase::ReviewPrograms(_) | Phase::Review(_) => Recorded::Review,
            Phase::ReviewHeld { .. } => Recorded::ReviewHeld,
            Phase::Revise(watch) => stages::in_slot_wait(Recorded::Revise, watch.awaits_answer),
            Phase::Exiting(_) => Recorded::Exiting,
            Phase::Resume(watch) => stages::in_slot_wait(Recorded::Resume, watch.awaits_answer),
            Phase::AwaitingSlot => Recorded::AwaitingSlot,
            Phase::AgentEval(_) => Recorded::AgentEval,
            Phase::AwaitingE2e => Recorded::AwaitingE2e,
            Phase::E2e(_) => Recorded::E2e,
            Phase::Landing(_) => Recorded::Landing,
            Phase::Recovery(_) => Recorded::Recovery,
        }
    }

    /// The attempt this phase starts: a revise, a conflict request or a
    /// resume, by its number; the other phases go on in the run's attempt.
    fn attempt(&self) -> Option<crate::domain::run_phase::Attempt> {
        use crate::domain::run_phase::{Attempt, AttemptKind};
        match self {
            Phase::Revise(watch) => Some(Attempt::of(
                match watch.fix {
                    Fix::Revise { .. } => AttemptKind::Revise,
                    Fix::Conflict(_) => AttemptKind::Conflict,
                },
                watch.attempt,
            )),
            Phase::Resume(watch) => Some(Attempt::of(AttemptKind::Resume, watch.attempt)),
            _ => None,
        }
    }
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
///
/// verdictを適用したeventはactorがsupervisor、`requested_by`がjobのactor id。
///   reviewの`concern`と差し戻せない`revise`（上限の後に収束していない・sessionが終わっていた・切り替えられなかった・送れなかった）と、`pass`の後の衝突の事前検査が上限で人に聞く`approve_landing`のaskは、sessionの終了の後に開くが、`AfterExit::Ask`がverdictを返したreview jobのactor（`requested_by`）を持ち、askを開くときだけ`for_job`で包むので、その`ask_opened`もactorがsupervisor、`requested_by`が`review-job:<run>:<attempt>`になる。
///   askの後にleaseを返すなどのeventは`for_job`の外で、`requested_by`を持たない。
///   supervisorの引き継ぎ（`adopt.rs`）が履歴から`AfterExit::Ask`を組み立て直すときは、引き継ぐ（か直前の）`review_finished`のpayloadの`attempt`から同じjobを復元し、`attempt`が無ければ別のjobを名指ししないよう`requested_by`を付けない。
///   引き継いだpassのrunの衝突の事前検査も同じjobの`for_job`の中で行うので、その`conflict_precheck`も生きている経路と同じく`requested_by`を持つ。
///   差し戻しや衝突の依頼を生きているsessionが果たさなかった後（sessionが終わった・receiptの直しを送れなかった）のaskは、verdictを適用し終えた後のsupervisor自身の判断なので`requested_by`を持たない。
///
/// jobが開くaskの`asked_by`（`plan_review`・`goal_review`など）は変えない。
///
/// 判定の適用:
///
/// - `pass`: 衝突の事前検査のあと、sessionの終了を待って着地の段（`AfterExit::Land`）へ進めるだけ。着地の関数は呼ばない。着地は別のthreadの`land_integrating`が、rebase後の検証コマンドを含む自分の検査を通したときだけ行い、落ちればrunは`needs_session`になる
/// - `revise` / `concern`: 生きているsessionへの差し戻し（sessionが終わっていれば`approve_landing`のask） / `approve_landing`のask。runを通した`revise`の上限の後の差し戻しは収束の判定（`revise_limit_judged`）に通し、コードの指摘が無ければaskを開かずにpassの道で着地して理由をfollow_upのdraftにし、あれば`approve_landing`のask（`docs/design/supervisor-lifecycle/review.md`の「Review」）
///
/// 読めないreviewの出力は答えの無いjobだけを同じ入力で1回やり直し
/// （`review_job_finished`）、それも無ければ`review_failed`と`approve_landing`のask。
/// 失敗のeventはjobの依頼ではなく、payloadの`attempt`でjobを識別する。
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
    /// Verify the run's `already_satisfied` claim on main (ADR-t1818-1).
    Satisfied,
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
    /// `[roles.*]` of `dagq.toml` as read now, or none (every role started
    /// as before) when it cannot be read, warned of for `role`.
    pub(in crate::application) fn role_models(
        &self,
        role: crate::domain::actor_model::ModelRole,
    ) -> crate::domain::actor_model::RoleModels {
        role_models_of(&*self.verifier, role)
    }

    /// The loop's shared parts and adapters ([`PassEnv`]), borrowed apart
    /// from 観測と分析's and host運用's states, with those states.
    fn split(&mut self) -> (PassEnv<'_>, &mut ObservationState, &mut HostOpsState) {
        (
            PassEnv {
                queue: &mut *self.queue,
                generators: &self.generators,
                layout: self.layout,
                processes: &self.processes,
                files: &self.files,
                repository: &self.repository,
                verifier: &self.verifier,
                spawner: self.spawner,
                sessions: self.sessions,
                token: self.registration.token(),
            },
            &mut self.observation,
            &mut self.host,
        )
    }

    /// Run `pass` of 観測と分析 on its state.
    fn on_observation<R>(
        &mut self,
        pass: impl FnOnce(&mut ObservationState, &mut ObservationEnv<'_>) -> R,
    ) -> R {
        let (env, observation, _) = self.split();
        pass(observation, &mut env.observation())
    }

    /// Run `pass` of host運用 on its state.
    fn on_host<R>(&mut self, pass: impl FnOnce(&mut HostOpsState, &mut HostEnv<'_>) -> R) -> R {
        let (env, _, host) = self.split();
        pass(host, &mut env.host())
    }

    /// Run `pass` of 計画管理 on its state, with 実行と着地's jobs and
    /// providers ([`JobDesk`]), the claim's expected files and the limits
    /// the loop read for it ([`contexts::PlanningEnv`]).
    fn on_planning<R>(
        &mut self,
        pass: impl FnOnce(&mut PlanningState, &mut contexts::PlanningEnv<'_>) -> R,
    ) -> R {
        let mut env = contexts::PlanningEnv {
            pass: PassEnv {
                queue: &mut *self.queue,
                generators: &self.generators,
                layout: self.layout,
                processes: &self.processes,
                files: &self.files,
                repository: &self.repository,
                verifier: &self.verifier,
                spawner: self.spawner,
                sessions: self.sessions,
                token: self.registration.token(),
            },
            jobs: JobDesk {
                reviewer: self.reviewer,
                codex_jobs: self.codex_jobs,
                signals: self.signals,
                service_access: self.service_access,
                telemetry_tokens: self.telemetry_tokens,
                telemetry_sinks: self.telemetry_sinks.clone(),
                unsent_steps: &self.unsent_steps,
                no_claude: self.no_claude,
                job_ends: &self.job_ends,
                run_children: &self.run_children,
                review_jobs: self.review_jobs,
                provider: &mut self.provider,
                queue_hold: &mut self.queue_hold,
            },
            defer: &mut self.claim.defer,
            runtime_planners: self.claim.limits.runtime_planners.value,
            conflicts: self.claim.conflicts,
            turn_limits: self.stall.turn_limits(),
        };
        pass(&mut self.planning, &mut env)
    }

    /// Run `call` with the loop's shared parts only.
    fn with_env<R>(&mut self, call: impl FnOnce(&mut PassEnv<'_>) -> R) -> R {
        let (mut env, _, _) = self.split();
        call(&mut env)
    }

    /// host運用's look at the sccache server before a process given
    /// `[run.env]` starts ([`HostOpsState::ensure_sccache`]).
    pub(in crate::application) fn ensure_sccache(
        &mut self,
        reason: crate::domain::sccache::CheckReason,
    ) -> crate::domain::sccache::ServerCheck {
        self.on_host(|host, env| host.ensure_sccache(env, reason))
    }

    /// The look and the guard in `dir` before a process given `[run.env]`
    /// starts ([`HostOpsState::sccache_look`]).
    pub(in crate::application) fn sccache_look(
        &mut self,
        reason: crate::domain::sccache::CheckReason,
        dir: &Path,
    ) -> crate::domain::sccache::GuardLook {
        self.on_host(|host, env| host.sccache_look(env, reason, dir))
    }

    /// Record `sccache_wrapper_removed` on `run` ([`sccache::record_wrapper_removed`]).
    pub(in crate::application) fn record_wrapper_removed(
        &mut self,
        run: &RunId,
        look: &crate::domain::sccache::GuardLook,
        fields: Value,
    ) {
        self.with_env(|env| sccache::record_wrapper_removed(env, run, look, fields));
    }

    /// The load over the run's interval that ends now, and start the next.
    pub(in crate::application) fn take_load(&mut self, id: &RunId) -> LoadSummary {
        self.claim.take_load(id)
    }
    /// Apply what the headless job `job` returned: the events written
    /// meanwhile record the supervisor as their actor and the job as
    /// `requested_by` (ADR-t728-1 decision 1, task 730). On the way out,
    /// whether `apply` failed or not, `requested_by` goes back to what it
    /// was, so a nested `for_job` leaves the outer job's (task 783).
    /// 再計画の診断verdictもこの範囲で適用する。継続・保留・ask、前提が古く捨てた
    /// `replan_diagnosed`を含め、`requested_by`は
    /// `recovery-job:<run>:replan_diagnosis:<attempt>`。
    /// jobの失敗、jobを使い切ったask、人の回答の適用はjobの依頼ではなく、
    /// `requested_by`を持たない。review passの後の着地はIntegratorが行い、依頼元は
    /// supervisorである。Integratorも抜けるときに入る前の依頼元へ戻す。
    pub(in crate::application) fn for_job<T>(
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
    pub(in crate::application) fn close_review_session(
        &mut self,
        run: &TaskRun,
        session: Option<&crate::domain::headless_job::JobSession>,
    ) {
        if let Err(error) = self.queue.close_review_session(run.id(), session) {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: the span of its failed review could not be closed: {error:#}", run.id());
        }
    }
    /// Kill the observer still running and the processes it started;
    /// `why` ends the log line.
    pub(in crate::application) fn stop_observer(&mut self, why: &str) {
        self.on_observation(|observation, env| observation.stop_observer(env, why));
    }
    /// Record the phase of run `id` as it is now that no slot holds it
    /// ([`stages::record_rest`]).
    pub(in crate::application) fn record_rest(
        &self,
        id: &RunId,
        attempt: Option<run_phase::Attempt>,
        cause: &str,
    ) {
        match self.queue.run(id) {
            Ok(run) => {
                stages::record_rest(
                    &*self.queue,
                    &run,
                    attempt,
                    cause,
                    Some(&self.layout.version),
                );
            }
            Err(error) => {
                warn!(run_id = %id, error = %format_args!("{error:#}"), "run {id}: could not record its phase: {error:#}");
            }
        }
    }
}

/// Register with `register` the sessions the agents of `slot`'s running
/// jobs named in their output so far ([`HeadlessJob::register_session`]):
/// its review's, its recovery's and its claim review's.
fn register_slot_sessions(
    slot: &mut Slot,
    register: &mut dyn FnMut(&crate::domain::session_step::StepTarget, &str) -> bool,
) {
    match &mut slot.phase {
        Phase::Review(watch) => watch.register_sessions(register),
        Phase::Recovery(watch) => watch.job.register_session(register),
        Phase::SatisfiedReview(watch) => watch.register_session(register),
        Phase::Session(watch) => watch.recovery.register_session(register),
        Phase::Resume(watch) => {
            watch.recovery.register_session(register);
            watch.live.recovery.register_session(register);
        }
        Phase::Revise(watch) => watch.live.recovery.register_session(register),
        _ => {}
    }
}

/// Kill the headless job (a review or a recovery job) of a slot the
/// supervisor stops watching, recording its Execution
/// ([`HeadlessJob::abandon`]).
fn stop_job(slot: &mut Slot) {
    match &mut slot.phase {
        Phase::Review(watch) => watch.abandon(),
        Phase::ReviewPrograms(watch) => watch.job.abandon(),
        Phase::Recovery(watch) => watch.job.abandon(),
        Phase::SatisfiedReview(watch) => watch.abandon(),
        _ => stop_recovery(slot),
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
/// unreadable counts as used up, but not a read the queue service did not
/// answer, which is the caller's error.
fn resumes_exhausted(queue: &dyn Queue, run_id: &RunId, config: ResumeConfig) -> Result<bool> {
    match queue.run_events(run_id) {
        Ok(events) => Ok(ResumeCount::of(&events).exhausted(config)),
        Err(error) if crate::application::supervisor_service::unreachable(&error) => Err(error),
        Err(_) => Ok(true),
    }
}

/// Complete `finished`, the payload of the `resume_finished` of an
/// unresolved resume of `run_id`: `exhausted`, whether the run's resumes
/// are used up once it ends so (ADR-0047 decision 24: its counted resumes,
/// its conflict-only ones up to `config`'s limit, its provider-only ones),
/// and, for a provider-only resume, `counted: false` with the payload of
/// the `auto_repaired` that records it (ADR-t3255-1 decision 2,
/// [`crate::domain::resume::finish_provider_only`]). Unreadable events count as used up,
/// and as no provider-only resume, but not a read the queue service did
/// not answer, which is the caller's error.
fn finish_unresolved(
    queue: &dyn Queue,
    run_id: &RunId,
    config: ResumeConfig,
    finished: &mut Value,
) -> Result<Option<Value>> {
    match queue.run_events(run_id) {
        Ok(events) => {
            let repair = crate::domain::resume::finish_provider_only(&events, finished);
            finished["exhausted"] =
                json!(ResumeCount::after_finish(&events, finished).exhausted(config));
            Ok(repair)
        }
        Err(error) if crate::application::supervisor_service::unreachable(&error) => Err(error),
        Err(_) => {
            finished["exhausted"] = json!(true);
            Ok(None)
        }
    }
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

/// The findings of the run's latest review a `send_back` answer names:
/// those of a `review_finished` ([`review_findings`]), or the error of a
/// `review_failed` after it.
fn latest_review_reasons(queue: &dyn Queue, run_id: &RunId) -> Result<Vec<String>> {
    Ok(queue
        .run_events(run_id)?
        .into_iter()
        .rev()
        .find(|e| {
            matches!(
                e.kind.known(),
                Some(EventKind::ReviewFinished | EventKind::ReviewFailed)
            )
        })
        .and_then(|e| match e.kind.known() {
            Some(EventKind::ReviewFailed) => e.payload["error"]
                .as_str()
                .map(|error| vec![format!("the headless review failed: {error}")]),
            _ => review_findings(&e.payload),
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
        // A run a recovery round handed off (an adopted one) is a claim its
        // round already checked, whose receipt names no claim: it is
        // accepted as one again, not judged as the worker wrote it
        // (ADR-t1818-2 decision 3).
        if crate::domain::satisfied::handed_off_since_resume(&queue.run_events(run.id())?) {
            let receipt = files
                .read_to_string(Path::new(run.receipt_path().unwrap_or_default()))
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or(Value::Null);
            return Ok(Validation {
                accepted: true,
                result_commit: None,
                reason: None,
                code: None,
                receipt,
                evidence_missing: Vec::new(),
                spike_result_missing: Vec::new(),
                scope_violation: Vec::new(),
                allowed_paths: Vec::new(),
                e2e_requirement: None,
                already_satisfied: true,
                load: LoadSummary::default(),
            });
        }
        let task = queue.show(run.task_id())?.task;
        let checked = check_receipt(&*repository, &*files, &task, &run, &e2e_paths)?;
        Ok(match checked {
            Ok(accepted) => Validation {
                accepted: true,
                // An `already_satisfied` claim has no commit to land.
                result_commit: (!accepted.already_satisfied).then_some(accepted.commit),
                reason: None,
                code: None,
                receipt: serde_json::to_value(accepted.receipt)?,
                evidence_missing: Vec::new(),
                spike_result_missing: Vec::new(),
                scope_violation: Vec::new(),
                allowed_paths: Vec::new(),
                e2e_requirement: Some(accepted.e2e_requirement),
                already_satisfied: accepted.already_satisfied,
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
                    spike_result_missing: rejection.spike_result_missing,
                    allowed_paths: if rejection.scope_violation.is_empty() {
                        Vec::new()
                    } else {
                        task.paths().to_vec()
                    },
                    scope_violation: rejection.scope_violation,
                    e2e_requirement: rejection.e2e_requirement,
                    already_satisfied: false,
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

/// `[roles.*]` of `dagq.toml` as `verifier` reads it now, or none (every
/// role started as before) when it cannot be read, warned of for `role`.
pub(super) fn role_models_of(
    verifier: &dyn Verifier,
    role: crate::domain::actor_model::ModelRole,
) -> crate::domain::actor_model::RoleModels {
    verifier.role_models().unwrap_or_else(|error| {
        warn!(error = %format_args!("{error:#}"), "[roles.{}] could not be read; starting it as before: {error:#}", role.as_str());
        crate::domain::actor_model::RoleModels::default()
    })
}

/// Run `apply` on `state` with `job` as the `requested_by` of the events
/// its queue (`log`) writes, and put back the `requested_by` it replaced
/// afterwards, whatever `apply` returned (task 783).
pub(super) fn requested_by_job<S: ?Sized, T>(
    state: &mut S,
    log: impl Fn(&S) -> &dyn EventStore,
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
    use crate::domain::{EventFilter, actor::ActorContext};

    /// Only a run in `AwaitingSlot`, in its slot, is offered to a cleanup
    /// for disk space, and none while a landing recheck runs or the
    /// process drains: one landing, running or waiting for its e2e,
    /// validating or with its review held is not.
    #[test]
    fn only_a_run_idle_in_awaiting_slot_is_offered_for_room() {
        assert!(offered_for_room(&Phase::AwaitingSlot, false, false, false));
        assert!(!offered_for_room(&Phase::AwaitingSlot, true, false, false));
        assert!(!offered_for_room(&Phase::AwaitingSlot, false, true, false));
        assert!(!offered_for_room(&Phase::AwaitingSlot, false, false, true));
        for phase in [
            Phase::Landing(None),
            Phase::AwaitingE2e,
            Phase::Validating(None, None),
            Phase::ReviewHeld {
                session: None,
                retried: false,
                providers: Vec::new(),
            },
        ] {
            assert!(!offered_for_room(&phase, false, false, false));
        }
    }

    /// A service that answers each call with `answer`'s error: lost after
    /// it read the request, or refused.
    struct Answers(fn() -> anyhow::Error);

    impl crate::application::supervisor_service::ServiceCaller for Answers {
        fn call(&self, _: crate::domain::queue_service::UseCase, _: Value) -> Result<Value> {
            Err((self.0)())
        }
        fn with_token(
            &self,
            _: &std::path::Path,
        ) -> Result<Arc<dyn crate::application::supervisor_service::ServiceCaller>> {
            anyhow::bail!("no landing here")
        }
        fn sign_in(&self, _: &LeaseToken, _: u32) -> Result<()> {
            Ok(())
        }
        fn take_back(&self, _: &LeaseToken, _: u32) -> Result<()> {
            Ok(())
        }
        fn hand_over(&self) {}
        fn sign_out(&self) {}
        fn credential(&self) -> Option<std::path::PathBuf> {
            None
        }
    }

    /// A count of the resumes the queue service did not answer is the
    /// caller's error, so a repair is not refused on it nor a resume's end
    /// recorded as the last; one that failed otherwise counts as used up.
    #[test]
    fn resumes_unanswered_are_not_taken_as_used_up() {
        let exhausted = |answer: fn() -> anyhow::Error| {
            let queue = crate::application::supervisor_service::SupervisorQueues::new(
                Arc::new(Answers(answer)),
                ActorContext::instance(crate::domain::actor::ActorRole::Supervisor, 1),
            )
            .open()
            .unwrap();
            resumes_exhausted(&*queue, &RunId::new("r1").unwrap(), ResumeConfig::default())
        };
        let lost = exhausted(|| {
            crate::application::supervisor_service::ServiceUnreachable::maybe_done("lost").into()
        })
        .unwrap_err();
        assert!(crate::application::supervisor_service::unreachable(&lost));
        assert!(exhausted(|| anyhow::anyhow!("refused")).unwrap());
    }

    /// An event store that keeps the requester [`EventStore::request_as`] sets, the
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
    impl EventStore for Requests {
        fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn all_events(&self) -> Result<Vec<RunEvent>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
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
        fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
            unreachable!("requested_by_job writes queue events only")
        }
        fn latest_event_id(&self) -> Result<EventId> {
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
        fn events_between(
            &self,
            after: EventId,
            upto: EventId,
            filter: &EventFilter,
            limit: usize,
        ) -> Result<Vec<RunEvent>> {
            unreachable!("events_between is not what the test reaches")
        }
    }

    #[test]
    fn a_nested_job_request_gives_the_outer_one_back() {
        let mut log = Requests::default();
        let outer = ActorContext::review_job(&RunId::new("r1").unwrap(), 1);
        let inner = ActorContext::review_job(&RunId::new("r2").unwrap(), 3);
        fn run_log(log: &Requests) -> &dyn EventStore {
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
        fn run_log(log: &Requests) -> &dyn EventStore {
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

    #[test]
    fn the_supervisor_policy_retries_until_the_leases_would_go_stale() {
        let policy = HeartbeatPolicy::supervisor(Duration::from_secs(2));
        assert_eq!(policy.stale_after, Duration::from_secs(30));
        assert_eq!(policy.attempt, Duration::from_secs(5));
        assert!(policy.retries(true, Duration::ZERO));
        assert!(policy.retries(true, Duration::from_secs(22)));
        assert!(!policy.retries(true, Duration::from_secs(23)));
        // A failure that does not pass is not tried again.
        assert!(!policy.retries(false, Duration::ZERO));
        assert!(!HeartbeatPolicy::leases(Duration::from_secs(2)).registered);
    }

    /// A heartbeat that stopped on a queue that stayed busy is told from
    /// one the service did not answer and from any other failure, and its
    /// record names the supervisor, how many writes failed over how long
    /// and the last error.
    #[test]
    fn a_heartbeat_stopped_by_a_busy_queue_is_told_from_other_failures() {
        assert_eq!(HeartbeatFailure::cause_of(true, false), "busy");
        assert_eq!(HeartbeatFailure::cause_of(false, true), "unreachable");
        assert_eq!(HeartbeatFailure::cause_of(false, false), "failed");
        let failure = HeartbeatFailure {
            cause: HeartbeatFailure::cause_of(true, false),
            failures: 4,
            secs: 27.04,
            error: "4 heartbeat writes failed over 28.1s since the last one written: database is locked".into(),
        };
        assert_eq!(
            failure.payload(&LeaseToken::new("sv")),
            json!({
                "supervisor": "sv",
                "cause": "busy",
                "failures": 4,
                "secs": 27.0,
                "error": "4 heartbeat writes failed over 28.1s since the last one written: database is locked",
            })
        );
    }
}
