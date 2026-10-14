//! The adapter of the [`ContainerRuntime`] port (goal 38) behind the parts
//! that answer before it (the queue's broker, the runs' relays, the Claude
//! credential, the dagq machine and the image): the executors of the
//! container. A worker's turns have theirs
//! ([`crate::application::run_container::PodmanActorExecutor`]), so once
//! every part answers, a worker set to the container can run; the
//! verification of `integrate` has none yet, and the answer says why.

use crate::application::ContainerRuntime;
use crate::domain::container_runtime::{Availability, RuntimeTarget};

/// The container's executors as this dagq has them.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContainerExecutors;

/// Why the verification cannot run in the container.
pub const VERIFICATION_NOT_BUILT: &str = "this dagq has no container executor of the \
verification of integrate yet";

impl ContainerRuntime for ContainerExecutors {
    fn availability(&self, target: RuntimeTarget) -> Availability {
        match target {
            RuntimeTarget::Worker => Availability::Available,
            RuntimeTarget::Verification => {
                Availability::Unavailable(VERIFICATION_NOT_BUILT.to_owned())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worker_can_run_once_its_parts_answer_and_the_verification_cannot_and_says_why() {
        assert_eq!(
            ContainerExecutors.availability(RuntimeTarget::Worker),
            Availability::Available
        );
        assert_eq!(
            ContainerExecutors.availability(RuntimeTarget::Verification),
            Availability::Unavailable(VERIFICATION_NOT_BUILT.to_owned())
        );
    }
}
