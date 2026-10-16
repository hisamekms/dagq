//! The fixture of the runtime tests of a headless Codex worker
//! (ADR-t813-1, ADR-t813-3): a task for a Codex worker whose turns the stub
//! `codex` of [`headless_codex`] runs, and a supervisor given that stub on
//! a thread, for `runtime_codex` and the other tests that need a Codex run.
use super::*;
use dagq::domain::{Provider, worker::WorkerMode};

pub const TASK: TaskId = TaskId::new(2);

/// A turn that commits and writes a receipt naming the new head.
pub const FINISH: &str = r#"commit work; receipt "$(git rev-parse HEAD)"; say finished"#;

/// The fixture's task, canceled, and in its place task 2 (`test task`) for
/// a Codex worker; the backend runs its turns with the stub `codex` of
/// [`headless_codex`], which the supervisor is given too (so that it
/// claims the task).
pub fn codex_fixture() -> (Fixture, PathBuf, PathBuf, TestWorkspace, PathBuf) {
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
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: Some(Provider::Codex),
            worker_mode: Some(WorkerMode::Headless),
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap();
    assert_eq!(task.id(), TASK);
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    let codex = headless_codex(dir.path(), &db);
    let mut backend = TestWorkspace::new(&db, false, "exit 99");
    backend.codex = Some(codex.clone());
    (dir, repo, db, backend, codex)
}

pub fn detail(db: &Path) -> dagq::domain::TaskDetail {
    SqliteQueue::open(db).unwrap().show(TASK).unwrap()
}

/// Supervise with the stub `codex` on a thread, with reviews of
/// `verdicts` in order.
pub fn supervise_thread(
    db: &Path,
    repo: &Path,
    backend: Arc<TestWorkspace>,
    codex: &Path,
    verdicts: &[String],
) -> thread::JoinHandle<Result<Value>> {
    let reviewer = TestReviewer::new(verdicts);
    let options = SuperviseOptions {
        codex: codex.to_owned(),
        ..supervise_options(4, true)
    };
    let (db, repo) = (db.to_owned(), repo.to_owned());
    thread::spawn(move || {
        runtime::supervise_with_reviewer(
            &db,
            &repo,
            &*backend,
            &claude_stub(&db),
            &reviewer,
            Path::new(env!("CARGO_BIN_EXE_dagq")),
            &options,
        )
    })
}

/// Wait for the supervisor thread and its sessions.
pub fn finished(backend: &TestWorkspace, supervisor: thread::JoinHandle<Result<Value>>) -> Value {
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    outcome
}

/// The one open ask of `kind`'s reason once it opened.
pub fn open_ask(db: &Path, matches: impl Fn(&dagq::domain::Ask) -> bool) -> dagq::domain::Ask {
    wait_until(db, common::STEP_LIMIT, |queue| {
        queue
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .any(&matches)
    });
    SqliteQueue::open(db)
        .unwrap()
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(matches)
        .unwrap()
}
