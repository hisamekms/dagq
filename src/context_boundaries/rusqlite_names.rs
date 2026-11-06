//! The store runs its shared SQL on the execution port
//! (`src/infrastructure/sql_exec/`, ADR-t3982-1 decision 1): outside test code
//! (see [`source`](super::source)), only the files of [`MAY_NAME_RUSQLITE`]
//! name the crate `rusqlite`, so a module that moved to the port does not go
//! back, and a new one starts on the port.
//!
//! Any use of the identifier counts: a `use`, a path and a type, and the
//! name in a macro's tokens. Comments and attributes (doc comments too) are
//! not read. A name imported from a listed module is not seen. Every row
//! still names the crate, so the change that moves a module off rusqlite
//! removes its row: the list only shrinks, down to the SQLite adapter.
//! Test code may open a memdb with rusqlite (ADR-t2895-1).

use super::source;

/// The files that may name rusqlite. The rows but the SQLite adapter's own
/// are store modules that have not moved to the port yet.
#[rustfmt::skip]
const MAY_NAME_RUSQLITE: &[&str] = &[
    "src/infrastructure/draft_planners.rs",
    "src/infrastructure/finding_planners.rs",
    "src/infrastructure/goal_reviews.rs",
    "src/infrastructure/plan_reviews.rs",
    "src/infrastructure/planners.rs",
    "src/infrastructure/related.rs",
    "src/infrastructure/runtime_store/attributes.rs",
    "src/infrastructure/runtime_store/coordination.rs",
    "src/infrastructure/runtime_store/ledger.rs",
    "src/infrastructure/runtime_store/mod.rs",
    "src/infrastructure/runtime_store/node_samples.rs",
    "src/infrastructure/runtime_store/planning_records.rs",
    "src/infrastructure/runtime_store/recovery.rs",
    "src/infrastructure/runtime_store/run_log.rs",
    "src/infrastructure/runtime_store/run_resources.rs",
    "src/infrastructure/runtime_store/session_registry.rs",
    "src/infrastructure/runtime_store/session_steps.rs",
    "src/infrastructure/runtime_store/transitions.rs",
    "src/infrastructure/search.rs",
    "src/infrastructure/sessions.rs",
    "src/infrastructure/sql_exec/sqlite.rs",
    "src/infrastructure/sqlite.rs",
];

const NAME: &str = "rusqlite";

/// Every failure of the files `rust` (paths from the repository root, with
/// their text) against `allowed`, one line each.
fn check(rust: &[(String, String)], allowed: &[&str]) -> Vec<String> {
    let (read, mut errors) = source::production_files(rust);
    let mut named = std::collections::BTreeSet::new();
    for (path, file) in &read {
        if source::name_uses(file, NAME).is_empty() {
            continue;
        }
        named.insert(*path);
        if !allowed.contains(path) {
            errors.insert(format!(
                "{path}: names rusqlite outside rusqlite_names::MAY_NAME_RUSQLITE; run its SQL on the execution port (infrastructure::sql_exec)"
            ));
        }
    }
    for path in allowed {
        if !named.contains(path) {
            errors.insert(format!(
                "rusqlite_names::MAY_NAME_RUSQLITE: stale row {path} (it names rusqlite no more); remove it"
            ));
        }
    }
    errors.into_iter().collect()
}

#[test]
fn only_the_listed_modules_name_rusqlite() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let errors = check(&super::files(root, "src", "rs"), MAY_NAME_RUSQLITE);
    assert!(
        errors.is_empty(),
        "rusqlite named outside its list (docs/design/persistence.md):\n{}",
        errors.join("\n")
    );
}

mod fixtures {
    use super::*;

    const LISTED: &str = "src/infrastructure/listed.rs";

    fn errors(files: &[(&str, &str)]) -> Vec<String> {
        let rust: Vec<(String, String)> = files
            .iter()
            .map(|(path, text)| (path.to_string(), text.to_string()))
            .collect();
        check(&rust, &[LISTED])
    }

    /// The forms a module names the crate in.
    const NAMES: [&str; 6] = [
        "use rusqlite::Connection;",
        "use rusqlite::{params, Row};",
        "fn f(conn: &rusqlite::Connection) {}",
        "fn f() { let _ = rusqlite::Error::QueryReturnedNoRows; }",
        "fn f() { let _ = vec![rusqlite::types::Null]; }",
        "impl Store { fn f(&self) -> rusqlite::Result<()> { Ok(()) } }",
    ];

    #[test]
    fn each_name_outside_the_list_fails() {
        for name in NAMES {
            let found = errors(&[
                (LISTED, "use rusqlite::Connection;"),
                ("src/infrastructure/store.rs", name),
            ]);
            assert_eq!(
                found,
                [
                    "src/infrastructure/store.rs: names rusqlite outside rusqlite_names::MAY_NAME_RUSQLITE; run its SQL on the execution port (infrastructure::sql_exec)"
                ],
                "{name}"
            );
        }
    }

    #[test]
    fn a_name_in_the_list_or_in_test_code_passes() {
        for name in NAMES {
            let tests = format!(
                "#[cfg(test)]\nmod tests {{ {name} }}\n#[cfg(test)]\nmod declared;\n#[cfg(all(test, unix))]\nmod other {{ {name} }}"
            );
            let test_file = format!("#![cfg(test)]\n{name}");
            let found = errors(&[
                (LISTED, name),
                ("src/infrastructure/store.rs", &tests),
                ("src/infrastructure/store/declared.rs", name),
                ("src/infrastructure/fixtures.rs", &test_file),
            ]);
            assert!(found.is_empty(), "{name}: {found:?}");
        }
    }

    #[test]
    fn what_only_mentions_rusqlite_passes() {
        let found = errors(&[
            (LISTED, "use rusqlite::Connection;"),
            (
                "src/infrastructure/store.rs",
                "//! Unlike rusqlite::Connection.\n/// See rusqlite::Row.\nfn f() { let _ = \"rusqlite\"; }",
            ),
        ]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_listed_module_that_names_it_no_more_is_a_stale_row() {
        assert_eq!(
            errors(&[(LISTED, "fn moved() {}")]),
            [
                "rusqlite_names::MAY_NAME_RUSQLITE: stale row src/infrastructure/listed.rs (it names rusqlite no more); remove it"
            ]
        );
    }

    #[test]
    fn a_file_that_does_not_parse_fails() {
        let found = errors(&[(LISTED, "use rusqlite::Connection;"), ("src/x.rs", "fn {")]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].starts_with("src/x.rs: not read as Rust"),
            "{found:?}"
        );
    }
}
