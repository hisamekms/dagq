//! Runtime tests: the `/exit` after a run's review, one that never got
//! there, and a session that holds it back (`stuck_exit`).
use crate::common;
use crate::runtime_support;
use dagq::domain::EventKind;

use runtime_support::*;

/// The `send_exit` attempts that failed, as (attempt, max_attempts,
/// retry_after_ms).
fn exit_attempts(detail: &dagq::domain::TaskDetail) -> Vec<(Value, Value, Value)> {
    backend_failures(detail)
        .into_iter()
        .filter(|e| e.payload["op"] == "send_exit")
        .map(|e| {
            (
                e.payload["attempt"].clone(),
                e.payload["max_attempts"].clone(),
                e.payload["retry_after_ms"].clone(),
            )
        })
        .collect()
}

/// Waits for the state a `/exit` that never got there and cannot land
/// reaches and keeps until the person answers: `exit_unsent` recorded and
/// the `stuck_exit` ask open (recorded after it). The supervisor holds the
/// run there, so nothing is caught in passing and the wait is bounded by
/// [`common::STEP_LIMIT`] rather than by how fast a loaded host gets there
/// (task 520).
fn wait_for_stuck_exit(db: &Path) {
    wait_until(db, common::STEP_LIMIT, |queue| {
        let detail = queue.show(TaskId::new(1)).unwrap();
        !payloads(&detail, "exit_unsent").is_empty()
            && queue
                .asks(AskQuery::default())
                .unwrap()
                .iter()
                .any(|ask| ask.kind == AskKind::StuckExit)
    });
}

/// A `/exit` that cmux timed out before it reached the session (the screen
/// shows the input box and no trace of it) is sent again after a backoff,
/// and the run goes on without being given up (task 354).
#[test]
fn an_exit_that_timed_out_before_reaching_the_session_is_sent_again() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_unsent.store(1, Ordering::SeqCst);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 2);
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(exit_attempts(&detail), [(json!(1), json!(3), json!(10))]);
    let kinds = event_kinds(&detail);
    // The session exited on the second /exit: nothing more to record.
    assert!(position(&kinds, "exit_requested") < position(&kinds, "session_exited"));
    for kind in ["exit_unsent", "exit_request_timed_out", "runtime_error"] {
        assert!(!kinds.contains(&kind), "{kinds:?}");
    }
}

/// A `/exit` that never got there on any attempt leaves a run whose review
/// passed and whose receipt still holds against a clean worktree to land:
/// the workspace is closed instead of waiting on a session that was never
/// asked, and `exit_unsent` records it (task 354).
#[test]
fn an_exit_that_never_got_there_closes_a_sound_passed_run_and_lands_it() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_unsent.store(usize::MAX, Ordering::SeqCst);
    backend.close_ends_session = true;
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    // Typed once and twice again, never more.
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 3);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    assert_eq!(
        exit_attempts(&detail),
        [
            (json!(1), json!(3), json!(10)),
            (json!(2), json!(3), json!(20)),
            (json!(3), json!(3), Value::Null),
        ]
    );
    assert_eq!(
        payloads(&detail, "exit_unsent"),
        [
            &json!({"code": "backend_timeout", "workspace_id": WORKSPACE_ID, "attempts": 3, "action": "close_and_land"})
        ]
    );
    // Closing the workspace to land is recorded as a repair.
    assert_eq!(
        payloads(&detail, "auto_repaired"),
        [&json!({
            "layer": "runtime",
            "repair": "exit_forced_close",
            "conditions": {
                "cause": "backend_timeout",
                "attempts": 3,
                "exit_reached": false,
                "then": "land",
                "review": "pass",
                "receipt_holds": true,
            },
            "detail": {"workspace_id": WORKSPACE_ID},
        })]
    );
    let kinds = event_kinds(&detail);
    for (earlier, later) in [
        ("review_finished", "exit_requested"),
        ("exit_requested", "exit_unsent"),
        ("exit_unsent", "workspace_closed"),
        ("workspace_closed", "run_integrated"),
    ] {
        assert!(
            position(&kinds, earlier) < position(&kinds, later),
            "{earlier} before {later}: {kinds:?}"
        );
    }
    for kind in ["exit_request_timed_out", "runtime_error"] {
        assert!(!kinds.contains(&kind), "{kinds:?}");
    }
    assert!(queue.asks(AskQuery::default()).unwrap().is_empty());
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
}

