//! The worker's local gates (ADR-t2589-1 decision 6): which of the checks
//! a project's `dagq.toml` declares as gates a run's diff requires. A gate
//! is required when a changed test is in its paths; one not required is
//! left out with the reason that no test it requires changed, which the
//! review takes from here rather than from the worker's evidence.
//!
//! A changed path is a test's when it matches the declared globs of the
//! tests and a changed line of it is in a test, as the toolchain profile
//! whose declaration's paths match it tells
//! ([`Toolchains::test_lines`]). Where no profile tells the file's lines
//! apart, or its text or its diff's lines cannot be read, any change of it
//! counts (a gate is kept, never left out by what is not known). The
//! runtime knows no gate, glob or language: they are the project's and the
//! profiles'.

use serde_json::{Value, json};

use super::scope::glob_matches;
use super::toolchain::{TestLines, Toolchains};

/// The gates a project declares and the globs of its tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalGates {
    /// The globs of the paths that are tests.
    pub tests: Vec<String>,
    /// Each gate, in the order declared.
    pub gates: Vec<LocalGate>,
}

/// One gate: a check the worker runs when a test of its paths changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalGate {
    /// The gate's name in the declaration.
    pub name: String,
    /// The globs of the tests that require it.
    pub paths: Vec<String>,
    /// The kind the project's scripts give its records
    /// ([`crate::domain::local_checks::LocalCheck::kind`]).
    pub kind: String,
}

/// What the review reads of a repository's `dagq.toml` to judge the
/// gates: the gates, `None` without a declaration, and the profiles.
#[derive(Debug, Clone)]
pub struct GateConfig {
    pub gates: Option<LocalGates>,
    pub toolchains: Toolchains,
}

/// A hunk of the diff on the side of the receipt's commit: `count` lines
/// from line `start`, or, with a `count` of 0, lines removed after line
/// `start` (0 before the first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hunk {
    pub start: usize,
    pub count: usize,
}

impl Hunk {
    /// The lines of the commit's side it touches: its own, or for lines
    /// removed, those on either side of where they were.
    fn lines(self) -> TestLines {
        if self.count == 0 {
            self.start.max(1)..=self.start + 1
        } else {
            self.start..=self.start + self.count - 1
        }
    }
}

/// How a changed path reads at the receipt's commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Not there at the commit: no test of it is left to run.
    Removed,
    /// Its text at the commit and the hunks of its diff.
    Lines { text: String, hunks: Vec<Hunk> },
    /// Its text or its diff's lines cannot be read.
    Unknown,
}

/// One gate as the diff requires it: required when `tests`, the changed
/// tests in its paths, has any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateNeed {
    pub name: String,
    pub kind: String,
    pub tests: Vec<String>,
}

impl GateNeed {
    pub fn required(&self) -> bool {
        !self.tests.is_empty()
    }

    /// The gate as the review's material lists it.
    pub fn value(&self) -> Value {
        json!({
            "gate": self.name,
            "kind": self.kind,
            "required": self.required(),
            "reason": if self.required() {
                "a test in its paths changed"
            } else {
                "no test in its paths changed"
            },
            "changed_tests": self.tests,
        })
    }
}

impl LocalGates {
    /// Each gate as the diff requires it, in the order declared: `changed`
    /// are the paths the diff changes, and `read` tells how one of them
    /// reads at the commit; it is called only for a path that matches the
    /// tests' globs and a gate's, once.
    pub fn judge(
        &self,
        toolchains: &Toolchains,
        changed: &[String],
        mut read: impl FnMut(&str) -> Change,
    ) -> Vec<GateNeed> {
        let in_gate = |path: &str| self.gates.iter().any(|gate| matches_any(&gate.paths, path));
        let tests: Vec<&String> = changed
            .iter()
            .filter(|path| matches_any(&self.tests, path) && in_gate(path))
            .filter(|path| changes_a_test(toolchains, path, &read(path)))
            .collect();
        self.gates
            .iter()
            .map(|gate| GateNeed {
                name: gate.name.clone(),
                kind: gate.kind.clone(),
                tests: tests
                    .iter()
                    .filter(|path| matches_any(&gate.paths, path))
                    .map(|path| (*path).clone())
                    .collect(),
            })
            .collect()
    }
}

fn matches_any(globs: &[String], path: &str) -> bool {
    globs.iter().any(|glob| glob_matches(glob, path))
}

