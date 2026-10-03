//! Runtime tests: The review job of a run, its revise and concern, and the
//! landing asks it opens.
use crate::runtime_support;

use runtime_support::*;

/// The worker goes idle after its receipt and never exits by itself; each
/// time a text arrives in its terminal it appends a line, commits, rewrites
/// the receipt and goes idle again, `revises` times.
pub(crate) fn revising_agent(revises: usize) -> String {
    format!(
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; \
         for n in $(seq 1 {revises}); do \
           while [ ! -f \"$MESSAGE\" ]; do sleep 0.1; done; rm \"$MESSAGE\"; \
           printf 'fix %s\\n' \"$n\" >> change.txt; git commit -q -am \"fix $n\"; \
           receipt \"$(git rev-parse HEAD)\"; idle; \
         done; await_exit"
    )
}

/// A receipt accepted with the session still open is reviewed before the
/// session is asked to exit (ADR-0027 decision 1); on `pass` the supervisor
/// sends `/exit`, closes the workspace and lands the run without anyone
/// calling `integrate`.
#[test]
fn a_passing_review_exits_the_live_session_and_lands_it() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &base);
    assert!(queue.run_leases().unwrap().is_empty());
    let kinds = event_kinds(&detail);
    // The session lives through validation and review; /exit comes after
    // the verdict, the landing after the close.
    for (earlier, later) in [
        ("session_idle_observed", "supervision_finished"),
        ("supervision_finished", "validation_finished"),
        ("validation_finished", "review_started"),
        ("review_started", "review_finished"),
        ("review_finished", "exit_requested"),
        ("exit_requested", "session_exited"),
        ("session_exited", "workspace_closed"),
        ("workspace_closed", "integration_started"),
        ("integration_started", "run_integrated"),
    ] {
        assert!(
            position(&kinds, earlier) < position(&kinds, later),
            "{earlier} before {later}: {kinds:?}"
        );
    }
    assert!(!kinds.contains(&"integration_approved"), "{kinds:?}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
    let supervised = payloads(&detail, "supervision_finished");
    assert_eq!(
        supervised[0],
        &json!({"status": "validating", "exit_code": null, "session_live": true})
    );
    let started = payloads(&detail, "review_started");
    assert_eq!(started.len(), 1);
    // The job's own session id (ADR-0048 decision 4).
    let session_id = started[0]["session_id"].as_str().unwrap();
    assert_eq!(
        started,
        [
            &json!({"attempt": 1, "workspace_id": WORKSPACE_ID, "session_live": true, "session_id": session_id,
                    "launch": {"role": "review", "provider": "claude", "model": null, "effort": null, "source": "default"}})
        ]
    );
    // The worker's session and the review's are spans, each closed once
    // (ADR-0048 decision 2).
    let opened: Vec<(&str, &str)> = payloads(&detail, "session_opened")
        .iter()
        .map(|p| {
            (
                p["kind"].as_str().unwrap(),
                p["session_id"].as_str().unwrap(),
            )
        })
        .collect();
    let run_id = detail.runs[0].id().as_str();
    assert_eq!(opened, [("worker", run_id), ("review", session_id)]);
    let closed: Vec<(&str, &str)> = payloads(&detail, "session_closed")
        .iter()
        .map(|p| (p["kind"].as_str().unwrap(), p["reason"].as_str().unwrap()))
        .collect();
    assert_eq!(closed, [("review", "job_finished"), ("worker", "exited")]);
    // No transcript of either session exists: their active time is not
    // recorded, and the run lands all the same (ADR-0048 decision 10).
    for closed in payloads(&detail, "session_closed") {
        assert_eq!(closed["active"], "unavailable", "{closed}");
        assert_eq!(
            closed["active_unavailable"], "transcript_missing",
            "{closed}"
        );
    }
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["verdict"], "pass");
    assert_eq!(finished[0]["reasons"], json!([]));
    assert_eq!(finished[0]["summary"], "meets the acceptance");
    assert_eq!(finished[0]["attempt"], 1);
    assert!(finished[0]["duration_secs"].is_u64());
    // The reviewer reads review.md and is told the acceptance, the schema
    // and the verdicts.
    let prompts = reviewer.prompts();
    assert_eq!(prompts.len(), 1);
    let run_dir = Path::new(run.run_dir().unwrap());
    let review_md = run_dir.join("review.md");
    assert!(review_md.is_file());
    for expected in [
        format!("Read the review material at {}", review_md.display()),
        "Acceptance criteria of the task:\nworks".to_owned(),
        r#"{"verdict": "pass" | "revise" | "concern", "reasons": [{"text": string, "codes": [string]}], "summary": string, "recommendation": "land" | "send_back" | null, "confidence": "high" | "low" | null, "reason_category": "scope" | "discard" | null}"#
            .to_owned(),
        "- revise: findings the worker can fix without a person's judgment".to_owned(),
        "- concern: findings that call for a judgment rather than a mechanical fix".to_owned(),
        "the runtime applies a sure judgment that needs no person (high, reason_category null) itself".to_owned(),
    ] {
        assert!(
            prompts[0].contains(&expected),
            "{expected:?} not in {}",
            prompts[0]
        );
    }
    assert_eq!(
        fs::read_to_string(run_dir.join("review-prompt-1.txt")).unwrap(),
        prompts[0]
    );
    assert!(run_dir.join("terminal-final.txt").is_file());
    // Nothing waits for anyone.
    assert!(run_attention_of(&runtime::status(&db).unwrap(), run.id()).is_none());
}