/// A `/exit` that never got there leaves a run that does not land on its
/// own (here its review failed) to the person, as a session that held its
/// `/exit` back is: the `stuck_exit` ask, at once, and the run kept (task
/// 354).
#[test]
fn an_exit_that_never_got_there_asks_for_a_run_that_cannot_land() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_unsent.store(usize::MAX, Ordering::SeqCst);
    backend.registration_timeout = common::STEP_LIMIT;
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_for_stuck_exit(&db);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(queue.run_lease(run.id()).unwrap().is_some());
    // A run held back is not repaired: it goes to the ask.
    assert!(payloads(&detail, "auto_repaired").is_empty());
    assert_eq!(
        payloads(&detail, "exit_unsent"),
        [&json!({
            "code": "backend_timeout", "workspace_id": WORKSPACE_ID, "attempts": 3,
            "action": "recover", "held": "the run does not land after its exit",
        })]
    );
    assert_eq!(
        payloads(&detail, "exit_request_timed_out"),
        [
            &json!({"code": "exit_timeout", "workspace_id": WORKSPACE_ID, "timeout_secs": 120, "unsent": true})
        ]
    );
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::StuckExit);
    assert!(
        asks[0].question.contains("(exit_unsent)"),
        "{}",
        asks[0].question
    );
    // The person has the session exit; the run goes on as before.
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 3);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "exit_unsent") < position(&kinds, "session_exited"));
    assert!(position(&kinds, "session_exited") < position(&kinds, "review_failed"));
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    assert!(queue.read_ask(asks[0].id).unwrap().closed_at.is_some());
}

/// A sound passed run whose workspace cannot be closed either is not
/// landed with its session alive: the `stuck_exit` ask says why (task 354).
#[test]
fn an_exit_that_never_got_there_asks_when_the_workspace_cannot_be_closed() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_unsent.store(usize::MAX, Ordering::SeqCst);
    backend.close_times_out = true;
    backend.registration_timeout = common::STEP_LIMIT;
    let backend = Arc::new(backend);
    let reviewer = Arc::new(TestReviewer::new(&[verdict(
        "pass",
        &[],
        "meets the acceptance",
    )]));
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || {
            let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &supervise_options(4, true),
            )
        })
    };
    wait_for_stuck_exit(&db);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    let unsent = payloads(&detail, "exit_unsent");
    assert_eq!(unsent[0]["action"], "recover", "{unsent:?}");
    assert!(
        unsent[0]["held"]
            .as_str()
            .unwrap()
            .starts_with("its workspace could not be closed"),
        "{unsent:?}"
    );
    assert!(run.workspace_closed_at().is_none());
    // The person has the session exit; the run lands without a close.
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
}

/// A passed run whose worktree changed after its review does not land
/// without its session's exit: the `/exit` that never got there is the
/// `stuck_exit` ask, saying why (task 354).
#[test]
fn an_exit_that_never_got_there_asks_for_a_passed_run_whose_worktree_changed() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_unsent.store(usize::MAX, Ordering::SeqCst);
    backend.registration_timeout = common::STEP_LIMIT;
    let backend = Arc::new(backend);
    // The review runs in the worktree; this one leaves a file behind.
    let reviewer = Arc::new(TestReviewer::new(&[format!(
        "printf 'x\\n' > stray.txt; {}",
        verdict("pass", &[], "meets the acceptance")
    )]));
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || supervise_reviewed(&db, &repo, &backend, &reviewer))
    };
    wait_for_stuck_exit(&db);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    let unsent = payloads(&detail, "exit_unsent");
    assert_eq!(unsent.len(), 1, "{unsent:?}");
    assert_eq!(unsent[0]["action"], "recover");
    let held = unsent[0]["held"].as_str().unwrap();
    assert!(
        held.starts_with("its receipt no longer holds: worktree is not clean"),
        "{held}"
    );
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks[0].kind, AskKind::StuckExit);
    assert!(asks[0].question.contains(held), "{}", asks[0].question);
    // The person cleans up and has the session exit: the run lands.
    fs::remove_file(Path::new(run.worktree_path().unwrap()).join("stray.txt")).unwrap();
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 3);
}

