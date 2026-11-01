//! Main's history as Git gives it, for the plan review's hotspots
//! (ADR-0069): read since the earliest event when there is a conflict.
//! `stats` reads main's history from the record the supervisor keeps
//! ([`crate::domain::stats::main_log`]), never from Git.

use anyhow::Result;

use crate::domain::stats::{
    History,
    conflicts::{MainHistory, earliest_conflict},
    timestamp_millis,
};

/// The unix second main's history is read since for `conflict_hotspots`:
/// a second before the earliest event of `events`, so that the landings
/// of any window before its first conflict are counted too; `None`, none
/// is read, without a conflict.
pub fn history_since(events: &[crate::domain::RunEvent]) -> Option<i64> {
    let earliest = events
        .iter()
        .filter_map(|event| timestamp_millis(&event.created_at))
        .min();
    earliest_conflict(events)
        .and(earliest)
        .map(|millis| millis.div_euclid(1000) - 1)
}

/// Main's history since [`history_since`] of `events`, or why it could not
/// be read; none is read without a conflict.
pub fn conflict_history(
    events: &[crate::domain::RunEvent],
    read: &dyn Fn(i64) -> Result<MainHistory>,
) -> History {
    match history_since(events) {
        None => History::Read(MainHistory::default()),
        Some(since) => read_history(read, since),
    }
}

/// Main's history for the conflicts of a window whose span starts at
/// `start` (unix milliseconds): since the second before it, as
/// [`conflict_history`] of the window's events reads it, and none without
/// a conflict among `conflicts` (the window's conflict events).
pub fn window_conflict_history(
    conflicts: &[crate::domain::RunEvent],
    start: Option<i64>,
    read: &dyn Fn(i64) -> Result<MainHistory>,
) -> History {
    match earliest_conflict(conflicts).and(start) {
        None => History::Read(MainHistory::default()),
        Some(millis) => read_history(read, millis.div_euclid(1000) - 1),
    }
}

/// `read` of main's history since `since`, or why it could not be read.
fn read_history(read: &dyn Fn(i64) -> Result<MainHistory>, since: i64) -> History {
    match read(since) {
        Ok(history) => History::Read(history),
        Err(error) => History::Unavailable(format!("{error:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::stats::conflict_config;
    use crate::domain::stats::ConflictConfig;
    use crate::domain::{EventId, RunEvent, TaskId};
    use serde_json::json;

    fn event(id: i64, kind: &str, payload: serde_json::Value, at: &str) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: at.to_owned(),
            actor: None,
        }
    }

    /// The history is read from the earliest event, not the earliest
    /// conflict, and only when there is a conflict; a failure to read it
    /// is reported, not raised.
    #[test]
    fn conflict_history_reads_from_the_earliest_event() {
        let quiet = [event(
            1,
            "run_claimed",
            json!({}),
            "1970-01-01T00:01:40.000Z",
        )];
        let never = |_: i64| -> Result<MainHistory> { unreachable!() };
        assert_eq!(
            conflict_history(&quiet, &never),
            History::Read(MainHistory::default())
        );
        let events = [
            quiet[0].clone(),
            event(
                2,
                "conflict_precheck",
                json!({"main": "m", "conflicts": ["a"]}),
                "1970-01-01T00:03:20.000Z",
            ),
        ];
        let since = std::cell::Cell::new(0);
        let read = |at: i64| {
            since.set(at);
            Ok(MainHistory::default())
        };
        conflict_history(&events, &read);
        assert_eq!(since.get(), 99);
        let failing = |_: i64| -> Result<MainHistory> { anyhow::bail!("no git") };
        assert_eq!(
            conflict_history(&events, &failing),
            History::Unavailable("no git".into())
        );
        assert_eq!(conflict_config(None).source, "default");
        assert_eq!(
            conflict_config(Some(ConflictConfig::default())).source,
            "file"
        );
    }
}
