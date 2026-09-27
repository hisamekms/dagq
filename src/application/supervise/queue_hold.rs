//! The wait and the answer of the authentication and usage-limit
//! `queue_hold` asks (ADR-0047 decision 42, task 437). Every pass reads the
//! queue's open ones: while one is open no new run is claimed (the
//! `claim_held` of [`HoldReason::Authentication`] or
//! [`HoldReason::UsageLimit`]) and no headless job starts (reviews wait in
//! [`Phase::ReviewHeld`]; the recovery jobs, the plan and goal reviews and
//! the observer are not started); the runs in flight keep their leases.
//! An answer the ask offered is applied here: `done` types
//! [`queue_hold::CONTINUE_TEXT`] into each held session this supervisor
//! watches (checked like every typed text, ADR-0047 decision 31) and
//! starts again the jobs that failed around the hold; `cancel_affected`
//! gives the held runs this supervisor watches up as an abandon does.
//! Either closes the ask and records `queue_hold_applied`. The disk's
//! `cost` ask (task 377) is applied by [`super::disk`].
//!
//! [`HoldReason::Authentication`]: crate::domain::claim_hold::HoldReason::Authentication
//! [`HoldReason::UsageLimit`]: crate::domain::claim_hold::HoldReason::UsageLimit

use super::*;
use crate::domain::{
    Ask, GoalId, PlannerOrigin, ProposalId, RunEvent,
    proposal::{PlannerOwner, Submission as Resubmission},
    queue_hold::{
        self, CANCEL_AFFECTED, DONE, HOLD_CONTINUE_SENT, JOB_RESTARTED, QUEUE_HOLD_APPLIED,
    },
    stats::timestamp_millis,
};

/// A job that failed this long before its hold ask opened ran into the
/// same wall: the ask opens once a session or a job shows it, and a job's
/// failure is recorded before that.
const FAILED_BEFORE_ASK_SECS: i64 = 600;
/// How many of the latest failures of each job kind are looked at.
const FAILURES_READ: usize = 50;

