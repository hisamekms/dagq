//! Runtime tests: The handoff of a supervisor to the next process.
use crate::runtime_support;
use dagq::domain::EventKind;
use dagq::domain::LeaseToken;
use dagq::infrastructure::git_binary::git_executable;

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
    // The registration stays for the exec'd process; the request was taken
    // before the supervisor prepared the exec.
    let kept = only_registration(db);
    assert_eq!(kept.token, registration.token);
    assert_eq!(kept.handoff_binary, None);
    assert_eq!(
        Connection::open(db)
            .unwrap()
            .query_row(
                "SELECT handoff_requested_at FROM supervisors WHERE token=?1",
                [&registration.token],
                |row| row.get::<_, Option<i64>>(0)
            )
            .unwrap(),
        None
    );
    assert!(
        !queue
            .cancel_handoff(&registration.token, "/next/dagq")
            .unwrap()
    );
    (outcome, registration.token.into_string())
}

#[test]
fn taking_a_handoff_requires_the_same_token_and_binary_and_beats_cancellation() {
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let token = LeaseToken::new("handoff-owner");
    queue
        .register_supervisor(&token, std::process::id(), 1, "old")
        .unwrap();
    queue.accept_handoff(&token).unwrap();
    assert!(queue.request_handoff(&token, "/next/dagq").unwrap());
    assert!(
        !queue
            .take_handoff(&LeaseToken::new("other"), "/next/dagq")
            .unwrap()
    );
    assert!(!queue.take_handoff(&token, "/other/dagq").unwrap());
    assert_eq!(
        queue.handoff_request(&token).unwrap().as_deref(),
        Some("/next/dagq")
    );
    assert!(queue.cancel_handoff(&token, "/next/dagq").unwrap());
    assert!(!queue.take_handoff(&token, "/next/dagq").unwrap());

    assert!(queue.request_handoff(&token, "/replacement/dagq").unwrap());
    assert!(!queue.take_handoff(&token, "/next/dagq").unwrap());
    assert!(queue.take_handoff(&token, "/replacement/dagq").unwrap());
    assert_eq!(queue.handoff_request(&token).unwrap(), None);
    // Mid-exec, it takes no other request until it registers again.
    assert!(!only_registration(&db).handoff_accepted);
    assert!(!queue.request_handoff(&token, "/next/dagq").unwrap());
    assert_eq!(
        Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT handoff_requested_at FROM supervisors WHERE token=?1",
                [&token],
                |row| row.get::<_, Option<i64>>(0)
            )
            .unwrap(),
        None
    );
    assert!(!queue.cancel_handoff(&token, "/replacement/dagq").unwrap());
    let back = queue
        .resume_registration(&token, std::process::id(), "new")
        .unwrap();
    assert!(back.handoff_accepted);
    assert!(queue.request_handoff(&token, "/next/dagq").unwrap());
}

/// A supervisor already draining a landing sees a withdrawn request on its
/// next pass. It keeps its registration and can claim the next task.
#[test]
fn canceling_a_handoff_during_a_landing_resumes_claims_without_exec() {
    let (_dir, repo, db) = fixture();
    let release = db.with_extension("release");
    // The fixture's directory has an apostrophe (`queue's data`): quoted
    // whole, or the shell fails at once and the landing ends early.
    let verification = json!([crate::common::await_path(&release)]);
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE tasks SET verification_commands=?1 WHERE id=1",
            [verification.to_string()],
        )
        .unwrap();
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "ready to land")]);
    // Two slots: the landing holds one until the release, and the task
    // added after the cancellation is claimed in the other.
    let options = supervise_options(2, false);
    let passes = options.passes.clone();
    let stop = options.stop.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options))
    };
    wait_until(&db, crate::common::STEP_LIMIT, |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"integration_started")
    });
    let token = only_registration(&db).token;
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.request_handoff(&token, "/next/dagq").unwrap());
    await_passes_landing(&passes, &mut queue);
    assert_eq!(
        queue.handoff_request(&token).unwrap().as_deref(),
        Some("/next/dagq")
    );
    assert!(queue.cancel_handoff(&token, "/next/dagq").unwrap());
    await_passes_landing(&passes, &mut queue);
    assert_eq!(only_registration(&db).token, token);
    add_ready_task(&mut queue, "after cancellation", &[]);
    wait_until(&db, crate::common::STEP_LIMIT, |queue| {
        !queue.show(TaskId::new(2)).unwrap().runs.is_empty()
    });
    std::fs::write(&release, "go").unwrap();
    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "supervisor after the handoff was canceled");
    assert_ne!(outcome["outcome"], "handoff", "{outcome}");
}

/// A supervisor draining a landing for a handoff, with a second slot free
/// for a claim. Returns the thread, the release file, its pass counter and
/// its token once it has read the request for `/next/dagq`.
fn draining_for_a_handoff(
    db: &Path,
    repo: &Path,
) -> (thread::JoinHandle<Value>, PathBuf, Arc<AtomicU64>, String) {
    let release = db.with_extension("release");
    let verification = json!([crate::common::await_path(&release)]);
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE tasks SET verification_commands=?1 WHERE id=1",
            [verification.to_string()],
        )
        .unwrap();
    let backend = Arc::new(TestWorkspace::new(db, false, VALID_AGENT));
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "ready to land")]);
    let options = supervise_options(2, false);
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.to_owned(), repo.to_owned(), backend.clone());
        thread::spawn(move || supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options))
    };
    wait_until(db, crate::common::STEP_LIMIT, |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"integration_started")
    });
    let token = only_registration(db).token;
    let mut queue = SqliteQueue::open(db).unwrap();
    assert!(queue.request_handoff(&token, "/next/dagq").unwrap());
    await_passes(&passes, SOME_PASSES);
    // Draining: a task made ready now is not claimed.
    add_ready_task(&mut queue, "while draining", &[]);
    (supervisor, release, passes, token.into_string())
}

