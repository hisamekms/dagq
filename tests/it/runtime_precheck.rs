//! Runtime tests: the limit of the conflict precheck's requests to the live
//! session of a passed run (ADR-0027 decision 4), which are conflict-only
//! attempts after a passed review (ADR-0047 decision 24, task 511).
use crate::runtime_review::{moving_main_then_pass, rebasing_agent};
use crate::runtime_support;

use dagq::domain::{AskKind, resume::CONFLICT_ONLY_RESUME_LIMIT};
use runtime_support::*;

/// The precheck's requests are not counted toward `MAX_RESUME_ATTEMPTS`:
/// when main keeps moving into the run, the live session is asked to
/// rebase up to the conflict-only limit. Past it nobody is asked: the run
/// goes on to land, its conflicting landing parks it with its resumes used
/// up, and the task is retried with the run's branch carried over
/// (`retry_inherit`), whose run lands.
#[test]
fn precheck_conflicts_up_to_the_conflict_only_limit_retry_the_task_with_its_branch() {
    let (_dir, repo, db) = fixture();
    // The first run's session rebases at each request; the retry's run
    // commits its work and waits for its /exit.
    let second = "\"$(git rev-parse --path-format=absolute --git-common-dir)/second-run\"";
    let agent = format!(
        "if [ -f {second} ]; then {IDLE_AGENT}; else touch {second}; {}; fi",
        rebasing_agent(CONFLICT_ONLY_RESUME_LIMIT)
    );
    let backend = TestWorkspace::new(&db, false, &agent);
    let mut reviews = vec![moving_main_then_pass(); CONFLICT_ONLY_RESUME_LIMIT + 1];
    reviews.push(verdict("pass", &[], "meets the acceptance"));
    let reviewer = TestReviewer::new(&reviews);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2, "{:?}", event_kinds(&detail));
    let first = detail.runs[0].clone();
    assert_eq!(first.status(), RunStatus::Failed);
    assert_eq!(detail.runs[1].status(), RunStatus::Integrated);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let of_first = |kind: &str| -> Vec<&Value> {
        detail
            .events
            .iter()
            .filter(|e| e.kind == kind && e.run_id.as_ref() == Some(first.id()))
            .map(|e| &e.payload)
            .collect()
    };
    let prechecks = of_first("conflict_precheck");
    let requested: Vec<&Value> = prechecks.iter().map(|p| &p["requested"]).collect();
    let mut expected = vec![&json!(true); CONFLICT_ONLY_RESUME_LIMIT];
    expected.push(&json!(false));
    assert_eq!(requested, expected);
    let last = prechecks[CONFLICT_ONLY_RESUME_LIMIT];
    assert_eq!(last["attempt"], CONFLICT_ONLY_RESUME_LIMIT + 1);
    assert_eq!(last["exhausted"], true);
    assert!(last.get("asked").is_none(), "{last}");
    assert_eq!(backend.texts().len(), CONFLICT_ONLY_RESUME_LIMIT);
    // Its landing conflicted and parked it; no resume started.
    let deferred = of_first("integration_deferred");
    assert_eq!(deferred.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(deferred[0]["code"], "rebase_conflict");
    assert!(of_first("resume_started").is_empty());
    // Retried with its branch carried over, with nobody asked.
    let finished = of_first("triage_finished");
    assert_eq!(finished.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(finished[0]["by"], "runtime");
    assert_eq!(finished[0]["action"], "retry_inherit");
    assert_eq!(finished[0]["counted_resumes"], 0);
    assert_eq!(finished[0]["conflict_only_resumes"], 0);
    assert_eq!(finished[0]["conflict_requests"], CONFLICT_ONLY_RESUME_LIMIT);
    let repaired = of_first("auto_repaired");
    assert!(
        repaired.iter().any(|p| p["repair"] == "inherit_retry"
            && p["conditions"]["conflict_requests"] == CONFLICT_ONLY_RESUME_LIMIT),
        "{repaired:?}"
    );
    let last_error = first.last_error().unwrap();
    assert!(
        last_error.starts_with(&format!(
            "resumed 0 times (0 of at most 3 counted) and asked {CONFLICT_ONLY_RESUME_LIMIT} times by the conflict precheck ({CONFLICT_ONLY_RESUME_LIMIT} of at most {CONFLICT_ONLY_RESUME_LIMIT} attempts for conflicts only after its review passed)"
        )),
        "{last_error}"
    );
    let asks = queue
        .asks(AskQuery {
            all: true,
            ..Default::default()
        })
        .unwrap();
    assert!(asks.is_empty(), "{asks:?}");
    assert!(
        detail
            .events
            .iter()
            .any(|e| e.kind == "run_inherited" && e.run_id.as_ref() == Some(detail.runs[1].id())),
        "{:?}",
        event_kinds(&detail)
    );
}

/// A run whose counted resumes (a failed verification, missing evidence)
/// are used up is not retried by the precheck: its conflict with main asks
/// a person, as before, without a request to its session.
#[test]
fn a_precheck_conflict_after_the_counted_resumes_asks_a_person() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let idle = run.idle_marker_path().unwrap();
    wait_until(&db, Duration::from_secs(20), |_| idle.is_file());
    let head = git_out(
        Path::new(run.worktree_path().unwrap()),
        &["rev-parse", "HEAD"],
    );
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration', result_commit=?2 WHERE id=?1",
            rusqlite::params![run.id(), head],
        )
        .unwrap();
    // Main moves into the run's change.
    fs::write(repo.join("change.txt"), "main moved\n").unwrap();
    git(&repo, &["add", "change.txt"]);
    git(&repo, &["commit", "-q", "-m", "main moves"]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    for attempt in 1..=3 {
        queue
            .record_runtime_event(
                run.id(),
                "resume_started",
                json!({"attempt": attempt, "counted": true}),
            )
            .unwrap();
    }
    // The last resume went on in the session that lives (the one the stub
    // runs), whose receipt was validated.
    queue
        .record_runtime_event(
            run.id(),
            "resume_finished",
            json!({"attempt": 3, "status": "validating", "workspace_id": WORKSPACE_ID}),
        )
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            "review_finished",
            json!({"attempt": 1, "verdict": "pass", "reasons": [], "summary": "meets the acceptance"}),
        )
        .unwrap();
    age_lease(&db, &run, 31);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 1);
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    let prechecks = payloads(&detail, "conflict_precheck");
    assert_eq!(prechecks.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(prechecks[0]["requested"], false);
    assert!(prechecks[0].get("exhausted").is_none());
    let asked = prechecks[0]["asked"].as_str().unwrap();
    assert!(
        asked.ends_with("after 0 conflict requests and 3 counted resumes (at most 3)"),
        "{asked}"
    );
    assert!(backend.texts().is_empty());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert!(payloads(&detail, "triage_finished").is_empty());
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].kind, AskKind::ApproveLanding);
    assert_eq!(asks[0].run_id.as_ref(), Some(run.id()));
    assert!(asks[0].question.contains(asked), "{}", asks[0].question);
}
