//! Review verdicts, retries and answers wired through a live worker session
//! to the queue's events, asks and status, with Git and agents in memory.

use super::{
    agent::{Script, login_wall, no_receipt, succeeded, succeeded_at_the_head, verdict},
    harness::Wiring,
};
use crate::{
    application::{RunFiles, TaskReads},
    domain::{
        AskKind, EventKind, ReasonCode, RunId, RunStatus, TaskDetail, TaskId, TaskRun, TaskStatus,
    },
    infrastructure::sqlite::SqliteQueue,
};
use serde_json::{Value, json};
use std::path::Path;

fn payloads<'a>(detail: &'a TaskDetail, kind: &str) -> Vec<&'a Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind.as_str() == kind)
        .map(|e| &e.payload)
        .collect()
}
fn event_kinds(detail: &TaskDetail) -> Vec<&str> {
    detail.events.iter().map(|e| e.kind.as_str()).collect()
}
fn position(kinds: &[&str], kind: &str) -> usize {
    kinds
        .iter()
        .position(|k| *k == kind)
        .unwrap_or_else(|| panic!("no {kind} in {kinds:?}"))
}
fn reviewed(
    name: &str,
    receipts: Vec<super::agent::Receipt>,
    verdicts: Vec<Value>,
) -> (Wiring, TaskId) {
    let wiring = Wiring::new(name, Script { receipts, verdicts }).with_review_retry();
    let task = wiring.ready_task(&["test -f seed.txt"]);
    (wiring, task)
}
fn finding(decision: &str, reason: &str, summary: &str) -> Value {
    json!({"verdict": decision, "reasons": [reason], "summary": summary})
}
fn landed(wiring: &Wiring, task: TaskId) -> TaskDetail {
    let detail = wiring.queue().show(task).unwrap();
    assert_eq!(
        detail.task.status(),
        TaskStatus::Completed,
        "{:?}",
        event_kinds(&detail)
    );
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert_ne!(wiring.git.main(), *detail.runs[0].base_commit());
    assert_eq!(detail.runs[0].result_commit(), Some(&wiring.git.main()));
    assert_eq!(wiring.git.pushed(), Some(wiring.git.main()));
    assert!(wiring.queue().run_leases().unwrap().is_empty());
    detail
}
fn attention<'a>(status: &'a Value, key: &str, value: Value) -> Vec<&'a Value> {
    status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e[key] == value)
        .collect()
}

