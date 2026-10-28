//! The Rust profile (ADR-t2268-1): how cargo, cargo test, cargo-nextest,
//! clippy, rustfmt and cargo-llvm-cov write a failure.
//!
//! The marks, tried in the order of [`crate::domain::verify_failure::classify`]:
//!
//! - `killed`, before the flaky tests: cargo's `process didn't exit
//!   successfully: … (signal: 9, SIGKILL` / `(signal: 15, SIGTERM` on a
//!   process it ran, and nextest's `SIGKILL [` / `SIGTERM [`.
//! - `flaky`: every failed test is one nextest names as passed on its
//!   retry (below); the line is the summary's `FLKY-FL n/m [` / `FLAKY
//!   n/m [`, else the retry's `TRY n PASS [`.
//! - `killed` and `timeout` after the flaky tests (a test killed or out of
//!   time once and passed on its retry is flaky): nextest's `TRY n KILL [`
//!   / `TRY n TERM [`, then its `TIMEOUT [` / `TRY n TMT [`.
//! - `build_error`: `error[E…]` with the `-->` location that follows it,
//!   else `error: could not compile` / `error: linking with`.
//! - `lint`: a clippy command's `could not compile`, named by the lint's
//!   own first `error: ` line.
//! - `test_failure`: nextest's failed statuses (below) but a kill and a
//!   timeout, then cargo test's `test … ... FAILED`, `test result:
//!   FAILED`, `error: test failed` and `error: test run failed`.
//! - `format`: rustfmt's `Diff in `.
//! - `coverage_below`: a command with `--fail-under-` and cargo-llvm-cov's
//!   `TOTAL ` line (it exits 1 without saying it fell under).
//!
//! The failed tests are named by cargo test's `test <name> ... FAILED`,
//! `---- <name> stdout ----` (or `stderr`), the indented list under
//! `failures:` and a test thread's `thread '<name>' panicked` (a name with
//! `::`), and by nextest's failed statuses, whose test is the last word of
//! the line.
//!
//! nextest's statuses (0.9.146, nextest-runner 0.124.0's `status_str` and
//! `short_status_str`), right aligned to 12 characters before ` [`, the
//! time, the binary and the test; [`nextest_status`] holds the set:
//!
//! - (A) a first attempt, and the last line without retries: `FAIL`,
//!   `FAIL + LEAK`, `XFAIL`, `LEAK-FAIL`, `TIMEOUT`, `ABORT`, `SIG<name>`
//!   of the 12 signals nextest names ([`NEXTEST_SIGNALS`]) and `ABORT SIG
//!   <n>` of the others.
//! - (B) under `retries`, `TRY n` and the short form: `FAIL`, `FL+LK`,
//!   `XFAIL`, `LKFAIL`, `TMT`, `ABORT`, a signal's bare name, `SIG <n>`.
//! - (C) passed on a retry: `FLKY-FL n/m` (`flaky-result = "fail"`),
//!   `FLAKY n/m`, and the retry's `TRY n PASS`.
//!
//! Any other status fails no test (`PASS`, `LEAK`, `TIMEOUT-PASS`, `SLOW`,
//! `TMPASS`, `TRY n SLOW`, `START`, `TERMINATING`, `DELAY n/m`, `SKIP`,
//! `SETUP`, …). The kills are `SIGKILL`, `SIGTERM`, `TRY n KILL` and `TRY n
//! TERM`; the timeouts `TIMEOUT` and `TRY n TMT`. The same set is matched
//! by `failed_status` of `scripts/stress-recent-tests.sh` and the "Failed
//! tests" step of the Linux job of `.github/workflows/ci.yml` ((A), (B) and
//! (C)'s `FLKY-FL` / `FLAKY`, one regular expression in both) and by
//! `scripts/slow-tests.sh` ((A) and (B)): when a new nextest changes its
//! statuses, the four are changed together.
//!
//! The flaky retry ([`RUST`]'s [`ToolchainProfile::flaky_retry_env`]):
//! `.config/nextest.toml` runs a failed test once more and, with
//! `flaky-result = "fail"`, still fails a run whose tests passed on their
//! retry, so the first verification stays a gate. The retry the landing
//! does when only flaky tests failed passes `NEXTEST_FLAKY_RESULT=pass`
//! (over `[run.env]`'s value of it), under which nextest passes such a
//! run. A verification command never passes on a `NEXTEST_FLAKY_RESULT`
//! its process inherited, only the one the runtime gives: nextest hands
//! the retry's `pass` to every test it runs, so a test that runs
//! `integrate` in its process inside a flaky retry would otherwise see
//! `pass` on its first verification, and a supervisor started with the
//! variable set would turn the first gate off.
//!
//! The CI watch names a test case of nextest's JUnit by its `classname`
//! (the test binary, `dagq::it`) and `name` joined by one space, as
//! nextest prints the test, and by `name` alone without a `classname`.
//!
//! The host's load counts as the compiler `rustc`, `clippy-driver` and
//! the `sccache` that wraps them, and as the build tool `cargo`, its
//! subcommands `cargo-*` and what runs from a `target/` directory (build
//! scripts and test binaries).
//!
//! The host's toolchain a claim records is `release` and `host` of
//! [`RUSTC_VERSION`] ([`rustc_version`]), run in the main checkout so
//! that its toolchain file applies.
//!
//! The commands' classification ([`cargo_rules`]) is cargo's alone: which
//! subcommand builds, tests, lints or formats, and which `cargo test` runs
//! the whole suite.
//!
//! The build output ([`BUILD_OUTPUT`]): cargo builds into the directory
//! `CARGO_TARGET_DIR` names, `target` under the runtime's directory of
//! the command (`<queue dir>/recheck/target` for the landing recheck,
//! `<queue dir>/update/target` for the automatic update's build).
//! What of a worktree's target no build reads any more is the sessions of
//! rustc's incremental compilation ([`incremental`]).
//!
//! The lines in a test ([`ToolchainProfile::test_lines`]) are those of the
//! items a test's attribute is put on ([`test_lines`]'s doc) in a `.rs`
//! file. A file under a directory named `tests` (cargo builds each as an
//! integration test) or named `tests.rs` (the usual file of a
//! `#[cfg(test)] mod tests;` declared in another) is not told apart, nor
//! is a file of another language. A file of a module declared under a
//! test's attribute in another file by another name (a test helper or
//! fake) is read alone: only the items under a test's attribute in it are
//! a test's, so a change of its helpers alone changes no test function.
use super::{
    BuildOutput, JunitTestName, Recommendation, RecommendedHost, RecommendedImage,
    RecommendedProgram, StaleEntries, TestLines, ToolCodeClass, ToolHostClass, ToolProcess,
    ToolchainProfile,
};
use crate::domain::command_rules::CommandRule;
use crate::domain::command_shape::{Runs, SubcommandList, VariableList};
use crate::domain::verify_failure::test_name;

