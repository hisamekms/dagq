//! What the forecast draws from (ADR-0070 decision 1): the landed runs'
//! `work`, `validate` and `wait_to_land` as the ledger's one read gives
//! them ([`sample`]), the delay from a goal's last landing to its close as
//! achieved and the people's answers to asks, from those events alone,
//! and where a run in flight is, from the fold of its events ([`running`]).

use crate::domain::EventKind;
use std::collections::HashMap;

use serde_json::Value;

use super::{Running, Stage};
use crate::domain::{
    EventId, GoalId, GoalVerdict, RunEvent, RunStatus, TaskChange, TaskId,
    ledger::{
        fold::fold_run,
        read::{LedgerValues, validation},
    },
    run_phase::Blocker,
    stats::{self, timestamp_millis},
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

    /// The length of `stage`.
    pub fn stage(&self, stage: Stage) -> i64 {
        match stage {
            Stage::Work => self.work,
            Stage::Validate => self.validate,
            Stage::WaitToLand => self.wait_to_land,
        }
    }

    /// `stage` and the stages after it.
    pub fn from(&self, stage: Stage) -> i64 {
        match stage {
            Stage::Work => self.total(),
            Stage::Validate => self.validate + self.wait_to_land,
            Stage::WaitToLand => self.wait_to_land,
        }
    }
}

/// The distributions the forecast draws from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    /// The landed runs with all three intervals, and their task's change
    /// (ADR-t980-1).
    pub runs: Vec<(Option<TaskChange>, Sample)>,
    /// Seconds from a goal's last landing to its close as achieved.
    pub close_delays: Vec<i64>,
    /// Seconds from an ask's opening to a person's answer.
    pub ask_waits: Vec<i64>,
    /// Per goal, the unix milliseconds its latest task landed.
    pub last_landings: HashMap<GoalId, i64>,
}

/// The sample of a run row as the ledger's one read gives it
/// ([`crate::domain::ledger::read::read`]): a landed run with all three
/// values, new or legacy alike.
pub fn sample(values: &LedgerValues) -> Option<Sample> {
    values.landed.then_some(())?;
    Some(Sample {
        work: values.work?,
        validate: values.validate?,
        wait_to_land: values.wait_to_land?,
    })
}

/// The history of the landed runs' samples (`runs`, with their task's
/// change), the `run_integrated` and `goal_closed` events (`landings`,
/// oldest first) and the `ask_opened` and `ask_answered` events (`asks`,
/// oldest first).
pub fn history(
    runs: Vec<(Option<TaskChange>, Sample)>,
    landings: &[RunEvent],
    asks: &[RunEvent],
    goals: &HashMap<TaskId, Option<GoalId>>,
) -> History {
    let mut last_landings: HashMap<GoalId, i64> = HashMap::new();
    let mut close_delays = Vec::new();
    for event in landings {
        let Some(at) = timestamp_millis(&event.created_at) else {
            continue;
        };
        match event.kind.known() {
            Some(EventKind::RunIntegrated) => {
                if let Some(goal) = event.task_id.and_then(|t| goals.get(&t).copied().flatten()) {
                    last_landings.insert(goal, at);
                }
            }
            Some(EventKind::GoalClosed)
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
    // The wait to a person's answer reads only the opening and the answer.
    let latest = asks.iter().map(|e| e.id).max();
    let ask_waits = latest.map_or_else(Vec::new, |latest| {
        stats::asks::human_waits(asks, EventId::new(0), latest, |_| true).0
    });
    History {
        runs,
        close_delays,
        ask_waits,
        last_landings,
    }
}

/// Whether the latest run of an `in_progress` task in `status` is in
/// flight for the forecast (task 1519): a run a process executes or
/// lands, and one between them (`awaiting_integration` through its
/// review, e2e, exit and the wait to land, `needs_session` sent back by
/// its review or e2e and through its resume). A `failed` or `interrupted` run is not: its retry waits
/// for a slot and starts over, as the forecast leaves retries out.
pub fn in_flight(status: RunStatus) -> bool {
    match status {
        RunStatus::Claimed
        | RunStatus::Starting
        | RunStatus::Running
        | RunStatus::Validating
        | RunStatus::AwaitingIntegration
        | RunStatus::Integrating
        | RunStatus::NeedsSession => true,
        RunStatus::Integrated
        | RunStatus::Succeeded
        | RunStatus::Failed
        | RunStatus::Interrupted
        | RunStatus::Replaced => false,
    }
}

/// Where the run of `events` (its own, oldest first) is at unix
/// milliseconds `now_ms`, from its fold up to then
/// ([`crate::domain::ledger::fold::fold_run`]): its stage by the rule of
/// the ledger's reading (before its first `validating`, in it, after it)
/// and how long it has been in it, the phase it goes on in, and how long
/// it has been in a phase a person decides out of the slots (blocker
/// `human`, holding none: an ask's answer, a hold). A run the fold does
/// not place (no `run_phase_changed`, or its life ended) has its stage
/// drawn whole: `work` from 0 seconds and no phase.
pub fn running(events: &[RunEvent], now_ms: i64) -> Running {
    let whole = Running {
        phase: None,
        stage: Stage::Work,
        elapsed: 0,
        waiting: None,
    };
    let recorded = events
        .iter()
        .any(|event| event.kind == EventKind::RunPhaseChanged);
    let Some(life) = fold_run(events, &[], now_ms).filter(|life| recorded && life.end.is_none())
    else {
        return whole;
    };
    let Some(current) = life.intervals.last() else {
        return whole;
    };
    let secs = |since: i64| (now_ms - since).max(0) / 1000;
    let (stage, since) = match validation(&life.intervals) {
        None => (Stage::Work, life.start),
        Some(validating) if std::ptr::eq(validating, current) => {
            (Stage::Validate, validating.start)
        }
        Some(validating) => (Stage::WaitToLand, validating.end),
    };
    Running {
        phase: Some(current.phase.clone()),
        stage,
        elapsed: secs(since),
        waiting: (current.blocker == Some(Blocker::Human) && !current.holds_slot())
            .then(|| secs(current.start)),
    }
}
