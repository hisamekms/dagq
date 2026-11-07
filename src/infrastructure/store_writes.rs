//! The writes the store's modules share, on the execution port
//! ([`super::sql_exec`]): a task's and the queue's events and a new task with
//! its dependencies. The functions of the same name in [`super::sqlite`] hand
//! a rusqlite connection to these for a module that has not moved.

use anyhow::Result;
use serde_json::{Value, json};

use super::sql_exec::{OptionalRow, SqlRead, SqlWrite, sql_params};
use super::store_rows::{read_goal, read_task};
use crate::domain::event_kind::EventKind;
use crate::domain::plan_request::{
    Creator, PriorityBy, RecordedOrigin, creation_origin, task_priority_at_creation,
};
use crate::domain::task_replan::GuardedStep;
use crate::domain::worker::WorkerMode;
use crate::domain::{
    EventId, GoalId, NewTask, Priority, RequestId, RunId, Task, TaskChange, TaskId, goal, task,
};

/// Register `new` inside the caller's write transaction with
/// `task_created` and its dependencies: what `add` does, and what a
/// follow-up triage's `adopt` does in the transaction that cancels the draft.
///
/// `creator` is who creates it (`sqlite::creator_of_actor` for the actor of
/// the write): its origin is recorded once, and a task of a request a person
/// gave a priority takes that priority as a person's own unless its goal
/// already has it (ADR-t1975-1 decisions 2 and 5).
///
/// `task_created` carries the worker it is registered with: `provider` and
/// `worker_mode` resolved, and `provider_named` and `worker_mode_named`,
/// whether the registrant named each (`add --provider`, `--headless`, or
/// the same fields of a draft) or left it to the default, which a task
/// follows when the default changes (ADR-t1340-1).
pub(super) fn insert_task(
    tx: &impl SqlWrite,
    mut new: NewTask,
    now: &str,
    creator: Creator,
) -> Result<Task> {
    let dependencies = new.dependencies.clone();
    let goal_dependencies = new.goal_dependencies.clone();
    let origin = creation_origin(creator);
    let goal = new
        .goal_id
        .map(|goal_id| read_goal(tx, goal_id))
        .transpose()?;
    if let Some(goal) = &goal {
        goal::check_accepts_tasks(goal)?;
    }
    let (priority, priority_by) = task_priority_at_creation(
        creator,
        request_priority(tx, origin)?,
        new.priority,
        goal.as_ref()
            .map(|goal| (goal.priority(), goal.priority_by())),
    )?;
    new.priority = priority;
    let (provider_named, worker_mode_named) = (new.provider.is_some(), new.worker_mode.is_some());
    let task = Task::new(TaskId::new(next_id(tx, "tasks")?), new, now.to_owned())?;
    let id = task.id();
    tx.execute(
            "INSERT INTO tasks(id, title, description, acceptance, verification_commands, status, goal_id,
                               context, required_evidence, paths, priority, created_at,
                               updated_at, worker_provider, worker_mode, change, wait_for_build,
                               priority_by, origin, origin_kind, origin_request_id, execution_class)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)",
            sql_params![id, task.title(), task.description(), task.acceptance(),
                serde_json::to_string(task.verification_commands())?, task.status().as_str(),
                task.goal_id(), task.context(), serde_json::to_string(task.required_evidence())?,
                serde_json::to_string(task.paths())?, task.own_priority().map(Priority::as_i64),
                task.created_at(), task.updated_at(),
                task.worker().provider.as_str(), task.stored_worker_mode().map(WorkerMode::as_str),
                task.change().map(TaskChange::as_str), task.wait_for_build(),
                priority_by.map(PriorityBy::as_str), origin.origin.as_str(),
                origin.kind.map(|kind| kind.as_str()), origin.request_id,
                task.execution_class().as_str()],
        )?;
    // The attributes the measurement folds (`domain::ledger::attributes`)
    // as stored: its own priority and who set it, none to inherit.
    event(
        tx,
        id,
        None,
        EventKind::TaskCreated,
        json!({"goal_id": task.goal_id(), "status": task.status(), "title": task.title(),
            "change": task.change().map(TaskChange::as_str),
            "execution_class": task.execution_class().as_str(),
            "priority": task.own_priority(),
            "priority_by": priority_by.map(PriorityBy::as_str),
            "provider": task.worker().provider.as_str(), "provider_named": provider_named,
            "worker_mode": task.worker().mode.as_str(), "worker_mode_named": worker_mode_named}),
    )?;
    // Inserted after the task, which is in its goal already, so the
    // cycle checks see the goal's wait for it (ADR-0038).
    for predecessor in dependencies {
        insert_dependency(tx, id, predecessor, now)?;
    }
    for goal_id in goal_dependencies {
        insert_goal_dependency(tx, id, goal_id, now)?;
    }
    read_task(tx, id)
}

