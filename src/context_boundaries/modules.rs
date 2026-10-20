//! The rules L1, L2's part clippy cannot hold, L3, L6, L8 and L9 of
//! docs/design/architecture.md, checked on the library's module references
//! read as syntax: a layer references no outer layer and no module outside
//! the layers but the shared parts the table
//! [`super::ownership::SHARED_MODULES`] lets it, the production code of the
//! domain references none of [`DOMAIN_FORBIDS`], and a context module of
//! `src/compose/` references no other module of `src/compose/` and globs no
//! `crate::compose`. The modules of `src/compose/` are declared private, so
//! the code outside the composition root reaches them only through its
//! re-exports (L9). C5 and C8 on the same references, and C7 for the
//! records of the ports, are [`ports`].
//!
//! What counts:
//!
//! - The modules are those `src/lib.rs` declares, followed through
//!   `mod name;` (`name.rs`, else `name/mod.rs`) and inline modules. Test
//!   code counts too, but not for L2 and C5: test code is an item, a field, a
//!   variant, an arm, a `let`, an expression (a block too), a statement
//!   macro or a field of a struct expression or pattern under a `cfg` that
//!   requires `test` ([`super::source::is_test`]), the files a test module
//!   declares and a file under `#![cfg(test)]`.
//! - A reference is the path of a `use` (each leaf of a group, `self` and
//!   `*` included), a path in an expression, a type, a pattern or an
//!   attribute, and a path in a macro's tokens (a name followed by `::`, a
//!   leading `::`, or `crate`, `self` or `super`; a group in them read as a
//!   `use` tree; for C5 only, outside test code, a name alone there too,
//!   [`Reference::bare`]). It is resolved
//!   to a path from the crate's root: `crate`, `self` and `super` by the
//!   module it is written in, a bare first segment when it names an item, a
//!   module or a `use` of that module or a name one of its globs brings in,
//!   a glob's module's own globs included. A bare first segment the
//!   library does not define there, and a leading `::` but the root's
//!   names of `extern crate self`, is another crate (a local or a generic
//!   reads the same, so one named like an item of [`DOMAIN_FORBIDS`] would
//!   count); `extern crate name as other;` makes `other` that crate where
//!   it is written, and in every module when it is at the root. The names
//!   a glob of another crate brings in are not followed (a glob of
//!   `std::process` counts at the glob). A name is looked up in the
//!   namespaces of the place it is written in, as Rust does ([`Ns`]): an
//!   item names it and hides a glob's name or an outer scope's only in its
//!   own namespace, a `use` in all of them. `extern crate self as name`
//!   names the root, from every module when it is written at the root. The
//!   items, `use`s and globs inside a fn body or a block are names of that
//!   block only, looked up from the innermost block around a reference out
//!   to its module. The check does not evaluate `cfg`: a name counts with
//!   each definition it may have in some build (`use`s of it under
//!   different `cfg`s, an item, a glob), and only an item or a `use` under
//!   no `cfg` shadows the globs and the outer scopes. The path of a `use`
//!   is resolved the same way where the `use` is written,
//!   and a cycle of `use`s and globs ends unresolved. Both that path and
//!   the path through the `use`s (`pub use` or not, globs too) to the
//!   module that defines the name count, so a re-export neither hides the
//!   module it is in nor the one it names.
//! - Comments, doc comments and string literals do not count.
//! - A violation is `<rule> <reference>` in its file, the reference cut to
//!   the module the rule names (`crate::view`, `crate::compose::host`,
//!   `crate::compose::*` for a glob of the root, or an item of
//!   [`DOMAIN_FORBIDS`] such as `std::process`). Every violation has a row
//!   of [`super::ownership::MODULES_ALLOWED`], and every row still matches
//!   one.

pub(super) mod ports;

use super::ownership::{Fix, Layer, ModuleAllowed, Rule, SharedModule};
use proc_macro2::{TokenStream, TokenTree};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use syn::visit::{self, Visit};

/// A module path from the crate's root, `[]` for the root; one of another
/// crate starts with [`EXTERN`].
type ModulePath = Vec<String>;

/// The first segment of a path of another crate (`["::", "std", "net"]`),
/// which no module of the library has.
const EXTERN: &str = "::";

/// The keywords a path with a leading `::` may follow in a macro's tokens.
const KEYWORDS: &[&str] = &[
    "as", "box", "break", "const", "dyn", "else", "for", "if", "impl", "in", "let", "match",
    "move", "mut", "ref", "return", "static", "where", "while", "yield",
];

/// What the production code of the domain does not reference (L2): the
/// paths of other crates the domain alone forbids, which clippy.toml's
/// lists, one per lint for the whole package, cannot hold, since the
/// application uses `anyhow` and `std::process::id` and the composition
/// root `std::process::id`.
const DOMAIN_FORBIDS: &[&[&str]] = &[&["anyhow"], &["std", "process"], &["std", "net"]];

/// The names a module, or a block, defines and imports.
#[derive(Debug, Default)]
struct Module {
    /// Its items and child modules, with their namespaces.
    items: BTreeMap<String, Ns>,
    /// Its `use` leaves by the name they bring in, as written: more than
    /// one when `use`s under different `cfg`s bring in the same name.
    aliases: BTreeMap<String, Vec<Vec<String>>>,
    /// Its glob `use`s, as written without the `*`.
    globs: Vec<Vec<String>>,
    /// The names it has an item or a `use` of under no `cfg`, with the
    /// namespaces it has them in: in every build they shadow its globs and
    /// the names of the scopes around it there.
    fixed: BTreeMap<String, Ns>,
}

/// A set of Rust's namespaces, as bits. A path's last segment is looked up
/// in the namespaces of the place it is written in: an expression's and a
/// tuple struct pattern's in [`VALUE`], a type's, a trait bound's and a
/// struct expression's or pattern's in [`TYPE`], a macro's and an
/// attribute's in [`MACRO`], a `use` leaf's and any other in [`ANY`]. The
/// segments before it and a glob's module are in [`TYPE`].
type Ns = u8;
/// Modules, types, traits and crates.
const TYPE: Ns = 1;
/// Fns, consts, statics and tuple and unit structs.
const VALUE: Ns = 2;
const MACRO: Ns = 4;
const ANY: Ns = TYPE | VALUE | MACRO;

