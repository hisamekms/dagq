//! The port references of the module reference check (C5, C8 and the
//! records C7 holds) on a small library: one that matches its records with
//! no violation, and one change put into it per case.

use super::modules::{ModuleRecords, check, ports::PortRecords};
use super::ownership::Context::{Execution, Host, Observation, Planning, Shared};
use super::ownership::Publicity::{Internal, Partial, Public};
use super::ownership::To::{All, Only};
use super::ownership::{
    Fix, Lent, Method, ModuleAllowed, ModuleContext, NotPort, OuterPort, Port, PortModules, Rule,
    Store,
};
use std::collections::BTreeMap;

/// The library: a port of each publicity in the port modules, an outer
/// port and a pub trait that is no port, a context's module of each kind
/// (a `ModuleContext` row, a store row, a path that tells it), comments,
/// strings and test code that name internal ports, and the port modules
/// naming what they may.
fn clean() -> BTreeMap<String, String> {
    let files = [
        (
            "src/lib.rs",
            "pub mod application;\npub mod infrastructure;\n",
        ),
        (
            "src/application.rs",
            "pub mod inherit;\npub mod planner;\nmod ports;\npub mod report;\n\
             pub use ports::*;\n",
        ),
        (
            "src/application/ports/mod.rs",
            "mod execution;\nmod observation;\nmod planning;\nmod shared;\n\
             pub use execution::*;\npub use observation::*;\npub use planning::*;\n\
             pub use shared::*;\n",
        ),
        (
            "src/application/ports/planning.rs",
            "//! Named in a comment only: super::execution::RunRecovery.\n\
             use super::shared::Clock;\nuse super::super::inherit;\n\
             pub trait TaskStore {\n    fn list(&self) -> u32;\n    fn claim(&mut self);\n\
             \x20   #[cfg(test)]\n    fn fake(&self);\n}\n\
             pub trait PlanStore { fn save(&mut self, clock: &dyn Clock); }\n\
             #[cfg(test)]\nmod tests { use super::*; use super::super::execution::RunReads; }\n",
        ),
        (
            "src/application/ports/execution.rs",
            "pub trait RunReads { fn run(&self); }\npub trait RunRecovery { fn recover(&mut self); }\n",
        ),
        (
            "src/application/ports/observation.rs",
            "use super::{execution::RunReads, planning::PlanStore, shared::Clock};\n\
             pub trait ObserverLog { fn log(&self); }\n",
        ),
        (
            "src/application/ports/shared.rs",
            "pub trait Clock {}\npub trait Queue { fn queue(&self, _: &dyn super::execution::RunRecovery); }\n",
        ),
        (
            "src/application/inherit.rs",
            "pub trait InheritStore { fn inherit(&mut self); }\npub trait Helper { fn help(&self); }\n\
             pub fn carry(_: &dyn super::ports::RunRecovery, _: &dyn super::ports::TaskStore) {}\n",
        ),
        (
            "src/application/planner.rs",
            r#"//! RunRecovery named in a comment only.
use super::ports::{RunReads, TaskStore};
use super::inherit::InheritStore;
pub fn plan(_: &dyn RunReads, _: &dyn TaskStore) { let _ = "crate::application::RunRecovery"; }
fn local() { struct RunRecovery; let _ = RunRecovery; }
#[cfg(test)]
mod tests { use super::super::ports::RunRecovery; }
#[cfg(all(test, unix))]
fn fake(_: &dyn crate::application::PlanStore) {}
"#,
        ),
        ("src/application/report.rs", "pub fn report() {}\n"),
        ("src/infrastructure.rs", "pub mod runs;\n"),
        (
            "src/infrastructure/runs.rs",
            "use crate::application::PlanStore;\npub fn lease(_: &dyn PlanStore) {}\n\
             #[cfg(test)]\npub fn leased() {}\n",
        ),
    ];
    files
        .into_iter()
        .map(|(path, text)| (path.to_owned(), text.to_owned()))
        .collect()
}

const MODULES: &[ModuleContext] = &[
    ModuleContext {
        path: "src/application/planner.rs",
        context: Planning,
    },
    ModuleContext {
        path: "src/application/inherit.rs",
        context: Execution,
    },
    ModuleContext {
        path: "src/application/report.rs",
        context: Observation,
    },
];

const STORES: &[Store] = &[Store {
    path: "src/infrastructure/runs.rs",
    context: Execution,
}];

