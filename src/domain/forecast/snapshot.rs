//! Forecast snapshots (ADR-0070 decision 3): what makes the supervisor
//! record the forecast of every open task and goal as one
//! `forecast_recorded` event, and whether a landing moved it enough to be
//! recorded. Pure: the application reads the trigger events and the latest
//! snapshot, and writes the event.

use serde::{Deserialize, Serialize};

use super::Forecast;
use crate::domain::{
    RunEvent, RunId, TaskId, event_kind,
    marks::{
        MARK_RECORDED, MARK_RETRACTED, RUN_ENV_CHANGED, SUPERVISOR_STARTED, SUPERVISOR_STOPPED,
    },
};

/// The event of one snapshot. A KPI bookkeeping event (ADR-0051 decision
/// 24): it never wakes the observer.
pub const FORECAST_RECORDED: &str = crate::domain::event_kind::EventKind::ForecastRecorded.as_str();
/// A landing is recorded when a task's or goal's p50 moved by at least
/// this share of the remaining time the previous snapshot gave it ...
pub const MOVE_RATIO: f64 = 0.2;
/// ... and by at least this many seconds (ADR-0070's first values).
pub const MOVE_MIN_SECS: i64 = 1800;

/// The changes of the plan on one task: a trigger only while that task is
/// forecast (`ready` or `in_progress`), so registering a draft is none.
const TASK_MARK_KINDS: [&str; 5] = [
    event_kind::TASK_PRIORITY_CHANGED,
    event_kind::DEPENDENCY_ADDED,
    event_kind::DEPENDENCY_REMOVED,
    event_kind::GOAL_DEPENDENCY_ADDED,
    event_kind::GOAL_DEPENDENCY_REMOVED,
];
/// The recorded change marks (ADR-0051 decisions 10-12); a start or a
/// handoff also carries a change of `parallel`, which is a derived mark.
const QUEUE_MARK_KINDS: [&str; 5] = [
    SUPERVISOR_STARTED,
    SUPERVISOR_STOPPED,
    RUN_ENV_CHANGED,
    MARK_RECORDED,
    MARK_RETRACTED,
];

/// The kinds of the events [`trigger`] may take for one.
pub const TRIGGER_KINDS: [&str; 13] = [
    event_kind::PLAN_REVIEW_FINISHED,
    event_kind::PLAN_DECIDED,
    event_kind::RUN_INTEGRATED,
    SUPERVISOR_STARTED,
    SUPERVISOR_STOPPED,
    RUN_ENV_CHANGED,
    MARK_RECORDED,
    MARK_RETRACTED,
    event_kind::TASK_PRIORITY_CHANGED,
    event_kind::DEPENDENCY_ADDED,
    event_kind::DEPENDENCY_REMOVED,
    event_kind::GOAL_DEPENDENCY_ADDED,
    event_kind::GOAL_DEPENDENCY_REMOVED,
];

/// Why a snapshot was taken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "trigger", rename_all = "snake_case")]
pub enum Trigger {
    /// (a) A proposal's plan was settled: plan review passed it, or a
    /// person answered its concern `ready`.
    PlanReview {
        event_id: i64,
        proposal_id: Option<i64>,
    },
    /// (b) A change mark, or a change of an open task's priority or
    /// dependencies (`task_id`).
    Mark {
        event_id: i64,
        kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        task_id: Option<TaskId>,
    },
    /// (c) A run landed.
    Landing {
        event_id: i64,
        task_id: Option<TaskId>,
        run_id: Option<RunId>,
    },
    /// (d) No snapshot yet on this local day (`YYYY-MM-DD`).
    Daily { day: String },
}

/// The trigger `event` is, if any.
pub fn trigger(event: &RunEvent) -> Option<Trigger> {
    let event_id = event.id.as_i64();
    let proposal_id = || event.payload.get("proposal_id").and_then(|p| p.as_i64());
    let kind = event.kind.as_str();
    match kind {
        event_kind::PLAN_REVIEW_FINISHED
            if event.payload.get("decision").and_then(|d| d.as_str()) == Some("pass") =>
        {
            Some(Trigger::PlanReview {
                event_id,
                proposal_id: proposal_id(),
            })
        }
        event_kind::PLAN_DECIDED
            if event.payload.get("status").and_then(|s| s.as_str()) == Some("accepted") =>
        {
            Some(Trigger::PlanReview {
                event_id,
                proposal_id: proposal_id(),
            })
        }
        event_kind::RUN_INTEGRATED => Some(Trigger::Landing {
            event_id,
            task_id: event.task_id,
            run_id: event.run_id.clone(),
        }),
        _ if QUEUE_MARK_KINDS.contains(&kind) => Some(Trigger::Mark {
            event_id,
            kind: kind.to_owned(),
            task_id: None,
        }),
        _ if TASK_MARK_KINDS.contains(&kind) && event.task_id.is_some() => Some(Trigger::Mark {
            event_id,
            kind: kind.to_owned(),
            task_id: event.task_id,
        }),
        _ => None,
    }
}

