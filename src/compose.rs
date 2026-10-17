//! The composition root: each entry point opens the queue and the
//! repository, builds the adapters of the ports (`SqliteQueue`,
//! `GitRepository`, `ClaudeCode`, `Launchctl`'s
//! port, `SystemProcesses`, `LocalRunFiles`, the system clock and IDs) and
//! calls the use case in `application` (ADR-0013). `main` resolves the
//! queue location, assembles the clock and IDs once ([`OneShot`] and
//! [`SuperviseOptions::generators`]), parses the CLI and prints what these
//! return; `runtime` and `lifecycle` re-export them under the names the
//! tests use.
//!
//! The wiring is split by context (docs/design/architecture.md, rules L7
//! and L9 of "レイヤーの規則"): `execution` (実行と着地), `planning`
//! (計画管理), `observation` (観測と分析) and `host` (host運用), and
//! `supervisor`, the supervisor loop's, which wires the ports of every
//! context. This module keeps what more than one context wires (the
//! one-shot generators, the opened queue's bound checkout, the worker
//! adapters, the queue service's view) and re-exports every entry point,
//! so its callers name `crate::compose::<entry>` and never a submodule.

use anyhow::{Result, ensure};
use serde_json::Value;
use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
};

use crate::{
    application::{Generators, WorkerAdapter, WorkerAdapters, session::WrapperStart},
    domain::{
        NewAsk, Provider,
        worker::{ProviderCheck, Worker, WorkerMode},
    },
    infrastructure::{
        adapters::{ClaudeCode, VERIFICATION_TIMEOUT, free_disk_bytes, main_checkout_of},
        clock,
        codex::Codex,
        sqlite::SqliteQueue,
        transcripts::ClaudeTranscripts,
    },
};

mod execution;
mod host;
mod observation;
mod planning;
mod supervisor;

pub use execution::{
    RunE2eOptions, SetupReviewOptions, ended_run_material, init_recommended, integrate, recover,
    review, review_in, session, session_in_background, session_in_background_as,
    session_in_background_as_with_processes, session_in_background_with_containers, setup_review,
};
pub use host::{
    AutoUpdateJob, COMMAND_TARGET, HostMetricsSettings, QueueBrokerControlPort, QueueBrokerOptions,
    QueueRelayControlPort, QueueRelayOptions, QueueServiceControlPort, QueueServiceOptions,
    ReleaseIndexPort, ReleaseUpdateJob, SccacheOptions, down, inbox, inbox_command, init_queue,
    install_restart_arguments, migrate_queue, queue_schema, queue_service_start,
    queue_service_status, queue_service_stop, rebind, up,
};
pub use observation::{
    CiWatchOptions, LedgerOptions, REPORTS_DIR, ci_failures, doctor, events, events_matching,
    graph_diagram, observe, observer_launch, queue_agents, read_queue, service_detail,
    service_reads, stats, status, status_for, throughput_review, throughput_review_launch,
    timeline, timeline_in, watch, watch_in,
};
pub use planning::{
    PLANNER_TIMEOUT, PlannerEntry, planner_session, planner_session_with_provider, planners,
};
pub use supervisor::{
    ContainerRuntimePort, PausesPort, ProcessesPort, RunContainersPort, RunFilesPort,
    SuperviseOptions, VerificationContainersPort, supervise, supervise_with_reviewer,
};

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
    /// The toolchain profiles a `[toolchains.<name>]` of `dagq.toml` may
    /// name: the runtime's built-in ones
    /// ([`crate::domain::toolchain::BUILTIN`]); tests register another.
    pub profiles: &'static crate::domain::toolchain::ProfileRegistry,
}

/// `ask`: register an ask (see [`crate::application::ask::ask`]); the
/// inbox's watch notifies the person of it.
pub fn ask(db: &Path, ask: NewAsk) -> Result<Value> {
    let mut queue = SqliteQueue::open(db)?;
    crate::application::ask::ask(&mut queue, ask)
}

/// The containers of the worker's turns of the queue at `db`, bound to
/// `main_checkout`, in the dagq machine: the run's parts there are made
/// ready by the host's context ([`RunMachine`](crate::application::podman_machine::RunMachine)).
pub(crate) fn run_containers(
    db: &Path,
    main_checkout: Option<PathBuf>,
) -> crate::infrastructure::run_container::PodmanRunContainers {
    let podman = crate::infrastructure::podman_machine::PodmanCli::default();
    let host_wide = crate::infrastructure::kpi_config::host_wide_file();
    crate::infrastructure::run_container::PodmanRunContainers::of_queue(
        podman.clone(),
        db,
        main_checkout.clone(),
        host_wide.clone(),
        std::sync::Arc::new(crate::infrastructure::podman_machine::QueueRunMachine {
            podman,
            db: db.to_path_buf(),
            main_checkout,
            host_wide,
        }),
    )
}

