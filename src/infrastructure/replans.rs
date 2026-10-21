//! The replanning operations (ADR-t1521-1, ADR-t1521-2):
//! `replan_operations` with their condition mappings (`replan_mappings`)
//! and replacements (`replan_replacements`), and the planning request a
//! person's request on a run records (`plan_requests.replan_operation_id`).
//! Opening one is one write transaction that inserts it only when its run
//! has none open, so two requests on one run (two supervisors, a person
//! and a worker at once) open one. The guard every ordinary step of a task
//! asks ([`check_task_step`]) reads the same rows, and the task's hold,
//! inside the step's own transaction. The condition mappings are written
//! only through [`write_mappings`], which refuses a second deletion of one
//! acceptance item inside the writing transaction: the table's key does
//! not compare a deletion's NULL child task.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::json;

use super::plan_requests::read_request;
use super::sqlite::{
    SqliteQueue, enum_col, event, json_col, read_task, record_queue_event_in, replan_guarding,
};
use crate::domain::event_kind::EventKind;
use crate::domain::plan_request::RequestRef;
use crate::domain::task_replan::{
    self, GoalConditions, GuardedStep, NewReplan, OriginalConditions, ReplanGuard, ReplanMove,
    ReplanOperation, ReplanOrigin, ReplanRecorded, ReplanStatus, acceptance_items,
};
use crate::domain::{GoalId, ReplanId, RunId, RunStatus, TaskId, TaskStatus};

/// The open operations (not closed), oldest first.
const OPEN: &str = "SELECT * FROM replan_operations WHERE closed_at IS NULL ORDER BY id";

impl SqliteQueue {
    /// Record `new` from `origin` as `by_role` with actor id `by_id`: a new
    /// `candidate` operation on its run with the task's and its goal's
    /// conditions as they are now, or the run's open one when it has one
    /// (`created` false). A person's request (`manual`) also records the
    /// operation's planning request, once per operation, whose planner
    /// waits for the operation's snapshot. `replan_requested` on the run
    /// records each call.
    pub fn record_replan(
        &mut self,
        new: &NewReplan,
        origin: ReplanOrigin,
        by_role: &str,
        by_id: &str,
    ) -> Result<ReplanRecorded> {
        new.validate()?;
        let now = self.generators.clock.now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (task_id, status): (TaskId, String) = tx
            .query_row(
                "SELECT task_id, status FROM task_runs WHERE id=?1",
                [&new.run],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .with_context(|| format!("run {} does not exist", new.run))?;
        let latest: RunId = tx.query_row(
            "SELECT id FROM task_runs WHERE task_id=?1 ORDER BY rowid DESC LIMIT 1",
            [task_id],
            |r| r.get(0),
        )?;
        let task = read_task(&tx, task_id)?;
        let existing = tx
            .query_row(
                "SELECT * FROM replan_operations WHERE run_id=?1 AND closed_at IS NULL
                 ORDER BY id LIMIT 1",
                [&new.run],
                operation_row,
            )
            .optional()?;
        let (id, created) = match existing {
            Some(op) => (op.id, false),
            None => {
                task_replan::check_replannable(
                    &new.run,
                    status.parse::<RunStatus>()?,
                    task.status(),
                    latest == new.run,
                )?;
                let conditions = conditions_of(&tx, task_id)?;
                let diagnosis = task_replan::Diagnosis {
                    evidence: new.evidence.clone(),
                    ..task_replan::Diagnosis::default()
                };
                tx.execute(
                    "INSERT INTO replan_operations(task_id, run_id, goal_id, origin, status,
                       generation, reason, conditions, task_version, goal_acceptance_version,
                       diagnosis, requested_by, requested_by_id, created_at, updated_at)
                     VALUES (?1,?2,?3,?4,'candidate',1,?5,?6,?7,?8,?9,?10,?11,?12,?12)",
                    params![
                        task_id,
                        new.run,
                        task.goal_id(),
                        origin.as_str(),
                        new.reason,
                        serde_json::to_string(&conditions)?,
                        conditions.version(),
                        conditions.goal.as_ref().map(|goal| goal.acceptance_version),
                        serde_json::to_string(&diagnosis)?,
                        by_role,
                        by_id,
                        now,
                    ],
                )?;
                (ReplanId::new(tx.last_insert_rowid()), true)
            }
        };
        let mut op = read_operation(&tx, id)?;
        if origin == ReplanOrigin::Manual && op.request_id.is_none() {
            let request = record_request(&tx, &op, new, by_role, by_id, now)?;
            tx.execute(
                "UPDATE replan_operations SET request_id=?1, updated_at=?2 WHERE id=?3",
                params![request, now, id],
            )?;
            op = read_operation(&tx, id)?;
        }
        event(
            &tx,
            task_id,
            Some(&new.run),
            EventKind::ReplanRequested,
            json!({"operation_id": id, "origin": origin, "created": created,
                   "status": op.status, "reason": new.reason, "evidence": new.evidence,
                   "request_id": op.request_id}),
        )?;
        let request = op
            .request_id
            .map(|request| read_request(&tx, request))
            .transpose()?;
        tx.commit()?;
        Ok(ReplanRecorded {
            operation: op,
            created,
            request,
        })
    }

