//! A review agent's table that writes its provider, model and effort
//! (a project's agent and a built-in's alike) wired from the landing
//! branch's `dagq.toml` to the agent's job: the job is started with that
//! model and effort as its provider's arguments, and its start, its finish
//! and the review's end record them.

use serde_json::{Value, json};

use super::{
    agent::{Script, already_satisfied, succeeded, verdict},
    harness::Wiring,
};
use crate::{
    application::TaskReads,
    domain::{Provider, TaskDetail},
};

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
            "design": {"provider": "claude", "model": "claude-haiku-5-5", "effort": "low", "planned_provider": "claude"},
            "builtin:security": {"provider": "claude", "model": "claude-sonnet-5-5", "effort": "xhigh", "planned_provider": "claude"},
        })
    );
}

/// `[roles.review]` on Codex, `design-layers` on Claude and `tests`
/// inheriting the role's Codex, both required for `change.txt`, and the
/// trial of the provider pair drawing `percent` of the tasks into its
/// treatment (whose review is Claude's).
fn pair_config(percent: u8) -> String {
    format!(
        "[roles.review]\nprovider = \"codex\"\nmodel = \"gpt-5.5\"\neffort = \"high\"\n\
         [review.subagents.design-layers]\npaths = [\"change.txt\"]\nprovider = \"claude\"\n\
         [review.subagents.tests]\npaths = [\"change.txt\"]\neffort = \"xhigh\"\n\
         [review.programs.\"builtin:secrets\"]\nenabled = false\n\
         [worker.provider_trial]\nenabled = true\nname = \"pairs\"\ntreatment_percent = {percent}\n"
    )
}

/// One task's run reviewed under `pair_config(percent)` by a supervisor
/// that runs Claude and Codex, `held` held for its usage limit: each
/// review job's `review_job_started` as (agent, provider, model, effort,
/// planned provider, switched from, switch reason), the review jobs'
/// records and the review's end.
fn reviewed(
    name: &str,
    percent: u8,
    held: Option<Provider>,
) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
    let config = pair_config(percent);
    let wiring = Wiring::new(
        name,
        Script {
            receipts: vec![succeeded],
            verdicts: vec![verdict("concern", "a person looks")],
        },
    )
    .with_codex();
    wiring.git.commit_files(
        &[
            ("dagq.toml", config.as_str()),
            (
                ".dagq/agents/design-layers/AGENT.md",
                "---\ndescription: layer checks\n---\nCheck the layers.\n",
            ),
            (
                ".dagq/agents/tests/AGENT.md",
                "---\ndescription: test checks\n---\nCheck the tests.\n",
            ),
        ],
        &["change.txt"],
    );
    wiring.verifier.set_config(&config);
    let task = wiring.ready_task(&["true"]);
    if let Some(provider) = held {
        wiring.hold(provider);
    }
    wiring.supervise_once();
    let detail = wiring.queue().show(task).unwrap();
    let owned =
        |kind: &str| -> Vec<Value> { payloads(&detail, kind).into_iter().cloned().collect() };
    let started = owned("review_job_started")
        .iter()
        .map(|job| {
            let launch = &job["launch"];
            json!([
                job["agent"],
                launch["provider"],
                launch["model"],
                launch["effort"],
                launch["planned_provider"],
                launch["switched_from"],
                launch["switch_reason"],
                launch["pair_group"],
                job["pair_group"],
            ])
        })
        .collect();
    (
        started,
        owned("review_job_finished"),
        owned("review_finished"),
    )
}

/// A treatment's run (worker on Codex) has its whole review and its Codex
/// agent (`tests`, the role's provider) started on Claude with Claude's
/// default model and effort, and `design-layers` on Claude as configured;
/// each is on its planned provider with no `switched_from`, and every
/// record of the review names the run's group and each job's planned
/// provider (ADR-t4005-1 decisions 5 and 6).
#[test]
fn a_treatments_review_starts_on_claude_and_records_its_group_and_planned_provider() {
    let (started, finished, reviewed) = reviewed(
        "a_treatments_review_starts_on_claude_and_records_its_group_and_planned_provider",
        100,
        None,
    );
    let claude = |agent: Value, model: &str, effort: &str| {
        json!([
            agent,
            "claude",
            model,
            effort,
            "claude",
            null,
            null,
            "treatment",
            "treatment"
        ])
    };
    assert_eq!(
        started,
        [
            claude(Value::Null, "claude-opus-5-5", "medium"),
            claude(json!("design-layers"), "claude-opus-5-5", "high"),
            claude(json!("tests"), "claude-opus-5-5", "medium"),
        ],
        "{finished:?}"
    );
    assert_eq!(finished.len(), 3, "{finished:?}");
    for job in &finished {
        assert_eq!(
            (
                &job["provider"],
                &job["planned_provider"],
                &job["pair_group"]
            ),
            (&json!("claude"), &json!("claude"), &json!("treatment")),
            "{job}"
        );
    }
    assert_eq!(reviewed.len(), 1, "{reviewed:?}");
    assert_eq!(
        (&reviewed[0]["pair_group"], &reviewed[0]["planned_provider"]),
        (&json!("treatment"), &json!("claude")),
        "{}",
        reviewed[0]
    );
    assert_eq!(
        reviewed[0]["launches"]["tests"]["planned_provider"],
        json!("claude"),
        "{}",
        reviewed[0]
    );
}

