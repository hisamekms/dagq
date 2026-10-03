//! Runtime tests (task 844): the inputs the supervisor types into a
//! worker's session whose idle marker is missing, or older than its last
//! input, wait for the idle its screen shows (ADR-t803-1) instead of a
//! marker that never comes: the answer of a `worker_question` in its first
//! session, a resumed one and a revised one, and a recovery job's
//! `send_instruction`. A screen at work, at a dialog or unreadable types
//! nothing.
use crate::common;
use crate::runtime_support;
use dagq::domain::EventKind;

use runtime_support::*;

/// How long a screen must look idle in these tests, in milliseconds.
const SCREEN_IDLE_MS: u64 = 200;

/// The screen's threshold: a check that nothing happens past it waits it
/// out and then some passes of the supervisor (task 1075).
const SCREEN_IDLE: Duration = Duration::from_millis(SCREEN_IDLE_MS);

/// The agent's `Stop` hook failed to write the marker (task 475: the disk
/// was full), as Claude Code's debug log says.
const HOOK_FAILED: &str = "printf '2026-09-27T01:02:00Z [DEBUG] Hook Stop (Stop) error: No space left on device\\n' >> \"$LOG\"";

/// A dialog Claude Code stops at.
const DIALOG_SCREEN: &str = " Auto mode is available\n\n ❯ 1. Yes, turn on auto mode\n   2. No, keep asking\n\n Esc to cancel\n";

/// A worker that asks a `worker_question` and commits the answer it got,
/// its hook writing no idle marker.
fn markerless_asking_agent() -> String {
    format!(
        r#"{HOOK_FAILED}
"$DAGQ" ask --run "$RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question 'Which word?' --cmux /usr/bin/true > /dev/null || exit 70
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
cp "$MESSAGE" answer.txt; git add answer.txt; git commit -q -m answer
receipt "$(git rev-parse HEAD)"; await_exit
"#
    )
}

/// The thresholds of these tests: a screen at rest for [`SCREEN_IDLE_MS`],
/// and a receipt-less idle of `idle_without_receipt_ms`.
fn stall(idle_without_receipt_ms: u64) -> dagq::domain::stall::StallConfig {
    dagq::domain::stall::StallConfig::default()
        .with_millis("screen_idle_secs", SCREEN_IDLE_MS)
        .with_millis("idle_without_receipt_secs", idle_without_receipt_ms)
}

/// When the ask `ask` was opened, in Unix milliseconds (its `created_at`
/// keeps only the second).
fn opened_ms(detail: &dagq::domain::TaskDetail, ask: &dagq::domain::Ask) -> i64 {
    detail
        .events
        .iter()
        .find(|e| e.kind == "ask_opened" && e.payload["ask_id"] == json!(ask.id))
        .and_then(|e| dagq::domain::stats::timestamp_millis(&e.created_at))
        .unwrap()
}

/// Supervise with `backend` on a thread, with `reviewer` when given; and
/// the count of the supervisor's passes.
fn supervise_in_thread(
    db: &Path,
    repo: &Path,
    backend: &Arc<TestWorkspace>,
    reviewer: Option<&Arc<TestReviewer>>,
    stall: dagq::domain::stall::StallConfig,
) -> (thread::JoinHandle<Result<Value>>, Arc<AtomicU64>) {
    let (db, repo, backend) = (db.to_owned(), repo.to_owned(), backend.clone());
    let reviewer = reviewer.cloned();
    let options = SuperviseOptions {
        stall: Some(stall),
        ..supervise_options(4, true)
    };
    let passes = options.passes.clone();
    let supervisor = thread::spawn(move || {
        let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
        match reviewer {
            Some(reviewer) => runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            ),
            None => supervise_with(&db, &repo, &backend, &options),
        }
    });
    (supervisor, passes)
}

/// The open `worker_question` of task `task`, once it is asked.
fn question_of(db: &Path, task: i64) -> dagq::domain::Ask {
    wait_until(db, Duration::from_secs(60), |queue| {
        queue.asks(Default::default()).unwrap().iter().any(|ask| {
            ask.kind == AskKind::WorkerQuestion && ask.task_id == Some(TaskId::new(task))
        })
    });
    SqliteQueue::open(db)
        .unwrap()
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.kind == AskKind::WorkerQuestion && ask.task_id == Some(TaskId::new(task)))
        .unwrap()
}

fn delivered(db: &Path, task: i64) -> bool {
    !payloads(
        &SqliteQueue::open(db)
            .unwrap()
            .show(TaskId::new(task))
            .unwrap(),
        "ask_delivered",
    )
    .is_empty()
}

/// Wait until `texts` were typed into the backend.
fn typed(backend: &TestWorkspace, texts: usize) {
    let started = Instant::now();
    while backend.texts().len() < texts {
        assert!(started.elapsed() < Duration::from_secs(60), "not typed");
        thread::sleep(Duration::from_millis(20));
    }
}

