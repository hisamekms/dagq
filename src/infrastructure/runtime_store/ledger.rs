//! The ledger's rows in queue.db (`ledger_rows`, [`LedgerStore`]).

use super::*;
use crate::application::LedgerStore;
use crate::domain::ledger::session::written::{self, SessionsWritten};
use crate::domain::ledger::{
    InputsThrough, LedgerHead, LedgerKind, LedgerRow, Measure, Subject, Totals, node,
};
use crate::domain::pre_claim::rows::Since;
use crate::domain::run_phase::{Blocker, Holds};

const BLOCKERS: [(Blocker, &str); 7] = [
    (Blocker::Queue, "blocker_queue_ms"),
    (Blocker::Ai, "blocker_ai_ms"),
    (Blocker::Compute, "blocker_compute_ms"),
    (Blocker::Human, "blocker_human_ms"),
    (Blocker::Runtime, "blocker_runtime_ms"),
    (Blocker::External, "blocker_external_ms"),
    (Blocker::Infra, "blocker_infra_ms"),
];

const HOLDS: [(Holds, &str); 3] = [
    (Holds::WorkerSlot, "holds_worker_slot_ms"),
    (Holds::LandingSlot, "holds_landing_slot_ms"),
    (Holds::None, "holds_none_ms"),
];

