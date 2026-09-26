//! Why a verification command of `integrate` failed after a clean rebase
//! (task 467, goal 37): one reading of its exit and its log, so the event
//! says whether the code broke (a build error, a failing test, a lint, the
//! format, the coverage) or the host did (the disk filled up, the command
//! was killed, a test ran out of time under load) without opening the log.
//! The marks follow cargo's, nextest's, rustfmt's and `tests/common`'s
//! output. How a class is handled (a retry instead of a resume) is not
//! decided here.
use serde::Serialize;
use serde_json::{Value, json};

/// What a failed verification command is put down to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// The disk filled up (`No space left on device`, `os error 28`).
    DiskFull,
    /// A signal ended the command or one of its processes (from outside,
    /// or the shell's exit 128 + N).
    Killed,
    /// A test ran out of its time: `tests/common`'s `within`, nextest's
    /// `TIMEOUT`, or `timeout(1)`'s exit 124.
    Timeout,
    /// The code did not compile (`error[E…]`, `could not compile`).
    BuildError,
    /// clippy denied a lint.
    Lint,
    /// A test failed.
    TestFailure,
    /// `cargo fmt --check` found a diff.
    Format,
    /// The line coverage fell under `--fail-under-lines`.
    CoverageBelow,
    /// None of the marks.
    Unknown,
}

impl FailureClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DiskFull => "disk_full",
            Self::Killed => "killed",
            Self::Timeout => "timeout",
            Self::BuildError => "build_error",
            Self::Lint => "lint",
            Self::TestFailure => "test_failure",
            Self::Format => "format",
            Self::CoverageBelow => "coverage_below",
            Self::Unknown => "unknown",
        }
    }
}

/// The class of a failure and the line (or exit) that shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyFailure {
    pub class: FailureClass,
    /// The first line that carries the class's mark, trimmed to
    /// [`EVIDENCE_CHARS`]; for a build error with its `-->` location; for
    /// a kill without a mark in the log, the exit; for `unknown` the log's
    /// last line.
    pub evidence: String,
}

impl VerifyFailure {
    /// The `failure` of the `verification_command` and
    /// `integration_deferred` payloads.
    pub fn to_json(&self) -> Value {
        json!({"class": self.class, "evidence": self.evidence})
    }
}

/// The longest evidence kept, in characters.
pub const EVIDENCE_CHARS: usize = 300;

