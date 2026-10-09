//! The state of the supervisor's 計画管理, 観測と分析 and host運用
//! contexts, apart from the loop's (docs/design/architecture.md,
//! "`Supervisor`の状態"). Each context's submodules change only their own
//! state: their passes take it with a [`PassEnv`] (計画管理's with a
//! [`PlanningEnv`]) and the values the loop reads for them (the clock,
//! whether it drains or hands off, the runs its slots hold), and the loop
//! calls them and applies what they return.

use super::*;

/// What a pass of a context reads and calls of the loop: the parts every
/// context shares (the queue, the clock, the places) and the adapters,
/// borrowed apart from the context's own state.
pub(super) struct PassEnv<'s> {
    pub(super) queue: &'s mut (dyn Queue + Send),
    pub(super) queues: &'s Arc<dyn QueueOpener>,
    pub(super) generators: &'s Generators,
    pub(super) layout: &'s Layout,
    pub(super) processes: &'s Arc<dyn ProcessControl + Send + Sync>,
    pub(super) files: &'s Arc<dyn RunFiles>,
    pub(super) repository: &'s Arc<dyn Repository + Send + Sync>,
    pub(super) verifier: &'s Arc<dyn Verifier + Send + Sync>,
    pub(super) spawner: &'s dyn Spawner,
    pub(super) sessions: &'s dyn SessionWrappers,
    pub(super) token: &'s LeaseToken,
}

impl PassEnv<'_> {
    /// `[roles.*]` of `dagq.toml` as read now ([`role_models_of`]).
    pub(super) fn role_models(
        &self,
        role: crate::domain::actor_model::ModelRole,
    ) -> crate::domain::actor_model::RoleModels {
        role_models_of(&**self.verifier, role)
    }

    /// What a session of `role` starts with, from `[roles.<role>]` as read
    /// now (ADR-0079 decision 7).
    pub(super) fn actor_launch(
        &self,
        role: crate::domain::actor_model::ModelRole,
    ) -> crate::domain::actor_model::ActorLaunch {
        self.role_models(role).launch(role)
    }

    /// Apply what the headless job `job` returned with the job as the
    /// `requested_by` of the events written meanwhile (ADR-t728-1 decision
    /// 1, task 730), put back afterwards (task 783).
    pub(super) fn for_job<T>(
        &mut self,
        job: &ActorContext,
        apply: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        requested_by_job(self, |env| &*env.queue, job, apply)
    }

    /// Record `kinds`' held or resumed event when `hold` differs from the
    /// hold in place on the queue (task 327); whether it holds.
    pub(super) fn record_hold(
        &mut self,
        kinds: claim_hold::HoldKinds,
        hold: Option<&ClaimHold>,
    ) -> Result<bool> {
        let last = self.queue.latest_queue_event(&kinds.kinds())?;
        // The supervisors running now: a hold another one recorded is in
        // place only while it runs (its registration's heartbeat is fresh).
        let now = self.generators.clock.now();
        let live: Vec<LeaseToken> = self
            .queue
            .supervisors()?
            .into_iter()
            .filter(|registration| {
                !heartbeat_stale(
                    self.processes.alive(registration.pid),
                    now - registration.heartbeat_at,
                )
            })
            .map(|registration| registration.token)
            .collect();
        if let Some((kind, payload)) =
            claim_hold::transition_of(kinds, hold, last.as_ref(), self.token, |holder| {
                live.iter().any(|token| token.as_str() == holder)
            })
        {
            self.queue.record_queue_event(kind, payload)?;
            match (hold, kinds == claim_hold::LANDINGS) {
                (Some(hold), _) => warn!("{}", hold.message_for(kinds)),
                (None, false) => info!("claims resume: nothing holds them any more"),
                (None, true) => info!("landings resume: there is room for their verification"),
            }
        }
        Ok(hold.is_some())
    }

    /// Record the change of a hold of this supervisor's claims kept in its
    /// memory against its own latest record
    /// ([`claim_hold::OwnHold::transition`]); a queue that cannot be read
    /// or written is warned of and `false` returned, for the caller to try
    /// again.
    pub(super) fn record_own_hold(
        &mut self,
        kinds: claim_hold::OwnHold,
        hold: Option<(&str, Value)>,
    ) -> bool {
        let recorded = crate::application::health::own_hold_records(
            &*self.queue,
            kinds,
            &[self.token.as_str()],
        )
        .and_then(|records| {
            let last = claim_hold::OwnHold::latest_of(&records, self.token.as_str());
            match kinds.transition(hold, last, self.token) {
                Some((kind, payload)) => self.queue.record_queue_event(kind, payload).map(drop),
                None => Ok(()),
            }
        });
        if let Err(error) = recorded {
            warn!(error = %format_args!("{error:#}"), "the hold {} could not be recorded: {error:#}", kinds.held.as_str());
            return false;
        }
        true
    }
}

