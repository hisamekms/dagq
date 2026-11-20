//! The session wrappers of a wiring test: a background start runs the
//! production wrapper ([`crate::application::session::run_session`]) of the
//! run its command names on a thread of the test, as a process of
//! [`FakeProcesses`] whose pid and start make its handle; the thread is
//! joined by the test ([`FakeSessions::join_by`]).

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde_json::Value;

use super::{
    agent::{CodexPlannerAgent, FakeAgent},
    host::{FakeProcesses, Nothing},
    service::MemoryTokens,
};
use crate::{
    application::{
        Clock, ProcessControl, RunFiles, SessionWrappers,
        memory_files::MemoryFiles,
        session::{Session, WrapperStart, run_session},
    },
    domain::{
        LeaseToken, RunId,
        actor::{ActorContext, ActorRole},
        background_wrapper::{BackgroundHandle, StopRoute, WrapperStop, start_token},
        turn::exit_path,
    },
    infrastructure::sqlite::SqliteQueue,
};

/// How often a test's wrapper looks again for the record of its start: the
/// test's own, shorter than the production's
/// [`REGISTRATION_POLL`](crate::application::session::REGISTRATION_POLL).
/// Each look opens the queue, so a much shorter wait takes from the
/// supervisor's passes the CPU they need.
const REGISTRATION_POLL: Duration = Duration::from_millis(50);

/// A wrapper started on a thread: its handle and run directory, and its
/// thread with the channel that says it returned.
struct Wrapper {
    handle: String,
    run_dir: PathBuf,
    thread: JoinHandle<Result<Value>>,
    returned: mpsc::Receiver<()>,
}

/// The wrappers of a test, and what they run with.
pub(super) struct FakeSessions {
    pub(super) db: PathBuf,
    pub(super) agent: Arc<FakeAgent>,
    pub(super) tokens: Arc<MemoryTokens>,
    pub(super) processes: Arc<FakeProcesses>,
    pub(super) files: Arc<MemoryFiles>,
    pub(super) clock: Arc<dyn Clock>,
    wrappers: Mutex<Vec<Wrapper>>,
    /// Parked planner wrappers: fake process handles, with no thread.
    planners: Mutex<Vec<String>>,
    /// What lets the next wrapper started (not a resume's) go on to its
    /// session, once: it waits, started and not yet registered, until it
    /// says so or is dropped (by the test's deadline at the latest).
    launch_gate: Mutex<Option<mpsc::Receiver<()>>>,
    /// Whether a resume's wrapper fails to start, as a launch that the
    /// host refused.
    pub(super) fail_resumes: std::sync::atomic::AtomicBool,
    /// Whether a new run's wrapper fails to start, as a launch that the
    /// host refused.
    pub(super) fail_claims: std::sync::atomic::AtomicBool,
    pub(super) fail_planners: std::sync::atomic::AtomicBool,
    pub(super) drive_codex_planners: std::sync::atomic::AtomicBool,
    planner_commands: Mutex<Vec<String>>,
}

