//! Only the queue service's process and the exceptions of ADR-t3102-2
//! decision 1 open a queue's DB (docs/design/queue-service.md「DBを開く
//! 経路」). Checked on the sources of `src/` as syntax: outside test code
//! (see [`source`](super::source)) and the library's test paths (an item,
//! a statement or an arm under `cfg(feature = "test-paths")`, which only
//! the tests build), nothing opens the queue's DB but the rows of
//! [`OPENERS`].
//!
//! What counts as an open:
//!
//! - a path (called or not) whose last two segments are `SqliteQueue::f`
//!   for an `f` of [`OPENS`], or `Self::f` in an `impl` of it;
//! - a struct expression of `SqliteOpener`, which opens the queue for each
//!   connection it is asked for, or of `SqliteServiceBackend`, the
//!   service's use cases on a DB they open;
//! - a path to an entry of the in-process service ([`IN_PROCESS`]), which
//!   only the tests build;
//! - the same in the tokens of a macro (`SqliteQueue :: f`, `SqliteOpener {`).
//!
//! Each of `SqliteQueue`, `SqliteOpener` and `SqliteServiceBackend` is also
//! any name a `type` alias or a `use ... as` of any file of `src/` gives
//! it, or gives such a name in turn: names, not types, so an alias of
//! another type with the same name counts too.
//!
//! [`DEFINER`], the adapter that defines the opens, is not read for
//! opens. A row allows where an open is written, not who calls it: so a
//! row of the service's is on what only the service's process reaches (a
//! private fn or type of its module, `serve`, or a method of the backend,
//! whose building counts as an open), never on a type every process uses
//! such as `SystemServiceAccess`. An open
//! is at the innermost fn around it (closures belong to their fn), named
//! `Type::method` in an `impl` or a trait, `-` outside any fn.

use super::source::{self, is_test, item_attrs};
use super::{children_dir, covers};
use proc_macro2::{TokenStream, TokenTree};
use std::collections::BTreeSet;
use syn::visit::{self, Visit};
use syn::{Attribute, Meta};

/// The file that defines the opens of the queue's DB.
const DEFINER: &str = "src/infrastructure/sqlite.rs";

/// The associated fns of `SqliteQueue` that open the DB at a path.
const OPENS: [&str; 12] = [
    "write_connection",
    "init",
    "create",
    "open",
    "open_watch",
    "open_read_only",
    "inspect_read_only",
    "schema",
    "binding_at",
    "migrate",
    "migrate_compatible",
    "refuse_breaking_in_use",
];

/// The entries of the service's use cases run in a process of their own
/// instead of the service's (a test's supervisor, `up` and `down` that keep
/// no service): `(owner, name)`, any name of `owner` when `name` is `None`.
const IN_PROCESS: [(&str, Option<&str>); 5] = [
    ("InProcessCaller", None),
    ("InProcessControlCaller", None),
    ("ServiceRoute", Some("InProcess")),
    ("ServiceRoute", Some("InProcessThrough")),
    ("LifecycleRoute", Some("InProcess")),
];

/// The entries of [`IN_PROCESS`] named by their last segment alone: the
/// lifecycle's queues and the telemetry tokens of a process that keeps no
/// service.
const IN_PROCESS_NAMES: [&str; 2] = ["lifecycle_in_process", "InProcessTelemetry"];

/// Why a fn may open the queue's DB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Why {
    /// The queue service's process (`service serve`), the one process that
    /// opens an existing queue (ADR-t3102-2 decision 1).
    Service,
    /// `init`, which makes the DB where there is none (ADR-t3102-2
    /// decision 1): it creates the file, and refuses one that is there,
    /// which its service initializes.
    Init,
    /// The connection of a `SqliteOpener`, whose building is the open the
    /// check counts (only the test paths build one).
    Opener,
}

/// A fn that may open the queue's DB, with why. Every row matches an
/// open, so the change that removes the open removes its row.
pub(super) struct Opener {
    pub(super) path: &'static str,
    pub(super) function: &'static str,
    pub(super) why: Why,
}

