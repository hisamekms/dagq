//! The queue's records of the CI watch (ADR-t1920-1): its events, one
//! settled run's events and finding written in one transaction that
//! another supervisor's record of the same run takes, the runs of the
//! scheduled workflows it declares (ADR-t3807-1) recorded the same way,
//! the records of how old the declared workflows' latest successes are,
//! kept unique per workflow and success, and the `ci_failure` findings a
//! task fixes.
//!
//! The watch's events have no run, task or goal and most reads hold the
//! write lock, so every read names `events_by_kind` (why:
//! [`super::kpi_push`]).
//!
//! Known failing tests are a view derived from `ci_checked`, not a table.
//! Recording one run's events, finding and runtime resolution is one write
//! transaction: it rereads the latest `ci_checked` and writes nothing if
//! another supervisor already recorded the run. A finding's `covered_by_task`
//! has no foreign key; the runtime writes only a task ID it just read.

use super::sql_dialect::{JsonType, SqlDialect};
use super::sql_exec::{OptionalRow, SqlRead, SqlWrite, sql_params};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::BTreeMap;

use super::event_bodies::set_event_body_field;
use super::findings::{record_in, set_status_in};
use super::sqlite::SqliteQueue;
use super::store_rows::event_row;
use crate::domain::ci_watch::freshness::{FRESHNESS_KINDS, FreshnessRecord};
use crate::domain::ci_watch::scheduled::{CI_SCHEDULED_CHECKED, ScheduledCheckRecord};
use crate::domain::ci_watch::{CI_CHECKED, CI_WATCH_KINDS, CiCheckRecord, CiCheckRecorded};
use crate::domain::{EventId, EventKind, FindingId, FindingStatus, NewFinding, RunEvent, TaskId};

/// The events of the kind `?1`, oldest first.
pub(super) const EVENTS_OF_KIND_SQL: &str = "SELECT * FROM run_events INDEXED BY events_by_kind
     WHERE kind=?1 AND task_id IS NULL AND run_id IS NULL AND goal_id IS NULL ORDER BY id";

/// The id of the latest event of the kind `?1`.
pub(super) const LATEST_OF_KIND_SQL: &str = "SELECT id FROM run_events INDEXED BY events_by_kind
     WHERE kind=?1 AND task_id IS NULL AND run_id IS NULL AND goal_id IS NULL
     ORDER BY id DESC LIMIT 1";

/// The `ci_workflow_stale` (`?1`) and `ci_workflow_fresh` (`?2`) events,
/// oldest first.
pub(super) const FRESHNESS_EVENTS_SQL: &str = "SELECT * FROM run_events INDEXED BY events_by_kind
     WHERE kind IN (?1,?2) AND task_id IS NULL AND run_id IS NULL AND goal_id IS NULL ORDER BY id";

/// Every event of the watch, oldest first.
pub(super) fn watch_events_sql() -> String {
    let kinds = CI_WATCH_KINDS
        .iter()
        .map(|kind| format!("'{kind}'"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "SELECT * FROM run_events INDEXED BY events_by_kind WHERE kind IN ({kinds})
         AND task_id IS NULL AND run_id IS NULL AND goal_id IS NULL ORDER BY id"
    )
}

/// The latest freshness record (kinds `?1`, `?2`) of the workflow `?3`.
pub(super) fn latest_freshness_sql(dialect: SqlDialect) -> String {
    format!(
        "SELECT id, kind FROM run_events INDEXED BY events_by_kind WHERE kind IN (?1,?2)
         AND task_id IS NULL AND run_id IS NULL AND goal_id IS NULL
         AND {}=?3
         ORDER BY id DESC LIMIT 1",
        dialect.json_field("payload", "$.workflow", JsonType::Text)
    )
}

/// Whether a record of the kind `?1` of the workflow `?2` and its run `?3`
/// is there.
pub(super) fn freshness_there_sql(dialect: SqlDialect) -> String {
    format!(
        "SELECT EXISTS(SELECT 1 FROM run_events INDEXED BY events_by_kind WHERE kind=?1
         AND task_id IS NULL AND run_id IS NULL AND goal_id IS NULL
         AND {}=?2
         AND {} IS ?3)",
        dialect.json_field("payload", "$.workflow", JsonType::Text),
        dialect.json_field("payload", "$.run_id", JsonType::Integer)
    )
}

impl SqliteQueue {
    /// Every event of the watch, oldest first.
    pub fn ci_watch_events(&self) -> Result<Vec<RunEvent>> {
        Ok(self.conn.query_map(&watch_events_sql(), &[], event_row)?)
    }

