//! The program reviews a case of an eval gets before its agent
//! (ADR-t1728-1 (i)): the same stage as a run's review (ADR-t1895-2), the
//! programs the landing branch's commit configures whose paths the case's
//! change touches ([`crate::domain::review_programs::select`]), run in
//! the configured order against the case's tree. A case's base is older
//! than the rules the commit's programs check, and may break them already;
//! a run's review starts from the landing branch, which does not (its CI
//! checks it). So each program runs first against the case's base tree,
//! and one that exits non-zero there is not run for the case (ADR-t2300-1):
//! the case is stopped only by a violation its change adds. A case one of
//! the others rejects never reaches the agent and is left out of its
//! scores; a program that could not run, or ran past its time, on either
//! tree leaves the case's violations unknown, and its round incomplete.
//! Everything here is without side effects: the supervisor starts and
//! waits for the programs.

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

/// The tree a program of a case runs against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Against {
    /// The case's base commit, without the change: a program that exits
    /// non-zero here is not run for the case.
    Base,
    /// The case's base with its change.
    Case,
}

/// What a case does next, given the programs it needs (`selected`, by
/// name in the configured order) and how those that ran ended, in order,
/// on the base's tree (`on_base`) and then on the case's (`on_case`):
/// `Some(true)` for exit 0, `Some(false)` for another exit, `None` for one
/// stopped past its time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Run this program next against this tree.
    Run(String, Against),
    /// The check is over.
    Done(CaseCheck),
}