const PORTS: &[Port] = &[
    Port {
        name: "TaskStore",
        publicity: Partial,
    },
    Port {
        name: "PlanStore",
        publicity: Public(Only(&[Observation])),
    },
    Port {
        name: "RunReads",
        publicity: Public(All),
    },
    Port {
        name: "RunRecovery",
        publicity: Internal,
    },
    Port {
        name: "ObserverLog",
        publicity: Internal,
    },
    Port {
        name: "Clock",
        publicity: Public(All),
    },
    Port {
        name: "Queue",
        publicity: Internal,
    },
];

const OUTER_PORTS: &[OuterPort] = &[OuterPort {
    path: "src/application/inherit.rs",
    name: "InheritStore",
    owner: Some(Execution),
    publicity: Public(Only(&[Planning])),
}];

const NOT_PORTS: &[NotPort] = &[NotPort {
    path: "src/application/inherit.rs",
    name: "Helper",
    reason: "a fixture's helper",
}];

const METHODS: &[Method] = &[
    Method {
        port: "TaskStore",
        method: "list",
        to: Only(&[Planning]),
    },
    Method {
        port: "TaskStore",
        method: "claim",
        to: Only(&[Execution]),
    },
];

const LENT: &[Lent] = &[Lent {
    path: "src/infrastructure/runs.rs",
    name: "lease",
    owner: Execution,
    to: Only(&[Planning]),
}];

const PORT_MODULES: &[PortModules] = &[
    PortModules {
        context: Observation,
        names: &[Planning, Execution, Host],
        reason: "fixture",
    },
    PortModules {
        context: Shared,
        names: &[Planning, Execution, Observation, Host],
        reason: "fixture",
    },
];

fn records() -> PortRecords<'static> {
    PortRecords {
        modules: MODULES,
        stores: STORES,
        ports: PORTS,
        outer_ports: OUTER_PORTS,
        not_ports: NOT_PORTS,
        methods: METHODS,
        lent: LENT,
        port_modules: PORT_MODULES,
    }
}

/// The C5 violation allowed in the clean library.
const RUNS: ModuleAllowed = ModuleAllowed {
    rule: Rule::C5,
    path: "src/infrastructure/runs.rs",
    reference: "PlanStore",
    fix: Fix::Unregistered,
    reason: "fixture",
};

fn errors_of(
    files: &BTreeMap<String, String>,
    ports: PortRecords,
    allowed: &[ModuleAllowed],
) -> Vec<String> {
    check(
        files,
        &ModuleRecords {
            shared: &[],
            allowed,
            ports,
        },
    )
}

/// The failures of the clean library with `line` added to `path`.
fn with(path: &str, line: &str) -> Vec<String> {
    let mut files = clean();
    files.entry(path.to_owned()).or_default().push_str(line);
    errors_of(&files, records(), &[RUNS])
}

fn forbids(path: &str, rule: &str, reference: &str) -> String {
    format!(
        "{path}: {rule} forbids {reference}; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason"
    )
}

#[track_caller]
fn fails_with(path: &str, line: &str, want: &str) {
    assert_eq!(with(path, line), [want.to_owned()], "{path} with {line}");
}

#[test]
fn a_library_that_matches_its_records_passes() {
    assert_eq!(
        errors_of(&clean(), records(), &[RUNS]),
        Vec::<String>::new()
    );
}

/// C5 knows a port by its definition's path: through a re-export, a group,
/// an alias, a `use` inside a fn and a path to its method alike.
#[test]
fn another_context_names_no_internal_port_by_any_path() {
    let planner = "src/application/planner.rs";
    let want = forbids(planner, "C5", "RunRecovery");
    for line in [
        "fn f(_: &dyn crate::application::RunRecovery) {}\n",
        "use crate::application::{ports::{RunRecovery}};\n",
        "use super::ports::RunRecovery as Recovery;\nfn f(_: &dyn Recovery) {}\n",
        "fn f() { use crate::application::*; let _ = RunRecovery::recover; }\n",
        "#[cfg(not(test))]\nfn f(_: &dyn super::RunRecovery) {}\n",
        "fn f<T: crate::application::RunRecovery>(_: T) {}\n",
    ] {
        fails_with(planner, line, &want);
    }
    // A store's context is its store row's.
    fails_with(
        "src/infrastructure/runs.rs",
        "pub fn save(_: &dyn crate::application::ObserverLog) {}\n",
        &forbids("src/infrastructure/runs.rs", "C5", "ObserverLog"),
    );
    // An outer port is public only to its contexts.
    fails_with(
        "src/application/report.rs",
        "use crate::application::inherit::InheritStore;\n",
        &forbids("src/application/report.rs", "C5", "InheritStore"),
    );
}

