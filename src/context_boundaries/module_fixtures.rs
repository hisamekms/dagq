//! The module reference check on small libraries: one with no violation,
//! and one violation of each rule put into it.

use super::modules::{ModuleRecords, check};
use super::ownership::{Fix, Layer, ModuleAllowed, Rule, SharedModule};
use std::collections::BTreeMap;

const SHARED: &[SharedModule] = &[
    SharedModule {
        module: "build_id",
        layers: &[Layer::Domain, Layer::Application, Layer::Infrastructure],
        reason: "fixture",
    },
    SharedModule {
        module: "migration_numbers",
        layers: &[Layer::Application, Layer::Infrastructure],
        reason: "fixture",
    },
];

/// A library with no violation: comments, doc comments and strings name
/// what the rules forbid, the layers reference inward and the shared parts,
/// the root and the loop of the composition name its contexts, and the
/// contexts name the root, themselves and their children.
fn clean() -> BTreeMap<String, String> {
    let files = [
        (
            "src/lib.rs",
            "pub mod application;\npub mod build_id;\npub mod compose;\npub mod domain;\n\
             pub mod infrastructure;\npub mod migration_numbers;\npub mod view;\n\
             pub use view::Shown;\n",
        ),
        ("src/build_id.rs", "pub fn named_commit() {}\n"),
        ("src/migration_numbers.rs", "pub const LAST: u32 = 1;\n"),
        (
            "src/view.rs",
            "pub struct Shown;\nuse crate::domain::run;\n",
        ),
        ("src/domain.rs", "pub mod run;\n"),
        (
            "src/domain/run.rs",
            r#"//! crate::application is named here only in a comment.
use crate::build_id::named_commit; // crate::infrastructure in a comment
/// See crate::compose::up.
pub fn f() -> &'static str { "crate::application::x in a string" }
/* crate::view in a block comment */
pub struct Run;
#[cfg(test)]
mod tests;
"#,
        ),
        ("src/domain/run/tests.rs", "use super::Run;\n"),
        (
            "src/application.rs",
            "pub mod use_case;\npub use use_case::Clock;\n",
        ),
        (
            "src/application/use_case.rs",
            "use crate::{domain::run::{self, Run}, migration_numbers::LAST};\n\
             pub trait Clock {}\nfn g() { crate::build_id::named_commit(); }\n",
        ),
        ("src/infrastructure.rs", "pub mod store;\n"),
        (
            "src/infrastructure/store.rs",
            "use crate::application::Clock;\nuse crate::domain::run::Run;\n",
        ),
        (
            "src/compose.rs",
            "mod host;\nmod observation;\nmod supervisor;\npub use host::up;\n\
             pub struct OneShot;\nfn shared() { host::up(); }\n",
        ),
        (
            "src/compose/host.rs",
            "//! See super::observation::status, named in a comment only.\n\
             use super::{OneShot, shared};\nuse crate::view::Shown;\n\
             pub fn up() { let _ = \"crate::compose::observation\"; shared(); }\n",
        ),
        (
            "src/compose/observation.rs",
            "mod child;\nuse super::OneShot;\npub fn status() { super::shared(); }\n\
             #[cfg(test)]\nmod tests { use super::*; }\n",
        ),
        (
            "src/compose/observation/child.rs",
            "use super::*;\nuse super::super::OneShot;\nfn f() { super::status(); super::child::f(); }\n",
        ),
        (
            "src/compose/supervisor.rs",
            "use super::{host, observation::status};\nuse super::*;\n\
             pub fn supervise() { host::up(); status(); }\n",
        ),
    ];
    files
        .iter()
        .map(|(path, text)| (path.to_string(), text.to_string()))
        .collect()
}

fn errors(files: &BTreeMap<String, String>, allowed: &[ModuleAllowed]) -> Vec<String> {
    check(
        files,
        &ModuleRecords {
            shared: SHARED,
            allowed,
        },
    )
}

/// The failures of the clean library with `line` added to `path`.
fn with(path: &str, line: &str) -> Vec<String> {
    let mut files = clean();
    files.entry(path.to_owned()).or_default().push_str(line);
    errors(&files, &[])
}

#[track_caller]
fn fails_with(path: &str, line: &str, want: &str) {
    let got = with(path, line);
    assert_eq!(got, [want.to_owned()], "{path} with {line}");
}

