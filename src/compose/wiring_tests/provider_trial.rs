//! The trial of the provider pair (ADR-t4005-1) wired from the checkout's
//! `[worker.provider_trial]` to the claim: the supervisor reads it once per
//! pass with `[worker.trial]`, the claim draws the task's group, a
//! treatment's worker is claimed on Codex outside the model and effort
//! trial, and `run_claimed` records the group, the trial's name and the
//! assigned worker and review in its attributes, which `kpi --by
//! pair_group` reads and the review's launch reads back
//! ([`RunPair::of_run`]).

use std::collections::HashMap;

use serde_json::{Value, json};

use super::{
    agent::{Script, verdict},
    harness::Wiring,
    worker_trial::{failed, predict},
};
use crate::{
    application::TaskReads,
    domain::{
        Provider, RunEvent,
        claim_facts::ATTRIBUTES,
        kpi::{KpiConfig, KpiInput, KpiQuery},
        provider_trial::RunPair,
        worker_model::TrialGroup,
    },
};

/// The model trial on for every task with a prediction and drawing all of
/// them into its treatment, and the provider trial at `percent`.
fn config(percent: u8) -> String {
    format!(
        "[worker.trial]\nenabled = true\nlimited = false\ntreatment_percent = 100\n\
         [worker.provider_trial]\nenabled = true\nname = \"pairs\"\ntreatment_percent = {percent}\n"
    )
}

/// One task claimed under `config` by a supervisor that runs Claude and
/// Codex workers: its `run_claimed` and the queue's events.
fn claim(name: &str, config: &str) -> (Value, Vec<RunEvent>) {
    let wiring = Wiring::new(
        name,
        Script {
            receipts: vec![failed],
            verdicts: vec![verdict("concern", "a person looks")],
        },
    )
    .with_codex();
    wiring.verifier.set_config(config);
    let task = wiring.ready_task(&["true"]);
    predict(&wiring, task);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let claims: Vec<&Value> = detail
        .events
        .iter()
        .filter(|event| event.kind.as_str() == "run_claimed")
        .map(|event| &event.payload)
        .collect();
    assert_eq!(claims.len(), 1, "{claims:?}");
    (claims[0].clone(), queue.all_events().unwrap())
}

/// The strata of `kpi --by pair_group` over `events`.
fn pair_strata(events: &[RunEvent]) -> Vec<String> {
    // A minute after the queue's last event: the test reads no clock.
    let now = events
        .iter()
        .filter_map(|event| crate::domain::stats::timestamp_millis(&event.created_at))
        .max()
        .expect("the queue's events")
        / 1000
        + 60;
    let empty = HashMap::new();
    let config = KpiConfig::default();
    let kpi = crate::domain::kpi::kpi(
        &KpiInput {
            events,
            goals: &HashMap::new(),
            changes: &HashMap::new(),
            areas: None,
            heartbeats: &empty,
            draft_origins: &HashMap::new(),
            now,
            utc_offset_secs: 0,
            cores: None,
            rust_toolchain: false,
            config: &config,
            host: None,
            ledger: None,
            rows: &[],
            post_landing: None,
            executions: None,
        },
        &KpiQuery {
            last: 1,
            by: vec!["pair_group".parse().unwrap()],
            ..KpiQuery::default()
        },
    )
    .unwrap();
    kpi.periods[0]
        .window
        .kpis
        .values()
        .flat_map(|strata| strata.keys().cloned())
        .collect()
}

/// A task drawn into the treatment is claimed on Codex outside the model
/// and effort trial, and `run_claimed` records the pair's group, name and
/// the assigned worker and review, in its payload and its attributes; the
/// review reads the run's pair back and `kpi --by pair_group` reads the
/// ended run in the treatment's stratum.
#[test]
fn a_treatment_task_is_claimed_on_codex_and_records_its_pair() {
    let (claim, events) = claim(
        "a_treatment_task_is_claimed_on_codex_and_records_its_pair",
        &config(100),
    );
    assert_eq!(
        (
            &claim["requested_provider"],
            &claim["provider"],
            &claim["group"],
            &claim["pair_group"],
            &claim["pair_trial"],
            &claim["pair_worker"],
            &claim["pair_review"],
        ),
        (
            &json!("codex"),
            &json!("codex"),
            &Value::Null,
            &json!("treatment"),
            &json!("pairs"),
            &json!("codex"),
            &json!("claude"),
        ),
        "{claim}"
    );
    // Outside the model trial: no name of it is recorded.
    assert!(claim.get("trial").is_none(), "{claim}");
    let attributes = &claim[ATTRIBUTES];
    assert_eq!(
        (
            &attributes["requested_provider"],
            &attributes["claim_provider"],
            &attributes["pair_group"],
            &attributes["pair_trial"],
            &attributes["pair_worker"],
            &attributes["pair_review"],
        ),
        (
            &json!("codex"),
            &json!("codex"),
            &json!("treatment"),
            &json!("pairs"),
            &json!("codex"),
            &json!("claude"),
        ),
        "{claim}"
    );
    assert!(attributes.get("trial_group").is_none(), "{claim}");
    let run: Vec<RunEvent> = events
        .iter()
        .filter(|event| event.kind.as_str() == "run_claimed")
        .cloned()
        .collect();
    let pair = RunPair::of_run(&run);
    assert_eq!(
        (pair.group, pair.review),
        (Some(TrialGroup::Treatment), Some(Provider::Claude))
    );
    let strata = pair_strata(&events);
    assert!(
        strata
            .iter()
            .any(|stratum| stratum == "pair_group=treatment"),
        "{strata:?}"
    );
}

