//! The life of a supervisor read from the events alone (ADR-t1662-1
//! decisions 6 and 14, docs/design/measurement.md "supervisorの一生"): its
//! stop (`supervisor_stopped`, recorded in the transaction that removes
//! its registration) and its evidence of life (every event naming it as
//! its `supervisor`, `supervisor_alive` at least every
//! [`SUPERVISOR_ALIVE_INTERVAL_SECS`]). The registrations and their
//! heartbeats are state, which the measurement does not read.

use serde_json::Value;

use super::{HEARTBEAT_TIMEOUT_SECS, RunEvent, event_kind, stats::timestamp_millis};

/// How often a supervisor records `supervisor_alive` at most: its
/// heartbeat writes the registration every two seconds, and one event
/// every five minutes keeps the records small. A supervisor that died is
/// read as ending at its last evidence, up to this much early, until the
/// prune's `supervisor_stopped` brings its last heartbeat.
pub const SUPERVISOR_ALIVE_INTERVAL_SECS: i64 = 300;

/// Whether a supervisor whose last `supervisor_alive` (or its start) was
/// `since_secs` ago records another now.
pub fn alive_due(since_secs: i64) -> bool {
    since_secs >= SUPERVISOR_ALIVE_INTERVAL_SECS
}

/// What ended a supervisor's life, and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifeEnd {
    /// Its `supervisor_stopped`: the earlier of its time and the
    /// `last_heartbeat_at` it carries (a prune's).
    Stopped { at_ms: i64 },
    /// No stop, and nothing of it for longer than its evidence of life
    /// allows: it ended at its last event.
    Silent { at_ms: i64 },
}

impl LifeEnd {
    pub fn at_ms(self) -> i64 {
        match self {
            Self::Stopped { at_ms } | Self::Silent { at_ms } => at_ms,
        }
    }
}

/// The end of the life of the supervisor `token` among `events` (ascending
/// id) at `now_ms`, `None` while it lives (or never ran). Its
/// `supervisor_stopped` ends it; without one, its last event (any whose
/// `supervisor` is `token`) ends it once `now_ms` is more than
/// [`SUPERVISOR_ALIVE_INTERVAL_SECS`] and [`HEARTBEAT_TIMEOUT_SECS`] after
/// it. A token of a build that recorded no `supervisor_alive` is read the
/// same way.
pub fn supervisor_life_end(events: &[RunEvent], token: &str, now_ms: i64) -> Option<LifeEnd> {
    let mut last: Option<i64> = None;
    for event in events {
        if event.payload.get("supervisor").and_then(Value::as_str) != Some(token) {
            continue;
        }
        let Some(at) = timestamp_millis(&event.created_at) else {
            continue;
        };
        if event.kind == event_kind::SUPERVISOR_STOPPED {
            let heartbeat = event
                .payload
                .get("last_heartbeat_at")
                .and_then(Value::as_i64)
                .and_then(|secs| secs.checked_mul(1000));
            return Some(LifeEnd::Stopped {
                at_ms: heartbeat.map_or(at, |heartbeat| heartbeat.min(at)),
            });
        }
        last = Some(last.map_or(at, |last: i64| last.max(at)));
    }
    let last = last?;
    let silent_after = (SUPERVISOR_ALIVE_INTERVAL_SECS + HEARTBEAT_TIMEOUT_SECS) * 1000;
    (now_ms > last + silent_after).then_some(LifeEnd::Silent { at_ms: last })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;
    use serde_json::json;

    pub(crate) fn event(id: i64, kind: &str, payload: Value, minute: i64) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: crate::domain::marks::utc_text(minute * 60_000),
            actor: None,
        }
    }

    const MIN: i64 = 60_000;

    #[test]
    fn an_alive_is_due_once_the_interval_passed() {
        assert!(!alive_due(SUPERVISOR_ALIVE_INTERVAL_SECS - 1));
        assert!(alive_due(SUPERVISOR_ALIVE_INTERVAL_SECS));
    }

    /// (iii) A supervisor that records nothing but its evidence of life
    /// lives on; (i) one whose evidence stopped (a stale registration)
    /// ends silent at its last event, and once the prune's stop comes, at
    /// its last heartbeat; (ii) one that removed its registration ends at
    /// its stop.
    #[test]
    fn a_life_ends_at_its_stop_or_its_silence_and_not_while_alive() {
        let alive: Vec<RunEvent> = (0..10)
            .map(|n| event(n, "supervisor_alive", json!({"supervisor": "s"}), n * 5))
            .collect();
        assert_eq!(supervisor_life_end(&alive, "s", 47 * MIN), None);
        assert_eq!(supervisor_life_end(&alive, "other", 47 * MIN), None);

        let mut stale = vec![
            event(1, "supervisor_started", json!({"supervisor": "s"}), 0),
            event(2, "supervisor_alive", json!({"supervisor": "s"}), 5),
            event(3, "slots_full_started", json!({"supervisor": "s"}), 7),
            event(4, "supervisor_alive", json!({"supervisor": "t"}), 30),
        ];
        // Within the interval and the timeout of its last event, it lives.
        assert_eq!(supervisor_life_end(&stale, "s", 12 * MIN), None);
        assert_eq!(
            supervisor_life_end(&stale, "s", 13 * MIN),
            Some(LifeEnd::Silent { at_ms: 7 * MIN })
        );
        stale.push(event(
            5,
            "supervisor_stopped",
            json!({"supervisor": "s", "outcome": "pruned", "last_heartbeat_at": 9 * 60}),
            40,
        ));
        assert_eq!(
            supervisor_life_end(&stale, "s", 41 * MIN),
            Some(LifeEnd::Stopped { at_ms: 9 * MIN })
        );

        let stopped = [
            event(1, "supervisor_started", json!({"supervisor": "s"}), 0),
            event(
                2,
                "supervisor_stopped",
                json!({"supervisor": "s", "outcome": "stopped"}),
                3,
            ),
        ];
        assert_eq!(
            supervisor_life_end(&stopped, "s", 4 * MIN),
            Some(LifeEnd::Stopped { at_ms: 3 * MIN })
        );
    }
}
