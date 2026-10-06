//! The screens and inputs of sessions, which no session of the runtime
//! has any more. A run has no screen: `run screen` is refused with the
//! reason and the turns' log CLI (`run log`, ADR-t1433-3), `run
//! close-workspaces` is refused as there is no run workspace, and `run
//! send` is refused and points to `answer`, whose answer the supervisor
//! delivers as a turn. A planner has none either (ADR-t1433-2): `planner
//! screen` reads nothing and says where its turns are, and `planner send`
//! is refused with the reason and what to do instead. Nothing here uses
//! cmux or records `screen_read` / `screen_input_sent`. Authorization
//! remains the caller's responsibility (`screen.read`, `screen.send`).

use std::path::Path;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::planner::planner_dir;
use super::{Queue, SessionRegistry};
use crate::domain::{PlannerId, PlannerRoute, Resource, RunId, TaskId, TaskRun, turn::turns_dir};

/// The lines `run screen` and `planner screen` accept and ignore by
/// default.
pub const DEFAULT_LINES: usize = 40;

/// The run a `run screen` / `run send` names: a run id, or a task id for
/// the task's latest run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunTarget {
    Run(RunId),
    Task(TaskId),
}

impl RunTarget {
    /// A number is a task id; anything else a run id.
    pub fn parse(text: &str) -> Result<Self> {
        match text.trim().parse::<i64>() {
            Ok(id) => Ok(Self::Task(TaskId::new(id))),
            Err(_) => Ok(Self::Run(RunId::new(text.trim())?)),
        }
    }

    /// What the command acts on for the authorizer.
    pub fn resource(&self) -> Resource {
        match self {
            Self::Run(id) => Resource::run(id.clone()),
            Self::Task(id) => Resource::task(*id),
        }
    }
}

/// The run `target` names: the run, or its task's latest run.
pub(crate) fn resolve_run(queue: &mut dyn Queue, target: &RunTarget) -> Result<TaskRun> {
    match target {
        RunTarget::Run(id) => queue.run(id),
        RunTarget::Task(id) => match queue.show(*id)?.runs.pop() {
            Some(run) => Ok(run),
            None => bail!("task {id} has no run"),
        },
    }
}

/// Why `run screen` is refused: no run's session has a screen to read
/// (ADR-t1433-3 decision 4). The turns' log CLI takes its place.
pub const RUN_SCREEN_REFUSED: &str = "run screen is refused: a run's session runs in the background without a screen (ADR-t1433-3; the interactive worker was retired, task 1437)";

/// Why `run close-workspaces` is refused: the runtime opens no workspace
/// for a run, so there is none to clean up (ADR-t1433-3 decision 3).
pub const CLOSE_WORKSPACES_REFUSED: &str = "run close-workspaces is refused: the runtime opens no workspace for a run any more and stops a run's background wrapper itself (ADR-t1433-3); close a workspace a run opened before in your own terminal";

/// `run screen`: refused for every run, with the reason and the turns' log
/// CLI (`dagq run log`) that replaced it (ADR-t1433-3 decision 4). The run
/// is resolved first, so that an unknown run or task is the queue's error.
/// Nothing is read or recorded, and cmux is not needed.
pub fn run_screen(queue: &mut dyn Queue, target: &RunTarget) -> Result<Value> {
    let run = resolve_run(queue, target)?;
    let turns = run
        .run_dir()
        .map(|dir| format!("; its turns are in {dir}/turns"))
        .unwrap_or_default();
    bail!(
        "{RUN_SCREEN_REFUSED}; read the turns of run {} with `dagq run log {}` (`--follow` to keep reading){turns}",
        run.id(),
        run.id()
    )
}

/// `run send`: refused for every run (task 1437). Answers are delivered by
/// the supervisor as the session's next turn, through `answer`.
pub fn run_send(queue: &mut dyn Queue, target: &RunTarget) -> Result<Value> {
    let run = resolve_run(queue, target)?;
    bail!(
        "run {} has no interactive input: run send takes no keys or answers; answer its asks with `answer`, and the supervisor delivers the answer as the next turn (read its turns in {}/turns)",
        run.id(),
        run.run_dir().unwrap_or("its run directory")
    )
}

/// `planner screen`: a planner's session has no screen to read (a planner
/// of the runtime's runs headless, ADR-t1394-2 decision 3 and ADR-t1433-2
/// decision 3; the terminal of a person's planner is not read any more):
/// the reply says so and where its turns are (`turns/` of its directory
/// under `planners_dir`). Nothing is read or recorded, and cmux is not
/// needed. An unknown planner is the registry's error.
pub fn planner_screen(
    registry: &dyn SessionRegistry,
    planners_dir: &Path,
    planner: PlannerId,
) -> Result<Value> {
    let session = registry.planner(planner)?;
    let reason = if session.route == PlannerRoute::Headless {
        "a headless planner has no screen"
    } else {
        "the screen of a planner in a workspace is not read any more (ADR-t1433-2); read it in its workspace in your own terminal"
    };
    Ok(json!({
        "planner_id": planner,
        "route": session.route,
        "screen": null,
        "reason": reason,
        "turns": turns_dir(&planner_dir(planners_dir, planner)).display().to_string(),
    }))
}

/// `planner send`: refused for every planner (ADR-t1433-2), with the
/// reason and what to do instead: the answer of a `planner_question` goes
/// through `answer`, and the supervisor delivers it (to a headless planner
/// as its next turn); a headless planner takes a follow-up through
/// `planner request`. Nothing is typed or recorded, and cmux is not
/// needed. An unknown planner is the registry's error.
pub fn planner_send(registry: &dyn SessionRegistry, planner: PlannerId) -> Result<Value> {
    let session = registry.planner(planner)?;
    if session.route == PlannerRoute::Headless {
        bail!(
            "planner {planner} is headless: its session has no screen and takes no keys or answers (the supervisor delivers the answer of its question as its next turn; hand it a follow-up with `planner request`)"
        );
    }
    bail!(
        "planner {planner} takes no keys or answers: nothing is typed into a planner's terminal any more (ADR-t1433-2); answer its question with `answer`, and the supervisor delivers the answer"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_is_a_task_by_number_and_a_run_otherwise() {
        assert_eq!(
            RunTarget::parse("12").unwrap(),
            RunTarget::Task(TaskId::new(12))
        );
        let run = RunTarget::parse("5b7e2393-1023").unwrap();
        assert!(matches!(run, RunTarget::Run(_)));
        assert!(matches!(run.resource(), Resource::Run { .. }));
        assert!(matches!(
            RunTarget::parse("7").unwrap().resource(),
            Resource::Task { .. }
        ));
    }
}
