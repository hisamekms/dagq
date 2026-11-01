//! The breakdown of `wait_to_land` by phase (goal 36, ADR-0049 decision
//! 5), kept under today's names for a period (ADR-t1662-1 decision 19):
//! the shapes of a run's `land_phases` and of their summaries over runs.
//! The values are the ledger's ([`crate::domain::ledger::read`]): a new
//! row's intervals from its first `validating` to its `push`, each under
//! the phase below its `run_phase_changed` falls in, or a legacy row's
//! frozen values. The push, which follows `run_integrated`, is measured on
//! its own and is not part of the sum. Each phase is cut to whole seconds,
//! so the sum may fall a few seconds short of `wait_to_land`.
use std::collections::BTreeMap;

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Summary, median};

/// The phases, in the order a landing goes through them, as `stats`'
/// `land_phases` names them. The run's phases each falls under are the
/// ledger's read's ([`crate::domain::ledger::read`]).
pub const PHASES: [&str; 12] = [
    // The session's exit, the hand-offs between phases and the time no
    // phase names (unattributed): what is left of the wait.
    "exit",
    // The review (`review`, `review_held`).
    "review",
    // A revise of the live session (`revise`).
    "revise",
    // A revise to resolve a conflict (`revise` in a conflict attempt).
    "conflict",
    // A person answering an ask of the run (`waiting`), its
    // `approve_landing` among them, or a landing given back to a person
    // (`landing_answer`): a person's wait, not the session's exit.
    "ask",
    // Parked `needs_session` and resumed (`needs_session`, `resume`,
    // `recovery`).
    "resume",
    // Waiting for the integration slot (`landing_queue`, `awaiting_slot`).
    "landing_queue",
    // Waiting for the e2e of another run before its own (`awaiting_e2e`,
    // ADR-t1233-2).
    "e2e_wait",
    // The runtime's e2e on the host after the review (`e2e`).
    "e2e",
    // `integrate` up to its rebase. The landing is one phase now, so a new
    // row's is 0 and all of it is `verify`; a legacy row keeps its value.
    "rebase",
    // `integrate`: the rebase, the checks and the commit to main
    // (`landing`).
    "verify",
    // Waiting for its task's hold (ADR-t1879-1, `held`): like `ask`, a
    // person's wait, which a reader leaves out of the run's time.
    "held",
];

/// The seconds a run spent in each phase of its wait to land, and in the
/// push after it (null when no push was recorded).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LandPhases {
    pub secs: [i64; PHASES.len()],
    pub push: Option<i64>,
    /// The `verify` phase per verification command, by command (task 509):
    /// all the run's `integrate` attempts, failed commands included. Their
    /// sum does not exceed `verify`. A legacy row's frozen list; empty for
    /// a new row, whose intervals do not split by command.
    pub verify_commands: Vec<CommandSecs>,
    /// The `landing_queue` phase per what queued the run (task 949): the
    /// `via` of its `landing_queued` (`exit`, `resume`, `approve`), or
    /// `approve` from an `approve_landing` answer the runtime applies;
    /// `unknown` for a `landing_queued` without one. Their sum does not
    /// exceed `landing_queue` (each is cut to the second). A legacy row's
    /// frozen list; empty for a new row.
    pub landing_queue_via: Vec<ViaSecs>,
}

/// One `via`'s share of a run's `landing_queue` phase.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViaSecs {
    pub via: String,
    pub secs: i64,
}

/// One verification command's share of a run's `verify` phase.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSecs {
    pub command: String,
    /// How many times it ran over the run's attempts.
    pub count: usize,
    pub secs: i64,
}

impl LandPhases {
    /// The phase the most time went to; `None` when none took any.
    pub fn longest(&self) -> Option<&'static str> {
        let (index, secs) = self
            .secs
            .iter()
            .enumerate()
            .max_by_key(|&(index, secs)| (*secs, std::cmp::Reverse(index)))?;
        (*secs > 0).then_some(PHASES[index])
    }
}

impl Serialize for LandPhases {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(PHASES.len() + 1))?;
        for (name, secs) in PHASES.iter().zip(self.secs) {
            map.serialize_entry(name, &secs)?;
        }
        map.serialize_entry("push", &self.push)?;
        map.serialize_entry("verify_commands", &self.verify_commands)?;
        map.serialize_entry("landing_queue_via", &self.landing_queue_via)?;
        map.end()
    }
}

