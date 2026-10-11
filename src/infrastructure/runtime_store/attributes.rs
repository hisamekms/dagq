//! The baselines of the attributes the measurement folds
//! (`domain::ledger::attributes` and `domain::ledger::findings`,
//! docs/design/attribute-records.md): a task, goal or finding whose
//! records do not give what the state tables hold (one a binary before the
//! record made or changed) gets its values now, read and written in one
//! transaction.

use std::collections::BTreeMap;

use anyhow::Result;
use rusqlite::{Connection, TransactionBehavior};
use serde_json::{Value, json};

use super::super::findings::{finding_event, finding_row};
use super::super::sqlite::{SqliteQueue, event, event_row, goal_event, read_goal};
use super::run_log::queue_event;
use crate::domain::ledger::attributes::{
    Attributes, Folded, GoalAttributes, KINDS, TaskAttributes, fold,
};
use crate::domain::ledger::findings;
use crate::domain::plan_request::PriorityBy;
use crate::domain::{EventKind, Finding, GoalId, Priority, RunEvent, RunId, RunStatus, TaskId};

impl SqliteQueue {
    /// Compare what the records fold to with what the state tables hold,
    /// and write `attributes_baseline` with the state's values on every
    /// task and goal that differs, then the queue's mark of the baseline
    /// that closes the group: how many tasks and goals, none when every one
    /// agrees.
    /// The comparison is read first without the write lock and made again
    /// inside the write. Idempotent: after it the records agree.
    pub fn record_attributes_baseline(&self) -> Result<Option<(usize, usize)>> {
        {
            let read =
                rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
            let (tasks, goals) = differing(&read)?;
            read.commit()?;
            if tasks.is_empty() && goals.is_empty() {
                return Ok(None);
            }
        }
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let (tasks, goals) = differing(&tx)?;
        if tasks.is_empty() && goals.is_empty() {
            return Ok(None);
        }
        for &goal in goals.keys() {
            let payload = json!({"goal": read_goal(&tx, goal)?});
            goal_event(&tx, goal, EventKind::AttributesBaseline, payload)?;
        }
        for (&task, attributes) in &tasks {
            event(
                &tx,
                task,
                None,
                EventKind::AttributesBaseline,
                task_payload(attributes),
            )?;
        }
        // The mark closes the group: the fold takes the group's records
        // into a window only when the window holds the mark.
        queue_event(
            &tx,
            EventKind::AttributesBaseline,
            &json!({"tasks": tasks.len(), "goals": goals.len()}),
        )?;
        tx.commit()?;
        Ok(Some((tasks.len(), goals.len())))
    }
}

impl SqliteQueue {
    /// Compare the findings the records fold to with the `findings` rows,
    /// and write `finding_baseline` with the row's attributes on every
    /// finding that differs, settled ones too, then the queue's mark that
    /// closes the group: how many findings, none when every one agrees.
    /// The comparison is read first without the write lock and made again
    /// inside the write. Idempotent: after it the records agree.
    pub fn record_findings_baseline(&self) -> Result<Option<usize>> {
        {
            let read =
                rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
            let differ = differing_findings(&read)?;
            read.commit()?;
            if differ.is_empty() {
                return Ok(None);
            }
        }
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let differ = differing_findings(&tx)?;
        if differ.is_empty() {
            return Ok(None);
        }
        for finding in &differ {
            finding_event(
                &tx,
                finding,
                EventKind::FindingBaseline,
                json!({
                    "finding_id": finding.id,
                    (findings::VALUES): finding.attributes(),
                }),
            )?;
        }
        // The mark closes the group, as the attributes' does.
        queue_event(
            &tx,
            EventKind::FindingBaseline,
            &json!({"findings": differ.len()}),
        )?;
        tx.commit()?;
        Ok(Some(differ.len()))
    }
}

/// The findings whose folded attributes differ from their row's, as
/// stored.
fn differing_findings(conn: &Connection) -> Result<Vec<Finding>> {
    let folded = match findings::fold(&events_of(conn, findings::KINDS)?, None) {
        findings::Folded::Findings(folded) => folded,
        findings::Folded::BeforeBaseline => findings::Findings::default(),
    };
    let rows: Vec<Finding> = conn
        .prepare("SELECT * FROM findings ORDER BY id")?
        .query_map([], finding_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows
        .into_iter()
        .filter(|row| folded.findings.get(&row.id) != Some(&row.attributes()))
        .collect())
}