/// A `revise` verdict goes to the live session as a fixed request; once
/// the session rewrote its receipt for a new head and went idle, the run is
/// validated and reviewed again, and lands on `pass` (ADR-0027 decision 2).
#[test]
fn a_revise_verdict_is_fixed_by_the_live_session_and_reviewed_again() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &revising_agent(1));
    let reviewer = TestReviewer::new(&[
        verdict("revise", &["add a line to change.txt"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &base);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        format!("change by {}\nfix 1\n", run.id())
    );
    let requested = payloads(&detail, "revise_requested");
    assert_eq!(requested.len(), 1);
    assert_eq!(requested[0]["attempt"], 1);
    assert_eq!(requested[0]["reasons"], json!(["add a line to change.txt"]));
    // Switched one step up before the request (ADR-0079 decision 5).
    crate::worker_escalation::assert_raised_by_revise(requested[0], &backend, &db);
    let revised = payloads(&detail, "revise_finished");
    assert_eq!(revised.len(), 1);
    let head = git_out(
        &repo,
        &["rev-parse", &format!("refs/dagq/runs/{}", run.id())],
    );
    assert_eq!(revised[0], &json!({"attempt": 1, "head": head}));
    let verdicts: Vec<&Value> = payloads(&detail, "review_finished")
        .iter()
        .map(|p| &p["verdict"])
        .collect();
    assert_eq!(verdicts, [&json!("revise"), &json!("pass")]);
    assert_eq!(payloads(&detail, "validation_finished").len(), 2);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "revise_requested") < position(&kinds, "revise_finished"));
    assert!(position(&kinds, "revise_finished") < position(&kinds, "exit_requested"));
    // One /exit, after the second review; the request named the findings.
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let texts = backend.texts();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0].0, WORKSPACE_ID);
    let text = &texts[0].1;
    for expected in [
        format!(
            "dagq: the supervisor's review of run {} (task 1) asks for changes (revise 1 of 2).",
            run.id()
        ),
        "Findings:\n- add a line to change.txt".to_owned(),
        "[\"test -f seed.txt\"]".to_owned(),
        format!(
            "Rewrite the receipt at {} with the new head commit",
            run.receipt_path().unwrap()
        ),
        runtime::STOP_BACKGROUND.to_owned(),
        "Do not merge or push. When done, report briefly and stop; do not run /exit.".to_owned(),
    ] {
        assert!(text.contains(&expected), "{expected:?} not in {text}");
    }
    let run_dir = Path::new(run.run_dir().unwrap());
    assert_eq!(
        &fs::read_to_string(run_dir.join("revise-1.txt")).unwrap(),
        text
    );
}