/// The namespaces `names` has `name` in, none when it lacks it.
fn ns_of(names: &BTreeMap<String, Ns>, name: &str) -> Ns {
    names.get(name).copied().unwrap_or(0)
}

/// A reference as written, in the module, the blocks and the file it is
/// written in.
#[derive(Debug)]
struct Reference {
    file: String,
    module: ModulePath,
    /// The blocks around it in its module, outermost first, as indexes of
    /// [`Crate::blocks`].
    blocks: Vec<usize>,
    path: Vec<String>,
    /// The namespaces its last segment is looked up in.
    ns: Ns,
    glob: bool,
    /// Whether it is in test code.
    test: bool,
    /// A name alone in a macro's tokens (`RunRecovery` in
    /// `vec![None::<&dyn RunRecovery>]`), outside test code: only C5 reads
    /// it, since syn does not tell what it is.
    bare: bool,
}

/// A trait the library defines, with the methods its body declares outside
/// test code.
#[derive(Debug)]
struct TraitDef {
    file: String,
    module: ModulePath,
    name: String,
    /// Declared `pub` or `pub(crate)`.
    public: bool,
    test: bool,
    methods: BTreeSet<String>,
}

/// The library read as syntax.
#[derive(Debug, Default)]
struct Crate {
    modules: BTreeMap<ModulePath, Module>,
    /// The names of each fn body and block: its items and `use`s are seen
    /// only inside it.
    blocks: Vec<Module>,
    references: Vec<Reference>,
    traits: Vec<TraitDef>,
    /// The names each file defines outside test code (an item at any
    /// depth, or a fn of an `impl`), by file.
    defined: BTreeSet<(String, String)>,
    errors: Vec<String>,
    /// Every name an item or a `use` defines somewhere in the library: a
    /// bare first segment outside it names nothing of the library.
    names: BTreeSet<String>,
    /// The names `extern crate self as` gives the root at the root: every
    /// module may name the root by them, bare or after `::`.
    prelude: BTreeSet<String>,
    /// The crates `extern crate name as other;` at the root names, by
    /// `other`: every module may name the crate by it.
    externs: BTreeMap<String, String>,
    memo: Memo,
}

/// The results of [`Crate::resolve`] and [`Crate::defines`] that no cut of
/// a cycle at a lookup begun before them changed, which are the same
/// whatever lookups are in progress.
#[derive(Debug, Default)]
struct Memo {
    resolved: RefCell<HashMap<(ModulePath, Ns), BTreeSet<ModulePath>>>,
    defined: RefCell<HashMap<(ModulePath, String, Ns), bool>>,
    /// The lowest index of [`Seen`] that a lookup found in progress, since
    /// the outermost result being computed began.
    cut: Cell<Option<usize>>,
}

/// The ownership the check reads.
pub(super) struct ModuleRecords<'a> {
    pub(super) shared: &'a [SharedModule],
    pub(super) allowed: &'a [ModuleAllowed],
    pub(super) ports: ports::PortRecords<'a>,
}

/// Reads the library from `src/lib.rs` of `files` (path from the repository
/// root to text).
fn read(files: &BTreeMap<String, String>) -> Crate {
    let mut krate = Crate::default();
    krate.modules.insert(Vec::new(), Module::default());
    let mut queue = vec![("src/lib.rs".to_owned(), Vec::new(), false)];
    while let Some((file, module, test)) = queue.pop() {
        let Some(text) = files.get(&file) else {
            krate
                .errors
                .push(format!("{file}: the module {} has no file", show(&module)));
            continue;
        };
        let parsed = match syn::parse_file(text) {
            Ok(parsed) => parsed,
            Err(error) => {
                krate
                    .errors
                    .push(format!("{file}: not read as Rust: {error}"));
                continue;
            }
        };
        let dir = super::children_dir(&file);
        let mut reader = Reader {
            krate: &mut krate,
            file: file.clone(),
            dir: vec![dir],
            module: vec![module],
            declared: Vec::new(),
            blocks: Vec::new(),
            cfg: false,
            test: test || super::source::is_test(&parsed.attrs),
        };
        reader.visit_file(&parsed);
        let declared = std::mem::take(&mut reader.declared);
        for (dir, child, test) in declared {
            let name = child.last().cloned().unwrap_or_default();
            let flat = format!("{dir}/{name}.rs");
            let nested = format!("{dir}/{name}/mod.rs");
            let path = if files.contains_key(&flat) || !files.contains_key(&nested) {
                flat
            } else {
                nested
            };
            queue.push((path, child, test));
        }
    }
    let scopes = krate.modules.values().chain(&krate.blocks);
    krate.names = scopes
        .flat_map(|scope| scope.items.keys().chain(scope.aliases.keys()))
        .cloned()
        .collect();
    krate
}

/// Walks one file of the library.
struct Reader<'a> {
    krate: &'a mut Crate,
    file: String,
    /// The directory of the files of the modules declared here, by depth of
    /// inline module.
    dir: Vec<String>,
    /// The module the code is in, by depth of inline module.
    module: Vec<ModulePath>,
    /// The modules declared without a body: their directory, path and
    /// whether they are test code.
    declared: Vec<(String, ModulePath, bool)>,
    /// The blocks around the code in its module, outermost first.
    blocks: Vec<usize>,
    /// Whether the `use` being read is under a `cfg`.
    cfg: bool,
    /// Whether the code being read is test code.
    test: bool,
}

