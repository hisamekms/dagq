//! The holds of tasks (ADR-t1879-1): `task_holds`, at most one open per
//! task. Holding and releasing are each one write transaction that reads
//! the task (and the hold open on it) and writes the row with its
//! `task_held` or `task_released` event, so two holds at once open one
//! and the event never disagrees with the row. The guard every step of a
//! task asks (`replans::check_task_step`) and the ready query read the same
//! rows inside the step's own transaction, so a claim, a resume or a
//! recovery round and a hold are ordered by the store: whichever commits
//! first wins.
//!
//! The run's wait for a hold is ordered the same way: the hold reads the
//! run's status and lease and is `held` at once only for a run at rest
//! ([`task_hold::run_wait`]); a run whose step goes settles in the
//! transaction that parks it or finds it at rest ([`settle_open`]), which
//! finds the hold still open and settling, and the release reads whether
//! it settled in its own, so a release while `settling` is refused and a
//! park after a release writes nothing.
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
use crate::domain::{HoldId, RunId, RunStatus, TaskId, TaskStatus, run_phase};

impl SqliteQueue {
    /// Hold the task `target` names for `reason` until `until` (a time to
    /// remind a person, unix seconds), as `by_role` with actor id `by_id`.
    /// In one transaction: resolve a run to its task, check the task takes
    /// a hold ([`task_hold::check_hold`]), insert the row and record
    /// `task_held`. The hold is `settling` while the task's latest run has
    /// a step that goes ([`task_hold::run_wait`]), else `held` at once, an
    /// unfinished run entering its `held` phase in the same transaction.
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
        let latest = latest_run(&tx, task_id)?;
        let latest_status = latest
            .as_ref()
            .map(|(_, status)| status.parse::<RunStatus>())
            .transpose()?;
        let leased = match &latest {
            Some((run, _)) => tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM run_leases WHERE run_id=?1)",
                [run],
                |r| r.get(0),
            )?,
            None => false,
        };
        let writer = match &latest {
            Some((run, _)) => rest_writer(&tx, run, false)?,
            None => task_hold::RestWriter::default(),
        };
        let state = task_hold::held_state(task_hold::rest_wait(latest_status, leased, writer));
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
        if state == HoldState::Held
            && let (Some((run, _)), Some(status)) = (&latest, latest_status)
            && status.is_unfinished()
        {
            enter_held_phase(&tx, run, EventKind::TaskHeld.as_str())?;
        }
        let hold = read_hold(&tx, id)?;
        tx.commit()?;
        Ok(HoldRecorded { hold, state })
    }

    /// Release the open hold of the task `target` names, as `by_role` with
    /// actor id `by_id`. In one transaction: check there is one and that it
    /// may be released now ([`task_hold::check_release`]: refused while the
    /// run has not settled, unless the task ended, which leaves nothing to
    /// settle; a refusal changes nothing and records nothing),
    /// close the row and record `task_released`, which closes the run's
    /// `held` phase with the phase it rests in. The supervisor's next pass
    /// takes the task by its own paths again.
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
        // A task that ended has nothing left to settle: its hold no longer
        // reaches it, and the release only closes the row.
        let ended = matches!(task.status(), TaskStatus::Completed | TaskStatus::Canceled);
        let wait = match open.as_ref().map(TaskHold::state) {
            _ if ended => RunHoldWait::None,
            Some(HoldState::Settling) => RunHoldWait::Settling,
            Some(HoldState::Held) => RunHoldWait::Settled,
            None => RunHoldWait::None,
        };
        task_hold::check_release(task_id, open.as_ref(), wait)?;
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
        if let Some((run, _)) = &latest {
            leave_held_phase(&tx, run)?;
        }
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

impl SqliteQueue {
    /// What fences `task`'s run at its turn's boundary now
    /// ([`crate::domain::task_replan::TaskGuards::fence`]).
    pub fn turn_fence(&self, task: TaskId) -> Result<Option<crate::domain::task_replan::Fence>> {
        Ok(crate::domain::task_replan::TaskGuards {
            replan: super::replans::guard_of(&self.conn, task)?,
            hold: guard_of(&self.conn, task)?,
        }
        .fence())
    }

    /// The open holds of tasks that have not ended whose run has not
    /// settled for them yet, oldest first.
    pub fn settling_holds(&self) -> Result<Vec<TaskHold>> {
        Ok(self
            .conn
            .prepare(
                "SELECT h.* FROM task_holds h JOIN tasks t ON t.id = h.task_id
                 WHERE h.released_at IS NULL AND h.settled_at IS NULL
                   AND t.status NOT IN ('completed','canceled')
                 ORDER BY h.id",
            )?
            .query_map([], hold_row)?
            .collect::<rusqlite::Result<_>>()?)
    }
}

/// Record on `conn` that the open hold `hold` settled at `now`: whether it
/// was still open and settling (a release or another settle that came
/// first leaves it, and the caller writes nothing more).
pub(super) fn settle_open(conn: &Connection, hold: HoldId, now: i64) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE task_holds SET settled_at=?2
         WHERE id=?1 AND released_at IS NULL AND settled_at IS NULL",
        params![hold, now],
    )? == 1)
}

