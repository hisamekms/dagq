//! Shared task, goal, event and note SQL on the execution port. Native
//! transaction entries and compatibility wrappers live in `sqlite`.
use super::sql_dialect::JsonType;
use super::sql_exec::{OptionalRow, SqlRead, SqlResult, SqlRow, SqlValue, sql_params};
use super::store_rows::{
    RecordedCancel, cancel_of, enum_col, event_row, goal_row, optional_enum_col,
    optional_priority_col, read_goal, read_task, ready_conditions, replan_guarding, run_row,
    select_tasks, task_row,
};
use super::store_writes::{
    Node, event, event_id, insert_dependency, insert_goal_dependency, insert_task, next_id,
    request_priority, touch, waits_for,
};
use crate::domain::claim_facts;
use crate::domain::event_kind::{self, EventKind};
use crate::domain::task_replan::GuardedStep;
use std::{collections::BTreeMap, path::Path, time::SystemTime};

use anyhow::{Context, Result, ensure};
use serde_json::json;

use crate::{
    application::{
        Generators, GraphGoalDependency, GraphInput, GraphTask, IdGenerator, LatestRun, ListedHold,
        ListedReplan, StatusFilter, TaskListItem, TaskPage, TaskQuery, dependency_graph, timestamp,
    },
    domain::{
        ChangeSet, ClaimOutcome, CommitSha, EventId, Goal, GoalDetail, GoalEdit, GoalId,
        GoalPredecessor, GoalSummary, GoalTask, GoalVerdict, LintInput, LintNode, NewGoal, NewNote,
        NewTask, NotePage, NoteQuery, NoteTarget, OBSERVATION_KIND, PlannerOrigin, Predecessor,
        Priority, ProposalId, RequestId, RunEvent, RunId, TagSet, Task, TaskAction,
        TaskAuthorization, TaskChange, TaskDetail, TaskEdit, TaskId, TaskRun, TaskStatus,
        TaskStatusCounts, TitlePrefixes,
        actor::ActorRole,
        authorization::{PriorityHolder, check_priority_change},
        cancel_reason::{CancelRecord, NewCancel},
        goal,
        plan_request::{
            CreatingPlanner, Creator, PriorityBy, creation_origin, goal_priority_at_creation,
        },
        provider_switch::{self, SwitchPhase, WorkerRoute},
        provider_trial, task,
        worker::{Worker, WorkerMode},
        worker_model::{self, WorkerTrial},
    },
};

const GOAL_DEPENDENCIES_QUERY: &str =
    "SELECT goal_id FROM task_goal_dependencies WHERE task_id=?1 ORDER BY goal_id";

const EDITABLE_TASK_FIELDS: [&str; 12] = [
    "title",
    "description",
    "acceptance",
    "verification_commands",
    "required_evidence",
    "paths",
    "context",
    "change",
    "provider",
    "worker_mode",
    "wait_for_build",
    "execution_class",
];

/// The ready tasks ([`ready_conditions`]) that no open hold keeps
/// (ADR-t1879-1).
/// In ID order: the claim order (ADR-0040 decision 4) needs the whole
/// dependency graph, so [`claim_order`] applies it, not SQL.
const READY_QUERY: &str = concat!(
    select_tasks!(),
    ready_conditions!(),
    "
      AND NOT EXISTS (
        SELECT 1 FROM task_holds h WHERE h.task_id = t.id AND h.released_at IS NULL
      )
    ORDER BY t.id"
);

/// The role of the actor of `conn`'s writes.
pub(super) fn actor_role(conn: &impl SqlRead) -> Result<ActorRole> {
    let role: String = conn.query_row("SELECT dagq_actor_role()", sql_params![], |r| r.get(0))?;
    Ok(role.parse()?)
}

