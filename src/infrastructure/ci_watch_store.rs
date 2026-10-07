//! The queue's records of the CI watch (ADR-t1920-1): its events, one
//! settled run's events and finding written in one transaction that
//! another supervisor's record of the same run takes, and the
//! `ci_failure` findings a task fixes.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

use super::findings::{record_in, set_status_in};
use super::sqlite::{SqliteQueue, event_row};
use crate::domain::ci_watch::{CI_CHECKED, CI_WATCH_KINDS, CiCheckRecord, CiCheckRecorded};
use crate::domain::{EventId, EventKind, FindingId, FindingStatus, RunEvent, TaskId};

impl SqliteQueue {
    /// Every event of the watch, oldest first.
    pub fn ci_watch_events(&self) -> Result<Vec<RunEvent>> {
        let kinds = CI_WATCH_KINDS
            .iter()
            .map(|kind| format!("'{kind}'"))
            .collect::<Vec<_>>()
            .join(",");
        Ok(self
            .conn
            .prepare(&format!(
                "SELECT * FROM run_events WHERE kind IN ({kinds})
                 AND task_id IS NULL AND run_id IS NULL AND goal_id IS NULL ORDER BY id"
            ))?
            .query_map([], event_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Write `record` unless the latest `ci_checked` event is no longer the
    /// one it was decided after (`None` then): of two supervisors that read
    /// the same run and attempt, one records it. The finding's evidence is the run's
    /// `ci_checked` and `ci_turned_red`, which name it back (`finding_id`,
    /// `finding_ids`); a resolved finding the run opens again is marked for
    /// a proposal anew.
    pub fn record_ci_check(&self, record: CiCheckRecord) -> Result<Option<CiCheckRecorded>> {
        let now = self.generators.clock.now();
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let latest: Option<i64> = tx
            .query_row(
                "SELECT id FROM run_events WHERE kind=?1
                 AND task_id IS NULL AND run_id IS NULL AND goal_id IS NULL
                 ORDER BY id DESC LIMIT 1",
                [CI_CHECKED],
                |r| r.get(0),
            )
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
                let outcome = record_in(&tx, &finding, now)?;
                let id = outcome.finding.id;
                if outcome.changed.contains(&"status")
                    && outcome.finding.status == FindingStatus::Open
                {
                    tx.execute(
                        "UPDATE findings SET propose_requested_at=?2 WHERE id=?1",
                        params![id, now],
                    )?;
                }
                tx.execute(
                    "UPDATE run_events SET payload=json_set(payload,'$.finding_id',?2) WHERE id=?1",
                    params![checked, id],
                )?;
                if let Some(red) = red {
                    tx.execute(
                        "UPDATE run_events SET payload=json_set(payload,'$.finding_ids',json_array(?2))
                         WHERE id=?1",
                        params![red, id],
                    )?;
                }
                Some(id)
            }
            None => None,
        };
        let mut resolved = Vec::new();
        for (id, reason) in record.resolve {
            let id = FindingId::new(id);
            if resolvable(&tx, id)? {
                set_status_in(&tx, id, FindingStatus::Resolved, &reason, "runtime", now)?;
                resolved.push(id);
            }
        }
        tx.commit()?;
        Ok(Some(CiCheckRecorded {
            event: checked,
            finding,
            resolved,
        }))
    }

    /// The `ci_failure` findings `task` fixes: linked to the proposal the
    /// task belongs to, or dismissed as covered by it (ADR-t1920-1
    /// decision 5).
    pub fn ci_failure_findings_of(&self, task: TaskId) -> Result<Vec<FindingId>> {
        Ok(self
            .conn
            .prepare(
                "SELECT f.id FROM findings f WHERE f.kind=?2 AND (f.covered_by_task=?1
                   OR (f.proposal_id IS NOT NULL
                       AND f.proposal_id=(SELECT proposal_id FROM tasks WHERE id=?1)))
                 ORDER BY f.id",
            )?
            .query_map(params![task, crate::domain::ci_watch::FINDING_KIND], |r| {
                r.get(0)
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
}

/// Insert a queue event of the watch.
fn insert(conn: &Connection, kind: EventKind, payload: &Value) -> Result<EventId> {
    conn.execute(
        "INSERT INTO run_events(kind,payload,actor_role,actor_id,requested_by)
         VALUES (?1,?2,dagq_actor_role(),dagq_actor_id(),dagq_requested_by())",
        params![kind.as_str(), serde_json::to_string(payload)?],
    )?;
    Ok(EventId::new(conn.last_insert_rowid()))
}

/// Whether the runtime resolves the finding: still `open`, with no planner
/// of the runtime's open for it (one that is open decides it).
fn resolvable(conn: &Connection, id: FindingId) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT status='open' AND NOT EXISTS(SELECT 1 FROM planners p
               WHERE p.finding_id=f.id AND p.closed_at IS NULL)
             FROM findings f WHERE f.id=?1",
            [id],
            |r| r.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false))
}
