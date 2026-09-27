//! The names `up` and `down` had before they moved to `application`
//! (ADR-0013), kept for the tests: the use cases and their types are
//! [`crate::application::lifecycle`] and the entry points
//! [`crate::compose::up`] and [`crate::compose::down`].
pub use crate::application::lifecycle::{
    DETACHED_CMUX_HINT, DownOptions, Handed, INBOX_ROLE, LAUNCHD_LOG_NAME, OBSERVER_ROLE,
    PLANNER_ROLE, QUEUE_ENV, QueueWorkspaces, REVIEWER_ROLE, ROLE_ENV, ROLE_STATUS_KEY,
    UpEnvironment, UpOptions, WORKER_ROLE, hand_off, handoff_failures, launch_agent_spec,
    session_look, supervise_command, untrusted_repository_hint,
};
pub use crate::compose::{COMMAND_TARGET, PlanOptions, down, inbox_command, plan, planners, up};