const fn row(path: &'static str, function: &'static str, why: Why) -> Opener {
    Opener {
        path,
        function,
        why,
    }
}

/// The fns of production code that open the queue's DB.
pub(super) const OPENERS: &[Opener] = &[
    row("src/compose/host.rs", "init_queue", Why::Init),
    row(
        "src/infrastructure/queue_service.rs",
        "SqliteServiceBackend::admin",
        Why::Service,
    ),
    row(
        "src/infrastructure/queue_service.rs",
        "SqliteServiceBackend::target_facts",
        Why::Service,
    ),
    row(
        "src/infrastructure/runtime_store/mod.rs",
        "SqliteOpener::connect",
        Why::Opener,
    ),
    row(
        "src/infrastructure/queue_service.rs",
        "judge_start",
        Why::Service,
    ),
    row(
        "src/infrastructure/queue_service.rs",
        "apply_start_migrations",
        Why::Service,
    ),
    row(
        "src/infrastructure/queue_service.rs",
        "serve_on",
        Why::Service,
    ),
    row(
        "src/infrastructure/queue_service.rs",
        "SqliteServiceBackend::binding",
        Why::Service,
    ),
    row(
        "src/infrastructure/queue_service.rs",
        "SqliteServiceBackend::open_read",
        Why::Service,
    ),
    row(
        "src/infrastructure/queue_service.rs",
        "SqliteServiceBackend::open_watch",
        Why::Service,
    ),
    row(
        "src/infrastructure/queue_service.rs",
        "write_queue",
        Why::Service,
    ),
    row(
        "src/infrastructure/queue_service.rs",
        "ServiceWriteLine::start",
        Why::Service,
    ),
];

/// A direct open of the queue's DB: what opened it, in which fn.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Open {
    function: String,
    what: String,
}

/// Every failure of the files `rust` (paths from the repository root, with
/// their text) against `openers`, one line each.
fn check(rust: &[(String, String)], openers: &[Opener]) -> Vec<String> {
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
    let names = names(read.iter().map(|(_, file)| file));
    read.retain(|(path, _)| *path != DEFINER && !test_paths.iter().any(|test| covers(test, path)));
    let mut listed = BTreeSet::new();
    for row in openers {
        let at = format!(
            "queue_opens::OPENERS: {} {} ({:?})",
            row.path, row.function, row.why
        );
        if !row.path.starts_with("src/") || row.function.trim().is_empty() {
            errors.insert(format!("{at}: want a path under src/ and a fn"));
        }
        if !listed.insert((row.path, row.function)) {
            errors.insert(format!("{at}: listed twice"));
        }
    }
    let mut matched = BTreeSet::new();
    for (path, file) in &read {
        for open in opens(file, &names) {
            if let Some(row) = openers
                .iter()
                .find(|row| row.path == *path && row.function == open.function)
            {
                matched.insert((row.path, row.function));
                // `init` makes a DB where there is none, and opens no queue
                // that is there.
                if row.why == Why::Init && !open.what.ends_with("::create") {
                    errors.insert(format!(
                        "{path}: {} opens the queue's DB in {}, a row of init's: init opens only where there is no queue (SqliteQueue::create), and sends the rest to the queue's service",
                        open.what, open.function
                    ));
                }
                continue;
            }
            errors.insert(format!(
                "{path}: {} opens the queue's DB in {}: only the queue service's process and the exceptions of ADR-t3102-2 decision 1 open it; send it to the service as a use case, put a test's path under cfg(feature = \"test-paths\"), or add a row with its reason to queue_opens::OPENERS",
                open.what, open.function
            ));
        }
    }
    for row in openers {
        if !matched.contains(&(row.path, row.function)) {
            errors.insert(format!(
                "queue_opens::OPENERS: stale row {} {} ({:?}) (it opens no queue any more); remove it",
                row.path, row.function, row.why
            ));
        }
    }
    errors.into_iter().collect()
}