mod incremental;
mod test_lines;

/// cargo's target directory: see the module's doc.
pub const BUILD_OUTPUT: BuildOutput = BuildOutput {
    dir: "target",
    env: "CARGO_TARGET_DIR",
};

/// The directories directly under a run's worktree cargo builds into:
/// `target`, and `llvm-cov-target`, where `cargo llvm-cov` builds when told
/// to build beside `target` rather than under it.
pub const WORKTREE_BUILD_DIRS: &[&str] = &["target", "llvm-cov-target"];

/// The Rust profile, `profile = "rust"`.
pub static RUST: Rust = Rust;

/// See the module's doc.
pub struct Rust;

/// The known subcommands of cargo, its tools' (nextest, llvm-cov) and
/// rustup's, and the options of theirs a shape keeps by name
/// ([`ToolchainProfile::subcommands`]).
const SUBCOMMANDS: &[SubcommandList] = &[
    SubcommandList {
        programs: &["cargo"],
        subcommands: &[
            "add",
            "archive",
            "audit",
            "bench",
            "build",
            "check",
            "clean",
            "clippy",
            "deny",
            "doc",
            "fetch",
            "fix",
            "fmt",
            "generate-lockfile",
            "help",
            "init",
            "install",
            "list",
            "llvm-cov",
            "locate-project",
            "machete",
            "metadata",
            "new",
            "nextest",
            "package",
            "publish",
            "remove",
            "report",
            "run",
            "rustc",
            "rustdoc",
            "search",
            "show-env",
            "test",
            "tree",
            "uninstall",
            "update",
            "vendor",
            "version",
        ],
        switches: &[
            "-h",
            "-q",
            "-r",
            "-v",
            "-V",
            "--all",
            "--all-features",
            "--all-targets",
            "--allow-dirty",
            "--allow-staged",
            "--benches",
            "--bins",
            "--check",
            "--doc",
            "--exact",
            "--examples",
            "--fail-fast",
            "--fix",
            "--frozen",
            "--future-incompat-report",
            "--help",
            "--html",
            "--ignore-rust-version",
            "--ignored",
            "--include-ignored",
            "--json",
            "--keep-going",
            "--lcov",
            "--lib",
            "--locked",
            "--no-capture",
            "--no-clean",
            "--no-default-features",
            "--no-deps",
            "--no-fail-fast",
            "--no-report",
            "--no-run",
            "--nocapture",
            "--offline",
            "--open",
            "--quiet",
            "--release",
            "--summary-only",
            "--tests",
            "--timings",
            "--verbose",
            "--version",
            "--workspace",
        ],
        options: &[
            "-E",
            "-F",
            "-j",
            "-p",
            "-Z",
            "--bench",
            "--bin",
            "--cargo-profile",
            "--color",
            "--config",
            "--example",
            "--exclude",
            "--fail-under-lines",
            "--features",
            "--ignore-filename-regex",
            "--jobs",
            "--manifest-path",
            "--message-format",
            "--output-path",
            "--package",
            "--partition",
            "--profile",
            "--retries",
            "--stress-count",
            "--stress-duration",
            "--target",
            "--target-dir",
            "--test",
            "--test-threads",
        ],
        // `cargo run`'s (and its alias `r`'s) words from the first that is
        // no flag are the binary's.
        runs: Runs::After(&["run", "r"]),
        // clap's: an option takes one word, before or after operands.
        options_after_operands: true,
        // `+TOOLCHAIN` as its first word.
        toolchain: true,
    },
    SubcommandList {
        programs: &["rustup"],
        subcommands: &[
            "add",
            "component",
            "default",
            "install",
            "list",
            "override",
            "remove",
            "run",
            "show",
            "target",
            "toolchain",
            "update",
            "which",
        ],
        switches: &[
            "-h",
            "-q",
            "-v",
            "-V",
            "--help",
            "--quiet",
            "--verbose",
            "--version",
        ],
        options: &["--component", "--profile", "--target", "--toolchain"],
        // `rustup run TOOLCHAIN CMD …`.
        runs: Runs::After(&["run"]),
        // clap's: an option takes one word, before or after operands.
        options_after_operands: true,
        // `+TOOLCHAIN` as its first word.
        toolchain: true,
    },
];

