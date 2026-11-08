//! SQLite query plans and event selection for the readiness replay.
use super::*;
use crate::infrastructure::sqlite::SqliteQueue;

#[test]
fn readiness_event_reads_name_and_use_their_indexes_without_statistics() {
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
    let sql = task_ready_at_sql(SqlDialect::Sqlite);
    assert_eq!(sql.matches("INDEXED BY events_by_task").count(), 6);
    assert_eq!(sql.matches("INDEXED BY events_by_kind").count(), 1);
    for branch in [
        "SELECT * FROM run_events INDEXED BY events_by_task WHERE task_id=?1",
        "SELECT * FROM run_events INDEXED BY events_by_task WHERE kind=",
        "SELECT * FROM run_events INDEXED BY events_by_kind WHERE task_id IS NULL",
    ] {
        assert!(sql.contains(branch), "{sql}");
    }
    let mut statement = queue
        .conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap();
    let plan: Vec<String> = statement
        .query_map([rusqlite::types::Null], |r| r.get("detail"))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let reads: Vec<_> = plan
        .iter()
        .filter(|step| step.starts_with("SEARCH run_events") || step.starts_with("SCAN run_events"))
        .collect();
    assert_eq!(reads.len(), 7, "{plan:#?}");
    assert_eq!(
        reads
            .iter()
            .filter(|step| step.contains("INDEX events_by_task (task_id=?"))
            .count(),
        6,
        "{plan:#?}"
    );
    assert_eq!(
        reads
            .iter()
            .filter(|step| step.contains("INDEX events_by_kind (kind=?"))
            .count(),
        1,
        "{plan:#?}"
    );
    assert!(
        !plan.iter().any(|step| step.contains("events_by_goal")
            || step.contains("events_by_run")
            || step.starts_with("SCAN run_events")),
        "{plan:#?}"
    );
}

#[test]
fn indexed_readiness_replay_keeps_the_events_and_ready_at_through_dependency_and_goal_edits() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    queue.conn.execute_batch(
        "INSERT INTO tasks(id,title,description,acceptance,verification_commands)
         VALUES (7,'task','','','[]'),(8,'predecessor','','','[]');
         INSERT INTO goals(id,title) VALUES (1,'original'),(2,'destination'),(3,'dependency'),(4,'unrelated');"
    ).unwrap();
    let sql = task_ready_at_sql(SqlDialect::Sqlite);
    // The same selection before index hints: compare rows, not just the
    // final time, at every prefix of the recorded history.
    let unpinned = sql
        .replace(" INDEXED BY events_by_task", "")
        .replace(" INDEXED BY events_by_kind", "");
    let history = [
        (
            None,
            Some(1),
            "goal_created",
            json!({"goal":{"status":"draft"}}),
        ),
        (
            Some(7),
            None,
            "task_created",
            json!({"status":"ready","goal_id":1}),
        ),
        (None, Some(1), "goal_status_changed", json!({"to":"open"})),
        (
            Some(7),
            None,
            "dependency_added",
            json!({"predecessor_id":8}),
        ),
        (
            Some(8),
            None,
            "task_status_changed",
            json!({"to":"completed"}),
        ),
        (Some(8), None, "task_status_changed", json!({"to":"ready"})),
        (
            Some(7),
            None,
            "dependency_removed",
            json!({"predecessor_id":8}),
        ),
        (
            None,
            Some(2),
            "goal_created",
            json!({"goal":{"status":"draft"}}),
        ),
        (Some(7), None, "task_goal_changed", json!({"from":1,"to":2})),
        (None, Some(2), "goal_status_changed", json!({"to":"open"})),
        (Some(7), None, "goal_dependency_added", json!({"goal_id":3})),
        (None, Some(3), "goal_closed", json!({"verdict":"achieved"})),
        (None, Some(3), "goal_reopened", json!({})),
        (
            Some(7),
            None,
            "goal_dependency_removed",
            json!({"goal_id":3}),
        ),
        (
            Some(7),
            None,
            "task_goal_changed",
            json!({"from":2,"to":null}),
        ),
        (None, Some(4), "goal_closed", json!({"verdict":"achieved"})),
        (Some(7), None, "task_edited", json!({})),
    ];
    for (index, (task, goal, kind, payload)) in history.into_iter().enumerate() {
        let at = format!("2026-10-01T10:{index:02}:00.000Z");
        queue.conn.execute(
            "INSERT INTO run_events(task_id,goal_id,kind,payload,created_at) VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![task, goal, kind, payload.to_string(), at],
        ).unwrap();
        let selected =
            SqlRead::query_map(&*queue.conn, &sql, sql_params![7_i64], event_row).unwrap();
        let old =
            SqlRead::query_map(&*queue.conn, &unpinned, sql_params![7_i64], event_row).unwrap();
        assert_eq!(
            serde_json::to_value(&selected).unwrap(),
            serde_json::to_value(&old).unwrap(),
            "prefix {index}"
        );
        let all = SqlRead::query_map(
            &*queue.conn,
            "SELECT * FROM run_events ORDER BY id",
            sql_params![],
            event_row,
        )
        .unwrap();
        assert_eq!(
            task_ready_at(&*queue.conn, TaskId::new(7)).unwrap(),
            claim_facts::claimable_since(TaskId::new(7), &all),
            "prefix {index}"
        );
    }
    assert_eq!(
        task_ready_at(&*queue.conn, TaskId::new(7)).unwrap(),
        Some("2026-10-01T10:13:00.000Z".to_owned())
    );
}
