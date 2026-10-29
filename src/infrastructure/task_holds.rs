//! The holds of tasks (ADR-t1879-1): `task_holds`, at most one open per
//! task. Holding and releasing are each one write transaction that reads
//! the task (and the hold open on it) and writes the row with its
//! `task_held` or `task_released` event, so two holds at once open one
//! and the event never disagrees with the row. The guard every step of a
//! task asks (`replans::check_task_step`) and the ready query read the same
//! rows inside the step's own transaction, so a claim, a resume or a
//! recovery round and a hold are ordered by the store: whichever commits
//! first wins.
//!
//! The run's wait for a hold is ordered the same way: the hold reads the
//! run's status and lease and is `held` at once only for a run at rest
//! ([`task_hold::run_wait`]); a run whose step goes settles in the
//! transaction that parks it or finds it at rest ([`settle_open`]), which
//! finds the hold still open and settling, and the release reads whether
//! it settled in its own, so a release while `settling` is refused and a
//! park after a release writes nothing.
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::json;

use super::sqlite::{SqliteQueue, event, read_task};
use crate::application::commands::holds::{ReleaseRequest, ReleaseView};
use crate::domain::event_kind::EventKind;
use crate::domain::task_hold::{
    self, HoldGuard, HoldRecorded, HoldState, HoldTarget, ProgressVersion, ReleaseRecorded,
    RunHoldWait, TaskHold,
};
use crate::domain::task_replan::GuardedStep;
use crate::domain::{
    HoldId, RunId, RunStatus, TaskId, TaskRun, TaskStatus, run_phase, worker_model,
};

