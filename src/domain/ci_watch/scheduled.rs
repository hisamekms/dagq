//! The scheduled workflows `[ci_watch]` declares (`scheduled_workflows`,
//! ADR-t3807-1): which of their finished `schedule` and
//! `workflow_dispatch` runs on the watched branch a check records, and
//! whether one opens or resolves the workflow's finding. Their record is
//! [`CI_SCHEDULED_CHECKED`], apart from `ci_checked`, so they change
//! neither the list of the tests that fail already, nor the state green
//! or red, nor any hold ([`super::WatchState`] does not read them).

use std::collections::BTreeSet;

use serde_json::{Value, json};

use super::{CiRun, CiState, FailedJob, FailureFinding, classify, short_sha};
use crate::domain::RunEvent;
use crate::domain::event_kind::EventKind;

/// A finished run of a scheduled workflow was read (one per run ID and
/// attempt, whatever its conclusion).
pub const CI_SCHEDULED_CHECKED: &str = EventKind::CiScheduledChecked.as_str();
/// The kind of the finding a failed scheduled run records: apart from
/// `ci_failure`, whose items are the list of the tests that fail already
/// and whose findings are repairs outside the improvement limit
/// (ADR-t2928-1). This one is an improvement; only the watch records it
/// (`finding::check_recordable_kind`), since the watch resolves it.
pub const SCHEDULED_FINDING_KIND: &str = "ci_scheduled_failure";
/// The events whose runs of a scheduled workflow are read
/// (`gh run list --event`): its schedule and its manual dispatch.
pub const SCHEDULED_EVENTS: [&str; 2] = ["schedule", "workflow_dispatch"];

/// The subject of a scheduled workflow's finding: one per workflow, so its
/// failures in a row update one finding and a success after them
/// resolves it.
pub fn scheduled_subject(workflow: &str) -> String {
    format!("{SCHEDULED_FINDING_KIND}:{workflow}")
}

/// One scheduled workflow as its [`CI_SCHEDULED_CHECKED`] events leave
/// it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScheduledState {
    /// The run ID and attempt of every run recorded: never again.
    pub processed: BTreeSet<(i64, i64)>,
    /// The creation time and run ID of the first run recorded: no run
    /// created before it is read.
    pub floor: Option<(String, i64)>,
    /// The creation time and run ID of the latest settled run recorded
    /// (not late).
    pub head: Option<(String, i64)>,
    /// The finding the latest failed run recorded, while no success came
    /// after it: what the next success resolves.
    pub open_finding: Option<i64>,
    /// The ID of the latest [`CI_SCHEDULED_CHECKED`] of any workflow: a
    /// record decided after it is written only while it is still the
    /// latest.
    pub last_event: Option<i64>,
}

impl ScheduledState {
    /// Fold `events` (oldest first) for `workflow`; the events of the
    /// other workflows only move [`Self::last_event`]. A run recorded
    /// `late` is only marked processed.
    pub fn fold(events: &[RunEvent], workflow: &str) -> Self {
        let mut state = Self::default();
        for event in events {
            if event.kind != EventKind::CiScheduledChecked {
                continue;
            }
            state.last_event = Some(event.id.as_i64());
            let payload = &event.payload;
            if payload["workflow"].as_str() != Some(workflow) {
                continue;
            }
            let run_id = payload["run_id"].as_i64().unwrap_or_default();
            let attempt = payload["attempt"].as_i64().unwrap_or(1);
            state.processed.insert((run_id, attempt));
            let key = (
                payload["created_at"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                run_id,
            );
            if state.floor.as_ref().is_none_or(|floor| key < *floor) {
                state.floor = Some(key.clone());
            }
            // A late run and one that settles nothing (cancelled,
            // skipped) decide nothing: a re-run of the run before them is
            // still not late.
            match (payload["late"].as_bool(), payload["state"].as_str()) {
                (Some(true), _) | (_, None) => continue,
                (_, Some("red")) => state.open_finding = payload["finding_id"].as_i64(),
                (_, Some(_)) => state.open_finding = None,
            }
            if state.head.as_ref().is_none_or(|head| key >= *head) {
                state.head = Some(key);
            }
        }
        state
    }

    fn head(&self) -> Option<(&str, i64)> {
        self.head.as_ref().map(|(at, id)| (at.as_str(), *id))
    }

    /// Whether `run` was created before the latest recorded run (and is
    /// not a re-run of it): a later run already decided the finding.
    fn is_late(&self, run: &CiRun) -> bool {
        self.head()
            .is_some_and(|head| head.1 != run.run_id && run.key() < head)
    }
}

/// The runs of a scheduled workflow a check records, in the order they
/// were created: as [`super::runs_to_process`], each one whose run ID and
/// attempt no record holds, created no earlier than the first recorded;
/// only the newest at the workflow's first check, so declaring it does
/// not read back its history. Until a settled run is recorded, the first
/// recorded run (one that settled nothing) stands for the head, so it and
/// the runs recorded since are not read again.
pub fn scheduled_runs_to_process(runs: Vec<CiRun>, state: &ScheduledState) -> Vec<CiRun> {
    let head = state
        .head()
        .or_else(|| state.floor.as_ref().map(|(at, id)| (at.as_str(), *id)));
    super::unprocessed(runs, head, state.floor.as_ref(), &state.processed).0
}

/// What recording one finished run of a scheduled workflow writes.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduledDecision {
    /// The [`CI_SCHEDULED_CHECKED`] payload; the store adds `finding_id`.
    pub checked: Value,
    /// A failed run's finding, marked for a proposal.
    pub finding: Option<FailureFinding>,
    /// The finding a success resolves, with the reason.
    pub resolved: Option<(i64, String)>,
}

