//! Plan review (ADR-0041 decisions 11-15, 17): the `plan_reviews` rows of
//! the headless job (one unfinished at a time, queue-wide), the plan-review
//! columns of `proposals` (a hold, and the revise on its way to a planner),
//! and the verdicts and `approve_plan` answers the supervisor applies, each
//! in one transaction. Events go to the first task of the proposal, since
//! an event needs a task or a goal.
use crate::domain::EventKind;
use crate::domain::actor_model::{ActorLaunch, LIVE_PLANNER_NOT_RAISED};
use crate::domain::write_rules::check_at_least;
use crate::domain::{LeaseToken, event_kind};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::path::Path;
use tracing::warn;

use super::{
    asks::{insert_ask, read_ask, record_ask_closed},
    proposals, sessions,
    sqlite::{
        SqliteQueue, cancel_as_duplicate, change_priority, check_duplicate, enum_col, event,
        insert_dependency, read_task, transition_task,
    },
};
use crate::{
    application::{
        PlanDecided, PlanReviewApplied, PlanReviewApply, PlanReviewFailure, PlanReviewHold,
        PlanReviewJob, PlanReviewStore, PlannerHold, ReopenedTask, RevisingProposal,
    },
    domain::{
        Ask, AskId, AskKind, HEARTBEAT_TIMEOUT_SECS, PlanAnswer, PlanReviewAction,
        PlanReviewCandidate, PlanReviewDecision, PlannerId, Priority, ProposalId, ProposalStatus,
        TaskAction, TaskId, TaskStatus,
        headless_job::JobSession,
        plan_quality::{self, ProposalFeatures},
        plan_review::{self, PassPriority, PlanReviewEnd, PlanReviewOutcome, ReviewHold},
        prediction, proposal, review_reason,
        sessions::SESSION_CLOSED,
    },
};

/// The first task of a proposal, where its plan review events go.
fn anchor(conn: &Connection, proposal_id: ProposalId) -> Result<TaskId> {
    conn.query_row(
        "SELECT min(id) FROM tasks WHERE proposal_id=?1",
        [proposal_id],
        |r| r.get::<_, Option<TaskId>>(0),
    )?
    .with_context(|| format!("proposal {proposal_id} has no task"))
}

fn hold(conn: &Connection, proposal_id: ProposalId) -> Result<Option<ReviewHold>> {
    let hold: Option<String> = conn.query_row(
        "SELECT review_hold FROM proposals WHERE id=?1",
        [proposal_id],
        |r| r.get(0),
    )?;
    Ok(hold.map(|hold| hold.parse()).transpose()?)
}

fn set_hold(conn: &Connection, proposal_id: ProposalId, hold: Option<ReviewHold>) -> Result<()> {
    conn.execute(
        "UPDATE proposals SET review_hold=?2 WHERE id=?1",
        params![proposal_id, hold.map(ReviewHold::as_str)],
    )?;
    Ok(())
}

/// The revise `reasons` wait for the supervisor to deliver them, from
/// `now` on.
fn await_delivery(
    conn: &Connection,
    proposal_id: ProposalId,
    reasons: &[String],
    now: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE proposals SET revise_reasons=?2, revised_at=?3, revise_sent_at=NULL,
             revise_planner_id=NULL, unresponsive_at=NULL, review_hold=NULL WHERE id=?1",
        params![proposal_id, serde_json::to_string(reasons)?, now],
    )?;
    Ok(())
}

/// The job's row, while it is unfinished and still `token`'s.
fn running(conn: &Connection, job: &PlanReviewJob, token: &LeaseToken) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM plan_reviews WHERE id=?1 AND supervisor_token=?2
         AND finished_at IS NULL)",
        params![job.id, token],
        |r| r.get(0),
    )?)
}

fn finish_row(
    conn: &Connection,
    id: i64,
    now: i64,
    outcome: PlanReviewOutcome,
    verdict: Option<&Value>,
    error: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE plan_reviews SET finished_at=?2, outcome=?3, verdict=?4, error=?5
         WHERE id=?1 AND finished_at IS NULL",
        params![
            id,
            now,
            outcome.as_str(),
            verdict.map(Value::to_string),
            error
        ],
    )?;
    Ok(())
}

/// Whether the proposal is still the submitted, unheld one the job took.
fn reviewable(conn: &Connection, proposal_id: ProposalId) -> Result<bool> {
    Ok(
        proposals::read(conn, proposal_id)?.status() == ProposalStatus::Submitted
            && hold(conn, proposal_id)?.is_none(),
    )
}

