//! The toolchain profiles (ADR-t2268-1): the knowledge of one language's
//! tools behind one port, [`ToolchainProfile`], which only its
//! implementations hold. The runtime builds them in ([`BUILTIN`], the one
//! place a name is looked up), and a repository declares in `dagq.toml`
//! which to use ([`ToolchainDeclaration`], resolved to [`Toolchains`]).
//!
//! What a profile reads so far is a verification command's output: the
//! marks of its tools that put a failure down to a class of
//! [`FailureClass`], the tests it names as failed and as passed on a retry,
//! and the environment a retry of flaky tests runs with. The runtime's own
//! marks (a full disk, a signal, the conventional exit of a timeout, the
//! limit of the whole command) and the order the marks are tried in are
//! [`crate::domain::verify_failure`]'s, and work whatever is declared.
//!
//! A new language is one implementation of [`ToolchainProfile`] and one
//! line in [`BUILTIN`].
use serde_json::{Value, json};

use super::verify_failure::FailureClass;

pub mod rust;

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

/// The knowledge of one language's tools (ADR-t2268-1 decision 1). Each
/// method reads a verification command's output without the terminal's
/// colors: `text` whole, or `lines`, its lines trimmed (the same lines,
/// so an index of one is a line of the other). The runtime calls them in
/// the order [`crate::domain::verify_failure::classify`] keeps, each
/// declared profile in turn, and takes the first answer.
pub trait ToolchainProfile: Sync {
    /// The name a declaration's `profile` names it by.
    fn name(&self) -> &'static str;

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
/// declares, in the order declared, or, while it declares none, the Rust
/// profile implied (ADR-t2268-1 decision 9).
#[derive(Clone)]
pub struct Toolchains {
    declared: Vec<Declared>,
    implicit: bool,
}

impl Toolchains {
    /// A repository that declares none: the Rust profile, implied while the
    /// runtime moves to declarations (ADR-t2268-1 decision 9).
    pub fn undeclared() -> Self {
        Self {
            declared: vec![Declared {
                name: rust::RUST.name().to_owned(),
                profile: &rust::RUST,
                paths: Vec::new(),
            }],
            implicit: true,
        }
    }

    /// `declarations` resolved against `registry`; none is
    /// [`Self::undeclared`]. A profile the registry does not have is a
    /// mistake of the file.
    pub fn resolve(
        registry: &ProfileRegistry,
        declarations: &[ToolchainDeclaration],
    ) -> Result<Self, String> {
        if declarations.is_empty() {
            return Ok(Self::undeclared());
        }
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
            implicit: false,
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

    /// What `doctor` and `status` show: each declaration with its profile
    /// and the paths it applies to (`**` for the whole repository), the
    /// profile implied while none is declared, and a warning then.
    pub fn view(&self) -> Value {
        let declared: Vec<Value> = if self.implicit {
            Vec::new()
        } else {
            self.declared
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
                .collect()
        };
        json!({
            "declared": declared,
            "implicit": self.implicit.then(|| rust::RUST.name()),
            "warning": self.implicit.then_some(UNDECLARED_WARNING),
        })
    }
}

/// `doctor`'s and `status`'s warning while a repository declares no
/// profile (ADR-t2268-1 decision 9).
pub const UNDECLARED_WARNING: &str = "dagq.toml declares no [toolchains.<name>]: the Rust profile reads the verification's output until the repository declares the profiles it uses";

/// [`Toolchains::view`] of a declaration that could be read, else the
/// mistake as `error`.
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

    #[test]
    fn no_declaration_implies_rust_and_warns() {
        let toolchains = Toolchains::resolve(&BUILTIN, &[]).unwrap();
        assert_eq!(
            toolchains.profiles().map(|p| p.name()).collect::<Vec<_>>(),
            ["rust"]
        );
        assert_eq!(
            toolchains.view(),
            json!({"declared": [], "implicit": "rust", "warning": UNDECLARED_WARNING})
        );
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
                "implicit": null,
                "warning": null,
            })
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

    #[test]
    fn the_flaky_retry_env_is_only_the_named_profiles_in_use() {
        let toolchains = Toolchains::undeclared();
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
