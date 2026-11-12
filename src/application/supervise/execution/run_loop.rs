//! The supervisor's loop: [`Supervisor::run_loop`] drives the passes of
//! [`Supervisor::drive`] until a stop, the end of the work or a handoff,
//! and ends the process's registration; [`Supervisor::loop_waits`] names
//! what a pass that holds no slot still waits for, [`Supervisor::end_by`]
//! when the loop must end, and the heartbeat's stop is noted and recorded
//! here ([`Supervisor::note_heartbeat_stopped`],
//! [`Supervisor::record_heartbeat_failure`]). The types they use
//! (`EndBy`, `Heartbeat`, `LoopSettings`) and the state of a slot stay in
//! `execution.rs`; this module only adds methods to `Supervisor`, whose
//! fields it sees as a submodule of `execution` without widening them
//! (rule C3 of docs/design/architecture.md).

use super::*;

impl Supervisor<'_> {
    /// Apply startup-only tables exactly once from the first good snapshot.
    /// A failed read leaves claims held; setting the host disk precedes the
    /// pass's resource reading. The config event records applied values.
    fn initialize_repository_rules(&mut self) -> Result<bool> {
        let Some(read) = self.repository_startup_file.clone() else {
            return Ok(false);
        };
        let Ok(rules) = read() else {
            return Ok(true);
        };
        self.stall = rules.stall;
        self.resume.config = rules.resume;
        self.review_jobs = rules.review_jobs;
        self.host.set_repository_disk(rules.disk);
        self.queue.record_queue_event(
            EventKind::SupervisorConfigChanged,
            json!({
                "supervisor": self.registration.token(),
                "repository_startup": { "stall": self.stall, "disk": self.host.disk_reader().config,
                    "resume": self.resume.config, "review_jobs": {"agent_timeout_secs": self.review_jobs.agent, "program_timeout_secs": self.review_jobs.program, "concurrency": self.review_jobs.concurrency} }
            }),
        )?;
        let mut stall = serde_json::to_value(self.stall)?;
        stall["supervisor"] = json!(self.registration.token());
        self.queue
            .record_queue_event(EventKind::StallConfigLoaded, stall)?;
        self.repository_startup_file = None;
        Ok(false)
    }

    /// Drive the loop, then remove this process's registration with the
    /// record of its stop: it is about to exit, whether it drained its
    /// runs, ran out of work, or failed on a claim or provisioning. A
    /// heartbeat failure (the database may be unreachable) or a stop that
    /// could not be recorded keeps the row, which goes stale with the
    /// leases for the next `up` or `down` to prune.
    ///
    /// Startup can enter with diagnostics abandoned after the service did
    /// not answer: the max-load display and start events are not resent,
    /// since their writes may have happened without an answer. Hold reads
    /// are made afresh; pending exec reconstruction runs at the beginning
    /// of the first reconciliation, before applying hold answers or
    /// starting new work. A further unanswered request leaves it pending
    /// for another pass through this loop's outage handler.
    ///
    /// The result, from `drive`: `outcome` is `stopped` when a stop
    /// was asked, `finished` when the loop ran out of work, and `handoff`
    /// when it hands over to another binary (with what the exec needs),
    /// `runs` the
    /// runs that came to rest (the `finished` field), `errors` those given up
    /// on ([`RunError`]), and `triaged` each run this process triaged with
    /// the status it went to.
    pub(super) fn run_loop(&mut self, options: &LoopSettings) -> Result<Value> {
        let result = loop {
            match self.drive(options) {
                // The service did not answer one of the supervisor's
                // operations (ADR-t3102-3 decision 5): nothing new starts,
                // the queue is not opened instead, and the pass is made
                // again once host運用 looked at the service, which it
                // starts again when it keeps it. A supervisor that stops
                // ends here.
                Err(error)
                    if crate::application::supervisor_service::unreachable(&error)
                        && !options.stop.load(Ordering::SeqCst)
                        && !options.once =>
                {
                    warn!(error = %format_args!("{error:#}"), "the queue service did not answer: no new run starts until it does: {error:#}");
                    self.unanswered.lost();
                    self.host.queue_service_unreached();
                    let reports = self.observation.reports();
                    if let Some((config, message)) = self
                        .on_host(|host, env| host.queue_service_pass(env, true, reports.as_ref()))
                    {
                        let now = self.generators.clock.monotonic();
                        self.observation.queue_pushes(config, vec![message], now);
                        self.on_observation(|observation, env| observation.push_pass(env, true));
                    }
                    thread::sleep(options.idle_poll);
                }
                result => break result,
            }
        };
        // The records held on the last pass go before the process ends or
        // execs, for as long as a few looks at the service take (ADR-t3102-3
        // decision 5), and for a supervisor whose owner is gone or whose
        // heartbeat stopped no longer than its grace; what is still held
        // then stays kept beside the queue for the next supervisor.
        self.pause_at_end(PausePoint::LoopEnded);
        // A heartbeat that stopped as the loop ended another way (a drain,
        // a handoff, an error) is found here; one that stops while the end
        // waits sets the deadline then, which every wait below reads again.
        self.note_heartbeat_stopped(options);
        let deadline = self.end_by(options);
        let clock = self.generators.clock.clone();
        let left = || deadline.left(clock.monotonic());
        let service = self.service.clone();
        match handoff::send_within(
            HELD_RECORD_TRIES,
            options.idle_poll,
            &left,
            &mut thread::sleep,
            &mut || matches!(service.send_held(), Ok(true)),
        ) {
            handoff::Sent::Taken => {}
            handoff::Sent::NotTaken => {
                warn!("the records the queue service did not take are kept for the next supervisor")
            }
            handoff::Sent::Late => warn!(
                "the grace of a supervisor whose owner is gone or whose heartbeat stopped is over: what it holds is not sent and stays kept for the next supervisor"
            ),
        }
        // The runs that ended on the last pass end their telemetry tokens
        // before the process goes; the process an exec hands the
        // registration to makes the revocations still held.
        self.sweep_telemetry();
        if self.registration.execs() {
            self.save_telemetry_retries();
        }
        self.host
            .host_metrics_finish(&deadline, &*self.generators.clock);
        self.host
            .moving_sizes_finish(&deadline, &*self.generators.clock);
        self.on_observation(|observation, env| observation.ledger_finish(env, &deadline));
        self.on_observation(|observation, env| observation.resources_finish(env, &deadline));
        // Why a heartbeat that stopped while those waited did, before the
        // jobs' ends, with which it is sent or kept.
        self.note_heartbeat_stopped(options);
        // The jobs the loop stopped last (a handoff stops them all), and
        // those a loop that failed leaves running: dropped only after this
        // last write, their ends would not be written.
        self.abandon_jobs();
        // An end the service did not take, and the stops of the jobs just
        // abandoned that it holds, are sent again for as long as the held
        // records were, within the same deadline; then the ends are kept
        // with them for the next supervisor, at once once it is over.
        let sent = handoff::send_within(
            HELD_RECORD_TRIES,
            options.idle_poll,
            &left,
            &mut thread::sleep,
            &mut || {
                let written = self.write_job_ends();
                let sent = matches!(service.send_held(), Ok(true));
                written && sent
            },
        );
        if sent != handoff::Sent::Taken {
            self.keep_job_ends();
        }
        // A loop that ended on an error lets the cleanup job end after its
        // current worktree (all candidates for disk space), recording its work.
        if result.is_err() {
            let held = self.cleanup_held();
            self.on_host(|host, env| {
                host.poll_cleanup(env, &held, true);
                host.finish_cleanup(env, &held, &deadline);
            });
        }
        if !self.registration.execs() {
            // Only a loop that ended on an error leaves one running.
            self.on_observation(|observation, env| {
                observation.stop_observer(env, "with the supervisor");
                observation.stop_throughput_review(env, "with the supervisor");
            });
        }
        // The heartbeat ends here, before the last look at it, so none
        // fails after that look: why one that stopped did is recorded (held
        // beside the queue at once when the service does not take it)
        // before the registration goes, or stays for a heartbeat that
        // stopped.
        self.pause_at_end(PausePoint::HeartbeatEnding);
        self.registration.stop_heartbeat();
        self.note_heartbeat_stopped(options);
        // Its token goes with the registration (ADR-t3102-1 decision 2):
        // the service is asked to revoke it as it stops before the
        // registration goes, since a `down --wait` that sees it gone may
        // stop the service before this process's last requests, as this
        // supervisor does after a `down`'s drain; one that keeps running
        // takes it back at this process's end.
        if self.registration.deregister(
            &*self.queue,
            &self.layout.version,
            result.is_ok(),
            &mut || crate::application::supervisor_service::sign_out_at_stop(&*service),
        ) {
            self.pause_at_end(PausePoint::Deregistered);
            // After the deregistration, so the last of the supervisors a
            // `down` stops sees no other one left.
            if result.is_ok() && options.stop.load(Ordering::SeqCst) {
                // The queue service after the supervisor (ADR-t1233-4
                // decision 1).
                // The queue broker with it (ADR-t2114-1 decision 1), first.
                self.on_host(|host, env| host.stop_queue_broker_after_down(env));
                self.on_host(|host, env| host.stop_queue_service_after_down(env));
            }
        }
        result
    }
    /// When the end of the loop stops waiting for its jobs ([`finished_by`])
    /// and for the queue to take what it holds ([`EndBy`]): for a
    /// supervisor whose owner is gone, once what is left of its grace from
    /// now is over, and for one whose heartbeat stopped, once the deadline
    /// set when it stopped is.
    fn end_by(&self, options: &LoopSettings) -> EndBy {
        let now = self.generators.clock.monotonic();
        EndBy {
            owner_gone: self.registration.owner_gone_since().map(|since| {
                now + handoff::owner_gone_left(
                    now.saturating_duration_since(since),
                    options.owner_gone_grace,
                )
            }),
            lease_lost: self.end.clone(),
        }
    }
    /// Whether the heartbeat stopped. The first look that finds it so sets
    /// the end's deadline from now, unless the heartbeat's thread set an
    /// earlier one as it stopped ([`EndDeadline`]), and records why
    /// ([`Self::record_heartbeat_failure`]): on each pass, and at the end of
    /// the loop for a heartbeat that stopped after the loop ended another
    /// way.
    fn note_heartbeat_stopped(&mut self, options: &LoopSettings) -> bool {
        if self.registration.check_heartbeat().is_ok() {
            return false;
        }
        if self.registration.note_lease_lost() {
            let now = self.generators.clock.monotonic();
            self.end.set_by(now + options.lease_lost_grace);
            self.record_heartbeat_failure();
        }
        true
    }
    /// Record why the heartbeat stopped as `supervisor_heartbeat_failed`
    /// ([`HeartbeatFailure::payload`]). A queue that stays busy, or a
    /// service that does not answer, holds it with the other records of
    /// results, sent at the end of the loop or kept beside the queue for
    /// the next supervisor (ADR-t3102-3 decision 5).
    fn record_heartbeat_failure(&mut self) {
        let Some(failure) = self.registration.heartbeat_failure() else {
            return;
        };
        let payload = failure.payload(self.registration.token());
        if let Err(error) = self
            .queue
            .record_queue_event(EventKind::SupervisorHeartbeatFailed, payload)
            && !crate::application::supervisor_service::resend::kept(&error)
        {
            warn!(error = %format_args!("{error:#}"), "why the heartbeat stopped could not be recorded: {error:#}");
        }
    }
    /// Abandon every headless job still running once the loop ended: the
    /// plan review and the goal review ([`PlanningState::abandon_jobs`])
    /// and the slots' jobs ([`HeadlessJob::abandon`]).
    fn abandon_jobs(&mut self) {
        self.on_planning(|planning, env| planning.register_stopping_sessions(env));
        self.register_job_sessions();
        self.planning.abandon_jobs();
        self.abandon_agent_eval();
        self.abandon_agent_cases();
        self.claim.slots.abandon_jobs();
    }
    fn drive(&mut self, options: &LoopSettings) -> Result<Value> {
        // Why a `--once` call went on to its next pass, as last logged.
        let mut goes_on = None;
        loop {
            let pass = options.passes.fetch_add(1, Ordering::SeqCst) + 1;
            // Every record of a `--once` call's pass names the pass.
            let _pass = options
                .once
                .then(|| tracing::debug_span!("pass", pass).entered());
            if let Err(error) = self.registration.check_heartbeat() {
                // Supervisor-level failure: every send from the heartbeat's
                // stop, of this pass and of the end of the loop, is bounded
                // by its deadline ([`LEASE_LOST_GRACE`]), its cause goes on
                // the queue first, and it is noted on every run; the leases
                // and the registration are kept, and go stale once this
                // process is gone.
                self.note_heartbeat_stopped(options);
                for slot in self.claim.slots.iter() {
                    let _ = self.queue.record_runtime_error(
                        slot.run.id(),
                        &format!("{error:#}"),
                        &ReasonCode::LeaseLost.into(),
                    );
                }
                // The runs' executors' telemetry ends with the lease: the
                // supervisor that adopts a run issues it another
                // (ADR-t1662-3 decision 11).
                for run in self.held_runs() {
                    self.revoke_lost_telemetry(&run);
                }
                return Err(error);
            }
            // An owner gone stops this process like a stop request, with
            // its own reason, but waits for nobody's answer: its runs are
            // let go and the loop ends within the grace. Looked for in a
            // drain too, which would otherwise wait for those answers.
            if self.registration.owner_gone_since().is_none()
                && handoff::owner_gone(options.owner, |pid| self.processes.alive(pid))
            {
                info!(
                    "supervisor {}: its owner (pid {}) is gone; it gives its runs back and stops",
                    self.registration.token(),
                    options.owner.unwrap_or_default()
                );
                let now = self.generators.clock.monotonic();
                self.registration.note_owner_gone(now);
                options.stop.store(true, Ordering::SeqCst);
            }
            // The records of results the service did not answer go first,
            // in order (ADR-t3102-3 decision 5), before the pass reads the
            // queue, so nothing it decides on misses them and nothing it
            // writes goes ahead of them: while the service still does not
            // answer, the pass is made again once host運用 looked at it, and
            // while the queue stays busy for them, after a wait, a `--once`
            // loop's too. A stop goes on without the wait, what it writes
            // kept behind them, and an owner gone past its grace ends the
            // loop: what is held is kept for the next supervisor.
            self.held_left = !self.service.send_held()?;
            if self.service.holds_for_busy_queue() && !options.stop.load(Ordering::SeqCst) {
                if self.owner_gone_ends(options) {
                    break;
                }
                info!(
                    "the records kept for the queue service go first: the pass is made once the queue takes them"
                );
                thread::sleep(options.idle_poll);
                continue;
            }
            let stopping = options.stop.load(Ordering::SeqCst);
            // First after a pass the queue service did not answer: what it
            // did without its answer reaching this process (ADR-t3102-3
            // decision 5), read with the results it held taken.
            self.reconcile_unanswered(stopping)?;
            if stopping && !self.registration.stop_recorded() {
                let runs = self.held_runs();
                self.registration
                    .record_stop_request(&*self.queue, &self.layout.version, &runs);
            }
            // Every pass, draining or handing off too (task 516).
            let handing_off = self.registration.handing_off();
            let (host, generators) = (&mut self.host, &self.generators);
            let (slots, queue) = (&self.claim.slots, &*self.queue);
            host.host_metrics_pass(
                handing_off,
                || generators.clock.now(),
                self.generators.clock.monotonic(),
                || waiting::held_worktrees(slots, queue),
            );
            // Before any job starts: none runs twice (task 443).
            self.tend_headless_jobs();
            // Every pass, draining or handing off too: the telemetry
            // tokens of the runs that ended (ADR-t1662-3 decision 11).
            self.sweep_telemetry();
            // What the cleanup job removed is recorded before the disk is
            // read (task 405).
            let ending = cleanup::ends_cleanup(stopping, self.registration.handing_off());
            let held = self.cleanup_held();
            self.on_host(|host, env| host.poll_cleanup(env, &held, ending));
            // After the cleanup was told to end: the loop's end joins it.
            if self.registration.owner_gone_since().is_some() {
                self.let_go_for_owner();
                if self.owner_gone_ends(options) {
                    break;
                }
            }
            // Every pass, draining or not, so a hold on landings ends as soon
            // as the program is found (ADR-0049 decision 9).
            self.check_run_env_programs()?;
            // And the container the worker or the verification is set to
            // (goal 38): what it holds waits, never on the host.
            self.check_container_runtime()?;
            // And the sccache server, which no process given [run.env]
            // may start (ADR-t1215-1, ADR-t2086-1).
            self.on_host(|host, env| host.sccache_pass(env));
            self.check_landing_branch(options.landing_recheck);
            self.mark_run_env_change()?;
            // Every pass, before any claim: a change of `[conflicts]` takes
            // effect without a restart (ADR-0080).
            self.reread_conflicts()?;
            // And `[supervisor]`: a change of `parallel`, `max_waiting` or
            // `runtime_planners` takes effect without a restart (task 698,
            // task 941).
            let startup_missing = self.initialize_repository_rules()?;
            self.claim
                .reread_limits(&*self.queue, self.registration.token())?;
            self.claim.repository_config_missing |= startup_missing;
            let hold = crate::application::supervise::repository_config::config_hold(
                self.claim.repository_config_missing,
            );
            let payload = hold.map(|hold| {
                (
                    "repository_config_unavailable",
                    json!({"message": hold.message()}),
                )
            });
            self.with_env(|env| env.record_own_hold(claim_hold::REPOSITORY_CONFIG_HOLD, payload));
            // And `[provider_fallback]`: turning the workers' fallback on
            // or off takes effect without a restart (ADR-t1857-1).
            self.reread_provider_fallback();
            // And `[fresh_session]`: a new threshold takes effect without
            // a restart (ADR-t2080-1).
            self.reread_fresh_session();
            // And `[ci_watch]`, whose check is reaped and started off the
            // loop, draining and handing off too (ADR-t1920-1).
            self.on_observation(|observation, env| observation.ci_watch_pass(env));
            // And the record of `stats`' judgments of now, draining and
            // handing off too, which nothing here reads back.
            let signals = self.signals;
            self.on_observation(|observation, env| {
                observation.live_alerts_pass(env, signals, options.live_alert_interval)
            });
            // And the comparison of the attributes the measurement folds
            // with the state, on the first pass and then at its interval,
            // which writes a baseline of what an older binary changed
            // without its records.
            self.on_observation(|observation, env| observation.attributes_baseline_pass(env));
            // And main's history, recorded as it moves for the measurement,
            // draining and handing off too.
            self.on_observation(|observation, env| observation.main_log_pass(env));
            // And the ledger, built off the loop from what was recorded,
            // draining and handing off too; nothing here reads it back.
            self.on_observation(|observation, env| observation.ledger_pass(env));
            let held = self.held_runs();
            self.on_observation(|observation, env| observation.resources_pass(env, held));
            // Every pass too, so a hold on landings ends as soon as there
            // is room (task 377).
            let landings: Vec<RunId> = self
                .claim
                .slots
                .iter()
                .filter(|slot| matches!(slot.phase, Phase::AwaitingSlot))
                .map(|slot| slot.run.id().clone())
                .collect();
            let held = self.cleanup_held();
            // The runs that go on by themselves, whose end may make room:
            // while one does, a shortage holds without asking. A run waiting
            // for a person, for its landing turn or for a provider's hold
            // does not.
            let moving: Vec<_> = self
                .claim
                .slots
                .iter()
                .filter(|slot| {
                    !slot.out_of_slot()
                        && !matches!(slot.phase, Phase::AwaitingSlot | Phase::ReviewHeld { .. })
                })
                .map(|slot| {
                    super::super::host_ops::moving_sizes::MovingTarget::of(
                        &slot.run,
                        &self.layout.runs_dir,
                    )
                })
                .collect();
            let in_flight = moving.len();
            // Of them, the landings under way: a landing's retry of its
            // verification counts the runs moving besides itself.
            let landing = self
                .claim
                .slots
                .iter()
                .filter(|slot| !slot.out_of_slot() && matches!(slot.phase, Phase::Landing(_)))
                .count();
            // A landing goes before the claims only when nothing but the
            // disk holds it.
            let landing_waiting = !landings.is_empty() && self.landing_goes_but_for_disk();
            self.on_host(|host, env| {
                host.check_disk(
                    env,
                    options.disk_cleanup_interval,
                    &landings,
                    &held,
                    super::super::host_ops::disk::DiskMoves {
                        in_flight,
                        landing,
                        landing_waiting,
                    },
                    &moving,
                )
            })?;
            // Every pass too: the answer of an authentication or usage-limit
            // ask is applied and the hold read before any work starts (task
            // 437), and Codex's hold ends once its time is up (ADR-t813-2).
            self.check_queue_hold(true)?;
            self.check_provider_holds()?;
            // A request may have been withdrawn while this process drained:
            // return to claims on this pass, without waiting for the slots
            // that had kept the handoff pending. One replaced by another
            // binary goes on draining for the new one, with no claim
            // between (task 1286).
            let mut handoff_withdrawn = false;
            if let Some(binary) = self.registration.handoff().map(str::to_owned) {
                let requested = self.queue.handoff_request(self.registration.token())?;
                let reread = handoff::reread(&binary, requested, stopping);
                if reread.change == handoff::HandoffChange::Withdrawn {
                    info!(
                        "supervisor {} handoff to {binary} was withdrawn; resuming normal work",
                        self.registration.token()
                    );
                    handoff_withdrawn = true;
                }
                self.registration.follow_reread(&binary, &reread);
                // The cleanup goes back to normal with the claims.
                if reread.resume_cleanup {
                    self.resume_cleanup();
                }
            }
            self.registration.drain_for(stopping, self.claim.claiming);
            self.note_handoff_drain(stopping);
            // Before any new work, draining or not: a drain waits for them
            // (ADR-0062 decision 8).
            self.return_waiting_runs();
            // A stop wins over a handoff: the drain goes on as before.
            if !stopping {
                if !self.registration.handing_off()
                    && !handoff_withdrawn
                    && let Some(binary) = self
                        .registration
                        .read_handoff(&*self.queue)?
                        .map(str::to_owned)
                {
                    info!(
                        "supervisor {} asked to hand off to {binary}: no new work starts; it execs once the validations and landings in progress are done",
                        self.registration.token()
                    );
                    // Read after the cleanup was polled: this pass
                    // drains on it already.
                    self.host.end_cleanup_for_handoff();
                    self.note_handoff_drain(stopping);
                }
                if let Some(binary) = self.registration.handoff().map(str::to_owned) {
                    self.sample_candidates(self.claim.parallel, true);
                    // A landing recheck in progress is waited for: its
                    // command would go on in the scratch worktree the next
                    // process uses (ADR-0068). None starts meanwhile (this
                    // pass drains). So is the cleanup job, which ends after
                    // its current worktree, or all candidates for disk space (task 648),
                    // and the rest of a cleanup for room it took on (task 1426).
                    self.recheck_pass();
                    if !self.landing.rechecks.running()
                        && !self.host.handoff_waits()
                        && !self.observation.handoff_waits()
                        // A run awaiting the landing slot is handed over,
                        // its landing not started (ADR-t3077-1).
                        && self.claim.slots.iter().all(Slot::rebuildable)
                    {
                        if self
                            .queue
                            .take_handoff(self.registration.token(), &binary)?
                        {
                            let runs = self.prepare_handoff();
                            info!(
                                "supervisor {} execs {binary}, handing over {runs} run(s)",
                                self.registration.token()
                            );
                            self.registration.exec_into(binary.clone());
                            return Ok(json!({
                                "outcome": "handoff",
                                "binary": binary,
                                "token": self.registration.token(),
                                "runs": self.finished,
                                "handed_over": runs,
                                "errors": self.errors,
                                "triaged": self.triage.triaged,
                            }));
                        }
                        // Not taken: withdrawn, or replaced by another
                        // binary, which the next pass execs (task 1286).
                        let requested = self.queue.handoff_request(self.registration.token())?;
                        let reread = handoff::reread(&binary, requested, stopping);
                        // The same binary asked again meanwhile: taken on
                        // the next pass; one replaced goes on draining.
                        if reread.change == handoff::HandoffChange::Withdrawn {
                            info!(
                                "supervisor {} handoff to {binary} was withdrawn before exec; resuming normal work",
                                self.registration.token()
                            );
                        }
                        self.registration.follow_reread(&binary, &reread);
                        self.registration.drain_for(stopping, self.claim.claiming);
                        self.note_handoff_drain(stopping);
                        if reread.resume_cleanup {
                            self.resume_cleanup();
                        }
                    }
                    if self.registration.handing_off() {
                        self.registration.drain();
                        self.poll_observer();
                        self.reap_throughput_reviews(options);
                        self.on_observation(|observation, env| {
                            observation.report_pass(env, false);
                            observation.forecast_pass(env, false);
                        });
                        self.on_host(|host, env| host.release_pass(env, false));
                        self.on_observation(|observation, env| observation.push_pass(env, false));
                        // A plan or goal review that ended meanwhile is
                        // reaped and its verdict applied, none started: a
                        // verdict left to the exec would be thrown away
                        // (task 1425). A task it readies waits for the
                        // next process's claim.
                        self.on_planning(|planning, env| {
                            planning.plan_review_pass(env, options, false);
                            planning.goal_review_pass(env, false);
                        });
                        self.agent_eval_pass(false);
                        self.agent_cases_pass(false);
                        self.tick(true)?;
                        self.unanswered.look();
                        thread::sleep(options.tick);
                        continue;
                    }
                }
            }
            // The queue service looked at, started again or replaced
            // (ADR-t1233-4 decision 2); while it is down no new run and no
            // queue's job starts.
            let working = !stopping && self.claim.claiming;
            let reports = self.observation.reports();
            if let Some((config, message)) =
                self.on_host(|host, env| host.queue_service_pass(env, working, reports.as_ref()))
            {
                let now = self.generators.clock.monotonic();
                self.observation.queue_pushes(config, vec![message], now);
            }
            // The queue broker likewise (ADR-t2114-1 decision 1); while it is
            // down the container runtime cannot be used.
            self.on_host(|host, env| host.queue_broker_pass(env, working));
            // And the runs' relays to it (ADR-t2114-3 decision 4).
            self.on_host(|host, env| host.broker_relay_pass(env, working));
            if self.claim.claiming && !stopping {
                let held = self.fill_slots(self.claim.parallel, options.sweep_interval)?;
                self.sample_candidates(self.claim.parallel, held);
            } else {
                self.sample_candidates(self.claim.parallel, true);
                // A draining supervisor still frees the integration slot a
                // dead landing holds: its own runs waiting to land, and so
                // the drain, would wait for it forever (task 1118).
                self.release_dead_landings()?;
                // A drain that stops applies the landing answers, and lands
                // what they queued only while its own runs keep it going
                // (ADR-t3330-1).
                if stopping {
                    self.drain_landing_answers()?;
                }
            }
            self.poll_observer();
            self.record_session_turns(false);
            let rechecked = self.recheck_pass();
            // A supervisor that stopped claiming is draining, not observing
            // nor starting plan reviews, nor updating itself.
            let working = !stopping && self.claim.claiming;
            self.on_observation(|observation, env| {
                // Reaped on every pass, started only by a supervisor at work.
                observation.report_pass(env, working);
                // Looked for by a supervisor at work only: the triggers stay
                // in the queue for the next look (ADR-0070 decision 3).
                observation.forecast_pass(env, working);
                // The watch of the adopted agent definitions, likewise:
                // what it counts stays in the queue (ADR-t1728-1 (g)).
                observation.agent_watch_pass(env, working);
            });
            // Looked for by a supervisor at work only (ADR-t618-1).
            self.on_host(|host, env| host.release_pass(env, working));
            // A message waiting is sent while the supervisor does not stop.
            self.on_observation(|observation, env| observation.push_pass(env, !stopping));
            // An inbox without a watcher while asks wait for it is recorded
            // and told through `[push]` (ADR-t1433-5 decision 1 (3)),
            // draining or not: a drain waits for their answers. The
            // watcher's changes are recorded either way (task 1021).
            let reports = self.observation.reports();
            if let Some((config, message)) =
                self.on_host(|_, env| inbox_nudge::inbox_nudge_pass(env, reports.as_ref()))
            {
                let now = self.generators.clock.monotonic();
                self.observation.queue_pushes(config, vec![message], now);
            }
            // Reaped on every pass, started only by a supervisor at work.
            // Its route decides on `--no-claude` and the hold: a login or
            // usage limit that holds Claude starts no Claude review, but
            // one whose role names its provider may run on Codex
            // (ADR-t1063-1 decision 5, ADR-t1204-1, task 1220).
            self.reap_throughput_reviews(options);
            self.start_throughput_review(
                options,
                !stopping && self.claim.claiming && self.host.service_up(),
            );
            if !stopping && self.claim.claiming {
                // Its route decides on `--no-claude` and the hold as the
                // throughput review's does (task 1223).
                if self.host.service_up() {
                    self.start_observer_when_due(options);
                }
                self.on_host(|host, env| {
                    host.auto_update_pass(env, options);
                    host.release_update_pass(env, options);
                });
            }
            // A plan review that just readied tasks is followed by one more
            // pass, which claims them.
            // The plan and goal reviews decide on the hold itself: a login
            // or usage limit that holds the queue starts no Claude job (task
            // 437), but one whose role names its provider may run on Codex
            // while Claude is held (ADR-t1063-1 decision 5). One in progress
            // is followed.
            let starting = !stopping && self.claim.claiming && self.host.service_up();
            // Outside the run slots: the detection of a long-running task
            // and the read-only diagnosis of the replanning operations,
            // which only continues, parks or asks; a job that runs is
            // reaped on every pass, a new one started only by a supervisor
            // at work.
            let diagnosed = self.replan_diagnosis_pass(starting).unwrap_or_else(|error| {
                warn!(error = %format_args!("{error:#}"), "the replanning diagnosis pass failed: {error:#}");
                false
            });
            let mut progressed = diagnosed;
            progressed |= self.on_planning(|planning, env| {
                let reviewed = planning.plan_review_pass(env, options, starting);
                planning.goal_review_pass(env, starting) | reviewed
            });
            // The eval's rounds (ADR-t1728-1 decision 9): outside the run
            // slots, one at a time, started only by a supervisor at work.
            progressed |= self.agent_eval_pass(starting);
            // The production sets asked for (ADR-t1728-1 (g)), the same way.
            progressed |= self.agent_cases_pass(starting);
            if self.claim.slots.is_empty() {
                // The jobs it waits for ([`Self::loop_waits`]) are waited
                // for like a run.
                let waits = self.loop_waits(options.once, stopping, rechecked);
                let job = !waits.is_empty();
                if !job && !progressed && (options.once || stopping || !self.claim.claiming) {
                    break;
                }
                if options.once {
                    note_once_goes_on(&mut goes_on, pass, 0, waits, progressed);
                }
                self.unanswered.look();
                thread::sleep(if job { options.tick } else { options.idle_poll });
                continue;
            }
            if options.once {
                let slots = self.claim.slots.len();
                note_once_goes_on(&mut goes_on, pass, slots, Vec::new(), false);
            }
            self.tick(false)?;
            self.unanswered.look();
            thread::sleep(options.tick);
        }
        let held = self.cleanup_held();
        let deadline = self.end_by(options);
        self.on_host(|host, env| host.finish_cleanup(env, &held, &deadline));
        if let Some(message) = &self.claim.provisioning_error {
            bail!(
                "{message}; claiming stopped and {} active run(s) were drained; inspect doctor before recovery",
                self.finished.len()
            );
        }
        let outcome = if self.registration.owner_gone_since().is_some() {
            "owner_gone"
        } else if options.stop.load(Ordering::SeqCst) {
            "stopped"
        } else {
            "finished"
        };
        Ok(json!({
            "outcome": outcome,
            "runs": self.finished,
            "errors": self.errors,
            "triaged": self.triage.triaged,
        }))
    }
    /// The jobs a loop with no slot waits for, like a run, by name: a
    /// running observer, KPI report job, plan review, landing recheck or
    /// cleanup for disk space (with the rest of one another job took on)
    /// or one a triage or resume waits for is bounded by its own timeout,
    /// its command or its worktrees. A recheck just applied
    /// (`rechecked`) is followed by one more pass, which resumes the runs
    /// it parked; a cleanup for room by one that claims if there is room
    /// now. Any other cleanup is joined once the loop ends. A message
    /// being sent is bounded by the command's timeout; `once` also waits
    /// for those still to be tried, a stop does not.
    fn loop_waits(&self, once: bool, stopping: bool, rechecked: bool) -> Vec<&'static str> {
        let mut waits: Vec<&'static str> = [
            ("observation", self.observation.busy(once && !stopping)),
            ("release", self.host.release_running()),
            ("planning", self.planning.busy(stopping)),
            ("agent_eval", self.agent_eval_busy()),
            ("agent_cases", self.agent_cases_busy()),
            ("landing_recheck", self.landing.rechecks.running()),
            ("cleanup_for_disk", self.host.cleanup().for_disk()),
            ("cleanup_deferred", self.host.cleanup().deferred()),
            ("recheck_applied", rechecked),
            ("replan_diagnosis", self.diagnosis_running()),
        ]
        .into_iter()
        .filter_map(|(name, waits)| waits.then_some(name))
        .collect();
        // Read from the queue, so only when nothing else is waited for. A
        // supervisor that stops leaves the park to the next.
        if waits.is_empty() && !stopping && self.hold_parks_going() {
            waits.push("hold_park");
        }
        waits
    }
}

