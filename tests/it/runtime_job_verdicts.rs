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
/// request; the landing is the supervisor's own, requested by no job.
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
    for kind in ["integration_started", "verification_command"] {
        assert_eq!(
            actor_of(&detail, run, kind),
            ("supervisor".to_owned(), supervisor.clone(), None),
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
