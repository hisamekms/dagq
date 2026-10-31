//! `rebind` (ADR-0020): bind the queue to the repository it moved to, the
//! only way the binding changes.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::path::PathBuf;

use super::{
    Clock, ProcessControl, Repository, RunCoordination, RunFiles, RunLog, SupervisorRegistry,
};
use crate::domain::{RunId, RunStatus, TaskId};

/// Append-only record of `rebind` under the queue's `logs/`, one JSON
/// object per changed binding; the schema has no queue-level event.
pub const REBIND_LOG: &str = "rebind.jsonl";

/// What `rebind` reads and changes through: of the queue, only its
/// registrations, leases and binding, and its runs.
pub struct Rebind<'a, Q: RunCoordination + SupervisorRegistry + RunLog + ?Sized> {
    pub queue: &'a mut Q,
    pub files: &'a dyn RunFiles,
    pub processes: &'a dyn ProcessControl,
    pub clock: &'a dyn Clock,
}

/// Where the queue and the new repository are.
pub struct RebindTarget {
    /// The queue database, canonical.
    pub db: PathBuf,
    /// The Git common directory of the new repository.
    pub common_dir: String,
    pub queue_dir: PathBuf,
    pub log_dir: PathBuf,
    /// The queue directory's `repository` file, rewritten when it exists.
    pub repository_file: PathBuf,
    /// Where a repository-resolved queue of the new repository lives;
    /// `None` without a data home.
    pub repository_queue_dir: Option<PathBuf>,
}

/// Bind the queue to the repository at `target.common_dir` after the
/// repository moved. Refused while a registered supervisor (a hung one with
/// an old heartbeat too; a dead pid's registration is ignored) or an
/// `integrate` still lives, since both hold paths of the old repository.
/// The change is appended to `logs/rebind.jsonl`, the queue directory's
/// `repository` file (if any) is rewritten, and every run worktree still on
/// disk is listed for [`repair_worktrees`], which its caller runs after it:
/// no Git runs in the queue's own operation (ADR-t3102-3 decision 5).
/// Reports where a repository-resolved queue now lives, which differs from
/// the queue's own directory until it is moved there.
pub fn rebind<Q: RunCoordination + SupervisorRegistry + RunLog + ?Sized>(
    ports: Rebind<'_, Q>,
    target: RebindTarget,
) -> Result<Value> {
    let Rebind {
        queue,
        files,
        processes,
        clock,
    } = ports;
    let common_dir = target.common_dir;
    let live = queue
        .supervisors()?
        .into_iter()
        .filter(|registration| processes.alive(registration.pid))
        .map(|registration| registration.pid)
        .collect::<Vec<_>>();
    if let Some(refusal) = rebind_refusal(&live, None) {
        bail!(refusal);
    }
    let leases = queue.run_leases()?;
    let integrating = queue
        .runs_with_status(RunStatus::Integrating)?
        .into_iter()
        .find(|run| {
            leases
                .iter()
                .any(|lease| lease.run_id == *run.id() && processes.alive(lease.pid))
        });
    if let Some(refusal) = rebind_refusal(
        &live,
        integrating.as_ref().map(|run| (run.id(), run.task_id())),
    ) {
        bail!(refusal);
    }
    let previous = queue.rebind_repository(&common_dir)?;
    let changed = previous.as_deref() != Some(common_dir.as_str());
    if changed {
        files.create_dir_all(&target.log_dir)?;
        files.append_line(
            &target.log_dir.join(REBIND_LOG),
            &json!({
                "at": clock.now(),
                "previous_git_common_dir": previous,
                "git_common_dir": common_dir,
                "binary_version": crate::VERSION,
            })
            .to_string(),
        )?;
        if files.is_file(&target.repository_file) {
            files.write(
                &target.repository_file,
                format!("{common_dir}\n").as_bytes(),
            )?;
        }
    }
    let worktrees = queue
        .all_runs()?
        .into_iter()
        // A container run's clone is no worktree of the repository, and no
        // git runs in it (ADR-t2179-1 decision 7).
        .filter(|run| !run.in_container())
        .filter_map(|run| {
            let path = PathBuf::from(run.worktree_path()?);
            Some((run.id().clone(), path))
        })
        .filter(|(_, path)| files.is_dir(path))
        .map(|(run_id, path)| json!({"run_id": run_id, "worktree_path": path}))
        .collect::<Vec<_>>();
    let resolved = target.repository_queue_dir;
    let move_to = resolved
        .clone()
        .filter(|dir| files.canonicalize(dir).ok().as_deref() != Some(target.queue_dir.as_path()));
    Ok(json!({
        "outcome": if changed { "rebound" } else { "unchanged" },
        "db": target.db,
        "previous_git_common_dir": previous,
        "git_common_dir": common_dir,
        "queue_dir": target.queue_dir,
        "repository_queue_dir": resolved,
        "move_to": move_to,
        "worktrees": worktrees,
    }))
}

/// Why the queue may not be rebound now, or `None` when it may: a live
/// supervisor (`live`, the pids of the registered ones that are alive)
/// holds the old repository's paths, and so does an `integrate` in
/// progress (`integrating`, a run integrating under a live lease, with its
/// task). The supervisors are named first.
fn rebind_refusal(live: &[u32], integrating: Option<(&RunId, TaskId)>) -> Option<String> {
    if !live.is_empty() {
        return Some(format!(
            "refusing to rebind while a supervisor is running (pid {live:?}); stop it with `down --wait` first"
        ));
    }
    integrating.map(|(run, task)| {
        format!("refusing to rebind while run {run} of task {task} is integrating")
    })
}

/// The answer of [`rebind`] with each run worktree it lists given its Git
/// link again from `repository`, the one the queue is bound to now: whether
/// it was repaired, and the error when not.
pub fn repair_worktrees(repository: &dyn Repository, mut answer: Value) -> Value {
    if let Some(worktrees) = answer["worktrees"].as_array_mut() {
        for worktree in worktrees {
            let path = worktree["worktree_path"].as_str().map(PathBuf::from);
            let error = match path {
                Some(path) => repository.repair_worktree(&path).err(),
                None => Some(anyhow::anyhow!("the worktree has no path")),
            };
            worktree["repaired"] = json!(error.is_none());
            worktree["error"] = json!(error.map(|e| format!("{e:#}")));
        }
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A live supervisor refuses the rebind before anything else, an
    /// `integrate` in progress refuses it naming its run and task, and
    /// with neither the queue may be rebound (ADR-0020).
    #[test]
    fn a_live_supervisor_or_an_integrating_run_refuses_the_rebind() {
        let run = RunId::new("00000000-0000-4000-8000-000000000001").unwrap();
        let integrating = Some((&run, TaskId::new(4)));
        let supervisor = "refusing to rebind while a supervisor is running (pid [12, 34]); stop it with `down --wait` first";
        assert_eq!(rebind_refusal(&[12, 34], None).as_deref(), Some(supervisor));
        assert_eq!(
            rebind_refusal(&[12, 34], integrating).as_deref(),
            Some(supervisor)
        );
        assert_eq!(
            rebind_refusal(&[], integrating).as_deref(),
            Some(
                "refusing to rebind while run 00000000-0000-4000-8000-000000000001 of task 4 is integrating"
            )
        );
        assert_eq!(rebind_refusal(&[], None), None);
    }
}
