//! The harness of the wiring tests: a queue of the test's own in SQLite's
//! memdb, copied from a template migrated once per process, the queue
//! service's use cases on it in this process, and the production
//! supervise ([`crate::application::supervise::supervise`]) run on a
//! thread with the ports the fakes fill, all within one deadline of the
//! test.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize},
        mpsc,
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::Result;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use super::{
    agent::{FakeAgent, Script},
    git::FakeGit,
    host::{
        FakeProcesses, FakeVerifier, MemoryVerificationTokens, Nothing, STOPPED_AT, StoppedClock,
    },
    service::{MemoryBackend, MemoryTokens},
    session::FakeSessions,
};
use crate::{
    application::{
        AgentProvider, Generators, HostOpsQueue, ObservationQueue, RecordingQueue, Repository,
        RunFiles, TaskStore, WorkerAdapter, WorkerAdapters,
        memory_files::MemoryFiles,
        supervise::{self, Layout, LoopSettings, Ports},
        supervisor_service::{
            ServiceCaller, SupervisorPorts, SupervisorQueues,
            end_bound::{EndBound, EndDeadline},
            resend::{InMemory, Resend, Resending},
        },
    },
    domain::{
        NewTask, Provider, TaskAction, TaskId,
        actor::{ActorContext, ActorRole},
        slot_limits::{SettingSource, SlotFlags, SlotLimits},
        worker::{ProviderCheck, Worker, WorkerMode},
    },
    infrastructure::{
        clock::UuidGenerator,
        queue_service::{InProcessCaller, SqliteServiceBackend},
        sqlite::SqliteQueue,
    },
};

/// The longest a wiring test may take, its supervisor's passes and its
/// wrappers' ends included: its one deadline. It catches a supervisor or a
/// wrapper that never ends, not a slow one: a test takes a few seconds
/// alone, but every test of a `cargo test --lib` shares the process and
/// the host's cores with the others and with other runs, and each pass
/// opens the queue anew per request, as the queue service does.
pub(super) const LIMIT: Duration = Duration::from_secs(120);

/// How often the test's supervisor goes round its loop.
const TICK: Duration = Duration::from_millis(20);

/// Where the repository of a test is checked out: made nowhere.
const CHECKOUT: &str = "/wiring/repo";

/// The memdb URI of the queue named `name`: one shared by every connection
/// of the process that opens the same name while one of them is open.
fn memdb(name: &str) -> PathBuf {
    PathBuf::from(format!("file:/{name}?vfs=memdb"))
}

/// A connection to the memdb queue at `uri`, as a test writes what no
/// use case of the supervisor's writes (a plan review's prediction).
pub(super) fn connect(uri: &Path) -> Connection {
    Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("open a memdb queue")
}

/// The queue migrated once for the process, never written after: the
/// connection that keeps its memdb.
fn template() -> &'static Mutex<Connection> {
    static TEMPLATE: OnceLock<Mutex<Connection>> = OnceLock::new();
    TEMPLATE.get_or_init(|| {
        let uri = memdb(&format!("wiring-template-{}", uuid::Uuid::new_v4()));
        let kept = connect(&uri);
        drop(SqliteQueue::init(&uri).expect("migrate the template queue"));
        Mutex::new(kept)
    })
}

/// A queue of the test's own, named `name` with a new UUID so that two runs
/// of one test at once do not share it: a copy of the template, kept while
/// the returned connection is open.
fn queue_copy(name: &str) -> (PathBuf, Connection) {
    let uri = memdb(&format!("wiring-{name}-{}", uuid::Uuid::new_v4()));
    let mut copy = connect(&uri);
    {
        let template = template()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rusqlite::backup::Backup::new(&template, &mut copy)
            .and_then(|backup| backup.run_to_completion(i32::MAX, Duration::ZERO, None))
            .expect("copy the template queue");
    }
    (uri, copy)
}