#[test]
fn a_library_with_no_violation_passes() {
    assert_eq!(errors(&clean(), &[]), Vec::<String>::new());
}

#[test]
fn the_domain_references_no_outer_layer_and_only_build_id_inside_tests_too() {
    let want = "src/domain/run.rs: L1 forbids crate::application; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason";
    fails_with(
        "src/domain/run.rs",
        "use crate::application::Clock;\n",
        want,
    );
    fails_with(
        "src/domain/run.rs",
        "#[cfg(test)]\nmod more { fn t() { let _ = super::super::super::application::Clock; } }\n",
        want,
    );
    fails_with(
        "src/domain/run.rs",
        "#[crate::application::custom_attribute]\nfn g() {}\n",
        want,
    );
    fails_with(
        "src/domain/run/tests.rs",
        "use crate::infrastructure::store;\n",
        "src/domain/run/tests.rs: L1 forbids crate::infrastructure; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason",
    );
    fails_with(
        "src/domain/run.rs",
        "fn g() -> u32 { crate::migration_numbers::LAST }\n",
        "src/domain/run.rs: L1 forbids crate::migration_numbers; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason",
    );
}

#[test]
fn the_application_references_no_infrastructure_or_compose_through_groups_and_macros() {
    fails_with(
        "src/application/use_case.rs",
        "use crate::{domain::run, infrastructure::store};\n",
        "src/application/use_case.rs: L3 forbids crate::infrastructure; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason",
    );
    fails_with(
        "src/application/use_case.rs",
        "fn h(x: u8) -> bool { matches!(x, crate::compose::LIMIT) }\n",
        "src/application/use_case.rs: L3 forbids crate::compose; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason",
    );
}

#[test]
fn the_infrastructure_references_no_compose() {
    fails_with(
        "src/infrastructure/store.rs",
        "pub fn q() { crate::compose::up(); }\n",
        "src/infrastructure/store.rs: L6 forbids crate::compose; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason",
    );
}

#[test]
fn a_layer_references_no_outer_module_even_through_a_re_export() {
    let want = "src/application/use_case.rs: L8 forbids crate::view; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason";
    fails_with(
        "src/application/use_case.rs",
        "use crate::view::Shown;\n",
        want,
    );
    fails_with(
        "src/application/use_case.rs",
        "fn s() -> crate::Shown { crate::Shown }\n",
        want,
    );
    let mut files = clean();
    files.insert(
        "src/lib.rs".to_owned(),
        files["src/lib.rs"].clone() + "pub mod runtime;\n",
    );
    files.insert(
        "src/runtime.rs".to_owned(),
        "pub use crate::domain::run;\n".to_owned(),
    );
    files
        .get_mut("src/infrastructure/store.rs")
        .unwrap()
        .push_str("use crate::runtime::run;\n");
    assert_eq!(
        errors(&files, &[]),
        [
            "src/infrastructure/store.rs: L8 forbids crate::runtime; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason"
        ]
    );
}

#[test]
fn a_composition_context_names_no_other_module_of_compose_nor_globs_the_root() {
    let host = "src/compose/observation.rs: L9 forbids crate::compose::host; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason";
    fails_with(
        "src/compose/observation.rs",
        "use super::{host::up, OneShot};\n",
        host,
    );
    fails_with(
        "src/compose/observation.rs",
        "fn u() { super::up(); }\n",
        host,
    );
    fails_with(
        "src/compose/observation.rs",
        "fn s() { crate::compose::supervisor::supervise(); }\n",
        "src/compose/observation.rs: L9 forbids crate::compose::supervisor; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason",
    );
    let glob = "src/compose/observation.rs: L9 forbids crate::compose::*; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason";
    fails_with("src/compose/observation.rs", "use super::*;\n", glob);
    fails_with(
        "src/compose/observation.rs",
        "use super::{OneShot as O, *};\n",
        glob,
    );
    fails_with(
        "src/compose/observation.rs",
        "#[cfg(test)]\nmod more { use crate::compose::*; }\n",
        glob,
    );
}

