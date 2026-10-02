//! How the AI's recommendations fared (ADR-t451-1 decision 1): per ask
//! kind, the answered asks that carried a `recommendation` and how many of
//! the answers chose it, and the judgements an AI made itself without
//! opening that kind of ask. Derived from `run_events` like the rest of
//! `stats`: the recommendation from `ask_opened`, the chosen option from
//! `ask_answered` (its `option`, or the `reasoned_option` of a
//! `send_back: <reason>`), and the decisions from the events listed in
//! [`DECIDED_WITHOUT_ASK`].

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use serde_json::Value;

use super::{EventId, RunEvent, TaskId};
use crate::domain::ANSWERED_BY_RUNTIME;

/// The recommendations and the AI's own decisions of a window.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Recommendations {
    /// Per ask kind, the asks answered in the window whose `ask_opened`
    /// carried a recommendation. An answer the runtime wrote itself (a
    /// withdrawn or superseded ask) is no one's choice and is left out.
    pub by_kind: BTreeMap<String, RecommendationMatch>,
    /// Per ask kind, the judgements of the window an AI made itself
    /// instead of opening that kind of ask.
    pub decided_without_ask: BTreeMap<String, i64>,
}

/// The answered asks of one kind that carried a recommendation.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RecommendationMatch {
    pub answered: i64,
    /// Of those, the answers that chose the recommended option.
    pub matched: i64,
    /// `matched / answered`, to three decimals; `None` without answers.
    pub rate: Option<f64>,
}

/// An event that records an AI's judgement made without an ask: the ask
/// kind it stood in for, and which of its events count.
pub struct DecidedWithoutAsk {
    pub event: &'static str,
    pub ask_kind: &'static str,
    pub applies: fn(&Value) -> bool,
}

/// The records of the AI's own decisions, per ask kind. The later
/// implementations of ADR-t451-1 add theirs here (the review's
/// `concern_decided`, the plan review's `plan_concern_decided`, the
/// observer's findings without an ask).
pub const DECIDED_WITHOUT_ASK: &[DecidedWithoutAsk] = &[
    DecidedWithoutAsk {
        // A follow_up draft a planner of the runtime's adopted on its own,
        // with no planner_question answered: not a person's submission.
        event: "follow_up_adopted",
        ask_kind: "planner_question",
        applies: |payload| payload["by"].as_str() == Some("planner") && payload["ask_id"].is_null(),
    },
    DecidedWithoutAsk {
        // A plan review's sure concern the runtime applied as its
        // `ready` or `send_back` (ADR-t451-1 decision 4), not one it left
        // to a person in an approve_plan ask.
        event: crate::domain::event_kind::PLAN_CONCERN_DECIDED,
        ask_kind: "approve_plan",
        applies: |payload| payload["applied"].as_bool() == Some(true),
    },
    DecidedWithoutAsk {
        // A review's concern the runtime landed or sent back on the job's
        // recommendation (ADR-t451-1 decision 3); one asked of a person
        // records `applied: false`.
        event: crate::domain::event_kind::CONCERN_DECIDED,
        ask_kind: "approve_landing",
        applies: |payload| payload["applied"].as_bool() == Some(true),
    },
];

