//! The breaches of the KPIs' targets and the push's attention as queue
//! events (ADR-0051 decisions 18 and 23). Each record reads the latest
//! state and writes in one `BEGIN IMMEDIATE`, so of two supervisors that
//! judged the same breach one records it, and a breach's immediate push is
//! counted against the day's limit once.
use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};

use super::sqlite::SqliteQueue;
use crate::domain::EventId;
use crate::domain::kpi::push::{
    KPI_BREACH_KINDS, KPI_BREACH_STARTED, KPI_PUSH_ABANDONED, KPI_PUSH_ATTENTION_KINDS, breach_key,
};

/// Every breach event, oldest first: (id, kind, payload).
fn breach_events(tx: &rusqlite::Connection) -> Result<Vec<(EventId, String, Value)>> {
    let mut statement = tx.prepare(
        "SELECT id, kind, payload FROM run_events WHERE kind IN (?1, ?2)
           AND run_id IS NULL AND task_id IS NULL AND goal_id IS NULL ORDER BY id",
    )?;
    let rows = statement.query_map(params![KPI_BREACH_KINDS[0], KPI_BREACH_KINDS[1]], |r| {
        Ok((
            EventId::new(r.get(0)?),
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut events = Vec::new();
    for row in rows {
        let (id, kind, payload) = row?;
        events.push((
            id,
            kind,
            serde_json::from_str(&payload).unwrap_or(Value::Null),
        ));
    }
    Ok(events)
}

/// The breaches started and not resolved: the latest event of each
/// period, KPI and stratum when it is a start, with its id.
fn open_in(conn: &rusqlite::Connection) -> Result<Vec<(EventId, Value)>> {
    let mut latest: Vec<(EventId, String, Value)> = Vec::new();
    for (id, kind, payload) in breach_events(conn)? {
        let key = breach_key(&payload);
        latest.retain(|(_, _, kept)| breach_key(kept) != key);
        latest.push((id, kind, payload));
    }
    Ok(latest
        .into_iter()
        .filter(|(_, kind, _)| kind == KPI_BREACH_STARTED)
        .map(|(id, _, payload)| (id, payload))
        .collect())
}

impl SqliteQueue {
    /// The breaches started and not resolved, each its
    /// `kpi_breach_started` payload.
    pub fn kpi_breaches_open(&self) -> Result<Vec<Value>> {
        Ok(open_in(&self.conn)?
            .into_iter()
            .map(|(_, payload)| payload)
            .collect())
    }

    /// [`Self::kpi_breaches_open`] with each `kpi_breach_started` event's
    /// id: the evidence of the observer's `kpi` finding (ADR-0051
    /// decision 24).
    pub fn kpi_breach_events_open(&self) -> Result<Vec<(EventId, Value)>> {
        open_in(&self.conn)
    }

    /// Record `kind` (`kpi_breach_started` / `kpi_breach_resolved`) with
    /// `payload` unless the breach of its period, KPI and stratum already
    /// stands so; returns the payload recorded. A start is marked
    /// `pushed` when `push_day` (the host's local day and the day's limit)
    /// allows one more immediate push that day.
    pub fn record_kpi_breach(
        &self,
        kind: &str,
        mut payload: Value,
        push_day: Option<(i64, usize)>,
    ) -> Result<Option<Value>> {
        if !KPI_BREACH_KINDS.contains(&kind) {
            bail!("{kind} is not a breach event");
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let key = breach_key(&payload);
        let open = open_in(&tx)?
            .iter()
            .any(|(_, open)| breach_key(open) == key);
        if open == (kind == KPI_BREACH_STARTED) {
            tx.commit()?;
            return Ok(None);
        }
        if kind == KPI_BREACH_STARTED {
            let pushed = push_day.is_some_and(|(day, limit)| {
                let today = breach_events(&tx).map_or(0, |events| {
                    events
                        .iter()
                        .filter(|(_, kind, event)| {
                            kind == KPI_BREACH_STARTED
                                && event.get("pushed") == Some(&Value::Bool(true))
                                && event.get("day").and_then(Value::as_i64) == Some(day)
                        })
                        .count()
                });
                today < limit
            });
            payload["pushed"] = json!(pushed);
            payload["day"] = json!(push_day.map(|(day, _)| day));
        }
        tx.execute(
            "INSERT INTO run_events(kind,payload,actor_role,actor_id,requested_by)
             VALUES (?1,?2,dagq_actor_role(),dagq_actor_id(),dagq_requested_by())",
            params![kind, serde_json::to_string(&payload)?],
        )?;
        tx.commit()?;
        Ok(Some(payload))
    }

    /// Record `kpi_push_abandoned` with `payload` unless one is recorded
    /// since the latest `kpi_push_sent`: a failure that goes on keeps the
    /// one attention. `false` when it was not recorded.
    pub fn record_kpi_push_abandoned(&self, payload: Value) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let latest: Option<String> = tx
            .query_row(
                "SELECT kind FROM run_events WHERE kind IN (?1, ?2)
                   AND run_id IS NULL AND task_id IS NULL AND goal_id IS NULL
                 ORDER BY id DESC LIMIT 1",
                params![KPI_PUSH_ATTENTION_KINDS[0], KPI_PUSH_ATTENTION_KINDS[1]],
                |r| r.get(0),
            )
            .optional()?;
        let record = latest.as_deref() != Some(KPI_PUSH_ABANDONED);
        if record {
            tx.execute(
                "INSERT INTO run_events(kind,payload,actor_role,actor_id,requested_by)
             VALUES (?1,?2,dagq_actor_role(),dagq_actor_id(),dagq_requested_by())",
                params![KPI_PUSH_ABANDONED, serde_json::to_string(&payload)?],
            )?;
        }
        tx.commit()?;
        Ok(record)
    }
}
