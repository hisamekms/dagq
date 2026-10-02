//! The fixture of the headless worker's runtime tests (ADR-t813-1): a
//! task for a headless Claude worker whose turns the stub `claude` of
//! [`headless_claude`] runs, and a supervisor on a thread
//! (`runtime_headless`, `runtime_headless_stall`).
use super::*;
use dagq::domain::{Provider, worker::WorkerMode};

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
