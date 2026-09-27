//! Leases, heartbeats, supervisor registrations, run processes and the
//! repository binding ([`RunCoordination`]).

use super::*;

impl SqliteQueue {
    /// Refresh every lease this supervisor holds. Zero rows is not an error:
    /// an idle supervisor owns nothing.
    pub fn heartbeat_leases(&self, token: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE run_leases SET heartbeat_at=?2 WHERE token=?1",
            params![token, self.generators.clock.now()],
        )?)
    }

    /// One heartbeat of a process identified by `token`: its registration
    /// (if it is a resident supervisor) and every run lease it holds, in one
    /// transaction so `status` never sees them disagree. Returns the number
    /// of leases refreshed.
    pub fn heartbeat(&mut self, token: &str) -> Result<usize> {
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
        token: &str,
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
        token: &str,
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
    pub fn deregister_supervisor(&self, token: &str) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM supervisors WHERE token=?1", [token])?
            == 1)
    }

    /// Mark the registration of `token` as one that takes a handoff
    /// (ADR-0045 decision 10): its process execs another binary when asked.
    pub fn accept_handoff(&self, token: &str) -> Result<()> {
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
    pub fn request_handoff(&self, token: &str, binary: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE supervisors SET handoff_binary=?2, handoff_requested_at=?3
             WHERE token=?1 AND handoff_accepted=1",
            params![token, binary, self.generators.clock.now()],
        )? == 1)
    }

    /// Withdraw the request that the supervisor `token` exec `binary`, if
    /// it has not taken it yet; `false` when there was none.
    pub fn cancel_handoff(&self, token: &str, binary: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE supervisors SET handoff_binary=NULL, handoff_requested_at=NULL
             WHERE token=?1 AND handoff_binary=?2",
            params![token, binary],
        )? == 1)
    }

    /// The binary the supervisor `token` was asked to exec, if any.
    pub fn handoff_request(&self, token: &str) -> Result<Option<String>> {
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
        token: &str,
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
    pub fn set_auto_update(&self, token: &str, enabled: bool) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE supervisors SET auto_update=?2 WHERE token=?1",
                params![token, i64::from(enabled)],
            )? == 1,
            "supervisor {token} is no longer registered"
        );
        Ok(())
    }

    /// Record the supervisor `token`'s `--max-waiting` (ADR-0062 decision 7).
    pub fn set_max_waiting(&self, token: &str, max_waiting: u32) -> Result<()> {
        ensure!(
            self.conn.execute(
                "UPDATE supervisors SET max_waiting=?2 WHERE token=?1",
                params![token, max_waiting],
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

    /// Give up ownership of a run that came to rest (`awaiting_integration`
    /// or `failed`). The run's `supervisor_token` stays as a record.
    pub fn release_lease(&mut self, id: &RunId, token: &str) -> Result<()> {
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
    pub fn holds_lease(&self, id: &RunId, token: &str) -> Result<bool> {
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
    pub fn backend_slots(&self, token: Option<&str>) -> Result<(i64, Option<i64>)> {
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
    pub fn register_resume_wrapper(&mut self, id: &RunId, token: &str, pid: u32) -> Result<()> {
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

    pub fn register_wrapper(&mut self, id: &RunId, token: &str, pid: u32) -> Result<()> {
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
    fn heartbeat_leases(&self, token: &str) -> Result<usize> {
        SqliteQueue::heartbeat_leases(self, token)
    }
    fn register_supervisor(
        &mut self,
        token: &str,
        pid: u32,
        parallel: u32,
        binary_version: &str,
    ) -> Result<SupervisorRegistration> {
        SqliteQueue::register_supervisor(self, token, pid, parallel, binary_version)
    }
    fn deregister_supervisor(&self, token: &str) -> Result<bool> {
        SqliteQueue::deregister_supervisor(self, token)
    }
    fn supervisors(&self) -> Result<Vec<SupervisorRegistration>> {
        SqliteQueue::supervisors(self)
    }
    fn release_lease(&mut self, id: &RunId, token: &str) -> Result<()> {
        SqliteQueue::release_lease(self, id, token)
    }
    fn accept_handoff(&self, token: &str) -> Result<()> {
        SqliteQueue::accept_handoff(self, token)
    }
    fn request_handoff(&self, token: &str, binary: &str) -> Result<bool> {
        SqliteQueue::request_handoff(self, token, binary)
    }
    fn handoff_request(&self, token: &str) -> Result<Option<String>> {
        SqliteQueue::handoff_request(self, token)
    }
    fn cancel_handoff(&self, token: &str, binary: &str) -> Result<bool> {
        SqliteQueue::cancel_handoff(self, token, binary)
    }
    fn resume_registration(
        &mut self,
        token: &str,
        pid: u32,
        binary_version: &str,
    ) -> Result<SupervisorRegistration> {
        SqliteQueue::resume_registration(self, token, pid, binary_version)
    }
    fn set_auto_update(&self, token: &str, enabled: bool) -> Result<()> {
        SqliteQueue::set_auto_update(self, token, enabled)
    }
    fn set_max_waiting(&self, token: &str, max_waiting: u32) -> Result<()> {
        SqliteQueue::set_max_waiting(self, token, max_waiting)
    }
    fn holds_lease(&self, id: &RunId, token: &str) -> Result<bool> {
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
    fn heartbeat(&mut self, token: &str) -> Result<usize> {
        SqliteQueue::heartbeat(self, token)
    }
    fn processes(&self, id: &RunId) -> Result<Vec<RunProcess>> {
        SqliteQueue::processes(self, id)
    }
    fn set_supervisor_mode(
        &self,
        token: &str,
        mode: SupervisorMode,
        workspace_id: Option<&str>,
    ) -> Result<()> {
        SqliteQueue::set_supervisor_mode(self, token, mode, workspace_id)
    }
    fn register_wrapper(&mut self, id: &RunId, token: &str, pid: u32) -> Result<()> {
        SqliteQueue::register_wrapper(self, id, token, pid)
    }
    fn register_resume_wrapper(&mut self, id: &RunId, token: &str, pid: u32) -> Result<()> {
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
    fn backend_slots(&self, token: Option<&str>) -> Result<(i64, Option<i64>)> {
        SqliteQueue::backend_slots(self, token)
    }
}