/// The variables of cargo, rustc, rustup and nextest whose names a shape
/// keeps in plain ([`ToolchainProfile::variables`]): the runtime's and the
/// tests' (`RUST_LOG`, `RUST_BACKTRACE` …), the compiler's flags and
/// wrappers (`RUSTFLAGS`, `RUSTC_WRAPPER` …), the coverage's profile file,
/// and every `CARGO_*`, `RUSTUP_*` and `NEXTEST_*`.
const VARIABLES: VariableList = VariableList {
    names: &[
        "LLVM_PROFILE_FILE",
        "RUSTC",
        "RUSTC_WRAPPER",
        "RUSTDOC",
        "RUSTDOCFLAGS",
        "RUSTFLAGS",
        "RUST_BACKTRACE",
        "RUST_LIB_BACKTRACE",
        "RUST_LOG",
        "RUST_MIN_STACK",
        "RUST_TEST_NOCAPTURE",
        "RUST_TEST_THREADS",
    ],
    prefixes: &["CARGO_", "NEXTEST_", "RUSTUP_"],
};

/// The flags of `cargo test` and `cargo nextest run` that choose a target
/// or a package or filter the tests: a command with one of them, or with a
/// word that is no flag before `--` (a filter, an option's value), runs
/// part of the suite, not the whole.
const NARROWING: &[&str] = &[
    "--test",
    "--lib",
    "--bin",
    "--bins",
    "--doc",
    "--example",
    "--package",
    "-p",
    "-E",
];

