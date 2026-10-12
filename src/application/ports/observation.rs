//! The ports of 観測と分析 (docs/design/architecture.md, section
//! "観測と分析"): the reads of the queue's records, the observer's log
//! and the marks, over the [`EventStore`] in [`super::shared`].

use super::execution::{RunCoordination, RunLog};
use super::host::SupervisorRegistry;
use super::planning::{PlanningRecords, TaskStore};
use super::shared::{AskStore, EventStore};
use crate::domain::host_metrics::{HostSample, NodeSample, SampleSource};
use crate::domain::ledger::{
    LedgerKind, LedgerRow,
    window::{LedgerWindow, WindowQuery},
};
use crate::domain::{AskId, EventId, EventKind, FindingView, RunEvent, TaskId};
use anyhow::Result;

/// What one role wrote in a window ([`ObserverLog::written_by`]): finding
/// ids it recorded, updated and closed (resolved or dismissed), its
/// asks' ids, and the findings it left without an ask.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WrittenBy {
    pub recorded: Vec<i64>,
    pub updated: Vec<i64>,
    pub closed: Vec<i64>,
    pub asks: Vec<i64>,
    /// The findings it recorded or updated and did not close that have
    /// no `blocked` ask: none it opened in the window and none open now
    /// (ADR-t451-1 decision 2, the reading it kept to the finding).
    pub without_ask: Vec<i64>,
}

/// What the observer reads of the queue's record of its observations and
/// of what it wrote (ADR-0044).
pub trait ObserverLog {
    /// The id of the last event recorded before `unix` (seconds), 0 when
    /// there is none: a cursor that reads everything from that time on.
    fn event_id_before(&self, unix: i64) -> Result<EventId>;
    /// The newest ask's ID: the mark [`Self::written_by`] counts past.
    fn ask_high_water(&self) -> Result<AskId>;
    /// What `role` wrote after the marks: the findings it recorded,
    /// updated and closed after `event_id` and its asks after `ask_id`.
    fn written_by(&self, role: &str, event_id: EventId, ask_id: AskId) -> Result<WrittenBy>;
    /// The last observation of `mode` that ran its agent: the id and the
    /// payload of its `observe_finished` (a skipped one is not).
    fn last_observation(&self, mode: &str) -> Result<Option<(EventId, serde_json::Value)>>;
    /// How many events after `after` the observer did not write itself
    /// (ADR-0044), its own spans (`span_kind`) excluded.
    fn events_besides(&self, role: &str, span_kind: &str, after: EventId) -> Result<i64>;
    /// The newest `limit` observations, newest first: each
    /// `observe_finished` with the `observe_started` of the same directory
    /// when there is one (a skipped observation has none).
    fn observations(&self, limit: usize) -> Result<Vec<(RunEvent, Option<RunEvent>)>>;
}

/// What the mark commands read and record (ADR-0051 decision 12): the
/// queue's events, to resolve `--at` and list the marks, and a new event
/// of the queue itself. Every [`EventStore`] is one.
pub trait MarkLog {
    /// Every event of the queue, oldest first.
    fn events(&self) -> Result<Vec<RunEvent>>;
    /// Record an event of the queue itself, on no task, goal or run.
    fn record_event(&self, kind: EventKind, payload: serde_json::Value) -> Result<EventId>;
}

impl<T: EventStore + ?Sized> MarkLog for T {
    fn events(&self) -> Result<Vec<RunEvent>> {
        self.all_events()
    }

    fn record_event(&self, kind: EventKind, payload: serde_json::Value) -> Result<EventId> {
        self.record_queue_event(kind, payload)
    }
}

