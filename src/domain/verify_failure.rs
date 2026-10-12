//! Why a verification command of `integrate` failed after a clean rebase
//! (task 467, goal 37): one reading of its exit and its log, so the event
//! says whether the code broke (a build error, a failing test, a lint, the
//! format, the coverage) or the host did (the disk filled up, the command
//! was killed, a test ran out of time under load) without opening the log.
//!
//! The runtime's own marks, which hold for any language and work whatever
//! the repository declares (ADR-t2268-1 decision 4), are here: a full
//! disk's error and errno, a signal and the shell's exit of 128 + N, the
//! conventional exit 124 of `timeout(1)`, and the limit of the whole
//! command ([`CommandTimedOut`]). The marks of a language's tools and the
//! tests they name are the declared profiles'
//! ([`crate::domain::toolchain`]); an output none of them reads is
//! `unknown` unless the runtime's marks say otherwise.
//!
//! How a class is handled is [`FailureClass::is_environmental`]'s:
//! integrate retries a command that failed on the host once instead of
//! resuming the worker (task 639), and lands a run whose failed tests all
//! passed when the tool ran them again ([`FailureClass::Flaky`]) once more
//! (task 768).
use serde::Serialize;
use serde_json::{Value, json};

use super::toolchain::{ToolHostClass, Toolchains};

