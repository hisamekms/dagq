//! Runtime tests: a resume the supervisor takes over (after a handoff, or
//! adopted from a supervisor that died) carries over the dialog and the
//! recovery job the previous process recorded during it (task 743).
use crate::runtime_adopt::backdate_event;
use crate::runtime_handoff::hand_off_when;
use crate::runtime_review_adopt::{DIALOG_SCREEN, screen_hash};
use crate::runtime_support;
use dagq::domain::EventKind;

use dagq::domain::{AskReason, LeaseToken, NewAsk};
use runtime_support::*;

/// A resume taken over by the process its supervisor handed off to, from
/// its `handoff.json`, keeps the dialog recorded during it. Here the run
/// waits outside its slot for the ask (ADR-0071), and goes back to its
/// resume once the session moves.
#[test]
fn a_resume_taken_over_after_a_handoff_keeps_its_dialog_and_clears_it_at_its_end() {
    resume_taken_over_at_dialog(true, dagq::domain::waiting::DEFAULT_MAX_WAITING);
}

/// The same with waits turned off: the resume's own watch reads the screen
/// while the dialog stays up and finds the dialog it took over.
#[test]
fn a_resume_taken_over_after_a_handoff_in_its_slot_does_not_record_its_dialog_again() {
    resume_taken_over_at_dialog(true, 0);
}

/// The same for a resume adopted from a supervisor that died, rebuilt from
/// the run's events.
#[test]
fn an_adopted_resume_keeps_its_dialog_and_clears_it_at_its_end() {
    resume_taken_over_at_dialog(false, dagq::domain::waiting::DEFAULT_MAX_WAITING);
}

/// An adopted resume with waits turned off.
#[test]
fn an_adopted_resume_in_its_slot_does_not_record_its_dialog_again() {
    resume_taken_over_at_dialog(false, 0);
}

/// A resumed session stopped at a dialog before its supervisor handed off
/// (`handoff`) or died: `prompt_waiting` recorded after the resume's
/// `resume_started`, its recovery job escalated to an `answer_prompt` ask.
/// The process that takes the resume over records neither the same screen
/// again nor another recovery job for it, and once the session resolves
/// the conflict and writes its receipt, the resume's end records
/// `prompt_cleared` once and closes the ask, and the run lands.
fn resume_taken_over_at_dialog(handoff: bool, max_waiting: usize) {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.prompt_wait = Duration::from_millis(300);
    let backend = Arc::new(backend);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        "await_message; while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let (_, token) = hand_off_when(&db, &repo, &backend, |queue| {
        event_kinds(&queue.show(TaskId::new(2)).unwrap()).contains(&"resume_request_sent")
    });
    let snapshot = Path::new(run.run_dir().unwrap()).join("handoff.json");
    if !handoff {
        // No state and a dead pid make it a supervisor that died.
        fs::remove_file(&snapshot).unwrap();
        Connection::open(&db)
            .unwrap()
            .execute(
                "UPDATE run_leases SET pid=?2 WHERE run_id=?1",
                rusqlite::params![run.id(), dead_pid()],
            )
            .unwrap();
    }

    // The session now waits at a dialog the previous process recorded and
    // escalated.
    *backend.screen.lock().unwrap() = DIALOG_SCREEN.into();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    let workspace = payloads(&detail, "workspace_created")
        .into_iter()
        .rfind(|p| p["resume_attempt"] == 1)
        .and_then(|p| p["workspace_id"].as_str())
        .unwrap()
        .to_owned();
    for (kind, payload) in [
        (
            EventKind::PromptWaiting,
            json!({
                "workspace_id": workspace,
                "excerpt": "Auto mode is available",
                "screen_hash": screen_hash(DIALOG_SCREEN),
                "prompt": "choice",
            }),
        ),
        (
            EventKind::RecoveryRequested,
            json!({"alert": "prompt_waiting", "attempt": 1}),
        ),
        (
            EventKind::RecoveryFinished,
            json!({"alert": "prompt_waiting", "attempt": 1, "outcome": "escalated"}),
        ),
    ] {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
    let ask = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::AnswerPrompt,
            task_id: Some(run.task_id()),
            run_id: Some(run.id().clone()),
            question: "run waits at a choice dialog".into(),
            options: vec![],
            asked_by: "supervisor".into(),
            reason_category: AskReason::RecoveryFailed,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask;

    let next = {
        let (db, repo, backend, token) = (db.clone(), repo.clone(), backend.clone(), token.clone());
        thread::spawn(move || {
            supervise_with(
                &db,
                &repo,
                &backend,
                &SuperviseOptions {
                    handoff_token: handoff.then(|| LeaseToken::new(token)),
                    max_waiting: Some(max_waiting),
                    ..supervise_options(4, true)
                },
            )
        })
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        payloads(&queue.show(TaskId::new(2)).unwrap(), "auto_repaired")
            .iter()
            .any(|p| p["repair"] == "resume_adopted")
    });
    // Several screen checks later, the dialog still up is the one recorded.
    let captures = backend.captures.load(Ordering::SeqCst);
    wait_until(&db, Duration::from_secs(30), |_| {
        backend.captures.load(Ordering::SeqCst) >= captures + 3
    });
    let detail = queue.show(TaskId::new(2)).unwrap();
    let kinds = event_kinds(&detail);
    assert_eq!(payloads(&detail, "prompt_waiting").len(), 1, "{kinds:?}");
    assert_eq!(
        payloads(&detail, "recovery_requested").len(),
        1,
        "{kinds:?}"
    );
    assert!(payloads(&detail, "prompt_cleared").is_empty(), "{kinds:?}");
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_none());

    // The session resolves the conflict, the dialog left on its screen.
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(next, "the supervisor taking the resume over").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "resume_started").count(), 1);
    assert_eq!(payloads(&detail, "prompt_waiting").len(), 1, "{kinds:?}");
    assert_eq!(
        payloads(&detail, "recovery_requested").len(),
        1,
        "{kinds:?}"
    );
    assert_eq!(payloads(&detail, "prompt_cleared").len(), 1, "{kinds:?}");
    assert!(position(&kinds, "prompt_cleared") < position(&kinds, "resume_finished"));
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert!(queue.asks(AskQuery::default()).unwrap().is_empty());
}

