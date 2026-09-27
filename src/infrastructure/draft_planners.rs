//! Drafts the runtime opens a planner for (ADR-0041 decision 16, which
//! replaced the follow-up triage job of ADR-0037): where a draft the
//! runtime or a job registered came from (`draft_origins`), the planners of
//! the runtime's opened for it (`planners.draft_task_id`), the answers of
//! their `planner_question` asks, and the `follow_up_depth` that limits
//! what such a planner submits without a person. Opening a planner is one
//! write transaction that re-checks the draft first, so two supervisors
//! never open one for the same draft.
use crate::domain::event_kind;
use std::collections::HashMap;

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

use super::{
    asks::{ask_row, read_ask},
    sqlite::{SqliteQueue, event, read_task},
};
use crate::application::{
    DraftPlannerStart, DraftPlannerStore, FindingPlannerStart, PlannerAnswerRoute,
};
use crate::domain::{
    Ask, AskId, AskKind, DraftOrigin, DraftTarget, Finding, FindingId, FindingQuery, FindingStatus,
    FindingView, GoalId, MAX_DRAFT_PLANNERS, PlannerId, PlannerOrigin, PlannerSession, Task,
    TaskId, TaskStatus,
    follow_up::{FollowUpFacts, adopt_needs_person},
};

/// How long a claim on typing the answer of a `planner_question` holds
/// (seconds): the typing takes seconds, so a claim this old was left by a
/// supervisor that ended before it typed, and another may take it over.
pub const PLANNER_ANSWER_CLAIM_SECS: i64 = 120;

/// The drafts waiting for a planner of the runtime's: `draft`, in no
/// proposal (or in one withdrawn: a canceled proposal holds no draft), with an origin (in `draft_origins`, or `reopened` in `draft_reopens`), no planner of the runtime's open for it, no
/// planner of the runtime's still open for the withdrawn proposal it was
/// in, no `planner_question` about it nobody closed, not kept as a draft by an
/// answer and not exhausted. A `follow_up` ask the retired triage left
/// holds a draft back only when answered `keep_draft`: nothing applies its
/// other answers any more. Drafts registered before this existed match too (the
/// migration gave them their origin).
fn targets() -> String {
    format!(
        "SELECT t.id FROM tasks t
    WHERE (EXISTS(SELECT 1 FROM draft_origins o WHERE o.task_id=t.id)
        OR EXISTS(SELECT 1 FROM draft_reopens r WHERE r.task_id=t.id
            AND json_extract(r.material,'$.proposal_id')=t.proposal_id))
    AND t.status='draft' AND NOT EXISTS(SELECT 1 FROM proposals x
        WHERE x.id=t.proposal_id AND x.status!='canceled')
    AND NOT EXISTS(SELECT 1 FROM planners p WHERE p.closed_at IS NULL
        AND (p.draft_task_id=t.id OR (t.proposal_id IS NOT NULL AND p.proposal_id=t.proposal_id)))
    AND NOT EXISTS(SELECT 1 FROM asks a WHERE a.task_id=t.id AND a.run_id IS NULL
        AND ((a.kind='planner_question' AND a.closed_at IS NULL)
             OR (a.kind IN ('planner_question','follow_up') AND trim(a.answer)='keep_draft')))
    AND NOT EXISTS(SELECT 1 FROM run_events e WHERE e.task_id=t.id
        AND e.kind='{}')",
        event_kind::DRAFT_PLANNER_EXHAUSTED
    )
}

impl SqliteQueue {
    pub fn record_draft_origin(
        &mut self,
        task: TaskId,
        origin: DraftOrigin,
        material: &Value,
    ) -> Result<()> {
        ensure!(
            material.is_object(),
            "the material of a draft's origin must be a JSON object"
        );
        ensure!(
            origin != DraftOrigin::Reopened,
            "a reopened draft's origin is recorded by withdrawing its proposal"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let status = read_task(&tx, task)?.status();
        ensure!(
            status == TaskStatus::Draft,
            "task {task} is {}, not a draft",
            status.as_str()
        );
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO draft_origins(task_id, origin, material, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                task,
                origin.as_str(),
                serde_json::to_string(material)?,
                self.generators.clock.now()
            ],
        )?;
        ensure!(inserted == 1, "task {task} already has an origin");
        tx.commit()?;
        Ok(())
    }

    pub fn draft_origin(&self, task: TaskId) -> Result<Option<(DraftOrigin, Value)>> {
        draft_origin(&self.conn, task)
    }