impl SqliteQueue {
    /// Hold the task `target` names for `reason` until `until` (a time to
    /// remind a person, unix seconds), as `by_role` with actor id `by_id`.
    /// In one transaction: resolve a run to its task, check the task takes
    /// a hold ([`task_hold::check_hold`]), insert the row and record
    /// `task_held`. The hold is `settling` while the task's latest run has
    /// a step that goes ([`task_hold::run_wait`]), else `held` at once, an
    /// unfinished run entering its `held` phase in the same transaction.
    pub fn hold_task(
        &mut self,
        target: &HoldTarget,
        reason: &str,
        until: Option<i64>,
        by_role: &str,
        by_id: &str,
    ) -> Result<HoldRecorded> {
        let now = self.generators.clock.now();
        let tx = self.write_tx("hold_task", TransactionBehavior::Immediate)?;
        let task_id = resolve(&tx, target)?;
        let task = read_task(&tx, task_id)?;
        let open = open_hold_of(&tx, task_id)?;
        task_hold::check_hold(task_id, task.status(), open.as_ref(), reason)?;
        let latest = latest_run(&tx, task_id)?;
        let latest_status = latest
            .as_ref()
            .map(|(_, status)| status.parse::<RunStatus>())
            .transpose()?;
        let leased = match &latest {
            Some((run, _)) => tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM run_leases WHERE run_id=?1)",
                [run],
                |r| r.get(0),
            )?,
            None => false,
        };
        let writer = match &latest {
            Some((run, _)) => rest_writer(&tx, run, false)?,
            None => task_hold::RestWriter::default(),
        };
        let state = task_hold::held_state(task_hold::rest_wait(latest_status, leased, writer));
        let named = match target {
            HoldTarget::Run(run) => Some(run.clone()),
            HoldTarget::Task(_) => None,
        };
        tx.execute(
            "INSERT INTO task_holds(task_id, run_id, reason, until, by_role, by_id,
               created_at, settled_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                task_id,
                named,
                reason.trim(),
                until,
                by_role,
                by_id,
                now,
                (state == HoldState::Held).then_some(now),
            ],
        )?;
        let id = HoldId::new(tx.last_insert_rowid());
        event(
            &tx,
            task_id,
            latest.as_ref().map(|(run, _)| run),
            EventKind::TaskHeld,
            json!({"hold_id": id, "reason": reason.trim(), "until": until,
                   "requested": target.requested(),
                   "run_id": latest.as_ref().map(|(run, _)| run),
                   "run_status": latest.as_ref().map(|(_, status)| status),
                   "state": state}),
        )?;
        if state == HoldState::Held
            && let (Some((run, _)), Some(status)) = (&latest, latest_status)
            && status.is_unfinished()
        {
            enter_held_phase(&tx, run, EventKind::TaskHeld.as_str())?;
        }
        let hold = read_hold(&tx, id)?;
        tx.commit()?;
        Ok(HoldRecorded { hold, state })
    }

    /// What a release of the task `target` names reads before its
    /// transaction: the task, its open hold, its latest run with its
    /// [`ProgressVersion`] and whether it has a session a resume goes on in
    /// ([`task_hold::session_to_resume`]).
    pub fn release_view(&self, target: &HoldTarget) -> Result<ReleaseView> {
        let task = read_task(&self.conn, resolve(&self.conn, target)?)?;
        // The hold before the run: a hold that settles in between is read
        // as settling, never as held with the version from before.
        let hold = open_hold_of(&self.conn, task.id())?;
        let run = latest_task_run(&self.conn, &self.runs_dir, task.id())?;
        let (version, session, kept) = match &run {
            Some(run) => (
                Some(progress_version(&self.conn, task.id(), run.id())?),
                task_hold::session_to_resume(&super::runtime_store::run_events_of(
                    &self.conn,
                    run.id(),
                )?),
                kept_unsettled(&self.conn, run.id())?,
            ),
            None => (None, false, false),
        };
        Ok(ReleaseView {
            hold,
            task,
            run,
            version,
            session,
            kept,
        })
    }

    /// Release the open hold of the task `request.target` names, as
    /// `by_role` with actor id `by_id`, going on as
    /// [`task_hold::release_decision`] decides. In one transaction: check
    /// there is one and that it may be released now
    /// ([`task_hold::check_release`]: refused while the run has not
    /// settled, unless the task ended, which leaves nothing to settle),
    /// before anything of the choice is read or written; check the model
    /// chosen against the provider the task goes on with
    /// ([`worker_model::check_model`]); close the row and record
    /// `task_released` with the decision, the provider, the model and what
    /// the next run carries over; record the provider on the task. A
    /// release that ends the run for a new one (T13) needs the work the
    /// caller read of that same run (`request.work`) and no writer of it
    /// left; it ends the run ([`super::runtime_store::end_released_run`],
    /// a retry by hand when it carries work over) and makes the task ready
    /// again without a plan review; the head it carries over is kept
    /// (`keep`) last, before the commit, so that a head that cannot be kept
    /// refuses the release. Otherwise the run's `held` phase closes with
    /// the phase it rests in, and the supervisor's next pass takes the task
    /// by its own paths again. A refusal changes nothing and records
    /// nothing.
    pub fn release_task(
        &mut self,
        request: &ReleaseRequest,
        by_role: &str,
        by_id: &str,
        keep: &dyn Fn(&TaskRun, &crate::domain::CommitSha) -> Result<()>,
    ) -> Result<ReleaseRecorded> {
        // The spans a carry-over's `triage_finished` closes read their
        // transcripts first (task 543), as `ready --inherit` does.
        let _read = match &request.work {
            Some((at, _)) => Some(super::sessions::read_before(
                &self.conn,
                super::sessions::Closing::Run(&at.run, &[crate::domain::EventKind::TriageFinished]),
            )?),
            None => None,
        };
        let now = self.generators.clock.now();
        let tx = self.write_tx("release_task", TransactionBehavior::Immediate)?;
        let task_id = resolve(&tx, &request.target)?;
        let task = read_task(&tx, task_id)?;
        let open = open_hold_of(&tx, task_id)?;
        // A task that ended has nothing left to settle: its hold no longer
        // reaches it, and the release only closes the row.
        let ended = matches!(task.status(), TaskStatus::Completed | TaskStatus::Canceled);
        let wait = match open.as_ref().map(TaskHold::state) {
            _ if ended => RunHoldWait::None,
            Some(HoldState::Settling) => RunHoldWait::Settling,
            Some(HoldState::Held) => RunHoldWait::Settled,
            None => RunHoldWait::None,
        };
        task_hold::check_release(task_id, open.as_ref(), wait)?;
        let hold = open.context("checked above")?;
        let latest = latest_task_run(&tx, &self.runs_dir, task_id)?;
        let facts = task_hold::ReleaseFacts {
            task: task.status(),
            run: latest.as_ref().map(TaskRun::status),
            run_provider: latest.as_ref().map(TaskRun::actual_provider),
            session: match &latest {
                Some(run) => task_hold::session_to_resume(&super::runtime_store::run_events_of(
                    &tx,
                    run.id(),
                )?),
                None => false,
            },
            kept: match &latest {
                Some(run) => kept_unsettled(&tx, run.id())?,
                None => false,
            },
        };
        let choice = &request.choice;
        let ends = task_hold::ends_run(facts, choice);
        // The provider the task's next turn takes: the one chosen, else the
        // run's own while it goes on, else the task's.
        let goes_on = match (&latest, ends || task.status() != TaskStatus::InProgress) {
            (Some(run), false) => run.actual_provider(),
            _ => task.worker().provider,
        };
        if let Some(model) = &choice.model {
            worker_model::check_model(choice.provider.unwrap_or(goes_on), model)?;
        }
        // What the request read before this transaction stands only while
        // the hold it was read under is open and the run has not gone on.
        let read = request.work.as_ref();
        let found = match &latest {
            Some(run) if ends || read.is_some() => Some(task_hold::ReleaseReadAt {
                hold: hold.id,
                run: run.id().clone(),
                version: progress_version(&tx, task_id, run.id())?,
            }),
            _ => None,
        };
        task_hold::check_release_read(
            task_id,
            hold.id,
            ends,
            read.map(|(at, _)| at),
            found.as_ref(),
        )?;
        let work = read
            .filter(|_| ends && latest.is_some())
            .map(|(_, work)| work);
        let decision =
            task_hold::release_decision(facts, choice, work.is_some_and(|work| work.own.is_some()));
        let inherit = match (&latest, work.and_then(|work| Some((work.head()?, work)))) {
            (Some(run), Some((head, work))) => Some(task_hold::ReleasedWork {
                branch: run.branch().map(str::to_owned),
                head: head.clone(),
                saved: work.saved.clone(),
            }),
            _ => None,
        };
        // A fresh release that saved uncommitted work carries it over to
        // the next run (the replacement's handed part among it): nothing
        // ends.
        let carry_ended = match (&latest, decision, &inherit) {
            (Some(run), task_hold::ReleaseDecision::RetryFresh, None) => {
                carry_ended(&tx, task_id, run.id(), facts, choice)?
            }
            _ => None,
        };
        let session = match &choice.model {
            Some(model) => {
                let events = match &latest {
                    Some(run) => super::runtime_store::run_events_of(&tx, run.id())?,
                    None => Vec::new(),
                };
                Some(worker_model::WorkerSession::released(
                    model,
                    &worker_model::WorkerSession::current(&events),
                ))
            }
            None => None,
        };
        tx.execute(
            "UPDATE task_holds SET released_at=?2, released_by_role=?3, released_by_id=?4,
               continuation=?5
             WHERE id=?1 AND released_at IS NULL",
            params![hold.id, now, by_role, by_id, decision.continuation()],
        )?;
        let record = task_hold::Released {
            hold_id: Some(hold.id),
            continuation: decision.continuation().to_owned(),
            decision: Some(decision),
            held_secs: now - hold.created_at,
            settled_secs: hold.settled_at.map(|at| now - at),
            provider: choice.provider,
            model: session.as_ref().map(|session| session.model.clone()),
            effort: session.as_ref().map(|session| session.effort.clone()),
            inherit: inherit.clone(),
        };
        event(
            &tx,
            task_id,
            latest.as_ref().map(TaskRun::id),
            EventKind::TaskReleased,
            serde_json::to_value(&record)?,
        )?;
        if let (Some(run), Some(ended)) = (&latest, &carry_ended) {
            event(
                &tx,
                task_id,
                Some(run.id()),
                EventKind::ReplanCarryEnded,
                serde_json::to_value(ended)?,
            )?;
        }
        if let Some(provider) = choice.provider
            && provider != task.worker().provider
        {
            let task = crate::domain::task::release_on(task.clone(), provider)?;
            tx.execute(
                "UPDATE tasks SET worker_provider=?2, worker_mode=?3 WHERE id=?1",
                params![
                    task_id,
                    task.worker().provider.as_str(),
                    task.stored_worker_mode().map(|mode| mode.as_str()),
                ],
            )?;
        }
        if let (Some(run), true) = (&latest, decision.new_run()) {
            let alive: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM run_leases WHERE run_id=?1
                     AND heartbeat_at >= ?2-?3)
                   OR EXISTS(SELECT 1 FROM run_processes WHERE run_id=?1
                     AND exited_at IS NULL AND heartbeat_at >= ?2-?3)",
                params![run.id(), now, crate::domain::HEARTBEAT_TIMEOUT_SECS],
                |r| r.get(0),
            )?;
            anyhow::ensure!(
                !alive,
                "run {} is still leased or a process of it still runs; nothing is released, try again once it waits",
                run.id()
            );
            let why = task_hold::new_run_reason(facts, choice.provider);
            let carried = inherit.as_ref().map(|inherit| {
                json!({"action": crate::domain::resume::RETRY_INHERIT, "by": by_role,
                       "reason": why, "inherit": inherit, "released": hold.id,
                       "status": run.status()})
            });
            super::runtime_store::end_released_run(
                &tx,
                run.id(),
                format!("{}: {why}", crate::domain::ReasonCode::Released),
                carried,
            )?;
            // The recovery job's `decide` asks on the run are answered by
            // the release: nothing of them applies to the run any more.
            let decides: Vec<crate::domain::Ask> = tx
                .prepare(
                    "SELECT * FROM asks WHERE run_id=?1 AND kind='decide' AND asked_by=?2
                     AND closed_at IS NULL ORDER BY id",
                )?
                .query_map(
                    params![run.id(), crate::application::TRIAGE_ASKER],
                    super::asks::ask_row,
                )?
                .collect::<rusqlite::Result<_>>()?;
            for ask in &decides {
                super::asks::close_by_runtime(
                    &tx,
                    ask,
                    &format!("released from its hold by {by_role} with a new run: {why}"),
                    now,
                )?;
            }
            leave_held_phase(&tx, run.id())?;
            super::sqlite::transition_task(
                &tx,
                task_id,
                crate::domain::TaskAction::Ready,
                &self.generators.clock.timestamp(),
            )?;
        } else if let Some(run) = &latest {
            leave_held_phase(&tx, run.id())?;
        }
        // Kept last, while the transaction can still be rolled back: a ref
        // that cannot be written leaves the hold, the run and the task as
        // they were, and the worktree's work where it is. A commit that
        // fails after it leaves only the ref, which holds the run's own work.
        if let (Some(run), Some(inherit)) = (&latest, &inherit) {
            keep(run, &inherit.head).with_context(|| {
                format!(
                    "the work run {} carries over could not be kept as {}; nothing is released",
                    run.id(),
                    crate::application::inherit::run_ref(run.id())
                )
            })?;
        }
        tx.commit()?;
        Ok(ReleaseRecorded {
            hold,
            continuation: decision.continuation(),
            decision,
            provider: choice.provider,
            model: session.map(|session| session.model),
            inherit,
            replan_carry_ended: carry_ended,
        })
    }

    /// The open hold of `task`, if any.
    pub fn task_hold(&self, task: TaskId) -> Result<Option<TaskHold>> {
        open_hold_of(&self.conn, task)
    }

    /// Every open hold of a task that has not ended, oldest first.
    pub fn task_holds(&self) -> Result<Vec<TaskHold>> {
        Ok(self
            .conn
            .prepare(
                "SELECT h.* FROM task_holds h JOIN tasks t ON t.id = h.task_id
                 WHERE h.released_at IS NULL AND t.status NOT IN ('completed','canceled')
                 ORDER BY h.id",
            )?
            .query_map([], hold_row)?
            .collect::<rusqlite::Result<_>>()?)
    }
}

