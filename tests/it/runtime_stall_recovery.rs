//! Runtime tests: the `stalled` alert's recovery job (task 442, ADR-0047
//! decisions 30, 31, 39 and 40): a session idle without a receipt after its
//! nudge, and a text the supervisor typed that the session did not take,
//! go to the recovery job before any ask.
use crate::runtime_support;
use dagq::domain::EventKind;

use runtime_support::*;

/// The worker of the idle tests: it commits and stops with background work
/// running, takes the nudge and stops again, then writes its receipt once
/// the recovery job's instruction arrives or `$EXIT.go` is written.
const STALLED_AGENT: &str = r#"
commit work; idle_bg
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
rm -f "$MESSAGE"; idle_bg
until grep -q "recovery job" "$MESSAGE" 2>/dev/null || [ -f "$EXIT.go" ]; do sleep 0.05; done
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#;

/// Supervise the fixture's task with `agent`, the thresholds `stall` and
/// `recoveries` as the recovery jobs' scripts, on a thread.
fn supervise_stalled(
    db: &Path,
    repo: &Path,
    agent: &str,
    stall: dagq::domain::stall::StallConfig,
    recoveries: &[String],
) -> (
    Arc<TestWorkspace>,
    Arc<TestReviewer>,
    thread::JoinHandle<Result<Value>>,
) {
    supervise_stalled_counted(db, repo, agent, stall, recoveries).0
}

/// [`supervise_stalled`], and the count of the supervisor's passes.
#[allow(clippy::type_complexity)]
fn supervise_stalled_counted(
    db: &Path,
    repo: &Path,
    agent: &str,
    stall: dagq::domain::stall::StallConfig,
    recoveries: &[String],
) -> (
    (
        Arc<TestWorkspace>,
        Arc<TestReviewer>,
        thread::JoinHandle<Result<Value>>,
    ),
    Arc<AtomicU64>,
) {
    let backend = Arc::new(TestWorkspace::new(db, false, agent));
    let (reviewer, supervisor, passes) =
        supervise_backend_counted(db, repo, backend.clone(), stall, recoveries);
    ((backend, reviewer, supervisor), passes)
}

/// [`supervise_stalled`] with a backend the test set up.
fn supervise_backend(
    db: &Path,
    repo: &Path,
    backend: Arc<TestWorkspace>,
    stall: dagq::domain::stall::StallConfig,
    recoveries: &[String],
) -> (Arc<TestReviewer>, thread::JoinHandle<Result<Value>>) {
    let (reviewer, supervisor, _) = supervise_backend_counted(db, repo, backend, stall, recoveries);
    (reviewer, supervisor)
}

/// [`supervise_backend`], and the count of the supervisor's passes.
fn supervise_backend_counted(
    db: &Path,
    repo: &Path,
    backend: Arc<TestWorkspace>,
    stall: dagq::domain::stall::StallConfig,
    recoveries: &[String],
) -> (
    Arc<TestReviewer>,
    thread::JoinHandle<Result<Value>>,
    Arc<AtomicU64>,
) {
    let reviewer =
        Arc::new(TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(recoveries));
    let options = SuperviseOptions {
        stall: Some(stall),
        ..supervise_options(4, true)
    };
    let passes = options.passes.clone();
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
                &options,
            )
        })
    };
    (reviewer, supervisor, passes)
}

/// The receipt-less idle threshold of these tests, in milliseconds.
const IDLE_MS: u64 = 200;

/// The threshold: a check that nothing happens past it waits it out and
/// then some passes of the supervisor (task 1075).
const IDLE: Duration = Duration::from_millis(IDLE_MS);

/// A receipt-less idle threshold of [`IDLE_MS`].
fn idle_short() -> dagq::domain::stall::StallConfig {
    dagq::domain::stall::StallConfig::default().with_millis("idle_without_receipt_secs", IDLE_MS)
}

/// A receipt-less idle threshold of one second, for a session that takes
/// an input some time after its idle: it must come short of the threshold.
fn idle_second() -> dagq::domain::stall::StallConfig {
    dagq::domain::stall::StallConfig {
        idle_without_receipt_secs: 1,
        ..Default::default()
    }
}

