//! The actor a test's `dagq` runs as (task 789).
//!
//! `dagq` authorizes a command by the caller's actor, which it reads from the
//! env (`DAGQ_ROLE` and the rest). A test that starts the binary, or a stub
//! that runs it, would otherwise pass on whatever actor runs the tests: a
//! worker's session that runs them would make every such `dagq` a worker.
//! Every such `Command` goes through [`WithoutActor::without_actor_env`], and
//! a test that runs `dagq` as a given actor sets that env on the command.

use std::process::Command;

/// The env of the caller's actor, which the test process's `dagq` children
/// do not inherit: the runtime's own list (task 902).
pub use dagq::domain::actor::ACTOR_ENV;

/// What a worker's or a job's env adds besides its actor, which the children
/// do not inherit either: the queue service's socket and token file of a
/// client-mode `dagq` (goal 82's stage (3)), with which tests run by a
/// worker would have every `dagq` they start go to that worker's queue
/// service. They name no actor, so they are not in the runtime's
/// [`ACTOR_ENV`].
pub const TURN_ENV: [&str; 2] = [
    dagq::domain::queue_service::SOCKET_ENV,
    dagq::domain::queue_service::CREDENTIAL_FILE_ENV,
];

/// The queue service's replacement grace period
/// ([`dagq::application::queue_service::REPLACEMENT_WAIT_ENV`]), in
/// milliseconds, that the `it` binary's process runs with and its `dagq`
/// children inherit (set as the binary loads, in `tests/it/main.rs`):
/// short, so that a command or an in-process call that meets a stopped or
/// missing service says so within half a second instead of production's
/// 30. A test that waits through a real replacement sets a longer one on
/// the `dagq` it starts, or on its own in-process call
/// ([`super::lifecycle::on_socket_through_replacements`]), and on nothing
/// else in the process.
pub const TEST_REPLACEMENT_WAIT_MS: u64 = 500;

/// The lock-free bulk-write pause inherited by this binary's children
/// and used by its in-process stores: short because their large payloads
/// check the socket boundary, not SQLite's busy-wait scheduling. Set as
/// the `it` binary loads, beside [`TEST_REPLACEMENT_WAIT_MS`].
pub const TEST_BATCH_PAUSE_MS: u64 = 1;

pub trait WithoutActor {
    /// Drops the [`ACTOR_ENV`] and [`TURN_ENV`] the command would inherit
    /// from the test process. A variable the command sets itself (the worker env of a stub
    /// agent's spec, a test's `env("DAGQ_ROLE", ..)`) stays, whether set
    /// before or after.
    fn without_actor_env(&mut self) -> &mut Self;
}

impl WithoutActor for Command {
    fn without_actor_env(&mut self) -> &mut Self {
        for name in ACTOR_ENV.into_iter().chain(TURN_ENV) {
            let set = self.get_envs().any(|(key, _)| key == name);
            if !set {
                self.env_remove(name);
            }
        }
        self
    }
}
