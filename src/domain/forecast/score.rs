//! Scoring the forecast snapshots (ADR-0070 decision 4): each snapshot's
//! p50 and p90 of a task that later completed or a goal later closed as
//! achieved, against when that happened. Derived from `run_events` when
//! read, nothing recorded: a sample is one (snapshot, target) pair, with
//! the change marks between the snapshot and the finish counted so the
//! error of the method can be read apart from the plan's changes. Pure.
use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::Value;

use super::snapshot::FORECAST_RECORDED;
use crate::domain::{
    EventId, GoalId, GoalVerdict, RunEvent, TaskId, event_kind,
    marks::{MARK_RETRACTED, Mark},
    stats::timestamp_millis,
};

/// The changes of one task's plan that count as a change mark between a
/// snapshot and a finish, when the snapshot forecast that task (the task
/// triggers of [`super::snapshot`]).
const TASK_MARK_KINDS: [&str; 5] = [
    event_kind::TASK_PRIORITY_CHANGED,
    event_kind::DEPENDENCY_ADDED,
    event_kind::DEPENDENCY_REMOVED,
    event_kind::GOAL_DEPENDENCY_ADDED,
    event_kind::GOAL_DEPENDENCY_REMOVED,
];

/// The bands of the remaining time a snapshot's p50 gave (its
/// `p50_secs`): the upper bound in seconds, and the label.
pub const BANDS: [(i64, &str); 5] = [
    (3600, "0-1h"),
    (6 * 3600, "1-6h"),
    (24 * 3600, "6-24h"),
    (3 * 24 * 3600, "1-3d"),
    (i64::MAX, "3d+"),
];

/// Which target a sample is of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Task(TaskId),
    Goal(GoalId),
}

impl Target {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Task(_) => "task",
            Self::Goal(_) => "goal",
        }
    }
}

/// One snapshot's forecast of one target that finished after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    pub target: Target,
    /// The `forecast_recorded` event.
    pub snapshot: EventId,
    pub method: i64,
    /// The snapshot's time and the finish's, unix ms.
    pub at_ms: i64,
    pub finished_ms: i64,
    /// Seconds from the snapshot's time to its p50 and p90.
    pub p50_secs: i64,
    pub p90_secs: Option<i64>,
    /// The change marks between the snapshot and the finish.
    pub marks: usize,
}

impl Sample {
    /// Seconds from the snapshot to the finish.
    pub const fn actual_secs(&self) -> i64 {
        (self.finished_ms - self.at_ms) / 1000
    }

    /// The actual minus the p50: positive when it finished later.
    pub const fn error_secs(&self) -> i64 {
        self.actual_secs() - self.p50_secs
    }

    /// The error over the remaining time the p50 gave; none for a p50 of
    /// now (0 seconds).
    #[allow(clippy::cast_precision_loss)]
    pub fn error_ratio(&self) -> Option<f64> {
        (self.p50_secs > 0).then(|| self.error_secs() as f64 / self.p50_secs as f64)
    }

    /// It finished at or before the p90; none without a p90.
    pub fn p90_hit(&self) -> Option<bool> {
        self.p90_secs.map(|p90| self.actual_secs() <= p90)
    }

    /// The band of the remaining time the p50 gave.
    pub fn band(&self) -> &'static str {
        BANDS
            .iter()
            .find(|(upper, _)| self.p50_secs < *upper)
            .map_or("3d+", |(_, label)| label)
    }
}

/// Why a snapshot's row of a finished target is not a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Excluded {
    /// The task was canceled: no actual.
    Canceled,
    /// The goal was closed as abandoned: no actual.
    Abandoned,
    /// It finished, but the snapshot gave it no p50 (it never finished in
    /// the simulation).
    Unforecast,
}

impl Excluded {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Canceled => "canceled",
            Self::Abandoned => "abandoned",
            Self::Unforecast => "unforecast",
        }
    }
}

/// Every sample, and the rows left out, each with the target and the time
/// it finished (unix ms), ascending by the snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scoring {
    pub samples: Vec<Sample>,
    pub excluded: Vec<(Target, i64, Excluded)>,
}

#[derive(Deserialize)]
struct Row {
    id: i64,
    #[serde(default)]
    p50_secs: Option<i64>,
    #[serde(default)]
    p90_secs: Option<i64>,
}