/// Revise is sent at most twice; a third review that does not pass becomes
/// an `approve_landing` ask after `/exit` and the close. Here the third is
/// a `concern` whose job recommends `send_back` with high confidence: past
/// the round's two revises it is not applied but asked, with the job's
/// recommendation (ADR-t451-1 decision 3). `land` then lands the run as an
/// approved one. A third `revise` asks the same way
/// (`runtime_job_verdicts::a_revise_past_its_limit_is_asked_at_the_review_jobs_request`,
/// its text `landing::tests::a_revise_past_its_limit_asks_with_the_count_of_revises`).
#[test]
fn a_third_review_that_does_not_pass_asks_a_person_and_land_lands_it() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &revising_agent(2));
    let send_back = json!({"verdict": "concern", "reasons": ["still short"], "summary": "not yet",
                           "recommendation": "send_back", "confidence": "high", "reason_category": null});
    let reviewer = TestReviewer::new(&[
        verdict("revise", &["still short"], "not yet"),
        verdict("revise", &["still short"], "not yet"),
        format!("printf '%s\\n' '{send_back}'"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(payloads(&detail, "revise_requested").len(), 2);
    crate::worker_escalation::assert_revises_raised(&detail, &backend);
    assert_eq!(payloads(&detail, "revise_finished").len(), 2);
    assert_eq!(payloads(&detail, "review_finished").len(), 3);
    assert_eq!(backend.texts().len(), 2);
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
    assert!(queue.run_leases().unwrap().is_empty());
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "workspace_closed") < position(&kinds, "ask_opened"));
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    let ask = &asks[0];
    assert_eq!(ask.kind, dagq::domain::AskKind::ApproveLanding);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert_eq!(ask.options, ["land", "send_back", "cancel"]);
    assert_eq!(ask.asked_by, "supervisor");
    assert_eq!(
        payloads(&detail, "concern_decided"),
        [
            &json!({"attempt": 3, "recommendation": "send_back", "confidence": "high",
                 "reason_category": null, "applied": false, "escalated_because": "revise_limit"})
        ]
    );
    assert_eq!(ask.recommendation.as_deref(), Some("send_back"));
    assert_eq!(ask.confidence, Some(dagq::domain::AskConfidence::High));
    assert!(
        ask.question.contains(
            "returned concern (the review recommends send_back after 2 revises): not yet"
        ),
        "{}",
        ask.question
    );
    assert!(ask.question.contains("\n- still short"), "{}", ask.question);
    // The ask is the attention, for the inbox; the run itself is not one.
    let status = runtime::status(&db).unwrap();
    assert!(
        run_attention_of(&status, run.id()).is_none() || {
            let entries: Vec<&Value> = status["attention"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|a| a["run_id"] == json!(run.id()))
                .collect();
            entries.iter().all(|a| a["ask_id"] == json!(ask.id))
        }
    );
    let entry = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["ask_id"] == json!(ask.id))
        .unwrap();
    assert_eq!(entry["next"], format!("answer ask {}", ask.id));

    queue.answer(ask.id, "land").unwrap();
    let status = runtime::status(&db).unwrap();
    let entry = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["ask_id"] == json!(ask.id))
        .unwrap();
    assert_eq!(
        entry["next"],
        format!("applying the answer of ask {} (runtime)", ask.id)
    );
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed_run(&detail.runs[0], &repo, &base);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let approved = payloads(&detail, "integration_approved");
    assert_eq!(approved.len(), 1);
    assert_eq!(approved[0]["ask_id"], ask.id.as_i64());
    // No fourth review.
    assert_eq!(reviewer.prompts().len(), 3);
}

/// A `concern` exits and closes the session and asks a person; `send_back`
/// parks the run for a resume whose request names the findings, and the
/// resumed session goes through validation and review like the worker's
/// (ADR-0027 decision 3): the review passes and the supervisor lands it.
#[test]
fn a_concern_sent_back_is_resumed_reviewed_again_and_landed() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[
        verdict(
            "concern",
            &["changes a file the task did not name"],
            "scope",
        ),
        verdict("pass", &[], "fixed"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(backend.texts().is_empty());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "review_finished") < position(&kinds, "exit_requested"));
    assert!(position(&kinds, "workspace_closed") < position(&kinds, "ask_opened"));
    let ask = queue.asks(Default::default()).unwrap()[0].clone();
    assert!(
        ask.question.contains("returned concern: scope"),
        "{}",
        ask.question
    );
    // The ask was opened through `runtime::ask`, which notifies the inbox.
    let notified = backend.notifications.lock().unwrap().clone();
    assert_eq!(notified.len(), 1, "{notified:?}");
    assert!(
        notified[0].1.contains(&format!("run {}", run.id())),
        "{notified:?}"
    );

    queue.answer(ask.id, "send_back").unwrap();
    backend.resume_script_for(
        1,
        "await_message; printf 'narrowed\\n' > change.txt; unlocked git commit -q -am narrowed; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let landed = detail.runs[0].clone();
    assert_landed_run(&landed, &repo, &base);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        "narrowed\n"
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let decided = payloads(&detail, "landing_decided");
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0]["answer"], "send_back");
    assert_eq!(decided[0]["status"], "needs_session");
    let text = &backend.texts()[0].1;
    assert!(
        text.contains("raised findings a person sent back to you"),
        "{text}"
    );
    assert!(
        text.contains("changes a file the task did not name"),
        "{text}"
    );
    assert!(text.contains("Fix the findings in the reason"), "{text}");
    // The resumed session stayed open through the review: validation,
    // review, then /exit and the close of its workspace, then the landing.
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1, "{finished:?} {:?}", event_kinds(&detail));
    assert_eq!(finished[0]["outcome"], "resolved");
    assert_eq!(finished[0]["status"], "validating");
    assert_eq!(finished[0]["workspace_closed"], false);
    assert_eq!(finished[0]["session_live"], true);
    let resume_workspace = finished[0]["workspace_id"].as_str().unwrap().to_owned();
    let kinds = event_kinds(&detail);
    let after_resume: Vec<&str> = kinds
        .iter()
        .skip_while(|k| **k != "resume_finished")
        .filter(|k| {
            matches!(
                **k,
                "validation_finished"
                    | "review_started"
                    | "review_finished"
                    | "exit_requested"
                    | "workspace_closed"
                    | "integration_started"
                    | "run_integrated"
            )
        })
        .copied()
        .collect();
    assert_eq!(
        after_resume,
        [
            "validation_finished",
            "review_started",
            "review_finished",
            "exit_requested",
            "workspace_closed",
            "integration_started",
            "run_integrated",
        ]
    );
    let closed = payloads(&detail, "workspace_closed");
    assert_eq!(
        closed.last().unwrap(),
        &&json!({"workspace_id": resume_workspace, "resume_attempt": 1})
    );
    assert!(backend.closed().contains(&resume_workspace));
    assert_eq!(payloads(&detail, "review_started")[1]["session_live"], true);
    assert_eq!(reviewer.prompts().len(), 2);
}