/// Reads back what [`Serialize`] wrote (the ledger's legacy rows keep it
/// as JSON); a phase that is not there reads as 0.
impl<'de> Deserialize<'de> for LandPhases {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = serde_json::Map::<String, Value>::deserialize(deserializer)?;
        let mut phases = Self::default();
        for (index, name) in PHASES.iter().enumerate() {
            phases.secs[index] = map.get(*name).and_then(Value::as_i64).unwrap_or(0);
        }
        phases.push = map.get("push").and_then(Value::as_i64);
        let list = |key: &str| map.get(key).cloned().unwrap_or(Value::Array(Vec::new()));
        phases.verify_commands =
            serde_json::from_value(list("verify_commands")).map_err(serde::de::Error::custom)?;
        phases.landing_queue_via =
            serde_json::from_value(list("landing_queue_via")).map_err(serde::de::Error::custom)?;
        Ok(phases)
    }
}

/// One phase over a set of runs: `Summary`'s count, sum and median, the
/// 90th percentile and the maximum, and the sum over the tail runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PhaseSummary {
    #[serde(flatten)]
    pub summary: Summary,
    pub p90: Option<i64>,
    pub max: Option<i64>,
    /// The sum over the runs whose `wait_to_land` is at least
    /// [`LandBreakdown::tail_threshold`].
    pub tail_total: i64,
}

/// The phases over a set of runs, with the long tail of `wait_to_land`
/// (the runs at or above its 90th percentile) and what each phase
/// contributed to it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LandBreakdown {
    /// Runs with a breakdown (those that landed).
    pub runs: usize,
    /// The 90th percentile of those runs' `wait_to_land`.
    pub tail_threshold: Option<i64>,
    pub tail_runs: usize,
    pub phases: [PhaseSummary; PHASES.len()],
    pub push: PhaseSummary,
    /// The `verify` phase per verification command, by command: each over
    /// the runs that ran it, with a run's attempts added up.
    pub verify_commands: Vec<CommandSummary>,
    /// The `landing_queue` phase per `via` (task 949): each over the runs
    /// that waited so, with a run's waits added up.
    pub landing_queue_via: Vec<ViaSummary>,
}

/// One `via`'s share of `landing_queue` over a set of runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ViaSummary {
    pub via: String,
    #[serde(flatten)]
    pub phase: PhaseSummary,
}

/// One verification command's share of `verify` over a set of runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CommandSummary {
    pub command: String,
    #[serde(flatten)]
    pub phase: PhaseSummary,
}

impl Serialize for LandBreakdown {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(PHASES.len() + 6))?;
        map.serialize_entry("runs", &self.runs)?;
        map.serialize_entry("tail_threshold", &self.tail_threshold)?;
        map.serialize_entry("tail_runs", &self.tail_runs)?;
        for (name, phase) in PHASES.iter().zip(&self.phases) {
            map.serialize_entry(name, phase)?;
        }
        map.serialize_entry("push", &self.push)?;
        map.serialize_entry("verify_commands", &self.verify_commands)?;
        map.serialize_entry("landing_queue_via", &self.landing_queue_via)?;
        map.end()
    }
}

/// The nearest-rank 90th percentile of `values` (sorted in place).
pub fn p90(values: &mut [i64]) -> Option<i64> {
    values.sort_unstable();
    let rank = (values.len() * 9).div_ceil(10);
    values.get(rank.checked_sub(1)?).copied()
}

fn phase_summary(values: &[(i64, bool)]) -> PhaseSummary {
    let mut all: Vec<i64> = values.iter().map(|(secs, _)| *secs).collect();
    PhaseSummary {
        summary: Summary {
            count: all.len(),
            total: all.iter().sum(),
            median: median(&mut all),
        },
        p90: p90(&mut all),
        max: all.iter().copied().max(),
        tail_total: values
            .iter()
            .filter(|(_, tail)| *tail)
            .map(|(secs, _)| secs)
            .sum(),
    }
}