/// Count the asks answered and the decisions recorded with `after < id <=
/// upto` whose task `counts` accepts. The recommendation of an ask opened
/// before the window is read from its `ask_opened` all the same.
pub fn recommendations(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> Recommendations {
    let mut stats = Recommendations::default();
    let mut recommended: HashMap<String, String> = HashMap::new();
    for event in events.iter().filter(|event| event.id <= upto) {
        let payload = &event.payload;
        match event.kind.as_str() {
            "ask_opened" => {
                if let (Some(id), Some(option)) =
                    (ask_id(payload), payload["recommendation"].as_str())
                {
                    recommended.insert(id, option.trim().to_owned());
                }
            }
            "ask_answered" if event.id > after && counts(event.task_id) => {
                let Some(option) = ask_id(payload).and_then(|id| recommended.get(&id)) else {
                    continue;
                };
                if payload["answered_by"].as_str() == Some(ANSWERED_BY_RUNTIME)
                    || payload["runtime_closed"].as_bool() == Some(true)
                {
                    continue;
                }
                let kind = payload["kind"].as_str().unwrap_or(super::asks::UNKNOWN);
                let entry = stats.by_kind.entry(kind.to_owned()).or_default();
                entry.answered += 1;
                if chosen(payload, kind).is_some_and(|chosen| chosen.trim() == option) {
                    entry.matched += 1;
                }
            }
            kind if event.id > after && counts(event.task_id) => {
                for decided in DECIDED_WITHOUT_ASK
                    .iter()
                    .filter(|decided| decided.event == kind && (decided.applies)(payload))
                {
                    *stats
                        .decided_without_ask
                        .entry(decided.ask_kind.to_owned())
                        .or_default() += 1;
                }
            }
            _ => {}
        }
    }
    for entry in stats.by_kind.values_mut() {
        entry.rate = super::worker_question_topics::rate(entry.matched, entry.answered);
    }
    stats
}

/// The option an answer chose: the exact `option`, or for a kind that
/// takes `<option>: <reason>` the `reasoned_option` (task 1389). A free
/// answer of another kind chooses none.
fn chosen<'a>(payload: &'a Value, kind: &str) -> Option<&'a str> {
    payload["option"].as_str().or_else(|| {
        crate::domain::AskKind::read(kind)
            .takes_reasoned_answers()
            .then(|| payload["reasoned_option"].as_str())
            .flatten()
    })
}

