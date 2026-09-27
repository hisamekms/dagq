//! `forecast` (ADR-0070 decision 2): the queue's reads that
//! [`crate::domain::forecast::forecast`] computes the completion forecast
//! from. `dagq forecast` reads only and records nothing; the supervisor
//! records a snapshot of it at its triggers (ADR-0070 decision 3,
//! [`pending`] and [`record_snapshot`]).

use crate::domain::LeaseToken;
use std::collections::HashMap;

use anyhow::Result;

use super::report::local_day;
use super::{ProcessControl, Queue, WaitFor, dependency_graph};
use crate::domain::{
    ClaimRank, EventId, GoalId, GoalStatus, RunEvent, SupervisorPulse, TaskId, TaskStatus,
    forecast::{
        DEFAULT_TRIALS, Forecast, ForecastGoal, ForecastInput, ForecastTask, forecast as simulate,
        history, running, seed,
        snapshot::{
            FORECAST_RECORDED, Previous, Snapshot, TRIGGER_KINDS, Trigger, decide, trigger,
        },
    },
    marks::utc_text,
};

/// What `forecast` looks at.
#[derive(Debug, Clone, Default)]
pub struct ForecastQuery {
    /// Only this task, and its goal (`--task`).
    pub task_id: Option<TaskId>,
    /// Only this goal and its tasks (`--goal`).
    pub goal_id: Option<GoalId>,
    /// The slots instead of the live supervisors' `parallel` (`--parallel`).
    pub parallel: Option<usize>,
    pub trials: usize,
}

/// The forecast of every open task and goal at the unix second `now`, each
/// kind drawing from the whole distribution below `min_samples` landed
/// runs, narrowed to `query` after the whole queue is simulated.
pub fn forecast(
    queue: &dyn Queue,
    processes: &dyn ProcessControl,
    now: i64,
    min_samples: usize,
    query: &ForecastQuery,
) -> Result<Forecast> {
    forecast_through(queue, processes, now, min_samples, query).map(|(forecast, _)| forecast)
}

/// [`forecast`], with the latest event ID the history was read through.
fn forecast_through(
    queue: &dyn Queue,
    processes: &dyn ProcessControl,
    now: i64,
    min_samples: usize,
    query: &ForecastQuery,
) -> Result<(Forecast, i64)> {
    let events = queue.all_events()?;
    let goals = queue.task_goals()?;
    let kinds = queue.task_kinds()?;
    let history = history(&events, &goals, &kinds, now);
    let parallel = match query.parallel {
        Some(parallel) => parallel,
        None => queue
            .supervisors()?
            .iter()
            .filter(|registration| {
                !SupervisorPulse::judge(registration, processes.alive(registration.pid), now).stale
            })
            .map(|registration| registration.parallel as usize)
            .sum(),
    };
    let mut runs: HashMap<TaskId, _> = HashMap::new();
    for run in queue.active_runs()? {
        runs.insert(run.task_id(), run.id().clone());
    }
    let graph = dependency_graph(queue.graph_input()?, None);
    let mut tasks = Vec::new();
    for node in graph.tasks.iter().filter(|node| {
        matches!(node.status, TaskStatus::Ready | TaskStatus::InProgress)
            && node.goal_status != Some(GoalStatus::Draft)
    }) {
        let running = match (node.status, runs.get(&node.id)) {
            (TaskStatus::InProgress, Some(run)) => {
                Some(running(&queue.run_events(run)?, now * 1000))
            }
            _ => None,
        };
        let (mut depends_on, mut goal_dependencies) = (Vec::new(), Vec::new());
        for wait in &node.ready_after {
            match *wait {
                WaitFor::Task(id) => depends_on.push(id),
                WaitFor::Goal { goal } => goal_dependencies.push(goal),
            }
        }
        tasks.push(ForecastTask {
            id: node.id,
            goal_id: node.goal_id,
            kind: kinds.get(&node.id).cloned().flatten(),
            rank: ClaimRank::new(node.effective_priority, node.unblocks, node.id),
            depends_on,
            goal_dependencies,
            running,
        });
    }
    let goal_list: Vec<ForecastGoal> = queue
        .list_goals()?
        .into_iter()
        .filter(|goal| goal.status != GoalStatus::Draft && !goal.closed)
        .map(|goal| ForecastGoal {
            id: goal.id,
            since_last_landing: history
                .last_landings
                .get(&goal.id)
                .map(|landed| (now * 1000 - landed).max(0) / 1000),
            unplanned_tasks: goal.tasks.draft + goal.tasks.submitted,
            empty: goal.tasks.total == 0,
        })
        .collect();
    let latest = events
        .iter()
        .map(|event| event.id.as_i64())
        .max()
        .unwrap_or(0);
    let mut result = simulate(&ForecastInput {
        now,
        tasks: &tasks,
        goals: &goal_list,
        parallel,
        history: &history,
        min_samples,
        trials: query.trials,
        seed: seed(now, latest),
    });
    if let Some(task_id) = query.task_id {
        let goal_id = result
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .and_then(|task| task.goal_id);
        result.tasks.retain(|task| task.id == task_id);
        result.goals.retain(|goal| Some(goal.id) == goal_id);
    }
    if let Some(goal_id) = query.goal_id {
        result.tasks.retain(|task| task.goal_id == Some(goal_id));
        result.goals.retain(|goal| goal.id == goal_id);
    }
    Ok((result, latest))
}

