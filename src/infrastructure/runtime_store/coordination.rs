//! Leases, heartbeats, supervisor registrations, run processes and the
//! repository binding ([`RunCoordination`]).

use super::*;

impl SqliteQueue {
    /// Refresh every lease this supervisor holds. Zero rows is not an error:
    /// an idle supervisor owns nothing.
    pub fn heartbeat_leases(&self, token: &LeaseToken) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE run_leases SET heartbeat_at=?2 WHERE token=?1",
            params![token, self.generators.clock.now()],
        )?)
    }

    /// One heartbeat of a process identified by `token`: its registration
    /// (if it is a resident supervisor) and every run lease it holds, in one
    /// transaction so `status` never sees them disagree. Returns the number
    /// of leases refreshed.
    pub fn heartbeat(&mut self, token: &LeaseToken) -> Result<usize> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        tx.execute(
            "UPDATE supervisors SET heartbeat_at=?2 WHERE token=?1",
            params![token, now],
        )?;
        let leases = tx.execute(
            "UPDATE run_leases SET heartbeat_at=?2 WHERE token=?1",
            params![token, now],
        )?;
        tx.commit()?;
        Ok(leases)
    }

    /// Register a resident `supervise` process before it claims anything,
    /// so `status` and `doctor` can list it while it holds no run.
    /// `binary_version` is the running binary's own build identifier
    /// (`crate::VERSION`), which only this process knows; `up` reads it back
    /// to decide whether a live supervisor is of its own build (ADR-0045).
    pub fn register_supervisor(
        &mut self,
        token: &LeaseToken,
        pid: u32,
        parallel: u32,
        binary_version: &str,
    ) -> Result<SupervisorRegistration> {
        ensure!(parallel >= 1, "parallel must be at least 1");
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.generators.clock.now();
        tx.execute(
            "INSERT INTO supervisors(token,pid,parallel,binary_version,started_at,heartbeat_at)
             VALUES (?1,?2,?3,?4,?5,?5)",
            params![token, pid, parallel, binary_version, now],
        )
        .context("supervisor token is already registered")?;
        let result = tx.query_row(
            "SELECT * FROM supervisors WHERE token=?1",
            [token],
            supervisor_row,
        )?;
        tx.commit()?;
        Ok(result)
    }

    /// Record how `up` started this supervisor, once its process has
    /// registered itself. Only `up` writes it, and only for a supervisor it
    /// started; the row's own process never does, so a supervisor started
    /// by hand keeps `mode` unset. The workspace belongs to `in_cmux` mode.
    pub fn set_supervisor_mode(
        &self,
        token: &LeaseToken,
        mode: SupervisorMode,
        workspace_id: Option<&str>,
    ) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE supervisors SET mode=?2, workspace_id=?3 WHERE token=?1",
                params![token, mode.as_str(), workspace_id],
            )? == 1,
            "supervisor {token} is no longer registered"
        );
        Ok(())
    }

    /// Remove the registration on a graceful exit. Leases are untouched; a
    /// missing row (already removed, or never written) is not an error, so a
    /// crashed supervisor's row is only ever removed by `up`, `down --force`
    /// or a person.
    pub fn deregister_supervisor(&self, token: &LeaseToken) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM supervisors WHERE token=?1", [token])?
            == 1)
    }

    /// Remove the registration of `token` and record the queue event of
    /// `kind` whose payload `stopped` builds from the row as it was, in one
    /// transaction: the stop of a supervisor that did not remove its own is
    /// recorded exactly when its row goes, and a failed record keeps the
    /// row for the next prune. `false`, recording nothing, when the row is
    /// already gone.
    pub fn prune_supervisor(
        &self,
        token: &LeaseToken,
        kind: &str,
        stopped: &dyn Fn(&SupervisorRegistration) -> Value,
    ) -> Result<bool> {
        crate::domain::check_event_target(kind, None, None)?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(registration) = tx
            .query_row(
                "SELECT * FROM supervisors WHERE token=?1",
                [token],
                supervisor_row,
            )
            .optional()?
        else {
            return Ok(false);
        };
        tx.execute("DELETE FROM supervisors WHERE token=?1", [token])?;
        // No session span is closed by a supervisor's stop, so there are no
        // transcripts to read before the write, as `record_queue_event` does.
        super::run_log::queue_event(&tx, kind, &stopped(&registration))?;
        tx.commit()?;
        Ok(true)
    }

    /// Mark the registration of `token` as one that takes a handoff
    /// (ADR-0045 decision 10): its process execs another binary when asked.
    pub fn accept_handoff(&self, token: &LeaseToken) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE supervisors SET handoff_accepted=1 WHERE token=?1",
                [token],
            )? == 1,
            "supervisor {token} is no longer registered"
        );
        Ok(())
    }

    /// Ask the supervisor `token` to exec `binary` at its next pause between
    /// short steps. `false` when it is not registered or does not take a
    /// handoff.
    pub fn request_handoff(&self, token: &LeaseToken, binary: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE supervisors SET handoff_binary=?2, handoff_requested_at=?3
             WHERE token=?1 AND handoff_accepted=1",
            params![token, binary, self.generators.clock.now()],
        )? == 1)
    }

    /// Withdraw the request that the supervisor `token` exec `binary`, if
    /// it has not taken it yet; `false` when there was none.
    pub fn cancel_handoff(&self, token: &LeaseToken, binary: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE supervisors SET handoff_binary=NULL, handoff_requested_at=NULL
             WHERE token=?1 AND handoff_binary=?2",
            params![token, binary],
        )? == 1)
    }

    /// The binary the supervisor `token` was asked to exec, if any.
    pub fn handoff_request(&self, token: &LeaseToken) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT handoff_binary FROM supervisors WHERE token=?1",
                [token],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Take the registration of `token` back after exec'ing another binary
    /// (or after an exec that failed): the same process (`pid`) now runs
    /// `binary_version`. Clears the request and refreshes the heartbeat, and
    /// leaves `mode`, `workspace_id`, `started_at` and every lease alone.
    pub fn resume_registration(
        &mut self,
        token: &LeaseToken,
        pid: u32,
        binary_version: &str,
    ) -> Result<SupervisorRegistration> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            tx.execute(
                "UPDATE supervisors SET binary_version=?3, heartbeat_at=?4, handoff_accepted=1,
                        handoff_binary=NULL, handoff_requested_at=NULL
                 WHERE token=?1 AND pid=?2",
                params![token, pid, binary_version, self.generators.clock.now()],
            )? == 1,
            "supervisor {token} (pid {pid}) is no longer registered; nothing to hand off to"
        );
        let result = tx.query_row(
            "SELECT * FROM supervisors WHERE token=?1",
            [token],
            supervisor_row,
        )?;
        tx.commit()?;
        Ok(result)
    }

    /// Turn the automatic update of the supervisor `token` on or off
    /// (ADR-0045 decision 17).
    pub fn set_auto_update(&self, token: &LeaseToken, enabled: bool) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE supervisors SET auto_update=?2 WHERE token=?1",
                params![token, i64::from(enabled)],
            )? == 1,
            "supervisor {token} is no longer registered"
        );
        Ok(())
    }

    /// Record the executables of the supervisor `token`'s providers as it
    /// resolved them at its start (ADR-t813-2).
    pub fn set_supervisor_providers(
        &self,
        token: &LeaseToken,
        providers: &[crate::domain::worker::ProviderCheck],
    ) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE supervisors SET providers=?2 WHERE token=?1",
                params![token, serde_json::to_string(providers)?],
            )? == 1,
            "supervisor {token} is no longer registered"
        );
        Ok(())
    }

    /// Record the supervisor `token`'s `parallel` and `max_waiting` in use
    /// (ADR-0062 decision 7) and where each comes from (task 698).
    pub fn set_slot_limits(&self, token: &LeaseToken, limits: SlotLimits) -> Result<()> {
        let parallel = u32::try_from(limits.parallel.value)?;
        ensure!(parallel >= 1, "parallel must be at least 1");
        let max_waiting = u32::try_from(limits.max_waiting.value)?;
        ensure!(
            self.conn.execute(
                "UPDATE supervisors SET parallel=?2, parallel_source=?3, max_waiting=?4, max_waiting_source=?5 WHERE token=?1",
                params![
                    token,
                    parallel,
                    limits.parallel.source.as_str(),
                    max_waiting,
                    limits.max_waiting.source.as_str()
                ],
            )? == 1,
            "supervisor {token} is no longer registered"
        );
        Ok(())
    }

    /// Every registered supervisor, oldest registration first, whether its
    /// process is alive or not.
    pub fn supervisors(&self) -> Result<Vec<SupervisorRegistration>> {
        supervisors_of(&self.conn)
    }

    /// Give up ownership of a run the supervisor stops driving. The lease is
    /// not released when a run comes to rest `awaiting_integration`: it is
    /// kept through the review and the landing (ADR-0054 decision 6), and a
    /// landed run loses it with `finish_integration`. The supervisor calls
    /// this when a run ends short of the landing (`needs_session` or
    /// `failed`), when a recovery round of a finished run ends, when it opens
    /// the run's `approve_landing` ask (a `concern` verdict or a failed
    /// review) or leaves the run waiting for its answer or for a person, when a resumed run's landing cannot start, and when it
    /// drains or hands off a run it cannot keep. The run's `supervisor_token`
    /// stays as a record.
    pub fn release_lease(&mut self, id: &RunId, token: &LeaseToken) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            tx.execute(
                "DELETE FROM run_leases WHERE run_id=?1 AND token=?2",
                params![id, token]
            )? == 1,
            "run lease was lost"
        );
        run_event(
            &tx,
            id,
            event_kind::LEASE_RELEASED,
            json!({"reason": "finished"}),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Whether this process still holds the run's lease. An adopted-away or
    /// recovered run answers `false`, and its former owner must not touch it.
    pub fn holds_lease(&self, id: &RunId, token: &LeaseToken) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_leases WHERE run_id=?1 AND token=?2)",
            params![id, token],
            |r| r.get(0),
        )?)
    }

    /// Bind the queue to a repository before any run exists, as `init` does for a
    /// queue resolved from the working directory. A queue already bound to
    /// another repository is refused; rebinding is never implicit.
    pub fn bind_repository(&mut self, common_dir: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR IGNORE INTO queue_repository(singleton, git_common_dir) VALUES (1,?1)",
            [common_dir],
        )?;
        let bound: String = tx.query_row(
            "SELECT git_common_dir FROM queue_repository WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        ensure!(
            bound == common_dir,
            "queue is bound to another Git repository: {bound}"
        );
        tx.commit()?;
        Ok(())
    }

    /// Point the queue at `common_dir` whatever it was bound to, and return
    /// the previous binding. Only the explicit `rebind` command calls this
    /// (ADR-0020); `init` and `supervise` go through `bind_repository`.
    pub fn rebind_repository(&mut self, common_dir: &str) -> Result<Option<String>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<String> = tx
            .query_row(
                "SELECT git_common_dir FROM queue_repository WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        tx.execute(
            "INSERT INTO queue_repository(singleton, git_common_dir) VALUES (1,?1)
             ON CONFLICT(singleton) DO UPDATE SET git_common_dir=excluded.git_common_dir",
            [common_dir],
        )?;
        tx.commit()?;
        Ok(previous)
    }

    /// Refuse a queue that belongs to another repository. An unbound queue
    /// (created with `--db` and never supervised) passes.
    pub fn assert_repository(&self, common_dir: &str) -> Result<()> {
        if let Some(bound) = self.repository_binding()? {
            ensure!(
                bound == common_dir,
                "queue is bound to another Git repository: {bound} (this repository is {common_dir})"
            );
        }
        Ok(())
    }

    /// Git common directory the queue is bound to, recorded by `init` for a
    /// repository queue or by the first `supervise` otherwise.
    pub fn repository_binding(&self) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT git_common_dir FROM queue_repository WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Every lease in the queue, oldest run first.
    pub fn run_leases(&self) -> Result<Vec<RunLease>> {
        Ok(self
            .conn
            .prepare("SELECT l.run_id,l.token,l.pid,l.heartbeat_at FROM run_leases l JOIN task_runs r ON r.id=l.run_id ORDER BY r.rowid")?
            .query_map([], lease_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn run_lease(&self, id: &RunId) -> Result<Option<RunLease>> {
        Ok(self
            .conn
            .query_row(
                "SELECT run_id,token,pid,heartbeat_at FROM run_leases WHERE run_id=?1",
                [id],
                lease_row,
            )
            .optional()?)
    }

    /// The slots held and the slots offered when a backend call failed:
    /// for the supervisor `token`, its leased runs and its `--parallel`;
    /// without one (`up`, `down`), every lease and the sum over every
    /// registered supervisor. `parallel` is `None` when no supervisor is
    /// registered (under `token`).
    pub fn backend_slots(&self, token: Option<&LeaseToken>) -> Result<(i64, Option<i64>)> {
        let slots = self.conn.query_row(
            "SELECT count(*) FROM run_leases WHERE ?1 IS NULL OR token=?1",
            [token],
            |r| r.get(0),
        )?;
        let parallel = self.conn.query_row(
            "SELECT SUM(parallel) FROM supervisors WHERE ?1 IS NULL OR token=?1",
            [token],
            |r| r.get(0),
        )?;
        Ok((slots, parallel))
    }

    /// Register the wrapper of a resumed session (ADR-0019): the run is
    /// `needs_session` and leased to `token`, and `begin_resume` cleared the
    /// previous session's process rows.
    pub fn register_resume_wrapper(
        &mut self,
        id: &RunId,
        token: &LeaseToken,
        pid: u32,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        let allowed =
            supervised_run(&tx, id, token)?.is_some_and(|run| run::check_resumable(&run).is_ok());
        ensure!(allowed, "run is not being resumed by this supervisor");
        tx.execute(
            "INSERT INTO run_processes(run_id,role,pid) VALUES (?1,'wrapper',?2)",
            params![id, pid],
        )
        .context("wrapper is already registered; a resume may only launch once")?;
        run_event(&tx, id, event_kind::WRAPPER_STARTED, json!({"pid": pid}))?;
        tx.commit()?;
        Ok(())
    }

    /// Register the agent of a resumed session; the run stays `needs_session`.
    pub fn register_resume_agent(
        &mut self,
        id: &RunId,
        wrapper_pid: u32,
        agent_pid: u32,
    ) -> Result<()> {
        // The spans it closes read their transcripts first (task 543).
        let _read = read_before(&self.conn, Closing::Run(id, &[event_kind::AGENT_STARTED]))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_wrapper(&tx, id, wrapper_pid)?;
        tx.execute(
            "INSERT INTO run_processes(run_id,role,pid) VALUES (?1,'agent',?2)",
            params![id, agent_pid],
        )?;
        run_event(
            &tx,
            id,
            event_kind::AGENT_STARTED,
            json!({"pid": agent_pid, "session_id": id}),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn register_wrapper(&mut self, id: &RunId, token: &LeaseToken, pid: u32) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        renew_lease(&tx, id, token, self.generators.clock.now())?;
        let allowed = supervised_run(&tx, id, token)?
            .is_some_and(|run| run::check_ready_for_wrapper(&run).is_ok());
        ensure!(allowed, "run is not ready for its wrapper");
        tx.execute(
            "INSERT INTO run_processes(run_id,role,pid) VALUES (?1,'wrapper',?2)",
            params![id, pid],
        )
        .context("wrapper is already registered; a run may only launch once")?;
        run_event(&tx, id, event_kind::WRAPPER_STARTED, json!({"pid": pid}))?;
        tx.commit()?;
        Ok(())
    }

    pub fn register_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32) -> Result<()> {
        // The spans it closes read their transcripts first (task 543).
        let _read = read_before(&self.conn, Closing::Run(id, &[event_kind::AGENT_STARTED]))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_wrapper(&tx, id, wrapper_pid)?;
        tx.execute(
            "INSERT INTO run_processes(run_id,role,pid) VALUES (?1,'agent',?2)",
            params![id, agent_pid],
        )?;
        apply(
            &tx,
            refusals(&self.runs_dir, &self.generators),
            id,
            None,
            || "run is not starting".to_owned(),
            run::mark_running,
        )?;
        run_event(
            &tx,
            id,
            event_kind::AGENT_STARTED,
            json!({"pid": agent_pid, "session_id": id}),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn heartbeat_wrapper(&self, id: &RunId, pid: u32) -> Result<()> {
        // Registration is immutable and a run ID is never reused.
        assert_wrapper(&self.conn, id, pid)?;
        self.conn.execute(
            "UPDATE run_processes SET heartbeat_at=?2 WHERE run_id=?1 AND exited_at IS NULL",
            params![id, self.generators.clock.now()],
        )?;
        Ok(())
    }

    pub fn wrapper_exited(&mut self, id: &RunId, pid: u32, exit_code: i32) -> Result<()> {
        // The spans it closes read their transcripts first (task 543).
        let _read = read_before(&self.conn, Closing::Run(id, &[event_kind::SESSION_EXITED]))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_wrapper(&tx, id, pid)?;
        tx.execute(
            "UPDATE run_processes SET exited_at=?3,exit_code=?2,heartbeat_at=?3
             WHERE run_id=?1 AND exited_at IS NULL",
            params![id, exit_code, self.generators.clock.now()],
        )?;
        run_event(
            &tx,
            id,
            event_kind::SESSION_EXITED,
            json!({"exit_code": exit_code}),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn processes(&self, id: &RunId) -> Result<Vec<RunProcess>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM run_processes WHERE run_id=?1 ORDER BY role")?
            .query_map([id], process_row)?
            .collect::<rusqlite::Result<_>>()?)
    }
}

/// The [`RunCoordination`] port over the inherent methods above, which callers
/// that hold a `SqliteQueue` keep using directly.
impl RunCoordination for SqliteQueue {
    fn heartbeat_leases(&self, token: &LeaseToken) -> Result<usize> {
        SqliteQueue::heartbeat_leases(self, token)
    }
    fn register_supervisor(
        &mut self,
        token: &LeaseToken,
        pid: u32,
        parallel: u32,
        binary_version: &str,
    ) -> Result<SupervisorRegistration> {
        SqliteQueue::register_supervisor(self, token, pid, parallel, binary_version)
    }
    fn deregister_supervisor(&self, token: &LeaseToken) -> Result<bool> {
        SqliteQueue::deregister_supervisor(self, token)
    }
    fn prune_supervisor(
        &self,
        token: &LeaseToken,
        kind: &str,
        stopped: &dyn Fn(&SupervisorRegistration) -> Value,
    ) -> Result<bool> {
        SqliteQueue::prune_supervisor(self, token, kind, stopped)
    }
    fn supervisors(&self) -> Result<Vec<SupervisorRegistration>> {
        SqliteQueue::supervisors(self)
    }
    fn release_lease(&mut self, id: &RunId, token: &LeaseToken) -> Result<()> {
        SqliteQueue::release_lease(self, id, token)
    }
    fn accept_handoff(&self, token: &LeaseToken) -> Result<()> {
        SqliteQueue::accept_handoff(self, token)
    }
    fn request_handoff(&self, token: &LeaseToken, binary: &str) -> Result<bool> {
        SqliteQueue::request_handoff(self, token, binary)
    }
    fn handoff_request(&self, token: &LeaseToken) -> Result<Option<String>> {
        SqliteQueue::handoff_request(self, token)
    }
    fn cancel_handoff(&self, token: &LeaseToken, binary: &str) -> Result<bool> {
        SqliteQueue::cancel_handoff(self, token, binary)
    }
    fn resume_registration(
        &mut self,
        token: &LeaseToken,
        pid: u32,
        binary_version: &str,
    ) -> Result<SupervisorRegistration> {
        SqliteQueue::resume_registration(self, token, pid, binary_version)
    }
    fn set_auto_update(&self, token: &LeaseToken, enabled: bool) -> Result<()> {
        SqliteQueue::set_auto_update(self, token, enabled)
    }
    fn set_slot_limits(&self, token: &LeaseToken, limits: SlotLimits) -> Result<()> {
        SqliteQueue::set_slot_limits(self, token, limits)
    }
    fn set_supervisor_providers(
        &self,
        token: &LeaseToken,
        providers: &[crate::domain::worker::ProviderCheck],
    ) -> Result<()> {
        SqliteQueue::set_supervisor_providers(self, token, providers)
    }
    fn holds_lease(&self, id: &RunId, token: &LeaseToken) -> Result<bool> {
        SqliteQueue::holds_lease(self, id, token)
    }
    fn run_leases(&self) -> Result<Vec<RunLease>> {
        SqliteQueue::run_leases(self)
    }
    fn run_lease(&self, id: &RunId) -> Result<Option<RunLease>> {
        SqliteQueue::run_lease(self, id)
    }
    fn rebind_repository(&mut self, common_dir: &str) -> Result<Option<String>> {
        SqliteQueue::rebind_repository(self, common_dir)
    }
    fn repository_binding(&self) -> Result<Option<String>> {
        SqliteQueue::repository_binding(self)
    }
    fn bind_repository(&mut self, common_dir: &str) -> Result<()> {
        SqliteQueue::bind_repository(self, common_dir)
    }
    fn assert_repository(&self, common_dir: &str) -> Result<()> {
        SqliteQueue::assert_repository(self, common_dir)
    }
    fn heartbeat(&mut self, token: &LeaseToken) -> Result<usize> {
        SqliteQueue::heartbeat(self, token)
    }
    fn processes(&self, id: &RunId) -> Result<Vec<RunProcess>> {
        SqliteQueue::processes(self, id)
    }
    fn set_supervisor_mode(
        &self,
        token: &LeaseToken,
        mode: SupervisorMode,
        workspace_id: Option<&str>,
    ) -> Result<()> {
        SqliteQueue::set_supervisor_mode(self, token, mode, workspace_id)
    }
    fn register_wrapper(&mut self, id: &RunId, token: &LeaseToken, pid: u32) -> Result<()> {
        SqliteQueue::register_wrapper(self, id, token, pid)
    }
    fn register_resume_wrapper(&mut self, id: &RunId, token: &LeaseToken, pid: u32) -> Result<()> {
        SqliteQueue::register_resume_wrapper(self, id, token, pid)
    }
    fn register_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32) -> Result<()> {
        SqliteQueue::register_agent(self, id, wrapper_pid, agent_pid)
    }
    fn register_resume_agent(
        &mut self,
        id: &RunId,
        wrapper_pid: u32,
        agent_pid: u32,
    ) -> Result<()> {
        SqliteQueue::register_resume_agent(self, id, wrapper_pid, agent_pid)
    }
    fn heartbeat_wrapper(&self, id: &RunId, pid: u32) -> Result<()> {
        SqliteQueue::heartbeat_wrapper(self, id, pid)
    }
    fn wrapper_exited(&mut self, id: &RunId, pid: u32, exit_code: i32) -> Result<()> {
        SqliteQueue::wrapper_exited(self, id, pid, exit_code)
    }
    fn backend_slots(&self, token: Option<&LeaseToken>) -> Result<(i64, Option<i64>)> {
        SqliteQueue::backend_slots(self, token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STOPPED: &str = crate::domain::marks::SUPERVISOR_STOPPED;

    fn stopped(removed: &SupervisorRegistration) -> Value {
        json!({"supervisor": removed.token, "last_heartbeat_at": removed.heartbeat_at})
    }

    fn stops(queue: &SqliteQueue) -> Vec<Value> {
        queue
            .conn
            .prepare("SELECT payload FROM run_events WHERE kind=?1 ORDER BY id")
            .unwrap()
            .query_map([STOPPED], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
            .collect()
    }

    fn registered(queue: &SqliteQueue) -> Vec<String> {
        queue
            .supervisors()
            .unwrap()
            .into_iter()
            .map(|registration| registration.token.into_string())
            .collect()
    }

    /// A parallel below 1 is refused before the write (ADR-t876-1: the
    /// rule the `supervisors.parallel` CHECK held), and the row keeps its
    /// value.
    #[test]
    fn a_parallel_below_1_is_not_written() {
        use crate::domain::slot_limits::{Setting, SettingSource, SlotLimits};
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let token = LeaseToken::new("me");
        assert!(queue.register_supervisor(&token, 1, 0, "0.0.1").is_err());
        assert!(registered(&queue).is_empty());
        queue.register_supervisor(&token, 1, 3, "0.0.1").unwrap();
        let setting = |value| Setting {
            value,
            source: SettingSource::Flag,
        };
        let error = queue
            .set_slot_limits(
                &token,
                SlotLimits {
                    parallel: setting(0),
                    max_waiting: setting(2),
                },
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "parallel must be at least 1");
        let parallel: i64 = queue
            .conn
            .query_row("SELECT parallel FROM supervisors", [], |r| r.get(0))
            .unwrap();
        assert_eq!(parallel, 3);
    }

    #[test]
    fn pruning_removes_the_row_and_records_its_stop_from_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let registration = queue
            .register_supervisor(&LeaseToken::new("dead"), 1, 1, "0.0.1")
            .unwrap();
        queue
            .register_supervisor(&LeaseToken::new("live"), std::process::id(), 1, "0.0.1")
            .unwrap();

        assert!(
            queue
                .prune_supervisor(&LeaseToken::new("dead"), STOPPED, &stopped)
                .unwrap()
        );

        assert_eq!(registered(&queue), ["live"]);
        assert_eq!(
            stops(&queue),
            [json!({"supervisor": "dead", "last_heartbeat_at": registration.heartbeat_at})]
        );
    }

    #[test]
    fn pruning_a_row_already_gone_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();

        assert!(
            !queue
                .prune_supervisor(&LeaseToken::new("gone"), STOPPED, &stopped)
                .unwrap()
        );

        assert!(stops(&queue).is_empty());
    }

    #[test]
    fn a_failed_record_keeps_the_row_for_the_next_prune() {
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        queue
            .register_supervisor(&LeaseToken::new("dead"), 1, 1, "0.0.1")
            .unwrap();
        // Fail every insert of an event, after the row's delete in the
        // same transaction.
        queue
            .conn
            .execute_batch(
                "CREATE TEMP TRIGGER fail_events BEFORE INSERT ON run_events
                 BEGIN SELECT RAISE(ABORT, 'record failed'); END",
            )
            .unwrap();

        let error = queue
            .prune_supervisor(&LeaseToken::new("dead"), STOPPED, &stopped)
            .unwrap_err();

        assert!(format!("{error:#}").contains("record failed"), "{error:#}");
        assert!(queue.conn.is_autocommit());
        assert_eq!(registered(&queue), ["dead"]);
        queue
            .conn
            .execute_batch("DROP TRIGGER fail_events")
            .unwrap();
        assert!(
            queue
                .prune_supervisor(&LeaseToken::new("dead"), STOPPED, &stopped)
                .unwrap()
        );
        assert!(registered(&queue).is_empty());
        assert_eq!(stops(&queue).len(), 1);
    }
}
