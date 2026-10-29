//! Fakes of host運用's queue ([`HostOpsQueue`]) and processes for the unit
//! tests of its passes: the update events a look at the update's jobs
//! reads change once the job's pid is looked at, as when a job records how
//! it ended and exits between the supervisor's read of the events and its
//! look at the pid. A method the fakes keep no state for panics, naming
//! it.

use crate::application::*;
use crate::domain::*;
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// The update events ([`EventStore::update_events`]) `before` the job's
/// pid is looked at and `after`, newest first, with the queue events
/// ([`EventStore::record_queue_event`]) and the asks of the update
/// ([`AskStore::open_update_ask`]) recorded.
#[derive(Default)]
pub(super) struct JobQueue {
    pub(super) before: Vec<RunEvent>,
    pub(super) after: Vec<RunEvent>,
    /// Set once [`EndsWhenLookedAt`] was asked about a pid.
    pub(super) ended: Arc<AtomicBool>,
    /// The kind and payload of each queue event recorded, oldest first.
    pub(super) recorded: Mutex<Vec<(String, serde_json::Value)>>,
    /// The kind of each ask of the update opened, oldest first.
    pub(super) asks: Mutex<Vec<AskKind>>,
}

impl JobQueue {
    /// A queue whose update events are `before` until the job's pid is
    /// looked at and `after` from then on, and the processes that look.
    pub(super) fn new(before: Vec<RunEvent>, after: Vec<RunEvent>) -> (Self, EndsWhenLookedAt) {
        let ended = Arc::new(AtomicBool::new(false));
        let processes = EndsWhenLookedAt {
            ended: ended.clone(),
        };
        let queue = Self {
            before,
            after,
            ended,
            ..Self::default()
        };
        (queue, processes)
    }
}

/// A job whose process is gone once its pid is looked at, and which ended
/// ([`JobQueue::ended`]) just before that look.
pub(super) struct EndsWhenLookedAt {
    ended: Arc<AtomicBool>,
}

impl ProcessControl for EndsWhenLookedAt {
    fn alive(&self, _: u32) -> bool {
        self.ended.store(true, Ordering::SeqCst);
        false
    }
    fn terminate(&self, _: u32) -> Result<()> {
        unreachable!("terminate is not what the test reaches")
    }
    fn interrupt(&self, _: u32) -> Result<()> {
        unreachable!("interrupt is not what the test reaches")
    }
    fn kill(&self, _: u32) -> Result<()> {
        unreachable!("kill is not what the test reaches")
    }
    fn executables(&self) -> Result<Vec<crate::domain::disk::ProcessExecutable>> {
        unreachable!("executables is not what the test reaches")
    }
}

