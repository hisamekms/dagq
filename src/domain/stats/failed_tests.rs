//! The tests that failed, by name (task 515): those `integrate`'s
//! verification commands named (`verification_command.failed_tests`) and
//! those the worker's own commands named (`session_closed.work
//! .failed_tests`), counted per test with the runs they failed in and when
//! they last did. A test that `integrate` saw fail in [`FLAKY_RUNS`] or
//! more runs is a flaky candidate, material for the observer: a worker's
//! own failures are mostly its work in progress (a test it has not made
//! pass yet), so they are counted but do not make a candidate.
use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

use crate::domain::{EventId, RunEvent, TaskId};

/// The runs whose `integrate` a test must fail in to be a flaky candidate.
pub const FLAKY_RUNS: usize = 2;

/// One test's failures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FailedTestStats {
    pub name: String,
    /// The commands that named it as failed.
    pub failures: usize,
    /// Those of `integrate`'s verification.
    pub integrate: usize,
    /// Those of the run's own sessions (worker, resume, revise).
    pub worker: usize,
    /// The runs they belong to.
    pub runs: usize,
    /// The runs whose `integrate` named it.
    pub integrate_runs: usize,
    /// The time of the last event that named it.
    pub last_failed_at: String,
}

/// The failed tests of the window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FailedTests {
    /// The runs a test must fail in to be a candidate ([`FLAKY_RUNS`]).
    pub flaky_runs: usize,
    /// Every test named, the most `integrate_runs` first, then the most
    /// runs, the most failures, and by name.
    pub tests: Vec<FailedTestStats>,
    /// The tests of `tests` that failed at `integrate` in `flaky_runs` runs
    /// or more.
    pub flaky_candidates: Vec<FailedTestStats>,
}

/// Where a failed test was named.
#[derive(Clone, Copy)]
enum Source {
    Integrate,
    Worker,
}

/// The names an event gives as failed: each name once per command.
fn named(event: &RunEvent) -> Option<(Source, Vec<&str>)> {
    let payload = &event.payload;
    let (source, names) = match event.kind.as_str() {
        "verification_command" if payload["phase"] == "integration" => {
            (Source::Integrate, &payload["failed_tests"])
        }
        "session_closed" => (Source::Worker, &payload["work"]["failed_tests"]),
        _ => return None,
    };
    let names: Vec<&str> = names.as_array()?.iter().filter_map(Value::as_str).collect();
    (!names.is_empty()).then_some((source, names))
}