impl FakeSessions {
    pub(super) fn new(
        db: PathBuf,
        agent: Arc<FakeAgent>,
        tokens: Arc<MemoryTokens>,
        processes: Arc<FakeProcesses>,
        files: Arc<MemoryFiles>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            db,
            agent,
            tokens,
            processes,
            files,
            clock,
            wrappers: Mutex::default(),
            planners: Mutex::default(),
            launch_gate: Mutex::default(),
            fail_resumes: std::sync::atomic::AtomicBool::new(false),
            fail_claims: Default::default(),
            fail_planners: Default::default(),
            drive_codex_planners: Default::default(),
            planner_commands: Mutex::default(),
        }
    }

    /// Hold the next wrapper started for a run's first session before it
    /// runs: it waits until `go` says so, as a wrapper slowed between its
    /// launch and its first turn.
    pub(super) fn hold_next_launch(&self, go: mpsc::Receiver<()>) {
        *self
            .launch_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(go);
    }

    /// Number of planner wrappers started; their turns are played by the test.
    pub(super) fn planner_launches(&self) -> usize {
        self.planner_commands.lock().unwrap().len()
    }

    pub(super) fn planner_commands(&self) -> Vec<String> {
        self.planner_commands.lock().unwrap().clone()
    }

    fn start_planner(
        &self,
        id: crate::domain::PlannerId,
        cwd: &Path,
        command: &str,
        env: &[(String, String)],
    ) -> Result<String> {
        use std::sync::atomic::Ordering;
        if self.fail_planners.load(Ordering::SeqCst) {
            anyhow::bail!("the wiring test refuses to start the wrapper of planner {id}");
        }
        let (pid, start) = self.processes.start();
        let handle = BackgroundHandle {
            pid,
            start: start_token(&start),
        }
        .to_string();
        if self.drive_codex_planners.load(Ordering::SeqCst) && command.contains("'--codex' ") {
            self.files.advance_clock_to(self.clock.system_time());
            let launch = env
                .iter()
                .find(|(key, _)| key == crate::domain::sessions::LAUNCH_ENV)
                .context("the wrapper has no launch")?;
            let launch: crate::domain::actor_model::ActorLaunch = serde_json::from_str(&launch.1)?;
            anyhow::ensure!(
                launch.provider == crate::domain::Provider::Codex,
                "the test drives Codex planner wrappers"
            );
            let (db, cwd, agent, tokens, processes, files, clock) = (
                self.db.clone(),
                cwd.to_owned(),
                self.agent.clone(),
                self.tokens.clone(),
                self.processes.clone(),
                self.files.clone(),
                self.clock.clone(),
            );
            let run_dir = crate::infrastructure::location::planners_dir(&db).join(id.to_string());
            let (returned, returns) = mpsc::channel();
            let thread = std::thread::Builder::new()
                .name(format!("planner wrapper {id}"))
                .spawn(move || {
                    let result = (|| {
                        let mut queue = SqliteQueue::open(&db)?.with_actor(ActorContext::instance(
                            ActorRole::Wrapper,
                            format_args!("planner:{id}"),
                        ));
                        let provider = CodexPlannerAgent::new();
                        crate::compose::planner_session_on(
                            crate::application::planner::PlannerWrapper {
                                queue: &mut queue,
                                db: &db,
                                provider: &provider,
                                spawner: &*agent,
                                files: &*files,
                                processes: &*processes,
                                pid,
                                clock: &*clock,
                                poll_interval: Duration::from_millis(20),
                                telemetry: &Nothing,
                                queue_service: Some(&*tokens),
                                read_snapshot: None,
                            },
                            id,
                            &cwd,
                            None,
                            &launch,
                        )
                    })();
                    processes.end(pid);
                    let _ = returned.send(());
                    result
                })?;
            self.lock().push(Wrapper {
                handle: handle.clone(),
                run_dir,
                thread,
                returned: returns,
            });
        } else {
            self.planners.lock().unwrap().push(handle.clone());
        }
        self.planner_commands
            .lock()
            .unwrap()
            .push(command.to_owned());
        Ok(handle)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Wrapper>> {
        self.wrappers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Join every wrapper started, each asked to exit, by `deadline`: what
    /// each returned. One that has not returned by then is named with the
    /// test's `name` in the panic.
    pub(super) fn join_by(&self, name: &str, deadline: Instant) -> Vec<Result<Value>> {
        let wrappers = std::mem::take(&mut *self.lock());
        // Every wrapper is asked to exit first, and each is waited for
        // before any that did not return fails the test.
        for wrapper in &wrappers {
            let _ = self.files.write(&exit_path(&wrapper.run_dir), b"");
        }
        let mut stuck = Vec::new();
        let mut returned = Vec::new();
        for wrapper in wrappers {
            let left = deadline.saturating_duration_since(Instant::now());
            if wrapper.returned.recv_timeout(left).is_err() {
                stuck.push(wrapper.handle);
                continue;
            }
            returned.push(wrapper.thread.join().unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "the session wrapper {} panicked",
                    wrapper.handle
                ))
            }));
        }
        assert!(
            stuck.is_empty(),
            "{name}: the session wrappers {stuck:?} did not return by the test's deadline"
        );
        returned
    }
}

/// The value of `flag` among the words of a wrapper's command.
fn after<'a>(words: &[&'a str], flag: &str) -> Option<&'a str> {
    let at = words.iter().position(|word| *word == flag)?;
    words.get(at + 1).copied()
}

