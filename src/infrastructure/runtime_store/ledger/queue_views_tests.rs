//! The indexes of the migration `ledger_queue_views` on the schema
//! `SqliteQueue::init` leaves and with no `sqlite_stat1`: each seek a read
//! from a time takes of what its range names (a supervisor's last event of
//! a kind, a mark's retraction, an ask by its id, CI's turns to red on a
//! sha, the findings linking a proposal, the latest mark of a baseline, a
//! commit by its sha and those reverting it, the snapshots naming a task,
//! the latest summary) and the store's own reads of the views search an
//! index and scan no table. The Postgres migration makes the same indexes
//! on the Postgres dialect's expressions. The store's write of a part of
//! the queue's rows with no marks writes the views with them.

use rusqlite::Connection;

use super::queue_views::{view_rows_from, view_rows_sql};
use super::*;

const POSTGRES: &str = include_str!("../../../../migrations-postgres/ledger_queue_views.sql");

/// The steps of the plan of `sql` that read a table or an index.
fn reads(conn: &Connection, sql: &str) -> Vec<String> {
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

/// Each seek, the index it searches, and its query as a read writes it.
fn seeks() -> Vec<(&'static str, &'static str, String)> {
    let dialect = SqlDialect::Sqlite;
    let key = |key: &str| dialect.ledger_index_key("payload", key);
    let mark = dialect.json_field("payload", "$.mark", JsonType::Integer);
    vec![
        (
            "a supervisor's last event of a kind before S, walked back in id",
            "events_by_kind_supervisor",
            format!(
                "SELECT * FROM run_events INDEXED BY events_by_kind_supervisor
                  WHERE kind = ?1 AND {} = ?2 AND id < ?3 ORDER BY id DESC LIMIT 1",
                key("supervisor")
            ),
        ),
        (
            "the retraction of a mark by its id (the index the window's reads made)",
            "events_by_retracted_mark",
            format!(
                "SELECT * FROM run_events INDEXED BY events_by_retracted_mark
                  WHERE kind = 'mark_retracted' AND {mark} = ?1 ORDER BY id LIMIT 1"
            ),
        ),
        (
            "the ask an answer of the range names",
            "asks_by_id",
            format!(
                "SELECT * FROM run_events INDEXED BY asks_by_id
                  WHERE kind = 'ask_opened' AND {} = ?1 ORDER BY id",
                key("ask_id")
            ),
        ),
        (
            "CI's turns to red on a landed commit",
            "ci_reds_by_sha",
            format!(
                "SELECT * FROM run_events INDEXED BY ci_reds_by_sha
                  WHERE kind = 'ci_turned_red' AND {} = ?1 ORDER BY id",
                key("sha")
            ),
        ),
        (
            "the findings that link a proposal",
            "finding_changes_by_proposal",
            format!(
                "SELECT * FROM run_events INDEXED BY finding_changes_by_proposal
                  WHERE kind = 'finding_status_changed' AND {} = ?1 ORDER BY id",
                key("proposal_id")
            ),
        ),
        (
            "the latest mark of the attributes' baselines before an id",
            "attribute_baseline_marks",
            "SELECT * FROM run_events INDEXED BY attribute_baseline_marks
              WHERE kind = 'attributes_baseline' AND task_id IS NULL AND goal_id IS NULL
                AND id < ?1 ORDER BY id DESC LIMIT 1"
                .to_owned(),
        ),
        (
            "the latest mark of the findings' baselines before an id",
            "finding_baseline_marks",
            format!(
                "SELECT * FROM run_events INDEXED BY finding_baseline_marks
                  WHERE kind = 'finding_baseline' AND {} IS NULL
                    AND id < ?1 ORDER BY id DESC LIMIT 1",
                key("finding")
            ),
        ),
        (
            "the commits recorded of a sha",
            "main_commits_by_sha",
            "SELECT event_id FROM ledger_main_commits WHERE sha = ?1 ORDER BY event_id".to_owned(),
        ),
        (
            "the commits that revert a sha",
            "main_commits_by_reverts",
            "SELECT event_id FROM ledger_main_commits WHERE reverts = ?1 ORDER BY event_id"
                .to_owned(),
        ),
        (
            "a commit recorded before an event (the summary's count of it once)",
            "main_commits_by_sha",
            "SELECT 1 FROM ledger_main_commits WHERE sha=?1 AND event_id<?2 LIMIT 1".to_owned(),
        ),
        (
            "the snapshots that name a task",
            "snapshot_targets_by_task",
            "SELECT snapshot_event_id FROM ledger_snapshot_targets WHERE task_id=?1
              ORDER BY snapshot_event_id"
                .to_owned(),
        ),
        (
            "the latest summary through an event",
            "INTEGER PRIMARY KEY",
            "SELECT summary FROM ledger_main_summaries WHERE through <= ?1
              ORDER BY through DESC LIMIT 1"
                .to_owned(),
        ),
        (
            "the latest summary at a time or before",
            "main_summaries_by_time",
            "SELECT summary FROM ledger_main_summaries WHERE at_ms <= ?1
              ORDER BY at_ms DESC, through DESC LIMIT 1"
                .to_owned(),
        ),
        (
            "whether a row of the ledger answers an ask",
            "answered_asks_by_ask",
            "SELECT 1 FROM ledger_answered_asks WHERE what=?1 LIMIT 1".to_owned(),
        ),
        (
            "the queue rows the filling takes past its mark",
            "ledger_rows_queue_views",
            format!(
                "SELECT key, inputs_from FROM {} WHERE {} AND inputs_from > ?1
                  ORDER BY inputs_from LIMIT ?2",
                view_rows_from(dialect),
                view_rows_sql()
            ),
        ),
    ]
}

#[test]
fn each_seek_of_what_a_range_names_searches_its_index() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    for (read, index, sql) in seeks() {
        let steps = reads(&queue.conn, &sql);
        assert_eq!(steps.len(), 1, "{read}\n{sql}\n{steps:#?}");
        let step = &steps[0];
        assert!(
            step.starts_with("SEARCH ") && step.contains(index),
            "{read}: {index}\n{sql}\n{steps:#?}"
        );
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

fn sqlite_migration() -> &'static str {
    crate::infrastructure::schema::MIGRATIONS
        .iter()
        .find(|migration| migration.contains("CREATE TABLE ledger_snapshot_targets"))
        .unwrap()
}

/// Both series make the same tables and indexes, each expression the one
/// its dialect writes for the read, and the queue rows' kinds and the
/// answer's key those the store's reads write.
#[test]
fn postgres_makes_the_same_views_and_indexes_on_its_dialect_s_expressions() {
    let names = |migration: &str, what: &str| -> Vec<String> {
        statements(migration)
            .split(&format!("CREATE {what} "))
            .skip(1)
            .map(|rest| rest.split([' ', '(']).next().unwrap().to_owned())
            .collect()
    };
    let sqlite = statements(sqlite_migration());
    let postgres = statements(POSTGRES);
    for what in ["TABLE", "INDEX"] {
        assert_eq!(names(&sqlite, what), names(&postgres, what), "{what}");
    }
    let searched: Vec<&str> = seeks().iter().map(|(_, index, _)| *index).collect();
    for index in names(&sqlite, "INDEX") {
        assert!(
            searched.contains(&index.as_str()),
            "no seek searches {index}"
        );
    }
    for (dialect, migration) in [
        (SqlDialect::Sqlite, &sqlite),
        (SqlDialect::Postgres, &postgres),
    ] {
        assert!(migration.contains(&format!("WHERE {};", view_rows_sql())));
        for (key, kind) in [
            ("ask_id", "ask_opened"),
            ("sha", "ci_turned_red"),
            ("proposal_id", "finding_status_changed"),
        ] {
            let expected = format!(
                "(({}), id) WHERE kind = '{kind}';",
                dialect.ledger_index_key("payload", key)
            );
            assert!(migration.contains(&expected), "{expected}\n{migration}");
        }
        let supervisor = dialect.ledger_index_key("payload", "supervisor");
        assert!(migration.contains(&format!("(kind, ({supervisor}), id)")));
    }
    // The retraction of a mark keeps the window's own index: not again.
    assert!(!sqlite.contains("'$.mark'"));
}

/// The rows of a pass written with no marks (as a part of the queue's rows
/// in its middle is) through the store's own operation keep the views with
/// them, R untouched.
#[test]
fn a_part_written_with_no_marks_keeps_the_views() {
    use crate::domain::ledger::queue_views::{asks_left_open, snapshot_targets};
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    assert!(queue.fill_ledger_queue_views().unwrap().is_done());
    let mut events = Vec::new();
    for (n, (kind, payload)) in [
        ("forecast_recorded", serde_json::json!({"tasks": [{"id": 5}, {"id": 6}]})),
        ("ask_opened", serde_json::json!({"ask_id": 3})),
        (
            "main_commits_recorded",
            serde_json::json!({"after": null, "commits": [{"sha": "a", "at": 1, "changes": [], "reverts": null}]}),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let id = EventId::new(i64::try_from(n).unwrap() + 1);
        events.push(RunEvent {
            id,
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: crate::domain::marks::utc_text(1_900_000_000_000 + id.as_i64()),
            actor: None,
        });
    }
    let rows: Vec<(LedgerRow, Option<LedgerRow>)> =
        crate::domain::ledger::build(&events, 1_900_000_100_000, "2030-01-01T00:00:00.000Z")
            .into_iter()
            .filter(|row| row.kind == LedgerKind::Queue)
            .map(|row| (row, None))
            .collect();
    assert_eq!(rows.len(), 3);
    // R and W at an event before these rows, as a pass wrote them.
    let tx = queue.write_entry("test_marks").unwrap();
    let cursors = serde_json::json!({"events": 0, "samples": 0, "steps": 0, "resources": 0, "inside_resources": 0});
    let r = crate::domain::ledger::session::written::Marks {
        sessions: SessionsWritten::at(EventId::new(0)),
        rows: Some(
            serde_json::from_value(serde_json::json!({"through": cursors, "ledger_v": crate::domain::ledger::LEDGER_V}))
                .unwrap(),
        ),
        omitted: None,
    };
    assert!(views_in::write_marks(&tx, &r).unwrap());
    tx.commit().unwrap();
    let marks = queue.ledger_marks().unwrap();
    assert_eq!(marks, Some(r));
    assert!(queue.write_session_rows_marked(&rows, None).unwrap());
    assert_eq!(queue.ledger_marks().unwrap(), marks);
    let tx = queue.read_entry("test").unwrap();
    let views = views_in::views(&tx).unwrap();
    assert_eq!(views_in::held(&tx).unwrap(), Filled::Done);
    let written: Vec<LedgerRow> = rows.into_iter().map(|(row, _)| row).collect();
    assert_eq!(
        views.targets,
        snapshot_targets(&events[0])
            .into_iter()
            .map(|task| (1, task))
            .collect()
    );
    assert_eq!(views.asks, asks_left_open(&written));
    assert_eq!(views.commits.len(), 1);
    assert_eq!(views.summaries.len(), 1);
}
