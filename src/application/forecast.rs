//! `forecast` (ADR-0070 decision 2): the queue's reads that
//! [`crate::domain::forecast::forecast`] computes the completion forecast
//! from. `dagq forecast` reads only and records nothing; the supervisor
//! records a snapshot of it at its triggers (ADR-0070 decision 3,
//! [`pending`] and [`record_snapshot`]). It reads no whole log
//! (ADR-t3745-1): the landed runs' samples from the LedgerStore's run
//! rows a page at a time through the ledger's one read, the landings, goal
//! closes and asks by their kinds, and a run in flight by its own events.

use crate::domain::EventKind;
use crate::domain::LeaseToken;
use std::collections::HashMap;

use anyhow::Result;

use super::report::local_day;
use super::{
    LedgerStore, PlanningRecords, ProcessControl, QueueRecords, RunLog, SupervisorReads, TaskReads,
    WaitFor, dependency_graph,
};
use crate::domain::{
    ClaimRank, EventId, GoalId, GoalStatus, RunEvent, SupervisorPulse, TaskChange, TaskId,
    TaskStatus, event_kind,
    forecast::{
        DEFAULT_TRIALS, Forecast, ForecastGoal, ForecastInput, ForecastTask, Sample,
        forecast as simulate, history, in_flight, running, sample, seed,
        snapshot::{
            FORECAST_RECORDED, Previous, Snapshot, TRIGGER_KINDS, Trigger, decide, trigger,
        },
    },
    ledger::{LedgerKind, read::read},
    marks::utc_text,
};

/// The ledger's run rows one read takes at most.
const ROWS_PAGE: usize = 200;
/// The events of the kinds the history reads one read takes at most: an
/// `ask_opened` carries its question, so a page stays well under what one
/// answer of the queue service holds.
const EVENTS_PAGE: usize = 500;

/// The landed runs' samples with their task's change, from the
/// LedgerStore's run rows a page at a time, each read through the
/// ledger's one read ([`crate::domain::ledger::read::read`]) and let go.
fn samples(
    ledger: &(impl LedgerStore + ?Sized),
    changes: &HashMap<TaskId, Option<TaskChange>>,
) -> Result<Vec<(Option<TaskChange>, Sample)>> {
    let mut runs = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = ledger.ledger_rows_after(LedgerKind::Run, after.as_deref(), ROWS_PAGE)?;
        for row in &page {
            if let Some(drawn) = sample(&read(row)) {
                let change = row
                    .task_id
                    .and_then(|task| changes.get(&task).cloned().flatten());
                runs.push((change, drawn));
            }
        }
        // A page may be cut short with rows after it: only an empty one
        // ends.
        match page.last() {
            Some(last) => after = Some(last.key.clone()),
            None => return Ok(runs),
        }
    }
}

/// The events of `kinds` up to `upto`, oldest first, a page at a time.
fn events_of(
    queue: &(impl RunLog + ?Sized),
    kinds: &[&str],
    upto: EventId,
) -> Result<Vec<RunEvent>> {
    let mut events = Vec::new();
    let mut after = EventId::new(0);
    loop {
        let page = queue.events_of_between(kinds, after, upto, EVENTS_PAGE)?;
        let full = page.len() >= EVENTS_PAGE;
        after = page.last().map_or(after, |last| last.id);
        events.extend(page);
        if !full {
            return Ok(events);
        }
    }
}

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
/// change drawing from the whole distribution below `min_samples` landed
/// runs, narrowed to `query` after the whole queue is simulated.
pub fn forecast(
    queue: &(impl PlanningRecords + SupervisorReads + RunLog + TaskReads + ?Sized),
    ledger: &(impl LedgerStore + ?Sized),
    processes: &dyn ProcessControl,
    now: i64,
    min_samples: usize,
    query: &ForecastQuery,
) -> Result<Forecast> {
    forecast_through(queue, ledger, processes, now, min_samples, query)
        .map(|(forecast, _)| forecast)
}

/// [`forecast`], with the latest event ID the history was read through.
fn forecast_through(
    queue: &(impl PlanningRecords + SupervisorReads + RunLog + TaskReads + ?Sized),
    ledger: &(impl LedgerStore + ?Sized),
    processes: &dyn ProcessControl,
    now: i64,
    min_samples: usize,
    query: &ForecastQuery,
) -> Result<(Forecast, i64)> {
    let latest = queue.latest_event_id()?;
    let goals = queue.task_goals()?;
    let changes = queue.task_changes()?;
    let landings = events_of(
        queue,
        &[event_kind::RUN_INTEGRATED, event_kind::GOAL_CLOSED],
        latest,
    )?;
    let asks = events_of(
        queue,
        &[event_kind::ASK_OPENED, event_kind::ASK_ANSWERED],
        latest,
    )?;
    let history = history(samples(ledger, &changes)?, &landings, &asks, &goals);
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
    // The latest run of each `in_progress` task, also through its review,
    // revise and resume (task 1519); a failed one's retry waits for a slot.
    let runs: HashMap<TaskId, _> = queue
        .latest_runs_in_progress()?
        .into_iter()
        .filter(|run| in_flight(run.status()))
        .map(|run| (run.task_id(), run.id().clone()))
        .collect();
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
            change: changes.get(&node.id).cloned().flatten(),
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
    let latest = latest.as_i64();
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
    queue: &(impl RunLog + ?Sized),
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
                &TRIGGER_KINDS.map(EventKind::as_str),
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
    queue: &(impl PlanningRecords + QueueRecords + SupervisorReads + RunLog + TaskReads + ?Sized),
    ledger: &(impl LedgerStore + ?Sized),
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
    let (forecast, events_through) =
        forecast_through(queue, ledger, processes, now, min_samples, &query)?;
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
