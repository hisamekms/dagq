//! The workspaces and worktrees of ended runs: what the triage or a
//! landing closes of a run's workspaces, and the supervisor's sweep that
//! closes whatever cmux still lists of the runs the triage never takes
//! (task 180); the build outputs an ended run's worktree holds, and the
//! worktree and branch once its task is over (task 376).

use super::*;
use crate::{
    application::EndedRunWorkspace,
    domain::run::{RunWorkspace, run_workspaces},
};

/// The build outputs removed from the worktree of an ended run: these
/// directories directly under the worktree, unless Git tracks a file in
/// them. `cargo llvm-cov` builds under `target/llvm-cov-target` unless told
/// otherwise.
pub(super) const BUILD_OUTPUT_DIRS: &[&str] = &["target", "llvm-cov-target"];

/// Who closes a run's workspaces through
/// [`Supervisor::close_open_workspaces`]: the triage of a `failed` /
/// `interrupted` run, or the supervisor once a run it landed ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkspaceCloser {
    Triage,
    Supervisor,
}

impl WorkspaceCloser {
    const fn by(self) -> &'static str {
        match self {
            Self::Triage => "triage",
            Self::Supervisor => "supervisor",
        }
    }
    const fn answer(self) -> &'static str {
        match self {
            Self::Triage => "the run was triaged; closed by the runtime",
            Self::Supervisor => "the run ended; closed by the runtime",
        }
    }
    const fn stuck_exit_answer(self) -> &'static str {
        match self {
            Self::Triage => "the triage closed the run's workspace",
            Self::Supervisor => "the supervisor closed the run's workspace",
        }
    }
}

/// Why the sweep closes a workspace of an ended run: a `failed` /
/// `interrupted` run the triage does not take was superseded (its task
/// moved on, or a later run took its place); a landed one just ended.
fn sweep_reason(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Failed | RunStatus::Interrupted => "superseded",
        _ => "ended",
    }
}