/// The next [`Step`] of a case: every selected program against the base's
/// tree first, in order, the check failed at the first that ran past its
/// time there; then, against the case's tree, the first of those that
/// exited 0 on the base ([`kept`]) not run yet while every one that ran
/// exited 0; the case stopped at the first that exited non-zero, failed at
/// the first that ran past its time, and passed once every one exited 0.
/// The programs after the one that ended the check never run.
pub fn step(selected: &[String], on_base: &[Option<bool>], on_case: &[Option<bool>]) -> Step {
    for (program, end) in selected.iter().zip(on_base) {
        if end.is_none() {
            return Step::Done(CaseCheck::Failed {
                program: program.clone(),
                failure: ProgramFailure::TimedOut,
            });
        }
    }
    if let Some(next) = selected.get(on_base.len()) {
        return Step::Run(next.clone(), Against::Base);
    }
    let kept = kept(selected, on_base);
    for (program, end) in kept.iter().zip(on_case) {
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
    kept.get(on_case.len())
        .map_or(Step::Done(CaseCheck::Passed), |next| {
            Step::Run(next.clone(), Against::Case)
        })
}

/// The programs of `selected` run against the case's tree: those that
/// exited 0 on the base's (`on_base`, as in [`step`]), in order.
pub fn kept(selected: &[String], on_base: &[Option<bool>]) -> Vec<String> {
    selected
        .iter()
        .zip(on_base)
        .filter(|(_, end)| **end == Some(true))
        .map(|(program, _)| program.clone())
        .collect()
}

/// The programs of `selected` that exited non-zero on the base's tree, in
/// order: not run for the case (`base_failing` of
/// `agent_eval_case_checked`).
pub fn base_failing(selected: &[String], on_base: &[Option<bool>]) -> Vec<String> {
    selected
        .iter()
        .zip(on_base)
        .filter(|(_, end)| **end == Some(false))
        .map(|(program, _)| program.clone())
        .collect()
}

/// `base_failing` of an `agent_eval_case_checked`: empty when it names
/// none.
pub fn read_base_failing(payload: &Value) -> Vec<String> {
    payload["base_failing"]
        .as_array()
        .map(|names| {
            names
                .iter()
                .filter_map(|name| name.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
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

/// `program_base_failing` of `agent_eval_finished`: how many cases had a
/// program not run for them as it exited non-zero on their base, and each
/// one's id and those programs, in `cases`' order.
pub fn base_failing_record(cases: &[Case], base_failing: &BTreeMap<String, Vec<String>>) -> Value {
    let failing: Vec<Value> = cases
        .iter()
        .filter_map(|case| {
            base_failing
                .get(&case.id)
                .map(|programs| json!({"id": case.id, "programs": programs}))
        })
        .collect();
    json!({"count": failing.len(), "cases": failing})
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
            builtin: None,
        }
    }

    /// The programs a case needs are those whose paths its change touches,
    /// run in the configured order, each against the base's tree first and
    /// then against the case's; when each exits 0 the case passes and its
    /// agent runs, and a program whose paths the change does not touch
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
        let base = |name: &str| Step::Run(name.to_owned(), Against::Base);
        let case = |name: &str| Step::Run(name.to_owned(), Against::Case);
        assert_eq!(step(&selected, &[], &[]), base("docs"));
        assert_eq!(step(&selected, &[Some(true)], &[]), base("rust"));
        let on_base = [Some(true), Some(true)];
        assert_eq!(step(&selected, &on_base, &[]), case("docs"));
        assert_eq!(step(&selected, &on_base, &[Some(true)]), case("rust"));
        assert_eq!(
            step(&selected, &on_base, &[Some(true), Some(true)]),
            Step::Done(CaseCheck::Passed)
        );
        assert!(base_failing(&selected, &on_base).is_empty());
        // A case none of whose programs is touched passes at once.
        assert_eq!(step(&[], &[], &[]), Step::Done(CaseCheck::Passed));
    }

    /// On the case's tree, the first program that exits non-zero stops the
    /// case, and none after it runs; one that ran past its time on either
    /// tree fails it.
    #[test]
    fn the_first_program_that_rejects_stops_the_case_and_a_timeout_fails_it() {
        let selected = names(&["fmt", "lint", "docs"]);
        let on_base = [Some(true), Some(true), Some(true)];
        assert_eq!(
            step(&selected, &on_base, &[Some(true), Some(false)]),
            Step::Done(CaseCheck::Stopped {
                program: "lint".to_owned()
            })
        );
        assert_eq!(
            step(&selected, &on_base, &[None]),
            Step::Done(CaseCheck::Failed {
                program: "fmt".to_owned(),
                failure: ProgramFailure::TimedOut
            })
        );
        assert_eq!(
            step(&selected, &[Some(true), None], &[]),
            Step::Done(CaseCheck::Failed {
                program: "lint".to_owned(),
                failure: ProgramFailure::TimedOut
            })
        );
    }

    /// A program that exits non-zero on the case's base breaks its rule
    /// before the change: it does not run against the case's tree and
    /// never stops the case, which goes on with the others; one that
    /// passes on the base and rejects the case's tree stops it, a
    /// violation the change adds.
    #[test]
    fn a_program_that_fails_on_the_base_is_left_out_and_only_an_added_violation_stops_the_case() {
        let selected = names(&["frontmatter", "size", "lint"]);
        let on_base = [Some(false), Some(true), Some(false)];
        assert_eq!(kept(&selected, &on_base), ["size"]);
        assert_eq!(base_failing(&selected, &on_base), ["frontmatter", "lint"]);
        assert_eq!(
            step(&selected, &on_base, &[]),
            Step::Run("size".to_owned(), Against::Case)
        );
        assert_eq!(
            step(&selected, &on_base, &[Some(true)]),
            Step::Done(CaseCheck::Passed)
        );
        assert_eq!(
            step(&selected, &on_base, &[Some(false)]),
            Step::Done(CaseCheck::Stopped {
                program: "size".to_owned()
            })
        );
        // Every program failing on the base: the case goes to its agent.
        let all_failing = [Some(false), Some(false), Some(false)];
        assert_eq!(
            step(&selected, &all_failing, &[]),
            Step::Done(CaseCheck::Passed)
        );
        // The names read back from the check's record.
        let mut payload = CaseCheck::Passed.record();
        payload["base_failing"] = json!(base_failing(&selected, &on_base));
        assert_eq!(read_base_failing(&payload), ["frontmatter", "lint"]);
        assert!(read_base_failing(&CaseCheck::Passed.record()).is_empty());
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
        assert_eq!(
            base_failing_record(
                &cases,
                &BTreeMap::from([("three".to_owned(), names(&["frontmatter"]))])
            ),
            json!({"count": 1, "cases": [{"id": "three", "programs": ["frontmatter"]}]})
        );
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