impl Reader<'_> {
    fn here(&self) -> &ModulePath {
        self.module.last().expect("a module")
    }

    /// The names of the innermost block around the code, or of its module.
    fn scope(&mut self) -> &mut Module {
        match self.blocks.last() {
            Some(&block) => &mut self.krate.blocks[block],
            None => {
                let here = self.here().clone();
                self.krate.modules.entry(here).or_default()
            }
        }
    }

    fn item(&mut self, name: String, cfg: bool, ns: Ns) {
        let scope = self.scope();
        if !cfg {
            *scope.fixed.entry(name.clone()).or_default() |= ns;
        }
        *scope.items.entry(name).or_default() |= ns;
    }

    fn reference(&mut self, path: Vec<String>, ns: Ns, glob: bool) {
        let module = self.here().clone();
        self.krate.references.push(Reference {
            file: self.file.clone(),
            module,
            blocks: self.blocks.clone(),
            path,
            ns,
            glob,
            test: self.test,
            bare: false,
        });
    }

    /// The name `name` alone in a macro's tokens, as a reference only C5
    /// reads; none in test code, which C5 does not count.
    fn bare(&mut self, name: String) {
        if self.test {
            return;
        }
        let module = self.here().clone();
        self.krate.references.push(Reference {
            file: self.file.clone(),
            module,
            blocks: self.blocks.clone(),
            path: vec![name],
            ns: ANY,
            glob: false,
            test: false,
            bare: true,
        });
    }

    /// Walks code with `attrs`, as test code when they make it so.
    fn with_attrs(&mut self, attrs: &[syn::Attribute], walk: impl FnOnce(&mut Self)) {
        let test = self.test;
        self.test |= super::source::is_test(attrs);
        walk(self);
        self.test = test;
    }

    /// `path` with its last segment in `ns`, and the paths of its generic
    /// arguments.
    fn path_in(&mut self, path: &syn::Path, ns: Ns) {
        let mut segments = Vec::new();
        if path.leading_colon.is_some() {
            segments.push(String::new());
        }
        segments.extend(path.segments.iter().map(|s| s.ident.to_string()));
        self.reference(segments, ns, false);
        visit::visit_path(self, path);
    }

    /// The leaves of a `use` tree under `prefix`.
    fn use_tree(&mut self, prefix: &[String], tree: &syn::UseTree) {
        let joined = |name: String| {
            let mut path = prefix.to_vec();
            path.push(name);
            path
        };
        match tree {
            syn::UseTree::Path(p) => self.use_tree(&joined(p.ident.to_string()), &p.tree),
            syn::UseTree::Group(g) => {
                for tree in &g.items {
                    self.use_tree(prefix, tree);
                }
            }
            syn::UseTree::Name(n) => {
                let name = n.ident.to_string();
                let (alias, path) = if name == "self" {
                    (prefix.last().cloned(), prefix.to_vec())
                } else {
                    (Some(name.clone()), joined(name))
                };
                self.alias(alias, path);
            }
            syn::UseTree::Rename(r) => {
                let ident = r.ident.to_string();
                let path = if ident == "self" {
                    prefix.to_vec()
                } else {
                    joined(ident)
                };
                self.alias(Some(r.rename.to_string()), path);
            }
            syn::UseTree::Glob(_) => {
                self.scope().globs.push(prefix.to_vec());
                self.reference(prefix.to_vec(), TYPE, true);
            }
        }
    }

    fn alias(&mut self, name: Option<String>, path: Vec<String>) {
        if let Some(name) = name.filter(|n| n != "_") {
            let cfg = self.cfg;
            let scope = self.scope();
            if !cfg {
                *scope.fixed.entry(name.clone()).or_default() |= ANY;
            }
            let targets = scope.aliases.entry(name).or_default();
            if !targets.contains(&path) {
                targets.push(path.clone());
            }
        }
        self.reference(path, ANY, false);
    }

    /// The paths in a macro's tokens, which syn does not parse: a name
    /// followed by `::`, a leading `::`, or `crate`, `self` or `super`, a
    /// group (`crate::{a, b::c}`) read as a `use` tree; and each other name
    /// alone (not a keyword, a field or method after `.`, a lifetime or a
    /// macro's `$` variable) as a [`Reference::bare`].
    fn tokens(&mut self, tokens: TokenStream) {
        let trees: Vec<TokenTree> = tokens.into_iter().collect();
        let mut i = 0;
        while i < trees.len() {
            if let TokenTree::Group(group) = &trees[i] {
                self.tokens(group.stream());
                i += 1;
                continue;
            }
            let colons_at = |at: usize| {
                matches!(trees.get(at), Some(TokenTree::Punct(p)) if p.as_char() == ':')
                    && matches!(trees.get(at + 1), Some(TokenTree::Punct(p)) if p.as_char() == ':')
            };
            let starts = matches!(&trees[i], TokenTree::Ident(id)
                if ["crate", "self", "super"].contains(&id.to_string().as_str()) || colons_at(i + 1) && !KEYWORDS.contains(&id.to_string().as_str()));
            let after_colons = i >= 2 && colons_at(i - 2);
            // `::name` after nothing, a punctuation or a keyword
            // (`<T>::name` and `a::name` go on a path).
            let leading = after_colons
                && (i == 2
                    || match &trees[i - 3] {
                        TokenTree::Punct(p) => p.as_char() != '>',
                        TokenTree::Ident(id) => KEYWORDS.contains(&id.to_string().as_str()),
                        _ => false,
                    });
            if !matches!(&trees[i], TokenTree::Ident(_))
                || after_colons && !leading
                || !after_colons && !starts
            {
                if let TokenTree::Ident(id) = &trees[i]
                    && !after_colons
                {
                    let name = id.to_string();
                    let marked = i >= 1
                        && matches!(&trees[i - 1], TokenTree::Punct(p)
                            if ['.', '\'', '$'].contains(&p.as_char()));
                    if !marked && !KEYWORDS.contains(&name.as_str()) {
                        self.bare(name);
                    }
                }
                i += 1;
                continue;
            }
            let mut path = if leading {
                vec![String::new()]
            } else {
                Vec::new()
            };
            let mut glob = false;
            while let Some(TokenTree::Ident(id)) = trees.get(i) {
                path.push(id.to_string());
                let colons = matches!(trees.get(i + 1), Some(TokenTree::Punct(p)) if p.as_char() == ':')
                    && matches!(trees.get(i + 2), Some(TokenTree::Punct(p)) if p.as_char() == ':');
                if !colons {
                    i += 1;
                    break;
                }
                i += 3;
                if matches!(trees.get(i), Some(TokenTree::Punct(p)) if p.as_char() == '*') {
                    glob = true;
                    i += 1;
                    break;
                }
                if let Some(TokenTree::Group(group)) = trees.get(i)
                    && group.delimiter() == proc_macro2::Delimiter::Brace
                {
                    let parser =
                        syn::punctuated::Punctuated::<syn::UseTree, syn::Token![,]>::parse_terminated;
                    if let Ok(items) = syn::parse::Parser::parse2(parser, group.stream()) {
                        let mut out = Vec::new();
                        for tree in &items {
                            leaves(&path, tree, &mut out);
                        }
                        for (leaf, glob) in out {
                            self.reference(leaf, if glob { TYPE } else { ANY }, glob);
                        }
                    }
                    i += 1;
                    break;
                }
            }
            self.reference(path, if glob { TYPE } else { ANY }, glob);
        }
    }
}

