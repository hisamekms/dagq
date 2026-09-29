//! Runtime tests: the headless review and recovery jobs of a run are
//! actors whose output is data (ADR-t728-1, ADR-t728-2). The supervisor
//! maps a verdict to a transition by its own rules: a review's `pass` only
//! lets the landing go on, which checks the run again itself, and a
//! recovery job's repair is applied only when it holds. An output the
//! runtime cannot read fails closed: nothing lands or is retried, and a
//! person is told.
use crate::runtime_support;

use runtime_support::*;

fn set_commands(db: &Path, commands: Value) {
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE tasks SET verification_commands=?1 WHERE id=1",
            [commands.to_string()],
        )
        .unwrap();
}

/// The actor of the first event of `kind` of `run`.
fn actor_of(
    detail: &dagq::domain::TaskDetail,
    run: &TaskRun,
    kind: &str,
) -> (String, String, Option<String>) {
    let event = detail
        .events
        .iter()
        .find(|e| e.kind == kind && e.run_id.as_ref() == Some(run.id()))
        .unwrap_or_else(|| panic!("no {kind} of {}", run.id()));
    let actor = event.actor.clone().expect("an actor");
    (actor.role, actor.id, actor.requested_by)
}

/// A review that passes does not land the run: the landing runs the
/// verification itself after it, and a verification that fails parks the
/// run for a session instead. The pass is recorded at the review job's
/// request; the supervisor asks the Integrator to land it, and the
/// landing is the Integrator's at the supervisor's request.
#[test]
fn a_passing_review_does_not_land_a_run_its_landing_does_not_verify() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    set_commands(&db, json!(["false"]));
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    supervise_reviewed(&db, &repo, &backend, &reviewer);
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::NeedsSession);
    assert_ne!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), base);
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"run_integrated"), "{kinds:?}");
    assert_eq!(payloads(&detail, "review_finished")[0]["verdict"], "pass");
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 1, "{kinds:?}");
    let code = verifications[0]["exit_code"]
        .as_i64()
        .expect("an exit code");
    assert_ne!(code, 0);
    assert!(position(&kinds, "review_finished") < position(&kinds, "integration_started"));

    let supervisor = format!("supervisor:{}", std::process::id());
    assert_eq!(
        actor_of(&detail, run, "review_finished"),
        (
            "supervisor".to_owned(),
            supervisor.clone(),
            Some(format!("review-job:{}:1", run.id()))
        )
    );
    // The supervisor takes the slot and asks; the Integrator lands, at the
    // supervisor's request (ADR-t728-2).
    assert_eq!(
        actor_of(&detail, run, "integration_started"),
        ("supervisor".to_owned(), supervisor.clone(), None)
    );
    for kind in ["verification_command", "integration_deferred"] {
        assert_eq!(
            actor_of(&detail, run, kind),
            (
                "integrator".to_owned(),
                format!("integrator:{}", std::process::id()),
                Some(supervisor.clone())
            ),
            "{kind}"
        );
    }
}

/// A review verdict with a field the runtime does not know is no verdict:
/// reviewed once more, then the run waits for a person in its
/// `approve_landing` ask, and nothing lands.
#[test]
fn a_review_verdict_with_an_unknown_field_asks_a_person_and_lands_nothing() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let json = json!({"verdict": "pass", "reasons": [], "summary": "ok", "land": true});
    let reviewer = TestReviewer::new(&[format!("printf '%s\\n' '{json}'")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), base);
    assert_eq!(reviewer.prompts().len(), 2);
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"review_finished"), "{kinds:?}");
    assert!(!kinds.contains(&"integration_started"), "{kinds:?}");
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1);
    let error = failed[0]["error"].as_str().unwrap();
    assert!(error.contains("unknown field `land`"), "{error}");
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::ApproveLanding);
    assert_eq!(asks[0].run_id.as_ref(), Some(run.id()));
}

