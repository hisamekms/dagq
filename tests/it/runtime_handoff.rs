//! Runtime tests: The handoff of a supervisor to the next process.
use crate::runtime_adopt::backdate_event;
use crate::runtime_support;
use dagq::domain::EventKind;
use dagq::domain::LeaseToken;

use runtime_support::*;

/// The only registered supervisor.
fn only_registration(db: &Path) -> dagq::domain::SupervisorRegistration {
    let registrations = SqliteQueue::open(db).unwrap().supervisors().unwrap();
    assert_eq!(registrations.len(), 1, "{registrations:?}");
    registrations.into_iter().next().unwrap()
}

/// Supervise until `condition` holds on the queue, then ask the supervisor
/// to hand off (ADR-0045 decision 10) and return its outcome and token.
pub(crate) fn hand_off_when(
    db: &Path,
    repo: &Path,
    backend: &Arc<TestWorkspace>,
    condition: impl FnMut(&mut SqliteQueue) -> bool,
) -> (Value, String) {
    let supervisor = {
        let (db, repo, backend) = (db.to_owned(), repo.to_owned(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    // The first supervisor may be slow to reach the condition under load;
    // it moves on as soon as the condition holds.
    wait_until(db, crate::common::STEP_LIMIT, condition);
    let registration = only_registration(db);
    assert!(registration.handoff_accepted, "{registration:?}");
    assert_eq!(registration.handoff_binary, None);
    let queue = SqliteQueue::open(db).unwrap();
    assert!(
        queue
            .request_handoff(&registration.token, "/next/dagq")
            .unwrap()
    );
    assert_eq!(
        queue
            .handoff_request(&registration.token)
            .unwrap()
            .as_deref(),
        Some("/next/dagq")
    );
    let outcome = joined(supervisor, "the supervisor asked to hand off").unwrap();
    assert_eq!(outcome["outcome"], "handoff", "{outcome}");
    assert_eq!(outcome["binary"], "/next/dagq");
    assert_eq!(outcome["token"], json!(registration.token));
    // The registration stays for the exec'd process, request and all.
    let kept = only_registration(db);
    assert_eq!(kept.token, registration.token);
    assert_eq!(kept.handoff_binary.as_deref(), Some("/next/dagq"));
    (outcome, registration.token.into_string())
}

/// The supervisor the exec'd binary runs: the same token, continued.
fn supervise_after_handoff(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    token: &str,
) -> Result<Value> {
    supervise_after_handoff_with(db, repo, backend, token, supervise_options(4, true))
}

/// [`supervise_after_handoff`] with `options` (its passes counted where a
/// test reads them).
fn supervise_after_handoff_with(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    token: &str,
    options: SuperviseOptions,
) -> Result<Value> {
    supervise_with(
        db,
        repo,
        backend,
        &SuperviseOptions {
            handoff_token: Some(LeaseToken::new(token)),
            ..options
        },
    )
}

/// A handoff does not wait for the session (ADR-0045 decision 10): the
/// supervisor ends its loop while the worker still works, keeping its
/// registration and the run's lease, and the process that continues it
/// under the same token takes the run over without an adoption and
/// drives it to its receipt, one `/exit` and validation.
#[test]
fn a_handoff_leaves_the_session_running_and_the_next_process_drives_it_on() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, PROMPTED_AGENT));
    let (outcome, token) = hand_off_when(&db, &repo, &backend, |queue| {
        queue
            .show(TaskId::new(1))
            .unwrap()
            .runs
            .first()
            .is_some_and(|run| run.status() == RunStatus::Running)
    });
    assert_eq!(outcome["handed_over"], 1, "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::Running);
    assert_eq!(queue.run_lease(run.id()).unwrap().unwrap().token, token);
    // The session was not asked anything.
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);

    let next = {
        let (db, repo, backend, token) = (db.clone(), repo.clone(), backend.clone(), token.clone());
        thread::spawn(move || supervise_after_handoff(&db, &repo, &backend, &token))
    };
    // The registration is taken back before the run moves on.
    wait_until(&db, Duration::from_secs(30), |queue| {
        queue
            .supervisors()
            .unwrap()
            .first()
            .is_some_and(|r| r.handoff_binary.is_none())
    });
    let taken = only_registration(&db);
    assert_eq!(taken.token, token);
    assert_eq!(taken.pid, std::process::id());
    assert_eq!(taken.binary_version.as_deref(), Some(VERSION));
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(next, "the supervisor after the handoff").unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished", "{outcome}");
    assert_eq!(outcome["errors"], json!([]));
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);

    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"run_adopted"), "{kinds:?}");
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    assert_eq!(kinds.iter().filter(|k| **k == "lease_acquired").count(), 1);
    assert_eq!(supervisor_token_of(&db, &run), token);
    assert_eq!(detail.runs.len(), 1);
    assert!(detail.runs[0].result_commit().is_some());
    let handed = events_of(&db, run.id(), runtime::SUPERVISOR_HANDED_OFF);
    assert_eq!(handed.len(), 1);
    assert_eq!(handed[0]["status"], "running");
    assert_eq!(handed[0]["state"], Value::Null);
    assert_eq!(handed[0]["supervisor"], json!(token));
    assert_eq!(handed[0]["previous_version"], VERSION);
    assert_eq!(handed[0]["version"], VERSION);
    // The continued supervisor ended like any other and removed its row.
    assert!(queue.supervisors().unwrap().is_empty());
    // The change marks (ADR-0051 decision 10): the start, the handoff and
    // one stop; the process that exec'd records no stop of its own.
    let marks: Vec<(String, Value)> = queue
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind.starts_with("supervisor_") && event.task_id.is_none())
        .map(|event| (event.kind, event.payload))
        .collect();
    let kinds: Vec<&str> = marks.iter().map(|(kind, _)| kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "supervisor_started",
            "supervisor_started",
            "supervisor_stopped"
        ]
    );
    assert_eq!(marks[0].1["handoff"], json!(false));
    assert_eq!(marks[1].1["handoff"], json!(true));
    assert_eq!(marks[1].1["previous_version"], VERSION);
    assert!(
        marks
            .iter()
            .all(|(_, payload)| payload["supervisor"] == json!(token))
    );
}

