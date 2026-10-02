//! Runtime tests: a review's `concern` decided on the job's recommendation
//! (ADR-t451-1 decision 3): a `high` one with no reason a person is needed
//! lands or goes back to the session, anything else asks a person with the
//! recommendation, and `concern_decided` records which.
use crate::runtime_e2e::{e2e_options, stub_e2e, with_e2e_paths};
use crate::runtime_review::revising_agent;
use crate::{common, runtime_support};

use dagq::domain::{AskConfidence, AskReason, EventKind, TaskDetail};
use runtime_support::*;

/// A reviewer script that prints a `concern` with its recommendation,
/// confidence and reason (`None` prints null).
fn concern(recommendation: &str, confidence: &str, reason: Option<&str>) -> String {
    let json = json!({
        "verdict": "concern",
        "reasons": ["a finding"],
        "summary": "judged",
        "recommendation": recommendation,
        "confidence": confidence,
        "reason_category": reason,
    });
    format!("printf '%s\\n' '{json}'")
}

fn concern_decided(detail: &TaskDetail) -> Vec<Value> {
    payloads(detail, "concern_decided")
        .into_iter()
        .cloned()
        .collect()
}

/// The one ask of the queue, which must be the run's `approve_landing`.
fn landing_ask(queue: &SqliteQueue, run: &TaskRun) -> dagq::domain::Ask {
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].clone();
    assert_eq!(ask.kind, AskKind::ApproveLanding);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    ask
}

/// A `land` of high confidence and no reason lands like a pass: the
/// session exits right before the landing, no ask opens, and stats counts
/// it among the judgements made without an `approve_landing`. Here the
/// first review prints no readable verdict (an unescaped quote), so the run
/// is reviewed once more with the same input (task 328) and the verdict of
/// that retry is acted on as any other.
#[test]
fn a_high_land_lands_the_run_without_an_ask() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[
        r#"printf '%s\n' '{"verdict":"pass","reasons":[],"summary":"says "fine""}'"#.to_owned(),
        concern("land", "high", None),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed_run(&detail.runs[0], &repo, &base);
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    assert!(backend.notifications.lock().unwrap().is_empty());
    // Reviewed once more with the same input after the unreadable verdict.
    let prompts = reviewer.prompts();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0], prompts[1]);
    let retried = payloads(&detail, "review_retried");
    assert_eq!(retried.len(), 1);
    assert_eq!(retried[0]["attempt"], 1);
    assert!(
        retried[0]["error"]
            .as_str()
            .unwrap()
            .contains("the review printed no verdict JSON"),
        "{retried:?}"
    );
    assert!(payloads(&detail, "review_failed").is_empty());
    assert_eq!(
        concern_decided(&detail),
        [
            json!({"attempt": 2, "recommendation": "land", "confidence": "high",
                "reason_category": null, "applied": true, "escalated_because": null})
        ]
    );
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["attempt"], 2);
    assert_eq!(finished[0]["recommendation"], "land");
    assert_eq!(finished[0]["confidence"], "high");
    let kinds = event_kinds(&detail);
    for (earlier, later) in [
        ("review_retried", "review_finished"),
        ("review_finished", "concern_decided"),
        ("concern_decided", "exit_requested"),
        ("exit_requested", "workspace_closed"),
        ("workspace_closed", "integration_started"),
    ] {
        assert!(
            position(&kinds, earlier) < position(&kinds, later),
            "{earlier} before {later}: {kinds:?}"
        );
    }
    assert!(!kinds.contains(&"integration_approved"), "{kinds:?}");
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(
        stats["recommendations"]["decided_without_ask"]["approve_landing"], 1,
        "{}",
        stats["recommendations"]
    );
}

