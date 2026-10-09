//! The program reviews of a run's review stage (ADR-t1895-2): `dagq.toml`
//! names each one with `[review.programs.<name>]`, the program it runs and
//! the globs that make it required, and a review runs every program one of
//! whose globs a path the reviewed commit changes matches. Which programs a
//! review needs is decided here, without side effects; the supervisor reads
//! the configuration and the scripts from the landing branch's committed
//! tree, never from the run's worktree (decision 2).

use super::scope::{glob_matches, validate_path_globs};

/// The configuration's section of the program reviews.
pub const SECTION: &str = "review.programs";

/// One `[review.programs.<name>]`: the program's name, the script it
/// runs, the globs that make it required (each once, in the order
/// written), and `timeout_secs`, its own time limit in seconds, which
/// without the key is `[review.jobs] program_timeout_secs`'s.
///
/// `script = "scripts/check.sh"` with `args = [...]` is a script of the
/// repository, its text taken from the landing branch's commit and run
/// from a copy outside the worktree against the worktree as its working
/// directory, so a worker's change to it has no effect until it lands
/// (ADR-t1895-2 decision 2). It is executed as it is, so it starts with
/// its interpreter's `#!` line; an outside tool (cargo, a linter) is
/// `exec`ed from it. There is no form that runs a program named by the
/// configuration: whether such a program, or a file an interpreter is
/// given, is the worktree's cannot be told from its words. Another
/// repository script the script calls by a path from its working
/// directory is the worktree's, which nothing here prevents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewProgram {
    pub name: String,
    pub script: String,
    pub args: Vec<String>,
    pub paths: Vec<String>,
    pub timeout_secs: Option<u64>,
}

/// A program a review needs, with the changed paths that made it so, in
/// the order of the changed paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedProgram {
    pub program: ReviewProgram,
    pub matched: Vec<String>,
}

/// Check a script's path: one repository file, relative to the root,
/// with no glob character.
pub fn check_script_path(path: &str) -> Result<(), String> {
    validate_path_globs(&[path.to_owned()]).map_err(|error| error.to_string())?;
    if path.contains(['*', '?']) {
        return Err(format!("{path:?} must name one file, not a glob"));
    }
    Ok(())
}

/// The programs `changed` requires, in the order configured, each with the
/// changed paths one of its globs matches.
pub fn select(configured: &[ReviewProgram], changed: &[String]) -> Vec<SelectedProgram> {
    configured
        .iter()
        .filter_map(|program| {
            let mut matched: Vec<String> = Vec::new();
            for path in changed {
                if !matched.contains(path) && program.paths.iter().any(|g| glob_matches(g, path)) {
                    matched.push(path.clone());
                }
            }
            (!matched.is_empty()).then(|| SelectedProgram {
                program: program.clone(),
                matched,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(name: &str, paths: &[&str]) -> ReviewProgram {
        ReviewProgram {
            name: name.to_owned(),
            script: "scripts/check.sh".to_owned(),
            args: Vec::new(),
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            timeout_secs: None,
        }
    }

    /// A program is required by the changed paths its globs match, in the
    /// order configured; one that matches nothing is not.
    #[test]
    fn the_changed_paths_select_the_programs() {
        let configured = [
            program("docs", &["docs/**", "*.md"]),
            program("never", &["nothing/**"]),
            program("src", &["src/**"]),
        ];
        let changed: Vec<String> = ["README.md", "src/a.rs", "docs/x.md", "README.md"]
            .map(str::to_owned)
            .to_vec();
        let selected = select(&configured, &changed);
        let names: Vec<(&str, Vec<&str>)> = selected
            .iter()
            .map(|s| {
                (
                    s.program.name.as_str(),
                    s.matched.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            names,
            [
                ("docs", vec!["README.md", "docs/x.md"]),
                ("src", vec!["src/a.rs"])
            ]
        );
        assert!(select(&configured, &[]).is_empty());
    }

    /// A script names one repository file.
    #[test]
    fn a_scripts_path_names_one_file_of_the_repository() {
        assert!(check_script_path("scripts/check.sh").is_ok());
        for bad in ["/bin/sh", "../x.sh", "scripts/*.sh", "a//b", " "] {
            assert!(check_script_path(bad).is_err(), "{bad}");
        }
    }
}
