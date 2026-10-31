//! The supervisor's pass and its periodic jobs: [`Supervisor::tick`]
//! steps the slots once (all of them, or with `unsettled_only` only those
//! a handoff waits for); the observer is started when due
//! ([`Supervisor::start_observer_when_due`]) and reaped
//! ([`Supervisor::poll_observer`]), the throughput reviews likewise
//! ([`Supervisor::start_throughput_review`],
//! [`Supervisor::reap_throughput_reviews`]), and the providers their
//! finishes found unusable are held ([`Supervisor::hold_timer_jobs`]); the
//! transcript turns and token cuts of the session spans are recorded at
//! their interval ([`Supervisor::record_session_turns`]), closing first
//! the hook spans whose session ended ([`Supervisor::close_ended_sessions`]).
//! The loop in `run_loop` calls them. The types they use and the state of
//! a slot stay in `execution.rs`; this module only adds methods to
//! `Supervisor`, whose fields it sees as a submodule of `execution`
//! without widening them (rule C3 of docs/design/architecture.md).

use super::*;

impl Supervisor<'_> {
    /// One pass over the slots; with `unsettled_only`, over the slots a
    /// handoff waits for (their validation, e2e or landing in progress)
    /// and its workers' receipts only ([`Slot::pass`]). A step the queue
    /// service did not answer ends the pass with that error, its slot kept
    /// as it was and judged against the queue once the service answers
    /// ([`Self::reconcile_unanswered`]): the service may have done what it
    /// was asked, so the step is neither given up on nor taken for done.
    pub(super) fn tick(&mut self, unsettled_only: bool) -> Result<()> {
        self.sample_load();
        // A turn's container waits for its sccache server before its agent.
        self.answer_sccache_requests();
        if !unsettled_only {
            self.start_waits();
        }
        let mut index = 0;
        while index < self.claim.slots.len() {
            let landing_waited = self.claim.slots.iter().any(Slot::landing_runs);
            let pass = self.claim.slots[index].pass(unsettled_only, landing_waited);
            if pass == Pass::Skip {
                index += 1;
                continue;
            }
            let mut slot = self.claim.slots.release(index);
            let writes = crate::application::supervisor_service::writes_tried();
            let stepped = if let Pass::Receipt { validate } = pass {
                self.observe_receipt_handing_off(&mut slot, validate)
            } else if slot.out_of_slot() {
                self.watch_waiting(&mut slot)
            } else {
                self.step(&mut slot)
            };
            if let Some(worker) = self.provider.moved.remove(slot.run.id()) {
                slot.run = slot.run.clone().running_on(worker);
            }
            let writes = crate::application::supervisor_service::writes_tried() - writes;
            // A step that failed on its own is given up on below unless
            // its lease went to another process; the lease read once here,
            // and one the service did not answer judged by the next
            // reconciliation, not taken for held or lost.
            let held = match &stepped {
                Err(error) if !crate::application::supervisor_service::unreachable(error) => {
                    match self
                        .queue
                        .holds_lease(slot.run.id(), self.registration.token())
                    {
                        Ok(held) => Some(held),
                        Err(read) if crate::application::supervisor_service::unreachable(&read) => {
                            self.unanswered
                                .doubt(slot.run.id(), reconcile::Doubt::MaybeDone);
                            self.claim.slots.put_back(index, slot);
                            return Err(read);
                        }
                        Err(_) => Some(true),
                    }
                }
                _ => None,
            };
            match stepped {
                Ok(Step::Continue) => {
                    // What moved without a move of its own: the landing
                    // turn the step judged again.
                    let cause = match &slot.waiting {
                        Some(waiting) if waiting.ended.is_some() => {
                            EventKind::RunWaitingEnded.as_str()
                        }
                        _ if slot.in_landing_queue() => "landing_turn",
                        _ => "observed",
                    };
                    slot.note_phase(cause, &*self.queue);
                    self.claim.slots.put_back(index, slot);
                    index += 1;
                }
                Ok(Step::Done(run)) => {
                    info!(run_id = %run.id(), "run {} is {}", run.id(), run.status().as_str());
                    stages::record_rest(
                        &*self.queue,
                        &run,
                        slot.track.attempt(),
                        run.status().as_str(),
                        Some(&self.layout.version),
                    );
                    // Main moved: the runs that wait to land are checked
                    // against it (ADR-0068 decision 1).
                    if run.status() == RunStatus::Integrated {
                        self.note_landed(&run);
                    }
                    // A landed run needs none of its workspaces; a failed
                    // one keeps them for its triage.
                    if matches!(run.status(), RunStatus::Integrated | RunStatus::Succeeded)
                        && let Err(error) = close_open_workspaces(
                            &mut *self.queue,
                            self.sessions,
                            &run,
                            WorkspaceCloser::Runtime,
                        )
                    {
                        warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: its workspaces could not all be closed: {error:#}", run.id());
                    }
                    close_ended_landing_asks(&mut *self.queue, Some(run.task_id()));
                    if !crate::domain::queue_service::run_holds_token(run.status()) {
                        self.revoke_worker_token(&run);
                    }
                    self.request_cleanup(Some(run.task_id()));
                    self.finished.push(*run);
                }
                Ok(Step::Triaged(run)) => self.note_triaged(&run),
                // What the step did is judged once the service answers
                // ([`reconcile`]): the slot stays as the step left it.
                Err(error) if crate::application::supervisor_service::unreachable(&error) => {
                    self.unanswered
                        .doubt(slot.run.id(), reconcile::Doubt::of(writes, &error));
                    self.claim.slots.put_back(index, slot);
                    return Err(error);
                }
                Ok(Step::Disowned) => {
                    self.stop_slot_job(&mut slot);
                    self.disown(&slot)
                }
                // A lease-guarded write that failed because the lease
                // changed hands mid-step (this process was stalled and
                // adopted from) is the other owner's run to describe. Its
                // jobs stop, and send their steps, before the token ends.
                Err(_) if held == Some(false) => {
                    self.stop_slot_job(&mut slot);
                    self.disown(&slot)
                }
                Err(error) if matches!(slot.phase, Phase::Recovery(_)) => {
                    self.stop_slot_job(&mut slot);
                    let (round, alert, attempt) = match &slot.phase {
                        Phase::Recovery(watch) => (watch.round, watch.alert, watch.attempt),
                        _ => unreachable!("matched a recovery"),
                    };
                    self.fail_recovery(
                        &slot.run,
                        round,
                        alert,
                        attempt,
                        format!("{error:#}"),
                        0,
                        &recovery::JobEnd::default(),
                    );
                    let run = self.queue.run(slot.run.id()).unwrap_or(slot.run);
                    self.note_triaged(&run);
                }
                Err(error) if matches!(slot.phase, Phase::AwaitingSlot) => {
                    // The resume already recorded its `resume_finished`;
                    // only the lease it kept for the landing goes.
                    let message = format!("landing could not start: {error:#}");
                    warn!(run_id = %slot.run.id(), "run {}: {message}", slot.run.id());
                    if let Err(error) = self
                        .queue
                        .release_lease(slot.run.id(), self.registration.token())
                    {
                        warn!(run_id = %slot.run.id(), error = %format_args!("{error:#}"), "run {}: could not release the lease: {error:#}", slot.run.id());
                    }
                    self.record_rest(slot.run.id(), slot.track.attempt(), "landing_not_started");
                    self.errors.push(RunError {
                        run_id: slot.run.id().clone(),
                        task_id: slot.run.task_id(),
                        message,
                    });
                }
                Err(error) if matches!(slot.phase, Phase::Resume(_)) => {
                    // Nobody reads the verdict of its recovery jobs now.
                    self.stop_slot_job(&mut slot);
                    let message = format!("{error:#}");
                    warn!(run_id = %slot.run.id(), "run {} resume stopped: {message}; its workspace is kept for inspection", slot.run.id());
                    let (attempt, workspace) = match &slot.phase {
                        Phase::Resume(watch) => (watch.attempt, Some(watch.workspace.clone())),
                        _ => unreachable!("matched a resume"),
                    };
                    // One read of whether its wrapper registered both
                    // holds a continuation that did not start `blocked`
                    // and stops the session: a wrapper that registers in
                    // between is stopped with it, not left running.
                    let processes = match self.queue.processes(slot.run.id()) {
                        Ok(processes) => {
                            self.block_unregistered_continuation(&slot.run, &message, &processes);
                            Some(processes)
                        }
                        Err(error) => {
                            warn!(run_id = %slot.run.id(), error = %format_args!("{error:#}"), "run {}: whether its resumed session registered could not be read: {error:#}", slot.run.id());
                            None
                        }
                    };
                    self.give_up_resume(
                        &slot.run,
                        attempt,
                        workspace.as_deref(),
                        message,
                        &reason_of_error(&error, ReasonCode::Other),
                        processes,
                    );
                }
                Err(error) => {
                    // Creation/communication failures can be ambiguous: the
                    // session may be alive. Disown the run, delete nothing,
                    // and keep serving the other slots. A headless review in
                    // progress is stopped: nobody would read its verdict.
                    // A session kept open through validation, review, revise
                    // or its `/exit` is asked to end, since nothing would
                    // watch it any more (task 237).
                    self.stop_slot_job(&mut slot);
                    let session = self.exit_abandoned_session(&slot);
                    let message = format!("{error:#}");
                    warn!(run_id = %slot.run.id(), "run {} retained for inspection: {message}; see show {} and doctor", slot.run.id(), slot.run.task_id());
                    self.abandon_with_session(
                        &slot.run,
                        message,
                        &reason_of_error(&error, ReasonCode::Other),
                        session.as_ref(),
                    );
                }
            }
        }
        Ok(())
    }
    /// Launch `dagq observe` when an observation is due and none is
    /// running ([`ObservationState::observer_due`]), on the route
    /// 実行と着地's holds give it. The observer reads the active time of the
    /// spans still open too, recorded first.
    pub(super) fn start_observer_when_due(&mut self, options: &LoopSettings) {
        let mode = match self
            .on_observation(|observation, env| observation.observer_due(env, options))
        {
            Ok(Some(mode)) => mode,
            Ok(None) => return,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "observer schedule could not be read: {error:#}");
                return;
            }
        };
        let Some(route) = self.job_start_route(crate::domain::actor_model::ModelRole::Observer)
        else {
            return;
        };
        // Its finish is the first `observe_finished` past this mark.
        let mark = match self.queue.latest_event_id() {
            Ok(mark) => mark,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "observer schedule could not be read: {error:#}");
                return;
            }
        };
        // The observer reads the spans' turns, so they are recorded first.
        self.record_session_turns(true);
        let fallback_jobs = self.provider.fallback.jobs;
        self.on_observation(|observation, env| {
            observation.start_observer(env, mode, route, mark, fallback_jobs);
        });
    }
    /// Start the throughput review due, on the route 実行と着地's holds give
    /// it, when `start` and none runs. The loop gives `start` false while
    /// claiming is stopped, the supervisor is stopping or the queue service
    /// is down, so no review starts then.
    pub(super) fn start_throughput_review(&mut self, options: &LoopSettings, start: bool) {
        let Some(due) = self.on_observation(|observation, env| {
            observation.throughput_review_due(env, options, start)
        }) else {
            return;
        };
        let Some(route) =
            self.job_start_route(crate::domain::actor_model::ModelRole::ThroughputReview)
        else {
            return;
        };
        let fallback_jobs = self.provider.fallback.jobs;
        self.on_observation(|observation, env| {
            observation.start_throughput_review(env, due, route, fallback_jobs);
        });
    }
    /// Reap the throughput reviews and have the providers their finishes
    /// say could not be used held.
    pub(super) fn reap_throughput_reviews(&mut self, options: &LoopSettings) {
        let unusable = self
            .on_observation(|observation, env| observation.reap_throughput_reviews(env, options));
        self.hold_timer_jobs(unusable);
    }
    /// Record the finished transcript turns of the open session spans, and
    /// the remaining turns and measurements of the hook spans closed
    /// without them (ADR-t655-1), and the token cuts of the inbox and
    /// person's planner spans that are due one (ADR-t1486-1 decision 3:
    /// due by the time of their last cut, so the cuts stay hourly across
    /// restarts), when [`SESSION_TURNS_INTERVAL`] passed
    /// since the last time (at once with `now`). A failure is logged only: it changes no run (ADR-0048
    /// decision 10), and the next time reads the transcripts again.
    pub(super) fn record_session_turns(&mut self, now: bool) {
        let since_last = self.last_turns.map(|at| self.generators.clock.since(at));
        if !session_turns_due(now, since_last) {
            return;
        }
        self.last_turns = Some(self.generators.clock.monotonic());
        self.close_ended_sessions();
        match self.queue.record_session_turns() {
            Ok(0) => {}
            Ok(spans) => info!("recorded the transcript turns of {spans} session span(s)"),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "transcript turns could not be recorded: {error:#}")
            }
        }
        match self.queue.record_session_tokens() {
            Ok(0) => {}
            Ok(cuts) => info!("recorded {cuts} token cut(s) of interactive session spans"),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the tokens of interactive session spans could not be cut: {error:#}")
            }
        }
    }
    /// Close, as `inferred`, the inbox and planner spans the hook recorded
    /// whose session is over without its `SessionEnd`
    /// ([`crate::domain::sessions::inferred_hook_closes`], ADR-t2022-1),
    /// judged from the queue's planner rows and the processes of their
    /// background wrappers; no cmux is asked. A failure is logged only
    /// (ADR-0048 decision 10).
    fn close_ended_sessions(&mut self) {
        let spans = match self.queue.open_hook_session_spans() {
            Ok(spans) if spans.is_empty() => return,
            Ok(spans) => spans,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the open inbox and planner session spans could not be read: {error:#}");
                return;
            }
        };
        // Unread, a person's planner counts as open: only the spans that
        // name no planner wait for the next pass.
        let person_planners_open = match self.queue.planners(false) {
            Ok(planners) => planners.iter().any(|planner| planner.person_retired()),
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the planners of the open session spans could not be read: {error:#}");
                true
            }
        };
        let mut over = std::collections::HashMap::new();
        for id in spans
            .iter()
            .filter_map(|span| span.payload["planner_id"].as_i64())
        {
            if over.contains_key(&id) {
                continue;
            }
            // A row that cannot be read is not taken for over.
            let ended = self
                .queue
                .planner(crate::domain::PlannerId::new(id))
                .is_ok_and(|planner| {
                    planner.session_over(
                        |pid| self.processes.alive(pid),
                        |pid| self.processes.start_identity(pid),
                    )
                });
            over.insert(id, ended);
        }
        let ended: Vec<EventId> = crate::domain::sessions::inferred_hook_closes(
            &spans,
            |id| over.get(&id).copied().unwrap_or(false),
            person_planners_open,
        )
        .into_iter()
        .map(|span| span.opened_event_id)
        .collect();
        if ended.is_empty() {
            return;
        }
        match self.queue.close_inferred_sessions(&ended) {
            Ok(0) => {}
            Ok(closed) => {
                info!("closed {closed} inbox or planner session span(s) whose session is over")
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the spans of ended inbox and planner sessions could not be closed: {error:#}")
            }
        }
    }
    /// Have 実行と着地, which owns the holds, hold the providers the
    /// finishes 観測と分析 reaped (`unusable`) say could not be used
    /// ([`Self::hold_timer_jobs_unusable`]), and make the jobs whose
    /// provider is held now due again ([`Self::timer_job_due_again`]),
    /// before either job's next start reads the holds.
    fn hold_timer_jobs(&mut self, unusable: Vec<observer::UnusableTimerJob>) {
        if unusable.is_empty() {
            return;
        }
        for job in self.hold_timer_jobs_unusable(unusable) {
            self.observation.timer_job_due_again(job);
        }
    }
    /// Reap the observer once it exited; one that found its provider
    /// unusable has that provider held and is due again
    /// ([`ObservationState::poll_observer`],
    /// [`Self::hold_timer_jobs`]).
    pub(super) fn poll_observer(&mut self) {
        let unusable = self.on_observation(|observation, env| observation.poll_observer(env));
        self.hold_timer_jobs(unusable.into_iter().collect());
    }
}

/// Whether the session turns are recorded now: at once with `now`, else on
/// the first pass and [`SESSION_TURNS_INTERVAL`] after the last time, given
/// as the time since.
fn session_turns_due(now: bool, since_last: Option<Duration>) -> bool {
    now || since_last.is_none_or(|since| since >= SESSION_TURNS_INTERVAL)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The session turns are recorded on the first pass, at once with
    /// `now`, and at [`SESSION_TURNS_INTERVAL`] after the last time, not
    /// 1 ms before.
    #[test]
    fn the_session_turns_are_recorded_at_their_interval() {
        let ms = Duration::from_millis(1);
        assert!(session_turns_due(false, None));
        assert!(!session_turns_due(false, Some(SESSION_TURNS_INTERVAL - ms)));
        assert!(session_turns_due(false, Some(SESSION_TURNS_INTERVAL)));
        assert!(session_turns_due(true, Some(Duration::ZERO)));
    }
}
