//! What a supervisor keeps of the events between its observations of the
//! judgments of now (docs/design/measurement.md "今の判定の記録"), so a
//! pass reads only the events that came since the last one and the events
//! of the runs not finished, not the whole history.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use super::super::{
    EventId, EventKind, GoalId, RunEvent, RunId, TaskId, claim_defer, claim_hold,
    stats::{LiveRun, WorkClock, timestamp_millis, work_outliers},
};

/// How many events one read of the events past the cursor asks for.
pub const EVENTS_PER_READ: usize = 5_000;

/// The events up to [`Self::upto`] folded to what the judgments of now
/// ([`super::judge`] and the inputs `stats` reads off the events) take
/// from beyond the runs not finished: the claim hold open, the open
/// deferral of each task, the latest `stall_config_loaded`, and each run's
/// work clock and its first task. A run's own events are not kept: a run
/// comes back unfinished (a triage's resume), so a pass reads those of the
/// runs not finished from the queue. The asks the judgments read are those
/// of these runs, which their own events open and close.
#[derive(Debug, Default)]
pub struct LiveHistory {
    upto: EventId,
    hold: Option<RunEvent>,
    deferrals: BTreeMap<TaskId, RunEvent>,
    stall: Option<RunEvent>,
    works: HashMap<RunId, (TaskId, WorkClock)>,
}

impl LiveHistory {
    /// The last event folded (0 before any).
    pub fn upto(&self) -> EventId {
        self.upto
    }

    /// Fold `events` (ascending id): those past [`Self::upto`], which
    /// moves on to the last of them.
    pub fn mark(&mut self, events: &[RunEvent]) {
        for event in events {
            if event.id > self.upto {
                self.observe(event);
                self.upto = event.id;
            }
        }
    }

    /// Move [`Self::upto`] on to `upto`, with no event between.
    pub fn reach(&mut self, upto: EventId) {
        self.upto = self.upto.max(upto);
    }

    /// Fold one event by the rules the judgments read the whole history
    /// by: [`claim_hold::hold_spans`]' for the claim hold,
    /// [`claim_defer::deferral_spans`]' for the deferrals,
    /// [`StallConfig::loaded`](super::super::stall::StallConfig::loaded)'s
    /// for the thresholds, and [`WorkClock`]'s.
    fn observe(&mut self, event: &RunEvent) {
        let opener = self
            .hold
            .as_ref()
            .map(|held| held.payload.get("supervisor").and_then(Value::as_str));
        if claim_hold::hold_end(claim_hold::CLAIMS, opener, event).is_some() {
            self.hold = (event.kind == claim_hold::CLAIMS.held).then(|| event.clone());
        }
        if let Some((task, _)) = claim_defer::deferral_end(event) {
            if event.kind == EventKind::ClaimDeferred {
                self.deferrals.insert(task, event.clone());
            } else {
                self.deferrals.remove(&task);
            }
        }
        if event.kind == EventKind::StallConfigLoaded {
            self.stall = Some(event.clone());
        }
        if let (Some(run), Some(task)) = (&event.run_id, event.task_id) {
            self.works
                .entry(run.clone())
                .or_insert((task, WorkClock::default()))
                .1
                .observe(event, timestamp_millis(&event.created_at));
        }
    }

    /// The events the judgments of now read at [`Self::upto`]: those kept
    /// with `runs` (the events of the runs not finished, as the queue reads
    /// them), each up to [`Self::upto`] and once, ascending. Judged as the
    /// whole history up to there, they give the same.
    pub fn events(&self, runs: impl IntoIterator<Item = RunEvent>) -> Vec<RunEvent> {
        let mut events: Vec<RunEvent> = self
            .hold
            .iter()
            .chain(self.deferrals.values())
            .chain(&self.stall)
            .cloned()
            .chain(runs.into_iter().filter(|event| event.id <= self.upto))
            .collect();
        events.sort_by_key(|event| event.id);
        events.dedup_by_key(|event| event.id);
        events
    }

    /// [`outlier_thresholds`](super::super::stats::outlier_thresholds) of
    /// `runs` on the history folded, `goals` mapping a task to its goal.
    pub fn outlier_thresholds(
        &self,
        goals: &HashMap<TaskId, Option<GoalId>>,
        runs: &[LiveRun],
    ) -> HashMap<RunId, i64> {
        let works = self
            .works
            .iter()
            .map(|(run, (task, clock))| (run, (goals.get(task).copied().flatten(), clock)))
            .collect();
        work_outliers(&works, runs)
    }
}