/// 観測と分析's state: the jobs on its timer (the observer, the throughput
/// review), the KPI reports and their push, the forecast snapshots, the
/// candidates' sample and the CI watch. The claim, the resume and the
/// landing read the CI watch's hold ([`CiWatchState::held`],
/// [`CiWatchState::unreadable`]).
pub(super) struct ObservationState {
    /// The observer job running now: one at a time, outside the run slots.
    pub(super) observer: Option<observer::ObserverJob>,
    /// When this process last launched each observation, so one that dies
    /// before it records anything is not relaunched on every pass.
    pub(super) observers_launched: Vec<(ObserveMode, Instant)>,
    /// The observation due again now whatever the queue says: one that
    /// found its provider unusable, which starts again on the other
    /// provider (ADR-t1063-1 decision 4, task 1223) or, with
    /// `[provider_fallback] jobs` off, on the same one once its hold ends
    /// (ADR-t1857-1).
    pub(super) observer_again: Option<ObserveMode>,
    /// The throughput review running now (ADR-t996-1), and the periods this
    /// process started.
    pub(super) throughput_review: throughput_review::ThroughputReviewWatch,
    /// The `candidates_sampled` this process recorded last (ADR-0051
    /// decision 3); `None` until its first claim pass records one.
    pub(super) candidates: Option<CandidatesSample>,
    /// Writes the daily KPI reports; `None` writes none.
    pub(super) reports: Option<ReportPort>,
    /// The report job and the day the reports were last found written.
    pub(super) report: report::ReportWatch,
    /// Takes the forecast snapshots; `None` takes none.
    pub(super) forecasts: Option<ForecastPort>,
    /// The snapshot job and where the last look for triggers left off.
    pub(super) forecast: forecast::ForecastWatch,
    /// The KPI push's messages waiting and the one being sent.
    pub(super) push: push::PushWatch,
    /// Watches the landing branch's CI (ADR-t1920-1).
    pub(super) ci_watch_port: Option<CiWatchPort>,
    pub(super) ci: ci_watch::CiWatchState,
    /// The record of `stats`' judgments of now.
    pub(super) live_alerts: live_alerts::LiveAlertWatch,
}

impl ObservationState {
    pub(super) fn new(ports: &Ports<'_>) -> Self {
        Self {
            observer: None,
            observers_launched: Vec::new(),
            observer_again: None,
            throughput_review: throughput_review::ThroughputReviewWatch::default(),
            candidates: None,
            reports: ports.reports.clone(),
            report: report::ReportWatch::default(),
            forecasts: ports.forecasts.clone(),
            forecast: forecast::ForecastWatch::default(),
            push: push::PushWatch::default(),
            ci_watch_port: ports.ci_watch.clone(),
            ci: ci_watch::CiWatchState::default(),
            live_alerts: live_alerts::LiveAlertWatch::default(),
        }
    }

    /// A job of the context runs that a handoff waits for: the KPI report,
    /// the forecast, the KPI push being sent and the CI check.
    pub(super) const fn handoff_waits(&self) -> bool {
        self.push.running() || self.report.running() || self.forecast.running() || self.ci.running()
    }