/// A markerless first session answered while its screen shows `screen`
/// (`None`: cmux cannot read it) gets no answer typed; once its screen is
/// at rest, the answer is typed, `ask_delivered` recorded, and the idle
/// the screen showed recorded as `idle_inferred` of its phase.
fn a_markerless_question_is_answered_once_the_screen_rests(screen: Option<&str>) {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, &markerless_asking_agent());
    *backend.screen.lock().unwrap() = WORKING_SCREEN.into();
    let backend = Arc::new(backend);
    let (supervisor, passes) = supervise_in_thread(&db, &repo, &backend, None, stall(600_000));
    let ask = question_of(&db, 1);
    match screen {
        Some(screen) => *backend.screen.lock().unwrap() = screen.into(),
        None => backend.capture_timeouts.store(usize::MAX, Ordering::SeqCst),
    }
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.answer(ask.id, "use blue").unwrap();
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
    assert!(backend.texts().is_empty(), "{:?}", backend.texts());
    assert!(!delivered(&db, 1));
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(payloads(&detail, "idle_inferred").is_empty());
    assert!(!event_kinds(&detail).contains(&"prompt_waiting"));

    backend.capture_timeouts.store(0, Ordering::SeqCst);
    *backend.screen.lock().unwrap() = READY_SCREEN.into();
    typed(&backend, 1);
    // The answer put the screen at work; the session is done with it.
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"receipt_observed")
    });
    *backend.screen.lock().unwrap() = READY_SCREEN.into();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert_eq!(
        backend.texts(),
        vec![(
            WORKSPACE_ID.to_owned(),
            format!("answer to ask {}: use blue", ask.id)
        )]
    );
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    assert!(!Path::new(&run.idle_marker_path().unwrap()).exists());
    let delivered = payloads(&detail, "ask_delivered");
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert_eq!(delivered[0]["ask_id"], json!(ask.id));
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let inferred = payloads(&detail, "idle_inferred");
    assert!(!inferred.is_empty());
    assert_eq!(inferred[0]["phase"], "session");
    assert_eq!(inferred[0]["marker"], "missing");
    assert!(inferred[0]["since_ms"].as_i64().unwrap() > opened_ms(&detail, &ask));
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "idle_inferred") < position(&kinds, "ask_delivered"));
}

/// Acceptance: the answer of a markerless worker's question is typed once
/// its screen is at rest, and not while it shows work.
#[test]
fn a_markerless_first_session_gets_its_answer_once_its_screen_rests() {
    a_markerless_question_is_answered_once_the_screen_rests(Some(WORKING_SCREEN));
}

#[test]
fn a_markerless_first_session_at_a_dialog_gets_no_answer() {
    a_markerless_question_is_answered_once_the_screen_rests(Some(DIALOG_SCREEN));
}

#[test]
fn a_markerless_first_session_whose_screen_cannot_be_read_gets_no_answer() {
    a_markerless_question_is_answered_once_the_screen_rests(None);
}

/// A resumed session that asks once its request arrived and writes no
/// marker gets its answer typed once its screen is at rest, then ends its
/// stage by its screen.
#[test]
fn a_markerless_resumed_session_gets_its_answer_once_its_screen_rests() {
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
    // The first session's marker is older than the question (asked in a
    // later second: a marker of the ask's second counts).
    fs::write(run.idle_marker_path().unwrap(), "{}").unwrap();
    backend.resume_script_for(
        2,
        r#"await_message; rm "$MESSAGE"; sleep 1.1
"$DAGQ" ask --run "$RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question 'Which side?' --cmux /usr/bin/true > /dev/null || exit 70
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
receipt "$(git rev-parse HEAD)"; await_exit"#,
    );
    let backend = Arc::new(backend);
    let (supervisor, passes) = supervise_in_thread(&db, &repo, &backend, None, stall(600_000));
    // The request put the screen at work, and keeps it there while the
    // question is asked and answered.
    let ask = question_of(&db, 2);
    queue.answer(ask.id, "theirs").unwrap();
    thread::sleep(SCREEN_IDLE);
    await_passes(&passes, SOME_PASSES);
    assert!(!delivered(&db, 2));
    *backend.screen.lock().unwrap() = READY_SCREEN.into();
    wait_until(&db, Duration::from_secs(30), |_| delivered(&db, 2));
    *backend.screen.lock().unwrap() = READY_SCREEN.into();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(
        backend
            .texts()
            .contains(&(workspace_id(2), format!("answer to ask {}: theirs", ask.id))),
        "{:?}",
        backend.texts()
    );
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(
        payloads(&detail, "resume_finished")[0]["outcome"],
        "resolved"
    );
    let inferred = payloads(&detail, "idle_inferred");
    assert!(!inferred.is_empty());
    assert!(
        inferred.iter().all(|e| e["phase"] == "resume"),
        "{inferred:?}"
    );
    assert_eq!(inferred[0]["marker"], "stale");
    assert!(inferred[0]["since_ms"].as_i64().unwrap() > opened_ms(&detail, &ask));
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "idle_inferred") < position(&kinds, "ask_delivered"));
}