/// The leaves of a `use` tree under `prefix`, each with whether it is a
/// glob.
fn leaves(prefix: &[String], tree: &syn::UseTree, out: &mut Vec<(Vec<String>, bool)>) {
    let joined = |name: &syn::Ident| {
        let mut path = prefix.to_vec();
        if name != "self" {
            path.push(name.to_string());
        }
        path
    };
    match tree {
        syn::UseTree::Path(p) => leaves(&joined(&p.ident), &p.tree, out),
        syn::UseTree::Group(g) => {
            for tree in &g.items {
                leaves(prefix, tree, out);
            }
        }
        syn::UseTree::Name(n) => out.push((joined(&n.ident), false)),
        syn::UseTree::Rename(r) => out.push((joined(&r.ident), false)),
        syn::UseTree::Glob(_) => out.push((prefix.to_vec(), true)),
    }
}

/// The name an item defines, whether it is under a `cfg` and its
/// namespaces.
fn item_name(item: &syn::Item) -> Option<(String, bool, Ns)> {
    use syn::Item;
    let (ident, attrs, ns) = match item {
        Item::Const(i) => (&i.ident, &i.attrs, VALUE),
        Item::Enum(i) => (&i.ident, &i.attrs, TYPE),
        Item::ExternCrate(_) => return None,
        Item::Fn(i) => (&i.sig.ident, &i.attrs, VALUE),
        Item::Macro(i) => (i.ident.as_ref()?, &i.attrs, MACRO),
        Item::Mod(i) => (&i.ident, &i.attrs, TYPE),
        Item::Static(i) => (&i.ident, &i.attrs, VALUE),
        Item::Struct(i) => (
            &i.ident,
            &i.attrs,
            match i.fields {
                syn::Fields::Named(_) => TYPE,
                _ => TYPE | VALUE,
            },
        ),
        Item::Trait(i) => (&i.ident, &i.attrs, TYPE),
        Item::TraitAlias(i) => (&i.ident, &i.attrs, TYPE),
        Item::Type(i) => (&i.ident, &i.attrs, TYPE),
        Item::Union(i) => (&i.ident, &i.attrs, TYPE),
        _ => return None,
    };
    Some((ident.to_string(), under_cfg(attrs), ns))
}

