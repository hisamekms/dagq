//! Runtime tests: the wait and the answer of the authentication and
//! usage-limit `queue_hold` asks (ADR-0047 decision 42, task 437): no new
//! claim and no headless job while one is open, and the runtime applies
//! `done` and `cancel_affected`.
use crate::runtime_support;

use dagq::domain::{AskReason, HOLD_OPTIONS, NewHold, queue_hold::USAGE_LIMIT_SUBJECT};
use runtime_support::*;
use std::sync::atomic::AtomicBool;

fn queue_events(db: &Path, kind: &str) -> Vec<Value> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == kind)
        .map(|event| event.payload)
        .collect()
}

/// Open the queue's hold ask of `reason` (and `subject`) with no run in it,
/// as the runtime does when a login or the usage limit stops the work.
fn open_hold(db: &Path, reason: AskReason, subject: Option<&str>) -> dagq::domain::Ask {
    SqliteQueue::open(db)
        .unwrap()
        .hold(NewHold {
            reason_category: reason,
            subject: subject.map(str::to_owned),
            run_id: None,
            job: None,
            question: "the login ran out".into(),
            options: HOLD_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
            asked_by: "supervisor".into(),
        })
        .unwrap()
        .ask
}

/// The kinds of the events of the task's runs.
fn kinds_of(db: &Path, task: i64) -> Vec<String> {
    SqliteQueue::open(db)
        .unwrap()
        .show(TaskId::new(task))
        .unwrap()
        .events
        .into_iter()
        .map(|event| event.kind)
        .collect()
}

/// While the usage-limit ask is open, the ready task is not claimed
/// (`claim_held`, reason `usage_limit`, with the ask) and the run whose
/// receipt came in waits for its review with its session open (no
/// `review_started`). `done` is the runtime's to apply: it closes the ask,
/// records `queue_hold_applied` and `claim_resumed`, and the review and the
/// claim follow.
#[test]
fn an_open_usage_limit_ask_holds_claims_and_reviews_until_done() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, GATED_AGENT));
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        ..supervise_options(2, false)
    };
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    // The run is claimed before its run dir is recorded; the gate below
    // needs the dir.
    wait_until(&db, Duration::from_secs(30), |queue| {
        queue
            .show(TaskId::new(1))
            .unwrap()
            .runs
            .first()
            .is_some_and(|run| run.run_dir().is_some())
    });
    let first = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();
    let ask = open_hold(&db, AskReason::Cost, Some(USAGE_LIMIT_SUBJECT));
    wait_until(&db, Duration::from_secs(30), |_| {
        !queue_events(&db, "claim_held").is_empty()
    });
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "second task", &[]);
    // The session finishes while the ask is open: validated, not reviewed.
    fs::write(
        exit_request_path(first.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        queue.run(first.id()).unwrap().status() == RunStatus::AwaitingIntegration
    });
    await_passes(&passes, SOME_PASSES);
    assert!(
        !kinds_of(&db, 1).iter().any(|kind| kind == "review_started"),
        "{:?}",
        kinds_of(&db, 1)
    );
    let held = queue_events(&db, "claim_held");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["reason"], "usage_limit");
    assert_eq!(held[0]["ask_id"], json!(ask.id));
    assert!(
        held[0]["message"]
            .as_str()
            .unwrap()
            .contains("no headless job"),
        "{held:?}"
    );
    assert!(
        SqliteQueue::open(&db)
            .unwrap()
            .show(TaskId::new(2))
            .unwrap()
            .runs
            .is_empty()
    );
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        status["supervisors"][0]["claim_hold"]["reason"], "usage_limit",
        "{status}"
    );

    // The limit is back: the runtime applies the answer.
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "done")
        .unwrap();
    let status = runtime::status(&db).unwrap();
    let applying = status["attention"].as_array().unwrap().iter().any(|a| {
        a["ask_id"] == json!(ask.id) && a["next"] == "applying the answer of ask 1 (runtime)"
    });
    assert!(
        applying
            || SqliteQueue::open(&db)
                .unwrap()
                .read_ask(ask.id)
                .unwrap()
                .closed_at
                .is_some(),
        "{status}"
    );
    wait_until(&db, Duration::from_secs(30), |queue| {
        queue
            .show(TaskId::new(2))
            .unwrap()
            .runs
            .first()
            .is_some_and(|run| run.run_dir().is_some())
    });
    wait_until(&db, Duration::from_secs(30), |_| {
        kinds_of(&db, 1).iter().any(|kind| kind == "review_started")
    });
    let second = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap()
        .runs[0]
        .clone();
    fs::write(
        exit_request_path(second.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let answered = queue_events(&db, "ask_answered");
    assert_eq!(answered[0]["runtime_delivers"], true, "{answered:?}");
    let applied = queue_events(&db, "queue_hold_applied");
    assert_eq!(applied.len(), 1, "{applied:?}");
    assert_eq!(applied[0]["answer"], "done");
    assert_eq!(applied[0]["reason_category"], "cost");
    assert_eq!(applied[0]["subject"], "usage_limit");
    assert!(
        SqliteQueue::open(&db)
            .unwrap()
            .read_ask(ask.id)
            .unwrap()
            .closed_at
            .is_some()
    );
    let resumed = queue_events(&db, "claim_resumed");
    assert_eq!(resumed[0]["reason"], "usage_limit", "{resumed:?}");
    wait_until(&db, Duration::from_secs(60), |queue| {
        [first.id(), second.id()]
            .iter()
            .all(|run| queue.run_lease(run).unwrap().is_none())
    });
    stop.store(true, Ordering::SeqCst);
    joined(supervisor, "the supervisor to drain").unwrap();
    backend.join();
}

/// A recovery job of an ended run that failed around the hold starts
/// again once a person answered `done`: `job_restarted` makes the run's
/// triage due, and `queue_hold_applied` lists it.
#[test]
fn done_starts_again_the_recovery_job_that_failed_during_the_hold() {
    let (_dir, repo, db) = fixture();
    // No recovery job can start: the triage of the failed run fails.
    let backend = TestWorkspace::new(&db, false, "commit work; exit 7");
    supervise_reviewed(
        &db,
        &repo,
        &backend,
        &TestReviewer::new(&[verdict("pass", &[], "ok")]),
    );
    let run = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();
    assert!(
        kinds_of(&db, 1).iter().any(|kind| kind == "triage_failed"),
        "{:?}",
        kinds_of(&db, 1)
    );
    let ask = open_hold(&db, AskReason::Authentication, None);
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "done")
        .unwrap();
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "ok")]).with_triages(&[recovery(
        json!({"verdict": "escalate", "confidence": "high", "diagnosis": "a person decides"}),
    )]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(reviewer.triage_prompts().len(), 1);
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let restarted = payloads(&detail, "job_restarted");
    assert_eq!(restarted.len(), 1, "{restarted:?}");
    assert_eq!(restarted[0]["job"], "triage");
    assert_eq!(restarted[0]["ask_id"], json!(ask.id));
    let applied = queue_events(&db, "queue_hold_applied");
    assert_eq!(
        applied[0]["restarted"],
        json!([{"job": "triage", "run_id": run.id()}]),
        "{applied:?}"
    );
    let kinds = event_kinds(&detail);
    assert!(
        position(&kinds, "job_restarted")
            < kinds.iter().rposition(|k| *k == "triage_started").unwrap(),
        "{kinds:?}"
    );
}
