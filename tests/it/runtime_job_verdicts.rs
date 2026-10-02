//! Runtime tests: the headless review and recovery jobs of a run are
//! actors whose output is data (ADR-t728-1, ADR-t728-2). The supervisor
//! maps a verdict to a transition by its own rules: a review's `pass` only
//! lets the landing go on, which checks the run again itself, and a
//! recovery job's repair is applied only when it holds. An output the
//! runtime cannot read fails closed: nothing lands or is retried, and a
//! person is told.
use crate::runtime_support;
use dagq::domain::EventKind;

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
    // No job asked for it: the ask of a failed review is the supervisor's
    // own (task 735), and stays so (task 798).
    assert_eq!(
        actor_of(&detail, run, "ask_opened"),
        (
            "supervisor".to_owned(),
            format!("supervisor:{}", std::process::id()),
            None
        )
    );
}

/// The `approve_landing` ask a review's verdict leads to is opened after
/// the session's `/exit`, at the request of the review job that returned
/// the verdict (task 798): its `ask_opened` records the supervisor as the
/// actor and the job as `requested_by`, and nothing written after it (the
/// lease given back) carries the job. The ask is still asked by the
/// supervisor.
fn assert_asked_at_the_jobs_request(db: &Path, attempt: usize, supervisor: &str) {
    let mut queue = SqliteQueue::open(db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::ApproveLanding);
    assert_eq!(asks[0].asked_by, "supervisor");
    assert_eq!(
        actor_of(&detail, run, "ask_opened"),
        (
            "supervisor".to_owned(),
            supervisor.to_owned(),
            Some(format!("review-job:{}:{attempt}", run.id()))
        )
    );
    let opened = detail
        .events
        .iter()
        .position(|e| e.kind == "ask_opened")
        .unwrap();
    let after: Vec<_> = detail.events[opened + 1..]
        .iter()
        .filter(|e| e.actor.as_ref().is_some_and(|a| a.requested_by.is_some()))
        .map(|e| e.kind.as_str())
        .collect();
    assert!(after.is_empty(), "{after:?}");
    assert!(queue.run_leases().unwrap().is_empty());
}

/// A `concern` asks a person at the review job's request.
#[test]
fn a_concern_is_asked_at_the_review_jobs_request() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("concern", &["out of scope"], "unsure")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_asked_at_the_jobs_request(&db, 1, &format!("supervisor:{}", std::process::id()));
}

/// A `revise` past the revises left cannot be sent back: the third review's
/// job asks for the person.
#[test]
fn a_revise_past_its_limit_is_asked_at_the_review_jobs_request() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, &crate::runtime_review::revising_agent(2));
    let reviewer = TestReviewer::new(&[verdict("revise", &["still short"], "not yet")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_asked_at_the_jobs_request(&db, 3, &format!("supervisor:{}", std::process::id()));
}

/// A supervisor that takes over a run whose review returned a `concern`
/// before its ask was opened opens it at the same job's request, read
/// from the `attempt` of the `review_finished` it adopts.
#[test]
fn an_adopted_concern_is_asked_at_the_review_jobs_request() {
    let (_dir, repo, db) = fixture();
    let run = orphan_run(&repo, &db, "dead-supervisor", dead_pid(), dead_pid());
    crate::runtime_triage::validated_orphan(&db, &run);
    let mut queue = SqliteQueue::open(&db).unwrap();
    for (kind, payload) in [
        (
            EventKind::ReviewStarted,
            json!({"attempt": 2, "workspace_id": null, "session_live": false, "session_id": "s"}),
        ),
        (
            EventKind::ReviewFinished,
            json!({"verdict": "concern", "reasons": ["out of scope"], "summary": "unsure", "attempt": 2}),
        ),
    ] {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
    crate::runtime_triage::kill_supervisor_and_wrapper(&db, &run);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "unused")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(reviewer.prompts().is_empty());
    assert_eq!(
        adoption_events(&queue.show(TaskId::new(1)).unwrap()).len(),
        1
    );
    assert_asked_at_the_jobs_request(&db, 2, &format!("supervisor:{}", std::process::id()));
}

/// A recovery job's output the runtime cannot read retries nothing: the
/// failed run waits to be recovered by hand (`triage by hand`). One
/// representative here, a known `retry` under a field the runtime does not
/// know; every broken shape's error is
/// `domain::recovery::tests::a_broken_verdict_is_refused_with_what_is_wrong`
/// (task 1415). A repair of low confidence is not applied either, but
/// asked, at the job's request. (A repair of high confidence is applied,
/// at the job's request, in `runtime_actor_env`; one whose precondition
/// fails is asked in
/// `runtime_triage::a_retry_of_a_run_with_commits_is_refused_and_asked_with_the_jobs_options`.)
#[test]
fn a_recovery_verdict_is_applied_only_when_it_holds_and_a_broken_one_fails_closed() {
    let expected = "unknown field `force`";
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, "exit 7");
    let reviewer = TestReviewer::new(&[verdict_script()]).with_triages(&[recovery(
        json!({"verdict": "repair", "confidence": "high", "diagnosis": "x",
               "actions": [{"action": "retry"}], "force": true}),
    )]);
    supervise_reviewed(&db, &repo, &backend, &reviewer);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 1, "no retry");
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Failed);
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"triage_finished"), "{kinds:?}");
    assert!(!kinds.contains(&"auto_repaired"), "{kinds:?}");
    let failed = payloads(&detail, "triage_failed");
    assert_eq!(failed.len(), 1);
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

