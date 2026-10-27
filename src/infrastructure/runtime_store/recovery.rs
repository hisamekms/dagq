//! The run aggregate moved by recovery: triage, resumes and adoption
//! ([`RunRecovery`]).

use super::*;
use crate::application::KeptOutcome;
use crate::domain::cancel_reason::NewCancel;
use crate::domain::replan_snapshot::RestoreStart;
use crate::domain::task_hold::{
    self, DiscardReason, Disposition, HoldGuard, KeptSource, ProgressVersion,
};
use crate::domain::task_replan::GuardedStep;
use crate::domain::{AskKind, EventKind, run_phase};

impl SqliteQueue {
    /// The runs whose lease carries `token`, oldest first: what a supervisor
    /// that exec'd another binary under the same token picks up again.
    pub fn runs_leased_by(&self, token: &LeaseToken) -> Result<Vec<TaskRun>> {
        Ok(self
            .conn
            .prepare(
                "SELECT r.* FROM task_runs r JOIN run_leases l ON l.run_id=r.id
                 WHERE l.token=?1 ORDER BY r.rowid",
            )?
            .query_map([token], run_row(&self.runs_dir))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The unclosed asks of the run, answered or not, oldest first.
    pub fn unclosed_run_asks(&self, run_id: &RunId) -> Result<Vec<crate::domain::Ask>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM asks WHERE run_id=?1 AND closed_at IS NULL ORDER BY id")?
            .query_map([run_id], crate::infrastructure::asks::ask_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// `running`, `validating`, `awaiting_integration` and `needs_session`
    /// runs whose lease carries a token other than `token`, oldest run
    /// first, each with its wrapper registration. An `awaiting_integration`
    /// run is leased while its supervisor reviews and lands it (ADR-0027,
    /// ADR-0054 decision 6), a
    /// `needs_session` one while it is resumed or waits to land (task 356). Runs in other statuses
    /// and runs without a lease are not adoptable, so they are not listed.
    pub fn runs_leased_by_others(&self, token: &LeaseToken) -> Result<Vec<LeasedRun>> {
        let mut statement = self.conn.prepare(
            "SELECT r.*, l.token, l.pid, l.heartbeat_at FROM task_runs r
             JOIN run_leases l ON l.run_id=r.id
             WHERE r.status IN ('running','validating','awaiting_integration','needs_session') AND l.token<>?1
             ORDER BY r.rowid",
        )?;
        let rows = statement
            .query_map([token], |r| {
                let run = run_row(&self.runs_dir)(r)?;
                let lease = RunLease {
                    run_id: run.id().clone(),
                    token: r.get("token")?,
                    pid: r.get("pid")?,
                    heartbeat_at: r.get("heartbeat_at")?,
                };
                Ok((run, lease))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(run, lease)| {
                let wrapper = self
                    .conn
                    .query_row(
                        "SELECT * FROM run_processes WHERE run_id=?1 AND role='wrapper'",
                        [&run.id()],
                        process_row,
                    )
                    .optional()?;
                Ok(LeasedRun {
                    run,
                    lease,
                    wrapper,
                })
            })
            .collect()
    }

    /// Take over a `running`, `validating`, `awaiting_integration` or
    /// `needs_session` run whose supervisor is gone
    /// (ADR-0012): the lease row keeps its run but gets this process's
    /// `token`, `pid` and a fresh heartbeat, `task_runs.supervisor_token`
    /// follows so the lease-guarded transitions accept the adopter, and a
    /// `run_adopted` event keeps the claimer's token and pid, the age of the
    /// heartbeat it left behind, and `wrapper` as the caller observed it.
    /// The staleness of the lease under `previous_token` is re-checked
    /// inside the transaction. `Ok(None)` means nothing was taken: the lease
    /// is fresh again, released, or already carries another token (a second
    /// adopter won the race). The wrapper registration and every other row
    /// are untouched.
    pub fn adopt_run(
        &mut self,
        id: &RunId,
        previous_token: &LeaseToken,
        token: &LeaseToken,
        pid: u32,
        wrapper: serde_json::Value,
    ) -> Result<Option<TaskRun>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let lease = tx
            .query_row(
                "SELECT l.run_id,l.token,l.pid,l.heartbeat_at FROM run_leases l
                 JOIN task_runs r ON r.id=l.run_id
                 WHERE l.run_id=?1 AND l.token=?2
                 AND r.status IN ('running','validating','awaiting_integration','needs_session')",
                params![id, previous_token],
                lease_row,
            )
            .optional()?;
        let Some(lease) = lease else {
            return Ok(None);
        };
        if !lease_is_stale(&lease, now) {
            return Ok(None);
        }
        let updated = tx.execute(
            "UPDATE run_leases SET token=?3,pid=?4,heartbeat_at=?5
             WHERE run_id=?1 AND token=?2",
            params![id, previous_token, token, pid, now],
        )?;
        if updated == 0 {
            return Ok(None);
        }
        ensure!(
            tx.execute(
                "UPDATE task_runs SET supervisor_token=?2 WHERE id=?1",
                params![id, token]
            )? == 1,
            "run does not exist"
        );
        run_event(
            &tx,
            id,
            EventKind::RunAdopted,
            json!({
                "previous_token": lease.token,
                "previous_pid": lease.pid,
                "previous_heartbeat_age_secs": now - lease.heartbeat_at,
                "wrapper": wrapper,
                "token": token,
                "pid": pid,
            }),
        )?;
        let result = tx.query_row(
            "SELECT * FROM task_runs WHERE id=?1",
            [id],
            run_row(&self.runs_dir),
        )?;
        tx.commit()?;
        Ok(Some(result))
    }

    /// Lease an `awaiting_integration` run nobody holds (no lease row, or
    /// a stale one, which is replaced) to `token` for its review: the run
    /// recovered from a landing it may not land again (task 1118).
    /// `token` becomes its supervisor and `lease_acquired` (`reason:
    /// review`) is recorded. `Ok(None)` means another process took it or it
    /// changed meanwhile, or a replanning or a hold guards its task: the
    /// review leads to its landing, and a hold that came after the caller
    /// looked would leave a run in a slot that no park takes.
    pub fn lease_for_review(&mut self, id: &RunId, token: &LeaseToken) -> Result<Option<TaskRun>> {
        self.lease_awaiting(id, token, "review")
    }

    /// [`Self::lease_for_review`] for the e2e of a run queued to land
    /// (ADR-t1233-2), recorded with `reason: e2e`, refused the same way.
    pub fn lease_for_e2e(&mut self, id: &RunId, token: &LeaseToken) -> Result<Option<TaskRun>> {
        self.lease_awaiting(id, token, "e2e")
    }

    /// Take up an `awaiting_integration` run left with no lease row and no
    /// open ask (ADR-t3049-1), leasing it to `token`, recorded as
    /// `lease_acquired` with `reason: stranded_landing`. Unlike an adoption
    /// (ADR-0039 decision 1) no lease is taken over: the transaction judges
    /// the run again as [`crate::domain::stranded_landing::judge`] does, on
    /// its lease row, its unclosed asks, its task's status, replanning and
    /// hold, and its events as they stand now, and inserts the lease only
    /// for a run it takes up. `Ok(None)` otherwise: a second supervisor
    /// that judged the same run finds the first one's lease, and one that
    /// judged it before another's landing held it for a person
    /// (`integration_held`) and gave it back finds that event, and each
    /// takes nothing. The judging build is `token`'s registration's
    /// `binary_version`, as the supervisor judged with its own: a run whose
    /// landing an error stopped is taken up once by each other build
    /// ([`crate::domain::stranded_landing::build_retry`], ADR-t3524-1), and
    /// one that build or another supervisor of it took up since is not; the
    /// take-up records the error and both builds under `build_retry`.
    pub fn lease_stranded_landing(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
    ) -> Result<Option<TaskRun>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let Some(run) =
            stored_run(&tx, id)?.filter(|run| run.status() == RunStatus::AwaitingIntegration)
        else {
            return Ok(None);
        };
        let (leased, ask_open, task): (bool, bool, Option<String>) = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_leases WHERE run_id=?1),
                    EXISTS(SELECT 1 FROM asks WHERE run_id=?1 AND closed_at IS NULL),
                    (SELECT t.status FROM tasks t JOIN task_runs r ON r.task_id=t.id WHERE r.id=?1)",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let Some(task) = task else {
            return Ok(None);
        };
        let seen = crate::domain::stranded_landing::Seen {
            status: run.status(),
            leased,
            ask_open,
            fenced: replanning(&tx, id, GuardedStep::Integrate)?,
            task: task.parse()?,
        };
        let events = run_events_of(&tx, id)?;
        let build: Option<String> = tx
            .query_row(
                "SELECT binary_version FROM supervisors WHERE token=?1",
                [token],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let build = build.as_deref();
        if crate::domain::stranded_landing::judge(seen, &events, build)
            != crate::domain::stranded_landing::Stranded::TakeUp
        {
            return Ok(None);
        }
        let mut payload = json!({"reason": "stranded_landing"});
        if let Some(retry) = crate::domain::stranded_landing::build_retry(&events, build) {
            payload[crate::domain::stranded_landing::BUILD_RETRY_KEY] = retry.record();
        }
        if !lease_in(&tx, id, token, now, payload)? {
            return Ok(None);
        }
        let run = stored_run(&tx, id)?.unwrap_or(run);
        tx.commit()?;
        Ok(Some(run.relocated(&self.runs_dir)))
    }

    /// Lease a `validating` run nobody holds to `token` to verify its
    /// `already_satisfied` claim again (ADR-t1818-1 decision 3), recorded
    /// with `reason: already_satisfied`; refused while a replanning guards
    /// its task (ADR-t1521-2), whose park takes the run as it rests, or a
    /// hold does (ADR-t1879-1), under which it rests as it is.
    pub fn lease_satisfied(&mut self, id: &RunId, token: &LeaseToken) -> Result<Option<TaskRun>> {
        self.lease_unheld(
            id,
            token,
            "already_satisfied",
            RunStatus::Validating,
            Some(GuardedStep::Integrate),
        )
    }

    /// Lease run `id`, waiting where it rests (`needs_session`,
    /// `awaiting_integration`, `validating`, `failed` or `interrupted`), to
    /// `token` for the park of replanning operation `op` (read under
    /// `generation`), so that the park is one supervisor's from the stop to
    /// the settle (ADR-t1521-2): in one transaction, refused (`None`) when
    /// `op` no longer parks the run, or another process holds it; a lease
    /// that is stale is replaced, and one `token` holds already (its own
    /// park going on) is kept. Recorded as `lease_acquired` with `reason:
    /// replan_park`.
    pub fn lease_for_park(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        op: crate::domain::ReplanId,
        generation: i64,
    ) -> Result<Option<TaskRun>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let operation = crate::infrastructure::replans::read_operation(&tx, op)?;
        if operation.run_id != *id
            || operation.generation != generation
            || !operation.status.parks()
        {
            return Ok(None);
        }
        let Some(run) = stored_run(&tx, id)?.filter(|run| {
            matches!(
                run.status(),
                RunStatus::NeedsSession
                    | RunStatus::AwaitingIntegration
                    | RunStatus::Validating
                    | RunStatus::Failed
                    | RunStatus::Interrupted
            )
        }) else {
            return Ok(None);
        };
        let own = tx.execute(
            "UPDATE run_leases SET heartbeat_at=?3 WHERE run_id=?1 AND token=?2",
            params![id, token, now],
        )? == 1;
        if !own
            && !lease_in(
                &tx,
                id,
                token,
                now,
                json!({"reason": "replan_park", "operation_id": op, "generation": generation}),
            )?
        {
            return Ok(None);
        }
        let run = stored_run(&tx, id)?.unwrap_or(run);
        tx.commit()?;
        Ok(Some(run.relocated(&self.runs_dir)))
    }

