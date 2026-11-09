//! The queue store's SQL spells what both dialects take: no `INSERT OR` or
//! `REPLACE INTO` (`ON CONFLICT ... DO NOTHING` / `DO UPDATE`), no `UPDATE OR`,
//! no `ifnull(` (`COALESCE`), and no `IS` or `IS NOT` that compares with a `?`,
//! a value or a column (`IS NOT DISTINCT FROM`, `IS DISTINCT FROM`), as
//! ADR-t3982-1 decision 3 has it. Checked on the sources of `src/` as syntax:
//! outside test code (see [`source`](super::source)), no string literal (byte
//! strings and those inside a macro's tokens too) spells one, as
//! [`sql::unshared_spellings`] reads it. `SqlDialect::index_key` in the
//! dialect's module (see [`DIALECT`]) is the one place `ifnull(` may stay: the
//! key of an expression index that SQLite's migrations spell with it; any other
//! spelling, and `ifnull(` in another fn of the module, fails there too.
//!
//! What is not seen: SQL built at run time from pieces that split the
//! spelling, a piece of a statement with no keyword in capitals that begins
//! no statement, whose `is` before anything but a `?` or a value in quotes
//! reads as prose, and the migrations (`migrations/`, not under `src/`),
//! whose SQL stays the dialect's own.

use super::covers;
use super::dialect::DIALECT;
use super::source;
use super::sql;
use std::collections::BTreeSet;

/// The fn of the dialect's module that returns the key of an expression index.
const INDEX_KEY: &str = "index_key";

/// Every failure of the files `rust` (paths from the repository root, with
/// their text), one line each.
fn check(rust: &[(String, String)]) -> Vec<String> {
    let mut errors = BTreeSet::new();
    for (path, source) in source::read_production(rust, &mut errors) {
        for literal in &source.literals {
            let inner = literal.fns.last().map_or("-", String::as_str);
            for spelling in sql::unshared_spellings(&literal.text) {
                if spelling == "ifnull("
                    && DIALECT.iter().any(|dialect| covers(dialect, path))
                    && literal.fns.iter().any(|f| f == INDEX_KEY)
                {
                    continue;
                }
                errors.insert(format!(
                    "{path}: SQL in {inner} spells {spelling}, which PostgreSQL does not take; write the shared spelling (ON CONFLICT, IS [NOT] DISTINCT FROM, COALESCE)"
                ));
            }
        }
    }
    errors.into_iter().collect()
}

#[test]
fn the_sql_of_src_spells_what_both_dialects_take() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let errors = check(&super::files(root, "src", "rs"));
    assert!(
        errors.is_empty(),
        "SQL that only SQLite takes (docs/design/persistence.md):\n{}",
        errors.join("\n")
    );
}

mod fixtures {
    use super::*;

    fn errors(files: &[(&str, &str)]) -> Vec<String> {
        let rust: Vec<(String, String)> = files
            .iter()
            .map(|(path, text)| (path.to_string(), text.to_string()))
            .collect();
        check(&rust)
    }

    /// Each kind of violation, in the spellings a statement may give it.
    const VIOLATIONS: [(&str, &str); 25] = [
        ("INSERT OR IGNORE INTO t VALUES (1)", "INSERT OR"),
        ("INSERT OR {} INTO t VALUES (1)", "INSERT OR"),
        ("UPDATE OR {choice} t SET a = 1", "UPDATE OR"),
        ("REPLACE INTO t VALUES (1)", "REPLACE INTO"),
        ("UPDATE OR IGNORE t SET a = 1", "UPDATE OR"),
        ("insert or replace into t values (1)", "INSERT OR"),
        ("SELECT ifnull(a, 0) FROM t", "ifnull("),
        ("SELECT IFNULL (a, 0) FROM t", "ifnull("),
        ("SELECT 1 FROM t WHERE a IS ?1", "IS"),
        ("SELECT 1 FROM t WHERE a IS b", "IS"),
        ("SELECT 1 FROM t WHERE a IS NOT ?1", "IS NOT"),
        ("SELECT 1 FROM t WHERE a IS NOT 'x'", "IS NOT"),
        ("SELECT 1 FROM t WHERE a is not b IS NOT 1", "IS NOT"),
        ("SELECT a FROM t WHERE a is b", "IS"),
        ("SELECT a FROM t WHERE a is not 1", "IS NOT"),
        ("select a from t where a is b", "IS"),
        ("select a from t where a is not 1", "IS NOT"),
        ("select 1 is 1", "IS"),
        ("select 1 is not 1", "IS NOT"),
        ("with t(a,b) as (values(1,2)) select a is b from t", "IS"),
        ("explain select 1 is 1", "IS"),
        ("explain query plan select 1 is not 1", "IS NOT"),
        ("explain insert or ignore into t values (1)", "INSERT OR"),
        ("create view v as select a is b from t", "IS"),
        ("create index i on t (a) where a is not 1", "IS NOT"),
    ];

