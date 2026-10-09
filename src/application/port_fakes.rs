//! Fakes of the queue's ports for the unit tests of the use cases that
//! take only those ports: they hold their state in memory, so a test checks
//! a use case's decision without opening SQLite. A method the fake does not
//! keep state for panics, naming it.

use super::*;
use crate::domain::*;
use anyhow::Result;
use std::cell::RefCell;

/// The supervisors' registrations ([`SupervisorRegistry::supervisors`]) and
/// the queue's events ([`EventStore::latest_events_of`],
/// [`EventStore::record_queue_event`]), newest last.
#[derive(Default)]
pub(crate) struct SupervisorsAndEvents {
    pub(crate) registrations: Vec<SupervisorRegistration>,
    pub(crate) events: RefCell<Vec<RunEvent>>,
}

#[allow(unused_variables)]
impl RunReads for SupervisorsAndEvents {
    fn active_runs(&self) -> Result<Vec<TaskRun>> {
        unreachable!("active_runs is not what the test reaches")
    }
    fn all_runs(&self) -> Result<Vec<TaskRun>> {
        unreachable!("all_runs is not what the test reaches")
    }
    fn run(&self, id: &RunId) -> Result<TaskRun> {
        unreachable!("run is not what the test reaches")
    }
    fn runs_with_status(&self, status: RunStatus) -> Result<Vec<TaskRun>> {
        unreachable!("runs_with_status is not what the test reaches")
    }
    fn next_awaiting_integration(&self) -> Result<Option<TaskRun>> {
        unreachable!("next_awaiting_integration is not what the test reaches")
    }
    fn ended_run_workspaces(&self) -> Result<Vec<EndedRunWorkspace>> {
        unreachable!("ended_run_workspaces is not what the test reaches")
    }
    fn ended_run_worktrees(&self) -> Result<Vec<EndedRunWorktree>> {
        unreachable!("ended_run_worktrees is not what the test reaches")
    }
    fn ended_run_worktree(&self, id: &RunId) -> Result<Option<EndedRunWorktree>> {
        unreachable!("ended_run_worktree is not what the test reaches")
    }
    fn latest_runs_in_progress(&self) -> Result<Vec<TaskRun>> {
        unreachable!("latest_runs_in_progress is not what the test reaches")
    }
    fn runs_with_pending_push(&self) -> Result<Vec<TaskRun>> {
        unreachable!("runs_with_pending_push is not what the test reaches")
    }
    fn run_in_workspace(&self, workspace_id: &str) -> Result<Option<RunId>> {
        unreachable!("run_in_workspace is not what the test reaches")
    }
}

#[allow(unused_variables)]
impl EventStore for SupervisorsAndEvents {
    fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        unreachable!("update_events is not what the test reaches")
    }
    fn all_events(&self) -> Result<Vec<RunEvent>> {
        unreachable!("all_events is not what the test reaches")
    }
    fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
        unreachable!("latest_task_events is not what the test reaches")
    }
    fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
        unreachable!("run_events is not what the test reaches")
    }
    fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool> {
        unreachable!("has_run_event is not what the test reaches")
    }
    fn record_runtime_event(
        &self,
        id: &RunId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<()> {
        unreachable!("record_runtime_event is not what the test reaches")
    }
    fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
        unreachable!("last_observe is not what the test reaches")
    }
    fn latest_event_id(&self) -> Result<EventId> {
        unreachable!("latest_event_id is not what the test reaches")
    }
    fn record_backend_failure(
        &self,
        run: Option<&RunId>,
        payload: serde_json::Value,
    ) -> Result<()> {
        unreachable!("record_backend_failure is not what the test reaches")
    }
    fn record_queue_event(&self, kind: EventKind, payload: serde_json::Value) -> Result<EventId> {
        let mut events = self.events.borrow_mut();
        let id = EventId::new(i64::try_from(events.len())? + 1);
        events.push(RunEvent {
            id,
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.as_str().to_owned(),
            payload,
            created_at: String::new(),
            actor: None,
        });
        Ok(id)
    }
    fn latest_event_of(&self, kind: &str) -> Result<Option<RunEvent>> {
        unreachable!("latest_event_of is not what the test reaches")
    }
    fn latest_events_of(&self, kind: &str, limit: usize) -> Result<Vec<RunEvent>> {
        Ok(self
            .events
            .borrow()
            .iter()
            .rev()
            .filter(|event| event.kind == kind)
            .take(limit)
            .cloned()
            .collect())
    }
    fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>> {
        unreachable!("latest_queue_event is not what the test reaches")
    }
    fn latest_events_by_supervisor(
        &self,
        kinds: &[&str],
        supervisors: &[&str],
    ) -> Result<Vec<RunEvent>> {
        unreachable!("latest_events_by_supervisor is not what the test reaches")
    }
    fn events_of_between(
        &self,
        kinds: &[&str],
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        unreachable!("events_of_between is not what the test reaches")
    }
    fn events_between(
        &self,
        after: EventId,
        upto: EventId,
        filter: &EventFilter,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        unreachable!("events_between is not what the test reaches")
    }
}