/// A recovery job's output the runtime cannot read, or one with a field or
/// an action it does not know, retries nothing: the failed run waits to be
/// recovered by hand (`triage by hand`). A repair of low confidence is not
/// applied either, but asked, at the job's request. (A repair of high
/// confidence is applied, at the job's request, in
/// `runtime_actor_env`; one whose precondition fails is asked in
/// `runtime_triage::a_retry_of_a_run_with_commits_is_refused_and_asked_with_the_jobs_options`.)
#[test]
fn a_recovery_verdict_is_applied_only_when_it_holds_and_a_broken_one_fails_closed() {
    for (verdict, expected) in [
        (
            "printf 'no verdict here\\n'".to_owned(),
            "printed no verdict JSON",
        ),
        (
            recovery(
                json!({"verdict": "repair", "confidence": "high", "diagnosis": "x",
                            "actions": [{"action": "retry"}], "force": true}),
            ),
            "unknown field `force`",
        ),
        (
            recovery(
                json!({"verdict": "repair", "confidence": "high", "diagnosis": "x",
                            "actions": [{"action": "integrate"}]}),
            ),
            "unknown variant `integrate`",
        ),
    ] {
        let (_dir, repo, db) = fixture();
        let backend = TestWorkspace::new(&db, false, "exit 7");
        let reviewer = TestReviewer::new(&[verdict_script()]).with_triages(&[verdict]);
        supervise_reviewed(&db, &repo, &backend, &reviewer);
        let mut queue = SqliteQueue::open(&db).unwrap();
        let detail = queue.show(TaskId::new(1)).unwrap();
        assert_eq!(detail.runs.len(), 1, "{expected}: no retry");
        let run = &detail.runs[0];
        assert_eq!(run.status(), RunStatus::Failed);
        let kinds = event_kinds(&detail);
        assert!(!kinds.contains(&"triage_finished"), "{kinds:?}");
        assert!(!kinds.contains(&"auto_repaired"), "{kinds:?}");
        let failed = payloads(&detail, "triage_failed");
        assert_eq!(failed.len(), 1, "{expected}");
        let error = failed[0]["error"].as_str().unwrap();
        assert!(error.contains(expected), "{error}");
        let status = runtime::status(&db).unwrap();
        assert!(
            status["attention"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["kind"] == "triage_failed" && a["next"] == "triage by hand"),
            "{status}"
        );
        assert!(queue.asks(Default::default()).unwrap().is_empty());
    }

    // A repair of low confidence is the job's recommendation to a person.
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, "exit 7");
    let reviewer = TestReviewer::new(&[verdict_script()]).with_triages(&[recovery(json!({
        "verdict": "repair", "confidence": "low", "diagnosis": "maybe transient",
        "actions": [{"action": "retry"}],
    }))]);
    supervise_reviewed(&db, &repo, &backend, &reviewer);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 1, "not retried");
    let run = &detail.runs[0];
    assert!(!event_kinds(&detail).contains(&"auto_repaired"));
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::Decide);
    // The escalation is recorded at the job's request.
    assert_eq!(
        actor_of(&detail, run, "triage_finished"),
        (
            "supervisor".to_owned(),
            format!("supervisor:{}", std::process::id()),
            Some(format!("recovery-job:{}:failed:1", run.id()))
        )
    );
}

fn verdict_script() -> String {
    verdict("pass", &[], "unused")
}