/// In `src/compose/<m>/`, `super` is `<m>` and `super::super` the root.
#[test]
fn in_a_nested_file_of_a_context_super_is_the_context_and_super_super_the_root() {
    let child = "src/compose/observation/child.rs";
    let host = format!(
        "{child}: L9 forbids crate::compose::host; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason"
    );
    fails_with(child, "fn u() { super::super::host::up(); }\n", &host);
    fails_with(child, "use crate::compose::host;\n", &host);
    fails_with(
        child,
        "use super::super::*;\n",
        &format!(
            "{child}: L9 forbids crate::compose::*; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason"
        ),
    );
}

#[test]
fn a_new_module_of_compose_is_a_context_without_a_list() {
    let mut files = clean();
    files
        .get_mut("src/compose.rs")
        .unwrap()
        .push_str("mod foo;\n");
    files.insert(
        "src/compose/foo.rs".to_owned(),
        "pub fn run() { super::shared(); }\n".to_owned(),
    );
    assert_eq!(errors(&files, &[]), Vec::<String>::new());
    files
        .get_mut("src/compose/foo.rs")
        .unwrap()
        .push_str("fn up() { super::observation::status(); }\n");
    assert_eq!(
        errors(&files, &[]),
        [
            "src/compose/foo.rs: L9 forbids crate::compose::observation; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason"
        ]
    );
}

/// L2: the production code of the domain references no `anyhow`,
/// `std::process` or `std::net`, however it names them.
#[test]
fn the_domains_production_code_references_no_anyhow_process_or_network() {
    let l2 = |item: &str| {
        format!(
            "src/domain/run.rs: L2 forbids {item}; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason"
        )
    };
    let cases = [
        ("fn r() -> anyhow::Result<()> { Ok(()) }\n", "anyhow"),
        ("fn p() -> u32 { std::process::id() }\n", "std::process"),
        ("fn n(_: std::net::IpAddr) {}\n", "std::net"),
        ("use std::process;\n", "std::process"),
        ("use std::{fmt, net::IpAddr};\n", "std::net"),
        ("use anyhow::Context as _;\n", "anyhow"),
        (
            "fn b() -> Result<(), u8> { anyhow::bail!(\"no\") }\n",
            "anyhow",
        ),
        (
            "fn m() { let _ = format!(\"{}\", std::process::id()); }\n",
            "std::process",
        ),
        (
            "fn m() { let _ = format!(\"{}\", ::std::net::Ipv4Addr::LOCALHOST); }\n",
            "std::net",
        ),
        ("extern crate anyhow as failure;\n", "anyhow"),
        (
            "fn m() { let _ = format!(\"{}\", 1 as ::std::process::ExitCode); }\n",
            "std::process",
        ),
        (
            "mod inner { fn f() { let _ = std::net::TcpStream::connect(\"x\"); } }\n",
            "std::net",
        ),
        (
            "#[cfg(any(test, unix))]\nfn u() -> u32 { std::process::id() }\n",
            "std::process",
        ),
        (
            "#[cfg(not(test))]\nfn u() -> u32 { std::process::id() }\n",
            "std::process",
        ),
        (
            "pub enum E { A, #[cfg(test)] Fake }\nfn k() -> u32 { std::process::id() }\n",
            "std::process",
        ),
        (
            "#[cfg(all(unix, any(test, unix)))]\nfn u() -> u32 { std::process::id() }\n",
            "std::process",
        ),
        (
            "#[cfg(all())]\nfn u() -> u32 { std::process::id() }\n",
            "std::process",
        ),
        (
            "#[cfg(feature = \"test\")]\nfn u() -> u32 { std::process::id() }\n",
            "std::process",
        ),
        (
            "fn f() { #[cfg(unix)] { let _ = std::process::id(); } }\n",
            "std::process",
        ),
    ];
    for (line, item) in cases {
        fails_with("src/domain/run.rs", line, &l2(item));
    }
    // A renamed `use` is a violation where it is written and where its
    // name is used.
    let mut files = clean();
    files.insert(
        "src/domain/other.rs".to_owned(),
        "use std::process as proc;\nfn p() -> u32 { proc::id() }\n".to_owned(),
    );
    files
        .get_mut("src/domain.rs")
        .unwrap()
        .push_str("mod other;\n");
    assert_eq!(
        errors(&files, &[]),
        [
            "src/domain/other.rs: L2 forbids std::process; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason"
        ]
    );
    // A rename of `extern crate` at the root names the crate everywhere.
    let mut files = clean();
    files
        .get_mut("src/lib.rs")
        .unwrap()
        .push_str("extern crate anyhow as failure;\n");
    files
        .get_mut("src/domain/run.rs")
        .unwrap()
        .push_str("fn b() -> Result<(), u8> { failure::bail!(\"no\") }\n");
    assert_eq!(errors(&files, &[]), [l2("anyhow")]);
}

