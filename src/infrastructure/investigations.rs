//! Investigations (ADR-t1487-1 decisions 5, 7 and 8; the rules are
//! [`crate::domain::investigation`]): the cycles of `spike_cycles`, opened
//! by a Spike's claim and closed once they ended, and what the limit of
//! the open ones counts. [`refresh`] runs inside a write transaction (the
//! claim's, the supervisor's pass): it records first the request of each
//! completed Spike that has none, so a cycle never ends before its last
//! Spike's request is there.
use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::domain::{
    AskKind, ExecutionClass, GoalId, TaskId,
    claim_defer::waits_for_owner,
    investigation::{Investigation, InvestigationKey, Investigations, awaited_spikes},
};

/// The statuses of an unfinished Spike that keeps its investigation open:
/// any but a draft and the terminal ones.
const UNFINISHED: &str = "('submitted','ready','in_progress')";

/// A draft Spike that plan review sent back keeps its investigation open:
/// it is on its way back to submitted, not set aside.
const SENT_BACK: &str = "(t.status='draft' AND EXISTS(SELECT 1 FROM proposals x
    WHERE x.id=t.proposal_id AND x.status='revising'))";

/// Whether the open cycle `c` still runs: its goal is not abandoned, and a
/// Spike of it is unfinished (or sent back by plan review) or a request of
/// its Spikes is `open`; for a goalless Spike's, the Spike itself or its
/// request.
fn active(cycle: &str) -> String {
    format!(
        "(CASE WHEN {c}.goal_id IS NOT NULL THEN
            coalesce((SELECT g.verdict FROM goals g WHERE g.id={c}.goal_id), '') != 'abandoned'
            AND (EXISTS(SELECT 1 FROM tasks t WHERE t.goal_id={c}.goal_id
                    AND t.execution_class='spike' AND (t.status IN {UNFINISHED} OR {SENT_BACK}))
                 OR EXISTS(SELECT 1 FROM plan_requests r JOIN tasks t ON t.id=r.spike_task_id
                    WHERE t.goal_id={c}.goal_id AND r.status='open'))
          ELSE EXISTS(SELECT 1 FROM tasks t WHERE t.id={c}.task_id
                AND (t.status IN {UNFINISHED} OR {SENT_BACK}))
            OR EXISTS(SELECT 1 FROM plan_requests r WHERE r.spike_task_id={c}.task_id
                AND r.status='open') END)",
        c = cycle,
    )
}

/// Record the missing requests of completed Spikes and close the cycles
/// that ended at `now` (unix seconds): what a claim of a Spike needs before
/// it opens a cycle, and what each pass reconciles.
pub(super) fn settle(conn: &Connection, now: i64) -> Result<()> {
    super::plan_requests::record_missing_spike_requests(conn, now)?;
    conn.execute(
        &format!(
            "UPDATE spike_cycles SET closed_at=?1 WHERE closed_at IS NULL AND NOT {}",
            active("spike_cycles")
        ),
        [now],
    )?;
    Ok(())
}

/// [`settle`], then read the open investigations the limit counts.
pub(super) fn refresh(conn: &Connection, now: i64) -> Result<Investigations> {
    settle(conn, now)?;
    let cycles: Vec<(Option<GoalId>, Option<TaskId>)> = conn
        .prepare("SELECT goal_id, task_id FROM spike_cycles WHERE closed_at IS NULL ORDER BY id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut open = Vec::new();
    let mut unfinished = Vec::new();
    for (goal, task) in cycles {
        let (key, spikes, requests) = match (goal, task) {
            (Some(goal), _) => (
                InvestigationKey::Goal(goal),
                ids(
                    conn,
                    &format!(
                        "SELECT id FROM tasks WHERE goal_id=?1 AND execution_class='spike'
                         AND status IN {UNFINISHED} ORDER BY id"
                    ),
                    goal.as_i64(),
                )?,
                ids(
                    conn,
                    "SELECT r.id FROM plan_requests r JOIN tasks t ON t.id=r.spike_task_id
                     WHERE t.goal_id=?1 AND r.status='open' ORDER BY r.id",
                    goal.as_i64(),
                )?,
            ),
            (None, Some(task)) => (
                InvestigationKey::Spike(task),
                ids(
                    conn,
                    &format!("SELECT id FROM tasks WHERE id=?1 AND status IN {UNFINISHED}"),
                    task.as_i64(),
                )?,
                ids(
                    conn,
                    "SELECT id FROM plan_requests WHERE spike_task_id=?1 AND status='open'",
                    task.as_i64(),
                )?,
            ),
            (None, None) => continue,
        };
        let mut waits = Vec::new();
        for &spike in &spikes {
            waits.push(spike_waits_for_person(conn, TaskId::new(spike))?);
        }
        for &request in &requests {
            waits.push(request_waits_for_person(conn, request)?);
        }
        unfinished.extend(spikes.into_iter().map(TaskId::new));
        open.push(Investigation { key, waits });
    }
    let mut predecessors: BTreeMap<TaskId, Vec<TaskId>> = BTreeMap::new();
    let edges: Vec<(TaskId, TaskId)> = conn
        .prepare(
            "SELECT d.task_id, d.predecessor_id FROM task_dependencies d
             JOIN tasks t ON t.id=d.task_id WHERE t.status NOT IN ('completed','canceled')",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (task, predecessor) in edges {
        predecessors.entry(task).or_default().push(predecessor);
    }
    let spikes: HashSet<TaskId> = conn
        .prepare("SELECT id FROM tasks WHERE execution_class='spike' AND status='ready'")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Investigations {
        open,
        awaited: awaited_spikes(&unfinished, &predecessors, &spikes),
    })
}

