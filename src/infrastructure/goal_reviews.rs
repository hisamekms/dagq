//! Goal review (ADR-0047 decision 43): the `goal_reviews` rows of the
//! headless job (one unfinished at a time, queue-wide), the goals it may
//! take, and the verdicts and `approve_goal` answers the supervisor
//! applies, each in one transaction. Events go to the goal.
use crate::domain::EventKind;
use crate::domain::LeaseToken;
use crate::domain::write_rules::{check_at_least, check_non_blank};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::path::Path;

use super::{
    asks::{insert_ask, read_ask, record_ask_closed},
    follow_up_membership, sessions,
    sqlite::{SqliteQueue, close_goal_in, enum_col, goal_event, insert_task},
};
use crate::{
    application::{
        GoalDecided, GoalReviewApplied, GoalReviewApply, GoalReviewFailure, GoalReviewHold,
        GoalReviewJob, GoalReviewRecord, GoalReviewStore,
    },
    domain::{
        Ask, AskId, AskKind, GoalId, GoalVerdict, HEARTBEAT_TIMEOUT_SECS, NewTask, TaskId,
        TaskStatus, follow_up,
        goal_review::{self, GoalAnswer, GoalGap, GoalReviewDecision, LatestReview},
    },
};

/// Each task of the goal with its status, in ID order.
fn goal_tasks(conn: &Connection, goal: GoalId) -> Result<Vec<(TaskId, TaskStatus)>> {
    Ok(conn
        .prepare("SELECT id, status FROM tasks WHERE goal_id=?1 ORDER BY id")?
        .query_map([goal], |r| Ok((r.get(0)?, enum_col(r, "status")?)))?
        .collect::<rusqlite::Result<_>>()?)
}

/// What a review of the goal sees now (ADR-t1504-2 decision 8): its
/// fingerprint (tasks, acceptance version, follow-ups and their
/// judgements) and whether it may close as achieved — its tasks all ended,
/// one completed, and no follow-up of it unsettled.
fn review_input(conn: &Connection, goal: GoalId) -> Result<(String, bool)> {
    let tasks = goal_tasks(conn, goal)?;
    let follow_ups = follow_up_membership::source_follow_ups(conn, goal)?;
    let version = follow_up_membership::acceptance_version(conn, goal)?;
    let closable =
        goal_review::tasks_done(&tasks) && follow_up::unsettled_follow_ups(&follow_ups).is_empty();
    Ok((
        goal_review::review_fingerprint(&tasks, version, &follow_ups),
        closable,
    ))
}

/// The goal's latest review that ran to an end (not `interrupted`):
/// its ID, outcome, fingerprint and whether a person rearmed it.
fn latest(conn: &Connection, goal: GoalId) -> Result<Option<(i64, String, String, bool)>> {
    Ok(conn
        .query_row(
            "SELECT id, outcome, fingerprint, rearmed_at IS NOT NULL FROM goal_reviews
             WHERE goal_id=?1 AND finished_at IS NOT NULL AND outcome != 'interrupted'
             ORDER BY id DESC LIMIT 1",
            [goal],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?)
}

/// What [`latest`] read, for the decisions of [`goal_review`].
fn latest_review((_, outcome, seen, rearmed): &(i64, String, String, bool)) -> LatestReview<'_> {
    LatestReview {
        outcome,
        seen,
        rearmed: *rearmed,
    }
}

/// An `approve_goal` ask of the goal's reviews that is not closed.
fn open_ask(conn: &Connection, goal: GoalId) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM goal_reviews r JOIN asks a ON a.id = r.ask_id
         WHERE r.goal_id=?1 AND a.closed_at IS NULL)",
        [goal],
        |r| r.get(0),
    )?)
}

