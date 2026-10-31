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

use super::run_container::RunContainers;
use super::run_git::{HostHead, SessionHead};
use super::{PreparedRef, Repository, RunFiles};
use crate::domain::{
    AskId, CommitSha, LeaseToken, RunId, RunStatus, Task, TaskId, TaskRun, TaskStatus,
    actor::ActorRole,
};

/// The ref that keeps a carried-over head after the run's branch is gone.
pub fn run_ref(run: &RunId) -> String {
    format!("refs/dagq/runs/{run}")
}

/// The commit of the run's branch a retry carries over: its validated
/// commit (the one its review passed), or without one the head of its
/// worktree when no rebase is stopped half way there, or its branch's
/// commit in the repository itself when its worktree is not registered on
/// its branch ([`super::run_git::host_head`], an error when that is not its
/// receipt's commit); for a run in a
/// container, the head its clone's facts say once they are of the end of
/// its last turn ([`container_head`], an error refusing it). `None` when
/// the branch holds nothing on top of the run's base. Nothing is written
/// but those facts.
pub fn carried_head(
    run: &TaskRun,
    files: &dyn RunFiles,
    repository: &dyn Repository,
    containers: &dyn RunContainers,
) -> Result<Option<CommitSha>> {
    // The reviewed commit, not whatever an unresolved session left in
    // the worktree (a rebase stopped half way, say).
    let head = match (run.result_commit(), run.worktree_path().map(Path::new)) {
        (Some(commit), _) => Some(commit.clone()),
        (None, Some(worktree)) if files.is_dir(worktree) && run.in_container() => {
            Some(container_head(run, files, repository, containers)?)
        }
        (None, Some(worktree)) if files.is_dir(worktree) => {
            match super::run_git::host_head(repository, files, run)? {
                HostHead::Branch(head) => Some(head),
                HostHead::Worktree if !super::run_git::rebase_in_progress(repository, run)? => {
                    Some(super::run_git::head(repository, run)?)
                }
                HostHead::Worktree => None,
            }
        }
        _ => None,
    };
    Ok(head.filter(|head| head != run.base_commit()))
}

/// What [`container_head`] does next with the facts of a run in a
/// container it read ([`facts_step`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactsStep {
    /// They are of the end of its last turn and say nothing unsaved: the
    /// head they name.
    Use(CommitSha),
    /// They are not of the end of its last turn, or cannot be read: a
    /// facts-only turn writes them anew before anything is judged.
    Refresh,
    /// The head cannot be carried over, for this reason.
    Refuse(String),
}

/// The step for the facts `read` of a run in a container, `refreshed` once
/// a facts-only turn wrote them anew: facts of the end of its last turn
/// that say nothing uncommitted, no rebase stopped and a head its bundle
/// holds are used as they are; others are written anew once
/// ([`FactsStep::Refresh`]); and facts that are still not of that end, or
/// that say what the control side cannot save (it runs no git in the
/// clone), refuse.
pub fn facts_step(read: &std::result::Result<SessionHead, String>, refreshed: bool) -> FactsStep {
    match read {
        Ok(read) if read.last_turn && read.clean => FactsStep::Use(read.head.clone()),
        Ok(read) if read.last_turn => FactsStep::Refuse(
            "its executor recorded uncommitted work, a rebase stopped half way or a head its bundle does not hold; the control side runs no git in the run's clone, so that cannot be saved"
                .to_owned(),
        ),
        _ if !refreshed => FactsStep::Refresh,
        Ok(_) => FactsStep::Refuse(
            "its facts are still not of the end of its last turn after a facts-only turn wrote them anew, so its latest work is not known".to_owned(),
        ),
        Err(why) => FactsStep::Refuse(format!(
            "its facts cannot be read after a facts-only turn wrote them anew: {why}"
        )),
    }
}

/// The head a release, a person's `ready --inherit`, the automatic
/// `retry_inherit` and the triage's own commits read of a run in a
/// container from its facts ([`super::run_git::session_head`]), the one
/// place they are judged: used when they are of the end of its last turn;
/// otherwise (that turn was cut off with its container, or no state is
/// there) written anew by a facts-only turn in its container
/// ([`RunContainers::facts_turn`]) and read again ([`facts_step`]). An
/// error, which refuses what reads it, when the facts-only turn does not
/// run, the clone is gone, or the facts are not of that end or say what
/// cannot be saved: no older head stands for its latest work.
pub fn container_head(
    run: &TaskRun,
    files: &dyn RunFiles,
    repository: &dyn Repository,
    containers: &dyn RunContainers,
) -> Result<CommitSha> {
    refreshed_head(
        run,
        || super::run_git::session_head(repository, run).map_err(|error| format!("{error:#}")),
        || {
            run.worktree_path()
                .is_some_and(|clone| files.is_dir(Path::new(clone)))
        },
        || containers.facts_turn(run),
    )
}