/// The attributes of an expression.
fn expr_attrs(e: &syn::Expr) -> &[syn::Attribute] {
    use syn::Expr;
    match e {
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
        Expr::Infer(e) => &e.attrs,
        Expr::Let(e) => &e.attrs,
        Expr::Lit(e) => &e.attrs,
        Expr::Loop(e) => &e.attrs,
        Expr::Macro(e) => &e.attrs,
        Expr::Match(e) => &e.attrs,
        Expr::MethodCall(e) => &e.attrs,
        Expr::Paren(e) => &e.attrs,
        Expr::Path(e) => &e.attrs,
        Expr::Range(e) => &e.attrs,
        Expr::RawAddr(e) => &e.attrs,
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

fn under_cfg(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| attr.path().is_ident("cfg"))
}

impl<'ast> Visit<'ast> for Reader<'_> {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let test = self.test || super::source::is_test(super::source::item_attrs(item));
        if let Some((name, cfg, ns)) = item_name(item) {
            if !test {
                self.krate.defined.insert((self.file.clone(), name.clone()));
            }
            self.item(name, cfg, ns);
        }
        self.with_attrs(super::source::item_attrs(item), |r| {
            visit::visit_item(r, item);
        });
    }

    fn visit_item_trait(&mut self, t: &'ast syn::ItemTrait) {
        let methods = t
            .items
            .iter()
            .filter_map(|item| match item {
                syn::TraitItem::Fn(f) if !super::source::is_test(&f.attrs) => {
                    Some(f.sig.ident.to_string())
                }
                _ => None,
            })
            .collect();
        let public = match &t.vis {
            syn::Visibility::Public(_) => true,
            syn::Visibility::Restricted(r) => r.in_token.is_none() && r.path.is_ident("crate"),
            syn::Visibility::Inherited => false,
        };
        let module = self.here().clone();
        self.krate.traits.push(TraitDef {
            file: self.file.clone(),
            module,
            name: t.ident.to_string(),
            public,
            test: self.test,
            methods,
        });
        visit::visit_item_trait(self, t);
    }

    /// `extern crate self as name;` names the crate's root, from every
    /// module when it is at the root; `extern crate name as other;` names
    /// the crate `name`.
    fn visit_item_extern_crate(&mut self, e: &'ast syn::ItemExternCrate) {
        let rename = e.rename.as_ref().map(|(_, r)| r.to_string());
        self.cfg = under_cfg(&e.attrs);
        if e.ident == "self" {
            if self.here().is_empty() && self.blocks.is_empty() {
                self.krate.prelude.extend(rename.clone());
            }
            self.alias(rename, vec!["crate".to_owned()]);
        } else {
            let name = e.ident.to_string();
            if let Some(rename) = &rename
                && self.here().is_empty()
                && self.blocks.is_empty()
            {
                self.krate.externs.insert(rename.clone(), name.clone());
            }
            self.alias(
                Some(rename.unwrap_or(name.clone())),
                vec![String::new(), name],
            );
        }
        self.cfg = false;
        visit::visit_item_extern_crate(self, e);
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        let attrs: &[syn::Attribute] = match item {
            syn::ImplItem::Const(i) => &i.attrs,
            syn::ImplItem::Fn(i) => &i.attrs,
            syn::ImplItem::Type(i) => &i.attrs,
            syn::ImplItem::Macro(i) => &i.attrs,
            _ => &[],
        };
        if let syn::ImplItem::Fn(f) = item
            && !self.test
            && !super::source::is_test(attrs)
        {
            self.krate
                .defined
                .insert((self.file.clone(), f.sig.ident.to_string()));
        }
        self.with_attrs(attrs, |r| visit::visit_impl_item(r, item));
    }

    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        let attrs: &[syn::Attribute] = match item {
            syn::TraitItem::Const(i) => &i.attrs,
            syn::TraitItem::Fn(i) => &i.attrs,
            syn::TraitItem::Type(i) => &i.attrs,
            syn::TraitItem::Macro(i) => &i.attrs,
            _ => &[],
        };
        self.with_attrs(attrs, |r| visit::visit_trait_item(r, item));
    }

    fn visit_field(&mut self, f: &'ast syn::Field) {
        self.with_attrs(&f.attrs, |r| visit::visit_field(r, f));
    }

    fn visit_variant(&mut self, v: &'ast syn::Variant) {
        self.with_attrs(&v.attrs, |r| visit::visit_variant(r, v));
    }

    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        self.with_attrs(&arm.attrs, |r| visit::visit_arm(r, arm));
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        self.with_attrs(&local.attrs, |r| visit::visit_local(r, local));
    }

    fn visit_expr(&mut self, e: &'ast syn::Expr) {
        self.with_attrs(expr_attrs(e), |r| visit::visit_expr(r, e));
    }

    fn visit_stmt_macro(&mut self, m: &'ast syn::StmtMacro) {
        self.with_attrs(&m.attrs, |r| visit::visit_stmt_macro(r, m));
    }

    fn visit_field_value(&mut self, f: &'ast syn::FieldValue) {
        self.with_attrs(&f.attrs, |r| visit::visit_field_value(r, f));
    }

    fn visit_field_pat(&mut self, f: &'ast syn::FieldPat) {
        self.with_attrs(&f.attrs, |r| visit::visit_field_pat(r, f));
    }

    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        for attr in &m.attrs {
            self.visit_attribute(attr);
        }
        if self.here() == &["compose"] && !matches!(m.vis, syn::Visibility::Inherited) {
            self.krate.errors.push(format!(
                "{}: L9: the module crate::compose::{} is not private; outside the composition root, reach it through the root's re-exports",
                self.file, m.ident
            ));
        }
        let mut child = self.here().clone();
        child.push(m.ident.to_string());
        self.krate.modules.entry(child.clone()).or_default();
        let dir = format!("{}/{}", self.dir.last().expect("a dir"), m.ident);
        match &m.content {
            None => {
                let parent = self.dir.last().expect("a dir").clone();
                self.declared.push((parent, child, self.test));
            }
            Some((_, items)) => {
                let blocks = std::mem::take(&mut self.blocks);
                self.module.push(child);
                self.dir.push(dir);
                for item in items {
                    self.visit_item(item);
                }
                self.dir.pop();
                self.module.pop();
                self.blocks = blocks;
            }
        }
    }

    fn visit_item_use(&mut self, u: &'ast syn::ItemUse) {
        let prefix = if u.leading_colon.is_some() {
            vec![String::new()]
        } else {
            Vec::new()
        };
        self.cfg = under_cfg(&u.attrs);
        self.use_tree(&prefix, &u.tree);
        self.cfg = false;
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        self.blocks.push(self.krate.blocks.len());
        self.krate.blocks.push(Module::default());
        visit::visit_block(self, block);
        self.blocks.pop();
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.path_in(path, ANY);
    }

    fn visit_expr_path(&mut self, e: &'ast syn::ExprPath) {
        for attr in &e.attrs {
            self.visit_attribute(attr);
        }
        if let Some(qself) = &e.qself {
            self.visit_qself(qself);
        }
        self.path_in(&e.path, VALUE);
    }

    fn visit_type_path(&mut self, t: &'ast syn::TypePath) {
        if let Some(qself) = &t.qself {
            self.visit_qself(qself);
        }
        self.path_in(&t.path, TYPE);
    }

    fn visit_trait_bound(&mut self, b: &'ast syn::TraitBound) {
        if let Some(lifetimes) = &b.lifetimes {
            self.visit_bound_lifetimes(lifetimes);
        }
        self.path_in(&b.path, TYPE);
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        for attr in &i.attrs {
            self.visit_attribute(attr);
        }
        self.visit_generics(&i.generics);
        if let Some((_, path, _)) = &i.trait_ {
            self.path_in(path, TYPE);
        }
        self.visit_type(&i.self_ty);
        for item in &i.items {
            self.visit_impl_item(item);
        }
    }

    fn visit_expr_struct(&mut self, e: &'ast syn::ExprStruct) {
        for attr in &e.attrs {
            self.visit_attribute(attr);
        }
        if let Some(qself) = &e.qself {
            self.visit_qself(qself);
        }
        self.path_in(&e.path, TYPE);
        for field in &e.fields {
            self.visit_field_value(field);
        }
        if let Some(rest) = &e.rest {
            self.visit_expr(rest);
        }
    }

    fn visit_pat_struct(&mut self, p: &'ast syn::PatStruct) {
        for attr in &p.attrs {
            self.visit_attribute(attr);
        }
        if let Some(qself) = &p.qself {
            self.visit_qself(qself);
        }
        self.path_in(&p.path, TYPE);
        for field in &p.fields {
            self.visit_field_pat(field);
        }
        if let Some(rest) = &p.rest {
            self.visit_pat_rest(rest);
        }
    }

    fn visit_pat_tuple_struct(&mut self, p: &'ast syn::PatTupleStruct) {
        for attr in &p.attrs {
            self.visit_attribute(attr);
        }
        if let Some(qself) = &p.qself {
            self.visit_qself(qself);
        }
        self.path_in(&p.path, VALUE);
        for elem in &p.elems {
            self.visit_pat(elem);
        }
    }

    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        if attr.path().is_ident("doc") {
            return;
        }
        self.path_in(attr.path(), MACRO);
        match &attr.meta {
            syn::Meta::List(list) => self.tokens(list.tokens.clone()),
            syn::Meta::NameValue(nv) => self.visit_expr(&nv.value),
            syn::Meta::Path(_) => {}
        }
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        self.path_in(&m.path, MACRO);
        self.tokens(m.tokens.clone());
    }
}

