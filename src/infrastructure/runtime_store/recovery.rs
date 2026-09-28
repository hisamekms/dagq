//! The run aggregate moved by recovery: triage, resumes and adoption
//! ([`RunRecovery`]).

use super::*;
use crate::domain::EventKind;

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
        let _read = read_before(&self.conn, Closing::Run(id, &[event_kind::RUN_RECOVERED]))?;
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
    /// this one, `counted`, `reason`, `main`). `Ok(None)` means another
    /// process took it or it changed meanwhile.
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
        let events = run_events_of(&tx, id)?;
        let resumes = ResumeCount::of(&events);
        if resumes.exhausted(config) {
            return Ok(None);
        }
        // A resume of a run parked only by a conflict after its review
        // passed is not one of the counted attempts.
        let basis = resume::conflict_only_basis(&events);
        let counted = basis.is_none();
        let Some(previous) = lease_parked_run(&tx, id, token, now, true)? else {
            return Ok(None);
        };
        let attempt = resumes.total() + 1;
        run_event(
            &tx,
            id,
            EventKind::LeaseAcquired,
            json!({"pid": std::process::id(), "reason": "resume", "previous_token": previous}),
        )?;
        // The resumed session keeps the model and effort of the claim
        // (ADR-0079 decision 3).
        let mut started = json!({"attempt": attempt, "counted": counted, "reason": reason.or(run.last_error()), "main": main});
        if let Some(started) = started.as_object_mut() {
            started.extend(WorkerSession::of_run(&events).fields());
        }
        run_event(&tx, id, EventKind::ResumeStarted, started)?;
        if let Some(basis) = basis {
            run_event(
                &tx,
                id,
                EventKind::AutoRepaired,
                json!({
                    "layer": "runtime",
                    "repair": "conflict_resume_uncounted",
                    "conditions": {
                        "review_passed": basis.passed,
                        "landing_approved": basis.approved,
                        "rechecked": basis.rechecked,
                        "parked": ReasonCode::RebaseConflict,
                        "counted_resumes": resumes.counted,
                        "conflict_only_resumes": resumes.conflict_only + 1,
                    },
                    "detail": {"attempt": attempt, "main": main},
                }),
            )?;
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
    /// `approved`, `status`). An approved run stays `needs_session` for the
    /// caller to land under the lease; an unapproved one becomes
    /// `validating`. No `resume_started` is recorded, so no attempt is used.
    /// `Ok(None)` means another process took it or it changed meanwhile.
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
        let Some(previous) = lease_parked_run(&tx, id, token, now, false)? else {
            return Ok(None);
        };
        let run = apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || format!("run {id} is not needs_session"),
            |run| run::skip_resume(run, approved),
        )?;
        let status = run.status();
        run_event(
            &tx,
            id,
            EventKind::LeaseAcquired,
            json!({"pid": std::process::id(), "reason": "resume_skipped", "previous_token": previous}),
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
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
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
        let resumed: i64 = tx.query_row(
            &format!(
                "SELECT coalesce(max(id),0) FROM run_events WHERE run_id=?1 AND kind='{}'",
                event_kind::RESUME_STARTED
            ),
            [id],
            |r| r.get(0),
        )?;
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
    /// (`attempt`, `status`). `Ok(None)` means another process took it or
    /// it changed.
    pub fn begin_triage(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        request: Option<serde_json::Value>,
        launch: &crate::domain::actor_model::ActorLaunch,
    ) -> Result<Option<(TaskRun, usize)>> {
        // The spans it closes read their transcripts first (task 543).
        let _read = read_before(&self.conn, Closing::Run(id, &[event_kind::TRIAGE_STARTED]))?;
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
        let events: Vec<RunEvent> = tx
            .prepare("SELECT * FROM run_events WHERE run_id=?1 ORDER BY id")?
            .query_map([id], event_row)?
            .collect::<rusqlite::Result<_>>()?;
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
            params![id, token, std::process::id(), now],
        )?;
        let attempt = RunHistory::from_events(&events).triage_attempts() + 1;
        run_event(
            &tx,
            id,
            EventKind::LeaseAcquired,
            json!({"pid": std::process::id(), "reason": "triage", "previous_token": lease.map(|l| l.token)}),
        )?;
        if let Some(request) = request {
            run_event(&tx, id, EventKind::RecoveryRequested, request)?;
        }
        run_event(
            &tx,
            id,
            EventKind::TriageStarted,
            // The job's Claude session id (ADR-0048 decision 4).
            json!({"attempt": attempt, "status": run.status().as_str(), "session_id": self.generators.ids.uuid(), "launch": launch.to_value()}),
        )?;
        tx.commit()?;
        Ok(Some((run, attempt)))
    }

    /// Act on a recovery round under its lease and record
    /// `triage_finished` with `payload`, the `action` and the run's status
    /// after it, then each of `also`, in one transaction: `Retry` makes the
    /// task `ready`, `RetryInherit` too with the branch to carry over,
    /// `Resume` the run `needs_session`, `Wait` and `Ask` change nothing.
    /// The lease stays for the workspace's close.
    pub fn finish_triage(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        action: &TriageAction,
        mut payload: serde_json::Value,
        also: Vec<(EventKind, serde_json::Value)>,
    ) -> Result<TaskRun> {
        // The spans it closes read their transcripts first (task 543).
        let kinds: Vec<&str> = std::iter::once(event_kind::TRIAGE_FINISHED)
            .chain(also.iter().map(|(kind, _)| kind.as_str()))
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
        match action {
            TriageAction::Retry => {
                crate::infrastructure::sqlite::transition_task(
                    &tx,
                    run.task_id(),
                    TaskAction::Ready,
                    &self.generators.clock.timestamp(),
                )?;
            }
            TriageAction::RetryInherit { branch, head } => {
                // Once per task, checked again where no other supervisor
                // can retry it.
                let task_events: Vec<RunEvent> = tx
                    .prepare("SELECT * FROM run_events WHERE task_id=?1 ORDER BY id")?
                    .query_map([run.task_id()], event_row)?
                    .collect::<rusqlite::Result<_>>()?;
                ensure!(
                    !task_events.iter().any(resume::is_inherit_retry),
                    "task {} was retried with a branch carried over already",
                    run.task_id()
                );
                crate::infrastructure::sqlite::transition_task(
                    &tx,
                    run.task_id(),
                    TaskAction::Ready,
                    &self.generators.clock.timestamp(),
                )?;
                payload["inherit"] = json!({"branch": branch, "head": head});
            }
            TriageAction::Wait { recheck_at } => payload["recheck_at"] = json!(recheck_at),
            TriageAction::Resume { instruction } => {
                apply(
                    &tx,
                    refusals(&self.runs_dir, &self.generators),
                    id,
                    None,
                    || format!("run {id} changed"),
                    |run| run::resume_after_triage(run, instruction.clone()),
                )?;
                payload["instruction"] = json!(instruction);
                payload["code"] = json!(ReasonCode::TriageResume);
            }
            TriageAction::Ask { ask_id } => payload["ask_id"] = json!(ask_id),
        }
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
    /// `Ok(None)` means it changed.
    pub fn exhaust_resumes(
        &mut self,
        id: &RunId,
        exhaustion: &Exhaustion,
        reason: &str,
        config: resume::ResumeConfig,
    ) -> Result<Option<TaskRun>> {
        // The spans it closes read their transcripts first (task 543).
        let _read = read_before(&self.conn, Closing::Run(id, &[event_kind::TRIAGE_FINISHED]))?;
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
    /// latest run, and close the ask, in one transaction (an ask closed
    /// meanwhile, by another supervisor or a person, is refused): `retry` makes the task `ready`, `resume` the run
    /// `needs_session` with `reason` as `last_error`, `cancel` cancels the
    /// task. Recorded as `triage_decided` (`ask_id`, `answer`, `reason`,
    /// `status`).
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
        let now = self.generators.clock.now();
        let lease = tx
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
        // The stale lease goes, so its holder cannot renew it and write
        // after this decision (ADR-0039 decision 7).
        tx.execute("DELETE FROM run_leases WHERE run_id=?1", [id])?;
        let run = tx.query_row(
            "SELECT * FROM task_runs WHERE id=?1",
            [id],
            run_row(&self.runs_dir),
        )?;
        ensure!(
            run::check_triageable(&run).is_ok(),
            "run {id} is {}, not failed or interrupted",
            run.status().as_str()
        );
        let latest: RunId = tx.query_row(
            "SELECT id FROM task_runs WHERE task_id=?1 ORDER BY rowid DESC LIMIT 1",
            [run.task_id()],
            |r| r.get(0),
        )?;
        ensure!(
            latest == *id,
            "run {id} is no longer the latest run of task {}",
            run.task_id()
        );
        let pending: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM asks WHERE id=?1 AND run_id=?2
             AND answered_at IS NOT NULL AND closed_at IS NULL)",
            params![ask_id, id],
            |r| r.get(0),
        )?;
        ensure!(
            pending,
            "ask {ask_id} is not an answered, unclosed ask of run {id}"
        );
        match answer {
            "retry" => {
                crate::infrastructure::sqlite::transition_task(
                    &tx,
                    run.task_id(),
                    TaskAction::Ready,
                    &self.generators.clock.timestamp(),
                )?;
            }
            "resume" => {
                apply(
                    &tx,
                    refusals(&self.runs_dir, &self.generators),
                    id,
                    None,
                    || format!("run {id} changed"),
                    |run| run::resume_after_triage(run, reason.to_owned()),
                )?;
            }
            "cancel" => {
                crate::infrastructure::sqlite::transition_task(
                    &tx,
                    run.task_id(),
                    TaskAction::Cancel,
                    &self.generators.clock.timestamp(),
                )?;
            }
            // Another option the ask offered is the recovery job's own: it
            // goes back to the job, whose next round reads the answer
            // (ADR-0047 decision 40). Nothing moves here.
            other => {
                let options: String =
                    tx.query_row("SELECT options FROM asks WHERE id=?1", [ask_id], |r| {
                        r.get(0)
                    })?;
                let options: Vec<String> = serde_json::from_str(&options)?;
                ensure!(
                    options.iter().any(|option| option == other),
                    "{other:?} is not an answer the recovery applies"
                );
            }
        }
        tx.execute(
            "UPDATE asks SET closed_at=?2 WHERE id=?1 AND closed_at IS NULL",
            params![ask_id, now],
        )?;
        let result = tx.query_row(
            "SELECT * FROM task_runs WHERE id=?1",
            [id],
            run_row(&self.runs_dir),
        )?;
        let mut payload = json!({"ask_id": ask_id, "answer": answer, "reason": reason, "status": result.status().as_str()});
        if answer == "resume" {
            payload["code"] = json!(ReasonCode::TriageResume);
        }
        if !matches!(answer, "retry" | "resume" | "cancel") {
            payload["action"] = json!(crate::domain::RECOVER_AGAIN);
        }
        run_event(&tx, id, EventKind::TriageDecided, payload)?;
        tx.commit()?;
        Ok(result)
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
}