/// The supervisor execs `/replacement/dagq` and task 2, ready all along,
/// was never claimed.
fn execs_the_replacement_without_a_claim(db: &Path, supervisor: thread::JoinHandle<Value>) {
    let outcome = joined(supervisor, "the supervisor whose handoff was replaced");
    assert_eq!(outcome["outcome"], "handoff", "{outcome}");
    assert_eq!(outcome["binary"], "/replacement/dagq", "{outcome}");
    let mut queue = SqliteQueue::open(db).unwrap();
    assert!(queue.show(TaskId::new(2)).unwrap().runs.is_empty());
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Completed
    );
}

/// Task 1286: a request replaced by another binary while the supervisor
/// drains goes on draining for the new binary: no pass claims between, and
/// the exec is of the replacement.
#[test]
fn a_handoff_replaced_while_draining_execs_the_new_binary_without_a_claim() {
    let (_dir, repo, db) = fixture();
    let (supervisor, release, passes, token) = draining_for_a_handoff(&db, &repo);
    let token = LeaseToken::new(token);
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.request_handoff(&token, "/replacement/dagq").unwrap());
    await_passes(&passes, SOME_PASSES);
    assert!(queue.show(TaskId::new(2)).unwrap().runs.is_empty());
    std::fs::write(&release, "go").unwrap();
    execs_the_replacement_without_a_claim(&db, supervisor);
}

/// Task 1286: a request replaced just before the supervisor takes it (the
/// take finds another binary) is read again: the supervisor goes on
/// draining and execs the replacement on its next pass, with no claim.
#[test]
fn a_handoff_replaced_at_the_take_execs_the_new_binary_without_a_claim() {
    let (_dir, repo, db) = fixture();
    // The take of `/next/dagq` meets the replacement: the trigger swaps the
    // binary and skips the take's own update, so the take changes no row.
    Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER replace_at_take BEFORE UPDATE OF handoff_accepted ON supervisors
             WHEN NEW.handoff_accepted = 0 AND OLD.handoff_binary = '/next/dagq'
             BEGIN
                 UPDATE supervisors SET handoff_binary = '/replacement/dagq'
                 WHERE token = OLD.token;
                 SELECT RAISE(IGNORE);
             END;",
        )
        .unwrap();
    let (supervisor, release, _passes, token) = draining_for_a_handoff(&db, &repo);
    std::fs::write(&release, "go").unwrap();
    execs_the_replacement_without_a_claim(&db, supervisor);
    // The replacement was taken: no request is left.
    assert_eq!(
        SqliteQueue::open(&db)
            .unwrap()
            .handoff_request(&LeaseToken::new(token))
            .unwrap(),
        None
    );
}

/// Task 1277: a supervisor asked to hand off while a landing holds it, and
/// then asked to stop, records `supervisor_draining` once; `hand_off`
/// fails it as stopping at once instead of at its timeout and withdraws
/// the request, and the stop still wins: the drain lands the run and the
/// supervisor ends without exec'ing.
#[test]
fn a_handoff_does_not_wait_for_a_supervisor_that_drains_for_a_stop() {
    let (_dir, repo, db) = fixture();
    let release = db.with_extension("release");
    let verification = json!([crate::common::await_path(&release)]);
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE tasks SET verification_commands=?1 WHERE id=1",
            [verification.to_string()],
        )
        .unwrap();
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "ready to land")]);
    let options = supervise_options(1, false);
    let passes = options.passes.clone();
    let stop = options.stop.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options))
    };
    wait_until(&db, crate::common::STEP_LIMIT, |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"integration_started")
    });
    let registration = only_registration(&db);
    let token = registration.token.clone();
    // Far longer than the test may take: only the stop ends the wait.
    let timeout = Duration::from_secs(600);
    let handing = {
        let db = db.clone();
        thread::spawn(move || {
            let queue = SqliteQueue::open(&db).unwrap();
            let started = Instant::now();
            let handed = dagq::lifecycle::hand_off(
                &queue,
                &dagq::infrastructure::adapters::SystemProcesses,
                &dagq::infrastructure::clock::SystemClock,
                std::slice::from_ref(&registration),
                Path::new("/next/dagq"),
                "9.9.9-dev+next",
                timeout,
                Duration::from_millis(20),
            )
            .unwrap();
            (handed, started.elapsed())
        })
    };
    let mut queue = SqliteQueue::open(&db).unwrap();
    wait_until(&db, crate::common::STEP_LIMIT, |queue| {
        queue.handoff_request(&token).unwrap().is_some()
    });
    // It has read the request and drains for the landing.
    await_passes(&passes, SOME_PASSES);
    stop.store(true, Ordering::SeqCst);
    let (handed, waited) = joined(handing, "the handoff to a stopping supervisor");
    assert!(waited < timeout, "{waited:?}");
    let handed = handed.into_iter().next().unwrap();
    assert!(handed.stopping, "{handed:?}");
    assert_eq!(handed.now, None);
    let error = handed.error.as_deref().unwrap();
    assert!(
        error.contains(&format!(
            "supervisor {token} (pid {}) is stopping (a stop request wins over the handoff) and \
was not handed off to /next/dagq",
            std::process::id()
        )),
        "{error}"
    );
    assert!(!error.contains("still finishes"), "{error}");
    let report = handed.report();
    assert_eq!(report["stopping"], true, "{report}");
    assert_eq!(report["error"], error);
    // The request was withdrawn.
    assert_eq!(queue.handoff_request(&token).unwrap(), None);
    // Recorded once, however many passes the drain takes.
    await_passes(&passes, SOME_PASSES);
    std::fs::write(&release, "go").unwrap();
    let outcome = joined(supervisor, "the supervisor draining for the stop");
    assert_ne!(outcome["outcome"], "handoff", "{outcome}");
    let drained = queue
        .latest_events_of(EventKind::SupervisorDraining.as_str(), 10)
        .unwrap();
    assert_eq!(drained.len(), 1, "{drained:?}");
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].id().clone();
    let payload = &drained[0].payload;
    assert_eq!(payload["supervisor"], json!(token));
    assert_eq!(payload["pid"], std::process::id());
    assert_eq!(payload["reason"], "stop_requested");
    assert_eq!(payload["handoff_binary"], "/next/dagq");
    assert_eq!(payload["runs"], json!([run]));
    assert!(payload["build"].is_string(), "{payload}");
    // The stop won: the drain landed the run in progress.
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Completed
    );
}

