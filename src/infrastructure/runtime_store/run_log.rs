//! Runs and events as read, and events recorded outside a transition
//! ([`RunLog`]).

use super::*;

impl SqliteQueue {
    /// The latest `limit` steps of the automatic update, newest first: the
    /// queue's `update_*` events (ADR-0073 decision 17).
    pub fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        Ok(self
            .conn
            .prepare(
                "SELECT * FROM run_events
                 WHERE kind IN (SELECT value FROM json_each(?1))
                 AND run_id IS NULL AND task_id IS NULL AND goal_id IS NULL
                 ORDER BY id DESC LIMIT ?2",
            )?
            .query_map(
                params![
                    serde_json::to_string(crate::domain::UPDATE_EVENT_KINDS)?,
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                event_row,
            )?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Whether the run has recorded at least one event of `kind`; an
    /// adopter rebuilds what the previous supervisor already did from these.
    pub fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_events WHERE run_id=?1 AND kind=?2)",
            params![id, kind],
            |r| r.get(0),
        )?)
    }

    /// Every run of the queue, oldest first.
    pub fn all_runs(&self) -> Result<Vec<TaskRun>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM task_runs ORDER BY rowid")?
            .query_map([], run_row(&self.runs_dir))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn run(&self, id: &RunId) -> Result<TaskRun> {
        self.conn
            .query_row(
                "SELECT * FROM task_runs WHERE id=?1",
                [id],
                run_row(&self.runs_dir),
            )
            .optional()?
            .with_context(|| format!("run {id} does not exist"))
    }

    pub fn record_runtime_event(
        &self,
        id: &RunId,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        // The event and the session spans it opens or closes (ADR-0048),
        // in one write transaction taken up front so it waits for other
        // writers rather than failing to upgrade a read. The transcripts of
        // the spans it closes are read before (task 543).
        let _read = read_before(&self.conn, Closing::Run(id, &[kind]))?;
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match run_event(&self.conn, id, kind, payload) {
            Ok(()) => Ok(self.conn.execute_batch("COMMIT")?),
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    /// Record `backend_call_failed`: on `run` (its id) when the call was for
    /// one, otherwise with neither a task nor a run.
    pub fn record_backend_failure(
        &self,
        run: Option<&RunId>,
        payload: serde_json::Value,
    ) -> Result<()> {
        match run {
            Some(id) => run_event(&self.conn, id, event_kind::BACKEND_CALL_FAILED, payload),
            None => self
                .record_queue_event(event_kind::BACKEND_CALL_FAILED, payload)
                .map(drop),
        }
    }

    /// Record an event of the queue itself, on no task, goal or run (the
    /// observer's `observe_started` / `observe_finished`); returns its id.
    pub fn record_queue_event(&self, kind: &str, payload: serde_json::Value) -> Result<EventId> {
        crate::domain::check_event_target(kind, None, None)?;
        // Immediate like every other write: a deferred one that read first
        // (the schema, to prepare the insert) got SQLITE_BUSY at once, past
        // the busy timeout, when another supervisor wrote at the same time.
        let _read = read_before(&self.conn, Closing::Queue(kind, &payload))?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let id = queue_event(&tx, kind, &payload)?;
        tx.commit()?;
        Ok(id)
    }

    /// The events of one of `kinds` with `after < id <= upto`, oldest
    /// first, at most `limit`.
    pub fn events_of_between(
        &self,
        kinds: &[&str],
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        let filter = EventFilter {
            kinds: Some(kinds.iter().map(|&kind| kind.to_owned()).collect()),
            ..EventFilter::default()
        };
        self.events_between(after, upto, &filter, limit)
    }

    /// The newest event of `kind`, on whatever task, goal or run.
    pub fn latest_event_of(&self, kind: &str) -> Result<Option<RunEvent>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM run_events WHERE kind=?1 ORDER BY id DESC LIMIT 1",
                [kind],
                event_row,
            )
            .optional()?)
    }

    /// The newest `limit` events of `kind`, on whatever task, goal or run,
    /// newest first.
    pub fn latest_events_of(&self, kind: &str, limit: usize) -> Result<Vec<RunEvent>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM run_events WHERE kind=?1 ORDER BY id DESC LIMIT ?2")?
            .query_map(
                params![kind, i64::try_from(limit).unwrap_or(i64::MAX)],
                event_row,
            )?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The newest event of the queue itself (on no task, goal or run) of one
    /// of `kinds`.
    /// One lookup per kind, so each walks `events_by_kind` from its newest
    /// row: the supervisor reads these on every pass, and a kind list made
    /// SQLite walk every goal-less event instead.
    pub fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>> {
        let mut latest: Option<RunEvent> = None;
        for kind in kinds {
            let event = self
                .conn
                .query_row(
                    "SELECT * FROM run_events
                     WHERE kind=?1 AND run_id IS NULL AND task_id IS NULL AND goal_id IS NULL
                     ORDER BY id DESC LIMIT 1",
                    [kind],
                    event_row,
                )
                .optional()?;
            if let Some(event) = event
                && latest.as_ref().is_none_or(|latest| latest.id < event.id)
            {
                latest = Some(event);
            }
        }
        Ok(latest)
    }

    /// Per task, its newest event of one of `kinds`, by task.
    pub fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; kinds.len()].join(",");
        Ok(self
            .conn
            .prepare(&format!(
                "SELECT * FROM run_events WHERE id IN (
                     SELECT MAX(id) FROM run_events
                     WHERE task_id IS NOT NULL AND kind IN ({marks})
                     GROUP BY task_id)
                 ORDER BY task_id"
            ))?
            .query_map(rusqlite::params_from_iter(kinds), event_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The id of the last event recorded before `unix` (seconds), 0 when
    /// there is none: a cursor that reads everything from that time on.
    pub fn event_id_before(&self, unix: i64) -> Result<EventId> {
        Ok(self.conn.query_row(
            "SELECT ifnull(max(id),0) FROM run_events
             WHERE CAST(strftime('%s',created_at) AS INTEGER) < ?1",
            [unix],
            |r| r.get(0),
        )?)
    }

    /// When the observer of `mode` last started or finished (unix seconds).
    pub fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
        Ok(self.conn.query_row(
            "SELECT max(CAST(strftime('%s',created_at) AS INTEGER)) FROM run_events
             WHERE kind IN (?2,?3)
               AND json_extract(payload,'$.mode')=?1",
            params![
                mode,
                event_kind::OBSERVE_STARTED,
                event_kind::OBSERVE_FINISHED
            ],
            |r| r.get(0),
        )?)
    }

    /// The newest ask's ID: the mark [`Self::written_by`] counts past.
    pub fn ask_high_water(&self) -> Result<AskId> {
        Ok(self
            .conn
            .query_row("SELECT ifnull(max(id),0) FROM asks", [], |r| r.get(0))?)
    }

    /// What `role` wrote after the marks: the ids of the findings it
    /// recorded, updated and closed (`finding_recorded`, `finding_updated`,
    /// and `finding_status_changed` to `resolved` or `dismissed`, after
    /// `event_id`, each id once, oldest first) and of its asks after
    /// `ask_id`. A reopening by a record (`to: open`) is not a close.
    pub fn written_by(&self, role: &str, event_id: EventId, ask_id: AskId) -> Result<WrittenBy> {
        let findings = |kind: &str, to: &[&str]| -> Result<Vec<i64>> {
            Ok(self
                .conn
                .prepare(
                    "SELECT json_extract(payload,'$.finding_id') AS finding FROM run_events
                     WHERE id>?2 AND kind=?3 AND json_extract(payload,'$.by')=?1
                       AND (?4 IS NULL OR json_extract(payload,'$.to')
                            IN (SELECT value FROM json_each(?4)))
                     GROUP BY finding ORDER BY min(id)",
                )?
                .query_map(
                    params![
                        role,
                        event_id,
                        kind,
                        (!to.is_empty()).then(|| serde_json::json!(to).to_string())
                    ],
                    |r| r.get(0),
                )?
                .collect::<rusqlite::Result<_>>()?)
        };
        Ok(WrittenBy {
            recorded: findings(event_kind::FINDING_RECORDED, &[])?,
            updated: findings(event_kind::FINDING_UPDATED, &[])?,
            closed: findings(
                event_kind::FINDING_STATUS_CHANGED,
                &[
                    crate::domain::FindingStatus::Resolved.as_str(),
                    crate::domain::FindingStatus::Dismissed.as_str(),
                ],
            )?,
            asks: self
                .conn
                .prepare("SELECT id FROM asks WHERE id>?2 AND asked_by=?1 ORDER BY id")?
                .query_map(params![role, ask_id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?,
        })
    }

    /// The last observation of `mode` that ran its agent: the id and the
    /// payload of its `observe_finished` (a skipped one is not).
    pub fn last_observation(&self, mode: &str) -> Result<Option<(EventId, Value)>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT id, payload FROM run_events
                 WHERE kind='{}' AND json_extract(payload,'$.mode')=?1
                   AND ifnull(json_extract(payload,'$.outcome'),'')<>'skipped'
                 ORDER BY id DESC LIMIT 1",
                    event_kind::OBSERVE_FINISHED
                ),
                [mode],
                |r| Ok((r.get(0)?, json_col(r, "payload")?)),
            )
            .optional()?)
    }

    /// How many events after `after` the observer did not write itself
    /// (ADR-0044): every event but `observe_started` / `observe_finished`,
    /// the findings and asks `role` wrote, the commands of `role` the
    /// authorizer refused, the session events of its own spans
    /// (`span_kind`), and the KPIs' bookkeeping
    /// ([`crate::domain::kpi::BOOKKEEPING_KINDS`], ADR-0051 decision 24).
    pub fn events_besides(&self, role: &str, span_kind: &str, after: EventId) -> Result<i64> {
        let mut ignored = vec![event_kind::OBSERVE_STARTED, event_kind::OBSERVE_FINISHED];
        ignored.extend_from_slice(crate::domain::kpi::BOOKKEEPING_KINDS);
        let ignored = serde_json::to_string(&ignored)?;
        Ok(self.conn.query_row(
            &format!(
                "SELECT count(*) FROM run_events WHERE id>?3 AND NOT (
               kind IN (SELECT value FROM json_each(?4))
               OR (kind IN ('{}','{}','{}')
                   AND json_extract(payload,'$.by') IS ?1)
               OR (kind='{}' AND json_extract(payload,'$.asked_by') IS ?1)
               OR (kind='{}' AND actor_role IS ?1)
               OR (kind IN ('{}','{}','{}')
                   AND json_extract(payload,'$.kind') IS ?2))",
                event_kind::FINDING_RECORDED,
                event_kind::FINDING_UPDATED,
                event_kind::FINDING_STATUS_CHANGED,
                event_kind::ASK_OPENED,
                event_kind::AUTHORIZATION_DENIED,
                event_kind::SESSION_OPENED,
                event_kind::SESSION_CLOSED,
                event_kind::SESSION_TURNS
            ),
            params![role, span_kind, after, ignored],
            |r| r.get(0),
        )?)
    }

    /// The newest `limit` observations, newest first: each
    /// `observe_finished` with the `observe_started` of the same directory
    /// when there is one (a skipped observation has none).
    pub fn observations(&self, limit: usize) -> Result<Vec<(RunEvent, Option<RunEvent>)>> {
        let finished = self.latest_events_of(event_kind::OBSERVE_FINISHED, limit)?;
        finished
            .into_iter()
            .map(|finished| {
                let started = match finished.payload["dir"].as_str() {
                    Some(dir) => self
                        .conn
                        .query_row(
                            &format!(
                                "SELECT * FROM run_events WHERE kind='{}'
                               AND id<?1 AND json_extract(payload,'$.dir')=?2
                             ORDER BY id DESC LIMIT 1",
                                event_kind::OBSERVE_STARTED
                            ),
                            params![finished.id, dir],
                            event_row,
                        )
                        .optional()?,
                    None => None,
                };
                Ok((finished, started))
            })
            .collect()
    }

    /// The latest run whose session opened in `workspace_id`, if any.
    pub fn run_in_workspace(&self, workspace_id: &str) -> Result<Option<RunId>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM task_runs WHERE workspace_id=?1 ORDER BY rowid DESC LIMIT 1",
                [workspace_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Runs a process is responsible for right now (executing under a
    /// supervisor, or being landed by `integrate`), oldest first.
    pub fn active_runs(&self) -> Result<Vec<TaskRun>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM task_runs WHERE status IN ('claimed','starting','running','validating','integrating') ORDER BY rowid")?
            .query_map([], run_row(&self.runs_dir))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The newest `run_events` id, 0 for an empty queue: the cursor that
    /// `status` hands out and `watch` starts from.
    pub fn latest_event_id(&self) -> Result<EventId> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(id),0) FROM run_events", [], |r| {
                r.get(0)
            })?)
    }

    /// Every run event, oldest first, for `stats`. A pure read.
    pub fn all_events(&self) -> Result<Vec<RunEvent>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM run_events ORDER BY id")?
            .query_map([], event_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Events with `after < id <= upto` that `filter` keeps, oldest first,
    /// at most `limit`. A pure read.
    pub fn events_between(
        &self,
        after: EventId,
        upto: EventId,
        filter: &EventFilter,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        let kinds = filter
            .kinds
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        Ok(self
            .conn
            .prepare(
                "SELECT * FROM run_events WHERE id>?1 AND id<=?2
                 AND (?3 IS NULL OR kind IN (SELECT value FROM json_each(?3)))
                 AND (?4 IS NULL OR run_id=?4) AND (?5 IS NULL OR task_id=?5)
                 AND (?6 IS NULL OR goal_id=?6
                      OR task_id IN (SELECT id FROM tasks WHERE goal_id=?6))
                 AND (?7 IS NULL OR julianday(created_at)>=julianday(?7))
                 AND (?8 IS NULL OR julianday(created_at)<julianday(?8))
                 ORDER BY id LIMIT ?9",
            )?
            .query_map(
                params![
                    after,
                    upto,
                    kinds,
                    filter.run,
                    filter.task,
                    filter.goal,
                    filter.since,
                    filter.until,
                    i64::try_from(limit)?
                ],
                event_row,
            )?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Every event of one run, oldest first.
    pub fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM run_events WHERE run_id=?1 ORDER BY id")?
            .query_map([id], event_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The latest run of every `in_progress` task, oldest first: the runs
    /// `status` judges for attention. An older run of a retried task is
    /// history, and a completed or canceled task needs nobody.
    pub fn latest_runs_in_progress(&self) -> Result<Vec<TaskRun>> {
        Ok(self
            .conn
            .prepare(
                "SELECT r.* FROM task_runs r JOIN tasks t ON t.id=r.task_id
                 WHERE t.status='in_progress'
                 AND r.rowid=(SELECT MAX(rowid) FROM task_runs WHERE task_id=r.task_id)
                 ORDER BY r.rowid",
            )?
            .query_map([], run_row(&self.runs_dir))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The `integrated` runs whose push of `main` failed after the latest
    /// successful push (`push_finished`), oldest first. A later successful
    /// push carries every earlier landing, so it clears them all.
    pub fn runs_with_pending_push(&self) -> Result<Vec<TaskRun>> {
        Ok(self
            .conn
            .prepare(
                "SELECT r.* FROM task_runs r WHERE r.status='integrated'
                 AND r.id IN (SELECT run_id FROM run_events WHERE kind=?1
                   AND id>(SELECT COALESCE(MAX(id),0) FROM run_events WHERE kind=?2))
                 ORDER BY r.rowid",
            )?
            .query_map(
                [event_kind::PUSH_FAILED, event_kind::PUSH_FINISHED],
                run_row(&self.runs_dir),
            )?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Every run in one status, oldest first; `up` reports the runs that
    /// wait for a person or the supervisor (`awaiting_integration`, `needs_session`).
    pub fn runs_with_status(&self, status: crate::domain::RunStatus) -> Result<Vec<TaskRun>> {
        Ok(self
            .conn
            .prepare("SELECT * FROM task_runs WHERE status=?1 ORDER BY rowid")?
            .query_map([status.as_str()], run_row(&self.runs_dir))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The oldest run awaiting integration by validation time: the FIFO
    /// order of the merge queue. Runs waiting for a session are skipped;
    /// they are resumed explicitly by `integrate ID`.
    pub fn next_awaiting_integration(&self) -> Result<Option<TaskRun>> {
        Ok(self
            .conn
            .query_row(
                "SELECT r.* FROM task_runs r WHERE r.status='awaiting_integration'
                 ORDER BY (SELECT MIN(e.id) FROM run_events e
                           WHERE e.run_id=r.id AND e.kind=?1) NULLS LAST,
                          r.rowid
                 LIMIT 1",
                [event_kind::VALIDATION_FINISHED],
                run_row(&self.runs_dir),
            )
            .optional()?)
    }

    /// The workspaces of the runs that ended (`integrated`, `succeeded`,
    /// `failed`, `interrupted`) and that no live supervisor leases (a stale
    /// lease, [`lease_is_stale`], counts as none: its holder died between
    /// ending the run and releasing the lease, task 396), except the runs the
    /// triage takes (the latest `failed` / `interrupted` run of an
    /// `in_progress` task): the worker's workspace and every workspace a
    /// `workspace_created` or `resume_finished` of the run names, whether
    /// or not its close is recorded (cmux's list decides), ordered by run.
    pub fn ended_run_workspaces(&self) -> Result<Vec<EndedRunWorkspace>> {
        let mut statement = self.conn.prepare(
            &format!("WITH ended AS (
               SELECT r.id, r.status, r.workspace_id, r.rowid AS run_row FROM task_runs r
               JOIN tasks t ON t.id=r.task_id
               WHERE r.status IN ('integrated','succeeded','failed','interrupted')
               AND NOT (r.status IN ('failed','interrupted') AND t.status='in_progress'
                        AND r.rowid=(SELECT MAX(rowid) FROM task_runs WHERE task_id=r.task_id))
             )
             SELECT id, status, workspace_id, run_row, 0 AS event_id FROM ended
             WHERE workspace_id IS NOT NULL
             UNION ALL
             SELECT ended.id, ended.status, json_extract(e.payload,'$.workspace_id'), ended.run_row, e.id
             FROM run_events e JOIN ended ON ended.id=e.run_id
             WHERE e.kind IN ('{}','{}')
             AND json_extract(e.payload,'$.workspace_id') IS NOT NULL
             ORDER BY 4, 5", event_kind::WORKSPACE_CREATED, event_kind::RESUME_FINISHED),
        )?;
        let rows = statement.query_map([], |row| {
            Ok(EndedRunWorkspace {
                run_id: row.get(0)?,
                status: enum_col(row, "status")?,
                workspace_id: row.get(2)?,
            })
        })?;
        let live = self.live_leased_runs()?;
        let mut seen = std::collections::HashSet::new();
        let mut workspaces: Vec<EndedRunWorkspace> = Vec::new();
        for row in rows {
            let row = row?;
            if !live.contains(&row.run_id)
                && seen.insert((row.run_id.clone(), row.workspace_id.clone()))
            {
                workspaces.push(row);
            }
        }
        Ok(workspaces)
    }

    /// The worktrees of the runs no live supervisor leases that ended
    /// (`integrated`, `succeeded`, `failed`, `interrupted`; a stale lease,
    /// [`lease_is_stale`], counts as none, as in
    /// [`Self::ended_run_workspaces`]), or of any status and no lease at
    /// all once their task is `completed` or `canceled`, by run; each is
    /// where the run's worktree lives under the run directory, whether or
    /// not it is still there.
    pub fn ended_run_worktrees(&self) -> Result<Vec<EndedRunWorktree>> {
        let mut statement = self.conn.prepare(
            "SELECT r.id, r.task_id, r.status AS run_status, t.status AS task_status, r.branch
             FROM task_runs r
             JOIN tasks t ON t.id=r.task_id
             WHERE r.worktree_path IS NOT NULL
             AND (r.status IN ('integrated','succeeded','failed','interrupted')
                  OR t.status IN ('completed','canceled'))
             AND (r.status IN ('integrated','succeeded','failed','interrupted')
                  OR NOT EXISTS (SELECT 1 FROM run_leases l WHERE l.run_id=r.id))
             ORDER BY r.rowid",
        )?;
        let live = self.live_leased_runs()?;
        let rows = statement.query_map([], |row| {
            let run_id: RunId = row.get(0)?;
            Ok(EndedRunWorktree {
                worktree: RunPaths::new(&self.runs_dir, &run_id)
                    .worktree
                    .to_string_lossy()
                    .into_owned(),
                run_id,
                task_id: row.get(1)?,
                status: enum_col(row, "run_status")?,
                task_status: enum_col(row, "task_status")?,
                branch: row.get(4)?,
            })
        })?;
        let mut worktrees = Vec::new();
        for row in rows {
            let row = row?;
            if !live.contains(&row.run_id) {
                worktrees.push(row);
            }
        }
        Ok(worktrees)
    }

    /// The runs whose lease is not stale ([`lease_is_stale`]): a live
    /// supervisor still holds them. The sweep leaves the stale leases of
    /// ended runs in place rather than deleting them: the heartbeat thread
    /// renews every 2 seconds, so a stale lease's holder is dead or hung,
    /// and all it may still do to an ended run is release the lease, which
    /// would fail if the row were gone.
    fn live_leased_runs(&self) -> Result<std::collections::HashSet<RunId>> {
        let now = self.generators.clock.now();
        let mut statement = self
            .conn
            .prepare("SELECT run_id,token,pid,heartbeat_at FROM run_leases")?;
        let leases = statement.query_map([], lease_row)?;
        let mut live = std::collections::HashSet::new();
        for lease in leases {
            let lease = lease?;
            if !lease_is_stale(&lease, now) {
                live.insert(lease.run_id);
            }
        }
        Ok(live)
    }
}

/// The [`RunLog`] port over the inherent methods above, which callers
/// that hold a `SqliteQueue` keep using directly.
impl RunLog for SqliteQueue {
    fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        SqliteQueue::update_events(self, limit)
    }
    fn active_runs(&self) -> Result<Vec<TaskRun>> {
        SqliteQueue::active_runs(self)
    }
    fn all_runs(&self) -> Result<Vec<TaskRun>> {
        SqliteQueue::all_runs(self)
    }
    fn all_events(&self) -> Result<Vec<RunEvent>> {
        SqliteQueue::all_events(self)
    }
    fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
        SqliteQueue::latest_task_events(self, kinds)
    }
    fn run(&self, id: &RunId) -> Result<TaskRun> {
        SqliteQueue::run(self, id)
    }
    fn runs_with_status(&self, status: RunStatus) -> Result<Vec<TaskRun>> {
        SqliteQueue::runs_with_status(self, status)
    }
    fn next_awaiting_integration(&self) -> Result<Option<TaskRun>> {
        SqliteQueue::next_awaiting_integration(self)
    }
    fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
        SqliteQueue::run_events(self, id)
    }
    fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool> {
        SqliteQueue::has_run_event(self, id, kind)
    }
    fn record_runtime_event(
        &self,
        id: &RunId,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        SqliteQueue::record_runtime_event(self, id, kind, payload)
    }
    fn ended_run_workspaces(&self) -> Result<Vec<EndedRunWorkspace>> {
        SqliteQueue::ended_run_workspaces(self)
    }
    fn ended_run_worktrees(&self) -> Result<Vec<EndedRunWorktree>> {
        SqliteQueue::ended_run_worktrees(self)
    }
    fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
        SqliteQueue::last_observe(self, mode)
    }
    fn latest_event_id(&self) -> Result<EventId> {
        SqliteQueue::latest_event_id(self)
    }
    fn latest_runs_in_progress(&self) -> Result<Vec<TaskRun>> {
        SqliteQueue::latest_runs_in_progress(self)
    }
    fn runs_with_pending_push(&self) -> Result<Vec<TaskRun>> {
        SqliteQueue::runs_with_pending_push(self)
    }
    fn run_in_workspace(&self, workspace_id: &str) -> Result<Option<RunId>> {
        SqliteQueue::run_in_workspace(self, workspace_id)
    }
    fn record_backend_failure(
        &self,
        run: Option<&RunId>,
        payload: serde_json::Value,
    ) -> Result<()> {
        SqliteQueue::record_backend_failure(self, run, payload)
    }
    fn record_queue_event(&self, kind: &str, payload: serde_json::Value) -> Result<EventId> {
        SqliteQueue::record_queue_event(self, kind, payload)
    }
    fn latest_event_of(&self, kind: &str) -> Result<Option<RunEvent>> {
        SqliteQueue::latest_event_of(self, kind)
    }
    fn latest_events_of(&self, kind: &str, limit: usize) -> Result<Vec<RunEvent>> {
        SqliteQueue::latest_events_of(self, kind, limit)
    }
    fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>> {
        SqliteQueue::latest_queue_event(self, kinds)
    }
    fn events_of_between(
        &self,
        kinds: &[&str],
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        SqliteQueue::events_of_between(self, kinds, after, upto, limit)
    }
    fn request_as(&self, requester: Option<&crate::domain::actor::ActorContext>) -> Option<String> {
        self.actors
            .request(requester.map(|actor| actor.actor_id().to_owned()))
    }
    fn restore_request(&self, previous: Option<String>) {
        self.actors.request(previous);
    }
    fn act_as(
        &self,
        actor: crate::domain::actor::ActorContext,
    ) -> Option<crate::domain::actor::ActorContext> {
        let previous = self.actors.get();
        self.actors.set(actor);
        Some(previous)
    }
}

/// Insert an event of the queue itself in the open transaction `tx`, with
/// the session span it opens or closes; its id.
pub(super) fn queue_event(tx: &Connection, kind: &str, payload: &Value) -> Result<EventId> {
    tx.execute(
        "INSERT INTO run_events(kind,payload,actor_role,actor_id,requested_by)
         VALUES (?1,?2,dagq_actor_role(),dagq_actor_id(),dagq_requested_by())",
        params![kind, serde_json::to_string(payload)?],
    )?;
    let id = EventId::new(tx.last_insert_rowid());
    // The observer's session span (ADR-0048).
    crate::infrastructure::sessions::follow(tx, id, None, None, kind, payload)?;
    Ok(id)
}
