//! Every transaction on the queue DB begins through the named write
//! transaction `WriteTx` (docs/design/persistence.md, "Transactions and
//! constraints"), so that one holding the write lock long is warned of and
//! recorded whatever it writes through. Checked on the sources of `src/` as
//! syntax: outside test code (see [`source`](super::source)), nothing begins
//! a rusqlite transaction but [`IMPLEMENTATION`] and the rows of
//! [`EXCEPTIONS`].
//!
//! What counts as a begin:
//!
//! - a method call `.transaction_with_behavior(..)`, `.unchecked_transaction()`
//!   or `.transaction()` with no argument (`Connection::transaction`);
//! - a path (called or not) ending in `Transaction::new` or
//!   `Transaction::new_unchecked`, `Transaction` being also any name a `use`
//!   of the file gives it with `as`, or ending in
//!   `transaction_with_behavior`, `unchecked_transaction` or
//!   `Connection::transaction`, the owner of a qualified `<T>::f` or
//!   `<T as Trait>::f` being `T`;
//! - a string literal that is the first argument of `.execute_batch`,
//!   `.execute`, `.prepare`, `.prepare_cached` or `.query_row` and has a
//!   statement `BEGIN` or `SAVEPOINT`.
//!
//! `Connection` is also any name a `use` gives it with `as`, and the SQL is
//! read as SQLite reads it (comments, and a `;` or a keyword inside a name
//! or a value, are no statement; [`begins_transaction`]). The tokens of a
//! macro are read for the same begins (`. name (..)` and `owner :: name`),
//! and test code is also an expression or a macro statement under a `cfg`
//! that requires `test`.
//!
//! A begin is at the innermost fn around it (closures belong to their fn),
//! named `Type::method` in an `impl` or a trait, `-` outside any fn. What
//! is not seen: SQL built at run time, and a test `cfg` inside a macro's
//! tokens.

use super::source::{self, is_test, item_attrs};
use super::sql::begins_transaction;
use super::{children_dir, covers};
use proc_macro2::{Delimiter, TokenTree};
use std::collections::BTreeSet;
use syn::visit::{self, Visit};

/// The implementation of `WriteTx`, whose begin is the one way in.
const IMPLEMENTATION: (&str, &str) = ("src/infrastructure/sqlite.rs", "WriteTx::begin");

/// A fn other than [`IMPLEMENTATION`] that may begin a transaction
/// directly, with why; the place itself carries the reason in a comment.
/// Every row matches a begin, so the change that removes the begin removes
/// its row.
pub(super) struct Exception {
    pub(super) path: &'static str,
    pub(super) function: &'static str,
    pub(super) reason: &'static str,
}

/// None today: every transaction of the queue DB is a `WriteTx`.
const EXCEPTIONS: &[Exception] = &[];

/// A direct begin of a transaction: what began it, in which fn.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Begin {
    function: String,
    what: String,
}

const METHODS: [&str; 2] = ["transaction_with_behavior", "unchecked_transaction"];
const SQL_METHODS: [&str; 5] = [
    "execute_batch",
    "execute",
    "prepare",
    "prepare_cached",
    "query_row",
];

/// Every failure of the files `rust` (paths from the repository root, with
/// their text) against `exceptions`, one line each.
fn check(rust: &[(String, String)], exceptions: &[Exception]) -> Vec<String> {
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
    let mut listed = BTreeSet::new();
    for row in exceptions {
        let at = format!("write_tx::EXCEPTIONS: {} {}", row.path, row.function);
        if !row.path.starts_with("src/")
            || row.function.trim().is_empty()
            || row.reason.trim().is_empty()
        {
            errors.insert(format!("{at}: want a path under src/, a fn and a reason"));
        }
        if !listed.insert((row.path, row.function)) {
            errors.insert(format!("{at}: listed twice"));
        }
    }
    let mut matched = BTreeSet::new();
    for (path, file) in &read {
        for begin in begins(file) {
            if (*path, begin.function.as_str()) == IMPLEMENTATION {
                continue;
            }
            if let Some(row) = exceptions
                .iter()
                .find(|row| row.path == *path && row.function == begin.function)
            {
                matched.insert((row.path, row.function));
                continue;
            }
            errors.insert(format!(
                "{path}: {} begins a transaction in {} outside WriteTx::begin; begin it as a WriteTx (SqliteQueue::write_tx), or add a row with its reason to write_tx::EXCEPTIONS",
                begin.what, begin.function
            ));
        }
    }
    for (path, function) in listed.difference(&matched) {
        errors.insert(format!(
            "write_tx::EXCEPTIONS: stale row {path} {function} (it begins no transaction any more); remove it"
        ));
    }
    errors.into_iter().collect()
}

