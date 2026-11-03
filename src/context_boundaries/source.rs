//! A Rust source read as syntax for the boundary tests: its string
//! literals outside test code with the fns they are in, its fns outside
//! test code, and the files its `#[cfg(test)] mod name;` declares. Test code
//! is an item (or a field, a variant, an arm, a `let`, an expression, a
//! macro statement) under a `cfg` that requires `test` ([`is_test`]), and a
//! file under `#![cfg(test)]`. Comments and attributes (doc comments too)
//! are not read; the tokens of a macro are read for their string literals
//! only.

use proc_macro2::TokenTree;
use std::collections::BTreeSet;
use syn::visit::{self, Visit};
use syn::{Attribute, Lit, Meta};

/// A string literal of production code and the fns around it, outermost
/// first (closures belong to their fn).
#[derive(Debug)]
pub(super) struct Literal {
    pub(super) text: String,
    pub(super) fns: Vec<String>,
}

#[derive(Debug, Default)]
pub(super) struct Source {
    pub(super) literals: Vec<Literal>,
    /// The names of the fns of production code, methods and trait methods
    /// included.
    pub(super) fns: BTreeSet<String>,
    /// The test modules this file declares without a body, as module paths
    /// below the file (`inner/name` for `mod inner { #[cfg(test)] mod name; }`).
    pub(super) test_modules: Vec<String>,
}

pub(super) fn read(text: &str) -> syn::Result<Source> {
    let file = syn::parse_file(text)?;
    let mut reader = Reader::default();
    if !is_test(&file.attrs) {
        reader.visit_file(&file);
    }
    Ok(reader.source)
}

/// Whether `attrs` hold a `cfg` whose predicate requires `test`, so that
/// what it is on is in no `test=false` build: `test`, an `all(...)` with an
/// argument that requires it, or an `any(...)` all of whose arguments do
/// (`cfg(any(test))`, `cfg(all(unix, any(test)))`). `not(...)`, a key and
/// value and an `any(...)` with an argument that does not require `test`
/// may hold without it.
pub(super) fn is_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg") && attr.parse_args::<Meta>().is_ok_and(|m| requires_test(&m))
    })
}

fn requires_test(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list) => {
            let Ok(args) = list.parse_args_with(
                syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
            ) else {
                return false;
            };
            if list.path.is_ident("all") {
                args.iter().any(requires_test)
            } else if list.path.is_ident("any") {
                args.iter().all(requires_test)
            } else {
                false
            }
        }
        Meta::NameValue(_) => false,
    }
}

