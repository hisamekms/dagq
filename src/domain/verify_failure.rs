//! Why a verification command of `integrate` failed after a clean rebase
//! (task 467, goal 37): one reading of its exit and its log, so the event
//! says whether the code broke (a build error, a failing test, a lint, the
//! format, the coverage) or the host did (the disk filled up, the command
//! was killed, a test ran out of time under load) without opening the log.
//! The marks follow cargo's, nextest's, rustfmt's and `tests/common`'s
//! output. How a class is handled is [`FailureClass::is_environmental`]'s:
//! integrate retries a command that failed on the host once instead of
//! resuming the worker (task 639), and lands a run whose failed tests all
//! passed when nextest ran them again ([`FailureClass::Flaky`]) once more
//! (task 768).
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
    /// Every test that failed passed when nextest ran it again (`retries`
    /// of `.config/nextest.toml`): its `FLKY-FL` (`flaky-result = "fail"`)
    /// or `TRY n PASS` lines name each (task 768, ADR-t768-1).
    Flaky,
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
            Self::Flaky => "flaky",
            Self::BuildError => "build_error",
            Self::Lint => "lint",
            Self::TestFailure => "test_failure",
            Self::Format => "format",
            Self::CoverageBelow => "coverage_below",
            Self::Unknown => "unknown",
        }
    }

    /// Whether the class puts the failure down to the host rather than the
    /// code (task 639, ADR-t639-1): a full disk, a kill, a test out of
    /// time. Integrate retries such a command once instead of resuming the
    /// worker, and reports it to a person when it fails so again. The set
    /// is not widened without a person's decision.
    pub fn is_environmental(self) -> bool {
        matches!(self, Self::DiskFull | Self::Killed | Self::Timeout)
    }
}

/// A verification command that ran past its limit for the whole command
/// and was killed (task 639): the error the verifier returns for it, which
/// integrate records as a `timeout` failure instead of a landing error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandTimedOut {
    /// The limit, in seconds.
    pub limit_secs: u64,
}

impl CommandTimedOut {
    /// The failure integrate records for the command.
    pub fn failure(self) -> VerifyFailure {
        VerifyFailure {
            class: FailureClass::Timeout,
            evidence: self.to_string(),
        }
    }
}

impl std::fmt::Display for CommandTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the command ran past its {} s limit and was killed",
            self.limit_secs
        )
    }
}

