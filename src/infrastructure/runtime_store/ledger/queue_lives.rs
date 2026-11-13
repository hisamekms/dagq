//! Shared SQL seeks for the queue's named inputs. Domain code collects names;
//! this module only queries them within the ledger read's transaction.
use super::*;
use crate::domain::ledger::window::queue_lives::{LIFE_KINDS, Names};

pub(super) fn ready(tx: &impl SqlRead) -> Result<bool> {
    for table in [
        "ledger_queue_views_filled",
        "ledger_snapshot_targets",
        "ledger_open",
        "ledger_open_filled",
        "ledger_answered_asks",
    ] {
        if !earlier::has_table(tx, table)? {
            return Ok(false);
        }
    }
    // A schema predating the legacy-key seeks remains readable through
    // promise 6; do not force a new index on an older read-only store.
    for index in ["asks_by_legacy_id", "asks_by_explicit_null"] {
        let exists = !SqlRead::query_map(
            tx,
            tx.dialect().index_exists_sql(),
            sql_params![index],
            |r| r.get::<_, i64>(0),
        )?
        .is_empty();
        if !exists {
            return Ok(false);
        }
    }
    Ok(held_view(tx, View::Queue)?.is_done()
        && held_filled(tx)?.is_done()
        && visibility::visible_in(tx, &[])?.is_some())
}

fn query(tx: &impl SqlRead, sql: &str, params: &[SqlValue]) -> Result<Vec<RunEvent>> {
    Ok(SqlRead::query_map(tx, sql, params, event_row)?)
}

fn visible(tx: &impl SqlRead, events: &[RunEvent]) -> Result<Vec<RunEvent>> {
    visibility::visible_in(tx, events)?.context("queue visibility changed inside its snapshot")
}

/// Select the last visible event before S, walking by id. Invisible events
/// must not turn an existing earlier input into an absent one.
fn last(tx: &impl SqlRead, kind: &str, token: Option<&str>, from_ms: i64) -> Result<Vec<RunEvent>> {
    let dialect = tx.dialect();
    let mut params = if let Some(token) = token {
        vec![
            kind.to_sql_value(),
            token.to_sql_value(),
            crate::domain::marks::utc_text(from_ms).to_sql_value(),
        ]
    } else {
        vec![
            kind.to_sql_value(),
            crate::domain::marks::utc_text(from_ms).to_sql_value(),
        ]
    };
    let mut below = i64::MAX;
    loop {
        params.push(below.to_sql_value());
        let events = query(tx, &last_sql(dialect, token.is_some()), &params)?;
        params.pop();
        let Some(event) = events.first() else {
            return Ok(Vec::new());
        };
        let next = event.id.as_i64().checked_sub(1);
        let taken = visible(tx, &events)?;
        if !taken.is_empty() {
            return Ok(taken);
        }
        let Some(next) = next else {
            return Ok(Vec::new());
        };
        below = next;
    }
}

pub(super) fn records(
    tx: &impl SqlRead,
    from: &RowsFrom,
    names: &Names,
    end_ms: i64,
) -> Result<Option<Vec<RunEvent>>> {
    let mut events = match small::mark_events(tx, from.ended_before_ms, end_ms)? {
        crate::application::WindowRead::Complete(events) => events,
        _ => return Ok(None),
    };
    let started = last(tx, "supervisor_started", None, from.ended_before_ms)?;
    let tokens = names.supervisors_with(&started);
    events.extend(started);
    for token in tokens {
        for kind in LIFE_KINDS {
            events.extend(last(tx, kind, Some(&token), from.ended_before_ms)?);
        }
    }
    for ask in &names.asks {
        for value in ask_values(tx.dialect(), ask.as_deref()) {
            let null = match &value {
                SqlValue::Null => true,
                SqlValue::Text(text) if tx.dialect() == SqlDialect::Postgres => text == "null",
                _ => false,
            };
            let statement = if null {
                explicit_null_sql(tx.dialect())
            } else {
                ask_sql(tx.dialect(), false)
            };
            let candidates = query(tx, &statement, std::slice::from_ref(&value))?;
            events.extend(
                visible(tx, &candidates)?
                    .into_iter()
                    .filter(|e| crate::domain::ledger::window::queue_lives::ask_matches(e, ask)),
            );
            // The fallback id is part of AskRef's legacy key contract. Its
            // null ask_id partition is sought, rather than all openings.
            let statement = if null {
                legacy_null_sql(tx.dialect())
            } else {
                ask_sql(tx.dialect(), true)
            };
            let candidates = query(tx, &statement, &[value])?;
            events.extend(
                visible(tx, &candidates)?
                    .into_iter()
                    .filter(|e| crate::domain::ledger::window::queue_lives::ask_matches(e, ask)),
            );
        }
        let what = Left::Ask(ask.clone()).text();
        let rows = SqlRead::query_map(tx, ANSWERS_SQL, sql_params![what], ledger_row)?
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        events.extend(rows.into_iter().flat_map(|row| row.records));
    }
    let open = SqlRead::query_map(tx, OPEN_SQL, sql_params![], ledger_row)?
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
    events.extend(open.into_iter().flat_map(|row| row.records));
    let mut snapshots = Vec::new();
    for task in &names.tasks {
        snapshots.extend(query(tx,
            "SELECT e.* FROM ledger_snapshot_targets t CROSS JOIN run_events e WHERE e.id=t.snapshot_event_id AND t.task_id=?1 ORDER BY e.id", sql_params![task])?);
    }
    let snapshots = visible(tx, &snapshots)?;
    // Spike 4670 (vii), decision A and 4756's does_not_hold: this
    // second stage grows with the target's open period, even for an old
    // snapshot. It must neither scan unrelated snapshots nor shrink S.
    if let crate::application::WindowRead::Complete(marks) =
        small::forecast_of(tx, &snapshots, end_ms)?
    {
        events.extend(marks);
    } else {
        return Ok(None);
    }
    events.extend(snapshots);
    let marks = names.marks_with(&events);
    let sql = retractions_sql(tx.dialect());
    for mark in marks {
        events.extend(visible(tx, &query(tx, &sql, sql_params![mark])?)?);
    }
    events.sort_by_key(|e| e.id);
    events.dedup_by_key(|e| e.id);
    Ok(Some(events))
}