/// A run whose review returned `concern` and opened ask A, then sent back
/// without the ask (as by hand) so that A stays open; with `answered`, A is
/// answered `land` but not applied (the run no longer awaits integration).
/// Its resume commits `narrowed` and goes idle. Returns the run and A.
fn sent_back_past_its_ask(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    reviewer: &TestReviewer,
    answered: bool,
) -> (TaskRun, dagq::domain::Ask) {
    let outcome = supervise_reviewed(db, repo, backend, reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let stale = asks[0].clone();
    assert!(
        stale.question.contains("returned concern: first"),
        "{}",
        stale.question
    );
    queue
        .decide_landing(
            run.id(),
            RunStatus::NeedsSession,
            "sent back by hand",
            dagq::domain::Reason::new(ReasonCode::SentBack).on(json!({})),
        )
        .unwrap();
    if answered {
        queue.answer(stale.id, "land").unwrap();
    }
    backend.resume_script_for(
        1,
        "await_message; printf 'narrowed\\n' > change.txt; unlocked git commit -q -am narrowed; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    (run, queue.read_ask(stale.id).unwrap())
}

/// Assert that `stale` was closed by the runtime with `answer`: an open one
/// as `ask_answered` with `runtime_closed`, an answered one as `ask_closed`.
fn assert_closed_by_runtime(
    queue: &mut SqliteQueue,
    run: &TaskRun,
    stale: &dagq::domain::Ask,
    answer: &str,
) {
    let closed = queue.read_ask(stale.id).unwrap();
    assert!(closed.closed_at.is_some(), "{closed:?}");
    let detail = queue.show(run.task_id()).unwrap();
    if stale.answer.is_none() {
        assert_eq!(closed.answer.as_deref(), Some(answer));
        let answered: Vec<_> = payloads(&detail, "ask_answered")
            .into_iter()
            .filter(|p| p["ask_id"] == stale.id.as_i64())
            .collect();
        assert_eq!(answered.len(), 1, "{answered:?}");
        assert_eq!(answered[0]["runtime_closed"], true);
        // The runtime's answer carries its own authority and approves
        // nothing, whatever the ask's kind (task 733).
        assert_eq!(answered[0]["authority"], "runtime");
        assert_eq!(answered[0]["approval"], false);
        assert_eq!(
            closed.answer_authority,
            Some(dagq::domain::AnswerAuthority::Runtime)
        );
        assert_eq!(closed.answer_approval, Some(false));
    } else {
        assert_eq!(closed.answer, stale.answer);
        assert!(
            payloads(&detail, "ask_closed")
                .iter()
                .any(|p| p["ask_id"] == stale.id.as_i64()),
            "{:?}",
            event_kinds(&detail)
        );
    }
}

/// A later review of a run that does not pass closes the run's earlier
/// `approve_landing` ask as the runtime, and opens a new one with the new
/// review's reasons, rather than being folded into the earlier one (task
/// 425). Here the earlier ask is open; one answered but not applied is
/// closed the same way (`SqliteQueue::close_approve_landing_asks`), as
/// [`a_run_the_supervisor_lands_closes_its_landing_ask`] shows.
#[test]
fn a_later_concern_closes_the_earlier_landing_ask_and_asks_again() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[
        verdict("concern", &["the first finding"], "first"),
        verdict("concern", &["the second finding"], "second"),
    ]);
    let (run, stale) = sent_back_past_its_ask(&db, &repo, &backend, &reviewer, false);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert_eq!(reviewer.prompts().len(), 2);
    assert_closed_by_runtime(
        &mut queue,
        &run,
        &stale,
        "a later review of the run asks again; closed by the runtime",
    );
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let fresh = &asks[0];
    assert_ne!(fresh.id, stale.id);
    assert_eq!(fresh.kind, dagq::domain::AskKind::ApproveLanding);
    assert_eq!(fresh.run_id.as_ref(), Some(run.id()));
    assert!(fresh.answer.is_none());
    assert!(
        fresh.question.contains("returned concern: second"),
        "{}",
        fresh.question
    );
    assert!(
        fresh.question.contains("\n- the second finding"),
        "{}",
        fresh.question
    );
    assert!(payloads(&detail, "integration_approved").is_empty());
}

