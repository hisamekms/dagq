//! The ledger's rows in queue.db (`ledger_rows`, [`LedgerStore`]).
use crate::infrastructure::sql_exec::TransactionEntries;

use super::*;
use crate::application::LedgerStore;
use crate::domain::ledger::session::written::omitted::{self, Coverage, Omitted, Point};
use crate::domain::ledger::session::written::{self, Marks, RowsWritten, SessionsWritten};
use crate::domain::ledger::window::{
    EARLIER_RUN_KINDS, EarlierRecords, RECORD_TIME_FORM, RUN_END_STATUSES, RowsFrom,
};
use crate::domain::ledger::{
    InputsThrough, LedgerHead, LedgerKind, LedgerRow, Measure, Subject, Totals, node,
};
use crate::domain::pre_claim::rows::Since;
use crate::domain::run_phase::{Blocker, Holds};
use crate::domain::stats::executions::FirstExecution;
use crate::domain::stats::landing_utilization;
use crate::infrastructure::sql_dialect::{JsonType, SqlDialect};
use crate::infrastructure::sql_exec::{
    OptionalRow, SqlRead, SqlResult, SqlRow, SqlValue, SqlWrite, ToSqlValue, sql_params,
};
use crate::infrastructure::store_rows::event_row;

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
/// The rows of a JSON list of `{"kind", "key"}`, in the order of the
/// list: the list is the outer loop (`CROSS JOIN` keeps it so in SQLite)
/// and each of its items finds its row by the primary key; an item of no
/// row gives none.
fn rows_of_sql(dialect: SqlDialect) -> String {
    format!(
        "SELECT r.* FROM {items} CROSS JOIN ledger_rows r
          WHERE r.kind = {kind} AND r.key = {key}
          ORDER BY {index}",
        items = dialect.json_elements_numbered("?1", "j"),
        kind = dialect.json_field_spaced("j.value", "$.kind", JsonType::Text),
        key = dialect.json_field_spaced("j.value", "$.key", JsonType::Text),
        index = dialect.json_element_index("j"),
    )
}

fn held_row(conn: &impl SqlRead, kind: LedgerKind, key: &str) -> Result<Option<LedgerRow>> {
    SqlRead::query_row(
        conn,
        "SELECT * FROM ledger_rows WHERE kind=?1 AND key=?2",
        sql_params![kind.as_str(), key],
        ledger_row,
    )
    .optional()?
    .transpose()
}

/// Write `mark` on `conn` as the one row of its kind
/// ([`written::KIND`]): its position as the `events` of its
/// `inputs_through`, its version as its `ledger_v`, nothing else. No
/// reader of the rows of a [`LedgerKind`] reads it.
fn write_mark(conn: &impl SqlWrite, mark: &SessionsWritten) -> Result<()> {
    let through = text(&InputsThrough {
        events: mark.through,
        supervisor: None,
        life_ended: Vec::new(),
        node_samples: None,
        steps: None,
        resources: None,
        inside_resources: None,
    })?;
    SqlWrite::execute(
        conn,
        "INSERT INTO ledger_rows(kind, key, ledger_v, built_at, inputs_through)
         VALUES (?1, ?2, ?3, '', ?4)
         ON CONFLICT(kind, key) DO UPDATE SET
           ledger_v=excluded.ledger_v, inputs_through=excluded.inputs_through",
        sql_params![written::KIND, written::KEY, mark.ledger_v, through],
    )?;
    Ok(())
}

/// Write `mark` on `conn` as the one row of its kind
/// ([`written::ROWS_KIND`]): its positions as the `events`,
/// `node_samples`, `steps`, `resources` and `inside_resources` of its
/// `inputs_through` with its read time and restart scheduling metadata ([`MarkThrough`]),
/// its version as its `ledger_v`, nothing else. No reader of the rows of a [`LedgerKind`]
/// reads it.
fn write_rows_mark(conn: &impl SqlWrite, mark: &RowsWritten) -> Result<()> {
    let through = text(&MarkThrough {
        positions: InputsThrough {
            events: mark.through.events,
            supervisor: None,
            life_ended: Vec::new(),
            node_samples: Some(mark.through.samples),
            steps: Some(mark.through.steps),
            resources: Some(mark.through.resources),
            inside_resources: Some(mark.through.inside_resources),
        },
        core: mark.core,
        queue: mark.queue,
        read_ms: mark.read_ms,
        take_offset: mark.take_offset,
        time_pending: mark.time_pending,
    })?;
    SqlWrite::execute(
        conn,
        "INSERT INTO ledger_rows(kind, key, ledger_v, built_at, inputs_through)
         VALUES (?1, ?2, ?3, '', ?4)
         ON CONFLICT(kind, key) DO UPDATE SET
           ledger_v=excluded.ledger_v, inputs_through=excluded.inputs_through",
        sql_params![written::ROWS_KIND, written::KEY, mark.ledger_v, through],
    )?;
    Ok(())
}

/// The coverage of the omitted queue events `conn` holds
/// ([`omitted::KIND`]'s row, the coverage the text of its `detail`), if any;
/// one whose text does not read is [`Coverage::unreadable`], so that a
/// builder of this version stops rather than fails or writes over it.
fn held_coverage(conn: &impl SqlRead) -> Result<Option<Coverage>> {
    let detail = SqlRead::query_row(
        conn,
        "SELECT detail FROM ledger_rows WHERE kind=?1 AND key=?2",
        sql_params![omitted::KIND, written::KEY],
        |r| r.get::<_, String>(0),
    )
    .optional()?;
    Ok(detail
        .map(|detail| serde_json::from_str(&detail).unwrap_or_else(|_| Coverage::unreadable())))
}

/// Write `omitted` on `conn` when its coverage supersedes the one held
/// ([`Coverage::supersedes`]): the coverage as the one row of its kind, S
/// as the `events` of its `inputs_through`, and each point not held yet in
/// `ledger_omitted_points` (a point held is the same one). The same
/// coverage again writes nothing. False, nothing written, when it
/// conflicts with the one held ([`Coverage::conflicts`]): the caller rolls
/// back the whole transaction, so that no W or R moves past a S it holds.
fn write_omitted(conn: &impl SqlWrite, omitted: &Omitted) -> Result<bool> {
    let coverage = &omitted.coverage;
    let held = held_coverage(conn)?;
    if coverage.conflicts(held.as_ref()) {
        return Ok(false);
    }
    if !coverage.supersedes(held.as_ref()) {
        return Ok(true);
    }
    let through = text(&InputsThrough {
        events: coverage.through,
        supervisor: None,
        life_ended: Vec::new(),
        node_samples: None,
        steps: None,
        resources: None,
        inside_resources: None,
    })?;
    SqlWrite::execute(
        conn,
        "INSERT INTO ledger_rows(kind, key, ledger_v, built_at, inputs_through, detail)
         VALUES (?1, ?2, ?3, '', ?4, ?5)
         ON CONFLICT(kind, key) DO UPDATE SET
           ledger_v=excluded.ledger_v, inputs_through=excluded.inputs_through,
           detail=excluded.detail",
        sql_params![
            omitted::KIND,
            written::KEY,
            crate::domain::ledger::LEDGER_V,
            through,
            text(coverage)?
        ],
    )?;
    for point in &omitted.points {
        SqlWrite::execute(
            conn,
            "INSERT INTO ledger_omitted_points(kind, at, max_ms, last, timed)
             VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(kind, at) DO NOTHING",
            sql_params![
                point.kind.as_str(),
                point.at.as_i64(),
                point.max_ms,
                point.last.map(EventId::as_i64),
                point.timed.map(EventId::as_i64)
            ],
        )?;
    }
    Ok(true)
}

/// A kind's points past a position, by position: a seek of the key.
const POINTS_AFTER_SQL: &str = "SELECT kind, at, max_ms, last, timed FROM ledger_omitted_points
  WHERE kind=?1 AND at > ?2 ORDER BY at LIMIT ?3";

/// A kind's last point whose M is before a time: a seek of the index of M,
/// the latest M then the latest position (M only rises with it).
const POINT_BEFORE_MS_SQL: &str = "SELECT kind, at, max_ms, last, timed FROM ledger_omitted_points
  WHERE kind=?1 AND max_ms < ?2 ORDER BY max_ms DESC, at DESC LIMIT 1";

/// A point as [`POINTS_AFTER_SQL`] and [`POINT_BEFORE_MS_SQL`] read it.
fn point_row(r: &SqlRow<'_>) -> SqlResult<Point> {
    Ok(Point {
        kind: r.get(0)?,
        at: EventId::new(r.get(1)?),
        max_ms: r.get(2)?,
        last: r.get::<_, Option<i64>>(3)?.map(EventId::new),
        timed: r.get::<_, Option<i64>>(4)?.map(EventId::new),
    })
}

/// The text of the `inputs_through` of the mark R's row: the positions of
/// [`InputsThrough`], and beside them the latest time read up to R's events
/// ([`RowsWritten::read_ms`]), this row's alone: a row of another kind has
/// no such field, and a reader of [`InputsThrough`] alone skips it. The
/// restart scheduling metadata is persisted here too, without changing input positions.
#[derive(serde::Serialize, serde::Deserialize)]
struct MarkThrough {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    core: Option<written::RowPosition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    queue: Option<written::RowPosition>,
    #[serde(flatten)]
    positions: InputsThrough,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    read_ms: Option<i64>,
    #[serde(default)]
    take_offset: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    time_pending: Option<i64>,
}

/// The marks of each of `kinds` (key [`written::KEY`]) `conn` holds, read
/// in one statement: each kind with its version and what its
/// `inputs_through` holds.
fn read_marks(conn: &impl SqlRead, kinds: [&str; 2]) -> Result<Vec<(String, u32, MarkThrough)>> {
    let statement = "SELECT kind, ledger_v, inputs_through FROM ledger_rows
         WHERE kind IN (?1, ?2) AND key=?3";
    let rows = SqlRead::query_map(
        conn,
        statement,
        sql_params![kinds[0], kinds[1], written::KEY],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        },
    )?;
    rows.into_iter()
        .map(|(kind, ledger_v, through)| {
            Ok((
                kind,
                u32::try_from(ledger_v)?,
                serde_json::from_str::<MarkThrough>(&through)?,
            ))
        })
        .collect()
}

/// Write `row` on `conn` as [`LedgerStore::write_ledger_row`] says.
fn upsert(conn: &impl SqlWrite, row: &LedgerRow) -> Result<bool> {
    let totals = &row.totals;
    let mut columns: Vec<(&str, SqlValue)> = vec![
        ("kind", row.kind.as_str().to_sql_value()),
        ("key", row.key.to_sql_value()),
        (
            "task_id",
            row.task_id.map(|task| task.as_i64()).to_sql_value(),
        ),
        ("ledger_v", i64::from(row.ledger_v).to_sql_value()),
        ("built_at", row.built_at.to_sql_value()),
        ("inputs_through", text(&row.inputs_through)?.to_sql_value()),
        ("final", row.is_final.to_sql_value()),
        ("legacy", row.is_legacy().to_sql_value()),
        (
            "legacy_values",
            row.legacy.as_ref().map(text).transpose()?.to_sql_value(),
        ),
        ("started_at", row.started_at.to_sql_value()),
        ("ended_at", row.ended_at.to_sql_value()),
        ("outcome", row.outcome.to_sql_value()),
        ("duration_ms", totals.all.duration_ms.to_sql_value()),
        ("slot_ms", totals.all.slot_ms.to_sql_value()),
        ("unattributed_ms", totals.unattributed_ms.to_sql_value()),
        ("attempts", totals.attempts.to_sql_value()),
        ("totals", text(totals)?.to_sql_value()),
        ("intervals", text(&row.intervals)?.to_sql_value()),
        ("attributes", text(&row.attributes)?.to_sql_value()),
        (
            "after_claim",
            row.after_claim
                .as_ref()
                .map(text)
                .transpose()?
                .to_sql_value(),
        ),
        ("detail", text(&row.detail)?.to_sql_value()),
        ("inputs_from", row.inputs_from.as_i64().to_sql_value()),
        ("records", text(&row.records)?.to_sql_value()),
    ];
    let subject = row.subject.as_ref();
    columns.extend([
        (
            "subject_kind",
            subject.map(|s| s.kind.clone()).to_sql_value(),
        ),
        ("subject_id", subject.map(|s| s.id.clone()).to_sql_value()),
        ("round", subject.and_then(|s| s.round).to_sql_value()),
        (
            "route",
            subject.and_then(|s| s.route.clone()).to_sql_value(),
        ),
        (
            "provider",
            subject.and_then(|s| s.provider.clone()).to_sql_value(),
        ),
        (
            "model",
            subject.and_then(|s| s.model.clone()).to_sql_value(),
        ),
    ]);
    for (blocker, column) in BLOCKERS {
        columns.push((
            column,
            millis(&totals.by_blocker, tag(blocker)).to_sql_value(),
        ));
    }
    for (holds, column) in HOLDS {
        columns.push((column, millis(&totals.by_holds, tag(holds)).to_sql_value()));
    }
    let names: Vec<&str> = columns.iter().map(|(name, _)| *name).collect();
    let marks: Vec<String> = (1..=names.len()).map(|n| format!("?{n}")).collect();
    // A legacy row's values (its legacy values, times, sums and
    // attributes) are frozen; what it owns, how far it reaches and what
    // its detail folds of its records (its sessions, jobs and
    // Executions, its use of providers and its failure cause; a legacy
    // row keeps nothing else there) are not, so
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
                    "{name}=CASE WHEN ledger_rows.legacy = TRUE THEN ledger_rows.{name} ELSE excluded.{name} END"
                ),
            })
            .collect();
    let frozen: bool = SqlRead::query_row(
        conn,
        "SELECT legacy FROM ledger_rows WHERE kind=?1 AND key=?2",
        sql_params![row.kind.as_str(), row.key],
        |r| r.get(0),
    )
    .optional()?
    .unwrap_or(false);
    SqlWrite::execute(
        conn,
        &format!(
            "INSERT INTO ledger_rows({}) VALUES ({})
                 ON CONFLICT(kind, key) DO UPDATE SET {}",
            names.join(","),
            marks.join(","),
            updates.join(","),
        ),
        &columns
            .into_iter()
            .map(|(_, value)| value)
            .collect::<Vec<_>>(),
    )?;
    Ok(!frozen)
}

/// The read of the rows [`crate::domain::pre_claim::rows::holds`] keeps
/// (`?2` the start's ms, `?3` its first event, `?4` the end of a life),
/// with no records, at most `?6`: with `after` (`?5`) the key's range after
/// it on the primary key, so a page reads no row before it; without, `?5`
/// is NULL.
fn wait_rows_sql(dialect: SqlDialect, after: bool) -> String {
    let from = if after {
        "key > ?5"
    } else {
        "CAST(?5 AS TEXT) IS NULL"
    };
    format!(
        "SELECT kind, key, task_id, ledger_v, built_at, inputs_through, final,
                legacy_values, started_at, ended_at, outcome, totals, intervals,
                attributes, after_claim, detail, inputs_from, '[]' AS records,
                subject_kind, subject_id, round, route, provider, model
           FROM ledger_rows
          WHERE kind=?1 AND {from}
            AND ((subject_kind IN ('slots_full', 'claim_hold', 'claim_deferral', 'landing_hold')
                  AND (final=FALSE OR ended_at IS NULL OR ended_at >= ?2 OR outcome=?4
                       OR {events} >= ?3))
                 OR (subject_kind='live_alert_stream'
                     AND (outcome IS NULL OR outcome=?4)))
          ORDER BY key LIMIT ?6",
        events = dialect.json_field_spaced("inputs_through", "$.events", JsonType::Integer),
    )
}

/// The read of a page of the rows of the kind `?1`, by key: with `after` the
/// key's range past `?2` on the primary key, at most `?3`, so a page walks no
/// row before it; without, from the kind's first, at most `?2`. Two statements
/// because an `OR` over the bound key would leave the index only `kind=?`.
fn rows_after_sql(after: bool) -> &'static str {
    if after {
        "SELECT * FROM ledger_rows WHERE kind=?1 AND key > ?2 ORDER BY key LIMIT ?3"
    } else {
        "SELECT * FROM ledger_rows WHERE kind=?1 ORDER BY key LIMIT ?2"
    }
}

