//! Runtime tests: the retries of a `/exit` the session held back past its
//! timeout (ADR-0047 decision 25), what each one sends for the screen it
//! reads, the close of a landing run's workspace once they are used up,
//! the `stuck_exit` path of any other run, and an adopter that carries the
//! retries on.
use crate::common;
use crate::runtime_support;

use dagq::domain::{EventKind, exit::ExitConfig};
use runtime_support::*;

/// Ignores the first `/exit` (its request file is taken away, as a dialog
/// that ate it would) and exits on the next one, or when its workspace is
/// closed.
const IGNORES_FIRST_EXIT: &str = "commit work; receipt \"$(git rev-parse HEAD)\"; idle; \
     while [ ! -f \"$EXIT\" ]; do sleep 0.05; done; rm \"$EXIT\"; await_exit";

/// An unknown dialog: nothing is sent over it.
const DIALOG_SCREEN: &str = "\
 Auto mode is available

 ❯ 1. Yes, turn on auto mode
   2. No, keep asking

 Esc to cancel
";

/// The exit timeout of the tests that show a screen with [`show_after_exit`]:
/// its 300ms and a loaded host's poll must end well before the first retry
/// reads the screen, which a shorter timeout would not leave room for.
const SHOW_AFTER_EXIT_TIMEOUT: Duration = Duration::from_secs(1);

/// Supervisor options that retry a held `/exit` `retries` times, `interval`
/// apart.
fn retrying(retries: usize, interval: Duration) -> SuperviseOptions {
    SuperviseOptions {
        exit: Some(ExitConfig {
            retries,
            intervals: vec![interval],
        }),
        ..supervise_options(4, true)
    }
}

fn reviewed_with(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    reviewer: &TestReviewer,
    options: &SuperviseOptions,
) -> Value {
    let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
    let outcome = runtime::supervise_with_reviewer(
        db,
        repo,
        backend,
        &claude_stub(db),
        reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        options,
    )
    .unwrap();
    backend.join();
    outcome
}

/// Wait for the `/exit` to be requested, then, once its own submit has
/// read the screen (well within the exit timeout), show `screen`. The tests
/// that use it keep [`SHOW_AFTER_EXIT_TIMEOUT`]: the first retry has to
/// read `screen`, 300ms after the `/exit` and the test's own poll.
fn show_after_exit(db: &Path, backend: &TestWorkspace, screen: &str) {
    wait_until(db, common::STEP_LIMIT, |queue| {
        !payloads(&queue.show(TaskId::new(1)).unwrap(), "exit_requested").is_empty()
    });
    thread::sleep(Duration::from_millis(300));
    *backend.screen.lock().unwrap() = screen.into();
}

/// A session that held its `/exit` back past the timeout and shows an
/// empty input box gets `/exit` again; it exits on that retry, which is
/// recorded as `exit_retried` and repaired as `auto_repaired` (`repair:
/// exit_retry`), counted by `stats`, and the run lands without an ask.
#[test]
fn a_held_exit_is_typed_again_into_a_ready_input_box() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IGNORES_FIRST_EXIT);
    // Kept at a second: the session has to take the first /exit away (a
    // shell loop polling every 50ms) before the first retry, due at the
    // timeout, types it again; a loaded host can stall that loop.
    backend.exit_timeout = Duration::from_secs(1);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = reviewed_with(
        &db,
        &repo,
        &backend,
        &reviewer,
        &retrying(3, Duration::from_secs(5)),
    );
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 2);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let retried = payloads(&detail, "exit_retried");
    assert_eq!(retried.len(), 1, "{retried:?}");
    assert_eq!(retried[0]["attempt"], 1);
    assert_eq!(retried[0]["cause"], "exit_timeout");
    assert_eq!(retried[0]["screen"], "input_ready");
    assert_eq!(retried[0]["send"], "exit");
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "exit_request_timed_out") < position(&kinds, "exit_retried"));
    // Recorded before the /exit it sends, which the session exits on.
    assert!(position(&kinds, "exit_retried") < position(&kinds, "session_exited"));
    let repaired = payloads(&detail, "auto_repaired");
    let retry: Vec<_> = repaired
        .iter()
        .filter(|p| p["repair"] == "exit_retry")
        .collect();
    assert_eq!(retry.len(), 1, "{repaired:?}");
    assert_eq!(retry[0]["layer"], "runtime");
    assert_eq!(
        retry[0]["conditions"],
        json!({"attempts": 1, "cause": "exit_timeout", "screen": "input_ready"})
    );
    assert!(!repaired.iter().any(|p| p["repair"] == "exit_forced_close"));
    assert!(queue.asks(AskQuery::default()).unwrap().is_empty());
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(
        stats["auto_repairs"]["by_layer"]["runtime"]["by_repair"]["exit_retry"], 1,
        "{stats}"
    );
}

