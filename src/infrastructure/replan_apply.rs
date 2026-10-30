//! The trusted runtime's atomic apply of a replacement (ADR-t1521-2
//! decision 5): an operation whose latest plan plan review approved moves
//! to `applying` ([`SqliteQueue::begin_replan_apply`]), and then one write
//! transaction checks everything again ([`replan_apply::apply_problems`])
//! and writes the whole replacement ([`SqliteQueue::apply_replacement`]):
//! the task and its parked run end `replaced`, the children and what each
//! takes of the saved work go to `replan_replacements`, the acceptance
//! mappings to `replan_mappings`, every task that waits for the task (one
//! that came to since included) waits for the children in its place, the
//! children become ready, the run's open asks are closed, and the
//! operation is `applied`, with `replan_applied`. Any problem or failure
//! rolls all of it back and holds the operation `blocked`, from
//! `applying`, with why: the snapshot, its ref and the run stay as the
//! park left them. It then goes on as its problems say
//! ([`replan_apply::ApplyRecovery`]): checked again at each pass, back to a
//! planner on a new request, or held for a person. A second apply of an
//! applied operation writes nothing.

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::json;
use std::collections::BTreeMap;

use super::replans::{
    approved_children as approved_children_of, check_replacement, child_version, latest_plan,
    move_in_as, read_operation, write_mappings,
};
use super::sqlite::{SqliteQueue, enum_col, event, read_task, transition_task};
use crate::domain::event_kind::EventKind;
use crate::domain::replan_apply::{
    self, ApplyOutcome, ApplyProblem, ApplyRecovery, ApplyState, ChildCarry, WaitNode,
};
use crate::domain::replan_plan::{ItemDisposition, PlanRevision};
use crate::domain::task_replan::{ConditionMapping, ReplanMove, ReplanOperation, ReplanStatus};
use crate::domain::{GoalId, ReplanId, RunStatus, TaskAction, TaskId, TaskStatus};

impl SqliteQueue {
    /// The operations whose replacement the trusted runtime applies now,
    /// oldest first: those `reviewing` whose latest plan plan review
    /// approved, its proposal accepted, those left `applying` (an apply a
    /// restart did not finish), and those a refused apply held `blocked`,
    /// checked again.
    pub fn replacements_to_apply(&self) -> Result<Vec<ReplanOperation>> {
        let ids: Vec<ReplanId> = self
            .conn
            .prepare(
                "SELECT o.id FROM replan_operations o
                 WHERE o.closed_at IS NULL AND (o.status='applying'
                   OR (o.status='blocked' AND o.return_phase='applying')
                   OR (o.status='reviewing' AND EXISTS(
                     SELECT 1 FROM replan_plans p JOIN proposals x ON x.id=p.proposal_id
                     WHERE p.operation_id=o.id AND p.proposal_id=o.proposal_id
                       AND p.approved_at IS NOT NULL AND x.status='accepted'
                       AND p.revision=(SELECT MAX(revision) FROM replan_plans
                                       WHERE operation_id=o.id))))
                 ORDER BY o.id",
            )?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter()
            .map(|id| read_operation(&self.conn, id))
            .collect()
    }

    /// Move operation `id`, read under `generation`, from `reviewing` to
    /// `applying` as the runtime starts its apply: it still guards the task
    /// and leaves its planner, so no revise, plan or answer moves it while
    /// the runtime checks the snapshot. One already `applying` is left as
    /// it is.
    pub fn begin_replan_apply(&mut self, id: ReplanId, generation: i64) -> Result<ReplanOperation> {
        let now = self.generators.clock.now();
        let tx = self.write_tx("begin_replan_apply", TransactionBehavior::Immediate)?;
        let op = read_operation(&tx, id)?;
        let op = if op.status == ReplanStatus::Applying && op.generation == generation {
            op
        } else {
            let revision = latest_plan(&tx, id)?.map(|plan| plan.revision);
            move_in_as(
                &tx,
                id,
                generation,
                &ReplanMove {
                    to: ReplanStatus::Applying,
                    reason: format!(
                        "the runtime applies plan revision {} that plan review approved",
                        revision.unwrap_or_default()
                    ),
                    snapshot_id: None,
                    diagnosis: None,
                },
                now,
                true,
            )?
        };
        tx.commit()?;
        Ok(op)
    }