/// A scope a name is looked up in: a module, or a block by its index in
/// [`Crate::blocks`].
#[derive(Clone, Debug, PartialEq, Eq)]
enum Scope {
    Module(ModulePath),
    Block(usize),
}

/// The lookups in progress, so that a cycle of `use`s and globs ends.
type Seen = Vec<(Scope, String)>;

impl Crate {
    /// `path` as written in `module` inside `blocks` (outermost first),
    /// its last segment in the namespaces `ns`, from the crate's root: one
    /// path for each definition its first segment may have, none for
    /// another crate, a local, a generic, an item of a block or a `super`
    /// past the root. A bare first segment is looked up from the innermost
    /// block around it out to the module: their `use`s of it, the module's
    /// item of it, the names their globs bring in, and the root's names of
    /// `extern crate self` ([`Crate::prelude`]). An item or a `use` under
    /// no `cfg` shadows the globs of its scope and the scopes around it in
    /// its namespaces.
    fn absolute(
        &self,
        module: &[String],
        blocks: &[usize],
        path: &[String],
        ns: Ns,
        seen: &mut Seen,
    ) -> BTreeSet<ModulePath> {
        let mut found = BTreeSet::new();
        let Some(first) = path.first() else {
            return found;
        };
        let mut want = if path.len() > 1 { TYPE } else { ns };
        match first.as_str() {
            "" => {
                if path.get(1).is_some_and(|name| self.prelude.contains(name)) {
                    found.insert(path[2..].to_vec());
                } else {
                    found.insert(self.extern_path(&path[1..]));
                }
            }
            "Self" => {}
            "crate" => {
                found.insert(path[1..].to_vec());
            }
            "self" if path.get(1).is_some_and(|s| s == "super") => {
                return self.absolute(module, blocks, &path[1..], ns, seen);
            }
            "self" => {
                found.insert([module, &path[1..]].concat());
            }
            "super" => {
                let mut base = module.to_vec();
                let mut rest = path;
                while rest.first().is_some_and(|s| s == "super") {
                    if base.pop().is_none() {
                        return found;
                    }
                    rest = &rest[1..];
                }
                found.insert([&base[..], rest].concat());
            }
            name if !self.names.contains(name) => {
                found.insert(self.extern_path(path));
            }
            name => {
                for (k, &block) in blocks.iter().enumerate().rev() {
                    let scope = &self.blocks[block];
                    let open = want & !ns_of(&scope.fixed, name);
                    let key = (Scope::Block(block), name.to_owned());
                    if !self.in_progress(seen, &key) {
                        seen.push(key);
                        let outer = &blocks[..=k];
                        for target in scope.aliases.get(name).into_iter().flatten() {
                            for target in self.absolute(module, outer, target, want, seen) {
                                found.insert([&target[..], &path[1..]].concat());
                            }
                        }
                        if open != 0 {
                            found.extend(self.through_globs(
                                module,
                                outer,
                                &scope.globs,
                                path,
                                open,
                                seen,
                            ));
                        }
                        seen.pop();
                    }
                    want = open;
                    if want == 0 {
                        return found;
                    }
                }
                let Some(here) = self.modules.get(module) else {
                    return found;
                };
                if ns_of(&here.items, name) & want != 0 || here.aliases.contains_key(name) {
                    found.insert([module, path].concat());
                }
                want &= !ns_of(&here.fixed, name);
                let key = (Scope::Module(module.to_vec()), name.to_owned());
                if want != 0 && !self.in_progress(seen, &key) {
                    seen.push(key);
                    found.extend(self.through_globs(module, &[], &here.globs, path, want, seen));
                    seen.pop();
                }
                if want & TYPE != 0 && self.prelude.contains(name) {
                    found.insert(path[1..].to_vec());
                }
                if found.is_empty() && want != 0 {
                    found.insert(self.extern_path(path));
                }
            }
        }
        found
    }

    /// `path` under each of `globs` (written in `module` inside `blocks`)
    /// whose module has its first segment in one of the namespaces `ns`.
    fn through_globs(
        &self,
        module: &[String],
        blocks: &[usize],
        globs: &[Vec<String>],
        path: &[String],
        ns: Ns,
        seen: &mut Seen,
    ) -> BTreeSet<ModulePath> {
        let mut found = BTreeSet::new();
        for glob in globs {
            for base in self.absolute(module, blocks, glob, TYPE, seen) {
                for base in self.resolve(base, TYPE, seen) {
                    if self.defines(&base, &path[0], ns, seen) {
                        found.insert([&base[..], path].concat());
                    }
                }
            }
        }
        found
    }

    /// Whether the module `module` has the name `name` in one of the
    /// namespaces `ns`: an item, a `use`, or a name one of its globs brings
    /// in.
    fn defines(&self, module: &[String], name: &str, ns: Ns, seen: &mut Seen) -> bool {
        if !self.names.contains(name) {
            return false;
        }
        let key = (module.to_vec(), name.to_owned(), ns);
        self.cached(&self.memo.defined, key, seen, |krate, seen| {
            krate.defines_uncached(module, name, ns, seen)
        })
    }

