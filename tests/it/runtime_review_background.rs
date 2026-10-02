//! Runtime tests: background work a session runs holds its exit, revise
//! and resume until it ends.
use crate::common;
use crate::runtime_support;

use dagq::domain::stats::timestamp_millis;
use runtime_support::*;

/// Waits until the run's idle marker shows background work running.
fn wait_for_background(run: &TaskRun) {
    let marker = run.idle_marker_path().unwrap();
    let started = Instant::now();
    while !fs::read_to_string(&marker).is_ok_and(|text| text.contains("\"running\"")) {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "no background marker"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Writes the Stop hook's marker for the run as the session would, with
/// `background_tasks` as given.
fn write_idle_marker(run: &TaskRun, background_tasks: Value) {
    let marker = run.idle_marker_path().unwrap();
    let hook = json!({
        "session_id": run.id(),
        "hook_event_name": "Stop",
        "stop_hook_active": false,
        "background_tasks": background_tasks,
    });
    let tmp = marker.with_extension("tmp");
    fs::write(&tmp, hook.to_string()).unwrap();
    fs::rename(&tmp, &marker).unwrap();
}

/// The session goes idle after its receipt with background work still
/// running (task 147): the supervisor does not take it for idle, so neither
/// validation nor `/exit` starts, until the work ended and the Stop hook
/// wrote a marker with empty `background_tasks`.
#[test]
fn background_work_holds_the_first_session_until_it_ends() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle_bg; \
         while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; idle_bg_done; await_exit",
    ));
    let options = supervise_options(4, true);
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"receipt_observed")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    wait_for_background(&run);
    // Passes after it: what the supervisor did not do by then it holds.
    await_passes(&passes, SOME_PASSES);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Running);
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"session_idle_observed"), "{kinds:?}");
    assert!(!kinds.contains(&"exit_requested"), "{kinds:?}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "session_idle_observed") < position(&kinds, "exit_requested"));
    assert!(!kinds.contains(&"exit_request_timed_out"));
}

/// A marker whose background work is over (`completed`) or that names none
/// is idle as before.
#[test]
fn a_marker_without_running_background_work_is_idle() {
    for tasks in [
        json!([]),
        json!([{"id": "b1", "type": "shell", "status": "completed"}]),
    ] {
        let script = format!(
            "commit work; receipt \"$(git rev-parse HEAD)\"; \
             printf '%s' '{}' > \"$IDLE.tmp\"; mv \"$IDLE.tmp\" \"$IDLE\"; await_exit",
            json!({"session_id": "s", "hook_event_name": "Stop", "background_tasks": tasks})
        );
        let (_dir, repo, db) = fixture();
        let backend = TestWorkspace::new(&db, false, &script);
        let outcome = supervise(&db, &repo, &backend).unwrap();
        backend.join();
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
        assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    }
}

/// After the review, the `/exit` waits while the session's marker shows
/// background work running (the session took a turn up again after its
/// receipt), and goes once the work ended.
#[test]
fn background_work_holds_the_exit_after_the_review() {
    let (dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let gate = dir.path().join("review-gate");
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let reviewer = Arc::new(TestReviewer::new(&[format!(
        "while [ ! -f {} ]; do sleep 0.05; done; {}",
        shell_join(&[gate.to_string_lossy().into_owned()]),
        verdict("pass", &[], "meets the acceptance")
    )]));
    let options = supervise_options(4, true);
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"review_started")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    write_idle_marker(
        &run,
        json!([{"id": "b1", "type": "shell", "status": "running", "command": "cargo test"}]),
    );
    fs::write(&gate, "").unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"review_finished")
    });
    // Passes after it: what the supervisor did not do by then it holds.
    await_passes(&passes, SOME_PASSES);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"exit_requested"), "{kinds:?}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);

    write_idle_marker(&run, json!([]));
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_landed(
        &repo,
        &queue.show(TaskId::new(1)).unwrap().runs[0],
        "test task",
        &base,
    );
}

/// A resumed session that rewrote its receipt and stopped with background
/// work running is not asked to exit until the work ended.
#[test]
fn background_work_holds_the_resumed_session_until_it_ends() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    let sent_before = backend.exits_sent.load(Ordering::SeqCst);
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle_bg; \
         while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; idle_bg_done; await_exit",
    );
    let backend = Arc::new(backend);
    let options = supervise_options(4, true);
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_for_background(&run);
    // Passes after it: what the supervisor did not do by then it holds.
    await_passes(&passes, SOME_PASSES);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert!(payloads(&detail, "resume_finished").is_empty());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), sent_before);

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), sent_before + 1);
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    assert_eq!(
        payloads(&detail, "resume_finished")[0]["outcome"],
        "resolved"
    );
}

