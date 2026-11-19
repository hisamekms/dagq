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
//! full or its claims are held, so a deferral may stay open then. A
//! `light_room` one stays open over a brief free slot in which the claim
//! may take its task, and holds the task out of the order only while no
//! live supervisor has a heavy slot free (`claim_order::in_effect`); this
//! module neither closes nor adds one, and runs no `claim_defer::decide`,
//! hotspot or provider route of its own. The supervisor's own reads of the
//! candidates (`fill_slots`, the samples of `stats` and `kpi`, the
//! observer) do not go through here.

use crate::domain::EventKind;
use anyhow::Result;
use serde::Serialize;

use std::collections::HashSet;

use crate::application::{GraphInput, RunLog};
use crate::domain::claim_defer::{DEFERRAL_KINDS, OpenDeferral};
use crate::domain::claim_order::{self, ClassLimits};
use crate::domain::slot_limits::SupervisorConfig;
use crate::domain::{GoalStatus, TaskId, TaskStatus};

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

/// Each task's open deferral as the supervisor last recorded it, of those
/// that hold the task out of the claim now ([`claim_order::in_effect`]):
/// `live` gives the tokens of the supervisors alive now, read only when a
/// record is of the light room, and each one's newest `slots_full_*` is
/// read however old it is.
pub fn open_deferrals(
    queue: &(impl RunLog + ?Sized),
    live: impl FnOnce() -> Result<Vec<String>>,
) -> Result<Vec<OpenDeferral>> {
    let open: Vec<OpenDeferral> = queue
        .latest_task_events(&DEFERRAL_KINDS.map(EventKind::as_str))?
        .iter()
        .filter_map(OpenDeferral::of)
        .collect();
    if !open
        .iter()
        .any(|deferral| deferral.reason == claim_order::LIGHT_ROOM)
    {
        return Ok(open);
    }
    let live = live()?;
    let tokens: Vec<&str> = live.iter().map(String::as_str).collect();
    let slots_full = queue.latest_events_by_supervisor(
        &[
            EventKind::SlotsFullStarted.as_str(),
            EventKind::SlotsFullEnded.as_str(),
        ],
        &tokens,
    )?;
    Ok(claim_order::in_effect(open, &live, &slots_full))
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

/// The latest Admission observation for each live supervisor. Older
/// samples still count for KPI and leave the legacy status views in use.
pub(crate) fn admission_records(
    queue: &(impl crate::application::RunLog + ?Sized),
    tokens: &[&str],
) -> anyhow::Result<Vec<crate::domain::RunEvent>> {
    use crate::domain::{EventKind, claim_hold::CLAIMS};
    let mut records =
        queue.latest_events_by_supervisor(&[crate::domain::kpi::CANDIDATES_SAMPLED], tokens)?;
    records.retain(|event| event.payload.get("admission_reason").is_some());
    if !records.is_empty()
        && let Some(resumed) = queue.latest_queue_event(&CLAIMS.kinds())?
        && resumed.kind == EventKind::ClaimResumed
    {
        // An older candidate sample cannot reopen a queue-wide resource
        // hold. The local decisions of other supervisors still apply.
        records.retain(|event| {
            event.id > resumed.id
                || !matches!(
                    event.payload["admission_reason"].as_str(),
                    Some(
                        "Disk"
                            | "DiskCompetition"
                            | "LandingFirst"
                            | "NoSamples"
                            | "Load"
                            | "Authentication"
                    )
                )
        });
    }
    Ok(records)
}

/// The decision and its timestamp, shared by status and candidates.
pub(crate) fn admission_view(event: &crate::domain::RunEvent) -> serde_json::Value {
    let mut view = event.payload.clone();
    view["since"] = serde_json::json!(event.created_at);
    view
}

/// A supervisor-local hold has no queue-wide hold event.
pub(crate) fn admission_hold(view: &serde_json::Value) -> Option<serde_json::Value> {
    (view["claims_held"] == true).then(|| {
        let mut hold = view.clone();
        hold["reason"] = hold["admission_reason"].clone();
        hold
    })
}

/// Whether a modern supervisor last waited for spacing. Its elapsed
/// deadline alone says nothing about whether Admission reached that gate.
pub(crate) fn show_spacing_decision(
    spacing: &mut serde_json::Value,
    event: &crate::domain::RunEvent,
) {
    let reason = &event.payload["admission_reason"];
    let waiting = reason == crate::domain::admission::Blocked::ClaimSpacing.name();
    spacing["waiting"] = serde_json::json!(waiting);
    if waiting {
        spacing["admission_reason"] = reason.clone();
        spacing["secs"] = event.payload["claim_spacing"].clone();
    }
}

/// How many task IDs each reason of [`ClaimExclusions`] lists, lowest
/// first.
pub const EXCLUDED_TASK_IDS: usize = 5;

/// The ready tasks of one reason of [`ClaimExclusions`]: how many, and the
/// lowest [`EXCLUDED_TASK_IDS`] of their IDs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ExcludedTasks {
    pub count: usize,
    pub task_ids: Vec<TaskId>,
}

