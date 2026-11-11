//! The indexes of the range reads of the ledger's window (the migration
//! `ledger_window_read_indexes`) on the schema `SqliteQueue::init` leaves and
//! with no `sqlite_stat1`: for each, the query a read of the Spike's table
//! ("読みの約束" of docs/plans/ledger-window-read-preconditions-spike.md)
//! takes, written with the dialect's expressions as a read would write it,
//! looks the index up and walks no other table's read of `run_events`.
//!
//! The reads themselves are not here: these are their forms. A query hits a
//! partial index only when its condition holds the index's own terms (the
//! kind as a literal, the same expression), and an expression index only
//! when the query writes the same expression and compares it with a bound
//! value. With no statistics SQLite's planner takes `kind=` alone on another
//! index (`events_by_opened_event`, `events_by_kind`), so a SQLite read names
//! its index with `INDEXED BY`, as these do; the statement fails to prepare
//! when its terms do not let the index serve it.

use rusqlite::Connection;
use serde_json::{Value, json};

use super::*;

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

fn field(path: &str, ty: JsonType) -> String {
    SqlDialect::Sqlite.json_field("payload", path, ty)
}

/// The condition of `stats::window::ends_run` on `run_events`, as the
/// indexes `run_ends_by_time` and `run_ends_by_run` hold it.
fn ends_run_sql(dialect: SqlDialect) -> String {
    format!(
        "run_id IS NOT NULL AND task_id IS NOT NULL
         AND (kind IN ('run_integrated', 'run_satisfied_closed')
              OR {status} IN ('failed', 'interrupted'))",
        status = dialect.json_field("payload", "$.status", JsonType::Text),
    )
}

/// A claim of `run_claimed` that recorded the derived attribute `attribute`
/// (`domain::marks::DERIVED`): the condition of its index.
fn claim_records_sql(dialect: SqlDialect, attribute: &str) -> String {
    let there = |key: &str| {
        format!(
            "{} IS NOT NULL",
            dialect.json_field("payload", &format!("$.{key}"), JsonType::Text)
        )
    };
    match attribute {
        "toolchain" => format!("{} AND {}", there("rustc_release"), there("rustc_host")),
        other => there(other),
    }
}

/// Each kind of `domain::marks::RECORDED_KINDS` and its index by time.
const RECORDED_BY_TIME: [(&str, &str); 5] = [
    ("supervisor_started", "starts_by_time"),
    ("supervisor_stopped", "stops_by_time"),
    ("run_env_changed", "run_env_changes_by_time"),
    ("mark_recorded", "marks_by_time"),
    ("mark_retracted", "retractions_by_time"),
];

