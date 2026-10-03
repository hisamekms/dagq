//! Runtime tests: a worker's session whose idle marker is missing, or older
//! than its last input, is judged idle by its screen (ADR-t803-1): its
//! first session goes on to validation after its receipt and is nudged when
//! idle without one, and a resumed or revised session ends its stage
//! without waiting out the resume timeout. A screen at work, at a dialog or
//! unreadable infers nothing, and a session with its marker is judged by
//! the marker as before. The tests run interactive sessions because only
//! an interactive session has a screen and an idle marker to judge (goal 92).
use crate::common;
use crate::runtime_support;
use dagq::domain::EventKind;

use runtime_support::*;

/// How long a screen must look idle in these tests, in milliseconds.
const SCREEN_IDLE_MS: u64 = 200;

/// The screen's threshold: a check that nothing happens past it waits it
/// out and then some passes of the supervisor (task 1075).
const SCREEN_IDLE: Duration = Duration::from_millis(SCREEN_IDLE_MS);

/// The thresholds of these tests: a screen at rest for [`SCREEN_IDLE_MS`].
fn stall() -> dagq::domain::stall::StallConfig {
    dagq::domain::stall::StallConfig::default().with_millis("screen_idle_secs", SCREEN_IDLE_MS)
}

/// The agent's `Stop` hook failed to write the marker (task 475: the disk
/// was full), as Claude Code's debug log says.
const HOOK_FAILED: &str = "printf '2026-09-27T01:02:00Z [DEBUG] Hook Stop (Stop) error: No space left on device\\n' >> \"$LOG\"";

/// A worker that commits and writes its receipt, but whose hook never
/// writes the idle marker.
fn markerless_agent() -> String {
    format!("{HOOK_FAILED}; commit work; receipt \"$(git rev-parse HEAD)\"; await_exit")
}

fn options() -> SuperviseOptions {
    SuperviseOptions {
        stall: Some(stall()),
        ..supervise_options(4, true)
    }
}

fn supervise_reviewed_with(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    reviewer: &TestReviewer,
) -> Value {
    let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
    let outcome = runtime::supervise_with_reviewer(
        db,
        repo,
        backend,
        &claude_stub(db),
        reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options(),
    )
    .unwrap();
    backend.join();
    outcome
}

fn inferred(detail: &dagq::domain::TaskDetail) -> Vec<&Value> {
    payloads(detail, "idle_inferred")
}

/// Task 475: the worker wrote its receipt and stopped, but its `Stop` hook
/// could not write the idle marker. Its screen, at rest over
/// `screen_idle_secs`, stands in: the run goes on to validation with the
/// session open, `session_idle_observed` says the idle was read from the
/// screen, and `idle_inferred` names the failed hook.
#[test]
fn a_first_session_whose_hook_could_not_write_its_marker_is_validated_by_its_screen() {
    interactive_workers();
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, &markerless_agent());
    let outcome = supervise_with(&db, &repo, &backend, &options()).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    assert!(!Path::new(&run.idle_marker_path().unwrap()).exists());
    let observed = payloads(&detail, "session_idle_observed");
    assert_eq!(observed.len(), 1, "{observed:?}");
    assert_eq!(observed[0]["source"], "screen");
    assert_eq!(observed[0]["marker"], "missing");
    assert_eq!(observed[0]["background_running"], false);
    assert!(observed[0]["captures"].as_i64().unwrap() >= 2);
    assert!(
        observed[0]["marker_modified"].as_i64().unwrap()
            >= observed[0]["receipt_modified"].as_i64().unwrap()
    );
    let events = inferred(&detail);
    assert!(!events.is_empty());
    let last = events.last().unwrap();
    assert_eq!(last["phase"], "session");
    assert_eq!(last["source"], "screen");
    assert_eq!(last["marker"], "missing");
    assert_eq!(last["workspace_id"], WORKSPACE_ID);
    assert_eq!(last["background_running"], false);
    assert!(last["observed_ms"].as_i64().unwrap() >= SCREEN_IDLE_MS as i64);
    assert_eq!(
        last["observed_secs"].as_i64().unwrap(),
        last["observed_ms"].as_i64().unwrap() / 1000
    );
    assert!(
        last["hook_error"]
            .as_str()
            .unwrap()
            .contains("Hook Stop (Stop) error: No space left on device"),
        "{last}"
    );
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "receipt_observed") < position(&kinds, "session_idle_observed"));
    assert!(position(&kinds, "session_idle_observed") < position(&kinds, "validation_finished"));
}

/// A session with its marker is judged by it: the marker the session
/// writes after its receipt takes it to validation while its screen never
/// looks idle, and nothing is inferred. (A screen at rest before the
/// marker is written would be inferred idle over the threshold, which is
/// what these tests make short.)
#[test]
fn a_first_session_with_its_marker_is_judged_by_the_marker() {
    interactive_workers();
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    *backend.screen.lock().unwrap() = WORKING_SCREEN.into();
    let outcome = supervise_with(&db, &repo, &backend, &options()).unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert!(inferred(&detail).is_empty());
    let observed = payloads(&detail, "session_idle_observed");
    assert_eq!(observed.len(), 1, "{observed:?}");
    assert_eq!(observed[0]["source"], Value::Null);
    assert_eq!(observed[0]["background_running"], false);
}

