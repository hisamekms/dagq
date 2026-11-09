//! The supervisor's telemetry, tokens and sweeps: issuing a run's
//! telemetry token at its start ([`Supervisor::issue_run_telemetry`]) and
//! registering its sessions and jobs with the telemetry, the periodic
//! sweep of the held revocations and the wrappers' kept steps
//! ([`Supervisor::sweep_telemetry`], whose reads of a run are
//! [`kept_run_of`]), the sweeps of the ended runs' worktrees and of the
//! verification and worker tokens left behind
//! ([`Supervisor::sweep_ended_runs`],
//! [`Supervisor::sweep_verification_tokens`],
//! [`Supervisor::sweep_worker_tokens`]), and the cleanup's requests to
//! host運用 ([`Supervisor::request_cleanup`], [`Supervisor::resume_cleanup`]).
//! The loop that calls them and the state they read stay in
//! `execution.rs`; this module only adds methods to `Supervisor`, whose
//! fields it sees as a submodule of `execution` without widening them
//! (rule C3 of docs/design/architecture.md).

use super::*;

/// The reads of the queue [`kept_run_of`] makes.
trait KeptRunReads {
    fn run(&self, id: &RunId) -> Result<TaskRun>;
    fn latest_run_of(&self, task: TaskId) -> Result<Option<TaskRun>>;
    fn list(&self, query: &crate::application::TaskQuery) -> Result<crate::application::TaskPage>;
}

impl KeptRunReads for dyn Queue + Send {
    fn run(&self, id: &RunId) -> Result<TaskRun> {
        crate::application::RunReads::run(self, id)
    }
    fn latest_run_of(&self, task: TaskId) -> Result<Option<TaskRun>> {
        crate::application::RunTransitions::latest_run_of(self, task)
    }
    fn list(&self, query: &crate::application::TaskQuery) -> Result<crate::application::TaskPage> {
        crate::application::TaskReads::list(self, query)
    }
}

/// What the sweep of the wrappers' kept steps reads of `run`
/// ([`crate::domain::telemetry::KeptRun::of`]): its status, and for a
/// `failed` or `interrupted` one, its task's latest run and its task's
/// status, which tell whether recovery can still bring it back.
fn kept_run_of<Q: KeptRunReads + ?Sized>(
    queue: &Q,
    run: &RunId,
) -> Result<crate::domain::telemetry::KeptRun> {
    use crate::domain::RunStatus;
    use crate::domain::telemetry::KeptRun;
    let found = match queue.run(run) {
        Ok(found) => found,
        Err(error)
            if error
                .downcast_ref::<crate::application::RunNotFound>()
                .is_some() =>
        {
            return Ok(KeptRun::of(run, None, None, None));
        }
        Err(error) => return Err(error),
    };
    let status = found.status();
    if !matches!(status, RunStatus::Failed | RunStatus::Interrupted) {
        // Only a failed or interrupted run's task is read: the status
        // alone decides any other.
        return Ok(KeptRun {
            status: Some(status),
            revivable: true,
        });
    }
    let task = found.task_id();
    let latest = queue.latest_run_of(task)?;
    let page = queue.list(&crate::application::TaskQuery {
        status: crate::application::StatusFilter::Any,
        limit: 1,
        before: Some(task),
        ..crate::application::TaskQuery::default()
    })?;
    let task_status = page
        .tasks
        .first()
        .filter(|item| item.id == task)
        .map(|item| item.status);
    Ok(KeptRun::of(
        run,
        Some(status),
        latest.as_ref().map(|latest| latest.id()),
        task_status,
    ))
}