/// Each index, and the queries of the reads it serves.
fn queries() -> Vec<(&'static str, &'static str, String)> {
    let at = field("$.at", JsonType::Text);
    let heartbeat = field("$.last_heartbeat_at", JsonType::Integer);
    let mark = field("$.mark", JsonType::Integer);
    let to = field("$.to", JsonType::Text);
    let events = SqlDialect::Sqlite.json_field("inputs_through", "$.events", JsonType::Integer);
    let ends = ends_run_sql(SqlDialect::Sqlite);
    let mut queries = vec![
        (
            "(1) a person's marks that take effect in the range",
            "events_by_mark_at",
            format!(
                "SELECT * FROM run_events WHERE kind = 'mark_recorded'
                   AND {at} > ?1 AND {at} <= ?2 ORDER BY {at}, id"
            ),
        ),
        (
            "(1) the pruned stops that take effect in the range, in whole seconds",
            "events_by_pruned_heartbeat",
            format!(
                "SELECT * FROM run_events WHERE kind = 'supervisor_stopped'
                   AND {heartbeat} >= ?1 AND {heartbeat} <= ?2 ORDER BY {heartbeat}, id"
            ),
        ),
        (
            "(2) the first retraction of a mark",
            "events_by_retracted_mark",
            format!(
                "SELECT * FROM run_events WHERE kind = 'mark_retracted' AND {mark} = ?1
                 ORDER BY id LIMIT 1"
            ),
        ),
        (
            "(5) routine: the last start at S or before in time",
            "starts_by_time",
            "SELECT * FROM run_events WHERE kind = 'supervisor_started' AND created_at <= ?1
             ORDER BY created_at DESC, id DESC LIMIT 1"
                .to_owned(),
        ),
        (
            "(5) forecast: the tasks' ends of the range by the time they were written",
            "task_ends_by_time",
            format!(
                "SELECT * FROM run_events WHERE kind = 'task_status_changed'
                   AND {to} IN ('completed', 'canceled')
                   AND created_at > ?1 AND created_at <= ?2 ORDER BY created_at, id"
            ),
        ),
        (
            "(5) forecast: the goals' ends of the range by the time they were written",
            "goal_ends_by_time",
            "SELECT * FROM run_events WHERE kind = 'goal_closed'
               AND created_at > ?1 AND created_at <= ?2 ORDER BY created_at, id"
                .to_owned(),
        ),
        (
            "(5) an event cursor's time: (created_at, id) walked back K events",
            "events_by_time",
            "SELECT MAX(created_at) FROM (SELECT id, created_at FROM run_events
               ORDER BY created_at DESC, id DESC LIMIT ?1) WHERE id <= ?2"
                .to_owned(),
        ),
        (
            "(5) an event cursor's time: the aggregate of the events up to the id",
            "events_by_time",
            "SELECT MAX(created_at) FROM run_events WHERE id <= ?1".to_owned(),
        ),
        (
            "(5) whether any event was written off the record's form",
            "events_off_the_record_form",
            format!(
                "SELECT EXISTS (SELECT 1 FROM run_events WHERE {})",
                SqlDialect::Sqlite.off_the_record_form("created_at")
            ),
        ),
        (
            "(6) a task's end before an end in id",
            "task_ends_by_task",
            format!(
                "SELECT * FROM run_events WHERE task_id = ?1 AND kind = 'task_status_changed'
                   AND {to} IN ('completed', 'canceled') AND id < ?2
                 ORDER BY id DESC LIMIT 1"
            ),
        ),
        (
            "(6) a goal's end before an end in id",
            "goal_ends_by_goal",
            "SELECT * FROM run_events WHERE goal_id = ?1 AND kind = 'goal_closed' AND id < ?2
             ORDER BY id DESC LIMIT 1"
                .to_owned(),
        ),
        (
            "(7) the ends of runs written in the range",
            "run_ends_by_time",
            format!(
                "SELECT run_id, id, created_at FROM run_events WHERE {ends}
                   AND created_at > ?1 AND created_at <= ?2 ORDER BY created_at, id"
            ),
        ),
        (
            "(7) a run's first end",
            "run_ends_by_run",
            format!(
                "SELECT id, created_at FROM run_events WHERE run_id = ?1 AND {ends}
                 ORDER BY id LIMIT 1"
            ),
        ),
        (
            "(8) the rows of a kind whose inputs reach past a mark",
            "ledger_rows_by_inputs_through",
            format!("SELECT key FROM ledger_rows WHERE kind = ?1 AND {events} > ?2"),
        ),
    ];
    for (kind, index) in RECORDED_BY_TIME {
        queries.push((
            "(1) the recorded marks written in the range, one kind each",
            index,
            format!(
                "SELECT * FROM run_events WHERE kind = '{kind}'
                   AND created_at > ?1 AND created_at <= ?2 ORDER BY created_at, id"
            ),
        ));
    }
    for (attribute, index) in [
        ("dagq_version", "claims_with_dagq_version"),
        ("claude_version", "claims_with_claude_version"),
        ("codex_version", "claims_with_codex_version"),
        ("parallel", "claims_with_parallel"),
        ("toolchain", "claims_with_toolchain"),
    ] {
        queries.push((
            "(3) the claim before a claim in id that recorded the attribute",
            index,
            format!(
                "SELECT * FROM run_events WHERE kind = 'run_claimed'
                   AND {} AND id < ?1 ORDER BY id DESC LIMIT 1",
                claim_records_sql(SqlDialect::Sqlite, attribute)
            ),
        ));
    }
    for (key, ty, index) in [
        ("dagq_version", JsonType::Text, "starts_by_dagq_version"),
        ("parallel", JsonType::Integer, "starts_by_parallel"),
    ] {
        let value = field(&format!("$.{key}"), ty);
        queries.push((
            "(4) a start between two claims in id that names the new value",
            index,
            format!(
                "SELECT * FROM run_events WHERE kind = 'supervisor_started' AND {value} = ?1
                   AND id > ?2 AND id < ?3 ORDER BY id LIMIT 1"
            ),
        ));
    }
    queries
        .into_iter()
        .map(|(read, index, sql)| {
            let table = if index.starts_with("ledger_rows") {
                "ledger_rows"
            } else {
                "run_events"
            };
            let from = format!("FROM {table}");
            let sql = sql.replacen(&from, &format!("{from} INDEXED BY {index}"), 1);
            (read, index, sql)
        })
        .collect()
}

