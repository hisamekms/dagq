use super::*;
use crate::infrastructure::postgres::exec::PostgresRow;
use crate::infrastructure::schema::{MIGRATIONS, floor_for};
use crate::infrastructure::sql_dialect::{JsonType, SqlDialect};
use crate::infrastructure::sql_exec::{SqlResult, SqlValue};
use crate::infrastructure::write_leader::IDENTITY_SQL;
use crate::migration_numbers::{self, POSTGRES_BASE, POSTGRES_DIRECTORY};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::Rc;

/// A database that answers from a script and records what it is told.
#[derive(Default)]
struct Scripted {
    statements: RefCell<Vec<String>>,
    /// What each [`read_schema`] finds of the version's table, in turn;
    /// the last one holds from then on.
    tables: RefCell<VecDeque<i64>>,
    objects: Cell<i64>,
    /// What each read of the row finds, in turn; the last one holds.
    rows: RefCell<VecDeque<Option<(i64, i64)>>>,
    /// What each lock changes, in turn; 1 when none is left.
    locks: RefCell<VecDeque<u64>>,
    /// The statement that fails.
    fails: RefCell<Option<String>>,
}

impl Scripted {
    fn empty() -> Self {
        let database = Self::default();
        database.tables.borrow_mut().push_back(0);
        database
    }

    fn at(version: i64, floor: i64) -> Self {
        let database = Self::default();
        database.tables.borrow_mut().push_back(1);
        database.rows.borrow_mut().push_back(Some((version, floor)));
        database
    }

    fn told(&self) -> Vec<String> {
        self.statements.borrow().clone()
    }

    fn was_told(&self, sql: &str) -> bool {
        self.statements.borrow().iter().any(|told| told == sql)
    }
}

/// The next of `script`, keeping the last.
fn next<T: Clone>(script: &RefCell<VecDeque<T>>) -> Option<T> {
    let mut script = script.borrow_mut();
    if script.len() > 1 {
        script.pop_front()
    } else {
        script.front().cloned()
    }
}

/// The row of one integer.
fn integer_row(value: i64) -> PostgresRow {
    PostgresRow::new(
        Rc::from(["count".to_owned()]),
        vec![SqlValue::Integer(value)],
    )
}

impl PostgresSession for Scripted {
    fn run(&self, sql: &str) -> Result<()> {
        self.statements.borrow_mut().push(sql.to_owned());
        if self.fails.borrow().as_deref() == Some(sql) {
            anyhow::bail!("the statement failed");
        }
        Ok(())
    }

    fn query(&self, sql: &str, _: &[SqlValue]) -> SqlResult<Vec<PostgresRow>> {
        self.statements.borrow_mut().push(sql.to_owned());
        Ok(match sql {
            HAS_SCHEMA_TABLE_SQL => vec![integer_row(next(&self.tables).unwrap_or(0))],
            OBJECTS_SQL => vec![integer_row(self.objects.get())],
            READ_SCHEMA_SQL => next(&self.rows)
                .flatten()
                .map(|(version, floor)| {
                    PostgresRow::new(
                        Rc::from(["version".to_owned(), "floor".to_owned()]),
                        vec![SqlValue::Integer(version), SqlValue::Integer(floor)],
                    )
                })
                .into_iter()
                .collect(),
            IDENTITY_SQL => vec![PostgresRow::new(
                Rc::from(["holder".to_owned(), "generation".to_owned()]),
                vec![SqlValue::Text("service-1".to_owned()), SqlValue::Integer(7)],
            )],
            other => panic!("a migration does not read {other}"),
        })
    }

    /// The only write with parameters a migration runs is the lock.
    fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<u64> {
        assert_eq!(sql, numbered_parameters(LOCK_MIGRATION_SQL));
        assert_eq!(
            params,
            [SqlValue::Text("service-1".to_owned()), SqlValue::Integer(7)]
        );
        self.statements.borrow_mut().push(sql.to_owned());
        Ok(self.locks.borrow_mut().pop_front().unwrap_or(1))
    }
}

/// Lets a test's pending migrations through whatever they declare, since
/// which of this binary's migrations are breaking is not the test's.
fn any_pending(_: &SchemaState) -> Result<()> {
    Ok(())
}