/// A run that needs the e2e and whose concern the job lands runs the e2e
/// on the host before it lands, as a passed run does (ADR-t1233-2).
#[test]
fn a_high_land_of_a_run_that_needs_the_e2e_runs_it_before_landing() {
    let (dir, repo, db) = fixture();
    with_e2e_paths(&repo, "[\"change.txt\"]");
    let ran = dir.path().join("ran");
    let backend = TestWorkspace::new(
        &db,
        false,
        "printf 'fixed\\n' > fixed.txt; git add fixed.txt; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let reviewer = TestReviewer::new(&[concern("land", "high", None)]);
    let outcome = supervise_reviewed_with(
        &db,
        &repo,
        &backend,
        &reviewer,
        &e2e_options(stub_e2e(&ran)),
    );
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(
        payloads(&detail, "validation_finished")[0]["e2e_requirement"]["required"],
        true
    );
    assert_eq!(concern_decided(&detail)[0]["applied"], true);
    let kinds = event_kinds(&detail);
    for (earlier, later) in [
        ("concern_decided", "run_e2e_started"),
        ("run_e2e_started", "run_e2e_finished"),
        ("run_e2e_finished", "integration_started"),
    ] {
        assert!(
            position(&kinds, earlier) < position(&kinds, later),
            "{earlier} before {later}: {kinds:?}"
        );
    }
    assert_eq!(
        payloads(&detail, "run_e2e_finished")[0]["outcome"],
        "passed"
    );
}

/// A `send_back` of high confidence goes to the live session as a revise,
/// counted like one; the next review passes and the run lands.
#[test]
fn a_high_send_back_revises_the_live_session() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &revising_agent(1));
    let reviewer = TestReviewer::new(&[
        concern("send_back", "high", None),
        verdict("pass", &[], "fixed"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed_run(&detail.runs[0], &repo, &base);
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    let requested = payloads(&detail, "revise_requested");
    assert_eq!(requested.len(), 1);
    assert_eq!(requested[0]["attempt"], 1);
    assert_eq!(requested[0]["reasons"], json!(["a finding"]));
    assert_eq!(
        concern_decided(&detail),
        [
            json!({"attempt": 1, "recommendation": "send_back", "confidence": "high",
                "reason_category": null, "applied": true, "escalated_because": null})
        ]
    );
    let texts = backend.texts();
    assert_eq!(texts.len(), 1);
    assert!(texts[0].1.contains("(revise 1 of 2)"), "{}", texts[0].1);
    assert_eq!(reviewer.prompts().len(), 2);
}

/// A concern the runtime does not apply asks a person with the job's
/// recommendation, confidence and reason; the session exits and its
/// workspace closes before the ask. Here a `discard` of high confidence,
/// asked for that reason. Which concerns are asked and why
/// (`concern::tests::only_a_high_confidence_without_a_reason_is_applied`),
/// the ask's text and reason for each
/// (`landing::tests::a_concern_asks_with_its_recommendation_and_why`,
/// `landing::tests::a_send_back_past_the_revise_limit_asks_with_the_count_of_revises`,
/// `landing::tests::a_concern_without_a_recommendation_asks_as_before`) and
/// `concern_decided` (`concern::tests::the_payload_says_whether_it_was_applied`)
/// are unit tests.
#[test]
fn a_discard_concern_asks_a_person_with_the_recommendation() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[concern("send_back", "high", Some("discard"))]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(
        concern_decided(&detail),
        [
            json!({"attempt": 1, "recommendation": "send_back", "confidence": "high",
                "reason_category": "discard", "applied": false, "escalated_because": "discard"})
        ]
    );
    let ask = landing_ask(&queue, &run);
    assert_eq!(ask.recommendation.as_deref(), Some("send_back"));
    assert_eq!(ask.confidence, Some(AskConfidence::High));
    // A person is needed for the reason the job gave.
    assert_eq!(ask.reason_category, AskReason::Discard);
    for part in [
        "returned concern (the review recommends send_back, but the judgement is whether to throw the work away (discard)): judged",
        "\n- a finding",
        "The review recommends send_back (high confidence).",
    ] {
        assert!(ask.question.contains(part), "{part} in {}", ask.question);
    }
    assert!(payloads(&detail, "revise_requested").is_empty());
    assert!(backend.texts().is_empty());
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "workspace_closed") < position(&kinds, "ask_opened"));
}

/// A run of `backend` past its validation whose supervisor recorded
/// `events` after it and died, with its lease stale. Returns the run.
fn left_by_a_dead_supervisor(
    repo: &Path,
    db: &Path,
    backend: &TestWorkspace,
    events: Vec<(EventKind, Value)>,
) -> TaskRun {
    let run = start_run_under_dead_supervisor(repo, db, backend, "dead-supervisor");
    let idle = run.idle_marker_path().unwrap();
    wait_until(db, Duration::from_secs(20), |_| idle.is_file());
    let head = git_out(
        Path::new(run.worktree_path().unwrap()),
        &["rev-parse", "HEAD"],
    );
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration', result_commit=?2 WHERE id=?1",
            rusqlite::params![run.id(), head],
        )
        .unwrap();
    let queue = SqliteQueue::open(db).unwrap();
    let validated = (
        EventKind::ValidationFinished,
        json!({"status": "awaiting_integration"}),
    );
    for (kind, payload) in std::iter::once(validated).chain(events) {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
    age_lease(db, &run, 31);
    run
}

