//! The branch a retry carries over to the next run of its task (ADR-0047
//! decision 24): which commit of an ended run is inherited and where it is
//! kept. The supervisor's automatic `retry_inherit` and a person's
//! `ready --inherit` (ADR-t1962-1) choose it the same way.
//!
//! Execution and landing owns this module. It publishes [`InheritStore`]
//! and [`CarriedBranches`] to planning management's `ready --inherit`, which
//! authorizes the command and leaves the retry to them
//! (`docs/design/architecture.md`, T10).

use std::path::Path;

use anyhow::Result;
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
            if files.is_dir(worktree) && !repository.rebase_in_progress(worktree)? =>
        {
            Some(repository.head(worktree)?)
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