/// Open the investigation of the Spike `task` (of `goal`) it was just
/// claimed for, inside the claim's transaction, unless it is open: whether
/// it opened one.
pub(super) fn open_cycle(
    conn: &Connection,
    task: TaskId,
    class: ExecutionClass,
    goal: Option<GoalId>,
    now: i64,
) -> Result<bool> {
    if !class.is_spike() {
        return Ok(false);
    }
    let opened = match goal {
        Some(goal) => conn.execute(
            "INSERT INTO spike_cycles(goal_id, task_id, opened_at) SELECT ?1, ?2, ?3
             WHERE NOT EXISTS(SELECT 1 FROM spike_cycles WHERE goal_id=?1 AND closed_at IS NULL)",
            params![goal, task, now],
        )?,
        None => conn.execute(
            "INSERT INTO spike_cycles(task_id, opened_at) SELECT ?1, ?2
             WHERE NOT EXISTS(SELECT 1 FROM spike_cycles WHERE goal_id IS NULL AND task_id=?1
                 AND closed_at IS NULL)",
            params![task, now],
        )?,
    };
    Ok(opened == 1)
}

/// How many investigations `goal` has had, the open one included.
pub(super) fn goal_cycles(conn: &Connection, goal: GoalId) -> Result<usize> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM spike_cycles WHERE goal_id=?1",
        [goal],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(count)?)
}

/// Since when the unfinished Spike `task` only waits for a person: its run
/// on a question of [`waits_for_owner`], or its proposal on plan review's
/// question to a person; `None` while it moves (ready, or worked on).
fn spike_waits_for_person(conn: &Connection, task: TaskId) -> Result<Option<i64>> {
    let status: String =
        conn.query_row("SELECT status FROM tasks WHERE id=?1", [task], |r| r.get(0))?;
    match status.as_str() {
        "in_progress" => {
            let asks: Vec<(String, i64)> = conn
                .prepare(
                    "SELECT kind, created_at FROM asks WHERE task_id=?1 AND run_id IS NOT NULL
                     AND answered_at IS NULL AND closed_at IS NULL",
                )?
                .query_map([task], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(asks
                .into_iter()
                .filter(|(kind, _)| waits_for_owner(&AskKind::read(kind)))
                .map(|(_, at)| at)
                .min())
        }
        "submitted" => Ok(conn
            .query_row(
                "SELECT min(a.created_at) FROM asks a JOIN tasks m ON m.id=a.task_id
                 JOIN tasks t ON t.id=?1 AND t.proposal_id IS NOT NULL AND m.proposal_id=t.proposal_id
                 WHERE a.kind='approve_plan' AND a.answered_at IS NULL AND a.closed_at IS NULL",
                [task],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten()),
        _ => Ok(None),
    }
}

/// Since when the `open` request `request` only waits for a person: its
/// `planner_question` unanswered while no planner works on it (none is
/// open, or the one open was asked to exit for the answer alone).
fn request_waits_for_person(conn: &Connection, request: i64) -> Result<Option<i64>> {
    let working: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM planners WHERE request_id=?1 AND closed_at IS NULL
             AND answer_wait_at IS NULL)",
        [request],
        |r| r.get(0),
    )?;
    if working {
        return Ok(None);
    }
    Ok(conn.query_row(
        "SELECT min(created_at) FROM asks WHERE request_id=?1 AND kind='planner_question'
         AND answered_at IS NULL AND closed_at IS NULL",
        [request],
        |r| r.get::<_, Option<i64>>(0),
    )?)
}

fn ids(conn: &Connection, sql: &str, param: i64) -> Result<Vec<i64>> {
    Ok(conn
        .prepare(sql)?
        .query_map([param], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}
