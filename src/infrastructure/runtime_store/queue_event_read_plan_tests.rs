//! The reads of the queue's own events by kind (no run, task or goal) stay
//! on `events_by_kind`, on the schema `SqliteQueue::init` leaves and with no
//! `sqlite_stat1`: left to the planner, `goal_id IS NULL` picks
//! `events_by_goal` and walks every event without a goal, which held the
//! write lock for minutes.

use super::eval_rounds::{
    another_round_runs_sql, any_of_round_sql, any_of_set_sql, round_owner_sql, set_owner_sql,
};
use super::run_log::{
    LATEST_QUEUE_EVENT_SQL, inbox_nudged_sql, installs_after_sql, latest_inbox_watcher_change_sql,
    update_events_sql,
};
use super::*;
use crate::infrastructure::ci_watch_store::{
    EVENTS_OF_KIND_SQL, FRESHNESS_EVENTS_SQL, LATEST_OF_KIND_SQL, freshness_there_sql,
    latest_freshness_sql, watch_events_sql,
};
use crate::infrastructure::sql_dialect::SqlDialect;
use rusqlite::Connection;

/// The plan of `sql`, one line per step, its parameters NULL.
fn plan(conn: &Connection, sql: &str) -> Vec<String> {
    let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let nulls = vec![rusqlite::types::Null; statement.parameter_count()];
    statement
        .query_map(rusqlite::params_from_iter(nulls), |r| {
            r.get::<_, String>("detail")
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// The steps that read a table of the queue: not the kinds' `json_each`, not
/// the constant row of a `SELECT EXISTS`.
fn table_reads(plan: &[String]) -> Vec<&String> {
    plan.iter()
        .filter(|step| step.starts_with("SEARCH") || step.starts_with("SCAN"))
        .filter(|step| !step.contains("json_each") && !step.contains("CONSTANT ROW"))
        .collect()
}

#[test]
fn the_queue_event_reads_by_kind_stay_on_events_by_kind() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    let count: i64 = queue
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='sqlite_stat1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "the plan is the one with no statistics");
    let one_read = [
        // Under the write lock.
        ("take_case_set: settled", any_of_set_sql(SqlDialect::Sqlite)),
        ("take_case_set: owner", set_owner_sql(SqlDialect::Sqlite)),
        ("any_of_round", any_of_round_sql(SqlDialect::Sqlite)),
        (
            "take_up_eval_round: owner",
            round_owner_sql(SqlDialect::Sqlite),
        ),
        (
            "record_ci_check, record_ci_scheduled_check: latest",
            LATEST_OF_KIND_SQL.to_owned(),
        ),
        (
            "record_ci_freshness: latest",
            latest_freshness_sql(SqlDialect::Sqlite),
        ),
        (
            "record_ci_freshness: there",
            freshness_there_sql(SqlDialect::Sqlite),
        ),
        ("claim_inbox_nudge", inbox_nudged_sql(SqlDialect::Sqlite)),
        (
            "record_inbox_watcher_change",
            latest_inbox_watcher_change_sql(SqlDialect::Sqlite),
        ),
        // Outside the lock.
        ("ci_watch_events", watch_events_sql()),
        ("ci_scheduled_events", EVENTS_OF_KIND_SQL.to_owned()),
        ("ci_freshness_events", FRESHNESS_EVENTS_SQL.to_owned()),
        ("update_events", update_events_sql(SqlDialect::Sqlite)),
        (
            "installs_after: count",
            format!("SELECT COUNT(*) {}", installs_after_sql(SqlDialect::Sqlite)),
        ),
        (
            "installs_after: rows",
            format!(
                "SELECT * {} ORDER BY id DESC LIMIT ?3",
                installs_after_sql(SqlDialect::Sqlite)
            ),
        ),
        ("latest_queue_event", LATEST_QUEUE_EVENT_SQL.to_owned()),
    ]
    .map(|(name, sql)| (name, sql, 1));
    let statements = one_read.into_iter().chain([(
        "settle_eval_round: another_runs (s and f)",
        another_round_runs_sql(SqlDialect::Sqlite),
        2,
    )]);
    for (name, sql, reads) in statements {
        // Named, not only chosen today: `kind=` alone picks the index too
        // until statistics or a new index change the planner's choice.
        assert_eq!(
            sql.matches("INDEXED BY events_by_kind").count(),
            reads,
            "{name}\n{sql}"
        );
        let plan = plan(&queue.conn, &sql);
        let table_reads = table_reads(&plan);
        assert_eq!(table_reads.len(), reads, "{name}\n{plan:#?}");
        assert!(
            table_reads
                .iter()
                .all(|step| step.contains("INDEX events_by_kind (kind=?")),
            "{name}\n{plan:#?}"
        );
        assert!(
            !plan.iter().any(|step| step.contains("events_by_goal")
                || step.contains("events_by_run")
                || step.contains("events_by_task")
                || step.starts_with("SCAN run_events")),
            "{name}\n{plan:#?}"
        );
    }
}
