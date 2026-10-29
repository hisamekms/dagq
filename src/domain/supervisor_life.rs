//! The life of a supervisor read from the events alone (ADR-t1662-1
//! decisions 6 and 14, docs/design/measurement.md "supervisorの一生"): its
//! stop (`supervisor_stopped`, recorded in the transaction that removes
//! its registration) and its evidence of life (every event naming it as
//! its `supervisor`, `supervisor_alive` at least every
//! [`SUPERVISOR_ALIVE_INTERVAL_SECS`]). The registrations and their
//! heartbeats are state, which the measurement does not read.

use crate::domain::EventKind;
use std::collections::HashMap;

use serde_json::Value;

use super::{EventId, HEARTBEAT_TIMEOUT_SECS, RunEvent, stats::timestamp_millis};

/// How often a supervisor records `supervisor_alive` at most: its
/// heartbeat writes the registration every two seconds, and one event
/// every five minutes keeps the records small. A supervisor that died is
/// read as ending at its last evidence, up to this much early, until the
/// prune's `supervisor_stopped` brings its last heartbeat.
pub const SUPERVISOR_ALIVE_INTERVAL_SECS: i64 = 300;

/// How long after its last event a supervisor with no stop is read as
/// silent: its life ends at that event once this much more has passed.
pub const SILENT_AFTER_MS: i64 = (SUPERVISOR_ALIVE_INTERVAL_SECS + HEARTBEAT_TIMEOUT_SECS) * 1000;

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
    let mut life = Life::default();
    for event in events
        .iter()
        .filter(|event| supervisor_of(event) == Some(token))
    {
        life.read(event);
        if life.stopped.is_some() {
            break;
        }
    }
    life.end(now_ms)
}

/// The end of the life of every supervisor of `events` (ascending id),
/// read in one pass: what [`supervisor_life_end`] gives each, without
/// reading all the events again per supervisor. It is kept as it is and
/// given each later event ([`Self::add`]), so a reader that holds it reads
/// no event twice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lives {
    by_token: HashMap<String, Life>,
}

impl Lives {
    pub fn new(events: &[RunEvent]) -> Self {
        let mut lives = Self::default();
        for event in events {
            lives.add(event);
        }
        lives
    }

    /// Read `event`, after every one read before.
    pub fn add(&mut self, event: &RunEvent) {
        if let Some(token) = supervisor_of(event) {
            match self.by_token.get_mut(token) {
                Some(life) => life.read(event),
                None => {
                    let mut life = Life::default();
                    life.read(event);
                    self.by_token.insert(token.to_owned(), life);
                }
            }
        }
    }

    /// How many supervisors it holds a life of.
    pub fn supervisors(&self) -> usize {
        self.by_token.len()
    }

    /// Roughly how many bytes it holds.
    pub fn approx_bytes(&self) -> usize {
        self.by_token
            .keys()
            .map(|token| size_of::<(String, Life)>() + token.len())
            .sum()
    }

    /// [`supervisor_life_end`] of `token` at `now_ms`.
    pub fn end(&self, token: &str, now_ms: i64) -> Option<LifeEnd> {
        self.by_token.get(token).and_then(|life| life.end(now_ms))
    }

    /// The earliest time at which [`Self::end`] of `token` gives an end
    /// as its events read so far tell: any time once it stopped, else the
    /// first past the silence after its last event; none while nothing of
    /// it can end it.
    pub fn ends_from(&self, token: &str) -> Option<i64> {
        self.by_token.get(token).and_then(|life| life.ends_from())
    }

    /// The last event naming `token` as its supervisor, its stop or a
    /// later one too: what a row its life ended reads up to.
    pub fn last_of(&self, token: &str) -> Option<EventId> {
        self.by_token.get(token).map(|life| life.last_id)
    }
}

/// A supervisor's events read in order: the end its first
/// `supervisor_stopped` gives, or else the latest time of them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Life {
    stopped: Option<i64>,
    last: Option<i64>,
    /// The last event naming it, whatever it is.
    last_id: EventId,
}

impl Life {
    /// Read one event naming the supervisor; nothing after its stop, and
    /// nothing whose time does not parse.
    fn read(&mut self, event: &RunEvent) {
        self.last_id = self.last_id.max(event.id);
        if self.stopped.is_some() {
            return;
        }
        let Some(at) = timestamp_millis(&event.created_at) else {
            return;
        };
        if event.kind == EventKind::SupervisorStopped {
            let heartbeat = event
                .payload
                .get("last_heartbeat_at")
                .and_then(Value::as_i64)
                .and_then(|secs| secs.checked_mul(1000));
            self.stopped = Some(heartbeat.map_or(at, |heartbeat| heartbeat.min(at)));
        } else {
            self.last = Some(self.last.map_or(at, |last| last.max(at)));
        }
    }