#[test]
fn a_passing_review_exits_the_live_session_and_lands_it() {
    let (wiring, task) = reviewed(
        "pass",
        vec![succeeded],
        vec![verdict("pass", "meets the acceptance")],
    );
    wiring.supervise_once();
    let detail = landed(&wiring, task);
    let run = &detail.runs[0];
    let kinds = event_kinds(&detail);
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
            "{kinds:?}"
        );
    }
    assert!(!kinds.contains(&"integration_approved"));
    assert_eq!(payloads(&detail, "exit_requested").len(), 1);
    assert_eq!(payloads(&detail, "workspace_closed").len(), 1);
    assert_eq!(
        payloads(&detail, "supervision_finished"),
        [&json!({"status": "validating", "exit_code": null, "session_live": true})]
    );
    let started = payloads(&detail, "review_started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["session_live"], true);
    assert_eq!(started[0]["attempt"], 1);
    assert_eq!(started[0]["revise_limit"], 2);
    for (key, expected) in [
        ("role", json!("review")),
        ("provider", json!("claude")),
        ("model", Value::Null),
        ("effort", Value::Null),
        ("source", json!("default")),
    ] {
        assert_eq!(started[0]["launch"][key], expected, "{}", started[0]);
    }
    assert_eq!(started[0]["workspace_id"], run.workspace_id().unwrap());
    let session_id = started[0]["session_id"].as_str().unwrap();
    let opened: Vec<_> = payloads(&detail, "session_opened")
        .iter()
        .map(|p| {
            (
                p["kind"].as_str().unwrap(),
                p["session_id"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        opened,
        [("worker", run.id().as_str()), ("review", session_id)]
    );
    let closed: Vec<_> = payloads(&detail, "session_closed")
        .iter()
        .map(|p| (p["kind"].as_str().unwrap(), p["reason"].as_str().unwrap()))
        .collect();
    assert_eq!(closed, [("review", "job_finished"), ("worker", "exited")]);
    for close in payloads(&detail, "session_closed") {
        if close["kind"] == "worker" {
            assert_eq!(close["active"], "recorded");
            continue;
        }
        assert_eq!(close["active"], "unavailable");
        assert_eq!(close["active_unavailable"], "transcript_missing");
    }
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["verdict"], "pass");
    assert_eq!(finished[0]["reasons"], json!([]));
    assert_eq!(finished[0]["summary"], "meets the acceptance");
    assert_eq!(finished[0]["attempt"], 1);
    assert!(finished[0]["duration_secs"].is_u64());
    let prompts = wiring.agent.review_prompts();
    assert_eq!(prompts.len(), 1);
    let run_dir = Path::new(run.run_dir().unwrap());
    assert!(wiring.files.exists(&run_dir.join("review.md")));
    for expected in [
        format!("Read the review material at {}", run_dir.join("review.md").display()),
        "Acceptance criteria of the task:\nworks".into(),
        r#"{"verdict": "pass" | "revise" | "concern", "reasons": [{"text": string, "codes": [string], "paths": [string]}], "summary": string, "recommendation": "land" | "send_back" | null, "confidence": "high" | "low" | null, "reason_category": "scope" | "discard" | null, "follow_ups": [{"text": string, "path": string, "section": string, "codes": [string]}]}"#
            .to_owned(),
        "- revise: findings the worker can fix without a person's judgment".into(),
        "- concern: findings that call for a judgment rather than a mechanical fix".into(),
        "the runtime applies a sure judgment that needs no person (high, reason_category null) itself".into(),
    ] { assert!(prompts[0].contains(&expected), "{expected}: {}", prompts[0]); }
    assert_eq!(
        wiring
            .files
            .read_to_string(&run_dir.join("review-prompt-1.txt"))
            .unwrap(),
        prompts[0]
    );
    let bytes = &started[0]["prompt_bytes"];
    assert_eq!(bytes["total"], prompts[0].len());
    assert_eq!(
        bytes["limit"],
        crate::application::prompt::RUN_REVIEW_PROMPT_LIMIT
    );
    assert!(bytes["sections"]["acceptance"].as_u64().unwrap() > 0);
    assert_eq!(bytes["omitted"], json!({}));
    assert!(bytes["sections"].get("known_failures").is_none());
    for prompt in [
        &prompts[0],
        &wiring
            .files
            .read_to_string(&run_dir.join("prompt.txt"))
            .unwrap(),
    ] {
        assert!(!prompt.contains("fail already"));
    }
    for file in [
        "known-failures.json",
        "review-1-known-failures.json",
        "terminal-final.txt",
    ] {
        assert!(!wiring.files.exists(&run_dir.join(file)));
    }
    assert!(attention(&wiring.status(), "run_id", json!(run.id())).is_empty());
}

#[test]
fn a_revise_verdict_is_fixed_by_the_live_session_and_reviewed_again() {
    let (wiring, task) = reviewed(
        "revise",
        vec![succeeded; 2],
        vec![
            finding("revise", "add a line to change.txt", "one gap"),
            verdict("pass", "fixed"),
        ],
    );
    wiring.supervise_once();
    let detail = landed(&wiring, task);
    assert_eq!(wiring.git.main_line().len(), 2);
    let requested = payloads(&detail, "revise_requested");
    assert_eq!(requested.len(), 1);
    assert_eq!(requested[0]["attempt"], 1);
    assert_eq!(requested[0]["reasons"], json!(["add a line to change.txt"]));
    assert_eq!(requested[0]["model"], "claude-opus-5-5");
    assert_eq!(requested[0]["effort"], "high");
    assert_eq!(
        requested[0]["escalated_from"],
        json!({"model": "claude-opus-5-5", "effort": "medium"})
    );
    assert_eq!(requested[0]["escalation_reason"], "revise");
    let runs = crate::domain::stats::run_stats(&detail.events);
    let escalations =
        crate::domain::stats::escalations::escalations(&runs.iter().collect::<Vec<_>>());
    assert_eq!(escalations.count, 1);
    assert_eq!(escalations.revises, 1);
    assert_eq!(escalations.by_reason.get("revise"), Some(&1));
    let revised = payloads(&detail, "revise_finished");
    assert_eq!(revised.len(), 1);
    assert_eq!(revised[0]["attempt"], 1);
    assert_eq!(
        revised[0]["head"],
        payloads(&detail, "validation_finished")[1]["result_commit"]
    );
    assert_eq!(
        payloads(&detail, "review_finished")
            .iter()
            .map(|p| p["verdict"].clone())
            .collect::<Vec<_>>(),
        [json!("revise"), json!("pass")]
    );
    assert_eq!(payloads(&detail, "validation_finished").len(), 2);
    assert_ne!(
        payloads(&detail, "validation_finished")[0]["result_commit"],
        revised[0]["head"]
    );
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "revise_requested") < position(&kinds, "revise_finished"));
    assert!(position(&kinds, "revise_finished") < position(&kinds, "exit_requested"));
    assert_eq!(payloads(&detail, "exit_requested").len(), 1);
    assert_eq!(
        wiring.agent.turn_models(),
        [
            ("claude-opus-5-5".into(), "medium".into()),
            ("claude-opus-5-5".into(), "high".into())
        ]
    );
    let prompts = wiring.agent.turn_prompts();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[1].contains(&format!(
        "dagq: the supervisor's review of run {} (task {}) asks for changes",
        detail.runs[0].id(),
        task
    )));
    for expected in [
        "asks for changes (revise 1 of 2 of the run).",
        "Findings:\n- add a line to change.txt",
        "[\"test -f seed.txt\"]",
        "Rewrite the receipt at",
        "Before you end the turn, stop every process you started",
        "Do not merge or push. Follow the repository's instructions for a worker",
    ] {
        assert!(prompts[1].contains(expected), "{expected}: {}", prompts[1]);
    }
    assert_eq!(
        wiring
            .files
            .read_to_string(&Path::new(detail.runs[0].run_dir().unwrap()).join("revise-1.txt"))
            .unwrap(),
        prompts[1]
    );
}