/// A wiring test: its queue, its fakes and its deadline.
pub(super) struct Wiring {
    name: String,
    pub(super) db: PathBuf,
    /// Keeps the test's memdb queue while the test runs.
    _memdb: Connection,
    deadline: Instant,
    generators: Generators,
    pid: u32,
    /// Whether the supervisor also runs Codex headless workers and jobs
    /// (on the same fake agent); Claude only by default.
    codex: bool,
    /// `[provider_fallback] workers`; on by default.
    worker_fallback: bool,
    /// `[fresh_session]`; no threshold by default.
    fresh_session: crate::domain::fresh_session::FreshSessionConfig,
    /// The slots (`--parallel`) of the supervisors started from now on;
    /// two by default.
    parallel: Arc<AtomicUsize>,
    /// The passes of the loop of the supervisor that runs, counted.
    passes: Arc<AtomicU64>,
    /// Where the supervisor pauses for the test ([`supervise::Ports::pauses`]);
    /// nowhere by default.
    pauses: Option<supervise::Pauses>,
    pub(super) files: Arc<MemoryFiles>,
    pub(super) git: FakeGit,
    pub(super) agent: Arc<FakeAgent>,
    pub(super) verifier: Arc<FakeVerifier>,
    processes: Arc<FakeProcesses>,
    tokens: Arc<MemoryTokens>,
    pub(super) sessions: Arc<FakeSessions>,
    /// The CI watch's port; none by default, as `--once` without it.
    ci_watch: Option<supervise::CiWatchPort>,
}

impl Wiring {
    /// The wiring test `name` whose agent does what `script` says and whose
    /// verification passes every command; its deadline starts now.
    pub(super) fn new(name: &str, script: Script) -> Self {
        let deadline = Instant::now() + LIMIT;
        let (db, memdb) = queue_copy(name);
        let clock: Arc<dyn crate::application::Clock> = Arc::new(StoppedClock::new(
            SystemTime::UNIX_EPOCH + STOPPED_AT,
            Instant::now(),
        ));
        let generators = Generators {
            clock: clock.clone(),
            ids: Arc::new(UuidGenerator),
        };
        // The supervisor is this process, as the queue service's peer is.
        let pid = std::process::id();
        let files = Arc::new(MemoryFiles::ticking());
        let git = FakeGit::new(Path::new(CHECKOUT), files.clone());
        let agent = Arc::new(FakeAgent::new(&db, files.clone(), git.clone(), script));
        let processes = Arc::new(FakeProcesses::with(pid));
        let tokens = Arc::new(MemoryTokens::new(
            PathBuf::from(format!("/wiring/{name}/queue")),
            clock.clone(),
        ));
        let sessions = Arc::new(FakeSessions::new(
            db.clone(),
            agent.clone(),
            tokens.clone(),
            processes.clone(),
            files.clone(),
            clock,
        ));
        Self {
            name: name.to_owned(),
            verifier: Arc::new(FakeVerifier::new(files.clone(), &[])),
            db,
            _memdb: memdb,
            deadline,
            generators,
            pid,
            codex: false,
            worker_fallback: true,
            fresh_session: Default::default(),
            parallel: Arc::new(AtomicUsize::new(2)),
            passes: Arc::new(AtomicU64::new(0)),
            pauses: None,
            files,
            git,
            agent,
            processes,
            tokens,
            sessions,
            ci_watch: None,
        }
    }

    /// The wiring whose supervisor watches the CI through `port`.
    pub(super) fn with_ci_watch(mut self, port: supervise::CiWatchPort) -> Self {
        self.ci_watch = Some(port);
        self
    }

    /// The wiring whose supervisor also runs Codex headless workers and
    /// jobs (a review's among them), on the same fake agent.
    pub(super) fn with_codex(mut self) -> Self {
        self.codex = true;
        self
    }

    /// The wiring whose supervisor starts no worker on the other provider
    /// when its own cannot be used (`[provider_fallback] workers = false`).
    pub(super) fn without_worker_fallback(mut self) -> Self {
        self.worker_fallback = false;
        self
    }