/// [`await_passes`] of [`SOME_PASSES`] while task 1's landing must still
/// be in progress, failing at once with the run's reason when it is not: a
/// landing that ended (its verification failed on a broken quote, task
/// 1335) leaves no slot the handoff waits for, so the supervisor takes the
/// request and execs, and the passes stop.
fn await_passes_landing(passes: &AtomicU64, queue: &mut SqliteQueue) {
    let from = passes.load(Ordering::SeqCst);
    let target = from + SOME_PASSES + 1;
    let started = Instant::now();
    while passes.load(Ordering::SeqCst) < target {
        let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
        assert_eq!(
            run.status(),
            RunStatus::Integrating,
            "the landing the test holds ended: {:?}",
            run.last_error()
        );
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the supervisor made {} of the {SOME_PASSES} passes waited for in 60 seconds",
            passes.load(Ordering::SeqCst).saturating_sub(from + 1)
        );
        thread::sleep(Duration::from_millis(20));
    }
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
/// drives it to its receipt, one exit request and validation.
#[test]
fn a_handoff_leaves_the_session_running_and_the_next_process_drives_it_on() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, GATED_AGENT));
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

    assert!(
        !Path::new(run.run_dir().unwrap())
            .join("turns/exit")
            .exists()
    );

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
    assert_exit_sent(&backend, &run, 1);

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
        "await_message; await_file \"$EXIT.go\"; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
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
        "await_message; await_file \"$EXIT.go\"; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
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
        "await_message; await_file \"$EXIT.go\"; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
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
    // One exit request for the resumed session.
    let resumed = position(&kinds, "resume_started");
    assert_eq!(
        kinds[resumed..]
            .iter()
            .filter(|k| **k == "exit_requested")
            .count(),
        1,
        "{kinds:?}"
    );
    assert!(!kinds.contains(&"supervisor_handed_off"), "{kinds:?}");
    let requests = session_texts(&detail.runs[0])
        .iter()
        .filter(|text| text.contains("main is now"))
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
    // The session ends at the exit request its wrapper takes.
    let exit = dagq::domain::turn::exit_path(Path::new(run.run_dir().unwrap()));
    fs::create_dir_all(exit.parent().unwrap()).unwrap();
    fs::write(&exit, "").unwrap();
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
        let output = Command::new(git_executable().expect("git executable"))
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