fn assert_closed_by_runtime(
    queue: &mut SqliteQueue,
    run: &TaskRun,
    stale: &crate::domain::Ask,
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
            Some(crate::domain::AnswerAuthority::Runtime)
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

#[test]
fn integrate_by_hand_closes_the_landing_and_blocked_asks_of_the_run() {
    use crate::domain::{AskReason, NewAsk};
    let (wiring, task) = reviewed(
        "integrate_by_hand_closes_the_landing_and_blocked_asks_of_the_run",
        vec![succeeded],
        vec![finding("concern", "a finding", "first")],
    );
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let run = queue.show(task).unwrap().runs[0].clone();
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
    let of_task = blocked(Some(task), None);
    let of_queue = blocked(None, None);
    wiring.integrate(task);
    let detail = queue.show(task).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
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
    let detail = queue.show(task).unwrap();
    let closed_of_task = detail
        .events
        .iter()
        .find(|e| e.kind == EventKind::AskAnswered && e.payload["ask_id"] == of_task.id.as_i64())
        .unwrap();
    assert!(closed_of_task.run_id.is_none());
}

#[test]
fn integrate_closes_every_unclosed_ask_of_the_run_whatever_its_kind() {
    use crate::domain::{AskReason, NewAsk};
    let (wiring, task) = reviewed(
        "integrate_closes_every_unclosed_ask_of_the_run_whatever_its_kind",
        vec![succeeded],
        vec![finding("concern", "a finding", "first")],
    );
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let run = queue.show(task).unwrap().runs[0].clone();
    let mut stale = queue.asks(Default::default()).unwrap();
    assert_eq!(stale.len(), 1, "{stale:?}");
    queue
        .record_runtime_event(
            run.id(),
            EventKind::StallNudged,
            json!({"phase": "session", "idle_secs": 1250, "threshold_secs": 1200}),
        )
        .unwrap();
    let asks = [
        (
            AskKind::WorkerQuestion,
            "worker",
            AskReason::Scope,
            "answered",
        ),
        (AskKind::WorkerQuestion, "worker", AskReason::Scope, "open"),
        (
            AskKind::StuckExit,
            "supervisor",
            AskReason::RecoveryFailed,
            "exit?",
        ),
        (
            AskKind::AnswerPrompt,
            "supervisor",
            AskReason::RecoveryFailed,
            "dialog?",
        ),
        (
            AskKind::Stalled,
            "supervisor",
            AskReason::RecoveryFailed,
            "idle?",
        ),
    ];
    for (kind, asked_by, reason_category, question) in asks {
        let topics = if kind == AskKind::WorkerQuestion {
            vec!["task_overlap".into()]
        } else {
            Vec::new()
        };
        let ask = queue
            .ask(NewAsk {
                recommendation: None,
                confidence: None,
                topics,
                kind,
                task_id: None,
                run_id: Some(run.id().clone()),
                question: question.into(),
                options: Vec::new(),
                asked_by: asked_by.into(),
                reason_category,
                finding_id: None,
                request_id: None,
            })
            .unwrap()
            .ask;
        assert!(stale.iter().all(|earlier| earlier.id != ask.id), "{ask:?}");
        // The person answered the first question; the run lands before
        // the answer is delivered.
        let ask = if question == "answered" {
            queue.answer(ask.id, "the answer").unwrap()
        } else {
            ask
        };
        stale.push(ask);
    }

    wiring.integrate(task);
    for ask in &stale {
        assert_closed_by_runtime(
            &mut queue,
            &run,
            ask,
            "the run was integrated; closed by the runtime",
        );
    }
    assert_eq!(
        queue.read_ask(stale[1].id).unwrap().answer.as_deref(),
        Some("the answer")
    );
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    let inbox = wiring.status();
    for ask in &stale {
        assert!(
            attention(&inbox, "ask_id", json!(ask.id)).is_empty(),
            "{inbox}"
        );
    }
    // The nudge ended when the stalled ask opened; the ask's detection
    // ends with the landing, once.
    let detail = queue.show(task).unwrap();
    let ends: Vec<_> = payloads(&detail, "stall_resolved")
        .into_iter()
        .filter(|end| end["detection"] == "ask")
        .collect();
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!(ends[0]["outcome"], "run_ended");
    assert_eq!(ends[0]["ask_id"], stale[5].id.as_i64());
}

#[test]
fn a_review_past_the_revise_limit_that_does_not_pass_asks_a_person_and_land_lands_it() {
    let (wiring, task) = reviewed(
        "limit",
        vec![succeeded; 3],
        vec![
            finding("revise", "still short", "not yet"),
            finding("revise", "still short", "not yet"),
            json!({"verdict": "concern", "reasons": ["still short"], "summary": "not yet", "recommendation": "send_back", "confidence": "high", "reason_category": null}),
        ],
    );
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert_eq!(payloads(&detail, "revise_requested").len(), 2);
    assert_eq!(
        payloads(&detail, "revise_requested")
            .iter()
            .map(|p| p["effort"].clone())
            .collect::<Vec<_>>(),
        [json!("high"), json!("xhigh")]
    );
    assert_eq!(
        wiring
            .agent
            .turn_models()
            .iter()
            .map(|(_, effort)| effort.as_str())
            .collect::<Vec<_>>(),
        ["medium", "high", "xhigh"]
    );
    assert_eq!(payloads(&detail, "revise_finished").len(), 2);
    assert_eq!(payloads(&detail, "review_finished").len(), 3);
    assert_eq!(wiring.agent.turn_prompts().len(), 3);
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(payloads(&detail, "workspace_closed").len(), 1);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "workspace_closed") < position(&kinds, "ask_opened"));
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    let ask = &asks[0];
    assert_eq!(ask.kind, AskKind::ApproveLanding);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert_eq!(ask.options, ["land", "send_back", "cancel"]);
    assert_eq!(ask.asked_by, "supervisor");
    assert_eq!(
        payloads(&detail, "concern_decided"),
        [
            &json!({"attempt": 3, "recommendation": "send_back", "confidence": "high", "reason_category": null, "applied": false, "escalated_because": "revise_limit"})
        ]
    );
    assert_eq!(ask.recommendation.as_deref(), Some("send_back"));
    assert_eq!(ask.confidence, Some(crate::domain::AskConfidence::High));
    assert!(ask.question.contains("returned concern (the review recommends send_back after the run's 2 revises, and code findings are left (unlabeled)): not yet"), "{}", ask.question);
    assert!(ask.question.contains("\n- still short"));
    let status = wiring.status();
    let entries = attention(&status, "run_id", json!(run.id()));
    assert!(entries.iter().all(|e| e["ask_id"] == json!(ask.id)));
    assert_eq!(
        attention(&status, "ask_id", json!(ask.id))[0]["next"],
        format!("answer ask {}", ask.id)
    );
    queue.answer(ask.id, "land").unwrap();
    assert_eq!(
        attention(&wiring.status(), "ask_id", json!(ask.id))[0]["next"],
        format!("applying the answer of ask {} (runtime)", ask.id)
    );
    wiring.supervise_once();
    let detail = landed(&wiring, task);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let approved = payloads(&detail, "integration_approved");
    assert_eq!(approved.len(), 1);
    assert_eq!(approved[0]["ask_id"], ask.id.as_i64());
    assert_eq!(wiring.agent.review_prompts().len(), 3);
}

