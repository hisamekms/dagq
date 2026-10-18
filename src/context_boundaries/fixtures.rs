//! The check on small trees: one that matches its records, and one
//! violation of each kind put into it.

use super::ownership::Context::{Execution, Planning, Shared};
use super::ownership::{Allowed, Fix, Store, Table, Transaction};
use super::{Records, Tree, check};

const MIGRATIONS: &str =
    "CREATE TABLE tasks (id INTEGER); CREATE TABLE IF NOT EXISTS task_runs (id TEXT);
CREATE TABLE run_leases (run_id TEXT);
CREATE TABLE asks (id INTEGER);\n";

const SQLITE: &str = r##"
pub fn add(conn: &Conn) {
    conn.execute("INSERT INTO tasks(id) VALUES (1)
        ON CONFLICT(id) DO UPDATE SET id = excluded.id");
}
pub fn claim_task(conn: &Conn) {
    let run = || conn.execute(r#"INSERT INTO task_runs(id) VALUES ("a")"#);
    conn.execute("INSERT INTO asks(id) VALUES (1)");
}
// UPDATE task_runs SET id = 1 in a comment does not count.
/// "UPDATE task_runs SET id = 1" in a doc comment does not count.
fn prose() -> &'static str { "the update of task_runs" }
#[cfg(test)]
mod tests { fn t(c: &Conn) { c.execute("DELETE FROM run_leases"); } }
#[cfg(test)]
mod helpers;
"##;

const RUNS: &str =
    r#"pub fn lease(conn: &Conn) { conn.execute("UPDATE run_leases SET run_id = 1"); }"#;

const TABLES: &[Table] = &[
    Table {
        name: "tasks",
        owner: Planning,
    },
    Table {
        name: "task_runs",
        owner: Execution,
    },
    Table {
        name: "run_leases",
        owner: Execution,
    },
    Table {
        name: "asks",
        owner: Shared,
    },
];

const STORES: &[Store] = &[
    Store {
        path: "src/infrastructure/sqlite.rs",
        context: Planning,
    },
    Store {
        path: "src/infrastructure/runs/",
        context: Execution,
    },
];

const CLAIM: Transaction = Transaction {
    id: "T1",
    writer: Execution,
    path: "src/infrastructure/sqlite.rs",
    function: "claim_task",
    reason: "fixture",
};

fn tree(extra: &[(&str, &str)]) -> Tree {
    let mut rust = vec![
        ("src/infrastructure/sqlite.rs".to_owned(), SQLITE.to_owned()),
        // A file the #[cfg(test)] mod declares is test code.
        (
            "src/infrastructure/sqlite/helpers.rs".to_owned(),
            r#"fn h(c: &Conn) { c.execute("DELETE FROM run_leases"); }"#.to_owned(),
        ),
        (
            "src/infrastructure/runs/lease.rs".to_owned(),
            RUNS.to_owned(),
        ),
    ];
    for (path, text) in extra {
        match rust.iter_mut().find(|(p, _)| p == path) {
            Some((_, old)) => old.push_str(text),
            None => rust.push(((*path).to_owned(), (*text).to_owned())),
        }
    }
    Tree {
        rust,
        migrations: MIGRATIONS.to_owned(),
    }
}

fn run(
    tree: &Tree,
    tables: &[Table],
    transactions: &[Transaction],
    allowed: &[Allowed],
) -> Vec<String> {
    check(
        tree,
        &Records {
            tables,
            stores: STORES,
            transactions,
            allowed,
        },
    )
}

fn assert_fails(errors: &[String], want: &str) {
    assert!(
        errors.iter().any(|e| e.contains(want)),
        "want an error with {want:?}, got {errors:#?}"
    );
}

#[test]
fn a_tree_that_matches_its_records_passes() {
    assert_eq!(run(&tree(&[]), TABLES, &[CLAIM], &[]), Vec::<String>::new());
}

#[test]
fn a_write_to_another_contexts_table_outside_a_transaction_fails_unless_allowed() {
    let touched = tree(&[(
        "src/infrastructure/sqlite.rs",
        "fn touch(c: &Conn) { c.execute(\"UPDATE OR IGNORE main.run_leases AS l SET run_id = 2\"); }\n",
    )]);
    let errors = run(&touched, TABLES, &[CLAIM], &[]);
    assert_eq!(
        errors,
        [
            "src/infrastructure/sqlite.rs: C1 forbids run_leases in touch; fix it, or add a row to ownership::ALLOWED with its fix and reason"
        ]
    );
    let allow = Allowed {
        path: "src/infrastructure/sqlite.rs",
        reference: "run_leases in touch",
        fix: Fix::Unregistered,
        reason: "fixture",
    };
    assert!(run(&touched, TABLES, &[CLAIM], &[allow]).is_empty());
    let stale = Allowed {
        path: "src/infrastructure/sqlite.rs",
        reference: "run_leases in gone",
        fix: Fix::Tasks(&[1]),
        reason: "fixture",
    };
    assert_fails(
        &run(&tree(&[]), TABLES, &[CLAIM], &[stale]),
        "stale row src/infrastructure/sqlite.rs run_leases in gone",
    );
}

#[test]
fn a_transaction_covers_only_the_fns_of_its_name_in_its_file() {
    // claim_task of another file is not the transaction.
    let errors = run(
        &tree(&[(
            "src/infrastructure/runs/lease.rs",
            "fn claim_task(c: &Conn) { c.execute(\"DELETE FROM tasks\"); }\n",
        )]),
        TABLES,
        &[CLAIM],
        &[],
    );
    assert_fails(
        &errors,
        "src/infrastructure/runs/lease.rs: C1 forbids tasks in claim_task",
    );
}

#[test]
fn a_transaction_row_the_code_does_not_have_fails_and_its_writes_count() {
    let gone = Transaction {
        function: "claim_gone",
        ..CLAIM
    };
    let errors = run(&tree(&[]), TABLES, &[gone], &[]);
    assert_fails(
        &errors,
        "C7: src/infrastructure/sqlite.rs has no fn claim_gone",
    );
    assert_fails(&errors, "C1 forbids task_runs in claim_task");
    // A fn of the name only in test code is not one.
    let test_only = tree(&[(
        "src/infrastructure/runs/lease.rs",
        "#[cfg(test)]\nfn renew() {}\n",
    )]);
    let renew = Transaction {
        path: "src/infrastructure/runs/lease.rs",
        function: "renew",
        ..CLAIM
    };
    assert_fails(
        &run(&test_only, TABLES, &[CLAIM, renew], &[]),
        "C7: src/infrastructure/runs/lease.rs has no fn renew",
    );
}

#[test]
fn the_rows_of_one_transaction_name_one_writer_and_a_reason() {
    let lease = Transaction {
        id: "T1",
        writer: Planning,
        path: "src/infrastructure/runs/lease.rs",
        function: "lease",
        reason: " ",
    };
    let errors = run(&tree(&[]), TABLES, &[CLAIM, lease], &[]);
    assert_fails(&errors, "X1: the transaction T1 names two writers");
    assert_fails(&errors, "X1: a transaction needs a reason");
}

#[test]
fn a_table_written_with_no_row_fails() {
    let errors = run(
        &tree(&[(
            "src/infrastructure/runs/lease.rs",
            "fn plan(c: &Conn) { c.execute(\"INSERT INTO goals VALUES (1)\"); }\n",
        )]),
        TABLES,
        &[CLAIM],
        &[],
    );
    assert_fails(
        &errors,
        "src/infrastructure/runs/lease.rs: C7: the table goals written in plan is in no row of ownership::TABLES",
    );
}

#[test]
fn a_file_whose_sql_writes_in_no_store_fails() {
    let errors = run(
        &tree(&[(
            "src/infrastructure/extra.rs",
            "fn f(c: &Conn) { c.execute(\"UPDATE tasks NOT INDEXED SET id = 1\"); }\n",
        )]),
        TABLES,
        &[CLAIM],
        &[],
    );
    assert_fails(
        &errors,
        "src/infrastructure/extra.rs: C7: SQL here writes a table, but the file is in no row of ownership::STORES",
    );
}

#[test]
fn the_table_rows_match_the_tables_the_migrations_leave() {
    let mut tables = TABLES.to_vec();
    tables.push(Table {
        name: "ghosts",
        owner: Planning,
    });
    assert_fails(
        &run(&tree(&[]), &tables, &[CLAIM], &[]),
        "ownership::TABLES: C7: no migration creates the table ghosts",
    );
    let mut left = tree(&[]);
    left.migrations.push_str(
        "CREATE TABLE questions_old (id INTEGER); ALTER TABLE questions_old RENAME TO questions;\n",
    );
    assert_fails(
        &run(&left, TABLES, &[CLAIM], &[]),
        "migrations: C7: the table questions the migrations leave is in no row of ownership::TABLES",
    );
    // A table rebuilt under a temporary name, or dropped, is not left.
    let mut rebuilt = tree(&[]);
    rebuilt.migrations.push_str(
        "CREATE TABLE tasks_new (id INTEGER); DROP TABLE tasks; ALTER TABLE tasks_new RENAME TO tasks;
         CREATE TABLE gone (id INTEGER); DROP TABLE IF EXISTS gone;\n",
    );
    assert!(run(&rebuilt, TABLES, &[CLAIM], &[]).is_empty());
}

#[test]
fn a_store_row_of_no_file_fails() {
    let mut stores = STORES.to_vec();
    stores.push(Store {
        path: "src/infrastructure/gone.rs",
        context: Planning,
    });
    let errors = check(
        &tree(&[]),
        &Records {
            tables: TABLES,
            stores: &stores,
            transactions: &[CLAIM],
            allowed: &[],
        },
    );
    assert_fails(
        &errors,
        "ownership::STORES: C7: src/infrastructure/gone.rs is no Rust file or directory of one",
    );
}