impl Supervisor<'_> {
    /// Apply the answered authentication and usage-limit asks, then read
    /// the one that holds the queue's work now (see the module).
    pub(super) fn check_queue_hold(&mut self) -> Result<()> {
        let unclosed: Vec<Ask> = self
            .queue
            .asks(AskQuery::default())?
            .into_iter()
            .filter(|ask| queue_hold::reason_of(ask).is_some())
            .collect();
        for ask in unclosed.iter().filter(|ask| ask.answered_at.is_some()) {
            let answer = ask.answer.as_deref().unwrap_or_default().trim();
            // Another answer is a person's to read and act on.
            if !queue_hold::applies(&ask.options, answer) {
                continue;
            }
            match answer {
                DONE => self.hold_done(ask)?,
                CANCEL_AFFECTED => self.hold_canceled(ask)?,
                _ => {}
            }
        }
        let hold = unclosed.iter().find_map(queue_hold::hold_of);
        match (self.queue_hold, hold) {
            (None, Some(hold)) => {
                warn!(
                    ask_id = hold.ask_id,
                    "ask {} ({}) holds the queue: no new run is claimed and no headless job starts until a person answers it",
                    hold.ask_id,
                    hold.reason.as_str()
                )
            }
            (Some(was), None) => {
                info!(
                    ask_id = was.ask_id,
                    "ask {} no longer holds the queue: claims and headless jobs resume", was.ask_id
                )
            }
            _ => {}
        }
        self.queue_hold = hold;
        Ok(())
    }
    /// `done`: the held sessions this supervisor watches (a worker's, a
    /// revise's or a resume's) get the text to go on (from their watch,
    /// [`SessionWatch::continue_after_hold`]),
    /// the failed jobs start again, and the ask closes. A held run another
    /// supervisor watches is that one's: its stall nudge tells it to go on.
    fn hold_done(&mut self, ask: &Ask) -> Result<()> {
        let mut continued = Vec::new();
        let mut moved_on = Vec::new();
        let mut elsewhere = Vec::new();
        for run in &ask.affected {
            match self.slots.iter().find(|slot| slot.run.id().as_str() == run) {
                Some(slot) if slot.phase.holds_live_session() => {
                    self.hold_continue.insert(slot.run.id().clone(), ask.id);
                    continued.push(run.clone());
                }
                // Past its session (a receipt, a review, an exit): nothing
                // to tell it.
                Some(_) => moved_on.push(run.clone()),
                None => elsewhere.push(run.clone()),
            }
        }
        let restarted = self.restart_failed_jobs(ask);
        self.queue.close_ask(ask.id)?;
        self.queue.record_queue_event(
            QUEUE_HOLD_APPLIED,
            json!({
                "ask_id": ask.id,
                "answer": DONE,
                "reason_category": ask.reason_category,
                "subject": ask.subject,
                "continued": continued,
                "released": [],
                "moved_on": moved_on,
                "restarted": restarted,
                "elsewhere": elsewhere,
                "supervisor": self.token,
            }),
        )?;
        info!(ask_id = %ask.id, "applied `done` to ask {}: {} held session(s) are told to go on, {} failed job(s) start again", ask.id, continued.len(), restarted.len());
        Ok(())
    }
    /// `cancel_affected`: each held run this supervisor watches is given
    /// up as an abandon does (its lease released, its session and worktree
    /// kept; `recover run` for the inbox), and the ask closes. Throwing the
    /// work away is the answer's, a person's decision.
    fn hold_canceled(&mut self, ask: &Ask) -> Result<()> {
        let mut released = Vec::new();
        let mut moved_on = Vec::new();
        let mut elsewhere = Vec::new();
        for run in &ask.affected {
            let Some(index) = self
                .slots
                .iter()
                .position(|slot| slot.run.id().as_str() == run)
            else {
                elsewhere.push(run.clone());
                continue;
            };
            // A run that went on by itself past its session (validating,
            // reviewing, landing) is no longer held: it is left alone.
            let phase = &self.slots[index].phase;
            if !(phase.holds_live_session() || matches!(phase, Phase::ReviewHeld(_))) {
                moved_on.push(run.clone());
                continue;
            }
            let mut slot = self.slots.remove(index);
            stop_job(&mut slot);
            self.hold_continue.remove(slot.run.id());
            self.loads.remove(slot.run.id());
            let message = format!(
                "a person answered `{CANCEL_AFFECTED}` to the {} ask {} that held the run: the supervisor gave it up",
                ask.reason_category.as_str(),
                ask.id
            );
            warn!(run_id = %slot.run.id(), "run {}: {message}", slot.run.id());
            self.abandon(&slot.run, message, &ReasonCode::HoldCanceled.into());
            released.push(run.clone());
        }
        self.queue.close_ask(ask.id)?;
        self.queue.record_queue_event(
            QUEUE_HOLD_APPLIED,
            json!({
                "ask_id": ask.id,
                "answer": CANCEL_AFFECTED,
                "reason_category": ask.reason_category,
                "subject": ask.subject,
                "continued": [],
                "released": released,
                "moved_on": moved_on,
                "restarted": [],
                "elsewhere": elsewhere,
                "supervisor": self.token,
            }),
        )?;
        info!(ask_id = %ask.id, "applied `{CANCEL_AFFECTED}` to ask {}: gave up {} held run(s)", ask.id, released.len());
        Ok(())
    }
    /// Start again the headless jobs that failed from
    /// [`FAILED_BEFORE_ASK_SECS`] before the ask opened, while nothing
    /// else moved them on: a recovery job of an ended run (`triage_failed`:
    /// `job_restarted` makes its triage due), a plan review (the proposal
    /// goes to plan review again as it is) and a goal review (rearmed). A
    /// failed review of a run already opened its `approve_landing` ask, and
    /// a live session's recovery job its alert's ask (ADR-t609-1): those
    /// stay a person's. What could not be started again is logged.
    fn restart_failed_jobs(&mut self, ask: &Ask) -> Vec<Value> {
        let since_ms = (ask.created_at - FAILED_BEFORE_ASK_SECS) * 1000;
        let failures = |sv: &Self, kind: &str| -> Vec<RunEvent> {
            match sv.queue.latest_events_of(kind, FAILURES_READ) {
                Ok(events) => events
                    .into_iter()
                    .filter(|e| timestamp_millis(&e.created_at).is_some_and(|at| at >= since_ms))
                    .collect(),
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "the failed {kind} jobs could not be read: {error:#}");
                    Vec::new()
                }
            }
        };
        let mut restarted = Vec::new();
        for event in failures(self, event_kind::TRIAGE_FAILED) {
            let Some(run_id) = event.run_id.clone() else {
                continue;
            };
            match self.restart_triage(ask, &event, &run_id) {
                Ok(true) => restarted.push(json!({"job": "triage", "run_id": run_id})),
                Ok(false) => {}
                Err(error) => {
                    warn!(run_id = %run_id, error = %format_args!("{error:#}"), "the triage of {run_id} could not be started again: {error:#}")
                }
            }
        }
        let held: Vec<ProposalId> = match self.queue.plan_review_holds() {
            Ok(holds) => holds
                .into_iter()
                .filter(|hold| hold.kind == event_kind::PLAN_REVIEW_FAILED)
                .map(|hold| hold.proposal_id)
                .collect(),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the failed plan reviews could not be read: {error:#}");
                Vec::new()
            }
        };
        let mut proposals: Vec<ProposalId> = failures(self, event_kind::PLAN_REVIEW_FAILED)
            .iter()
            .filter_map(|e| e.payload["proposal_id"].as_i64().map(ProposalId::new))
            .filter(|id| held.contains(id))
            .collect();
        proposals.sort_by_key(|id| id.as_i64());
        proposals.dedup();
        for proposal in proposals {
            let again = self.queue.submit(Resubmission {
                tasks: Vec::new(),
                goals: Vec::new(),
                proposal: Some(proposal),
                owner: PlannerOwner {
                    origin: PlannerOrigin::Runtime,
                    workspace_id: None,
                },
            });
            match again {
                Ok(_) => restarted.push(json!({"job": "plan_review", "proposal_id": proposal})),
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "the plan review of proposal {proposal} could not be started again: {error:#}")
                }
            }
        }
        let held: Vec<GoalId> = match self.queue.goal_review_holds() {
            Ok(holds) => holds.into_iter().map(|hold| hold.goal_id).collect(),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the failed goal reviews could not be read: {error:#}");
                Vec::new()
            }
        };
        let mut goals: Vec<GoalId> = failures(self, event_kind::GOAL_REVIEW_FAILED)
            .iter()
            .filter_map(|e| e.goal_id)
            .filter(|id| held.contains(id))
            .collect();
        goals.sort_by_key(|id| id.as_i64());
        goals.dedup();
        for goal in goals {
            match self.queue.rearm_goal_review(goal) {
                Ok(_) => restarted.push(json!({"job": "goal_review", "goal_id": goal})),
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "the goal review of goal {goal} could not be started again: {error:#}")
                }
            }
        }
        restarted
    }
    /// Make the triage of an ended run due again when its latest triage
    /// is the failure `event` and nobody leases the run.
    fn restart_triage(&mut self, ask: &Ask, event: &RunEvent, run_id: &RunId) -> Result<bool> {
        let run = self.queue.run(run_id)?;
        if !matches!(run.status(), RunStatus::Failed | RunStatus::Interrupted)
            || self.queue.run_lease(run_id)?.is_some()
        {
            return Ok(false);
        }
        let events = self.queue.run_events(run_id)?;
        let latest = events
            .iter()
            .rev()
            .find(|e| e.kind == event_kind::TRIAGE_FAILED)
            .map(|e| e.id);
        if triage_state(&events) != TriageState::Failed || latest != Some(event.id) {
            return Ok(false);
        }
        self.queue.record_runtime_event(
            run_id,
            JOB_RESTARTED,
            json!({"job": "triage", "ask_id": ask.id, "event_id": event.id}),
        )?;
        info!(run_id = %run_id, "the recovery job of {run_id} failed while ask {} held the queue: it is started again", ask.id);
        Ok(true)
    }
}