/// Let the session of the idle tests write its receipt, and wait for the
/// supervisor.
fn release(
    db: &Path,
    backend: &TestWorkspace,
    supervisor: thread::JoinHandle<Result<Value>>,
) -> Value {
    let mut queue = SqliteQueue::open(db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    outcome
}

/// The `detection` and `outcome` of each `stall_resolved`.
fn resolutions(detail: &dagq::domain::TaskDetail) -> Vec<(String, String)> {
    payloads(detail, "stall_resolved")
        .into_iter()
        .map(|p| {
            (
                p["detection"].as_str().unwrap().to_owned(),
                p["outcome"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn pair(detection: &str, outcome: &str) -> (String, String) {
    (detection.to_owned(), outcome.to_owned())
}

/// Acceptance (1) and (3): the session stays idle after its nudge, so the
/// recovery job is requested (`stalled`, reason `idle_without_receipt`)
/// before any ask; its `send_instruction` of high confidence is applied
/// (`auto_repaired`, layer `recovery`), the session takes it and writes its
/// receipt, and no ask opens.
#[test]
fn an_idle_after_the_nudge_is_repaired_by_the_recovery_job_without_an_ask() {
    let (_dir, repo, db) = fixture();
    let (backend, reviewer, supervisor) = supervise_stalled(
        &db,
        &repo,
        STALLED_AGENT,
        idle_short(),
        &[repair(
            json!({"action": "send_instruction", "instruction": "stop waiting for the tests and write the receipt"}),
            "the session waits for tests that finished",
        )],
    );
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["alert"], "stalled");
    assert_eq!(requested[0]["reason"], "idle_without_receipt");
    assert_eq!(requested[0]["threshold"], "idle_without_receipt_secs");
    assert_eq!(requested[0]["background_tasks"][0]["command"], "cargo test");
    // The live run's recovery job records the provider it runs on (task
    // 1062).
    assert_eq!(requested[0]["launch"]["role"], "recovery");
    assert_eq!(requested[0]["launch"]["provider"], "claude");
    let nudged = detail
        .events
        .iter()
        .find(|e| e.kind == "stall_nudged")
        .unwrap();
    assert_eq!(requested[0]["evidence"], json!([nudged.id]));
    let prompts = reviewer.triage_prompts();
    assert_eq!(prompts.len(), 1);
    let (prompt, cwd) = &prompts[0];
    assert_eq!(cwd, Path::new(run.run_dir().unwrap()));
    for part in ["alert stalled", "idle_without_receipt", "send_instruction"] {
        assert!(prompt.contains(part), "{part}: {prompt}");
    }
    let texts = backend.texts();
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(
        texts[1]
            .1
            .contains("stop waiting for the tests and write the receipt"),
        "{texts:?}"
    );
    let repaired = payloads(&detail, "auto_repaired");
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert_eq!(repaired[0]["layer"], "recovery");
    assert_eq!(repaired[0]["repair"], "send_instruction");
    assert_eq!(repaired[0]["alert"], "stalled");
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["applied"], json!(["send_instruction"]));
    assert_eq!(finished[0]["escalated"], false);
    assert_eq!(finished[0]["reason"], "idle_without_receipt");
    assert!(stalled_asks(&queue).is_empty());
    assert_eq!(
        resolutions(&detail),
        [
            pair("nudge", "escalated"),
            pair("recovery", "resolved_by_recovery")
        ]
    );
}

/// Acceptance (4): a `repair` of low confidence is not applied: one
/// `stalled` ask opens with the job's diagnosis, its actions as the
/// recommendation, its options added and its reason category, and the
/// watch closes it once the session moves on.
#[test]
fn a_recovery_job_of_low_confidence_raises_one_stalled_ask_that_closes_when_the_session_moves() {
    let (_dir, repo, db) = fixture();
    let ((backend, _reviewer, supervisor), passes) = supervise_stalled_counted(
        &db,
        &repo,
        STALLED_AGENT,
        idle_short(),
        &[recovery(json!({
            "verdict": "repair",
            "confidence": "low",
            "diagnosis": "maybe the tests hang",
            "actions": [{"action": "stop_processes", "pids": [4242]}],
            "question": "Stop the tests and start them again?",
            "options": ["restart_tests"],
            "reason_category": "discard",
        }))],
    );
    wait_until(&db, Duration::from_secs(30), |queue| {
        !stalled_asks(queue).is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = stalled_asks(&queue).remove(0);
    assert_eq!(ask.reason_category, dagq::domain::AskReason::Discard);
    assert_eq!(ask.options[..3], ["wait", "intervene", "restart_tests"]);
    for part in [
        "reason: idle_without_receipt",
        "confidence low",
        "Diagnosis: maybe the tests hang",
        "\"pids\":[4242]",
        "Question: Stop the tests and start them again?",
        "recovery-stalled-1.prompt.txt",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    // Asked once, with no other job.
    thread::sleep(IDLE);
    await_passes(&passes, SOME_PASSES);
    assert_eq!(stalled_asks(&queue).len(), 1);
    release(&db, &backend, supervisor);
    let closed = queue.read_ask(ask.id).unwrap();
    assert_eq!(
        closed.answer.as_deref(),
        Some("the session moved on; closed by the runtime")
    );
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "recovery_requested").len(), 1);
    assert!(payloads(&detail, "auto_repaired").is_empty());
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["escalated"], true);
    assert_eq!(finished[0]["ask_id"], json!(ask.id));
    assert_eq!(finished[0]["reason_category"], "discard");
    assert_eq!(
        resolutions(&detail),
        [
            pair("nudge", "escalated"),
            pair("recovery", "escalated"),
            pair("ask", "resolved_by_itself")
        ]
    );
}

/// Acceptance (4): the recovery job's `wait` holds the alert, and past the
/// alert's three jobs the next look asks the inbox without a fourth.
#[test]
fn a_stall_past_its_three_recovery_jobs_is_asked_without_a_fourth() {
    let (_dir, repo, db) = fixture();
    let wait = repair(json!({"action": "wait", "recheck_after_secs": 1}), "slow");
    let (backend, reviewer, supervisor) = supervise_stalled(
        &db,
        &repo,
        STALLED_AGENT,
        idle_short(),
        &[wait.clone(), wait.clone(), wait],
    );
    wait_until(&db, Duration::from_secs(60), |queue| {
        !stalled_asks(queue).is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = stalled_asks(&queue).remove(0);
    assert!(
        ask.question
            .contains("the recovery job ran 3 times for this alert already"),
        "{}",
        ask.question
    );
    assert_eq!(ask.reason_category, dagq::domain::AskReason::RecoveryFailed);
    release(&db, &backend, supervisor);
    assert_eq!(reviewer.triage_prompts().len(), 3);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 3, "{requested:?}");
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished.len(), 4, "{finished:?}");
    assert!(
        finished[..3]
            .iter()
            .all(|f| f["applied"] == json!(["wait"]))
    );
    assert_eq!(finished[3]["ask_id"], json!(ask.id));
    let recoveries = resolutions(&detail)
        .into_iter()
        .filter(|(detection, _)| detection == "recovery")
        .count();
    assert_eq!(recoveries, 3);
}

/// Acceptance (4): a recovery job that prints no verdict opens the
/// `stalled` ask (task 442; every live alert opens its own ask since
/// ADR-t609-1), with no other job while it is open, and the ask
/// closes once the session moves on.
#[test]
fn a_failed_stalled_job_raises_the_stalled_ask() {
    let (_dir, repo, db) = fixture();
    let ((backend, reviewer, supervisor), passes) = supervise_stalled_counted(
        &db,
        &repo,
        STALLED_AGENT,
        idle_short(),
        &["printf 'no verdict here\\n'".to_owned()],
    );
    wait_until(&db, Duration::from_secs(30), |queue| {
        !stalled_asks(queue).is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = stalled_asks(&queue).remove(0);
    assert_eq!(ask.reason_category, dagq::domain::AskReason::RecoveryFailed);
    assert_eq!(ask.options[..2], ["wait", "intervene"]);
    for part in [
        "reason: idle_without_receipt",
        "the recovery job failed (",
        "Why a person: recovery_failed",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    thread::sleep(IDLE);
    await_passes(&passes, SOME_PASSES);
    assert_eq!(stalled_asks(&queue).len(), 1);
    assert_eq!(reviewer.triage_prompts().len(), 1);
    release(&db, &backend, supervisor);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(payloads(&detail, "recovery_failed").is_empty());
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["outcome"], "job_failed");
    assert_eq!(finished[0]["escalated"], true);
    assert_eq!(finished[0]["ask_id"], json!(ask.id));
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert_eq!(
        resolutions(&detail),
        [
            pair("nudge", "escalated"),
            pair("recovery", "escalated"),
            pair("ask", "resolved_by_itself")
        ]
    );
}

/// Acceptance (4): an explicit `escalate` opens the `stalled` ask with the
/// job's options, question and reason category.
#[test]
fn an_escalated_recovery_job_raises_the_stalled_ask_with_its_category() {
    let (_dir, repo, db) = fixture();
    let (backend, _reviewer, supervisor) = supervise_stalled(
        &db,
        &repo,
        STALLED_AGENT,
        idle_short(),
        &[recovery(json!({
            "verdict": "escalate",
            "confidence": "high",
            "diagnosis": "the session needs a wider scope",
            "question": "May it change the migrations too?",
            "options": ["widen_paths"],
            "reason_category": "scope",
        }))],
    );
    wait_until(&db, Duration::from_secs(30), |queue| {
        !stalled_asks(queue).is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = stalled_asks(&queue).remove(0);
    assert_eq!(ask.reason_category, dagq::domain::AskReason::Scope);
    assert_eq!(ask.options[..3], ["wait", "intervene", "widen_paths"]);
    for part in [
        "the recovery job could not repair it",
        "Diagnosis: the session needs a wider scope",
        "Question: May it change the migrations too?",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    release(&db, &backend, supervisor);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished[0]["verdict"], "escalate");
    assert_eq!(finished[0]["ask_id"], json!(ask.id));
    assert!(payloads(&detail, "auto_repaired").is_empty());
}

/// Acceptance (4): a `repair` of high confidence whose precondition does
/// not hold when it is applied (a pid that is not the run's) is not
/// applied, and opens the `stalled` ask saying why.
#[test]
fn a_repair_whose_precondition_fails_raises_the_stalled_ask() {
    let (_dir, repo, db) = fixture();
    let (backend, _reviewer, supervisor) = supervise_stalled(
        &db,
        &repo,
        STALLED_AGENT,
        idle_short(),
        &[repair(
            json!({"action": "stop_processes", "pids": [1]}),
            "init holds the session",
        )],
    );
    wait_until(&db, Duration::from_secs(30), |queue| {
        !stalled_asks(queue).is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = stalled_asks(&queue).remove(0);
    assert_eq!(ask.reason_category, dagq::domain::AskReason::RecoveryFailed);
    for part in [
        "the runtime did not apply the recovery job's repair",
        "pid 1 is not one of the run's own processes",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    release(&db, &backend, supervisor);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(payloads(&detail, "auto_repaired").is_empty());
    assert_eq!(
        payloads(&detail, "recovery_finished")[0]["ask_id"],
        json!(ask.id)
    );
}

/// The worker of the send tests: it commits, leaves an orphan `sleep` in its
/// worktree (its pid in `bg.pid` of the run directory), asks a question and
/// stops, and writes its receipt once the orphan is gone.
const ASKING_AGENT: &str = r#"
commit work
bg="$(dirname "$RECEIPT")/bg.pid"
( sleep 300 >/dev/null 2>&1 & echo $! > "$bg.tmp"; mv "$bg.tmp" "$bg" )
"$DAGQ" ask --run "$RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question 'Which word?' --cmux /usr/bin/true > /dev/null || exit 70
idle
pid=$(cat "$bg")
while kill -0 "$pid" 2>/dev/null; do sleep 0.05; done
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#;

/// Acceptance (2) and (3): the answer to the worker's question is lost
/// twice (`submit_not_started`), so the recovery job is requested
/// (`stalled`, reason `send_unconfirmed`) instead of an `answer_prompt`
/// ask; its `stop_processes` is applied and the session goes on.
#[test]
fn a_send_the_session_did_not_take_is_repaired_by_the_recovery_job() {
    let (_dir, repo, db) = fixture();
    let (backend, _reviewer, supervisor) = supervise_stalled(
        &db,
        &repo,
        ASKING_AGENT,
        dagq::domain::stall::StallConfig::default().with_millis("send_confirm_secs", IDLE_MS),
        &["printf '%s\\n' \"{\\\"verdict\\\": \\\"repair\\\", \\\"confidence\\\": \\\"high\\\", \\\"diagnosis\\\": \\\"an orphan holds the session\\\", \\\"actions\\\": [{\\\"action\\\": \\\"stop_processes\\\", \\\"pids\\\": [$(cat bg.pid)]}]}\"".to_owned()],
    );
    backend.dropped_texts.store(2, Ordering::SeqCst);
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(Default::default()).unwrap().is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let question = queue.asks(Default::default()).unwrap().remove(0);
    queue.answer(question.id, "blue").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let not_started = detail
        .events
        .iter()
        .find(|e| e.kind == "submit_not_started")
        .expect("submit_not_started");
    assert_eq!(not_started.payload["resent"], true);
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["alert"], "stalled");
    assert_eq!(requested[0]["reason"], "send_unconfirmed");
    assert_eq!(requested[0]["send_event"], json!(not_started.id));
    assert_eq!(
        requested[0]["send"],
        format!("answer of ask {}", question.id)
    );
    let repaired = payloads(&detail, "auto_repaired");
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert_eq!(repaired[0]["layer"], "recovery");
    assert_eq!(repaired[0]["repair"], "stop_processes");
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["applied"], json!(["stop_processes"]));
    assert_eq!(finished[0]["reason"], "send_unconfirmed");
    let asks = other_asks(&mut queue, true);
    assert_eq!(asks.len(), 1, "only the question: {asks:?}");
    let resolved = payloads(&detail, "stall_resolved");
    assert_eq!(resolved.len(), 1, "{resolved:?}");
    assert_eq!(resolved[0]["detection"], "recovery");
    assert_eq!(resolved[0]["outcome"], "resolved_by_recovery");
    assert_eq!(resolved[0]["threshold"], "send_confirm_secs");
    assert_eq!(resolved[0]["send_event"], json!(not_started.id));
}

/// Acceptance (5): the supervisor that nudged the session and whose
/// recovery job answered `wait` died: the adopter neither nudges nor starts
/// a job nor asks while the wait holds, and the session moving on ends the
/// job's detection.
#[test]
fn an_adopted_stall_whose_job_waits_gets_no_second_nudge_job_or_ask() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        r#"
commit work; idle_bg
while [ ! -f "$EXIT.go" ]; do sleep 0.05; done
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#,
    ));
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let marker = run.idle_marker_path().unwrap();
    wait_until(&db, Duration::from_secs(30), |_| marker.exists());
    thread::sleep(Duration::from_millis(IDLE_MS * 11 / 10));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let recheck_at_ms = (SystemTime::now() + Duration::from_secs(3600))
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    for (kind, payload) in [
        (
            EventKind::StallNudged,
            json!({"phase": "session", "idle_secs": 1, "threshold_secs": 1}),
        ),
        (
            EventKind::RecoveryRequested,
            json!({"alert": "stalled", "reason": "idle_without_receipt", "attempt": 1, "idle_secs": 1}),
        ),
        (
            EventKind::RecoveryFinished,
            json!({"alert": "stalled", "reason": "idle_without_receipt", "attempt": 1, "verdict": "repair", "confidence": "high", "applied": ["wait"], "escalated": false, "recheck_at_ms": recheck_at_ms}),
        ),
    ] {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
    age_lease(&db, &run, 31);
    let options = SuperviseOptions {
        stall: Some(idle_short()),
        ..supervise_options(4, true)
    };
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !adoption_events(&queue.show(TaskId::new(1)).unwrap()).is_empty()
    });
    // The idle was past the threshold before the adopter started: its
    // passes since are what would nudge, start a job or ask.
    await_passes(&passes, SOME_PASSES);
    assert!(backend.texts().is_empty(), "{:?}", backend.texts());
    assert!(stalled_asks(&queue).is_empty());
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "recovery_requested").len(), 1);
    release(&db, &backend, supervisor);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "stall_nudged").len(), 1);
    assert_eq!(payloads(&detail, "recovery_requested").len(), 1);
    assert_eq!(
        resolutions(&detail),
        [pair("recovery", "resolved_by_itself")]
    );
}

/// Acceptance (2) and (4) in the first session: the answer to the worker's
/// question is lost twice, its recovery job escalates, and the send becomes
/// the `stalled` ask (reason `send_unconfirmed`), never an `answer_prompt`
/// ask; the ask closes once the session moves on.
#[test]
fn a_send_the_job_cannot_repair_becomes_the_stalled_ask_in_the_first_session() {
    let (_dir, repo, db) = fixture();
    let (backend, _reviewer, supervisor) = supervise_stalled(
        &db,
        &repo,
        r#"
commit work
"$DAGQ" ask --run "$RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question 'Which word?' --cmux /usr/bin/true > /dev/null || exit 70
idle
while [ ! -f "$EXIT.go" ]; do sleep 0.05; done
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#,
        dagq::domain::stall::StallConfig::default().with_millis("send_confirm_secs", IDLE_MS),
        &[],
    );
    backend.dropped_texts.store(2, Ordering::SeqCst);
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(Default::default()).unwrap().is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let question = queue.asks(Default::default()).unwrap().remove(0);
    queue.answer(question.id, "blue").unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        !stalled_asks(queue).is_empty()
    });
    let ask = stalled_asks(&queue).remove(0);
    for part in [
        "reason: send_unconfirmed",
        &format!(
            "showed no sign of work within 1s of the answer of ask {} the supervisor sent twice",
            question.id
        ),
        "the recovery job could not repair it",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    assert_eq!(ask.options[..2], ["wait", "intervene"]);
    release(&db, &backend, supervisor);
    assert!(
        !other_asks(&mut queue, true)
            .iter()
            .any(|a| a.kind == AskKind::AnswerPrompt)
    );
    let closed = queue.read_ask(ask.id).unwrap();
    assert_eq!(
        closed.answer.as_deref(),
        Some("the session moved on; closed by the runtime")
    );
    let detail = queue.show(TaskId::new(1)).unwrap();
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["reason"], "send_unconfirmed");
    let resolved: Vec<(&Value, &Value, &Value)> = payloads(&detail, "stall_resolved")
        .into_iter()
        .map(|p| (&p["detection"], &p["outcome"], &p["threshold"]))
        .collect();
    assert_eq!(
        resolved,
        [
            (
                &json!("recovery"),
                &json!("escalated"),
                &json!("send_confirm_secs")
            ),
            (
                &json!("ask"),
                &json!("resolved_by_itself"),
                &json!("send_confirm_secs")
            ),
        ]
    );
}