/// The ledger (ADR-t1662-1 decisions 10 to 13, ADR-t1662-2): one row per
/// measured object of each [`LedgerKind`], a view folded from the
/// [`EventStore`] by [`crate::domain::ledger`]. Readers go through this
/// port and [`crate::domain::ledger::read::read`] (one row) or
/// [`Self::ledger_window`] (a window of every kind) only, and no decision
/// of the runtime (claim, slot, resume, landing) reads it.
pub trait LedgerStore {
    /// Write `row` in place of the row of the same kind and key, unless
    /// that one is legacy: its values stay as first written and only its
    /// records and their range follow `row`; `false` then. Writing the
    /// same row again leaves one row.
    fn write_ledger_row(&self, row: &LedgerRow) -> Result<bool>;
    /// The row of `kind` and `key`, if there is one.
    fn ledger_row(&self, kind: LedgerKind, key: &str) -> Result<Option<LedgerRow>>;
    /// Every row of `kind`, by key.
    fn ledger_rows(&self, kind: LedgerKind) -> Result<Vec<LedgerRow>>;
    /// The retention of the node rows, by the same local day as
    /// [`NodeSampleStore::remove_node_samples`]: drop from each the samples
    /// of a day before `before_day` and remove a row left with none
    /// ([`crate::domain::ledger::node::retained`]), in one transaction.
    /// Returns how many rows were removed.
    fn remove_node_rows(&self, before_day: i64) -> Result<usize>;
    /// The window `query` asks of the rows of every kind
    /// ([`crate::domain::ledger::window::select`]): the one entry the
    /// statistics read the ledger by.
    fn ledger_window(&self, query: &WindowQuery) -> Result<LedgerWindow> {
        let mut rows = Vec::new();
        for kind in LedgerKind::ALL {
            rows.extend(self.ledger_rows(kind)?);
        }
        Ok(crate::domain::ledger::window::select(rows, query))
    }
}

/// The NodeSampleStore (ADR-t1662-2 decisions 1 and 5): the permanent
/// record of each node's load, one row per node and unix second, the
/// input the measurement reads in place of the host's CSV files. Only
/// host運用's record of the host's load writes it
/// ([`crate::domain::host_metrics`]); the measurement reads it.
pub trait NodeSampleStore {
    /// Record `sample` of `node` on its local day `day` from `source`
    /// unless a row of `node` is at its second, or on the same `day` less
    /// than `apart_secs` from it
    /// ([`crate::domain::host_metrics::apart_secs`]: another supervisor of
    /// the queue sampled); whether it was written.
    fn record_node_sample(
        &self,
        node: &str,
        day: i64,
        sample: &HostSample,
        source: SampleSource,
        apart_secs: i64,
    ) -> Result<bool>;
    /// Add the samples of `node` read from the CSV `file` of the local day
    /// `day` (`csv_import`) whose second has no row of `node` yet, however
    /// near another row they are (a supervisor kept them), and mark the
    /// file imported up to the `bytes` read, in one transaction. Returns
    /// how many rows were added.
    fn import_node_samples(
        &self,
        node: &str,
        file: &str,
        day: i64,
        samples: &[HostSample],
        bytes: u64,
    ) -> Result<usize>;
    /// The CSV files of `node` marked imported, each with the bytes read.
    fn imported_files(&self, node: &str) -> Result<std::collections::BTreeMap<String, u64>>;
    /// Remove the rows and the marks of the files of a local day before
    /// `before_day`: the retention, by the day as the files go. Returns how
    /// many rows were removed.
    fn remove_node_samples(&self, before_day: i64) -> Result<usize>;
    /// The rows of `node` taken in `[from, until]` (unix seconds), in the
    /// order they were taken.
    fn node_samples(&self, node: &str, from: i64, until: i64) -> Result<Vec<NodeSample>>;
    /// Every node's rows that came in after the row `after` (their id),
    /// in the order they came in: the cursor of what builds the ledger.
    fn node_samples_after(&self, after: i64) -> Result<Vec<NodeSample>>;
}

/// What builds the ledger reads and writes (docs/design/measurement.md
/// "台帳を作る係"): the EventStore and the NodeSampleStore, its inputs, and
/// the LedgerStore. Every store of the three is one.
pub trait LedgerQueue: EventStore + LedgerStore + NodeSampleStore {}

