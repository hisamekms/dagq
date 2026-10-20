//! The dev a run that changes an agent's definition runs before it lands
//! (ADR-t1728-1 decision 10): which agents of a run it runs for, and where
//! the run goes from the rounds' ends. Everything here is without side
//! effects: the supervisor reads the diff, the trees and the eval's events
//! and acts.
//!
//! - Which agents: those whose `.dagq/agents/<name>/AGENT.md` the run's
//!   diff changes ([`changed_agents`]); one whose definition the run's
//!   head no longer has, or whose `evals/dev.json` the landing branch's
//!   commit does not have yet, is left out with why ([`targets`]).
//! - Where the run goes ([`judge`]): it waits while a round of it waits or
//!   runs; a round refused, ended incomplete, ended with runs without a
//!   judgment, or waiting for a provider the supervisor does not have, is
//!   for a person; a complete round under the threshold sends the run back
//!   with its failed cases and the agent's reasons; every round passed,
//!   the run goes on to land.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::record::Round;
use super::round::ProviderWait;
use crate::domain::DomainError;
use crate::domain::review_subagents::{DEFINITION_DIR, DEFINITION_FILE, valid_agent_name};

/// The agents whose definition one of `changed` (repository-relative
/// paths) is, each once, in name order.
pub fn changed_agents<'a>(changed: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let prefix = format!("{DEFINITION_DIR}/");
    let suffix = format!("/{DEFINITION_FILE}");
    changed
        .into_iter()
        .filter_map(|path| path.strip_prefix(&prefix)?.strip_suffix(&suffix))
        .filter(|name| valid_agent_name(name))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

// Why an agent whose definition a run changes runs no dev before the run
// lands: the run removes its definition (`definition_removed`), or the
// landing branch's commit has no `evals/dev.json` of it yet
// (`no_dev_cases`, an agent with no cases yet).
string_enum!(SkipReason {
    DefinitionRemoved => "definition_removed",
    NoDevCases => "no_dev_cases",
});

/// The agents a run's dev runs for, and those left out with why.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Targets {
    pub agents: Vec<String>,
    pub skipped: Vec<(String, SkipReason)>,
}

/// Of `changed` ([`changed_agents`]), those the run's head has a
/// definition of (`at_head`) and the landing branch's commit has dev cases
/// of (`with_dev_cases`) run; the others are left out with why.
pub fn targets(
    changed: Vec<String>,
    at_head: impl Fn(&str) -> bool,
    with_dev_cases: impl Fn(&str) -> bool,
) -> Targets {
    let mut targets = Targets::default();
    for agent in changed {
        if !at_head(&agent) {
            targets.skipped.push((agent, SkipReason::DefinitionRemoved));
        } else if !with_dev_cases(&agent) {
            targets.skipped.push((agent, SkipReason::NoDevCases));
        } else {
            targets.agents.push(agent);
        }
    }
    targets
}

// Why a run waits before it lands for its rounds: its round runs
// (`agent_eval_running`), its round waits for its turn while another
// round runs or goes first (`agent_eval_queued`), or its round waits for
// the review's provider while it is held (`agent_eval_provider_held`).
string_enum!(WaitReason {
    Running => "agent_eval_running",
    Queued => "agent_eval_queued",
    ProviderHeld => "agent_eval_provider_held",
});

/// Where a run goes from its rounds before it lands.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Every round passed: it goes on to land.
    Pass,
    /// A round of it waits or runs: it waits in its slot.
    Wait(WaitReason),
    /// A complete round fell under the threshold: back to the session
    /// with these reasons, as a review's revise.
    SendBack(Vec<String>),
    /// A round could not tell (refused, incomplete, runs without a
    /// judgment, no provider to run on): a person decides, told why.
    Ask(String),
}

/// What became of one round, for [`judge`].
#[derive(Debug, Clone, PartialEq)]
enum End {
    Waiting(WaitReason),
    Passed,
    Below(Vec<String>),
    Unavailable(String),
}