/// Whether a replanning's park of `run` ended `blocked` while it stopped
/// or saved the run's work (ADR-t1521-2): its writer may still run in the
/// worktree, so the run's rest is no safe wait until a look at the
/// worktree finds none.
fn park_blocked(conn: &Connection, run: &RunId) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM replan_operations WHERE run_id=?1 AND status='blocked'
           AND return_phase IN ('pause_requested','quiescing','snapshotting'))",
        [run],
        |r| r.get(0),
    )?)
}

/// Whether a session process of `run` the queue registered has no exit
/// recorded. A lease given back is no writer's end (a resume given up keeps
/// a session whose wrapper may still run its turn), and neither is a
/// heartbeat gone old: a wrapper is dead only when its heartbeat is old
/// and its process is gone, which the supervisor's park looks at, not the
/// store.
fn registered_writer(conn: &Connection, run: &RunId) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_processes WHERE run_id=?1 AND exited_at IS NULL)",
        [run],
        |r| r.get(0),
    )?)
}

/// What may still write `run` as its records tell ([`park_blocked`],
/// [`registered_writer`]), `checked` when the caller's park found the
/// writer ended ([`task_hold::RestWriter`]).
fn rest_writer(conn: &Connection, run: &RunId, checked: bool) -> Result<task_hold::RestWriter> {
    Ok(task_hold::RestWriter {
        park_blocked: park_blocked(conn, run)?,
        registered_process: registered_writer(conn, run)?,
        checked,
    })
}

/// Settle, on `conn` inside the caller's transaction, the open hold of
/// run `run`'s task while it waits for the run's step to end, once the run
/// rests, as [`task_hold::settles_at_rest`] decides from what the records
/// tell ([`rest_writer`]): a run whose writer may still run settles only
/// when the caller's park found the writer ended (`writers_checked`). An
/// unfinished run enters its `held` phase. Whether a hold settled now.
pub(super) fn settle_at_rest_in(
    conn: &Connection,
    run: &RunId,
    writers_checked: bool,
) -> Result<bool> {
    let Some((task, status)) = conn
        .query_row(
            "SELECT task_id, status FROM task_runs WHERE id=?1",
            [run],
            |r| Ok((r.get::<_, TaskId>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
    else {
        return Ok(false);
    };
    let Some(hold) = guard_of(conn, task)? else {
        return Ok(false);
    };
    let status: RunStatus = status.parse()?;
    let leased: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_leases WHERE run_id=?1)",
        [run],
        |r| r.get(0),
    )?;
    let latest = latest_run(conn, task)?.is_some_and(|(latest, _)| latest == *run);
    let writer = rest_writer(conn, run, writers_checked)?;
    if !task_hold::settles_at_rest(status, leased, latest, writer) {
        return Ok(false);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64);
    if !settle_open(conn, hold.hold, now)? {
        return Ok(false);
    }
    if status.is_unfinished() {
        enter_held_phase(conn, run, "hold_settled")?;
    }
    Ok(true)
}

/// Record on `conn` that run `run` entered its hold's wait
/// ([`run_phase::Phase::Held`]), which `cause` moved it to, unless it is
/// the phase last recorded.
pub(super) fn enter_held_phase(conn: &Connection, run: &RunId, cause: &str) -> Result<()> {
    let events = super::runtime_store::run_events_of(conn, run)?;
    record_phase(
        conn,
        run,
        run_phase::next_change(&events, run_phase::Phase::Held, cause),
    )
}

/// Close the hold's wait of run `run` at its release with the phase the
/// run rests in now ([`run_phase::Phase::resting`], which `task_released`
/// among its events lets say), when its last record is the wait.
fn leave_held_phase(conn: &Connection, run: &RunId) -> Result<()> {
    let events = super::runtime_store::run_events_of(conn, run)?;
    let held = run_phase::last_recorded(&events)
        .is_some_and(|last| last.phase == run_phase::Phase::Held.name());
    if !held {
        return Ok(());
    }
    let status: String =
        conn.query_row("SELECT status FROM task_runs WHERE id=?1", [run], |r| {
            r.get(0)
        })?;
    let change = run_phase::Phase::resting(status.parse()?, &events)
        .and_then(|phase| run_phase::next_change(&events, phase, EventKind::TaskReleased.as_str()));
    record_phase(conn, run, change)
}

fn record_phase(
    conn: &Connection,
    run: &RunId,
    change: Option<run_phase::PhaseChange>,
) -> Result<()> {
    let Some(change) = change else {
        return Ok(());
    };
    let task: TaskId = conn.query_row("SELECT task_id FROM task_runs WHERE id=?1", [run], |r| {
        r.get(0)
    })?;
    event(
        conn,
        task,
        Some(run),
        EventKind::RunPhaseChanged,
        change.payload(),
    )
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
pub(super) fn latest_run(conn: &Connection, task: TaskId) -> Result<Option<(RunId, String)>> {
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