/// Who creates a goal or task as the actor of `conn`'s writes
/// (ADR-t1975-1 decision 5): the user or the inbox, a planner by its row
/// (`planner:<id>`), or any other actor.
pub(super) fn creator_of_actor(conn: &impl SqlRead) -> Result<Creator> {
    let (role, actor): (String, String) = conn.query_row(
        "SELECT dagq_actor_role(), dagq_actor_id()",
        sql_params![],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if crate::domain::plan_review::is_person_actor(&role) {
        return Ok(Creator::Person);
    }
    if role != crate::domain::ActorRole::Planner.as_str() {
        return Ok(Creator::Other);
    }
    let Some(planner) = actor
        .strip_prefix("planner:")
        .and_then(|id| id.parse::<i64>().ok())
    else {
        return Ok(Creator::Planner(CreatingPlanner::default()));
    };
    let row = conn
        .query_row(
            "SELECT p.origin, p.request_id, p.finding_id IS NOT NULL,
               coalesce(p.draft_task_id, (SELECT min(m.task_id) FROM draft_bundle_members m
                   WHERE m.planner_id = p.id)),
               (SELECT min(l.request_id) FROM plan_request_proposals l
                   WHERE l.proposal_id = p.proposal_id)
             FROM planners p WHERE p.id = ?1",
            sql_params![planner],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<RequestId>>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, Option<TaskId>>(3)?,
                    r.get::<_, Option<RequestId>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((opened_by, request, finding, draft, revised_request)) = row else {
        return Ok(Creator::Planner(CreatingPlanner::default()));
    };
    let draft = match draft {
        Some(task) => {
            Some(super::draft_planners::draft_origin_port(conn, task)?.map(|(origin, _)| origin))
        }
        None => None,
    };
    Ok(Creator::Planner(CreatingPlanner {
        opened_by: opened_by.parse::<PlannerOrigin>().ok(),
        request,
        revised_request,
        finding,
        draft,
    }))
}

pub(super) fn goal_task_row(row: &SqlRow<'_>) -> SqlResult<GoalTask> {
    let own = optional_priority_col(row, "priority")?;
    let (priority, priority_source) =
        crate::domain::base_priority(own, optional_priority_col(row, "goal_priority")?);
    Ok(GoalTask {
        id: row.get("id")?,
        title: row.get("title")?,
        status: enum_col(row, "status")?,
        priority,
        priority_source,
        priority_by: PriorityBy::effective(
            own,
            optional_enum_col(row, "priority_by")?,
            optional_enum_col(row, "goal_priority_by")?,
        ),
    })
}

/// The tasks `query` selects by `key`, each with the run that landed it.
pub(super) fn landed_tasks(
    conn: &impl SqlRead,
    runs_dir: &Path,
    query: &str,
    key: i64,
) -> Result<Vec<Predecessor>> {
    let tasks: Vec<Task> = conn.query_map(query, sql_params![key], task_row)?;
    tasks
        .into_iter()
        .map(|task| {
            // At most one run per task is integrated (`one_integrated_run_per_task`).
            let integrated_run = conn
                .query_row(
                    "SELECT * FROM task_runs WHERE task_id = ?1 AND status = 'integrated'",
                    sql_params![task.id()],
                    run_row(runs_dir),
                )
                .optional()?;
            Ok(Predecessor {
                task,
                integrated_run,
            })
        })
        .collect()
}

/// Close `goal_id` with `verdict` as `goal close` does (`achieved` only
/// when its follow-ups' membership is settled), in the caller's
/// transaction: the one check of `goal close`, a goal review's `achieved`
/// and an `approve_goal` answer alike. Records
/// `goal_closed` with the task counts and the fields of `extra` (who closed
/// it and why), and `dependency_stranded` on each task an `abandoned` close
/// leaves that others wait on. A goal review's `achieved` passes `by:
/// "goal_review"`, its `goal_review_id` and `reason` (the verdict's
/// `summary`); an `approve_goal` answer passes `by: "person"`, the
/// `ask_id` answered and the `goal_review_id` of the review that asked.
pub(super) fn close_goal_in(
    conn: &impl crate::infrastructure::sql_exec::SqlWrite,
    goal_id: GoalId,
    verdict: GoalVerdict,
    now: &str,
    extra: serde_json::Value,
) -> Result<()> {
    let goal = read_goal(conn, goal_id)?;
    let counts = task_counts(conn, goal_id)?;
    let replacing = super::replan_apply::goal_replacing_port(conn, goal_id)?;
    let closed = goal::close(goal, verdict, &counts, &replacing, now.to_owned())?;
    // The follow-ups found from it, read in the same transaction as the
    // close, wherever they belong now (ADR-t1504-2 decision 8).
    goal::check_follow_ups(
        goal_id,
        verdict,
        &super::follow_up_membership::source_follow_ups(conn, goal_id)?,
    )?;
    // A broken replacement record is never resolved (ADR-t1521-2
    // decision 5): the replaced task's own work did not land.
    goal::check_replacements(
        goal_id,
        verdict,
        &super::replan_apply::goal_unresolved_port(conn, goal_id)?,
    )?;
    // `closed_at IS NULL` only detects a concurrent close; the domain
    // decided whether this one may happen.
    let changed = conn.execute(
        "UPDATE goals SET closed_at=?1, verdict=?2, updated_at=?3
         WHERE id=?4 AND closed_at IS NULL",
        sql_params![
            closed.closed_at(),
            closed.verdict().map(GoalVerdict::as_str),
            closed.updated_at(),
            goal_id
        ],
    )?;
    ensure!(changed == 1, "goal {goal_id} was closed concurrently");
    let mut payload = json!({"verdict": verdict, "tasks": counts});
    if let (Some(payload), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
        payload.extend(extra.clone());
    }
    goal_event(conn, goal_id, EventKind::GoalClosed, payload)?;
    // `abandoned` leaves unstarted tasks as they are; one another goal's
    // task waits on is told to the inbox (task 421).
    if verdict == GoalVerdict::Abandoned {
        super::stranded::record_abandoned(conn, goal_id)?;
    }
    Ok(())
}

pub(super) fn task_counts(conn: &impl SqlRead, goal_id: GoalId) -> Result<TaskStatusCounts> {
    let mut counts = TaskStatusCounts::default();
    let rows = "SELECT status, count(*) AS n FROM tasks WHERE goal_id=?1 GROUP BY status";
    for row in conn.query_map(rows, sql_params![goal_id], |row| {
        Ok((
            enum_col::<TaskStatus>(row, "status")?,
            row.get::<_, i64>("n")?,
        ))
    })? {
        let (status, n) = row;
        counts.count(status, usize::try_from(n)?);
    }
    Ok(counts)
}

pub(super) fn goal_event(
    conn: &impl crate::infrastructure::sql_exec::SqlWrite,
    goal_id: GoalId,
    kind: EventKind,
    payload: serde_json::Value,
) -> Result<()> {
    goal_event_id(conn, goal_id, kind, payload).map(drop)
}

/// [`goal_event`], returning the ID of the event it inserted.
pub(super) fn goal_event_id(
    conn: &impl crate::infrastructure::sql_exec::SqlWrite,
    goal_id: GoalId,
    kind: EventKind,
    payload: serde_json::Value,
) -> Result<EventId> {
    let id = EventId::new(conn.insert_id(
        "INSERT INTO run_events(goal_id,kind,payload,actor_role,actor_id,requested_by)
         VALUES (?1,?2,dagq_event_body(?3),dagq_actor_role(),dagq_actor_id(),dagq_requested_by())",
        sql_params![goal_id, kind.as_str(), serde_json::to_string(&payload)?],
    )?);
    // The goal review's session span this event starts or ends (ADR-0048).
    super::sessions::follow_goal(conn, id, goal_id, kind, &payload)?;
    Ok(id)
}

/// The claimable tasks, in ID order.
pub(super) fn ready_tasks(conn: &impl SqlRead) -> Result<Vec<Task>> {
    Ok(conn.query_map(READY_QUERY, sql_params![], task_row)?)
}

/// The unfinished tasks with their predecessors, goal dependencies and
/// priorities, and the IDs of the claimable ones, read inside the caller's
/// transaction so they are one snapshot.
pub(super) fn read_graph_input(conn: &impl SqlRead) -> Result<GraphInput> {
    let tasks: Vec<Task> = conn.query_map(
        concat!(
            select_tasks!(),
            " WHERE t.status IN ('draft','submitted','ready','in_progress') ORDER BY t.id"
        ),
        sql_params![],
        task_row,
    )?;
    let dependencies =
        "SELECT predecessor_id FROM task_dependencies WHERE task_id=?1 ORDER BY predecessor_id";
    let goal_dependencies =
        "SELECT g.* FROM task_goal_dependencies d JOIN goals g ON g.id = d.goal_id
         WHERE d.task_id=?1 ORDER BY g.id";
    let tasks = tasks
        .into_iter()
        .map(|task| {
            let goal_status = task
                .goal_id()
                .map(|goal_id| read_goal(conn, goal_id).map(|goal| goal.status()))
                .transpose()?;
            Ok(GraphTask {
                depends_on: conn.query_map(dependencies, sql_params![task.id()], |r| r.get(0))?,
                goal_dependencies: conn
                    .query_map(goal_dependencies, sql_params![task.id()], goal_row)?
                    .into_iter()
                    .map(|goal| GraphGoalDependency {
                        goal_id: goal.id(),
                        verdict: goal.verdict(),
                    })
                    .collect(),
                goal_status,
                id: task.id(),
                status: task.status(),
                priority: task.priority(),
                priority_source: task.priority_source(),
                priority_by: task.priority_by(),
                execution_class: task.execution_class(),
                goal_id: task.goal_id(),
                title: task.into_title(),
            })
        })
        .collect::<Result<_>>()?;
    let candidates = conn.query_map(READY_QUERY, sql_params![], |r| r.get("id"))?;
    let canceled = conn.query_map(
        "SELECT DISTINCT d.predecessor_id FROM task_dependencies d
             JOIN tasks t ON t.id = d.task_id
             JOIN tasks p ON p.id = d.predecessor_id
             WHERE p.status = 'canceled'
               AND t.status IN ('draft','submitted','ready','in_progress')
             ORDER BY d.predecessor_id",
        sql_params![],
        |r| r.get(0),
    )?;
    Ok(GraphInput {
        tasks,
        candidates,
        canceled,
    })
}

/// The queue as `lint` reads it: `targets` in the order given and every
/// task's status and dependencies, and every goal's verdict.
pub(super) fn read_lint_input(conn: &impl SqlRead, targets: &[TaskId]) -> Result<LintInput> {
    let targets: Vec<Task> = targets
        .iter()
        .map(|&id| read_task(conn, id))
        .collect::<Result<_>>()?;
    let mut nodes: BTreeMap<TaskId, LintNode> = conn
        .query_map(
            "SELECT id, status, goal_id FROM tasks",
            sql_params![],
            |r| {
                Ok((
                    r.get("id")?,
                    LintNode {
                        status: enum_col(r, "status")?,
                        goal_id: r.get("goal_id")?,
                        depends_on: Vec::new(),
                        goal_dependencies: Vec::new(),
                    },
                ))
            },
        )?
        .into_iter()
        .collect();
    let edges: Vec<(TaskId, TaskId)> = conn.query_map(
        "SELECT task_id, predecessor_id FROM task_dependencies ORDER BY predecessor_id",
        sql_params![],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    for (task_id, predecessor_id) in edges {
        if let Some(node) = nodes.get_mut(&task_id) {
            node.depends_on.push(predecessor_id);
        }
    }
    let goal_edges: Vec<(TaskId, GoalId)> = conn.query_map(
        "SELECT task_id, goal_id FROM task_goal_dependencies ORDER BY goal_id",
        sql_params![],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    for (task_id, goal_id) in goal_edges {
        if let Some(node) = nodes.get_mut(&task_id) {
            node.goal_dependencies.push(goal_id);
        }
    }
    let all_goals: Vec<Goal> = conn.query_map("SELECT * FROM goals", sql_params![], goal_row)?;
    let goals = all_goals
        .iter()
        .map(|goal| (goal.id(), goal.verdict()))
        .collect();
    let draft_goal_tags = all_goals
        .iter()
        .filter(|goal| goal.is_draft())
        .map(|goal| (goal.id(), goal.tags().to_vec()))
        .collect();
    let mut membership_gaps = BTreeMap::new();
    for task in &targets {
        if let Some(gap) = super::follow_up_membership::membership_gap(conn, task.id())? {
            membership_gaps.insert(task.id(), gap);
        }
    }
    // A replacement plan's problems, once per proposal, on its first
    // target.
    let mut replacement = BTreeMap::new();
    let mut proposals = std::collections::BTreeSet::new();
    for task in &targets {
        let proposal: Option<ProposalId> = conn.query_row(
            "SELECT proposal_id FROM tasks WHERE id=?1",
            sql_params![task.id()],
            |r| r.get(0),
        )?;
        if let Some(proposal) = proposal
            && proposals.insert(proposal)
        {
            let problems = super::replans::replacement_problems_port(conn, proposal, &[])?;
            if !problems.is_empty() {
                replacement.insert(task.id(), problems);
            }
        }
    }
    Ok(LintInput {
        targets,
        nodes,
        goals,
        changes: None,
        title_prefixes: None,
        goal_tags: None,
        draft_goal_tags,
        membership_gaps,
        replacement,
    })
}

/// The claimable task IDs in claim order (ADR-0040 decision 4), from the
/// one ordering [`dependency_graph`] applies, so `candidates`, `graph` and
/// every claim agree.
pub(super) fn claim_order(conn: &impl SqlRead) -> Result<Vec<TaskId>> {
    Ok(dependency_graph(read_graph_input(conn)?, None).candidates)
}

/// Reserve a dependency-ready task inside the caller's write transaction:
/// the first task of `order` that is still a candidate, or the first
/// candidate in claim order when none of them is (an empty `order` means
/// claim order). There
/// is no queue-wide execution slot; `one_unfinished_run_per_task` is the
/// only limit, so concurrent claims take different tasks. The run is
/// created at `at` with an ID from `ids`. `attributes` (an object: what a
/// supervisor measured at the claim, task 197) go into `run_claimed`
/// beside its transition, and so does the worker session `trial` chooses
/// for the task (ADR-0079 decisions 3 and 4): chosen here, in the claim's
/// transaction, so two claims never take the same turn of the trial. A
/// task whose worker has none of `routes` is not claimed (ADR-t813-2):
/// the run requests the provider of its task's worker and runs on its
/// route's, recorded as `provider_switched` (phase `start`) when they
/// differ.
#[allow(clippy::too_many_arguments)]
pub(super) fn claim_task(
    tx: &impl crate::infrastructure::sql_exec::SqlWrite,
    runs_dir: &Path,
    ids: &dyn IdGenerator,
    at: SystemTime,
    base_commit: &CommitSha,
    order: &[TaskId],
    held: &crate::domain::claim_order::ClaimHolds,
    attributes: Option<&serde_json::Value>,
    trial: &WorkerTrial,
    routes: &[WorkerRoute],
) -> Result<ClaimOutcome> {
    let mut ready = ready_tasks(tx)?;
    // The pair the trial of the provider pair assigns each ready task
    // (ADR-t4005-1), whose worker it requests; none with the trial off.
    let mut pairs = std::collections::HashMap::new();
    if trial.providers.enabled {
        for task in &ready {
            if let Some(choice) = trial.providers.choose(
                task.id(),
                task.worker(),
                task.execution_class(),
                &pair_claims(tx, task.id())?,
            ) {
                pairs.insert(task.id(), choice);
            }
        }
    }
    let requested = |task: &Task| {
        pairs
            .get(&task.id())
            .map_or(task.worker(), |choice: &provider_trial::PairChoice| {
                choice.requested(task.worker())
            })
    };
    ready.retain(|task| provider_switch::route_of(routes, requested(task)).is_some());
    // The classes a class limit holds (ADR-t1487-2): no task of them is
    // claimed here, not one read after the supervisor made its order, nor
    // by the fallback below.
    ready.retain(|task| !held.classes.contains(&task.execution_class()));
    // Nor a Spike that would open an investigation past the limit
    // (ADR-t1487-1 decision 7), judged on the queue as it is now.
    let now_secs = crate::application::unix_seconds(at);
    // The cycles are settled for every claim (a Spike's opens one); the
    // investigations are read only when a limit holds them.
    let investigations = if held.investigations.max.is_some()
        && ready.iter().any(|task| task.execution_class().is_spike())
    {
        super::investigations::refresh(tx, now_secs)?
    } else {
        super::investigations::settle(tx, now_secs)?;
        crate::domain::investigation::Investigations::default()
    };
    ready.retain(|task| {
        investigations
            .limit(
                task.id(),
                task.execution_class(),
                task.goal_id(),
                held.investigations,
                now_secs,
            )
            .is_none()
    });
    if ready.is_empty() {
        return Ok(ClaimOutcome::NoReadyTask);
    }
    let position = |ids: &[TaskId]| {
        ids.iter()
            .find_map(|id| ready.iter().position(|task| task.id() == *id))
    };
    let preferred = match position(order) {
        Some(index) => index,
        // Back to the claim order when none of `order` is ready any more
        // (claimed elsewhere, canceled): never a task that waits for a
        // build containing its dependencies' landings, which only the
        // supervisor that judged it may claim (ADR-t1632-1).
        None => {
            let fallback = claim_order(tx)?
                .iter()
                .find_map(|id| {
                    ready
                        .iter()
                        .position(|task| task.id() == *id && !task.wait_for_build())
                })
                .or_else(|| ready.iter().position(|task| !task.wait_for_build()));
            match fallback {
                Some(index) => index,
                None => return Ok(ClaimOutcome::NoReadyTask),
            }
        }
    };
    let task = task::claim(ready.swap_remove(preferred))?;
    let worker = requested(&task);
    let pair = pairs.remove(&task.id());
    let route = provider_switch::route_of(routes, worker)
        .copied()
        .context("the claimed task has no route")?;
    // A run on Codex is in no group of the trial, which compares Claude's
    // models (task 892): it neither takes a group nor a turn of it; nor is
    // a treatment of the provider pair, on whichever provider it runs
    // (ADR-t4005-1 decision 4).
    let choice = if route.actual.provider == crate::domain::Provider::Codex
        || pair
            .as_ref()
            .is_some_and(provider_trial::PairChoice::is_treatment)
    {
        worker_model::TrialChoice {
            session: worker_model::WorkerSession::default(),
            percentile: None,
            trial: None,
        }
    } else {
        worker_model::choose(trial, task.id(), &trial_events(tx, trial)?)
    };
    // A step the task was raised to stays with its later runs (ADR-0079
    // decision 5), and so does a model a release chose (ADR-t1879-1).
    let (session, inherited) = choice
        .session
        .clone()
        .claimed(&session_events(tx, task.id())?);
    let now = timestamp(at);
    let run_id = RunId::new(ids.uuid())?;
    let actual = crate::domain::worker::Worker {
        mode: crate::domain::worker::WorkerMode::Headless,
        ..route.actual
    };
    let run = TaskRun::new(run_id, &task, base_commit, now.clone())?
        .requesting(worker)
        .running_on(actual);
    // `status='ready'` only detects a concurrent change; the domain decided the claim.
    ensure!(
        tx.execute(
            "UPDATE tasks SET status=?1, updated_at=?2 WHERE id=?3 AND status='ready'",
            sql_params![task.status().as_str(), now, task.id()],
        )? == 1,
        "task {} changed concurrently",
        task.id()
    );
    tx.execute(
        "INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,worker_mode,base_commit,created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        sql_params![
            run.id(),
            run.task_id(),
            run.status().as_str(),
            run.requested_provider().as_str(),
            run.actual_provider().as_str(),
            run.worker_mode().as_str(),
            run.base_commit(),
            run.created_at()
        ],
    )?;
    // The task's open replanning operation goes on with its new run.
    super::replans::carry_open_to_port(tx, task.id(), run.id(), now_secs)?;
    if task.worker().mode == crate::domain::worker::WorkerMode::Interactive {
        event(
            tx,
            task.id(),
            Some(run.id()),
            EventKind::WorkerModeConverted,
            json!({"phase":"claim", "from":"interactive", "to":"headless", "reason":"interactive_worker_removed"}),
        )?;
    }
    if let Some(reason) = route.switch {
        event(
            tx,
            task.id(),
            Some(run.id()),
            EventKind::ProviderSwitched,
            provider_switch::switched_payload(
                run.requested_provider(),
                actual,
                reason,
                SwitchPhase::Start,
                None,
                1,
                None,
            ),
        )?;
    }
    event(tx, task.id(), Some(run.id()), EventKind::RunClaimed, {
        // The claimed task's execution class (ADR-t1487-2 decision 4).
        let mut payload = json!({"from": "ready", "to": task.status(), "provider": run.actual_provider(), "requested_provider": run.requested_provider(), "worker_mode": run.worker_mode(), "execution_class": task.execution_class()});
        if let (Some(payload), Some(serde_json::Value::Object(attributes))) =
            (payload.as_object_mut(), attributes)
        {
            // The version of the run's own provider among the host's
            // (ADR-t813-2 decision 7): `claude_version` or `codex_version`.
            let version = attributes
                .get(&format!("{}_version", run.actual_provider().as_str()))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let mut attributes = attributes.clone();
            // The versions of the instructions the run's worker reads
            // (goal 113), of its own provider among the supervisor's.
            crate::domain::instructions::settle(&mut attributes, run.actual_provider().as_str());
            // Where the task stood in the supervisor's line, and its
            // priority then (ADR-t1662-1 decision 6).
            claim_facts::settle(&mut attributes, task.id());
            payload.extend(attributes);
            payload.insert("provider_version".to_owned(), version);
        }
        if let Some(payload) = payload.as_object_mut() {
            // Codex's model is not the step's Claude model: its turns
            // record the one it used.
            payload.extend(session.fields_on(route.actual.provider));
            if let Some(percentile) = choice.percentile {
                payload.insert("trial_percentile".to_owned(), json!(percentile));
            }
            // The trial's name next to its group (ADR-t3957-1 decision 7).
            if let (Some(_), Some(name)) = (session.group, &choice.trial) {
                payload.insert("trial".to_owned(), json!(name));
            }
            if inherited {
                payload.insert("escalation_inherited".to_owned(), json!(true));
            }
            // The pair's group, its name and the assigned worker and review,
            // or why the task is outside (ADR-t4005-1 decision 11).
            if let Some(pair) = &pair {
                payload.extend(pair.fields());
            }
            // A Spike's claim opens its investigation unless it is open
            // (ADR-t1487-1 decision 7).
            if super::investigations::open_cycle(
                tx,
                task.id(),
                task.execution_class(),
                task.goal_id(),
                now_secs,
            )? {
                payload.insert("investigation_opened".to_owned(), json!(true));
            }
            if let Some(ready_at) = task_ready_at(tx, task.id())? {
                payload.insert("ready_at".to_owned(), json!(ready_at));
            }
            // Last: the open set of the claim's attributes, from the
            // fields above (ADR-t1662-1 decisions 21 to 26).
            let attributes = claim_facts::attributes(payload);
            payload.insert(
                claim_facts::ATTRIBUTES.to_owned(),
                serde_json::Value::Object(attributes),
            );
        }
        payload
    })?;
    let run = run.relocated(runs_dir);
    Ok(ClaimOutcome::Claimed { run: Box::new(run) })
}

/// When `task` became claimable ([`claim_facts::claimable_since`], which
/// judges it): the events that move it in and out of the candidates, read
/// here in one query.
pub(super) fn task_ready_at(conn: &impl SqlRead, task: TaskId) -> Result<Option<String>> {
    let own = [
        EventKind::TaskCreated,
        EventKind::TaskStatusChanged,
        EventKind::RunClaimed,
        EventKind::DependencyAdded,
        EventKind::DependencyRemoved,
        EventKind::GoalDependencyAdded,
        EventKind::GoalDependencyRemoved,
        EventKind::TaskGoalChanged,
    ]
    .map(|kind| format!("'{}'", kind.as_str()))
    .join(",");
    let goals = [
        EventKind::GoalCreated,
        EventKind::GoalStatusChanged,
        EventKind::GoalClosed,
        EventKind::GoalReopened,
    ]
    .map(|kind| format!("'{}'", kind.as_str()))
    .join(",");
    let events = conn.query_map(
        &format!(
            "SELECT * FROM run_events WHERE task_id=?1 AND kind IN ({own})
             UNION
             SELECT * FROM run_events WHERE kind='{status}' AND task_id IN (
               SELECT {predecessor_id} FROM run_events
                WHERE task_id=?1 AND kind IN ('{added}','{removed}'))
             UNION
             SELECT * FROM run_events WHERE task_id IS NULL AND kind IN ({goals}) AND goal_id IN (
               SELECT {goal_id} FROM run_events
                WHERE task_id=?1 AND kind IN ('{goal_added}','{goal_removed}','{created}')
               UNION SELECT {from} FROM run_events
                WHERE task_id=?1 AND kind='{moved}'
               UNION SELECT {to} FROM run_events
                WHERE task_id=?1 AND kind='{moved}')
             ORDER BY id",
            status = EventKind::TaskStatusChanged.as_str(),
            added = EventKind::DependencyAdded.as_str(),
            removed = EventKind::DependencyRemoved.as_str(),
            goal_added = EventKind::GoalDependencyAdded.as_str(),
            goal_removed = EventKind::GoalDependencyRemoved.as_str(),
            created = EventKind::TaskCreated.as_str(),
            moved = EventKind::TaskGoalChanged.as_str(),
            predecessor_id =
                conn.dialect()
                    .json_field("payload", "$.predecessor_id", JsonType::Integer),
            goal_id = conn
                .dialect()
                .json_field("payload", "$.goal_id", JsonType::Integer),
            from = conn
                .dialect()
                .json_field("payload", "$.from", JsonType::Integer),
            to = conn
                .dialect()
                .json_field("payload", "$.to", JsonType::Integer),
        ),
        sql_params![task],
        event_row,
    )?;
    Ok(claim_facts::claimable_since(task, &events))
}

/// What the trial chooses a claim's worker session from: every
/// `task_weight_predicted` and `run_claimed`, oldest first; none while it
/// is off.
pub(super) fn trial_events(conn: &impl SqlRead, trial: &WorkerTrial) -> Result<Vec<RunEvent>> {
    if !trial.enabled {
        return Ok(Vec::new());
    }
    Ok(conn.query_map(
        &format!(
            "SELECT * FROM run_events WHERE kind IN ('{}','{}') ORDER BY id",
            event_kind::TASK_WEIGHT_PREDICTED,
            event_kind::RUN_CLAIMED
        ),
        sql_params![],
        event_row,
    )?)
}

/// The `run_claimed` of `task`'s earlier runs and its releases, oldest
/// first: the group of the provider pair it was drawn into stays with it,
/// and a release that chose a provider takes it out (ADR-t4005-1).
pub(super) fn pair_claims(conn: &impl SqlRead, task: TaskId) -> Result<Vec<RunEvent>> {
    Ok(conn.query_map(
        &format!(
            "SELECT * FROM run_events WHERE task_id=?1 AND kind IN ('{}','{}') ORDER BY id",
            event_kind::RUN_CLAIMED,
            EventKind::TaskReleased.as_str()
        ),
        sql_params![task],
        event_row,
    )?)
}

/// The events of `task`'s earlier runs that opened a worker session
/// (`worker_model::SESSION_EVENTS`) and its releases, which may choose the
/// next one's model, oldest first.
pub(super) fn session_events(conn: &impl SqlRead, task: TaskId) -> Result<Vec<RunEvent>> {
    let kinds = worker_model::SESSION_EVENTS
        .iter()
        .chain([&EventKind::TaskReleased])
        .map(|kind| format!("'{}'", kind.as_str()))
        .collect::<Vec<_>>()
        .join(",");
    Ok(conn.query_map(
        &format!("SELECT * FROM run_events WHERE task_id=?1 AND kind IN ({kinds}) ORDER BY id"),
        sql_params![task],
        event_row,
    )?)
}

/// Executing, awaiting or undergoing integration, or waiting for a session;
/// the same set as `one_unfinished_run_per_task`.
/// Apply `action` to the task inside the caller's transaction, recording
/// `task_status_changed`: what `transition` does, and what the triage does
/// to the task of the run it triaged.
pub(super) fn transition_task(
    conn: &impl crate::infrastructure::sql_exec::SqlWrite,
    task_id: TaskId,
    action: TaskAction,
    now: &str,
) -> Result<Task> {
    apply_transition(conn, task_id, action, now, None)
}

/// Complete the task of `run` inside the caller's transaction:
/// `in_progress` → `completed`, recorded as `task_status_changed` on the
/// run, with a completed Spike's replanning request (`now_secs`). The
/// planning side's write of the task, which the run's end calls rather
/// than writing `tasks` itself: its landing (T2 of
/// `src/context_boundaries/ownership.rs`) or the close of a verified
/// `already_satisfied` claim without one (T12, ADR-t1818-1 decision 5).
pub(super) fn complete_task(
    conn: &impl crate::infrastructure::sql_exec::SqlWrite,
    task_id: TaskId,
    run: &RunId,
    now: &str,
    now_secs: i64,
) -> Result<Task> {
    ensure!(
        conn.execute(
            "UPDATE tasks SET status='completed', updated_at=?2 WHERE id=?1 AND status='in_progress'",
            sql_params![task_id, now],
        )? == 1,
        "task {task_id} is not in progress"
    );
    event(
        conn,
        task_id,
        Some(run),
        EventKind::TaskStatusChanged,
        json!({"from": TaskStatus::InProgress, "to": TaskStatus::Completed}),
    )?;
    // A Spike's completion leaves its replanning request in the same
    // transaction (ADR-t1487-1 decision 5).
    super::plan_requests::record_spike_request(conn, task_id, now_secs)?;
    super::task_holds::end_with_task_port(conn, task_id, TaskStatus::Completed, now_secs)?;
    read_task(conn, task_id)
}

/// End `task_id`, whose replanning operation `operation` replaced it with
/// child tasks, inside the apply's transaction: `in_progress` →
/// `replaced` (neither completed nor canceled), recorded as
/// `task_status_changed` on its parked run `run`, with its open hold ended.
pub(super) fn replace_task(
    conn: &impl crate::infrastructure::sql_exec::SqlWrite,
    task_id: TaskId,
    run: &RunId,
    operation: crate::domain::ReplanId,
    now: &str,
    now_secs: i64,
) -> Result<Task> {
    ensure!(
        conn.execute(
            "UPDATE tasks SET status='replaced', updated_at=?2 WHERE id=?1 AND status='in_progress'",
            sql_params![task_id, now],
        )? == 1,
        "task {task_id} is not in progress"
    );
    event(
        conn,
        task_id,
        Some(run),
        EventKind::TaskStatusChanged,
        json!({"from": TaskStatus::InProgress, "to": TaskStatus::Replaced,
               "replan_operation_id": operation}),
    )?;
    super::task_holds::end_with_task_port(conn, task_id, TaskStatus::Replaced, now_secs)?;
    read_task(conn, task_id)
}

/// Cancel `task_id` inside the caller's transaction with the reason the
/// domain decides of `cancel` (ADR-t947-4), recorded in the payload of its
/// `task_status_changed`: what `cancel` does, plan review's
/// `cancel_duplicate` and the `cancel` answers of the asks the runtime
/// applies. A task named as the one that holds the content is checked by
/// [`check_duplicate`] and gives the reason of a `--duplicate-of` alone.
pub(super) fn cancel_task(
    conn: &impl crate::infrastructure::sql_exec::SqlWrite,
    task_id: TaskId,
    cancel: &NewCancel,
    now: &str,
) -> Result<Task> {
    let target = match cancel.duplicate_of {
        Some(target) => {
            check_duplicate(conn, task_id, target)?;
            Some(read_task(conn, target)?.status())
        }
        None => None,
    };
    let record = cancel.record(target)?;
    apply_transition(conn, task_id, TaskAction::Cancel, now, Some(&record))
}

/// Check that `task_id` may be canceled as a duplicate of `duplicate_of`:
/// the target must be another task that exists and is not canceled; a
/// completed one means the task was already implemented there. A target
/// canceled as a duplicate itself is refused with its own target, so the
/// records never chain nor loop.
pub(super) fn check_duplicate(
    conn: &impl SqlRead,
    task_id: TaskId,
    duplicate_of: TaskId,
) -> Result<()> {
    let target = if task_id == duplicate_of {
        None
    } else {
        let status: Option<String> = conn
            .query_row(
                "SELECT status FROM tasks WHERE id=?1",
                sql_params![duplicate_of],
                |r| r.get(0),
            )
            .optional()?;
        match status
            .map(|status| status.parse::<TaskStatus>())
            .transpose()?
        {
            Some(TaskStatus::Canceled) => {
                Some((TaskStatus::Canceled, duplicate_target(conn, duplicate_of)?))
            }
            other => other.map(|status| (status, None)),
        }
    };
    match crate::domain::task::duplicate_refusal(task_id, duplicate_of, target) {
        Some(why) => anyhow::bail!(why),
        None => Ok(()),
    }
}

/// The task `task_id`'s latest cancel names as the one that holds its
/// content (ADR-0046 decision 5, ADR-t947-4 decision 2): the
/// `duplicate_of` of its latest `task_status_changed`, while that event
/// canceled it.
pub(super) fn duplicate_target(conn: &impl SqlRead, task_id: TaskId) -> Result<Option<TaskId>> {
    Ok(cancel_of(conn, task_id)?.and_then(|cancel| cancel.duplicate_of))
}

/// The canceled tasks whose latest cancel names `task_id`, ascending, each
/// with its reason (one recorded before reasons is read by its target).
pub(super) fn duplicates_of(
    conn: &impl SqlRead,
    task_id: TaskId,
) -> Result<BTreeMap<TaskId, String>> {
    let candidates: Vec<TaskId> = conn.query_map(
        &format!(
            "SELECT DISTINCT e.task_id FROM run_events e JOIN tasks t ON t.id=e.task_id
             WHERE e.kind='{}' AND t.status='canceled'
             AND {duplicate_of}=?1 ORDER BY e.task_id",
            event_kind::TASK_STATUS_CHANGED,
            duplicate_of =
                conn.dialect()
                    .json_field("e.payload", "$.duplicate_of", JsonType::Integer),
        ),
        sql_params![task_id],
        |r| r.get(0),
    )?;
    let mut duplicates = BTreeMap::new();
    for candidate in candidates {
        if let Some(RecordedCancel {
            duplicate_of: Some(target),
            reason: Some(reason),
        }) = cancel_of(conn, candidate)?
            && target == task_id
        {
            duplicates.insert(candidate, reason);
        }
    }
    Ok(duplicates)
}

pub(super) fn apply_transition(
    conn: &impl crate::infrastructure::sql_exec::SqlWrite,
    task_id: TaskId,
    action: TaskAction,
    now: &str,
    cancel: Option<&CancelRecord>,
) -> Result<Task> {
    // No status of a task a replanning operation guards changes but by
    // the operation's own outcome (ADR-t1521-2 decision 5).
    super::replans::check_task_step(
        conn,
        task_id,
        match action {
            TaskAction::Ready | TaskAction::BypassReview | TaskAction::Approve => {
                GuardedStep::Ready
            }
            TaskAction::Cancel => GuardedStep::Cancel,
            TaskAction::Draft | TaskAction::Submit | TaskAction::Reopen => GuardedStep::Edit,
        },
    )?;
    // A child of a replacement proposal waits for the trusted runtime's
    // apply, whoever readies it.
    if matches!(
        action,
        TaskAction::Ready | TaskAction::BypassReview | TaskAction::Approve
    ) {
        super::replans::check_child_ready_port(conn, task_id)?;
    }
    let task = read_task(conn, task_id)?;
    let from = task.status();
    if action == TaskAction::BypassReview {
        // A person's bypass takes a follow_up past plan review only with a
        // current membership judgement, like submit (ADR-t1504-2 decision
        // 7); a submitted one too, as plan review is the other check.
        super::follow_up_membership::check_judged(conn, &[task_id], true)?;
    }
    let task = task::transition(task, action, has_unfinished_run(conn, task_id)?)?;
    // `status=?3` only detects a concurrent change; the domain decided the move.
    let changed = conn.execute(
        "UPDATE tasks SET status=?1, updated_at=?2 WHERE id=?3 AND status=?4",
        sql_params![task.status().as_str(), now, task_id, from.as_str()],
    )?;
    ensure!(changed == 1, "task {task_id} changed concurrently");
    if action == TaskAction::BypassReview {
        // A person readied the task past plan review: its follow-ups count
        // again from 1 (ADR-0037 decision 6, kept by ADR-0041 decision 16).
        conn.execute(
            "UPDATE tasks SET follow_up_depth=0 WHERE id=?1",
            sql_params![task_id],
        )?;
    }
    let mut payload = json!({"from": from, "to": task.status()});
    if let (Some(cancel), Some(object)) = (cancel, payload.as_object_mut()) {
        object.extend(cancel.fields());
    }
    // A draft the runtime or a job made carries where it came from into
    // its cancel, so the cancel is found from its source (ADR-t807-1).
    if from == TaskStatus::Draft
        && task.status() == TaskStatus::Canceled
        && let Some(object) = payload.as_object_mut()
    {
        object.extend(super::draft_planners::origin_fields_port(conn, task_id)?);
    }
    event(conn, task_id, None, EventKind::TaskStatusChanged, payload)?;
    // A task that ends takes its hold with it (ADR-t1879-1).
    super::task_holds::end_with_task_port(conn, task_id, task.status(), unix_secs(now))?;
    // A person skipped plan review (ADR-0041 decision 8).
    if action == TaskAction::BypassReview {
        event(
            conn,
            task_id,
            None,
            EventKind::ReviewBypassed,
            json!({"from": from}),
        )?;
    }
    read_task(conn, task_id)
}

/// The unix seconds of `now`, a store timestamp the caller's clock made;
/// the system clock's for one that does not parse.
pub(super) fn unix_secs(now: &str) -> i64 {
    crate::domain::stats::timestamp_millis(now).map_or_else(
        || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs() as i64)
        },
        |millis| millis.div_euclid(1000),
    )
}

pub(super) fn has_unfinished_run(conn: &impl SqlRead, task_id: TaskId) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM task_runs WHERE task_id=?1
         AND status IN ('claimed','starting','running','validating','awaiting_integration',
                        'integrating','needs_session'))",
        sql_params![task_id],
        |r| r.get(0),
    )?)
}

