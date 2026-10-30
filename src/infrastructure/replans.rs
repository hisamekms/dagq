//! The replanning operations (ADR-t1521-1, ADR-t1521-2):
//! `replan_operations` with their condition mappings (`replan_mappings`)
//! and replacements (`replan_replacements`), and the planning request a
//! person's request on a run records (`plan_requests.replan_operation_id`).
//! A task has at most one open operation, on its latest run unless it
//! guards the task: opening one is one write transaction that inserts it
//! only when the task has none open, so two requests on one run (two
//! supervisors, a person and a worker at once) open one; a new run of the
//! task takes over the open one of the run before it ([`carry_open_to`]);
//! and [`move_in`] refuses to diagnose, fence or reopen one beside another
//! ([`task_replan::needs_task_alone`]). The guard every ordinary step of a
//! task asks ([`check_task_step`]) reads the same rows, and the task's hold,
//! inside the step's own transaction. The condition mappings are written
//! only through [`write_mappings`], which refuses a second deletion of one
//! acceptance item, and a deletion of an item a child task holds, inside
//! the writing transaction: the table's key does not compare a deletion's
//! NULL child task.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::json;

use super::plan_requests::read_request;
use super::sqlite::{
    SqliteQueue, enum_col, event, json_col, read_task, record_queue_event_in, replan_guarding,
};
use crate::domain::DomainError;
use crate::domain::event_kind::EventKind;
use crate::domain::plan_request::{RequestRef, RequestStatus};
use crate::domain::plan_review::PlanReviewAction;
use crate::domain::replan_detect;
use crate::domain::replan_plan::{
    self, DeletionAuthority, PlanContext, PlanProblem, PlanProblemCode, PlanRevision, PlanTask,
    ReplacementPlan, ReplanBrief, ReviewFinding, SuccessorBrief,
};
use crate::domain::task_replan::{
    self, ChildConditions, GoalConditions, GuardedStep, NewReplan, OriginalConditions, ReplanGuard,
    ReplanMove, ReplanOperation, ReplanOrigin, ReplanRecorded, ReplanStatus, acceptance_items,
};
use crate::domain::{GoalId, ProposalId, ReplanId, RunId, RunStatus, TaskId, TaskStatus};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// The open operations (not closed), oldest first.
const OPEN: &str = "SELECT * FROM replan_operations WHERE closed_at IS NULL ORDER BY id";