#[allow(unused_variables)]
impl SupervisorRegistry for SupervisorsAndEvents {
    fn register_supervisor(
        &mut self,
        token: &LeaseToken,
        pid: u32,
        limits: crate::domain::slot_limits::SlotLimits,
        binary_version: &str,
    ) -> Result<SupervisorRegistration> {
        unreachable!("register_supervisor is not what the test reaches")
    }
    fn prune_supervisor(
        &self,
        token: &LeaseToken,
        kind: EventKind,
        stopped: &dyn Fn(&SupervisorRegistration) -> serde_json::Value,
    ) -> Result<bool> {
        unreachable!("prune_supervisor is not what the test reaches")
    }
    fn supervisors(&self) -> Result<Vec<SupervisorRegistration>> {
        Ok(self.registrations.clone())
    }
    fn accept_handoff(&self, token: &LeaseToken) -> Result<()> {
        unreachable!("accept_handoff is not what the test reaches")
    }
    fn request_handoff(&self, token: &LeaseToken, binary: &str) -> Result<bool> {
        unreachable!("request_handoff is not what the test reaches")
    }
    fn handoff_request(&self, token: &LeaseToken) -> Result<Option<String>> {
        unreachable!("handoff_request is not what the test reaches")
    }
    fn take_handoff(&self, token: &LeaseToken, binary: &str) -> Result<bool> {
        unreachable!("take_handoff is not what the test reaches")
    }
    fn cancel_handoff(&self, token: &LeaseToken, binary: &str) -> Result<bool> {
        unreachable!("cancel_handoff is not what the test reaches")
    }
    fn resume_registration(
        &mut self,
        token: &LeaseToken,
        pid: u32,
        binary_version: &str,
    ) -> Result<SupervisorRegistration> {
        unreachable!("resume_registration is not what the test reaches")
    }
    fn set_auto_update(&self, token: &LeaseToken, enabled: bool) -> Result<()> {
        unreachable!("set_auto_update is not what the test reaches")
    }
    fn set_slot_limits(
        &self,
        token: &LeaseToken,
        limits: crate::domain::slot_limits::SlotLimits,
    ) -> Result<()> {
        unreachable!("set_slot_limits is not what the test reaches")
    }
    fn set_max_load(&self, token: &LeaseToken, max_load: Option<f64>) -> Result<()> {
        unreachable!("set_max_load is not what the test reaches")
    }
    fn set_supervisor_providers(
        &self,
        token: &LeaseToken,
        providers: &[crate::domain::worker::ProviderCheck],
    ) -> Result<()> {
        unreachable!("set_supervisor_providers is not what the test reaches")
    }
    fn rebind_repository(&mut self, common_dir: &str) -> Result<Option<String>> {
        unreachable!("rebind_repository is not what the test reaches")
    }
    fn repository_binding(&self) -> Result<Option<String>> {
        unreachable!("repository_binding is not what the test reaches")
    }
    fn bind_repository(&mut self, common_dir: &str) -> Result<()> {
        unreachable!("bind_repository is not what the test reaches")
    }
    fn assert_repository(&self, common_dir: &str) -> Result<()> {
        unreachable!("assert_repository is not what the test reaches")
    }
    fn set_supervisor_mode(
        &self,
        token: &LeaseToken,
        mode: SupervisorMode,
        workspace_id: Option<&str>,
    ) -> Result<()> {
        unreachable!("set_supervisor_mode is not what the test reaches")
    }
}