    /// Write `record` unless the latest `ci_checked` event is no longer the
    /// one it was decided after (`None` then): of two supervisors that read
    /// the same run and attempt, one records it. The finding's evidence is the run's
    /// `ci_checked` and `ci_turned_red`, which name it back (`finding_id`,
    /// `finding_ids`); a resolved finding the run opens again is marked for
    /// a proposal anew. The finding is recorded as
    /// [`SqliteQueue::record_finding`] does (the observer's `finding record`
    /// match and update), a write of the runtime's that passes no CLI
    /// authorization, by `supervisor`.
    pub fn record_ci_check(&self, record: CiCheckRecord) -> Result<Option<CiCheckRecorded>> {
        let now = self.generators.clock.now();
        let tx = self.write_tx("record_ci_check")?;
        let latest: Option<i64> = tx
            .query_row(LATEST_OF_KIND_SQL, sql_params![CI_CHECKED], |r| r.get(0))
            .optional()?;
        if latest != record.previous {
            return Ok(None);
        }
        let checked = insert(&tx, EventKind::CiChecked, &record.checked)?;
        let red = record
            .turned_red
            .map(|mut payload| {
                payload["finding_ids"] = json!([]);
                insert(&tx, EventKind::CiTurnedRed, &payload)
            })
            .transpose()?;
        if let Some(payload) = &record.turned_green {
            insert(&tx, EventKind::CiTurnedGreen, payload)?;
        }
        let finding = match record.finding {
            Some(mut finding) => {
                finding.evidence = std::iter::once(checked).chain(red).collect();
                let id = record_marked(&tx, &finding, now)?;
                set_event_body_field(&tx, checked, "finding_id", &json!(id))?;
                if let Some(red) = red {
                    set_event_body_field(&tx, red, "finding_ids", &json!([id]))?;
                }
                Some(id)
            }
            None => None,
        };
        let resolved = resolve_all(&tx, record.resolve, now)?;
        tx.commit()?;
        Ok(Some(CiCheckRecorded {
            event: checked,
            finding,
            resolved,
        }))
    }

    /// Every `ci_scheduled_checked` event, oldest first.
    pub fn ci_scheduled_events(&self) -> Result<Vec<RunEvent>> {
        Ok(self.conn.query_map(
            EVENTS_OF_KIND_SQL,
            sql_params![CI_SCHEDULED_CHECKED],
            event_row,
        )?)
    }

    /// Write `record` of a scheduled workflow's run unless the latest
    /// `ci_scheduled_checked` is no longer the one it was decided after
    /// (`None` then), as [`Self::record_ci_check`]: the event, the
    /// finding with the event as its evidence (named back by
    /// `finding_id`, marked for a proposal anew when it opens again), and
    /// the findings resolved while `open` with no planner open for them.
    pub fn record_ci_scheduled_check(
        &self,
        record: ScheduledCheckRecord,
    ) -> Result<Option<CiCheckRecorded>> {
        let now = self.generators.clock.now();
        let tx = self.write_tx("record_ci_scheduled_check")?;
        let latest: Option<i64> = tx
            .query_row(LATEST_OF_KIND_SQL, sql_params![CI_SCHEDULED_CHECKED], |r| {
                r.get(0)
            })
            .optional()?;
        if latest != record.previous {
            return Ok(None);
        }
        let checked = insert(&tx, EventKind::CiScheduledChecked, &record.checked)?;
        let finding = match record.finding {
            Some(mut finding) => {
                finding.evidence = vec![checked];
                let id = record_marked(&tx, &finding, now)?;
                set_event_body_field(&tx, checked, "finding_id", &json!(id))?;
                Some(id)
            }
            None => None,
        };
        let resolved = resolve_all(&tx, record.resolve, now)?;
        tx.commit()?;
        Ok(Some(CiCheckRecorded {
            event: checked,
            finding,
            resolved,
        }))
    }

    /// Every `ci_workflow_stale` and `ci_workflow_fresh` event, oldest
    /// first.
    pub fn ci_freshness_events(&self) -> Result<Vec<RunEvent>> {
        let [stale, fresh] = FRESHNESS_KINDS.map(EventKind::as_str);
        Ok(self
            .conn
            .query_map(FRESHNESS_EVENTS_SQL, sql_params![stale, fresh], event_row)?)
    }

