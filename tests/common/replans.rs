//! The queue of the replanning tests (`tests/it/task_replanning.rs`,
//! `tests/it/replan_resolve.rs`): a claimed run of a task with a successor,
//! the CLI as a role, a person's request, the denials recorded and the
//! trusted runtime's moves of an operation.

use std::path::Path;

use dagq::{
    application::{TaskStore, commands::replans::Replans},
    domain::{
        ActorContext, ActorRole, ClaimOutcome, ReplanId, RunId, StaticPolicy, TaskAction, TaskId,
        task_replan::{ReplanMove, ReplanStatus},
    },
    infrastructure::sqlite::SqliteQueue,
};
use rusqlite::Connection;
use serde_json::Value;

use super::cli::invoke_with;
use super::queue::{base, new_goal, new_task};

/// A goal with task A (two acceptance items) claimed as a run, and task B
/// that waits for A: the queue, A, B and A's run.
pub fn claimed(db: &Path) -> (SqliteQueue, TaskId, TaskId, RunId) {
    let mut queue = SqliteQueue::open(db).unwrap();
    let goal = queue.add_goal(new_goal("replanning")).unwrap().id();
    let a = queue
        .add(dagq::domain::NewTask {
            acceptance: "the API behaves\nthe operations are documented".into(),
            goal_id: Some(goal),
            ..new_task("A")
        })
        .unwrap()
        .id();
    let b = queue
        .add(dagq::domain::NewTask {
            dependencies: vec![a],
            goal_id: Some(goal),
            ..new_task("B")
        })
        .unwrap()
        .id();
    for task in [a, b] {
        queue.transition(task, TaskAction::BypassReview).unwrap();
    }
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!("claim")
    };
    assert_eq!(run.task_id(), a);
    let run = run.id().clone();
    (queue, a, b, run)
}

/// `dagq` as `role` (with a worker's run and task; a person at a plain
/// terminal for `user`), its JSON and whether it succeeded.
pub fn as_role(
    db: &Path,
    role: &str,
    worker: Option<(&RunId, TaskId)>,
    args: &[&str],
) -> (bool, Value) {
    let task = worker.map(|(_, task)| task.to_string());
    let mut env = Vec::new();
    if role != "user" {
        env.push(("DAGQ_ROLE", role));
    }
    if let (Some((run, _)), Some(task)) = (worker, task.as_deref()) {
        env.extend([
            ("DAGQ_RUN_ID", run.as_str()),
            ("DAGQ_TASK_ID", task),
            ("DAGQ_ACTOR_ID", run.as_str()),
        ]);
    }
    let output = invoke_with(&env, db, args);
    let bytes = if output.status.success() {
        &output.stdout
    } else {
        &output.stderr
    };
    (
        output.status.success(),
        serde_json::from_slice(bytes).unwrap_or(Value::Null),
    )
}

pub fn request(run: &RunId, reason: &str) -> Vec<String> {
    [
        "replan",
        "request",
        "--run",
        run.as_str(),
        "--reason",
        reason,
        "--evidence",
        "1",
    ]
    .map(str::to_owned)
    .to_vec()
}

pub fn args(words: &[String]) -> Vec<&str> {
    words.iter().map(String::as_str).collect()
}

pub fn denials(db: &Path, capability: &str) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM run_events WHERE kind='authorization_denied'
             AND json_extract(payload,'$.capability')=?1",
            [capability],
            |r| r.get(0),
        )
        .unwrap()
}

/// Move operation `id` as the trusted runtime does (the supervisor).
pub fn park(queue: &mut SqliteQueue, id: ReplanId, generation: i64, to: ReplanStatus) -> i64 {
    let supervisor = ActorContext::instance(ActorRole::Supervisor, 1);
    Replans::new(queue, &supervisor, &StaticPolicy)
        .advance(
            id,
            generation,
            &ReplanMove {
                to,
                reason: format!("diagnosed: {}", to.as_str()),
                snapshot_id: None,
                diagnosis: None,
            },
        )
        .unwrap()
        .generation
}