/// A revised session that asks and writes no marker gets its answer typed
/// once its screen is at rest, ends its revise by its screen, and lands.
#[test]
fn a_markerless_revised_session_gets_its_answer_once_its_screen_rests() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        r#"commit work; receipt "$(git rev-parse HEAD)"; idle
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done; rm "$MESSAGE"; sleep 1.1
"$DAGQ" ask --run "$RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question 'Which line?' --cmux /usr/bin/true > /dev/null || exit 70
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
cp "$MESSAGE" answer.txt; git add answer.txt; git commit -q -m answer
receipt "$(git rev-parse HEAD)"; await_exit"#,
    ));
    // At work until the question is answered: the first session is judged
    // by its marker alone, which is older than the question (asked in a
    // later second: a marker of the ask's second counts).
    *backend.screen.lock().unwrap() = WORKING_SCREEN.into();
    let reviewer = Arc::new(TestReviewer::new(&[
        verdict("revise", &["add a line"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]));
    let (supervisor, passes) =
        supervise_in_thread(&db, &repo, &backend, Some(&reviewer), stall(600_000));
    let ask = question_of(&db, 1);
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.answer(ask.id, "the last").unwrap();
    thread::sleep(SCREEN_IDLE);
    await_passes(&passes, SOME_PASSES);
    assert!(!delivered(&db, 1));
    *backend.screen.lock().unwrap() = READY_SCREEN.into();
    wait_until(&db, Duration::from_secs(30), |_| delivered(&db, 1));
    // At work on the answer until its receipt names the commit that holds
    // it: a screen left at rest meanwhile would end the revise before the
    // worker's commit under load, as a session idle with its old receipt.
    *backend.screen.lock().unwrap() = WORKING_SCREEN.into();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let first = run.result_commit().unwrap().as_str().to_owned();
    wait_until(&db, Duration::from_secs(30), |_| {
        let receipt: Option<Value> = fs::read(run.receipt_path().unwrap())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        receipt.is_some_and(|receipt| {
            receipt["commit"]
                .as_str()
                .is_some_and(|commit| commit != first)
        })
    });
    *backend.screen.lock().unwrap() = READY_SCREEN.into();
    let outcome = joined(supervisor, "the supervisor thread to return");
    backend.join();
    let outcome = outcome.unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
    let inferred = payloads(&detail, "idle_inferred");
    assert!(!inferred.is_empty());
    assert!(
        inferred.iter().all(|e| e["phase"] == "revise"),
        "{inferred:?}"
    );
    assert_eq!(inferred[0]["marker"], "stale");
    assert!(inferred[0]["since_ms"].as_i64().unwrap() > opened_ms(&detail, &ask));
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "idle_inferred") < position(&kinds, "ask_delivered"));
}

/// A markerless worker that stops without a receipt, takes the nudge and
/// stops again, then writes its receipt once the recovery job's
/// instruction arrives or `$EXIT.go` is written.
fn markerless_stalled_agent() -> String {
    format!(
        r#"{HOOK_FAILED}; commit work
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
rm -f "$MESSAGE"
until grep -q "recovery job" "$MESSAGE" 2>/dev/null || [ -f "$EXIT.go" ]; do sleep 0.05; done
receipt "$(git rev-parse HEAD)"; await_exit
"#
    )
}

/// The recovery job's `send_instruction`, once `gate` exists.
fn instruction_job(gate: &Path) -> String {
    format!(
        "while [ ! -f {} ]; do sleep 0.05; done; {}",
        shell_path(gate),
        repair(
            json!({"action": "send_instruction", "instruction": "write the receipt"}),
            "the session stopped at its prompt",
        )
    )
}

/// Supervise a markerless stalled worker whose recovery job answers once
/// `gate` exists; returns once its job was requested, the screen at rest,
/// with the count of the supervisor's passes.
fn markerless_stall(
    db: &Path,
    repo: &Path,
    gate: &Path,
) -> (
    Arc<TestWorkspace>,
    thread::JoinHandle<Result<Value>>,
    Arc<AtomicU64>,
) {
    let backend = Arc::new(TestWorkspace::new(db, false, &markerless_stalled_agent()));
    let reviewer = Arc::new(
        TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[instruction_job(gate)]),
    );
    let (supervisor, passes) =
        supervise_in_thread(db, repo, &backend, Some(&reviewer), stall(SCREEN_IDLE_MS));
    // The nudge put the screen at work; the session stopped again.
    typed(&backend, 1);
    *backend.screen.lock().unwrap() = READY_SCREEN.into();
    wait_until(db, Duration::from_secs(60), |queue| {
        !payloads(&queue.show(TaskId::new(1)).unwrap(), "recovery_requested").is_empty()
    });
    (backend, supervisor, passes)
}

