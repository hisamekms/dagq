//! Review job scheduling, missing answers and provider waits reach the
//! queue and the next round's prompts through the production supervisor.

use super::{
    agent::{Script, login_wall, succeeded, verdict},
    harness::{Wiring, connect},
};
use crate::{
    application::{RunFiles, TaskReads},
    domain::{EventKind, Provider, RunStatus, TaskDetail, TaskStatus},
};
use serde_json::{Value, json};
use std::{path::Path, sync::mpsc};

const DESIGN: &str = "---\ndescription: design checks\n---\nCheck the design.\n";
const TESTS: &str = "---\ndescription: test checks\n---\nCheck the tests.\n";
const CONFIG: &str = "[review.subagents.design]\npaths = ['change.txt']\n[review.subagents.tests]\npaths = ['change.txt']\n[review.programs.\"builtin:secrets\"]\nenabled = false\n";

fn payloads<'a>(detail: &'a TaskDetail, kind: &str) -> Vec<&'a Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind.as_str() == kind)
        .map(|e| &e.payload)
        .collect()
}

fn setup(wiring: &Wiring, config: &str) {
    wiring.verifier.set_config(config);
    wiring.git.commit_files(
        &[
            ("dagq.toml", config),
            (".dagq/agents/design/AGENT.md", DESIGN),
            (".dagq/agents/tests/AGENT.md", TESTS),
        ],
        &["change.txt"],
    );
}

fn landing_ask(wiring: &Wiring) -> crate::domain::Ask {
    let asks = wiring.queue().asks(Default::default()).unwrap();
    let open: Vec<_> = asks.into_iter().filter(|a| a.closed_at.is_none()).collect();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].kind.as_str(), "approve_landing");
    open[0].clone()
}

/// A selected definition is required before any review job starts.
#[test]
fn a_selected_agent_without_its_definition_fails_the_review() {
    let wiring = Wiring::new(
        "missing_review_agent",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![verdict("pass", "would pass")],
        },
    );
    wiring
        .git
        .commit_files(&[("dagq.toml", CONFIG)], &["change.txt"]);
    let main = wiring.git.main();
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let detail = wiring.queue().show(task).unwrap();
    assert!(wiring.agent.review_prompts().is_empty());
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(payloads(&detail, "review_started").is_empty());
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1);
    let why = format!(
        "the review's required subagents could not be read: the review subagent design that dagq.toml names has no definition .dagq/agents/design/AGENT.md in the landing branch's commit {main}"
    );
    assert!(
        failed[0]["error"].as_str().unwrap().contains(&why),
        "{failed:?}"
    );
    assert!(landing_ask(&wiring).question.contains(&why));
}

/// Every job starts before the held whole review ends; rows, definitions,
/// results and the machine's route survive the wiring.
#[test]
fn a_review_runs_its_whole_review_and_each_agent_as_jobs_side_by_side() {
    let wiring = Wiring::new(
        "parallel_review_agents",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![verdict("pass", "meets the acceptance")],
        },
    );
    setup(&wiring, CONFIG);
    wiring.agent.review_agent_says("design", &[("pass", &[])]);
    wiring.agent.review_agent_says("tests", &[("pass", &[])]);
    let (release, gate) = mpsc::channel();
    wiring.agent.hold_next_review(gate);
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once_while(|queue| {
        let detail = queue.show(task).unwrap();
        if payloads(&detail, "review_job_started").len() != 3 {
            return false;
        }
        assert!(
            !payloads(&detail, "review_job_finished")
                .iter()
                .any(|p| p["agent"].is_null())
        );
        release.send(()).unwrap();
        true
    });
    let detail = wiring.queue().show(task).unwrap();
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::Integrated,
        "{:?}",
        detail.events
    );
    let kinds: Vec<_> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    let first_end = kinds
        .iter()
        .position(|k| *k == "review_job_finished")
        .unwrap();
    assert_eq!(
        kinds
            .iter()
            .take(first_end)
            .filter(|k| **k == "review_job_started")
            .count(),
        3
    );
    let rows: Vec<(String, Option<String>)> = connect(&wiring.db)
        .prepare("SELECT kind,label FROM headless_jobs ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        rows,
        [
            ("review".into(), None),
            ("review".into(), Some("design".into())),
            ("review".into(), Some("tests".into()))
        ]
    );
    let prompts = wiring.agent.agent_prompts();
    assert_eq!(prompts.len(), 2);
    for (prompt, definition) in prompts.iter().zip([DESIGN, TESTS]) {
        assert!(
            prompt.contains(&format!("<definition>\n{definition}\n</definition>")),
            "{prompt}"
        );
    }
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(
        finished[0]["agents"],
        json!([
            {"agent":"design","status":"completed","verdict":"pass","reasons":[],"summary":"design looked"},
            {"agent":"tests","status":"completed","verdict":"pass","reasons":[],"summary":"tests looked"}
        ])
    );
    assert_eq!(
        finished[0]["route"],
        json!({"destination":"land","parent":"land","parent_lighter":false,"agents":[
            {"agent":"design","destination":"land","escalated_because":null}, {"agent":"tests","destination":"land","escalated_because":null}
        ]})
    );
    for end in payloads(&detail, "review_job_finished") {
        assert!(end.get("tokens_source").is_some(), "{end}");
    }
    assert!(payloads(&detail, "review_retried").is_empty());
}