/// The failed tests of the events with `after < id <= upto` whose task
/// `counts` accepts.
pub(super) fn failed_tests(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> FailedTests {
    #[derive(Default)]
    struct Acc<'a> {
        integrate: usize,
        worker: usize,
        runs: BTreeSet<&'a str>,
        integrate_runs: BTreeSet<&'a str>,
        last: &'a str,
    }
    let mut by_name: BTreeMap<&str, Acc> = BTreeMap::new();
    for event in events
        .iter()
        .filter(|event| event.id > after && event.id <= upto && counts(event.task_id))
    {
        let Some((source, names)) = named(event) else {
            continue;
        };
        for name in names {
            let acc = by_name.entry(name).or_default();
            match source {
                Source::Integrate => acc.integrate += 1,
                Source::Worker => acc.worker += 1,
            }
            if let Some(run) = &event.run_id {
                acc.runs.insert(run.as_str());
                if matches!(source, Source::Integrate) {
                    acc.integrate_runs.insert(run.as_str());
                }
            }
            acc.last = acc.last.max(event.created_at.as_str());
        }
    }
    let mut tests: Vec<FailedTestStats> = by_name
        .into_iter()
        .map(|(name, acc)| FailedTestStats {
            name: name.to_owned(),
            failures: acc.integrate + acc.worker,
            integrate: acc.integrate,
            worker: acc.worker,
            runs: acc.runs.len(),
            integrate_runs: acc.integrate_runs.len(),
            last_failed_at: acc.last.to_owned(),
        })
        .collect();
    tests.sort_by(|a, b| {
        b.integrate_runs
            .cmp(&a.integrate_runs)
            .then_with(|| b.runs.cmp(&a.runs))
            .then_with(|| b.failures.cmp(&a.failures))
            .then_with(|| a.name.cmp(&b.name))
    });
    let flaky_candidates = tests
        .iter()
        .filter(|test| test.integrate_runs >= FLAKY_RUNS)
        .cloned()
        .collect();
    FailedTests {
        flaky_runs: FLAKY_RUNS,
        tests,
        flaky_candidates,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::RunId;

    fn event(id: i64, run: &str, kind: &str, payload: Value, at: &str) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(id)),
            goal_id: None,
            run_id: Some(RunId::new(run).unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: format!("2026-09-27T00:00:{at}.000Z"),
        }
    }

    fn integrate(id: i64, run: &str, names: Value, at: &str) -> RunEvent {
        event(
            id,
            run,
            "verification_command",
            json!({"phase": "integration", "exit_code": 101, "failed_tests": names}),
            at,
        )
    }

    fn worker(id: i64, run: &str, names: Value, at: &str) -> RunEvent {
        event(
            id,
            run,
            "session_closed",
            json!({"kind": "worker", "work": {"total_secs": 1, "failed_tests": names}}),
            at,
        )
    }

    /// A test is counted per command that named it, by where and in how
    /// many runs; one that failed at `integrate` in two runs is a flaky
    /// candidate, one that failed twice in one run is not, nor one that
    /// failed in two runs' own sessions only.
    #[test]
    fn tests_are_counted_per_name_and_run() {
        let events = [
            integrate(1, "a", json!(["x::flaky", "y::broken"]), "01"),
            worker(2, "a", json!(["y::broken"]), "02"),
            worker(3, "a", json!(["y::broken"]), "03"),
            integrate(4, "b", json!(["x::flaky"]), "04"),
            // Passing commands, another phase and no names count nothing.
            integrate(5, "b", Value::Null, "05"),
            event(
                6,
                "b",
                "verification_command",
                json!({"phase": "recheck", "failed_tests": ["z::other"]}),
                "06",
            ),
            worker(7, "c", json!(["w::in_progress"]), "07"),
            worker(11, "a", json!(["w::in_progress"]), "07"),
            event(8, "c", "session_closed", json!({"kind": "worker"}), "08"),
            event(
                9,
                "c",
                "session_exited",
                json!({"work_breakdown": {"failed_tests": ["x::flaky"]}}),
                "09",
            ),
            // Outside the window.
            integrate(12, "c", json!(["x::flaky"]), "10"),
        ];
        let stats = failed_tests(&events, EventId::new(0), EventId::new(11), |_| true);
        assert_eq!(
            json!(stats),
            json!({
                "flaky_runs": 2,
                "tests": [
                    {"name": "x::flaky", "failures": 2, "integrate": 2, "worker": 0, "runs": 2,
                     "integrate_runs": 2, "last_failed_at": "2026-09-27T00:00:04.000Z"},
                    {"name": "y::broken", "failures": 3, "integrate": 1, "worker": 2, "runs": 1,
                     "integrate_runs": 1, "last_failed_at": "2026-09-27T00:00:03.000Z"},
                    {"name": "w::in_progress", "failures": 2, "integrate": 0, "worker": 2,
                     "runs": 2, "integrate_runs": 0,
                     "last_failed_at": "2026-09-27T00:00:07.000Z"},
                ],
                "flaky_candidates": [
                    {"name": "x::flaky", "failures": 2, "integrate": 2, "worker": 0, "runs": 2,
                     "integrate_runs": 2, "last_failed_at": "2026-09-27T00:00:04.000Z"},
                ],
            })
        );
        // The task filter of `--goal`.
        let none = failed_tests(&events, EventId::new(0), EventId::new(9), |task| {
            task == Some(TaskId::new(4))
        });
        assert_eq!(none.tests.len(), 1);
        assert!(none.flaky_candidates.is_empty());
    }
}
