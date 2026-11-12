//! Coverage-only decisions for the optional acceptance precheck. Failures
//! (including unreadable replies and timeouts) never stand in for a review.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::domain::{ReviewVerdict, review_builtins::AcceptancePrecheck};

/// At most two coverage returns per run, including requests recovered after
/// a restart. They consume no ordinary review revise. The request is counted
/// before delivery, so a crash or failed delivery cannot replenish this budget.
pub const RETURN_LIMIT: usize = 2;
pub const REQUEST_FIELD: &str = "acceptance_precheck";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Missing,
    Clear,
    Failed,
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    Return { number: usize },
    Review,
}

/// Only demonstrated coverage gaps return the worker; after the dedicated
/// budget, retain observations as review material and run every normal job.
pub fn next(outcome: Outcome, returns: usize) -> Next {
    match outcome {
        Outcome::Missing if returns < RETURN_LIMIT => Next::Return {
            number: returns + 1,
        },
        _ => Next::Review,
    }
}

/// Dedicated settings ignore the supplied run pair, so both experimental
/// groups receive the same job. Review role settings are never inherited.
pub fn launch(
    settings: &AcceptancePrecheck,
    _pair: &crate::domain::provider_trial::RunPair,
) -> Result<crate::domain::actor_model::ActorLaunch, String> {
    settings.launch()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    findings: Vec<Finding>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Finding {
    item: usize,
    kind: Kind,
    evidence: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    SharedBaseline,
    Unconnected,
    MissingItem,
}

/// A narrow reply grammar cannot introduce code/docs/security verdicts or a
/// concern. Empty findings mean no demonstrated gap, never a review pass.
/// Each finding must name a positive item and diff/receipt evidence.
pub fn parse(reply: Value) -> Result<ReviewVerdict, String> {
    let reply: Reply = serde_json::from_value(reply).map_err(|e| e.to_string())?;
    if reply
        .findings
        .iter()
        .any(|f| f.item == 0 || f.evidence.trim().is_empty())
    {
        return Err("coverage findings need an item and evidence".to_owned());
    }
    let reasons: Vec<_> = reply
        .findings
        .iter()
        .map(|f| {
            json!({"text": format!("[acceptance:{}] {:?}: {}", f.item, f.kind, f.evidence),
               "codes": ["acceptance_unmet"]})
        })
        .collect();
    ReviewVerdict::parse(
        &json!({
            "verdict": if reasons.is_empty() { "pass" } else { "revise" },
            "reasons": reasons, "summary": "acceptance coverage precheck",
        })
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Provider;

    #[test]
    fn coverage_outcomes_and_the_run_wide_budget_choose_the_next_stage() {
        for (outcome, returns, expected) in [
            (Outcome::Missing, 0, Next::Return { number: 1 }),
            (Outcome::Missing, 1, Next::Return { number: 2 }),
            (Outcome::Missing, 2, Next::Review),
            (Outcome::Missing, 99, Next::Review),
            (Outcome::Clear, 0, Next::Review),
            (Outcome::Failed, 0, Next::Review),
            (Outcome::TimedOut, 0, Next::Review),
        ] {
            assert_eq!(next(outcome, returns), expected);
        }
    }

    #[test]
    fn only_the_three_coverage_kinds_are_readable() {
        for kind in ["shared_baseline", "unconnected", "missing_item"] {
            let verdict = parse(json!({"findings": [{"item": 1, "kind": kind,
                "evidence": "diff test compares two calls through the new implementation"}]}))
            .unwrap();
            assert_eq!(verdict.verdict, crate::domain::ReviewDecision::Revise);
            assert!(verdict.reasons[0].contains("[acceptance:1]"));
        }
        assert_eq!(
            parse(json!({"findings": []})).unwrap().verdict,
            crate::domain::ReviewDecision::Pass
        );
        for invalid in [
            json!({"findings": [{"item": 1, "kind": "code_defect", "evidence": "bug"}]}),
            json!({"findings": [{"item": 1, "kind": "docs_drift", "evidence": "stale"}]}),
            json!({"findings": [{"item": 1, "kind": "security", "evidence": "leak"}]}),
            json!({"findings": [{"item": 0, "kind": "missing_item", "evidence": "x"}]}),
            json!({"findings": [{"item": 1, "kind": "missing_item", "evidence": " "}]}),
            json!({"verdict": "pass"}),
        ] {
            assert!(parse(invalid).is_err());
        }
    }

    #[test]
    fn dedicated_settings_ignore_the_ab_group_and_review_role() {
        // Both groups call this same function; explicit expected values are
        // independent of the role/pair planning implementation.
        use crate::domain::{provider_trial::RunPair, worker_model::TrialGroup};
        for (group, review) in [
            (TrialGroup::Control, None),
            (TrialGroup::Treatment, Some(Provider::Claude)),
        ] {
            let pair = RunPair {
                group: Some(group),
                review,
                ..Default::default()
            };
            let got = launch(&AcceptancePrecheck::default(), &pair).unwrap();
            assert_eq!(got.provider, Provider::Claude);
            assert_eq!(got.model.as_deref(), Some("claude-haiku-5-5"));
            assert_eq!(got.effort.as_deref(), Some("low"));
            let got = launch(
                &AcceptancePrecheck {
                    provider: Some(Provider::Codex),
                    model: Some("gpt-5.5".into()),
                    effort: Some("medium".into()),
                    ..Default::default()
                },
                &pair,
            )
            .unwrap();
            assert_eq!(got.provider, Provider::Codex);
            assert_eq!(got.model.as_deref(), Some("gpt-5.5"));
            assert_eq!(got.effort.as_deref(), Some("medium"));
        }
    }
}