/// What a snapshot keeps of each task or goal to judge the next landing.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Row {
    pub id: i64,
    pub p50_secs: Option<i64>,
}

/// The latest snapshot, as the next one reads it back.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Previous {
    /// The unix second it was computed at.
    pub at_secs: i64,
    /// The trigger events it looked at, through this event ID.
    #[serde(default)]
    pub triggers_through: i64,
    #[serde(default)]
    pub tasks: Vec<Row>,
    #[serde(default)]
    pub goals: Vec<Row>,
}

impl Previous {
    /// The snapshot `payload` records, `None` when it does not read as one.
    pub fn read(payload: &serde_json::Value) -> Option<Self> {
        serde_json::from_value(payload.clone()).ok()
    }
}

/// A task or goal whose p50 moved past the thresholds since the previous
/// snapshot: seconds are from each snapshot's time to its p50, and `shift`
/// is between the two p50s; null where one never finishes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Moved {
    /// `task` or `goal`.
    pub target: &'static str,
    pub id: i64,
    pub previous_p50_secs: Option<i64>,
    pub p50_secs: Option<i64>,
    pub shift_secs: Option<i64>,
}

/// The tasks and goals whose p50 at `now` moved since `previous`: by at
/// least [`MOVE_MIN_SECS`] and [`MOVE_RATIO`] of the remaining time
/// `previous` gave it, from a time to none or back, or not in `previous`.
/// One `previous` no longer holds (it finished) did not move.
pub fn moved(previous: Option<&Previous>, forecast: &Forecast, now: i64) -> Vec<Moved> {
    let current = forecast
        .tasks
        .iter()
        .map(|task| ("task", task.id.as_i64(), task.at.p50_secs))
        .chain(
            forecast
                .goals
                .iter()
                .map(|goal| ("goal", goal.id.as_i64(), goal.at.p50_secs)),
        );
    let mut moved = Vec::new();
    for (target, id, p50_secs) in current {
        let before = previous.and_then(|previous| {
            let rows = if target == "task" {
                &previous.tasks
            } else {
                &previous.goals
            };
            rows.iter()
                .find(|row| row.id == id)
                .map(|row| (previous.at_secs, row.p50_secs))
        });
        let (previous_p50_secs, shift_secs, is_moved) = match before {
            None => (None, None, true),
            Some((at, before)) => match (before, p50_secs) {
                (None, None) => (None, None, false),
                (Some(before), Some(after)) => {
                    let shift = (now + after - (at + before)).abs();
                    let remaining = before.max(0) as f64;
                    let is_moved = shift >= MOVE_MIN_SECS && shift as f64 >= MOVE_RATIO * remaining;
                    (Some(before), Some(shift), is_moved)
                }
                (before, _) => (before, None, true),
            },
        };
        if is_moved {
            moved.push(Moved {
                target,
                id,
                previous_p50_secs,
                p50_secs,
                shift_secs,
            });
        }
    }
    moved
}

/// What a snapshot at `now` records of `triggers`, or `None` when none
/// holds: a change of a task's priority or dependencies only while the
/// task is forecast, and the landings only when some p50 [`moved`] (then
/// all of them, with what moved). The other triggers always hold.
pub fn decide(
    triggers: Vec<Trigger>,
    forecast: &Forecast,
    previous: Option<&Previous>,
    now: i64,
) -> Option<(Vec<Trigger>, Vec<Moved>)> {
    let forecast_task = |id: TaskId| forecast.tasks.iter().any(|task| task.id == id);
    let landed = triggers
        .iter()
        .any(|trigger| matches!(trigger, Trigger::Landing { .. }));
    let moved = if landed {
        moved(previous, forecast, now)
    } else {
        Vec::new()
    };
    let kept: Vec<Trigger> = triggers
        .into_iter()
        .filter(|trigger| match trigger {
            Trigger::Mark {
                task_id: Some(task),
                ..
            } => forecast_task(*task),
            Trigger::Landing { .. } => !moved.is_empty(),
            _ => true,
        })
        .collect();
    (!kept.is_empty()).then_some((kept, moved))
}

