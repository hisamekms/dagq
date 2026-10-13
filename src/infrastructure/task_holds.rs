//! The holds of tasks (ADR-t1879-1): `task_holds`, at most one open per
//! task. Holding and releasing are each one write transaction that reads
//! the task (and the hold open on it) and writes the row with its
//! `task_held` or `task_released` event, so two holds at once open one
//! and the event never disagrees with the row. The guard every step of a
//! task asks (`replans::check_task_step`) and the ready query read the same
//! rows inside the step's own transaction, so a claim, a resume or a
//! recovery round and a hold are ordered by the store: whichever commits
//! first wins.
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::json;

use super::sqlite::{SqliteQueue, event, read_task};
use crate::domain::event_kind::EventKind;
use crate::domain::task_hold::{
    self, HoldGuard, HoldRecorded, HoldState, HoldTarget, ProgressVersion, ReleaseRecorded,
    RunHoldWait, TaskHold,
};
use crate::domain::task_replan::GuardedStep;
use crate::domain::{HoldId, RunId, RunStatus, TaskId, TaskStatus};

impl SqliteQueue {
    /// Hold the task `target` names for `reason` until `until` (a time to
    /// remind a person, unix seconds), as `by_role` with actor id `by_id`.
    /// In one transaction: resolve a run to its task, check the task takes
    /// a hold ([`task_hold::check_hold`]), insert the row and record
    /// `task_held`. The run's wait is [`RunHoldWait::None`] (a running
    /// turn is not reached by a hold yet), so the hold is `held` at once.
    pub fn hold_task(
        &mut self,
        target: &HoldTarget,
        reason: &str,
        until: Option<i64>,
        by_role: &str,
        by_id: &str,
    ) -> Result<HoldRecorded> {
        let now = self.generators.clock.now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task_id = resolve(&tx, target)?;
        let task = read_task(&tx, task_id)?;
        let open = open_hold_of(&tx, task_id)?;
        task_hold::check_hold(task_id, task.status(), open.as_ref(), reason)?;
        let state = task_hold::held_state(RunHoldWait::None);
        let named = match target {
            HoldTarget::Run(run) => Some(run.clone()),
            HoldTarget::Task(_) => None,
        };
        tx.execute(
            "INSERT INTO task_holds(task_id, run_id, reason, until, by_role, by_id,
               created_at, settled_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                task_id,
                named,
                reason.trim(),
                until,
                by_role,
                by_id,
                now,
                (state == HoldState::Held).then_some(now),
            ],
        )?;
        let id = HoldId::new(tx.last_insert_rowid());
        let latest = latest_run(&tx, task_id)?;
        event(
            &tx,
            task_id,
            latest.as_ref().map(|(run, _)| run),
            EventKind::TaskHeld,
            json!({"hold_id": id, "reason": reason.trim(), "until": until,
                   "requested": target.requested(),
                   "run_id": latest.as_ref().map(|(run, _)| run),
                   "run_status": latest.as_ref().map(|(_, status)| status),
                   "state": state}),
        )?;
        let hold = read_hold(&tx, id)?;
        tx.commit()?;
        Ok(HoldRecorded { hold, state })
    }

    /// Release the open hold of the task `target` names, as `by_role` with
    /// actor id `by_id`. In one transaction: check there is one and that it
    /// may be released now ([`task_hold::check_release`]; a refusal changes
    /// nothing and records nothing), close the row and record
    /// `task_released`. The supervisor's next pass takes the task by its
    /// own paths again.
    pub fn release_task(
        &mut self,
        target: &HoldTarget,
        by_role: &str,
        by_id: &str,
    ) -> Result<ReleaseRecorded> {
        let now = self.generators.clock.now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task_id = resolve(&tx, target)?;
        let task = read_task(&tx, task_id)?;
        let open = open_hold_of(&tx, task_id)?;
        task_hold::check_release(task_id, open.as_ref(), RunHoldWait::None)?;
        let hold = open.context("checked above")?;
        let continuation = if task.status() == TaskStatus::InProgress {
            "continue"
        } else {
            "claim"
        };
        tx.execute(
            "UPDATE task_holds SET released_at=?2, released_by_role=?3, released_by_id=?4,
               continuation=?5
             WHERE id=?1 AND released_at IS NULL",
            params![hold.id, now, by_role, by_id, continuation],
        )?;
        let latest = latest_run(&tx, task_id)?;
        event(
            &tx,
            task_id,
            latest.as_ref().map(|(run, _)| run),
            EventKind::TaskReleased,
            json!({"hold_id": hold.id, "continuation": continuation,
                   "held_secs": now - hold.created_at,
                   "settled_secs": hold.settled_at.map(|at| now - at)}),
        )?;
        tx.commit()?;
        Ok(ReleaseRecorded { hold, continuation })
    }

    /// The open hold of `task`, if any.
    pub fn task_hold(&self, task: TaskId) -> Result<Option<TaskHold>> {
        open_hold_of(&self.conn, task)
    }

    /// Every open hold of a task that has not ended, oldest first.
    pub fn task_holds(&self) -> Result<Vec<TaskHold>> {
        Ok(self
            .conn
            .prepare(
                "SELECT h.* FROM task_holds h JOIN tasks t ON t.id = h.task_id
                 WHERE h.released_at IS NULL AND t.status NOT IN ('completed','canceled')
                 ORDER BY h.id",
            )?
            .query_map([], hold_row)?
            .collect::<rusqlite::Result<_>>()?)
    }
}

