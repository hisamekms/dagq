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
//! Every supervisor applies an answer to the held runs in its own slots,
//! once each (`hold_answer_applied`, task 754), and the ask closes, with
//! `queue_hold_applied`, once no run in it waits for a live supervisor to
//! apply it. The disk's
//! `cost` ask (task 377) is applied by [`super::disk`].
//!
//! [`HoldReason::Authentication`]: crate::domain::claim_hold::HoldReason::Authentication
//! [`HoldReason::UsageLimit`]: crate::domain::claim_hold::HoldReason::UsageLimit

use super::*;
use crate::domain::EventKind;
use crate::domain::{
    Ask, GoalId, PlannerOrigin, ProposalId, RunEvent,
    proposal::{PlannerOwner, Submission as Resubmission},
    queue_hold::{self, CANCEL_AFFECTED, DONE, HOLD_ANSWER_APPLIED},
    stats::timestamp_millis,
};

/// A job that failed this long before its hold ask opened ran into the
/// same wall: the ask opens once a session or a job shows it, and a job's
/// failure is recorded before that.
const FAILED_BEFORE_ASK_SECS: i64 = 600;
/// How many of the latest failures of each job kind are looked at.
const FAILURES_READ: usize = 50;
/// How long an answered ask waits for the other live supervisors to apply
/// the answer to the runs they watch before it closes without them.
const APPLY_WAIT_SECS: i64 = 300;

/// What became of a run the answer of a hold ask was applied to, or was
/// not (the `outcome` of `hold_answer_applied` and of `queue_hold_applied`'s
/// `runs`).
const CONTINUED: &str = "continued";
const RELEASED: &str = "released";
const MOVED_ON: &str = "moved_on";
/// Another live supervisor leased it and did not apply the answer within
/// [`APPLY_WAIT_SECS`].
const ELSEWHERE: &str = "elsewhere";
/// No live supervisor watched it: no lease, or a stale one.
const UNWATCHED: &str = "unwatched";

