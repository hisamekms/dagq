//! What the forecast draws from (ADR-0070 decision 1), derived from the run
//! events: the landed runs' intervals as `stats` measures them, the delay
//! from a goal's last landing to its close as achieved, the people's
//! answers to asks, and where a run in flight is.

use std::collections::HashMap;

use serde_json::Value;

use super::{Phase, Running};
use crate::domain::{
    EventId, GoalId, GoalVerdict, RunEvent, TaskId, TaskKind,
    event_kind::{GOAL_CLOSED, RUN_INTEGRATED},
    stats::{self, LiveSnapshot, SlotSnapshot, StatsQuery, timestamp_millis},
    waiting::WaitState,
};

/// One landed run's intervals in seconds, drawn together so that a slow
/// `work` and its long `wait_to_land` stay one run. Its resumes and the
/// deferrals of its landing are inside them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub work: i64,
    pub validate: i64,
    pub wait_to_land: i64,
}

impl Sample {
    pub fn total(&self) -> i64 {
        self.work + self.validate + self.wait_to_land
    }

    /// The length of `phase`.
    pub fn phase(&self, phase: Phase) -> i64 {
        match phase {
            Phase::Work => self.work,
            Phase::Validate => self.validate,
            Phase::WaitToLand => self.wait_to_land,
        }
    }

    /// `phase` and the phases after it.
    pub fn from(&self, phase: Phase) -> i64 {
        match phase {
            Phase::Work => self.total(),
            Phase::Validate => self.validate + self.wait_to_land,
            Phase::WaitToLand => self.wait_to_land,
        }
    }
}

/// The distributions the forecast draws from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    /// The landed runs with all three intervals, and their task's kind.
    pub runs: Vec<(Option<TaskKind>, Sample)>,
    /// Seconds from a goal's last landing to its close as achieved.
    pub close_delays: Vec<i64>,
    /// Seconds from an ask's opening to a person's answer.
    pub ask_waits: Vec<i64>,
    /// Per goal, the unix milliseconds its latest task landed.
    pub last_landings: HashMap<GoalId, i64>,
}

/// The history of `events` (every run event, oldest first).
pub fn history(
    events: &[RunEvent],
    goals: &HashMap<TaskId, Option<GoalId>>,
    kinds: &HashMap<TaskId, Option<TaskKind>>,
    now: i64,
) -> History {
    let derived = stats::stats(
        events,
        goals,
        now,
        SlotSnapshot::default(),
        &StatsQuery {
            full: true,
            ..StatsQuery::default()
        },
        &LiveSnapshot::default(),
    );
    let runs = derived
        .runs
        .iter()
        .filter(|run| run.status.as_deref() == Some("integrated"))
        .filter_map(|run| {
            let sample = Sample {
                work: run.work?,
                validate: run.validate?,
                wait_to_land: run.wait_to_land?,
            };
            Some((kinds.get(&run.task_id).copied().flatten(), sample))
        })
        .collect();
    let mut last_landings: HashMap<GoalId, i64> = HashMap::new();
    let mut close_delays = Vec::new();
    for event in events {
        let Some(at) = timestamp_millis(&event.created_at) else {
            continue;
        };
        match event.kind.as_str() {
            RUN_INTEGRATED => {
                if let Some(goal) = event.task_id.and_then(|t| goals.get(&t).copied().flatten()) {
                    last_landings.insert(goal, at);
                }
            }
            GOAL_CLOSED
                if event.payload.get("verdict").and_then(Value::as_str)
                    == Some(GoalVerdict::Achieved.as_str()) =>
            {
                if let Some(landed) = event.goal_id.and_then(|g| last_landings.get(&g)) {
                    close_delays.push((at - landed).max(0) / 1000);
                }
            }
            _ => {}
        }
    }
    let latest = events.iter().map(|e| e.id).max();
    let ask_waits = latest.map_or_else(Vec::new, |latest| {
        stats::asks::human_waits(events, EventId::new(0), latest, |_| true).0
    });
    History {
        runs,
        close_delays,
        ask_waits,
        last_landings,
    }
}

/// Where the run of `events` (its own, oldest first) is at unix
/// milliseconds `now_ms`: the phase `stats` would put it in and how long
/// it has been there, and how long it has waited for a person while it
/// waits.
pub fn running(events: &[RunEvent], now_ms: i64) -> Running {
    let first = |kind: &str| {
        events
            .iter()
            .find(|event| event.kind == kind)
            .and_then(|event| timestamp_millis(&event.created_at))
    };
    let secs = |since: Option<i64>| since.map_or(0, |since| (now_ms - since).max(0) / 1000);
    let (phase, since) = match (
        first("validation_finished"),
        first("receipt_observed"),
        first("run_claimed"),
    ) {
        (Some(validated), _, _) => (Phase::WaitToLand, Some(validated)),
        (None, Some(receipt), _) => (Phase::Validate, Some(receipt)),
        (None, None, claimed) => (Phase::Work, claimed),
    };
    let waiting = WaitState::of(events)
        .filter(|wait| wait.ended.is_none())
        .map(|wait| (now_ms - wait.since_ms).max(0) / 1000);
    Running {
        phase,
        elapsed: secs(since),
        waiting,
    }
}