/// A dialog on the screen gets nothing from the retries (neither `/exit`
/// nor Enter). Once they are used up, the run that lands, whose receipt
/// still holds against its clean worktree, has its workspace closed and
/// lands, recorded as `exit_forced_close` with `cause: exit_timeout`.
#[test]
fn used_up_retries_over_a_dialog_close_the_workspace_of_a_landing_run() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IGNORES_FIRST_EXIT);
    backend.exit_timeout = SHOW_AFTER_EXIT_TIMEOUT;
    backend.close_ends_session = true;
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
            reviewed_with(
                &db,
                &repo,
                &backend,
                &reviewer,
                &retrying(2, Duration::from_millis(300)),
            )
        })
    };
    show_after_exit(&db, &backend, DIALOG_SCREEN);
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_eq!(backend.enters.load(Ordering::SeqCst), 0);
    assert!(backend.keys.lock().unwrap().is_empty());
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let retried = payloads(&detail, "exit_retried");
    assert_eq!(retried.len(), 2, "{retried:?}");
    for (n, retry) in retried.iter().enumerate() {
        assert_eq!(retry["attempt"], n + 1);
        assert_eq!(retry["screen"], "dialog");
        assert_eq!(retry["send"], "nothing");
    }
    let repaired = payloads(&detail, "auto_repaired");
    let forced: Vec<_> = repaired
        .iter()
        .filter(|p| p["repair"] == "exit_forced_close")
        .collect();
    assert_eq!(forced.len(), 1, "{repaired:?}");
    assert_eq!(forced[0]["conditions"]["cause"], "exit_timeout");
    assert_eq!(forced[0]["conditions"]["attempts"], 2);
    assert_eq!(forced[0]["conditions"]["exit_reached"], true);
    assert!(!repaired.iter().any(|p| p["repair"] == "exit_retry"));
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "exit_retried") < position(&kinds, "workspace_closed"));
    assert!(!kinds.contains(&"recovery_requested"), "{kinds:?}");
    assert_eq!(backend.closed.lock().unwrap().len(), 1);
}

/// A `/exit` left in the input box gets Enter alone on each retry. Used
/// up, a run that does not land goes on to its `stuck_exit` recovery job
/// and ask (the test stand-in escalates), keeping its lease; no workspace
/// is closed.
#[test]
fn used_up_retries_of_a_run_that_does_not_land_go_to_the_stuck_exit_ask() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, HELD_AGENT);
    backend.exit_timeout = SHOW_AFTER_EXIT_TIMEOUT;
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            supervise_with(
                &db,
                &repo,
                &backend,
                &retrying(3, Duration::from_millis(200)),
            )
        })
    };
    show_after_exit(&db, &backend, &pending_screen("/exit"));
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .any(|ask| ask.kind == AskKind::StuckExit)
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let retried = payloads(&detail, "exit_retried");
    assert_eq!(retried.len(), 3, "{retried:?}");
    for retry in &retried {
        assert_eq!(retry["screen"], "input_pending");
        assert_eq!(retry["send"], "enter");
    }
    assert!(backend.enters.load(Ordering::SeqCst) >= 3);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "exit_retried") < position(&kinds, "recovery_requested"));
    assert!(
        !payloads(&detail, "auto_repaired")
            .iter()
            .any(|p| p["repair"] == "exit_forced_close" || p["repair"] == "exit_retry")
    );
    let run = detail.runs[0].clone();
    assert!(queue.run_lease(run.id()).unwrap().is_some());
    release_held_session(run.run_dir().unwrap());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(backend.closed.lock().unwrap().len() <= 1);
    // No more retries after the ask.
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "exit_retried").len(), 3);
}

