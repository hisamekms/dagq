//! The lookups of the planning tables the other contexts read: the related
//! tasks, the search, and what `stats`, `kpi` and `forecast` join with
//! ([`PlanningRecords`]).

use super::*;

impl SqliteQueue {
    /// The goal of every task, for `stats`. A pure read.
    pub fn task_goals(&self) -> Result<HashMap<TaskId, Option<GoalId>>> {
        Ok(self
            .conn
            .prepare("SELECT id, goal_id FROM tasks")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The title of every task.
    pub fn task_titles(&self) -> Result<HashMap<TaskId, String>> {
        Ok(self
            .conn
            .prepare("SELECT id, title FROM tasks")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The change of every task (ADR-t980-1), for `stats`, `kpi` and
    /// `forecast`; a value that is not a label is read as none.
    pub fn task_changes(&self) -> Result<HashMap<TaskId, Option<TaskChange>>> {
        Ok(self
            .conn
            .prepare("SELECT id, change FROM tasks")?
            .query_map([], |row| {
                let change: Option<String> = row.get(1)?;
                Ok((row.get(0)?, change.and_then(|change| change.parse().ok())))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
}

/// The [`PlanningRecords`] port over the inherent methods, which callers
/// that hold a `SqliteQueue` keep using directly.
impl PlanningRecords for SqliteQueue {
    fn related_landed_commits(&self, task: TaskId, limit: usize) -> Result<Vec<String>> {
        SqliteQueue::related_landed_commits(self, task, limit)
    }
    fn related_tasks(
        &self,
        task: TaskId,
        statuses: &[String],
        limit: usize,
    ) -> Result<RelatedPage> {
        SqliteQueue::related(self, task.as_i64(), statuses, limit)
    }
    fn search_documents(&self, query: &SearchQuery) -> Result<SearchPage> {
        SqliteQueue::search(self, query)
    }
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