#[derive(Deserialize)]
struct Recorded {
    at_secs: i64,
    /// The trigger events the snapshot looked at: a mark through it was
    /// already in the forecast.
    #[serde(default)]
    triggers_through: i64,
    #[serde(default)]
    method: i64,
    #[serde(default)]
    tasks: Vec<Row>,
    #[serde(default)]
    goals: Vec<Row>,
}

/// How a target finished: the event, its time, and whether it counts.
#[derive(Clone, Copy)]
struct Finish {
    event: EventId,
    ms: i64,
    /// `None` for a finish with an actual; why not otherwise.
    excluded: Option<Excluded>,
}

/// The samples of every snapshot in `events` (all of the queue's,
/// ascending id); `marks` are the queue's change marks (`marks::marks`
/// over everything).
pub fn score(events: &[RunEvent], marks: &[Mark]) -> Scoring {
    let mut task_finishes: HashMap<TaskId, Vec<Finish>> = HashMap::new();
    let mut goal_finishes: HashMap<GoalId, Vec<Finish>> = HashMap::new();
    // The plan's changes on a task, (event, time, task), ascending id.
    let mut task_marks: Vec<(EventId, i64, TaskId)> = Vec::new();
    for event in events {
        let Some(ms) = timestamp_millis(&event.created_at) else {
            continue;
        };
        let finish = |excluded| Finish {
            event: event.id,
            ms,
            excluded,
        };
        match event.kind.as_str() {
            event_kind::TASK_STATUS_CHANGED => {
                let excluded = match event.payload.get("to").and_then(Value::as_str) {
                    Some("completed") => None,
                    Some("canceled") => Some(Excluded::Canceled),
                    _ => continue,
                };
                if let Some(task) = event.task_id {
                    task_finishes
                        .entry(task)
                        .or_default()
                        .push(finish(excluded));
                }
            }
            event_kind::GOAL_CLOSED => {
                let achieved = event.payload.get("verdict").and_then(Value::as_str)
                    == Some(GoalVerdict::Achieved.as_str());
                if let Some(goal) = event.goal_id {
                    goal_finishes
                        .entry(goal)
                        .or_default()
                        .push(finish((!achieved).then_some(Excluded::Abandoned)));
                }
            }
            kind if TASK_MARK_KINDS.contains(&kind) => {
                if let Some(task) = event.task_id {
                    task_marks.push((event.id, ms, task));
                }
            }
            _ => {}
        }
    }
    // The queue's marks that split the KPIs: not retracted, and not a
    // retraction; a derived one has no event of its own.
    let queue_marks: Vec<(Option<EventId>, i64)> = marks
        .iter()
        .filter(|mark| mark.retracted_by.is_none() && mark.kind != MARK_RETRACTED)
        .filter_map(|mark| Some((mark.id, timestamp_millis(&mark.at)?)))
        .collect();

    let mut scoring = Scoring::default();
    for event in events.iter().filter(|e| e.kind == FORECAST_RECORDED) {
        let Ok(recorded) = Recorded::deserialize(&event.payload) else {
            continue;
        };
        let at_ms = recorded.at_secs * 1000;
        // A mark is after the snapshot when the snapshot did not look at
        // it: past the trigger events it read (a trigger in the same
        // second as `at_secs` is not after it), or, for a derived mark or
        // a snapshot without that cursor, later than `at_secs`.
        let seen = EventId::new(recorded.triggers_through);
        let after = |id: Option<EventId>, ms: i64| match id {
            Some(id) if recorded.triggers_through > 0 => id > seen,
            _ => ms > at_ms,
        };
        let forecast: HashSet<TaskId> = recorded.tasks.iter().map(|r| TaskId::new(r.id)).collect();
        let mut later: Vec<i64> = queue_marks
            .iter()
            .filter(|(id, ms)| after(*id, *ms))
            .map(|(_, ms)| *ms)
            .chain(
                task_marks
                    .iter()
                    .filter(|(id, ms, task)| after(Some(*id), *ms) && forecast.contains(task))
                    .map(|(_, ms, _)| *ms),
            )
            .collect();
        later.sort_unstable();
        let rows = recorded
            .tasks
            .iter()
            .map(|row| (Target::Task(TaskId::new(row.id)), row))
            .chain(
                recorded
                    .goals
                    .iter()
                    .map(|row| (Target::Goal(GoalId::new(row.id)), row)),
            );
        for (target, row) in rows {
            let finishes = match target {
                Target::Task(task) => task_finishes.get(&task),
                Target::Goal(goal) => goal_finishes.get(&goal),
            };
            // The first finish after the snapshot.
            let Some(finish) = finishes
                .into_iter()
                .flatten()
                .find(|finish| finish.event > event.id)
            else {
                continue;
            };
            let excluded = finish
                .excluded
                .or_else(|| row.p50_secs.is_none().then_some(Excluded::Unforecast));
            match (excluded, row.p50_secs) {
                (None, Some(p50_secs)) => scoring.samples.push(Sample {
                    target,
                    snapshot: event.id,
                    method: recorded.method,
                    at_ms,
                    finished_ms: finish.ms,
                    p50_secs,
                    p90_secs: row.p90_secs,
                    marks: later.partition_point(|&ms| ms <= finish.ms),
                }),
                (excluded, _) => scoring.excluded.push((
                    target,
                    finish.ms,
                    excluded.unwrap_or(Excluded::Unforecast),
                )),
            }
        }
    }
    scoring
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::marks::{self, MARK_RECORDED};

    const T0: i64 = 1_790_000_000;
    const HOUR: i64 = 3600;

    #[derive(Default)]
    struct Events(Vec<RunEvent>);

    impl Events {
        fn push(
            &mut self,
            kind: &str,
            task: Option<i64>,
            goal: Option<i64>,
            payload: Value,
            secs: i64,
        ) {
            self.0.push(RunEvent {
                id: EventId::new(self.0.len() as i64 + 1),
                task_id: task.map(TaskId::new),
                goal_id: goal.map(GoalId::new),
                run_id: None,
                kind: kind.into(),
                payload,
                created_at: marks::utc_text(secs * 1000),
            });
        }

        fn snapshot(&mut self, secs: i64, tasks: Value, goals: Value) {
            self.push(
                FORECAST_RECORDED,
                None,
                None,
                json!({"at_secs": secs, "method": 1, "tasks": tasks, "goals": goals}),
                secs,
            );
        }

        fn task(&mut self, task: i64, to: &str, secs: i64) {
            self.push(
                event_kind::TASK_STATUS_CHANGED,
                Some(task),
                None,
                json!({"from": "in_progress", "to": to}),
                secs,
            );
        }

        fn score(&self) -> Scoring {
            score(&self.0, &marks::marks(&self.0, None, None))
        }
    }

    fn row(id: i64, p50: Option<i64>, p90: Option<i64>) -> Value {
        json!({"id": id, "p50_secs": p50, "p90_secs": p90})
    }

    /// Each snapshot of a finished target is a sample with the error of
    /// its p50 and whether its p90 held; a canceled task, an abandoned
    /// goal and a row with no p50 are left out and counted.
    #[test]
    fn every_snapshot_of_a_finished_target_is_a_sample() {
        let mut events = Events::default();
        events.snapshot(
            T0,
            json!([
                row(1, Some(2 * HOUR), Some(4 * HOUR)),
                row(2, Some(HOUR), Some(2 * HOUR)),
                row(3, None, None),
                row(4, Some(HOUR), None)
            ]),
            json!([
                row(10, Some(10 * HOUR), Some(20 * HOUR)),
                row(11, Some(HOUR), None)
            ]),
        );
        events.snapshot(
            T0 + HOUR,
            json!([row(1, Some(4 * HOUR), Some(5 * HOUR))]),
            json!([]),
        );
        // Task 1 finishes 3 hours after the first snapshot.
        events.task(1, "completed", T0 + 3 * HOUR);
        events.task(2, "canceled", T0 + 3 * HOUR);
        events.task(3, "completed", T0 + 3 * HOUR);
        // Task 4 is still open; goal 10 is achieved, 11 abandoned.
        events.push(
            event_kind::GOAL_CLOSED,
            None,
            Some(10),
            json!({"verdict": "achieved"}),
            T0 + 12 * HOUR,
        );
        events.push(
            event_kind::GOAL_CLOSED,
            None,
            Some(11),
            json!({"verdict": "abandoned"}),
            T0 + 12 * HOUR,
        );
        let scoring = events.score();
        let samples: Vec<(&str, i64, i64, Option<bool>, &str)> = scoring
            .samples
            .iter()
            .map(|s| {
                (
                    s.target.as_str(),
                    s.snapshot.as_i64(),
                    s.error_secs(),
                    s.p90_hit(),
                    s.band(),
                )
            })
            .collect();
        assert_eq!(
            samples,
            [
                ("task", 1, HOUR, Some(true), "1-6h"),
                ("goal", 1, 2 * HOUR, Some(true), "6-24h"),
                // 2 hours left, 4 given, 5 at p90.
                ("task", 2, -2 * HOUR, Some(true), "1-6h"),
            ]
        );
        assert_eq!(scoring.samples[0].error_ratio(), Some(0.5));
        assert_eq!(scoring.samples[2].error_ratio(), Some(-0.5));
        let excluded: Vec<(&str, &str)> = scoring
            .excluded
            .iter()
            .map(|(target, _, why)| (target.as_str(), why.as_str()))
            .collect();
        assert_eq!(
            excluded,
            [
                ("task", "canceled"),
                ("task", "unforecast"),
                ("goal", "abandoned")
            ]
        );
    }

    /// The marks between a snapshot and the finish are counted: the
    /// queue's change marks (not a retracted one), and a change of the
    /// plan of a task the snapshot forecast; a later p90 missed.
    #[test]
    fn the_marks_between_the_snapshot_and_the_finish_are_counted() {
        let mut events = Events::default();
        events.snapshot(
            T0,
            json!([
                row(1, Some(HOUR), Some(HOUR)),
                row(2, Some(HOUR), Some(HOUR))
            ]),
            json!([]),
        );
        events.push(
            MARK_RECORDED,
            None,
            None,
            json!({"label": "sccache"}),
            T0 + 60,
        );
        events.push(MARK_RECORDED, None, None, json!({"label": "oops"}), T0 + 70);
        events.push(MARK_RETRACTED, None, None, json!({"mark": 3}), T0 + 80);
        // A dependency of task 2 (forecast) and of task 9 (not).
        events.push(
            event_kind::DEPENDENCY_ADDED,
            Some(2),
            None,
            json!({}),
            T0 + 90,
        );
        events.push(
            event_kind::DEPENDENCY_ADDED,
            Some(9),
            None,
            json!({}),
            T0 + 90,
        );
        events.task(1, "completed", T0 + 2 * HOUR);
        events.push(
            event_kind::TASK_PRIORITY_CHANGED,
            Some(2),
            None,
            json!({}),
            T0 + 3 * HOUR,
        );
        events.task(2, "completed", T0 + 3 * HOUR);
        // A mark after both finishes counts for neither.
        events.push(
            MARK_RECORDED,
            None,
            None,
            json!({"label": "late"}),
            T0 + 4 * HOUR,
        );
        // A snapshot a mark in its second triggered (it read through it)
        // does not count that mark.
        let start = T0 + 5 * HOUR;
        events.push(MARK_RECORDED, None, None, json!({"label": "start"}), start);
        let through = events.0.len() as i64;
        events.push(
            FORECAST_RECORDED,
            None,
            None,
            json!({"at_secs": start - 1, "triggers_through": through, "method": 1, "tasks": [row(5, Some(HOUR), Some(HOUR))], "goals": []}),
            start,
        );
        events.task(5, "completed", start + HOUR);
        let scoring = events.score();
        let counted: Vec<(i64, usize, Option<bool>)> = scoring
            .samples
            .iter()
            .map(|s| (s.error_secs(), s.marks, s.p90_hit()))
            .collect();
        assert_eq!(
            counted,
            [
                (HOUR, 2, Some(false)),
                (2 * HOUR, 3, Some(false)),
                (1, 0, Some(false))
            ]
        );
    }

    #[test]
    fn bands_and_a_p50_of_now() {
        let sample = |p50_secs| Sample {
            target: Target::Task(TaskId::new(1)),
            snapshot: EventId::new(1),
            method: 1,
            at_ms: 0,
            finished_ms: 60_000,
            p50_secs,
            p90_secs: None,
            marks: 0,
        };
        let bands: Vec<&str> = [0, 3599, 3600, 6 * HOUR, 24 * HOUR, 3 * 24 * HOUR]
            .into_iter()
            .map(|p50| sample(p50).band())
            .collect();
        assert_eq!(bands, ["0-1h", "0-1h", "1-6h", "6-24h", "1-3d", "3d+"]);
        assert_eq!(sample(0).error_ratio(), None);
        assert_eq!(sample(0).p90_hit(), None);
        assert_eq!(sample(0).error_secs(), 60);
    }
}