    /// [`Self::lease_for_park`] for the open hold `hold` of `id`'s task while
    /// it settles (ADR-t1879-1): a run resting (as there) under a lease
    /// that is stale, or one `token` holds already, its task's latest run,
    /// is leased to `token` (`lease_acquired` with `reason: hold_park`) in
    /// one transaction that finds the hold still open and settling, so the
    /// park that takes over from a supervisor that died is one
    /// supervisor's. A fresh lease of another process, a hold released or
    /// settled, or a run that moved is refused (`None`).
    pub fn lease_for_hold_park(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        hold: crate::domain::HoldId,
    ) -> Result<Option<TaskRun>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let Some(run) = stored_run(&tx, id)?.filter(|run| {
            matches!(
                run.status(),
                RunStatus::NeedsSession
                    | RunStatus::AwaitingIntegration
                    | RunStatus::Validating
                    | RunStatus::Failed
                    | RunStatus::Interrupted
            )
        }) else {
            return Ok(None);
        };
        let open = crate::infrastructure::task_holds::open_hold_of(&tx, run.task_id())?;
        let latest = crate::infrastructure::task_holds::latest_run(&tx, run.task_id())?;
        if open.is_none_or(|open| open.id != hold || open.settled_at.is_some())
            || latest.is_none_or(|(latest, _)| latest != *id)
        {
            return Ok(None);
        }
        let own = tx.execute(
            "UPDATE run_leases SET heartbeat_at=?3 WHERE run_id=?1 AND token=?2",
            params![id, token, now],
        )? == 1;
        if !own
            && !lease_in(
                &tx,
                id,
                token,
                now,
                json!({"reason": "hold_park", "hold_id": hold}),
            )?
        {
            return Ok(None);
        }
        let run = stored_run(&tx, id)?.unwrap_or(run);
        tx.commit()?;
        Ok(Some(run.relocated(&self.runs_dir)))
    }

    /// Start the restore of the work replanning operation `op` (read under
    /// `generation`) saved for parked run `id` (ADR-t1521-2): in one
    /// transaction, [`RestoreStart::Done`] when a `replan_restored` of the
    /// operation is recorded already; else the run, `needs_session` with no
    /// lease but a stale one, is leased to `token` (`lease_acquired` with
    /// `reason: replan_restore`) and the restore is `token`'s alone
    /// ([`RestoreStart::Owned`]) until [`Self::finish_restore`]. A lease
    /// `token` holds already (a restore of its own whose end could not give
    /// it back) is taken up again. Another holder, a run or an operation
    /// that moved is [`RestoreStart::Elsewhere`].
    pub fn begin_restore(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        op: crate::domain::ReplanId,
        generation: i64,
    ) -> Result<RestoreStart> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let operation = crate::infrastructure::replans::read_operation(&tx, op)?;
        if operation.run_id != *id
            || operation.generation != generation
            || !operation.status.reopens()
        {
            return Ok(RestoreStart::Elsewhere);
        }
        if restored_by(&tx, id, op)? {
            return Ok(RestoreStart::Done);
        }
        if stored_run(&tx, id)?.is_none_or(|run| run.status() != RunStatus::NeedsSession) {
            return Ok(RestoreStart::Elsewhere);
        }
        let own = tx.execute(
            "UPDATE run_leases SET heartbeat_at=?3 WHERE run_id=?1 AND token=?2",
            params![id, token, now],
        )? == 1;
        if !own
            && !lease_in(
                &tx,
                id,
                token,
                now,
                json!({"reason": "replan_restore", "operation_id": op, "generation": generation}),
            )?
        {
            return Ok(RestoreStart::Elsewhere);
        }
        tx.commit()?;
        Ok(RestoreStart::Owned)
    }

    /// Record the end of the restore [`Self::begin_restore`] gave `token`:
    /// `restored` is the `replan_restored` payload, or why it failed, which
    /// records `replan_restore_failed` and moves operation `op` back to
    /// `blocked`, with the lease given back, in one transaction; whether it
    /// was recorded. Only while `token` still holds the run and `op` is
    /// still at `generation` and closed: a restore whose owner lost the run
    /// (its lease taken over as stale) or whose operation moved records
    /// nothing. Whatever was recorded, the lease `token` still holds is
    /// given back, so the run is held no longer than its restore.
    pub fn finish_restore(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        op: crate::domain::ReplanId,
        generation: i64,
        restored: std::result::Result<serde_json::Value, String>,
    ) -> Result<bool> {
        let recorded = self.record_restore(id, token, op, generation, restored);
        if matches!(recorded, Ok(true)) {
            return recorded;
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute(
            "DELETE FROM run_leases WHERE run_id=?1 AND token=?2",
            params![id, token],
        )? == 1
        {
            run_event(
                &tx,
                id,
                EventKind::LeaseReleased,
                json!({"reason": "replan_restore", "operation_id": op, "recorded": false}),
            )?;
        }
        tx.commit()?;
        recorded
    }

    /// Record the answer a container's executor gave to the restore of
    /// operation `op` (read under `generation`) handed for run `id`, which
    /// `token` holds in a slot for the resume the restore came before
    /// ([`Self::finish_restore`]'s records, the lease kept): only once, while
    /// `token` holds the run and `op` is still at `generation` and closed;
    /// whether it was recorded.
    pub fn record_clone_restore(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        op: crate::domain::ReplanId,
        generation: i64,
        restored: std::result::Result<serde_json::Value, String>,
    ) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        if renew_lease(&tx, id, token, now).is_err() || restored_by(&tx, id, op)? {
            return Ok(false);
        }
        let operation = crate::infrastructure::replans::read_operation(&tx, op)?;
        if operation.run_id != *id
            || operation.generation != generation
            || !operation.status.reopens()
        {
            return Ok(false);
        }
        record_restore_in(&tx, id, op, generation, &operation, restored, now)?;
        tx.commit()?;
        Ok(true)
    }

    fn record_restore(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        op: crate::domain::ReplanId,
        generation: i64,
        restored: std::result::Result<serde_json::Value, String>,
    ) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        if renew_lease(&tx, id, token, now).is_err() {
            return Ok(false);
        }
        let operation = crate::infrastructure::replans::read_operation(&tx, op)?;
        if operation.generation != generation || !operation.status.reopens() {
            return Ok(false);
        }
        record_restore_in(&tx, id, op, generation, &operation, restored, now)?;
        tx.execute(
            "DELETE FROM run_leases WHERE run_id=?1 AND token=?2",
            params![id, token],
        )?;
        run_event(
            &tx,
            id,
            EventKind::LeaseReleased,
            json!({"reason": "replan_restore", "operation_id": op}),
        )?;
        tx.commit()?;
        Ok(true)
    }
}

/// Record in `tx` the result of the restore of operation `op` for run
/// `id`: `replan_restored` with its payload, or `replan_restore_failed`
/// with why and the operation back to `blocked`.
fn record_restore_in(
    tx: &rusqlite::Transaction<'_>,
    id: &RunId,
    op: crate::domain::ReplanId,
    generation: i64,
    operation: &crate::domain::task_replan::ReplanOperation,
    restored: std::result::Result<serde_json::Value, String>,
    now: i64,
) -> Result<()> {
    match restored {
        Ok(payload) => run_event(tx, id, EventKind::ReplanRestored, payload)?,
        Err(why) => {
            run_event(
                tx,
                id,
                EventKind::ReplanRestoreFailed,
                json!({"operation_id": op, "snapshot_id": operation.snapshot_id, "reason": why}),
            )?;
            crate::infrastructure::replans::move_in(
                tx,
                op,
                generation,
                &crate::domain::task_replan::ReplanMove {
                    to: crate::domain::task_replan::ReplanStatus::Blocked,
                    reason: why,
                    snapshot_id: None,
                    diagnosis: None,
                },
                now,
            )?;
        }
    }
    Ok(())
}

impl SqliteQueue {
    fn lease_awaiting(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
    ) -> Result<Option<TaskRun>> {
        self.lease_unheld(
            id,
            token,
            reason,
            RunStatus::AwaitingIntegration,
            Some(GuardedStep::Integrate),
        )
    }