/// A task drawn into the control is claimed on its own provider, Claude,
/// where the model and effort trial still draws it, and records the
/// control's pair with the review as configured.
#[test]
fn a_control_task_is_claimed_on_claude_inside_the_model_trial() {
    let (claim, _) = claim(
        "a_control_task_is_claimed_on_claude_inside_the_model_trial",
        &config(0),
    );
    assert_eq!(
        (
            &claim["requested_provider"],
            &claim["provider"],
            &claim["group"],
            &claim["pair_group"],
            &claim["pair_worker"],
        ),
        (
            &json!("claude"),
            &json!("claude"),
            &json!("treatment"),
            &json!("control"),
            &json!("claude"),
        ),
        "{claim}"
    );
    assert!(claim.get("pair_review").is_none(), "{claim}");
    assert_eq!(claim[ATTRIBUTES]["pair_group"], json!("control"), "{claim}");
}

/// The events of `kind` of `task`, as the queue shows them.
fn task_payloads(wiring: &Wiring, task: crate::domain::TaskId, kind: &str) -> Vec<Value> {
    wiring
        .queue()
        .show(task)
        .unwrap()
        .events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| event.payload.clone())
        .collect()
}

/// With Claude held and the fallback off, the deferrals before the claim
/// judge a treatment task by the worker its pair requests, as the claim
/// does: it is claimed on Codex, which can be used, while a Spike outside
/// the trial waits for Claude.
#[test]
fn a_treatment_task_is_claimed_on_codex_while_claude_is_held() {
    let wiring = Wiring::new(
        "a_treatment_task_is_claimed_on_codex_while_claude_is_held",
        Script {
            receipts: vec![failed],
            verdicts: vec![verdict("concern", "a person looks")],
        },
    )
    .with_codex()
    .without_worker_fallback();
    wiring.verifier.set_config(&config(100));
    let treatment = wiring.ready_task(&["true"]);
    let spike = wiring.ready_task(&["true"]);
    super::harness::connect(&wiring.db)
        .execute(
            "UPDATE tasks SET execution_class='spike' WHERE id=?1",
            rusqlite::params![spike],
        )
        .expect("make the task a Spike");
    wiring.hold(Provider::Claude);
    wiring.supervise_once();

    let claims = task_payloads(&wiring, treatment, "run_claimed");
    assert_eq!(claims.len(), 1, "{claims:?}");
    assert_eq!(
        (
            &claims[0]["requested_provider"],
            &claims[0]["provider"],
            &claims[0]["pair_group"]
        ),
        (&json!("codex"), &json!("codex"), &json!("treatment")),
        "{}",
        claims[0]
    );
    assert!(
        task_payloads(&wiring, treatment, "claim_deferred").is_empty(),
        "the treatment waited for Claude"
    );
    assert!(task_payloads(&wiring, spike, "run_claimed").is_empty());
    let deferred = task_payloads(&wiring, spike, "claim_deferred");
    assert!(
        deferred
            .iter()
            .any(|payload| payload["reason"] == "provider_fallback_off"
                && payload["provider"] == "claude"),
        "{deferred:?}"
    );
}

/// With Codex unusable and the fallback off, a treatment task is deferred
/// for Codex, the worker its pair requests, and not claimed.
#[test]
fn a_treatment_task_waits_for_codex_without_the_fallback() {
    let wiring = Wiring::new(
        "a_treatment_task_waits_for_codex_without_the_fallback",
        Script {
            receipts: vec![failed],
            verdicts: vec![verdict("concern", "a person looks")],
        },
    )
    .without_worker_fallback();
    wiring.verifier.set_config(&config(100));
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();

    assert!(task_payloads(&wiring, task, "run_claimed").is_empty());
    let deferred = task_payloads(&wiring, task, "claim_deferred");
    assert!(
        deferred.iter().any(|payload| payload["provider"] == "codex"
            && payload["reason"] == "provider_fallback_off"),
        "{deferred:?}"
    );
}
