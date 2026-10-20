//! The rules C5 and C8 of docs/design/architecture.md on the module
//! references [`super::check`] resolves, and C7 for the records they read.
//!
//! - The context of a file is the one its path tells
//!   (`src/application/ports/<context>.rs`,
//!   `src/application/queue_reads/<context>.rs`, `src/compose/<context>.rs`),
//!   else the longest path of [`ModuleContext`] and [`Store`] that covers
//!   it; a file with neither has no context.
//! - A port is a trait known by the path of the module that defines it, so
//!   a re-export (`crate::application::RunRecovery`) or a `use` under
//!   another name names the same port, and a trait of the same name
//!   elsewhere is another one. A port of `ports/<context>.rs` is owned by
//!   that context.
//! - C5: outside test code, in a file of a context other than `shared`, a
//!   reference whose path (as written or through the `use`s to the
//!   definition) is, or starts with, a port that another context but
//!   `shared` owns is a violation unless the port is public to the file's
//!   context; a partial port is public, by its name, to every context one
//!   of its methods names. Its reference is the port's name.
//! - C8: in the module of `src/application/ports/<context>.rs` (not its
//!   inline modules), test code too, a path written `super::<name>` whose
//!   name is not the module of `shared` or of a context [`PortModules`]
//!   lets it name, the glob `super::*`, and a path written through
//!   `crate::application::ports` or `super::super::ports`. A port is named
//!   by its module (`super::shared::Clock`), never through the re-exports.
//! - C7: each pub (or `pub(crate)`) trait outside test code of
//!   `src/application/ports/<context>.rs` (but `mod.rs`) has a [`Port`] row,
//!   and each of `src/application/` outside `ports/` an [`OuterPort`] or a
//!   [`NotPort`] row of its file; every row names what the code has (a
//!   trait of its file, a method its body declares, a file or a directory
//!   of one, a name its file defines), once.
//!
//! The overall review keeps the method-level references to a partial port
//! (whether a context calls a method not public to it, which needs the
//! types), the files with no context, and whom a [`Lent`] value is lent to.

use super::super::ownership::{
    Context, Lent, Method, ModuleContext, NotPort, OuterPort, Port, PortModules, Publicity, Store,
    To,
};
use super::{Crate, ModulePath, Reference};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// The records the check reads.
pub(in super::super) struct PortRecords<'a> {
    pub(in super::super) modules: &'a [ModuleContext],
    pub(in super::super) stores: &'a [Store],
    pub(in super::super) ports: &'a [Port],
    pub(in super::super) outer_ports: &'a [OuterPort],
    pub(in super::super) not_ports: &'a [NotPort],
    pub(in super::super) methods: &'a [Method],
    pub(in super::super) lent: &'a [Lent],
    pub(in super::super) port_modules: &'a [PortModules],
}

/// Whom a port is public to: `None` for all, else these contexts.
type Open = Option<BTreeSet<Context>>;

/// A port the code defines, by the path of its definition.
struct Known {
    name: String,
    owner: Context,
    open: Open,
}

/// The records matched against the library, ready for each reference.
pub(super) struct PortCheck<'a> {
    records: &'a PortRecords<'a>,
    ports: HashMap<ModulePath, Known>,
}

const CONTEXTS: [(&str, Context); 5] = [
    ("planning", Context::Planning),
    ("execution", Context::Execution),
    ("observation", Context::Observation),
    ("host", Context::Host),
    ("shared", Context::Shared),
];

fn named(name: &str) -> Option<Context> {
    CONTEXTS.iter().find(|(n, _)| *n == name).map(|&(_, c)| c)
}

fn name_of(context: Context) -> &'static str {
    CONTEXTS
        .iter()
        .find(|(_, c)| *c == context)
        .map_or("", |(n, _)| n)
}

