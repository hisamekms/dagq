//! Append-only membership decisions and transactional membership changes.
use anyhow::{Result, anyhow, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use super::asks::{insert_ask, read_ask};
use super::sqlite::{SqliteQueue, enum_col, event, goal_event, read_goal, read_task, set_goal_in};
use crate::domain::follow_up::{
    self, CorrectionAnswer, MembershipClassification as Class, MembershipFacts, MembershipGap,
    MembershipJudgement, ReleasedDependent, SourceFollowUp, WaitingFollowUp,
    membership_gap as membership_gap_of,
};
use crate::domain::{
    Ask, AskId, AskKind, AskReason, DraftOrigin, EventKind, GoalId, GoalVerdict, NewAsk, TaskId,
    TaskStatus, goal,
};

pub(super) fn acceptance_version(conn: &Connection, goal: GoalId) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT acceptance_version FROM goals WHERE id=?1",
        [goal],
        |r| r.get(0),
    )?)
}

fn json_array_column(row: &rusqlite::Row<'_>, name: &str) -> rusqlite::Result<Value> {
    let text: String = row.get(name)?;
    let parsed: Vec<String> = serde_json::from_str(&text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            row.as_ref().column_index(name).unwrap_or(0),
            rusqlite::types::Type::Text,
            Box::new(e),
        )
    })?;
    Ok(json!(parsed))
}

pub(super) fn judgements(conn: &Connection, task: TaskId) -> Result<Vec<Value>> {
    let mut stmt =
        conn.prepare("SELECT * FROM follow_up_judgements WHERE task_id=?1 ORDER BY id")?;
    let rows = stmt.query_map([task], |r| {
        Ok(json!({
            "id": r.get::<_,i64>("id")?, "task_id": r.get::<_,i64>("task_id")?,
            "source_goal_id": r.get::<_,i64>("source_goal_id")?,
            "source_kind": r.get::<_,String>("source_kind")?,
            "classification": r.get::<_,String>("classification")?,
            "acceptance_items": json_array_column(r, "acceptance_items")?,
            "reason": r.get::<_,String>("reason")?,
            "evidence": json_array_column(r, "evidence")?,
            "destination_goal_id": r.get::<_,Option<i64>>("destination_goal_id")?,
            "acceptance_version": r.get::<_,i64>("acceptance_version")?,
            "corrects": r.get::<_,Option<i64>>("corrects")?,
            "actor_role": r.get::<_,String>("actor_role")?,
            "created_at": r.get::<_,String>("created_at")?
        }))
    })?;
    let mut result = Vec::new();
    for row in rows {
        let mut row = row?;
        let _: Class = row["classification"].as_str().unwrap().parse()?;
        let version =
            acceptance_version(conn, GoalId::new(row["source_goal_id"].as_i64().unwrap()))?;
        row["current_acceptance_version"] = json!(version);
        row["needs_recheck"] = json!(row["acceptance_version"].as_i64() != Some(version));
        result.push(row);
    }
    Ok(result)
}

/// The unclosed `correct_goal` ask about follow-up `task`, if any: while
/// it is open the follow-up's membership waits for the person
/// (ADR-t1504-2 decision 9).
fn open_correction(conn: &Connection, task: TaskId) -> Result<Option<AskId>> {
    Ok(conn
        .query_row(
            "SELECT id FROM asks WHERE kind=?1 AND task_id=?2 AND closed_at IS NULL
             ORDER BY id LIMIT 1",
            params![AskKind::CorrectGoal.as_str(), task],
            |r| r.get(0),
        )
        .optional()?)
}

fn check_no_open_correction(conn: &Connection, task: TaskId) -> Result<()> {
    if let Some(ask) = open_correction(conn, task)? {
        anyhow::bail!(
            "task {task} waits for a person's answer to correct_goal ask {ask}; its membership does not change until the supervisor applies it"
        );
    }
    Ok(())
}

pub(super) fn check_set_goal(conn: &Connection, task: TaskId, goal: Option<GoalId>) -> Result<()> {
    check_no_open_correction(conn, task)?;
    if let Some(last) = judgements(conn, task)?.last()
        && last["classification"] != "undecided"
    {
        ensure!(
            last["destination_goal_id"].as_i64() == goal.map(GoalId::as_i64),
            "task {task} has a membership judgement for a different goal; record a correction with judge-follow-up"
        );
    }
    Ok(())
}