/// The direct begins of `file` outside its test code.
fn begins(file: &syn::File) -> BTreeSet<Begin> {
    let mut reader = Reader::default();
    if is_test(&file.attrs) {
        return reader.found;
    }
    reader.aliases.insert("Transaction".to_owned());
    reader.connections.insert("Connection".to_owned());
    let mut uses = Uses {
        transactions: &mut reader.aliases,
        connections: &mut reader.connections,
    };
    uses.visit_file(file);
    reader.visit_file(file);
    reader.found
}

/// The names a `use` gives `Transaction` and `Connection` with `as`.
struct Uses<'a> {
    transactions: &'a mut BTreeSet<String>,
    connections: &'a mut BTreeSet<String>,
}

impl<'ast> Visit<'ast> for Uses<'_> {
    fn visit_use_rename(&mut self, rename: &'ast syn::UseRename) {
        if rename.ident == "Transaction" {
            self.transactions.insert(rename.rename.to_string());
        } else if rename.ident == "Connection" {
            self.connections.insert(rename.rename.to_string());
        }
    }
}

#[derive(Default)]
struct Reader {
    /// The names of `Transaction` and of `Connection` in the file.
    aliases: BTreeSet<String>,
    connections: BTreeSet<String>,
    /// The self type of each `impl` (or the trait) around, innermost last.
    owners: Vec<String>,
    fns: Vec<String>,
    found: BTreeSet<Begin>,
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

    fn found(&mut self, what: String) {
        let function = self.fns.last().cloned().unwrap_or_else(|| "-".to_owned());
        self.found.insert(Begin { function, what });
    }

    /// A call of the method `method`, with `no_args` when it has none.
    fn method(&mut self, method: &str, no_args: bool) {
        if METHODS.contains(&method) || (method == "transaction" && no_args) {
            self.found(format!(".{method}()"));
        }
    }

    /// A path whose last two segments are `owner::name`.
    fn path(&mut self, owner: &str, name: &str) {
        let new = self.aliases.contains(owner) && (name == "new" || name == "new_unchecked");
        let method =
            METHODS.contains(&name) || (self.connections.contains(owner) && name == "transaction");
        if new || method {
            self.found(format!("{owner}::{name}"));
        }
    }

    /// The first argument of a call of `method` is the string `sql`.
    fn sql(&mut self, method: &str, sql: &str) {
        if SQL_METHODS.contains(&method) && begins_transaction(sql) {
            self.found(format!(".{method}(\"BEGIN|SAVEPOINT ...\")"));
        }
    }

    /// The tokens of a macro, which syn does not parse: the same begins
    /// read from the tokens (`. name ( .. )`, `owner :: name`), in its
    /// groups too.
    fn tokens(&mut self, tokens: proc_macro2::TokenStream) {
        let trees: Vec<TokenTree> = tokens.into_iter().collect();
        for (at, tree) in trees.iter().enumerate() {
            let next = |n: usize| trees.get(at + n);
            match tree {
                TokenTree::Group(group) => self.tokens(group.stream()),
                TokenTree::Punct(dot) if dot.as_char() == '.' => {
                    let (Some(TokenTree::Ident(name)), Some(TokenTree::Group(args))) =
                        (next(1), next(2))
                    else {
                        continue;
                    };
                    if args.delimiter() != Delimiter::Parenthesis {
                        continue;
                    }
                    let name = name.to_string();
                    let mut args = args.stream().into_iter();
                    let first = args.next();
                    self.method(&name, first.is_none());
                    if let Some(TokenTree::Literal(literal)) = first
                        && let syn::Lit::Str(sql) = syn::Lit::new(literal)
                    {
                        self.sql(&name, &sql.value());
                    }
                }
                TokenTree::Punct(close) if close.as_char() == '>' => {
                    if let (
                        Some(TokenTree::Punct(a)),
                        Some(TokenTree::Punct(b)),
                        Some(TokenTree::Ident(name)),
                    ) = (next(1), next(2), next(3))
                        && a.as_char() == ':'
                        && b.as_char() == ':'
                        && let Some(owner) = qualified_owner(&trees[..at])
                    {
                        self.path(&owner, &name.to_string());
                    }
                }
                TokenTree::Ident(owner) => {
                    if let (
                        Some(TokenTree::Punct(a)),
                        Some(TokenTree::Punct(b)),
                        Some(TokenTree::Ident(name)),
                    ) = (next(1), next(2), next(3))
                        && a.as_char() == ':'
                        && b.as_char() == ':'
                    {
                        self.path(&owner.to_string(), &name.to_string());
                    }
                }
                _ => {}
            }
        }
    }
}

