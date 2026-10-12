//! The git of a run as the control side reads and handles it, the one
//! place that tells a run on the host (its worktree, read with git as
//! before) from a run in a container (ADR-t2179-1): the latter's facts come
//! from the bundle and the state file its executor writes
//! ([`Repository::clone_facts`]), its landing, review and e2e work in a
//! landing worktree of the control side's own ([`landing_tree`]), and the
//! git that handles its commits is guarded ([`for_run`]). No caller hands
//! a clone's path to git.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{CloneFacts, Repository};
use crate::domain::{CommitSha, TaskRun, run_clone::CLONE_MAIN_REF};

/// What a session left in a run's workplace: its head, and whether it is
/// clean (nothing uncommitted, no rebase in progress for a container run,
/// whose state must also name the imported head).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHead {
    pub head: CommitSha,
    pub clean: bool,
}

fn worktree(run: &TaskRun) -> Result<&Path> {
    Ok(Path::new(run.worktree_path().context("missing worktree")?))
}

/// The facts of a run in a container, a bundle or state file the session
/// must put right being an error.
fn facts(repository: &dyn Repository, run: &TaskRun) -> Result<CloneFacts> {
    match repository.clone_facts(run)? {
        Ok(facts) => Ok(facts),
        Err(why) => bail!("run {}: {why}", run.id()),
    }
}

/// The head of the run's branch: its worktree's `HEAD` on the host; for a
/// run in a container, the run branch imported from its bundle.
pub fn head(repository: &dyn Repository, run: &TaskRun) -> Result<CommitSha> {
    if run.in_container() {
        Ok(facts(repository, run)?.head)
    } else {
        repository.head(worktree(run)?)
    }
}

/// What is uncommitted where the run works (`git status --porcelain`,
/// blank when clean): as the container's executor last wrote it for a run
/// in a container.
pub fn status(repository: &dyn Repository, run: &TaskRun) -> Result<String> {
    if run.in_container() {
        Ok(facts(repository, run)?.state.status)
    } else {
        repository.status(worktree(run)?)
    }
}

/// The head and whether the workplace is clean ([`SessionHead`]).
pub fn session_head(repository: &dyn Repository, run: &TaskRun) -> Result<SessionHead> {
    if run.in_container() {
        let CloneFacts { head, state } = facts(repository, run)?;
        let clean =
            state.status.trim().is_empty() && !state.rebase_in_progress && state.head() == head;
        Ok(SessionHead { head, clean })
    } else {
        let worktree = worktree(run)?;
        Ok(SessionHead {
            head: repository.head(worktree)?,
            clean: repository.status(worktree)?.trim().is_empty(),
        })
    }
}

/// Whether a rebase is in progress where the run works.
pub fn rebase_in_progress(repository: &dyn Repository, run: &TaskRun) -> Result<bool> {
    if run.in_container() {
        Ok(facts(repository, run)?.state.rebase_in_progress)
    } else {
        repository.rebase_in_progress(worktree(run)?)
    }
}

/// Whether `error` says the guard does not hold
/// ([`GuardDoesNotHold`](crate::domain::run_clone::GuardDoesNotHold)), not
/// that git could not be run to read the configuration.
pub fn guard_does_not_hold(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<crate::domain::run_clone::GuardDoesNotHold>())
}

/// `repository` as the git that handles `run`'s commits must be: guarded
/// for a run in a container (ADR-t2179-1 decision 8), as it is for a run on
/// the host. `Err` when the guard cannot be made to hold.
pub fn for_run<'a>(repository: &'a dyn Repository, run: &TaskRun) -> Result<RunRepository<'a>> {
    Ok(if run.in_container() {
        RunRepository::Guarded(repository.guarded().with_context(|| {
            format!(
                "the git that handles run {}'s commits cannot be kept from running their hooks and drivers",
                run.id()
            )
        })?)
    } else {
        RunRepository::Plain(repository)
    })
}

/// The repository [`for_run`] hands out.
pub enum RunRepository<'a> {
    Plain(&'a dyn Repository),
    Guarded(Box<dyn Repository + Send + Sync>),
}