/// The broken outputs of a live run's recovery job, as (verdict, part of
/// the error): no JSON at all, JSON without a verdict, a verdict with a
/// field the runtime does not know, and one with an action it does not
/// know. The two verdicts with a known shape carry `action`, the repair
/// the alert's test would see applied if the verdict held (for
/// `prompt_waiting` it would be refused, as the runtime answers a known
/// dialog before the alert: there the guard is the `job_failed` outcome
/// and its error, not the keys).
fn broken_live_verdicts(action: Value) -> [(Option<Value>, &'static str); 4] {
    [
        (None, "the recovery job printed no verdict JSON"),
        (
            Some(json!({"confidence": "high", "diagnosis": "x", "actions": [action.clone()]})),
            "missing field `verdict`",
        ),
        (
            Some(
                json!({"verdict": "repair", "confidence": "high", "diagnosis": "x",
                        "actions": [action], "force": true}),
            ),
            "unknown field `force`",
        ),
        (
            Some(
                json!({"verdict": "repair", "confidence": "high", "diagnosis": "x",
                        "actions": [{"action": "kill_session"}]}),
            ),
            "unknown variant `kill_session`",
        ),
    ]
}

/// What a live run's broken recovery job leaves (ADR-t609-1): its
/// `recovery_finished` is an escalation of the failed job with nothing
/// applied, naming `ask`, whose reason is `recovery_failed`; nothing was
/// repaired and no key was sent.
fn assert_failed_live_job(
    detail: &dagq::domain::TaskDetail,
    backend: &TestWorkspace,
    ask: &dagq::domain::Ask,
    alert: &str,
    expected: &str,
) {
    assert_eq!(ask.reason_category, dagq::domain::AskReason::RecoveryFailed);
    for part in [
        "the recovery job failed (",
        expected,
        "Why a person: recovery_failed",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    let finished = payloads(detail, "recovery_finished");
    assert_eq!(finished.len(), 1, "{expected}: {finished:?}");
    assert_eq!(finished[0]["alert"], alert);
    assert_eq!(finished[0]["escalated"], true);
    assert_eq!(finished[0]["outcome"], "job_failed");
    assert_eq!(finished[0]["applied"], json!([]));
    assert_eq!(finished[0]["reason_category"], "recovery_failed");
    assert_eq!(finished[0]["ask_id"], json!(ask.id));
    let error = finished[0]["error"].as_str().unwrap();
    assert!(error.contains(expected), "{error}");
    assert!(payloads(detail, "auto_repaired").is_empty(), "{expected}");
    assert!(backend.keys.lock().unwrap().is_empty(), "{expected}");
}

/// Task 797: a `long_background` alert's recovery job whose output is no
/// verdict, or one with a field or an action the runtime does not know,
/// stops no process, even the orphan its repair names: the `stalled` ask
/// opens for the inbox with `recovery_failed`, and nothing is applied.
#[test]
fn a_broken_long_background_recovery_verdict_stops_nothing_and_opens_the_stalled_ask() {
    let stop = json!({"action": "stop_processes", "pids": ["PID"]});
    for (output, expected) in broken_live_verdicts(stop) {
        let script = output.map_or_else(
            || "printf 'no verdict here\\n'".to_owned(),
            |v| crate::runtime_repair::recovery_verdict(&v),
        );
        let (_dir, repo, db) = fixture();
        let (backend, reviewer, supervisor) =
            crate::runtime_repair::supervise_long_background(&db, &repo, &script);
        // Checks that the orphan still runs when the ask is open.
        let (ask, detail) =
            crate::runtime_repair::escalated_long_background(&db, &backend, supervisor);
        assert_eq!(ask.kind, AskKind::Stalled, "{expected}");
        assert_eq!(ask.options, ["wait", "intervene", "propose"], "{expected}");
        assert!(
            ask.question.contains("alert: long_background"),
            "{}",
            ask.question
        );
        assert_failed_live_job(&detail, &backend, &ask, "long_background", expected);
        assert_eq!(reviewer.triage_prompts().len(), 1, "{expected}");
    }
}

/// Task 797: a `stuck_exit` alert's recovery job whose output is no
/// verdict, or one with a field or an action the runtime does not know,
/// does not close the workspace and land the run (its `close_and_proceed`
/// would hold) and sends no `/exit` or key: the `stuck_exit` ask opens
/// with `recovery_failed`, and the run lands only once the session exits.
#[test]
fn a_broken_stuck_exit_recovery_verdict_closes_nothing_and_opens_the_stuck_exit_ask() {
    for (output, expected) in broken_live_verdicts(json!({"action": "close_and_proceed"})) {
        let script = output.map_or_else(|| "printf 'no verdict here\\n'".to_owned(), recovery);
        let (_dir, repo, db) = fixture();
        let base = git_out(&repo, &["rev-parse", "main"]);
        let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
        backend.close_ends_session = true;
        let backend = Arc::new(backend);
        let run = crate::runtime_review_exit::stuck_exit_after_a_pass(&repo, &db, &backend);
        let (reviewer, supervisor) =
            crate::runtime_review_exit::supervise_recovering(&db, &repo, &backend, script);
        let ask = crate::runtime_review_exit::failed_stuck_exit_job_ask(&db, &run, expected);
        let mut queue = SqliteQueue::open(&db).unwrap();
        assert_failed_live_job(
            &queue.show(TaskId::new(1)).unwrap(),
            &backend,
            &ask,
            "stuck_exit",
            expected,
        );
        assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0, "{expected}");
        assert!(
            backend.closed().is_empty(),
            "{expected}: {:?}",
            backend.closed()
        );
        assert!(backend.texts.lock().unwrap().is_empty(), "{expected}");
        assert_eq!(git_out(&repo, &["rev-parse", "main"]), base, "{expected}");
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
        assert_eq!(reviewer.triage_prompts().len(), 1, "{expected}");
        assert!(payloads(&queue.show(TaskId::new(1)).unwrap(), "auto_repaired").is_empty());
    }
}

/// Task 797: a `prompt_waiting` alert's recovery job whose output is no
/// verdict, or one with a field or an action the runtime does not know,
/// answers no dialog (no key, no text): the `answer_prompt` ask opens
/// with `recovery_failed` and closes once the dialog is gone.
#[test]
fn a_broken_prompt_waiting_recovery_verdict_answers_nothing_and_opens_the_answer_prompt_ask() {
    let answer = json!({"action": "answer_known_dialog", "dialog": "background_work"});
    for (output, expected) in broken_live_verdicts(answer) {
        let script = output.map_or_else(|| "printf 'no verdict here\\n'".to_owned(), recovery);
        let (_dir, repo, db) = fixture();
        let mut backend = TestWorkspace::new(&db, false, PROMPTED_AGENT);
        backend.prompt_wait = Duration::from_millis(300);
        *backend.screen.lock().unwrap() = crate::runtime_review_adopt::DIALOG_SCREEN.into();
        let backend = Arc::new(backend);
        let reviewer = Arc::new(
            TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")])
                .with_triages(&[script]),
        );
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
        let open_asks = |queue: &SqliteQueue| queue.asks(AskQuery::default()).unwrap();
        // The ask opens before the job's `recovery_finished` is recorded:
        // both are waited for.
        wait_until(&db, crate::common::STEP_LIMIT, |queue| {
            !open_asks(queue).is_empty()
                && !payloads(&queue.show(TaskId::new(1)).unwrap(), "recovery_finished").is_empty()
        });
        let mut queue = SqliteQueue::open(&db).unwrap();
        let asks = open_asks(&queue);
        assert_eq!(asks.len(), 1, "{expected}: {asks:?}");
        let ask = asks[0].clone();
        assert_eq!(ask.kind, AskKind::AnswerPrompt, "{expected}");
        assert!(
            ask.question.contains("waits at a choice dialog"),
            "{}",
            ask.question
        );
        assert_failed_live_job(
            &queue.show(TaskId::new(1)).unwrap(),
            &backend,
            &ask,
            "prompt_waiting",
            expected,
        );
        assert!(backend.texts.lock().unwrap().is_empty(), "{expected}");
        assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0, "{expected}");
        // Someone answers the dialog: the ask closes, and the run goes on.
        *backend.screen.lock().unwrap() = WORK_SCREEN.into();
        wait_until(&db, crate::common::STEP_LIMIT, |queue| {
            queue.read_ask(ask.id).unwrap().closed_at.is_some()
        });
        let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
        fs::write(
            exit_request_path(run.run_dir().unwrap()).with_extension("go"),
            "",
        )
        .unwrap();
        let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
        backend.join();
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
        assert_eq!(reviewer.triage_prompts().len(), 1, "{expected}");
        assert!(payloads(&queue.show(TaskId::new(1)).unwrap(), "auto_repaired").is_empty());
    }
}