    /// The operations, oldest first: the open ones, or every one with
    /// `all`.
    pub fn replan_operations(&self, all: bool) -> Result<Vec<ReplanOperation>> {
        let sql = if all {
            "SELECT * FROM replan_operations ORDER BY id"
        } else {
            OPEN
        };
        let ops: Vec<ReplanOperation> = self
            .conn
            .prepare(sql)?
            .query_map([], operation_row)?
            .collect::<rusqlite::Result<_>>()?;
        ops.into_iter()
            .map(|op| with_outcome(&self.conn, op))
            .collect()
    }

    /// The open operations, oldest first, each with its task's status
    /// now: what `status` and `doctor` show.
    pub fn open_replans(&self) -> Result<Vec<(ReplanOperation, TaskStatus)>> {
        self.replan_operations(false)?
            .into_iter()
            .map(|op| {
                let status = read_task(&self.conn, op.task_id)?.status();
                Ok((op, status))
            })
            .collect()
    }

    /// Operation `id`; a missing one is an error.
    pub fn replan_operation(&self, id: ReplanId) -> Result<ReplanOperation> {
        read_operation(&self.conn, id)
    }

    /// Move operation `id`, read under `generation`, as `step` says: the
    /// trusted runtime's only write of an operation once it is open
    /// ([`task_replan::check_move`]), with `replan_status_changed` in the
    /// same transaction. The pause's fence raises the generation; a closed
    /// status closes it.
    pub fn move_replan(
        &mut self,
        id: ReplanId,
        generation: i64,
        step: &ReplanMove,
    ) -> Result<ReplanOperation> {
        let now = self.generators.clock.now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let moved = move_in(&tx, id, generation, step, now)?;
        tx.commit()?;
        Ok(moved)
    }

    /// Add `mappings` to operation `id`'s condition mappings in one write
    /// transaction ([`write_mappings`]), and the operation as it is then.
    pub fn record_replan_mappings(
        &mut self,
        id: ReplanId,
        mappings: &[task_replan::ConditionMapping],
    ) -> Result<ReplanOperation> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        write_mappings(&tx, id, mappings)?;
        let op = read_operation(&tx, id)?;
        tx.commit()?;
        Ok(op)
    }

    /// The open operations whose run the trusted runtime parks now
    /// ([`ReplanStatus::parks`]), oldest first.
    pub fn parking_replans(&self) -> Result<Vec<ReplanOperation>> {
        Ok(self
            .replan_operations(false)?
            .into_iter()
            .filter(|op| op.status.parks())
            .collect())
    }

    /// Every operation on run `run`, oldest first, with its outcome.
    pub fn replans_of_run(&self, run: &RunId) -> Result<Vec<ReplanOperation>> {
        let ops: Vec<ReplanOperation> = self
            .conn
            .prepare("SELECT * FROM replan_operations WHERE run_id=?1 ORDER BY id")?
            .query_map([run], operation_row)?
            .collect::<rusqlite::Result<_>>()?;
        ops.into_iter()
            .map(|op| with_outcome(&self.conn, op))
            .collect()
    }

    /// The operation that guards `task` now, if any.
    pub fn replan_guard(&self, task: TaskId) -> Result<Option<ReplanGuard>> {
        guard_of(&self.conn, task)
    }
}

impl crate::application::commands::replans::ReplanStore for SqliteQueue {
    fn run_task(&self, run: &RunId) -> Result<Option<TaskId>> {
        Ok(self
            .conn
            .query_row("SELECT task_id FROM task_runs WHERE id=?1", [run], |r| {
                r.get(0)
            })
            .optional()?)
    }

    fn record_replan(
        &mut self,
        new: &NewReplan,
        origin: ReplanOrigin,
        by_role: &str,
        by_id: &str,
    ) -> Result<ReplanRecorded> {
        SqliteQueue::record_replan(self, new, origin, by_role, by_id)
    }

