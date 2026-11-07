//! The SQL functions whose spelling differs between the dialects of the
//! queue store (the JSON functions, the date and time functions, the clock
//! in seconds) are called only from the dialect's functions
//! (docs/design/persistence.md, "責務と境界"; ADR-t3982-1 decisions 3 and
//! 4): a statement takes the expression from
//! `crate::infrastructure::sql_dialect` instead of spelling it. Checked on
//! the sources of `src/` as syntax: outside test code (see
//! [`source`](super::source)) and outside [`DIALECT`], no string literal
//! (byte strings and those inside a macro's tokens too) calls a function
//! of [`FUNCTIONS`], read as SQLite reads it ([`sql::calls`]).
//!
//! Parameters compared with NULL must carry their type through
//! CAST in shared SQL; a bare IS NULL cannot infer a Postgres parameter
//! type. This rule applies inside the dialect module too.
//!
//! What is not seen: SQL built at run time from pieces that split a name
//! from its `(`, the messages of [`NOT_SQL`], and the migrations (`migrations/`, not under `src/`),
//! whose SQL stays the dialect's own.

use super::source;
use super::sql;
use super::{children_dir, covers};
use std::collections::BTreeSet;

/// The module of the dialect's functions, whose SQL spells the functions.
const DIALECT: &[&str] = &[
    "src/infrastructure/sql_dialect.rs",
    "src/infrastructure/sql_dialect/",
];

/// The functions spelled only in [`DIALECT`], in lower case.
const FUNCTIONS: [&str; 34] = [
    "json",
    "jsonb",
    "json_array",
    "jsonb_array",
    "json_array_length",
    "json_each",
    "json_error_position",
    "json_extract",
    "jsonb_extract",
    "json_group_array",
    "jsonb_group_array",
    "json_group_object",
    "jsonb_group_object",
    "json_insert",
    "jsonb_insert",
    "json_object",
    "jsonb_object",
    "json_patch",
    "jsonb_patch",
    "json_pretty",
    "json_quote",
    "json_remove",
    "jsonb_remove",
    "json_replace",
    "jsonb_replace",
    "json_set",
    "jsonb_set",
    "json_tree",
    "json_type",
    "json_valid",
    "datetime",
    "strftime",
    "julianday",
    "unixepoch",
];

/// Messages that read as a call but are no SQL, each with its file: the word
/// `JSON` before a parenthesis. By the whole literal, since a call of `json`
/// spaced, quoted or commented away from its `(` runs in SQLite all the same.
const NOT_SQL: [(&str, &str); 1] = [(
    "src/domain/transcript.rs",
    "none of its lines is JSON ({} skipped)",
)];

/// Every failure of the files `rust` (paths from the repository root, with
/// their text), one line each.
fn check(rust: &[(String, String)]) -> Vec<String> {
    let mut errors = BTreeSet::new();
    let mut read = Vec::new();
    let mut test_paths = Vec::new();
    for (path, text) in rust {
        match source::read(text) {
            Ok(source) => {
                let dir = children_dir(path);
                for module in &source.test_modules {
                    test_paths.push(format!("{dir}/{module}.rs"));
                    test_paths.push(format!("{dir}/{module}/"));
                }
                read.push((path.as_str(), source));
            }
            Err(error) => {
                errors.insert(format!("{path}: not read as Rust: {error}"));
            }
        }
    }
    read.retain(|(path, _)| !test_paths.iter().any(|test| covers(test, path)));
    for (path, source) in &read {
        for literal in &source.literals {
            if NOT_SQL.contains(&(*path, literal.text.as_str())) {
                continue;
            }
            let inner = literal.fns.last().map_or("-", String::as_str);
            for parameter in sql::untyped_null_parameters(&literal.text) {
                errors.insert(format!(
                    "{path}: SQL in {inner} tests untyped {parameter} against NULL; CAST the parameter to its column's type"
                ));
            }
            if DIALECT.iter().any(|dialect| covers(dialect, path)) {
                continue;
            }
            for function in sql::calls(&literal.text, &FUNCTIONS) {
                errors.insert(format!(
                    "{path}: SQL in {inner} calls {function}( outside the dialect's functions; take the expression from crate::infrastructure::sql_dialect"
                ));
            }
        }
    }
    errors.into_iter().collect()
}