/// A failed call [`SessionRecords`] recorded: the run it was on and its
/// payload.
pub(crate) type RecordedFailure = (Option<RunId>, serde_json::Value);

/// The runs whose sessions the recording of the session wrappers finds
/// ([`RunReads::run_in_workspace`]), the slots it reports
/// ([`RunCoordination::backend_slots`], with the tokens it asked for) and
/// the failed calls it records ([`EventStore::record_backend_failure`]).
/// Every connection it opens shares what is recorded.
#[derive(Clone, Default)]
pub(crate) struct SessionRecords {
    pub(crate) runs: std::collections::HashMap<String, RunId>,
    pub(crate) slots: (i64, Option<i64>),
    pub(crate) slots_of: std::sync::Arc<std::sync::Mutex<Vec<Option<LeaseToken>>>>,
    pub(crate) failures: std::sync::Arc<std::sync::Mutex<Vec<RecordedFailure>>>,
}

impl QueueOpener<dyn RecordingQueue + Send> for SessionRecords {
    fn open(&self) -> Result<Box<dyn RecordingQueue + Send>> {
        Ok(Box::new(self.clone()))
    }
}

#[allow(unused_variables)]
impl RunReads for SessionRecords {
    fn active_runs(&self) -> Result<Vec<TaskRun>> {
        unreachable!("active_runs is not what the test reaches")
    }
    fn all_runs(&self) -> Result<Vec<TaskRun>> {
        unreachable!("all_runs is not what the test reaches")
    }
    fn run(&self, id: &RunId) -> Result<TaskRun> {
        unreachable!("run is not what the test reaches")
    }
    fn runs_with_status(&self, status: RunStatus) -> Result<Vec<TaskRun>> {
        unreachable!("runs_with_status is not what the test reaches")
    }
    fn next_awaiting_integration(&self) -> Result<Option<TaskRun>> {
        unreachable!("next_awaiting_integration is not what the test reaches")
    }
    fn ended_run_workspaces(&self) -> Result<Vec<EndedRunWorkspace>> {
        unreachable!("ended_run_workspaces is not what the test reaches")
    }
    fn ended_run_worktrees(&self) -> Result<Vec<EndedRunWorktree>> {
        unreachable!("ended_run_worktrees is not what the test reaches")
    }
    fn ended_run_worktree(&self, id: &RunId) -> Result<Option<EndedRunWorktree>> {
        unreachable!("ended_run_worktree is not what the test reaches")
    }
    fn latest_runs_in_progress(&self) -> Result<Vec<TaskRun>> {
        unreachable!("latest_runs_in_progress is not what the test reaches")
    }
    fn runs_with_pending_push(&self) -> Result<Vec<TaskRun>> {
        unreachable!("runs_with_pending_push is not what the test reaches")
    }
    fn run_in_workspace(&self, workspace_id: &str) -> Result<Option<RunId>> {
        Ok(self.runs.get(workspace_id).cloned())
    }
}

#[allow(unused_variables)]
impl EventStore for SessionRecords {
    fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        unreachable!("update_events is not what the test reaches")
    }
    fn all_events(&self) -> Result<Vec<RunEvent>> {
        unreachable!("all_events is not what the test reaches")
    }
    fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
        unreachable!("latest_task_events is not what the test reaches")
    }
    fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
        unreachable!("run_events is not what the test reaches")
    }
    fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool> {
        unreachable!("has_run_event is not what the test reaches")
    }
    fn record_runtime_event(
        &self,
        id: &RunId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<()> {
        unreachable!("record_runtime_event is not what the test reaches")
    }
    fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
        unreachable!("last_observe is not what the test reaches")
    }
    fn latest_event_id(&self) -> Result<EventId> {
        unreachable!("latest_event_id is not what the test reaches")
    }
    fn record_backend_failure(
        &self,
        run: Option<&RunId>,
        payload: serde_json::Value,
    ) -> Result<()> {
        self.failures.lock().unwrap().push((run.cloned(), payload));
        Ok(())
    }
    fn record_queue_event(&self, kind: EventKind, payload: serde_json::Value) -> Result<EventId> {
        unreachable!("record_queue_event is not what the test reaches")
    }
    fn latest_event_of(&self, kind: &str) -> Result<Option<RunEvent>> {
        unreachable!("latest_event_of is not what the test reaches")
    }
    fn latest_events_of(&self, kind: &str, limit: usize) -> Result<Vec<RunEvent>> {
        unreachable!("latest_events_of is not what the test reaches")
    }
    fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>> {
        unreachable!("latest_queue_event is not what the test reaches")
    }
    fn latest_events_by_supervisor(
        &self,
        kinds: &[&str],
        supervisors: &[&str],
    ) -> Result<Vec<RunEvent>> {
        unreachable!("latest_events_by_supervisor is not what the test reaches")
    }
    fn events_of_between(
        &self,
        kinds: &[&str],
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        unreachable!("events_of_between is not what the test reaches")
    }
    fn events_between(
        &self,
        after: EventId,
        upto: EventId,
        filter: &EventFilter,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        unreachable!("events_between is not what the test reaches")
    }
}