impl SqliteQueue {
    /// Every open hold of a task that has not ended, oldest first, each
    /// with its task's latest run and that run's status now.
    pub fn held_tasks(&self) -> Result<Vec<task_hold::HeldTask>> {
        self.task_holds()?
            .into_iter()
            .map(|hold| {
                let run = latest_run(&self.conn, hold.task_id)?
                    .map(|(run, status)| Ok::<_, anyhow::Error>((run, status.parse()?)))
                    .transpose()?;
                Ok((hold, run))
            })
            .collect()
    }

    /// Tell a person of each open hold due now ([`TaskHold::reminder_due`]):
    /// in one transaction, read the open holds of tasks that have not
    /// ended and when each was last told, and record `task_hold_reminded`
    /// for each due, so two supervisors at once tell a hold once. The
    /// holds told, with why.
    pub fn remind_task_holds(&mut self) -> Result<Vec<(TaskHold, task_hold::HoldReminder)>> {
        let now = self.generators.clock.now();
        let tx = self.write_tx("remind_task_holds", TransactionBehavior::Immediate)?;
        let holds: Vec<TaskHold> = tx
            .prepare(
                "SELECT h.* FROM task_holds h JOIN tasks t ON t.id = h.task_id
                 WHERE h.released_at IS NULL AND t.status NOT IN ('completed','canceled')
                 ORDER BY h.id",
            )?
            .query_map([], hold_row)?
            .collect::<rusqlite::Result<_>>()?;
        let mut told = Vec::new();
        for hold in holds {
            let last: Option<i64> = tx
                .query_row(
                    "SELECT json_extract(payload,'$.at') FROM run_events
                     WHERE task_id=?1 AND kind=?2 AND json_extract(payload,'$.hold_id')=?3
                     ORDER BY id DESC LIMIT 1",
                    params![hold.task_id, EventKind::TaskHoldReminded.as_str(), hold.id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            let Some(cause) = hold.reminder_due(last, now) else {
                continue;
            };
            event(
                &tx,
                hold.task_id,
                None,
                EventKind::TaskHoldReminded,
                json!({"hold_id": hold.id, "task_id": hold.task_id, "reason": hold.reason,
                       "until": hold.until, "held_at": hold.created_at,
                       "settled_at": hold.settled_at, "cause": cause, "at": now}),
            )?;
            told.push((hold, cause));
        }
        tx.commit()?;
        Ok(told)
    }

    /// What fences `task`'s run at its turn's boundary now
    /// ([`crate::domain::task_replan::TaskGuards::fence`]).
    pub fn turn_fence(&self, task: TaskId) -> Result<Option<crate::domain::task_replan::Fence>> {
        Ok(crate::domain::task_replan::TaskGuards {
            replan: super::replans::guard_of(&self.conn, task)?,
            hold: guard_of(&self.conn, task)?,
        }
        .fence())
    }

    /// The open holds of tasks that have not ended whose run has not
    /// settled for them yet, oldest first.
    pub fn settling_holds(&self) -> Result<Vec<TaskHold>> {
        Ok(self
            .conn
            .prepare(
                "SELECT h.* FROM task_holds h JOIN tasks t ON t.id = h.task_id
                 WHERE h.released_at IS NULL AND h.settled_at IS NULL
                   AND t.status NOT IN ('completed','canceled')
                 ORDER BY h.id",
            )?
            .query_map([], hold_row)?
            .collect::<rusqlite::Result<_>>()?)
    }
}

/// The end of the handover of the saved work of the replacement `task`
/// is a child of, which the release of its hold that ends `run` afresh
/// (`retry_fresh`, at `facts` with `choice`) records
/// ([`task_hold::ReplanCarryEnded`]); `None` for a task that is no child
/// taking saved work, or whose handover never began or ended already
/// ([`task_hold::replan_carry_live`]).
fn carry_ended(
    conn: &Connection,
    task: TaskId,
    run: &RunId,
    facts: task_hold::ReleaseFacts,
    choice: &task_hold::ReleaseChoice,
) -> Result<Option<task_hold::ReplanCarryEnded>> {
    let Some(carry) = super::replan_apply::child_carry(conn, task)? else {
        return Ok(None);
    };
    let events: Vec<crate::domain::RunEvent> = conn
        .prepare("SELECT * FROM run_events WHERE task_id=?1 ORDER BY id DESC")?
        .query_map([task], super::sqlite::event_row)?
        .collect::<rusqlite::Result<_>>()?;
    if !task_hold::replan_carry_live(&events, carry.operation) {
        return Ok(None);
    }
    Ok(Some(task_hold::ReplanCarryEnded::new(
        &carry,
        task,
        run.clone(),
        facts,
        choice.provider,
    )))
}

/// Close, on `conn` inside the caller's transaction that moved `task` to
/// `status`, the hold a task that ended takes with it
/// ([`task_hold::HoldEnd`]): its open hold is released by the runtime and
/// recorded as `task_hold_ended` with the end, and every result kept for
/// the task that is not settled yet is discarded (`hold_result_discarded`,
/// `task_ended`), a kept answer's ask closed. Nothing for a status that
/// has not ended the task. Whether a hold was open.
pub(super) fn end_with_task(
    conn: &Connection,
    task: TaskId,
    status: TaskStatus,
    now: i64,
) -> Result<bool> {
    let Some(end) = task_hold::HoldEnd::of(status) else {
        return Ok(false);
    };
    let run = latest_run(conn, task)?.map(|(run, _)| run);
    let open = open_hold_of(conn, task)?;
    if let Some(hold) = &open {
        conn.execute(
            "UPDATE task_holds SET released_at=?2, released_by_role=?3, released_by_id=?3
             WHERE id=?1 AND released_at IS NULL",
            params![hold.id, now, HOLD_ENDER],
        )?;
        event(
            conn,
            task,
            run.as_ref(),
            EventKind::TaskHoldEnded,
            json!({"hold_id": hold.id, "reason": end, "held_secs": now - hold.created_at,
                   "settled_secs": hold.settled_at.map(|at| now - at)}),
        )?;
    }
    discard_kept_of_ended(conn, task, end, now)?;
    Ok(open.is_some())
}

/// Who `released_by_role` and `released_by_id` name for a hold its task's
/// end closed.
const HOLD_ENDER: &str = "runtime";

/// Discard on `conn` every result kept for `task` that is neither applied
/// nor discarded yet, its task having ended (`end`): `hold_result_discarded`
/// with reason `task_ended`, superseded (it makes no round due: the run of
/// an ended task is not recovered), and a kept answer's ask closed by the
/// runtime, each read back as a [`task_hold::KeptResult`]. A result being
/// applied now is settled already and stays.
fn discard_kept_of_ended(
    conn: &Connection,
    task: TaskId,
    end: task_hold::HoldEnd,
    now: i64,
) -> Result<()> {
    let unsettled: Vec<crate::domain::RunEvent> = conn
        .prepare(
            "SELECT e.* FROM run_events e
             WHERE e.task_id=?1 AND e.kind=?2 AND e.run_id IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM run_events s WHERE s.run_id=e.run_id
                 AND s.kind IN (?3,?4) AND json_extract(s.payload,'$.kept_id')=e.id)
             ORDER BY e.id",
        )?
        .query_map(
            params![
                task,
                EventKind::HoldResultKept.as_str(),
                EventKind::HoldResultApplied.as_str(),
                EventKind::HoldResultDiscarded.as_str()
            ],
            super::sqlite::event_row,
        )?
        .collect::<rusqlite::Result<_>>()?;
    for kept in unsettled {
        let (Some(result), Some(run)) = (task_hold::KeptResult::of_event(&kept), &kept.run_id)
        else {
            continue;
        };
        let task_hold::Disposition::Discard(reason) = end.kept_result() else {
            unreachable!("a result kept for a task that ended is discarded");
        };
        if let Some(ask) = result.ask_to_close()
            && let Some(ask) = conn
                .query_row(
                    "SELECT * FROM asks WHERE id=?1 AND closed_at IS NULL",
                    [ask],
                    super::asks::ask_row,
                )
                .optional()?
        {
            super::asks::discard_by_runtime(
                conn,
                &ask,
                reason.as_str(),
                &format!(
                    "task {task} {}: the answer kept while it was held is discarded",
                    end.as_str()
                ),
                now,
            )?;
        }
        event(
            conn,
            task,
            Some(run),
            EventKind::HoldResultDiscarded,
            json!({"kept_id": kept.id, "source": result.source, "reason": reason,
                   "error": null, task_hold::SUPERSEDED: true}),
        )?;
    }
    Ok(())
}

/// Close each hold left open on a task that ended before its end closed
/// holds ([`end_with_task`]), as that end would have, the task unchanged:
/// in one transaction, which a pass with none to close does not open. The
/// tasks whose hold was closed.
pub(super) fn end_holds_of_ended(queue: &SqliteQueue, now: i64) -> Result<Vec<TaskId>> {
    if ended_with_open_hold(&queue.conn)?.is_empty() {
        return Ok(Vec::new());
    }
    let tx = queue.write_tx("end_holds_of_ended", TransactionBehavior::Immediate)?;
    let mut closed = Vec::new();
    for (task, status) in ended_with_open_hold(&tx)? {
        if end_with_task(&tx, task, status, now)? {
            closed.push(task);
        }
    }
    tx.commit()?;
    Ok(closed)
}

/// The tasks that ended with a hold still open, each with its status.
fn ended_with_open_hold(conn: &Connection) -> Result<Vec<(TaskId, TaskStatus)>> {
    let rows: Vec<(TaskId, String)> = conn
        .prepare(
            "SELECT DISTINCT h.task_id, t.status FROM task_holds h JOIN tasks t ON t.id = h.task_id
             WHERE h.released_at IS NULL AND t.status IN ('completed','canceled')
             ORDER BY h.task_id",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(|(task, status)| Ok((task, status.parse()?)))
        .collect()
}

/// Record on `conn` that the open hold `hold` settled at `now`: whether it
/// was still open and settling (a release or another settle that came
/// first leaves it, and the caller writes nothing more).
pub(super) fn settle_open(conn: &Connection, hold: HoldId, now: i64) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE task_holds SET settled_at=?2
         WHERE id=?1 AND released_at IS NULL AND settled_at IS NULL",
        params![hold, now],
    )? == 1)
}

