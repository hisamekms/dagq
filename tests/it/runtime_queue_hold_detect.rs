//! Runtime tests: what joins the authentication and usage-limit
//! `queue_hold` asks (ADR-0047 decision 42, task 438): a worker's screen
//! at the usage limit, and headless jobs whose output shows a login that
//! ran out or the usage limit, each listed in the one ask's `affected`.
use crate::runtime_support;

use dagq::domain::{
    AskReason, ClaimOutcome, NewHold,
    queue_hold::{HoldJob, Wall},
};
use runtime_support::*;
use std::sync::atomic::AtomicBool;

/// A session stopped at Claude Code's usage limit.
const LIMIT_SCREEN: &str = "\
⏺ Bash(cargo test)
  ⎿  5-hour limit reached ∙ resets 3pm
     /upgrade to increase your usage limit.

│ ❯
  ? for shortcuts
";

fn queue_events(db: &Path, kind: &str) -> Vec<dagq::domain::RunEvent> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == kind)
        .collect()
}

fn open_holds(db: &Path) -> Vec<dagq::domain::Ask> {
    SqliteQueue::open(db)
        .unwrap()
        .asks(AskQuery {
            open: true,
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .filter(|ask| ask.kind == AskKind::QueueHold)
        .collect()
}

/// A session idle without a receipt at the usage limit is neither nudged
/// nor raised as `stalled`: it records `usage_limited` and joins the `cost`
/// ask of `subject: usage_limit`, which lists it as a run.
#[test]
fn an_idle_session_at_the_usage_limit_joins_the_cost_ask() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        r#"
commit work; idle
while [ ! -f "$EXIT.go" ]; do sleep 0.05; done
"#,
    );
    *backend.screen.lock().unwrap() = LIMIT_SCREEN.into();
    let backend = Arc::new(backend);
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        stall: Some(
            dagq::domain::stall::StallConfig::default()
                .with_millis("idle_without_receipt_secs", 200),
        ),
        ..supervise_options(2, false)
    };
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"usage_limited")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let ask = queue.hold_of(run.id()).unwrap().unwrap();
    assert_eq!(ask.reason_category, AskReason::Cost);
    assert_eq!(ask.subject.as_deref(), Some("usage_limit"));
    assert_eq!(ask.affected, [run.id().as_str()]);
    assert!(
        ask.question
            .starts_with("Claude Code reached its usage limit"),
        "{}",
        ask.question
    );
    assert!(
        ask.question
            .ends_with(&format!("\n\nAffected: run {}", run.id())),
        "{}",
        ask.question
    );
    let detail = queue.show(TaskId::new(1)).unwrap();
    let limited = payloads(&detail, "usage_limited");
    assert_eq!(limited[0]["ask_id"], json!(ask.id));
    assert!(
        limited[0]["excerpt"]
            .as_str()
            .unwrap()
            .contains("limit reached"),
        "{limited:?}"
    );
    assert!(payloads(&detail, "auth_required").is_empty());
    assert!(payloads(&detail, "stall_nudged").is_empty());
    assert!(stalled_asks(&queue).is_empty());
    assert!(backend.texts().is_empty());
    queue.answer(ask.id, "cancel_affected").unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        queue.run_lease(run.id()).unwrap().is_none()
    });
    stop.store(true, Ordering::SeqCst);
    joined(supervisor, "the supervisor to stop").unwrap();
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    backend.join();
}

