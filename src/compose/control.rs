//! The control-side commands a person or the inbox runs on the host
//! (`integrate`, `review`, `observe`, `throughput-review`, `setup-review`)
//! through the queue service (goal 182's stage (5)): the command runs here,
//! with its Git, its verification and its agent's process (ADR-t3102-3
//! decision 5), and each operation of the queue it makes is a step of its
//! command the service answers in one transaction as the caller's principal
//! ([`crate::application::supervisor_service::control`]). The process opens
//! no queue, and a service that does not answer ends the command with that
//! error (fail closed).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::OneShot;
use crate::application::integrate::IntegrateTarget;
use crate::application::supervisor_service::{
    ServiceCaller, ServiceTokens, SupervisorQueues,
    control::{self, CONTROL_QUEUE},
};
use crate::application::{AgentProvider, AgentSignals, MainRemote, QueueOpener, SupervisorReads};
use crate::domain::actor_model::ActorLaunch;
use crate::domain::queue_service::{ControlCall, UseCase};
use crate::domain::{ActorContext, ActorRole, TaskId};
use crate::infrastructure::clock;
use crate::infrastructure::queue_service::{Client, ControlCaller, SystemServiceAccess};

/// The queue of a control-side command through the queue service: each
/// operation a step of the command, as the principal of the client's token
/// (none for the user), whose actor the service named.
pub struct ControlQueues {
    pub queues: SupervisorQueues,
    /// The queue's DB the service serves: where its runs', tokens' and
    /// logs' files are, which the command reads and writes itself.
    pub db: PathBuf,
    /// The process of the command, the integrator's of its landing.
    pid: u32,
}

/// The queues of `control`'s steps through `client`: the service names the
/// queue it serves and the principal's actor first, so a principal the
/// command is not for is refused before anything is done. The records of
/// the results the command made are sent again until the service takes
/// them, for as long as it takes ([`control::resending`]).
pub fn control_queues(client: Client, control: ControlCall) -> Result<ControlQueues> {
    let caller: Arc<dyn ServiceCaller> = Arc::new(ControlCaller::new(client, control)?);
    let begun = caller.call(UseCase::Supervisor, json!({"op": CONTROL_QUEUE}))?;
    let db: PathBuf = serde_json::from_value(begun["db"].clone())
        .context("the queue service's answer names no queue")?;
    let role: ActorRole = serde_json::from_value(begun["role"].clone())
        .context("the queue service's answer names no role")?;
    let actor_id = begun["actor_id"]
        .as_str()
        .context("the queue service's answer names no actor")?;
    let caller = control::resending(caller, clock::system());
    Ok(ControlQueues {
        queues: SupervisorQueues::new(Arc::new(caller), ActorContext::new(role, actor_id)),
        db,
        pid: control.pid,
    })
}

/// The queues of the steps of `up`, `down`, `dagq inbox` or `service
/// start|stop` on `caller` ([`ControlCall::command`] one whose
/// [`lifecycle`](crate::domain::queue_service::ControlCommand::lifecycle)
/// holds): the service names the principal's actor first, so a principal
/// the command is not for is refused before anything is done. A record the
/// service does not take is the command's error, sent no more: the command
/// stops rather than waits for a service that is not there, and the queue is
/// not opened instead (fail closed).
pub(super) fn lifecycle_queues(caller: Arc<dyn ServiceCaller>) -> Result<SupervisorQueues> {
    let begun = caller.call(UseCase::Supervisor, json!({"op": CONTROL_QUEUE}))?;
    let role: ActorRole = serde_json::from_value(begun["role"].clone())
        .context("the queue service's answer names no role")?;
    let actor_id = begun["actor_id"]
        .as_str()
        .context("the queue service's answer names no actor")?;
    Ok(SupervisorQueues::new(
        caller,
        ActorContext::new(role, actor_id),
    ))
}

/// [`lifecycle_queues`] of `control`'s steps on `client`'s socket.
pub(super) fn lifecycle_socket(client: Client, control: ControlCall) -> Result<SupervisorQueues> {
    lifecycle_queues(Arc::new(ControlCaller::new(client, control)?))
}

