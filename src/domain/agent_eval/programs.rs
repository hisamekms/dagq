//! The program reviews a case of an eval gets before its agent
//! (ADR-t1728-1 (i)): the same stage as a run's review (ADR-t1895-2), the
//! programs the landing branch's commit configures whose paths the case's
//! change touches ([`crate::domain::review_programs::select`]), run in
//! the configured order against the case's tree. A case one of them
//! rejects never reaches the agent and is left out of its scores; a
//! program that could not run, or ran past its time, leaves the case's
//! violations unknown, and its round incomplete. Everything here is
//! without side effects: the supervisor starts and waits for the programs.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::Case;

/// Why a program could not tell whether a case breaks its check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramFailure {
    /// It did not start: its configuration or script did not read, or its
    /// backend refused it (one not implemented, never the host instead).
    StartFailed,
    /// It ran past its time and was stopped.
    TimedOut,
}

impl ProgramFailure {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StartFailed => "start_failed",
            Self::TimedOut => "timed_out",
        }
    }

    fn read(text: &str) -> Option<Self> {
        [Self::StartFailed, Self::TimedOut]
            .into_iter()
            .find(|failure| failure.as_str() == text)
    }
}

/// What a case's programs said, once they all ran or one of them ended
/// the case's check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaseCheck {
    /// Every program the case needs exited 0 (or it needs none): its agent
    /// runs.
    Passed,
    /// `program` exited non-zero: the case stops there, as a run's review
    /// does, and its agent does not run.
    Stopped { program: String },
    /// `program` could not tell: the round is incomplete.
    Failed {
        program: String,
        failure: ProgramFailure,
    },
}

impl CaseCheck {
    /// `outcome` (`passed`, `stopped` or `failed`), `program` and `failure`
    /// of `agent_eval_case_checked`.
    pub fn record(&self) -> Value {
        match self {
            Self::Passed => json!({"outcome": "passed", "program": null, "failure": null}),
            Self::Stopped { program } => {
                json!({"outcome": "stopped", "program": program, "failure": null})
            }
            Self::Failed { program, failure } => {
                json!({"outcome": "failed", "program": program, "failure": failure.as_str()})
            }
        }
    }

    pub fn read(payload: &Value) -> Option<Self> {
        let program = || payload["program"].as_str().map(str::to_owned);
        match payload["outcome"].as_str()? {
            "passed" => Some(Self::Passed),
            "stopped" => Some(Self::Stopped {
                program: program()?,
            }),
            "failed" => Some(Self::Failed {
                program: program()?,
                failure: ProgramFailure::read(payload["failure"].as_str()?)?,
            }),
            _ => None,
        }
    }
}

/// What a case does next, given the programs it needs (`selected`, by
/// name in the configured order) and how those that ran ended, in order
/// (`ended`: `Some(true)` for exit 0, `Some(false)` for another exit,
/// `None` for one stopped past its time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Run this program next.
    Run(String),
    /// The check is over.
    Done(CaseCheck),
}

/// The next [`Step`] of a case: the first program not run yet while every
/// one that ran exited 0; the case stopped at the first that exited
/// non-zero, failed at the first that ran past its time, and passed once
/// every one exited 0. The programs after the one that ended the check
/// never run.
pub fn step(selected: &[String], ended: &[Option<bool>]) -> Step {
    for (program, end) in selected.iter().zip(ended) {
        match end {
            Some(true) => {}
            Some(false) => {
                return Step::Done(CaseCheck::Stopped {
                    program: program.clone(),
                });
            }
            None => {
                return Step::Done(CaseCheck::Failed {
                    program: program.clone(),
                    failure: ProgramFailure::TimedOut,
                });
            }
        }
    }
    selected
        .get(ended.len())
        .map_or(Step::Done(CaseCheck::Passed), |next| {
            Step::Run(next.clone())
        })
}

/// The cases the agent's scores count: `cases` without those a program
/// stopped, which the agent never saw.
pub fn scored_cases(cases: &[Case], checks: &BTreeMap<String, CaseCheck>) -> Vec<Case> {
    cases
        .iter()
        .filter(|case| !matches!(checks.get(&case.id), Some(CaseCheck::Stopped { .. })))
        .cloned()
        .collect()
}

/// `program_stopped` of `agent_eval_finished`: how many cases a program
/// stopped, and each one's id and the program, in `cases`' order.
pub fn stopped_record(cases: &[Case], checks: &BTreeMap<String, CaseCheck>) -> Value {
    let stopped: Vec<Value> = cases
        .iter()
        .filter_map(|case| match checks.get(&case.id) {
            Some(CaseCheck::Stopped { program }) => {
                Some(json!({"id": case.id, "program": program}))
            }
            _ => None,
        })
        .collect();
    json!({"count": stopped.len(), "cases": stopped})
}

