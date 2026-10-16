//! Runtime tests: the supervisor with a run whose task a replanning
//! operation guards (ADR-t1521-2). A review job script moves the task's
//! operation past the pause's fence as the trusted runtime would: the
//! verdict of the review is not acted on (no revise is sent, no landing's
//! ask is opened, no landing begins) and the run is parked out of its slot
//! with its work saved.
use crate::runtime_support;

use runtime_support::*;

/// A shell command that puts task `task` under an operation past the
/// pause's fence (`pause_requested`) on its latest run.
fn guard(db: &Path, task: i64) -> String {
    let conditions = r#"{"title":"t","description":"d","acceptance":[],"paths":[],"verify":[],"evidence":[],"predecessors":[],"successors":[],"goal":null}"#;
    let sql = format!(
        "INSERT INTO replan_operations(task_id, run_id, origin, status, generation, reason,
           conditions, task_version, diagnosis, requested_by, requested_by_id, created_at,
           updated_at)
         SELECT {task}, id, 'runtime', 'pause_requested', 2, 'replanning in the test', '{conditions}',
           'v', '{{\"evidence\":[]}}', 'supervisor', 'supervisor', unixepoch(), unixepoch()
         FROM task_runs WHERE task_id={task} ORDER BY rowid DESC LIMIT 1;"
    );
    format!(
        "sqlite3 -cmd '.timeout 5000' {} {} || exit 97",
        shell_path(db),
        dagq::infrastructure::adapters::shell_quote(&sql)
    )
}

/// The status and snapshot of task `task`'s operation.
fn operation(db: &Path, task: i64) -> (String, Option<String>) {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT status, snapshot_id FROM replan_operations WHERE task_id=?1",
            [task],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

/// A `revise` verdict on a run whose task is being replanned is not sent:
/// `revise_withheld` records it, no landing's ask opens, and the run is
/// parked with its slot given back, which another task takes.
#[test]
fn a_guarded_runs_revise_is_withheld_and_the_run_is_parked() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[
        format!(
            "{} && {}",
            guard(&db, 1),
            verdict("revise", &["split the two parts"], "too big")
        ),
        verdict("pass", &[], "fine"),
    ]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let other = add_ready_task(&mut queue, "other task", &[]);
    let options = supervise_options(1, true);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let guarded = detail.runs[0].clone();
    assert_eq!(guarded.status(), RunStatus::NeedsSession);
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert!(payloads(&detail, "revise_requested").is_empty());
    let withheld = payloads(&detail, "revise_withheld");
    assert_eq!(withheld.len(), 1, "{withheld:?}");
    assert_eq!(withheld[0]["reasons"], json!(["split the two parts"]));
    let kinds = event_kinds(&detail);
    assert!(kinds.contains(&"replan_parked"), "{kinds:?}");
    assert!(!kinds.contains(&"ask_opened"), "{kinds:?}");
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    let (status, snapshot) = operation(&db, 1);
    assert_eq!(status, "parked");
    assert!(snapshot.is_some());
    assert_eq!(
        queue.show(other).unwrap().task.status(),
        TaskStatus::Completed
    );
}

/// A run that passed its review while its task is being replanned begins
/// no landing: it is parked, and its lease given back.
#[test]
fn a_guarded_run_that_passed_is_parked_rather_than_landed() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[format!(
        "{} && {}",
        guard(&db, 1),
        verdict("pass", &[], "meets the acceptance")
    )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::NeedsSession);
    let kinds = event_kinds(&detail);
    for kind in ["integration_started", "runtime_error"] {
        assert!(!kinds.contains(&kind), "{kind} in {kinds:?}");
    }
    assert!(kinds.contains(&"replan_parked"), "{kinds:?}");
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert_eq!(operation(&db, 1).0, "parked");
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), base);
}
