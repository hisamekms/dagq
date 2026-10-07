//! What `candidates` and `graph`'s `candidates` show (ADR-t1992-1): the
//! claim order of the rule ([`crate::application::dependency_graph`]) less
//! the tasks whose claim the supervisor recorded as deferred, and those
//! tasks apart in `deferred`.
//!
//! The deferrals are read, not judged: each task's open `claim_deferred`
//! as the supervisor's `claimable` last recorded it (the same record as
//! `status`' `claim_deferrals`), a snapshot that the supervisor closes only
//! when it judges again. The supervisor does not judge while its slots are
//! full or its claims are held, so a deferral may stay open then; this
//! module neither closes nor adds one, and runs no `claim_defer::decide`,
//! hotspot or provider route of its own. The supervisor's own reads of the
//! candidates (`fill_slots`, the samples of `stats` and `kpi`, the
//! observer) do not go through here.

use anyhow::Result;
use serde::Serialize;

use crate::application::RunLog;
use crate::domain::TaskId;
use crate::domain::claim_defer::{DEFERRAL_KINDS, OpenDeferral};

/// Whether the shown order leaves out the deferred tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deferrals {
    /// The order as claimed: the deferred tasks are out of `candidates`.
    Excluded,
    /// The rule's order (`candidates --ignore-deferrals`): every candidate
    /// stays in place.
    Ignored,
}

/// The candidates as shown, and the deferred ones apart.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClaimView<T> {
    /// In the rule's order, without the deferred tasks unless they are
    /// [`Deferrals::Ignored`].
    pub candidates: Vec<T>,
    /// The open deferral of each candidate that has one, in the rule's
    /// order, with what the supervisor recorded (`reason`, `files`, `runs`,
    /// `since`, ...) as is.
    pub deferred: Vec<OpenDeferral>,
}

/// Each task's open deferral as the supervisor last recorded it.
pub fn open_deferrals(queue: &(impl RunLog + ?Sized)) -> Result<Vec<OpenDeferral>> {
    Ok(queue
        .latest_task_events(&DEFERRAL_KINDS)?
        .iter()
        .filter_map(OpenDeferral::of)
        .collect())
}

/// `ordered` (the candidates in the rule's order, each `id`) as shown with
/// the deferrals `open`: a deferral of a task that is no candidate here
/// (another goal's, or one the supervisor has yet to close) is not shown.
pub fn claim_view<T>(
    ordered: Vec<T>,
    id: impl Fn(&T) -> TaskId,
    open: Vec<OpenDeferral>,
    deferrals: Deferrals,
) -> ClaimView<T> {
    let mut open = open;
    let mut candidates = Vec::with_capacity(ordered.len());
    let mut deferred = Vec::new();
    for candidate in ordered {
        match open
            .iter()
            .position(|record| record.task_id == id(&candidate))
        {
            Some(index) => {
                deferred.push(open.swap_remove(index));
                if deferrals == Deferrals::Ignored {
                    candidates.push(candidate);
                }
            }
            None => candidates.push(candidate),
        }
    }
    ClaimView {
        candidates,
        deferred,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn deferral(task: i64, reason: &str) -> OpenDeferral {
        OpenDeferral {
            task_id: TaskId::new(task),
            reason: reason.to_owned(),
            since: "2026-10-06 22:40:00".to_owned(),
            files: json!(["src/main.rs"]),
            runs: json!([{"run_id": "r", "task_id": 9}]),
            supervisor: Some("tok".to_owned()),
            build: None,
            missing: None,
        }
    }

    fn ids(ids: &[i64]) -> Vec<TaskId> {
        ids.iter().copied().map(TaskId::new).collect()
    }

    #[test]
    fn the_deferred_tasks_leave_the_order_and_stand_apart_in_it() {
        let view = claim_view(
            ids(&[4, 1, 3, 2]),
            |id| *id,
            vec![
                deferral(3, "hot_files"),
                deferral(4, "provider_unavailable"),
            ],
            Deferrals::Excluded,
        );
        assert_eq!(view.candidates, ids(&[1, 2]));
        let deferred: Vec<_> = view
            .deferred
            .iter()
            .map(|d| (d.task_id, d.reason.as_str()))
            .collect();
        assert_eq!(
            deferred,
            [
                (TaskId::new(4), "provider_unavailable"),
                (TaskId::new(3), "hot_files")
            ]
        );
    }

    #[test]
    fn ignoring_the_deferrals_keeps_the_rule_s_order_and_still_shows_them() {
        let view = claim_view(
            ids(&[4, 1, 3]),
            |id| *id,
            vec![deferral(3, "hot_files")],
            Deferrals::Ignored,
        );
        assert_eq!(view.candidates, ids(&[4, 1, 3]));
        assert_eq!(view.deferred, [deferral(3, "hot_files")]);
    }

    #[test]
    fn a_deferral_of_a_task_that_is_no_candidate_here_is_not_shown() {
        let view = claim_view(
            ids(&[1]),
            |id| *id,
            vec![deferral(7, "hot_files")],
            Deferrals::Excluded,
        );
        assert_eq!(view.candidates, ids(&[1]));
        assert!(view.deferred.is_empty());
    }

    #[test]
    fn a_deferral_shows_what_the_supervisor_recorded_as_is() {
        let view = claim_view(
            ids(&[3]),
            |id| *id,
            vec![deferral(3, "not_in_build")],
            Deferrals::Excluded,
        );
        assert_eq!(
            serde_json::to_value(&view).unwrap(),
            json!({
                "candidates": [],
                "deferred": [{
                    "task_id": 3,
                    "reason": "not_in_build",
                    "since": "2026-10-06 22:40:00",
                    "files": ["src/main.rs"],
                    "runs": [{"run_id": "r", "task_id": 9}],
                    "supervisor": "tok",
                }],
            })
        );
    }
}