    /// Lease a run in `status` nobody holds (or whose lease is stale) to
    /// `token`, recorded with `reason`; `None` when another process holds
    /// it, it changed, or `guarded` is a step a replanning or a hold of its
    /// task refuses now.
    fn lease_unheld(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        status: RunStatus,
        guarded: Option<GuardedStep>,
    ) -> Result<Option<TaskRun>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let Some(run) = stored_run(&tx, id)?.filter(|run| run.status() == status) else {
            return Ok(None);
        };
        if let Some(step) = guarded
            && replanning(&tx, id, step)?
        {
            return Ok(None);
        }
        if !lease_in(&tx, id, token, now, json!({"reason": reason}))? {
            return Ok(None);
        }
        let run = stored_run(&tx, id)?.unwrap_or(run);
        tx.commit()?;
        Ok(Some(run.relocated(&self.runs_dir)))
    }

    /// Recovery of an orphaned run (the supervisor's, or `recover` by hand). The caller has checked that the
    /// registered processes are dead; `checked_processes` guards against a
    /// registration that happened in between, and a fresh lease is refused here
    /// again. Only this run's lease is deleted; other runs, their leases,
    /// resources and the task's `in_progress` status are left untouched. An
    /// executing run becomes `interrupted`; a run abandoned mid-integration
    /// goes back to `awaiting_integration`, since its validated result is intact.
    pub fn recover_run(
        &mut self,
        id: &RunId,
        checked_processes: usize,
        mut report: serde_json::Value,
    ) -> Result<TaskRun> {
        // The spans it closes read their transcripts first (task 543).
        let _read = read_before(&self.conn, Closing::Run(id, &[EventKind::RunRecovered]))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let fresh: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_leases WHERE run_id=?1 AND heartbeat_at >= ?2-?3)",
            params![id, self.generators.clock.now(), HEARTBEAT_TIMEOUT_SECS],
            |r| r.get(0),
        )?;
        ensure!(!fresh, "run lease heartbeat is fresh");
        let registered: i64 = tx.query_row(
            "SELECT count(*) FROM run_processes WHERE run_id=?1",
            [id],
            |r| r.get(0),
        )?;
        ensure!(
            usize::try_from(registered).ok() == Some(checked_processes),
            "run processes changed during recovery; inspect doctor again"
        );
        let previous = stored_run(&tx, id)?
            .with_context(|| format!("run {id} does not exist"))?
            .status();
        let run = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || {
                format!(
                    "run {id} is {}; only unfinished runs can be recovered",
                    previous.as_str()
                )
            },
            run::interrupt,
        )?;
        let leases_deleted = tx.execute("DELETE FROM run_leases WHERE run_id=?1", [id])?;
        report["previous_status"] = json!(previous);
        report["status"] = json!(run.status());
        report["lease_deleted"] = json!(leases_deleted == 1);
        Reason::new(ReasonCode::Orphaned).apply_to(&mut report);
        run_event(&tx, id, EventKind::RunRecovered, report)?;
        // An interrupted run ends; a landing given up waits to land or be
        // reviewed again, which the supervisor takes up on its own. A run
        // recovered again stays where it was, recorded once.
        let events = run_events_of(&tx, id)?;
        if let Some(change) = run_phase::Phase::after_recovery(run.status(), &events)
            .and_then(|phase| run_phase::next_change(&events, phase, event_kind::RUN_RECOVERED))
        {
            run_event(&tx, id, EventKind::RunPhaseChanged, change.payload())?;
        }
        // No session of it is stalled any more (ADR-0047 decision 30).
        end_stalled_detections(&tx, id, STALL_RECOVERED_CLOSED, self.generators.clock.now())?;
        // The task stays in_progress; a retry is an explicit `ready` and a new run.
        tx.commit()?;
        Ok(run.relocated(&self.runs_dir))
    }

    /// `needs_session` runs, oldest first, with their lease (if any), their
    /// wrapper registration (the latest session's) and how many resumes
    /// were started: what the supervisor judges for a resume (ADR-0019).
    pub fn runs_needing_session(&self) -> Result<Vec<ResumeCandidate>> {
        let runs = self.runs_with_status(crate::domain::RunStatus::NeedsSession)?;
        runs.into_iter()
            .map(|run| {
                let lease = self.run_lease(run.id())?;
                let wrapper = self
                    .conn
                    .query_row(
                        "SELECT * FROM run_processes WHERE run_id=?1 AND role='wrapper'",
                        [&run.id()],
                        process_row,
                    )
                    .optional()?;
                let resumes = resume_count(&self.conn, run.id())?;
                Ok(ResumeCandidate {
                    run,
                    lease,
                    wrapper,
                    resumes,
                })
            })
            .collect()
    }

    /// Take a `needs_session` run for a resume: in one transaction, check
    /// that it is still `needs_session` with resumes left (ADR-0047
    /// decision 24: [`ResumeCount::exhausted`] with `config`'s limit), no lease but a stale one (which is replaced) and no
    /// session still heartbeating, lease it to `token`, clear the previous
    /// session's process rows, make `token` its supervisor and record
    /// `resume_started` (`attempt`, the number of every resume so far and
    /// this one, `counted`, `reason`, which is the run's `last_error` when
    /// none is given, and `main`, the commit `refs/heads/<branch>` of the
    /// landing branch named when the resume began). `Ok(None)` means another
    /// process took it, it changed meanwhile, or a replanning operation
    /// guards its task.
    pub fn begin_resume(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        main: &CommitSha,
        reason: Option<&str>,
        config: resume::ResumeConfig,
    ) -> Result<Option<(TaskRun, usize)>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let Some(run) = stored_run(&tx, id)?.filter(|run| run::check_resumable(run).is_ok()) else {
            return Ok(None);
        };
        if replanning(&tx, id, GuardedStep::Resume)? {
            return Ok(None);
        }
        let events = run_events_of(&tx, id)?;
        let resumes = ResumeCount::of(&events);
        if resumes.exhausted(config) {
            return Ok(None);
        }
        // A resume of a run parked only by a conflict after its review
        // passed is not one of the counted attempts, nor one the recovery
        // job (or a person) decided after a signal from outside killed the
        // session (ADR-t946-1).
        let plan = resume::resume_start_plan(&events, main);
        let Some(previous) = lease_parked_run(&tx, id, token, now, true)? else {
            return Ok(None);
        };
        let run = if run.worker_mode() == crate::domain::worker::WorkerMode::Interactive {
            tx.execute(
                "UPDATE task_runs SET worker_mode='headless' WHERE id=?1",
                [id],
            )?;
            run_event(
                &tx,
                id,
                EventKind::WorkerModeConverted,
                json!({"phase":"resume", "from":"interactive", "to":"headless", "reason":"interactive_worker_removed"}),
            )?;
            let provider = run.actual_provider();
            run.running_on(crate::domain::worker::Worker {
                provider,
                mode: crate::domain::worker::WorkerMode::Headless,
            })
        } else {
            run
        };
        let attempt = plan.attempt;
        run_event(
            &tx,
            id,
            EventKind::LeaseAcquired,
            json!({"pid": holder_pid(&tx, token)?, "reason": "resume", "previous_token": previous}),
        )?;
        // The resumed session keeps the model and effort of the last one,
        // raised a step after a failure the task caused (ADR-0079 decisions
        // 3 and 5).
        let (session, raise) = worker_model::for_resume(&events);
        let mut started = json!({"attempt": attempt, "counted": plan.counted, "reason": reason.or(run.last_error()), "main": main});
        if let Some(started) = started.as_object_mut() {
            started.extend(session.fields_raised(run.actual_provider(), raise.as_ref()));
        }
        run_event(&tx, id, EventKind::ResumeStarted, started)?;
        if let Some(repair) = plan.repair {
            run_event(&tx, id, EventKind::AutoRepaired, repair)?;
        }
        tx.commit()?;
        Ok(Some((run.relocated(&self.runs_dir), attempt)))
    }

    /// Move a `needs_session` run on without a session because an earlier
    /// attempt already resolved it (its receipt names the clean worktree
    /// `head`, which sits on `main`): under the same checks as
    /// [`Self::begin_resume`] but whatever the attempts so far, lease it to
    /// `token` (`lease_acquired` with `reason: resume_skipped`), make
    /// `token` its supervisor and record `resume_skipped` (`head`, `main`,
    /// `approved`, `status`). An approved run stays `needs_session` with
    /// `head` as the commit it lands, for the caller to land under the
    /// lease; an unapproved one becomes `validating`. No `resume_started` is
    /// recorded, so no attempt is used, and with no session started the
    /// earlier session's `run_processes` rows are kept.
    /// `Ok(None)` means another process took it, it changed meanwhile, or
    /// a replanning operation guards its task.
    pub fn skip_resume(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        head: &CommitSha,
        main: &CommitSha,
        approved: bool,
    ) -> Result<Option<TaskRun>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        // No session starts: the earlier session's rows stay as they are.
        if replanning(&tx, id, GuardedStep::Resume)? {
            return Ok(None);
        }
        let Some(previous) = lease_parked_run(&tx, id, token, now, false)? else {
            return Ok(None);
        };
        let run = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || format!("run {id} is not needs_session"),
            |run| run::skip_resume(run, head.clone(), approved),
        )?;
        let status = run.status();
        run_event(
            &tx,
            id,
            EventKind::LeaseAcquired,
            json!({"pid": holder_pid(&tx, token)?, "reason": "resume_skipped", "previous_token": previous}),
        )?;
        run_event(
            &tx,
            id,
            EventKind::ResumeSkipped,
            json!({"head": head, "main": main, "approved": approved, "status": status.as_str()}),
        )?;
        tx.commit()?;
        Ok(Some(run.relocated(&self.runs_dir)))
    }

    /// End a resume: record `resume_finished` (with `status`, the run's
    /// status after it) and, unless the caller goes on to land the run
    /// under the same lease (`keep_lease`), release the lease. `status`
    /// `awaiting_integration` or `failed` moves the run there from
    /// `needs_session` (`reason` then becomes `last_error`); `None` keeps it
    /// `needs_session`.
    pub fn finish_resume(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        status: Option<crate::domain::RunStatus>,
        reason: Option<&str>,
        keep_lease: bool,
        payload: serde_json::Value,
    ) -> Result<TaskRun> {
        self.end_resume(id, token, status, reason, None, keep_lease, payload)
    }

    /// [`Self::finish_resume`] of an approved run its session resolved at
    /// `head`, the head of its branch: in the same transaction `head`
    /// becomes the commit it lands ([`run::resolve_approved_resume`]); the
    /// run stays `needs_session` and keeps its lease for the landing.
    pub fn finish_approved_resume(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        head: &CommitSha,
        payload: serde_json::Value,
    ) -> Result<TaskRun> {
        self.end_resume(id, token, None, None, Some(head), true, payload)
    }

    #[allow(clippy::too_many_arguments)]
    fn end_resume(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        status: Option<crate::domain::RunStatus>,
        reason: Option<&str>,
        landing: Option<&CommitSha>,
        keep_lease: bool,
        payload: serde_json::Value,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        if let Some(head) = landing {
            apply(
                &tx,
                refusals(&self.runs_dir, &self.generators),
                id,
                None,
                || format!("run {id} is not needs_session"),
                |run| run::resolve_approved_resume(run, head.clone()),
            )?;
        }
        if let Some(status) = status {
            apply(
                &tx,
                refusals(&self.runs_dir, &self.generators),
                id,
                None,
                || format!("run {id} is not needs_session"),
                |run| run::finish_resume(run, status, reason.map(str::to_owned)),
            )?;
        }
        let finished = run::resume_finished(status, payload);
        let mut payload = finished.payload;
        // The work of the resumed session, closed when it exited (task 514).
        let resumed: i64 = tx.query_row(&last_resume_started_sql(), [id], |r| r.get(0))?;
        if let Some(work) = crate::infrastructure::sessions::closed_work(
            &tx,
            id,
            crate::domain::sessions::RESUME,
            crate::domain::EventId::new(resumed),
        )? {
            payload["work_breakdown"] = work;
        }
        // Its tokens (task 199).
        if let Some(tokens) = crate::infrastructure::sessions::closed_tokens(
            &tx,
            id,
            crate::domain::sessions::RESUME,
            crate::domain::EventId::new(resumed),
        )? {
            payload["tokens"] = tokens;
        }
        run_event(&tx, id, finished.kind, payload)?;
        if !keep_lease {
            tx.execute(
                "DELETE FROM run_leases WHERE run_id=?1 AND token=?2",
                params![id, token],
            )?;
            run_event(
                &tx,
                id,
                EventKind::LeaseReleased,
                json!({"reason": "resume_finished"}),
            )?;
        }
        let result = tx.query_row(
            "SELECT * FROM task_runs WHERE id=?1",
            [id],
            run_row(&self.runs_dir),
        )?;
        tx.commit()?;
        Ok(result)
    }
    /// The latest run of every `in_progress` task that is `failed` or
    /// `interrupted`, oldest first: the runs the triage looks at. An older
    /// run of a retried task is history.
    pub fn runs_to_triage(&self) -> Result<Vec<TaskRun>> {
        Ok(self
            .latest_runs_in_progress()?
            .into_iter()
            .filter(|run| run::check_triageable(run).is_ok())
            .collect())
    }

    /// Take a `failed` or `interrupted` run for a recovery round: in one
    /// transaction, check that its task is `in_progress`, that it has no
    /// lease but a stale one (which is replaced), that no round took it
    /// since its last resume or that the `wait` of the last one is over
    /// ([`crate::domain::triage_state`]), and that it is still the task's
    /// latest run, lease it to `token` and record `lease_acquired`,
    /// `request` as `recovery_requested` when given, and `triage_started`
    /// (`attempt`, `status`, the session id of a Claude job and `launch`).
    /// `Ok(None)` means another process took it, it changed, or a
    /// replanning operation guards its task.
    pub fn begin_triage(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        request: Option<serde_json::Value>,
        launch: &crate::domain::actor_model::ActorLaunch,
    ) -> Result<Option<(TaskRun, usize)>> {
        // The spans it closes read their transcripts first (task 543).
        let _read = read_before(&self.conn, Closing::Run(id, &[EventKind::TriageStarted]))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let run = tx
            .query_row(
                "SELECT r.* FROM task_runs r JOIN tasks t ON t.id=r.task_id
                 WHERE r.id=?1 AND r.status IN ('failed','interrupted') AND t.status='in_progress'
                 AND r.rowid=(SELECT MAX(rowid) FROM task_runs WHERE task_id=r.task_id)",
                [id],
                run_row(&self.runs_dir),
            )
            .optional()?;
        let Some(run) = run else {
            return Ok(None);
        };
        if replanning(&tx, id, GuardedStep::Recover)? {
            return Ok(None);
        }
        let events = run_events_of(&tx, id)?;
        match crate::domain::triage_state(&events) {
            crate::domain::TriageState::Pending => {}
            crate::domain::TriageState::Waiting { until } if until <= now => {}
            _ => return Ok(None),
        }
        let lease = tx
            .query_row(
                "SELECT run_id,token,pid,heartbeat_at FROM run_leases WHERE run_id=?1",
                [id],
                lease_row,
            )
            .optional()?;
        if let Some(lease) = &lease {
            if !lease_is_stale(lease, now) {
                return Ok(None);
            }
            tx.execute("DELETE FROM run_leases WHERE run_id=?1", [id])?;
        }
        tx.execute(
            "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,?2,?3,?4)",
            params![id, token, holder_pid(&tx, token)?, now],
        )?;
        let attempt = RunHistory::from_events(&events).triage_attempts() + 1;
        run_event(
            &tx,
            id,
            EventKind::LeaseAcquired,
            json!({"pid": holder_pid(&tx, token)?, "reason": "triage", "previous_token": lease.map(|l| l.token)}),
        )?;
        if let Some(request) = request {
            run_event(&tx, id, EventKind::RecoveryRequested, request)?;
        }
        // The job's Claude session id (ADR-0048 decision 4); Codex names
        // its thread itself, which the round's end records (ADR-t1063-1
        // decision 6).
        let session_id = (launch.provider == crate::domain::Provider::Claude)
            .then(|| self.generators.ids.uuid());
        run_event(
            &tx,
            id,
            EventKind::TriageStarted,
            json!({"attempt": attempt, "status": run.status().as_str(), "session_id": session_id, "launch": launch.to_value()}),
        )?;
        tx.commit()?;
        Ok(Some((run, attempt)))
    }

    /// Act on a recovery round under its lease and record
    /// `triage_finished` with `payload`, the `action` and the run's status
    /// after it, then each of `also`, in one transaction: `Retry` makes the
    /// task `ready`, `RetryInherit` too with the branch to carry over,
    /// `Resume` the run `needs_session` (still the latest run of a task not
    /// `completed` or `canceled`, checked again), `HandOff` the run `validating`
    /// (still the latest run of its `in_progress` task, checked again),
    /// `Wait` and `Ask` change nothing.
    /// An action that moves on a task its hold guards (ADR-t1879-1
    /// decision 6), read in the same transaction, is kept instead
    /// ([`keep_held_verdict`]). The lease stays for the workspace's close.
    pub fn finish_triage(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        action: &TriageAction,
        mut payload: serde_json::Value,
        also: Vec<(EventKind, serde_json::Value)>,
    ) -> Result<TaskRun> {
        // The spans it closes read their transcripts first (task 543).
        let kinds: Vec<EventKind> = std::iter::once(EventKind::TriageFinished)
            .chain(also.iter().map(|(kind, _)| *kind))
            .collect();
        let _read = read_before(&self.conn, Closing::Run(id, &kinds))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        let run = tx.query_row(
            "SELECT * FROM task_runs WHERE id=?1",
            [id],
            run_row(&self.runs_dir),
        )?;
        ensure!(
            run::check_triageable(&run).is_ok(),
            "run {id} is {}; only failed or interrupted runs are triaged",
            run.status().as_str()
        );
        let held = if action.moves_on() {
            crate::infrastructure::task_holds::guard_of(&tx, run.task_id())?
        } else {
            None
        };
        if let Some(hold) = held {
            keep_held_verdict(&tx, &run, hold, action, payload, also)?;
        } else {
            apply_triage_action(
                &tx,
                refusals(&self.runs_dir, &self.generators),
                &run,
                action,
                &mut payload,
            )?;
            let result = tx.query_row(
                "SELECT * FROM task_runs WHERE id=?1",
                [id],
                run_row(&self.runs_dir),
            )?;
            payload["action"] = json!(action.as_str());
            payload["status"] = json!(result.status().as_str());
            run_event(&tx, id, EventKind::TriageFinished, payload)?;
            for (kind, payload) in also {
                run_event(&tx, id, kind, payload)?;
            }
        }
        let result = tx.query_row(
            "SELECT * FROM task_runs WHERE id=?1",
            [id],
            run_row(&self.runs_dir),
        )?;
        tx.commit()?;
        Ok(result)
    }

    /// End a `needs_session` run whose resumes are used up (ADR-0024's
    /// Consequences, ADR-0047 decision 24): in one transaction, check that
    /// it is still `needs_session` with its resumes used up
    /// ([`ResumeCount::exhausted`] with `config`'s limit), the latest run of an `in_progress`
    /// task, and not leased but stale; make it `failed` with `reason` as
    /// `last_error`. [`Exhaustion::Recover`] records `recovery_requested`
    /// (`alert: resume_exhausted`, `by: runtime`), which the next recovery
    /// round of the run takes; [`Exhaustion::Inherit`] makes the task
    /// `ready` again for a run that carries this run's branch over, recorded
    /// as `triage_finished` (action `retry_inherit`, `by: runtime`) with
    /// `auto_repaired` (`repair: inherit_retry`), so no round takes it.
    /// `Ok(None)` means it changed, or a hold or a replanning guards its
    /// task.
    pub fn exhaust_resumes(
        &mut self,
        id: &RunId,
        exhaustion: &Exhaustion,
        reason: &str,
        config: resume::ResumeConfig,
    ) -> Result<Option<TaskRun>> {
        // The spans it closes read their transcripts first (task 543).
        let _read = read_before(&self.conn, Closing::Run(id, &[EventKind::TriageFinished]))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let run = tx
            .query_row(
                "SELECT r.* FROM task_runs r JOIN tasks t ON t.id=r.task_id
                 WHERE r.id=?1 AND r.status='needs_session' AND t.status='in_progress'
                 AND r.rowid=(SELECT MAX(rowid) FROM task_runs WHERE task_id=r.task_id)",
                [id],
                run_row(&self.runs_dir),
            )
            .optional()?;
        let Some(run) = run else {
            return Ok(None);
        };
        // Neither failed nor retried while its task is held (ADR-t1879-1),
        // nor while a replanning guards it (ADR-t1521-2): the park keeps
        // the run, its work and its resumes for the operation's outcome.
        if crate::infrastructure::task_holds::guard_of(&tx, run.task_id())?.is_some()
            || crate::infrastructure::replans::guard_of(&tx, run.task_id())?.is_some()
        {
            return Ok(None);
        }
        let events = run_events_of(&tx, id)?;
        let resumes = ResumeCount::of(&events);
        let leased = tx
            .query_row(
                "SELECT run_id,token,pid,heartbeat_at FROM run_leases WHERE run_id=?1",
                [id],
                lease_row,
            )
            .optional()?
            .is_some_and(|lease| !lease_is_stale(&lease, now));
        if !resumes.exhausted(config) || leased {
            return Ok(None);
        }
        // Once per task, and only for a run still parked by a conflict
        // alone: checked again here, where no other supervisor can retry it.
        if matches!(exhaustion, Exhaustion::Inherit { .. }) {
            let task_events: Vec<RunEvent> = tx
                .prepare("SELECT * FROM run_events WHERE task_id=?1 ORDER BY id")?
                .query_map([run.task_id()], event_row)?
                .collect::<rusqlite::Result<_>>()?;
            if !resume::inherits_on_exhaustion(&events, &task_events) {
                return Ok(None);
            }
        }
        tx.execute("DELETE FROM run_leases WHERE run_id=?1", [id])?;
        let mut recorded = Vec::new();
        let result = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || format!("run {id} changed"),
            |run| {
                let (run, events) =
                    run::record_exhausted_resumes(run, reason, exhaustion, resumes, &events)?;
                recorded = events;
                Ok(run)
            },
        )?;
        // The task is ready again before the run's events, as it always
        // was in the event log.
        if matches!(exhaustion, Exhaustion::Inherit { .. }) {
            crate::infrastructure::sqlite::transition_task(
                &tx,
                run.task_id(),
                TaskAction::Ready,
                &self.generators.clock.timestamp(),
            )?;
        }
        record_events(&tx, id, recorded)?;
        tx.commit()?;
        Ok(Some(result.relocated(&self.runs_dir)))
    }

    /// Apply the answer of a triage's `decide` ask to its `failed` or
    /// `interrupted` run that nobody leases and that is still its task's
    /// latest run, and close the ask, in one transaction
    /// ([`decide_triage_in`]).
    pub fn decide_triage(
        &mut self,
        id: &RunId,
        ask_id: AskId,
        answer: &str,
        reason: &str,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = decide_triage_in(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            ask_id,
            answer,
            reason,
        )?;
        tx.commit()?;
        Ok(result)
    }

    /// The guard the open hold of `task` puts on its steps, if any.
    pub fn hold_guard(&self, task: TaskId) -> Result<Option<HoldGuard>> {
        crate::infrastructure::task_holds::guard_of(&self.conn, task)
    }

    /// The tasks a claim would take but for their open hold.
    pub fn held_claims(&self) -> Result<Vec<HoldGuard>> {
        crate::infrastructure::sqlite::held_claims(&self.conn)
    }

    /// Record `hold_deferred` once per hold and step (see
    /// [`RunRecovery::note_hold_deferred`]).
    pub fn note_hold_deferred(
        &mut self,
        hold: HoldGuard,
        run: Option<&RunId>,
        step: GuardedStep,
        supervisor: &LeaseToken,
    ) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let noted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_events WHERE task_id=?1 AND kind=?2
               AND json_extract(payload,'$.hold_id')=?3 AND json_extract(payload,'$.step')=?4)",
            params![
                hold.task,
                event_kind::HOLD_DEFERRED,
                hold.hold,
                step.as_str()
            ],
            |r| r.get(0),
        )?;
        if noted {
            return Ok(false);
        }
        crate::infrastructure::sqlite::event(
            &tx,
            hold.task,
            run,
            EventKind::HoldDeferred,
            json!({"hold_id": hold.hold, "step": step, "supervisor": supervisor}),
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Keep a person's answer to a triage ask while its task is held (see
    /// [`RunRecovery::keep_triage_answer`]).
    pub fn keep_triage_answer(
        &mut self,
        id: &RunId,
        ask_id: AskId,
        answer: &str,
        reason: &str,
    ) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = task_of(&tx, id)?;
        let Some(hold) = crate::infrastructure::task_holds::guard_of(&tx, task)? else {
            return Ok(false);
        };
        if unsettled_kept_answer(&tx, id, ask_id)?.is_none() {
            let version = crate::infrastructure::task_holds::progress_version(&tx, task, id)?;
            run_event(
                &tx,
                id,
                EventKind::HoldResultKept,
                json!({"source": KeptSource::TriageAnswer, "hold_id": hold.hold,
                       "ask_id": ask_id, "answer": answer, "reason": reason,
                       "version": version}),
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Keep a recovery job's verdict on a live session while its task is
    /// held (see [`RunRecovery::keep_live_verdict`]).
    pub fn keep_live_verdict(&mut self, id: &RunId, verdict: serde_json::Value) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = task_of(&tx, id)?;
        let Some(hold) = crate::infrastructure::task_holds::guard_of(&tx, task)? else {
            return Ok(false);
        };
        let version = crate::infrastructure::task_holds::progress_version(&tx, task, id)?;
        run_event(
            &tx,
            id,
            EventKind::HoldResultKept,
            json!({"source": KeptSource::LiveVerdict, "hold_id": hold.hold,
                   "verdict": verdict, "version": version}),
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Close each hold left open on a task that ended, as its end would
    /// have ([`crate::infrastructure::task_holds::end_with_task`]), the
    /// task unchanged.
    pub fn end_holds_of_ended_tasks(&mut self) -> Result<Vec<TaskId>> {
        let now = self.generators.clock.now();
        crate::infrastructure::task_holds::end_holds_of_ended(&mut self.conn, now)
    }

    /// The kept results whose hold is released and that are not settled.
    pub fn released_kept_results(&self) -> Result<Vec<EventId>> {
        Ok(self
            .conn
            .prepare(
                "SELECT e.id FROM run_events e WHERE e.kind=?1
                   AND NOT EXISTS (SELECT 1 FROM run_events s WHERE s.run_id=e.run_id
                     AND s.kind IN (?2,?3) AND json_extract(s.payload,'$.kept_id')=e.id)
                   AND NOT EXISTS (SELECT 1 FROM task_holds h WHERE h.task_id=e.task_id
                     AND h.released_at IS NULL)
                 ORDER BY e.id",
            )?
            .query_map(
                params![
                    event_kind::HOLD_RESULT_KEPT,
                    event_kind::HOLD_RESULT_APPLIED,
                    event_kind::HOLD_RESULT_DISCARDED
                ],
                |r| r.get(0),
            )?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Apply or discard the kept result `kept` once its hold is released
    /// (ADR-t1879-1 decision 6), in one transaction that reads the hold,
    /// the task and the run: unchanged since it was kept, it is applied by
    /// the path that would have applied it (its preconditions checked
    /// again) and recorded as `hold_result_applied`; otherwise, or when
    /// that path refuses it now, it is discarded as `hold_result_discarded`
    /// with the reason, a kept answer's ask closed with that reason
    /// ([`crate::infrastructure::asks::discard_by_runtime`]). A discarded result
    /// leaves the run as if none came: its next recovery round is due.
    pub fn settle_kept_result(&mut self, kept: EventId) -> Result<KeptOutcome> {
        let mut tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(event) = tx
            .query_row(
                "SELECT * FROM run_events WHERE id=?1 AND kind=?2",
                params![kept, event_kind::HOLD_RESULT_KEPT],
                crate::infrastructure::sqlite::event_row,
            )
            .optional()?
        else {
            return Ok(KeptOutcome::Untouched);
        };
        let (Some(task), Some(run_id)) = (event.task_id, event.run_id.clone()) else {
            return Ok(KeptOutcome::Untouched);
        };
        let settled: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_events WHERE run_id=?1 AND kind IN (?2,?3)
               AND json_extract(payload,'$.kept_id')=?4)",
            params![
                run_id,
                event_kind::HOLD_RESULT_APPLIED,
                event_kind::HOLD_RESULT_DISCARDED,
                kept
            ],
            |r| r.get(0),
        )?;
        if settled || crate::infrastructure::task_holds::guard_of(&tx, task)?.is_some() {
            return Ok(KeptOutcome::Untouched);
        }
        let source: KeptSource = event.payload["source"]
            .as_str()
            .unwrap_or_default()
            .parse()?;
        let version: ProgressVersion = serde_json::from_value(event.payload["version"].clone())?;
        let now = crate::infrastructure::task_holds::progress_version(&tx, task, &run_id)?;
        let refusals = refusals(&self.runs_dir, &self.generators);
        let (outcome, error) = match task_hold::disposition(source, &version, &now) {
            Disposition::Discard(reason) => (KeptOutcome::Discarded(reason), None),
            Disposition::Apply => {
                let savepoint = tx.savepoint()?;
                // Recorded before the apply, so an apply that ends the task
                // finds it settled and does not discard it with the task's
                // end; a refusal rolls it back with the rest.
                run_event(
                    &savepoint,
                    &run_id,
                    EventKind::HoldResultApplied,
                    json!({"kept_id": kept, "source": source}),
                )?;
                match apply_kept(&savepoint, refusals, &run_id, kept, source, &event.payload) {
                    Ok(run) => {
                        savepoint.commit()?;
                        (KeptOutcome::Applied(Box::new(run)), None)
                    }
                    Err(error) => (
                        KeptOutcome::Discarded(DiscardReason::Refused),
                        Some(format!("{error:#}")),
                    ),
                }
            }
        };
        if let KeptOutcome::Discarded(reason) = &outcome {
            // A discarded answer's ask closes with the discard's reason.
            if let Some(ask) = task_hold::KeptResult::of_event(&event)
                .and_then(task_hold::KeptResult::ask_to_close)
                && let Some(ask) = tx
                    .query_row(
                        "SELECT * FROM asks WHERE id=?1 AND closed_at IS NULL",
                        params![ask],
                        crate::infrastructure::asks::ask_row,
                    )
                    .optional()?
            {
                let note = match reason {
                    DiscardReason::TaskEnded => format!(
                        "task {task} {}: the answer kept while it was held is discarded",
                        now.task_status.as_str()
                    ),
                    _ => format!(
                        "the answer kept while task {task} was held is discarded: {}",
                        reason.as_str()
                    ),
                };
                crate::infrastructure::asks::discard_by_runtime(
                    &tx,
                    &ask,
                    reason.as_str(),
                    &note,
                    self.generators.clock.now(),
                )?;
            }
            // A round or a decision after the kept result (applied
            // in the window before this pass read the release) stays
            // the run's last word.
            let superseded: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM run_events WHERE run_id=?1 AND id>?2
                       AND kind IN ('triage_started','triage_finished','triage_decided',
                                    'triage_failed','resume_started'))",
                params![run_id, kept],
                |r| r.get(0),
            )?;
            run_event(
                &tx,
                &run_id,
                EventKind::HoldResultDiscarded,
                json!({"kept_id": kept, "source": source, "reason": reason, "error": error,
                           task_hold::SUPERSEDED: superseded}),
            )?;
        }
        tx.commit()?;
        Ok(outcome)
    }

    /// A person's retry that carries the task's latest run over
    /// (`ready --inherit`, ADR-t1962-1), in one transaction as immediate as
    /// [`Self::begin_triage`]'s: check the task's status against the
    /// authorized one, that the run `request.at` read is still its latest
    /// run, with the same status and validated commit, and the
    /// preconditions of [`crate::domain::inherit_by_hand::check`] (a lease
    /// that is not stale refuses; a stale one is removed, so that the round
    /// that held it can neither finish nor apply anything after this). Then
    /// make the task `ready` without plan review, close the recovery job's
    /// unclosed `decide` asks of the run (an open one answered by the
    /// runtime), and record `triage_finished` (action `retry_inherit`) with
    /// `by`, `reason`, `inherit`, `closed_asks` and `removed_lease`, which
    /// the next run inherits. A refusal changes nothing.
    pub fn inherit_by_hand(
        &mut self,
        request: crate::application::inherit::InheritRequest,
    ) -> Result<crate::application::inherit::InheritedByHand> {
        use crate::domain::inherit_by_hand::{InheritFacts, InheritRefusal, check};
        let task_id = request.task;
        let refused =
            |refusal: InheritRefusal| anyhow!("task {task_id} is not carried over: {refusal}");
        // The spans it closes read their transcripts first (task 543).
        let _read = match &request.at.run {
            Some(run) => Some(read_before(
                &self.conn,
                Closing::Run(&run.id, &[EventKind::TriageFinished]),
            )?),
            None => None,
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        let task = read_task(&tx, task_id)?;
        crate::domain::task::check_status_authorized(&task, request.authorized)?;
        crate::domain::task::check_status_authorized(&task, request.at.task)?;
        let latest = tx
            .query_row(
                "SELECT * FROM task_runs WHERE task_id=?1 ORDER BY rowid DESC LIMIT 1",
                [task_id],
                run_row(&self.runs_dir),
            )
            .optional()?;
        ensure!(
            latest
                .as_ref()
                .map(crate::application::inherit::InheritRunAt::of)
                == request.at.run,
            "task {task_id}'s latest run changed while it was read; nothing is carried over, try again"
        );
        let (lease, process_alive) = match &latest {
            Some(run) => (
                run_lease_of(&tx, run.id())?,
                tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM run_processes WHERE run_id=?1
                     AND exited_at IS NULL AND heartbeat_at >= ?2-?3)",
                    params![run.id(), now, HEARTBEAT_TIMEOUT_SECS],
                    |r| r.get(0),
                )?,
            ),
            None => (None, false),
        };
        check(InheritFacts {
            role: request.role,
            task: task.status(),
            latest_run: latest.as_ref().map(TaskRun::status),
            leased: lease.as_ref().is_some_and(|l| !lease_is_stale(l, now)),
            process_alive,
            own_commits: request.head.is_some(),
        })
        .map_err(refused)?;
        let (Some(run), Some(head)) = (latest, request.head) else {
            unreachable!("checked above");
        };
        let id = run.id();
        // The stale lease goes, so its holder cannot renew it and finish or
        // apply its round after this (ADR-0039 decision 7).
        let removed_lease = match lease {
            Some(lease) => {
                tx.execute("DELETE FROM run_leases WHERE run_id=?1", [id])?;
                Some(lease.token)
            }
            None => None,
        };
        let task = crate::infrastructure::sqlite::transition_task(
            &tx,
            task_id,
            TaskAction::Ready,
            &self.generators.clock.timestamp(),
        )?;
        let decides: Vec<crate::domain::Ask> = tx
            .prepare(
                "SELECT * FROM asks WHERE run_id=?1 AND kind='decide' AND asked_by=?2
                 AND closed_at IS NULL ORDER BY id",
            )?
            .query_map(
                params![id, TRIAGE_ASKER],
                crate::infrastructure::asks::ask_row,
            )?
            .collect::<rusqlite::Result<_>>()?;
        let answer = format!(
            "carried over by hand ({} ran `dagq ready --inherit`): {}",
            request.role.as_str(),
            request.reason
        );
        for ask in &decides {
            crate::infrastructure::asks::close_by_runtime(&tx, ask, &answer, now)?;
        }
        let closed_asks: Vec<AskId> = decides.iter().map(|ask| ask.id).collect();
        let branch = run.branch().map(str::to_owned);
        run_event(
            &tx,
            id,
            EventKind::TriageFinished,
            json!({
                "action": resume::RETRY_INHERIT,
                "by": request.role.as_str(),
                "reason": request.reason,
                "inherit": {"branch": branch, "head": head},
                "closed_asks": closed_asks,
                "removed_lease": removed_lease,
                "status": run.status().as_str(),
            }),
        )?;
        tx.commit()?;
        Ok(crate::application::inherit::InheritedByHand {
            task,
            run_id: id.clone(),
            branch,
            head,
            closed_asks,
            removed_lease,
        })
    }

    /// The answered `decide` asks the supervisor opened about a `failed` or
    /// `interrupted` run that nobody closed, oldest first: the triage's
    /// asks whose answers the supervisor applies.
    pub fn triage_answers(&self) -> Result<Vec<crate::domain::Ask>> {
        Ok(self
            .asks(crate::infrastructure::asks::AskQuery::default())?
            .into_iter()
            .filter(|ask| {
                ask.kind == crate::domain::AskKind::Decide
                    && ask.asked_by == TRIAGE_ASKER
                    && ask.run_id.is_some()
                    && ask.answered_at.is_some()
            })
            .collect())
    }
}