/// Refuse, before anything is written, a task whose status, proposal,
/// proposal status or proposal owner in the open transaction is not
/// `authorized`, what its command was authorized with (ADR-t883-1, task
/// 1609); `None` checks nothing.
pub(super) fn check_authorized(
    conn: &impl SqlRead,
    task_id: TaskId,
    authorized: Option<&TaskAuthorization>,
) -> Result<()> {
    if let Some(authorized) = authorized {
        authorized.check_unchanged(task_id, &task_authorization(conn, task_id)?)?;
    }
    Ok(())
}

/// The status of `task` and the proposal it is in, as a planning command
/// is authorized with them; a missing task is an error.
pub(super) fn task_authorization(conn: &impl SqlRead, task: TaskId) -> Result<TaskAuthorization> {
    Ok(TaskAuthorization {
        status: read_task(conn, task)?.status(),
        proposal: super::proposals::task_proposal(conn, task)?,
    })
}

/// Give the task a priority of its own, or with none take its goal's again
/// (`set-priority`, `--inherit` for none), recording `task_priority_changed`
/// (with `by` when the runtime changes it for a job) when the own priority
/// changes, even if the base value stays the same (ADR-t1639-1 decision 2).
pub(super) fn change_priority(
    conn: &impl crate::infrastructure::sql_exec::SqlWrite,
    task_id: TaskId,
    priority: Option<Priority>,
    now: &str,
    by: Option<&str>,
) -> Result<()> {
    let task = read_task(conn, task_id)?;
    let (from, from_source, from_by) =
        (task.priority(), task.priority_source(), task.priority_by());
    let own = task.own_priority();
    let task = task::set_priority(task, priority)?;
    if own == task.own_priority() {
        return Ok(());
    }
    // A person's priority, its own or its goal's, only a person changes
    // (ADR-t1975-1 decision 3).
    check_priority_change(
        actor_role(conn)?,
        PriorityHolder::Task(task_id),
        from,
        from_by,
    )?;
    // Who set it (ADR-t1975-1 decision 3); none without an own priority.
    let priority_by = match task.own_priority() {
        Some(_) => Some(PriorityBy::of(creator_of_actor(conn)?)),
        None => None,
    };
    conn.execute(
        "UPDATE tasks SET priority=?1, updated_at=?2, priority_by=?4 WHERE id=?3",
        sql_params![
            task.own_priority().map(Priority::as_i64),
            now,
            task_id,
            priority_by.map(PriorityBy::as_str)
        ],
    )?;
    // The own priority and who set it as stored, for the measurement's fold.
    let mut payload = json!({"from": from, "to": task.priority(),
        "from_source": from_source, "to_source": task.priority_source(),
        "own": task.own_priority(), "own_by": priority_by.map(PriorityBy::as_str)});
    if let Some(by) = by {
        payload["by"] = json!(by);
    }
    event(conn, task_id, None, EventKind::TaskPriorityChanged, payload)
}