/// A run whose session held the `/exit` after its passing review back past
/// the exit timeout, left by a supervisor that died before it did anything
/// about it: validated, reviewed, `/exit` sent and timed out.
fn stuck_exit_after_a_pass(repo: &Path, db: &Path, backend: &TestWorkspace) -> TaskRun {
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
    for (kind, payload) in [
        (
            EventKind::ValidationFinished,
            json!({"status": "awaiting_integration"}),
        ),
        (EventKind::ReviewStarted, json!({"attempt": 1})),
        (
            EventKind::ReviewFinished,
            json!({"verdict": "pass", "reasons": [], "summary": "ok", "attempt": 1}),
        ),
        (
            EventKind::ExitRequested,
            json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
        ),
        (
            EventKind::ExitRequestTimedOut,
            json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
        ),
    ] {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
    age_lease(db, &run, 31);
    run
}

/// Supervise with `recovery` as the only recovery job's script, on a
/// thread.
fn supervise_recovering(
    db: &Path,
    repo: &Path,
    backend: &Arc<TestWorkspace>,
    recovery: String,
) -> (Arc<TestReviewer>, thread::JoinHandle<Result<Value>>) {
    let reviewer = Arc::new(
        TestReviewer::new(&[verdict("concern", &["x"], "never")]).with_triages(&[recovery]),
    );
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
                &supervise_options(4, true),
            )
        })
    };
    (reviewer, supervisor)
}

/// ADR-0047 decisions 39 and 40: a session that holds the `/exit` after a
/// passing review back is the `stuck_exit` alert's recovery job's. Its
/// `close_and_proceed` holds (the review passed, the worktree is clean and
/// the receipt names its HEAD, the reviewed commit), so the runtime closes
/// the workspace and lands the run, with no ask and no second `/exit`.
#[test]
fn a_stuck_exit_after_a_pass_is_closed_and_landed_by_its_recovery_job() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.close_ends_session = true;
    let backend = Arc::new(backend);
    let run = stuck_exit_after_a_pass(&repo, &db, &backend);
    let (reviewer, supervisor) = supervise_recovering(
        &db,
        &repo,
        &backend,
        repair(
            json!({"action": "close_and_proceed"}),
            "a dialog holds the exit of a finished session",
        ),
    );
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    assert!(
        queue
            .asks(AskQuery {
                all: true,
                ..Default::default()
            })
            .unwrap()
            .is_empty()
    );
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    assert!(backend.closed().contains(&WORKSPACE_ID.to_owned()));
    let events = queue.run_events(run.id()).unwrap();
    let timed_out = events
        .iter()
        .find(|e| e.kind == "exit_request_timed_out")
        .unwrap();
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["alert"], "stuck_exit");
    assert_eq!(requested[0]["evidence"], json!([timed_out.id]));
    assert_eq!(requested[0]["then"], "land");
    let repaired = payloads(&detail, "auto_repaired");
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert_eq!(repaired[0]["layer"], "recovery");
    assert_eq!(repaired[0]["repair"], "close_and_proceed");
    assert_eq!(repaired[0]["alert"], "stuck_exit");
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished[0]["applied"], json!(["close_and_proceed"]));
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "recovery_finished") < position(&kinds, "run_integrated"));
    let (prompt, _) = &reviewer.triage_prompts()[0];
    for part in [
        "raised the alert stuck_exit",
        "close_and_proceed",
        "is still running",
    ] {
        assert!(prompt.contains(part), "{part}: {prompt}");
    }
}

