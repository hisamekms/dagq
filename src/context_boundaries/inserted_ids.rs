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

use super::source::{self, expr_attrs, is_test, item_attrs};
use super::{children_dir, covers};
use proc_macro2::TokenTree;
use std::collections::BTreeSet;
use syn::visit::{self, Visit};

/// The adapter's internal fn under its public inserts that runs an insert
/// and reads the inserted row's ID.
const IMPLEMENTATION: (&str, &str) = ("src/infrastructure/sqlite.rs", "insert_counted");

/// The name only [`IMPLEMENTATION`] uses.
const NAME: &str = "last_insert_rowid";

/// Every failure of the files `rust` (paths from the repository root, with
/// their text), one line each.
fn check(rust: &[(String, String)]) -> Vec<String> {
    let mut errors = BTreeSet::new();
    let mut read = Vec::new();
    let mut test_paths = Vec::new();
    for (path, text) in rust {
        let parsed = syn::parse_file(text).and_then(|file| Ok((source::read(text)?, file)));
        match parsed {
            Ok((source, file)) => {
                let dir = children_dir(path);
                for module in &source.test_modules {
                    test_paths.push(format!("{dir}/{module}.rs"));
                    test_paths.push(format!("{dir}/{module}/"));
                }
                read.push((path.as_str(), file));
            }
            Err(error) => {
                errors.insert(format!("{path}: not read as Rust: {error}"));
            }
        }
    }
    read.retain(|(path, _)| !test_paths.iter().any(|test| covers(test, path)));
    for (path, file) in &read {
        for function in uses(file) {
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

/// The fns of `file` that use [`NAME`] outside its test code.
fn uses(file: &syn::File) -> BTreeSet<String> {
    let mut reader = Reader::default();
    if !is_test(&file.attrs) {
        reader.visit_file(file);
    }
    reader.found
}

#[derive(Default)]
struct Reader {
    /// The self type of each `impl` (or the trait) around, innermost last.
    owners: Vec<String>,
    fns: Vec<String>,
    found: BTreeSet<String>,
}

impl Reader {
    fn in_fn(&mut self, name: &syn::Ident, method: bool, walk: impl FnOnce(&mut Self)) {
        let name = match self.owners.last() {
            Some(owner) if method => format!("{owner}::{name}"),
            _ => name.to_string(),
        };
        self.fns.push(name);
        walk(self);
        self.fns.pop();
    }

    fn in_owner(&mut self, owner: String, walk: impl FnOnce(&mut Self)) {
        self.owners.push(owner);
        walk(self);
        self.owners.pop();
    }

    fn found(&mut self) {
        let function = self.fns.last().cloned().unwrap_or_else(|| "-".to_owned());
        self.found.insert(function);
    }

    /// The tokens of a macro, which syn does not parse, in its groups too.
    fn tokens(&mut self, tokens: proc_macro2::TokenStream) {
        for tree in tokens {
            match tree {
                TokenTree::Group(group) => self.tokens(group.stream()),
                TokenTree::Ident(ident) if ident == NAME => self.found(),
                _ => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for Reader {
    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {}

    fn visit_ident(&mut self, ident: &'ast proc_macro2::Ident) {
        if ident == NAME {
            self.found();
        }
    }

    fn visit_item(&mut self, item: &'ast syn::Item) {
        if !is_test(item_attrs(item)) {
            visit::visit_item(self, item);
        }
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let owner = match &*i.self_ty {
            syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
            _ => None,
        };
        self.in_owner(owner.unwrap_or_else(|| "_".to_owned()), |r| {
            visit::visit_item_impl(r, i)
        });
    }

    fn visit_item_trait(&mut self, t: &'ast syn::ItemTrait) {
        self.in_owner(t.ident.to_string(), |r| visit::visit_item_trait(r, t));
    }

    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        self.in_fn(&f.sig.ident, false, |r| visit::visit_item_fn(r, f));
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        let attrs = match item {
            syn::ImplItem::Const(i) => &i.attrs,
            syn::ImplItem::Fn(i) => &i.attrs,
            syn::ImplItem::Type(i) => &i.attrs,
            syn::ImplItem::Macro(i) => &i.attrs,
            _ => return visit::visit_impl_item(self, item),
        };
        if !is_test(attrs) {
            visit::visit_impl_item(self, item);
        }
    }

    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        self.in_fn(&f.sig.ident, true, |r| visit::visit_impl_item_fn(r, f));
    }

    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        let attrs = match item {
            syn::TraitItem::Const(i) => &i.attrs,
            syn::TraitItem::Fn(i) => &i.attrs,
            syn::TraitItem::Type(i) => &i.attrs,
            syn::TraitItem::Macro(i) => &i.attrs,
            _ => return visit::visit_trait_item(self, item),
        };
        if !is_test(attrs) {
            visit::visit_trait_item(self, item);
        }
    }

    fn visit_trait_item_fn(&mut self, f: &'ast syn::TraitItemFn) {
        self.in_fn(&f.sig.ident, true, |r| visit::visit_trait_item_fn(r, f));
    }

    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        if !is_test(&arm.attrs) {
            visit::visit_arm(self, arm);
        }
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        if !is_test(&local.attrs) {
            visit::visit_local(self, local);
        }
    }

    fn visit_expr(&mut self, expr: &'ast syn::Expr) {
        if !is_test(expr_attrs(expr)) {
            visit::visit_expr(self, expr);
        }
    }

    fn visit_stmt_macro(&mut self, m: &'ast syn::StmtMacro) {
        if !is_test(&m.attrs) {
            visit::visit_stmt_macro(self, m);
        }
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        self.tokens(m.tokens.clone());
    }
}

#[test]
fn only_the_adapter_s_insert_reads_the_inserted_row_s_id() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let rust = super::files(root, "src", "rs");
    let (path, function) = IMPLEMENTATION;
    let implementation = rust.iter().find(|(p, _)| p == path).unwrap();
    let file = syn::parse_file(&implementation.1).unwrap();
    assert!(
        uses(&file).contains(function),
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
