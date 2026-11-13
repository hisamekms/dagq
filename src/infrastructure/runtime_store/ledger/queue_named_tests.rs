//! The reads of the queue rows by the views on SQLite
//! ([`super::queue_named`]): with 3000 settled units the answers are those
//! of every row and what the store returns is as many as with 30 (the
//! store contract's case, of more units), and each seek searches an index
//! and scans no table, on the schema `SqliteQueue::init` leaves and with no
//! `sqlite_stat1`. The Postgres migration makes the same indexes.

use super::queue_named::{
    COMMITS_BY_REVERTS_SQL, COMMITS_BY_SHA_SQL, SUMMARY_SQL, after_summary_sql, ci_boundary_sql,
    closes_up_sql, finding_row_sql, goal_down_sql, links_sql, marks_down_sql, marks_up_sql,
    reds_by_sha_sql, submissions_down_sql,
};
use super::*;

const POSTGRES: &str =
    include_str!("../../../../migrations-postgres/ledger_queue_named_indexes.sql");
const SQLITE: &str = include_str!("../../../../migrations/0119_ledger_queue_named_indexes.sql");

/// The steps of the plan of `sql` that read a table or an index.
fn reads(conn: &rusqlite::Connection, sql: &str) -> Vec<String> {
    let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let nulls = vec![rusqlite::types::Null; statement.parameter_count()];
    statement
        .query_map(rusqlite::params_from_iter(nulls), |r| {
            r.get::<_, String>("detail")
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .into_iter()
        .filter(|step| step.starts_with("SEARCH") || step.starts_with("SCAN"))
        .collect()
}

/// The store contract of the reads by the views with 3000 settled units.
#[test]
fn the_reads_by_the_views_return_as_many_with_3000_settled_units() {
    let uri = format!(
        "file:/ledger-queue-named-{}?vfs=memdb",
        uuid::Uuid::new_v4()
    );
    let _keeper = SqliteQueue::init(&uri).unwrap();
    crate::infrastructure::store_contract::ledger_queue_named::ledger_queue_named_with(
        &|| SqliteQueue::open(&uri),
        30,
        3000,
    )
    .unwrap();
}

/// The clock anomaly precedes every settled unit, so summary backtracking
/// and its unsummarized rows cannot hide outside the measured range.
#[test]
fn an_early_clock_set_back_keeps_the_reads_bounded_with_3000_settled_units() {
    let uri = format!(
        "file:/ledger-queue-clock-{}?vfs=memdb",
        uuid::Uuid::new_v4()
    );
    let _keeper = SqliteQueue::init(&uri).unwrap();
    crate::infrastructure::store_contract::ledger_queue_named::ledger_queue_named_clock_with(
        &|| SqliteQueue::open(&uri),
        30,
        3000,
    )
    .unwrap();
}

/// No later main event may hide the clock regression made by CI alone.
#[test]
fn only_ci_after_a_future_main_event_keeps_the_reads_bounded_with_3000_checks() {
    let uri = format!(
        "file:/ledger-queue-ci-only-{}?vfs=memdb",
        uuid::Uuid::new_v4()
    );
    let _keeper = SqliteQueue::init(&uri).unwrap();
    crate::infrastructure::store_contract::ledger_queue_named::ledger_queue_named_ci_only_with(
        &|| SqliteQueue::open(&uri),
        30,
        3000,
    )
    .unwrap();
}

/// A mark done and kept through the generation now, over summaries an
/// older binary wrote (no independent CI bound), is filled again through the store's
/// filling ([`crate::application::LedgerStore::fill_ledger_queue_views`]):
/// the views go from untrusted back to trusted.
#[test]
fn the_store_s_filling_makes_again_the_views_over_older_summaries() {
    use crate::application::LedgerStore;
    use crate::infrastructure::store_contract::ledger_queue_named::{
        lay_down_named, named_history,
    };
    let uri = format!(
        "file:/ledger-queue-named-older-{}?vfs=memdb",
        uuid::Uuid::new_v4()
    );
    let queue = SqliteQueue::init(&uri).unwrap();
    let tx = queue.write_entry("older_summaries").unwrap();
    lay_down_named(&tx, &named_history(30)).unwrap();
    assert!(crate::infrastructure::runtime_store::ledger_views_in::usable(&tx).unwrap());
    SqlWrite::execute(
        &tx,
        "UPDATE ledger_main_summaries SET summary = json_remove(summary, '$.record.main_by_time')",
        sql_params![],
    )
    .unwrap();
    assert!(
        crate::infrastructure::runtime_store::ledger_views_in::held(&tx)
            .unwrap()
            .is_done()
    );
    assert!(!crate::infrastructure::runtime_store::ledger_views_in::usable(&tx).unwrap());
    tx.commit().unwrap();
    let mut passes = 0;
    while !queue.fill_ledger_queue_views().unwrap().is_done() {
        passes += 1;
        assert!(passes < 10_000, "the filling did not finish");
    }
    assert!(passes > 0, "the filling began again");
    let tx = queue.read_entry("older_summaries_read").unwrap();
    assert!(crate::infrastructure::runtime_store::ledger_views_in::usable(&tx).unwrap());
}

/// Each seek of what the range names searches its index, none of them a
/// scan of a table.
#[test]
fn each_seek_of_the_views_searches_its_index() {
    use crate::domain::EventKind;
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    let dialect = SqlDialect::Sqlite;
    let seeks: Vec<(&str, String)> = vec![
        ("ledger_main_summaries", SUMMARY_SQL.to_owned()),
        ("ledger_ci_by_branch_time", ci_boundary_sql(dialect, false)),
        ("ledger_ci_by_branch_time", ci_boundary_sql(dialect, true)),
        ("ledger_rows_queue_views", after_summary_sql(dialect)),
        ("main_commits_by_sha", COMMITS_BY_SHA_SQL.to_owned()),
        ("main_commits_by_reverts", COMMITS_BY_REVERTS_SQL.to_owned()),
        ("ci_reds_by_sha", reds_by_sha_sql(dialect)),
        ("events_by_goal", goal_down_sql(dialect)),
        ("goal_submissions", submissions_down_sql(dialect)),
        ("goal_closes", closes_up_sql(dialect)),
        (
            "attribute_baseline_marks",
            marks_down_sql(dialect, EventKind::AttributesBaseline),
        ),
        (
            "attribute_baseline_marks",
            marks_up_sql(dialect, EventKind::AttributesBaseline),
        ),
        (
            "finding_baseline_marks",
            marks_down_sql(dialect, EventKind::FindingBaseline),
        ),
        ("finding_changes_by_proposal", links_sql(dialect)),
        ("ledger_rows_findings", finding_row_sql(dialect)),
    ];
    for (index, sql) in seeks {
        let steps = reads(&queue.conn, &sql);
        assert!(!steps.is_empty(), "{sql}");
        // A summary is sought by its primary key, every other seek by its
        // index.
        let searched = |step: &String| {
            step.starts_with("SEARCH ")
                && step.contains(index)
                && (index != "ledger_main_summaries" || step.contains("INTEGER PRIMARY KEY"))
        };
        assert!(steps.iter().all(searched), "{index}\n{sql}\n{steps:#?}");
    }
}

/// The statements of a migration, its comments out and its whitespace one
/// space.
fn statements(migration: &str) -> String {
    migration
        .lines()
        .map(|line| line.split("--").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Both series make the same indexes.
#[test]
fn postgres_makes_the_same_indexes_of_the_views_seeks() {
    // The CI expression index spells JSON paths in the adapter's dialect.
    let normalized = |sql: &str, dialect: SqlDialect| {
        let branch = dialect.json_field("records", "$[0].payload.branch", JsonType::Text);
        assert!(sql.contains(&branch));
        statements(&sql.replace(&branch, "BRANCH"))
    };
    assert_eq!(
        normalized(SQLITE, SqlDialect::Sqlite),
        normalized(POSTGRES, SqlDialect::Postgres)
    );
    assert!(
        SqliteQueue::init(format!(
            "file:/ledger-queue-named-schema-{}?vfs=memdb",
            uuid::Uuid::new_v4()
        ))
        .is_ok()
    );
}
