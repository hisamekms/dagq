//! Runtime tests: the supervisor with a run whose task a replanning
//! operation guards (ADR-t1521-2). A review job script moves the task's
//! operation past the pause's fence as the trusted runtime would: the
//! verdict of the review is not acted on (no revise is sent, no landing's
//! ask is opened, no landing begins) and the run is parked out of its slot
//! with its work saved, or, once the operation parks it no more, gives
//! its slot back.
use crate::runtime_support;

use dagq::domain::task_replan::{ReplanMove, ReplanStatus};
use dagq::domain::{ReplanId, TaskDetail};
use runtime_support::*;

/// A shell command that puts task `task` under an operation past the
/// pause's fence (`pause_requested`) on its latest run.
fn guard(db: &Path, task: i64) -> String {
    guard_in(db, task, "pause_requested")
}

/// A shell command that puts task `task` under an operation in `status`
/// on its latest run.
fn guard_in(db: &Path, task: i64, status: &str) -> String {
    let conditions = r#"{"title":"t","description":"d","acceptance":[],"paths":[],"verify":[],"evidence":[],"predecessors":[],"successors":[],"goal":null}"#;
    let sql = format!(
        "INSERT INTO replan_operations(task_id, run_id, origin, status, generation, reason,
           conditions, task_version, diagnosis, requested_by, requested_by_id, created_at,
           updated_at)
         SELECT {task}, id, 'runtime', '{status}', 2, 'replanning in the test', '{conditions}',
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
/// `revise_withheld` records it, no landing's ask opens, and the review
/// waits held for the park, which gives the slot back, and another task
/// takes it.
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

/// End task `task`'s operation in a withdrawal, as the trusted runtime
/// would.
fn withdraw(db: &Path, task: TaskId) {
    let mut queue = SqliteQueue::open(db).unwrap();
    let id: i64 = Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT id FROM replan_operations WHERE task_id=?1",
            [task.as_i64()],
            |r| r.get(0),
        )
        .unwrap();
    let op = queue.replan_operation(ReplanId::new(id)).unwrap();
    queue
        .move_replan(
            op.id,
            op.generation,
            &ReplanMove {
                to: ReplanStatus::Withdrawn,
                reason: "the test withdraws it".into(),
                snapshot_id: None,
                diagnosis: None,
            },
        )
        .unwrap();
}

/// A run of task 1 whose review moves the task under an operation whose
/// park is over (`blocked`) and gives `verdict`, then passes: supervised
/// with one slot until the run rests, and again once the operation is
/// withdrawn. The task's detail at each of the two ends, and main before
/// and after.
fn rest_then_withdraw(verdict_script: String) -> (TaskDetail, TaskDetail, String, String) {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.resume_script_for(1, IDLE_AGENT);
    let base = git_out(&repo, &["rev-parse", "main"]);
    let reviewer = TestReviewer::new(&[
        format!("{} && {verdict_script}", guard_in(&db, 1, "blocked")),
        verdict("pass", &[], "meets the acceptance"),
    ]);
    let options = supervise_options(1, true);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let rested = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), base);
    assert_eq!(operation(&db, 1).0, "blocked");
    withdraw(&db, TaskId::new(1));
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let landed = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    (rested, landed, base, git_out(&repo, &["rev-parse", "main"]))
}

/// Assert that `detail`'s run rested out of its slot for the operation
/// whose park is over: `needs_session` as the park leaves a run
/// (`replan_parked` naming the operation's status), its lease given back,
/// its session closed, and no landing begun nor ask opened.
fn assert_rested(detail: &TaskDetail) {
    let run = &detail.runs[0];
    let kinds = event_kinds(detail);
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert_eq!(run.status(), RunStatus::NeedsSession, "{kinds:?}");
    let parked = payloads(detail, "replan_parked");
    assert_eq!(parked.len(), 1, "{parked:?}");
    assert_eq!(parked[0]["fence"], "blocked");
    assert!(parked[0]["operation_id"].is_i64(), "{parked:?}");
    let at = |kind: &str| {
        kinds
            .iter()
            .rposition(|k| *k == kind)
            .unwrap_or_else(|| panic!("no {kind} in {kinds:?}"))
    };
    assert!(at("workspace_closed") < at("replan_parked"), "{kinds:?}");
    assert!(at("replan_parked") < at("lease_released"), "{kinds:?}");
    for kind in ["integration_started", "ask_opened", "revise_requested"] {
        assert!(!kinds.contains(&kind), "{kind} in {kinds:?}");
    }
}

/// Assert that `detail`'s run, resumed after the withdrawal, landed only
/// through a new validation and review.
fn assert_landed_after_the_outcome(detail: &TaskDetail) {
    let kinds = event_kinds(detail);
    assert_eq!(detail.task.status(), TaskStatus::Completed, "{kinds:?}");
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    let at = |kind: &str| {
        kinds
            .iter()
            .position(|k| *k == kind)
            .unwrap_or_else(|| panic!("no {kind} in {kinds:?}"))
    };
    let last = |kind: &str| kinds.iter().rposition(|k| *k == kind).unwrap();
    assert!(at("replan_parked") < at("resume_started"), "{kinds:?}");
    assert!(at("resume_started") < last("review_started"), "{kinds:?}");
    assert!(
        last("review_started") < at("integration_started"),
        "{kinds:?}"
    );
    assert_eq!(payloads(detail, "review_finished").len(), 2);
}

