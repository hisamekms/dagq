//! The replanning of long-running tasks in a window
//! (docs/plans/task-replanning.md, "回数・抑制・上限と観測"): the turns by
//! cause and the reviews, counted as the runtime's detection counts them
//! ([`crate::domain::replan_detect::count_turns_where`]), the operations
//! opened by origin, the diagnosis jobs started, what the runtime made of
//! each diagnosis (by outcome and, for a person, why), the jobs that
//! failed, and the moves of the operations by the status they reached
//! (the parks, the applies, the continuations and the withdrawals), so
//! that the candidates, the diagnoses and the parks read apart.
use crate::domain::EventKind;
use crate::domain::replan_detect::{TurnCounts, count_turns_where};
use std::collections::BTreeMap;

use serde::Serialize;

use super::{EventId, RunEvent, TaskId};

/// What the replanning did in a window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReplanStats {
    /// The turns sent and ended, by cause, and the reviews, of the turns
    /// and reviews in the window.
    pub turns: TurnCounts,
    /// The operations opened (`replan_requested` that opened one), by
    /// origin: `runtime` is the detection's candidates.
    pub opened: BTreeMap<String, usize>,
    /// The diagnosis jobs started.
    pub diagnoses_started: usize,
    /// The diagnoses ended with a verdict or an ask, by outcome
    /// (`continue`, `park`, `ask`, `discarded`).
    pub diagnosed: BTreeMap<String, usize>,
    /// The diagnoses that went to a person, by why.
    pub asked: BTreeMap<String, usize>,
    /// The diagnosis jobs that failed.
    pub diagnoses_failed: usize,
    /// The moves of the operations, by the status they reached.
    pub moved_to: BTreeMap<String, usize>,
}

/// Count the events with `after < id <= upto` whose task `counts` accepts;
/// the turns' causes read from every event before them.
pub fn replans(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> ReplanStats {
    let within = |event: &RunEvent| event.id > after && event.id <= upto && counts(event.task_id);
    let mut stats = ReplanStats {
        turns: count_turns_where(events, within).total,
        ..ReplanStats::default()
    };
    let text = |event: &RunEvent, key: &str| {
        event.payload[key]
            .as_str()
            .unwrap_or(super::asks::UNKNOWN)
            .to_owned()
    };
    for event in events.iter().filter(|event| within(event)) {
        match event.kind.known() {
            Some(EventKind::ReplanRequested) if event.payload["created"] == true => {
                *stats.opened.entry(text(event, "origin")).or_default() += 1;
            }
            Some(EventKind::ReplanDiagnosisStarted) => stats.diagnoses_started += 1,
            Some(EventKind::ReplanDiagnosed) => {
                *stats.diagnosed.entry(text(event, "outcome")).or_default() += 1;
                if event.payload["outcome"] == "ask" {
                    *stats.asked.entry(text(event, "why")).or_default() += 1;
                }
            }
            Some(EventKind::ReplanDiagnosisFailed) => stats.diagnoses_failed += 1,
            Some(EventKind::ReplanStatusChanged) => {
                *stats.moved_to.entry(text(event, "to")).or_default() += 1;
            }
            _ => {}
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RunId;
    use serde_json::{Value, json};

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(4)),
            goal_id: None,
            run_id: Some(RunId::new("r1").unwrap()),
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    /// The window's candidates, diagnoses and moves read apart, and the
    /// turns of the window only, by the cause of the events before them.
    #[test]
    fn the_windows_candidates_diagnoses_and_parks_are_counted_apart() {
        let events = vec![
            event(1, "conflict_precheck", json!({})),
            event(2, "turn_started", json!({"what": "resolution request"})),
            event(3, "review_finished", json!({"verdict": "revise"})),
            event(
                4,
                "replan_requested",
                json!({"origin": "runtime", "created": true}),
            ),
            event(
                5,
                "replan_requested",
                json!({"origin": "manual", "created": false}),
            ),
            event(6, "replan_diagnosis_started", json!({"operation_id": 1})),
            event(7, "replan_diagnosis_failed", json!({"operation_id": 1})),
            event(
                8,
                "replan_diagnosed",
                json!({"operation_id": 1, "outcome": "ask", "why": "low_confidence"}),
            ),
            event(9, "replan_status_changed", json!({"to": "pause_requested"})),
            event(
                10,
                "replan_diagnosed",
                json!({"operation_id": 2, "outcome": "continue"}),
            ),
        ];
        let stats = replans(&events, EventId::new(1), EventId::new(10), |_| true);
        assert_eq!(stats.turns.by_cause["conflict"], 1);
        assert_eq!((stats.turns.started, stats.turns.non_pass), (1, 1));
        assert_eq!(stats.opened, BTreeMap::from([("runtime".into(), 1)]));
        assert_eq!((stats.diagnoses_started, stats.diagnoses_failed), (1, 1));
        assert_eq!(
            stats.diagnosed,
            BTreeMap::from([("ask".into(), 1), ("continue".into(), 1)])
        );
        assert_eq!(stats.asked, BTreeMap::from([("low_confidence".into(), 1)]));
        assert_eq!(
            stats.moved_to,
            BTreeMap::from([("pause_requested".into(), 1)])
        );
        let none = replans(&events, EventId::new(10), EventId::new(10), |_| true);
        assert_eq!(none, ReplanStats::default());
        let other = replans(&events, EventId::new(0), EventId::new(10), |_| false);
        assert_eq!(other.diagnoses_started, 0);
    }
}