/// The verifications' containers of the queue at `db`, whose parts the
/// host's context makes ready as it does a worker's
/// ([`run_containers`]).
pub(crate) fn verification_containers(
    db: &Path,
    main_checkout: Option<PathBuf>,
) -> crate::infrastructure::run_container::PodmanVerificationContainers {
    let podman = crate::infrastructure::podman_machine::PodmanCli::default();
    let host_wide = crate::infrastructure::kpi_config::host_wide_file();
    crate::infrastructure::run_container::PodmanVerificationContainers {
        podman: podman.clone(),
        db: db.to_path_buf(),
        main_checkout: main_checkout.clone(),
        host_wide: host_wide.clone(),
        machine: std::sync::Arc::new(crate::infrastructure::podman_machine::QueueRunMachine {
            podman,
            db: db.to_path_buf(),
            main_checkout,
            host_wide,
        }),
    }
}

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
/// and mode: Claude Code's headless turns (`claude -p`, ADR-t813-1), and
/// Codex's headless turns (`codex exec`, ADR-t813-3) when `codex` is
/// given. A headless run has no screen, so Codex's signals are never
/// read; its spans are looked for as Claude's and are not found (no active
/// time or tokens are recorded for them). A run recorded on Claude's
/// interactive worker is resumed headless (ADR-t1433-2), so it has no
/// adapters of its own.
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
    let workers = WorkerAdapters::default().with(Worker::CLAUDE_HEADLESS, adapter);
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
                Provider::Claude => crate::infrastructure::adapters::provider_executable(path),
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

/// The queue service as `status`, `doctor` and `dagq service status` show
/// it ([`crate::application::queue_service::view`]): its socket under the
/// queue's directory, and its attention when `queue` can be read.
fn queue_service_view(db: &Path, queue: Option<&SqliteQueue>) -> Value {
    crate::application::queue_service::view(
        &crate::infrastructure::queue_service::probe(db.parent().unwrap_or(Path::new("."))),
        queue.map(|queue| queue as &dyn crate::application::RunLog),
    )
}

/// Open the queue at `db` for the CLI's commands.
pub fn open_queue(db: &Path) -> Result<SqliteQueue> {
    SqliteQueue::open(db)
}

/// The live read-only connection for `watch`, refusing older schemas.
pub fn open_queue_watch(db: &Path) -> Result<SqliteQueue> {
    SqliteQueue::open_watch(db)
}

/// Open the queue at `db` read-only for the CLI's reads.
pub fn open_queue_read_only(db: &Path) -> Result<SqliteQueue> {
    SqliteQueue::open_read_only(db)
}

impl OneShot {
    pub fn new(generators: Generators) -> Self {
        Self {
            generators,
            user_config: None,
            disk: None,
            free_space: free_disk_bytes,
            verification_timeout: VERIFICATION_TIMEOUT,
            profiles: &crate::domain::toolchain::BUILTIN,
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

/// The `[tasks] title_prefixes` of the `dagq.toml` of the main checkout
/// `queue` is bound to, which `submit` and `lint` hold the tasks' titles
/// to; none for a queue bound to no checkout or without them.
pub fn task_title_prefixes(queue: &SqliteQueue) -> Result<Option<crate::domain::TitlePrefixes>> {
    match bound_checkout(queue)? {
        Some(checkout) => crate::infrastructure::run_env::load_title_prefixes(&checkout),
        None => Ok(None),
    }
}

/// What the threshold of the aging `candidates` and `graph` show is
/// chosen from ([`crate::application::claim_view::class_aging_secs`]):
/// the read of `[supervisor]` of the `dagq.toml` of the main checkout
/// `queue` is bound to, as it came (`None` without a checkout), and each
/// registration's value, oldest first.
pub fn class_aging_inputs(
    queue: &SqliteQueue,
) -> Result<crate::application::claim_view::AgingInputs> {
    let file = bound_checkout(queue)?
        .map(|checkout| crate::infrastructure::run_env::load_supervisor_config(&checkout));
    let registered = queue
        .supervisors()?
        .iter()
        .map(|registration| registration.class_aging)
        .collect();
    Ok(crate::application::claim_view::AgingInputs { file, registered })
}

/// The `[goals] max_spike_cycles` of the `dagq.toml` of the main checkout
/// `queue` is bound to (ADR-t1487-1 decision 8), which `submit` holds a
/// planner of the runtime's to; none for a queue bound to no checkout or
/// without it.
pub fn max_spike_cycles(queue: &SqliteQueue) -> Result<Option<usize>> {
    match bound_checkout(queue)? {
        Some(checkout) => crate::infrastructure::run_env::load_max_spike_cycles(&checkout),
        None => Ok(None),
    }
}

/// The `[goals] tags` of the `dagq.toml` of the main checkout `queue` is
/// bound to (ADR-t1639-1 decision 6), which `goal add`, `goal edit`,
/// `submit` and `lint` hold the goals to; none for a queue bound to no
/// checkout or without them.
pub fn goal_tags(queue: &SqliteQueue) -> Result<Option<crate::domain::TagSet>> {
    match bound_checkout(queue)? {
        Some(checkout) => crate::infrastructure::run_env::load_goal_tags(&checkout),
        None => Ok(None),
    }
}