/// A session revising its work that stops with background work running is
/// not taken for done: the revise waits until the work ended.
#[test]
fn background_work_holds_the_revise_until_it_ends() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; \
         while [ ! -f \"$MESSAGE\" ]; do sleep 0.05; done; rm \"$MESSAGE\"; \
         printf 'fix\\n' >> change.txt; git commit -q -am fix; \
         receipt \"$(git rev-parse HEAD)\"; idle_bg; \
         while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; idle_bg_done; await_exit",
    ));
    let reviewer = Arc::new(TestReviewer::new(&[
        verdict("revise", &["add a line"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]));
    let options = supervise_options(4, true);
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options))
    };
    // The revise follows the claim, worker session, receipt, validation and
    // review job, so reaching it grows with the host's load without testing
    // a runtime limit. Bound the wait by the test's limit, and also stop if
    // the supervisor returns without requesting a revise.
    wait_until(&db, common::STEP_LIMIT, |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"revise_requested")
            || supervisor.is_finished()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(
        kinds.contains(&"revise_requested"),
        "supervisor returned without requesting a revise: {kinds:?}"
    );
    let run = detail.runs[0].clone();
    wait_for_background(&run);
    // Passes after it: what the supervisor did not do by then it holds.
    await_passes(&passes, SOME_PASSES);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"revise_finished"), "{kinds:?}");
    assert!(!kinds.contains(&"exit_requested"), "{kinds:?}");
    assert!(queue.asks(AskQuery::default()).unwrap().is_empty());

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
}

/// A revise the session will not finish (it goes idle without rewriting the
/// receipt) ends in `/exit`; a session that holds that `/exit` back past the
/// exit timeout raises one `stuck_exit` ask, closed by the runtime once the
/// session exits, and the run then goes on to its `approve_landing` ask.
#[test]
fn a_revise_session_that_holds_exit_back_raises_a_stuck_exit_ask() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "commit work; receipt \"$(git rev-parse HEAD)\"; idle; \
             while [ ! -f \"$MESSAGE\" ]; do sleep 0.05; done; idle; {HOLD}"
        ),
    );
    backend.exit_timeout = Duration::from_millis(500);
    let backend = Arc::new(backend);
    let reviewer = Arc::new(TestReviewer::new(&[verdict(
        "revise",
        &["add a line"],
        "one gap",
    )]));
    let options = supervise_options(4, true);
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options))
    };
    // The ask comes after the claim, the review job, the revise, the
    // /exit's timeout and the recovery job, each with processes of its
    // own, so when it comes grows with the host's load (about 4s on an
    // idle host, 20-26s at a load of 120), while none of the runtime's
    // limits in between is at stake. The wait ends at the ask, or at a
    // supervisor that returned without one; the test's limit bounds it.
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty() || supervisor.is_finished()
    });
    // Passes after it: no second ask follows. A supervisor that returned
    // makes no more passes, and the asks below say what it left.
    if !supervisor.is_finished() {
        await_passes(&passes, SOME_PASSES);
    }
    let mut queue = SqliteQueue::open(&db).unwrap();
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].clone();
    assert_eq!(ask.kind, AskKind::StuckExit);
    assert!(
        ask.question
            .contains("opens an approve_landing ask for the person once the session exits"),
        "{}",
        ask.question
    );
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "revise_requested") < position(&kinds, "exit_requested"));
    assert!(!kinds.contains(&"revise_finished"), "{kinds:?}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    // The ask follows the /exit's timeout (500ms) and its recovery job, not
    // the resume timeout: measured from the /exit, not from the start.
    let at = |kind: &str, ask_kind: Option<&str>| {
        let event = detail
            .events
            .iter()
            .find(|e| e.kind == kind && ask_kind.is_none_or(|k| e.payload["kind"] == k))
            .unwrap();
        timestamp_millis(&event.created_at).unwrap()
    };
    let asked = at("ask_opened", Some("stuck_exit")) - at("exit_requested", None);
    let resume_limit = i64::try_from(backend.resume_timeout.as_millis()).unwrap();
    assert!(
        asked < resume_limit,
        "the stuck_exit ask came {asked}ms after the /exit"
    );

    release_held_session(run.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let closed = queue.read_ask(ask.id).unwrap();
    assert!(closed.closed_at.is_some());
    let open = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].kind, AskKind::ApproveLanding);
}

/// Background work that never ends does not hold the run forever: past the
/// resume timeout from the receipt the run goes on to validation (its
/// `session_idle_observed` saying the work still ran), and the `/exit` then
/// goes without waiting the resume timeout again for the same work (task
/// 242), so that a dialog becomes a `stuck_exit` ask about one resume
/// timeout after the receipt, not two.
#[test]
fn background_work_that_never_ends_is_waited_for_up_to_the_resume_timeout() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle_bg; await_exit",
    );
    let resume_timeout = Duration::from_secs(3);
    backend.resume_timeout = resume_timeout;
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let idle = payloads(&detail, "session_idle_observed");
    assert_eq!(idle.len(), 1);
    assert_eq!(idle[0]["background_running"], true);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "session_idle_observed") < position(&kinds, "exit_requested"));
    let at = |kind: &str| {
        let event = detail.events.iter().find(|e| e.kind == kind).unwrap();
        timestamp_millis(&event.created_at).unwrap()
    };
    let limit = i64::try_from(resume_timeout.as_millis()).unwrap();
    let before = at("session_idle_observed") - at("receipt_observed");
    assert!(before >= limit, "went on {before}ms after the receipt");
    // The /exit's wait on background work begins once the review is over
    // (the session stays open through validation and review, ADR-0027
    // decision 1), so its absence shows from there: validation and the
    // review take their own time, which load stretches past the limit.
    let exit = position(&kinds, "exit_requested");
    let reviewed = detail.events[..exit]
        .iter()
        .rev()
        .find(|e| e.kind == "session_closed" && e.payload["kind"] == "review")
        .expect("the review's session closed before the /exit");
    let after = at("exit_requested") - timestamp_millis(&reviewed.created_at).unwrap();
    assert!(
        after < limit,
        "the /exit waited {after}ms after the review for the work already waited for"
    );
}
