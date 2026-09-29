//! Asks (ADR-0022): questions for a person kept as queue rows. Registering
//! and answering one also writes `ask_opened` / `ask_answered` to
//! `run_events`, so the change rides the cursor `status` and `watch` hand out.
use crate::domain::event_kind::EventKind;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::json;

use super::adapters::process_alive;
use super::sqlite::{SqliteQueue, enum_col, json_col};
use crate::domain::Ask;
use crate::domain::write_rules::check_run_has_task;
use crate::domain::{
    APPROVE_RELEASE_OPTIONS, AnswerAuthority, Answerer, AskId, AskKind, AskOutcome, AskReason,
    HoldOutcome, LANDING_OPTIONS, NewAsk, NewHold, RunId, RunStatus, TaskId, UPDATE_FAILED_OPTIONS,
    answer_approves, check_ask_kind, check_event_target, finding, option_index,
    session_takes_answers,
};

pub use crate::application::AskQuery;

impl SqliteQueue {
    /// Register an ask, or return the open one of the same task, run and
    /// kind unchanged. A new ask writes `ask_opened` (with the run when it
    /// has one) in the same transaction. A `blocked` ask may name neither a
    /// task nor a run: the observer's threshold that belongs to no task.
    pub fn ask(&mut self, ask: NewAsk) -> Result<AskOutcome> {
        ask.validate()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome = insert_ask(&tx, &ask)?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Open the `queue_hold` ask of the hold's reason and subject with its
    /// run or headless job, or add it to the open one (ADR-0047 decision
    /// 42). A new ask writes `ask_opened` on the queue; a run or job that
    /// joins an open one rewrites its question's list and writes
    /// `ask_updated` on the run (a job's run, or the queue for a job with
    /// none, task 438). One already in it changes nothing, nor does a hold
    /// without a run or job while the ask is open (task 377).
    pub fn hold(&mut self, hold: NewHold) -> Result<HoldOutcome> {
        hold.validate()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task_id: Option<TaskId> = match &hold.run_id {
            Some(run_id) => Some(
                tx.query_row("SELECT task_id FROM task_runs WHERE id=?1", [run_id], |r| {
                    r.get(0)
                })
                .optional()?
                .with_context(|| format!("run {run_id} does not exist"))?,
            ),
            None => None,
        };
        let open = tx
            .query_row(
                "SELECT * FROM asks WHERE kind='queue_hold' AND reason_category=?1
                 AND ifnull(subject,'')=ifnull(?2,'')
                 AND answered_at IS NULL AND closed_at IS NULL",
                params![hold.reason_category.as_str(), hold.subject],
                ask_row,
            )
            .optional()?;
        let entry = hold.entry();
        let outcome = match (open, entry) {
            (Some(ask), None) => HoldOutcome {
                ask,
                created: false,
                joined: false,
            },
            (Some(ask), Some(entry)) if ask.affected.contains(&entry) => HoldOutcome {
                ask,
                created: false,
                joined: false,
            },
            (Some(ask), Some(entry)) => {
                let mut affected = ask.affected.clone();
                affected.push(entry.clone());
                let base = NewHold::base_question(&ask.question);
                tx.execute(
                    "UPDATE asks SET affected=?2, question=?3 WHERE id=?1",
                    params![
                        ask.id,
                        serde_json::to_string(&affected)?,
                        NewHold::question_for(base, &affected)
                    ],
                )?;
                ask_event(
                    &tx,
                    task_id,
                    hold.run_id.as_ref(),
                    EventKind::AskUpdated,
                    json!({
                        "ask_id": ask.id,
                        "kind": ask.kind,
                        "reason_category": ask.reason_category,
                        "affected": affected,
                        "joined": entry,
                    }),
                )?;
                HoldOutcome {
                    ask: read_ask(&tx, ask.id)?,
                    created: false,
                    joined: true,
                }
            }
            (None, entry) => {
                let affected: Vec<String> = entry.into_iter().collect();
                let kind = AskKind::QueueHold;
                check_ask_kind(&kind, None, None, hold.reason_category)?;
                tx.execute(
                    "INSERT INTO asks(kind,question,options,asked_by,reason_category,subject,affected)
                     VALUES (?7,?1,?2,?3,?4,?5,?6)",
                    params![
                        NewHold::question_for(&hold.question, &affected),
                        serde_json::to_string(&hold.options)?,
                        hold.asked_by,
                        hold.reason_category.as_str(),
                        hold.subject,
                        serde_json::to_string(&affected)?,
                        kind.as_str(),
                    ],
                )?;
                let id = AskId::new(tx.last_insert_rowid());
                ask_event(
                    &tx,
                    None,
                    None,
                    EventKind::AskOpened,
                    json!({
                        "ask_id": id,
                        "kind": AskKind::QueueHold,
                        "asked_by": hold.asked_by,
                        "reason_category": hold.reason_category,
                        "affected": affected,
                    }),
                )?;
                HoldOutcome {
                    ask: read_ask(&tx, id)?,
                    created: true,
                    joined: hold.entry().is_some(),
                }
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// The open `queue_hold` ask that holds the run's session, if any. The
    /// disk's `cost` ask (task 377) lists the runs whose landing waited,
    /// and holds no session: it is not one.
    pub fn hold_of(&self, run_id: &RunId) -> Result<Option<Ask>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM asks WHERE kind='queue_hold'
                 AND ifnull(subject,'') <> 'disk'
                 AND answered_at IS NULL AND closed_at IS NULL
                 AND EXISTS (SELECT 1 FROM json_each(asks.affected) WHERE value=?1)
                 ORDER BY id LIMIT 1",
                [run_id],
                ask_row,
            )
            .optional()?)
    }

    /// Whether a `queue_hold` ask (not the disk's) that holds the run is
    /// unclosed: open, or answered and not yet applied.
    pub fn hold_unclosed(&self, run_id: &RunId) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM asks WHERE kind='queue_hold'
             AND ifnull(subject,'') <> 'disk' AND closed_at IS NULL
             AND EXISTS (SELECT 1 FROM json_each(asks.affected) WHERE value=?1))",
            [run_id],
            |row| row.get(0),
        )?)
    }

    /// A person's answer from a terminal with no `DAGQ_ROLE`: see
    /// [`Self::answer_as`].
    pub fn answer(&mut self, id: AskId, text: &str) -> Result<Ask> {
        self.answer_as(id, text, Answerer::PERSON)
    }

    /// Write the answer of an open ask, who gave it (`answered_by`), the
    /// authority it carries, whether it approves, and the option it chose,
    /// and record `ask_answered` (with the run when the ask has one)
    /// carrying them.
    pub fn answer_as(&mut self, id: AskId, text: &str, answerer: Answerer) -> Result<Ask> {
        ensure!(!text.trim().is_empty(), "answer must not be blank");
        // Read before the transaction: the host's `[update]` is a file.
        let releases = self.release_updates_on();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ask = read_ask(&tx, id)?;
        ensure!(ask.is_open(), "ask {id} is not open");
        let now = self.generators.clock.now();
        let mut payload =
            json!({"ask_id": id, "kind": ask.kind, "reason_category": ask.reason_category});
        write_answer(&tx, &ask, text, answerer, now, &mut payload)?;
        // A `propose` or `dismiss` answer the ask offered is applied to its
        // finding here (ADR-0044 decision 19): nobody has to carry it. The
        // observer's `blocked` ask is done with it; a `stalled` ask stays
        // for the supervisor that watches its run.
        if let Some(applied) =
            super::finding_planners::apply_answer(&tx, &ask, text, answerer.by, now)?
        {
            payload["finding_id"] = json!(applied.finding);
            payload["finding_applied"] = json!(applied.action);
            payload["runtime_delivers"] = json!(true);
            ask_event(
                &tx,
                ask.task_id,
                ask.run_id.as_ref(),
                EventKind::AskAnswered,
                payload,
            )?;
            if ask.kind == AskKind::Blocked {
                tx.execute("UPDATE asks SET closed_at=?2 WHERE id=?1", params![id, now])?;
                // Applied: `stats` reads `ask_closed` as the answer applied.
                ask_event(
                    &tx,
                    ask.task_id,
                    ask.run_id.as_ref(),
                    EventKind::AskClosed,
                    json!({"ask_id": id, "kind": ask.kind}),
                )?;
            }
            let answered = read_ask(&tx, id)?;
            tx.commit()?;
            return Ok(answered);
        }
        if ask.kind == AskKind::WorkerQuestion
            && let Some(run_id) = ask.run_id.as_ref()
        {
            // The supervisor types it into a running worker's terminal, or
            // into a live session it asked to revise (task 238); the answer
            // of a run that stopped running is the inbox's. A stale lease
            // (its supervisor died) delivers nothing, as `status` reads it
            // (task 584).
            let status: String =
                tx.query_row("SELECT status FROM task_runs WHERE id=?1", [run_id], |r| {
                    r.get(0)
                })?;
            let status: RunStatus = status.parse()?;
            let leased = super::runtime_store::run_lease_of(&tx, run_id)?
                .is_some_and(|lease| !super::runtime_store::lease_is_stale(&lease, now));
            payload["runtime_delivers"] = json!(
                status == RunStatus::Running
                    || (leased
                        && session_takes_answers(
                            status,
                            &super::runtime_store::run_events_of(&tx, run_id)?,
                            ask.created_at,
                        ))
            );
        }
        if ask.kind == AskKind::ApproveLanding
            && let Some(run_id) = ask.run_id.as_ref()
        {
            // The supervisor lands, sends back or cancels a run awaiting
            // integration as answered (ADR-0027); any other answer, or one
            // for a run that moved on, is the inbox's to read.
            let status: String =
                tx.query_row("SELECT status FROM task_runs WHERE id=?1", [run_id], |r| {
                    r.get(0)
                })?;
            payload["runtime_delivers"] = json!(
                status == RunStatus::AwaitingIntegration.as_str()
                    && LANDING_OPTIONS.contains(&text.trim())
            );
        }
        if ask.kind == AskKind::Decide
            && ask.asked_by == super::runtime_store::TRIAGE_ASKER
            && let Some(run_id) = ask.run_id.as_ref()
        {
            // The supervisor retries, resumes or cancels a recovered run as
            // answered, and hands any other option the ask offered (the
            // recovery job's own) back to the recovery job (ADR-0047
            // decision 40); a free answer, or one for a run that moved on,
            // is a person's to read.
            let status: String =
                tx.query_row("SELECT status FROM task_runs WHERE id=?1", [run_id], |r| {
                    r.get(0)
                })?;
            payload["runtime_delivers"] = json!(
                (status == RunStatus::Failed.as_str() || status == RunStatus::Interrupted.as_str())
                    && ask.options.iter().any(|option| option == text.trim())
            );
        }
        if ask.kind == AskKind::PlannerQuestion {
            // The supervisor types it into the workspace of the runtime's
            // planner that works on its task, or opens one for a draft that
            // still waits (ADR-0041 decision 13); otherwise it is the
            // inbox's to deliver.
            let answered = Ask {
                answer: Some(text.to_owned()),
                ..ask.clone()
            };
            payload["runtime_delivers"] = json!(
                super::draft_planners::route_of(&tx, &answered)?
                    != crate::application::PlannerAnswerRoute::Person
            );
        }
        if ask.kind == AskKind::ApprovePlan {
            // The supervisor readies, sends back or cancels the proposal as
            // answered (ADR-0041 decision 11); any other answer, or one for
            // a proposal that moved on, is a person's to read.
            payload["runtime_delivers"] =
                json!(super::plan_reviews::plan_answer_applies(&tx, &ask, text)?);
        }
        if ask.kind == AskKind::ApproveGoal {
            // The supervisor closes the goal, registers its gaps or leaves
            // it open as answered (ADR-0047 decision 43); any other answer,
            // or one that cannot be applied now, is a person's to read.
            payload["runtime_delivers"] =
                json!(super::goal_reviews::goal_answer_applies(&tx, &ask, text)?);
        }
        if ask.kind == AskKind::QueueHold {
            // The supervisor applies an option the ask offered: `done` /
            // `cancel_affected` of an authentication or usage-limit ask
            // (task 437), `done` / `wait` of the disk's (task 377); any
            // other answer is a person's to read.
            payload["runtime_delivers"] =
                json!(crate::domain::queue_hold::applies(&ask.options, text));
        }
        if ask.kind == AskKind::UpdateFailed {
            // The live supervisor that updates the binary retries or leaves
            // the update as answered (ADR-0045 decision 17, ADR-t618-1), by
            // the rule `status` reports it with; any other answer, or one
            // nobody updates for, is a person's to read.
            let now = self.generators.clock.now();
            // The failure of a release's job is a release supervisor's to
            // apply, a build's one with the automatic update.
            let source: Option<String> = tx
                .query_row(
                    "SELECT json_extract(payload,'$.source') FROM run_events
                     WHERE kind=?1 AND json_extract(payload,'$.ask_id')=?2
                     ORDER BY id DESC LIMIT 1",
                    params![crate::domain::UPDATE_FAILED, id],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            let release = source.as_deref() == Some(crate::application::update::RELEASE_SOURCE);
            let applied = UPDATE_FAILED_OPTIONS.contains(&text.trim())
                && super::runtime_store::supervisors_of(&tx)?
                    .iter()
                    .any(|registration| {
                        if release {
                            registration.applies_releases(now, process_alive, releases)
                        } else {
                            registration.applies_updates(now, process_alive)
                        }
                    });
            payload["runtime_delivers"] = json!(applied);
        }
        if ask.kind == AskKind::ApproveRelease {
            // A live supervisor of a release build installs or skips the
            // release as answered (ADR-t618-1 decision 4); without one, a
            // person reads it and runs `install --release`.
            let now = self.generators.clock.now();
            let applied = APPROVE_RELEASE_OPTIONS.contains(&text.trim())
                && super::runtime_store::supervisors_of(&tx)?
                    .iter()
                    .any(|registration| {
                        registration.applies_releases(now, process_alive, releases)
                    });
            payload["runtime_delivers"] = json!(applied);
        }
        ask_event(
            &tx,
            ask.task_id,
            ask.run_id.as_ref(),
            EventKind::AskAnswered,
            payload,
        )?;
        let answered = read_ask(&tx, id)?;
        tx.commit()?;
        Ok(answered)
    }

    /// Whether the host's `[update]` of this queue looks for releases
    /// (ADR-t618-1): read from `host.toml` next to the database and the
    /// host-wide one.
    pub fn release_updates_on(&self) -> bool {
        self.runs_dir
            .parent()
            .is_some_and(super::release_update::releases_on)
    }

    /// Mark an answered ask read (by the inbox, once the person acted on it,
    /// or by the supervisor, once it applied the answer), and record
    /// `ask_closed`, which `stats` reads as the answer applied (task 468). An
    /// open ask cannot be closed: `ask_answered` is the one event that ends
    /// an ask in `run_events` (what `stats` pairs with `ask_opened`), so an
    /// ask is withdrawn by answering it.
    pub fn close_ask(&mut self, id: AskId) -> Result<Ask> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ask = read_ask(&tx, id)?;
        ensure!(ask.closed_at.is_none(), "ask {id} is already closed");
        ensure!(
            ask.answered_at.is_some(),
            "ask {id} is not answered yet; answer it (for example that it is withdrawn) before closing it"
        );
        tx.execute(
            "UPDATE asks SET closed_at=?2 WHERE id=?1",
            params![id, self.generators.clock.now()],
        )?;
        ask_event(
            &tx,
            ask.task_id,
            ask.run_id.as_ref(),
            EventKind::AskClosed,
            json!({"ask_id": id, "kind": ask.kind}),
        )?;
        let closed = read_ask(&tx, id)?;
        tx.commit()?;
        Ok(closed)
    }

    /// Asks matching `query`, oldest first. A pure read.
    pub fn asks(&self, query: AskQuery) -> Result<Vec<Ask>> {
        let asks: Vec<Ask> = self
            .conn
            .prepare(
                "SELECT * FROM asks WHERE (?1 OR closed_at IS NULL)
                 AND (NOT ?2 OR answered_at IS NULL) ORDER BY id",
            )?
            .query_map(params![query.all, query.open], ask_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(asks
            .into_iter()
            .filter(|ask| query.role.is_none() || ask.waits_for() == query.role)
            .collect())
    }

    /// Open the ask of the automatic update of `kind` (ADR-0073 decision
    /// 17): `update_failed` or `approve_update`, about no task or run. One
    /// of the same kind still open is about an older build, so it is
    /// answered `superseded` and closed first (by the runtime, which writes
    /// `ask_answered` with `runtime_closed`). Writes `ask_opened` like any
    /// ask.
    pub fn open_update_ask(
        &mut self,
        kind: AskKind,
        question: &str,
        options: &[&str],
        asked_by: &str,
        subject: Option<&str>,
    ) -> Result<Ask> {
        ensure!(!question.trim().is_empty(), "question must not be blank");
        ensure!(
            kind.is_update(),
            "a {kind} ask is not one of the automatic update"
        );
        let reason = AskReason::Scope;
        check_ask_kind(&kind, None, None, reason)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let open: Vec<Ask> = tx
            .prepare(
                "SELECT * FROM asks WHERE kind=?1 AND task_id IS NULL
                 AND answered_at IS NULL AND closed_at IS NULL ORDER BY id",
            )?
            .query_map([kind.as_str()], ask_row)?
            .collect::<rusqlite::Result<_>>()?;
        for ask in open {
            let mut payload = json!({"ask_id": ask.id, "kind": ask.kind, "runtime_closed": true});
            write_answer(
                &tx,
                &ask,
                "superseded",
                Answerer::RUNTIME,
                now,
                &mut payload,
            )?;
            tx.execute(
                "UPDATE asks SET closed_at=?2 WHERE id=?1",
                params![ask.id, now],
            )?;
            ask_event(&tx, None, None, EventKind::AskAnswered, payload)?;
        }
        tx.execute(
            "INSERT INTO asks(kind,question,options,asked_by,reason_category,subject)
             VALUES (?5,?1,?2,?3,?4,?6)",
            params![
                question,
                serde_json::to_string(options)?,
                asked_by,
                reason.as_str(),
                kind.as_str(),
                subject
            ],
        )?;
        let id = AskId::new(tx.last_insert_rowid());
        ask_event(
            &tx,
            None,
            None,
            EventKind::AskOpened,
            json!({
                "ask_id": id,
                "kind": kind,
                "asked_by": asked_by,
                "reason_category": reason,
            }),
        )?;
        let opened = read_ask(&tx, id)?;
        tx.commit()?;
        Ok(opened)
    }

    /// The asks of the automatic update of `kind` that were answered and
    /// nobody closed yet, oldest first: answers the supervisor still has to
    /// apply.
    pub fn update_answers(&self, kind: &AskKind) -> Result<Vec<Ask>> {
        Ok(self
            .conn
            .prepare(
                "SELECT * FROM asks WHERE kind=?1 AND task_id IS NULL
                 AND answered_at IS NOT NULL AND closed_at IS NULL ORDER BY id",
            )?
            .query_map([kind.as_str()], ask_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The answered `worker_question` asks of a run that nobody closed yet,
    /// oldest first: answers the supervisor still has to type into the
    /// worker's terminal.
    pub fn undelivered_answers(&self, run_id: &RunId) -> Result<Vec<Ask>> {
        Ok(self
            .conn
            .prepare(
                "SELECT * FROM asks WHERE run_id=?1 AND kind='worker_question'
                 AND answered_at IS NOT NULL AND closed_at IS NULL ORDER BY id",
            )?
            .query_map([run_id], ask_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Whether the run has a `worker_question` nobody closed, answered or
    /// not: its worker stopped at the ask and waits for the answer.
    pub fn has_unclosed_worker_question(&self, run_id: &RunId) -> Result<bool> {
        self.has_unclosed_ask(run_id, AskKind::WorkerQuestion)
    }

    /// Whether the run has a `worker_question` nobody closed that was
    /// created at or after `created_from` (unix seconds).
    pub fn has_unclosed_worker_question_since(
        &self,
        run_id: &RunId,
        created_from: i64,
    ) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM asks WHERE run_id=?1 AND kind='worker_question'
             AND closed_at IS NULL AND created_at>=?2)",
            params![run_id, created_from],
            |r| r.get(0),
        )?)
    }

    /// When the run's `worker_question` closed last (unix seconds): its
    /// answer was delivered, by the supervisor or by hand.
    pub fn last_worker_question_closed(&self, run_id: &RunId) -> Result<Option<i64>> {
        Ok(self.conn.query_row(
            "SELECT MAX(closed_at) FROM asks WHERE run_id=?1 AND kind='worker_question'",
            [run_id],
            |r| r.get(0),
        )?)
    }

    /// Whether the run has an ask of `kind` nobody closed, answered or not.
    pub fn has_unclosed_ask(&self, run_id: &RunId, kind: AskKind) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM asks WHERE run_id=?1 AND kind=?2
             AND closed_at IS NULL)",
            params![run_id, kind.as_str()],
            |r| r.get(0),
        )?)
    }

    /// The answered `approve_landing` asks about a run that nobody closed,
    /// oldest first: the answers the supervisor applies (ADR-0027).
    pub fn landing_answers(&self) -> Result<Vec<Ask>> {
        Ok(self
            .conn
            .prepare(
                "SELECT * FROM asks WHERE kind='approve_landing' AND run_id IS NOT NULL
                 AND answered_at IS NOT NULL AND closed_at IS NULL ORDER BY id",
            )?
            .query_map([], ask_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Close an answered `worker_question` whose answer was typed into the
    /// worker's terminal, and record `ask_delivered` in the same transaction.
    /// An ask someone closed meanwhile is left as it is.
    pub fn ask_delivered(&mut self, id: AskId, workspace_id: &str) -> Result<Ask> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ask = read_ask(&tx, id)?;
        if ask.closed_at.is_some() {
            return Ok(ask);
        }
        ensure!(ask.answered_at.is_some(), "ask {id} is not answered");
        tx.execute(
            "UPDATE asks SET closed_at=?2 WHERE id=?1",
            params![id, self.generators.clock.now()],
        )?;
        ask_event(
            &tx,
            ask.task_id,
            ask.run_id.as_ref(),
            EventKind::AskDelivered,
            json!({"ask_id": id, "workspace_id": workspace_id}),
        )?;
        let closed = read_ask(&tx, id)?;
        tx.commit()?;
        Ok(closed)
    }

    /// Close every `stuck_exit` ask of the run nobody closed: its session
    /// exited, so nobody needs to answer it any more. An open one is
    /// answered with `answer` first and records `ask_answered` with
    /// `runtime_closed: true` (the one event that ends an ask, which is
    /// no attention); an answered one is only closed, like `ask close`.
    /// Returns the asks it closed, oldest first.
    pub fn close_stuck_exit_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        self.close_runtime_asks(run_id, AskKind::StuckExit, answer)
    }

    /// Close every `answer_prompt` ask of the run nobody closed, the way
    /// [`Self::close_stuck_exit_asks`] does: the dialog it was about is gone
    /// (or the session ended), so nobody needs to answer it any more.
    pub fn close_answer_prompt_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        self.close_runtime_asks(run_id, AskKind::AnswerPrompt, answer)
    }

    /// The run's `stalled` ask nobody closed, answered or not: at most one
    /// is open, and an answered one the supervisor has not applied yet
    /// comes first (ADR-0043 decision 1).
    pub fn unclosed_stalled_ask(&self, run_id: &RunId) -> Result<Option<Ask>> {
        Ok(self
            .conn
            .prepare(
                "SELECT * FROM asks WHERE run_id=?1 AND kind='stalled' AND closed_at IS NULL
                 ORDER BY id LIMIT 1",
            )?
            .query_map([run_id], ask_row)?
            .next()
            .transpose()?)
    }

    /// Close every `stalled` ask of the run nobody closed, the way
    /// [`Self::close_stuck_exit_asks`] does: the session moved on or ended,
    /// so nobody needs to answer it any more.
    pub fn close_stalled_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        self.close_runtime_asks(run_id, AskKind::Stalled, answer)
    }

    /// Close every `stalled` ask of the run nobody closed, with `answer`,
    /// and end each stalled detection of the run with no end recorded
    /// (`stall_resolved` of outcome `run_ended`, once): the run ended with
    /// no watch of its own to end them (the sweep).
    pub fn end_stalled_detections(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let closed = super::runtime_store::end_stalled_detections(&tx, run_id, answer, now)?;
        tx.commit()?;
        Ok(closed)
    }

    /// Close every `approve_landing` ask of the run nobody closed, the way
    /// [`Self::close_stuck_exit_asks`] does: a later review of the run
    /// asks afresh (task 328, task 425) or the run was integrated
    /// (task 425), so an earlier question, or an answer to it not applied
    /// yet, no longer fits the run.
    pub fn close_approve_landing_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>> {
        self.close_runtime_asks(run_id, AskKind::ApproveLanding, answer)
    }

    /// Close every `blocked` ask of the run, or of its task, nobody closed,
    /// the way [`Self::close_stuck_exit_asks`] does: the run was integrated
    /// and its task completed, so what the observer asked about is over
    /// (task 329). Its finding, if any, stays as it is.
    pub fn close_blocked_asks(
        &mut self,
        run_id: &RunId,
        task_id: TaskId,
        answer: &str,
    ) -> Result<Vec<Ask>> {
        self.close_asks_of(run_id, Some(task_id), AskKind::Blocked, answer)
    }

    /// Add `note` as a paragraph to the question of every ask of the run
    /// nobody closed, answered or not, and record `ask_updated` (with the
    /// ask, its kind and `why`) on the run for each: what the person reads
    /// before answering now includes it (ADR-0068 decision 4). Returns the
    /// asks as noted, oldest first.
    pub fn note_on_asks(&mut self, run_id: &RunId, note: &str, why: &str) -> Result<Vec<Ask>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let unclosed: Vec<Ask> = tx
            .prepare("SELECT * FROM asks WHERE run_id=?1 AND closed_at IS NULL ORDER BY id")?
            .query_map([run_id], ask_row)?
            .collect::<rusqlite::Result<_>>()?;
        let mut noted = Vec::with_capacity(unclosed.len());
        for ask in unclosed {
            tx.execute(
                "UPDATE asks SET question=?2 WHERE id=?1",
                params![ask.id, format!("{}\n\n{note}", ask.question)],
            )?;
            ask_event(
                &tx,
                ask.task_id,
                Some(run_id),
                EventKind::AskUpdated,
                json!({"ask_id": ask.id, "kind": ask.kind, "why": why}),
            )?;
            noted.push(read_ask(&tx, ask.id)?);
        }
        tx.commit()?;
        Ok(noted)
    }

    /// Close the `queue_hold` asks of `reason` and `subject` nobody closed
    /// (task 377): an open one is answered `answer` by the runtime
    /// (`ask_answered` with `runtime_closed`), an answered one's answer is
    /// applied by this close (`ask_closed`). The asks closed.
    pub fn close_hold_asks(
        &mut self,
        reason: AskReason,
        subject: Option<&str>,
        answer: &str,
    ) -> Result<Vec<Ask>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let unclosed: Vec<Ask> = tx
            .prepare(
                "SELECT * FROM asks WHERE kind='queue_hold' AND reason_category=?1
                 AND ifnull(subject,'')=ifnull(?2,'') AND closed_at IS NULL ORDER BY id",
            )?
            .query_map(params![reason.as_str(), subject], ask_row)?
            .collect::<rusqlite::Result<_>>()?;
        let now = self.generators.clock.now();
        let mut closed = Vec::with_capacity(unclosed.len());
        for ask in unclosed {
            if ask.is_open() {
                let mut payload =
                    json!({"ask_id": ask.id, "kind": ask.kind, "runtime_closed": true});
                write_answer(&tx, &ask, answer, Answerer::RUNTIME, now, &mut payload)?;
                ask_event(&tx, None, None, EventKind::AskAnswered, payload)?;
            } else {
                ask_event(
                    &tx,
                    None,
                    None,
                    EventKind::AskClosed,
                    json!({"ask_id": ask.id, "kind": ask.kind}),
                )?;
            }
            tx.execute(
                "UPDATE asks SET closed_at=?2 WHERE id=?1",
                params![ask.id, now],
            )?;
            closed.push(read_ask(&tx, ask.id)?);
        }
        tx.commit()?;
        Ok(closed)
    }

    fn close_runtime_asks(
        &mut self,
        run_id: &RunId,
        kind: AskKind,
        answer: &str,
    ) -> Result<Vec<Ask>> {
        self.close_asks_of(run_id, None, kind, answer)
    }

    /// Close the asks of `kind` nobody closed that are about the run, or
    /// about `task_id` with no run named, recording each on the run and
    /// task its `ask_opened` named.
    fn close_asks_of(
        &mut self,
        run_id: &RunId,
        task_id: Option<TaskId>,
        kind: AskKind,
        answer: &str,
    ) -> Result<Vec<Ask>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let closed = close_asks_in(&tx, run_id, task_id, kind, answer, now)?;
        tx.commit()?;
        Ok(closed)
    }

    pub fn read_ask(&self, id: AskId) -> Result<Ask> {
        read_ask(&self.conn, id)
    }
}