/// The tasks of the proposal edited since the job started (ADR-0041
/// decision 9), by [`plan_review::edited_during`]: its verdict is of their
/// old contents. Event ids order the edits against the job's
/// `plan_review_started`.
fn edited_during(conn: &Connection, job: &PlanReviewJob) -> Result<Vec<TaskId>> {
    let started: Option<i64> = conn
        .query_row(
            &format!(
                "SELECT id FROM run_events WHERE task_id=?1 AND kind='{}'
                 AND json_extract(payload,'$.plan_review_id')=?2",
                event_kind::PLAN_REVIEW_STARTED
            ),
            params![job.anchor, job.id],
            |r| r.get(0),
        )
        .optional()?;
    let edits: Vec<(i64, TaskId)> = conn
        .prepare(&format!(
            "SELECT e.id, e.task_id FROM run_events e JOIN tasks t ON t.id = e.task_id
             WHERE e.kind='{}' AND t.proposal_id=?1",
            event_kind::TASK_EDITED
        ))?
        .query_map([job.proposal_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(plan_review::edited_during(started, &edits))
}

/// Take the end of a job that is not applied ([`plan_review::plan_review_end`]):
/// a proposal that moved on interrupts the job's row; tasks edited during
/// it interrupt it too, leaving the proposal for the next pass to review
/// again. An edit records `plan_review_discarded`, and so does a proposal
/// that moved on when the job's agent ran; it has the Execution
/// (ADR-t1486-1) of an agent that ran (`ran`: what its output named).
/// `verdict` is the
/// job's verdict, as recorded and as decided, `failure` its error when it
/// failed. `false` when the end is to be applied.
fn end_unapplied(
    conn: &Connection,
    job: &PlanReviewJob,
    now: i64,
    verdict: Option<(&Value, PlanReviewDecision)>,
    failure: Option<&str>,
    ran: Option<Option<&JobSession>>,
) -> Result<bool> {
    let reviewable = reviewable(conn, job.proposal_id)?;
    let edited = if reviewable {
        edited_during(conn, job)?
    } else {
        Vec::new()
    };
    let recorded = verdict.map(|(recorded, _)| recorded);
    let (edited, error) = match plan_review::plan_review_end(reviewable, edited, failure) {
        PlanReviewEnd::Ends => return Ok(false),
        PlanReviewEnd::MovedOn { error } => (None, error),
        PlanReviewEnd::Discarded { edited, error } => (Some(edited), error),
    };
    finish_row(
        conn,
        job.id,
        now,
        PlanReviewOutcome::Interrupted,
        recorded,
        Some(&error),
    )?;
    sessions::close_plan_review(conn, job.id, false)?;
    if edited.is_none() && ran.is_none() {
        return Ok(true);
    }
    let mut discarded = json!({
        "proposal_id": job.proposal_id,
        "plan_review_id": job.id,
        "attempt": job.attempt,
        "verdict": verdict.map(|(_, decided)| decided),
        "edited": edited.unwrap_or_default(),
        "error": error,
    });
    if let Some(session) = ran {
        JobSession::record_execution(session, &mut discarded);
    }
    event(
        conn,
        job.anchor,
        None,
        EventKind::PlanReviewDiscarded,
        discarded,
    )?;
    Ok(true)
}

/// Check an action against the proposal before anything is applied
/// ([`plan_review::check_action`]): it changes a submitted task of the
/// proposal, a dependency names another task that exists, a priority only
/// goes down, and a duplicate is of another task that is not canceled.
fn check_action(conn: &Connection, members: &[TaskId], action: &PlanReviewAction) -> Result<()> {
    let task_id = action.task_id();
    let target = if members.contains(&task_id) {
        let task = read_task(conn, task_id)?;
        Some((task.status(), task.priority()))
    } else {
        None
    };
    plan_review::check_action(members, action, target).map_err(anyhow::Error::msg)?;
    match action {
        PlanReviewAction::AddDependency { depends_on, .. } => {
            read_task(conn, *depends_on)?;
        }
        PlanReviewAction::LowerPriority { .. } => {}
        PlanReviewAction::CancelDuplicate { duplicate_of, .. } => {
            check_duplicate(conn, task_id, *duplicate_of)?;
        }
    }
    Ok(())
}

fn apply_action(conn: &Connection, action: &PlanReviewAction, now: &str) -> Result<()> {
    match action {
        PlanReviewAction::AddDependency {
            task_id,
            depends_on,
        } => insert_dependency(conn, *task_id, *depends_on, now),
        // An AI's task takes its goal's priority instead of a lower one of
        // its own (ADR-t1971-1 decision 4).
        PlanReviewAction::LowerPriority { task_id, .. } => inherit_priority(conn, *task_id, now),
        PlanReviewAction::CancelDuplicate {
            task_id,
            duplicate_of,
        } => {
            cancel_as_duplicate(conn, *task_id, *duplicate_of, Some("plan_review"), now)?;
            Ok(())
        }
    }
}

/// How a pass sees the task's priority: by what the queue records of the
/// task and its goal (ADR-t1975-1 decision 7).
fn pass_priority(conn: &Connection, task_id: TaskId) -> Result<PassPriority> {
    Ok(PassPriority::of(&read_task(conn, task_id)?))
}

/// A pass's own priority taken off the task for its goal's, the way
/// `set-priority --inherit` does, marked as the plan review's.
fn inherit_priority(conn: &Connection, task_id: TaskId, now: &str) -> Result<()> {
    change_priority(conn, task_id, None, now, Some("plan_review"))
}

/// Take the ready task out of the claim into a proposal of its own that
/// waits for a planner with `reason` (ADR-0041 decision 14). A task that is
/// not ready, or belongs to an active proposal, is skipped with why.
fn reopen(
    conn: &Connection,
    task_id: TaskId,
    reason: &str,
    reviewed: ProposalId,
    now: &str,
    at: i64,
) -> Result<std::result::Result<ReopenedTask, String>> {
    let found = conn
        .query_row(
            "SELECT t.status, p.id, p.status FROM tasks t
             LEFT JOIN proposals p ON p.id = t.proposal_id WHERE t.id=?1",
            [task_id],
            |r| {
                let proposal: Option<ProposalId> = r.get(1)?;
                let active = match r.get::<_, Option<String>>(2)? {
                    Some(status) => status
                        .parse::<ProposalStatus>()
                        .map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                2,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?
                        .is_active(),
                    None => false,
                };
                Ok((
                    enum_col::<TaskStatus>(r, "status")?,
                    proposal.filter(|_| active),
                ))
            },
        )
        .optional()?;
    if let Some(why) = plan_review::reopen_refusal(task_id, found) {
        return Ok(Err(why));
    }
    let id = ProposalId::new(super::sqlite::next_id(conn, "proposals")?);
    let reopened = proposal::reopen(id, vec![task_id], now.into())?;
    proposals::save(conn, &reopened)?;
    transition_task(conn, task_id, TaskAction::Reopen, now)?;
    conn.execute(
        "UPDATE tasks SET proposal_id=?1 WHERE id=?2",
        params![id, task_id],
    )?;
    await_delivery(
        conn,
        id,
        &[plan_review::reopen_reason(reviewed, task_id, reason)],
        at,
    )?;
    event(
        conn,
        task_id,
        None,
        EventKind::TaskReopened,
        json!({"proposal_id": id, "reviewed_proposal_id": reviewed, "reason": reason}),
    )?;
    Ok(Ok(ReopenedTask {
        task_id,
        proposal_id: id,
    }))
}

/// Whether the answer `text` of `ask` is one the supervisor applies
/// ([`plan_review::plan_answer_applies`]).
pub(super) fn plan_answer_applies(conn: &Connection, ask: &Ask, text: &str) -> Result<bool> {
    // The proposal's status and hold are read only for an `approve_plan`
    // ask; any other kind does not apply whatever they are.
    let proposal = match ask_proposal(conn, ask)? {
        Some(proposal_id) if ask.kind == AskKind::ApprovePlan => Some((
            proposals::read(conn, proposal_id)?.status(),
            hold(conn, proposal_id)?,
        )),
        _ => None,
    };
    Ok(plan_review::plan_answer_applies(&ask.kind, text, proposal))
}

fn ask_proposal(conn: &Connection, ask: &Ask) -> Result<Option<ProposalId>> {
    let Some(task_id) = ask.task_id else {
        return Ok(None);
    };
    Ok(conn.query_row(
        "SELECT proposal_id FROM tasks WHERE id=?1",
        [task_id],
        |r| r.get(0),
    )?)
}

/// How many follow-ups deep a draft is: 0 for none, 1 for a follow-up of a
/// task that is none, and so on; bounded so a cycle ends.
fn follow_up_depth(conn: &Connection, task_id: TaskId) -> Result<i64> {
    let mut depth = 0;
    let mut task = task_id.as_i64();
    while depth < 32 {
        let source: Option<Option<i64>> = conn
            .query_row(
                "SELECT json_extract(material, '$.source_task_id') FROM draft_origins
                 WHERE task_id=?1 AND origin='follow_up'",
                [task],
                |r| r.get(0),
            )
            .optional()?;
        let Some(source) = source else {
            break;
        };
        depth += 1;
        let Some(source) = source else {
            break;
        };
        task = source;
    }
    Ok(depth)
}

impl SqliteQueue {
    /// What `proposal_id` is like now (ADR-0079 decision 7): where it came
    /// from, how deep its follow-ups go, how close its closest existing
    /// task is (`dagq related`), and how often plan review sent it back.
    /// `None` when it does not exist.
    fn proposal_features(&self, proposal_id: ProposalId) -> Result<Option<ProposalFeatures>> {
        let exists: bool = self.conn.query_row(
            "SELECT count(*) > 0 FROM proposals WHERE id=?1",
            [proposal_id],
            |r| r.get(0),
        )?;
        if !exists {
            return Ok(None);
        }
        let proposal = proposals::read(&self.conn, proposal_id)?;
        let members = proposal.task_ids();
        let has = |sql: &str| -> Result<bool> {
            Ok(self.conn.query_row(sql, [proposal_id], |r| r.get(0))?)
        };
        let origin = plan_quality::origin(
            has(
                "SELECT count(*) > 0 FROM draft_origins d JOIN tasks t ON t.id = d.task_id
                 WHERE t.proposal_id=?1 AND d.origin='follow_up'",
            )?,
            has(
                "SELECT count(*) > 0 FROM draft_origins d JOIN tasks t ON t.id = d.task_id
                 WHERE t.proposal_id=?1 AND d.origin='goal_gap'",
            )?,
            has("SELECT count(*) > 0 FROM findings WHERE proposal_id=?1")?,
            proposal.owner().origin,
        );
        let mut follow_up = 0;
        let mut related: Option<f64> = None;
        for &task in members {
            follow_up = follow_up.max(follow_up_depth(&self.conn, task)?);
            let page = self.related(task.as_i64(), &[], members.len() + 1)?;
            let best = page
                .related
                .iter()
                .find(|other| !members.contains(&TaskId::new(other.id)))
                .map(|other| other.score);
            if let Some(best) = best {
                related = Some(related.map_or(best, |kept| kept.max(best)));
            }
        }
        Ok(Some(ProposalFeatures {
            origin,
            follow_up_depth: follow_up,
            related_score: related,
            revise_count: i64::from(proposal.revise_count()),
        }))
    }
}

impl PlanReviewStore for SqliteQueue {
    fn interrupt_plan_reviews_for_handoff(&mut self, token: &LeaseToken) -> Result<()> {
        let now = self.generators.clock.now();
        // Read transcripts before taking the write lock, as in begin/finish.
        let _read = sessions::read_before(&self.conn, sessions::Closing::PlanReviews(None, false))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let unfinished: Vec<i64> = tx
            .prepare(
                "SELECT id FROM plan_reviews WHERE supervisor_token=?1 AND finished_at IS NULL",
            )?
            .query_map([token], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in unfinished {
            finish_row(
                &tx,
                id,
                now,
                PlanReviewOutcome::Interrupted,
                None,
                Some("stopped for the supervisor handoff"),
            )?;
            sessions::close_plan_review(&tx, id, false)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn plan_review_candidates(&self) -> Result<Vec<PlanReviewCandidate>> {
        candidates(&self.conn)
    }

    fn begin_plan_review(
        &mut self,
        proposal_id: ProposalId,
        token: &LeaseToken,
        plan_reviews_dir: &Path,
        cwd: &Path,
        launch: &crate::domain::actor_model::ActorLaunch,
    ) -> Result<Option<PlanReviewJob>> {
        let now = self.generators.clock.now();
        // What the proposal is like, read before the write lock (task
        // 579); a failure leaves it unrecorded, not the review undone.
        let features = match self.proposal_features(proposal_id) {
            Ok(features) => features.map(|features| features.payload()),
            Err(error) => {
                warn!("proposal {proposal_id}: features not recorded: {error:#}");
                None
            }
        };
        // The spans it closes read their transcripts first (task 543).
        let _read = sessions::read_before(&self.conn, sessions::Closing::PlanReviews(None, true))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let unfinished: Vec<(i64, LeaseToken, Option<i64>)> = tx
            .prepare(
                "SELECT r.id, r.supervisor_token, s.heartbeat_at FROM plan_reviews r
                 LEFT JOIN supervisors s ON s.token = r.supervisor_token
                 WHERE r.finished_at IS NULL",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, owner, heartbeat) in unfinished {
            let live =
                owner != *token && heartbeat.is_some_and(|at| now - at <= HEARTBEAT_TIMEOUT_SECS);
            if live {
                return Ok(None);
            }
            finish_row(
                &tx,
                id,
                now,
                PlanReviewOutcome::Interrupted,
                None,
                Some("its supervisor is gone"),
            )?;
            sessions::close_plan_review(&tx, id, true)?;
        }
        let candidate = candidates(&tx)?;
        if !candidate.iter().any(|c| c.proposal_id == proposal_id) {
            // Keep the rows finished above.
            tx.commit()?;
            return Ok(None);
        }
        let attempt: usize = tx.query_row(
            "SELECT count(*) + 1 FROM plan_reviews WHERE proposal_id=?1 AND outcome IS NOT 'interrupted'",
            [proposal_id],
            |r| r.get::<_, i64>(0),
        )? as usize;
        check_at_least("plan review attempt", attempt as i64, 1)?;
        tx.execute(
            "INSERT INTO plan_reviews(proposal_id, attempt, supervisor_token, started_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![proposal_id, attempt as i64, token, now],
        )?;
        let id = tx.last_insert_rowid();
        let dir = plan_reviews_dir.join(id.to_string());
        let dir_text = dir.to_str().context("plan review directory is not UTF-8")?;
        tx.execute(
            "UPDATE plan_reviews SET dir=?2 WHERE id=?1",
            params![id, dir_text],
        )?;
        let anchor = anchor(&tx, proposal_id)?;
        // The job's Claude session id, given to it by the runtime (ADR-0048
        // decision 4); Codex names its thread itself, which the job's end
        // records (ADR-t1063-1 decision 6).
        let session_id = (launch.provider == crate::domain::Provider::Claude)
            .then(|| self.generators.ids.uuid());
        let cwd = cwd.to_str().context("repository checkout is not UTF-8")?;
        event(
            &tx,
            anchor,
            None,
            EventKind::PlanReviewStarted,
            json!({"proposal_id": proposal_id, "plan_review_id": id, "attempt": attempt, "dir": dir_text, "session_id": session_id, "cwd": cwd, "features": features, "launch": launch.to_value()}),
        )?;
        tx.commit()?;
        Ok(Some(PlanReviewJob {
            id,
            proposal_id,
            attempt,
            anchor,
            dir,
            session_id,
        }))
    }

    fn finish_plan_review(
        &mut self,
        job: &PlanReviewJob,
        token: &LeaseToken,
        apply: &PlanReviewApply,
    ) -> Result<PlanReviewApplied> {
        let now = self.generators.clock.now();
        let stamp = self.generators.clock.timestamp();
        // The spans it closes read their transcripts first (task 543).
        let _read = sessions::read_before(
            &self.conn,
            sessions::Closing::PlanReviews(Some(job.id), false),
        )?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !running(&tx, job, token)? {
            return Ok(PlanReviewApplied {
                stale: true,
                ..PlanReviewApplied::default()
            });
        }
        let verdict_json = serde_json::to_value(&apply.verdict)?;
        if end_unapplied(
            &tx,
            job,
            now,
            Some((&verdict_json, apply.verdict.verdict)),
            None,
            Some(apply.session.as_ref()),
        )? {
            tx.commit()?;
            return Ok(PlanReviewApplied {
                stale: true,
                ..PlanReviewApplied::default()
            });
        }
        let members = proposals::read(&tx, job.proposal_id)?.task_ids().to_vec();
        // The tasks it predicted the weight of (ADR-0079 decision 2): those
        // still submitted, before a pass readies or cancels them.
        let mut submitted = Vec::new();
        for &member in &members {
            if read_task(&tx, member)?.status() == TaskStatus::Submitted {
                submitted.push(member);
            }
        }
        let predictions =
            prediction::parse_predictions(apply.verdict.predictions.as_ref(), &submitted);
        // A pass leaves a priority it may not change (ADR-t1971-1
        // decision 3, by the records of ADR-t1975-1 decision 7) unapplied
        // with why, and the rest of the verdict goes on.
        let mut actions = Vec::new();
        let mut actions_skipped = Vec::new();
        if apply.decision == PlanReviewDecision::Pass {
            for action in &apply.verdict.actions {
                let refusal = match action {
                    PlanReviewAction::LowerPriority { task_id, .. }
                        if members.contains(task_id) =>
                    {
                        plan_review::priority_refusal(&pass_priority(&tx, *task_id)?)
                    }
                    _ => None,
                };
                match refusal {
                    Some(reason) => {
                        actions_skipped.push(json!({"action": action, "reason": reason}));
                    }
                    None => {
                        check_action(&tx, &members, action)?;
                        actions.push(action);
                    }
                }
            }
        }
        plan_review::check_verdict(&members, apply.decision, &apply.verdict)
            .map_err(anyhow::Error::msg)?;
        let mut applied = PlanReviewApplied::default();
        let mut skipped = Vec::new();
        let mut priorities_inherited = Vec::new();
        match apply.decision {
            PlanReviewDecision::Pass => {
                for action in actions {
                    apply_action(&tx, action, &stamp)?;
                }
                // Every submitted AI's task takes its goal's priority
                // instead of one the AI set, actions or none (ADR-t1971-1
                // decision 4).
                let mut tasks = Vec::new();
                for &member in &members {
                    tasks.push(pass_priority(&tx, member)?);
                }
                for (task_id, _) in plan_review::priorities_to_inherit(&tasks) {
                    inherit_priority(&tx, task_id, &stamp)?;
                    priorities_inherited.push(task_id);
                }
                proposals::approve(&tx, job.proposal_id, &stamp)?;
            }
            PlanReviewDecision::Revise => {
                proposals::send_back(&tx, job.proposal_id, &stamp)?;
                await_delivery(&tx, job.proposal_id, &apply.revise_reasons, now)?;
            }
            PlanReviewDecision::Concern => {
                set_hold(&tx, job.proposal_id, Some(ReviewHold::Concern))?;
                let ask = apply
                    .ask
                    .as_ref()
                    .context("a concern opens an approve_plan ask")?;
                ask.validate()?;
                applied.ask = Some(insert_ask(&tx, ask)?);
            }
        }
        for reopen_task in &apply.verdict.reopen {
            match reopen(
                &tx,
                reopen_task.task_id,
                &reopen_task.reason,
                job.proposal_id,
                &stamp,
                now,
            )? {
                Ok(reopened) => applied.reopened.push(reopened),
                Err(why) => skipped.push(why),
            }
        }
        finish_row(
            &tx,
            job.id,
            now,
            apply.decision.into(),
            Some(&verdict_json),
            None,
        )?;
        let mut finished = json!({
                "proposal_id": job.proposal_id,
                "plan_review_id": job.id,
                "attempt": job.attempt,
                "verdict": apply.verdict.verdict,
                "decision": apply.decision,
                "overridden": apply.overridden,
                "reasons": apply.verdict.reasons,
                "reason_codes": apply.verdict.recorded_codes(),
                "primary_code": (apply.decision != PlanReviewDecision::Pass)
                    .then(|| review_reason::primary(&apply.verdict.recorded_codes())),
                "summary": apply.verdict.summary,
                "actions": apply.verdict.actions,
                "actions_skipped": actions_skipped,
                "priorities_inherited": priorities_inherited,
                "reopened": applied.reopened,
                "reopen_skipped": skipped,
                "precedents": apply.verdict.precedents,
                "ask_id": applied.ask.as_ref().map(|outcome| outcome.ask.id),
                "duration_secs": apply.duration_secs,
                "prediction_error": predictions.as_ref().err(),
                "prompt_bytes": apply.prompt_bytes,
        });
        if let Some(session) = &apply.session {
            session.record(&mut finished);
        }
        event(
            &tx,
            job.anchor,
            None,
            EventKind::PlanReviewFinished,
            finished,
        )?;
        if let Some(decided) = &apply.concern {
            // What became of the concern (ADR-t451-1 decision 4): applied
            // as its recommendation, or left to a person and why.
            event(
                &tx,
                job.anchor,
                None,
                EventKind::PlanConcernDecided,
                json!({
                    "proposal_id": job.proposal_id,
                    "plan_review_id": job.id,
                    "recommendation": decided.recommendation,
                    "confidence": decided.confidence,
                    "reason_category": decided.reason_category,
                    "applied": decided.applied.is_some(),
                    "decision": decided.applied,
                    "escalated_because": decided.escalated_because,
                    "ask_id": applied.ask.as_ref().map(|outcome| outcome.ask.id),
                }),
            )?;
        }
        if let Ok(predictions) = &predictions {
            record_predictions(&tx, job, apply.session.as_ref(), predictions)?;
        }
        tx.commit()?;
        Ok(applied)
    }

    fn fail_plan_review(
        &mut self,
        job: &PlanReviewJob,
        token: &LeaseToken,
        failure: &PlanReviewFailure,
    ) -> Result<()> {
        let error = failure.error.as_str();
        let duration_secs = failure.duration_secs;
        let now = self.generators.clock.now();
        // The spans it closes read their transcripts first (task 543).
        let _read = sessions::read_before(
            &self.conn,
            sessions::Closing::PlanReviews(Some(job.id), false),
        )?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !running(&tx, job, token)? {
            return Ok(());
        }
        let ran = failure.ran.then_some(failure.session.as_ref());
        if end_unapplied(&tx, job, now, None, Some(error), ran)? {
            tx.commit()?;
            return Ok(());
        }
        let (outcome, hold) = plan_review::failed_end(failure.unusable.is_some());
        finish_row(&tx, job.id, now, outcome, None, Some(error))?;
        if hold.is_some() {
            set_hold(&tx, job.proposal_id, hold)?;
        }
        let mut failed = json!({
            "code": crate::domain::ReasonCode::JobFailed,
            "proposal_id": job.proposal_id,
            "plan_review_id": job.id,
            "attempt": job.attempt,
            "error": error,
            "duration_secs": duration_secs,
            "status": TaskStatus::Submitted.as_str(),
            "prompt_bytes": failure.prompt_bytes,
        });
        if let Some(session) = &failure.session {
            session.record(&mut failed);
        }
        if let Some((provider, reason)) = failure.unusable {
            failed["provider_unusable"] = json!({"provider": provider, "reason": reason});
        }
        event(&tx, job.anchor, None, EventKind::PlanReviewFailed, failed)?;
        tx.commit()?;
        Ok(())
    }

    fn revising_proposals(&self) -> Result<Vec<RevisingProposal>> {
        let tx = self.conn.unchecked_transaction()?;
        let ids: Vec<ProposalId> = tx
            .prepare("SELECT id FROM proposals WHERE status='revising' ORDER BY id")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter()
            .map(|id| {
                let (reasons, revised_at, sent_at, planner_id, unresponsive_at) = tx.query_row(
                    "SELECT revise_reasons, revised_at, revise_sent_at, revise_planner_id,
                            unresponsive_at FROM proposals WHERE id=?1",
                    [id],
                    |r| {
                        Ok((
                            r.get::<_, Option<String>>(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                        ))
                    },
                )?;
                Ok(RevisingProposal {
                    proposal: proposals::read(&tx, id)?,
                    reasons: reasons
                        .map(|text| serde_json::from_str(&text))
                        .transpose()?
                        .unwrap_or_default(),
                    revised_at,
                    sent_at,
                    planner_id,
                    unresponsive_at,
                })
            })
            .collect()
    }

    fn claim_revise(&mut self, proposal_id: ProposalId) -> Result<bool> {
        let now = self.generators.clock.now();
        Ok(self.conn.execute(
            "UPDATE proposals SET revise_sent_at=?2, revise_planner_id=NULL
             WHERE id=?1 AND status='revising' AND revise_sent_at IS NULL",
            params![proposal_id, now],
        )? == 1)
    }

    fn revise_sent(
        &mut self,
        proposal_id: ProposalId,
        planner: PlannerId,
        workspace: &str,
        opened: Option<&crate::domain::actor_model::ActorLaunch>,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE proposals SET revise_planner_id=?2, unresponsive_at=NULL
             WHERE id=?1 AND status='revising'",
            params![proposal_id, planner],
        )?;
        event(
            &tx,
            anchor(&tx, proposal_id)?,
            None,
            EventKind::PlanReviseSent,
            json!({
                "proposal_id": proposal_id,
                "planner_id": planner,
                "workspace_id": workspace,
                "opened": opened.is_some(),
                "launch": opened.map(ActorLaunch::to_value),
                "effort_raised": opened.is_some_and(|launch| {
                    launch.escalated_from.is_some() && launch.escalated_from != launch.effort
                }),
                "effort_not_raised": opened.is_none().then_some(LIVE_PLANNER_NOT_RAISED),
            }),
        )?;
        tx.commit()?;
        Ok(())
    }

    fn revise_lost(
        &mut self,
        proposal_id: ProposalId,
        planner: Option<PlannerId>,
        why: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE proposals SET revise_sent_at=NULL, revise_planner_id=NULL, unresponsive_at=NULL
             WHERE id=?1 AND status='revising' AND revise_sent_at IS NOT NULL",
            [proposal_id],
        )?;
        if changed == 1 && planner.is_some() {
            event(
                &tx,
                anchor(&tx, proposal_id)?,
                None,
                EventKind::PlanReviseLost,
                json!({"proposal_id": proposal_id, "planner_id": planner, "reason": why}),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn planner_unresponsive(
        &mut self,
        proposal_id: ProposalId,
        planner: Option<PlannerId>,
        waited_secs: i64,
        holders: &[PlannerHold],
    ) -> Result<()> {
        let now = self.generators.clock.now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE proposals SET unresponsive_at=?2
             WHERE id=?1 AND status='revising' AND unresponsive_at IS NULL",
            params![proposal_id, now],
        )?;
        if changed == 1 {
            let mut payload = json!({
                "proposal_id": proposal_id,
                "planner_id": planner,
                "waited_secs": waited_secs,
            });
            let reason = match planner {
                Some(planner) => format!(
                    "planner {planner} did not submit proposal {proposal_id} again within {waited_secs} seconds of its revise"
                ),
                None => {
                    // Why the revise waits: the runtime's planners that
                    // fill the limit, each with why it is not ended.
                    payload["holders"] = serde_json::to_value(holders)?;
                    let held = holders
                        .iter()
                        .map(|hold| {
                            let busy = if hold.busy.is_empty() {
                                "done".to_owned()
                            } else {
                                hold.busy.join(", ")
                            };
                            format!("planner {} ({busy})", hold.planner_id)
                        })
                        .collect::<Vec<_>>();
                    let mut reason = format!(
                        "the revise of proposal {proposal_id} waited {waited_secs} seconds for a planner to take it"
                    );
                    if !held.is_empty() {
                        reason.push_str(&format!(
                            "; the runtime's planners at the limit: {}",
                            held.join("; ")
                        ));
                    }
                    reason
                }
            };
            payload["reason"] = json!(reason);
            event(
                &tx,
                anchor(&tx, proposal_id)?,
                None,
                EventKind::PlannerUnresponsive,
                payload,
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn settle_proposals(&mut self) -> Result<Vec<(ProposalId, ProposalStatus)>> {
        let stamp = self.generators.clock.timestamp();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let emptied: Vec<ProposalId> = tx
            .prepare(
                "SELECT p.id FROM proposals p WHERE p.status='submitted'
                 AND NOT EXISTS (SELECT 1 FROM tasks t WHERE t.proposal_id = p.id
                                 AND t.status='submitted')
                 AND NOT EXISTS (SELECT 1 FROM plan_reviews r WHERE r.proposal_id = p.id
                                 AND r.finished_at IS NULL)
                 ORDER BY p.id",
            )?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut settled = Vec::new();
        for id in emptied {
            let live: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM tasks WHERE proposal_id=?1 AND status!='canceled')",
                [id],
                |r| r.get(0),
            )?;
            let current = proposals::read(&tx, id)?;
            let ended = if live {
                proposal::accept(current, stamp.clone())?
            } else {
                proposal::cancel(current, stamp.clone())?
            };
            proposals::save(&tx, &ended)?;
            set_hold(&tx, id, None)?;
            if let Ok(anchor) = anchor(&tx, id) {
                event(
                    &tx,
                    anchor,
                    None,
                    EventKind::ProposalSettled,
                    json!({"proposal_id": id, "status": ended.status(), "reason": "none of its tasks waits for plan review any more"}),
                )?;
            }
            settled.push((id, ended.status()));
        }
        tx.commit()?;
        Ok(settled)
    }

    fn plan_answers(&self) -> Result<Vec<Ask>> {
        let ids: Vec<AskId> = self
            .conn
            .prepare(
                "SELECT id FROM asks WHERE kind='approve_plan' AND answered_at IS NOT NULL
                 AND closed_at IS NULL ORDER BY id",
            )?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter().map(|id| read_ask(&self.conn, id)).collect()
    }

    fn decide_plan(&mut self, ask_id: AskId) -> Result<Option<PlanDecided>> {
        let now = self.generators.clock.now();
        let stamp = self.generators.clock.timestamp();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ask = read_ask(&tx, ask_id)?;
        ensure!(
            ask.kind == AskKind::ApprovePlan,
            "ask {ask_id} is not an approve_plan ask"
        );
        let text = ask.answer.clone().unwrap_or_default();
        let Some(answer) = PlanAnswer::parse(&text) else {
            return Ok(None);
        };
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
        if !plan_answer_applies(&tx, &ask, &text)? {
            close(&tx)?;
            // Nothing else names the ask: `ask_closed` ends its wait (task 568).
            record_ask_closed(&tx, &ask)?;
            tx.commit()?;
            return Ok(None);
        }
        let proposal_id = ask_proposal(&tx, &ask)?.context("the ask's task has no proposal")?;
        set_hold(&tx, proposal_id, None)?;
        let decided = match &answer {
            PlanAnswer::Ready => proposals::approve(&tx, proposal_id, &stamp)?,
            PlanAnswer::SendBack(reason) => {
                let sent = proposals::send_back(&tx, proposal_id, &stamp)?;
                let mut reasons = vec![plan_review::person_send_back_reason(
                    ask_id,
                    reason.as_deref(),
                )];
                reasons.extend(latest_reasons(&tx, proposal_id)?);
                await_delivery(&tx, proposal_id, &reasons, now)?;
                sent
            }
            PlanAnswer::Cancel => {
                let canceled = proposal::cancel(proposals::read(&tx, proposal_id)?, stamp.clone())?;
                proposals::save(&tx, &canceled)?;
                for &task_id in canceled.task_ids() {
                    if read_task(&tx, task_id)?.status() == TaskStatus::Submitted {
                        transition_task(&tx, task_id, TaskAction::Cancel, &stamp)?;
                    }
                }
                proposals::read(&tx, proposal_id)?
            }
        };
        close(&tx)?;
        record_plan_outcome(&tx, proposal_id, ask_id, &text)?;
        event(
            &tx,
            anchor(&tx, proposal_id)?,
            None,
            EventKind::PlanDecided,
            json!({"proposal_id": proposal_id, "ask_id": ask_id, "answer": text.trim(), "status": decided.status()}),
        )?;
        tx.commit()?;
        Ok(Some(PlanDecided {
            proposal: decided,
            answer: text.trim().to_owned(),
        }))
    }

    fn applies_plan_answer(&self, ask: &Ask) -> Result<bool> {
        match &ask.answer {
            Some(text) if ask.closed_at.is_none() => plan_answer_applies(&self.conn, ask, text),
            _ => Ok(false),
        }
    }

    fn stranded_dependencies(&self) -> Result<Vec<crate::domain::StrandedDependency>> {
        SqliteQueue::stranded_dependencies(self)
    }

    fn plan_review_holds(&self) -> Result<Vec<PlanReviewHold>> {
        let rows: Vec<(ProposalId, bool, Option<String>)> = self
            .conn
            .prepare(
                "SELECT p.id, p.status='submitted',
                        (SELECT error FROM plan_reviews r WHERE r.proposal_id = p.id
                         ORDER BY r.id DESC LIMIT 1)
                 FROM proposals p
                 WHERE (p.status='submitted' AND p.review_hold='failed')
                    OR (p.status='submitted' AND p.review_hold='concern' AND NOT EXISTS (
                        SELECT 1 FROM asks a JOIN tasks t ON t.id = a.task_id
                        WHERE t.proposal_id = p.id AND a.kind='approve_plan'
                          AND a.closed_at IS NULL))
                    OR (p.status='revising' AND p.unresponsive_at IS NOT NULL)
                 ORDER BY p.id",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(proposal_id, failed, error)| {
                // A proposal with no task left has nothing to show on.
                let anchor = anchor(&self.conn, proposal_id).ok()?;
                Some(PlanReviewHold {
                    proposal_id,
                    anchor,
                    kind: if failed {
                        event_kind::PLAN_REVIEW_FAILED
                    } else {
                        event_kind::PLANNER_UNRESPONSIVE
                    },
                    error: if failed { error } else { None },
                })
            })
            .collect())
    }

    fn answered_asks(&self, limit: usize) -> Result<Vec<Ask>> {
        let ids: Vec<AskId> = self
            .conn
            .prepare(
                "SELECT id FROM asks WHERE answered_at IS NOT NULL ORDER BY answered_at DESC, id DESC
                 LIMIT ?1",
            )?
            .query_map([i64::try_from(limit)?], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter().map(|id| read_ask(&self.conn, id)).collect()
    }
}

/// Record `predictions` as one `task_weight_predicted` per task (ADR-0079
/// decision 2), with the model and effort the job's session used, which the
/// `session_closed` its `plan_review_finished` wrote carries (none when its
/// transcript named no model), or, for a job that named its session itself
/// (Codex), the model its output named (`named`). A later plan review of
/// the task adds its own; the last one is the task's.
fn record_predictions(
    conn: &Connection,
    job: &PlanReviewJob,
    named: Option<&crate::domain::headless_job::JobSession>,
    predictions: &[prediction::TaskWeightPrediction],
) -> Result<()> {
    let session: Value = match &job.session_id {
        Some(session_id) => conn
            .query_row(
                "SELECT payload FROM run_events WHERE kind=?1 AND json_extract(payload,'$.session_id')=?2
                 ORDER BY id DESC LIMIT 1",
                params![SESSION_CLOSED, session_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(|text| serde_json::from_str(&text))
            .transpose()?
            .unwrap_or(Value::Null),
        None => json!({"model": named.and_then(|session| session.model.clone())}),
    };
    for predicted in predictions {
        let mut body = serde_json::to_value(predicted)?;
        if let Some(body) = body.as_object_mut() {
            body.remove("task_id");
        }
        event(
            conn,
            predicted.task_id,
            None,
            EventKind::TaskWeightPredicted,
            json!({
                "proposal_id": job.proposal_id,
                "plan_review_id": job.id,
                "attempt": job.attempt,
                "prediction": body,
                "model": session.get("model"),
                "effort": session.get("effort"),
            }),
        )?;
    }
    Ok(())
}

/// Submitted proposals with no hold and a submitted task, oldest
/// submission first, each with whether a submitted task is `interrupt`.
/// The `coalesce` is [`crate::domain::base_priority`] in SQL, so the whole
/// queue is not read to find one: change the two together.
fn candidates(conn: &Connection) -> Result<Vec<PlanReviewCandidate>> {
    Ok(conn
        .prepare(
            "SELECT p.id, p.submitted_at, max(t.status='submitted'
                 AND coalesce(t.priority, (SELECT g.priority FROM goals g WHERE g.id = t.goal_id), 1)=?1)
             FROM proposals p JOIN tasks t ON t.proposal_id = p.id
             WHERE p.status='submitted' AND p.review_hold IS NULL
             GROUP BY p.id HAVING max(t.status='submitted')
             ORDER BY p.submitted_at, p.id",
        )?
        .query_map([Priority::Interrupt.as_i64()], |r| {
            Ok(PlanReviewCandidate {
                proposal_id: r.get(0)?,
                submitted_at: r.get(1)?,
                interrupt: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// Record what a person's answer to the `approve_plan` ask of a concern
/// says of plan review's findings (ADR-t947-1 decision 4): a
/// `plan_review_outcome` on the proposal's latest review with a verdict,
/// when that review was the concern, with its codes (a `send_back` carries
/// them on).
fn record_plan_outcome(
    conn: &Connection,
    proposal_id: ProposalId,
    ask_id: AskId,
    answer: &str,
) -> Result<()> {
    let Some(outcome) = review_reason::answer_outcome(answer) else {
        return Ok(());
    };
    let latest: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT id, outcome, verdict FROM plan_reviews WHERE proposal_id=?1
             AND verdict IS NOT NULL ORDER BY id DESC LIMIT 1",
            [proposal_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((plan_review_id, decided, verdict)) = latest else {
        return Ok(());
    };
    if decided != PlanReviewOutcome::Concern.as_str() {
        return Ok(());
    }
    // The row keeps the verdict as the runtime read it: the texts in
    // `reasons` and the codes in `reason_codes`, absent before the codes.
    let verdict: Value = serde_json::from_str(&verdict)?;
    let printed: Vec<Vec<String>> =
        serde_json::from_value(verdict["reason_codes"].clone()).unwrap_or_default();
    let reasons = verdict["reasons"].as_array().map_or(0, Vec::len);
    let codes = review_reason::recorded(&printed, reasons);
    event(
        conn,
        anchor(conn, proposal_id)?,
        None,
        EventKind::PlanReviewOutcome,
        json!({
            "proposal_id": proposal_id,
            "plan_review_id": plan_review_id,
            "ask_id": ask_id,
            "outcome": outcome,
            "reason_codes": codes,
            "primary_code": review_reason::primary(&codes),
        }),
    )
}

/// The reasons of the proposal's latest finished review with a verdict.
fn latest_reasons(conn: &Connection, proposal_id: ProposalId) -> Result<Vec<String>> {
    let verdict: Option<String> = conn
        .query_row(
            "SELECT verdict FROM plan_reviews WHERE proposal_id=?1 AND verdict IS NOT NULL
             ORDER BY id DESC LIMIT 1",
            [proposal_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(verdict) = verdict else {
        return Ok(Vec::new());
    };
    let value: Value = serde_json::from_str(&verdict)?;
    match value.get("reasons") {
        Some(Value::Array(items)) => Ok(items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()),
        Some(_) => bail!("plan review verdict of proposal {proposal_id} has malformed reasons"),
        None => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        application::TaskStore,
        domain::{NewTask, PlannerOrigin, PlannerOwner, Submission},
    };

    /// The job's checkout is on its start event, and so on the span of its
    /// Claude session (ADR-0048).
    #[test]
    fn a_plan_review_records_the_checkout_it_runs_in() {
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let task_id = queue
            .add(NewTask {
                title: "t".into(),
                description: String::new(),
                acceptance: String::new(),
                verification_commands: Vec::new(),
                required_evidence: Vec::new(),
                paths: Vec::new(),
                dependencies: Vec::new(),
                goal_dependencies: Vec::new(),
                priority: Default::default(),
                goal_id: None,
                context: String::new(),
                change: None,
                provider: None,
                worker_mode: None,
                wait_for_build: false,
                execution_class: Default::default(),
            })
            .unwrap()
            .id();
        let proposal_id = queue
            .submit(Submission {
                tasks: vec![task_id],
                goals: Vec::new(),
                proposal: None,
                owner: PlannerOwner {
                    origin: PlannerOrigin::Person,
                    workspace_id: None,
                },
            })
            .unwrap()
            .id();
        let job = queue
            .begin_plan_review(
                proposal_id,
                &LeaseToken::new("token"),
                &dir.path().join("plan-reviews"),
                Path::new("/repo"),
                &ActorLaunch::default_of(crate::domain::actor_model::ModelRole::PlanReview),
            )
            .unwrap()
            .unwrap();
        let payload = |kind: &str| -> Value {
            let text: String = queue
                .conn
                .query_row(
                    "SELECT payload FROM run_events WHERE kind=?1",
                    [kind],
                    |r| r.get(0),
                )
                .unwrap();
            serde_json::from_str(&text).unwrap()
        };
        let started = payload("plan_review_started");
        assert_eq!(started["cwd"], "/repo");
        assert_eq!(started["session_id"], job.session_id.as_deref().unwrap());
        assert_eq!(started["plan_review_id"], job.id);
        let opened = payload("session_opened");
        assert_eq!(opened["kind"], "plan_review");
        assert_eq!(opened["cwd"], "/repo");
        // A person's proposal of one task close to nothing (task 579).
        assert_eq!(
            started["features"],
            json!({"origin": "person", "follow_up_depth": 0, "related_score": null,
                   "related": "low", "revise_count": 0})
        );
    }

    fn new_task(title: &str) -> NewTask {
        NewTask {
            title: title.into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            priority: Default::default(),
            goal_id: None,
            context: String::new(),
            change: None,
            provider: None,
            worker_mode: None,
            wait_for_build: false,
            execution_class: Default::default(),
        }
    }

    /// A proposal of a follow-up of a follow-up is `follow_up` two deep,
    /// and its closest task outside it scores; its own tasks do not.
    #[test]
    fn a_plan_review_records_what_its_proposal_is_like() {
        use crate::domain::DraftOrigin;
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let source = queue
            .add(new_task("stats: count the widget gadgets per window"))
            .unwrap()
            .id();
        let first = queue.add(new_task("widget gadgets")).unwrap().id();
        let second = queue.add(new_task("widget gadgets again")).unwrap().id();
        for (task, of) in [(first, source), (second, first)] {
            queue
                .record_draft_origin(
                    task,
                    DraftOrigin::FollowUp,
                    &json!({"source_task_id": of.as_i64(), "source_goal_state": "none"}),
                )
                .unwrap();
        }
        let proposal_id = queue
            .submit(Submission {
                tasks: vec![first, second],
                goals: Vec::new(),
                proposal: None,
                owner: PlannerOwner {
                    origin: PlannerOrigin::Person,
                    workspace_id: None,
                },
            })
            .unwrap()
            .id();
        queue
            .begin_plan_review(
                proposal_id,
                &LeaseToken::new("token"),
                &dir.path().join("plan-reviews"),
                Path::new("/repo"),
                &ActorLaunch::default_of(crate::domain::actor_model::ModelRole::PlanReview),
            )
            .unwrap()
            .unwrap();
        let text: String = queue
            .conn
            .query_row(
                "SELECT payload FROM run_events WHERE kind='plan_review_started'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let features = &serde_json::from_str::<Value>(&text).unwrap()["features"];
        assert_eq!(features["origin"], "follow_up");
        assert_eq!(features["follow_up_depth"], 2);
        assert_eq!(features["revise_count"], 0);
        let best = queue.related(first.as_i64(), &[], 5).unwrap();
        let outside = best
            .related
            .iter()
            .find(|task| task.id == source.as_i64())
            .unwrap()
            .score;
        let score = features["related_score"].as_f64().unwrap();
        assert!(score >= outside, "{score} < {outside}");
        assert_eq!(
            features["related"],
            crate::domain::plan_quality::related_tier(Some(score))
        );
        // A proposal that is gone has no features.
        assert!(
            queue
                .proposal_features(ProposalId::new(99))
                .unwrap()
                .is_none()
        );
    }

    /// A job whose task was edited while it ran is discarded with the
    /// Execution it was (ADR-t1486-1): the tokens its output gave, or not
    /// measured when it named none.
    #[test]
    fn a_discarded_plan_review_records_its_execution() {
        use crate::domain::{
            TaskEdit,
            tokens::{ExecutionTokens, TokenSource, TokenUsage},
        };
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let task_id = queue.add(new_task("t")).unwrap().id();
        let proposal_id = queue
            .submit(Submission {
                tasks: vec![task_id],
                goals: Vec::new(),
                proposal: None,
                owner: PlannerOwner {
                    origin: PlannerOrigin::Person,
                    workspace_id: None,
                },
            })
            .unwrap()
            .id();
        let token = LeaseToken::new("token");
        let measured = JobSession {
            tokens: Some(ExecutionTokens {
                tokens: Some(TokenUsage {
                    input: 40,
                    output: 2,
                    ..TokenUsage::default()
                }),
                source: Some(TokenSource::UsageRecord),
                ..ExecutionTokens::default()
            }),
            ..JobSession::default()
        };
        // A failed job and a job with a verdict, each discarded.
        let ends = [
            ("first edit", Some(measured.clone()), false),
            ("second edit", None, false),
            ("third edit", Some(measured), true),
        ];
        for (edit, session, verdict) in ends {
            let job = queue
                .begin_plan_review(
                    proposal_id,
                    &token,
                    &dir.path().join("plan-reviews"),
                    Path::new("/repo"),
                    &ActorLaunch::default_of(crate::domain::actor_model::ModelRole::PlanReview),
                )
                .unwrap()
                .unwrap();
            queue
                .edit_task(
                    task_id,
                    TaskEdit {
                        description: Some(edit.into()),
                        ..TaskEdit::default()
                    },
                    TaskStatus::Submitted,
                )
                .unwrap();
            if verdict {
                let applied = queue
                    .finish_plan_review(
                        &job,
                        &token,
                        &PlanReviewApply {
                            verdict: plan_review::PlanReviewVerdict::parse(
                                r#"{"verdict": "pass", "reasons": [], "summary": "fine"}"#,
                            )
                            .unwrap(),
                            decision: PlanReviewDecision::Pass,
                            overridden: None,
                            revise_reasons: Vec::new(),
                            ask: None,
                            concern: None,
                            duration_secs: 1,
                            session,
                            prompt_bytes: None,
                        },
                    )
                    .unwrap();
                assert!(applied.stale);
            } else {
                queue
                    .fail_plan_review(
                        &job,
                        &token,
                        &PlanReviewFailure {
                            error: "no verdict".into(),
                            session,
                            ran: true,
                            ..PlanReviewFailure::default()
                        },
                    )
                    .unwrap();
            }
        }
        let discarded: Vec<Value> = queue
            .conn
            .prepare(
                "SELECT payload FROM run_events WHERE kind='plan_review_discarded' ORDER BY id",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|text| serde_json::from_str(&text.unwrap()).unwrap())
            .collect();
        assert_eq!(discarded.len(), 3);
        assert_eq!(discarded[0]["tokens"]["input"], 40);
        assert_eq!(discarded[0]["tokens_source"], "token_usage_record");
        assert!(discarded[1]["tokens"].is_null(), "{}", discarded[1]);
        assert!(discarded[1].get("tokens_source").is_some());
        assert_eq!(discarded[1]["tokens_reason"], "tokens_not_read");
        assert_eq!(discarded[2]["verdict"], "pass");
        assert_eq!(discarded[2]["tokens"]["output"], 2);
        assert_eq!(discarded[2]["tokens_source"], "token_usage_record");
    }
    /// A job whose proposal moved on while it ran (here: held) records
    /// `plan_review_discarded` with the Execution of its agent: the tokens
    /// its output gave, or not measured. A job whose agent never started
    /// records nothing.
    #[test]
    fn a_plan_review_whose_proposal_moved_on_records_its_execution() {
        use crate::domain::tokens::{ExecutionTokens, TokenSource, TokenUsage};
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let task_id = queue.add(new_task("t")).unwrap().id();
        let proposal_id = queue
            .submit(Submission {
                tasks: vec![task_id],
                goals: Vec::new(),
                proposal: None,
                owner: PlannerOwner {
                    origin: PlannerOrigin::Person,
                    workspace_id: None,
                },
            })
            .unwrap()
            .id();
        let token = LeaseToken::new("token");
        let measured = JobSession {
            tokens: Some(ExecutionTokens {
                tokens: Some(TokenUsage {
                    input: 50,
                    output: 5,
                    ..TokenUsage::default()
                }),
                source: Some(TokenSource::UsageRecord),
                ..ExecutionTokens::default()
            }),
            ..JobSession::default()
        };
        // A verdict, a failure of an agent that ran, and a failure of one
        // that never started, each after the proposal was held.
        let ends = [
            (Some(measured), true, true),
            (None, true, false),
            (None, false, false),
        ];
        for (session, ran, verdict) in ends {
            let job = queue
                .begin_plan_review(
                    proposal_id,
                    &token,
                    &dir.path().join("plan-reviews"),
                    Path::new("/repo"),
                    &ActorLaunch::default_of(crate::domain::actor_model::ModelRole::PlanReview),
                )
                .unwrap()
                .unwrap();
            queue
                .conn
                .execute(
                    "UPDATE proposals SET review_hold='failed' WHERE id=?1",
                    [proposal_id],
                )
                .unwrap();
            if verdict {
                let applied = queue
                    .finish_plan_review(
                        &job,
                        &token,
                        &PlanReviewApply {
                            verdict: plan_review::PlanReviewVerdict::parse(
                                r#"{"verdict": "pass", "reasons": [], "summary": "fine"}"#,
                            )
                            .unwrap(),
                            decision: PlanReviewDecision::Pass,
                            overridden: None,
                            revise_reasons: Vec::new(),
                            ask: None,
                            concern: None,
                            duration_secs: 1,
                            session,
                            prompt_bytes: None,
                        },
                    )
                    .unwrap();
                assert!(applied.stale);
            } else {
                queue
                    .fail_plan_review(
                        &job,
                        &token,
                        &PlanReviewFailure {
                            error: "no verdict".into(),
                            session,
                            ran,
                            ..PlanReviewFailure::default()
                        },
                    )
                    .unwrap();
            }
            queue
                .conn
                .execute(
                    "UPDATE proposals SET review_hold=NULL WHERE id=?1",
                    [proposal_id],
                )
                .unwrap();
        }
        let discarded: Vec<Value> = queue
            .conn
            .prepare(
                "SELECT payload FROM run_events WHERE kind='plan_review_discarded' ORDER BY id",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|text| serde_json::from_str(&text.unwrap()).unwrap())
            .collect();
        assert_eq!(discarded.len(), 2, "{discarded:?}");
        assert_eq!(discarded[0]["verdict"], "pass");
        assert_eq!(discarded[0]["edited"], json!([]));
        assert_eq!(discarded[0]["tokens"]["input"], 50);
        assert_eq!(discarded[0]["tokens_source"], "token_usage_record");
        assert!(discarded[1]["tokens"].is_null(), "{}", discarded[1]);
        assert!(discarded[1].get("tokens_source").is_some());
        assert_eq!(discarded[1]["tokens_reason"], "tokens_not_read");
        assert_eq!(discarded[1]["error"], "no verdict");
    }
}
