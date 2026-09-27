//! The run aggregate saved as it moves: claim, provisioning, supervision,
//! validation, landing decisions, integration and clean-up ([`RunTransitions`]).

use super::*;

impl SqliteQueue {
    /// Reserve the next dependency-ready task of the interactive Claude
    /// worker for this supervisor: the run,
    /// its `supervisor_token` and its lease row are created in one transaction,
    /// so a claimed run never exists without an owner. Concurrent supervisors
    /// on the same queue take different tasks.
    pub fn claim_for_supervisor(
        &mut self,
        base_commit: &CommitSha,
        token: &LeaseToken,
    ) -> Result<ClaimOutcome> {
        self.claim_for_supervisor_in_order(
            base_commit,
            token,
            &[],
            None,
            &WorkerTrial::default(),
            &[Worker::DEFAULT],
        )
    }

    /// [`Self::claim_for_supervisor`], taking the first task of `order` that
    /// is still claimable (the lowest-ID candidate when none is), with
    /// `attributes` in its `run_claimed` and the worker session `trial`
    /// chooses for the task; only a task whose worker is one of `workers`.
    pub fn claim_for_supervisor_in_order(
        &mut self,
        base_commit: &CommitSha,
        token: &LeaseToken,
        order: &[TaskId],
        attributes: Option<&Value>,
        trial: &WorkerTrial,
        workers: &[Worker],
    ) -> Result<ClaimOutcome> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // The claim, its run and its first heartbeat share one time.
        let at = self.generators.clock.system_time();
        let outcome = claim_task(
            &tx,
            &self.runs_dir,
            self.generators.ids.as_ref(),
            at,
            base_commit,
            order,
            attributes,
            trial,
            workers,
        )?;
        if let ClaimOutcome::Claimed { run } = &outcome {
            tx.execute(
                "UPDATE task_runs SET supervisor_token=?2 WHERE id=?1",
                params![run.id(), token],
            )?;
            tx.execute(
                "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,?2,?3,?4)",
                params![run.id(), token, std::process::id(), unix_seconds(at)],
            )?;
            run_event(
                &tx,
                run.id(),
                event_kind::LEASE_ACQUIRED,
                json!({"pid": std::process::id()}),
            )?;
        }
        tx.commit()?;
        Ok(outcome)
    }

    /// Record a runtime error and disown the run without changing its status
    /// or touching its processes and resources. Dropping the lease lets
    /// `recover` judge the run by its registered processes alone while this
    /// supervisor keeps serving other runs; a wrapper that has not registered
    /// yet can no longer do so. `session` says what became of the run's
    /// live session (the `/exit` the supervisor sent it, or why none got
    /// there), recorded as the event's `session`.
    pub fn abandon_run(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        message: &str,
        reason: &Reason,
        session: Option<&Value>,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || "run does not exist".to_owned(),
            |run| run::abandon(run, message.to_owned()),
        )?;
        let released = tx.execute(
            "DELETE FROM run_leases WHERE run_id=?1 AND token=?2",
            params![id, token],
        )?;
        run_event(
            &tx,
            id,
            event_kind::RUNTIME_ERROR,
            reason.on(abandon_payload(message, released == 1, session)),
        )?;
        // Nobody watches its session any more (ADR-0047 decision 30).
        end_stalled_detections(&tx, id, STALL_ABANDONED_CLOSED, self.generators.clock.now())?;
        tx.commit()?;
        Ok(run.relocated(&self.runs_dir))
    }

    pub fn plan_run(&mut self, id: &RunId, token: &LeaseToken, plan: &RunPlan) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            Some(token),
            || "run cannot be provisioned twice".to_owned(),
            |run| run::start_provisioning(run, plan),
        )?;
        run_event(
            &tx,
            id,
            event_kind::RUN_PLANNED,
            serde_json::to_value(plan)?,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn workspace_created(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        workspace: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            Some(token),
            || "workspace cannot be attached to this run".to_owned(),
            |run| run::attach_workspace(run, workspace.to_owned()),
        )?;
        run_event(
            &tx,
            id,
            event_kind::WORKSPACE_CREATED,
            json!({"workspace_id": workspace}),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn record_runtime_error(
        &mut self,
        id: &RunId,
        message: &str,
        reason: &Reason,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || "run does not exist".to_owned(),
            |run| run::abandon(run, message.to_owned()),
        )?;
        run_event(
            &tx,
            id,
            event_kind::RUNTIME_ERROR,
            reason.on(json!({"message": message})),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn finish_supervision(&mut self, id: &RunId, token: &LeaseToken) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        let code: i32 = tx.query_row(
            "SELECT exit_code FROM run_processes WHERE run_id=?1 AND role='wrapper' AND exited_at IS NOT NULL",
            [id], |r| r.get(0)
        ).context("wrapper has not reported session exit")?;
        let run = apply_recorded(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            Some(token),
            || "run is not owned by this supervisor".to_owned(),
            |run| run::end_session(run, Some(code)),
        )?;
        // Completion and dependency release belong to the next validation stage.
        tx.commit()?;
        Ok(run.relocated(&self.runs_dir))
    }

    /// Hand a run whose session went idle after its receipt to validation
    /// with the session still alive (ADR-0027 decision 1): `running` becomes
    /// `validating` under the same lease, and `supervision_finished` records
    /// `session_live: true` with no exit code.
    pub fn finish_supervision_live(&mut self, id: &RunId, token: &LeaseToken) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        let run = apply_recorded(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            Some(token),
            || "run is not owned by this supervisor".to_owned(),
            |run| run::end_session(run, None),
        )?;
        tx.commit()?;
        Ok(run.relocated(&self.runs_dir))
    }

    /// Validate an `awaiting_integration` run again under the lease that
    /// reviews it, after its live session rewrote the receipt for a
    /// `revise` verdict (ADR-0027 decision 2); `revise_finished` is the
    /// record of why.
    pub fn restart_validation(&mut self, id: &RunId, token: &LeaseToken) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        let run = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            Some(token),
            || "run is not awaiting integration under this supervisor".to_owned(),
            run::restart_validation,
        )?;
        tx.commit()?;
        Ok(run.relocated(&self.runs_dir))
    }

    /// Apply the `send_back` or `cancel` answer of an `approve_landing` ask
    /// to a run awaiting integration that nobody leases: it becomes
    /// `status` (`needs_session` or `failed`) with `reason` as `last_error`,
    /// recorded as `landing_decided` with `payload`.
    pub fn decide_landing(
        &mut self,
        id: &RunId,
        status: crate::domain::RunStatus,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let leased: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_leases WHERE run_id=?1)",
            [id],
            |r| r.get(0),
        )?;
        ensure!(!leased, "run {id} is leased");
        let run = apply_recorded(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || format!("run {id} is not awaiting integration"),
            |run| run::record_landing_decision(run, status, reason, payload),
        )?;
        tx.commit()?;
        Ok(run.relocated(&self.runs_dir))
    }

    /// Park a running run leased to `token` whose live session a recovery
    /// job's `resume` sends back to a session of its own (task 442): it
    /// becomes `needs_session` with `reason` as `last_error`, recorded as
    /// `recovery_parked` with `payload`, the status and the reason. The
    /// lease stays: the supervisor asks the session to exit first.
    pub fn park_live(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        let run = apply_recorded(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            Some(token),
            || "run is not running under this supervisor".to_owned(),
            |run| run::record_live_park(run, reason, payload),
        )?;
        tx.commit()?;
        Ok(run.relocated(&self.runs_dir))
    }

    /// Park a run awaiting integration that the landing recheck found no
    /// longer landing on main (ADR-0068 decision 3): it becomes
    /// `needs_session` with `reason` as `last_error`, recorded as
    /// `landing_recheck_failed` with `payload`, `action: resumed`, the
    /// status and the reason. With `token` the run must be leased to it,
    /// and the lease goes; without one it must be leased to nobody.
    /// `None`, and nothing written, when the run is not so (it moved on, or
    /// someone took it meanwhile).
    pub fn park_rechecked(
        &mut self,
        id: &RunId,
        token: Option<&LeaseToken>,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<Option<TaskRun>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let lease: Option<LeaseToken> = tx
            .query_row("SELECT token FROM run_leases WHERE run_id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        let waiting = stored_run(&tx, id)?
            .is_some_and(|run| run.status() == crate::domain::RunStatus::AwaitingIntegration);
        if lease.as_ref() != token || !waiting {
            return Ok(None);
        }
        let mut events = Vec::new();
        let run = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || format!("run {id} is not awaiting integration"),
            |run| {
                let (run, recorded) = run::record_recheck_park(run, reason, payload)?;
                events = recorded;
                Ok(run)
            },
        )?;
        // The lease goes first, as it always has in the event log.
        if let Some(token) = token {
            tx.execute(
                "DELETE FROM run_leases WHERE run_id=?1 AND token=?2",
                params![id, token],
            )?;
            run_event(
                &tx,
                id,
                event_kind::LEASE_RELEASED,
                json!({"reason": crate::domain::recheck::LANDING_RECHECK_FAILED}),
            )?;
        }
        record_events(&tx, id, events)?;
        tx.commit()?;
        Ok(Some(run.relocated(&self.runs_dir)))
    }

    pub fn finish_validation(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        validation: &Validation,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        let refusal = || "run is not validating under this supervisor".to_owned();
        let run = apply_recorded(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            Some(token),
            refusal,
            |run| run::finish_validation(run, validation),
        )?;
        // Task completion still waits for integration into main.
        tx.commit()?;
        Ok(run.relocated(&self.runs_dir))
    }

    /// Take the single integration slot for an awaiting run or one coming
    /// back from a session: the run becomes `integrating` and this process
    /// owns it through a lease row for the duration, so `doctor` can see who
    /// is landing what. A lease this `token` already holds (a supervisor
    /// landing the run it resumed) is kept; one under another token refuses. `one_integrating_run_per_queue` backs the explicit check.
    pub fn begin_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        main: &CommitSha,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let busy: Option<RunId> = tx
            .query_row(
                "SELECT id FROM task_runs WHERE status='integrating'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(other) = busy {
            ensure!(
                other == *id,
                "run {other} is integrating; one run lands at a time (see doctor if it is stuck)"
            );
            bail!("run {id} is already integrating (see doctor if it is stuck)");
        }
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
                    "run {id} is {}; only a run awaiting integration or a session can be integrated",
                    previous.as_str()
                )
            },
            run::begin_integration,
        )?;
        // A supervisor landing a run it resumed or reviewed already holds
        // its lease under the same token; any other lease means someone
        // owns the run.
        ensure!(
            tx.execute(
                "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,?2,?3,?4)
                 ON CONFLICT(run_id) DO UPDATE SET pid=excluded.pid,heartbeat_at=excluded.heartbeat_at
                 WHERE run_leases.token=excluded.token",
                params![id, token, std::process::id(), self.generators.clock.now()],
            )? == 1,
            "run is still leased"
        );
        run_event(
            &tx,
            id,
            event_kind::INTEGRATION_STARTED,
            json!({"main": main, "previous_status": previous, "pid": std::process::id()}),
        )?;
        tx.commit()?;
        Ok(run.relocated(&self.runs_dir))
    }

    /// Park an integrating run for a session: `needs_session` with the reason
    /// in `last_error`; the slot and lease are released. The worktree is left
    /// as the landing attempt left it.
    pub fn defer_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        detail: serde_json::Value,
    ) -> Result<TaskRun> {
        self.leave_integration(
            id,
            token,
            |run| run::defer_integration(run, reason.to_owned()),
            reason,
            event_kind::INTEGRATION_DEFERRED,
            detail,
        )
    }

    /// Hold an integrating run whose verification command failed on the
    /// host again after its retry (task 639): `awaiting_integration` with
    /// the reason in `last_error` and `integration_held`; the slot and
    /// lease are released, and no resume is used.
    pub fn hold_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        detail: serde_json::Value,
    ) -> Result<TaskRun> {
        self.leave_integration(
            id,
            token,
            |run| run::hold_integration(run, reason.to_owned()),
            reason,
            event_kind::INTEGRATION_HELD,
            detail,
        )
    }

    /// End an integrating run whose rewritten receipt reports `failed`. The
    /// receipt's JSON goes into the `integration_failed` event, since the DB
    /// otherwise holds only the receipt seen at validation time.
    pub fn fail_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        receipt: serde_json::Value,
    ) -> Result<TaskRun> {
        self.leave_integration(
            id,
            token,
            |run| run::fail_integration(run, reason.to_owned()),
            reason,
            event_kind::INTEGRATION_FAILED,
            json!({"code": ReasonCode::WorkerFailed, "receipt": receipt}),
        )
    }

    /// Give the slot back after an error before `main` moved: the run returns
    /// to the status it had when the landing started.
    pub fn abort_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        revert_to: &str,
        message: &str,
        reason: &Reason,
    ) -> Result<TaskRun> {
        let revert_to: RunStatus = revert_to.parse()?;
        self.leave_integration(
            id,
            token,
            |run| run::abort_integration(run, revert_to, message.to_owned()),
            message,
            event_kind::INTEGRATION_ERROR,
            reason.on(json!({})),
        )
    }

    fn leave_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        command: impl FnOnce(TaskRun) -> Result<TaskRun, DomainError>,
        reason: &str,
        kind: &str,
        mut detail: serde_json::Value,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        let run = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || format!("run {id} is not integrating"),
            command,
        )?;
        tx.execute(
            "DELETE FROM run_leases WHERE run_id=?1 AND token=?2",
            params![id, token],
        )?;
        detail["status"] = json!(run.status());
        detail["reason"] = json!(reason);
        run_event(&tx, id, kind, detail)?;
        run_event(&tx, id, event_kind::LEASE_RELEASED, json!({"reason": kind}))?;
        tx.commit()?;
        Ok(run.relocated(&self.runs_dir))
    }

    /// Complete the task once its run landed on `main`: the run becomes
    /// `integrated` with the landed commit as `result_commit`, the task
    /// `completed`, and the slot and lease are released. The status
    /// predicates make a repeated or concurrent completion fail without effect.
    pub fn finish_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        landing: &Landing,
        common_dir: &str,
    ) -> Result<(Task, TaskRun)> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let at = self.generators.clock.system_time();
        renew_lease(&tx, id, token, unix_seconds(at))?;
        let run = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || format!("run {id} is not integrating"),
            |run| run::finish_integration(run, landing.commit.clone()),
        )?
        .relocated(&self.runs_dir);
        tx.execute(
            "DELETE FROM run_leases WHERE run_id=?1 AND token=?2",
            params![id, token],
        )?;
        ensure!(
            tx.execute(
                "UPDATE tasks SET status='completed', updated_at=?2
                 WHERE id=?1 AND status='in_progress'",
                params![run.task_id(), timestamp(at)]
            )? == 1,
            "task {} is not in progress",
            run.task_id()
        );
        let mut payload = serde_json::to_value(landing)?;
        payload["result_commit"] = json!(landing.commit);
        payload["git_common_dir"] = json!(common_dir);
        run_event(&tx, id, event_kind::RUN_INTEGRATED, payload)?;
        run_event(
            &tx,
            id,
            event_kind::LEASE_RELEASED,
            json!({"reason": "integrated"}),
        )?;
        event(
            &tx,
            run.task_id(),
            Some(id),
            event_kind::TASK_STATUS_CHANGED,
            json!({"from": "in_progress", "to": "completed"}),
        )?;
        let task = read_task(&tx, run.task_id())?;
        tx.commit()?;
        Ok((task, run))
    }

    /// Note a post-landing cleanup failure (worktree or branch removal) on a
    /// run whose status no longer changes.
    pub fn record_cleanup_failure(
        &mut self,
        id: &RunId,
        message: &str,
        reason: &Reason,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || "run does not exist".to_owned(),
            |run| run::record_cleanup_failure(run, message.to_owned()),
        )?;
        run_event(
            &tx,
            id,
            event_kind::CLEANUP_FAILED,
            reason.on(json!({"message": message})),
        )?;
        tx.commit()?;
        Ok(())
    }
    /// Record a confirmed cmux close. Only an accepted run whose workspace is
    /// still recorded as open qualifies; the worktree and branch stay for integration.
    pub fn workspace_closed(&mut self, id: &RunId, token: &LeaseToken) -> Result<TaskRun> {
        // The spans it closes read their transcripts first (task 543).
        let _read = read_before(
            &self.conn,
            Closing::Run(id, &[event_kind::WORKSPACE_CLOSED]),
        )?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        renew_lease(&tx, id, token, now)?;
        let result = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            Some(token),
            not_at_rest,
            |run| run::workspace_closed(run, now),
        )?
        .relocated(&self.runs_dir);
        run_event(
            &tx,
            id,
            event_kind::WORKSPACE_CLOSED,
            json!({"workspace_id": result.workspace_id(), "closed_at": result.workspace_closed_at()}),
        )?;
        tx.commit()?;
        Ok(result)
    }

    /// A failed close leaves `workspace_closed_at` null so the workspace is never
    /// treated as cleaned; the run status does not change.
    pub fn cleanup_failed(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        message: &str,
        reason: &Reason,
    ) -> Result<TaskRun> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        let result = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            Some(token),
            not_at_rest,
            |run| run::record_close_failure(run, message.to_owned()),
        )?
        .relocated(&self.runs_dir);
        run_event(
            &tx,
            id,
            event_kind::CLEANUP_FAILED,
            reason.on(json!({"workspace_id": result.workspace_id(), "message": message})),
        )?;
        tx.commit()?;
        Ok(result)
    }

    /// Record that the triage or the supervisor's sweep closed
    /// `workspace_id` of the run as `workspace_closed` (`payload` with the
    /// `workspace_id`): the worker's own (`workspace_closed_at` is set) or a
    /// resume's.
    pub fn record_workspace_closed(
        &mut self,
        id: &RunId,
        workspace_id: &str,
        mut payload: serde_json::Value,
    ) -> Result<()> {
        // The spans it closes read their transcripts first (task 543).
        let _read = read_before(
            &self.conn,
            Closing::Run(id, &[event_kind::WORKSPACE_CLOSED]),
        )?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        if let Some(run) = stored_run(&tx, id)? {
            let from = run.status();
            let run = run::record_closed_workspace(run, workspace_id, now)?;
            save_run(&tx, &run, from, None)?;
        }
        payload["workspace_id"] = json!(workspace_id);
        run_event(&tx, id, event_kind::WORKSPACE_CLOSED, payload)?;
        tx.commit()?;
        Ok(())
    }
}