/// cargo's rules of the commands' classification
/// ([`ToolchainProfile::command_rules`]), tried in this order: `cargo
/// llvm-cov` is `llvm_cov`; `cargo test` and `cargo nextest run` with no
/// target, package or filter ([`NARROWING`]) and no other word before
/// `--` are `full_test` (counted in the group `test`), any other `cargo
/// test` or `cargo nextest` is `test`; `cargo build`, `clippy`, `check`
/// and `run` are `build`, `cargo fmt` is `fmt`. A project's test targets
/// (which of them is its e2e, which runs nearly its whole suite) are its
/// own rules in its `dagq.toml`, tried before these.
pub fn cargo_rules() -> Vec<CommandRule> {
    let mut rules = vec![
        CommandRule::new("llvm_cov", "cargo").subcommands(&["llvm-cov"]),
        CommandRule::new("full_test", "cargo")
            .group("test")
            .subcommands(&["test"])
            .none_of(NARROWING)
            .positional_none(),
        CommandRule::new("full_test", "cargo")
            .group("test")
            .subcommands(&["nextest", "run"])
            .none_of(NARROWING)
            .positional_none(),
        CommandRule::new("test", "cargo").subcommands(&["test"]),
        CommandRule::new("test", "cargo").subcommands(&["nextest"]),
    ];
    rules.extend(
        ["build", "clippy", "check", "run"]
            .into_iter()
            .map(|sub| CommandRule::new("build", "cargo").subcommands(&[sub])),
    );
    rules.push(CommandRule::new("fmt", "cargo").subcommands(&["fmt"]));
    rules
}

impl ToolchainProfile for Rust {
    fn name(&self) -> &'static str {
        "rust"
    }

    fn subcommands(&self) -> &'static [SubcommandList] {
        SUBCOMMANDS
    }

    fn variables(&self) -> VariableList {
        VARIABLES
    }

    fn command_rules(&self) -> Vec<CommandRule> {
        cargo_rules()
    }

    fn killed_line(&self, lines: &[&str]) -> Option<usize> {
        lines.iter().position(|line| {
            (line.contains("process didn't exit successfully")
                && (line.contains("(signal: 9, SIGKILL") || line.contains("(signal: 15, SIGTERM")))
                || line.starts_with("SIGKILL [")
                || line.starts_with("SIGTERM [")
        })
    }

    fn failed_tests(&self, text: &str) -> Vec<(usize, String)> {
        let mut found = Vec::new();
        let mut in_list = false;
        for (index, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if in_list {
                // `failures:` then one indented name per line, to a blank line.
                if !line.is_empty() && raw.starts_with([' ', '\t']) {
                    found.extend(test_name(line).map(|name| (index, name)));
                    continue;
                }
                in_list = false;
            }
            if raw.trim_end() == "failures:" {
                in_list = true;
                continue;
            }
            let name = if let Some(rest) = line.strip_prefix("test ") {
                rest.strip_suffix(" ... FAILED")
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
            found.extend(name.and_then(test_name).map(|name| (index, name)));
        }
        found
    }

    fn flaky_tests(&self, text: &str) -> Vec<String> {
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

    fn flaky_line(&self, lines: &[&str]) -> Option<usize> {
        lines
            .iter()
            .position(|line| line.starts_with("FLKY-FL ") || line.starts_with("FLAKY "))
            .or_else(|| lines.iter().position(|line| flaky_line(line).is_some()))
    }

    fn test_host_line(&self, lines: &[&str]) -> Option<(ToolHostClass, usize)> {
        let first = |status| {
            lines
                .iter()
                .position(|line| nextest_line(line).is_some_and(|(found, _)| found == status))
        };
        first(NextestStatus::Killed)
            .map(|at| (ToolHostClass::Killed, at))
            .or_else(|| {
                lines
                    .iter()
                    .position(|line| {
                        line.starts_with("TIMEOUT [")
                            || nextest_line(line)
                                .is_some_and(|(found, _)| found == NextestStatus::TimedOut)
                    })
                    .map(|at| (ToolHostClass::Timeout, at))
            })
    }

    fn code_failure(&self, command: &str, lines: &[&str]) -> Option<(ToolCodeClass, String)> {
        let first = |mark: &dyn Fn(&str) -> bool| lines.iter().position(|line| mark(line));
        if let Some(at) = first(&|line| line.starts_with("error[E")) {
            let location = lines[at + 1..]
                .iter()
                .take(5)
                .find(|line| line.starts_with("-->"));
            return Some((
                ToolCodeClass::BuildError,
                match location {
                    Some(location) => format!("{} {location}", lines[at]),
                    None => lines[at].to_owned(),
                },
            ));
        }
        if let Some(at) = first(&|line| {
            line.starts_with("error: could not compile") || line.starts_with("error: linking with")
        }) {
            if command.contains("clippy") && lines[at].starts_with("error: could not compile") {
                // The lint itself, not cargo's summary of it.
                let lint = first(&|line| {
                    line.starts_with("error: ") && !line.starts_with("error: could not compile")
                });
                return Some((ToolCodeClass::Lint, lines[lint.unwrap_or(at)].to_owned()));
            }
            return Some((ToolCodeClass::BuildError, lines[at].to_owned()));
        }
        if let Some(at) = first(&|line| {
            // nextest's failures but a kill and a timeout (tried before),
            // also after `TRY n` under its retries.
            line.starts_with("FAIL [")
                || nextest_line(line).is_some_and(|(found, _)| found == NextestStatus::Failed)
        })
        .or_else(|| {
            first(&|line| {
                (line.starts_with("test ") && line.ends_with(" ... FAILED"))
                    || line.starts_with("test result: FAILED")
                    || line.starts_with("error: test failed")
                    || line.starts_with("error: test run failed")
            })
        }) {
            return Some((ToolCodeClass::TestFailure, lines[at].to_owned()));
        }
        if let Some(at) = first(&|line| line.starts_with("Diff in ")) {
            return Some((ToolCodeClass::Format, lines[at].to_owned()));
        }
        if command.contains("--fail-under-")
            && let Some(at) = first(&|line| line.starts_with("TOTAL "))
        {
            return Some((ToolCodeClass::CoverageBelow, lines[at].to_owned()));
        }
        None
    }

    fn flaky_retry_env(&self) -> &'static [(&'static str, &'static str)] {
        &[("NEXTEST_FLAKY_RESULT", "pass")]
    }

    fn junit_test_name(&self) -> Option<JunitTestName> {
        Some(|classname, name| {
            if classname.is_empty() {
                name.to_owned()
            } else {
                format!("{classname} {name}")
            }
        })
    }

    fn process_kind(&self, command: &str, name: &str) -> Option<ToolProcess> {
        if matches!(name, "rustc" | "clippy-driver" | "sccache") {
            Some(ToolProcess::Compiler)
        } else if name == "cargo" || name.starts_with("cargo-") || command.contains("/target/") {
            Some(ToolProcess::Build)
        } else {
            None
        }
    }

    fn build_output(&self) -> Option<BuildOutput> {
        Some(BUILD_OUTPUT)
    }

    fn worktree_build_dirs(&self) -> &'static [&'static str] {
        WORKTREE_BUILD_DIRS
    }

    /// The finished sessions of rustc's incremental compilation older than
    /// the newest one of each crate, with their locks ([`incremental`]'s
    /// doc, which names the rustc versions read).
    fn stale_build_entries(
        &self,
        build_dir: &std::path::Path,
        read_dir: &dyn Fn(&std::path::Path) -> Vec<String>,
    ) -> Vec<StaleEntries> {
        incremental::stale_entries(build_dir, read_dir)
    }

    fn recommendation(&self) -> Recommendation {
        RECOMMENDATION
    }

    fn test_lines(&self, path: &str, text: &str) -> Option<Vec<TestLines>> {
        let mut parts = path.rsplit('/');
        let file = parts.next()?;
        if !file.ends_with(".rs") || file == "tests.rs" || parts.any(|dir| dir == "tests") {
            return None;
        }
        Some(test_lines::test_lines(text))
    }
}