/// The plan and goal reviews a handoff meets (task 1425): a verdict that
/// was written before the exec is applied, not thrown away, and only a job
/// still running is stopped and runs again in the next process.
mod review_verdicts {
    use crate::goal_review::{goal_done, goal_events};
    use crate::plan_review::{
        Fixture, PlanWorkspace, StubReviewer, add, events, fixture, options, status, submit,
        supervise_with,
    };
    use crate::runtime_support::{joined, wait_until};
    use dagq::{
        compose::HostMetricsSettings,
        domain::{
            GoalId, LeaseToken, Priority, ProposalId, TaskId, TaskStatus, host_metrics::HostSample,
        },
        infrastructure::sqlite::SqliteQueue,
        runtime::{self, SuperviseOptions},
    };
    use rusqlite::Connection;
    use serde_json::{Value, json};
    use std::{
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    /// Whether [`held_host`] keeps its sample from ending, which keeps the
    /// handoff waiting (it waits for a sample in progress).
    static HOLD_SAMPLE: AtomicBool = AtomicBool::new(true);

    fn held_host(now: i64) -> HostSample {
        let started = Instant::now();
        while HOLD_SAMPLE.load(Ordering::SeqCst) && started.elapsed() < crate::common::STEP_LIMIT {
            thread::sleep(Duration::from_millis(10));
        }
        HostSample::new(now)
    }

    /// A finished goal and a submitted proposal, each a review candidate.
    fn candidates(fx: &Fixture) -> (GoalId, TaskId, ProposalId) {
        let (goal, _) = goal_done(fx);
        let mut queue = SqliteQueue::open(&fx.db).unwrap();
        let task = add(
            &mut queue,
            "review this plan",
            &[TaskId::new(1)],
            Priority::Normal,
        );
        let proposal = submit(&mut queue, &[task], None);
        (goal, task, proposal)
    }

    /// A reviewer whose jobs wait for `gate`: each plan review (started
    /// first) passes, each goal review finds the goal achieved, `rounds`
    /// times.
    fn reviewer(gate: &Path, rounds: usize) -> Arc<StubReviewer> {
        let round = [
            json!({"verdict": "pass", "reasons": [], "summary": "sound", "actions": []}),
            json!({"verdict": "achieved", "criteria": [], "summary": "done"}),
        ];
        let verdicts: Vec<Value> = (0..rounds).flat_map(|_| round.clone()).collect();
        Arc::new(StubReviewer::new(&verdicts).gated(gate))
    }

    /// The supervisor on its own thread until it hands off.
    fn spawn(
        fx: &Fixture,
        reviewer: &Arc<StubReviewer>,
        options: SuperviseOptions,
    ) -> thread::JoinHandle<Value> {
        let (db, repo, claude) = (fx.db.clone(), fx.repo.clone(), fx.claude.clone());
        let reviewer = reviewer.clone();
        thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &PlanWorkspace::default(),
                &claude,
                &*reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
            .unwrap()
        })
    }

    fn both_started(queue: &mut SqliteQueue, goal: GoalId, task: TaskId) -> bool {
        !goal_events(queue, goal, "goal_review_started").is_empty()
            && !events(queue, task, "plan_review_started").is_empty()
    }

    fn request_handoff(db: &Path) -> LeaseToken {
        let queue = SqliteQueue::open(db).unwrap();
        let token = queue.supervisors().unwrap().remove(0).token;
        assert!(queue.request_handoff(&token, "/next/dagq").unwrap());
        token
    }

    /// The outcome and error of every row of `table`, in order.
    fn rows(db: &Path, table: &str) -> Vec<(Option<String>, Option<String>)> {
        Connection::open(db)
            .unwrap()
            .prepare(&format!("SELECT outcome, error FROM {table} ORDER BY id"))
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// The next process under `token`, once, then what each review table
    /// holds.
    fn continue_after(fx: &Fixture, reviewer: &StubReviewer, token: LeaseToken) {
        let mut next = options(0, Duration::from_secs(3600));
        next.handoff_token = Some(token);
        supervise_with(fx, &PlanWorkspace::default(), reviewer, &next);
    }

    /// Both verdicts applied once, and neither review started again.
    fn applied_once(fx: &Fixture, reviewer: &StubReviewer, goal: GoalId, task: TaskId) {
        let mut queue = SqliteQueue::open(&fx.db).unwrap();
        let finished = events(&mut queue, task, "plan_review_finished");
        assert_eq!(finished.len(), 1, "{finished:?}");
        assert_eq!(finished[0]["decision"], "pass");
        assert_eq!(status(&mut queue, task), TaskStatus::Ready);
        assert_eq!(
            goal_events(&mut queue, goal, "goal_review_finished").len(),
            1
        );
        for table in ["plan_reviews", "goal_reviews"] {
            let rows = rows(&fx.db, table);
            assert_eq!(rows.len(), 1, "{table}: {rows:?}");
            assert_ne!(
                rows[0].0.as_deref(),
                Some("interrupted"),
                "{table}: {rows:?}"
            );
        }
        assert_eq!(reviewer.prompts().len(), 2);
        assert_eq!(events(&mut queue, task, "plan_review_started").len(), 1);
        assert_eq!(
            goal_events(&mut queue, goal, "goal_review_started").len(),
            1
        );
    }

    /// A plan and a goal review that end while the handoff waits (here for
    /// a host sample in progress) are reaped and applied in the wait; none
    /// starts again there nor in the next process.
    #[test]
    fn reviews_that_end_while_a_handoff_waits_are_applied_in_the_wait() {
        let fx = fixture();
        let (goal, task, _) = candidates(&fx);
        let gate = fx.db.with_extension("gate");
        let reviewer = reviewer(&gate, 1);
        let mut first = options(0, Duration::from_secs(3600));
        first.once = false;
        first.host_metrics = Some(HostMetricsSettings {
            sample: held_host,
            disk: |_| None,
            ..HostMetricsSettings::new(Duration::from_secs(3600), 30)
        });
        let passes = first.passes.clone();
        let supervisor = spawn(&fx, &reviewer, first);
        wait_until(&fx.db, crate::common::STEP_LIMIT, |queue| {
            both_started(queue, goal, task)
        });
        let token = request_handoff(&fx.db);
        crate::runtime_support::await_passes(&passes, crate::runtime_support::SOME_PASSES);
        std::fs::write(&gate, "go").unwrap();
        wait_until(&fx.db, crate::common::STEP_LIMIT, |queue| {
            !events(queue, task, "plan_review_finished").is_empty()
                && !goal_events(queue, goal, "goal_review_finished").is_empty()
        });
        // Applied while the handoff still waits for the sample: the
        // request is taken only at the exec.
        assert!(!supervisor.is_finished());
        assert_eq!(
            SqliteQueue::open(&fx.db)
                .unwrap()
                .handoff_request(&token)
                .unwrap()
                .as_deref(),
            Some("/next/dagq")
        );
        HOLD_SAMPLE.store(false, Ordering::SeqCst);
        let outcome = joined(supervisor, "the supervisor handing off");
        assert_eq!(outcome["outcome"], "handoff", "{outcome}");
        continue_after(&fx, &reviewer, token);
        applied_once(&fx, &reviewer, goal, task);
    }

    /// A plan and a goal review that ended just before the exec (the
    /// supervisor sleeps between its passes) are applied by the exec's
    /// preparation, not stopped and run again by the next process.
    #[test]
    fn reviews_that_ended_before_the_exec_are_applied_not_run_again() {
        let fx = fixture();
        let (goal, task, _) = candidates(&fx);
        let gate = fx.db.with_extension("gate");
        let reviewer = reviewer(&gate, 1);
        let mut first = options(0, Duration::from_secs(3600));
        first.once = false;
        // Long enough for both jobs to end between two passes.
        first.tick = Duration::from_secs(5);
        let supervisor = spawn(&fx, &reviewer, first);
        wait_until(&fx.db, crate::common::STEP_LIMIT, |queue| {
            both_started(queue, goal, task)
        });
        let token = request_handoff(&fx.db);
        std::fs::write(&gate, "go").unwrap();
        let done = PathBuf::from(format!("{}.done", gate.display()));
        let started = Instant::now();
        // Each job appends a line once its verdict is printed, just before
        // it exits.
        while std::fs::read_to_string(&done).map_or(0, |text| text.lines().count()) < 2 {
            assert!(
                started.elapsed() < crate::common::STEP_LIMIT,
                "the jobs never ended"
            );
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(300));
        let outcome = joined(supervisor, "the supervisor handing off");
        assert_eq!(outcome["outcome"], "handoff", "{outcome}");
        continue_after(&fx, &reviewer, token);
        applied_once(&fx, &reviewer, goal, task);
    }

    /// A review still running at the exec is stopped and runs again in the
    /// next process, its first attempt interrupted.
    #[test]
    fn reviews_still_running_at_the_exec_are_stopped_and_run_again() {
        let fx = fixture();
        let (goal, task, _) = candidates(&fx);
        let gate = fx.db.with_extension("gate");
        let reviewer = reviewer(&gate, 2);
        let mut first = options(0, Duration::from_secs(3600));
        first.once = false;
        let supervisor = spawn(&fx, &reviewer, first);
        wait_until(&fx.db, crate::common::STEP_LIMIT, |queue| {
            both_started(queue, goal, task)
        });
        let token = request_handoff(&fx.db);
        let outcome = joined(supervisor, "the supervisor handing off");
        assert_eq!(outcome["outcome"], "handoff", "{outcome}");
        std::fs::write(&gate, "go").unwrap();
        continue_after(&fx, &reviewer, token);
        let mut queue = SqliteQueue::open(&fx.db).unwrap();
        for table in ["plan_reviews", "goal_reviews"] {
            let rows = rows(&fx.db, table);
            assert_eq!(rows.len(), 2, "{table}: {rows:?}");
            assert_eq!(
                rows[0].0.as_deref(),
                Some("interrupted"),
                "{table}: {rows:?}"
            );
            assert_eq!(
                rows[0].1.as_deref(),
                Some("stopped for the supervisor handoff")
            );
            assert_ne!(
                rows[1].0.as_deref(),
                Some("interrupted"),
                "{table}: {rows:?}"
            );
        }
        assert_eq!(reviewer.prompts().len(), 4);
        assert_eq!(status(&mut queue, task), TaskStatus::Ready);
        assert_eq!(
            goal_events(&mut queue, goal, "goal_review_finished").len(),
            1
        );
    }
}