    /// A job of the context runs that a loop with no slot waits for, like a
    /// run: the observer, the throughput review, the report, the forecast,
    /// the CI check and the message being sent, and with `drain_pushes`
    /// the messages still to be tried too.
    pub(super) fn busy(&self, drain_pushes: bool) -> bool {
        self.observer.is_some()
            || self.throughput_review.running()
            || self.report.running()
            || self.forecast.running()
            // A CI check holds the claims until it answers.
            || self.ci.running()
            // A message being sent is bounded by the command's timeout.
            || self.push.running()
            || (drain_pushes && self.push.busy())
    }

    /// Record `candidates_sampled` when `sample` differs from the sample
    /// this process recorded last, and on its first claim pass. A failure
    /// is logged: the sample is bookkeeping for `kpi`, and the next pass
    /// tries again.
    pub(super) fn record_candidates(
        &mut self,
        env: &mut PassEnv<'_>,
        sample: Result<CandidatesSample>,
    ) {
        let result = sample.and_then(|sample| {
            if let Some(mut payload) = sample.transition(self.candidates.as_ref()) {
                payload["supervisor"] = json!(env.token);
                env.queue
                    .record_queue_event(EventKind::CandidatesSampled, payload)?;
                self.candidates = Some(sample);
            }
            Ok(())
        });
        if let Err(error) = result {
            warn!(error = %format_args!("{error:#}"), "the candidates could not be sampled: {error:#}");
        }
    }
}

/// host運用's state: the queue service, the automatic update and the
/// release update, the sccache server, the free disk space and the cleanup
/// of what ended runs left, the sweep of their sessions, the record of the
/// host's load and its limit. The supervisor's registration and handoff
/// are [`handoff::Registration`]. The claim and the landing read the
/// disk's reading ([`HostOpsState::free`], [`disk::DiskWatch`]), the
/// sccache server's look and the load.
pub(super) struct HostOpsState {
    /// Records the host's load; `None` records none (task 516).
    pub(super) host_metrics_port: Option<HostMetricsPort>,
    /// The sample job of the host's load.
    pub(super) host_metrics: host_metrics::HostMetricsWatch,
    /// Keeps the queue's service; `None` keeps none.
    pub(super) queue_service_port: Option<QueueServicePort>,
    /// What the supervisor knows of the queue's service.
    pub(super) queue_service: queue_service::QueueServiceWatch,
    /// Whether the queue service ran at the last look, or no service is
    /// kept (ADR-t1233-4 decision 2): new claims and jobs wait for it.
    pub(super) service_up: bool,
    /// The automatic update's look at main (ADR-0045 decision 17).
    pub(super) update: update::UpdateWatch,
    /// Looks for a new release; `None` looks for none.
    pub(super) release_port: Option<ReleasePort>,
    /// The release look running and when the last one started.
    pub(super) release: release::ReleaseWatch,
    /// Looks at and starts the host's sccache server (ADR-t1215-1).
    pub(super) sccache_port: Option<SccachePort>,
    pub(super) sccache: sccache::SccacheWatch,
    /// `[disk]`: how much free disk space a claim and a landing need
    /// (task 377).
    pub(super) disk_config: crate::domain::disk::DiskConfig,
    /// Reads the free bytes of the file system of a path.
    pub(super) free_space: fn(&Path) -> Option<u64>,
    /// Lists the directories of the Claude Code scratchpads (task 1100).
    pub(super) scratchpad_roots: ScratchpadRoots,
    /// The disk between passes (task 377).
    pub(super) disk: disk::DiskWatch,
    /// The free bytes of the queue's directory read this pass.
    pub(super) free: Option<u64>,
    /// The cleanup of ended runs' worktrees off the loop (task 405).
    pub(super) cleanup: cleanup::CleanupWatch,
    /// The sweep of the session wrappers ended runs left running.
    pub(super) sweep: sweep::SweepWatch,
    /// `--max-load` (task 327).
    pub(super) max_load: Option<f64>,
    /// The 1-minute load average, and the host's versions a claim records.
    pub(super) load_average: fn() -> Option<f64>,
    pub(super) host_versions: fn(&Path, Option<&Path>, Option<&Path>) -> HostVersions,
}