/// The condition on `ledger_rows r` that keeps what
/// [`RowsFrom::leaves_out`] leaves out, with the time and the event as the
/// parameters `?{time}` and `?{event}` and the time's text ([`RowsFrom::time_text`])
/// as the one after them.
fn rows_kept_sql(dialect: SqlDialect, time: usize, event: usize) -> String {
    let through = dialect.json_field_spaced("r.inputs_through", "$.events", JsonType::Integer);
    let record = record_not_before_sql(dialect, time + 2);
    format!(
        "(r.legacy = TRUE OR r.started_at IS NULL OR r.ended_at IS NULL
          OR r.ended_at >= ?{time} OR {through} > ?{event} OR {record})"
    )
}

/// The same condition's opposite, for the rows left out.
fn rows_left_out_sql(dialect: SqlDialect, time: usize, event: usize) -> String {
    let through = dialect.json_field_spaced("r.inputs_through", "$.events", JsonType::Integer);
    let record = record_not_before_sql(dialect, time + 2);
    format!(
        "r.legacy = FALSE AND r.started_at IS NOT NULL AND r.ended_at < ?{time}
         AND {through} <= ?{event} AND NOT {record}"
    )
}

/// Whether a record of the row does not stand before the time, the text
/// `?{text}` ([`RowsFrom::leaves_out`]), last in the conditions above: only
/// the rows the columns would leave out have their records walked.
fn record_not_before_sql(dialect: SqlDialect, text: usize) -> String {
    let before = |time: &str| format!("({time} LIKE '{RECORD_TIME_FORM}' AND {time} < ?{text})");
    let recorded = dialect.json_field_spaced("t.value", "$.created_at", JsonType::Text);
    let at = dialect.json_field_spaced("t.value", "$.payload.at", JsonType::Text);
    format!(
        "EXISTS (SELECT 1 FROM {records} t
                  WHERE NOT {recorded_before} OR ({at} IS NOT NULL AND NOT {at_before}))",
        records = dialect.json_elements("r.records"),
        recorded_before = before(&recorded),
        at_before = before(&at),
    )
}

/// The read of the queue rows' heads (the kind `?1`), their columns and the
/// ids of their records, by key, with `filter` on the rows and `limit`.
fn queue_heads_sql(dialect: SqlDialect, filter: &str, limit: &str) -> String {
    format!(
        "SELECT r.key, r.inputs_from, r.inputs_through, r.final, r.ledger_v,
                (SELECT COALESCE({ids}, '[]')
                   FROM {records} j) AS ids,
                r.task_id, r.subject_kind, r.subject_id, r.started_at, r.ended_at,
                r.outcome, r.detail
           FROM ledger_rows r WHERE r.kind=?1 {filter} ORDER BY r.key {limit}",
        ids = dialect.json_array_agg(&dialect.json_field_spaced(
            "j.value",
            "$.id",
            JsonType::Integer
        )),
        records = dialect.json_elements("r.records"),
    )
}

/// The read of the stops (`?2`) of the queue rows, each with its row's key
/// and its index among the row's records, at most `?5`: with `after` (the
/// key `?3` and the index `?4`) the key's range from it on the primary key,
/// so a page reads no row before it; without, `?3` and `?4` are NULL.
fn stops_after_sql(dialect: SqlDialect, after: bool) -> String {
    let index = dialect.json_element_index("j");
    let from = if after {
        format!("r.key >= ?3 AND (r.key > ?3 OR {index} > ?4)")
    } else {
        "CAST(?3 AS TEXT) IS NULL AND CAST(?4 AS BIGINT) IS NULL".to_owned()
    };
    format!(
        "SELECT r.key, {index}, {record} FROM ledger_rows r, {records}
          WHERE r.kind=?1 AND {from}
            AND {kind}=?2
          ORDER BY r.key, {index} LIMIT ?5",
        record = dialect.json_element("j.value", JsonType::Text),
        records = dialect.json_elements_numbered("r.records", "j"),
        kind = dialect.json_field_spaced("j.value", "$.kind", JsonType::Text),
    )
}