/// Whether `attrs` hold a `cfg` that requires the feature `test-paths`
/// (`cfg(feature = "test-paths")`, or an `all(...)` with it): what only
/// the tests build.
fn is_test_path(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<Meta>()
                .is_ok_and(|meta| requires_test_paths(&meta))
    })
}

fn requires_test_paths(meta: &Meta) -> bool {
    match meta {
        Meta::NameValue(pair) => {
            pair.path.is_ident("feature")
                && matches!(
                    &pair.value,
                    syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(name), .. })
                        if name.value() == "test-paths"
                )
        }
        Meta::List(list) if list.path.is_ident("all") => list
            .parse_args_with(syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated)
            .is_ok_and(|args| args.iter().any(requires_test_paths)),
        _ => false,
    }
}

/// Whether what `attrs` are on is outside production code.
fn skipped(attrs: &[Attribute]) -> bool {
    is_test(attrs) || is_test_path(attrs)
}

/// The names of `SqliteQueue` (`queues`) and of the struct expressions
/// that open the DB (`openers`) in the crate.
#[derive(Debug, Clone, Default)]
struct Names {
    queues: BTreeSet<String>,
    openers: BTreeSet<String>,
}

/// The names `files` give `SqliteQueue`, `SqliteOpener` and
/// `SqliteServiceBackend` with a `type` alias or a `use ... as`, to the
/// names of names, in any file and any code (an alias in test code that
/// production code names is still an alias).
fn names<'a>(files: impl Iterator<Item = &'a syn::File> + Clone) -> Names {
    let mut names = Names::default();
    names.queues.insert("SqliteQueue".to_owned());
    names.openers.insert("SqliteOpener".to_owned());
    names.openers.insert("SqliteServiceBackend".to_owned());
    loop {
        let before = (names.queues.len(), names.openers.len());
        for file in files.clone() {
            Aliases { names: &mut names }.visit_file(file);
        }
        if (names.queues.len(), names.openers.len()) == before {
            return names;
        }
    }
}

/// The opens of `file` outside its test code and test paths.
fn opens(file: &syn::File, names: &Names) -> BTreeSet<Open> {
    let mut reader = Reader {
        aliases: names.queues.clone(),
        openers: names.openers.clone(),
        ..Reader::default()
    };
    if skipped(&file.attrs) {
        return reader.found;
    }
    reader.visit_file(file);
    reader.found
}

/// The names a `type` alias or a `use ... as` gives a name of [`Names`].
struct Aliases<'a> {
    names: &'a mut Names,
}

impl Aliases<'_> {
    fn alias(&mut self, of: &str, name: String) {
        if self.names.queues.contains(of) {
            self.names.queues.insert(name);
        } else if self.names.openers.contains(of) {
            self.names.openers.insert(name);
        }
    }
}

impl<'ast> Visit<'ast> for Aliases<'_> {
    fn visit_use_rename(&mut self, rename: &'ast syn::UseRename) {
        self.alias(&rename.ident.to_string(), rename.rename.to_string());
    }

    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        if let Some(of) = type_name(&item.ty) {
            self.alias(&of, item.ident.to_string());
        }
        visit::visit_item_type(self, item);
    }
}

