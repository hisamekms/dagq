//! What `candidates` and `graph`'s `candidates` show (ADR-t1992-1): the
//! claim order of the rule ([`crate::application::dependency_graph`]) less
//! the tasks whose claim the supervisor recorded as deferred, and those
//! tasks apart in `deferred`; the rest arranged by the aging across the
//! execution classes as the supervisor's claim arranges them
//! ([`claim_order::arrange`], ADR-t1489-1), those it moved ahead in
//! `aged`. A deferral for a class limit (`spike_limit`) or for the
//! investigation limit (`investigation_limit`, ADR-t1487-1 decision 7) is
//! one of the records: neither limit is judged here. The supervisor records
//! a Spike under the Spike limit's reason while that limit holds every
//! Spike, and under the investigation limit's once it lets Spikes go, so a
//! Spike shows the reason that holds it now; either way it is out of the
//! shown order as it is out of the claim.
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

use crate::domain::EventKind;
use anyhow::Result;
use serde::Serialize;

use std::collections::HashSet;

use crate::application::RunLog;
use crate::domain::TaskId;
use crate::domain::claim_defer::{DEFERRAL_KINDS, OpenDeferral};
use crate::domain::claim_order::{self, ClassLimits};
use crate::domain::slot_limits::SupervisorConfig;

/// Whether the shown order leaves out the deferred tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deferrals {
    /// The order as claimed: the deferred tasks are out of `candidates`.
    Excluded,
    /// The rule's order (`candidates --ignore-deferrals`): every candidate
    /// stays in place.
    Ignored,
}

/// What the aging of the shown order compares: every candidate of the
/// queue in the rule's order (not only a goal's: the head is the queue's),
/// the threshold in seconds and the time now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaimAging {
    pub candidates: Vec<claim_order::Candidate>,
    pub secs: usize,
    pub now_ms: i64,
}

/// What the threshold of the shown aging is chosen from
/// ([`class_aging_secs`]).
#[derive(Debug, Default)]
pub struct AgingInputs {
    /// The read of `[supervisor]` of the `dagq.toml` the supervisors read,
    /// as it came: `None` for a queue bound to no checkout, `Ok(None)` for
    /// no file, an error for a file that cannot be read.
    pub file: Option<Result<Option<SupervisorConfig>>>,
    /// Each registration's `class_aging`, oldest first.
    pub registered: Vec<Option<u32>>,
}

/// The threshold the supervisors age by, as the command line can tell it:
/// `class_aging` has no flag, so a supervisor uses the file's value, else
/// the default, read again each pass, and keeps the value in use while
/// there is no file or it cannot be read. So when `file` was read, its
/// value, else the default; when it was not (no checkout, no file, an
/// error), the latest registration's value (what a supervisor last used),
/// else the default.
pub fn class_aging_secs(
    file: Option<&Result<Option<SupervisorConfig>>>,
    registered: &[Option<u32>],
) -> usize {
    let default = claim_order::DEFAULT_CLASS_AGING_SECS;
    match file {
        Some(Ok(Some(config))) => config.class_aging.unwrap_or(default),
        _ => registered
            .iter()
            .rev()
            .find_map(|value| value.and_then(|secs| usize::try_from(secs).ok()))
            .unwrap_or(default),
    }
}

/// The candidates as shown, and the deferred ones apart.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClaimView<T> {
    /// In the order a claim takes them, without the deferred tasks; with
    /// [`Deferrals::Ignored`], every candidate in the rule's order.
    pub candidates: Vec<T>,
    /// The shown candidates aging moved ahead within their effective
    /// priority, in the shown order; never with [`Deferrals::Ignored`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aged: Vec<TaskId>,
    /// The open deferral of each candidate that has one, in the rule's
    /// order, with what the supervisor recorded (`reason`, `files`, `runs`,
    /// `since`, ...) as is.
    pub deferred: Vec<OpenDeferral>,
}

/// Each task's open deferral as the supervisor last recorded it.
pub fn open_deferrals(queue: &(impl RunLog + ?Sized)) -> Result<Vec<OpenDeferral>> {
    Ok(queue
        .latest_task_events(&DEFERRAL_KINDS.map(EventKind::as_str))?
        .iter()
        .filter_map(OpenDeferral::of)
        .collect())
}