/// [`container_head`] over what reads the facts (`read`), tells whether
/// the clone is there (`clone_there`) and runs the facts-only turn
/// (`refresh`).
fn refreshed_head(
    run: &TaskRun,
    read: impl Fn() -> std::result::Result<SessionHead, String>,
    clone_there: impl FnOnce() -> bool,
    refresh: impl FnOnce() -> Result<()>,
) -> Result<CommitSha> {
    let refuse = |why: String| -> Result<CommitSha> {
        bail!("run {} works in a container: {why}", run.id())
    };
    match facts_step(&read(), false) {
        FactsStep::Use(head) => return Ok(head),
        FactsStep::Refuse(why) => return refuse(why),
        FactsStep::Refresh => {}
    }
    if !clone_there() {
        return refuse(
            "its facts are not of the end of its last turn, and its clone is gone, so no facts-only turn can write them anew".to_owned(),
        );
    }
    if let Err(error) = refresh() {
        return refuse(format!(
            "its facts are not of the end of its last turn, and the facts-only turn that would write them anew did not run: {error:#}"
        ));
    }
    match facts_step(&read(), true) {
        FactsStep::Use(head) => Ok(head),
        FactsStep::Refuse(why) => refuse(why),
        FactsStep::Refresh => unreachable!("a refreshed read is used or refused"),
    }
}

/// Keep `head` under [`run_ref`] so that it outlives the run's branch.
pub fn keep_carried_head(repository: &dyn Repository, run: &RunId, head: &CommitSha) -> Result<()> {
    repository.update_ref(&run_ref(run), head.as_str())
}

/// The head the record of a retry names, kept under [`run_ref`] of `run`
/// before the next run starts from it (ADR-t3294-1): written unless the
/// ref holds it already, over whatever else it holds.
pub fn keep_inherited_head(
    repository: &dyn Repository,
    run: &RunId,
    head: &CommitSha,
) -> Result<()> {
    if repository.ref_value(&run_ref(run))?.as_ref() == Some(head) {
        return Ok(());
    }
    keep_carried_head(repository, run, head)
}

/// Where a person's `ready --inherit` reads the head to carry over and
/// keeps it, in the run's own repository, on the control side (ADR-t3102-3
/// decision 5): [`carried_head`], and [`run_ref`] written before the
/// store's transaction is asked to record the retry and put back when it
/// refuses (ADR-t3294-1, [`keep_head`]).
pub trait CarriedBranches {
    /// [`carried_head`] of a run that ended (`failed` or `interrupted`);
    /// `None` for any other run, whose branch is not read (the store
    /// refuses it with its status).
    fn carried_head(&self, run: &TaskRun) -> Result<Option<CommitSha>>;
    fn keep(&self, run: &TaskRun, head: &CommitSha) -> Result<()>;
    /// The commit [`run_ref`] of `run` holds now; `None` when there is no
    /// such ref.
    fn kept(&self, run: &TaskRun) -> Result<Option<CommitSha>>;
    /// The write of [`run_ref`] of `run` to `value` (`None` deletes it),
    /// refused unless the ref holds `old` now, prepared
    /// ([`Repository::prepare_ref`]): the ref is locked and checked, and does
    /// not move until the handle commits.
    fn prepare_write(
        &self,
        run: &TaskRun,
        value: Option<&CommitSha>,
        old: Option<&CommitSha>,
    ) -> Result<Box<dyn PreparedRef>>;
}

/// The store side of a person's retry that carries a run over: one
/// transaction that checks the preconditions and applies it (T10).
pub trait InheritStore {
    /// Carry the latest run of `request.task` over by hand, or refuse with
    /// the precondition that does not hold
    /// ([`crate::domain::inherit_by_hand::check`]) or with what moved since
    /// [`InheritRequest::at`] was read, and change nothing. The ref of the
    /// head is the caller's, prepared before it asks ([`keep_head`]).
    fn inherit_by_hand(&mut self, request: InheritRequest) -> Result<InheritedByHand>;
    /// Record `records`, the image's records the client of a person's
    /// retry kept as a facts-only turn of a run in a container made its
    /// image ready, as the queue's events of the caller, in one
    /// transaction.
    fn record_images(
        &mut self,
        records: &[(crate::domain::EventKind, serde_json::Value)],
    ) -> Result<()>;
}

