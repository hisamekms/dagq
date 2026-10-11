//! The ledger (ADR-t1662-1 decisions 10, 13, 17 and 18, ADR-t1662-2,
//! docs/design/measurement.md "台帳の形"): one row per run and per task,
//! folded from the EventStore alone by [`fold`], kept by the
//! `LedgerStore` and read by [`read`]. A view: a row is built again when
//! [`LEDGER_V`] goes up or a later event arrives ([`rebuilds`]), except a
//! legacy row (a run from before `run_phase_changed`, and a task with such
//! a run), which keeps the values `stats` gave it once. Measurement only:
//! no decision of the runtime reads it.

pub mod attributes;
mod baseline;
pub mod findings;
pub mod fold;
pub mod read;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use self::fold::{Interval, RunLife, TaskFold, TaskLife};
use super::after_claim::AfterClaim;
use super::claim_facts::{self, ATTRIBUTES};
use super::pre_claim::PreClaimInterval;
use super::stats::{self, RunStats};
use super::{EventId, RunEvent, TaskId, event_kind};

/// The version of the fold: raised when what a row holds or how it is
/// folded changes, so every row but a legacy one is built again.
pub const LEDGER_V: u32 = 1;

/// The kind of a row. The store keeps every kind in one table by this
/// column, so a new kind is a new value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    Run,
    Task,
}

impl LedgerKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Task => "task",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "run" => Some(Self::Run),
            "task" => Some(Self::Task),
            _ => None,
        }
    }
}

/// How far into its inputs a row was folded: the last EventStore event
/// id. The later kinds of input (steps, node samples, resources) add their
/// own key beside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputsThrough {
    pub events: EventId,
}

/// The two measures of a set of intervals, in milliseconds: the time and
/// the part of it that held a slot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Measure {
    pub duration_ms: i64,
    pub slot_ms: i64,
}

impl Measure {
    fn add(&mut self, interval: &Interval) {
        self.duration_ms += interval.millis();
        if interval.holds_slot() {
            self.slot_ms += interval.millis();
        }
    }
}

/// A row's sums: the whole, the time no record accounts for, and per
/// blocker, per holds and per attempt (`<kind>.<n>`), the unattributed
/// time in none of them; and how many attempts the intervals went through.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    #[serde(flatten)]
    pub all: Measure,
    pub unattributed_ms: i64,
    pub by_blocker: BTreeMap<String, Measure>,
    pub by_holds: BTreeMap<String, Measure>,
    pub by_attempt: BTreeMap<String, Measure>,
    pub attempts: i64,
}

impl Totals {
    pub fn of(intervals: &[Interval]) -> Self {
        let mut totals = Self::default();
        for interval in intervals {
            totals.all.add(interval);
            if interval.is_unattributed() {
                totals.unattributed_ms += interval.millis();
                continue;
            }
            if let Some(blocker) = &interval.blocker {
                totals
                    .by_blocker
                    .entry(name(blocker))
                    .or_default()
                    .add(interval);
            }
            if let Some(holds) = &interval.holds {
                totals
                    .by_holds
                    .entry(name(holds))
                    .or_default()
                    .add(interval);
            }
            if let Some(attempt) = &interval.attempt {
                let key = format!("{}.{}", name(attempt.kind), attempt.n);
                totals.by_attempt.entry(key).or_default().add(interval);
            }
        }
        totals.attempts = i64::try_from(totals.by_attempt.len()).unwrap_or(i64::MAX);
        totals
    }
}

