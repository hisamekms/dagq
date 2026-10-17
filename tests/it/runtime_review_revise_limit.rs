//! Runtime tests: the run's revises counted across the run and the review
//! that sends the run back past them (ADR-t2599-1). Its reasons' codes
//! decide: with no code finding the run lands and the reasons become
//! follow_up drafts, otherwise a person decides in `approve_landing`, and
//! the review after a person's `send_back` is judged the same way. The
//! decisions are the unit tests' (`domain::review_reason`,
//! `domain::run::history`, `supervise::landing`); these keep the wiring.
use crate::runtime_review::revising_agent;
use crate::runtime_support;

use runtime_support::*;

/// A `revise` verdict script whose reasons carry codes: each `(text,
/// codes)`.
fn coded_revise(reasons: &[(&str, &[&str])]) -> String {
    let reasons: Vec<Value> = reasons
        .iter()
        .map(|(text, codes)| json!({"text": text, "codes": codes}))
        .collect();
    let json = json!({"verdict": "revise", "reasons": reasons, "summary": "not yet"});
    format!("printf '%s\\n' '{json}'")
}

/// A resumed session's turns: each (the resume's and every revise's)
/// appends a line to `change.txt`, commits it and rewrites the receipt.
const RESUMED_FIXES: &str = "await_message; printf 'resumed %s\\n' \"$TURN\" >> change.txt; unlocked git commit -q -am \"resumed $TURN\"; receipt \"$(git rev-parse HEAD)\"; idle; await_exit";

/// The one open ask of the queue, an `approve_landing`.
fn open_landing_ask(queue: &mut SqliteQueue) -> dagq::domain::Ask {
    let open: Vec<dagq::domain::Ask> = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .filter(|ask| ask.closed_at.is_none())
        .collect();
    assert_eq!(open.len(), 1, "{open:?}");
    let ask = open[0].clone();
    assert_eq!(ask.kind, dagq::domain::AskKind::ApproveLanding);
    assert_eq!(ask.options, ["land", "send_back", "cancel"]);
    ask
}