/// ADR-0047 decision 40's `resume` for a stalled first session: the run is
/// parked as `needs_session` with the job's instruction
/// (`recovery_parked`), its session is asked to exit, and the supervisor
/// resumes it with the instruction in the resolution request; the resumed
/// session resolves it and the run lands, with no ask.
#[test]
fn a_stalled_session_the_job_resumes_is_parked_resumed_and_lands() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        r#"
commit work; idle_bg
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
rm -f "$MESSAGE"; idle
await_exit
"#,
    ));
    backend.resume_script_for(
        1,
        "await_message; cp \"$MESSAGE\" \"$MESSAGE.seen\"; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let (_reviewer, supervisor) = supervise_backend(
        &db,
        &repo,
        backend.clone(),
        idle_short(),
        &[repair(
            json!({"action": "resume", "instruction": "restart the hung tests in a fresh session"}),
            "the session hangs on its tests",
        )],
    );
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    let parked = payloads(&detail, "recovery_parked");
    assert_eq!(parked.len(), 1, "{parked:?}");
    assert_eq!(parked[0]["status"], "needs_session");
    assert_eq!(
        parked[0]["instruction"],
        "restart the hung tests in a fresh session"
    );
    let repaired = payloads(&detail, "auto_repaired");
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert_eq!(repaired[0]["repair"], "resume");
    assert_eq!(payloads(&detail, "resume_started").len(), 1);
    let seen = fs::read_to_string(
        resume_message_path(detail.runs[0].run_dir().unwrap()).with_extension("seen"),
    )
    .unwrap();
    assert!(
        seen.contains("restart the hung tests in a fresh session"),
        "{seen}"
    );
    assert!(stalled_asks(&queue).is_empty());
    assert!(
        !other_asks(&mut queue, true)
            .iter()
            .any(|a| a.kind == AskKind::AnswerPrompt)
    );
    assert_eq!(
        resolutions(&detail),
        [
            pair("nudge", "escalated"),
            pair("recovery", "resolved_by_recovery")
        ]
    );
}

