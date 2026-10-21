//! The ledger (ADR-t1662-1 decisions 10, 13, 17 and 18, ADR-t1662-2,
//! docs/design/measurement.md "台帳の形"): one row per run and per task
//! ([`fold`]), per session or job of no run ([`session`]) and per interval
//! or event of the queue as a whole ([`queue`]), folded from the
//! EventStore alone, each event into the one row [`owner`] gives it
//! ([`build`]), and per node and hour of its load ([`node`]), folded from
//! the NodeSampleStore alone; kept by the `LedgerStore`, read one row by [`read`] and a
//! window of rows by [`window`]. A view: a row is built again when
//! [`LEDGER_V`] goes up or a later event arrives ([`rebuilds`]). A legacy
//! row (a run from before `run_phase_changed`, and a task with such a run)
//! is built again too, so that its records, their range and what its
//! detail folds of them follow the events, but keeps the values `stats`
//! gave it once: the store does not write over them. Measurement only: no
//! decision of the runtime reads it.

use crate::domain::EventKind;
pub mod attributes;
mod baseline;
pub mod builder;
pub mod compare;
pub mod findings;
pub mod fold;
pub mod node;
pub mod owner;
pub mod queue;
pub mod read;
pub mod session;
pub mod settle;
pub mod steps;
#[cfg(test)]
pub(crate) mod testkit;
pub mod window;

pub use baseline::WindowEnd;

use std::borrow::Borrow;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use self::fold::{Interval, RunLife, TaskFold, TaskLife};
use super::after_claim::AfterClaim;
use super::claim_facts::{self, ATTRIBUTES};
use super::pre_claim::PreClaimInterval;
use super::stats::{self, RunStats};
use super::{EventId, RunEvent, TaskId};

/// The version of the fold: raised when what a row holds or how it is
/// folded changes, so every row is built again (a legacy row's frozen
/// values aside, which the store keeps).
pub const LEDGER_V: u32 = 4;

/// The kind of a row. The store keeps every kind in one table by this
/// column, so a new kind is a new value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    Run,
    Task,
    /// A session or a job of no run ([`session`]).
    Session,
    /// An interval or an event of the queue as a whole ([`queue`]).
    Queue,
    /// A node's load over an hour ([`node`]).
    Node,
}

impl LedgerKind {
    /// Every kind, in the order a window lists them.
    pub const ALL: [Self; 5] = [
        Self::Run,
        Self::Task,
        Self::Session,
        Self::Queue,
        Self::Node,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Task => "task",
            Self::Session => "session",
            Self::Queue => "queue",
            Self::Node => "node",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
    }
}

/// What a session or queue row is of: for a session, the kind and id of
/// its object (`runtime_planner` and the planner's id, `plan_review` and
/// the plan review's, `inbox` and none), which of its sessions or jobs it
/// is (`round`, from 1; 0 for its events outside any) and what it ran on;
/// for a queue row, the kind of interval or event and what it is about
/// (a supervisor, a finding, a run). Run and task rows have none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    pub kind: String,
    pub id: String,
    pub round: Option<i64>,
    pub route: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
}

/// How far into its inputs a row was folded: the last EventStore event
/// id, the last NodeSampleStore id for a node row and the last
/// SessionStepStore id for a run row with steps. The later kinds of input
/// (resources) add their own key beside them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputsThrough {
    pub events: EventId,
    /// For a row its supervisor's life ended (an interval or a stream with
    /// no end of its own), the last event naming that supervisor the fold
    /// read: a later one (its stop, or more of its life) may move the end
    /// ([`settle::life_input`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor: Option<EventId>,
    /// For a node row, the largest NodeSampleStore id it folded: a sample
    /// of its bucket with a larger one (a late one, an imported one) makes
    /// it dirty ([`settle::node_settlement`]). Its `events` is 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_samples: Option<i64>,
    /// For a run row, the largest SessionStepStore id of its steps it
    /// folded (or that the fold it went on from had): a step of the run
    /// with a larger one makes it dirty ([`builder::writes`]). It stays
    /// once the retention removed those steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<i64>,
}

impl InputsThrough {
    /// The last input the row was folded from, whichever kind.
    pub fn last(&self) -> EventId {
        self.supervisor
            .map_or(self.events, |life| life.max(self.events))
    }
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

/// One row of the ledger: what every kind has (its key, the version of
/// its fold, how far into its inputs it was folded, the marks of settled
/// and legacy, its times and sums, its records) and what its kind adds
/// (`detail`, `subject`). The columns of `ledger_rows` are its fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRow {
    /// `run`, `task`, `session`, `queue` or `node`.
    pub kind: LedgerKind,
    /// What the row is of, one per kind: a run's id, a task's id, a
    /// session's `<subject kind>/<subject id>/<session|job|events>/<round>`
    /// ([`session`]), a queue row's id of the event that starts it
    /// ([`queue`]), and a node row's `<node id>/<the hour's first unix
    /// second>` ([`node`]).
    pub key: String,
    pub task_id: Option<TaskId>,
    /// The version of the fold that built it ([`LEDGER_V`]): a row of an
    /// older one is built again ([`rebuilds`]), a legacy row aside.
    pub ledger_v: u32,
    /// When it was built (the queue's timestamp text).
    pub built_at: String,
    /// The first EventStore event it was folded from; with
    /// `inputs_through`, the range of ids a window matches it by.
    pub inputs_from: EventId,
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
    /// What else the kind folds: a run's milestones, the run that closed
    /// its pending push and its session steps ([`steps::DETAIL_KEY`]), a
    /// task's ready time, runs and overlays, a
    /// node row's samples.
    pub detail: Value,
    /// A legacy row's frozen values (and its mark: `Some`): a run's
    /// `stats` values under today's names (`work`, `validate`,
    /// `wait_to_land`, `land_phases`, `startup`), a task's runs. Written
    /// once; the store keeps them while the records follow the events.
    pub legacy: Option<Value>,
    /// What a session, queue or node row is of.
    pub subject: Option<Subject>,
    /// The events the row owns ([`owner`]), ascending id: every event is
    /// some one row's, so a window's rows give back the events today's
    /// functions of `stats`, `kpi` and `report` read
    /// ([`window::LedgerWindow::events`]).
    pub records: Vec<RunEvent>,
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
            inputs_from: EventId::new(0),
            inputs_through: InputsThrough {
                events: EventId::new(0),
                supervisor: None,
                node_samples: None,
                steps: None,
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
            subject: None,
            records: Vec::new(),
        }
    }

    /// Keep `records` as the events the row owns, its range of inputs
    /// reaching over them too (from the first, unless the fold read an
    /// earlier one).
    fn own(&mut self, records: Vec<RunEvent>) {
        if let Some(first) = records.first() {
            self.inputs_from = if self.inputs_from == EventId::new(0) {
                first.id
            } else {
                self.inputs_from.min(first.id)
            };
        }
        if let Some(last) = records.last() {
            self.inputs_through.events = self.inputs_through.events.max(last.id);
        }
        self.records = records;
    }
}