    /// Apply the replacement of operation `id`, read under `generation`,
    /// in one write transaction, or refuse it whole ([`ApplyOutcome`]).
    /// `snapshot_problem` is why the runtime found the operation's snapshot
    /// not as saved (its files or its ref), which refuses it for a person.
    /// An operation held `blocked` by an earlier refusal is applied once
    /// nothing refuses it, and otherwise left as it is unless it now needs
    /// a new plan.
    pub fn apply_replacement(
        &mut self,
        id: ReplanId,
        generation: i64,
        snapshot_problem: Option<&str>,
    ) -> Result<ApplyOutcome> {
        let now = self.generators.clock.now();
        let tx = self.write_tx("apply_replacement", TransactionBehavior::Immediate)?;
        let op = read_operation(&tx, id)?;
        if op.status == ReplanStatus::Applied {
            return Ok(ApplyOutcome::AlreadyApplied);
        }
        // Only an operation the runtime began to apply, or held from its
        // apply, is applied or held here: one a submit or an answer moved
        // since the pass read it is left to that move.
        let begun = op.status == ReplanStatus::Applying
            || (op.status == ReplanStatus::Blocked
                && op.return_phase == Some(ReplanStatus::Applying));
        if !begun || op.generation != generation {
            return Ok(ApplyOutcome::Refused {
                problems: vec![format!(
                    "replan operation {id} is {} at generation {}, not the apply begun at generation {generation}",
                    op.status.as_str(),
                    op.generation
                )],
                recovery: ApplyRecovery::Retry,
                recorded: false,
            });
        }
        let attempt = || -> Result<std::result::Result<ReplanOperation, Vec<ApplyProblem>>> {
            let mut problems = problems_now(&tx, &op, generation)?;
            if let Some(problem) = snapshot_problem {
                problems.push(ApplyProblem::new(ApplyRecovery::Person, problem));
            }
            if !problems.is_empty() {
                return Ok(Err(problems));
            }
            write_replacement(&tx, &op, generation, now).map(Ok)
        };
        let problems = match attempt() {
            Ok(Ok(applied)) => {
                tx.commit()?;
                return Ok(ApplyOutcome::Applied(Box::new(applied)));
            }
            Ok(Err(problems)) => problems,
            Err(error) => vec![ApplyProblem::new(
                ApplyRecovery::Retry,
                format!("the apply failed and was rolled back: {error:#}"),
            )],
        };
        drop(tx);
        let recovery = replan_apply::recovery_of(&problems).unwrap_or(ApplyRecovery::Retry);
        let problems: Vec<String> = problems.into_iter().map(|problem| problem.reason).collect();
        let recorded = self.hold_apply(id, generation, &problems, recovery, now)?;
        Ok(ApplyOutcome::Refused {
            problems,
            recovery,
            recorded,
        })
    }

    /// Hold operation `id` `blocked` with `problems`, in a transaction of
    /// its own after the apply's rolled back, when it is still where the
    /// apply read it (`applying`, or `blocked` from it, under
    /// `generation`). One held before whose reasons changed (a failed write
    /// that now finds the snapshot gone) keeps them as its
    /// `status_reason`, with `replan_apply_rechecked`; with the same
    /// reasons nothing is written. When the refusal calls for a new plan,
    /// the operation then goes back to a planner
    /// ([`super::replans::replan_again`]). Whether anything was written.
    fn hold_apply(
        &mut self,
        id: ReplanId,
        generation: i64,
        problems: &[String],
        recovery: ApplyRecovery,
        now: i64,
    ) -> Result<bool> {
        let tx = self.write_tx("hold_apply", TransactionBehavior::Immediate)?;
        let mut op = read_operation(&tx, id)?;
        let reason = format!(
            "the replacement was not applied, nothing of it is written and the saved work is kept: {}",
            problems.join("; ")
        );
        let mut recorded = false;
        if op.generation != generation {
            return Ok(false);
        }
        if op.status == ReplanStatus::Applying {
            op = move_in_as(
                &tx,
                id,
                generation,
                &ReplanMove {
                    to: ReplanStatus::Blocked,
                    reason,
                    snapshot_id: None,
                    diagnosis: None,
                },
                now,
                true,
            )?;
            recorded = true;
        } else if op.status == ReplanStatus::Blocked
            && op.return_phase == Some(ReplanStatus::Applying)
            && op.status_reason.as_deref() != Some(reason.as_str())
        {
            tx.execute(
                "UPDATE replan_operations SET status_reason=?1, updated_at=?2 WHERE id=?3",
                params![reason, now, id],
            )?;
            event(
                &tx,
                op.task_id,
                Some(&op.run_id),
                EventKind::ReplanApplyRechecked,
                json!({"operation_id": id, "generation": generation,
                       "recovery": recovery, "problems": problems}),
            )?;
            op = read_operation(&tx, id)?;
            recorded = true;
        }
        if op.status == ReplanStatus::Blocked
            && op.return_phase == Some(ReplanStatus::Applying)
            && recovery == ApplyRecovery::Replan
        {
            super::replans::replan_again(
                &tx,
                &op,
                format!(
                    "the approved plan no longer applies, so a planner makes a new one from the saved work, continues the run or withdraws: {}",
                    problems.join("; ")
                ),
                now,
            )?;
            recorded = true;
        }
        tx.commit()?;
        Ok(recorded)
    }