/// A passed run whose task an operation guards but parks no more (its park
/// ended `blocked`) does not wait for its landing in its slot: on its way
/// there it rests out of the slot as the park leaves a run, and once the
/// operation ends the resume takes it on and it lands through a new
/// review.
#[test]
fn a_passed_run_whose_operation_parks_no_more_rests_out_of_its_slot_and_lands_after_the_outcome() {
    let (rested, landed, base, main) =
        rest_then_withdraw(verdict("pass", &[], "meets the acceptance"));
    assert_rested(&rested);
    let reviewed = payloads(&rested, "review_finished");
    assert_eq!(reviewed.len(), 1, "{reviewed:?}");
    assert_eq!(reviewed[0]["verdict"], "pass", "{reviewed:?}");
    assert_landed_after_the_outcome(&landed);
    assert_ne!(main, base);
}

/// A `revise` verdict on a run whose task an operation guards but parks no
/// more is not sent and opens no ask: `revise_withheld` records it, the run
/// rests out of its slot with its session closed, and once the operation
/// ends the resume takes it on and it lands through a new review.
#[test]
fn a_withheld_revise_under_an_operation_that_parks_no_more_rests_the_run_out_of_its_slot() {
    let (rested, landed, base, main) =
        rest_then_withdraw(verdict("revise", &["split the two parts"], "too big"));
    assert_rested(&rested);
    let withheld = payloads(&rested, "revise_withheld");
    assert_eq!(withheld.len(), 1, "{withheld:?}");
    assert_eq!(withheld[0]["reasons"], json!(["split the two parts"]));
    assert_eq!(withheld[0]["guard"]["status"], "blocked");
    assert_landed_after_the_outcome(&landed);
    assert_ne!(main, base);
}

/// A passed run that waits in the landing slot for its landing's turn (the
/// automatic update's gate, which the test holds, keeps the landing checks
/// from starting) and whose task an operation then guards past its park
/// does not wait there: it begins no landing, rests out of the slot with
/// its lease given back, and another task, added while it held the only
/// worker's slot, takes the slot and lands once the gate is let go.
#[test]
fn a_run_waiting_in_the_landing_slot_rests_out_of_it_once_its_task_is_guarded() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[
        verdict("pass", &[], "meets the acceptance"),
        verdict("pass", &[], "fine"),
    ]);
    let options = supervise_options(1, true);
    let gate = dagq::application::heavy_turn::HeavyTurn::under(db.parent().unwrap())
        .gate(&dagq::infrastructure::run_files::LocalRunFiles, |_| {})
        .unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let other = thread::scope(|scope| {
        let supervisor =
            scope.spawn(|| supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options));
        wait_until(&db, crate::common::STEP_LIMIT, |queue| {
            queue
                .show(TaskId::new(1))
                .unwrap()
                .runs
                .first()
                .is_some_and(|run| !events_of(&db, run.id(), "landing_queued").is_empty())
        });
        await_passes(&options.passes, SOME_PASSES);
        let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
        assert_eq!(run.status(), RunStatus::AwaitingIntegration);
        assert!(
            queue.run_lease(run.id()).unwrap().is_some(),
            "it waits in its slot"
        );
        let other = add_ready_task(&mut queue, "other task", &[]);
        let guarded = std::process::Command::new("sh")
            .arg("-c")
            .arg(guard_in(&db, 1, "blocked"))
            .status()
            .unwrap();
        assert!(guarded.success());
        wait_until(&db, crate::common::STEP_LIMIT, |queue| {
            queue.show(TaskId::new(1)).unwrap().runs[0].status() == RunStatus::NeedsSession
        });
        drop(gate);
        let _waiting = crate::common::within(crate::common::STEP_LIMIT, "the supervisor to return");
        let outcome = supervisor
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        other
    });
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    let kinds = event_kinds(&detail);
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert_eq!(run.status(), RunStatus::NeedsSession, "{kinds:?}");
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    let at = |kind: &str| {
        kinds
            .iter()
            .rposition(|k| *k == kind)
            .unwrap_or_else(|| panic!("no {kind} in {kinds:?}"))
    };
    assert!(at("landing_queued") < at("replan_parked"), "{kinds:?}");
    assert!(at("replan_parked") < at("lease_released"), "{kinds:?}");
    assert_eq!(payloads(&detail, "replan_parked")[0]["fence"], "blocked");
    for kind in ["integration_started", "ask_opened"] {
        assert!(!kinds.contains(&kind), "{kind} in {kinds:?}");
    }
    assert_eq!(
        queue.show(other).unwrap().task.status(),
        TaskStatus::Completed
    );
}
