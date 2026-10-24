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

use super::{CloneFacts, Repository, RunFiles, WorktreeAdmin};
use crate::domain::{CommitSha, Receipt, TaskRun, run_clone::CLONE_MAIN_REF};

/// What a session left in a run's workplace: its head, and whether it is
/// clean (nothing uncommitted, no rebase in progress for a container run,
/// whose state must also name the imported head).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHead {
    pub head: CommitSha,
    pub clean: bool,
    /// Whether it is what the last turn left: always on the host, whose
    /// worktree is read as it is; for a run in a container, only when its
    /// executor's state is of the end of the last turn it started
    /// ([`RunState::of_the_last_turn`](crate::domain::run_clone::RunState::of_the_last_turn)),
    /// not one a turn cut off with its container left.
    pub last_turn: bool,
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

/// Where the head of a run on the host whose worktree is there is read
/// ([`host_head`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostHead {
    /// Its worktree's git reads and its `HEAD` is the run's branch (or
    /// detached): read there, as [`head`] does.
    Worktree,
    /// Its worktree's registration is broken: the commit of the run's
    /// branch in the repository itself, which is its receipt's commit.
    Branch(CommitSha),
}

/// Where the head of `run`, on the host with its worktree there, is read.
/// Git finds a worktree's own git (its admin dir) by name alone, so when
/// the admin dir is gone the worktree's git does not read, and when
/// another worktree took the name it reads that one's branch. Either way
/// (its symbolic `HEAD` cannot be read, names a branch other than the
/// run's, or is detached in an admin dir that does not point back at it)
/// the worktree's `HEAD` is not the run's, and the run's branch
/// `refs/heads/<branch>` is read in the repository itself instead
/// ([`HostHead::Branch`]), only when it is the commit the run's receipt
/// names: then the worker left its work committed there. An error, which
/// refuses what reads it, when the branch is not there or is not that
/// commit, or the run has no receipt that reads: the broken worktree's
/// uncommitted work cannot be read, and whether to leave it is a person's
/// call. A worktree registered on the run's branch, or detached in its
/// own admin dir, is read as it is ([`HostHead::Worktree`]).
pub fn host_head(
    repository: &dyn Repository,
    files: &dyn RunFiles,
    run: &TaskRun,
) -> Result<HostHead> {
    let branch = format!("refs/heads/{}", run.branch().context("missing branch")?);
    let worktree = worktree(run)?;
    judged(
        run,
        &branch,
        repository.current_branch(worktree),
        || {
            matches!(
                repository.worktree_admin(worktree),
                Ok(WorktreeAdmin::Found {
                    points_back: false,
                    ..
                })
            )
        },
        || {
            let receipt = run
                .receipt_path()
                .context("no receipt path")
                .and_then(|path| Ok(files.read_to_string(Path::new(path))?))
                .and_then(|text| Ok(Receipt::parse(&text)?));
            Ok((repository.branch_head(&branch)?, receipt))
        },
    )
}