/// (2, 4) The revises are counted across the run, and past them a review
/// is judged by its reasons' codes after every resume. A person's
/// `send_back` resumes the run, and the resumed session gets the run's
/// second revise (`revise 2 of 2 of the run`), not the first of a new
/// count. The next review has a code finding past the limit: it asks a
/// person, naming the code findings left, and sends nothing to the session.
/// The review after that `send_back` is judged the same way without a
/// revise: with no code finding left, the run lands and its reason becomes
/// a follow_up draft.
#[test]
fn revises_are_counted_across_resumes_and_each_review_past_them_is_judged_by_its_codes() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &revising_agent(1));
    backend.resume_script_for(1, RESUMED_FIXES);
    let reviewer = TestReviewer::new(&[
        verdict("revise", &["still short"], "not yet"),
        verdict("concern", &["is the flag wanted"], "unsure"),
        verdict("revise", &["still short"], "not yet"),
        coded_revise(&[
            ("the loop never ends", &["local_slip", "code_defect"]),
            ("a typo", &["local_slip"]),
        ]),
        coded_revise(&[("a typo", &["local_slip"])]),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = open_landing_ask(&mut queue);
    assert!(
        ask.question.contains("returned concern"),
        "{}",
        ask.question
    );
    queue.answer(ask.id, "send_back").unwrap();
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let requested: Vec<&Value> = payloads(&detail, "revise_requested")
        .iter()
        .map(|p| &p["attempt"])
        .collect();
    assert_eq!(requested, [&json!(1), &json!(2)]);
    let kinds = event_kinds(&detail);
    let second = kinds
        .iter()
        .enumerate()
        .filter(|(_, kind)| **kind == "revise_requested")
        .map(|(at, _)| at)
        .nth(1)
        .unwrap();
    assert!(position(&kinds, "resume_started") < second, "{kinds:?}");
    let texts = session_texts(&run);
    assert!(
        texts
            .iter()
            .any(|text| text.contains("(revise 2 of 2 of the run)")),
        "{texts:?}"
    );
    assert_eq!(
        payloads(&detail, "revise_limit_judged"),
        [&json!({"attempt": 4, "revises": 2, "converged": false,
                 "code_findings": [{"text": "the loop never ends",
                                    "codes": ["local_slip", "code_defect"], "security": false}]})]
    );
    assert!(payloads(&detail, "follow_up_registered").is_empty());
    let ask = open_landing_ask(&mut queue);
    assert!(
        ask.question.contains(
            "(the review still asks for changes after the run's 2 revises, and code findings are left (local_slip, code_defect)): not yet"
        ),
        "{}",
        ask.question
    );
    assert!(
        ask.question.contains("\n- the loop never ends"),
        "{}",
        ask.question
    );

    queue.answer(ask.id, "send_back").unwrap();
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    assert_eq!(payloads(&detail, "resume_started").len(), 2);
    // No revise after the limit: the resume's review is judged at once.
    assert_eq!(payloads(&detail, "revise_requested").len(), 2);
    assert_eq!(payloads(&detail, "review_finished").len(), 5);
    let judged: Vec<&Value> = payloads(&detail, "revise_limit_judged")
        .iter()
        .map(|p| &p["converged"])
        .collect();
    assert_eq!(judged, [&json!(false), &json!(true)]);
    let registered: Vec<&Value> = payloads(&detail, "follow_up_registered");
    assert_eq!(registered.len(), 1, "{registered:?}");
    assert_eq!(registered[0]["review_attempt"], 5);
}

/// (3) Past the run's revises, a review whose every reason has only codes
/// of no code finding lands the run without an ask, through the e2e and
/// the landing like a pass, and each reason becomes a follow_up draft of
/// the category its first code maps to.
#[test]
fn a_review_past_the_limit_with_no_code_finding_lands_and_follows_up_its_reasons() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &revising_agent(2));
    let reviewer = TestReviewer::new(&[
        coded_revise(&[("add a line", &["acceptance_unmet"])]),
        coded_revise(&[("add a test", &["test_gap"])]),
        coded_revise(&[
            ("a stale sentence in docs/a.md", &["docs_drift"]),
            (
                "a line out of order",
                &["local_slip", "repo_rule_violation"],
            ),
        ]),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &base);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    assert_eq!(payloads(&detail, "revise_requested").len(), 2);
    assert_eq!(payloads(&detail, "review_finished").len(), 3);
    assert!(payloads(&detail, "integration_approved").is_empty());
    assert_eq!(
        payloads(&detail, "revise_limit_judged"),
        [&json!({"attempt": 3, "revises": 2, "converged": true, "code_findings": []})]
    );
    let kinds = event_kinds(&detail);
    for (earlier, later) in [
        ("follow_up_registered", "revise_limit_judged"),
        ("revise_limit_judged", "exit_requested"),
        ("exit_requested", "run_integrated"),
    ] {
        assert!(
            position(&kinds, earlier) < position(&kinds, later),
            "{earlier} before {later}: {kinds:?}"
        );
    }
    let registered: Vec<&Value> = payloads(&detail, "follow_up_registered")
        .into_iter()
        .filter(|p| p["from"] == "review")
        .collect();
    assert_eq!(registered.len(), 2, "{registered:?}");
    for (payload, (category, text)) in registered.iter().zip([
        ("docs_drift", "a stale sentence in docs/a.md"),
        ("improvement", "a line out of order"),
    ]) {
        assert_eq!(payload["review_attempt"], 3);
        let draft = queue
            .show(TaskId::new(payload["task_id"].as_i64().unwrap()))
            .unwrap()
            .task;
        assert_eq!(draft.status(), TaskStatus::Draft);
        assert_eq!(draft.title(), format!("{category}: {text}"));
        assert!(
            draft.description().starts_with(text),
            "{}",
            draft.description()
        );
        let (_, material) = queue.draft_origin(draft.id()).unwrap().unwrap();
        assert_eq!(material["category"], category);
        assert_eq!(material["source_run_id"], run.id().as_str());
    }
}

