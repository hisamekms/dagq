//! The review's subagents (ADR-t1453-1): `dagq.toml` names each one with
//! `[review.subagents.<agent>] paths`, and a review must run every agent
//! one of whose globs a path the reviewed commit changes matches. Which
//! agents a review needs is decided here, without side effects; the
//! supervisor reads the configuration and the definitions from the
//! landing branch's committed tree.

use super::scope::glob_matches;

/// Where an agent's definition is, relative to the repository root
/// (ADR-t1453-1 decision 2).
pub const DEFINITION_DIR: &str = ".dagq/review-agents";

/// One `[review.subagents.<agent>]`: the agent's name and the globs that
/// make it required, each once, in the order written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSubagent {
    pub name: String,
    pub paths: Vec<String>,
}

/// An agent a review needs, with the changed paths that made it so, in
/// the order of the changed paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedAgent {
    pub name: String,
    pub matched: Vec<String>,
}

/// Whether `name` is kebab-case: lowercase ASCII letters and digits in
/// words joined by single `-`, as a file name of the definition can be.
pub fn valid_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('-').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// The repository-relative path of `agent`'s definition.
pub fn definition_path(agent: &str) -> String {
    format!("{DEFINITION_DIR}/{agent}.md")
}

/// The agents of `configured` one of whose globs matches a path of
/// `changed`, each once, in the order configured. `changed` is the
/// reviewed range's paths as Git lists them without rename detection:
/// both sides of a rename and the old path of a deletion, so either side
/// selects an agent (ADR-t1453-1 decision 3). Nothing configured selects
/// nothing.
pub fn select(configured: &[ReviewSubagent], changed: &[String]) -> Vec<SelectedAgent> {
    let mut selected: Vec<SelectedAgent> = Vec::new();
    for agent in configured {
        if selected.iter().any(|s| s.name == agent.name) {
            continue;
        }
        let mut matched: Vec<String> = Vec::new();
        for path in changed {
            if !matched.contains(path) && agent.paths.iter().any(|glob| glob_matches(glob, path)) {
                matched.push(path.clone());
            }
        }
        if !matched.is_empty() {
            selected.push(SelectedAgent {
                name: agent.name.clone(),
                matched,
            });
        }
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    fn agent(name: &str, paths: &[&str]) -> ReviewSubagent {
        ReviewSubagent {
            name: name.to_owned(),
            paths: strings(paths),
        }
    }

    #[test]
    fn an_agent_is_selected_once_with_every_path_it_matches() {
        let configured = [
            agent("design", &["src/**", "docs/design/**"]),
            agent("migrations", &["migrations/*.sql"]),
            agent("plugin", &["plugins/**"]),
        ];
        let changed = strings(&[
            "src/lib.rs",
            "docs/design/review.md",
            "src/a/b.rs",
            "README.md",
        ]);
        assert_eq!(
            select(&configured, &changed),
            vec![SelectedAgent {
                name: "design".into(),
                matched: strings(&["src/lib.rs", "docs/design/review.md", "src/a/b.rs"]),
            }]
        );
        // A path two globs of one agent match counts once.
        let overlapping = [agent("design", &["src/**", "**/*.rs"])];
        assert_eq!(
            select(&overlapping, &strings(&["src/lib.rs", "src/lib.rs"])),
            vec![SelectedAgent {
                name: "design".into(),
                matched: strings(&["src/lib.rs"]),
            }]
        );
        assert!(select(&configured, &strings(&["README.md"])).is_empty());
    }

    #[test]
    fn either_side_of_a_rename_and_a_deleted_path_select_an_agent() {
        let configured = [agent("migrations", &["migrations/**"])];
        // `git diff --no-renames` lists a rename as the deleted old path
        // and the added new one.
        let only_old = strings(&["migrations/0001_a.sql", "attic/0001_a.sql"]);
        let only_new = strings(&["attic/b.sql", "migrations/b.sql"]);
        let deleted = strings(&["migrations/0002_b.sql"]);
        for (changed, path) in [
            (only_old, "migrations/0001_a.sql"),
            (only_new, "migrations/b.sql"),
            (deleted, "migrations/0002_b.sql"),
        ] {
            assert_eq!(
                select(&configured, &changed),
                vec![SelectedAgent {
                    name: "migrations".into(),
                    matched: strings(&[path]),
                }]
            );
        }
    }

    #[test]
    fn nothing_configured_selects_nothing_and_names_are_kebab_case() {
        assert!(select(&[], &strings(&["src/lib.rs"])).is_empty());
        for name in ["design", "design-consistency", "a1-b2"] {
            assert!(valid_agent_name(name), "{name}");
        }
        for name in ["", "Design", "-a", "a-", "a--b", "a_b", "a.b", "a/b"] {
            assert!(!valid_agent_name(name), "{name}");
        }
        assert_eq!(definition_path("design"), ".dagq/review-agents/design.md");
    }
}