/// Whether a program of the round could not tell about a case: the round
/// is then incomplete.
pub fn any_failed(checks: &BTreeMap<String, CaseCheck>) -> bool {
    checks
        .values()
        .any(|check| matches!(check, CaseCheck::Failed { .. }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::review_programs::{ReviewProgram, select};

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn program(name: &str, paths: &[&str]) -> ReviewProgram {
        ReviewProgram {
            name: name.to_owned(),
            script: format!("scripts/{name}.sh"),
            args: Vec::new(),
            paths: names(paths),
            timeout_secs: None,
        }
    }

    /// The programs a case needs are those whose paths its change touches,
    /// run in the configured order; when each exits 0 the case passes and
    /// its agent runs, and a program whose paths the change does not touch
    /// never runs.
    #[test]
    fn a_case_whose_programs_all_pass_goes_to_its_agent_and_an_untouched_program_never_runs() {
        let configured = [
            program("docs", &["docs/**"]),
            program("never", &["nothing/**"]),
            program("rust", &["src/**"]),
        ];
        let changed = names(&["src/a.rs", "docs/x.md"]);
        let selected: Vec<String> = select(&configured, &changed)
            .into_iter()
            .map(|selected| selected.program.name)
            .collect();
        assert_eq!(selected, ["docs", "rust"]);
        assert_eq!(step(&selected, &[]), Step::Run("docs".to_owned()));
        assert_eq!(step(&selected, &[Some(true)]), Step::Run("rust".to_owned()));
        assert_eq!(
            step(&selected, &[Some(true), Some(true)]),
            Step::Done(CaseCheck::Passed)
        );
        // A case none of whose programs is touched passes at once.
        assert_eq!(step(&[], &[]), Step::Done(CaseCheck::Passed));
    }

    /// The first program that exits non-zero stops the case, and none after
    /// it runs; one that ran past its time fails it.
    #[test]
    fn the_first_program_that_rejects_stops_the_case_and_a_timeout_fails_it() {
        let selected = names(&["fmt", "lint", "docs"]);
        assert_eq!(
            step(&selected, &[Some(true), Some(false)]),
            Step::Done(CaseCheck::Stopped {
                program: "lint".to_owned()
            })
        );
        assert_eq!(
            step(&selected, &[None]),
            Step::Done(CaseCheck::Failed {
                program: "fmt".to_owned(),
                failure: ProgramFailure::TimedOut
            })
        );
    }

    /// A stopped case is out of the agent's scores and named in the
    /// record; a failed check makes the round's failure known; each check
    /// reads back as recorded.
    #[test]
    fn a_stopped_case_is_left_out_of_the_scores_and_recorded_by_id_and_program() {
        let case = |id: &str| {
            let list = json!({"agent": "a", "role": "review", "codes": ["D-1"], "k": 1, "cases": [
                {"id": id, "source": "handmade", "made_by": "t",
                 "base_commit": "f46a7963cf708d621bf529eb855ecfb552f78b33",
                 "patch": "a".repeat(64),
                 "review": {"input": {}, "expected": {"verdict": "clean", "codes": []}}}
            ]});
            super::super::read_case_file(
                "a",
                "dev.json",
                &list.to_string(),
                &std::collections::BTreeSet::from(["a".repeat(64)]),
            )
            .unwrap()
            .cases
            .remove(0)
        };
        let cases = vec![case("one"), case("two"), case("three")];
        let checks = BTreeMap::from([
            ("one".to_owned(), CaseCheck::Passed),
            (
                "two".to_owned(),
                CaseCheck::Stopped {
                    program: "fmt".to_owned(),
                },
            ),
        ]);
        let scored: Vec<String> = scored_cases(&cases, &checks)
            .into_iter()
            .map(|case| case.id)
            .collect();
        assert_eq!(scored, ["one", "three"]);
        assert_eq!(
            stopped_record(&cases, &checks),
            json!({"count": 1, "cases": [{"id": "two", "program": "fmt"}]})
        );
        assert!(!any_failed(&checks));
        let failed = CaseCheck::Failed {
            program: "fmt".to_owned(),
            failure: ProgramFailure::StartFailed,
        };
        assert!(any_failed(&BTreeMap::from([(
            "x".to_owned(),
            failed.clone()
        )])));
        for check in [checks["one"].clone(), checks["two"].clone(), failed] {
            assert_eq!(CaseCheck::read(&check.record()), Some(check));
        }
    }
}
