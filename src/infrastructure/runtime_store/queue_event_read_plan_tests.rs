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

/// All presence combinations, plus empty kinds (which must return no rows).
fn event_filters() -> Vec<EventFilter> {
    (0..64)
        .flat_map(|mask| {
            let filter = EventFilter {
                kinds: (mask & 1 != 0)
                    .then(|| vec!["follow_up_adopted".into(), "task_status_changed".into()]),
                run: (mask & 2 != 0).then(|| RunId::new("run-a").unwrap()),
                task: (mask & 4 != 0).then_some(TaskId::new(1)),
                goal: (mask & 8 != 0).then_some(GoalId::new(1)),
                since: (mask & 16 != 0).then(|| "2026-11-02T00:00:00Z".into()),
                until: (mask & 32 != 0).then(|| "2026-11-03T00:00:00Z".into()),
            };
            let mut filters = vec![filter.clone()];
            if filter.kinds.is_some() {
                filters.push(EventFilter {
                    kinds: Some(vec![]),
                    ..filter
                });
            }
            filters
        })
        .collect()
}

#[test]
fn events_between_uses_the_filter_index_without_optional_or_or_table_scans() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    assert_eq!(
        queue
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='sqlite_stat1'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    for filter in event_filters() {
        let sql = super::run_log::events_between_sql(&filter, SqlDialect::Sqlite);
        assert!(!sql.contains("IS NULL OR"), "{sql}");
        let plan = plan(&queue.conn, &sql);
        let reads: Vec<_> = table_reads(&plan)
            .into_iter()
            .filter(|step| step.contains("run_events"))
            .collect();
        let expected = if filter.run.is_some() {
            vec!["events_by_run", "events_by_kind"]
        } else if filter.task.is_some() {
            vec!["events_by_task"]
        } else if filter.goal.is_some() {
            vec!["events_by_goal", "events_by_task"]
        } else if filter.kinds.is_some() {
            vec!["events_by_kind"]
        } else {
            vec!["INTEGER PRIMARY KEY (rowid>? AND rowid<?)"]
        };
        assert_eq!(reads.len(), expected.len(), "{filter:?} {plan:#?}");
        for index in expected {
            assert!(
                reads
                    .iter()
                    .any(|step| step.starts_with("SEARCH run_events") && step.contains(index)),
                "{filter:?}: missing {index}: {plan:#?}"
            );
        }
        assert!(
            !plan.iter().any(|step| step.starts_with("SCAN run_events")),
            "{plan:#?}"
        );
        let postgres = super::run_log::events_between_sql(&filter, SqlDialect::Postgres);
        assert!(
            !postgres.contains("IS NULL OR") && !postgres.contains("INDEXED BY"),
            "{postgres}"
        );
    }
}

/// The previous statement is deliberately independent of the SQL builder.
fn events_between_oracle(
    queue: &SqliteQueue,
    after: EventId,
    upto: EventId,
    filter: &EventFilter,
    limit: usize,
) -> Vec<RunEvent> {
    use crate::infrastructure::sql_dialect::{JsonType, TimeForm};
    use crate::infrastructure::sql_exec::{SqlRead, sql_params};
    let kinds = filter
        .kinds
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .unwrap();
    SqlRead::query_map(
        &*queue.conn,
        &format!(
            "SELECT * FROM run_events WHERE id>?1 AND id<=?2
                 AND (CAST(?3 AS TEXT) IS NULL OR kind IN (SELECT {kind} FROM {kinds}))
                 AND (CAST(?4 AS TEXT) IS NULL OR run_id=?4 OR {}=?4)
                 AND (CAST(?5 AS BIGINT) IS NULL OR task_id=?5)
                 AND (CAST(?6 AS BIGINT) IS NULL OR goal_id=?6
                      OR task_id IN (SELECT id FROM tasks WHERE goal_id=?6))
                 AND (CAST(?7 AS TEXT) IS NULL OR {}>={})
                 AND (CAST(?8 AS TEXT) IS NULL OR {}<{})
                 ORDER BY id LIMIT ?9",
            SqlDialect::Sqlite.json_field("payload", "$.source_run_id", JsonType::Text),
            SqlDialect::Sqlite.time_value("created_at", TimeForm::Ordinal),
            SqlDialect::Sqlite.time_value("?7", TimeForm::Ordinal),
            SqlDialect::Sqlite.time_value("created_at", TimeForm::Ordinal),
            SqlDialect::Sqlite.time_value("?8", TimeForm::Ordinal),
            kind = SqlDialect::Sqlite.json_element("value", JsonType::Text),
            kinds = SqlDialect::Sqlite.json_elements("?3"),
        ),
        sql_params![
            after,
            upto,
            kinds,
            filter.run,
            filter.task,
            filter.goal,
            filter.since,
            filter.until,
            i64::try_from(limit).unwrap()
        ],
        event_row,
    )
    .unwrap()
}