/// [`SqliteQueue::close_asks_of`] inside the caller's write transaction,
/// at `now`: an open ask gets `answer` from the runtime (`ask_answered`
/// with `runtime_closed: true`), an answered one `ask_closed`.
pub(super) fn close_asks_in(
    tx: &Connection,
    run_id: &RunId,
    task_id: Option<TaskId>,
    kind: AskKind,
    answer: &str,
    now: i64,
) -> Result<Vec<Ask>> {
    let unclosed: Vec<Ask> = tx
        .prepare(
            "SELECT * FROM asks WHERE (run_id=?1 OR (run_id IS NULL AND task_id=?3))
             AND kind=?2 AND closed_at IS NULL ORDER BY id",
        )?
        .query_map(params![run_id, kind.as_str(), task_id], ask_row)?
        .collect::<rusqlite::Result<_>>()?;
    let mut closed = Vec::with_capacity(unclosed.len());
    for ask in unclosed {
        if ask.is_open() {
            let mut payload = json!({"ask_id": ask.id, "kind": ask.kind, "runtime_closed": true});
            write_answer(tx, &ask, answer, Answerer::RUNTIME, now, &mut payload)?;
            ask_event(
                tx,
                ask.task_id,
                ask.run_id.as_ref(),
                EventKind::AskAnswered,
                payload,
            )?;
        } else {
            // An answer given before is applied by this close.
            ask_event(
                tx,
                ask.task_id,
                ask.run_id.as_ref(),
                EventKind::AskClosed,
                json!({"ask_id": ask.id, "kind": ask.kind}),
            )?;
        }
        tx.execute(
            "UPDATE asks SET closed_at=?2 WHERE id=?1",
            params![ask.id, now],
        )?;
        closed.push(read_ask(tx, ask.id)?);
    }
    Ok(closed)
}