    /// Where every draft with an origin came from; an origin this binary
    /// does not know is left out.
    pub fn draft_origins(&self) -> Result<HashMap<TaskId, DraftOrigin>> {
        Ok(self
            .conn
            .prepare("SELECT task_id, origin FROM draft_origins")?
            .query_map([], |r| Ok((r.get(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<(TaskId, String)>>>()?
            .into_iter()
            .filter_map(|(task, origin)| Some((task, origin.parse().ok()?)))
            .collect())
    }

    pub fn planner_drafts(&self) -> Result<Vec<DraftTarget>> {
        let ids: Vec<TaskId> = self
            .conn
            .prepare(&format!("{} ORDER BY t.id", targets()))?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter().map(|id| target(&self.conn, id)).collect()
    }

    /// See [`DraftPlannerStore::open_draft_planner`]. A draft that had
    /// [`MAX_DRAFT_PLANNERS`] planners records `draft_planner_exhausted`
    /// (the inbox's attention) instead.
    pub fn open_draft_planner(
        &mut self,
        draft: TaskId,
        answer: Option<AskId>,
    ) -> Result<DraftPlannerStart> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let eligible = match answer {
            None => is_target(&tx, draft)?,
            Some(ask) => {
                let ask = read_ask(&tx, ask)?;
                ask.task_id == Some(draft) && route_of(&tx, &ask)? == PlannerAnswerRoute::NewPlanner
            }
        };
        if !eligible {
            return Ok(DraftPlannerStart::Skipped);
        }
        let task = read_task(&tx, draft)?;
        let opened = planners_opened(&tx, draft)?;
        // A person's answer is carried past the limit: it was promised to
        // the runtime (`runtime_delivers`), and the planner it opens has
        // the person's decision to apply.
        if opened >= MAX_DRAFT_PLANNERS && answer.is_none() {
            event(
                &tx,
                draft,
                None,
                event_kind::DRAFT_PLANNER_EXHAUSTED,
                json!({
                    "planners": opened,
                    "ask_id": answer,
                    "reason": format!(
                        "{opened} planners of the runtime's ended without deciding the draft (at most {MAX_DRAFT_PLANNERS})"
                    ),
                }),
            )?;
            tx.commit()?;
            return Ok(DraftPlannerStart::Exhausted { attempts: opened });
        }
        tx.execute(
            "INSERT INTO planners(origin, draft_task_id, created_at) VALUES (?1, ?2, ?3)",
            params![
                PlannerOrigin::Runtime.as_str(),
                draft,
                self.generators.clock.now()
            ],
        )?;
        let planner = PlannerId::new(tx.last_insert_rowid());
        let attempt = opened + 1;
        let (origin, _) = draft_origin(&tx, draft)?.context("the draft has no origin")?;
        event(
            &tx,
            draft,
            None,
            event_kind::DRAFT_PLANNER_OPENED,
            json!({
                "planner_id": planner,
                "attempt": attempt,
                "origin": origin,
                "ask_id": answer,
                "goal_id": task.goal_id(),
            }),
        )?;
        let target = target(&tx, draft)?;
        tx.commit()?;
        Ok(DraftPlannerStart::Opened {
            planner: self.planner(planner)?,
            target: Box::new(target),
            attempt,
        })
    }

    pub fn planner_answers(&self) -> Result<Vec<Ask>> {
        Ok(self
            .conn
            .prepare(
                "SELECT * FROM asks WHERE kind='planner_question'
                 AND answered_at IS NOT NULL AND closed_at IS NULL ORDER BY id",
            )?
            .query_map([], ask_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn planner_answer_route(&self, ask: &Ask) -> Result<PlannerAnswerRoute> {
        route_of(&self.conn, ask)
    }

    /// Claim the typing of the answer of `id` into `planner`'s `workspace`
    /// in one write transaction (`planner_answer_claimed`): `false` when
    /// the ask was closed, no longer goes to that planner, or another
    /// process claimed it within [`PLANNER_ANSWER_CLAIM_SECS`] (two
    /// supervisors across a handoff). An older claim is taken over: its
    /// supervisor ended before typing (a typing that fails records
    /// `ask_delivery_failed`, which holds the answer back first).
    pub fn claim_planner_answer(
        &mut self,
        id: AskId,
        planner: PlannerId,
        workspace: &str,
    ) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ask = read_ask(&tx, id)?;
        // A question about a finding may be about no task: its claim is on
        // the finding's target (ADR-0044 decision 19).
        if ask.task_id.is_none() && ask.finding_id.is_none() {
            return Ok(false);
        }
        if !matches!(route_of(&tx, &ask)?, PlannerAnswerRoute::Planner(to) if to.id == planner) {
            return Ok(false);
        }
        let now = self.generators.clock.now();
        let claimed: bool = tx.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM run_events WHERE kind='{}'
             AND json_extract(payload,'$.ask_id')=?1
             AND COALESCE(json_extract(payload,'$.claimed_at'), 0) > ?2)",
                event_kind::PLANNER_ANSWER_CLAIMED
            ),
            params![id, now - PLANNER_ANSWER_CLAIM_SECS],
            |r| r.get(0),
        )?;
        if claimed {
            return Ok(false);
        }
        let payload = json!({"ask_id": id, "planner_id": planner, "workspace_id": workspace, "claimed_at": now});
        match (ask.finding_id, ask.task_id) {
            (Some(finding), _) => {
                let finding = super::findings::read_finding(&tx, finding)?;
                super::findings::finding_event(
                    &tx,
                    &finding,
                    event_kind::PLANNER_ANSWER_CLAIMED,
                    payload,
                )?;
            }
            (None, Some(task)) => {
                event(&tx, task, None, event_kind::PLANNER_ANSWER_CLAIMED, payload)?
            }
            (None, None) => return Ok(false),
        }
        tx.commit()?;
        Ok(true)
    }

    pub fn close_planner_answer(&mut self, id: AskId, why: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ask = read_ask(&tx, id)?;
        if ask.closed_at.is_some() {
            return Ok(());
        }
        ensure!(ask.answered_at.is_some(), "ask {id} is not answered");
        tx.execute(
            "UPDATE asks SET closed_at=?2 WHERE id=?1",
            params![id, self.generators.clock.now()],
        )?;
        let payload = json!({"ask_id": id, "reason": why});
        if let Some(finding) = ask.finding_id {
            let finding = super::findings::read_finding(&tx, finding)?;
            super::findings::finding_event(
                &tx,
                &finding,
                event_kind::PLANNER_ANSWER_CLOSED,
                payload,
            )?;
        } else if let Some(task) = ask.task_id {
            event(&tx, task, None, event_kind::PLANNER_ANSWER_CLOSED, payload)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn exhausted_drafts(&self) -> Result<Vec<Task>> {
        let ids: Vec<TaskId> = self
            .conn
            .prepare(&format!(
                "SELECT t.id FROM tasks t WHERE t.status='draft' AND EXISTS(
                     SELECT 1 FROM run_events e WHERE e.task_id=t.id
                     AND e.kind='{}') ORDER BY t.id",
                event_kind::DRAFT_PLANNER_EXHAUSTED
            ))?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter()
            .map(|id| read_task(&self.conn, id))
            .collect()
    }

    pub fn follow_up_depth(&self, task: TaskId) -> Result<i64> {
        depth(&self.conn, task)
    }

    pub fn set_follow_up_depth(&mut self, task: TaskId, depth: i64) -> Result<()> {
        ensure!(depth >= 0, "follow_up_depth must not be negative");
        ensure!(
            self.conn.execute(
                "UPDATE tasks SET follow_up_depth=?2 WHERE id=?1",
                params![task, depth],
            )? == 1,
            "task {task} does not exist"
        );
        Ok(())
    }
}

impl DraftPlannerStore for SqliteQueue {
    fn record_draft_origin(
        &mut self,
        task: TaskId,
        origin: DraftOrigin,
        material: &Value,
    ) -> Result<()> {
        SqliteQueue::record_draft_origin(self, task, origin, material)
    }
    fn draft_origin(&self, task: TaskId) -> Result<Option<(DraftOrigin, Value)>> {
        SqliteQueue::draft_origin(self, task)
    }
    fn planner_drafts(&self) -> Result<Vec<DraftTarget>> {
        SqliteQueue::planner_drafts(self)
    }
    fn open_draft_planner(
        &mut self,
        draft: TaskId,
        answer: Option<AskId>,
    ) -> Result<DraftPlannerStart> {
        SqliteQueue::open_draft_planner(self, draft, answer)
    }
    fn planner_answers(&self) -> Result<Vec<Ask>> {
        SqliteQueue::planner_answers(self)
    }
    fn planner_answer_route(&self, ask: &Ask) -> Result<PlannerAnswerRoute> {
        SqliteQueue::planner_answer_route(self, ask)
    }
    fn claim_planner_answer(
        &mut self,
        ask: AskId,
        planner: PlannerId,
        workspace: &str,
    ) -> Result<bool> {
        SqliteQueue::claim_planner_answer(self, ask, planner, workspace)
    }
    fn close_planner_answer(&mut self, ask: AskId, why: &str) -> Result<()> {
        SqliteQueue::close_planner_answer(self, ask, why)
    }
    fn record_task_event(&mut self, task: TaskId, kind: &str, payload: Value) -> Result<()> {
        event(&self.conn, task, None, kind, payload)
    }
    fn exhausted_drafts(&self) -> Result<Vec<Task>> {
        SqliteQueue::exhausted_drafts(self)
    }
    fn follow_up_depth(&self, task: TaskId) -> Result<i64> {
        SqliteQueue::follow_up_depth(self, task)
    }
    fn set_follow_up_depth(&mut self, task: TaskId, depth: i64) -> Result<()> {
        SqliteQueue::set_follow_up_depth(self, task, depth)
    }
    fn planner_findings(&self) -> Result<Vec<Finding>> {
        SqliteQueue::planner_findings(self)
    }
    fn open_finding_planner(
        &mut self,
        finding: FindingId,
        answer: Option<AskId>,
        limit: usize,
    ) -> Result<FindingPlannerStart> {
        SqliteQueue::open_finding_planner(self, finding, answer, limit)
    }
    fn improvements(&self, limit: usize) -> Result<crate::domain::ImprovementLimit> {
        SqliteQueue::improvements(self, limit)
    }
    fn finding_view(&self, finding: FindingId) -> Result<FindingView> {
        SqliteQueue::findings(
            self,
            &FindingQuery {
                id: Some(finding),
                full: true,
                ..FindingQuery::default()
            },
        )?
        .into_iter()
        .next()
        .with_context(|| format!("finding {finding} does not exist"))
    }
    fn finding_asks(&self, finding: FindingId) -> Result<Vec<Ask>> {
        SqliteQueue::finding_asks(self, finding)
    }
    fn settle_findings(&mut self) -> Result<Vec<(FindingId, FindingStatus)>> {
        SqliteQueue::settle_findings(self)
    }
    fn exhausted_findings(&self) -> Result<Vec<Finding>> {
        SqliteQueue::exhausted_findings(self)
    }
    fn record_finding_event(
        &mut self,
        finding: FindingId,
        kind: &str,
        payload: Value,
    ) -> Result<()> {
        SqliteQueue::record_finding_event(self, finding, kind, payload)
    }
    fn ask_delivered_to(&self, ask: AskId, workspace: &str) -> Result<bool> {
        SqliteQueue::ask_delivered_to(self, ask, workspace)
    }
    fn ask_delivery_failed(&self, ask: AskId) -> Result<bool> {
        SqliteQueue::ask_delivery_failed(self, ask)
    }
}

fn is_target(conn: &Connection, draft: TaskId) -> Result<bool> {
    Ok(conn.query_row(
        &format!("SELECT EXISTS({} AND t.id=?1)", targets()),
        [draft],
        |r| r.get(0),
    )?)
}

fn target(conn: &Connection, draft: TaskId) -> Result<DraftTarget> {
    let (origin, material) = draft_origin(conn, draft)?.context("the draft has no origin")?;
    Ok(DraftTarget {
        task: read_task(conn, draft)?,
        origin,
        material,
        planners: planners_opened(conn, draft)?,
    })
}

/// Record that the draft `task` is a ready task plan review reopened and
/// whose proposal was withdrawn (origin `reopened`, task 418), inside the
/// caller's write transaction; a later withdrawal replaces the material.
pub(super) fn record_reopened(
    conn: &Connection,
    task: TaskId,
    material: &Value,
    now: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO draft_reopens(task_id, material, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(task_id) DO UPDATE SET material=excluded.material, created_at=excluded.created_at",
        params![task, serde_json::to_string(material)?, now],
    )?;
    Ok(())
}

/// Where the draft came from and its material: `reopened` when plan review
/// reopened it and its proposal was withdrawn (while the task is still of
/// that proposal: a later proposal it joins makes the row stale), else its
/// `draft_origins` row.
pub(super) fn draft_origin(
    conn: &Connection,
    task: TaskId,
) -> Result<Option<(DraftOrigin, Value)>> {
    let reopened: Option<String> = conn
        .query_row(
            "SELECT r.material FROM draft_reopens r JOIN tasks t ON t.id=r.task_id
             WHERE r.task_id=?1 AND json_extract(r.material,'$.proposal_id')=t.proposal_id",
            [task],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(material) = reopened {
        return Ok(Some((
            DraftOrigin::Reopened,
            serde_json::from_str(&material)?,
        )));
    }
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT origin, material FROM draft_origins WHERE task_id=?1",
            [task],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(origin, material)| Ok((origin.parse()?, serde_json::from_str(&material)?)))
        .transpose()
}

fn planners_opened(conn: &Connection, draft: TaskId) -> Result<usize> {
    let count: i64 = conn.query_row(
        &format!(
            "SELECT count(*) FROM run_events WHERE task_id=?1 AND kind='{}'",
            event_kind::DRAFT_PLANNER_OPENED
        ),
        [draft],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(count)?)
}

/// Where the answer of `ask` goes: the planner of the runtime's not closed
/// that works on its task (opened for it as a draft, or for its proposal);
/// else closed by the supervisor for `keep_draft` (nothing to apply) or a
/// draft that moved on; else a new planner for a draft that still waits
/// (unless its planners are used up); else a person's.
pub(super) fn route_of(conn: &Connection, ask: &Ask) -> Result<PlannerAnswerRoute> {
    if ask.kind != AskKind::PlannerQuestion || ask.answer.is_none() || ask.closed_at.is_some() {
        return Ok(PlannerAnswerRoute::Person);
    }
    // A question about a finding goes the way of the finding's planners
    // (ADR-0044 decision 19).
    if let Some(finding) = ask.finding_id {
        return super::finding_planners::route_of(conn, finding);
    }
    let Some(task_id) = ask.task_id else {
        return Ok(PlannerAnswerRoute::Person);
    };
    let planner: Option<i64> = conn
        .query_row(
            "SELECT p.id FROM planners p JOIN tasks t ON t.id=?1
             WHERE p.origin='runtime' AND p.closed_at IS NULL
             AND (p.draft_task_id=t.id OR (t.proposal_id IS NOT NULL AND p.proposal_id=t.proposal_id))
             ORDER BY p.id DESC LIMIT 1",
            [task_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = planner {
        return Ok(PlannerAnswerRoute::Planner(read_planner(
            conn,
            PlannerId::new(id),
        )?));
    }
    // A draft kept for a person's planner needs nothing more of the
    // runtime's: nobody is left to tell.
    if ask.answer.as_deref().map(str::trim) == Some("keep_draft") {
        return Ok(PlannerAnswerRoute::Close);
    }
    let task = read_task(conn, task_id)?;
    if draft_origin(conn, task_id)?.is_none() {
        return Ok(PlannerAnswerRoute::Person);
    }
    let exhausted: bool = conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM run_events WHERE task_id=?1 AND kind='{}')",
            event_kind::DRAFT_PLANNER_EXHAUSTED
        ),
        [task_id],
        |r| r.get(0),
    )?;
    let in_proposal: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM tasks t JOIN proposals p ON p.id=t.proposal_id
         WHERE t.id=?1 AND p.status!='canceled')",
        [task_id],
        |r| r.get(0),
    )?;
    match task.status() {
        TaskStatus::Draft if !in_proposal && !exhausted => Ok(PlannerAnswerRoute::NewPlanner),
        TaskStatus::Draft => Ok(PlannerAnswerRoute::Person),
        _ => Ok(PlannerAnswerRoute::Close),
    }
}