/// The last name of the type `ty` (through references and parentheses).
fn type_name(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        syn::Type::Reference(r) => type_name(&r.elem),
        syn::Type::Paren(p) => type_name(&p.elem),
        syn::Type::Group(g) => type_name(&g.elem),
        _ => None,
    }
}

/// The self type of the qualified path `<T>` or `<T as Trait>` whose `>`
/// ends `before`: the last name before `as` at its level, or else the last
/// one. `None` when its `<` is not found or opens generic arguments
/// (`f::<T>`).
fn qualified_owner(before: &[TokenTree]) -> Option<String> {
    let mut depth = 0;
    let mut names: Vec<String> = Vec::new();
    for (at, tree) in before.iter().enumerate().rev() {
        match tree {
            TokenTree::Punct(p) if p.as_char() == '>' => depth += 1,
            TokenTree::Punct(p) if p.as_char() == '<' && depth > 0 => depth -= 1,
            TokenTree::Punct(p) if p.as_char() == '<' => {
                let turbofish =
                    at > 0 && matches!(&before[at - 1], TokenTree::Punct(c) if c.as_char() == ':');
                if turbofish {
                    return None;
                }
                names.reverse();
                return match names.iter().position(|n| n == "as") {
                    Some(0) => None,
                    Some(as_at) => Some(names[as_at - 1].clone()),
                    None => names.pop(),
                };
            }
            TokenTree::Ident(name) if depth == 0 => names.push(name.to_string()),
            _ => {}
        }
    }
    None
}

/// The attributes of an expression (`#[cfg(test)] { .. }` and the like).
fn expr_attrs(expr: &syn::Expr) -> &[syn::Attribute] {
    use syn::Expr;
    match expr {
        Expr::Array(e) => &e.attrs,
        Expr::Assign(e) => &e.attrs,
        Expr::Async(e) => &e.attrs,
        Expr::Await(e) => &e.attrs,
        Expr::Binary(e) => &e.attrs,
        Expr::Block(e) => &e.attrs,
        Expr::Break(e) => &e.attrs,
        Expr::Call(e) => &e.attrs,
        Expr::Cast(e) => &e.attrs,
        Expr::Closure(e) => &e.attrs,
        Expr::Const(e) => &e.attrs,
        Expr::Continue(e) => &e.attrs,
        Expr::Field(e) => &e.attrs,
        Expr::ForLoop(e) => &e.attrs,
        Expr::Group(e) => &e.attrs,
        Expr::If(e) => &e.attrs,
        Expr::Index(e) => &e.attrs,
        Expr::Let(e) => &e.attrs,
        Expr::Lit(e) => &e.attrs,
        Expr::Loop(e) => &e.attrs,
        Expr::Macro(e) => &e.attrs,
        Expr::Match(e) => &e.attrs,
        Expr::MethodCall(e) => &e.attrs,
        Expr::Paren(e) => &e.attrs,
        Expr::Path(e) => &e.attrs,
        Expr::Range(e) => &e.attrs,
        Expr::Reference(e) => &e.attrs,
        Expr::Repeat(e) => &e.attrs,
        Expr::Return(e) => &e.attrs,
        Expr::Struct(e) => &e.attrs,
        Expr::Try(e) => &e.attrs,
        Expr::TryBlock(e) => &e.attrs,
        Expr::Tuple(e) => &e.attrs,
        Expr::Unary(e) => &e.attrs,
        Expr::Unsafe(e) => &e.attrs,
        Expr::While(e) => &e.attrs,
        Expr::Yield(e) => &e.attrs,
        _ => &[],
    }
}

