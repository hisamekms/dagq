//! The landing's program reviews (ADR-t3749-1): after the rebase, the
//! renumbering of a migration and the check of the task's paths, and
//! before the verification commands, the landing runs the review programs
//! of the landing branch's commit (the built-ins first) whose paths the
//! diff that lands (`main..rebased`) touches, one at a time to its end, as
//! the review stage runs them ([`run_program`]: the script of that commit,
//! the run's landing tree as the working directory, the narrowed
//! environment and the limit). They are the cheap gate in front of the
//! heavy one: the first that exits non-zero sends the run back to its
//! worker before any command runs.

use anyhow::Result;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::review_programs::{
    ProgramBackends, ProgramRun, SnapshotProgram, program_scratch, programs_at, programs_for_range,
    run_program,
};
use super::super::{Clock, Queue, Repository, RunFiles, Spawner, Verifier};
use super::{HandoffDrain, Verdict, drains_for_handoff};
use crate::domain::{ActorRole, CommitSha, EventKind, Reason, ReasonCode, RunEvent, TaskRun};

/// What a landing runs its program reviews with: the backends of the
/// review's actor (fail closed: one nothing can run on runs nothing, never
/// the host instead, ADR-t1895-2 decision 5), what starts their
/// processes, the queue's `review-programs/` its scripts are written
/// under ([`program_scratch`], outside what the worker can write), and the
/// limit of a program without its own `timeout_secs`
/// (`[review.jobs] program_timeout_secs`, or the reviewer's).
pub struct LandingPrograms<'a> {
    pub backends: ProgramBackends<'a>,
    pub spawner: &'a dyn Spawner,
    pub scratch: &'a Path,
    pub timeout: Duration,
}

/// Where a landing's program reviews run and what the verdict says of the
/// run: its landing tree and run directory, and `main..rebased`, the diff
/// that lands.
pub(super) struct ProgramsAt<'r> {
    pub run: &'r TaskRun,
    pub worktree: &'r Path,
    pub run_dir: &'r Path,
    pub onto: &'r str,
    pub main: &'r CommitSha,
    pub rebased: &'r CommitSha,
}

/// The `result` of a program run in `integrate_programs_finished`.
fn result_of(ran: &ProgramRun) -> &'static str {
    match ran {
        ProgramRun::Passed(_) => "passed",
        ProgramRun::Rejected { .. } => "failed",
        ProgramRun::StartFailed(_) => "start_failed",
        ProgramRun::TimedOut { .. } => "timed_out",
    }
}

/// Whether a program run is the host's failure, not the worker's
/// (ADR-t1895-2 decision 4): it could not start or ran past its time.
const fn is_environmental(ran: &ProgramRun) -> bool {
    matches!(
        ran,
        ProgramRun::StartFailed(_) | ProgramRun::TimedOut { .. }
    )
}

/// Why a program run that is the host's failure did not pass.
fn environmental_why(name: &str, ran: &ProgramRun) -> String {
    match ran {
        ProgramRun::StartFailed(why) => why.clone(),
        ProgramRun::TimedOut { timeout, .. } => format!(
            "the review program {name} ran past its {}s limit and was stopped",
            timeout.as_secs()
        ),
        _ => String::new(),
    }
}

/// How a landing's program reviews ended ([`run_all`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ProgramsEnd {
    /// Every program exited 0: the verification commands run.
    Passed,
    /// A program exited non-zero: `reason` names it, its exit and the end
    /// of its output, and the run goes back to its worker.
    Rejected { reason: String },
    /// A program could not start or ran past its time, again when run once
    /// more: the run waits for a person.
    Held { program: String, why: String },
    /// A program could not start or ran past its time while the supervisor
    /// drains for a handoff: not run again, the landing is handed over.
    HandedOver { program: String, why: String },
}

impl ProgramsEnd {
    /// Its `outcome` in `integrate_programs_finished`.
    const fn outcome(&self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Rejected { .. } => "failed",
            Self::Held { .. } => "held",
            Self::HandedOver { .. } => "handed_over",
        }
    }
}

/// One run of a program: which, how it ended, how long it took, and the
/// stem of its output files.
pub(super) struct Ran {
    pub name: String,
    pub run: ProgramRun,
    pub took: Duration,
    pub stem: String,
    pub retry: bool,
}