#[test]
fn a_concern_sent_back_is_resumed_reviewed_again_and_landed() {
    let (wiring, task) = reviewed(
        "send_back",
        vec![succeeded; 2],
        vec![
            finding("concern", "changes a file the task did not name", "scope"),
            verdict("pass", "fixed"),
        ],
    );
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert_eq!(payloads(&detail, "exit_requested").len(), 1);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "review_finished") < position(&kinds, "exit_requested"));
    assert!(position(&kinds, "workspace_closed") < position(&kinds, "ask_opened"));
    let ask = queue.asks(Default::default()).unwrap().remove(0);
    assert!(ask.question.contains("returned concern: scope"));
    queue
        .answer(ask.id, "send_back:  keep the change to the named file ")
        .unwrap();
    let detail = queue.show(task).unwrap();
    assert_eq!(
        payloads(&detail, "ask_answered")[0]["runtime_delivers"],
        true
    );
    assert_eq!(
        attention(&wiring.status(), "ask_id", json!(ask.id))[0]["next"],
        format!("applying the answer of ask {} (runtime)", ask.id)
    );
    wiring.supervise_once();
    let detail = landed(&wiring, task);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let decided = payloads(&detail, "landing_decided");
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0]["answer"], "send_back");
    assert_eq!(
        decided[0]["person_reason"],
        "keep the change to the named file"
    );
    assert_eq!(decided[0]["status"], "needs_session");
    assert_eq!(decided[0]["code"], "sent_back");
    let reason = format!(
        "a person sent the run back in ask {}: keep the change to the named file; the review's findings: changes a file the task did not name",
        ask.id
    );
    assert_eq!(decided[0]["reason"], reason);
    let outcomes = payloads(&detail, "review_outcome");
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0]["outcome"], "deviation_rejected");
    assert_eq!(outcomes[0]["ask_id"], ask.id.as_i64());
    let prompts = wiring.agent.turn_prompts();
    assert_eq!(prompts.len(), 2);
    for part in [
        "raised findings a person sent back to you",
        &reason,
        "Fix the findings in the reason",
    ] {
        assert!(prompts[1].contains(part), "{}", prompts[1]);
    }
    let resumed = payloads(&detail, "resume_finished");
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0]["outcome"], "resolved");
    assert_eq!(resumed[0]["status"], "validating");
    assert_eq!(resumed[0]["workspace_closed"], false);
    assert_eq!(resumed[0]["session_live"], true);
    let closed = payloads(&detail, "workspace_closed");
    assert_eq!(
        closed.last().unwrap()["workspace_id"],
        resumed[0]["workspace_id"]
    );
    assert_eq!(closed.last().unwrap()["resume_attempt"], 1);
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
    assert_eq!(payloads(&detail, "review_started")[1]["session_live"], true);
    assert_eq!(wiring.agent.review_prompts().len(), 2);
}