impl ExcludedTasks {
    fn push(&mut self, id: TaskId) {
        self.count += 1;
        if self.task_ids.len() < EXCLUDED_TASK_IDS {
            self.task_ids.push(id);
        }
    }
}

/// `candidates`' `deferred` while `held` (its `held`) leads with the
/// queue-wide hold on the claims (`claim_held`): each deferral carries that
/// hold's reason as `claims_held`. No supervisor judges a task's deferral
/// while the claims are held, so the one shown is the last recorded before
/// the hold, and the queue-wide hold, not it, is why the task waits now.
/// With no such hold, `deferred` is left as it is.
pub fn under_claims_hold(deferred: &mut serde_json::Value, held: &[serde_json::Value]) {
    let Some(reason) = held
        .iter()
        .find(|entry| entry["reason"] == crate::domain::claim_hold::CLAIM_HELD)
        .map(|entry| entry["record"]["reason"].clone())
    else {
        return;
    };
    for deferral in deferred.as_array_mut().into_iter().flatten() {
        deferral["claims_held"] = reason.clone();
    }
}

/// The ready tasks the claim order never takes, by why: `candidates`'
/// `excluded` and `status`' `claim_excluded`. They fall out before the
/// order, so no supervisor judges them and none has a deferral. Each task
/// counts under its first reason in this order: its goal is a draft
/// (`draft_goal`), a direct predecessor is not completed (`dependencies`:
/// unfinished, or canceled, which never releases the task), a goal it
/// depends on is not closed as achieved (`goal_dependencies`, an abandoned
/// one included). A ready task without any of these that is no candidate
/// (an unfinished run, a hold) is not counted. The claim reads none of
/// this.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ClaimExclusions {
    pub draft_goal: ExcludedTasks,
    pub dependencies: ExcludedTasks,
    pub goal_dependencies: ExcludedTasks,
}

impl ClaimExclusions {
    /// The ready tasks of `input` (every unfinished task of the queue) out
    /// of the claim order, by their first reason.
    pub fn of(input: &GraphInput) -> Self {
        let open: HashSet<TaskId> = input.tasks.iter().map(|task| task.id).collect();
        let canceled: HashSet<TaskId> = input.canceled.iter().copied().collect();
        let mut tasks: Vec<_> = input
            .tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Ready)
            .collect();
        tasks.sort_by_key(|task| task.id);
        let mut exclusions = Self::default();
        for task in tasks {
            if task.goal_status == Some(GoalStatus::Draft) {
                exclusions.draft_goal.push(task.id);
            } else if task
                .depends_on
                .iter()
                .any(|id| open.contains(id) || canceled.contains(id))
            {
                exclusions.dependencies.push(task.id);
            } else if task.goal_dependencies.iter().any(|d| !d.is_met()) {
                exclusions.goal_dependencies.push(task.id);
            }
        }
        exclusions
    }
}

/// Facts read from the unfinished-run registry and its selected measurement
/// cache. `phase` is the recorded phase used to name a waiting stage;
/// `measurement` is (raw bytes, walk-start Unix milliseconds, stale).
/// The cache has already checked target identity and measurement age.
#[derive(Debug, Clone)]
pub struct RunDiskInput {
    pub run: crate::domain::RunId,
    pub task: TaskId,
    pub stage: crate::domain::disk::DiskStage,
    pub phase: String,
    pub container: bool,
    pub measurement: (Option<u64>, Option<i64>, bool),
}