/// Make the last `kind` event of `run` `seconds` older.
fn backdate_event(db: &Path, run: &TaskRun, kind: &str, seconds: i64) {
    let changed = Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE run_events SET created_at=strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?3 || ' seconds')
             WHERE id=(SELECT MAX(id) FROM run_events WHERE run_id=?1 AND kind=?2)",
            rusqlite::params![run.id(), kind, -seconds],
        )
        .unwrap();
    assert_eq!(changed, 1, "no {kind} of {} to backdate", run.id());
}

/// When the first `kind` event of `detail` whose payload `keep` takes was
/// recorded, in milliseconds.
fn recorded_millis(
    detail: &dagq::domain::TaskDetail,
    kind: &str,
    keep: impl Fn(&Value) -> bool,
) -> i64 {
    let event = detail
        .events
        .iter()
        .find(|e| e.kind == kind && keep(&e.payload))
        .unwrap_or_else(|| panic!("no {kind}: {:?}", event_kinds(detail)));
    dagq::domain::stats::timestamp_millis(&event.created_at).unwrap()
}

/// A resume whose exit was requested (twice the exit timeout ago) before
/// its supervisor handed off keeps that request: the next process asks for
/// no second exit and lets the session go at once, without waiting the
/// exit timeout again. Moved from the interactive
/// `a_resume_taken_over_after_a_handoff_times_its_exit_from_the_recorded_request`
/// (task 1437).
#[test]
fn a_resume_taken_over_after_a_handoff_keeps_its_recorded_exit_request() {
    resume_taken_over_after_its_exit(true);
}

