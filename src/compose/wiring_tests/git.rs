//! The repository of a wiring test in memory: main's line of commits, the
//! run's worktrees on their branches and the scratch checkouts, without
//! Git. A commit is only its name; it holds no file but those a test
//! commits to every commit alike, and every diff and listing of files is
//! empty but the changed paths a test names. It does what a claim, a run's validation and
//! the verification of a snapshot of main need; what it does not do yet
//! (a rebase, a commit of its own) is an error that names it.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail, ensure};

use crate::{
    application::{
        DiffNumbers, MainRemote, Repository, RepositoryReads, RunFiles, integrate::PushGrant,
        memory_files::MemoryFiles,
    },
    domain::{CommitSha, TaskId, TaskRun, landing_branch::LandingBranch},
};

/// A worktree of the repository: its branch and its head.
#[derive(Debug, Clone)]
struct Worktree {
    branch: String,
    head: CommitSha,
}

#[derive(Debug)]
struct State {
    /// Main's commits, oldest first: the last is main's head.
    main: Vec<CommitSha>,
    /// The worktrees of the runs, by their path.
    worktrees: HashMap<PathBuf, Worktree>,
    /// The scratch checkouts the snapshots of main are verified in, by
    /// their path: the commit each holds.
    scratches: HashMap<PathBuf, CommitSha>,
    /// Every scratch checkout made, with its commit, in order.
    checked_out: Vec<(PathBuf, CommitSha)>,
    /// The files every commit holds, by path: none unless a test commits
    /// some ([`FakeGit::commit_files`]).
    files: HashMap<String, String>,
    /// The paths every range changes: none unless a test names some.
    changed: Vec<String>,
    /// The parent of each commit a worker made on its worktree.
    parents: HashMap<CommitSha, CommitSha>,
    /// The message of each commit of main's a test landed with one
    /// ([`FakeGit::land_with_message`]); any other has none.
    messages: HashMap<CommitSha, String>,
}

/// The repository in memory, its main checkout at `checkout`; clones share
/// it.
#[derive(Clone)]
pub(super) struct FakeGit {
    checkout: PathBuf,
    files: Arc<MemoryFiles>,
    state: Arc<Mutex<State>>,
}

/// The name of the `n`th commit of a test's repository: 40 hex digits.
fn commit(n: usize) -> CommitSha {
    CommitSha::parse(format!("{n:040x}"), "commit").expect("a commit's name")
}