/// Classify the failure of `command` that exited with `exit_code` (`None`
/// when a signal ended it) or `signal`, from its `log`. The classes are
/// tried in order: what the host did first, since a full disk or a kill
/// also leaves build and test errors behind.
pub fn classify(
    command: &str,
    exit_code: Option<i32>,
    signal: Option<i32>,
    log: &str,
) -> VerifyFailure {
    let text = strip_ansi(log);
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let found = |class, line: &str| VerifyFailure {
        class,
        evidence: shorten(line),
    };
    let first = |mark: &dyn Fn(&str) -> bool| lines.iter().position(|line| mark(line));

    if let Some(at) = first(&|line| {
        line.contains("No space left on device")
            || line.contains("os error 28")
            || line.contains("ENOSPC")
    }) {
        return found(FailureClass::DiskFull, lines[at]);
    }
    let shell_signal = exit_code
        .filter(|code| (129..=192).contains(code))
        .map(|code| code - 128);
    if let Some(signal) = signal.or(shell_signal) {
        let name = signal_name(signal);
        let evidence = match exit_code {
            Some(code) => format!("exit {code} (signal {signal}, {name})"),
            None => format!("killed by signal {signal} ({name})"),
        };
        return found(FailureClass::Killed, &evidence);
    }
    if let Some(at) = first(&|line| {
        // cargo on a process it ran: `process didn't exit successfully: … (signal: 9, SIGKILL: kill)`.
        (line.contains("process didn't exit successfully")
            && (line.contains("(signal: 9, SIGKILL") || line.contains("(signal: 15, SIGTERM")))
            || line.starts_with("SIGKILL [")
            || line.starts_with("SIGTERM [")
    }) {
        return found(FailureClass::Killed, lines[at]);
    }
    if let Some(at) = first(&|line| {
        // `tests/common`'s `within` starts its own line with the test.
        (line.starts_with("test ")
            && line.contains(" timed out: ")
            && line.contains("did not happen within"))
            || line.starts_with("TIMEOUT [")
    }) {
        return found(FailureClass::Timeout, lines[at]);
    }
    if exit_code == Some(124) {
        return found(FailureClass::Timeout, "exit 124 (timeout)");
    }
    if let Some(at) = first(&|line| line.starts_with("error[E")) {
        let location = lines[at + 1..]
            .iter()
            .take(5)
            .find(|line| line.starts_with("-->"));
        return found(
            FailureClass::BuildError,
            &match location {
                Some(location) => format!("{} {location}", lines[at]),
                None => lines[at].to_owned(),
            },
        );
    }
    if let Some(at) = first(&|line| {
        line.starts_with("error: could not compile") || line.starts_with("error: linking with")
    }) {
        if command.contains("clippy") && lines[at].starts_with("error: could not compile") {
            // The lint itself, not cargo's summary of it.
            let lint = first(&|line| {
                line.starts_with("error: ") && !line.starts_with("error: could not compile")
            });
            return found(FailureClass::Lint, lines[lint.unwrap_or(at)]);
        }
        return found(FailureClass::BuildError, lines[at]);
    }
    if let Some(at) = first(&|line| line.starts_with("FAIL [")).or_else(|| {
        first(&|line| {
            (line.starts_with("test ") && line.ends_with(" ... FAILED"))
                || line.starts_with("test result: FAILED")
                || line.starts_with("error: test failed")
                || line.starts_with("error: test run failed")
        })
    }) {
        return found(FailureClass::TestFailure, lines[at]);
    }
    if let Some(at) = first(&|line| line.starts_with("Diff in ")) {
        return found(FailureClass::Format, lines[at]);
    }
    if command.contains("--fail-under-")
        && let Some(at) = first(&|line| line.starts_with("TOTAL "))
    {
        return found(FailureClass::CoverageBelow, lines[at]);
    }
    match lines.iter().rev().find(|line| !line.is_empty()) {
        Some(line) => found(FailureClass::Unknown, line),
        None => found(
            FailureClass::Unknown,
            &match exit_code {
                Some(code) => format!("exit {code} with an empty log"),
                None => "an empty log".to_owned(),
            },
        ),
    }
}

/// The most test names one failed command keeps (task 515); the rest
/// are only counted.
pub const MAX_FAILED_TESTS: usize = 20;

/// The tests a failed command's output names as failed (task 515): the
/// first [`MAX_FAILED_TESTS`] in the order they appear, each once, and how
/// many more there were.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FailedTests {
    pub names: Vec<String>,
    pub omitted: usize,
}

impl FailedTests {
    /// Whether it names none.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.omitted == 0
    }
}

