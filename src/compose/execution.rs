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
        session::{self as wrapper, Session, WrapperStart},
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
/// run's worktree, with the cmux the supervisor uses, the `[run.env]` of
/// the main checkout, the host's e2e lock and [`installation::E2E_TIMEOUT`]. Elsewhere there is none unless
/// `command` gives one.
#[derive(Debug, Clone, Default)]
pub struct RunE2eOptions {
    /// A shell command in place of the e2e (tests): it runs in any
    /// repository, with no cmux to ping unless `cmux` names one.
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

/// The runtime's own constructor of the recording of the session wrappers,
/// as [`RecordingBackend::new`](crate::application::recording::RecordingBackend::new) is of cmux's.
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
/// workspace of its own ([`wrapper_entry`]).
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
    let run = SqliteQueue::open(db)?.run(id)?;
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
    start: WrapperStart,
    sccache: Option<crate::domain::sccache::SccacheTarget>,
) -> Result<Value> {
    run_session_as(
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
    )
}

/// [`run_session`] as the wrapper `pid`.
#[allow(clippy::too_many_arguments)]
fn run_session_as(
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
) -> Result<Value> {
    // The wrapper's events are its own, not the worker's (ADR-t728-1).
    let mut queue = SqliteQueue::open(db)?.with_actor(
        crate::domain::actor::ActorContext::instance(crate::domain::actor::ActorRole::Wrapper, id),
    );
    // The wrapper only looks at the server and makes the guard of its
    // turns (ADR-t2086-1); the supervisor starts the server.
    let looker = crate::infrastructure::sccache::SystemSccache {
        log: PathBuf::new(),
        start_timeout: Duration::ZERO,
        lsof: "lsof".into(),
        ps: "ps".into(),
        dagq: dagq.map(Path::to_path_buf),
    };
    wrapper::run_session(
        Session {
            queue: &mut queue,
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
/// its place, with the cmux the supervisor uses (`cmux`) unless `options`
/// names one, the `[run.env]` of `checkout` and the host's e2e lock of
/// the queue at `db`; none elsewhere.
pub(super) fn run_e2e_port(
    db: &Path,
    checkout: &Path,
    options: &RunE2eOptions,
    cmux: &Path,
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
            cmux: options
                .cmux
                .clone()
                .or_else(|| (!stub).then(|| cmux.to_path_buf())),
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
        },
        run: Arc::new(|worktree, settings| {
            crate::infrastructure::e2e_gate::run(worktree, None, settings)
        }),
        retry: options
            .retry
            .unwrap_or(Duration::from_secs(crate::domain::run_e2e::RETRY_SECS)),
    })
}