#[test]
fn each_read_of_the_window_s_range_looks_up_its_index() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    let stats: i64 = queue
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='sqlite_stat1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stats, 0, "the plan is the one with no statistics");
    assert_eq!(
        RECORDED_BY_TIME.map(|(kind, _)| kind),
        crate::domain::marks::RECORDED_KINDS.map(|kind| kind.as_str())
    );
    for (read, index, sql) in queries() {
        let plan = plan(&queue.conn, &sql);
        let reads: Vec<&String> = plan
            .iter()
            .filter(|step| step.starts_with("SEARCH") || step.starts_with("SCAN"))
            .filter(|step| !step.contains("(subquery-") && !step.contains("CONSTANT ROW"))
            .collect();
        assert_eq!(reads.len(), 1, "{read}\n{sql}\n{plan:#?}");
        let step = reads[0];
        assert!(
            step.contains(&format!("INDEX {index}")),
            "{read}: {index}\n{sql}\n{plan:#?}"
        );
        // A SCAN is of the index alone and stops at what bounds it: the K of
        // its LIMIT, the off-form events the partial index holds, the ids up
        // to the cursor of the aggregate the store answers in one row.
        assert!(
            step.starts_with("SEARCH")
                || matches!(index, "events_by_time" | "events_off_the_record_form"),
            "{read}: {step}"
        );
    }
}

/// The statements of a migration, its comments out and its whitespace one
/// space.
fn statements(migration: &str) -> String {
    let text: Vec<&str> = migration
        .lines()
        .map(|line| line.split("--").next().unwrap_or(""))
        .collect();
    text.join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The SQLite migration, found by what it makes: its number is the one
/// `integrate` gives it on landing.
fn sqlite_migration() -> &'static str {
    crate::infrastructure::schema::MIGRATIONS
        .iter()
        .find(|migration| migration.contains("CREATE INDEX events_by_mark_at"))
        .unwrap()
}

const POSTGRES_MIGRATION: &str =
    include_str!("../../../../migrations-postgres/ledger_window_read_indexes.sql");