/// A live session's recovery job that fails (here it exits non-zero) opens
/// the `stuck_exit` ask (ADR-t609-1): its question says the job failed and
/// why, its reason is `recovery_failed`, and no `recover by hand`
/// attention is made. Returns the ask, once its `recovery_finished` is
/// recorded too.
fn failed_stuck_exit_job_ask(db: &Path, run: &TaskRun) -> dagq::domain::Ask {
    wait_until(db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
            && !events_of(db, run.id(), "recovery_finished").is_empty()
    });
    let queue = SqliteQueue::open(db).unwrap();
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].clone();
    assert_eq!(ask.kind, AskKind::StuckExit);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert_eq!(ask.options, ["exit", "wait"]);
    assert_eq!(ask.reason_category, dagq::domain::AskReason::RecoveryFailed);
    for part in [
        "Its recovery job looked first, and the recovery job failed (",
        "broken",
        "Why a person: recovery_failed",
        "recovery-stuck_exit-1.prompt.txt",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    assert!(!ask.question.contains("by hand"), "{}", ask.question);
    let finished = events_of(db, run.id(), "recovery_finished");
    assert_eq!(finished[0]["outcome"], "job_failed");
    assert_eq!(finished[0]["escalated"], true);
    assert_eq!(finished[0]["ask_id"], json!(ask.id));
    assert_eq!(finished[0]["reason_category"], "recovery_failed");
    assert!(
        finished[0]["error"].as_str().unwrap().contains("broken"),
        "{finished:?}"
    );
    assert!(events_of(db, run.id(), "recovery_failed").is_empty());
    let status = runtime::status(db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
    ask
}

/// A failed `stuck_exit` job's ask answered `exit`: the answer comes back
/// to the inbox as for any `stuck_exit` ask, no other job starts while it
/// is open, and once the person's `/exit` ends the session the ask is
/// closed by the runtime and the run lands.
#[test]
fn a_failed_stuck_exit_job_opens_the_stuck_exit_ask_answered_exit() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let run = stuck_exit_after_a_pass(&repo, &db, &backend);
    let (reviewer, supervisor) =
        supervise_recovering(&db, &repo, &backend, "echo broken >&2; exit 3".to_owned());
    let ask = failed_stuck_exit_job_ask(&db, &run);
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.answer(ask.id, "exit").unwrap();
    let status = runtime::status(&db).unwrap();
    assert!(
        status["attention"].as_array().unwrap().iter().any(|a| {
            a["ask_id"] == json!(ask.id)
                && a["next"] == format!("read the answer of ask {} and close it", ask.id)
        }),
        "{status}"
    );
    // No other job while the ask is open.
    thread::sleep(Duration::from_millis(500));
    assert_eq!(reviewer.triage_prompts().len(), 1);
    // The person carries out `exit`: the session exits and the run lands.
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_landed(
        &repo,
        &queue.show(TaskId::new(1)).unwrap().runs[0],
        "test task",
        &base,
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert_eq!(reviewer.triage_prompts().len(), 1);
}

/// A failed `stuck_exit` job's ask answered `wait` and closed by the
/// person: the session is left as it is and no other job starts for the
/// alert; the run lands once the session exits by itself later.
#[test]
fn a_failed_stuck_exit_job_opens_the_stuck_exit_ask_answered_wait() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let run = stuck_exit_after_a_pass(&repo, &db, &backend);
    let (reviewer, supervisor) =
        supervise_recovering(&db, &repo, &backend, "echo broken >&2; exit 3".to_owned());
    let ask = failed_stuck_exit_job_ask(&db, &run);
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.answer(ask.id, "wait").unwrap();
    queue.close_ask(ask.id).unwrap();
    thread::sleep(Duration::from_millis(500));
    assert_eq!(reviewer.triage_prompts().len(), 1);
    assert!(queue.asks(AskQuery::default()).unwrap().is_empty());
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().runs[0].status(),
        RunStatus::AwaitingIntegration
    );
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_landed(
        &repo,
        &queue.show(TaskId::new(1)).unwrap().runs[0],
        "test task",
        &base,
    );
    assert_eq!(reviewer.triage_prompts().len(), 1);
}