    /// Write `record` in one transaction, or nothing (`None`): a stale
    /// record when the workflow's latest record is still the one it was
    /// decided after (`previous`), so a late write of an older reading
    /// does not open its success over a newer record, and no
    /// `ci_workflow_stale` of the same `workflow` and `run_id` (null with
    /// null) is there, so two supervisors and a restart start the workflow
    /// once per latest success; a fresh record when the workflow's latest
    /// record is the stale one it `closes`.
    pub fn record_ci_freshness(&self, record: FreshnessRecord) -> Result<Option<EventId>> {
        let tx = self.write_tx("record_ci_freshness")?;
        let [stale, fresh] = FRESHNESS_KINDS;
        let latest = |workflow: Option<&str>| -> Result<Option<(i64, String)>> {
            Ok(tx
                .query_row(
                    &latest_freshness_sql(tx.dialect()),
                    sql_params![stale.as_str(), fresh.as_str(), workflow],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?)
        };
        let (kind, payload) = match record {
            FreshnessRecord::Stale { previous, payload } => {
                let workflow = payload["workflow"].as_str();
                if workflow.is_none() || latest(workflow)?.map(|(id, _)| id) != previous {
                    return Ok(None);
                }
                let there: bool = tx.query_row(
                    &freshness_there_sql(tx.dialect()),
                    sql_params![stale.as_str(), workflow, payload["run_id"].as_i64()],
                    |r| r.get(0),
                )?;
                if there {
                    return Ok(None);
                }
                (stale, payload)
            }
            FreshnessRecord::Fresh { payload } => {
                let closes = payload["closes"].as_i64();
                let open = latest(payload["workflow"].as_str())?;
                if open.is_none_or(|(id, kind)| kind != stale.as_str() || Some(id) != closes) {
                    return Ok(None);
                }
                (fresh, payload)
            }
        };
        let id = insert(&tx, kind, &payload)?;
        tx.commit()?;
        Ok(Some(id))
    }

    /// The `ci_failure` findings `task` fixes: linked to the proposal the
    /// task belongs to, or dismissed as covered by it (ADR-t1920-1
    /// decision 5).
    pub fn ci_failure_findings_of(&self, task: TaskId) -> Result<Vec<FindingId>> {
        Ok(self.conn.query_map(
            "SELECT f.id FROM findings f WHERE f.kind=?2 AND (f.covered_by_task=?1
                   OR (f.proposal_id IS NOT NULL
                       AND f.proposal_id=(SELECT proposal_id FROM tasks WHERE id=?1)))
                 ORDER BY f.id",
            sql_params![task, crate::domain::ci_watch::FINDING_KIND],
            |r| r.get(0),
        )?)
    }

    /// The tasks that fix each `ci_failure` finding: those of the
    /// proposal linked to it and the one it was dismissed as covered by,
    /// canceled ones aside (ADR-t1920-1 decision 5).
    pub fn ci_failure_fix_tasks(&self) -> Result<BTreeMap<i64, Vec<TaskId>>> {
        let mut fixes: BTreeMap<i64, Vec<TaskId>> = BTreeMap::new();
        let rows = self.conn.query_map(
            "SELECT f.id, t.id FROM findings f JOIN tasks t
                   ON t.id=f.covered_by_task
                   OR (f.proposal_id IS NOT NULL AND t.proposal_id=f.proposal_id)
                 WHERE f.kind=?1 AND t.status!='canceled'
                 ORDER BY f.id, t.id",
            sql_params![crate::domain::ci_watch::FINDING_KIND],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, TaskId>(1)?)),
        )?;
        for (finding, task) in rows {
            fixes.entry(finding).or_default().push(task);
        }
        Ok(fixes)
    }
}

/// Insert a queue event of the watch.
fn insert(tx: &impl SqlWrite, kind: EventKind, payload: &Value) -> Result<EventId> {
    Ok(EventId::new(tx.insert_id(
        "INSERT INTO run_events(kind,payload,actor_role,actor_id,requested_by)
         VALUES (?1,dagq_event_body(?2),dagq_actor_role(),dagq_actor_id(),dagq_requested_by())",
        sql_params![kind.as_str(), serde_json::to_string(payload)?],
    )?))
}

/// Record `finding` (matched and updated as `finding record` does); one
/// that opens again is marked for a proposal anew. Its ID.
fn record_marked(tx: &impl SqlWrite, finding: &NewFinding, now: i64) -> Result<FindingId> {
    let outcome = record_in(tx, finding, now)?;
    let id = outcome.finding.id;
    if outcome.changed.contains(&"status") && outcome.finding.status == FindingStatus::Open {
        tx.execute(
            "UPDATE findings SET propose_requested_at=?2 WHERE id=?1",
            sql_params![id, now],
        )?;
    }
    Ok(id)
}

/// Resolve each finding of `resolve` with its reason where
/// [`resolvable`]: the ones resolved.
fn resolve_all(
    tx: &impl SqlWrite,
    resolve: Vec<(i64, String)>,
    now: i64,
) -> Result<Vec<FindingId>> {
    let mut resolved = Vec::new();
    for (id, reason) in resolve {
        let id = FindingId::new(id);
        if resolvable(tx, id)? {
            set_status_in(tx, id, FindingStatus::Resolved, &reason, "runtime", now)?;
            resolved.push(id);
        }
    }
    Ok(resolved)
}

/// Whether the runtime resolves the finding: still `open`, with no planner
/// of the runtime's open for it (one that is open decides it).
fn resolvable(tx: &impl SqlRead, id: FindingId) -> Result<bool> {
    Ok(tx
        .query_row(
            "SELECT status='open' AND NOT EXISTS(SELECT 1 FROM planners p
               WHERE p.finding_id=f.id AND p.closed_at IS NULL)
             FROM findings f WHERE f.id=?1",
            sql_params![id],
            |r| r.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false))
}