/// The port execution and landing publishes to `ready --inherit` (T10).
impl crate::application::inherit::InheritStore for SqliteQueue {
    fn inherit_by_hand(
        &mut self,
        request: crate::application::inherit::InheritRequest,
    ) -> Result<crate::application::inherit::InheritedByHand> {
        SqliteQueue::inherit_by_hand(self, request)
    }

    fn record_images(&mut self, records: &[(EventKind, serde_json::Value)]) -> Result<()> {
        crate::application::EventStore::record_queue_events(self, records.to_vec()).map(drop)
    }
}

/// The [`RunRecovery`] port over the inherent methods above, which callers
/// that hold a `SqliteQueue` keep using directly.
impl RunRecovery for SqliteQueue {
    fn runs_leased_by_others(&self, token: &LeaseToken) -> Result<Vec<LeasedRun>> {
        SqliteQueue::runs_leased_by_others(self, token)
    }
    fn runs_leased_by(&self, token: &LeaseToken) -> Result<Vec<TaskRun>> {
        SqliteQueue::runs_leased_by(self, token)
    }
    fn unclosed_run_asks(&self, run_id: &RunId) -> Result<Vec<crate::domain::Ask>> {
        SqliteQueue::unclosed_run_asks(self, run_id)
    }
    fn adopt_run(
        &mut self,
        id: &RunId,
        previous_token: &LeaseToken,
        token: &LeaseToken,
        pid: u32,
        wrapper: serde_json::Value,
    ) -> Result<Option<TaskRun>> {
        SqliteQueue::adopt_run(self, id, previous_token, token, pid, wrapper)
    }
    fn recover_run(
        &mut self,
        id: &RunId,
        checked_processes: usize,
        report: serde_json::Value,
    ) -> Result<TaskRun> {
        SqliteQueue::recover_run(self, id, checked_processes, report)
    }
    fn lease_for_review(&mut self, id: &RunId, token: &LeaseToken) -> Result<Option<TaskRun>> {
        SqliteQueue::lease_for_review(self, id, token)
    }
    fn lease_for_e2e(&mut self, id: &RunId, token: &LeaseToken) -> Result<Option<TaskRun>> {
        SqliteQueue::lease_for_e2e(self, id, token)
    }
    fn lease_stranded_landing(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
    ) -> Result<Option<TaskRun>> {
        SqliteQueue::lease_stranded_landing(self, id, token)
    }
    fn lease_satisfied(&mut self, id: &RunId, token: &LeaseToken) -> Result<Option<TaskRun>> {
        SqliteQueue::lease_satisfied(self, id, token)
    }
    fn runs_to_triage(&self) -> Result<Vec<TaskRun>> {
        SqliteQueue::runs_to_triage(self)
    }
    fn begin_triage(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        request: Option<serde_json::Value>,
        launch: &crate::domain::actor_model::ActorLaunch,
    ) -> Result<Option<(TaskRun, usize)>> {
        SqliteQueue::begin_triage(self, id, token, request, launch)
    }
    fn finish_triage(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        action: &TriageAction,
        payload: serde_json::Value,
        also: Vec<(EventKind, serde_json::Value)>,
    ) -> Result<TaskRun> {
        SqliteQueue::finish_triage(self, id, token, action, payload, also)
    }
    fn decide_triage(
        &mut self,
        id: &RunId,
        ask_id: AskId,
        answer: &str,
        reason: &str,
    ) -> Result<TaskRun> {
        SqliteQueue::decide_triage(self, id, ask_id, answer, reason)
    }
    fn runs_needing_session(&self) -> Result<Vec<ResumeCandidate>> {
        SqliteQueue::runs_needing_session(self)
    }
    fn begin_resume(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        main: &CommitSha,
        reason: Option<&str>,
        config: resume::ResumeConfig,
    ) -> Result<Option<(TaskRun, usize)>> {
        SqliteQueue::begin_resume(self, id, token, main, reason, config)
    }
    fn finish_resume(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        status: Option<RunStatus>,
        reason: Option<&str>,
        keep_lease: bool,
        payload: serde_json::Value,
    ) -> Result<TaskRun> {
        SqliteQueue::finish_resume(self, id, token, status, reason, keep_lease, payload)
    }
    fn finish_approved_resume(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        head: &CommitSha,
        payload: serde_json::Value,
    ) -> Result<TaskRun> {
        SqliteQueue::finish_approved_resume(self, id, token, head, payload)
    }
    fn skip_resume(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        head: &CommitSha,
        main: &CommitSha,
        approved: bool,
    ) -> Result<Option<TaskRun>> {
        SqliteQueue::skip_resume(self, id, token, head, main, approved)
    }
    fn exhaust_resumes(
        &mut self,
        id: &RunId,
        exhaustion: &Exhaustion,
        reason: &str,
        config: resume::ResumeConfig,
    ) -> Result<Option<TaskRun>> {
        SqliteQueue::exhaust_resumes(self, id, exhaustion, reason, config)
    }
    fn hold_guard(&self, task: TaskId) -> Result<Option<HoldGuard>> {
        SqliteQueue::hold_guard(self, task)
    }
    fn held_claims(&self) -> Result<Vec<HoldGuard>> {
        SqliteQueue::held_claims(self)
    }
    fn note_hold_deferred(
        &mut self,
        hold: HoldGuard,
        run: Option<&RunId>,
        step: GuardedStep,
        supervisor: &LeaseToken,
    ) -> Result<bool> {
        SqliteQueue::note_hold_deferred(self, hold, run, step, supervisor)
    }
    fn keep_triage_answer(
        &mut self,
        id: &RunId,
        ask_id: AskId,
        answer: &str,
        reason: &str,
    ) -> Result<bool> {
        SqliteQueue::keep_triage_answer(self, id, ask_id, answer, reason)
    }
    fn keep_live_verdict(&mut self, id: &RunId, verdict: serde_json::Value) -> Result<bool> {
        SqliteQueue::keep_live_verdict(self, id, verdict)
    }
    fn end_holds_of_ended_tasks(&mut self) -> Result<Vec<TaskId>> {
        SqliteQueue::end_holds_of_ended_tasks(self)
    }
    fn released_kept_results(&self) -> Result<Vec<EventId>> {
        SqliteQueue::released_kept_results(self)
    }
    fn settle_kept_result(&mut self, kept: EventId) -> Result<KeptOutcome> {
        SqliteQueue::settle_kept_result(self, kept)
    }
}