#[derive(Default)]
struct Reader {
    /// The names of `SqliteQueue` and of `SqliteOpener` in the file.
    aliases: BTreeSet<String>,
    openers: BTreeSet<String>,
    /// The self type of each `impl` (or the trait) around, innermost last.
    owners: Vec<String>,
    fns: Vec<String>,
    found: BTreeSet<Open>,
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
        self.found.insert(Open { function, what });
    }

    /// A path whose last two segments are `owner::name`.
    fn path(&mut self, owner: &str, name: &str) {
        let owner = match self.owners.last() {
            Some(of) if owner == "Self" => of.clone(),
            _ => owner.to_owned(),
        };
        let queue = self.aliases.contains(&owner) && OPENS.contains(&name);
        let in_process = IN_PROCESS
            .iter()
            .any(|(of, entry)| *of == owner && entry.is_none_or(|entry| entry == name));
        if queue || in_process {
            self.found(format!("{owner}::{name}"));
        }
    }

    /// The tokens of a macro, which syn does not parse: `owner :: name`
    /// and `SqliteOpener {`, in its groups too.
    fn tokens(&mut self, tokens: TokenStream) {
        let trees: Vec<TokenTree> = tokens.into_iter().collect();
        for (at, tree) in trees.iter().enumerate() {
            match tree {
                TokenTree::Group(group) => self.tokens(group.stream()),
                TokenTree::Ident(owner) => {
                    let owner = owner.to_string();
                    if self.openers.contains(&owner)
                        && matches!(trees.get(at + 1), Some(TokenTree::Group(group))
                            if group.delimiter() == proc_macro2::Delimiter::Brace)
                    {
                        self.found(owner.clone());
                    }
                    if let (
                        Some(TokenTree::Punct(a)),
                        Some(TokenTree::Punct(b)),
                        Some(TokenTree::Ident(name)),
                    ) = (trees.get(at + 1), trees.get(at + 2), trees.get(at + 3))
                        && a.as_char() == ':'
                        && b.as_char() == ':'
                    {
                        self.path(&owner, &name.to_string());
                    }
                }
                _ => {}
            }
        }
    }
}

fn type_name(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(path) => path.path.segments.last().map(|s| s.ident.to_string()),
        syn::Type::Reference(reference) => type_name(&reference.elem),
        _ => None,
    }
}

impl<'ast> Visit<'ast> for Reader {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        if !skipped(item_attrs(item)) {
            visit::visit_item(self, item);
        }
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.in_fn(&item.sig.ident, false, |r| visit::visit_item_fn(r, item));
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        let owner = type_name(&item.self_ty).unwrap_or_default();
        self.in_owner(owner, |r| visit::visit_item_impl(r, item));
    }

    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        self.in_owner(item.ident.to_string(), |r| visit::visit_item_trait(r, item));
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        let attrs = match item {
            syn::ImplItem::Fn(f) => &f.attrs,
            syn::ImplItem::Const(c) => &c.attrs,
            syn::ImplItem::Type(t) => &t.attrs,
            syn::ImplItem::Macro(m) => &m.attrs,
            _ => return visit::visit_impl_item(self, item),
        };
        if !skipped(attrs) {
            visit::visit_impl_item(self, item);
        }
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.in_fn(&item.sig.ident, true, |r| {
            visit::visit_impl_item_fn(r, item)
        });
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        if !skipped(&item.attrs) {
            self.in_fn(&item.sig.ident, true, |r| {
                visit::visit_trait_item_fn(r, item)
            });
        }
    }

    fn visit_stmt(&mut self, stmt: &'ast syn::Stmt) {
        let attrs: &[Attribute] = match stmt {
            syn::Stmt::Local(local) => &local.attrs,
            syn::Stmt::Macro(mac) => &mac.attrs,
            syn::Stmt::Expr(syn::Expr::Call(call), _) => &call.attrs,
            syn::Stmt::Expr(syn::Expr::MethodCall(call), _) => &call.attrs,
            syn::Stmt::Expr(syn::Expr::Block(block), _) => &block.attrs,
            _ => &[],
        };
        if !skipped(attrs) {
            visit::visit_stmt(self, stmt);
        }
    }

    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        if !skipped(&arm.attrs) {
            visit::visit_arm(self, arm);
        }
    }

    fn visit_field_value(&mut self, field: &'ast syn::FieldValue) {
        if !skipped(&field.attrs) {
            visit::visit_field_value(self, field);
        }
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.tokens(mac.tokens.clone());
        visit::visit_macro(self, mac);
    }

    fn visit_expr_struct(&mut self, expr: &'ast syn::ExprStruct) {
        let name = match (expr.path.segments.last(), self.owners.last()) {
            (Some(name), Some(of)) if name.ident == "Self" => Some(of.clone()),
            (name, _) => name.map(|name| name.ident.to_string()),
        };
        if let Some(name) = name
            && self.openers.contains(&name)
        {
            self.found(name);
        }
        visit::visit_expr_struct(self, expr);
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
        if let Some(name) = segments
            .last()
            .filter(|name| IN_PROCESS_NAMES.contains(&name.as_str()))
        {
            self.found(name.clone());
        }
        visit::visit_expr_path(self, path);
    }
}

