//! Reports, KPI breaches and forecasts ([`QueueRecords`]), and the
//! replanning `status` and `doctor` read
//! ([`crate::application::health::ReplanSource`]).
use crate::infrastructure::sql_exec::TransactionEntries;

use super::*;
use crate::infrastructure::sql_dialect::JsonType;
use crate::infrastructure::sql_exec::{SqlRead, SqlWrite};

impl SqliteQueue {
    /// The reports recorded as written: (period, label) of every
    /// `report_written` (ADR-0051 decision 20).
    pub fn reports_written(&self) -> Result<std::collections::HashSet<(String, String)>> {
        let tx = self.read_entry("reports_written")?;
        let dialect = SqlRead::dialect(&tx);
        let period = dialect.json_field("payload", "$.period", JsonType::Text);
        let label = dialect.json_field("payload", "$.label", JsonType::Text);
        let rows = SqlRead::query_map(
            &tx,
            &format!(
                "SELECT {period}, {label}
             FROM run_events WHERE kind=?1",
            ),
            sql_params![REPORT_WRITTEN],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                ))
            },
        )?;
        let mut written = std::collections::HashSet::new();
        for row in rows {
            if let (Some(period), Some(label)) = row {
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
        let tx = self.write_tx("record_report_written")?;
        let recorded: bool = SqlRead::query_row(
            &tx,
            &format!(
                "SELECT EXISTS(SELECT 1 FROM run_events WHERE kind=?1
               AND {}=?2 AND {}=?3)",
                SqlRead::dialect(&tx).json_field("payload", "$.period", JsonType::Text),
                SqlRead::dialect(&tx).json_field("payload", "$.label", JsonType::Text),
            ),
            sql_params![REPORT_WRITTEN, period, label],
            |r| r.get(0),
        )?;
        if !recorded {
            SqlWrite::execute(
                &tx,
                "INSERT INTO run_events(kind,payload,actor_role,actor_id,requested_by)
             VALUES (?1,dagq_event_body(?2),dagq_actor_role(),dagq_actor_id(),dagq_requested_by())",
                sql_params![
                    EventKind::ReportWritten.as_str(),
                    serde_json::to_string(&payload)?
                ],
            )?;
        }
        tx.commit()?;
        Ok(!recorded)
    }

    /// The completed KPI checks: (period, label) of every
    /// `kpi_breach_checked` event.
    pub fn kpi_breaches_checked(&self) -> Result<std::collections::HashSet<(String, String)>> {
        let tx = self.read_entry("kpi_breaches_checked")?;
        let dialect = SqlRead::dialect(&tx);
        let period = dialect.json_field("payload", "$.period", JsonType::Text);
        let label = dialect.json_field("payload", "$.label", JsonType::Text);
        let rows = SqlRead::query_map(
            &tx,
            &format!(
                "SELECT {period}, {label}
             FROM run_events WHERE kind=?1",
            ),
            sql_params![EventKind::KpiBreachChecked.as_str()],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                ))
            },
        )?;
        let mut written = std::collections::HashSet::new();
        for row in rows {
            if let (Some(period), Some(label)) = row {
                written.insert((period, label));
            }
        }
        Ok(written)
    }

    /// Record `kpi_breach_checked` unless a check of the same `period` and
    /// `label` is recorded, in one write: of two supervisors that wrote
    /// the same check, one records it. `false` when it was recorded.
    pub fn record_kpi_breach_checked(&self, payload: serde_json::Value) -> Result<bool> {
        let (Some(period), Some(label)) = (
            payload.get("period").and_then(Value::as_str),
            payload.get("label").and_then(Value::as_str),
        ) else {
            bail!("kpi_breach_checked needs a period and a label");
        };
        let tx = self.write_tx("record_kpi_breach_checked")?;
        let recorded: bool = SqlRead::query_row(
            &tx,
            &format!(
                "SELECT EXISTS(SELECT 1 FROM run_events WHERE kind=?1
               AND {}=?2 AND {}=?3)",
                SqlRead::dialect(&tx).json_field("payload", "$.period", JsonType::Text),
                SqlRead::dialect(&tx).json_field("payload", "$.label", JsonType::Text),
            ),
            sql_params![EventKind::KpiBreachChecked.as_str(), period, label],
            |r| r.get(0),
        )?;
        if !recorded {
            SqlWrite::execute(
                &tx,
                "INSERT INTO run_events(kind,payload,actor_role,actor_id,requested_by)
             VALUES (?1,dagq_event_body(?2),dagq_actor_role(),dagq_actor_id(),dagq_requested_by())",
                sql_params![
                    EventKind::KpiBreachChecked.as_str(),
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
        let tx = self.write_tx("forecast_snapshot")?;
        let latest: Option<EventId> = SqlRead::query_row(
            &tx,
            "SELECT MAX(id) FROM run_events WHERE kind=?1",
            sql_params![FORECAST_RECORDED],
            |r| r.get(0),
        )?;
        if latest != previous {
            return Ok(None);
        }
        let id = EventId::new(SqlWrite::insert_id(
            &tx,
            "INSERT INTO run_events(kind,payload,actor_role,actor_id,requested_by)
             VALUES (?1,dagq_event_body(?2),dagq_actor_role(),dagq_actor_id(),dagq_requested_by())",
            sql_params![
                EventKind::ForecastRecorded.as_str(),
                serde_json::to_string(&payload)?
            ],
        )?);
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
    fn kpi_breaches_checked(&self) -> Result<std::collections::HashSet<(String, String)>> {
        SqliteQueue::kpi_breaches_checked(self)
    }
    fn record_kpi_breach_checked(&self, payload: serde_json::Value) -> Result<bool> {
        SqliteQueue::record_kpi_breach_checked(self, payload)
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
    fn ci_scheduled_events(&self) -> Result<Vec<RunEvent>> {
        SqliteQueue::ci_scheduled_events(self)
    }
    fn record_ci_scheduled_check(
        &self,
        record: crate::domain::ci_watch::scheduled::ScheduledCheckRecord,
    ) -> Result<Option<crate::domain::ci_watch::CiCheckRecorded>> {
        SqliteQueue::record_ci_scheduled_check(self, record)
    }
    fn ci_freshness_events(&self) -> Result<Vec<RunEvent>> {
        SqliteQueue::ci_freshness_events(self)
    }
    fn record_ci_freshness(
        &self,
        record: crate::domain::ci_watch::freshness::FreshnessRecord,
    ) -> Result<Option<EventId>> {
        SqliteQueue::record_ci_freshness(self, record)
    }
    fn record_finding_once(
        &self,
        finding: crate::domain::NewFinding,
    ) -> Result<Option<crate::domain::FindingId>> {
        SqliteQueue::record_finding_once(self, finding)
    }
    fn ci_failure_findings_of(&self, task: TaskId) -> Result<Vec<crate::domain::FindingId>> {
        SqliteQueue::ci_failure_findings_of(self, task)
    }
    fn ci_failure_fix_tasks(&self) -> Result<std::collections::BTreeMap<i64, Vec<TaskId>>> {
        SqliteQueue::ci_failure_fix_tasks(self)
    }
}

impl crate::application::health::ReplanSource for SqliteQueue {
    fn open_replans(
        &self,
    ) -> Result<
        Vec<(
            crate::domain::task_replan::ReplanOperation,
            crate::domain::TaskStatus,
        )>,
    > {
        Self::open_replans(self)
    }
    fn replan_operations(&self) -> Result<Vec<crate::domain::task_replan::ReplanOperation>> {
        Self::replan_operations(self, true)
    }
    fn task_events(&self, task: TaskId) -> Result<Vec<RunEvent>> {
        Self::task_events(self, task)
    }
    fn replan_lineage(
        &self,
        task: TaskId,
    ) -> Result<Vec<crate::domain::task_replan::ReplanOperation>> {
        Self::replan_lineage(self, task)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_breach_checks_are_recorded_once_per_period_and_label() {
        let uri = format!("file:/breach-checks-{}?vfs=memdb", uuid::Uuid::new_v4());
        let queue = SqliteQueue::init(&uri).unwrap();
        assert!(queue.kpi_breaches_checked().unwrap().is_empty());
        for (period, label) in [
            ("day", "2026-11-08"),
            ("week", "2026-W45"),
            ("day", "2026-11-09"),
        ] {
            let payload = serde_json::json!({"period": period, "label": label, "build": "build", "supervisor": "first"});
            assert!(queue.record_kpi_breach_checked(payload.clone()).unwrap());
            let mut repeated = payload;
            repeated["supervisor"] = serde_json::json!("restarted");
            assert!(!queue.record_kpi_breach_checked(repeated).unwrap());
        }
        assert_eq!(queue.kpi_breaches_checked().unwrap().len(), 3);
        let count: i64 = queue
            .conn
            .query_row(
                "SELECT COUNT(*) FROM run_events WHERE kind='kpi_breach_checked'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
        assert!(queue.reports_written().unwrap().is_empty());
        assert!(
            queue
                .record_kpi_breach_checked(serde_json::json!({"period": "day"}))
                .is_err()
        );
    }

    #[test]
    fn completed_breach_checks_do_not_wake_the_observer() {
        let uri = format!(
            "file:/breach-checks-quiet-{}?vfs=memdb",
            uuid::Uuid::new_v4()
        );
        let queue = SqliteQueue::init(&uri).unwrap();
        let after = queue
            .record_queue_event(
                EventKind::ReportWritten,
                json!({
                    "period": "day", "label": "2026-11-08"
                }),
            )
            .unwrap();
        let count = || {
            queue
                .events_besides("observer", crate::domain::sessions::OBSERVER, after)
                .unwrap()
        };
        for (period, label) in [("day", "2026-11-08"), ("week", "2026-W45")] {
            assert!(
                queue
                    .record_kpi_breach_checked(json!({
                        "period": period, "label": label, "build": "test", "supervisor": "s"
                    }))
                    .unwrap()
            );
            assert_eq!(count(), 0);
        }
        for (kind, expected) in [
            (EventKind::KpiBreachStarted, 1),
            (EventKind::KpiBreachResolved, 2),
        ] {
            queue.record_queue_event(kind, json!({})).unwrap();
            assert_eq!(count(), expected);
        }
    }
}
