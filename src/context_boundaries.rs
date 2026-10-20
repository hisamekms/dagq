//! The rules C1, C7 and X1 of docs/design/architecture.md, checked on the
//! sources as syntax: the SQL of a store module of `src/infrastructure/`
//! writes only its own context's tables and the shared ones, another
//! context's table only inside a transaction's function, and the ownership
//! table ([`ownership`]) names exactly the tables the migrations leave and
//! the stores and functions the code has.
//!
//! What counts:
//!
//! - SQL is read in the string literals (byte strings and those inside a
//!   macro's tokens too) of the production code of the files under
//!   `src/infrastructure/`, decoded as Rust decodes them and read as SQLite
//!   reads them ([`sql::writes`]). SQL built at run time (a table from a
//!   variable or `format!`) is not seen, nor what the migrations' triggers
//!   write (`search_index`, `landed_commits`).
//! - A write is inside a transaction when a fn around it (at any depth,
//!   closures included) has a row of [`ownership::TRANSACTIONS`] for its
//!   file; a row covers every fn of its name in the file. SQL a
//!   transaction's function runs through a helper fn outside its body
//!   counts in the helper.
//! - A violation is `<table> in <fn>` in its file (the innermost fn, `-`
//!   outside any). Every violation has a row of [`ownership::ALLOWED`], and
//!   every row still matches one, so the change that fixes a violation
//!   removes its row.
//! - Test code (see [`source`]) does not count.
//!
//! The rules L1, L2, L3, L6, L8, L9, C5 and C8 on the module references,
//! and C7 for the records of the ports and of the modules of
//! `src/application/`, are checked by [`modules`] with the same table.
//!
//! This is test code of the library (a `#[cfg(test)]` module of
//! `src/lib.rs`, run by `cargo test --lib`), outside the library's layers,
//! and reads the sources and the migrations as files.

mod fixtures;
mod module_fixtures;
mod modules;
mod ownership;
mod port_fixtures;
mod source;
mod sql;

use ownership::{Allowed, Context, Fix, Store, Table, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// What the check reads: the Rust files of `src/infrastructure/` with their
/// paths from the repository root, and the migrations' text in their order.
struct Tree {
    rust: Vec<(String, String)>,
    migrations: String,
}

/// The ownership the check matches the tree against.
struct Records<'a> {
    tables: &'a [Table],
    stores: &'a [Store],
    transactions: &'a [Transaction],
    allowed: &'a [Allowed],
}

/// The directory that holds the files of the modules `file` declares.
fn children_dir(file: &str) -> String {
    let (dir, name) = file.rsplit_once('/').unwrap_or(("", file));
    let stem = name.trim_end_matches(".rs");
    if ["mod", "lib", "main"].contains(&stem) {
        dir.to_owned()
    } else {
        format!("{dir}/{stem}")
    }
}

fn covers(record: &str, file: &str) -> bool {
    record == file || (record.ends_with('/') && file.starts_with(record))
}

/// The context of `file` by the longest store path that covers it.
fn store_of(stores: &[Store], file: &str) -> Option<Context> {
    stores
        .iter()
        .filter(|s| covers(s.path, file))
        .max_by_key(|s| s.path.len())
        .map(|s| s.context)
}

/// Every failure of the tree against the records, one line each.
fn check(tree: &Tree, records: &Records) -> Vec<String> {
    let mut errors = BTreeSet::new();
    let owners = tables(records.tables, &tree.migrations, &mut errors);
    stores(records.stores, &tree.rust, &mut errors);
    let mut sources = BTreeMap::new();
    let mut test_paths = Vec::new();
    for (path, text) in &tree.rust {
        match source::read(text) {
            Ok(read) => {
                let dir = children_dir(path);
                for module in &read.test_modules {
                    test_paths.push(format!("{dir}/{module}.rs"));
                    test_paths.push(format!("{dir}/{module}/"));
                }
                sources.insert(path.as_str(), read);
            }
            Err(error) => {
                errors.insert(format!("{path}: not read as Rust: {error}"));
            }
        }
    }
    sources.retain(|path, _| !test_paths.iter().any(|test| covers(test, path)));
    transactions(records.transactions, &sources, &mut errors);
    let mut found = BTreeSet::new();
    for (path, read) in &sources {
        let context = store_of(records.stores, path);
        for literal in &read.literals {
            for table in sql::writes(&literal.text) {
                let inner = literal.fns.last().map_or("-", String::as_str);
                let Some(&owner) = owners.get(table.as_str()) else {
                    errors.insert(format!(
                        "{path}: C7: the table {table} written in {inner} is in no row of ownership::TABLES"
                    ));
                    continue;
                };
                let Some(context) = context else {
                    errors.insert(format!(
                        "{path}: C7: SQL here writes a table, but the file is in no row of ownership::STORES"
                    ));
                    continue;
                };
                if owner == context || owner == Context::Shared {
                    continue;
                }
                let in_transaction = literal.fns.iter().any(|f| {
                    records
                        .transactions
                        .iter()
                        .any(|t| t.path == *path && t.function == f)
                });
                if !in_transaction {
                    found.insert((path.to_string(), format!("{table} in {inner}")));
                }
            }
        }
    }
    allowed(records.allowed, &found, &mut errors);
    errors.into_iter().collect()
}

