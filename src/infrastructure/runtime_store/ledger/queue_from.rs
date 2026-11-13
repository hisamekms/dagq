//! The queue's rows a read from a time reads kind by kind
//! ([`crate::domain::ledger::window::queue_from`]), in the SQL SQLite and
//! Postgres share: the kinds read whole by the primary key's pages, and
//! those read by their range by `ledger_rows_by_subject_end` (the rows not
//! ended, ended at the time or after it, and the last end before it),
//! `ledger_rows_started_after_end` (those ended before it whose start was
//! recorded at it or after it, or with no time) and
//! `ledger_rows_by_inputs_through` (those whose inputs pass
//! `queue_before`), so that what the latter two walk is the range and what
//! was folded past it, not the settled rows of the kind.
use super::*;
use crate::domain::ledger::window::queue_from::by_range;

/// The kinds read by their range, with the views trusted (`views`) or not,
/// as a SQL list of literals.
fn by_range_list(views: bool) -> String {
    by_range(views)
        .iter()
        .map(|kind| format!("'{kind}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A page of the queue rows (`?1` the kind) of the subject kinds read whole,
/// past the key `?2` with `after`, at most the last parameter, by key.
#[cfg(test)]
pub(super) fn whole_sql(after: bool) -> String {
    whole_sql_with(after, false)
}

/// [`whole_sql`] with the views trusted (`views`, the kinds of
/// [`crate::domain::ledger::window::queue_from::BY_VIEWS`] read by their
/// range too) or not.
pub(super) fn whole_sql_with(after: bool, views: bool) -> String {
    let (key, limit) = if after {
        ("AND key > ?2 ", "?3")
    } else {
        ("", "?2")
    };
    format!(
        "SELECT * FROM ledger_rows WHERE kind=?1 {key}AND COALESCE(subject_kind, '') NOT IN ({kinds})
          ORDER BY key LIMIT {limit}",
        kinds = by_range_list(views),
    )
}

/// The queue rows (`?1`) of the subject kind `?2` that reach the time `?3`
/// (unix ms): not ended, ended at it or after it, each of the last end
/// before it, and those ended before it whose start was recorded at it or
/// after it or with no time (a clock ahead and back), by key (a row of the
/// last end may come twice; the reader takes it once). Each part is a
/// seek of `ledger_rows_by_subject_end` or, the last,
/// `ledger_rows_started_after_end`, which holds only rows started after
/// their end; SQLite's planner is told (with no statistics it may take
/// another).
pub(super) fn reaching_sql(dialect: SqlDialect) -> String {
    let index = dialect.indexed_by("ledger_rows_by_subject_end");
    format!(
        "SELECT * FROM (
          SELECT * FROM ledger_rows{index} WHERE kind=?1 AND subject_kind=?2 AND ended_at IS NULL
          UNION ALL
          SELECT * FROM ledger_rows{index} WHERE kind=?1 AND subject_kind=?2 AND ended_at >= ?3
          UNION ALL
          SELECT * FROM ledger_rows{index} WHERE kind=?1 AND subject_kind=?2 AND ended_at = (
            SELECT MAX(ended_at) FROM ledger_rows{index}
             WHERE kind=?1 AND subject_kind=?2 AND ended_at < ?3)
          UNION ALL
          SELECT * FROM ledger_rows{skewed} WHERE kind=?1 AND subject_kind=?2
             AND ended_at IS NOT NULL AND (started_at IS NULL OR started_at > ended_at)
             AND ended_at < ?3 AND (started_at IS NULL OR started_at >= ?3)
        ) r ORDER BY key",
        skewed = dialect.indexed_by("ledger_rows_started_after_end"),
    )
}

/// The queue rows (`?1`) whose inputs pass the event `?2`, of the subject
/// kind `?3` and ended before the time `?4`, by key: a seek of
/// `ledger_rows_by_inputs_through` past `queue_before`, which SQLite's
/// planner is told (`ledger_rows_by_subject_end` would walk every row of
/// the kind ended before the time).
pub(super) fn past_sql(dialect: SqlDialect) -> String {
    format!(
        "SELECT * FROM ledger_rows{index} WHERE kind=?1 AND {through} > ?2
          AND subject_kind=?3 AND ended_at < ?4 ORDER BY key",
        index = dialect.indexed_by("ledger_rows_by_inputs_through"),
        through = dialect.json_field("inputs_through", "$.events", JsonType::Integer),
    )
}

/// [`crate::application::LedgerWindowRead::queue_rows_whole`] of `tx`, or
/// with the views trusted (`views`)
/// [`crate::application::LedgerWindowRead::queue_rows_whole_by_views`].
pub(super) fn whole_in(
    tx: &impl SqlRead,
    after: Option<&str>,
    limit: usize,
    views: bool,
) -> Result<Vec<LedgerRow>> {
    let queue = LedgerKind::Queue.as_str();
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let statement = whole_sql_with(after.is_some(), views);
    let rows = match after {
        Some(after) => {
            SqlRead::query_map(tx, &statement, sql_params![queue, after, limit], ledger_row)?
        }
        None => SqlRead::query_map(tx, &statement, sql_params![queue, limit], ledger_row)?,
    };
    rows.into_iter().collect()
}

/// [`crate::application::LedgerWindowRead::queue_rows_reaching`] of `tx`.
pub(super) fn reaching_in(tx: &impl SqlRead, kind: &str, from_ms: i64) -> Result<Vec<LedgerRow>> {
    let rows = SqlRead::query_map(
        tx,
        &reaching_sql(SqlRead::dialect(tx)),
        sql_params![LedgerKind::Queue.as_str(), kind, from_ms],
        ledger_row,
    )?;
    rows.into_iter().collect()
}

/// [`crate::application::LedgerWindowRead::queue_rows_past`] of `tx`.
pub(super) fn past_in(
    tx: &impl SqlRead,
    kind: &str,
    from_ms: i64,
    queue_before: EventId,
) -> Result<Vec<LedgerRow>> {
    let rows = SqlRead::query_map(
        tx,
        &past_sql(SqlRead::dialect(tx)),
        sql_params![LedgerKind::Queue.as_str(), queue_before, kind, from_ms],
        ledger_row,
    )?;
    rows.into_iter().collect()
}
