//! The fixture of the headless worker's runtime tests (ADR-t813-1): a
//! task for a headless Claude worker whose turns the stub `claude` of
//! [`headless_claude`] runs, and a supervisor on a thread
//! (`runtime_headless`, `runtime_headless_stall`).
use super::*;
use anyhow::Context as _;
use dagq::domain::{EventKind, Provider, worker::WorkerMode};

/// The fixture's task, canceled, and in its place task 2 (`test task`) for
/// a headless Claude worker that requires `evidence`; the backend runs its
/// turns with the stub `claude` of [`headless_claude`] (a turn does `say
/// working` until the test sets its turns).
pub fn headless_fixture(evidence: &[EvidenceCheck]) -> (Fixture, PathBuf, PathBuf, TestWorkspace) {
    let (dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let task = queue
        .add(NewTask {
            title: "test task".into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: evidence.to_vec(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: Some(Provider::Claude),
            worker_mode: Some(WorkerMode::Headless),
            wait_for_build: false,
        })
        .unwrap();
    assert_eq!(task.id(), TASK);
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    let mut backend = TestWorkspace::new(&db, false, "exit 99");
    backend.headless = Some(headless_claude(dir.path(), &db));
    (dir, repo, db, backend)
}

pub const TASK: TaskId = TaskId::new(2);

/// A turn that commits and writes a receipt naming the new head.
pub const FINISH: &str = r#"commit work; receipt "$(git rev-parse HEAD)"; say finished"#;

pub fn detail(db: &Path) -> dagq::domain::TaskDetail {
    SqliteQueue::open(db).unwrap().show(TASK).unwrap()
}

/// Supervise the task on a thread with `stall` and the recovery jobs'
/// `recoveries`, and a review that passes.
pub fn supervise_thread(
    db: &Path,
    repo: &Path,
    backend: Arc<TestWorkspace>,
    stall: dagq::domain::stall::StallConfig,
    recoveries: &[String],
) -> (Arc<TestReviewer>, thread::JoinHandle<Result<Value>>) {
    supervise_thread_in(db, repo, backend, stall, recoveries, 4)
}

/// [`supervise_thread`] with `parallel` slots.
pub fn supervise_thread_in(
    db: &Path,
    repo: &Path,
    backend: Arc<TestWorkspace>,
    stall: dagq::domain::stall::StallConfig,
    recoveries: &[String],
    parallel: usize,
) -> (Arc<TestReviewer>, thread::JoinHandle<Result<Value>>) {
    let (reviewer, supervisor, _) =
        supervise_thread_counted_in(db, repo, backend, stall, recoveries, parallel);
    (reviewer, supervisor)
}

/// [`supervise_thread`], and the count of the supervisor's passes.
pub fn supervise_thread_counted(
    db: &Path,
    repo: &Path,
    backend: Arc<TestWorkspace>,
    stall: dagq::domain::stall::StallConfig,
    recoveries: &[String],
) -> (
    Arc<TestReviewer>,
    thread::JoinHandle<Result<Value>>,
    Arc<AtomicU64>,
) {
    supervise_thread_counted_in(db, repo, backend, stall, recoveries, 4)
}

/// [`supervise_thread_counted`] with `parallel` slots.
fn supervise_thread_counted_in(
    db: &Path,
    repo: &Path,
    backend: Arc<TestWorkspace>,
    stall: dagq::domain::stall::StallConfig,
    recoveries: &[String],
    parallel: usize,
) -> (
    Arc<TestReviewer>,
    thread::JoinHandle<Result<Value>>,
    Arc<AtomicU64>,
) {
    let reviewer =
        Arc::new(TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(recoveries));
    let options = SuperviseOptions {
        stall: Some(stall),
        ..supervise_options(parallel, true)
    };
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend, reviewer) = (
            db.to_owned(),
            repo.to_owned(),
            backend.clone(),
            reviewer.clone(),
        );
        thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
        })
    };
    (reviewer, supervisor, passes)
}

/// Keep startup scheduling out of tests of a running turn's time limits.
/// The real child is already running, but the wrapper cannot start its
/// limit/silence timers until `spawn` returns. Each turn (including resume)
/// must publish a fresh marker after its prerequisites are ready.
pub struct ReadySpawner {
    pub inner: StubSpawner,
    pub ready: Option<PathBuf>,
}