/// The same for a resume adopted from a supervisor that died, rebuilt from
/// the run's events. Moved from the interactive
/// `an_adopted_resume_times_its_exit_from_the_recorded_request` (task 1437).
#[test]
fn an_adopted_resume_keeps_its_recorded_exit_request() {
    resume_taken_over_after_its_exit(false);
}

/// A resumed session asked to exit under the previous process, which
/// recorded `exit_requested` of the attempt 120 seconds ago and never saw
/// the session go (here the session works on, and no exit request was
/// written for it): taken over (after a `handoff`, or adopted) with an exit
/// timeout of 60 seconds, the resume writes no exit request and records no
/// second `exit_requested`, and ends `unresolved` with `exit_timed_out`
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
        "await_message; await_file \"$EXIT.go\"; resolve; receipt \"$(git rev-parse HEAD)\"",
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
            json!({"workspace_id": workspace, "resume_attempt": 1}),
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
            if handoff {
                supervise_after_handoff(&db, &repo, &backend, &token)
            } else {
                supervise(&db, &repo, &backend)
            }
        })
    };
    // Bounded well below the exit timeout: a timeout run from the takeover
    // does not end the resume within it.
    wait_until(&db, Duration::from_secs(30), |queue| {
        !payloads(&queue.show(TaskId::new(2)).unwrap(), "resume_finished").is_empty()
    });
    let detail = queue.show(TaskId::new(2)).unwrap();
    let kinds = event_kinds(&detail);
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished[0]["outcome"], "unresolved", "{kinds:?}");
    assert_eq!(finished[0]["exit_timed_out"], true, "{kinds:?}");
    let adopted = recorded_millis(&detail, "auto_repaired", |p| {
        p["repair"] == "resume_adopted" && p["conditions"]["handoff"] == handoff
    });
    let ended = recorded_millis(&detail, "resume_finished", |_| true);
    let waited = Duration::from_millis(u64::try_from(ended - adopted).unwrap());
    assert!(
        waited < exit_timeout,
        "the resume was let go {waited:?} after the takeover"
    );
    // No second exit request: none written, and only the test's recorded
    // during the resume.
    let exit = dagq::domain::turn::exit_path(Path::new(run.run_dir().unwrap()));
    assert!(!exit.exists(), "{}", exit.display());
    assert_eq!(
        kinds[position(&kinds, "resume_started")..]
            .iter()
            .filter(|k| **k == "exit_requested")
            .count(),
        1,
        "{kinds:?}"
    );

    // The session let go resolves the conflict and exits.
    let run_dir = run.run_dir().unwrap();
    fs::write(exit_request_path(run_dir).with_extension("go"), "").unwrap();
    fs::write(dagq::domain::turn::exit_path(Path::new(run_dir)), "").unwrap();
    let outcome = joined(next, "the supervisor taking the resume over").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let kinds = event_kinds(&queue.show(TaskId::new(2)).unwrap())
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(!kinds.iter().any(|k| k == "runtime_error"), "{kinds:?}");
}

/// Start the wrapper of `run`'s resumed session the way the test backend's
/// background start does, for a resume that backend started without a
/// wrapper (`resume_no_session`): as the process the backend started for
/// it.
fn start_resume_wrapper(
    backend: &TestWorkspace,
    run: &TaskRun,
) -> thread::JoinHandle<Result<Value>> {
    let claude = backend.claude_for(run, true).unwrap();
    let (provider, other) = headless_provider(run, claude.as_deref(), backend.codex.as_deref());
    let token: String = Connection::open(&backend.db)
        .unwrap()
        .query_row(
            "SELECT token FROM run_leases WHERE run_id=?1",
            [run.id()],
            |r| r.get(0),
        )
        .unwrap();
    // The process of the resume's handle, its last session.
    let handle = SqliteQueue::open(&backend.db)
        .unwrap()
        .run_events(run.id())
        .unwrap()
        .iter()
        .rev()
        .find(|e| e.kind == "workspace_created")
        .and_then(|e| e.payload["workspace_id"].as_str().map(str::to_owned))
        .expect("the resume's handle");
    let pid = backend
        .stands
        .lock()
        .unwrap()
        .iter()
        .find(|(id, _)| *id == handle)
        .expect("the resume's process")
        .1
        .pid;
    let (db, id) = (backend.db.clone(), run.id().clone());
    thread::spawn(move || {
        runtime::session_in_background_as(
            &db,
            &id,
            &LeaseToken::new(&token),
            &provider,
            Some(&other),
            &StubSpawner { db: db.clone() },
            true,
            pid,
            None,
        )
    })
}

/// How the next process finds a resume whose request was not sent yet.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Unsent {
    /// In `handoff.json` as the supervisor wrote it, with what the request
    /// took.
    Kept,
    /// In a `handoff.json` an older binary wrote, without what the request
    /// took, whose request (and `resume-1.txt`) is one written before the
    /// limits, past its whole limit.
    Old,
    /// Without `handoff.json`, adopted from the run's events and its
    /// `resume-1.txt`, which holds such a request.
    Adopted,
}