/// The broken output of a live run's recovery job each alert's test
/// gives, as (verdict, part of the error): a verdict with a field the
/// runtime does not know, carrying `action`, the repair the alert's test
/// would see applied if the verdict held (for `prompt_waiting` it would be
/// refused, as the runtime answers a known dialog before the alert: there
/// the guard is the `job_failed` outcome and its error, not the keys). The
/// other broken shapes (no JSON, no verdict, an unknown action) are read
/// alike, by `RecoveryVerdict::parse`, and their errors are
/// `domain::recovery::tests::a_broken_verdict_is_refused_with_what_is_wrong`;
/// what a failed job's ask and `recovery_finished` carry for every alert
/// is `supervise::recovery::tests::a_failed_job_escalates_with_recovery_failed_and_applies_nothing`
/// and the asks' options
/// `supervise::recovery::tests::each_live_alerts_ask_offers_its_own_options_then_the_jobs`
/// (task 1415).
fn broken_live_verdict(action: Value) -> (Value, &'static str) {
    (
        json!({"verdict": "repair", "confidence": "high", "diagnosis": "x",
               "actions": [action], "force": true}),
        "unknown field `force`",
    )
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

/// Task 797: a `long_background` alert's recovery job whose output the
/// runtime cannot read ([`broken_live_verdict`]) stops no process, even the
/// orphan its repair names: the `stalled` ask opens for the inbox with
/// `recovery_failed`, and nothing is applied.
#[test]
fn a_broken_long_background_recovery_verdict_stops_nothing_and_opens_the_stalled_ask() {
    let stop = json!({"action": "stop_processes", "pids": ["PID"]});
    let (output, expected) = broken_live_verdict(stop);
    let script = crate::runtime_repair::recovery_verdict(&output);
    let (_dir, repo, db) = fixture();
    let (backend, reviewer, supervisor) =
        crate::runtime_repair::supervise_long_background(&db, &repo, &script);
    // Checks that the orphan still runs when the ask is open.
    let (ask, detail) = crate::runtime_repair::escalated_long_background(&db, &backend, supervisor);
    assert_eq!(ask.kind, AskKind::Stalled);
    assert_eq!(ask.options, ["wait", "intervene", "propose"]);
    assert!(
        ask.question.contains("alert: long_background"),
        "{}",
        ask.question
    );
    assert_failed_live_job(&detail, &backend, &ask, "long_background", expected);
    assert_eq!(reviewer.triage_prompts().len(), 1);
}

/// Task 797: a `stuck_exit` alert's recovery job whose output the runtime
/// cannot read ([`broken_live_verdict`]) does not close the workspace and
/// land the run (its `close_and_proceed` would hold) and sends no `/exit`
/// or key: the `stuck_exit` ask opens with `recovery_failed`, and the run
/// lands only once the session exits.
#[test]
fn a_broken_stuck_exit_recovery_verdict_closes_nothing_and_opens_the_stuck_exit_ask() {
    let (output, expected) = broken_live_verdict(json!({"action": "close_and_proceed"}));
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.close_ends_session = true;
    let backend = Arc::new(backend);
    let run = crate::runtime_review_exit::stuck_exit_after_a_pass(&repo, &db, &backend);
    let (reviewer, supervisor) =
        crate::runtime_review_exit::supervise_recovering(&db, &repo, &backend, recovery(output));
    let ask = crate::runtime_review_exit::failed_stuck_exit_job_ask(&db, &run, expected);
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_failed_live_job(
        &queue.show(TaskId::new(1)).unwrap(),
        &backend,
        &ask,
        "stuck_exit",
        expected,
    );
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    assert!(backend.closed().is_empty(), "{:?}", backend.closed());
    assert!(backend.texts.lock().unwrap().is_empty());
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), base);
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
    assert_eq!(reviewer.triage_prompts().len(), 1);
    assert!(payloads(&queue.show(TaskId::new(1)).unwrap(), "auto_repaired").is_empty());
}

/// Task 797: a `prompt_waiting` alert's recovery job whose output the
/// runtime cannot read ([`broken_live_verdict`]) answers no dialog (no
/// key, no text): the `answer_prompt` ask opens with `recovery_failed`
/// and closes once the dialog is gone.
#[test]
fn a_broken_prompt_waiting_recovery_verdict_answers_nothing_and_opens_the_answer_prompt_ask() {
    let answer = json!({"action": "answer_known_dialog", "dialog": "background_work"});
    let (output, expected) = broken_live_verdict(answer);
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, PROMPTED_AGENT);
    backend.prompt_wait = Duration::from_millis(300);
    *backend.screen.lock().unwrap() = crate::runtime_review_adopt::DIALOG_SCREEN.into();
    let backend = Arc::new(backend);
    let reviewer = Arc::new(
        TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")])
            .with_triages(&[recovery(output)]),
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
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].clone();
    assert_eq!(ask.kind, AskKind::AnswerPrompt);
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
    assert!(backend.texts.lock().unwrap().is_empty());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
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
    assert_eq!(reviewer.triage_prompts().len(), 1);
    assert!(payloads(&queue.show(TaskId::new(1)).unwrap(), "auto_repaired").is_empty());
}