impl HostOpsState {
    pub(super) fn new(ports: &Ports<'_>, settings: &LoopSettings) -> Self {
        Self {
            host_metrics_port: ports.host_metrics.clone(),
            host_metrics: host_metrics::HostMetricsWatch::default(),
            queue_service_port: ports.queue_service.clone(),
            queue_service: queue_service::QueueServiceWatch::default(),
            service_up: true,
            update: update::UpdateWatch::default(),
            release_port: ports.release.clone(),
            release: release::ReleaseWatch::default(),
            sccache_port: ports.sccache.clone(),
            sccache: sccache::SccacheWatch::default(),
            disk_config: settings.disk,
            free_space: ports.free_space,
            scratchpad_roots: ports.scratchpad_roots.clone(),
            disk: disk::DiskWatch::default(),
            free: None,
            cleanup: cleanup::CleanupWatch::default(),
            sweep: sweep::SweepWatch::default(),
            max_load: settings.max_load,
            load_average: ports.load_average,
            host_versions: ports.host_versions,
        }
    }

    /// A job of the context runs that a handoff waits for: the cleanup,
    /// the sample of the host's load and the release look.
    pub(super) const fn handoff_waits(&self) -> bool {
        self.cleanup.running() || self.host_metrics.running() || self.release.running()
    }
}

/// 計画管理's state: the plan review and the goal review running now (one
/// each, queue-wide, outside the run slots), the runtime's planners asked
/// to exit, and what opening a planner reads. Only its submodules change
/// it, through their passes.
pub(super) struct PlanningState {
    /// The plan review job running now (ADR-0041 decision 11).
    pub(super) plan_review: Option<plan_review::PlanReviewWatch>,
    /// The goal review job running now.
    pub(super) goal_review: Option<goal_review::GoalReviewWatch>,
    /// The runtime's planners this process asked to exit (their exit
    /// request), and when.
    pub(super) planner_exits: Vec<(crate::domain::PlannerId, Instant)>,
    /// Whether `[roles.runtime_planner] route`, which the runtime's
    /// planners ignore, was warned of (ADR-t1433-2 decision 3).
    pub(super) route_setting_warned: bool,
    /// Reads the limit on the improvement proposals running.
    pub(super) max_improvement_proposals: Arc<dyn Fn() -> Result<usize> + Send + Sync>,
}

impl PlanningState {
    pub(super) fn new(ports: &Ports<'_>) -> Self {
        Self {
            plan_review: None,
            goal_review: None,
            planner_exits: Vec::new(),
            route_setting_warned: false,
            max_improvement_proposals: ports.max_improvement_proposals.clone(),
        }
    }

    /// A plan or goal review runs, which a loop with no slot waits for
    /// like a run.
    pub(super) const fn busy(&self) -> bool {
        self.plan_review.is_some() || self.goal_review.is_some()
    }
}

/// What a pass of 計画管理 reads and calls apart from its own state: the
/// loop's shared parts ([`PassEnv`], which it derefs to), 実行と着地's
/// headless jobs and providers ([`JobDesk`]) and the expected files of the
/// claim's deferral (a plan review's material), and the values the loop
/// reads for it: the limit on the runtime's planners, the `[conflicts]`
/// thresholds and the limits of a headless turn.
pub(super) struct PlanningEnv<'s> {
    pub(super) pass: PassEnv<'s>,
    pub(super) jobs: JobDesk<'s>,
    pub(super) defer: &'s mut claim_defer::DeferWatch,
    /// `runtime_planners` of `[supervisor]` as last read.
    pub(super) runtime_planners: usize,
    pub(super) conflicts: crate::domain::stats::ConflictConfigReport,
    pub(super) turn_limits: crate::domain::turn::TurnLimits,
}

impl<'s> std::ops::Deref for PlanningEnv<'s> {
    type Target = PassEnv<'s>;

    fn deref(&self) -> &Self::Target {
        &self.pass
    }
}

impl std::ops::DerefMut for PlanningEnv<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.pass
    }
}