/// What a person's `ready --inherit` read of the task and its latest run
/// before the head was chosen, which the store's transaction checks again:
/// a task or run that moved since is refused and nothing changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct InheritReadAt {
    /// The task's status the command was authorized with.
    pub task: TaskStatus,
    /// The latest run; `None` when the task has none.
    pub run: Option<InheritRunAt>,
}

/// The latest run as [`InheritReadAt`] read it: what [`carried_head`]
/// chooses the head by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct InheritRunAt {
    pub id: RunId,
    pub status: RunStatus,
    pub result_commit: Option<CommitSha>,
}

impl InheritRunAt {
    pub fn of(run: &TaskRun) -> Self {
        Self {
            id: run.id().clone(),
            status: run.status(),
            result_commit: run.result_commit().cloned(),
        }
    }
}

/// A person's retry that carries the task's latest run over
/// (`ready --inherit`), as the store checks and applies it.
#[derive(Debug, Clone)]
pub struct InheritRequest {
    pub task: TaskId,
    /// The task's status the record was authorized with.
    pub authorized: TaskStatus,
    /// What the head was chosen from; the store refuses when the task's
    /// status, its latest run, or that run's status or validated commit is
    /// another by then.
    pub at: InheritReadAt,
    /// What [`CarriedBranches::carried_head`] read; `None` is a branch with
    /// nothing to carry over.
    pub head: Option<CommitSha>,
    pub role: ActorRole,
    /// Why the person carries it over, recorded with it.
    pub reason: String,
}

/// The head [`keep_head`] chose and wrote under [`run_ref`] before the
/// record is asked for, and what the ref held before, which
/// [`Self::settle`] puts back when the record is refused.
#[derive(Debug)]
pub struct KeptHead {
    pub head: Option<CommitSha>,
    written: Option<Written>,
}

/// The write [`keep_head`] made: of `run`'s ref, which held `before`.
#[derive(Debug)]
struct Written {
    run: TaskRun,
    before: Option<CommitSha>,
}

/// The control side's Git of a person's `ready --inherit` before the
/// record is asked for (ADR-t3294-1): the head of `run`, the latest run
/// read, chosen as [`CarriedBranches::carried_head`] chooses it, and
/// written under [`run_ref`], refused unless the ref still holds what was
/// read of it, and committed at once: no lock is held across the record,
/// so another writer may move the ref before the retry is recorded. What
/// is promised is the next run's start, from the head its record names,
/// which the supervisor keeps under the ref then
/// ([`keep_inherited_head`]). A ref that holds the head already is left as
/// it is. A head that cannot be kept is an error, and nothing is to be
/// recorded; no ref is touched when there is no head to carry over.
pub fn keep_head(run: Option<&TaskRun>, branches: &dyn CarriedBranches) -> Result<KeptHead> {
    let none = || KeptHead {
        head: None,
        written: None,
    };
    let Some(run) = run else {
        return Ok(none());
    };
    let Some(head) = branches.carried_head(run)? else {
        return Ok(none());
    };
    let unkept = || {
        format!(
            "the head {head} run {} carries over could not be kept as {}; nothing is carried over",
            run.id(),
            run_ref(run.id())
        )
    };
    let before = branches.kept(run).with_context(unkept)?;
    if before.as_ref() == Some(&head) {
        return Ok(KeptHead {
            head: Some(head),
            written: None,
        });
    }
    branches
        .prepare_write(run, Some(&head), before.as_ref())
        .and_then(PreparedRef::commit)
        .with_context(unkept)?;
    Ok(KeptHead {
        head: Some(head),
        written: Some(Written {
            run: run.clone(),
            before,
        }),
    })
}

