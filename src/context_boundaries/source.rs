//! A Rust source read as syntax for the boundary tests: its string
//! literals outside test code with the fns they are in, its fns outside
//! test code, and the files its `#[cfg(test)] mod name;` declares. Test code
//! is an item (or a field, a variant, an arm, a `let`) under a `cfg` that
//! requires `test` ([`is_test`]), and a file under `#![cfg(test)]`. Comments and attributes (doc comments too) are not read;
//! the tokens of a macro are read for their string literals only.

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