/// Run `programs` in their order with `run` (the program, its index from
/// 1 and whether it is the run once more), stopping at the first that
/// exits non-zero (ADR-t1895-2 decision 3). One that could not start or
/// ran past its time is run once more in the same attempt, unless
/// `draining` (the supervisor drains for a handoff, ADR-t3344-1), and held
/// when it fails so again (decision 4, as a verification command that
/// failed on the host).
pub(super) fn run_all(
    programs: &[SnapshotProgram],
    run: &mut dyn FnMut(&SnapshotProgram, usize, bool) -> Ran,
    draining: &dyn Fn() -> bool,
) -> (Vec<Ran>, ProgramsEnd) {
    let mut ran = Vec::new();
    for (index, program) in programs.iter().enumerate() {
        let name = program.program.name.clone();
        let mut last = run(program, index + 1, false);
        if is_environmental(&last.run) {
            let why = environmental_why(&name, &last.run);
            ran.push(last);
            if draining() {
                return (ran, ProgramsEnd::HandedOver { program: name, why });
            }
            last = run(program, index + 1, true);
            if is_environmental(&last.run) {
                let why = environmental_why(&name, &last.run);
                ran.push(last);
                return (ran, ProgramsEnd::Held { program: name, why });
            }
        }
        let rejected = match &last.run {
            ProgramRun::Rejected { reason, .. } => Some(reason.clone()),
            _ => None,
        };
        ran.push(last);
        if let Some(reason) = rejected {
            return (ran, ProgramsEnd::Rejected { reason });
        }
    }
    (ran, ProgramsEnd::Passed)
}

/// Whether the programs of an earlier landing of the run all passed on
/// `head` (`integrate_programs_finished` with `outcome` `passed`): a
/// verification done again on the same rebased head (a flaky retry, a
/// handoff's next landing) does not run them again. One held, failed or
/// handed over on it runs them anew.
pub(super) fn passed_before(events: &[RunEvent], head: &CommitSha) -> bool {
    events.iter().any(|event| {
        event.kind == EventKind::IntegrateProgramsFinished
            && event.payload["outcome"] == "passed"
            && event.payload["head"] == head.as_str()
    })
}

/// The payload of `integrate_programs_finished` of programs the backend
/// of the review's actor cannot run (ADR-t1895-2 decision 5): none ran,
/// none on the host instead, and the verification commands go on.
/// `None` when it can run them.
pub(super) fn backend_unavailable(backends: &ProgramBackends<'_>) -> Option<Value> {
    let error = backends.backend().err()?;
    Some(json!({
        "outcome": "skipped",
        "reason": "backend_unavailable",
        "backend": backends.config.backend_of(ActorRole::ReviewJob).as_str(),
        "error": format!("{error:#}"),
    }))
}

/// The output files of the program run named by `stem` in `run_dir`.
fn logs(run_dir: &Path, stem: &str) -> (PathBuf, PathBuf) {
    (
        run_dir.join(format!("{stem}.out")),
        run_dir.join(format!("{stem}.err")),
    )
}

/// The `programs` of `integrate_programs_finished`.
fn ran_json(ran: &[Ran], run_dir: &Path) -> Value {
    ran.iter()
        .map(|ran| {
            let (stdout, stderr) = logs(run_dir, &ran.stem);
            json!({
                "name": ran.name,
                "result": result_of(&ran.run),
                "retry": ran.retry,
                "duration_secs": ran.took.as_secs_f64(),
                "log": {"stdout": stdout, "stderr": stderr},
            })
        })
        .collect()
}

