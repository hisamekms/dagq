//! `forecast` (ADR-0070 decision 2): the queue's reads that
//! [`crate::domain::forecast::forecast`] computes the completion forecast
//! from. Reads only; nothing is recorded.

use std::collections::HashMap;

use anyhow::Result;

use super::{ProcessControl, Queue, WaitFor, dependency_graph};
use crate::domain::{
    ClaimRank, GoalId, GoalStatus, SupervisorPulse, TaskId, TaskStatus,
    forecast::{
        Forecast, ForecastGoal, ForecastInput, ForecastTask, forecast as simulate, history,
        running, seed,
    },
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
            kind: kinds.get(&node.id).copied().flatten(),
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
    Ok(result)
}
