//! The drafts the supervisor registers from `[[recurring]]` of `dagq.toml`
//! (ADR-0072 decision 2, [`crate::domain::recurring`]). A key's history is
//! its rows of origin `recurring` in `draft_origins` (the key in their
//! `material`); a key removed from the file keeps them, and adding it
//! again or editing it does not reset its interval or its overlap check.
//! Registering re-checks the history, the tasks of the key not ended and
//! the interval in one `BEGIN IMMEDIATE` transaction with the draft and its
//! origin, so two supervisors across a handoff never register twice.
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::{
    draft_planners::insert_draft_origin,
    sqlite::{SqliteQueue, insert_task, read_goal},
};
use crate::domain::{
    DraftOrigin, GoalId, TaskId, TaskStatus,
    goal::check_accepts_tasks,
    plan_request::Creator,
    recurring::{
        RecurringDraft, RecurringOutcome, RecurringSchedule, held_back, joined_goal,
        origin_material,
    },
};

/// The last registration of `key`: its task and when (Unix seconds).
fn last_registration(conn: &Connection, key: &str) -> Result<Option<(TaskId, i64)>> {
    Ok(conn
        .query_row(
            "SELECT task_id, created_at FROM draft_origins
             WHERE origin=?1 AND json_extract(material,'$.key')=?2
             ORDER BY created_at DESC, task_id DESC LIMIT 1",
            params![DraftOrigin::Recurring.as_str(), key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// The oldest task registered for `key` that has not ended: a draft, or
/// one adopted and submitted, ready, running, waiting for a person or
/// held, or one a replanning replaced while a task its replacement reaches
/// has not ended ([`super::replan_apply::replacement_open`]).
fn open_task(conn: &Connection, key: &str) -> Result<Option<TaskId>> {
    let tasks: Vec<(TaskId, TaskStatus)> = conn
        .prepare(
            "SELECT o.task_id, t.status FROM draft_origins o JOIN tasks t ON t.id=o.task_id
             WHERE o.origin=?1 AND json_extract(o.material,'$.key')=?2
               AND t.status NOT IN ('completed','canceled')
             ORDER BY o.task_id",
        )?
        .query_map(params![DraftOrigin::Recurring.as_str(), key], |r| {
            Ok((r.get(0)?, super::sqlite::enum_col(r, "status")?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (task, status) in tasks {
        if status != TaskStatus::Replaced || super::replan_apply::replacement_open(conn, task)? {
            return Ok(Some(task));
        }
    }
    Ok(None)
}

/// Whether `goal` accepts tasks now (open, not a draft goal); a goal that
/// is not there accepts none.
fn accepts_tasks(conn: &Connection, goal: Option<GoalId>) -> bool {
    goal.and_then(|goal| read_goal(conn, goal).ok())
        .is_some_and(|read| check_accepts_tasks(&read).is_ok())
}

impl SqliteQueue {
    /// Register the draft of `item` when its interval came at the queue's
    /// now and no task of its key is still to end, with its origin
    /// `recurring` (`material`: the key, the previous registration and the
    /// setting as it is now) and `follow_up_depth` 0, in one write
    /// transaction. A supervisor stopped past the due time registers one,
    /// whose time the next interval counts from.
    pub fn register_recurring_draft(&mut self, item: &RecurringDraft) -> Result<RecurringOutcome> {
        let now = self.generators.clock.now();
        let timestamp = self.generators.clock.timestamp();
        let tx = self.write_tx("register_recurring_draft", TransactionBehavior::Immediate)?;
        let open = open_task(&tx, &item.key)?;
        let last = last_registration(&tx, &item.key)?;
        if let Some(held) = held_back(now, item.interval_days, last.map(|(_, at)| at), open) {
            return Ok(held);
        }
        let goal = joined_goal(item.goal, accepts_tasks(&tx, item.goal));
        let created = insert_task(
            &tx,
            item.new_task(goal),
            &timestamp,
            Creator::Draft(DraftOrigin::Recurring),
        )?;
        let material = origin_material(item, last);
        anyhow::ensure!(
            insert_draft_origin(&tx, created.id(), DraftOrigin::Recurring, &material, now)?,
            "task {} already has an origin",
            created.id()
        );
        tx.commit()?;
        Ok(RecurringOutcome::Registered {
            task_id: created.id(),
        })
    }

    /// When each of `items` is registered next, from the queue's history
    /// of its key.
    pub fn recurring_schedule(&self, items: &[RecurringDraft]) -> Result<Vec<RecurringSchedule>> {
        items
            .iter()
            .map(|item| {
                Ok(RecurringSchedule::of(
                    &item.key,
                    item.interval_days,
                    last_registration(&self.conn, &item.key)?,
                    open_task(&self.conn, &item.key)?,
                ))
            })
            .collect()
    }
}
