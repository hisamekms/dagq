//! The helper that keeps the tests' `dagq` from running as the actor of the
//! session running the tests (task 789).

use crate::common::{
    WithoutActor,
    actor::{ACTOR_ENV, TURN_ENV},
};
use dagq::domain::{ActorContext, RunId, TaskId};
use std::{collections::HashMap, process::Command};

/// What the command sets or removes itself: `Some` a value, `None` removed.
fn explicit(command: &Command) -> HashMap<String, Option<String>> {
    command
        .get_envs()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.map(|value| value.to_string_lossy().into_owned()),
            )
        })
        .collect()
}

#[test]
fn the_actor_env_is_the_callers_identity() {
    assert_eq!(
        ACTOR_ENV,
        [
            "DAGQ_ROLE",
            "DAGQ_ACTOR_ID",
            "DAGQ_RUN_ID",
            "DAGQ_TASK_ID",
            "DAGQ_SESSION_KIND",
            "DAGQ_PLANNER_ID",
            "DAGQ_PLANNER_ORIGIN",
        ]
    );
    // Everything the runtime gives a worker to name it is dropped.
    let worker = ActorContext::worker(&RunId::new("run-1").unwrap(), TaskId::new(3));
    for (name, _) in worker.env() {
        assert!(ACTOR_ENV.contains(&name.as_str()), "{name}");
    }
}

#[test]
fn the_actor_env_is_dropped_unless_the_command_sets_it() {
    let mut command = Command::new("env");
    command
        .env("DAGQ_ROLE", "observer")
        .env("DAGQ_QUEUE", "queue.db")
        .without_actor_env()
        .env("DAGQ_ACTOR_ID", "observer:1");
    let env = explicit(&command);
    assert_eq!(env["DAGQ_ROLE"].as_deref(), Some("observer"));
    assert_eq!(env["DAGQ_ACTOR_ID"].as_deref(), Some("observer:1"));
    assert_eq!(env["DAGQ_QUEUE"].as_deref(), Some("queue.db"));
    for name in &ACTOR_ENV[2..] {
        assert_eq!(env[*name], None, "{name}");
    }
    // So do the queue service's socket and token file of a client-mode
    // dagq (goal 82's stage (3)).
    assert_eq!(
        TURN_ENV,
        ["DAGQ_SERVICE_SOCKET", "DAGQ_SERVICE_CREDENTIAL_FILE"]
    );
    for name in TURN_ENV {
        assert_eq!(env[name], None, "{name}");
    }
}