/// A run the supervisor lands closes its `approve_landing` ask nobody
/// closed (task 425): here one answered but not applied, whose answer the
/// landing does not apply. An open one is closed the same way, as
/// [`a_later_concern_closes_the_earlier_landing_ask_and_asks_again`] and
/// [`integrate_by_hand_closes_the_landing_and_blocked_asks_of_the_run`] show.
#[test]
fn a_run_the_supervisor_lands_closes_its_landing_ask() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[
        verdict("concern", &["the first finding"], "first"),
        verdict("pass", &[], "fixed"),
    ]);
    let (run, stale) = sent_back_past_its_ask(&db, &repo, &backend, &reviewer, true);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed_run(&detail.runs[0], &repo, &base);
    assert_closed_by_runtime(
        &mut queue,
        &run,
        &stale,
        "the run was integrated; closed by the runtime",
    );
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    // Closed by the landing, after the run was integrated.
    let kinds = event_kinds(&detail);
    let closed_at = kinds
        .iter()
        .rposition(|k| *k == "ask_closed")
        .expect("the ask was closed");
    assert!(position(&kinds, "run_integrated") < closed_at, "{kinds:?}");
    // The earlier answer was not applied: the review's pass landed it.
    assert!(payloads(&detail, "integration_approved").is_empty());
}

/// `integrate` by hand of a run whose review asked a person closes the
/// run's `approve_landing` ask: nobody needs to answer it any more
/// (task 425). Landing it also closes the `blocked` asks the observer
/// opened on the run or on its task, as the runtime, so they no longer
/// raise `ask_unanswered`; a blocked ask about no task stays open (task
/// 329).
#[test]
fn integrate_by_hand_closes_the_landing_and_blocked_asks_of_the_run() {
    use dagq::domain::{AskReason, NewAsk};
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("concern", &["a finding"], "first")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let stale = queue.asks(Default::default()).unwrap()[0].clone();
    assert!(stale.closed_at.is_none());
    let mut blocked = |task_id: Option<TaskId>, run_id: Option<RunId>| {
        queue
            .ask(NewAsk {
                recommendation: None,
                confidence: None,
                topics: Vec::new(),
                kind: AskKind::Blocked,
                task_id,
                run_id,
                question: "is it stuck?".into(),
                options: Vec::new(),
                asked_by: "observer".into(),
                reason_category: AskReason::Scope,
                finding_id: None,
                request_id: None,
            })
            .unwrap()
            .ask
    };
    let of_run = blocked(None, Some(run.id().clone()));
    let of_task = blocked(Some(TaskId::new(1)), None);
    let of_queue = blocked(None, None);
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed_run(&detail.runs[0], &repo, &base);
    for stale in [&stale, &of_run, &of_task] {
        assert_closed_by_runtime(
            &mut queue,
            &run,
            stale,
            "the run was integrated; closed by the runtime",
        );
    }
    let left: Vec<_> = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .map(|ask| ask.id)
        .collect();
    assert_eq!(left, [of_queue.id]);
    // Each close is recorded on the run and task its ask named, so
    // `stats` pairs it with the ask's `ask_opened`.
    let detail = queue.show(TaskId::new(1)).unwrap();
    let closed_of_task = detail
        .events
        .iter()
        .find(|e| e.kind == "ask_answered" && e.payload["ask_id"] == of_task.id.as_i64())
        .unwrap();
    assert!(closed_of_task.run_id.is_none());
}