/// The trigger events one look reads at most.
const TRIGGERS_READ: usize = 1000;

/// The triggers of a snapshot (ADR-0070 decision 3) found at one look,
/// with the latest snapshot they are judged against.
#[derive(Debug, Clone)]
pub struct Pending {
    pub triggers: Vec<Trigger>,
    /// The trigger events were read through this event ID.
    pub through: i64,
    /// The latest `forecast_recorded`, `None` before the first.
    pub previous: Option<RunEvent>,
}

/// The triggers of a snapshot at the unix second `now`: the trigger
/// events after the latest snapshot's (and after `checked`, where a look
/// that recorded nothing left off), and the daily one when no snapshot was
/// taken yet on the host's local day (`utc_offset` seconds east of UTC).
/// Before the first snapshot only the daily one: the history is no
/// trigger. `None` when there is none.
pub fn pending(
    queue: &dyn Queue,
    now: i64,
    utc_offset: fn(i64) -> i64,
    checked: Option<i64>,
) -> Result<Option<Pending>> {
    let mut through = queue.latest_event_id()?.as_i64();
    let previous = queue.latest_event_of(FORECAST_RECORDED)?;
    let read = previous
        .as_ref()
        .and_then(|event| Previous::read(&event.payload));
    let day = |secs: i64| local_day(secs, utc_offset(secs));
    let mut triggers = Vec::new();
    if let Some(read) = &read {
        let after = read.triggers_through.max(checked.unwrap_or(0));
        if after < through {
            let events = queue.events_of_between(
                &TRIGGER_KINDS,
                EventId::new(after),
                EventId::new(through),
                TRIGGERS_READ,
            )?;
            // Read through the last one only when there may be more: the
            // next look takes the rest.
            if events.len() >= TRIGGERS_READ
                && let Some(last) = events.last()
            {
                through = last.id.as_i64();
            }
            triggers.extend(events.iter().filter_map(trigger));
        }
    }
    if read
        .as_ref()
        .is_none_or(|read| day(read.at_secs) < day(now))
    {
        let local = now + utc_offset(now);
        triggers.push(Trigger::Daily {
            day: utc_text(local * 1000)[..10].to_owned(),
        });
    }
    Ok((!triggers.is_empty()).then_some(Pending {
        triggers,
        through,
        previous,
    }))
}

/// What one snapshot job did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotOutcome {
    /// Recorded as this event, with this many tasks and goals.
    Recorded {
        event: EventId,
        tasks: usize,
        goals: usize,
    },
    /// Nothing held: only landings that moved no p50, or changes of tasks
    /// not forecast.
    Unmoved,
    /// Another supervisor recorded a snapshot meanwhile.
    Taken,
}

/// Compute the forecast of every open task and goal at `now` and record it
/// as one `forecast_recorded` by `supervisor` when [`decide`] keeps any of
/// `pending`'s triggers.
pub fn record_snapshot(
    queue: &dyn Queue,
    processes: &dyn ProcessControl,
    supervisor: &LeaseToken,
    now: i64,
    min_samples: usize,
    pending: &Pending,
) -> Result<SnapshotOutcome> {
    let query = ForecastQuery {
        trials: DEFAULT_TRIALS,
        ..ForecastQuery::default()
    };
    let (forecast, events_through) = forecast_through(queue, processes, now, min_samples, &query)?;
    let previous = pending
        .previous
        .as_ref()
        .and_then(|event| Previous::read(&event.payload));
    let Some((triggers, moved)) =
        decide(pending.triggers.clone(), &forecast, previous.as_ref(), now)
    else {
        return Ok(SnapshotOutcome::Unmoved);
    };
    let payload = serde_json::to_value(Snapshot {
        supervisor: supervisor.as_str(),
        at_secs: now,
        triggers: &triggers,
        triggers_through: pending.through,
        events_through,
        moved: &moved,
        forecast: &forecast,
    })?;
    Ok(
        match queue.record_forecast(payload, pending.previous.as_ref().map(|event| event.id))? {
            Some(event) => SnapshotOutcome::Recorded {
                event,
                tasks: forecast.tasks.len(),
                goals: forecast.goals.len(),
            },
            None => SnapshotOutcome::Taken,
        },
    )
}
