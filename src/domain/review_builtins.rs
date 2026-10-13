//! The built-in review agents (ADR-t2259-1 decisions 1, 4, 5, 6, 8 and 9,
//! as ADR-t2389-1 decision 1 amends them): agents whose definitions the
//! binary carries, configured in `dagq.toml` by
//! `[review.subagents."builtin:<name>"]` and selected and run as a
//! project's agents are. Each definition is the file
//! `src/domain/review_builtins/<name>.md` of this source tree, taken into
//! the binary by `include_str!`: written for any project, it names no
//! language, tool or document rule of one, and no instructions file to
//! look for. A project's rules for one are its file [`rules_path`] on the
//! landing branch's commit, which the agent's job reads after the
//! definition; a project never replaces a definition, it turns the
//! built-in off and writes its own agent.

use serde_json::{Value, json};

use super::Provider;

/// The prefix that names a built-in: no project's agent may take it
/// (a name with it that is no built-in's is a mistake of `dagq.toml`).
pub const PREFIX: &str = "builtin:";

/// The whole review's job (ADR-t2259-1 decision 3): always run, on every
/// change, on `[roles.review]`'s provider. Its table may say only
/// `enabled = true`; `enabled = false`, `paths` and `provider` are
/// mistakes. It is no agent job of [`BuiltinAgent`]: its prompt is the
/// runtime's, and the project's rules for it are
/// [`acceptance_rules_path`].
pub const ACCEPTANCE: &str = "builtin:acceptance";

/// Where a project's rules for the built-ins are, relative to the
/// repository root.
pub const RULES_DIR: &str = ".dagq/review";

/// Globs of files that run or build code in most projects: sources of the
/// common languages, shell and build scripts, container files and CI
/// workflows. Both built-ins start from them: a change of documents,
/// plain text or data only selects neither.
const CODE_PATHS: &[&str] = &[
    "**/*.c",
    "**/*.cc",
    "**/*.cpp",
    "**/*.cs",
    "**/*.go",
    "**/*.h",
    "**/*.hpp",
    "**/*.java",
    "**/*.js",
    "**/*.jsx",
    "**/*.kt",
    "**/*.lua",
    "**/*.mjs",
    "**/*.cjs",
    "**/*.php",
    "**/*.pl",
    "**/*.ps1",
    "**/*.py",
    "**/*.rb",
    "**/*.rs",
    "**/*.scala",
    "**/*.sh",
    "**/*.bash",
    "**/*.swift",
    "**/*.ts",
    "**/*.tsx",
    "**/Dockerfile",
    "**/Containerfile",
    "**/Makefile",
    ".github/workflows/**",
];

/// A built-in agent the binary carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinAgent {
    /// `builtin:security`: what the change does about sending data out,
    /// running commands and handling credentials. On by default, on
    /// [`CODE_PATHS`].
    Security,
    /// `builtin:tests`: whether the tests cover the decisions the change
    /// makes and are stable. Off by default; on [`CODE_PATHS`] when on.
    Tests,
}

impl BuiltinAgent {
    /// Every built-in agent, in the order a review selects them after the
    /// project's agents.
    pub const ALL: [Self; 2] = [Self::Security, Self::Tests];

    /// Its name without [`PREFIX`].
    pub const fn short(self) -> &'static str {
        match self {
            Self::Security => "security",
            Self::Tests => "tests",
        }
    }

    /// Its name, `builtin:<short>`: the name of its table, its job and its
    /// result in the review's verdict.
    pub fn name(self) -> String {
        format!("{PREFIX}{}", self.short())
    }

    /// Whether it runs when `dagq.toml` does not say `enabled`.
    pub const fn enabled_by_default(self) -> bool {
        match self {
            Self::Security => true,
            Self::Tests => false,
        }
    }

    /// The globs that select it when `dagq.toml` does not say `paths`;
    /// `paths` written replaces them, it is not added to them.
    pub const fn default_paths(self) -> &'static [&'static str] {
        CODE_PATHS
    }

    /// Its definition as the binary carries it, frontmatter included.
    pub const fn definition(self) -> &'static str {
        match self {
            Self::Security => include_str!("review_builtins/security.md"),
            Self::Tests => include_str!("review_builtins/tests.md"),
        }
    }

    /// The built-in agent `name` names: `Ok(None)` for a name without
    /// [`PREFIX`] (a project's agent), `Err` with why for one with it that
    /// names no built-in agent ([`ACCEPTANCE`] is read apart).
    pub fn named(name: &str) -> Result<Option<Self>, String> {
        let Some(short) = name.strip_prefix(PREFIX) else {
            return Ok(None);
        };
        Self::ALL
            .into_iter()
            .find(|agent| agent.short() == short)
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "{name} is no built-in agent (they are {}); names starting with {PREFIX} are the built-ins', so a project's agent may not take one",
                    std::iter::once(ACCEPTANCE.to_owned())
                        .chain(Self::ALL.map(Self::name))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }
}

/// The repository-relative path of the project's rules for `agent`:
/// `.dagq/review/builtin-<short>.md`.
pub fn rules_path(agent: BuiltinAgent) -> String {
    format!("{RULES_DIR}/builtin-{}.md", agent.short())
}

/// The repository-relative path of the project's rules for
/// [`ACCEPTANCE`], the whole review: `.dagq/review/builtin-acceptance.md`.
pub fn acceptance_rules_path() -> String {
    format!("{RULES_DIR}/builtin-acceptance.md")
}