fn end(agent: &str, round: &Round) -> End {
    let id = round.id;
    if let Some(refused) = &round.refused {
        return End::Unavailable(format!(
            "the agent eval {id} of {agent} was not started: {}: {}",
            refused["reason"].as_str().unwrap_or("?"),
            refused["detail"].as_str().unwrap_or("")
        ));
    }
    if let Some(finished) = &round.finished {
        if finished["passed"] == true {
            return End::Passed;
        }
        if finished["outcome"] != "complete" {
            return End::Unavailable(format!(
                "the agent eval {id} of {agent} ended incomplete ({}), so whether the definition reaches the threshold is not known",
                finished["incomplete_reason"].as_str().unwrap_or("?")
            ));
        }
        let below = finished["below"].as_array().cloned().unwrap_or_default();
        if below.is_empty() {
            return End::Unavailable(format!(
                "the agent eval {id} of {agent} had {} run(s) without a judgment",
                finished["scores"]["errors"]
            ));
        }
        return End::Below(below_reasons(agent, id, finished, &below));
    }
    if round.started.is_some() {
        return End::Waiting(WaitReason::Running);
    }
    match round.waiting.as_deref().and_then(|wait| wait.parse().ok()) {
        Some(ProviderWait::Held) => End::Waiting(WaitReason::ProviderHeld),
        Some(wait @ (ProviderWait::NoClaude | ProviderWait::NoAgent)) => End::Unavailable(format!(
            "the agent eval {id} of {agent} cannot start: the review's provider cannot be used ({})",
            wait.as_str()
        )),
        None => End::Waiting(WaitReason::Queued),
    }
}

/// The reasons a round under the threshold sends the run back with: what
/// fell under it, then each failed case's id with the agent's reasons.
fn below_reasons(agent: &str, id: i64, finished: &Value, below: &[Value]) -> Vec<String> {
    let values: Vec<String> = below
        .iter()
        .map(|item| {
            let value = item["value"]
                .as_f64()
                .map_or_else(|| "none".to_owned(), |value| format!("{value:.2}"));
            format!("{} {value}", item["metric"].as_str().unwrap_or("?"))
        })
        .collect();
    let mut reasons = vec![format!(
        "agent eval {id} of {agent}: the definition the run changed falls under the threshold {} on the landing branch's dev cases ({}); the run's own cases are not used",
        finished["threshold"],
        values.join(", ")
    )];
    for case in finished["failed_cases"].as_array().into_iter().flatten() {
        let said: Vec<String> = case["reasons"]
            .as_array()
            .into_iter()
            .flatten()
            .map(said)
            .filter(|text| !text.is_empty())
            .collect();
        reasons.push(format!(
            "agent eval {id} of {agent}: case {} failed ({} of {} run(s) right){}",
            case["id"].as_str().unwrap_or("?"),
            case["correct"],
            case["runs"],
            if said.is_empty() {
                String::new()
            } else {
                format!("; the agent said: {}", said.join(" | "))
            }
        ));
    }
    reasons
}

/// An agent's reasons as one line: its strings joined, or the JSON of
/// anything else.
fn said(reasons: &Value) -> String {
    match reasons {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(said).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    }
}