    fn replan_operation(&self, id: ReplanId) -> Result<ReplanOperation> {
        SqliteQueue::replan_operation(self, id)
    }

    fn move_replan(
        &mut self,
        id: ReplanId,
        generation: i64,
        step: &ReplanMove,
    ) -> Result<ReplanOperation> {
        SqliteQueue::move_replan(self, id, generation, step)
    }
}

/// Move operation `id`, read under `generation`, as `step` says, on
/// `conn` inside the caller's transaction ([`SqliteQueue::move_replan`]):
/// `replan_status_changed` is written with it. A move that closes the
/// operation before it holds a snapshot (`continuing`, or `withdrawn` and
/// `continuing_after_park` out of a `blocked` one that never parked)
/// declines its still `open` planning request with the outcome as the
/// reason ([`super::plan_requests::decline_unplanned`]), and the event
/// names it (`request_declined`). `blocked` is not an end: one that goes
/// back to the snapshot keeps its request for the planner. A move of an
/// operation that holds a snapshot leaves its request as it is.
pub(crate) fn move_in(
    conn: &Connection,
    id: ReplanId,
    generation: i64,
    step: &ReplanMove,
    now: i64,
) -> Result<ReplanOperation> {
    let op = read_operation(conn, id)?;
    task_replan::check_move(&op, generation, step)?;
    // A closed operation reopened (a restore that failed) must stay the
    // run's one open operation.
    if op.status.is_closed() && !step.to.is_closed() {
        let other: Option<ReplanId> = conn
            .query_row(
                "SELECT id FROM replan_operations WHERE run_id=?1 AND closed_at IS NULL AND id<>?2
                 LIMIT 1",
                params![op.run_id, id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(other) = other {
            anyhow::bail!(
                "replan operation {id} cannot be reopened: run {} has operation {other} open",
                op.run_id
            );
        }
    }
    let next = task_replan::generation_after(op.status, step.to, op.generation);
    let return_phase = (step.to == ReplanStatus::Blocked).then_some(op.status);
    let mut diagnosis = op.diagnosis.clone();
    // A diagnosis keeps the evidence the request named.
    if let Some(made) = &step.diagnosis {
        let mut evidence = diagnosis.evidence.clone();
        for id in &made.evidence {
            if !evidence.contains(id) {
                evidence.push(*id);
            }
        }
        diagnosis = task_replan::Diagnosis {
            evidence,
            ..made.clone()
        };
    }
    conn.execute(
        "UPDATE replan_operations SET status=?1, status_reason=?2, generation=?3,
           return_phase=?4, snapshot_id=COALESCE(?5, snapshot_id), diagnosis=?6,
           updated_at=?7, closed_at=?8
         WHERE id=?9 AND generation=?10",
        params![
            step.to.as_str(),
            step.reason,
            next,
            return_phase.map(ReplanStatus::as_str),
            step.snapshot_id,
            serde_json::to_string(&diagnosis)?,
            now,
            step.to.is_closed().then_some(now),
            id,
            generation,
        ],
    )?;
    let declined = match op.request_id {
        Some(request)
            if step
                .to
                .ends_request_unplanned(op.snapshot_id.is_some() || step.snapshot_id.is_some()) =>
        {
            super::plan_requests::decline_unplanned(conn, request, id, step.to, &step.reason, now)?
                .then_some(request)
        }
        _ => None,
    };
    let mut payload = json!({"operation_id": id, "from": op.status, "to": step.to,
                             "reason": step.reason, "generation": next,
                             "snapshot_id": step.snapshot_id});
    if let Some(request) = declined {
        payload["request_declined"] = json!(request);
    }
    event(
        conn,
        op.task_id,
        Some(&op.run_id),
        EventKind::ReplanStatusChanged,
        payload,
    )?;
    read_operation(conn, id)
}

/// Add `mappings` to operation `id`'s condition mappings on `conn` inside
/// the caller's transaction (the apply writes them with its replacement).
/// The operation's mappings are read in the same transaction, and
/// [`task_replan::check_mappings`] refuses a second deletion of one item
/// before anything is written.
pub(crate) fn write_mappings(
    conn: &Connection,
    id: ReplanId,
    mappings: &[task_replan::ConditionMapping],
) -> Result<()> {
    let op = read_operation(conn, id)?;
    task_replan::check_mappings(id, &op.mappings, mappings)?;
    let mut insert = conn.prepare(
        "INSERT INTO replan_mappings(operation_id, ordinal, hash, child_task_id, child_ordinal,
           disposition, authority) VALUES (?1,?2,?3,?4,?5,?6,?7)",
    )?;
    for mapping in mappings {
        insert.execute(params![
            id,
            i64::try_from(mapping.ordinal)?,
            mapping.hash,
            mapping.child_task_id,
            mapping.child_ordinal.map(i64::try_from).transpose()?,
            mapping.disposition,
            mapping
                .authority
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        ])?;
    }
    Ok(())
}

/// The operation that guards `task` now, read on `conn` (inside the
/// caller's transaction when it has one).
pub(super) fn guard_of(conn: &Connection, task: TaskId) -> Result<Option<ReplanGuard>> {
    Ok(conn
        .query_row(
            concat!(
                "SELECT id, status FROM replan_operations WHERE task_id=?1 AND status IN (",
                replan_guarding!(),
                ") ORDER BY id LIMIT 1"
            ),
            [task],
            |r| {
                Ok((
                    r.get::<_, ReplanId>(0)?,
                    enum_col::<ReplanStatus>(r, "status")?,
                ))
            },
        )
        .optional()?
        .map(|(operation, status)| ReplanGuard {
            task,
            operation,
            status,
        }))
}

/// Refuse `step` of `task` while an operation or a hold guards it
/// ([`task_replan::check_step`]), both read on `conn` inside the step's
/// own transaction.
pub(super) fn check_task_step(conn: &Connection, task: TaskId, step: GuardedStep) -> Result<()> {
    let guards = task_replan::TaskGuards {
        replan: guard_of(conn, task)?,
        hold: super::task_holds::guard_of(conn, task)?,
    };
    task_replan::check_step(&guards, step)?;
    Ok(())
}

/// The operations on `task`, oldest first, with their outcome.
pub(super) fn operations_of(conn: &Connection, task: TaskId) -> Result<Vec<ReplanOperation>> {
    let ops: Vec<ReplanOperation> = conn
        .prepare("SELECT * FROM replan_operations WHERE task_id=?1 ORDER BY id")?
        .query_map([task], operation_row)?
        .collect::<rusqlite::Result<_>>()?;
    ops.into_iter().map(|op| with_outcome(conn, op)).collect()
}

/// The open operation on `task`, if any: what `list` marks it with.
pub(super) fn open_operation_of(
    conn: &Connection,
    task: TaskId,
) -> Result<Option<ReplanOperation>> {
    Ok(conn
        .query_row(
            "SELECT * FROM replan_operations WHERE task_id=?1 AND closed_at IS NULL
             ORDER BY id LIMIT 1",
            [task],
            operation_row,
        )
        .optional()?)
}

/// The task's and its goal's conditions as they are on `conn` now.
fn conditions_of(conn: &Connection, task_id: TaskId) -> Result<OriginalConditions> {
    let task = read_task(conn, task_id)?;
    let ids = |sql: &str| -> Result<Vec<TaskId>> {
        Ok(conn
            .prepare(sql)?
            .query_map([task_id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    };
    let predecessors = ids(
        "SELECT predecessor_id FROM task_dependencies WHERE task_id=?1 ORDER BY predecessor_id",
    )?;
    let successors =
        ids("SELECT task_id FROM task_dependencies WHERE predecessor_id=?1 ORDER BY task_id")?;
    let goal = task
        .goal_id()
        .map(|goal: GoalId| {
            conn.query_row(
                "SELECT acceptance, acceptance_version FROM goals WHERE id=?1",
                [goal],
                |r| {
                    Ok(GoalConditions {
                        id: goal,
                        acceptance: acceptance_items(&r.get::<_, String>(0)?),
                        acceptance_version: r.get(1)?,
                    })
                },
            )
        })
        .transpose()?;
    Ok(OriginalConditions {
        title: task.title().to_owned(),
        description: task.description().to_owned(),
        acceptance: acceptance_items(task.acceptance()),
        paths: task.paths().to_vec(),
        verify: task.verification_commands().to_vec(),
        evidence: serde_json::to_value(task.required_evidence())?,
        predecessors,
        successors,
        goal,
    })
}

/// Record the planning request of a person's request on `op`'s run: the
/// person's words, referring to the run, its task and its goal, carrying
/// the operation, `open` with `request_recorded`.
fn record_request(
    conn: &Connection,
    op: &ReplanOperation,
    new: &NewReplan,
    by_role: &str,
    by_id: &str,
    now: i64,
) -> Result<crate::domain::RequestId> {
    let mut refs = vec![
        RequestRef::Run(op.run_id.clone()),
        RequestRef::Task(op.task_id),
    ];
    refs.extend(op.goal_id.map(RequestRef::Goal));
    refs.extend(new.evidence.iter().copied().map(RequestRef::Event));
    conn.execute(
        "INSERT INTO plan_requests(text, refs, requested_by, requested_by_id, status,
           created_at, updated_at, replan_operation_id) VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?5, ?6)",
        params![
            new.reason,
            serde_json::to_string(&refs)?,
            by_role,
            by_id,
            now,
            op.id
        ],
    )?;
    let id = crate::domain::RequestId::new(conn.last_insert_rowid());
    record_queue_event_in(
        conn,
        EventKind::RequestRecorded,
        &json!({"request_id": id, "refs": refs, "requested_by": by_role,
                "replan_operation_id": op.id}),
    )?;
    Ok(id)
}

pub(super) fn read_operation(conn: &Connection, id: ReplanId) -> Result<ReplanOperation> {
    let op = conn
        .query_row(
            "SELECT * FROM replan_operations WHERE id=?1",
            [id],
            operation_row,
        )
        .optional()?
        .with_context(|| format!("replan operation {id} does not exist"))?;
    with_outcome(conn, op)
}

/// `op` with its condition mappings and the tasks that replaced its task.
fn with_outcome(conn: &Connection, mut op: ReplanOperation) -> Result<ReplanOperation> {
    op.mappings = conn
        .prepare(
            "SELECT * FROM replan_mappings WHERE operation_id=?1
             ORDER BY ordinal, child_task_id",
        )?
        .query_map([op.id], |r| {
            Ok(task_replan::ConditionMapping {
                ordinal: usize::try_from(r.get::<_, i64>("ordinal")?).unwrap_or_default(),
                hash: r.get("hash")?,
                child_task_id: r.get("child_task_id")?,
                child_ordinal: r
                    .get::<_, Option<i64>>("child_ordinal")?
                    .and_then(|ordinal| usize::try_from(ordinal).ok()),
                disposition: r.get("disposition")?,
                authority: r
                    .get::<_, Option<String>>("authority")?
                    .and_then(|text| serde_json::from_str(&text).ok()),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    op.replaced_by = conn
        .prepare(
            "SELECT to_task_id FROM replan_replacements WHERE operation_id=?1 ORDER BY to_task_id",
        )?
        .query_map([op.id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    op.request_status = op
        .request_id
        .map(|request| {
            conn.query_row(
                "SELECT status FROM plan_requests WHERE id=?1",
                [request],
                |r| enum_col(r, "status"),
            )
            .optional()
        })
        .transpose()?
        .flatten();
    ensure!(
        op.replaced_by.is_empty() || op.status == ReplanStatus::Applied,
        "replan operation {} names replacements but is {}",
        op.id,
        op.status.as_str()
    );
    Ok(op)
}

fn operation_row(r: &Row<'_>) -> rusqlite::Result<ReplanOperation> {
    Ok(ReplanOperation {
        id: r.get("id")?,
        task_id: r.get("task_id")?,
        run_id: r.get("run_id")?,
        goal_id: r.get("goal_id")?,
        origin: enum_col(r, "origin")?,
        status: enum_col(r, "status")?,
        status_reason: r.get("status_reason")?,
        generation: r.get("generation")?,
        return_phase: r
            .get::<_, Option<String>>("return_phase")?
            .map(|phase| {
                phase.parse().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })
            })
            .transpose()?,
        reason: r.get("reason")?,
        conditions: json_col(r, "conditions")?,
        task_version: r.get("task_version")?,
        goal_acceptance_version: r.get("goal_acceptance_version")?,
        diagnosis: json_col(r, "diagnosis")?,
        snapshot_id: r.get("snapshot_id")?,
        request_id: r.get("request_id")?,
        request_status: None,
        proposal_id: r.get("proposal_id")?,
        requested_by: r.get("requested_by")?,
        requested_by_id: r.get("requested_by_id")?,
        mappings: Vec::new(),
        replaced_by: Vec::new(),
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        closed_at: r.get("closed_at")?,
    })
}

#[cfg(test)]
mod tests {
    use crate::domain::task_replan::ReplanStatus;

    #[test]
    fn the_sql_names_the_statuses_the_domain_guards_with() {
        let sql: Vec<String> = super::replan_guarding!()
            .split(',')
            .map(|status| status.trim().trim_matches('\'').to_owned())
            .collect();
        let guarding: Vec<String> = ReplanStatus::ALL
            .into_iter()
            .filter(|status| status.guards())
            .map(|status| status.as_str().to_owned())
            .collect();
        assert_eq!(sql, guarding);
    }
}