/// An unreadable agent alone retries once, then asks without judging.
#[test]
fn an_agent_that_never_answers_fails_the_review_and_no_other_job_runs_again() {
    let wiring = Wiring::new(
        "unanswered_review_agent",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![verdict("pass", "meets the acceptance")],
        },
    )
    .with_review_retry();
    setup(&wiring, CONFIG);
    wiring.agent.review_agent_says("design", &[("pass", &[])]);
    wiring
        .agent
        .review_agent_answers("tests", vec![Value::Null]);
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let detail = wiring.queue().show(task).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(payloads(&detail, "review_finished").is_empty());
    let ends: Vec<_> = payloads(&detail, "review_job_finished")
        .iter()
        .map(|p| {
            (
                p["agent"].clone(),
                p["outcome"].clone(),
                p["runs_again"].clone(),
            )
        })
        .collect();
    assert_eq!(ends.iter().filter(|e| e.0 == "tests").count(), 2);
    for again in [true, false] {
        assert!(
            ends.contains(&(json!("tests"), json!("unreadable"), json!(again))),
            "{ends:?}"
        );
    }
    assert_eq!(ends.iter().filter(|e| e.0 == "design").count(), 1);
    assert_eq!(ends.iter().filter(|e| e.0.is_null()).count(), 1);
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1);
    let error = failed[0]["error"].as_str().unwrap();
    let run_dir = detail.runs[0].run_dir().unwrap();
    for part in [
        "the review's jobs did not all give an answer: the agent tests: ".to_owned(),
        format!("its output is in {run_dir}/review-1-agent-tests.again.out and .err"),
    ] {
        assert!(error.contains(&part), "{error}");
    }
    assert_eq!(
        landing_ask(&wiring).run_id.as_ref(),
        Some(detail.runs[0].id())
    );
}

/// Two reviews share one slot even while one whole review is held open.
#[test]
fn the_reviews_of_two_runs_share_the_limit_of_jobs_at_once() {
    let wiring = Wiring::new(
        "shared_review_job_limit",
        Script {
            receipts: vec![succeeded; 2],
            verdicts: vec![verdict("concern", "a person looks")],
        },
    )
    .with_review_concurrency(1);
    setup(&wiring, CONFIG);
    let tasks = [wiring.ready_task(&["true"]), wiring.ready_task(&["true"])];
    let (release, gate) = mpsc::channel();
    wiring.agent.hold_next_review(gate);
    wiring.supervise_once_while(|queue| {
        if !tasks
            .iter()
            .all(|t| !payloads(&queue.show(*t).unwrap(), "review_started").is_empty())
        {
            return false;
        }
        let events = queue.all_events().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind.as_str() == "review_job_started")
                .count(),
            1
        );
        assert!(
            !events
                .iter()
                .any(|e| e.kind.as_str() == "review_job_finished")
        );
        release.send(()).unwrap();
        true
    });
    let events = wiring.queue().all_events().unwrap();
    let (mut running, mut most, mut started) = (0, 0, 0);
    for e in &events {
        match e.kind.as_str() {
            "review_job_started" => {
                running += 1;
                started += 1;
            }
            "review_job_finished" => running -= 1,
            _ => continue,
        }
        most = most.max(running);
    }
    assert_eq!((started, most, running), (6, 1, 0));
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::ReviewFinished)
            .count(),
        2
    );
    for task in tasks {
        assert_eq!(
            wiring.queue().show(task).unwrap().runs[0].status(),
            RunStatus::AwaitingIntegration
        );
    }
}