/// A handoff after the resumed session's workspace opened but before its
/// wrapper registered, so before the resolution request was sent: the
/// state carries the resume without a send, and the next process sends the
/// request once the wrapper registers, once, into the same resume, without
/// resuming the run again, and the run lands. Moved from the interactive
/// `a_handoff_before_the_resume_request_lets_the_next_supervisor_send_it`
/// (task 1437). The request records what it took as it was built
/// (ADR-t2072-1).
#[test]
fn a_handoff_before_the_resume_request_lets_the_next_supervisor_send_it_once() {
    handoff_before_the_resume_request(Unsent::Kept);
}

/// ADR-t2072-1: a request carried over by an older binary's
/// `handoff.json`, which says nothing of what it took, is measured anew
/// when it is sent; one written before the limits is cut to the whole
/// limit, keeping its start, and names `resume-1.txt`, which has it whole.
#[test]
fn a_resume_request_from_an_old_handoff_state_is_measured_and_cut_to_its_limit() {
    handoff_before_the_resume_request(Unsent::Old);
}

/// ADR-t2072-1: the same for a resume adopted from the run's events after
/// a handoff that left no `handoff.json`: its request, read from
/// `resume-1.txt`, is measured anew and cut when it is sent.
#[test]
fn a_resume_request_adopted_after_a_handoff_is_measured_and_cut_to_its_limit() {
    handoff_before_the_resume_request(Unsent::Adopted);
}

fn handoff_before_the_resume_request(unsent: Unsent) {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    // The resume starts no wrapper until the test starts one.
    backend.resume_no_session = true;
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"",
    );
    let backend = Arc::new(backend);
    let (outcome, token) = hand_off_when(&db, &repo, &backend, |queue| {
        payloads(&queue.show(TaskId::new(2)).unwrap(), "workspace_created")
            .iter()
            .any(|p| p["resume_attempt"] == 1)
    });
    assert_eq!(outcome["handed_over"], 1, "{outcome}");
    let kinds = event_kinds(
        &SqliteQueue::open(&db)
            .unwrap()
            .show(TaskId::new(2))
            .unwrap(),
    )
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    assert!(
        !kinds.iter().any(|k| k == "resume_request_sent"),
        "{kinds:?}"
    );
    let run_dir = Path::new(run.run_dir().unwrap());
    let snapshot = run_dir.join("handoff.json");
    let mut written: Value = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
    assert_eq!(written["phase"], "resume");
    assert_eq!(written["message_sent_at"], Value::Null, "{written}");
    assert_eq!(written["message_bytes"]["limit"], 36_000, "{written}");
    // A request written before the limits: the built one (whose start
    // the session reads its main from) and much more.
    let file = run_dir.join("resume-1.txt");
    let old = format!(
        "{}\n{}",
        written["message"].as_str().unwrap(),
        "旧".repeat(20_000)
    );
    match unsent {
        Unsent::Kept => {}
        Unsent::Old => {
            written["message"] = json!(old);
            written.as_object_mut().unwrap().remove("message_bytes");
            fs::write(&snapshot, written.to_string()).unwrap();
            fs::write(&file, &old).unwrap();
        }
        Unsent::Adopted => {
            fs::remove_file(&snapshot).unwrap();
            fs::write(&file, &old).unwrap();
        }
    }

    // An adoption needs the resumed session's wrapper alive, so it starts
    // and registers before the next process looks.
    let early = (unsent == Unsent::Adopted).then(|| {
        let wrapper = start_resume_wrapper(&backend, &run);
        wait_until(&db, crate::common::STEP_LIMIT, |queue| {
            queue
                .processes(run.id())
                .unwrap()
                .iter()
                .any(|p| p.role == "wrapper" && p.exited_at.is_none())
        });
        wrapper
    });
    let next = {
        let (db, repo, backend, token) = (db.clone(), repo.clone(), backend.clone(), token.clone());
        thread::spawn(move || supervise_after_handoff(&db, &repo, &backend, &token))
    };
    wait_until(&db, crate::common::STEP_LIMIT, |_| !snapshot.exists());
    let wrapper = early.unwrap_or_else(|| start_resume_wrapper(&backend, &run));
    // Well within the resume timeout, after which a request never sent
    // would end the resume too.
    wait_until(&db, Duration::from_secs(30), |queue| {
        !payloads(&queue.show(TaskId::new(2)).unwrap(), "resume_request_sent").is_empty()
    });
    let outcome = joined(next, "the supervisor after the handoff").unwrap();
    joined(wrapper, "the resumed session's wrapper to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    let kinds = event_kinds(&detail);
    assert_eq!(kinds.iter().filter(|k| **k == "resume_started").count(), 1);
    // The request went once, from the next process, into the same resume.
    let sent = payloads(&detail, "resume_request_sent");
    assert_eq!(sent.len(), 1, "{kinds:?}");
    assert_eq!(sent[0]["resume_attempt"], 1);
    assert_eq!(sent[0]["workspace_id"], written["workspace"]);
    let handed = position(&kinds, "supervisor_handed_off");
    assert!(
        handed < position(&kinds, "resume_request_sent"),
        "{kinds:?}"
    );
    let requests: Vec<&Value> = payloads(&detail, "turn_requested")
        .into_iter()
        .filter(|p| p["what"] == "resolution request")
        .collect();
    assert_eq!(requests.len(), 1, "{kinds:?}");
    let bytes = &requests[0]["prompt_bytes"];
    let taken: dagq::domain::turn::TurnRequest = serde_json::from_str(
        &fs::read_to_string(dagq::domain::turn::taken_path(
            run_dir,
            requests[0]["seq"].as_u64().unwrap(),
        ))
        .unwrap(),
    )
    .unwrap();
    if unsent == Unsent::Kept {
        // What the request took as it was built.
        assert_eq!(*bytes, written["message_bytes"], "{bytes}");
        assert_eq!(taken.prompt, written["message"].as_str().unwrap());
    } else {
        assert_eq!(bytes["limit"], 36_000, "{bytes}");
        assert_eq!(bytes["omitted"]["request"], 1, "{bytes}");
        assert_eq!(bytes["total"], taken.prompt.len(), "{bytes}");
        assert!(taken.prompt.len() <= 36_000, "{}", taken.prompt.len());
        let (kept, note) = taken.prompt.split_once("\n[… ").unwrap();
        assert!(old.starts_with(kept));
        assert_eq!(
            note,
            format!(
                "{} bytes left out by the prompt's limit; the whole request is in {}]",
                old.len() - kept.len(),
                file.display()
            )
        );
    }
    let adopted: Vec<&Value> = payloads(&detail, "auto_repaired")
        .into_iter()
        .filter(|p| p["repair"] == "resume_adopted")
        .collect();
    assert_eq!(adopted.len(), 1, "{kinds:?}");
    assert_eq!(adopted[0]["conditions"]["handoff"], true);
}

