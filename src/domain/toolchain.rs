//! The toolchain profiles (ADR-t2268-1): the knowledge of one language's
//! tools behind one port, [`ToolchainProfile`], which only its
//! implementations hold. The runtime builds them in ([`BUILTIN`], the one
//! place a name is looked up), and a repository declares in `dagq.toml`
//! which to use ([`ToolchainDeclaration`], resolved to [`Toolchains`]).
//!
//! What a profile reads is a verification command's output (the marks of
//! its tools that put a failure down to a class of [`FailureClass`], the
//! tests it names as failed and as passed on a retry, and the environment
//! a retry of flaky tests runs with), the names of the tests of its JUnit
//! the CI watch lists ([`ToolchainProfile::junit_test_name`]) and the
//! processes of its tools the host's load is split by
//! ([`ToolchainProfile::process_kind`]); and where its tools put what they
//! build ([`BuildOutput`]) when the runtime runs a command in a directory
//! of its own; and which lines of a file of its language are in a test
//! ([`ToolchainProfile::test_lines`]). The runtime's own marks (a full disk, a signal, the
//! conventional exit of a timeout, the limit of the whole command) and the
//! order the marks are tried in are [`crate::domain::verify_failure`]'s,
//! and work whatever is declared; so are the kinds of process of no
//! language's tools (`claude`, `dagq`, `other`,
//! [`crate::domain::host_metrics::process_kind`]).
//!
//! It also recommends settings a person writes into `dagq.toml`
//! ([`recommend`]), which work only as written there.
//!
//! A new language is one implementation of [`ToolchainProfile`] and one
//! line in [`BUILTIN`].
use std::path::Path;

use serde_json::{Value, json};

use super::{
    command_rules::TelemetrySettings, scope::glob_matches, verify_failure::FailureClass,
    verify_marks::ProjectMark,
};

pub mod recommend;
pub mod rust;

pub use recommend::{Recommendation, RecommendedProgram};

#[cfg(test)]
pub(crate) mod fake;

/// A class a profile may put a failure on the host down to: one of the
/// host's classes the runtime already has, never a new one, so a profile
/// adds marks to them without widening
/// [`FailureClass::is_environmental`] (ADR-t639-1 decision 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolHostClass {
    Killed,
    Timeout,
}

impl From<ToolHostClass> for FailureClass {
    fn from(class: ToolHostClass) -> Self {
        match class {
            ToolHostClass::Killed => Self::Killed,
            ToolHostClass::Timeout => Self::Timeout,
        }
    }
}

/// A class a profile may put a failure of the code down to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCodeClass {
    BuildError,
    Lint,
    TestFailure,
    Format,
    CoverageBelow,
}

impl From<ToolCodeClass> for FailureClass {
    fn from(class: ToolCodeClass) -> Self {
        match class {
            ToolCodeClass::BuildError => Self::BuildError,
            ToolCodeClass::Lint => Self::Lint,
            ToolCodeClass::TestFailure => Self::TestFailure,
            ToolCodeClass::Format => Self::Format,
            ToolCodeClass::CoverageBelow => Self::CoverageBelow,
        }
    }
}

/// A kind of process of a language's tools the host's load is split by
/// ([`crate::domain::host_metrics`]). Its columns keep the names of the
/// Rust tools they were first made for (`cpu_cargo`, `cpu_rustc` and their
/// `rss_*_mb`), since the NodeSampleStore has a column of each name and
/// every file is read by its header's names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolProcess {
    /// The build tool and what it runs (build scripts, test binaries):
    /// the `cargo` columns.
    Build,
    /// The compiler and what wraps it: the `rustc` columns.
    Compiler,
}

impl ToolProcess {
    /// The kind of process the columns are named by.
    pub const fn column_kind(self) -> &'static str {
        match self {
            Self::Build => "cargo",
            Self::Compiler => "rustc",
        }
    }
}

/// How a profile names a test case of its tool's JUnit XML, given its
/// `classname` and `name`.
pub type JunitTestName = fn(&str, &str) -> String;

/// The knowledge of one language's tools (ADR-t2268-1 decision 1). Each
/// method reads a verification command's output without the terminal's
/// colors: `text` whole, or `lines`, its lines trimmed (the same lines,
/// so an index of one is a line of the other). The runtime calls them in
/// the order [`crate::domain::verify_failure::classify`] keeps, each
/// declared profile in turn, and takes the first answer.
pub trait ToolchainProfile: Sync {
    /// The name a declaration's `profile` names it by.
    fn name(&self) -> &'static str;