/// Log why a `--once` call goes on from its pass `pass` to the next when
/// the reason changed from `last`, and say whether it did: the slots it
/// holds, or with none the jobs it waits for
/// ([`Supervisor::loop_waits`]) and whether this pass progressed. Logged
/// only with `--once`, whose passes are bounded by the call; each of its
/// records, a resume's too, names its pass (the `pass` span of
/// [`Supervisor::drive`]), and as one pass resumes a run at most once, two
/// resumes of one call show in which passes they were and what kept the
/// loop going between them.
fn note_once_goes_on(
    last: &mut Option<(usize, Vec<&'static str>, bool)>,
    pass: u64,
    slots: usize,
    waits: Vec<&'static str>,
    progressed: bool,
) -> bool {
    let now = (slots, waits, progressed);
    if last.as_ref() == Some(&now) {
        return false;
    }
    let (slots, waits, progressed) = &now;
    tracing::debug!(
        pass,
        slots,
        waits = ?waits,
        progressed,
        "--once pass {pass} goes on: {slots} slot(s), waiting for {waits:?}, progressed {progressed}"
    );
    *last = Some(now);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `--once` call logs why it goes on to another pass only when the
    /// reason changed since the pass it last logged.
    #[test]
    fn a_once_call_logs_why_it_goes_on_when_the_reason_changes() {
        let mut last = None;
        assert!(note_once_goes_on(&mut last, 1, 1, Vec::new(), false));
        assert!(!note_once_goes_on(&mut last, 2, 1, Vec::new(), false));
        assert!(note_once_goes_on(
            &mut last,
            3,
            0,
            vec!["observation"],
            false
        ));
        assert!(!note_once_goes_on(
            &mut last,
            4,
            0,
            vec!["observation"],
            false
        ));
        assert!(note_once_goes_on(&mut last, 5, 0, Vec::new(), true));
        assert_eq!(last, Some((0, Vec::new(), true)));
    }
}