/// A resume whose `/exit` was requested (twice the exit timeout ago) before
/// its supervisor handed off keeps the time already waited: the next
/// process goes to the `stuck_exit` recovery job without waiting the exit
/// timeout again (task 894).
#[test]
fn a_resume_taken_over_after_a_handoff_times_its_exit_from_the_recorded_request() {
    resume_taken_over_after_its_exit(true);
}

/// The same for a resume adopted from a supervisor that died.
#[test]
fn an_adopted_resume_times_its_exit_from_the_recorded_request() {
    resume_taken_over_after_its_exit(false);
}

/// A resumed session asked to exit under the previous process, which
/// recorded `exit_requested` of the attempt 120 seconds ago and never saw
/// the session go: taken over with an exit timeout of 60 seconds, the
/// `/exit` is not typed again and its `stuck_exit` recovery job starts
/// well within the timeout of the takeover.
fn resume_taken_over_after_its_exit(handoff: bool) {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let exit_timeout = Duration::from_secs(60);
    backend.exit_timeout = exit_timeout;
    let backend = Arc::new(backend);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        "await_message; while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let (_, token) = hand_off_when(&db, &repo, &backend, |queue| {
        event_kinds(&queue.show(TaskId::new(2)).unwrap()).contains(&"resume_request_sent")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    let workspace = payloads(&detail, "workspace_created")
        .into_iter()
        .rfind(|p| p["resume_attempt"] == 1)
        .and_then(|p| p["workspace_id"].as_str())
        .unwrap()
        .to_owned();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::ExitRequested,
            json!({"workspace_id": workspace, "timeout_secs": 60, "resume_attempt": 1}),
        )
        .unwrap();
    backdate_event(&db, &run, "exit_requested", 120);
    let snapshot = Path::new(run.run_dir().unwrap()).join("handoff.json");
    if handoff {
        // The state the handing-off process wrote had the request.
        let mut written: Value = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
        assert_eq!(written["phase"], "resume");
        written["exit_requested"] = json!(true);
        fs::write(&snapshot, serde_json::to_vec(&written).unwrap()).unwrap();
    } else {
        // No state and a dead pid make it a supervisor that died.
        fs::remove_file(&snapshot).unwrap();
        Connection::open(&db)
            .unwrap()
            .execute(
                "UPDATE run_leases SET pid=?2 WHERE run_id=?1",
                rusqlite::params![run.id(), dead_pid()],
            )
            .unwrap();
    }

    let next = {
        let (db, repo, backend, token) = (db.clone(), repo.clone(), backend.clone(), token.clone());
        thread::spawn(move || {
            supervise_with(
                &db,
                &repo,
                &backend,
                &SuperviseOptions {
                    handoff_token: handoff.then(|| LeaseToken::new(token)),
                    ..supervise_options(4, true)
                },
            )
        })
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        payloads(&queue.show(TaskId::new(2)).unwrap(), "recovery_requested")
            .iter()
            .any(|p| p["alert"] == "stuck_exit")
    });
    let detail = queue.show(TaskId::new(2)).unwrap();
    let at = |kind: &str, keep: &dyn Fn(&Value) -> bool| {
        let event = detail
            .events
            .iter()
            .find(|e| e.kind == kind && keep(&e.payload))
            .unwrap();
        dagq::domain::stats::timestamp_millis(&event.created_at).unwrap()
    };
    let adopted = at("auto_repaired", &|p| p["repair"] == "resume_adopted");
    let recovery = at("recovery_requested", &|p| p["alert"] == "stuck_exit");
    let waited = Duration::from_millis(u64::try_from(recovery - adopted).unwrap());
    assert!(
        waited < exit_timeout,
        "stuck_exit recovery {waited:?} after the takeover"
    );
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);

    // The session let go resolves the conflict and exits; the run is
    // resumed again and lands.
    let exit = exit_request_path(run.run_dir().unwrap());
    fs::write(exit.with_extension("go"), "").unwrap();
    fs::write(&exit, "").unwrap();
    let outcome = joined(next, "the supervisor taking the resume over").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let kinds = event_kinds(&queue.show(TaskId::new(2)).unwrap())
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(!kinds.iter().any(|k| k == "runtime_error"), "{kinds:?}");
}
