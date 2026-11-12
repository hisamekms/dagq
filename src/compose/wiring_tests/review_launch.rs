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

/// A successful receipt with unbacked deferral goes through the ordinary
/// evidence resume, before either the whole reviewer or selected agents.
#[test]
fn an_unanswered_acceptance_deferral_resumes_before_any_review_job() {
    use super::{agent::succeeded_at_the_head, git::FakeGit};
    use crate::domain::RunId;
    use std::path::Path;

    fn deferred(run: &RunId, worktree: &Path, git: &FakeGit) -> anyhow::Result<Value> {
        let mut receipt = succeeded(run, worktree, git)?;
        receipt["acceptance_items"][0]["status"] = json!("deferred");
        receipt["acceptance_items"][0]["evidence"] =
            json!("startup connection postponed without a question");
        Ok(receipt)
    }

    // The second turn supplies met. A review may start only after this
    // automatic resume, and the accepted head is the original commit.
    let wiring = Wiring::new(
        "an_unanswered_acceptance_deferral_resumes_before_any_review_job",
        Script {
            receipts: vec![deferred, succeeded_at_the_head],
            verdicts: vec![verdict("concern", "a person looks")],
        },
    );
    wiring.git.commit_files(
        &[
            ("dagq.toml", CONFIG),
            (
                ".dagq/agents/design/AGENT.md",
                "---\ndescription: design checks\n---\nCheck design.\n",
            ),
        ],
        &["change.txt"],
    );
    wiring.verifier.set_config(CONFIG);
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let detail = wiring.queue().show(task).unwrap();
    let parked = detail
        .events
        .iter()
        .position(|event| {
            event.kind.as_str() == "evidence_missing"
                && event.payload["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("acceptance item 1 deferred"))
        })
        .expect("deferral parks for the worker");
    let resumed = detail
        .events
        .iter()
        .enumerate()
        .skip(parked + 1)
        .find(|(_, event)| event.kind.as_str() == "resume_started")
        .map(|(index, _)| index)
        .expect("the existing automatic resume starts");
    assert!(
        detail.events[..=resumed]
            .iter()
            .all(|event| event.kind.as_str() != "review_job_started")
    );
    assert_eq!(payloads(&detail, "review_job_started").len(), 3);
    assert_eq!(wiring.agent.agent_jobs().len(), 2);
    assert_eq!(payloads(&detail, "review_finished").len(), 1);
    assert_eq!(payloads(&detail, "revise_requested").len(), 0);
    assert!(wiring.agent.turn_sessions().len() >= 2);
}

/// Re-review of an older receipt holds a review lease. Acceptance gaps
/// must go through validation's evidence park under that lease, before
/// any new review jobs, and resume the worker without a review revise.
#[test]
fn retrying_an_old_receipt_parks_under_the_review_lease_before_any_new_job() {
    use super::agent::succeeded_at_the_head;
    use crate::application::RunFiles;
    use crate::domain::{AskKind, RunStatus, run::RunHistory};
    use std::path::Path;

    let wiring = Wiring::new(
        "retrying_an_old_receipt_parks_under_the_review_lease_before_any_new_job",
        Script {
            receipts: vec![succeeded, succeeded_at_the_head],
            verdicts: vec![
                json!("no verdict here"),
                verdict("concern", "a person looks"),
            ],
        },
    );
    wiring.git.commit_files(
        &[
            ("dagq.toml", CONFIG),
            (
                ".dagq/agents/design/AGENT.md",
                "---\ndescription: design checks\n---\nCheck design.\n",
            ),
        ],
        &["change.txt"],
    );
    wiring.verifier.set_config(CONFIG);
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let before = queue.show(task).unwrap();
    let run = &before.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    let ask = before
        .asks
        .iter()
        .find(|ask| ask.kind == AskKind::ApproveLanding)
        .unwrap();
    assert!(ask.options.iter().any(|option| option == "retry_review"));
    // Model a receipt written before this gate existed. Its Git facts and
    // other evidence remain sound, so the new acceptance gate rejects it.
    let path = Path::new(run.receipt_path().unwrap());
    let mut receipt: Value =
        serde_json::from_str(&wiring.files.read_to_string(path).unwrap()).unwrap();
    receipt.as_object_mut().unwrap().remove("acceptance_items");
    wiring
        .files
        .write(path, receipt.to_string().as_bytes())
        .unwrap();
    let cursor = before.events.last().unwrap().id;
    queue.answer(ask.id, "retry_review").unwrap();
    wiring.supervise_once();

    let detail = queue.show(task).unwrap();
    let events: Vec<_> = detail
        .events
        .iter()
        .filter(|event| event.id > cursor)
        .collect();
    let at = |kind: &str| {
        events
            .iter()
            .position(|event| event.kind.as_str() == kind)
            .unwrap()
    };
    let lease = events
        .iter()
        .position(|event| {
            event.kind.as_str() == "lease_acquired" && event.payload["reason"] == "review"
        })
        .expect("the re-review leases the run before checking its receipt");
    let parked = at("evidence_missing");
    let resumed = at("resume_started");
    assert!(at("review_retry_queued") < lease && lease < parked && parked < resumed);
    assert!(
        events[..=resumed].iter().all(|event| {
            !matches!(event.kind.as_str(), "review_started" | "review_job_started")
        })
    );
    let validation = events
        .iter()
        .find(|event| event.kind.as_str() == "validation_finished")
        .unwrap();
    assert_eq!(validation.payload["status"], "needs_session");
    assert_eq!(validation.payload["code"], "evidence_missing");
    assert!(
        events[parked].payload["reason"]
            .as_str()
            .unwrap()
            .contains("acceptance_items missing")
    );
    assert_eq!(events[parked].payload["checks"], json!([]));
    assert!(at("validation_finished") < parked);
    assert!(payloads(&detail, "landing_decided").is_empty());
    assert!(payloads(&detail, "revise_requested").is_empty());
    let history = RunHistory::from_events(&detail.events);
    assert_eq!(history.resumes().counted, 1);
    assert_eq!(history.revise_attempts(), 0);
    // Only the rewritten met receipt starts the whole review and agents.
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind.as_str() == "review_job_started")
            .count(),
        3
    );
    assert!(resumed < at("review_job_started"));
    assert_eq!(detail.runs.len(), 1);
    assert_eq!(detail.runs[0].id(), run.id());
    assert!(queue.run_lease(run.id()).unwrap().is_none());
}