/// Whether a replanning's park of `run` ended `blocked` while it stopped
/// or saved the run's work (ADR-t1521-2): its writer may still run in the
/// worktree, so the run's rest is no safe wait until a look at the
/// worktree finds none.
fn park_blocked(conn: &Connection, run: &RunId) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM replan_operations WHERE run_id=?1 AND status='blocked'
           AND return_phase IN ('pause_requested','quiescing','snapshotting'))",
        [run],
        |r| r.get(0),
    )?)
}

/// Whether a session process of `run` the queue registered has no exit
/// recorded. A lease given back is no writer's end (a resume given up keeps
/// a session whose wrapper may still run its turn), and neither is a
/// heartbeat gone old: a wrapper is dead only when its heartbeat is old
/// and its process is gone, which the supervisor's park looks at, not the
/// store.
fn registered_writer(conn: &Connection, run: &RunId) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_processes WHERE run_id=?1 AND exited_at IS NULL)",
        [run],
        |r| r.get(0),
    )?)
}

/// What may still write `run` as its records tell ([`park_blocked`],
/// [`registered_writer`]), `checked` when the caller's park found the
/// writer ended ([`task_hold::RestWriter`]).
fn rest_writer(conn: &Connection, run: &RunId, checked: bool) -> Result<task_hold::RestWriter> {
    Ok(task_hold::RestWriter {
        park_blocked: park_blocked(conn, run)?,
        registered_process: registered_writer(conn, run)?,
        checked,
    })
}