    /// The tool's programs that a command's shape keeps by name once the
    /// profile is declared, with their known subcommands and options that
    /// take no value, kept in plain, and where a command they run begins
    /// (ADR-t1673-1); none by default.
    fn subcommands(&self) -> &'static [super::command_shape::SubcommandList] {
        &[]
    }

    /// The profile's own rules of the commands' classification, tried
    /// after the project's `[[telemetry.commands]]` and before the
    /// runtime's common rules once the profile is declared
    /// ([`super::command_rules::effective_rules`]): what its tools'
    /// commands are, never a project's own names (its test targets);
    /// none by default.
    fn command_rules(&self) -> Vec<super::command_rules::CommandRule> {
        Vec::new()
    }

    /// The line where the tool says a signal ended a process it ran: a
    /// kill, tried before the flaky tests, since a kill under the tool is
    /// the host's even when a test then passed on its retry.
    fn killed_line(&self, lines: &[&str]) -> Option<usize>;

    /// The tests the output names as failed, each with the index of the
    /// line that names it, in the order of the lines (a test may come
    /// more than once; the runtime keeps the first).
    fn failed_tests(&self, text: &str) -> Vec<(usize, String)>;

    /// The tests the output names as passed when the tool ran them again
    /// after a failure, in the order they appear, each once.
    fn flaky_tests(&self, text: &str) -> Vec<String>;

    /// The line that shows the failure flaky, once every failed test is
    /// one of [`Self::flaky_tests`].
    fn flaky_line(&self, lines: &[&str]) -> Option<usize>;

    /// After the flaky tests: the line where the tool says a test was
    /// killed or ran out of its time.
    fn test_host_line(&self, lines: &[&str]) -> Option<(ToolHostClass, usize)>;

    /// After the conventional exit of a timeout: why the code failed, and
    /// the evidence (a line, or a line with where it points).
    fn code_failure(&self, command: &str, lines: &[&str]) -> Option<(ToolCodeClass, String)>;

    /// The environment the landing's verification runs with when it is
    /// done once more because only flaky tests failed (ADR-t768-1); those
    /// variables are never passed on from the process that runs the first
    /// verification.
    fn flaky_retry_env(&self) -> &'static [(&'static str, &'static str)];

    /// How the CI watch names a test case of the tool's JUnit XML: as the
    /// tool prints the test, so the list of the failing tests compares
    /// with the landing's verification output. `None` when the tool writes
    /// no JUnit the CI watch reads.
    fn junit_test_name(&self) -> Option<JunitTestName>;

    /// The kind of a process of the tool's by its command (`command`, a
    /// path or a name as `ps` gives it, `name` its last part), counted in
    /// the host's load; `None` for a process not the tool's.
    fn process_kind(&self, command: &str, name: &str) -> Option<ToolProcess>;

    /// Where the profile's tools put what they build when the runtime
    /// runs a command in a directory of its own (ADR-t2268-1 decision 1):
    /// the landing recheck's, and the automatic update's build of dagq's
    /// own source. `None` for tools that keep no build output there.
    fn build_output(&self) -> Option<BuildOutput>;

    /// The directories directly under a run's worktree the profile's tools
    /// build into when a worker's turn or its checks run them there: what
    /// the cleanup of a run's worktree removes, unless Git tracks a file
    /// in one. Empty for tools that build nothing there.
    fn worktree_build_dirs(&self) -> &'static [&'static str];

    /// The settings the profile recommends a repository that uses it
    /// (ADR-t2268-1 decision 7): proposals `dagq init` and `dagq
    /// setup-review` show ([`recommend`]), never values the runtime uses
    /// unless `dagq.toml` has them.
    fn recommendation(&self) -> Recommendation;

    /// The lines of `text`, the file at `path` (from the repository's
    /// root), that are in a test (ADR-t2589-1 decision 6): ranges of line
    /// numbers from 1, ends included, which may overlap. `None` when the
    /// profile does not tell the lines of the file apart (not its
    /// language's, or one whose every line its tools build as a test): a
    /// changed line there counts as a test's.
    fn test_lines(&self, path: &str, text: &str) -> Option<Vec<TestLines>>;
}

/// Lines of a file in a test ([`ToolchainProfile::test_lines`]).
pub type TestLines = std::ops::RangeInclusive<usize>;

/// A profile's build output in a directory of the runtime's: the
/// directory `dir` names under it, and the variable `env` set to it on the
/// command, over the `[run.env]` value of it. Profiles name different
/// directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildOutput {
    pub dir: &'static str,
    pub env: &'static str,
}