/// Only the Codex job waits on its login wall with fallback disabled;
/// answered peers stay answered and it retries on Codex when released.
#[test]
fn with_the_fallback_off_an_agent_whose_codex_login_ran_out_waits_for_it_and_the_review_lands() {
    let wiring = Wiring::new(
        "codex_agent_waits",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![verdict("pass", "meets the acceptance")],
        },
    )
    .with_codex()
    .without_job_fallback();
    let config = CONFIG.replace(
        "[review.subagents.design]",
        "[review.subagents.design]\nprovider = 'codex'",
    );
    setup(&wiring, &config);
    wiring.agent.review_agent_answers(
        "design",
        vec![
            login_wall(),
            json!({"verdict":"pass", "reasons":[], "summary":"codex agent passed"}),
        ],
    );
    wiring.agent.review_agent_says("tests", &[("pass", &[])]);
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once_while(|queue| {
        let detail = queue.show(task).unwrap();
        let Some(held) = queue.all_events().unwrap().into_iter().find(|e| e.kind == EventKind::ProviderHeld) else { return false; };
        let answered: Vec<_> = payloads(&detail, "review_job_finished").iter().filter(|p| p["outcome"] == "answered").map(|p| p["agent"].clone()).collect();
        if answered.len() != 2 { return false; }
        assert_eq!(answered, [Value::Null, json!("tests")]);
        assert!(payloads(&detail, "review_finished").is_empty());
        assert_eq!(held.payload["provider"], "codex");
        assert_eq!(held.payload["reason"], "authentication");
        queue.record_queue_event(EventKind::ProviderReleased, json!({"provider":"codex", "reason":"authentication", "since":held.payload["since"], "why":"retry_due"})).unwrap();
        true
    });
    let detail = wiring.queue().show(task).unwrap();
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::Integrated,
        "{:?}",
        detail.events
    );
    let starts = payloads(&detail, "review_job_started");
    let actual: Vec<_> = starts
        .iter()
        .map(|p| {
            json!([
                p["agent"],
                p["launch"]["provider"],
                p["launch"]["source"],
                p["launch"]["switched_from"]
            ])
        })
        .collect();
    assert_eq!(
        actual,
        [
            json!([null, "claude", "default", null]),
            json!(["design", "codex", "review.subagents", null]),
            json!(["tests", "claude", "default", null]),
            json!(["design", "codex", "review.subagents", null])
        ]
    );
    let ends = payloads(&detail, "review_job_finished");
    assert!(
        ends.iter()
            .any(|p| p["agent"] == "design" && p["outcome"] == "held")
    );
    assert_eq!(ends.last().unwrap()["agent"], "design");
    assert_eq!(ends.last().unwrap()["outcome"], "answered");
    assert!(payloads(&detail, "review_failed").is_empty());
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished[0]["agents"][0]["summary"], "codex agent passed");
    assert_eq!(finished[0]["agents"][1]["summary"], "tests looked");
    assert!(
        wiring
            .queue()
            .all_events()
            .unwrap()
            .iter()
            .filter(|e| e.kind == EventKind::ProviderHeld)
            .all(|e| e.payload["provider"] == "codex")
    );
}