/// (2) A resume of a landing that stopped does not start the count again
/// either. Run 2's landing is parked by a rebase conflict with run 1's,
/// and run 3's by its verification failing after the rebase (its verify
/// needs a file only its resumed session adds). Each had revises before
/// its landing, recorded here (`revise_requested`, which is all the count
/// reads): one for run 2, two for run 3. Both were landed by the
/// supervisor, not approved by `integrate`, so after the resume each is
/// validated and reviewed again in its resumed session, and every review
/// has a code finding. Run 2 gets the run's second revise (`revise 2 of 2
/// of the run`) and then, past the limit, an `approve_landing` ask; run 3
/// gets no revise at all and its first review after the resume asks.
#[test]
fn a_resume_after_a_rebase_conflict_or_a_failed_landing_verification_keeps_the_runs_count() {
    warm_dagq();
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    let third = queue
        .add(NewTask {
            title: "third".into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f fixed.txt".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap();
    queue
        .transition(third.id(), TaskAction::BypassReview)
        .unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.script_for(
        3,
        "printf 'third\\n' > third.txt && git add third.txt && git commit -q -m third; receipt \"$(git rev-parse HEAD)\"",
    );
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let runs: Vec<TaskRun> = [2, 3]
        .into_iter()
        .map(|task| {
            let parked = integrate(&db, task, &repo).unwrap();
            assert_eq!(parked["outcome"], "needs_session", "{parked}");
            queue.show(TaskId::new(task)).unwrap().runs[0].clone()
        })
        .collect();
    let connection = Connection::open(&db).unwrap();
    for (run, earlier) in runs.iter().zip([1, 2]) {
        connection
            .execute(
                "DELETE FROM run_events WHERE run_id=?1 AND kind='integration_approved'",
                [&run.id()],
            )
            .unwrap();
        for attempt in 1..=earlier {
            queue
                .record_runtime_event(
                    run.id(),
                    dagq::domain::EventKind::ReviseRequested,
                    json!({"attempt": attempt, "reasons": ["an earlier finding"]}),
                )
                .unwrap();
        }
    }
    backend.resume_script_for(
        2,
        "await_message; resolve; printf 'resumed %s\\n' \"$TURN\" >> change.txt; unlocked git commit -q -am \"resumed $TURN\"; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    backend.resume_script_for(
        3,
        "await_message; printf 'resumed %s\\n' \"$TURN\" >> fixed.txt; git add fixed.txt; unlocked git commit -q -m \"resumed $TURN\"; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let reviewer = TestReviewer::new(&[coded_revise(&[("the loop never ends", &["code_defect"])])]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    for (task, code) in [(2, "rebase_conflict"), (3, "verification_failed")] {
        let detail = queue.show(TaskId::new(task)).unwrap();
        let run = detail.runs[0].clone();
        assert_eq!(run.status(), RunStatus::AwaitingIntegration, "{task}");
        assert_eq!(
            payloads(&detail, "integration_deferred")[0]["code"],
            code,
            "{task}"
        );
        assert_eq!(
            payloads(&detail, "resume_started").len(),
            1,
            "{task}: {:?}",
            event_kinds(&detail)
        );
        let requested: Vec<Value> = payloads(&detail, "revise_requested")
            .iter()
            .map(|p| p["attempt"].clone())
            .collect();
        assert_eq!(requested, [json!(1), json!(2)], "{task}");
        let judged: Vec<Value> = payloads(&detail, "revise_limit_judged")
            .iter()
            .map(|p| json!([p["revises"], p["converged"], p["code_findings"]]))
            .collect();
        assert_eq!(
            judged,
            [
                json!([2, false, [{"text": "the loop never ends", "codes": ["code_defect"], "security": false}]])
            ],
            "{task}"
        );
        let texts = session_texts(&run);
        assert_eq!(
            texts
                .iter()
                .any(|text| text.contains("(revise 2 of 2 of the run)")),
            task == 2,
            "{task}: {texts:?}"
        );
        let open: Vec<dagq::domain::Ask> = queue
            .asks(Default::default())
            .unwrap()
            .into_iter()
            .filter(|ask| ask.run_id.as_ref() == Some(run.id()) && ask.closed_at.is_none())
            .collect();
        assert_eq!(open.len(), 1, "{task}: {open:?}");
        assert_eq!(open[0].kind, dagq::domain::AskKind::ApproveLanding);
        assert!(
            open[0]
                .question
                .contains("after the run's 2 revises, and code findings are left (code_defect)"),
            "{}",
            open[0].question
        );
    }
}
