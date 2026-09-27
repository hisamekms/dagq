//! Session workspaces, planner sessions and Claude session spans
//! ([`SessionRegistry`]).

use super::*;

impl SqliteQueue {
    /// Record the cmux workspace `up` opened for `role`, replacing any
    /// earlier one: the UUID is how `up` finds it again, never its title
    /// (ADR-0026).
    pub fn register_session_workspace(&self, role: SessionRole, workspace_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session_workspaces(role,workspace_id,created_at) VALUES (?1,?2,?3)
             ON CONFLICT(role) DO UPDATE SET workspace_id=excluded.workspace_id,
                                              created_at=excluded.created_at",
            params![role.as_str(), workspace_id, self.generators.clock.now()],
        )?;
        Ok(())
    }

    /// The workspace last recorded for `role`, whether or not cmux still has it.
    pub fn session_workspace(&self, role: SessionRole) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT workspace_id FROM session_workspaces WHERE role=?1",
                [role.as_str()],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Forget the workspace of `role`; `false` when none was recorded.
    pub fn remove_session_workspace(&self, role: SessionRole) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM session_workspaces WHERE role=?1",
            [role.as_str()],
        )? == 1)
    }

    /// Forget the workspaces recorded for a role `up` no longer opens (the
    /// resident sessions ADR-0024 and ADR-0041 decision 6 retired: the
    /// maintainer and the resident planner): only the in-cmux supervisor's
    /// and the inbox's are kept. Returns how many were forgotten; the
    /// workspaces themselves are a person's to close.
    pub fn forget_retired_session_workspaces(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM session_workspaces WHERE role NOT IN (?1,?2)",
            params![
                SessionRole::Supervisor.as_str(),
                SessionRole::Inbox.as_str()
            ],
        )?)
    }
}

/// The [`SessionRegistry`] port over the inherent methods above, which callers
/// that hold a `SqliteQueue` keep using directly.
impl SessionRegistry for SqliteQueue {
    fn session_workspace(&self, role: SessionRole) -> Result<Option<String>> {
        SqliteQueue::session_workspace(self, role)
    }
    fn register_session_workspace(&self, role: SessionRole, workspace_id: &str) -> Result<()> {
        SqliteQueue::register_session_workspace(self, role, workspace_id)
    }
    fn remove_session_workspace(&self, role: SessionRole) -> Result<bool> {
        SqliteQueue::remove_session_workspace(self, role)
    }
    fn forget_retired_session_workspaces(&self) -> Result<usize> {
        SqliteQueue::forget_retired_session_workspaces(self)
    }
    fn open_planner(
        &self,
        origin: PlannerOrigin,
        proposal: Option<ProposalId>,
    ) -> Result<PlannerSession> {
        SqliteQueue::open_planner(self, origin, proposal)
    }
    fn planner_workspace_created(&self, id: PlannerId, workspace_id: &str) -> Result<()> {
        SqliteQueue::planner_workspace_created(self, id, workspace_id)
    }
    fn close_planner(&self, id: PlannerId, error: Option<&str>) -> Result<PlannerSession> {
        SqliteQueue::close_planner(self, id, error)
    }
    fn planner(&self, id: PlannerId) -> Result<PlannerSession> {
        SqliteQueue::planner(self, id)
    }
    fn planners(&self, all: bool) -> Result<Vec<PlannerSession>> {
        SqliteQueue::planners(self, all)
    }
    fn register_planner_wrapper(&self, id: PlannerId, pid: u32) -> Result<()> {
        SqliteQueue::register_planner_wrapper(self, id, pid)
    }
    fn register_planner_agent(&self, id: PlannerId, wrapper_pid: u32, agent: u32) -> Result<()> {
        SqliteQueue::register_planner_agent(self, id, wrapper_pid, agent)
    }
    fn heartbeat_planner(&self, id: PlannerId, wrapper_pid: u32) -> Result<()> {
        SqliteQueue::heartbeat_planner(self, id, wrapper_pid)
    }
    fn planner_exited(&self, id: PlannerId, wrapper_pid: u32, exit_code: i32) -> Result<()> {
        SqliteQueue::planner_exited(self, id, wrapper_pid, exit_code)
    }
    fn record_session_turns(&self) -> Result<usize> {
        crate::infrastructure::sessions::record_open_turns(&self.conn)
    }
    fn record_session_hook(
        &self,
        hook: &crate::domain::sessions::SessionHook,
    ) -> Result<serde_json::Value> {
        crate::infrastructure::sessions::record_hook(&self.conn, hook)
    }
    fn hook_session_workspaces(&self) -> Result<Vec<(EventId, String)>> {
        crate::infrastructure::sessions::hook_workspaces(&self.conn)
    }
    fn close_gone_sessions(&self, gone: &[EventId]) -> Result<usize> {
        crate::infrastructure::sessions::close_gone_hook_spans(&self.conn, gone)
    }
    fn close_review_session(&self, id: &RunId) -> Result<usize> {
        let _read = read_before(&self.conn, Closing::Run(id, &[event_kind::REVIEW_FAILED]))?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let closed = crate::infrastructure::sessions::close_review(&tx, id)?;
        tx.commit()?;
        Ok(closed)
    }
}