impl BuildOutput {
    /// The directory under `root`.
    pub fn dir_under(&self, root: &Path) -> std::path::PathBuf {
        root.join(self.dir)
    }

    /// The build output of the declaration named `declaration` of the
    /// profile named `profile`, one directory per declaration
    /// (ADR-t2268-1 decision 6): `dir` itself for the declaration named
    /// after its profile, so a repository that declares its profile under
    /// the profile's name builds in the profile's own directory;
    /// `<dir>-<declaration>` for any other. The names
    /// of the declarations are distinct and kebab-case, so are these.
    pub fn declared(&self, declaration: &str, profile: &str) -> BuildDir {
        let dir = if declaration == profile {
            self.dir.to_owned()
        } else {
            format!("{}-{declaration}", self.dir)
        };
        BuildDir { dir, env: self.env }
    }

    /// Whether `name`, an entry of a directory the runtime builds in, is
    /// a directory [`Self::declared`] gives, for whichever declaration.
    pub fn holds(&self, name: &str) -> bool {
        name == self.dir
            || name
                .strip_prefix(self.dir)
                .and_then(|rest| rest.strip_prefix('-'))
                .is_some_and(|declaration| !declaration.is_empty())
    }
}

/// A declaration's build output ([`BuildOutput::declared`]): the
/// directory `dir` under the runtime's directory of the command, the only
/// one the runtime makes there for the declaration, and the variable
/// `env` set to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildDir {
    pub dir: String,
    pub env: &'static str,
}

impl BuildDir {
    /// The directory under `root`.
    pub fn dir_under(&self, root: &Path) -> std::path::PathBuf {
        root.join(&self.dir)
    }
}

/// The profiles a declaration may name, looked up by name.
pub struct ProfileRegistry {
    profiles: &'static [&'static dyn ToolchainProfile],
}

impl std::fmt::Debug for ProfileRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.profiles().map(|profile| profile.name()))
            .finish()
    }
}

impl ProfileRegistry {
    pub const fn new(profiles: &'static [&'static dyn ToolchainProfile]) -> Self {
        Self { profiles }
    }

    /// The profile named `name`.
    pub fn get(&self, name: &str) -> Option<&'static dyn ToolchainProfile> {
        self.profiles
            .iter()
            .copied()
            .find(|profile| profile.name() == name)
    }

    /// Every profile, in the order registered.
    pub fn profiles(&self) -> impl Iterator<Item = &'static dyn ToolchainProfile> + '_ {
        self.profiles.iter().copied()
    }

    /// Whether `name`, an entry of a directory the runtime builds in, is
    /// the build output of a declaration of any of the profiles
    /// ([`BuildOutput::holds`]): what the cleanup for room clears, whatever
    /// a repository declares now, so the directory of a declaration since
    /// removed does not stay.
    pub fn holds_build_dir(&self, name: &str) -> bool {
        self.profiles()
            .filter_map(|profile| profile.build_output())
            .any(|output| output.holds(name))
    }

    /// The names of the profiles, joined for a message.
    fn names(&self) -> String {
        self.profiles()
            .map(|profile| profile.name())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The profiles the runtime builds in: one line per profile.
pub static BUILTIN: ProfileRegistry = ProfileRegistry::new(&[&rust::RUST]);

/// A `[toolchains.<name>]` of `dagq.toml` as written (its format is on
/// the table's definition in the config's reader).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolchainDeclaration {
    /// The table's `<name>`.
    pub name: String,
    /// The profile it uses, a name of the registry.
    pub profile: String,
    /// The globs of the paths it applies to; empty is the whole
    /// repository.
    pub paths: Vec<String>,
    /// The line of its header in `dagq.toml`, for its mistakes.
    pub line: usize,
}

/// A declaration resolved to its profile.
#[derive(Clone)]
struct Declared {
    name: String,
    profile: &'static dyn ToolchainProfile,
    paths: Vec<String>,
}

/// The profiles that read a repository's output: those its `dagq.toml`
/// declares, in the order declared, and none while it declares none
/// (ADR-t2268-1 decision 3), when only the runtime's own marks read it;
/// and the project's own marks ([`verify_marks`]).
#[derive(Clone)]
pub struct Toolchains {
    declared: Vec<Declared>,
    marks: Vec<ProjectMark>,
    telemetry: TelemetrySettings,
}

impl std::fmt::Debug for Toolchains {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Toolchains")
            .field(
                "profiles",
                &self
                    .profiles()
                    .map(|profile| profile.name())
                    .collect::<Vec<_>>(),
            )
            .field("marks", &self.marks.len())
            .finish()
    }
}