/// L2 does not count test code, the application, the composition root, a
/// module of the domain of the same name or another part of `std`.
#[test]
fn the_domains_test_code_and_the_outer_layers_may_reference_anyhow_process_and_network() {
    let mut files = clean();
    files.get_mut("src/domain/run.rs").unwrap().push_str(
        "#[cfg(test)]\nfn t() -> u32 { std::process::id() }\n\
         #[cfg(all(test, unix))]\nuse std::net::IpAddr;\n\
         #[cfg(test)]\nmod more { use anyhow::Result; fn r() -> Result<()> { anyhow::bail!(\"no\") } }\n\
         pub struct S { #[cfg(test)] pid: std::process::ExitCode }\n\
         impl S { #[cfg(test)] fn pid() -> u32 { std::process::id() } }\n\
         mod process { pub fn id() -> u32 { 1 } }\nfn own() -> u32 { process::id() }\n\
         fn fmt(_: std::fmt::Error) {}\n\
         #[cfg(any(test))]\nfn single_any() -> u32 { std::process::id() }\n\
         #[cfg(all(unix, any(test)))]\nfn nested() -> u32 { std::process::id() }\n\
         #[cfg(any(all(test, unix), all(test, feature = \"x\"),))]\nfn every_branch() -> u32 { std::process::id() }\n\
         #[cfg(any())]\nfn never() -> u32 { std::process::id() }\n\
         #[cfg(any(test))]\nuse std::process as proc;\n\
         fn blocks() {\n\
             #[cfg(test)] { let _ = std::process::id(); }\n\
             #[cfg(test)] println!(\"{}\", std::process::id());\n\
             let _ = (1, #[cfg(test)] std::net::Ipv4Addr::LOCALHOST);\n\
         }\n\
         fn built() -> S { S { #[cfg(test)] pid: std::process::ExitCode::SUCCESS } }\n",
    );
    files.get_mut("src/domain/run/tests.rs").unwrap().push_str(
        "use std::net::IpAddr;\nfn r() -> anyhow::Result<u32> { Ok(std::process::id()) }\n",
    );
    files
        .get_mut("src/domain.rs")
        .unwrap()
        .push_str("mod probe;\n");
    files.insert(
        "src/domain/probe.rs".to_owned(),
        "#![cfg(test)]\nfn p() -> u32 { std::process::id() }\n".to_owned(),
    );
    files
        .get_mut("src/application/use_case.rs")
        .unwrap()
        .push_str(
            "use anyhow::Context;\nfn a() -> anyhow::Result<u32> { Ok(std::process::id()) }\n",
        );
    files
        .get_mut("src/compose.rs")
        .unwrap()
        .push_str("fn pid() -> u32 { std::process::id() }\nfn ip(_: std::net::IpAddr) {}\n");
    assert_eq!(errors(&files, &[]), Vec::<String>::new());
    let allowed = [ModuleAllowed {
        rule: Rule::L2,
        path: "src/domain/run.rs",
        reference: "std::process",
        fix: Fix::Tasks(&[1]),
        reason: "fixture",
    }];
    files
        .get_mut("src/domain/run.rs")
        .unwrap()
        .push_str("fn p() -> u32 { std::process::id() }\n");
    assert_eq!(errors(&files, &allowed), Vec::<String>::new());
}

#[test]
fn an_allowed_row_lets_its_violation_and_fails_when_stale_or_without_a_task() {
    let row = |fix| ModuleAllowed {
        rule: Rule::L8,
        path: "src/application/use_case.rs",
        reference: "crate::view",
        fix,
        reason: "fixture",
    };
    let mut files = clean();
    files
        .get_mut("src/application/use_case.rs")
        .unwrap()
        .push_str("use crate::view::Shown;\n");
    assert_eq!(
        errors(&files, &[row(Fix::Tasks(&[1]))]),
        Vec::<String>::new()
    );
    assert_eq!(
        errors(&files, &[row(Fix::Tasks(&[]))]),
        [
            "ownership::MODULES_ALLOWED: src/application/use_case.rs L8 crate::view: want task IDs, or Fix::Unregistered"
        ]
    );
    assert_eq!(
        errors(&clean(), &[row(Fix::Unregistered)]),
        [
            "ownership::MODULES_ALLOWED: stale row src/application/use_case.rs L8 crate::view (no such reference any more); remove it"
        ]
    );
}

