//! The helpers of the tests of the cleanup of the runs left in the middle
//! (task 1289): a run that waits for a person's answer.
use super::*;

/// A run whose review returned `concern`: `awaiting_integration` with no
/// lease and no live session, its `approve_landing` ask open. Its worktree
/// then gets build outputs, and an uncommitted source with `uncommitted`;
/// returns the run and the ask.
pub fn run_awaiting_an_answer(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    uncommitted: bool,
) -> (TaskRun, dagq::domain::Ask) {
    let reviewer = TestReviewer::new(&[verdict("concern", &["a finding"], "first")]);
    let outcome = supervise_reviewed_owned(db, repo, backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    let ask = queue.asks(Default::default()).unwrap()[0].clone();
    assert_eq!(ask.kind, AskKind::ApproveLanding);
    let worktree = Path::new(run.worktree_path().unwrap());
    for dir in ["target/debug", "llvm-cov-target/debug"] {
        fs::create_dir_all(worktree.join(dir)).unwrap();
        fs::write(worktree.join(dir).join("big"), vec![0u8; 65536]).unwrap();
    }
    if uncommitted {
        fs::write(worktree.join("uncommitted.rs"), "fn main() {}\n").unwrap();
    }
    (run, ask)
}