/// Resume without applying the earlier ask, leaving either an open ask
/// or an answered one for the next review or landing to close.
fn past_ask(wiring: &Wiring, task: TaskId, answered: bool) -> (TaskRun, crate::domain::Ask) {
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let run = queue.show(task).unwrap().runs[0].clone();
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let stale = asks[0].clone();
    assert!(
        stale.question.contains("returned concern: first"),
        "{}",
        stale.question
    );
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    queue
        .decide_landing(
            run.id(),
            RunStatus::NeedsSession,
            "sent back by hand",
            crate::domain::Reason::new(ReasonCode::SentBack).on(json!({})),
        )
        .unwrap();
    if answered {
        queue.answer(stale.id, "land").unwrap();
    }
    (run, queue.read_ask(stale.id).unwrap())
}

#[test]
fn a_later_concern_closes_the_earlier_landing_ask_and_asks_again() {
    let (wiring, task) = reviewed(
        "later_concern",
        vec![succeeded; 2],
        vec![
            finding("concern", "the first finding", "first"),
            finding("concern", "the second finding", "second"),
        ],
    );
    let (run, stale) = past_ask(&wiring, task, false);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert_closed_by_runtime(
        &mut queue,
        &run,
        &stale,
        "a later review of the run asks again; closed by the runtime",
    );
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    let fresh = &asks[0];
    assert_ne!(fresh.id, stale.id);
    assert_eq!(fresh.kind, AskKind::ApproveLanding);
    assert_eq!(fresh.run_id.as_ref(), Some(run.id()));
    assert!(fresh.answer.is_none());
    assert!(fresh.question.contains("returned concern: second"));
    assert!(fresh.question.contains("\n- the second finding"));
    assert!(payloads(&detail, "integration_approved").is_empty());
    assert_eq!(wiring.agent.review_prompts().len(), 2);
}

#[test]
fn a_run_the_supervisor_lands_closes_its_landing_ask() {
    let (wiring, task) = reviewed(
        "stale_landing",
        vec![succeeded; 2],
        vec![
            finding("concern", "the first finding", "first"),
            verdict("pass", "fixed"),
        ],
    );
    let (run, stale) = past_ask(&wiring, task, true);
    wiring.supervise_once();
    let detail = landed(&wiring, task);
    let mut queue = wiring.queue();
    assert_closed_by_runtime(
        &mut queue,
        &run,
        &stale,
        "the run was integrated; closed by the runtime",
    );
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    let kinds = event_kinds(&detail);
    assert!(
        position(&kinds, "run_integrated")
            < kinds.iter().rposition(|k| *k == "ask_closed").unwrap()
    );
    assert!(payloads(&detail, "integration_approved").is_empty());
}