/// The task of run `id`.
fn task_of(conn: &Connection, id: &RunId) -> Result<TaskId> {
    Ok(
        conn.query_row("SELECT task_id FROM task_runs WHERE id=?1", [id], |r| {
            r.get(0)
        })?,
    )
}

/// Move `run` and its task on as a recovery round's `action` says,
/// adding to the round's `payload` what the action records.
fn apply_triage_action(
    conn: &Connection,
    refusals: Refusals<'_>,
    run: &TaskRun,
    action: &TriageAction,
    payload: &mut serde_json::Value,
) -> Result<()> {
    let id = run.id();
    let now = refusals.generators.clock.timestamp();
    match action {
        TriageAction::Retry => {
            crate::infrastructure::sqlite::transition_task(
                conn,
                run.task_id(),
                TaskAction::Ready,
                &now,
            )?;
        }
        TriageAction::RetryInherit { branch, head } => {
            // Once per task, checked again where no other supervisor
            // can retry it.
            let task_events: Vec<RunEvent> = conn
                .prepare("SELECT * FROM run_events WHERE task_id=?1 ORDER BY id")?
                .query_map([run.task_id()], event_row)?
                .collect::<rusqlite::Result<_>>()?;
            ensure!(
                !task_events.iter().any(resume::uses_automatic_inherit),
                "task {} was retried with a branch carried over already",
                run.task_id()
            );
            crate::infrastructure::sqlite::transition_task(
                conn,
                run.task_id(),
                TaskAction::Ready,
                &now,
            )?;
            payload["inherit"] = json!({"branch": branch, "head": head});
        }
        TriageAction::Wait { recheck_at } => payload["recheck_at"] = json!(recheck_at),
        TriageAction::Resume { instruction } => {
            ensure_revivable(conn, id)?;
            apply(
                conn,
                refusals,
                id,
                None,
                || format!("run {id} changed"),
                |run| run::resume_after_triage(run, instruction.clone()),
            )?;
            payload["instruction"] = json!(instruction);
            payload["code"] = json!(ReasonCode::TriageResume);
        }
        TriageAction::HandOff => {
            // Still the latest run of a task nobody moved on, checked again
            // where no person's `ready` or answer can come in between.
            let latest: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM task_runs r JOIN tasks t ON t.id=r.task_id
                 WHERE r.id=?1 AND t.status='in_progress'
                 AND r.rowid=(SELECT MAX(rowid) FROM task_runs WHERE task_id=r.task_id))",
                [id],
                |r| r.get(0),
            )?;
            ensure!(
                latest,
                "run {id} is no longer the latest run of an in_progress task"
            );
            apply(
                conn,
                refusals,
                id,
                None,
                || format!("run {id} changed"),
                run::hand_off_after_triage,
            )?;
        }
        TriageAction::Ask { ask_id } => payload["ask_id"] = json!(ask_id),
    }
    Ok(())
}