impl Supervisor<'_> {
    /// Close the workspaces a run left open, each only while cmux still
    /// lists it: its worker workspace and each resume's, unless its close
    /// is recorded ([`run_workspaces`]). A close records `workspace_closed`
    /// (`by` the closer); a cmux failure records `cleanup_failed` and the
    /// others go on. A `stuck_exit` ask of the run is closed with its
    /// workspace, and its `answer_prompt` and `stalled` asks in any case.
    pub(super) fn close_open_workspaces(
        &mut self,
        run: &TaskRun,
        closer: WorkspaceCloser,
    ) -> Result<()> {
        let events = self.queue.run_events(run.id())?;
        let open: Vec<RunWorkspace> = run_workspaces(run, &events)
            .into_iter()
            .filter(|w| !w.closed)
            .collect();
        let mut closed = false;
        for workspace in open {
            let id = workspace.workspace_id;
            let result = self.cmux.exists(&id).and_then(|listed| {
                if listed {
                    self.cmux.close(&id)?;
                }
                Ok(listed)
            });
            match result {
                Ok(true) => {
                    let mut payload = json!({"by": closer.by()});
                    if let Some(attempt) = workspace.resume_attempt {
                        payload["resume_attempt"] = json!(attempt);
                    }
                    if closer == WorkspaceCloser::Supervisor {
                        payload["reason"] = json!(sweep_reason(run.status()));
                    }
                    self.queue.record_workspace_closed(run.id(), &id, payload)?;
                    closed = true;
                }
                Ok(false) => {}
                Err(error) => self.record_close_failure(run.id(), &id, closer, &error)?,
            }
        }
        if closed {
            self.queue
                .close_stuck_exit_asks(run.id(), closer.stuck_exit_answer())?;
        }
        // Whatever path took the run out of `running`, no dialog of it waits
        // for an answer any more, nor is its session stalled.
        self.queue
            .close_answer_prompt_asks(run.id(), closer.answer())?;
        self.queue.close_stalled_asks(run.id(), closer.answer())?;
        Ok(())
    }
    /// Close what cmux still lists of the workspaces of ended runs the
    /// triage never takes (task 180): runs superseded in their task,
    /// runs of a task that moved on, and landed runs, however they ended
    /// (a hand `integrate` or `recover` included). At most once per
    /// `interval`, and at once on the first pass. cmux's one listing of all windows decides, not
    /// the recorded closes; a workspace it does not list gets no event. A
    /// close records `workspace_closed` (`by: supervisor`, `reason`
    /// `superseded` or `ended`) and closes the run's `stuck_exit`,
    /// `answer_prompt` and `stalled` asks; a cmux failure records
    /// `cleanup_failed` (once per workspace and process; the close is
    /// retried on every sweep) and the others go on. Worktrees, branches and run
    /// directories stay for a person.
    ///
    /// The same pass asks for the disk of the ended runs to be freed
    /// ([`Self::clean_ended_worktrees`]), which a job does off the loop.
    pub(super) fn sweep_ended_runs(&mut self, interval: Duration) -> Result<()> {
        if self
            .last_sweep
            .is_some_and(|last| last.elapsed() < interval)
        {
            return Ok(());
        }
        self.last_sweep = Some(Instant::now());
        self.clean_ended_worktrees(None);
        self.sweep_ended_workspaces()
    }
    fn sweep_ended_workspaces(&mut self) -> Result<()> {
        let candidates: Vec<EndedRunWorkspace> = self
            .queue
            .ended_run_workspaces()?
            .into_iter()
            .filter(|w| !self.slots.iter().any(|slot| *slot.run.id() == w.run_id))
            .collect();
        if candidates.is_empty() {
            return Ok(());
        }
        let listed = match self.cmux.listed_workspace_ids() {
            Ok(listed) => listed,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the workspaces of ended runs could not be swept: {error:#}");
                return Ok(());
            }
        };
        let mut closed_runs: Vec<RunId> = Vec::new();
        for candidate in candidates {
            if !listed
                .iter()
                .any(|id| id.eq_ignore_ascii_case(&candidate.workspace_id))
            {
                continue;
            }
            let workspace = &candidate.workspace_id;
            match self.cmux.close(workspace) {
                Ok(()) => {
                    info!(run_id = %candidate.run_id, "run {} is {}; closed its workspace {workspace} cmux still listed", candidate.run_id, candidate.status.as_str());
                    self.queue.record_workspace_closed(
                        &candidate.run_id,
                        workspace,
                        json!({"by": "supervisor", "reason": sweep_reason(candidate.status)}),
                    )?;
                    if !closed_runs.contains(&candidate.run_id) {
                        closed_runs.push(candidate.run_id);
                    }
                }
                // Retried on the next sweep; recorded once.
                Err(error) if self.sweep_failures.contains(workspace) => {
                    warn!(run_id = %candidate.run_id, "run {}: workspace {workspace} still could not be closed: {error:#}", candidate.run_id);
                }
                Err(error) => {
                    self.sweep_failures.push(workspace.clone());
                    self.record_close_failure(
                        &candidate.run_id,
                        workspace,
                        WorkspaceCloser::Supervisor,
                        &error,
                    )?;
                }
            }
        }
        for run_id in closed_runs {
            let answer = "the run ended; the runtime closed its workspace";
            self.queue.close_stuck_exit_asks(&run_id, answer)?;
            self.queue.close_answer_prompt_asks(&run_id, answer)?;
            self.queue.close_stalled_asks(&run_id, answer)?;
        }
        Ok(())
    }
    /// Free the disk the worktrees of ended runs take, for every such run
    /// or only `task`'s (task 376), off the loop (task 405: a job thread
    /// does it, see [`super::cleanup`]). A run nobody leases and no slot
    /// holds qualifies once it is `integrated`, `succeeded`, `failed` or
    /// `interrupted`, or whatever its status once its task is over:
    ///
    /// - its task `completed` or `canceled`: the worktree and its branch are
    ///   removed, recorded as `worktree_removed` (`path`, `branch`, `bytes`,
    ///   `by: supervisor`, `reason` `task_completed` / `task_canceled`, and
    ///   `repaired: true` when the worktree had to be repaired first, after
    ///   the queue's rebind). A worktree whose directory is already gone
    ///   loses its branch (after `git worktree prune`), recorded the same
    ///   with `bytes` 0 and `worktree_missing: true`;
    /// - otherwise (the task may run it again, or retry it on a new run):
    ///   only the build outputs ([`BUILD_OUTPUT_DIRS`]) go, and the sources,
    ///   commits and run directory stay; recorded as
    ///   `build_outputs_removed` (`paths`, `bytes`, `by: supervisor`).
    ///
    /// `bytes` is what the removed files took on disk. Only a worktree
    /// under the run directory is touched, never the checkout the
    /// supervisor was given. A failure records `cleanup_failed` (`path`,
    /// `message`, `by: supervisor`) once per worktree and process, is
    /// retried on the next sweep, and the others go on.
    pub(super) fn clean_ended_worktrees(&mut self, task: Option<TaskId>) {
        self.request_cleanup(task, None);
    }
    /// [`Self::clean_ended_worktrees`] for `task` as one of its runs ends.
    pub(super) fn clean_task_worktrees(&mut self, task: TaskId) {
        self.clean_ended_worktrees(Some(task));
    }
    /// Record `cleanup_failed` for a workspace of the run cmux could not
    /// close.
    fn record_close_failure(
        &mut self,
        run_id: &RunId,
        workspace: &str,
        closer: WorkspaceCloser,
        error: &anyhow::Error,
    ) -> Result<()> {
        let message = format!("workspace {workspace} could not be closed: {error:#}");
        warn!(run_id = %run_id, "run {run_id}: {message}");
        self.queue.record_runtime_event(
            run_id,
            "cleanup_failed",
            reason_of_error(error, ReasonCode::Other)
                .on(json!({"workspace_id": workspace, "message": message, "by": closer.by()})),
        )
    }
}
