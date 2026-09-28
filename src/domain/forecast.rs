//! Completion forecasts (ADR-0070 decisions 1 and 2): when the open tasks
//! and goals are likely to finish if the plan flows as it is now, as the p50
//! and p90 of a simulation. The simulation puts the claimable tasks into the
//! free slots in claim order ([`ClaimRank`]), draws each run's `work`,
//! `validate` and `wait_to_land` together from one landed run of the task's
//! change (ADR-t980-1; the whole distribution when the change has fewer
//! than `min_samples`),
//! and repeats that `trials` times with a seeded generator, so the same input
//! always gives the same forecast. New tasks (inflow) are never added.
//! Nothing here reads or writes the queue; [`history`] derives the samples
//! from the run events.

pub mod history;
pub mod score;
pub mod snapshot;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

use super::{ClaimRank, GoalId, TaskChange, TaskId, marks::utc_text};

pub use history::{History, Sample, history, running};

/// The version of the method (ADR-0070 decision 1): raised whenever the
/// calculation changes, so the scoring can be read per method: 2 draws
/// from the task's change instead of its kind (ADR-t980-1).
pub const METHOD: u32 = 2;
/// Simulation trials (ADR-0070's first value).
pub const DEFAULT_TRIALS: usize = 1000;
/// The name of the whole distribution.
pub const ALL: &str = "all";
/// The name of the change of a task without one.
pub const UNKNOWN: &str = "unknown";
/// What the forecast leaves out, printed with it.
const LEFT_OUT: [&str; 4] = [
    "draft and submitted tasks",
    "ready tasks of a draft goal",
    "inflow: new tasks, follow-ups and plan review send-backs",
    "failed runs and their retries",
];

/// Where a run in flight is: its interval as `stats` measures it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Claimed, no receipt yet.
    Work,
    /// A receipt, not validated yet.
    Validate,
    /// Validated, not landed yet (review, resumes and `integrate`).
    WaitToLand,
}

/// A run in flight of an open task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Running {
    pub phase: Phase,
    /// Seconds since the phase started.
    pub elapsed: i64,
    /// Seconds it has waited for a person so far, while it waits (ADR-0071:
    /// it holds no slot then).
    pub waiting: Option<i64>,
}

/// An open task (`ready` or `in_progress`, outside a draft goal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForecastTask {
    pub id: TaskId,
    pub goal_id: Option<GoalId>,
    /// The change it declares (ADR-t980-1), whose landed runs it draws from.
    pub change: Option<TaskChange>,
    /// Where it stands in the claim order now.
    pub rank: ClaimRank,
    /// The unfinished tasks it waits for. One that is not forecast (a draft
    /// or submitted task) never finishes here, so neither does this one.
    pub depends_on: Vec<TaskId>,
    /// The goals it waits for, not closed as achieved. One that is not
    /// forecast (a draft or an abandoned goal) never releases it.
    pub goal_dependencies: Vec<GoalId>,
    /// Its run in flight; `None` waits for a slot.
    pub running: Option<Running>,
}

/// An open goal: not a draft and not closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForecastGoal {
    pub id: GoalId,
    /// Seconds since its latest task landed, for a goal with no open task
    /// left (it only waits to be closed).
    pub since_last_landing: Option<i64>,
    /// Its draft and submitted tasks, which the forecast leaves out. A goal
    /// with any never closes here, as it cannot close before they land.
    pub unplanned_tasks: usize,
    /// It has no task at all, finished or not: nothing says when it closes.
    pub empty: bool,
}

/// What [`forecast`] computes from.
#[derive(Debug, Clone, Copy)]
pub struct ForecastInput<'a> {
    /// Unix seconds.
    pub now: i64,
    pub tasks: &'a [ForecastTask],
    pub goals: &'a [ForecastGoal],
    /// The slots: the live supervisors' `parallel` together.
    pub parallel: usize,
    pub history: &'a History,
    /// A change with fewer landed runs uses the whole distribution.
    pub min_samples: usize,
    pub trials: usize,
    pub seed: u64,
}

