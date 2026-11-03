//! The rule X4 of docs/design/architecture.md: the store takes the ID of a
//! row it inserted only from the adapter's public inserts (`insert_id`,
//! `insert_id_if_inserted`, `WriteTx::insert_id`; docs/design/persistence.md,
//! "Transactions and constraints"), so that another adapter can read it its
//! own way (a `RETURNING`). Checked on the sources of `src/` as syntax:
//! outside test code (see [`source`](super::source)), the name
//! `last_insert_rowid` (rusqlite's `Connection::last_insert_rowid`) is used
//! only in [`IMPLEMENTATION`], the internal fn under those inserts; a public
//! insert that called it directly fails too.
//!
//! Any use of the name counts: a method call, a path (called or not), and
//! the name in a macro's tokens. Comments and attributes (doc comments
//! too) are not read, and test code is also an expression, a `let`, an arm
//! or a macro statement under a `cfg` that requires `test`. A use is at the
//! innermost fn around it (closures belong to their fn), named
//! `Type::method` in an `impl` or a trait, `-` outside any fn.

use super::source;

/// The adapter's internal fn under its public inserts that runs an insert
/// and reads the inserted row's ID.
const IMPLEMENTATION: (&str, &str) = ("src/infrastructure/sqlite.rs", "insert_counted");

/// The name only [`IMPLEMENTATION`] uses.
const NAME: &str = "last_insert_rowid";

/// Every failure of the files `rust` (paths from the repository root, with
/// their text), one line each.
fn check(rust: &[(String, String)]) -> Vec<String> {
    let (read, mut errors) = source::production_files(rust);
    for (path, file) in &read {
        for function in source::name_uses(file, NAME) {
            if (*path, function.as_str()) == IMPLEMENTATION {
                continue;
            }
            errors.insert(format!(
                "{path}: {NAME} is used in {function} outside {}; take the inserted row's ID from insert_id (WriteTx::insert_id in a transaction)",
                IMPLEMENTATION.1
            ));
        }
    }
    errors.into_iter().collect()
}

#[test]
fn only_the_adapter_s_insert_reads_the_inserted_row_s_id() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let rust = super::files(root, "src", "rs");
    let (path, function) = IMPLEMENTATION;
    let implementation = rust.iter().find(|(p, _)| p == path).unwrap();
    let file = syn::parse_file(&implementation.1).unwrap();
    assert!(
        source::name_uses(&file, NAME).contains(function),
        "{path}: {function} reads the inserted row's ID, and no {NAME} is seen in it"
    );
    let errors = check(&rust);
    assert!(
        errors.is_empty(),
        "inserted rows' IDs read outside the adapter's insert (docs/design/persistence.md, Transactions and constraints):\n{}",
        errors.join("\n")
    );
}

mod fixtures {
    use super::*;

    /// The uses this module forbids, one expression each.
    const USES: [&str; 6] = [
        "tx.last_insert_rowid()",
        "conn.last_insert_rowid()",
        "rusqlite::Connection::last_insert_rowid(conn)",
        "Connection::last_insert_rowid",
        "<Connection>::last_insert_rowid(&conn)",
        "self.conn.last_insert_rowid()",
    ];

    /// The forms a use is written in: as it is, and inside a macro's tokens.
    fn forms(call: &str) -> [String; 3] {
        [
            call.to_owned(),
            format!("vec![{call}]"),
            format!("format!(\"{{:?}}\", {call})"),
        ]
    }

    fn errors(files: &[(&str, &str)]) -> Vec<String> {
        let rust: Vec<(String, String)> = files
            .iter()
            .map(|(path, text)| (path.to_string(), text.to_string()))
            .collect();
        check(&rust)
    }

    #[test]
    fn each_use_outside_the_adapter_s_insert_fails() {
        for call in USES.iter().flat_map(|c| forms(c)) {
            for (path, text, function) in [
                (
                    "src/infrastructure/store.rs",
                    format!("fn store(conn: &Connection) {{ let _ = {call}; }}"),
                    "store",
                ),
                (
                    "src/infrastructure/store.rs",
                    format!("impl Store {{ fn write(&self) {{ let run = || {call}; run(); }} }}"),
                    "Store::write",
                ),
                (
                    "src/infrastructure/sqlite.rs",
                    format!("fn insert_id(conn: &Connection) {{ let _ = {call}; }}"),
                    "insert_id",
                ),
                (
                    "src/infrastructure/other.rs",
                    format!("fn insert_counted(conn: &Connection) {{ let _ = {call}; }}"),
                    "insert_counted",
                ),
                (
                    "src/infrastructure/sqlite.rs",
                    format!("const ID: fn() = || {{ let _ = {call}; }};"),
                    "-",
                ),
            ] {
                let found = errors(&[(path, &text)]);
                assert_eq!(
                    found,
                    [format!(
                        "{path}: last_insert_rowid is used in {function} outside insert_counted; take the inserted row's ID from insert_id (WriteTx::insert_id in a transaction)"
                    )],
                    "{call} in {path}"
                );
            }
        }
    }

    #[test]
    fn a_use_in_the_adapter_s_insert_or_in_test_code_passes() {
        for call in USES.iter().flat_map(|c| forms(c)) {
            let insert = format!(
                "fn insert_counted(conn: &Connection) -> (usize, i64) {{ (conn.execute(\"INSERT\", [])?, {call}) }}"
            );
            let tests = format!(
                "#[cfg(test)]\nmod tests {{ fn t() {{ let _ = {call}; }} }}\n#[cfg(all(test, unix))]\nfn u() {{ let _ = {call}; }}\nimpl S {{ #[cfg(test)] fn m(&self) {{ let _ = {call}; }} }}\nfn l() {{ #[cfg(test)] let _ = {call}; }}\nfn b() {{ #[cfg(test)] {{ let _ = {call}; }} }}\nfn s() {{ #[cfg(test)] println!(\"{{:?}}\", {call}); }}\n#[cfg(test)]\nmod declared;"
            );
            let declared = format!("fn d() {{ let _ = {call}; }}");
            let test_file = format!("#![cfg(test)]\nfn f() {{ let _ = {call}; }}");
            let found = errors(&[
                ("src/infrastructure/sqlite.rs", &insert),
                ("src/infrastructure/store.rs", &tests),
                ("src/infrastructure/store/declared.rs", &declared),
                ("src/infrastructure/fixtures.rs", &test_file),
            ]);
            assert!(found.is_empty(), "{call}: {found:?}");
        }
    }

    #[test]
    fn what_uses_no_last_insert_rowid_passes() {
        let found = errors(&[(
            "src/infrastructure/store.rs",
            "/// Not `last_insert_rowid`.\n#[doc = \"last_insert_rowid\"]\nfn f(tx: &WriteTx, conn: &Connection) {\n    // conn.last_insert_rowid()\n    let id = tx.insert_id(\"INSERT INTO t VALUES (1)\", [])?;\n    let other = insert_id(conn, \"SELECT 'last_insert_rowid'\", [])?;\n    let row_id = conn.last_insert_row_id();\n    println!(\"last_insert_rowid {id}\");\n}",
        )]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_file_not_read_as_rust_fails() {
        let found = errors(&[("src/infrastructure/store.rs", "fn f( {")]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].starts_with("src/infrastructure/store.rs: not read as Rust: "),
            "{found:?}"
        );
    }
}