#[test]
fn a_review_that_exits_non_zero_twice_closes_the_session_and_asks_a_person() {
    let expected = "exited with exit status: 3: broken";
    let (wiring, task) = reviewed(
        "a_review_that_exits_non_zero_twice_closes_the_session_and_asks_a_person",
        vec![succeeded],
        vec![json!({"wiring_exit": 3})],
    );
    let cursor = wiring.queue().latest_event_id().unwrap().as_i64();
    let outcome = wiring.supervise_once();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let run = detail.runs[0].clone();
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(payloads(&detail, "workspace_closed").len(), 1);
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"review_finished"), "{kinds:?}");
    assert!(!kinds.contains(&"concern_decided"), "{kinds:?}");
    // The job that exited non-zero runs once more with the same prompt in
    // the same review, and that run's own failure does not run it again.
    let prompts = wiring.agent.review_prompts();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0], prompts[1]);
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert!(payloads(&detail, "review_retried").is_empty(), "{kinds:?}");
    let ends = payloads(&detail, "review_job_finished");
    let outcomes: Vec<_> = ends
        .iter()
        .map(|end| (&end["again"], &end["outcome"], &end["runs_again"]))
        .collect();
    assert_eq!(
        outcomes,
        [
            (&json!(false), &json!("job_failed"), &json!(true)),
            (&json!(true), &json!("job_failed"), &json!(false)),
        ]
    );
    assert!(
        ends[0]["error"].as_str().unwrap().contains(expected),
        "{ends:?}"
    );
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
    assert_eq!(ask.kind, crate::domain::AskKind::ApproveLanding);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert_eq!(ask.options, ["land", "send_back", "cancel", "retry_review"]);
    assert_eq!(ask.asked_by, "supervisor");
    assert_eq!(ask.recommendation, None);
    assert_eq!(ask.confidence, None);
    let run_dir = run.run_dir().unwrap();
    for part in [
        "failed and gave no verdict (review 1): ".to_owned(),
        expected.to_owned(),
        format!("its output is in {run_dir}/review-1.again.out and .err"),
        format!("Review material: {run_dir}/review.md"),
        format!("Review output: {run_dir}/review-1.out, {run_dir}/review-1.err"),
    ] {
        assert!(ask.question.contains(&part), "{part} in {}", ask.question);
    }
    // The ask is the attention, and the only event that wakes the inbox.
    let status = wiring.status();
    assert!(
        attention(&status, "run_id", json!(run.id()))
            .iter()
            .all(|entry| !entry["ask_id"].is_null()),
        "{status}"
    );
    assert!(
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["ask_id"] == json!(ask.id) && a["next"] == format!("answer ask {}", ask.id)),
        "{status}"
    );
    let events = wiring
        .inbox_watch()
        .into_iter()
        .filter(|e| e["id"].as_i64().unwrap_or_default() > cursor)
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["kind"], "ask_opened");
    // The supervisor applies the answer.
    queue.answer(ask.id, "cancel").unwrap();
    let outcome = wiring.supervise_once();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(task).unwrap();
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    assert_eq!(
        detail.runs[0].last_error(),
        Some(format!("canceled by ask {}", ask.id).as_str())
    );
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    // The answer it applied is the cancel's reason (ADR-t947-4 decision 3).
    let canceled = detail
        .events
        .iter()
        .rfind(|event| event.kind == EventKind::TaskStatusChanged)
        .unwrap();
    assert_eq!(
        canceled.payload,
        json!({"from": "in_progress", "to": "canceled", "reason": "answered_cancel", "by": "approve_landing", "ask_id": ask.id})
    );
    // No review after the answer.
    assert_eq!(wiring.agent.review_prompts().len(), 2);
}

#[test]
fn a_review_that_exits_non_zero_once_is_reviewed_again_and_lands() {
    let (wiring, task) = reviewed(
        "a_review_that_exits_non_zero_once_is_reviewed_again_and_lands",
        vec![succeeded],
        vec![
            json!({"wiring_exit": 1, "wiring_error": "flaky"}),
            verdict("pass", "meets the acceptance"),
        ],
    );
    let outcome = wiring.supervise_once();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    landed(&wiring, task);
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"review_failed"), "{kinds:?}");
    let prompts = wiring.agent.review_prompts();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0], prompts[1]);
    let ends = payloads(&detail, "review_job_finished");
    assert_eq!(ends.len(), 2, "{kinds:?}");
    assert_eq!(ends[0]["attempt"], 1);
    assert_eq!(ends[0]["outcome"], "job_failed");
    assert_eq!(ends[0]["runs_again"], true);
    assert!(
        ends[0]["error"]
            .as_str()
            .unwrap()
            .contains("exited with exit status: 1: flaky"),
        "{ends:?}"
    );
    assert_eq!(ends[1]["outcome"], "answered");
    assert_eq!(ends[1]["verdict"], "pass");
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    let jobs = payloads(&detail, "review_job_started");
    let again: Vec<_> = jobs.iter().map(|job| &job["again"]).collect();
    assert_eq!(again, [&json!(false), &json!(true)]);
    assert_eq!(jobs[1]["output"], "review-1.again");
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["attempt"], 1);
    assert_eq!(finished[0]["verdict"], "pass");
    assert!(position(&kinds, "review_job_finished") < position(&kinds, "review_finished"));
    assert!(queue.asks(Default::default()).unwrap().is_empty());
}