/// A headless review whose job fails (here a non-zero exit) exits and
/// closes the session, is not reviewed again, and in the step that records
/// `review_failed` opens an `approve_landing` ask with the failure and where
/// the review material and output are (task 328). The ask carries no
/// recommendation and no `concern_decided` is recorded (ADR-t451-1). The
/// ask, not the failure, is the attention, and its answer is applied by the
/// supervisor: here `cancel` fails the run and cancels the task. Which ends
/// are reviewed again and the ask's text for each are
/// `landing::tests::only_an_unreadable_verdict_is_reviewed_again_once` and
/// `landing::tests::a_failed_review_that_ran_names_its_own_output`; the
/// timeout is `runtime_headless_jobs::a_timed_out_review_stops_the_processes_it_started`.
#[test]
fn a_failed_review_closes_the_session_and_asks_a_person_in_the_same_step() {
    let expected = "exited with exit status: 3: broken";
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&["echo broken >&2; exit 3".to_owned()]);
    let cursor = SqliteQueue::open(&db)
        .unwrap()
        .latest_event_id()
        .unwrap()
        .as_i64();
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"review_finished"), "{kinds:?}");
    assert!(!kinds.contains(&"concern_decided"), "{kinds:?}");
    // A job that failed is not reviewed again.
    assert_eq!(reviewer.prompts().len(), 1);
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert!(payloads(&detail, "review_retried").is_empty(), "{kinds:?}");
    assert!(position(&kinds, "workspace_closed") < position(&kinds, "ask_opened"));
    assert!(position(&kinds, "ask_opened") < position(&kinds, "review_failed"));
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["attempt"], 1);
    assert_eq!(failed[0]["code"], "job_failed");
    assert_eq!(failed[0]["status"], "awaiting_integration");
    let error = failed[0]["error"].as_str().unwrap();
    assert!(error.contains(expected), "{error}");
    // The ask carries the failure and the material.
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = &asks[0];
    assert_eq!(failed[0]["ask_id"], ask.id.as_i64());
    assert_eq!(ask.kind, dagq::domain::AskKind::ApproveLanding);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert_eq!(ask.options, ["land", "send_back", "cancel"]);
    assert_eq!(ask.asked_by, "supervisor");
    assert_eq!(ask.recommendation, None);
    assert_eq!(ask.confidence, None);
    let run_dir = run.run_dir().unwrap();
    for part in [
        "failed and gave no verdict (review 1): ".to_owned(),
        expected.to_owned(),
        format!("Review material: {run_dir}/review.md"),
        format!("Review output: {run_dir}/review-1.out, {run_dir}/review-1.err"),
    ] {
        assert!(ask.question.contains(&part), "{part} in {}", ask.question);
    }
    {
        let notifications = backend.notifications.lock().unwrap();
        assert_eq!(notifications.len(), 1, "{notifications:?}");
        assert!(notifications[0].0.ends_with("approve_landing"));
    }
    // The ask is the attention, and the only event that wakes the inbox.
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
    assert!(
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["ask_id"] == json!(ask.id) && a["next"] == format!("answer ask {}", ask.id)),
        "{status}"
    );
    let events = dagq::watch::events(&db, EventId::new(cursor), 100, false).unwrap();
    let events = events["events"].as_array().unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["kind"], "ask_opened");
    // The supervisor applies the answer.
    queue.answer(ask.id, "cancel").unwrap();
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    assert_eq!(
        detail.runs[0].last_error(),
        Some(format!("canceled by ask {}", ask.id).as_str())
    );
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    // No review after the answer.
    assert_eq!(reviewer.prompts().len(), 1);
}

/// A failed review's span ends with its job (task 541), before the
/// session's `/exit`, which here takes half a second more; `review_failed`
/// and its ask still follow the exit. Here review 1 prints no readable
/// verdict, so it is reviewed once more with the same input (task 328),
/// and that review cannot start: both spans are closed as `job_finished`,
/// and the ask names only the output of review 1, the one that ran (task
/// 426). Which ends are reviewed again and the ask's text for each are
/// `landing::tests::only_an_unreadable_verdict_is_reviewed_again_once` and
/// `landing::tests::a_review_that_could_not_start_names_no_output_of_its_own`.
#[test]
fn a_failed_review_span_ends_with_its_job_not_with_the_exit() {
    use dagq::domain::stats::rfc3339_millis;
    let slow_exit = "commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit; sleep 0.5";
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, slow_exit);
    let reviewer = TestReviewer::new(&[
        "echo 'no verdict here'".to_owned(),
        UNSTARTABLE_REVIEW.to_owned(),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let event = |kind: &str| {
        detail
            .events
            .iter()
            .rev()
            .find(|e| e.kind == kind)
            .unwrap_or_else(|| panic!("no {kind} in {:?}", event_kinds(&detail)))
    };
    // Reviewed once more with the same input, so the same prompt.
    let prompts = reviewer.prompts();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0], prompts[1]);
    let retried = payloads(&detail, "review_retried");
    assert_eq!(retried.len(), 1);
    assert_eq!(retried[0]["attempt"], 1);
    assert!(
        retried[0]["error"]
            .as_str()
            .unwrap()
            .contains("the review printed no verdict JSON"),
        "{retried:?}"
    );
    let reviews_closed: Vec<_> = detail
        .events
        .iter()
        .filter(|e| e.kind == "session_closed" && e.payload["kind"] == "review")
        .collect();
    assert_eq!(reviews_closed.len(), 2);
    assert!(
        reviews_closed
            .iter()
            .all(|e| e.payload["reason"] == "job_finished"),
        "{reviews_closed:?}"
    );
    // Every review span is closed; none is left open.
    assert_eq!(
        payloads(&detail, "session_opened")
            .iter()
            .filter(|p| p["kind"] == "review")
            .count(),
        2
    );
    let closed = reviews_closed.last().unwrap();
    let failed = event("review_failed");
    // Closed before the /exit was even asked, and half a second or
    // more before the session exited and `review_failed` was recorded.
    assert!(closed.id < event("exit_requested").id);
    let at = |e: &dagq::domain::RunEvent| rfc3339_millis(&e.created_at).unwrap();
    assert!(
        at(event("session_exited")) - at(closed) >= 500,
        "{} then {}",
        closed.created_at,
        event("session_exited").created_at
    );
    assert!(at(failed) >= at(event("session_exited")));
    // The ask goes with the failure as before.
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(failed.payload["ask_id"], asks[0].id.as_i64());
    assert_eq!(failed.payload["attempt"], 2);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "ask_opened") < position(&kinds, "review_failed"));
    // The review that could not start wrote nothing and is not named; the
    // one before it ran and is.
    let run_dir = detail.runs[0].run_dir().unwrap();
    let question = &asks[0].question;
    for part in [
        "failed and gave no verdict (review 2): ".to_owned(),
        "the headless review could not start".to_owned(),
        "the test reviewer cannot start this review".to_owned(),
        format!("Review material: {run_dir}/review.md"),
        format!(
            "Review output of the earlier review 1: {run_dir}/review-1.out, {run_dir}/review-1.err"
        ),
    ] {
        assert!(question.contains(&part), "{part} in {question}");
    }
    assert!(Path::new(&format!("{run_dir}/review-1.out")).exists());
    assert!(!Path::new(&format!("{run_dir}/review-2.out")).exists());
    assert!(!question.contains("review-2."), "{question}");
}