impl Toolchains {
    /// No profile and no mark, as a repository that declares neither:
    /// an output read with it names no test and is classed by the
    /// runtime's own marks alone. What reads with the repository's
    /// declaration and cannot know it (its `dagq.toml` cannot be read or
    /// has a mistake) reads with this too rather than guess a profile
    /// (ADR-t2257-1 decision 1).
    pub fn none() -> Self {
        Self {
            declared: Vec::new(),
            marks: Vec::new(),
            telemetry: TelemetrySettings::default(),
        }
    }

    /// The Rust profile declared under its name over the whole repository,
    /// with `tests/common`'s `within` line as a mark: what this
    /// repository's `dagq.toml` declares, for the tests that read its
    /// output.
    #[cfg(test)]
    pub(crate) fn this_repository() -> Self {
        Self {
            declared: vec![Declared {
                name: rust::RUST.name().to_owned(),
                profile: &rust::RUST,
                paths: Vec::new(),
            }],
            marks: vec![super::verify_marks::within()],
            telemetry: TelemetrySettings::default(),
        }
    }

    /// The profiles the runtime's measures read a worker's commands' output
    /// with (the tests they name as failed), from `resolved`, the
    /// repository's declaration as its landing resolves it; `None`, a
    /// queue bound to no repository, and `Err`, a main checkout that is not
    /// found or a `dagq.toml` that cannot be read or has a mistake, read
    /// with [`Self::none`], so the measures do not stop and read with no
    /// profile the repository may not use.
    pub fn for_measures<E>(resolved: Option<Result<Self, E>>) -> Self {
        match resolved {
            Some(Ok(toolchains)) => toolchains,
            None | Some(Err(_)) => Self::none(),
        }
    }

    /// With the project's own `marks` as declared, in the order declared:
    /// none keeps the marks as they are.
    pub fn with_marks(mut self, marks: Vec<ProjectMark>) -> Self {
        if !marks.is_empty() {
            self.marks = marks;
        }
        self
    }

    /// The project's own marks, in the order declared.
    pub fn marks(&self) -> &[ProjectMark] {
        &self.marks
    }

    /// With `dagq.toml`'s `[telemetry]` and `[[telemetry.commands]]` as
    /// read.
    #[must_use]
    pub fn with_telemetry(mut self, telemetry: TelemetrySettings) -> Self {
        self.telemetry = telemetry;
        self
    }

    /// `dagq.toml`'s `[telemetry]` and `[[telemetry.commands]]`: the
    /// defaults and no project rule without them.
    pub fn telemetry(&self) -> &TelemetrySettings {
        &self.telemetry
    }

    /// `declarations` resolved against `registry`; none is
    /// [`Self::none`], so no profile works. A profile the registry does
    /// not have is a mistake of the file.
    pub fn resolve(
        registry: &ProfileRegistry,
        declarations: &[ToolchainDeclaration],
    ) -> Result<Self, String> {
        let declared = declarations
            .iter()
            .map(|declaration| {
                let profile = registry.get(&declaration.profile).ok_or_else(|| {
                    format!(
                        "dagq.toml:{}: [toolchains.{}]: unknown profile {:?}; the profiles are {}",
                        declaration.line,
                        declaration.name,
                        declaration.profile,
                        registry.names()
                    )
                })?;
                Ok(Declared {
                    name: declaration.name.clone(),
                    profile,
                    paths: declaration.paths.clone(),
                })
            })
            .collect::<Result<_, String>>()?;
        Ok(Self {
            declared,
            marks: Vec::new(),
            telemetry: TelemetrySettings::default(),
        })
    }

    /// The profiles that read a verification command's output: every one
    /// declared, in the order declared, whatever its paths, since the
    /// commands run over the whole repository (ADR-t2268-1 decision 6).
    pub fn profiles(&self) -> impl Iterator<Item = &'static dyn ToolchainProfile> + '_ {
        self.declared.iter().map(|declared| declared.profile)
    }

