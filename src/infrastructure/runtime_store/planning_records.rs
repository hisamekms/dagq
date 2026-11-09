//! The lookups of the planning tables that `stats`, `kpi` and `forecast` join with
//! ([`PlanningRecords`]).
use crate::infrastructure::sql_exec::TransactionEntries;
use crate::infrastructure::store_queue::StoreQueue;

use super::*;
use crate::infrastructure::sql_exec::SqlRead;

impl<E: TransactionEntries> StoreQueue<E> {
    /// The goal of every task, for `stats`. A pure read.
    pub fn task_goals(&self) -> Result<HashMap<TaskId, Option<GoalId>>> {
        let tx = self.read_entry("task_goals")?;
        Ok(
            SqlRead::query_map(&tx, "SELECT id, goal_id FROM tasks", &[], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .into_iter()
            .collect(),
        )
    }

    /// The title of every task.
    pub fn task_titles(&self) -> Result<HashMap<TaskId, String>> {
        let tx = self.read_entry("task_titles")?;
        Ok(
            SqlRead::query_map(&tx, "SELECT id, title FROM tasks", &[], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .into_iter()
            .collect(),
        )
    }

    /// The change of every task (ADR-t980-1), for `stats`, `kpi` and
    /// `forecast`; a value that is not a label is read as none.
    pub fn task_changes(&self) -> Result<HashMap<TaskId, Option<TaskChange>>> {
        let tx = self.read_entry("task_changes")?;
        Ok(
            SqlRead::query_map(&tx, "SELECT id, change FROM tasks", &[], |row| {
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
impl<E: TransactionEntries> PlanningRecords for StoreQueue<E> {
    fn task_goals(&self) -> Result<HashMap<TaskId, Option<GoalId>>> {
        StoreQueue::task_goals(self)
    }
    fn task_titles(&self) -> Result<HashMap<TaskId, String>> {
        StoreQueue::task_titles(self)
    }
    fn task_changes(&self) -> Result<HashMap<TaskId, Option<TaskChange>>> {
        StoreQueue::task_changes(self)
    }
    fn draft_origins(&self) -> Result<HashMap<TaskId, crate::domain::DraftOrigin>> {
        StoreQueue::draft_origins(self)
    }
}