/// Two reviews that stop at a login that ran out are no `review_failed`:
/// both join one authentication ask, listed as jobs, each records
/// `auth_required` with `job: review` on its run, and the runs wait with
/// their sessions open. `done` starts the reviews again and the runs land.
#[test]
fn reviews_stopped_at_a_login_join_one_ask_and_review_again_after_done() {
    let (dir, repo, db) = fixture();
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "second task", &[]);
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        r#"printf 'change by %s\n' "$RUN_ID" > "change-$RUN_ID.txt" && git add . && git commit -q -m work
receipt "$(git rev-parse HEAD)"; idle; await_exit"#,
    ));
    let gate = dir.db.parent().unwrap().join("logged-out");
    let logged_out = format!(
        "while [ ! -f '{}' ]; do sleep 0.05; done; printf 'Invalid API key · Please run /login\\n'; exit 1",
        gate.display()
    );
    let reviewer = Arc::new(TestReviewer::new(&[
        logged_out.clone(),
        logged_out,
        verdict("pass", &[], "meets the acceptance"),
    ]));
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &supervise_options(2, true),
            )
        })
    };
    wait_until(&db, Duration::from_secs(60), |_| {
        queue_events(&db, "review_started").len() == 2
    });
    fs::write(&gate, "").unwrap();
    wait_until(&db, Duration::from_secs(60), |_| {
        open_holds(&db)
            .first()
            .is_some_and(|ask| ask.affected.len() == 2)
    });
    let holds = open_holds(&db);
    assert_eq!(holds.len(), 1, "{holds:?}");
    let ask = &holds[0];
    assert_eq!(ask.reason_category, AskReason::Authentication);
    let detail = |task: i64| {
        SqliteQueue::open(&db)
            .unwrap()
            .show(TaskId::new(task))
            .unwrap()
    };
    let (one, two) = (detail(1), detail(2));
    let mut expected: Vec<String> = [&one, &two]
        .iter()
        .map(|d| HoldJob::Review(d.runs[0].id().clone()).entry())
        .collect();
    let mut affected = ask.affected.clone();
    affected.sort();
    expected.sort();
    assert_eq!(affected, expected);
    assert!(
        ask.question.contains("\n\nAffected: review job of run "),
        "{}",
        ask.question
    );
    for detail in [&one, &two] {
        let required = payloads(detail, "auth_required");
        assert_eq!(required.len(), 1, "{required:?}");
        assert_eq!(required[0]["job"], "review");
        assert_eq!(required[0]["ask_id"], json!(ask.id));
        assert!(payloads(detail, "review_failed").is_empty());
        assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    }
    // The job that joined the open ask updated it on its run.
    let updated = queue_events(&db, "ask_updated");
    assert_eq!(updated.len(), 1, "{updated:?}");
    assert!(updated[0].run_id.is_some());
    // The ask is the attention, not a review by hand.
    let status = runtime::status(&db).unwrap();
    let nexts: Vec<&Value> = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| &a["next"])
        .collect();
    assert!(!nexts.contains(&&json!("review by hand")), "{status}");

    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "done")
        .unwrap();
    let outcome = joined(supervisor, "the supervisor to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    for task in [1, 2] {
        let detail = detail(task);
        assert_eq!(detail.task.status(), TaskStatus::Completed, "{task}");
        assert_eq!(payloads(&detail, "review_started").len(), 2);
    }
    assert_eq!(reviewer.prompts().len(), 4);
    let applied = queue_events(&db, "queue_hold_applied");
    assert_eq!(applied.len(), 1, "{applied:?}");
    let mut jobs: Vec<String> = serde_json::from_value(applied[0].payload["jobs"].clone()).unwrap();
    jobs.sort();
    assert_eq!(jobs, expected);
    assert_eq!(applied[0].payload["continued"], json!([]));
}

/// Runs and jobs of any kind join the one open ask of their wall: a job
/// with no run (the observer, a plan review) updates it on the queue.
#[test]
fn runs_and_jobs_join_the_one_ask_of_their_wall() {
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim(&sha("0123456789abcdef0123456789abcdef01234567"))
        .unwrap()
    else {
        panic!("claimed nothing")
    };
    let first = queue
        .hold(NewHold::wall(
            Wall::UsageLimit,
            Some(run.id().clone()),
            None,
        ))
        .unwrap();
    assert!(first.created && first.joined);
    let observer = queue
        .hold(NewHold::wall(
            Wall::UsageLimit,
            None,
            Some(HoldJob::Observer),
        ))
        .unwrap();
    assert!(!observer.created && observer.joined);
    assert_eq!(observer.ask.id, first.ask.id);
    let recovery = queue
        .hold(NewHold::wall(
            Wall::UsageLimit,
            Some(run.id().clone()),
            Some(HoldJob::Recovery(run.id().clone())),
        ))
        .unwrap();
    assert!(recovery.joined);
    let again = queue
        .hold(NewHold::wall(
            Wall::UsageLimit,
            None,
            Some(HoldJob::Observer),
        ))
        .unwrap();
    assert!(!again.joined);
    let ask = again.ask;
    assert_eq!(
        ask.affected,
        [
            run.id().to_string(),
            "observer job".to_owned(),
            format!("recovery job of run {}", run.id()),
        ]
    );
    assert!(
        ask.question.ends_with(&format!(
            "\n\nAffected: run {0}, observer job, recovery job of run {0}",
            run.id()
        )),
        "{}",
        ask.question
    );
    // The run's session is held; its recovery job is listed apart.
    assert_eq!(queue.hold_of(run.id()).unwrap().unwrap().id, ask.id);
    let updated = queue_events(&db, "ask_updated");
    assert_eq!(updated.len(), 2, "{updated:?}");
    assert_eq!(
        (updated[0].task_id, updated[0].run_id.as_ref()),
        (None, None)
    );
    assert_eq!(updated[0].payload["joined"], "observer job");
    assert_eq!(updated[1].run_id.as_ref(), Some(run.id()));
    // Another wall is another ask.
    let login = queue
        .hold(NewHold::wall(
            Wall::Authentication,
            None,
            Some(HoldJob::Observer),
        ))
        .unwrap();
    assert!(login.created && login.ask.id != ask.id);
    assert_eq!(login.ask.affected, ["observer job"]);
}