/// A session that writes its receipt and goes idle before its wrapper has
/// registered its agent (the wrapper slowed by load between the two) is
/// not taken to validation while the run is still `starting`: the
/// supervisor waits for `agent_started`, the wrapper's registration is not
/// refused ("run is not starting"), and the run is reviewed and lands
/// (task 1274).
#[test]
fn a_session_idle_before_its_agent_is_registered_waits_for_the_agent_and_lands() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.agent_registers_late = true;
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_landed_run(&detail.runs[0], &repo, &base);
    let kinds = event_kinds(&detail);
    assert!(
        position(&kinds, "agent_started") < position(&kinds, "supervision_finished"),
        "{kinds:?}"
    );
}

/// A revise whose rewritten receipt does not name the clean worktree HEAD
/// (here the commit before the fix) is not handed to validation, which
/// would fail the run and its work: the live session is asked to rewrite
/// it for HEAD, and once it does the run is reviewed again and lands
/// (task 107, description (d)).
#[test]
fn a_revise_receipt_for_another_commit_is_sent_back_to_the_session_until_it_names_head() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let script = "commit work; receipt \"$(git rev-parse HEAD)\"; idle; \
        while [ ! -f \"$MESSAGE\" ]; do sleep 0.1; done; rm \"$MESSAGE\"; \
        before=$(git rev-parse HEAD); printf 'fix\\n' >> change.txt; git commit -q -am fix; \
        receipt \"$before\"; idle; \
        while [ ! -f \"$MESSAGE\" ]; do sleep 0.1; done; rm \"$MESSAGE\"; \
        receipt \"$(git rev-parse HEAD)\"; idle; await_exit";
    let backend = TestWorkspace::new(&db, false, script);
    let reviewer = TestReviewer::new(&[
        verdict("revise", &["add a line"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &base);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        format!("change by {}\nfix\n", run.id())
    );
    let rejected = payloads(&detail, "revise_receipt_rejected");
    assert_eq!(rejected.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(rejected[0]["attempt"], 1);
    assert!(
        rejected[0]["reason"]
            .as_str()
            .unwrap()
            .contains("but the worktree HEAD is"),
        "{}",
        rejected[0]
    );
    // Validation saw only the receipt for HEAD: nothing failed.
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 2);
    assert!(validated.iter().all(|v| v["accepted"] == true));
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "revise_receipt_rejected") < position(&kinds, "revise_finished"));
    let texts = backend.texts();
    assert_eq!(texts.len(), 2);
    assert!(
        texts[1].1.contains(&format!(
            "dagq: the receipt you rewrote for revise 1 of run {} cannot be accepted",
            run.id()
        )),
        "{}",
        texts[1].1
    );
    assert!(texts[1].1.contains("git rev-parse HEAD"), "{}", texts[1].1);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
}

/// A worker that asks a `worker_question` while it revises: once the revise
/// request arrives it asks, goes idle, waits for the answer in `$MESSAGE`
/// and commits it.
const REVISE_ASKING_AGENT: &str = r#"
commit work; receipt "$(git rev-parse HEAD)"; idle
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done; rm "$MESSAGE"
"$DAGQ" ask --run "$RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question 'Which line?' --cmux /usr/bin/true > /dev/null || exit 70
idle
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
cp "$MESSAGE" answer.txt; rm "$MESSAGE"
git add answer.txt; git commit -q -m answer
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#;