/// A demonstrated shared baseline returns a turn before any ordinary job.
/// The worker then explicitly fails, keeping this test about that one return.
#[test]
fn acceptance_precheck_returns_shared_baseline_without_starting_any_review_job() {
    let wiring = Wiring::new(
        "acceptance_precheck_returns_shared_baseline_without_starting_any_review_job",
        Script {
            receipts: vec![succeeded, super::worker_trial::failed],
            verdicts: vec![json!({"findings": [{"item": 1, "kind": "shared_baseline",
                "evidence": "diff test compares wrappers that both call the new implementation"}]})],
        },
    );
    let config =
        format!("{CONFIG}\n[review.subagents.\"builtin:acceptance-precheck\"]\nenabled = true\n");
    wiring.git.commit_files(
        &[
            ("dagq.toml", &config),
            (
                ".dagq/agents/design/AGENT.md",
                "---\ndescription: design\n---\nCheck design.\n",
            ),
        ],
        &["change.txt"],
    );
    wiring.verifier.set_config(&config);
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let detail = wiring.queue().show(task).unwrap();
    let jobs = payloads(&detail, "review_job_started");
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0]["agent"], "builtin:acceptance-precheck");
    assert!(payloads(&detail, "review_started").is_empty());
    assert!(payloads(&detail, "review_finished").is_empty());
    assert!(wiring.agent.agent_jobs().is_empty());
    let requests = payloads(&detail, "revise_requested");
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0]["acceptance_precheck"], true);
    assert!(
        requests[0]["reasons"][0]
            .as_str()
            .unwrap()
            .contains("[acceptance:1]")
    );
    assert_eq!(wiring.agent.turn_sessions().len(), 2);
}

/// No demonstrated gap permits the unchanged whole review and both selected
/// subagents to start. A concern deliberately stops the run before landing.
#[test]
fn acceptance_precheck_clear_starts_the_whole_review_and_subagent_jobs() {
    let wiring = Wiring::new(
        "acceptance_precheck_clear_starts_the_whole_review_and_subagent_jobs",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![
                json!({"findings": []}),
                verdict("concern", "a person looks"),
            ],
        },
    );
    let config =
        format!("{CONFIG}\n[review.subagents.\"builtin:acceptance-precheck\"]\nenabled = true\n");
    wiring.git.commit_files(
        &[
            ("dagq.toml", &config),
            (
                ".dagq/agents/design/AGENT.md",
                "---\ndescription: design\n---\nCheck design.\n",
            ),
        ],
        &["change.txt"],
    );
    wiring.verifier.set_config(&config);
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let detail = wiring.queue().show(task).unwrap();
    let jobs: Vec<_> = payloads(&detail, "review_job_started")
        .iter()
        .map(|p| p["agent"].clone())
        .collect();
    assert_eq!(
        jobs,
        [
            json!("builtin:acceptance-precheck"),
            Value::Null,
            json!("design"),
            json!("builtin:security")
        ]
    );
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert_eq!(payloads(&detail, "review_finished").len(), 1);
    assert!(payloads(&detail, "revise_requested").is_empty());
    assert_eq!(wiring.agent.agent_jobs().len(), 2);
}

