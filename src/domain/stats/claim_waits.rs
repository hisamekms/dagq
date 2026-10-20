//! The claims' waits per execution class (ADR-t1487-2): from when the
//! task became claimable (`run_claimed`'s `ready_at`) to its claim, for the
//! claims in the window. A claim recorded before the claims carried the
//! class reads as an implementation's (ADR-t1487-1), and one without a
//! `ready_at` is not counted.

use crate::domain::EventKind;
use std::collections::BTreeMap;

use serde::Deserialize;

use super::asks::Spread;
use super::{EventId, RunEvent, TaskId, timestamp_millis};
use crate::domain::ExecutionClass;
use crate::domain::run::payload::{restore, typed};

/// What [`claim_waits`] reads of a `run_claimed`, restored as typed values:
/// a missing key, `null` or a value of another type (an older or broken
/// writer) reads as absent, so no recorded claim is refused.
#[derive(Debug, Default, Deserialize)]
struct Claimed {
    #[serde(default, deserialize_with = "typed")]
    ready_at: Option<String>,
    #[serde(default, deserialize_with = "typed")]
    execution_class: Option<ExecutionClass>,
}

/// The `run_claimed` events with `after < id <= upto` whose task `counts`
/// accepts, by the class of the task claimed: the seconds each waited from
/// `ready_at` to the claim.
pub fn claim_waits(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> BTreeMap<&'static str, Spread> {
    let mut secs: BTreeMap<&'static str, Vec<i64>> = BTreeMap::new();
    for event in events.iter().filter(|event| {
        event.kind == EventKind::RunClaimed
            && event.id > after
            && event.id <= upto
            && counts(event.task_id)
    }) {
        let claimed: Claimed = restore(&event.payload);
        let Some(ready_at) = claimed.ready_at.as_deref().and_then(timestamp_millis) else {
            continue;
        };
        let Some(claimed_at) = timestamp_millis(&event.created_at) else {
            continue;
        };
        let class = claimed.execution_class.unwrap_or_default();
        secs.entry(class.as_str())
            .or_default()
            .push(((claimed_at - ready_at) / 1000).max(0));
    }
    secs.into_iter()
        .map(|(class, secs)| (class, Spread::of(secs)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::event_kind::RUN_CLAIMED;
    use serde_json::{Value, json};

    fn claimed(id: i64, task: i64, at: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(task)),
            goal_id: None,
            run_id: None,
            kind: RUN_CLAIMED.into(),
            payload,
            created_at: at.to_owned(),
            actor: None,
        }
    }

    #[test]
    fn the_claims_waits_are_split_by_the_class_claimed() {
        let events = [
            // Before the window.
            claimed(
                1,
                1,
                "2026-10-11 10:00:00",
                json!({"ready_at": "2026-10-11 09:00:00", "execution_class": "spike"}),
            ),
            claimed(
                2,
                2,
                "2026-10-11 10:10:00",
                json!({"ready_at": "2026-10-11 10:00:00", "execution_class": "spike"}),
            ),
            claimed(
                3,
                3,
                "2026-10-11 10:30:00",
                json!({"ready_at": "2026-10-11 10:00:00", "execution_class": "spike"}),
            ),
            // An older claim without the class: an implementation's.
            claimed(
                4,
                4,
                "2026-10-11 10:01:00",
                json!({"ready_at": "2026-10-11 10:00:00"}),
            ),
            claimed(
                5,
                5,
                "2026-10-11 10:02:00",
                json!({"ready_at": "2026-10-11 10:00:00", "execution_class": "implementation"}),
            ),
            // No `ready_at`: not counted.
            claimed(
                6,
                6,
                "2026-10-11 10:02:00",
                json!({"execution_class": "spike"}),
            ),
            // Another goal's.
            claimed(
                7,
                70,
                "2026-10-11 12:00:00",
                json!({"ready_at": "2026-10-11 10:00:00", "execution_class": "spike"}),
            ),
        ];
        let waits = claim_waits(&events, EventId::new(1), EventId::new(7), |task| {
            task != Some(TaskId::new(70))
        });
        assert_eq!(
            serde_json::to_value(&waits).unwrap(),
            json!({
                "implementation": {"count": 2, "median": 90, "p90": 120, "max": 120},
                "spike": {"count": 2, "median": 1200, "p90": 1800, "max": 1800},
            })
        );
    }

    /// What an older or broken writer left reads as absent: an unknown
    /// class is an implementation's, a `ready_at` that is no text is not
    /// counted, and a payload that is no object counts nothing.
    #[test]
    fn an_unreadable_claim_reads_as_absent() {
        let events = [
            claimed(
                1,
                1,
                "2026-10-11 10:03:00",
                json!({"ready_at": "2026-10-11 10:00:00", "execution_class": "research"}),
            ),
            claimed(
                2,
                2,
                "2026-10-11 10:03:00",
                json!({"ready_at": 5, "execution_class": "spike"}),
            ),
            claimed(3, 3, "2026-10-11 10:03:00", json!("ready")),
        ];
        let waits = claim_waits(&events, EventId::new(0), EventId::new(3), |_| true);
        assert_eq!(
            serde_json::to_value(&waits).unwrap(),
            json!({"implementation": {"count": 1, "median": 180, "p90": 180, "max": 180}})
        );
    }
}
