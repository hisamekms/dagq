//! Fakes of the queue's ports for the unit tests of the use cases that
//! take only those ports: they hold their state in memory, so a test checks
//! a use case's decision without opening SQLite. A method the fake does not
//! keep state for panics, naming it.

use super::*;
use crate::domain::*;
use anyhow::Result;
use std::cell::RefCell;

/// The supervisors' registrations ([`SupervisorRegistry::supervisors`]) and
/// the queue's events ([`RunLog::latest_events_of`],
/// [`RunLog::record_queue_event`]), newest last.
#[derive(Default)]
pub(crate) struct SupervisorsAndEvents {
    pub(crate) registrations: Vec<SupervisorRegistration>,
    pub(crate) events: RefCell<Vec<RunEvent>>,
}

#[allow(unused_variables)]
impl RunLog for SupervisorsAndEvents {
    fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        unreachable!("update_events is not what the test reaches")
    }
    fn active_runs(&self) -> Result<Vec<TaskRun>> {
        unreachable!("active_runs is not what the test reaches")
    }
    fn all_runs(&self) -> Result<Vec<TaskRun>> {
        unreachable!("all_runs is not what the test reaches")
    }
    fn all_events(&self) -> Result<Vec<RunEvent>> {
        unreachable!("all_events is not what the test reaches")
    }
    fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
        unreachable!("latest_task_events is not what the test reaches")
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
    fn ended_run_workspaces(&self) -> Result<Vec<EndedRunWorkspace>> {
        unreachable!("ended_run_workspaces is not what the test reaches")
    }
    fn ended_run_worktrees(&self) -> Result<Vec<EndedRunWorktree>> {
        unreachable!("ended_run_worktrees is not what the test reaches")
    }
    fn ended_run_worktree(&self, id: &RunId) -> Result<Option<EndedRunWorktree>> {
        unreachable!("ended_run_worktree is not what the test reaches")
    }
    fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
        unreachable!("last_observe is not what the test reaches")
    }
    fn latest_event_id(&self) -> Result<EventId> {
        unreachable!("latest_event_id is not what the test reaches")
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
