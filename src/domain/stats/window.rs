//! The default window of [`super::stats`] (no `--since`, `--until`,
//! `--goal` or `--full`) as a reader finds it without every event: from
//! just before the first event of the [`super::DEFAULT_RUNS`] runs that
//! ended last to the latest event. A store reads it by a range of its own
//! ([`crate::application::EventStore::stats_window`]); [`default_window`]
//! is the same rule over a list of events, which a store without one
//! derives it with and the tests compare a store's against.
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{DEFAULT_RUNS, payload_status};
use crate::domain::{EventId, EventKind, RunEvent, RunId};

/// The default window of `stats`: the events with `after < id <= upto`,
/// whose times run from `start_ms` to `end_ms` (unix milliseconds, none
/// for a window with no event).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsWindow {
    pub after: EventId,
    pub upto: EventId,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
}

impl StatsWindow {
    /// The window's span in time, as [`super::conflicts::window_span`]
    /// gives it.
    pub fn span(&self) -> (Option<i64>, Option<i64>) {
        (self.start_ms, self.end_ms)
    }
}

/// Whether `event` ends its run for `stats`: an event of a run's task that
/// lands it, closes it satisfied or gives it the status `failed` or
/// `interrupted`. A run's first such event is its `finished_event_id`.
pub fn ends_run(event: &RunEvent) -> bool {
    event.run_id.is_some()
        && event.task_id.is_some()
        && (matches!(
            event.kind.known(),
            Some(EventKind::RunIntegrated | EventKind::RunSatisfiedClosed)
        ) || matches!(
            payload_status(&event.payload),
            Some("failed" | "interrupted")
        ))
}

/// The ended runs that pick the window: of `ended` (each run with the id
/// of the first event that ended it), the [`DEFAULT_RUNS`] that ended
/// last, latest first.
pub fn latest_ended(mut ended: Vec<(RunId, EventId)>) -> Vec<(RunId, EventId)> {
    ended.sort_by_key(|(_, at)| std::cmp::Reverse(*at));
    ended.truncate(DEFAULT_RUNS);
    ended
}

/// Where a window whose earliest run starts at `first` (none without an
/// ended run) starts: just before that event, else at the start.
pub fn window_after(first: Option<EventId>) -> EventId {
    first.map_or(EventId::new(0), |id| EventId::new(id.as_i64() - 1))
}

/// The default window of `events` (every event, ascending id).
pub fn default_window(events: &[RunEvent]) -> StatsWindow {
    let mut first: HashMap<&RunId, EventId> = HashMap::new();
    let mut ended: HashMap<&RunId, EventId> = HashMap::new();
    for event in events {
        let Some(run_id) = &event.run_id else {
            continue;
        };
        first.entry(run_id).or_insert(event.id);
        if ends_run(event) {
            ended.entry(run_id).or_insert(event.id);
        }
    }
    let latest = latest_ended(
        ended
            .into_iter()
            .map(|(run_id, at)| (run_id.clone(), at))
            .collect(),
    );
    let after = window_after(
        latest
            .iter()
            .filter_map(|(run, _)| first.get(run))
            .copied()
            .min(),
    );
    let upto = events
        .iter()
        .map(|event| event.id)
        .max()
        .unwrap_or(EventId::new(0));
    let (start_ms, end_ms) = super::conflicts::window_span(events, after, upto);
    StatsWindow {
        after,
        upto,
        start_ms,
        end_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskId;
    use crate::domain::stats::tests::at;
    use serde_json::{Value, json};

    fn run(n: usize) -> RunId {
        RunId::new(format!("{n:08x}-1111-4111-8111-111111111111")).unwrap()
    }

    fn event(id: i64, run_n: Option<usize>, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: run_n.map(|n| TaskId::new(i64::try_from(n).unwrap() + 1)),
            goal_id: None,
            run_id: run_n.map(run),
            kind: kind.into(),
            payload,
            created_at: at(1_800_000_000 + id),
            actor: None,
        }
    }

    #[test]
    fn ends_run_is_where_stats_finishes_a_run() {
        let events = vec![
            event(1, Some(1), "run_claimed", json!({})),
            event(
                2,
                Some(1),
                "validation_finished",
                json!({"status": "failed"}),
            ),
            event(3, Some(1), "run_integrated", json!({})),
            event(4, Some(2), "run_claimed", json!({})),
            event(
                5,
                Some(2),
                "resume_finished",
                json!({"status": "interrupted"}),
            ),
            event(6, Some(3), "run_claimed", json!({})),
            event(7, Some(3), "run_satisfied_closed", json!({})),
            event(8, Some(4), "run_claimed", json!({"status": "running"})),
            event(9, None, "run_integrated", json!({})),
        ];
        let finished: Vec<Option<EventId>> = super::super::run_stats(&events)
            .iter()
            .map(|run| run.finished_event_id)
            .collect();
        let first_end = |n: usize| {
            events
                .iter()
                .find(|e| e.run_id == Some(run(n)) && ends_run(e))
                .map(|e| e.id)
        };
        assert_eq!(
            finished,
            vec![first_end(1), first_end(2), first_end(3), first_end(4)]
        );
        assert_eq!(finished[0], Some(EventId::new(2)));
        assert_eq!(finished[3], None);
    }

    #[test]
    fn the_window_starts_before_the_first_event_of_the_runs_that_ended_last() {
        let mut events = Vec::new();
        let mut id = 0;
        let mut push = |run_n: Option<usize>, kind: &str, payload: Value| {
            id += 1;
            events.push(event(id, run_n, kind, payload));
        };
        // Run 0 starts early and ends early, then ends again late: its
        // first end counts.
        push(Some(0), "run_claimed", json!({}));
        push(Some(0), "run_failed", json!({"status": "failed"}));
        for n in 1..=DEFAULT_RUNS + 5 {
            push(Some(n), "run_claimed", json!({}));
            push(None, "host_sampled", json!({}));
            push(Some(n), "run_integrated", json!({}));
        }
        push(Some(0), "run_integrated", json!({}));
        let window = default_window(&events);
        // Runs 6..=55 are the latest; run 6's first event is 2 + 5 * 3 + 1.
        assert_eq!(window.after, EventId::new(17));
        assert_eq!(window.upto, EventId::new(id));
        assert_eq!(window.start_ms, Some((1_800_000_000 + 18) * 1000));
        assert_eq!(window.end_ms, Some((1_800_000_000 + id) * 1000));
        assert_eq!(default_window(&[]), StatsWindow::default());
    }
}