impl<'ast> Visit<'ast> for Reader {
    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {}

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

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        let method = call.method.to_string();
        self.method(&method, call.args.is_empty());
        if let Some(syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(sql),
            ..
        })) = call.args.first()
        {
            self.sql(&method, &sql.value());
        }
        visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        let segments: Vec<String> = path
            .path
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect();
        // `<T>::f` and `<T as Trait>::f` name `f` of `T` in their qself.
        let owner = match &path.qself {
            Some(qself) if qself.position + 1 >= segments.len() => type_name(&qself.ty),
            _ => segments.len().checked_sub(2).map(|at| segments[at].clone()),
        };
        if let (Some(owner), Some(name)) = (owner, segments.last()) {
            self.path(&owner, name);
        }
        visit::visit_expr_path(self, path);
    }
}

#[test]
fn every_transaction_of_src_begins_as_a_write_tx() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let rust = super::files(root, "src", "rs");
    let (path, function) = IMPLEMENTATION;
    let implementation = rust.iter().find(|(p, _)| p == path).unwrap();
    let file = syn::parse_file(&implementation.1).unwrap();
    assert!(
        begins(&file).iter().any(|b| b.function == function),
        "{path}: {function} is the begin of WriteTx, and none is seen in it"
    );
    let errors = check(&rust, EXCEPTIONS);
    assert!(
        errors.is_empty(),
        "transactions begun outside WriteTx (docs/design/persistence.md, Transactions and constraints):\n{}",
        errors.join("\n")
    );
}

mod fixtures {
    use super::*;

    /// The direct begins this module forbids, one statement each, with the
    /// name its error gives it.
    const BEGINS: [(&str, &str); 14] = [
        (
            "conn.transaction_with_behavior(TransactionBehavior::Immediate)",
            ".transaction_with_behavior()",
        ),
        ("conn.transaction()", ".transaction()"),
        ("conn.unchecked_transaction()", ".unchecked_transaction()"),
        (
            "rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)",
            "Transaction::new_unchecked",
        ),
        (
            "Transaction::new(conn, TransactionBehavior::Immediate)",
            "Transaction::new",
        ),
        ("Tx::new_unchecked(conn, b)", "Tx::new_unchecked"),
        ("C::transaction(conn)", "C::transaction"),
        (
            "<rusqlite::Transaction>::new_unchecked(conn, b)",
            "Transaction::new_unchecked",
        ),
        ("<rusqlite::Transaction>::new(conn, b)", "Transaction::new"),
        (
            "<rusqlite::Connection>::transaction(conn)",
            "Connection::transaction",
        ),
        (
            "<&C as std::any::Any>::transaction_with_behavior(conn, b)",
            "C::transaction_with_behavior",
        ),
        (
            "conn.execute_batch(\"begin immediate\")",
            ".execute_batch(\"BEGIN|SAVEPOINT ...\")",
        ),
        (
            "conn.execute(\"PRAGMA foreign_keys = ON; SAVEPOINT s\", [])",
            ".execute(\"BEGIN|SAVEPOINT ...\")",
        ),
        (
            "conn.execute_batch(\"/* writer */ BEGIN IMMEDIATE\")",
            ".execute_batch(\"BEGIN|SAVEPOINT ...\")",
        ),
    ];

    /// The forms a begin is written in: as it is, and inside a macro's
    /// tokens.
    fn forms(call: &str) -> [String; 3] {
        [
            call.to_owned(),
            format!("vec![{call}]"),
            format!("format!(\"{{:?}}\", {call})"),
        ]
    }

    const PREAMBLE: &str = "use rusqlite::{Connection as C, Transaction, Transaction as Tx};";

    fn errors(files: &[(&str, &str)], exceptions: &[Exception]) -> Vec<String> {
        let rust: Vec<(String, String)> = files
            .iter()
            .map(|(path, text)| (path.to_string(), format!("{text}\n{PREAMBLE}")))
            .collect();
        check(&rust, exceptions)
    }

