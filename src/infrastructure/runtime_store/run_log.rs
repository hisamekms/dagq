//! Runs and events as read, and events recorded outside a transition
//! ([`RunReads`], [`EventStore`]), and the records of the inbox's watcher
//! ([`InboxWatchLog`], host運用's).
//!
//! The reads of the queue's own events by kind (no run, task or goal) name
//! `events_by_kind`, why: [`crate::infrastructure::kpi_push`].

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
const EVENTS_OF_KIND_BETWEEN_SQL: &str =
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
         AND {} IS ?2
         AND {} IS ?3)",
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

impl SqliteQueue {
    /// See [`RunReads::last_turns_of`]. Malformed last turns stay absent;
    /// parsing never retries an earlier turn. Width is bounded in SQL,
    /// before a start can enter an adapter row or a service answer.
    pub fn last_turns_of(&self, sessions: &[RunSessionPair]) -> Result<Vec<LastSessionTurn>> {
        read_last_turns(&*self.conn, sessions)
    }

    /// The latest `limit` steps of the automatic update, newest first: the
    /// queue's `update_*` events (ADR-0073 decision 17), each job's
    /// `update_started` before its steps ([`crate::domain::in_job_order`]).
    pub fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        Ok(crate::domain::in_job_order(SqlRead::query_map(
            &*self.conn,
            &update_events_sql(SqlRead::dialect(&*self.conn)),
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
        let tx = self.read_tx("installs_after")?;
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
        tx.finish()?;
        installs.reverse();
        Ok((installs, usize::try_from(total).unwrap_or(usize::MAX)))
    }

    /// The latest `limit` events of the e2e gates, newest first (see
    /// [`crate::application::EventStore::e2e_gate_events`]).
    pub fn e2e_gate_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        use crate::domain::event_kind::{RUN_E2E_FAILED, RUN_E2E_FINISHED};
        let kinds = [
            crate::domain::UPDATE_E2E_PASSED,
            crate::domain::UPDATE_FAILED,
            RUN_E2E_FINISHED,
            RUN_E2E_FAILED,
            crate::domain::INSTALL_E2E_FINISHED,
        ];
        Ok(SqlRead::query_map(
            &*self.conn,
            &format!(
                "SELECT * FROM run_events
                 WHERE kind IN (SELECT {} FROM {})
                 ORDER BY id DESC LIMIT ?2",
                SqlRead::dialect(&*self.conn).json_element("value", JsonType::Text),
                SqlRead::dialect(&*self.conn).json_elements("?1"),
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
        Ok(SqlRead::query_row(
            &*self.conn,
            HAS_RUN_EVENT_SQL,
            sql_params![id, kind],
            |r| r.get(0),
        )?)
    }

    /// Every run of the queue, oldest first.
    pub fn all_runs(&self) -> Result<Vec<TaskRun>> {
        Ok(SqlRead::query_map(
            &*self.conn,
            "SELECT * FROM task_runs ORDER BY rowid",
            sql_params![],
            run_row(&self.runs_dir),
        )?)
    }

    pub fn run(&self, id: &RunId) -> Result<TaskRun> {
        SqlRead::query_row(
            &*self.conn,
            "SELECT * FROM task_runs WHERE id=?1",
            sql_params![id],
            run_row(&self.runs_dir),
        )
        .optional()?
        .ok_or_else(|| crate::application::RunNotFound(id.clone()).into())
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
        let _read = read_before_port(&*self.conn, Closing::Event(id, kind, &payload))?;
        let tx = self.write_tx("record_runtime_event")?;
        run_event_port(&tx, id, kind, payload)?;
        tx.commit()?;
        Ok(())
    }

    /// [`Self::record_runtime_event`] once `check`, run in the same write
    /// transaction before the event, answers `true`: `false` records
    /// nothing, and a check that fails records nothing and fails. Whether
    /// it recorded.
    pub fn record_runtime_event_if(
        &self,
        id: &RunId,
        kind: EventKind,
        payload: serde_json::Value,
        check: &mut dyn FnMut() -> Result<bool>,
    ) -> Result<bool> {
        let _read = read_before_port(&*self.conn, Closing::Event(id, kind, &payload))?;
        let tx = self.write_tx("record_runtime_event_if")?;
        if !check()? {
            return Ok(false);
        }
        run_event_port(&tx, id, kind, payload)?;
        tx.commit()?;
        Ok(true)
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
                let tx = self.write_tx("record_backend_failure")?;
                run_event_port(&tx, id, EventKind::BackendCallFailed, payload)?;
                tx.commit()?;
                Ok(())
            }
            None => self
                .record_queue_event(EventKind::BackendCallFailed, payload)
                .map(drop),
        }
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
        let _read = read_before_port(&*self.conn, Closing::Queue(kind, &payload))?;
        let tx = self.write_tx("record_queue_event")?;
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
            .map(|(kind, payload)| read_before_port(&*self.conn, Closing::Queue(*kind, payload)))
            .collect::<Result<Vec<_>>>()?;
        let tx = self.write_tx("record_queue_events")?;
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
        let _read = read_before_port(&*self.conn, closing)?;
        let tx = self.write_tx("record_event_once")?;
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

    /// Record `inbox_nudged` with `payload` unless one with the same
    /// `absent_since` and `attempt` is recorded, in one write transaction
    /// (ADR-t1433-5 decision 1 (3)): `false` when another supervisor
    /// recorded it first, and only the claimer nudges.
    pub fn claim_inbox_nudge(&self, payload: serde_json::Value) -> Result<bool> {
        let kind = EventKind::InboxNudged;
        crate::domain::check_event_target(kind, None, None)?;
        let _read = read_before_port(&*self.conn, Closing::Queue(kind, &payload))?;
        let tx = self.write_tx("claim_inbox_nudge")?;
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
        let _read = read_before_port(&*self.conn, Closing::Queue(kind, &payload))?;
        let tx = self.write_tx("record_inbox_watcher_change")?;
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
        let mut kinds = kinds.to_vec();
        kinds.sort_unstable();
        kinds.dedup();
        let mut events = Vec::new();
        for kind in kinds {
            events.extend(SqlRead::query_map(
                &*self.conn,
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
        Ok(SqlRead::query_row(
            &*self.conn,
            "SELECT * FROM run_events WHERE kind=?1 ORDER BY id DESC LIMIT 1",
            sql_params![kind],
            event_row,
        )
        .optional()?)
    }

    /// The newest `limit` events of `kind`, on whatever task, goal or run,
    /// newest first.
    pub fn latest_events_of(&self, kind: &str, limit: usize) -> Result<Vec<RunEvent>> {
        Ok(SqlRead::query_map(
            &*self.conn,
            "SELECT * FROM run_events WHERE kind=?1 ORDER BY id DESC LIMIT ?2",
            sql_params![kind, i64::try_from(limit).unwrap_or(i64::MAX)],
            event_row,
        )?)
    }

    /// The newest event of the queue itself (on no task, goal or run) of one
    /// of `kinds`.
    /// One lookup per kind, so each walks `events_by_kind` from its newest
    /// row: the supervisor reads these on every pass, and a kind list made
    /// SQLite walk every goal-less event instead.
    pub fn latest_queue_event(&self, kinds: &[&str]) -> Result<Option<RunEvent>> {
        let mut latest: Option<RunEvent> = None;
        for kind in kinds {
            let event = SqlRead::query_row(
                &*self.conn,
                LATEST_QUEUE_EVENT_SQL,
                sql_params![kind],
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

    /// For each of `supervisors`, the newest event of one of `kinds` whose
    /// payload names it as its `supervisor`, in one query.
    pub fn latest_events_by_supervisor(
        &self,
        kinds: &[&str],
        supervisors: &[&str],
    ) -> Result<Vec<RunEvent>> {
        if kinds.is_empty() || supervisors.is_empty() {
            return Ok(Vec::new());
        }
        let kind_marks = vec!["?"; kinds.len()].join(",");
        let supervisor_marks = vec!["?"; supervisors.len()].join(",");
        Ok(SqlRead::query_map(
            &*self.conn,
            &format!(
                "SELECT * FROM run_events WHERE id IN (
                     SELECT MAX(id) FROM run_events
                     WHERE kind IN ({kind_marks})
                       AND {supervisor} IN ({supervisor_marks})
                     GROUP BY {supervisor})
                 ORDER BY id",
                supervisor = SqlRead::dialect(&*self.conn).json_field_spaced(
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
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; kinds.len()].join(",");
        Ok(SqlRead::query_map(
            &*self.conn,
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
        Ok(SqlRead::query_row(
            &*self.conn,
            &format!(
                "SELECT ifnull(max(id),0) FROM run_events
             WHERE {} < ?1",
                SqlRead::dialect(&*self.conn).time_value("created_at", TimeForm::EpochSeconds)
            ),
            sql_params![unix],
            |r| r.get(0),
        )?)
    }

    /// When the observer of `mode` last started or finished (unix seconds).
    pub fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
        Ok(SqlRead::query_row(
            &*self.conn,
            &format!(
                "SELECT max({}) FROM run_events
             WHERE kind IN (?2,?3)
               AND {}=?1",
                SqlRead::dialect(&*self.conn).time_value("created_at", TimeForm::EpochSeconds),
                SqlRead::dialect(&*self.conn).json_field("payload", "$.mode", JsonType::Text),
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
        Ok(SqlRead::query_row(
            &*self.conn,
            "SELECT ifnull(max(id),0) FROM asks",
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
        let findings = |kind: &str, to: &[&str]| -> Result<Vec<i64>> {
            Ok(SqlRead::query_map(
                &*self.conn,
                &format!(
                    "SELECT {} AS finding FROM run_events
                     WHERE id>?2 AND kind=?3 AND {}=?1
                       AND (CAST(?4 AS TEXT) IS NULL OR {}
                            IN (SELECT {to} FROM {tos}))
                     GROUP BY finding ORDER BY min(id)",
                    SqlRead::dialect(&*self.conn).json_field(
                        "payload",
                        "$.finding_id",
                        JsonType::Integer
                    ),
                    SqlRead::dialect(&*self.conn).json_field("payload", "$.by", JsonType::Text),
                    SqlRead::dialect(&*self.conn).json_field("payload", "$.to", JsonType::Text),
                    to = SqlRead::dialect(&*self.conn).json_element("value", JsonType::Text),
                    tos = SqlRead::dialect(&*self.conn).json_elements("?4"),
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
                &*self.conn,
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
                &*self.conn,
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
        Ok(SqlRead::query_row(
            &*self.conn,
            &format!(
                "SELECT id, payload FROM run_events
                 WHERE kind='{}' AND {}=?1
                   AND ifnull({},'')<>'skipped'
                 ORDER BY id DESC LIMIT 1",
                event_kind::OBSERVE_FINISHED,
                SqlRead::dialect(&*self.conn).json_field("payload", "$.mode", JsonType::Text),
                SqlRead::dialect(&*self.conn).json_field("payload", "$.outcome", JsonType::Text),
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
        let mut ignored = vec![
            event_kind::OBSERVE_STARTED,
            event_kind::OBSERVE_FINISHED,
            event_kind::TELEMETRY_TOKEN_ISSUED,
            event_kind::TELEMETRY_TOKEN_REVOKED,
        ];
        ignored.extend_from_slice(crate::domain::kpi::BOOKKEEPING_KINDS);
        let ignored = serde_json::to_string(&ignored)?;
        Ok(SqlRead::query_row(
            &*self.conn,
            &format!(
                "SELECT count(*) FROM run_events WHERE id>?3 AND NOT (
               kind IN (SELECT {ignored_kind} FROM {ignored_kinds})
               OR (kind IN ('{}','{}','{}')
                   AND {by} IS ?1)
               OR (kind='{}' AND {asked_by} IS ?1)
               OR (kind='{}' AND actor_role IS ?1)
               OR (kind IN ('{}','{}','{}')
                   AND {kind} IS ?2))",
                event_kind::FINDING_RECORDED,
                event_kind::FINDING_UPDATED,
                event_kind::FINDING_STATUS_CHANGED,
                event_kind::ASK_OPENED,
                event_kind::AUTHORIZATION_DENIED,
                event_kind::SESSION_OPENED,
                event_kind::SESSION_CLOSED,
                event_kind::SESSION_TURNS,
                ignored_kind = SqlRead::dialect(&*self.conn).json_element("value", JsonType::Text),
                ignored_kinds = SqlRead::dialect(&*self.conn).json_elements("?4"),
                by = SqlRead::dialect(&*self.conn).json_field("payload", "$.by", JsonType::Text),
                asked_by = SqlRead::dialect(&*self.conn).json_field(
                    "payload",
                    "$.asked_by",
                    JsonType::Text
                ),
                kind =
                    SqlRead::dialect(&*self.conn).json_field("payload", "$.kind", JsonType::Text),
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
        finished
            .into_iter()
            .map(|finished| {
                let started = match finished.payload["dir"].as_str() {
                    Some(dir) => SqlRead::query_row(
                        &*self.conn,
                        &format!(
                            "SELECT * FROM run_events WHERE kind='{}'
                               AND id<?1 AND {}=?2
                             ORDER BY id DESC LIMIT 1",
                            event_kind::OBSERVE_STARTED,
                            SqlRead::dialect(&*self.conn).json_field(
                                "payload",
                                "$.dir",
                                JsonType::Text
                            ),
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
        let current = SqlRead::query_row(
            &*self.conn,
            "SELECT id FROM task_runs WHERE workspace_id=?1 ORDER BY rowid DESC LIMIT 1",
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
            &*self.conn,
            &format!(
                "SELECT run_id FROM run_events WHERE kind='workspace_created'
                 AND run_id IS NOT NULL AND {}=?1
                 ORDER BY id DESC LIMIT 1",
                SqlRead::dialect(&*self.conn).json_field(
                    "payload",
                    "$.workspace_id",
                    JsonType::Text
                ),
            ),
            sql_params![workspace_id],
            |r| r.get(0),
        )
        .optional()?)
    }

    /// Runs a process is responsible for right now (executing under a
    /// supervisor, or being landed by `integrate`), oldest first.
    pub fn active_runs(&self) -> Result<Vec<TaskRun>> {
        Ok(SqlRead::query_map(
            &*self.conn,
            "SELECT * FROM task_runs WHERE status IN ('claimed','starting','running','validating','integrating') ORDER BY rowid",
            sql_params![],
            run_row(&self.runs_dir),
        )?)
    }

    /// The newest `run_events` id, 0 for an empty queue: the cursor that
    /// `status` hands out and `watch` starts from.
    pub fn latest_event_id(&self) -> Result<EventId> {
        Ok(SqlRead::query_row(
            &*self.conn,
            "SELECT COALESCE(MAX(id),0) FROM run_events",
            sql_params![],
            |r| r.get(0),
        )?)
    }

    /// Every run event, oldest first, for `stats`. A pure read.
    pub fn all_events(&self) -> Result<Vec<RunEvent>> {
        Ok(SqlRead::query_map(
            &*self.conn,
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
        let at = SqlRead::dialect(&*self.conn).time_value("created_at", TimeForm::Ordinal);
        let statement = format!(
            "SELECT created_at FROM run_events WHERE {at} IS NOT NULL
             ORDER BY {at}, id",
        );
        let mut found = None;
        SqlRead::visit_rows(&*self.conn, &statement, &[], &mut |row| {
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
        // Each run met: its first event and the first event that ended it.
        let mut ended: HashMap<RunId, (EventId, EventId)> = HashMap::new();
        let newest = "SELECT * FROM run_events WHERE run_id IS NOT NULL AND task_id IS NOT NULL
             AND id<=?1 ORDER BY id DESC";
        let of_run = "SELECT * FROM run_events WHERE run_id=?1 AND id<=?2 ORDER BY id";
        SqlRead::visit_rows(&*self.conn, newest, sql_params![upto], &mut |row| {
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
            SqlRead::visit_rows(&*self.conn, of_run, sql_params![run_id, upto], &mut |row| {
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
        SqlRead::visit_rows(&*self.conn, times, sql_params![after, upto], &mut |row| {
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
        let kinds = filter
            .kinds
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        Ok(SqlRead::query_map(
            &*self.conn,
            &format!(
                "SELECT * FROM run_events WHERE id>?1 AND id<=?2
                 AND (CAST(?3 AS TEXT) IS NULL OR kind IN (SELECT {kind} FROM {kinds}))
                 AND (CAST(?4 AS TEXT) IS NULL OR run_id=?4 OR {}=?4)
                 AND (CAST(?5 AS BIGINT) IS NULL OR task_id=?5)
                 AND (CAST(?6 AS BIGINT) IS NULL OR goal_id=?6
                      OR task_id IN (SELECT id FROM tasks WHERE goal_id=?6))
                 AND (CAST(?7 AS TEXT) IS NULL OR {}>={})
                 AND (CAST(?8 AS TEXT) IS NULL OR {}<{})
                 ORDER BY id LIMIT ?9",
                SqlRead::dialect(&*self.conn).json_field(
                    "payload",
                    "$.source_run_id",
                    JsonType::Text
                ),
                SqlRead::dialect(&*self.conn).time_value("created_at", TimeForm::Ordinal),
                SqlRead::dialect(&*self.conn).time_value("?7", TimeForm::Ordinal),
                SqlRead::dialect(&*self.conn).time_value("created_at", TimeForm::Ordinal),
                SqlRead::dialect(&*self.conn).time_value("?8", TimeForm::Ordinal),
                kind = SqlRead::dialect(&*self.conn).json_element("value", JsonType::Text),
                kinds = SqlRead::dialect(&*self.conn).json_elements("?3"),
            ),
            sql_params![
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
        )?)
    }

    /// Every event of one run, oldest first.
    pub fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
        run_events_of_port(&*self.conn, id)
    }

    /// Every event of one task (its runs' too), oldest first, by
    /// `events_by_task`.
    pub fn task_events(&self, task: TaskId) -> Result<Vec<RunEvent>> {
        Ok(SqlRead::query_map(
            &*self.conn,
            "SELECT * FROM run_events WHERE task_id=?1 ORDER BY id",
            sql_params![task],
            event_row,
        )?)
    }

    /// The latest run of every `in_progress` task, oldest first: the runs
    /// `status` judges for attention. An older run of a retried task is
    /// history, and a completed or canceled task needs nobody.
    pub fn latest_runs_in_progress(&self) -> Result<Vec<TaskRun>> {
        Ok(SqlRead::query_map(
            &*self.conn,
            "SELECT r.* FROM task_runs r JOIN tasks t ON t.id=r.task_id
                 WHERE t.status='in_progress'
                 AND r.rowid=(SELECT MAX(rowid) FROM task_runs WHERE task_id=r.task_id)
                 ORDER BY r.rowid",
            sql_params![],
            run_row(&self.runs_dir),
        )?)
    }

    /// The `integrated` runs whose push of `main` failed after the latest
    /// successful push (`push_finished`), oldest first. A later successful
    /// push carries every earlier landing, so it clears them all.
    pub fn runs_with_pending_push(&self) -> Result<Vec<TaskRun>> {
        Ok(SqlRead::query_map(
            &*self.conn,
            "SELECT r.* FROM task_runs r WHERE r.status='integrated'
                 AND r.id IN (SELECT run_id FROM run_events WHERE kind=?1
                   AND id>(SELECT COALESCE(MAX(id),0) FROM run_events WHERE kind=?2))
                 ORDER BY r.rowid",
            sql_params![event_kind::PUSH_FAILED, event_kind::PUSH_FINISHED],
            run_row(&self.runs_dir),
        )?)
    }

    /// Every run in one status, oldest first; `up` reports the runs that
    /// wait for a person or the supervisor (`awaiting_integration`, `needs_session`).
    pub fn runs_with_status(&self, status: crate::domain::RunStatus) -> Result<Vec<TaskRun>> {
        Ok(SqlRead::query_map(
            &*self.conn,
            "SELECT * FROM task_runs WHERE status=?1 ORDER BY rowid",
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
        Ok(SqlRead::query_row(
            &*self.conn,
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
                          r.rowid
                 LIMIT 1"
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
        let statement = format!(
            "WITH ended AS (
               SELECT r.id, r.status, r.workspace_id, r.rowid AS run_row FROM task_runs r
               JOIN tasks t ON t.id=r.task_id
               WHERE r.status IN ('integrated','succeeded','failed','interrupted')
               AND NOT (r.status IN ('failed','interrupted') AND t.status='in_progress'
                        AND r.rowid=(SELECT MAX(rowid) FROM task_runs WHERE task_id=r.task_id))
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
            workspace = SqlRead::dialect(&*self.conn).json_field(
                "e.payload",
                "$.workspace_id",
                JsonType::Text
            )
        );
        let rows = SqlRead::query_map(&*self.conn, &statement, sql_params![], |row| {
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
        let statement = format!("{ENDED_RUN_WORKTREES} ORDER BY r.rowid");
        let live = self.live_leased_runs()?;
        let now = self.generators.clock.now();
        let rows = SqlRead::query_map(
            &*self.conn,
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
        let now = self.generators.clock.now();
        let lease = SqlRead::query_row(
            &*self.conn,
            "SELECT run_id,token,pid,heartbeat_at FROM run_leases WHERE run_id=?1",
            sql_params![id],
            lease_row_port,
        )
        .optional()?;
        if lease.is_some_and(|lease| !lease_is_stale(&lease, now)) {
            return Ok(None);
        }
        Ok(SqlRead::query_row(
            &*self.conn,
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
        let now = self.generators.clock.now();
        let statement = "SELECT run_id,token,pid,heartbeat_at FROM run_leases";
        let leases = SqlRead::query_map(&*self.conn, statement, sql_params![], lease_row_port)?;
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
impl InboxWatchLog for SqliteQueue {
    fn claim_inbox_nudge(&self, payload: serde_json::Value) -> Result<bool> {
        SqliteQueue::claim_inbox_nudge(self, payload)
    }
    fn record_inbox_watcher_change(
        &self,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<bool> {
        SqliteQueue::record_inbox_watcher_change(self, kind, payload)
    }
}

/// The [`RunReads`] port over the inherent methods above, which callers
/// that hold a `SqliteQueue` keep using directly.
impl RunReads for SqliteQueue {
    fn last_turns_of(&self, sessions: &[RunSessionPair]) -> Result<Vec<LastSessionTurn>> {
        SqliteQueue::last_turns_of(self, sessions)
    }

    fn active_runs(&self) -> Result<Vec<TaskRun>> {
        SqliteQueue::active_runs(self)
    }
    fn all_runs(&self) -> Result<Vec<TaskRun>> {
        SqliteQueue::all_runs(self)
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
    fn ended_run_workspaces(&self) -> Result<Vec<EndedRunWorkspace>> {
        SqliteQueue::ended_run_workspaces(self)
    }
    fn ended_run_worktrees(&self) -> Result<Vec<EndedRunWorktree>> {
        SqliteQueue::ended_run_worktrees(self)
    }
    fn ended_run_worktree(&self, id: &RunId) -> Result<Option<EndedRunWorktree>> {
        SqliteQueue::ended_run_worktree(self, id)
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
}

/// The [`EventStore`] port over the inherent methods above, in the same
/// `run_events` table and transactions as the state the other ports
/// change (ADR-t1662-2 D1).
impl EventStore for SqliteQueue {
    fn update_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        SqliteQueue::update_events(self, limit)
    }
    fn installs_after(&self, after: EventId, limit: usize) -> Result<(Vec<RunEvent>, usize)> {
        SqliteQueue::installs_after(self, after, limit)
    }
    fn e2e_gate_events(&self, limit: usize) -> Result<Vec<RunEvent>> {
        SqliteQueue::e2e_gate_events(self, limit)
    }
    fn all_events(&self) -> Result<Vec<RunEvent>> {
        SqliteQueue::all_events(self)
    }
    fn earliest_event_at(&self) -> Result<Option<i64>> {
        SqliteQueue::earliest_event_at(self)
    }
    fn stats_window(&self) -> Result<crate::domain::stats::StatsWindow> {
        SqliteQueue::stats_window(self)
    }
    fn latest_task_events(&self, kinds: &[&str]) -> Result<Vec<RunEvent>> {
        SqliteQueue::latest_task_events(self, kinds)
    }
    fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
        SqliteQueue::run_events(self, id)
    }
    fn task_events(&self, task: TaskId) -> Result<Vec<RunEvent>> {
        SqliteQueue::task_events(self, task)
    }
    fn has_run_event(&self, id: &RunId, kind: &str) -> Result<bool> {
        SqliteQueue::has_run_event(self, id, kind)
    }
    fn record_runtime_event(
        &self,
        id: &RunId,
        kind: EventKind,
        payload: serde_json::Value,
    ) -> Result<()> {
        SqliteQueue::record_runtime_event(self, id, kind, payload)
    }
    fn last_observe(&self, mode: &str) -> Result<Option<i64>> {
        SqliteQueue::last_observe(self, mode)
    }
    fn latest_event_id(&self) -> Result<EventId> {
        SqliteQueue::latest_event_id(self)
    }
    fn record_backend_failure(
        &self,
        run: Option<&RunId>,
        payload: serde_json::Value,
    ) -> Result<()> {
        SqliteQueue::record_backend_failure(self, run, payload)
    }
    fn record_queue_event(&self, kind: EventKind, payload: serde_json::Value) -> Result<EventId> {
        SqliteQueue::record_queue_event(self, kind, payload)
    }
    fn record_queue_events(
        &self,
        events: Vec<(EventKind, serde_json::Value)>,
    ) -> Result<Vec<EventId>> {
        SqliteQueue::record_queue_events(self, &events)
    }
    fn record_event_once(
        &self,
        run: Option<&RunId>,
        kind: EventKind,
        payload: serde_json::Value,
        record: &str,
    ) -> Result<EventId> {
        SqliteQueue::record_event_once(self, run, kind, payload, record)
    }
    fn record_attributes_baseline(&self) -> Result<Option<(usize, usize)>> {
        SqliteQueue::record_attributes_baseline(self)
    }
    fn record_findings_baseline(&self) -> Result<Option<usize>> {
        SqliteQueue::record_findings_baseline(self)
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
    fn latest_events_by_supervisor(
        &self,
        kinds: &[&str],
        supervisors: &[&str],
    ) -> Result<Vec<RunEvent>> {
        SqliteQueue::latest_events_by_supervisor(self, kinds, supervisors)
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
    fn events_between(
        &self,
        after: EventId,
        upto: EventId,
        filter: &EventFilter,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        SqliteQueue::events_between(self, after, upto, filter, limit)
    }
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

impl crate::application::ObserverLog for SqliteQueue {
    fn event_id_before(&self, unix: i64) -> Result<EventId> {
        SqliteQueue::event_id_before(self, unix)
    }
    fn ask_high_water(&self) -> Result<AskId> {
        SqliteQueue::ask_high_water(self)
    }
    fn written_by(&self, role: &str, event_id: EventId, ask_id: AskId) -> Result<WrittenBy> {
        SqliteQueue::written_by(self, role, event_id, ask_id)
    }
    fn last_observation(&self, mode: &str) -> Result<Option<(EventId, Value)>> {
        SqliteQueue::last_observation(self, mode)
    }
    fn events_besides(&self, role: &str, span_kind: &str, after: EventId) -> Result<i64> {
        SqliteQueue::events_besides(self, role, span_kind, after)
    }
    fn observations(&self, limit: usize) -> Result<Vec<(RunEvent, Option<RunEvent>)>> {
        SqliteQueue::observations(self, limit)
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::infrastructure::sql_dialect::json_field_spaced;

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