impl<'a> std::ops::Deref for RunRepository<'a> {
    type Target = dyn Repository + 'a;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Plain(repository) => *repository,
            Self::Guarded(repository) => &**repository,
        }
    }
}

/// The tree the control side lands, reviews and runs the e2e of `run` in:
/// its worktree on the host; for a run in a container, its landing
/// worktree (ADR-t2179-1 decision 5) with the commit its receipt's
/// validation accepted, made when missing. `Ok(Err(why))` refuses to make
/// it.
pub fn landing_tree(
    repository: &dyn Repository,
    run: &TaskRun,
) -> Result<std::result::Result<PathBuf, String>> {
    if !run.in_container() {
        return Ok(Ok(worktree(run)?.to_path_buf()));
    }
    let commit = run
        .result_commit()
        .with_context(|| format!("run {} has no validated commit to land", run.id()))?;
    repository.landing_worktree(run, commit)
}

/// [`landing_tree`] with a refusal as an error, for the review and the e2e.
pub fn review_tree(repository: &dyn Repository, run: &TaskRun) -> Result<PathBuf> {
    match landing_tree(repository, run)? {
        Ok(tree) => Ok(tree),
        Err(why) => bail!("{why}"),
    }
}

/// The environment a process started in `tree`, the run's
/// [`landing_tree`], gets so that the git it runs is guarded as the
/// control side's own (ADR-t2179-1 decision 8); none for a run on the host.
pub fn tree_env(
    repository: &dyn Repository,
    run: &TaskRun,
    tree: &Path,
) -> Result<Vec<(String, String)>> {
    if run.in_container() {
        repository.guard_env(tree)
    } else {
        Ok(Vec::new())
    }
}

/// What a request that asks `run` to rebase onto main names: main's commit
/// for a run on the host, the clone's ref main's bundle is fetched into for
/// a run in a container (ADR-t2179-1 decision 6).
pub fn rebase_target(run: &TaskRun, main: &CommitSha) -> String {
    if run.in_container() {
        CLONE_MAIN_REF.to_owned()
    } else {
        main.to_string()
    }
}

/// Hand main to a run in a container before a request that may ask it to
/// rebase ([`Repository::hand_main`]); nothing for a run on the host.
pub fn hand_main(repository: &dyn Repository, run: &TaskRun, main: &CommitSha) -> Result<()> {
    if run.in_container() {
        repository.hand_main(run, main)
    } else {
        Ok(())
    }
}

/// Remove what the run left of its workplace: its worktree and branch on
/// the host; its clone, landing worktree, the control side's run branch
/// and its bundles and state file for a run in a container (ADR-t2179-1
/// decision 7).
pub fn remove(repository: &dyn Repository, run: &TaskRun, branch: &str) -> Result<()> {
    if run.in_container() {
        repository.remove_clone(worktree(run)?, branch)
    } else {
        repository.remove_worktree_and_branch(worktree(run)?, branch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Provider, RunId, RunRecord, RunStatus, TaskId};

    fn run_at(worktree: &str) -> TaskRun {
        TaskRun::restore(RunRecord {
            id: RunId::new("r1").unwrap(),
            task_id: TaskId::new(1),
            status: RunStatus::Running,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            worker_mode: crate::domain::worker::WorkerMode::Headless,
            base_commit: CommitSha::try_from("a".repeat(40)).unwrap(),
            branch: Some("dagq/r1".into()),
            worktree_path: Some(worktree.into()),
            workspace_id: None,
            receipt_path: None,
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: Some("/q/runs/r1".into()),
            last_error: None,
            workspace_closed_at: None,
            created_at: String::new(),
        })
        .unwrap()
    }

    #[test]
    fn a_container_run_is_asked_to_rebase_onto_its_clones_main_ref() {
        let main = CommitSha::try_from("b".repeat(40)).unwrap();
        assert_eq!(
            rebase_target(&run_at("/q/clones/r1"), &main),
            CLONE_MAIN_REF
        );
        assert_eq!(
            rebase_target(&run_at("/q/runs/r1/worktree"), &main),
            main.as_str()
        );
    }
}
