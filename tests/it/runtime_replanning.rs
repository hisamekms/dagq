//! Runtime tests: the supervisor with a run whose task a replanning
//! operation guards (ADR-t1521-2). Nothing parks a run yet, so a review job
//! script puts the task under a guarding operation as the trusted
//! runtime's move would, and the test lifts it again where the run has to
//! go on: the revise of its verdict is withheld and goes to the landing's
//! ask, its approved landing does not hold back another run's, and in the
//! landing slot it waits for the operation's outcome rather than failing.
use crate::common;
use crate::runtime_support;

use runtime_support::*;
use rusqlite::OptionalExtension;

/// A shell command that puts task `task` under a guarding replanning
/// operation (`quiescing`, past the pause's fence) on its latest run.
fn guard(db: &Path, task: i64) -> String {
    let conditions = r#"{"title":"t","description":"d","acceptance":[],"paths":[],"verify":[],"evidence":[],"predecessors":[],"successors":[],"goal":null}"#;
    let sql = format!(
        "INSERT INTO replan_operations(task_id, run_id, origin, status, generation, reason,
           conditions, task_version, diagnosis, requested_by, requested_by_id, created_at,
           updated_at)
         SELECT {task}, id, 'runtime', 'quiescing', 2, 'replanning in the test', '{conditions}',
           'v', '{{\"evidence\":[]}}', 'supervisor', 'supervisor', 0, 0
         FROM task_runs WHERE task_id={task} ORDER BY rowid DESC LIMIT 1;"
    );
    format!(
        "sqlite3 -cmd '.timeout 5000' {} {} || exit 97",
        shell_path(db),
        dagq::infrastructure::adapters::shell_quote(&sql)
    )
}

/// End every open operation of task `task` as a continuation, as the
/// runtime's outcome would.
fn lift(db: &Path, task: i64) {
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE replan_operations SET status='continuing', closed_at=1
             WHERE task_id=?1 AND closed_at IS NULL",
            [task],
        )
        .unwrap();
}

/// How many events of `kind` run `run` has.
fn count(db: &Path, run: &str, kind: &str) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM run_events WHERE run_id=?1 AND kind=?2",
            [run, kind],
            |r| r.get(0),
        )
        .unwrap()
}

/// The approve_landing ask of `run`, answered `land`.
fn land(queue: &mut SqliteQueue, run: &RunId) -> dagq::domain::Ask {
    let ask = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.run_id.as_ref() == Some(run))
        .unwrap();
    assert_eq!(ask.kind, dagq::domain::AskKind::ApproveLanding);
    queue.answer(ask.id, "land").unwrap();
    ask
}

/// A `revise` verdict on a run whose task is being replanned is not sent:
/// `revise_withheld` records it and the verdict goes to the landing's ask.
/// `land` queues the run, which waits for the operation's outcome without
/// holding back another run approved after it, which lands.
#[test]
fn a_guarded_runs_revise_is_withheld_and_its_approved_landing_lets_another_land() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[format!(
        "{} && {}",
        guard(&db, 1),
        verdict("revise", &["split the two parts"], "too big")
    )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let guarded = detail.runs[0].clone();
    assert_eq!(guarded.status(), RunStatus::AwaitingIntegration);
    assert!(payloads(&detail, "revise_requested").is_empty());
    assert!(session_texts(&guarded).is_empty(), "no revise was typed");
    let withheld = payloads(&detail, "revise_withheld");
    assert_eq!(withheld.len(), 1, "{withheld:?}");
    assert_eq!(withheld[0]["guard"]["status"], "quiescing");
    assert_eq!(withheld[0]["guard"]["task"], 1);
    assert_eq!(withheld[0]["reasons"], json!(["split the two parts"]));
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "revise_withheld") < position(&kinds, "ask_opened"));
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].kind, dagq::domain::AskKind::ApproveLanding);
    assert!(
        asks[0].question.contains("task 1 is being replanned"),
        "{}",
        asks[0].question
    );

    // Another run, whose review asks a person too.
    let other = add_ready_task(&mut queue, "other task", &[]);
    let reviewer = TestReviewer::new(&[verdict("concern", &["a judgment"], "decide")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let second = queue.show(other).unwrap().runs[0].clone();
    assert_eq!(second.status(), RunStatus::AwaitingIntegration);

    // Both are approved, the guarded one first: it waits, the other lands.
    land(&mut queue, guarded.id());
    land(&mut queue, second.id());
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let landed = queue.show(other).unwrap();
    assert_eq!(landed.task.status(), TaskStatus::Completed);
    assert_eq!(landed.runs[0].status(), RunStatus::Integrated);
    let held = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(held.task.status(), TaskStatus::InProgress);
    // The two runs change the same file: after the other landed, the
    // landing recheck may park the guarded run for a resume, which does
    // not start while the operation guards its task.
    assert!(
        matches!(
            held.runs[0].status(),
            RunStatus::AwaitingIntegration | RunStatus::NeedsSession
        ),
        "{:?}",
        held.runs[0].status()
    );
    for kind in ["integration_started", "resume_started", "runtime_error"] {
        assert_eq!(count(&db, guarded.id().as_str(), kind), 0, "{kind}");
    }
    // The landed run is the other one, on the base the guarded one left.
    assert_ne!(git_out(&repo, &["rev-parse", "main"]), base);
}

/// A run that passed its review and reached the landing slot while its
/// task is being replanned waits there, its lease kept, and begins no
/// landing; nothing fails. Once the operation ends it lands.
#[test]
fn a_guarded_run_in_the_landing_slot_waits_for_the_outcome_and_then_lands() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[format!(
        "{} && {}",
        guard(&db, 1),
        verdict("pass", &[], "meets the acceptance")
    )]);
    // While the supervisor runs: once the run waits in the landing slot,
    // a few passes later it still has not begun to land, nor failed, and
    // holds its lease; then the operation ends.
    let watcher = {
        let db = db.clone();
        std::thread::spawn(move || {
            let _waiting = common::within(common::STEP_LIMIT, "the run to reach the landing slot");
            let connection = Connection::open(&db).unwrap();
            let run: String = loop {
                let waiting: Option<String> = connection
                    .query_row(
                        "SELECT run_id FROM run_events WHERE kind='run_phase_changed'
                         AND json_extract(payload,'$.phase')='awaiting_slot' LIMIT 1",
                        [],
                        |r| r.get(0),
                    )
                    .optional()
                    .unwrap();
                if let Some(run) = waiting {
                    break run;
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            std::thread::sleep(TEST_TICK * 20);
            let leased: i64 = connection
                .query_row(
                    "SELECT count(*) FROM run_leases WHERE run_id=?1",
                    [&run],
                    |r| r.get(0),
                )
                .unwrap();
            let seen = (
                count(&db, &run, "integration_started"),
                count(&db, &run, "runtime_error"),
                leased,
            );
            lift(&db, 1);
            seen
        })
    };
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    let seen = watcher.join().unwrap();
    assert_eq!(
        seen,
        (0, 0, 1),
        "landing, failures and leases while guarded"
    );
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_landed_run(&detail.runs[0], &repo, &base);
    assert!(payloads(&detail, "runtime_error").is_empty());
}
