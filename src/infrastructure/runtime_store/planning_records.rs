//! The lookups of the planning tables that `stats`, `kpi` and `forecast` join with
//! ([`PlanningRecords`]).

use super::*;
use crate::infrastructure::sql_exec::SqlRead;

impl SqliteQueue {
    /// The goal of every task, for `stats`. A pure read.
    pub fn task_goals(&self) -> Result<HashMap<TaskId, Option<GoalId>>> {
        Ok(
            SqlRead::query_map(&*self.conn, "SELECT id, goal_id FROM tasks", &[], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .into_iter()
            .collect(),
        )
    }

    /// The title of every task.
    pub fn task_titles(&self) -> Result<HashMap<TaskId, String>> {
        Ok(
            SqlRead::query_map(&*self.conn, "SELECT id, title FROM tasks", &[], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .into_iter()
            .collect(),
        )
    }

    /// The change of every task (ADR-t980-1), for `stats`, `kpi` and
    /// `forecast`; a value that is not a label is read as none.
    pub fn task_changes(&self) -> Result<HashMap<TaskId, Option<TaskChange>>> {
        Ok(
            SqlRead::query_map(&*self.conn, "SELECT id, change FROM tasks", &[], |row| {
                let change: Option<String> = row.get(1)?;
                Ok((row.get(0)?, change.and_then(|change| change.parse().ok())))
            })?
            .into_iter()
            .collect(),
        )
    }
}

/// The [`PlanningRecords`] port over the inherent methods, which callers
/// that hold a `SqliteQueue` keep using directly.
impl PlanningRecords for SqliteQueue {
    fn task_goals(&self) -> Result<HashMap<TaskId, Option<GoalId>>> {
        SqliteQueue::task_goals(self)
    }
    fn task_titles(&self) -> Result<HashMap<TaskId, String>> {
        SqliteQueue::task_titles(self)
    }
    fn task_changes(&self) -> Result<HashMap<TaskId, Option<TaskChange>>> {
        SqliteQueue::task_changes(self)
    }
    fn draft_origins(&self) -> Result<HashMap<TaskId, crate::domain::DraftOrigin>> {
        SqliteQueue::draft_origins(self)
    }
}