/// C7 for the tables: each row's table is created by some migration, and
/// each table the migrations leave has a row. Returns the owners.
fn tables<'a>(
    rows: &'a [Table],
    migrations: &str,
    errors: &mut BTreeSet<String>,
) -> BTreeMap<&'a str, Context> {
    let (created, kept) = sql::migrated_tables(migrations);
    let mut owners = BTreeMap::new();
    for row in rows {
        if owners.insert(row.name, row.owner).is_some() {
            errors.insert(format!(
                "ownership::TABLES: the table {} has two rows",
                row.name
            ));
        }
        if !created.contains(row.name) {
            errors.insert(format!(
                "ownership::TABLES: C7: no migration creates the table {}",
                row.name
            ));
        }
    }
    for table in &kept {
        if !owners.contains_key(table.as_str()) {
            errors.insert(format!(
                "migrations: C7: the table {table} the migrations leave is in no row of ownership::TABLES"
            ));
        }
    }
    owners
}

/// C7 for the stores: each row is a path under `src/infrastructure/` of a
/// Rust file or a directory of one, named once.
fn stores(rows: &[Store], rust: &[(String, String)], errors: &mut BTreeSet<String>) {
    let mut seen = BTreeSet::new();
    for row in rows {
        if !seen.insert(row.path) {
            errors.insert(format!("ownership::STORES: {} has two rows", row.path));
        }
        if !row.path.starts_with("src/infrastructure/") {
            errors.insert(format!(
                "ownership::STORES: {} is not under src/infrastructure/",
                row.path
            ));
        } else if !rust.iter().any(|(path, _)| covers(row.path, path)) {
            errors.insert(format!(
                "ownership::STORES: C7: {} is no Rust file or directory of one",
                row.path
            ));
        }
    }
}

/// X1 and C7 for the transactions: each row has an ID `T<n>` and a reason,
/// names a fn of production code its file has, and is named once; the rows
/// of one ID name one writer.
fn transactions(
    rows: &[Transaction],
    sources: &BTreeMap<&str, source::Source>,
    errors: &mut BTreeSet<String>,
) {
    let mut seen = BTreeSet::new();
    let mut writers = BTreeMap::new();
    for row in rows {
        let at = format!(
            "ownership::TRANSACTIONS: {} {} {}",
            row.id, row.path, row.function
        );
        let numbered = row.id.strip_prefix('T').is_some_and(|n| {
            !n.starts_with('0') && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
        });
        if !numbered {
            errors.insert(format!("{at}: X1: want an ID T1, T2, ..."));
        }
        if row.reason.trim().is_empty() {
            errors.insert(format!("{at}: X1: a transaction needs a reason"));
        }
        if !seen.insert((row.path, row.function)) {
            errors.insert(format!("{at}: the function has two rows"));
        }
        if *writers.entry(row.id).or_insert(row.writer) != row.writer {
            errors.insert(format!(
                "{at}: X1: the transaction {} names two writers",
                row.id
            ));
        }
        if !sources
            .get(row.path)
            .is_some_and(|s| s.fns.contains(row.function))
        {
            errors.insert(format!("{at}: C7: {} has no fn {}", row.path, row.function));
        }
    }
}

/// C1 and X1 against the allowed rows: every violation found has a row, and
/// every row still matches a violation.
fn allowed(rows: &[Allowed], found: &BTreeSet<(String, String)>, errors: &mut BTreeSet<String>) {
    let mut listed = BTreeSet::new();
    for row in rows {
        let at = format!("ownership::ALLOWED: {} {}", row.path, row.reference);
        if !row.path.starts_with("src/")
            || row.reference.trim().is_empty()
            || row.reason.trim().is_empty()
        {
            errors.insert(format!(
                "{at}: want a path under src/, a reference and a reason"
            ));
        }
        if matches!(row.fix, Fix::Tasks(tasks) if tasks.is_empty()) {
            errors.insert(format!("{at}: want task IDs, or Fix::Unregistered"));
        }
        if !listed.insert((row.path.to_owned(), row.reference.to_owned())) {
            errors.insert(format!("{at}: listed twice"));
        }
    }
    for (path, reference) in found {
        if !listed.contains(&(path.clone(), reference.clone())) {
            errors.insert(format!(
                "{path}: C1 forbids {reference}; fix it, or add a row to ownership::ALLOWED with its fix and reason"
            ));
        }
    }
    for (path, reference) in &listed {
        if !found.contains(&(path.clone(), reference.clone())) {
            errors.insert(format!(
                "ownership::ALLOWED: stale row {path} {reference} (no such violation any more); remove it"
            ));
        }
    }
}

/// The files under `dir` with extension `ext`, sorted by path, as paths
/// from `root`.
fn files(root: &Path, dir: &str, ext: &str) -> Vec<(String, String)> {
    fn walk(path: &Path, ext: &str, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, ext, out);
            } else if path.extension().is_some_and(|e| e == ext) {
                out.push(path);
            }
        }
    }
    let mut paths = Vec::new();
    walk(&root.join(dir), ext, &mut paths);
    let mut out: Vec<(String, String)> = paths
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).unwrap();
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            (relative, text)
        })
        .collect();
    out.sort();
    out
}

#[test]
fn the_stores_write_their_own_tables_and_the_ownership_matches_the_code() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let migrations = files(root, "migrations", "sql")
        .into_iter()
        .map(|(_, text)| text + "\n")
        .collect();
    let tree = Tree {
        rust: files(root, "src/infrastructure", "rs"),
        migrations,
    };
    let records = Records {
        tables: ownership::TABLES,
        stores: ownership::STORES,
        transactions: ownership::TRANSACTIONS,
        allowed: ownership::ALLOWED,
    };
    let errors = check(&tree, &records);
    assert!(
        errors.is_empty(),
        "the context boundaries (docs/design/architecture.md, C1, C7 and X1):\n{}",
        errors.join("\n")
    );
}