/// What the Rust profile recommends, after this repository's own settings:
/// a declaration wherever a `Cargo.toml` is; the formatting, the lints
/// denied as errors and the tests by nextest (whose output the profile
/// reads, flaky retries included) as a task's verification; and the
/// formatting as a review program, which builds nothing, so it returns a
/// run before the review's agents start. For the container: an image with
/// the toolchain `rust-toolchain.toml` names (rustup installs it read-only
/// for every user, and cargo keeps its downloads under the user's own
/// `CARGO_HOME`), the hosts cargo fetches the dependencies of crates.io
/// from, and that whole `CARGO_HOME` as the worker cache, so a worker
/// does not fetch them again. The whole of it, not its `registry` and
/// `git` alone: cargo's lock of its downloads (`.package-cache`) is at
/// the root of `CARGO_HOME`, and on the container's read-only root cargo
/// goes on without it, so workers sharing the downloads would unpack the
/// same crate at once. The image installs what each verification command
/// runs besides cargo (the rustfmt and clippy components and cargo-nextest,
/// at a pinned version) and its build calls each of them, so an image
/// that cannot run a recommended verification fails to build. It assumes
/// the repository pins its toolchain in a `rust-toolchain.toml` at its
/// root: without one its build fails, so the person adds one or adapts the
/// lines.
pub const RECOMMENDATION: Recommendation = Recommendation {
    markers: &["Cargo.toml"],
    verify: &[
        "cargo fmt --all --check",
        "cargo clippy --locked --all-targets -- -D warnings",
        "cargo nextest run --locked --workspace",
    ],
    programs: &[RecommendedProgram {
        name: "rustfmt",
        script: ".dagq/review/programs/rustfmt.sh",
        paths: &["**/*.rs", "rustfmt.toml", ".rustfmt.toml"],
        body: "#!/bin/sh\n\
               # The formatting of the Rust sources, as rustfmt checks it: no build, no test.\n\
               root=$(git rev-parse --show-toplevel) || exit 2\n\
               cd \"$root\" || exit 2\n\
               exec cargo fmt --all --check\n",
    }],
    image: Some(RecommendedImage {
        inputs: &["rust-toolchain.toml"],
        lines: "USER root\n\
                ENV RUSTUP_HOME=/usr/local/rustup \\\n    CARGO_HOME=/usr/local/cargo \\\n    PATH=/usr/local/cargo/bin:$PATH\n\
                ARG NEXTEST_VERSION=0.9.146\n\
                COPY rust-toolchain.toml /usr/local/share/dagq-toolchain/rust-toolchain.toml\n\
                RUN set -eu; \\\n\
                \x20   apt-get update; \\\n\
                \x20   apt-get install -y --no-install-recommends build-essential curl pkg-config; \\\n\
                \x20   rm -rf /var/lib/apt/lists/*; \\\n\
                \x20   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \\\n\
                \x20       | sh -s -- -y --no-modify-path --profile minimal --default-toolchain none; \\\n\
                \x20   cd /usr/local/share/dagq-toolchain; \\\n\
                \x20   rustup toolchain install; \\\n\
                \x20   rustup default \"$(rustup show active-toolchain | cut -d' ' -f1)\"; \\\n\
                \x20   rustup component add rustfmt clippy; \\\n\
                \x20   case \"$(uname -m)\" in \\\n\
                \x20       x86_64) nextest=linux ;; \\\n\
                \x20       aarch64) nextest=linux-arm ;; \\\n\
                \x20       *) echo \"no cargo-nextest for $(uname -m)\" >&2; exit 1 ;; \\\n\
                \x20   esac; \\\n\
                \x20   curl --proto '=https' --tlsv1.2 -LsSf \"https://get.nexte.st/${NEXTEST_VERSION}/${nextest}\" \\\n\
                \x20       | tar zxf - -C \"$CARGO_HOME/bin\"; \\\n\
                \x20   chmod -R a+rX \"$RUSTUP_HOME\" \"$CARGO_HOME\"; \\\n\
                \x20   cargo fmt --version; \\\n\
                \x20   cargo clippy --version; \\\n\
                \x20   cargo nextest --version\n\
                ENV CARGO_HOME=/home/dagq/.cargo\n\
                USER dagq\n",
    }),
    registries: &[
        RecommendedHost {
            host: "index.crates.io",
            why: "the sparse index cargo reads the versions of crates.io's crates from",
        },
        RecommendedHost {
            host: "static.crates.io",
            why: "where cargo downloads the crates of crates.io",
        },
    ],
    worker_cache: &[CARGO_HOME],
};