/// `ordered` (the candidates in the rule's order, each `id`) as shown with
/// the deferrals `open` and `aging`: a deferral of a task that is no
/// candidate here (another goal's, or one the supervisor has yet to close)
/// is not shown, but leaves the aging's order all the same.
pub fn claim_view<T>(
    ordered: Vec<T>,
    id: impl Fn(&T) -> TaskId,
    open: Vec<OpenDeferral>,
    deferrals: Deferrals,
    aging: &ClaimAging,
) -> ClaimView<T> {
    let excluded: HashSet<TaskId> = open.iter().map(|record| record.task_id).collect();
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
    let mut aged = Vec::new();
    if deferrals == Deferrals::Excluded {
        let arranged = claim_order::arrange(
            &aging.candidates,
            &excluded,
            ClassLimits::default(),
            aging.now_ms,
            aging.secs,
        );
        let place = |candidate: &T| {
            arranged
                .order
                .iter()
                .position(|task| *task == id(candidate))
                .unwrap_or(usize::MAX)
        };
        candidates.sort_by_key(place);
        aged = candidates
            .iter()
            .map(&id)
            .filter(|task| arranged.aged.contains(task))
            .collect();
    }
    ClaimView {
        candidates,
        aged,
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
            &ClaimAging::default(),
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
            &ClaimAging::default(),
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
            &ClaimAging::default(),
        );
        assert_eq!(view.candidates, ids(&[1]));
        assert!(view.deferred.is_empty());
    }

    /// The shown order is the claim's: the deferred Spike (here, at the
    /// class limit) is out and not aged, the Spike left past the threshold
    /// goes first within its priority. A goal's view without the aged task
    /// shows none, and the rule's order shows no aging.
    #[test]
    fn the_shown_order_is_arranged_by_the_aging_as_the_claim_arranges_it() {
        use crate::domain::{ExecutionClass, Priority};
        let candidate = |id, class, waited_min: i64| claim_order::Candidate {
            id: TaskId::new(id),
            class,
            priority: Priority::Normal,
            ready_at_ms: Some(3_600_000 - waited_min * 60_000),
        };
        let aging = ClaimAging {
            candidates: vec![
                candidate(1, ExecutionClass::Implementation, 1),
                candidate(2, ExecutionClass::Spike, 50),
                candidate(3, ExecutionClass::Spike, 40),
                candidate(4, ExecutionClass::Implementation, 1),
            ],
            secs: 600,
            now_ms: 3_600_000,
        };
        let open = vec![deferral(2, claim_order::SPIKE_LIMIT)];
        let view = claim_view(
            ids(&[1, 2, 3, 4]),
            |id| *id,
            open.clone(),
            Deferrals::Excluded,
            &aging,
        );
        assert_eq!(view.candidates, ids(&[3, 1, 4]));
        assert_eq!(view.aged, ids(&[3]));
        assert_eq!(view.deferred, open);
        let view = claim_view(
            ids(&[1, 4]),
            |id| *id,
            open.clone(),
            Deferrals::Excluded,
            &aging,
        );
        assert_eq!((view.candidates, view.aged), (ids(&[1, 4]), Vec::new()));
        let view = claim_view(
            ids(&[1, 2, 3, 4]),
            |id| *id,
            open,
            Deferrals::Ignored,
            &aging,
        );
        assert_eq!(
            (view.candidates, view.aged),
            (ids(&[1, 2, 3, 4]), Vec::new())
        );
    }

    /// In the room the landing queue leaves the claim arranges only what
    /// the room takes, and records the others as `light_room`: from that
    /// record the shown order is the claim's. A heavy implementation that
    /// waited longest would otherwise keep the implementations first.
    #[test]
    fn the_light_room_s_record_gives_the_claim_s_order() {
        use crate::domain::{ExecutionClass, Priority};
        let candidate = |id, class, waited_min: i64| claim_order::Candidate {
            id: TaskId::new(id),
            class,
            priority: Priority::Normal,
            ready_at_ms: Some(10_000_000 - waited_min * 60_000),
        };
        let lined = vec![
            candidate(1, ExecutionClass::Implementation, 120),
            candidate(2, ExecutionClass::Implementation, 1),
            candidate(3, ExecutionClass::Spike, 30),
        ];
        let aging = ClaimAging {
            candidates: lined.clone(),
            secs: 600,
            now_ms: 10_000_000,
        };
        // The claim in the light room, which takes 2 and 3.
        let heavy: HashSet<TaskId> = [TaskId::new(1)].into();
        let claim = claim_order::arrange(&lined, &heavy, ClassLimits::default(), 10_000_000, 600);
        let view = claim_view(
            ids(&[1, 2, 3]),
            |id| *id,
            vec![deferral(1, claim_order::LIGHT_ROOM)],
            Deferrals::Excluded,
            &aging,
        );
        assert_eq!(view.candidates, claim.order);
        assert_eq!(view.aged, claim.aged);
        assert_eq!(view.candidates, ids(&[3, 2]));
        // Without the record the view would not be the claim's.
        let unrecorded = claim_view(
            ids(&[1, 2, 3]),
            |id| *id,
            Vec::new(),
            Deferrals::Excluded,
            &aging,
        );
        assert_eq!(unrecorded.candidates, ids(&[1, 2, 3]));
    }

    #[test]
    fn the_threshold_is_the_file_s_and_the_latest_registration_s_without_it() {
        let default = claim_order::DEFAULT_CLASS_AGING_SECS;
        let registered = [Some(60), None, Some(900), None];
        let read = |class_aging| -> Result<Option<SupervisorConfig>> {
            Ok(Some(SupervisorConfig {
                class_aging,
                ..SupervisorConfig::default()
            }))
        };
        // The file read: its value, or the default without the key,
        // whatever the registrations hold.
        assert_eq!(class_aging_secs(Some(&read(Some(30))), &registered), 30);
        assert_eq!(class_aging_secs(Some(&read(None)), &registered), default);
        // Not read (no checkout, no file, an error): the latest
        // registration that records one.
        assert_eq!(class_aging_secs(None, &registered), 900);
        assert_eq!(class_aging_secs(Some(&Ok(None)), &registered), 900);
        let unreadable = Err(anyhow::anyhow!("dagq.toml:2: invalid"));
        assert_eq!(class_aging_secs(Some(&unreadable), &registered), 900);
        assert_eq!(class_aging_secs(None, &[None]), default);
        assert_eq!(class_aging_secs(None, &[]), default);
    }

    #[test]
    fn a_deferral_shows_what_the_supervisor_recorded_as_is() {
        let view = claim_view(
            ids(&[3]),
            |id| *id,
            vec![deferral(3, "not_in_build")],
            Deferrals::Excluded,
            &ClaimAging::default(),
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