/// Move the task to `goal_id`, or out of any goal, as `by` moves it
/// ([`task::set_goal`]): a person's priority it took from its old goal and
/// an AI's move would carry off is kept as its own, recorded as
/// `task_priority_changed` with `kept_on_move` before the move's
/// `task_goal_changed` (ADR-t1975-1 decision 4). A person's priority it
/// took from its old goal that the AI's `out_of_scope` judgement lets go
/// of is recorded the same way with `dropped_on_move`, the value it had
/// and `from_by: human` (ADR-t2373-1 decision 4).
pub(super) fn set_goal_in(
    conn: &impl crate::infrastructure::sql_exec::SqlWrite,
    task_id: TaskId,
    goal_id: Option<GoalId>,
    timestamp: &str,
    by: task::MovedBy,
) -> Result<Task> {
    let task = read_task(conn, task_id)?;
    let from = task.goal_id();
    let (from_priority, from_source) = (task.priority(), task.priority_source());
    let own = task.own_priority();
    let goal = goal_id.map(|id| read_goal(conn, id)).transpose()?;
    let to = goal.as_ref().map(task::Destination::of);
    let dropped = (from != goal_id)
        .then(|| task::persons_priority_dropped_on_move(&task, to, by))
        .flatten();
    let task = task::set_goal(task, to, by)?;
    if let Some(goal) = &goal {
        let goal_id = goal.id();
        goal::check_accepts_tasks(goal)?;
        if from != Some(goal_id) {
            // The goal would wait for the task: the task must not wait
            // for the goal (ADR-0038).
            let direct: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM task_goal_dependencies
                     WHERE task_id=?1 AND goal_id=?2)",
                sql_params![task_id, goal_id],
                |r| r.get(0),
            )?;
            let cycle = waits_for(conn, Node::Task(task_id), Node::Goal(goal_id))?;
            task::check_membership_acyclic(&task, goal_id, direct, cycle)?;
        }
    }
    if own != task.own_priority() {
        conn.execute(
            "UPDATE tasks SET priority=?1, priority_by=?2 WHERE id=?3",
            sql_params![
                task.own_priority().map(Priority::as_i64),
                task.own_priority_by().map(PriorityBy::as_str),
                task_id
            ],
        )?;
        event(
            conn,
            task_id,
            None,
            EventKind::TaskPriorityChanged,
            json!({"from": from_priority, "to": task.priority(), "from_source": from_source,
                "to_source": task.priority_source(), "kept_on_move": true,
                "own": task.own_priority(),
                "own_by": task.own_priority_by().map(PriorityBy::as_str)}),
        )?;
    }
    if let Some(dropped) = dropped {
        event(
            conn,
            task_id,
            None,
            EventKind::TaskPriorityChanged,
            json!({"from": dropped, "to": task.priority(), "from_source": from_source,
                "to_source": task.priority_source(), "from_by": PriorityBy::Human.as_str(),
                "to_by": task.priority_by().as_str(), "dropped_on_move": true,
                "judgement": "out_of_scope", "from_goal": from, "to_goal": task.goal_id()}),
        )?;
    }
    if from != task.goal_id() {
        conn.execute(
            "UPDATE tasks SET goal_id=?1, updated_at=?2 WHERE id=?3",
            sql_params![task.goal_id(), timestamp, task_id],
        )?;
        event(
            conn,
            task_id,
            None,
            EventKind::TaskGoalChanged,
            json!({"from": from, "to": task.goal_id()}),
        )?;
    }
    let result = read_task(conn, task_id)?;
    Ok(result)
}