    /// What child `task` takes of the saved work of the run its applied
    /// operation replaced, if it takes any ([`ChildCarry`]).
    pub fn child_carry(&self, task: TaskId) -> Result<Option<ChildCarry>> {
        child_carry(&self.conn, task)
    }
}

/// Every task the applied replacements of `from` reach, through the
/// replacements of a replaced child too, with its status, in ID order, read
/// on `conn`: a replaced task is resolved only once each of these has
/// ended (ADR-t1521-2 decision 5), wherever it belongs now.
pub(super) fn replacement_tasks(
    conn: &Connection,
    from: &[TaskId],
) -> Result<Vec<(TaskId, TaskStatus)>> {
    if from.is_empty() {
        return Ok(Vec::new());
    }
    Ok(conn
        .prepare(
            "WITH RECURSIVE reached(id) AS (
               SELECT r.to_task_id FROM replan_replacements r
                 JOIN replan_operations o ON o.id=r.operation_id AND o.status='applied'
               WHERE r.from_task_id IN (SELECT value FROM json_each(?1))
               UNION
               SELECT r.to_task_id FROM replan_replacements r
                 JOIN replan_operations o ON o.id=r.operation_id AND o.status='applied'
                 JOIN reached ON r.from_task_id=reached.id
             )
             SELECT t.id, t.status FROM reached JOIN tasks t ON t.id=reached.id ORDER BY t.id",
        )?
        .query_map([serde_json::to_string(from)?], |r| {
            Ok((r.get(0)?, enum_col(r, "status")?))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// Whether replaced task `task` is not resolved yet: a task its
/// replacements reach has not ended ([`replacement_tasks`]), or a record on
/// the way is broken ([`unresolved_replacements`]).
pub(super) fn replacement_open(conn: &Connection, task: TaskId) -> Result<bool> {
    Ok(replacement_tasks(conn, &[task])?
        .iter()
        .any(|(_, status)| !status.is_terminal())
        || !unresolved_replacements(conn, &[task])?.is_empty())
}

/// The replaced tasks reached from replaced tasks `from` whose applied
/// replacement record is empty, names a task that is not there or loops
/// ([`replan_apply::unresolved_replacements`]), read on `conn`. The walk
/// visits each task once, so it ends on a loop that the recursive query of
/// [`replacement_tasks`] would only stop at.
pub(super) fn unresolved_replacements(
    conn: &Connection,
    from: &[TaskId],
) -> Result<Vec<replan_apply::UnresolvedReplacement>> {
    if from.is_empty() {
        return Ok(Vec::new());
    }
    let mut replacements: BTreeMap<TaskId, Vec<TaskId>> = BTreeMap::new();
    let rows: Vec<(TaskId, TaskId)> = conn
        .prepare(
            "SELECT r.from_task_id, r.to_task_id FROM replan_replacements r
             JOIN replan_operations o ON o.id=r.operation_id AND o.status='applied'
             ORDER BY r.from_task_id, r.to_task_id",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (task, to) in rows {
        replacements.entry(task).or_default().push(to);
    }
    let statuses: BTreeMap<TaskId, TaskStatus> = conn
        .prepare(
            "SELECT id, status FROM tasks
             WHERE id IN (SELECT value FROM json_each(?1))
                OR id IN (SELECT r.to_task_id FROM replan_replacements r
                          JOIN replan_operations o ON o.id=r.operation_id
                            AND o.status='applied')",
        )?
        .query_map([serde_json::to_string(from)?], |r| {
            Ok((r.get(0)?, enum_col(r, "status")?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(replan_apply::unresolved_replacements(
        from,
        &replacements,
        &statuses,
    ))
}

/// [`unresolved_replacements`] from goal `goal`'s replaced tasks: while
/// any is there, the goal neither starts a review nor closes as achieved.
pub(super) fn goal_unresolved(
    conn: &Connection,
    goal: GoalId,
) -> Result<Vec<replan_apply::UnresolvedReplacement>> {
    let replaced: Vec<TaskId> = conn
        .prepare("SELECT id FROM tasks WHERE goal_id=?1 AND status='replaced' ORDER BY id")?
        .query_map([goal], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    unresolved_replacements(conn, &replaced)
}

/// [`unresolved_replacements`] from the replaced tasks of each open goal
/// in one walk per goal (a loop is found once, where it closes), and from
/// those with no goal, each with the goal it holds back, in goal and task
/// order: what `status` shows a person, who decides the goal (it closes
/// otherwise) since the record is not fixed by hand. A broken task two
/// goals reach is shown for each.
pub(super) fn open_unresolved(
    conn: &Connection,
) -> Result<Vec<(Option<GoalId>, replan_apply::UnresolvedReplacement)>> {
    let replaced: Vec<(Option<GoalId>, TaskId)> = conn
        .prepare(
            "SELECT t.goal_id, t.id FROM tasks t LEFT JOIN goals g ON g.id=t.goal_id
             WHERE t.status='replaced' AND (t.goal_id IS NULL OR g.closed_at IS NULL)
             ORDER BY t.goal_id, t.id",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut by_goal: BTreeMap<Option<GoalId>, Vec<TaskId>> = BTreeMap::new();
    for (goal, task) in replaced {
        by_goal.entry(goal).or_default().push(task);
    }
    let mut found = Vec::new();
    for (goal, roots) in by_goal {
        for unresolved in unresolved_replacements(conn, &roots)? {
            found.push((goal, unresolved));
        }
    }
    Ok(found)
}

/// The tasks outside goal `goal` that its replaced tasks' replacements
/// reach ([`replacement_tasks`]), which hold its review and its close back
/// as its own tasks do.
pub(super) fn goal_replacing(conn: &Connection, goal: GoalId) -> Result<Vec<(TaskId, TaskStatus)>> {
    let replaced: Vec<TaskId> = conn
        .prepare("SELECT id FROM tasks WHERE goal_id=?1 AND status='replaced' ORDER BY id")?
        .query_map([goal], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut reached = replacement_tasks(conn, &replaced)?;
    let members: std::collections::BTreeSet<TaskId> = conn
        .prepare("SELECT id FROM tasks WHERE goal_id=?1")?
        .query_map([goal], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    reached.retain(|(task, _)| !members.contains(task));
    Ok(reached)
}

/// What replaced goal `goal`'s replaced tasks, through the replacements of
/// replaced children too, and the tasks those replacements reach outside
/// the goal ([`replan_apply::GoalReplacements`]), read on `conn`.
pub(super) fn goal_replacements(
    conn: &Connection,
    goal: GoalId,
) -> Result<replan_apply::GoalReplacements> {
    let members: Vec<TaskId> = conn
        .prepare("SELECT id FROM tasks WHERE goal_id=?1 AND status='replaced' ORDER BY id")?
        .query_map([goal], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut replaced: std::collections::BTreeSet<TaskId> = members.iter().copied().collect();
    replaced.extend(
        replacement_tasks(conn, &members)?
            .into_iter()
            .filter(|(_, status)| *status == TaskStatus::Replaced)
            .map(|(task, _)| task),
    );
    let mut lineage = Vec::new();
    for task in replaced {
        let id: Option<ReplanId> = conn
            .query_row(
                "SELECT r.operation_id FROM replan_replacements r
                 JOIN replan_operations o ON o.id=r.operation_id AND o.status='applied'
                 WHERE r.from_task_id=?1 ORDER BY r.operation_id DESC LIMIT 1",
                [task],
                |r| r.get(0),
            )
            .optional()?;
        let Some(id) = id else {
            continue;
        };
        let op = read_operation(conn, id)?;
        let plan = latest_plan(conn, id)?;
        lineage.push(replan_apply::ReplacementLineage {
            operation: id,
            replaced_task: task,
            task_version: op.task_version.clone(),
            goal_acceptance_version: op.goal_acceptance_version,
            plan_revision: plan.as_ref().map_or(0, |plan| plan.revision),
            plan_hash: plan.map(|plan| plan.hash).unwrap_or_default(),
            conditions: op.conditions.acceptance.clone(),
            children: op.replaced_by.clone(),
            mappings: op.mappings.clone(),
        });
    }
    let outside = goal_replacing(conn, goal)?
        .into_iter()
        .map(|(task, _)| task)
        .collect();
    Ok(replan_apply::GoalReplacements { lineage, outside })
}

/// [`SqliteQueue::child_carry`] on `conn`.
pub(super) fn child_carry(conn: &Connection, task: TaskId) -> Result<Option<ChildCarry>> {
    let carry = conn
        .query_row(
            "SELECT r.operation_id, r.from_task_id, r.carry, r.carry_order, o.run_id,
               COALESCE(o.snapshot_id, '') AS snapshot_id, o.proposal_id
             FROM replan_replacements r JOIN replan_operations o ON o.id=r.operation_id
             WHERE r.to_task_id=?1 AND o.status='applied'
               AND r.carry IN ('snapshot_head','snapshot_diff')
             ORDER BY r.operation_id DESC LIMIT 1",
            [task],
            |r| {
                Ok(ChildCarry {
                    operation: r.get("operation_id")?,
                    from_task: r.get("from_task_id")?,
                    from_run: r.get("run_id")?,
                    proposal: r.get("proposal_id")?,
                    snapshot_id: r.get("snapshot_id")?,
                    carry: enum_col(r, "carry")?,
                    order: r.get("carry_order")?,
                })
            },
        )
        .optional()?;
    if let Some(carry) = &carry {
        ensure!(
            !carry.snapshot_id.is_empty(),
            "applied replan operation {} has no snapshot",
            carry.operation
        );
    }
    Ok(carry)
}

/// The tasks that wait for `task` now and have not ended, with their
/// statuses, in ID order: a canceled successor waits for nothing, so the
/// apply neither refuses for it nor moves its edge.
pub(super) fn successors_now(
    conn: &Connection,
    task: TaskId,
) -> Result<BTreeMap<TaskId, TaskStatus>> {
    let all: Vec<(TaskId, TaskStatus)> = conn
        .prepare(
            "SELECT t.id, t.status FROM task_dependencies d JOIN tasks t ON t.id=d.task_id
             WHERE d.predecessor_id=?1 ORDER BY t.id",
        )?
        .query_map([task], |r| Ok((r.get(0)?, enum_col(r, "status")?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(all
        .into_iter()
        .filter(|(_, status)| !status.is_terminal())
        .collect())
}

/// The latest plan of `op`, which an apply needs.
fn plan_of(conn: &Connection, op: &ReplanOperation) -> Result<Option<PlanRevision>> {
    latest_plan(conn, op.id)
}

/// The wait graph (ADR-0038) as the apply of the replacement of `task`
/// would leave it before it adds the successors' edges onto the children
/// and the task's onto them, read on `conn`: each task's predecessors and
/// the goals it depends on, each goal's tasks but a canceled one (which
/// never holds its goal back), and each task an applied replacement
/// replaced, its replacement tasks (its goal waits for them,
/// [`replacement_tasks`]), as `dep add` reads it (`sqlite::waits_for`).
/// The edges onto `task` are left out, since the apply re-points them.
pub(super) fn wait_graph(
    conn: &Connection,
    task: TaskId,
) -> Result<BTreeMap<WaitNode, Vec<WaitNode>>> {
    let mut waits: BTreeMap<WaitNode, Vec<WaitNode>> = BTreeMap::new();
    let mut add = |from: WaitNode, to: WaitNode| waits.entry(from).or_default().push(to);
    let rows: Vec<(TaskId, TaskId)> = conn
        .prepare("SELECT task_id, predecessor_id FROM task_dependencies WHERE predecessor_id<>?1")?
        .query_map([task], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (from, to) in rows {
        add(WaitNode::Task(from), WaitNode::Task(to));
    }
    let rows: Vec<(TaskId, GoalId)> = conn
        .prepare("SELECT task_id, goal_id FROM task_goal_dependencies")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (from, to) in rows {
        add(WaitNode::Task(from), WaitNode::Goal(to));
    }
    let rows: Vec<(GoalId, TaskId)> = conn
        .prepare("SELECT goal_id, id FROM tasks WHERE goal_id IS NOT NULL AND status<>'canceled'")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (from, to) in rows {
        add(WaitNode::Goal(from), WaitNode::Task(to));
    }
    let rows: Vec<(TaskId, TaskId)> = conn
        .prepare(
            "SELECT r.from_task_id, r.to_task_id FROM replan_replacements r
             JOIN replan_operations o ON o.id=r.operation_id AND o.status='applied'",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (from, to) in rows {
        add(WaitNode::Task(from), WaitNode::Task(to));
    }
    Ok(waits)
}

/// Why `op` cannot be applied now, read on `conn` inside the apply's
/// transaction ([`replan_apply::apply_problems`]).
fn problems_now(
    conn: &Connection,
    op: &ReplanOperation,
    generation: i64,
) -> Result<Vec<ApplyProblem>> {
    let plan = plan_of(conn, op)?;
    let proposal = plan
        .as_ref()
        .map(|plan| super::proposals::read(conn, plan.proposal_id))
        .transpose()?;
    let (run, run_leased): (RunStatus, bool) = conn.query_row(
        "SELECT status, EXISTS(SELECT 1 FROM run_leases WHERE run_id=?1) AS leased
         FROM task_runs WHERE id=?1",
        [&op.run_id],
        |r| Ok((enum_col(r, "status")?, r.get(1)?)),
    )?;
    let mut children = BTreeMap::new();
    let mut child_versions = BTreeMap::new();
    let mut approved_children = None;
    let mut plan_problems = Vec::new();
    let successors = successors_now(conn, op.task_id)?;
    if let (Some(plan), Some(proposal)) = (&plan, &proposal) {
        for child in plan.plan.child_ids() {
            children.insert(child, read_task(conn, child)?.status());
            child_versions.insert(child, child_version(conn, child)?);
        }
        approved_children = approved_children_of(conn, op.id, plan.revision)?;
        // The check of the plan sees a loop through the whole wait graph
        // too (`replans::check_replacement`).
        plan_problems = check_replacement(conn, op, &plan.plan, proposal.task_ids(), &[])?;
    }
    Ok(replan_apply::apply_problems(&ApplyState {
        operation: op,
        generation,
        plan: plan.as_ref(),
        proposal: proposal.as_ref().map(crate::domain::Proposal::status),
        task: read_task(conn, op.task_id)?.status(),
        run,
        run_leased,
        children,
        child_versions,
        approved_children,
        successors,
        plan_problems,
    }))
}

/// Write the replacement of `op`, checked by [`problems_now`] in the same
/// transaction, and the operation as it then is.
fn write_replacement(
    conn: &Connection,
    op: &ReplanOperation,
    generation: i64,
    now: i64,
) -> Result<ReplanOperation> {
    let latest = plan_of(conn, op)?.context("an approved plan")?;
    let plan = &latest.plan;
    let children = plan.child_ids();
    let stamp = crate::application::timestamp(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::try_from(now)?),
    );
    // The operation leaves its task first: closed, it no longer guards the
    // task or holds the children back from `ready`.
    let reason = format!(
        "plan revision {} of proposal {} replaced task {} with tasks {children:?}",
        latest.revision, latest.proposal_id, op.task_id
    );
    for to in [ReplanStatus::Applying, ReplanStatus::Applied] {
        let current = read_operation(conn, op.id)?;
        if current.status == to {
            continue;
        }
        move_in_as(
            conn,
            op.id,
            generation,
            &ReplanMove {
                to,
                reason: reason.clone(),
                snapshot_id: None,
                diagnosis: None,
            },
            now,
            true,
        )?;
    }
    // The parked run and its task end, neither completed nor canceled.
    super::runtime_store::end_replaced_run(conn, &op.run_id)?;
    super::sqlite::replace_task(conn, op.task_id, &op.run_id, op.id, &stamp, now)?;
    let mut insert = conn.prepare(
        "INSERT INTO replan_replacements(operation_id, from_task_id, to_task_id, carry, carry_order)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for child in &plan.children {
        insert.execute(params![
            op.id,
            op.task_id,
            child.task_id,
            child.carry.as_str(),
            child.order
        ])?;
    }
    write_mappings(conn, op.id, &mappings_of(plan))?;
    // Every task that waits for the task now waits for the children in
    // its place, one that came to wait since the plan included.
    let successors: Vec<TaskId> = successors_now(conn, op.task_id)?.into_keys().collect();
    let edges = replan_apply::successor_edges(plan, &successors);
    for (successor, needs) in &edges {
        conn.execute(
            "DELETE FROM task_dependencies WHERE task_id=?1 AND predecessor_id=?2",
            params![successor, op.task_id],
        )?;
        for need in needs {
            conn.execute(
                "INSERT OR IGNORE INTO task_dependencies(task_id, predecessor_id) VALUES (?1, ?2)",
                params![successor, need],
            )?;
        }
    }
    for &child in &children {
        transition_task(conn, child, TaskAction::Approve, &stamp)?;
    }
    // The run's asks can no longer be applied to it.
    let note = format!(
        "run {} ended replaced by tasks {children:?} (replan operation {}); this ask no longer applies to it",
        op.run_id, op.id
    );
    let asks: Vec<crate::domain::Ask> = conn
        .prepare("SELECT * FROM asks WHERE run_id=?1 AND closed_at IS NULL ORDER BY id")?
        .query_map([&op.run_id], super::asks::ask_row)?
        .collect::<rusqlite::Result<_>>()?;
    for ask in &asks {
        super::asks::discard_by_runtime(conn, ask, "replaced", &note, now)?;
    }
    event(
        conn,
        op.task_id,
        Some(&op.run_id),
        EventKind::ReplanApplied,
        json!({"operation_id": op.id, "proposal_id": latest.proposal_id,
               "revision": latest.revision, "hash": latest.hash,
               "request_id": op.request_id, "snapshot_id": op.snapshot_id,
               "children": children,
               "carry": plan.children,
               "successors": edges.iter().map(|(task, needs)| json!({
                   "task_id": task, "before": [op.task_id], "after": needs}))
                   .collect::<Vec<_>>(),
               "closed_asks": asks.iter().map(|ask| ask.id).collect::<Vec<_>>()}),
    )?;
    read_operation(conn, op.id)
}

/// The condition mappings an applied `plan` records: one row per child
/// item that holds an item, and a deletion with the ask that decided it.
fn mappings_of(plan: &crate::domain::replan_plan::ReplacementPlan) -> Vec<ConditionMapping> {
    let mut rows = Vec::new();
    for mapping in &plan.mappings {
        if mapping.disposition == ItemDisposition::Deleted {
            rows.push(ConditionMapping {
                ordinal: mapping.ordinal,
                hash: mapping.hash.clone(),
                child_task_id: None,
                child_ordinal: None,
                disposition: mapping.disposition.as_str().to_owned(),
                authority: Some(json!({"ask_id": mapping.authority_ask_id,
                                       "reason": mapping.reason})),
            });
            continue;
        }
        for held in &mapping.children {
            rows.push(ConditionMapping {
                ordinal: mapping.ordinal,
                hash: mapping.hash.clone(),
                child_task_id: Some(held.task_id),
                child_ordinal: Some(held.ordinal),
                disposition: mapping.disposition.as_str().to_owned(),
                authority: mapping
                    .landed_task_id
                    .map(|landed| json!({"landed_task_id": landed})),
            });
        }
    }
    rows
}
