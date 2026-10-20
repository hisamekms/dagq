//! The branch a retry carries over to the next run of its task (ADR-0047
//! decision 24): which commit of an ended run is inherited and where it is
//! kept. The supervisor's automatic `retry_inherit` and a person's
//! `ready --inherit` (ADR-t1962-1) choose it the same way.
//!
//! Execution and landing owns this module. It publishes [`InheritStore`]
//! and [`CarriedBranches`] to planning management's `ready --inherit`, which
//! authorizes the command and leaves the retry to them
//! (transaction T10 of `src/context_boundaries/ownership.rs`), and
//! [`ReleasedBranches`] to the release of a task's hold that ends its run
//! for a new one (ADR-t1879-1), whose store carries the run over by hand
//! the same way (T13).

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::{Repository, RunFiles};
use crate::domain::{
    AskId, CommitSha, LeaseToken, RunId, Task, TaskId, TaskRun, TaskStatus, actor::ActorRole,
};

/// The ref that keeps a carried-over head after the run's branch is gone.
pub fn run_ref(run: &RunId) -> String {
    format!("refs/dagq/runs/{run}")
}

/// The commit of the run's branch a retry carries over: its validated
/// commit (the one its review passed), or without one the head of its
/// worktree when no rebase is stopped half way there. `None` when the
/// branch holds nothing on top of the run's base. Nothing is written.
pub fn carried_head(
    run: &TaskRun,
    files: &dyn RunFiles,
    repository: &dyn Repository,
) -> Result<Option<CommitSha>> {
    // The reviewed commit, not whatever an unresolved session left in
    // the worktree (a rebase stopped half way, say).
    let head = match (run.result_commit(), run.worktree_path().map(Path::new)) {
        (Some(commit), _) => Some(commit.clone()),
        (None, Some(worktree))
            if files.is_dir(worktree) && !super::run_git::rebase_in_progress(repository, run)? =>
        {
            Some(super::run_git::head(repository, run)?)
        }
        _ => None,
    };
    Ok(head.filter(|head| head != run.base_commit()))
}

/// Keep `head` under [`run_ref`] so that it outlives the run's branch.
pub fn keep_carried_head(repository: &dyn Repository, run: &RunId, head: &CommitSha) -> Result<()> {
    repository.update_ref(&run_ref(run), head.as_str())
}

/// Where a person's `ready --inherit` reads the head to carry over and
/// keeps it, in the run's own repository: [`carried_head`] before the
/// store's transaction, and [`keep_carried_head`] only after it committed
/// (ADR-t1962-1), so that a refusal leaves the ref as it was.
pub trait CarriedBranches {
    /// [`carried_head`] of a run that ended (`failed` or `interrupted`);
    /// `None` for any other run, whose branch is not read (the store
    /// refuses it with its status).
    fn carried_head(&self, run: &TaskRun) -> Result<Option<CommitSha>>;
    fn keep(&self, run: &TaskRun, head: &CommitSha) -> Result<()>;
}

/// The store side of a person's retry that carries a run over: one
/// transaction that checks the preconditions and applies it (T10).
pub trait InheritStore {
    /// Carry the latest run of `request.task` over by hand, or refuse with
    /// the precondition that does not hold
    /// ([`crate::domain::inherit_by_hand::check`]) and change nothing. The
    /// ref of the head is the caller's to write afterwards.
    fn inherit_by_hand(&mut self, request: InheritRequest) -> Result<InheritedByHand>;
}

/// A person's retry that carries the task's latest run over
/// (`ready --inherit`), as the store checks and applies it.
#[derive(Debug, Clone)]
pub struct InheritRequest {
    pub task: TaskId,
    /// The task's status the command was authorized with.
    pub authorized: TaskStatus,
    /// The latest run the head was read from; the store refuses when
    /// another run is the latest by then.
    pub run: Option<RunId>,
    /// What [`CarriedBranches::carried_head`] read; `None` is a branch with
    /// nothing to carry over.
    pub head: Option<CommitSha>,
    pub role: ActorRole,
    /// Why the person carries it over, recorded with it.
    pub reason: String,
}

/// What a retry by hand that carried a run over did.
#[derive(Debug, Clone, Serialize)]
pub struct InheritedByHand {
    pub task: Task,
    pub run_id: RunId,
    pub branch: Option<String>,
    pub head: CommitSha,
    /// The run's `decide` asks of the recovery job it closed.
    pub closed_asks: Vec<AskId>,
    /// The token of the stale lease it removed, whose holder can no longer
    /// apply its round.
    pub removed_lease: Option<LeaseToken>,
}