/// The name a tag serializes to.
fn name(value: impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// One row of the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRow {
    pub kind: LedgerKind,
    /// The run id or the task id.
    pub key: String,
    pub task_id: Option<TaskId>,
    pub ledger_v: u32,
    /// When it was built (the queue's timestamp text).
    pub built_at: String,
    pub inputs_through: InputsThrough,
    /// Marked settled by whoever builds the ledger; a fold never sets it.
    pub is_final: bool,
    /// The life's start and end in unix milliseconds (no end while it is
    /// not settled), and how it ended.
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub outcome: Option<String>,
    pub totals: Totals,
    pub intervals: Vec<Interval>,
    /// A run's claim attributes, whatever their keys; empty for a task.
    pub attributes: Map<String, Value>,
    pub after_claim: Option<AfterClaim>,
    /// What else the kind folds: a run's milestones and the run that
    /// closed its pending push, a task's ready time, runs and overlays.
    pub detail: Value,
    /// A legacy row's frozen values (and its mark: `Some`).
    pub legacy: Option<Value>,
}

impl LedgerRow {
    pub fn is_legacy(&self) -> bool {
        self.legacy.is_some()
    }

    fn new(kind: LedgerKind, key: String, task_id: Option<TaskId>, built_at: &str) -> Self {
        Self {
            kind,
            key,
            task_id,
            ledger_v: LEDGER_V,
            built_at: built_at.to_owned(),
            inputs_through: InputsThrough {
                events: EventId::new(0),
            },
            is_final: false,
            started_at: None,
            ended_at: None,
            outcome: None,
            totals: Totals::default(),
            intervals: Vec::new(),
            attributes: Map::new(),
            after_claim: None,
            detail: json!({}),
            legacy: None,
        }
    }
}

/// Whether a row (`existing`, none when there is none) is built again
/// now that its object's last event is `last_event` and the queue's
/// latest `push_finished` is `latest_push`: a missing row, one of an older
/// [`LEDGER_V`] and one an event arrived after are, and so is one that
/// ends unsettled in `push_pending` once a later push arrives (another
/// run's push closes it); a legacy row never is.
pub fn rebuilds(
    existing: Option<&LedgerRow>,
    last_event: EventId,
    latest_push: Option<EventId>,
) -> bool {
    match existing {
        None => true,
        Some(row) if row.is_legacy() => false,
        Some(row) => {
            let through = row.inputs_through.events;
            let pending = row.ended_at.is_none()
                && row
                    .intervals
                    .last()
                    .is_some_and(|last| last.phase == fold::PUSH_PENDING);
            row.ledger_v != LEDGER_V
                || through < last_event
                || (pending && latest_push.is_some_and(|push| push > through))
        }
    }
}

/// The row of the run `run` from its `events` (ascending id), the queue's
/// `push_finished` (`pushes`) and `now_ms`, built at `built_at`: folded
/// when it has `run_phase_changed`, else a legacy row of what `stats`
/// makes of it.
pub fn run_row(
    run: &str,
    events: &[RunEvent],
    pushes: &[RunEvent],
    now_ms: i64,
    built_at: &str,
) -> LedgerRow {
    let task_id = events.iter().find_map(|event| event.task_id);
    let mut row = LedgerRow::new(LedgerKind::Run, run.to_owned(), task_id, built_at);
    match fold::fold_run(events, pushes, now_ms) {
        Some(life) => fill_run(&mut row, life),
        None => fill_legacy_run(&mut row, events),
    }
    row
}

fn fill_run(row: &mut LedgerRow, life: RunLife) {
    row.inputs_through.events = life.inputs_through;
    row.started_at = Some(life.start);
    row.ended_at = life.end;
    row.outcome = life.outcome;
    row.totals = Totals::of(&life.intervals);
    row.intervals = life.intervals;
    row.attributes = life.attributes;
    row.after_claim = Some(life.after_claim);
    row.detail = json!({"milestones": life.milestones, "closed_by": life.closed_by});
}