pub(super) struct Store<'a, T> {
    pub tx: &'a T,
    pub runs_dir: &'a Path,
    pub generators: &'a Generators,
    pub changes: &'a Option<ChangeSet>,
    pub title_prefixes: &'a Option<TitlePrefixes>,
    pub goal_tags: &'a Option<TagSet>,
}
impl<T: crate::infrastructure::sql_exec::SqlWrite> Store<'_, T> {
    pub(super) fn transition_authorized(
        &mut self,
        task_id: TaskId,
        action: TaskAction,
        authorized: Option<&TaskAuthorization>,
    ) -> Result<Task> {
        let tx = self.tx;
        check_authorized(tx, task_id, authorized)?;
        let result = transition_task(tx, task_id, action, &self.generators.clock.timestamp())?;

        Ok(result)
    }

    pub(super) fn cancel_authorized(
        &mut self,
        task_id: TaskId,
        cancel: &NewCancel,
        authorized: Option<&TaskAuthorization>,
    ) -> Result<Task> {
        let tx = self.tx;
        check_authorized(tx, task_id, authorized)?;
        let result = cancel_task(tx, task_id, cancel, &self.generators.clock.timestamp())?;

        Ok(result)
    }

    pub(super) fn add_dependency_authorized(
        &mut self,
        task_id: TaskId,
        predecessor_id: TaskId,
        authorized: Option<&TaskAuthorization>,
    ) -> Result<()> {
        let tx = self.tx;
        check_authorized(tx, task_id, authorized)?;
        insert_dependency(
            tx,
            task_id,
            predecessor_id,
            &self.generators.clock.timestamp(),
        )?;

        Ok(())
    }

    pub(super) fn remove_dependency_authorized(
        &mut self,
        task_id: TaskId,
        predecessor_id: TaskId,
        authorized: Option<&TaskAuthorization>,
    ) -> Result<()> {
        let tx = self.tx;
        check_authorized(tx, task_id, authorized)?;
        super::replans::check_task_step(tx, task_id, GuardedStep::Dependency)?;
        super::replans::check_task_step(tx, predecessor_id, GuardedStep::Dependency)?;
        task::check_dependencies_editable(&read_task(tx, task_id)?)?;
        let changed = tx.execute(
            "DELETE FROM task_dependencies WHERE task_id=?1 AND predecessor_id=?2",
            sql_params![task_id, predecessor_id],
        )?;
        ensure!(
            changed == 1,
            "dependency {task_id} -> {predecessor_id} does not exist"
        );
        touch(tx, task_id, &self.generators.clock.timestamp())?;
        event(
            tx,
            task_id,
            None,
            EventKind::DependencyRemoved,
            json!({"predecessor_id": predecessor_id}),
        )?;

        Ok(())
    }

    pub(super) fn add_goal_dependency_authorized(
        &mut self,
        task_id: TaskId,
        goal_id: GoalId,
        authorized: Option<&TaskAuthorization>,
    ) -> Result<()> {
        let tx = self.tx;
        check_authorized(tx, task_id, authorized)?;
        super::replans::check_task_step(tx, task_id, GuardedStep::Dependency)?;
        insert_goal_dependency(tx, task_id, goal_id, &self.generators.clock.timestamp())?;

        Ok(())
    }

    pub(super) fn remove_goal_dependency_authorized(
        &mut self,
        task_id: TaskId,
        goal_id: GoalId,
        authorized: Option<&TaskAuthorization>,
    ) -> Result<()> {
        let tx = self.tx;
        check_authorized(tx, task_id, authorized)?;
        super::replans::check_task_step(tx, task_id, GuardedStep::Dependency)?;
        task::check_dependencies_editable(&read_task(tx, task_id)?)?;
        let changed = tx.execute(
            "DELETE FROM task_goal_dependencies WHERE task_id=?1 AND goal_id=?2",
            sql_params![task_id, goal_id],
        )?;
        ensure!(
            changed == 1,
            "dependency {task_id} -> goal {goal_id} does not exist"
        );
        touch(tx, task_id, &self.generators.clock.timestamp())?;
        event(
            tx,
            task_id,
            None,
            EventKind::GoalDependencyRemoved,
            json!({"goal_id": goal_id}),
        )?;

        Ok(())
    }

    pub(super) fn set_goal_authorized(
        &mut self,
        task_id: TaskId,
        goal_id: Option<GoalId>,
        authorized: Option<&TaskAuthorization>,
    ) -> Result<Task> {
        let tx = self.tx;
        check_authorized(tx, task_id, authorized)?;
        super::replans::check_task_step(tx, task_id, GuardedStep::Edit)?;
        let role = actor_role(tx)?;
        super::follow_up_membership::check_set_goal(tx, task_id, goal_id, role)?;
        let by = task::MovedBy::of(role);
        let result = set_goal_in(tx, task_id, goal_id, &self.generators.clock.timestamp(), by)?;

        Ok(result)
    }

    pub(super) fn set_paths_authorized(
        &mut self,
        task_id: TaskId,
        paths: Vec<String>,
        authorized: Option<&TaskAuthorization>,
    ) -> Result<Task> {
        let tx = self.tx;
        check_authorized(tx, task_id, authorized)?;
        super::replans::check_task_step(tx, task_id, GuardedStep::Edit)?;
        let task = read_task(tx, task_id)?;
        let from = task.paths().to_vec();
        let task = task::set_paths(task, paths)?;
        if from != task.paths() {
            tx.execute(
                "UPDATE tasks SET paths=?1, updated_at=?2 WHERE id=?3",
                sql_params![
                    serde_json::to_string(task.paths())?,
                    self.generators.clock.timestamp(),
                    task_id
                ],
            )?;
            event(
                tx,
                task_id,
                None,
                EventKind::TaskPathsChanged,
                json!({"from": from, "to": task.paths()}),
            )?;
        }
        let result = read_task(tx, task_id)?;

        Ok(result)
    }

    pub(super) fn set_priority_authorized(
        &mut self,
        task_id: TaskId,
        priority: Option<Priority>,
        authorized: Option<&TaskAuthorization>,
    ) -> Result<Task> {
        let tx = self.tx;
        check_authorized(tx, task_id, authorized)?;
        change_priority(
            tx,
            task_id,
            priority,
            &self.generators.clock.timestamp(),
            None,
        )?;
        let result = read_task(tx, task_id)?;

        Ok(result)
    }

    pub(super) fn edit_task_authorized(
        &mut self,
        task_id: TaskId,
        edit: TaskEdit,
        authorized: TaskStatus,
        full: Option<&TaskAuthorization>,
    ) -> Result<Task> {
        let tx = self.tx;
        super::replans::check_task_step(tx, task_id, GuardedStep::Edit)?;
        let old = read_task(tx, task_id)?;
        task::check_status_authorized(&old, authorized)?;
        check_authorized(tx, task_id, full)?;
        // The event keeps the fields before the edit, which consumes the task.
        let old_json = serde_json::to_value(&old)?;
        let old_mode = old.stored_worker_mode();
        let new = if old.status() == TaskStatus::InProgress {
            let latest: Option<String> = tx
                .query_row(
                    "SELECT status FROM task_runs WHERE task_id=?1 ORDER BY rowid DESC LIMIT 1",
                    sql_params![task_id],
                    |row| row.get(0),
                )
                .optional()?;
            ensure!(
                edit.verify_only()
                    && !has_unfinished_run(tx, task_id)?
                    && !tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM run_leases l JOIN task_runs r ON r.id=l.run_id WHERE r.task_id=?1)",
                        sql_params![task_id], |row| row.get::<_, bool>(0),
                    )?
                    && matches!(latest.as_deref(), Some("failed" | "interrupted")),
                "task {task_id} is in_progress; only --verify/--no-verify may be edited by user or inbox after its latest run has ended and no live run remains"
            );
            task::edit_ended_verify(old, edit)?
        } else {
            task::edit(old, edit)?
        };
        let new_json = serde_json::to_value(&new)?;
        let (mut from, mut to) = (serde_json::Map::new(), serde_json::Map::new());
        // The JSON leaves out a wait for the build not declared: recorded
        // as false (ADR-t1632-1).
        let value = |json: &serde_json::Value, field: &str| match field {
            "wait_for_build" => json!(json[field].as_bool().unwrap_or(false)),
            _ => json[field].clone(),
        };
        for field in EDITABLE_TASK_FIELDS {
            if old_json[field] != new_json[field] {
                from.insert(field.to_owned(), value(&old_json, field));
                to.insert(field.to_owned(), value(&new_json, field));
            }
        }
        // Naming the mode the default already resolves to (`--headless` on a
        // Claude task that named none) changes what is stored, not what is
        // shown: recorded with the stored values, null for the default.
        if old_mode != new.stored_worker_mode() && !to.contains_key("worker_mode") {
            from.insert("worker_mode".to_owned(), json!(old_mode));
            to.insert("worker_mode".to_owned(), json!(new.stored_worker_mode()));
        }
        if !to.is_empty() {
            tx.execute(
                "UPDATE tasks SET title=?1, description=?2, acceptance=?3,
                 verification_commands=?4, required_evidence=?5, paths=?6, context=?7,
                 worker_provider=?10, worker_mode=?11, change=?12, wait_for_build=?13,
                 execution_class=?14, updated_at=?8 WHERE id=?9",
                sql_params![
                    new.title(),
                    new.description(),
                    new.acceptance(),
                    serde_json::to_string(new.verification_commands())?,
                    serde_json::to_string(new.required_evidence())?,
                    serde_json::to_string(new.paths())?,
                    new.context(),
                    self.generators.clock.timestamp(),
                    task_id,
                    new.worker().provider.as_str(),
                    new.stored_worker_mode().map(WorkerMode::as_str),
                    new.change().map(TaskChange::as_str),
                    new.wait_for_build(),
                    new.execution_class().as_str(),
                ],
            )?;
            event(
                tx,
                task_id,
                None,
                EventKind::TaskEdited,
                json!({"from": from, "to": to}),
            )?;
        }
        let result = read_task(tx, task_id)?;

        Ok(result)
    }

    pub(super) fn add(&mut self, new: NewTask) -> Result<Task> {
        let tx = self.tx;
        let creator = creator_of_actor(tx)?;
        let result = insert_task(tx, new, &self.generators.clock.timestamp(), creator)?;

        Ok(result)
    }

    pub(super) fn claim(&mut self, base_commit: &CommitSha) -> Result<ClaimOutcome> {
        let tx = self.tx;
        let outcome = claim_task(
            tx,
            self.runs_dir,
            self.generators.ids.as_ref(),
            self.generators.clock.system_time(),
            base_commit,
            &[],
            &crate::domain::claim_order::ClaimHolds::default(),
            None,
            &WorkerTrial::default(),
            // What every binary runs (ADR-t813-2); a supervisor claims
            // through its table of adapters instead.
            &WorkerRoute::direct(&[Worker::CLAUDE_INTERACTIVE, Worker::CLAUDE_HEADLESS]),
        )?;

        Ok(outcome)
    }

    pub(super) fn add_goal(&mut self, new: NewGoal) -> Result<Goal> {
        let tx = self.tx;
        // Its origin, and a request's priority attached as a person's
        // (ADR-t1975-1 decisions 2 and 5).
        let creator = creator_of_actor(tx)?;
        let origin = creation_origin(creator);
        let (priority, priority_by) =
            goal_priority_at_creation(creator, request_priority(tx, origin)?, new.priority)?;
        let mut new = new;
        new.priority = Some(priority);
        let goal = Goal::new(
            GoalId::new(next_id(tx, "goals")?),
            new,
            self.generators.clock.timestamp(),
        )?
        .with_record(priority_by, origin);
        let id = goal.id();
        tx.execute(
            "INSERT INTO goals(id, title, description, acceptance, constraints, doc, status,
                               created_at, updated_at, priority, tags, priority_by, origin,
                               origin_kind, origin_request_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            sql_params![
                id,
                goal.title(),
                goal.description(),
                goal.acceptance(),
                goal.constraints(),
                goal.doc(),
                goal.status().as_str(),
                goal.created_at(),
                goal.updated_at(),
                goal.priority().as_i64(),
                serde_json::to_string(goal.tags())?,
                goal.priority_by().as_str(),
                origin.origin.as_str(),
                origin.kind.map(|kind| kind.as_str()),
                origin.request_id
            ],
        )?;
        let result = read_goal(tx, id)?;
        goal_event(tx, id, EventKind::GoalCreated, json!({"goal": result}))?;

        Ok(result)
    }

    pub(super) fn edit_goal(&mut self, goal_id: GoalId, edit: GoalEdit) -> Result<Goal> {
        let tx = self.tx;
        let old = read_goal(tx, goal_id)?;
        // The event keeps the goal before the edit, which consumes it.
        let old_json = serde_json::to_value(&old)?;
        let old_version = super::follow_up_membership::acceptance_version(tx, goal_id)?;
        // A change of priority records who set it (ADR-t1975-1 decision 3).
        // Only a person changes a person's (ADR-t1975-1 decision 3).
        let priority_by = match edit.priority {
            Some(priority) if priority != old.priority() => {
                check_priority_change(
                    actor_role(tx)?,
                    PriorityHolder::Goal(goal_id),
                    old.priority(),
                    old.priority_by(),
                )?;
                PriorityBy::of(creator_of_actor(tx)?)
            }
            _ => old.priority_by(),
        };
        let new = goal::edit(old, edit)?;
        tx.execute(
            "UPDATE goals SET title=?1, description=?2, acceptance=?3, constraints=?4, doc=?5,
             updated_at=?6, priority=?8, tags=?9, priority_by=?10 WHERE id=?7",
            sql_params![
                new.title(),
                new.description(),
                new.acceptance(),
                new.constraints(),
                new.doc(),
                self.generators.clock.timestamp(),
                goal_id,
                new.priority().as_i64(),
                serde_json::to_string(new.tags())?,
                priority_by.as_str()
            ],
        )?;
        goal_event(
            tx,
            goal_id,
            EventKind::GoalUpdated,
            // `new` keeps the setter of the priority before the edit; who
            // set it is the stored one (for the measurement's fold).
            json!({"old": old_json, "new": new, "old_acceptance_version": old_version,
                "new_acceptance_version": super::follow_up_membership::acceptance_version(tx, goal_id)?,
                "priority_by": priority_by.as_str()}),
        )?;
        let result = read_goal(tx, goal_id)?;

        Ok(result)
    }

    pub(super) fn close_goal(&mut self, goal_id: GoalId, verdict: GoalVerdict) -> Result<Goal> {
        let tx = self.tx;
        close_goal_in(
            tx,
            goal_id,
            verdict,
            &self.generators.clock.timestamp(),
            json!({}),
        )?;
        let result = read_goal(tx, goal_id)?;

        Ok(result)
    }

    pub(super) fn ready_goal(&mut self, goal_id: GoalId) -> Result<Goal> {
        let tx = self.tx;
        let draft = read_goal(tx, goal_id)?;
        let from = draft.status();
        let opened = goal::ready(draft)?;
        tx.execute(
            "UPDATE goals SET status=?1, updated_at=?2 WHERE id=?3",
            sql_params![
                opened.status().as_str(),
                self.generators.clock.timestamp(),
                goal_id
            ],
        )?;
        goal_event(
            tx,
            goal_id,
            EventKind::GoalStatusChanged,
            json!({"from": from, "to": opened.status()}),
        )?;
        let result = read_goal(tx, goal_id)?;

        Ok(result)
    }

    pub(super) fn add_note(&mut self, note: NewNote) -> Result<RunEvent> {
        let tx = self.tx;
        let payload = note.payload();
        let id = match &note.target {
            NoteTarget::Task(task_id) => {
                read_task(tx, *task_id)?;
                event_id(tx, *task_id, None, EventKind::Observation, payload)?
            }
            NoteTarget::Run(run_id) => {
                let task_id: TaskId = tx
                    .query_row(
                        "SELECT task_id FROM task_runs WHERE id=?1",
                        sql_params![run_id],
                        |r| r.get(0),
                    )
                    .optional()?
                    .with_context(|| format!("run {run_id} does not exist"))?;
                event_id(tx, task_id, Some(run_id), EventKind::Observation, payload)?
            }
            NoteTarget::Goal(goal_id) => {
                read_goal(tx, *goal_id)?;
                goal_event_id(tx, *goal_id, EventKind::Observation, payload)?
            }
        };
        let result = tx.query_row(
            "SELECT * FROM run_events WHERE id=?1",
            sql_params![id],
            event_row,
        )?;

        Ok(result)
    }
}