/// Where a run goes from its `rounds` (each with its agent) before it
/// lands: it waits while any of them waits or runs (the first such one's
/// reason), else a person decides when any could not tell (with every
/// reason, those under the threshold too), else it is sent back when any
/// fell under the threshold, else it lands.
pub fn judge<'a>(rounds: impl IntoIterator<Item = (&'a str, &'a Round)>) -> Verdict {
    let ends: Vec<End> = rounds
        .into_iter()
        .map(|(agent, round)| end(agent, round))
        .collect();
    if let Some(wait) = ends.iter().find_map(|end| match end {
        End::Waiting(wait) => Some(*wait),
        _ => None,
    }) {
        return Verdict::Wait(wait);
    }
    let unavailable: Vec<&str> = ends
        .iter()
        .filter_map(|end| match end {
            End::Unavailable(why) => Some(why.as_str()),
            _ => None,
        })
        .collect();
    let below: Vec<String> = ends
        .iter()
        .filter_map(|end| match end {
            End::Below(reasons) => Some(reasons.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    if !unavailable.is_empty() {
        let mut why = unavailable.join("; ");
        if !below.is_empty() {
            why.push_str("; and ");
            why.push_str(&below.join("; "));
        }
        return Verdict::Ask(why);
    }
    if below.is_empty() {
        Verdict::Pass
    } else {
        Verdict::SendBack(below)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventKind;
    use crate::domain::agent_eval::Split;
    use crate::domain::agent_eval::record::{Request, rounds};
    use crate::domain::event_kind::RecordedKind;
    use serde_json::json;

    #[test]
    fn the_agents_are_those_whose_definition_the_run_changed() {
        let changed = [
            ".dagq/agents/adr-rules/AGENT.md",
            ".dagq/agents/adr-rules/evals/dev.json",
            ".dagq/agents/migration-rules/evals/dev.json",
            ".dagq/agents/Bad_Name/AGENT.md",
            ".dagq/agents/a/b/AGENT.md",
            "src/lib.rs",
            ".dagq/agents/doc-rules/AGENT.md",
            ".dagq/agents/adr-rules/AGENT.md",
        ];
        assert_eq!(
            changed_agents(changed),
            vec!["adr-rules".to_owned(), "doc-rules".to_owned()]
        );
    }

    /// A run that changes no definition runs no dev; one of an agent with
    /// no dev cases on the landing branch, or whose definition it removes,
    /// is left out with why.
    #[test]
    fn an_agent_without_dev_cases_or_a_definition_is_left_out_with_why() {
        assert_eq!(targets(Vec::new(), |_| true, |_| true), Targets::default());
        let changed = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let picked = targets(changed, |agent| agent != "c", |agent| agent != "b");
        assert_eq!(picked.agents, vec!["a".to_owned()]);
        assert_eq!(
            picked.skipped,
            vec![
                ("b".to_owned(), SkipReason::NoDevCases),
                ("c".to_owned(), SkipReason::DefinitionRemoved),
            ]
        );
    }

    /// The rounds `steps` (each round's request id with its later events)
    /// say.
    fn read(steps: &[(i64, EventKind, Value)]) -> Vec<Round> {
        let mut events = Vec::new();
        for (id, kind, payload) in steps {
            events.push((*id, RecordedKind::from(kind.as_str()), payload.clone()));
        }
        rounds(
            events
                .iter()
                .map(|(id, kind, payload)| (*id, kind, payload)),
        )
    }

    fn requested(id: i64) -> (i64, EventKind, Value) {
        let request = Request {
            agent: "adr-rules".into(),
            split: Split::Dev,
            k: None,
            cases: None,
            set: None,
            rerun: false,
            requested_by: "supervisor".into(),
            requested_by_id: "s1".into(),
            run_id: Some("r1".into()),
            before_landing: true,
            definition_commit: Some("head".into()),
        };
        (id, EventKind::AgentEvalRequested, request.record())
    }

    fn started(id: i64, eval: i64) -> (i64, EventKind, Value) {
        (
            id,
            EventKind::AgentEvalStarted,
            json!({"eval_id": eval, "provider": "claude", "definition_commit": "head",
                   "definition_digest": "d", "case_set_digest": "s", "planned": [["a", 1]],
                   "estimate": {"per_run_usd": 0.4}, "max_cost_usd": 30.0, "concurrency": 4,
                   "threshold": 0.9, "supervisor": "s1"}),
        )
    }

    fn finished(id: i64, eval: i64, fields: Value) -> (i64, EventKind, Value) {
        let mut payload = json!({"eval_id": eval, "outcome": "complete", "passed": false,
            "threshold": 0.9, "below": [], "scores": {"errors": 0}, "failed_cases": []});
        if let (Some(payload), Some(fields)) = (payload.as_object_mut(), fields.as_object()) {
            payload.extend(fields.clone());
        }
        (id, EventKind::AgentEvalFinished, payload)
    }

    fn judged(steps: &[(i64, EventKind, Value)]) -> Verdict {
        let rounds = read(steps);
        judge(
            rounds
                .iter()
                .map(|round| (round.request.agent.as_str(), round)),
        )
    }

    /// While another round runs, the run's round waits for its turn and the
    /// run with it; once it starts, the run waits for it to end; a round
    /// waiting for a held provider makes the run wait too.
    #[test]
    fn the_run_waits_while_its_round_waits_or_runs_and_says_why() {
        assert_eq!(judged(&[requested(1)]), Verdict::Wait(WaitReason::Queued));
        assert_eq!(
            judged(&[requested(1), started(2, 1)]),
            Verdict::Wait(WaitReason::Running)
        );
        let held = (
            2,
            EventKind::AgentEvalWaiting,
            json!({"eval_id": 1, "provider": "claude", "reason": "provider_held"}),
        );
        assert_eq!(
            judged(&[requested(1), held]),
            Verdict::Wait(WaitReason::ProviderHeld)
        );
        // One round of the run that waits keeps it waiting, whatever the
        // others' ends.
        assert_eq!(
            judged(&[
                requested(1),
                started(2, 1),
                finished(3, 1, json!({"passed": true})),
                requested(4),
            ]),
            Verdict::Wait(WaitReason::Queued)
        );
    }

    #[test]
    fn every_round_passed_lands() {
        assert_eq!(
            judged(&[
                requested(1),
                started(2, 1),
                finished(3, 1, json!({"passed": true})),
            ]),
            Verdict::Pass
        );
    }

    /// A complete round under the threshold sends the run back with what
    /// fell under it and each failed case's id and the agent's reasons.
    #[test]
    fn a_round_under_the_threshold_sends_back_its_failed_cases_and_reasons() {
        let below = finished(
            3,
            1,
            json!({
                "below": [{"metric": "code_recall", "value": 0.5}, {"metric": "code_precision", "value": null}],
                "failed_cases": [
                    {"id": "adr-missing-status", "runs": 2, "correct": 1, "errors": 0,
                     "reasons": [["A-1: no status"], ["A-1: status missing", "A-2"]]},
                    {"id": "clean-one", "runs": 1, "correct": 0, "errors": 0, "reasons": []},
                ],
            }),
        );
        let Verdict::SendBack(reasons) = judged(&[requested(1), started(2, 1), below]) else {
            panic!("a round under the threshold sends the run back");
        };
        assert_eq!(reasons.len(), 3);
        assert!(reasons[0].contains("code_recall 0.50"), "{}", reasons[0]);
        assert!(reasons[0].contains("code_precision none"), "{}", reasons[0]);
        assert!(
            reasons[1].contains("case adr-missing-status failed (1 of 2 run(s) right)")
                && reasons[1].contains("A-1: no status | A-1: status missing; A-2"),
            "{}",
            reasons[1]
        );
        assert!(
            reasons[2].ends_with("case clean-one failed (0 of 1 run(s) right)"),
            "{}",
            reasons[2]
        );
    }

    /// A round that could not tell goes to a person with why: refused
    /// (`cost_unknown` among them), incomplete, runs without a judgment, or
    /// a provider the supervisor does not have; one under the threshold
    /// beside it is told too.
    #[test]
    fn a_round_that_could_not_tell_asks_a_person_with_why() {
        let refused = (
            2,
            EventKind::AgentEvalRefused,
            json!({"eval_id": 1, "reason": "cost_unknown", "detail": "codex reports no dollars"}),
        );
        let Verdict::Ask(why) = judged(&[requested(1), refused]) else {
            panic!("a refused round asks");
        };
        assert!(
            why.contains("was not started: cost_unknown: codex reports no dollars"),
            "{why}"
        );
        let incomplete = finished(
            3,
            1,
            json!({"outcome": "incomplete", "incomplete_reason": "cost_limit"}),
        );
        let Verdict::Ask(why) = judged(&[requested(1), started(2, 1), incomplete]) else {
            panic!("an incomplete round asks");
        };
        assert!(why.contains("ended incomplete (cost_limit)"), "{why}");
        let errors = finished(3, 1, json!({"scores": {"errors": 2}}));
        let Verdict::Ask(why) = judged(&[requested(1), started(2, 1), errors]) else {
            panic!("a round with runs without a judgment asks");
        };
        assert!(why.contains("2 run(s) without a judgment"), "{why}");
        let no_claude = (
            2,
            EventKind::AgentEvalWaiting,
            json!({"eval_id": 1, "provider": "claude", "reason": "no_claude"}),
        );
        let Verdict::Ask(why) = judged(&[requested(1), no_claude]) else {
            panic!("a round no provider can run asks");
        };
        assert!(why.contains("cannot be used (no_claude)"), "{why}");
        // With a round under the threshold beside it, a person is told
        // both.
        let below = finished(5, 3, json!({"below": [{"metric": "recall", "value": 0.5}]}));
        let Verdict::Ask(why) = judged(&[
            requested(1),
            (
                2,
                EventKind::AgentEvalRefused,
                json!({"eval_id": 1, "reason": "over_cost_limit", "detail": "$40"}),
            ),
            requested(3),
            started(4, 3),
            below,
        ]) else {
            panic!("a refused round asks whatever the others");
        };
        assert!(
            why.contains("over_cost_limit") && why.contains("recall 0.50"),
            "{why}"
        );
    }
}