/// The events of `kinds`, ascending id.
fn events_of(conn: &Connection, kinds: &[&str]) -> Result<Vec<RunEvent>> {
    let marks = vec!["?"; kinds.len()].join(",");
    Ok(conn
        .prepare(&format!(
            "SELECT * FROM run_events WHERE kind IN ({marks}) ORDER BY id"
        ))?
        .query_map(rusqlite::params_from_iter(kinds), event_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

type Differing = (
    BTreeMap<TaskId, TaskAttributes>,
    BTreeMap<GoalId, GoalAttributes>,
);

/// The tasks and goals whose folded attributes differ from the state's,
/// with the state's.
fn differing(conn: &Connection) -> Result<Differing> {
    let folded = match fold(&events_of(conn, KINDS)?, None) {
        Folded::Attributes(attributes) => attributes,
        Folded::BeforeBaseline => Attributes::default(),
    };
    let state = state_attributes(conn)?;
    let tasks = state
        .tasks
        .into_iter()
        .filter(|(id, task)| folded.tasks.get(id) != Some(task))
        .collect();
    let goals = state
        .goals
        .into_iter()
        .filter(|(id, goal)| folded.goals.get(id) != Some(goal))
        .collect();
    Ok((tasks, goals))
}

/// The attributes as the state tables hold them, read as the store's
/// readers read them.
fn state_attributes(conn: &Connection) -> Result<Attributes> {
    let mut attributes = Attributes::default();
    let rows: Vec<(TaskId, TaskRow)> = conn
        .prepare(
            "SELECT id, status, goal_id, title, change, execution_class, priority, priority_by
             FROM tasks ORDER BY id",
        )?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                TaskRow {
                    status: row.get(1)?,
                    goal_id: row.get(2)?,
                    title: row.get(3)?,
                    change: row.get(4)?,
                    execution_class: row.get(5)?,
                    priority: row.get(6)?,
                    priority_by: row.get(7)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (id, row) in rows {
        let own_priority = row.priority.map(Priority::from_i64).transpose()?;
        attributes.tasks.insert(
            id,
            TaskAttributes {
                status: row.status.parse()?,
                goal_id: row.goal_id,
                title: row.title,
                change: row.change.and_then(|change| change.parse().ok()),
                execution_class: row.execution_class.parse().unwrap_or_default(),
                own_priority,
                own_priority_by: own_priority
                    .and(row.priority_by.and_then(|by| by.parse::<PriorityBy>().ok())),
                origin: None,
                depends_on: Default::default(),
                goal_dependencies: Default::default(),
                unfinished_run: None,
            },
        );
    }
    let pairs = |sql: &str| -> Result<Vec<(TaskId, i64)>> {
        Ok(conn
            .prepare(sql)?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    };
    for (task, predecessor) in pairs("SELECT task_id, predecessor_id FROM task_dependencies")? {
        if let Some(task) = attributes.tasks.get_mut(&task) {
            task.depends_on.insert(TaskId::new(predecessor));
        }
    }
    for (task, goal) in pairs("SELECT task_id, goal_id FROM task_goal_dependencies")? {
        if let Some(task) = attributes.tasks.get_mut(&task) {
            task.goal_dependencies.insert(GoalId::new(goal));
        }
    }
    let texts = |sql: &str| -> Result<Vec<(TaskId, String, String)>> {
        Ok(conn
            .prepare(sql)?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?)
    };
    for (task, origin, _) in texts("SELECT task_id, origin, '' FROM draft_origins")? {
        if let Some(task) = attributes.tasks.get_mut(&task) {
            task.origin = origin.parse().ok();
        }
    }
    // The latest unfinished run of each task (a task has one at most).
    for (task, run, status) in texts("SELECT task_id, id, status FROM task_runs ORDER BY rowid")? {
        if status
            .parse::<RunStatus>()
            .is_ok_and(RunStatus::is_unfinished)
            && let Some(task) = attributes.tasks.get_mut(&task)
        {
            task.unfinished_run = Some(RunId::new(run)?);
        }
    }
    let goals: Vec<GoalId> = conn
        .prepare("SELECT id FROM goals ORDER BY id")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in goals {
        let goal = read_goal(conn, id)?;
        attributes.goals.insert(
            id,
            GoalAttributes {
                title: goal.title().to_owned(),
                status: goal.status(),
                priority: goal.priority(),
                priority_by: goal.priority_by(),
                tags: goal.tags().to_vec(),
                verdict: goal.verdict(),
                closed: goal.is_closed(),
            },
        );
    }
    Ok(attributes)
}

/// A task's columns as stored.
struct TaskRow {
    status: String,
    goal_id: Option<GoalId>,
    title: String,
    change: Option<String>,
    execution_class: String,
    priority: Option<i64>,
    priority_by: Option<String>,
}

/// The task's attributes in the shape of `task_created` with its origin,
/// predecessors, goal dependencies and unfinished run.
fn task_payload(task: &TaskAttributes) -> Value {
    json!({
        "status": task.status,
        "goal_id": task.goal_id,
        "title": task.title,
        "change": task.change.as_ref().map(|change| change.as_str()),
        "execution_class": task.execution_class.as_str(),
        "priority": task.own_priority,
        "priority_by": task.own_priority_by.map(PriorityBy::as_str),
        "origin": task.origin.map(|origin| origin.as_str()),
        "depends_on": task.depends_on,
        "goal_dependencies": task.goal_dependencies,
        "unfinished_run": task.unfinished_run,
    })
}