fn read_planner(conn: &Connection, id: PlannerId) -> Result<PlannerSession> {
    conn.query_row(
        "SELECT * FROM planners WHERE id=?1",
        [id],
        super::planners::planner_row,
    )
    .optional()?
    .with_context(|| format!("planner {id} does not exist"))
}

fn depth(conn: &Connection, task: TaskId) -> Result<i64> {
    conn.query_row(
        "SELECT follow_up_depth FROM tasks WHERE id=?1",
        [task],
        |r| r.get(0),
    )
    .optional()?
    .with_context(|| format!("task {task} does not exist"))
}

/// Whether the task's goal takes tasks: it has one and it is not closed.
fn goal_open(conn: &Connection, goal: Option<GoalId>) -> Result<bool> {
    let Some(goal) = goal else {
        return Ok(false);
    };
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM goals WHERE id=?1 AND closed_at IS NULL)",
        [goal],
        |r| r.get(0),
    )?)
}

/// What a submission does to the drafts the runtime or a job registered,
/// inside the submit's transaction, before any task moves (ADR-0041
/// decision 16): a planner of the runtime's may not submit a follow_up
/// draft whose goal is closed or that is
/// [`FOLLOW_UP_ASK_DEPTH`](crate::domain::follow_up::FOLLOW_UP_ASK_DEPTH)
/// or more follow-ups from a person without a person's `adopt` answer to a
/// `planner_question` about it (the error says why). Returns what
/// [`record_adoptions`] records afterwards.
pub(super) fn check_adoptions(
    conn: &Connection,
    tasks: &[TaskId],
    origin: PlannerOrigin,
) -> Result<Vec<Adoption>> {
    let mut adoptions = Vec::new();
    for &task_id in tasks {
        let task = read_task(conn, task_id)?;
        if task.status() != TaskStatus::Draft {
            continue;
        }
        let depth = depth(conn, task_id)?;
        let Some((draft_origin, material)) = draft_origin(conn, task_id)? else {
            adoptions.push(Adoption {
                task: task_id,
                origin: None,
                material: Value::Null,
                by_person: origin == PlannerOrigin::Person,
                ask: None,
                depth,
            });
            continue;
        };
        let adopted: Option<AskId> = conn
            .query_row(
                "SELECT id FROM asks WHERE task_id=?1 AND run_id IS NULL
                 AND kind IN ('planner_question','follow_up') AND trim(answer)='adopt'
                 ORDER BY id DESC LIMIT 1",
                [task_id],
                |r| r.get(0),
            )
            .optional()?;
        // A draft a person already adopted (a revise sends it back to
        // `draft`) is not asked about again.
        let adopted_by_person: bool = conn.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM run_events WHERE task_id=?1
             AND kind IN ('{}','{}')
             AND json_extract(payload,'$.by')='person')",
                event_kind::FOLLOW_UP_ADOPTED,
                event_kind::DRAFT_ADOPTED
            ),
            [task_id],
            |r| r.get(0),
        )?;
        if origin == PlannerOrigin::Runtime
            && adopted.is_none()
            && !adopted_by_person
            && draft_origin == DraftOrigin::FollowUp
            && let Some(why) = adopt_needs_person(FollowUpFacts {
                goal_open: goal_open(conn, task.goal_id())?,
                depth,
            })
        {
            anyhow::bail!(
                "task {task_id} is a follow_up draft a planner of the runtime's may not submit without a person: {why}. Ask with `dagq ask --task {task_id} --kind planner_question --because scope` and submit it once the answer is adopt"
            );
        }
        adoptions.push(Adoption {
            task: task_id,
            origin: Some(draft_origin),
            material,
            by_person: origin == PlannerOrigin::Person || adopted.is_some(),
            ask: adopted,
            depth,
        });
    }
    Ok(adoptions)
}

