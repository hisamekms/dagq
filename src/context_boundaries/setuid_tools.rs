//! `ps` and `top` start only where a refusal stops nothing (ADR-t813-3 keeps
//! the sandbox as it is; docs/development/testing.md, "macOSに固有のtest"):
//! Codex's sandbox refuses the exec of a setuid tool, and the workers outside
//! it, the host's gate and the CI would not see a path that went back to one
//! until the next run inside it failed. Processes are read by
//! `crate::infrastructure::process_read`, which the sandbox lets through.
//!
//! Checked on the sources of `src/` and `tests/` as syntax, test code
//! included (see [`source`](super::source)). A start is one of:
//!
//! - `Command::new` or a function named `tool`
//!   (`crate::infrastructure::host_metrics::tool`) called with the string
//!   literal of a program of [`PROGRAMS`], by name or path, as first argument;
//! - a string literal that holds [`BIN_PS`], or a form of [`SHELL_FLAGS`]
//!   (byte strings and those inside a macro's tokens too).
//!
//! What is not seen: the program passed by value (`Command::new(ps)`, as
//! `sccache.rs` does with the one its configuration gives), under another
//! name of `Command` or `tool`, and a fake `ps` a test puts on `PATH`, which
//! starts no setuid tool. The names are the check's: that a tool is setuid
//! is not read from the host.
//!
//! This file is not read: its text holds the forms as data.

use super::source;
use std::collections::BTreeSet;

/// The setuid tools a call may not name.
const PROGRAMS: [&str; 2] = ["ps", "top"];

/// The path a shell's text may not name.
const BIN_PS: &str = "/bin/ps";

/// What follows `ps` in a shell's text that starts it (`-ax` also stands for
/// `-axo`, `-axww`).
const SHELL_FLAGS: [&str; 4] = ["-o", "-A", "-ax", "-U"];

/// The modules that run only on the host and whose work a refused `ps` does
/// not stop, with why.
const HOST_ONLY: [(&str, &str); 3] = [
    (
        "src/infrastructure/host_metrics.rs",
        "reads %CPU as ps gives it: another reading changes what the column means, and when it \
         does not read the sample's CPU and memory per kind are only empty",
    ),
    (
        "src/infrastructure/e2e_gate.rs",
        "the cleanup after an e2e on the host; a failed ps gives no process to clean up",
    ),
    (
        "src/infrastructure/sccache.rs",
        "starts the ps its configuration gives, and its test a fake script",
    ),
];

/// The one helper that compares a stored identifier with what the real `ps`
/// prints, as (file, fn, why): the identifiers must keep reading as `ps`
/// printed them, and no fixture can show that.
const COMPARISON: (&str, &str, &str) = (
    "tests/it/queue_service_reads.rs",
    "ps_lstart_if_runnable",
    "returns None when a sandbox refuses the spawn, so that only the comparison is skipped, and \
     fails on any other failure",
);

/// This file, which is not read.
const THIS_FILE: &str = "src/context_boundaries/setuid_tools.rs";

/// The form of a shell's start of `ps` that `text` holds, if any: a path
/// that ends in [`BIN_PS`] (`/usr/bin/ps` too), whatever follows it, or the
/// word `ps` before a flag of [`SHELL_FLAGS`]. The word is not the end of
/// `maps`, of `x-ps`, nor of another path (`./ps`).
fn shell_start(text: &str) -> Option<String> {
    let names_bin_ps = text.match_indices(BIN_PS).any(|(at, _)| {
        !text[at + BIN_PS.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '-')
    });
    if names_bin_ps {
        return Some(BIN_PS.to_owned());
    }
    text.match_indices("ps").find_map(|(at, _)| {
        let word = !text[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || "_-./".contains(c));
        let rest = &text[at + 2..];
        let flags = rest.trim_start();
        let flag = SHELL_FLAGS.iter().find(|flag| flags.starts_with(**flag))?;
        (word && flags.len() < rest.len()).then(|| format!("ps {flag}"))
    })
}

/// The starts in a source, as (what starts, the innermost fn).
fn starts(source: &source::Source) -> BTreeSet<(String, String)> {
    let inner = |fns: &[String]| fns.last().map_or("-", String::as_str).to_owned();
    let calls = source.calls.iter().filter_map(|call| {
        let names: Vec<&str> = call.path.iter().map(String::as_str).collect();
        let starter = names.ends_with(&["Command", "new"]) || names.last() == Some(&"tool");
        let program = call.first.rsplit('/').next().unwrap_or_default();
        (starter && PROGRAMS.contains(&program)).then(|| {
            (
                format!("{}({:?})", names.join("::"), call.first),
                inner(&call.fns),
            )
        })
    });
    let literals = source
        .literals
        .iter()
        .filter_map(|literal| Some((shell_start(&literal.text)?, inner(&literal.fns))));
    calls.chain(literals).collect()
}

