//! Runs and events as read, and events recorded outside a transition
//! ([`RunReads`], [`EventStore`]), and the records of the inbox's watcher
//! ([`InboxWatchLog`], host運用's).
//!
//! The reads of the queue's own events by kind (no run, task or goal) name
//! `events_by_kind`, why: [`crate::infrastructure::kpi_push`].
use crate::infrastructure::sql_exec::CommitTransaction as _;
use crate::infrastructure::sql_exec::TransactionEntries;
use crate::infrastructure::store_queue::StoreQueue;

use super::*;
use crate::application::{LAST_TURNS_PAGE, LastSessionTurn, RunSessionPair};
use crate::infrastructure::sessions::read_before_port;
use crate::infrastructure::sql_dialect::{JsonType, SqlDialect, TimeForm};
use crate::infrastructure::sql_exec::{
    OptionalRow, SqlRead, SqlResult, SqlRow, SqlWrite, ToSqlValue, sql_params,
};
use crate::infrastructure::store_rows::{enum_col, event_row, json_col, run_row};

/// The events of one kind in a range of ids, oldest first, at most a
/// limit: a range of `events_by_kind`.
pub(super) const EVENTS_OF_KIND_BETWEEN_SQL: &str =
    "SELECT * FROM run_events WHERE kind=?1 AND id>?2 AND id<=?3
     ORDER BY id LIMIT ?4";

/// The rows of [`SqliteQueue::installs_after`]: the queue's events of kind
/// `?1` past id `?2` that installed more than the plugin.
pub(super) fn installs_after_sql(dialect: SqlDialect) -> String {
    format!(
        "FROM run_events INDEXED BY events_by_kind
     WHERE kind = ?1 AND id > ?2
     AND run_id IS NULL AND task_id IS NULL AND goal_id IS NULL
     AND COALESCE({}, 0) = 0",
        dialect.json_field_spaced("payload", "$.plugin_only", JsonType::Boolean)
    )
}

/// The queue's events of the kinds `?1` (a JSON array), newest first, at
/// most `?2`.
pub(super) fn update_events_sql(dialect: SqlDialect) -> String {
    format!(
        "SELECT * FROM run_events INDEXED BY events_by_kind
     WHERE kind IN (SELECT {} FROM {})
     AND run_id IS NULL AND task_id IS NULL AND goal_id IS NULL
     ORDER BY id DESC LIMIT ?2",
        dialect.json_element("value", JsonType::Text),
        dialect.json_elements("?1"),
    )
}

/// The newest of the queue's events of the kind `?1`.
pub(super) const LATEST_QUEUE_EVENT_SQL: &str = "SELECT * FROM run_events INDEXED BY events_by_kind
     WHERE kind=?1 AND run_id IS NULL AND task_id IS NULL AND goal_id IS NULL
     ORDER BY id DESC LIMIT 1";

/// Whether an event of the kind `?1` with the `absent_since` `?2` and the
/// `attempt` `?3` is there.
pub(super) fn inbox_nudged_sql(dialect: SqlDialect) -> String {
    format!(
        "SELECT EXISTS(SELECT 1 FROM run_events INDEXED BY events_by_kind
         WHERE kind=?1 AND run_id IS NULL AND task_id IS NULL AND goal_id IS NULL
         AND {} IS NOT DISTINCT FROM ?2
         AND {} IS NOT DISTINCT FROM ?3)",
        dialect.json_field("payload", "$.absent_since", JsonType::Integer),
        dialect.json_field("payload", "$.attempt", JsonType::Integer),
    )
}

/// The kind and `at` of the latest event of the kinds `?1` and `?2`.
pub(super) fn latest_inbox_watcher_change_sql(dialect: SqlDialect) -> String {
    format!(
        "SELECT kind, {} FROM run_events INDEXED BY events_by_kind
         WHERE kind IN (?1, ?2) AND run_id IS NULL AND task_id IS NULL AND goal_id IS NULL
         ORDER BY id DESC LIMIT 1",
        dialect.json_field("payload", "$.at", JsonType::Integer),
    )
}

/// Three bounded index seeks per pair: newest matching launch, first
/// session boundary after it, newest turn between them. No event list or
/// window partition is accumulated, even for a run with many turns.
pub(super) fn last_turns_sql(dialect: SqlDialect) -> String {
    let field = |column: &str, path: &str| dialect.json_field(column, path, JsonType::Text);
    let start = field("t.payload", "$.start");
    let bytes = match dialect {
        SqlDialect::Sqlite => format!("length(CAST({start} AS BLOB))"),
        SqlDialect::Postgres => format!("octet_length({start})"),
    };
    format!(
        "WITH pairs AS (
           SELECT {run} AS run_id, {handle} AS workspace_id FROM {pairs}
         ), launches AS MATERIALIZED (
           SELECT p.*, (SELECT id FROM run_events
             WHERE run_id=p.run_id AND kind='wrapper_launched' AND {workspace}=p.workspace_id
             ORDER BY id DESC LIMIT 1) AS launched FROM pairs p
         ), bounds AS MATERIALIZED (
           SELECT l.*, (SELECT id FROM run_events
             WHERE run_id=l.run_id AND id>l.launched
             AND kind IN ('workspace_created','wrapper_launched')
             ORDER BY id LIMIT 1) AS ended FROM launches l
         )
         SELECT b.run_id, b.workspace_id,
           CASE WHEN {pid_type}='integer' THEN
             CASE WHEN CAST({pid} AS NUMERIC) BETWEEN 0 AND 4294967295 THEN CAST({pid} AS TEXT) END END,
           CASE WHEN {start_type}='text' AND {bytes}<=128 THEN {start} END
         FROM bounds b JOIN run_events t ON t.id=(
           SELECT id FROM run_events WHERE run_id=b.run_id AND id>b.launched
             AND id<=COALESCE(b.ended-1,9223372036854775807) AND kind='turn_started'
           ORDER BY id DESC LIMIT 1)",
        run = field("value", "$.run_id"),
        handle = field("value", "$.workspace_id"),
        pairs = dialect.json_elements("?1"),
        workspace = field("payload", "$.workspace_id"),
        pid_type = dialect.json_type_of("t.payload", "$.pid"),
        pid = field("t.payload", "$.pid"),
        start_type = dialect.json_type_of("t.payload", "$.start"),
    )
}

/// Execute the bounded read once, through the same SqlRead port on both
/// storage dialects; separated so the adapter test can count statements.
fn read_last_turns(
    reader: &impl SqlRead,
    sessions: &[RunSessionPair],
) -> Result<Vec<LastSessionTurn>> {
    anyhow::ensure!(sessions.len() <= LAST_TURNS_PAGE, "too many session pairs");
    let sessions: Vec<_> = sessions.iter().filter(|s| s.fits_sweep_read()).collect();
    if sessions.is_empty() {
        return Ok(Vec::new());
    }
    let rows = SqlRead::query_map(
        reader,
        &last_turns_sql(reader.dialect()),
        sql_params![serde_json::to_string(&sessions)?],
        |r| {
            let run_id: RunId = r.get(0)?;
            let workspace_id: String = r.get(1)?;
            let pid: Option<String> = r.get(2)?;
            let start: Option<String> = r.get(3)?;
            Ok(pid
                .and_then(|pid| pid.parse::<u32>().ok())
                .zip(start)
                .map(|(pid, start)| LastSessionTurn {
                    run_id,
                    workspace_id,
                    pid,
                    start,
                }))
        },
    )?;
    Ok(rows.into_iter().flatten().collect())
}

/// Kinds whose writers put a top-level `source_run_id` in the payload.
/// `draft_planners::register_follow_ups` writes `follow_up_registered`;
/// `draft_planners::record_adoptions` writes `follow_up_adopted` and
/// `draft_adopted`. Its `origin_fields` also feeds `open_draft_planner`
/// (`draft_planner_opened`), `settle_bundle` (`draft_planner_settled`) and
/// `task_store::apply_transition` (`task_status_changed`, canceling a draft).
/// When a writer starts carrying this field on another kind, extend this
/// list and the source-only fixtures in the events_between oracle test.
/// The JSON predicate has no index, so only these kinds may enter that
/// branch; the run's own events use events_by_run independently.
const SOURCE_RUN_EVENT_KINDS: &[EventKind] = &[
    EventKind::FollowUpRegistered,
    EventKind::FollowUpAdopted,
    EventKind::DraftAdopted,
    EventKind::DraftPlannerOpened,
    EventKind::DraftPlannerSettled,
    EventKind::TaskStatusChanged,
];

/// Build a read without optional-parameter ORs. Parameters are after, upto,
/// then only the present filter fields in declaration order, then limit.
/// Choose the driving index in order: run, task, goal, kinds, or the id range.
/// UNION deduplicates events matching both identity branches before paging.
pub(super) fn events_between_sql(filter: &EventFilter, dialect: SqlDialect) -> String {
    let mut next = 2;
    let mut parameter = |present: bool| {
        present.then(|| {
            next += 1;
            format!("?{next}")
        })
    };
    let kinds = parameter(filter.kinds.is_some());
    let run = parameter(filter.run.is_some());
    let task = parameter(filter.task.is_some());
    let goal = parameter(filter.goal.is_some());
    let since = parameter(filter.since.is_some());
    let until = parameter(filter.until.is_some());
    let mut predicates = vec!["id>?1".to_owned(), "id<=?2".to_owned()];
    if let Some(kinds) = &kinds {
        predicates.push(format!(
            "kind IN (SELECT {} FROM {})",
            dialect.json_element("value", JsonType::Text),
            dialect.json_elements(kinds)
        ));
    }
    if let Some(task) = &task {
        predicates.push(format!("task_id={task}"));
    }
    // With a higher-priority run/task, goal is a residual predicate.
    if let Some(goal) = &goal
        && (run.is_some() || task.is_some())
    {
        predicates.push(format!(
            "(goal_id={goal} OR task_id IN (SELECT id FROM tasks WHERE goal_id={goal}))"
        ));
    }
    for (bound, op) in [(&since, ">="), (&until, "<")] {
        if let Some(bound) = bound {
            predicates.push(format!(
                "{}{op}{}",
                dialect.time_value("created_at", TimeForm::Ordinal),
                dialect.time_value(bound, TimeForm::Ordinal)
            ));
        }
    }
    let common = predicates.join(" AND ");
    let branch = |index: &str, predicate: String| {
        let index = if dialect == SqlDialect::Sqlite && !index.is_empty() {
            format!(" INDEXED BY {index}")
        } else {
            String::new()
        };
        format!("SELECT * FROM run_events{index} WHERE {common}{predicate}")
    };
    let sql = if let Some(run) = run {
        let source_kinds = SOURCE_RUN_EVENT_KINDS
            .iter()
            .map(|kind| format!("'{}'", kind.as_str()))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{} UNION {}",
            branch("events_by_run", format!(" AND run_id={run}")),
            branch(
                "events_by_kind",
                format!(
                    " AND kind IN ({source_kinds}) AND {}={run}",
                    dialect.json_field("payload", "$.source_run_id", JsonType::Text)
                )
            )
        )
    } else if task.is_some() {
        branch("events_by_task", String::new())
    } else if let Some(goal) = goal {
        format!(
            "{} UNION {}",
            branch("events_by_goal", format!(" AND goal_id={goal}")),
            branch(
                "events_by_task",
                format!(" AND task_id IN (SELECT id FROM tasks WHERE goal_id={goal})")
            )
        )
    } else if kinds.is_some() {
        branch("events_by_kind", String::new())
    } else {
        branch("", String::new())
    };
    format!("{sql} ORDER BY id LIMIT ?{}", next + 1)
}

/// Read a run on the caller's transaction, with its queue-local paths
/// resolved. Approval uses its write transaction so checking the run and
/// recording the approval are atomic, without opening a nested read entry.
pub(super) fn run_of(tx: &impl SqlRead, runs_dir: &Path, id: &RunId) -> Result<TaskRun> {
    tx.query_row(
        "SELECT * FROM task_runs WHERE id=?1",
        sql_params![id],
        run_row(runs_dir),
    )
    .optional()?
    .ok_or_else(|| crate::application::RunNotFound(id.clone()).into())
}

impl<E: TransactionEntries> StoreQueue<E> {
    /// See [`RunReads::last_turns_of`]. Malformed last turns stay absent;
    /// parsing never retries an earlier turn. Width is bounded in SQL,
    /// before a start can enter an adapter row or a service answer.
    pub fn last_turns_of(&self, sessions: &[RunSessionPair]) -> Result<Vec<LastSessionTurn>> {
        let tx = self.read_entry("last_turns_of")?;
        read_last_turns(&tx, sessions)
    }

    /// The latest `limit` steps of the automatic update, newest first: the
    /// queue's `update_*` events (ADR-0073 decision 17), each job's
    /// `update_started` before its steps ([`crate::domain::in_job_order`]).
    pub fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        let tx = self.read_entry("update_events")?;
        Ok(crate::domain::in_job_order(SqlRead::query_map(
            &tx,
            &update_events_sql(SqlRead::dialect(&tx)),
            sql_params![
                serde_json::to_string(
                    &crate::domain::UPDATE_EVENT_KINDS
                        .iter()
                        .map(|kind| kind.as_str())
                        .collect::<Vec<_>>()
                )?,
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            event_row,
        )?))
    }

