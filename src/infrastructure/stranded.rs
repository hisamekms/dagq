//! Stranded dependencies (task 421): a task of a closed goal left `draft`,
//! `submitted` or `ready` never completes, so the tasks waiting on it are
//! never claimed. Approve withholding such a task and an `abandoned` close
//! record `dependency_stranded` on it for the inbox, and `status` shows
//! every one from the queue as it is now. Nothing here holds or cancels the
//! waiting tasks: that is the plan's call.
use super::sql_exec::SqlWrite;
use crate::domain::EventKind;
use crate::infrastructure::sql_exec::TransactionEntries;
use crate::infrastructure::store_queue::StoreQueue;
use anyhow::Result;
use serde_json::{Value, json};

use super::sql_exec::{OptionalRow, SqlRead, sql_params};
use super::store_rows::enum_col;
use super::store_writes::event;
use crate::domain::{GoalId, StrandedDependency, TaskId, event_kind};

/// The `cause` of a strand `approve` made by withholding the task is
/// [`event_kind::APPROVE_WITHHELD`].
/// The `cause` when approve readied a task that waits on the strand.
pub(super) const APPROVE_READIED: &str = "approve_readied";
/// This one's is that its goal was closed as `abandoned` with the task unfinished.
pub(super) const GOAL_ABANDONED: &str = "goal_abandoned";

/// The unfinished tasks of closed goals, the only ones that can strand,
/// with their goal and its verdict.
const UNFINISHED_OF_CLOSED: &str = "
    SELECT t.id, g.id AS goal_id, g.verdict FROM tasks t JOIN goals g ON g.id = t.goal_id
    WHERE g.closed_at IS NOT NULL AND t.status IN ('draft','submitted','ready')";

/// The `ready` and `submitted` tasks of open goals (or of none) that wait
/// on `?1`, directly or through other unfinished tasks. A task of a closed
/// goal is not listed: it strands on its own.
const WAITING: &str = "
    WITH RECURSIVE dependents(id) AS (
      SELECT d.task_id FROM task_dependencies d WHERE d.predecessor_id = ?1
      UNION
      SELECT d.task_id FROM task_dependencies d
        JOIN dependents x ON d.predecessor_id = x.id
        JOIN tasks p ON p.id = x.id
        WHERE p.status NOT IN ('completed','canceled')
    )
    SELECT t.id FROM dependents x JOIN tasks t ON t.id = x.id
      LEFT JOIN goals g ON g.id = t.goal_id
    WHERE t.status IN ('ready','submitted') AND g.closed_at IS NULL
    ORDER BY t.id";

/// The unfinished tasks of closed goals `?1` waits on, directly or through
/// other unfinished tasks, by ID.
const UPSTREAM: &str = "
    WITH RECURSIVE predecessors(id) AS (
      SELECT d.predecessor_id FROM task_dependencies d WHERE d.task_id = ?1
      UNION
      SELECT d.predecessor_id FROM task_dependencies d
        JOIN predecessors x ON d.task_id = x.id
        JOIN tasks p ON p.id = x.id
        WHERE p.status NOT IN ('completed','canceled')
    )
    SELECT t.id FROM predecessors x JOIN tasks t ON t.id = x.id
      JOIN goals g ON g.id = t.goal_id
    WHERE g.closed_at IS NOT NULL AND t.status IN ('draft','submitted','ready')
    ORDER BY t.id";

fn upstream(tx: &impl SqlRead, task: TaskId) -> Result<Vec<TaskId>> {
    Ok(tx.query_map(UPSTREAM, sql_params![task], |r| r.get(0))?)
}

/// `task` as a stranded dependency: a task of a closed goal left
/// unfinished that some task waits on, and that waits on no other such
/// task (a chain of them strands its dependents once, at its root).
/// `None` otherwise.
fn stranded(tx: &impl SqlRead, task: TaskId) -> Result<Option<StrandedDependency>> {
    let Some((goal_id, verdict)) = tx
        .query_row(
            &format!("{UNFINISHED_OF_CLOSED} AND t.id = ?1"),
            sql_params![task],
            |r| Ok((r.get::<_, GoalId>("goal_id")?, enum_col(r, "verdict")?)),
        )
        .optional()?
    else {
        return Ok(None);
    };
    // A task that itself waits on such a task is not the strand: its root
    // is, and tells the same waiting tasks once.
    if !upstream(tx, task)?.is_empty() {
        return Ok(None);
    }
    let waiting: Vec<TaskId> = tx.query_map(WAITING, sql_params![task], |r| r.get(0))?;
    Ok((!waiting.is_empty()).then_some(StrandedDependency {
        task_id: task,
        goal_id,
        verdict,
        waiting,
    }))
}

/// Record `dependency_stranded` on `task` when it strands its dependents,
/// with `cause` and the fields of `extra`, in the caller's transaction.
/// The same strand is told once: nothing is recorded while the task's
/// latest `dependency_stranded` names the same waiting tasks.
pub(super) fn record(tx: &impl SqlWrite, task: TaskId, cause: &str, extra: Value) -> Result<()> {
    let Some(stranded) = stranded(tx, task)? else {
        return Ok(());
    };
    let waiting = json!(stranded.waiting);
    let told: Option<String> = tx
        .query_row(
            "SELECT payload FROM run_events WHERE task_id=?1 AND kind=?2
             ORDER BY id DESC LIMIT 1",
            sql_params![task, event_kind::DEPENDENCY_STRANDED],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(told) = told
        && serde_json::from_str::<Value>(&told)?.get("waiting") == Some(&waiting)
    {
        return Ok(());
    }
    let mut payload = json!({
        "goal_id": stranded.goal_id,
        "verdict": stranded.verdict,
        "waiting": waiting,
        "cause": cause,
    });
    if let (Some(payload), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
        payload.extend(extra.clone());
    }
    event(tx, task, None, EventKind::DependencyStranded, payload)
}

/// [`record`] for every unfinished task of the goal just closed as
/// `abandoned`.
pub(super) fn record_abandoned(tx: &impl SqlWrite, goal: GoalId) -> Result<()> {
    let tasks: Vec<TaskId> = tx.query_map(
        &format!("{UNFINISHED_OF_CLOSED} AND g.id = ?1 ORDER BY t.id"),
        sql_params![goal],
        |r| r.get(0),
    )?;
    for task in tasks {
        record(tx, task, GOAL_ABANDONED, json!({}))?;
    }
    Ok(())
}

/// [`record`] for every strand `task` waits on: approve made it `ready`
/// (or a draft of another goal was approved after an abandoned close told
/// its strand without it), so the waiting tasks changed.
pub(super) fn record_upstream(
    tx: &impl SqlWrite,
    task: TaskId,
    cause: &str,
    extra: Value,
) -> Result<()> {
    for stranded in upstream(tx, task)? {
        record(tx, stranded, cause, extra.clone())?;
    }
    Ok(())
}

impl<E: TransactionEntries> StoreQueue<E> {
    /// Every stranded dependency in the queue now, by task ID.
    pub fn stranded_dependencies(&self) -> Result<Vec<StrandedDependency>> {
        let tx = self.read_entry("stranded_dependencies")?;
        let tasks: Vec<TaskId> =
            tx.query_map(&format!("{UNFINISHED_OF_CLOSED} ORDER BY t.id"), &[], |r| {
                r.get(0)
            })?;
        let mut all = Vec::new();
        for task in tasks {
            all.extend(stranded(&tx, task)?);
        }
        Ok(all)
    }
}