/// A session idle without a receipt and without a marker is nudged once
/// its screen has looked idle for `idle_without_receipt_secs`; it answers
/// with its receipt (and a marker, the disk freed).
#[test]
fn a_first_session_without_its_marker_is_nudged_when_idle_without_a_receipt() {
    interactive_workers();
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "{HOOK_FAILED}; commit work
while [ ! -f \"$MESSAGE\" ]; do sleep 0.05; done
receipt \"$(git rev-parse HEAD)\"; idle; await_exit"
        ),
    );
    let options = SuperviseOptions {
        stall: Some(stall().with_millis("idle_without_receipt_secs", SCREEN_IDLE_MS)),
        ..supervise_options(4, true)
    };
    let outcome = supervise_with(&db, &repo, &backend, &options).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let texts = backend.texts();
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].1.contains("without a receipt"), "{}", texts[0].1);
    let nudged = payloads(&detail, "stall_nudged");
    assert_eq!(nudged.len(), 1, "{nudged:?}");
    assert_eq!(nudged[0]["phase"], "session");
    assert_eq!(nudged[0]["background_running"], false);
    let events = inferred(&detail);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["phase"], "session");
    assert_eq!(events[0]["marker"], "missing");
    let resolved = payloads(&detail, "stall_resolved");
    assert_eq!(resolved[0]["outcome"], "resolved_by_nudge", "{resolved:?}");
    // The receipt went with a marker: judged by it.
    let observed = payloads(&detail, "session_idle_observed");
    assert_eq!(observed[0]["source"], Value::Null);
}

/// A markerless session after its receipt whose screen shows `screen`
/// (`None`: cmux cannot read it) is not taken for idle; once the screen
/// is at rest it is.
fn a_markerless_session_is_held_by(screen: Option<&str>) {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, &markerless_agent());
    match screen {
        Some(screen) => *backend.screen.lock().unwrap() = screen.into(),
        None => backend.capture_timeouts.store(usize::MAX, Ordering::SeqCst),
    }
    let backend = Arc::new(backend);
    let options = options();
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"receipt_observed")
    });
    // Past the threshold, and passes after it, with captures on the way.
    // A screen that cannot be read makes each pass take about 250 ms (its
    // captures fail), so there the fixed 2.5 thresholds stays: shorter
    // than the passes, and as before (task 1075).
    let captured = backend.captures.load(Ordering::SeqCst);
    if screen.is_some() {
        thread::sleep(SCREEN_IDLE);
        await_passes(&passes, SOME_PASSES);
    } else {
        thread::sleep(SCREEN_IDLE * 5 / 2);
    }
    assert!(backend.captures.load(Ordering::SeqCst) >= captured + 2);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"session_idle_observed"), "{kinds:?}");
    assert!(inferred(&detail).is_empty());
    backend.capture_timeouts.store(0, Ordering::SeqCst);
    *backend.screen.lock().unwrap() = READY_SCREEN.into();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(
        payloads(&detail, "session_idle_observed")[0]["source"],
        "screen"
    );
}

#[test]
fn a_markerless_session_at_work_is_not_idle() {
    interactive_workers();
    a_markerless_session_is_held_by(Some(WORKING_SCREEN));
}

#[test]
fn a_markerless_session_at_a_dialog_is_not_idle() {
    interactive_workers();
    a_markerless_session_is_held_by(Some(
        " Auto mode is available\n\n ❯ 1. Yes, turn on auto mode\n   2. No, keep asking\n\n Esc to cancel\n",
    ));
}

#[test]
fn a_markerless_session_whose_screen_cannot_be_read_is_not_idle() {
    interactive_workers();
    a_markerless_session_is_held_by(None);
}

/// Claude Code at rest with background shells still running, as its
/// status line under the input box counts them.
const BACKGROUND_SCREEN: &str = "\
⏺ Done.

──────────────────────────────────────────────────────────────────────
❯\x20
──────────────────────────────────────────────────────────────────────
  ⏵⏵ auto mode on · 2 shells · ← for agents · ↓ to manage
";