/// A port named alone in a macro's tokens, which syn leaves unparsed, is
/// resolved where the macro is written, outside test code; such a name is
/// no reference for the layer rules.
#[test]
fn a_port_named_alone_in_a_macro_counts_outside_test_code() {
    let planner = "src/application/planner.rs";
    let want = forbids(planner, "C5", "RunRecovery");
    for line in [
        "use crate::application::*;\nfn probe() { let _ = vec![None::<&dyn RunRecovery>]; }\n",
        "fn probe() { use super::ports::*; assert!(matches!(None::<Box<dyn RunRecovery>>, None)); }\n",
    ] {
        fails_with(planner, line, &want);
    }
    for line in [
        "#[cfg(test)]\nfn probe() { use crate::application::*; let _ = vec![None::<&dyn RunRecovery>]; }\n",
        "fn probe() { use crate::application::*; struct RunRecovery; let _ = vec![RunRecovery]; }\n",
        "fn probe() { use crate::*; let infrastructure = 1; let _ = vec![infrastructure]; }\n",
    ] {
        assert_eq!(
            with(planner, line),
            Vec::<String>::new(),
            "{planner} with {line}"
        );
    }
}

/// A local item of a port's name, test code, the owner, a public port and a
/// file of no context do not count.
#[test]
fn what_is_no_reference_to_another_contexts_internal_port_passes() {
    for (path, line) in [
        (
            "src/application/planner.rs",
            "fn f() { trait RunRecovery {} fn g(_: &dyn RunRecovery) {} }\n",
        ),
        (
            "src/application/planner.rs",
            "#[cfg(test)]\nmod more { fn f(_: &dyn crate::application::RunRecovery) {} }\n",
        ),
        (
            "src/application/inherit.rs",
            "fn f(_: &dyn crate::application::RunRecovery) {}\n",
        ),
        (
            "src/application/report.rs",
            "fn f(_: &dyn crate::application::PlanStore, _: &dyn crate::application::RunReads) {}\n",
        ),
        ("src/application.rs", "fn f(_: &dyn RunRecovery) {}\n"),
    ] {
        assert_eq!(with(path, line), Vec::<String>::new(), "{path} with {line}");
    }
}

/// A partial port is public, by its name, to the contexts of its methods.
#[test]
fn a_partial_port_is_public_to_the_contexts_of_its_methods_only() {
    fails_with(
        "src/application/report.rs",
        "fn f(_: &dyn crate::application::TaskStore) {}\n",
        &forbids("src/application/report.rs", "C5", "TaskStore"),
    );
}

#[test]
fn an_allowed_row_lets_its_violation_and_fails_when_stale() {
    assert_eq!(
        errors_of(&clean(), records(), &[]),
        [forbids("src/infrastructure/runs.rs", "C5", "PlanStore")]
    );
    let mut files = clean();
    files.insert(
        "src/infrastructure/runs.rs".to_owned(),
        "pub fn lease() {}\n".to_owned(),
    );
    assert_eq!(
        errors_of(&files, records(), &[RUNS]),
        [
            "ownership::MODULES_ALLOWED: stale row src/infrastructure/runs.rs C5 PlanStore (no such reference any more); remove it"
        ]
    );
}

/// C8: a port module names its own and `shared`'s module and those of
/// [`PortModules`] by their paths, never the re-exports, test code too.
#[test]
fn a_port_module_names_only_the_port_modules_it_may() {
    let planning = "src/application/ports/planning.rs";
    for (line, written) in [
        ("use super::execution::RunReads;\n", "super::execution"),
        (
            "#[cfg(test)]\nuse super::{execution::RunReads};\n",
            "super::execution",
        ),
        ("use super::RunReads;\n", "super::RunReads"),
        ("use super::*;\n", "super::*"),
        (
            "fn f(_: &dyn crate::application::ports::shared::Clock) {}\n",
            "crate::application::ports",
        ),
        ("use super::super::ports::Clock;\n", "super::super::ports"),
    ] {
        fails_with(planning, line, &forbids(planning, "C8", written));
    }
    fails_with(
        "src/application/ports/observation.rs",
        "use super::inherit::Helper;\n",
        &forbids(
            "src/application/ports/observation.rs",
            "C8",
            "super::inherit",
        ),
    );
}