/// What `run` of `workflow` on `branch` records against `state`: a red
/// run (`failure`, `timed_out`) the finding of the workflow, with its
/// failed jobs and steps and its URL; a green run resolves the finding a
/// red one before it recorded; a run that settles nothing (cancelled,
/// skipped) and a run created before the latest recorded one only record
/// that they were read.
pub fn decide_scheduled(
    state: &ScheduledState,
    run: &CiRun,
    failed_jobs: &[FailedJob],
    workflow: &str,
    branch: &str,
) -> ScheduledDecision {
    let late = state.is_late(run);
    let settled = classify(&run.conclusion);
    let mut checked = json!({
        "workflow": workflow,
        "branch": branch,
        "run_id": run.run_id,
        "run_number": run.run_number,
        "attempt": run.attempt,
        "sha": run.sha,
        "url": run.url,
        "created_at": run.created_at,
        "conclusion": run.conclusion,
        "state": settled.map(CiState::as_str),
        "failed_jobs": failed_jobs,
    });
    if late {
        checked["late"] = json!(true);
        return ScheduledDecision {
            checked,
            finding: None,
            resolved: None,
        };
    }
    let finding = (settled == Some(CiState::Red)).then(|| FailureFinding {
        subject: scheduled_subject(workflow),
        summary: format!(
            "scheduled CI {workflow} on {branch} fails at {}",
            short_sha(&run.sha)
        ),
        detail: json!({
            "workflow": workflow,
            "branch": branch,
            "failed_jobs": failed_jobs,
            "url": run.url,
            "run": {"run_id": run.run_id, "attempt": run.attempt, "sha": run.sha,
                    "url": run.url, "created_at": run.created_at},
        }),
        propose: format!("the scheduled CI {workflow} on {branch} failed; a fix task is needed"),
    });
    let resolved = (settled == Some(CiState::Green))
        .then_some(state.open_finding)
        .flatten()
        .map(|id| {
            (
                id,
                format!(
                    "the scheduled CI {workflow} passed on {}",
                    short_sha(&run.sha)
                ),
            )
        });
    ScheduledDecision {
        checked,
        finding,
        resolved,
    }
}