    fn defines_uncached(&self, module: &[String], name: &str, ns: Ns, seen: &mut Seen) -> bool {
        let Some(here) = self.modules.get(module) else {
            return false;
        };
        if ns_of(&here.items, name) & ns != 0 || here.aliases.contains_key(name) {
            return true;
        }
        let key = (Scope::Module(module.to_vec()), name.to_owned());
        if here.globs.is_empty() {
            return false;
        }
        if self.in_progress(seen, &key) {
            return false;
        }
        seen.push(key);
        let found = here.globs.iter().any(|glob| {
            let bases: BTreeSet<ModulePath> = self
                .absolute(module, &[], glob, TYPE, seen)
                .into_iter()
                .flat_map(|base| self.resolve(base, TYPE, seen))
                .collect();
            bases.iter().any(|base| self.defines(base, name, ns, seen))
        });
        seen.pop();
        found
    }

    /// `path` from the root through the `use`s and globs to the module that
    /// defines each of its segments, as far as the modules go, its last
    /// segment in the namespaces `ns`: one path for each definition a
    /// segment may have (its item, each of its `use`s and each glob that
    /// brings it in).
    fn resolve(&self, path: ModulePath, ns: Ns, seen: &mut Seen) -> BTreeSet<ModulePath> {
        self.cached(
            &self.memo.resolved,
            (path.clone(), ns),
            seen,
            |krate, seen| krate.resolve_from(path, 0, ns, seen),
        )
    }

    /// Whether `key` is a lookup in progress, noting the cut when it is.
    fn in_progress(&self, seen: &Seen, key: &(Scope, String)) -> bool {
        let Some(at) = seen.iter().position(|k| k == key) else {
            return false;
        };
        let cut = self.memo.cut.get().map_or(at, |cut| cut.min(at));
        self.memo.cut.set(Some(cut));
        true
    }

    /// `compute`'s value for `key` in `table`, kept when no cut reached a
    /// lookup in progress before it began.
    fn cached<K: Eq + std::hash::Hash, V: Clone>(
        &self,
        table: &RefCell<HashMap<K, V>>,
        key: K,
        seen: &mut Seen,
        compute: impl FnOnce(&Self, &mut Seen) -> V,
    ) -> V {
        if let Some(hit) = table.borrow().get(&key) {
            return hit.clone();
        }
        let depth = seen.len();
        let outer = self.memo.cut.take();
        let value = compute(self, seen);
        let cut = self.memo.cut.get();
        if cut.is_none_or(|cut| cut >= depth) {
            table.borrow_mut().insert(key, value.clone());
        }
        self.memo.cut.set(match (outer, cut) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        });
        value
    }

    /// `path` (as written, from a crate's name) as a path of another crate,
    /// a root `extern crate` rename replaced by the crate's name.
    fn extern_path(&self, path: &[String]) -> ModulePath {
        let mut out = vec![EXTERN.to_owned()];
        out.extend(path.iter().cloned());
        if let Some(name) = out.get(1).and_then(|first| self.externs.get(first)) {
            out[1] = name.clone();
        }
        out
    }

    /// [`Crate::resolve`] of the segments of `path` from `start` on.
    fn resolve_from(
        &self,
        path: ModulePath,
        start: usize,
        ns: Ns,
        seen: &mut Seen,
    ) -> BTreeSet<ModulePath> {
        for i in start..path.len() {
            let Some(module) = self.modules.get(&path[..i]) else {
                break;
            };
            let name = &path[i];
            let here = if i + 1 < path.len() { TYPE } else { ns };
            let item = ns_of(&module.items, name) & here != 0;
            let open = here & !ns_of(&module.fixed, name);
            let aliases = module.aliases.get(name);
            if aliases.is_none() && (open == 0 || module.globs.is_empty()) {
                if item {
                    continue;
                }
                break;
            }
            let key = (Scope::Module(path[..i].to_vec()), name.clone());
            if self.in_progress(seen, &key) {
                if item {
                    continue;
                }
                break;
            }
            seen.push(key);
            let mut targets = BTreeSet::new();
            for target in aliases.into_iter().flatten() {
                for target in self.absolute(&path[..i], &[], target, here, seen) {
                    targets.insert([&target[..], &path[i + 1..]].concat());
                }
            }
            for glob in module.globs.iter().filter(|_| open != 0) {
                for base in self.absolute(&path[..i], &[], glob, TYPE, seen) {
                    for base in self.resolve(base, TYPE, seen) {
                        if self.defines(&base, name, open, seen) {
                            targets.insert([&base[..], &path[i..]].concat());
                        }
                    }
                }
            }
            let mut resolved = BTreeSet::new();
            for target in targets {
                resolved.extend(self.resolve(target, ns, seen));
            }
            seen.pop();
            if item {
                resolved.extend(self.resolve_from(path.clone(), i + 1, ns, seen));
            }
            if resolved.iter().all(|p| is_extern(p)) {
                resolved.insert(path);
            }
            return resolved;
        }
        BTreeSet::from([path])
    }
}

fn is_extern(path: &[String]) -> bool {
    path.first().is_some_and(|s| s == EXTERN)
}

/// The item of [`DOMAIN_FORBIDS`] `to` names, as L2 prints it.
fn domain_forbids(to: &[String]) -> Option<String> {
    let crate_path = to.strip_prefix(&[EXTERN.to_owned()][..])?;
    DOMAIN_FORBIDS
        .iter()
        .find(|item| {
            item.len() <= crate_path.len() && item.iter().zip(crate_path).all(|(a, b)| a == b)
        })
        .map(|item| item.join("::"))
}

fn show(path: &[String]) -> String {
    if path.is_empty() {
        "crate".to_owned()
    } else {
        format!("crate::{}", path.join("::"))
    }
}

/// The layer a module path is in, `None` outside the layers.
fn layer(path: &[String]) -> Option<Layer> {
    match path.first().map(String::as_str) {
        Some("domain") => Some(Layer::Domain),
        Some("application") => Some(Layer::Application),
        Some("infrastructure") => Some(Layer::Infrastructure),
        Some("compose") => Some(Layer::Compose),
        _ => None,
    }
}

