//! The ports of 観測と分析 (docs/design/architecture.md, section
//! "観測と分析"): the reads of the queue's records, the observer's log
//! and the marks, over the [`EventStore`] in [`super::shared`].

use super::execution::{CoordinationReads, RunLog};
use super::host::SupervisorReads;
use super::planning::{PlanningRecords, TaskReads};
use super::shared::{AskStore, EventStore};
use crate::domain::host_metrics::{HostSample, NodeSample, SampleSource};
use crate::domain::ledger::{
    LedgerKind, LedgerRow,
    window::{LedgerWindow, WindowQuery},
};
use crate::domain::session_step::{
    ResourceSample, SeqRange, SessionOwner, SessionResource, SessionStep, Step, StepTarget,
    StepsRecorded,
};
use crate::domain::telemetry::{ReportRecorded, TelemetryReport};
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
    /// ([`crate::domain::ledger::node::retained`]). Only the rows it
    /// changes are written, a bounded number a transaction: with none to
    /// change nothing is written, and one stopped part way is finished by
    /// the next call. Returns how many rows were removed.
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
    /// file imported up to the `bytes` read. The rows are committed a
    /// bounded number a transaction and the mark with the last of them, so
    /// an import stopped part way leaves the file's mark as it was (none,
    /// or that of a shorter read before the file grew) and the next import
    /// of it adds the rest, none twice. Returns how many rows were
    /// added.
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

/// The SessionStepStore (ADR-t1662-2 decisions 1 and 4, ADR-t1662-3
/// decision 12): the steps of the sessions, one row per
/// `(session_id, seq)`, each with the one target its session is of. A
/// run's steps are for its ledger row; a target of no run's are kept here
/// only, read by their target. A step is kept
/// [`crate::domain::session_step::RETENTION_DAYS`] after it came in.
pub trait SessionStepStore {
    /// Record `steps` of `session_id` of `target` as received at
    /// `received_at` (unix ms), in one transaction: a step whose
    /// `(session_id, seq)` is held already, or was removed by the
    /// retention from a session that keeps a step, adds nothing, whatever
    /// it says now; steps that do not hold together
    /// ([`crate::domain::session_step::check`]) and a session whose steps
    /// came in under another target refuse the whole call, recording none
    /// ([`crate::domain::session_step::StepError`]).
    fn record_steps(
        &self,
        target: &StepTarget,
        session_id: &str,
        steps: &[Step],
        received_at: i64,
    ) -> Result<StepsRecorded>;
    /// The gaps of the seqs of `session_id`
    /// ([`crate::domain::session_step::gaps`]): a seq the retention
    /// removed is no gap, a seq that never came in is one.
    fn session_gaps(&self, session_id: &str) -> Result<Vec<SeqRange>>;
    /// At most `limit` steps that came in after the step `after` (their
    /// id), in the order they came in: the cursor of what builds the
    /// ledger.
    fn steps_after(&self, after: i64, limit: usize) -> Result<Vec<SessionStep>>;
    /// The steps of `target`, in the order they came in.
    fn target_steps(&self, target: &StepTarget) -> Result<Vec<SessionStep>>;
    /// Remove every step that came in before `before` (unix ms); a session
    /// keeping other steps keeps the seqs removed, so they open no gap and
    /// are not added again. The resource samples that came in before it go
    /// too, and the owners registered before it whose session keeps no
    /// step and no sample ([`SessionOwners`]). With none to remove nothing
    /// is written. Returns how many steps were removed.
    fn remove_steps(&self, before: i64) -> Result<usize>;
}

/// The resource samples of the sessions (ADR-t1662-3 decisions 1 and 4):
/// what an executor measured from inside where the supervisor cannot, one
/// row per `(session_id, seq)`, each with its session's target, kept and
/// removed with the steps ([`SessionStepStore::remove_steps`]).
pub trait SessionResourceStore {
    /// Record `samples` of `session_id` of `target` as received at
    /// `received_at` (unix ms), in one transaction: a sample whose
    /// `(session_id, seq)` is held already adds nothing; samples that do
    /// not hold together ([`crate::domain::session_step::check_resources`])
    /// and a session whose steps or samples came in under another target
    /// refuse the whole call, recording none.
    fn record_resources(
        &self,
        target: &StepTarget,
        session_id: &str,
        samples: &[ResourceSample],
        received_at: i64,
    ) -> Result<StepsRecorded>;
    /// The samples of `target`, in the order they came in.
    fn target_resources(&self, target: &StepTarget) -> Result<Vec<SessionResource>>;
}

/// The owners of the sessions (ADR-t1662-3 decision 10): the control side
/// registers the target a session is of before its executor reports, and
/// the queue service records a report only for the telemetry principal of
/// the session's owner, so no token takes another target's session first.
/// Only the control side registers; no use case of the service does.
pub trait SessionOwners {
    /// Register `session_id` as `target`'s at `registered_at` (unix ms).
    /// Registering it again to the same target changes nothing; a session
    /// registered to another target, or whose steps or samples came in
    /// under another, is refused
    /// ([`crate::domain::session_step::StepError::SessionOfAnotherTarget`]).
    fn register_session(
        &self,
        target: &StepTarget,
        session_id: &str,
        registered_at: i64,
    ) -> Result<()>;
    /// The owner `session_id` was registered to; `None` for one never
    /// registered (or removed by the retention).
    fn session_owner(&self, session_id: &str) -> Result<Option<SessionOwner>>;
}

/// Where an executor sends what its sessions did (ADR-t1662-3 decisions 1
/// and 2, docs/design/measurement.md "送る口"): a report of one session's
/// steps, and of the resources it measured from inside where the
/// supervisor cannot, of the one target its telemetry token was issued
/// for, every
/// [`crate::domain::telemetry::REPORT_INTERVAL_SECS`]. The adapter now is
/// the queue service's `telemetry_report` through the client mode, on the
/// host too; the agent the executor runs sends nothing. A report that does
/// not arrive changes no run's, job's or session's outcome: the executor
/// sends its steps and samples again, and the stores keep one of each
/// `(session_id, seq)`.
pub trait TelemetrySink {
    /// Send `report`: what the SessionStepStore added of its steps and
    /// samples and held already, or why it was refused. A report past its
    /// bounds ([`crate::domain::telemetry::check`]) is refused before it
    /// is sent.
    fn report(&self, report: &TelemetryReport) -> Result<ReportRecorded>;
}

/// What builds the ledger reads and writes (docs/design/measurement.md
/// "台帳を作る係"): the EventStore, the NodeSampleStore and the
/// SessionStepStore, its inputs, and the LedgerStore. Every store of the
/// four is one.
pub trait LedgerQueue: EventStore + LedgerStore + NodeSampleStore + SessionStepStore {}

impl<T: EventStore + LedgerStore + NodeSampleStore + SessionStepStore + ?Sized> LedgerQueue for T {}

/// What reports read and record of the queue as a whole: the findings,
/// the written reports, KPI breaches, forecasts and the CI watch.
///
/// Owned by the observation and analysis context: its reads are open to
/// every context and its `record_*` writes are its own
/// (src/context_boundaries/ownership.rs).
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
    TaskReads + PlanningRecords + SupervisorReads + CoordinationReads + RunLog + QueueRecords + AskStore
{
}

impl<
    T: TaskReads
        + PlanningRecords
        + SupervisorReads
        + CoordinationReads
        + RunLog
        + QueueRecords
        + AskStore
        + ?Sized,
> ObservationQueue for T
{
}