impl FakeGit {
    /// A repository of one commit on main, checked out at `checkout`.
    pub(super) fn new(checkout: &Path, files: Arc<MemoryFiles>) -> Self {
        Self {
            checkout: checkout.to_owned(),
            files,
            state: Arc::new(Mutex::new(State {
                main: vec![commit(1)],
                worktrees: HashMap::new(),
                scratches: HashMap::new(),
                checked_out: Vec::new(),
                files: HashMap::new(),
                changed: Vec::new(),
                parents: HashMap::new(),
                messages: HashMap::new(),
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Every commit holds `files` (path and text), and every range
    /// changes `changed`.
    pub(super) fn commit_files(&self, files: &[(&str, &str)], changed: &[&str]) {
        let mut state = self.lock();
        for (path, text) in files {
            state.files.insert((*path).to_owned(), (*text).to_owned());
        }
        state.changed = changed.iter().map(|path| (*path).to_owned()).collect();
    }

    /// A commit of the worker's on the worktree at `path`, on top of its
    /// head: the worktree's head from now on.
    pub(super) fn commit_in(&self, path: &Path) -> Result<CommitSha> {
        let mut state = self.lock();
        let made = commit(1000 + state.parents.len());
        let worktree = state.worktrees.get_mut(path).with_context(|| {
            format!("{} is no worktree of the test's repository", path.display())
        })?;
        let parent = std::mem::replace(&mut worktree.head, made.clone());
        state.parents.insert(made.clone(), parent);
        Ok(made)
    }

    /// A commit on main with `message`, as a person's push makes one:
    /// main's head from now on.
    pub(super) fn land_with_message(&self, message: &str) -> CommitSha {
        let mut state = self.lock();
        let made = commit(2000 + state.messages.len());
        state.main.push(made.clone());
        state.messages.insert(made.clone(), message.to_owned());
        made
    }

    /// Main's head now.
    pub(super) fn main(&self) -> CommitSha {
        self.lock().main.last().cloned().expect("main has a commit")
    }

    /// The commit the scratch checkout at `path` holds, `None` when there
    /// is none there.
    fn scratch(&self, path: &Path) -> Option<CommitSha> {
        self.lock().scratches.get(path).cloned()
    }

    /// Every scratch checkout made, with the commit it held, in order.
    pub(super) fn checked_out(&self) -> Vec<(PathBuf, CommitSha)> {
        self.lock().checked_out.clone()
    }

    /// Where `commit` is on main's line, `None` off it.
    fn position(&self, commit: &str) -> Option<usize> {
        self.lock().main.iter().position(|on| on.as_str() == commit)
    }

    fn known(&self, commit: &str) -> bool {
        let state = self.lock();
        state.main.iter().any(|on| on.as_str() == commit)
            || state
                .worktrees
                .values()
                .any(|worktree| worktree.head.as_str() == commit)
    }

    fn worktree(&self, path: &Path) -> Result<Worktree> {
        self.lock()
            .worktrees
            .get(path)
            .cloned()
            .with_context(|| format!("{} is no worktree of the test's repository", path.display()))
    }
}

impl RepositoryReads for FakeGit {
    fn main_head(&self) -> Result<CommitSha> {
        Ok(self.main())
    }

    /// The file a test committed at `path`, none otherwise.
    fn file_in(&self, commit: &str, path: &str) -> Result<Option<String>> {
        ensure!(self.known(commit), "no commit {commit}");
        Ok(self.lock().files.get(path).cloned())
    }

    fn tree_paths(&self, commit: &str) -> Result<Vec<String>> {
        ensure!(self.known(commit), "no commit {commit}");
        Ok(Vec::new())
    }

    fn has_commit(&self, commit: &str) -> Result<bool> {
        Ok(self.known(commit))
    }

    /// The commits of main's line after `after` up to `head`, each at its
    /// place on the line as its time (so `since` is not read), changing no
    /// path, and reverting what its message says.
    fn main_commits(
        &self,
        head: &str,
        after: Option<&str>,
        _since: i64,
    ) -> Result<Vec<crate::domain::stats::conflicts::MainCommit>> {
        let end = self
            .position(head)
            .with_context(|| format!("{head} is not on main"))?;
        let start = after
            .and_then(|after| self.position(after))
            .map_or(0, |at| at + 1);
        let state = self.lock();
        Ok(state.main[start..=end]
            .iter()
            .zip(start..)
            .map(|(sha, at)| crate::domain::stats::conflicts::MainCommit {
                sha: sha.as_str().to_owned(),
                at: i64::try_from(at).unwrap_or(i64::MAX),
                changes: Vec::new(),
                reverts: crate::domain::stats::revert::RevertOf::of_message(
                    state.messages.get(sha).map_or("", String::as_str),
                ),
            })
            .collect())
    }

    fn on_first_parent_line(&self, commit: &str, head: &str) -> Result<bool> {
        Ok(matches!(
            (self.position(commit), self.position(head)),
            (Some(commit), Some(head)) if commit <= head
        ))
    }

    /// The earlier of two commits of main's line; a worktree's head is the
    /// commit its run started from.
    fn merge_base(&self, a: &str, b: &str) -> Result<Option<CommitSha>> {
        let (Some(a), Some(b)) = (self.position(a), self.position(b)) else {
            return Ok(None);
        };
        Ok(self.lock().main.get(a.min(b)).cloned())
    }
}

impl Repository for FakeGit {
    fn is_dagq_source(&self) -> bool {
        false
    }

    fn reachable_commit(&self, commit: &str) -> Result<bool> {
        Ok(self.known(commit))
    }

    fn branch_head(&self, branch: &str) -> Result<Option<CommitSha>> {
        let short = branch.strip_prefix("refs/heads/").unwrap_or(branch);
        if short == LandingBranch::main().name {
            return Ok(Some(self.main()));
        }
        Ok(self
            .lock()
            .worktrees
            .values()
            .find(|worktree| worktree.branch == short)
            .map(|worktree| worktree.head.clone()))
    }

    fn current_branch(&self, worktree: &Path) -> Result<Option<String>> {
        if worktree == self.checkout {
            return Ok(Some(format!("refs/heads/{}", LandingBranch::main().name)));
        }
        Ok(Some(format!(
            "refs/heads/{}",
            self.worktree(worktree)?.branch
        )))
    }

    fn head(&self, worktree: &Path) -> Result<CommitSha> {
        if worktree == self.checkout {
            return Ok(self.main());
        }
        if let Some(commit) = self.scratch(worktree) {
            return Ok(commit);
        }
        Ok(self.worktree(worktree)?.head)
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        if ancestor == descendant {
            return Ok(self.known(ancestor));
        }
        // A worker's commit: through its parents to main's line.
        let mut at = descendant.to_owned();
        loop {
            let parent = self
                .lock()
                .parents
                .iter()
                .find(|(made, _)| made.as_str() == at)
                .map(|(_, parent)| parent.as_str().to_owned());
            match parent {
                Some(parent) if parent == ancestor => return Ok(true),
                Some(parent) => at = parent,
                None => break,
            }
        }
        self.on_first_parent_line(ancestor, &at)
    }

    /// Clean: nothing is ever written in a worktree.
    fn status(&self, worktree: &Path) -> Result<String> {
        if worktree != self.checkout {
            self.worktree(worktree)?;
        }
        Ok(String::new())
    }

    fn worktree_diff(&self, _: &Path, _: bool) -> Result<Vec<u8>> {
        Ok(Vec::new())
    }

    fn untracked_paths(&self, _: &Path) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// None: a clean worktree has nothing to save.
    fn worktree_snapshot(&self, _: &Path, _: &str) -> Result<Option<CommitSha>> {
        Ok(None)
    }

    fn discard_worktree(&self, path: &Path, _: &str) -> Result<()> {
        self.lock().worktrees.remove(path);
        Ok(())
    }

    fn rebase_in_progress(&self, _: &Path) -> Result<bool> {
        Ok(false)
    }

    fn rebase_abort(&self, _: &Path) -> Result<()> {
        Ok(())
    }

    fn rebase(&self, worktree: &Path, onto: &str) -> Result<std::result::Result<(), String>> {
        bail!(
            "the wiring test's repository rebases no worktree ({} onto {onto})",
            worktree.display()
        )
    }

    fn conflicted_files(&self, _: &Path) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn changed_paths(&self, _: &str, _: &str) -> Result<Vec<String>> {
        Ok(self.lock().changed.clone())
    }

    fn added_paths(&self, _: &str, _: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn paths_in(&self, _: &str, _: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn paths_containing(&self, _: &str, _: &str, _: &[String]) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn rename_and_commit(
        &self,
        worktree: &Path,
        _: &str,
        _: &str,
        _: &[String],
    ) -> Result<std::result::Result<CommitSha, String>> {
        bail!(
            "the wiring test's repository commits nothing in {}",
            worktree.display()
        )
    }

    fn tree_of(&self, commit: &str) -> Result<String> {
        ensure!(self.known(commit), "no commit {commit}");
        Ok(format!("tree of {commit}"))
    }

    fn commit_tree(&self, _: &str, parent: &str, _: &[String]) -> Result<CommitSha> {
        bail!("the wiring test's repository makes no commit on {parent}")
    }

    fn update_ref(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }

    /// Main moves from `from` to `to`, a commit of its own: only from its
    /// head.
    fn advance_main(&self, _: &LandingBranch, from: &str, to: &str) -> Result<()> {
        let mut state = self.lock();
        ensure!(
            state.main.last().map(CommitSha::as_str) == Some(from),
            "main is not at {from}"
        );
        state.main.push(CommitSha::parse(to, "commit")?);
        Ok(())
    }

    fn repair_worktree(&self, _: &Path) -> Result<()> {
        Ok(())
    }

    fn remove_worktree_and_branch(&self, worktree: &Path, _: &str) -> Result<()> {
        self.lock().worktrees.remove(worktree);
        Ok(())
    }

    fn branches(&self) -> Result<Vec<String>> {
        let state = self.lock();
        Ok(std::iter::once(LandingBranch::main().name)
            .chain(
                state
                    .worktrees
                    .values()
                    .map(|worktree| worktree.branch.clone()),
            )
            .collect())
    }

    fn delete_branch(&self, branch: &str) -> Result<()> {
        self.lock()
            .worktrees
            .retain(|_, worktree| worktree.branch != branch);
        Ok(())
    }

    fn tracks(&self, _: &Path, _: &str) -> Result<bool> {
        Ok(false)
    }

    fn main_checkout(&self) -> Result<Option<PathBuf>> {
        Ok(Some(self.checkout.clone()))
    }

    /// The run's worktree on its branch at its base commit.
    fn create_worktree(&self, run: &TaskRun) -> Result<String> {
        let path = PathBuf::from(run.worktree_path().context("the run has no worktree")?);
        let branch = run.branch().context("the run has no branch")?.to_owned();
        ensure!(
            self.known(run.base_commit().as_str()),
            "no commit {}",
            run.base_commit()
        );
        let mut state = self.lock();
        ensure!(
            !state.worktrees.contains_key(&path),
            "{} is a worktree already",
            path.display()
        );
        // A directory the run's review finds there.
        self.files.put_dir(&path);
        state.worktrees.insert(
            path,
            Worktree {
                branch,
                head: run.base_commit().clone(),
            },
        );
        Ok(String::new())
    }

    fn merge_conflicts(&self, _: &str, _: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn checkout_scratch(&self, path: &Path, commit: &str) -> Result<()> {
        ensure!(self.known(commit), "no commit {commit}");
        let commit = CommitSha::parse(commit, "commit")?;
        let mut state = self.lock();
        state.scratches.insert(path.to_owned(), commit.clone());
        state.checked_out.push((path.to_owned(), commit));
        Ok(())
    }

    fn fresh_scratch(&self, path: &Path, commit: &str) -> Result<()> {
        self.checkout_scratch(path, commit)
    }

    fn remove_scratch(&self, path: &Path) -> Result<()> {
        self.lock().scratches.remove(path);
        Ok(())
    }

    /// None: no commit of main's line carries a task's trailer.
    fn landed_task_ids(&self, _: &str, _: &str) -> Result<Vec<TaskId>> {
        Ok(Vec::new())
    }

    fn log_oneline(&self, _: &str, _: &str) -> Result<String> {
        Ok(String::new())
    }

    fn diff_stat(&self, _: &str, _: &str) -> Result<String> {
        Ok(String::new())
    }

    fn diff_numbers(&self, _: &str, _: &str) -> Result<DiffNumbers> {
        Ok(DiffNumbers::default())
    }

    /// An empty diff, written to the test's files.
    fn diff_to_file(&self, _: &str, _: &str, path: &Path) -> Result<()> {
        Ok(self.files.write(path, b"")?)
    }
}

/// No remote: main is pushed nowhere.
impl MainRemote for FakeGit {
    fn has_remote(&self, _: &str) -> Result<bool> {
        Ok(false)
    }

    fn push_main(&self, _: &PushGrant, remote: &str, _: &LandingBranch) -> Result<()> {
        bail!("the wiring test's repository has no remote {remote}")
    }

    fn contains_landed_commit(&self, _: &str, _: &LandingBranch, _: &CommitSha) -> Result<bool> {
        Ok(false)
    }
}
