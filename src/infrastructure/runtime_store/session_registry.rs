//! Session workspaces, planner sessions and Claude session spans
//! ([`SessionRegistry`]).

use super::*;
use crate::domain::EventKind;

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

impl SqliteQueue {
    /// Record `planner_unresponsive` about planner `id` of the runtime's
    /// (task 805) with `payload`, which names it by `planner_id` with
    /// `subject: "planner"`: once per planner, `false` when it was recorded
    /// before.
    pub fn planner_silent(&self, id: PlannerId, payload: Value) -> Result<bool> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let seen: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_events WHERE kind=?1
                 AND json_extract(payload,'$.subject')='planner'
                 AND json_extract(payload,'$.planner_id')=?2)",
            params![event_kind::PLANNER_UNRESPONSIVE, id],
            |r| r.get(0),
        )?;
        if seen {
            return Ok(false);
        }
        super::run_log::queue_event(&tx, EventKind::PlannerUnresponsive, &payload)?;
        tx.commit()?;
        Ok(true)
    }

    /// Close the row of planner `id` as the runtime ends it (ADR-t1300-1)
    /// and record `planner_closed` with `payload` in the same transaction,
    /// with what became of the drafts of its bundle (ADR-t807-1): `false`,
    /// with nothing recorded, when the row was closed already.
    pub fn end_planner(&self, id: PlannerId, payload: &Value) -> Result<bool> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let closed = tx.execute(
            "UPDATE planners SET closed_at=?2 WHERE id=?1 AND closed_at IS NULL",
            params![id, now],
        )? == 1;
        if closed {
            crate::infrastructure::draft_planners::settle_bundle(&tx, id, now)?;
            super::run_log::queue_event(&tx, EventKind::PlannerClosed, payload)?;
        }
        tx.commit()?;
        Ok(closed)
    }

    /// The planners not closed that a `planner_unresponsive` names
    /// ([`Self::planner_silent`]), each with that event, oldest first. A
    /// planner a revise went to since is left out: the revise's own
    /// `planner_unresponsive` times it.
    pub fn silent_planners(&self) -> Result<Vec<(PlannerSession, RunEvent)>> {
        let events: Vec<RunEvent> = self
            .conn
            .prepare(
                "SELECT e.* FROM run_events e JOIN planners p
                   ON p.id = json_extract(e.payload,'$.planner_id')
                 WHERE e.kind=?1 AND json_extract(e.payload,'$.subject')='planner'
                   AND p.closed_at IS NULL
                   AND NOT EXISTS (SELECT 1 FROM proposals r WHERE r.status='revising'
                                   AND r.revise_planner_id = p.id)
                 ORDER BY p.id",
            )?
            .query_map([event_kind::PLANNER_UNRESPONSIVE], event_row)?
            .collect::<rusqlite::Result<_>>()?;
        events
            .into_iter()
            .map(|event| {
                let id = event.payload["planner_id"]
                    .as_i64()
                    .ok_or_else(|| anyhow!("event {} names no planner", event.id))?;
                Ok((self.planner(PlannerId::new(id))?, event))
            })
            .collect()
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
    fn set_planner_route(&self, id: PlannerId, route: crate::domain::PlannerRoute) -> Result<()> {
        SqliteQueue::set_planner_route(self, id, route)
    }
    fn planner_turn_events(&self, id: PlannerId) -> Result<Vec<RunEvent>> {
        SqliteQueue::planner_turn_events(self, id)
    }
    fn planner_answer_wait(
        &self,
        id: PlannerId,
        asks: &[crate::domain::AskId],
        payload: &Value,
    ) -> Result<bool> {
        SqliteQueue::planner_answer_wait(self, id, asks, payload)
    }
    fn planner_handover(
        &self,
        ask: crate::domain::AskId,
    ) -> Result<Option<crate::domain::PlannerHandover>> {
        SqliteQueue::planner_handover(self, ask)
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
    fn end_planner(&self, id: PlannerId, payload: &Value) -> Result<bool> {
        SqliteQueue::end_planner(self, id, payload)
    }
    fn planner_silent(&self, id: PlannerId, payload: Value) -> Result<bool> {
        SqliteQueue::planner_silent(self, id, payload)
    }
    fn silent_planners(&self) -> Result<Vec<(PlannerSession, RunEvent)>> {
        SqliteQueue::silent_planners(self)
    }
    fn record_session_turns(&self) -> Result<usize> {
        crate::infrastructure::sessions::record_open_turns(&self.conn)
    }
    fn record_session_tokens(&self) -> Result<usize> {
        crate::infrastructure::session_tokens::record_session_tokens(&self.conn)
    }
    fn record_session_hook(
        &self,
        hook: &crate::domain::sessions::SessionHook,
    ) -> Result<serde_json::Value> {
        crate::infrastructure::sessions::record_hook(&self.conn, hook)
    }
    fn open_hook_session_spans(&self) -> Result<Vec<crate::domain::sessions::OpenSpan>> {
        crate::infrastructure::sessions::hook_spans(&self.conn)
    }
    fn close_inferred_sessions(&self, ended: &[EventId]) -> Result<usize> {
        crate::infrastructure::sessions::close_inferred_hook_spans(&self.conn, ended)
    }
    fn close_review_session(
        &self,
        id: &RunId,
        session: Option<&crate::domain::headless_job::JobSession>,
    ) -> Result<usize> {
        let _read = read_before(&self.conn, Closing::Run(id, &[event_kind::REVIEW_FAILED]))?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let ending = session.map(|session| {
            let mut ending = serde_json::json!({});
            session.record(&mut ending);
            ending
        });
        let closed = crate::infrastructure::sessions::close_review(&tx, id, ending.as_ref())?;
        tx.commit()?;
        Ok(closed)
    }
}