/// The request `origin` names and the priority a person gave it, when it
/// has one (ADR-t1975-1 decision 2).
pub(super) fn request_priority(
    tx: &impl SqlRead,
    origin: RecordedOrigin,
) -> Result<Option<(RequestId, Priority)>> {
    let Some(request) = origin.request_id else {
        return Ok(None);
    };
    let priority: Option<i64> = tx
        .query_row(
            "SELECT priority FROM plan_requests WHERE id=?1",
            sql_params![request],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(priority
        .map(Priority::from_i64)
        .transpose()?
        .map(|priority| (request, priority)))
}

/// A vertex of the wait graph (ADR-0038): a task waits for its predecessor
/// tasks and the goals it depends on, a goal waits for its tasks, and a
/// replaced task for the tasks its applied replacement made (ADR-t1521-2
/// decision 5).
#[derive(Debug, Clone, Copy)]
pub(super) enum Node {
    Task(TaskId),
    Goal(GoalId),
}

impl Node {
    fn key(self) -> (&'static str, i64) {
        match self {
            Self::Task(id) => ("task", id.as_i64()),
            Self::Goal(id) => ("goal", id.as_i64()),
        }
    }
}

/// Whether `from` already waits for `to`, directly or not, over every task
/// and goal: the edge `to -> from` would close a cycle. Found in SQL
/// because it needs the whole graph; whether that rejects the edge is the
/// domain's rule (ADR-0013).
pub(super) fn waits_for(tx: &impl SqlRead, from: Node, to: Node) -> Result<bool> {
    let (from_kind, from_id) = from.key();
    let (to_kind, to_id) = to.key();
    Ok(tx.query_row(
        "WITH RECURSIVE
           edges(from_kind, from_id, to_kind, to_id) AS (
             SELECT 'task', task_id, 'task', predecessor_id FROM task_dependencies
             UNION ALL
             SELECT 'task', task_id, 'goal', goal_id FROM task_goal_dependencies
             UNION ALL
             -- A canceled task never holds its goal back (achieved allows it).
             SELECT 'goal', goal_id, 'task', id FROM tasks
               WHERE goal_id IS NOT NULL AND status <> 'canceled'
             UNION ALL
             -- A replaced task stands for the tasks it was replaced by,
             -- wherever they belong (goal close waits for them).
             SELECT 'task', r.from_task_id, 'task', r.to_task_id FROM replan_replacements r
               JOIN replan_operations o ON o.id = r.operation_id AND o.status = 'applied'
           ),
           reached(kind, id) AS (
             SELECT ?1, ?2
             UNION
             SELECT e.to_kind, e.to_id FROM edges e
               JOIN reached r ON e.from_kind = r.kind AND e.from_id = r.id
           )
         SELECT EXISTS(SELECT 1 FROM reached WHERE kind = ?3 AND id = ?4)",
        sql_params![from_kind, from_id, to_kind, to_id],
        |r| r.get(0),
    )?)
}

/// The adapter's ID for constructing an aggregate before saving it in
/// the caller's write transaction. [`SqlRead::next_id`] owns allocation;
/// the eventual insert supplies the returned ID explicitly.
pub(in crate::infrastructure) fn next_id(tx: &impl SqlRead, table: &str) -> Result<i64> {
    Ok(tx.next_id(table)?)
}

pub(super) fn insert_dependency(
    tx: &impl SqlWrite,
    task_id: TaskId,
    predecessor_id: TaskId,
    now: &str,
) -> Result<()> {
    task::check_not_self(task_id, predecessor_id)?;
    // Neither end of an edge of a task being replanned changes: its apply
    // moves every successor's edge itself (ADR-t1521-2 decision 5).
    super::replans::check_task_step(tx, task_id, GuardedStep::Dependency)?;
    super::replans::check_task_step(tx, predecessor_id, GuardedStep::Dependency)?;
    task::check_dependencies_editable(&read_task(tx, task_id)?)?;
    task::check_not_replaced(
        task_id,
        predecessor_id,
        read_task(tx, predecessor_id)?.status(),
    )?;
    // Through goals too: the predecessor may wait for a goal that waits
    // for the task (ADR-0038).
    let cycle = waits_for(tx, Node::Task(predecessor_id), Node::Task(task_id))?;
    task::check_acyclic(task_id, predecessor_id, cycle)?;
    let inserted = tx.execute(
        "INSERT INTO task_dependencies(task_id, predecessor_id) VALUES (?1,?2)
         ON CONFLICT(task_id, predecessor_id) DO NOTHING",
        sql_params![task_id, predecessor_id],
    )?;
    if inserted != 0 {
        touch(tx, task_id, now)?;
        event(
            tx,
            task_id,
            None,
            EventKind::DependencyAdded,
            json!({"predecessor_id": predecessor_id}),
        )?;
    }
    Ok(())
}

pub(super) fn insert_goal_dependency(
    tx: &impl SqlWrite,
    task_id: TaskId,
    goal_id: GoalId,
    now: &str,
) -> Result<()> {
    let task = read_task(tx, task_id)?;
    task::check_dependencies_editable(&task)?;
    goal::check_accepts_dependents(&read_goal(tx, goal_id)?)?;
    task::check_not_own_goal(&task, goal_id)?;
    let cycle = waits_for(tx, Node::Goal(goal_id), Node::Task(task_id))?;
    task::check_goal_acyclic(task_id, goal_id, cycle)?;
    let inserted = tx.execute(
        "INSERT INTO task_goal_dependencies(task_id, goal_id) VALUES (?1,?2)
         ON CONFLICT(task_id, goal_id) DO NOTHING",
        sql_params![task_id, goal_id],
    )?;
    if inserted != 0 {
        touch(tx, task_id, now)?;
        event(
            tx,
            task_id,
            None,
            EventKind::GoalDependencyAdded,
            json!({"goal_id": goal_id}),
        )?;
    }
    Ok(())
}

pub(super) fn touch(tx: &impl SqlWrite, task_id: TaskId, now: &str) -> Result<()> {
    tx.execute(
        "UPDATE tasks SET updated_at=?1 WHERE id=?2",
        sql_params![now, task_id],
    )?;
    Ok(())
}

/// Insert an event of the queue itself (one of [`EventKind::is_queue`]) in
/// the open transaction `tx`, with the session span it opens or closes.
pub(super) fn record_queue_event_in(
    tx: &impl SqlWrite,
    kind: EventKind,
    payload: &Value,
) -> Result<()> {
    crate::domain::check_event_target(kind, None, None)?;
    let id = tx.insert_id(
        "INSERT INTO run_events(kind,payload,actor_role,actor_id,requested_by)
         VALUES (?1,dagq_event_body(?2),dagq_actor_role(),dagq_actor_id(),dagq_requested_by())",
        sql_params![kind.as_str(), serde_json::to_string(payload)?],
    )?;
    super::sessions::follow(tx, EventId::new(id), None, None, kind, payload)
}

pub(super) fn event(
    tx: &impl SqlWrite,
    task_id: TaskId,
    run_id: Option<&RunId>,
    kind: EventKind,
    payload: Value,
) -> Result<()> {
    event_id(tx, task_id, run_id, kind, payload).map(drop)
}

/// [`event`], returning the ID of the event it inserted.
pub(super) fn event_id(
    tx: &impl SqlWrite,
    task_id: TaskId,
    run_id: Option<&RunId>,
    kind: EventKind,
    payload: Value,
) -> Result<EventId> {
    let id = EventId::new(tx.insert_id(
        "INSERT INTO run_events(task_id,run_id,kind,payload,actor_role,actor_id,requested_by)
         VALUES (?1,?2,?3,dagq_event_body(?4),dagq_actor_role(),dagq_actor_id(),dagq_requested_by())",
        sql_params![
            task_id,
            run_id,
            kind.as_str(),
            serde_json::to_string(&payload)?
        ],
    )?);
    // The Claude session spans this event starts or ends (ADR-0048).
    super::sessions::follow(tx, id, Some(task_id), run_id, kind, &payload)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::super::sql_exec::sqlite::port_tx;
    use super::super::sqlite::{self, SqliteQueue};
    use super::*;

    fn queue() -> SqliteQueue {
        let uri = format!("file:/store-writes-{}?vfs=memdb", uuid::Uuid::new_v4());
        SqliteQueue::init(uri).unwrap()
    }

    fn new_task(title: &str, dependencies: Vec<TaskId>) -> NewTask {
        NewTask {
            title: title.into(),
            description: "d".into(),
            acceptance: "a".into(),
            verification_commands: vec!["true".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: None,
            change: None,
            dependencies,
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: None,
            wait_for_build: false,
            execution_class: Default::default(),
        }
    }

    /// Every event writer is callable with SqlWrite as its only bound. Keep
    /// these calls inside the generic body so adding a driver trait fails build.
    #[test]
    fn event_writers_require_only_sql_write() {
        fn write(tx: &impl SqlWrite) -> Result<()> {
            let task = insert_task(tx, new_task("generic", Vec::new()), "now", Creator::Other)?;
            event(tx, task.id(), None, EventKind::TaskCreated, json!({}))?;
            super::super::stranded::record(tx, task.id(), "test", json!({}))?;
            super::super::investigations::settle(tx, 0)?;
            super::super::plan_requests::record_missing_spike_requests(tx, 0)?;
            super::super::draft_planners::insert_draft_origin(
                tx,
                task.id(),
                crate::domain::DraftOrigin::FollowUp,
                &json!({}),
                0,
            )?;
            Ok(())
        }
        let queue = queue();
        let tx = queue.write_tx("generic_event_writers").unwrap();
        write(&tx).unwrap();
    }

    fn opened(tx: &impl SqlRead) -> i64 {
        tx.query_row(
            "SELECT count(*) FROM run_events WHERE kind='session_opened'",
            &[],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// An event opens its session's span through a transaction of the port
    /// as through the wrapper the unmoved modules call, each on the caller's
    /// transaction.
    #[test]
    fn an_event_follows_its_spans_on_each_transaction() {
        let queue = queue();
        let tx = queue.write_tx("store_writes_test").unwrap();
        let task = insert_task(&tx, new_task("t", Vec::new()), "now", Creator::Other).unwrap();
        let review = |id: i64| json!({"proposal_id": 1, "plan_review_id": id, "attempt": 1});
        event(
            &tx,
            task.id(),
            None,
            EventKind::PlanReviewStarted,
            review(1),
        )
        .unwrap();
        assert_eq!(opened(&tx), 1);
        event(
            &port_tx(&tx),
            task.id(),
            None,
            EventKind::PlanReviewStarted,
            review(2),
        )
        .unwrap();
        assert_eq!(opened(&tx), 2);
        sqlite::event(
            &tx,
            task.id(),
            None,
            EventKind::PlanReviewStarted,
            review(3),
        )
        .unwrap();
        assert_eq!(opened(&tx), 3);
    }

    /// A task with a dependency is written with its event and edge, and the
    /// edge back is the cycle the graph refuses.
    #[test]
    fn a_new_task_takes_its_dependencies_and_a_cycle_is_refused() {
        let queue = queue();
        let tx = queue.write_tx("store_writes_test").unwrap();
        let first = insert_task(&tx, new_task("a", Vec::new()), "now", Creator::Other).unwrap();
        let second =
            insert_task(&tx, new_task("b", vec![first.id()]), "now", Creator::Other).unwrap();
        assert_eq!(next_id(&tx, "tasks").unwrap(), second.id().as_i64() + 1);
        assert!(waits_for(&tx, Node::Task(second.id()), Node::Task(first.id())).unwrap());
        assert!(!waits_for(&tx, Node::Task(first.id()), Node::Task(second.id())).unwrap());
        assert!(insert_dependency(&tx, first.id(), second.id(), "now").is_err());
    }
}