/// Whether a row (`existing`, none when there is none) is built again
/// now that its object's last event is `last_event` and the queue's
/// latest `push_finished` is `latest_push`: a missing row, one of an older
/// [`LEDGER_V`] and one an event arrived after are, and so is one that
/// ends unsettled in `push_pending` once a later push arrives (another
/// run's push closes it). A legacy row is built again by the same rules
/// (it has no intervals, so no pending push): its records follow, its
/// frozen values stay as the store keeps them.
pub fn rebuilds(
    existing: Option<&LedgerRow>,
    last_event: EventId,
    latest_push: Option<EventId>,
) -> bool {
    match existing {
        None => true,
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
    let owned: Vec<RunEvent> = events
        .iter()
        .filter(|event| owner::owner(event) == owner::Owner::Run)
        .filter(|event| event.run_id.as_ref().is_some_and(|id| id.as_str() == run))
        .cloned()
        .collect();
    if let Value::Object(detail) = &mut row.detail {
        detail.extend(session::run_sessions(&owned));
    }
    row.inputs_from = events.first().map_or(row.inputs_from, |first| first.id);
    row.own(owned);
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
        .find(|event| event.kind == EventKind::RunClaimed)
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
/// (by the id of their start) and `now_ms`, built at `built_at`. A task with a legacy run is a legacy
/// row naming its runs.
pub fn task_row(
    task: TaskId,
    events: &[RunEvent],
    pushes: &[RunEvent],
    pre_claim: impl IntoIterator<Item = impl Borrow<PreClaimInterval>>,
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
    let owned: Vec<RunEvent> = events
        .iter()
        .filter(|event| owner::owner(event) == owner::Owner::Task && event.task_id == Some(task))
        .cloned()
        .collect();
    row.inputs_from = events.first().map_or(row.inputs_from, |first| first.id);
    row.own(owned);
    row
}

/// Every row of `events` (the whole EventStore, ascending id) at `now_ms`,
/// built at `built_at`: a run's and a task's ([`run_row`], [`task_row`]),
/// the sessions' and jobs' of no run ([`session::session_rows`]) and the
/// queue's ([`queue::queue_rows`]). Each event is the records of exactly
/// one row ([`owner`]). What builds the ledger writes these, a legacy row
/// too: the store keeps a legacy row's values as first written and takes
/// its records, their range and its detail (its sessions, jobs and
/// Executions) from the new one.
pub fn build(events: &[RunEvent], now_ms: i64, built_at: &str) -> Vec<LedgerRow> {
    use std::collections::BTreeSet;
    let pushes: Vec<RunEvent> = events
        .iter()
        .filter(|event| event.kind == EventKind::PushFinished)
        .cloned()
        .collect();
    let pre_claim = super::pre_claim::pre_claim_intervals(events, now_ms);
    let mut runs: BTreeMap<String, Vec<RunEvent>> = BTreeMap::new();
    let mut tasks: BTreeMap<TaskId, Vec<RunEvent>> = BTreeMap::new();
    let (mut sessions, mut queue) = (Vec::new(), Vec::new());
    for event in events {
        if let Some(run) = &event.run_id {
            runs.entry(run.as_str().to_owned())
                .or_default()
                .push(event.clone());
        }
        if let Some(task) = event.task_id {
            tasks.entry(task).or_default().push(event.clone());
        }
        match owner::owner(event) {
            owner::Owner::Session => sessions.push(event.clone()),
            owner::Owner::Queue => queue.push(event.clone()),
            owner::Owner::Run | owner::Owner::Task => {}
        }
    }
    let with_rows: BTreeSet<TaskId> = events
        .iter()
        .filter(|event| owner::owner(event) == owner::Owner::Task || event.run_id.is_some())
        .filter_map(|event| event.task_id)
        .collect();
    let mut rows: Vec<LedgerRow> = runs
        .iter()
        .map(|(run, events)| run_row(run, events, &pushes, now_ms, built_at))
        .collect();
    rows.extend(
        tasks
            .iter()
            .filter(|(task, _)| with_rows.contains(task))
            .map(|(task, events)| task_row(*task, events, &pushes, &pre_claim, now_ms, built_at)),
    );
    rows.extend(session::session_rows(&sessions, built_at));
    rows.extend(queue::queue_rows(&queue, events, now_ms, built_at));
    rows
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
