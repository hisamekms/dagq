//! The wiring of 実行と着地 (docs/design/architecture.md): `integrate`,
//! `recover`, `review`, the session wrapper of a run and the material of
//! its recovery job, and the e2e port the supervisor runs a run's e2e
//! through. Each entry builds the adapters and calls the use case; what is
//! decided is in `application` and `domain`.

use super::{OneShot, worker_adapters, wrapper_entry};
use crate::application::install as installation;
use crate::domain::LeaseToken;
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use crate::{
    application::{
        AgentProvider, MainRemote, ProcessControl, RecordingQueue, Repository, SessionWrappers,
        Spawner, health,
        integrate::{self as integration, IntegrateTarget, Integration, Integrator},
        prompt,
        recording::RecordingSessions,
        review::{self as reviewing, Review},
        session::{self as wrapper, Session, WrapperStart, WrapperStore},
        supervise::{self as supervisor, Heartbeat},
    },
    domain::{
        IntegrationOutcome, RunId, TaskDetail, TaskId, TaskRun,
        worker::{Worker, WorkerMode},
    },
    infrastructure::{
        adapters::{ClaudeCode, GitRepository, SystemProcesses, load_average, path_text},
        clock,
        codex::Codex,
        location::runs_dir,
        process::LocalSpawner,
        run_env::{ShellVerifier, load_disk_config},
        run_files::LocalRunFiles,
        runtime_store::{SqliteOpener, SqlitePorts},
        sqlite::SqliteQueue,
        transcripts::ClaudeTranscripts,
    },
};

/// How the supervisor runs the e2e of a run after its review (ADR-t1233-2):
/// in dagq's source `cargo test --locked --test e2e -- --ignored` in the
/// run's worktree, with the `[run.env]` of the main checkout, the host's
/// e2e lock and [`installation::E2E_TIMEOUT`]. Elsewhere there is none
/// unless `command` gives one.
#[derive(Debug, Clone, Default)]
pub struct RunE2eOptions {
    /// A shell command in place of the e2e (tests): it runs in any
    /// repository.
    pub command: Option<String>,
    /// How long it may run; `None` is [`installation::E2E_TIMEOUT`].
    pub timeout: Option<Duration>,
    /// How long a run waits before its e2e that could not run is tried
    /// again; `None` is [`crate::domain::run_e2e::RETRY_SECS`] (tests
    /// shorten it).
    pub retry: Option<Duration>,
    /// The host's e2e lock it takes, in place of
    /// [`installation::e2e_lock_path`] of the queue (which a `command`
    /// does not take unless it is given here).
    pub lock: Option<PathBuf>,
}