/// Whether the goal is one a review may take now; `Some` with what the
/// review sees ([`review_input`]) when it is.
fn candidate(conn: &Connection, goal: GoalId) -> Result<Option<String>> {
    let open: bool = conn.query_row(
        "SELECT status='open' AND closed_at IS NULL FROM goals WHERE id=?1",
        [goal],
        |r| r.get(0),
    )?;
    if !open {
        return Ok(None);
    }
    let (fingerprint, closable) = review_input(conn, goal)?;
    // Read only what the decision still needs.
    let ask_open = closable && open_ask(conn, goal)?;
    let latest = if closable && !ask_open {
        latest(conn, goal)?
    } else {
        None
    };
    let latest = latest.as_ref().map(latest_review);
    Ok(
        goal_review::reviewable(open, closable, ask_open, latest, &fingerprint)
            .then_some(fingerprint),
    )
}

fn candidates(conn: &Connection) -> Result<Vec<GoalId>> {
    let goals: Vec<GoalId> = conn
        .prepare("SELECT id FROM goals WHERE status='open' AND closed_at IS NULL ORDER BY id")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut found = Vec::new();
    for goal in goals {
        if candidate(conn, goal)?.is_some() {
            found.push(goal);
        }
    }
    Ok(found)
}

/// The goal's first task, where its `approve_goal` ask belongs.
fn anchor(conn: &Connection, goal: GoalId) -> Result<Option<TaskId>> {
    Ok(
        conn.query_row("SELECT min(id) FROM tasks WHERE goal_id=?1", [goal], |r| {
            r.get(0)
        })?,
    )
}

fn finish_row(
    conn: &Connection,
    id: i64,
    now: i64,
    outcome: &str,
    verdict: Option<&Value>,
    error: Option<&str>,
) -> Result<()> {
    check_non_blank("goal review outcome", outcome)?;
    conn.execute(
        "UPDATE goal_reviews SET finished_at=?2, outcome=?3, verdict=?4, error=?5
         WHERE id=?1 AND finished_at IS NULL",
        params![id, now, outcome, verdict.map(Value::to_string), error],
    )?;
    Ok(())
}

/// The job's row, while it is unfinished and still `token`'s.
fn running(conn: &Connection, job: &GoalReviewJob, token: &LeaseToken) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM goal_reviews WHERE id=?1 AND supervisor_token=?2
         AND finished_at IS NULL)",
        params![job.id, token],
        |r| r.get(0),
    )?)
}