/// Supervise as the adopter, with a reviewer that gives `verdicts`.
fn adopt(db: &Path, repo: &Path, backend: &Arc<TestWorkspace>, verdicts: Vec<String>) -> Value {
    let (db, repo, backend) = (db.to_owned(), repo.to_owned(), backend.clone());
    joined(
        thread::spawn(move || {
            let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
            let outcome = runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &TestReviewer::new(&verdicts),
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &supervise_options(4, true),
            )
            .unwrap();
            backend.join();
            outcome
        }),
        "the supervisor thread to return",
    )
}

/// A run whose supervisor recorded a `concern` verdict and died before it
/// acted on it: its adopter decides the concern from the verdict as the
/// supervisor would have, and records `concern_decided` once. Here a `land`
/// of high confidence, which lands; one the adopter asks a person about is
/// [`an_adopter_records_a_send_back_left_unrecorded_by_a_dead_supervisor`],
/// and which concerns are asked is
/// `concern::tests::only_a_high_confidence_without_a_reason_is_applied`.
#[test]
fn an_adopter_decides_a_concern_left_by_a_dead_supervisor() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    left_by_a_dead_supervisor(
        &repo,
        &db,
        &backend,
        vec![
            (EventKind::ReviewStarted, json!({"attempt": 1})),
            (
                EventKind::ReviewFinished,
                json!({"verdict": "concern", "reasons": ["a finding"], "summary": "judged",
                       "attempt": 1, "recommendation": "land", "confidence": "high",
                       "reason_category": null}),
            ),
        ],
    );
    let outcome = adopt(&db, &repo, &backend, Vec::new());
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(
        concern_decided(&detail),
        [
            json!({"attempt": 1, "recommendation": "land", "confidence": "high",
                "reason_category": null, "applied": true, "escalated_because": null})
        ]
    );
    // Not reviewed again.
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert_landed_run(&detail.runs[0], &repo, &base);
    assert!(queue.asks(Default::default()).unwrap().is_empty());
}

/// A supervisor that died after it sent a concern back on the job's
/// recommendation and before it recorded `concern_decided`: here the
/// request was recorded (`revise_requested`) but could not be typed
/// (`revise_unsent`). Its adopter records the decision once, as `unsent`,
/// and asks a person as the supervisor would have.
#[test]
fn an_adopter_records_a_send_back_left_unrecorded_by_a_dead_supervisor() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let run = left_by_a_dead_supervisor(
        &repo,
        &db,
        &backend,
        vec![
            (EventKind::ReviewStarted, json!({"attempt": 1})),
            (
                EventKind::ReviewFinished,
                json!({"verdict": "concern", "reasons": ["a finding"], "summary": "judged",
                       "attempt": 1, "recommendation": "send_back", "confidence": "high",
                       "reason_category": null}),
            ),
            (
                EventKind::ReviseRequested,
                json!({"attempt": 1, "reasons": ["a finding"]}),
            ),
            (
                EventKind::ReviseUnsent,
                json!({"attempt": 1, "error": "the revise request could not be sent"}),
            ),
        ],
    );
    let outcome = adopt(&db, &repo, &backend, Vec::new());
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(
        concern_decided(&detail),
        [
            json!({"attempt": 1, "recommendation": "send_back", "confidence": "high",
                "reason_category": null, "applied": false, "escalated_because": "unsent"})
        ]
    );
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "run_adopted") < position(&kinds, "concern_decided"));
    let decided = detail
        .events
        .iter()
        .find(|e| e.kind == "concern_decided")
        .unwrap();
    // Recorded at the review job's request, as the live supervisor does.
    assert_eq!(
        decided.actor.as_ref().and_then(|a| a.requested_by.clone()),
        Some(format!("review-job:{}:1", run.id())),
        "{decided:?}"
    );
    let ask = landing_ask(&queue, &run);
    assert_eq!(ask.recommendation.as_deref(), Some("send_back"));
}