/// The membership gap of `task` (ADR-t1504-2 decision 7), or `None` for a
/// task that is not a follow_up, has no source goal to judge by, or has a
/// current decided judgement.
pub(super) fn membership_gap(conn: &Connection, task: TaskId) -> Result<Option<MembershipGap>> {
    let material: Option<String> = conn
        .query_row(
            "SELECT material FROM draft_origins WHERE task_id=?1 AND origin=?2",
            params![task, DraftOrigin::FollowUp.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    let Some(material) = material else {
        return Ok(None);
    };
    let material: Value = serde_json::from_str(&material)?;
    let history = judgements(conn, task)?;
    let last = history.last();
    let source = material["source_goal_id"]
        .as_i64()
        .or_else(|| last.and_then(|l| l["source_goal_id"].as_i64()));
    let source_goal_abandoned = match source {
        Some(goal) => conn
            .query_row(
                "SELECT verdict='abandoned' FROM goals WHERE id=?1 AND closed_at IS NOT NULL",
                [goal],
                |r| r.get::<_, Option<bool>>(0),
            )
            .optional()?
            .flatten()
            .unwrap_or(false),
        None => false,
    };
    let latest = last
        .map(|l| {
            Ok::<_, anyhow::Error>((
                l["classification"].as_str().unwrap_or_default().parse()?,
                l["needs_recheck"].as_bool() == Some(true),
            ))
        })
        .transpose()?;
    Ok(membership_gap_of(MembershipFacts {
        source_goal_none: material["source_goal_state"] == "none",
        source_goal_abandoned,
        latest,
    }))
}

/// Refuse a submission that takes a follow_up draft (or a person's bypass
/// of a draft or submitted follow_up, `include_submitted`) without a
/// current decided membership judgement (ADR-t1504-2 decision 7).
pub(super) fn check_judged(
    conn: &Connection,
    tasks: &[TaskId],
    include_submitted: bool,
) -> Result<()> {
    let mut refused = Vec::new();
    for &task in tasks {
        match read_task(conn, task)?.status() {
            TaskStatus::Draft => {}
            TaskStatus::Submitted if include_submitted => {}
            _ => continue,
        }
        if let Some(gap) = membership_gap(conn, task)? {
            refused.push(format!("task {task}: {}", gap.explain()));
        }
    }
    ensure!(
        refused.is_empty(),
        "follow_up drafts need a current membership judgement before submit or bypass (ADR-t1504-2): {}. Record one with `dagq judge-follow-up TASK --classification required|out_of_scope ...`",
        refused.join("; ")
    );
    Ok(())
}

impl SqliteQueue {
    pub fn judge_follow_up(
        &mut self,
        task: TaskId,
        judgement: MembershipJudgement,
        role: &str,
    ) -> Result<Value> {
        ensure!(
            ["user", "inbox", "planner"].contains(&role),
            "{role} may not judge follow_up membership"
        );
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let origin: Option<(String, String)> = tx
            .query_row(
                "SELECT origin,material FROM draft_origins WHERE task_id=?1",
                [task],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (origin, material) =
            origin.ok_or_else(|| anyhow!("task {task} has no follow_up origin"))?;
        ensure!(
            origin == DraftOrigin::FollowUp.as_str(),
            "only follow_up tasks have membership judgements"
        );
        let material: Value = serde_json::from_str(&material)?;
        check_no_open_correction(&tx, task)?;
        let history = judgements(&tx, task)?;
        let last = history.last();
        let recorded_source = material["source_goal_id"].as_i64().map(GoalId::new);
        let source = recorded_source.or_else(|| last.and_then(|l| l["source_goal_id"].as_i64()).map(GoalId::new)).or(judgement.source_goal_id)
            .ok_or_else(|| anyhow!("source goal is unknown; name --source-goal with evidence (a known absent goal cannot be judged)"))?;
        ensure!(
            material["source_goal_state"] != "none",
            "a follow_up registered without a source goal has no acceptance to judge"
        );
        ensure!(
            judgement.source_goal_id.is_none_or(|g| g == source),
            "source goal does not match its immutable origin or previous judgement"
        );
        if recorded_source.is_none() {
            ensure!(
                !judgement.evidence.is_empty(),
                "naming an unknown source goal requires evidence"
            );
        }
        let previous = last
            .map(|l| {
                Ok::<_, anyhow::Error>((
                    l["id"].as_i64().unwrap(),
                    l["classification"].as_str().unwrap().parse()?,
                ))
            })
            .transpose()?;
        judgement
            .validate(previous, source)
            .map_err(anyhow::Error::msg)?;
        let source_goal = read_goal(&tx, source)?;
        let destination = match judgement.classification {
            Class::Required => Some(source),
            Class::OutOfScope => judgement.destination_goal_id,
            Class::Undecided => judgement.destination_goal_id,
        };
        if let Some(goal) = destination {
            let goal = read_goal(&tx, goal)?;
            if judgement.classification == Class::OutOfScope {
                crate::domain::goal::check_accepts_tasks(&goal)?;
            }
        }
        let task_row = read_task(&tx, task)?;
        if judgement.classification != Class::Undecided
            && matches!(task_row.status(), TaskStatus::Draft | TaskStatus::Ready)
            && !(judgement.classification == Class::Required && source_goal.is_closed())
            && destination != task_row.goal_id()
        {
            set_goal_in(&tx, task, destination, &self.generators.clock.timestamp())?;
        }
        let version = acceptance_version(&tx, source)?;
        let source_kind = if recorded_source.is_none() {
            "named_by_judge"
        } else {
            material["source_goal_provenance"]
                .as_str()
                .unwrap_or("recorded")
        };
        tx.execute("INSERT INTO follow_up_judgements(task_id,source_goal_id,source_kind,classification,acceptance_items,reason,evidence,destination_goal_id,acceptance_version,corrects,actor_role,created_at)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)", params![task,source,source_kind,judgement.classification.as_str(),serde_json::to_string(&judgement.acceptance_items)?,judgement.reason,serde_json::to_string(&judgement.evidence)?,destination,version,judgement.corrects,role,self.generators.clock.timestamp()])?;
        let mut row = judgements(&tx, task)?.pop().unwrap();
        // A required judgement after an achieved close keeps the close and
        // its verdict, and asks a person (ADR-t1504-2 decision 9).
        if follow_up::opens_correction(
            judgement.classification,
            previous.map(|(_, class)| class),
            source_goal.verdict(),
        ) {
            let judgement_id = row["id"].as_i64().unwrap();
            let question = follow_up::correction_question(
                source,
                task,
                judgement_id,
                &judgement,
                &released_dependents(&tx, source)?,
            );
            let opened = insert_ask(
                &tx,
                &NewAsk {
                    kind: AskKind::CorrectGoal,
                    task_id: Some(task),
                    run_id: None,
                    question,
                    options: follow_up::CORRECTION_OPTIONS
                        .iter()
                        .map(|o| (*o).to_owned())
                        .collect(),
                    asked_by: follow_up::CORRECTION_ASKER.to_owned(),
                    reason_category: AskReason::Scope,
                    topics: Vec::new(),
                    recommendation: None,
                    confidence: None,
                    finding_id: None,
                    request_id: None,
                },
            )?;
            row["correction_ask_id"] = json!(opened.ask.id);
        }
        event(&tx, task, None, EventKind::FollowUpJudged, row.clone())?;
        goal_event(&tx, source, EventKind::FollowUpJudged, row.clone())?;
        tx.commit()?;
        Ok(row)
    }
}

/// The tasks that wait on `goal` (`task_goal_dependencies`), each with its
/// runs: released when the goal closed as achieved, in ID order.
fn released_dependents(conn: &Connection, goal: GoalId) -> Result<Vec<ReleasedDependent>> {
    let tasks = conn
        .prepare(
            "SELECT t.id, t.title, t.status FROM task_goal_dependencies d
             JOIN tasks t ON t.id = d.task_id WHERE d.goal_id=?1 ORDER BY t.id",
        )?
        .query_map([goal], |r| {
            Ok((
                r.get::<_, TaskId>(0)?,
                r.get::<_, String>(1)?,
                enum_col::<TaskStatus>(r, "status")?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    tasks
        .into_iter()
        .map(|(task, title, status)| {
            let runs = conn
                .prepare("SELECT id, status FROM task_runs WHERE task_id=?1 ORDER BY rowid")?
                .query_map([task], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(ReleasedDependent {
                task,
                title,
                status,
                runs,
            })
        })
        .collect()
}

/// The judgement a `correct_goal` ask was opened for: its row's ID and
/// source goal, from the `follow_up_judged` event that names the ask.
fn correction_of(conn: &Connection, ask: &Ask) -> Result<Option<(i64, GoalId)>> {
    if ask.kind != AskKind::CorrectGoal {
        return Ok(None);
    }
    let Some(task) = ask.task_id else {
        return Ok(None);
    };
    Ok(conn
        .query_row(
            "SELECT json_extract(payload,'$.id'), json_extract(payload,'$.source_goal_id')
             FROM run_events WHERE kind=?1 AND task_id=?2
               AND json_extract(payload,'$.correction_ask_id')=?3
             ORDER BY id DESC LIMIT 1",
            params![EventKind::FollowUpJudged.as_str(), task, ask.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// Whether the goal is still closed as achieved, so a correction of it
/// still has something to settle.
fn still_achieved(conn: &Connection, goal: GoalId) -> Result<bool> {
    Ok(read_goal(conn, goal)?.verdict() == Some(GoalVerdict::Achieved))
}

/// What the answer `text` of `ask` does when the supervisor applies it
/// now: `None` when it is no option, the ask names no judgement, or the
/// goal is no longer closed as achieved.
fn correction_applicable(
    conn: &Connection,
    ask: &Ask,
    text: &str,
) -> Result<Option<(CorrectionAnswer, i64, GoalId)>> {
    let Some(answer) = CorrectionAnswer::parse(text) else {
        return Ok(None);
    };
    let Some((judgement, goal)) = correction_of(conn, ask)? else {
        return Ok(None);
    };
    Ok(still_achieved(conn, goal)?.then_some((answer, judgement, goal)))
}

pub(super) fn correction_answer_applies(conn: &Connection, ask: &Ask, text: &str) -> Result<bool> {
    Ok(ask.closed_at.is_none() && correction_applicable(conn, ask, text)?.is_some())
}

/// Answered `correct_goal` asks nobody closed whose answer the runtime
/// took to apply when it was given (`runtime_delivers`), oldest first.
pub(super) fn correction_answers(conn: &Connection) -> Result<Vec<Ask>> {
    let ids: Vec<AskId> = conn
        .prepare(
            "SELECT a.id FROM asks a WHERE a.kind=?1 AND a.answered_at IS NOT NULL
                 AND a.closed_at IS NULL AND (SELECT json_extract(e.payload, '$.runtime_delivers')
                     FROM run_events e WHERE e.kind='ask_answered'
                     AND json_extract(e.payload, '$.ask_id') = a.id
                     ORDER BY e.id DESC LIMIT 1) = 1
                 ORDER BY a.id",
        )?
        .query_map([AskKind::CorrectGoal.as_str()], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ids.into_iter().map(|id| read_ask(conn, id)).collect()
}

/// Apply a person's answer to a `correct_goal` ask in one transaction
/// and close it (ADR-t1504-2 decision 9): `reopen` opens the goal again
/// (`goal_reopened`; its `goal_closed` stays) and moves a draft or
/// ready follow-up back into it; `correct_verdict` and `keep_achieved`
/// leave the goal closed. Each records `goal_correction_decided` on the
/// goal and the follow-up; no running task is stopped and no event is
/// rewritten. `None` when the answer is not one the runtime applies now
/// (left for the inbox); a goal no longer closed as achieved closes the
/// ask without a change.
pub(super) fn decide_correction(
    conn: &mut Connection,
    ask_id: AskId,
    stamp: String,
    now: i64,
) -> Result<Option<Value>> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let ask = read_ask(&tx, ask_id)?;
    ensure!(
        ask.kind == AskKind::CorrectGoal,
        "ask {ask_id} is not a correct_goal ask"
    );
    if ask.closed_at.is_some() {
        return Ok(None);
    }
    let close = |tx: &Connection| -> Result<()> {
        tx.execute(
            "UPDATE asks SET closed_at=?2 WHERE id=?1",
            params![ask_id, now],
        )?;
        Ok(())
    };
    if let Some((_, goal)) = correction_of(&tx, &ask)?
        && !still_achieved(&tx, goal)?
    {
        // Reopened by another answer meanwhile: nothing left to settle.
        close(&tx)?;
        super::asks::record_ask_closed(&tx, &ask)?;
        tx.commit()?;
        return Ok(None);
    }
    let text = ask.answer.clone().unwrap_or_default();
    let Some((answer, judgement, goal_id)) = correction_applicable(&tx, &ask, &text)? else {
        return Ok(None);
    };
    let task = ask
        .task_id
        .ok_or_else(|| anyhow!("ask {ask_id} names no task"))?;
    let mut moved = false;
    let mut move_refused = None;
    let mut closed_asks = Vec::new();
    if answer == CorrectionAnswer::Reopen {
        let closed = read_goal(&tx, goal_id)?;
        let previous_closed_at = closed.closed_at().map(str::to_owned);
        let reopened = goal::reopen(closed, stamp.clone())?;
        tx.execute(
            "UPDATE goals SET status=?2, closed_at=NULL, verdict=NULL, updated_at=?3 WHERE id=?1",
            params![goal_id, reopened.status().as_str(), reopened.updated_at()],
        )?;
        goal_event(
            &tx,
            goal_id,
            EventKind::GoalReopened,
            json!({"by": "person", "ask_id": ask_id, "task_id": task,
                    "judgement_id": judgement, "previous_verdict": GoalVerdict::Achieved,
                    "previous_closed_at": previous_closed_at}),
        )?;
        let follow_up = read_task(&tx, task)?;
        if matches!(follow_up.status(), TaskStatus::Draft | TaskStatus::Ready)
            && follow_up.goal_id() != Some(goal_id)
        {
            // `set_goal_in` checks before it writes: a move the goal's
            // dependencies refuse (the follow-up waits on the goal) leaves
            // the follow-up where it is, and the goal waits for it as a
            // required one outside, instead of failing the answer.
            match set_goal_in(&tx, task, Some(goal_id), &stamp) {
                Ok(_) => moved = true,
                Err(error) => move_refused = Some(format!("{error:#}")),
            }
        }
        // The goal's other open questions were asked of the closed goal:
        // the other `correct_goal` asks about it and an `approve_goal` ask
        // left from before its close. The runtime answers and closes them,
        // so they neither hold their follow-ups nor apply to the open goal.
        let reason = format!("goal {goal_id} was reopened by the answer to ask {ask_id}");
        let mut stale = Vec::new();
        for other in tx
            .prepare("SELECT id FROM asks WHERE kind=?1 AND closed_at IS NULL AND id != ?2")?
            .query_map(params![AskKind::CorrectGoal.as_str(), ask_id], |r| {
                r.get::<_, AskId>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        {
            let other = read_ask(&tx, other)?;
            if correction_of(&tx, &other)?.is_some_and(|(_, goal)| goal == goal_id) {
                stale.push(other);
            }
        }
        for other in tx
            .prepare(
                "SELECT a.id FROM goal_reviews r JOIN asks a ON a.id = r.ask_id
                 WHERE r.goal_id=?1 AND a.closed_at IS NULL",
            )?
            .query_map([goal_id], |r| r.get::<_, AskId>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
        {
            stale.push(read_ask(&tx, other)?);
        }
        for other in &stale {
            super::asks::close_by_runtime(&tx, other, &reason, now)?;
            closed_asks.push(other.id);
        }
    }
    close(&tx)?;
    // The answer applied: `ask_closed`, as `ask close` records it.
    super::asks::record_ask_closed(&tx, &ask)?;
    let decided = json!({
        "ask_id": ask_id,
        "goal_id": goal_id,
        "task_id": task,
        "judgement_id": judgement,
        "answer": text.trim(),
        "decision": answer.as_str(),
        "reopened": answer == CorrectionAnswer::Reopen,
        "moved": moved,
        "move_refused": move_refused,
        "closed_asks": closed_asks,
    });
    goal_event(
        &tx,
        goal_id,
        EventKind::GoalCorrectionDecided,
        decided.clone(),
    )?;
    event(
        &tx,
        task,
        None,
        EventKind::GoalCorrectionDecided,
        decided.clone(),
    )?;
    tx.commit()?;
    Ok(Some(decided))
}

pub(super) fn goal_memberships(conn: &Connection, goal: GoalId) -> Result<Vec<Value>> {
    let ids = conn.prepare("SELECT o.task_id FROM draft_origins o JOIN tasks t ON t.id=o.task_id
        WHERE o.origin='follow_up' AND (json_extract(o.material,'$.source_goal_id')=?1 OR t.goal_id=?1
        OR EXISTS(SELECT 1 FROM follow_up_judgements j WHERE j.task_id=o.task_id AND j.source_goal_id=?1)) ORDER BY o.task_id")?
        .query_map([goal], |r| r.get::<_,TaskId>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    ids.into_iter()
        .map(|id| {
            let task = read_task(conn, id)?;
            let material: String = conn.query_row(
                "SELECT material FROM draft_origins WHERE task_id=?1",
                [id],
                |r| r.get(0),
            )?;
            let depth: i64 =
                conn.query_row("SELECT follow_up_depth FROM tasks WHERE id=?1", [id], |r| {
                    r.get(0)
                })?;
            Ok(
                json!({"task_id": id, "goal_id": task.goal_id(), "status": task.status(),
            "follow_up_depth": depth, "material": serde_json::from_str::<Value>(&material)?,
            "judgements": judgements(conn,id)?}),
            )
        })
        .collect()
}

/// The follow_ups whose source goal is `goal` as recorded or restored at
/// registration (one whose source is unknown enters no goal's check, even
/// when its judge named one: ADR-t1504-2 decision 12(ii)), wherever
/// they belong now, each with its current judgement against the goal's
/// acceptance version (ADR-t1504-2 decision 8). Read inside the caller's
/// transaction, so a review and a close check the queue as it is.
pub(super) fn source_follow_ups(conn: &Connection, goal: GoalId) -> Result<Vec<SourceFollowUp>> {
    let version = acceptance_version(conn, goal)?;
    let rows = conn
        .prepare(
            "SELECT o.task_id, t.status, t.goal_id IS ?1, j.id, j.classification, j.acceptance_version
             FROM draft_origins o JOIN tasks t ON t.id = o.task_id
             LEFT JOIN follow_up_judgements j ON j.id =
                 (SELECT max(id) FROM follow_up_judgements WHERE task_id = o.task_id)
             WHERE o.origin = 'follow_up'
               AND json_extract(o.material, '$.source_goal_id') = ?1
             ORDER BY o.task_id",
        )?
        .query_map([goal], |r| {
            Ok((
                r.get::<_, TaskId>(0)?,
                enum_col::<TaskStatus>(r, "status")?,
                r.get::<_, bool>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<i64>>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(task, status, in_goal, id, class, judged)| {
            let judgement = match (id, class, judged) {
                (Some(id), Some(class), Some(judged)) => {
                    Some((id, class.parse::<Class>()?, judged != version))
                }
                _ => None,
            };
            Ok(SourceFollowUp {
                task,
                status,
                in_goal,
                judgement,
            })
        })
        .collect()
}

/// The source follow-ups of `goal` with no settled membership, each with
/// what already shows or handles it (task 1660): a planner of the runtime's
/// for the draft, its `draft_planner_exhausted`, an ask about it not closed.
pub(super) fn waiting_follow_ups(conn: &Connection, goal: GoalId) -> Result<Vec<WaitingFollowUp>> {
    source_follow_ups(conn, goal)?
        .into_iter()
        .filter(|f| f.unsettled_reason().is_some())
        .map(|follow_up| {
            let task = follow_up.task;
            let (exhausted, open_ask) = conn.query_row(
                &format!(
                    "SELECT EXISTS(SELECT 1 FROM run_events WHERE task_id=?1 AND kind='{}'),
                            EXISTS(SELECT 1 FROM asks WHERE task_id=?1 AND closed_at IS NULL)",
                    crate::domain::event_kind::DRAFT_PLANNER_EXHAUSTED
                ),
                [task],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok(WaitingFollowUp {
                draft_planned: super::draft_planners::draft_planned(conn, task)?,
                exhausted,
                open_ask,
                follow_up,
            })
        })
        .collect()
}