/// [`host_head`] over the symbolic `HEAD` the worktree's git reads
/// (`current`), whether a detached one is another worktree's (`taken`: the
/// admin dir its `.git` names does not point back at it), and what `read`
/// reads only when the registration is broken: the branch's commit in the
/// repository and the run's receipt.
fn judged(
    run: &TaskRun,
    branch: &str,
    current: Result<Option<String>>,
    taken: impl FnOnce() -> bool,
    read: impl FnOnce() -> Result<(Option<CommitSha>, Result<Receipt>)>,
) -> Result<HostHead> {
    let broken = match &current {
        Ok(None) if !taken() => return Ok(HostHead::Worktree),
        Ok(None) => "its worktree's .git names another worktree's admin dir".to_owned(),
        Ok(Some(current)) if current == branch => return Ok(HostHead::Worktree),
        Ok(Some(current)) => format!("its worktree's git is on {current}"),
        Err(error) => format!("its worktree's git cannot be read ({error:#})"),
    };
    let refuse = |why: String| -> Result<HostHead> {
        bail!(
            "the worktree of run {} is not registered on its branch: {broken}, and {why}",
            run.id()
        )
    };
    let (head, receipt) = read()?;
    let Some(head) = head else {
        return refuse(format!("{branch} is not in the repository"));
    };
    match receipt {
        Ok(receipt) if receipt.names_commit(head.as_str()) => Ok(HostHead::Branch(head)),
        Ok(receipt) => refuse(format!(
            "{branch} is at {head}, not at its receipt's commit {}",
            receipt.commit()
        )),
        Err(error) => refuse(format!(
            "it has no receipt that names {branch}'s commit {head} ({error:#})"
        )),
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
        Ok(SessionHead {
            head,
            clean,
            last_turn: state.of_the_last_turn(),
        })
    } else {
        let worktree = worktree(run)?;
        Ok(SessionHead {
            head: repository.head(worktree)?,
            clean: repository.status(worktree)?.trim().is_empty(),
            last_turn: true,
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

    fn receipt(commit: &str) -> Result<Receipt> {
        Ok(Receipt::parse(&format!(
            r#"{{"run_id":"r1","result":"failed","commit":"{commit}","tests":{{"status":"failed","evidence_or_reason":"x"}},"e2e":{{"status":"not_applicable","evidence_or_reason":"x"}},"subagent_review":{{"status":"not_applicable","evidence_or_reason":"x"}},"summary":"x"}}"#
        ))?)
    }

    /// A worktree on the run's branch, or detached in its own admin dir,
    /// is read there and its branch is not read; one whose git cannot be
    /// read, is on another branch or is detached in another's admin dir is
    /// read from the run's branch only when that is its
    /// receipt's commit, and refused otherwise.
    #[test]
    fn a_worktree_not_registered_on_its_branch_carries_its_branch_only_at_its_receipts_commit() {
        let run = run_at("/q/runs/r1/worktree");
        let branch = "refs/heads/dagq/r1";
        let (b, c) = ("b".repeat(40), "c".repeat(40));
        let sha = |text: &str| CommitSha::try_from(text.to_owned()).unwrap();
        let unread = || -> Result<(Option<CommitSha>, Result<Receipt>)> {
            unreachable!("a worktree registered on its branch is read there")
        };
        for current in [None, Some(branch.to_owned())] {
            assert_eq!(
                judged(&run, branch, Ok(current), || false, unread).unwrap(),
                HostHead::Worktree
            );
        }
        let gone = || Err(anyhow::anyhow!("fatal: not a git repository"));
        let other = || Ok(Some("refs/heads/dagq/r0".to_owned()));
        let detached = || Ok(None);
        for (current, taken) in [(gone(), false), (other(), false), (detached(), true)] {
            assert_eq!(
                judged(
                    &run,
                    branch,
                    current,
                    || taken,
                    || Ok((Some(sha(&b)), receipt(&b)))
                )
                .unwrap(),
                HostHead::Branch(sha(&b))
            );
        }
        let refused = |current: Result<Option<String>>,
                       read: (Option<CommitSha>, Result<Receipt>),
                       parts: &[&str]| {
            let why = format!(
                "{:#}",
                judged(&run, branch, current, || true, || Ok(read)).unwrap_err()
            );
            for part in ["is not registered on its branch"].iter().chain(parts) {
                assert!(why.contains(part), "{part}: {why}");
            }
        };
        refused(
            gone(),
            (Some(sha(&c)), receipt(&b)),
            &["cannot be read", "not at its receipt's commit"],
        );
        refused(
            other(),
            (Some(sha(&b)), Err(anyhow::anyhow!("receipt is missing"))),
            &["is on refs/heads/dagq/r0", "it has no receipt"],
        );
        refused(gone(), (None, receipt(&b)), &["is not in the repository"]);
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
