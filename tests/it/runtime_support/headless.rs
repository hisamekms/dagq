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
    let reviewer =
        Arc::new(TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(recoveries));
    let options = SuperviseOptions {
        stall: Some(stall),
        ..supervise_options(parallel, true)
    };
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
    (reviewer, supervisor)
}
