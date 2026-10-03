//! Runtime tests: the retries of a `/exit` a resumed session held back past
//! its timeout, or that never reached it (ADR-0047 decision 25, task 936):
//! what each one sends for the screen it reads, the close of the workspace
//! of a run whose review passed once they are used up, the `stuck_exit`
//! path of any other run, and a supervisor taking the resume over that
//! carries the retries on. The tests run interactive sessions because the
//! retries read a terminal session's screen and type `/exit` into it,
//! which a headless session does not get (goal 92).
use crate::common;
use crate::runtime_adopt::backdate_event;
use crate::runtime_handoff::hand_off_when;
use crate::runtime_review_adopt::DIALOG_SCREEN;
use crate::runtime_support;

use dagq::domain::{EventKind, LeaseToken, exit::ExitConfig};
use runtime_support::*;

/// Resolves the conflict, rewrites the receipt, goes idle, ignores the
/// first `/exit` (its request file is taken away, as a dialog that ate it
/// would) and exits on the next one, or when its workspace is closed.
const IGNORES_FIRST_EXIT: &str = "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; \
     await_file \"$EXIT\"; rm \"$EXIT\"; await_exit";

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

fn supervised(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    options: &SuperviseOptions,
) -> Value {
    let outcome = {
        let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
        supervise_with(db, repo, backend, options).unwrap()
    };
    backend.join();
    outcome
}

/// Record that the parked run's latest review passed, as one whose landing
/// conflicted after its review would have.
fn review_passed(db: &Path, run: &TaskRun) {
    SqliteQueue::open(db)
        .unwrap()
        .record_runtime_event(
            run.id(),
            EventKind::ReviewFinished,
            json!({"verdict": "pass", "reasons": []}),
        )
        .unwrap();
}

fn repairs<'a>(detail: &'a dagq::domain::TaskDetail, repair: &str) -> Vec<&'a Value> {
    payloads(detail, "auto_repaired")
        .into_iter()
        .filter(|p| p["repair"] == repair)
        .collect()
}

fn stuck_exit_asks(queue: &mut SqliteQueue) -> usize {
    queue
        .asks(AskQuery::default())
        .unwrap()
        .iter()
        .filter(|ask| ask.kind == AskKind::StuckExit)
        .count()
}

/// A resumed session that held its `/exit` back past the timeout and
/// shows an empty input box gets `/exit` again: `exit_request_timed_out`
/// (of the attempt) and `exit_retried` are recorded, it exits on that
/// retry, which is repaired as `auto_repaired` (`repair: exit_retry`) and
/// counted by `stats`, and the run lands without a recovery job or ask.
#[test]
fn a_resumed_sessions_held_exit_is_typed_again_into_a_ready_input_box() {
    interactive_workers();
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (_, first_landed) = parked_conflict(&repo, &db, &backend);
    // Kept at a second: the session has to take the first /exit away (a
    // shell loop polling every 50ms) before the first retry, due at the
    // timeout, types it again; a loaded host can stall that loop.
    backend.exit_timeout = Duration::from_secs(1);
    backend.resume_script_for(2, IGNORES_FIRST_EXIT);
    let before = backend.exits_sent.load(Ordering::SeqCst);
    let outcome = supervised(&db, &repo, &backend, &retrying(3, Duration::from_secs(5)));
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst) - before, 2);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    let timed_out = payloads(&detail, "exit_request_timed_out");
    assert_eq!(timed_out.len(), 1, "{timed_out:?}");
    assert_eq!(timed_out[0]["resume_attempt"], 1);
    assert_eq!(timed_out[0]["code"], "exit_timeout");
    let retried = payloads(&detail, "exit_retried");
    assert_eq!(retried.len(), 1, "{retried:?}");
    assert_eq!(retried[0]["attempt"], 1);
    assert_eq!(retried[0]["cause"], "exit_timeout");
    assert_eq!(retried[0]["screen"], "input_ready");
    assert_eq!(retried[0]["send"], "exit");
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "exit_request_timed_out") < position(&kinds, "exit_retried"));
    assert!(position(&kinds, "exit_retried") < position(&kinds, "resume_finished"));
    let retry = repairs(&detail, "exit_retry");
    assert_eq!(retry.len(), 1);
    assert_eq!(
        retry[0]["conditions"],
        json!({"attempts": 1, "cause": "exit_timeout", "screen": "input_ready"})
    );
    assert!(repairs(&detail, "exit_forced_close").is_empty());
    assert_eq!(
        payloads(&detail, "resume_finished")[0]["outcome"],
        "resolved"
    );
    assert!(!kinds.contains(&"recovery_requested"), "{kinds:?}");
    assert_eq!(stuck_exit_asks(&mut queue), 0);
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(
        stats["auto_repairs"]["by_layer"]["runtime"]["by_repair"]["exit_retry"], 1,
        "{stats}"
    );
}