/// Settle, on `conn` inside the caller's transaction, the open hold of
/// run `run`'s task while it waits for the run's step to end, once the run
/// rests, as [`task_hold::settles_at_rest`] decides from what the records
/// tell ([`rest_writer`]): a run whose writer may still run settles only
/// when the caller's park found the writer ended (`writers_checked`). An
/// unfinished run enters its `held` phase. Whether a hold settled now.
pub(super) fn settle_at_rest_in(
    conn: &Connection,
    run: &RunId,
    writers_checked: bool,
) -> Result<bool> {
    let Some((task, status)) = conn
        .query_row(
            "SELECT task_id, status FROM task_runs WHERE id=?1",
            [run],
            |r| Ok((r.get::<_, TaskId>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
    else {
        return Ok(false);
    };
    let Some(hold) = guard_of(conn, task)? else {
        return Ok(false);
    };
    let status: RunStatus = status.parse()?;
    let leased: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_leases WHERE run_id=?1)",
        [run],
        |r| r.get(0),
    )?;
    let latest = latest_run(conn, task)?.is_some_and(|(latest, _)| latest == *run);
    let writer = rest_writer(conn, run, writers_checked)?;
    if !task_hold::settles_at_rest(status, leased, latest, writer) {
        return Ok(false);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64);
    if !settle_open(conn, hold.hold, now)? {
        return Ok(false);
    }
    if status.is_unfinished() {
        enter_held_phase(conn, run, "hold_settled")?;
    }
    Ok(true)
}

/// Record on `conn` that run `run` entered its hold's wait
/// ([`run_phase::Phase::Held`]), which `cause` moved it to, unless it is
/// the phase last recorded.
pub(super) fn enter_held_phase(conn: &Connection, run: &RunId, cause: &str) -> Result<()> {
    let events = super::runtime_store::run_events_of(conn, run)?;
    record_phase(
        conn,
        run,
        run_phase::next_change(&events, run_phase::Phase::Held, cause),
    )
}

/// Close the hold's wait of run `run` at its release with the phase the
/// run rests in now ([`run_phase::Phase::resting`], which `task_released`
/// among its events lets say), when its last record is the wait.
fn leave_held_phase(conn: &Connection, run: &RunId) -> Result<()> {
    let events = super::runtime_store::run_events_of(conn, run)?;
    let held = run_phase::last_recorded(&events)
        .is_some_and(|last| last.phase == run_phase::Phase::Held.name());
    if !held {
        return Ok(());
    }
    let status: String =
        conn.query_row("SELECT status FROM task_runs WHERE id=?1", [run], |r| {
            r.get(0)
        })?;
    let change = run_phase::Phase::resting(status.parse()?, &events, None)
        .and_then(|phase| run_phase::next_change(&events, phase, EventKind::TaskReleased.as_str()));
    record_phase(conn, run, change)
}

fn record_phase(
    conn: &Connection,
    run: &RunId,
    change: Option<run_phase::PhaseChange>,
) -> Result<()> {
    let Some(change) = change else {
        return Ok(());
    };
    let task: TaskId = conn.query_row("SELECT task_id FROM task_runs WHERE id=?1", [run], |r| {
        r.get(0)
    })?;
    event(
        conn,
        task,
        Some(run),
        EventKind::RunPhaseChanged,
        change.payload(),
    )
}

/// Whether a result was kept for `run` while its task was held
/// (`hold_result_kept`) that is neither applied nor discarded yet.
fn kept_unsettled(conn: &Connection, run: &RunId) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_events e WHERE e.run_id=?1 AND e.kind=?2
           AND NOT EXISTS (SELECT 1 FROM run_events s WHERE s.run_id=e.run_id
             AND s.kind IN (?3,?4) AND json_extract(s.payload,'$.kept_id')=e.id))",
        params![
            run,
            EventKind::HoldResultKept.as_str(),
            EventKind::HoldResultApplied.as_str(),
            EventKind::HoldResultDiscarded.as_str()
        ],
        |r| r.get(0),
    )?)
}