impl std::error::Error for CommandTimedOut {}

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
    // Before the timeout: a test that ran out of time and passed when run
    // again is flaky, as any other.
    if let Some(evidence) = all_flaky(&text) {
        return found(FailureClass::Flaky, evidence);
    }
    // nextest's kill under its retries (`TRY n KILL [`), after the flaky:
    // a test killed once and passed on its retry is flaky, as any other.
    if let Some(at) = first(&|line| nextest_failed_as(line, NextestStatus::Killed)) {
        return found(FailureClass::Killed, lines[at]);
    }
    if let Some(at) = first(&|line| {
        // `tests/common`'s `within` starts its own line with the test.
        (line.starts_with("test ")
            && line.contains(" timed out: ")
            && line.contains("did not happen within"))
            || line.starts_with("TIMEOUT [")
            || nextest_failed_as(line, NextestStatus::TimedOut)
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
    if let Some(at) = first(&|line| {
        // nextest's failures but a kill and a timeout (above), also
        // after `TRY n` under its retries.
        line.starts_with("FAIL [") || nextest_failed_as(line, NextestStatus::Failed)
    })
    .or_else(|| {
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
/// (the failed statuses of [`NextestStatus`], with the test last) and of
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
        } else if let Some((status, test)) = nextest_line(line) {
            (status != NextestStatus::Flaky).then_some(test)
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

/// The tests `log` names as passed when nextest ran them again after a
/// failure (task 768): the last word of its `FLKY-FL n/m [` (the status of
/// a flaky test under `flaky-result = "fail"`), `FLAKY n/m [` and `TRY n
/// PASS [` lines, each once, in the order they appear.
pub fn flaky_tests(log: &str) -> Vec<String> {
    let text = strip_ansi(log);
    let mut names: Vec<String> = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(name) = flaky_line(line).and_then(test_name)
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    names
}

/// The test of a nextest status line that says it passed on a retry.
fn flaky_line(line: &str) -> Option<&str> {
    match nextest_line(line)? {
        (NextestStatus::Flaky, test) => Some(test),
        _ => None,
    }
}

/// The line that shows the failure flaky when every test `text` names as
/// failed is one nextest names as passed on its retry: the first flaky
/// status line. `None` when a failed test is not flaky, when the failure
/// names no test, or when some names were cut off.
fn all_flaky(text: &str) -> Option<&str> {
    let flaky = flaky_tests(text);
    if flaky.is_empty() {
        return None;
    }
    let failed = failed_tests(text);
    if failed.names.is_empty()
        || failed.omitted > 0
        || !failed.names.iter().all(|name| flaky.contains(name))
    {
        return None;
    }
    let lines = || text.lines().map(str::trim);
    lines()
        .find(|line| line.starts_with("FLKY-FL ") || line.starts_with("FLAKY "))
        .or_else(|| lines().find(|line| flaky_line(line).is_some()))
}

/// The names nextest gives the signals that end a test (its
/// `signal_str`): `SIG<name>` in the status of a first attempt, the bare
/// name after `TRY n`. Any other signal is `ABORT SIG <n>` / `TRY n SIG <n>`.
const NEXTEST_SIGNALS: [&str; 12] = [
    "HUP", "INT", "QUIT", "ILL", "TRAP", "ABRT", "FPE", "KILL", "SEGV", "PIPE", "ALRM", "TERM",
];

/// What a nextest status line says of its test (task 1272). The set is
/// the explicit one of docs/design/supervisor-lifecycle/integrate.md, which
/// scripts/stress-recent-tests.sh and the Linux job of ci.yml match too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NextestStatus {
    /// The test failed: `FAIL`, `FAIL + LEAK`, `XFAIL`, `LEAK-FAIL`,
    /// `ABORT`, a signal other than a kill, and after `TRY n` their short
    /// forms (`FAIL`, `FL+LK`, `XFAIL`, `LKFAIL`, `ABORT`).
    Failed,
    /// The test ran out of time: `TIMEOUT`, `TRY n TMT`.
    TimedOut,
    /// A kill ended the test: `SIGKILL`, `SIGTERM`, `TRY n KILL`, `TRY n
    /// TERM`.
    Killed,
    /// The test passed on a retry: `FLKY-FL n/m`, `FLAKY n/m`, `TRY n PASS`.
    Flaky,
}

/// The status of a nextest status line (the words before ` [`), or `None`
/// for a status that does not fail a test (`PASS`, `LEAK`, `SLOW`,
/// `TRY n SLOW`, `START`, `TERMINATING`, `DELAY n/m`, `SKIP`, …).
fn nextest_status(words: &[&str]) -> Option<NextestStatus> {
    use NextestStatus::*;
    let number = |n: &str| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit());
    let signal = |name: &str| match name {
        "KILL" | "TERM" => Some(Killed),
        _ if NEXTEST_SIGNALS.contains(&name) => Some(Failed),
        _ => None,
    };
    match words {
        ["FAIL"] | ["FAIL", "+", "LEAK"] | ["XFAIL"] | ["LEAK-FAIL"] | ["ABORT"] => Some(Failed),
        ["TIMEOUT"] => Some(TimedOut),
        [status] => signal(status.strip_prefix("SIG")?),
        ["ABORT", "SIG", n] if number(n) => Some(Failed),
        ["FLKY-FL" | "FLAKY", count] => {
            let (attempt, total) = count.split_once('/')?;
            (number(attempt) && number(total)).then_some(Flaky)
        }
        ["TRY", n, rest @ ..] if number(n) => match rest {
            ["FAIL" | "FL+LK" | "XFAIL" | "LKFAIL" | "ABORT"] => Some(Failed),
            ["TMT"] => Some(TimedOut),
            ["PASS"] => Some(Flaky),
            ["SIG", n] if number(n) => Some(Failed),
            [name] => signal(name),
            _ => None,
        },
        _ => None,
    }
}

/// The status and the test of a nextest status line of a test that failed
/// or passed on a retry: the status starts the line, and a binary and a
/// test follow the time (not a line that only says `SIGTERM [`). The test
/// is the last word, so a status of several words (`FAIL + LEAK`, `ABORT
/// SIG 64`, `TRY 2 SIG 64`) does not move it.
fn nextest_line(line: &str) -> Option<(NextestStatus, &str)> {
    let (status, rest) = line.split_once(" [")?;
    let status = nextest_status(&status.split_whitespace().collect::<Vec<_>>())?;
    let (_, after) = rest.split_once(']')?;
    let words: Vec<&str> = after
        .split_whitespace()
        .filter(|word| !word.starts_with('(') && !word.ends_with(')'))
        .collect();
    // A binary and a test.
    (words.len() >= 2).then(|| (status, words[words.len() - 1]))
}

/// A nextest status line of a test that failed as `status` says.
fn nextest_failed_as(line: &str, status: NextestStatus) -> bool {
    nextest_line(line).is_some_and(|(found, _)| found == status)
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
    #[test]
    fn the_host_classes_are_environmental_and_the_code_classes_are_not() {
        use FailureClass::*;
        for class in [DiskFull, Killed, Timeout] {
            assert!(class.is_environmental(), "{class:?}");
        }
        for class in [
            Flaky,
            BuildError,
            Lint,
            TestFailure,
            Format,
            CoverageBelow,
            Unknown,
        ] {
            assert!(!class.is_environmental(), "{class:?}");
        }
        let timed_out = CommandTimedOut { limit_secs: 1800 };
        assert_eq!(
            timed_out.failure(),
            VerifyFailure {
                class: Timeout,
                evidence: "the command ran past its 1800 s limit and was killed".to_owned(),
            }
        );
        let error = anyhow::Error::new(timed_out).context("verification command \"x\"");
        assert_eq!(error.downcast_ref::<CommandTimedOut>(), Some(&timed_out));
    }

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
            FailureClass::Flaky,
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

    /// nextest 0.9.146 with `retries = 1` and `flaky-result = "fail"`: a
    /// test that failed and passed on its retry (`runtime_x::flaky`), and
    /// one that failed both times (`runtime_x::broken`).
    fn nextest_retried(broken: bool) -> String {
        let mut lines = vec![
            "────────────",
            " Nextest run ID b7e8aa25 with nextest profile: default",
            "    Starting 3 tests across 1 binary",
            "  TRY 1 FAIL [   0.010s] (───) dagq::it runtime_x::flaky",
            "  stdout ───",
            "    running 1 test",
            "    test runtime_x::flaky ... FAILED",
            "",
            "    failures:",
            "        runtime_x::flaky",
            "",
            "  stderr ───",
            "    thread 'runtime_x::flaky' (243136103) panicked at tests/it/runtime_x.rs:8:9:",
            "    test runtime_x::flaky timed out: the run to land did not happen within 20s",
            "  TRY 2 PASS [   0.007s] (1/3) dagq::it runtime_x::flaky",
        ];
        if broken {
            lines.extend([
                "  TRY 1 FAIL [   0.006s] (───) dagq::it runtime_x::broken",
                "  TRY 2 FAIL [   0.006s] (2/3) dagq::it runtime_x::broken",
            ]);
        }
        lines.extend([
            "  Cancelling due to test failure: 1 test still running",
            "────────────",
            "     Summary [   0.017s] 3 tests run: 1 passed, 2 failed, 0 skipped",
        ]);
        if broken {
            lines.push("  TRY 2 FAIL [   0.006s] (2/3) dagq::it runtime_x::broken");
        }
        lines.extend([
            " FLKY-FL 2/2 [   0.007s] (1/3) dagq::it runtime_x::flaky",
            "error: test run failed",
            "error: process didn't exit successfully: `cargo nextest run` (exit status: 100)",
        ]);
        lines.join("\n") + "\n"
    }

    #[test]
    fn failed_tests_that_all_passed_on_their_retry_are_flaky() {
        let log = nextest_retried(false);
        let failure = classify(LLVM_COV, Some(1), None, &log);
        assert_eq!(failure.class, FailureClass::Flaky);
        assert_eq!(
            failure.evidence,
            "FLKY-FL 2/2 [   0.007s] (1/3) dagq::it runtime_x::flaky"
        );
        assert_eq!(flaky_tests(&log), ["runtime_x::flaky"]);
        assert_eq!(failed_tests(&log).names, ["runtime_x::flaky"]);
        // Without the summary's status (cut off), the retry's line shows it.
        let cut = log.split("  Cancelling").next().unwrap();
        let failure = classify(LLVM_COV, Some(1), None, cut);
        assert_eq!(failure.class, FailureClass::Flaky);
        assert!(failure.evidence.starts_with("TRY 2 PASS ["), "{failure:?}");
        // `flaky-result = "pass"`'s status names it too.
        assert_eq!(
            flaky_tests("       FLAKY 2/2 [   0.007s] dagq::it a::b\n"),
            ["a::b"]
        );
    }

    #[test]
    fn a_test_that_failed_its_retry_too_is_a_test_failure() {
        let log = nextest_retried(true);
        let failure = classify(LLVM_COV, Some(1), None, &log);
        // `within`'s line belongs to the flaky test, and the broken one
        // names no timeout: the failed tests decide.
        assert_eq!(failure.class, FailureClass::Timeout);
        assert_eq!(flaky_tests(&log), ["runtime_x::flaky"]);
        assert_eq!(
            failed_tests(&log).names,
            ["runtime_x::flaky", "runtime_x::broken"]
        );
        let plain = log.replace(
            "test runtime_x::flaky timed out: the run to land did not happen within 20s",
            "assertion failed",
        );
        assert_eq!(
            class(LLVM_COV, Some(1), None, &plain),
            FailureClass::TestFailure
        );
        // A flaky line alone, without a failed test, or with names cut off.
        assert_eq!(
            class(
                LLVM_COV,
                Some(1),
                None,
                "  TRY 2 PASS [ 0.1s] dagq::it a::b\nboom\n"
            ),
            FailureClass::Unknown
        );
        let many: String = (0..MAX_FAILED_TESTS + 1)
            .map(|i| {
                format!(
                    "  TRY 1 FAIL [ 0.1s] dagq::it t::n{i}\n  TRY 2 PASS [ 0.1s] dagq::it t::n{i}\n"
                )
            })
            .collect();
        assert_eq!(
            class(LLVM_COV, Some(100), None, &many),
            FailureClass::TestFailure
        );
        // Lines that only look like it.
        for line in [
            "TRY x PASS [ 0.1s] dagq::it a::b",
            "FLKY-FL [ 0.1s] dagq::it a::b",
            "  TRY 2 PASS [ 0.1s] a::b",
            "PASS [ 0.1s] dagq::it a::b",
            "TRY 2 PASS 0.1s dagq::it a::b",
            "TRY 2 PASS [ 0.1s dagq::it a::b",
        ] {
            assert!(flaky_tests(line).is_empty(), "{line}");
        }
    }

    /// A nextest status line as its displayer writes it: the status right
    /// aligned to 12 characters, the time, the counter, the binary and the
    /// test.
    fn status_line(status: &str, test: &str) -> String {
        format!("{status:>12} [   1.000s] (  9/100) dagq::it {test}\n")
    }

    #[test]
    fn every_failed_status_of_nextest_names_its_test() {
        use FailureClass::*;
        // nextest-runner 0.124.0's status_str (a first attempt, a final
        // line without retries) and `TRY n` with its short_status_str (a
        // retry, and the attempt that will be retried).
        let cases = [
            ("FAIL", TestFailure),
            ("FAIL + LEAK", TestFailure),
            ("XFAIL", TestFailure),
            ("LEAK-FAIL", TestFailure),
            ("TIMEOUT", Timeout),
            ("ABORT", TestFailure),
            ("SIGKILL", Killed),
            ("SIGTERM", Killed),
            ("SIGSEGV", TestFailure),
            ("SIGABRT", TestFailure),
            ("SIGHUP", TestFailure),
            ("ABORT SIG 64", TestFailure),
            ("TRY 2 FAIL", TestFailure),
            ("TRY 1 FAIL", TestFailure),
            ("TRY 2 FL+LK", TestFailure),
            ("TRY 2 XFAIL", TestFailure),
            ("TRY 2 LKFAIL", TestFailure),
            ("TRY 2 TMT", Timeout),
            ("TRY 2 ABORT", TestFailure),
            ("TRY 2 KILL", Killed),
            ("TRY 1 TERM", Killed),
            ("TRY 1 SEGV", TestFailure),
            ("TRY 2 ABRT", TestFailure),
            ("TRY 2 PIPE", TestFailure),
            ("TRY 2 SIG 64", TestFailure),
            ("TRY 10 LKFAIL", TestFailure),
        ];
        for (status, expected) in cases {
            let line = status_line(status, "runtime_x::broken");
            let log = format!("{line}error: test run failed\n");
            assert_eq!(names(&log), ["runtime_x::broken"], "{status}");
            assert!(flaky_tests(&log).is_empty(), "{status}");
            let failure = classify(LLVM_COV, Some(100), None, &log);
            assert_eq!(failure.class, expected, "{status}");
            assert_eq!(failure.evidence, line.trim(), "{status}");
        }
        // `TRY 2 LKFAIL` fills the 12 characters: no space starts the line.
        assert!(status_line("TRY 2 LKFAIL", "a::b").starts_with("TRY 2 LKFAIL ["));
        // The flaky statuses name the test as flaky, not as failed.
        for status in ["FLKY-FL 2/2", "FLAKY 2/2", "TRY 2 PASS"] {
            let log = status_line(status, "runtime_x::flaky");
            assert_eq!(flaky_tests(&log), ["runtime_x::flaky"], "{status}");
            assert!(names(&log).is_empty(), "{status}");
        }
    }

    #[test]
    fn the_statuses_that_do_not_fail_a_test_name_none() {
        for status in [
            "PASS",
            "LEAK",
            "TIMEOUT-PASS",
            "SLOW",
            "SLOW + LEAK",
            "SLOW+TMPASS",
            "TMPASS",
            "TRY 2 SLOW",
            "START",
            "TRY 2 START",
            "TERMINATING",
            "TRY 1 TRMNTG",
            "DELAY 2/2",
            "SKIP",
            "SETUP",
            "TRY 2 LEAK",
            "TRY 2 TMPASS",
            // Not nextest's: a signal it has no name for, a bad count.
            "SIGUSR1",
            "SIG",
            "TRY x FAIL",
            "TRY 2 SIG x",
            "ABORT SIG",
            "FLKY-FL 2/x",
            "FAIL + SLOW",
        ] {
            let log = status_line(status, "runtime_x::fine");
            assert!(failed_tests(&log).is_empty(), "{status}");
            assert!(flaky_tests(&log).is_empty(), "{status}");
            assert_eq!(
                class(LLVM_COV, Some(1), None, &log),
                FailureClass::Unknown,
                "{status}"
            );
        }
    }

    #[test]
    fn a_failure_in_a_short_form_beside_a_flaky_test_is_not_all_flaky() {
        // `runtime_x::flaky` failed and passed on its retry; the other test
        // failed both times, with a leak or killed. Before task 1272 the
        // short forms named no test, so the flaky test alone decided.
        for (first, last, expected) in [
            ("TRY 1 FL+LK", "TRY 2 FL+LK", FailureClass::TestFailure),
            ("TRY 1 KILL", "TRY 2 KILL", FailureClass::Killed),
            ("TRY 1 SIG 64", "TRY 2 SIG 64", FailureClass::TestFailure),
        ] {
            let log = [
                status_line("TRY 1 FAIL", "runtime_x::flaky"),
                status_line("TRY 2 PASS", "runtime_x::flaky"),
                status_line(first, "runtime_repair::leaks"),
                status_line(last, "runtime_repair::leaks"),
                "     Summary [   0.017s] 2 tests run: 0 passed, 2 failed, 0 skipped\n".to_owned(),
                status_line(last, "runtime_repair::leaks"),
                status_line("FLKY-FL 2/2", "runtime_x::flaky"),
                "error: test run failed\n".to_owned(),
            ]
            .concat();
            assert_eq!(all_flaky(&log), None, "{last}");
            assert_eq!(
                names(&log),
                ["runtime_x::flaky", "runtime_repair::leaks"],
                "{last}"
            );
            assert_eq!(flaky_tests(&log), ["runtime_x::flaky"], "{last}");
            assert_eq!(class(LLVM_COV, Some(100), None, &log), expected, "{last}");
        }
        // Killed, timed out or failed with a leak once and passed on the
        // retry: flaky, not a kill or a timeout.
        for first in [
            "TRY 1 KILL",
            "TRY 1 TERM",
            "TRY 1 TMT",
            "TRY 1 FL+LK",
            "TRY 1 SIG 64",
        ] {
            let log = [
                status_line(first, "runtime_x::flaky"),
                status_line("TRY 2 PASS", "runtime_x::flaky"),
                status_line("FLKY-FL 2/2", "runtime_x::flaky"),
                "error: test run failed\n".to_owned(),
            ]
            .concat();
            let failure = classify(LLVM_COV, Some(100), None, &log);
            assert_eq!(failure.class, FailureClass::Flaky, "{first}");
            assert!(failure.evidence.starts_with("FLKY-FL 2/2 ["), "{first}");
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