/// The task `target` names; a run the queue does not know is an error.
fn resolve(conn: &Connection, target: &HoldTarget) -> Result<TaskId> {
    match target {
        HoldTarget::Task(task) => Ok(*task),
        HoldTarget::Run(run) => conn
            .query_row("SELECT task_id FROM task_runs WHERE id=?1", [run], |r| {
                r.get(0)
            })
            .optional()?
            .with_context(|| format!("run {run} does not exist")),
    }
}

/// The latest run of `task` and its status, if it has one.
fn latest_run(conn: &Connection, task: TaskId) -> Result<Option<(RunId, String)>> {
    Ok(conn
        .query_row(
            "SELECT id, status FROM task_runs WHERE task_id=?1 ORDER BY rowid DESC LIMIT 1",
            [task],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

fn read_hold(conn: &Connection, id: HoldId) -> Result<TaskHold> {
    Ok(conn.query_row("SELECT * FROM task_holds WHERE id=?1", [id], hold_row)?)
}

fn hold_row(row: &Row<'_>) -> rusqlite::Result<TaskHold> {
    Ok(TaskHold {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        run_id: row.get("run_id")?,
        reason: row.get("reason")?,
        until: row.get("until")?,
        by_role: row.get("by_role")?,
        by_id: row.get("by_id")?,
        created_at: row.get("created_at")?,
        settled_at: row.get("settled_at")?,
    })
}

/// The open hold of `task`, read on `conn` (inside the caller's
/// transaction when it has one), whatever the task's status.
pub(super) fn open_hold_of(conn: &Connection, task: TaskId) -> Result<Option<TaskHold>> {
    Ok(conn
        .query_row(
            "SELECT * FROM task_holds WHERE task_id=?1 AND released_at IS NULL
             ORDER BY id LIMIT 1",
            [task],
            hold_row,
        )
        .optional()?)
}

/// The guard the open hold of `task` puts on its steps, read on `conn`:
/// none for a task that ended, which a hold no longer reaches.
pub(super) fn guard_of(conn: &Connection, task: TaskId) -> Result<Option<HoldGuard>> {
    Ok(conn
        .query_row(
            "SELECT h.* FROM task_holds h JOIN tasks t ON t.id = h.task_id
             WHERE h.task_id=?1 AND h.released_at IS NULL
               AND t.status NOT IN ('completed','canceled')
             ORDER BY h.id LIMIT 1",
            [task],
            hold_row,
        )
        .optional()?
        .map(|hold| hold.guard()))
}

/// The version of `task` and of its run `run` now, read on `conn`: what a
/// kept result is compared with after its hold's release.
pub(super) fn progress_version(
    conn: &Connection,
    task: TaskId,
    run: &RunId,
) -> Result<ProgressVersion> {
    let (status, updated_at): (String, String) = conn.query_row(
        "SELECT status, updated_at FROM tasks WHERE id=?1",
        [task],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let run_status: String =
        conn.query_row("SELECT status FROM task_runs WHERE id=?1", [run], |r| {
            r.get(0)
        })?;
    let events = super::runtime_store::run_events_of(conn, run)?;
    Ok(ProgressVersion::of(
        status.parse()?,
        &updated_at,
        run_status.parse::<RunStatus>()?,
        &events,
    ))
}

impl crate::application::commands::holds::HoldStore for SqliteQueue {
    fn run_task(&self, run: &RunId) -> Result<Option<TaskId>> {
        Ok(self
            .conn
            .query_row("SELECT task_id FROM task_runs WHERE id=?1", [run], |r| {
                r.get(0)
            })
            .optional()?)
    }

    fn hold_task(
        &mut self,
        target: &HoldTarget,
        reason: &str,
        until: Option<i64>,
        by_role: &str,
        by_id: &str,
    ) -> Result<HoldRecorded> {
        SqliteQueue::hold_task(self, target, reason, until, by_role, by_id)
    }

    fn release_task(
        &mut self,
        target: &HoldTarget,
        by_role: &str,
        by_id: &str,
    ) -> Result<ReleaseRecorded> {
        SqliteQueue::release_task(self, target, by_role, by_id)
    }
}

/// Refuse `step` of `task` while its hold guards it, by the same judgment
/// as every other guarded step ([`crate::domain::task_replan::check_step`]).
pub(super) fn check_not_held(conn: &Connection, task: TaskId, step: GuardedStep) -> Result<()> {
    crate::domain::task_replan::check_step(
        &crate::domain::task_replan::TaskGuards {
            replan: None,
            hold: guard_of(conn, task)?,
        },
        step,
    )?;
    Ok(())
}