/// The forecast, as `dagq forecast` prints it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Forecast {
    pub method: u32,
    /// When it was computed.
    pub at: String,
    pub seed: u64,
    pub trials: usize,
    pub assumptions: Assumptions,
    pub tasks: Vec<TaskForecast>,
    pub goals: Vec<GoalForecast>,
}

/// What the forecast assumed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Assumptions {
    pub parallel: usize,
    pub min_samples: usize,
    pub samples: Samples,
    /// The changes of forecast tasks that drew from the whole distribution
    /// for want of their own samples.
    pub substituted: Vec<String>,
    pub left_out: Vec<&'static str>,
}

/// How many samples each distribution had.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Samples {
    /// Landed runs with all three intervals.
    pub all: usize,
    /// Per change of the landed runs and of the forecast tasks.
    pub changes: BTreeMap<String, ChangeSamples>,
    /// Goals closed as achieved after a landing: the delay to close.
    pub close_delay: usize,
    /// Asks a person answered: the wait of a run waiting for one.
    pub ask_wait: usize,
}

/// A change's samples and the distribution its tasks draw from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangeSamples {
    pub runs: usize,
    /// The change itself, or `all`.
    pub distribution: String,
}

/// When a task is likely to finish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskForecast {
    pub id: TaskId,
    pub goal_id: Option<GoalId>,
    pub change: Option<TaskChange>,
    /// The phase of its run in flight; null while it waits for a slot.
    pub phase: Option<Phase>,
    pub waiting: bool,
    /// The change whose distribution it drew from, or `all`.
    pub distribution: String,
    #[serde(flatten)]
    pub at: Percentiles,
}

/// When a goal is likely to be closed as achieved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GoalForecast {
    pub id: GoalId,
    pub open_tasks: usize,
    pub unplanned_tasks: usize,
    #[serde(flatten)]
    pub at: Percentiles,
}

/// The p50 and p90 completion times, and the seconds from now to them;
/// all null with a `reason` when it never finishes in the simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Percentiles {
    pub p50: Option<String>,
    pub p90: Option<String>,
    pub p50_secs: Option<i64>,
    pub p90_secs: Option<i64>,
    /// `no_samples` (no landed run to draw from), `no_slots` (`parallel`
    /// is 0) or `blocked` (it waits for a task or goal the forecast leaves
    /// out); absent when forecast.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

/// A seed from the input (ADR-0070 decision 1): the time and the queue's
/// latest event, so the same moment gives the same forecast.
pub fn seed(now: i64, latest_event: i64) -> u64 {
    Rng::new((now as u64) ^ (latest_event as u64).rotate_left(32)).next()
}

