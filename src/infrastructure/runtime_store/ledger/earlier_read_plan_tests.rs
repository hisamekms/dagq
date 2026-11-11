//! Representative SQL for the earlier seeks, before the reads replace the
//! whole read. Use the dialect's expressions and bound IDs, as the later
//! reads must, on the migrated adapter with no planner statistics.
use super::*;

const POSTGRES: &str =
    include_str!("../../../../migrations-postgres/ledger_earlier_seek_indexes.sql");

fn expression_indexes() -> Vec<(&'static str, &'static str, Option<&'static str>)> {
    vec![
        ("events_created_by_goal", "goal_id", Some("task_created")),
        (
            "events_baseline_by_goal",
            "goal_id",
            Some("attributes_baseline"),
        ),
        ("events_changed_by_goal", "to", Some("task_goal_changed")),
        (
            "events_submitted_by_proposal",
            "proposal_id",
            Some("task_submitted"),
        ),
        (
            "events_reopened_by_proposal",
            "proposal_id",
            Some("task_reopened"),
        ),
        (
            "events_goals_by_proposal",
            "proposal_id",
            Some("goal_submitted"),
        ),
        ("events_by_plan_review", "plan_review_id", None),
        (
            "follow_ups_by_task",
            "task_id",
            Some("follow_up_registered"),
        ),
    ]
}

/// Require a bounded SEARCH, including the expression/range part of the
/// key: an index name alone could hide walking every event of a kind.
fn assert_seek(conn: &rusqlite::Connection, sql: &str, index: &str, bound: &str) {
    let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let nulls = vec![rusqlite::types::Null; statement.parameter_count()];
    let steps: Vec<String> = statement
        .query_map(rusqlite::params_from_iter(nulls), |r| r.get("detail"))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(steps.len(), 1, "{sql}: {steps:?}");
    let step = &steps[0];
    assert!(
        step.starts_with("SEARCH ") && step.contains(index) && step.contains(bound),
        "{sql}: {steps:?}"
    );
}

#[test]
fn earlier_seeks_search_the_indexed_keys_instead_of_scanning_history() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    for kind in [
        "task_created",
        "attributes_baseline",
        "task_goal_changed",
        "task_status_changed",
        "task_reopened",
    ] {
        // The reopening is read only after the latest status's id.
        let after = if kind == "task_reopened" {
            " AND id > ?2"
        } else {
            ""
        };
        assert_seek(
            &queue.conn,
            &format!(
                "SELECT * FROM run_events INDEXED BY events_by_task_kind
             WHERE task_id = ?1 AND kind = '{kind}'{after} ORDER BY id DESC LIMIT 1"
            ),
            "events_by_task_kind",
            "task_id=? AND kind=?",
        );
    }
    for (index, key, kind) in expression_indexes() {
        let field = SqlDialect::Sqlite.ledger_index_key("payload", key);
        let predicate = kind
            .map(|k| format!("kind = '{k}' AND "))
            .unwrap_or_default();
        // INDEXED BY also makes preparation fail if a future predicate no
        // longer implies the partial index's kind (as in window_read_plan_tests).
        assert_seek(
            &queue.conn,
            &format!("SELECT * FROM run_events INDEXED BY {index} WHERE {predicate}{field} = ?1"),
            index,
            "<expr>=?",
        );
    }
    assert_seek(
        &queue.conn,
        "SELECT * FROM ledger_rows WHERE kind = 'queue' AND subject_kind = ?1
         AND ended_at < ?2 ORDER BY ended_at DESC LIMIT 1",
        "ledger_rows_by_subject_end",
        "kind=? AND subject_kind=? AND ended_at<?",
    );
    for prefix in ["runtime_planner", "plan_review"] {
        assert_seek(
            &queue.conn,
            &format!(
                "SELECT * FROM ledger_rows WHERE kind = 'session'
             AND key >= '{prefix}/9/' AND key < '{prefix}/90'"
            ),
            "sqlite_autoindex_ledger_rows_1",
            "kind=? AND key>? AND key<?",
        );
    }
}

#[test]
fn both_migrations_index_the_dialect_s_earlier_key_expressions() {
    let sqlite = crate::infrastructure::schema::MIGRATIONS
        .iter()
        .find(|m| m.contains("CREATE INDEX events_by_task_kind"))
        .unwrap();
    for (migration, dialect) in [
        (*sqlite, SqlDialect::Sqlite),
        (POSTGRES, SqlDialect::Postgres),
    ] {
        for (index, key, kind) in expression_indexes() {
            let predicate = kind
                .map(|k| format!("\n  WHERE kind = '{k}'"))
                .unwrap_or_default();
            let expected = format!(
                "CREATE INDEX {index} ON run_events(({})){predicate};",
                dialect.ledger_index_key("payload", key)
            );
            assert!(migration.contains(&expected), "missing {expected}");
        }
        for expected in [
            "CREATE INDEX events_by_task_kind ON run_events(task_id, kind, id);",
            "CREATE INDEX ledger_rows_by_subject_end ON ledger_rows(kind, subject_kind, ended_at);",
        ] {
            assert!(migration.contains(expected), "missing {expected}");
        }
    }
}
