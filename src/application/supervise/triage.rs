//! The recovery of `failed` and `interrupted` runs (ADR-0047 decisions 39
//! and 40, in place of the triage of ADR-0024 decision 3): dead runs
//! recovered, the recovery job of their `failed`, `interrupted` and
//! `resume_exhausted` alerts, its verdict, and the answers to the `decide`
//! asks it escalates to. The rounds keep the triage's event names
//! (`triage_started`, `triage_finished`, `triage_failed`).

use super::recovery::JobEnd;
use super::*;
use crate::domain::EventKind;
use crate::domain::RecoveredLanding;
use crate::domain::actor_model::{ActorLaunch, JobStartRoute, ModelRole};
use crate::domain::landing_release;
use crate::domain::recovery::{
    ENDED_ACTIONS, MAX_RECHECK_SECS, MAX_RECOVERY_ATTEMPTS, ProcessInfo, RecoveryAction,
    VERIFY_FIX_OPTION, attempts, current_alert, pending_request, run_processes,
    verification_failed, verify_fix_round,
};

impl Supervisor<'_> {
    /// Recover the unfinished runs whose wrapper exited or died and whose
    /// supervisor is gone (ADR-0024 decision 3, amending ADR-0012):
    /// `recover`'s own check (no live process of the run; `doctor`'s
    /// blockers empty) on `claimed` / `starting` / `running` / `validating`
    /// runs without a lease row, or with a stale lease whose pid is dead
    /// that [`Self::adopt_stale_runs`] would not take (task 236: a run the
    /// dead supervisor left behind with its session). They become
    /// `interrupted` with `run_recovered` (`by: supervisor`) and go to the
    /// triage, never straight to `ready`. A run that changed meanwhile is
    /// left for a later pass. An `integrating` run whose landing nobody
    /// carries on is released instead ([`Self::release_dead_landing`]).
    pub(super) fn recover_dead_runs(&mut self) -> Result<()> {
        let now = self.generators.clock.now();
        for run in self.queue.active_runs()? {
            if run.status() == RunStatus::Integrating {
                self.release_dead_landing(&run, now)?;
                continue;
            }
            let processes = self.queue.processes(run.id())?;
            let lease = match self.queue.run_lease(run.id())? {
                None => None,
                Some(lease) => {
                    let wrapper = processes.iter().find(|p| p.role == "wrapper");
                    if self.processes.alive(lease.pid) || self.adoptable(&run, wrapper, now)? {
                        continue;
                    }
                    Some(lease_health(&lease, now, &*self.processes))
                }
            };
            let leased = lease.is_some();
            let health = run_health(&run, &processes, lease, now, &*self.processes, &*self.files);
            if !health.recoverable {
                continue;
            }
            let report = json!({"run": health, "by": "supervisor"});
            match self.queue.recover_run(run.id(), processes.len(), report) {
                Ok(recovered) => {
                    let whose = if leased {
                        "its supervisor died"
                    } else {
                        "nobody leases it"
                    };
                    info!(run_id = %recovered.id(), task_id = %recovered.task_id(), "run {} of task {} recovered from {}: {whose} and its session is gone; it goes to triage", recovered.id(), recovered.task_id(), run.status().as_str())
                }
                Err(error) => {
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {} could not be recovered: {error:#}", run.id())
                }
            }
        }
        Ok(())
    }
    /// [`Self::release_dead_landing`] of every `integrating` run, for a
    /// supervisor that drains and recovers nothing else.
    pub(super) fn release_dead_landings(&mut self) -> Result<()> {
        let now = self.generators.clock.now();
        for run in self.queue.runs_with_status(RunStatus::Integrating)? {
            self.release_dead_landing(&run, now)?;
        }
        Ok(())
    }
    /// Give back the integration slot an `integrating` run holds while
    /// nobody lands it (task 1118): its lease row is missing, or its lease
    /// is stale (as [`Self::adopt_stale_runs`] judges it) with its pid dead,
    /// and `recover`'s own check passes (no registered process of the run
    /// alive, the lease's heartbeat older than the limit), and no process
    /// works in its worktree (a verification command the dead landing left
    /// running; past their grace the runtime stops them by pid,
    /// [`Self::stop_landing_processes`], task 1129). The run goes back to `awaiting_integration` unleased with
    /// `run_recovered` (`by: supervisor`), and `auto_repaired` (`repair:
    /// landing_released`) says what follows from its history
    /// ([`RunHistory::recovered_landing`]): an approved or passed run waits
    /// to land again (`landing_queued`, `via: recover`,
    /// [`Self::start_approved_landings`]), any other is reviewed
    /// ([`Self::review_recovered_runs`]). Whether its landing reached
    /// `main` already is the Integrator's to find when it lands it again.
    fn release_dead_landing(&mut self, run: &TaskRun, now: i64) -> Result<()> {
        let processes = self.queue.processes(run.id())?;
        let lease = match self.queue.run_lease(run.id())? {
            None => None,
            Some(lease) => {
                if !self.lease_stale(&lease, now) || self.processes.alive(lease.pid) {
                    return Ok(());
                }
                Some(lease_health(&lease, now, &*self.processes))
            }
        };
        let leased = lease.as_ref().map(|lease| lease.pid);
        let health = run_health(run, &processes, lease, now, &*self.processes, &*self.files);
        if !health.recoverable {
            return Ok(());
        }
        // Children of the dead landing (its verification) outlive it.
        let Some(worktree) = run.worktree_path().map(Path::new) else {
            return Ok(());
        };
        let listed = self.processes.list().map(|all| {
            run_processes(&all, worktree, None, None, std::process::id())
                .into_iter()
                .cloned()
                .collect::<Vec<_>>()
        });
        let working = match listed {
            Ok(working) => working,
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {} is integrating under a dead supervisor, but the processes in its worktree could not be listed: {error:#}; it is left integrating for a later pass", run.id());
                self.landing_unlisted(run, now, &error)?;
                return Ok(());
            }
        };
        let stopped = if working.is_empty() {
            None
        } else {
            match self.stop_landing_processes(run, worktree, &working, now)? {
                Some(stopped) => Some(stopped),
                None => return Ok(()),
            }
        };
        let report = json!({"run": health, "by": "supervisor"});
        let recovered = match self.queue.recover_run(run.id(), processes.len(), report) {
            Ok(recovered) => recovered,
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {} could not be recovered: {error:#}", run.id());
                return Ok(());
            }
        };
        let events = self.queue.run_events(run.id())?;
        let history = RunHistory::from_events(&events);
        // Another supervisor released it first: its records stand.
        if history
            .last(event_kind::RUN_RECOVERED)
            .is_none_or(|e| e.payload["previous_status"] != RunStatus::Integrating.as_str())
        {
            return Ok(());
        }
        let then = match history.recovered_landing() {
            Some(RecoveredLanding::Land(_)) => "land",
            _ => "review",
        };
        let noted = self
            .queue
            .record_runtime_event(
                run.id(),
                EventKind::AutoRepaired,
                json!({
                    "layer": "runtime",
                    "repair": "landing_released",
                    "conditions": {
                        "lease": if leased.is_some() { "stale" } else { "none" },
                        "supervisor_pid": leased,
                        "worktree_processes": 0,
                        "stopped_processes": stopped.unwrap_or(0),
                        "review_passed": history.review_lets_land(),
                        "approved": history.approved(),
                    },
                    "detail": {"then": then, "status": recovered.status().as_str()},
                }),
            )
            .and_then(|_| {
                if then == "land" {
                    self.queue.record_runtime_event(
                        run.id(),
                        EventKind::LandingQueued,
                        json!({"via": "recover"}),
                    )?;
                }
                Ok(())
            });
        if let Err(error) = noted {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: its release could not be recorded: {error:#}", run.id());
        }
        info!(run_id = %run.id(), task_id = %run.task_id(), "run {} of task {} gave back the integration slot: its landing's supervisor is gone; it goes to {then} again", run.id(), run.task_id());
        Ok(())
    }
    /// The processes `working` left by the dead landing of `run` in its
    /// worktree (task 1129): the first pass that finds them records
    /// `landing_release_waiting`, from whose `seen_at` the grace
    /// ([`landing_release::GRACE_SECS`]) counts across supervisors; past
    /// it they are stopped by pid (SIGTERM, then SIGKILL after
    /// [`super::recovery::STOP_GRACE`]) and `auto_repaired` (`repair:
    /// landing_processes_stopped`) is recorded. Returns how many were
    /// stopped once none is left, `None` while the run waits: within the
    /// grace, or when one outlived its SIGKILL (`landing_release_stuck`,
    /// the inbox's attention, recorded once for the landing), or a new one
    /// started meanwhile.
    fn stop_landing_processes(
        &mut self,
        run: &TaskRun,
        worktree: &Path,
        working: &[ProcessInfo],
        now: i64,
    ) -> Result<Option<usize>> {
        let pids: Vec<u32> = working.iter().map(|p| p.pid).collect();
        let events = self.queue.run_events(run.id())?;
        let Some(since) = landing_release::waiting_since(&events) else {
            self.note_landing(
                run,
                EventKind::LandingReleaseWaiting,
                json!({"pids": pids, "seen_at": now, "grace_secs": landing_release::GRACE_SECS}),
            );
            info!(run_id = %run.id(), "run {} is integrating under a dead supervisor, but processes {pids:?} still work in its worktree; it is released once they end, or they are stopped after {}s", run.id(), landing_release::GRACE_SECS);
            return Ok(None);
        };
        let waited = now - since;
        if waited < landing_release::GRACE_SECS {
            info!(run_id = %run.id(), "run {} is integrating under a dead supervisor, but processes {pids:?} still work in its worktree; waited {waited}s of {}s before they are stopped", run.id(), landing_release::GRACE_SECS);
            return Ok(None);
        }
        // A process that outlived its SIGKILL is a person's now.
        if landing_release::stuck(&events)
            .is_some_and(|e| e.payload["cause"] == landing_release::SURVIVED_STOP)
        {
            return Ok(None);
        }
        for pid in &pids {
            if let Err(error) = self.processes.terminate(*pid) {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: SIGTERM to pid {pid} failed: {error:#}", run.id());
            }
        }
        let alive = |sv: &Self| -> Vec<u32> {
            pids.iter()
                .copied()
                .filter(|pid| {
                    sv.processes.reap(*pid);
                    sv.processes.alive(*pid)
                })
                .collect()
        };
        let started = Instant::now();
        while started.elapsed() < super::recovery::STOP_GRACE && !alive(self).is_empty() {
            thread::sleep(Duration::from_millis(50));
        }
        let killed = alive(self);
        for pid in &killed {
            if let Err(error) = self.processes.kill(*pid) {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: SIGKILL to pid {pid} failed: {error:#}", run.id());
            }
        }
        let started = Instant::now();
        while started.elapsed() < super::recovery::STOP_GRACE && !alive(self).is_empty() {
            thread::sleep(Duration::from_millis(50));
        }
        let left = alive(self);
        if !left.is_empty() {
            let processes: Vec<&ProcessInfo> =
                working.iter().filter(|p| left.contains(&p.pid)).collect();
            self.note_landing(
                run,
                EventKind::LandingReleaseStuck,
                json!({
                    "cause": landing_release::SURVIVED_STOP,
                    "pids": left,
                    "processes": processes,
                    "waited_secs": waited,
                    "error": format!("processes {left:?} in the worktree of the dead landing outlived SIGKILL; stop them by pid"),
                }),
            );
            warn!(run_id = %run.id(), "run {} is integrating under a dead supervisor, and processes {left:?} in its worktree outlived SIGKILL; the inbox is told", run.id());
            return Ok(None);
        }
        self.note_landing(
            run,
            EventKind::AutoRepaired,
            json!({
                "layer": "runtime",
                "repair": "landing_processes_stopped",
                "conditions": {
                    "pids": pids,
                    "waited_secs": waited,
                    "grace_secs": landing_release::GRACE_SECS,
                },
                "detail": {
                    "stopped": working.iter().map(|p| json!({
                        "pid": p.pid,
                        "ppid": p.ppid,
                        "command": p.command,
                        "cwd": p.cwd,
                        "killed": killed.contains(&p.pid),
                    })).collect::<Vec<_>>(),
                },
            }),
        );
        info!(run_id = %run.id(), "run {}: stopped processes {pids:?} the dead landing left in its worktree after {waited}s", run.id());
        // One that started meanwhile waits for the next pass.
        let again = self
            .processes
            .list()
            .map(|all| !run_processes(&all, worktree, None, None, std::process::id()).is_empty());
        Ok((!matches!(again, Ok(true) | Err(_))).then_some(pids.len()))
    }
    /// The processes of the worktree of `run`'s dead landing could not be
    /// listed (`error`): the first time records `landing_release_waiting`
    /// without `pids`, and past [`landing_release::UNLISTED_SECS`] from it
    /// `landing_release_stuck` (`cause: unlisted`) is the inbox's, once.
    fn landing_unlisted(&mut self, run: &TaskRun, now: i64, error: &anyhow::Error) -> Result<()> {
        let events = self.queue.run_events(run.id())?;
        match landing_release::waiting_since(&events) {
            None => self.note_landing(
                run,
                EventKind::LandingReleaseWaiting,
                json!({"pids": null, "seen_at": now, "error": format!("{error:#}")}),
            ),
            Some(since)
                if now - since >= landing_release::UNLISTED_SECS
                    && landing_release::stuck(&events).is_none() =>
            {
                self.note_landing(
                    run,
                    EventKind::LandingReleaseStuck,
                    json!({
                        "cause": landing_release::UNLISTED,
                        "pids": [],
                        "waited_secs": now - since,
                        "error": format!("the processes in the worktree of the dead landing could not be listed: {error:#}"),
                    }),
                )
            }
            Some(_) => {}
        }
        Ok(())
    }
    /// Record `payload` as `kind` on `run`; a failure is only logged, as
    /// the release goes on from the run's state at the next pass.
    fn note_landing(&mut self, run: &TaskRun, kind: EventKind, payload: Value) {
        if let Err(error) = self.queue.record_runtime_event(run.id(), kind, payload) {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: {kind} could not be recorded: {error:#}", run.id());
        }
    }
    /// Apply the answered `decide` asks of the recovery job (an option the
    /// ask offered) to their `failed` / `interrupted` run nobody leases:
    /// `retry` readies the task, `resume` parks the run as `needs_session`
    /// with the round's reason, `cancel` cancels the task, and any other
    /// option (the job's own) goes back to the job: the run is taken by
    /// another round that reads the answer (`triage_decided`'s `action:
    /// recover`, within the alert's [`MAX_RECOVERY_ATTEMPTS`]). The ask is
    /// closed with it. An ask whose task is no longer in progress, or has a
    /// newer run, has nothing left to apply and is closed. A free answer is
    /// a person's to read.
    pub(super) fn apply_triage_answers(&mut self) -> Result<()> {
        for ask in self.queue.triage_answers()? {
            let Some(run_id) = ask.run_id.clone() else {
                continue;
            };
            let answer = ask.answer.as_deref().unwrap_or_default().trim().to_owned();
            let run = self.queue.run(&run_id)?;
            // Only an option the ask offered: a run whose resumes are used
            // up is not offered `resume`.
            if !ask.options.contains(&answer)
                || !matches!(run.status(), RunStatus::Failed | RunStatus::Interrupted)
                || self.queue.run_lease(&run_id)?.is_some()
            {
                continue;
            }
            let detail = self.queue.show(run.task_id())?;
            if detail.task.status() != TaskStatus::InProgress
                || detail
                    .runs
                    .last()
                    .is_some_and(|latest| *latest.id() != *run.id())
            {
                info!(ask_id = %ask.id, run_id = %run.id(), task_id = %run.task_id(), "ask {} of run {} is closed: task {} moved on without it", ask.id, run.id(), run.task_id());
                self.queue.close_ask(ask.id)?;
                continue;
            }
            let events = self.queue.run_events(run.id())?;
            let reason = RunHistory::from_events(&events)
                .last(event_kind::TRIAGE_FINISHED)
                .and_then(|e| e.payload.get("reason").and_then(Value::as_str))
                .map_or_else(
                    || run.last_error().map(str::to_owned).unwrap_or_default(),
                    str::to_owned,
                );
            let reason = format!("{reason} (a person chose {answer} in ask {})", ask.id);
            match self.queue.decide_triage(run.id(), ask.id, &answer, &reason) {
                Ok(decided) => {
                    info!(run_id = %decided.id(), task_id = %decided.task_id(), ask_id = %ask.id, "run {} of task {}: {answer} as ask {} answered; the run is {}", decided.id(), decided.task_id(), ask.id, decided.status().as_str());
                    self.clean_task_worktrees(decided.task_id());
                }
                Err(error) => {
                    warn!(run_id = %run.id(), ask_id = %ask.id, error = %format_args!("{error:#}"), "run {}: the answer {answer:?} of ask {} could not be applied: {error:#}", run.id(), ask.id)
                }
            }
        }
        Ok(())
    }
    /// The `failed` / `interrupted` runs whose recovery job is due: no
    /// round took them since their last resume, or their round's `wait` is
    /// over (ADR-0047 decision 39), in the order the queue lists them. A
    /// run someone leases (a session still asked to exit) waits, and none
    /// is due while no provider can run the job ([`Self::recovery_route`]
    /// waits): a login or usage limit that holds the queue holds a role
    /// that names no provider (task 437), while one whose role names its
    /// provider may start on Codex meanwhile (ADR-t1063-1 decision 5).
    pub(super) fn triage_candidates(&mut self) -> Result<Vec<TaskRun>> {
        if self.recovery_route().is_none() {
            return Ok(Vec::new());
        }
        let now = self.generators.clock.now();
        let mut due_runs = Vec::new();
        for run in self.queue.runs_to_triage()? {
            let events = self.queue.run_events(run.id())?;
            let due = match triage_state(&events) {
                TriageState::Pending => true,
                TriageState::Waiting { until } => until <= now,
                TriageState::Failed | TriageState::Finished => false,
            };
            if !due
                || self.in_slot(run.id())
                || self
                    .queue
                    .run_lease(run.id())?
                    .is_some_and(|lease| !self.lease_stale(&lease, now))
            {
                continue;
            }
            due_runs.push(run);
        }
        Ok(due_runs)
    }
    /// Start the recovery job of `run`, one of [`Self::triage_candidates`],
    /// in a free slot: the fill pass calls it in the order of its line
    /// (ADR-t1850-1). The alert is that of the run's latest
    /// `recovery_requested` since its last resume (`resume_exhausted`, or
    /// the alert a `wait` or a stopped round left), else its status; a
    /// round records its own request unless one is pending. An alert that
    /// got its [`MAX_RECOVERY_ATTEMPTS`] jobs is escalated without one (but
    /// for a person's verify fix after an edit, [`verify_fix_round`]), and a
    /// job that cannot even start fails its round right away. The job starts
    /// on the provider [`Self::recovery_route`] says, which is recorded as
    /// the `launch` of `triage_started` the job is started from; under
    /// `--no-claude` with no provider for it, the round fails to a person
    /// told why.
    pub(super) fn triage_run(&mut self, run: TaskRun) -> Result<()> {
        let events = self.queue.run_events(run.id())?;
        let pending = pending_request(&events);
        let alert = current_alert(&events).unwrap_or_else(|| RecoveryAlert::of_ended(run.status()));
        let done = attempts(&events, alert);
        // A person's verify-fix answer after the task's verification
        // was edited gets its round past the limit (ADR-t883-1).
        let granted = pending.is_none()
            && done >= MAX_RECOVERY_ATTEMPTS
            && verify_fix_round(&events, &self.queue.show(run.task_id())?.events);
        let (used_up, attempt) = recovery_round(pending.is_some(), done, granted);
        let request = match pending {
            Some(_) => None,
            None if used_up => None,
            None => {
                let evidence: Vec<EventId> = events
                    .iter()
                    .rev()
                    .find(|e| e.payload["status"] == run.status().as_str())
                    .map(|e| e.id)
                    .into_iter()
                    .collect();
                let mut request = json!({
                    "alert": alert,
                    "attempt": done + 1,
                    "status": run.status().as_str(),
                    "evidence": evidence,
                    "last_error": run.last_error(),
                });
                // A person chose one of the last job's own options.
                if let Some(decided) = events
                    .iter()
                    .rev()
                    .take_while(|e| e.kind != event_kind::TRIAGE_STARTED)
                    .find(|e| {
                        e.kind == event_kind::TRIAGE_DECIDED
                            && e.payload["action"] == crate::domain::RECOVER_AGAIN
                    })
                {
                    request["person_answer"] = json!({
                        "ask_id": decided.payload["ask_id"],
                        "answer": decided.payload["answer"],
                    });
                }
                Some(request)
            }
        };
        // Not while the cleanup job is to clear the run's worktree (task
        // 405).
        let cleaning = self.cleanup.cleaning();
        let mut guard = cleanup::lock_cleaning(&cleaning);
        if !guard.may_lease(run.id()) {
            self.cleanup.deferred = true;
            return Ok(());
        }
        // The route as it reads now: a job reaped earlier in this pass may
        // have held a provider.
        let Some(route) = self.recovery_route() else {
            return Ok(());
        };
        let (launch, switchable, unavailable) = match route {
            JobStartRoute::Start(launch, switchable) => (launch, switchable, None),
            JobStartRoute::Unavailable(launch, why) => (launch, false, Some(why)),
        };
        let begun = self
            .queue
            .begin_triage(run.id(), &self.token, request, &launch)?;
        drop(guard);
        let Some((run, round)) = begun else {
            return Ok(());
        };
        if used_up {
            let used = attempt - 1;
            let end = JobEnd::default();
            if let Err(error) =
                self.escalate_ended(&run, round, alert, used, Escalation::UsedUp(used), 0, &end)
            {
                self.fail_recovery(&run, round, alert, used, format!("{error:#}"), 0, &end);
            }
            let run = self.queue.run(run.id())?;
            self.note_triaged(&run);
            return Ok(());
        }
        if let Some(why) = unavailable {
            let error = format!("the recovery job could not start: {why}");
            self.fail_recovery(&run, round, alert, attempt, error, 0, &JobEnd::default());
            let run = self.queue.run(run.id())?;
            self.note_triaged(&run);
            return Ok(());
        }
        // The outer error is the job's own preparation, the inner one the
        // start of its provider's process.
        let started = self
            .spawn_ended(&run, round, alert, attempt, switchable)
            .map_err(|error| (error, false))
            .and_then(|started| started.map_err(|error| (error, true)));
        match started {
            Ok(watch) => {
                info!(run_id = %run.id(), task_id = %run.task_id(), "run {} of task {} ({}) recovery job {attempt} for {} started on {} (round {round})", run.id(), run.task_id(), run.status().as_str(), alert.as_str(), watch.job.provider.as_str());
                self.slots.push(Slot::new(run, Phase::Recovery(watch)));
            }
            Err((failed, spawned)) => {
                let error = format!("the recovery job could not start: {failed:#}");
                let unusable = spawned
                    .then(|| {
                        let launch = self.started_launch(&run);
                        self.recovery_start_failed(run.id(), &launch, &failed, &error, switchable)
                    })
                    .flatten();
                let end = JobEnd {
                    session: None,
                    unusable,
                };
                self.fail_recovery(&run, round, alert, attempt, error, 0, &end);
                let run = self.queue.run(run.id())?;
                self.note_triaged(&run);
            }
        }
        Ok(())
    }
    /// Write the recovery job's prompt for a run that ended and start the
    /// headless job in the run's directory, allowed to read only
    /// (ADR-0047 decision 39: the task, the run's error, receipt, logs,
    /// final screen and events, the processes left in its worktree, its
    /// git state and earlier repairs).
    /// The outer error is the job's own preparation, the inner one the
    /// start of its provider's process ([`super::recovery::start_job`]).
    fn spawn_ended(
        &mut self,
        run: &TaskRun,
        round: usize,
        alert: RecoveryAlert,
        attempt: usize,
        switchable: bool,
    ) -> Result<Result<EndedRecovery>> {
        let dir = match &run.run_dir() {
            Some(dir) => PathBuf::from(dir),
            None => self.layout.runs_dir.join(run.id().as_str()),
        };
        let detail = self.queue.show(run.task_id())?;
        let events = self.queue.run_events(run.id())?;
        let resumes = ResumeCount::of(&events);
        let ended = ended_run_material(
            &*self.files,
            &detail,
            run,
            resumes,
            self.resume_config,
            &dir,
        );
        let facts = events
            .iter()
            .rev()
            .find(|e| e.kind == event_kind::RECOVERY_REQUESTED)
            .map_or_else(
                || json!({"alert": alert}),
                |e| {
                    // What a live recovery job was started with is not a
                    // fact of the run (ADR-0079 decision 7).
                    let mut facts = e.payload.clone();
                    if let Some(facts) = facts.as_object_mut() {
                        facts.remove("launch");
                    }
                    facts
                },
            );
        let processes = match run.worktree_path().map(Path::new) {
            Some(worktree) if self.files.is_dir(worktree) => self
                .processes
                .list()
                .map(|all| {
                    run_processes(&all, worktree, None, None, std::process::id())
                        .into_iter()
                        .cloned()
                        .collect()
                })
                .map_err(|error| format!("{error:#}")),
            _ => Err("the run has no worktree".to_owned()),
        };
        let (status, head, receipt) = git_facts(self, run)?;
        let mut history = repair_history(self, run)?;
        history.extend(
            detail
                .events
                .iter()
                .filter(|event| event.kind == event_kind::TASK_EDITED)
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()?,
        );
        let workspace = run.workspace_id().unwrap_or("none").to_owned();
        let binary = super::recovery::binary_facts_of(self, run, &detail)?;
        let material = RecoveryMaterial {
            alert,
            ended: Some(ended),
            facts: &facts,
            workspace: &workspace,
            screen: { "(the session is gone: its turns are above)" },
            processes,
            git_status: &status,
            head: &head,
            receipt_commit: receipt.as_deref(),
            history: &history,
            allowed: &ENDED_ACTIONS,
            binary: &binary,
        };
        let prompt = recovery_prompt(&detail.task, run, attempt, &material)?
            .with_language(self.verifier.language().as_ref());
        // The session id `triage_started` recorded (ADR-0048 decision 4;
        // none for Codex, which names its thread itself), and the provider,
        // model and effort (ADR-0079 decision 7, ADR-t1063-1).
        let session_id = events
            .iter()
            .rev()
            .find(|e| e.kind == event_kind::TRIAGE_STARTED)
            .and_then(|e| e.payload["session_id"].as_str())
            .map(str::to_owned);
        let launch = launch_of(&events);
        let job = start_job(
            self,
            run.id(),
            &dir,
            (alert, attempt),
            (&prompt, &binary),
            session_id.as_deref(),
            &launch,
        )?;
        Ok(job.map(|job| EndedRecovery {
            round,
            alert,
            attempt,
            switchable,
            job,
        }))
    }

    /// The launch the latest `triage_started` of `run` recorded.
    fn started_launch(&self, run: &TaskRun) -> ActorLaunch {
        self.queue.run_events(run.id()).map_or_else(
            |_| ActorLaunch::default_of(ModelRole::Recovery),
            |events| launch_of(&events),
        )
    }
    /// Act on the recovery job's verdict for a run that ended (ADR-0047
    /// decision 40): a `repair` of high confidence whose one action's
    /// preconditions hold now is applied and recorded as `auto_repaired`
    /// (`layer: recovery`, but for `wait`), `triage_finished` (its action)
    /// and `recovery_finished`; anything else becomes the `decide` ask.
    /// Then the workspaces the run left open are closed and the lease is
    /// released.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn act_on_recovery(
        &mut self,
        run: &TaskRun,
        round: usize,
        alert: RecoveryAlert,
        attempt: usize,
        duration_secs: u64,
        verdict: RecoveryVerdict,
        end: &JobEnd,
    ) -> Result<TaskRun> {
        ensure!(
            self.queue.holds_lease(run.id(), &self.token)?,
            "the recovery job's lease of run {} was lost",
            run.id()
        );
        if !verdict.applies() {
            return self.escalate_ended(
                run,
                round,
                alert,
                attempt,
                Escalation::Verdict(verdict),
                duration_secs,
                end,
            );
        }
        let (action, conditions) = match self.plan_ended(run, &verdict) {
            Ok(planned) => planned,
            Err(why) => {
                warn!(run_id = %run.id(), "run {}: recovery job {attempt} of {} answered repair, but {why}; asking the inbox", run.id(), alert.as_str());
                return self.escalate_ended(
                    run,
                    round,
                    alert,
                    attempt,
                    Escalation::Refused(verdict, why),
                    duration_secs,
                    end,
                );
            }
        };
        let name = verdict.actions[0].name();
        let mut payload = json!({
            "attempt": round,
            "alert": alert,
            "recovery_attempt": attempt,
            "verdict": verdict.verdict,
            "confidence": verdict.confidence,
            "reason": verdict.diagnosis,
            "duration_secs": duration_secs,
        });
        end.record(&mut payload);
        let mut also = Vec::new();
        if !matches!(action, TriageAction::Wait { .. }) {
            also.push((
                EventKind::AutoRepaired,
                json!({
                    "layer": "recovery",
                    "repair": name,
                    "alert": alert,
                    "attempt": attempt,
                    "conditions": conditions,
                }),
            ));
        }
        let mut finished = json!({
            "alert": alert,
            "attempt": attempt,
            "verdict": verdict.verdict,
            "confidence": verdict.confidence,
            "diagnosis": verdict.diagnosis,
            "applied": [name],
            "escalated": false,
            "recheck_at": match &action {
                TriageAction::Wait { recheck_at } => Some(*recheck_at),
                _ => None,
            },
            "duration_secs": duration_secs,
        });
        end.record(&mut finished);
        also.push((EventKind::RecoveryFinished, finished));
        let finished = self
            .queue
            .finish_triage(run.id(), &self.token, &action, payload, also)?;
        info!(run_id = %run.id(), "run {}: recovery job {attempt} of {} repaired it ({name}): {}; the run is {}", run.id(), alert.as_str(), verdict.diagnosis, finished.status().as_str());
        self.end_round(&finished)
    }
    /// The action of a `repair` for a run that ended, once its
    /// preconditions hold now (ADR-0047 decision 40), with the values
    /// checked; `Err` says which one does not. Exactly one of
    /// [`ENDED_ACTIONS`]: `retry` only for a run whose branch holds no
    /// commit of its own and a task that did not fail
    /// [`TRIAGE_RETRY_FAILURES`] times, `retry_inherit` only for one with
    /// commits and once per task, `resume` only with resumes left and a
    /// worktree, `wait` for at most [`MAX_RECHECK_SECS`].
    fn plan_ended(
        &mut self,
        run: &TaskRun,
        verdict: &RecoveryVerdict,
    ) -> std::result::Result<(TriageAction, Value), String> {
        let [action] = verdict.actions.as_slice() else {
            return Err(format!(
                "a run that ended takes exactly one action of {}, not {}",
                ENDED_ACTIONS.join(", "),
                verdict
                    .actions
                    .iter()
                    .map(RecoveryAction::name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        };
        let unreadable = |error: anyhow::Error| format!("{error:#}");
        match action {
            RecoveryAction::Retry => {
                let failures = self
                    .queue
                    .show(run.task_id())
                    .map_err(unreadable)?
                    .runs
                    .iter()
                    .filter(|r| matches!(r.status(), RunStatus::Failed | RunStatus::Interrupted))
                    .count();
                if failures >= TRIAGE_RETRY_FAILURES {
                    return Err(format!(
                        "task {} has {failures} failed or interrupted runs, so it is not retried without a person",
                        run.task_id()
                    ));
                }
                let own = self.own_commits(run).map_err(|error| {
                    format!("whether its branch holds commits could not be read: {error:#}")
                })?;
                if own {
                    return Err("its branch holds commits of its own, which a retry from scratch would throw away (retry_inherit carries them over; throwing them away is a person's call)".to_owned());
                }
                Ok((
                    TriageAction::Retry,
                    json!({"failures": failures, "own_commits": false}),
                ))
            }
            RecoveryAction::RetryInherit => {
                let detail = self.queue.show(run.task_id()).map_err(unreadable)?;
                if detail
                    .events
                    .iter()
                    .any(crate::domain::resume::uses_automatic_inherit)
                {
                    return Err(format!(
                        "task {} was retried with a branch carried over already (once per task)",
                        run.task_id()
                    ));
                }
                let own = self.own_commits(run).map_err(|error| {
                    format!("whether its branch holds commits could not be read: {error:#}")
                })?;
                let head = match self.inherited_head(run) {
                    Ok(Some(head)) if own => head,
                    Ok(_) => {
                        return Err(
                            "its branch holds no commit of its own to carry over".to_owned()
                        );
                    }
                    Err(error) => {
                        return Err(format!(
                            "its branch could not be kept for a retry: {error:#}"
                        ));
                    }
                };
                let branch = run.branch().map(str::to_owned);
                Ok((
                    TriageAction::RetryInherit {
                        branch: branch.clone(),
                        head: head.clone(),
                    },
                    json!({"own_commits": true, "branch": branch, "head": head, "first_in_task": true}),
                ))
            }
            RecoveryAction::Resume { instruction } => {
                let resumes =
                    ResumeCount::of(&self.queue.run_events(run.id()).map_err(unreadable)?);
                if resumes.exhausted(self.resume_config) {
                    return Err(format!(
                        "its resumes are used up ({} counted of at most {MAX_RESUME_ATTEMPTS}, {} after conflicts only, {} of at most {KILL_ONLY_RESUME_LIMIT} after its session was killed)",
                        resumes.counted,
                        resumes.conflict_attempts(),
                        resumes.kill_only
                    ));
                }
                let worktree = run
                    .worktree_path()
                    .is_some_and(|path| self.files.is_dir(Path::new(path)));
                if !worktree || run.receipt_path().is_none() {
                    return Err("the run has no worktree a session could resume in".to_owned());
                }
                let instruction = if instruction.trim().is_empty() {
                    verdict.diagnosis.clone()
                } else {
                    instruction.trim().to_owned()
                };
                Ok((
                    TriageAction::Resume { instruction },
                    json!({"counted_resumes": resumes.counted, "kill_only_resumes": resumes.kill_only, "resumes": resumes.total(), "worktree": true}),
                ))
            }
            RecoveryAction::Wait { recheck_after_secs } => {
                let secs = (*recheck_after_secs).min(MAX_RECHECK_SECS);
                Ok((
                    TriageAction::Wait {
                        recheck_at: self.generators.clock.now() + i64::try_from(secs).unwrap_or(0),
                    },
                    json!({"recheck_after_secs": secs}),
                ))
            }
            other => Err(format!(
                "{} does not apply to a run that ended: its session is gone (allowed: {})",
                other.name(),
                ENDED_ACTIONS.join(", ")
            )),
        }
    }
    /// Whether the run's branch holds commits of its own: its reviewed
    /// commit, else its worktree's HEAD, else its branch, is not on main.
    /// A run that never got a worktree has none; one whose worktree is
    /// gone without a reviewed commit cannot be told.
    fn own_commits(&self, run: &TaskRun) -> Result<bool> {
        let worktree = run
            .worktree_path()
            .map(Path::new)
            .filter(|path| self.files.is_dir(path));
        let head = match (run.result_commit(), worktree) {
            (Some(commit), _) => commit.to_string(),
            (None, Some(worktree)) => self.repository.head(worktree)?.to_string(),
            (None, None)
                if !self
                    .queue
                    .has_run_event(run.id(), event_kind::WORKTREE_CREATED)? =>
            {
                return Ok(false);
            }
            (None, None) => bail!("its worktree is gone and it has no reviewed commit"),
        };
        let main = self.repository.main_head()?;
        Ok(!self.repository.is_ancestor(&head, main.as_str())?)
    }
    /// Escalate the alert of a run that ended to the inbox (ADR-0047
    /// decision 40): a `decide` ask with the job's diagnosis and why a
    /// person is needed, the options `retry` / `resume` / `cancel`
    /// (without `resume` once its resumes are used up) and the job's, and
    /// the job's reason category; recorded as `triage_finished` (action
    /// `ask`) and `recovery_finished`. The supervisor applies the answer
    /// ([`Self::apply_triage_answers`]).
    #[allow(clippy::too_many_arguments)]
    fn escalate_ended(
        &mut self,
        run: &TaskRun,
        round: usize,
        alert: RecoveryAlert,
        attempt: usize,
        escalation: Escalation,
        duration_secs: u64,
        end: &JobEnd,
    ) -> Result<TaskRun> {
        let note = escalation.note(run, alert, attempt);
        let exhausted =
            ResumeCount::of(&self.queue.run_events(run.id())?).exhausted(self.resume_config);
        // Only a run whose `integrate` verification failed is offered the
        // verify fix (ADR-t883-1).
        let verify_failed = verification_failed(&self.queue.run_events(run.id())?);
        let options = ended_options(&note, exhausted, verify_failed);
        let question = ended_question(run, alert, &note, exhausted, verify_failed);
        let outcome = ask::ask(
            &mut *self.queue,
            NewAsk {
                recommendation: None,
                confidence: None,
                kind: alert.ask_kind(),
                task_id: None,
                run_id: Some(run.id().clone()),
                question,
                options,
                asked_by: TRIAGE_ASKER.to_owned(),
                reason_category: note.category,
                topics: Vec::new(),
                finding_id: None,
                request_id: None,
            },
        )?;
        let ask_id = outcome["id"]
            .as_i64()
            .map(AskId::new)
            .context("ask returned no id")?;
        let verdict = escalation.verdict();
        let mut payload = json!({
            "attempt": round,
            "alert": alert,
            "recovery_attempt": attempt,
            "verdict": verdict.map(|v| v.verdict),
            "confidence": verdict.map(|v| v.confidence),
            "reason": match verdict {
                Some(verdict) => format!("{}: {}", note.why, verdict.diagnosis),
                None => note.why.clone(),
            },
            "duration_secs": duration_secs,
        });
        end.record(&mut payload);
        let mut finished = escalation.finished(
            alert,
            attempt,
            &note,
            Some(ask_id),
            json!({"duration_secs": duration_secs}),
        );
        end.record(&mut finished);
        let asked = match self.queue.finish_triage(
            run.id(),
            &self.token,
            &TriageAction::Ask { ask_id },
            payload,
            vec![(EventKind::RecoveryFinished, finished)],
        ) {
            Ok(asked) => asked,
            Err(error) => {
                // The round fails (`triage_failed`): an ask this pass opened
                // would ask a person twice, so it is withdrawn.
                if outcome["created"] == true {
                    self.queue.answer_as(
                        ask_id,
                        "withdrawn: the recovery round could not be recorded",
                        crate::domain::Answerer::RUNTIME,
                    )?;
                    self.queue.close_ask(ask_id)?;
                }
                return Err(error);
            }
        };
        warn!(ask_id = %ask_id, run_id = %run.id(), "run {}: {}; decide ask {ask_id} (notified: {})", run.id(), note.why, outcome["notified"]);
        self.end_round(&asked)
    }
    /// The end of a round whose outcome is recorded: the workspaces the run
    /// left open are closed and the lease is released. What fails here is
    /// logged, not a failed round.
    fn end_round(&mut self, run: &TaskRun) -> Result<TaskRun> {
        if let Err(error) = self.close_open_workspaces(run, WorkspaceCloser::Triage) {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: its workspaces could not all be closed: {error:#}", run.id());
        }
        if let Err(error) = self.queue.release_lease(run.id(), &self.token) {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: could not release the lease: {error:#}", run.id());
        }
        self.queue.run(run.id())
    }
    /// Record a round whose recovery job failed (it could not start, exited
    /// non-zero, timed out, printed no verdict, or its outcome could not be
    /// applied) as `triage_failed`, the attention a person recovers the run
    /// from by hand, and `recovery_finished` (`outcome: job_failed`), and
    /// give the lease back; the run stays as it is (ADR-0047 decision 40).
    /// Both record the job's `end` (its Codex thread and model, the
    /// provider it found unusable). A round whose provider could not be
    /// used (`provider_unusable`) is no person's: the run is due again and
    /// its next round starts on the other provider, or, when none can run
    /// it (`--no-claude`), fails to a person told why (ADR-t1063-1 decision
    /// 4, [`crate::domain::triage_state`]). Any other failure of a Codex job
    /// never moves to Claude.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn fail_recovery(
        &mut self,
        run: &TaskRun,
        round: usize,
        alert: RecoveryAlert,
        attempt: usize,
        error: String,
        duration_secs: u64,
        end: &JobEnd,
    ) {
        let then = match end.unusable {
            Some(_) => "its provider cannot be used, and the next round starts on the other one",
            None => "the run waits to be recovered by hand",
        };
        warn!(run_id = %run.id(), error = %error, "run {} recovery job {attempt} of {} failed: {error}; {then}", run.id(), alert.as_str());
        for (kind, payload) in [
            (
                EventKind::TriageFailed,
                json!({
                    "code": ReasonCode::JobFailed,
                    "attempt": round,
                    "alert": alert,
                    "recovery_attempt": attempt,
                    "error": error,
                    "duration_secs": duration_secs,
                    "status": run.status().as_str(),
                }),
            ),
            (
                EventKind::RecoveryFinished,
                json!({
                    "alert": alert,
                    "attempt": attempt,
                    "outcome": "job_failed",
                    "escalated": false,
                    "error": error,
                    "reason_category": AskReason::RecoveryFailed,
                }),
            ),
        ] {
            let mut payload = payload;
            end.record(&mut payload);
            if let Err(error) = self.queue.record_runtime_event(run.id(), kind, payload) {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: could not record the failed recovery job: {error:#}", run.id());
            }
        }
        if self
            .queue
            .holds_lease(run.id(), &self.token)
            .unwrap_or(false)
            && let Err(error) = self.queue.release_lease(run.id(), &self.token)
        {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: could not release the lease: {error:#}", run.id());
        }
    }
    pub(super) fn note_triaged(&mut self, run: &TaskRun) {
        self.clean_task_worktrees(run.task_id());
        let task = self
            .queue
            .show(run.task_id())
            .map(|detail| detail.task.status());
        info!(run_id = %run.id(), "run {} triaged: the run is {}{}", run.id(), run.status().as_str(), match task {
            Ok(status) => format!(", task {} is {}", run.task_id(), status.as_str()),
            Err(_) => String::new(),
        });
        self.triaged.push(json!({
            "run_id": run.id(),
            "task_id": run.task_id(),
            "status": run.status(),
        }));
    }
}

/// The launch the latest `triage_started` among `events` recorded, which
/// the round's job is started from; the default of the role for a round
/// recorded before launches were.
fn launch_of(events: &[RunEvent]) -> ActorLaunch {
    events
        .iter()
        .rev()
        .find(|e| e.kind == event_kind::TRIAGE_STARTED)
        .map_or_else(
            || ActorLaunch::default_of(ModelRole::Recovery),
            |e| ActorLaunch::recorded(&e.payload, ModelRole::Recovery),
        )
}

/// Whether a round of a run that ended finds its alert's jobs used up, and
/// the number of its job: a `pending` request is the job `done` counted
/// already; else the next one, used up past [`MAX_RECOVERY_ATTEMPTS`]
/// unless a person's verify fix was `granted` its round (ADR-t883-1).
fn recovery_round(pending: bool, done: usize, granted: bool) -> (bool, usize) {
    if pending {
        return (false, done);
    }
    (done >= MAX_RECOVERY_ATTEMPTS && !granted, done + 1)
}

/// The options of the `decide` ask of a run that ended: `retry` /
/// `resume` / `cancel` (without `resume` once its resumes are
/// `exhausted`), the job's, and the verify fix for a run whose `integrate`
/// verification failed.
fn ended_options(note: &Note, exhausted: bool, verify_failed: bool) -> Vec<String> {
    let base = if exhausted {
        EXHAUSTED_OPTIONS
    } else {
        TRIAGE_OPTIONS
    };
    let mut options = super::recovery::ask_options(base, Some(note));
    if verify_failed && !options.iter().any(|option| option == VERIFY_FIX_OPTION) {
        options.push(VERIFY_FIX_OPTION.to_owned());
    }
    options
}

/// The question of the `decide` ask of a run that ended: where the run
/// stands, the escalation's `note`, its last error and what each option
/// does.
fn ended_question(
    run: &TaskRun,
    alert: RecoveryAlert,
    note: &Note,
    exhausted: bool,
    verify_failed: bool,
) -> String {
    let verify_fix = if verify_failed {
        format!(
            " Its integrate verification failed: if the task's verify itself is wrong, user or inbox first runs `dagq edit {task_id} --verify ...` (or `--no-verify`), then answers `{VERIFY_FIX_OPTION}`; the recovery job runs again with the corrected commands, even once its tries are used up, and can carry the committed branch forward with retry_inherit. Answered without that edit, it goes back to the job like any other option.",
            task_id = run.task_id()
        )
    } else {
        String::new()
    };
    let resume = if exhausted {
        ""
    } else {
        " resume: resume the run's own session with the job's diagnosis."
    };
    let others = if note.options.is_empty() {
        ""
    } else {
        " Any other option goes back to the recovery job, which runs again with your choice."
    };
    format!(
        "The supervisor's recovery job for run {run_id} (task {task_id}, {status}; alert: {alert}) did not move it on: {why}.\n{text}\nLast error: {last_error}\nretry: make the task ready for a new run from scratch (this run's work is not carried over).{resume} cancel: cancel the task.{verify_fix}{others}",
        run_id = run.id(),
        task_id = run.task_id(),
        status = run.status().as_str(),
        alert = alert.as_str(),
        why = note.why,
        text = note.text,
        last_error = or_none(tail(run.last_error().unwrap_or_default(), 500)),
    )
}

#[cfg(test)]
mod tests {
    use super::super::recovery::{Escalation, test_run};
    use super::*;

    /// A round takes the pending request's job, else the next; past the
    /// alert's jobs it is used up, unless a person's verify fix was granted
    /// its round (moved from `runtime_triage`, task 1415; when the round is
    /// granted is `verify_fix_round`'s).
    #[test]
    fn a_round_past_the_alerts_jobs_is_used_up_unless_granted() {
        assert_eq!(recovery_round(false, 0, false), (false, 1));
        assert_eq!(recovery_round(false, 2, false), (false, 3));
        assert_eq!(
            recovery_round(false, MAX_RECOVERY_ATTEMPTS, false),
            (true, 4)
        );
        assert_eq!(
            recovery_round(false, MAX_RECOVERY_ATTEMPTS, true),
            (false, 4)
        );
        assert_eq!(
            recovery_round(true, MAX_RECOVERY_ATTEMPTS, false),
            (false, 3)
        );
        assert_eq!(recovery_round(true, 1, false), (false, 1));
    }

    fn note_of(job: Option<serde_json::Value>) -> Note {
        let run = test_run(RunStatus::Failed, None);
        match job {
            Some(json) => Escalation::Verdict(RecoveryVerdict::parse(&json.to_string()).unwrap()),
            None => Escalation::UsedUp(MAX_RECOVERY_ATTEMPTS),
        }
        .note(&run, RecoveryAlert::Failed, 1)
    }

    /// The `decide` ask of a run that ended offers `retry`, `resume` and
    /// `cancel` (no `resume` once its resumes are used up), then the job's
    /// options, then the verify fix for a run whose `integrate`
    /// verification failed, even once its jobs are used up (moved from
    /// `runtime_triage`, task 1415).
    #[test]
    fn a_decide_ask_offers_the_triage_options_the_jobs_and_the_verify_fix() {
        let jobs = note_of(Some(json!({
            "verdict": "escalate", "confidence": "high", "diagnosis": "x",
            "options": ["retry anyway", "cancel"],
        })));
        assert_eq!(
            ended_options(&jobs, false, false),
            ["retry", "resume", "cancel", "retry anyway"]
        );
        assert_eq!(
            ended_options(&jobs, true, false),
            ["retry", "cancel", "retry anyway"]
        );
        assert_eq!(
            ended_options(&jobs, false, true),
            [
                "retry",
                "resume",
                "cancel",
                "retry anyway",
                VERIFY_FIX_OPTION
            ]
        );
        let used_up = note_of(None);
        assert_eq!(
            ended_options(&used_up, false, true),
            ["retry", "resume", "cancel", VERIFY_FIX_OPTION]
        );
        // The job's own verify fix is not offered twice.
        let fix = note_of(Some(json!({
            "verdict": "escalate", "confidence": "high", "diagnosis": "x",
            "options": [VERIFY_FIX_OPTION],
        })));
        assert_eq!(
            ended_options(&fix, false, true),
            ["retry", "resume", "cancel", VERIFY_FIX_OPTION]
        );
    }

    /// The `decide` ask's question names the run and its alert, why a
    /// person is asked with the job's note, the last error, what each
    /// option does, the verify fix's `dagq edit` only after a failed
    /// verification, and the hand-back only when the job gave options
    /// (moved from `runtime_triage`, task 1415).
    #[test]
    fn a_decide_ask_tells_what_each_option_does() {
        let run = test_run(RunStatus::Failed, Some("session exited with code 7"));
        let jobs = note_of(Some(json!({
            "verdict": "escalate", "confidence": "high", "diagnosis": "flaky",
            "options": ["retry anyway"], "reason_category": "discard",
        })));
        let question = ended_question(&run, RecoveryAlert::Failed, &jobs, false, false);
        for part in [
            "recovery job for run r1 (task 1, failed; alert: failed) did not move it on: the recovery job could not repair it.",
            "Why a person: discard",
            "Diagnosis: flaky",
            "recovery-failed-1.prompt.txt",
            "Last error: session exited with code 7",
            "retry: make the task ready for a new run from scratch",
            " resume: resume the run's own session",
            " cancel: cancel the task.",
            "Any other option goes back to the recovery job",
        ] {
            assert!(question.contains(part), "{part}: {question}");
        }
        assert!(!question.contains("dagq edit"), "{question}");

        let used_up = note_of(None);
        let question = ended_question(&run, RecoveryAlert::Failed, &used_up, true, true);
        for part in [
            "the recovery job ran 3 times for this alert already (at most 3)",
            "dagq edit 1 --verify",
            VERIFY_FIX_OPTION,
        ] {
            assert!(question.contains(part), "{part}: {question}");
        }
        for absent in [" resume: ", "Any other option"] {
            assert!(!question.contains(absent), "{absent}: {question}");
        }
        let quiet = test_run(RunStatus::Interrupted, None);
        let question = ended_question(&quiet, RecoveryAlert::Interrupted, &used_up, false, false);
        assert!(question.contains("Last error: (none)"), "{question}");
    }
}