/// Point the live wrapper row of `run_id` at the process `stand_in`: the
/// session it stands for stays after its exit request until the test
/// records that process's exit, while the in-test wrapper, which ends at
/// the request, can no longer record its exit on the row.
fn stand_in_for_the_wrapper(db: &Path, run_id: &RunId, stand_in: u32) {
    let changed = Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE run_processes SET pid=?2 WHERE run_id=?1 AND role='wrapper' AND exited_at IS NULL",
            rusqlite::params![run_id, stand_in],
        )
        .unwrap();
    assert_eq!(changed, 1, "no live wrapper of {run_id}");
}

/// A run rejected by validation waits for its session to exit; a handoff in
/// that wait carries the recorded exit request over in `handoff.json`, so
/// the next process writes no second exit request and lets the run rest
/// once the session exits. The session that holds its exit back is a
/// stand-in process the wrapper row names from before the request on.
/// Moved from the interactive
/// `a_handoff_while_a_rejected_run_waits_for_its_exit_sends_no_second_exit`
/// (task 1437).
#[test]
fn a_handoff_while_a_rejected_run_waits_for_its_exit_requests_no_second_exit() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        "commit work; printf 'scratch\\n' > untracked.txt; : > \"$RUN_DIR/turn-started\"; \
         await_file \"$EXIT.go\"; receipt \"$(git rev-parse HEAD)\"",
    ));
    let mut stand_in = crate::common::KillOnDrop::new(sleeper(), "the wrapper's stand-in");
    let stand_in_pid = stand_in.child().id();
    let mut taken = false;
    let (outcome, token) = hand_off_when(&db, &repo, &backend, |queue| {
        let detail = queue.show(TaskId::new(1)).unwrap();
        let Some(run) = detail.runs.first() else {
            return false;
        };
        // Once the turn works, the stand-in takes the wrapper's row and the
        // turn writes its receipt.
        if !taken {
            // A run just claimed has no run directory yet.
            let Some(run_dir) = run.run_dir() else {
                return false;
            };
            if !Path::new(run_dir).join("turn-started").exists() {
                return false;
            }
            stand_in_for_the_wrapper(&db, run.id(), stand_in_pid);
            fs::write(exit_request_path(run_dir).with_extension("go"), "").unwrap();
            taken = true;
            return false;
        }
        event_kinds(&detail).contains(&"exit_requested")
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
    // The in-test wrapper ended at the request; its exit request is
    // removed, so a second one shows.
    let wrapper = backend.sessions.lock().unwrap()[0].1.worker.take().unwrap();
    let _ = joined(wrapper, "the in-test wrapper to end at its exit request");
    let exit = dagq::domain::turn::exit_path(Path::new(run.run_dir().unwrap()));
    fs::remove_file(&exit).unwrap();

    let options = supervise_options(4, true);
    let passes = options.passes.clone();
    let next = {
        let (db, repo, backend, token) = (db.clone(), repo.clone(), backend.clone(), token.clone());
        thread::spawn(move || supervise_after_handoff_with(&db, &repo, &backend, &token, options))
    };
    wait_until(&db, Duration::from_secs(30), |_| !snapshot.exists());
    // Several passes of the next process with the session still there.
    await_passes(&passes, SOME_PASSES);
    assert!(!exit.exists());
    let kinds = event_kinds(&queue.show(TaskId::new(1)).unwrap())
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(
        kinds.iter().filter(|k| *k == "exit_requested").count(),
        1,
        "{kinds:?}"
    );
    // The session exits.
    queue.wrapper_exited(run.id(), stand_in_pid, 0).unwrap();
    let outcome = joined(next, "the supervisor after the handoff").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "failed");
    assert!(!exit.exists());
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    let kinds = event_kinds(&queue.show(TaskId::new(1)).unwrap())
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(
        kinds.iter().filter(|k| *k == "exit_requested").count(),
        1,
        "{kinds:?}"
    );
    assert!(!kinds.iter().any(|k| k == "run_adopted"), "{kinds:?}");
}