#[test]
fn only_the_queue_service_and_the_adr_s_exceptions_open_the_queue_db() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let rust = super::files(root, "src", "rs");
    let errors = check(&rust, OPENERS);
    assert!(
        errors.is_empty(),
        "the queue's DB opened outside the queue service (ADR-t3102-2 decision 1, docs/design/queue-service.md):\n{}",
        errors.join("\n")
    );
}

mod fixtures {
    use super::*;

    /// The opens this module finds, one statement each, with the name its
    /// error gives it.
    const OPENS_FOUND: [(&str, &str); 6] = [
        (
            "let q = SqliteQueue::open(db).unwrap();",
            "SqliteQueue::open",
        ),
        (
            "let q = crate::infrastructure::sqlite::SqliteQueue::open_read_only(db);",
            "SqliteQueue::open_read_only",
        ),
        (
            "let f = SqliteQueue::open_watch;",
            "SqliteQueue::open_watch",
        ),
        ("let q = Queue::init(db);", "Queue::init"),
        (
            "let o = SqliteOpener { db, generators, actor: None };",
            "SqliteOpener",
        ),
        (
            "let q = format!(\"{}\", SqliteQueue::open(db).is_ok());",
            "SqliteQueue::open",
        ),
    ];

    fn file_with(body: &str) -> String {
        format!(
            "use crate::infrastructure::sqlite::SqliteQueue as Queue;\nfn reads(db: &Path) {{ {body} }}\n"
        )
    }

    fn errors(files: &[(&str, &str)], openers: &[Opener]) -> Vec<String> {
        let rust: Vec<(String, String)> = files
            .iter()
            .map(|(p, t)| ((*p).to_owned(), (*t).to_owned()))
            .collect();
        check(&rust, openers)
    }