/// Every failure of the files `rust` (paths from the repository root, with
/// their text), one line each.
fn check(rust: &[(String, String)]) -> Vec<String> {
    let mut errors = BTreeSet::new();
    for (path, text) in rust {
        if path == THIS_FILE || HOST_ONLY.iter().any(|(host, _)| host == path) {
            continue;
        }
        let source = match source::read_with_tests(text) {
            Ok(source) => source,
            Err(error) => {
                errors.insert(format!("{path}: not read as Rust: {error}"));
                continue;
            }
        };
        for (what, function) in starts(&source) {
            if (path.as_str(), function.as_str()) != (COMPARISON.0, COMPARISON.1) {
                errors.insert(format!("{path}: {function} starts {what}"));
            }
        }
    }
    errors.into_iter().collect()
}

/// The rows that no longer say anything of `rust`: a module that is gone, and
/// a helper that starts no `ps`. The fix of either removes its row.
fn stale_rows(rust: &[(String, String)]) -> Vec<String> {
    let text_of = |path: &str| rust.iter().find(|(file, _)| file == path).map(|(_, t)| t);
    let mut stale: Vec<String> = HOST_ONLY
        .iter()
        .filter(|(path, _)| text_of(path).is_none())
        .map(|(path, _)| format!("HOST_ONLY: {path} is gone"))
        .collect();
    let (file, function, _) = COMPARISON;
    let starts_there = text_of(file)
        .and_then(|text| source::read_with_tests(text).ok())
        .is_some_and(|source| starts(&source).iter().any(|(_, f)| f == function));
    if !starts_there {
        stale.push(format!("COMPARISON: {function} of {file} starts no ps"));
    }
    stale
}

#[test]
fn ps_and_top_start_only_where_a_refusal_stops_nothing() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut rust = super::files(root, "src", "rs");
    rust.extend(super::files(root, "tests", "rs"));
    let mut allowed: Vec<String> = HOST_ONLY
        .iter()
        .map(|(path, why)| format!("{path}: {why}"))
        .collect();
    allowed.push(format!(
        "{} in {}: {}",
        COMPARISON.1, COMPARISON.0, COMPARISON.2
    ));
    let errors = check(&rust);
    assert!(
        errors.is_empty(),
        "ps or top started where a sandbox's refusal stops the work; read processes with \
         infrastructure::process_read (docs/development/testing.md):\n{}\nallowed:\n{}",
        errors.join("\n"),
        allowed.join("\n")
    );
    assert_eq!(stale_rows(&rust), Vec::<String>::new());
}

mod fixtures {
    use super::*;

    /// The text with `NEW` for the call that starts a process: spelled out
    /// here, each line of these fixtures would read as a test that starts
    /// one to `scripts/check-test-rules.sh`.
    fn spell(text: &str) -> String {
        text.replace("NEW", concat!("Command", "::new"))
    }

    fn tree(files: &[(&str, &str)]) -> Vec<(String, String)> {
        files
            .iter()
            .map(|(path, text)| (path.to_string(), spell(text)))
            .collect()
    }

    fn errors(path: &str, text: &str) -> Vec<String> {
        check(&tree(&[(path, text)]))
    }