impl Spawner for ReadySpawner {
    fn spawn(&self, spec: &CommandSpec, streams: Streams<'_>) -> Result<Box<dyn Spawned>> {
        if let Some(ready) = &self.ready {
            match fs::remove_file(ready) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let mut child = self.inner.spawn(spec, streams)?;
        if let Some(ready) = &self.ready {
            let _waiting = common::within(common::STEP_LIMIT, "the turn's ready marker");
            while !ready.exists() {
                // A turn may write the marker and exit between the checks.
                ensure!(
                    child.try_wait()?.is_none() || ready.exists(),
                    "turn exited before {}",
                    ready.display()
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
        Ok(child)
    }
}

/// Starts each command as if the wrapper's own environment held `env`:
/// those variables come before the command's own, so one the command
/// removes (a Codex turn's `RUSTC_WRAPPER`, ADR-t1215-1) is not inherited,
/// as with a real wrapper whose workspace got the `[run.env]`.
pub struct InheritingSpawner<S> {
    pub inner: S,
    pub env: Vec<(String, String)>,
}

impl<S: Spawner> Spawner for InheritingSpawner<S> {
    fn spawn(&self, spec: &CommandSpec, streams: Streams<'_>) -> Result<Box<dyn Spawned>> {
        let mut inherited = CommandSpec::new(spec.get_program());
        inherited.args(spec.get_args());
        for (key, value) in &self.env {
            inherited.env(key, value);
        }
        for (key, value) in spec.get_envs() {
            match value {
                Some(value) => inherited.env(key, value),
                None => inherited.env_remove(key),
            };
        }
        if let Some(dir) = spec.get_current_dir() {
            inherited.current_dir(dir);
        }
        if spec.get_new_session() {
            inherited.new_session();
        }
        if let Some(text) = spec.get_stdin() {
            inherited.stdin(text);
        }
        self.inner.spawn(&inherited, streams)
    }
}

/// Have every turn signal readiness with the returned shell command.
pub fn ready_turn(dir: &Path, backend: &mut TestWorkspace) -> String {
    let ready = dir.join("turn-ready");
    let command = format!(": > {}", shell_join(&[ready.display().to_string()]));
    backend.headless_ready = Some(ready);
    command
}

/// The home of the stub `codex` of [`headless_codex`], next to it: its
/// rollouts are under `sessions`.
pub const CODEX_HOME: &str = "codex-home";

/// Have the stub `codex` in `dir` write each turn's `model` to its
/// thread's rollout, as Codex does, from now on.
pub fn set_codex_model(dir: &Path, model: &str) {
    fs::write(dir.join("codex-model"), model).unwrap();
}

/// One `launch_background` of [`TestWorkspace`]: the directory, the
/// command line and the environment the supervisor started a background
/// wrapper with.
#[derive(Debug, Clone)]
pub struct Launch {
    pub cwd: PathBuf,
    pub command: String,
    pub env: Vec<(String, String)>,
    pub log: PathBuf,
}

/// [`TestWorkspace`]'s background start (ADR-t1404-1), the runtime
/// tests' default (task 1439): the wrapper of the run the command names
/// runs on a thread, entered as a background one (no terminal, no
/// workspace of its own), with the stub headless agents. It goes by a
/// process of its own that lives while it runs ([`Stand`]): its handle
/// carries that process's pid and start, which the wrapper registers with
/// and the supervisor checks (ADR-t1404-1 decision 2), so each session has
/// a handle of its own. The backend lists it until it is closed, as it
/// lists a workspace (the cmux adapter asks the process instead), and
/// sends it no signal. Each session the supervisor starts (with an
/// environment) has the worker's environment without the queue's path,
/// and the database path's apostrophe quoted in its command.
pub fn launch_background(
    backend: &TestWorkspace,
    cwd: &Path,
    command: &str,
    env: &[(String, String)],
    log: &Path,
) -> Result<String> {
    assert!(command.ends_with(" '--background'"), "{command}");
    backend.launched.lock().unwrap().push(Launch {
        cwd: cwd.to_owned(),
        command: command.to_owned(),
        env: env.to_vec(),
        log: log.to_owned(),
    });
    let words: Vec<&str> = command.split(' ').map(|w| w.trim_matches('\'')).collect();
    let after = |flag: &str| {
        words
            .iter()
            .position(|w| *w == flag)
            .map(|at| words[at + 1].to_owned())
            .unwrap()
    };
    let id = RunId::new(after("--run"))?;
    let token = LeaseToken::new(after("--lease"));
    let resume = words.contains(&"--resume");
    let run = SqliteQueue::open(&backend.db)?.run(&id)?;
    if !env.is_empty() {
        assert!(command.contains("'\"'\"'"), "{command}");
        assert!(
            Path::new(run.worktree_path().unwrap())
                .join("seed.txt")
                .exists()
        );
        for (name, value) in [
            ("DAGQ_ROLE", "worker".to_owned()),
            ("DAGQ_ACTOR_ID", format!("worker:{}", run.id())),
            ("DAGQ_RUN_ID", run.id().to_string()),
            ("DAGQ_TASK_ID", run.task_id().to_string()),
        ] {
            assert!(
                env.iter().any(|(k, v)| k == name && *v == value),
                "{name}={value} in {env:?}"
            );
        }
        assert!(!env.iter().any(|(k, _)| k == "DAGQ_QUEUE"), "{env:?}");
    }
    if backend.fail || backend.fail_tasks.lock().unwrap().contains(&run.task_id()) {
        bail!("injected background launch failure");
    }
    let run_dir = run.run_dir().unwrap().to_owned();
    if resume {
        let _ = fs::remove_file(exit_request_path(&run_dir));
        let _ = fs::remove_file(resume_message_path(&run_dir));
    }
    let claude = backend.claude_for(&run, resume)?;
    let (provider, other) = headless_provider(&run, claude.as_deref(), backend.codex.as_deref());
    let stand = Stand::start()?;
    let handle = stand.handle.clone();
    let pid = stand.pid;
    // The process runs but starts no wrapper, which never registers.
    if (resume && backend.resume_no_session) || (!resume && backend.no_session) {
        backend.sessions.lock().unwrap().push((
            handle.clone(),
            TestSession {
                run_id: id,
                run_dir,
                worker: None,
            },
        ));
        backend.stands.lock().unwrap().push((handle.clone(), stand));
        return Ok(handle);
    }
    let (db, ready) = (backend.db.clone(), backend.headless_ready.clone());
    let sccache = backend.sccache.clone();
    let inherited_env = backend.inherited_env.clone();
    let worker = thread::spawn(move || {
        let spawner = ReadySpawner {
            inner: StubSpawner { db: db.clone() },
            ready,
        };
        let spawner = InheritingSpawner {
            inner: spawner,
            env: if sccache.is_some() {
                inherited_env
            } else {
                Vec::new()
            },
        };
        let returned = runtime::session_in_background_as(
            &db,
            &id,
            &token,
            &provider,
            Some(&other),
            &spawner,
            resume,
            pid,
            sccache,
        );
        // The wrapper's process ends with it.
        drop(stand);
        returned
    });
    let mut sessions = backend.sessions.lock().unwrap();
    sessions.push((
        handle.clone(),
        TestSession {
            run_id: run.id().clone(),
            run_dir,
            worker: Some(worker),
        },
    ));
    Ok(handle)
}

/// The process a background wrapper of [`launch_background`] goes by: it
/// lives until it is dropped (the wrapper returned) or this test process
/// is gone; its only child is a `sleep` of a second.
pub struct Stand {
    pub pid: u32,
    pub handle: String,
    child: std::process::Child,
}

impl Stand {
    pub fn start() -> Result<Self> {
        let child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "while kill -0 {} 2>/dev/null; do sleep 1; done",
                std::process::id()
            ))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let pid = child.id();
        let start = SystemProcesses
            .start_identity(pid)
            .context("the background wrapper's start")?;
        let handle =
            dagq::domain::background_wrapper::BackgroundHandle::new(pid, &start).to_string();
        Ok(Self { pid, handle, child })
    }
}

impl Drop for Stand {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The background starts of resumed sessions (`--resume`), in order.
pub fn resume_launches(backend: &TestWorkspace) -> Vec<Launch> {
    backend
        .launched
        .lock()
        .unwrap()
        .iter()
        .filter(|launch| launch.command.contains("'--resume'"))
        .cloned()
        .collect()
}

/// Put `[headless] wrapper = "workspace"` in the main checkout's
/// `dagq.toml`, committed: a worker accepts and ignores it, and its session
/// wrapper starts in the background all the same (ADR-t1433-3 decision 2).
pub fn workspace_wrapper_setting(repo: &Path) {
    fs::write(
        repo.join("dagq.toml"),
        "[headless]\nwrapper = \"workspace\"\n",
    )
    .unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-qm", "the workspace wrapper setting"]);
}

/// Write `prompt` as the next request (`what`) of the headless session of
/// `run`, the way the supervisor writes one (a temporary file renamed into
/// `turns/`), and return its number: the test stands in for whatever has
/// the session take another turn (a headless session takes none by itself).
pub fn write_turn_request(run: &TaskRun, prompt: &str, what: &str) -> u64 {
    use dagq::domain::turn::{TurnRequest, next_seq, request_path, turns_dir};
    let run_dir = Path::new(run.run_dir().unwrap());
    let turns = turns_dir(run_dir);
    fs::create_dir_all(&turns).unwrap();
    let names: Vec<String> = fs::read_dir(&turns)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    let seq = next_seq(names.iter().map(String::as_str));
    let request = TurnRequest {
        seq,
        what: what.to_owned(),
        prompt: prompt.to_owned(),
    };
    let path = request_path(run_dir, seq);
    let written = path.with_extension("json.tmp");
    fs::write(&written, serde_json::to_string(&request).unwrap()).unwrap();
    fs::rename(&written, &path).unwrap();
    seq
}

/// [`write_turn_request`] with its `turn_requested`, as the supervisor's
/// request records it (ADR-t813-1): what a supervisor that died after
/// sending a request left.
pub fn request_turn_left_by_a_dead_supervisor(db: &Path, run: &TaskRun, prompt: &str, what: &str) {
    let seq = write_turn_request(run, prompt, what);
    SqliteQueue::open(db)
        .unwrap()
        .record_runtime_event(
            run.id(),
            EventKind::TurnRequested,
            json!({"seq": seq, "what": what, "workspace_id": run.workspace_id()}),
        )
        .unwrap();
}
