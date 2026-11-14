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

#[test]
fn the_named_queue_answers_and_counts_with_3000_settled_units() {
    let uri = format!(
        "file:/ledger-named-queue-{}?vfs=memdb",
        uuid::Uuid::new_v4()
    );
    let _keeper = SqliteQueue::init(&uri).unwrap();
    crate::infrastructure::store_contract::ledger_queue_lives::ledger_queue_lives_with(
        &|| SqliteQueue::open(&uri),
        3000,
    )
    .unwrap();
}

#[test]
fn the_named_queue_falls_back_past_an_old_writer_then_seeks_after_refilling() {
    let uri = format!("file:/ledger-named-old-{}?vfs=memdb", uuid::Uuid::new_v4());
    let _keeper = SqliteQueue::init(&uri).unwrap();
    crate::infrastructure::store_contract::ledger_queue_lives_past_an_old_writer(&|| {
        SqliteQueue::open(&uri)
    })
    .unwrap();
}

#[test]
fn the_named_queue_seeks_use_their_indexes_and_the_whole_read_skips_excluded_kinds() {
    use super::queue_lives::{
        ANSWERS_SQL, OPEN_SQL, TASK_SNAPSHOTS_SQL, ask_sql, goal_snapshots_sql, last_sql,
        legacy_null_sql, retractions_sql,
    };
    let queue = queue();
    for (sql, index) in [
        (
            last_sql(SqlDialect::Sqlite, true),
            "events_by_kind_supervisor",
        ),
        (last_sql(SqlDialect::Sqlite, false), "events_by_kind"),
        (
            retractions_sql(SqlDialect::Sqlite),
            "events_by_retracted_mark",
        ),
        (ask_sql(SqlDialect::Sqlite, false), "asks_by_id"),
        (ask_sql(SqlDialect::Sqlite, true), "asks_by_legacy_id"),
        (legacy_null_sql(SqlDialect::Sqlite), "asks_by_legacy_id"),
        (OPEN_SQL.to_owned(), "sqlite_autoindex_ledger_open_1"),
        (
            super::queue_lives::explicit_null_sql(SqlDialect::Sqlite),
            "asks_by_explicit_null",
        ),
        (ANSWERS_SQL.to_owned(), "answered_asks_by_ask"),
        (TASK_SNAPSHOTS_SQL.to_owned(), "snapshot_targets_by_task"),
        (goal_snapshots_sql(SqlDialect::Sqlite), "events_by_kind"),
    ] {
        let steps = plan(&queue.conn, &sql);
        assert!(
            steps
                .iter()
                .any(|step| step.starts_with("SEARCH") && step.contains(index)),
            "{sql}: {steps:?}"
        );
        assert!(
            !steps.iter().any(|step| step.starts_with("SCAN ")),
            "{sql}: {steps:?}"
        );
    }
    for after in [false, true] {
        let sql = super::queue_from::whole_sql_for(SqlDialect::Sqlite, after, false, true);
        let steps = plan(&queue.conn, &sql);
        for step in steps
            .iter()
            .filter(|step| step.contains("SEARCH ledger_rows"))
        {
            assert!(step.contains("subject_kind"), "{steps:?}");
        }
        assert!(
            !steps
                .iter()
                .any(|step| step.starts_with("SCAN ledger_rows")),
            "{steps:?}"
        );
    }
}

#[test]
fn the_named_queue_shared_contract_answers_and_falls_back() {
    let uri = format!(
        "file:/ledger-named-contract-{}?vfs=memdb",
        uuid::Uuid::new_v4()
    );
    let _keeper = SqliteQueue::init(&uri).unwrap();
    crate::infrastructure::store_contract::ledger_queue_lives_answers_as_every_row(&|| {
        SqliteQueue::open(&uri)
    })
    .unwrap();
}

#[test]
fn the_named_queue_keeps_the_marks_of_an_old_snapshot() {
    let uri = format!(
        "file:/ledger-named-old-snapshot-{}?vfs=memdb",
        uuid::Uuid::new_v4()
    );
    let _keeper = SqliteQueue::init(&uri).unwrap();
    crate::infrastructure::store_contract::ledger_queue_lives_keeps_an_old_snapshot(&|| {
        SqliteQueue::open(&uri)
    })
    .unwrap();
}

#[test]
fn the_last_supervisor_seek_includes_the_largest_event_id() {
    let queue = queue();
    let tx = queue.write_entry("last_supervisor_at_max_id").unwrap();
    SqlWrite::execute(
        &tx,
        "INSERT INTO run_events(id,kind,payload,created_at) VALUES (?1,'supervisor_started',?2,?3)",
        sql_params![
            i64::MAX,
            r#"{"supervisor":"edge","parallel":2}"#,
            "2030-01-01T00:00:00.000Z"
        ],
    )
    .unwrap();
    let id = SqlRead::query_row(
        &tx,
        &super::queue_lives::last_sql(SqlDialect::Sqlite, true),
        sql_params![
            "supervisor_started",
            "edge",
            "2030-01-02T00:00:00.000Z",
            i64::MAX
        ],
        |r| r.get::<_, i64>("id"),
    )
    .unwrap();
    assert_eq!(id, i64::MAX);
}