/// One `[review.subagents."builtin:<name>"]` as written: each key `None`
/// when the table does not have it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinSetting {
    pub agent: BuiltinAgent,
    pub enabled: Option<bool>,
    pub paths: Option<Vec<String>>,
    /// As a project's agent's `provider`.
    pub provider: Option<Provider>,
}

/// What a built-in agent is with `dagq.toml`'s `setting` of it, if any,
/// over its defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveBuiltin {
    pub agent: BuiltinAgent,
    pub enabled: bool,
    pub paths: Vec<String>,
    /// Whether `paths` is the one `dagq.toml` wrote.
    pub paths_written: bool,
    pub provider: Option<Provider>,
}

impl EffectiveBuiltin {
    /// Each built-in agent in [`BuiltinAgent::ALL`]'s order as `settings`
    /// (those `dagq.toml` writes) make it.
    pub fn all(settings: &[BuiltinSetting]) -> Vec<Self> {
        BuiltinAgent::ALL
            .into_iter()
            .map(|agent| {
                let setting = settings.iter().find(|s| s.agent == agent);
                let written = setting.and_then(|s| s.paths.clone());
                Self {
                    agent,
                    enabled: setting
                        .and_then(|s| s.enabled)
                        .unwrap_or(agent.enabled_by_default()),
                    paths_written: written.is_some(),
                    paths: written.unwrap_or_else(|| {
                        agent
                            .default_paths()
                            .iter()
                            .map(|glob| (*glob).to_owned())
                            .collect()
                    }),
                    provider: setting.and_then(|s| s.provider),
                }
            })
            .collect()
    }

    /// What `dagq doctor`'s `agents` says of it: its name, whether it
    /// runs and whether it does by default, its globs and whether
    /// `dagq.toml` wrote them, its provider, and the project's rules file
    /// found on the landing branch's commit (`rules`, `null` without one).
    pub fn doctor_value(&self, rules: Option<&str>) -> Value {
        json!({
            "agent": self.agent.name(),
            "enabled": self.enabled,
            "enabled_by_default": self.agent.enabled_by_default(),
            "paths": self.paths,
            "paths_written": self.paths_written,
            "provider": self.provider,
            "rules": rules,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setting(agent: BuiltinAgent) -> BuiltinSetting {
        BuiltinSetting {
            agent,
            enabled: None,
            paths: None,
            provider: None,
        }
    }

    /// Without a setting, security is on and tests off, both on the
    /// default globs; `paths` written replaces them, `enabled` and
    /// `provider` are taken as written (ADR-t2259-1 decision 5).
    #[test]
    fn a_setting_replaces_the_defaults_it_writes() {
        let defaults = EffectiveBuiltin::all(&[]);
        assert_eq!(
            defaults
                .iter()
                .map(|b| (b.agent, b.enabled, b.paths_written))
                .collect::<Vec<_>>(),
            [
                (BuiltinAgent::Security, true, false),
                (BuiltinAgent::Tests, false, false)
            ]
        );
        assert!(defaults[0].paths.iter().any(|p| p == "**/*.py"));
        let written = EffectiveBuiltin::all(&[
            BuiltinSetting {
                enabled: Some(false),
                ..setting(BuiltinAgent::Security)
            },
            BuiltinSetting {
                enabled: Some(true),
                paths: Some(vec!["lib/**".into()]),
                provider: Some(Provider::Codex),
                ..setting(BuiltinAgent::Tests)
            },
        ]);
        assert!(!written[0].enabled);
        assert_eq!(written[1].paths, ["lib/**"]);
        assert!(written[1].enabled && written[1].paths_written);
        assert_eq!(written[1].provider, Some(Provider::Codex));
        assert_eq!(
            written[1].doctor_value(Some(".dagq/review/builtin-tests.md")),
            json!({"agent": "builtin:tests", "enabled": true, "enabled_by_default": false,
                   "paths": ["lib/**"], "paths_written": true, "provider": "codex",
                   "rules": ".dagq/review/builtin-tests.md"})
        );
    }

    /// A name with the prefix names a built-in or is a mistake; docs is
    /// not carried (ADR-t2389-1 decision 1).
    #[test]
    fn names_with_the_prefix_are_the_builtins() {
        assert_eq!(
            BuiltinAgent::named("builtin:security"),
            Ok(Some(BuiltinAgent::Security))
        );
        assert_eq!(BuiltinAgent::named("design"), Ok(None));
        for unknown in ["builtin:docs", "builtin:", "builtin:Security"] {
            let error = BuiltinAgent::named(unknown).unwrap_err();
            assert!(error.contains("no built-in agent"), "{error}");
            assert!(
                error.contains("builtin:acceptance, builtin:security, builtin:tests"),
                "{error}"
            );
        }
        assert_eq!(
            rules_path(BuiltinAgent::Tests),
            ".dagq/review/builtin-tests.md"
        );
        assert_eq!(
            acceptance_rules_path(),
            ".dagq/review/builtin-acceptance.md"
        );
    }

    /// The definitions are generic: no instructions file to look for, and
    /// no language, tool or document rule of one project (ADR-t2259-1
    /// decision 6, goal 188's constraints).
    #[test]
    fn the_definitions_name_no_project_of_their_own() {
        for agent in BuiltinAgent::ALL {
            let definition = agent.definition();
            assert!(definition.starts_with("---\n"), "{definition}");
            let lower = definition.to_lowercase();
            for word in [
                "agents.md",
                "claude.md",
                "instructions",
                "rust",
                "cargo",
                "clippy",
                "docs/",
                "adr",
                "dagq",
            ] {
                assert!(!lower.contains(word), "{word} in {}", agent.name());
            }
        }
    }
}