impl ControlQueues {
    /// The tokens of the telemetry of the command's agent or
    /// verification, issued and revoked by the service, their files read
    /// where they are.
    fn tokens(&self) -> ServiceTokens {
        ServiceTokens {
            queues: self.queues.clone(),
            files: Arc::new(SystemServiceAccess),
            telemetry_files: Arc::new(SystemServiceAccess),
        }
    }

    /// The records a landing's container verification reads and makes on
    /// the service's queue ([`super::ContainerRecords::of_service`]).
    fn records(&self) -> super::ContainerRecords {
        super::ContainerRecords::of_service(self.queues.clone(), &self.db)
    }

    /// The main checkout of the repository the queue is bound to, `None`
    /// for one bound to none or whose checkout cannot be found.
    fn checkout(&self) -> Result<Option<PathBuf>> {
        Ok(SupervisorReads::repository_binding(&self.queues.queue())?
            .and_then(|common_dir| super::main_checkout_of(Path::new(&common_dir)).ok()))
    }

    /// `integrate`: [`OneShot::integrate`] on the service's queue, the
    /// landing's lease kept alive and its records made as the integrator
    /// whose token the service issued as its slot was taken.
    pub fn integrate(
        &self,
        target: IntegrateTarget,
        repo: &Path,
        remote: Option<&dyn MainRemote>,
    ) -> Result<Value> {
        let mut queue = self.queues.queue();
        let heartbeat = |_: &crate::application::integrate::IntegrationRequest| {
            let integrator = ActorContext::instance(ActorRole::Integrator, self.pid);
            match self
                .queues
                .landing_token()
                .map(|file| self.queues.service.with_token(&file))
            {
                Some(Ok(caller)) => {
                    Arc::new(SupervisorQueues::new(caller, integrator)) as Arc<dyn QueueOpener>
                }
                Some(Err(error)) => {
                    tracing::warn!(
                        "the integrator's token could not be read for its heartbeat: {error:#}"
                    );
                    Arc::new(self.queues.clone())
                }
                None => Arc::new(self.queues.clone()),
            }
        };
        OneShot::system().integrate_on(
            &mut queue,
            &self.queues.actor,
            &self.db,
            target,
            repo,
            remote,
            &heartbeat,
            &self.tokens(),
            Some(&self.records()),
        )
    }

    /// `review`: [`super::execution::review_on`] the service's queue.
    pub fn review(&self, task: TaskId) -> Result<Value> {
        super::execution::review_on(&mut self.queues.queue(), task, None)
    }

    /// What the observer starts with: `[roles.observer]` of the bound
    /// checkout's `dagq.toml` ([`super::observer_launch`]).
    pub fn observer_launch(&self) -> Result<ActorLaunch> {
        Ok(crate::infrastructure::observer::observer_launch(
            self.checkout()?.as_deref(),
        ))
    }

    /// What the throughput review starts with
    /// ([`super::throughput_review_launch`]).
    pub fn throughput_review_launch(&self) -> Result<ActorLaunch> {
        Ok(crate::infrastructure::throughput_review::review_launch(
            self.checkout()?.as_deref(),
        ))
    }

    /// `observe`: [`super::observe_through`] the service's queue.
    pub fn observe(
        &self,
        provider: &dyn AgentProvider,
        signals: Option<&dyn AgentSignals>,
        options: &crate::application::observer::ObserveOptions,
    ) -> Result<Value> {
        super::observation::observe_with(
            &self.db,
            self.queues.clone(),
            self.tokens(),
            provider,
            signals,
            options,
        )
    }

    /// `throughput-review`: [`super::throughput_review_through`] the
    /// service's queue.
    pub fn throughput_review(
        &self,
        provider: &dyn AgentProvider,
        options: &crate::application::throughput_review::ReviewOptions,
    ) -> Result<Value> {
        super::observation::throughput_review_with(
            &self.db,
            self.queues.clone(),
            self.tokens(),
            provider,
            options,
        )
    }

    /// `setup-review` in `cwd` ([`super::setup_review`]), its job's agent's
    /// events recorded through the service.
    pub fn setup_review(&self, cwd: &Path, options: &super::SetupReviewOptions) -> Result<Value> {
        let events = self.queues.queue();
        super::execution::setup_review_on(&self.db, cwd, options, Some(&events))
    }
}