/// The landing's program reviews of `at` (ADR-t3749-1): `Ok(None)` when
/// the verification commands go on (every program passed, none is
/// required, they passed on this head before, or the review's backend
/// cannot run them), `Ok(Some(verdict))` when the run goes back to its
/// worker, waits for a person or is handed over. Each attempt that ran or
/// skipped any records one `integrate_programs_finished`; an attempt that
/// needs none records nothing.
#[allow(clippy::too_many_arguments)]
pub(super) fn landing_programs(
    queue: &mut dyn Queue,
    repository: &dyn Repository,
    verifier: &dyn Verifier,
    (files, clock): (&dyn RunFiles, &dyn Clock),
    programs: &LandingPrograms<'_>,
    handing_off: Option<HandoffDrain<'_>>,
    at: &ProgramsAt<'_>,
) -> Result<Option<Verdict>> {
    let (main, rebased) = (at.main, at.rebased);
    let events = queue.run_events(at.run.id())?;
    if passed_before(&events, rebased) {
        return Ok(None);
    }
    let round = events
        .iter()
        .filter(|event| event.kind == EventKind::IntegrateProgramsFinished)
        .count()
        + 1;
    let record = |queue: &mut dyn Queue, mut payload: Value| -> Result<()> {
        payload["main"] = json!(main);
        payload["head"] = json!(rebased);
        payload["round"] = json!(round);
        queue.record_runtime_event(at.run.id(), EventKind::IntegrateProgramsFinished, payload)
    };
    // The programs and their scripts of the landing branch's commit, which
    // is main now (ADR-t1895-2 decision 2), selected by the diff that
    // lands.
    let read = || -> Result<Vec<SnapshotProgram>> {
        let configured = programs_at(repository, main.as_str(), &|text| {
            verifier.review_programs_in(text)
        })?
        .effective();
        if configured.is_empty() {
            return Ok(Vec::new());
        }
        programs_for_range(
            repository,
            main.as_str(),
            &configured,
            (main.as_str(), rebased.as_str()),
        )
    };
    let selected = match read() {
        Ok(selected) => selected,
        // What to run cannot be known: not the worker's (decision 4), read
        // once more as a program is run once more.
        Err(first) => {
            let end = if drains_for_handoff(handing_off) {
                Err(ProgramsEnd::HandedOver {
                    program: String::new(),
                    why: format!("{first:#}"),
                })
            } else {
                read().map_err(|again| ProgramsEnd::Held {
                    program: String::new(),
                    why: format!("{again:#}"),
                })
            };
            match end {
                Ok(selected) => selected,
                Err(end) => {
                    record(
                        queue,
                        json!({"outcome": end.outcome(), "programs": [], "error": why_of(&end)}),
                    )?;
                    return Ok(Some(verdict(at, &end)));
                }
            }
        }
    };
    if selected.is_empty() {
        return Ok(None);
    }
    if let Some(skipped) = backend_unavailable(&programs.backends) {
        tracing::warn!(
            op = "integrate",
            "run {}: the review programs of its landing were not run: {}; the verification commands go on",
            at.run.id(),
            skipped["error"]
        );
        record(queue, skipped)?;
        return Ok(None);
    }
    let scratch = program_scratch(programs.scratch, at.run.id());
    // A container run's landing tree, with its git guarded (ADR-t2179-1
    // decision 8).
    let env = super::super::run_git::tree_env(repository, at.run, at.worktree)?;
    let mut run = |program: &SnapshotProgram, index: usize, retry: bool| {
        let stem = format!(
            "integrate-program-{round}-{index}{}",
            if retry { "-retry" } else { "" }
        );
        let since = clock.monotonic();
        let ended = run_program(
            (programs.spawner, &programs.backends, files, clock),
            program,
            (at.worktree, &env),
            (at.run_dir, &scratch, &stem),
            programs.timeout,
        );
        Ran {
            name: program.program.name.clone(),
            run: ended,
            took: clock.since(since),
            stem,
            retry,
        }
    };
    let (ran, end) = run_all(&selected, &mut run, &|| drains_for_handoff(handing_off));
    let mut payload = json!({"outcome": end.outcome(), "programs": ran_json(&ran, at.run_dir)});
    if let Some(why) = why_of(&end) {
        payload["error"] = json!(why);
    }
    record(queue, payload)?;
    Ok(match end {
        ProgramsEnd::Passed => None,
        end => Some(verdict_with_logs(at, &end, ran.last())),
    })
}

/// What a held or handed-over end says went wrong.
fn why_of(end: &ProgramsEnd) -> Option<String> {
    match end {
        ProgramsEnd::Held { why, .. } | ProgramsEnd::HandedOver { why, .. } => Some(why.clone()),
        _ => None,
    }
}

/// The verdict of `end` with no program's output to point at.
fn verdict(at: &ProgramsAt<'_>, end: &ProgramsEnd) -> Verdict {
    verdict_with_logs(at, end, None)
}