/// The attributes of an expression (`#[cfg(test)] { .. }` and the like).
pub(super) fn expr_attrs(expr: &syn::Expr) -> &[syn::Attribute] {
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

pub(super) fn item_attrs(item: &syn::Item) -> &[Attribute] {
    use syn::Item;
    match item {
        Item::Const(i) => &i.attrs,
        Item::Enum(i) => &i.attrs,
        Item::ExternCrate(i) => &i.attrs,
        Item::Fn(i) => &i.attrs,
        Item::ForeignMod(i) => &i.attrs,
        Item::Impl(i) => &i.attrs,
        Item::Macro(i) => &i.attrs,
        Item::Mod(i) => &i.attrs,
        Item::Static(i) => &i.attrs,
        Item::Struct(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        Item::TraitAlias(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        Item::Union(i) => &i.attrs,
        Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

#[derive(Default)]
struct Reader {
    source: Source,
    fns: Vec<String>,
    modules: Vec<String>,
}

impl Reader {
    fn in_fn(&mut self, name: &syn::Ident, walk: impl FnOnce(&mut Self)) {
        self.source.fns.insert(name.to_string());
        self.fns.push(name.to_string());
        walk(self);
        self.fns.pop();
    }

    fn literal(&mut self, text: String) {
        self.source.literals.push(Literal {
            text,
            fns: self.fns.clone(),
        });
    }

    /// The string literals of a macro's tokens, which syn does not parse.
    fn tokens(&mut self, tokens: proc_macro2::TokenStream) {
        for tree in tokens {
            match tree {
                TokenTree::Literal(literal) => match Lit::new(literal) {
                    Lit::Str(s) => self.literal(s.value()),
                    Lit::ByteStr(s) => {
                        self.literal(String::from_utf8_lossy(&s.value()).into_owned())
                    }
                    _ => {}
                },
                TokenTree::Group(group) => self.tokens(group.stream()),
                _ => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for Reader {
    fn visit_attribute(&mut self, _: &'ast Attribute) {}

    fn visit_item(&mut self, item: &'ast syn::Item) {
        if is_test(item_attrs(item)) {
            if let syn::Item::Mod(m) = item
                && m.content.is_none()
            {
                let mut path = self.modules.clone();
                path.push(m.ident.to_string());
                self.source.test_modules.push(path.join("/"));
            }
            return;
        }
        visit::visit_item(self, item);
    }

    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        self.modules.push(m.ident.to_string());
        visit::visit_item_mod(self, m);
        self.modules.pop();
    }

    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        self.in_fn(&f.sig.ident, |r| visit::visit_item_fn(r, f));
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
        self.in_fn(&f.sig.ident, |r| visit::visit_impl_item_fn(r, f));
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
        self.in_fn(&f.sig.ident, |r| visit::visit_trait_item_fn(r, f));
    }

    fn visit_field(&mut self, f: &'ast syn::Field) {
        if !is_test(&f.attrs) {
            visit::visit_field(self, f);
        }
    }

    fn visit_variant(&mut self, v: &'ast syn::Variant) {
        if !is_test(&v.attrs) {
            visit::visit_variant(self, v);
        }
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

    fn visit_lit_str(&mut self, s: &'ast syn::LitStr) {
        self.literal(s.value());
    }

    fn visit_lit_byte_str(&mut self, s: &'ast syn::LitByteStr) {
        self.literal(String::from_utf8_lossy(&s.value()).into_owned());
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        self.tokens(m.tokens.clone());
    }
}

/// The files of `rust` (paths from the repository root, with their text)
/// that are production code, parsed: without those a `#[cfg(test)] mod name;`
/// of another declares. A file that does not parse is an error line.
pub(super) fn production_files(
    rust: &[(String, String)],
) -> (Vec<(&str, syn::File)>, std::collections::BTreeSet<String>) {
    let mut errors = BTreeSet::new();
    let mut read = Vec::new();
    let mut test_paths = Vec::new();
    for (path, text) in rust {
        let parsed = syn::parse_file(text).and_then(|file| Ok((self::read(text)?, file)));
        match parsed {
            Ok((source, file)) => {
                let dir = super::children_dir(path);
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
    read.retain(|(path, _)| !test_paths.iter().any(|test| super::covers(test, path)));
    (read, errors)
}

/// The fns of `file` that use the identifier `name` outside its test code:
/// in a path (called or not), a `use`, a type or a macro's tokens. Comments
/// and attributes (doc comments too) are not read. A use is at the innermost
/// fn around it (closures belong to their fn), named `Type::method` in an
/// `impl` or a trait, `-` outside any fn.
pub(super) fn name_uses(file: &syn::File, name: &str) -> BTreeSet<String> {
    let mut reader = NameReader {
        name,
        owners: Vec::new(),
        fns: Vec::new(),
        found: BTreeSet::new(),
    };
    if !is_test(&file.attrs) {
        reader.visit_file(file);
    }
    reader.found
}

struct NameReader<'n> {
    name: &'n str,
    /// The self type of each `impl` (or the trait) around, innermost last.
    owners: Vec<String>,
    fns: Vec<String>,
    found: BTreeSet<String>,
}

impl NameReader<'_> {
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
                TokenTree::Ident(ident) if ident == self.name => self.found(),
                _ => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for NameReader<'_> {
    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {}

    fn visit_ident(&mut self, ident: &'ast proc_macro2::Ident) {
        if ident == self.name {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_outside_test_code_carry_their_fns() {
        let source = read(
            r##"
//! "UPDATE comment SET x = 1"
/// "UPDATE doc SET x = 1"
const TOP: &str = "top";
fn outer() {
    let run = || conn.execute(r#"raw"#);
    fn inner() { format!("in {}", b"bytes"); }
}
impl S { fn method(&self) { "method"; } }
trait T { fn provided(&self) { "provided"; } fn required(&self); }
#[cfg(test)]
mod tests { fn t() { "test"; } }
#[cfg(all(test, unix))]
fn unix_test() { "unix test"; }
#[cfg(all(unix, any(test)))]
fn nested_test() { "nested test"; }
#[cfg(any(test, unix))]
fn maybe_test() { "maybe test"; }
#[cfg(test)]
mod helpers;
mod inner { #[cfg(test)] mod deeper; }
fn arms(x: u8) {
    match x {
        #[cfg(test)]
        0 => "test arm",
        _ => "arm",
    };
}
"##,
        )
        .unwrap();
        let literals: Vec<(String, String)> = source
            .literals
            .iter()
            .map(|l| (l.text.clone(), l.fns.join("::")))
            .collect();
        let want = [
            ("top", ""),
            ("raw", "outer"),
            ("in {}", "outer::inner"),
            ("bytes", "outer::inner"),
            ("method", "method"),
            ("provided", "provided"),
            ("maybe test", "maybe_test"),
            ("arm", "arms"),
        ];
        let want: Vec<(String, String)> = want
            .iter()
            .map(|(t, f)| (t.to_string(), f.to_string()))
            .collect();
        assert_eq!(literals, want);
        let fns: Vec<&str> = source.fns.iter().map(String::as_str).collect();
        assert_eq!(
            fns,
            [
                "arms",
                "inner",
                "maybe_test",
                "method",
                "outer",
                "provided",
                "required"
            ]
        );
        assert_eq!(source.test_modules, ["helpers", "inner/deeper"]);
    }

    #[test]
    fn a_file_under_cfg_test_has_nothing_to_read() {
        let source = read("#![cfg(test)]\nfn f() { \"UPDATE t SET x = 1\"; }\n").unwrap();
        assert!(source.literals.is_empty() && source.fns.is_empty());
    }
}