/// A supervisor that adopts a run whose `/exit` timed out and was retried
/// once under the previous one goes on from the second retry: the first is
/// not made again, and the session exiting on the second is repaired with
/// its two attempts.
#[test]
fn an_adopter_carries_the_retries_of_the_exit_on() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_timeout = Duration::from_millis(500);
    let backend = Arc::new(backend);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let mut queue = SqliteQueue::open(&db).unwrap();
    for (kind, payload) in [
        (
            "exit_requested",
            json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 1}),
        ),
        (
            "exit_request_timed_out",
            json!({"code": "exit_timeout", "workspace_id": WORKSPACE_ID, "timeout_secs": 1}),
        ),
        (
            "exit_retried",
            json!({"attempt": 1, "cause": "exit_timeout", "screen": "not_ready", "send": "nothing"}),
        ),
    ] {
        queue
            .record_runtime_event(run.id(), EventKind::from_name(kind).unwrap(), payload)
            .unwrap();
    }
    age_lease(&db, &run, 31);
    let outcome = {
        let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
        supervise_with(
            &db,
            &repo,
            &backend,
            &retrying(2, Duration::from_millis(300)),
        )
        .unwrap()
    };
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(!adoption_events(&detail).is_empty());
    let retried = payloads(&detail, "exit_retried");
    assert_eq!(retried.len(), 2, "{retried:?}");
    assert_eq!(retried[1]["attempt"], 2);
    assert_eq!(retried[1]["screen"], "input_ready");
    assert_eq!(retried[1]["send"], "exit");
    let retry: Vec<_> = payloads(&detail, "auto_repaired")
        .into_iter()
        .filter(|p| p["repair"] == "exit_retry")
        .collect();
    assert_eq!(retry.len(), 1);
    assert_eq!(retry[0]["conditions"]["attempts"], 2);
    // No stuck_exit ask (the other is its stand-in review's).
    assert!(
        !queue
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .any(|ask| ask.kind == AskKind::StuckExit)
    );
}

/// Record `events` (kind and payload) on `run`, as a supervisor that died
/// had.
fn record(db: &Path, run: &TaskRun, events: &[(&str, Value)]) {
    let queue = SqliteQueue::open(db).unwrap();
    for (kind, payload) in events {
        queue
            .record_runtime_event(
                run.id(),
                EventKind::from_name(kind).unwrap(),
                payload.clone(),
            )
            .unwrap();
    }
}

fn exit_requested() -> (&'static str, Value) {
    (
        "exit_requested",
        json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 1}),
    )
}

fn exit_timed_out() -> (&'static str, Value) {
    (
        "exit_request_timed_out",
        json!({"code": "exit_timeout", "workspace_id": WORKSPACE_ID, "timeout_secs": 1}),
    )
}

fn retried(attempt: usize) -> (&'static str, Value) {
    (
        "exit_retried",
        json!({"attempt": attempt, "cause": "exit_timeout", "screen": "dialog", "send": "nothing"}),
    )
}

/// The running session's `/exit` (`SessionWatch`, here of a run adopted
/// after its `/exit` was requested and before it timed out, which is never
/// sent twice): past the exit timeout it records `exit_request_timed_out`
/// itself and retries, and the session exits on the first retry.
#[test]
fn the_running_sessions_exit_is_retried_after_its_timeout() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_timeout = Duration::from_millis(500);
    let backend = Arc::new(backend);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    record(&db, &run, &[exit_requested()]);
    age_lease(&db, &run, 31);
    let outcome = {
        let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
        supervise_with(&db, &repo, &backend, &retrying(3, Duration::from_secs(5))).unwrap()
    };
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "run_adopted") < position(&kinds, "exit_request_timed_out"));
    assert!(position(&kinds, "exit_request_timed_out") < position(&kinds, "exit_retried"));
    assert!(position(&kinds, "exit_retried") < position(&kinds, "session_exited"));
    let retried = payloads(&detail, "exit_retried");
    assert_eq!(retried.len(), 1, "{retried:?}");
    assert_eq!(retried[0]["attempt"], 1);
    assert_eq!(retried[0]["cause"], "exit_timeout");
    assert_eq!(retried[0]["screen"], "input_ready");
    assert_eq!(retried[0]["send"], "exit");
    let repaired: Vec<_> = payloads(&detail, "auto_repaired")
        .into_iter()
        .filter(|p| p["repair"] == "exit_retry")
        .collect();
    assert_eq!(repaired.len(), 1);
    assert_eq!(repaired[0]["conditions"]["attempts"], 1);
    assert!(!kinds.contains(&"recovery_requested"), "{kinds:?}");
}