impl Phase {
    /// Whether the slot watches a live session a hold ask can stop: a
    /// worker's own, or that of a revise or a resume.
    pub(super) fn holds_live_session(&self) -> bool {
        matches!(
            self,
            Phase::Session(_) | Phase::Revise(_) | Phase::Resume(_)
        )
    }
}

impl SessionWatch {
    /// Type the fixed text to go on into a session a person's `done`
    /// released from a hold ask (task 437), once, while no receipt is in
    /// (a worker's session, or the live session of a revise or resume);
    /// when it was typed:
    /// the send is checked as every typed text is (ADR-0047 decision 31),
    /// and `hold_continue_sent` records it.
    pub(super) fn continue_after_hold(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
    ) -> Result<Option<SystemTime>> {
        let Some(ask_id) = sv.hold_continue.remove(run.id()) else {
            return Ok(None);
        };
        if self.receipt_seen {
            return Ok(None);
        }
        let sent_at = sv.files.now();
        let workspace = self.workspace.clone();
        let text = continue_text(run);
        match submit(sv, run, &workspace, Input::Text(&text), "continue") {
            Ok(submission) => {
                self.stall.input_sent(sent_at, Some(&text));
                self.answer_start = Some(StartCheck::new("continue", &text, sent_at, &submission));
                sv.queue.record_runtime_event(
                    run.id(),
                    HOLD_CONTINUE_SENT,
                    json!({
                        "ask_id": ask_id,
                        "workspace_id": workspace,
                        "submitted": !matches!(submission, Submission::Stuck(_)),
                    }),
                )?;
                info!(run_id = %run.id(), "run {} was held by ask {ask_id}: told it to go on in workspace {workspace}", run.id());
                Ok(Some(sent_at))
            }
            Err(error) => {
                // Its stall nudge (or its stage's timeout) follows.
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "the text to go on could not be typed into workspace {workspace} of {}: {error:#}", run.id());
                Ok(None)
            }
        }
    }
}