/// Register `ask` inside the caller's write transaction, or return the open
/// one of the same task, run and kind unchanged (see [`SqliteQueue::ask`]).
pub(super) fn insert_ask(tx: &Connection, ask: &NewAsk) -> Result<AskOutcome> {
    let task_id = match (&ask.run_id, ask.task_id) {
        (Some(run_id), _) => tx
            .query_row("SELECT task_id FROM task_runs WHERE id=?1", [run_id], |r| {
                r.get::<_, TaskId>(0)
            })
            .optional()?
            .with_context(|| format!("run {run_id} does not exist"))
            .map(Some)?,
        (None, Some(task_id)) => {
            ensure!(
                tx.query_row("SELECT count(*) FROM tasks WHERE id=?1", [task_id], |r| r
                    .get::<_, i64>(
                    0
                ))? == 1,
                "task {task_id} does not exist"
            );
            Some(task_id)
        }
        // `validate` admits this for a blocked ask only.
        (None, None) => None,
    };
    check_ask_kind(&ask.kind, task_id, ask.run_id.as_ref(), ask.reason_category)?;
    check_run_has_task(task_id, ask.run_id.as_ref())?;
    if let Some(finding_id) = ask.finding_id {
        super::findings::read_finding(tx, finding_id)?;
    }
    // An ask about a finding offers to make a proposal of it or dismiss
    // it, a `stalled` one to make a proposal of its cause (ADR-0044
    // decision 19); the runtime applies those answers.
    let options = match (&ask.kind, ask.finding_id) {
        (AskKind::Blocked, Some(_)) => finding::with_finding_options(&ask.options),
        (AskKind::Stalled, _) => finding::with_propose_option(&ask.options),
        _ => ask.options.clone(),
    };
    if let Some(existing) = tx
            .query_row(
                "SELECT * FROM asks WHERE ifnull(task_id,0)=ifnull(?1,0) AND ifnull(run_id,'')=ifnull(?2,'')
                 AND kind=?3 AND ifnull(finding_id,0)=ifnull(?4,0)
                 AND answered_at IS NULL AND closed_at IS NULL",
                params![task_id, ask.run_id, ask.kind.as_str(), ask.finding_id],
                ask_row,
            )
            .optional()?
        {
            return Ok(AskOutcome {
                ask: existing,
                created: false,
            });
        }
    // A worker_question's topic codes (ADR-t947-2), primary first; NULL
    // for the kinds that carry none.
    let topics = crate::domain::worker_question::normalize_topics(&ask.topics);
    let topics_column = (!topics.is_empty())
        .then(|| serde_json::to_string(&topics))
        .transpose()?;
    tx.execute(
        "INSERT INTO asks(kind,task_id,run_id,question,options,asked_by,reason_category,finding_id,topics)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            ask.kind.as_str(),
            task_id,
            ask.run_id,
            ask.question,
            serde_json::to_string(&options)?,
            ask.asked_by,
            ask.reason_category.as_str(),
            ask.finding_id,
            topics_column
        ],
    )?;
    let id = AskId::new(tx.last_insert_rowid());
    let mut payload = json!({
        "ask_id": id,
        "kind": ask.kind,
        "asked_by": ask.asked_by,
        "reason_category": ask.reason_category,
    });
    if !topics.is_empty() {
        payload["topics"] = json!(topics);
    }
    ask_event(
        tx,
        task_id,
        ask.run_id.as_ref(),
        EventKind::AskOpened,
        payload,
    )?;
    let created = read_ask(tx, id)?;
    Ok(AskOutcome {
        ask: created,
        created: true,
    })
}