impl SqliteQueue {
    /// The queue heads a read of [`queue_heads_sql`] gives.
    fn queue_heads(
        &self,
        sql: &str,
        params: &[SqlValue],
    ) -> Result<Vec<crate::domain::ledger::queue::QueueHead>> {
        let tx = self.read_entry("queue_heads")?;
        let statement = sql;
        let kind = LedgerKind::Queue;
        let rows = SqlRead::query_map(&tx, statement, params, |r| {
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
        rows.into_iter().collect()
    }

    /// The rows [`crate::domain::pre_claim::rows::holds`] keeps for `since`,
    /// picked by their columns, at most `limit` after the key `after`
    /// (`None`: from the first), by key, each with no records.
    fn wait_rows_after(
        &self,
        since: Since,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LedgerRow>> {
        let tx = self.read_entry("wait_rows_after")?;
        let statement = wait_rows_sql(SqlRead::dialect(&tx), after.is_some());
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = SqlRead::query_map(
            &tx,
            &statement,
            sql_params![
                LedgerKind::Queue.as_str(),
                since.ms,
                since.id.as_i64(),
                crate::domain::claim_hold::ENDED_SUPERVISOR_GONE,
                after,
                limit,
            ],
            ledger_row,
        )?;
        rows.into_iter().collect()
    }
}

/// The last event naming a supervisor up to an id, by the index
/// `events_by_supervisor` (its expression the one the index is on).
fn last_event_naming_supervisor_sql(dialect: SqlDialect) -> String {
    format!(
        "SELECT * FROM run_events
  WHERE {} = ?1 AND id <= ?2
    AND {} = 'text'
  ORDER BY id DESC LIMIT 1",
        dialect.json_field_spaced("payload", "$.supervisor", JsonType::Text),
        dialect.json_type_of("payload", "$.supervisor"),
    )
}

/// The push records of the run rows (`?1`), by their ids: every
/// `push_finished` (`?2`), and the `push_failed` (`?3`) and
/// `run_phase_changed` (`?4`) records of the rows holding a `push_failed`.
///
/// Whether a row holds a `push_failed` is decided once for the row, not
/// once for each of its records: an uncorrelated subquery picks the keys
/// of those rows a single time for the statement (SQLite's `LIST
/// SUBQUERY`), and a record of the two kinds is kept when its row's key is
/// among them. An `EXISTS` over the row's records evaluated for each
/// record would walk the row again for each one, so a row's time would
/// grow with the square of its records. A derived table marking each row
/// is the other way to decide it once, but SQLite flattens it into the
/// outer statement and evaluates the mark again for each record, so the
/// subquery of the keys is the one kept.
fn push_records_sql(dialect: SqlDialect) -> String {
    push_records_of_sql(dialect, "", "", "")
}

/// The push records of [`push_records_sql`] of the run rows `rows` keeps
/// alone, by their ids: `rows` is a condition on the row's alias with
/// `{t}` in place of it, put on the rows read (`r`) and on those picked for
/// holding a `push_failed` (`g`) alike, so neither reads a row outside it;
/// `kept` is a condition on the records kept (`j`, its id `{id}`) and
/// `tail` follows the order (a `LIMIT`).
fn push_records_of_sql(dialect: SqlDialect, rows: &str, kept: &str, tail: &str) -> String {
    let id = dialect.json_field_spaced("j.value", "$.id", JsonType::Integer);
    format!(
        "SELECT {record} FROM ledger_rows r, {records} j
              WHERE r.kind=?1{r_rows}
                AND ({kind}=?2
                     OR ({kind} IN (?3, ?4)
                         AND r.key IN (SELECT g.key FROM ledger_rows g, {failed_records} f
                                        WHERE g.kind=?1{g_rows} AND {failed}=?3))){kept}
              ORDER BY {id}{tail}",
        record = dialect.json_element("j.value", JsonType::Text),
        records = dialect.json_elements("r.records"),
        kind = dialect.json_field_spaced("j.value", "$.kind", JsonType::Text),
        failed_records = dialect.json_elements("g.records"),
        failed = dialect.json_field_spaced("f.value", "$.kind", JsonType::Text),
        r_rows = rows.replace("{t}", "r"),
        g_rows = rows.replace("{t}", "g"),
        kept = kept.replace("{id}", &id),
    )
}

/// The push records of the run rows of keys after `?5` up to `?6` (`after`
/// false: up to `?5`, from the first), by id: a page of whole rows, both
/// ends of the range on the primary key so that no other row is read.
fn push_records_range_sql(dialect: SqlDialect, after: bool) -> String {
    let rows = if after {
        " AND {t}.key > ?5 AND {t}.key <= ?6"
    } else {
        " AND {t}.key <= ?5"
    };
    push_records_of_sql(dialect, rows, "", "")
}

/// The push records of the run row of key `?5` alone, after the record
/// `?6` when `after` (from its first otherwise), at most `?7` (`?6` without
/// `after`) of them, by id: a part of one row.
fn push_records_in_row_sql(dialect: SqlDialect, after: bool) -> String {
    if after {
        push_records_of_sql(dialect, " AND {t}.key = ?5", " AND {id} > ?6", " LIMIT ?7")
    } else {
        push_records_of_sql(dialect, " AND {t}.key = ?5", "", " LIMIT ?6")
    }
}

/// The keys of at most `?3` (`?2` without `after`) run rows (`?1`) after
/// the key `?2` when `after`, by key: no column but the key read.
fn row_keys_after_sql(after: bool) -> &'static str {
    if after {
        "SELECT key FROM ledger_rows WHERE kind=?1 AND key > ?2 ORDER BY key LIMIT ?3"
    } else {
        "SELECT key FROM ledger_rows WHERE kind=?1 ORDER BY key LIMIT ?2"
    }
}

impl SqliteQueue {
    /// The heads of the rows of `kind` (`?1` of `params`) that `filter`
    /// (SQL over the row `r`) keeps, by key, each with whether its row is
    /// legacy: the ids of their records come from SQLite's JSON functions
    /// over the stored array, and no record is parsed.
    fn heads_where(
        &self,
        kind: LedgerKind,
        filter: &str,
        params: &[SqlValue],
    ) -> Result<Vec<(LedgerHead, bool)>> {
        let tx = self.read_entry("heads_where")?;
        let dialect = SqlRead::dialect(&tx);
        let statement = format!(
            "SELECT r.key, r.inputs_from, r.inputs_through, r.final, r.ledger_v, r.legacy,
                    (SELECT COALESCE({ids}, '[]')
                       FROM {records} j) AS ids
               FROM ledger_rows r WHERE r.kind=?1 AND {filter} ORDER BY r.key",
            ids = dialect.json_array_agg(&dialect.json_field_spaced(
                "j.value",
                "$.id",
                JsonType::Integer
            )),
            records = dialect.json_elements("r.records"),
        );
        let rows = SqlRead::query_map(&tx, &statement, params, |r| {
            let key: String = r.get("key")?;
            let inputs_from: i64 = r.get("inputs_from")?;
            let inputs_through: String = r.get("inputs_through")?;
            let is_final: bool = r.get("final")?;
            let ledger_v: i64 = r.get("ledger_v")?;
            let legacy: bool = r.get("legacy")?;
            let ids: String = r.get("ids")?;
            Ok((|| {
                let mut records: Vec<i64> = serde_json::from_str(&ids)?;
                records.sort_unstable();
                let head = LedgerHead {
                    kind,
                    key,
                    inputs_from: EventId::new(inputs_from),
                    inputs_through: serde_json::from_str(&inputs_through)?,
                    is_final,
                    ledger_v: u32::try_from(ledger_v)?,
                    records: records.into_iter().map(EventId::new).collect(),
                };
                Ok((head, legacy))
            })())
        })?;
        rows.into_iter().collect()
    }
}

/// These reads take the caller's snapshot, including all pages and the
/// earlier records, so no nested transaction entry is opened.
fn rows_after_in(
    tx: &impl SqlRead,
    kind: LedgerKind,
    after: Option<&str>,
    limit: usize,
) -> Result<Vec<LedgerRow>> {
    let statement = rows_after_sql(after.is_some());
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows = match after {
        Some(after) => SqlRead::query_map(
            tx,
            statement,
            sql_params![kind.as_str(), after, limit],
            ledger_row,
        )?,
        None => SqlRead::query_map(tx, statement, sql_params![kind.as_str(), limit], ledger_row)?,
    };
    rows.into_iter().collect()
}

fn rows_from_in(
    tx: &impl SqlRead,
    kind: LedgerKind,
    after: Option<&str>,
    limit: usize,
    from: &RowsFrom,
) -> Result<Vec<LedgerRow>> {
    if kind == LedgerKind::Queue {
        return rows_after_in(tx, kind, after, limit);
    }
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    // By the primary key's range from `after`, as [`rows_after_sql`]:
    // a page walks only the rows it goes over, each once in the read.
    let mut values = vec![kind.as_str().to_sql_value()];
    let mut key = "";
    if let Some(after) = after {
        values.push(after.to_sql_value());
        key = "AND r.key > ?2";
    }
    let next = values.len() + 1;
    let kept = if kind == LedgerKind::Node {
        values.push(from.node_ended_by_ms().to_sql_value());
        format!("(r.ended_at IS NULL OR r.ended_at > ?{next})")
    } else {
        values.push(from.ended_before_ms.to_sql_value());
        values.push(from.inputs_through.as_i64().to_sql_value());
        values.push(from.time_text().to_sql_value());
        rows_kept_sql(SqlRead::dialect(tx), next, next + 1)
    };
    values.push(limit.to_sql_value());
    let rows = SqlRead::query_map(
        tx,
        &format!(
            "SELECT r.* FROM ledger_rows r WHERE r.kind=?1 {key} AND {kept}
                 ORDER BY r.key LIMIT ?{}",
            values.len()
        ),
        &values,
        ledger_row,
    )?;
    rows.into_iter().collect()
}

fn earlier_in(tx: &impl SqlRead, from: &RowsFrom) -> Result<EarlierRecords> {
    let dialect = SqlRead::dialect(tx);
    let left_out = rows_left_out_sql(dialect, 2, 3);
    let bound = (
        from.ended_before_ms,
        from.inputs_through.as_i64(),
        from.time_text(),
    );
    let mut records: Vec<RunEvent> = Vec::new();
    for kind in [LedgerKind::Task, LedgerKind::Session] {
        let read = SqlRead::query_map(
            tx,
            &format!("SELECT r.records FROM ledger_rows r WHERE r.kind=?1 AND {left_out}"),
            sql_params![kind.as_str(), bound.0, bound.1, bound.2.as_str()],
            |r| {
                Ok(serde_json::from_str::<Vec<RunEvent>>(
                    &r.get::<_, String>(0)?,
                ))
            },
        )?;
        for row in read {
            records.extend(row?);
        }
    }
    let quoted = |values: &mut dyn Iterator<Item = &str>| {
        values
            .map(|value| format!("'{value}'"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let picked = SqlRead::query_map(
        tx,
        &format!(
            "SELECT {record} FROM ledger_rows r, {records} j
                  WHERE r.kind=?1 AND {left_out}
                    AND ({kind} IN ({kinds}) OR {status} IN ({statuses}))",
            record = dialect.json_element("j.value", JsonType::Text),
            records = dialect.json_elements("r.records"),
            kind = dialect.json_field_spaced("j.value", "$.kind", JsonType::Text),
            status = dialect.json_field_spaced("j.value", "$.payload.status", JsonType::Text),
            kinds = quoted(
                &mut EARLIER_RUN_KINDS
                    .iter()
                    .chain(&landing_utilization::KINDS)
                    .map(|kind| kind.as_str())
            ),
            statuses = quoted(
                &mut RUN_END_STATUSES
                    .iter()
                    .chain(&landing_utilization::LEAVING_STATUSES)
                    .copied()
            ),
        ),
        sql_params![LedgerKind::Run.as_str(), bound.0, bound.1, bound.2.as_str()],
        |r| Ok(serde_json::from_str::<RunEvent>(&r.get::<_, String>(0)?)),
    )?;
    for event in picked {
        records.push(event?);
    }
    let executions = SqlRead::query_map(
        tx,
        &format!(
            "SELECT {actor}, {provider}, MIN({at})
                   FROM ledger_rows r, {executions} j
                  WHERE r.kind=?1 AND {left_out}
                  GROUP BY {actor}, {provider}",
            executions = dialect.json_elements_at_spaced("r.detail", "$.executions"),
            actor = dialect.json_field_spaced("j.value", "$.actor", JsonType::Text),
            provider = dialect.json_field_spaced("j.value", "$.provider", JsonType::Text),
            at = dialect.json_field_spaced("j.value", "$.at", JsonType::Integer),
        ),
        sql_params![LedgerKind::Run.as_str(), bound.0, bound.1, bound.2.as_str()],
        |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        },
    )?
    .into_iter()
    .filter_map(|(actor, provider, at)| FirstExecution::of(&actor?, provider?, at?))
    .collect();
    Ok(EarlierRecords {
        records,
        executions,
    })
}

struct LedgerSnapshot<'a, T> {
    tx: &'a T,
}

impl<T: SqlRead> crate::application::LedgerWindowRead for LedgerSnapshot<'_, T> {
    fn rows_after(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LedgerRow>> {
        rows_after_in(self.tx, kind, after, limit)
    }
    fn rows_from(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
        from: &RowsFrom,
    ) -> Result<Vec<LedgerRow>> {
        rows_from_in(self.tx, kind, after, limit, from)
    }
    fn earlier(&self, from: &RowsFrom) -> Result<EarlierRecords> {
        earlier_in(self.tx, from)
    }
    fn row(&self, kind: LedgerKind, key: &str) -> Result<Option<LedgerRow>> {
        held_row(self.tx, kind, key)
    }
    fn run_events(&self, run: &RunId) -> Result<Vec<RunEvent>> {
        run_events_of(self.tx, run)
    }
}

impl LedgerStore for SqliteQueue {
    fn ledger_run_events(
        &self,
        run: &crate::domain::RunId,
    ) -> Result<Vec<crate::domain::RunEvent>> {
        crate::application::EventStore::run_events(self, run)
    }

    /// One look up of the index `events_by_supervisor`: the query writes
    /// its expression as the index does and binds the name as text, and
    /// takes only a payload whose supervisor is a JSON string, so an event
    /// naming it by a number, an object or an array is not taken.
    fn last_event_naming_supervisor(
        &self,
        supervisor: &str,
        upto: EventId,
    ) -> Result<Option<crate::domain::RunEvent>> {
        let tx = self.read_entry("last_event_naming_supervisor")?;
        Ok(SqlRead::query_row(
            &tx,
            &last_event_naming_supervisor_sql(SqlRead::dialect(&tx)),
            sql_params![supervisor, upto.as_i64()],
            event_row,
        )
        .optional()?)
    }

    fn write_ledger_row(&self, row: &LedgerRow) -> Result<bool> {
        let tx = self.write_tx("write_ledger_row")?;
        let changed = upsert(&tx, row)?;
        tx.commit()?;
        Ok(changed)
    }

    /// Reads the row again and writes under one write lock, so no other
    /// writer comes between.
    fn write_ledger_row_over(
        &self,
        row: &LedgerRow,
        read: Option<&LedgerRow>,
    ) -> Result<Option<bool>> {
        let tx = self.write_tx("ledger_row_over")?;
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
        let marks = mark.map(|&sessions| Marks {
            sessions,
            rows: None,
            omitted: None,
        });
        self.write_session_rows_marked(rows, marks.as_ref())
    }

    /// As [`Self::write_session_rows_over`], R written in the same
    /// transaction after the session rows' mark.
    fn write_session_rows_marked(
        &self,
        rows: &[(LedgerRow, Option<LedgerRow>)],
        marks: Option<&Marks>,
    ) -> Result<bool> {
        let tx = self.write_tx("ledger_session_rows")?;
        for (row, read) in rows {
            if held_row(&tx, row.kind, &row.key)?.as_ref() != read.as_ref() {
                return Ok(false);
            }
            upsert(&tx, row)?;
        }
        if let Some(marks) = marks {
            write_mark(&tx, &marks.sessions)?;
            if let Some(rows) = &marks.rows {
                write_rows_mark(&tx, rows)?;
            }
            // A coverage refused rolls back the rows and marks with it.
            if let Some(omitted) = &marks.omitted
                && !write_omitted(&tx, omitted)?
            {
                return Ok(false);
            }
        }
        tx.commit()?;
        Ok(true)
    }

    fn sessions_written(&self) -> Result<Option<SessionsWritten>> {
        Ok(self.ledger_marks()?.map(|marks| marks.sessions))
    }

    /// Both marks read in one statement, so that R is the one written
    /// with the session rows' mark or before it.
    fn ledger_marks(&self) -> Result<Option<Marks>> {
        let tx = self.read_entry("ledger_marks")?;
        let (mut sessions, mut rows) = (None, None);
        for (kind, ledger_v, mark) in read_marks(&tx, [written::KIND, written::ROWS_KIND])? {
            let through = mark.positions;
            if kind == written::KIND {
                sessions = Some(SessionsWritten {
                    through: through.events,
                    ledger_v,
                });
            } else {
                rows = Some(RowsWritten {
                    through: written::Cursors {
                        events: through.events,
                        samples: through.node_samples.unwrap_or_default(),
                        steps: through.steps.unwrap_or_default(),
                        resources: through.resources.unwrap_or_default(),
                        inside_resources: through.inside_resources.unwrap_or_default(),
                    },
                    ledger_v,
                    core: mark.core,
                    queue: mark.queue,
                    read_ms: mark.read_ms,
                    take_offset: mark.take_offset,
                    time_pending: mark.time_pending,
                });
            }
        }
        let omitted = held_coverage(&tx)?.map(|coverage| Omitted {
            coverage,
            points: Vec::new(),
        });
        Ok(sessions.map(|sessions| Marks {
            sessions,
            rows,
            omitted,
        }))
    }

    fn ledger_omitted_points_after(
        &self,
        kind: &str,
        after: EventId,
        limit: usize,
    ) -> Result<Vec<Point>> {
        let tx = self.read_entry("ledger_omitted_points_after")?;
        Ok(SqlRead::query_map(
            &tx,
            POINTS_AFTER_SQL,
            sql_params![
                kind,
                after.as_i64(),
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            point_row,
        )?)
    }

    fn ledger_omitted_point_before_ms(&self, kind: &str, ms: i64) -> Result<Option<Point>> {
        let tx = self.read_entry("ledger_omitted_point_before_ms")?;
        Ok(
            SqlRead::query_row(&tx, POINT_BEFORE_MS_SQL, sql_params![kind, ms], point_row)
                .optional()?,
        )
    }

    fn ledger_row(&self, kind: LedgerKind, key: &str) -> Result<Option<LedgerRow>> {
        let tx = self.read_entry("ledger_row")?;
        held_row(&tx, kind, key)
    }

    /// One statement over the list sent as JSON ([`rows_of_sql`]): each
    /// row found by its primary key, none of the kind's keys walked.
    fn ledger_rows_of(&self, keys: &[(LedgerKind, String)]) -> Result<Vec<LedgerRow>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let list: Vec<Value> = keys
            .iter()
            .map(|(kind, key)| json!({"kind": kind.as_str(), "key": key}))
            .collect();
        let tx = self.read_entry("ledger_rows_of")?;
        let statement = rows_of_sql(SqlRead::dialect(&tx));
        let rows = SqlRead::query_map(
            &tx,
            &statement,
            sql_params![serde_json::to_string(&list)?],
            ledger_row,
        )?;
        rows.into_iter().collect()
    }

    fn ledger_rows(&self, kind: LedgerKind) -> Result<Vec<LedgerRow>> {
        let tx = self.read_entry("ledger_rows")?;
        let statement = "SELECT * FROM ledger_rows WHERE kind=?1 ORDER BY key";
        let rows = SqlRead::query_map(&tx, statement, sql_params![kind.as_str()], ledger_row)?;
        rows.into_iter().collect()
    }

    fn ledger_events_expected(&self) -> Result<usize> {
        Ok(usize::try_from(self.latest_event_id()?.as_i64()).unwrap_or(0))
    }

    fn ledger_rows_after(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LedgerRow>> {
        let tx = self.read_entry("ledger_rows_after")?;
        rows_after_in(&tx, kind, after, limit)
    }

    /// A read transaction on this connection: it takes no lock, and what the
    /// builder writes meanwhile is not seen until it ends.
    fn ledger_read_snapshot(
        &self,
        read: &mut dyn FnMut(&dyn crate::application::LedgerWindowRead) -> Result<()>,
    ) -> Result<()> {
        let tx = self.read_tx("ledger_read_snapshot")?;
        read(&LedgerSnapshot { tx: &tx })?;
        tx.commit()?;
        Ok(())
    }

    /// One statement's condition on the row, walking the kind's keys: of a
    /// row left out SQLite reads its columns and the times of its records,
    /// and nothing of it is parsed here.
    fn ledger_rows_from(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
        from: &RowsFrom,
    ) -> Result<Vec<LedgerRow>> {
        let tx = self.read_entry("ledger_rows_from")?;
        rows_from_in(&tx, kind, after, limit, from)
    }

    /// The records of the task and session rows are parsed whole; SQLite
    /// picks those of the run rows out of their stored arrays and sums the
    /// entries of their Executions: no other record is parsed.
    fn ledger_earlier(&self, from: &RowsFrom) -> Result<EarlierRecords> {
        let tx = self.read_entry("ledger_earlier")?;
        earlier_in(&tx, from)
    }

    /// The ids of the records come from SQLite's JSON functions over the
    /// stored array: no record is parsed here.
    fn ledger_heads(&self, kind: LedgerKind) -> Result<Vec<LedgerHead>> {
        Ok(self
            .heads_where(kind, "1", sql_params![kind.as_str()])?
            .into_iter()
            .map(|(head, _)| head)
            .collect())
    }

    /// SQLite picks the push records out of the stored arrays of the run
    /// rows ([`push_records_sql`]): no other record is parsed.
    fn ledger_push_records(&self) -> Result<Vec<crate::domain::RunEvent>> {
        let tx = self.read_entry("ledger_push_records")?;
        use crate::domain::EventKind;
        let statement = push_records_sql(SqlRead::dialect(&tx));
        let rows = SqlRead::query_map(
            &tx,
            &statement,
            sql_params![
                LedgerKind::Run.as_str(),
                EventKind::PushFinished.as_str(),
                EventKind::PushFailed.as_str(),
                EventKind::RunPhaseChanged.as_str(),
            ],
            |r| r.get::<_, String>(0),
        )?;
        rows.into_iter()
            .map(|row| {
                let event: crate::domain::RunEvent = serde_json::from_str(&row)?;
                Ok(crate::domain::ledger::builder::pushes::cut(&event))
            })
            .collect()
    }

    /// The keys of the page's rows are read first (the key alone, by the
    /// primary key), then the push records of those rows alone
    /// ([`push_records_range_sql`], [`push_records_in_row_sql`]), both in
    /// one read transaction: no row outside the page is read, and of a row
    /// read in parts only its records after the part before are sent.
    fn ledger_push_records_page(
        &self,
        read: &crate::domain::ledger::builder::pushes::page::PushRecordsRead,
    ) -> Result<crate::domain::ledger::builder::pushes::page::PushRecordsPage> {
        use crate::domain::ledger::builder::pushes::page::{
            PushRecordsAt, PushRecordsPage, PushRecordsRead,
        };
        let tx = self.read_entry("ledger_push_records_page")?;
        let dialect = SqlRead::dialect(&tx);
        let count = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
        let keys = |after: Option<&str>, rows: usize| -> Result<Vec<String>> {
            let statement = row_keys_after_sql(after.is_some());
            let read = match after {
                Some(after) => SqlRead::query_map(
                    &tx,
                    statement,
                    sql_params![LedgerKind::Run.as_str(), after, count(rows)],
                    |r| r.get::<_, String>(0),
                )?,
                None => SqlRead::query_map(
                    &tx,
                    statement,
                    sql_params![LedgerKind::Run.as_str(), count(rows)],
                    |r| r.get::<_, String>(0),
                )?,
            };
            Ok(read)
        };
        let records = |statement: &str, rest: Vec<SqlValue>| -> Result<Vec<RunEvent>> {
            use crate::domain::EventKind;
            let mut params = vec![
                LedgerKind::Run.as_str().to_sql_value(),
                EventKind::PushFinished.as_str().to_sql_value(),
                EventKind::PushFailed.as_str().to_sql_value(),
                EventKind::RunPhaseChanged.as_str().to_sql_value(),
            ];
            params.extend(rest);
            SqlRead::query_map(&tx, statement, &params, |r| r.get::<_, String>(0))?
                .into_iter()
                .map(|row| {
                    let event: RunEvent = serde_json::from_str(&row)?;
                    Ok(crate::domain::ledger::builder::pushes::cut(&event))
                })
                .collect()
        };
        let in_row = |key: String, after: Option<EventId>, limit: usize| -> Result<_> {
            // One more than the part asked for says whether the row goes on.
            let more = count(limit.max(1)).saturating_add(1);
            let part = match after {
                Some(after) => records(
                    &push_records_in_row_sql(dialect, true),
                    vec![
                        key.as_str().to_sql_value(),
                        after.as_i64().to_sql_value(),
                        more.to_sql_value(),
                    ],
                )?,
                None => records(
                    &push_records_in_row_sql(dialect, false),
                    vec![key.as_str().to_sql_value(), more.to_sql_value()],
                )?,
            };
            Ok(PushRecordsPage::in_row(key, 1, part, limit))
        };
        let page = match read {
            PushRecordsRead::Rows { after, rows } => {
                let keys = keys(after.as_deref(), *rows)?;
                match (keys.last(), after) {
                    (None, _) => PushRecordsPage::default(),
                    (Some(last), after) => {
                        let records = match after {
                            Some(after) => records(
                                &push_records_range_sql(dialect, true),
                                vec![after.as_str().to_sql_value(), last.as_str().to_sql_value()],
                            )?,
                            None => records(
                                &push_records_range_sql(dialect, false),
                                vec![last.as_str().to_sql_value()],
                            )?,
                        };
                        PushRecordsPage {
                            records,
                            rows: keys.len(),
                            at: Some(PushRecordsAt {
                                key: last.clone(),
                                cut: None,
                            }),
                        }
                    }
                }
            }
            PushRecordsRead::FirstOfRow { after, records } => {
                match keys(after.as_deref(), 1)?.into_iter().next() {
                    Some(key) => in_row(key, None, *records)?,
                    None => PushRecordsPage::default(),
                }
            }
            PushRecordsRead::RestOfRow {
                key,
                after,
                records,
            } => in_row(key.clone(), Some(*after), *records)?,
        };
        drop(tx);
        Ok(page)
    }

    /// SQLite picks by their columns the run and task rows that may not be
    /// settled at `through` and the cursor `resources`, and gives their
    /// heads as [`Self::ledger_heads`] does: no record is parsed.
    fn ledger_heads_open_at(&self, through: EventId, resources: i64) -> Result<Vec<LedgerHead>> {
        let tx = self.read_entry("ledger_heads_open_at")?;
        use crate::domain::ledger::builder::pushes::settled_at;
        let open = format!(
            "(r.final=FALSE OR r.legacy=TRUE OR r.ledger_v<>?2
                     OR {events}>?3
                     OR COALESCE({supervisor}, 0)>?3
                     OR COALESCE({windows}, 0)>?4
                     OR {life_ended_count}>0)",
            events = SqlRead::dialect(&tx).json_field_spaced(
                "r.inputs_through",
                "$.events",
                JsonType::Integer
            ),
            supervisor = SqlRead::dialect(&tx).json_field_spaced(
                "r.inputs_through",
                "$.supervisor",
                JsonType::Integer
            ),
            windows = SqlRead::dialect(&tx).json_field_spaced(
                "r.inputs_through",
                "$.resources",
                JsonType::Integer
            ),
            life_ended_count = SqlRead::dialect(&tx).json_array_length(&format!(
                "COALESCE({}, '[]')",
                SqlRead::dialect(&tx).json_field_spaced(
                    "r.inputs_through",
                    "$.life_ended",
                    JsonType::Any
                )
            )),
        );
        let mut heads = Vec::new();
        drop(tx);
        for kind in [LedgerKind::Run, LedgerKind::Task] {
            heads.extend(
                self.heads_where(
                    kind,
                    &open,
                    sql_params![
                        kind.as_str(),
                        crate::domain::ledger::LEDGER_V,
                        through.as_i64(),
                        resources
                    ],
                )?
                .into_iter()
                .filter(|(head, legacy)| !settled_at(head, *legacy, through, resources))
                .map(|(head, _)| head),
            );
        }
        Ok(heads)
    }

    /// SQLite picks the runs out of the task rows' details and a legacy
    /// row's values, and keeps those with no run row: no record is read.
    fn ledger_runs_without_row(&self) -> Result<Vec<crate::domain::RunId>> {
        let tx = self.read_entry("ledger_runs_without_row")?;
        let dialect = SqlRead::dialect(&tx);
        let statement = format!(
            "SELECT DISTINCT {run}
               FROM ledger_rows t, {detail} j
              WHERE t.kind=?1 AND {run_type}='text'
                AND NOT EXISTS (SELECT 1 FROM ledger_rows r WHERE r.kind=?2 AND r.key={run})
             UNION
             SELECT {run}
               FROM ledger_rows t, {legacy} j
              WHERE t.kind=?1 AND {run_type}='text'
                AND NOT EXISTS (SELECT 1 FROM ledger_rows r WHERE r.kind=?2 AND r.key={run})
              ORDER BY 1",
            run = dialect.json_element("j.value", JsonType::Text),
            run_type = dialect.json_element_type("j"),
            detail = dialect.json_elements_at_spaced("t.detail", "$.runs"),
            legacy = dialect.json_elements_at_spaced("COALESCE(t.legacy_values, '{}')", "$.runs"),
        );
        let rows = SqlRead::query_map(
            &tx,
            &statement,
            sql_params![LedgerKind::Task.as_str(), LedgerKind::Run.as_str()],
            |r| r.get::<_, String>(0),
        )?;
        Ok(rows
            .into_iter()
            .filter_map(|run| crate::domain::RunId::new(&run).ok())
            .collect())
    }

    /// SQLite walks the events of the legacy task rows' tasks by the index
    /// on their task and keeps the runs of their claims with no run row:
    /// no payload is read.
    fn ledger_legacy_runs_without_row(&self) -> Result<Vec<crate::domain::RunId>> {
        let tx = self.read_entry("ledger_legacy_runs_without_row")?;
        let statement = "SELECT DISTINCT e.run_id
               FROM ledger_rows t
               JOIN run_events e ON e.task_id=t.task_id
              WHERE t.kind=?1 AND t.legacy=TRUE AND e.kind=?2 AND e.run_id IS NOT NULL
                AND NOT EXISTS (SELECT 1 FROM ledger_rows r WHERE r.kind=?3 AND r.key=e.run_id)
              ORDER BY 1";
        let rows = SqlRead::query_map(
            &tx,
            statement,
            sql_params![
                LedgerKind::Task.as_str(),
                crate::domain::EventKind::RunClaimed.as_str(),
                LedgerKind::Run.as_str(),
            ],
            |r| r.get::<_, String>(0),
        )?;
        Ok(rows
            .into_iter()
            .filter_map(|run| crate::domain::RunId::new(&run).ok())
            .collect())
    }

    /// SQLite walks the claims of the task by the index on its task: no
    /// payload is read.
    fn ledger_runs_claimed(
        &self,
        task: crate::domain::TaskId,
    ) -> Result<Vec<crate::domain::RunId>> {
        let tx = self.read_entry("ledger_runs_claimed")?;
        let statement = "SELECT DISTINCT run_id FROM run_events
              WHERE task_id=?1 AND kind=?2 AND run_id IS NOT NULL
              ORDER BY 1";
        let rows = SqlRead::query_map(
            &tx,
            statement,
            sql_params![task.as_i64(), crate::domain::EventKind::RunClaimed.as_str()],
            |r| r.get::<_, String>(0),
        )?;
        Ok(rows
            .into_iter()
            .filter_map(|run| crate::domain::RunId::new(&run).ok())
            .collect())
    }

    /// SQLite picks them by their columns, from `after` on the primary key:
    /// no row is parsed.
    fn ledger_older_rows_after(
        &self,
        after: Option<&(LedgerKind, String)>,
        limit: usize,
    ) -> Result<Vec<(LedgerKind, String)>> {
        let tx = self.read_entry("ledger_older_rows_after")?;
        let (after_kind, after_key) = after.map_or((None, None), |(kind, key)| {
            (Some(kind.as_str()), Some(key.as_str()))
        });
        let from = if after.is_some() {
            "(kind, key) > (?7, ?8)"
        } else {
            "CAST(?7 AS TEXT) IS NULL AND CAST(?8 AS TEXT) IS NULL"
        };
        let statement = format!(
            "SELECT kind, key FROM ledger_rows
              WHERE {from} AND kind IN (?1, ?2, ?3, ?4, ?5)
                AND legacy=FALSE AND ledger_v<?6
              ORDER BY kind, key LIMIT ?9",
        );
        let rows = SqlRead::query_map(
            &tx,
            &statement,
            sql_params![
                LedgerKind::Run.as_str(),
                LedgerKind::Task.as_str(),
                LedgerKind::Session.as_str(),
                LedgerKind::Queue.as_str(),
                LedgerKind::Node.as_str(),
                crate::domain::ledger::LEDGER_V,
                after_kind,
                after_key,
                i64::try_from(limit).unwrap_or(i64::MAX),
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )?;
        rows.into_iter()
            .map(|row| {
                let (kind, key) = row;
                let kind = LedgerKind::ALL
                    .into_iter()
                    .find(|known| known.as_str() == kind)
                    .ok_or_else(|| anyhow::anyhow!("ledger row of unknown kind {kind}"))?;
                Ok((kind, key))
            })
            .collect()
    }

    /// SQLite picks the keys by the rules in each run row's detail: no
    /// row is parsed.
    fn ledger_runs_classified_otherwise(&self, version: &str) -> Result<Vec<String>> {
        let tx = self.read_entry("ledger_runs_classified_otherwise")?;
        let statement = format!(
            "SELECT key FROM ledger_rows
              WHERE kind=?1 AND {steps} IS NOT NULL
                AND {rules} IS NOT ?2
              ORDER BY key",
            steps = SqlRead::dialect(&tx).json_type_of("detail", "$.steps"),
            rules = SqlRead::dialect(&tx).json_field_spaced(
                "detail",
                "$.steps.fold.classes.rules",
                JsonType::Text
            ),
        );
        let keys: Vec<String> = SqlRead::query_map(
            &tx,
            &statement,
            sql_params![LedgerKind::Run.as_str(), version],
            |r| r.get(0),
        )?;
        Ok(keys)
    }

    /// SQLite looks each run row's next event after its inputs up in the
    /// index on the run: no record and no event is read.
    fn ledger_runs_past_their_rows(&self, through: EventId) -> Result<Vec<crate::domain::RunId>> {
        let tx = self.read_entry("ledger_runs_past_their_rows")?;
        let statement = format!(
            "SELECT r.key FROM ledger_rows r
              WHERE r.kind=?1
                AND EXISTS (SELECT 1 FROM run_events e
                             WHERE e.run_id=r.key AND e.id>{inputs} AND e.id<=?2)
              ORDER BY r.key",
            inputs = SqlRead::dialect(&tx).json_field_spaced(
                "r.inputs_through",
                "$.events",
                JsonType::Integer
            ),
        );
        let rows = SqlRead::query_map(
            &tx,
            &statement,
            sql_params![LedgerKind::Run.as_str(), through.as_i64()],
            |r| r.get::<_, String>(0),
        )?;
        Ok(rows
            .into_iter()
            .filter_map(|run| crate::domain::RunId::new(&run).ok())
            .collect())
    }

    /// SQLite picks the pushes of no run out of the task rows' records and
    /// out of the queue rows of a push alone (by their subject's kind): only
    /// those are parsed.
    fn ledger_runless_pushes(&self) -> Result<Vec<crate::domain::RunEvent>> {
        let tx = self.read_entry("ledger_runless_pushes")?;
        use crate::domain::EventKind;
        let statement = format!(
            "SELECT {record} FROM ledger_rows r, {records} j
              WHERE (r.kind=?1 OR (r.kind=?2 AND r.subject_kind=?3))
                AND {kind}=?3
                AND {run} IS NULL
              ORDER BY {id}",
            record = SqlRead::dialect(&tx).json_element("j.value", JsonType::Text),
            records = SqlRead::dialect(&tx).json_elements("r.records"),
            kind = SqlRead::dialect(&tx).json_field_spaced("j.value", "$.kind", JsonType::Text),
            run = SqlRead::dialect(&tx).json_field_spaced("j.value", "$.run_id", JsonType::Any),
            id = SqlRead::dialect(&tx).json_field_spaced("j.value", "$.id", JsonType::Integer),
        );
        let rows = SqlRead::query_map(
            &tx,
            &statement,
            sql_params![
                LedgerKind::Task.as_str(),
                LedgerKind::Queue.as_str(),
                EventKind::PushFinished.as_str(),
            ],
            |r| r.get::<_, String>(0),
        )?;
        let mut pushes: Vec<crate::domain::RunEvent> = rows
            .into_iter()
            .map(|row| {
                let event: crate::domain::RunEvent = serde_json::from_str(&row)?;
                Ok(crate::domain::ledger::builder::pushes::cut(&event))
            })
            .collect::<Result<_>>()?;
        pushes.dedup_by_key(|event| event.id);
        Ok(pushes)
    }

    /// Its columns and the ids of its records (SQLite's JSON functions over
    /// the stored array): no record is parsed here.
    fn ledger_queue_heads(&self) -> Result<Vec<crate::domain::ledger::queue::QueueHead>> {
        let tx = self.read_entry("ledger_queue_heads")?;
        let statement = queue_heads_sql(SqlRead::dialect(&tx), "", "");
        drop(tx);
        self.queue_heads(&statement, sql_params![LedgerKind::Queue.as_str()])
    }

    /// SQLite picks the rows by their columns
    /// ([`crate::domain::ledger::queue::QueueHead::forgotten_at`]; a row's
    /// `inputs_through` is at least its last record, [`LedgerRow::own`], so
    /// the columns tell that none of its records is past `at`, and a
    /// wait's `outcome` and `detail` tell what no life holds), and from
    /// the key `after` on by the primary key: it gives no head of the rest
    /// and parses no record's ids of them.
    fn ledger_queue_heads_at_after(
        &self,
        at: EventId,
        since: Since,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<crate::domain::ledger::queue::QueueHead>> {
        let tx = self.read_entry("ledger_queue_heads_at_after")?;
        let from = if after.is_some() {
            "r.key > ?3"
        } else {
            "CAST(?3 AS TEXT) IS NULL"
        };
        let filter = format!(
            "AND {from} AND NOT (r.final=TRUE AND r.ledger_v>=?9
               AND {events}<=?2 AND COALESCE({supervisor}, 0)<=?2
               AND ((r.subject_kind='live_alert_stream' AND COALESCE(r.outcome, '')='replaced')
                    OR (r.subject_kind='live_alert' AND r.ended_at IS NOT NULL)
                    OR (r.subject_kind IN ('slots_full', 'claim_hold', 'claim_deferral',
                                           'landing_hold')
                        AND r.ended_at IS NOT NULL AND r.ended_at < ?5 AND {events} < ?6
                        AND COALESCE(r.outcome, '') <> ?7
                        AND CASE r.subject_kind
                              WHEN 'slots_full' THEN COALESCE({end}, '') = ?8
                                   OR {supervisor_type} IS NOT 'text'
                              ELSE COALESCE({end}, '') <> ?7
                                   OR {ended_by_type} IS NOT 'text'
                            END)
                    OR COALESCE(r.subject_kind, '') NOT IN
                       ('live_alert_stream', 'live_alert', 'finding', 'landing_slot',
                        'slots_full', 'claim_hold', 'claim_deferral', 'landing_hold')))",
            supervisor_type =
                SqlRead::dialect(&tx).json_type_of("r.detail", "$.interval.supervisor"),
            ended_by_type = SqlRead::dialect(&tx).json_type_of("r.detail", "$.interval.ended_by"),
            events = SqlRead::dialect(&tx).json_field_spaced(
                "r.inputs_through",
                "$.events",
                JsonType::Integer
            ),
            supervisor = SqlRead::dialect(&tx).json_field_spaced(
                "r.inputs_through",
                "$.supervisor",
                JsonType::Integer
            ),
            end = SqlRead::dialect(&tx).json_field_spaced(
                "r.detail",
                "$.interval.end",
                JsonType::Text
            ),
        );
        let statement = queue_heads_sql(SqlRead::dialect(&tx), &filter, "LIMIT ?4");
        drop(tx);
        self.queue_heads(
            &statement,
            sql_params![
                LedgerKind::Queue.as_str(),
                at.as_i64(),
                after,
                i64::try_from(limit).unwrap_or(i64::MAX),
                since.ms,
                since.id.as_i64(),
                crate::domain::claim_hold::ENDED_SUPERVISOR_GONE,
                crate::domain::pre_claim::ENDED_FREED,
                crate::domain::ledger::LEDGER_V,
            ],
        )
    }

    /// Only the anchors of each session row are parsed: SQLite picks the
    /// first record and the `session_closed` ones out of the stored array.
    fn ledger_session_anchors(&self) -> Result<Vec<(String, crate::domain::RunEvent)>> {
        let tx = self.read_entry("ledger_session_anchors")?;
        let statement = format!(
            "SELECT r.key, {record} FROM ledger_rows r, {records}
              WHERE r.kind=?1
                AND ({index} = 0 OR {kind} = 'session_closed')
              ORDER BY r.key, {index}",
            record = SqlRead::dialect(&tx).json_element("j.value", JsonType::Text),
            records = SqlRead::dialect(&tx).json_elements_numbered("r.records", "j"),
            index = SqlRead::dialect(&tx).json_element_index("j"),
            kind = SqlRead::dialect(&tx).json_field_spaced("j.value", "$.kind", JsonType::Text),
        );
        let rows = SqlRead::query_map(
            &tx,
            &statement,
            sql_params![LedgerKind::Session.as_str()],
            |r| {
                let key: String = r.get(0)?;
                let event: String = r.get(1)?;
                Ok((key, event))
            },
        )?;
        rows.into_iter()
            .map(|row| {
                let (key, event) = row;
                Ok((key, serde_json::from_str(&event)?))
            })
            .collect()
    }

    /// One aggregate over the task rows not settled: no row is read.
    fn ledger_tasks_open_since(&self) -> Result<Since> {
        let tx = self.read_entry("ledger_tasks_open_since")?;
        let (ms, id): (Option<i64>, Option<i64>) = SqlRead::query_row(
            &tx,
            "SELECT MIN(COALESCE(started_at, -9223372036854775808)), MIN(inputs_from)
               FROM ledger_rows WHERE kind=?1 AND final=FALSE",
            sql_params![LedgerKind::Task.as_str()],
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

    /// One aggregate over the task rows, as [`Self::ledger_tasks_open_since`]
    /// with the cut to `at`: no row is read.
    fn ledger_tasks_open_since_at(&self, at: EventId) -> Result<Since> {
        let tx = self.read_entry("ledger_tasks_open_since_at")?;
        let (ms, id): (Option<i64>, Option<i64>) = SqlRead::query_row(
            &tx,
            &format!(
                "SELECT MIN(COALESCE(started_at, -9223372036854775808)), MIN(inputs_from)
               FROM ledger_rows
              WHERE kind=?1 AND inputs_from <= ?2
                AND (final=FALSE
                     OR {} > ?2)",
                SqlRead::dialect(&tx).scalar_max(
                    &SqlRead::dialect(&tx).json_field_spaced(
                        "inputs_through",
                        "$.events",
                        JsonType::Integer
                    ),
                    &format!(
                        "COALESCE({}, 0)",
                        SqlRead::dialect(&tx).json_field_spaced(
                            "inputs_through",
                            "$.supervisor",
                            JsonType::Integer
                        )
                    ),
                ),
            ),
            sql_params![LedgerKind::Task.as_str(), at.as_i64()],
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
        let tx = self.read_entry("ledger_settled_waits")?;
        let statement = format!(
            "SELECT kind, key, task_id, ledger_v, built_at, inputs_through, final,
                    legacy_values, started_at, ended_at, outcome, totals, intervals,
                    attributes, after_claim, detail, inputs_from, '[]' AS records,
                    subject_kind, subject_id, round, route, provider, model
               FROM ledger_rows
              WHERE kind=?1
                AND subject_kind IN ('slots_full', 'claim_hold', 'claim_deferral')
                AND final=TRUE AND ended_at < ?2 AND (outcome IS NULL OR outcome<>?4)
                AND {} < ?3
                AND started_at <= ?7
                AND (ended_at >= ?5 OR {} >= ?6)
              ORDER BY key",
            SqlRead::dialect(&tx).json_field_spaced(
                "inputs_through",
                "$.events",
                JsonType::Integer
            ),
            SqlRead::dialect(&tx).json_field_spaced(
                "inputs_through",
                "$.events",
                JsonType::Integer
            ),
        );
        let rows = SqlRead::query_map(
            &tx,
            &statement,
            sql_params![
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
        rows.into_iter().collect()
    }

    /// SQLite picks the stops out of the stored arrays: no other record is
    /// parsed.
    fn ledger_supervisor_stops(&self) -> Result<Vec<crate::domain::RunEvent>> {
        let tx = self.read_entry("ledger_supervisor_stops")?;
        let statement = format!(
            "SELECT {} FROM ledger_rows r, {} j
              WHERE r.kind=?1 AND {}=?2
              ORDER BY {}",
            SqlRead::dialect(&tx).json_element("j.value", JsonType::Text),
            SqlRead::dialect(&tx).json_elements("r.records"),
            SqlRead::dialect(&tx).json_field_spaced("j.value", "$.kind", JsonType::Text),
            SqlRead::dialect(&tx).json_field_spaced("j.value", "$.id", JsonType::Integer),
        );
        let rows = SqlRead::query_map(
            &tx,
            &statement,
            sql_params![
                LedgerKind::Queue.as_str(),
                crate::domain::EventKind::SupervisorStopped.as_str()
            ],
            |r| r.get::<_, String>(0),
        )?;
        rows.into_iter()
            .map(|row| Ok(serde_json::from_str(&row)?))
            .collect()
    }

    /// SQLite picks the stops out of the stored arrays of the rows from the
    /// key of `after` on, by the primary key, and stops at `limit`: a page
    /// parses no other record and reads no row before it nor past it.
    fn ledger_supervisor_stops_after(
        &self,
        after: Option<(&str, i64)>,
        limit: usize,
    ) -> Result<Vec<((String, i64), crate::domain::RunEvent)>> {
        let tx = self.read_entry("ledger_supervisor_stops_after")?;
        let statement = stops_after_sql(SqlRead::dialect(&tx), after.is_some());
        let (key, index) = after.unzip();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = SqlRead::query_map(
            &tx,
            &statement,
            sql_params![
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
        rows.into_iter()
            .map(|row| {
                let (key, index, event) = row;
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
                let Some(row) = SqlRead::query_row(
                    tx,
                    "SELECT * FROM ledger_rows WHERE kind=?1 AND key=?2",
                    sql_params![LedgerKind::Node.as_str(), key],
                    ledger_row,
                )
                .optional()?
                .transpose()?
                else {
                    continue;
                };
                match node::retained(&row, before_day) {
                    None => {
                        removed += SqlWrite::execute(
                            tx,
                            "DELETE FROM ledger_rows WHERE kind=?1 AND key=?2",
                            sql_params![LedgerKind::Node.as_str(), key],
                        )?;
                    }
                    Some(kept) if kept != row => {
                        SqlWrite::execute(
                            tx,
                            "UPDATE ledger_rows SET detail=?3 WHERE kind=?1 AND key=?2",
                            sql_params![LedgerKind::Node.as_str(), key, text(&kept.detail)?],
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
fn ledger_row(r: &SqlRow<'_>) -> SqlResult<Result<LedgerRow>> {
    let json = |column: &str| -> SqlResult<String> { r.get(column) };
    let optional = |column: &str| -> SqlResult<Option<String>> { r.get(column) };
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
mod omitted_tests;
#[cfg(test)]
mod read_from_tests;

#[cfg(test)]
mod tests {
    use rusqlite::params;

    use super::*;
    use crate::domain::ledger::{self, LEDGER_V};
    use crate::domain::{EventId, RunEvent, RunId};
    use crate::infrastructure::sqlite::event_row;

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

    /// The rows of a list of keys are those [`LedgerStore::ledger_row`]
    /// reads one by one, in the order of the keys: a key of no row, of
    /// another kind's key among them, gives none and the others are read;
    /// the one statement finds each row by the primary key.
    #[test]
    fn the_rows_of_a_list_of_keys_are_those_read_one_by_one_and_none_of_a_key_of_no_row() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let session = |id: i64, kind: &str, payload: Value| RunEvent {
            task_id: None,
            run_id: None,
            ..event(id, kind, payload, id)
        };
        let events: Vec<RunEvent> = (0..3)
            .flat_map(|planner| {
                let opened = 2 * planner + 1;
                [
                    session(
                        opened,
                        "session_opened",
                        json!({"kind": "runtime_planner", "planner_id": planner}),
                    ),
                    session(
                        opened + 1,
                        "session_closed",
                        json!({"opened_event_id": opened, "session_id": format!("s{planner}")}),
                    ),
                ]
            })
            .collect();
        let rows = ledger::session::session_rows(&events, "2030-01-01T00:00:00.000Z");
        assert_eq!(rows.len(), 3);
        for row in &rows {
            queue.write_ledger_row(row).unwrap();
        }
        let key = |row: &LedgerRow| (row.kind, row.key.clone());
        let keys = vec![
            key(&rows[2]),
            (LedgerKind::Session, "no such session".to_owned()),
            (LedgerKind::Run, rows[0].key.clone()),
            key(&rows[0]),
        ];
        let one_by_one: Vec<LedgerRow> = keys
            .iter()
            .filter_map(|(kind, key)| queue.ledger_row(*kind, key).unwrap())
            .collect();
        assert_eq!(one_by_one, [rows[2].clone(), rows[0].clone()]);
        assert_eq!(queue.ledger_rows_of(&keys).unwrap(), one_by_one);
        assert!(queue.ledger_rows_of(&[]).unwrap().is_empty());
        let plan: Vec<String> = queue
            .conn
            .prepare(&format!(
                "EXPLAIN QUERY PLAN {}",
                rows_of_sql(SqlDialect::Sqlite)
            ))
            .unwrap()
            .query_map(["[]"], |r| r.get::<_, String>(3))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let plan = plan.join("\n");
        assert!(
            plan.contains("SEARCH r USING INDEX sqlite_autoindex_ledger_rows_1 (kind=? AND key=?)"),
            "{plan}"
        );
        assert!(!plan.contains("SCAN r"), "{plan}");
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

    /// The heads at a position SQLite picks by the rows' columns are those
    /// [`ledger::queue::QueueHead::forgotten_at`] leaves of the heads of
    /// every row, at every position, and a page of them goes from a key on.
    #[test]
    fn the_queue_heads_at_a_position_are_those_not_forgotten_there() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let log = ledger::testkit::forgotten_log(2);
        let rows = ledger::testkit::built_and_settled(&log.events, ledger::testkit::ms(100_000));
        for row in &rows {
            queue.write_ledger_row(row).unwrap();
        }
        // A settled row its supervisor's life read past later positions.
        let reached = rows
            .iter()
            .find(|row| {
                row.is_final
                    && row.kind == LedgerKind::Queue
                    && row.subject.as_ref().is_some_and(|s| s.kind == "mark_added")
            })
            .unwrap();
        queue
            .conn
            .execute(
                "UPDATE ledger_rows SET inputs_through=json_set(inputs_through, '$.supervisor', ?1)
                  WHERE kind='queue' AND key=?2",
                rusqlite::params![log.last().as_i64(), reached.key],
            )
            .unwrap();
        // A settled stream of no outcome is read, a settled row of no
        // subject is not.
        let stream = rows
            .iter()
            .find(|row| {
                row.kind == LedgerKind::Queue
                    && row.key != reached.key
                    && row
                        .subject
                        .as_ref()
                        .is_some_and(|s| s.kind == "live_alert_stream")
            })
            .unwrap();
        let bare = rows
            .iter()
            .rfind(|row| {
                row.is_final
                    && row.kind == LedgerKind::Queue
                    && row.key != reached.key
                    && row.subject.as_ref().is_some_and(|s| s.kind == "mark_added")
            })
            .unwrap();
        queue
            .conn
            .execute(
                "UPDATE ledger_rows SET final=1, outcome=NULL WHERE kind='queue' AND key=?1",
                [&stream.key],
            )
            .unwrap();
        queue
            .conn
            .execute(
                "UPDATE ledger_rows SET subject_kind=NULL WHERE kind='queue' AND key=?1",
                [&bare.key],
            )
            .unwrap();
        let every = queue.ledger_queue_heads().unwrap();
        let earliest = Since {
            ms: i64::MIN,
            id: EventId::new(0),
        };
        let held = queue.ledger_queue_heads_at(log.last(), earliest).unwrap();
        assert!(held.iter().any(|head| head.head.key == stream.key));
        assert!(!held.iter().any(|head| head.head.key == bare.key));
        let mut fewest = every.len();
        for at in log.events.iter().map(|event| event.id) {
            let expected: Vec<ledger::queue::QueueHead> = every
                .iter()
                .filter(|head| !head.forgotten_at(at, earliest))
                .cloned()
                .collect();
            assert_eq!(
                queue.ledger_queue_heads_at(at, earliest).unwrap(),
                expected,
                "{at}"
            );
            fewest = fewest.min(expected.len());
        }
        assert!(fewest < every.len());
        let at = log.last();
        let all = queue.ledger_queue_heads_at(at, earliest).unwrap();
        // A row left out at this version is read at an older one.
        queue
            .conn
            .execute(
                "UPDATE ledger_rows SET ledger_v=?1 WHERE kind='queue' AND key=?2",
                rusqlite::params![LEDGER_V - 1, bare.key],
            )
            .unwrap();
        let read = queue.ledger_queue_heads_at(at, earliest).unwrap();
        assert!(read.iter().any(|head| head.head.key == bare.key));
        queue
            .conn
            .execute(
                "UPDATE ledger_rows SET ledger_v=?1 WHERE kind='queue' AND key=?2",
                rusqlite::params![LEDGER_V, bare.key],
            )
            .unwrap();
        let is_reached = |head: &ledger::queue::QueueHead| head.head.key == reached.key;
        assert!(!all.iter().any(is_reached));
        let before = EventId::new(at.as_i64() - 1);
        assert!(
            queue
                .ledger_queue_heads_at(before, earliest)
                .unwrap()
                .iter()
                .any(is_reached)
        );
        let first = queue
            .ledger_queue_heads_at_after(at, earliest, None, 2)
            .unwrap();
        assert_eq!(first, all[..2]);
        let next = queue
            .ledger_queue_heads_at_after(at, earliest, Some(&first[1].head.key), 3)
            .unwrap();
        assert_eq!(next, all[2..5]);
    }

    /// The waits settled SQLite leaves out at a position, for a [`Since`],
    /// are those [`ledger::queue::QueueHead::forgotten_at`] does, at every
    /// position and several [`Since`]s: the waits of each kind ended before
    /// it, and none open, not settled, ended by a life, reached by a life or
    /// ended from it on, which rows edited to say so show.
    #[test]
    fn the_waits_at_a_position_and_a_since_are_those_not_forgotten_there() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let log = ledger::testkit::settled_waits_log(6);
        let rows = ledger::testkit::built_and_settled(&log.events, ledger::testkit::ms(100_000));
        for row in &rows {
            queue.write_ledger_row(row).unwrap();
        }
        let waits = |kind: &str| -> Vec<String> {
            rows.iter()
                .filter(|row| {
                    row.is_final
                        && row.kind == LedgerKind::Queue
                        && row.subject.as_ref().is_some_and(|s| s.kind == kind)
                        && row.detail["interval"]["supervisor"] == "f"
                })
                .map(|row| row.key.clone())
                .collect()
        };
        let (full, hold, deferral, landing) = (
            waits("slots_full"),
            waits("claim_hold"),
            waits("claim_deferral"),
            waits("landing_hold"),
        );
        let edit = |sql: &str, key: &str| {
            let changed = queue
                .conn
                .execute(
                    &format!("UPDATE ledger_rows SET {sql} WHERE kind='queue' AND key=?1"),
                    [key],
                )
                .unwrap();
            assert_eq!(changed, 1, "{sql} {key}");
        };
        let gone = crate::domain::claim_hold::ENDED_SUPERVISOR_GONE;
        // Not settled, open though settled, and ended by a life.
        edit("final=0", &hold[1]);
        edit("ended_at=NULL", &landing[1]);
        edit(&format!("outcome='{gone}'"), &deferral[1]);
        // Reached by a life by what the detail says: a slots full not
        // freed, and a stop's end. Then those of no one to reach them.
        let detail = |interval: Value| json!({ "interval": interval }).to_string();
        edit(
            &format!(
                "detail='{}'",
                detail(json!({"end": "other", "supervisor": "f"}))
            ),
            &full[1],
        );
        edit(
            &format!("detail='{}'", detail(json!({"end": gone, "ended_by": "f"}))),
            &hold[2],
        );
        edit(&format!("detail='{}'", detail(json!({"end": 5}))), &full[2]);
        edit(
            &format!("detail='{}'", detail(json!({"end": gone}))),
            &hold[3],
        );
        edit("detail='{}'", &deferral[2]);
        edit(
            &format!(
                "detail='{}'",
                detail(json!({"end": "other", "supervisor": 5}))
            ),
            &full[3],
        );
        edit(
            &format!("detail='{}'", detail(json!({"end": gone, "ended_by": 5}))),
            &deferral[3],
        );
        let every = queue.ledger_queue_heads().unwrap();
        let of_key = |key: &String| every.iter().find(|head| &head.head.key == key).unwrap();
        let end_of = |key: &String| of_key(key).ended_at.unwrap();
        let events_of = |key: &String| of_key(key).head.inputs_through.events;
        let at = log.last();
        let before = Since {
            ms: i64::MIN,
            id: EventId::new(0),
        };
        // By its time, and by its event: a wait ended at the Since is kept.
        let by_time = Since {
            ms: end_of(&full[0]),
            ..Since::NONE
        };
        let by_id = Since {
            ms: i64::MAX,
            id: events_of(&landing[0]),
        };
        let exact = queue.ledger_tasks_open_since().unwrap();
        for (since, name) in [
            (Since::NONE, "none"),
            (before, "before"),
            (by_time, "time"),
            (by_id, "id"),
            (exact, "exact"),
        ] {
            for at in log.events.iter().map(|event| event.id) {
                let expected: Vec<ledger::queue::QueueHead> = every
                    .iter()
                    .filter(|head| !head.forgotten_at(at, since))
                    .cloned()
                    .collect();
                assert_eq!(
                    queue.ledger_queue_heads_at(at, since).unwrap(),
                    expected,
                    "{name} {at}"
                );
            }
        }
        let gives = |since: Since, key: &String| {
            queue
                .ledger_queue_heads_at(at, since)
                .unwrap()
                .iter()
                .any(|head| &head.head.key == key)
        };
        for key in [&full[0], &hold[0], &deferral[0], &landing[0]] {
            assert!(!gives(Since::NONE, key), "{key}");
            assert!(gives(before, key), "{key}");
        }
        // Whatever the Since.
        for key in [&hold[1], &landing[1], &deferral[1], &full[1], &hold[2]] {
            assert!(gives(Since::NONE, key), "{key}");
        }
        // No one to reach them.
        for key in [&full[2], &hold[3], &deferral[2], &full[3], &deferral[3]] {
            assert!(!gives(Since::NONE, key), "{key}");
        }
        assert!(gives(by_time, &full[0]));
        assert!(!gives(by_id, &full[0]));
        assert!(gives(by_id, &landing[0]));
    }

    /// The events of `log` in `queue`, and the rows a builder from its
    /// cursors' start drains of them at `now`.
    fn drained_on_the_store(queue: &SqliteQueue, log: &ledger::testkit::Log, now: i64) {
        use crate::application::supervise::{Retain, build_pass};
        use crate::domain::ledger::builder::Builder;
        queue
            .conn
            .pragma_update(None, "foreign_keys", false)
            .unwrap();
        for event in &log.events {
            queue
                .conn
                .execute(
                    "INSERT INTO run_events(id, task_id, run_id, kind, payload, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        event.id.as_i64(),
                        event.task_id.map(|task| task.as_i64()),
                        event.run_id.as_ref().map(|run| run.as_str()),
                        event.kind.as_str(),
                        event.payload.to_string(),
                        event.created_at,
                    ],
                )
                .unwrap();
        }
        let mut builder = Builder::default();
        for _ in 0..50 {
            let built = build_pass(
                queue,
                &mut builder,
                (now, "2030-01-01T00:00:00.000Z"),
                Retain::default(),
                None,
            );
            assert_eq!(built.failure, None);
            if built.backlog == 0 {
                break;
            }
        }
    }

    /// Through the SQLite adapter, a builder's rows of a history and the
    /// same with more settled rows of each kind a start forgets: the heads a
    /// start takes at the position it goes on from are as many, and so is
    /// what it keeps of the queue's rows; the heads of every row are not.
    #[test]
    fn a_start_on_the_store_takes_as_many_queue_heads_however_many_rows_it_forgets() {
        use crate::application::supervise::start_builder;
        let taken = |fill: usize| {
            let dir = tempfile::tempdir().unwrap();
            let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
            let log = ledger::testkit::forgotten_log(fill);
            let now = ledger::testkit::ms(100_000);
            drained_on_the_store(&queue, &log, now);
            let started = start_builder(&queue, now).unwrap();
            assert_eq!(started.events, log.last());
            let since = queue.ledger_tasks_open_since_at(log.last()).unwrap();
            (
                queue
                    .ledger_queue_heads_at(log.last(), since)
                    .unwrap()
                    .len(),
                queue.ledger_queue_heads().unwrap().len(),
                started.kept().size().queue_rows,
            )
        };
        let (few, many) = (taken(2), taken(40));
        assert_eq!(few.0, many.0);
        assert_eq!(few.2, many.2);
        assert_eq!(many.1 - few.1, 38 * 3);
    }

    /// Through the SQLite adapter, a builder's rows of a history and the
    /// same with more waits settled that an event ended, every task row
    /// settled: the heads a start receives at the position it goes on from
    /// are as many, and so is what it keeps of the queue's rows and of the
    /// rows dropped, and its waits; the heads of every row are not.
    #[test]
    fn a_start_on_the_store_keeps_as_much_however_many_waits_settled() {
        let taken = |fill: usize| {
            let dir = tempfile::tempdir().unwrap();
            let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
            let log = ledger::testkit::settled_waits_log(fill);
            let now = ledger::testkit::ms(100_000);
            drained_on_the_store(&queue, &log, now);
            queue
                .conn
                .execute("UPDATE ledger_rows SET final=1 WHERE kind='task'", [])
                .unwrap();
            let started = crate::application::supervise::start_builder(&queue, now).unwrap();
            assert_eq!(started.events, log.last());
            let since = queue.ledger_tasks_open_since_at(log.last()).unwrap();
            assert_eq!(since, Since::NONE);
            let size = started.kept().size();
            (
                (
                    queue
                        .ledger_queue_heads_at(log.last(), since)
                        .unwrap()
                        .len(),
                    size.queue_rows,
                    size.dropped_rows,
                    started.kept().pre_claim().waits(),
                ),
                queue.ledger_queue_heads().unwrap().len(),
            )
        };
        let (few, many) = (taken(2), taken(40));
        assert_eq!(few.0, many.0);
        assert_eq!(many.1 - few.1, 38 * 4);
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
            &wait_rows_sql(SqlDialect::Sqlite, true),
            &[&queue_kind, &0, &0, &"x", &"1", &10],
        );
        assert!(waits.contains("kind=? AND key>?"), "{waits}");
        let stops = plan(
            &stops_after_sql(SqlDialect::Sqlite, true),
            &[&queue_kind, &"supervisor_stopped", &"1", &0, &10],
        );
        assert!(stops.contains("kind=? AND key>?"), "{stops}");
    }

    /// A page of the rows of a kind searches the primary key: past a place by
    /// the key's range, so it walks no row before it, and from the first by
    /// the kind, in the index's order, with no scan and no sort.
    #[test]
    fn a_page_of_the_rows_of_a_kind_searches_the_primary_key() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let plan = |sql: &str| -> String {
            let mut statement = queue
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let count = statement.parameter_count();
            let rows = statement
                .query_map(
                    rusqlite::params_from_iter(std::iter::repeat_n("1", count)),
                    |r| r.get::<_, String>(3),
                )
                .unwrap();
            rows.map(Result::unwrap).collect::<Vec<_>>().join("\n")
        };
        let after = plan(rows_after_sql(true));
        assert!(after.contains("kind=? AND key>?"), "{after}");
        let first = plan(rows_after_sql(false));
        assert!(
            first
                .contains("SEARCH ledger_rows USING INDEX sqlite_autoindex_ledger_rows_1 (kind=?)"),
            "{first}"
        );
        assert!(!first.contains("SCAN"), "{first}");
        assert!(!first.contains("USE TEMP B-TREE"), "{first}");
    }

    /// The two statements of a page give the rows, their order and their
    /// count of the one statement with the bound key's `OR` they stand for:
    /// from the first, from a key on the way, from the last row's key and
    /// from past it, with a limit under the rest, of the rest exactly and
    /// over it, and none of another kind.
    #[test]
    fn a_page_of_the_rows_gives_the_rows_of_the_one_statement() {
        use crate::domain::ledger::testkit::run_row;
        const ONE_STATEMENT: &str = "SELECT * FROM ledger_rows
             WHERE kind=?1 AND (?2 IS NULL OR key > ?2) ORDER BY key LIMIT ?3";
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        for key in ["d", "b", "e", "a", "c"] {
            queue
                .write_ledger_row(&run_row(key, None, Vec::new()))
                .unwrap();
        }
        for key in ["0", "b", "z"] {
            let row = LedgerRow {
                kind: LedgerKind::Queue,
                ..run_row(key, None, Vec::new())
            };
            queue.write_ledger_row(&row).unwrap();
        }

        let one_statement = |after: Option<&str>, limit: i64| -> Vec<LedgerRow> {
            let mut statement = queue.conn.prepare(ONE_STATEMENT).unwrap();
            let rows = statement
                .query_map(params![LedgerKind::Run.as_str(), after, limit], |row| {
                    with_row(row, ledger_row)
                })
                .unwrap();
            rows.map(|row| row.unwrap().unwrap()).collect()
        };
        let cases: [(Option<&str>, usize, &[&str]); 8] = [
            (None, 2, &["a", "b"]),
            (None, 5, &["a", "b", "c", "d", "e"]),
            (None, 9, &["a", "b", "c", "d", "e"]),
            (Some("b"), 2, &["c", "d"]),
            (Some("b"), 3, &["c", "d", "e"]),
            (Some("b"), 9, &["c", "d", "e"]),
            (Some("e"), 9, &[]),
            (Some("f"), 9, &[]),
        ];
        for (after, limit, want) in cases {
            let page = queue
                .ledger_rows_after(LedgerKind::Run, after, limit)
                .unwrap();
            let got: Vec<&str> = page.iter().map(|row| row.key.as_str()).collect();
            assert_eq!(got, want, "{after:?} {limit}");
            assert_eq!(
                page,
                one_statement(after, i64::try_from(limit).unwrap()),
                "{after:?} {limit}"
            );
        }
    }

    /// The last event naming a supervisor up to an id is the last of every
    /// event up to it whose payload names it as the domain reads a name
    /// ([`crate::domain::supervisor_life::supervisor_of`]), whatever its
    /// kind: none after the id, none naming another supervisor or none,
    /// none naming it by a number; `None` with none. The read looks up the
    /// index `events_by_supervisor`.
    #[test]
    fn the_last_event_naming_a_supervisor_up_to_an_id_is_looked_up_by_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let events: [(i64, &str, Value); 9] = [
            (1, "supervisor_started", json!({"supervisor": "s"})),
            (2, "supervisor_started", json!({"supervisor": "t"})),
            (3, "observation", json!({})),
            (
                4,
                "slots_full_started",
                json!({"supervisor": "s", "class": "heavy"}),
            ),
            (5, "observation", json!({"supervisor": 7})),
            (6, "supervisor_stopped", json!({"supervisor": "t"})),
            (
                7,
                "live_alert_baseline",
                json!({"supervisor": "s", "judgment": "x"}),
            ),
            (8, "observation", json!({"note": {"supervisor": "s"}})),
            (9, "observation", json!({"supervisor": {"x": 1}})),
        ];
        for (id, kind, payload) in &events {
            queue
                .conn
                .execute(
                    "INSERT INTO run_events(id, kind, payload, actor_role, actor_id)
                     VALUES (?1, ?2, ?3, 'supervisor', 'supervisor')",
                    params![id, kind, payload.to_string()],
                )
                .unwrap();
        }
        let every: Vec<RunEvent> = queue
            .conn
            .prepare("SELECT * FROM run_events ORDER BY id")
            .unwrap()
            .query_map([], event_row)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(every.len(), events.len());
        for supervisor in ["s", "t", "u", "7", "{\"x\":1}"] {
            for upto in 0..=10 {
                let expected = every
                    .iter()
                    .rfind(|event| {
                        event.id.as_i64() <= upto
                            && crate::domain::supervisor_life::supervisor_of(event)
                                == Some(supervisor)
                    })
                    .cloned();
                let read = queue
                    .last_event_naming_supervisor(supervisor, EventId::new(upto))
                    .unwrap();
                assert_eq!(read, expected, "{supervisor} {upto}");
            }
        }
        let id = |supervisor, upto| {
            queue
                .last_event_naming_supervisor(supervisor, EventId::new(upto))
                .unwrap()
                .map(|event| event.id.as_i64())
        };
        assert_eq!(id("s", 9), Some(7));
        assert_eq!(id("s", 6), Some(4));
        assert_eq!(id("s", 3), Some(1));
        assert_eq!(id("s", 0), None);
        assert_eq!(id("t", 5), Some(2));
        assert_eq!(id("t", 6), Some(6));
        assert_eq!(id("u", 9), None);
        assert_eq!(id("7", 9), None);
        assert_eq!(id("{\"x\":1}", 10), None);

        let mut statement = queue
            .conn
            .prepare(&format!(
                "EXPLAIN QUERY PLAN {}",
                last_event_naming_supervisor_sql(SqlDialect::Sqlite)
            ))
            .unwrap();
        let plan = statement
            .query_map(params!["s", 9], |r| r.get::<_, String>(3))
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plan.contains("events_by_supervisor"), "{plan}");
    }

    /// The start of the task rows for a position before the one the rows
    /// were written at is no later than the one of the rows folded up to
    /// that position, and is that of the rows whose inputs start by it,
    /// not settled or folded from an input past it; at the position the
    /// rows were written at it is the start of those not settled. A row
    /// whose records read whole no more leaves it as it is.
    #[test]
    fn the_start_of_the_tasks_for_a_position_is_no_later_than_the_one_there() {
        use crate::domain::ledger::testkit::{built_and_settled, ms, queue_across, waits_across};
        let (waits, position, at) = waits_across();
        let (queued, queued_position) = queue_across();
        let mut differs = false;
        for (events, now) in [
            (waits.events[..position].to_vec(), ms(at)),
            (queued.events[..queued_position].to_vec(), ms(40)),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
            let rows = built_and_settled(&events, now);
            for row in &rows {
                queue.write_ledger_row(row).unwrap();
            }
            queue
                .conn
                .execute("UPDATE ledger_rows SET records='[{\"id\":1}]'", [])
                .unwrap();
            let p = events.last().unwrap().id;
            assert_eq!(
                queue.ledger_tasks_open_since_at(p).unwrap(),
                queue.ledger_tasks_open_since().unwrap()
            );
            for at in 0..=p.as_i64() {
                let at = EventId::new(at);
                let read = queue.ledger_tasks_open_since_at(at).unwrap();
                let cut: Vec<LedgerRow> = rows
                    .iter()
                    .filter(|row| row.inputs_from <= at)
                    .map(|row| LedgerRow {
                        is_final: row.is_final && row.inputs_through.last() <= at,
                        ..row.clone()
                    })
                    .collect();
                assert_eq!(read, Since::of(&cut), "{at}");
                let upto: Vec<RunEvent> = events.iter().filter(|e| e.id <= at).cloned().collect();
                let exact = Since::of(&built_and_settled(&upto, now));
                assert!(
                    read.ms <= exact.ms && read.id <= exact.id,
                    "{at}: {read:?} {exact:?}"
                );
                differs |= read != queue.ledger_tasks_open_since().unwrap();
            }
        }
        assert!(differs);

        // A settled task row folded from a supervisor's event past `at`
        // (its inputs' last by the supervisor, not by the events) counts.
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let mut row = built_and_settled(&waits.events[..position], ms(at))
            .into_iter()
            .find(|row| row.kind == LedgerKind::Task)
            .unwrap();
        row.is_final = true;
        row.inputs_through.supervisor = Some(EventId::new(row.inputs_through.events.as_i64() + 5));
        queue.write_ledger_row(&row).unwrap();
        let start = Since {
            ms: row.started_at.unwrap_or(i64::MIN),
            id: row.inputs_from,
        };
        let events = row.inputs_through.events.as_i64();
        assert_eq!(
            queue
                .ledger_tasks_open_since_at(EventId::new(events + 4))
                .unwrap(),
            start
        );
        assert_eq!(
            queue
                .ledger_tasks_open_since_at(EventId::new(events + 5))
                .unwrap(),
            Since::NONE
        );
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

    /// R is written with the session rows' mark in one transaction and
    /// reads back with it, in a row of its own kind that no reading of the
    /// rows by their kind (nor a window) reads; a write with the session
    /// rows' mark alone leaves the R held. A write that fails part way (as
    /// R is written, after the rows and the session rows' mark) writes no
    /// row and neither mark.
    #[test]
    fn r_is_written_with_the_session_rows_mark_all_or_none() {
        use crate::domain::ledger::session::written::{
            Cursors, Marks, RowsWritten, SessionsWritten,
        };
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        assert_eq!(queue.ledger_marks().unwrap(), None);
        let batch: Vec<(LedgerRow, Option<LedgerRow>)> =
            sessions().into_iter().map(|row| (row, None)).collect();
        let mut marks = Marks {
            sessions: SessionsWritten::at(EventId::new(7)),
            rows: Some(RowsWritten::at(
                Cursors {
                    events: EventId::new(5),
                    samples: 2,
                    steps: 3,
                    resources: 4,
                    inside_resources: 6,
                },
                1_900_000_000_000,
            )),
            omitted: None,
        };
        let rows = marks.rows.as_mut().unwrap();
        rows.queue = rows.position(true);
        rows.core = Some(written::RowPosition {
            through: written::Cursors {
                events: EventId::new(7),
                ..rows.through
            },
            read_ms: 1_900_000_000_500,
        });
        marks.rows.as_mut().unwrap().take_offset = 1700;
        marks.rows.as_mut().unwrap().time_pending = Some(1_900_000_001_000);
        queue
            .conn
            .execute_batch(
                "CREATE TEMP TRIGGER part_way BEFORE INSERT ON ledger_rows
                   WHEN NEW.kind = 'rows_written'
                 BEGIN SELECT RAISE(ABORT, 'part way'); END;",
            )
            .unwrap();
        let error = queue
            .write_session_rows_marked(&batch, Some(&marks))
            .unwrap_err();
        assert!(format!("{error:#}").contains("part way"), "{error:#}");
        assert_eq!(rows_of_kind(&queue, "session"), 0);
        assert_eq!(queue.ledger_marks().unwrap(), None);
        queue.conn.execute_batch("DROP TRIGGER part_way").unwrap();

        assert!(
            queue
                .write_session_rows_marked(&batch, Some(&marks))
                .unwrap()
        );
        assert_eq!(queue.ledger_marks().unwrap(), Some(marks.clone()));
        assert_eq!(queue.sessions_written().unwrap(), Some(marks.sessions));
        assert_eq!(rows_of_kind(&queue, ledger::session::written::ROWS_KIND), 1);
        for kind in LedgerKind::ALL {
            assert!(
                queue
                    .ledger_rows(kind)
                    .unwrap()
                    .iter()
                    .all(|row| row.key != ledger::session::written::KEY),
                "{kind:?}"
            );
            assert!(
                queue
                    .ledger_heads(kind)
                    .unwrap()
                    .iter()
                    .all(|head| head.key != ledger::session::written::KEY),
                "{kind:?}"
            );
        }
        let window = queue
            .ledger_window(&ledger::window::WindowQuery::default())
            .unwrap();
        assert!(!format!("{window:?}").contains(ledger::session::written::ROWS_KIND));

        // The session rows' mark alone moves W and leaves R.
        let later = SessionsWritten::at(EventId::new(9));
        assert!(queue.write_session_rows_over(&[], Some(&later)).unwrap());
        assert_eq!(
            queue.ledger_marks().unwrap(),
            Some(Marks {
                sessions: later,
                ..marks.clone()
            })
        );
    }

    /// The window of the store's rows: through a page's length exactly and
    /// past it no row is lost or repeated, its records are each event once
    /// by id, none on a row, no queue row and no run row's steps; a run on
    /// its way, folded at the read, the same.
    #[test]
    fn the_window_reads_the_rows_by_pages_and_holds_each_event_once() {
        use crate::domain::ledger::window::{ROWS_PER_PAGE, WindowQuery};
        use crate::domain::marks::utc_text;
        let run_id = |n: usize| format!("{n:08x}-1111-4111-8111-111111111111");
        let way = run_id(999);
        for runs in [ROWS_PER_PAGE, ROWS_PER_PAGE + 1, 2 * ROWS_PER_PAGE + 1] {
            let dir = tempfile::tempdir().unwrap();
            let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
            let event = |kind: &str, task: Option<i64>, run: Option<&str>, payload: Value| {
                queue
                    .conn
                    .execute(
                        "INSERT INTO run_events(task_id, run_id, kind, payload, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            task,
                            run,
                            kind,
                            payload.to_string(),
                            utc_text(1_900_000_000_000)
                        ],
                    )
                    .unwrap();
            };
            queue
                .conn
                .execute_batch("PRAGMA foreign_keys = OFF; PRAGMA ignore_check_constraints = ON;")
                .unwrap();
            for n in 0..runs {
                let run = run_id(n);
                let task = Some(n as i64 + 1);
                event("run_claimed", task, Some(&run), json!({}));
                event(
                    "run_phase_changed",
                    task,
                    Some(&run),
                    json!({"phase": "ended", "blocker": "runtime", "holds": "none",
                           "attempt": {"kind": "first", "n": 1}, "cause": "x", "v": 1}),
                );
            }
            event("update_installed", None, None, json!({"supervisor": "s"}));
            event("task_created", Some(999), None, json!({}));
            event("run_claimed", Some(999), Some(&way), json!({}));
            let events: Vec<RunEvent> = queue.all_events().unwrap();
            let on_way = RunId::new(way.clone()).unwrap();
            for row in ledger::build(&events, 1_900_000_001_000, "2030-01-01T00:00:00.000Z") {
                if row.key != way {
                    let mut row = row;
                    if row.kind == LedgerKind::Run {
                        row.detail[ledger::steps::DETAIL_KEY] = json!({"outside": 0});
                    }
                    queue.write_ledger_row(&row).unwrap();
                }
            }
            assert!(!queue.ledger_rows(LedgerKind::Queue).unwrap().is_empty());
            let window = queue
                .ledger_window(&WindowQuery {
                    now_ms: 1_900_000_001_000,
                    ..WindowQuery::default()
                })
                .unwrap();
            assert_eq!(window.events(), events, "{runs} runs");
            assert!(window.rows.iter().all(|row| row.records.is_empty()));
            assert_eq!(window.of(LedgerKind::Queue).count(), 0);
            let mut keys: Vec<&str> = window
                .of(LedgerKind::Run)
                .map(|row| row.key.as_str())
                .collect();
            keys.sort_unstable();
            let mut expected: Vec<String> = (0..runs).map(run_id).collect();
            expected.push(on_way.to_string());
            assert_eq!(keys, expected, "{runs} runs");
            assert!(
                window
                    .of(LedgerKind::Run)
                    .all(|row| row.detail.get(ledger::steps::DETAIL_KEY).is_none())
            );
        }
    }

    /// R's time read reads back as written: the lowest too (no event read),
    /// and none as none, a mark of no such field. It stays beside the
    /// positions of its row alone: a reader of [`InputsThrough`] skips it,
    /// and a write that fails part way keeps the mark as it was.
    #[test]
    fn the_time_read_goes_with_r_in_its_row_alone() {
        use crate::domain::ledger::session::written::{
            Cursors, Marks, RowsWritten, SessionsWritten,
        };
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let cursors = Cursors {
            events: EventId::new(5),
            samples: 2,
            steps: 3,
            resources: 4,
            inside_resources: 6,
        };
        let marks_of = |rows: RowsWritten| Marks {
            sessions: SessionsWritten::at(EventId::new(7)),
            rows: Some(rows),
            omitted: None,
        };
        let held = marks_of(RowsWritten::at(cursors, 1_900_000_000_000));
        let batch: Vec<(LedgerRow, Option<LedgerRow>)> =
            sessions().into_iter().map(|row| (row, None)).collect();
        assert!(
            queue
                .write_session_rows_marked(&batch, Some(&held))
                .unwrap()
        );
        assert_eq!(queue.ledger_marks().unwrap(), Some(held.clone()));
        let text = |kind: &str| -> String {
            queue
                .conn
                .query_row(
                    "SELECT inputs_through FROM ledger_rows WHERE kind=?1 LIMIT 1",
                    [kind],
                    |r| r.get(0),
                )
                .unwrap()
        };
        let of_r = text(written::ROWS_KIND);
        assert!(of_r.contains("read_ms"), "{of_r}");
        let positions: InputsThrough = serde_json::from_str(&of_r).unwrap();
        assert_eq!(positions.events, cursors.events);
        assert_eq!(positions.inside_resources, Some(6));
        for kind in [written::KIND, "session"] {
            assert!(!text(kind).contains("read_ms"), "{kind}");
        }
        for kind in LedgerKind::ALL {
            let rows = queue.ledger_rows(kind).unwrap();
            assert!(rows.iter().all(|row| row.key != written::KEY), "{kind:?}");
        }
        let window = queue
            .ledger_window(&ledger::window::WindowQuery::default())
            .unwrap();
        assert!(!format!("{window:?}").contains("read_ms"));

        // A write of the rows and the marks that fails as it writes R.
        queue
            .conn
            .execute_batch(
                "CREATE TEMP TRIGGER part_way BEFORE INSERT ON ledger_rows
                   WHEN NEW.kind = 'rows_written'
                 BEGIN SELECT RAISE(ABORT, 'part way'); END;",
            )
            .unwrap();
        let later = marks_of(RowsWritten::at(cursors, 1_900_000_000_001));
        let over: Vec<(LedgerRow, Option<LedgerRow>)> = batch
            .iter()
            .map(|(row, _)| {
                let mut changed = row.clone();
                changed.built_at = "2031-01-01T00:00:00.000Z".into();
                (changed, Some(row.clone()))
            })
            .collect();
        let error = queue
            .write_session_rows_marked(&over, Some(&later))
            .unwrap_err();
        assert!(format!("{error:#}").contains("part way"), "{error:#}");
        assert_eq!(queue.ledger_marks().unwrap(), Some(held.clone()));
        assert!(
            queue
                .ledger_rows(LedgerKind::Session)
                .unwrap()
                .iter()
                .all(|row| !row.built_at.starts_with("2031"))
        );
        queue.conn.execute_batch("DROP TRIGGER part_way").unwrap();

        for rows in [
            RowsWritten::at(cursors, i64::MIN),
            RowsWritten {
                read_ms: None,
                ..RowsWritten::at(cursors, 0)
            },
        ] {
            let marks = marks_of(rows);
            queue.write_ledger_marks(&marks).unwrap();
            assert_eq!(queue.ledger_marks().unwrap(), Some(marks.clone()));
        }
        assert!(!text(written::ROWS_KIND).contains("read_ms"));
    }

    /// The statement the push records were read with before
    /// [`push_records_sql`]: an `EXISTS` over the row's records for each
    /// `push_failed` and `run_phase_changed` record.
    const PUSH_RECORDS_OLD_SQL: &str = "SELECT j.value FROM ledger_rows r, json_each(r.records) j
      WHERE r.kind=?1
        AND (json_extract(j.value, '$.kind')=?2
             OR (json_extract(j.value, '$.kind') IN (?3, ?4)
                 AND EXISTS (SELECT 1 FROM json_each(r.records) f
                              WHERE json_extract(f.value, '$.kind')=?3)))
      ORDER BY json_extract(j.value, '$.id')";

    /// A store of run rows of every mix of push records: `push_finished`
    /// alone, a `push_failed` with `run_phase_changed` and `push_finished`,
    /// `run_phase_changed` with no `push_failed`, and no push record, with
    /// a task row holding a `push_failed` under the key of the run row
    /// without one (its `push_failed` is no run row's); the ids of
    /// the records interleave across the rows.
    fn mixed_push_rows() -> (tempfile::TempDir, SqliteQueue, Vec<LedgerRow>) {
        use crate::domain::EventKind;
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let record = |id: i64, kind: EventKind| RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload: serde_json::json!({"phase": "ended", "reason": "r", "extra": id}),
            created_at: "2026-11-01T00:00:00Z".to_owned(),
            actor: None,
        };
        let row = |kind: LedgerKind, key: &str, records: Vec<(i64, EventKind)>| {
            let mut row = ledger::testkit::run_row(key, None, Vec::new());
            row.kind = kind;
            row.records = records.into_iter().map(|(id, k)| record(id, k)).collect();
            row
        };
        use EventKind::{PushFailed, PushFinished, RunClaimed, RunPhaseChanged};
        let rows = vec![
            row(
                LedgerKind::Run,
                "a",
                vec![(1, RunClaimed), (5, PushFinished), (9, RunPhaseChanged)],
            ),
            row(
                LedgerKind::Run,
                "b",
                vec![
                    (2, RunClaimed),
                    (6, RunPhaseChanged),
                    (10, PushFailed),
                    (13, RunPhaseChanged),
                    (14, PushFinished),
                ],
            ),
            row(
                LedgerKind::Run,
                "c",
                vec![(3, RunClaimed), (7, RunPhaseChanged), (11, RunPhaseChanged)],
            ),
            row(LedgerKind::Run, "d", vec![(4, RunClaimed), (8, RunClaimed)]),
            row(
                LedgerKind::Task,
                "c",
                vec![(12, PushFailed), (15, RunPhaseChanged)],
            ),
        ];
        for row in &rows {
            queue.write_ledger_row(row).unwrap();
        }
        (dir, queue, rows)
    }

    /// The push records of a store of every mix of them are what the rows
    /// read whole give by the port's rule, and the records, their cut and
    /// their order of the statement the store read them with before.
    #[test]
    fn the_push_records_of_every_mix_are_the_old_statements() {
        use crate::domain::EventKind;
        use crate::domain::ledger::builder::pushes::cut;
        let (_dir, queue, rows) = mixed_push_rows();
        let read = queue.ledger_push_records().unwrap();
        let ids: Vec<i64> = read.iter().map(|e| e.id.as_i64()).collect();
        assert_eq!(ids, [5, 6, 10, 13, 14]);
        let mut expected: Vec<RunEvent> = rows
            .iter()
            .filter(|row| row.kind == LedgerKind::Run)
            .flat_map(|row| {
                let failed = row.records.iter().any(|e| e.kind == EventKind::PushFailed);
                row.records.iter().filter(move |e| {
                    e.kind == EventKind::PushFinished
                        || (failed
                            && (e.kind == EventKind::PushFailed
                                || e.kind == EventKind::RunPhaseChanged))
                })
            })
            .map(cut)
            .collect();
        expected.sort_by_key(|e| e.id);
        assert_eq!(read, expected);
        let old: Vec<RunEvent> = queue
            .conn
            .prepare(PUSH_RECORDS_OLD_SQL)
            .unwrap()
            .query_map(
                params!["run", "push_finished", "push_failed", "run_phase_changed"],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
            .map(|text| cut(&serde_json::from_str(&text.unwrap()).unwrap()))
            .collect();
        assert_eq!(read, old);
    }

    /// Whether a run row holds a `push_failed` is decided once: on the
    /// schema `SqliteQueue::init` lays down, with no statistics, the plan of
    /// the push records has no subquery correlated to the record, where the
    /// old statement's has one.
    #[test]
    fn the_push_records_decide_a_failed_push_once_a_row() {
        let (_dir, queue, _) = mixed_push_rows();
        let plan = |sql: &str| -> String {
            queue
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(
                    params!["run", "push_finished", "push_failed", "run_phase_changed"],
                    |r| r.get::<_, String>(3),
                )
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>()
                .join("\n")
        };
        let new = plan(&push_records_sql(SqlDialect::Sqlite));
        assert!(!new.contains("CORRELATED"), "{new}");
        assert!(new.contains("LIST SUBQUERY"), "{new}");
        let old = plan(PUSH_RECORDS_OLD_SQL);
        assert!(old.contains("CORRELATED SCALAR SUBQUERY"), "{old}");
    }

    /// Each page of the push records of the run rows the store answers
    /// ([`LedgerStore::ledger_push_records_page`]) holds the records, cut
    /// and by id, of the rows it names alone, on rows of every mix
    /// ([`mixed_push_rows`]) and a row of a `push_failed` with nine
    /// records: a page of whole rows counts the rows without a push record
    /// and ends at its last row, so the pages of any number of rows after
    /// one another hold the whole read once; a part of one row holds at most
    /// the records asked after where it starts and ends cut after its last
    /// one only when the row goes on, and a row past the last is no row.
    #[test]
    fn the_pages_of_the_push_records_hold_the_records_of_their_rows() {
        use crate::domain::EventKind;
        use crate::domain::ledger::builder::pushes::page::{
            PushRecordsAt, PushRecordsPage, PushRecordsRead,
        };
        let (_dir, queue, _) = mixed_push_rows();
        let mut many = ledger::testkit::run_row("bb", None, Vec::new());
        many.records = (0..9)
            .map(|n| RunEvent {
                id: EventId::new(100 + n),
                task_id: None,
                goal_id: None,
                run_id: None,
                kind: if n == 4 {
                    EventKind::PushFailed
                } else {
                    EventKind::RunPhaseChanged
                }
                .into(),
                payload: serde_json::json!({"phase": "p", "extra": n}),
                created_at: "2026-11-01T00:00:00Z".to_owned(),
                actor: None,
            })
            .collect();
        queue.write_ledger_row(&many).unwrap();
        let whole = queue.ledger_push_records().unwrap();
        let held: [(&str, Vec<i64>); 5] = [
            ("a", vec![5]),
            ("b", vec![6, 10, 13, 14]),
            ("bb", (100..109).collect()),
            ("c", vec![]),
            ("d", vec![]),
        ];
        let records_of = |ids: &[i64]| -> Vec<RunEvent> {
            whole
                .iter()
                .filter(|record| ids.contains(&record.id.as_i64()))
                .cloned()
                .collect()
        };
        let all: Vec<i64> = held.iter().flat_map(|(_, ids)| ids.clone()).collect();
        assert_eq!(records_of(&all), whole);
        assert_eq!(whole.len(), 14);
        for rows in [1, 2, 3, 1_000] {
            let (mut after, mut from, mut read) = (None::<String>, 0, Vec::new());
            loop {
                let page = queue
                    .ledger_push_records_page(&PushRecordsRead::Rows {
                        after: after.clone(),
                        rows,
                    })
                    .unwrap();
                let named = &held[from..(from + rows).min(held.len())];
                let ids: Vec<i64> = named.iter().flat_map(|(_, ids)| ids.clone()).collect();
                assert_eq!(
                    page.records,
                    records_of(&ids),
                    "rows {rows} after {after:?}"
                );
                assert_eq!(page.rows, named.len(), "rows {rows} after {after:?}");
                assert_eq!(
                    page.at,
                    named.last().map(|(key, _)| PushRecordsAt {
                        key: (*key).to_owned(),
                        cut: None
                    })
                );
                read.extend(page.records);
                if page.rows < rows {
                    break;
                }
                (after, from) = (page.at.map(|at| at.key), from + rows);
            }
            read.sort_by_key(|record| record.id);
            assert_eq!(read, whole, "rows {rows}");
        }
        let part = |read: PushRecordsRead| queue.ledger_push_records_page(&read).unwrap();
        let at = |key: &str, cut: Option<i64>| {
            Some(PushRecordsAt {
                key: key.to_owned(),
                cut: cut.map(EventId::new),
            })
        };
        let page = |ids: &[i64], key: &str, cut: Option<i64>| PushRecordsPage {
            records: records_of(ids),
            rows: 1,
            at: at(key, cut),
        };
        let first = |after: Option<&str>, records| PushRecordsRead::FirstOfRow {
            after: after.map(str::to_owned),
            records,
        };
        let rest = |key: &str, after: i64, records| PushRecordsRead::RestOfRow {
            key: key.to_owned(),
            after: EventId::new(after),
            records,
        };
        assert_eq!(part(first(None, 1)), page(&[5], "a", None));
        assert_eq!(part(first(Some("a"), 4)), page(&[6, 10, 13, 14], "b", None));
        assert_eq!(
            part(first(Some("b"), 4)),
            page(&[100, 101, 102, 103], "bb", Some(103))
        );
        assert_eq!(
            part(rest("bb", 103, 4)),
            page(&[104, 105, 106, 107], "bb", Some(107))
        );
        assert_eq!(part(rest("bb", 107, 4)), page(&[108], "bb", None));
        assert_eq!(part(rest("b", 10, 1)), page(&[13], "b", Some(13)));
        assert_eq!(part(first(Some("bb"), 4)), page(&[], "c", None));
        assert_eq!(part(first(Some("d"), 4)), PushRecordsPage::default());
    }

    /// A page of the push records reads the run rows of its keys alone: on
    /// the schema `SqliteQueue::init` lays down, with no statistics, the
    /// rows read and those picked for a `push_failed` are searched by the
    /// primary key's range or key, and no statement of a page scans the
    /// rows.
    #[test]
    fn a_page_of_the_push_records_searches_its_rows_by_key() {
        let (_dir, queue, _) = mixed_push_rows();
        let plan = |sql: &str, rest: &[&dyn rusqlite::ToSql]| -> String {
            let mut params: Vec<&dyn rusqlite::ToSql> = vec![
                &"run",
                &"push_finished",
                &"push_failed",
                &"run_phase_changed",
            ];
            params.extend_from_slice(rest);
            queue
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(params.as_slice(), |r| r.get::<_, String>(3))
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>()
                .join("\n")
        };
        // Each of `r` and `g` searched by the key of the rows (the primary
        // key's index) on `condition`.
        let searched = |plan: &str, condition: &str| {
            for alias in ["r", "g"] {
                assert!(
                    plan.lines()
                        .any(|line| line.starts_with(&format!("SEARCH {alias} USING "))
                            && line.contains("ledger_rows")
                            && line.ends_with(condition)),
                    "{alias} {condition}: {plan}"
                );
            }
        };
        let range = plan(
            &push_records_range_sql(SqlDialect::Sqlite, true),
            &[&"a", &"c"],
        );
        searched(&range, "(kind=? AND key>? AND key<?)");
        let first = plan(&push_records_range_sql(SqlDialect::Sqlite, false), &[&"c"]);
        searched(&first, "(kind=? AND key<?)");
        let rest = plan(
            &push_records_in_row_sql(SqlDialect::Sqlite, true),
            &[&"b", &5, &3],
        );
        searched(&rest, "(kind=? AND key=?)");
        let row = plan(
            &push_records_in_row_sql(SqlDialect::Sqlite, false),
            &[&"b", &3],
        );
        searched(&row, "(kind=? AND key=?)");
        for sql in [&range, &first, &rest, &row] {
            assert!(!sql.contains("SCAN r") && !sql.contains("SCAN g"), "{sql}");
            assert!(!sql.contains("CORRELATED"), "{sql}");
        }
    }

    /// The push records, the heads of the rows not settled at a position
    /// and the runs a task names with no row read back what the rows read
    /// whole give by the port's rule: SQLite picks them out of the stored
    /// rows.
    #[test]
    fn the_push_records_and_open_heads_read_what_the_rows_whole_give() {
        use crate::domain::ledger::builder::pushes::{cut, is_push_record, settled_at};
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let (log, now) = ledger::testkit::pushes_across(2);
        let rows: Vec<LedgerRow> = ledger::build(&log.events, ledger::testkit::ms(now), "t")
            .into_iter()
            .map(|mut row| {
                row.is_final = row
                    .ended_at
                    .is_some_and(|end| end < ledger::testkit::ms(now - 900));
                row
            })
            .collect();
        for row in &rows {
            queue.write_ledger_row(row).unwrap();
        }
        let of = |kind: LedgerKind| rows.iter().filter(move |row| row.kind == kind);
        let mut records: Vec<RunEvent> = of(LedgerKind::Run)
            .flat_map(|row| {
                let failed = row
                    .records
                    .iter()
                    .any(|e| e.kind == crate::domain::EventKind::PushFailed);
                row.records.iter().filter(move |event| {
                    is_push_record(event)
                        && (failed || event.kind == crate::domain::EventKind::PushFinished)
                })
            })
            .map(cut)
            .collect();
        records.sort_by_key(|event| event.id);
        assert!(
            records
                .iter()
                .any(|e| e.kind == crate::domain::EventKind::PushFinished)
        );
        assert!(
            records
                .iter()
                .any(|e| e.kind == crate::domain::EventKind::RunPhaseChanged)
        );
        assert_eq!(queue.ledger_push_records().unwrap(), records);
        for at in [EventId::new(40), log.last()] {
            let open: Vec<LedgerHead> = [LedgerKind::Run, LedgerKind::Task]
                .into_iter()
                .flat_map(|kind| {
                    let mut heads: Vec<LedgerHead> = of(kind).map(LedgerHead::of).collect();
                    heads.sort_by(|a, b| a.key.cmp(&b.key));
                    heads
                })
                .filter(|head| !settled_at(head, false, at, 0))
                .collect();
            assert!(!open.is_empty());
            assert_eq!(queue.ledger_heads_open_at(at, 0).unwrap(), open);
        }
        let open = queue.ledger_heads_open_at(log.last(), 0).unwrap().len();
        assert!(open < of(LedgerKind::Run).count() + of(LedgerKind::Task).count());
        // A settled run row that folded a window after the cursor of the
        // windows is open at it, and not at that window.
        let settled = of(LedgerKind::Run)
            .find(|row| settled_at(&LedgerHead::of(row), false, log.last(), 0))
            .unwrap()
            .key
            .clone();
        queue
            .conn
            .execute(
                "UPDATE ledger_rows SET inputs_through=json_set(inputs_through, '$.resources', 5)
                  WHERE kind='run' AND key=?1",
                [&settled],
            )
            .unwrap();
        let opens = |cursor: i64| {
            queue
                .ledger_heads_open_at(log.last(), cursor)
                .unwrap()
                .iter()
                .any(|head| head.kind == LedgerKind::Run && head.key == settled)
        };
        assert!(opens(4) && !opens(5));
        // Rows of a run on its way are not written: take run 5's out.
        let on_its_way = crate::domain::RunId::new(ledger::testkit::run_id(5)).unwrap();
        queue
            .conn
            .execute(
                "DELETE FROM ledger_rows WHERE kind='run' AND key=?1",
                [on_its_way.as_str()],
            )
            .unwrap();
        assert_eq!(queue.ledger_runs_without_row().unwrap(), [on_its_way]);
    }

    /// Of the runs of a legacy task with no row, its row's values name
    /// those of its claims up to its first write, its claims name those
    /// since too; a run with a row is neither, and neither is a run of a
    /// task whose row is not legacy (its row names those it has).
    #[test]
    fn the_runs_of_a_legacy_task_with_no_row_are_read_of_its_values_and_its_claims() {
        use crate::domain::ledger::testkit::{Log, run_id};
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let (old, named, written, claimed, of_today) =
            (run_id(1), run_id(2), run_id(3), run_id(4), run_id(5));
        let claim = serde_json::json!({"provider": "claude"});
        let mut log = Log::default();
        log.legacy(1, &old, 0)
            .add(Some(1), Some(&named), "run_claimed", claim.clone(), 2000);
        let first = ledger::build(&log.events, ledger::testkit::ms(2000), "t");
        let rows =
            |of: &[LedgerRow], key: &str| of.iter().find(|row| row.key == key).unwrap().clone();
        queue.write_ledger_row(&rows(&first, "1")).unwrap();
        queue.write_ledger_row(&rows(&first, &old)).unwrap();
        log.add(Some(1), Some(&written), "run_claimed", claim.clone(), 3000)
            .add(Some(1), Some(&claimed), "run_claimed", claim.clone(), 3000)
            .add(Some(2), Some(&of_today), "run_claimed", claim, 3000);
        let later = ledger::build(&log.events, ledger::testkit::ms(3000), "t");
        for key in ["1", "2", written.as_str()] {
            queue.write_ledger_row(&rows(&later, key)).unwrap();
        }
        let task = queue.ledger_row(LedgerKind::Task, "1").unwrap().unwrap();
        assert_eq!(
            ledger::builder::pushes::task_runs(&task),
            [old.as_str(), named.as_str()]
        );
        queue
            .conn
            .pragma_update(None, "foreign_keys", false)
            .unwrap();
        for event in &log.events {
            queue
                .conn
                .execute(
                    "INSERT INTO run_events(id, task_id, run_id, kind, payload)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        event.id.as_i64(),
                        event.task_id.map(|task| task.as_i64()),
                        event.run_id.as_ref().map(|run| run.as_str()),
                        event.kind.as_str(),
                        event.payload.to_string()
                    ],
                )
                .unwrap();
        }
        let ids = |runs: &[&String]| -> Vec<RunId> {
            runs.iter().map(|run| RunId::new(*run).unwrap()).collect()
        };
        assert_eq!(
            queue.ledger_runs_without_row().unwrap(),
            ids(&[&named, &of_today])
        );
        assert_eq!(
            queue.ledger_legacy_runs_without_row().unwrap(),
            ids(&[&named, &claimed])
        );
        // The claims of a task name its runs, with a row or not.
        assert_eq!(
            queue.ledger_runs_claimed(TaskId::new(1)).unwrap(),
            ids(&[&old, &named, &written, &claimed])
        );
    }

    /// A run with an event up to a position after the inputs of its row is
    /// past its row, up to that position and not before it; one whose row
    /// holds its events, or with no row, is not.
    #[test]
    fn the_runs_past_their_rows_have_an_event_up_to_the_position_after_their_inputs() {
        use crate::domain::ledger::testkit::{Log, run_id};
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let (went_on, holds, no_row) = (run_id(1), run_id(2), run_id(3));
        let mut log = Log::default();
        log.landed(1, &went_on, 0).landed(2, &holds, 0);
        for row in ledger::build(&log.events, ledger::testkit::ms(2000), "t") {
            if row.kind == LedgerKind::Run {
                queue.write_ledger_row(&row).unwrap();
            }
        }
        let written = log.last();
        log.add(
            Some(3),
            Some(&no_row),
            "run_claimed",
            serde_json::json!({}),
            2000,
        )
        .add(
            Some(1),
            Some(&went_on),
            "note_added",
            serde_json::json!({}),
            2000,
        );
        queue
            .conn
            .pragma_update(None, "foreign_keys", false)
            .unwrap();
        for event in &log.events {
            queue
                .conn
                .execute(
                    "INSERT INTO run_events(id, task_id, run_id, kind, payload)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        event.id.as_i64(),
                        event.task_id.map(|task| task.as_i64()),
                        event.run_id.as_ref().map(|run| run.as_str()),
                        event.kind.as_str(),
                        event.payload.to_string()
                    ],
                )
                .unwrap();
        }
        let before = EventId::new(log.last().as_i64() - 1);
        assert!(before > written);
        assert!(
            queue
                .ledger_runs_past_their_rows(before)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            queue.ledger_runs_past_their_rows(log.last()).unwrap(),
            [RunId::new(&went_on).unwrap()]
        );
    }

    /// SQLite says what the rows whole say of the rows older than this
    /// version, a legacy one's version aside, and of the runs other rules
    /// classified: a run with no steps is of neither.
    #[test]
    fn the_older_rows_and_the_runs_of_other_rules_read_what_the_rows_whole_give() {
        use crate::domain::ledger::steps::DETAIL_KEY;
        use crate::domain::ledger::testkit::{Log, run_id};
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let (a, b, c, old) = (run_id(1), run_id(2), run_id(3), run_id(4));
        let mut log = Log::default();
        log.landed(1, &a, 0)
            .landed(2, &b, 100)
            .landed(3, &c, 200)
            .legacy(4, &old, 300);
        let rows = ledger::build(&log.events, ledger::testkit::ms(4000), "t");
        let classified = |rules: Option<&str>| serde_json::json!({DETAIL_KEY: {"fold": {"classes": {"rules": rules}}}});
        for mut row in rows {
            if row.key == a {
                row.detail = classified(Some("one"));
            } else if row.key == b {
                row.detail = classified(Some("two"));
            }
            queue.write_ledger_row(&row).unwrap();
        }
        let whole = |version: &str| {
            let mut runs = Vec::new();
            for row in queue.ledger_rows(LedgerKind::Run).unwrap() {
                if row.detail.get(DETAIL_KEY).is_some()
                    && ledger::steps::rules_of(&row.detail) != Some(version)
                {
                    runs.push(row.key);
                }
            }
            runs
        };
        for version in ["one", "two", "three"] {
            assert_eq!(
                queue.ledger_runs_classified_otherwise(version).unwrap(),
                whole(version),
                "{version}"
            );
        }
        assert_eq!(queue.ledger_runs_classified_otherwise("one").unwrap(), [b]);

        assert!(!queue.ledger_older_rows().unwrap());
        assert_eq!(queue.ledger_older_rows_after(None, 9).unwrap(), Vec::new());
        // A legacy row keeps the version it was first written at.
        queue
            .conn
            .execute(
                "UPDATE ledger_rows SET ledger_v=?1 WHERE legacy=1",
                params![ledger::LEDGER_V - 1],
            )
            .unwrap();
        assert!(!queue.ledger_older_rows().unwrap());
        assert_eq!(queue.ledger_older_rows_after(None, 9).unwrap(), Vec::new());
        // No marks' row is one.
        queue
            .write_ledger_marks(&written::Marks {
                sessions: SessionsWritten {
                    through: EventId::new(1),
                    ledger_v: ledger::LEDGER_V - 1,
                },
                rows: None,
                omitted: None,
            })
            .unwrap();
        assert_eq!(queue.ledger_older_rows_after(None, 9).unwrap(), Vec::new());
        assert!(!queue.ledger_older_rows().unwrap());
        queue
            .conn
            .execute(
                "UPDATE ledger_rows SET ledger_v=?1 WHERE key=?2",
                params![ledger::LEDGER_V - 1, a],
            )
            .unwrap();
        assert_eq!(
            queue.ledger_older_rows_after(None, 9).unwrap(),
            [(LedgerKind::Run, a.clone())]
        );
        assert!(queue.ledger_older_rows().unwrap());
        queue
            .conn
            .execute(
                "UPDATE ledger_rows SET ledger_v=?1 WHERE legacy=0 AND kind IN ('run', 'task')",
                params![ledger::LEDGER_V - 1],
            )
            .unwrap();
        // In pages, as the port's default gives them over the rows whole.
        let every = queue.ledger_older_rows_after(None, usize::MAX).unwrap();
        assert!(every.len() > 3, "{every:?}");
        assert!(every.iter().any(|(kind, _)| *kind == LedgerKind::Run));
        assert!(every.iter().any(|(kind, _)| *kind == LedgerKind::Task));
        let (mut paged, mut after) = (Vec::new(), None);
        loop {
            let page = queue.ledger_older_rows_after(after.as_ref(), 2).unwrap();
            if page.is_empty() {
                break;
            }
            after = page.last().cloned();
            paged.extend(page);
        }
        assert_eq!(paged, every);
    }

    /// The store keeps a legacy row's mark as first written, so its head is
    /// read as not settled at a position whatever that mark, at today's
    /// version too; a settled row of today's form is not.
    #[test]
    fn a_legacy_row_is_open_at_a_position_whatever_its_mark() {
        use crate::domain::ledger::testkit::{Log, ms, run_id};
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let (old, today) = (run_id(1), run_id(2));
        let mut log = Log::default();
        log.legacy(1, &old, 0).landed(2, &today, 0);
        let rows: Vec<LedgerRow> = ledger::build(&log.events, ms(100_000), "t")
            .into_iter()
            .filter(|row| matches!(row.kind, LedgerKind::Run | LedgerKind::Task))
            .map(|mut row| {
                row.is_final = true;
                row
            })
            .collect();
        for row in &rows {
            queue.write_ledger_row(row).unwrap();
        }
        let legacy: Vec<LedgerHead> = [LedgerKind::Run, LedgerKind::Task]
            .into_iter()
            .flat_map(|kind| rows.iter().filter(move |row| row.kind == kind))
            .filter(|row| row.is_legacy() && row.ledger_v == ledger::LEDGER_V)
            .map(LedgerHead::of)
            .collect();
        assert_eq!(
            legacy
                .iter()
                .map(|head| head.key.as_str())
                .collect::<Vec<_>>(),
            [old.as_str(), "1"]
        );
        assert_eq!(queue.ledger_heads_open_at(log.last(), 0).unwrap(), legacy);
    }

    /// The pushes of no run SQLite picks out of the task rows and the queue
    /// rows of a push alone are those the rows' records whole give.
    #[test]
    fn the_runless_pushes_read_what_the_rows_whole_give() {
        use crate::domain::ledger::builder::pushes::cut;
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let (mut log, now) = ledger::testkit::pushes_across(1);
        let pushed = serde_json::json!({"remote": "origin", "branch": "late", "commit": "c"});
        log.add(Some(2), None, "push_finished", pushed.clone(), now)
            .add(None, None, "push_finished", pushed, now + 1);
        let rows = ledger::build(&log.events, ledger::testkit::ms(now + 2), "t");
        for row in &rows {
            queue.write_ledger_row(row).unwrap();
        }
        let mut expected: Vec<RunEvent> = rows
            .iter()
            .filter(|row| matches!(row.kind, LedgerKind::Task | LedgerKind::Queue))
            .flat_map(|row| row.records.iter())
            .filter(|event| {
                event.kind == crate::domain::EventKind::PushFinished && event.run_id.is_none()
            })
            .map(cut)
            .collect();
        expected.sort_by_key(|event| event.id);
        assert_eq!(expected.len(), 2);
        assert_eq!(queue.ledger_runless_pushes().unwrap(), expected);
    }
}
