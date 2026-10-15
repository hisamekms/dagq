//! The adapter of the [`ContainerRuntime`] port (goal 38) behind the parts
//! that answer before it (the queue's broker, the runs' relays, the Claude
//! credential, the dagq machine and the image): the executors of the
//! container. A worker's turns have theirs
//! ([`crate::application::run_container::PodmanActorExecutor`]) and so
//! does the verification of `integrate`
//! ([`crate::application::run_container::VerificationContainers`]), so
//! once every part answers, either target set to the container can run.

use crate::application::ContainerRuntime;
use crate::domain::container_runtime::{Availability, RuntimeTarget};

/// The container's executors as this dagq has them.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContainerExecutors;

impl ContainerRuntime for ContainerExecutors {
    fn availability(&self, _: RuntimeTarget) -> Availability {
        Availability::Available
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worker_and_the_verification_can_run_once_their_parts_answer() {
        for target in [RuntimeTarget::Worker, RuntimeTarget::Verification] {
            assert_eq!(
                ContainerExecutors.availability(target),
                Availability::Available
            );
        }
    }
}
