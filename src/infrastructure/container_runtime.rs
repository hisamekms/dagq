//! The adapter of the [`ContainerRuntime`] port (goal 38) before the
//! container runtime's parts exist: the dagq machine, the image, the
//! queue's broker, the relay and the long-lived token are each a later
//! task's, which answers for its part here. Until then nothing set to the
//! container can run, and the answer says why.

use crate::application::ContainerRuntime;
use crate::domain::container_runtime::{Availability, RuntimeTarget};

/// The container runtime with none of its parts built: it can never be
/// used, and says so.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnbuiltContainer;

/// Why [`UnbuiltContainer`] cannot be used.
pub const CONTAINER_NOT_BUILT: &str = "this dagq has no container runtime yet (the dagq machine, \
the image, the queue broker, the relay and the long-lived token are not built)";

impl ContainerRuntime for UnbuiltContainer {
    fn availability(&self, _target: RuntimeTarget) -> Availability {
        Availability::Unavailable(CONTAINER_NOT_BUILT.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_its_parts_the_container_can_never_be_used_and_says_why() {
        for target in RuntimeTarget::ALL {
            assert_eq!(
                UnbuiltContainer.availability(target),
                Availability::Unavailable(CONTAINER_NOT_BUILT.to_owned())
            );
        }
    }
}
