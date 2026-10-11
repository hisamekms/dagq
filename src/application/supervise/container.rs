//! The targets `dagq.toml` sets to the container (goal 38): read every
//! pass with whether the container runtime can run each
//! ([`ContainerRuntime`]), recorded on the queue as
//! `container_runtime_unavailable` (the inbox's attention) and
//! `container_runtime_available` where the answer differs from the latest
//! one. While the worker's container cannot be used, no task is claimed
//! (each candidate's claim is deferred, `container_unavailable`) and no run
//! is resumed; a worker that would run on Codex waits the same way, as the
//! container does not reach it (`container_provider_unsupported`,
//! ADR-t2114-1 decision 2). While the verification's cannot, no run lands.
//! Nothing set to the container runs on the host instead (ADR-t2113-1
//! decision 4).

use super::*;
use crate::domain::container_runtime::{
    CONTAINER_RUNTIME_KINDS, ContainerCheck, RuntimeTarget, worker_deferral,
};

impl Supervisor<'_> {
    /// Read the runtimes of `dagq.toml` again and ask the container about
    /// each target set to it. A `dagq.toml` that cannot be read keeps the
    /// last answer: provisioning and `integrate` report that file
    /// themselves.
    pub(super) fn check_container_runtime(&mut self) -> Result<()> {
        let runtimes = match self.verifier.runtimes() {
            Ok(runtimes) => runtimes,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the runtimes of dagq.toml could not be read: {error:#}; keeping those in use");
                return Ok(());
            }
        };
        let container = self.container.clone();
        let check = ContainerCheck::of(&runtimes, |target| container.availability(target));
        let last = self.queue.latest_queue_event(&CONTAINER_RUNTIME_KINDS)?;
        if let Some((kind, mut payload)) =
            check.transition(last.as_ref().map(|e| (e.kind.as_str(), &e.payload)))
        {
            payload["supervisor"] = json!(self.registration.token);
            self.queue.record_queue_event(kind, payload)?;
            match check.message() {
                Some(message) => warn!("{message}"),
                None => info!(
                    "the container runtime dagq.toml sets can be used, or none is set; its claims, resumes and landings go on"
                ),
            }
        }
        self.landing.runtimes = runtimes;
        self.landing.container = check;
        Ok(())
    }

    /// Why a worker that would run on `provider` is not claimed or
    /// resumed here for its runtime, `None` when it may run
    /// ([`worker_deferral`]).
    pub(super) fn container_deferral(&self, provider: Provider) -> Option<&'static str> {
        worker_deferral(
            self.landing.runtimes.worker,
            self.landing.container.why(RuntimeTarget::Worker),
            provider,
        )
    }

    /// Whether the verification of `integrate` is set to a container that
    /// cannot be used: no run lands meanwhile.
    pub(super) fn verification_container_held(&self) -> bool {
        self.landing
            .container
            .why(RuntimeTarget::Verification)
            .is_some()
    }
}
