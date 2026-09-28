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
/// do not inherit.
pub const ACTOR_ENV: [&str; 7] = [
    "DAGQ_ROLE",
    "DAGQ_ACTOR_ID",
    "DAGQ_RUN_ID",
    "DAGQ_TASK_ID",
    "DAGQ_SESSION_KIND",
    "DAGQ_PLANNER_ID",
    "DAGQ_PLANNER_ORIGIN",
];

pub trait WithoutActor {
    /// Drops the [`ACTOR_ENV`] the command would inherit from the test
    /// process. A variable the command sets itself (the worker env of a stub
    /// agent's spec, a test's `env("DAGQ_ROLE", ..)`) stays, whether set
    /// before or after.
    fn without_actor_env(&mut self) -> &mut Self;
}

impl WithoutActor for Command {
    fn without_actor_env(&mut self) -> &mut Self {
        for name in ACTOR_ENV {
            let set = self.get_envs().any(|(key, _)| key == name);
            if !set {
                self.env_remove(name);
            }
        }
        self
    }
}