/// Task 823: a markerless session after its receipt whose screen shows
/// background shells running is inferred idle with its background work,
/// and is not taken for idle (it waits as a marker's background work
/// would); once the shells are done, the screen at rest starts a new span
/// and the run goes on to validation.
#[test]
fn a_markerless_session_that_shows_background_work_is_not_idle() {
    interactive_workers();
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, &markerless_agent());
    *backend.screen.lock().unwrap() = BACKGROUND_SCREEN.into();
    let backend = Arc::new(backend);
    let options = options();
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    // The screen is read from the session's start: wait for its receipt
    // too, which a loaded host may write after the first inference.
    wait_until(&db, Duration::from_secs(30), |queue| {
        let detail = queue.show(TaskId::new(1)).unwrap();
        !inferred(&detail).is_empty() && event_kinds(&detail).contains(&"receipt_observed")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(inferred(&detail)[0]["background_running"], true);
    // Past the threshold, and passes after it, with captures on the way.
    let captured = backend.captures.load(Ordering::SeqCst);
    thread::sleep(SCREEN_IDLE);
    await_passes(&passes, SOME_PASSES);
    assert!(backend.captures.load(Ordering::SeqCst) >= captured + 2);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(kinds.contains(&"receipt_observed"), "{kinds:?}");
    assert!(!kinds.contains(&"session_idle_observed"), "{kinds:?}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    *backend.screen.lock().unwrap() = READY_SCREEN.into();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let observed = payloads(&detail, "session_idle_observed");
    assert_eq!(observed[0]["source"], "screen");
    assert_eq!(observed[0]["background_running"], false);
    let events = inferred(&detail);
    assert_eq!(
        events.last().unwrap()["background_running"],
        false,
        "{events:?}"
    );
    assert!(events.len() >= 2, "{events:?}");
}

/// A resumed session that rewrote its receipt but wrote no marker ends its
/// stage once its screen is at rest, well before the resume timeout, and
/// goes on to validation with the session open.
#[test]
fn a_resumed_session_without_its_marker_ends_its_stage_by_its_screen() {
    interactive_workers();
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    let mut queue = SqliteQueue::open(&db).unwrap();
    Connection::open(&db)
        .unwrap()
        .execute(
            "DELETE FROM run_events WHERE run_id=?1 AND kind='integration_approved'",
            [&run.id()],
        )
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::EvidenceMissing,
            json!({"status": "needs_session", "reason": "e2e has no evidence"}),
        )
        .unwrap();
    // The first session's marker is older than the request.
    fs::write(run.idle_marker_path().unwrap(), "{}").unwrap();
    backend.resume_script_for(
        2,
        "await_message; receipt \"$(git rev-parse HEAD)\"; await_exit",
    );
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options()))
    };
    // The request put the screen at work; the session is done with it
    // once it rewrote its receipt. A screen at rest before that (a slow
    // stub under load) would be inferred idle before the receipt and
    // again after it, the receipt being a later input of the stage.
    let receipt = PathBuf::from(run.receipt_path().unwrap());
    let started = Instant::now();
    while backend.texts().is_empty()
        || !fs::read_to_string(&receipt).is_ok_and(|text| text.contains("\"resolved\""))
    {
        assert!(started.elapsed() < Duration::from_secs(30));
        thread::sleep(Duration::from_millis(20));
    }
    *backend.screen.lock().unwrap() = READY_SCREEN.into();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(backend.resume_timeout > Duration::from_secs(60));
    let detail = queue.show(TaskId::new(2)).unwrap();
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["outcome"], "resolved");
    assert_eq!(finished[0]["session_live"], true);
    let events = inferred(&detail);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["phase"], "resume");
    assert_eq!(events[0]["marker"], "stale");
    assert_eq!(
        events[0]["workspace_id"], finished[0]["workspace_id"],
        "{events:?}"
    );
}

/// A revised session that fixed its run and rewrote its receipt but wrote
/// no marker ends its revise once its screen is at rest, well before the
/// resume timeout; the run is reviewed again and lands.
#[test]
fn a_revised_session_without_its_marker_ends_its_revise_by_its_screen() {
    interactive_workers();
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle
while [ ! -f \"$MESSAGE\" ]; do sleep 0.05; done
printf 'fix\\n' >> change.txt; git commit -q -am fix
receipt \"$(git rev-parse HEAD)\"; await_exit",
    ));
    // At work until the revise is done: the first session is judged by its
    // marker alone.
    *backend.screen.lock().unwrap() = WORKING_SCREEN.into();
    let reviewer = Arc::new(TestReviewer::new(&[
        verdict("revise", &["add a line to change.txt"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]));
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || supervise_reviewed_with(&db, &repo, &backend, &reviewer))
    };
    let started = Instant::now();
    while backend.texts().is_empty() {
        assert!(started.elapsed() < Duration::from_secs(30));
        thread::sleep(Duration::from_millis(20));
    }
    *backend.screen.lock().unwrap() = READY_SCREEN.into();
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &base);
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
    // One event per span. The revise watch counts the rewritten receipt as
    // an input and the prompt watch of the same session does not, so a
    // span the prompt watch saw begin before the receipt can be started
    // over by the revise watch and recorded again.
    let events = inferred(&detail);
    assert!((1..=2).contains(&events.len()), "{events:?}");
    for event in &events {
        assert_eq!(event["phase"], "revise");
        assert_eq!(event["marker"], "stale");
    }
    let since: Vec<i64> = events
        .iter()
        .map(|e| e["since_ms"].as_i64().unwrap())
        .collect();
    assert!(since.windows(2).all(|pair| pair[0] < pair[1]), "{events:?}");
}
