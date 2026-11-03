//! The state machine of a slot: [`Supervisor::step`] takes one look at a
//! slot's run and moves it on by its `Phase` (one function across the
//! phases, so the order of its checks and calls is read in one place),
//! [`Supervisor::step_awaiting_slot`] the look at a run waiting for the
//! landing slot (it lands, runs its e2e first, is parked or waits) and
//! [`Supervisor::step_landing_wait`] that look under the lock of a
//! cleanup for disk space. `Phase`, the free functions these share with
//! the other submodules and `tick`, the pass that calls `step`, stay in
//! `execution.rs`; this module only adds methods to `Supervisor`, whose
//! fields it sees as a submodule of `execution` without widening them
//! (rule C3 of docs/design/architecture.md).

use super::*;

impl Supervisor<'_> {
    /// A look at a run in its slot waiting for the landing slot, its park
    /// included ([`Self::park_step`]): not while a cleanup for disk space
    /// has it reserved, removing its build outputs; it lands once the job
    /// passed it, building them again. The lock is held until the look is
    /// done, so the job reserves no run this look moves on (a landing, an
    /// e2e, a park or the lease handed back), and a run moved on is no
    /// longer the job's to take ([`cleanup::Cleaning::withdraw`]).
    fn step_landing_wait(&mut self, slot: &mut Slot) -> Result<Step> {
        let cleaning = self.host.cleanup().cleaning();
        let mut guard = cleanup::lock_cleaning(&cleaning);
        if guard.reserved(slot.run.id()) {
            return Ok(Step::Continue);
        }
        let step = match self.park_step(slot) {
            Ok(Some(step)) => Ok(step),
            Ok(None) => self.step_awaiting_slot(slot),
            Err(error) => Err(error),
        };
        if !matches!(
            (&step, &slot.phase),
            (Ok(Step::Continue), Phase::AwaitingSlot)
        ) {
            guard.withdraw(slot.run.id());
        }
        step
    }

    /// A look at a run in its slot waiting for the landing slot: it lands,
    /// runs its e2e first, is parked, or waits.
    fn step_awaiting_slot(&mut self, slot: &mut Slot) -> Result<Step> {
        // Judged again on each look: only the landing of another run
        // keeps it in the landing queue (ADR-t1591-1).
        slot.landing_turn = false;
        slot.blocked_by = None;
        // Its verification would fail on the missing program, an
        // unreadable CI, an unresolved landing branch or short disk:
        // the run stays awaiting integration, leased, and the
        // integration slot stays free. A supervisor that stops
        // gives the lease back for a hold that does not end by
        // itself, leaving the run awaiting integration for a
        // supervisor that does not drain to take up again
        // (ADR-t3049-1); one that hands off keeps it, and hands the
        // run over leased (ADR-t3048-1,
        // [`crate::domain::landing_hold::judge`]).
        match self.landing_hold() {
            crate::domain::landing_hold::LandingHold::Proceed => {}
            crate::domain::landing_hold::LandingHold::Wait => return Ok(Step::Continue),
            crate::domain::landing_hold::LandingHold::HandBack(why) => {
                warn!(run_id = %slot.run.id(), "run {} is left awaiting integration: {why} and this supervisor stops", slot.run.id());
                self.queue
                    .release_lease(slot.run.id(), self.registration.token())?;
                return Ok(Step::Done(Box::new(self.queue.run(slot.run.id())?)));
            }
        }
        // An e2e that could not run waits to be tried again; a
        // supervisor that stops cannot wait for it, and leaves the
        // run awaiting integration as above (one that hands off
        // leaves it to the next process, `AwaitingE2e`).
        if self.registration.draining()
            && !self.registration.handing_off()
            && self.e2e_retry_pending(&slot.run)
        {
            warn!(run_id = %slot.run.id(), "run {} is left awaiting integration: its e2e could not run and this supervisor stops", slot.run.id());
            self.queue
                .release_lease(slot.run.id(), self.registration.token())?;
            return Ok(Step::Done(Box::new(self.queue.run(slot.run.id())?)));
        }
        // The e2e a passed run needs runs before it lands
        // (ADR-t1233-2), whatever the integration slot; a head the
        // landing recheck found not landing is parked first, as
        // below, rather than tested.
        if self.e2e_due(&slot.run)?.is_some() {
            let current = self.queue.run(slot.run.id())?;
            let main = self.repository.main_head()?;
            if let Some(parked) = self.park_held_by_recheck(&current, &main)? {
                return Ok(Step::Done(Box::new(parked)));
            }
        }
        if let Some(phase) = self.e2e_before_landing(&slot.run)? {
            slot.transition(phase, "e2e_due", &*self.queue);
            return Ok(Step::Continue);
        }
        if let Some(landing) = self.queue.runs_with_status(RunStatus::Integrating)?.first() {
            // Reviewed and its e2e done, it waits only for its
            // landing turn (ADR-t1591-1).
            slot.landing_turn = true;
            slot.blocked_by = Some(landing.id().clone());
            return Ok(Step::Continue);
        }
        let current = self.queue.run(slot.run.id())?;
        let previous = current.status();
        let main = self.repository.main_head()?;
        // A head the landing recheck found not landing on this main
        // is parked for a resume instead (ADR-0068 decision 3).
        if let Some(parked) = self.park_held_by_recheck(&current, &main)? {
            return Ok(Step::Done(Box::new(parked)));
        }
        // A run whose task is being replanned or held does not
        // land (ADR-t1521-2 decision 1, ADR-t1879-1): the park
        // above takes it out of its slot; a fence that came since
        // that look is the next look's.
        if self.queue.turn_fence(current.task_id())?.is_some() {
            return Ok(Step::Continue);
        }
        // Its verification takes turns with the automatic update's
        // build and e2e (ADR-t2719-1): only looked at here, the
        // landing takes the turn itself before its commands.
        if self.landing_waits_its_turn(&current)? {
            slot.landing_turn = true;
            return Ok(Step::Continue);
        }
        let run = match self.begin_landing(slot.run.id(), &main) {
            Ok(run) => run,
            // Unanswered, it may have taken the slot for this run:
            // the reconciliation reads what the queue holds.
            Err(error) if crate::application::supervisor_service::unreachable(&error) => {
                return Err(error);
            }
            // A handoff asked since this pass read none at its
            // start: the same write refused the landing
            // (ADR-t3077-1 decision 1), and the run waits, its
            // lease kept, for the next pass to drain for the
            // request.
            Err(error)
                if error
                    .downcast_ref::<crate::application::HandoffAsked>()
                    .is_some() =>
            {
                info!(run_id = %slot.run.id(), "run {} does not land: its supervisor was asked to hand off", slot.run.id());
                return Ok(Step::Continue);
            }
            // An `integrate` took the slot since the check: try again later.
            Err(_)
                if let Some(landing) =
                    self.queue.runs_with_status(RunStatus::Integrating)?.first() =>
            {
                slot.landing_turn = true;
                slot.blocked_by = Some(landing.id().clone());
                return Ok(Step::Continue);
            }
            Err(error) => return Err(error),
        };
        info!(run_id = %run.id(), "run {} lands onto main {main} ({})", run.id(), previous.as_str());
        let landing = Phase::Landing(Some(self.spawn_landing(run.clone(), previous, main)?));
        slot.run = run;
        slot.transition(
            landing,
            EventKind::IntegrationStarted.as_str(),
            &*self.queue,
        );
        Ok(Step::Continue)
    }

    pub(super) fn step(&mut self, slot: &mut Slot) -> Result<Step> {
        // A landing releases the lease itself when it ends, so it is joined
        // before the lease is checked.
        if let Phase::Landing(handle) = &mut slot.phase {
            // Joined already when the read of what it left the run as was
            // not answered: only that read is made again.
            if let Some(running) = handle {
                if !running.is_finished() {
                    return Ok(Step::Continue);
                }
                let landed = handle
                    .take()
                    .context("landing already joined")?
                    .join()
                    .map_err(|_| anyhow!("landing thread panicked"));
                match landed.and_then(|result| result) {
                    // Handed over before its verification was done again
                    // (ADR-t3344-1): it waits for the landing slot again
                    // under its lease, which a handoff hands over.
                    Ok(IntegrationOutcome::HandedOver { run, .. }) => {
                        info!(run_id = %run.id(), "run {} waits for the landing slot again: its landing was handed over", run.id());
                        slot.run = *run;
                        slot.transition(
                            Phase::AwaitingSlot,
                            EventKind::IntegrationHandedOver.as_str(),
                            &*self.queue,
                        );
                        return Ok(Step::Continue);
                    }
                    Ok(outcome) => {
                        let outcome = serde_json::to_value(&outcome)?;
                        info!(run_id = %slot.run.id(), "run {} landing: {}", slot.run.id(), outcome["outcome"]);
                    }
                    Err(error) => {
                        let message = format!("landing failed: {error:#}");
                        warn!(run_id = %slot.run.id(), "run {}: {message}", slot.run.id());
                        self.errors.push(RunError {
                            run_id: slot.run.id().clone(),
                            task_id: slot.run.task_id(),
                            message,
                        });
                    }
                }
            }
            return Ok(Step::Done(Box::new(self.queue.run(slot.run.id())?)));
        }
        if !self
            .queue
            .holds_lease(slot.run.id(), self.registration.token())?
        {
            // Another holds it now: a cleanup for room no longer takes it.
            cleanup::lock_cleaning(&self.host.cleanup().cleaning()).withdraw(slot.run.id());
            return Ok(Step::Disowned);
        }
        // Its park too waits for a cleanup for room that has it reserved.
        if matches!(slot.phase, Phase::AwaitingSlot) {
            return self.step_landing_wait(slot);
        }
        // A run whose task is being replanned or held goes no further than
        // its park (ADR-t1521-2 decision 1, ADR-t1879-1 decision 7): no
        // next turn, review, revise or landing starts, and the park gives
        // its slot back.
        if let Some(step) = self.park_step(slot)? {
            return Ok(step);
        }
        match &mut slot.phase {
            Phase::Resume(watch) => {
                let awaited = watch.awaits_answer;
                let polled = watch.poll(self, &slot.run)?;
                // As a revise's: its own questions stop it in its slot.
                slot.note_questions(awaited, &*self.queue);
                let Some(verdict) = polled else {
                    return Ok(Step::Continue);
                };
                let Phase::Resume(watch) = &slot.phase else {
                    unreachable!("the resume's slot is still in it");
                };
                let attempt = watch.attempt;
                let workspace = watch.workspace.clone();
                let rewritten = watch.receipt_rewritten(&*self.files);
                self.finish_resumed_session(slot, attempt, &workspace, verdict, rewritten)
            }
            Phase::AwaitingSlot => {
                unreachable!("a run waiting for the landing slot is looked at before its park")
            }
            Phase::AgentEval(watch) => {
                let run = self.queue.run(slot.run.id())?;
                if let Some(next) = self.look_at_eval(&run, watch)? {
                    slot.run = run;
                    slot.transition(next, EventKind::RunAgentEvalFinished.as_str(), &*self.queue);
                }
                Ok(Step::Continue)
            }
            // Decided again as a run awaiting the slot.
            Phase::AwaitingE2e => {
                slot.transition(Phase::AwaitingSlot, "e2e_waited", &*self.queue);
                Ok(Step::Continue)
            }
            Phase::E2e(watch) => {
                if !watch.finished() {
                    return Ok(Step::Continue);
                }
                // Read before the e2e is taken out of the slot.
                let run = self.queue.run(slot.run.id())?;
                let events = self.queue.run_events(slot.run.id())?;
                let history = self
                    .queue
                    .e2e_gate_events(crate::application::e2e_verdict::HISTORY)?;
                let Phase::E2e(watch) = std::mem::replace(&mut slot.phase, Phase::AwaitingSlot)
                else {
                    unreachable!("matched above")
                };
                self.finish_e2e(slot, watch, run, &events, &history)
            }
            Phase::Landing(_) => unreachable!("joined above"),
            Phase::Recovery(watch) => {
                // The job's reply, session and failure are read by the
                // provider it ran on (ADR-t1063-1 decisions 2, 4 and 6).
                let agent = self.job_agent(watch.job.provider).unwrap_or(self.reviewer);
                let Some(outcome) = watch.poll(&*self.files, agent)? else {
                    return Ok(Step::Continue);
                };
                let (round, alert, attempt) = (watch.round, watch.alert, watch.attempt);
                let duration_secs = watch.job.elapsed().as_secs();
                let run = self.queue.run(slot.run.id())?;
                // A job that failed: stopped at a wall only a person moves
                // (Claude's), it joins the hold ask, whose `done` starts it
                // again, and its `triage_failed` is no attention meanwhile
                // (task 438). When `[roles.recovery]` names its provider
                // and that provider could not be used, the provider is held
                // and the round's `triage_failed` carries
                // `provider_unusable`: no attention, and the next round
                // starts on the other provider (ADR-t1063-1 decision 4), or,
                // with `[provider_fallback] jobs` off, on the same one once
                // its hold ends (ADR-t1857-1).
                // Only under `--no-claude`, with no provider left, does the
                // next round go to a person told why, never to Claude.
                let end = self.recovery_job_end(
                    run.id(),
                    &watch.job,
                    agent,
                    watch.switchable,
                    outcome.as_ref().err().map(String::as_str),
                );
                let acted = match outcome {
                    Ok(verdict) => {
                        let job = ActorContext::recovery_job(run.id(), alert.as_str(), attempt);
                        self.for_job(&job, |sv| {
                            sv.act_on_recovery(
                                &run,
                                round,
                                alert,
                                attempt,
                                duration_secs,
                                verdict,
                                &end,
                            )
                        })
                    }
                    Err(error) => Err(anyhow!("{error}")),
                };
                if let Err(error) = acted {
                    // Unanswered, the round may or may not be recorded: the
                    // reconciliation judges it, no failure is recorded on it.
                    if crate::application::supervisor_service::unreachable(&error) {
                        return Err(error);
                    }
                    // Another process took the run's lease meanwhile: its
                    // round is the record.
                    if !self
                        .queue
                        .holds_lease(run.id(), self.registration.token())?
                    {
                        return Ok(Step::Disowned);
                    }
                    self.fail_recovery(
                        &run,
                        round,
                        alert,
                        attempt,
                        format!("{error:#}"),
                        duration_secs,
                        &end,
                    );
                }
                Ok(Step::Triaged(Box::new(self.queue.run(run.id())?)))
            }
            Phase::Session(watch) => {
                let awaited = watch.awaits_answer;
                let polled = watch.poll(self, &slot.run)?;
                // As a revise's: its own questions stop it in its slot.
                slot.note_questions(awaited, &*self.queue);
                let Some(run) = polled else {
                    return Ok(Step::Continue);
                };
                self.supervision_finished(slot, run)
            }
            Phase::Validating(handle, session) => {
                // A validation a park joined and set aside, whose park
                // did not happen after all, is made again.
                if handle.is_none() {
                    *handle = Some(self.validate(slot.run.clone()));
                    return Ok(Step::Continue);
                }
                if !handle.as_ref().is_some_and(|h| h.is_finished()) {
                    return Ok(Step::Continue);
                }
                self.pause_at(PausePoint::ValidationEnd, slot.run.id());
                let mut validation = handle
                    .take()
                    .context("validation already joined")?
                    .join()
                    .map_err(|_| anyhow!("validation thread panicked"))??;
                validation.load = self.take_load(slot.run.id());
                let run = self.queue.finish_validation(
                    slot.run.id(),
                    self.registration.token(),
                    &validation,
                )?;
                // Kept in the phase until it is replaced, so an error before
                // that still finds the session to ask to exit (task 237).
                let session = session.clone();
                let events = self.queue.run_events(run.id())?;
                let history = RunHistory::from_events(&events);
                // A landing recheck resumed the run without waiting for the
                // answer to its approve_landing ask (ADR-0068 decision 4): it
                // is not reviewed again, and waits for the answer unleased once
                // its session was asked to exit, seen to end and stopped if it
                // did not (ADR-t1433-3 decision 3).
                let awaits_landing_answer = run.status() == RunStatus::AwaitingIntegration
                    && !history.approved()
                    && crate::domain::resume::parked_by_recheck(history.events())
                    && self
                        .queue
                        .has_unclosed_ask(run.id(), AskKind::ApproveLanding)?;
                let next =
                    match after_validation(run.status(), history.approved(), awaits_landing_answer)
                    {
                        AfterValidation::Land => {
                            Phase::Exiting(ExitWatch::new(session, AfterExit::Land))
                        }
                        AfterValidation::Review => self.start_review(&run, session)?,
                        AfterValidation::Satisfied => {
                            Phase::Exiting(ExitWatch::new(session, AfterExit::Satisfied))
                        }
                        AfterValidation::Rest { close } => {
                            Phase::Exiting(ExitWatch::new(session, AfterExit::Rest { close }))
                        }
                    };
                slot.run = run;
                slot.transition(next, EventKind::ValidationFinished.as_str(), &*self.queue);
                Ok(Step::Continue)
            }
            Phase::ReviewHeld {
                session,
                retried,
                providers,
            } => {
                let events = self.queue.run_events(slot.run.id())?;
                let pair = crate::domain::provider_trial::RunPair::of_run(&events);
                if !self.review_held_waits(&pair, providers) {
                    let (session, retried) = (session.take(), *retried);
                    let run = self.queue.run(slot.run.id())?;
                    let review = self.resume_review(&run, session, retried)?;
                    slot.run = run;
                    slot.transition(review, "hold_ended", &*self.queue);
                }
                Ok(Step::Continue)
            }
            Phase::ReviewPrograms(watch) => {
                let Some(end) = watch.job.poll_program(&*self.files)? else {
                    return Ok(Step::Continue);
                };
                let ran = (watch.job.elapsed().as_secs(), watch.job.timeout.as_secs());
                // Read before the programs' results are taken out of the
                // slot; the job's end is read again as it was.
                let run = self.queue.run(slot.run.id())?;
                let programs = watch.programs.take();
                let next = self.review_program_ended(&run, programs, &end, ran)?;
                slot.run = run;
                slot.transition(
                    next,
                    EventKind::ReviewProgramFinished.as_str(),
                    &*self.queue,
                );
                Ok(Step::Continue)
            }
            Phase::Review(watch) => {
                // The other runs' reviews' jobs share the limit of jobs at
                // once with this one's.
                let elsewhere = self.review_jobs_running();
                let run = slot.run.clone();
                let Some(next) = self.tend_review(&run, watch, elsewhere)? else {
                    return Ok(Step::Continue);
                };
                slot.run = self.queue.run(run.id())?;
                slot.transition(next, EventKind::ReviewFinished.as_str(), &*self.queue);
                Ok(Step::Continue)
            }
            Phase::Revise(watch) => {
                let awaited = watch.awaits_answer;
                let polled = watch.poll(self, &slot.run)?;
                // Its own questions stop it, each from the pass that finds
                // it until the answer lets it go on: an outcome the poll
                // found after an answer moves it on from its revise.
                slot.note_questions(awaited, &*self.queue);
                let Some(outcome) = polled else {
                    return Ok(Step::Continue);
                };
                let Phase::Revise(watch) = &mut slot.phase else {
                    unreachable!("the revise's slot is still in it");
                };
                let session = watch.session.clone();
                let label = watch.fix.label(watch.attempt);
                match outcome {
                    ReviseOutcome::Rewritten(head) => {
                        let kind = match watch.fix {
                            Fix::Revise { .. } => EventKind::ReviseFinished,
                            Fix::Conflict(_) => EventKind::ConflictResolved,
                        };
                        self.queue.record_runtime_event(
                            slot.run.id(),
                            kind,
                            json!({"attempt": watch.attempt, "head": head}),
                        )?;
                        info!(run_id = %slot.run.id(), "run {} rewrote its receipt for {label} (head {head}); validating again", slot.run.id());
                        let run = self
                            .queue
                            .restart_validation(slot.run.id(), self.registration.token())?;
                        let handle = self.validate(run.clone());
                        slot.run = run;
                        slot.transition(
                            Phase::Validating(Some(handle), Some(session)),
                            kind.as_str(),
                            &*self.queue,
                        );
                    }
                    ReviseOutcome::Mismatch(code, why) => {
                        let message = revise_mismatch_request(&slot.run, &label, &why)?
                            .with_language(self.verifier.language().as_ref());
                        // Only what the session writes after this counts.
                        let sent_at = self.files.now();
                        let run = slot.run.clone();
                        match submit(
                            self,
                            &run,
                            &session.workspace,
                            Input::from(&message),
                            "receipt fix request",
                        ) {
                            Ok(_submission) => {
                                watch.requested(sent_at, sent_at);
                                let kind = match watch.fix {
                                    Fix::Revise { .. } => EventKind::ReviseReceiptRejected,
                                    Fix::Conflict(_) => EventKind::ConflictReceiptRejected,
                                };
                                self.queue.record_runtime_event(
                                    slot.run.id(),
                                    kind,
                                    json!({"code": code, "attempt": watch.attempt, "reason": why}),
                                )?;
                                info!(run_id = %slot.run.id(), "run {}: {why}; asked the session to fix it ({label})", slot.run.id());
                            }
                            // A hold's fence refused the request: the run
                            // waits for the park, and the resume after the
                            // release has its receipt judged again.
                            Err(error)
                                if error
                                    .chain()
                                    .any(|cause| cause.is::<headless::TurnFenced>()) =>
                            {
                                info!(run_id = %slot.run.id(), "run {}: {why}; {error:#}", slot.run.id());
                                slot.transition(
                                    Phase::ReviewHeld {
                                        session: Some(session),
                                        retried: false,
                                        providers: Vec::new(),
                                    },
                                    "fix_withheld",
                                    &*self.queue,
                                );
                            }
                            Err(error) => {
                                let why = format!(
                                    "{why}, and the request to fix it could not be sent: {error:#}"
                                );
                                warn!(run_id = %slot.run.id(), "run {}: {why}", slot.run.id());
                                let then = watch.fix.ask(why.clone(), why, None);
                                slot.transition(
                                    Phase::Exiting(ExitWatch::new(Some(session), then)),
                                    "fix_unsent",
                                    &*self.queue,
                                );
                            }
                        }
                    }
                    ReviseOutcome::Ended(why) => {
                        info!(run_id = %slot.run.id(), "run {}: the session {why} after {label}; asking a person", slot.run.id());
                        let then = watch.fix.ask(
                            format!("the session {why}"),
                            format!("the session {why} after {label}"),
                            None,
                        );
                        let exit = ExitWatch::new(Some(session), then);
                        slot.transition(Phase::Exiting(exit), "fix_ended", &*self.queue);
                    }
                }
                Ok(Step::Continue)
            }
            Phase::Exiting(watch) => {
                if !watch.poll(self, &slot.run)? {
                    return Ok(Step::Continue);
                }
                let session = watch.session.take();
                let then = std::mem::replace(&mut watch.then, AfterExit::Rest { close: false });
                let mut run = self.queue.run(slot.run.id())?;
                let close = !matches!(then, AfterExit::Rest { close: false });
                if close && let Some(session) = &session {
                    run = self.close_session(&run, session)?;
                }
                match then {
                    AfterExit::Land => {
                        self.queue_landing(&run, "exit");
                        slot.run = run;
                        slot.transition(
                            Phase::AwaitingSlot,
                            EventKind::LandingQueued.as_str(),
                            &*self.queue,
                        );
                        Ok(Step::Continue)
                    }
                    AfterExit::Ask {
                        decision,
                        reasons,
                        summary,
                        why,
                        recommendation,
                        confidence,
                        reason_category,
                        requested_by,
                        sent_back,
                    } => {
                        let open = |sv: &mut Self| {
                            sv.open_landing_ask(
                                &run,
                                decision,
                                &reasons,
                                &summary,
                                why.as_deref(),
                                (recommendation, confidence, reason_category),
                            )
                        };
                        // Only the ask's own events carry the job: the
                        // lease given back after it is the supervisor's.
                        let ask = match &requested_by {
                            Some(job) => self.for_job(job, open)?,
                            None => open(self)?,
                        };
                        if let Some(sent_back) = sent_back {
                            self.queue.record_runtime_event(
                                run.id(),
                                EventKind::ConcernSendBackEscalated,
                                json!({"attempt": sent_back.attempt, "why": sent_back.why, "ask_id": ask}),
                            )?;
                        }
                        info!(run_id = %run.id(), "run {} waits for a person in ask {ask}", run.id());
                        self.queue
                            .release_lease(run.id(), self.registration.token())?;
                        Ok(Step::Done(Box::new(self.queue.run(run.id())?)))
                    }
                    AfterExit::ReviewFailed {
                        attempt,
                        error,
                        duration_secs,
                        output,
                        session,
                    } => {
                        // The ask goes with the failure (task 328): the
                        // person answers it rather than finding the run in
                        // the attention. When it cannot be opened, the
                        // failure is the attention (review by hand).
                        let ask = match self.open_failed_review_ask(&run, attempt, output, &error) {
                            Ok(ask) => {
                                info!(run_id = %run.id(), "run {} waits for a person in ask {ask} after its failed review", run.id());
                                Some(ask)
                            }
                            Err(ask_error) => {
                                warn!(run_id = %run.id(), error = %format_args!("{ask_error:#}"), "run {}: the approve_landing ask of its failed review could not be opened: {ask_error:#}; it waits for a review by hand", run.id());
                                None
                            }
                        };
                        let mut payload = json!({
                            "code": if error.starts_with(landing::REVIEW_PROVIDER_DISABLED) {
                                "provider_disabled"
                            } else {
                                ReasonCode::JobFailed.as_str()
                            },
                            "attempt": attempt,
                            "error": error,
                            "duration_secs": duration_secs,
                            "status": run.status().as_str(),
                        });
                        if let Some(ask) = ask {
                            payload["ask_id"] = json!(ask);
                        }
                        if let Some(session) = &session {
                            session.record(&mut payload);
                        }
                        self.queue.record_runtime_event(
                            run.id(),
                            EventKind::ReviewFailed,
                            payload,
                        )?;
                        self.queue
                            .release_lease(run.id(), self.registration.token())?;
                        Ok(Step::Done(Box::new(self.queue.run(run.id())?)))
                    }
                    AfterExit::Rest { .. } => {
                        self.queue
                            .release_lease(run.id(), self.registration.token())?;
                        Ok(Step::Done(Box::new(run)))
                    }
                    AfterExit::Satisfied => match self.start_satisfied(&run)? {
                        Some(phase) => {
                            slot.run = run;
                            slot.transition(
                                phase,
                                EventKind::RunSatisfiedSnapshot.as_str(),
                                &*self.queue,
                            );
                            Ok(Step::Continue)
                        }
                        None => {
                            self.queue
                                .release_lease(run.id(), self.registration.token())?;
                            Ok(Step::Done(Box::new(run)))
                        }
                    },
                }
            }
            Phase::Satisfied(watch) => {
                // While a replanning or a hold fences the task, the
                // verification's end is acted on neither by an ask nor by a
                // close: the slot holds it for the park, which withholds it.
                if self.queue.turn_fence(slot.run.task_id())?.is_some() {
                    return Ok(Step::Continue);
                }
                // A verification a park joined and set aside, whose park
                // did not happen after all, runs again.
                if watch.handle.is_none() {
                    let ask = watch.ask;
                    slot.phase = self.verify_satisfied(&slot.run, ask)?;
                    return Ok(Step::Continue);
                }
                if !watch.handle.as_ref().is_some_and(|h| h.is_finished()) {
                    return Ok(Step::Continue);
                }
                // Read before the verification is taken out of the slot.
                let run = self.queue.run(slot.run.id())?;
                let outcome = watch
                    .handle
                    .take()
                    .context("verification already joined")?
                    .join()
                    .map_err(|_| anyhow!("the verification thread panicked"))?;
                match self.satisfied_verified(&run, watch, outcome)? {
                    satisfied::AfterVerified::Rested(run) => Ok(Step::Done(run)),
                    satisfied::AfterVerified::Judged(phase) => {
                        slot.run = run;
                        slot.transition(
                            *phase,
                            EventKind::RunSatisfiedReviewStarted.as_str(),
                            &*self.queue,
                        );
                        Ok(Step::Continue)
                    }
                }
            }
            Phase::SatisfiedReview(watch) => {
                // Fenced, the slot holds the judgment for the park, which
                // lets its job end and withholds what it found; a job that
                // ended is reaped meanwhile, so it holds no review job's
                // room, and nothing is acted on.
                if self.queue.turn_fence(slot.run.task_id())?.is_some() {
                    let run = slot.run.clone();
                    self.reap_claim_review(&run, watch)?;
                    return Ok(Step::Continue);
                }
                let run = slot.run.clone();
                Ok(match self.tend_claim_review(&run, watch)? {
                    Some(satisfied_review::AfterJudged::Rested(run)) => Step::Done(run),
                    Some(satisfied_review::AfterJudged::Verify(phase)) => {
                        slot.transition(
                            *phase,
                            EventKind::RunSatisfiedSnapshot.as_str(),
                            &*self.queue,
                        );
                        Step::Continue
                    }
                    None => Step::Continue,
                })
            }
        }
    }
}