#[test]
fn a_failed_review_span_ends_with_its_job_not_with_the_exit() {
    let (wiring, task) = reviewed(
        "a_failed_review_span_ends_with_its_job_not_with_the_exit",
        vec![succeeded],
        vec![
            json!("no verdict here"),
            json!({"wiring_start_failed": true}),
        ],
    );
    let outcome = wiring.supervise_once();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let event = |kind: &str| {
        detail
            .events
            .iter()
            .rev()
            .find(|e| e.kind.as_str() == kind)
            .unwrap_or_else(|| panic!("no {kind} in {:?}", event_kinds(&detail)))
    };
    // Reviewed once more with the same input, so the same prompt.
    let prompts = wiring.agent.review_prompts();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0], prompts[1]);
    let ends = payloads(&detail, "review_job_finished");
    assert_eq!(ends.len(), 2);
    assert_eq!(ends[0]["attempt"], 1);
    assert!(
        ends[0]["error"]
            .as_str()
            .unwrap()
            .contains("the review printed no verdict JSON"),
        "{ends:?}"
    );
    assert_eq!(ends[0]["outcome"], "unreadable");
    assert_eq!(ends[1]["outcome"], "start_failed");
    let reviews_closed: Vec<_> = detail
        .events
        .iter()
        .filter(|e| e.kind == EventKind::SessionClosed && e.payload["kind"] == "review")
        .collect();
    assert_eq!(reviews_closed.len(), 1);
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
        1
    );
    let closed = reviews_closed.last().unwrap();
    let failed = event("review_failed");
    // Closed before the exit was even asked, and before the session
    // exited and `review_failed` was recorded.
    assert!(closed.id < event("exit_requested").id);
    assert!(event("exit_requested").id < event("session_exited").id);
    assert!(event("session_exited").id < failed.id);
    // The ask goes with the failure as before.
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(failed.payload["ask_id"], asks[0].id.as_i64());
    assert_eq!(failed.payload["attempt"], 1);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "ask_opened") < position(&kinds, "review_failed"));
    // The run that could not start wrote nothing and is not named; the one
    // before it ran and is.
    let run_dir = detail.runs[0].run_dir().unwrap();
    let question = &asks[0].question;
    for part in [
        "failed and gave no verdict (review 1): ".to_owned(),
        "the whole review: it could not start".to_owned(),
        "the test reviewer cannot start this review".to_owned(),
        format!("its earlier run's output is in {run_dir}/review-1.out and .err"),
        format!("Review material: {run_dir}/review.md"),
        format!("Review output: {run_dir}/review-1.out, {run_dir}/review-1.err"),
    ] {
        assert!(question.contains(&part), "{part} in {question}");
    }
    assert!(
        wiring
            .files
            .exists(Path::new(&format!("{run_dir}/review-1.out")))
    );
    assert!(
        !wiring
            .files
            .exists(Path::new(&format!("{run_dir}/review-1.again.out")))
    );
    assert!(!question.contains("review-1.again"), "{question}");
}

#[test]
fn a_retried_review_that_waited_at_a_wall_and_fails_again_asks_a_person() {
    let (wiring, task) = reviewed(
        "retry_wall",
        vec![succeeded],
        vec![
            json!({"wiring_exit": 3}),
            login_wall(),
            json!({"wiring_exit": 3, "wiring_error": "broken again"}),
            verdict("pass", "unused"),
        ],
    );
    let mut answered = false;
    wiring.supervise_once_while(|queue| {
        let Some(ask) = queue
            .asks(Default::default())
            .unwrap()
            .into_iter()
            .find(|a| a.kind == AskKind::QueueHold)
        else {
            return false;
        };
        queue.answer(ask.id, "done").unwrap();
        answered = true;
        true
    });
    assert!(answered);
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    assert_eq!(wiring.agent.review_prompts().len(), 3);
    assert_eq!(
        payloads(&detail, "review_started")
            .iter()
            .map(|p| p["attempt"].clone())
            .collect::<Vec<_>>(),
        [json!(1)]
    );
    let ends = payloads(&detail, "review_job_finished");
    assert_eq!(
        ends.iter()
            .map(|p| (p["again"].clone(), p["outcome"].clone()))
            .collect::<Vec<_>>(),
        [
            (json!(false), json!("job_failed")),
            (json!(true), json!("held")),
            (json!(true), json!("job_failed"))
        ]
    );
    assert_eq!(ends[2]["runs_again"], false);
    assert!(payloads(&detail, "review_finished").is_empty());
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["attempt"], 1);
    assert_eq!(failed[0]["code"], "job_failed");
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap()
            .contains("exited with exit status: 3: broken again")
    );
    let asks: Vec<_> = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .filter(|a| a.kind == AskKind::ApproveLanding)
        .collect();
    assert_eq!(asks.len(), 1);
    assert_eq!(failed[0]["ask_id"], json!(asks[0].id));
}

/// A revise changes HEAD but mistakenly names its parent in the receipt.
fn stale_receipt(run: &RunId, worktree: &Path, git: &super::git::FakeGit) -> anyhow::Result<Value> {
    let mut receipt = succeeded_at_the_head(run, worktree, git)?;
    git.commit_in(worktree)?;
    receipt["summary"] = json!("fixed but named the earlier head");
    Ok(receipt)
}

#[test]
fn a_revise_receipt_for_another_commit_is_sent_back_to_the_session_until_it_names_head() {
    let (wiring, task) = reviewed(
        "stale_receipt",
        vec![succeeded, stale_receipt, succeeded_at_the_head],
        vec![
            finding("revise", "add a line", "one gap"),
            verdict("pass", "fixed"),
        ],
    );
    wiring.supervise_once();
    let detail = landed(&wiring, task);
    let rejected = payloads(&detail, "revise_receipt_rejected");
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0]["attempt"], 1);
    assert!(
        rejected[0]["reason"]
            .as_str()
            .unwrap()
            .contains("but the worktree HEAD is")
    );
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 2);
    assert!(validated.iter().all(|v| v["accepted"] == true));
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "revise_receipt_rejected") < position(&kinds, "revise_finished"));
    let prompts = wiring.agent.turn_prompts();
    assert_eq!(prompts.len(), 3);
    assert!(prompts[2].contains(&format!(
        "dagq: the receipt you rewrote for revise 1 of run {} cannot be accepted",
        detail.runs[0].id()
    )));
    assert!(prompts[2].contains("git rev-parse HEAD"));
    assert_eq!(payloads(&detail, "exit_requested").len(), 1);
}