impl KeptHead {
    /// `recorded`, the answer to the record of the retry, once the ref is
    /// settled by it (ADR-t3294-1). Recorded: the ref is left as it is.
    /// Refused (`refused` says so): the ref is put back to what it held
    /// before (deleted when it held nothing), but only while it still holds
    /// the head written, so another head written since stays; the same head
    /// another call wrote and recorded cannot be told from this one's and
    /// may be put back. A failure whose outcome is not known leaves the
    /// head, which may have been recorded and is the run's own work.
    /// Whatever the ref holds meanwhile, the next run starts from the head
    /// its record names, which the supervisor keeps under the ref then. A
    /// ref that cannot be put back is added to the refusal.
    pub fn settle<T>(
        self,
        branches: &dyn CarriedBranches,
        recorded: Result<T>,
        refused: impl Fn(&anyhow::Error) -> bool,
    ) -> Result<T> {
        let (Some(head), Some(Written { run, before })) = (self.head, self.written) else {
            return recorded;
        };
        let error = match recorded {
            Ok(recorded) => return Ok(recorded),
            Err(error) => error,
        };
        let name = run_ref(run.id());
        if !refused(&error) {
            return Err(error.context(format!(
                "whether the retry was recorded is not known; {name} keeps {head}, the run's own work"
            )));
        }
        match branches
            .prepare_write(&run, before.as_ref(), Some(&head))
            .and_then(PreparedRef::commit)
        {
            Ok(()) => Err(error),
            // Another writer moved the ref since: its value is theirs.
            Err(_) if branches.kept(&run).ok().flatten().as_ref() != Some(&head) => Err(error),
            Err(back) => Err(error.context(format!(
                "{name} could not be put back from {head}: {back:#}"
            ))),
        }
    }
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
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
/// worktree changes. A run in a container is read from its clone's facts
/// with no git in its clone, written anew by a facts-only turn when they
/// are not of the end of its last turn ([`container_head`]): what refuses
/// that head refuses the release.
pub fn released_work(
    run: &TaskRun,
    files: &dyn RunFiles,
    repository: &dyn Repository,
    containers: &dyn RunContainers,
) -> Result<ReleasedRead> {
    let worktree = run
        .worktree_path()
        .map(Path::new)
        .filter(|worktree| files.is_dir(worktree));
    let Some(worktree) = worktree else {
        return Ok(ReleasedRead {
            own: carried_head(run, files, repository, containers)?,
            saved: None,
        });
    };
    if run.in_container() {
        let head = container_head(run, files, repository, containers)
            .context("its work cannot be carried over, and the hold stays")?;
        return Ok(ReleasedRead {
            own: Some(head).filter(|head| head != run.base_commit()),
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::application::memory_files::MemoryFiles;
    use crate::application::run_container::PreparedRun;
    use crate::application::{CloneFacts, CommandSpec, RepositoryReads};
    use crate::domain::run_clone::RunState;
    use crate::domain::{Provider, RunRecord, RunStatus};

    const CLONE: &str = "/q/clones/r1";

    fn sha(c: char) -> CommitSha {
        CommitSha::try_from(c.to_string().repeat(40)).unwrap()
    }

    /// A run in a container with no validated commit, at the base `a`.
    fn container_run() -> TaskRun {
        container_run_at(None)
    }

    /// [`container_run`] holding `result_commit`.
    fn container_run_at(result_commit: Option<CommitSha>) -> TaskRun {
        TaskRun::restore(RunRecord {
            id: RunId::new("r1").unwrap(),
            task_id: TaskId::new(1),
            status: RunStatus::Interrupted,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            worker_mode: crate::domain::worker::WorkerMode::Headless,
            base_commit: sha('a'),
            branch: Some("dagq/r1".into()),
            worktree_path: Some(CLONE.into()),
            workspace_id: None,
            receipt_path: None,
            log_path: None,
            result_commit,
            repo_path: None,
            run_dir: Some("/q/runs/r1".into()),
            last_error: None,
            workspace_closed_at: None,
            created_at: String::new(),
        })
        .unwrap()
    }

    /// The state an executor wrote of the head `head`, at a turn's end
    /// or at the start of one that was cut off, with `status` uncommitted.
    fn state(head: char, ended: Option<bool>, status: &str) -> RunState {
        RunState {
            branch: Some("refs/heads/dagq/r1".into()),
            head: head.to_string().repeat(40),
            status: status.into(),
            rebase_in_progress: false,
            turn_ended: ended,
            generation: None,
        }
    }

    /// The facts as read: the head the bundle imported and the state.
    fn read(head: char, state: RunState) -> std::result::Result<SessionHead, String> {
        let clean = state.status.is_empty() && state.head() == sha(head);
        Ok(SessionHead {
            head: sha(head),
            clean,
            last_turn: state.of_the_last_turn(),
        })
    }

    #[test]
    fn facts_of_the_last_turn_s_end_are_used_and_others_are_written_anew_once() {
        assert_eq!(
            facts_step(&read('b', state('b', Some(true), "")), false),
            FactsStep::Use(sha('b'))
        );
        // Of the end of the last turn but unsaved: refused, not written anew.
        assert!(matches!(
            facts_step(&read('b', state('b', Some(true), " M a")), false),
            FactsStep::Refuse(why) if why.contains("cannot be saved")
        ));
        assert!(matches!(
            facts_step(&read('c', state('b', Some(true), "")), false),
            FactsStep::Refuse(_)
        ));
        // Cut off, unmarked or unreadable: written anew first.
        for facts in [
            read('b', state('b', Some(false), "")),
            read('b', state('b', None, "")),
            read('b', state('b', Some(false), " M a")),
            Err("the run's state file is missing".to_owned()),
        ] {
            assert_eq!(facts_step(&facts, false), FactsStep::Refresh, "{facts:?}");
        }
        // Written anew and still not of that end, or unreadable: refused.
        assert!(matches!(
            facts_step(&read('b', state('b', Some(false), "")), true),
            FactsStep::Refuse(why) if why.contains("still not of the end")
        ));
        assert!(matches!(
            facts_step(&Err("gone".to_owned()), true),
            FactsStep::Refuse(why) if why.contains("gone")
        ));
        assert!(matches!(
            facts_step(&read('b', state('b', Some(true), " M a")), true),
            FactsStep::Refuse(why) if why.contains("cannot be saved")
        ));
    }

    /// The entry takes facts of the last turn's end as they are, writes
    /// others anew once and reads them again, and refuses, with no older
    /// head, when the clone is gone, the facts-only turn does not run, or
    /// what it wrote is still not of that end or unsaved.
    #[test]
    fn the_entry_writes_facts_anew_before_it_judges_and_refuses_with_no_older_head() {
        let run = container_run();
        let facts = |before: RunState, after: RunState| {
            let reads = Mutex::new(vec![read('c', after), read('b', before)]);
            move || reads.lock().unwrap().pop().unwrap()
        };
        let refreshes = Mutex::new(0);
        let refresh = || {
            *refreshes.lock().unwrap() += 1;
            Ok(())
        };
        let head = refreshed_head(
            &run,
            facts(state('b', Some(true), ""), state('c', Some(true), "")),
            || true,
            refresh,
        );
        assert_eq!(head.unwrap(), sha('b'));
        assert_eq!(*refreshes.lock().unwrap(), 0);
        let head = refreshed_head(
            &run,
            facts(state('b', Some(false), ""), state('c', Some(true), "")),
            || true,
            refresh,
        );
        assert_eq!(head.unwrap(), sha('c'));
        assert_eq!(*refreshes.lock().unwrap(), 1);
        let refused = |result: Result<CommitSha>, part: &str| {
            let why = format!("{:#}", result.unwrap_err());
            assert!(why.contains(part), "{part}: {why}");
        };
        refused(
            refreshed_head(
                &run,
                facts(state('b', None, ""), state('c', Some(true), "")),
                || false,
                refresh,
            ),
            "its clone is gone",
        );
        refused(
            refreshed_head(
                &run,
                facts(state('b', Some(false), ""), state('c', Some(true), "")),
                || true,
                || bail!("the machine cannot start"),
            ),
            "the machine cannot start",
        );
        refused(
            refreshed_head(
                &run,
                facts(state('b', Some(false), ""), state('c', Some(false), "")),
                || true,
                refresh,
            ),
            "still not of the end",
        );
        refused(
            refreshed_head(
                &run,
                facts(
                    state('b', Some(false), ""),
                    state('c', Some(true), "?? wip"),
                ),
                || true,
                refresh,
            ),
            "cannot be saved",
        );
        assert_eq!(*refreshes.lock().unwrap(), 3);
    }

    /// A repository whose run in a container has the facts `facts`, which
    /// its containers' facts-only turn replaces with `written` (or fails
    /// to, when `None`).
    struct Facts {
        facts: Mutex<RunState>,
        written: Option<RunState>,
        turns: Mutex<usize>,
    }

    impl Facts {
        fn new(facts: RunState, written: Option<RunState>) -> Self {
            Self {
                facts: Mutex::new(facts),
                written,
                turns: Mutex::new(0),
            }
        }
    }

    impl RunContainers for Facts {
        fn prepare(&self, _: &TaskRun) -> Result<PreparedRun> {
            unreachable!()
        }
        fn prepare_run_dir(&self, _: &Path, _: &Path) -> Result<std::path::PathBuf> {
            unreachable!()
        }
        fn command(&self, _: &[String]) -> CommandSpec {
            unreachable!()
        }
        fn remove(&self, _: &RunId) -> Result<()> {
            unreachable!()
        }
        fn facts_turn(&self, _: &TaskRun) -> Result<()> {
            *self.turns.lock().unwrap() += 1;
            let written = self.written.clone().context("the machine cannot start")?;
            *self.facts.lock().unwrap() = written;
            Ok(())
        }
    }

    impl RepositoryReads for Facts {
        fn main_head(&self) -> Result<CommitSha> {
            unreachable!()
        }
        fn merge_base(&self, _: &str, _: &str) -> Result<Option<CommitSha>> {
            unreachable!()
        }
    }

    impl Repository for Facts {
        fn is_dagq_source(&self) -> bool {
            false
        }
        fn clone_facts(&self, _: &TaskRun) -> Result<std::result::Result<CloneFacts, String>> {
            let state = self.facts.lock().unwrap().clone();
            Ok(Ok(CloneFacts {
                head: state.head(),
                state,
            }))
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
        fn changed_paths(&self, _: &str, _: &str) -> Result<Vec<String>> {
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
        fn main_checkout(&self) -> Result<Option<std::path::PathBuf>> {
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
        fn diff_numbers(&self, _: &str, _: &str) -> Result<crate::application::DiffNumbers> {
            unreachable!()
        }
        fn diff_to_file(&self, _: &str, _: &str, _: &Path) -> Result<()> {
            unreachable!()
        }
    }

    /// A retry carries over the run's `result_commit` as the store holds it
    /// when the retry is made, whatever that commit is; its clone or
    /// worktree is not read for it. One that is the run's base carries
    /// nothing.
    #[test]
    fn the_carried_head_is_the_result_commit_the_run_holds_now() {
        let run = container_run_at;
        let files = MemoryFiles::default();
        files.put_dir(Path::new(CLONE));
        // Facts that refuse: read, they would fail the call.
        let facts = Facts::new(state('b', Some(false), ""), None);
        assert_eq!(
            carried_head(&run(Some(sha('d'))), &files, &facts, &facts).unwrap(),
            Some(sha('d'))
        );
        assert_eq!(*facts.turns.lock().unwrap(), 0);
        assert_eq!(
            carried_head(&run(Some(sha('a'))), &files, &facts, &facts).unwrap(),
            None
        );
        assert!(carried_head(&run(None), &files, &facts, &facts).is_err());
    }

    /// The release's and the retry's reads of a container run both go
    /// through the entry: facts a cut-off turn left are written anew and
    /// the head the facts-only turn found is carried, never the older one;
    /// a facts-only turn that cannot run refuses both.
    #[test]
    fn the_release_and_the_carried_head_read_a_cut_off_container_run_through_the_entry() {
        let run = container_run();
        let files = MemoryFiles::default();
        files.put_dir(Path::new(CLONE));
        let cut_off = || state('b', Some(false), "");
        let facts = Facts::new(cut_off(), Some(state('c', Some(true), "")));
        assert_eq!(
            carried_head(&run, &files, &facts, &facts).unwrap(),
            Some(sha('c'))
        );
        assert_eq!(*facts.turns.lock().unwrap(), 1);
        let facts = Facts::new(cut_off(), Some(state('c', Some(true), "")));
        assert_eq!(
            released_work(&run, &files, &facts, &facts).unwrap(),
            ReleasedRead {
                own: Some(sha('c')),
                saved: None
            }
        );
        assert_eq!(*facts.turns.lock().unwrap(), 1);
        // Facts of the last turn's end are read as they are.
        let facts = Facts::new(state('b', Some(true), ""), None);
        assert_eq!(
            carried_head(&run, &files, &facts, &facts).unwrap(),
            Some(sha('b'))
        );
        assert_eq!(*facts.turns.lock().unwrap(), 0);
        // No facts-only turn: both refuse, and nothing older is carried.
        let facts = Facts::new(cut_off(), None);
        assert!(carried_head(&run, &files, &facts, &facts).is_err());
        let why = format!(
            "{:#}",
            released_work(&run, &files, &facts, &facts).unwrap_err()
        );
        assert!(why.contains("the hold stays"), "{why}");
        assert!(why.contains("the machine cannot start"), "{why}");
    }

    /// The ref of a run as a repository holds it: `head` chosen, the ref's
    /// value, whether a write is prepared (its lock held), whether a write
    /// fails (`refuse_keep`, a ref below its name), and the values the ref
    /// showed, in order.
    #[derive(Clone)]
    struct Refs {
        head: Option<CommitSha>,
        value: Arc<Mutex<Option<CommitSha>>>,
        locked: Arc<Mutex<bool>>,
        refuse_keep: bool,
        shown: Arc<Mutex<Vec<Option<CommitSha>>>>,
    }

    impl Refs {
        fn new(head: Option<char>, value: Option<char>, refuse_keep: bool) -> Self {
            Self {
                head: head.map(sha),
                value: Arc::new(Mutex::new(value.map(sha))),
                locked: Arc::new(Mutex::new(false)),
                refuse_keep,
                shown: Arc::new(Mutex::new(Vec::new())),
            }
        }
        fn value(&self) -> Option<CommitSha> {
            self.value.lock().unwrap().clone()
        }
        fn locked(&self) -> bool {
            *self.locked.lock().unwrap()
        }
        fn set(&self, value: Option<CommitSha>) {
            *self.value.lock().unwrap() = value.clone();
            self.shown.lock().unwrap().push(value);
        }
        /// The same ref, another writer choosing `head`.
        fn choosing(&self, head: char) -> Self {
            Self {
                head: Some(sha(head)),
                ..self.clone()
            }
        }
    }

    /// A write [`Refs`] prepared.
    struct Prepared {
        refs: Refs,
        value: Option<CommitSha>,
    }

    impl PreparedRef for Prepared {
        fn commit(self: Box<Self>) -> Result<()> {
            self.refs.set(self.value.clone());
            Ok(())
        }
        fn abort(self: Box<Self>) -> Result<()> {
            Ok(())
        }
    }

    impl Drop for Prepared {
        fn drop(&mut self) {
            *self.refs.locked.lock().unwrap() = false;
        }
    }

    impl CarriedBranches for Refs {
        fn carried_head(&self, _: &TaskRun) -> Result<Option<CommitSha>> {
            Ok(self.head.clone())
        }
        /// A write that is not prepared, as a release's and a recovery
        /// round's: it fails on the lock.
        fn keep(&self, _: &TaskRun, head: &CommitSha) -> Result<()> {
            if self.locked() {
                bail!("cannot lock ref refs/dagq/runs/r1");
            }
            self.set(Some(head.clone()));
            Ok(())
        }
        fn kept(&self, _: &TaskRun) -> Result<Option<CommitSha>> {
            Ok(self.value())
        }
        fn prepare_write(
            &self,
            _: &TaskRun,
            value: Option<&CommitSha>,
            old: Option<&CommitSha>,
        ) -> Result<Box<dyn PreparedRef>> {
            if self.refuse_keep {
                bail!("a ref is under refs/dagq/runs/r1/");
            }
            if self.locked() {
                bail!("cannot lock ref refs/dagq/runs/r1");
            }
            if self.value().as_ref() != old {
                bail!("refs/dagq/runs/r1 is not at the expected value");
            }
            *self.locked.lock().unwrap() = true;
            Ok(Box::new(Prepared {
                refs: self.clone(),
                value: value.cloned(),
            }))
        }
    }

    /// ADR-t3294-1: the head is written (committed) before the record is
    /// asked for; a recorded retry leaves it, a refused one puts back what
    /// the ref held, and one whose outcome is not known leaves the head. A
    /// ref that held the head already is not written. A head that cannot be
    /// kept stops it before any record, and no lock is held between the
    /// calls.
    #[test]
    fn the_head_is_written_before_the_record_and_put_back_on_a_refusal() {
        let run = container_run();
        let refused = || -> Result<()> { bail!("the run moved") };
        for before in [None, Some('b'), Some('c')] {
            // Recorded.
            let refs = Refs::new(Some('b'), before, false);
            let kept = keep_head(Some(&run), &refs).unwrap();
            assert_eq!(kept.head, Some(sha('b')));
            assert_eq!(refs.value(), Some(sha('b')), "{before:?}");
            assert!(!refs.locked());
            kept.settle(&refs, Ok(()), |_| true).unwrap();
            assert_eq!(refs.value(), Some(sha('b')), "{before:?}");
            let written = refs.shown.lock().unwrap().len();
            assert_eq!(written, usize::from(before != Some('b')), "{before:?}");
            // Refused: what it held.
            let refs = Refs::new(Some('b'), before, false);
            let kept = keep_head(Some(&run), &refs).unwrap();
            let why = kept.settle(&refs, refused(), |_| true).unwrap_err();
            assert_eq!(why.to_string(), "the run moved");
            assert_eq!(refs.value(), before.map(sha), "{before:?}");
            // Not known whether recorded: the head stays.
            let refs = Refs::new(Some('b'), before, false);
            let kept = keep_head(Some(&run), &refs).unwrap();
            let why = format!(
                "{:#}",
                kept.settle(&refs, refused(), |_| false).unwrap_err()
            );
            assert_eq!(refs.value(), Some(sha('b')), "{before:?}");
            assert_eq!(why.contains("is not known"), before != Some('b'), "{why}");
            assert!(!refs.locked());
        }
        // A head that cannot be kept: an error before any record.
        let refs = Refs::new(Some('b'), Some('c'), true);
        let why = format!("{:#}", keep_head(Some(&run), &refs).unwrap_err());
        assert!(why.contains("nothing is carried over"), "{why}");
        assert_eq!(refs.value(), Some(sha('c')));
        // Nothing to carry over, or no run: the ref is not touched.
        for run in [Some(&run), None] {
            let refs = Refs::new(None, Some('c'), false);
            let kept = keep_head(run, &refs).unwrap();
            assert_eq!(kept.head, None);
            assert!(kept.settle(&refs, refused(), |_| true).is_err());
            assert_eq!(refs.value(), Some(sha('c')));
        }
    }

    /// [`Refs`] read before another writer moved them: they read nothing.
    struct ReadEarlier(Refs);

    impl CarriedBranches for ReadEarlier {
        fn carried_head(&self, run: &TaskRun) -> Result<Option<CommitSha>> {
            self.0.carried_head(run)
        }
        fn keep(&self, run: &TaskRun, head: &CommitSha) -> Result<()> {
            self.0.keep(run, head)
        }
        fn kept(&self, _: &TaskRun) -> Result<Option<CommitSha>> {
            Ok(None)
        }
        fn prepare_write(
            &self,
            run: &TaskRun,
            value: Option<&CommitSha>,
            old: Option<&CommitSha>,
        ) -> Result<Box<dyn PreparedRef>> {
            self.0.prepare_write(run, value, old)
        }
    }

    /// The orders of writers on one ref: a refused caller puts the ref
    /// back only while it holds the head it wrote, so another head a writer
    /// recorded before or wrote since stays; a writer that moved the ref
    /// after the caller read it makes the caller's write fail before any
    /// record; a head another writer kept first is not written over. The
    /// one order that leaves no ref, a same-head writer
    /// writing between this caller's write and its put-back, is the
    /// supervisor's to mend when the next run starts
    /// ([`keep_inherited_head`]; the integration tests
    /// `runtime_inherit_by_hand::the_inbox_carries_*` and
    /// `runtime_clone::a_container_runs_clone_takes_the_carried_head_by_its_recorded_id`).
    #[test]
    fn a_refused_caller_puts_back_only_the_head_it_wrote() {
        let run = container_run();
        let refused = || -> Result<()> { bail!("the task is ready now") };
        // The other recorded `c` before this caller kept `b`.
        let refs = Refs::new(Some('b'), None, false);
        let other = keep_head(Some(&run), &refs.choosing('c')).unwrap();
        other.settle(&refs, Ok(()), |_| true).unwrap();
        let kept = keep_head(Some(&run), &refs).unwrap();
        assert!(kept.settle(&refs, refused(), |_| true).is_err());
        assert_eq!(refs.value(), Some(sha('c')));
        // Another writer wrote `c` after this caller wrote `b`.
        let refs = Refs::new(Some('b'), None, false);
        let kept = keep_head(Some(&run), &refs).unwrap();
        refs.keep(&run, &sha('c')).unwrap();
        let why = kept.settle(&refs, refused(), |_| true).unwrap_err();
        assert_eq!(why.to_string(), "the task is ready now");
        assert_eq!(refs.value(), Some(sha('c')));
        // Another writer moved the ref after this caller read it: no write.
        let refs = Refs::new(Some('b'), Some('c'), false);
        assert!(keep_head(Some(&run), &ReadEarlier(refs.clone())).is_err());
        assert_eq!(refs.value(), Some(sha('c')));
        assert!(!refs.locked());
        // A release or a recovery round kept the same head first, and
        // records it after this caller was refused.
        let refs = Refs::new(Some('b'), None, false);
        refs.keep(&run, &sha('b')).unwrap();
        let kept = keep_head(Some(&run), &refs).unwrap();
        assert!(kept.settle(&refs, refused(), |_| true).is_err());
        assert_eq!(refs.value(), Some(sha('b')));
        assert!(!refs.shown.lock().unwrap().contains(&None));
    }
}