/// A dialog on the resumed session's screen gets nothing from the retries
/// (neither `/exit` nor Enter). Once they are used up, the run whose
/// latest review passed and whose receipt names its clean head has its
/// workspace closed (`exit_forced_close`), and the resume is judged as for
/// a session that exited: resolved and approved, it lands, its lease kept.
#[test]
fn used_up_retries_over_a_dialog_close_a_reviewed_resume_and_judge_it() {
    interactive_workers();
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    review_passed(&db, &run);
    // The dialog comes up once the submit's confirmation has read the
    // screen the first /exit reached (the stub arms it on send_exit and
    // shows it after the next capture), so the first retry, due a second
    // later, reads it with no timing of the test's thread involved.
    backend.exit_timeout = Duration::from_secs(1);
    backend.close_ends_session = true;
    backend.resume_script_for(2, IGNORES_FIRST_EXIT);
    *backend.screen_after_exit.lock().unwrap() = Some(DIALOG_SCREEN.into());
    let before = backend.exits_sent.load(Ordering::SeqCst);
    let outcome = supervised(
        &db,
        &repo,
        &backend,
        &retrying(2, Duration::from_millis(300)),
    );
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    // Only the first /exit: nothing is typed over the dialog.
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst) - before, 1);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    let retried = payloads(&detail, "exit_retried");
    assert_eq!(retried.len(), 2, "{retried:?}");
    for retry in &retried {
        assert_eq!(retry["screen"], "dialog");
        assert_eq!(retry["send"], "nothing");
    }
    let forced = repairs(&detail, "exit_forced_close");
    assert_eq!(forced.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(forced[0]["conditions"]["cause"], "exit_timeout");
    assert_eq!(forced[0]["conditions"]["attempts"], 2);
    assert_eq!(forced[0]["conditions"]["then"], "resume_verdict");
    assert_eq!(forced[0]["conditions"]["resume_attempt"], 1);
    assert!(repairs(&detail, "exit_retry").is_empty());
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["outcome"], "resolved");
    assert_eq!(finished[0]["workspace_closed"], true);
    assert_eq!(finished[0]["exit_forced_close"], true);
    let workspace = finished[0]["workspace_id"].as_str().unwrap();
    assert!(backend.closed().iter().any(|w| w == workspace));
    let kinds = event_kinds(&detail);
    assert!(
        payloads(&detail, "workspace_closed")
            .iter()
            .any(|p| p["resume_attempt"] == 1 && p["workspace_id"] == workspace)
    );
    assert!(!kinds.contains(&"recovery_requested"), "{kinds:?}");
    assert_eq!(stuck_exit_asks(&mut queue), 0);
    assert!(
        Path::new(run.run_dir().unwrap())
            .join("terminal-resume-1.txt")
            .is_file()
    );
}

/// A resumed session whose run has not passed a review (a resume before
/// validation, here one whose stand-in review failed) is never closed:
/// the retries, typed into an empty input box, are used up and the run
/// goes to the `stuck_exit` recovery job and ask, its workspace kept.
#[test]
fn used_up_retries_of_an_unreviewed_resume_go_to_the_stuck_exit_ask() {
    interactive_workers();
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    let exit_timeout = backend.exit_timeout;
    backend.exit_timeout = Duration::from_millis(500);
    backend.close_ends_session = true;
    backend.resume_script_for(
        2,
        &format!("await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; {HOLD}"),
    );
    let before = backend.exits_sent.load(Ordering::SeqCst);
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
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    // The first /exit and one per retry.
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst) - before, 3);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    let retried = payloads(&detail, "exit_retried");
    assert_eq!(retried.len(), 2, "{retried:?}");
    for retry in &retried {
        assert_eq!(retry["send"], "exit");
    }
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "exit_retried") < position(&kinds, "recovery_requested"));
    assert!(repairs(&detail, "exit_forced_close").is_empty());
    assert!(repairs(&detail, "exit_retry").is_empty());
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished[0]["outcome"], "unresolved");
    assert_eq!(finished[0]["exit_timed_out"], true);
    assert_eq!(finished[0]["workspace_closed"], false);
    let kept = finished[0]["workspace_id"].as_str().unwrap().to_owned();
    assert!(!backend.closed().contains(&kept));
    assert_eq!(stuck_exit_asks(&mut queue), 1);
    release_held_session(run.run_dir().unwrap());
    backend.join();
    backend.exit_timeout = exit_timeout;
}

