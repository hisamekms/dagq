//! The worker's trial (ADR-t3957-1) wired from the checkout's
//! `[worker.trial]` to the claim: the supervisor reads it once per pass,
//! the claim draws the task's group and opens the group's candidate, and
//! `run_claimed` records the group, the model, the effort and the trial's
//! name in its attributes, which `kpi --by group` and `--by trial` read.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::{
    agent::{Script, succeeded, verdict},
    harness::Wiring,
};
use crate::{
    application::TaskReads,
    domain::{
        RunEvent,
        kpi::{Axis, KpiConfig, KpiInput, KpiQuery},
    },
};

/// The trial on for every task with a prediction, all of them drawn into
/// the treatment, whose candidate is Opus at `high`.
const CONFIG: &str = "[worker.trial]\nenabled = true\nname = \"wired\"\nlimited = false\n\
treatment_percent = 100\ntreatment_model = \"claude-opus-5-5\"\ntreatment_effort = \"high\"\n";

/// The plan review's prediction of `task`, as the queue records it.
pub(super) fn predict(wiring: &Wiring, task: crate::domain::TaskId) {
    super::harness::connect(&wiring.db)
        .execute(
            "INSERT INTO run_events(task_id,kind,payload) VALUES (?1,'task_weight_predicted',?2)",
            rusqlite::params![
                task,
                json!({"prediction": {"nature": "implementation", "expected_output_tokens": 1000}})
                    .to_string()
            ],
        )
        .expect("record the prediction");
}

/// The receipt of a worker that could not finish the task, on its
/// worktree's head: its run ends, which `kpi` counts.
pub(super) fn failed(
    run: &crate::domain::RunId,
    worktree: &std::path::Path,
    git: &super::git::FakeGit,
) -> anyhow::Result<Value> {
    let head = crate::application::Repository::head(git, worktree)?;
    Ok(json!({
        "run_id": run,
        "result": "failed",
        "commit": head,
        "tests": {"status": "not_applicable", "evidence_or_reason": "nothing changed"},
        "e2e": {"status": "not_applicable", "evidence_or_reason": "the runtime runs it"},
        "subagent_review": {"status": "not_applicable", "evidence_or_reason": "a wiring test"},
        "summary": "could not",
    }))
}

/// A task with a prediction under the trial is claimed in the treatment
/// with its candidate, and `run_claimed` records the group, model, effort
/// and trial, in its payload and its attributes; once its run ended,
/// `kpi --by group --by trial` reads the run in the treatment's and the
/// trial's strata.
#[test]
fn a_claim_under_the_trial_records_its_group_and_candidate() {
    let wiring = Wiring::new(
        "a_claim_under_the_trial_records_its_group_and_candidate",
        Script {
            receipts: vec![failed],
            verdicts: vec![verdict("concern", "a person looks")],
        },
    );
    wiring.verifier.set_config(CONFIG);
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
    let claim = claims[0];
    assert_eq!(
        (
            &claim["group"],
            &claim["model"],
            &claim["effort"],
            &claim["trial"]
        ),
        (
            &json!("treatment"),
            &json!("claude-opus-5-5"),
            &json!("high"),
            &json!("wired")
        ),
        "{claim}"
    );
    // The claim's open set of attributes, which the ledger and `kpi` read,
    // holds the group, the model, the effort and the trial's name.
    let attributes = &claim[crate::domain::claim_facts::ATTRIBUTES];
    assert_eq!(
        (
            &attributes["trial_group"],
            &attributes["claim_model"],
            &attributes["effort"],
            &attributes["trial"]
        ),
        (
            &json!("treatment"),
            &json!("claude-opus-5-5"),
            &json!("high"),
            &json!("wired")
        ),
        "{claim}"
    );

    // `kpi --by group --by trial`, of the claim's attributes (no ledger
    // row).
    let events: Vec<RunEvent> = queue.all_events().unwrap();
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
            events: &events,
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
            by: vec![Axis::GROUP, "trial".parse().unwrap()],
            ..KpiQuery::default()
        },
    )
    .unwrap();
    let kinds: Vec<&str> = detail
        .events
        .iter()
        .map(|event| event.kind.as_str())
        .collect();
    let strata: Vec<&String> = kpi.periods[0]
        .window
        .kpis
        .values()
        .flat_map(|strata| strata.keys())
        .collect();
    assert!(
        strata.iter().any(|stratum| *stratum == "group=treatment"),
        "{strata:?} {kinds:?}"
    );
    assert!(
        strata.iter().any(|stratum| *stratum == "trial=wired"),
        "{strata:?}"
    );
    assert!(
        !strata
            .iter()
            .any(|stratum| *stratum == "group=control" || *stratum == "trial=unknown"),
        "{strata:?}"
    );
}

/// With no `[worker.trial]`, the claim is outside the trial as before: the
/// default session and no group or trial.
#[test]
fn a_claim_without_the_trial_records_the_default_outside_any_group() {
    let wiring = Wiring::new(
        "a_claim_without_the_trial_records_the_default_outside_any_group",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![verdict("concern", "a person looks")],
        },
    );
    let task = wiring.ready_task(&["true"]);
    predict(&wiring, task);
    wiring.supervise_once();

    let detail = wiring.queue().show(task).unwrap();
    let claim = &detail
        .events
        .iter()
        .find(|event| event.kind.as_str() == "run_claimed")
        .expect("claimed")
        .payload;
    assert_eq!(
        (&claim["group"], &claim["model"], &claim["effort"]),
        (&Value::Null, &json!("claude-opus-5-5"), &json!("medium")),
        "{claim}"
    );
    assert!(claim.get("trial").is_none(), "{claim}");
}
