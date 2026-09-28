//! The moves of workers to the other provider (ADR-t813-2): the
//! `provider_switched` events of the window by reason, by the pair of
//! providers and by phase, with the runs that moved, and the holds of
//! Codex (`provider_held`) by reason, so that how often a provider could
//! not be used reads from one place.
use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

use super::{EventId, RunEvent, TaskId, asks::UNKNOWN};
use crate::domain::event_kind;

/// The switches and provider holds of a window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ProviderSwitchStats {
    pub count: i64,
    /// The runs that moved at least once.
    pub runs: i64,
    /// By `reason` (`executable_missing`, `authentication`, `usage_limit`,
    /// `launch_failed`).
    pub by_reason: BTreeMap<String, i64>,
    /// By `<from>-><to>`.
    pub by_direction: BTreeMap<String, i64>,
    /// By `phase` (`start`, `answer`, `revise`, `resume`, `nudge`).
    pub by_phase: BTreeMap<String, i64>,
    /// The holds of Codex that started in the window, by `reason`.
    pub holds_by_reason: BTreeMap<String, i64>,
}

/// Count the `provider_switched` and `provider_held` events with `after <
/// id <= upto` whose task `counts` accepts (a hold, a queue event, is
/// counted whatever `counts` says of no task).
pub fn provider_switches(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> ProviderSwitchStats {
    let mut stats = ProviderSwitchStats::default();
    let mut runs = BTreeSet::new();
    let text = |payload: &Value, key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or(UNKNOWN)
            .to_owned()
    };
    for event in events
        .iter()
        .filter(|event| event.id > after && event.id <= upto)
    {
        let payload = &event.payload;
        match event.kind.as_str() {
            event_kind::PROVIDER_SWITCHED if counts(event.task_id) => {
                stats.count += 1;
                if let Some(run) = &event.run_id {
                    runs.insert(run.clone());
                }
                *stats.by_reason.entry(text(payload, "reason")).or_default() += 1;
                *stats
                    .by_direction
                    .entry(format!(
                        "{}->{}",
                        text(payload, "from"),
                        text(payload, "to")
                    ))
                    .or_default() += 1;
                *stats.by_phase.entry(text(payload, "phase")).or_default() += 1;
            }
            event_kind::PROVIDER_HELD => {
                *stats
                    .holds_by_reason
                    .entry(text(payload, "reason"))
                    .or_default() += 1;
            }
            _ => {}
        }
    }
    stats.runs = runs.len() as i64;
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RunId;
    use serde_json::json;

    fn event(id: i64, kind: &str, run: Option<&str>, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: run.map(|_| TaskId::new(1)),
            goal_id: None,
            run_id: run.map(|r| RunId::new(r).unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: "2026-09-28T00:00:00Z".to_owned(),
            actor: None,
        }
    }

    #[test]
    fn switches_are_counted_by_reason_direction_and_phase() {
        let events = [
            event(
                1,
                event_kind::PROVIDER_SWITCHED,
                Some("a"),
                json!({"from": "codex", "to": "claude", "reason": "executable_missing", "phase": "start"}),
            ),
            event(
                2,
                event_kind::PROVIDER_SWITCHED,
                Some("b"),
                json!({"from": "claude", "to": "codex", "reason": "usage_limit", "phase": "answer"}),
            ),
            event(
                3,
                event_kind::PROVIDER_SWITCHED,
                Some("b"),
                json!({"from": "codex", "to": "claude", "reason": "usage_limit", "phase": "nudge"}),
            ),
            event(
                4,
                event_kind::PROVIDER_HELD,
                None,
                json!({"provider": "codex", "reason": "usage_limit"}),
            ),
            event(5, event_kind::TURN_FINISHED, Some("a"), json!({})),
        ];
        let stats = provider_switches(&events, EventId::new(0), EventId::new(5), |_| true);
        assert_eq!(stats.count, 3);
        assert_eq!(stats.runs, 2);
        assert_eq!(stats.by_reason["usage_limit"], 2);
        assert_eq!(stats.by_reason["executable_missing"], 1);
        assert_eq!(stats.by_direction["codex->claude"], 2);
        assert_eq!(stats.by_direction["claude->codex"], 1);
        assert_eq!(stats.by_phase["start"], 1);
        assert_eq!(stats.holds_by_reason["usage_limit"], 1);
        // Out of the window, or not counted.
        let stats = provider_switches(&events, EventId::new(1), EventId::new(5), |_| false);
        assert_eq!(stats.count, 0);
        assert_eq!(stats.holds_by_reason["usage_limit"], 1);
    }
}