#[allow(unused_variables)]
impl RunCoordination for SessionRecords {
    fn heartbeat_leases(&self, token: &LeaseToken) -> Result<usize> {
        unreachable!("heartbeat_leases is not what the test reaches")
    }
    fn release_lease(&mut self, id: &RunId, token: &LeaseToken) -> Result<()> {
        unreachable!("release_lease is not what the test reaches")
    }
    fn holds_lease(&self, id: &RunId, token: &LeaseToken) -> Result<bool> {
        unreachable!("holds_lease is not what the test reaches")
    }
    fn run_leases(&self) -> Result<Vec<RunLease>> {
        unreachable!("run_leases is not what the test reaches")
    }
    fn run_lease(&self, id: &RunId) -> Result<Option<RunLease>> {
        unreachable!("run_lease is not what the test reaches")
    }
    fn heartbeat(&mut self, token: &LeaseToken) -> Result<HeartbeatWrite> {
        unreachable!("heartbeat is not what the test reaches")
    }
    fn processes(&self, id: &RunId) -> Result<Vec<RunProcess>> {
        unreachable!("processes is not what the test reaches")
    }
    fn register_wrapper(&mut self, id: &RunId, token: &LeaseToken, pid: u32) -> Result<()> {
        unreachable!("register_wrapper is not what the test reaches")
    }
    fn register_resume_wrapper(&mut self, id: &RunId, token: &LeaseToken, pid: u32) -> Result<()> {
        unreachable!("register_resume_wrapper is not what the test reaches")
    }
    fn clear_lost_session(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        lost: Option<u32>,
    ) -> Result<()> {
        unreachable!("clear_lost_session is not what the test reaches")
    }
    fn session_reopened(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        workspace: &str,
        attempt: u64,
        repaired: serde_json::Value,
    ) -> Result<()> {
        unreachable!("session_reopened is not what the test reaches")
    }
    fn register_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32) -> Result<()> {
        unreachable!("register_agent is not what the test reaches")
    }
    fn register_resume_agent(
        &mut self,
        id: &RunId,
        wrapper_pid: u32,
        agent_pid: u32,
    ) -> Result<()> {
        unreachable!("register_resume_agent is not what the test reaches")
    }
    fn register_turn_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32) -> Result<()> {
        unreachable!("register_turn_agent is not what the test reaches")
    }
    fn heartbeat_wrapper(&self, id: &RunId, pid: u32) -> Result<()> {
        unreachable!("heartbeat_wrapper is not what the test reaches")
    }
    fn wrapper_exited(&mut self, id: &RunId, pid: u32, exit_code: i32) -> Result<()> {
        unreachable!("wrapper_exited is not what the test reaches")
    }
    fn backend_slots(&self, token: Option<&LeaseToken>) -> Result<(i64, Option<i64>)> {
        self.slots_of.lock().unwrap().push(token.cloned());
        Ok(self.slots)
    }
}

