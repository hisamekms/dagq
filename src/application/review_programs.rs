//! The program reviews of a run's review (ADR-t1895-2): which programs a
//! review attempt runs and what each runs, read from the landing branch's
//! committed tree when the attempt starts, never from the run's worktree
//! or the main checkout's files (decision 2). The programs then run as
//! program jobs ([`super::supervise::start_review_program`]) against the
//! run's worktree.

use anyhow::{Context, Result};

use super::Repository;
use super::review::{CONFIG_FILE, ReviewRange};
use crate::domain::CommitSha;
use crate::domain::review_programs::{self, ReviewProgram};

/// One program a review attempt runs, as the landing branch's commit has
/// it: its configuration, the changed paths that required it, and its
/// script's text at that commit, which is what runs whatever the run's
/// worktree holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotProgram {
    pub program: ReviewProgram,
    pub matched: Vec<String>,
    pub script: String,
}

/// The program reviews of one attempt read from the landing branch's
/// commit `commit`: those its range requires, none when the range touches
/// no configured glob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramSnapshot {
    pub commit: String,
    /// The range the programs were selected from.
    pub range: ReviewRange,
    pub programs: Vec<SnapshotProgram>,
}

/// Read the program reviews from the landing branch's committed tree and
/// select those the paths `<base>...<head>` changes require, each script
/// read at the same commit. `Ok(None)` when the landing branch has no
/// `dagq.toml` or one without `[review.programs.*]`: the review goes as
/// before. `Err` when the file cannot be read or parsed, or a selected
/// script is not in the commit: what to run is not known, which is the
/// review's failure, not the worker's (ADR-t1895-2 decision 4). `range`
/// is called only when programs are configured.
pub fn snapshot_programs(
    repository: &dyn Repository,
    parse: &dyn Fn(&str) -> Result<Vec<ReviewProgram>>,
    range: &dyn Fn(&CommitSha) -> Result<ReviewRange>,
) -> Result<Option<ProgramSnapshot>> {
    let main = repository.main_head()?;
    let commit = main.to_string();
    let Some(text) = repository
        .file_in(&commit, CONFIG_FILE)
        .with_context(|| format!("read {CONFIG_FILE} in the landing branch's commit {commit}"))?
    else {
        return Ok(None);
    };
    let configured = parse(&text)
        .with_context(|| format!("parse {CONFIG_FILE} in the landing branch's commit {commit}"))?;
    if configured.is_empty() {
        return Ok(None);
    }
    let range = range(&main)?;
    let from = repository
        .merge_base(&range.base, &range.head)?
        .map_or_else(|| range.base.clone(), CommitSha::into_string);
    let changed = repository.changed_paths(&from, &range.head)?;
    let mut programs = Vec::new();
    for selected in review_programs::select(&configured, &changed) {
        let path = &selected.program.script;
        let script = repository
            .file_in(&commit, path)
            .with_context(|| format!("read {path} in the landing branch's commit {commit}"))?
            .with_context(|| {
                format!(
                    "the review program {} that {CONFIG_FILE} names runs {path}, which is not in the landing branch's commit {commit}",
                    selected.program.name
                )
            })?;
        programs.push(SnapshotProgram {
            program: selected.program,
            matched: selected.matched,
            script,
        });
    }
    Ok(Some(ProgramSnapshot {
        commit,
        range,
        programs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::DiffNumbers;
    use crate::domain::{TaskId, TaskRun};
    use std::path::{Path, PathBuf};

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const CONFIG: &str = "the committed dagq.toml";
    const SCRIPT: &str = "#!/bin/sh\necho main's check\n";
    const FMT: &str = "#!/bin/sh\nexec cargo fmt --check\n";

    /// A repository whose landing branch is at [`SHA`] and holds `files`
    /// there only, and whose reviewed range changes `changed`. The run's
    /// worktree is never read: every read is at the landing branch's
    /// commit.
    struct Committed {
        files: Vec<(&'static str, &'static str)>,
        changed: Vec<String>,
    }

    impl Repository for Committed {
        fn is_dagq_source(&self) -> bool {
            false
        }
        fn main_head(&self) -> Result<CommitSha> {
            Ok(CommitSha::try_from(SHA).unwrap())
        }
        fn file_in(&self, commit: &str, path: &str) -> Result<Option<String>> {
            assert_eq!(commit, SHA, "only the landing branch's commit is read");
            Ok(self
                .files
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, text)| (*text).to_owned()))
        }
        fn merge_base(&self, _: &str, _: &str) -> Result<Option<CommitSha>> {
            Ok(None)
        }
        fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<String>> {
            assert_eq!((from, to), ("base", "head"));
            Ok(self.changed.clone())
        }
        fn current_branch(&self, _: &Path) -> Result<Option<String>> {
            unreachable!()
        }
        fn head(&self, _: &Path) -> Result<CommitSha> {
            unreachable!()
        }
        fn is_ancestor(&self, _: &str, _: &str) -> Result<bool> {
            unreachable!()
        }
        fn status(&self, _: &Path) -> Result<String> {
            unreachable!()
        }
        fn rebase_in_progress(&self, _: &Path) -> Result<bool> {
            unreachable!()
        }
        fn rebase_abort(&self, _: &Path) -> Result<()> {
            unreachable!()
        }
        fn rebase(&self, _: &Path, _: &str) -> Result<std::result::Result<(), String>> {
            unreachable!()
        }
        fn conflicted_files(&self, _: &Path) -> Result<Vec<String>> {
            unreachable!()
        }
        fn added_paths(&self, _: &str, _: &str) -> Result<Vec<String>> {
            unreachable!()
        }
        fn paths_in(&self, _: &str, _: &str) -> Result<Vec<String>> {
            unreachable!()
        }
        fn paths_containing(&self, _: &str, _: &str, _: &[String]) -> Result<Vec<String>> {
            unreachable!()
        }
        fn rename_and_commit(
            &self,
            _: &Path,
            _: &str,
            _: &str,
            _: &[String],
        ) -> Result<std::result::Result<CommitSha, String>> {
            unreachable!()
        }
        fn tree_of(&self, _: &str) -> Result<String> {
            unreachable!()
        }
        fn commit_tree(&self, _: &str, _: &str, _: &[String]) -> Result<CommitSha> {
            unreachable!()
        }
        fn update_ref(&self, _: &str, _: &str) -> Result<()> {
            unreachable!()
        }
        fn advance_main(
            &self,
            _: &crate::domain::landing_branch::LandingBranch,
            _: &str,
            _: &str,
        ) -> Result<()> {
            unreachable!()
        }
        fn repair_worktree(&self, _: &Path) -> Result<()> {
            unreachable!()
        }
        fn remove_worktree_and_branch(&self, _: &Path, _: &str) -> Result<()> {
            unreachable!()
        }
        fn branches(&self) -> Result<Vec<String>> {
            unreachable!()
        }
        fn delete_branch(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn tracks(&self, _: &Path, _: &str) -> Result<bool> {
            unreachable!()
        }
        fn main_checkout(&self) -> Result<Option<PathBuf>> {
            unreachable!()
        }
        fn create_worktree(&self, _: &TaskRun) -> Result<String> {
            unreachable!()
        }
        fn merge_conflicts(&self, _: &str, _: &str) -> Result<Vec<String>> {
            unreachable!()
        }
        fn landed_task_ids(&self, _: &str, _: &str) -> Result<Vec<TaskId>> {
            unreachable!()
        }
        fn log_oneline(&self, _: &str, _: &str) -> Result<String> {
            unreachable!()
        }
        fn diff_stat(&self, _: &str, _: &str) -> Result<String> {
            unreachable!()
        }
        fn diff_numbers(&self, _: &str, _: &str) -> Result<DiffNumbers> {
            unreachable!()
        }
        fn diff_to_file(&self, _: &str, _: &str, _: &Path) -> Result<()> {
            unreachable!()
        }
    }

    /// `fmt` runs the script `scripts/fmt.sh` on `src/**`; `links` runs
    /// `scripts/links.sh` on `docs/**`. A config of `bad` does not parse;
    /// any other names no program.
    fn parse(text: &str) -> Result<Vec<ReviewProgram>> {
        let program = |name: &str, script: &str, args: &[&str], glob: &str| ReviewProgram {
            name: name.to_owned(),
            script: script.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            paths: vec![glob.to_owned()],
            timeout_secs: None,
        };
        match text {
            CONFIG => Ok(vec![
                program("fmt", "scripts/fmt.sh", &[], "src/**"),
                program("links", "scripts/links.sh", &["--quiet"], "docs/**"),
            ]),
            "bad" => anyhow::bail!("dagq.toml:1: [review.programs.fmt] has no paths"),
            _ => Ok(Vec::new()),
        }
    }

    fn snapshot(
        files: Vec<(&'static str, &'static str)>,
        changed: &[&str],
    ) -> Result<Option<ProgramSnapshot>> {
        let repository = Committed {
            files,
            changed: changed.iter().map(|p| (*p).to_owned()).collect(),
        };
        let range = |main: &CommitSha| {
            assert_eq!(main.as_str(), SHA);
            Ok(ReviewRange {
                base: "base".to_owned(),
                head: "head".to_owned(),
            })
        };
        snapshot_programs(&repository, &parse, &range)
    }

    /// The programs and each script are read from the landing branch's
    /// commit (ADR-t1895-2 decision 2): those the range's changed paths
    /// require, a script with its committed text.
    #[test]
    fn the_programs_and_scripts_are_read_from_the_landing_branchs_commit() {
        let found = snapshot(
            vec![
                (CONFIG_FILE, CONFIG),
                ("scripts/fmt.sh", FMT),
                ("scripts/links.sh", SCRIPT),
            ],
            &["docs/a.md", "src/b.rs", "docs/c.md"],
        )
        .unwrap()
        .unwrap();
        assert_eq!(found.commit, SHA);
        let read: Vec<_> = found
            .programs
            .iter()
            .map(|p| (p.program.name.as_str(), p.matched.clone(), p.script.clone()))
            .collect();
        assert_eq!(
            read,
            [
                ("fmt", vec!["src/b.rs".to_owned()], FMT.to_owned()),
                (
                    "links",
                    vec!["docs/a.md".to_owned(), "docs/c.md".to_owned()],
                    SCRIPT.to_owned()
                ),
            ]
        );
        let only_src = snapshot(
            vec![(CONFIG_FILE, CONFIG), ("scripts/fmt.sh", FMT)],
            &["src/b.rs"],
        )
        .unwrap()
        .unwrap();
        assert_eq!(only_src.programs.len(), 1);
    }

    /// Without `[review.programs]` on the landing branch (no `dagq.toml`,
    /// or one without the table, an older one) there is no snapshot and the
    /// review is as before; with the table but a range it does not select,
    /// the snapshot is empty.
    #[test]
    fn a_review_without_programs_reads_as_before() {
        assert!(snapshot(Vec::new(), &["src/b.rs"]).unwrap().is_none());
        assert!(
            snapshot(vec![(CONFIG_FILE, "[run.env]")], &["src/b.rs"])
                .unwrap()
                .is_none()
        );
        let none = snapshot(vec![(CONFIG_FILE, CONFIG)], &["README.md"])
            .unwrap()
            .unwrap();
        assert!(none.programs.is_empty());
    }

    /// What to run cannot be known: a committed `dagq.toml` that does not
    /// parse, or a selected script that is not in the landing branch's
    /// commit (though the run's worktree may have added it), is an error
    /// saying which.
    #[test]
    fn an_unreadable_config_or_a_script_missing_from_the_commit_is_an_error() {
        let error = format!(
            "{:#}",
            snapshot(vec![(CONFIG_FILE, "bad")], &["src/b.rs"]).unwrap_err()
        );
        assert!(
            error.contains(&format!(
                "parse dagq.toml in the landing branch's commit {SHA}"
            )) && error.contains("[review.programs.fmt] has no paths"),
            "{error}"
        );
        let error = format!(
            "{:#}",
            snapshot(vec![(CONFIG_FILE, CONFIG)], &["docs/a.md"]).unwrap_err()
        );
        assert!(
            error.contains(&format!(
                "the review program links that dagq.toml names runs scripts/links.sh, which is not in the landing branch's commit {SHA}"
            )),
            "{error}"
        );
    }
}
