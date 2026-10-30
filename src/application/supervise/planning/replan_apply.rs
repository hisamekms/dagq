//! The trusted runtime's apply of the replacements plan review approved
//! (ADR-t1521-2 decision 5), in the supervisor's planner pass once the
//! replanning requests are settled: each operation waiting for its apply
//! moves to `applying`, its snapshot is checked against its manifest and
//! the ref that pins the parked run's head, and the store then checks the
//! rest again and writes the whole replacement in one transaction, or
//! nothing and holds the operation `blocked` with why. A restart finds an
//! operation left `applying` and goes through the same steps again; one
//! held `blocked` is checked again at each pass without moving it, writing
//! only what changed of its reasons, and applied once nothing refuses it.

use super::*;
use crate::domain::{
    replan_apply::ApplyOutcome,
    task_replan::{ReplanOperation, ReplanStatus},
};

/// Apply every replacement waiting for its apply ([`apply_one`]). One
/// whose apply cannot be recorded now is left where it is for the next
/// pass, and the other operations and the rest of the pass go on.
pub(in crate::application::supervise) fn apply_replacements(
    env: &mut PlanningEnv<'_>,
) -> Result<()> {
    for op in env.queue.replacements_to_apply()? {
        if let Err(error) = apply_one(env, &op) {
            warn!(
                "replan operation {} of task {}: its apply is tried again at the next pass: {error:#}",
                op.id, op.task_id
            );
        }
    }
    Ok(())
}

/// Apply the replacement of `op`: to `applying` (one held `blocked` stays
/// so until it applies), the snapshot checked, then the store's one
/// transaction.
fn apply_one(env: &mut PlanningEnv<'_>, op: &ReplanOperation) -> Result<()> {
    let op = if op.status == ReplanStatus::Blocked {
        op.clone()
    } else {
        env.queue.begin_replan_apply(op.id, op.generation)?
    };
    let problem = check_snapshot(env, &op)
        .err()
        .map(|error| format!("{error:#}"));
    match env
        .queue
        .apply_replacement(op.id, op.generation, problem.as_deref())?
    {
        ApplyOutcome::Applied(applied) => {
            info!(
                "replan operation {} replaced task {} with tasks {:?}",
                applied.id, applied.task_id, applied.replaced_by
            );
        }
        ApplyOutcome::AlreadyApplied => {}
        // One held before and still refused for the same reasons was told
        // when it was held.
        ApplyOutcome::Refused {
            recorded: false, ..
        } => {}
        ApplyOutcome::Refused {
            problems, recovery, ..
        } => {
            warn!(
                "replan operation {} of task {} was not applied and is held blocked with its saved work ({recovery:?}): {}",
                op.id,
                op.task_id,
                problems.join("; ")
            );
        }
    }
    Ok(())
}

/// Whether `op`'s snapshot is as the park saved it: its manifest names
/// the operation and its run, every file reads back with its size and
/// hash, and the ref that pins the run's head still names that head.
fn check_snapshot(env: &PlanningEnv<'_>, op: &ReplanOperation) -> Result<()> {
    let id = op
        .snapshot_id
        .as_deref()
        .with_context(|| format!("replan operation {} has no snapshot", op.id))?;
    let manifest = crate::application::commands::replans::check_snapshot_files(
        &**env.files,
        &env.layout.replans_dir,
        op,
        id,
    )?;
    let pinned = env
        .repository
        .is_ancestor(&manifest.head, &manifest.pinned_ref)
        .and_then(|first| {
            Ok(first
                && env
                    .repository
                    .is_ancestor(&manifest.pinned_ref, &manifest.head)?)
        })
        .unwrap_or(false);
    ensure!(
        pinned,
        "{} does not name the saved head {} of snapshot {id}",
        manifest.pinned_ref,
        manifest.head
    );
    Ok(())
}