/// The [`RunTransitions`] port over the inherent methods above, which callers
/// that hold a `SqliteQueue` keep using directly.
impl RunTransitions for SqliteQueue {
    fn claim_for_supervisor(
        &mut self,
        base_commit: &CommitSha,
        token: &LeaseToken,
    ) -> Result<ClaimOutcome> {
        SqliteQueue::claim_for_supervisor(self, base_commit, token)
    }
    fn abandon_run(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        message: &str,
        reason: &Reason,
        session: Option<&Value>,
    ) -> Result<TaskRun> {
        SqliteQueue::abandon_run(self, id, token, message, reason, session)
    }
    fn begin_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        main: &CommitSha,
    ) -> Result<TaskRun> {
        SqliteQueue::begin_integration(self, id, token, main)
    }
    fn defer_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        detail: serde_json::Value,
    ) -> Result<TaskRun> {
        SqliteQueue::defer_integration(self, id, token, reason, detail)
    }
    fn hold_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        detail: serde_json::Value,
    ) -> Result<TaskRun> {
        SqliteQueue::hold_integration(self, id, token, reason, detail)
    }
    fn fail_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        receipt: serde_json::Value,
    ) -> Result<TaskRun> {
        SqliteQueue::fail_integration(self, id, token, reason, receipt)
    }
    fn abort_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        revert_to: &str,
        message: &str,
        reason: &Reason,
    ) -> Result<TaskRun> {
        SqliteQueue::abort_integration(self, id, token, revert_to, message, reason)
    }
    fn finish_integration(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        landing: &Landing,
        common_dir: &str,
    ) -> Result<(Task, TaskRun)> {
        SqliteQueue::finish_integration(self, id, token, landing, common_dir)
    }
    fn record_cleanup_failure(&mut self, id: &RunId, message: &str, reason: &Reason) -> Result<()> {
        SqliteQueue::record_cleanup_failure(self, id, message, reason)
    }
    fn workspace_closed(&mut self, id: &RunId, token: &LeaseToken) -> Result<TaskRun> {
        SqliteQueue::workspace_closed(self, id, token)
    }
    fn cleanup_failed(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        message: &str,
        reason: &Reason,
    ) -> Result<TaskRun> {
        SqliteQueue::cleanup_failed(self, id, token, message, reason)
    }
    fn claim_for_supervisor_in_order(
        &mut self,
        base_commit: &CommitSha,
        token: &LeaseToken,
        order: &[TaskId],
        attributes: Option<&Value>,
        trial: &WorkerTrial,
        workers: &[Worker],
    ) -> Result<ClaimOutcome> {
        SqliteQueue::claim_for_supervisor_in_order(
            self,
            base_commit,
            token,
            order,
            attributes,
            trial,
            workers,
        )
    }
    fn record_runtime_error(&mut self, id: &RunId, message: &str, reason: &Reason) -> Result<()> {
        SqliteQueue::record_runtime_error(self, id, message, reason)
    }
    fn plan_run(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        plan: &crate::domain::RunPlan,
    ) -> Result<()> {
        SqliteQueue::plan_run(self, id, token, plan)
    }
    fn workspace_created(&mut self, id: &RunId, token: &LeaseToken, workspace: &str) -> Result<()> {
        SqliteQueue::workspace_created(self, id, token, workspace)
    }
    fn finish_supervision(&mut self, id: &RunId, token: &LeaseToken) -> Result<TaskRun> {
        SqliteQueue::finish_supervision(self, id, token)
    }
    fn finish_supervision_live(&mut self, id: &RunId, token: &LeaseToken) -> Result<TaskRun> {
        SqliteQueue::finish_supervision_live(self, id, token)
    }
    fn finish_validation(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        validation: &Validation,
    ) -> Result<TaskRun> {
        SqliteQueue::finish_validation(self, id, token, validation)
    }
    fn restart_validation(&mut self, id: &RunId, token: &LeaseToken) -> Result<TaskRun> {
        SqliteQueue::restart_validation(self, id, token)
    }
    fn decide_landing(
        &mut self,
        id: &RunId,
        status: RunStatus,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<TaskRun> {
        SqliteQueue::decide_landing(self, id, status, reason, payload)
    }
    fn park_live(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<TaskRun> {
        SqliteQueue::park_live(self, id, token, reason, payload)
    }
    fn park_rechecked(
        &mut self,
        id: &RunId,
        token: Option<&LeaseToken>,
        reason: &str,
        payload: serde_json::Value,
    ) -> Result<Option<TaskRun>> {
        SqliteQueue::park_rechecked(self, id, token, reason, payload)
    }
    fn record_workspace_closed(
        &mut self,
        id: &RunId,
        workspace_id: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        SqliteQueue::record_workspace_closed(self, id, workspace_id, payload)
    }
}

/// The payload of an abandon's `runtime_error`: the message, whether the
/// lease went, and what became of the run's live session, when it had one.
fn abandon_payload(message: &str, lease_released: bool, session: Option<&Value>) -> Value {
    let mut payload = json!({"message": message, "lease_released": lease_released});
    if let Some(session) = session {
        payload["session"] = session.clone();
    }
    payload
}