/// A run rejected by validation waits for its session to take the `/exit`;
/// a handoff in that wait carries the request over in the run's
/// `handoff.json`, so the next process sends no second `/exit` and lets
/// the run rest once the session exits.
#[test]
fn a_handoff_while_a_rejected_run_waits_for_its_exit_sends_no_second_exit() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        &format!(
            "commit work; printf 'scratch\\n' > untracked.txt; receipt \"$(git rev-parse HEAD)\"; idle; {HOLD}"
        ),
    ));
    let (outcome, token) = hand_off_when(&db, &repo, &backend, |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"exit_requested")
    });
    assert_eq!(outcome["handed_over"], 1, "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::Failed);
    let snapshot = Path::new(run.run_dir().unwrap()).join("handoff.json");
    let written: Value = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
    assert_eq!(written["phase"], "exit");
    assert_eq!(written["requested"], true);
    assert_eq!(written["close"], false);

    let next = {
        let (db, repo, backend, token) = (db.clone(), repo.clone(), backend.clone(), token.clone());
        thread::spawn(move || supervise_after_handoff(&db, &repo, &backend, &token))
    };
    wait_until(&db, Duration::from_secs(30), |_| !snapshot.exists());
    release_held_session(run.run_dir().unwrap());
    let outcome = joined(next, "the supervisor after the handoff").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "failed");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    assert!(!kinds.contains(&"run_adopted"), "{kinds:?}");
}

/// The `/exit` carried over in `handoff.json` keeps the time already
/// waited (task 894): requested 120 seconds before the takeover with an
/// exit timeout of 60 seconds, the next process records
/// `exit_request_timed_out` without waiting the timeout again, and sends
/// no second `/exit`.
#[test]
fn a_handoff_while_a_run_waits_for_its_exit_times_it_from_the_recorded_request() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "commit work; printf 'scratch\\n' > untracked.txt; receipt \"$(git rev-parse HEAD)\"; idle; {HOLD}"
        ),
    );
    let exit_timeout = Duration::from_secs(60);
    backend.exit_timeout = exit_timeout;
    let backend = Arc::new(backend);
    let (_, token) = hand_off_when(&db, &repo, &backend, |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"exit_requested")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let snapshot = Path::new(run.run_dir().unwrap()).join("handoff.json");
    let written: Value = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
    assert_eq!(written["phase"], "exit");
    assert_eq!(written["requested"], true);
    backdate_event(&db, &run, "exit_requested", 120);

    let next = {
        let (db, repo, backend, token) = (db.clone(), repo.clone(), backend.clone(), token.clone());
        thread::spawn(move || supervise_after_handoff(&db, &repo, &backend, &token))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"exit_request_timed_out")
    });
    let detail = queue.show(TaskId::new(1)).unwrap();
    let at = |kind: &str| {
        let event = detail.events.iter().find(|e| e.kind == kind).unwrap();
        dagq::domain::stats::timestamp_millis(&event.created_at).unwrap()
    };
    let waited = Duration::from_millis(
        u64::try_from(at("exit_request_timed_out") - at("supervisor_handed_off")).unwrap(),
    );
    assert!(
        waited < exit_timeout,
        "timed out {waited:?} after the handoff"
    );
    release_held_session(run.run_dir().unwrap());
    let outcome = joined(next, "the supervisor after the handoff").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "failed");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    assert!(!kinds.contains(&"run_adopted"), "{kinds:?}");
}