/// The payload of `forecast_recorded`: the forecast with who took it, when
/// (`at_secs`), why, and through which events it looked for triggers
/// (`triggers_through`) and read the history (`events_through`).
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot<'a> {
    pub supervisor: &'a str,
    pub at_secs: i64,
    pub triggers: &'a [Trigger],
    pub triggers_through: i64,
    pub events_through: i64,
    #[serde(skip_serializing_if = "<[Moved]>::is_empty")]
    pub moved: &'a [Moved],
    #[serde(flatten)]
    pub forecast: &'a Forecast,
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::domain::{
        EventId, GoalId,
        forecast::{Assumptions, GoalForecast, METHOD, Percentiles, Samples, TaskForecast},
    };

    const NOW: i64 = 1_790_000_000;

    fn at(p50_secs: Option<i64>) -> Percentiles {
        Percentiles {
            p50: None,
            p90: None,
            p50_secs,
            p90_secs: p50_secs,
            reason: p50_secs.is_none().then_some("no_samples"),
        }
    }

    fn forecast(tasks: &[(i64, Option<i64>)], goals: &[(i64, Option<i64>)]) -> Forecast {
        Forecast {
            method: METHOD,
            at: String::new(),
            seed: 0,
            trials: 1,
            assumptions: Assumptions {
                parallel: 1,
                min_samples: 1,
                samples: Samples {
                    all: 0,
                    changes: Default::default(),
                    close_delay: 0,
                    ask_wait: 0,
                },
                substituted: Vec::new(),
                left_out: Vec::new(),
            },
            tasks: tasks
                .iter()
                .map(|&(id, p50)| TaskForecast {
                    id: TaskId::new(id),
                    goal_id: None,
                    change: None,
                    phase: None,
                    waiting: false,
                    distribution: "all".into(),
                    at: at(p50),
                })
                .collect(),
            goals: goals
                .iter()
                .map(|&(id, p50)| GoalForecast {
                    id: GoalId::new(id),
                    open_tasks: 0,
                    unplanned_tasks: 0,
                    at: at(p50),
                })
                .collect(),
        }
    }

    fn previous(
        at_secs: i64,
        tasks: &[(i64, Option<i64>)],
        goals: &[(i64, Option<i64>)],
    ) -> Previous {
        let rows = |rows: &[(i64, Option<i64>)]| {
            rows.iter()
                .map(|&(id, p50_secs)| Row { id, p50_secs })
                .collect()
        };
        Previous {
            at_secs,
            triggers_through: 0,
            tasks: rows(tasks),
            goals: rows(goals),
        }
    }

    fn event(id: i64, kind: &str, task: Option<i64>, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: task.map(TaskId::new),
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    fn landing() -> Trigger {
        Trigger::Landing {
            event_id: 9,
            task_id: Some(TaskId::new(1)),
            run_id: None,
        }
    }

    #[test]
    fn the_events_that_are_triggers() {
        let pass = event(
            1,
            event_kind::PLAN_REVIEW_FINISHED,
            Some(2),
            json!({"decision": "pass", "proposal_id": 4}),
        );
        assert_eq!(
            trigger(&pass),
            Some(Trigger::PlanReview {
                event_id: 1,
                proposal_id: Some(4)
            })
        );
        let revise = event(
            2,
            event_kind::PLAN_REVIEW_FINISHED,
            Some(2),
            json!({"decision": "revise"}),
        );
        assert_eq!(trigger(&revise), None);
        let ready = event(
            3,
            event_kind::PLAN_DECIDED,
            Some(2),
            json!({"status": "accepted", "proposal_id": 4}),
        );
        assert!(matches!(
            trigger(&ready),
            Some(Trigger::PlanReview { event_id: 3, .. })
        ));
        let canceled = event(
            4,
            event_kind::PLAN_DECIDED,
            Some(2),
            json!({"status": "canceled"}),
        );
        assert_eq!(trigger(&canceled), None);
        let started = event(5, SUPERVISOR_STARTED, None, json!({}));
        assert_eq!(
            trigger(&started),
            Some(Trigger::Mark {
                event_id: 5,
                kind: SUPERVISOR_STARTED.into(),
                task_id: None
            })
        );
        let priority = event(6, event_kind::TASK_PRIORITY_CHANGED, Some(7), json!({}));
        assert_eq!(
            trigger(&priority),
            Some(Trigger::Mark {
                event_id: 6,
                kind: event_kind::TASK_PRIORITY_CHANGED.into(),
                task_id: Some(TaskId::new(7)),
            })
        );
        let landed = event(8, event_kind::RUN_INTEGRATED, Some(7), json!({}));
        assert_eq!(
            trigger(&landed),
            Some(Trigger::Landing {
                event_id: 8,
                task_id: Some(TaskId::new(7)),
                run_id: None
            })
        );
        assert_eq!(
            trigger(&event(9, event_kind::TASK_CREATED, Some(7), json!({}))),
            None
        );
        for kind in QUEUE_MARK_KINDS.iter().chain(&TASK_MARK_KINDS) {
            assert!(TRIGGER_KINDS.contains(kind), "{kind}");
        }
        assert_eq!(
            serde_json::to_value(Trigger::Daily {
                day: "2026-09-27".into()
            })
            .unwrap(),
            json!({"trigger": "daily", "day": "2026-09-27"})
        );
    }

    #[test]
    fn a_landing_holds_only_when_a_p50_moved_past_both_thresholds() {
        // 10 hours left an hour ago: 20% is 2 hours.
        let before = previous(NOW - 3600, &[(1, Some(36_000))], &[]);
        // Now 9 hours left: on time, nothing moved.
        let steady = forecast(&[(1, Some(32_400))], &[]);
        assert_eq!(decide(vec![landing()], &steady, Some(&before), NOW), None);
        // 1.5 hours later than before: past 30 minutes, not 20%.
        let late = forecast(&[(1, Some(32_400 + 5400))], &[]);
        assert_eq!(decide(vec![landing()], &late, Some(&before), NOW), None);
        // 2 hours earlier: both.
        let early = forecast(&[(1, Some(32_400 - 7200))], &[]);
        let (triggers, moved) = decide(vec![landing()], &early, Some(&before), NOW).unwrap();
        assert_eq!(triggers, [landing()]);
        assert_eq!(
            moved,
            [Moved {
                target: "task",
                id: 1,
                previous_p50_secs: Some(36_000),
                p50_secs: Some(25_200),
                shift_secs: Some(7200)
            }]
        );
        // A short remainder: 20% of 10 minutes is small, 30 minutes is not.
        let short = previous(NOW, &[], &[(3, Some(600))]);
        assert_eq!(
            decide(
                vec![landing()],
                &forecast(&[], &[(3, Some(1800))]),
                Some(&short),
                NOW
            ),
            None
        );
        assert!(
            decide(
                vec![landing()],
                &forecast(&[], &[(3, Some(2400))]),
                Some(&short),
                NOW
            )
            .is_some()
        );
    }

    #[test]
    fn a_forecast_gained_lost_or_new_moved_and_a_finished_one_did_not() {
        let before = previous(
            NOW,
            &[(1, None), (2, Some(100)), (3, Some(100)), (4, None)],
            &[],
        );
        let after = forecast(&[(1, Some(100)), (2, None), (4, None), (5, Some(100))], &[]);
        let moved: Vec<(i64, Option<i64>)> = super::moved(Some(&before), &after, NOW)
            .into_iter()
            .map(|moved| (moved.id, moved.previous_p50_secs))
            .collect();
        // 1 gained a time, 2 lost it, 5 is new; 3 finished and 4 still
        // has none.
        assert_eq!(moved, [(1, None), (2, Some(100)), (5, None)]);
        // Without a snapshot, everything is new.
        assert_eq!(super::moved(None, &after, NOW).len(), 4);
    }

    #[test]
    fn the_other_triggers_hold_and_a_change_counts_only_on_a_forecast_task() {
        let now = forecast(&[(1, Some(100))], &[]);
        let mark = |task: i64| Trigger::Mark {
            event_id: task,
            kind: event_kind::DEPENDENCY_ADDED.into(),
            task_id: Some(TaskId::new(task)),
        };
        // A draft's dependency is no trigger.
        assert_eq!(decide(vec![mark(8)], &now, None, NOW), None);
        let (triggers, moved) = decide(vec![mark(1), mark(8)], &now, None, NOW).unwrap();
        assert_eq!(triggers, [mark(1)]);
        assert!(moved.is_empty());
        let daily = Trigger::Daily {
            day: "2026-09-27".into(),
        };
        let same = previous(NOW, &[(1, Some(100))], &[]);
        // A landing that moved nothing rides along with the daily one only
        // when it moved something.
        let (triggers, moved) =
            decide(vec![landing(), daily.clone()], &now, Some(&same), NOW).unwrap();
        assert_eq!(triggers, [daily]);
        assert!(moved.is_empty());
    }

    #[test]
    fn a_snapshot_reads_back_as_the_previous_one() {
        let now = forecast(&[(1, Some(100))], &[(2, None)]);
        let triggers = [Trigger::Daily {
            day: "2026-09-27".into(),
        }];
        let payload = serde_json::to_value(Snapshot {
            supervisor: "s",
            at_secs: NOW,
            triggers: &triggers,
            triggers_through: 42,
            events_through: 43,
            moved: &[],
            forecast: &now,
        })
        .unwrap();
        assert_eq!(payload["method"], METHOD);
        assert_eq!(payload["triggers"][0]["trigger"], "daily");
        assert!(payload.get("moved").is_none());
        assert_eq!(
            Previous::read(&payload),
            Some(previous(NOW, &[(1, Some(100))], &[(2, None)])).map(|previous| Previous {
                triggers_through: 42,
                ..previous
            })
        );
        assert_eq!(Previous::read(&json!({"tasks": 3})), None);
    }
}