    /// The environment the flaky retry adds: that of the profile named
    /// `profile`, the one that read the failure as flaky, and nothing for
    /// a profile not in use.
    pub fn flaky_retry_env(&self, profile: &str) -> Vec<(String, String)> {
        self.profiles()
            .find(|found| found.name() == profile)
            .map(|found| {
                found
                    .flaky_retry_env()
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// How the CI watch names the test cases of a run's JUnit: as the
    /// first profile declared that reads JUnit does; `None` when none
    /// does, and the watch lists the failed jobs and steps instead.
    pub fn junit_test_name(&self) -> Option<JunitTestName> {
        self.profiles()
            .find_map(|profile| profile.junit_test_name())
    }

    /// The kind of a process of a declared profile's tools, by the first
    /// profile declared that knows it; `None` when none does.
    pub fn process_kind(&self, command: &str, name: &str) -> Option<ToolProcess> {
        self.profiles()
            .find_map(|profile| profile.process_kind(command, name))
    }

    /// Whether the host's Rust toolchain (`rustc -vV`'s release and host)
    /// is recorded at a claim and the KPIs' `toolchain` axis has strata:
    /// when the Rust profile is declared (whatever its paths, since the
    /// toolchain is asked once for the main checkout), and never while the
    /// repository declares none, dagq's source or not (ADR-t2268-1
    /// decisions 2 and 3).
    pub fn records_rust_toolchain(&self) -> bool {
        self.profiles()
            .any(|profile| profile.name() == rust::RUST.name())
    }

    /// The lines of `text`, the file at `path`, that are in a test, as
    /// the first profile whose declaration's paths match `path` (one
    /// without paths always does) and tells them
    /// apart says ([`ToolchainProfile::test_lines`], ADR-t2589-1 decision
    /// 6); `None` when none does, and a changed line there counts as a
    /// test's.
    pub fn test_lines(&self, path: &str, text: &str) -> Option<Vec<TestLines>> {
        self.declared
            .iter()
            .filter(|declared| {
                declared.paths.is_empty()
                    || declared.paths.iter().any(|glob| glob_matches(glob, path))
            })
            .find_map(|declared| declared.profile.test_lines(path, text))
    }

    /// The directories the cleanup of a run's worktree removes: those of
    /// every profile declared ([`ToolchainProfile::worktree_build_dirs`]),
    /// each once, in the order declared, and none while no profile is
    /// declared. A declaration's paths do not narrow them, unlike
    /// [`Self::build_runs`]: those say which files of a tree a command
    /// builds an output of its own for, while a worktree's directories
    /// are what a worker's turn and its checks built there, run over the
    /// whole repository; a directory absent costs nothing, and one Git
    /// tracks a file in stays.
    pub fn worktree_build_dirs(&self) -> Vec<&'static str> {
        let mut dirs: Vec<&'static str> = Vec::new();
        for dir in self
            .profiles()
            .flat_map(|profile| profile.worktree_build_dirs().iter().copied())
        {
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        dirs
    }

    /// The runs of a command the runtime runs over a tree of the
    /// repository, each the build outputs it builds into: those of the
    /// declarations whose paths match a file of the tree (a declaration
    /// without paths always does), each in a
    /// directory of its own ([`BuildOutput::declared`], ADR-t2268-1
    /// decision 6), in the order declared. One run gives a variable one
    /// value, so the command runs as many times as the most declarations
    /// that match and set one variable: once when they set different
    /// variables. Each declaration that matches is in one run; a variable
    /// that fewer of them set keeps its last one in the later runs, so no
    /// run builds where `[run.env]` says while a declaration matches. With
    /// no declaration that matches, one run with no output. `tree` lists
    /// the tree's files, and is called only when a declaration has paths.
    pub fn build_runs<E>(
        &self,
        tree: impl FnOnce() -> Result<Vec<String>, E>,
    ) -> Result<Vec<Vec<BuildDir>>, E> {
        let files = if self.declared.iter().any(|d| !d.paths.is_empty()) {
            tree()?
        } else {
            Vec::new()
        };
        let matching = self.declared.iter().filter(|declared| {
            declared.paths.is_empty()
                || files
                    .iter()
                    .any(|file| declared.paths.iter().any(|glob| glob_matches(glob, file)))
        });
        // The outputs of each variable, in the order declared.
        let mut by_env: Vec<Vec<BuildDir>> = Vec::new();
        for declared in matching {
            let Some(output) = declared.profile.build_output() else {
                continue;
            };
            let output = output.declared(&declared.name, declared.profile.name());
            match by_env.iter_mut().find(|same| same[0].env == output.env) {
                Some(same) => same.push(output),
                None => by_env.push(vec![output]),
            }
        }
        let runs = by_env.iter().map(Vec::len).max().unwrap_or(1);
        Ok((0..runs)
            .map(|run| {
                by_env
                    .iter()
                    .map(|same| same[run.min(same.len() - 1)].clone())
                    .collect()
            })
            .collect())
    }

    /// What `doctor` and `status` show: each declaration with its profile
    /// and the paths it applies to (`**` for the whole repository), none
    /// while the repository declares none; and `marks`, the project's own
    /// marks the output is read with, each with its `name`, the `class` it
    /// gives and the shape of its `line`.
    pub fn view(&self) -> Value {
        let declared: Vec<Value> = self
            .declared
            .iter()
            .map(|declared| {
                json!({
                    "name": declared.name,
                    "profile": declared.profile.name(),
                    "paths": if declared.paths.is_empty() {
                        vec!["**".to_owned()]
                    } else {
                        declared.paths.clone()
                    },
                })
            })
            .collect();
        json!({
            "declared": declared,
            "marks": self.marks.iter().map(|mark| json!({
                "name": mark.name,
                "class": mark.class.as_str(),
                "line": mark.line.as_str(),
            })).collect::<Vec<_>>(),
        })
    }
}

/// [`Toolchains::view`] of a declaration that could be read, else the
/// mistake as `error` alone: a profile no registry has, or a `dagq.toml`
/// that cannot be read, which stops every landing with an error meanwhile,
/// so `doctor` shows it first.
pub fn view(toolchains: Result<Toolchains, String>) -> Value {
    match toolchains {
        Ok(toolchains) => toolchains.view(),
        Err(error) => json!({"error": error}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration(name: &str, profile: &str, paths: &[&str]) -> ToolchainDeclaration {
        ToolchainDeclaration {
            name: name.to_owned(),
            profile: profile.to_owned(),
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            line: 3,
        }
    }

    #[test]
    fn the_builtin_registry_has_the_rust_profile_by_name() {
        assert_eq!(BUILTIN.get("rust").map(|p| p.name()), Some("rust"));
        assert!(BUILTIN.get("typescript").is_none());
    }

    /// No declaration is no profile and no mark, in dagq's source too
    /// (ADR-t2268-1 decision 3): nothing of a language reads the output,
    /// and `doctor` and `status` show it with no warning.
    #[test]
    fn no_declaration_has_no_profile() {
        let toolchains = Toolchains::resolve(&BUILTIN, &[]).unwrap();
        assert_eq!(toolchains.profiles().count(), 0);
        assert!(toolchains.marks().is_empty());
        assert_eq!(toolchains.view(), json!({"declared": [], "marks": []}));
        assert!(toolchains.flaky_retry_env("rust").is_empty());
        assert!(toolchains.junit_test_name().is_none());
        assert_eq!(toolchains.process_kind("cargo", "cargo"), None);
        assert!(!toolchains.records_rust_toolchain());
        // A declared mark is read without a profile.
        let marked = toolchains.with_marks(vec![crate::domain::verify_marks::ProjectMark {
            name: "hung".to_owned(),
            class: crate::domain::verify_marks::MarkClass::parse("killed").unwrap(),
            line: crate::domain::verify_marks::LinePattern::parse("HUNG {test} now").unwrap(),
        }]);
        assert_eq!(
            marked.view()["marks"],
            json!([{"name": "hung", "class": "killed", "line": "HUNG {test} now"}])
        );
        assert_eq!(marked.profiles().count(), 0);
    }

    #[test]
    fn declarations_resolve_in_order_and_show_their_paths() {
        let toolchains = Toolchains::resolve(
            &BUILTIN,
            &[
                declaration("core", "rust", &["src/**", "Cargo.toml"]),
                declaration("tools", "rust", &[]),
            ],
        )
        .unwrap();
        assert_eq!(toolchains.profiles().count(), 2);
        assert_eq!(
            toolchains.view(),
            json!({
                "declared": [
                    {"name": "core", "profile": "rust", "paths": ["src/**", "Cargo.toml"]},
                    {"name": "tools", "profile": "rust", "paths": ["**"]},
                ],
                "marks": [],
            })
        );
    }

    /// The worktree's build outputs are every declared profile's, each
    /// once, whatever the declarations' paths; none with no profile.
    #[test]
    fn the_worktree_build_dirs_are_every_declared_profiles() {
        assert!(Toolchains::none().worktree_build_dirs().is_empty());
        let both = Toolchains::resolve(
            &fake::REGISTRY,
            &[
                declaration("core", "rust", &["src/**"]),
                declaration("web", "fake", &["web/**"]),
                declaration("tools", "rust", &[]),
            ],
        )
        .unwrap();
        assert_eq!(
            both.worktree_build_dirs(),
            ["target", "llvm-cov-target", "fake-out", "fake-cache"]
        );
    }

    #[test]
    fn an_unknown_profile_is_a_mistake_of_the_file() {
        let error = Toolchains::resolve(&BUILTIN, &[declaration("web", "typescript", &[])])
            .err()
            .unwrap();
        assert_eq!(
            error,
            "dagq.toml:3: [toolchains.web]: unknown profile \"typescript\"; the profiles are rust"
        );
        assert_eq!(view(Err(error.clone())), json!({"error": error}));
    }

    /// The measures read with what the declaration resolved to, and with
    /// no profile a queue bound to no repository and a declaration that
    /// cannot be read or resolved (an unreadable file, a mistake of the
    /// file, a main checkout not found).
    #[test]
    fn the_measures_read_with_the_resolution_or_with_none() {
        let names = |toolchains: Toolchains| {
            toolchains
                .profiles()
                .map(|profile| profile.name())
                .collect::<Vec<_>>()
        };
        assert!(names(Toolchains::for_measures::<String>(None)).is_empty());
        let declared = Toolchains::resolve(&BUILTIN, &[declaration("core", "rust", &[])]).unwrap();
        assert_eq!(
            names(Toolchains::for_measures::<String>(Some(Ok(declared)))),
            ["rust"]
        );
        assert_eq!(
            names(Toolchains::for_measures::<String>(Some(Ok(
                fake::only_fake()
            )))),
            ["fake"]
        );
        let unresolved = Toolchains::resolve(&BUILTIN, &[declaration("web", "typescript", &[])]);
        let none = Toolchains::for_measures(Some(unresolved));
        assert!(names(none.clone()).is_empty());
        assert!(none.marks().is_empty());
        let unreadable = Toolchains::for_measures(Some(Err("dagq.toml: Is a directory")));
        assert!(names(unreadable).is_empty());
    }

    #[test]
    fn the_flaky_retry_env_is_only_the_named_profiles_in_use() {
        let toolchains = Toolchains::this_repository();
        assert_eq!(
            toolchains.flaky_retry_env("rust"),
            rust::RUST
                .flaky_retry_env()
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect::<Vec<_>>()
        );
        assert!(toolchains.flaky_retry_env("other").is_empty());
    }

    fn build_dir(dir: &str, env: &'static str) -> BuildDir {
        BuildDir {
            dir: dir.to_owned(),
            env,
        }
    }

    /// The runs of a command over a tree (ADR-t2268-1 decision 6): one
    /// with no output while none is declared, without listing the tree;
    /// the profile's own directory for a declaration named after it and
    /// without paths, without listing the tree either; else one directory
    /// per declaration whose paths match a file of the tree, as many runs
    /// as the most matching declarations of one variable, and one run
    /// with none when none matches.
    #[test]
    fn the_build_runs_give_each_matching_declaration_its_directory() {
        let rust = rust::BUILD_OUTPUT.env;
        let fake = fake::BUILD_OUTPUT.env;
        let unlisted = || -> Result<Vec<String>, ()> { panic!("the tree is not listed") };
        assert_eq!(
            Toolchains::resolve(&BUILTIN, &[])
                .unwrap()
                .build_runs(unlisted),
            Ok(vec![vec![]])
        );
        assert_eq!(
            Toolchains::this_repository().build_runs(unlisted),
            Ok(vec![vec![build_dir("target", rust)]])
        );
        let tree = || -> Result<Vec<String>, ()> {
            Ok(vec!["src/lib.rs".to_owned(), "web/app.ts".to_owned()])
        };
        let declared = |declarations: &[ToolchainDeclaration]| {
            Toolchains::resolve(&fake::REGISTRY, declarations).unwrap()
        };
        // Named after its profile: the profile's directory.
        assert_eq!(
            declared(&[declaration("rust", "rust", &["src/**"])]).build_runs(tree),
            Ok(vec![vec![build_dir("target", rust)]])
        );
        // Two declarations of the Rust profile match: a run for each, the
        // fake profile's one declaration in both.
        assert_eq!(
            declared(&[
                declaration("core", "rust", &["src/**"]),
                declaration("tools", "rust", &[]),
                declaration("web", "fake", &["web/**"]),
            ])
            .build_runs(tree),
            Ok(vec![
                vec![
                    build_dir("target-core", rust),
                    build_dir("fake-out-web", fake)
                ],
                vec![
                    build_dir("target-tools", rust),
                    build_dir("fake-out-web", fake)
                ],
            ])
        );
        assert_eq!(
            declared(&[
                declaration("core", "rust", &["crates/**"]),
                declaration("tools", "rust", &[]),
                declaration("web", "fake", &["web/**"]),
            ])
            .build_runs(tree),
            Ok(vec![vec![
                build_dir("target-tools", rust),
                build_dir("fake-out-web", fake)
            ]])
        );
        assert_eq!(
            declared(&[declaration("core", "rust", &["**/*.rs"])])
                .build_runs(|| Ok::<_, ()>(vec!["README.md".to_owned()])),
            Ok(vec![vec![]])
        );
        assert_eq!(
            fake::only_fake().build_runs(unlisted),
            Ok(vec![vec![build_dir("fake-out-made-up", fake)]])
        );
        assert_eq!(
            declared(&[declaration("core", "rust", &["src/**"])]).build_runs(|| Err("no tree")),
            Err("no tree")
        );
    }

    /// The cleanup clears the directory of any declaration of a registered
    /// profile, whatever is declared now, and nothing else the runtime
    /// keeps beside them.
    #[test]
    fn the_registry_knows_each_declarations_build_directory() {
        for name in ["target", "target-core", "target-tools"] {
            assert!(BUILTIN.holds_build_dir(name), "{name}");
        }
        for name in [
            "worktree",
            "lock",
            "dagq-rustc-wrapper",
            "target-",
            "targets",
            "fake-out",
            "fake-out-web",
        ] {
            assert!(!BUILTIN.holds_build_dir(name), "{name}");
        }
        assert!(fake::REGISTRY.holds_build_dir("target-core"));
        assert!(fake::REGISTRY.holds_build_dir("fake-out"));
        assert!(fake::REGISTRY.holds_build_dir("fake-out-web"));
        assert!(!fake::REGISTRY.holds_build_dir("worktree"));
    }

    #[test]
    fn the_rust_toolchain_is_recorded_by_the_declared_profiles() {
        // Undeclared: none (ADR-t2268-1 decision 3).
        assert!(
            !Toolchains::resolve(&BUILTIN, &[])
                .unwrap()
                .records_rust_toolchain()
        );
        // Declared: the Rust profile, whatever its paths.
        let rust =
            Toolchains::resolve(&BUILTIN, &[declaration("core", "rust", &["src/**"])]).unwrap();
        assert!(rust.records_rust_toolchain());
        let both = Toolchains::resolve(
            &fake::REGISTRY,
            &[
                declaration("tool", "fake", &[]),
                declaration("core", "rust", &[]),
            ],
        )
        .unwrap();
        assert!(both.records_rust_toolchain());
        // No Rust profile resolved: none.
        assert!(!fake::only_fake().records_rust_toolchain());
        assert!(!Toolchains::none().records_rust_toolchain());
    }

    #[test]
    fn the_junit_names_and_the_processes_are_the_first_declared_profiles() {
        let rust_first = Toolchains::resolve(
            &fake::REGISTRY,
            &[
                declaration("core", "rust", &[]),
                declaration("tool", "fake", &[]),
            ],
        )
        .unwrap();
        let name = rust_first.junit_test_name().unwrap();
        assert_eq!(name("dagq::it", "a::b"), "dagq::it a::b");
        assert_eq!(
            rust_first.process_kind("/x/fakec", "fakec"),
            Some(ToolProcess::Compiler)
        );
        assert_eq!(
            rust_first.process_kind("cargo", "cargo"),
            Some(ToolProcess::Build)
        );
        let fake_only = fake::only_fake();
        assert_eq!(
            fake_only.junit_test_name().unwrap()("suite", "t"),
            "suite/t"
        );
        assert_eq!(fake_only.process_kind("cargo", "cargo"), None);
        assert_eq!(
            fake_only.process_kind("fakebuild", "fakebuild"),
            Some(ToolProcess::Build)
        );
        let none = Toolchains::none();
        assert!(none.junit_test_name().is_none());
        assert_eq!(none.process_kind("rustc", "rustc"), None);
    }

    #[test]
    fn a_profile_names_only_classes_the_runtime_has() {
        // The host's classes a profile may name are the runtime's own:
        // the set is not widened (ADR-t639-1 decision 5).
        for class in [ToolHostClass::Killed, ToolHostClass::Timeout] {
            assert!(FailureClass::from(class).is_environmental(), "{class:?}");
        }
        for class in [
            ToolCodeClass::BuildError,
            ToolCodeClass::Lint,
            ToolCodeClass::TestFailure,
            ToolCodeClass::Format,
            ToolCodeClass::CoverageBelow,
        ] {
            assert!(!FailureClass::from(class).is_environmental(), "{class:?}");
        }
    }
}