/// How many of the goal's latest reviews that decided something were
/// `gaps`, counted back from the newest.
fn gaps_in_a_row(conn: &Connection, goal: GoalId) -> Result<usize> {
    let outcomes: Vec<String> = conn
        .prepare(
            "SELECT outcome FROM goal_reviews WHERE goal_id=?1
             AND outcome IN ('achieved', 'gaps', 'ask') ORDER BY id DESC",
        )?
        .query_map([goal], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let outcomes: Vec<&str> = outcomes.iter().map(String::as_str).collect();
    Ok(goal_review::gaps_in_a_row(&outcomes))
}

/// Register `gaps` as draft tasks of the goal, each with the origin
/// `goal_gap` and the material its planner is shown.
fn register_gaps(
    conn: &Connection,
    goal: GoalId,
    review: i64,
    gaps: &[GoalGap],
    summary: &str,
    stamp: &str,
    now: i64,
) -> Result<Vec<TaskId>> {
    let mut added = Vec::new();
    for gap in gaps {
        let mut description = gap.description.clone();
        if !gap.criterion.trim().is_empty() {
            description.push_str(&format!(
                "\n\nMissing for the goal's acceptance: {}",
                gap.criterion
            ));
        }
        let task = insert_task(
            conn,
            NewTask {
                title: gap.title.trim().to_owned(),
                description,
                acceptance: String::new(),
                verification_commands: Vec::new(),
                required_evidence: Vec::new(),
                paths: Vec::new(),
                priority: Default::default(),
                change: None,
                dependencies: Vec::new(),
                goal_dependencies: Vec::new(),
                goal_id: Some(goal),
                context: format!(
                    "goal_gap: goal review {review} of goal {goal} found this missing"
                ),
                provider: None,
                worker_mode: None,
                wait_for_build: false,
            },
            stamp,
        )?;
        let material = json!({
            "goal_id": goal,
            "goal_review_id": review,
            "criterion": gap.criterion,
            "summary": summary,
        });
        conn.execute(
            "INSERT INTO draft_origins(task_id, origin, material, created_at)
             VALUES (?1, 'goal_gap', ?2, ?3)",
            params![task.id(), material.to_string(), now],
        )?;
        added.push(task.id());
    }
    Ok(added)
}

/// The review row that opened `ask`, and its goal.
fn ask_review(conn: &Connection, ask: AskId) -> Result<Option<(i64, GoalId)>> {
    Ok(conn
        .query_row(
            "SELECT id, goal_id FROM goal_reviews WHERE ask_id=?1",
            [ask],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// What the answer `text` of `ask` does when the supervisor applies it
/// now: `None` when it is no option, the goal closed, or it cannot be
/// applied (`achieved` with a task not ended or a follow-up whose
/// membership is not settled, `abandoned` with one in
/// progress, `gaps` with nothing to register).
fn applicable(
    conn: &Connection,
    ask: &Ask,
    text: &str,
) -> Result<Option<(GoalAnswer, i64, GoalId)>> {
    if ask.kind != AskKind::ApproveGoal {
        return Ok(None);
    }
    let Some(answer) = GoalAnswer::parse(text) else {
        return Ok(None);
    };
    let Some((review, goal)) = ask_review(conn, ask.id)? else {
        return Ok(None);
    };
    let closed: bool = conn.query_row(
        "SELECT closed_at IS NOT NULL FROM goals WHERE id=?1",
        [goal],
        |r| r.get(0),
    )?;
    if closed {
        return Ok(None);
    }
    let tasks = goal_tasks(conn, goal)?;
    // Read only what the answer needs.
    let follow_ups_settled = answer != GoalAnswer::Achieved
        || !tasks
            .iter()
            .all(|(_, status)| GoalVerdict::Achieved.allows(*status))
        || follow_up::unsettled_follow_ups(&follow_up_membership::source_follow_ups(conn, goal)?)
            .is_empty();
    let review_has_gaps =
        answer == GoalAnswer::Gaps(None) && !review_gaps(conn, review)?.is_empty();
    let fits = answer.fits(&tasks, follow_ups_settled, review_has_gaps);
    Ok(fits.then_some((answer, review, goal)))
}

/// The gaps the verdict of review `id` listed.
fn review_gaps(conn: &Connection, id: i64) -> Result<Vec<GoalGap>> {
    let verdict: Option<String> =
        conn.query_row("SELECT verdict FROM goal_reviews WHERE id=?1", [id], |r| {
            r.get(0)
        })?;
    let Some(verdict) = verdict else {
        return Ok(Vec::new());
    };
    let verdict: Value = serde_json::from_str(&verdict)?;
    Ok(serde_json::from_value(verdict["gaps"].clone()).unwrap_or_default())
}

pub(super) fn goal_answer_applies(conn: &Connection, ask: &Ask, text: &str) -> Result<bool> {
    Ok(ask.closed_at.is_none() && applicable(conn, ask, text)?.is_some())
}

impl GoalReviewStore for SqliteQueue {
    fn interrupt_goal_reviews_for_handoff(&mut self, token: &LeaseToken) -> Result<()> {
        let now = self.generators.clock.now();
        // Read transcripts before taking the write lock, as in begin/finish.
        let _read = sessions::read_before(&self.conn, sessions::Closing::GoalReviews(None, false))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let unfinished: Vec<i64> = tx
            .prepare(
                "SELECT id FROM goal_reviews WHERE supervisor_token=?1 AND finished_at IS NULL",
            )?
            .query_map([token], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in unfinished {
            finish_row(
                &tx,
                id,
                now,
                "interrupted",
                None,
                Some("stopped for the supervisor handoff"),
            )?;
            sessions::close_goal_review(&tx, id, false)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn goal_review_candidates(&self) -> Result<Vec<GoalId>> {
        candidates(&self.conn)
    }

    fn begin_goal_review(
        &mut self,
        goal: GoalId,
        token: &LeaseToken,
        goal_reviews_dir: &Path,
        cwd: &Path,
        launch: &crate::domain::actor_model::ActorLaunch,
    ) -> Result<Option<GoalReviewJob>> {
        let now = self.generators.clock.now();
        // The spans it closes read their transcripts first (task 543).
        let _read = sessions::read_before(&self.conn, sessions::Closing::GoalReviews(None, true))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let unfinished: Vec<(i64, LeaseToken, Option<i64>)> = tx
            .prepare(
                "SELECT r.id, r.supervisor_token, s.heartbeat_at FROM goal_reviews r
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
                "interrupted",
                None,
                Some("its supervisor is gone"),
            )?;
            sessions::close_goal_review(&tx, id, true)?;
        }
        let Some(fingerprint) = candidate(&tx, goal)? else {
            // Keep the rows finished above.
            tx.commit()?;
            return Ok(None);
        };
        let anchor = anchor(&tx, goal)?.context("a goal to review has a task")?;
        let attempt = tx.query_row(
            "SELECT count(*) + 1 FROM goal_reviews WHERE goal_id=?1 AND outcome IS NOT 'interrupted'",
            [goal],
            |r| r.get::<_, i64>(0),
        )? as usize;
        check_at_least("goal review attempt", attempt as i64, 1)?;
        let gaps = gaps_in_a_row(&tx, goal)?;
        tx.execute(
            "INSERT INTO goal_reviews(goal_id, attempt, supervisor_token, fingerprint, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![goal, attempt as i64, token, fingerprint, now],
        )?;
        let id = tx.last_insert_rowid();
        let dir = goal_reviews_dir.join(id.to_string());
        let dir_text = dir.to_str().context("goal review directory is not UTF-8")?;
        tx.execute(
            "UPDATE goal_reviews SET dir=?2 WHERE id=?1",
            params![id, dir_text],
        )?;
        // The job's Claude session id, given to it by the runtime (ADR-0048
        // decision 4); Codex names its thread itself, which the job's end
        // records (ADR-t1063-1 decision 6).
        let session_id = (launch.provider == crate::domain::Provider::Claude)
            .then(|| self.generators.ids.uuid());
        let cwd = cwd.to_str().context("repository checkout is not UTF-8")?;
        goal_event(
            &tx,
            goal,
            EventKind::GoalReviewStarted,
            json!({"goal_review_id": id, "attempt": attempt, "dir": dir_text, "tasks": fingerprint, "gaps_in_a_row": gaps, "session_id": session_id, "cwd": cwd, "launch": launch.to_value()}),
        )?;
        tx.commit()?;
        Ok(Some(GoalReviewJob {
            id,
            goal_id: goal,
            attempt,
            anchor,
            dir,
            gaps_in_a_row: gaps,
            session_id,
        }))
    }

    fn goal_reviews(&self, goal: GoalId) -> Result<Vec<GoalReviewRecord>> {
        Ok(self
            .conn
            .prepare(
                "SELECT id, attempt, outcome, verdict, error FROM goal_reviews
                 WHERE goal_id=?1 AND finished_at IS NOT NULL ORDER BY id",
            )?
            .query_map([goal], |r| {
                let verdict: Option<String> = r.get(3)?;
                Ok(GoalReviewRecord {
                    id: r.get(0)?,
                    attempt: r.get::<_, i64>(1)? as usize,
                    outcome: r.get(2)?,
                    // A verdict is stored as the JSON it was checked to be.
                    verdict: verdict.and_then(|v| serde_json::from_str(&v).ok()),
                    error: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn finish_goal_review(
        &mut self,
        job: &GoalReviewJob,
        token: &LeaseToken,
        apply: &GoalReviewApply,
    ) -> Result<GoalReviewApplied> {
        let now = self.generators.clock.now();
        let stamp = self.generators.clock.timestamp();
        // The spans it closes read their transcripts first (task 543).
        let _read = sessions::read_before(
            &self.conn,
            sessions::Closing::GoalReviews(Some(job.id), false),
        )?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !running(&tx, job, token)? {
            return Ok(GoalReviewApplied {
                stale: true,
                ..GoalReviewApplied::default()
            });
        }
        let verdict_json = serde_json::to_value(&apply.verdict)?;
        // What the job saw (the goal's tasks, acceptance version and
        // follow-ups with their judgements), unchanged and still closable,
        // and no one closed the goal meanwhile. A verdict collected across a
        // handoff (task 1425) is applied here too.
        let seen: String = tx.query_row(
            "SELECT fingerprint FROM goal_reviews WHERE id=?1",
            [job.id],
            |r| r.get(0),
        )?;
        let open: bool = tx.query_row(
            "SELECT status='open' AND closed_at IS NULL FROM goals WHERE id=?1",
            [job.goal_id],
            |r| r.get(0),
        )?;
        let (fingerprint, closable) = review_input(&tx, job.goal_id)?;
        if !open || fingerprint != seen || !closable {
            finish_row(
                &tx,
                job.id,
                now,
                "interrupted",
                Some(&verdict_json),
                Some("the goal or its tasks changed during its review"),
            )?;
            sessions::close_goal_review(&tx, job.id, false)?;
            tx.commit()?;
            return Ok(GoalReviewApplied {
                stale: true,
                ..GoalReviewApplied::default()
            });
        }
        let mut applied = GoalReviewApplied::default();
        match apply.decision {
            GoalReviewDecision::Achieved => {
                close_goal_in(
                    &tx,
                    job.goal_id,
                    GoalVerdict::Achieved,
                    &stamp,
                    json!({"by": "goal_review", "goal_review_id": job.id, "reason": apply.verdict.summary}),
                )?;
                applied.closed = true;
            }
            GoalReviewDecision::Gaps => {
                applied.gap_tasks = register_gaps(
                    &tx,
                    job.goal_id,
                    job.id,
                    &apply.verdict.gaps,
                    &apply.verdict.summary,
                    &stamp,
                    now,
                )?;
            }
            GoalReviewDecision::Ask => {
                let ask = apply
                    .ask
                    .as_ref()
                    .context("an ask verdict opens an approve_goal ask")?;
                ask.validate()?;
                let outcome = insert_ask(&tx, ask)?;
                tx.execute(
                    "UPDATE goal_reviews SET ask_id=?2 WHERE id=?1",
                    params![job.id, outcome.ask.id],
                )?;
                applied.ask = Some(outcome);
            }
        }
        finish_row(
            &tx,
            job.id,
            now,
            apply.decision.as_str(),
            Some(&verdict_json),
            None,
        )?;
        let mut finished = json!({
            "goal_review_id": job.id,
            "attempt": job.attempt,
            "verdict": apply.verdict.verdict,
            "decision": apply.decision,
            "overridden": apply.overridden,
            "criteria": apply.verdict.criteria,
            "summary": apply.verdict.summary,
            "gaps": apply.verdict.gaps,
            "gap_tasks": applied.gap_tasks,
            "ask_id": applied.ask.as_ref().map(|outcome| outcome.ask.id),
            "duration_secs": apply.duration_secs,
            "prompt_bytes": apply.prompt_bytes,
        });
        if let Some(session) = &apply.session {
            session.record(&mut finished);
        }
        goal_event(&tx, job.goal_id, EventKind::GoalReviewFinished, finished)?;
        tx.commit()?;
        Ok(applied)
    }

    fn fail_goal_review(
        &mut self,
        job: &GoalReviewJob,
        token: &LeaseToken,
        failure: &GoalReviewFailure,
    ) -> Result<()> {
        let now = self.generators.clock.now();
        // The span it closes reads its transcript first (task 543).
        let _read = sessions::read_before(
            &self.conn,
            sessions::Closing::GoalReviews(Some(job.id), false),
        )?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !running(&tx, job, token)? {
            return Ok(());
        }
        // A provider that could not be used leaves the goal to be reviewed
        // again at once, on the other provider (ADR-t1063-1 decision 4).
        let outcome = if failure.unusable.is_some() {
            "interrupted"
        } else {
            "failed"
        };
        finish_row(&tx, job.id, now, outcome, None, Some(&failure.error))?;
        let mut failed = json!({
            "code": crate::domain::ReasonCode::JobFailed,
            "goal_review_id": job.id,
            "attempt": job.attempt,
            "error": failure.error,
            "duration_secs": failure.duration_secs,
            "reason_category": crate::domain::AskReason::RecoveryFailed,
            "prompt_bytes": failure.prompt_bytes,
        });
        if let Some(session) = &failure.session {
            session.record(&mut failed);
        }
        if let Some((provider, reason)) = failure.unusable {
            failed["provider_unusable"] = json!({"provider": provider, "reason": reason});
        }
        goal_event(&tx, job.goal_id, EventKind::GoalReviewFailed, failed)?;
        tx.commit()?;
        Ok(())
    }

    fn goal_answers(&self) -> Result<Vec<Ask>> {
        let ids: Vec<AskId> = self
            .conn
            .prepare(
                "SELECT a.id FROM asks a WHERE a.kind='approve_goal' AND a.answered_at IS NOT NULL
                 AND a.closed_at IS NULL AND (SELECT json_extract(e.payload, '$.runtime_delivers')
                     FROM run_events e WHERE e.kind='ask_answered'
                     AND json_extract(e.payload, '$.ask_id') = a.id
                     ORDER BY e.id DESC LIMIT 1) = 1
                 ORDER BY a.id",
            )?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter().map(|id| read_ask(&self.conn, id)).collect()
    }

    fn decide_goal(&mut self, ask_id: AskId) -> Result<Option<GoalDecided>> {
        let now = self.generators.clock.now();
        let stamp = self.generators.clock.timestamp();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ask = read_ask(&tx, ask_id)?;
        ensure!(
            ask.kind == AskKind::ApproveGoal,
            "ask {ask_id} is not an approve_goal ask"
        );
        if ask.closed_at.is_some() {
            return Ok(None);
        }
        let text = ask.answer.clone().unwrap_or_default();
        let close = |tx: &Connection| -> Result<()> {
            tx.execute(
                "UPDATE asks SET closed_at=?2 WHERE id=?1",
                params![ask_id, now],
            )?;
            Ok(())
        };
        // A goal closed meanwhile no longer waits for the answer.
        if let Some((_, goal)) = ask_review(&tx, ask_id)? {
            let closed: bool = tx.query_row(
                "SELECT closed_at IS NOT NULL FROM goals WHERE id=?1",
                [goal],
                |r| r.get(0),
            )?;
            if closed {
                close(&tx)?;
                // Nothing else names the ask: `ask_closed` ends its wait
                // (task 568).
                record_ask_closed(&tx, &ask)?;
                tx.commit()?;
                return Ok(None);
            }
        }
        let Some((answer, review, goal)) = applicable(&tx, &ask, &text)? else {
            return Ok(None);
        };
        let mut decided = GoalDecided {
            goal_id: goal,
            answer: text.trim().to_owned(),
            closed: None,
            gap_tasks: Vec::new(),
        };
        let by = json!({"by": "person", "ask_id": ask_id, "goal_review_id": review});
        match &answer {
            GoalAnswer::Achieved | GoalAnswer::Abandoned => {
                let verdict = answer.closes().context("the answer closes the goal")?;
                close_goal_in(&tx, goal, verdict, &stamp, by)?;
                decided.closed = Some(verdict);
            }
            GoalAnswer::Gaps(what) => {
                let gaps = match what {
                    Some(what) => vec![goal_review::person_gap(what)],
                    None => review_gaps(&tx, review)?,
                };
                decided.gap_tasks = register_gaps(
                    &tx,
                    goal,
                    review,
                    &gaps,
                    &format!("a person answered ask {ask_id} with gaps"),
                    &stamp,
                    now,
                )?;
            }
            // A rearm left from before the ask would review it again at
            // once: keep_open waits for the tasks to change.
            GoalAnswer::KeepOpen => {
                tx.execute(
                    "UPDATE goal_reviews SET rearmed_at=NULL WHERE goal_id=?1",
                    [goal],
                )?;
            }
        }
        close(&tx)?;
        goal_event(
            &tx,
            goal,
            EventKind::GoalDecided,
            json!({
                "ask_id": ask_id,
                "goal_review_id": review,
                "answer": decided.answer,
                "decision": answer.as_str(),
                "verdict": decided.closed,
                "gap_tasks": decided.gap_tasks,
            }),
        )?;
        tx.commit()?;
        Ok(Some(decided))
    }

    fn applies_goal_answer(&self, ask: &Ask) -> Result<bool> {
        match &ask.answer {
            Some(text) => goal_answer_applies(&self.conn, ask, text),
            None => Ok(false),
        }
    }

    fn correction_answers(&self) -> Result<Vec<Ask>> {
        follow_up_membership::correction_answers(&self.conn)
    }

    fn decide_correction(&mut self, ask: AskId) -> Result<Option<Value>> {
        let stamp = self.generators.clock.timestamp();
        let now = self.generators.clock.now();
        follow_up_membership::decide_correction(&mut self.conn, ask, stamp, now)
    }

    fn applies_correction_answer(&self, ask: &Ask) -> Result<bool> {
        match &ask.answer {
            Some(text) => follow_up_membership::correction_answer_applies(&self.conn, ask, text),
            None => Ok(false),
        }
    }

    fn goal_review_holds(&self) -> Result<Vec<GoalReviewHold>> {
        let goals: Vec<GoalId> = self
            .conn
            .prepare("SELECT id FROM goals WHERE status='open' AND closed_at IS NULL ORDER BY id")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut holds = Vec::new();
        for goal in goals {
            let Some(row) = latest(&self.conn, goal)? else {
                continue;
            };
            // Its input is read only for a failed review nobody rearmed.
            if row.1 != "failed" || row.3 {
                continue;
            }
            let fingerprint = review_input(&self.conn, goal)?.0;
            if !goal_review::waits_for_a_person(Some(latest_review(&row)), &fingerprint) {
                continue;
            }
            let id = row.0;
            let error: Option<String> =
                self.conn
                    .query_row("SELECT error FROM goal_reviews WHERE id=?1", [id], |r| {
                        r.get(0)
                    })?;
            holds.push(GoalReviewHold {
                goal_id: goal,
                anchor: anchor(&self.conn, goal)?,
                error,
            });
        }
        Ok(holds)
    }

    fn goal_follow_ups(&self) -> Result<Vec<follow_up::GoalFollowUps>> {
        let goals: Vec<GoalId> = self
            .conn
            .prepare("SELECT id FROM goals WHERE status='open' AND closed_at IS NULL ORDER BY id")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut found = Vec::new();
        for goal in goals {
            let follow_ups = follow_up_membership::waiting_follow_ups(&self.conn, goal)?;
            if follow_ups.is_empty() {
                continue;
            }
            found.push(follow_up::GoalFollowUps {
                goal,
                tasks: goal_tasks(&self.conn, goal)?,
                follow_ups,
            });
        }
        Ok(found)
    }

    fn rearm_goal_review(&mut self, goal: GoalId) -> Result<Value> {
        let now = self.generators.clock.now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let open: Option<bool> = tx
            .query_row(
                "SELECT status='open' AND closed_at IS NULL FROM goals WHERE id=?1",
                [goal],
                |r| r.get(0),
            )
            .optional()?;
        goal_review::rearmable(goal, open).map_err(anyhow::Error::msg)?;
        let rearmed = match latest(&tx, goal)? {
            Some((id, ..)) => {
                tx.execute(
                    "UPDATE goal_reviews SET rearmed_at=?2 WHERE id=?1",
                    params![id, now],
                )?;
                Some(id)
            }
            None => None,
        };
        goal_event(
            &tx,
            goal,
            EventKind::GoalReviewRearmed,
            json!({"goal_review_id": rearmed}),
        )?;
        let reviewable = candidate(&tx, goal)?.is_some();
        tx.commit()?;
        Ok(json!({
            "goal_id": goal,
            "rearmed_goal_review_id": rearmed,
            "reviewable": reviewable,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A blank outcome is refused before the write (ADR-t876-1: the rule
    /// the `goal_reviews` CHECK held).
    #[test]
    fn a_blank_outcome_is_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let error = finish_row(&queue.conn, 1, 0, " ", None, None).unwrap_err();
        assert_eq!(error.to_string(), "goal review outcome must not be blank");
    }
}