/// The key a tag's sum is under in [`Totals`].
fn tag(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn millis(map: &std::collections::BTreeMap<String, Measure>, key: String) -> i64 {
    map.get(&key).map_or(0, |measure| measure.duration_ms)
}

fn text(value: &impl serde::Serialize) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

/// The row of `kind` and `key` `conn` holds, if there is one.
fn held_row(conn: &rusqlite::Connection, kind: LedgerKind, key: &str) -> Result<Option<LedgerRow>> {
    conn.query_row(
        "SELECT * FROM ledger_rows WHERE kind=?1 AND key=?2",
        params![kind.as_str(), key],
        ledger_row,
    )
    .optional()?
    .transpose()
}

/// Write `mark` on `conn` as the one row of its kind
/// ([`written::KIND`]): its position as the `events` of its
/// `inputs_through`, its version as its `ledger_v`, nothing else. No
/// reader of the rows of a [`LedgerKind`] reads it.
fn write_mark(conn: &rusqlite::Connection, mark: &SessionsWritten) -> Result<()> {
    let through = text(&InputsThrough {
        events: mark.through,
        supervisor: None,
        life_ended: Vec::new(),
        node_samples: None,
        steps: None,
        resources: None,
        inside_resources: None,
    })?;
    conn.execute(
        "INSERT INTO ledger_rows(kind, key, ledger_v, built_at, inputs_through)
         VALUES (?1, ?2, ?3, '', ?4)
         ON CONFLICT(kind, key) DO UPDATE SET
           ledger_v=excluded.ledger_v, inputs_through=excluded.inputs_through",
        params![written::KIND, written::KEY, mark.ledger_v, through],
    )?;
    Ok(())
}

/// Write `row` on `conn` as [`LedgerStore::write_ledger_row`] says.
fn upsert(conn: &rusqlite::Connection, row: &LedgerRow) -> Result<bool> {
    let totals = &row.totals;
    let mut columns: Vec<(&str, rusqlite::types::Value)> = vec![
        ("kind", row.kind.as_str().to_owned().into()),
        ("key", row.key.clone().into()),
        ("task_id", row.task_id.map(|task| task.as_i64()).into()),
        ("ledger_v", i64::from(row.ledger_v).into()),
        ("built_at", row.built_at.clone().into()),
        ("inputs_through", text(&row.inputs_through)?.into()),
        ("final", i64::from(row.is_final).into()),
        ("legacy", i64::from(row.is_legacy()).into()),
        (
            "legacy_values",
            row.legacy.as_ref().map(text).transpose()?.into(),
        ),
        ("started_at", row.started_at.into()),
        ("ended_at", row.ended_at.into()),
        ("outcome", row.outcome.clone().into()),
        ("duration_ms", totals.all.duration_ms.into()),
        ("slot_ms", totals.all.slot_ms.into()),
        ("unattributed_ms", totals.unattributed_ms.into()),
        ("attempts", totals.attempts.into()),
        ("totals", text(totals)?.into()),
        ("intervals", text(&row.intervals)?.into()),
        ("attributes", text(&row.attributes)?.into()),
        (
            "after_claim",
            row.after_claim.as_ref().map(text).transpose()?.into(),
        ),
        ("detail", text(&row.detail)?.into()),
        ("inputs_from", row.inputs_from.as_i64().into()),
        ("records", text(&row.records)?.into()),
    ];
    let subject = row.subject.as_ref();
    columns.extend([
        ("subject_kind", subject.map(|s| s.kind.clone()).into()),
        ("subject_id", subject.map(|s| s.id.clone()).into()),
        ("round", subject.and_then(|s| s.round).into()),
        ("route", subject.and_then(|s| s.route.clone()).into()),
        ("provider", subject.and_then(|s| s.provider.clone()).into()),
        ("model", subject.and_then(|s| s.model.clone()).into()),
    ]);
    for (blocker, column) in BLOCKERS {
        columns.push((column, millis(&totals.by_blocker, tag(blocker)).into()));
    }
    for (holds, column) in HOLDS {
        columns.push((column, millis(&totals.by_holds, tag(holds)).into()));
    }
    let names: Vec<&str> = columns.iter().map(|(name, _)| *name).collect();
    let marks: Vec<String> = (1..=names.len()).map(|n| format!("?{n}")).collect();
    // A legacy row's values (its legacy values, times, sums and
    // attributes) are frozen; what it owns, how far it reaches and what
    // its detail folds of its records (its sessions, jobs and
    // Executions; a legacy row keeps nothing else there) are not, so
    // that every event stays some row's records and their Executions
    // are read.
    let updates: Vec<String> = names
            .iter()
            .filter(|name| !matches!(**name, "kind" | "key"))
            .map(|name| match *name {
                "records" | "inputs_from" | "inputs_through" | "built_at" | "detail" => {
                    format!("{name}=excluded.{name}")
                }
                _ => format!(
                    "{name}=CASE WHEN ledger_rows.legacy = 1 THEN ledger_rows.{name} ELSE excluded.{name} END"
                ),
            })
            .collect();
    let frozen: bool = conn
        .query_row(
            "SELECT legacy FROM ledger_rows WHERE kind=?1 AND key=?2",
            params![row.kind.as_str(), row.key],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false);
    conn.execute(
        &format!(
            "INSERT INTO ledger_rows({}) VALUES ({})
                 ON CONFLICT(kind, key) DO UPDATE SET {}",
            names.join(","),
            marks.join(","),
            updates.join(","),
        ),
        rusqlite::params_from_iter(columns.into_iter().map(|(_, value)| value)),
    )?;
    Ok(!frozen)
}

/// The read of the rows [`crate::domain::pre_claim::rows::holds`] keeps
/// (`?2` the start's ms, `?3` its first event, `?4` the end of a life),
/// with no records, at most `?6`: with `after` (`?5`) the key's range after
/// it on the primary key, so a page reads no row before it; without, `?5`
/// is NULL.
fn wait_rows_sql(after: bool) -> String {
    let from = if after { "key > ?5" } else { "?5 IS NULL" };
    format!(
        "SELECT kind, key, task_id, ledger_v, built_at, inputs_through, final,
                legacy_values, started_at, ended_at, outcome, totals, intervals,
                attributes, after_claim, detail, inputs_from, '[]' AS records,
                subject_kind, subject_id, round, route, provider, model
           FROM ledger_rows
          WHERE kind=?1 AND {from}
            AND ((subject_kind IN ('slots_full', 'claim_hold', 'claim_deferral', 'landing_hold')
                  AND (final=0 OR ended_at IS NULL OR ended_at >= ?2 OR outcome=?4
                       OR json_extract(inputs_through, '$.events') >= ?3))
                 OR (subject_kind='live_alert_stream'
                     AND (outcome IS NULL OR outcome=?4)))
          ORDER BY key LIMIT ?6"
    )
}

/// The read of the stops (`?2`) of the queue rows, each with its row's key
/// and its index among the row's records, at most `?5`: with `after` (the
/// key `?3` and the index `?4`) the key's range from it on the primary key,
/// so a page reads no row before it; without, `?3` and `?4` are NULL.
fn stops_after_sql(after: bool) -> String {
    let from = if after {
        "r.key >= ?3 AND (r.key > ?3 OR j.key > ?4)"
    } else {
        "?3 IS NULL AND ?4 IS NULL"
    };
    format!(
        "SELECT r.key, j.key, j.value FROM ledger_rows r, json_each(r.records) j
          WHERE r.kind=?1 AND {from}
            AND json_extract(j.value, '$.kind')=?2
          ORDER BY r.key, j.key LIMIT ?5"
    )
}

impl SqliteQueue {
    /// The rows [`crate::domain::pre_claim::rows::holds`] keeps for `since`,
    /// picked by their columns, at most `limit` after the key `after`
    /// (`None`: from the first), by key, each with no records.
    fn wait_rows_after(
        &self,
        since: Since,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LedgerRow>> {
        let mut statement = self.conn.prepare(&wait_rows_sql(after.is_some()))?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = statement.query_map(
            params![
                LedgerKind::Queue.as_str(),
                since.ms,
                since.id.as_i64(),
                crate::domain::claim_hold::ENDED_SUPERVISOR_GONE,
                after,
                limit,
            ],
            ledger_row,
        )?;
        rows.map(|row| row?).collect()
    }
}

impl LedgerStore for SqliteQueue {
    fn ledger_run_events(
        &self,
        run: &crate::domain::RunId,
    ) -> Result<Vec<crate::domain::RunEvent>> {
        crate::application::EventStore::run_events(self, run)
    }

    fn write_ledger_row(&self, row: &LedgerRow) -> Result<bool> {
        upsert(&self.conn, row)
    }

    /// Reads the row again and writes under one write lock, so no other
    /// writer comes between.
    fn write_ledger_row_over(
        &self,
        row: &LedgerRow,
        read: Option<&LedgerRow>,
    ) -> Result<Option<bool>> {
        let tx = self.write_tx("ledger_row_over", TransactionBehavior::Immediate)?;
        if held_row(&tx, row.kind, &row.key)?.as_ref() != read {
            return Ok(None);
        }
        let written = upsert(&tx, row)?;
        tx.commit()?;
        Ok(Some(written))
    }

    /// Every row read again and written, and the mark, under one write
    /// lock in one transaction: a row another writer wrote since its read,
    /// or a failure part way, rolls back the rows written before it.
    fn write_session_rows_over(
        &self,
        rows: &[(LedgerRow, Option<LedgerRow>)],
        mark: Option<&SessionsWritten>,
    ) -> Result<bool> {
        let tx = self.write_tx("ledger_session_rows", TransactionBehavior::Immediate)?;
        for (row, read) in rows {
            if held_row(&tx, row.kind, &row.key)?.as_ref() != read.as_ref() {
                return Ok(false);
            }
            upsert(&tx, row)?;
        }
        if let Some(mark) = mark {
            write_mark(&tx, mark)?;
        }
        tx.commit()?;
        Ok(true)
    }

    fn sessions_written(&self) -> Result<Option<SessionsWritten>> {
        self.conn
            .query_row(
                "SELECT ledger_v, inputs_through FROM ledger_rows WHERE kind=?1 AND key=?2",
                params![written::KIND, written::KEY],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
            .map(|(ledger_v, through)| {
                Ok(SessionsWritten {
                    through: serde_json::from_str::<InputsThrough>(&through)?.events,
                    ledger_v: u32::try_from(ledger_v)?,
                })
            })
            .transpose()
    }

    fn ledger_row(&self, kind: LedgerKind, key: &str) -> Result<Option<LedgerRow>> {
        held_row(&self.conn, kind, key)
    }

    fn ledger_rows(&self, kind: LedgerKind) -> Result<Vec<LedgerRow>> {
        let mut statement = self
            .conn
            .prepare("SELECT * FROM ledger_rows WHERE kind=?1 ORDER BY key")?;
        let rows = statement.query_map([kind.as_str()], ledger_row)?;
        rows.map(|row| row?).collect()
    }

    fn ledger_rows_after(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LedgerRow>> {
        let mut statement = self.conn.prepare(
            "SELECT * FROM ledger_rows WHERE kind=?1 AND (?2 IS NULL OR key > ?2)
             ORDER BY key LIMIT ?3",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = statement.query_map(params![kind.as_str(), after, limit], ledger_row)?;
        rows.map(|row| row?).collect()
    }

    /// The ids of the records come from SQLite's JSON functions over the
    /// stored array: no record is parsed here.
    fn ledger_heads(&self, kind: LedgerKind) -> Result<Vec<LedgerHead>> {
        let mut statement = self.conn.prepare(
            "SELECT r.key, r.inputs_from, r.inputs_through, r.final, r.ledger_v,
                    (SELECT COALESCE(json_group_array(json_extract(j.value, '$.id')), '[]')
                       FROM json_each(r.records) j) AS ids
               FROM ledger_rows r WHERE r.kind=?1 ORDER BY r.key",
        )?;
        let rows = statement.query_map([kind.as_str()], |r| {
            let key: String = r.get("key")?;
            let inputs_from: i64 = r.get("inputs_from")?;
            let inputs_through: String = r.get("inputs_through")?;
            let is_final: bool = r.get("final")?;
            let ledger_v: i64 = r.get("ledger_v")?;
            let ids: String = r.get("ids")?;
            Ok((|| {
                let mut records: Vec<i64> = serde_json::from_str(&ids)?;
                records.sort_unstable();
                Ok(LedgerHead {
                    kind,
                    key,
                    inputs_from: EventId::new(inputs_from),
                    inputs_through: serde_json::from_str(&inputs_through)?,
                    is_final,
                    ledger_v: u32::try_from(ledger_v)?,
                    records: records.into_iter().map(EventId::new).collect(),
                })
            })())
        })?;
        rows.map(|row| row?).collect()
    }

    /// Its columns and the ids of its records (SQLite's JSON functions over
    /// the stored array): no record is parsed here.
    fn ledger_queue_heads(&self) -> Result<Vec<crate::domain::ledger::queue::QueueHead>> {
        let mut statement = self.conn.prepare(
            "SELECT r.key, r.inputs_from, r.inputs_through, r.final, r.ledger_v,
                    (SELECT COALESCE(json_group_array(json_extract(j.value, '$.id')), '[]')
                       FROM json_each(r.records) j) AS ids,
                    r.task_id, r.subject_kind, r.subject_id, r.started_at, r.ended_at,
                    r.outcome, r.detail
               FROM ledger_rows r WHERE r.kind=?1 ORDER BY r.key",
        )?;
        let kind = LedgerKind::Queue;
        let rows = statement.query_map([kind.as_str()], |r| {
            let key: String = r.get("key")?;
            let inputs_from: i64 = r.get("inputs_from")?;
            let inputs_through: String = r.get("inputs_through")?;
            let is_final: bool = r.get("final")?;
            let ledger_v: i64 = r.get("ledger_v")?;
            let ids: String = r.get("ids")?;
            let task_id: Option<i64> = r.get("task_id")?;
            let subject_kind: Option<String> = r.get("subject_kind")?;
            let subject_id: Option<String> = r.get("subject_id")?;
            let started_at: Option<i64> = r.get("started_at")?;
            let ended_at: Option<i64> = r.get("ended_at")?;
            let outcome: Option<String> = r.get("outcome")?;
            let detail: String = r.get("detail")?;
            Ok((|| {
                let mut records: Vec<i64> = serde_json::from_str(&ids)?;
                records.sort_unstable();
                Ok(crate::domain::ledger::queue::QueueHead {
                    head: LedgerHead {
                        kind,
                        key,
                        inputs_from: EventId::new(inputs_from),
                        inputs_through: serde_json::from_str(&inputs_through)?,
                        is_final,
                        ledger_v: u32::try_from(ledger_v)?,
                        records: records.into_iter().map(EventId::new).collect(),
                    },
                    task_id: task_id.map(crate::domain::TaskId::new),
                    subject_kind: subject_kind.unwrap_or_default(),
                    subject_id: subject_id.unwrap_or_default(),
                    started_at,
                    ended_at,
                    outcome,
                    detail: serde_json::from_str(&detail)?,
                })
            })())
        })?;
        rows.map(|row| row?).collect()
    }

    /// Only the anchors of each session row are parsed: SQLite picks the
    /// first record and the `session_closed` ones out of the stored array.
    fn ledger_session_anchors(&self) -> Result<Vec<(String, crate::domain::RunEvent)>> {
        let mut statement = self.conn.prepare(
            "SELECT r.key, j.value FROM ledger_rows r, json_each(r.records) j
              WHERE r.kind=?1
                AND (j.key = 0 OR json_extract(j.value, '$.kind') = 'session_closed')
              ORDER BY r.key, j.key",
        )?;
        let rows = statement.query_map([LedgerKind::Session.as_str()], |r| {
            let key: String = r.get(0)?;
            let event: String = r.get(1)?;
            Ok((key, event))
        })?;
        rows.map(|row| {
            let (key, event) = row?;
            Ok((key, serde_json::from_str(&event)?))
        })
        .collect()
    }

    /// One aggregate over the task rows not settled: no row is read.
    fn ledger_tasks_open_since(&self) -> Result<Since> {
        let (ms, id): (Option<i64>, Option<i64>) = self.conn.query_row(
            "SELECT MIN(COALESCE(started_at, -9223372036854775808)), MIN(inputs_from)
               FROM ledger_rows WHERE kind=?1 AND final=0",
            [LedgerKind::Task.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(match (ms, id) {
            (Some(ms), Some(id)) => Since {
                ms,
                id: EventId::new(id),
            },
            _ => Since::NONE,
        })
    }

    /// SQLite picks the rows [`crate::domain::pre_claim::rows::holds`] keeps by their
    /// columns, and gives each with no records: none is parsed here.
    fn ledger_wait_rows(&self, since: Since) -> Result<Vec<LedgerRow>> {
        self.wait_rows_after(since, None, usize::MAX)
    }

    /// SQLite reads from the key `after` on by the primary key and stops at
    /// `limit`: a page reads no row before it nor past it.
    fn ledger_wait_rows_after(
        &self,
        since: Since,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LedgerRow>> {
        self.wait_rows_after(since, after, limit)
    }

    /// SQLite picks the rows [`crate::domain::pre_claim::rows::lacks`]
    /// keeps by their columns, and gives each with no records.
    fn ledger_settled_waits(
        &self,
        held: Since,
        from: Since,
        until_ms: i64,
    ) -> Result<Vec<LedgerRow>> {
        let mut statement = self.conn.prepare(
            "SELECT kind, key, task_id, ledger_v, built_at, inputs_through, final,
                    legacy_values, started_at, ended_at, outcome, totals, intervals,
                    attributes, after_claim, detail, inputs_from, '[]' AS records,
                    subject_kind, subject_id, round, route, provider, model
               FROM ledger_rows
              WHERE kind=?1
                AND subject_kind IN ('slots_full', 'claim_hold', 'claim_deferral')
                AND final=1 AND ended_at < ?2 AND (outcome IS NULL OR outcome<>?4)
                AND json_extract(inputs_through, '$.events') < ?3
                AND started_at <= ?7
                AND (ended_at >= ?5 OR json_extract(inputs_through, '$.events') >= ?6)
              ORDER BY key",
        )?;
        let rows = statement.query_map(
            params![
                LedgerKind::Queue.as_str(),
                held.ms,
                held.id.as_i64(),
                crate::domain::claim_hold::ENDED_SUPERVISOR_GONE,
                from.ms,
                from.id.as_i64(),
                until_ms,
            ],
            ledger_row,
        )?;
        rows.map(|row| row?).collect()
    }

    /// SQLite picks the stops out of the stored arrays: no other record is
    /// parsed.
    fn ledger_supervisor_stops(&self) -> Result<Vec<crate::domain::RunEvent>> {
        let mut statement = self.conn.prepare(
            "SELECT j.value FROM ledger_rows r, json_each(r.records) j
              WHERE r.kind=?1 AND json_extract(j.value, '$.kind')=?2
              ORDER BY json_extract(j.value, '$.id')",
        )?;
        let rows = statement.query_map(
            params![
                LedgerKind::Queue.as_str(),
                crate::domain::EventKind::SupervisorStopped.as_str()
            ],
            |r| r.get::<_, String>(0),
        )?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    /// SQLite picks the stops out of the stored arrays of the rows from the
    /// key of `after` on, by the primary key, and stops at `limit`: a page
    /// parses no other record and reads no row before it nor past it.
    fn ledger_supervisor_stops_after(
        &self,
        after: Option<(&str, i64)>,
        limit: usize,
    ) -> Result<Vec<((String, i64), crate::domain::RunEvent)>> {
        let mut statement = self.conn.prepare(&stops_after_sql(after.is_some()))?;
        let (key, index) = after.unzip();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = statement.query_map(
            params![
                LedgerKind::Queue.as_str(),
                crate::domain::EventKind::SupervisorStopped.as_str(),
                key,
                index,
                limit
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )?;
        rows.map(|row| {
            let (key, index, event) = row?;
            Ok(((key, index), serde_json::from_str(&event)?))
        })
        .collect()
    }

    /// Read the node rows with no lock, then write only those the
    /// retention changes, [`ROWS_PER_TRANSACTION`] a transaction
    /// ([`in_batches`]): with none to change (the day's retention already
    /// ran, by another process too) nothing is written and no write lock
    /// taken. Each batch reads its rows again under its lock, so a row
    /// another writer changed since the first read is judged as it is now,
    /// and a retention stopped between batches is finished by the next
    /// (one whose rows are gone or kept as they are changes nothing).
    fn remove_node_rows(&self, before_day: i64) -> Result<usize> {
        let changed: Vec<String> = self
            .ledger_rows(LedgerKind::Node)?
            .into_iter()
            .filter(|row| node::retained(row, before_day).as_ref() != Some(row))
            .map(|row| row.key)
            .collect();
        if changed.is_empty() {
            return Ok(0);
        }
        let mut removed = 0;
        in_batches(self, "ledger_node_retention", &changed, |tx, keys, _| {
            for key in keys {
                let Some(row) = tx
                    .query_row(
                        "SELECT * FROM ledger_rows WHERE kind=?1 AND key=?2",
                        params![LedgerKind::Node.as_str(), key],
                        ledger_row,
                    )
                    .optional()?
                    .transpose()?
                else {
                    continue;
                };
                match node::retained(&row, before_day) {
                    None => {
                        removed += tx.execute(
                            "DELETE FROM ledger_rows WHERE kind=?1 AND key=?2",
                            params![LedgerKind::Node.as_str(), key],
                        )?;
                    }
                    Some(kept) if kept != row => {
                        tx.execute(
                            "UPDATE ledger_rows SET detail=?3 WHERE kind=?1 AND key=?2",
                            params![LedgerKind::Node.as_str(), key, text(&kept.detail)?],
                        )?;
                    }
                    Some(_) => {}
                }
            }
            Ok(())
        })?;
        Ok(removed)
    }
}

/// A row of `ledger_rows`; its JSON is read in the outer result, so a row
/// that does not read fails the read rather than reading as a default.
fn ledger_row(r: &Row) -> rusqlite::Result<Result<LedgerRow>> {
    let json = |column: &str| -> rusqlite::Result<String> { r.get(column) };
    let optional = |column: &str| -> rusqlite::Result<Option<String>> { r.get(column) };
    let kind: String = r.get("kind")?;
    let key: String = r.get("key")?;
    let task_id: Option<i64> = r.get("task_id")?;
    let ledger_v: i64 = r.get("ledger_v")?;
    let built_at: String = r.get("built_at")?;
    let inputs_through = json("inputs_through")?;
    let is_final: bool = r.get("final")?;
    let legacy = optional("legacy_values")?;
    let started_at: Option<i64> = r.get("started_at")?;
    let ended_at: Option<i64> = r.get("ended_at")?;
    let outcome: Option<String> = r.get("outcome")?;
    let (totals, intervals, attributes, detail) = (
        json("totals")?,
        json("intervals")?,
        json("attributes")?,
        json("detail")?,
    );
    let after_claim = optional("after_claim")?;
    let inputs_from: i64 = r.get("inputs_from")?;
    let records = json("records")?;
    let subject_kind: Option<String> = r.get("subject_kind")?;
    let subject = match subject_kind {
        Some(kind) => Some(Subject {
            kind,
            id: r
                .get::<_, Option<String>>("subject_id")?
                .unwrap_or_default(),
            round: r.get("round")?,
            route: r.get("route")?,
            provider: r.get("provider")?,
            model: r.get("model")?,
        }),
        None => None,
    };
    Ok((|| {
        Ok(LedgerRow {
            kind: LedgerKind::parse(&kind).ok_or_else(|| anyhow!("unknown ledger kind {kind}"))?,
            key,
            task_id: task_id.map(TaskId::new),
            ledger_v: u32::try_from(ledger_v)?,
            built_at,
            inputs_through: serde_json::from_str::<InputsThrough>(&inputs_through)?,
            is_final,
            started_at,
            ended_at,
            outcome,
            totals: serde_json::from_str::<Totals>(&totals)?,
            intervals: serde_json::from_str(&intervals)?,
            attributes: serde_json::from_str(&attributes)?,
            after_claim: after_claim
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?,
            detail: serde_json::from_str(&detail)?,
            legacy: legacy.as_deref().map(serde_json::from_str).transpose()?,
            inputs_from: EventId::new(inputs_from),
            subject,
            records: serde_json::from_str(&records)?,
        })
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ledger::{self, LEDGER_V};
    use crate::domain::{EventId, RunEvent, RunId};

    const RUN: &str = "11111111-1111-4111-8111-111111111111";

    fn event(id: i64, kind: &str, payload: Value, secs: i64) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(7)),
            goal_id: None,
            run_id: Some(RunId::new(RUN).unwrap()),
            kind: kind.into(),
            payload,
            created_at: crate::domain::marks::utc_text((1_900_000_000 + secs) * 1000),
            actor: None,
        }
    }

    fn phase(id: i64, phase: &str, blocker: &str, holds: &str, secs: i64) -> RunEvent {
        event(
            id,
            "run_phase_changed",
            json!({"phase": phase, "blocker": blocker, "holds": holds,
                   "attempt": {"kind": "first", "n": 1}, "cause": "x", "v": 1}),
            secs,
        )
    }

    fn rows(queue: &SqliteQueue) -> i64 {
        queue
            .conn
            .query_row("SELECT count(*) FROM ledger_rows", [], |r| r.get(0))
            .unwrap()
    }

    /// The heads read back what the rows hold of them and the ids of their
    /// records, and the anchors each session row's first record and its
    /// closes, as the rows read whole give them; neither reads the rest of
    /// a record: a row whose records hold only their ids, which reads whole
    /// no more, gives its head.
    #[test]
    fn the_heads_and_anchors_read_no_record_but_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let session = |id: i64, kind: &str, payload: Value| RunEvent {
            task_id: None,
            run_id: None,
            ..event(id, kind, payload, id)
        };
        let events = vec![
            session(
                1,
                "session_opened",
                json!({"kind": "runtime_planner", "planner_id": 1}),
            ),
            session(2, "session_turns", json!({"opened_event_id": 1})),
            session(
                3,
                "session_closed",
                json!({"opened_event_id": 1, "session_id": "s", "model": "m"}),
            ),
            session(4, "session_closed", json!({"session_id": "out"})),
            session(5, "session_turns", json!({"session_id": "out"})),
        ];
        let rows = ledger::session::session_rows(&events, "2030-01-01T00:00:00.000Z");
        assert_eq!(rows.len(), 2);
        for row in &rows {
            queue.write_ledger_row(row).unwrap();
        }
        let mut heads: Vec<LedgerHead> = rows.iter().map(LedgerHead::of).collect();
        heads.sort_by(|a, b| a.key.cmp(&b.key));
        assert_eq!(queue.ledger_heads(LedgerKind::Session).unwrap(), heads);
        let mut anchors = ledger::session::anchors(&rows);
        anchors.sort_by(|a, b| (&a.0, a.1.id).cmp(&(&b.0, b.1.id)));
        assert_eq!(queue.ledger_session_anchors().unwrap(), anchors);
        let ids: Vec<i64> = anchors.iter().map(|(_, event)| event.id.as_i64()).collect();
        assert_eq!(ids, [1, 3, 4]);
        assert!(queue.ledger_heads(LedgerKind::Run).unwrap().is_empty());

        queue
            .conn
            .execute(
                "UPDATE ledger_rows SET records='[{\"id\":4},{\"id\":5,\"payload\":1}]'
                  WHERE key='session//events/0'",
                [],
            )
            .unwrap();
        assert!(queue.ledger_rows(LedgerKind::Session).is_err());
        let heads = queue.ledger_heads(LedgerKind::Session).unwrap();
        let held = heads
            .iter()
            .find(|head| head.key == "session//events/0")
            .unwrap();
        assert_eq!(held.records, [EventId::new(4), EventId::new(5)]);
    }

    /// The queue heads read back what the queue rows hold of themselves
    /// and the ids of their records, as the rows read whole give them,
    /// and read no record: a row whose records hold only their ids gives
    /// its head.
    #[test]
    fn the_queue_heads_read_no_record() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let (log, position) = ledger::testkit::queue_across();
        let events = &log.events[..position];
        let owned: Vec<RunEvent> = events
            .iter()
            .filter(|event| ledger::owner::owner(event) == ledger::owner::Owner::Queue)
            .cloned()
            .collect();
        let rows = ledger::queue::queue_rows(
            &owned,
            events,
            ledger::testkit::ms(40),
            "2030-01-01T00:00:00.000Z",
        );
        assert!(rows.len() >= 10);
        for row in &rows {
            queue.write_ledger_row(row).unwrap();
        }
        let mut heads: Vec<ledger::queue::QueueHead> =
            rows.iter().map(ledger::queue::QueueHead::of).collect();
        heads.sort_by(|a, b| a.head.key.cmp(&b.head.key));
        assert_eq!(queue.ledger_queue_heads().unwrap(), heads);

        queue
            .conn
            .execute(
                "UPDATE ledger_rows SET records='[{\"id\":4},{\"id\":17,\"payload\":1}]'
                  WHERE kind='queue' AND key='4'",
                [],
            )
            .unwrap();
        assert!(queue.ledger_rows(LedgerKind::Queue).is_err());
        let heads = queue.ledger_queue_heads().unwrap();
        let held = heads.iter().find(|head| head.head.key == "4").unwrap();
        assert_eq!(held.head.records, [EventId::new(4), EventId::new(17)]);
        assert_eq!(held.subject_kind, "finding");
    }

    /// The rows the waits and lives are made again from, the stops and the
    /// start of the tasks not settled are those the rows read whole give
    /// ([`LedgerStore`]'s own reading of [`LedgerStore::ledger_rows`]):
    /// the waits open, ended by a life or since a task not settled, the
    /// stops whether a row of their own or a hold's end; none of them reads
    /// another record, so a queue row whose records hold only their ids,
    /// which reads whole no more, leaves them as they are.
    #[test]
    fn the_wait_rows_and_stops_read_no_record_but_the_stops() {
        use crate::domain::ledger::testkit::{built_and_settled, ms, waits_across};
        use crate::domain::pre_claim::rows::holds;
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        assert_eq!(queue.ledger_tasks_open_since().unwrap(), Since::NONE);
        let (log, position, at) = waits_across();
        let mut events = log.events[..position].to_vec();
        // A second stop, its own row, after the one that ends b's hold.
        events.push(RunEvent {
            id: EventId::new(100),
            task_id: None,
            run_id: None,
            ..event(100, "supervisor_stopped", json!({"supervisor": "f"}), 850)
        });
        let rows = built_and_settled(&events, ms(at));
        for row in &rows {
            queue.write_ledger_row(row).unwrap();
        }
        let since = Since::of(&rows);
        assert_eq!(queue.ledger_tasks_open_since().unwrap(), since);
        let mut held: Vec<LedgerRow> = rows
            .iter()
            .filter(|row| holds(row, since))
            .map(|row| LedgerRow {
                records: Vec::new(),
                ..row.clone()
            })
            .collect();
        held.sort_by(|a, b| a.key.cmp(&b.key));
        assert!(held.len() > 5);
        let read = |queue: &SqliteQueue| {
            (
                queue.ledger_wait_rows(since).unwrap(),
                queue.ledger_supervisor_stops().unwrap(),
            )
        };
        let (wait_rows, stops) = read(&queue);
        assert_eq!(wait_rows, held);
        let ids: Vec<i64> = stops.iter().map(|stop| stop.id.as_i64()).collect();
        assert_eq!(ids, [9, 100]);
        assert_eq!(stops[0], events[8]);
        assert!(!held.iter().any(|row| row.key == "1"));

        // Every row settled, one ended before is held only by its end.
        queue
            .conn
            .execute("UPDATE ledger_rows SET final=1", [])
            .unwrap();
        let settled: Vec<LedgerRow> = rows
            .iter()
            .map(|row| LedgerRow {
                is_final: true,
                records: Vec::new(),
                ..row.clone()
            })
            .collect();
        // The id of its end alone holds it, whatever its time.
        let by_id = Since {
            ms: i64::MAX,
            id: EventId::new(6),
        };
        let keys = |since: Since| -> Vec<String> {
            queue
                .ledger_wait_rows(since)
                .unwrap()
                .into_iter()
                .map(|row| row.key)
                .collect()
        };
        for since in [since, by_id, Since::NONE] {
            let mut expected: Vec<LedgerRow> = settled
                .iter()
                .filter(|row| holds(row, since))
                .cloned()
                .collect();
            expected.sort_by(|a, b| a.key.cmp(&b.key));
            assert_eq!(queue.ledger_wait_rows(since).unwrap(), expected);
        }
        assert!(keys(by_id).contains(&"5".to_owned()));
        assert!(!keys(Since::NONE).contains(&"5".to_owned()));
        assert!(!keys(Since::NONE).contains(&"1".to_owned()));
        queue
            .conn
            .execute(
                "UPDATE ledger_rows SET final=0 WHERE kind='queue' AND key='1'",
                [],
            )
            .unwrap();
        assert!(keys(Since::NONE).contains(&"1".to_owned()));
        let wait_rows = queue.ledger_wait_rows(since).unwrap();

        queue
            .conn
            .execute(
                "UPDATE ledger_rows SET records='[{\"id\":1}]' WHERE kind='queue' AND key='1'",
                [],
            )
            .unwrap();
        assert!(queue.ledger_rows(LedgerKind::Queue).is_err());
        assert_eq!(read(&queue), (wait_rows, stops));
    }

    /// A page of the wait rows or of the stops after a place reads the rows
    /// from it on by the primary key's range, not every row of the kind
    /// from the first: the plan searches the key past `kind`.
    #[test]
    fn a_page_after_a_place_reads_the_keys_range() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let plan = |sql: &str, args: &[&dyn rusqlite::ToSql]| -> String {
            let mut statement = queue
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let rows = statement
                .query_map(args, |r| r.get::<_, String>(3))
                .unwrap();
            rows.map(Result::unwrap).collect::<Vec<_>>().join("\n")
        };
        let queue_kind = LedgerKind::Queue.as_str();
        let waits = plan(
            &wait_rows_sql(true),
            &[&queue_kind, &0, &0, &"x", &"1", &10],
        );
        assert!(waits.contains("kind=? AND key>?"), "{waits}");
        let stops = plan(
            &stops_after_sql(true),
            &[&queue_kind, &"supervisor_stopped", &"1", &0, &10],
        );
        assert!(stops.contains("kind=? AND key>?"), "{stops}");
    }

    /// The waits settled a task's fold reads besides the book made again
    /// are those the rows read whole give
    /// ([`crate::domain::pre_claim::rows::lacks`]): of the waits ended by
    /// an event before where the book holds from, those over the task's
    /// time, by their ends' times or ids and their starts; none of them
    /// reads a record, so a row whose records hold only their ids leaves
    /// them as they are.
    #[test]
    fn the_settled_waits_read_no_record() {
        use crate::domain::ledger::testkit::{built_and_settled, ms, waits_across};
        use crate::domain::pre_claim::rows::lacks;
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let (log, position, at) = waits_across();
        let rows: Vec<LedgerRow> = built_and_settled(&log.events[..position], ms(at))
            .into_iter()
            .map(|row| LedgerRow {
                is_final: true,
                ..row
            })
            .collect();
        for row in &rows {
            queue.write_ledger_row(row).unwrap();
        }
        let since = |ms, id| Since {
            ms,
            id: EventId::new(id),
        };
        let asks = [
            (Since::NONE, since(i64::MIN, 0), i64::MAX),
            (Since::NONE, since(ms(100), 3), i64::MAX),
            (since(ms(1000), 100), since(ms(100), 3), ms(115)),
            (since(ms(140), 7), since(i64::MAX, 6), i64::MAX),
            (since(ms(100), 3), since(i64::MIN, 0), i64::MAX),
        ];
        let read = |queue: &SqliteQueue| -> Vec<Vec<LedgerRow>> {
            asks.iter()
                .map(|(held, from, until)| {
                    queue.ledger_settled_waits(*held, *from, *until).unwrap()
                })
                .collect()
        };
        let expected: Vec<Vec<LedgerRow>> = asks
            .iter()
            .map(|(held, from, until)| {
                let mut kept: Vec<LedgerRow> = rows
                    .iter()
                    .filter(|row| lacks(row, *held, *from, *until))
                    .map(|row| LedgerRow {
                        records: Vec::new(),
                        ..row.clone()
                    })
                    .collect();
                kept.sort_by(|a, b| a.key.cmp(&b.key));
                kept
            })
            .collect();
        let keys: Vec<Vec<&str>> = expected
            .iter()
            .map(|rows| rows.iter().map(|row| row.key.as_str()).collect())
            .collect();
        // d's slots full freed (1) and e's (5); a slots full ended by a
        // life (7, 10) or open (13, 19), a hold or a deferral open and a
        // hold on the landings are never among them.
        assert_eq!(
            keys,
            [vec!["1", "5"], vec!["5"], vec![], vec!["5"], vec!["1"]]
        );
        assert_eq!(read(&queue), expected);

        queue
            .conn
            .execute(
                "UPDATE ledger_rows SET records='[{\"id\":5}]' WHERE kind='queue' AND key='5'",
                [],
            )
            .unwrap();
        assert!(queue.ledger_rows(LedgerKind::Queue).is_err());
        assert_eq!(read(&queue), expected);
    }

    /// A folded run's row round-trips with its sums in their columns, and
    /// writing it again (or a newer fold of it) keeps one row.
    #[test]
    fn a_run_row_is_written_once_per_run_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let events = vec![
            event(
                1,
                "run_claimed",
                json!({"attributes": {"build": "1", "x.new": "kept"}}),
                0,
            ),
            phase(2, "provisioning", "infra", "worker_slot", 0),
            phase(3, "worker", "ai", "worker_slot", 10),
            phase(4, "landing_answer", "human", "none", 70),
            phase(5, "ended", "runtime", "none", 100),
        ];
        let row = ledger::run_row(RUN, &events, &[], 0, "2030-01-01T00:00:00.000Z");
        assert!(queue.write_ledger_row(&row).unwrap());
        assert!(queue.write_ledger_row(&row).unwrap());
        assert_eq!(rows(&queue), 1);
        let read = queue.ledger_row(LedgerKind::Run, RUN).unwrap().unwrap();
        assert_eq!(read, row);
        assert_eq!(read.inputs_through.events, EventId::new(5));
        assert_eq!(read.attributes["x.new"], "kept");
        let (duration, slot, ai, human, none, legacy, v): (i64, i64, i64, i64, i64, i64, i64) =
            queue
                .conn
                .query_row(
                    "SELECT duration_ms, slot_ms, blocker_ai_ms, blocker_human_ms, holds_none_ms,
                        legacy, ledger_v FROM ledger_rows",
                    [],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                        ))
                    },
                )
                .unwrap();
        assert_eq!(
            (duration, slot, ai, human, none, legacy, v),
            (
                100_000,
                70_000,
                60_000,
                30_000,
                30_000,
                0,
                i64::from(LEDGER_V)
            )
        );
        let mut later = row.clone();
        later.ledger_v = LEDGER_V + 1;
        assert!(queue.write_ledger_row(&later).unwrap());
        assert_eq!(rows(&queue), 1);
        assert_eq!(queue.ledger_rows(LedgerKind::Run).unwrap(), vec![later]);
        assert!(queue.ledger_rows(LedgerKind::Task).unwrap().is_empty());
    }

    /// A legacy row is frozen: a later write of the same run, of any
    /// version, leaves it as first written.
    #[test]
    fn a_legacy_row_is_not_written_over() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let events = vec![
            event(1, "run_claimed", json!({"dagq_version": "0.1"}), 0),
            event(2, "receipt_observed", json!({}), 60),
            event(3, "run_integrated", json!({}), 90),
        ];
        let legacy = ledger::run_row(RUN, &events, &[], 0, "2030-01-01T00:00:00.000Z");
        assert!(legacy.is_legacy());
        assert!(queue.write_ledger_row(&legacy).unwrap());
        let mut rebuilt = legacy.clone();
        rebuilt.legacy = None;
        rebuilt.ledger_v = LEDGER_V + 1;
        assert!(!queue.write_ledger_row(&rebuilt).unwrap());
        assert_eq!(rows(&queue), 1);
        let read = queue.ledger_row(LedgerKind::Run, RUN).unwrap().unwrap();
        assert_eq!(read, legacy);
        assert_eq!(read.legacy.unwrap()["work"], 60);
    }

    /// The session rows of [`crate::domain::ledger::testkit::sessions_across`].
    fn sessions() -> Vec<LedgerRow> {
        let (log, _) = crate::domain::ledger::testkit::sessions_across();
        let rows = ledger::session::session_rows(&log.events, "2030-01-01T00:00:00.000Z");
        assert!(rows.len() >= 3);
        rows
    }

    /// The session rows and the mark are written in one transaction, each
    /// over the row it read: they read back as written, and the mark is in
    /// no reading of the rows by their kind (nor a window of every kind);
    /// a row another writer wrote since its read writes none of them, the
    /// rows before it in the batch rolled back, and the mark stays as it
    /// was.
    #[test]
    fn the_session_rows_and_the_mark_are_written_all_or_none() {
        use crate::domain::ledger::session::written::SessionsWritten;
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        assert_eq!(queue.sessions_written().unwrap(), None);
        let rows = sessions();
        let batch: Vec<(LedgerRow, Option<LedgerRow>)> =
            rows.iter().map(|row| (row.clone(), None)).collect();
        let mark = SessionsWritten::at(EventId::new(7));
        assert!(queue.write_session_rows_over(&batch, Some(&mark)).unwrap());
        assert_eq!(queue.sessions_written().unwrap(), Some(mark));
        let mut sorted = rows.clone();
        sorted.sort_by(|a, b| a.key.cmp(&b.key));
        assert_eq!(queue.ledger_rows(LedgerKind::Session).unwrap(), sorted);
        assert_eq!(
            queue.ledger_heads(LedgerKind::Session).unwrap().len(),
            rows.len()
        );
        for kind in LedgerKind::ALL {
            assert!(
                queue
                    .ledger_rows(kind)
                    .unwrap()
                    .iter()
                    .all(|row| row.key != ledger::session::written::KEY),
                "{kind:?}"
            );
        }
        let window = queue
            .ledger_window(&ledger::window::WindowQuery::default())
            .unwrap();
        assert!(!format!("{window:?}").contains(ledger::session::written::KIND));
        assert_eq!(rows_of_kind(&queue, ledger::session::written::KIND), 1);

        // Over a stale read of the last: none written, the mark as it was.
        let mut again: Vec<(LedgerRow, Option<LedgerRow>)> = rows
            .iter()
            .map(|row| {
                let mut later = row.clone();
                later.built_at = "2031-01-01T00:00:00.000Z".into();
                (later, Some(row.clone()))
            })
            .collect();
        let stale = again.len() - 1;
        again[stale].1 = None;
        let later = SessionsWritten::at(EventId::new(9));
        assert!(!queue.write_session_rows_over(&again, Some(&later)).unwrap());
        assert_eq!(queue.ledger_rows(LedgerKind::Session).unwrap(), sorted);
        assert_eq!(queue.sessions_written().unwrap(), Some(mark));
        // Over the rows read: all written, the mark moved; none moves it.
        again[stale].1 = Some(rows[stale].clone());
        assert!(queue.write_session_rows_over(&again, Some(&later)).unwrap());
        assert_eq!(queue.sessions_written().unwrap(), Some(later));
        assert!(
            queue
                .ledger_rows(LedgerKind::Session)
                .unwrap()
                .iter()
                .all(|row| row.built_at.starts_with("2031"))
        );
        assert!(queue.write_session_rows_over(&[], None).unwrap());
        assert_eq!(queue.sessions_written().unwrap(), Some(later));
    }

    /// How many rows of `kind` the table holds, whatever reads them.
    fn rows_of_kind(queue: &SqliteQueue, kind: &str) -> i64 {
        queue
            .conn
            .query_row(
                "SELECT count(*) FROM ledger_rows WHERE kind=?1",
                [kind],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// A write of the session rows that fails part way (here as the mark
    /// is written, after every row) writes no row and no mark: the
    /// transaction rolls back the rows written before the failure.
    #[test]
    fn a_write_of_the_session_rows_that_fails_part_way_writes_no_row_and_no_mark() {
        use crate::domain::ledger::session::written::SessionsWritten;
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        queue
            .conn
            .execute_batch(
                "CREATE TEMP TRIGGER part_way BEFORE INSERT ON ledger_rows
                   WHEN NEW.kind = 'sessions_written'
                 BEGIN SELECT RAISE(ABORT, 'part way'); END;",
            )
            .unwrap();
        let batch: Vec<(LedgerRow, Option<LedgerRow>)> =
            sessions().into_iter().map(|row| (row, None)).collect();
        let mark = SessionsWritten::at(EventId::new(7));
        let error = queue
            .write_session_rows_over(&batch, Some(&mark))
            .unwrap_err();
        assert!(format!("{error:#}").contains("part way"), "{error:#}");
        assert_eq!(rows_of_kind(&queue, "session"), 0);
        assert_eq!(queue.sessions_written().unwrap(), None);
        queue.conn.execute_batch("DROP TRIGGER part_way").unwrap();
        assert!(queue.write_session_rows_over(&batch, Some(&mark)).unwrap());
        assert_eq!(
            rows_of_kind(&queue, "session"),
            i64::try_from(batch.len()).unwrap()
        );
        assert_eq!(queue.sessions_written().unwrap(), Some(mark));
    }
}