/// Both series make the same indexes, and each index of an expression in
/// Postgres is on the expression the Postgres dialect writes for the read's
/// path and type, so a read built by the dialect gives the same value in
/// both stores and hits the index there too.
#[test]
fn postgres_makes_the_same_indexes_on_the_dialect_s_expressions() {
    let names = |migration: &str| -> Vec<String> {
        statements(migration)
            .split("CREATE INDEX ")
            .skip(1)
            .map(|rest| rest.split(' ').next().unwrap().to_owned())
            .collect()
    };
    let sqlite = names(sqlite_migration());
    assert_eq!(sqlite, names(POSTGRES_MIGRATION));
    let every: Vec<&str> = queries().iter().map(|(_, index, _)| *index).collect();
    for name in &sqlite {
        assert!(every.contains(&name.as_str()), "no read looks up {name}");
    }

    let postgres = statements(POSTGRES_MIGRATION);
    let pg = SqlDialect::Postgres;
    let text = |path: &str| pg.json_field("payload", path, JsonType::Text);
    let integer = |path: &str| pg.json_field("payload", path, JsonType::Integer);
    for expected in [
        format!(
            "ON run_events(({}), id) WHERE kind = 'mark_recorded';",
            text("$.at")
        ),
        format!(
            "ON run_events(({}), id) WHERE kind = 'supervisor_stopped';",
            integer("$.last_heartbeat_at")
        ),
        format!(
            "ON run_events(kind, ({}), id) WHERE kind = 'mark_retracted';",
            integer("$.mark")
        ),
        format!(
            "ON run_events(({}), id) WHERE kind = 'supervisor_started';",
            text("$.dagq_version")
        ),
        format!(
            "ON run_events(({}), id) WHERE kind = 'supervisor_started';",
            integer("$.parallel")
        ),
        format!(
            "ON run_events(task_id, id) WHERE kind = 'task_status_changed' AND {} IN ('completed', 'canceled');",
            text("$.to")
        ),
        format!(
            "ON ledger_rows(kind, ({}));",
            pg.json_field("inputs_through", "$.events", JsonType::Integer)
        ),
        format!(
            "ON run_events(created_at, id) WHERE {};",
            statements(&ends_run_sql(pg))
        ),
        format!(
            "ON run_events(run_id, id) WHERE {};",
            statements(&ends_run_sql(pg))
        ),
        format!("WHERE {};", pg.off_the_record_form("created_at")),
    ] {
        assert!(postgres.contains(&expected), "{expected}\n{postgres}");
    }
    for attribute in [
        "dagq_version",
        "claude_version",
        "codex_version",
        "parallel",
        "toolchain",
    ] {
        let expected = format!(
            "ON run_events(id) WHERE kind = 'run_claimed' AND {};",
            claim_records_sql(pg, attribute)
        );
        assert!(postgres.contains(&expected), "{expected}\n{postgres}");
    }
}