    /// The binary's installs past event `after`, the newest `limit` oldest
    /// first, and their count in all (see
    /// [`crate::application::EventStore::installs_after`]). A pure read.
    pub fn installs_after(&self, after: EventId, limit: usize) -> Result<(Vec<RunEvent>, usize)> {
        let kind = crate::domain::UPDATE_INSTALLED;
        // One read snapshot keeps the installs and their count consistent.
        let tx = self.read_entry("installs_after")?;
        let total: i64 = SqlRead::query_row(
            &tx,
            &format!("SELECT COUNT(*) {}", installs_after_sql(tx.dialect())),
            sql_params![kind, after],
            |row| row.get(0),
        )?;
        let mut installs: Vec<RunEvent> = SqlRead::query_map(
            &tx,
            &format!(
                "SELECT * {} ORDER BY id DESC LIMIT ?3",
                installs_after_sql(tx.dialect())
            ),
            sql_params![kind, after, i64::try_from(limit).unwrap_or(i64::MAX)],
            event_row,
        )?;
        drop(tx);
        installs.reverse();
        Ok((installs, usize::try_from(total).unwrap_or(usize::MAX)))
    }

    /// The latest `limit` events of the e2e gates, newest first (see
    /// [`crate::application::EventStore::e2e_gate_events`]).
    pub fn e2e_gate_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        let tx = self.read_entry("e2e_gate_events")?;
        use crate::domain::event_kind::{RUN_E2E_FAILED, RUN_E2E_FINISHED};
        let kinds = [
            crate::domain::UPDATE_E2E_PASSED,
            crate::domain::UPDATE_FAILED,
            RUN_E2E_FINISHED,
            RUN_E2E_FAILED,
            crate::domain::INSTALL_E2E_FINISHED,
        ];
        Ok(SqlRead::query_map(
            &tx,
            &format!(
                "SELECT * FROM run_events
                 WHERE kind IN (SELECT {} FROM {})
                 ORDER BY id DESC LIMIT ?2",
                SqlRead::dialect(&tx).json_element("value", JsonType::Text),
                SqlRead::dialect(&tx).json_elements("?1"),
            ),
            sql_params![
                serde_json::to_string(&kinds)?,
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            event_row,
        )?)
    }

    /// Whether the run has recorded at least one event of `kind`; an
    /// adopter rebuilds what the previous supervisor already did from these.
    pub fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool> {
        let tx = self.read_entry("has_run_event")?;
        Ok(SqlRead::query_row(
            &tx,
            HAS_RUN_EVENT_SQL,
            sql_params![id, kind],
            |r| r.get(0),
        )?)
    }

    /// Every run of the queue, oldest first.
    pub fn all_runs(&self) -> Result<Vec<TaskRun>> {
        let tx = self.read_entry("all_runs")?;
        Ok(SqlRead::query_map(
            &tx,
            &format!(
                "SELECT * FROM task_runs ORDER BY {insert_order}",
                insert_order = SqlRead::dialect(&tx).insertion_order("")
            ),
            sql_params![],
            run_row(&self.runs_dir),
        )?)
    }

    pub fn run(&self, id: &RunId) -> Result<TaskRun> {
        let tx = self.read_entry("run")?;
        run_of(&tx, &self.runs_dir, id)
    }