    fn end(self, now_ms: i64) -> Option<LifeEnd> {
        if let Some(at_ms) = self.stopped {
            return Some(LifeEnd::Stopped { at_ms });
        }
        let last = self.last?;
        (now_ms >= self.ends_from()?).then_some(LifeEnd::Silent { at_ms: last })
    }

    fn ends_from(self) -> Option<i64> {
        if self.stopped.is_some() {
            return Some(i64::MIN);
        }
        Some(self.last?.saturating_add(SILENT_AFTER_MS + 1))
    }
}

/// The kinds of event of a supervisor's own life and settings.
pub const KINDS: [EventKind; 8] = [
    EventKind::SupervisorStarted,
    EventKind::SupervisorAlive,
    EventKind::SupervisorStopped,
    EventKind::SupervisorDraining,
    EventKind::SupervisorHandedOff,
    EventKind::SupervisorHeartbeatRetried,
    EventKind::SupervisorHeartbeatFailed,
    EventKind::SupervisorConfigChanged,
];

/// The supervisor an event names as its writer (its `supervisor`), if it
/// names one.
pub fn supervisor_of(event: &RunEvent) -> Option<&str> {
    event.payload.get("supervisor").and_then(Value::as_str)
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
            kind: kind.into(),
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

    /// The end as one supervisor's scan of every event read it: its first
    /// stop whose time parses, else its latest time once silent.
    fn life_end_by_scan(events: &[RunEvent], token: &str, now_ms: i64) -> Option<LifeEnd> {
        let mut last: Option<i64> = None;
        for event in events {
            if event.payload.get("supervisor").and_then(Value::as_str) != Some(token) {
                continue;
            }
            let Some(at) = timestamp_millis(&event.created_at) else {
                continue;
            };
            if event.kind == EventKind::SupervisorStopped {
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
        (now_ms > last + SILENT_AFTER_MS).then_some(LifeEnd::Silent { at_ms: last })
    }

    /// Every supervisor's end read in one pass ([`Lives`]) and by
    /// [`supervisor_life_end`] is the one its own scan gave: a stop with
    /// and without a heartbeat, a stop whose time does not parse, events
    /// after the stop, clocks out of order, silence and a life going on.
    #[test]
    fn the_lives_read_in_one_pass_end_as_each_supervisors_scan_does() {
        let mut events = vec![
            event(1, "supervisor_started", json!({"supervisor": "a"}), 0),
            event(2, "supervisor_alive", json!({"supervisor": "b"}), 4),
            event(3, "slots_full_started", json!({"supervisor": "a"}), 2),
            event(4, "supervisor_stopped", json!({"supervisor": "b"}), 6),
            event(
                5,
                "supervisor_stopped",
                json!({"supervisor": "a", "last_heartbeat_at": 3 * 60}),
                8,
            ),
            event(6, "supervisor_alive", json!({"supervisor": "a"}), 9),
            event(
                7,
                "supervisor_stopped",
                json!({"supervisor": "a", "last_heartbeat_at": 60}),
                10,
            ),
            event(8, "supervisor_alive", json!({"supervisor": "b"}), 11),
            event(9, "supervisor_stopped", json!({"supervisor": "c"}), 12),
            event(10, "supervisor_alive", json!({"supervisor": "c"}), 13),
            event(11, "supervisor_alive", json!({"supervisor": "d"}), 20),
            event(12, "supervisor_alive", json!({"supervisor": "d"}), 15),
            event(13, "supervisor_alive", json!({"other": "e"}), 16),
        ];
        events[8].created_at = "garbled".into();
        let lives = Lives::new(&events);
        for token in ["a", "b", "c", "d", "e", "none"] {
            for now in [0, 10, 20, 25, 26, 40, 1000].map(|minute| minute * MIN) {
                let scan = life_end_by_scan(&events, token, now);
                assert_eq!(lives.end(token, now), scan, "{token} {now}");
                assert_eq!(
                    supervisor_life_end(&events, token, now),
                    scan,
                    "{token} {now}"
                );
            }
            // The first time it ends at is the one it ends from.
            match lives.ends_from(token) {
                Some(i64::MIN) => assert!(lives.end(token, i64::MIN).is_some(), "{token}"),
                Some(at) => {
                    assert!(lives.end(token, at).is_some(), "{token}");
                    assert_eq!(lives.end(token, at - 1), None, "{token}");
                }
                None => assert_eq!(lives.end(token, i64::MAX), None, "{token}"),
            }
        }
        assert_eq!(lives.end("a", 0), Some(LifeEnd::Stopped { at_ms: 3 * MIN }));
        assert_eq!(lives.end("b", 0), Some(LifeEnd::Stopped { at_ms: 6 * MIN }));
        assert_eq!(
            lives.end("c", 1000 * MIN),
            Some(LifeEnd::Silent { at_ms: 13 * MIN })
        );
        assert_eq!(lives.end("d", 20 * MIN), None);
        assert_eq!(
            lives.end("d", 1000 * MIN),
            Some(LifeEnd::Silent { at_ms: 20 * MIN })
        );
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