/// On rows of each shape, the condition of the run's ends selects what
/// `stats::window::ends_run` does, and the check of the record's form finds
/// an event off the form and none while every event is in it, and a start's
/// `parallel` is found by the integer a read binds.
#[test]
fn the_conditions_select_what_the_domain_reads() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    queue.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    let rows: [(&str, Option<&str>, Value); 8] = [
        ("run_claimed", Some("r"), json!({})),
        (
            "validation_finished",
            Some("r"),
            json!({"status": "failed"}),
        ),
        (
            "resume_finished",
            Some("r"),
            json!({"status": "interrupted"}),
        ),
        ("run_integrated", Some("r"), json!({})),
        ("run_satisfied_closed", Some("r"), json!({})),
        ("run_claimed", Some("r"), json!({"status": "running"})),
        ("run_integrated", None, json!({})),
        ("observation", None, json!({"status": "failed"})),
    ];
    for (kind, run, payload) in &rows {
        queue
            .conn
            .execute(
                "INSERT INTO run_events(task_id, run_id, kind, payload) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![run.map(|_| 1), run, kind, payload.to_string()],
            )
            .unwrap();
    }
    let every: Vec<RunEvent> = queue
        .conn
        .prepare("SELECT * FROM run_events ORDER BY id")
        .unwrap()
        .query_map([], crate::infrastructure::sqlite::event_row)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let expected: Vec<i64> = every
        .iter()
        .filter(|event| crate::domain::stats::window::ends_run(event))
        .map(|event| event.id.as_i64())
        .collect();
    assert_eq!(expected, [2, 3, 4, 5]);
    let selected: Vec<i64> = queue
        .conn
        .prepare(&format!(
            "SELECT id FROM run_events INDEXED BY run_ends_by_time WHERE {}
               AND created_at > '' ORDER BY id",
            ends_run_sql(SqlDialect::Sqlite)
        ))
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(selected, expected);

    let off_the_form = || -> bool {
        queue
            .conn
            .query_row(&off_the_form_sql(), [], |r| r.get(0))
            .unwrap()
    };
    assert!(!off_the_form(), "the queue writes every time in the form");
    queue
        .conn
        .execute(
            "INSERT INTO run_events(kind, payload, created_at)
             VALUES ('observation', '{}', '2026-11-11 10:00:00')",
            [],
        )
        .unwrap();
    assert!(off_the_form());

    queue
        .conn
        .execute(
            "INSERT INTO run_events(id, kind, payload)
             VALUES (100, 'supervisor_started', '{\"parallel\": 4}')",
            [],
        )
        .unwrap();
    let start = |parallel: &dyn rusqlite::ToSql| -> Option<i64> {
        queue
            .conn
            .query_row(
                &format!(
                    "SELECT id FROM run_events INDEXED BY starts_by_parallel
                      WHERE kind = 'supervisor_started' AND {} = ?1 AND id > 0 AND id < 200",
                    field("$.parallel", JsonType::Integer)
                ),
                [parallel],
                |r| r.get(0),
            )
            .ok()
    };
    assert_eq!(
        start(&4_i64),
        Some(100),
        "a read binds parallel as an integer"
    );
    assert_eq!(start(&"4"), None);
}

/// Whether any event was written off the record's form, on its index.
fn off_the_form_sql() -> String {
    format!(
        "SELECT EXISTS (SELECT 1 FROM run_events INDEXED BY events_off_the_record_form
           WHERE {})",
        SqlDialect::Sqlite.off_the_record_form("created_at")
    )
}

/// A time's text that a range read by `created_at` would order apart from
/// its time is off the form, whatever its case, while one in the form at the
/// range's very end is not and is in the range: a `z` for the `Z` at the end
/// (which `timestamp_millis` reads as the same time, and `<=` the end leaves
/// out), a `t`, a space for the `T`, no milliseconds, a letter for a digit,
/// a trailing character.
#[test]
fn a_time_off_the_form_in_any_case_is_found_and_one_at_the_end_is_in_range() {
    const END: &str = "2026-11-11T10:00:00.000Z";
    let off = [
        "2026-11-11T10:00:00.000z",
        "2026-11-11t10:00:00.000Z",
        "2026-11-11 10:00:00.000Z",
        "2026-11-11T10:00:00Z",
        "2026-11-1aT10:00:00.000Z",
        "2026-11-11T10:00:00.000Z ",
    ];
    assert_eq!(
        crate::domain::stats::timestamp_millis(off[0]),
        crate::domain::stats::timestamp_millis(END),
        "the domain reads the lower-case z as the same time"
    );
    for text in off {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let write = |at: &str| {
            queue
                .conn
                .execute(
                    "INSERT INTO run_events(kind, payload, created_at) VALUES ('observation', '{}', ?1)",
                    [at],
                )
                .unwrap();
        };
        write("2026-11-11T09:59:59.999Z");
        write(END);
        let found = || -> bool {
            queue
                .conn
                .query_row(&off_the_form_sql(), [], |r| r.get(0))
                .unwrap()
        };
        assert!(!found(), "{text}: every time so far is in the form");
        let in_range: i64 = queue
            .conn
            .query_row(
                "SELECT COUNT(*) FROM run_events INDEXED BY events_by_time
                  WHERE created_at > '2026-11-11T09:00:00.000Z' AND created_at <= ?1",
                [END],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(in_range, 2, "the end of the range is in it");
        write(text);
        assert!(found(), "{text:?} is off the form");
    }
}