    /// Each start, as a line of Rust in a fn, with what the error names.
    const STARTS: [(&str, &str); 15] = [
        (r#"NEW("ps").output();"#, "NEW(\"ps\")"),
        (r#"NEW("top").output();"#, "NEW(\"top\")"),
        (r#"std::process::NEW("ps");"#, "std::process::NEW(\"ps\")"),
        (
            r#"tokio::process::NEW("/usr/bin/top");"#,
            "tokio::process::NEW(\"/usr/bin/top\")",
        ),
        (r#"tool("ps", &["-A"]);"#, "tool(\"ps\")"),
        (r#"tool("top", &[]);"#, "tool(\"top\")"),
        (
            r#"host_metrics::tool("ps", &[]);"#,
            "host_metrics::tool(\"ps\")",
        ),
        (r#"assert!(NEW("ps").status().is_ok());"#, "NEW(\"ps\")"),
        (r#"sh("/bin/ps -p 1");"#, "/bin/ps"),
        (r#"sh("/usr/bin/ps -p 1");"#, "/bin/ps"),
        (r#"sh("ps -o lstart= -p 1");"#, "ps -o"),
        (r#"sh("LC_ALL=C ps -A -o pid=");"#, "ps -A"),
        (r#"sh("x=$(ps -axo pid,ppid)");"#, "ps -ax"),
        (r#"sh("ps -U 501 | grep x");"#, "ps -U"),
        (r#"script!(b"ps  -o pid=");"#, "ps -o"),
    ];

    #[test]
    fn each_start_outside_the_allowed_fails_in_code_and_in_tests() {
        for (line, what) in STARTS {
            for (path, text) in [
                (
                    "src/infrastructure/other.rs",
                    format!("fn f() {{ {line} }}"),
                ),
                // The module this check keeps out of the allowed.
                (
                    "src/infrastructure/environment.rs",
                    format!("fn f() {{ {line} }}"),
                ),
                (
                    "src/application/other.rs",
                    format!("#[cfg(test)] mod tests {{ fn f() {{ {line} }} }}"),
                ),
                (
                    "src/application/tests_only.rs",
                    format!("#![cfg(test)]\nfn f() {{ {line} }}"),
                ),
                ("tests/it/other.rs", format!("fn f() {{ {line} }}")),
                (
                    "tests/common/other.rs",
                    format!("#[test] fn f() {{ {line} }}"),
                ),
            ] {
                let found = errors(path, &text);
                assert_eq!(
                    found,
                    [spell(&format!("{path}: f starts {what}"))],
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn nothing_fails_in_the_host_only_modules() {
        for (path, _) in HOST_ONLY {
            for (line, _) in STARTS {
                assert_eq!(
                    errors(path, &format!("fn f() {{ {line} }}")),
                    Vec::<String>::new()
                );
            }
        }
    }

    /// Inside the helper the comparison passes; another fn of its file, and
    /// a fn of its name in another file, fail.
    #[test]
    fn only_the_comparisons_helper_in_its_file_may_start_ps() {
        let (file, function, _) = COMPARISON;
        let start = r#"{ let mut c = NEW("ps"); c.args(["-o", "lstart="]); }"#;
        let inside = format!("fn {function}() {start}");
        assert_eq!(errors(file, &inside), Vec::<String>::new());
        assert_eq!(
            errors(file, &format!("{inside}\nfn other() {start}")),
            [spell(&format!("{file}: other starts NEW(\"ps\")"))]
        );
        assert_eq!(
            errors("tests/it/other.rs", &inside),
            [spell(&format!(
                "tests/it/other.rs: {function} starts NEW(\"ps\")"
            ))]
        );
    }

    #[test]
    fn what_is_no_start_does_not_fail() {
        let text = r##"
//! Reads what `ps -o lstart=` prints, with NEW("ps").
/// `/bin/ps`
fn f(ps: &Path, dir: &Path) {
    NEW(ps).args(["-o", "lstart="]);
    NEW("podman").args(["ps", "--quiet"]);
    tool(dir, "ps", "exit 0");
    tool(StepKind::ToolCall, "top", 500, 4_500);
    tool("vm_stat", &[]);
    fake.tool("ps");
    assert!(fake.tool("ps").is_ok());
    vec![Row { tool: ("ps", 1) }];
    other("ps");
    script(&bin.join("ps"), "#!/bin/sh\necho 1");
    sh("./ps -o pid=");
    sh("maps -o x");
    sh("top-ps -A");
    sh("/bin/ps2 x");
    sh("ps");
    sh("`ps` to exit");
    let top = "top";
}
"##;
        assert_eq!(errors("src/other.rs", text), Vec::<String>::new());
    }

    #[test]
    fn this_file_is_not_read_and_a_file_that_does_not_parse_fails() {
        let text = r#"fn f() { NEW("ps"); }"#;
        assert_eq!(errors(THIS_FILE, text), Vec::<String>::new());
        let found = errors("src/broken.rs", "fn {");
        assert_eq!(found.len(), 1);
        assert!(
            found[0].starts_with("src/broken.rs: not read as Rust: "),
            "{found:?}"
        );
    }

    #[test]
    fn a_row_that_says_nothing_of_the_tree_is_stale() {
        let helper = format!(r#"fn {}() {{ NEW("ps"); }}"#, COMPARISON.1);
        let mut files: Vec<(&str, &str)> = HOST_ONLY.iter().map(|(path, _)| (*path, "")).collect();
        files.push((COMPARISON.0, &helper));
        assert_eq!(stale_rows(&tree(&files)), Vec::<String>::new());

        // The helper starts no ps any more, and a module is gone.
        files.pop();
        files.push((COMPARISON.0, "fn other() { NEW(\"ps\"); }"));
        files.remove(0);
        assert_eq!(
            stale_rows(&tree(&files)),
            [
                format!("HOST_ONLY: {} is gone", HOST_ONLY[0].0),
                format!(
                    "COMPARISON: {} of {} starts no ps",
                    COMPARISON.1, COMPARISON.0
                ),
            ]
        );
    }
}