/// A session that stops at its own `worker_question` while it revises is not
/// taken for one that went idle without rewriting its receipt: the answer is
/// the runtime's to type (`runtime_delivers`), it is typed into the session
/// once answered, and the revise goes on to its review and landing (task
/// 238).
#[test]
fn a_worker_question_asked_while_revising_is_answered_and_the_run_lands() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(TestWorkspace::new(&db, false, REVISE_ASKING_AGENT));
    let reviewer = Arc::new(TestReviewer::new(&[
        verdict("revise", &["say which line"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]));
    let options = supervise_options(4, true);
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue.asks(AskQuery::default()).unwrap().remove(0);
    assert_eq!(ask.kind, AskKind::WorkerQuestion);
    // Idle at its question for a while: the revise waits for the answer.
    // The session goes idle after its question, and the supervisor passes
    // over that idle some times.
    let detail = queue.show(TaskId::new(1)).unwrap();
    await_written_after(
        &detail.runs[0].idle_marker_path().unwrap(),
        first_event_millis(&detail, "ask_opened"),
    );
    await_passes(&passes, SOME_PASSES);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(kinds.contains(&"revise_requested"), "{kinds:?}");
    assert!(!kinds.contains(&"exit_requested"), "{kinds:?}");
    assert!(!kinds.contains(&"revise_finished"), "{kinds:?}");
    assert_eq!(queue.asks(AskQuery::default()).unwrap().len(), 1);

    queue.answer(ask.id, "the second").unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let answered = payloads(&detail, "ask_answered");
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0]["runtime_delivers"], true, "{}", answered[0]);
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    let texts = backend.texts();
    assert_eq!(texts.len(), 2, "{texts:?}");
    let answer = format!("answer to ask {}: the second", ask.id);
    assert_eq!(texts[1], (WORKSPACE_ID.to_owned(), answer.clone()));
    assert_eq!(fs::read_to_string(repo.join("answer.txt")).unwrap(), answer);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert_eq!(
        payloads(&detail, "ask_delivered"),
        vec![&json!({"ask_id": ask.id, "workspace_id": WORKSPACE_ID})]
    );
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
    // Nothing waits for a person.
    assert!(queue.asks(AskQuery::default()).unwrap().is_empty());
}

/// A dialog screen as Claude Code draws it.
const REVISE_DIALOG_SCREEN: &str = "\
 Auto mode is available

 ❯ 1. Yes, turn on auto mode
   2. No, keep asking

 Esc to cancel
";

/// A session that stops at a dialog while it revises records
/// `prompt_waiting` and raises it (through its recovery job) as an
/// `answer_prompt` ask, as a worker's own session does, instead of waiting
/// out the resume timeout; the dialog gone, `prompt_cleared` closes the ask
/// and the revise goes on (task 238).
#[test]
fn a_dialog_while_revising_is_recorded_as_prompt_waiting() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let mut backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; \
         while [ ! -f \"$MESSAGE\" ]; do sleep 0.05; done; rm \"$MESSAGE\"; \
         while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; \
         printf 'fix\\n' >> change.txt; git commit -q -am fix; \
         receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    backend.prompt_wait = Duration::from_millis(300);
    let backend = Arc::new(backend);
    let reviewer = Arc::new(TestReviewer::new(&[
        verdict("revise", &["add a line"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]));
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || supervise_reviewed(&db, &repo, &backend, &reviewer))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"revise_requested")
    });
    *backend.screen.lock().unwrap() = REVISE_DIALOG_SCREEN.into();
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(AskQuery::default()).unwrap().is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "revise_requested") < position(&kinds, "prompt_waiting"));
    let waiting = payloads(&detail, "prompt_waiting");
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0]["workspace_id"], WORKSPACE_ID);
    assert_eq!(waiting[0]["prompt"], "choice");
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::AnswerPrompt);
    assert!(!kinds.contains(&"exit_requested"), "{kinds:?}");

    // Someone answers the dialog: the screen goes back to work.
    *backend.screen.lock().unwrap() = WORK_SCREEN.into();
    // `prompt_cleared` is recorded before the ask is closed, in its own
    // write: wait for both.
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"prompt_cleared")
            && queue.read_ask(asks[0].id).unwrap().closed_at.is_some()
    });
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
    assert_eq!(payloads(&detail, "prompt_waiting").len(), 1);
    assert!(queue.asks(AskQuery::default()).unwrap().is_empty());
}

/// The general runtime fixture needs the failed-review state, not an
/// extra no-verdict job. Explicit review tests retain the production retry.
#[test]
fn the_default_test_reviewer_skips_retry_but_still_asks_after_its_job() {
    assert!(SuperviseOptions::new(4, true).retry_unreadable_review);
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert!(payloads(&detail, "review_retried").is_empty());
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["attempt"], 1);
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].kind, AskKind::ApproveLanding);
    assert_eq!(failed[0]["ask_id"], json!(asks[0].id));
    assert!(asks[0].question.contains("review-1.out"));
    assert!(!asks[0].question.contains("review-2.out"));
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    let closed = detail
        .events
        .iter()
        .filter(|e| e.kind == "session_closed" && e.payload["kind"] == "review")
        .collect::<Vec<_>>();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].payload["reason"], "job_finished");
    let failed_event = detail
        .events
        .iter()
        .find(|e| e.kind == "review_failed")
        .unwrap();
    assert!(closed[0].id < failed_event.id);
}