/// A draft a submission takes, as [`check_adoptions`] found it.
pub(super) struct Adoption {
    task: TaskId,
    origin: Option<DraftOrigin>,
    material: Value,
    by_person: bool,
    ask: Option<AskId>,
    depth: i64,
}

/// Record the submission's adoptions: a task a person submitted (through
/// their planner, or a planner of the runtime's with a person's `adopt`
/// answer) counts its follow-ups from 0 again; a draft of the runtime's or a
/// job's records `follow_up_adopted` (`draft_adopted` for another origin)
/// the first time it is submitted.
pub(super) fn record_adoptions(conn: &Connection, adoptions: &[Adoption]) -> Result<()> {
    for adoption in adoptions {
        let depth = if adoption.by_person {
            0
        } else {
            adoption.depth
        };
        if adoption.by_person {
            conn.execute(
                "UPDATE tasks SET follow_up_depth=0 WHERE id=?1",
                [adoption.task],
            )?;
        }
        let Some(origin) = adoption.origin else {
            continue;
        };
        let kind = match origin {
            DraftOrigin::FollowUp => event_kind::FOLLOW_UP_ADOPTED,
            DraftOrigin::GoalGap | DraftOrigin::Reopened => event_kind::DRAFT_ADOPTED,
        };
        let recorded: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_events WHERE task_id=?1 AND kind=?2)",
            params![adoption.task, kind],
            |r| r.get(0),
        )?;
        if recorded {
            continue;
        }
        event(
            conn,
            adoption.task,
            None,
            kind,
            json!({
                "task_id": adoption.task,
                "origin": origin,
                "source_task_id": adoption.material.get("source_task_id"),
                "source_run_id": adoption.material.get("source_run_id"),
                "by": if adoption.by_person { "person" } else { "planner" },
                "ask_id": adoption.ask,
                "depth": depth,
            }),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::TaskStore;
    use crate::domain::{NewAsk, NewTask};

    fn draft(queue: &mut SqliteQueue, title: &str) -> TaskId {
        queue
            .add(NewTask {
                title: title.into(),
                description: String::new(),
                acceptance: String::new(),
                verification_commands: Vec::new(),
                required_evidence: Vec::new(),
                paths: Vec::new(),
                dependencies: Vec::new(),
                goal_dependencies: Vec::new(),
                priority: Default::default(),
                kind: None,
                goal_id: None,
                context: String::new(),
            })
            .unwrap()
            .id()
    }

    fn question(queue: &mut SqliteQueue, task: TaskId, kind: AskKind) -> Ask {
        queue
            .ask(NewAsk {
                kind,
                task_id: Some(task),
                run_id: None,
                question: "q".into(),
                options: Vec::new(),
                asked_by: "planner".into(),
                reason_category: crate::domain::AskReason::Scope,
                finding_id: None,
            })
            .unwrap()
            .ask
    }

    /// A planner of the runtime's submits a follow_up draft of an open goal
    /// up to two follow-ups from a person, and is refused from three on or
    /// for a draft without a goal (ADR-t808-1).
    #[test]
    fn a_runtime_planner_submits_follow_ups_up_to_depth_two() {
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let goal = queue
            .add_goal(crate::domain::NewGoal {
                title: "g".into(),
                description: String::new(),
                acceptance: String::new(),
                constraints: String::new(),
                doc: None,
                draft: false,
            })
            .unwrap()
            .id();
        let follow_up = |queue: &mut SqliteQueue, goal_id, depth| {
            let task = queue
                .add(NewTask {
                    title: format!("depth {depth}"),
                    description: String::new(),
                    acceptance: String::new(),
                    verification_commands: Vec::new(),
                    required_evidence: Vec::new(),
                    paths: Vec::new(),
                    dependencies: Vec::new(),
                    goal_dependencies: Vec::new(),
                    priority: Default::default(),
                    kind: None,
                    goal_id,
                    context: String::new(),
                })
                .unwrap()
                .id();
            queue
                .record_draft_origin(
                    task,
                    DraftOrigin::FollowUp,
                    &json!({"source_task_id": 1, "source_run_id": "r"}),
                )
                .unwrap();
            queue.set_follow_up_depth(task, depth).unwrap();
            task
        };
        let submit = |queue: &mut SqliteQueue, task| {
            queue.submit(crate::domain::Submission {
                tasks: vec![task],
                goals: Vec::new(),
                proposal: None,
                owner: crate::domain::PlannerOwner {
                    origin: PlannerOrigin::Runtime,
                    workspace_id: None,
                },
            })
        };
        for depth in [1, 2] {
            let task = follow_up(&mut queue, Some(goal), depth);
            submit(&mut queue, task).unwrap();
            assert_eq!(queue.follow_up_depth(task).unwrap(), depth);
        }
        let deep = follow_up(&mut queue, Some(goal), 3);
        let error = submit(&mut queue, deep).unwrap_err().to_string();
        assert!(error.contains("planner_question"), "{error}");
        assert!(error.contains("3 steps"), "{error}");
        assert_eq!(
            read_task(&queue.conn, deep).unwrap().status(),
            TaskStatus::Draft
        );
        let orphan = follow_up(&mut queue, None, 1);
        let error = submit(&mut queue, orphan).unwrap_err().to_string();
        assert!(error.contains("goal is closed"), "{error}");
    }

    #[test]
    fn an_origin_is_recorded_once_for_a_draft_with_an_object() {
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let task = draft(&mut queue, "d");
        assert!(
            queue
                .record_draft_origin(task, DraftOrigin::GoalGap, &json!([1]))
                .is_err()
        );
        queue
            .record_draft_origin(task, DraftOrigin::GoalGap, &json!({"gap": 1}))
            .unwrap();
        let error = queue
            .record_draft_origin(task, DraftOrigin::FollowUp, &json!({}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("already has an origin"), "{error}");
        assert_eq!(
            queue.draft_origin(task).unwrap(),
            Some((DraftOrigin::GoalGap, json!({"gap": 1})))
        );
        queue
            .transition(task, crate::domain::TaskAction::Cancel)
            .unwrap();
        let other = draft(&mut queue, "e");
        queue
            .transition(other, crate::domain::TaskAction::Cancel)
            .unwrap();
        let error = queue
            .record_draft_origin(other, DraftOrigin::GoalGap, &json!({}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a draft"), "{error}");
        // A canceled draft waits for no planner.
        assert!(queue.planner_drafts().unwrap().is_empty());
        assert!(matches!(
            queue.open_draft_planner(task, None).unwrap(),
            DraftPlannerStart::Skipped
        ));
    }

    /// Withdrawing a proposal gives origin `reopened` to the member plan
    /// review reopened into it, with the reason and both proposals, and
    /// none to the other member; neither becomes ready (task 418).
    #[test]
    fn a_withdrawn_reopen_gives_its_task_origin_reopened() {
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let reopened = draft(&mut queue, "reopened");
        let plain = draft(&mut queue, "plain");
        assert!(
            queue
                .record_draft_origin(reopened, DraftOrigin::Reopened, &json!({}))
                .unwrap_err()
                .to_string()
                .contains("withdrawing")
        );
        let proposal = queue
            .submit(crate::domain::Submission {
                tasks: vec![reopened, plain],
                goals: Vec::new(),
                proposal: None,
                owner: crate::domain::PlannerOwner {
                    origin: PlannerOrigin::Runtime,
                    workspace_id: None,
                },
            })
            .unwrap()
            .id();
        // A reopen into another proposal is not this withdrawal's.
        for (into, reason) in [
            (proposal.as_i64() + 100, "elsewhere"),
            (proposal.as_i64(), "it must use the new API"),
        ] {
            event(
                &queue.conn,
                reopened,
                None,
                event_kind::TASK_REOPENED,
                json!({"proposal_id": into, "reviewed_proposal_id": 9, "reason": reason}),
            )
            .unwrap();
        }
        assert!(queue.planner_drafts().unwrap().is_empty());
        queue.withdraw_proposal(proposal).unwrap();
        for task in [reopened, plain] {
            assert_eq!(queue.show(task).unwrap().task.status(), TaskStatus::Draft);
        }
        let material = json!({"reason": "it must use the new API", "proposal_id": proposal, "reviewed_proposal_id": 9});
        assert_eq!(
            queue.draft_origin(reopened).unwrap(),
            Some((DraftOrigin::Reopened, material.clone()))
        );
        assert_eq!(queue.draft_origin(plain).unwrap(), None);
        // Only `draft_origins` is what stats counts as registered drafts.
        assert!(queue.draft_origins().unwrap().is_empty());
        let targets = queue.planner_drafts().unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].task.id(), reopened);
        assert_eq!(targets[0].origin, DraftOrigin::Reopened);
        assert_eq!(targets[0].material, material);

        // Submitted into another proposal and withdrawn again, it is no
        // longer the reopen's: the stale row gives it no origin.
        let later = queue
            .submit(crate::domain::Submission {
                tasks: vec![reopened],
                goals: Vec::new(),
                proposal: None,
                owner: crate::domain::PlannerOwner {
                    origin: PlannerOrigin::Person,
                    workspace_id: None,
                },
            })
            .unwrap()
            .id();
        queue.withdraw_proposal(later).unwrap();
        assert_eq!(
            queue.show(reopened).unwrap().task.status(),
            TaskStatus::Draft
        );
        assert_eq!(queue.draft_origin(reopened).unwrap(), None);
        assert!(queue.planner_drafts().unwrap().is_empty());
    }

    #[test]
    fn answers_about_what_the_runtime_does_not_plan_are_a_persons() {
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        // A person's draft: no origin, so a person delivers the answer.
        let mine = draft(&mut queue, "mine");
        let asked = question(&mut queue, mine, AskKind::PlannerQuestion);
        let answered = queue.answer(asked.id, "adopt").unwrap();
        assert_eq!(
            queue.planner_answer_route(&answered).unwrap(),
            PlannerAnswerRoute::Person
        );
        // Another kind, or an open question, is never the route's.
        let other = question(&mut queue, mine, AskKind::Decide);
        assert_eq!(
            queue.planner_answer_route(&other).unwrap(),
            PlannerAnswerRoute::Person
        );
        let runtime = draft(&mut queue, "runtime");
        queue
            .record_draft_origin(runtime, DraftOrigin::FollowUp, &json!({}))
            .unwrap();
        let open = question(&mut queue, runtime, AskKind::PlannerQuestion);
        assert_eq!(
            queue.planner_answer_route(&open).unwrap(),
            PlannerAnswerRoute::Person
        );
        assert!(
            queue
                .planner_answers()
                .unwrap()
                .iter()
                .all(|a| a.id != open.id)
        );
        assert!(queue.close_planner_answer(open.id, "x").is_err());
        let answered = queue.answer(open.id, "adopt").unwrap();
        // A planner working on the draft gets it typed.
        let DraftPlannerStart::Opened { planner, .. } =
            queue.open_draft_planner(runtime, Some(open.id)).unwrap()
        else {
            panic!("no planner opened");
        };
        assert_eq!(
            queue.planner_answer_route(&answered).unwrap(),
            PlannerAnswerRoute::Planner(queue.planner(planner.id).unwrap())
        );
        queue.close_planner_answer(open.id, "done").unwrap();
        // Closing twice keeps the first.
        queue.close_planner_answer(open.id, "again").unwrap();
    }

    #[test]
    fn an_answer_is_carried_past_the_limit_and_an_old_follow_up_answer_holds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let task = draft(&mut queue, "d");
        queue
            .record_draft_origin(task, DraftOrigin::FollowUp, &json!({}))
            .unwrap();
        // An answered ask the retired triage left does not hold it back.
        let old = question(&mut queue, task, AskKind::FollowUp);
        queue.answer(old.id, "adopt").unwrap();
        assert_eq!(queue.planner_drafts().unwrap().len(), 1);
        for _ in 0..MAX_DRAFT_PLANNERS {
            let DraftPlannerStart::Opened { planner, .. } =
                queue.open_draft_planner(task, None).unwrap()
            else {
                panic!("no planner opened");
            };
            queue.close_planner(planner.id, None).unwrap();
        }
        let asked = question(&mut queue, task, AskKind::PlannerQuestion);
        queue.answer(asked.id, "cancel").unwrap();
        assert!(matches!(
            queue.open_draft_planner(task, Some(asked.id)).unwrap(),
            DraftPlannerStart::Opened { attempt, .. } if attempt == MAX_DRAFT_PLANNERS + 1
        ));
    }
}