/// Task 672: after its nudge the session ends a turn, then takes a
/// person's input whose turn is interrupted (Esc: no `Stop` hook, so no
/// idle marker follows the input marker). Past `idle_without_receipt_secs`
/// from the input, with the screen at rest, the idle goes to its recovery
/// job, and the session counts as at its prompt: the job's
/// `send_instruction` is applied, not refused for a session not idle.
#[test]
fn an_instruction_reaches_a_session_whose_interrupted_turn_left_no_idle_marker() {
    let (_dir, repo, db) = fixture();
    let (backend, _reviewer, supervisor) = supervise_stalled(
        &db,
        &repo,
        r#"
commit work; idle_bg
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
rm -f "$MESSAGE"; idle_bg
sleep 0.3
INPUT="$(dirname "$IDLE")/prompt-submit.json"
printf '{"hook_event_name":"UserPromptSubmit","prompt":"hold on"}' > "$INPUT.tmp"
mv "$INPUT.tmp" "$INPUT"
until grep -q "recovery job" "$MESSAGE" 2>/dev/null || [ -f "$EXIT.go" ]; do sleep 0.05; done
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#,
        idle_second(),
        &[repair(
            json!({"action": "send_instruction", "instruction": "write the receipt"}),
            "the person stopped the turn",
        )],
    );
    // Esc stopped the turn the input started: the screen is at rest again
    // (the nudge put it at work).
    let started = Instant::now();
    loop {
        let detail = SqliteQueue::open(&db)
            .unwrap()
            .show(TaskId::new(1))
            .unwrap();
        // A run is claimed before it is planned: its run directory comes
        // with the plan.
        if let Some(run) = detail.runs.first().filter(|run| run.run_dir().is_some()) {
            let input =
                Path::new(&run.idle_marker_path().unwrap()).with_file_name("prompt-submit.json");
            if input.exists() {
                break;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "no input taken"
        );
        thread::sleep(Duration::from_millis(20));
    }
    *backend.screen.lock().unwrap() = READY_SCREEN.into();
    wait_until(&db, Duration::from_secs(60), |queue| {
        !payloads(&queue.show(TaskId::new(1)).unwrap(), "recovery_finished").is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let applied = queue.show(TaskId::new(1)).unwrap();
    // Released either way, so a refused instruction fails here, not by
    // the supervisor's timeout.
    let outcome = release(&db, &backend, supervisor);
    assert_eq!(outcome["runs"][0]["status"], "integrated");
    let finished = payloads(&applied, "recovery_finished");
    assert_eq!(
        finished[0]["applied"],
        json!(["send_instruction"]),
        "{finished:?}"
    );
    let detail = queue.show(TaskId::new(1)).unwrap();
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["reason"], "idle_without_receipt");
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["applied"], json!(["send_instruction"]));
    assert_eq!(finished[0]["escalated"], false);
    let texts = backend.texts();
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(texts[1].1.contains("write the receipt"), "{texts:?}");
    assert!(stalled_asks(&queue).is_empty());
}