/// Keep, instead of applying, a recovery round's `action` on `run` whose
/// task `hold` guards (ADR-t1879-1 decision 6): record `triage_finished`
/// with action `held` (so the run's next round is due once the hold is
/// released, [`crate::domain::triage_state`]) and what the round would
/// have done, `also` without its repairs and with `recovery_finished`
/// saying nothing was applied, then `hold_result_kept` with the action
/// and the version of the task and the run as they are kept.
fn keep_held_verdict(
    conn: &Connection,
    run: &TaskRun,
    hold: HoldGuard,
    action: &TriageAction,
    mut payload: serde_json::Value,
    also: Vec<(EventKind, serde_json::Value)>,
) -> Result<()> {
    let id = run.id();
    payload["action"] = json!(task_hold::HELD_ACTION);
    payload["kept_action"] = json!(action.as_str());
    payload["hold_id"] = json!(hold.hold);
    payload["status"] = json!(run.status().as_str());
    run_event(conn, id, EventKind::TriageFinished, payload)?;
    for (kind, mut payload) in also {
        if kind == EventKind::AutoRepaired {
            continue;
        }
        if kind == EventKind::RecoveryFinished {
            payload["applied"] = json!([]);
            payload["held"] = json!(true);
            payload["hold_id"] = json!(hold.hold);
        }
        run_event(conn, id, kind, payload)?;
    }
    let version = crate::infrastructure::task_holds::progress_version(conn, run.task_id(), id)?;
    run_event(
        conn,
        id,
        EventKind::HoldResultKept,
        json!({"source": KeptSource::RecoveryVerdict, "hold_id": hold.hold,
               "action": action.to_kept(), "version": version}),
    )
}

