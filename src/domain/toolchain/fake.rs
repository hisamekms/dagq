//! A second profile only the tests have: a made-up tool whose output says
//! `BROKEN <test>` for a failed test, `WOBBLY <test>` for one that passed
//! on its retry, `HUNG <test>` for one out of time and `NO BUILD` for a
//! build that failed, and whose retry runs with `FAKE_FLAKY=accept`; whose
//! JUnit names a test `<classname>/<name>`; whose processes are the
//! build tool `fakebuild` and the compiler `fakec`; and which builds into
//! the directory `FAKE_OUT` names, `fake-out`, and in a run's worktree
//! into `fake-out` and `fake-cache`; whose files are `*.fake`,
//! a line of which that starts with `TEST ` is a test's; which recommends
//! itself where a `fake.build` is, which classes `fakebuild check` as
//! `fake_check`, `fakebuild check` as the verification,
//! and two review programs, `fake-lint` under `.dagq/review/programs/` and
//! `fake-outside` outside it, which `setup-review` may not write. It
//! is registered beside the built-in ones in [`REGISTRY`], and nothing
//! else of the runtime knows it (ADR-t2268-1 decision 8).
use super::{
    BuildOutput, JunitTestName, ProfileRegistry, Recommendation, RecommendedProgram, TestLines,
    ToolCodeClass, ToolHostClass, ToolProcess, ToolchainDeclaration, ToolchainProfile, Toolchains,
    rust,
};

pub static FAKE: Fake = Fake;

pub static REGISTRY: ProfileRegistry = ProfileRegistry::new(&[&rust::RUST, &FAKE]);

pub struct Fake;

/// The test a line of `mark` names.
fn named<'a>(line: &'a str, mark: &str) -> Option<&'a str> {
    line.trim().strip_prefix(mark)?.strip_prefix(' ')
}

impl ToolchainProfile for Fake {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn command_rules(&self) -> Vec<crate::domain::command_rules::CommandRule> {
        vec![
            crate::domain::command_rules::CommandRule::new("fake_check", "fakebuild")
                .subcommands(&["check"]),
        ]
    }

    fn killed_line(&self, _: &[&str]) -> Option<usize> {
        None
    }

    fn failed_tests(&self, text: &str) -> Vec<(usize, String)> {
        text.lines()
            .enumerate()
            .filter_map(|(index, line)| {
                named(line, "BROKEN")
                    .or_else(|| named(line, "HUNG"))
                    .map(|test| (index, test.to_owned()))
            })
            .collect()
    }

    fn flaky_tests(&self, text: &str) -> Vec<String> {
        text.lines()
            .filter_map(|line| named(line, "WOBBLY").map(str::to_owned))
            .collect()
    }

    fn flaky_line(&self, lines: &[&str]) -> Option<usize> {
        lines
            .iter()
            .position(|line| named(line, "WOBBLY").is_some())
    }

    fn test_host_line(&self, lines: &[&str]) -> Option<(ToolHostClass, usize)> {
        lines
            .iter()
            .position(|line| named(line, "HUNG").is_some())
            .map(|at| (ToolHostClass::Timeout, at))
    }

    fn code_failure(&self, _: &str, lines: &[&str]) -> Option<(ToolCodeClass, String)> {
        if let Some(at) = lines.iter().position(|line| *line == "NO BUILD") {
            return Some((ToolCodeClass::BuildError, lines[at].to_owned()));
        }
        lines
            .iter()
            .find(|line| named(line, "BROKEN").is_some())
            .map(|line| (ToolCodeClass::TestFailure, (*line).to_owned()))
    }

    fn flaky_retry_env(&self) -> &'static [(&'static str, &'static str)] {
        &[("FAKE_FLAKY", "accept")]
    }

    fn junit_test_name(&self) -> Option<JunitTestName> {
        Some(|classname, name| format!("{classname}/{name}"))
    }

    fn process_kind(&self, _: &str, name: &str) -> Option<ToolProcess> {
        match name {
            "fakebuild" => Some(ToolProcess::Build),
            "fakec" => Some(ToolProcess::Compiler),
            _ => None,
        }
    }

    fn build_output(&self) -> Option<BuildOutput> {
        Some(BUILD_OUTPUT)
    }

    fn worktree_build_dirs(&self) -> &'static [&'static str] {
        &["fake-out", "fake-cache"]
    }

    fn recommendation(&self) -> Recommendation {
        Recommendation {
            markers: &["fake.build"],
            verify: &["fakebuild check"],
            programs: &[
                RecommendedProgram {
                    name: "fake-lint",
                    script: ".dagq/review/programs/fake-lint.sh",
                    paths: &["**/*.fake"],
                    body: "#!/bin/sh\nroot=$(git rev-parse --show-toplevel) || exit 2\nexit 0\n",
                },
                RecommendedProgram {
                    name: "fake-outside",
                    script: "scripts/fake-outside.sh",
                    paths: &["**"],
                    body: "#!/bin/sh\nexit 0\n",
                },
            ],
        }
    }

    fn test_lines(&self, path: &str, text: &str) -> Option<Vec<TestLines>> {
        path.ends_with(".fake").then(|| {
            text.lines()
                .enumerate()
                .filter(|(_, line)| line.starts_with("TEST "))
                .map(|(index, _)| index + 1..=index + 1)
                .collect()
        })
    }
}

/// The fake profile's build output.
pub const BUILD_OUTPUT: BuildOutput = BuildOutput {
    dir: "fake-out",
    env: "FAKE_OUT",
};

/// A repository that declares only the fake profile, over the whole of it.
pub fn only_fake() -> Toolchains {
    Toolchains::resolve(
        &REGISTRY,
        &[ToolchainDeclaration {
            name: "made-up".to_owned(),
            profile: "fake".to_owned(),
            paths: Vec::new(),
            line: 1,
        }],
    )
    .unwrap()
}