/// Where the recommended image points cargo's own files (its downloads and
/// their lock) for the container's user, which the worker cache mounts.
pub const CARGO_HOME: &str = "/home/dagq/.cargo";

/// The program and arguments that print the host's Rust toolchain.
pub const RUSTC_VERSION: (&str, &[&str]) = ("rustc", &["-vV"]);

/// `release` and `host` of the output of [`RUSTC_VERSION`].
pub fn rustc_version(output: &str) -> (Option<String>, Option<String>) {
    let field = |name: &str| {
        output.lines().find_map(|line| {
            line.strip_prefix(name)
                .and_then(|rest| rest.strip_prefix(':'))
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
    };
    (field("release"), field("host"))
}

/// The test of a nextest status line that says it passed on a retry.
fn flaky_line(line: &str) -> Option<&str> {
    match nextest_line(line)? {
        (NextestStatus::Flaky, test) => Some(test),
        _ => None,
    }
}

/// The names nextest gives the signals that end a test (its
/// `signal_str`): `SIG<name>` in the status of a first attempt, the bare
/// name after `TRY n`. Any other signal is `ABORT SIG <n>` / `TRY n SIG <n>`.
const NEXTEST_SIGNALS: [&str; 12] = [
    "HUP", "INT", "QUIT", "ILL", "TRAP", "ABRT", "FPE", "KILL", "SEGV", "PIPE", "ALRM", "TERM",
];

/// What a nextest status line says of its test (the module's doc).
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
/// for a status that does not fail a test.
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

#[cfg(test)]
mod tests {
    //! The Rust profile with the runtime's own marks, as this repository
    //! declares it ([`Toolchains::this_repository`]). `tests/common`'s
    //! `within` line in these logs is read by the mark it declares beside
    //! ([`crate::domain::verify_marks`]), not the profile.
    use crate::domain::toolchain::Toolchains;
    use crate::domain::verify_failure::{
        EVIDENCE_CHARS, FailedTests, FailureClass, MAX_FAILED_TESTS, VerifyFailure,
    };
    use serde_json::json;

    #[test]
    fn only_a_rust_file_outside_the_test_files_has_its_lines_told_apart() {
        use super::{RUST, ToolchainProfile};
        let text = "fn a() {}\n#[test]\nfn b() {}\n";
        let lines = RUST.test_lines("src/a.rs", text).unwrap();
        assert_eq!(
            lines
                .iter()
                .map(|l| (*l.start(), *l.end()))
                .collect::<Vec<_>>(),
            [(2, 3)]
        );
        assert_eq!(RUST.test_lines("a.rs", "fn a() {}\n"), Some(vec![]));
        for path in [
            "tests/it/main.rs",
            "crates/x/tests/a.rs",
            "src/main_log/tests.rs",
            "src/a.toml",
            "README.md",
        ] {
            assert_eq!(RUST.test_lines(path, text), None, "{path}");
        }
    }

    fn rust() -> Toolchains {
        Toolchains::this_repository()
    }

    fn classify(command: &str, exit: Option<i32>, signal: Option<i32>, log: &str) -> VerifyFailure {
        crate::domain::verify_failure::classify(&rust(), command, exit, signal, log)
    }

    fn failed_tests(log: &str) -> FailedTests {
        crate::domain::verify_failure::failed_tests(&rust(), log)
    }

    fn flaky_tests(log: &str) -> Vec<String> {
        crate::domain::verify_failure::flaky_tests(&rust(), log)
    }

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

    #[test]
    fn nextest_s_junit_names_a_test_as_it_prints_it() {
        use crate::domain::toolchain::ToolchainProfile;
        let name = super::RUST.junit_test_name().unwrap();
        assert_eq!(
            name("dagq::it", "runtime_claim::claims_in_order"),
            "dagq::it runtime_claim::claims_in_order"
        );
        assert_eq!(name("", "alone"), "alone");
    }

    #[test]
    fn the_compiler_and_the_build_tool_are_told_by_their_commands() {
        use super::RUST;
        use crate::domain::toolchain::{ToolProcess, ToolchainProfile};
        for (command, name, kind) in [
            ("/usr/bin/rustc", "rustc", Some(ToolProcess::Compiler)),
            (
                "clippy-driver",
                "clippy-driver",
                Some(ToolProcess::Compiler),
            ),
            ("sccache", "sccache", Some(ToolProcess::Compiler)),
            ("/h/.cargo/bin/cargo", "cargo", Some(ToolProcess::Build)),
            ("cargo-nextest", "cargo-nextest", Some(ToolProcess::Build)),
            (
                "/r/target/debug/deps/it-0a1b",
                "it-0a1b",
                Some(ToolProcess::Build),
            ),
            ("/bin/zsh", "zsh", None),
        ] {
            assert_eq!(RUST.process_kind(command, name), kind, "{command}");
        }
    }

    #[test]
    fn rustc_s_release_and_host_are_read() {
        let output = "rustc 1.90.0 (abc 2025-09-14)\nbinary: rustc\nhost: aarch64-apple-darwin\nrelease: 1.90.0\n";
        assert_eq!(
            super::rustc_version(output),
            (
                Some("1.90.0".to_owned()),
                Some("aarch64-apple-darwin".to_owned())
            )
        );
        assert_eq!(super::rustc_version("release:\n"), (None, None));
    }
}