impl<T: EventStore + LedgerStore + NodeSampleStore + ?Sized> LedgerQueue for T {}

/// What reports read and record of the queue as a whole: the findings,
/// the written reports, KPI breaches, forecasts and the CI watch.
///
/// Owned by the observation and analysis context: its reads are open to
/// every context and its `record_*` writes are its own
/// (.config/context-boundaries.txt).
pub trait QueueRecords {
    /// The findings `query` lists, larger impact first (`findings`), for
    /// the KPI report's open findings.
    fn findings(&self, query: &crate::domain::FindingQuery) -> Result<Vec<FindingView>>;
    /// The reports recorded as written (`report_written`, ADR-0051
    /// decision 20): each (period, label).
    fn reports_written(&self) -> Result<std::collections::HashSet<(String, String)>>;
    /// Record `report_written` with `payload` (its `period` and `label`)
    /// unless the same report is recorded; `false` when it is.
    fn record_report_written(&self, payload: serde_json::Value) -> Result<bool>;
    /// The KPI breaches started and not resolved (ADR-0051 decision 18),
    /// each its `kpi_breach_started` payload.
    fn kpi_breaches_open(&self) -> Result<Vec<serde_json::Value>>;
    /// Record a breach's start or end unless it already stands so; a
    /// start is marked `pushed` while `push_day` (the local day, the
    /// day's limit) allows. Returns the payload recorded.
    fn record_kpi_breach(
        &self,
        kind: EventKind,
        payload: serde_json::Value,
        push_day: Option<(i64, usize)>,
    ) -> Result<Option<serde_json::Value>>;
    /// Record `kpi_push_abandoned` unless one stands since the latest
    /// push that succeeded (ADR-0051 decision 23); `false` when it does.
    fn record_kpi_push_abandoned(&self, payload: serde_json::Value) -> Result<bool>;
    /// Record a forecast snapshot (`forecast_recorded`, ADR-0070 decision
    /// 3) unless another was recorded after `previous`; its ID, or `None`.
    fn record_forecast(
        &self,
        payload: serde_json::Value,
        previous: Option<EventId>,
    ) -> Result<Option<EventId>>;
    /// Every event of the CI watch (ADR-t1920-1), oldest first.
    fn ci_watch_events(&self) -> Result<Vec<RunEvent>>;
    /// Record one settled CI run in one write transaction unless another
    /// supervisor recorded a run since the event `record.previous`; `None`
    /// then.
    fn record_ci_check(
        &self,
        record: crate::domain::ci_watch::CiCheckRecord,
    ) -> Result<Option<crate::domain::ci_watch::CiCheckRecorded>>;
    /// The `ci_failure` findings `task` fixes (linked to its proposal, or
    /// dismissed as covered by it).
    fn ci_failure_findings_of(&self, task: TaskId) -> Result<Vec<crate::domain::FindingId>>;
    /// The tasks that fix each `ci_failure` finding, by its ID: those of
    /// the proposal linked to it and the one it was dismissed as covered
    /// by, canceled ones aside, in ID order.
    fn ci_failure_fix_tasks(&self) -> Result<std::collections::BTreeMap<i64, Vec<TaskId>>>;
}

/// The ports of the queue 観測と分析's passes of the supervisor and their
/// jobs reach: the tasks and the records of the planning tables, the runs,
/// their leases and the supervisors read, its own records and findings,
/// and the asks a finding holds by. [`super::shared::Queue`] has it as a
/// supertrait, so the loop's connection upcasts to it.
pub trait ObservationQueue:
    TaskStore
    + PlanningRecords
    + SupervisorRegistry
    + RunCoordination
    + RunLog
    + QueueRecords
    + AskStore
{
}

impl<
    T: TaskStore
        + PlanningRecords
        + SupervisorRegistry
        + RunCoordination
        + RunLog
        + QueueRecords
        + AskStore
        + ?Sized,
> ObservationQueue for T
{
}