/// What a release that ends a run for a new one read of the run's work
/// ([`released_work`]): the head of its own commits as its workplace has it
/// now and the commit that saved its uncommitted work on top of that head,
/// each `None` when there is none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReleasedRead {
    pub own: Option<CommitSha>,
    pub saved: Option<CommitSha>,
}

impl ReleasedRead {
    /// The commit the next run carries over: the saved work, which sits on
    /// the run's own commits, or else those commits.
    pub fn head(&self) -> Option<&CommitSha> {
        self.saved.as_ref().or(self.own.as_ref())
    }
}

/// The work of `run` a release that ends it for a new one carries over
/// (ADR-t1879-1), whatever the run's status: the latest it left, the head
/// of its worktree on its branch, whatever the commit its review passed
/// (only a run whose worktree is gone goes on from [`carried_head`]), and
/// its worktree's uncommitted work saved as a commit on top of that head
/// ([`Repository::worktree_snapshot`]), which the next run then brings
/// onto main with the rest. An error, which refuses the release, when a
/// Git operation is half done in the worktree, its `HEAD` is not on the
/// run's branch (detached, or another branch checked out) or the work
/// cannot be saved: no work is thrown away unsaid. Nothing in the
/// worktree changes. A run in a container is read from the facts its
/// executor last recorded at a turn's end ([`super::run_git::session_head`],
/// its clone's head as imported from its bundle), with no git in its
/// clone: facts that are not of the end of the last turn it started (that
/// turn was cut off with its container, and its work is not in them), or
/// that say uncommitted work, a rebase stopped in its clone or a head its
/// bundle does not hold, cannot be saved and refuse the release, as do
/// facts that cannot be read.
pub fn released_work(
    run: &TaskRun,
    files: &dyn RunFiles,
    repository: &dyn Repository,
) -> Result<ReleasedRead> {
    let worktree = run
        .worktree_path()
        .map(Path::new)
        .filter(|worktree| files.is_dir(worktree));
    let Some(worktree) = worktree else {
        return Ok(ReleasedRead {
            own: carried_head(run, files, repository)?,
            saved: None,
        });
    };
    if run.in_container() {
        let session = super::run_git::session_head(repository, run)?;
        if !session.last_turn {
            bail!(
                "run {} works in a container whose executor did not record the end of the last turn it started (that turn was cut off with its container, or its end could not write them), so its facts are not of its latest work, which cannot be saved; the hold stays",
                run.id()
            );
        }
        if !session.clean {
            bail!(
                "run {} works in a container whose executor recorded uncommitted work, a rebase stopped half way or a head its bundle does not hold; the control side runs no git in the run's clone, so that cannot be saved and the hold stays",
                run.id()
            );
        }
        return Ok(ReleasedRead {
            own: Some(session.head).filter(|head| head != run.base_commit()),
            saved: None,
        });
    }
    let git = super::run_git::for_run(repository, run)?;
    if let Some(operation) = git.git_operation(worktree)? {
        bail!(
            "a {operation} is half done in the worktree of run {}; finish or abort it there and release again",
            run.id()
        );
    }
    let branch = format!("refs/heads/{}", run.branch().context("missing branch")?);
    let current = git.current_branch(worktree)?;
    if current.as_deref() != Some(branch.as_str()) {
        bail!(
            "the worktree of run {} is on {} instead of {branch}; check out its branch with the work to carry over there and release again",
            run.id(),
            current.as_deref().unwrap_or("a detached HEAD")
        );
    }
    let own = Some(git.head(worktree)?).filter(|head| head != run.base_commit());
    let saved = git
        .worktree_snapshot(
            worktree,
            &format!(
                "dagq: the uncommitted work of run {}, saved when its task's hold was released",
                run.id()
            ),
        )
        .with_context(|| {
            format!(
                "the uncommitted work of run {} could not be saved; the hold stays",
                run.id()
            )
        })?;
    Ok(ReleasedRead { own, saved })
}

/// Where a release that ends a run for a new one reads its work
/// ([`released_work`]) before the store's transaction and keeps the head
/// it carries over ([`keep_carried_head`]) in that transaction, before it
/// commits, so that a head that cannot be kept refuses the release; in the
/// run's own repository.
pub trait ReleasedBranches {
    fn released_work(&self, run: &TaskRun) -> Result<ReleasedRead>;
    fn keep(&self, run: &TaskRun, head: &CommitSha) -> Result<()>;
}
