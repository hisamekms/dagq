//! The ports of 観測と分析 (docs/design/architecture.md, section
//! "portのmodule"): the reads of the queue's events and records, the
//! observer's log and the marks.

use super::execution::RunLog;
use crate::domain::{
    AskId, DraftOrigin, EventId, EventKind, FindingView, GoalId, RunEvent, TaskChange, TaskId,
    related::RelatedPage,
    search::{SearchPage, SearchQuery},
};
use anyhow::Result;
use std::collections::HashMap;

/// The events `events`, `timeline` and `watch` read past a cursor.
pub trait EventReads {
    /// Events with `after < id <= upto` that `filter` keeps, oldest first,
    /// at most `limit`. A pure read.
    fn events_between(
        &self,
        after: EventId,
        upto: EventId,
        filter: &crate::domain::EventFilter,
        limit: usize,
    ) -> Result<Vec<RunEvent>>;
}

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
/// of the queue itself. Every [`RunLog`] is one.
pub trait MarkLog {
    /// Every event of the queue, oldest first.
    fn events(&self) -> Result<Vec<RunEvent>>;
    /// Record an event of the queue itself, on no task, goal or run.
    fn record_event(&self, kind: EventKind, payload: serde_json::Value) -> Result<EventId>;
}

impl<T: RunLog + ?Sized> MarkLog for T {
    fn events(&self) -> Result<Vec<RunEvent>> {
        self.all_events()
    }

    fn record_event(&self, kind: EventKind, payload: serde_json::Value) -> Result<EventId> {
        self.record_queue_event(kind, payload)
    }
}

/// What reports read and record of the queue as a whole: the written
/// reports, KPI breaches, forecasts and the lookups `stats` joins runs with.
///
/// Owned by the observation and analysis context
/// (docs/design/architecture.md): its reads are open to every context and
/// its `record_*` writes are its own. The lookups of the planning tables
/// (`related_*`, `search_documents`, `task_goals`, `task_titles`,
/// `task_changes`, `draft_origins`) belong to planning and move there when
/// the ports are split by context.
pub trait QueueRecords {
    /// The commits that landed the `limit` completed tasks most related to
    /// `task` (`dagq related`, ADR-0046), for the files it is expected to
    /// touch when it declares no concrete path (ADR-0069, ADR-t1981-1).
    fn related_landed_commits(&self, task: TaskId, limit: usize) -> Result<Vec<String>>;
    /// The `limit` tasks most related to `task`, best first, with their
    /// clues, kept to `statuses` (empty: any status; `dagq related`,
    /// ADR-0046 decision 4).
    fn related_tasks(&self, task: TaskId, statuses: &[String], limit: usize)
    -> Result<RelatedPage>;
    /// The documents matching `query`, best first (`dagq search`, ADR-0046).
    fn search_documents(&self, query: &SearchQuery) -> Result<SearchPage>;
    /// The goal of every task, for `stats`.
    fn task_goals(&self) -> Result<HashMap<TaskId, Option<GoalId>>>;
    /// The title of every task, for `stats`.
    fn task_titles(&self) -> Result<HashMap<TaskId, String>>;
    /// The change of every task (none for a task without one, ADR-t980-1),
    /// for `stats`, `kpi` and `forecast`.
    fn task_changes(&self) -> Result<HashMap<TaskId, Option<TaskChange>>>;
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
    /// Where every draft the runtime or a job registered came from, for
    /// `stats`' `draft_flow`.
    fn draft_origins(&self) -> Result<HashMap<TaskId, DraftOrigin>>;
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
}
