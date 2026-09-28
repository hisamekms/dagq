//! Reports, KPI breaches, forecasts and the lookups `stats` joins with
//! ([`QueueRecords`]).

use super::*;

impl SqliteQueue {
    /// The reports recorded as written: (period, label) of every
    /// `report_written` (ADR-0051 decision 20).
    pub fn reports_written(&self) -> Result<std::collections::HashSet<(String, String)>> {
        let mut statement = self.conn.prepare(
            "SELECT json_extract(payload,'$.period'), json_extract(payload,'$.label')
             FROM run_events WHERE kind=?1",
        )?;
        let rows = statement.query_map([REPORT_WRITTEN], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
            ))
        })?;
        let mut written = std::collections::HashSet::new();
        for row in rows {
            if let (Some(period), Some(label)) = row? {
                written.insert((period, label));
            }
        }
        Ok(written)
    }

    /// Record `report_written` unless a report of the same `period` and
    /// `label` is recorded, in one write: of two supervisors that wrote
    /// the same report, one records it. `false` when it was recorded.
    pub fn record_report_written(&self, payload: serde_json::Value) -> Result<bool> {
        let (Some(period), Some(label)) = (
            payload.get("period").and_then(Value::as_str),
            payload.get("label").and_then(Value::as_str),
        ) else {
            bail!("report_written needs a period and a label");
        };
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let recorded: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_events WHERE kind=?1
               AND json_extract(payload,'$.period')=?2 AND json_extract(payload,'$.label')=?3)",
            params![REPORT_WRITTEN, period, label],
            |r| r.get(0),
        )?;
        if !recorded {
            tx.execute(
                "INSERT INTO run_events(kind,payload,actor_role,actor_id,requested_by)
             VALUES (?1,?2,dagq_actor_role(),dagq_actor_id(),dagq_requested_by())",
                params![
                    EventKind::ReportWritten.as_str(),
                    serde_json::to_string(&payload)?
                ],
            )?;
        }
        tx.commit()?;
        Ok(!recorded)
    }

    /// Record `forecast_recorded` (ADR-0070 decision 3) unless another
    /// snapshot was recorded after `previous` (the latest one the caller
    /// read, `None` for none), in one write: of two supervisors that took
    /// a snapshot for the same triggers, one records it. `None` when
    /// another was recorded.
    pub fn record_forecast(
        &self,
        payload: serde_json::Value,
        previous: Option<EventId>,
    ) -> Result<Option<EventId>> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let latest: Option<EventId> = tx.query_row(
            "SELECT MAX(id) FROM run_events WHERE kind=?1",
            [FORECAST_RECORDED],
            |r| r.get(0),
        )?;
        if latest != previous {
            return Ok(None);
        }
        tx.execute(
            "INSERT INTO run_events(kind,payload,actor_role,actor_id,requested_by)
             VALUES (?1,?2,dagq_actor_role(),dagq_actor_id(),dagq_requested_by())",
            params![
                EventKind::ForecastRecorded.as_str(),
                serde_json::to_string(&payload)?
            ],
        )?;
        let id = EventId::new(tx.last_insert_rowid());
        tx.commit()?;
        Ok(Some(id))
    }

    /// The goal of every task, for `stats`. A pure read.
    pub fn task_goals(&self) -> Result<HashMap<TaskId, Option<GoalId>>> {
        Ok(self
            .conn
            .prepare("SELECT id, goal_id FROM tasks")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The title of every task.
    pub fn task_titles(&self) -> Result<HashMap<TaskId, String>> {
        Ok(self
            .conn
            .prepare("SELECT id, title FROM tasks")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The kind of every task, for `stats`; a value that is not a label
    /// is read as none, as `task_row` reads it.
    pub fn task_kinds(&self) -> Result<HashMap<TaskId, Option<TaskKind>>> {
        Ok(self
            .conn
            .prepare("SELECT id, kind FROM tasks")?
            .query_map([], |row| {
                let kind: Option<String> = row.get(1)?;
                Ok((row.get(0)?, kind.and_then(|kind| kind.parse().ok())))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The change of every task (ADR-t980-1), for `stats`, `kpi` and
    /// `forecast`; a value that is not a label is read as none.
    pub fn task_changes(&self) -> Result<HashMap<TaskId, Option<TaskChange>>> {
        Ok(self
            .conn
            .prepare("SELECT id, change FROM tasks")?
            .query_map([], |row| {
                let change: Option<String> = row.get(1)?;
                Ok((row.get(0)?, change.and_then(|change| change.parse().ok())))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
}

/// The [`QueueRecords`] port over the inherent methods above, which callers
/// that hold a `SqliteQueue` keep using directly.
impl QueueRecords for SqliteQueue {
    fn findings(
        &self,
        query: &crate::domain::FindingQuery,
    ) -> Result<Vec<crate::domain::FindingView>> {
        SqliteQueue::findings(self, query)
    }
    fn reports_written(&self) -> Result<std::collections::HashSet<(String, String)>> {
        SqliteQueue::reports_written(self)
    }
    fn record_report_written(&self, payload: serde_json::Value) -> Result<bool> {
        SqliteQueue::record_report_written(self, payload)
    }
    fn kpi_breaches_open(&self) -> Result<Vec<serde_json::Value>> {
        SqliteQueue::kpi_breaches_open(self)
    }
    fn record_kpi_breach(
        &self,
        kind: EventKind,
        payload: serde_json::Value,
        push_day: Option<(i64, usize)>,
    ) -> Result<Option<serde_json::Value>> {
        SqliteQueue::record_kpi_breach(self, kind, payload, push_day)
    }
    fn record_kpi_push_abandoned(&self, payload: serde_json::Value) -> Result<bool> {
        SqliteQueue::record_kpi_push_abandoned(self, payload)
    }
    fn related_landed_commits(&self, task: TaskId, limit: usize) -> Result<Vec<String>> {
        SqliteQueue::related_landed_commits(self, task, limit)
    }
    fn related_tasks(&self, task: TaskId, limit: usize) -> Result<RelatedPage> {
        SqliteQueue::related(self, task.as_i64(), &[], limit)
    }
    fn search_documents(&self, query: &SearchQuery) -> Result<SearchPage> {
        SqliteQueue::search(self, query)
    }
    fn task_goals(&self) -> Result<HashMap<TaskId, Option<GoalId>>> {
        SqliteQueue::task_goals(self)
    }
    fn task_kinds(&self) -> Result<HashMap<TaskId, Option<TaskKind>>> {
        SqliteQueue::task_kinds(self)
    }
    fn task_changes(&self) -> Result<HashMap<TaskId, Option<TaskChange>>> {
        SqliteQueue::task_changes(self)
    }
    fn task_titles(&self) -> Result<HashMap<TaskId, String>> {
        SqliteQueue::task_titles(self)
    }
    fn draft_origins(&self) -> Result<HashMap<TaskId, crate::domain::DraftOrigin>> {
        SqliteQueue::draft_origins(self)
    }
    fn record_forecast(
        &self,
        payload: serde_json::Value,
        previous: Option<EventId>,
    ) -> Result<Option<EventId>> {
        SqliteQueue::record_forecast(self, payload, previous)
    }
}