/// The latest run of `task` as stored, if it has one.
fn latest_task_run(
    conn: &Connection,
    runs_dir: &std::path::Path,
    task: TaskId,
) -> Result<Option<TaskRun>> {
    Ok(conn
        .query_row(
            "SELECT * FROM task_runs WHERE task_id=?1 ORDER BY rowid DESC LIMIT 1",
            [task],
            super::sqlite::run_row(runs_dir),
        )
        .optional()?)
}

/// The task `target` names; a run the queue does not know is an error.
fn resolve(conn: &Connection, target: &HoldTarget) -> Result<TaskId> {
    match target {
        HoldTarget::Task(task) => Ok(*task),
        HoldTarget::Run(run) => conn
            .query_row("SELECT task_id FROM task_runs WHERE id=?1", [run], |r| {
                r.get(0)
            })
            .optional()?
            .with_context(|| format!("run {run} does not exist")),
    }
}

/// The latest run of `task` and its status, if it has one.
pub(super) fn latest_run(conn: &Connection, task: TaskId) -> Result<Option<(RunId, String)>> {
    Ok(conn
        .query_row(
            "SELECT id, status FROM task_runs WHERE task_id=?1 ORDER BY rowid DESC LIMIT 1",
            [task],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

fn read_hold(conn: &Connection, id: HoldId) -> Result<TaskHold> {
    Ok(conn.query_row("SELECT * FROM task_holds WHERE id=?1", [id], hold_row)?)
}

fn hold_row(row: &Row<'_>) -> rusqlite::Result<TaskHold> {
    Ok(TaskHold {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        run_id: row.get("run_id")?,
        reason: row.get("reason")?,
        until: row.get("until")?,
        by_role: row.get("by_role")?,
        by_id: row.get("by_id")?,
        created_at: row.get("created_at")?,
        settled_at: row.get("settled_at")?,
    })
}

/// The open hold of `task`, read on `conn` (inside the caller's
/// transaction when it has one), whatever the task's status.
pub(super) fn open_hold_of(conn: &Connection, task: TaskId) -> Result<Option<TaskHold>> {
    Ok(conn
        .query_row(
            "SELECT * FROM task_holds WHERE task_id=?1 AND released_at IS NULL
             ORDER BY id LIMIT 1",
            [task],
            hold_row,
        )
        .optional()?)
}

/// The guard the open hold of `task` puts on its steps, read on `conn`:
/// none for a task that ended, which a hold no longer reaches.
pub(super) fn guard_of(conn: &Connection, task: TaskId) -> Result<Option<HoldGuard>> {
    Ok(conn
        .query_row(
            "SELECT h.* FROM task_holds h JOIN tasks t ON t.id = h.task_id
             WHERE h.task_id=?1 AND h.released_at IS NULL
               AND t.status NOT IN ('completed','canceled')
             ORDER BY h.id LIMIT 1",
            [task],
            hold_row,
        )
        .optional()?
        .map(|hold| hold.guard()))
}

/// The version of `task` and of its run `run` now, read on `conn`: what a
/// kept result is compared with after its hold's release.
pub(super) fn progress_version(
    conn: &Connection,
    task: TaskId,
    run: &RunId,
) -> Result<ProgressVersion> {
    let (status, updated_at): (String, String) = conn.query_row(
        "SELECT status, updated_at FROM tasks WHERE id=?1",
        [task],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let run_status: String =
        conn.query_row("SELECT status FROM task_runs WHERE id=?1", [run], |r| {
            r.get(0)
        })?;
    let events = super::runtime_store::run_events_of(conn, run)?;
    Ok(ProgressVersion::of(
        status.parse()?,
        &updated_at,
        run_status.parse::<RunStatus>()?,
        &events,
    ))
}

impl crate::application::commands::holds::HoldStore for SqliteQueue {
    fn record_images(&mut self, records: &[(EventKind, serde_json::Value)]) -> Result<()> {
        crate::application::EventStore::record_queue_events(self, records.to_vec()).map(drop)
    }

    fn run_task(&self, run: &RunId) -> Result<Option<TaskId>> {
        Ok(self
            .conn
            .query_row("SELECT task_id FROM task_runs WHERE id=?1", [run], |r| {
                r.get(0)
            })
            .optional()?)
    }

    fn hold_task(
        &mut self,
        target: &HoldTarget,
        reason: &str,
        until: Option<i64>,
        by_role: &str,
        by_id: &str,
    ) -> Result<HoldRecorded> {
        SqliteQueue::hold_task(self, target, reason, until, by_role, by_id)
    }

    fn release_view(&self, target: &HoldTarget) -> Result<ReleaseView> {
        SqliteQueue::release_view(self, target)
    }

    fn release_task(
        &mut self,
        request: &ReleaseRequest,
        by_role: &str,
        by_id: &str,
        keep: &dyn Fn(&TaskRun, &crate::domain::CommitSha) -> Result<()>,
    ) -> Result<ReleaseRecorded> {
        SqliteQueue::release_task(self, request, by_role, by_id, keep)
    }
}

/// Refuse `step` of `task` while its hold guards it, by the same judgment
/// as every other guarded step ([`crate::domain::task_replan::check_step`]).
pub(super) fn check_not_held(conn: &Connection, task: TaskId, step: GuardedStep) -> Result<()> {
    crate::domain::task_replan::check_step(
        &crate::domain::task_replan::TaskGuards {
            replan: None,
            hold: guard_of(conn, task)?,
        },
        step,
    )?;
    Ok(())
}
