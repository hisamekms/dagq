//! The trial of the worker's model (ADR-0079 decisions 4 and 6), group by
//! group: over the runs of the page claimed in a group, how fast they were
//! and how often their task caused rework. The judgement (about 45 runs a
//! group, the treatment's rework rate at most 5 points above the
//! control's) is left to a person and the planner.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::{RunStats, Summary, rfc3339_millis, summary};
use crate::domain::TaskId;

/// One group of the trial.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrialGroupStats {
    /// `control` or `treatment`.
    pub group: String,
    /// The models and efforts its runs were claimed with, and how many.
    pub sessions: BTreeMap<String, usize>,
    pub runs: usize,
    pub tasks: usize,
    /// Claim to landing, seconds, of its runs that landed.
    pub lead_time: Summary,
    pub work: Summary,
    /// The model's seconds in its runs' own sessions.
    pub model_secs: Summary,
    /// The output tokens of its runs' own sessions.
    pub output_tokens: Summary,
    /// Its runs with task-caused rework (ADR-0079 decision 1: `integrate`'s
    /// verification failed, a review concern or revise; a conflict or a
    /// kill is not one), and their share of `runs` in percent, to one
    /// decimal.
    pub task_rework: usize,
    pub task_rework_rate: f64,
}

/// The groups of `runs` that were claimed in one, by name.
pub fn trial_groups(runs: &[&RunStats]) -> Vec<TrialGroupStats> {
    let mut groups: BTreeMap<&str, Vec<&RunStats>> = BTreeMap::new();
    for run in runs {
        if let Some(group) = run.measures.trial_group.as_deref() {
            groups.entry(group).or_default().push(run);
        }
    }
    groups
        .into_iter()
        .map(|(group, runs)| {
            let mut sessions = BTreeMap::new();
            for run in &runs {
                let model = run.measures.worker_model.as_deref().unwrap_or("unknown");
                let effort = run.measures.worker_effort.as_deref().unwrap_or("unknown");
                *sessions.entry(format!("{model}/{effort}")).or_default() += 1;
            }
            let tasks: BTreeSet<TaskId> = runs.iter().map(|run| run.task_id).collect();
            let task_rework = runs.iter().filter(|run| run.actual.task_rework).count();
            #[allow(clippy::cast_precision_loss)]
            let rate = (task_rework as f64 / runs.len() as f64 * 1000.0).round() / 10.0;
            TrialGroupStats {
                group: group.to_owned(),
                sessions,
                runs: runs.len(),
                tasks: tasks.len(),
                lead_time: summary(runs.iter().map(|run| lead_time(run))),
                work: summary(runs.iter().map(|run| run.work)),
                model_secs: summary(runs.iter().map(|run| run.actual.model_secs)),
                output_tokens: summary(runs.iter().map(|run| run.actual.output_tokens)),
                task_rework,
                task_rework_rate: rate,
            }
        })
        .collect()
}

/// Seconds from the run's claim to its landing.
fn lead_time(run: &RunStats) -> Option<i64> {
    let claimed = rfc3339_millis(run.claimed_at.as_deref()?)?;
    let landed = rfc3339_millis(run.landed_at.as_deref()?)?;
    Some((landed - claimed) / 1000)
}