/// Acceptance: a markerless session whose screen is at rest is at its
/// prompt for the recovery job's `send_instruction`, which is applied; the
/// session takes it and lands.
#[test]
fn an_instruction_reaches_a_markerless_session_whose_screen_rests() {
    let (dir, repo, db) = fixture();
    let gate = dir.path().join("gate");
    let (backend, supervisor, _) = markerless_stall(&db, &repo, &gate);
    fs::write(&gate, "").unwrap();
    wait_until(&db, Duration::from_secs(60), |queue| {
        !payloads(&queue.show(TaskId::new(1)).unwrap(), "recovery_finished").is_empty()
    });
    // The instruction put the screen at work; the session wrote its receipt.
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"receipt_observed")
    });
    *backend.screen.lock().unwrap() = READY_SCREEN.into();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["applied"], json!(["send_instruction"]));
    assert_eq!(finished[0]["escalated"], false);
    assert!(stalled_asks(&queue).is_empty());
    let texts = backend.texts();
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(texts[1].1.contains("write the receipt"), "{texts:?}");
    let inferred = payloads(&detail, "idle_inferred");
    assert!(
        inferred.iter().all(|e| e["phase"] == "session"),
        "{inferred:?}"
    );
}

/// A markerless session whose screen went back to work before its
/// recovery job answered is no longer idle at its prompt: the idle the job
/// was for is over (`alert_cleared`), and its `send_instruction` is not
/// typed.
#[test]
fn an_instruction_does_not_reach_a_markerless_session_at_work() {
    let (dir, repo, db) = fixture();
    let gate = dir.path().join("gate");
    let (backend, supervisor, passes) = markerless_stall(&db, &repo, &gate);
    *backend.screen.lock().unwrap() = WORKING_SCREEN.into();
    // Past a capture of the screen at work.
    let captured = backend.captures.load(Ordering::SeqCst);
    let started = Instant::now();
    while backend.captures.load(Ordering::SeqCst) < captured + 2 {
        assert!(started.elapsed() < Duration::from_secs(30), "not captured");
        thread::sleep(Duration::from_millis(20));
    }
    fs::write(&gate, "").unwrap();
    wait_until(&db, Duration::from_secs(60), |queue| {
        !payloads(&queue.show(TaskId::new(1)).unwrap(), "recovery_finished").is_empty()
    });
    // Passes for a verdict to be applied, were it.
    await_passes(&passes, SOME_PASSES);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["outcome"], "alert_cleared", "{finished:?}");
    assert!(payloads(&detail, "auto_repaired").is_empty());
    assert_eq!(backend.texts().len(), 1, "{:?}", backend.texts());
    // Let the session write its receipt and rest.
    let run = detail.runs[0].clone();
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    *backend.screen.lock().unwrap() = READY_SCREEN.into();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
}

/// A markerless session that stops at a dialog has it recorded as
/// `prompt_waiting` (its screen does not look idle there), and once its
/// screen is at rest the dialog is cleared (`prompt_cleared`) without a
/// marker; the run goes on to validation.
#[test]
fn a_markerless_session_has_its_dialog_cleared_once_its_screen_rests() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            r#"{HOOK_FAILED}; commit work
while [ ! -f "$EXIT.go" ]; do sleep 0.05; done
receipt "$(git rev-parse HEAD)"; await_exit"#
        ),
    );
    backend.prompt_wait = Duration::from_millis(300);
    *backend.screen.lock().unwrap() = DIALOG_SCREEN.into();
    let backend = Arc::new(backend);
    let (supervisor, _) = supervise_in_thread(&db, &repo, &backend, None, stall(600_000));
    let count = |queue: &mut SqliteQueue, kind: &str| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap())
            .iter()
            .filter(|k| **k == kind)
            .count()
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        count(queue, "prompt_waiting") == 1
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(payloads(&queue.show(TaskId::new(1)).unwrap(), "idle_inferred").is_empty());
    *backend.screen.lock().unwrap() = READY_SCREEN.into();
    wait_until(&db, Duration::from_secs(30), |queue| {
        count(queue, "prompt_cleared") == 1
    });
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert!(!Path::new(&run.idle_marker_path().unwrap()).exists());
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(count(&mut queue, "prompt_waiting"), 1);
    let inferred = payloads(&detail, "idle_inferred");
    assert!(!inferred.is_empty());
    assert_eq!(inferred[0]["phase"], "session");
    assert_eq!(inferred[0]["marker"], "missing");
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "prompt_waiting") < position(&kinds, "idle_inferred"));
}