/// A legacy run's row: `stats`' values for it, its claim's attributes
/// (or, for a claim that recorded none, the same keys from its measures)
/// and its facts after the claim from its measures (ADR-t1662-1 decisions
/// 18 and 26).
fn fill_legacy_run(row: &mut LedgerRow, events: &[RunEvent]) {
    row.inputs_through.events = events
        .iter()
        .map(|event| event.id)
        .max()
        .unwrap_or(EventId::new(0));
    let run = stats::run_stats(events)
        .into_iter()
        .find(|run| run.run_id.as_str() == row.key);
    let Some(run) = run else {
        row.legacy = Some(json!({}));
        return;
    };
    row.started_at = run.claimed_at.as_deref().and_then(stats::timestamp_millis);
    row.outcome = run.status.clone();
    let recorded = events
        .iter()
        .find(|event| event.kind == event_kind::RUN_CLAIMED)
        .and_then(|claim| claim.payload.get(ATTRIBUTES))
        .and_then(Value::as_object);
    row.attributes = match recorded {
        Some(set) => set.clone(),
        None => legacy_attributes(&run),
    };
    let measures = &run.measures;
    row.after_claim = Some(AfterClaim {
        last_provider: measures.actual_provider.clone(),
        on_codex: measures.on_codex,
        codex_model: measures.worker_model.clone().filter(|_| measures.on_codex),
    });
    row.legacy = Some(json!({
        "work": run.work,
        "validate": run.validate,
        "wait_to_land": run.wait_to_land,
        "land_phases": run.land_phases,
        "startup": run.startup,
    }));
}

/// The claim's attributes of a run claimed before they were recorded,
/// from its measures under the fields `run_claimed` holds them in, so
/// that [`claim_facts::attributes`] names them. The model is the claim's
/// only for a run that did not work on Codex (whose measures hold its
/// Codex model instead), and the claim's own provider is not among the
/// measures.
fn legacy_attributes(run: &RunStats) -> Map<String, Value> {
    let measures = &run.measures;
    let (rustc_release, rustc_host) = measures.rustc();
    let fields = json!({
        "dagq_version": measures.dagq_version,
        "claude_version": measures.claude_version,
        "codex_version": measures.codex_version,
        "rustc_release": rustc_release,
        "rustc_host": rustc_host,
        "parallel": measures.claim_parallel,
        "slots": measures.claim_slots,
        "load_avg": measures.claim_load_avg,
        "requested_provider": measures.provider,
        "worker_mode": measures.route,
        "model": measures.worker_model.clone().filter(|_| !measures.on_codex),
        "effort": measures.worker_effort,
        "group": measures.trial_group,
    });
    fields
        .as_object()
        .map(claim_facts::attributes)
        .unwrap_or_default()
}

/// The row of `task` from its `events` (ascending id, its runs' among
/// them), the queue's `push_finished` (`pushes`), the waits before a claim
/// and `now_ms`, built at `built_at`. A task with a legacy run is a legacy
/// row naming its runs.
pub fn task_row(
    task: TaskId,
    events: &[RunEvent],
    pushes: &[RunEvent],
    pre_claim: &[PreClaimInterval],
    now_ms: i64,
    built_at: &str,
) -> LedgerRow {
    let mut row = LedgerRow::new(LedgerKind::Task, task.to_string(), Some(task), built_at);
    match fold::fold_task(task, events, pushes, pre_claim, now_ms) {
        TaskFold::Life(life) => fill_task(&mut row, life),
        TaskFold::Legacy { runs } => {
            row.inputs_through.events = events
                .iter()
                .map(|event| event.id)
                .max()
                .unwrap_or(EventId::new(0));
            row.legacy = Some(json!({"runs": runs}));
        }
    }
    row
}

fn fill_task(row: &mut LedgerRow, life: TaskLife) {
    row.inputs_through.events = life.inputs_through;
    row.started_at = Some(life.start);
    row.ended_at = life.end;
    row.outcome = life.outcome;
    row.totals = Totals::of(&life.intervals);
    row.intervals = life.intervals;
    row.detail = json!({
        "ready_at": life.ready_at,
        "first_claimed": life.first_claimed,
        "runs": life.runs,
        "overlays": life.overlays,
        "overlay_ms": life.overlay_millis,
    });
}

#[cfg(test)]
mod tests;