/// The forecast of `input` (ADR-0070 decision 1).
pub fn forecast(input: &ForecastInput<'_>) -> Forecast {
    let history = input.history;
    let change_name =
        |change: Option<&TaskChange>| change.map_or(UNKNOWN, TaskChange::as_str).to_owned();
    let all: Vec<&Sample> = history.runs.iter().map(|(_, sample)| sample).collect();
    let mut by_change: BTreeMap<String, Vec<&Sample>> = BTreeMap::new();
    for (change, sample) in &history.runs {
        by_change
            .entry(change_name(change.as_ref()))
            .or_default()
            .push(sample);
    }
    for task in input.tasks {
        by_change
            .entry(change_name(task.change.as_ref()))
            .or_default();
    }
    let own = |runs: &[&Sample]| !runs.is_empty() && runs.len() >= input.min_samples;
    let changes: BTreeMap<String, ChangeSamples> = by_change
        .iter()
        .map(|(change, runs)| {
            let distribution = if own(runs) {
                change.clone()
            } else {
                ALL.to_owned()
            };
            (
                change.clone(),
                ChangeSamples {
                    runs: runs.len(),
                    distribution,
                },
            )
        })
        .collect();
    let distribution_of = |task: &ForecastTask| {
        changes[&change_name(task.change.as_ref())]
            .distribution
            .clone()
    };
    let pools: Vec<&[&Sample]> = input
        .tasks
        .iter()
        .map(|task| {
            let name = distribution_of(task);
            if name == ALL {
                all.as_slice()
            } else {
                by_change[&name].as_slice()
            }
        })
        .collect();
    let mut substituted: Vec<String> = input
        .tasks
        .iter()
        .map(|task| change_name(task.change.as_ref()))
        .filter(|change| changes[change].distribution == ALL)
        .collect();
    substituted.sort();
    substituted.dedup();

    let trials = input.trials.max(1);
    let simulation = Simulation::new(input, &pools);
    let mut task_ends: Vec<Vec<Option<i64>>> = vec![Vec::with_capacity(trials); input.tasks.len()];
    let mut goal_ends: Vec<Vec<Option<i64>>> = vec![Vec::with_capacity(trials); input.goals.len()];
    let mut rng = Rng::new(input.seed);
    if !all.is_empty() {
        for _ in 0..trials {
            let (tasks, goals) = simulation.trial(&mut rng);
            for (ends, end) in task_ends.iter_mut().zip(tasks) {
                ends.push(end);
            }
            for (ends, end) in goal_ends.iter_mut().zip(goals) {
                ends.push(end);
            }
        }
    }
    let unfinished = |blocked: bool| {
        if all.is_empty() {
            "no_samples"
        } else if blocked {
            "blocked"
        } else {
            "no_slots"
        }
    };
    let tasks = input
        .tasks
        .iter()
        .zip(&task_ends)
        .zip(&simulation.blocked_tasks)
        .map(|((task, ends), &blocked)| TaskForecast {
            id: task.id,
            goal_id: task.goal_id,
            change: task.change.clone(),
            phase: task.running.map(|running| running.phase),
            waiting: task.running.is_some_and(|r| r.waiting.is_some()),
            distribution: distribution_of(task),
            at: percentiles(input.now, ends, unfinished(blocked)),
        })
        .collect();
    let goals = input
        .goals
        .iter()
        .zip(&goal_ends)
        .zip(&simulation.blocked_goals)
        .map(|((goal, ends), &blocked)| GoalForecast {
            id: goal.id,
            open_tasks: input
                .tasks
                .iter()
                .filter(|task| task.goal_id == Some(goal.id))
                .count(),
            unplanned_tasks: goal.unplanned_tasks,
            at: percentiles(input.now, ends, unfinished(blocked)),
        })
        .collect();
    Forecast {
        method: METHOD,
        at: utc_text(input.now * 1000),
        seed: input.seed,
        trials,
        assumptions: Assumptions {
            parallel: input.parallel,
            min_samples: input.min_samples,
            samples: Samples {
                all: all.len(),
                changes,
                close_delay: history.close_delays.len(),
                ask_wait: history.ask_waits.len(),
            },
            substituted,
            left_out: LEFT_OUT.to_vec(),
        },
        tasks,
        goals,
    }
}

/// The nearest-rank p50 and p90 of `ends` (seconds from `now`); null when
/// a trial never finished it.
fn percentiles(now: i64, ends: &[Option<i64>], unfinished: &'static str) -> Percentiles {
    let finished: Option<Vec<i64>> = ends.iter().copied().collect();
    match finished.filter(|ends| !ends.is_empty()) {
        None => Percentiles {
            p50: None,
            p90: None,
            p50_secs: None,
            p90_secs: None,
            reason: Some(unfinished),
        },
        Some(mut ends) => {
            ends.sort_unstable();
            let rank = |percent: usize| ends[(ends.len() * percent).div_ceil(100) - 1];
            let (p50, p90) = (rank(50), rank(90));
            Percentiles {
                p50: Some(utc_text((now + p50) * 1000)),
                p90: Some(utc_text((now + p90) * 1000)),
                p50_secs: Some(p50),
                p90_secs: Some(p90),
                reason: None,
            }
        }
    }
}