/// The cases a resolver by names alone would miss.
#[test]
fn globs_self_super_macro_groups_and_fn_local_names_are_resolved() {
    let l1 = "src/domain/run.rs: L1 forbids crate::application; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason";
    fails_with(
        "src/domain/run.rs",
        "mod g { use crate::*; fn f() { let _ = application::Clock; } }\n",
        l1,
    );
    fails_with(
        "src/domain/run.rs",
        "use self::super::super::application::Clock;\n",
        l1,
    );
    fails_with(
        "src/domain/run.rs",
        "#[path_like = crate::application::X]\nfn g() {}\n",
        l1,
    );
    fails_with(
        "src/application/use_case.rs",
        "macro_rules! m { () => { use crate::{domain::run, infrastructure::store}; } }\n",
        "src/application/use_case.rs: L3 forbids crate::infrastructure; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason",
    );
    let mut files = clean();
    files
        .get_mut("src/compose.rs")
        .unwrap()
        .push_str("fn local() { fn up() {} up(); }\n");
    files
        .get_mut("src/compose/observation.rs")
        .unwrap()
        .push_str("fn u() { super::up(); }\n");
    assert_eq!(
        errors(&files, &[]),
        [
            "src/compose/observation.rs: L9 forbids crate::compose::host; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason"
        ]
    );
}

#[test]
fn a_module_of_compose_declared_public_fails() {
    let mut files = clean();
    let root = files["src/compose.rs"].replace("mod host;", "pub(crate) mod host;");
    files.insert("src/compose.rs".to_owned(), root);
    assert_eq!(
        errors(&files, &[]),
        [
            "src/compose.rs: L9: the module crate::compose::host is not private; outside the composition root, reach it through the root's re-exports"
        ]
    );
}

/// A `use` inside a fn body or a block names what it imports there only,
/// and a reference through it counts as one through the module it names.
#[test]
fn a_use_or_a_glob_inside_a_fn_body_is_resolved_in_its_block() {
    let observation = "src/compose/observation.rs";
    let host = format!(
        "{observation}: L9 forbids crate::compose::host; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason"
    );
    fails_with(
        observation,
        "fn bypass() { use crate::compose as root; root::host::up(); }\n",
        &host,
    );
    fails_with(
        observation,
        "fn bypass() { { use super::super::compose as root; use root::host as h; h::up(); } }\n",
        &host,
    );
    let glob = format!(
        "{observation}: L9 forbids crate::compose::*; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason"
    );
    assert_eq!(
        with(
            observation,
            "fn g() { use crate::compose::*; host::up(); }\n"
        ),
        [glob, host]
    );
    fails_with(
        "src/domain/run.rs",
        "fn f() { use crate::*; let _ = application::Clock; }\n",
        "src/domain/run.rs: L1 forbids crate::application; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason",
    );
    let mut files = clean();
    files
        .get_mut(observation)
        .unwrap()
        .push_str("fn a() { use crate::compose as root; }\nfn b() { root::host::up(); }\n");
    assert_eq!(errors(&files, &[]), Vec::<String>::new());
}

/// A `use` whose path starts with a name a glob brought in, in a block or
/// in a module, and a glob of a module whose own glob brought the name in,
/// are followed; a cycle of globs ends without a reference.
#[test]
fn a_use_through_a_glob_and_a_chain_of_globs_are_resolved_and_a_cycle_ends() {
    let observation = "src/compose/observation.rs";
    let host = format!(
        "{observation}: L9 forbids crate::compose::host; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason"
    );
    fails_with(
        observation,
        "fn bypass() { use crate::*; use compose as root; root::host::up(); }\n",
        &host,
    );
    fails_with(
        observation,
        "mod inner { use crate::*; use compose as root; fn f() { root::host::up(); } }\n",
        &host,
    );
    fails_with(
        observation,
        "mod a { use crate::*; mod b { use super::*; fn f() { compose::host::up(); } } }\n",
        &host,
    );
    assert_eq!(
        with(
            "src/domain/run.rs",
            "mod x { pub use super::y::*; }\nmod y { pub use super::x::*; use z::Q; fn f() { let _ = Q; } }\n",
        ),
        Vec::<String>::new()
    );
}