    #[test]
    fn an_open_outside_the_service_and_the_exceptions_fails() {
        for (open, name) in OPENS_FOUND {
            let found = errors(&[("src/compose/reads.rs", &file_with(open))], &[]);
            assert_eq!(found.len(), 1, "{open}: {found:#?}");
            assert!(
                found[0].starts_with(&format!(
                    "src/compose/reads.rs: {name} opens the queue's DB in reads"
                )),
                "{open}: {found:#?}"
            );
        }
        // A method of an impl is named with its type.
        let found = errors(
            &[(
                "src/main.rs",
                "impl Cli { fn run(&self) { let _ = SqliteQueue::open(&self.db); } }",
            )],
            &[],
        );
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].contains("in Cli::run"), "{found:#?}");
    }

    /// Opens through an alias, `Self` or the in-process service: each case's
    /// files (the one to read first), with the name its error gives it.
    const THROUGH: [(&str, &str, &str); 10] = [
        (
            "",
            "type Q = crate::infrastructure::sqlite::SqliteQueue;\nfn reads(db: &Path) { let _ = Q::open(db); }",
            "Q::open",
        ),
        (
            "pub use crate::infrastructure::sqlite::SqliteQueue as Store;\npub type Again = Store;",
            "fn reads(db: &Path) { let _ = crate::infrastructure::store::Again::open_read_only(db); }",
            "Again::open_read_only",
        ),
        (
            "",
            "impl SqliteQueue { fn reads(db: &Path) -> Self { Self::open(db).unwrap() } }",
            "SqliteQueue::open",
        ),
        (
            "pub type Backend = crate::infrastructure::queue_service::SqliteServiceBackend;",
            "fn reads(db: PathBuf) { let _ = Backend { db, queue_dir }; }",
            "Backend",
        ),
        (
            "",
            "fn reads(db: PathBuf) { let _ = SqliteServiceBackend { db, queue_dir }; }",
            "SqliteServiceBackend",
        ),
        (
            "",
            "fn reads(db: &Path) { let _ = InProcessCaller::new(db, generators, reads, writes, detail, commands); }",
            "InProcessCaller::new",
        ),
        (
            "",
            "fn reads(options: &mut SuperviseOptions) { options.service_route = ServiceRoute::InProcess; }",
            "ServiceRoute::InProcess",
        ),
        (
            "",
            "fn reads() { let _ = crate::compose::LifecycleRoute::InProcess; }",
            "LifecycleRoute::InProcess",
        ),
        (
            "",
            "fn reads(db: &Path) { let _ = super::lifecycle_in_process(db, generators, control, None); }",
            "lifecycle_in_process",
        ),
        (
            "",
            "fn reads(db: &Path, target: &StepTarget) { let _ = crate::infrastructure::queue_service::InProcessTelemetry.target_facts(db, target); }",
            "InProcessTelemetry",
        ),
    ];

    /// The telemetry tokens every process takes, as a pub type of the
    /// service's module, open no queue: a row of the service's lets only
    /// the backend's open, not that of a type every process uses.
    #[test]
    fn the_shared_telemetry_tokens_that_open_the_queue_fail() {
        let text = "impl TelemetryTokens for SystemServiceAccess { fn target_facts(&self, db: &Path, target: &StepTarget) -> Result<TargetFacts> { SqliteQueue::open(db)?.telemetry_target(target) } }";
        let found = check(
            &[(
                "src/infrastructure/queue_service.rs".to_owned(),
                text.to_owned(),
            )],
            &[],
        );
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            found[0].starts_with("src/infrastructure/queue_service.rs: SqliteQueue::open opens the queue's DB in SystemServiceAccess::target_facts"),
            "{found:#?}"
        );
        assert!(
            !OPENERS
                .iter()
                .any(|row| row.function.starts_with("SystemServiceAccess::")),
            "a row lets SystemServiceAccess, which every process uses, open the queue"
        );
    }

    #[test]
    fn an_open_through_an_alias_or_self_or_the_in_process_service_fails() {
        for (other, text, name) in THROUGH {
            let mut files = vec![("src/compose/reads.rs", text)];
            if !other.is_empty() {
                files.push(("src/infrastructure/store.rs", other));
            }
            let found = errors(&files, &[]);
            assert_eq!(found.len(), 1, "{text}: {found:#?}");
            assert!(
                found[0].starts_with(&format!(
                    "src/compose/reads.rs: {name} opens the queue's DB in "
                )),
                "{text}: {found:#?}"
            );
            // Under the test paths, the same passes.
            let tested = format!("#![cfg(feature = \"test-paths\")]\n{text}");
            files[0] = ("src/compose/reads.rs", &tested);
            assert!(errors(&files, &[]).is_empty(), "{tested}");
        }
    }

    #[test]
    fn an_open_in_a_test_or_a_test_path_or_listed_or_in_the_adapter_passes() {
        for (open, _) in OPENS_FOUND {
            for text in [
                format!("#[cfg(test)]\nmod tests {{ fn reads(db: &Path) {{ {open} }} }}"),
                format!("#[cfg(feature = \"test-paths\")]\nfn reads(db: &Path) {{ {open} }}"),
                format!(
                    "impl OneShot {{ #[cfg(all(unix, feature = \"test-paths\"))]\nfn reads(&self, db: &Path) {{ {open} }} }}"
                ),
                format!("fn reads(db: &Path) {{ #[cfg(feature = \"test-paths\")]\n{open} }}"),
            ] {
                assert!(
                    errors(&[("src/compose/reads.rs", &text)], &[]).is_empty(),
                    "{text}"
                );
            }
            assert!(
                errors(
                    &[("src/compose/reads.rs", &file_with(open))],
                    &[row("src/compose/reads.rs", "reads", Why::Service)]
                )
                .is_empty(),
                "{open}"
            );
            assert!(
                errors(&[(DEFINER, &file_with(open))], &[]).is_empty(),
                "{open}"
            );
        }
        // A test path that names another feature is production code.
        let found = errors(
            &[(
                "src/compose/reads.rs",
                "#[cfg(feature = \"other\")]\nfn reads(db: &Path) { SqliteQueue::open(db); }",
            )],
            &[],
        );
        assert_eq!(found.len(), 1, "{found:#?}");
    }

    #[test]
    fn what_opens_no_queue_passes() {
        let found = errors(
            &[(
                "src/compose/reads.rs",
                "fn reads(queue: &SqliteQueue) { queue.schema_version(); SqliteQueue::SCHEMA_VERSION; let open = Client::open(db); let _ = ServiceRoute::Socket; let _ = LifecycleRoute::Socket(env); }",
            )],
            &[],
        );
        assert!(found.is_empty(), "{found:#?}");
    }

    /// No row lets the queue's administration (`migrate`, its copy, its
    /// check, `doctor`) open the DB outside the service's process, and init's
    /// row lets only the making of a DB where there is none: `migrate` or an
    /// open of an existing queue in the command line's fns fails, row or
    /// not (ADR-t3102-2 decisions 1 and 8).
    #[test]
    fn the_queue_s_administration_opens_the_db_only_in_the_service() {
        for row in OPENERS {
            assert!(
                matches!(row.why, Why::Service | Why::Opener)
                    || (row.why == Why::Init && row.path == "src/compose/host.rs"),
                "{} {}: an exception outside the service",
                row.path,
                row.function
            );
        }
        assert_eq!(
            OPENERS.iter().filter(|row| row.why == Why::Init).count(),
            1,
            "init's entry is the one exception"
        );
        let init = [row("src/compose/host.rs", "init_queue", Why::Init)];
        for (open, name) in [
            (
                "let _ = SqliteQueue::migrate(db, None, 0);",
                "SqliteQueue::migrate",
            ),
            ("let _ = SqliteQueue::schema(db);", "SqliteQueue::schema"),
            ("let _ = SqliteQueue::init(db);", "SqliteQueue::init"),
            ("let _ = SqliteQueue::open(db);", "SqliteQueue::open"),
            (
                "let _ = SqliteQueue::inspect_read_only(db);",
                "SqliteQueue::inspect_read_only",
            ),
        ] {
            let text = format!("fn init_queue(db: &Path) {{ {open} }}");
            let found = errors(&[("src/compose/host.rs", &text)], &init);
            assert_eq!(found.len(), 1, "{open}: {found:#?}");
            assert!(
                found[0].starts_with(&format!(
                    "src/compose/host.rs: {name} opens the queue's DB in init_queue, a row of init's"
                )),
                "{open}: {found:#?}"
            );
            let text = format!("fn migrate_queue(db: &Path) {{ {open} }}");
            let found = errors(&[("src/compose/host.rs", &text)], &init);
            assert!(
                found.iter().any(|error| error.starts_with(&format!(
                    "src/compose/host.rs: {name} opens the queue's DB in migrate_queue"
                ))),
                "{open}: {found:#?}"
            );
        }
        let made = "fn init_queue(db: &Path) { let _ = SqliteQueue::create(db); }";
        assert!(errors(&[("src/compose/host.rs", made)], &init).is_empty());
    }

    #[test]
    fn a_row_needs_an_open_to_cover() {
        let found = errors(
            &[("src/compose/reads.rs", "fn reads() {}")],
            &[row("src/compose/reads.rs", "reads", Why::Init)],
        );
        assert_eq!(
            found,
            [
                "queue_opens::OPENERS: stale row src/compose/reads.rs reads (Init) (it opens no queue any more); remove it"
            ]
        );
    }
}