/// A `stuck_exit` recovery job that answered the dialog holding the `/exit`
/// back gave it its timeout again: carried over in `handoff.json`, the
/// timeout runs from that repair, not from the request 120 seconds ago
/// (task 894), so the next process records no `exit_request_timed_out` at
/// once.
#[test]
fn a_handoff_after_a_recovery_repair_of_the_exit_times_it_from_the_repair() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "commit work; printf 'scratch\\n' > untracked.txt; receipt \"$(git rev-parse HEAD)\"; idle; {HOLD}"
        ),
    );
    backend.exit_timeout = Duration::from_secs(60);
    let backend = Arc::new(backend);
    let (_, token) = hand_off_when(&db, &repo, &backend, |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"exit_requested")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let snapshot = Path::new(run.run_dir().unwrap()).join("handoff.json");
    backdate_event(&db, &run, "exit_requested", 120);
    queue
        .record_runtime_event(
            run.id(),
            EventKind::AutoRepaired,
            json!({"layer": "recovery", "repair": "answer_known_dialog", "alert": "stuck_exit", "attempt": 1}),
        )
        .unwrap();

    let options = supervise_options(4, true);
    let passes = options.passes.clone();
    let next = {
        let (db, repo, backend, token) = (db.clone(), repo.clone(), backend.clone(), token.clone());
        thread::spawn(move || supervise_after_handoff_with(&db, &repo, &backend, &token, options))
    };
    wait_until(&db, Duration::from_secs(30), |_| !snapshot.exists());
    // Several passes of the next process.
    await_passes(&passes, SOME_PASSES);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"exit_request_timed_out"), "{kinds:?}");
    release_held_session(run.run_dir().unwrap());
    let outcome = joined(next, "the supervisor after the handoff").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "failed");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
}

/// A handoff while a resumed session resolves a conflict carries the
/// resume over in `handoff.json`: the next process goes on watching that
/// session instead of resuming the run again, and records it as
/// `auto_repaired` (`repair: resume_adopted`, ADR-0047 decision 24).
#[test]
fn a_handoff_during_a_resume_goes_on_watching_the_resumed_session() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let (run, _) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        "await_message; while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let (outcome, token) = hand_off_when(&db, &repo, &backend, |queue| {
        event_kinds(&queue.show(TaskId::new(2)).unwrap()).contains(&"resume_started")
    });
    assert_eq!(outcome["handed_over"], 1, "{outcome}");
    let snapshot = Path::new(run.run_dir().unwrap()).join("handoff.json");
    let written: Value = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
    assert_eq!(written["phase"], "resume");

    let next = {
        let (db, repo, backend, token) = (db.clone(), repo.clone(), backend.clone(), token.clone());
        thread::spawn(move || supervise_after_handoff(&db, &repo, &backend, &token))
    };
    wait_until(&db, Duration::from_secs(30), |_| !snapshot.exists());
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(next, "the supervisor after the handoff").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "resume_started").count(), 1);
    let adopted: Vec<&Value> = payloads(&detail, "auto_repaired")
        .into_iter()
        .filter(|p| p["repair"] == "resume_adopted")
        .collect();
    assert_eq!(adopted.len(), 1, "{kinds:?}");
    assert_eq!(adopted[0]["layer"], "runtime");
    assert_eq!(adopted[0]["conditions"]["handoff"], true);
    assert_eq!(adopted[0]["conditions"]["attempt"], 1);
    assert_eq!(adopted[0]["detail"]["version"], VERSION);
    // The conflict-only resume was not counted, and that too is a repair.
    assert!(
        payloads(&detail, "auto_repaired")
            .iter()
            .any(|p| p["repair"] == "conflict_resume_uncounted"),
        "{kinds:?}"
    );
}