/// The context `path` tells.
fn derived(path: &str) -> Option<Context> {
    [
        "src/application/ports/",
        "src/application/queue_reads/",
        "src/compose/",
    ]
    .iter()
    .find_map(|dir| path.strip_prefix(dir))
    .and_then(|rest| rest.strip_suffix(".rs"))
    .filter(|name| !name.contains('/'))
    .and_then(named)
}

/// The context of the port module `path` (`src/application/ports/<context>.rs`).
fn port_module(path: &str) -> Option<Context> {
    path.starts_with("src/application/ports/")
        .then(|| derived(path))
        .flatten()
}

fn add(open: &mut Open, to: To) {
    match (open.as_mut(), to) {
        (_, To::All) => *open = None,
        (Some(set), To::Only(contexts)) => set.extend(contexts),
        (None, To::Only(_)) => {}
    }
}

/// A port row by its name: its publicity (`None` for a not-port), its file
/// (`None` for a port of `ports/`) and its owner where the row tells it.
type Row = (Option<Publicity>, Option<&'static str>, Option<Context>);

fn publicity_errors(publicity: Publicity, at: &str, errors: &mut BTreeSet<String>) {
    if let Publicity::Public(to) = publicity
        && !valid(to)
    {
        errors.insert(format!("{at}: want Public(All) or Public(Only(contexts))"));
    }
}

fn valid(to: To) -> bool {
    !matches!(to, To::Only(contexts) if contexts.is_empty())
}

impl<'a> PortCheck<'a> {
    /// Matches the records against the library's traits, definitions and
    /// files, each failure into `errors`.
    pub(super) fn new(
        krate: &Crate,
        files: &BTreeMap<String, String>,
        records: &'a PortRecords<'a>,
        errors: &mut BTreeSet<String>,
    ) -> Self {
        let mut check = PortCheck {
            records,
            ports: HashMap::new(),
        };
        check.modules(files, errors);
        check.lent(krate, errors);
        check.ports(krate, errors);
        check.port_modules(errors);
        check
    }

    fn modules(&self, files: &BTreeMap<String, String>, errors: &mut BTreeSet<String>) {
        let mut seen = BTreeSet::new();
        for row in self.records.modules {
            let at = format!("ownership::MODULES: {}", row.path);
            if !row.path.starts_with("src/application/") {
                errors.insert(format!("{at}: a module row is under src/application/"));
            } else if let Some(context) = derived(row.path) {
                errors.insert(format!(
                    "{at}: the path tells its context {}; remove the row",
                    name_of(context)
                ));
            }
            if !seen.insert(row.path) {
                errors.insert(format!("{at}: listed twice"));
            }
            if !files
                .keys()
                .any(|file| super::super::covers(row.path, file))
            {
                errors.insert(format!(
                    "{at}: C7: no Rust file or directory of one; remove the row"
                ));
            }
        }
    }

    fn lent(&self, krate: &Crate, errors: &mut BTreeSet<String>) {
        let mut seen = BTreeSet::new();
        for row in self.records.lent {
            let at = format!("ownership::LENT: {} {}", row.path, row.name);
            if !valid(row.to) || matches!(row.to, To::Only(to) if to.contains(&row.owner)) {
                errors.insert(format!(
                    "{at}: want To::All or contexts other than its owner"
                ));
            }
            if !seen.insert((row.path, row.name)) {
                errors.insert(format!("{at}: listed twice"));
            }
            if !krate
                .defined
                .contains(&(row.path.to_owned(), row.name.to_owned()))
            {
                errors.insert(format!(
                    "{at}: C7: {} does not define {} outside test code",
                    row.path, row.name
                ));
            }
        }
    }

    fn port_modules(&self, errors: &mut BTreeSet<String>) {
        let mut seen = BTreeSet::new();
        for row in self.records.port_modules {
            let at = format!("ownership::PORT_MODULES: {}", name_of(row.context));
            if row.reason.trim().is_empty() || row.names.is_empty() {
                errors.insert(format!("{at}: want the contexts and a reason"));
            }
            if !seen.insert(name_of(row.context)) {
                errors.insert(format!("{at}: listed twice"));
            }
        }
    }

    /// The port rows against the traits, and the ports by their paths.
    fn ports(&mut self, krate: &Crate, errors: &mut BTreeSet<String>) {
        let records = self.records;
        let mut rows: BTreeMap<&str, Row> = BTreeMap::new();
        let mut row =
            |name: &'static str, at: String, value: Row, errors: &mut BTreeSet<String>| {
                if rows.insert(name, value).is_some() {
                    errors.insert(format!("{at}: the name {name} has another row"));
                }
            };
        for port in records.ports {
            let at = format!("ownership::PORTS: {}", port.name);
            publicity_errors(port.publicity, &at, errors);
            row(port.name, at, (Some(port.publicity), None, None), errors);
        }
        for port in records.outer_ports {
            let at = format!("ownership::OUTER_PORTS: {} {}", port.path, port.name);
            publicity_errors(port.publicity, &at, errors);
            if port.path.starts_with("src/application/ports/") {
                errors.insert(format!("{at}: a trait of ports/ is a PORTS row"));
            }
            let owner = match (port.owner, derived(port.path)) {
                (Some(_), Some(_)) => {
                    errors.insert(format!("{at}: the path tells the owner; write None"));
                    None
                }
                (None, None) => {
                    errors.insert(format!("{at}: the path does not tell the owner; write it"));
                    None
                }
                (owner, by_path) => owner.or(by_path),
            };
            row(
                port.name,
                at,
                (Some(port.publicity), Some(port.path), owner),
                errors,
            );
        }
        for port in records.not_ports {
            let at = format!("ownership::NOT_PORTS: {} {}", port.path, port.name);
            if port.reason.trim().is_empty() {
                errors.insert(format!("{at}: a not-port needs a reason"));
            }
            row(port.name, at, (None, Some(port.path), None), errors);
        }
        // The traits of the code against the rows.
        let mut defined: BTreeMap<&str, &super::TraitDef> = BTreeMap::new();
        for def in krate.traits.iter().filter(|d| d.public && !d.test) {
            let at = format!("{}: C7: the pub trait {}", def.file, def.name);
            let row = rows.get(def.name.as_str());
            if let Some(owner) = port_module(&def.file) {
                match row {
                    None => errors.insert(format!(
                        "{at} of ports/ has no row; add it to ownership::PORTS"
                    )),
                    Some((_, Some(_), _)) => {
                        errors.insert(format!("{at} is in ports/; make its row a PORTS row"))
                    }
                    Some(&(publicity, None, _)) => {
                        if defined.insert(&def.name, def).is_some() {
                            errors.insert(format!("{at} is defined in two port modules"));
                        } else if let Some(publicity) = publicity {
                            self.known(def, owner, publicity);
                        }
                        true
                    }
                };
            } else if def.file.starts_with("src/application/")
                && !def.file.starts_with("src/application/ports/")
            {
                match row {
                    Some(&(publicity, Some(path), owner)) if path == def.file => {
                        defined.insert(&def.name, def);
                        if let (Some(publicity), Some(owner)) = (publicity, owner) {
                            self.known(def, owner, publicity);
                        }
                    }
                    _ => {
                        errors.insert(format!(
                            "{at} outside ports/ has no row for its file; add it to ownership::OUTER_PORTS or ownership::NOT_PORTS"
                        ));
                    }
                }
            }
        }
        for (name, (_, path, _)) in &rows {
            if !defined.contains_key(name) {
                errors.insert(match path {
                    None => format!(
                        "ownership::PORTS: {name}: C7: no pub trait of src/application/ports/<context>.rs; remove the row"
                    ),
                    Some(path) => format!(
                        "ownership: {path} {name}: C7: no pub trait of the file; remove the row"
                    ),
                });
            }
        }
        // The methods of the partial ports.
        let mut methods = BTreeSet::new();
        let mut with_methods = BTreeSet::new();
        for row in records.methods {
            let at = format!("ownership::METHODS: {}::{}", row.port, row.method);
            if !valid(row.to) {
                errors.insert(format!("{at}: want To::All or contexts"));
            }
            if !methods.insert((row.port, row.method)) {
                errors.insert(format!("{at}: listed twice"));
            }
            if !matches!(rows.get(row.port), Some((Some(Publicity::Partial), _, _))) {
                errors.insert(format!("{at}: {} is no partial port", row.port));
                continue;
            }
            with_methods.insert(row.port);
            if let Some(def) = defined.get(row.port)
                && !def.methods.contains(row.method)
            {
                errors.insert(format!(
                    "{at}: C7: the trait {} declares no method {}",
                    row.port, row.method
                ));
            }
        }
        for (name, (publicity, _, _)) in &rows {
            if matches!(publicity, Some(Publicity::Partial)) && !with_methods.contains(name) {
                errors.insert(format!(
                    "ownership: the partial port {name} has no row of ownership::METHODS"
                ));
            }
        }
    }

    /// Keeps the port `def` owned by `owner` by its path.
    fn known(&mut self, def: &super::TraitDef, owner: Context, publicity: Publicity) {
        let mut open: Open = Some(BTreeSet::new());
        match publicity {
            Publicity::Public(to) => add(&mut open, to),
            Publicity::Internal => {}
            Publicity::Partial => {
                for row in self.records.methods.iter().filter(|m| m.port == def.name) {
                    add(&mut open, row.to);
                }
            }
        }
        let mut path = def.module.clone();
        path.push(def.name.clone());
        self.ports.insert(
            path,
            Known {
                name: def.name.clone(),
                owner,
                open,
            },
        );
    }

    /// The context of `file`.
    fn context(&self, file: &str) -> Option<Context> {
        if let Some(context) = derived(file) {
            return Some(context);
        }
        let modules = self.records.modules.iter().map(|m| (m.path, m.context));
        let stores = self.records.stores.iter().map(|s| (s.path, s.context));
        modules
            .chain(stores)
            .filter(|(path, _)| super::super::covers(path, file))
            .max_by_key(|(path, _)| path.len())
            .map(|(_, context)| context)
    }

    /// The ports `paths` (one reference's, resolved) name that C5 forbids
    /// in `file`.
    pub(super) fn c5(&self, file: &str, paths: &BTreeSet<ModulePath>) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        let Some(context) = self.context(file).filter(|c| *c != Context::Shared) else {
            return found;
        };
        for path in paths {
            for end in 1..=path.len() {
                let Some(port) = self.ports.get(&path[..end]) else {
                    continue;
                };
                let open = port.open.as_ref().is_none_or(|to| to.contains(&context));
                if port.owner != context && port.owner != Context::Shared && !open {
                    found.insert(port.name.clone());
                }
            }
        }
        found
    }

    /// The path as written that C8 forbids in `reference`, if any.
    pub(super) fn c8(&self, reference: &Reference) -> Option<String> {
        let [application, ports, module] = &reference.module[..] else {
            return None;
        };
        let context = named(module)?;
        if application != "application" || ports != "ports" {
            return None;
        }
        let path: Vec<&str> = reference.path.iter().map(String::as_str).collect();
        match &path[..] {
            ["crate", "application", "ports", ..] => Some("crate::application::ports".to_owned()),
            ["super", "super", "ports", ..] => Some("super::super::ports".to_owned()),
            ["super", "super", ..] => None,
            ["super"] if reference.glob => Some("super::*".to_owned()),
            ["super", name, ..] => {
                let may = named(name).is_some_and(|other| {
                    other == Context::Shared
                        || self
                            .records
                            .port_modules
                            .iter()
                            .any(|row| row.context == context && row.names.contains(&other))
                });
                (!may).then(|| format!("super::{name}"))
            }
            _ => None,
        }
    }
}