#[test]
fn the_sql_of_src_takes_the_dialects_functions_from_sql_dialect() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let rust = super::files(root, "src", "rs");
    assert!(
        rust.iter()
            .any(|(path, _)| DIALECT.iter().any(|dialect| covers(dialect, path))),
        "the dialect's module {} is gone",
        DIALECT[0]
    );
    let errors = check(&rust);
    assert!(
        errors.is_empty(),
        "SQL with dialect functions outside sql_dialect or untyped NULL parameters (docs/design/persistence.md):\n{}",
        errors.join("\n")
    );
    for (path, message) in NOT_SQL {
        assert!(
            rust.iter()
                .any(|(file, text)| file == path && text.contains(message)),
            "{path} no longer has the message {message}: take it off NOT_SQL"
        );
    }
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

    /// A call of each function, in the spellings a statement may give it.
    const CALLS: [(&str, &str); 21] = [
        ("json (?1)", "json"),
        ("[json](?1)", "json"),
        ("`json`(?1)", "json"),
        ("json/*c*/(?1)", "json"),
        ("json_array_length(x)", "json_array_length"),
        ("jsonb_set(p, '{a}', v)", "jsonb_set"),
        ("json_each(?1)", "json_each"),
        ("json_each (r.records, '$.a')", "json_each"),
        ("json_group_array(x)", "json_group_array"),
        ("json_type(payload, '$.a')", "json_type"),
        ("JSON_SET(payload,?3,1)", "json_set"),
        ("json_remove(payload,'$.a')", "json_remove"),
        ("json(?2)", "json"),
        ("\\\"JSON\\\"(?2)", "json"),
        ("unixepoch()", "unixepoch"),
        ("json_extract(payload,'$.kind')", "json_extract"),
        ("JSON_EXTRACT(payload, '$.kind')", "json_extract"),
        ("datetime('now')", "datetime"),
        ("strftime ('%s', created_at)", "strftime"),
        ("julianday(created_at)", "julianday"),
        ("\\\"julianday\\\"(created_at)", "julianday"),
    ];

    /// The forms SQL is written in: a literal, a raw literal, a byte string,
    /// a format string and a literal inside a macro's tokens.
    fn forms(call: &str) -> [String; 5] {
        let raw = call.replace("\\\"", "\"");
        [
            format!("conn.query_row(\"SELECT {call} FROM t\", [], f)"),
            format!("conn.query_row(r#\"SELECT {raw} FROM t\"#, [], f)"),
            format!("conn.execute(b\"SELECT {call}\", [])"),
            format!("conn.prepare(&format!(\"SELECT {{}}, {call}\", x))"),
            format!("vec![\"SELECT {call}\"]"),
        ]
    }

    #[test]
    fn each_call_outside_the_dialect_fails() {
        for (call, name) in CALLS {
            for statement in forms(call) {
                for (path, text) in [
                    (
                        "src/infrastructure/store.rs",
                        format!("fn store(conn: &Connection) {{ let _ = {statement}; }}"),
                    ),
                    (
                        "src/application/report.rs",
                        format!("const SQL: &str = {{ {statement} }};"),
                    ),
                ] {
                    let found = errors(&[(path, &text)]);
                    assert_eq!(found.len(), 1, "{statement} in {path}: {found:?}");
                    assert!(
                        found[0].starts_with(&format!("{path}: SQL in ")),
                        "{statement}: {found:?}"
                    );
                    assert!(
                        found[0].contains(&format!(" calls {name}( ")),
                        "{statement}: {found:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_call_in_the_dialect_or_in_test_code_passes() {
        for (call, _) in CALLS {
            for statement in forms(call) {
                let used = format!("fn f(conn: &Connection) {{ let _ = {statement}; }}");
                let tests = format!(
                    "#[cfg(test)]\nmod tests {{ fn t() {{ let _ = {statement}; }} }}\nfn l() {{ #[cfg(test)] let _ = {statement}; }}\nfn b() {{ #[cfg(test)] {{ let _ = {statement}; }} }}\nfn s() {{ #[cfg(test)] println!(\"{{:?}}\", {statement}); }}\n#[cfg(test)]\nmod declared;"
                );
                let test_file = format!("#![cfg(test)]\n{used}");
                let found = errors(&[
                    ("src/infrastructure/sql_dialect.rs", &used),
                    ("src/infrastructure/sql_dialect/sqlite.rs", &used),
                    ("src/infrastructure/store.rs", &tests),
                    ("src/infrastructure/store/declared.rs", &used),
                    ("src/infrastructure/fixtures.rs", &test_file),
                ]);
                assert!(found.is_empty(), "{statement}: {found:?}");
            }
        }
    }

    /// Only the listed message in its listed file is left alone.
    #[test]
    fn only_a_listed_message_that_reads_as_a_call_passes() {
        let (path, message) = NOT_SQL[0];
        let message = format!("fn f(n: u32) -> String {{ format!(\"{message}\", n) }}");
        assert!(errors(&[(path, &message)]).is_empty());
        let found = errors(&[("src/domain/other.rs", &message)]);
        assert_eq!(found.len(), 1, "{found:?}");
        let found = errors(&[(path, "const SQL: &str = \"SELECT JSON ({} )\";")]);
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn what_calls_no_dialects_function_passes() {
        let found = errors(&[(
            "src/infrastructure/store.rs",
            "/// Reads `json_extract(payload,'$.kind')`.\nfn f(conn: &Connection) {\n    // strftime('%s', x)\n    conn.query_row(&format!(\"SELECT {} FROM t\", json_field(\"payload\", \"$.kind\", JsonType::Text)), [], f);\n    conn.execute(\"SELECT my_json(payload), json, unixepoch, 'json(x)', 'datetime(x)', created_at FROM t -- julianday(x)\", []);\n    let message = \"the strftime of a time\";\n}",
        )]);
        assert!(found.is_empty(), "{found:?}");
    }
}

#[test]
fn untyped_null_parameters_fail_and_casts_and_test_code_pass() {
    for expression in [
        "? IS NULL",
        "? IS NOT NULL",
        "(?) /* c */ IS NOT NULL",
        "?1 IS NULL",
        "?12 is not null",
        "(?2) /* c */ IS NULL",
        "$3 IS NULL",
    ] {
        let text = format!("fn f() {{ let _ = \"SELECT 1 WHERE {expression}\"; }}");
        let errors = check(&[("src/infrastructure/store.rs".into(), text)]);
        assert_eq!(errors.len(), 1, "{expression}: {errors:?}");
        assert!(errors[0].contains("tests untyped"));
    }
    let text = r#"
        fn f() {
            let _ = "SELECT 1 WHERE CAST(?1 AS BIGINT) IS NULL OR CAST(?2 AS TEXT) IS NOT NULL";
            let _ = "SELECT 1 WHERE CAST(? AS BIGINT) IS NULL OR CAST(? AS TEXT) IS NOT NULL";
            let _ = "SELECT '?3 IS NULL', \"?4 IS NULL\" -- ?5 IS NULL";
        }
        #[cfg(test)] mod tests { fn f() { let _ = "SELECT ?1 IS NULL"; } }
        #[cfg(test)] mod declared;
    "#;
    assert!(
        check(&[
            ("src/infrastructure/store.rs".into(), text.into()),
            (
                "src/infrastructure/store/declared.rs".into(),
                "const SQL: &str = \"SELECT ?1 IS NULL\";".into()
            ),
        ])
        .is_empty()
    );
}