/// Write `text` as the answer of the open `ask` at `now`, with who gave it
/// and the option it chose (task 325), the authority it carries and
/// whether it approves (task 733), and add them to `payload`, the
/// `ask_answered` the caller records: `answered_by`, `authority`,
/// `approval`, and `option_index` with the option's text as `option` (a
/// free answer has a null index and no `option`).
pub(super) fn write_answer(
    conn: &Connection,
    ask: &Ask,
    text: &str,
    answerer: Answerer,
    now: i64,
    payload: &mut serde_json::Value,
) -> Result<()> {
    let index = option_index(&ask.options, text);
    // The runtime withdrawing or superseding an ask approves nothing.
    let approval = answerer.authority != AnswerAuthority::Runtime && answer_approves(ask, text);
    conn.execute(
        "UPDATE asks SET answer=?2, answered_at=?3, answered_by=?4, option_index=?5,
           answer_authority=?6, answer_approval=?7 WHERE id=?1",
        params![
            ask.id,
            text,
            now,
            answerer.by,
            index,
            answerer.authority.as_str(),
            approval
        ],
    )?;
    payload["answered_by"] = json!(answerer.by);
    payload["authority"] = json!(answerer.authority);
    payload["approval"] = json!(approval);
    payload["option_index"] = json!(index);
    if let Some(option) = index
        .and_then(|index| usize::try_from(index).ok())
        .and_then(|index| ask.options.get(index))
    {
        payload["option"] = json!(option);
    }
    Ok(())
}