/// Derive one run's recorded reservation from cached facts, without I/O.
/// Waiting uses its displayed size and contributes zero to U. WORKER and
/// INTEGRATOR use effective W/I, subtracting only a fresh known size.
/// Containers discard host readings, including their timestamps and stale
/// flags; a moving container therefore leaves its whole reservation unused.
pub fn run_disk_status(
    needs: &crate::domain::disk::DiskNeeds,
    input: RunDiskInput,
) -> crate::domain::disk::RunDiskStatus {
    use crate::domain::disk::{DiskStage, RunDiskStatus};
    let waiting = input.stage == DiskStage::Waiting;
    let (current, measured_at_ms, stale) = if input.container {
        (None, None, false)
    } else {
        input.measurement
    };
    let reservation = match input.stage {
        DiskStage::Worker => needs.expected,
        DiskStage::Integrator => needs.integrator_expected,
        DiskStage::Waiting => current,
    };
    RunDiskStatus {
        run: input.run,
        task: input.task,
        stage: match input.stage {
            DiskStage::Worker => "WORKER".into(),
            DiskStage::Integrator => "INTEGRATOR".into(),
            DiskStage::Waiting => input.phase,
        },
        reservation,
        current,
        measured_at_ms,
        stale,
        container: input.container,
        target: if waiting { "build" } else { "worktree+tmp" }.into(),
        unused: if waiting {
            0
        } else {
            reservation
                .unwrap_or(0)
                .saturating_sub(if stale { 0 } else { current.unwrap_or(0) })
        },
    }
}

/// W/I provenance from the same cached samples and time as `stage_budgets`.
/// Each stage uses p90 (or the maximum with fewer than five samples); an
/// empty stage falls back to the single run estimate. If only WORKER peaks
/// exist, I falls back to W. These labels describe inputs before relaxation.
pub fn disk_sources(
    method: crate::domain::disk::DiskEstimate,
    runs: &[crate::domain::disk::RunSample],
    stages: &crate::domain::disk::StageSamples,
    now_ms: i64,
) -> [&'static str; 2] {
    use crate::domain::disk::{DiskEstimate, QUANTILE_SAMPLES, expected_run_estimate};
    if method == DiskEstimate::Fixed {
        return ["configuration"; 2];
    }
    let source =
        |samples: &[crate::domain::disk::RunSample]| match expected_run_estimate(samples, now_ms).1
        {
            0 => "single run estimate fallback",
            n if n < QUANTILE_SAMPLES => "stage peaks maximum (few samples)",
            _ => "stage peaks p90",
        };
    let worker = source(&stages.worker);
    let integrator = if expected_run_estimate(&stages.integrator, now_ms).1 == 0
        && expected_run_estimate(runs, now_ms).1 == 0
        && expected_run_estimate(&stages.worker, now_ms).1 > 0
    {
        "WORKER estimate fallback"
    } else {
        source(&stages.integrator)
    };
    [worker, integrator]
}