    pub fn record_runtime_event(
        &self,
        id: &RunId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<()> {
        // The event and the session spans it opens or closes (ADR-0048),
        // in one write transaction taken up front so it waits for other
        // writers rather than failing to upgrade a read. The transcripts of
        // the spans it closes are read before (task 543). A failed event
        // or COMMIT rolls back as the transaction drops (task 1500), so the
        // connection leaves no transaction open and the closes' lines are
        // known rolled back.
        let _read = read_before_port(self, Closing::Event(id, kind, &payload))?;
        let tx = self.write_entry("record_runtime_event")?;
        run_event_port(&tx, id, kind, payload)?;
        tx.commit()?;
        Ok(())
    }

    /// Record `backend_call_failed`: on `run` (its id) when the call was for
    /// one, otherwise with neither a task nor a run.
    pub fn record_backend_failure(
        &self,
        run: Option<&RunId>,
        payload: serde_json::Value,
    ) -> Result<()> {
        match run {
            Some(id) => {
                let tx = self.write_entry("record_backend_failure")?;
                run_event_port(&tx, id, EventKind::BackendCallFailed, payload)?;
                tx.commit()?;
                Ok(())
            }
            None => self
                .record_queue_event(EventKind::BackendCallFailed, payload)
                .map(drop),
        }
    }

    /// Record the change of the runner rulesets' hold
    /// ([`crate::domain::ruleset_gate::transition`] from the latest of its
    /// pair of events) and, as a hold begins, open its operations ask: a
    /// `blocked` ask about no task (the runtime cannot fix the rulesets and
    /// only reads its answer; the gate resumes by itself). Every record of
    /// the hold names its ask, and the resume closes it, answered or not,
    /// so each hold has one ask of its own. The read, the ask and the event
    /// are one transaction, so a failure leaves neither and a retry opens
    /// the ask once.
    pub fn record_runner_rulesets(
        &self,
        now: std::result::Result<(), &crate::domain::ruleset_gate::Unverified>,
    ) -> Result<()> {
        let at = self.generators.clock.now();
        let tx = self.write_entry("runner_rulesets_hold")?;
        if runner_rulesets_in(&tx, now, at)? {
            tx.commit()?;
        }
        Ok(())
    }

    /// A control-side refusal use case writes both sinks from the same
    /// refusal facts. Generic operational event writes cannot create audits.
    pub(crate) fn record_refusal(
        &self,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<EventId> {
        anyhow::ensure!(
            matches!(
                kind,
                EventKind::AuthorizationDenied | EventKind::QueueServiceUnauthenticated
            ),
            "not a refusal kind"
        );
        let mut audit_failed = false;
        let result = (|| {
            let tx = self.write_entry("record_refusal")?;
            // Failure is counted once per call, even if both sinks fail.
            let mut lost = 0;
            crate::application::audit::write_refusal(
                || {
                    crate::infrastructure::audit::control(
                        &tx,
                        crate::application::audit::authorization_fact(&payload, false),
                    )
                },
                &mut lost,
            );
            audit_failed = lost != 0;
            let id = queue_event(&tx, kind, &payload)?;
            tx.commit()?;
            Ok(id)
        })();
        crate::infrastructure::audit::count_refusal_failure(audit_failed || result.is_err());
        result
    }

    /// Record an event of the queue itself, on no task, goal or run (the
    /// observer's `observe_started` / `observe_finished`); returns its id.
    pub fn record_queue_event(
        &self,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<EventId> {
        crate::domain::check_event_target(kind, None, None)?;
        // Immediate like every other write: a deferred one that read first
        // (the schema, to prepare the insert) got SQLITE_BUSY at once, past
        // the busy timeout, when another supervisor wrote at the same time.
        let _read = read_before_port(self, Closing::Queue(kind, &payload))?;
        let tx = self.write_entry("record_queue_event")?;
        let id = queue_event(&tx, kind, &payload)?;
        tx.commit()?;
        Ok(id)
    }

    /// Record `events`, each an event of the queue itself, in one write
    /// transaction: none is recorded unless all are; returns their ids.
    pub fn record_queue_events(
        &self,
        events: &[(EventKind, serde_json::Value)],
    ) -> Result<Vec<EventId>> {
        for (kind, _) in events {
            crate::domain::check_event_target(*kind, None, None)?;
        }
        let _read = events
            .iter()
            .map(|(kind, payload)| read_before_port(self, Closing::Queue(*kind, payload)))
            .collect::<Result<Vec<_>>>()?;
        let tx = self.write_entry("record_queue_events")?;
        let ids = events
            .iter()
            .map(|(kind, payload)| queue_event(&tx, *kind, payload))
            .collect::<Result<Vec<_>>>()?;
        tx.commit()?;
        Ok(ids)
    }

    /// [`crate::application::EventStore::record_event_once`]: the event
    /// `event_records` names under `record` is this one; otherwise the
    /// event is written and named under `record` in the same transaction.
    pub fn record_event_once(
        &self,
        run: Option<&RunId>,
        kind: EventKind,
        payload: serde_json::Value,
        record: &str,
    ) -> Result<EventId> {
        if run.is_none() {
            crate::domain::check_event_target(kind, None, None)?;
        }
        let closing = match run {
            Some(id) => Closing::Event(id, kind, &payload),
            None => Closing::Queue(kind, &payload),
        };
        let _read = read_before_port(self, closing)?;
        let tx = self.write_entry("record_event_once")?;
        let found: Option<i64> = SqlRead::query_row(
            &tx,
            "SELECT event_id FROM event_records WHERE record=?1",
            sql_params![record],
            |row| row.get(0),
        )
        .optional()?;
        if let Some(id) = found {
            return Ok(EventId::new(id));
        }
        let id = match run {
            Some(id) => {
                run_event_port(&tx, id, kind, payload)?;
                EventId::new(SqlRead::query_row(
                    &tx,
                    "SELECT max(id) FROM run_events WHERE run_id=?1 AND kind=?2",
                    sql_params![id.as_str(), kind.as_str()],
                    |row| row.get(0),
                )?)
            }
            None => queue_event(&tx, kind, &payload)?,
        };
        SqlWrite::execute(
            &tx,
            "INSERT INTO event_records(record, event_id) VALUES (?1, ?2)",
            sql_params![record, id.as_i64()],
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// [`crate::application::EventStore::record_queue_event_if_newest`]:
    /// the newest of `kinds` is read and `kind` written in one write
    /// transaction, so a record made in between, or the same record by a
    /// second supervisor, writes nothing.
    pub fn record_queue_event_if_newest(
        &self,
        kinds: &[&str],
        newest: EventId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<Option<EventId>> {
        crate::domain::check_event_target(kind, None, None)?;
        let _read = read_before_port(self, Closing::Queue(kind, &payload))?;
        let tx = self.write_entry("record_queue_event_if_newest")?;
        let mut latest: Option<EventId> = None;
        for kinded in kinds {
            let event =
                SqlRead::query_row(&tx, LATEST_QUEUE_EVENT_SQL, sql_params![kinded], event_row)
                    .optional()?;
            latest = latest.max(event.map(|event| event.id));
        }
        if latest != Some(newest) {
            return Ok(None);
        }
        let id = queue_event(&tx, kind, &payload)?;
        tx.commit()?;
        Ok(Some(id))
    }

    /// [`crate::application::EventStore::expire_disk_relax`]: the newest
    /// of the relaxation's records is read, judged at the queue's clock and
    /// its end written in one write transaction.
    pub fn expire_disk_relax(&self, relaxed: EventId) -> Result<Option<EventId>> {
        use crate::domain::disk::{RELAX_KINDS, RelaxState, expired_payload, relax_state};
        let kind = EventKind::DiskRelaxExpired;
        let tx = self.write_entry("expire_disk_relax")?;
        let mut newest: Option<RunEvent> = None;
        for kinded in RELAX_KINDS.map(EventKind::as_str) {
            let event =
                SqlRead::query_row(&tx, LATEST_QUEUE_EVENT_SQL, sql_params![kinded], event_row)
                    .optional()?;
            if let Some(event) = event
                && newest.as_ref().is_none_or(|newest| newest.id < event.id)
            {
                newest = Some(event);
            }
        }
        let now = crate::application::unix_millis(self.generators.clock.system_time());
        let RelaxState::Expired(expired) = relax_state(newest.as_ref(), now) else {
            return Ok(None);
        };
        if expired.event_id != relaxed.as_i64() {
            return Ok(None);
        }
        let id = queue_event(&tx, kind, &expired_payload(&expired))?;
        tx.commit()?;
        Ok(Some(id))
    }

    /// Record `inbox_nudged` with `payload` unless one with the same
    /// `absent_since` and `attempt` is recorded, in one write transaction
    /// (ADR-t1433-5 decision 1 (3)): `false` when another supervisor
    /// recorded it first, and only the claimer nudges.
    pub fn claim_inbox_nudge(&self, payload: serde_json::Value) -> Result<bool> {
        let kind = EventKind::InboxNudged;
        crate::domain::check_event_target(kind, None, None)?;
        let _read = read_before_port(self, Closing::Queue(kind, &payload))?;
        let tx = self.write_entry("claim_inbox_nudge")?;
        let claimed: bool = SqlRead::query_row(
            &tx,
            &inbox_nudged_sql(tx.dialect()),
            sql_params![
                kind.as_str(),
                payload["absent_since"].as_i64(),
                payload["attempt"].as_i64()
            ],
            |r| r.get(0),
        )?;
        if claimed {
            return Ok(false);
        }
        queue_event(&tx, kind, &payload)?;
        tx.commit()?;
        Ok(true)
    }

    /// Record `kind` (`inbox_watcher_absent` or `inbox_watcher_returned`)
    /// with `payload` unless the latest of the two on the queue is already
    /// `kind`, in one write transaction (task 1021): `false` when the
    /// state did not change, another supervisor recorded the change first
    /// (each change is recorded once), or the latest record's `at` is
    /// later than `payload`'s (a judgment older than the recorded one).
    pub fn record_inbox_watcher_change(
        &self,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<bool> {
        anyhow::ensure!(
            matches!(
                kind,
                EventKind::InboxWatcherAbsent | EventKind::InboxWatcherReturned
            ),
            "{} is not a change of the inbox's watcher",
            kind.as_str()
        );
        crate::domain::check_event_target(kind, None, None)?;
        let _read = read_before_port(self, Closing::Queue(kind, &payload))?;
        let tx = self.write_entry("record_inbox_watcher_change")?;
        let latest: Option<(String, Option<i64>)> = SqlRead::query_row(
            &tx,
            &latest_inbox_watcher_change_sql(tx.dialect()),
            sql_params![
                EventKind::InboxWatcherAbsent.as_str(),
                EventKind::InboxWatcherReturned.as_str()
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
        // The same state, or a judgment older than the one recorded (a
        // supervisor slower to write than another that judged later).
        if let Some((latest, at)) = latest
            && (latest == kind.as_str()
                || at
                    .zip(payload["at"].as_i64())
                    .is_some_and(|(at, now)| now < at))
        {
            return Ok(false);
        }
        queue_event(&tx, kind, &payload)?;
        tx.commit()?;
        Ok(true)
    }

    /// The events of one of `kinds` with `after < id <= upto`, oldest
    /// first, at most `limit`
    /// ([`EVENTS_OF_KIND_BETWEEN_SQL`]).
    /// One range per kind, so each walks `events_by_kind`: the
    /// supervisor's sweep reads these every minute, and a kind list made
    /// SQLite walk every event of the range instead.
    pub fn events_of_between(
        &self,
        kinds: &[&str],
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        let tx = self.read_entry("events_of_between")?;
        let mut kinds = kinds.to_vec();
        kinds.sort_unstable();
        kinds.dedup();
        let mut events = Vec::new();
        for kind in kinds {
            events.extend(SqlRead::query_map(
                &tx,
                EVENTS_OF_KIND_BETWEEN_SQL,
                sql_params![kind, after, upto, i64::try_from(limit).unwrap_or(i64::MAX)],
                event_row,
            )?);
        }
        events.sort_by_key(|event| event.id);
        events.truncate(limit);
        Ok(events)
    }

    /// The newest event of `kind`, on whatever task, goal or run.
    pub fn latest_event_of(&self, kind: &str) -> Result<Option<RunEvent>> {
        let tx = self.read_entry("latest_event_of")?;
        Ok(SqlRead::query_row(
            &tx,
            "SELECT * FROM run_events WHERE kind=?1 ORDER BY id DESC LIMIT 1",
            sql_params![kind],
            event_row,
        )
        .optional()?)
    }

    /// The newest `limit` events of `kind`, on whatever task, goal or run,
    /// newest first.
    pub fn latest_events_of(&self, kind: &str, limit: usize) -> Result<Vec<RunEvent>> {
        let tx = self.read_entry("latest_events_of")?;
        Ok(SqlRead::query_map(
            &tx,
            "SELECT * FROM run_events WHERE kind=?1 ORDER BY id DESC LIMIT ?2",
            sql_params![kind, i64::try_from(limit).unwrap_or(i64::MAX)],
            event_row,
        )?)
    }

    /// The newest `inbox_opened` of the inbox whose parent is the person
    /// `parent` (its `issuer` and `subject`), or of the host's inbox (no
    /// parent) for `None`: filtered in the query, so however many opens of
    /// others come after it, it is found.
    pub fn latest_inbox_opened(&self, parent: Option<(&str, &str)>) -> Result<Option<RunEvent>> {
        self.latest_inbox_event(crate::domain::EventKind::InboxOpened, parent)
    }

    /// [`Self::latest_inbox_opened`] of the event `kind` (an
    /// `inbox_guardrail_reached` too, whose `parent` the service writes).
    pub fn latest_inbox_event(
        &self,
        kind: crate::domain::EventKind,
        parent: Option<(&str, &str)>,
    ) -> Result<Option<RunEvent>> {
        let tx = self.read_entry("latest_inbox_event")?;
        let dialect = SqlRead::dialect(&tx);
        let issuer = dialect.json_field_spaced("payload", "$.parent.issuer", JsonType::Text);
        let subject = dialect.json_field_spaced("payload", "$.parent.subject", JsonType::Text);
        let kind = kind.as_str();
        Ok(match parent {
            Some((parent_issuer, parent_subject)) => SqlRead::query_row(
                &tx,
                &format!(
                    "SELECT * FROM run_events WHERE kind=?1 AND {issuer}=?2 AND {subject}=?3 \
                     ORDER BY id DESC LIMIT 1"
                ),
                sql_params![kind, parent_issuer, parent_subject],
                event_row,
            ),
            None => SqlRead::query_row(
                &tx,
                &format!(
                    "SELECT * FROM run_events WHERE kind=?1 AND {subject} IS NULL \
                     ORDER BY id DESC LIMIT 1"
                ),
                sql_params![kind],
                event_row,
            ),
        }
        .optional()?)
    }

    /// The newest event of the queue itself (on no task, goal or run) of one
    /// of `kinds`.
    /// One lookup per kind, so each walks `events_by_kind` from its newest
    /// row: the supervisor reads these on every pass, and a kind list made
    /// SQLite walk every goal-less event instead.
    pub fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>> {
        let tx = self.read_entry("latest_queue_event")?;
        let mut latest: Option<RunEvent> = None;
        for kind in kinds {
            let event =
                SqlRead::query_row(&tx, LATEST_QUEUE_EVENT_SQL, sql_params![kind], event_row)
                    .optional()?;
            if let Some(event) = event
                && latest.as_ref().is_none_or(|latest| latest.id < event.id)
            {
                latest = Some(event);
            }
        }
        Ok(latest)
    }

    /// For each of `supervisors`, the newest event of one of `kinds` whose
    /// payload names it as its `supervisor`, in one query.
    pub fn latest_events_by_supervisor(
        &self,
        kinds: &[&str],
        supervisors: &[&str],
    ) -> Result<Vec<RunEvent>> {
        let tx = self.read_entry("latest_events_by_supervisor")?;
        if kinds.is_empty() || supervisors.is_empty() {
            return Ok(Vec::new());
        }
        let kind_marks = vec!["?"; kinds.len()].join(",");
        let supervisor_marks = vec!["?"; supervisors.len()].join(",");
        Ok(SqlRead::query_map(
            &tx,
            &format!(
                "SELECT * FROM run_events WHERE id IN (
                     SELECT MAX(id) FROM run_events
                     WHERE kind IN ({kind_marks})
                       AND {supervisor} IN ({supervisor_marks})
                     GROUP BY {supervisor})
                 ORDER BY id",
                supervisor = SqlRead::dialect(&tx).json_field_spaced(
                    "payload",
                    "$.supervisor",
                    JsonType::Text
                ),
            ),
            &kinds
                .iter()
                .chain(supervisors)
                .map(|value| value.to_sql_value())
                .collect::<Vec<_>>(),
            event_row,
        )?)
    }

    /// Per task, its newest event of one of `kinds`, by task.
    pub fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
        let tx = self.read_entry("latest_task_events")?;
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; kinds.len()].join(",");
        Ok(SqlRead::query_map(
            &tx,
            &format!(
                "SELECT * FROM run_events WHERE id IN (
                     SELECT MAX(id) FROM run_events
                     WHERE task_id IS NOT NULL AND kind IN ({marks})
                     GROUP BY task_id)
                 ORDER BY task_id"
            ),
            &kinds
                .iter()
                .map(|value| value.to_sql_value())
                .collect::<Vec<_>>(),
            event_row,
        )?)
    }

    /// The id of the last event recorded before `unix` (seconds), 0 when
    /// there is none: a cursor that reads everything from that time on.
    pub fn event_id_before(&self, unix: i64) -> Result<EventId> {
        let tx = self.read_entry("event_id_before")?;
        Ok(SqlRead::query_row(
            &tx,
            &format!(
                "SELECT COALESCE(max(id),0) FROM run_events
             WHERE {} < ?1",
                SqlRead::dialect(&tx).time_value("created_at", TimeForm::EpochSeconds)
            ),
            sql_params![unix],
            |r| r.get(0),
        )?)
    }

    /// When the observer of `mode` last started or finished (unix seconds).
    pub fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
        let tx = self.read_entry("last_observe")?;
        Ok(SqlRead::query_row(
            &tx,
            &format!(
                "SELECT max({}) FROM run_events
             WHERE kind IN (?2,?3)
               AND {}=?1",
                SqlRead::dialect(&tx).time_value("created_at", TimeForm::EpochSeconds),
                SqlRead::dialect(&tx).json_field("payload", "$.mode", JsonType::Text),
            ),
            sql_params![
                mode,
                event_kind::OBSERVE_STARTED,
                event_kind::OBSERVE_FINISHED
            ],
            |r| r.get(0),
        )?)
    }

    /// The newest ask's ID: the mark [`Self::written_by`] counts past.
    pub fn ask_high_water(&self) -> Result<AskId> {
        let tx = self.read_entry("ask_high_water")?;
        Ok(SqlRead::query_row(
            &tx,
            "SELECT COALESCE(max(id),0) FROM asks",
            sql_params![],
            |r| r.get(0),
        )?)
    }

    /// What `role` wrote after the marks: the ids of the findings it
    /// recorded, updated and closed (`finding_recorded`, `finding_updated`,
    /// and `finding_status_changed` to `resolved` or `dismissed`, after
    /// `event_id`, each id once, oldest first) and of its asks after
    /// `ask_id`. A reopening by a record (`to: open`) is not a close.
    /// `without_ask` is the recorded and updated ones it did not close
    /// with no `blocked` ask opened after `ask_id` or open now.
    pub fn written_by(&self, role: &str, event_id: EventId, ask_id: AskId) -> Result<WrittenBy> {
        let tx = self.read_entry("written_by")?;
        let findings = |kind: &str, to: &[&str]| -> Result<Vec<i64>> {
            Ok(SqlRead::query_map(
                &tx,
                &format!(
                    "SELECT {} AS finding FROM run_events
                     WHERE id>?2 AND kind=?3 AND {}=?1
                       AND (CAST(?4 AS TEXT) IS NULL OR {}
                            IN (SELECT {to} FROM {tos}))
                     GROUP BY finding ORDER BY min(id)",
                    SqlRead::dialect(&tx).json_field("payload", "$.finding_id", JsonType::Integer),
                    SqlRead::dialect(&tx).json_field("payload", "$.by", JsonType::Text),
                    SqlRead::dialect(&tx).json_field("payload", "$.to", JsonType::Text),
                    to = SqlRead::dialect(&tx).json_element("value", JsonType::Text),
                    tos = SqlRead::dialect(&tx).json_elements("?4"),
                ),
                sql_params![
                    role,
                    event_id,
                    kind,
                    (!to.is_empty()).then(|| serde_json::json!(to).to_string())
                ],
                |r| r.get(0),
            )?)
        };
        let mut written = WrittenBy {
            recorded: findings(event_kind::FINDING_RECORDED, &[])?,
            updated: findings(event_kind::FINDING_UPDATED, &[])?,
            closed: findings(
                event_kind::FINDING_STATUS_CHANGED,
                &[
                    crate::domain::FindingStatus::Resolved.as_str(),
                    crate::domain::FindingStatus::Dismissed.as_str(),
                ],
            )?,
            asks: SqlRead::query_map(
                &tx,
                "SELECT id FROM asks WHERE id>?2 AND asked_by=?1 ORDER BY id",
                sql_params![role, ask_id],
                |r| r.get(0),
            )?,
            without_ask: Vec::new(),
        };
        let asked = "SELECT EXISTS(SELECT 1 FROM asks WHERE kind=?1 AND finding_id=?2
               AND (id>?3 OR (answered_at IS NULL AND closed_at IS NULL)))";
        for &finding in written.recorded.iter().chain(&written.updated) {
            if written.without_ask.contains(&finding) || written.closed.contains(&finding) {
                continue;
            }
            if !SqlRead::query_row(
                &tx,
                asked,
                sql_params![crate::domain::AskKind::Blocked.as_str(), finding, ask_id],
                |r| r.get::<_, bool>(0),
            )? {
                written.without_ask.push(finding);
            }
        }
        Ok(written)
    }

    /// The last observation of `mode` that ran its agent: the id and the
    /// payload of its `observe_finished` (a skipped one is not).
    pub fn last_observation(&self, mode: &str) -> Result<Option<(EventId, Value)>> {
        let tx = self.read_entry("last_observation")?;
        Ok(SqlRead::query_row(
            &tx,
            &format!(
                "SELECT id, payload FROM run_events
                 WHERE kind='{}' AND {}=?1
                   AND COALESCE({},'')<>'skipped'
                 ORDER BY id DESC LIMIT 1",
                event_kind::OBSERVE_FINISHED,
                SqlRead::dialect(&tx).json_field("payload", "$.mode", JsonType::Text),
                SqlRead::dialect(&tx).json_field("payload", "$.outcome", JsonType::Text),
            ),
            sql_params![mode],
            |r| Ok((r.get(0)?, json_col(r, "payload")?)),
        )
        .optional()?)
    }

    /// How many events after `after` the observer did not write itself
    /// (ADR-0044): every event but `observe_started` / `observe_finished`,
    /// the findings and asks `role` wrote, the commands of `role` the
    /// authorizer refused, the session events of its own spans
    /// (`span_kind`), the issue and revocation of telemetry tokens (its own
    /// observation's among them, ADR-t1662-3 decision 11), and the KPIs'
    /// bookkeeping ([`crate::domain::kpi::BOOKKEEPING_KINDS`], ADR-0051
    /// decision 24).
    pub fn events_besides(&self, role: &str, span_kind: &str, after: EventId) -> Result<i64> {
        let tx = self.read_entry("events_besides")?;
        let mut ignored = vec![
            event_kind::OBSERVE_STARTED,
            event_kind::OBSERVE_FINISHED,
            event_kind::TELEMETRY_TOKEN_ISSUED,
            event_kind::TELEMETRY_TOKEN_REVOKED,
        ];
        ignored.extend_from_slice(crate::domain::kpi::BOOKKEEPING_KINDS);
        let ignored = serde_json::to_string(&ignored)?;
        Ok(SqlRead::query_row(
            &tx,
            &format!(
                "SELECT count(*) FROM run_events WHERE id>?3 AND NOT (
               kind IN (SELECT {ignored_kind} FROM {ignored_kinds})
               OR (kind IN ('{}','{}','{}')
                   AND {by} IS NOT DISTINCT FROM ?1)
               OR (kind='{}' AND {asked_by} IS NOT DISTINCT FROM ?1)
               OR (kind='{}' AND actor_role IS NOT DISTINCT FROM ?1)
               OR (kind IN ('{}','{}','{}')
                   AND {kind} IS NOT DISTINCT FROM ?2))",
                event_kind::FINDING_RECORDED,
                event_kind::FINDING_UPDATED,
                event_kind::FINDING_STATUS_CHANGED,
                event_kind::ASK_OPENED,
                event_kind::AUTHORIZATION_DENIED,
                event_kind::SESSION_OPENED,
                event_kind::SESSION_CLOSED,
                event_kind::SESSION_TURNS,
                ignored_kind = SqlRead::dialect(&tx).json_element("value", JsonType::Text),
                ignored_kinds = SqlRead::dialect(&tx).json_elements("?4"),
                by = SqlRead::dialect(&tx).json_field("payload", "$.by", JsonType::Text),
                asked_by =
                    SqlRead::dialect(&tx).json_field("payload", "$.asked_by", JsonType::Text),
                kind = SqlRead::dialect(&tx).json_field("payload", "$.kind", JsonType::Text),
            ),
            sql_params![role, span_kind, after, ignored],
            |r| r.get(0),
        )?)
    }

    /// The newest `limit` observations, newest first: each
    /// `observe_finished` with the `observe_started` of the same directory
    /// when there is one (a skipped observation has none).
    pub fn observations(&self, limit: usize) -> Result<Vec<(RunEvent, Option<RunEvent>)>> {
        let finished = self.latest_events_of(event_kind::OBSERVE_FINISHED, limit)?;
        let tx = self.read_entry("observations")?;
        finished
            .into_iter()
            .map(|finished| {
                let started = match finished.payload["dir"].as_str() {
                    Some(dir) => SqlRead::query_row(
                        &tx,
                        &format!(
                            "SELECT * FROM run_events WHERE kind='{}'
                               AND id<?1 AND {}=?2
                             ORDER BY id DESC LIMIT 1",
                            event_kind::OBSERVE_STARTED,
                            SqlRead::dialect(&tx).json_field("payload", "$.dir", JsonType::Text),
                        ),
                        sql_params![finished.id, dir],
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
        let tx = self.read_entry("run_in_workspace")?;
        let current = SqlRead::query_row(
            &tx,
            &format!("SELECT id FROM task_runs WHERE workspace_id=?1 ORDER BY {insert_order} DESC LIMIT 1", insert_order = SqlRead::dialect(&tx).insertion_order("")),
            sql_params![workspace_id],
            |r| r.get(0),
        )
        .optional()?;
        if current.is_some() {
            return Ok(current);
        }
        // A resume's workspace (or a background wrapper's handle,
        // ADR-t1404-1) is recorded in the run's events only.
        Ok(SqlRead::query_row(
            &tx,
            &format!(
                "SELECT run_id FROM run_events WHERE kind='workspace_created'
                 AND run_id IS NOT NULL AND {}=?1
                 ORDER BY id DESC LIMIT 1",
                SqlRead::dialect(&tx).json_field("payload", "$.workspace_id", JsonType::Text),
            ),
            sql_params![workspace_id],
            |r| r.get(0),
        )
        .optional()?)
    }

    /// Runs a process is responsible for right now (executing under a
    /// supervisor, or being landed by `integrate`), oldest first.
    pub fn active_runs(&self) -> Result<Vec<TaskRun>> {
        let tx = self.read_entry("active_runs")?;
        Ok(SqlRead::query_map(
            &tx,
            &format!(
                "SELECT * FROM task_runs WHERE status IN ('claimed','starting','running','validating','integrating') ORDER BY {insert_order}",
                insert_order = SqlRead::dialect(&tx).insertion_order("")
            ),
            sql_params![],
            run_row(&self.runs_dir),
        )?)
    }

    /// The newest `run_events` id, 0 for an empty queue: the cursor that
    /// `status` hands out and `watch` starts from.
    pub fn latest_event_id(&self) -> Result<EventId> {
        let tx = self.read_entry("latest_event_id")?;
        Ok(SqlRead::query_row(
            &tx,
            "SELECT COALESCE(MAX(id),0) FROM run_events",
            sql_params![],
            |r| r.get(0),
        )?)
    }

    /// Every run event, oldest first, for `stats`. A pure read.
    pub fn all_events(&self) -> Result<Vec<RunEvent>> {
        let tx = self.read_entry("all_events")?;
        Ok(SqlRead::query_map(
            &tx,
            "SELECT * FROM run_events ORDER BY id",
            sql_params![],
            event_row,
        )?)
    }

    /// When the earliest event was recorded (unix milliseconds), by its
    /// `created_at` and not its id, reading only that column of the rows in
    /// the order of their times until one whose time
    /// [`crate::domain::stats::timestamp_millis`] reads. A pure read.
    pub fn earliest_event_at(&self) -> Result<Option<i64>> {
        let tx = self.read_entry("earliest_event_at")?;
        let at = SqlRead::dialect(&tx).time_value("created_at", TimeForm::Ordinal);
        let statement = format!(
            "SELECT created_at FROM run_events WHERE {at} IS NOT NULL
             ORDER BY {at}, id",
        );
        let mut found = None;
        SqlRead::visit_rows(&tx, &statement, &[], &mut |row| {
            let created_at: String = row.get(0)?;
            if let Some(millis) = crate::domain::stats::timestamp_millis(&created_at) {
                found = Some(millis);
                return Ok(false);
            }
            Ok(true)
        })?;
        Ok(found)
    }

    /// The default window of `stats` without reading every event
    /// ([`crate::domain::stats::window`]): the run events from the newest
    /// back, each run's first by `events_by_run` once one of its events
    /// that ends it is met, until [`crate::domain::stats::DEFAULT_RUNS`]
    /// runs ended past the event read, which no run met later can end
    /// before; then the times of the window's events, without their
    /// payloads. Every read stops at the latest event read first, so an
    /// event written meanwhile moves no bound. A pure read.
    pub fn stats_window(&self) -> Result<crate::domain::stats::StatsWindow> {
        use crate::domain::stats::{DEFAULT_RUNS, StatsWindow, timestamp_millis, window};
        let upto = self.latest_event_id()?;
        let tx = self.read_entry("stats_window")?;
        // Each run met: its first event and the first event that ended it.
        let mut ended: HashMap<RunId, (EventId, EventId)> = HashMap::new();
        let newest = "SELECT * FROM run_events WHERE run_id IS NOT NULL AND task_id IS NOT NULL
             AND id<=?1 ORDER BY id DESC";
        let of_run = "SELECT * FROM run_events WHERE run_id=?1 AND id<=?2 ORDER BY id";
        SqlRead::visit_rows(&tx, newest, sql_params![upto], &mut |row| {
            let event = event_row(row)?;
            if ended.values().filter(|(_, end)| *end > event.id).count() >= DEFAULT_RUNS {
                return Ok(false);
            }
            let Some(run_id) = event.run_id.as_ref().filter(|_| window::ends_run(&event)) else {
                return Ok(true);
            };
            if ended.contains_key(run_id) {
                return Ok(true);
            }
            let mut first = None;
            let mut end = event.id;
            SqlRead::visit_rows(&tx, of_run, sql_params![run_id, upto], &mut |row| {
                let run_event = event_row(row)?;
                first.get_or_insert(run_event.id);
                if window::ends_run(&run_event) {
                    end = run_event.id;
                    return Ok(false);
                }
                Ok(true)
            })?;
            ended.insert(run_id.clone(), (first.unwrap_or(end), end));
            Ok(true)
        })?;
        let latest = window::latest_ended(
            ended
                .iter()
                .map(|(run_id, (_, end))| (run_id.clone(), *end))
                .collect(),
        );
        let after = window::window_after(latest.iter().map(|(run, _)| ended[run].0).min());
        let (mut start_ms, mut end_ms) = (None::<i64>, None::<i64>);
        let times = "SELECT created_at FROM run_events WHERE id>?1 AND id<=?2";
        SqlRead::visit_rows(&tx, times, sql_params![after, upto], &mut |row| {
            if let Some(at) = timestamp_millis(&row.get::<_, String>(0)?) {
                start_ms = Some(start_ms.map_or(at, |start| start.min(at)));
                end_ms = Some(end_ms.map_or(at, |end| end.max(at)));
            }
            Ok(true)
        })?;
        Ok(StatsWindow {
            after,
            upto,
            start_ms,
            end_ms,
        })
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
        let tx = self.read_entry("events_between")?;
        let kinds = filter
            .kinds
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        // Keep this order in step with the compact parameters in events_between_sql.
        let mut params = vec![after.to_sql_value(), upto.to_sql_value()];
        for value in [
            kinds.map(|v| v.to_sql_value()),
            filter.run.as_ref().map(ToSqlValue::to_sql_value),
            filter.task.map(|v| v.to_sql_value()),
            filter.goal.map(|v| v.to_sql_value()),
            filter.since.as_ref().map(ToSqlValue::to_sql_value),
            filter.until.as_ref().map(ToSqlValue::to_sql_value),
        ]
        .into_iter()
        .flatten()
        {
            params.push(value);
        }
        params.push(i64::try_from(limit)?.to_sql_value());
        Ok(SqlRead::query_map(
            &tx,
            &events_between_sql(filter, SqlRead::dialect(&tx)),
            &params,
            event_row,
        )?)
    }

    /// Every event of one run, oldest first.
    pub fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
        let tx = self.read_entry("run_events")?;
        run_events_of_port(&tx, id)
    }

    /// Every event of one task (its runs' too), oldest first, by
    /// `events_by_task`.
    pub fn task_events(&self, task: TaskId) -> Result<Vec<RunEvent>> {
        let tx = self.read_entry("task_events")?;
        Ok(SqlRead::query_map(
            &tx,
            "SELECT * FROM run_events WHERE task_id=?1 ORDER BY id",
            sql_params![task],
            event_row,
        )?)
    }

    /// The latest run of every `in_progress` task, oldest first: the runs
    /// `status` judges for attention. An older run of a retried task is
    /// history, and a completed or canceled task needs nobody.
    pub fn latest_runs_in_progress(&self) -> Result<Vec<TaskRun>> {
        let tx = self.read_entry("latest_runs_in_progress")?;
        Ok(SqlRead::query_map(
            &tx,
            &format!("SELECT r.* FROM task_runs r JOIN tasks t ON t.id=r.task_id
                 WHERE t.status='in_progress'
                 AND {insert_order_r}=(SELECT MAX({insert_order}) FROM task_runs WHERE task_id=r.task_id)
                 ORDER BY {insert_order_r}", insert_order_r = SqlRead::dialect(&tx).insertion_order("r"), insert_order = SqlRead::dialect(&tx).insertion_order("")),
            sql_params![],
            run_row(&self.runs_dir),
        )?)
    }

    /// The `integrated` runs whose push of `main` failed after the latest
    /// successful push (`push_finished`), oldest first. A later successful
    /// push carries every earlier landing, so it clears them all.
    pub fn runs_with_pending_push(&self) -> Result<Vec<TaskRun>> {
        let tx = self.read_entry("runs_with_pending_push")?;
        Ok(SqlRead::query_map(
            &tx,
            &format!(
                "SELECT r.* FROM task_runs r WHERE r.status='integrated'
                 AND r.id IN (SELECT run_id FROM run_events WHERE kind=?1
                   AND id>(SELECT COALESCE(MAX(id),0) FROM run_events WHERE kind=?2))
                 ORDER BY {insert_order_r}",
                insert_order_r = SqlRead::dialect(&tx).insertion_order("r")
            ),
            sql_params![event_kind::PUSH_FAILED, event_kind::PUSH_FINISHED],
            run_row(&self.runs_dir),
        )?)
    }

    /// Every run in one status, oldest first; `up` reports the runs that
    /// wait for a person or the supervisor (`awaiting_integration`, `needs_session`).
    pub fn runs_with_status(&self, status: crate::domain::RunStatus) -> Result<Vec<TaskRun>> {
        let tx = self.read_entry("runs_with_status")?;
        Ok(SqlRead::query_map(
            &tx,
            &format!(
                "SELECT * FROM task_runs WHERE status=?1 ORDER BY {insert_order}",
                insert_order = SqlRead::dialect(&tx).insertion_order("")
            ),
            sql_params![status.as_str()],
            run_row(&self.runs_dir),
        )?)
    }

    /// The oldest run awaiting integration by validation time: the FIFO
    /// order of the merge queue. Runs waiting for a session are skipped;
    /// they are resumed explicitly by `integrate ID`. So are the runs of a
    /// task a replanning operation guards (ADR-t1521-2) or a hold is open on
    /// (ADR-t1879-1), which wait for the outcome or the release without
    /// holding back the others.
    pub fn next_awaiting_integration(&self) -> Result<Option<TaskRun>> {
        let tx = self.read_entry("next_awaiting_integration")?;
        Ok(SqlRead::query_row(
            &tx,
            &format!(
                concat!(
                    "SELECT r.* FROM task_runs r WHERE r.status='awaiting_integration'
                 AND NOT EXISTS (SELECT 1 FROM replan_operations o WHERE o.task_id=r.task_id
                   AND o.status IN (",
                    crate::infrastructure::sqlite::replan_guarding!(),
                    "))
                 AND NOT EXISTS (SELECT 1 FROM task_holds h WHERE h.task_id=r.task_id
                   AND h.released_at IS NULL)
                 ORDER BY (SELECT MIN(e.id) FROM run_events e
                           WHERE e.run_id=r.id AND e.kind=?1) NULLS LAST,
                          {insert_order_r}
                 LIMIT 1"
                ),
                insert_order_r = SqlRead::dialect(&tx).insertion_order("r")
            ),
            sql_params![event_kind::VALIDATION_FINISHED],
            run_row(&self.runs_dir),
        )
        .optional()?)
    }

    /// The sessions of the runs that ended (`integrated`, `succeeded`,
    /// `failed`, `interrupted`) and that no live supervisor leases (a stale
    /// lease, [`lease_is_stale`], counts as none: its holder died between
    /// ending the run and releasing the lease, task 396), except the runs the
    /// triage takes (the latest `failed` / `interrupted` run of an
    /// `in_progress` task): the worker's session and every session a
    /// `workspace_created` or `resume_finished` of the run names, whether
    /// or not its stop is recorded (whether its wrapper still runs decides),
    /// ordered by run.
    pub fn ended_run_workspaces(&self) -> Result<Vec<EndedRunWorkspace>> {
        let tx = self.read_entry("ended_run_workspaces")?;
        let statement = format!(
            "WITH ended AS (
               SELECT r.id, r.status, r.workspace_id, {insert_order_r} AS run_row FROM task_runs r
               JOIN tasks t ON t.id=r.task_id
               WHERE r.status IN ('integrated','succeeded','failed','interrupted')
               AND NOT (r.status IN ('failed','interrupted') AND t.status='in_progress'
                        AND {insert_order_r}=(SELECT MAX({insert_order}) FROM task_runs WHERE task_id=r.task_id))
             )
             SELECT id, status, workspace_id, run_row, 0 AS event_id FROM ended
             WHERE workspace_id IS NOT NULL
             UNION ALL
             SELECT ended.id, ended.status, {workspace}, ended.run_row, e.id
             FROM run_events e JOIN ended ON ended.id=e.run_id
             WHERE e.kind IN ('{}','{}')
             AND {workspace} IS NOT NULL
             ORDER BY 4, 5",
            event_kind::WORKSPACE_CREATED,
            event_kind::RESUME_FINISHED,
            workspace = SqlRead::dialect(&tx).json_field(
                "e.payload",
                "$.workspace_id",
                JsonType::Text
            ),
            insert_order_r = SqlRead::dialect(&tx).insertion_order("r"),
            insert_order = SqlRead::dialect(&tx).insertion_order("")
        );
        let rows = SqlRead::query_map(&tx, &statement, sql_params![], |row| {
            Ok(EndedRunWorkspace {
                run_id: row.get(0)?,
                status: enum_col(row, "status")?,
                workspace_id: row.get(2)?,
            })
        })?;
        drop(tx);
        let live = self.live_leased_runs()?;
        let mut seen = std::collections::HashSet::new();
        let mut workspaces: Vec<EndedRunWorkspace> = Vec::new();
        for row in rows {
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
    /// all once their task is `completed` or `canceled`
    /// ([`WorktreeCleanup::Ended`]), by run; each is where the run's
    /// worktree lives under the run directory, whether or not it is still
    /// there. With them, the `awaiting_integration` and `needs_session`
    /// runs of a task that goes on with no lease at all and no session
    /// heartbeating (task 1289): [`WorktreeCleanup::AwaitingAnswer`] when
    /// an ask of the run is open and unanswered, else
    /// [`WorktreeCleanup::Idle`].
    pub fn ended_run_worktrees(&self) -> Result<Vec<EndedRunWorktree>> {
        let live = self.live_leased_runs()?;
        let tx = self.read_entry("ended_run_worktrees")?;
        let statement = format!(
            "{ENDED_RUN_WORKTREES} ORDER BY {insert_order_r}",
            insert_order_r = SqlRead::dialect(&tx).insertion_order("r")
        );
        let now = self.generators.clock.now();
        let rows = SqlRead::query_map(
            &tx,
            &statement,
            sql_params![now, HEARTBEAT_TIMEOUT_SECS],
            |row| self.ended_run_worktree_row(row),
        )?;
        let mut worktrees = Vec::new();
        for row in rows {
            if !live.contains(&row.run_id) {
                worktrees.push(row);
            }
        }
        Ok(worktrees)
    }

    /// Run `id` as [`Self::ended_run_worktrees`] would list it, or `None`
    /// when it would not: the cleanup job's check of a run it picked
    /// before it cleans it (task 1586), which reads that run alone by its
    /// key and its lease, not every run again.
    pub fn ended_run_worktree(&self, id: &RunId) -> Result<Option<EndedRunWorktree>> {
        let tx = self.read_entry("ended_run_worktree")?;
        let now = self.generators.clock.now();
        let lease = SqlRead::query_row(
            &tx,
            "SELECT run_id,token,pid,heartbeat_at FROM run_leases WHERE run_id=?1",
            sql_params![id],
            lease_row_port,
        )
        .optional()?;
        if lease.is_some_and(|lease| !lease_is_stale(&lease, now)) {
            return Ok(None);
        }
        Ok(SqlRead::query_row(
            &tx,
            &ended_run_worktree_sql(),
            sql_params![now, HEARTBEAT_TIMEOUT_SECS, id],
            |row| self.ended_run_worktree_row(row),
        )
        .optional()?)
    }

    /// A row of [`ENDED_RUN_WORKTREES`].
    fn ended_run_worktree_row(&self, row: &SqlRow<'_>) -> SqlResult<EndedRunWorktree> {
        let run_id: RunId = row.get(0)?;
        let ended: bool = row.get(5)?;
        let ask: Option<i64> = row.get(6)?;
        Ok(EndedRunWorktree {
            worktree: RunPaths::new(&self.runs_dir, &run_id)
                .with_layout_of(row.get::<_, Option<String>>(7)?.as_deref())
                .worktree
                .to_string_lossy()
                .into_owned(),
            run_id,
            task_id: row.get(1)?,
            status: enum_col(row, "run_status")?,
            task_status: enum_col(row, "task_status")?,
            branch: row.get(4)?,
            cleanup: match (ended, ask) {
                (true, _) => WorktreeCleanup::Ended,
                (false, Some(ask)) => WorktreeCleanup::AwaitingAnswer(AskId::new(ask)),
                (false, None) => WorktreeCleanup::Idle,
            },
        })
    }

    /// The runs whose lease is not stale ([`lease_is_stale`]): a live
    /// supervisor still holds them. The sweep leaves the stale leases of
    /// ended runs in place rather than deleting them: the heartbeat thread
    /// renews every 2 seconds, so a stale lease's holder is dead or hung,
    /// and all it may still do to an ended run is release the lease, which
    /// would fail if the row were gone.
    fn live_leased_runs(&self) -> Result<std::collections::HashSet<RunId>> {
        let tx = self.read_entry("live_leased_runs")?;
        let now = self.generators.clock.now();
        let statement = "SELECT run_id,token,pid,heartbeat_at FROM run_leases";
        let leases = SqlRead::query_map(&tx, statement, sql_params![], lease_row_port)?;
        let mut live = std::collections::HashSet::new();
        for lease in leases {
            if !lease_is_stale(&lease, now) {
                live.insert(lease.run_id);
            }
        }
        Ok(live)
    }
}

/// The runs of [`SqliteQueue::ended_run_worktrees`] but for their live
/// leases, which the caller leaves out; `?1` is now and `?2`
/// [`HEARTBEAT_TIMEOUT_SECS`].
const ENDED_RUN_WORKTREES: &str =
    "SELECT r.id, r.task_id, r.status AS run_status, t.status AS task_status, r.branch,
            (r.status IN ('integrated','succeeded','failed','interrupted')
             OR t.status IN ('completed','canceled')) AS ended,
            (SELECT min(a.id) FROM asks a
             WHERE a.run_id=r.id AND a.answered_at IS NULL AND a.closed_at IS NULL) AS ask_id,
            r.worktree_path
     FROM task_runs r
     JOIN tasks t ON t.id=r.task_id
     WHERE r.worktree_path IS NOT NULL
     AND (((r.status IN ('integrated','succeeded','failed','interrupted')
            OR t.status IN ('completed','canceled'))
           AND (r.status IN ('integrated','succeeded','failed','interrupted')
                OR NOT EXISTS (SELECT 1 FROM run_leases l WHERE l.run_id=r.id)))
          OR (r.status IN ('awaiting_integration','needs_session')
              AND t.status NOT IN ('completed','canceled')
              AND NOT EXISTS (SELECT 1 FROM run_leases l WHERE l.run_id=r.id)
              AND NOT EXISTS (SELECT 1 FROM run_processes p WHERE p.run_id=r.id
                              AND p.exited_at IS NULL AND p.heartbeat_at >= ?1-?2)))";

/// [`ENDED_RUN_WORKTREES`] for the run `?3` alone, by its key.
fn ended_run_worktree_sql() -> String {
    format!("{ENDED_RUN_WORKTREES} AND r.id=?3")
}

/// The [`InboxWatchLog`] port over the inherent methods above.
impl<E: TransactionEntries> InboxWatchLog for StoreQueue<E> {
    fn claim_inbox_nudge(&self, payload: serde_json::Value) -> Result<bool> {
        StoreQueue::claim_inbox_nudge(self, payload)
    }
    fn record_inbox_watcher_change(
        &self,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<bool> {
        StoreQueue::record_inbox_watcher_change(self, kind, payload)
    }
}

/// The [`RunReads`] port over the inherent methods above, which callers
/// that hold a `SqliteQueue` keep using directly.
impl<E: TransactionEntries> RunReads for StoreQueue<E> {
    fn last_turns_of(&self, sessions: &[RunSessionPair]) -> Result<Vec<LastSessionTurn>> {
        StoreQueue::last_turns_of(self, sessions)
    }

    fn active_runs(&self) -> Result<Vec<TaskRun>> {
        StoreQueue::active_runs(self)
    }
    fn all_runs(&self) -> Result<Vec<TaskRun>> {
        StoreQueue::all_runs(self)
    }
    fn run(&self, id: &RunId) -> Result<TaskRun> {
        StoreQueue::run(self, id)
    }
    fn runs_with_status(&self, status: RunStatus) -> Result<Vec<TaskRun>> {
        StoreQueue::runs_with_status(self, status)
    }
    fn next_awaiting_integration(&self) -> Result<Option<TaskRun>> {
        StoreQueue::next_awaiting_integration(self)
    }
    fn ended_run_workspaces(&self) -> Result<Vec<EndedRunWorkspace>> {
        StoreQueue::ended_run_workspaces(self)
    }
    fn ended_run_worktrees(&self) -> Result<Vec<EndedRunWorktree>> {
        StoreQueue::ended_run_worktrees(self)
    }
    fn ended_run_worktree(&self, id: &RunId) -> Result<Option<EndedRunWorktree>> {
        StoreQueue::ended_run_worktree(self, id)
    }
    fn latest_runs_in_progress(&self) -> Result<Vec<TaskRun>> {
        StoreQueue::latest_runs_in_progress(self)
    }
    fn runs_with_pending_push(&self) -> Result<Vec<TaskRun>> {
        StoreQueue::runs_with_pending_push(self)
    }
    fn run_in_workspace(&self, workspace_id: &str) -> Result<Option<RunId>> {
        StoreQueue::run_in_workspace(self, workspace_id)
    }
}

/// The [`EventStore`] port over the inherent methods above, in the same
/// `run_events` table and transactions as the state the other ports
/// change (ADR-t1662-2 D1).
impl<E: TransactionEntries> EventStore for StoreQueue<E> {
    fn audit_privileged_operation(&self, operation: &str) -> Result<()> {
        anyhow::ensure!(
            matches!(operation, "down_force" | "guardrail_disable"),
            "unknown privileged lifecycle operation"
        );
        crate::application::commands::DenialLog::record_privileged(
            self,
            serde_json::json!({"use_case": operation, "operation": operation, "capability": "service.lifecycle", "reason": "allowed"}),
        )
    }

    fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        StoreQueue::update_events(self, limit)
    }
    fn installs_after(&self, after: EventId, limit: usize) -> Result<(Vec<RunEvent>, usize)> {
        StoreQueue::installs_after(self, after, limit)
    }
    fn e2e_gate_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        StoreQueue::e2e_gate_events(self, limit)
    }
    fn all_events(&self) -> Result<Vec<RunEvent>> {
        StoreQueue::all_events(self)
    }
    fn earliest_event_at(&self) -> Result<Option<i64>> {
        StoreQueue::earliest_event_at(self)
    }
    fn stats_window(&self) -> Result<crate::domain::stats::StatsWindow> {
        StoreQueue::stats_window(self)
    }
    fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
        StoreQueue::latest_task_events(self, kinds)
    }
    fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
        StoreQueue::run_events(self, id)
    }
    fn task_events(&self, task: TaskId) -> Result<Vec<RunEvent>> {
        StoreQueue::task_events(self, task)
    }
    fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool> {
        StoreQueue::has_run_event(self, id, kind)
    }
    fn record_runtime_event(
        &self,
        id: &RunId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<()> {
        StoreQueue::record_runtime_event(self, id, kind, payload)
    }
    fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
        StoreQueue::last_observe(self, mode)
    }
    fn latest_event_id(&self) -> Result<EventId> {
        StoreQueue::latest_event_id(self)
    }
    fn record_backend_failure(
        &self,
        run: Option<&RunId>,
        payload: serde_json::Value,
    ) -> Result<()> {
        StoreQueue::record_backend_failure(self, run, payload)
    }
    fn record_queue_event(&self, kind: EventKind, payload: serde_json::Value) -> Result<EventId> {
        StoreQueue::record_queue_event(self, kind, payload)
    }
    fn record_queue_events(
        &self,
        events: Vec<(EventKind, serde_json::Value)>,
    ) -> Result<Vec<EventId>> {
        StoreQueue::record_queue_events(self, &events)
    }
    fn record_event_once(
        &self,
        run: Option<&RunId>,
        kind: EventKind,
        payload: serde_json::Value,
        record: &str,
    ) -> Result<EventId> {
        StoreQueue::record_event_once(self, run, kind, payload, record)
    }
    fn record_attributes_baseline(&self) -> Result<Option<(usize, usize)>> {
        StoreQueue::record_attributes_baseline(self)
    }
    fn record_findings_baseline(&self) -> Result<Option<usize>> {
        StoreQueue::record_findings_baseline(self)
    }
    fn latest_event_of(&self, kind: &str) -> Result<Option<RunEvent>> {
        StoreQueue::latest_event_of(self, kind)
    }
    fn latest_events_of(&self, kind: &str, limit: usize) -> Result<Vec<RunEvent>> {
        StoreQueue::latest_events_of(self, kind, limit)
    }
    fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>> {
        StoreQueue::latest_queue_event(self, kinds)
    }
    fn record_queue_event_if_newest(
        &self,
        kinds: &[&str],
        newest: EventId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<Option<EventId>> {
        StoreQueue::record_queue_event_if_newest(self, kinds, newest, kind, payload)
    }
    fn expire_disk_relax(&self, relaxed: EventId) -> Result<Option<EventId>> {
        StoreQueue::expire_disk_relax(self, relaxed)
    }
    fn latest_events_by_supervisor(
        &self,
        kinds: &[&str],
        supervisors: &[&str],
    ) -> Result<Vec<RunEvent>> {
        StoreQueue::latest_events_by_supervisor(self, kinds, supervisors)
    }
    fn events_of_between(
        &self,
        kinds: &[&str],
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        StoreQueue::events_of_between(self, kinds, after, upto, limit)
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
    fn events_between(
        &self,
        after: EventId,
        upto: EventId,
        filter: &EventFilter,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        StoreQueue::events_between(self, after, upto, filter, limit)
    }
}

/// The one option of the rulesets' hold ask, and the runtime's answer when
/// it closes the ask as the hold ends.
const RULESETS_ASK_ANSWER: &str = "done";

/// The subject of the rulesets' hold ask: it tells that ask from any other
/// `blocked` ask about no task or run, which the hold neither reuses nor
/// closes.
const RULESETS_ASK_SUBJECT: &str = "runner_rulesets";

/// [`StoreQueue::record_runner_rulesets`]'s writes in the open transaction
/// `tx` at `at` (Unix seconds); whether it wrote any.
pub(super) fn runner_rulesets_in(
    tx: &impl crate::infrastructure::sql_exec::SqlWrite,
    now: std::result::Result<(), &crate::domain::ruleset_gate::Unverified>,
    at: i64,
) -> Result<bool> {
    use crate::domain::ruleset_gate::{HOLD_KINDS, ask_question, transition};
    let mut last: Option<RunEvent> = None;
    for kind in HOLD_KINDS {
        let event = SqlRead::query_row(
            tx,
            LATEST_QUEUE_EVENT_SQL,
            sql_params![kind.as_str()],
            event_row,
        )
        .optional()?;
        if let Some(event) = event
            && last.as_ref().is_none_or(|last| last.id < event.id)
        {
            last = Some(event);
        }
    }
    let Some(change) = transition(now, last.as_ref()) else {
        return Ok(false);
    };
    use crate::infrastructure::asks::{close_by_runtime, insert_subject_ask, read_ask};
    let mut payload = change.payload;
    // The ask of the hold in progress, which each of its records names.
    let held_ask = last
        .as_ref()
        .filter(|event| event.kind == EventKind::RunnerRulesetsHeld)
        .and_then(|event| event.payload["ask_id"].as_i64())
        .map(crate::domain::AskId::new);
    match now {
        Err(reason) if change.open_ask => {
            let ask = crate::domain::NewAsk {
                kind: crate::domain::AskKind::Blocked,
                task_id: None,
                run_id: None,
                question: ask_question(reason),
                options: vec![RULESETS_ASK_ANSWER.into()],
                asked_by: "supervisor".into(),
                reason_category: crate::domain::AskReason::RecoveryFailed,
                topics: vec![],
                recommendation: None,
                confidence: None,
                finding_id: None,
                request_id: None,
            };
            ask.validate()?;
            // Each hold has its own ask: an open one of the hold's subject
            // left from before is closed by the runtime, not reused. The
            // open asks' unique index keeps at most one of the subject.
            let mut opened = insert_subject_ask(tx, &ask, RULESETS_ASK_SUBJECT)?;
            if !opened.created {
                close_by_runtime(tx, &opened.ask, RULESETS_ASK_ANSWER, at)?;
                opened = insert_subject_ask(tx, &ask, RULESETS_ASK_SUBJECT)?;
            }
            payload["ask_id"] = serde_json::json!(opened.ask.id);
        }
        Err(_) => payload["ask_id"] = serde_json::json!(held_ask),
        Ok(()) => {
            // The hold ended by itself: its ask, answered or not, is closed
            // so that a later hold opens a new one.
            if let Some(id) = held_ask {
                let ask = read_ask(tx, id)?;
                if ask.closed_at.is_none() {
                    close_by_runtime(tx, &ask, RULESETS_ASK_ANSWER, at)?;
                }
                payload["ask_id"] = serde_json::json!(id);
            }
        }
    }
    queue_event(tx, change.kind, &payload)?;
    Ok(true)
}

/// Insert an event of the queue itself in the open transaction `tx`, with
/// the session span it opens or closes; its id.
pub(super) fn queue_event(
    tx: &impl crate::infrastructure::sql_exec::SqlWrite,
    kind: EventKind,
    payload: &Value,
) -> Result<EventId> {
    let id = EventId::new(SqlWrite::insert_id(
        tx,
        "INSERT INTO run_events(kind,payload,actor_role,actor_id,requested_by)
         VALUES (?1,dagq_event_body(?2),dagq_actor_role(),dagq_actor_id(),dagq_requested_by())",
        sql_params![kind.as_str(), serde_json::to_string(payload)?],
    )?);
    // The observer's session span (ADR-0048).
    crate::infrastructure::sessions::follow(tx, id, None, None, kind, payload)?;
    Ok(id)
}

impl<E: TransactionEntries> crate::application::ObserverLog for StoreQueue<E> {
    fn event_id_before(&self, unix: i64) -> Result<EventId> {
        StoreQueue::event_id_before(self, unix)
    }
    fn ask_high_water(&self) -> Result<AskId> {
        StoreQueue::ask_high_water(self)
    }
    fn written_by(&self, role: &str, event_id: EventId, ask_id: AskId) -> Result<WrittenBy> {
        StoreQueue::written_by(self, role, event_id, ask_id)
    }
    fn last_observation(&self, mode: &str) -> Result<Option<(EventId, Value)>> {
        StoreQueue::last_observation(self, mode)
    }
    fn events_besides(&self, role: &str, span_kind: &str, after: EventId) -> Result<i64> {
        StoreQueue::events_besides(self, role, span_kind, after)
    }
    fn observations(&self, limit: usize) -> Result<Vec<(RunEvent, Option<RunEvent>)>> {
        StoreQueue::observations(self, limit)
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::infrastructure::sql_dialect::json_field_spaced;

    /// A queue at a fixed time: the hold's records read the queue's clock.
    fn rulesets_queue(name: &str) -> SqliteQueue {
        struct Still;
        impl crate::application::Clock for Still {
            fn system_time(&self) -> std::time::SystemTime {
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_900_000_000)
            }
            fn monotonic(&self) -> std::time::Instant {
                static AT: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
                *AT.get_or_init(std::time::Instant::now)
            }
        }
        let uri = format!("file:/{name}-{}?vfs=memdb", uuid::Uuid::new_v4());
        SqliteQueue::init(&uri)
            .unwrap()
            .with_generators(crate::application::Generators {
                clock: std::sync::Arc::new(Still),
                ..crate::infrastructure::clock::system()
            })
    }

    /// The runner rulesets' hold: its read, ask and event are one
    /// transaction, so a write that does not commit leaves no ask, and a
    /// retry, or a hold that goes on after its ask was answered, opens none
    /// again; a new hold after a resume opens a new one.
    #[test]
    fn the_rulesets_hold_and_its_ask_commit_together_and_ask_once() {
        use crate::domain::ruleset_gate::Unverified;
        let queue = rulesets_queue("rulesets");
        let count = |sql: &str| -> i64 { queue.conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        let asks = || count("SELECT count(*) FROM asks WHERE kind='blocked'");
        let held = || count("SELECT count(*) FROM run_events WHERE kind='runner_rulesets_held'");
        let unreadable = Unverified::Unreadable;
        // A transaction that wrote but did not commit leaves nothing.
        let tx = queue.write_entry("test").unwrap();
        assert!(runner_rulesets_in(&tx, Err(&unreadable), 1).unwrap());
        drop(tx);
        assert_eq!((asks(), held()), (0, 0));
        // The retry opens the ask once, with the event naming it.
        queue.record_runner_rulesets(Err(&unreadable)).unwrap();
        queue.record_runner_rulesets(Err(&unreadable)).unwrap();
        assert_eq!((asks(), held()), (1, 1));
        let ask_id = count("SELECT id FROM asks WHERE kind='blocked'");
        let event = queue
            .latest_queue_event(&["runner_rulesets_held"])
            .unwrap()
            .unwrap();
        assert_eq!(event.payload["ask_id"], ask_id);
        assert_eq!(event.payload["reason"], "ruleset_unreadable");
        // Answered and closed while the hold goes on: no second ask, even
        // when the reason changes.
        queue
            .conn
            .execute("UPDATE asks SET closed_at=1", [])
            .unwrap();
        queue.record_runner_rulesets(Err(&unreadable)).unwrap();
        queue
            .record_runner_rulesets(Err(&Unverified::PlanUnavailable))
            .unwrap();
        assert_eq!((asks(), held()), (1, 2));
        queue.record_runner_rulesets(Ok(())).unwrap();
        assert_eq!(
            count("SELECT count(*) FROM run_events WHERE kind='runner_rulesets_resumed'"),
            1
        );
        queue.record_runner_rulesets(Err(&unreadable)).unwrap();
        assert_eq!(asks(), 2);
    }

    /// An unanswered hold that resumes by itself closes its ask, so a later
    /// hold for another reason opens a new ask (with its own question and
    /// `ask_opened`) instead of reusing the old one; an open ask of the same
    /// key left from before is closed, not reused.
    #[test]
    fn each_hold_has_its_own_ask_even_when_the_last_was_never_answered() {
        use crate::domain::ruleset_gate::Unverified;
        let queue = rulesets_queue("rulesets-each");
        let open_asks = || -> Vec<(i64, String)> {
            let mut statement = queue
                .conn
                .prepare("SELECT id, question FROM asks WHERE kind='blocked' AND closed_at IS NULL")
                .unwrap();
            statement
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        let count = |sql: &str| -> i64 { queue.conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        queue
            .record_runner_rulesets(Err(&Unverified::Unreadable))
            .unwrap();
        let first = open_asks();
        assert_eq!(first.len(), 1);
        assert!(first[0].1.contains("ruleset_unreadable"));
        // Unanswered, the hold ends by itself: the runtime closes its ask.
        queue.record_runner_rulesets(Ok(())).unwrap();
        assert!(open_asks().is_empty());
        let answered = count(&format!(
            "SELECT count(*) FROM asks WHERE id={} AND answered_by IS NOT NULL AND closed_at IS NOT NULL",
            first[0].0
        ));
        assert_eq!(answered, 1);
        let resumed = queue
            .latest_queue_event(&["runner_rulesets_resumed"])
            .unwrap()
            .unwrap();
        assert_eq!(resumed.payload["ask_id"], first[0].0);
        // Held again for another reason: a new ask with that reason.
        queue
            .record_runner_rulesets(Err(&Unverified::PlanUnavailable))
            .unwrap();
        let second = open_asks();
        assert_eq!(second.len(), 1);
        assert_ne!(second[0].0, first[0].0);
        assert!(second[0].1.contains("ruleset_plan_unavailable"));
        assert_eq!(
            count("SELECT count(*) FROM run_events WHERE kind='ask_opened'"),
            2
        );
        // A change of reason within the hold names the same ask.
        queue
            .record_runner_rulesets(Err(&Unverified::Stale))
            .unwrap();
        let changed = queue
            .latest_queue_event(&["runner_rulesets_held"])
            .unwrap()
            .unwrap();
        assert_eq!(changed.payload["ask_id"], second[0].0);
        assert_eq!(open_asks(), second);
        // An open ask of the same key that no hold names (a resume that
        // never ran) is closed and replaced by the next hold's own.
        queue.record_runner_rulesets(Ok(())).unwrap();
        queue
            .conn
            .execute(
                "UPDATE asks SET closed_at=NULL, answered_at=NULL, answered_by=NULL, answer=NULL WHERE id=(SELECT max(id) FROM asks)",
                [],
            )
            .unwrap();
        queue
            .record_runner_rulesets(Err(&Unverified::Unreadable))
            .unwrap();
        let third = open_asks();
        assert_eq!(third.len(), 1);
        assert!(third[0].0 > second[0].0);
    }

    /// Another open `blocked` ask about no task or run, of the same reason,
    /// is not the hold's: a hold that begins opens its own ask beside it,
    /// and neither the hold nor its resume answers or closes the other.
    #[test]
    fn a_hold_leaves_an_unrelated_blocked_ask_open() {
        use crate::domain::ruleset_gate::Unverified;
        let mut queue = rulesets_queue("rulesets-unrelated");
        let other = queue
            .ask(crate::domain::NewAsk {
                kind: crate::domain::AskKind::Blocked,
                task_id: None,
                run_id: None,
                question: "Is the deploy key rotated?".into(),
                options: vec![],
                asked_by: "inbox".into(),
                reason_category: crate::domain::AskReason::RecoveryFailed,
                topics: vec![],
                recommendation: None,
                confidence: None,
                finding_id: None,
                request_id: None,
            })
            .unwrap()
            .ask;
        let untouched = |queue: &SqliteQueue| {
            let row: (Option<i64>, Option<i64>, Option<String>) = queue
                .conn
                .query_row(
                    "SELECT answered_at, closed_at, answer FROM asks WHERE id=?1",
                    [other.id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!(row, (None, None, None), "the other ask stays open");
        };
        queue
            .record_runner_rulesets(Err(&Unverified::Unreadable))
            .unwrap();
        untouched(&queue);
        let held = queue
            .latest_queue_event(&["runner_rulesets_held"])
            .unwrap()
            .unwrap();
        let hold_ask = held.payload["ask_id"].as_i64().unwrap();
        assert_ne!(hold_ask, other.id.as_i64());
        let subject: Option<String> = queue
            .conn
            .query_row("SELECT subject FROM asks WHERE id=?1", [hold_ask], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(subject.as_deref(), Some(RULESETS_ASK_SUBJECT));
        queue.record_runner_rulesets(Ok(())).unwrap();
        untouched(&queue);
        let closed: Option<i64> = queue
            .conn
            .query_row("SELECT closed_at FROM asks WHERE id=?1", [hold_ask], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(closed.is_some(), "the hold's own ask is closed on resume");
        // A later hold opens a new ask of its own, still beside the other.
        queue
            .record_runner_rulesets(Err(&Unverified::PlanUnavailable))
            .unwrap();
        untouched(&queue);
        let again = queue
            .latest_queue_event(&["runner_rulesets_held"])
            .unwrap()
            .unwrap();
        assert!(again.payload["ask_id"].as_i64().unwrap() > hold_ask);
    }

    /// The other order: a blocked ask about no task, run, finding or request
    /// registered while the hold's ask is open is a new ask with its own
    /// question, not the hold's, and the hold's resume closes only its own.
    #[test]
    fn a_blocked_ask_after_the_hold_is_its_own_and_outlives_the_resume() {
        use crate::domain::ruleset_gate::Unverified;
        let mut queue = rulesets_queue("rulesets-hold-first");
        queue
            .record_runner_rulesets(Err(&Unverified::Unreadable))
            .unwrap();
        let hold_ask = queue
            .latest_queue_event(&["runner_rulesets_held"])
            .unwrap()
            .unwrap()
            .payload["ask_id"]
            .as_i64()
            .unwrap();
        let other = queue
            .ask(crate::domain::NewAsk {
                kind: crate::domain::AskKind::Blocked,
                task_id: None,
                run_id: None,
                question: "Is the deploy key rotated?".into(),
                options: vec![],
                asked_by: "inbox".into(),
                reason_category: crate::domain::AskReason::RecoveryFailed,
                topics: vec![],
                recommendation: None,
                confidence: None,
                finding_id: None,
                request_id: None,
            })
            .unwrap();
        assert!(other.created, "a new ask, not the hold's");
        assert_ne!(other.ask.id.as_i64(), hold_ask);
        assert_eq!(other.ask.question, "Is the deploy key rotated?");
        let state = |queue: &SqliteQueue, id: i64| {
            queue
                .conn
                .query_row(
                    "SELECT subject, closed_at FROM asks WHERE id=?1",
                    [id],
                    |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<i64>>(1)?)),
                )
                .unwrap()
        };
        assert_eq!(state(&queue, other.ask.id.as_i64()), (None, None));
        queue.record_runner_rulesets(Ok(())).unwrap();
        assert_eq!(
            state(&queue, other.ask.id.as_i64()),
            (None, None),
            "the resume leaves the other ask open"
        );
        let (subject, closed) = state(&queue, hold_ask);
        assert_eq!(subject.as_deref(), Some(RULESETS_ASK_SUBJECT));
        assert!(closed.is_some(), "the hold's own ask is closed on resume");
    }

    /// Equal timestamps and reverse-sorted text IDs must keep insertion
    /// order, including the latest-run predicates used by recovery.
    #[test]
    fn equal_time_runs_and_supervisors_keep_insertion_order() {
        let uri = format!("file:/run-order-{}?vfs=memdb", uuid::Uuid::new_v4());
        let queue = SqliteQueue::init(&uri).unwrap();
        queue
            .conn
            .pragma_update(None, "foreign_keys", false)
            .unwrap();
        queue.conn.execute("INSERT INTO tasks(id,title,description,acceptance,verification_commands,status,created_at,updated_at) VALUES (1,'t','','','[]','in_progress','2026-11-07T00:00:00.000Z','2026-11-07T00:00:00.000Z')", []).unwrap();
        let ids = [
            "ffffffff-ffff-4fff-8fff-ffffffffffff",
            "11111111-1111-4111-8111-111111111111",
        ];
        for id in ids {
            queue.conn.execute("INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,created_at,workspace_id) VALUES (?1,1,'failed','codex','codex','0000000000000000000000000000000000000000','2026-11-07T00:00:00.000Z','shared')", [id]).unwrap();
        }
        let ids_of = |runs: Vec<TaskRun>| {
            runs.into_iter()
                .map(|run| run.id().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids_of(queue.all_runs().unwrap()), ids);
        assert_eq!(
            ids_of(queue.runs_with_status(RunStatus::Failed).unwrap()),
            ids
        );
        assert_eq!(ids_of(queue.latest_runs_in_progress().unwrap()), [ids[1]]);
        assert_eq!(
            queue.run_in_workspace("shared").unwrap().unwrap().as_str(),
            ids[1]
        );
        let conn = &*queue.conn;
        assert_eq!(
            crate::infrastructure::task_holds::latest_run(conn, TaskId::new(1))
                .unwrap()
                .unwrap()
                .0
                .as_str(),
            ids[1]
        );
        for token in ["z", "a"] {
            queue.conn.execute("INSERT INTO supervisors(token,pid,parallel,binary_version,started_at,heartbeat_at,handoff_accepted,auto_update) VALUES (?1,1,1,'v',10,10,2,2)", [token]).unwrap();
        }
        let tokens: Vec<_> = queue
            .supervisors()
            .unwrap()
            .into_iter()
            .map(|s| {
                assert!(!s.handoff_accepted);
                assert!(!s.auto_update);
                s.token.into_string()
            })
            .collect();
        assert_eq!(tokens, ["z", "a"]);
        // SQLite's historical nonzero build flag remains declared.
        queue
            .conn
            .execute("UPDATE tasks SET status='ready', wait_for_build=2", [])
            .unwrap();
        assert!(
            crate::application::TaskReads::build_waits(&queue)
                .unwrap()
                .contains_key(&TaskId::new(1))
        );
        assert!(
            crate::infrastructure::store_rows::read_task(&*queue.conn, TaskId::new(1))
                .unwrap()
                .wait_for_build()
        );
    }

    struct CountReads<'a> {
        conn: &'a Connection,
        reads: std::cell::Cell<usize>,
    }

    impl SqlRead for CountReads<'_> {
        fn dialect(&self) -> SqlDialect {
            SqlRead::dialect(self.conn)
        }
        fn visit_rows(
            &self,
            sql: &str,
            params: &[crate::infrastructure::sql_exec::SqlValue],
            visit: &mut dyn FnMut(&SqlRow<'_>) -> SqlResult<bool>,
        ) -> SqlResult<()> {
            self.reads.set(self.reads.get() + 1);
            SqlRead::visit_rows(self.conn, sql, params, visit)
        }
    }

    /// Insert a synthetic run's log, without requiring lifecycle setup.
    /// This tests the read adapter on init's real schema and indexes.
    fn append_turn_event(queue: &SqliteQueue, run: &str, kind: &str, payload: Value) {
        queue
            .conn
            .execute(
                "INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (1,?1,?2,?3)",
                rusqlite::params![run, kind, payload.to_string()],
            )
            .unwrap();
    }

    fn pair(run: &str, handle: &str) -> RunSessionPair {
        RunSessionPair {
            run_id: RunId::new(run).unwrap(),
            workspace_id: handle.into(),
        }
    }

    #[test]
    fn last_turn_pages_match_the_event_oracle_and_bound_start_width() {
        use crate::domain::background_wrapper::{BackgroundHandle, last_turn_of};
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        queue
            .conn
            .pragma_update(None, "foreign_keys", false)
            .unwrap();
        let mut pairs = Vec::new();
        for (n, payload) in [
            serde_json::json!({"pid": 2, "start": "turn"}),
            serde_json::json!({"pid": u32::MAX, "start": "\u{1}".repeat(128)}),
            serde_json::json!({"start": "turn"}),
            serde_json::json!({"pid": 2}),
            serde_json::json!({"pid": 4294967296_u64, "start": "turn"}),
            serde_json::json!({"pid": -1, "start": "turn"}),
            serde_json::json!({"pid": "2", "start": "turn"}),
            serde_json::json!({"pid": 2.0, "start": "turn"}),
            serde_json::json!({"pid": 2, "start": 3}),
            serde_json::json!({"pid": 2, "start": "x".repeat(129)}),
            serde_json::json!({"pid": 2, "start": "é".repeat(65)}),
        ]
        .into_iter()
        .enumerate()
        {
            let run = format!("run{n}");
            let p = pair(&run, "background:1:start");
            append_turn_event(
                &queue,
                &run,
                "wrapper_launched",
                serde_json::json!({"workspace_id": p.workspace_id}),
            );
            append_turn_event(
                &queue,
                &run,
                "turn_started",
                serde_json::json!({"pid": 3, "start": "earlier"}),
            );
            append_turn_event(&queue, &run, "turn_started", payload);
            pairs.push(p);
        }
        // Repeated launches of the same handle select its last launch;
        // another wrapper and workspace_created both cut its interval.
        for (kind, payload) in [
            ("wrapper_launched", serde_json::json!({"workspace_id":"a"})),
            ("turn_started", serde_json::json!({"pid":10,"start":"old"})),
            ("wrapper_launched", serde_json::json!({"workspace_id":"a"})),
            ("turn_started", serde_json::json!({"pid":11,"start":"a"})),
            ("wrapper_launched", serde_json::json!({"workspace_id":"b"})),
            ("turn_started", serde_json::json!({"pid":12,"start":"b"})),
            ("workspace_created", serde_json::json!({"workspace_id":"c"})),
            (
                "turn_started",
                serde_json::json!({"pid":13,"start":"outside"}),
            ),
            (
                "wrapper_launched",
                serde_json::json!({"workspace_id":"empty"}),
            ),
        ] {
            append_turn_event(&queue, "multi", kind, payload);
        }
        pairs.extend([
            pair("multi", "a"),
            pair("multi", "b"),
            pair("multi", "empty"),
            pair("multi", "missing"),
        ]);
        let rows = queue.last_turns_of(&pairs).unwrap();
        for p in &pairs {
            let events = queue.run_events(&p.run_id).unwrap();
            let oracle = last_turn_of(&events, &p.workspace_id);
            let found = rows
                .iter()
                .find(|r| r.run_id == p.run_id && r.workspace_id == p.workspace_id)
                .map(|r| BackgroundHandle {
                    pid: r.pid,
                    start: r.start.clone(),
                });
            if oracle.as_ref().is_some_and(|turn| turn.start.len() > 128) {
                assert!(found.is_none());
                // The real OS start token can never equal an oversized
                // recorded start; both paths classify it as not alive.
                let current_start = Some("OS-start");
                assert!(!oracle.as_ref().unwrap().is(2, current_start));
                assert!(!found.as_ref().is_some_and(|turn| turn.is(2, current_start)));
            } else {
                assert_eq!(found, oracle, "{p:?}");
            }
        }
        assert_eq!(rows.len(), 4);
        assert!(
            queue
                .last_turns_of(&[pair(&"r".repeat(257), "a")])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn last_turn_seeks_use_events_by_run_without_statistics() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let stats: i64 = queue
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='sqlite_stat1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stats, 0);
        let steps = plan(&queue.conn, &last_turns_sql(SqlDialect::Sqlite));
        assert_eq!(
            steps
                .iter()
                .filter(|step| step.contains("events_by_run"))
                .count(),
            3,
            "{steps:#?}"
        );
        assert!(
            !steps.iter().any(|step| step.contains("SCAN run_events")),
            "{steps:#?}"
        );
        // Each event subquery must walk a run/id range, not sort all turns
        // or materialize every event of a run.
        assert!(
            !steps.iter().any(|step| step.contains("TEMP B-TREE")),
            "{steps:#?}"
        );
    }

    #[test]
    fn one_page_returns_one_row_per_pair_even_when_history_exceeds_the_wire_limit() {
        use crate::domain::queue_service::MAX_ANSWER_BYTES;
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        queue
            .conn
            .pragma_update(None, "foreign_keys", false)
            .unwrap();
        queue.conn.execute_batch("BEGIN").unwrap();
        append_turn_event(
            &queue,
            "huge",
            "wrapper_launched",
            serde_json::json!({"workspace_id":"huge"}),
        );
        let noise = "x".repeat(MAX_ANSWER_BYTES / 64 + 1);
        for pid in 1..=65 {
            append_turn_event(
                &queue,
                "huge",
                "turn_started",
                serde_json::json!({"pid":pid,"start":"turn","noise":noise}),
            );
            append_turn_event(
                &queue,
                "multi",
                "wrapper_launched",
                serde_json::json!({"workspace_id":"multi"}),
            );
            append_turn_event(
                &queue,
                "multi",
                "turn_started",
                serde_json::json!({"pid":pid,"start":"turn"}),
            );
        }
        queue.conn.execute_batch("COMMIT").unwrap();
        // One call, one SQL statement; no event paging or per-run reads.
        let reader = CountReads {
            conn: &queue.conn,
            reads: std::cell::Cell::new(0),
        };
        let rows =
            read_last_turns(&reader, &[pair("huge", "huge"), pair("multi", "multi")]).unwrap();
        assert_eq!(reader.reads.get(), 1);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.pid == 65));
        assert!(serde_json::to_vec(&rows).unwrap().len() < MAX_ANSWER_BYTES);
    }

    /// The plan of `sql`, one line per step, its parameters NULL.
    fn plan(conn: &Connection, sql: &str) -> Vec<String> {
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let nulls = vec![rusqlite::types::Null; statement.parameter_count()];
        statement
            .query_map(rusqlite::params_from_iter(nulls), |r| {
                r.get::<_, String>("detail")
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// The range of one kind is read by `events_by_kind`, not a walk of
    /// every event. In memory, on the table's columns and the index as the
    /// migrations leave them: the plan needs no queue file.
    #[test]
    fn a_range_of_one_kind_is_read_by_its_index() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE run_events (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, task_id INTEGER, goal_id INTEGER,
                 run_id TEXT, kind TEXT NOT NULL, payload TEXT NOT NULL, created_at TEXT NOT NULL);
             CREATE INDEX events_by_kind ON run_events(kind, id);",
        )
        .unwrap();
        let steps = plan(&conn, EVENTS_OF_KIND_BETWEEN_SQL);
        assert!(
            steps
                .iter()
                .any(|line| line.starts_with("SEARCH run_events USING INDEX events_by_kind")),
            "{steps:#?}"
        );
        assert!(
            !steps.iter().any(|line| line.starts_with("SCAN")),
            "{steps:#?}"
        );
    }

    /// The cleanup job's check of one run (task 1586) finds the run by its
    /// key and its task by its own, and walks no table.
    #[test]
    fn the_check_of_one_run_searches_it_by_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let steps = plan(&queue.conn, &ended_run_worktree_sql());
        let has = |step: &str| steps.iter().any(|line| line.starts_with(step));
        assert!(
            has("SEARCH r USING INDEX sqlite_autoindex_task_runs_1 (id=?)"),
            "{steps:#?}"
        );
        assert!(
            has("SEARCH t USING INTEGER PRIMARY KEY (rowid=?)"),
            "{steps:#?}"
        );
        assert!(!has("SCAN"), "{steps:#?}");
        let lease = plan(
            &queue.conn,
            "SELECT run_id,token,pid,heartbeat_at FROM run_leases WHERE run_id=?1",
        );
        assert!(
            lease.iter().all(|line| line.starts_with("SEARCH")),
            "{lease:#?}"
        );
    }

    /// A person's newest `inbox_opened` and the host's (no parent) are
    /// found however many opens of another person come after them.
    #[test]
    fn an_inbox_s_newest_open_is_found_past_others_opens() {
        use crate::domain::EventKind;
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let open = |payload: serde_json::Value| {
            queue
                .record_queue_event(EventKind::InboxOpened, payload)
                .unwrap()
        };
        let person = serde_json::json!({"issuer": "https://idp", "subject": "a"});
        let other = serde_json::json!({"issuer": "https://idp", "subject": "b"});
        open(serde_json::json!({"provider": "codex", "parent": person}));
        let own = open(serde_json::json!({"provider": "claude", "parent": person}));
        let host = open(serde_json::json!({"provider": "claude"}));
        for _ in 0..600 {
            open(serde_json::json!({"provider": "claude", "parent": other}));
        }
        let id = |parent| queue.latest_inbox_opened(parent).unwrap().map(|e| e.id);
        assert_eq!(id(Some(("https://idp", "a"))), Some(own));
        assert_eq!(id(None), Some(host));
        assert_eq!(id(Some(("https://idp", "c"))), None);
        // The guardrail's runs are read by their person as well: another
        // person's later run is not this person's, nor the host's.
        let reached = |payload| {
            queue
                .record_queue_event(EventKind::InboxGuardrailReached, payload)
                .unwrap()
        };
        let host_run = reached(serde_json::json!({"session_id": "h"}));
        reached(serde_json::json!({"session_id": "b", "parent": other}));
        let run = |parent| {
            queue
                .latest_inbox_event(EventKind::InboxGuardrailReached, parent)
                .unwrap()
                .map(|e| e.id)
        };
        assert_eq!(run(Some(("https://idp", "a"))), None);
        assert_eq!(run(None), Some(host_run));
        let own_run = reached(serde_json::json!({"session_id": "a", "parent": person}));
        assert_eq!(run(Some(("https://idp", "a"))), Some(own_run));
    }

    /// Each supervisor named gets its newest record of the kinds, however
    /// many the others wrote after it; one with none, a kind not asked for
    /// and a supervisor not named are left out.
    #[test]
    fn each_supervisor_s_latest_is_read_past_the_others_records() {
        use crate::domain::EventKind;
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let record = |kind: EventKind, supervisor: &str| {
            queue
                .record_queue_event(kind, serde_json::json!({"supervisor": supervisor}))
                .unwrap()
        };
        record(EventKind::CiWatchHeld, "a");
        let a = record(EventKind::CiWatchResumed, "a");
        let mut b = record(EventKind::CiWatchHeld, "b");
        for _ in 0..100 {
            record(EventKind::CiWatchHeld, "c");
            b = record(EventKind::CiWatchResumed, "b");
        }
        record(EventKind::LandingBranchUnresolved, "a");
        let kinds = [
            EventKind::CiWatchHeld.as_str(),
            EventKind::CiWatchResumed.as_str(),
        ];
        let latest = queue
            .latest_events_by_supervisor(&kinds, &["a", "b", "none"])
            .unwrap();
        let ids: Vec<_> = latest.iter().map(|event| event.id).collect();
        assert_eq!(ids, [a, b]);
        assert_eq!(latest[0].kind, EventKind::CiWatchResumed);
        assert!(
            queue
                .latest_events_by_supervisor(&kinds, &[])
                .unwrap()
                .is_empty()
        );
        // The records of the kinds are found by their index, not by a walk
        // of every event.
        let steps = plan(
            &queue.conn,
            &format!(
                "SELECT MAX(id) FROM run_events WHERE kind IN (?1, ?2)
               AND {} IN (?3)
             GROUP BY {}",
                json_field_spaced("payload", "$.supervisor", JsonType::Text),
                json_field_spaced("payload", "$.supervisor", JsonType::Text),
            ),
        );
        assert!(
            !steps.iter().any(|line| line == "SCAN run_events"),
            "{steps:#?}"
        );
    }

    #[test]
    fn installs_after_includes_missing_null_and_false_plugin_only() {
        use crate::domain::EventKind;
        let uri = format!("file:/installs-{}?vfs=memdb", uuid::Uuid::new_v4());
        let queue = SqliteQueue::init(&uri).unwrap();
        let mut expected = Vec::new();
        for payload in [
            serde_json::json!({}),
            serde_json::json!({"plugin_only": null}),
            serde_json::json!({"plugin_only": false}),
        ] {
            expected.push(
                queue
                    .record_queue_event(EventKind::UpdateInstalled, payload)
                    .unwrap(),
            );
        }
        queue
            .record_queue_event(
                EventKind::UpdateInstalled,
                serde_json::json!({"plugin_only": true}),
            )
            .unwrap();
        let (found, total) = queue.installs_after(EventId::new(0), 10).unwrap();
        assert_eq!(found.iter().map(|e| e.id).collect::<Vec<_>>(), expected);
        assert_eq!(total, 3);
    }

    /// The installs past a run's claim are read past any number of the
    /// other `update_*` steps: not one before the claim, of the plugin
    /// only, of another kind or of a task, the newest `limit` oldest first
    /// and all of them counted (task 1831). The read walks no table.
    #[test]
    fn the_installs_past_a_claim_are_read_past_the_other_update_steps_and_counted() {
        use crate::domain::EventKind;
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let record = |kind: EventKind, payload: serde_json::Value| {
            queue.record_queue_event(kind, payload).unwrap()
        };
        let before = record(
            EventKind::UpdateInstalled,
            serde_json::json!({"version": "old"}),
        );
        let claim = record(EventKind::UpdateStarted, serde_json::json!({}));
        assert!(before < claim);
        let mut installs = Vec::new();
        for n in 0..300 {
            record(EventKind::UpdateStarted, serde_json::json!({}));
            record(EventKind::UpdateBuilt, serde_json::json!({}));
            record(
                EventKind::CiWatchHeld,
                serde_json::json!({"supervisor": "a"}),
            );
            if n < 3 {
                installs.push(record(
                    EventKind::UpdateInstalled,
                    serde_json::json!({"version": format!("v{n}")}),
                ));
                record(
                    EventKind::UpdateInstalled,
                    serde_json::json!({"plugin_only": true}),
                );
            }
        }
        // The steps after them push the installs out of the newest update
        // steps.
        assert!(
            queue
                .update_events(200)
                .unwrap()
                .iter()
                .all(|e| e.kind != EventKind::UpdateInstalled)
        );
        let ids = |events: &[RunEvent]| events.iter().map(|e| e.id).collect::<Vec<_>>();
        let (read, total) = queue.installs_after(claim, 1_000).unwrap();
        assert_eq!((ids(&read), total), (installs.clone(), 3));
        let (read, total) = queue.installs_after(claim, 2).unwrap();
        assert_eq!((ids(&read), total), (installs[1..].to_vec(), 3));
        let (read, total) = queue.installs_after(EventId::new(0), 1_000).unwrap();
        assert_eq!(read.first().map(|e| e.id), Some(before));
        assert_eq!(total, 4);
        for sql in [
            format!("SELECT COUNT(*) {}", installs_after_sql(SqlDialect::Sqlite)),
            format!(
                "SELECT * {} ORDER BY id DESC LIMIT ?3",
                installs_after_sql(SqlDialect::Sqlite)
            ),
        ] {
            let steps = plan(&queue.conn, &sql);
            assert!(
                !steps.iter().any(|line| line == "SCAN run_events"),
                "{steps:#?}"
            );
        }
    }
}