/// Record `ask_closed` for `ask`, on its task and run, in the caller's
/// transaction that closes it: `stats` reads it as the answer applied
/// (task 468) when nothing else that names the ask applied it (task 568).
pub(super) fn record_ask_closed(conn: &Connection, ask: &Ask) -> Result<()> {
    ask_event(
        conn,
        ask.task_id,
        ask.run_id.as_ref(),
        EventKind::AskClosed,
        json!({"ask_id": ask.id, "kind": ask.kind}),
    )
}

/// An ask's event: on its task (and run), or, for a task-less `blocked`
/// ask, on nothing.
fn ask_event(
    conn: &Connection,
    task_id: Option<TaskId>,
    run_id: Option<&RunId>,
    kind: EventKind,
    payload: serde_json::Value,
) -> Result<()> {
    check_event_target(kind, task_id, None)?;
    check_run_has_task(task_id, run_id)?;
    conn.execute(
        "INSERT INTO run_events(task_id,run_id,kind,payload,actor_role,actor_id,requested_by)
         VALUES (?1,?2,?3,?4,dagq_actor_role(),dagq_actor_id(),dagq_requested_by())",
        params![
            task_id,
            run_id,
            kind.as_str(),
            serde_json::to_string(&payload)?
        ],
    )?;
    Ok(())
}