impl SessionWrappers for FakeSessions {
    /// The wrapper of the run the command names (`--run`, `--lease`,
    /// `--resume`), on a thread as a new process.
    fn launch_background(
        &self,
        cwd: &Path,
        command: &str,
        env: &[(String, String)],
        _: &std::path::Path,
    ) -> Result<String> {
        let words: Vec<&str> = command
            .split(' ')
            .map(|word| word.trim_matches('\''))
            .collect();
        if let Some(id) = after(&words, "--planner") {
            return self.start_planner(
                crate::domain::PlannerId::new(id.parse()?),
                cwd,
                command,
                env,
            );
        }
        let id = RunId::new(after(&words, "--run").context("the wrapper names no run")?)?;
        let token =
            LeaseToken::new(after(&words, "--lease").context("the wrapper names no lease")?);
        let resume = words.contains(&"--resume");
        if !resume && self.fail_claims.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("the wiring test refuses to start the wrapper of run {id}");
        }
        if resume && self.fail_resumes.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("the wiring test refuses to start the wrapper of run {id}'s resume");
        }
        let (pid, start) = self.processes.start();
        let handle = BackgroundHandle {
            pid,
            start: start_token(&start),
        }
        .to_string();
        let run_dir = PathBuf::from(
            crate::application::RunReads::run(&SqliteQueue::open(&self.db)?, &id)?
                .run_dir()
                .context("the run has no directory")?,
        );
        let (db, agent, tokens, processes, files, clock) = (
            self.db.clone(),
            self.agent.clone(),
            self.tokens.clone(),
            self.processes.clone(),
            self.files.clone(),
            self.clock.clone(),
        );
        let gate = if resume {
            None
        } else {
            self.launch_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
        };
        let (returned, returns) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name(format!("wrapper {id}"))
            .spawn(move || {
                if let Some(gate) = gate {
                    let _ = gate.recv_timeout(super::harness::LIMIT);
                }
                let returned_value = wrap(
                    &db, &id, &token, resume, pid, &agent, &tokens, &processes, &files, &*clock,
                );
                // Its process ends with it.
                processes.end(pid);
                let _ = returned.send(());
                returned_value
            })?;
        self.lock().push(Wrapper {
            handle: handle.clone(),
            run_dir,
            thread,
            returned: returns,
        });
        Ok(handle)
    }

    /// The wrapper asked to exit, as the production stop asks it; its
    /// thread ends at its next look.
    fn stop_background(&self, handle: &str, _: StopRoute) -> Result<Option<WrapperStop>> {
        if self.planners.lock().unwrap().contains(&handle.to_owned()) {
            if let Some(handle) = BackgroundHandle::parse(handle) {
                self.processes.end(handle.pid);
            }
            return Ok(None);
        }
        let run_dir = self
            .lock()
            .iter()
            .find(|wrapper| wrapper.handle == handle)
            .map(|wrapper| wrapper.run_dir.clone());
        if let Some(run_dir) = run_dir {
            self.files.write(&exit_path(&run_dir), b"")?;
        }
        Ok(None)
    }

    /// The fake knows all process identities, so absence is definitive.
    fn wrapper_exists(&self, handle: &str) -> Result<bool> {
        self.exists(handle)
    }

    /// Whether the process of `handle` lives with the start it names.
    fn exists(&self, handle: &str) -> Result<bool> {
        Ok(BackgroundHandle::parse(handle).is_some_and(|handle| {
            handle.is(
                handle.pid,
                self.processes.start_identity(handle.pid).as_deref(),
            )
        }))
    }
}