const BEGIN: &str = "BEGIN ISOLATION LEVEL READ COMMITTED";
const ROLLBACK: &str = "ROLLBACK";
const COMMIT: &str = "COMMIT";

/// A series of `after` files that reaches this binary's schema, named by
/// their versions.
fn series(after: usize) -> PostgresSeries {
    let files: Vec<&'static str> = (0..after)
        .map(|index| -> &'static str { Box::leak(format!("-- file {index}").into_boxed_str()) })
        .collect();
    PostgresSeries {
        base_version: BINARY_SCHEMA - after as i64,
        base: "-- the base",
        after: Box::leak(files.into_boxed_slice()),
    }
}

fn record_sql(series: &PostgresSeries) -> String {
    format!(
        "UPDATE dagq_schema SET version = {BINARY_SCHEMA}, floor = {}",
        recorded_floor(series.latest()).unwrap()
    )
}

#[test]
fn an_empty_database_gets_the_base_and_everything_after_it_in_one_transaction() {
    let database = Scripted::empty();
    let series = series(2);
    let report = migrate_series(&database, &series, SchemaState::refuse_breaking)
        .unwrap()
        .unwrap();
    let told = database.told();
    let begin = told.iter().position(|sql| sql == BEGIN).unwrap();
    assert_eq!(
        told[begin..],
        [
            BEGIN.to_owned(),
            "-- the base".to_owned(),
            format!(
                "INSERT INTO dagq_schema (version, floor) VALUES ({}, {})",
                series.base_version,
                recorded_floor(series.base_version).unwrap()
            ),
            "-- file 0".to_owned(),
            "-- file 1".to_owned(),
            record_sql(&series),
            COMMIT.to_owned(),
        ]
    );
    assert_eq!(report.previous_version, 0);
    assert_eq!(report.schema_version, BINARY_SCHEMA);
    assert_eq!(report.floor, recorded_floor(BINARY_SCHEMA).unwrap());
    assert_eq!(report.applied.len(), BINARY_SCHEMA as usize);
}

#[test]
fn this_binary_s_series_on_an_empty_database_reaches_its_schema_at_its_floor() {
    let database = Scripted::empty();
    let report = migrate(&database).unwrap().unwrap();
    assert!(database.was_told(SERIES.base));
    assert_eq!(report.schema_version, BINARY_SCHEMA);
    assert!(database.was_told(&format!(
        "UPDATE dagq_schema SET version = {BINARY_SCHEMA}, floor = {}",
        floor_for(BINARY_SCHEMA)
    )));
}

#[test]
fn a_database_with_other_tables_and_no_schema_is_refused_and_not_written() {
    let database = Scripted::empty();
    database.objects.set(1);
    let error = migrate(&database).unwrap_err();
    assert_eq!(error.to_string(), "database is not an empty dagq queue");
    assert!(!database.was_told(BEGIN));
}

#[test]
fn another_init_first_leaves_this_one_to_judge_what_it_made() {
    let database = Scripted::default();
    database.tables.borrow_mut().extend([0, 1]);
    database
        .rows
        .borrow_mut()
        .push_back(Some((BINARY_SCHEMA, 1)));
    let series = series(0);
    *database.fails.borrow_mut() = Some(series.base.to_owned());
    assert!(
        migrate_series(&database, &series, any_pending)
            .unwrap()
            .is_none()
    );
    assert!(database.was_told(ROLLBACK));
    assert!(!database.was_told(COMMIT));
}

#[test]
fn a_base_that_fails_on_a_database_still_empty_is_the_error() {
    let database = Scripted::empty();
    let series = series(0);
    *database.fails.borrow_mut() = Some(series.base.to_owned());
    let error = migrate_series(&database, &series, any_pending).unwrap_err();
    assert!(format!("{error:#}").contains("apply the Postgres base"));
    assert!(database.was_told(ROLLBACK));
}

#[test]
fn a_database_at_this_binary_s_schema_is_left_alone() {
    let database = Scripted::at(BINARY_SCHEMA, 1);
    assert!(migrate(&database).unwrap().is_none());
    assert!(!database.was_told(BEGIN));
}

#[test]
fn a_floor_past_this_binary_refuses_it_and_writes_nothing() {
    let database = Scripted::at(BINARY_SCHEMA + 1, BINARY_SCHEMA + 1);
    let error = migrate(&database).unwrap_err();
    assert!(
        error.to_string().contains("install a newer dagq"),
        "{error}"
    );
    assert!(!database.was_told(BEGIN));
}

#[test]
fn a_newer_schema_whose_floor_allows_this_binary_is_left_alone_and_opens() {
    let database = Scripted::at(BINARY_SCHEMA + 1, 1);
    assert!(migrate(&database).unwrap().is_none());
    check_opens(&database).unwrap();
}

#[test]
fn only_a_database_at_this_binary_s_schema_or_past_it_opens() {
    check_opens(&Scripted::at(BINARY_SCHEMA, 1)).unwrap();
    let error = check_opens(&Scripted::at(BINARY_SCHEMA - 1, 1)).unwrap_err();
    assert!(error.to_string().contains("run `dagq migrate`"), "{error}");
    let error = check_opens(&Scripted::empty()).unwrap_err();
    assert!(error.to_string().contains("run init"), "{error}");
    let database = Scripted::default();
    database.tables.borrow_mut().push_back(1);
    let error = check_opens(&database).unwrap_err();
    assert!(error.to_string().contains("has no row"), "{error}");
}

#[test]
fn pending_files_are_applied_after_the_lock_on_the_row_read_outside_the_transaction() {
    let database = Scripted::at(BINARY_SCHEMA - 1, 1);
    let series = series(2);
    let report = migrate_series(&database, &series, any_pending)
        .unwrap()
        .unwrap();
    let told = database.told();
    let identity = told.iter().position(|sql| sql == IDENTITY_SQL).unwrap();
    assert_eq!(
        told[identity..],
        [
            IDENTITY_SQL.to_owned(),
            BEGIN.to_owned(),
            numbered_parameters(LOCK_MIGRATION_SQL),
            READ_SCHEMA_SQL.to_owned(),
            "-- file 1".to_owned(),
            record_sql(&series),
            COMMIT.to_owned(),
        ]
    );
    assert_eq!(report.previous_version, BINARY_SCHEMA - 1);
    assert_eq!(report.applied.len(), 1);
}

#[test]
fn a_takeover_between_the_read_and_the_lock_applies_nothing_and_reads_again() {
    let database = Scripted::at(BINARY_SCHEMA - 1, 1);
    database.locks.borrow_mut().push_back(0);
    let series = series(1);
    migrate_series(&database, &series, any_pending)
        .unwrap()
        .unwrap();
    let told = database.told();
    let rollback = told.iter().position(|sql| sql == ROLLBACK).unwrap();
    assert!(!told[..rollback].contains(&"-- file 0".to_owned()));
    assert_eq!(told.iter().filter(|sql| *sql == IDENTITY_SQL).count(), 2);
    assert_eq!(told.iter().filter(|sql| *sql == "-- file 0").count(), 1);
    assert_eq!(told.last().unwrap(), COMMIT);
}

#[test]
fn a_migrate_that_went_first_under_the_lock_leaves_nothing_to_apply() {
    let database = Scripted::default();
    database.tables.borrow_mut().push_back(1);
    database
        .rows
        .borrow_mut()
        .extend([Some((BINARY_SCHEMA - 1, 1)), Some((BINARY_SCHEMA, 1))]);
    let series = series(1);
    assert!(
        migrate_series(&database, &series, any_pending)
            .unwrap()
            .is_none()
    );
    assert!(!database.was_told("-- file 0"));
    assert!(database.was_told(ROLLBACK));
}

#[test]
fn a_breaking_pending_migration_is_refused_only_where_compatible_ones_alone_may_run() {
    // The version before the last breaking migration: that one is pending.
    let last_breaking = floor_for(BINARY_SCHEMA);
    let series = series((BINARY_SCHEMA - last_breaking + 1) as usize);
    let database = Scripted::at(last_breaking - 1, 1);
    let error = migrate_series(&database, &series, SchemaState::refuse_breaking).unwrap_err();
    assert!(
        error.to_string().contains("breaking migration(s)"),
        "{error}"
    );
    assert!(!database.was_told(BEGIN));

    let database = Scripted::at(last_breaking - 1, 1);
    assert!(
        migrate_series(&database, &series, any_pending)
            .unwrap()
            .is_some()
    );
    assert!(database.was_told(COMMIT));
}

#[test]
fn a_database_older_than_the_base_cannot_be_brought_forward() {
    let series = series(1);
    let database = Scripted::at(series.base_version - 1, 1);
    let error = migrate_series(&database, &series, any_pending).unwrap_err();
    assert!(error.to_string().contains("older than the base"), "{error}");
    assert!(!database.was_told(BEGIN));
}

/// The base without its comments, its whitespace runs as one space.
fn base_statements() -> String {
    let text: String = SERIES
        .base
        .lines()
        .map(|line| line.split("--").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The columns of each table of the base, each with the rest of its line.
fn base_tables() -> BTreeMap<String, BTreeMap<String, String>> {
    let mut tables = BTreeMap::new();
    for line in SERIES.base.lines() {
        let Some(table) = line
            .strip_prefix("CREATE TABLE ")
            .and_then(|rest| rest.strip_suffix(" ("))
        else {
            continue;
        };
        let start = SERIES.base.find(line).unwrap() + line.len();
        let body = &SERIES.base[start..];
        let body = &body[..body.find("\n);").unwrap()];
        let columns = body
            .lines()
            .map(|line| line.trim().trim_end_matches(','))
            .filter_map(|line| line.split_once(' '))
            .filter(|(word, _)| !matches!(*word, "PRIMARY" | "UNIQUE" | "FOREIGN"))
            .map(|(name, rest)| (name.to_owned(), rest.to_owned()))
            .collect();
        tables.insert(table.to_owned(), columns);
    }
    tables
}

/// The SQLite schema at the base's version, on a database of its own.
fn sqlite_at_the_base() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for migration in &MIGRATIONS[..SERIES.base_version as usize] {
        conn.execute_batch(migration).unwrap();
    }
    conn
}

/// `(table, column, type)` of every column SQLite's schema has at the base's
/// version, without the tables the base does not carry over.
fn sqlite_columns(conn: &rusqlite::Connection) -> Vec<(String, String, String)> {
    conn.prepare(
        "SELECT m.name, c.name, upper(c.type)
         FROM sqlite_master m JOIN pragma_table_info(m.name) c
         WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%'
           AND m.name NOT LIKE 'search_index%' AND m.name <> 'schema_floor'",
    )
    .unwrap()
    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

const BOOLEANS: [(&str, &str); 8] = [
    ("asks", "answer_approval"),
    ("supervisors", "auto_update"),
    ("supervisors", "handoff_accepted"),
    ("session_steps", "background"),
    ("ledger_rows", "final"),
    ("ledger_rows", "legacy"),
    ("run_resources", "is_last"),
    ("tasks", "wait_for_build"),
];

#[test]
fn the_base_has_sqlite_s_columns_at_its_version_in_their_postgres_types() {
    let conn = sqlite_at_the_base();
    let tables = base_tables();
    let mut real = 0;
    for (table, column, ty) in sqlite_columns(&conn) {
        let declared = tables
            .get(&table)
            .and_then(|columns| columns.get(&column))
            .unwrap_or_else(|| panic!("{table}.{column} is not in the base"));
        let is_boolean = BOOLEANS.contains(&(table.as_str(), column.as_str()));
        let holds = match ty.as_str() {
            "REAL" => {
                real += 1;
                declared.starts_with("double precision")
            }
            "INTEGER" if is_boolean => declared.starts_with("boolean"),
            "INTEGER" if (table.as_str(), column.as_str()) == ("write_leader", "singleton") => {
                declared.starts_with("integer")
            }
            "INTEGER" => declared.starts_with("bigint"),
            "TEXT" => declared.starts_with("text") || declared.starts_with("jsonb"),
            _ => false,
        };
        assert!(holds, "{table}.{column} {ty} is `{declared}`");
    }
    assert!(real > 0, "no REAL column was compared");
}

#[test]
fn the_base_has_nothing_sqlite_s_schema_does_not_but_the_order_the_version_and_the_search() {
    let conn = sqlite_at_the_base();
    let columns = sqlite_columns(&conn);
    for (table, declared) in base_tables() {
        if matches!(table.as_str(), "dagq_schema" | "search_index") {
            continue;
        }
        for column in declared.keys() {
            let in_sqlite = columns.iter().any(|(t, c, _)| *t == table && c == column);
            let is_order = column == "seq" && matches!(table.as_str(), "task_runs" | "supervisors");
            assert!(in_sqlite || is_order, "{table}.{column} is not in SQLite");
        }
    }
}

#[test]
fn the_tables_read_in_rowid_order_have_their_insert_order_as_a_unique_identity() {
    let tables = base_tables();
    let statements = base_statements();
    for table in ["task_runs", "supervisors"] {
        assert_eq!(
            tables[table]["seq"],
            "bigint GENERATED BY DEFAULT AS IDENTITY NOT NULL"
        );
        assert!(
            statements.contains(&format!(
                "CREATE UNIQUE INDEX {table}_by_seq ON {table}(seq);"
            )),
            "{table}"
        );
    }
}

#[test]
fn every_id_the_inserts_leave_to_the_database_is_an_identity_kept_on_import() {
    let conn = sqlite_at_the_base();
    let tables = base_tables();
    let autoincrement: Vec<String> = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND sql LIKE '%AUTOINCREMENT%'",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(!autoincrement.is_empty());
    let rowids = [
        "plan_requests",
        "spike_cycles",
        "replan_operations",
        "task_holds",
    ];
    for table in autoincrement.iter().map(String::as_str).chain(rowids) {
        assert_eq!(
            tables[table]["id"], "bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY",
            "{table}"
        );
    }
}

#[test]
fn the_primary_key_with_a_null_is_not_null_columns_and_a_partial_unique_index() {
    let tables = base_tables();
    let mappings = &tables["replan_mappings"];
    assert_eq!(mappings["operation_id"], "bigint NOT NULL");
    assert_eq!(mappings["ordinal"], "bigint NOT NULL");
    assert_eq!(mappings["child_task_id"], "bigint");
    assert!(base_statements().contains(
        "CREATE UNIQUE INDEX replan_mappings_key ON replan_mappings(operation_id, ordinal, child_task_id) \
         WHERE child_task_id IS NOT NULL;"
    ));
    assert!(!section_of("replan_mappings").contains("PRIMARY KEY"));
}

/// The text of the table's `CREATE TABLE`.
fn section_of(table: &str) -> &'static str {
    let open = format!("CREATE TABLE {table} (");
    let start = SERIES.base.find(&open).unwrap();
    let rest = &SERIES.base[start..];
    &rest[..rest.find("\n);").unwrap()]
}

#[test]
fn the_base_has_the_unique_indexes_of_the_invariants() {
    let statements = base_statements();
    for index in [
        "CREATE UNIQUE INDEX one_integrating_run_per_queue ON task_runs((1)) WHERE status = 'integrating';",
        "CREATE UNIQUE INDEX one_unfinished_run_per_task ON task_runs(task_id) WHERE status IN \
         ('claimed', 'starting', 'running', 'validating', 'awaiting_integration', 'integrating', 'needs_session');",
        "CREATE UNIQUE INDEX one_integrated_run_per_task ON task_runs(task_id) WHERE status = 'integrated';",
        "CREATE UNIQUE INDEX plan_reviews_running ON plan_reviews((finished_at IS NULL)) WHERE finished_at IS NULL;",
        "CREATE UNIQUE INDEX asks_open ON asks((coalesce(task_id, 0)), (coalesce(run_id, '')), kind, \
         reason_category, (coalesce(subject, '')), (coalesce(finding_id, 0))) \
         WHERE answered_at IS NULL AND closed_at IS NULL;",
        "CREATE UNIQUE INDEX findings_unsettled ON findings(kind, target, (coalesce(task_id, 0)), \
         (coalesce(run_id, '')), (coalesce(goal_id, 0)), subject) WHERE status IN ('open', 'proposed');",
        "CREATE UNIQUE INDEX write_leader_one_row ON write_leader((1));",
        "CREATE UNIQUE INDEX dagq_schema_one_row ON dagq_schema((1));",
    ] {
        assert!(statements.contains(index), "{index}");
    }
    assert!(section_of("write_leader").contains("singleton integer PRIMARY KEY"));
}

#[test]
fn the_base_has_sqlite_s_indexes_and_makes_the_row_of_the_leader() {
    let conn = sqlite_at_the_base();
    let statements = base_statements();
    let names: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND sql IS NOT NULL")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(!names.is_empty());
    for name in names {
        assert!(statements.contains(&format!(" INDEX {name} ON ")), "{name}");
    }
    assert!(statements.contains(
        "INSERT INTO write_leader (singleton, generation, lease_until_ms) VALUES (1, 0, 0);"
    ));
}

#[test]
fn the_json_indexes_read_as_the_dialect_reads() {
    let statements = base_statements();
    let supervisor = SqlDialect::Postgres.json_field("payload", "$.supervisor", JsonType::Text);
    assert!(statements.contains(&format!(
        "CREATE INDEX events_by_supervisor ON run_events({supervisor}, id) WHERE {supervisor} IS NOT NULL;"
    )));
    let opened = SqlDialect::Postgres.json_field("payload", "$.opened_event_id", JsonType::Integer);
    assert!(statements.contains(&format!(
        "CREATE INDEX events_by_opened_event ON run_events(kind, ({opened}));"
    )));
}

#[test]
fn the_defaults_read_the_clock_as_the_dialect_does() {
    let tables = base_tables();
    let text = crate::infrastructure::sql_dialect::TimeForm::Text;
    let seconds = crate::infrastructure::sql_dialect::TimeForm::EpochSeconds;
    let now = |form| SqlDialect::Postgres.time_value("'now'", form);
    assert_eq!(
        tables["tasks"]["created_at"],
        format!("text NOT NULL DEFAULT ({})", now(text))
    );
    assert_eq!(
        tables["supervisors"]["started_at"].to_lowercase(),
        format!("bigint not null default ({})", now(seconds).to_lowercase())
    );
    assert_eq!(
        tables["tasks"]["wait_for_build"],
        "boolean NOT NULL DEFAULT false"
    );
}

#[test]
fn the_base_has_no_trigger_and_no_check() {
    let statements = base_statements().to_ascii_uppercase();
    for word in ["TRIGGER", "CHECK"] {
        assert!(
            !statements
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .any(|found| found == word),
            "{word}"
        );
    }
}

#[test]
fn every_foreign_key_names_a_table_made_before_it() {
    let mut made = BTreeSet::new();
    for line in SERIES.base.lines() {
        let line = line.trim();
        if let Some(table) = line
            .strip_prefix("CREATE TABLE ")
            .and_then(|rest| rest.strip_suffix(" ("))
        {
            made.insert(table);
        }
        if let Some((_, target)) = line.split_once("REFERENCES ") {
            let target = target.split('(').next().unwrap();
            assert!(made.contains(target), "{line}");
        }
    }
}

#[test]
fn the_series_is_the_directory_and_reaches_this_binary_s_schema() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let directory = root.join(POSTGRES_DIRECTORY);
    let names: Vec<String> = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    let sqlite: Vec<String> = std::fs::read_dir(root.join(migration_numbers::DIRECTORY))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    let base = std::fs::read_to_string(directory.join(POSTGRES_BASE)).unwrap();
    assert_eq!(base, SERIES.base);
    let version = migration_numbers::postgres_base_version(&base).unwrap();
    assert_eq!(version as i64, SERIES.base_version);
    let series = migration_numbers::postgres_series(
        &migration_numbers::ordered(&sqlite).unwrap(),
        version,
        &names,
    )
    .unwrap();
    assert_eq!(series.len(), SERIES.after.len());
    for (file, name) in SERIES.after.iter().zip(&series) {
        assert_eq!(
            *file,
            std::fs::read_to_string(directory.join(name)).unwrap()
        );
        assert_eq!(
            migration_numbers::spelled_number(file, None),
            None,
            "{name}"
        );
    }
    assert_eq!(migration_numbers::spelled_number(SERIES.base, None), None);
    assert_eq!(SERIES.latest(), BINARY_SCHEMA);
    assert_eq!(
        BINARY_SCHEMA,
        crate::infrastructure::sqlite::SqliteQueue::SCHEMA_VERSION
    );
}