/// Task 640: a handoff during a resume whose `handoff.json` could not be
/// written (here: is gone) still goes on watching the resumed session: the
/// next process finds the `needs_session` run under its token with a live
/// session, keeps the lease and rebuilds the resume from the run's events
/// instead of giving the lease back, and records `auto_repaired`
/// (`repair: resume_adopted`, `handoff: true`, with the previous version).
#[test]
fn a_handoff_without_its_state_during_a_resume_still_watches_the_session() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let (run, _) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        "await_message; while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let (_, token) = hand_off_when(&db, &repo, &backend, |queue| {
        event_kinds(&queue.show(TaskId::new(2)).unwrap()).contains(&"resume_request_sent")
    });
    fs::remove_file(Path::new(run.run_dir().unwrap()).join("handoff.json")).unwrap();

    let next = {
        let (db, repo, backend, token) = (db.clone(), repo.clone(), backend.clone(), token.clone());
        thread::spawn(move || supervise_after_handoff(&db, &repo, &backend, &token))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        payloads(&queue.show(TaskId::new(2)).unwrap(), "auto_repaired")
            .iter()
            .any(|p| p["repair"] == "resume_adopted")
    });
    // The lease stayed with the token: nothing was given back.
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(queue.run_lease(run.id()).unwrap().unwrap().token, token);
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(next, "the supervisor after the handoff").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "resume_started").count(), 1);
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "resume_request_sent")
            .count(),
        1
    );
    assert!(!kinds.contains(&"run_adopted"), "{kinds:?}");
    let handed = payloads(&detail, "supervisor_handed_off");
    assert_eq!(handed.len(), 1, "{kinds:?}");
    assert_eq!(handed[0]["status"], "needs_session");
    assert_eq!(handed[0]["state"], Value::Null);
    let repaired: Vec<&Value> = payloads(&detail, "auto_repaired")
        .into_iter()
        .filter(|p| p["repair"] == "resume_adopted")
        .collect();
    assert_eq!(repaired.len(), 1, "{kinds:?}");
    assert_eq!(repaired[0]["conditions"]["handoff"], true);
    assert_eq!(repaired[0]["conditions"]["attempt"], 1);
    assert_eq!(repaired[0]["conditions"]["request_sent"], true);
    assert!(
        repaired[0]["detail"]
            .as_object()
            .unwrap()
            .contains_key("previous_version"),
        "{:?}",
        repaired[0]
    );
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["outcome"], "resolved");
}

/// Task 640: a `needs_session` run found after a handoff without its
/// `handoff.json` whose resumed session has ended has nothing to watch: its
/// lease is given back as before, no `resume_adopted` is recorded, and the
/// supervisor resumes it again.
#[test]
fn a_handoff_without_its_state_after_the_resumed_session_ended_gives_the_lease_back() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let (run, _) = parked_conflict(&repo, &db, &backend);
    // The first resumed session ends without resolving anything.
    backend.resume_script_for(
        2,
        "await_message; while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done",
    );
    let (_, token) = hand_off_when(&db, &repo, &backend, |queue| {
        event_kinds(&queue.show(TaskId::new(2)).unwrap()).contains(&"resume_request_sent")
    });
    fs::remove_file(Path::new(run.run_dir().unwrap()).join("handoff.json")).unwrap();
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    backend.join();
    // The next resume resolves the conflict.
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );

    let outcome = supervise_after_handoff(&db, &repo, &backend, &token).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "resume_started").count(), 2);
    let handed = payloads(&detail, "supervisor_handed_off");
    assert_eq!(handed.len(), 1, "{kinds:?}");
    assert_eq!(handed[0]["state"], Value::Null);
    assert!(
        !payloads(&detail, "auto_repaired")
            .iter()
            .any(|p| p["repair"] == "resume_adopted"),
        "{kinds:?}"
    );
}

