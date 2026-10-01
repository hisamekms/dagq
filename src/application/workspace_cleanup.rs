//! `run close-workspaces` (ADR-t1228-1 decision 6): close the workspaces
//! the ended runs left open while no supervisor swept them, on the same
//! terms as the supervisor's sweep (`docs/design/supervisor-lifecycle/
//! run-workspaces.md`), by a run's ID, a task's ID or all at once, without
//! a supervisor. Without `apply` it only lists what it would close (a dry
//! run, the default). The workspaces are found by what the queue recorded,
//! never by their title, and only the ones cmux still lists are closed.
//!
//! - **All** (no ID): the sweep's set ([`RunLog::ended_run_workspaces`]):
//!   the ended runs no live supervisor leases, but the triage's (the
//!   latest `failed` / `interrupted` run of an `in_progress` task).
//! - **A run or a task**: that run, or every ended run of that task, the
//!   triage's included (a run `triage by hand` left), once no live lease,
//!   wrapper or agent is behind it. A run that has not ended (a live or a
//!   waiting one) is refused when named, and skipped of a task.
//!
//! The workspaces `up` recorded for the inbox and the supervisor, and every
//! planner's, are never closed, whatever a run's record says. A close
//! records `workspace_closed` (`by`: the caller's role, `reason:
//! cleanup`) as the caller (the event's actor), and closes the run's
//! `stuck_exit`, `answer_prompt` and `stalled` asks as the sweep does; a
//! cmux failure records `cleanup_failed` and the others go on.

use std::collections::HashSet;

use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::json;

use super::health::lease_health;
use super::{Clock, ProcessControl, Queue, WorkspaceBackend, reason_of_error};
use crate::domain::run::run_workspaces;
use crate::domain::{
    ActorContext, EventKind, ReasonCode, RunId, RunStatus, SessionRole, TaskId, TaskRun,
};

/// The answer the asks of a run whose workspace was closed get.
const ASK_ANSWER: &str = "the run ended; its workspace was closed by `run close-workspaces`";

/// Which ended runs' workspaces to close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupTarget {
    /// The supervisor's sweep's set.
    All,
    Run(RunId),
    Task(TaskId),
}

/// One workspace the command closes, closed, or failed to close.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CleanupWorkspace {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub run_status: RunStatus,
    pub workspace_id: String,
    /// `would_close` (a dry run), `closed` or `failed`.
    pub outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// An ended run of a named task left alone, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedRun {
    pub run_id: RunId,
    pub reason: String,
}

/// What `run close-workspaces` found and did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CleanupReport {
    pub dry_run: bool,
    pub workspaces: Vec<CleanupWorkspace>,
    pub skipped: Vec<SkippedRun>,
}

/// Why nothing may close the workspaces of `run` now: it has not ended, a
/// live supervisor leases it, or its wrapper or agent is alive; `None`
/// when they may be closed.
fn blocker(
    queue: &dyn Queue,
    control: &dyn ProcessControl,
    now: i64,
    run: &TaskRun,
) -> Result<Option<String>> {
    if !matches!(
        run.status(),
        RunStatus::Integrated | RunStatus::Succeeded | RunStatus::Failed | RunStatus::Interrupted
    ) {
        return Ok(Some(format!(
            "run {} is {}; only an ended run's workspaces are closed",
            run.id(),
            run.status().as_str()
        )));
    }
    if let Some(lease) = queue.run_lease(run.id())? {
        let lease = lease_health(&lease, now, control);
        if lease.alive && !lease.stale {
            return Ok(Some(format!(
                "a live supervisor (pid {}) leases run {}",
                lease.pid,
                run.id()
            )));
        }
    }
    for process in queue.processes(run.id())? {
        if process.exited_at.is_none() && control.alive(process.pid) {
            return Ok(Some(format!(
                "the {} of run {} (pid {}) is alive",
                process.role,
                run.id(),
                process.pid
            )));
        }
    }
    Ok(None)
}

/// The workspaces no run cleanup may close: the inbox's, the
/// supervisor's and every planner's.
fn session_workspaces(queue: &dyn Queue) -> Result<HashSet<String>> {
    let mut kept = HashSet::new();
    for role in [SessionRole::Inbox, SessionRole::Supervisor] {
        kept.extend(queue.session_workspace(role)?);
    }
    kept.extend(
        queue
            .planners(true)?
            .into_iter()
            .filter_map(|planner| planner.workspace_id),
    );
    Ok(kept.into_iter().map(|id| id.to_ascii_lowercase()).collect())
}

