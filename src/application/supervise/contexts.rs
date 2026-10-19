//! What the passes of the supervisor's 計画管理, 観測と分析 and host運用
//! contexts read and call of the loop, apart from their state, which is in
//! each context's module (rule C3 of docs/design/architecture.md; which
//! context owns each field is on `Supervisor`). Each context's submodules
//! change only their own state: their passes take it with a [`PassEnv`]
//! (計画管理's with a [`PlanningEnv`]) and the values the loop reads for
//! them (the clock, whether it drains or hands off, the runs its slots
//! hold), and the loop calls them and applies what they return.

use super::*;
use crate::application::{HostOpsQueue, ObservationQueue, RepositoryReads, SupervisorReads};

/// What a pass of a context reads and calls of the loop: the parts every
/// context shares (the queue's connection, the clock, the
/// places) and the adapters, borrowed apart from the context's own state.
/// The connection is the loop's whole queue until [`PassEnv::observation`]
/// or [`PassEnv::host`] keeps the ports `Q` of the context, and the
/// repository `R` the loop's whole [`Repository`] until
/// [`PassEnv::observation`] keeps its reads.
pub(super) struct PassEnv<
    's,
    Q: ?Sized = dyn Queue + Send,
    R: ?Sized = Arc<dyn Repository + Send + Sync>,
> {
    pub(super) queue: &'s mut Q,
    pub(super) generators: &'s Generators,
    pub(super) layout: &'s Layout,
    pub(super) processes: &'s Arc<dyn ProcessControl + Send + Sync>,
    pub(super) files: &'s Arc<dyn RunFiles>,
    pub(super) repository: &'s R,
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
}

/// 観測と分析's [`PassEnv`]: the loop's connection as its ports and the
/// repository as its reads (rule C2 of docs/design/architecture.md).
pub(super) type ObservationEnv<'s> =
    PassEnv<'s, dyn ObservationQueue + Send, dyn RepositoryReads + Send + Sync>;

/// host運用's [`PassEnv`]: the loop's connection as its ports.
pub(super) type HostEnv<'s> = PassEnv<'s, dyn HostOpsQueue + Send>;

impl<'s> PassEnv<'s> {
    /// This environment with the connection as 観測と分析's ports.
    pub(super) fn observation(self) -> ObservationEnv<'s> {
        let PassEnv {
            queue,
            generators,
            layout,
            processes,
            files,
            repository,
            verifier,
            spawner,
            sessions,
            token,
        } = self;
        PassEnv {
            queue,
            generators,
            layout,
            processes,
            files,
            repository: &**repository,
            verifier,
            spawner,
            sessions,
            token,
        }
    }

    /// This environment with the connection as host運用's ports.
    pub(super) fn host(self) -> HostEnv<'s> {
        let PassEnv {
            queue,
            generators,
            layout,
            processes,
            files,
            repository,
            verifier,
            spawner,
            sessions,
            token,
        } = self;
        PassEnv {
            queue,
            generators,
            layout,
            processes,
            files,
            repository,
            verifier,
            spawner,
            sessions,
            token,
        }
    }
}

impl<Q: SupervisorReads + RunLog + ?Sized, R: ?Sized> PassEnv<'_, Q, R> {
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