/// The verdict on the run of a landing whose programs ended as `end`
/// (not [`ProgramsEnd::Passed`]), `last` the program run it ended at.
fn verdict_with_logs(at: &ProgramsAt<'_>, end: &ProgramsEnd, last: Option<&Ran>) -> Verdict {
    let (main, head, onto) = (at.main, at.rebased, at.onto);
    let (stdout, stderr) = last.map_or((None, None), |ran| {
        let (stdout, stderr) = logs(at.run_dir, &ran.stem);
        (Some(stdout), Some(stderr))
    });
    let detail = json!({
        "main": main,
        "head": head,
        "program": last.map(|ran| ran.name.clone()),
        "log": {"stdout": stdout, "stderr": stderr},
    });
    match end {
        ProgramsEnd::Rejected { reason } => Verdict::Deferred {
            reason: format!(
                "{reason}\n(the landing ran it after the rebase onto {onto} {main}, before the verification commands, which did not run; fix the change, commit, and rewrite the receipt with the new head)"
            ),
            detail: Reason::new(ReasonCode::VerificationFailed).on(detail),
        },
        ProgramsEnd::Held { why, .. } => Verdict::Held {
            reason: format!(
                "{why} after the rebase onto {onto} {main}, again when run once more; see {}. Fix the host or the review programs of {onto}, then land it with dagq integrate {}; no session is resumed",
                stderr.map_or_else(
                    || "the queue's events".to_owned(),
                    |path| path.display().to_string()
                ),
                at.run.task_id()
            ),
            detail: Reason::new(ReasonCode::VerificationEnvironment).on(detail),
        },
        ProgramsEnd::HandedOver { why, .. } => Verdict::HandedOver {
            reason: format!(
                "{why} after the rebase onto {onto} {main}. It would be run once more, but the supervisor drains for a handoff: the landing is handed over, its lease kept, and the next process lands it again"
            ),
            detail,
        },
        ProgramsEnd::Passed => unreachable!("a landing whose programs passed has no verdict"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::execution::{ExecutionConfig, ExecutorBackend};
    use crate::application::review_programs::ProgramEnd;
    use crate::domain::review_programs::ReviewProgram;
    use crate::domain::{EventId, RunId};

    fn program(name: &str) -> SnapshotProgram {
        SnapshotProgram {
            program: ReviewProgram {
                name: name.to_owned(),
                script: format!("scripts/{name}.sh"),
                args: Vec::new(),
                paths: vec!["**".to_owned()],
                timeout_secs: None,
                builtin: None,
            },
            matched: vec!["a".to_owned()],
            script: "#!/bin/sh\n".to_owned(),
        }
    }

    fn end() -> ProgramEnd {
        ProgramEnd {
            exit: None,
            stdout_tail: String::new(),
            stderr_tail: "not formatted\n".to_owned(),
        }
    }

    fn passed() -> ProgramRun {
        ProgramRun::Passed(end())
    }

    fn rejected(name: &str) -> ProgramRun {
        ProgramRun::Rejected {
            reason: format!("the review program {name} rejected the change"),
            end: end(),
        }
    }

    fn timed_out() -> ProgramRun {
        ProgramRun::TimedOut {
            timeout: Duration::from_secs(10),
            end: end(),
        }
    }

    /// Run `programs` with each run's end taken from `ends` in order,
    /// returning the runs made (name, retry) and the end.
    fn runs(
        programs: &[&str],
        ends: Vec<ProgramRun>,
        draining: bool,
    ) -> (Vec<(String, bool, &'static str)>, ProgramsEnd) {
        let programs: Vec<_> = programs.iter().map(|name| program(name)).collect();
        let mut ends = ends.into_iter();
        let mut run = |program: &SnapshotProgram, index: usize, retry: bool| Ran {
            name: program.program.name.clone(),
            run: ends.next().expect("a run past the ends given"),
            took: Duration::ZERO,
            stem: format!("integrate-program-1-{index}"),
            retry,
        };
        let (ran, end) = run_all(&programs, &mut run, &|| draining);
        (
            ran.iter()
                .map(|ran| (ran.name.clone(), ran.retry, result_of(&ran.run)))
                .collect(),
            end,
        )
    }

    /// Every program runs in its order when each exits 0.
    #[test]
    fn every_program_runs_in_order_when_each_passes() {
        let (ran, end) = runs(
            &["builtin:secrets", "docs"],
            vec![passed(), passed()],
            false,
        );
        assert_eq!(end, ProgramsEnd::Passed);
        assert_eq!(
            ran,
            [
                ("builtin:secrets".to_owned(), false, "passed"),
                ("docs".to_owned(), false, "passed")
            ]
        );
    }

    /// The first that exits non-zero sends the run back with its reason,
    /// and no later program runs.
    #[test]
    fn the_first_program_that_fails_stops_the_rest_and_goes_back_to_the_worker() {
        let (ran, end) = runs(&["docs", "links"], vec![rejected("docs")], false);
        assert_eq!(
            end,
            ProgramsEnd::Rejected {
                reason: "the review program docs rejected the change".to_owned()
            }
        );
        assert_eq!(ran, [("docs".to_owned(), false, "failed")]);
    }

    /// One that could not start or ran past its time is run once more:
    /// passing then, the next runs; failing so again, the run is held and
    /// no later program runs. A failure of the code on the run once more is
    /// the worker's.
    #[test]
    fn a_program_that_could_not_start_or_timed_out_is_run_once_more_then_held() {
        let failed =
            ProgramRun::StartFailed("the review program docs could not start: gone".to_owned());
        let (ran, end) = runs(
            &["docs", "links"],
            vec![failed.clone(), passed(), passed()],
            false,
        );
        assert_eq!(end, ProgramsEnd::Passed);
        assert_eq!(
            ran,
            [
                ("docs".to_owned(), false, "start_failed"),
                ("docs".to_owned(), true, "passed"),
                ("links".to_owned(), false, "passed")
            ]
        );
        let (ran, end) = runs(&["docs", "links"], vec![timed_out(), timed_out()], false);
        assert_eq!(
            end,
            ProgramsEnd::Held {
                program: "docs".to_owned(),
                why: "the review program docs ran past its 10s limit and was stopped".to_owned()
            }
        );
        assert_eq!(ran.len(), 2);
        let (_, end) = runs(&["docs"], vec![failed.clone(), failed], false);
        assert!(matches!(end, ProgramsEnd::Held { why, .. } if why.contains("could not start")));
        let (_, end) = runs(&["docs"], vec![timed_out(), rejected("docs")], false);
        assert!(matches!(end, ProgramsEnd::Rejected { .. }));
    }

    /// While the supervisor drains for a handoff, one that could not start
    /// or timed out is not run again: the landing is handed over.
    #[test]
    fn a_handoffs_drain_hands_the_landing_over_instead_of_running_it_again() {
        let (ran, end) = runs(&["docs"], vec![timed_out()], true);
        assert!(matches!(end, ProgramsEnd::HandedOver { program, .. } if program == "docs"));
        assert_eq!(ran, [("docs".to_owned(), false, "timed_out")]);
        // A pass or a failure of the code does not look at the drain.
        let (_, end) = runs(&["docs"], vec![rejected("docs")], true);
        assert!(matches!(end, ProgramsEnd::Rejected { .. }));
    }

    fn event(payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: Some(RunId::new("00000000-0000-4000-8000-000000000001").unwrap()),
            kind: EventKind::IntegrateProgramsFinished.into(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    /// Programs that all passed on the head are not run again on it; any
    /// other end on it, or a pass on another head, runs them anew.
    #[test]
    fn programs_run_again_unless_they_all_passed_on_the_same_head() {
        let head = CommitSha::parse("2222222222222222222222222222222222222222", "head").unwrap();
        let other = "3333333333333333333333333333333333333333";
        assert!(!passed_before(&[], &head));
        assert!(passed_before(
            &[event(json!({"outcome": "passed", "head": head}))],
            &head
        ));
        for (outcome, at) in [
            ("held", head.as_str()),
            ("failed", head.as_str()),
            ("handed_over", head.as_str()),
            ("skipped", head.as_str()),
            ("passed", other),
        ] {
            assert!(
                !passed_before(&[event(json!({"outcome": outcome, "head": at}))], &head),
                "{outcome} {at}"
            );
        }
    }

    struct Never;

    impl crate::application::ReviewProgramBackend for Never {
        fn command(
            &self,
            _: &SnapshotProgram,
            _: &Path,
            _: &Path,
            _: &Path,
        ) -> Result<crate::application::CommandSpec> {
            panic!("no program runs on the host instead")
        }
    }

    /// With the review's actor on a backend nothing runs a program on (the
    /// container), the landing's programs are skipped as
    /// `backend_unavailable` naming it, and the host is never asked; on
    /// the host they run.
    #[test]
    fn programs_the_reviews_backend_cannot_run_are_skipped_naming_it() {
        let container = ExecutionConfig {
            backend: ExecutorBackend::Host,
            actors: vec![(ActorRole::ReviewJob, ExecutorBackend::Container)],
        };
        let skipped = backend_unavailable(&ProgramBackends::new(&Never, container)).unwrap();
        assert_eq!(skipped["outcome"], "skipped");
        assert_eq!(skipped["reason"], "backend_unavailable");
        assert_eq!(skipped["backend"], "container");
        assert!(
            backend_unavailable(&ProgramBackends::new(&Never, ExecutionConfig::default()))
                .is_none()
        );
    }
}