/// A `stuck_exit` repair the runtime cannot apply (a key to a dialog that
/// is not a known one) is not applied at all: the `stuck_exit` ask opens
/// with the job's diagnosis, its options added to `exit` and `wait`, and
/// its reason category.
#[test]
fn a_stuck_exit_repair_that_does_not_hold_becomes_the_stuck_exit_ask() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let run = stuck_exit_after_a_pass(&repo, &db, &backend);
    let (_reviewer, supervisor) = supervise_recovering(
        &db,
        &repo,
        &backend,
        recovery(json!({
            "verdict": "repair",
            "confidence": "high",
            "diagnosis": "an unknown dialog",
            "actions": [{"action": "answer_known_dialog", "dialog": "background_work"}],
            "options": ["press escape"],
            "reason_category": "scope",
        })),
    );
    // The ask opens before the job's `recovery_finished` is recorded: both
    // are waited for.
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
            && !payloads(&queue.show(TaskId::new(1)).unwrap(), "recovery_finished").is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue.asks(AskQuery::default()).unwrap().remove(0);
    assert_eq!(ask.kind, AskKind::StuckExit);
    assert_eq!(ask.options, ["exit", "wait", "press escape"]);
    assert_eq!(ask.reason_category, dagq::domain::AskReason::Scope);
    for part in [
        "Its recovery job looked first, and the runtime did not apply the recovery job's repair: answer_known_dialog: no known dialog is on the screen",
        "Why a person: scope",
        "Diagnosis: an unknown dialog",
        "recovery-stuck_exit-1.prompt.txt",
        "lands on main once the session exits",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(payloads(&detail, "auto_repaired").is_empty());
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished[0]["escalated"], true);
    assert_eq!(finished[0]["ask_id"], json!(ask.id));
    // The person's /exit reaches the session; the run lands.
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_landed(
        &repo,
        &queue.show(TaskId::new(1)).unwrap().runs[0],
        "test task",
        &base,
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
}

/// A supervisor died while it waited for the session to exit after a
/// passing review, with the exit timeout recorded and no `stuck_exit` ask
/// made yet. The adopter reviews nothing again and sends no second `/exit`
/// (ADR-0027), asks about the stuck exit once with what follows (the
/// landing), closes that ask once the session exits, and lands the run.
#[test]
fn adopted_run_waiting_for_its_exit_after_a_pass_asks_once_and_lands() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let idle = run.idle_marker_path().unwrap();
    wait_until(&db, Duration::from_secs(20), |_| idle.is_file());
    let head = git_out(
        Path::new(run.worktree_path().unwrap()),
        &["rev-parse", "HEAD"],
    );
    // What the dead supervisor got through: validation, a passing review,
    // the /exit and its timeout.
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration', result_commit=?2 WHERE id=?1",
            rusqlite::params![run.id(), head],
        )
        .unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    for (kind, payload) in [
        (
            EventKind::ValidationFinished,
            json!({"status": "awaiting_integration"}),
        ),
        (EventKind::ReviewStarted, json!({"attempt": 1})),
        (
            EventKind::ReviewFinished,
            json!({"verdict": "pass", "reasons": [], "summary": "ok", "attempt": 1}),
        ),
        (
            EventKind::ExitRequested,
            json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
        ),
        (
            EventKind::ExitRequestTimedOut,
            json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
        ),
    ] {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
    age_lease(&db, &run, 31);
    let reviewer = Arc::new(TestReviewer::new(&[verdict("concern", &["x"], "never")]));
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
                &supervise_options(4, true),
            )
        })
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
    });
    thread::sleep(Duration::from_millis(1500));
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].clone();
    assert_eq!(ask.kind, AskKind::StuckExit);
    assert!(
        ask.question
            .contains("The run stays awaiting_integration under the supervisor after its validation and review, and lands on main once the session exits"),
        "{}",
        ask.question
    );
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    // The person's /exit reaches the session.
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    assert!(reviewer.prompts().is_empty(), "reviewed again");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    assert_eq!(kinds.iter().filter(|k| **k == "review_started").count(), 1);
    let closed = queue.read_ask(ask.id).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(
        closed.answer.as_deref(),
        Some("the session exited; closed by the runtime")
    );
    assert_eq!(
        queue
            .asks(AskQuery {
                all: true,
                ..Default::default()
            })
            .unwrap()
            .len(),
        1
    );
}