/// Each group exercises one supervisor pipeline; the pure launch test covers
/// their matrix. Separate cases keep each wiring test within five seconds.
fn precheck_trial_group(percent: u8) {
    let config = format!(
        "{}\n[review.subagents.\"builtin:acceptance-precheck\"]\nenabled = true\n",
        pair_config(percent)
    );
    let wiring = Wiring::new(
        &format!("acceptance_precheck_launch_is_identical_in_both_provider_trial_groups_{percent}"),
        Script {
            receipts: vec![succeeded],
            verdicts: vec![
                json!({"findings": []}),
                verdict("concern", "a person looks"),
            ],
        },
    )
    .with_codex();
    wiring.git.commit_files(
        &[
            ("dagq.toml", &config),
            (
                ".dagq/agents/design-layers/AGENT.md",
                "---\ndescription: design\n---\nCheck design.\n",
            ),
            (
                ".dagq/agents/tests/AGENT.md",
                "---\ndescription: tests\n---\nCheck tests.\n",
            ),
        ],
        &["change.txt"],
    );
    wiring.verifier.set_config(&config);
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let detail = wiring.queue().show(task).unwrap();
    let jobs = payloads(&detail, "review_job_started");
    let precheck = jobs[0];
    assert_eq!(precheck["agent"], "builtin:acceptance-precheck");
    assert_eq!(precheck["launch"]["provider"], "claude");
    assert_eq!(precheck["launch"]["model"], "claude-haiku-5-5");
    assert_eq!(precheck["launch"]["effort"], "low");
    assert_eq!(jobs[1]["agent"], Value::Null);
    assert_eq!(
        jobs[1]["launch"]["provider"],
        if percent == 0 { "codex" } else { "claude" }
    );
}

#[test]
fn acceptance_precheck_control_group_uses_dedicated_settings() {
    precheck_trial_group(0);
}

#[test]
fn acceptance_precheck_treatment_group_uses_dedicated_settings() {
    precheck_trial_group(100);
}

#[test]
fn acceptance_precheck_failure_proceeds_to_every_normal_review_job() {
    let wiring = Wiring::new(
        "acceptance_precheck_failure_proceeds_to_every_normal_review_job",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![
                json!({"wiring_start_failed": true}),
                verdict("concern", "a person looks"),
            ],
        },
    );
    let config =
        format!("{CONFIG}\n[review.subagents.\"builtin:acceptance-precheck\"]\nenabled = true\n");
    wiring.git.commit_files(
        &[
            ("dagq.toml", &config),
            (
                ".dagq/agents/design/AGENT.md",
                "---\ndescription: design\n---\nCheck design.\n",
            ),
        ],
        &["change.txt"],
    );
    wiring.verifier.set_config(&config);
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let detail = wiring.queue().show(task).unwrap();
    let jobs: Vec<_> = payloads(&detail, "review_job_started")
        .iter()
        .map(|p| p["agent"].clone())
        .collect();
    assert_eq!(
        jobs,
        [Value::Null, json!("design"), json!("builtin:security")]
    );
    let failures = payloads(&detail, "review_job_finished");
    assert_eq!(failures[0]["agent"], "builtin:acceptance-precheck");
    assert_eq!(failures[0]["outcome"], "start_failed");
    assert_eq!(failures[0]["runs_again"], false);
    assert!(payloads(&detail, "revise_requested").is_empty());
}

#[test]
fn acceptance_precheck_return_limit_preserves_the_ordinary_review_budget() {
    let missing = json!({"findings": [{"item": 1, "kind": "missing_item", "evidence": "no test evidence for item 1"}]});
    let wiring = Wiring::new(
        "acceptance_precheck_return_limit_preserves_the_ordinary_review_budget",
        Script {
            receipts: vec![succeeded, succeeded, succeeded],
            verdicts: vec![
                missing.clone(),
                missing.clone(),
                missing,
                verdict("concern", "a person looks"),
            ],
        },
    );
    let config =
        format!("{CONFIG}\n[review.subagents.\"builtin:acceptance-precheck\"]\nenabled = true\n");
    wiring.git.commit_files(
        &[
            ("dagq.toml", &config),
            (
                ".dagq/agents/design/AGENT.md",
                "---\ndescription: design\n---\nCheck design.\n",
            ),
        ],
        &["change.txt"],
    );
    wiring.verifier.set_config(&config);
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let detail = wiring.queue().show(task).unwrap();
    let requests = payloads(&detail, "revise_requested");
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(requests.iter().all(|r| r["acceptance_precheck"] == true));
    assert_eq!(
        crate::domain::run::RunHistory::from_events(&detail.events).revise_attempts(),
        0
    );
    let jobs: Vec<_> = payloads(&detail, "review_job_started")
        .iter()
        .map(|p| p["agent"].clone())
        .collect();
    assert_eq!(
        jobs,
        [
            json!("builtin:acceptance-precheck"),
            json!("builtin:acceptance-precheck"),
            json!("builtin:acceptance-precheck"),
            Value::Null,
            json!("design"),
            json!("builtin:security")
        ]
    );
    let finished = payloads(&detail, "review_job_finished");
    assert_eq!(finished[2]["limit_reached"], true);
    assert_eq!(payloads(&detail, "review_started").len(), 1);
}
