//! The queue's rows of the kinds a read from a time reads by their range,
//! of SQLite: each read seeks its index and walks no table whole, and with
//! 30 and 3000 settled units before the range the read answers as every
//! row and returns as many rows. The same contract runs on Postgres with
//! 30 and 300 ([`crate::infrastructure::store_contract::ledger_range`]).
use super::queue_from::{past_sql, reaching_sql, whole_sql};
use super::*;

/// The plan of `sql` (its parameters NULL), one line per step.
fn plan(conn: &rusqlite::Connection, sql: &str) -> Vec<String> {
    let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let nulls = vec![rusqlite::types::Null; statement.parameter_count()];
    statement
        .query_map(rusqlite::params_from_iter(nulls), |r| r.get("detail"))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// A store of a memdb of its own.
fn queue() -> SqliteQueue {
    let uri = format!("file:/ledger-queue-from-{}?vfs=memdb", uuid::Uuid::new_v4());
    SqliteQueue::init(uri).unwrap()
}

/// Each read of a kind read by its range seeks `index` with `bound` on
/// every step of `ledger_rows`, and no step scans the table.
fn assert_seeks(sql: &str, index: &str, bound: &str) {
    assert_seeks_any(sql, &[index], bound);
}

/// [`assert_seeks`] with any of `indexes` on each step.
fn assert_seeks_any(sql: &str, indexes: &[&str], bound: &str) {
    let queue = queue();
    let steps = plan(&queue.conn, sql);
    let of_rows: Vec<&String> = steps
        .iter()
        .filter(|step| step.contains("ledger_rows"))
        .collect();
    assert!(!of_rows.is_empty(), "{sql}: {steps:?}");
    for step in of_rows {
        assert!(
            step.starts_with("SEARCH ")
                && indexes.iter().any(|index| step.contains(index))
                && step.contains(bound),
            "{sql}: {steps:?}"
        );
    }
    assert!(
        !steps
            .iter()
            .any(|step| step.starts_with("SCAN ledger_rows")),
        "{sql}: {steps:?}"
    );
}

/// The rows that reach the time and the last end before it seek
/// `ledger_rows_by_subject_end` by the kind and its end; those past
/// `queue_before` seek `ledger_rows_by_inputs_through` past it.
#[test]
fn the_reads_by_range_seek_their_indexes_and_scan_no_table() {
    let dialect = SqlDialect::Sqlite;
    assert_seeks_any(
        &reaching_sql(dialect),
        &[
            "ledger_rows_by_subject_end",
            "ledger_rows_started_after_end",
        ],
        "kind=? AND subject_kind=?",
    );
    let steps = plan(&queue().conn, &reaching_sql(dialect));
    assert!(
        steps
            .iter()
            .any(|step| step.contains("ledger_rows_started_after_end")),
        "{steps:?}"
    );
    for bound in ["ended_at=?", "ended_at>?", "ended_at<?"] {
        assert!(
            steps.iter().any(|step| step.contains(bound)),
            "{bound}: {steps:?}"
        );
    }
    assert_seeks(
        &past_sql(dialect),
        "ledger_rows_by_inputs_through",
        "kind=? AND <expr>>?",
    );
    // The kinds read whole walk the primary key by pages, as before.
    for after in [false, true] {
        let steps = plan(&queue().conn, &whole_sql(after));
        assert!(
            steps
                .iter()
                .all(|step| !step.starts_with("SCAN ledger_rows")),
            "{steps:?}"
        );
    }
}

/// The store contract of the range read with 3000 settled units: the
/// answers are those of every row and what the store returns of the run,
/// task, session and node rows, the earlier records and the queue's rows
/// of the kinds read by their range is as many as with 30.
#[test]
fn the_range_read_returns_as_many_with_3000_settled_units() {
    let uri = format!("file:/ledger-range-{}?vfs=memdb", uuid::Uuid::new_v4());
    let _keeper = SqliteQueue::init(&uri).unwrap();
    crate::infrastructure::store_contract::ledger_range::ledger_range_with(
        &|| SqliteQueue::open(&uri),
        30,
        3000,
    )
    .unwrap();
}
