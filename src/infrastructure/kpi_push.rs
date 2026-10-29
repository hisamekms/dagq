//! The breaches of the KPIs' targets and the push's attention as queue
//! events (ADR-0051 decisions 18 and 23). Each record reads the latest
//! state and writes in one `BEGIN IMMEDIATE`, so of two supervisors that
//! judged the same breach one records it, and a breach's immediate push is
//! counted against the day's limit once.
//!
//! The breach and attention events have no run, task or goal, and the
//! write lock is held while they are read, so the reads name
//! `events_by_kind`: left to the planner without `sqlite_stat1`,
//! `goal_id IS NULL` picks `events_by_goal` and walks every event without
//! a goal, and `+run_id` alone does not change that. Naming the index keeps
//! the read to the events of the two kinds whatever statistics or new
//! indexes the queue gets, and a lost index fails the prepare.
use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

use super::sqlite::SqliteQueue;
use crate::domain::kpi::push::{
    KPI_BREACH_KINDS, KPI_PUSH_ABANDONED, KPI_PUSH_ATTENTION_KINDS, breach_key,
};
use crate::domain::{EventId, EventKind, RecordedKind};

/// The breach events, oldest first, bound to [`KPI_BREACH_KINDS`].
pub const BREACH_EVENTS: &str = "SELECT id, kind, payload FROM run_events INDEXED BY events_by_kind
   WHERE kind IN (?1, ?2) AND run_id IS NULL AND task_id IS NULL AND goal_id IS NULL
   ORDER BY id";

/// The kind of the latest push attention event, bound to
/// [`KPI_PUSH_ATTENTION_KINDS`].
pub const LATEST_PUSH_ATTENTION: &str = "SELECT kind FROM run_events INDEXED BY events_by_kind
   WHERE kind IN (?1, ?2) AND run_id IS NULL AND task_id IS NULL AND goal_id IS NULL
   ORDER BY id DESC LIMIT 1";

/// Every breach event, oldest first: (id, kind, payload).
fn breach_events(tx: &rusqlite::Connection) -> Result<Vec<(EventId, RecordedKind, Value)>> {
    let mut statement = tx.prepare(BREACH_EVENTS)?;
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
            RecordedKind::from_name(&kind),
            serde_json::from_str(&payload).unwrap_or(Value::Null),
        ));
    }
    Ok(events)
}

/// The breaches started and not resolved: the latest event of each
/// period, KPI and stratum when it is a start, with its id.
fn open_in(conn: &rusqlite::Connection) -> Result<Vec<(EventId, Value)>> {
    Ok(open_among(breach_events(conn)?))
}

/// [`open_in`] over `events`, the breach events oldest first.
fn open_among(events: Vec<(EventId, RecordedKind, Value)>) -> Vec<(EventId, Value)> {
    let mut latest: Vec<(EventId, RecordedKind, Value)> = Vec::new();
    for (id, kind, payload) in events {
        let key = breach_key(&payload);
        latest.retain(|(_, _, kept)| breach_key(kept) != key);
        latest.push((id, kind, payload));
    }
    latest
        .into_iter()
        .filter(|(_, kind, _)| *kind == EventKind::KpiBreachStarted)
        .map(|(id, _, payload)| (id, payload))
        .collect()
}

/// The starts among `events` marked `pushed` on the local `day`.
fn pushed_on(events: &[(EventId, RecordedKind, Value)], day: i64) -> usize {
    events
        .iter()
        .filter(|(_, kind, event)| {
            *kind == EventKind::KpiBreachStarted
                && event.get("pushed") == Some(&Value::Bool(true))
                && event.get("day").and_then(Value::as_i64) == Some(day)
        })
        .count()
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
    /// allows one more immediate push that day: the starts marked on that
    /// day are counted in the same transaction, the day's and the week's
    /// breaches together.
    ///
    /// Under the write lock the breach events are read once
    /// ([`BREACH_EVENTS`], only the breach events by `events_by_kind`), and both
    /// whether the breach is open and the day's pushed starts come from
    /// that read.
    ///
    /// Each `period` is a breach of its own: the same KPI and stratum in
    /// breach for both the day and the week are two starts, each up for an
    /// immediate push within the day's limit, since the day's streak and
    /// the week's are judged apart and start at different times.
    pub fn record_kpi_breach(
        &self,
        kind: EventKind,
        mut payload: Value,
        push_day: Option<(i64, usize)>,
    ) -> Result<Option<Value>> {
        if !KPI_BREACH_KINDS.contains(&kind.as_str()) {
            bail!("{kind} is not a breach event");
        }
        let tx = self.write_tx("record_kpi_breach", TransactionBehavior::Immediate)?;
        let key = breach_key(&payload);
        let events = breach_events(&tx)?;
        let pushed_today = push_day.map(|(day, _)| pushed_on(&events, day));
        let open = open_among(events)
            .iter()
            .any(|(_, open)| breach_key(open) == key);
        if open == (kind == EventKind::KpiBreachStarted) {
            tx.commit()?;
            return Ok(None);
        }
        if kind == EventKind::KpiBreachStarted {
            let pushed = push_day
                .zip(pushed_today)
                .is_some_and(|((_, limit), today)| today < limit);
            payload["pushed"] = json!(pushed);
            payload["day"] = json!(push_day.map(|(day, _)| day));
        }
        tx.execute(
            "INSERT INTO run_events(kind,payload,actor_role,actor_id,requested_by)
             VALUES (?1,?2,dagq_actor_role(),dagq_actor_id(),dagq_requested_by())",
            params![kind.as_str(), serde_json::to_string(&payload)?],
        )?;
        tx.commit()?;
        Ok(Some(payload))
    }

    /// Record `kpi_push_abandoned` with `payload` unless one is recorded
    /// since the latest `kpi_push_sent`: a failure that goes on keeps the
    /// one attention. `false` when it was not recorded.
    pub fn record_kpi_push_abandoned(&self, payload: Value) -> Result<bool> {
        let tx = self.write_tx("record_kpi_push_abandoned", TransactionBehavior::Immediate)?;
        let latest: Option<String> = tx
            .query_row(
                LATEST_PUSH_ATTENTION,
                params![KPI_PUSH_ATTENTION_KINDS[0], KPI_PUSH_ATTENTION_KINDS[1]],
                |r| r.get(0),
            )
            .optional()?;
        let record = latest.as_deref() != Some(KPI_PUSH_ABANDONED);
        if record {
            tx.execute(
                "INSERT INTO run_events(kind,payload,actor_role,actor_id,requested_by)
             VALUES (?1,?2,dagq_actor_role(),dagq_actor_id(),dagq_requested_by())",
                params![
                    EventKind::KpiPushAbandoned.as_str(),
                    serde_json::to_string(&payload)?
                ],
            )?;
        }
        tx.commit()?;
        Ok(record)
    }
}