impl Supervisor<'_> {
    /// Issue `run`'s executor its telemetry token under this process's
    /// lease at the claim, a resume or its adoption, revoking every one it
    /// held ([`telemetry_tokens::issue_run`]). One not issued is logged:
    /// the run goes on without telemetry. A revocation that failed is held
    /// for the next sweep, in place of any revocation held of the run.
    pub(super) fn issue_run_telemetry(&mut self, run: &TaskRun) {
        self.record_environment(run.id());
        telemetry_tokens::issue_run(
            self.telemetry_tokens,
            &*self.queue,
            &self.layout.db,
            (run.id(), run.task_id()),
            self.registration.token(),
            &mut self.telemetry_retries,
        );
        self.register_run_sessions(run.id());
    }

    /// Record what `run` is attached to as it starts on this process (its
    /// claim, a resume, its adoption): the environment's
    /// `environment_attached`, under which the runs' resources are
    /// measured (docs/design/measurement-environment.md
    /// "environment_attached"). One not
    /// recorded is logged: the run goes on, and its resources are not
    /// measured until its next start.
    fn record_environment(&mut self, run: &RunId) {
        let Some(environment) = &self.environment else {
            return;
        };
        let payload = environment.attached().payload();
        if let Err(error) =
            self.queue
                .record_runtime_event(run, EventKind::EnvironmentAttached, payload)
        {
            warn!(run_id = %run, "environment_attached could not be recorded for run {run}: {error:#}");
        }
    }

    /// Register to `run` the sessions its events tell of
    /// ([`telemetry_tokens::register_run_sessions`]), before its wrapper
    /// starts the next turn: the claim's, a resume's, an adoption's, and a
    /// switch's or a renewal's new one. Events that cannot be read, and a
    /// registration that failed, are logged and tried again at the next
    /// pass ([`Self::sweep_telemetry`]); the run goes on as it would.
    pub(super) fn register_run_sessions(&mut self, run: &RunId) {
        match self.queue.run_events(run) {
            Ok(events) => telemetry_tokens::register_run_sessions(
                self.telemetry_tokens,
                &self.layout.db,
                run,
                &events,
                &mut self.telemetry_sessions,
            ),
            Err(error) => {
                warn!(run_id = %run, "the events of run {run} could not be read to register its sessions; the next pass tries again: {error:#}");
            }
        }
    }

    /// Register to each slot's run (and to the run of the replanning's
    /// diagnosis) the sessions its running jobs' agents named so far and
    /// not yet registered (a review agent's on Claude, a
    /// Codex thread: any whose session the runtime did not name ahead),
    /// before anything of them is sent ([`super::jobs::JobStepsOn`]): on
    /// every pass ([`Self::sweep_telemetry`]), and before a handoff or the
    /// loop's end stops them, so the steps their stop sends are the run's.
    /// A registration that failed is logged and asked again at the next
    /// pass, unless another target holds the session
    /// ([`telemetry_tokens::register_settled`]).
    pub(super) fn register_job_sessions(&mut self) {
        let (tokens, db) = (self.telemetry_tokens, self.layout.db.clone());
        let mut register = |target: &crate::domain::session_step::StepTarget, session: &str| {
            telemetry_tokens::register_settled(tokens, &db, target, session)
        };
        for slot in self.claim.slots.iter_mut() {
            register_slot_sessions(slot, &mut register);
        }
        // A replanning's diagnosis runs on a run outside its slot.
        if let Some(diagnosis) = self.replan_diagnosis.as_mut() {
            diagnosis.register_session(&mut register);
        }
    }

    /// [`stop_job`] on a slot taken out of the table, the sessions its
    /// jobs' agents named registered to its run first
    /// ([`Self::register_job_sessions`]).
    pub(super) fn stop_slot_job(&mut self, slot: &mut Slot) {
        let (tokens, db) = (self.telemetry_tokens, self.layout.db.clone());
        register_slot_sessions(slot, &mut |target, session| {
            telemetry_tokens::register_settled(tokens, &db, target, session)
        });
        stop_job(slot);
    }

    /// Register `session_id`, the session of a job this process starts on
    /// `run` named ahead (a review's, a recovery job's), to the run before
    /// the job starts ([`telemetry_tokens::register_session`]); `None` is
    /// left to the job's end, whose event names its thread.
    pub(super) fn register_run_job_session(&mut self, run: &RunId, session_id: Option<&str>) {
        if let Some(session_id) = session_id
            && !self.telemetry_sessions.contains(session_id)
            && telemetry_tokens::register_session(
                self.telemetry_tokens,
                &self.layout.db,
                &crate::domain::session_step::StepTarget::run(run),
                session_id,
            )
        {
            self.telemetry_sessions.insert(session_id.to_owned());
        }
    }

    /// A session of its own for a verification of `run` (the landing's,
    /// the e2e's), registered to the run before the verification starts,
    /// whose commands are sent as steps to the run's sink (ADR-t1662-3
    /// decision 1): `None` for a run that holds no token.
    pub(super) fn verification_steps(
        &mut self,
        run: &TaskRun,
    ) -> Option<crate::application::step_report::VerificationSteps> {
        if !crate::domain::queue_service::run_holds_token(run.status()) {
            return None;
        }
        Some(crate::application::step_report::VerificationSteps::of_run(
            self.telemetry_tokens,
            self.telemetry_sinks.clone(),
            &self.layout.db,
            run.id(),
            self.generators.ids.uuid(),
        ))
    }

    /// Before this process starts an executor on `run` (a review, a
    /// recovery job, the landing's or the e2e's verification): the run's
    /// telemetry token issued under its lease, issued now when the run
    /// holds none ([`telemetry_tokens::credential_for`]). A run that ended
    /// holds no token, as the service refuses them.
    pub(super) fn ready_run_telemetry(&mut self, run: &TaskRun) {
        if !crate::domain::queue_service::run_holds_token(run.status()) {
            return;
        }
        telemetry_tokens::credential_for(
            self.telemetry_tokens,
            &*self.queue,
            &self.layout.db,
            (run.id(), run.task_id()),
            self.registration.token(),
            &mut self.telemetry_retries,
        );
    }

    /// Try again the revocations held, then revoke the telemetry tokens
    /// that are due by the queue: the runs that ended, and those whose
    /// lease was lost, each only under the lease it was issued under
    /// ([`telemetry_tokens::sweep`]), and the targets of no run that ended
    /// or whose runner is gone, by the queue and this host's processes
    /// ([`telemetry_tokens::sweep_targets`]); then remove what the
    /// wrappers kept of the steps of targets that ended
    /// ([`telemetry_tokens::sweep_kept_steps`]).
    pub(super) fn sweep_telemetry(&mut self) {
        const CALLER: &str = "sweeps.sweep_telemetry";
        let _caller = crate::application::supervisor_service::caller::scope(CALLER);
        self.register_held_sessions();
        self.register_job_sessions();
        self.resend_kept_steps();
        let processes = &self.processes;
        telemetry_tokens::sweep_targets(
            self.telemetry_tokens,
            &*self.queue,
            &self.layout.db,
            self.generators.clock.now().saturating_mul(1000),
            &|pid| processes.alive(pid),
        );
        let mut retries = std::mem::take(&mut self.telemetry_retries);
        let now = self.generators.clock.now();
        let states = |run: &RunId, issuer: Option<&crate::domain::LeaseToken>| {
            self.run_token_state(run, issuer, now)
        };
        telemetry_tokens::sweep(
            self.telemetry_tokens,
            &*self.queue,
            &self.layout.db,
            self.registration.token(),
            &mut retries,
            &states,
        );
        self.telemetry_retries = retries;
        telemetry_tokens::sweep_kept_steps(self.telemetry_tokens, &self.layout.db, &|run| {
            self.kept_run(run)
        });
    }

    /// What the sweep of the wrappers' kept steps reads of `run`
    /// ([`kept_run_of`]).
    fn kept_run(&self, run: &RunId) -> Result<crate::domain::telemetry::KeptRun> {
        kept_run_of(&*self.queue, run)
    }

    /// Send again the steps kept ([`UnsentSteps`]), the session each kept
    /// step names first registered to its target, a run or an eval's job
    /// ([`super::jobs::resend_kept`]). On every pass, and before the ends
    /// of jobs are written ([`Self::write_job_ends`]): the queue service
    /// refuses the token of a row that ended, so what a job's end kept
    /// goes while its row is still open.
    ///
    /// [`UnsentSteps`]: crate::application::step_report::UnsentSteps
    pub(super) fn resend_kept_steps(&self) {
        super::jobs::resend_kept(self.telemetry_tokens, &self.layout.db, &self.unsent_steps);
    }

    /// Register the sessions of each run whose telemetry token this
    /// process issued and still holds ([`Self::register_run_sessions`]),
    /// before the sweep revokes those of the runs that ended: a thread a
    /// Codex turn or a job's end named since the last pass, as its
    /// executor reports it, and a registration that failed before. The
    /// tokens that cannot be listed are left to the next pass.
    fn register_held_sessions(&mut self) {
        let Ok(held) = self.telemetry_tokens.held_telemetry(&self.layout.db) else {
            return;
        };
        let lease = self.registration.token().clone();
        for token in held {
            if token.issued_under.as_ref() == Some(&lease)
                && let Some(run) = token.target.run_id()
            {
                self.register_run_sessions(&run);
            }
        }
    }

    /// Where the revocations this registration holds wait for the exec of
    /// its handoff ([`telemetry_tokens::Retries::file`]).
    fn telemetry_retries_file(&self) -> PathBuf {
        telemetry_tokens::Retries::file(
            self.layout.db.parent().unwrap_or(Path::new(".")),
            self.registration.token(),
        )
    }

    /// Keep the revocations held for the process the exec of the handoff
    /// hands the registration to: it keeps every lease, so no sweep would
    /// make them. One that cannot be kept is logged.
    pub(super) fn save_telemetry_retries(&self) {
        if let Err(error) = self.telemetry_retries.save(
            &*self.files,
            &self.telemetry_retries_file(),
            self.registration.token(),
        ) {
            warn!(
                "the telemetry tokens' revocations still to make could not be kept for the exec: {error:#}"
            );
        }
    }

    /// After the exec of a handoff, take over the revocations the process
    /// before held, which its next sweeps make.
    pub(super) fn take_telemetry_retries(&mut self) {
        self.telemetry_retries = telemetry_tokens::Retries::take(
            &*self.files,
            &self.telemetry_retries_file(),
            self.registration.token(),
        );
    }

    /// What the sweep reads of `run` for a token issued under `issuer` at
    /// `now` (unix seconds): its status, its lease and whether that lease
    /// stopped beating, and, with no lease, whether the issuer's
    /// registration is still there and beating.
    fn run_token_state(
        &self,
        run: &RunId,
        issuer: Option<&crate::domain::LeaseToken>,
        now: i64,
    ) -> Result<crate::domain::telemetry::RunTokenState> {
        let status = match self.queue.run(run) {
            Ok(found) => Some(found.status()),
            Err(error)
                if error
                    .downcast_ref::<crate::application::RunNotFound>()
                    .is_some() =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        let lease = self
            .queue
            .run_lease(run)?
            .map(|lease| crate::domain::telemetry::LeaseView {
                stale: self.lease_stale(&lease, now),
                token: lease.token,
            });
        let issuer_alive = match (issuer, &lease) {
            (Some(issuer), None) if issuer != self.registration.token() => {
                self.queue.supervisors()?.iter().any(|registration| {
                    registration.token == *issuer
                        && !crate::domain::heartbeat_stale(
                            self.processes.alive(registration.pid),
                            now - registration.heartbeat_at,
                        )
                })
            }
            _ => true,
        };
        Ok(crate::domain::telemetry::RunTokenState {
            status,
            lease,
            issuer_alive,
        })
    }

    /// Revoke the telemetry token of `run` issued under this process's
    /// lease, which it lost ([`telemetry_tokens::revoke_lost`]); a token
    /// another supervisor issued since is left as it is. One that cannot be
    /// revoked is held for the next sweep.
    pub(super) fn revoke_lost_telemetry(&mut self, run: &RunId) {
        telemetry_tokens::revoke_lost(
            self.telemetry_tokens,
            &*self.queue,
            &self.layout.db,
            run,
            self.registration.token(),
            &mut self.telemetry_retries,
        );
    }

    /// Tell a landing in progress whether this process drains for a
    /// handoff now (ADR-t3344-1): a stop wins over a handoff, so a drain
    /// that stops verifies again as one that does not drain.
    pub(super) fn note_handoff_drain(&self, stopping: bool) {
        self.handoff_drain.store(
            handoff::hands_landings_over(stopping, self.registration.handing_off()),
            Ordering::SeqCst,
        );
    }

    /// The runs the slots hold, which a cleanup and the sweep leave alone.
    pub(super) fn held_runs(&self) -> Vec<RunId> {
        self.claim.slots.runs()
    }

    /// The runs the slots hold, which a cleanup leaves alone, but for the
    /// waiting ones and those waiting for the landing slot it may take for
    /// disk space ([`Self::waiting_worktrees`], [`Self::landing_worktrees`]).
    pub(super) fn cleanup_held(&self) -> cleanup::Held {
        cleanup::Held {
            runs: self.held_runs(),
            waiting: self.waiting_worktrees(),
            landing: self.landing_worktrees(),
        }
    }

    /// The runs in the slots that wait for the landing slot
    /// (`AwaitingSlot`), which a cleanup for disk space may take the build
    /// outputs of ([`cleanup::LandingRun`]): none while a landing recheck
    /// runs or this process drains (a stop or a handoff lands or hands
    /// them over as before). The landing queue keeps no order of its own:
    /// each pass looks at the slots in turn, and the first of them that
    /// finds the landing slot free lands. So the last slot is the last to
    /// land, and they are offered from the last slot to the first.
    fn landing_worktrees(&self) -> Vec<cleanup::LandingRun> {
        let recheck = self.landing.rechecks.running();
        let draining = self.registration.draining();
        let mut landing: Vec<cleanup::LandingRun> = self
            .claim
            .slots
            .iter()
            .filter(|slot| offered_for_room(&slot.phase, slot.out_of_slot(), recheck, draining))
            .filter_map(|slot| {
                Some(cleanup::LandingRun {
                    run_id: slot.run.id().clone(),
                    task_id: slot.run.task_id(),
                    worktree: slot.run.worktree_path()?.to_owned(),
                    branch: slot.run.branch().map(str::to_owned),
                })
            })
            .collect();
        landing.reverse();
        landing
    }

    /// The sweep of what ended runs left, when host運用's
    /// [`sweep::SweepWatch`] says it is due: in the same pass the
    /// `approve_landing` asks the paths that ended their runs left open are
    /// closed ([`sweep::close_ended_landing_asks`]), the disk of the ended
    /// runs is asked to be freed ([`Self::request_cleanup`]), 計画管理
    /// sweeps its planners ([`PlanningState::sweep_planners`]), the
    /// registrations other supervisors left behind are pruned
    /// ([`sweep::prune_left_supervisors`]), what ended sessions and jobs
    /// left past its retention is removed
    /// ([`HostOpsState::remove_expired`]), and the background wrappers of
    /// ended runs still running are stopped
    /// ([`sweep::SweepWatch::sweep_ended_sessions`]).
    pub(super) fn sweep_ended_runs(&mut self, interval: Duration) -> Result<()> {
        const CALLER: &str = "sweeps.sweep_ended_runs";
        let _caller = crate::application::supervisor_service::caller::scope(CALLER);
        if !self
            .host
            .sweep_due(self.generators.clock.monotonic(), interval)
        {
            return Ok(());
        }
        close_ended_landing_asks(&mut *self.queue, None);
        self.request_cleanup(None);
        self.on_planning(|planning, env| planning.sweep_planners(env));
        self.sweep_worker_tokens();
        self.sweep_verification_tokens();
        let held = self.held_runs();
        // The sweep closes 実行と着地's asks and stalled detections of the
        // runs it stops, so it takes the loop's whole queue.
        let (mut env, _, host) = self.split();
        if let Err(error) = host.prune_left_supervisors(&env) {
            warn!(error = %format_args!("{error:#}"), "the registrations other supervisors left behind could not be read: {error:#}");
        }
        host.remove_expired(&env);
        host.sweep_ended_sessions(&mut env, &held)
    }

    /// Revoke the tokens of the verifications no run runs now
    /// ([`crate::application::queue_broker::sweep_left_tokens`]): those an
    /// `integrate` or a supervisor killed in the middle of one left
    /// behind, on the first pass and at each sweep after. A failure is
    /// logged and recorded, and the sweep after takes it up again.
    fn sweep_verification_tokens(&self) {
        use crate::application::queue_broker::{note_token_sweep, sweep_left_tokens};
        let queue = &self.queue;
        let swept = sweep_left_tokens(&*self.verification_tokens, &|run| match queue.run(run) {
            Ok(run) => Ok(Some(run.status())),
            Err(error)
                if error
                    .downcast_ref::<crate::application::RunNotFound>()
                    .is_some() =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        });
        note_token_sweep(&swept, "supervisor", |payload| {
            self.queue
                .record_queue_event(EventKind::QueueBrokerTokensSwept, payload)
                .map(drop)
        });
    }

    /// The tokens of `run`'s verifications as issued now, read before a
    /// recovery moves it from `previous`
    /// ([`crate::application::queue_broker::LeftTokens`]).
    pub(super) fn left_verification_tokens(
        &self,
        run: &RunId,
        previous: RunStatus,
    ) -> crate::application::queue_broker::LeftTokens {
        crate::application::queue_broker::LeftTokens::read(
            &*self.verification_tokens,
            run,
            previous,
        )
    }

    /// Revoke the tokens `left` read whose verification does not run now
    /// the run is `status`, recovered with none running; a failure is
    /// logged and recorded, and the next sweep takes it up.
    pub(super) fn revoke_left_verification_tokens(
        &self,
        left: crate::application::queue_broker::LeftTokens,
        status: RunStatus,
    ) {
        let revoked = left.revoke(&*self.verification_tokens, status);
        crate::application::queue_broker::note_token_sweep(&revoked, "recovery", |payload| {
            self.queue
                .record_queue_event(EventKind::QueueBrokerTokensSwept, payload)
                .map(drop)
        });
    }

    /// Revoke the queue service's tokens of `run`'s worker and its session
    /// wrapper, whose run has ended (ADR-t1233-4 decision 4, ADR-t3102-1
    /// decision 2): their values leave the disk with the run. A failure is
    /// the sweep's to take up again.
    pub(super) fn revoke_worker_token(&self, run: &TaskRun) {
        let worker = ActorContext::worker(run.id(), run.task_id());
        let wrapper = crate::domain::queue_service::wrapper_actor_id(
            &crate::domain::session_step::StepTarget::run(run.id()),
        );
        for actor_id in [worker.actor_id(), wrapper.as_str()] {
            if let Err(error) = self.service_access.revoke(&self.layout.db, actor_id) {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: the queue service token of {actor_id} could not be revoked: {error:#}", run.id());
            }
        }
    }

    /// Revoke the tokens of the workers and the session wrappers whose
    /// runs ended or are not in the queue (`run_token_ended`), those the
    /// end of their run did not revoke: a run that ended while no
    /// supervisor looked, by a cancel, or by another process; those of
    /// the planners and their wrappers whose planner ended
    /// (`planner_token_ended`); the inbox's whose session ended
    /// (`session_token_ended`); the landings' integrators whose run left
    /// `integrating` with no live landing on it; the jobs' whose process
    /// that asked for them ended, which the service reads from each token's
    /// record (`job_token_ended`); and the supervisors' whose registration
    /// is gone, which the service revokes once the process that asked for
    /// it ended too (`supervisor_token_ended`). A run or a planner whose
    /// record does not read keeps its token, and so does every supervisor's
    /// when the registrations do not read.
    fn sweep_worker_tokens(&self) {
        use crate::domain::queue_service::{Principal, run_token_ended};
        let queue = &self.queue;
        let processes = &self.processes;
        let landing: Vec<RunId> = self
            .claim
            .slots
            .iter()
            .filter(|slot| matches!(slot.phase, Phase::Landing(_)))
            .map(|slot| slot.run.id().clone())
            .collect();
        let own_integrator =
            ActorContext::instance(crate::domain::ActorRole::Integrator, self.layout.pid);
        let registered: Option<Vec<LeaseToken>> = queue.supervisors().ok().map(|registrations| {
            registrations
                .into_iter()
                .map(|registration| registration.token)
                .collect()
        });
        let mut ended = |principal: &Principal| {
            // A supervisor's sign-in of a registration that ended
            // (ADR-t3102-1 decision 2): pruned by its stop, by `up`, `down`
            // or a sweep that could not revoke it.
            if principal.role == crate::domain::ActorRole::Supervisor {
                return principal.registration.as_ref().is_some_and(|token| {
                    registered
                        .as_ref()
                        .is_some_and(|registered| !registered.contains(token))
                });
            }
            // An inbox's ends with its session's process (ADR-t3102-1
            // decision 2): its pid gone, or another process's start on it.
            // A start that does not read keeps the token, as a run whose
            // status does not read does.
            if principal.session.is_some() {
                return crate::domain::queue_service::session_token_ended(principal, |session| {
                    processes.alive(session.pid)
                        && processes
                            .session_start(session.pid)
                            .is_none_or(|start| start == session.start)
                });
            }
            // A landing's integrator (ADR-t3102-1 decision 2): ended once
            // its run left `integrating` and no landing of this process or
            // of a live one is on it.
            if let Some(run) = principal.landing_run() {
                let landing_here = principal.actor_id == own_integrator.actor_id();
                if landing_here && landing.contains(&run) {
                    return false;
                }
                let gone = landing_here
                    || principal
                        .actor_id
                        .strip_prefix("integrator:")
                        .and_then(|pid| pid.parse::<u32>().ok())
                        .is_none_or(|pid| !processes.alive(pid));
                return gone
                    && match queue.run(&run) {
                        Ok(run) => run.status() != crate::domain::RunStatus::Integrating,
                        Err(error) => error
                            .downcast_ref::<crate::application::RunNotFound>()
                            .is_some(),
                    };
            }
            if let Some(ended) =
                crate::domain::queue_service::planner_token_ended(principal, |id| {
                    queue.planner(id).ok()
                })
            {
                return ended;
            }
            // A job's is offered, and the service revokes it only once the
            // process that asked for it ended (ADR-t3102-1 decision 2).
            if crate::domain::queue_service::is_job(principal.role) {
                return true;
            }
            let Some(run) = &principal.run_id else {
                return false;
            };
            let status = match queue.run(run) {
                Ok(run) => Some(run.status()),
                Err(error)
                    if error
                        .downcast_ref::<crate::application::RunNotFound>()
                        .is_some() =>
                {
                    None
                }
                Err(_) => return false,
            };
            run_token_ended(principal, status)
        };
        match self.service_access.sweep(&self.layout.db, &mut ended) {
            Ok(revoked) if !revoked.is_empty() => {
                info!("revoked the queue service tokens of ended runs and planners: {revoked:?}");
            }
            Ok(_) => {}
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the queue service tokens of ended runs could not all be revoked: {error:#}");
            }
        }
    }

    /// Ask host運用 for the worktrees of ended runs to be cleaned, every
    /// such run's or only `task`'s ([`HostOpsState::request_cleanup`]).
    /// Requested when a slot ends (integrated, failed or interrupted),
    /// after triage, and after applying triage decide or landing approval
    /// answers, including cancellation (then for that task alone). The
    /// periodic sweep requests every run. Manual integrate removes its
    /// landed worktree itself; manual recover, ready or cancel, and runs
    /// ended outside the supervisor, are picked up by the next sweep.
    pub(in crate::application) fn request_cleanup(&mut self, task: Option<TaskId>) {
        let held = self.cleanup_held();
        self.on_host(|host, env| host.request_cleanup(env, &held, task, None));
    }

    /// A handoff withdrawn while this process drained: the cleanup goes
    /// back to normal ([`HostOpsState::resume_cleanup`]).
    pub(super) fn resume_cleanup(&mut self) {
        let held = self.cleanup_held();
        self.on_host(|host, env| host.resume_cleanup(env, &held));
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// The runs, the tasks and each task's latest run of a queue, for
    /// [`kept_run_of`]; the tasks whose latest run was read and the queries
    /// `list` was asked, and a read of a run that fails otherwise than with
    /// [`crate::application::RunNotFound`].
    #[derive(Default)]
    struct KeptRuns {
        runs: Vec<TaskRun>,
        tasks: Vec<(TaskId, crate::domain::TaskStatus)>,
        latest: HashMap<TaskId, TaskRun>,
        broken: bool,
        latest_reads: RefCell<Vec<TaskId>>,
        queries: RefCell<Vec<(crate::application::StatusFilter, usize, Option<TaskId>)>>,
    }

    impl KeptRunReads for KeptRuns {
        fn run(&self, id: &RunId) -> Result<TaskRun> {
            anyhow::ensure!(!self.broken, "the queue is busy");
            self.runs
                .iter()
                .find(|run| run.id() == id)
                .cloned()
                .ok_or_else(|| crate::application::RunNotFound(id.clone()).into())
        }
        fn latest_run_of(&self, task: TaskId) -> Result<Option<TaskRun>> {
            self.latest_reads.borrow_mut().push(task);
            Ok(self.latest.get(&task).cloned())
        }
        /// As the queue pages them: newest first, from `before` on (at most
        /// it), any status with [`crate::application::StatusFilter::Any`],
        /// else (`kept_run_of` asks no other) the tasks that did not end.
        fn list(
            &self,
            query: &crate::application::TaskQuery,
        ) -> Result<crate::application::TaskPage> {
            use crate::application::StatusFilter;
            self.queries
                .borrow_mut()
                .push((query.status.clone(), query.limit, query.before));
            let mut tasks: Vec<_> = self
                .tasks
                .iter()
                .filter(|(id, status)| {
                    query.before.is_none_or(|before| *id <= before)
                        && (query.status == StatusFilter::Any || !status.is_terminal())
                })
                .collect();
            tasks.sort_by_key(|(id, _)| std::cmp::Reverse(*id));
            let total = tasks.len();
            Ok(crate::application::TaskPage {
                tasks: tasks
                    .into_iter()
                    .take(query.limit)
                    .map(|(id, status)| {
                        crate::application::TaskListItem::new(
                            kept_task(*id, *status),
                            Vec::new(),
                            Vec::new(),
                            None,
                            None,
                            false,
                        )
                    })
                    .collect(),
                next: None,
                total,
            })
        }
    }

    fn kept_task(id: TaskId, status: crate::domain::TaskStatus) -> crate::domain::Task {
        crate::domain::Task::restore(crate::domain::TaskRecord {
            goal_priority: None,
            id,
            title: "work".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            status,
            goal_id: None,
            context: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            worker: crate::domain::worker::Worker::CLAUDE_INTERACTIVE,
            named_mode: None,
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap()
    }

    fn kept_run_in(id: &str, task: TaskId, status: RunStatus) -> TaskRun {
        let provider = crate::domain::Provider::Claude;
        let commit =
            crate::domain::CommitSha::parse("1111111111111111111111111111111111111111", "commit")
                .unwrap();
        TaskRun::restore(crate::domain::RunRecord {
            id: RunId::new(id).unwrap(),
            task_id: task,
            status,
            requested_provider: provider,
            actual_provider: provider,
            worker_mode: crate::domain::worker::Worker::default_mode(provider),
            // An `integrated` run holds its landing's commit.
            result_commit: (status == RunStatus::Integrated).then(|| commit.clone()),
            base_commit: commit,
            branch: None,
            worktree_path: None,
            workspace_id: None,
            receipt_path: None,
            log_path: None,
            repo_path: None,
            run_dir: None,
            last_error: None,
            workspace_closed_at: None,
            created_at: String::new(),
        })
        .unwrap()
    }

    /// A run not in the queue is read as one whose kept steps go, and a
    /// read that fails otherwise is the sweep's error, which keeps them.
    #[test]
    fn a_run_not_in_the_queue_is_kept_as_gone_and_a_failed_read_is_an_error() {
        use crate::domain::telemetry::KeptRun;
        let gone = RunId::new("gone").unwrap();
        let queue = KeptRuns::default();
        assert_eq!(
            kept_run_of(&queue, &gone).unwrap(),
            KeptRun::of(&gone, None, None, None)
        );
        let broken = KeptRuns {
            broken: true,
            ..KeptRuns::default()
        };
        let error = kept_run_of(&broken, &gone).unwrap_err().to_string();
        assert_eq!(error, "the queue is busy");
    }

    /// A `succeeded` or `integrated` run is decided by its status alone:
    /// its task is not read.
    #[test]
    fn a_run_that_did_not_fail_is_kept_by_its_status_alone() {
        use crate::domain::telemetry::KeptRun;
        let task = TaskId::new(5);
        for status in [RunStatus::Succeeded, RunStatus::Integrated] {
            let run = kept_run_in("done", task, status);
            let queue = KeptRuns {
                runs: vec![run.clone()],
                tasks: vec![(task, crate::domain::TaskStatus::Completed)],
                ..KeptRuns::default()
            };
            assert_eq!(
                kept_run_of(&queue, run.id()).unwrap(),
                KeptRun {
                    status: Some(status),
                    revivable: true,
                }
            );
            assert!(queue.latest_reads.borrow().is_empty());
            assert!(queue.queries.borrow().is_empty());
        }
    }

    /// A `failed` or `interrupted` run is revivable while it is its task's
    /// latest run and its task goes on (`ready`, `in_progress`), and is not
    /// once its task has a later run or ended. Its task's status is read
    /// off the page that starts at the task (`before` takes the task in),
    /// so the tasks just before and after it, which ended, are not read in
    /// its place.
    #[test]
    fn a_failed_runs_task_is_read_for_whether_recovery_can_bring_it_back() {
        use crate::domain::TaskStatus;
        let task = TaskId::new(5);
        let neighbours = [
            (TaskId::new(4), TaskStatus::Canceled),
            (TaskId::new(6), TaskStatus::Completed),
        ];
        let case = |status: RunStatus, task_status: TaskStatus, later: bool| {
            let run = kept_run_in("run", task, status);
            let latest = if later {
                kept_run_in("later", task, RunStatus::Claimed)
            } else {
                run.clone()
            };
            let queue = KeptRuns {
                runs: vec![run.clone()],
                tasks: [(task, task_status)]
                    .into_iter()
                    .chain(neighbours)
                    .collect(),
                latest: HashMap::from([(task, latest)]),
                ..KeptRuns::default()
            };
            let kept = kept_run_of(&queue, run.id()).unwrap();
            assert_eq!(kept.status, Some(status));
            assert_eq!(
                queue.queries.into_inner(),
                [(crate::application::StatusFilter::Any, 1, Some(task))]
            );
            kept.revivable
        };
        for status in [RunStatus::Failed, RunStatus::Interrupted] {
            for going_on in [TaskStatus::Ready, TaskStatus::InProgress] {
                assert!(case(status, going_on, false), "{status:?} {going_on:?}");
                assert!(!case(status, going_on, true), "{status:?} {going_on:?}");
            }
            for ended in [TaskStatus::Completed, TaskStatus::Canceled] {
                assert!(!case(status, ended, false), "{status:?} {ended:?}");
            }
        }
        // A run with no latest run recorded whose task goes on is revivable;
        // one whose task is not in the queue is not, though the page that
        // starts at it holds an older task that goes on.
        let run = kept_run_in("run", task, RunStatus::Failed);
        let queue = |tasks: Vec<(TaskId, TaskStatus)>| KeptRuns {
            runs: vec![run.clone()],
            tasks,
            ..KeptRuns::default()
        };
        let alone = queue(vec![(task, TaskStatus::InProgress)]);
        assert!(kept_run_of(&alone, run.id()).unwrap().revivable);
        assert_eq!(alone.latest_reads.into_inner(), [task]);
        let missing = queue(vec![(TaskId::new(4), TaskStatus::Ready)]);
        assert!(!kept_run_of(&missing, run.id()).unwrap().revivable);
    }
}