#[allow(unused_variables)]
impl RunReads for JobQueue {
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
impl EventStore for JobQueue {
    fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        Ok(if self.ended.load(Ordering::SeqCst) {
            self.after.clone()
        } else {
            self.before.clone()
        })
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
        let mut recorded = self.recorded.lock().unwrap();
        recorded.push((kind.as_str().to_owned(), payload));
        Ok(EventId::new(100 + i64::try_from(recorded.len())?))
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
impl SupervisorReads for JobQueue {
    fn supervisors(&self) -> Result<Vec<SupervisorRegistration>> {
        unreachable!("supervisors is not what the test reaches")
    }
    fn handoff_request(&self, token: &LeaseToken) -> Result<Option<String>> {
        unreachable!("handoff_request is not what the test reaches")
    }
    fn repository_binding(&self) -> Result<Option<String>> {
        unreachable!("repository_binding is not what the test reaches")
    }
}

#[allow(unused_variables)]
impl SupervisorRegistry for JobQueue {
    fn register_supervisor(
        &mut self,
        token: &LeaseToken,
        pid: u32,
        limits: crate::domain::slot_limits::SlotLimits,
        providers: &[crate::domain::worker::ProviderCheck],
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
    fn accept_handoff(&self, token: &LeaseToken) -> Result<()> {
        unreachable!("accept_handoff is not what the test reaches")
    }
    fn request_handoff(&self, token: &LeaseToken, binary: &str) -> Result<bool> {
        unreachable!("request_handoff is not what the test reaches")
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
        limits: crate::domain::slot_limits::SlotLimits,
        providers: &[crate::domain::worker::ProviderCheck],
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
    fn set_max_load(
        &self,
        token: &LeaseToken,
        max_load: Option<f64>,
        source: Option<crate::domain::slot_limits::SettingSource>,
    ) -> Result<()> {
        unreachable!("set_max_load is not what the test reaches")
    }
    fn rebind_repository(&mut self, common_dir: &str) -> Result<Option<String>> {
        unreachable!("rebind_repository is not what the test reaches")
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

#[allow(unused_variables)]
impl InboxWatchLog for JobQueue {
    fn claim_inbox_nudge(&self, payload: serde_json::Value) -> Result<bool> {
        unreachable!("claim_inbox_nudge is not what the test reaches")
    }
    fn record_inbox_watcher_change(
        &self,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<bool> {
        unreachable!("record_inbox_watcher_change is not what the test reaches")
    }
}

#[allow(unused_variables)]
impl AskStore for JobQueue {
    fn open_update_ask(
        &mut self,
        kind: crate::domain::AskKind,
        question: &str,
        options: &[&str],
        asked_by: &str,
        subject: Option<&str>,
        details: serde_json::Value,
    ) -> Result<crate::domain::Ask> {
        let mut asks = self.asks.lock().unwrap();
        asks.push(kind.clone());
        Ok(Ask {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            id: AskId::new(i64::try_from(asks.len())?),
            kind,
            task_id: None,
            run_id: None,
            question: question.to_owned(),
            options: options.iter().map(|option| (*option).to_owned()).collect(),
            answer: None,
            asked_by: asked_by.to_owned(),
            reason_category: AskReason::Scope,
            subject: subject.map(str::to_owned),
            affected: Vec::new(),
            created_at: 0,
            answered_at: None,
            closed_at: None,
            finding_id: None,
            request_id: None,
            answered_by: None,
            option_index: None,
            answer_authority: None,
            answer_approval: None,
        })
    }
    fn ask_opened_payload(&self, id: crate::domain::AskId) -> Result<serde_json::Value> {
        unreachable!("ask_opened_payload is not what the test reaches")
    }
    fn close_installed_update_ask(
        &mut self,
        id: crate::domain::AskId,
        answer: &str,
        installed: &str,
    ) -> Result<Option<crate::domain::Ask>> {
        unreachable!("close_installed_update_ask is not what the test reaches")
    }
    fn release_updates_on(&self) -> bool {
        unreachable!("release_updates_on is not what the test reaches")
    }
    fn update_answers(&self, kind: &crate::domain::AskKind) -> Result<Vec<crate::domain::Ask>> {
        unreachable!("update_answers is not what the test reaches")
    }
    fn asks(&self, query: AskQuery) -> Result<Vec<Ask>> {
        unreachable!("asks is not what the test reaches")
    }
    fn has_unclosed_ask(&self, run_id: &RunId, kind: AskKind) -> Result<bool> {
        unreachable!("has_unclosed_ask is not what the test reaches")
    }
    fn ask(&mut self, ask: NewAsk) -> Result<AskOutcome> {
        unreachable!("ask is not what the test reaches")
    }
    fn hold(&mut self, hold: crate::domain::NewHold) -> Result<crate::domain::HoldOutcome> {
        unreachable!("hold is not what the test reaches")
    }
    fn hold_of(&self, run_id: &RunId) -> Result<Option<Ask>> {
        unreachable!("hold_of is not what the test reaches")
    }
    fn hold_unclosed(&self, run_id: &RunId) -> Result<bool> {
        unreachable!("hold_unclosed is not what the test reaches")
    }
    fn close_hold_asks(
        &mut self,
        reason: crate::domain::AskReason,
        subject: Option<&str>,
        answer: &str,
    ) -> Result<Vec<Ask>> {
        unreachable!("close_hold_asks is not what the test reaches")
    }
    fn read_ask(&self, id: AskId) -> Result<Ask> {
        unreachable!("read_ask is not what the test reaches")
    }
    fn answer_as(
        &mut self,
        id: AskId,
        text: &str,
        answerer: crate::domain::Answerer,
    ) -> Result<Ask> {
        unreachable!("answer_as is not what the test reaches")
    }
    fn close_ask(&mut self, id: AskId) -> Result<Ask> {
        unreachable!("close_ask is not what the test reaches")
    }
    fn landing_answers(&self) -> Result<Vec<Ask>> {
        unreachable!("landing_answers is not what the test reaches")
    }
    fn triage_answers(&self) -> Result<Vec<Ask>> {
        unreachable!("triage_answers is not what the test reaches")
    }
    fn undelivered_answers(&self, run_id: &RunId) -> Result<Vec<Ask>> {
        unreachable!("undelivered_answers is not what the test reaches")
    }
    fn ask_delivered(&mut self, id: AskId, workspace_id: &str) -> Result<Ask> {
        unreachable!("ask_delivered is not what the test reaches")
    }
    fn has_unclosed_worker_question(&self, run_id: &RunId) -> Result<bool> {
        unreachable!("has_unclosed_worker_question is not what the test reaches")
    }
    fn has_unclosed_worker_question_since(
        &self,
        run_id: &RunId,
        created_from: i64,
    ) -> Result<bool> {
        unreachable!("has_unclosed_worker_question_since is not what the test reaches")
    }
    fn last_worker_question_closed(&self, run_id: &RunId) -> Result<Option<i64>> {
        unreachable!("last_worker_question_closed is not what the test reaches")
    }
    fn close_stuck_exit_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        unreachable!("close_stuck_exit_asks is not what the test reaches")
    }
    fn close_answer_prompt_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        unreachable!("close_answer_prompt_asks is not what the test reaches")
    }
    fn unclosed_stalled_ask(&self, run_id: &RunId) -> Result<Option<Ask>> {
        unreachable!("unclosed_stalled_ask is not what the test reaches")
    }
    fn close_stalled_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        unreachable!("close_stalled_asks is not what the test reaches")
    }
    fn end_stalled_detections(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        unreachable!("end_stalled_detections is not what the test reaches")
    }
    fn note_on_asks(&mut self, run_id: &RunId, note: &str, why: &str) -> Result<Vec<Ask>> {
        unreachable!("note_on_asks is not what the test reaches")
    }
    fn close_approve_landing_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        unreachable!("close_approve_landing_asks is not what the test reaches")
    }
    fn close_run_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        unreachable!("close_run_asks is not what the test reaches")
    }
    fn close_ended_landing_asks(&mut self, task: Option<TaskId>) -> Result<Vec<Ask>> {
        unreachable!("close_ended_landing_asks is not what the test reaches")
    }
    fn close_blocked_asks(
        &mut self,
        run_id: &RunId,
        task_id: TaskId,
        answer: &str,
    ) -> Result<Vec<Ask>> {
        unreachable!("close_blocked_asks is not what the test reaches")
    }
}