/// A control's run keeps the review as configured, the whole review and
/// `tests` on Codex and `design-layers` on Claude: each on its planned
/// provider, none marked as switched.
#[test]
fn a_controls_review_mixes_its_configured_providers_without_a_switch() {
    let (started, _, reviewed) = reviewed(
        "a_controls_review_mixes_its_configured_providers_without_a_switch",
        0,
        None,
    );
    let on = |agent: Value, provider: &str, model: Value, effort: &str| {
        json!([
            agent, provider, model, effort, provider, null, null, "control", "control"
        ])
    };
    assert_eq!(
        started,
        [
            on(Value::Null, "codex", json!("gpt-5.5"), "high"),
            on(
                json!("design-layers"),
                "claude",
                json!("claude-opus-5-5"),
                "high"
            ),
            on(json!("tests"), "codex", json!("gpt-5.5"), "xhigh"),
        ]
    );
    assert_eq!(
        reviewed[0]["pair_group"],
        json!("control"),
        "{}",
        reviewed[0]
    );
}

/// With Claude held, a treatment's review jobs planned on Claude fall back
/// to Codex as any job does: their launch says it moved from the planned
/// provider and why.
#[test]
fn a_treatments_review_falls_back_from_its_planned_provider_with_a_reason() {
    let (started, finished, _) = reviewed(
        "a_treatments_review_falls_back_from_its_planned_provider_with_a_reason",
        100,
        Some(Provider::Claude),
    );
    assert!(!started.is_empty(), "{finished:?}");
    for job in &started {
        assert_eq!(
            (&job[1], &job[4], &job[5], &job[6], &job[7]),
            (
                &json!("codex"),
                &json!("claude"),
                &json!("claude"),
                &json!("usage_limit"),
                &json!("treatment")
            ),
            "{job}"
        );
    }
}

/// One task whose worker claims its base already meets it, judged under
/// `pair_config(percent)` by a supervisor that runs Claude and Codex,
/// `held` held for its usage limit: the claim judgment's
/// `run_satisfied_review_started`, each of its jobs' starts as (provider,
/// model, effort, planned provider, switched from, switch reason, the
/// launch's group, the start's group) and its jobs' ends.
fn judged(name: &str, percent: u8, held: Option<Provider>) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
    let config = pair_config(percent);
    let wiring = Wiring::new(
        name,
        Script {
            receipts: vec![already_satisfied],
            verdicts: vec![verdict("pass", "fine")],
        },
    )
    .with_codex();
    wiring
        .git
        .commit_files(&[("dagq.toml", config.as_str())], &[]);
    wiring.verifier.set_config(&config);
    let task = wiring.ready_task(&["true"]);
    if let Some(provider) = held {
        wiring.hold(provider);
    }
    wiring.supervise_once();
    let detail = wiring.queue().show(task).unwrap();
    let owned =
        |kind: &str| -> Vec<Value> { payloads(&detail, kind).into_iter().cloned().collect() };
    let started = owned("run_satisfied_review_job_started")
        .iter()
        .map(|job| {
            let launch = &job["launch"];
            json!([
                launch["provider"],
                launch["model"],
                launch["effort"],
                launch["planned_provider"],
                launch["switched_from"],
                launch["switch_reason"],
                launch["pair_group"],
                job["pair_group"],
            ])
        })
        .collect();
    (
        owned("run_satisfied_review_started"),
        started,
        owned("run_satisfied_review_job_finished"),
    )
}

/// The judgment of a treatment's `already_satisfied` claim is a review of
/// the run: with `[roles.review]` on Codex it starts on Claude, the
/// provider the pair assigns, with Claude's default model and effort and
/// no `switched_from`, and its records name the run's group and its
/// planned provider; a control's starts on Codex as configured.
#[test]
fn a_treatments_claim_judgment_starts_on_claude_and_a_controls_as_configured() {
    let (begun, started, finished) =
        judged("a_treatments_claim_judgment_starts_on_claude", 100, None);
    assert_eq!(begun.len(), 1, "{begun:?}");
    assert_eq!(begun[0]["pair_group"], json!("treatment"), "{}", begun[0]);
    assert_eq!(
        started,
        [json!([
            "claude",
            "claude-opus-5-5",
            "medium",
            "claude",
            null,
            null,
            "treatment",
            "treatment"
        ])],
        "{finished:?}"
    );
    assert!(!finished.is_empty());
    for job in &finished {
        assert_eq!(
            (
                &job["provider"],
                &job["planned_provider"],
                &job["pair_group"]
            ),
            (&json!("claude"), &json!("claude"), &json!("treatment")),
            "{job}"
        );
    }
    let (_, started, _) = judged("a_controls_claim_judgment_starts_as_configured", 0, None);
    assert_eq!(
        started,
        [json!([
            "codex", "gpt-5.5", "high", "codex", null, null, "control", "control"
        ])]
    );
}

/// With Claude held, a treatment's claim judgment planned on Claude falls
/// back to Codex as a run review's job does, its launch saying it moved
/// from the planned provider and why.
#[test]
fn a_treatments_claim_judgment_falls_back_from_its_planned_provider_with_a_reason() {
    let (_, started, finished) = judged(
        "a_treatments_claim_judgment_falls_back_from_its_planned_provider",
        100,
        Some(Provider::Claude),
    );
    assert_eq!(started.len(), 1, "{finished:?}");
    let job = &started[0];
    assert_eq!(
        (&job[0], &job[3], &job[4], &job[5], &job[6]),
        (
            &json!("codex"),
            &json!("claude"),
            &json!("claude"),
            &json!("usage_limit"),
            &json!("treatment")
        ),
        "{job}"
    );
}