#[allow(unused_variables)]
impl SessionRegistry for SessionRecords {
    fn session_workspace(&self, role: SessionRole) -> Result<Option<String>> {
        unreachable!("session_workspace is not what the test reaches")
    }
    fn register_session_workspace(&self, role: SessionRole, workspace_id: &str) -> Result<()> {
        unreachable!("register_session_workspace is not what the test reaches")
    }
    fn remove_session_workspace(&self, role: SessionRole) -> Result<bool> {
        unreachable!("remove_session_workspace is not what the test reaches")
    }
    fn forget_retired_session_workspaces(&self) -> Result<usize> {
        unreachable!("forget_retired_session_workspaces is not what the test reaches")
    }
    fn open_planner(
        &self,
        origin: PlannerOrigin,
        proposal: Option<ProposalId>,
    ) -> Result<PlannerSession> {
        unreachable!("open_planner is not what the test reaches")
    }
    fn planner_workspace_created(&self, id: PlannerId, workspace_id: &str) -> Result<()> {
        unreachable!("planner_workspace_created is not what the test reaches")
    }
    fn close_planner(&self, id: PlannerId, error: Option<&str>) -> Result<PlannerSession> {
        unreachable!("close_planner is not what the test reaches")
    }
    fn end_planner(&self, id: PlannerId, payload: &serde_json::Value) -> Result<bool> {
        unreachable!("end_planner is not what the test reaches")
    }
    fn planner(&self, id: PlannerId) -> Result<PlannerSession> {
        unreachable!("planner is not what the test reaches")
    }
    fn planners(&self, all: bool) -> Result<Vec<PlannerSession>> {
        unreachable!("planners is not what the test reaches")
    }
    fn set_planner_route(&self, id: PlannerId, route: crate::domain::PlannerRoute) -> Result<()> {
        unreachable!("set_planner_route is not what the test reaches")
    }
    fn planner_turn_events(&self, id: PlannerId) -> Result<Vec<RunEvent>> {
        unreachable!("planner_turn_events is not what the test reaches")
    }
    fn planner_answer_wait(
        &self,
        id: PlannerId,
        asks: &[AskId],
        payload: &serde_json::Value,
    ) -> Result<bool> {
        unreachable!("planner_answer_wait is not what the test reaches")
    }
    fn planner_answer_undelivered(
        &self,
        id: PlannerId,
        ask: AskId,
        payload: &serde_json::Value,
    ) -> Result<bool> {
        unreachable!("planner_answer_undelivered is not what the test reaches")
    }
    fn planner_handover(&self, ask: AskId) -> Result<Option<crate::domain::PlannerHandover>> {
        unreachable!("planner_handover is not what the test reaches")
    }
    fn register_planner_wrapper(&self, id: PlannerId, pid: u32) -> Result<()> {
        unreachable!("register_planner_wrapper is not what the test reaches")
    }
    fn register_planner_agent(&self, id: PlannerId, wrapper_pid: u32, agent: u32) -> Result<()> {
        unreachable!("register_planner_agent is not what the test reaches")
    }
    fn heartbeat_planner(&self, id: PlannerId, wrapper_pid: u32) -> Result<()> {
        unreachable!("heartbeat_planner is not what the test reaches")
    }
    fn planner_exited(&self, id: PlannerId, wrapper_pid: u32, exit_code: i32) -> Result<()> {
        unreachable!("planner_exited is not what the test reaches")
    }
    fn planner_silent(&self, id: PlannerId, payload: serde_json::Value) -> Result<bool> {
        unreachable!("planner_silent is not what the test reaches")
    }
    fn silent_planners(&self) -> Result<Vec<(PlannerSession, RunEvent)>> {
        unreachable!("silent_planners is not what the test reaches")
    }
    fn record_session_turns(&self) -> Result<usize> {
        unreachable!("record_session_turns is not what the test reaches")
    }
    fn record_session_tokens(&self) -> Result<usize> {
        unreachable!("record_session_tokens is not what the test reaches")
    }
    fn record_session_hook(
        &self,
        hook: &crate::domain::sessions::SessionHook,
    ) -> Result<serde_json::Value> {
        unreachable!("record_session_hook is not what the test reaches")
    }
    fn open_hook_session_spans(&self) -> Result<Vec<crate::domain::sessions::OpenSpan>> {
        unreachable!("open_hook_session_spans is not what the test reaches")
    }
    fn close_inferred_sessions(&self, ended: &[EventId]) -> Result<usize> {
        unreachable!("close_inferred_sessions is not what the test reaches")
    }
    fn close_review_session(
        &self,
        id: &RunId,
        session: Option<&crate::domain::headless_job::JobSession>,
    ) -> Result<usize> {
        unreachable!("close_review_session is not what the test reaches")
    }
}