/// Describe an already frozen disk decision. This function only arranges
/// recorded facts; neither status nor its formatter walks a worktree or
/// recomputes the supervisor's claim allowance.
pub fn disk_status(
    decision: &crate::domain::disk::DiskDecision,
    runs: Vec<crate::domain::disk::RunDiskStatus>,
    parallel: usize,
    sources: [&str; 2],
) -> crate::domain::disk::DiskStatus {
    use crate::domain::disk::{ClaimsWaitReason, DiskEstimate, DiskStatus, gib};
    let needs = decision.needs;
    let method = match needs.estimate {
        DiskEstimate::Fixed => "fixed",
        DiskEstimate::Recent => "recent",
    };
    let reason = if decision.reserve.below {
        "reserve_floor"
    } else if decision.free.is_none() {
        "unread"
    } else if needs.claims_wait_reason == Some(ClaimsWaitReason::NoSamples) {
        "no_samples"
    } else if decision.claim_short {
        "disk_space"
    } else {
        "allowed"
    };
    let integration_slot: Vec<_> = runs
        .iter()
        .filter(|r| r.stage == "INTEGRATOR")
        .map(|r| r.run.clone())
        .collect();
    let show = |bytes: u64| gib(bytes as f64);
    let maybe = |bytes: Option<u64>| bytes.map_or_else(|| "unmeasured".into(), show);
    let mut status = DiskStatus {
        needs,
        free: decision.free,
        worker_source: sources[0].into(),
        integrator_source: sources[1].into(),
        runs,
        integration_slot,
        deficit: decision.free.map(|f| needs.claim.saturating_sub(f)),
        reason: reason.into(),
        claims: decision.claims.unwrap_or(parallel).min(parallel),
        parallel,
        message: String::new(),
    };
    let slot = if status.integration_slot.is_empty() {
        "free".into()
    } else {
        status
            .integration_slot
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut lines = vec![format!(
        "disk {}: W {} ({}), I {} ({}); reserve {}, free {}; U {}; INTEGRATOR slot {} ({}); claim needs {}, deficit {}; reason {}; claims {}/parallel {}; relax {}",
        method,
        maybe(needs.expected),
        sources[0],
        maybe(needs.integrator_expected),
        sources[1],
        show(needs.reserve),
        maybe(decision.free),
        show(needs.remaining),
        maybe(needs.integrator_expected),
        slot,
        show(needs.claim),
        maybe(status.deficit),
        reason,
        status.claims,
        parallel,
        needs.relaxed.map_or_else(|| "none".into(), |r| r.message()),
    )];
    for run in &status.runs {
        let current = if run.container {
            "unmeasured (container)".into()
        } else {
            maybe(run.current)
        };
        lines.push(format!(
            "run {} task {} {}: reservation {}, current {}{} [{}] at {}{}; unused {}",
            run.run,
            run.task,
            run.stage,
            maybe(run.reservation),
            current,
            if run.stale { " (stale)" } else { "" },
            run.target,
            run.measured_at_ms
                .map_or_else(|| "unmeasured".into(), |t| t.to_string()),
            if run.stale { " (stale)" } else { "" },
            show(run.unused)
        ));
    }
    status.message = lines.join("\n");
    status
}

/// Overlay a live supervisor reading on a copy of a claim's start event.
/// The start time/id and reason remain the hold's; size, allowance, deficit,
/// stale flags and the human message are the latest pass's facts.
/// Landing holds never use this overlay. No event is written by this view.
pub fn refresh_disk_hold(event: &mut crate::domain::RunEvent, current: serde_json::Value) {
    if current["needs"].is_object() && current["message"].is_string() {
        event.payload["disk"] = current["needs"].clone();
        event.payload["value"] = current["free"].clone();
        event.payload["threshold"] = current["needs"]["claim"].clone();
        event.payload["message"] = current["message"].clone();
        event.payload["disk_status"] = current;
    }
}

/// Unix milliseconds used by both off-loop measurement jobs.
pub(crate) fn disk_measured_at(clock: &dyn crate::application::Clock) -> i64 {
    clock
        .system_time()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn status_spacing_uses_the_recorded_wait() {
        let mut event: crate::domain::RunEvent = serde_json::from_value(json!({
            "id": 1, "kind": "candidates_sampled", "created_at": "2026-01-01 00:00:00",
            "payload": {"admission_reason": "ClaimSpacing", "claims_held": false, "claim_spacing": 30},
        })).unwrap();
        let mut spacing = json!({"waiting": false, "secs": 30});
        show_spacing_decision(&mut spacing, &event);
        assert_eq!(
            spacing,
            json!({"waiting": true, "secs": 30, "admission_reason": "ClaimSpacing"})
        );
        event.payload["admission_reason"] = json!("WorkerSlot");
        let mut spacing = json!({"waiting": true, "secs": 30});
        show_spacing_decision(&mut spacing, &event);
        assert_eq!(spacing, json!({"waiting": false, "secs": 30}));
    }

    /// The deferrals shown under the queue-wide hold name it; with no such
    /// hold (none, or only another kind in `held`) they are left alone.
    #[test]
    fn deferrals_under_the_claims_hold_name_it() {
        let deferred = json!([{"task_id": 7, "reason": "investigation_limit"}]);
        let held = [
            json!({"reason": "claim_held", "since": "t",
                   "record": {"reason": "disk_space", "supervisor": "s"}}),
            json!({"reason": "run_env_program_missing", "record": {}}),
        ];
        let mut shown = deferred.clone();
        under_claims_hold(&mut shown, &held);
        assert_eq!(
            shown,
            json!([{"task_id": 7, "reason": "investigation_limit", "claims_held": "disk_space"}])
        );
        let mut shown = deferred.clone();
        under_claims_hold(&mut shown, &held[1..]);
        assert_eq!(shown, deferred);
        let mut shown = deferred.clone();
        under_claims_hold(&mut shown, &[]);
        assert_eq!(shown, deferred);
    }

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

    fn ready(
        id: i64,
        goal: Option<GoalStatus>,
        depends_on: &[i64],
    ) -> crate::application::GraphTask {
        crate::application::GraphTask {
            id: TaskId::new(id),
            status: TaskStatus::Ready,
            priority: crate::domain::Priority::Normal,
            priority_source: crate::domain::PrioritySource::Default,
            priority_by: crate::domain::plan_request::PriorityBy::Ai,
            execution_class: crate::domain::ExecutionClass::Implementation,
            title: format!("task {id}"),
            goal_id: goal.map(|_| crate::domain::GoalId::new(1)),
            goal_status: goal,
            depends_on: ids(depends_on),
            goal_dependencies: Vec::new(),
        }
    }

    fn waits_for_goal(
        mut task: crate::application::GraphTask,
        verdict: Option<crate::domain::GoalVerdict>,
    ) -> crate::application::GraphTask {
        task.goal_dependencies
            .push(crate::application::GraphGoalDependency {
                goal_id: crate::domain::GoalId::new(7),
                verdict,
            });
        task
    }

    /// 1 is in a draft goal and waits for 9 too: it counts once, under the
    /// draft goal. 2 waits for the unfinished 9, 6 for the canceled 10, 3
    /// for an open goal, 4 for an abandoned one; 5 only for the finished 8 and an achieved goal, so
    /// it is a candidate. 9 is in progress, so it is not counted.
    #[test]
    fn the_ready_tasks_out_of_the_order_count_under_their_first_reason() {
        use crate::domain::GoalVerdict;
        let mut running = ready(9, None, &[]);
        running.status = TaskStatus::InProgress;
        let input = GraphInput {
            tasks: vec![
                waits_for_goal(
                    ready(5, Some(GoalStatus::Open), &[8]),
                    Some(GoalVerdict::Achieved),
                ),
                ready(1, Some(GoalStatus::Draft), &[9]),
                ready(2, Some(GoalStatus::Open), &[9]),
                waits_for_goal(ready(3, None, &[]), None),
                waits_for_goal(ready(4, None, &[8]), Some(GoalVerdict::Abandoned)),
                running,
                ready(6, None, &[8, 10]),
            ],
            candidates: ids(&[5]),
            canceled: ids(&[10]),
        };
        assert_eq!(
            serde_json::to_value(ClaimExclusions::of(&input)).unwrap(),
            json!({
                "draft_goal": {"count": 1, "task_ids": [1]},
                "dependencies": {"count": 2, "task_ids": [2, 6]},
                "goal_dependencies": {"count": 2, "task_ids": [3, 4]},
            })
        );
    }

    #[test]
    fn a_reason_lists_only_its_lowest_task_ids_and_counts_them_all() {
        let input = GraphInput {
            tasks: (1..=7)
                .rev()
                .map(|id| ready(id, Some(GoalStatus::Draft), &[]))
                .collect(),
            candidates: Vec::new(),
            ..Default::default()
        };
        let exclusions = ClaimExclusions::of(&input);
        assert_eq!(exclusions.draft_goal.count, 7);
        assert_eq!(exclusions.draft_goal.task_ids, ids(&[1, 2, 3, 4, 5]));
        assert_eq!(exclusions.dependencies, ExcludedTasks::default());
    }
}

#[cfg(test)]
mod disk_tests {
    use super::*;
    use crate::domain::disk::*;
    use serde_json::json;
    const G: u64 = 1 << 30;

    fn shown(decision: &DiskDecision, runs: Vec<RunDiskStatus>, parallel: usize) -> DiskStatus {
        disk_status(
            decision,
            runs,
            parallel,
            disk_sources(decision.needs.estimate, &[], &StageSamples::default(), 0),
        )
    }

    #[test]
    fn live_disk_state_replaces_the_view_without_changing_the_start_event() {
        let event: crate::domain::RunEvent = serde_json::from_value(json!({
            "id": 1, "kind":"claim_held", "created_at":"2026-01-01 00:00:00",
            "payload":{"reason":"disk_space","supervisor":"s","message":"initial",
                "disk_status":{"runs":[]}}
        }))
        .unwrap();
        let mut view = event.clone();
        for (stage, current, stale, claims, deficit) in [
            ("WORKER", 12, true, 0, 10),
            ("review_held", 18, false, 1, 0),
        ] {
            refresh_disk_hold(
                &mut view,
                json!({"needs":{"claim":50},"free":40,
                "message":"latest","claims":claims,"deficit":deficit,
                "runs":[{"stage":stage,"current":current,"stale":stale}]}),
            );
            assert_eq!(view.payload["disk_status"]["runs"][0]["stage"], stage);
            assert_eq!(view.payload["disk_status"]["runs"][0]["current"], current);
            assert_eq!(view.payload["disk_status"]["runs"][0]["stale"], stale);
            assert_eq!(view.payload["disk_status"]["claims"], claims);
            assert_eq!(view.payload["disk_status"]["deficit"], deficit);
            assert_eq!(view.payload["message"], "latest");
            assert_eq!(view.payload["value"], 40);
            assert_eq!(view.payload["threshold"], 50);
            assert_eq!(view.id, event.id);
            assert_eq!(view.created_at, event.created_at);
        }
        assert_eq!(event.payload["message"], "initial");
        assert_eq!(event.payload["disk_status"]["runs"], json!([]));
    }

    #[test]
    fn landing_holds_do_not_carry_the_claim_explanation() {
        let state = DiskState {
            free: Some(G),
            total: 0,
            expected: Some(20 * G),
            integrator_expected: Some(20 * G),
            runs: vec![],
            moving: 1,
            landing_current: None,
            landing_waiting: true,
            relaxed: None,
        };
        let mut decision = state.decide(
            0.05,
            &[LandingCandidate {
                run: crate::domain::RunId::new("landing").unwrap(),
                current: None,
                integrating: false,
                waiting: true,
            }],
            DiskEstimate::Fixed,
            0,
        );
        decision.status = Some(shown(&decision, vec![], 3));
        assert!(decision.claim_hold().unwrap().disk_status.is_some());
        let hold = decision.landing_hold().unwrap();
        assert!(hold.disk_status.is_none());
        let (_, payload) = crate::domain::claim_hold::landing_transition(
            Some(&hold),
            None,
            &decision.landings,
            &crate::domain::LeaseToken::new("s"),
            |_| true,
        )
        .unwrap();
        assert!(payload.get("disk_status").is_none());
        assert!(!payload["message"].as_str().unwrap().contains("claim needs"));
    }

    #[test]
    fn sources_distinguish_each_stage_and_its_fallback() {
        let sample = RunSample {
            bytes: G,
            at_millis: 10,
        };
        let mut stages = StageSamples::default();
        assert_eq!(
            disk_sources(DiskEstimate::Fixed, &[], &stages, 10),
            ["configuration"; 2]
        );
        assert_eq!(
            disk_sources(DiskEstimate::Recent, &[], &stages, 10),
            ["single run estimate fallback"; 2]
        );
        stages.worker = vec![sample; 5];
        assert_eq!(
            disk_sources(DiskEstimate::Recent, &[], &stages, 10),
            ["stage peaks p90", "WORKER estimate fallback"]
        );
        assert_eq!(
            disk_sources(
                DiskEstimate::Recent,
                std::slice::from_ref(&sample),
                &stages,
                10
            ),
            ["stage peaks p90", "single run estimate fallback"]
        );
        stages.integrator = vec![sample; 1];
        let sources = disk_sources(DiskEstimate::Recent, &[], &stages, 10);
        assert_eq!(
            sources,
            ["stage peaks p90", "stage peaks maximum (few samples)"]
        );
        let state = DiskState {
            free: Some(100 * G),
            total: 0,
            expected: Some(G),
            integrator_expected: Some(G),
            runs: vec![],
            moving: 1,
            landing_current: None,
            landing_waiting: false,
            relaxed: None,
        };
        let status = disk_status(
            &state.decide(0.05, &[], DiskEstimate::Recent, 5),
            vec![],
            3,
            sources,
        );
        assert!(status.message.contains(
            "W 1.0 GiB (stage peaks p90), I 1.0 GiB (stage peaks maximum (few samples))"
        ));
        let json = serde_json::to_value(status).unwrap();
        assert_eq!(json["worker_source"], "stage peaks p90");
        assert_eq!(
            json["integrator_source"],
            "stage peaks maximum (few samples)"
        );
        assert_eq!(
            disk_sources(
                DiskEstimate::Recent,
                &[],
                &stages,
                11 + SAMPLE_WINDOW_MILLIS
            ),
            ["single run estimate fallback"; 2]
        );
    }

    #[test]
    fn fixed_reservations_explain_the_shortage_and_reserve_floor() {
        let mut state = DiskState {
            free: Some(60 * G),
            total: 228 * G,
            expected: Some(20 * G),
            integrator_expected: Some(20 * G),
            runs: vec![
                DiskRun {
                    stage: DiskStage::Worker,
                    current: Some(12 * G),
                },
                DiskRun {
                    stage: DiskStage::Worker,
                    current: Some(5 * G),
                },
                DiskRun {
                    stage: DiskStage::Waiting,
                    current: Some(18 * G),
                },
            ],
            moving: 2,
            landing_current: None,
            landing_waiting: false,
            relaxed: None,
        };
        let runs = |needs: &DiskNeeds| {
            [
                ("a", DiskStage::Worker, 12),
                ("b", DiskStage::Worker, 5),
                ("c", DiskStage::Waiting, 18),
            ]
            .into_iter()
            .map(|(id, stage, current)| {
                run_disk_status(
                    needs,
                    RunDiskInput {
                        run: crate::domain::RunId::new(id).unwrap(),
                        task: TaskId::new(7),
                        stage,
                        phase: "waiting_answer".into(),
                        container: false,
                        measurement: (Some(current * G), Some(1234), false),
                    },
                )
            })
            .collect()
        };
        let decision = state.decide(0.05, &[], DiskEstimate::Fixed, 0);
        let status = shown(&decision, runs(&decision.needs), 3);
        assert_eq!(
            status.runs.iter().map(|run| run.unused).sum::<u64>(),
            23 * G
        );
        assert_eq!(
            status.runs.iter().map(|run| run.unused).sum::<u64>(),
            status.needs.remaining
        );
        assert_eq!(
            status
                .runs
                .iter()
                .map(|run| run.reservation)
                .collect::<Vec<_>>(),
            [Some(20 * G), Some(20 * G), Some(18 * G)]
        );
        assert_eq!(
            status
                .runs
                .iter()
                .map(|run| run.stage.as_str())
                .collect::<Vec<_>>(),
            ["WORKER", "WORKER", "waiting_answer"]
        );
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["needs"]["estimate"], "fixed");
        assert_eq!(json["worker_source"], "configuration");
        assert_eq!(json["needs"]["expected"], 20 * G);
        assert_eq!(json["needs"]["integrator_expected"], 20 * G);
        assert_eq!(json["needs"]["remaining"], 23 * G);
        assert_eq!(json["integration_slot"], serde_json::json!([]));
        assert_eq!(json["claims"], 0);
        assert_eq!(json["reason"], "disk_space");
        assert_eq!(json["runs"][0]["unused"], 8 * G);
        assert_eq!(json["runs"][1]["unused"], 15 * G);
        assert_eq!(json["runs"][2]["unused"], 0);
        for text in [
            "disk fixed: W 20.0 GiB",
            "I 20.0 GiB",
            "reserve 11.4 GiB",
            "free 60.0 GiB",
            "U 23.0 GiB",
            "INTEGRATOR slot 20.0 GiB (free)",
            "claim needs 74.4 GiB",
            "deficit 14.4 GiB",
            "reason disk_space",
            "claims 0/parallel 3",
            "WORKER: reservation 20.0 GiB, current 12.0 GiB",
            "unused 8.0 GiB",
            "unused 15.0 GiB",
            "waiting_answer: reservation 18.0 GiB, current 18.0 GiB",
            "unused 0.0 GiB",
        ] {
            assert!(
                status.message.contains(text),
                "missing {text}: {}",
                status.message
            );
        }
        state.free = Some(10 * G);
        let decision = state.decide(0.05, &[], DiskEstimate::Fixed, 0);
        assert_eq!(
            shown(&decision, runs(&decision.needs), 3).reason,
            "reserve_floor"
        );
    }

    #[test]
    fn occupied_integrator_and_stale_size_explain_relaxation() {
        let state = DiskState {
            free: Some(100 * G),
            total: 0,
            expected: Some(20 * G),
            integrator_expected: Some(30 * G),
            runs: vec![DiskRun {
                stage: DiskStage::Integrator,
                current: None,
            }],
            moving: 1,
            landing_current: None,
            landing_waiting: false,
            relaxed: Some(DiskRelaxed {
                relax: DiskRelax {
                    expected: None,
                    no_remaining: true,
                    no_landing_expectation: false,
                },
                until_millis: 1000,
                event_id: 7,
            }),
        };
        let decision = state.decide(0.05, &[], DiskEstimate::Fixed, 0);
        let run = run_disk_status(
            &decision.needs,
            RunDiskInput {
                run: crate::domain::RunId::new("i").unwrap(),
                task: TaskId::new(9),
                stage: DiskStage::Integrator,
                phase: "landing".into(),
                container: false,
                measurement: (Some(18 * G), Some(123), true),
            },
        );
        assert_eq!(run.reservation, Some(30 * G));
        assert_eq!(run.unused, 30 * G);
        let status = shown(&decision, vec![run], 3);
        assert_eq!(
            status.integration_slot,
            [crate::domain::RunId::new("i").unwrap()]
        );
        assert_eq!(status.needs.remaining, 30 * G);
        assert_eq!(status.needs.claim_remaining, 0);
        assert_eq!(status.needs.landing_expected, 0);
        assert_eq!(status.needs.claim, 25 * G);
        assert_eq!(status.claims, 3);
        assert!(status.message.contains("INTEGRATOR slot 30.0 GiB (i)"));
        assert!(
            status
                .message
                .contains("current 18.0 GiB (stale) [worktree+tmp]")
        );
        assert!(status.message.contains("at 123 (stale); unused 30.0 GiB"));
        assert!(status.message.contains("disk relax (event 7)"));
    }

    #[test]
    fn wait_reasons_relaxation_and_unknown_container_are_explicit() {
        let mut state = DiskState {
            free: Some(100 * G),
            total: 0,
            expected: None,
            integrator_expected: None,
            runs: vec![],
            moving: 1,
            landing_current: None,
            landing_waiting: false,
            relaxed: None,
        };
        let decision = state.decide(0.05, &[], DiskEstimate::Recent, 0);
        let run = run_disk_status(
            &decision.needs,
            RunDiskInput {
                run: crate::domain::RunId::new("container").unwrap(),
                task: TaskId::new(1),
                stage: DiskStage::Waiting,
                phase: "waiting_answer".into(),
                container: true,
                measurement: (Some(18 * G), Some(123), true),
            },
        );
        assert_eq!(
            (run.current, run.measured_at_ms, run.stale),
            (None, None, false)
        );
        let status = shown(&decision, vec![run], 2);
        assert_eq!(status.reason, "no_samples");
        assert!(status.message.contains("unmeasured (container)"));
        assert!(status.message.contains("single run estimate fallback"));
        assert!(status.message.contains("relax none"));
        state.moving = 0;
        assert_eq!(
            shown(&state.decide(0.05, &[], DiskEstimate::Recent, 0), vec![], 2).claims,
            1
        );
        state.expected = Some(G);
        state.integrator_expected = Some(G);
        assert_eq!(
            shown(&state.decide(0.05, &[], DiskEstimate::Fixed, 0), vec![], 2).claims,
            2
        );
        state.free = None;
        state.expected = None;
        state.moving = 1;
        assert_eq!(
            shown(&state.decide(0.05, &[], DiskEstimate::Recent, 0), vec![], 2).reason,
            "unread"
        );
    }
}