impl<T: SqlRead> Store<'_, T> {
    pub(super) fn ready_at(
        &self,
        tasks: &[TaskId],
    ) -> Result<std::collections::HashMap<TaskId, i64>> {
        let tx = self.tx;
        let mut times = std::collections::HashMap::new();
        for task in tasks {
            if let Some(at) = task_ready_at(tx, *task)?
                .as_deref()
                .and_then(crate::domain::stats::timestamp_millis)
            {
                times.insert(*task, at);
            }
        }
        Ok(times)
    }

    pub(super) fn list(&self, query: &TaskQuery) -> Result<TaskPage> {
        let tx = self.tx;
        ensure!(query.limit > 0, "limit must be at least 1");
        let mut filters = Vec::new();
        let mut values = Vec::new();
        let statuses: Vec<TaskStatus> = match &query.status {
            StatusFilter::Open => [
                TaskStatus::Draft,
                TaskStatus::Submitted,
                TaskStatus::Ready,
                TaskStatus::InProgress,
                TaskStatus::Completed,
                TaskStatus::Canceled,
            ]
            .into_iter()
            .filter(|status| !status.is_terminal())
            .collect(),
            StatusFilter::Any => Vec::new(),
            StatusFilter::Only(statuses) => statuses.clone(),
        };
        if !matches!(query.status, StatusFilter::Any) {
            filters.push(format!(
                "status IN ({})",
                vec!["?"; statuses.len()].join(",")
            ));
            values.extend(
                statuses
                    .iter()
                    .map(|s| SqlValue::Text(s.as_str().to_owned())),
            );
        }
        if let Some(goal_id) = query.goal_id {
            filters.push("goal_id = ?".into());
            values.push(SqlValue::Integer(goal_id.as_i64()));
        }
        let matching = if filters.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", filters.join(" AND "))
        };
        // One read snapshot keeps the page, its count and its runs consistent.

        let total: i64 = tx.query_row(
            &format!("SELECT count(*) FROM tasks{matching}"),
            &values,
            |r| r.get(0),
        )?;
        if let Some(before) = query.before {
            filters.push("id <= ?".into());
            values.push(SqlValue::Integer(before.as_i64()));
        }
        let page = if filters.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", filters.join(" AND "))
        };
        // One extra row tells whether another page follows.
        values.push(SqlValue::Integer(
            i64::try_from(query.limit)?.saturating_add(1),
        ));
        let mut tasks: Vec<Task> = tx.query_map(
            &format!(
                concat!(select_tasks!(), "{page} ORDER BY id DESC LIMIT ?"),
                page = page
            ),
            &values,
            task_row,
        )?;
        let next = if tasks.len() > query.limit {
            tasks.pop().map(|task| task.id())
        } else {
            None
        };
        let dependencies =
            "SELECT predecessor_id FROM task_dependencies WHERE task_id=?1 ORDER BY predecessor_id";
        let goal_dependencies = GOAL_DEPENDENCIES_QUERY;
        let latest_run =
            "SELECT id, status FROM task_runs WHERE task_id=?1 ORDER BY rowid DESC LIMIT 1";
        let tasks = tasks
            .into_iter()
            .map(|task| {
                let dependencies =
                    tx.query_map(dependencies, sql_params![task.id()], |r| r.get(0))?;
                let goal_dependencies =
                    tx.query_map(goal_dependencies, sql_params![task.id()], |r| r.get(0))?;
                let latest_run = tx
                    .query_row(latest_run, sql_params![task.id()], |row| {
                        Ok(LatestRun {
                            id: row.get("id")?,
                            status: enum_col(row, "status")?,
                        })
                    })
                    .optional()?;
                let cancel = if task.status() == TaskStatus::Canceled {
                    cancel_of(tx, task.id())?
                } else {
                    None
                };
                let (duplicate_of, cancel_reason) =
                    cancel.map_or((None, None), |cancel| (cancel.duplicate_of, cancel.reason));
                let replan = super::replans::open_operation_of_port(tx, task.id())?
                    .as_ref()
                    .map(ListedReplan::of);
                let hold = super::task_holds::open_hold_of_port(tx, task.id())?
                    .as_ref()
                    .map(ListedHold::of);
                Ok(TaskListItem::new(
                    task,
                    dependencies,
                    goal_dependencies,
                    latest_run,
                    duplicate_of,
                    query.full,
                )
                .with_cancel_reason(cancel_reason)
                .with_replan(replan)
                .with_hold(hold))
            })
            .collect::<Result<_>>()?;
        Ok(TaskPage {
            tasks,
            next,
            total: usize::try_from(total)?,
        })
    }

    pub(super) fn show(&mut self, task_id: TaskId) -> Result<TaskDetail> {
        let tx = self.tx;
        let task = read_task(tx, task_id)?;
        let dependencies = tx.query_map(
            "SELECT predecessor_id FROM task_dependencies WHERE task_id=?1 ORDER BY predecessor_id",
            sql_params![task_id],
            |r| r.get(0),
        )?;
        let goal_dependencies =
            tx.query_map(GOAL_DEPENDENCIES_QUERY, sql_params![task_id], |r| r.get(0))?;
        let runs = tx.query_map(
            "SELECT * FROM task_runs WHERE task_id=?1 ORDER BY rowid",
            sql_params![task_id],
            run_row(self.runs_dir),
        )?;
        let events = tx.query_map(
            "SELECT * FROM run_events WHERE task_id=?1 ORDER BY id",
            sql_params![task_id],
            event_row,
        )?;
        let processes = super::runtime_store::processes_for_task_port(tx, task_id)?;
        let (duplicate_of, cancel_reason) = cancel_of(tx, task_id)?
            .map_or((None, None), |cancel| (cancel.duplicate_of, cancel.reason));
        let duplicate_reasons = duplicates_of(tx, task_id)?;
        let duplicates = duplicate_reasons.keys().copied().collect();
        let membership_judgements = super::follow_up_membership::judgements(tx, task_id)?;
        let origin = super::draft_planners::task_origin_port(tx, task_id)?;
        let follow_up_drafts = super::draft_planners::follow_up_drafts_port(tx, task_id)?;
        let revisit = super::draft_planners::read_revisit_port(tx, task_id)?;
        let replans = super::replans::operations_of_port(tx, task_id)?;
        let hold = super::task_holds::open_hold_of_port(tx, task_id)?;
        let asks = tx.query_map(
            "SELECT * FROM asks WHERE task_id=?1
                 OR run_id IN (SELECT id FROM task_runs WHERE task_id=?1) ORDER BY id",
            sql_params![task_id],
            super::asks::ask_row,
        )?;

        Ok(TaskDetail {
            membership_judgements,
            task,
            dependencies,
            goal_dependencies,
            duplicate_of,
            cancel_reason,
            duplicates,
            duplicate_reasons,
            runs,
            events,
            processes,
            origin,
            follow_up_drafts,
            revisit,
            asks,
            replans,
            hold,
        })
    }

    pub(super) fn candidates(&self) -> Result<Vec<Task>> {
        let tx = self.tx;
        let order = claim_order(tx)?;
        let mut ready = ready_tasks(tx)?;
        ready.sort_by_key(|task| order.iter().position(|id| *id == task.id()));
        Ok(ready)
    }

    pub(super) fn build_waits(
        &self,
    ) -> Result<std::collections::HashMap<TaskId, Vec<crate::domain::Landing>>> {
        let mut waits: std::collections::HashMap<TaskId, Vec<_>> = Default::default();
        let statement = "SELECT t.id, c.task_id, c.commit_sha FROM tasks t
             LEFT JOIN task_dependencies d ON d.task_id = t.id
             LEFT JOIN landed_commits c ON c.task_id = d.predecessor_id
             WHERE t.status = 'ready' AND t.wait_for_build != 0
             ORDER BY t.id, c.id";
        let rows = self.tx.query_map(statement, sql_params![], |row| {
            Ok((
                row.get::<_, TaskId>(0)?,
                row.get::<_, Option<TaskId>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (task, predecessor, commit) = row;
            let landings = waits.entry(task).or_default();
            if let (Some(task_id), Some(commit)) = (predecessor, commit) {
                landings.push(crate::domain::Landing { task_id, commit });
            }
        }
        Ok(waits)
    }

    pub(super) fn graph_input(&self) -> Result<GraphInput> {
        let tx = self.tx;
        read_graph_input(tx)
    }

    pub(super) fn predecessors(&self, task_id: TaskId) -> Result<Vec<Predecessor>> {
        landed_tasks(
            self.tx,
            self.runs_dir,
            concat!(
                select_tasks!(),
                " JOIN task_dependencies d ON t.id = d.predecessor_id
                 WHERE d.task_id = ?1 ORDER BY t.id"
            ),
            task_id.as_i64(),
        )
    }

    pub(super) fn goal_predecessors(&self, task_id: TaskId) -> Result<Vec<GoalPredecessor>> {
        let goals: Vec<Goal> = self.tx.query_map(
            "SELECT g.* FROM task_goal_dependencies d JOIN goals g ON g.id = d.goal_id
                 WHERE d.task_id = ?1 ORDER BY g.id",
            sql_params![task_id],
            goal_row,
        )?;
        goals
            .into_iter()
            .map(|goal| {
                let tasks = landed_tasks(
                    self.tx,
                    self.runs_dir,
                    concat!(
                        select_tasks!(),
                        " WHERE t.goal_id = ?1 AND t.status = 'completed' ORDER BY t.id"
                    ),
                    goal.id().as_i64(),
                )?;
                Ok(GoalPredecessor { goal, tasks })
            })
            .collect()
    }

    pub(super) fn tasks_in_progress(&self) -> Result<Vec<Task>> {
        Ok(self.tx.query_map(
            concat!(
                select_tasks!(),
                " WHERE t.status = 'in_progress' ORDER BY t.id"
            ),
            sql_params![],
            task_row,
        )?)
    }

    pub(super) fn list_goals(&self) -> Result<Vec<GoalSummary>> {
        let goals: Vec<Goal> =
            self.tx
                .query_map("SELECT * FROM goals ORDER BY id", sql_params![], goal_row)?;
        goals
            .into_iter()
            .map(|goal| {
                Ok(GoalSummary {
                    id: goal.id(),
                    status: goal.status(),
                    priority: goal.priority(),
                    tags: goal.tags().to_vec(),
                    closed: goal.is_closed(),
                    verdict: goal.verdict(),
                    tasks: task_counts(self.tx, goal.id())?,
                    title: goal.into_title(),
                })
            })
            .collect()
    }

    pub(super) fn show_goal(&mut self, goal_id: GoalId) -> Result<GoalDetail> {
        let tx = self.tx;
        let goal = read_goal(tx, goal_id)?;
        let acceptance_version = super::follow_up_membership::acceptance_version(tx, goal_id)?;
        let follow_up_memberships = super::follow_up_membership::goal_memberships(tx, goal_id)?;
        let tasks = tx.query_map(
            "SELECT t.id, t.title, t.status, t.priority, t.priority_by,
                 g.priority AS goal_priority, g.priority_by AS goal_priority_by
                 FROM tasks t JOIN goals g ON g.id = t.goal_id WHERE t.goal_id=?1 ORDER BY t.id",
            sql_params![goal_id],
            goal_task_row,
        )?;
        let dependents = tx.query_map(
            "SELECT t.id, t.title, t.status, t.priority, t.priority_by,
                 g.priority AS goal_priority, g.priority_by AS goal_priority_by
                 FROM task_goal_dependencies d
                 JOIN tasks t ON t.id = d.task_id LEFT JOIN goals g ON g.id = t.goal_id
                 WHERE d.goal_id=?1 AND t.status NOT IN ('completed','canceled') ORDER BY t.id",
            sql_params![goal_id],
            goal_task_row,
        )?;
        let events = tx.query_map(
            "SELECT * FROM run_events WHERE goal_id=?1 ORDER BY id",
            sql_params![goal_id],
            event_row,
        )?;

        Ok(GoalDetail {
            acceptance_version,
            follow_up_memberships,
            closed: goal.is_closed(),
            goal,
            tasks,
            dependents,
            events,
        })
    }

    pub(super) fn lint_input(&self, tasks: &[TaskId]) -> Result<LintInput> {
        let tx = self.tx;
        let mut input = read_lint_input(tx, tasks)?;
        input.changes = self.changes.clone();
        input.title_prefixes = self.title_prefixes.clone();
        input.goal_tags = self.goal_tags.clone();
        Ok(input)
    }

    pub(super) fn notes(&self, query: &NoteQuery) -> Result<NotePage> {
        ensure!(query.limit > 0, "limit must be at least 1");
        let mut filters = vec!["kind = ?".to_owned()];
        let mut values = vec![SqlValue::Text(OBSERVATION_KIND.to_owned())];
        if let Some(goal_id) = query.goal_id {
            filters.push(
                "(goal_id = ? OR task_id IN (SELECT id FROM tasks WHERE goal_id = ?))".into(),
            );
            values.extend([
                SqlValue::Integer(goal_id.as_i64()),
                SqlValue::Integer(goal_id.as_i64()),
            ]);
        }
        if let Some(task_id) = query.task_id {
            filters.push("task_id = ?".into());
            values.push(SqlValue::Integer(task_id.as_i64()));
        }
        // Past a cursor the page runs forward from it; without one it is
        // the latest `limit` notes. Either way it is printed oldest first.
        let order = if let Some(since) = query.since {
            filters.push("id > ?".into());
            values.push(SqlValue::Integer(since.as_i64()));
            "ASC"
        } else {
            "DESC"
        };
        values.push(SqlValue::Integer(i64::try_from(query.limit)?));
        let mut notes: Vec<RunEvent> = self.tx.query_map(
            &format!(
                "SELECT * FROM run_events WHERE {} ORDER BY id {order} LIMIT ?",
                filters.join(" AND ")
            ),
            &values,
            event_row,
        )?;
        notes.sort_by_key(|note| note.id);
        let cursor = notes
            .last()
            .map(|note| note.id)
            .or(query.since)
            .unwrap_or(EventId::new(0));
        Ok(NotePage { notes, cursor })
    }
}