pub(super) fn read_ask(conn: &Connection, id: AskId) -> Result<Ask> {
    conn.query_row("SELECT * FROM asks WHERE id=?1", [id], ask_row)
        .optional()?
        .with_context(|| format!("ask {id} does not exist"))
}

pub(super) fn ask_row(row: &Row<'_>) -> rusqlite::Result<Ask> {
    Ok(Ask {
        id: row.get("id")?,
        kind: AskKind::read(&row.get::<_, String>("kind")?),
        task_id: row.get("task_id")?,
        run_id: row.get("run_id")?,
        question: row.get("question")?,
        options: json_col(row, "options")?,
        answer: row.get("answer")?,
        asked_by: row.get("asked_by")?,
        reason_category: enum_col(row, "reason_category")?,
        // NULL for the kinds without topics and the asks before them; a
        // value that is no array of texts reads as none.
        topics: row
            .get::<_, Option<String>>("topics")?
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_default(),
        subject: row.get("subject")?,
        affected: json_col(row, "affected")?,
        created_at: row.get("created_at")?,
        answered_at: row.get("answered_at")?,
        closed_at: row.get("closed_at")?,
        finding_id: row.get("finding_id")?,
        answered_by: row.get("answered_by")?,
        option_index: row.get("option_index")?,
        // A newer binary's authority this one does not know reads as none.
        answer_authority: row
            .get::<_, Option<String>>("answer_authority")?
            .and_then(|value| value.parse().ok()),
        answer_approval: row.get("answer_approval")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::health;
    use crate::domain::AttentionNext;
    use crate::domain::LeaseToken;
    use crate::infrastructure::adapters::SystemProcesses;

    /// Open an `update_failed` ask, answer it `text`, and return whether
    /// the answer was recorded as the runtime's and whether `status`
    /// reports it as the runtime applying it.
    fn answer_update_failed(queue: &mut SqliteQueue, text: &str) -> (bool, bool) {
        answer_update(queue, AskKind::UpdateFailed, UPDATE_FAILED_OPTIONS, text)
    }

    fn answer_update(
        queue: &mut SqliteQueue,
        kind: AskKind,
        options: &[&str],
        text: &str,
    ) -> (bool, bool) {
        answer_update_of(queue, kind, options, text, None)
    }

    /// [`answer_update`] of an ask whose `update_failed` step names
    /// `source` (`release` for a release's job).
    fn answer_update_of(
        queue: &mut SqliteQueue,
        kind: AskKind,
        options: &[&str],
        text: &str,
        source: Option<&str>,
    ) -> (bool, bool) {
        let ask = queue
            .open_update_ask(kind, "retry?", options, "runtime", Some("0.4.0"))
            .unwrap();
        if let Some(source) = source {
            queue
                .record_queue_event(
                    EventKind::UpdateFailed,
                    json!({"ask_id": ask.id, "source": source, "release": "0.4.0"}),
                )
                .unwrap();
        }
        queue.answer(ask.id, text).unwrap();
        let payload: String = queue
            .conn
            .query_row(
                "SELECT payload FROM run_events WHERE kind='ask_answered'
                 AND json_extract(payload,'$.ask_id')=?1",
                [ask.id],
                |r| r.get(0),
            )
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let recorded = payload["runtime_delivers"].as_bool().unwrap();
        let now = queue.generators.clock.now();
        let registrations = queue.supervisors().unwrap();
        let attention = health::attention(&*queue, &registrations, now, &SystemProcesses).unwrap();
        let applying = attention
            .iter()
            .find(|a| a.ask_id == Some(ask.id))
            .is_some_and(|a| matches!(a.next, AttentionNext::ApplyingAnswer { .. }));
        (recorded, applying)
    }

    #[test]
    fn an_update_answer_is_the_runtimes_only_with_a_live_auto_update_supervisor() {
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        // Nobody supervises: the answer is a person's.
        assert_eq!(answer_update_failed(&mut queue, "retry"), (false, false));

        // A live supervisor of a development build without auto-update
        // applies nothing.
        queue
            .register_supervisor(
                &LeaseToken::new("live"),
                std::process::id(),
                1,
                "0.0.1-dev+abc",
            )
            .unwrap();
        assert_eq!(answer_update_failed(&mut queue, "retry"), (false, false));

        // With auto-update it applies an option, and leaves a free answer.
        queue
            .set_auto_update(&LeaseToken::new("live"), true)
            .unwrap();
        assert_eq!(answer_update_failed(&mut queue, "skip"), (true, true));
        assert_eq!(answer_update_failed(&mut queue, "later"), (false, false));

        // Its heartbeat went stale: nobody applies it.
        queue
            .conn
            .execute(
                "UPDATE supervisors SET heartbeat_at=heartbeat_at-?1 WHERE token='live'",
                [crate::domain::HEARTBEAT_TIMEOUT_SECS + 1],
            )
            .unwrap();
        assert_eq!(answer_update_failed(&mut queue, "retry"), (false, false));
        queue
            .deregister_supervisor(&LeaseToken::new("live"))
            .unwrap();

        // A dead auto-update supervisor whose row is left behind.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        queue
            .register_supervisor(&LeaseToken::new("dead"), dead, 1, "0.0.1")
            .unwrap();
        queue
            .set_auto_update(&LeaseToken::new("dead"), true)
            .unwrap();
        assert_eq!(answer_update_failed(&mut queue, "retry"), (false, false));
    }

    /// A supervisor of a release build applies the answers of the release
    /// update's asks (ADR-t618-1 decision 4) unless the host's `release`
    /// is `off`; a development build never applies `approve_release`.
    #[test]
    fn a_release_answer_is_the_runtimes_only_with_a_live_release_supervisor() {
        use crate::domain::APPROVE_RELEASE_OPTIONS;
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let release = |queue: &mut SqliteQueue, text: &str| {
            answer_update(
                queue,
                AskKind::ApproveRelease,
                APPROVE_RELEASE_OPTIONS,
                text,
            )
        };
        assert_eq!(release(&mut queue, "install"), (false, false));

        let token = LeaseToken::new("dev");
        queue
            .register_supervisor(&token, std::process::id(), 1, "0.4.0-dev+abc")
            .unwrap();
        queue.set_auto_update(&token, true).unwrap();
        assert_eq!(release(&mut queue, "install"), (false, false));
        // A build's updater applies no failure of a release's job.
        assert_eq!(
            answer_update_of(
                &mut queue,
                AskKind::UpdateFailed,
                UPDATE_FAILED_OPTIONS,
                "retry",
                Some("release"),
            ),
            (false, false)
        );
        queue.deregister_supervisor(&token).unwrap();

        // The queue's host.toml wins over the host-wide one.
        std::fs::write(
            dir.path().join("host.toml"),
            "[update]\nrelease = \"ask\"\n",
        )
        .unwrap();
        let token = LeaseToken::new("release");
        queue
            .register_supervisor(&token, std::process::id(), 1, "0.3.0")
            .unwrap();
        assert!(queue.release_updates_on());
        assert_eq!(release(&mut queue, "install"), (true, true));
        assert_eq!(release(&mut queue, "skip"), (true, true));
        assert_eq!(release(&mut queue, "later"), (false, false));
        // It applies the failure of a release's job, not of a build.
        let release_failed = |queue: &mut SqliteQueue, text: &str| {
            answer_update_of(
                queue,
                AskKind::UpdateFailed,
                UPDATE_FAILED_OPTIONS,
                text,
                Some("release"),
            )
        };
        assert_eq!(release_failed(&mut queue, "retry"), (true, true));
        assert_eq!(answer_update_failed(&mut queue, "retry"), (false, false));

        std::fs::write(
            dir.path().join("host.toml"),
            "[update]\nrelease = \"off\"\n",
        )
        .unwrap();
        assert!(!queue.release_updates_on());
        assert_eq!(release(&mut queue, "install"), (false, false));
        assert_eq!(release_failed(&mut queue, "retry"), (false, false));
    }

    /// An update ask with a blank question is refused before the write
    /// (ADR-t876-1: the rule the `asks.question` CHECK held).
    #[test]
    fn an_update_ask_with_a_blank_question_is_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let error = queue
            .open_update_ask(
                AskKind::UpdateFailed,
                " \n",
                UPDATE_FAILED_OPTIONS,
                "runtime",
                None,
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "question must not be blank");
        let rows: i64 = queue
            .conn
            .query_row("SELECT count(*) FROM asks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    /// An ask's event about a run that names no task is refused before the
    /// write (ADR-t876-1: the rule the `run_events` CHECK held).
    #[test]
    fn an_ask_event_about_a_run_without_its_task_is_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let run = RunId::new("run-1").unwrap();
        let error = ask_event(
            &queue.conn,
            None,
            Some(&run),
            EventKind::AskOpened,
            json!({}),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "run run-1 is written without its task");
    }

    /// Answer a `worker_question` on a run of `status` whose lease has
    /// `pid` and a heartbeat `age` seconds old, after a revise request;
    /// return whether the answer was recorded as the runtime's and whether
    /// `status` reports the runtime delivering it.
    fn answer_leased_question(status: RunStatus, pid: u32, age: i64) -> (bool, bool) {
        use crate::application::TaskStore;
        use crate::domain::NewTask;
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
            })
            .unwrap()
            .id();
        let run = RunId::new("11111111-1111-4111-8111-111111111111").unwrap();
        let now = queue.generators.clock.now();
        queue
            .conn
            .execute(
                "INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,worktree_path,workspace_id,run_dir)
                 VALUES (?1,?2,?3,'claude','claude','0000000000000000000000000000000000000000','/wt','W','/run')",
                params![run, task_id, status.as_str()],
            )
            .unwrap();
        queue
            .conn
            .execute(
                "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,'sup',?2,?3)",
                params![run, pid, now - age],
            )
            .unwrap();
        super::super::sqlite::event(
            &queue.conn,
            task_id,
            Some(&run),
            EventKind::ReviseRequested,
            json!({"workspace_id": "W", "sent_at": 0}),
        )
        .unwrap();
        let ask = queue
            .ask(NewAsk {
                topics: vec!["task_overlap".into()],
                kind: AskKind::WorkerQuestion,
                task_id: Some(task_id),
                run_id: Some(run.clone()),
                question: "Which line?".into(),
                options: vec![],
                asked_by: "worker".into(),
                reason_category: AskReason::Scope,
                finding_id: None,
            })
            .unwrap()
            .ask;
        queue.answer(ask.id, "the second").unwrap();
        let payload: String = queue
            .conn
            .query_row(
                "SELECT payload FROM run_events WHERE kind='ask_answered'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let recorded = payload["runtime_delivers"].as_bool().unwrap();
        let registrations = queue.supervisors().unwrap();
        let attention = health::attention(&queue, &registrations, now, &SystemProcesses).unwrap();
        let delivering = attention
            .iter()
            .find(|a| a.ask_id == Some(ask.id))
            .is_some_and(|a| matches!(a.next, AttentionNext::DeliveringAnswer { .. }));
        (recorded, delivering)
    }

    /// The answer of a question asked during a revise is the runtime's to
    /// type only while a live supervisor holds the run's lease, as
    /// `status` judges it (task 584); a running run's answer stays the
    /// runtime's for the supervisor that adopts it.
    #[test]
    fn a_revise_answer_is_the_runtimes_only_with_a_lease_that_is_not_stale() {
        let live = std::process::id();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let old = crate::domain::HEARTBEAT_TIMEOUT_SECS + 1;
        let awaiting = RunStatus::AwaitingIntegration;
        assert_eq!(answer_leased_question(awaiting, live, 0), (true, true));
        assert_eq!(answer_leased_question(awaiting, dead, 0), (false, false));
        assert_eq!(answer_leased_question(awaiting, live, old), (false, false));
        assert!(answer_leased_question(RunStatus::Running, dead, 0).0);
        assert!(answer_leased_question(RunStatus::Running, live, old).0);
    }
}