/// What the store writes for one run of a scheduled workflow, in one
/// transaction.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScheduledCheckRecord {
    /// [`ScheduledState::last_event`] as the caller read it: another
    /// supervisor's record since then takes the run.
    pub previous: Option<i64>,
    pub checked: Value,
    /// Its evidence is the [`CI_SCHEDULED_CHECKED`] event, which names it
    /// back (`finding_id`).
    pub finding: Option<crate::domain::NewFinding>,
    /// Resolved only while `open` with no planner of the runtime's open
    /// for it, as [`super::CiCheckRecord::resolve`].
    pub resolve: Vec<(i64, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;

    fn run(id: i64, conclusion: &str) -> CiRun {
        CiRun {
            run_id: id,
            run_number: id,
            sha: format!("sha{id}"),
            conclusion: conclusion.into(),
            url: format!("https://github.com/o/n/actions/runs/{id}"),
            created_at: format!("2026-10-31T00:{id:02}:00Z"),
            attempt: 1,
        }
    }

    /// The [`CI_SCHEDULED_CHECKED`] event of `decision` for `run`, as the
    /// store writes it (with `finding_id` on a red one).
    fn recorded(id: i64, decision: &ScheduledDecision, finding_id: Option<i64>) -> RunEvent {
        let mut payload = decision.checked.clone();
        if let Some(finding) = finding_id {
            payload["finding_id"] = json!(finding);
        }
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: CI_SCHEDULED_CHECKED.into(),
            payload,
            created_at: "2026-10-31T01:00:00Z".into(),
            actor: None,
        }
    }

    /// The first check records the newest run only; a run is recorded
    /// once per attempt, a re-run's new attempt and one created after
    /// the head are read, and one before the first recorded is not.
    #[test]
    fn each_finished_run_is_recorded_once_from_the_newest_at_the_first_check() {
        let empty = ScheduledState::default();
        let first = scheduled_runs_to_process(vec![run(2, "success"), run(1, "failure")], &empty);
        assert_eq!(first, vec![run(2, "success")]);
        let decision = decide_scheduled(&empty, &first[0], &[], "nightly.yml", "main");
        let state = ScheduledState::fold(&[recorded(1, &decision, None)], "nightly.yml");
        assert_eq!(state.processed, BTreeSet::from([(2, 1)]));
        assert_eq!(state.last_event, Some(1));
        let mut again = run(2, "success");
        again.attempt = 2;
        let next = scheduled_runs_to_process(
            vec![
                run(1, "failure"),
                run(2, "success"),
                again.clone(),
                run(3, "cancelled"),
            ],
            &state,
        );
        assert_eq!(next, vec![again, run(3, "cancelled")]);
        // Another workflow's record moves only the latest event.
        let other = decide_scheduled(&empty, &run(9, "failure"), &[], "weekly.yml", "main");
        let both = ScheduledState::fold(
            &[recorded(1, &decision, None), recorded(2, &other, Some(7))],
            "nightly.yml",
        );
        assert_eq!(both.processed, state.processed);
        assert_eq!(both.open_finding, None);
        assert_eq!(both.last_event, Some(2));
    }

    /// A failed run opens the workflow's finding, marked for a proposal,
    /// with its failed jobs and steps and its URL; a cancelled run neither
    /// opens nor resolves; the next success resolves the finding the
    /// failure recorded, and a success with none open resolves nothing.
    #[test]
    fn a_failure_opens_the_finding_and_a_later_success_resolves_it() {
        let jobs = vec![FailedJob {
            job: "budget".into(),
            steps: vec!["measure".into()],
        }];
        let red = decide_scheduled(
            &ScheduledState::default(),
            &run(1, "failure"),
            &jobs,
            "it-budget.yml",
            "main",
        );
        assert_eq!(red.checked["state"], "red");
        assert_eq!(red.resolved, None);
        let finding = red.finding.clone().unwrap();
        assert_eq!(finding.subject, "ci_scheduled_failure:it-budget.yml");
        assert!(!finding.propose.is_empty());
        assert_eq!(
            finding.detail["failed_jobs"],
            json!([{"job": "budget", "steps": ["measure"]}])
        );
        assert_eq!(
            finding.detail["url"],
            "https://github.com/o/n/actions/runs/1"
        );
        let state = ScheduledState::fold(&[recorded(1, &red, Some(5))], "it-budget.yml");
        assert_eq!(state.open_finding, Some(5));

        let cancelled =
            decide_scheduled(&state, &run(2, "cancelled"), &[], "it-budget.yml", "main");
        assert_eq!(cancelled.checked["state"], Value::Null);
        assert_eq!((&cancelled.finding, &cancelled.resolved), (&None, &None));
        let state = ScheduledState::fold(
            &[recorded(1, &red, Some(5)), recorded(2, &cancelled, None)],
            "it-budget.yml",
        );
        assert_eq!(state.open_finding, Some(5));

        let green = decide_scheduled(&state, &run(3, "success"), &[], "it-budget.yml", "main");
        assert_eq!(green.finding, None);
        assert_eq!(green.resolved.as_ref().map(|(id, _)| *id), Some(5));
        let state = ScheduledState::fold(
            &[
                recorded(1, &red, Some(5)),
                recorded(2, &cancelled, None),
                recorded(3, &green, None),
            ],
            "it-budget.yml",
        );
        assert_eq!(state.open_finding, None);
        let green_again =
            decide_scheduled(&state, &run(4, "success"), &[], "it-budget.yml", "main");
        assert_eq!(green_again.resolved, None);
    }

    /// A cancelled run after a failure does not make the failed run's
    /// passing re-run late: the re-run resolves the finding.
    #[test]
    fn a_passing_re_run_after_a_cancelled_run_resolves_the_finding() {
        let red = decide_scheduled(
            &ScheduledState::default(),
            &run(5, "failure"),
            &[],
            "nightly.yml",
            "main",
        );
        let cancelled = decide_scheduled(
            &ScheduledState::fold(&[recorded(1, &red, Some(8))], "nightly.yml"),
            &run(6, "cancelled"),
            &[],
            "nightly.yml",
            "main",
        );
        let state = ScheduledState::fold(
            &[recorded(1, &red, Some(8)), recorded(2, &cancelled, None)],
            "nightly.yml",
        );
        let mut rerun = run(5, "success");
        rerun.attempt = 2;
        assert_eq!(
            scheduled_runs_to_process(vec![rerun.clone()], &state),
            vec![rerun.clone()]
        );
        let green = decide_scheduled(&state, &rerun, &[], "nightly.yml", "main");
        assert_eq!(green.checked.get("late"), None);
        assert_eq!(green.resolved.map(|(id, _)| id), Some(8));
    }

    /// A run that settles nothing, recorded first, is not read again at
    /// the next check with the same list, and the runs after it are.
    #[test]
    fn a_cancelled_first_run_is_recorded_once() {
        let empty = ScheduledState::default();
        let first = scheduled_runs_to_process(vec![run(3, "cancelled")], &empty);
        assert_eq!(first, vec![run(3, "cancelled")]);
        let cancelled = decide_scheduled(&empty, &first[0], &[], "nightly.yml", "main");
        let state = ScheduledState::fold(&[recorded(1, &cancelled, None)], "nightly.yml");
        assert_eq!(state.head, None);
        assert_eq!(
            scheduled_runs_to_process(vec![run(3, "cancelled")], &state),
            Vec::<CiRun>::new()
        );
        assert_eq!(
            scheduled_runs_to_process(
                vec![
                    run(2, "failure"),
                    run(3, "cancelled"),
                    run(4, "failure"),
                    run(5, "success")
                ],
                &state
            ),
            vec![run(4, "failure"), run(5, "success")]
        );
    }

    /// A run created before the latest recorded one is recorded `late` and
    /// neither opens nor resolves: the later run decided.
    #[test]
    fn a_run_that_ends_late_changes_no_finding() {
        let red = decide_scheduled(
            &ScheduledState::default(),
            &run(5, "failure"),
            &[],
            "nightly.yml",
            "main",
        );
        let state = ScheduledState::fold(&[recorded(1, &red, Some(8))], "nightly.yml");
        let late = decide_scheduled(&state, &run(4, "success"), &[], "nightly.yml", "main");
        assert_eq!(late.checked["late"], true);
        assert_eq!((&late.finding, &late.resolved), (&None, &None));
        let state = ScheduledState::fold(
            &[recorded(1, &red, Some(8)), recorded(2, &late, None)],
            "nightly.yml",
        );
        assert_eq!(state.open_finding, Some(8));
        assert!(state.processed.contains(&(4, 1)));
    }

    /// The scheduled records leave the push watch as it was: the list,
    /// the state and whether the watch recorded anything.
    #[test]
    fn scheduled_records_change_neither_the_list_nor_the_state() {
        let red = decide_scheduled(
            &ScheduledState::default(),
            &run(1, "failure"),
            &[FailedJob {
                job: "budget".into(),
                steps: Vec::new(),
            }],
            "nightly.yml",
            "main",
        );
        let watch = super::super::WatchState::fold(&[recorded(1, &red, Some(3))]);
        assert_eq!(watch, super::super::WatchState::default());
        assert!(
            !super::super::CI_WATCH_KINDS.contains(&EventKind::CiScheduledChecked),
            "the push watch's events are its own"
        );
    }
}