/// Task 356: a supervisor that stops during a resume (its lease goes
/// stale while the resumed session lives on, and it leaves no
/// `handoff.json`) is replaced by one with another token, which adopts the
/// `needs_session` run, rebuilds the resume from its events and goes on
/// watching the session: no second resume and no second resolution
/// request, and the run lands. The adoption is `run_adopted` and
/// `auto_repaired` (`repair: resume_adopted`, `handoff: false`).
#[test]
fn a_supervisor_that_stops_during_a_resume_leaves_the_session_to_its_adopter() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let (run, _) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        "await_message; while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    // The first supervisor stops once the request is typed; the handoff
    // only ends its loop, and dropping its state and its pid makes it one
    // that died.
    let (_, token) = hand_off_when(&db, &repo, &backend, |queue| {
        event_kinds(&queue.show(TaskId::new(2)).unwrap()).contains(&"resume_request_sent")
    });
    fs::remove_file(Path::new(run.run_dir().unwrap()).join("handoff.json")).unwrap();
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET pid=?2 WHERE run_id=?1",
            rusqlite::params![run.id(), dead_pid()],
        )
        .unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::NeedsSession
    );
    assert_eq!(queue.run_lease(run.id()).unwrap().unwrap().token, token);

    let next = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(2)).unwrap()).contains(&"run_adopted")
    });
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(next, "the adopting supervisor").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "resume_started").count(), 1);
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "resume_request_sent")
            .count(),
        1
    );
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    assert!(!kinds.contains(&"supervisor_handed_off"), "{kinds:?}");
    let requests = backend
        .texts
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, text)| text.contains("main is now"))
        .count();
    assert_eq!(requests, 1);
    let adopted = adoption_events(&detail);
    assert_eq!(adopted.len(), 1, "{kinds:?}");
    assert_eq!(adopted[0]["previous_token"], json!(token));
    assert_eq!(adopted[0]["wrapper"]["alive"], true);
    let repaired: Vec<&Value> = payloads(&detail, "auto_repaired")
        .into_iter()
        .filter(|p| p["repair"] == "resume_adopted")
        .collect();
    assert_eq!(repaired.len(), 1, "{kinds:?}");
    assert_eq!(repaired[0]["conditions"]["handoff"], false);
    assert_eq!(repaired[0]["conditions"]["attempt"], 1);
    assert_eq!(repaired[0]["conditions"]["request_sent"], true);
    assert!(repaired[0]["detail"].get("previous_version").is_none());
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["outcome"], "resolved");
}

/// A handoff that finds a leased run it has nothing to rebuild from (a
/// resting run without a `handoff.json`) gives the lease back, and a
/// registration that is gone refuses the continuation.
#[test]
fn after_a_handoff_a_run_without_state_gives_its_lease_back() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .register_supervisor(&LeaseToken::new("gone-by"), std::process::id(), 1, "0.0.1")
        .unwrap();
    // No handoff for a registration that does not take one.
    assert!(
        !queue
            .request_handoff(&LeaseToken::new("gone-by"), "/next/dagq")
            .unwrap()
    );
    queue.accept_handoff(&LeaseToken::new("gone-by")).unwrap();
    assert!(
        queue
            .request_handoff(&LeaseToken::new("gone-by"), "/next/dagq")
            .unwrap()
    );
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "gone-by");
    backend.join();
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='succeeded' WHERE id=?1",
            [run.id()],
        )
        .unwrap();
    assert_eq!(
        queue
            .runs_leased_by(&LeaseToken::new("gone-by"))
            .unwrap()
            .len(),
        1
    );
    let outcome = supervise_after_handoff(&db, &repo, &backend, "gone-by").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert!(
        queue
            .runs_leased_by(&LeaseToken::new("gone-by"))
            .unwrap()
            .is_empty()
    );
    let error = supervise_after_handoff(&db, &repo, &backend, "gone-by").unwrap_err();
    assert!(
        format!("{error:#}").contains("is no longer registered"),
        "{error:#}"
    );
}

