//! The rows the store's modules share, on the execution port
//! ([`super::sql_exec`]): how a column reads as a domain value and a row as
//! a task, a goal, a run or an event, and the shared reads of them. A row
//! the domain refuses to restore is a read error, never a default.
//!
//! This is the one body of each helper: a module on the port calls it with
//! its transaction and row, and the functions of the same name in
//! [`super::sqlite`] hand a rusqlite row or connection to it for a module
//! that has not moved.

use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;

use super::event_actor::event_actor;
use super::sql_dialect::JsonType;
use super::sql_exec::{OptionalRow, SqlError, SqlRead, SqlResult, SqlRow, ValueKind, sql_params};
use crate::domain::event_kind::{self, RecordedKind};
use crate::domain::plan_request::RecordedOrigin;
use crate::domain::worker::{Worker, WorkerMode};
use crate::domain::{
    DomainError, Goal, GoalId, GoalRecord, GoalVerdict, Priority, RunEvent, RunRecord, Task,
    TaskId, TaskRecord, TaskRun, cancel_reason, task_hold::HoldGuard,
};

/// The start of a query of tasks aliased `t`, each with the priority of its
/// goal as `goal_priority`, which [`task_row`] reads to resolve what a task
/// without a priority of its own inherits (ADR-t1639-1 decision 2).
macro_rules! select_tasks {
    () => {
        "SELECT t.*, (SELECT g.priority FROM goals g WHERE g.id = t.goal_id) AS goal_priority,
         (SELECT g.priority_by FROM goals g WHERE g.id = t.goal_id) AS goal_priority_by
         FROM tasks t"
    };
}
pub(super) use select_tasks;