/// Close (with `apply`) or list the workspaces of the ended runs `target`
/// names; see the module's documentation.
#[allow(clippy::too_many_arguments)]
pub fn close_ended_workspaces(
    queue: &mut dyn Queue,
    cmux: &dyn WorkspaceBackend,
    control: &dyn ProcessControl,
    clock: &dyn Clock,
    actor: &ActorContext,
    target: &CleanupTarget,
    apply: bool,
) -> Result<CleanupReport> {
    let now = clock.now();
    let mut skipped = Vec::new();
    // (run, workspace) in the order of the runs.
    let mut candidates: Vec<(TaskRun, String)> = Vec::new();
    match target {
        CleanupTarget::All => {
            for workspace in queue.ended_run_workspaces()? {
                let run = queue.run(&workspace.run_id)?;
                if let Some(reason) = blocker(&*queue, control, now, &run)? {
                    skipped.push(SkippedRun {
                        run_id: run.id().clone(),
                        reason,
                    });
                    continue;
                }
                candidates.push((run, workspace.workspace_id));
            }
        }
        CleanupTarget::Run(id) => {
            let run = queue.run(id)?;
            if let Some(reason) = blocker(&*queue, control, now, &run)? {
                bail!("refusing to close the workspaces of run {id}: {reason}");
            }
            let events = queue.run_events(id)?;
            for workspace in run_workspaces(&run, &events) {
                candidates.push((run.clone(), workspace.workspace_id));
            }
        }
        CleanupTarget::Task(task) => {
            for run in queue.show(*task)?.runs {
                if let Some(reason) = blocker(&*queue, control, now, &run)? {
                    skipped.push(SkippedRun {
                        run_id: run.id().clone(),
                        reason,
                    });
                    continue;
                }
                let events = queue.run_events(run.id())?;
                for workspace in run_workspaces(&run, &events) {
                    candidates.push((run.clone(), workspace.workspace_id));
                }
            }
        }
    }
    skipped.dedup();
    let kept = session_workspaces(&*queue)?;
    candidates.retain(|(_, workspace)| !kept.contains(&workspace.to_ascii_lowercase()));
    let mut seen = HashSet::new();
    candidates
        .retain(|(run, workspace)| seen.insert((run.id().clone(), workspace.to_ascii_lowercase())));
    let mut workspaces = Vec::new();
    if candidates.is_empty() {
        return Ok(CleanupReport {
            dry_run: !apply,
            workspaces,
            skipped,
        });
    }
    // cmux's list decides, not the recorded closes.
    let listed = cmux.listed_workspace_ids()?;
    let mut closed_runs: Vec<RunId> = Vec::new();
    for (run, workspace) in candidates {
        if !listed.iter().any(|id| id.eq_ignore_ascii_case(&workspace)) {
            continue;
        }
        let mut entry = CleanupWorkspace {
            run_id: run.id().clone(),
            task_id: run.task_id(),
            run_status: run.status(),
            workspace_id: workspace.clone(),
            outcome: "would_close",
            error: None,
        };
        if apply {
            match cmux.close(&workspace) {
                Ok(()) => {
                    queue.record_workspace_closed(
                        run.id(),
                        &workspace,
                        json!({"by": actor.role().as_str(), "reason": "cleanup"}),
                    )?;
                    if !closed_runs.contains(run.id()) {
                        closed_runs.push(run.id().clone());
                    }
                    entry.outcome = "closed";
                }
                Err(error) => {
                    let message = format!("workspace {workspace} could not be closed: {error:#}");
                    queue.record_runtime_event(
                        run.id(),
                        EventKind::CleanupFailed,
                        reason_of_error(&error, ReasonCode::Other).on(json!({
                            "workspace_id": workspace,
                            "message": message,
                            "by": actor.role().as_str(),
                        })),
                    )?;
                    entry.outcome = "failed";
                    entry.error = Some(message);
                }
            }
        }
        workspaces.push(entry);
    }
    for run_id in closed_runs {
        queue.close_stuck_exit_asks(&run_id, ASK_ANSWER)?;
        queue.close_answer_prompt_asks(&run_id, ASK_ANSWER)?;
        queue.end_stalled_detections(&run_id, ASK_ANSWER)?;
    }
    Ok(CleanupReport {
        dry_run: !apply,
        workspaces,
        skipped,
    })
}