/// A `/exit` that cmux timed out on without it reaching the resumed
/// session is retried (`cause: backend_timeout`) instead of waiting out
/// the exit timeout; the retry's `/exit` gets there and the session exits,
/// repaired as `exit_retry`.
#[test]
fn an_unsent_exit_of_a_resumed_session_is_retried() {
    interactive_workers();
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (_, first_landed) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    // Every attempt of the first /exit times out without reaching it.
    backend.exit_unsent.store(3, Ordering::SeqCst);
    let before = backend.exits_sent.load(Ordering::SeqCst);
    let outcome = supervised(&db, &repo, &backend, &retrying(3, Duration::from_secs(5)));
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst) - before, 4);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    let timed_out = payloads(&detail, "exit_request_timed_out");
    assert_eq!(timed_out.len(), 1, "{timed_out:?}");
    assert_eq!(timed_out[0]["unsent"], true);
    assert_eq!(timed_out[0]["resume_attempt"], 1);
    let retried = payloads(&detail, "exit_retried");
    assert_eq!(retried.len(), 1, "{retried:?}");
    assert_eq!(retried[0]["cause"], "backend_timeout");
    assert_eq!(retried[0]["send"], "exit");
    let retry = repairs(&detail, "exit_retry");
    assert_eq!(retry.len(), 1);
    assert_eq!(retry[0]["conditions"]["cause"], "backend_timeout");
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"recovery_requested"), "{kinds:?}");
    assert_eq!(stuck_exit_asks(&mut queue), 0);
}

/// A resume taken over by the process its supervisor handed off to after
/// its `/exit` timed out and was retried once: the timeout is not recorded
/// again, the first retry is not made again, and the session exiting on
/// the second is repaired with its two attempts.
#[test]
fn a_resume_taken_over_after_a_handoff_carries_its_exit_retries_on() {
    interactive_workers();
    resume_taken_over_after_a_retry(true, false);
}

/// The same for a resume adopted from a supervisor that died.
#[test]
fn an_adopted_resume_carries_its_exit_retries_on() {
    interactive_workers();
    resume_taken_over_after_a_retry(false, false);
}

/// A retry that answered a known dialog gave the `/exit` its timeout again
/// before the supervisor handed off: the next process records the timeout
/// again once it passes, but goes on from the second retry rather than
/// making all of them anew (ADR-0047 decision 38).
#[test]
fn a_resume_taken_over_after_a_retry_answered_a_dialog_does_not_retry_anew() {
    interactive_workers();
    resume_taken_over_after_a_retry(true, true);
}

/// `answered`: the first retry answered a known dialog by its rule
/// (`auto_repaired` with `repair: dialog_answered`, 90 seconds ago), which
/// restarted the exit timeout of 60 seconds.
fn resume_taken_over_after_a_retry(handoff: bool, answered: bool) {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.exit_timeout = Duration::from_secs(60);
    let backend = Arc::new(backend);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        "await_message; await_file \"$EXIT.go\"; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
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
    for (kind, payload) in [
        (
            EventKind::ExitRequested,
            json!({"workspace_id": workspace, "timeout_secs": 60, "resume_attempt": 1}),
        ),
        (
            EventKind::ExitRequestTimedOut,
            json!({"code": "exit_timeout", "workspace_id": workspace, "timeout_secs": 60, "resume_attempt": 1}),
        ),
        (
            EventKind::ExitRetried,
            json!({"attempt": 1, "cause": "exit_timeout", "screen": "not_ready", "send": "nothing"}),
        ),
    ] {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
    backdate_event(&db, &run, "exit_requested", 120);
    backdate_event(&db, &run, "exit_request_timed_out", 60);
    backdate_event(&db, &run, "exit_retried", 30);
    if answered {
        queue
            .record_runtime_event(
                run.id(),
                EventKind::AutoRepaired,
                json!({"layer": "runtime", "repair": "dialog_answered", "dialog": "settings_panel"}),
            )
            .unwrap();
        backdate_event(&db, &run, "auto_repaired", 90);
    }
    let snapshot = Path::new(run.run_dir().unwrap()).join("handoff.json");
    if handoff {
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
    // The session has resolved the conflict and waits at its input box.
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let before = backend.exits_sent.load(Ordering::SeqCst);
    let outcome = supervised(
        &db,
        &repo,
        &backend,
        &SuperviseOptions {
            handoff_token: handoff.then(|| LeaseToken::new(token)),
            // The second retry is due at the takeover (the first was 30
            // seconds ago). The wait after it is long: the retries are used
            // up at its end, and the session, released just before the
            // takeover, has to resolve, write its receipt and exit on that
            // retry's `/exit` before then for it to count as `exit_retry`.
            // Under load that took longer than a short wait (task 1043).
            exit: Some(ExitConfig {
                retries: 2,
                intervals: vec![Duration::from_millis(300), Duration::from_secs(60)],
            }),
            ..supervise_options(4, true)
        },
    );
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst) - before, 1);
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    assert_eq!(
        payloads(&detail, "exit_request_timed_out").len(),
        if answered { 2 } else { 1 }
    );
    let retried = payloads(&detail, "exit_retried");
    assert_eq!(retried.len(), 2, "{retried:?}");
    assert_eq!(retried[1]["attempt"], 2);
    assert_eq!(retried[1]["screen"], "input_ready");
    assert_eq!(retried[1]["send"], "exit");
    let retry = repairs(&detail, "exit_retry");
    assert_eq!(retry.len(), 1);
    assert_eq!(retry[0]["conditions"]["attempts"], 2);
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"recovery_requested"), "{kinds:?}");
}