/// The runtime's own constructor of the recording of the session wrappers:
/// failures are recorded in the queue at `db`, and `token` is the
/// supervisor whose slots are reported (`None` reports all of them).
impl<'a> RecordingSessions<'a> {
    pub fn new(inner: &'a dyn SessionWrappers, db: PathBuf, token: Option<LeaseToken>) -> Self {
        Self::over(
            inner,
            Arc::new(SqlitePorts {
                opener: SqliteOpener {
                    db,
                    generators: clock::system(),
                    actor: None,
                },
                keep: |queue| -> Box<dyn RecordingQueue + Send> { Box::new(queue) },
            }),
            token,
            load_average,
        )
    }
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

/// `recover` on the system clock: see [`OneShot::recover`].
pub fn recover(db: &Path, id: &RunId) -> Result<Value> {
    OneShot::system().recover(db, id)
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

/// The session wrapper of run `id`. `resume` reopens the session of a `needs_session` run the supervisor is
/// resuming (ADR-0019) instead of starting the worker. A wrapper the run
/// refuses ends and leaves any workspace it runs in alone. Started in the
/// background (`background`, ADR-t1404-1), it needs no terminal and has no
/// workspace of its own ([`wrapper_entry`]). It opens no queue itself (the
/// dagq machine's parts a run in a clone readies before its turns still
/// read and record on the queue): it reads and records through the queue
/// service as the wrapper's principal,
/// whose token the supervisor issued before it started the wrapper
/// (ADR-t3102-1 decision 4), and a service it cannot reach ends it with
/// that error.
#[allow(clippy::too_many_arguments)]
pub fn session(
    db: &Path,
    id: &RunId,
    token: &LeaseToken,
    claude: &Path,
    codex: &Path,
    resume: bool,
    background: bool,
) -> Result<Value> {
    let start = wrapper_entry(background)?;
    let provider = ClaudeCode {
        executable: claude.into(),
    };
    let transcripts = ClaudeTranscripts::from_env();
    let codex = Codex::new(codex.into());
    let workers = worker_adapters(&provider, &transcripts, Some(&codex));
    // The run's provider picks the adapters (ADR-t813-2). Every wrapper
    // runs headless turns: a run still recorded on the interactive worker
    // (reopened before a resume converted it) takes its provider's
    // headless adapters (ADR-t1433-2).
    let mut queue = crate::infrastructure::queue_service::ServiceWrapperStore::of_queue(
        db,
        crate::domain::session_step::StepTarget::run(id),
    );
    let run = WrapperStore::run(&queue, id)?;
    let worker = Worker::new(run.actual_provider(), WorkerMode::Headless)?;
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
    // A headless turn outlives a wrapper killed with its workspace, or sent
    // SIGTERM in the background, unless the wrapper stops it (ADR-t813-1
    // decision 3, ADR-t1404-1 decision 3).
    crate::infrastructure::process::stop_groups_on_exit_signals();
    run_session(
        &mut queue,
        db,
        id,
        token,
        adapter.agent,
        other,
        &LocalSpawner,
        resume,
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

/// The session wrapper (`resume` for `session --resume`) with `provider`'s
/// agent and, for a headless run moved to the other provider (ADR-t813-2),
/// `other`'s, started by `spawner` in the background (ADR-t1404-1, the
/// only way a run's wrapper starts, ADR-t1433-3): without a terminal or a
/// workspace of its own, it registers once the supervisor recorded its
/// start (`wrapper_launched`) with this process's pid.
pub fn session_in_background(
    db: &Path,
    id: &RunId,
    token: &LeaseToken,
    provider: &dyn AgentProvider,
    other: Option<&dyn AgentProvider>,
    spawner: &dyn Spawner,
    resume: bool,
) -> Result<Value> {
    session_in_background_as(
        db,
        id,
        token,
        provider,
        other,
        spawner,
        resume,
        std::process::id(),
        None,
    )
}

/// [`session_in_background`] whose wrapper is the process `pid` rather
/// than this one, its environment naming `sccache` as `RUSTC_WRAPPER` when
/// given (ADR-t2086-1: a turn runs without it unless its server listens
/// on the loopback port just before): a test that runs each background
/// wrapper on a thread names it by a process of its own, so that every
/// session has a handle of its own (task 1439).
#[allow(clippy::too_many_arguments)]
pub fn session_in_background_as(
    db: &Path,
    id: &RunId,
    token: &LeaseToken,
    provider: &dyn AgentProvider,
    other: Option<&dyn AgentProvider>,
    spawner: &dyn Spawner,
    resume: bool,
    pid: u32,
    sccache: Option<crate::domain::sccache::SccacheTarget>,
) -> Result<Value> {
    session_in_background_as_with_processes(
        db,
        id,
        token,
        provider,
        other,
        spawner,
        resume,
        pid,
        &SystemProcesses,
        sccache,
        std::env::current_exe().ok().as_deref(),
    )
}

/// A background wrapper with injected process identity and control, whose
/// turns' guard links to `dagq` (ADR-t2086-1).
#[allow(clippy::too_many_arguments)]
pub fn session_in_background_as_with_processes(
    db: &Path,
    id: &RunId,
    token: &LeaseToken,
    provider: &dyn AgentProvider,
    other: Option<&dyn AgentProvider>,
    spawner: &dyn Spawner,
    resume: bool,
    pid: u32,
    processes: &dyn ProcessControl,
    sccache: Option<crate::domain::sccache::SccacheTarget>,
    dagq: Option<&Path>,
) -> Result<Value> {
    run_session_as(
        &mut wrapper_queue(db, id)?,
        db,
        id,
        token,
        provider,
        other,
        spawner,
        resume,
        WrapperStart::Background,
        sccache,
        pid,
        processes,
        dagq,
        None,
    )
}

/// A background wrapper as [`session_in_background_as_with_processes`]
/// whose run's turns in a clone run in `containers`' containers rather
/// than the dagq machine's: a test's fake podman.
#[allow(clippy::too_many_arguments)]
pub fn session_in_background_with_containers(
    db: &Path,
    id: &RunId,
    token: &LeaseToken,
    provider: &dyn AgentProvider,
    spawner: &dyn Spawner,
    resume: bool,
    pid: u32,
    processes: &dyn ProcessControl,
    containers: &dyn crate::application::run_container::RunContainers,
) -> Result<Value> {
    run_session_as(
        &mut wrapper_queue(db, id)?,
        db,
        id,
        token,
        provider,
        None,
        spawner,
        resume,
        WrapperStart::Background,
        None,
        pid,
        processes,
        None,
        Some(containers),
    )
}

/// The queue at `db` as the wrapper of run `id` writes it: a test's
/// wrapper in a thread, which a library call runs on the queue itself.
fn wrapper_queue(db: &Path, id: &RunId) -> Result<SqliteQueue> {
    // The wrapper's events are its own, not the worker's (ADR-t728-1).
    Ok(
        SqliteQueue::open(db)?.with_actor(crate::domain::actor::ActorContext::instance(
            crate::domain::actor::ActorRole::Wrapper,
            id,
        )),
    )
}

#[allow(clippy::too_many_arguments)]
fn run_session(
    queue: &mut dyn WrapperStore,
    db: &Path,
    id: &RunId,
    token: &LeaseToken,
    provider: &dyn AgentProvider,
    other: Option<&dyn AgentProvider>,
    spawner: &dyn Spawner,
    resume: bool,
    start: WrapperStart,
    sccache: Option<crate::domain::sccache::SccacheTarget>,
) -> Result<Value> {
    run_session_as(
        queue,
        db,
        id,
        token,
        provider,
        other,
        spawner,
        resume,
        start,
        sccache,
        std::process::id(),
        &SystemProcesses,
        std::env::current_exe().ok().as_deref(),
        None,
    )
}

/// [`run_session`] as the wrapper `pid`, on `queue`.
#[allow(clippy::too_many_arguments)]
fn run_session_as(
    queue: &mut dyn WrapperStore,
    db: &Path,
    id: &RunId,
    token: &LeaseToken,
    provider: &dyn AgentProvider,
    other: Option<&dyn AgentProvider>,
    spawner: &dyn Spawner,
    resume: bool,
    start: WrapperStart,
    sccache: Option<crate::domain::sccache::SccacheTarget>,
    pid: u32,
    processes: &dyn ProcessControl,
    dagq: Option<&Path>,
    containers: Option<&dyn crate::application::run_container::RunContainers>,
) -> Result<Value> {
    // The wrapper only looks at the server and makes the guard of its
    // turns (ADR-t2086-1); the supervisor starts the server.
    let looker = crate::infrastructure::sccache::SystemSccache {
        log: PathBuf::new(),
        start_timeout: Duration::ZERO,
        lsof: "lsof".into(),
        ps: "ps".into(),
        dagq: dagq.map(Path::to_path_buf),
    };
    // Read again before each turn (goal 38).
    let bound_main = queue.repository_binding().map(|binding| {
        binding.map(|common_dir| {
            crate::infrastructure::adapters::main_checkout_of(Path::new(&common_dir))
        })
    });
    let checkout = match &bound_main {
        Ok(found) => Ok(found
            .as_ref()
            .and_then(|found| found.as_ref().ok())
            .cloned()),
        Err(error) => Err(anyhow::anyhow!("{error:#}")),
    };
    let machine_containers =
        crate::compose::run_containers(db, checkout.as_ref().ok().cloned().flatten());
    let containers = containers.unwrap_or(&machine_containers);
    // The tests a turn's commands name as failed are read with the
    // profiles of the same `dagq.toml`, read again before each turn.
    let main_checkout = match bound_main {
        Ok(None) => None,
        Ok(Some(Ok(checkout))) => Some(Ok(checkout)),
        Ok(Some(Err(error))) | Err(error) => Some(Err(format!("{error:#}"))),
    };
    let toolchains = move || {
        crate::infrastructure::run_env::measured_toolchains(
            main_checkout
                .as_ref()
                .map(|checkout| checkout.as_deref().map_err(String::as_str)),
        )
    };
    let bound = crate::infrastructure::run_env::BoundRuntimes::new(checkout);
    let runtimes = move || bound.read();
    wrapper::run_session(
        Session {
            queue,
            db,
            provider,
            other,
            spawner,
            queue_service: &crate::infrastructure::queue_service::SystemServiceAccess,
            processes,
            files: &LocalRunFiles,
            pid,
            start,
            sccache: sccache
                .map(|target| (target, &looker as &dyn crate::application::SccacheServer)),
            clock: &crate::infrastructure::clock::SystemClock,
            runtimes: &runtimes,
            toolchains: &toolchains,
            containers,
            telemetry: &crate::infrastructure::queue_service::SystemServiceAccess,
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
            gate_config: &crate::infrastructure::run_env::gate_config,
        },
        task_id,
        range,
    )
}

impl OneShot {
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
            profiles: self.profiles,
            children: crate::domain::environment::RunChildren::default(),
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
        // Only looked at: the supervisor starts the server (ADR-t2086-1).
        let sccache = crate::infrastructure::sccache::SystemSccache {
            dagq: std::env::current_exe().ok(),
            ..crate::infrastructure::sccache::SystemSccache::new(
                db.parent().unwrap_or(Path::new(".")),
                Duration::ZERO,
            )
        };
        let mut integration = Integration {
            queue: &mut queue,
            repository: &repository,
            verifier: &verifier,
            // The queue broker, the Claude credential, the runs' relays, the
            // dagq machine and the image are the parts of the container
            // runtime built yet (goal 38).
            container: &crate::application::queue_broker::BrokerGate {
                // Only looked at: `up` and the supervisor start it.
                broker: Arc::new(crate::infrastructure::queue_broker::SystemQueueBroker::new(
                    &db,
                    Path::new("dagq"),
                )),
                // The supervisor's record of the relays.
                container: crate::application::broker_relay::RelayGate {
                    records: Arc::new(crate::infrastructure::broker_relay::QueueRelayHealth {
                        db: db.clone(),
                    }),
                    // Only looked at: the worker's Claude credential.
                    container:
                        crate::infrastructure::claude_credential::ClaudeCredentialGate::of_queue(
                            db.parent().unwrap_or(Path::new(".")),
                            // Only looked at: a container's start makes it run.
                            crate::application::podman_machine::MachineGate {
                                podman: Arc::new(
                                    crate::infrastructure::podman_machine::PodmanCli::default(),
                                ),
                                required: crate::domain::podman_machine::MachineMounts::of_queue(
                                    db.parent().unwrap_or(Path::new(".")),
                                ),
                                room: Arc::new(
                                    crate::infrastructure::podman_machine::HostMemoryRoom::under(
                                        db.parent()
                                            .and_then(Path::parent)
                                            .unwrap_or(Path::new(".")),
                                        crate::infrastructure::kpi_config::host_wide_file(),
                                    ),
                                ),
                                // Only looked at: a container's start builds it.
                                container: crate::application::container_image::ImageGate {
                                    specs: Arc::new(
                                        crate::infrastructure::container_image::CheckoutImages::new(
                                            &main_checkout,
                                            db.parent().unwrap_or(Path::new(".")),
                                        ),
                                    ),
                                    records: Arc::new(
                                        crate::infrastructure::container_image::QueueImageRecords {
                                            db: db.clone(),
                                            actor: Some(requester.clone()),
                                        },
                                    ),
                                    now: {
                                        let clock = self.generators.clock.clone();
                                        Arc::new(move || clock.now())
                                    },
                                    container:
                                        crate::infrastructure::container_runtime::ContainerExecutors,
                                },
                            },
                        ),
                },
            },
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
            sccache: Some(&sccache),
            verification_tokens: &crate::infrastructure::queue_broker::SystemVerificationTokens {
                db: db.clone(),
            },
            verification_containers: &crate::compose::verification_containers(
                &db,
                Some(main_checkout.clone()),
            ),
            // A person's landing waits for the automatic update's build and
            // e2e as the supervisor's does (ADR-t2719-1).
            heavy_turn: db
                .parent()
                .map(crate::application::heavy_turn::HeavyTurn::under),
            steps: None,
            // No handoff drains a person's landing.
            handing_off: None,
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
        let outcome = integrator.land_reporting(
            &mut integration,
            &request,
            crate::application::integrate::LandingTelemetry {
                tokens: &crate::infrastructure::queue_service::SystemServiceAccess,
                sinks: Arc::new(crate::infrastructure::queue_service::SystemServiceAccess),
                db: &db,
                clock: self.generators.clock.clone(),
            },
        )?;
        drop(heartbeat); // Stops the lease heartbeat before this process reports.
        Ok(serde_json::to_value(outcome)?)
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
}

/// The e2e the supervisor runs of a run after its review (ADR-t1233-2):
/// in dagq's source `checkout`, or anywhere `options` gives a command in
/// its place, with the `[run.env]` of `checkout` and the host's e2e lock
/// of the queue at `db`; none elsewhere.
pub(super) fn run_e2e_port(
    db: &Path,
    checkout: &Path,
    options: &RunE2eOptions,
) -> Option<crate::application::supervise::RunE2ePort> {
    let queue_dir = db.parent().unwrap_or(Path::new(".")).to_path_buf();
    let command = options.command.clone();
    let stub = command.is_some();
    if !stub && !crate::infrastructure::adapters::is_dagq_source(checkout) {
        return None;
    }
    Some(crate::application::supervise::RunE2ePort {
        settings: installation::E2eSettings {
            command,
            timeout: options.timeout.unwrap_or(installation::E2E_TIMEOUT),
            run_env_root: Some(checkout.to_path_buf()),
            queue_dir: Some(queue_dir.clone()),
            scratch: queue_dir.join("e2e"),
            // The run's, set for each e2e.
            log: queue_dir.join("e2e.log"),
            utc_offset_secs: 0,
            lock: options.lock.clone().or_else(|| {
                (!stub)
                    .then(|| installation::e2e_lock_path(&queue_dir))
                    .flatten()
            }),
            git_env: Vec::new(),
            ended: None,
        },
        run: Arc::new(|worktree, settings| {
            crate::infrastructure::e2e_gate::run(worktree, None, settings)
        }),
        retry: options
            .retry
            .unwrap_or(Duration::from_secs(crate::domain::run_e2e::RETRY_SECS)),
    })
}

/// What `setup-review` is run with: the providers' executables (a bare
/// name is resolved as the throughput review resolves it), Codex's home
/// (tests), the job's time limit, how the person approves, the `dagq`
/// binary, and the toolchain profiles whose recommendations it proposes
/// (the built-in ones, and a made-up one beside them in tests).
pub struct SetupReviewOptions {
    pub profiles: &'static crate::domain::toolchain::ProfileRegistry,
    pub claude: PathBuf,
    pub codex: PathBuf,
    pub codex_home: Option<PathBuf>,
    pub timeout: Duration,
    pub approval: crate::application::setup_review::Approval,
    pub dagq: PathBuf,
}

/// `setup-review` in `cwd` on the queue at `db`
/// ([`crate::application::setup_review::setup_review`]): its job starts
/// on the provider of `[roles.setup_review]` of the main checkout's
/// `dagq.toml`, Claude without.
pub fn setup_review(db: &Path, cwd: &Path, options: &SetupReviewOptions) -> Result<Value> {
    use crate::{
        domain::Provider,
        infrastructure::setup_review::{Interrupt, LocalSetupReview, setup_launch},
    };
    // Held until the place is removed: an interrupt stops the job or the
    // prompt instead of the process, so the place is removed.
    let _interrupt = Interrupt::install()?;
    let checkout = crate::infrastructure::adapters::main_checkout_of(cwd)
        .context("setup-review runs in a repository's checkout")?;
    let launch = setup_launch(&checkout);
    let resolved = |path: &Path, find: fn(&Path) -> Result<PathBuf>| {
        find(path).unwrap_or_else(|_| path.to_path_buf())
    };
    let provider: Box<dyn AgentProvider> = match launch.provider {
        Provider::Claude => Box::new(ClaudeCode {
            executable: resolved(
                &options.claude,
                crate::infrastructure::adapters::claude_at_entry,
            ),
        }),
        Provider::Codex => Box::new(Codex {
            executable: resolved(&options.codex, crate::infrastructure::codex::codex_at_entry),
            home: options.codex_home.clone(),
        }),
    };
    crate::application::setup_review::setup_review(
        &LocalSetupReview::new(
            db,
            cwd,
            provider.as_ref(),
            &launch,
            &options.dagq,
            options.timeout,
        ),
        options.approval,
        options.profiles,
    )
}

/// The review's settings `dagq init` recommends in `cwd`
/// ([`crate::application::setup_review::init_recommended`]) with the
/// toolchain profiles of `profiles`.
pub fn init_recommended(cwd: &Path, profiles: &crate::domain::toolchain::ProfileRegistry) -> Value {
    crate::application::setup_review::init_recommended(
        &crate::infrastructure::setup_review::LocalInitRepository { cwd },
        profiles,
    )
}