impl SqliteQueue {
    /// Record `new` from `origin` as `by_role` with actor id `by_id`: a new
    /// `candidate` operation on its run with the task's and its goal's
    /// conditions as they are now, or the task's open one (`created` false)
    /// as [`task_replan::request_on`] says: the one on the same run, or one
    /// that guards the task, which stays on the run it fenced, whatever run
    /// the request names. A request on the task's latest run first keeps
    /// one open operation on the task ([`carry_open_to`]); while one that
    /// does not guard is open on another run, the request is refused.
    /// A person's request (`manual`) also records the
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
        let tx = self.write_tx("record_replan", TransactionBehavior::Immediate)?;
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
        if latest == new.run {
            carry_open_to(&tx, task_id, &new.run, now)?;
        }
        let open = open_operations(&tx, task_id)?;
        let (id, created) = match task_replan::request_on(task_replan::current(&open), &new.run) {
            task_replan::Request::Join(id) => (id, false),
            task_replan::Request::Refuse(id) => {
                let on = open
                    .iter()
                    .find(|op| op.id == id)
                    .map(|op| op.run_id.to_string())
                    .unwrap_or_default();
                anyhow::bail!(
                    "run {} cannot be replanned: replan operation {id} of task {task_id} is open on run {on}",
                    new.run
                );
            }
            task_replan::Request::Open => {
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
        let tx = self.write_tx("move_replan", TransactionBehavior::Immediate)?;
        let moved = move_in(&tx, id, generation, step, now)?;
        tx.commit()?;
        Ok(moved)
    }

    /// Open `ask` for the diagnosis of operation `id`, read under
    /// `generation`, and record its `replan_diagnosed` on `run` with
    /// `payload`, in one transaction: the operation is claimed again
    /// (`diagnosing` under a new generation, for `reason`) so that the ask
    /// owns it, and the event names the ask (or the open ask of the task
    /// the queue gave instead) and that generation
    /// ([`replan_detect::asked_payload`]). An operation moved on since, or
    /// a `premise` of the diagnosis job the ask carries the verdict of that
    /// no longer holds ([`check_premise`], read in the same transaction),
    /// refuses it, and then neither the ask nor the event is written.
    pub fn ask_replan_diagnosis(
        &mut self,
        ask: &crate::domain::NewAsk,
        (id, generation): (ReplanId, i64),
        reason: &str,
        run: &RunId,
        (payload, premise): (serde_json::Value, Option<&task_replan::DiagnosisPremise>),
    ) -> Result<(crate::domain::AskOutcome, ReplanOperation)> {
        let mut payload = payload;
        ask.validate()?;
        let op = read_operation(&self.conn, id)?;
        // This path opens the diagnosis's own ask only: a `decide` about the
        // operation's task, holding no run, finding or request.
        ensure!(
            ask.kind == crate::domain::AskKind::Decide
                && ask.asked_by == replan_detect::DIAGNOSIS_ASKER
                && ask.task_id == Some(op.task_id)
                && ask.run_id.is_none()
                && ask.finding_id.is_none()
                && ask.request_id.is_none(),
            "replan operation {id}: only its diagnosis's decide ask about task {} opens here",
            op.task_id
        );
        let now = self.generators.clock.now();
        let tx = self.write_tx("ask_replan_diagnosis", TransactionBehavior::Immediate)?;
        if let Some(premise) = premise {
            let read = read_operation(&tx, id)?;
            check_premise(&tx, &read, ReplanStatus::Diagnosing, premise)?;
        }
        let claimed = move_in(
            &tx,
            id,
            generation,
            &ReplanMove {
                to: ReplanStatus::Diagnosing,
                reason: reason.to_owned(),
                snapshot_id: None,
                diagnosis: None,
            },
            now,
        )?;
        let outcome = super::asks::insert_ask(&tx, ask)?;
        replan_detect::asked_payload(&mut payload, &outcome.ask, claimed.generation);
        event(
            &tx,
            claimed.task_id,
            Some(run),
            EventKind::ReplanDiagnosed,
            payload,
        )?;
        tx.commit()?;
        Ok((outcome, claimed))
    }

    /// Start a diagnosis job of operation `id`, read under `generation`, on
    /// `run` for supervisor `owner`, in one transaction: the operation is
    /// claimed (`diagnosing` under a new generation, for `reason`) and its
    /// `replan_diagnosis_started` is written with `payload`, that
    /// generation and `owner` (its pid, token and process's start), once no diagnosis job of any operation runs
    /// in the queue ([`running_diagnoses`], read in the same transaction,
    /// with the supervisors in `gone` taken as running none): a job runs
    /// until the end tied to its start is recorded, whatever its operation
    /// does meanwhile. Of two
    /// supervisors that start one at once, the second is refused here and
    /// writes nothing.
    pub fn start_replan_diagnosis(
        &mut self,
        (id, generation): (ReplanId, i64),
        reason: &str,
        run: &RunId,
        (payload, owner, gone): (
            serde_json::Value,
            &replan_detect::JobOwner,
            &[replan_detect::JobOwner],
        ),
    ) -> Result<ReplanOperation> {
        let mut payload = payload;
        let now = self.generators.clock.now();
        let tx = self.write_tx("start_replan_diagnosis", TransactionBehavior::Immediate)?;
        let running = running_diagnoses(&tx, gone)?;
        ensure!(
            running.is_empty(),
            "replan operation {id}: the diagnosis of replan operation {} runs, and one runs at a time in the queue",
            running
                .iter()
                .map(|job| job.operation.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let claimed = move_in(
            &tx,
            id,
            generation,
            &ReplanMove {
                to: ReplanStatus::Diagnosing,
                reason: reason.to_owned(),
                snapshot_id: None,
                diagnosis: None,
            },
            now,
        )?;
        payload["operation_id"] = json!(claimed.id);
        payload["generation"] = json!(claimed.generation);
        payload["supervisor_pid"] = json!(owner.pid);
        payload["supervisor_token"] = json!(owner.token);
        payload["supervisor_process_start"] = json!(owner.process_start);
        event(
            &tx,
            claimed.task_id,
            Some(run),
            EventKind::ReplanDiagnosisStarted,
            payload,
        )?;
        tx.commit()?;
        Ok(claimed)
    }

    /// Record the failure of the apply of a diagnosis job's verdict on
    /// operation `id`, read under `generation` (the job's), as its
    /// `replan_diagnosis_failed` on `run` with `payload`, in one
    /// transaction with a claim of the operation (`diagnosing` under a new
    /// generation, for `reason`). Every move and ask of that verdict is
    /// written under the same generation ([`move_in`]), so of a supervisor
    /// that applies the verdict (a verdict a hold kept, which any
    /// supervisor applies after the release) and another that records its
    /// failure, the one that commits first wins: the other is refused
    /// ([`DomainError`]) and writes nothing. A held task refuses it too,
    /// and its verdict stays kept for the release.
    pub fn fail_replan_diagnosis(
        &mut self,
        (id, generation): (ReplanId, i64),
        reason: &str,
        run: &RunId,
        payload: serde_json::Value,
    ) -> Result<ReplanOperation> {
        let now = self.generators.clock.now();
        let tx = self.write_tx("fail_replan_diagnosis", TransactionBehavior::Immediate)?;
        let claimed = move_in(
            &tx,
            id,
            generation,
            &ReplanMove {
                to: ReplanStatus::Diagnosing,
                reason: reason.to_owned(),
                snapshot_id: None,
                diagnosis: None,
            },
            now,
        )?;
        event(
            &tx,
            claimed.task_id,
            Some(run),
            EventKind::ReplanDiagnosisFailed,
            payload,
        )?;
        tx.commit()?;
        Ok(claimed)
    }

    /// The supervisors that own a diagnosis job that runs now, queue-wide
    /// ([`running_diagnoses`]), each with the heartbeat of the
    /// `supervisors` row of its token, whose liveness the next start is
    /// told of.
    pub fn running_diagnosis_owners(&self) -> Result<Vec<replan_detect::RunningOwner>> {
        let mut heartbeat = self
            .conn
            .prepare("SELECT heartbeat_at FROM supervisors WHERE token=?1")?;
        running_diagnoses(&self.conn, &[])?
            .into_iter()
            .map(|job| {
                let heartbeat_at = match &job.owner.token {
                    Some(token) => heartbeat.query_row([token], |r| r.get(0)).optional()?,
                    None => None,
                };
                Ok(replan_detect::RunningOwner {
                    owner: job.owner,
                    heartbeat_at,
                })
            })
            .collect()
    }

    /// Add `mappings` to operation `id`'s condition mappings in one write
    /// transaction ([`write_mappings`]), and the operation as it is then.
    pub fn record_replan_mappings(
        &mut self,
        id: ReplanId,
        mappings: &[task_replan::ConditionMapping],
    ) -> Result<ReplanOperation> {
        let tx = self.write_tx("record_replan_mappings", TransactionBehavior::Immediate)?;
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

    /// Every operation of `task`'s lineage, oldest first, with its
    /// outcome: the operations of the task, of the tasks it replaced (up to
    /// the first) and of every task that replaced one of them, and theirs,
    /// along `replan_replacements`. A loop in the records is followed once.
    pub fn replan_lineage(&self, task: TaskId) -> Result<Vec<ReplanOperation>> {
        lineage_in(&self.conn, task)
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

    fn resolve_replan(
        &mut self,
        resolve: &task_replan::ResolveReplan,
        snapshot_problem: Option<&str>,
        by_role: &str,
        by_id: &str,
    ) -> Result<task_replan::ReplanResolved> {
        SqliteQueue::resolve_replan(self, resolve, snapshot_problem, by_role, by_id)
    }
}

/// Move operation `id`, read under `generation`, as `step` says, on
/// `conn` inside the caller's transaction ([`SqliteQueue::move_replan`]):
/// `replan_status_changed` is written with it. A move that closes the
/// operation while no planner of the runtime's is open for its planning
/// request (none was opened, or each one ended without deciding it;
/// `continuing` before the snapshot, `continuing_after_park` and
/// `withdrawn` before it or after it, out of `parked`, `planning`,
/// `reviewing`, `awaiting_answer` or `blocked`) declines the still `open`
/// request with the outcome as the reason
/// ([`task_replan::ReplanStatus::ends_request_undecided`],
/// [`super::plan_requests::decline_undecided`]), and the event names it
/// (`request_declined`): it is declined at the close, not when the run's
/// restore is confirmed, since the run may go on without a restore. A
/// planner open at the close is left to submit or decline the request
/// itself; if it ends without deciding, its row's close declines it
/// ([`super::plan_requests::decline_after_planner`]). `blocked` is not an
/// end: one that goes on to the planner keeps its request for it. A
/// closed operation whose restore failed, back in `blocked`, that reaches
/// the planner again (straight or through the snapshot) records a new
/// request in place of the one the runtime declined at a close
/// ([`task_replan::ReplanStatus::renews_request`],
/// [`super::plan_requests::declined_at_close`],
/// [`super::plan_requests::renew_declined`]), and the event names it
/// (`request_renewed`): its planner is opened once, for the new request.
/// A request its planner submitted or declined is left as it is.
pub(crate) fn move_in(
    conn: &Connection,
    id: ReplanId,
    generation: i64,
    step: &ReplanMove,
    now: i64,
) -> Result<ReplanOperation> {
    move_in_as(conn, id, generation, step, now, false)
}

/// [`move_in`], by the trusted runtime's atomic apply when `apply`: the
/// one writer that moves an operation into `applying` and `applied`
/// ([`task_replan::check_apply_move`], [`super::replan_apply`]).
pub(super) fn move_in_as(
    conn: &Connection,
    id: ReplanId,
    generation: i64,
    step: &ReplanMove,
    now: i64,
    apply: bool,
) -> Result<ReplanOperation> {
    let op = read_operation(conn, id)?;
    if apply {
        task_replan::check_apply_move(&op, generation, step)?;
    } else {
        task_replan::check_move(&op, generation, step)?;
    }
    if let Some(premise) = step
        .diagnosis
        .as_ref()
        .and_then(|made| made.premise.as_ref())
    {
        check_premise(conn, &op, step.to, premise)?;
    } else if step.to == ReplanStatus::Diagnosing {
        check_not_held(conn, &op)?;
    }
    // An operation diagnosed, fenced or reopened (a restore that failed)
    // must be the task's one open operation.
    if task_replan::needs_task_alone(op.status, step.to) {
        let other: Option<ReplanId> = conn
            .query_row(
                "SELECT id FROM replan_operations WHERE task_id=?1 AND closed_at IS NULL AND id<>?2
                 LIMIT 1",
                params![op.task_id, id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(other) = other {
            anyhow::bail!(
                "replan operation {id} cannot move to {}: task {} has operation {other} open",
                step.to.as_str(),
                op.task_id
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
    if step.to.is_closed() && step.to != ReplanStatus::Applied {
        release_children(conn, &op, now)?;
    }
    let planner_open = op
        .request_id
        .map(|request| super::plan_requests::planner_open(conn, request))
        .transpose()?
        .unwrap_or(false);
    let declined = match op.request_id {
        Some(request) if step.to.ends_request_undecided(planner_open) => {
            super::plan_requests::decline_undecided(conn, request, id, step.to, &step.reason, now)?
                .then_some(request)
        }
        _ => None,
    };
    let renewed = match op.request_id {
        // Its `request_declined` is read only for a move onto the planner.
        Some(request)
            if op.status.renews_request(step.to, true)
                && super::plan_requests::declined_at_close(conn, request, id)? =>
        {
            let renewed = super::plan_requests::renew_declined(conn, request, id, now)?;
            conn.execute(
                "UPDATE replan_operations SET request_id=?1 WHERE id=?2",
                params![renewed, id],
            )?;
            Some(renewed)
        }
        _ => None,
    };
    // A diagnosis that fences an operation no person asked for (the
    // runtime's detection, a worker's proposal) records the planning
    // request its planner is opened for once the snapshot is saved, in
    // the same transaction: the request of a person's is recorded with it.
    let recorded = match (op.request_id, &step.diagnosis) {
        (None, Some(made)) if step.to == ReplanStatus::PauseRequested => {
            let new = NewReplan {
                run: op.run_id.clone(),
                reason: format!(
                    "{}; diagnosed: {}",
                    op.reason,
                    made.summary.as_deref().unwrap_or(&step.reason)
                ),
                evidence: diagnosis.evidence.clone(),
            };
            let request = record_request(conn, &op, &new, "supervisor", &op.requested_by_id, now)?;
            conn.execute(
                "UPDATE replan_operations SET request_id=?1 WHERE id=?2",
                params![request, id],
            )?;
            Some(request)
        }
        _ => None,
    };
    let mut payload = json!({"operation_id": id, "from": op.status, "to": step.to,
                             "reason": step.reason, "generation": next,
                             "snapshot_id": step.snapshot_id});
    if let Some(request) = recorded {
        payload["request_recorded"] = json!(request);
    }
    if let Some(request) = declined {
        payload["request_declined"] = json!(request);
    }
    if let Some(request) = renewed {
        payload["request_renewed"] = json!(request);
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

/// [`SqliteQueue::replan_lineage`] on `conn`, inside the caller's
/// transaction when it holds one.
pub(crate) fn lineage_in(conn: &Connection, task: TaskId) -> Result<Vec<ReplanOperation>> {
    let mut root = task;
    let mut seen = std::collections::BTreeSet::from([task]);
    while let Some(from) = conn
        .query_row(
            "SELECT from_task_id FROM replan_replacements WHERE to_task_id=?1 LIMIT 1",
            [root],
            |r| r.get::<_, TaskId>(0),
        )
        .optional()?
    {
        if !seen.insert(from) {
            break;
        }
        root = from;
    }
    let mut tasks = vec![root];
    let mut at = 0;
    while let Some(&from) = tasks.get(at) {
        at += 1;
        let mut children =
            conn.prepare("SELECT to_task_id FROM replan_replacements WHERE from_task_id=?1")?;
        for child in children.query_map([from], |r| r.get::<_, TaskId>(0))? {
            let child = child?;
            if !tasks.contains(&child) {
                tasks.push(child);
            }
        }
    }
    let mut ops = Vec::new();
    for task in tasks {
        let of_task: Vec<ReplanOperation> = conn
            .prepare("SELECT * FROM replan_operations WHERE task_id=?1 ORDER BY id")?
            .query_map([task], operation_row)?
            .collect::<rusqlite::Result<_>>()?;
        ops.extend(of_task);
    }
    ops.sort_by_key(|op| op.id);
    ops.into_iter().map(|op| with_outcome(conn, op)).collect()
}

/// Refuse the claim of `op` for a diagnosis job or its ask, and the move
/// or the ask of a verdict or a person's answer, while `op`'s task is held
/// (ADR-t1879-1 decision 6), read in the write's own transaction: a hold
/// that commits first wins, and the supervisor keeps the verdict or the
/// answer until the release.
fn check_not_held(conn: &Connection, op: &ReplanOperation) -> Result<()> {
    if let Some(hold) = super::task_holds::guard_of(conn, op.task_id)? {
        return Err(DomainError::ReplanDiagnosisHeld {
            operation: op.id,
            task_id: op.task_id,
            hold: hold.hold,
        }
        .into());
    }
    Ok(())
}

/// The diagnosis jobs that run now, queue-wide, on `conn` (inside the
/// caller's transaction when it has one), read from the records of every
/// operation's jobs ([`replan_detect::running_jobs`]), the supervisors in
/// `gone` taken as running none.
fn running_diagnoses(
    conn: &Connection,
    gone: &[replan_detect::JobOwner],
) -> Result<Vec<replan_detect::RunningJob>> {
    let records: Vec<(String, String)> = conn
        .prepare("SELECT kind, payload FROM run_events WHERE kind IN (?1, ?2, ?3) ORDER BY id")?
        .query_map(
            params![
                EventKind::ReplanDiagnosisStarted.as_str(),
                EventKind::ReplanDiagnosed.as_str(),
                EventKind::ReplanDiagnosisFailed.as_str(),
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?
        .collect::<rusqlite::Result<_>>()?;
    let mut read = Vec::new();
    for (kind, payload) in records {
        let Some(kind) = EventKind::from_name(&kind) else {
            continue;
        };
        let payload: serde_json::Value = serde_json::from_str(&payload)?;
        read.extend(replan_detect::JobRecord::of(kind, &payload));
    }
    let records = read;
    Ok(replan_detect::running_jobs(&records, gone))
}

/// Refuse the move, or the ask, of a diagnosis whose premise
/// ([`task_replan::DiagnosisPremise`]) no longer holds, read in the write's
/// own transaction: a hold open on the task ([`check_not_held`]), a landing
/// that began, a review, a turn or a receipt that came, an edit of the task
/// or another run since the diagnosis job started, a person's request that
/// joined an operation the decision read as no person's, or another park
/// of the lineage that used up its limit wins over the decision.
fn check_premise(
    conn: &Connection,
    op: &ReplanOperation,
    to: ReplanStatus,
    premise: &task_replan::DiagnosisPremise,
) -> Result<()> {
    check_not_held(conn, op)?;
    let stale = |why: String| {
        anyhow::Error::new(DomainError::ReplanPremiseStale {
            operation: op.id,
            why,
        })
    };
    let task = read_task(conn, op.task_id)?;
    if task.status() != TaskStatus::InProgress {
        return Err(stale(format!(
            "task {} is {}",
            op.task_id,
            task.status().as_str()
        )));
    }
    let latest: Option<(RunId, String)> = conn
        .query_row(
            "SELECT id, status FROM task_runs WHERE task_id=?1 ORDER BY rowid DESC LIMIT 1",
            [op.task_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let landing = [
        RunStatus::Integrating,
        RunStatus::Integrated,
        RunStatus::Succeeded,
    ];
    match latest {
        Some((run, status))
            if run == op.run_id
                && status == premise.run_status.as_str()
                && !landing.iter().any(|landing| landing.as_str() == status) => {}
        Some((run, status)) => {
            return Err(stale(format!(
                "run {run} is {status}, and the diagnosis read run {} {}",
                op.run_id,
                premise.run_status.as_str()
            )));
        }
        None => return Err(stale(format!("task {} has no run", op.task_id))),
    }
    if let Some(window_end) = premise.read_until {
        let kinds = replan_detect::PROGRESS.map(EventKind::as_str);
        let progress: Option<(i64, String)> = conn
            .query_row(
                "SELECT id, kind FROM run_events WHERE task_id=?1 AND id>?2
                 AND kind IN (?3, ?4, ?5, ?6, ?7, ?8) ORDER BY id LIMIT 1",
                params![
                    op.task_id, window_end, kinds[0], kinds[1], kinds[2], kinds[3], kinds[4],
                    kinds[5]
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((id, kind)) = progress {
            return Err(stale(format!(
                "{kind} event {id} came after the last event it read ({window_end})"
            )));
        }
    }
    if premise.manual == Some(false) {
        let joined: Option<i64> = conn
            .query_row(
                "SELECT id FROM run_events WHERE task_id=?1 AND kind=?2
                 AND json_extract(payload,'$.operation_id')=?3
                 AND json_extract(payload,'$.origin')=?4 LIMIT 1",
                params![
                    op.task_id,
                    EventKind::ReplanRequested.as_str(),
                    op.id,
                    ReplanOrigin::Manual.as_str()
                ],
                |r| r.get(0),
            )
            .optional()?;
        if op.origin == ReplanOrigin::Manual || joined.is_some() {
            return Err(stale(
                "a person's request joined the operation since it was decided".to_owned(),
            ));
        }
    }
    if let Some(limit) = premise.max_auto_parks
        && to == ReplanStatus::PauseRequested
    {
        let parks = replan_detect::auto_parks(&lineage_in(conn, op.task_id)?);
        if parks >= limit {
            return Err(stale(format!(
                "the lineage's parks without a person are used up ({parks} of {limit})"
            )));
        }
    }
    let mut now = conditions_of(conn, op.task_id)?;
    now.successors.clone_from(&op.conditions.successors);
    if now.version() != op.task_version {
        return Err(stale(format!("task {} was edited", op.task_id)));
    }
    Ok(())
}

/// Add `mappings` to operation `id`'s condition mappings on `conn` inside
/// the caller's transaction (the apply writes them with its replacement).
/// The operation's mappings are read in the same transaction, and
/// [`task_replan::check_mappings`] refuses a second deletion of one item,
/// and a deletion of an item a child task holds (or the reverse), before
/// anything is written.
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

/// The open operations on `task`, oldest first.
fn open_operations(conn: &Connection, task: TaskId) -> Result<Vec<ReplanOperation>> {
    Ok(conn
        .prepare(
            "SELECT * FROM replan_operations WHERE task_id=?1 AND closed_at IS NULL ORDER BY id",
        )?
        .query_map([task], operation_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// Move operation `id` (closed in a continuation or a withdrawal), read
/// under `generation`, back to `blocked` with `why` when its restore or
/// its new session failed (ADR-t1521-2 decision 2), on `conn` inside the
/// caller's transaction, keeping its task to one open operation: one
/// opened on the task since that does not guard it (`candidate`,
/// `diagnosing`, which leaves the run going and holds no work) is
/// superseded by `id` as `continuing`, as [`carry_open_to`] supersedes
/// one, so that a request joins `id` from then on. One that guards the
/// task parks the run itself and keeps it: nothing is moved, and `false`
/// (the caller writes nothing either).
pub(crate) fn reopen_blocked(
    conn: &Connection,
    id: ReplanId,
    generation: i64,
    why: &str,
    now: i64,
) -> Result<bool> {
    let op = read_operation(conn, id)?;
    let open = open_operations(conn, op.task_id)?;
    if open.iter().any(|other| other.status.guards()) {
        return Ok(false);
    }
    for other in open.iter().filter(|other| other.id != id) {
        let step = ReplanMove {
            to: ReplanStatus::Continuing,
            reason: format!(
                "superseded by replan operation {id} of the task, held blocked again: {why}"
            ),
            snapshot_id: None,
            diagnosis: None,
        };
        move_in(conn, other.id, other.generation, &step, now)?;
    }
    let step = ReplanMove {
        to: ReplanStatus::Blocked,
        reason: why.to_owned(),
        snapshot_id: None,
        diagnosis: None,
    };
    move_in(conn, id, generation, &step, now)?;
    Ok(true)
}

/// The task's open operation, if any ([`task_replan::current`]): what
/// `list` marks it with. A task has at most one open operation that does
/// not guard it, on its latest run ([`carry_open_to`]); one that guards
/// stays on the run it parked, and is the one read.
pub(super) fn open_operation_of(
    conn: &Connection,
    task: TaskId,
) -> Result<Option<ReplanOperation>> {
    Ok(task_replan::current(&open_operations(conn, task)?).cloned())
}

/// Keep one open operation on `task` now that `run` is its latest run, on
/// `conn` inside the caller's transaction (the claim of `run`, or a request
/// on it), as [`task_replan::carry`] decides and why: the others that do
/// not guard close as `continuing` (`replan_status_changed`), and the kept
/// one, unless it guards, moves to `run` under a raised generation with
/// `replan_carried`, its planning request naming `run` beside the earlier
/// run.
pub(super) fn carry_open_to(conn: &Connection, task: TaskId, run: &RunId, now: i64) -> Result<()> {
    let open = open_operations(conn, task)?;
    let carry = task_replan::carry(&open, run);
    let Some(kept) = carry.kept else {
        return Ok(());
    };
    for older in open.iter().filter(|op| carry.superseded.contains(&op.id)) {
        let step = ReplanMove {
            to: ReplanStatus::Continuing,
            reason: format!("superseded by replan operation {kept} of the task"),
            snapshot_id: None,
            diagnosis: None,
        };
        move_in(conn, older.id, older.generation, &step, now)?;
    }
    let Some((id, generation)) = carry.moved else {
        return Ok(());
    };
    let Some(op) = open.iter().find(|op| op.id == id) else {
        return Ok(());
    };
    conn.execute(
        "UPDATE replan_operations SET run_id=?1, generation=?2, updated_at=?3 WHERE id=?4",
        params![run, generation, now, id],
    )?;
    if let Some(request) = op.request_id {
        let mut refs = read_request(conn, request)?.refs;
        let to = RequestRef::Run(run.clone());
        if !refs.contains(&to) {
            refs.push(to);
            conn.execute(
                "UPDATE plan_requests SET refs=?1, updated_at=?2 WHERE id=?3",
                params![serde_json::to_string(&refs)?, now, request],
            )?;
        }
    }
    event(
        conn,
        task,
        Some(run),
        EventKind::ReplanCarried,
        json!({"operation_id": id, "from_run": op.run_id, "to_run": run,
               "generation": generation}),
    )?;
    Ok(())
}

/// The task's and its goal's conditions as they are on `conn` now.
pub(super) fn conditions_of(conn: &Connection, task_id: TaskId) -> Result<OriginalConditions> {
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

/// The open operation whose planning request `proposal` is linked to: the
/// operation the proposal would replace the task of, read on `conn`.
pub(super) fn replacing_operation(
    conn: &Connection,
    proposal: ProposalId,
) -> Result<Option<ReplanOperation>> {
    let id: Option<ReplanId> = conn
        .query_row(
            "SELECT o.id FROM plan_request_proposals l
             JOIN plan_requests r ON r.id=l.request_id
             JOIN replan_operations o ON o.id=r.replan_operation_id
             WHERE l.proposal_id=?1 AND o.closed_at IS NULL ORDER BY o.id LIMIT 1",
            [proposal],
            |r| r.get(0),
        )
        .optional()?;
    id.map(|id| read_operation(conn, id)).transpose()
}

/// The version of child `task`'s conditions ([`ChildConditions`]) that
/// plan review approves and the apply compares, read on `conn`.
pub(super) fn child_version(conn: &Connection, task: TaskId) -> Result<String> {
    let goals = conn
        .prepare("SELECT goal_id FROM task_goal_dependencies WHERE task_id=?1 ORDER BY goal_id")?
        .query_map([task], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ChildConditions::of(&read_task(conn, task)?, conditions_of(conn, task)?, goals).version())
}

/// The version of each child's conditions plan review approved with plan
/// `revision` of operation `id` (`replan_plans.approved_children`), `None`
/// when none was recorded.
pub(super) fn approved_children(
    conn: &Connection,
    id: ReplanId,
    revision: i64,
) -> Result<Option<BTreeMap<TaskId, String>>> {
    let text: Option<String> = conn
        .query_row(
            "SELECT approved_children FROM replan_plans WHERE operation_id=?1 AND revision=?2",
            params![id, revision],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    text.map(|text| serde_json::from_str(&text).map_err(Into::into))
        .transpose()
}

/// The latest plan operation `id` was submitted with, if any.
pub(super) fn latest_plan(conn: &Connection, id: ReplanId) -> Result<Option<PlanRevision>> {
    let row: Option<(i64, ProposalId, String, String, Option<i64>)> = conn
        .query_row(
            "SELECT revision, proposal_id, plan, hash, approved_at FROM replan_plans
             WHERE operation_id=?1 ORDER BY revision DESC LIMIT 1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    row.map(|(revision, proposal_id, plan, hash, approved_at)| {
        Ok(PlanRevision {
            revision,
            proposal_id,
            plan: ReplacementPlan::parse(&plan)?,
            hash,
            approved_at,
        })
    })
    .transpose()
}

/// The problems of `plan` for `op`, whose proposal holds `members`, as the
/// queue is on `conn` now once plan review's pass applies `actions` (none
/// outside a pass) ([`replan_plan::check_plan`],
/// [`replan_plan::PlanContext::after_actions`]).
pub(super) fn check_replacement(
    conn: &Connection,
    op: &ReplanOperation,
    plan: &ReplacementPlan,
    members: &[TaskId],
    actions: &[PlanReviewAction],
) -> Result<Vec<PlanProblem>> {
    let mut named: BTreeSet<TaskId> = plan.child_ids().into_iter().collect();
    named.extend(members.iter().copied());
    named.extend(plan.successors.iter().map(|successor| successor.task_id));
    named.extend(op.conditions.successors.iter().copied());
    named.extend(op.conditions.predecessors.iter().copied());
    named.extend(
        plan.mappings
            .iter()
            .filter_map(|mapping| mapping.landed_task_id),
    );
    let mut tasks = BTreeMap::new();
    for id in named {
        if let Some(task) = plan_task(conn, id)? {
            tasks.insert(id, task);
        }
    }
    let mut downstream = BTreeSet::new();
    let mut stack = vec![op.task_id];
    while let Some(next) = stack.pop() {
        let waiting: Vec<TaskId> = conn
            .prepare("SELECT task_id FROM task_dependencies WHERE predecessor_id=?1")?
            .query_map([next], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for task in waiting {
            if downstream.insert(task) {
                stack.push(task);
            }
        }
    }
    let mut authorities = BTreeMap::new();
    for ask in plan
        .mappings
        .iter()
        .filter_map(|mapping| mapping.authority_ask_id)
    {
        let Ok(read) = super::asks::read_ask(conn, ask) else {
            continue;
        };
        // Asked since the operation began: an older answer decided
        // something else.
        let about_operation = read.created_at >= op.created_at
            && ((read.request_id.is_some() && read.request_id == op.request_id)
                || read.run_id.as_ref() == Some(&op.run_id)
                || read.task_id == Some(op.task_id));
        let answered_by_person = read.answer.is_some()
            && read.answered_by.as_deref().is_some_and(|by| {
                matches!(by, crate::domain::ANSWERED_BY_PERSON | "user" | "inbox")
            });
        authorities.insert(
            ask,
            DeletionAuthority {
                about_operation,
                scope: read.reason_category == crate::domain::AskReason::Scope,
                answered_by_person,
                answer: read.answer.clone().unwrap_or_default(),
                options: read.options.clone(),
            },
        );
    }
    let goal_acceptance_version_now = op
        .goal_id
        .map(|goal| {
            conn.query_row(
                "SELECT acceptance_version FROM goals WHERE id=?1",
                [goal],
                |r| r.get(0),
            )
        })
        .transpose()?;
    // The tasks that wait for the task are read live: one that came to
    // wait since the operation fixed its conditions makes no plan stale,
    // and the apply has it wait for every child.
    let mut now = conditions_of(conn, op.task_id)?;
    now.successors.clone_from(&op.conditions.successors);
    let ctx = PlanContext {
        operation: op,
        task_version_now: now.version(),
        goal_acceptance_version_now,
        members: members.to_vec(),
        tasks,
        downstream,
        authorities,
    }
    .after_actions(actions);
    let mut problems = replan_plan::check_plan(plan, &ctx);
    // A loop through goals as well as tasks (ADR-0038), on the graph the
    // apply would leave: the successors and the task wait for the
    // children.
    let successors: Vec<TaskId> = super::replan_apply::successors_now(conn, op.task_id)?
        .into_keys()
        .collect();
    let mut edges = crate::domain::replan_apply::successor_edges(plan, &successors);
    edges.push((op.task_id, plan.child_ids()));
    if let Some(task) = crate::domain::replan_apply::closes_loop(
        &super::replan_apply::wait_graph(conn, op.task_id)?,
        &edges,
    ) {
        problems.push(PlanProblem {
            code: PlanProblemCode::DependencyCycle,
            task_id: Some(task),
            reason: format!(
                "task {task} would wait for itself, through the tasks and goals it waits for, once the successors of task {} wait for the children",
                op.task_id
            ),
        });
    }
    Ok(problems)
}

/// Task `id` as [`replan_plan::check_plan`] reads it; `None` for a task
/// the queue does not have.
fn plan_task(conn: &Connection, id: TaskId) -> Result<Option<PlanTask>> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1)",
        [id],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    let task = read_task(conn, id)?;
    let depends_on = conn
        .prepare("SELECT predecessor_id FROM task_dependencies WHERE task_id=?1 ORDER BY 1")?
        .query_map([id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let evidence = match serde_json::to_value(task.required_evidence())? {
        serde_json::Value::Array(items) => items,
        _ => Vec::new(),
    };
    Ok(Some(PlanTask {
        status: task.status(),
        goal_id: task.goal_id(),
        acceptance: acceptance_items(task.acceptance()),
        verify: task.verification_commands().to_vec(),
        paths: task.paths().to_vec(),
        evidence,
        depends_on,
    }))
}

/// Record, inside the submit's transaction, the plan `plan` of `proposal`
/// (holding `members`) when the proposal is linked to the planning request
/// of an open operation: the submit is refused without a plan, with a plan
/// that is not one of such a proposal, or with one [`check_replacement`]
/// finds problems in. A plan that holds is kept as the operation's next
/// revision, names the proposal on the operation, and moves the operation
/// on to `reviewing` (through `planning` from `parked`), with
/// `replan_plan_submitted`. The children stay `submitted` until the
/// trusted runtime applies the replacement: plan review's pass does not
/// make them ready ([`approve_replacement`]).
pub(super) fn record_replacement(
    conn: &Connection,
    proposal: ProposalId,
    members: &[TaskId],
    plan: Option<&ReplacementPlan>,
    now: i64,
) -> Result<()> {
    let Some(mut op) = replacing_operation(conn, proposal)? else {
        ensure!(
            plan.is_none(),
            "a replacement plan goes only with a proposal of a planner of a replanning operation's request; proposal {proposal} is not one"
        );
        return Ok(());
    };
    let plan = plan.with_context(|| {
        format!(
            "proposal {proposal} replaces task {} of replan operation {}: submit it with --replacement <plan.json> (the request's prompt says what the plan holds), or decline the request with --continue-run to continue the run",
            op.task_id, op.id
        )
    })?;
    let problems = check_replacement(conn, &op, plan, members, &[])?;
    ensure!(
        problems.is_empty(),
        "the replacement plan of proposal {proposal} for replan operation {} is refused:\n{}",
        op.id,
        replan_plan::describe(&problems).join("\n")
    );
    let text = serde_json::to_string(plan)?;
    let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
    let revision: i64 = conn.query_row(
        "SELECT COALESCE(MAX(revision), 0) + 1 FROM replan_plans WHERE operation_id=?1",
        [op.id],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO replan_plans(operation_id, revision, proposal_id, plan, hash, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![op.id, revision, proposal, text, hash, now],
    )?;
    conn.execute(
        "UPDATE replan_operations SET proposal_id=?1, updated_at=?2 WHERE id=?3",
        params![proposal, now, op.id],
    )?;
    let reason = format!("proposal {proposal} was submitted with plan revision {revision}");
    if op.status == ReplanStatus::Parked {
        op = move_in(conn, op.id, op.generation, &planning_step(&reason), now)?;
    }
    if op.status != ReplanStatus::Reviewing {
        move_in(
            conn,
            op.id,
            op.generation,
            &ReplanMove {
                to: ReplanStatus::Reviewing,
                reason,
                snapshot_id: None,
                diagnosis: None,
            },
            now,
        )?;
    }
    event(
        conn,
        op.task_id,
        Some(&op.run_id),
        EventKind::ReplanPlanSubmitted,
        json!({"operation_id": op.id, "proposal_id": proposal, "revision": revision,
               "hash": hash, "choice": plan.choice, "children": plan.child_ids()}),
    )?;
    Ok(())
}

/// The move of an operation to `planning` for `reason`.
fn planning_step(reason: &str) -> ReplanMove {
    ReplanMove {
        to: ReplanStatus::Planning,
        reason: reason.to_owned(),
        snapshot_id: None,
        diagnosis: None,
    }
}

/// Move the operation of `request` from `parked` to `planning` as planner
/// `planner` opens for it, inside the opening's transaction; nothing for a
/// request of no operation or one past `parked`.
pub(super) fn planner_opened(
    conn: &Connection,
    request: crate::domain::RequestId,
    planner: crate::domain::PlannerId,
    now: i64,
) -> Result<()> {
    let op: Option<ReplanId> = conn
        .query_row(
            "SELECT replan_operation_id FROM plan_requests WHERE id=?1",
            [request],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let Some(op) = op.map(|id| read_operation(conn, id)).transpose()? else {
        return Ok(());
    };
    if op.status == ReplanStatus::Parked {
        let reason = format!("planner {planner} opened for request {request}");
        move_in(conn, op.id, op.generation, &planning_step(&reason), now)?;
    }
    Ok(())
}

/// Accept `proposal`, which replaces the task of the open operation `op`,
/// inside the caller's transaction: plan review's pass, or a person's
/// ready answer to its concern, found it applicable. Its latest plan is
/// checked again ([`check_replacement`]) and refused with its problems;
/// one that holds is marked approved, the proposal accepted, and the
/// children stay `submitted`, where no claim takes them, until the trusted
/// runtime applies the replacement (ADR-t1521-2 decision 4). An operation
/// awaiting a person's answer goes back to `reviewing`, with
/// `replan_plan_approved`.
pub(super) fn approve_replacement(
    conn: &Connection,
    op: &ReplanOperation,
    proposal: ProposalId,
    accepted: &crate::domain::Proposal,
    now: i64,
) -> Result<()> {
    let latest = latest_plan(conn, op.id)?
        .filter(|plan| plan.proposal_id == proposal)
        .with_context(|| {
            format!(
                "proposal {proposal} replaces task {} of replan operation {} without a plan",
                op.task_id, op.id
            )
        })?;
    let problems = check_replacement(conn, op, &latest.plan, accepted.task_ids(), &[])?;
    ensure!(
        problems.is_empty(),
        "proposal {proposal} cannot be accepted: its replacement plan for replan operation {} no longer holds:\n{}",
        op.id,
        replan_plan::describe(&problems).join("\n")
    );
    // What plan review approved of each child, which the apply compares
    // with the child as it is then.
    let mut approved = BTreeMap::new();
    for child in latest.plan.child_ids() {
        approved.insert(child, child_version(conn, child)?);
    }
    conn.execute(
        "UPDATE replan_plans SET approved_at=?1, approved_children=?4
         WHERE operation_id=?2 AND revision=?3",
        params![
            now,
            op.id,
            latest.revision,
            serde_json::to_string(&approved)?
        ],
    )?;
    if op.status == ReplanStatus::AwaitingAnswer {
        move_in(
            conn,
            op.id,
            op.generation,
            &ReplanMove {
                to: ReplanStatus::Reviewing,
                reason: format!("proposal {proposal} was accepted"),
                snapshot_id: None,
                diagnosis: None,
            },
            now,
        )?;
    }
    event(
        conn,
        op.task_id,
        Some(&op.run_id),
        EventKind::ReplanPlanApproved,
        json!({"operation_id": op.id, "proposal_id": proposal, "revision": latest.revision,
               "hash": latest.hash, "children": latest.plan.child_ids()}),
    )?;
    Ok(())
}

/// The problems plan review's pass of `proposal` would accept, inside its
/// transaction: those of the latest plan of the operation whose task the
/// proposal replaces, now and once the pass applies `actions` (none for a
/// person's `ready` and for `lint`); none for any other proposal, and a
/// missing plan is one.
pub(super) fn replacement_problems(
    conn: &Connection,
    proposal: ProposalId,
    actions: &[PlanReviewAction],
) -> Result<Vec<String>> {
    let Some(op) = replacing_operation(conn, proposal)? else {
        return Ok(Vec::new());
    };
    let members = super::proposals::read(conn, proposal)?.task_ids().to_vec();
    match latest_plan(conn, op.id)?.filter(|plan| plan.proposal_id == proposal) {
        Some(latest) => Ok(replan_plan::describe(&check_replacement(
            conn,
            &op,
            &latest.plan,
            &members,
            actions,
        )?)),
        None => Ok(vec![format!(
            "proposal {proposal} replaces task {} of replan operation {} without a replacement plan",
            op.task_id, op.id
        )]),
    }
}

/// Move the operation whose task `proposal` replaces as plan review holds
/// it for a person (its concern) or its planner withdraws it, inside that
/// transaction: to `awaiting_answer`, or back to `planning`. Nothing for
/// any other proposal.
pub(super) fn proposal_moved(
    conn: &Connection,
    proposal: ProposalId,
    to: ReplanStatus,
    reason: &str,
    now: i64,
) -> Result<()> {
    let Some(op) = replacing_operation(conn, proposal)? else {
        return Ok(());
    };
    if op.proposal_id == Some(proposal) && op.status != to && op.status.can_move_to(to) {
        move_in(
            conn,
            op.id,
            op.generation,
            &ReplanMove {
                to,
                reason: reason.to_owned(),
                snapshot_id: None,
                diagnosis: None,
            },
            now,
        )?;
    }
    Ok(())
}

/// Refuse to make task `task` ready while it is a child of the replacement
/// proposal of an open operation: only the trusted runtime's apply does.
pub(super) fn check_child_ready(conn: &Connection, task: TaskId) -> Result<()> {
    let proposal: Option<ProposalId> = conn
        .query_row("SELECT proposal_id FROM tasks WHERE id=?1", [task], |r| {
            r.get(0)
        })
        .optional()?
        .flatten();
    // Any live proposal linked to an open operation's request, not only
    // the one the operation names: an older one sent back stays its.
    if let Some(proposal) = proposal
        && let Some(op) = replacing_operation(conn, proposal)?
        && super::proposals::read(conn, proposal)?.status()
            != crate::domain::ProposalStatus::Canceled
    {
        anyhow::bail!(
            "task {task} is a child of proposal {proposal}, which replaces task {} of replan operation {} ({}): it becomes ready only when the trusted runtime applies the replacement",
            op.task_id,
            op.id,
            op.status.as_str()
        );
    }
    Ok(())
}

/// Send operation `op`, held `blocked` with its snapshot (from its apply
/// because its approved plan no longer holds, or at a person's word), back
/// to a planner for `reason` inside the caller's transaction (ADR-t1521-2
/// decision 2): the children of the plan's proposal go back to draft
/// ([`release_children`]), the operation to `parked` with its snapshot,
/// and its ended request is renewed
/// ([`super::plan_requests::renew_declined`]) so that one planner opens for
/// it with the material and the plan's problems. That planner makes a new
/// plan, continues the run or withdraws; the saved work stays.
pub(super) fn replan_again(
    conn: &Connection,
    op: &ReplanOperation,
    reason: String,
    now: i64,
) -> Result<ReplanOperation> {
    release_children(conn, op, now)?;
    let moved = move_in(
        conn,
        op.id,
        op.generation,
        &ReplanMove {
            to: ReplanStatus::Parked,
            reason,
            snapshot_id: None,
            diagnosis: None,
        },
        now,
    )?;
    if let Some(request) = moved.request_id
        && moved.request_id == op.request_id
    {
        let open: bool = conn.query_row(
            "SELECT status='open' FROM plan_requests WHERE id=?1",
            [request],
            |r| r.get(0),
        )?;
        if !open {
            let renewed = super::plan_requests::renew_declined(conn, request, op.id, now)?;
            conn.execute(
                "UPDATE replan_operations SET request_id=?1 WHERE id=?2",
                params![renewed, op.id],
            )?;
        }
    }
    read_operation(conn, op.id)
}

/// Release the children of operation `op`'s replacement proposal, which
/// was never applied, as the operation closes (a continuation or a
/// withdrawal), inside the move's transaction: an active proposal is
/// withdrawn and an accepted one's children go back from `submitted` to
/// `draft`, so none is claimed, with `replan_children_released`.
fn release_children(conn: &Connection, op: &ReplanOperation, now: i64) -> Result<()> {
    let Some(proposal) = op.proposal_id else {
        return Ok(());
    };
    let stamp = crate::application::timestamp(
        std::time::UNIX_EPOCH
            + std::time::Duration::from_secs(u64::try_from(now).unwrap_or_default()),
    );
    let current = super::proposals::read(conn, proposal)?;
    if current.status().is_active() {
        super::proposals::withdraw(conn, proposal, &stamp, now)?;
    }
    let mut released = Vec::new();
    for &task in current.task_ids() {
        if read_task(conn, task)?.status() == TaskStatus::Submitted {
            super::sqlite::transition_task(conn, task, crate::domain::TaskAction::Draft, &stamp)?;
        }
        if read_task(conn, task)?.status() == TaskStatus::Draft {
            released.push(task);
        }
    }
    event(
        conn,
        op.task_id,
        Some(&op.run_id),
        EventKind::ReplanChildrenReleased,
        json!({"operation_id": op.id, "proposal_id": proposal, "children": released}),
    )?;
    Ok(())
}

impl SqliteQueue {
    /// Resolve the operation `resolve` names, held `blocked` by anything
    /// but its apply, at a person's word (`by_role`, `by_id`: the user or
    /// the inbox), in one write transaction ([`task_replan::resolve_to`]
    /// reads the operation, its generation and the children of its
    /// replacement proposal there, and `snapshot_problem` is why the caller
    /// found its snapshot not as saved): one refused writes nothing, so a
    /// second same resolution, under the generation the first raised past,
    /// changes nothing. The operation takes a new generation, so that a
    /// write decided under the one the person read (a restore's late
    /// result) is refused, and then moves: to `continuing_after_park` or
    /// `withdrawn`, which release the proposal's children to draft and
    /// whose restore and resume are the continuation's ([`move_in`]); to
    /// `parked` with its snapshot, on a new request when its request ended
    /// ([`replan_again`]); or back to the save. `replan_resolved` records
    /// the choice, who made it and why, beside the move's
    /// `replan_status_changed`. The task's conditions and dependencies are
    /// not touched.
    pub fn resolve_replan(
        &mut self,
        resolve: &task_replan::ResolveReplan,
        snapshot_problem: Option<&str>,
        by_role: &str,
        by_id: &str,
    ) -> Result<task_replan::ReplanResolved> {
        resolve.validate()?;
        let now = self.generators.clock.now();
        let tx = self.write_tx("resolve_replan", TransactionBehavior::Immediate)?;
        let op = read_operation(&tx, resolve.operation)?;
        let mut children = Vec::new();
        if let Some(proposal) = op.proposal_id {
            for &task in super::proposals::read(&tx, proposal)?.task_ids() {
                children.push((task, read_task(&tx, task)?.status()));
            }
        }
        let to = task_replan::resolve_to(&task_replan::ResolveState {
            operation: &op,
            generation: resolve.generation,
            resolution: resolve.resolution,
            children: &children,
            snapshot_problem,
        })?;
        let generation = op.generation + 1;
        tx.execute(
            "UPDATE replan_operations SET generation=?1, updated_at=?2 WHERE id=?3 AND generation=?4",
            params![generation, now, op.id, op.generation],
        )?;
        let raised = read_operation(&tx, op.id)?;
        let reason = format!(
            "{by_role} chose to {}: {}",
            match resolve.resolution {
                task_replan::ReplanResolution::Continue => "continue the run on its saved work",
                task_replan::ReplanResolution::Withdraw =>
                    "withdraw the plan and continue the run on its saved work",
                task_replan::ReplanResolution::Replan => "plan it again",
            },
            resolve.reason
        );
        event(
            &tx,
            op.task_id,
            Some(&op.run_id),
            EventKind::ReplanResolved,
            json!({"operation_id": op.id, "resolution": resolve.resolution,
                   "blocked_at": op.return_phase, "to": to, "generation": generation,
                   "reason": resolve.reason, "by": by_role, "by_id": by_id}),
        )?;
        let moved = if to == ReplanStatus::Parked {
            replan_again(&tx, &raised, reason, now)?
        } else {
            move_in(
                &tx,
                op.id,
                generation,
                &ReplanMove {
                    to,
                    reason,
                    snapshot_id: None,
                    diagnosis: None,
                },
                now,
            )?
        };
        tx.commit()?;
        Ok(task_replan::ReplanResolved {
            operation: moved,
            resolution: resolve.resolution,
            blocked_at: op.return_phase,
        })
    }

    /// What the planner of operation `id`'s request reads of it
    /// ([`ReplanBrief`]).
    pub fn replan_brief(&self, id: ReplanId) -> Result<ReplanBrief> {
        let op = read_operation(&self.conn, id)?;
        let mut turns: BTreeMap<String, usize> = BTreeMap::new();
        let mut reviews = Vec::new();
        let mut statement = self.conn.prepare(
            "SELECT kind, payload FROM run_events WHERE run_id=?1
             AND kind IN ('turn_started','review_finished') ORDER BY id",
        )?;
        let rows = statement.query_map([&op.run_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (kind, payload) = row?;
            let payload: serde_json::Value = serde_json::from_str(&payload).unwrap_or_default();
            if kind == "turn_started" {
                let what = payload["what"].as_str().unwrap_or_default();
                *turns.entry(replan_plan::turn_cause(what)).or_default() += 1;
            } else if let Some(sent) = crate::domain::run::review_sent_back(&payload) {
                // Where the route sent the run, not the review's own
                // verdict alone: its agents may send back a review that
                // passed.
                let verdict = sent
                    .verdicts
                    .iter()
                    .map(|verdict| verdict.as_str())
                    .collect::<Vec<_>>()
                    .join("/");
                reviews.push(ReviewFinding {
                    verdict,
                    reasons: sent.findings,
                });
            }
        }
        let mut successors = Vec::new();
        for &task in &op.conditions.successors {
            if let Some(read) = plan_task(&self.conn, task)? {
                successors.push(SuccessorBrief {
                    task_id: task,
                    status: read.status,
                    title: read_task(&self.conn, task)?.title().to_owned(),
                });
            }
        }
        let plan = latest_plan(&self.conn, id)?;
        let problems = match &plan {
            Some(latest) => {
                let members = super::proposals::read(&self.conn, latest.proposal_id)?
                    .task_ids()
                    .to_vec();
                check_replacement(&self.conn, &op, &latest.plan, &members, &[])?
            }
            None => Vec::new(),
        };
        let mut resolutions = Vec::new();
        let mut statement = self.conn.prepare(
            "SELECT id, created_at, payload FROM run_events WHERE task_id=?1 AND kind=?2
             AND json_extract(payload,'$.operation_id')=?3 ORDER BY id",
        )?;
        let rows = statement.query_map(
            params![op.task_id, EventKind::ReplanResolved.as_str(), id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )?;
        for row in rows {
            let (event_id, at, payload) = row?;
            let mut resolution: serde_json::Value =
                serde_json::from_str(&payload).unwrap_or_default();
            resolution["event_id"] = json!(event_id);
            resolution["at"] = json!(at);
            resolutions.push(resolution);
        }
        Ok(ReplanBrief {
            operation: op,
            turns: turns.into_iter().collect(),
            reviews,
            successors,
            plan,
            problems,
            resolutions,
        })
    }

    /// The brief of the operation whose task `proposal` replaces, if it
    /// replaces an open one's ([`Self::replan_brief`]).
    pub fn replacement_brief(&self, proposal: ProposalId) -> Result<Option<ReplanBrief>> {
        replacing_operation(&self.conn, proposal)?
            .map(|op| self.replan_brief(op.id))
            .transpose()
    }

    /// End the planning of each operation with the planner whose request
    /// ended without a proposal under review, as the trusted runtime (the
    /// planner that declined it has decided, whether or not its session
    /// has ended yet): a
    /// request its planner declined to continue the run
    /// (`request_declined` with `continue_run`) continues it
    /// (`continuing_after_park`, whose restore resumes the run on its
    /// saved work); one declined otherwise, out of planners (`exhausted`),
    /// or whose replacement proposal its planner withdrew and ended without
    /// another, leaves the operation `blocked` with the work kept, for a
    /// person. The operations moved, oldest first.
    pub fn settle_replan_requests(&mut self) -> Result<Vec<ReplanOperation>> {
        let now = self.generators.clock.now();
        let tx = self.write_tx("settle_replan_requests", TransactionBehavior::Immediate)?;
        let ended: Vec<(ReplanId, String)> = tx
            .prepare(
                "SELECT o.id, r.status FROM replan_operations o
                 JOIN plan_requests r ON r.id=o.request_id
                 WHERE o.closed_at IS NULL AND o.status IN ('parked','planning')
                 AND (r.status IN ('declined','exhausted')
                   OR (r.status='proposed' AND o.status='planning'
                     AND NOT EXISTS(SELECT 1 FROM planners p WHERE p.request_id=r.id
                         AND p.closed_at IS NULL)
                     AND NOT EXISTS(SELECT 1 FROM proposals x WHERE x.id=o.proposal_id
                         AND x.status IN ('submitted','revising','accepted'))))
                 ORDER BY o.id",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut moved = Vec::new();
        for (id, status) in ended {
            let op = read_operation(&tx, id)?;
            let request = op.request_id.context("an ended request")?;
            let declined: Option<(bool, String)> = tx
                .query_row(
                    "SELECT COALESCE(json_extract(payload,'$.continue_run'), 0),
                       COALESCE(json_extract(payload,'$.reason'), '')
                     FROM run_events WHERE kind='request_declined'
                     AND json_extract(payload,'$.request_id')=?1 ORDER BY id DESC LIMIT 1",
                    [request],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let (to, reason) =
                replan_plan::planning_end(request, status.parse::<RequestStatus>()?, declined);
            moved.push(move_in(
                &tx,
                id,
                op.generation,
                &ReplanMove {
                    to,
                    reason,
                    snapshot_id: None,
                    diagnosis: None,
                },
                now,
            )?);
        }
        tx.commit()?;
        Ok(moved)
    }
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