    /// The wiring whose `[fresh_session]` starts a new session for a
    /// resume past `peak_context_above` tokens.
    pub(super) fn with_fresh_session(mut self, peak_context_above: u64) -> Self {
        self.fresh_session.peak_context_above = Some(peak_context_above);
        self
    }

    /// The wiring whose supervisors have `parallel` slots.
    pub(super) fn with_parallel(self, parallel: usize) -> Self {
        self.set_parallel(parallel);
        self
    }

    /// The supervisors started from now on have `parallel` slots.
    pub(super) fn set_parallel(&self, parallel: usize) {
        self.parallel
            .store(parallel, std::sync::atomic::Ordering::SeqCst);
    }

    /// The wiring whose supervisor calls `pauses` at each point a test may
    /// pause it.
    pub(super) fn with_pauses(mut self, pauses: supervise::Pauses) -> Self {
        self.pauses = Some(pauses);
        self
    }

    /// How many passes the loops of this test's supervisors have gone, all
    /// told.
    pub(super) fn passes(&self) -> u64 {
        self.passes.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// `provider` held for its usage limit, as a supervisor records it,
    /// until long after the test's stopped clock.
    pub(super) fn hold(&self, provider: Provider) {
        let now = i64::try_from(STOPPED_AT.as_secs()).expect("seconds");
        let hold = crate::domain::provider_switch::ProviderHold {
            provider,
            reason: crate::domain::provider_switch::SwitchReason::UsageLimit,
            since: now,
            retry_at: now + 86_400,
        };
        connect(&self.db)
            .execute(
                "INSERT INTO run_events(kind,payload) VALUES ('provider_held',?1)",
                rusqlite::params![hold.held_payload(None).to_string()],
            )
            .expect("record the hold");
    }

    /// The ids and the clock of this test's queue.
    pub(super) fn generators(&self) -> Generators {
        self.generators.clone()
    }

    /// From now on, the wrapper of a resume fails to start.
    pub(super) fn fail_resume_launches(&self) {
        self.sessions
            .fail_resumes
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether the wrapper of a new run fails to start from now on.
    pub(super) fn fail_claim_launches(&self, fail: bool) {
        self.sessions
            .fail_claims
            .store(fail, std::sync::atomic::Ordering::SeqCst);
    }

    /// The test's queue, as the test reads and writes it.
    pub(super) fn queue(&self) -> SqliteQueue {
        SqliteQueue::open(&self.db)
            .expect("open the test's queue")
            .with_generators(self.generators.clone())
    }

    /// A ready task verified by `verify`.
    pub(super) fn ready_task(&self, verify: &[&str]) -> TaskId {
        self.ready_task_after(verify, &[])
    }

    /// A ready task verified by `verify` that depends on `after`.
    pub(super) fn ready_task_after(&self, verify: &[&str], after: &[TaskId]) -> TaskId {
        let mut queue = self.queue();
        let task = crate::application::TaskStore::add(
            &mut queue,
            NewTask {
                title: "wired task".into(),
                description: "small change".into(),
                acceptance: "works".into(),
                verification_commands: verify.iter().map(|&command| command.into()).collect(),
                required_evidence: Vec::new(),
                paths: Vec::new(),
                priority: Default::default(),
                change: None,
                dependencies: after.to_vec(),
                goal_dependencies: Vec::new(),
                goal_id: None,
                context: String::new(),
                provider: None,
                worker_mode: Some(WorkerMode::Headless),
                wait_for_build: false,
                execution_class: Default::default(),
            },
        )
        .expect("add the task");
        crate::application::TaskStore::transition(&mut queue, task.id(), TaskAction::BypassReview)
            .expect("make the task ready");
        task.id()
    }

    /// The kinds of the events of each task of the queue, by task, oldest
    /// first: what a test that timed out shows of where it stopped.
    fn events_by_task(&self) -> Vec<(i64, Vec<String>)> {
        let mut queue = self.queue();
        (1..)
            .map(TaskId::new)
            .map_while(|task| crate::application::TaskReads::show(&mut queue, task).ok())
            .map(|detail| {
                (
                    detail.task.id().as_i64(),
                    detail
                        .events
                        .iter()
                        .map(|event| event.kind.as_str().to_owned())
                        .collect(),
                )
            })
            .collect()
    }

    /// A replanning of `run` opened by the runtime and asked to pause, as
    /// the supervisor's diagnosis opens one: its operation.
    pub(super) fn pause_replan(
        &self,
        run: &crate::domain::RunId,
    ) -> crate::domain::task_replan::ReplanOperation {
        pause_replan(&self.db, &self.generators, run)
    }

    /// What `dagq status` shows of this test's queue now, attention and
    /// all, as a person reads it.
    pub(super) fn status(&self) -> Value {
        crate::application::health::status(
            &self.queue(),
            &*self.processes,
            &*self.generators.clock,
            None,
            None,
            None,
        )
        .expect("read the queue's status")
    }

    /// What a `watch --role inbox` from the queue's start returns now, as
    /// the inbox reads it: its events in their compact form.
    pub(super) fn inbox_watch(&self) -> Vec<Value> {
        let watched = crate::application::watch::watch(
            &self.queue(),
            &*self.generators.clock,
            &*self.processes,
            None,
            None,
            &crate::application::watch::WatchOptions {
                after: Some(crate::domain::EventId::new(0)),
                timeout: Some(Duration::ZERO),
                interval: Duration::from_millis(10),
                role: Some(crate::domain::SessionRole::Inbox),
            },
        )
        .expect("watch the queue as the inbox");
        watched["events"].as_array().cloned().unwrap_or_default()
    }

    /// A person's `recover` of `run` on this test's queue, its processes
    /// and its files: the report, or why it refused.
    pub(super) fn recover(&self, run: &crate::domain::RunId) -> Result<Value> {
        crate::application::health::recover(
            &mut self.queue(),
            &*self.processes,
            &*self.files,
            &Nothing,
            &*self.generators.clock,
            run,
        )
    }

    /// The queue service of this test in this process: its use cases on
    /// the memdb queue, its tokens in memory.
    fn service(&self) -> Arc<dyn ServiceCaller> {
        let tokens = self.tokens.clone();
        let (reads, writes, detail, commands) = super::super::wiring_service_use_cases();
        Arc::new(InProcessCaller::on(
            Arc::new(MemoryBackend {
                sqlite: SqliteServiceBackend {
                    db: self.db.clone(),
                    queue_dir: self.tokens.dir().to_owned(),
                    generators: self.generators.clone(),
                    reads,
                    writes,
                    detail,
                    commands,
                    at_stop: None,
                    stands_for: None,
                    doctor: None,
                    write_line: None,
                },
                tokens: self.tokens.clone(),
                processes: self.processes.clone(),
            }),
            self.tokens.dir().to_owned(),
            Arc::new(move |file: &Path| tokens.read(file)),
        ))
    }

    /// One `--once` supervisor on this test's queue: run until no run is
    /// active and nothing can be claimed, then every wrapper it started
    /// joined, all by the test's deadline. What it returned, which must
    /// name no error.
    pub(super) fn supervise_once(&self) -> Value {
        self.supervise(false, None)
    }

    /// [`Self::supervise_once`] whose outcome may name the errors of the
    /// runs it went on past (a resume that could not start), or the error
    /// the supervisor stopped on (`supervisor_failed`), which the caller
    /// checks.
    pub(super) fn supervise_once_reporting(&self) -> Value {
        self.supervise_reporting(false, None)
    }

    /// [`Self::supervise_once`] with a stop asked before its first pass:
    /// a supervisor that drains.
    pub(super) fn supervise_draining(&self) -> Value {
        self.supervise(true, None)
    }

    /// [`Self::supervise_once`] while a person acts on the queue: `act` is
    /// given the queue each tick until it says it acted, as a person
    /// answers what the running supervisor asked (a hold its `--once` loop
    /// waits on), all within the test's one deadline.
    pub(super) fn supervise_once_while(
        &self,
        mut act: impl FnMut(&mut SqliteQueue) -> bool,
    ) -> Value {
        self.supervise(false, Some(&mut act))
    }

    /// [`Self::supervise_once_while`] whose `act` is also given what asks
    /// the supervisor to stop (it drains, as at a stop request), and whose
    /// outcome may name the errors of its runs (a lease another process
    /// took), which the caller checks.
    pub(super) fn supervise_once_while_stoppable(
        &self,
        mut act: impl FnMut(&mut SqliteQueue, &dyn Fn()) -> bool,
    ) -> Value {
        let stop = Arc::new(AtomicBool::new(false));
        let asks_stop = stop.clone();
        let ask_stop = move || asks_stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut acting = |queue: &mut SqliteQueue| act(queue, &ask_stop);
        self.supervise_with(stop, Some(&mut acting))
    }

    /// [`Self::supervise_reporting`] whose outcome must name no error.
    fn supervise(
        &self,
        stopped: bool,
        act: Option<&mut dyn FnMut(&mut SqliteQueue) -> bool>,
    ) -> Value {
        let outcome = self.supervise_reporting(stopped, act);
        assert_eq!(outcome["errors"], serde_json::json!([]), "{outcome}");
        outcome
    }

    fn supervise_reporting(
        &self,
        stopped: bool,
        act: Option<&mut dyn FnMut(&mut SqliteQueue) -> bool>,
    ) -> Value {
        self.supervise_with(Arc::new(AtomicBool::new(stopped)), act)
    }

    /// [`Self::supervise_reporting`] whose supervisor drains once `stop`
    /// is set.
    fn supervise_with(
        &self,
        stop: Arc<AtomicBool>,
        mut act: Option<&mut dyn FnMut(&mut SqliteQueue) -> bool>,
    ) -> Value {
        let (returned, returns) = mpsc::channel();
        let supervisor = {
            let parts = Parts {
                service: self.service(),
                db: self.db.clone(),
                pid: self.pid,
                codex: self.codex,
                worker_fallback: self.worker_fallback,
                fresh_session: self.fresh_session,
                parallel: self.parallel.load(std::sync::atomic::Ordering::SeqCst),
                passes: self.passes.clone(),
                pauses: self.pauses.clone(),
                generators: self.generators.clone(),
                files: self.files.clone(),
                git: self.git.clone(),
                agent: self.agent.clone(),
                verifier: self.verifier.clone(),
                processes: self.processes.clone(),
                tokens: self.tokens.clone(),
                sessions: self.sessions.clone(),
                ci_watch: self.ci_watch.clone(),
                stop: stop.clone(),
            };
            std::thread::Builder::new()
                .name(format!("{} supervisor", self.name))
                .spawn(move || {
                    let outcome = parts.supervise();
                    let _ = returned.send(());
                    outcome
                })
                .expect("start the supervisor's thread")
        };
        let returned = loop {
            let left = self.deadline.saturating_duration_since(Instant::now());
            let Some(acting) = act.as_mut() else {
                break returns.recv_timeout(left).is_ok();
            };
            match returns.recv_timeout(left.min(TICK)) {
                Ok(()) => break true,
                Err(mpsc::RecvTimeoutError::Disconnected) => break false,
                Err(mpsc::RecvTimeoutError::Timeout) if left <= TICK => break false,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if acting(&mut self.queue()) {
                        act = None;
                    }
                }
            }
        };
        if !returned {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            panic!(
                "{}: the supervisor did not return within {LIMIT:?}; the tasks' events: {:?}",
                self.name,
                self.events_by_task()
            );
        }
        let outcome = supervisor.join();
        // The wrappers are joined before anything fails, a supervisor's
        // failure included.
        let wrappers = self.sessions.join_by(&self.name, self.deadline);
        let outcome = outcome
            .unwrap_or_else(|_| panic!("{}: the supervisor panicked", self.name))
            // A supervisor that stopped on an error (one that stops its
            // claims, as a run's wrapper that could not start) says it as
            // the outcome's only error, for the caller to check.
            .unwrap_or_else(|error| {
                serde_json::json!({"supervisor_failed": true,
                                   "errors": [{"message": format!("{error:#}")}]})
            });
        for wrapper in wrappers {
            wrapper.unwrap_or_else(|error| {
                panic!("{}: a session wrapper failed: {error:#}", self.name)
            });
        }
        outcome
    }
}

/// What the supervisor's thread builds its ports of.
struct Parts {
    service: Arc<dyn ServiceCaller>,
    db: PathBuf,
    pid: u32,
    codex: bool,
    worker_fallback: bool,
    fresh_session: crate::domain::fresh_session::FreshSessionConfig,
    parallel: usize,
    passes: Arc<AtomicU64>,
    pauses: Option<supervise::Pauses>,
    generators: Generators,
    files: Arc<MemoryFiles>,
    git: FakeGit,
    agent: Arc<FakeAgent>,
    verifier: Arc<FakeVerifier>,
    processes: Arc<FakeProcesses>,
    tokens: Arc<MemoryTokens>,
    sessions: Arc<FakeSessions>,
    ci_watch: Option<supervise::CiWatchPort>,
    stop: Arc<AtomicBool>,
}

impl Parts {
    /// The production supervise on these parts, wired as the composition
    /// root wires the binary's (`compose::supervisor`), each port that
    /// reaches the host a fake.
    fn supervise(self) -> Result<Value> {
        let clock = self.generators.clock.clone();
        // Bounded by the end's deadline, behind the holding of the records
        // not taken, as the binary's.
        let end = Arc::new(EndDeadline::default());
        let service: Arc<dyn ServiceCaller> =
            Arc::new(EndBound::new(self.service, clock.clone(), end.clone()));
        let service: Arc<dyn ServiceCaller> = Arc::new(
            Resending::new(service, Resend::Hold, self.generators.clone())
                .kept_in(Arc::new(InMemory::default())),
        );
        let queues = SupervisorQueues::new(
            service.clone(),
            ActorContext::instance(ActorRole::Supervisor, self.pid),
        );
        let review_queues = queues.clone();
        let (files, git, pid) = (self.files.clone(), self.git.clone(), self.pid);
        let review_material =
            move |task_id: TaskId, range: Option<&crate::application::review::ReviewRange>| {
                let open = |_: &Path| -> Result<Box<dyn Repository>> { Ok(Box::new(git.clone())) };
                crate::application::review::review(
                    crate::application::review::Review {
                        queue: &mut review_queues.queue() as &mut dyn TaskStore,
                        files: &*files,
                        open_repository: &open,
                        pid,
                        gate_config: &crate::infrastructure::run_env::gate_config,
                    },
                    task_id,
                    range,
                )
            };
        let agent = &*self.agent;
        let adapter = WorkerAdapter {
            agent,
            signals: agent,
            transcripts: agent,
        };
        let mut workers = WorkerAdapters::default().with(Worker::CLAUDE_HEADLESS, adapter);
        if self.codex {
            workers = workers.with(
                Worker {
                    provider: Provider::Codex,
                    mode: crate::domain::worker::WorkerMode::Headless,
                },
                adapter,
            );
        }
        let layout = layout(&self.db, self.pid, &workers);
        let ports = Ports {
            queues: Arc::new(queues.clone()),
            service,
            observation_queues: Arc::new(SupervisorPorts {
                queues: queues.clone(),
                keep: |queue| -> Box<dyn ObservationQueue + Send> { Box::new(queue) },
            }),
            host_queues: Arc::new(SupervisorPorts {
                queues: queues.clone(),
                keep: |queue| -> Box<dyn HostOpsQueue + Send> { Box::new(queue) },
            }),
            recording_queues: Arc::new(SupervisorPorts {
                queues: queues.clone(),
                keep: |queue| -> Box<dyn RecordingQueue + Send> { Box::new(queue) },
            }),
            repository: Arc::new(self.git.clone()),
            remote: Arc::new(self.git.clone()),
            verifier: self.verifier.clone(),
            container: Arc::new(Nothing),
            run_containers: Arc::new(Nothing),
            verification_tokens: Arc::new(MemoryVerificationTokens::default()),
            verification_containers: Arc::new(Nothing),
            sessions: &*self.sessions,
            workers,
            reviewer: agent as &dyn AgentProvider,
            codex_jobs: self.codex.then_some(agent as &dyn AgentProvider),
            spawner: agent,
            programs: crate::application::review_programs::ProgramBackends::new(
                &Nothing,
                crate::application::execution::ExecutionConfig::default(),
            ),
            landing_programs: crate::application::review_programs::ProgramHost {
                host: Arc::new(Nothing),
                spawner: self.agent.clone(),
            },
            service_access: &*self.tokens,
            telemetry_tokens: &*self.tokens,
            telemetry_sinks: Arc::new(Nothing),
            end,
            pauses: self.pauses,
            files: self.files.clone() as Arc<dyn RunFiles>,
            processes: self.processes.clone(),
            generators: self.generators.clone(),
            review_material: &review_material,
            load_average: || None,
            free_space: |_| Some(u64::MAX),
            scratchpad_roots: Arc::new(Vec::new),
            host_versions: |_, _, _| Default::default(),
            worker_plugin: Arc::new(|_| crate::domain::instructions::NO_PLUGIN.to_owned()),
            reports: None,
            max_improvement_proposals: Arc::new(|| Ok(0)),
            held_verdicts: Arc::new(InMemory::default()),
            conflicts_file: None,
            supervisor_file: None,
            provider_fallback_file: None,
            fresh_session_file: None,
            replan_file: None,
            recurring_file: None,
            forecasts: None,
            agent_watch: None,
            release: None,
            host_metrics: None,
            queue_service: None,
            queue_broker: None,
            broker_relay: None,
            run_e2e: None,
            sccache: None,
            ci_watch: self.ci_watch,
            ledger: None,
            environment: None,
            resources: None,
            layout,
        };
        supervise::supervise(
            &ports,
            &settings(
                self.stop,
                self.worker_fallback,
                self.fresh_session,
                self.parallel,
                self.passes,
            ),
        )
    }
}

/// Where the test's supervisor works: the memdb queue's URI, whose
/// directories (`file:/runs` and the like) are only names in the test's
/// files, and the repository checked out at [`CHECKOUT`].
fn layout(db: &Path, pid: u32, workers: &WorkerAdapters<'_>) -> Layout {
    let queue_dir = db.parent().unwrap_or(Path::new("")).to_owned();
    Layout {
        db: db.to_owned(),
        runs_dir: queue_dir.join("runs"),
        queue_hash: "wiring".into(),
        repo_root: CHECKOUT.into(),
        main_checkout: CHECKOUT.into(),
        common_dir: Path::new(CHECKOUT).join(".git"),
        claude: "wiring-claude".into(),
        codex: "wiring-codex".into(),
        codex_home: None,
        providers: vec![
            ProviderCheck {
                provider: Provider::Claude,
                executable: "wiring-claude".into(),
                found: true,
                error: None,
                modes: workers.modes(Provider::Claude),
            },
            {
                let modes = workers.modes(Provider::Codex);
                ProviderCheck {
                    provider: Provider::Codex,
                    executable: "wiring-codex".into(),
                    found: !modes.is_empty(),
                    error: modes
                        .is_empty()
                        .then(|| "no Codex in this wiring test".into()),
                    modes,
                }
            },
        ],
        runner: "wiring-dagq".into(),
        pid,
        version: crate::VERSION.into(),
        observer_env_remove: Vec::new(),
        planners_dir: queue_dir.join("planners"),
        plugin_dir: None,
        plan_reviews_dir: queue_dir.join("plan-reviews"),
        goal_reviews_dir: queue_dir.join("goal-reviews"),
        agent_evals_dir: queue_dir.join("agent-evals"),
        agent_cases_dir: queue_dir.join("agent-cases"),
        review_programs_dir: queue_dir.join("review-programs"),
        satisfied_dir: queue_dir.join("satisfied"),
        replans_dir: queue_dir.join("replans"),
        service_credential: OnceLock::new(),
    }
}

/// A `--once` supervisor's loop of `parallel` slots, its waits the test's ticks,
/// `[provider_fallback] workers` as `worker_fallback` says,
/// `[fresh_session]` as `fresh_session`, its passes counted in `passes`,
/// every other setting the production's default; `stop` drains it.
fn settings(
    stop: Arc<AtomicBool>,
    worker_fallback: bool,
    fresh_session: crate::domain::fresh_session::FreshSessionConfig,
    parallel: usize,
    passes: Arc<AtomicU64>,
) -> LoopSettings {
    let slot_flags = SlotFlags {
        parallel: Some(parallel),
        max_waiting: None,
        runtime_planners: None,
        claim_spacing: None,
    };
    LoopSettings {
        no_claude: false,
        retry_unreadable_review: false,
        limits: SlotLimits::resolve(slot_flags, &Default::default()),
        slot_flags,
        light_changes: Default::default(),
        provider_fallback: crate::domain::provider_switch::ProviderFallback {
            workers: worker_fallback,
            ..Default::default()
        },
        fresh_session,
        review_jobs: Default::default(),
        once: true,
        stop,
        observe_interval: Duration::ZERO,
        observe_daily: false,
        throughput_review: false,
        utc_offset: |_| 0,
        tick: TICK,
        idle_poll: TICK,
        landing_recheck: supervise::LANDING_BRANCH_RECHECK,
        heartbeat_interval: supervise::HEARTBEAT_INTERVAL,
        sweep_interval: Duration::from_secs(60),
        disk_cleanup_interval: supervise::CLEANUP_INTERVAL,
        live_alert_interval: Duration::from_secs(
            crate::domain::live_alerts::OBSERVE_INTERVAL_SECS.unsigned_abs(),
        ),
        stall: Default::default(),
        conflicts: Default::default(),
        conflicts_error: None,
        planner_timeout: super::super::wiring_planner_timeout(),
        handoff_token: None,
        mode: None,
        update: Default::default(),
        max_load: None,
        max_load_source: SettingSource::Default,
        disk: Default::default(),
        resume: Default::default(),
        owner: None,
        owner_gone_grace: supervise::OWNER_GONE_GRACE,
        lease_lost_grace: supervise::LEASE_LOST_GRACE,
        passes,
    }
}

/// Two runs of one test at once (as a stress runs them in one process)
/// each have a queue of their own.
#[test]
fn two_wirings_of_one_name_share_no_queue() {
    let script = || Script {
        receipts: Vec::new(),
        verdicts: Vec::new(),
    };
    let (one, other) = (
        Wiring::new("shared", script()),
        Wiring::new("shared", script()),
    );
    assert_ne!(one.db, other.db);
    let task = one.ready_task(&[]);
    assert!(crate::application::TaskReads::show(&mut one.queue(), task).is_ok());
    assert!(crate::application::TaskReads::show(&mut other.queue(), task).is_err());
}

/// [`Wiring::pause_replan`] on the queue at `db`: callable from a fake's
/// hook, which holds no [`Wiring`].
pub(super) fn pause_replan(
    db: &Path,
    generators: &Generators,
    run: &crate::domain::RunId,
) -> crate::domain::task_replan::ReplanOperation {
    use crate::domain::task_replan::{NewReplan, ReplanMove, ReplanOrigin, ReplanStatus};
    let mut queue = SqliteQueue::open(db)
        .expect("open the test's queue")
        .with_generators(generators.clone());
    let op = queue
        .record_replan(
            &NewReplan {
                run: run.clone(),
                reason: "two independent parts".into(),
                evidence: Vec::new(),
            },
            ReplanOrigin::Runtime,
            "supervisor",
            "supervisor",
        )
        .expect("open the replanning")
        .operation;
    crate::application::RunTransitions::move_replan(
        &mut queue,
        op.id,
        op.generation,
        &ReplanMove {
            to: ReplanStatus::PauseRequested,
            reason: "the test pauses it".into(),
            snapshot_id: None,
            diagnosis: None,
        },
    )
    .expect("ask the replanning to pause")
}
