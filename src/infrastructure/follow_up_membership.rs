//! Append-only membership decisions and transactional membership changes.
use anyhow::{Result, anyhow, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use super::sqlite::{SqliteQueue, enum_col, event, goal_event, read_goal, read_task, set_goal_in};
use crate::domain::follow_up::{
    MembershipClassification as Class, MembershipFacts, MembershipGap, MembershipJudgement,
    SourceFollowUp, membership_gap as membership_gap_of,
};
use crate::domain::{DraftOrigin, EventKind, GoalId, TaskId, TaskStatus};

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

pub(super) fn check_set_goal(conn: &Connection, task: TaskId, goal: Option<GoalId>) -> Result<()> {
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
        let row = judgements(&tx, task)?.pop().unwrap();
        event(&tx, task, None, EventKind::FollowUpJudged, row.clone())?;
        goal_event(&tx, source, EventKind::FollowUpJudged, row.clone())?;
        tx.commit()?;
        Ok(row)
    }
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