/// Apply the result `kept` (from `source`, with `payload`) to run `id` by
/// the path that would have applied it: a recovery verdict as the round's
/// action on a run nobody leases that is still its task's latest, a
/// person's answer as [`decide_triage_in`].
fn apply_kept(
    conn: &Connection,
    refusals: Refusals<'_>,
    id: &RunId,
    kept: EventId,
    source: KeptSource,
    payload: &serde_json::Value,
) -> Result<TaskRun> {
    match source {
        KeptSource::RecoveryVerdict => {
            let action = TriageAction::from_kept(&payload["action"])
                .context("the kept verdict names no action this binary applies")?;
            let run = unleased_latest_triageable(conn, refusals.generators.clock.now(), id)?;
            let mut finished = json!({"by": "runtime", "kept_id": kept,
                "reason": "the recovery job's verdict kept while the task was held"});
            apply_triage_action(conn, refusals, &run, &action, &mut finished)?;
            let result = conn.query_row(
                "SELECT * FROM task_runs WHERE id=?1",
                [id],
                run_row(refusals.runs_dir),
            )?;
            finished["action"] = json!(action.as_str());
            finished["status"] = json!(result.status().as_str());
            run_event(conn, id, EventKind::TriageFinished, finished)?;
            Ok(result)
        }
        KeptSource::TriageAnswer => {
            let ask_id: AskId = serde_json::from_value(payload["ask_id"].clone())?;
            decide_triage_in(
                conn,
                refusals,
                id,
                ask_id,
                payload["answer"].as_str().unwrap_or_default(),
                payload["reason"].as_str().unwrap_or_default(),
            )
        }
        KeptSource::LiveVerdict => bail!("a verdict on a live session is not applied later"),
    }
}