/// The tests `log` names as failed, from the marks of cargo test
/// (`test <name> ... FAILED`, `---- <name> stdout ----`, the list under
/// `failures:`, a test thread's `thread '<name>' panicked`), of nextest
/// (`FAIL [`, `TIMEOUT [`, `SIG… [` with the test last) and of
/// `tests/common`'s `within` (`test <name> timed out: …`). A failure that
/// names no test (a panic outside a test, `error: test failed` alone)
/// gives none.
pub fn failed_tests(log: &str) -> FailedTests {
    let text = strip_ansi(log);
    let mut found: Vec<String> = Vec::new();
    let mut in_list = false;
    for raw in text.lines() {
        let line = raw.trim();
        if in_list {
            // `failures:` then one indented name per line, to a blank line.
            if !line.is_empty() && raw.starts_with([' ', '\t']) {
                found.extend(test_name(line));
                continue;
            }
            in_list = false;
        }
        if raw.trim_end() == "failures:" {
            in_list = true;
            continue;
        }
        let name = if let Some(rest) = line.strip_prefix("test ") {
            rest.strip_suffix(" ... FAILED").or_else(|| {
                rest.contains("did not happen within")
                    .then(|| rest.split(" timed out: ").next())
                    .flatten()
            })
        } else if let Some(rest) = line.strip_prefix("---- ") {
            rest.strip_suffix(" stdout ----")
                .or_else(|| rest.strip_suffix(" stderr ----"))
        } else if let Some(rest) = line.strip_prefix("thread '") {
            // Only a test's thread: tests are paths.
            rest.split_once("' panicked")
                .map(|(name, _)| name)
                .filter(|name| name.contains("::"))
        } else if nextest_failure(line) {
            line.split_once(']')
                .and_then(|(_, rest)| rest.split_whitespace().last())
        } else {
            None
        };
        found.extend(name.and_then(test_name));
    }
    let mut names: Vec<String> = Vec::new();
    for name in found {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let omitted = names.len().saturating_sub(MAX_FAILED_TESTS);
    names.truncate(MAX_FAILED_TESTS);
    FailedTests { names, omitted }
}

/// A nextest status line of a failed test: `FAIL [`, `TRY 2 FAIL [`,
/// `TIMEOUT [`, `SIGKILL [` and the other signals.
fn nextest_failure(line: &str) -> bool {
    let Some((status, rest)) = line.split_once(" [") else {
        return false;
    };
    // The status starts the line (`TRY n` may come first), and a binary
    // and a test follow the time: not a line that only says `SIGTERM [`.
    let status = match status.split_whitespace().collect::<Vec<_>>()[..] {
        [status] => status,
        ["TRY", n, status] if n.chars().all(|c| c.is_ascii_digit()) => status,
        _ => return false,
    };
    let after: Vec<&str> = rest
        .split_once(']')
        .map(|(_, after)| after.split_whitespace().collect())
        .unwrap_or_default();
    let words = after
        .iter()
        .filter(|word| !word.starts_with('(') && !word.ends_with(')'));
    let is_status = matches!(status, "FAIL" | "LEAK-FAIL" | "TIMEOUT" | "ABORT")
        || (status.len() > 3
            && status.starts_with("SIG")
            && status[3..].chars().all(|c| c.is_ascii_uppercase()));
    is_status && words.count() >= 2
}

/// `text` when it looks like a test's name (a Rust path).
fn test_name(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':'))
    .then(|| text.to_owned())
}

fn signal_name(signal: i32) -> &'static str {
    match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        _ => "unnamed",
    }
}

fn shorten(line: &str) -> String {
    let line = line.trim();
    match line.char_indices().nth(EVIDENCE_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_owned(),
    }
}