/// A `/exit` that never reached the session of a run that does not land
/// on its own (its review failed) is retried (`cause: backend_timeout`)
/// instead of going to the `stuck_exit` path at once; the retry's `/exit`
/// gets there and the session exits, repaired as `exit_retry`.
#[test]
fn an_unsent_exit_of_a_run_that_cannot_land_is_retried() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    // Every attempt of the first /exit times out without reaching it.
    backend.exit_unsent.store(3, Ordering::SeqCst);
    let outcome = {
        let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
        supervise_with(&db, &repo, &backend, &retrying(3, Duration::from_secs(5))).unwrap()
    };
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 4);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "exit_unsent")[0]["action"], "recover");
    assert_eq!(
        payloads(&detail, "exit_request_timed_out")[0]["unsent"],
        true
    );
    let retried = payloads(&detail, "exit_retried");
    assert_eq!(retried.len(), 1, "{retried:?}");
    assert_eq!(retried[0]["cause"], "backend_timeout");
    assert_eq!(retried[0]["screen"], "input_ready");
    assert_eq!(retried[0]["send"], "exit");
    let repaired: Vec<_> = payloads(&detail, "auto_repaired")
        .into_iter()
        .filter(|p| p["repair"] == "exit_retry")
        .collect();
    assert_eq!(repaired.len(), 1);
    assert_eq!(repaired[0]["conditions"]["cause"], "backend_timeout");
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "exit_retried") < position(&kinds, "session_exited"));
    assert!(!kinds.contains(&"recovery_requested"), "{kinds:?}");
    assert!(
        !queue
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .any(|ask| ask.kind == AskKind::StuckExit)
    );
}

/// An unsent `/exit` whose retries never get there either: used up, the
/// run that does not land goes to the `stuck_exit` recovery job and ask.
#[test]
fn an_unsent_exit_whose_retries_never_get_there_goes_to_the_stuck_exit_ask() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_unsent.store(usize::MAX, Ordering::SeqCst);
    backend.registration_timeout = common::STEP_LIMIT;
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            supervise_with(
                &db,
                &repo,
                &backend,
                &retrying(2, Duration::from_millis(200)),
            )
        })
    };
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .any(|ask| ask.kind == AskKind::StuckExit)
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let retried = payloads(&detail, "exit_retried");
    assert_eq!(retried.len(), 2, "{retried:?}");
    for retry in &retried {
        assert_eq!(retry["cause"], "backend_timeout");
        assert_eq!(retry["send"], "exit");
    }
    // The first /exit and each retry's, all attempts timed out.
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 9);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "exit_retried") < position(&kinds, "recovery_requested"));
    assert!(payloads(&detail, "auto_repaired").is_empty());
    let run = detail.runs[0].clone();
    assert!(queue.run_lease(run.id()).unwrap().is_some());
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(backend.closed().len() <= 1);
}