/// Refuse to bring run `id` back under its own ID once recovery may no
/// longer ([`crate::domain::telemetry::run_revivable`]: its task has a later
/// run or ended), read in the caller's transaction, where no person's
/// `ready`, cancel or claim can come in between: the supervisor's sweep
/// removes such a run's kept steps
/// ([`crate::domain::telemetry::run_ended`]), so the wrapper of its resume
/// would number its sessions from the first seq again.
fn ensure_revivable(conn: &Connection, id: &RunId) -> Result<()> {
    let (latest, task): (Option<RunId>, Option<String>) = conn.query_row(
        "SELECT (SELECT id FROM task_runs WHERE task_id=r.task_id ORDER BY rowid DESC LIMIT 1),
                (SELECT status FROM tasks WHERE id=r.task_id)
         FROM task_runs r WHERE r.id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let task = task
        .map(|status| status.parse::<crate::domain::TaskStatus>())
        .transpose()?;
    ensure!(
        crate::domain::telemetry::run_revivable(id, latest.as_ref(), task),
        "run {id} is no longer the latest run of a task that goes on"
    );
    Ok(())
}

/// Run `id`, `failed` or `interrupted`, its task's latest run, with no
/// lease but a stale one, which goes (so its holder cannot renew it and
/// write after this, ADR-0039 decision 7).
fn unleased_latest_triageable(conn: &Connection, now: i64, id: &RunId) -> Result<TaskRun> {
    let lease = conn
        .query_row(
            "SELECT run_id,token,pid,heartbeat_at FROM run_leases WHERE run_id=?1",
            [id],
            lease_row,
        )
        .optional()?;
    ensure!(
        lease.is_none_or(|lease| lease_is_stale(&lease, now)),
        "run {id} is leased"
    );
    conn.execute("DELETE FROM run_leases WHERE run_id=?1", [id])?;
    let run = stored_run(conn, id)?.with_context(|| format!("run {id} does not exist"))?;
    ensure!(
        run::check_triageable(&run).is_ok(),
        "run {id} is {}, not failed or interrupted",
        run.status().as_str()
    );
    let latest: RunId = conn.query_row(
        "SELECT id FROM task_runs WHERE task_id=?1 ORDER BY rowid DESC LIMIT 1",
        [run.task_id()],
        |r| r.get(0),
    )?;
    ensure!(
        latest == *id,
        "run {id} is no longer the latest run of task {}",
        run.task_id()
    );
    Ok(run)
}

/// Apply the answer of a triage's `decide` ask to its `failed` or
/// `interrupted` run that nobody leases and that is still its task's
/// latest run, and close the ask, inside the caller's transaction (an
/// ask closed meanwhile, by another supervisor or a person, is refused,
/// and so is a run whose task is held, ADR-t1879-1): `retry` makes the
/// task `ready`, `resume` the run `needs_session` with `reason` as
/// `last_error` (refused once its task ended, [`ensure_revivable`]),
/// `cancel` cancels the task. Recorded as `triage_decided`
/// (`ask_id`, `answer`, `reason`, `status`), and an answer a hold kept as
/// `hold_result_applied`.
fn decide_triage_in(
    conn: &Connection,
    refusals: Refusals<'_>,
    id: &RunId,
    ask_id: AskId,
    answer: &str,
    reason: &str,
) -> Result<TaskRun> {
    let now = refusals.generators.clock.now();
    let run = unleased_latest_triageable(conn, now, id)?;
    crate::infrastructure::task_holds::check_not_held(conn, run.task_id(), GuardedStep::Recover)?;
    let pending: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM asks WHERE id=?1 AND run_id=?2
         AND answered_at IS NOT NULL AND closed_at IS NULL)",
        params![ask_id, id],
        |r| r.get(0),
    )?;
    ensure!(
        pending,
        "ask {ask_id} is not an answered, unclosed ask of run {id}"
    );
    // The answer a hold kept, applied here after its release before a
    // pass settled it, is settled as applied: neither the task's end
    // below nor that pass discards it.
    if let Some(kept) = unsettled_kept_answer(conn, id, ask_id)? {
        run_event(
            conn,
            id,
            EventKind::HoldResultApplied,
            json!({"kept_id": kept, "source": KeptSource::TriageAnswer}),
        )?;
    }
    let timestamp = refusals.generators.clock.timestamp();
    match answer {
        "retry" => {
            crate::infrastructure::sqlite::transition_task(
                conn,
                run.task_id(),
                TaskAction::Ready,
                &timestamp,
            )?;
        }
        "resume" => {
            ensure_revivable(conn, id)?;
            apply(
                conn,
                refusals,
                id,
                None,
                || format!("run {id} changed"),
                |run| run::resume_after_triage(run, reason.to_owned()),
            )?;
        }
        "cancel" => {
            crate::infrastructure::sqlite::cancel_task(
                conn,
                run.task_id(),
                &NewCancel::answered(AskKind::Decide, ask_id),
                &timestamp,
            )?;
        }
        // Another option the ask offered is the recovery job's own: it
        // goes back to the job, whose next round reads the answer
        // (ADR-0047 decision 40). Nothing moves here.
        other => {
            let options: String =
                conn.query_row("SELECT options FROM asks WHERE id=?1", [ask_id], |r| {
                    r.get(0)
                })?;
            let options: Vec<String> = serde_json::from_str(&options)?;
            ensure!(
                options.iter().any(|option| option == other),
                "{other:?} is not an answer the recovery applies"
            );
        }
    }
    conn.execute(
        "UPDATE asks SET closed_at=?2 WHERE id=?1 AND closed_at IS NULL",
        params![ask_id, now],
    )?;
    let result = conn.query_row(
        "SELECT * FROM task_runs WHERE id=?1",
        [id],
        run_row(refusals.runs_dir),
    )?;
    let mut payload = json!({"ask_id": ask_id, "answer": answer, "reason": reason, "status": result.status().as_str()});
    if answer == "resume" {
        payload["code"] = json!(ReasonCode::TriageResume);
    }
    if !matches!(answer, "retry" | "resume" | "cancel") {
        payload["action"] = json!(crate::domain::RECOVER_AGAIN);
    }
    run_event(conn, id, EventKind::TriageDecided, payload)?;
    Ok(result)
}

/// The result a hold kept of the answer to ask `ask_id` of run `id` that
/// is neither applied nor discarded yet.
fn unsettled_kept_answer(conn: &Connection, id: &RunId, ask_id: AskId) -> Result<Option<EventId>> {
    Ok(conn
        .query_row(
            "SELECT e.id FROM run_events e WHERE e.run_id=?1 AND e.kind=?2
               AND json_extract(e.payload,'$.ask_id')=?3
               AND NOT EXISTS (SELECT 1 FROM run_events s WHERE s.run_id=e.run_id
                 AND s.kind IN (?4,?5) AND json_extract(s.payload,'$.kept_id')=e.id)
             ORDER BY e.id LIMIT 1",
            params![
                id,
                event_kind::HOLD_RESULT_KEPT,
                ask_id,
                event_kind::HOLD_RESULT_APPLIED,
                event_kind::HOLD_RESULT_DISCARDED
            ],
            |r| r.get(0),
        )
        .optional()?)
}

/// Lease run `id` to `token` inside the caller's transaction unless a lease
/// that is not stale holds it (a stale one is replaced): the lease row, the
/// run's supervisor and `lease_acquired` with `payload`, its pid and the
/// previous token. Whether it was leased.
fn lease_in(
    conn: &Connection,
    id: &RunId,
    token: &LeaseToken,
    now: i64,
    mut payload: serde_json::Value,
) -> Result<bool> {
    let lease = conn
        .query_row(
            "SELECT run_id,token,pid,heartbeat_at FROM run_leases WHERE run_id=?1",
            [id],
            lease_row,
        )
        .optional()?;
    if let Some(lease) = &lease {
        if !lease_is_stale(lease, now) {
            return Ok(false);
        }
        conn.execute("DELETE FROM run_leases WHERE run_id=?1", [id])?;
    }
    conn.execute(
        "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,?2,?3,?4)",
        params![id, token, holder_pid(conn, token)?, now],
    )?;
    conn.execute(
        "UPDATE task_runs SET supervisor_token=?2 WHERE id=?1",
        params![id, token],
    )?;
    payload["pid"] = json!(holder_pid(conn, token)?);
    payload["previous_token"] = json!(lease.map(|l| l.token));
    run_event(conn, id, EventKind::LeaseAcquired, payload)?;
    Ok(true)
}

/// Whether run `id` records the restore of operation `op`'s work.
fn restored_by(conn: &Connection, id: &RunId, op: crate::domain::ReplanId) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_events WHERE run_id=?1 AND kind=?2
         AND json_extract(payload,'$.operation_id')=?3)",
        params![id, EventKind::ReplanRestored.as_str(), op],
        |r| r.get(0),
    )?)
}

/// Whether a replanning operation or a hold guards the task of run `id`,
/// so that `step` waits for the operation's outcome (ADR-t1521-2,
/// decision 1) or the hold's release (ADR-t1879-1): no resume nor
/// recovery round of it starts.
fn replanning(conn: &Connection, id: &RunId, step: GuardedStep) -> Result<bool> {
    let task: Option<TaskId> = conn
        .query_row("SELECT task_id FROM task_runs WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(task.is_some_and(|task| {
        crate::infrastructure::replans::check_task_step(conn, task, step).is_err()
    }))
}