/// What a failed verification command is put down to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// The disk filled up (`No space left on device`, `os error 28`).
    DiskFull,
    /// A signal ended the command or one of its processes (from outside,
    /// or the shell's exit 128 + N).
    Killed,
    /// A test ran out of its time, the command exited 124 (`timeout(1)`),
    /// or it ran past its limit for the whole command.
    Timeout,
    /// Every test that failed passed when the tool ran it again (task 768,
    /// ADR-t768-1).
    Flaky,
    /// The code did not compile.
    BuildError,
    /// A linter denied a lint.
    Lint,
    /// A test failed.
    TestFailure,
    /// The formatter's check found a diff.
    Format,
    /// The line coverage fell under the command's threshold.
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
    /// is not widened without a person's decision; a profile only adds
    /// marks to its classes ([`crate::domain::toolchain::ToolHostClass`]).
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
            profile: None,
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
    /// [`EVIDENCE_CHARS`] (a profile may add where it points); for a kill
    /// without a mark in the log, the exit; for `unknown` the log's last
    /// line.
    pub evidence: String,
    /// The profile whose marks gave the class; `None` for the runtime's
    /// own marks and for `unknown`. The flaky retry adds the environment of
    /// this profile only (ADR-t2268-1 decision 6).
    pub profile: Option<&'static str>,
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
/// when a signal ended it) or `signal`, from its `log`, with the runtime's
/// marks and those of `toolchains`' profiles, each declared one in turn
/// at each step (ADR-t2268-1 decision 6). The steps are tried in order:
/// what the host did first, since a full disk or a kill also leaves build
/// and test errors behind; the tests that passed on their retry before a
/// test out of time, so a test that ran out of time once and passed is
/// flaky as any other; the exit 124 after the tools' own timeouts; the
/// code's classes last.
pub fn classify(
    toolchains: &Toolchains,
    command: &str,
    exit_code: Option<i32>,
    signal: Option<i32>,
    log: &str,
) -> VerifyFailure {
    let text = strip_ansi(log);
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let found = |class, line: &str, profile| VerifyFailure {
        class,
        evidence: shorten(line),
        profile,
    };

    if let Some(at) = lines.iter().position(|line| {
        line.contains("No space left on device")
            || line.contains("os error 28")
            || line.contains("ENOSPC")
    }) {
        return found(FailureClass::DiskFull, lines[at], None);
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
        return found(FailureClass::Killed, &evidence, None);
    }
    for profile in toolchains.profiles() {
        if let Some(at) = profile.killed_line(&lines) {
            return found(FailureClass::Killed, lines[at], Some(profile.name()));
        }
    }
    let failed = failed_tests(toolchains, &text);
    for profile in toolchains.profiles() {
        let flaky = profile.flaky_tests(&text);
        if !flaky.is_empty()
            && !failed.names.is_empty()
            && failed.omitted == 0
            && failed.names.iter().all(|name| flaky.contains(name))
            && let Some(at) = profile.flaky_line(&lines)
        {
            return found(FailureClass::Flaky, lines[at], Some(profile.name()));
        }
    }
    // A tool's kill first; a tool's timeout and the project's own
    // timeout line by the first line either is on.
    let within = lines.iter().position(|line| within_timeout(line).is_some());
    for profile in toolchains.profiles() {
        match profile.test_host_line(&lines) {
            Some((ToolHostClass::Timeout, at)) if within.is_some_and(|w| w < at) => break,
            Some((class, at)) => return found(class.into(), lines[at], Some(profile.name())),
            None => {}
        }
    }
    if let Some(at) = within {
        return found(FailureClass::Timeout, lines[at], None);
    }
    if exit_code == Some(124) {
        return found(FailureClass::Timeout, "exit 124 (timeout)", None);
    }
    for profile in toolchains.profiles() {
        if let Some((class, evidence)) = profile.code_failure(command, &lines) {
            return found(class.into(), &evidence, Some(profile.name()));
        }
    }
    match lines.iter().rev().find(|line| !line.is_empty()) {
        Some(line) => found(FailureClass::Unknown, line, None),
        None => found(
            FailureClass::Unknown,
            &match exit_code {
                Some(code) => format!("exit {code} with an empty log"),
                None => "an empty log".to_owned(),
            },
            None,
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

/// The tests `log` names as failed, by `toolchains`' profiles and by the
/// timeout line of the repository's own test helper ([`within_timeout`]),
/// in the order of the lines that name them. A failure that names no test
/// gives none, and so does an output no profile reads.
pub fn failed_tests(toolchains: &Toolchains, log: &str) -> FailedTests {
    let text = strip_ansi(log);
    let mut found: Vec<(usize, String)> = toolchains
        .profiles()
        .flat_map(|profile| profile.failed_tests(&text))
        .chain(within_tests(&text))
        .collect();
    // Stable: a line two profiles name keeps the first's.
    found.sort_by_key(|(index, _)| *index);
    let mut names: Vec<String> = Vec::new();
    for (_, name) in found {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let omitted = names.len().saturating_sub(MAX_FAILED_TESTS);
    names.truncate(MAX_FAILED_TESTS);
    FailedTests { names, omitted }
}

/// The tests `log` names as passed when the tool ran them again after a
/// failure (task 768), by `toolchains`' profiles, each once, in the order
/// of the profiles and then of the lines.
pub fn flaky_tests(toolchains: &Toolchains, log: &str) -> Vec<String> {
    let text = strip_ansi(log);
    let mut names: Vec<String> = Vec::new();
    for name in toolchains
        .profiles()
        .flat_map(|profile| profile.flaky_tests(&text))
    {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// The test of the timeout line of this repository's own test helper
/// (`tests/common`'s `within`: `test <name> timed out: … did not happen
/// within …`, starting its own line). It is a project's mark, not a
/// language's: the runtime reads it whatever the repository declares
/// until it moves to the project's own marks in `dagq.toml`
/// (ADR-t2268-1 decision 2).
fn within_timeout(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("test ")?;
    rest.contains("did not happen within")
        .then(|| rest.split_once(" timed out: "))
        .flatten()
        .map(|(name, _)| name)
}

/// The tests of the project's own timeout lines ([`within_timeout`]) with
/// their lines, but those of the indented list under a `failures:` line,
/// which names tests rather than says why they failed.
fn within_tests(text: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let mut in_list = false;
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if in_list {
            if !line.is_empty() && raw.starts_with([' ', '\t']) {
                continue;
            }
            in_list = false;
        }
        if raw.trim_end() == "failures:" {
            in_list = true;
            continue;
        }
        found.extend(
            within_timeout(line)
                .and_then(test_name)
                .map(|name| (index, name)),
        );
    }
    found
}

/// `text` when it looks like a test's name: letters, digits, `_` and `:`
/// (a path of modules).
pub(crate) fn test_name(text: &str) -> Option<String> {
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
    use crate::domain::toolchain::{BUILTIN, ToolchainDeclaration, fake};

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
                profile: None,
            }
        );
    }

    /// The repositories the runtime's own marks must hold for: one that
    /// declares nothing (the Rust profile implied), one that declares
    /// Rust, and one that declares only a profile of another tool.
    fn every_declaration() -> [(&'static str, Toolchains); 3] {
        let rust = Toolchains::resolve(
            &BUILTIN,
            &[ToolchainDeclaration {
                name: "rust".to_owned(),
                profile: "rust".to_owned(),
                paths: Vec::new(),
                line: 1,
            }],
        )
        .unwrap();
        [
            ("undeclared", Toolchains::undeclared()),
            ("rust", rust),
            ("fake", fake::only_fake()),
        ]
    }

    #[test]
    fn the_runtimes_host_marks_hold_whatever_is_declared() {
        for (declared, toolchains) in every_declaration() {
            let class = |exit, signal, log: &str| {
                let failure = classify(&toolchains, "make check", exit, signal, log);
                assert_eq!(failure.profile, None, "{declared}: {log}");
                failure.class
            };
            for log in [
                "write failed: No space left on device\n",
                "write failed: os error 28\n",
                "cc: ENOSPC while writing\n",
            ] {
                assert_eq!(
                    class(Some(1), None, log),
                    FailureClass::DiskFull,
                    "{declared}"
                );
            }
            // Even when the build was killed on the way.
            assert_eq!(
                class(Some(143), None, "ENOSPC\n"),
                FailureClass::DiskFull,
                "{declared}"
            );
            assert_eq!(class(None, Some(9), "boom\n"), FailureClass::Killed);
            let failure = classify(&toolchains, "make", Some(137), None, "");
            assert_eq!(failure.class, FailureClass::Killed, "{declared}");
            assert_eq!(failure.evidence, "exit 137 (signal 9, SIGKILL)");
            let failure = classify(&toolchains, "timeout 60 make", Some(124), None, "boom\n");
            assert_eq!(failure.class, FailureClass::Timeout, "{declared}");
            assert_eq!(failure.evidence, "exit 124 (timeout)");
            // The limit of the whole command is the verifier's, not a mark.
            assert_eq!(
                CommandTimedOut { limit_secs: 5 }.failure().class,
                FailureClass::Timeout
            );
            assert_eq!(
                class(Some(2), None, "make: *** [all] Error 2\n"),
                FailureClass::Unknown,
                "{declared}"
            );
        }
    }

    #[test]
    fn an_output_no_declared_profile_reads_is_unknown_and_names_no_test() {
        let only_fake = fake::only_fake();
        // cargo's and nextest's marks mean nothing to a repository that
        // declares only another tool's profile.
        for log in [
            "error[E0063]: missing field\nerror: could not compile `dagq`\n",
            "        FAIL [   4.935s] (1/2) dagq::it a::b\nerror: test run failed\n",
            "  TRY 1 FAIL [ 0.1s] dagq::it a::b\n  TRY 2 PASS [ 0.1s] dagq::it a::b\n",
            "Diff in /w/src/a.rs:12:\n",
            "Caused by:\n  process didn't exit successfully: `rustc` (signal: 9, SIGKILL: kill)\n",
            "     TIMEOUT [ 600.003s] dagq::it a::b\n",
        ] {
            let failure = classify(
                &only_fake,
                "cargo test --fail-under-lines 80",
                Some(101),
                None,
                log,
            );
            assert_eq!(failure.class, FailureClass::Unknown, "{log}");
            assert_eq!(failure.profile, None, "{log}");
            assert!(failed_tests(&only_fake, log).is_empty(), "{log}");
            assert!(flaky_tests(&only_fake, log).is_empty(), "{log}");
        }
        // The same output under the Rust profile is read.
        assert_eq!(
            classify(
                &Toolchains::undeclared(),
                "cargo build",
                Some(101),
                None,
                "error[E0063]: missing field\n"
            )
            .profile,
            Some("rust")
        );
    }

    #[test]
    fn a_second_profile_classifies_names_its_tests_and_gives_its_retry_env() {
        let only_fake = fake::only_fake();
        let build = classify(&only_fake, "fake build", Some(1), None, "x\nNO BUILD\n");
        assert_eq!(
            (build.class, build.profile),
            (FailureClass::BuildError, Some("fake"))
        );
        let log = "BROKEN a::one\nok a::two\nBROKEN b::three\n";
        let failure = classify(&only_fake, "fake test", Some(1), None, log);
        assert_eq!(failure.class, FailureClass::TestFailure);
        assert_eq!(failure.evidence, "BROKEN a::one");
        assert_eq!(failed_tests(&only_fake, log).names, ["a::one", "b::three"]);
        // Out of time: a host class the runtime has, retried as any other.
        let hung = classify(&only_fake, "fake test", Some(1), None, "HUNG a::slow\n");
        assert_eq!(hung.class, FailureClass::Timeout);
        assert!(hung.class.is_environmental());
        // Every failed test passed on its retry: flaky, with the fake's env.
        let log = "BROKEN a::one\nWOBBLY a::one\n";
        let flaky = classify(&only_fake, "fake test", Some(1), None, log);
        assert_eq!(
            (flaky.class, flaky.profile, flaky.evidence.as_str()),
            (FailureClass::Flaky, Some("fake"), "WOBBLY a::one")
        );
        assert_eq!(flaky_tests(&only_fake, log), ["a::one"]);
        assert_eq!(
            only_fake.flaky_retry_env("fake"),
            [("FAKE_FLAKY".to_owned(), "accept".to_owned())]
        );
        assert!(only_fake.flaky_retry_env("rust").is_empty());
        // One that failed its retry too is the code's.
        let log = "BROKEN a::one\nWOBBLY a::one\nBROKEN b::two\n";
        assert_eq!(
            classify(&only_fake, "fake test", Some(1), None, log).class,
            FailureClass::TestFailure
        );
    }

    #[test]
    fn several_profiles_are_tried_in_the_order_declared() {
        let both = |first: &str, second: &str| {
            Toolchains::resolve(
                &fake::REGISTRY,
                &[first, second].map(|profile| ToolchainDeclaration {
                    name: format!("{profile}-part"),
                    profile: profile.to_owned(),
                    paths: vec![format!("{profile}/**")],
                    line: 1,
                }),
            )
            .unwrap()
        };
        // Each profile reads its own tool's lines, whatever the paths.
        let log = "test a::b ... FAILED\nBROKEN c::d\n";
        let rust_first = both("rust", "fake");
        let failure = classify(&rust_first, "check", Some(1), None, log);
        assert_eq!(failure.profile, Some("rust"));
        assert_eq!(failed_tests(&rust_first, log).names, ["a::b", "c::d"]);
        let fake_first = both("fake", "rust");
        assert_eq!(
            classify(&fake_first, "check", Some(1), None, log).profile,
            Some("fake")
        );
        // The flaky retry adds the env of the profile that read it flaky.
        let log = "BROKEN c::d\nWOBBLY c::d\n";
        let failure = classify(&rust_first, "check", Some(1), None, log);
        assert_eq!(failure.class, FailureClass::Flaky);
        assert_eq!(
            rust_first.flaky_retry_env(failure.profile.unwrap()),
            [("FAKE_FLAKY".to_owned(), "accept".to_owned())]
        );
    }

    #[test]
    fn the_projects_timeout_line_is_read_whatever_is_declared() {
        let log = "test runtime_x::waits timed out: the run to land did not happen within 20s\n";
        for (declared, toolchains) in every_declaration() {
            let failure = classify(&toolchains, "check", Some(101), None, log);
            assert_eq!(failure.class, FailureClass::Timeout, "{declared}");
            assert_eq!(failed_tests(&toolchains, log).names, ["runtime_x::waits"]);
        }
        // The quoted text of an assertion is not a mark.
        let quoted = "stderr: \"test a::b timed out: x did not happen within 1s\"\n";
        assert!(failed_tests(&fake::only_fake(), quoted).is_empty());
    }

    #[test]
    fn the_first_timeout_line_is_the_evidence_and_a_listed_test_is_no_mark() {
        let toolchains = Toolchains::undeclared();
        let log = "        FAIL [ 1.0s] dagq::it it::a\n\
    test it::a timed out: x did not happen within 30s\n\
     TIMEOUT [ 60.0s] dagq::it it::b\n";
        let failure = classify(&toolchains, "check", Some(100), None, log);
        assert_eq!(failure.class, FailureClass::Timeout);
        assert_eq!(
            failure.evidence,
            "test it::a timed out: x did not happen within 30s"
        );
        let log = "     TIMEOUT [ 60.0s] dagq::it it::b\n\
test it::a timed out: x did not happen within 30s\n";
        let failure = classify(&toolchains, "check", Some(100), None, log);
        assert_eq!(failure.evidence, "TIMEOUT [ 60.0s] dagq::it it::b");
        assert_eq!(failure.profile, Some("rust"));
        // Under `failures:`, an indented line names a test, not a timeout.
        let listed = "failures:\n    test x::y timed out: z did not happen within 1s\n";
        assert!(failed_tests(&fake::only_fake(), listed).is_empty());
    }

    #[test]
    fn colors_are_taken_off_before_the_marks() {
        assert_eq!(strip_ansi("a\u{1b}b"), "a");
        assert_eq!(strip_ansi("\u{1b}[31mBROKEN a::b\u{1b}[0m"), "BROKEN a::b");
        assert_eq!(
            failed_tests(&fake::only_fake(), "\u{1b}[31mBROKEN a::b\u{1b}[0m\n").names,
            ["a::b"]
        );
    }
}