/// A landing run whose retries over a dialog are used up is not closed
/// when `hold` (run before the retries end) breaks a condition of decision
/// 25: it goes to the `stuck_exit` recovery job and ask instead, its
/// workspace open; `held` is undone and the session let go at the end.
fn used_up_retries_do_not_close_when(
    hold: impl FnOnce(&Path, &TaskRun),
    held: impl FnOnce(&Path, &TaskRun),
) {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IGNORES_FIRST_EXIT);
    backend.exit_timeout = SHOW_AFTER_EXIT_TIMEOUT;
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
            reviewed_with(
                &db,
                &repo,
                &backend,
                &reviewer,
                &retrying(2, Duration::from_millis(300)),
            )
        })
    };
    show_after_exit(&db, &backend, DIALOG_SCREEN);
    let run = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();
    hold(&db, &run);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .any(|ask| ask.kind == AskKind::StuckExit)
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "exit_retried").len(), 2);
    assert!(
        !payloads(&detail, "auto_repaired")
            .iter()
            .any(|p| p["repair"] == "exit_forced_close")
    );
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "exit_retried") < position(&kinds, "recovery_requested"));
    assert!(!kinds.contains(&"workspace_closed"), "{kinds:?}");
    assert!(backend.closed().is_empty());
    assert!(queue.run_lease(run.id()).unwrap().is_some());
    held(&db, &run);
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    joined(supervisor, "the supervisor thread to return");
}

#[test]
fn used_up_retries_do_not_close_a_run_with_a_rebase_in_progress() {
    let marker = |run: &TaskRun| {
        let worktree = Path::new(run.worktree_path().unwrap());
        worktree.join(git_out(
            worktree,
            &["rev-parse", "--git-path", "rebase-merge"],
        ))
    };
    used_up_retries_do_not_close_when(
        |_, run| fs::create_dir_all(marker(run)).unwrap(),
        |_, run| fs::remove_dir_all(marker(run)).unwrap(),
    );
}

#[test]
fn used_up_retries_do_not_close_a_run_with_an_open_worker_question() {
    used_up_retries_do_not_close_when(
        |db, run| {
            SqliteQueue::open(db)
                .unwrap()
                .ask(NewAsk {
                    topics: vec!["task_overlap".into()],
                    kind: AskKind::WorkerQuestion,
                    task_id: Some(run.task_id()),
                    run_id: Some(run.id().clone()),
                    question: "keep the old flag?".into(),
                    options: Vec::new(),
                    asked_by: "worker".into(),
                    reason_category: dagq::domain::AskReason::Scope,
                    finding_id: None,
                })
                .unwrap();
        },
        |_, _| (),
    );
}

/// An adopter of a run whose previous supervisor used its retries up and
/// went on to the `stuck_exit` path (`mark`) retries no more, and records
/// no `exit_retry` when the session exits after all.
fn an_adopter_after_used_up_retries(mark: impl FnOnce(&Path, &TaskRun)) {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_timeout = Duration::from_millis(500);
    let backend = Arc::new(backend);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    record(
        &db,
        &run,
        &[exit_requested(), exit_timed_out(), retried(1), retried(2)],
    );
    mark(&db, &run);
    age_lease(&db, &run, 31);
    let interval = Duration::from_millis(200);
    let options = retrying(2, interval);
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !adoption_events(&queue.show(TaskId::new(1)).unwrap()).is_empty()
    });
    // Past the adopter's own exit timeout and the interval after it, when
    // a retry would be sent, and passes after that.
    thread::sleep(backend.exit_timeout + interval);
    await_passes(&passes, SOME_PASSES);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(payloads(&detail, "exit_retried").len(), 2);
    assert!(
        !payloads(&detail, "auto_repaired")
            .iter()
            .any(|p| p["repair"] == "exit_retry")
    );
    assert!(event_kinds(&detail).contains(&"session_exited"));
}

#[test]
fn an_adopter_does_not_retry_after_a_stuck_exit_ask() {
    an_adopter_after_used_up_retries(|db, run| {
        SqliteQueue::open(db)
            .unwrap()
            .ask(NewAsk {
                topics: Vec::new(),
                kind: AskKind::StuckExit,
                task_id: Some(run.task_id()),
                run_id: Some(run.id().clone()),
                question: "send /exit".into(),
                options: vec!["exit".into(), "wait".into()],
                asked_by: "supervisor".into(),
                reason_category: dagq::domain::AskReason::RecoveryFailed,
                finding_id: None,
            })
            .unwrap();
    });
}

#[test]
fn an_adopter_does_not_retry_after_a_stuck_exit_recovery_job() {
    an_adopter_after_used_up_retries(|db, run| {
        record(
            db,
            run,
            &[(
                "recovery_requested",
                json!({"alert": "stuck_exit", "attempt": 1}),
            )],
        );
    });
}