    /// The forms SQL is written in: a literal, a raw literal, a byte string
    /// and a literal inside a macro's tokens.
    fn forms(sql: &str) -> [String; 4] {
        [
            format!("conn.query_row(\"{sql}\", [], f)"),
            format!("conn.query_row(r#\"{sql}\"#, [], f)"),
            format!("conn.execute(b\"{sql}\", [])"),
            format!("vec![\"{sql}\"]"),
        ]
    }

    #[test]
    fn each_spelling_only_sqlite_takes_fails_wherever_it_is() {
        for (sql, name) in VIOLATIONS {
            for statement in forms(sql) {
                for path in [
                    "src/infrastructure/store.rs",
                    "src/infrastructure/sql_dialect.rs",
                    "src/application/report.rs",
                ] {
                    let text = format!("fn store(conn: &Connection) {{ let _ = {statement}; }}");
                    let found = errors(&[(path, &text)]);
                    assert_eq!(found.len(), 1, "{statement} in {path}: {found:?}");
                    assert!(
                        found[0].starts_with(&format!("{path}: SQL in store spells {name}, ")),
                        "{statement}: {found:?}"
                    );
                }
            }
        }
    }

    /// `ifnull(` stays in the dialect's `index_key` only: in another fn of
    /// the module, and in an `index_key` elsewhere, it fails.
    #[test]
    fn ifnull_stays_only_in_the_dialect_s_index_key() {
        let index_key =
            "impl SqlDialect { fn index_key(self) -> String { format!(\"ifnull(a,0)\") } }";
        let other = "impl SqlDialect { fn key(self) -> String { format!(\"ifnull(a,0)\") } }";
        for (path, text, fails) in [
            ("src/infrastructure/sql_dialect.rs", index_key, false),
            ("src/infrastructure/sql_dialect/keys.rs", index_key, false),
            ("src/infrastructure/sql_dialect.rs", other, true),
            ("src/infrastructure/store.rs", index_key, true),
        ] {
            let found = errors(&[(path, text)]);
            assert_eq!(found.len(), usize::from(fails), "{path}: {text}: {found:?}");
        }
    }

    #[test]
    fn the_same_spelling_in_test_code_passes() {
        for (sql, _) in VIOLATIONS {
            for statement in forms(sql) {
                let used = format!("fn f(conn: &Connection) {{ let _ = {statement}; }}");
                let tests = format!(
                    "#[cfg(test)]\nmod tests {{ fn t() {{ let _ = {statement}; }} }}\n#[cfg(test)]\nmod declared;"
                );
                let test_file = format!("#![cfg(test)]\n{used}");
                let found = errors(&[
                    ("src/infrastructure/store.rs", &tests),
                    ("src/infrastructure/store/declared.rs", &used),
                    ("src/infrastructure/fixtures.rs", &test_file),
                ]);
                assert!(found.is_empty(), "{statement}: {found:?}");
            }
        }
    }

    #[test]
    fn what_both_dialects_take_passes() {
        let found = errors(&[(
            "src/infrastructure/store.rs",
            "/// Not `INSERT OR IGNORE`, `a IS ?1` or `ifnull(a,0)`.\nfn f(conn: &Connection) {\n    // a IS NOT ?1\n    conn.execute(\"INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING\", []);\n    conn.query_row(\"SELECT COALESCE(a, 0) FROM t WHERE a IS NULL AND b IS NOT NULL AND c IS NOT DISTINCT FROM ?1 AND d IS DISTINCT FROM 'x'\", [], f);\n    let message = \"the run is closed and it is not ready\";\n}",
        )]);
        assert!(found.is_empty(), "{found:?}");
    }
}