/// The rule a reference from `from` to `to` breaks, with the module it
/// names, by the layer rules L1, L3, L6 and L8.
fn layer_rule(
    from: Layer,
    to: &[String],
    shared: &[SharedModule],
    top: &BTreeSet<String>,
) -> Option<(Rule, String)> {
    let first = to.first()?;
    if !top.contains(first) {
        return None;
    }
    let rule = match from {
        Layer::Domain => Rule::L1,
        Layer::Application => Rule::L3,
        Layer::Infrastructure => Rule::L6,
        Layer::Compose => return None,
    };
    let allowed = match layer(to) {
        Some(to) => match from {
            Layer::Domain => to == Layer::Domain,
            Layer::Application => matches!(to, Layer::Domain | Layer::Application),
            Layer::Infrastructure => to != Layer::Compose,
            Layer::Compose => true,
        },
        None => {
            if let Some(row) = shared.iter().find(|s| s.module == first) {
                row.layers.contains(&from)
            } else {
                return Some((Rule::L8, show(&to[..1])));
            }
        }
    };
    (!allowed).then(|| (rule, show(&to[..1])))
}

/// The rule L9 for a reference from `from` to `to` (a glob of `to` when
/// `glob`): a context module of `src/compose/` names another module of it,
/// or globs the root.
fn compose_rule(
    from: &[String],
    to: &[String],
    glob: bool,
    loops: &BTreeSet<String>,
    children: &BTreeSet<String>,
) -> Option<String> {
    let [compose, context, ..] = from else {
        return None;
    };
    if compose != "compose" || loops.contains(context) || !children.contains(context) {
        return None;
    }
    if glob && to == ["compose"] {
        return Some("crate::compose::*".to_owned());
    }
    match to {
        [root, other, ..] if root == "compose" && other != context && children.contains(other) => {
            Some(show(&to[..2]))
        }
        _ => None,
    }
}

/// Every failure of the library in `files` against the records, one line
/// each.
pub(super) fn check(files: &BTreeMap<String, String>, records: &ModuleRecords) -> Vec<String> {
    let krate = read(files);
    let mut errors: BTreeSet<String> = krate.errors.iter().cloned().collect();
    let top: BTreeSet<String> = krate
        .modules
        .keys()
        .filter(|m| m.len() == 1)
        .map(|m| m[0].clone())
        .collect();
    let children: BTreeSet<String> = krate
        .modules
        .keys()
        .filter(|m| m.len() == 2 && m[0] == "compose")
        .map(|m| m[1].clone())
        .collect();
    let loops: BTreeSet<String> = super::ownership::COMPOSE_LOOP
        .iter()
        .map(|s| s.to_string())
        .collect();
    for row in records.shared {
        if row.reason.trim().is_empty()
            || !top.contains(row.module)
            || layer(&[row.module.to_owned()]).is_some()
        {
            errors.insert(format!(
                "ownership::SHARED_MODULES: {}: want a module of the crate's root outside the layers, and a reason",
                row.module
            ));
        }
    }
    let ports = ports::PortCheck::new(&krate, files, &records.ports, &mut errors);
    let mut found = BTreeSet::new();
    for reference in &krate.references {
        let Some(from) = layer(&reference.module) else {
            continue;
        };
        if !reference.bare
            && let Some(written) = ports.c8(reference)
        {
            found.insert((reference.file.clone(), Rule::C8, written));
        }
        let mut seen = Vec::new();
        let written = krate.absolute(
            &reference.module,
            &reference.blocks,
            &reference.path,
            reference.ns,
            &mut seen,
        );
        let mut paths = BTreeSet::new();
        for to in written {
            paths.extend(krate.resolve(to.clone(), reference.ns, &mut seen));
            paths.insert(to);
        }
        for to in paths.iter().filter(|_| !reference.bare) {
            if let Some((rule, module)) = layer_rule(from, to, records.shared, &top) {
                found.insert((reference.file.clone(), rule, module));
            }
            if let Some(module) =
                compose_rule(&reference.module, to, reference.glob, &loops, &children)
            {
                found.insert((reference.file.clone(), Rule::L9, module));
            }
            if from == Layer::Domain
                && !reference.test
                && let Some(item) = domain_forbids(to)
            {
                found.insert((reference.file.clone(), Rule::L2, item));
            }
        }
        if !reference.test {
            for port in ports.c5(&reference.file, &paths) {
                found.insert((reference.file.clone(), Rule::C5, port));
            }
        }
    }
    let mut listed = BTreeSet::new();
    for row in records.allowed {
        let at = format!(
            "ownership::MODULES_ALLOWED: {} {:?} {}",
            row.path, row.rule, row.reference
        );
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
        if !listed.insert((row.path.to_owned(), row.rule, row.reference.to_owned())) {
            errors.insert(format!("{at}: listed twice"));
        }
    }
    for key @ (path, rule, reference) in &found {
        if !listed.contains(key) {
            errors.insert(format!(
                "{path}: {rule:?} forbids {reference}; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason"
            ));
        }
    }
    for key @ (path, rule, reference) in &listed {
        if !found.contains(key) {
            errors.insert(format!(
                "ownership::MODULES_ALLOWED: stale row {path} {rule:?} {reference} (no such reference any more); remove it"
            ));
        }
    }
    errors.into_iter().collect()
}

#[test]
fn the_layers_the_composition_contexts_and_the_ports_reference_only_what_they_may() {
    use super::ownership as o;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let files = super::files(root, "src", "rs").into_iter().collect();
    let records = ModuleRecords {
        shared: o::SHARED_MODULES,
        allowed: o::MODULES_ALLOWED,
        ports: ports::PortRecords {
            modules: o::MODULES,
            stores: o::STORES,
            ports: o::PORTS,
            outer_ports: o::OUTER_PORTS,
            not_ports: o::NOT_PORTS,
            methods: o::METHODS,
            lent: o::LENT,
            port_modules: o::PORT_MODULES,
        },
    };
    let errors = check(&files, &records);
    assert!(
        errors.is_empty(),
        "the module references (docs/design/architecture.md, L1, L2, L3, L6, L8, L9, C5, C7 and C8):\n{}",
        errors.join("\n")
    );
}