/// The production wrapper of run `id` under `token`, as process `pid`, on
/// the queue at `db` as the wrapper writes it.
#[allow(clippy::too_many_arguments)]
fn wrap(
    db: &std::path::Path,
    id: &RunId,
    token: &LeaseToken,
    resume: bool,
    pid: u32,
    agent: &FakeAgent,
    tokens: &MemoryTokens,
    processes: &FakeProcesses,
    files: &MemoryFiles,
    clock: &dyn Clock,
) -> Result<Value> {
    // The wrapper's events are its own, not the worker's (ADR-t728-1).
    let mut queue =
        SqliteQueue::open(db)?.with_actor(ActorContext::instance(ActorRole::Wrapper, id));
    let runtimes = || Ok(Default::default());
    let toolchains = crate::domain::toolchain::Toolchains::none;
    run_session(
        Session {
            queue: &mut queue,
            db,
            provider: agent,
            other: None,
            spawner: agent,
            queue_service: tokens,
            processes,
            files,
            pid,
            start: WrapperStart::Background,
            registration_poll: REGISTRATION_POLL,
            sccache: None,
            clock,
            runtimes: &runtimes,
            toolchains: &toolchains,
            containers: &Nothing,
            telemetry: &Nothing,
        },
        id,
        token,
        resume,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        application::{
            AgentProvider, CommandSpec, Spawned, Spawner, Streams, TurnReader, TurnTarget,
            session::RuntimesRead,
        },
        domain::{
            ClaimOutcome, CommitSha, EventKind, Provider, RunPlan, TaskRun,
            headless_job::JobAccess,
            provider_switch::WorkerRoute,
            toolchain::Toolchains,
            turn::{TurnRequest, TurnSession, request_path},
            worker::{Worker, WorkerMode},
        },
        infrastructure::{adapters::ClaudeCode, codex::Codex},
    };
    use serde_json::json;
    use std::{
        cell::Cell,
        path::Path,
        sync::atomic::{AtomicUsize, Ordering},
        time::SystemTime,
    };

    use super::super::{agent::Script, harness::Wiring};

    /// Script the turn's process and output, and use the actual provider
    /// adapter to apply model arguments before the executor calls spawn.
    struct TurnProbe<'a> {
        scripted: &'a FakeAgent,
        adapter: &'a (dyn AgentProvider + Sync),
        launched: Mutex<Vec<Vec<String>>>,
    }

    impl AgentProvider for TurnProbe<'_> {
        fn preflight(&self) -> Result<()> {
            Ok(())
        }

        fn wait_interval(&self) -> Duration {
            Duration::ZERO
        }

        fn review_command(
            &self,
            run: &TaskRun,
            tree: &Path,
            prompt: &str,
            access: JobAccess,
        ) -> Result<CommandSpec> {
            self.scripted.review_command(run, tree, prompt, access)
        }

        fn turn_command(
            &self,
            target: &TurnTarget<'_>,
            prompt: &str,
            session: TurnSession<'_>,
        ) -> Result<CommandSpec> {
            self.scripted.turn_command(target, prompt, session)
        }

        fn turn_reader(&self, toolchains: &Toolchains) -> Result<Box<dyn TurnReader>> {
            self.scripted.turn_reader(toolchains)
        }

        fn select_model(&self, command: &mut CommandSpec, model: &str, effort: &str) {
            self.adapter.select_model(command, model, effort);
        }

        fn select_worker_model(
            &self,
            command: &mut CommandSpec,
            model: &str,
            effort: &str,
            configured: Option<&str>,
        ) {
            self.adapter
                .select_worker_model(command, model, effort, configured);
        }
    }

    impl Spawner for TurnProbe<'_> {
        fn spawn(&self, command: &CommandSpec, streams: Streams<'_>) -> Result<Box<dyn Spawned>> {
            // The scripted command already names the run directory and
            // session. Capture the options added by the production executor.
            self.launched.lock().unwrap().push(
                command
                    .get_args()
                    .skip(2)
                    .map(|arg| arg.to_str().unwrap().to_owned())
                    .collect(),
            );
            self.scripted.spawn(command, streams)
        }
    }

    /// The wrapper reads the clock on every polling pass. Bound those
    /// passes even if it stops consuming the requests already in memory.
    struct TurnClock<'a> {
        clock: &'a dyn Clock,
        reads: AtomicUsize,
    }

    impl Clock for TurnClock<'_> {
        fn system_time(&self) -> SystemTime {
            assert!(
                self.reads.fetch_add(1, Ordering::Relaxed) < 64,
                "the wrapper did not complete its three scripted turn attempts"
            );
            self.clock.system_time()
        }

        fn monotonic(&self) -> Instant {
            self.clock.monotonic()
        }
    }

    #[test]
    fn each_worker_turn_reads_settings_and_only_codex_uses_the_override() {
        let codex = Codex::new("codex".into());
        let claude = ClaudeCode {
            executable: "claude".into(),
        };
        let claude_args = ["--model", "claude-opus-5-5", "--effort", "medium"];
        for (provider, adapter, expected) in [
            (
                Provider::Codex,
                &codex as &(dyn AgentProvider + Sync),
                [
                    [
                        "-m",
                        "gpt-first",
                        "-c",
                        r#"model_reasoning_effort="medium""#,
                    ],
                    [
                        "-m",
                        "gpt-second",
                        "-c",
                        r#"model_reasoning_effort="medium""#,
                    ],
                ],
            ),
            (
                Provider::Claude,
                &claude as &(dyn AgentProvider + Sync),
                [claude_args; 2],
            ),
        ] {
            let wiring = Wiring::new(
                provider.as_str(),
                Script {
                    receipts: vec![],
                    verdicts: vec![],
                },
            );
            let task = wiring.ready_task_with(&[], &[], Some(provider));
            let token = LeaseToken::new("turn-model-test");
            let mut queue = wiring.queue();
            let ClaimOutcome::Claimed { run } = queue
                .claim_for_supervisor_in_order(
                    &CommitSha::try_from("a".repeat(40)).unwrap(),
                    &token,
                    &[task],
                    None,
                    &Default::default(),
                    &WorkerRoute::direct(&[Worker::new(provider, WorkerMode::Headless).unwrap()]),
                )
                .unwrap()
            else {
                panic!("the task must be claimed on {provider:?}");
            };
            assert_eq!(run.actual_provider(), provider);
            let run_dir = PathBuf::from("/wiring/runs").join(run.id().as_str());
            let path = |name: &str| run_dir.join(name).to_str().unwrap().to_owned();
            queue
                .plan_run(
                    run.id(),
                    &token,
                    &RunPlan {
                        repo_path: "/wiring/repo".into(),
                        run_dir: path(""),
                        worktree_path: path("worktree"),
                        receipt_path: path("receipt.json"),
                        log_path: path("session.log"),
                        branch: "dagq/worker-model-test".into(),
                    },
                )
                .unwrap();

            let processes = FakeProcesses::with(1);
            let (pid, start) = processes.start();
            let start = start_token(&start);
            let handle = BackgroundHandle {
                pid,
                start: start.clone(),
            }
            .to_string();
            queue.workspace_created(run.id(), &token, &handle).unwrap();
            queue
                .record_runtime_event(
                    run.id(),
                    EventKind::WrapperLaunched,
                    json!({"pid": pid, "start": start, "workspace_id": handle}),
                )
                .unwrap();
            // The store derives queue-local paths when the wrapper reads the run.
            let run = queue.run(run.id()).unwrap();
            let run_dir = Path::new(run.run_dir().unwrap());
            wiring
                .files
                .write(&run_dir.join("prompt.txt"), b"work")
                .unwrap();
            for seq in 1..=2 {
                let request = TurnRequest {
                    seq,
                    what: "revise request".into(),
                    prompt: "continue".into(),
                };
                wiring
                    .files
                    .write(
                        &request_path(run_dir, seq),
                        &serde_json::to_vec(&request).unwrap(),
                    )
                    .unwrap();
            }
            let probe = TurnProbe {
                scripted: &wiring.agent,
                adapter,
                launched: Mutex::default(),
            };
            let reads = Cell::new(0);
            let read = || {
                reads.set(reads.get() + 1);
                // Edit the setting between actual process launches, then
                // make the third attempted turn's shared config unreadable.
                let model = match probe.launched.lock().unwrap().len() {
                    0 => "gpt-first",
                    1 => "gpt-second",
                    _ => anyhow::bail!("dagq.toml:2: model is blank"),
                };
                Ok(RuntimesRead {
                    codex_model: Some(model.into()),
                    ..Default::default()
                })
            };
            let clock = wiring.generators().clock;
            let tokens = MemoryTokens::new(run_dir.join("service"), clock.clone());
            let clock = TurnClock {
                clock: &*clock,
                reads: AtomicUsize::new(0),
            };
            let mut queue = queue.with_actor(ActorContext::instance(ActorRole::Wrapper, run.id()));
            let error = run_session(
                Session {
                    queue: &mut queue,
                    db: &wiring.db,
                    provider: &probe,
                    other: None,
                    spawner: &probe,
                    queue_service: &tokens,
                    processes: &processes,
                    files: &*wiring.files,
                    pid,
                    start: WrapperStart::Background,
                    registration_poll: Duration::ZERO,
                    sccache: None,
                    clock: &clock,
                    runtimes: &read,
                    toolchains: &Toolchains::none,
                    containers: &Nothing,
                    telemetry: &Nothing,
                },
                run.id(),
                &token,
                false,
            )
            .unwrap_err();

            assert_eq!(
                format!("{error:#}"),
                "the runtimes of dagq.toml could not be read; dagq refuses to start the worker's turn rather than run it on the host: dagq.toml:2: model is blank",
                "{provider:?}: {} settings reads, argv {:?}",
                reads.get(),
                probe.launched.lock().unwrap()
            );
            assert_eq!(reads.get(), 3, "{provider:?}");
            assert_eq!(*probe.launched.lock().unwrap(), expected, "{provider:?}");
            assert_eq!(
                wiring.agent.turn_sessions(),
                [
                    format!("start {}", run.id()),
                    format!("resume {}", run.id())
                ]
            );
        }
    }
}