/// `text` without the terminal's color escapes (`ESC [ … letter`).
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LLVM_COV: &str = "cargo llvm-cov nextest --locked --fail-under-lines 80";

    fn class(command: &str, exit: Option<i32>, signal: Option<i32>, log: &str) -> FailureClass {
        classify(command, exit, signal, log).class
    }

    #[test]
    fn a_compile_error_is_a_build_error_with_its_location() {
        // Task 292's landing on top of 360: a field 360 added was missing.
        let log = "   Compiling dagq v0.4.0\n\
error[E0063]: missing field `finding_id` in initializer of `domain::NewAsk`\n\
   --> src/application/recovery.rs:120:20\n\
    |\n\
error: could not compile `dagq` (lib test) due to 1 previous error\n";
        let failure = classify(LLVM_COV, Some(1), None, log);
        assert_eq!(failure.class, FailureClass::BuildError);
        assert_eq!(
            failure.evidence,
            "error[E0063]: missing field `finding_id` in initializer of `domain::NewAsk` --> src/application/recovery.rs:120:20"
        );
        assert_eq!(
            class(
                "cargo build",
                Some(101),
                None,
                "error: linking with `cc` failed: exit status: 1\n"
            ),
            FailureClass::BuildError
        );
        assert_eq!(
            class(
                "cargo build",
                Some(101),
                None,
                "error: could not compile `dagq` (bin \"dagq\")\n"
            ),
            FailureClass::BuildError
        );
    }

    #[test]
    fn a_failing_test_names_the_test() {
        let nextest = "        PASS [   0.010s] (1/2) dagq::it a::b\n\
        FAIL [   4.935s] (725/779) dagq::it runtime_session::unanswered_exit_request\n\
error: test run failed\n";
        let failure = classify(LLVM_COV, Some(100), None, nextest);
        assert_eq!(failure.class, FailureClass::TestFailure);
        assert_eq!(
            failure.evidence,
            "FAIL [   4.935s] (725/779) dagq::it runtime_session::unanswered_exit_request"
        );
        let cargo_test = "test a::passes ... ok\ntest a::breaks ... FAILED\n\nfailures:\n\
test result: FAILED. 1 passed; 1 failed\nerror: test failed, to rerun pass `--lib`\n";
        let failure = classify("cargo test --locked", Some(101), None, cargo_test);
        assert_eq!(failure.class, FailureClass::TestFailure);
        assert_eq!(failure.evidence, "test a::breaks ... FAILED");
        assert_eq!(
            class(
                "cargo test",
                Some(101),
                None,
                "error: test failed, to rerun pass `--test runtime`\n"
            ),
            FailureClass::TestFailure
        );
    }

    #[test]
    fn a_signal_or_the_shells_exit_above_128_is_a_kill() {
        let failure = classify(LLVM_COV, None, Some(9), "   Compiling dagq\n");
        assert_eq!(failure.class, FailureClass::Killed);
        assert_eq!(failure.evidence, "killed by signal 9 (SIGKILL)");
        let failure = classify(LLVM_COV, Some(143), None, "test a ... FAILED\n");
        assert_eq!(failure.class, FailureClass::Killed);
        assert_eq!(failure.evidence, "exit 143 (signal 15, SIGTERM)");
        assert_eq!(
            classify(LLVM_COV, Some(137), None, "").evidence,
            "exit 137 (signal 9, SIGKILL)"
        );
        assert_eq!(
            classify(LLVM_COV, Some(160), None, "").evidence,
            "exit 160 (signal 32, unnamed)"
        );
        // A process under cargo killed from outside (the OOM killer).
        let log = "error: could not compile `dagq` (lib)\n\n\
Caused by:\n  process didn't exit successfully: `rustc --crate-name dagq` (signal: 9, SIGKILL: kill)\n";
        let failure = classify(LLVM_COV, Some(101), None, log);
        assert_eq!(failure.class, FailureClass::Killed);
        assert!(failure.evidence.ends_with("(signal: 9, SIGKILL: kill)"));
        assert_eq!(
            class(
                LLVM_COV,
                Some(100),
                None,
                "     SIGKILL [ 3.0s] dagq::it a::b\n"
            ),
            FailureClass::Killed
        );
    }

    #[test]
    fn no_space_left_is_a_full_disk_before_the_build_error_it_causes() {
        // Task 276 at 13:26: the disk filled up during the llvm-cov build.
        let log = "error: couldn't create a temp dir: No space left on device (os error 28) at path \"/q/target/deps/rmeta\"\n\
error: could not compile `dagq` (lib)\n";
        let failure = classify(LLVM_COV, Some(1), None, log);
        assert_eq!(failure.class, FailureClass::DiskFull);
        assert!(
            failure
                .evidence
                .starts_with("error: couldn't create a temp dir")
        );
        assert_eq!(
            class(LLVM_COV, Some(1), None, "write failed: os error 28\n"),
            FailureClass::DiskFull
        );
        // Even when the build was killed on the way.
        assert_eq!(
            class("sh", Some(143), None, "cc: ENOSPC while writing\n"),
            FailureClass::DiskFull
        );
    }

    #[test]
    fn a_test_out_of_time_is_a_timeout_before_the_test_failure() {
        // tests/common's within, and the failures cargo reports after it.
        let log = "running 3 tests\n\
test runtime_x::waits timed out: the run to land did not happen within 20s\n\
test result: FAILED. 2 passed; 1 failed\n";
        let failure = classify(LLVM_COV, Some(101), None, log);
        assert_eq!(failure.class, FailureClass::Timeout);
        assert_eq!(
            failure.evidence,
            "test runtime_x::waits timed out: the run to land did not happen within 20s"
        );
        assert_eq!(
            class(
                LLVM_COV,
                Some(100),
                None,
                "     TIMEOUT [ 600.003s] dagq::it a::b\n        FAIL [0.1s] dagq::it c::d\n"
            ),
            FailureClass::Timeout
        );
        let failure = classify("timeout 60 cargo test", Some(124), None, "");
        assert_eq!(failure.class, FailureClass::Timeout);
        assert_eq!(failure.evidence, "exit 124 (timeout)");
    }

    #[test]
    fn the_marks_quoted_by_a_failing_assertion_are_not_the_cause() {
        // A test about `within` or a kill fails and prints the text it expected.
        let log = "thread 'a::b' panicked at tests/it/x.rs:1:1:\n\
stderr: \"test a::b timed out: x did not happen within 1s\"\n\
assertion failed: e.ends_with(\"(signal: 9, SIGKILL: kill)\")\n\
test a::b ... FAILED\n";
        assert_eq!(
            class("cargo test", Some(101), None, log),
            FailureClass::TestFailure
        );
        // A clippy run that failed to link is a build error.
        assert_eq!(
            class(
                "cargo clippy",
                Some(101),
                None,
                "error: linking with `cc` failed\nerror: could not compile `dagq`\n"
            ),
            FailureClass::BuildError
        );
    }

    #[test]
    fn clippy_format_and_coverage_are_their_own() {
        let clippy = "error: this `if` has identical blocks\n  --> src/a.rs:1:1\n  |\n  = help: for further information visit https://rust-lang.github.io/rust-clippy/master/index.html#if_same_then_else\n\
error: could not compile `dagq` (lib) due to 1 previous error\n";
        let failure = classify(
            "cargo clippy --locked -- -D warnings",
            Some(101),
            None,
            clippy,
        );
        assert_eq!(failure.class, FailureClass::Lint);
        assert_eq!(failure.evidence, "error: this `if` has identical blocks");
        let failure = classify(
            "cargo fmt --all --check",
            Some(1),
            None,
            "Diff in /w/src/a.rs:12:\n-fn a(){}\n+fn a() {}\n",
        );
        assert_eq!(failure.class, FailureClass::Format);
        assert_eq!(failure.evidence, "Diff in /w/src/a.rs:12:");
        let report = "Filename  Regions  Lines  Cover\n\
TOTAL  1000  100  90.00%  20000  4100  79.50%\n";
        let failure = classify(LLVM_COV, Some(1), None, report);
        assert_eq!(failure.class, FailureClass::CoverageBelow);
        assert!(failure.evidence.starts_with("TOTAL "));
        // Without --fail-under-* the table alone says nothing.
        assert_eq!(
            class("cargo llvm-cov", Some(1), None, report),
            FailureClass::Unknown
        );
    }

    #[test]
    fn anything_else_is_unknown_with_the_last_line() {
        let failure = classify("make", Some(2), None, "one\nmake: *** [all] Error 2\n\n");
        assert_eq!(failure.class, FailureClass::Unknown);
        assert_eq!(failure.evidence, "make: *** [all] Error 2");
        assert_eq!(
            classify("false", Some(1), None, "").evidence,
            "exit 1 with an empty log"
        );
        assert_eq!(classify("x", None, None, "").evidence, "an empty log");
    }

    #[test]
    fn the_evidence_is_short_and_without_colors() {
        let long = format!(
            "\u{1b}[1m\u{1b}[31merror[E0425]\u{1b}[0m: {}\n",
            "é".repeat(400)
        );
        let failure = classify("cargo build", Some(101), None, &long);
        assert_eq!(failure.class, FailureClass::BuildError);
        assert!(failure.evidence.starts_with("error[E0425]: é"));
        assert_eq!(failure.evidence.chars().count(), EVIDENCE_CHARS + 1);
        assert!(failure.evidence.ends_with('…'));
        assert_eq!(strip_ansi("a\u{1b}b"), "a");
        assert_eq!(
            classify("x", Some(1), None, "boom").to_json(),
            json!({"class": "unknown", "evidence": "boom"})
        );
        assert_eq!(FailureClass::DiskFull.as_str(), "disk_full");
        for class in [
            FailureClass::Killed,
            FailureClass::Timeout,
            FailureClass::BuildError,
            FailureClass::Lint,
            FailureClass::TestFailure,
            FailureClass::Format,
            FailureClass::CoverageBelow,
            FailureClass::Unknown,
        ] {
            assert_eq!(json!(class), json!(class.as_str()));
        }
    }

    fn names(log: &str) -> Vec<String> {
        failed_tests(log).names
    }

    #[test]
    fn one_failed_test_is_named_once() {
        // cargo test names it on its line, over its output and in the list.
        let log = "running 2 tests\n\
test a::passes ... ok\n\
test a::breaks ... FAILED\n\
\n\
failures:\n\
\n\
---- a::breaks stdout ----\n\
assertion failed: false\n\
\n\
\n\
failures:\n\
    a::breaks\n\
\n\
test result: FAILED. 1 passed; 1 failed; 0 ignored\n\
error: test failed, to rerun pass `--lib`\n";
        assert_eq!(
            failed_tests(log),
            FailedTests {
                names: vec!["a::breaks".to_owned()],
                omitted: 0
            }
        );
    }

    #[test]
    fn several_failed_tests_keep_their_order_up_to_the_limit() {
        let log = "\u{1b}[31mtest b::one ... FAILED\u{1b}[0m\ntest a::two ... FAILED\n\nfailures:\n    b::one\n    a::two\n    c::three\n\ntest result: FAILED. 0 passed; 3 failed\n";
        assert_eq!(names(log), ["b::one", "a::two", "c::three"]);
        // nextest: the test is the last word of the status line.
        let nextest = "        PASS [   0.010s] (1/4) dagq::it a::b\n\
        FAIL [   4.935s] (2/4) dagq::it runtime_session::exit_request\n\
     TIMEOUT [ 600.003s] (3/4) dagq::it runtime_claim::waits\n\
     SIGKILL [   1.000s] dagq e2e_happy_path\n\
   LEAK-FAIL [   0.200s] (4/4) dagq::it runtime_claim::leaks\n\
sent SIGTERM [pid 42] to worker\n\
        FAIL [   0.100s]\n\
  TRY 2 FAIL [   0.100s] dagq::it runtime_session::exit_request\n\
     Summary [ 610.000s] 4 tests run: 1 passed, 3 failed\n\
        FAIL [   4.935s] (2/4) dagq::it runtime_session::exit_request\n\
error: test run failed\n";
        assert_eq!(
            names(nextest),
            [
                "runtime_session::exit_request",
                "runtime_claim::waits",
                "e2e_happy_path",
                "runtime_claim::leaks"
            ]
        );
        let many: String = (0..MAX_FAILED_TESTS + 3)
            .map(|i| format!("test t::n{i} ... FAILED\n"))
            .collect();
        let failed = failed_tests(&many);
        assert_eq!(failed.names.len(), MAX_FAILED_TESTS);
        assert_eq!(failed.names[0], "t::n0");
        assert_eq!(failed.omitted, 3);
        assert!(!failed.is_empty());
    }

    #[test]
    fn a_panic_or_a_timeout_names_its_test() {
        // A test's thread that panicked, before cargo's own lines (cut off).
        let log = "thread 'runtime_claim::claims' panicked at tests/it/runtime_claim.rs:10:5:\n\
assertion `left == right` failed\n";
        assert_eq!(names(log), ["runtime_claim::claims"]);
        // tests/common's within: the test that ran out of time.
        let log = "test runtime_x::waits timed out: the run to land did not happen within 20s\n";
        assert_eq!(names(log), ["runtime_x::waits"]);
        // The quoted text of an assertion is not a mark.
        let log = "stderr: \"test a::b timed out: x did not happen within 1s\"\n";
        assert!(names(log).is_empty());
    }

    #[test]
    fn a_failure_without_a_name_names_none() {
        for log in [
            "",
            "error: test failed, to rerun pass `--test it`\n",
            "thread 'main' panicked at src/main.rs:1:1:\nboom\n",
            "test result: FAILED. 0 passed; 0 failed\n",
            "failures:\n\n---- odd name with spaces stdout ----\n",
            "   Compiling dagq\nerror[E0063]: missing field\n",
        ] {
            let failed = failed_tests(log);
            assert!(failed.is_empty(), "{log:?}: {failed:?}");
        }
    }
}