#[test]
fn a_worker_question_asked_while_revising_is_answered_and_the_run_lands() {
    let (wiring, task) = reviewed(
        "revise_question",
        vec![succeeded, no_receipt, succeeded],
        vec![
            finding("revise", "say which line", "one gap"),
            verdict("pass", "fixed"),
        ],
    );
    let agent = wiring.agent.clone();
    let db = wiring.db.clone();
    wiring.agent.on_review(Box::new(move || {
        agent.on_next_turn(Box::new(move || {
            let mut queue = SqliteQueue::open(&db).unwrap();
            let run = queue.show(task).unwrap().runs[0].clone();
            queue
                .ask(new_ask(
                    AskKind::WorkerQuestion,
                    Some(task),
                    Some(run.id().clone()),
                    "Which line?",
                ))
                .unwrap();
        }));
    }));
    // The session goes idle at its question, and the supervisor passes over
    // that idle some times: the revise waits for the answer.
    let mut idle_at = None;
    let mut waiting = None;
    let mut answered = None;
    wiring.supervise_once_while(|queue| {
        let Some(ask) = queue
            .asks(Default::default())
            .unwrap()
            .into_iter()
            .find(|a| a.kind == AskKind::WorkerQuestion)
        else {
            return false;
        };
        let detail = queue.show(task).unwrap();
        if payloads(&detail, "turn_finished").len() < 2 {
            return false;
        }
        let idle_at = *idle_at.get_or_insert(wiring.passes());
        if wiring.passes() < idle_at + 5 {
            return false;
        }
        waiting = Some((detail, queue.asks(Default::default()).unwrap().len()));
        queue.answer(ask.id, "the second").unwrap();
        answered = Some(ask.id);
        true
    });
    let ask = answered.expect("answered during the revise");
    let (detail, open_asks) = waiting.expect("looked at while it waited");
    assert_eq!(payloads(&detail, "revise_requested").len(), 1);
    assert!(payloads(&detail, "revise_finished").is_empty());
    assert!(payloads(&detail, "exit_requested").is_empty());
    assert_eq!(open_asks, 1);
    let detail = landed(&wiring, task);
    let answers = payloads(&detail, "ask_answered");
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0]["runtime_delivers"], true);
    let prompts = wiring.agent.turn_prompts();
    assert_eq!(prompts.len(), 3);
    assert!(prompts[2].starts_with(&format!("answer to ask {ask}: the second")));
    assert!(wiring.queue().read_ask(ask).unwrap().closed_at.is_some());
    assert_eq!(
        payloads(&detail, "ask_delivered"),
        [&json!({"ask_id": ask, "workspace_id": detail.runs[0].workspace_id().unwrap()})]
    );
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
    assert!(wiring.queue().asks(Default::default()).unwrap().is_empty());
}

fn new_ask(
    kind: AskKind,
    task_id: Option<TaskId>,
    run_id: Option<RunId>,
    question: &str,
) -> crate::domain::NewAsk {
    crate::domain::NewAsk {
        recommendation: None,
        confidence: None,
        topics: if kind == AskKind::WorkerQuestion {
            vec!["acceptance_conflict".into()]
        } else {
            vec![]
        },
        kind,
        task_id,
        run_id,
        question: question.into(),
        options: vec![],
        asked_by: "worker".into(),
        reason_category: crate::domain::AskReason::Scope,
        finding_id: None,
        request_id: None,
    }
}

#[test]
fn the_default_test_reviewer_skips_retry_but_still_asks_after_its_job() {
    let wiring = Wiring::new(
        "no_retry",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![json!("no verdict here")],
        },
    );
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert!(payloads(&detail, "review_retried").is_empty());
    assert_eq!(wiring.agent.review_prompts().len(), 1);
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["attempt"], 1);
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].kind, AskKind::ApproveLanding);
    assert_eq!(failed[0]["ask_id"], json!(asks[0].id));
    assert!(asks[0].question.contains("review-1.out"));
    assert!(!asks[0].question.contains("review-2.out"));
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    let closed: Vec<_> = detail
        .events
        .iter()
        .filter(|e| e.kind == EventKind::SessionClosed && e.payload["kind"] == "review")
        .collect();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].payload["reason"], "job_finished");
    assert!(
        closed[0].id
            < detail
                .events
                .iter()
                .find(|e| e.kind == EventKind::ReviewFailed)
                .unwrap()
                .id
    );
}