/// A whole review waiting for Claude actually starts in the session its
/// span named, while the Codex agent can answer during that wait.
#[test]
fn a_whole_review_that_waited_for_claude_starts_in_the_session_its_span_names() {
    use crate::domain::provider_switch::{ProviderHold, SwitchReason};
    let wiring = Wiring::new(
        "whole_review_waits",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![verdict("pass", "meets the acceptance")],
        },
    )
    .with_codex()
    .without_job_fallback();
    setup(
        &wiring,
        "[roles.review]\nprovider = 'claude'\n[review.subagents.design]\npaths = ['change.txt']\nprovider = 'codex'\n[review.programs.\"builtin:secrets\"]\nenabled = false\n",
    );
    let hold = ProviderHold::new(
        Provider::Claude,
        SwitchReason::LaunchFailed,
        i64::try_from(super::host::STOPPED_AT.as_secs()).unwrap(),
    );
    let db = wiring.db.clone();
    let held = hold.held_payload(None);
    wiring.agent.on_next_turn(Box::new(move || {
        crate::infrastructure::sqlite::SqliteQueue::open(&db)
            .unwrap()
            .record_queue_event(EventKind::ProviderHeld, held)
            .unwrap();
    }));
    let (release_worker, gate) = mpsc::channel();
    wiring.agent.hold_next_turn(gate);
    let task = wiring.ready_task(&["true"]);
    let mut expected_session = None;
    let mut held_at_pass = None;
    let mut worker_released = false;
    wiring.supervise_once_while(|queue| {
        // The hold is recorded by the worker's start. Let the supervisor
        // read it on a later pass before the worker writes its receipt.
        if !worker_released {
            if !queue
                .all_events()
                .unwrap()
                .iter()
                .any(|e| e.kind == EventKind::ProviderHeld)
            {
                return false;
            }
            let at = *held_at_pass.get_or_insert_with(|| wiring.passes());
            if wiring.passes() <= at + 1 {
                return false;
            }
            release_worker.send(()).unwrap();
            worker_released = true;
        }
        let detail = queue.show(task).unwrap();
        if !payloads(&detail, "review_job_finished")
            .iter()
            .any(|p| p["agent"] == "design" && p["outcome"] == "answered")
        {
            return false;
        }
        let started = payloads(&detail, "review_started");
        assert_eq!(started.len(), 1);
        let jobs = payloads(&detail, "review_job_started");
        assert_eq!(jobs.len(), 1, "{jobs:?}");
        assert_eq!(jobs[0]["agent"], "design");
        assert_eq!(jobs[0]["launch"]["provider"], "codex");
        assert!(payloads(&detail, "review_finished").is_empty());
        expected_session = Some(started[0]["session_id"].as_str().unwrap().to_owned());
        queue
            .record_queue_event(
                EventKind::ProviderReleased,
                hold.released_payload("retry_due"),
            )
            .unwrap();
        true
    });
    let session = expected_session.unwrap();
    let detail = wiring.queue().show(task).unwrap();
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::Integrated,
        "{:?}",
        detail.events
    );
    assert_eq!(
        wiring.agent.review_sessions().as_slice(),
        std::slice::from_ref(&session)
    );
    let jobs = payloads(&detail, "review_job_started");
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[1]["agent"], Value::Null);
    assert_eq!(jobs[1]["launch"]["provider"], "claude");
    assert_eq!(jobs[1]["launch"]["source"], "dagq.toml");
    assert_eq!(jobs[1]["launch"]["switched_from"], Value::Null);
    assert_eq!(jobs[1]["session_id"], session);
    assert!(payloads(&detail, "review_failed").is_empty());
    let opened: Vec<_> = payloads(&detail, "session_opened")
        .into_iter()
        .filter(|p| p["kind"] == "review")
        .collect();
    let closed: Vec<_> = payloads(&detail, "session_closed")
        .into_iter()
        .filter(|p| p["kind"] == "review")
        .collect();
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0]["session_id"], session);
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["reason"], "job_finished");
}

/// A person's answer is material for the resumed round's whole review
/// and agents, exactly once, including the prompt persisted for the job.
#[test]
fn a_persons_landing_answer_reaches_the_next_rounds_review_and_its_agents() {
    const ACCEPTED: &str = "A person already decided the concerns these answers accept";
    let wiring = Wiring::new(
        "review_agents_landing_answer",
        Script {
            receipts: vec![succeeded; 2],
            verdicts: vec![
                json!({"verdict":"concern", "reasons":["adds a permission the task did not name"], "summary":"scope"}),
                verdict("pass", "fixed"),
            ],
        },
    );
    setup(
        &wiring,
        "[review.subagents.design]\npaths = ['change.txt']\n[review.programs.\"builtin:secrets\"]\nenabled = false\n",
    );
    wiring.agent.review_agent_says("design", &[("pass", &[])]);
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let ask = landing_ask(&wiring);
    let answer = "send_back: the added permission is as designed; narrow the change";
    wiring.queue().answer(ask.id, answer).unwrap();
    wiring.supervise_once();
    let detail = wiring.queue().show(task).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(payloads(&detail, "resume_started").len(), 1);
    let line = format!("- ask {}, after review 1: {answer}\n", ask.id);
    let whole = wiring.agent.review_prompts();
    assert_eq!(whole.len(), 2);
    let agents: Vec<_> = wiring
        .agent
        .agent_prompts()
        .into_iter()
        .filter(|p| p.contains("You are the review agent `design`"))
        .collect();
    assert_eq!(agents.len(), 2);
    for prompts in [&whole, &agents] {
        assert!(
            !prompts[0].contains(&line) && !prompts[0].contains(ACCEPTED),
            "{}",
            prompts[0]
        );
        assert_eq!(prompts[1].matches(&line).count(), 1, "{}", prompts[1]);
        assert_eq!(prompts[1].matches(ACCEPTED).count(), 1, "{}", prompts[1]);
    }
    let run_dir = detail.runs[0].run_dir().unwrap();
    assert_eq!(
        wiring
            .files
            .read_to_string(Path::new(&format!(
                "{run_dir}/review-2-agents/design/prompt.txt"
            )))
            .unwrap(),
        agents[1]
    );
    let material = wiring
        .files
        .read_to_string(Path::new(&format!("{run_dir}/review.md")))
        .unwrap();
    let section = material
        .split("## A person's answers to this run's landing asks")
        .nth(1)
        .and_then(|s| s.split("## Commits").next())
        .unwrap();
    assert!(section.contains(&line), "{section}");
    assert!(
        !section.contains("adds a permission the task did not name"),
        "{section}"
    );
}