/// The statuses of a replanning operation that guard its task
/// ([`crate::domain::task_replan::ReplanStatus::guards`]), as a list of
/// SQL strings: the ready query and the guard's read name the same ones.
macro_rules! replan_guarding {
    () => {
        "'pause_requested','quiescing','snapshotting','parked','planning','reviewing',
         'awaiting_answer','applying','blocked'"
    };
}
pub(super) use replan_guarding;
/// The conditions of a claimable task `t` but its hold: `ready`, its
/// predecessors completed, its goal dependencies all closed as achieved
/// (ADR-0038), no unfinished run of it, its goal, if any, not a draft
/// (ADR-0024 decision 5), and no replanning operation guarding it
/// (ADR-t1521-2).
macro_rules! ready_conditions {
    () => {
        concat!(
            "
    WHERE t.status = 'ready'
      AND NOT EXISTS (
        SELECT 1 FROM goals g WHERE g.id = t.goal_id AND g.status = 'draft'
      )
      AND NOT EXISTS (
        SELECT 1 FROM task_dependencies d JOIN tasks p ON p.id = d.predecessor_id
        WHERE d.task_id = t.id AND p.status <> 'completed'
      )
      AND NOT EXISTS (
        SELECT 1 FROM task_goal_dependencies gd JOIN goals g ON g.id = gd.goal_id
        WHERE gd.task_id = t.id
          AND NOT (g.closed_at IS NOT NULL AND g.verdict = 'achieved')
      )
      AND NOT EXISTS (
        SELECT 1 FROM task_runs r WHERE r.task_id = t.id
          AND r.status IN ('claimed','starting','running','validating','awaiting_integration',
                           'integrating','needs_session')
      )
      AND NOT EXISTS (
        SELECT 1 FROM replan_operations o WHERE o.task_id = t.id
          AND o.status IN (",
            replan_guarding!(),
            ")
      )"
        )
    };
}
pub(super) use ready_conditions;

/// The tasks that would be ready but for their open hold, each with the
/// hold's ID, in ID order.
const HELD_READY_QUERY: &str = concat!(
    "SELECT t.id, h.id FROM tasks t
     JOIN task_holds h ON h.task_id = t.id AND h.released_at IS NULL",
    ready_conditions!(),
    " ORDER BY t.id"
);

/// A value of the text column `name` that does not convert.
fn text_conversion(
    row: &SqlRow<'_>,
    name: &str,
    cause: impl std::error::Error + Send + Sync + 'static,
) -> SqlError {
    SqlError::Conversion {
        index: row.column_index(name).unwrap_or(0),
        stored: ValueKind::Text,
        cause: Box::new(cause),
    }
}

/// Column `name` read as the domain's enum `T`: a stored value that is not
/// one of `T`'s is a conversion error with [`DomainError::UnknownValue`] as
/// its cause, never a default.
pub(super) fn enum_col<T: FromStr<Err = DomainError>>(
    row: &SqlRow<'_>,
    name: &str,
) -> SqlResult<T> {
    let value: String = row.get(name)?;
    value
        .parse()
        .map_err(|error: DomainError| text_conversion(row, name, error))
}

pub(super) fn json_col<T: DeserializeOwned>(row: &SqlRow<'_>, name: &str) -> SqlResult<T> {
    let value: String = row.get(name)?;
    serde_json::from_str(&value).map_err(|error| text_conversion(row, name, error))
}

/// A nullable column of a [`string_enum!`] type.
pub(super) fn optional_enum_col<T: FromStr<Err = DomainError>>(
    row: &SqlRow<'_>,
    name: &str,
) -> SqlResult<Option<T>> {
    match row.get::<_, Option<String>>(name)? {
        Some(_) => enum_col(row, name).map(Some),
        None => Ok(None),
    }
}

/// A nullable priority column, stored as `low`=0 … `interrupt`=4.
pub(super) fn optional_priority_col(row: &SqlRow<'_>, name: &str) -> SqlResult<Option<Priority>> {
    row.get::<_, Option<i64>>(name)?
        .map(Priority::from_i64)
        .transpose()
        .map_err(restore_error)
}

/// A stored row the domain refuses to restore, reported like a column that
/// does not convert, with the domain's message as the cause.
fn restore_error(error: DomainError) -> SqlError {
    SqlError::Conversion {
        index: 0,
        stored: ValueKind::Null,
        cause: Box::new(error),
    }
}

/// The origin a goal or task row records (ADR-t1975-1 decision 5).
fn origin_cols(row: &SqlRow<'_>) -> SqlResult<RecordedOrigin> {
    Ok(RecordedOrigin {
        origin: enum_col(row, "origin")?,
        kind: optional_enum_col(row, "origin_kind")?,
        request_id: row.get("origin_request_id")?,
    })
}

pub(super) fn task_row(row: &SqlRow<'_>) -> SqlResult<Task> {
    Ok(Task::restore(TaskRecord {
        id: row.get("id")?,
        title: row.get("title")?,
        description: row.get("description")?,
        acceptance: row.get("acceptance")?,
        verification_commands: json_col(row, "verification_commands")?,
        required_evidence: json_col(row, "required_evidence")?,
        paths: json_col(row, "paths")?,
        // NULL inherits the goal's (ADR-t1639-1 decision 2).
        priority: optional_priority_col(row, "priority")?,
        goal_priority: optional_priority_col(row, "goal_priority")?,
        // Any label reads as written (ADR-t980-1), whatever set dagq.toml
        // names now; a value that is not a label reads as none, so the task
        // still restores and the queue keeps claiming.
        change: row
            .get::<_, Option<String>>("change")?
            .and_then(|change| change.parse().ok()),
        // Any value but 0 is declared (ADR-t1632-1); the column has no CHECK.
        wait_for_build: row.get("wait_for_build")?,
        // A class this binary does not know reads as `implementation`
        // (ADR-t1487-1); the column has no CHECK.
        execution_class: row
            .get::<_, String>("execution_class")?
            .parse()
            .unwrap_or_default(),
        // NULL is the provider's default (Claude, headless: ADR-t1340-1): a
        // task that names no mode, and one from before the worker existed;
        // the columns' CHECK keeps any other value out.
        worker: Worker::resolve(
            optional_enum_col(row, "worker_provider")?,
            optional_enum_col(row, "worker_mode")?,
        )
        .map_err(restore_error)?,
        named_mode: optional_enum_col(row, "worker_mode")?,
        status: enum_col(row, "status")?,
        goal_id: row.get("goal_id")?,
        context: row.get("context")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
    .map_err(restore_error)?
    .with_record(
        optional_enum_col(row, "priority_by")?,
        optional_enum_col(row, "goal_priority_by")?,
        origin_cols(row)?,
    ))
}

pub(super) fn goal_row(row: &SqlRow<'_>) -> SqlResult<Goal> {
    let verdict: Option<String> = row.get("verdict")?;
    Ok(Goal::restore(GoalRecord {
        id: row.get("id")?,
        title: row.get("title")?,
        description: row.get("description")?,
        acceptance: row.get("acceptance")?,
        constraints: row.get("constraints")?,
        doc: row.get("doc")?,
        priority: Priority::from_i64(row.get("priority")?).map_err(restore_error)?,
        tags: serde_json::from_str(&row.get::<_, String>("tags")?).map_err(|error| {
            SqlError::Conversion {
                index: 0,
                stored: ValueKind::Text,
                cause: Box::new(error),
            }
        })?,
        status: enum_col(row, "status")?,
        closed_at: row.get("closed_at")?,
        verdict: verdict
            .map(|_| enum_col::<GoalVerdict>(row, "verdict"))
            .transpose()?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
    .map_err(restore_error)?
    .with_record(enum_col(row, "priority_by")?, origin_cols(row)?))
}

/// Reads a run with its queue-local paths resolved under `runs_dir`.
pub(super) fn run_row(runs_dir: &Path) -> impl Fn(&SqlRow<'_>) -> SqlResult<TaskRun> + '_ {
    move |row| Ok(stored_run_row(row)?.relocated(runs_dir))
}

/// Reads a run as stored, paths included: what a command starts from
/// before the store saves its result.
pub(super) fn stored_run_row(row: &SqlRow<'_>) -> SqlResult<TaskRun> {
    TaskRun::restore(RunRecord {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        status: enum_col(row, "status")?,
        requested_provider: enum_col(row, "requested_provider")?,
        actual_provider: enum_col(row, "actual_provider")?,
        worker_mode: optional_enum_col(row, "worker_mode")?.unwrap_or(WorkerMode::Interactive),
        base_commit: row.get("base_commit")?,
        branch: row.get("branch")?,
        worktree_path: row.get("worktree_path")?,
        workspace_id: row.get("workspace_id")?,
        receipt_path: row.get("receipt_path")?,
        log_path: row.get("log_path")?,
        result_commit: row.get("result_commit")?,
        repo_path: row.get("repo_path")?,
        run_dir: row.get("run_dir")?,
        last_error: row.get("last_error")?,
        workspace_closed_at: row.get("workspace_closed_at")?,
        created_at: row.get("created_at")?,
    })
    .map_err(restore_error)
}

pub(super) fn event_row(row: &SqlRow<'_>) -> SqlResult<RunEvent> {
    Ok(RunEvent {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        goal_id: row.get("goal_id")?,
        run_id: row.get("run_id")?,
        kind: RecordedKind::from_name(&row.get::<_, String>("kind")?),
        payload: json_col(row, "payload")?,
        created_at: row.get("created_at")?,
        actor: event_actor(row)?,
    })
}

pub(super) fn read_task(tx: &impl SqlRead, task_id: TaskId) -> Result<Task> {
    tx.query_row(
        concat!(select_tasks!(), " WHERE t.id=?1"),
        sql_params![task_id],
        task_row,
    )
    .optional()?
    .with_context(|| format!("task {task_id} does not exist"))
}

pub(super) fn read_goal(tx: &impl SqlRead, goal_id: GoalId) -> Result<Goal> {
    tx.query_row(
        "SELECT * FROM goals WHERE id=?1",
        sql_params![goal_id],
        goal_row,
    )
    .optional()?
    .with_context(|| format!("goal {goal_id} does not exist"))
}

/// The tasks that would be claimable but for their open hold, each with
/// the guard of its hold, in ID order.
pub(super) fn held_claims(tx: &impl SqlRead) -> Result<Vec<HoldGuard>> {
    Ok(tx.query_map(HELD_READY_QUERY, &[], |r| {
        Ok(HoldGuard {
            task: r.get(0)?,
            hold: r.get(1)?,
        })
    })?)
}

/// What a task's latest cancel recorded (see [`cancel_of`]).
pub(super) struct RecordedCancel {
    pub(super) duplicate_of: Option<TaskId>,
    pub(super) reason: Option<String>,
}

/// What the latest cancel of `task_id` recorded, while that event canceled
/// it: the task it names and its reason, read as
/// [`cancel_reason::Recorded::reason`] reads one recorded before reasons,
/// with whether the task it names was completed before it.
pub(super) fn cancel_of(tx: &impl SqlRead, task_id: TaskId) -> Result<Option<RecordedCancel>> {
    let latest: Option<(i64, String)> = tx
        .query_row(
            &format!(
                "SELECT id, payload FROM run_events WHERE task_id=?1 AND kind='{}'
                 ORDER BY id DESC LIMIT 1",
                event_kind::TASK_STATUS_CHANGED
            ),
            sql_params![task_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((event_id, payload)) = latest else {
        return Ok(None);
    };
    let cancel = cancel_reason::Recorded::of(&serde_json::from_str(&payload)?);
    if !cancel.canceled() {
        return Ok(None);
    }
    let target_completed = match cancel.duplicate_of {
        Some(target) => completed_before(tx, target, event_id)?,
        None => false,
    };
    Ok(Some(RecordedCancel {
        duplicate_of: cancel.duplicate_of,
        reason: cancel.reason(target_completed),
    }))
}

/// Whether `task_id` was completed by an event before `event_id`.
fn completed_before(tx: &impl SqlRead, task_id: TaskId, event_id: i64) -> Result<bool> {
    Ok(tx.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM run_events WHERE task_id=?1 AND kind='{}'
             AND {to}='completed' AND id<?2)",
            event_kind::TASK_STATUS_CHANGED,
            to = tx.dialect().json_field("payload", "$.to", JsonType::Text),
        ),
        sql_params![task_id, event_id],
        |r| r.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use serde_json::json;

    use super::*;
    use crate::application::TaskStore;
    use crate::domain::cancel_reason::NewCancel;
    use crate::domain::event_kind::EventKind;
    use crate::domain::{NewGoal, NewTask, TaskStatus};
    use crate::infrastructure::sql_exec::sqlite::{port_tx, with_row};
    use crate::infrastructure::sqlite::{self, SqliteQueue};

    fn queue(name: &str) -> SqliteQueue {
        let uri = format!("file:/store-rows-{name}-{}?vfs=memdb", uuid::Uuid::new_v4());
        SqliteQueue::init(uri).unwrap()
    }

    fn new_goal() -> NewGoal {
        NewGoal {
            title: "goal".into(),
            description: "d".into(),
            acceptance: "a".into(),
            constraints: "c".into(),
            doc: None,
            draft: false,
            priority: Some(Priority::High),
            tags: Vec::new(),
        }
    }

    fn new_task(goal: GoalId) -> NewTask {
        NewTask {
            title: "task".into(),
            description: "d".into(),
            acceptance: "a".into(),
            verification_commands: vec!["true".into()],
            required_evidence: Vec::new(),
            paths: vec!["src/**".into()],
            priority: None,
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: Some(goal),
            context: "context".into(),
            provider: None,
            worker_mode: None,
            wait_for_build: false,
            execution_class: Default::default(),
        }
    }

    /// A moved reader returns on an entry's transaction, on the read
    /// connection and through its wrapper what the others do.
    #[test]
    fn the_readers_return_the_same_through_the_port_and_the_wrappers() {
        let mut queue = queue("readers");
        let goal = queue.add_goal(new_goal()).unwrap();
        let task = queue.add(new_task(goal.id())).unwrap();
        let tx = queue.read_tx("store_rows_test").unwrap();
        let on_conn: &Connection = &tx;

        let through_entry = read_task(&tx, task.id()).unwrap();
        assert_eq!(through_entry.title(), "task");
        assert_eq!(through_entry.goal_id(), Some(goal.id()));
        let expected = format!("{through_entry:?}");
        let wrapper = sqlite::read_task(&tx, task.id()).unwrap();
        assert_eq!(format!("{wrapper:?}"), expected);
        let read = read_task(on_conn, task.id()).unwrap();
        assert_eq!(format!("{read:?}"), expected);

        let through_entry = read_goal(&tx, goal.id()).unwrap();
        assert_eq!(through_entry.title(), "goal");
        let wrapper = sqlite::read_goal(&tx, goal.id()).unwrap();
        assert_eq!(format!("{wrapper:?}"), format!("{through_entry:?}"));

        let missing = TaskId::new(99);
        for error in [
            read_task(&tx, missing).unwrap_err(),
            sqlite::read_task(&tx, missing).unwrap_err(),
        ] {
            assert_eq!(error.to_string(), "task 99 does not exist");
        }
        for error in [
            read_goal(&tx, GoalId::new(99)).unwrap_err(),
            sqlite::read_goal(&tx, GoalId::new(99)).unwrap_err(),
        ] {
            assert_eq!(error.to_string(), "goal 99 does not exist");
        }
        assert!(held_claims(&tx).unwrap().is_empty());
        assert!(sqlite::held_claims(&tx).unwrap().is_empty());
    }

    /// An event row reads the same both ways, its actor included, and a
    /// query that does not select the actor's columns has none.
    #[test]
    fn an_event_row_reads_the_same_through_the_port_and_the_wrapper() {
        let queue = queue("events");
        queue
            .record_queue_event(EventKind::ObserveStarted, json!({"n": 1}))
            .unwrap();
        let sql = "SELECT * FROM run_events ORDER BY id DESC LIMIT 1";
        let tx = port_tx(&queue.conn);
        let through_port = tx.query_row(sql, &[], event_row).unwrap();
        let actor = through_port.actor.clone().unwrap();
        assert_eq!((actor.role.as_str(), actor.id.as_str()), ("user", "user"));
        let through_wrapper = queue.conn.query_row(sql, [], sqlite::event_row).unwrap();
        assert_eq!(format!("{through_port:?}"), format!("{through_wrapper:?}"));
        let no_actor = tx
            .query_row(
                "SELECT id, task_id, goal_id, run_id, kind, payload, created_at FROM run_events",
                &[],
                event_row,
            )
            .unwrap();
        assert!(no_actor.actor.is_none());
    }

    /// A run reads the same both ways, with its paths under `runs_dir` from
    /// `run_row`; the row comes from the query alone.
    #[test]
    fn a_run_row_reads_the_same_through_the_port_and_the_wrapper() {
        let queue = queue("runs");
        let sql = "SELECT 'r1' AS id, 4 AS task_id, 'running' AS status,
                'claude' AS requested_provider, 'claude' AS actual_provider,
                'headless' AS worker_mode,
                '0123456789abcdef0123456789abcdef01234567' AS base_commit,
                NULL AS branch, NULL AS worktree_path, NULL AS workspace_id,
                NULL AS receipt_path, NULL AS log_path, NULL AS result_commit,
                NULL AS repo_path, 'old/r1' AS run_dir, NULL AS last_error,
                NULL AS workspace_closed_at, 'now' AS created_at";
        let tx = port_tx(&queue.conn);
        let stored = tx.query_row(sql, &[], stored_run_row).unwrap();
        let wrapper = queue
            .conn
            .query_row(sql, [], sqlite::stored_run_row)
            .unwrap();
        assert_eq!(format!("{stored:?}"), format!("{wrapper:?}"));
        let runs_dir = Path::new("/runs");
        let relocated = tx.query_row(sql, &[], run_row(runs_dir)).unwrap();
        let wrapper = queue
            .conn
            .query_row(sql, [], sqlite::run_row(runs_dir))
            .unwrap();
        assert_eq!(format!("{relocated:?}"), format!("{wrapper:?}"));
    }

    /// A value the domain does not know is an error of the column, never a
    /// default, with the same text and cause both ways; `NULL` is `None`.
    #[test]
    fn a_column_helper_reads_and_refuses_the_same_through_the_port_and_the_wrapper() {
        let queue = queue("columns");
        let conn = &queue.conn;
        let tx = port_tx(conn);
        let sql = "SELECT 'ready' AS status, 'nope' AS bad, NULL AS absent,
                '[1,2]' AS list, 'not json' AS broken, 3 AS priority, 9 AS bad_priority";
        let both = |read: &dyn Fn(&SqlRow<'_>) -> SqlResult<String>| {
            let port = tx.query_row(sql, &[], read).map_err(|e| e.to_string());
            let wrapper = conn
                .query_row(sql, [], |row| with_row(row, read))
                .map_err(|e| e.to_string());
            assert_eq!(port, wrapper);
            port
        };
        let status = |name: &'static str| {
            both(&move |row| Ok(format!("{:?}", optional_enum_col::<TaskStatus>(row, name)?)))
        };
        assert_eq!(status("absent").unwrap(), "None");
        assert_eq!(status("status").unwrap(), "Some(Ready)");
        let bad =
            both(&|row| Ok(format!("{:?}", enum_col::<TaskStatus>(row, "bad")?))).unwrap_err();
        assert!(
            bad.starts_with("Conversion error from type Text at index: 1, "),
            "{bad}"
        );
        let wrapper = conn
            .query_row(sql, [], |row| sqlite::enum_col::<TaskStatus>(row, "bad"))
            .unwrap_err();
        assert_eq!(wrapper.to_string(), bad);
        assert!(matches!(
            wrapper,
            rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, _)
        ));
        let list = |name: &'static str| {
            both(&move |row| Ok(format!("{:?}", json_col::<Vec<i64>>(row, name)?)))
        };
        assert_eq!(list("list").unwrap(), "[1, 2]");
        let broken = list("broken").unwrap_err();
        assert!(
            broken.starts_with("Conversion error from type Text at index: 4, "),
            "{broken}"
        );
        let priority = |name: &'static str| {
            both(&move |row| Ok(format!("{:?}", optional_priority_col(row, name)?)))
        };
        assert_eq!(
            priority("priority").unwrap(),
            format!("{:?}", Some(Priority::from_i64(3).unwrap()))
        );
        let refused = priority("bad_priority").unwrap_err();
        assert!(
            refused.starts_with("Conversion error from type Null at index: 0, "),
            "{refused}"
        );
    }

    /// What a task's cancel recorded reads the same both ways.
    #[test]
    fn a_cancel_reads_the_same_through_the_port_and_the_wrapper() {
        let mut queue = queue("cancels");
        let goal = queue.add_goal(new_goal()).unwrap();
        let open = queue.add(new_task(goal.id())).unwrap();
        let canceled = queue.add(new_task(goal.id())).unwrap();
        let cancel = NewCancel::by_hand(Some("not needed".to_owned()), None, None);
        queue.cancel(canceled.id(), cancel).unwrap();
        let tx = port_tx(&queue.conn);
        assert!(cancel_of(&tx, open.id()).unwrap().is_none());
        assert!(sqlite::cancel_of(&queue.conn, open.id()).unwrap().is_none());
        let port = cancel_of(&tx, canceled.id()).unwrap().unwrap();
        let wrapper = sqlite::cancel_of(&queue.conn, canceled.id())
            .unwrap()
            .unwrap();
        assert_eq!(port.reason.as_deref(), Some("not needed"));
        assert_eq!(port.reason, wrapper.reason);
        assert_eq!(port.duplicate_of, wrapper.duplicate_of);
    }

    /// A driver's failure in a moved reader comes back to an unmoved caller
    /// as rusqlite's own error.
    #[test]
    fn a_reader_s_failure_is_rusqlite_s_through_the_wrapper() {
        let queue = queue("failure");
        queue
            .conn
            .execute_batch("PRAGMA foreign_keys = OFF; DROP TABLE task_holds;")
            .unwrap();
        let error = sqlite::held_claims(&queue.conn).unwrap_err();
        assert!(
            error.downcast_ref::<rusqlite::Error>().is_some(),
            "{error:#}"
        );
        assert!(error.to_string().contains("task_holds"), "{error:#}");
    }
}