/// Aggregate the breakdowns of runs, each with its `wait_to_land`.
pub fn breakdown<'a>(runs: impl Iterator<Item = (&'a LandPhases, i64)>) -> LandBreakdown {
    let runs: Vec<(&LandPhases, i64)> = runs.collect();
    let tail_threshold = p90(&mut runs.iter().map(|(_, wait)| *wait).collect::<Vec<_>>());
    let tail = |wait: i64| tail_threshold.is_some_and(|threshold| wait >= threshold);
    LandBreakdown {
        runs: runs.len(),
        tail_threshold,
        tail_runs: runs.iter().filter(|(_, wait)| tail(*wait)).count(),
        phases: std::array::from_fn(|index| {
            phase_summary(
                &runs
                    .iter()
                    .map(|(phases, wait)| (phases.secs[index], tail(*wait)))
                    .collect::<Vec<_>>(),
            )
        }),
        push: phase_summary(
            &runs
                .iter()
                .filter_map(|(phases, wait)| Some((phases.push?, tail(*wait))))
                .collect::<Vec<_>>(),
        ),
        verify_commands: {
            let mut by_command: BTreeMap<&str, Vec<(i64, bool)>> = BTreeMap::new();
            for (phases, wait) in &runs {
                for command in &phases.verify_commands {
                    by_command
                        .entry(&command.command)
                        .or_default()
                        .push((command.secs, tail(*wait)));
                }
            }
            by_command
                .into_iter()
                .map(|(command, values)| CommandSummary {
                    command: command.to_owned(),
                    phase: phase_summary(&values),
                })
                .collect()
        },
        landing_queue_via: {
            let mut by_via: BTreeMap<&str, Vec<(i64, bool)>> = BTreeMap::new();
            for (phases, wait) in &runs {
                for via in &phases.landing_queue_via {
                    by_via
                        .entry(&via.via)
                        .or_default()
                        .push((via.secs, tail(*wait)));
                }
            }
            by_via
                .into_iter()
                .map(|(via, values)| ViaSummary {
                    via: via.to_owned(),
                    phase: phase_summary(&values),
                })
                .collect()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const EXIT: usize = 0;
    const VERIFY: usize = 10;

    #[test]
    fn breakdowns_split_verify_by_command_over_the_runs_that_ran_it() {
        let run = |verify: i64, commands: &[(&str, i64)]| LandPhases {
            secs: std::array::from_fn(|index| if index == VERIFY { verify } else { 0 }),
            push: None,
            verify_commands: commands
                .iter()
                .map(|(command, secs)| CommandSecs {
                    command: (*command).to_owned(),
                    count: 1,
                    secs: *secs,
                })
                .collect(),
            landing_queue_via: Vec::new(),
        };
        let runs = [
            run(100, &[("fmt", 2), ("llvm-cov", 90)]),
            run(200, &[("fmt", 3), ("llvm-cov", 190)]),
            run(10, &[("fmt", 1)]),
        ];
        let result = breakdown(runs.iter().map(|phases| (phases, phases.secs[VERIFY])));
        assert_eq!(result.tail_threshold, Some(200));
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(
            json["verify_commands"],
            json!([
                {"command": "fmt", "count": 3, "total": 6, "median": 2, "p90": 3, "max": 3, "tail_total": 3},
                {"command": "llvm-cov", "count": 2, "total": 280, "median": 140, "p90": 190, "max": 190, "tail_total": 190},
            ])
        );
        assert_eq!(json["verify"]["total"], 310);
    }

    #[test]
    fn breakdowns_sum_and_mark_the_tail() {
        let run = |exit: i64, verify: i64, push: Option<i64>| LandPhases {
            secs: std::array::from_fn(|index| match index {
                EXIT => exit,
                VERIFY => verify,
                _ => 0,
            }),
            push,
            verify_commands: Vec::new(),
            landing_queue_via: Vec::new(),
        };
        let runs: Vec<LandPhases> = (1..=10)
            .map(|n| {
                run(
                    n,
                    if n == 10 { 1000 } else { 10 },
                    (n % 2 == 0).then_some(n),
                )
            })
            .collect();
        let result = breakdown(runs.iter().map(|phases| (phases, phases.secs.iter().sum())));
        assert_eq!(result.runs, 10);
        assert_eq!(result.tail_threshold, Some(19));
        assert_eq!(result.tail_runs, 2);
        let verify = &result.phases[VERIFY];
        assert_eq!(verify.summary.total, 1090);
        assert_eq!(verify.summary.median, Some(10));
        assert_eq!((verify.p90, verify.max), (Some(10), Some(1000)));
        assert_eq!(verify.tail_total, 1010);
        assert_eq!(result.push.summary.count, 5);
        assert_eq!(result.push.tail_total, 10);
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["verify"]["count"], 10);
        assert_eq!(json["verify"]["p90"], 10);
        assert_eq!(json["tail_runs"], 2);
        assert_eq!(json["review"]["median"], 0);
        assert_eq!(serde_json::to_value(&runs[0]).unwrap()["push"], Value::Null);
        assert_eq!(breakdown(std::iter::empty()).tail_threshold, None);
        assert_eq!(p90(&mut []), None);
    }
}