/// Open queue rows are sought by the open view's kind/key prefix.
pub(super) const OPEN_SQL: &str = "SELECT r.* FROM ledger_open o CROSS JOIN ledger_rows r WHERE r.kind=o.kind AND r.key=o.key AND o.kind='queue' ORDER BY r.key";

/// The answers are indexed by the same canonical AskRef key as ledger_open.
pub(super) const ANSWERS_SQL: &str = "SELECT r.* FROM ledger_answered_asks a CROSS JOIN ledger_rows r WHERE r.kind='queue' AND r.key=a.key AND a.what=?1 ORDER BY r.key";

/// The ask_id expression index (the null partition also holds legacy ids).
pub(super) fn ask_sql(dialect: SqlDialect, legacy: bool) -> String {
    let ask = dialect.ledger_index_key("payload", "ask_id");
    let key = if legacy {
        dialect.ledger_index_key("payload", "id")
    } else {
        ask.clone()
    };
    let absent = if legacy {
        format!("{ask} IS NULL AND ")
    } else {
        String::new()
    };
    let parameter = dialect.ask_key_parameter();
    format!(
        "SELECT * FROM run_events{} WHERE kind='ask_opened' AND task_id IS NULL AND run_id IS NULL AND {absent}{key} = {parameter} ORDER BY id",
        dialect.indexed_by(if legacy {
            "asks_by_legacy_id"
        } else {
            "asks_by_id"
        })
    )
}

/// Bind both spellings whose AskRef key is the same: string and JSON value.
fn ask_values(dialect: SqlDialect, key: Option<&str>) -> Vec<SqlValue> {
    let mut values = vec![key.map_or(serde_json::Value::Null, |key| {
        serde_json::Value::String(key.to_owned())
    })];
    if let Some(key) = key
        && let Ok(value) = serde_json::from_str(key)
    {
        values.push(value);
    }
    values
        .into_iter()
        .map(|value| match dialect {
            SqlDialect::Postgres => SqlValue::Text(value.to_string()),
            SqlDialect::Sqlite => match value {
                serde_json::Value::Null => SqlValue::Null,
                serde_json::Value::Bool(v) => SqlValue::Bool(v),
                serde_json::Value::Number(v) => v
                    .as_i64()
                    .map(SqlValue::Integer)
                    .unwrap_or_else(|| SqlValue::Real(v.as_f64().unwrap_or(0.0))),
                serde_json::Value::String(v) => SqlValue::Text(v),
                v => SqlValue::Text(v.to_string()),
            },
        })
        .collect()
}

/// The last visible event is selected by walking this seek in descending id.
pub(super) fn last_sql(dialect: SqlDialect, named: bool) -> String {
    if named {
        let field = dialect.ledger_index_key("payload", "supervisor");
        format!(
            "SELECT * FROM run_events{} WHERE kind=?1 AND {field} IS NOT NULL AND {field}=?2 AND created_at < ?3 AND id <= ?4 ORDER BY id DESC LIMIT 1",
            dialect.indexed_by("events_by_kind_supervisor")
        )
    } else {
        format!(
            "SELECT * FROM run_events{} WHERE kind=?1 AND created_at < ?2 AND id <= ?3 ORDER BY id DESC LIMIT 1",
            dialect.indexed_by("events_by_kind")
        )
    }
}

pub(super) fn retractions_sql(dialect: SqlDialect) -> String {
    let mark = dialect.json_field("payload", "$.mark", JsonType::Integer);
    format!(
        "SELECT * FROM run_events{} WHERE kind='mark_retracted' AND {mark}=?1 ORDER BY id",
        dialect.indexed_by("events_by_retracted_mark")
    )
}

pub(super) fn explicit_null_sql(dialect: SqlDialect) -> String {
    let condition = format!(
        "{} AND {}",
        dialect.ask_explicit_null(),
        dialect.ask_null_parameter()
    );
    format!(
        "SELECT * FROM run_events{} WHERE kind='ask_opened' AND task_id IS NULL AND run_id IS NULL AND {condition} AND id >= -9223372036854775808 ORDER BY id",
        dialect.indexed_by("asks_by_explicit_null")
    )
}
pub(super) fn legacy_null_sql(dialect: SqlDialect) -> String {
    let ask = dialect.ledger_index_key("payload", "ask_id");
    let id = dialect.ledger_index_key("payload", "id");
    let parameter = dialect.ask_null_parameter();
    format!(
        "SELECT * FROM run_events{} WHERE kind='ask_opened' AND task_id IS NULL AND run_id IS NULL AND {ask} IS NULL AND {id} IS NULL AND {parameter} ORDER BY id",
        dialect.indexed_by("asks_by_legacy_id")
    )
}