/// C7: every pub trait of `src/application/` has its row, and every row
/// names what the code has.
#[test]
fn the_port_records_match_the_traits_and_files_of_the_code() {
    let at = |path: &str, line: &str| with(path, line);
    assert_eq!(
        at("src/application/report.rs", "pub trait Extra {}\n"),
        [
            "src/application/report.rs: C7: the pub trait Extra outside ports/ has no row for its file; add it to ownership::OUTER_PORTS or ownership::NOT_PORTS"
        ]
    );
    assert_eq!(
        at(
            "src/application/ports/execution.rs",
            "pub(crate) trait Unlisted {}\n"
        ),
        [
            "src/application/ports/execution.rs: C7: the pub trait Unlisted of ports/ has no row; add it to ownership::PORTS"
        ]
    );
    assert_eq!(
        at(
            "src/application/report.rs",
            "#[cfg(test)]\npub trait Fake {}\ntrait Private {}\n"
        ),
        Vec::<String>::new()
    );
    let ports: Vec<Port> = PORTS
        .iter()
        .map(|p| Port {
            name: p.name,
            publicity: p.publicity,
        })
        .chain([Port {
            name: "Gone",
            publicity: Internal,
        }])
        .collect();
    let methods: Vec<Method> = METHODS
        .iter()
        .map(|m| Method {
            port: m.port,
            method: m.method,
            to: m.to,
        })
        .chain([
            Method {
                port: "TaskStore",
                method: "fake",
                to: All,
            },
            Method {
                port: "RunReads",
                method: "run",
                to: All,
            },
        ])
        .collect();
    let modules = [
        ModuleContext {
            path: "src/application/ports/planning.rs",
            context: Planning,
        },
        ModuleContext {
            path: "src/application/gone/",
            context: Host,
        },
    ];
    let lent = [Lent {
        path: "src/infrastructure/runs.rs",
        name: "leased",
        owner: Execution,
        to: Only(&[Execution]),
    }];
    let records = PortRecords {
        modules: &modules,
        ports: &ports,
        methods: &methods,
        lent: &lent,
        ..records()
    };
    let mut got = errors_of(&clean(), records, &[RUNS]);
    got.sort();
    assert_eq!(
        got,
        [
            "ownership::LENT: src/infrastructure/runs.rs leased: C7: src/infrastructure/runs.rs does not define leased outside test code",
            "ownership::LENT: src/infrastructure/runs.rs leased: want To::All or contexts other than its owner",
            "ownership::METHODS: RunReads::run: RunReads is no partial port",
            "ownership::METHODS: TaskStore::fake: C7: the trait TaskStore declares no method fake",
            "ownership::MODULES: src/application/gone/: C7: no Rust file or directory of one; remove the row",
            "ownership::MODULES: src/application/ports/planning.rs: the path tells its context planning; remove the row",
            "ownership::PORTS: Gone: C7: no pub trait of src/application/ports/<context>.rs; remove the row",
        ]
    );
    // An outer port whose trait moved to another file.
    let mut files = clean();
    files.insert(
        "src/application/inherit.rs".to_owned(),
        "pub trait Helper { fn help(&self); }\n".to_owned(),
    );
    files
        .get_mut("src/application/report.rs")
        .unwrap()
        .push_str("pub trait InheritStore {}\n");
    assert_eq!(
        errors_of(&files, self::records(), &[RUNS]),
        [
            "ownership: src/application/inherit.rs InheritStore: C7: no pub trait of the file; remove the row",
            "src/application/report.rs: C7: the pub trait InheritStore outside ports/ has no row for its file; add it to ownership::OUTER_PORTS or ownership::NOT_PORTS",
        ]
    );
    let bare = [Port {
        name: "TaskStore",
        publicity: Partial,
    }];
    let records = PortRecords {
        ports: &bare,
        methods: &[],
        ..self::records()
    };
    let got = errors_of(&clean(), records, &[]);
    assert!(
        got.contains(
            &"ownership: the partial port TaskStore has no row of ownership::METHODS".to_owned()
        ),
        "{got:?}"
    );
}