/// Whether `change` of the test's path `path` changes a test: see the
/// module's doc. A text with no hunk (a change of its mode alone) counts.
pub fn changes_a_test(toolchains: &Toolchains, path: &str, change: &Change) -> bool {
    match change {
        Change::Removed => false,
        Change::Unknown => true,
        Change::Lines { text, hunks } => {
            let Some(tests) = toolchains.test_lines(path, text) else {
                return true;
            };
            hunks.is_empty()
                || hunks.iter().any(|hunk| {
                    let lines = hunk.lines();
                    tests
                        .iter()
                        .any(|test| test.start() <= lines.end() && lines.start() <= test.end())
                })
        }
    }
}

/// The hunks of a unified diff with no context (`git diff -U0`), from its
/// `@@ -a,b +c,d @@` lines; a line that is none is passed over.
pub fn hunks_of(diff: &str) -> Vec<Hunk> {
    diff.lines()
        .filter_map(|line| {
            let new = line.strip_prefix("@@ -")?.split(' ').nth(1)?;
            let new = new.strip_prefix('+')?;
            let (start, count) = match new.split_once(',') {
                Some((start, count)) => (start.parse().ok()?, count.parse().ok()?),
                None => (new.parse().ok()?, 1),
            };
            Some(Hunk { start, count })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::toolchain::{ToolchainDeclaration, fake};

    fn gates() -> LocalGates {
        LocalGates {
            tests: vec!["src/**".into(), "tests/**".into(), "lib/**".into()],
            gates: vec![
                LocalGate {
                    name: "stress".into(),
                    paths: vec!["src/**".into(), "tests/**".into(), "lib/**".into()],
                    kind: "stress".into(),
                },
                LocalGate {
                    name: "slow".into(),
                    paths: vec!["tests/**".into()],
                    kind: "time-gate".into(),
                },
            ],
        }
    }

    fn lines(text: &str, hunks: &[(usize, usize)]) -> Change {
        Change::Lines {
            text: text.into(),
            hunks: hunks
                .iter()
                .map(|&(start, count)| Hunk { start, count })
                .collect(),
        }
    }

    fn required(needs: &[GateNeed]) -> Vec<(&str, Vec<&str>)> {
        needs
            .iter()
            .map(|need| {
                (
                    need.name.as_str(),
                    need.tests.iter().map(String::as_str).collect(),
                )
            })
            .collect()
    }

    const RUST_SOURCE: &str =
        "fn a() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n";

    #[test]
    fn a_gate_is_required_by_a_changed_line_of_a_test_in_its_paths() {
        let changed = vec![
            "src/a.rs".to_owned(),
            "src/b.rs".to_owned(),
            "docs/x.md".to_owned(),
        ];
        let mut read = Vec::new();
        let needs = gates().judge(&Toolchains::this_repository(), &changed, |path| {
            read.push(path.to_owned());
            match path {
                // A line of the test module changed.
                "src/a.rs" => lines(RUST_SOURCE, &[(6, 1)]),
                // Only the production function changed.
                _ => lines(RUST_SOURCE, &[(1, 1)]),
            }
        });
        assert_eq!(
            required(&needs),
            [("stress", vec!["src/a.rs"]), ("slow", vec![])]
        );
        assert!(needs[0].required() && !needs[1].required());
        assert_eq!(needs[1].value()["reason"], "no test in its paths changed");
        // The docs match no glob and are not read.
        assert_eq!(read, ["src/a.rs", "src/b.rs"]);
    }

    /// A change of the body of a test whose signature has generic
    /// arguments with a comma changes the test.
    #[test]
    fn a_changed_body_of_a_test_returning_a_generic_type_requires_its_gate() {
        let text = "#[test]\nfn t() -> Result<(), std::io::Error> {\n    Ok(())\n}\n\nfn a() {}\n";
        let judge = |hunks: &[(usize, usize)]| {
            gates().judge(
                &Toolchains::this_repository(),
                &["src/a.rs".to_owned()],
                |_| lines(text, hunks),
            )[0]
            .required()
        };
        assert!(judge(&[(3, 1)]));
        assert!(!judge(&[(6, 1)]));
    }

    /// A change of the body of a test whose return type is a macro's call
    /// in braces changes the test.
    #[test]
    fn a_changed_body_of_a_test_returning_a_macro_s_type_requires_its_gate() {
        let text = "macro_rules! R { () => { Result<(), std::io::Error> }; }\n#[test]\nfn t() -> R!{} {\n    Ok(())\n}\n\nfn a() {}\n";
        let judge = |hunks: &[(usize, usize)]| {
            gates().judge(
                &Toolchains::this_repository(),
                &["src/a.rs".to_owned()],
                |_| lines(text, hunks),
            )[0]
            .required()
        };
        assert!(judge(&[(4, 1)]));
        assert!(!judge(&[(7, 1)]));
    }

    #[test]
    fn what_no_profile_tells_apart_or_cannot_be_read_counts_and_what_is_removed_does_not() {
        let toolchains = Toolchains::this_repository();
        let change = |path: &str, change: Change| {
            gates().judge(&toolchains, &[path.to_owned()], |_| change.clone())
        };
        // Every file under `tests/` is a test's to the Rust profile.
        let needs = change("tests/it/a.rs", lines("fn helper() {}\n", &[(1, 1)]));
        assert_eq!(
            required(&needs),
            [
                ("stress", vec!["tests/it/a.rs"]),
                ("slow", vec!["tests/it/a.rs"])
            ]
        );
        // No profile reads a file of another language.
        let needs = change("lib/a.py", lines("x = 1\n", &[(1, 1)]));
        assert!(needs[0].required());
        // A text or diff that cannot be read, and a mode alone.
        assert!(change("src/a.rs", Change::Unknown)[0].required());
        assert!(change("src/a.rs", lines(RUST_SOURCE, &[]))[0].required());
        // A removed file leaves no test to run.
        assert!(!change("src/a.rs", Change::Removed)[0].required());
        // Lines removed beside a test count; far from one do not.
        assert!(change("src/a.rs", lines(RUST_SOURCE, &[(5, 0)]))[0].required());
        assert!(!change("src/a.rs", lines(RUST_SOURCE, &[(1, 0)]))[0].required());
    }

    /// The judgement goes by the profile a declaration's paths match,
    /// whatever the language: the fake profile tells `*.fake` files apart,
    /// and the Rust profile is not asked where it is not declared.
    #[test]
    fn the_profile_whose_paths_match_tells_the_lines_apart_whatever_its_language() {
        let declare = |name: &str, profile: &str, paths: &[&str]| ToolchainDeclaration {
            name: name.into(),
            profile: profile.into(),
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            line: 1,
        };
        let toolchains = Toolchains::resolve(
            &fake::REGISTRY,
            &[
                declare("made-up", "fake", &["lib/**"]),
                declare("core", "rust", &["src/**"]),
            ],
        )
        .unwrap();
        let text = "setup\nTEST one\nTEST two\n";
        let judge = |path: &str, hunks: &[(usize, usize)]| {
            gates().judge(&toolchains, &[path.to_owned()], |_| lines(text, hunks))[0].required()
        };
        assert!(judge("lib/a.fake", &[(2, 1)]));
        assert!(!judge("lib/a.fake", &[(1, 1)]));
        // A `.fake` file where only the Rust profile's paths match: the
        // Rust profile does not read it, so it counts.
        assert!(judge("src/a.fake", &[(1, 1)]));
        // A Rust source where only the fake profile's paths match is not
        // told apart by the Rust profile.
        let needs = gates().judge(&toolchains, &["lib/a.rs".to_owned()], |_| {
            lines(RUST_SOURCE, &[(1, 1)])
        });
        assert!(needs[0].required());
        // No profile declared: none tells a Rust source apart, so a change
        // of its production code alone counts.
        let needs = gates().judge(&Toolchains::none(), &["src/a.rs".to_owned()], |_| {
            lines(RUST_SOURCE, &[(1, 1)])
        });
        assert!(needs[0].required());
        // Only the fake profile declared: the Rust profile is not asked.
        let needs = gates().judge(&fake::only_fake(), &["src/a.rs".to_owned()], |_| {
            lines(RUST_SOURCE, &[(1, 1)])
        });
        assert!(needs[0].required());
        let needs = gates().judge(&fake::only_fake(), &["src/a.fake".to_owned()], |_| {
            lines(text, &[(1, 1)])
        });
        assert!(!needs[0].required());
    }

    #[test]
    fn the_hunks_are_read_from_the_lines_of_a_diff_without_context() {
        let diff = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -3 +3 @@ fn a\n-a\n+b\n@@ -10,2 +9,0 @@\n-c\n-d\n@@ -20,0 +18,4 @@\n+@@ -1 +1 @@\n";
        assert_eq!(
            hunks_of(diff),
            [
                Hunk { start: 3, count: 1 },
                Hunk { start: 9, count: 0 },
                Hunk {
                    start: 18,
                    count: 4
                },
            ]
        );
    }
}
