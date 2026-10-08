//! Reports, KPI breaches and forecasts ([`QueueRecords`]).

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
    fn record_forecast(
        &self,
        payload: serde_json::Value,
        previous: Option<EventId>,
    ) -> Result<Option<EventId>> {
        SqliteQueue::record_forecast(self, payload, previous)
    }
    fn ci_watch_events(&self) -> Result<Vec<RunEvent>> {
        SqliteQueue::ci_watch_events(self)
    }
    fn record_ci_check(
        &self,
        record: crate::domain::ci_watch::CiCheckRecord,
    ) -> Result<Option<crate::domain::ci_watch::CiCheckRecorded>> {
        SqliteQueue::record_ci_check(self, record)
    }
    fn ci_failure_findings_of(&self, task: TaskId) -> Result<Vec<crate::domain::FindingId>> {
        SqliteQueue::ci_failure_findings_of(self, task)
    }
}
