//! A second toolchain profile only the tests have (ADR-t2268-1 decision
//! 8): a made-up tool, implemented and registered here and nowhere in the
//! runtime, which the runtime tests and the CLI tests declare in
//! `[toolchains.<name>]` with `profile = "made-up"`.
use dagq::domain::toolchain::{
    BuildOutput, JunitTestName, ProfileRegistry, Recommendation, RecommendedProgram, TestLines,
    ToolCodeClass, ToolHostClass, ToolProcess, ToolchainProfile, rust,
};

/// A made-up tool: `BROKEN <test>` failed, `WOBBLY <test>` passed on its
/// retry, `HUNG <test>` ran out of time; its retry runs with
/// `MADE_FLAKY=accept`; its JUnit names a test `<classname>#<name>`, its
/// build tool is `madebuild`, it keeps no build output in a directory
/// of the runtime's, and it tells no test's lines apart. It recommends
/// itself where a `made.build` is, `madebuild check` as the verification,
/// and the review programs `made-lint` under `.dagq/review/programs/` and
/// `made-outside` in `scripts/`, outside what `setup-review` may write.
pub struct Made;

pub static MADE: Made = Made;

/// The built-in profiles and the made-up one.
pub static PROFILES: ProfileRegistry = ProfileRegistry::new(&[&rust::RUST, &MADE]);

fn named<'a>(line: &'a str, mark: &str) -> Option<&'a str> {
    line.trim().strip_prefix(mark)?.strip_prefix(' ')
}

impl ToolchainProfile for Made {
    fn name(&self) -> &'static str {
        "made-up"
    }
    fn killed_line(&self, _: &[&str]) -> Option<usize> {
        None
    }
    fn failed_tests(&self, text: &str) -> Vec<(usize, String)> {
        text.lines()
            .enumerate()
            .filter_map(|(index, line)| named(line, "BROKEN").map(|t| (index, t.to_owned())))
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
        lines
            .iter()
            .find(|line| named(line, "BROKEN").is_some())
            .map(|line| (ToolCodeClass::TestFailure, (*line).to_owned()))
    }
    fn flaky_retry_env(&self) -> &'static [(&'static str, &'static str)] {
        &[("MADE_FLAKY", "accept")]
    }
    fn junit_test_name(&self) -> Option<JunitTestName> {
        Some(|classname, name| format!("{classname}#{name}"))
    }
    fn process_kind(&self, _: &str, name: &str) -> Option<ToolProcess> {
        (name == "madebuild").then_some(ToolProcess::Build)
    }
    fn build_output(&self) -> Option<BuildOutput> {
        None
    }
    fn recommendation(&self) -> Recommendation {
        Recommendation {
            markers: &["made.build"],
            verify: &["madebuild check"],
            programs: &[
                RecommendedProgram {
                    name: "made-lint",
                    script: ".dagq/review/programs/made-lint.sh",
                    paths: &["**/*.made"],
                    body: "#!/bin/sh\nroot=$(git rev-parse --show-toplevel) || exit 2\nexit 0\n",
                },
                RecommendedProgram {
                    name: "made-outside",
                    script: "scripts/made-outside.sh",
                    paths: &["**"],
                    body: "#!/bin/sh\nexit 0\n",
                },
            ],
        }
    }

    fn test_lines(&self, _: &str, _: &str) -> Option<Vec<TestLines>> {
        None
    }
}