impl Supervisor<'_> {
    /// Apply the answered authentication and usage-limit asks (unless
    /// `apply` is false: the pass of a handoff, before this supervisor
    /// rebuilt the slots of its runs), then read the one that holds the
    /// queue's work now (see the module).
    pub(super) fn check_queue_hold(&mut self, apply: bool) -> Result<()> {
        let unclosed: Vec<Ask> = self
            .queue
            .asks(AskQuery::default())?
            .into_iter()
            .filter(|ask| queue_hold::reason_of(ask).is_some())
            .collect();
        for ask in unclosed.iter().filter(|ask| ask.answered_at.is_some()) {
            let answer = ask.answer.as_deref().unwrap_or_default().trim();
            // Another answer is a person's to read and act on.
            if !apply || !queue_hold::applies(&ask.options, answer) {
                continue;
            }
            if matches!(answer, DONE | CANCEL_AFFECTED) {
                self.apply_hold_answer(ask, answer)?;
            }
        }
        // The asks remain open and keep their real cause. An explicit ban
        // does not wait for Claude's login/limit before handing work to a person.
        let hold = unclosed
            .iter()
            .find_map(queue_hold::hold_of)
            .filter(|_| !self.no_claude);
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
    /// Apply `answer` to the held runs of `ask` this supervisor watches,
    /// each once (`hold_answer_applied`: another pass, or another
    /// supervisor, sees it applied), then close the ask once every run in
    /// it was applied or no live supervisor watches it (task 754).
    fn apply_hold_answer(&mut self, ask: &Ask, answer: &str) -> Result<()> {
        for run in queue_hold::affected_runs(ask) {
            let Some(index) = self
                .slots
                .iter()
                .position(|slot| slot.run.id().as_str() == run)
            else {
                continue;
            };
            let id = self.slots[index].run.id().clone();
            if applied_to(&self.queue.run_events(&id)?, ask.id).is_some() {
                continue;
            }
            // Recorded before the answer acts: a run given up has no lease,
            // and another supervisor closing the ask must not read it as
            // one nobody watched.
            let outcome = hold_outcome(&self.slots[index].phase, answer);
            self.queue.record_runtime_event(
                &id,
                EventKind::HoldAnswerApplied,
                json!({
                    "ask_id": ask.id,
                    "answer": answer,
                    "outcome": outcome,
                    "supervisor": self.token,
                }),
            )?;
            match outcome {
                CONTINUED => {
                    self.hold_continue.insert(id, ask.id);
                }
                RELEASED => self.hold_canceled(ask, index),
                _ => {}
            }
        }
        self.close_hold(ask, answer)
    }
    /// `cancel_affected` to the held run in slot `index`: given up as an
    /// abandon does (its lease released, its session and worktree kept;
    /// `recover run` for the inbox). Throwing the work away is the
    /// answer's, a person's decision.
    fn hold_canceled(&mut self, ask: &Ask, index: usize) {
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
    }
    /// Close the answered `ask` once no run in it waits for a live
    /// supervisor to apply the answer: each run is applied
    /// (`hold_answer_applied`), has no lease (`unwatched`: nothing runs it
    /// to apply the answer to), or [`APPLY_WAIT_SECS`] passed since the
    /// answer (a run another supervisor still leases is then `elsewhere`,
    /// one whose lease is stale `unwatched`). A run whose lease went stale
    /// is waited for until then, as a supervisor may adopt it and apply
    /// the answer. The supervisor that closes the ask starts the failed
    /// jobs again (`done`) and records `queue_hold_applied`; one that
    /// finds it closed by another does neither.
    fn close_hold(&mut self, ask: &Ask, answer: &str) -> Result<()> {
        let now = self.generators.clock.now();
        let due = ask
            .answered_at
            .is_some_and(|at| now - at >= APPLY_WAIT_SECS);
        let mut runs = Vec::new();
        for run in queue_hold::affected_runs(ask) {
            // A run the queue does not know has nothing to apply.
            let Ok(id) = RunId::new(run.clone()) else {
                runs.push(json!({"run_id": run, "outcome": UNWATCHED, "supervisor": null}));
                continue;
            };
            // What cannot be read now is read again on the next pass.
            let Ok(events) = self.queue.run_events(&id) else {
                return Ok(());
            };
            let applied = applied_to(&events, ask.id).cloned();
            if let Some(payload) = applied {
                runs.push(json!({
                    "run_id": run,
                    "outcome": payload["outcome"],
                    "supervisor": payload["supervisor"],
                }));
                continue;
            }
            let Ok(lease) = self.queue.run_lease(&id) else {
                return Ok(());
            };
            let (outcome, supervisor) = match lease {
                None => (UNWATCHED, None),
                // Its supervisor applies it on its next pass.
                Some(_) if !due => return Ok(()),
                // Leased by this supervisor but in none of its slots, or
                // by a gone one: nobody watches it to apply the answer.
                Some(lease) if lease.token == self.token || self.lease_stale(&lease, now) => {
                    (UNWATCHED, Some(lease.token))
                }
                Some(lease) => (ELSEWHERE, Some(lease.token)),
            };
            runs.push(json!({"run_id": run, "outcome": outcome, "supervisor": supervisor}));
        }
        if let Err(error) = self.queue.close_ask(ask.id) {
            // Another supervisor closed it first and recorded the rest.
            if self.queue.read_ask(ask.id)?.closed_at.is_some() {
                return Ok(());
            }
            return Err(error);
        }
        let restarted = if answer == DONE {
            // A person fixed what the providers stopped at: Codex is tried
            // again too (ADR-t813-2 decision 6).
            self.release_provider_holds(DONE)?;
            self.restart_failed_jobs(ask)
        } else {
            Vec::new()
        };
        let of = |outcome: &str| -> Vec<Value> {
            runs.iter()
                .filter(|r| r["outcome"] == outcome)
                .map(|r| r["run_id"].clone())
                .collect()
        };
        self.queue.record_queue_event(
            EventKind::QueueHoldApplied,
            json!({
                "ask_id": ask.id,
                "answer": answer,
                "reason_category": ask.reason_category,
                "subject": ask.subject,
                "continued": of(CONTINUED),
                "released": of(RELEASED),
                "moved_on": of(MOVED_ON),
                "restarted": restarted,
                "elsewhere": of(ELSEWHERE),
                "unwatched": of(UNWATCHED),
                "runs": runs,
                "jobs": jobs_of(ask),
                "supervisor": self.token,
            }),
        )?;
        info!(ask_id = %ask.id, "applied `{answer}` to ask {}: {} run(s) told to go on, {} given up, {} failed job(s) start again", ask.id, of(CONTINUED).len(), of(RELEASED).len(), restarted.len());
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
            EventKind::JobRestarted,
            json!({"job": "triage", "ask_id": ask.id, "event_id": event.id}),
        )?;
        info!(run_id = %run_id, "the recovery job of {run_id} failed while ask {} held the queue: it is started again", ask.id);
        Ok(true)
    }
}

/// What `answer` does to a held run in `phase`. `done`: a session a hold
/// can stop (a worker's, a revise's or a resume's) gets the text to go on
/// from its watch ([`SessionWatch::continue_after_hold`]). `cancel_affected`:
/// such a session, or a review waiting for the hold, is given up. A run
/// that went on by itself past its session (a receipt, validating, a
/// review, an exit, landing) is no longer held: it is left alone.
fn hold_outcome(phase: &Phase, answer: &str) -> &'static str {
    match answer {
        DONE if phase.holds_live_session() => CONTINUED,
        CANCEL_AFFECTED if phase.holds_live_session() || matches!(phase, Phase::ReviewHeld(_)) => {
            RELEASED
        }
        _ => MOVED_ON,
    }
}

/// The `hold_answer_applied` of the ask `ask_id` among a run's `events`.
fn applied_to(events: &[RunEvent], ask_id: AskId) -> Option<&Value> {
    events
        .iter()
        .find(|e| e.kind == HOLD_ANSWER_APPLIED && e.payload["ask_id"] == json!(ask_id))
        .map(|e| &e.payload)
}

/// The headless jobs `ask` lists next to its runs (task 438): a review
/// held with its session starts again once the hold ends; a failed
/// recovery, plan review or goal review is started again by `done`
/// ([`Supervisor::restart_failed_jobs`]).
fn jobs_of(ask: &Ask) -> Vec<&String> {
    ask.affected
        .iter()
        .filter(|entry| !queue_hold::is_run_entry(entry))
        .collect()
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
                    EventKind::HoldContinueSent,
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