fn ask_id(payload: &Value) -> Option<String> {
    payload
        .get("ask_id")
        .map(|id| id.as_str().map_or_else(|| id.to_string(), str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: "2026-10-02T00:00:00.000Z".into(),
            actor: None,
        }
    }

    fn opened(id: i64, ask: i64, kind: &str, recommendation: Option<&str>) -> RunEvent {
        event(
            id,
            "ask_opened",
            json!({"ask_id": ask, "kind": kind, "recommendation": recommendation}),
        )
    }

    fn answered(id: i64, ask: i64, kind: &str, option: Option<&str>, by: &str) -> RunEvent {
        event(
            id,
            "ask_answered",
            json!({"ask_id": ask, "kind": kind, "option": option, "answered_by": by}),
        )
    }

    #[test]
    fn answers_are_matched_against_the_recommendation_of_their_ask() {
        let events = vec![
            opened(1, 10, "planner_question", Some("adopt")),
            opened(2, 11, "planner_question", Some("adopt")),
            opened(3, 12, "planner_question", None),
            opened(4, 13, "blocked", Some("leave it")),
            opened(5, 14, "blocked", Some("leave it")),
            answered(6, 10, "planner_question", Some("adopt"), "inbox"),
            answered(7, 11, "planner_question", None, "person"),
            answered(8, 12, "planner_question", Some("adopt"), "inbox"),
            answered(9, 13, "blocked", Some(" leave it "), "inbox"),
            answered(10, 14, "blocked", Some("leave it"), "runtime"),
        ];
        let stats = recommendations(&events, EventId::new(0), EventId::new(10), |_| true);
        assert_eq!(
            stats.by_kind["planner_question"],
            RecommendationMatch {
                answered: 2,
                matched: 1,
                rate: Some(0.5)
            }
        );
        assert_eq!(
            stats.by_kind["blocked"],
            RecommendationMatch {
                answered: 1,
                matched: 1,
                rate: Some(1.0)
            }
        );
        assert!(stats.decided_without_ask.is_empty());
    }

    fn reasoned(id: i64, ask: i64, kind: &str, option: &str) -> RunEvent {
        event(
            id,
            "ask_answered",
            json!({"ask_id": ask, "kind": kind, "option_index": null,
                   "reasoned_option": option, "answered_by": "inbox"}),
        )
    }

    #[test]
    fn a_send_back_with_a_reason_matches_a_send_back_recommendation() {
        let events = vec![
            opened(1, 10, "approve_plan", Some("send_back")),
            opened(2, 11, "approve_landing", Some("send_back")),
            opened(3, 12, "approve_landing", Some("land")),
            opened(4, 13, "planner_question", Some("adopt")),
            opened(5, 14, "approve_plan", Some("send_back")),
            opened(6, 15, "approve_plan", Some("ready")),
            reasoned(7, 10, "approve_plan", "send_back"),
            reasoned(8, 11, "approve_landing", "send_back"),
            reasoned(9, 12, "approve_landing", "send_back"),
            // Another kind's free answer chooses no option, even with a
            // stray reasoned_option.
            reasoned(10, 13, "planner_question", "adopt"),
            answered(11, 14, "approve_plan", Some("send_back"), "inbox"),
            answered(12, 15, "approve_plan", None, "inbox"),
        ];
        let stats = recommendations(&events, EventId::new(0), EventId::new(12), |_| true);
        assert_eq!(
            stats.by_kind["approve_plan"],
            RecommendationMatch {
                answered: 3,
                matched: 2,
                rate: Some(0.667)
            }
        );
        assert_eq!(
            stats.by_kind["approve_landing"],
            RecommendationMatch {
                answered: 2,
                matched: 1,
                rate: Some(0.5)
            }
        );
        assert_eq!(stats.by_kind["planner_question"].matched, 0);
    }

    #[test]
    fn an_ask_opened_before_the_window_still_carries_its_recommendation() {
        let events = vec![
            opened(1, 10, "approve_landing", Some("land")),
            answered(5, 10, "approve_landing", Some("send_back"), "inbox"),
        ];
        let stats = recommendations(&events, EventId::new(2), EventId::new(5), |_| true);
        assert_eq!(stats.by_kind["approve_landing"].matched, 0);
        assert_eq!(stats.by_kind["approve_landing"].rate, Some(0.0));
        let none = recommendations(&events, EventId::new(2), EventId::new(5), |_| false);
        assert!(none.by_kind.is_empty());
    }

    #[test]
    fn a_runtime_planners_own_adoption_counts_as_decided_without_a_planner_question() {
        let events = vec![
            event(
                1,
                "follow_up_adopted",
                json!({"by": "planner", "ask_id": null}),
            ),
            event(
                2,
                "follow_up_adopted",
                json!({"by": "person", "ask_id": null}),
            ),
            event(
                3,
                "follow_up_adopted",
                json!({"by": "planner", "ask_id": 4}),
            ),
            event(4, "draft_adopted", json!({"by": "planner", "ask_id": null})),
        ];
        let stats = recommendations(&events, EventId::new(0), EventId::new(4), |_| true);
        assert_eq!(
            stats.decided_without_ask,
            BTreeMap::from([("planner_question".to_owned(), 1)])
        );
    }

    #[test]
    fn a_plan_concern_the_runtime_applied_counts_as_decided_without_an_approve_plan() {
        let events = vec![
            event(1, "plan_concern_decided", json!({"applied": true})),
            event(2, "plan_concern_decided", json!({"applied": false})),
            event(3, "plan_concern_decided", json!({"applied": true})),
        ];
        let stats = recommendations(&events, EventId::new(0), EventId::new(3), |_| true);
        assert_eq!(
            stats.decided_without_ask,
            BTreeMap::from([("approve_plan".to_owned(), 2)])
        );
    }

    #[test]
    fn a_concern_the_runtime_applied_counts_as_decided_without_an_approve_landing() {
        let events = vec![
            event(
                1,
                "concern_decided",
                json!({"applied": true, "recommendation": "land"}),
            ),
            event(
                2,
                "concern_decided",
                json!({"applied": true, "recommendation": "send_back"}),
            ),
            event(
                3,
                "concern_decided",
                json!({"applied": false, "escalated_because": "scope"}),
            ),
        ];
        let stats = recommendations(&events, EventId::new(0), EventId::new(3), |_| true);
        assert_eq!(
            stats.decided_without_ask,
            BTreeMap::from([("approve_landing".to_owned(), 2)])
        );
    }
}