/// The supervisor's look at main for the automatic update (ADR-0045
/// decision 17): with `auto_update` on its registration it starts the
/// update job for main's head when the commits since the last update
/// change the runtime, not for documentation alone, and builds the same
/// head again when the `update_failed` ask is answered `retry`. The stub
/// build fails, so nothing is ever replaced.
#[test]
fn auto_update_builds_runtime_landings_and_retries_on_the_answer() {
    let (_fixture, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    // The automatic update builds only dagq's source (ADR-t614-1).
    fs::write(repo.join("Cargo.toml"), "[package]\nname = \"dagq\"\n").unwrap();
    git(&repo, &["add", "Cargo.toml"]);
    git(&repo, &["commit", "-m", "dagq's manifest"]);
    let options = SuperviseOptions {
        update: dagq::application::supervise::UpdateSettings {
            register: true,
            interval: Duration::ZERO,
            build_command: Some("echo no build here >&2; exit 1".into()),
            e2e_command: None,
            e2e_timeout: None,
            poll: None,
            cmux: Some(PathBuf::from("/usr/bin/true")),
            cargo: None,
        },
        ..supervise_options(1, true)
    };
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let head = |repo: &Path| {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "main"])
            .bounded_output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    let commit = |path: &str| {
        let file = repo.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, path).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", path]);
        head(&repo)
    };
    let updates = || SqliteQueue::open(&db).unwrap().update_events(100).unwrap();
    let of = |kind: &str, sha: &str| {
        updates()
            .iter()
            .filter(|u| u.kind == kind && u.payload["commit"] == sha)
            .count()
    };
    let wait_failed = |sha: &str, times: usize| {
        let started = Instant::now();
        while of("update_failed", sha) < times {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "the job for {sha} did not fail: {:?}",
                updates()
            );
            thread::sleep(Duration::from_millis(100));
        }
    };

    // This build names a commit the repository does not have: main's head
    // is built once.
    let seed = head(&repo);
    supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(of("update_started", &seed), 1, "{:?}", updates());
    wait_failed(&seed, 1);
    let failed = updates()
        .into_iter()
        .find(|u| u.kind == "update_failed")
        .unwrap();
    assert_eq!(failed.payload["stage"], "build", "{failed:?}");
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["auto_update"]["state"], "failed", "{status}");
    assert_eq!(status["auto_update"]["enabled"], false, "{status}");

    // Nothing new on main, then documentation only: no job.
    supervise_with(&db, &repo, &backend, &options).unwrap();
    let docs = commit("docs/notes.md");
    supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(of("update_started", &seed), 1);
    assert_eq!(of("update_started", &docs), 0, "{:?}", updates());

    // `retry` builds main's head again, whatever it changed.
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.kind == dagq::domain::AskKind::UpdateFailed)
        .unwrap();
    queue.answer(ask.id, "retry").unwrap();
    supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(of("update_started", &docs), 1, "{:?}", updates());
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert!(updates().iter().any(|u| u.kind == "update_retry"));
    wait_failed(&docs, 1);

    // A change of the runtime starts the job for it.
    let source = commit("src/lib.rs");
    supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(of("update_started", &source), 1, "{:?}", updates());
    wait_failed(&source, 1);

    // A job that died without recording how it ended is reported, not
    // rebuilt on its own; an answer row does not hide a running job.
    let queue = SqliteQueue::open(&db).unwrap();
    queue
        .record_queue_event(
            EventKind::UpdateStarted,
            json!({"pid": 999_999_999u32, "commit": source}),
        )
        .unwrap();
    supervise_with(&db, &repo, &backend, &options).unwrap();
    let latest = updates().remove(0);
    assert_eq!(latest.kind, "update_failed", "{latest:?}");
    assert_eq!(latest.payload["stage"], "interrupted", "{latest:?}");
    assert_eq!(of("update_started", &source), 2);
    // So is one that died after it put the old binary back.
    queue
        .record_queue_event(
            EventKind::UpdateRestored,
            json!({"pid": 999_999_999u32, "commit": source}),
        )
        .unwrap();
    supervise_with(&db, &repo, &backend, &options).unwrap();
    let latest = updates().remove(0);
    assert_eq!(latest.kind, "update_failed", "{latest:?}");
    assert_eq!(latest.payload["after"], "update_restored", "{latest:?}");
    assert_eq!(of("update_started", &source), 2);
    queue
        .record_queue_event(
            EventKind::UpdateStarted,
            json!({"pid": std::process::id(), "commit": source}),
        )
        .unwrap();
    queue
        .record_queue_event(EventKind::UpdateRetry, json!({}))
        .unwrap();
    supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(of("update_started", &source), 3, "{:?}", updates());
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["auto_update"]["state"], "building", "{status}");
    backend.join();
}

#[test]
fn a_resume_handoff_preserves_the_exit_for_its_background_dialog() {
    resumed_background_dialog(true);
}

#[test]
fn a_resume_without_a_snapshot_recovers_the_exit_for_its_background_dialog() {
    resumed_background_dialog(false);
}

