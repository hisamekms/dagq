//! The repository of a wiring test in memory: its commits each with its
//! parent, main's line, the run's worktrees on their branches, the scratch
//! checkouts and main's remote, without Git. A commit is only its name; it
//! holds no file but those a test commits to every commit alike, and every
//! diff and listing of files is empty but the changed paths a test names.
//! It does what
//! a claim, a run's validation, the verification of a snapshot of main and
//! a landing (a rebase, the squash commit on main and its push) need; what
//! it does not do (a commit renamed in a worktree) is an error that names
//! it.

use std::{
    collections::{HashMap, HashSet},
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

/// The remote main is pushed to: `origin`, as `dagq.toml` names none.
const REMOTE: &str = "origin";

/// A worktree of the repository: its branch, the commit it started from
/// (or was last rebased onto) and its head, the last commit of its own when
/// made one.
#[derive(Debug, Clone)]
struct Worktree {
    branch: String,
    base: CommitSha,
    head: CommitSha,
}

#[derive(Debug)]
struct State {
    /// Every commit made, with its parent (`None` for the first).
    parents: HashMap<CommitSha, Option<CommitSha>>,
    /// How many commits were made: the next one's number less one.
    made: usize,
    /// Main's commits, oldest first: the last is main's head.
    main: Vec<CommitSha>,
    /// Main's head on the remote, as last pushed.
    pushed: Option<CommitSha>,
    /// The worktrees of the runs, by their path.
    worktrees: HashMap<PathBuf, Worktree>,
    /// The scratch checkouts the snapshots of main are verified in, by
    /// their path: the commit each holds.
    scratches: HashMap<PathBuf, CommitSha>,
    /// Every scratch checkout made, with its commit, in order.
    checked_out: Vec<(PathBuf, CommitSha)>,
    case_bases: Vec<(PathBuf, String, Vec<String>)>,
    /// Files every commit holds as bytes, none of them UTF-8 text
    /// ([`FakeGit::commit_blob`]).
    blobs: HashMap<String, Vec<u8>>,
    /// Each change case tree made: its path, base, the patch's bytes and
    /// what it hides.
    case_trees: Vec<(PathBuf, String, Vec<u8>, Vec<String>)>,
    /// The files every commit holds, by path: none unless a test commits
    /// some ([`FakeGit::commit_files`]).
    files: HashMap<String, String>,
    /// The paths every range changes: none unless a test names some.
    changed: Vec<String>,
    /// The blob ID each changed path has in any range's end: `blob-<path>`
    /// unless a rebase changed it
    /// ([`FakeGit::change_blob_at_next_rebase`]).
    changed_blobs: HashMap<String, String>,
    /// The blob a changed path gets at the next rebase of a run's worktree
    /// ([`FakeGit::change_blob_at_next_rebase`]), once.
    blob_at_next_rebase: Option<(String, String)>,
    /// The blob a changed path gets once the next binding read the diff
    /// ([`FakeGit::change_blob_after_next_binding`]), once.
    blob_after_next_binding: Option<(String, String)>,
    /// Whether a diff's text cannot be read ([`FakeGit::fail_diff_text`]).
    diff_text_fails: bool,
    /// Whether each run's worktree gets a commit once the next binding read
    /// the diff ([`FakeGit::commit_after_next_binding`]), once.
    commit_after_next_binding: bool,
    /// The blob a changed path gets with a commit on each run's worktree
    /// once the next check of a head's merge with main ran
    /// ([`FakeGit::commit_after_next_merge_check`]), once.
    commit_after_next_merge_check: Option<(String, String)>,
    /// The message of each commit of main's a test landed with one
    /// ([`FakeGit::land_with_message`]); any other has none.
    messages: HashMap<CommitSha, String>,
    /// The refs written ([`Repository::update_ref`]), by name: the commit
    /// each holds.
    refs: HashMap<String, CommitSha>,
    /// The paths the next merge of a run's head with a main other than the
    /// one named conflicts on, once ([`FakeGit::conflict_once_main_moves`]).
    conflict_next_merge: Option<(Vec<String>, CommitSha)>,
    merge_clean: bool,
    /// The result of combining an unmodified run with another main. The
    /// fake has no per-commit trees, so it remembers the made commit.
    clean_rebases: HashMap<(String, String), CommitSha>,
    /// The changed path the next rebase of a run's worktree conflicts on,
    /// once, and the blob the commit that resolves it gives the path
    /// ([`FakeGit::conflict_at_next_rebase`]).
    conflict_next_rebase: Option<(String, String)>,
    /// The paths the last rebase conflicted on.
    conflicted: Vec<String>,
    /// The blob the next commit of a run's own gives a changed path: the
    /// resolution of the conflict the rebase found.
    blob_at_next_commit: Option<(String, String)>,
    /// The blobs the changed paths had at a commit a resolution was made
    /// on, by its name: what its binding reads after the resolution.
    blobs_at: HashMap<String, HashMap<String, String>>,
    /// How many commits of a run's own from now on the one is whose first
    /// binding cannot be read ([`FakeGit::fail_first_binding_of_commit`]).
    commits_to_failing_binding: Option<usize>,
    /// The commit whose next binding cannot be read, once.
    failing_binding: Option<CommitSha>,
    /// How many bindings could not be read so far.
    failed_bindings: usize,
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
                parents: HashMap::from([(commit(1), None)]),
                made: 1,
                main: vec![commit(1)],
                pushed: None,
                worktrees: HashMap::new(),
                scratches: HashMap::new(),
                checked_out: Vec::new(),
                case_bases: Vec::new(),
                case_trees: Vec::new(),
                blobs: HashMap::new(),
                files: HashMap::new(),
                changed: Vec::new(),
                changed_blobs: HashMap::new(),
                blob_at_next_rebase: None,
                blob_after_next_binding: None,
                diff_text_fails: false,
                commit_after_next_binding: false,
                commit_after_next_merge_check: None,
                messages: HashMap::new(),
                refs: HashMap::new(),
                conflict_next_merge: None,
                merge_clean: false,
                clean_rebases: HashMap::new(),
                conflict_next_rebase: None,
                conflicted: Vec::new(),
                blob_at_next_commit: None,
                blobs_at: HashMap::new(),
                commits_to_failing_binding: None,
                failing_binding: None,
                failed_bindings: 0,
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn allow_clean_merges(&self) {
        self.lock().merge_clean = true;
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

    pub(super) fn case_bases(&self) -> Vec<(PathBuf, String, Vec<String>)> {
        self.lock().case_bases.clone()
    }

    /// The next rebase of a run's worktree gives the changed `path` the
    /// blob `blob`, once: main changed the file the run changes, so the
    /// merged result's content differs from what the run's judgment saw.
    pub(super) fn change_blob_at_next_rebase(&self, path: &str, blob: &str) {
        self.lock().blob_at_next_rebase = Some((path.to_owned(), blob.to_owned()));
    }

    /// Once the next binding read the diff (its text, after its blobs),
    /// the changed `path` has the blob `blob`, once: the run's branch moved
    /// while what read the binding (a review as it starts) went on.
    pub(super) fn change_blob_after_next_binding(&self, path: &str, blob: &str) {
        self.lock().blob_after_next_binding = Some((path.to_owned(), blob.to_owned()));
    }

    /// The changed `path` has the blob `blob` from now on: the run's
    /// branch moved (a resume's commit).
    pub(super) fn set_changed_blob(&self, path: &str, blob: &str) {
        self.lock()
            .changed_blobs
            .insert(path.to_owned(), blob.to_owned());
    }

    /// The next rebase of a run's worktree conflicts on the changed `path`,
    /// once, and the next commit of a run's own after it (the resolution)
    /// gives the path the blob `resolved`.
    pub(super) fn conflict_at_next_rebase(&self, path: &str, resolved: &str) {
        self.lock().conflict_next_rebase = Some((path.to_owned(), resolved.to_owned()));
    }

    /// The first binding of the `nth` commit of a run's own from now on
    /// (its diff's text read up to it) cannot be read, once: a Git that
    /// failed for a while.
    pub(super) fn fail_first_binding_of_commit(&self, nth: usize) {
        self.lock().commits_to_failing_binding = Some(nth);
    }

    /// How many bindings could not be read so far
    /// ([`Self::fail_first_binding_of_commit`]).
    pub(super) fn failed_bindings(&self) -> usize {
        self.lock().failed_bindings
    }

    /// No diff's text can be read from now on.
    pub(super) fn fail_diff_text(&self) {
        self.lock().diff_text_fails = true;
    }

    /// Once the next binding read the diff, each run's worktree gets a
    /// commit its receipt does not name, once: its worker committed while
    /// what read the binding (a review as it starts) went on.
    pub(super) fn commit_after_next_binding(&self) {
        self.lock().commit_after_next_binding = true;
    }

    /// Once the next check of a head's merge with main ran (the conflict
    /// precheck after a judgment that lets the run land, before it lands),
    /// each run's worktree gets a commit its receipt does not name, and the
    /// changed `path` the blob `blob`, once: its worker committed after the
    /// judgment.
    pub(super) fn commit_after_next_merge_check(&self, path: &str, blob: &str) {
        self.lock().commit_after_next_merge_check = Some((path.to_owned(), blob.to_owned()));
    }

    /// A diff's text can be read again.
    pub(super) fn restore_diff_text(&self) {
        self.lock().diff_text_fails = false;
    }

    /// Every commit holds `bytes` at `path`, read only byte-wise.
    pub(super) fn commit_blob(&self, path: &str, bytes: &[u8]) {
        self.lock().blobs.insert(path.to_owned(), bytes.to_vec());
    }

    pub(super) fn case_trees(&self) -> Vec<(PathBuf, String, Vec<u8>, Vec<String>)> {
        self.lock().case_trees.clone()
    }

    /// Main's head now.
    pub(super) fn main(&self) -> CommitSha {
        self.lock().main.last().cloned().expect("main has a commit")
    }

    /// Main's commits, oldest first.
    pub(super) fn main_line(&self) -> Vec<CommitSha> {
        self.lock().main.clone()
    }

    /// Main's head on its remote, `None` before the first push.
    pub(super) fn pushed(&self) -> Option<CommitSha> {
        self.lock().pushed.clone()
    }

    /// The next merge of a run's head with main (the landing recheck's),
    /// once main moved past its head now, conflicts on `paths`, once.
    pub(super) fn conflict_once_main_moves(&self, paths: &[&str]) {
        let mut state = self.lock();
        let now = state.main.last().cloned().expect("main has a head");
        state.conflict_next_merge =
            Some((paths.iter().map(|path| (*path).to_owned()).collect(), now));
    }

    /// Main moved on by a commit of someone else's, as a landing elsewhere
    /// moves it: its new head.
    pub(super) fn move_main(&self) -> CommitSha {
        self.lock().move_main()
    }

    /// A commit on main with `message`, as a person's push makes one:
    /// main's head from now on.
    pub(super) fn land_with_message(&self, message: &str) -> CommitSha {
        let mut state = self.lock();
        let made = state.move_main();
        state.messages.insert(made.clone(), message.to_owned());
        made
    }

    /// A commit of the run's own on the worktree at `worktree`, as its
    /// worker makes one: the worktree's new head.
    pub(super) fn commit_in(&self, worktree: &Path) -> Result<CommitSha> {
        let mut state = self.lock();
        let head = state
            .worktrees
            .get(worktree)
            .map(|worktree| worktree.head.clone())
            .with_context(|| {
                format!(
                    "{} is no worktree of the test's repository",
                    worktree.display()
                )
            })?;
        let made = state.make(Some(head.clone()));
        state
            .worktrees
            .get_mut(worktree)
            .expect("the worktree just read")
            .head = made.clone();
        if let Some((path, blob)) = state.blob_at_next_commit.take() {
            let before = state.changed_blobs.clone();
            state.blobs_at.insert(head.into_string(), before);
            state.changed_blobs.insert(path, blob);
        }
        match state.commits_to_failing_binding {
            Some(1) => {
                state.commits_to_failing_binding = None;
                state.failing_binding = Some(made.clone());
            }
            Some(left) => state.commits_to_failing_binding = Some(left - 1),
            None => {}
        }
        Ok(made)
    }

    /// The head of the run's worktree at `worktree`.
    pub(super) fn worktree_head(&self, worktree: &Path) -> Result<CommitSha> {
        Ok(self.worktree(worktree)?.head)
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
        self.lock().known(commit)
    }

    fn worktree(&self, path: &Path) -> Result<Worktree> {
        self.lock()
            .worktrees
            .get(path)
            .cloned()
            .with_context(|| format!("{} is no worktree of the test's repository", path.display()))
    }
}

impl State {
    /// A new commit on `parent`.
    fn make(&mut self, parent: Option<CommitSha>) -> CommitSha {
        self.made += 1;
        let made = commit(self.made);
        self.parents.insert(made.clone(), parent);
        made
    }

    /// A new commit on main's head: main's head from now on.
    fn move_main(&mut self) -> CommitSha {
        let parent = self.main.last().cloned();
        let moved = self.make(parent);
        self.main.push(moved.clone());
        moved
    }

    fn known(&self, commit: &str) -> bool {
        self.parents.keys().any(|known| known.as_str() == commit)
    }

    /// `commit` and its ancestors, nearest first; empty for a commit not
    /// made.
    fn line(&self, commit: &str) -> Vec<CommitSha> {
        let mut line = Vec::new();
        let mut next = self
            .parents
            .keys()
            .find(|known| known.as_str() == commit)
            .cloned();
        while let Some(at) = next {
            next = self.parents.get(&at).cloned().flatten();
            line.push(at);
        }
        line
    }
}

impl RepositoryReads for FakeGit {
    fn tree_id(&self, commit: &str) -> Result<String> {
        ensure!(
            self.lock().parents.keys().any(|c| c.as_str() == commit),
            "no commit {commit}"
        );
        // Distinct commits have distinct synthetic trees, without Git.
        Ok(format!("tree-{commit}"))
    }
    fn main_head(&self) -> Result<CommitSha> {
        Ok(self.main())
    }

    /// The file a test committed at `path`, none otherwise.
    fn file_in(&self, commit: &str, path: &str) -> Result<Option<String>> {
        ensure!(self.known(commit), "no commit {commit}");
        Ok(self.lock().files.get(path).cloned())
    }

    fn file_bytes_in(&self, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
        ensure!(self.known(commit), "no commit {commit}");
        let state = self.lock();
        Ok(state
            .blobs
            .get(path)
            .cloned()
            .or_else(|| state.files.get(path).map(|t| t.clone().into_bytes())))
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

    /// Every commit has one parent, so its line is its first parents'.
    fn on_first_parent_line(&self, commit: &str, head: &str) -> Result<bool> {
        let state = self.lock();
        let commit = state.refs.get(commit).map_or(commit, |sha| sha.as_str());
        let head = state.refs.get(head).map_or(head, |sha| sha.as_str());
        Ok(state.line(head).iter().any(|on| on.as_str() == commit))
    }

    /// The nearest commit of `a`'s line on `b`'s.
    fn merge_base(&self, a: &str, b: &str) -> Result<Option<CommitSha>> {
        let state = self.lock();
        let b: HashSet<CommitSha> = state.line(b).into_iter().collect();
        Ok(state.line(a).into_iter().find(|on| b.contains(on)))
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
        self.on_first_parent_line(ancestor, descendant)
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
        self.files.remove_empty_dir(path)?;
        Ok(())
    }

    fn rebase_in_progress(&self, _: &Path) -> Result<bool> {
        Ok(false)
    }

    fn rebase_abort(&self, _: &Path) -> Result<()> {
        Ok(())
    }

    /// The worktree's own commits, as many as it made (a revise's turn
    /// makes one more), made again one by one on `onto`; a worktree of
    /// none moves to `onto`. A commit holds no file, so no rebase
    /// conflicts.
    fn rebase(&self, worktree: &Path, onto: &str) -> Result<std::result::Result<(), String>> {
        let mut state = self.lock();
        ensure!(state.known(onto), "no commit {onto}");
        if let Some((path, resolved)) = state.conflict_next_rebase.take() {
            state.conflicted = vec![path.clone()];
            state.blob_at_next_commit = Some((path, resolved));
            return Ok(Err("CONFLICT (content): merge conflict".to_owned()));
        }
        state.conflicted.clear();
        let mutation = state.blob_at_next_rebase.take();
        let clean = mutation.is_none();
        if let Some((path, blob)) = mutation {
            state.changed_blobs.insert(path, blob);
        }
        let onto = CommitSha::parse(onto, "commit")?;
        let at = state.worktrees.get(worktree).cloned().with_context(|| {
            format!(
                "{} is no worktree of the test's repository",
                worktree.display()
            )
        })?;
        let own = state.line(at.head.as_str()).len() - state.line(at.base.as_str()).len();
        let head = if at.base == onto {
            // Already on `onto`: a no-op, as Git's.
            at.head.clone()
        } else {
            (0..own).fold(onto.clone(), |parent, _| state.make(Some(parent)))
        };
        if at.base != onto && clean {
            state
                .clean_rebases
                .insert((onto.to_string(), at.head.to_string()), head.clone());
        }
        let moved = state
            .worktrees
            .get_mut(worktree)
            .expect("the worktree just read");
        moved.head = head;
        moved.base = onto;
        Ok(Ok(()))
    }

    fn conflicted_files(&self, _: &Path) -> Result<Vec<String>> {
        Ok(self.lock().conflicted.clone())
    }

    fn changed_paths(&self, _: &str, _: &str) -> Result<Vec<String>> {
        Ok(self.lock().changed.clone())
    }

    /// A diff's text of the changed paths and their blobs, for its hash.
    fn diff_text(&self, _: &str, to: &str) -> Result<Vec<u8>> {
        ensure!(
            !self.lock().diff_text_fails,
            "the diff's text cannot be read"
        );
        {
            let mut state = self.lock();
            if state
                .failing_binding
                .as_ref()
                .is_some_and(|failing| failing.as_str() == to)
            {
                state.failing_binding = None;
                state.failed_bindings += 1;
                bail!("the diff's text up to {to} cannot be read now");
            }
        }
        let text: String = self
            .landing_blobs("", "")?
            .iter()
            .map(|(path, state)| format!("diff --git a/{path} b/{path}\n+{state:?}\n"))
            .collect();
        let mut state = self.lock();
        if let Some((path, blob)) = state.blob_after_next_binding.take() {
            state.changed_blobs.insert(path, blob);
        }
        if std::mem::take(&mut state.commit_after_next_binding) {
            let heads: Vec<(PathBuf, CommitSha)> = state
                .worktrees
                .iter()
                .map(|(path, worktree)| (path.clone(), worktree.head.clone()))
                .collect();
            for (path, head) in heads {
                let made = state.make(Some(head));
                state.worktrees.get_mut(&path).expect("a worktree").head = made;
            }
        }
        Ok(text.into_bytes())
    }

    /// The changed paths, each with the blob a test gave it or
    /// `blob-<path>`; at a commit a resolution was made on, the blob it had
    /// there.
    fn landing_blobs(&self, _: &str, to: &str) -> Result<crate::domain::landing_binding::BlobSet> {
        let state = self.lock();
        let blobs = state.blobs_at.get(to).unwrap_or(&state.changed_blobs);
        Ok(state
            .changed
            .iter()
            .map(|path| {
                let blob = blobs
                    .get(path)
                    .cloned()
                    .unwrap_or_else(|| format!("blob-{path}"));
                (path.clone(), crate::domain::audit::BlobState::Blob(blob))
            })
            .collect())
    }

    fn added_paths(&self, _: &str, _: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn paths_in(&self, _: &str, prefix: &str) -> Result<Vec<String>> {
        Ok(self
            .lock()
            .files
            .keys()
            .filter(|path| path.starts_with(prefix))
            .cloned()
            .collect())
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

    /// A new commit on `parent`: a commit holds no tree to tell apart.
    fn commit_tree(&self, _: &str, parent: &str, _: &[String]) -> Result<CommitSha> {
        let mut state = self.lock();
        ensure!(state.known(parent), "no commit {parent}");
        let parent = CommitSha::parse(parent, "commit")?;
        Ok(state.make(Some(parent)))
    }

    fn update_ref(&self, name: &str, value: &str) -> Result<()> {
        let mut state = self.lock();
        ensure!(state.known(value), "no commit {value}");
        let value = CommitSha::parse(value, "commit")?;
        state.refs.insert(name.to_owned(), value);
        Ok(())
    }

    fn ref_value(&self, name: &str) -> Result<Option<CommitSha>> {
        Ok(self.lock().refs.get(name).cloned())
    }

    /// Main moves from `from` to `to`, a commit of its own: only from its
    /// head.
    fn advance_main(&self, _: &LandingBranch, from: &str, to: &str) -> Result<()> {
        let mut state = self.lock();
        ensure!(
            state.main.last().map(CommitSha::as_str) == Some(from),
            "main is not at {from}"
        );
        let to = CommitSha::parse(to, "commit")?;
        ensure!(
            state
                .parents
                .get(&to)
                .cloned()
                .flatten()
                .as_ref()
                .map(CommitSha::as_str)
                == Some(from),
            "{to} is no commit on {from}"
        );
        state.main.push(to);
        Ok(())
    }

    fn repair_worktree(&self, _: &Path) -> Result<()> {
        Ok(())
    }

    fn remove_worktree_and_branch(&self, worktree: &Path, _: &str) -> Result<()> {
        self.lock().worktrees.remove(worktree);
        self.files.remove_empty_dir(worktree)?;
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
        // The worktree is a directory of the test's files, as Git makes it.
        self.files.put_dir(&path);
        state.worktrees.insert(
            path,
            Worktree {
                branch,
                base: run.base_commit().clone(),
                head: run.base_commit().clone(),
            },
        );
        Ok(String::new())
    }

    fn merge_conflicts(&self, _: &str, _: &str) -> Result<Vec<String>> {
        let mut state = self.lock();
        if let Some((path, blob)) = state.commit_after_next_merge_check.take() {
            state.changed_blobs.insert(path, blob);
            let heads: Vec<(PathBuf, CommitSha)> = state
                .worktrees
                .iter()
                .map(|(path, worktree)| (path.clone(), worktree.head.clone()))
                .collect();
            for (path, head) in heads {
                let made = state.make(Some(head));
                state.worktrees.get_mut(&path).expect("a worktree").head = made;
            }
        }
        Ok(Vec::new())
    }

    fn guarded(&self) -> Result<Box<dyn Repository + Send + Sync>> {
        Ok(Box::new(self.clone()))
    }

    /// The requested conflict once main moves, or a clean merge when the
    /// test enabled it; otherwise this fake cannot merge a tree.
    fn merged_tree(
        &self,
        main: &str,
        head: &str,
    ) -> Result<std::result::Result<String, Vec<String>>> {
        let mut state = self.lock();
        match state.conflict_next_merge.take() {
            Some((paths, before)) if before.as_str() != main => Ok(Err(paths)),
            unmoved => {
                state.conflict_next_merge = unmoved;
                if let Some(rebased) = state.clean_rebases.get(&(main.to_owned(), head.to_owned()))
                {
                    Ok(Ok(format!("tree of {rebased}")))
                } else if state.merge_clean {
                    Ok(Ok(format!("merged tree on {main}")))
                } else {
                    bail!("the wiring test's repository merges no tree")
                }
            }
        }
    }

    fn add_case_tree(
        &self,
        path: &Path,
        base: &str,
        patch: &Path,
        hidden: &[&str],
    ) -> Result<CommitSha> {
        self.checkout_scratch(path, base)?;
        let bytes = self.files.read(patch).unwrap_or_default();
        self.lock().case_trees.push((
            path.to_owned(),
            base.to_owned(),
            bytes,
            hidden.iter().map(|p| (*p).to_owned()).collect(),
        ));
        Ok(CommitSha::parse(base, "case head")?)
    }

    fn add_case_base_tree(&self, path: &Path, base: &str, hidden: &[&str]) -> Result<()> {
        self.checkout_scratch(path, base)?;
        self.lock().case_bases.push((
            path.to_owned(),
            base.to_owned(),
            hidden.iter().map(|p| (*p).to_owned()).collect(),
        ));
        Ok(())
    }

    fn remove_case_tree(&self, path: &Path) -> Result<()> {
        self.remove_scratch(path)
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

    /// A fixed change copy naming its range, without invoking Git.
    fn diff_binary_to_file(&self, base: &str, head: &str, path: &Path) -> Result<()> {
        Ok(self
            .files
            .write(path, format!("binary change {base}...{head}").as_bytes())?)
    }
    /// An empty display diff, written to the test's files.
    fn diff_to_file(&self, _: &str, _: &str, path: &Path) -> Result<()> {
        Ok(self.files.write(path, b"")?)
    }
}

/// One remote, [`REMOTE`]: a push puts main's head there.
impl MainRemote for FakeGit {
    fn has_remote(&self, remote: &str) -> Result<bool> {
        Ok(remote == REMOTE)
    }

    fn push_main(&self, _: &PushGrant, remote: &str, branch: &LandingBranch) -> Result<()> {
        ensure!(
            remote == REMOTE,
            "the wiring test's repository has no remote {remote}"
        );
        ensure!(
            branch.name == LandingBranch::main().name,
            "the wiring test's repository pushes only main, not {}",
            branch.name
        );
        let mut state = self.lock();
        state.pushed = state.main.last().cloned();
        Ok(())
    }

    fn contains_landed_commit(
        &self,
        remote: &str,
        _: &LandingBranch,
        commit: &CommitSha,
    ) -> Result<bool> {
        let state = self.lock();
        Ok(remote == REMOTE
            && state
                .pushed
                .as_ref()
                .is_some_and(|pushed| state.line(pushed.as_str()).contains(commit)))
    }
}