    #[test]
    fn each_direct_begin_outside_write_tx_fails() {
        for (call, name) in BEGINS.iter().flat_map(|(c, n)| forms(c).map(|c| (c, *n))) {
            for (path, text) in [
                (
                    "src/infrastructure/store.rs",
                    format!("fn store(conn: &mut Connection) {{ let _ = {call}; }}"),
                ),
                (
                    "src/infrastructure/store.rs",
                    format!("impl Store {{ fn write(&self) {{ let run = || {call}; run(); }} }}"),
                ),
                (
                    "src/infrastructure/sqlite.rs",
                    format!("impl WriteTx<'_> {{ fn commit(self) {{ let _ = {call}; }} }}"),
                ),
                (
                    "src/infrastructure/other.rs",
                    format!(
                        "impl WriteTx<'_> {{ fn begin(conn: &Connection) {{ let _ = {call}; }} }}"
                    ),
                ),
            ] {
                let found = errors(&[(path, &text)], &[]);
                assert_eq!(found.len(), 1, "{call} in {path}: {found:?}");
                assert!(
                    found[0].starts_with(&format!("{path}: {name} begins a transaction in ")),
                    "{call}: {found:?}"
                );
            }
        }
    }

    #[test]
    fn a_begin_in_write_tx_begin_in_test_code_or_listed_passes() {
        for call in BEGINS.iter().flat_map(|(c, _)| forms(c)) {
            let begin = format!(
                "impl<'c> WriteTx<'c> {{ pub fn begin(conn: &'c Connection) -> Self {{ let _ = {call}; }} }}"
            );
            let tests = format!(
                "#[cfg(test)]\nmod tests {{ fn t() {{ let _ = {call}; }} }}\n#[cfg(all(test, unix))]\nfn u() {{ let _ = {call}; }}\nimpl S {{ #[cfg(test)] fn m(&self) {{ let _ = {call}; }} }}\nfn l() {{ #[cfg(test)] let _ = {call}; }}\nfn b() {{ #[cfg(test)] {{ let _ = {call}; }} }}\nfn s() {{ #[cfg(test)] println!(\"{{:?}}\", {call}); }}\n#[cfg(test)]\nmod declared;"
            );
            let declared = format!("fn d() {{ let _ = {call}; }}");
            let test_file = format!("#![cfg(test)]\nfn f() {{ let _ = {call}; }}");
            let listed = format!("fn migrate(conn: &mut Connection) {{ let _ = {call}; }}");
            let found = errors(
                &[
                    ("src/infrastructure/sqlite.rs", &begin),
                    ("src/infrastructure/store.rs", &tests),
                    ("src/infrastructure/store/declared.rs", &declared),
                    ("src/infrastructure/fixtures.rs", &test_file),
                    ("src/infrastructure/schema.rs", &listed),
                ],
                &[Exception {
                    path: "src/infrastructure/schema.rs",
                    function: "migrate",
                    reason: "the fixture's reason",
                }],
            );
            assert!(found.is_empty(), "{call}: {found:?}");
        }
    }

    #[test]
    fn what_begins_no_transaction_passes() {
        let found = errors(
            &[(
                "src/infrastructure/store.rs",
                "fn f(tx: &WriteTx, conn: &Connection) {\n    tx.execute(\"INSERT INTO t VALUES (1)\", []);\n    conn.execute_batch(\"PRAGMA foreign_keys = ON; BEGINNING\");\n    conn.execute(\"SELECT 'BEGIN'\", []);\n    conn.execute(\"INSERT INTO t VALUES ('text;BEGIN')\", []);\n    conn.execute_batch(\"-- BEGIN\\nSELECT \\\"x;savepoint\\\" FROM t\");\n    vec![wrapper.transaction(&name), other::Thing::new(conn)];\n    let v = Vec::<Transaction>::new();\n    let w = vec![Vec::<Transaction>::new(), <Other>::new(conn), <Vec<Transaction>>::new()];\n    let o = <Other as From<Transaction>>::new(conn);\n    wrapper.transaction(&name);\n    let words = [\"BEGIN\"];\n    other::Thing::new(conn);\n}",
            )],
            &[],
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn an_exception_needs_a_reason_and_a_begin_to_cover() {
        let found = errors(
            &[(
                "src/infrastructure/store.rs",
                "fn listed(conn: &mut Connection) { let _ = conn.transaction(); }",
            )],
            &[
                Exception {
                    path: "src/infrastructure/store.rs",
                    function: "listed",
                    reason: " ",
                },
                Exception {
                    path: "src/infrastructure/store.rs",
                    function: "gone",
                    reason: "it was once here",
                },
            ],
        );
        assert_eq!(
            found,
            [
                "write_tx::EXCEPTIONS: src/infrastructure/store.rs listed: want a path under src/, a fn and a reason",
                "write_tx::EXCEPTIONS: stale row src/infrastructure/store.rs gone (it begins no transaction any more); remove it",
            ]
        );
    }
}