/// The check reads no `cfg`: a name that `use`s under different `cfg`s, an
/// item or a glob each define counts with all of them, so the test build's
/// alias is not hidden by the other build's.
#[test]
fn a_name_defined_under_different_cfgs_counts_with_each_definition() {
    let observation = "src/compose/observation.rs";
    let host = format!(
        "{observation}: L9 forbids crate::compose::host; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason"
    );
    fails_with(
        observation,
        "#[cfg(test)]\nuse crate::compose as wiring;\n#[cfg(not(test))]\nuse crate::domain as wiring;\n\
         #[cfg(test)]\nuse wiring::host as _;\n",
        &host,
    );
    fails_with(
        observation,
        "#[cfg(not(test))]\nuse crate::domain as wiring;\n#[cfg(test)]\nuse crate::compose as wiring;\n\
         fn f() { wiring::host::up(); }\n",
        &host,
    );
    fails_with(
        observation,
        "fn f() { #[cfg(test)]\nuse crate::compose as wiring;\n#[cfg(not(test))]\nuse crate::domain as wiring;\n\
         wiring::host::up(); }\n",
        &host,
    );
    fails_with(
        "src/domain/run.rs",
        "#[cfg(not(test))]\nmod wiring {}\n#[cfg(test)]\nuse crate::application as wiring;\n\
         fn f() { let _ = wiring::Clock; }\n",
        "src/domain/run.rs: L1 forbids crate::application; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason",
    );
    fails_with(
        "src/domain/run.rs",
        "mod g { use crate::*; #[cfg(not(test))]\nfn application() {}\nfn f() { let _ = application::Clock; } }\n",
        "src/domain/run.rs: L1 forbids crate::application; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason",
    );
}

/// A name is looked up in the namespaces of where it is written: a fn of
/// that name hides neither a `use` nor a glob's module at the start of a
/// longer path, at the end of a glob's module or in a type, yet a single
/// segment in an expression still names the fn. `extern crate self as`
/// names the root, from every module when it is at the root.
#[test]
fn a_name_is_looked_up_in_its_namespaces_and_extern_crate_self_names_the_root() {
    let observation = "src/compose/observation.rs";
    let host = format!(
        "{observation}: L9 forbids crate::compose::host; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason"
    );
    fails_with(
        observation,
        "#[cfg(test)]\nuse crate::compose as wiring;\nfn f() { fn wiring() {} wiring::host::up(); }\n",
        &host,
    );
    fails_with(
        "src/domain/run.rs",
        "mod g { use crate::*; fn application() {} fn f() { let _ = application::Clock; } }\n",
        "src/domain/run.rs: L1 forbids crate::application; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason",
    );
    fails_with(
        observation,
        "extern crate self as me;\nfn f() { me::compose::host::up(); }\n",
        &host,
    );
    fails_with(
        "src/domain/run.rs",
        "mod g { pub use crate::*; pub fn application() {} }\nuse self::g::application::*;\n",
        "src/domain/run.rs: L1 forbids crate::application; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason",
    );
    fails_with(
        "src/domain/run.rs",
        "mod g { use crate::*; #[allow(non_snake_case)]\nfn Shown() {}\nfn h(_: Shown) {} }\n",
        "src/domain/run.rs: L8 forbids crate::view; fix it, or add a row to ownership::MODULES_ALLOWED with its fix and reason",
    );
    assert_eq!(
        with(
            "src/domain/run.rs",
            "mod g { use crate::*; fn application() {} fn f() { application(); } }\n",
        ),
        Vec::<String>::new()
    );
    for bypass in [
        "fn f() { me::compose::host::up(); }\n",
        "fn f() { ::me::compose::host::up(); }\n",
    ] {
        let mut files = clean();
        files
            .get_mut("src/lib.rs")
            .unwrap()
            .push_str("extern crate self as me;\n");
        files.get_mut(observation).unwrap().push_str(bypass);
        assert_eq!(errors(&files, &[]), std::slice::from_ref(&host), "{bypass}");
    }
}