#[test]
fn events_between_matches_the_old_statement_for_filters_ranges_and_limits() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    queue.conn.execute_batch("INSERT INTO goals(id,title) VALUES (1,'one'),(2,'two');
        INSERT INTO tasks(id,title,description,acceptance,verification_commands,goal_id)
        VALUES (1,'one','','','[]',1),(2,'two','','','[]',2),(3,'three','','','[]',NULL);
        INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit)
        VALUES ('run-a',1,'succeeded','codex','codex','base'),('run-b',2,'succeeded','codex','codex','base');").unwrap();
    // Independent enumeration: each source kind has source-only hits, run-only
    // hits, and hits on both UNION branches. Distinct ids with identical
    // payloads must remain distinct; a row matching both branches stays once.
    let kinds = [
        "follow_up_registered",
        "follow_up_adopted",
        "draft_adopted",
        "draft_planner_opened",
        "draft_planner_settled",
        "task_status_changed",
        "run_started",
    ];
    for kind in kinds {
        for (task, goal, run) in [
            (None, None, None),
            (None, Some(1), None),
            (None, None, Some("run-a")),
            (Some(1), None, None),
            (Some(2), Some(1), None),
            (Some(1), Some(1), Some("run-a")),
            (Some(2), Some(2), Some("run-b")),
            (Some(3), None, None),
        ] {
            for source in [None, Some("run-a"), Some("run-b")] {
                // Only the enumerated writers carry source_run_id.
                if kind == "run_started" && source.is_some() {
                    continue;
                }
                for at in [
                    "2026-11-01T23:59:59Z",
                    "2026-11-02T00:00:00Z",
                    "2026-11-02T12:00:00.000Z",
                    "2026-11-03T00:00:00Z",
                    "invalid",
                ] {
                    queue
                        .conn
                        .execute(
                            "INSERT INTO run_events(task_id,goal_id,run_id,kind,payload,created_at)
                         VALUES (?1,?2,?3,?4,?5,?6)",
                            rusqlite::params![
                                task,
                                goal,
                                run,
                                kind,
                                json!({"source_run_id":source}).to_string(),
                                at
                            ],
                        )
                        .unwrap();
                }
            }
        }
    }
    let last: i64 = queue
        .conn
        .query_row("SELECT MAX(id) FROM run_events", [], |r| r.get(0))
        .unwrap();
    for filter in event_filters() {
        for (after, upto) in [
            (0, last),
            (1, last - 1),
            (last / 3, last * 2 / 3),
            (0, 1),
            (last, last),
            (last, 0),
        ] {
            for limit in [0, 1, 7, 2000] {
                let actual = queue
                    .events_between(EventId::new(after), EventId::new(upto), &filter, limit)
                    .unwrap();
                let expected = events_between_oracle(
                    &queue,
                    EventId::new(after),
                    EventId::new(upto),
                    &filter,
                    limit,
                );
                assert_eq!(
                    serde_json::to_value(actual).unwrap(),
                    serde_json::to_value(expected).unwrap(),
                    "{filter:?}, {after}..{upto}, limit {limit}"
                );
            }
        }
    }
    // Show each source-only kind is actually observed (not merely compared
    // against an accidentally empty oracle result).
    let events = queue
        .events_between(
            EventId::new(0),
            EventId::new(last),
            &EventFilter {
                run: Some(RunId::new("run-a").unwrap()),
                ..Default::default()
            },
            2000,
        )
        .unwrap();
    for kind in kinds.into_iter().filter(|kind| *kind != "run_started") {
        assert!(
            events.iter().any(|e| e.kind.as_str() == kind
                && e.run_id.is_none()
                && e.payload["source_run_id"] == "run-a"),
            "{kind}"
        );
    }
}
