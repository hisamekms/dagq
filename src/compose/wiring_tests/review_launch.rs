//! A review agent's table that writes its provider, model and effort
//! (a project's agent and a built-in's alike) wired from the landing
//! branch's `dagq.toml` to the agent's job: the job is started with that
//! model and effort as its provider's arguments, and its start, its finish
//! and the review's end record them.

use serde_json::{Value, json};

use super::{
    agent::{Script, succeeded, verdict},
    harness::Wiring,
};
use crate::{application::TaskReads, domain::TaskDetail};

/// The payloads of `detail`'s events of `kind`, oldest first.
fn payloads<'a>(detail: &'a TaskDetail, kind: &str) -> Vec<&'a Value> {
    detail
        .events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| &event.payload)
        .collect()
}

/// `[roles.review]` on Claude's Opus at `medium`; `design` and
/// `builtin:security` both required for `change.txt`, each writing a
/// provider, a model and an effort of its own; no program review runs.
const CONFIG: &str = "[roles.review]\nprovider = \"claude\"\nmodel = \"claude-opus-5-5\"\neffort = \"medium\"\n\
[review.subagents.design]\npaths = [\"change.txt\"]\nprovider = \"claude\"\nmodel = \"claude-haiku-5-5\"\neffort = \"low\"\n\
[review.subagents.\"builtin:security\"]\npaths = [\"change.txt\"]\nprovider = \"claude\"\nmodel = \"claude-sonnet-5-5\"\neffort = \"xhigh\"\n\
[review.programs.\"builtin:secrets\"]\nenabled = false\n";

/// A run whose change requires `design` and `builtin:security`: each
/// agent's job starts with the model and effort its table writes, the
/// whole review's with `[roles.review]`'s, and each job's
/// `review_job_started` launch, its `review_job_finished` and the review's
/// `launches` say which.
#[test]
fn an_agents_written_model_and_effort_reach_its_jobs_arguments_and_records() {
    let wiring = Wiring::new(
        "an_agents_written_model_and_effort_reach_its_jobs_arguments_and_records",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![verdict("concern", "a person looks")],
        },
    );
    wiring.git.commit_files(
        &[
            ("dagq.toml", CONFIG),
            (
                ".dagq/agents/design/AGENT.md",
                "---\ndescription: design checks\n---\nCheck the design.\n",
            ),
        ],
        &["change.txt"],
    );
    wiring.verifier.set_config(CONFIG);
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();

    let detail = wiring.queue().show(task).unwrap();
    let kinds: Vec<&str> = detail
        .events
        .iter()
        .map(|event| event.kind.as_str())
        .collect();
    let haiku = ["--model", "claude-haiku-5-5", "--effort", "low"];
    let sonnet = ["--model", "claude-sonnet-5-5", "--effort", "xhigh"];
    let jobs = wiring.agent.agent_jobs();
    assert_eq!(jobs.len(), 2, "{jobs:?} {kinds:?}");
    let has = |args: &[String], wanted: &[&str]| {
        args.windows(wanted.len())
            .any(|window| window.iter().map(String::as_str).eq(wanted.iter().copied()))
    };
    assert!(has(&jobs[0], &haiku), "{jobs:?}");
    assert!(has(&jobs[1], &sonnet), "{jobs:?}");
    let launches: Vec<(Value, Value, Value, Value, Value)> =
        payloads(&detail, "review_job_started")
            .iter()
            .map(|job| {
                let launch = &job["launch"];
                (
                    job["agent"].clone(),
                    launch["provider"].clone(),
                    launch["model"].clone(),
                    launch["effort"].clone(),
                    launch["source"].clone(),
                )
            })
            .collect();
    assert_eq!(
        launches,
        [
            (
                Value::Null,
                json!("claude"),
                json!("claude-opus-5-5"),
                json!("medium"),
                json!("dagq.toml")
            ),
            (
                json!("design"),
                json!("claude"),
                json!("claude-haiku-5-5"),
                json!("low"),
                json!("review.subagents")
            ),
            (
                json!("builtin:security"),
                json!("claude"),
                json!("claude-sonnet-5-5"),
                json!("xhigh"),
                json!("review.subagents")
            ),
        ],
        "{kinds:?}"
    );
    let finished: Vec<(Value, Value, Value, Value)> = payloads(&detail, "review_job_finished")
        .iter()
        .filter(|job| !job["agent"].is_null())
        .map(|job| {
            (
                job["agent"].clone(),
                job["provider"].clone(),
                job["model"].clone(),
                job["effort"].clone(),
            )
        })
        .collect();
    assert_eq!(
        finished,
        [
            (
                json!("design"),
                json!("claude"),
                json!("claude-haiku-5-5"),
                json!("low")
            ),
            (
                json!("builtin:security"),
                json!("claude"),
                json!("claude-sonnet-5-5"),
                json!("xhigh")
            ),
        ]
    );
    let reviewed = payloads(&detail, "review_finished");
    assert_eq!(reviewed.len(), 1, "{kinds:?}");
    assert_eq!(
        reviewed[0]["launches"],
        json!({
            "design": {"provider": "claude", "model": "claude-haiku-5-5", "effort": "low"},
            "builtin:security": {"provider": "claude", "model": "claude-sonnet-5-5", "effort": "xhigh"},
        })
    );
}