fn resumed_background_dialog(snapshot: bool) {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.exit_timeout = Duration::from_secs(60);
    let backend = Arc::new(backend);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        &format!("await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; {HOLD}"),
    );
    let before = backend.exits_sent.load(Ordering::SeqCst);
    let (_, token) = hand_off_when(&db, &repo, &backend, |queue| {
        payloads(&queue.show(TaskId::new(2)).unwrap(), "exit_requested")
            .iter()
            .any(|p| p["resume_attempt"] == 1)
    });
    let path = Path::new(run.run_dir().unwrap()).join("handoff.json");
    let written: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(written["exit_typed"], true);
    if !snapshot {
        fs::remove_file(path).unwrap();
    }
    backdate_event(&db, &run, "exit_requested", 120);
    *backend.screen.lock().unwrap() = "Background work is running\n❯ 1. Exit and stop tasks\n  2. Move to background and exit\n  3. Stay\nEnter to confirm · Esc to cancel".into();
    let outcome = supervise_after_handoff(&db, &repo, &backend, &token).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(*backend.keys.lock().unwrap(), ["enter"]);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst) - before, 1);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    assert!(
        payloads(&detail, "auto_repaired")
            .iter()
            .any(|p| p["repair"] == "dialog_answered"
                && p["conditions"]["exit_requested"] == true
                && p["conditions"]["clean"] == true
                && p["conditions"]["receipt_commit"] == p["conditions"]["head"])
    );
    assert!(
        queue
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .all(|a| a.kind != AskKind::StuckExit)
    );
}

#[test]
fn a_resume_handoff_before_exit_does_not_answer_background_work() {
    resumed_background_dialog_is_not_answered(true, false);
}

#[test]
fn an_adopted_resume_before_exit_does_not_answer_background_work() {
    resumed_background_dialog_is_not_answered(false, false);
}

#[test]
fn an_adopted_resume_with_an_unsent_exit_does_not_answer_background_work() {
    resumed_background_dialog_is_not_answered(false, true);
}

fn resumed_background_dialog_is_not_answered(snapshot: bool, unsent: bool) {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.exit_timeout = Duration::from_secs(60);
    let backend = Arc::new(backend);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        &format!("await_message; resolve; receipt \"$(git rev-parse HEAD)\"; {HOLD}"),
    );
    let before = backend.exits_sent.load(Ordering::SeqCst);
    let (_, token) = hand_off_when(&db, &repo, &backend, |queue| {
        event_kinds(&queue.show(TaskId::new(2)).unwrap()).contains(&"resume_request_sent")
    });
    let path = Path::new(run.run_dir().unwrap()).join("handoff.json");
    let written: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(written["exit_requested"], false);
    assert_eq!(written["exit_typed"], false);
    if !snapshot {
        fs::remove_file(path).unwrap();
    }
    if unsent {
        let queue = SqliteQueue::open(&db).unwrap();
        for (kind, payload) in [
            (EventKind::ExitRequested, json!({"resume_attempt": 1})),
            (
                EventKind::ExitRequestTimedOut,
                json!({"resume_attempt": 1, "unsent": true}),
            ),
        ] {
            queue.record_runtime_event(run.id(), kind, payload).unwrap();
        }
    }
    *backend.screen.lock().unwrap() = "Background work is running\n❯ 1. Exit and stop tasks\n  2. Move to background and exit\n  3. Stay\nEnter to confirm · Esc to cancel".into();
    let options = SuperviseOptions {
        exit: Some(dagq::domain::exit::ExitConfig {
            retries: 1,
            intervals: vec![Duration::from_secs(60)],
        }),
        ..supervise_options(4, true)
    };
    let passes = options.passes.clone();
    let next = {
        let (db, repo, backend, token) = (db.clone(), repo.clone(), backend.clone(), token.clone());
        thread::spawn(move || supervise_after_handoff_with(&db, &repo, &backend, &token, options))
    };
    await_passes(&passes, SOME_PASSES);
    assert!(backend.keys.lock().unwrap().is_empty());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), before);
    SqliteQueue::open(&db)
        .unwrap()
        .request_handoff(&LeaseToken::new(token), "/next/dagq")
        .unwrap();
    let outcome = joined(next, "negative dialog handoff").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let restored: Value = serde_json::from_slice(
        &fs::read(Path::new(run.run_dir().unwrap()).join("handoff.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(restored["exit_typed"], false);
    release_held_session(run.run_dir().unwrap());
    backend.join();
}