/// The fixed part of every trial.
struct Simulation<'a> {
    input: &'a ForecastInput<'a>,
    pools: &'a [&'a [&'a Sample]],
    /// Task indexes in claim order.
    order: Vec<usize>,
    /// Per task: the indexes of the tasks it waits for, and of the goals;
    /// `None` when it waits for one the forecast leaves out.
    waits: Vec<Option<(Vec<usize>, Vec<usize>)>>,
    /// Per goal: the indexes of its tasks.
    members: Vec<Vec<usize>>,
    /// Per task and per goal: whether it waits, directly or through others,
    /// for what the forecast leaves out, so it never finishes here.
    blocked_tasks: Vec<bool>,
    blocked_goals: Vec<bool>,
}

impl<'a> Simulation<'a> {
    fn new(input: &'a ForecastInput<'a>, pools: &'a [&'a [&'a Sample]]) -> Self {
        let task_index: HashMap<TaskId, usize> = input
            .tasks
            .iter()
            .enumerate()
            .map(|(index, task)| (task.id, index))
            .collect();
        let goal_index: HashMap<GoalId, usize> = input
            .goals
            .iter()
            .enumerate()
            .map(|(index, goal)| (goal.id, index))
            .collect();
        let mut order: Vec<usize> = (0..input.tasks.len()).collect();
        order.sort_by_key(|&index| input.tasks[index].rank);
        let waits: Vec<Option<(Vec<usize>, Vec<usize>)>> = input
            .tasks
            .iter()
            .map(|task| {
                let tasks: Option<Vec<usize>> = task
                    .depends_on
                    .iter()
                    .map(|id| task_index.get(id).copied())
                    .collect();
                let goals: Option<Vec<usize>> = task
                    .goal_dependencies
                    .iter()
                    .map(|id| goal_index.get(id).copied())
                    .collect();
                Some((tasks?, goals?))
            })
            .collect();
        let members: Vec<Vec<usize>> = input
            .goals
            .iter()
            .map(|goal| {
                (0..input.tasks.len())
                    .filter(|&index| input.tasks[index].goal_id == Some(goal.id))
                    .collect()
            })
            .collect();
        let mut blocked_tasks: Vec<bool> = waits.iter().map(Option::is_none).collect();
        let mut blocked_goals: Vec<bool> = input
            .goals
            .iter()
            .map(|goal| goal.unplanned_tasks > 0 || goal.empty)
            .collect();
        loop {
            let mut changed = false;
            for (index, wait) in waits.iter().enumerate() {
                if let Some((tasks, goals)) = wait
                    && !blocked_tasks[index]
                    && (tasks.iter().any(|&t| blocked_tasks[t])
                        || goals.iter().any(|&g| blocked_goals[g]))
                {
                    blocked_tasks[index] = true;
                    changed = true;
                }
            }
            for (index, tasks) in members.iter().enumerate() {
                if !blocked_goals[index] && tasks.iter().any(|&t| blocked_tasks[t]) {
                    blocked_goals[index] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        Self {
            input,
            pools,
            order,
            waits,
            members,
            blocked_tasks,
            blocked_goals,
        }
    }

    /// One trial: per task and per goal, the seconds from now it finishes
    /// in, `None` when it never does.
    fn trial(&self, rng: &mut Rng) -> (Vec<Option<i64>>, Vec<Option<i64>>) {
        let input = self.input;
        let history = input.history;
        let mut end: Vec<Option<i64>> = vec![None; input.tasks.len()];
        let mut holds = vec![false; input.tasks.len()];
        let mut close: Vec<Option<i64>> = vec![None; input.goals.len()];
        for (index, task) in input.tasks.iter().enumerate() {
            if let Some(running) = task.running {
                let sample = draw(self.pools[index], rng);
                let left = remaining(self.pools[index], sample, running, rng);
                let asked = running
                    .waiting
                    .map_or(0, |waited| draw_after(&history.ask_waits, waited, rng));
                end[index] = Some(left + asked);
                holds[index] = running.waiting.is_none();
            }
        }
        for (index, goal) in input.goals.iter().enumerate() {
            if self.members[index].is_empty() && !self.blocked_goals[index] {
                let since = goal.since_last_landing.unwrap_or(0);
                close[index] = Some(draw_after(&history.close_delays, since, rng));
            }
        }
        let mut now = 0;
        loop {
            loop {
                for (index, members) in self.members.iter().enumerate() {
                    if close[index].is_none()
                        && !self.blocked_goals[index]
                        && !members.is_empty()
                        && let Some(last) =
                            members.iter().map(|&m| end[m]).collect::<Option<Vec<_>>>()
                    {
                        let last = last.into_iter().max().unwrap_or(now);
                        close[index] = Some(last + draw_after(&history.close_delays, 0, rng));
                    }
                }
                let used = (0..end.len())
                    .filter(|&i| holds[i] && end[i].is_some_and(|at| at > now))
                    .count();
                let mut free = input.parallel.saturating_sub(used);
                let mut claimed = false;
                for &index in &self.order {
                    if free == 0 {
                        break;
                    }
                    if end[index].is_some() || !self.released(index, now, &end, &close) {
                        continue;
                    }
                    let sample = draw(self.pools[index], rng);
                    end[index] = Some(now + sample.total());
                    holds[index] = true;
                    free -= 1;
                    claimed = true;
                }
                if !claimed {
                    break;
                }
            }
            let next = end
                .iter()
                .chain(&close)
                .filter_map(|at| *at)
                .filter(|&at| at > now)
                .min();
            match next {
                Some(next) => now = next,
                None => break,
            }
        }
        (end, close)
    }

    /// Whether every task and goal `index` waits for is done at `now`.
    fn released(&self, index: usize, now: i64, end: &[Option<i64>], close: &[Option<i64>]) -> bool {
        let done = |at: Option<i64>| at.is_some_and(|at| at <= now);
        self.waits[index].as_ref().is_some_and(|(tasks, goals)| {
            tasks.iter().all(|&t| done(end[t])) && goals.iter().all(|&g| done(close[g]))
        })
    }
}

/// The seconds left of a run in flight: the rest of its phase from a
/// landed run whose phase lasted longer than it has so far (`fallback`
/// when none did), and the phases after it.
fn remaining(pool: &[&Sample], fallback: &Sample, running: Running, rng: &mut Rng) -> i64 {
    let longer: Vec<&Sample> = pool
        .iter()
        .copied()
        .filter(|sample| sample.phase(running.phase) > running.elapsed)
        .collect();
    match longer.is_empty() {
        true => fallback.from(running.phase),
        false => draw(&longer, rng).from(running.phase) - running.elapsed,
    }
}

/// A value of `values` longer than `elapsed` (any value when nothing has
/// elapsed), less `elapsed`; a value of all of them when none is longer,
/// and 0 without any.
fn draw_after(values: &[i64], elapsed: i64, rng: &mut Rng) -> i64 {
    let longer: Vec<i64> = values
        .iter()
        .copied()
        .filter(|&v| v > elapsed || elapsed <= 0)
        .collect();
    if !longer.is_empty() {
        return longer[rng.below(longer.len())] - elapsed;
    }
    if values.is_empty() {
        0
    } else {
        values[rng.below(values.len())]
    }
}

fn draw<'s>(pool: &[&'s Sample], rng: &mut Rng) -> &'s Sample {
    pool[rng.below(pool.len())]
}

/// SplitMix64: small, seedable and the same on every platform.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// An index below `len` (> 0).
    fn below(&mut self, len: usize) -> usize {
        (self.next() % len as u64) as usize
    }
}
