//! Durable per-run supervisor ownership and one-shot wrapper registration.
//!
//! One module per port of the run store: `transitions` and `recovery`
//! save the run aggregate as it moves, `coordination` and
//! `session_registry` hold the state processes coordinate through,
//! `run_log` reads runs and events, and `queue_records` serves reports.
//! The helpers they share stay here.
use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::{Value, json};

use super::{
    adapters::process_alive,
    sessions::{Closing, read_before},
    sqlite::{
        SqliteQueue, claim_task, enum_col, event, event_row, json_col, read_task, run_row,
        stored_run_row,
    },
};
use crate::application::{
    AskStore, Generators, QueueRecords, RunCoordination, RunLog, RunRecovery, RunTransitions,
    SessionRegistry, timestamp, unix_seconds,
};
use crate::domain::slot_limits::{SettingSource, SlotLimits};
use crate::domain::worker_model::{self, WorkerTrial};
use crate::domain::{
    AskId, ClaimOutcome, CommitSha, DomainError, EventFilter, EventId, GoalId, PlannerId,
    PlannerOrigin, PlannerSession, ProposalId, Reason, ReasonCode, RunEvent, RunHistory, RunId,
    RunLease, RunPaths, RunProcess, RunStatus, SessionRole, SupervisorMode, SupervisorRegistration,
    Task, TaskAction, TaskChange, TaskId, TaskRun, event_kind,
    forecast::snapshot::FORECAST_RECORDED,
    kpi::report::REPORT_WRITTEN,
    related::RelatedPage,
    resume::{self, ResumeCount},
    run,
    search::{SearchPage, SearchQuery},
};
use crate::domain::{EventKind, LeaseToken};
use crate::domain::{provider_switch::WorkerRoute, worker::Worker};

pub use crate::application::{
    EndedRunWorkspace, EndedRunWorktree, Exhaustion, Landing, LeasedRun, ResumeCandidate,
    TRIAGE_ASKER, TriageAction, Validation,
};
pub use crate::domain::{HEARTBEAT_TIMEOUT_SECS, RunPlan};

/// What one role wrote in a window ([`SqliteQueue::written_by`]): finding
/// ids it recorded, updated and closed (resolved or dismissed), and its
/// asks' ids.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WrittenBy {
    pub recorded: Vec<i64>,
    pub updated: Vec<i64>,
    pub closed: Vec<i64>,
    pub asks: Vec<i64>,
}

/// Whether a lease no longer has a working process behind it: its pid is
/// dead or its heartbeat is older than `HEARTBEAT_TIMEOUT_SECS`. The rule
/// `status` / `doctor` report as `stale` and the one adoption re-checks.
pub fn lease_is_stale(lease: &RunLease, now: i64) -> bool {
    !process_alive(lease.pid) || now - lease.heartbeat_at > HEARTBEAT_TIMEOUT_SECS
}

mod ask_store;
mod coordination;
mod queue_records;
mod recovery;
mod run_log;
mod session_registry;
mod transitions;

/// The run as stored (its paths not relocated), for a command to start from.
fn stored_run(conn: &Connection, id: &RunId) -> Result<Option<TaskRun>> {
    Ok(conn
        .query_row("SELECT * FROM task_runs WHERE id=?1", [id], stored_run_row)
        .optional()?)
}

/// The run as stored if `token` is its supervisor.
fn supervised_run(conn: &Connection, id: &RunId, token: &LeaseToken) -> Result<Option<TaskRun>> {
    Ok(conn
        .query_row(
            "SELECT * FROM task_runs WHERE id=?1 AND supervisor_token=?2",
            params![id, token],
            stored_run_row,
        )
        .optional()?)
}

/// Save what a domain command made of a run read with [`stored_run`].
/// `status=from` (the status the command started from) and, when given,
/// `supervisor_token=token` only detect a concurrent change: the domain
/// decided the move. Whether a row was updated.
fn save_run(
    conn: &Connection,
    run: &TaskRun,
    from: RunStatus,
    token: Option<&LeaseToken>,
) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE task_runs SET status=?3,repo_path=?4,run_dir=?5,branch=?6,worktree_path=?7,
         receipt_path=?8,log_path=?9,workspace_id=?10,result_commit=?11,last_error=?12,
         workspace_closed_at=?13
         WHERE id=?1 AND status=?2 AND (?14 IS NULL OR supervisor_token=?14)",
        params![
            run.id(),
            from.as_str(),
            run.status().as_str(),
            run.repo_path(),
            run.run_dir(),
            run.branch(),
            run.worktree_path(),
            run.receipt_path(),
            run.log_path(),
            run.workspace_id(),
            run.result_commit(),
            run.last_error(),
            run.workspace_closed_at(),
            token,
        ],
    )? == 1)
}

/// Read the run, apply the domain `command` and save the result, inside
/// the caller's transaction; the stored (not relocated) run comes back.
/// A missing run, a command the domain refuses and a row that changed
/// meanwhile all fail with `refusal`, the message the store has always
/// given for the operation. The reason of a domain refusal is not part of
/// that message; it is kept in `refusals` instead.
fn apply(
    conn: &Connection,
    refusals: Refusals<'_>,
    id: &RunId,
    token: Option<&LeaseToken>,
    refusal: impl Fn() -> String,
    command: impl FnOnce(TaskRun) -> Result<TaskRun, DomainError>,
) -> Result<TaskRun> {
    let run = stored_run(conn, id)?.ok_or_else(|| anyhow!(refusal()))?;
    let from = run.status();
    let run = command(run).map_err(|reason| {
        let message = refusal();
        refusals.record(id, &message, &reason);
        anyhow!(message)
    })?;
    ensure!(save_run(conn, &run, from, token)?, refusal());
    Ok(run)
}

/// [`apply`] for a transition that returns the events it records
/// ([`run::Recorded`]): the run is saved and the events written in the
/// caller's transaction, in order.
fn apply_recorded(
    conn: &Connection,
    refusals: Refusals<'_>,
    id: &RunId,
    token: Option<&LeaseToken>,
    refusal: impl Fn() -> String,
    command: impl FnOnce(TaskRun) -> Result<run::Recorded, DomainError>,
) -> Result<TaskRun> {
    let mut events = Vec::new();
    let run = apply(conn, refusals, id, token, refusal, |run| {
        let (run, recorded) = command(run)?;
        events = recorded;
        Ok(run)
    })?;
    record_events(conn, id, events)?;
    Ok(run)
}

/// Write the events a transition returned for run `id`.
fn record_events(conn: &Connection, id: &RunId, events: Vec<run::NewRunEvent>) -> Result<()> {
    for event in events {
        run_event(conn, id, event.kind, event.payload)?;
    }
    Ok(())
}

/// The answer the runtime writes into a `stalled` ask it closes as the run
/// is recovered.
pub const STALL_RECOVERED_CLOSED: &str = "the run was recovered; closed by the runtime";

/// The answer the runtime writes into a `stalled` ask it closes as the
/// supervisor abandons the run.
pub const STALL_ABANDONED_CLOSED: &str = "the supervisor gave up on the run; closed by the runtime";

/// The file in a run's directory that keeps why the domain refused a
/// transition of the run: one `[<unix seconds>] <message>: <DomainError>`
/// line per refusal, `<message>` being the error the operation returned.
pub const REFUSALS_LOG: &str = "refusals.log";

/// Where [`apply`] leaves the reason of a refused transition. The
/// operation's error keeps the message the store has always given, so the
/// CLI's `{"error"}` and `last_error` do not change; the domain's reason
/// (the status the run was in and the command) goes to [`REFUSALS_LOG`].
/// The caller's transaction rolls back after a refusal, which is why the
/// reason is not a run event.
#[derive(Clone, Copy)]
struct Refusals<'a> {
    runs_dir: &'a Path,
    generators: &'a Generators,
}

fn refusals<'a>(runs_dir: &'a Path, generators: &'a Generators) -> Refusals<'a> {
    Refusals {
        runs_dir,
        generators,
    }
}

impl Refusals<'_> {
    /// Append the line. A run directory that is gone or does not accept the
    /// write loses the line and not the operation's error.
    fn record(&self, id: &RunId, message: &str, reason: &DomainError) {
        let run_dir = RunPaths::new(self.runs_dir, id).run_dir;
        let line = format!("[{}] {message}: {reason}\n", self.generators.clock.now());
        if let Err(error) = super::agent_dir::append(&run_dir.join(REFUSALS_LOG), line.as_bytes())
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%id, %error, "could not append run refusal");
        }
    }
}

/// Why a workspace close of a run is not recorded.
fn not_at_rest() -> String {
    "run is not awaiting integration or a session with an open workspace under this supervisor"
        .to_owned()
}

/// Renew `token`'s lease on the run at `now`, inside the caller's
/// `BEGIN IMMEDIATE` transaction, or refuse when the lease row is gone or
/// carries another token (ADR-0039 decision 7). A heartbeat older than
/// `HEARTBEAT_TIMEOUT_SECS` is no reason to refuse: after a host sleep the
/// row is still this supervisor's until an adopter swaps the token or
/// `recover` removes it, and SQLite serializes that write with this one.
fn renew_lease(conn: &Connection, id: &RunId, token: &LeaseToken, now: i64) -> Result<()> {
    let renewed = conn.execute(
        "UPDATE run_leases SET heartbeat_at=?3 WHERE run_id=?1 AND token=?2",
        params![id, token, now],
    )?;
    ensure!(
        renewed == 1,
        "run lease is missing or held by another supervisor"
    );
    Ok(())
}

fn lease_row(r: &Row<'_>) -> rusqlite::Result<RunLease> {
    Ok(RunLease {
        run_id: r.get(0)?,
        token: r.get(1)?,
        pid: r.get(2)?,
        heartbeat_at: r.get(3)?,
    })
}

/// The lease of run `id` on `conn`, if any.
pub(super) fn run_lease_of(conn: &Connection, id: &RunId) -> Result<Option<RunLease>> {
    Ok(conn
        .query_row(
            "SELECT run_id,token,pid,heartbeat_at FROM run_leases WHERE run_id=?1",
            [id],
            lease_row,
        )
        .optional()?)
}

/// Every registered supervisor on `conn`, oldest registration first.
pub(super) fn supervisors_of(conn: &Connection) -> Result<Vec<SupervisorRegistration>> {
    Ok(conn
        .prepare("SELECT * FROM supervisors ORDER BY started_at, rowid")?
        .query_map([], supervisor_row)?
        .collect::<rusqlite::Result<_>>()?)
}

fn supervisor_row(r: &Row<'_>) -> rusqlite::Result<SupervisorRegistration> {
    Ok(SupervisorRegistration {
        token: r.get("token")?,
        pid: r.get("pid")?,
        parallel: r.get("parallel")?,
        started_at: r.get("started_at")?,
        heartbeat_at: r.get("heartbeat_at")?,
        mode: r
            .get::<_, Option<String>>("mode")?
            .map(|_| enum_col(r, "mode"))
            .transpose()?,
        workspace_id: r.get("workspace_id")?,
        binary_version: r.get("binary_version")?,
        handoff_accepted: r.get::<_, Option<i64>>("handoff_accepted")? == Some(1),
        handoff_binary: r.get("handoff_binary")?,
        auto_update: r.get::<_, Option<i64>>("auto_update")? == Some(1),
        max_waiting: r.get("max_waiting")?,
        parallel_source: source(r, "parallel_source")?,
        max_waiting_source: source(r, "max_waiting_source")?,
        runtime_planners: r.get("runtime_planners")?,
        runtime_planners_source: source(r, "runtime_planners_source")?,
        // Unreadable JSON (another binary's shape) reads as none.
        providers: r
            .get::<_, Option<String>>("providers")?
            .and_then(|json| serde_json::from_str(&json).ok()),
    })
}

/// A `*_source` column of `supervisors` (task 698); an unknown text reads
/// as none.
fn source(r: &Row<'_>, column: &str) -> rusqlite::Result<Option<SettingSource>> {
    Ok(r.get::<_, Option<String>>(column)?
        .as_deref()
        .and_then(SettingSource::parse))
}

fn assert_wrapper(conn: &Connection, id: &RunId, pid: u32) -> Result<()> {
    let valid: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM run_processes WHERE run_id=?1 AND role='wrapper' AND pid=?2 AND exited_at IS NULL)",
        params![id,pid], |r| r.get(0))?;
    ensure!(valid, "wrapper is not the live owner of this run");
    Ok(())
}

fn run_event(
    conn: &Connection,
    id: &RunId,
    kind: EventKind,
    payload: serde_json::Value,
) -> Result<()> {
    let task_id: TaskId =
        conn.query_row("SELECT task_id FROM task_runs WHERE id=?1", [id], |r| {
            r.get(0)
        })?;
    event(conn, task_id, Some(id), kind, payload)
}

/// End the run's stalled detections inside the caller's transaction, the
/// run being taken out of its session by no watch of its own (`recover`,
/// the supervisor's abandon, the sweep; ADR-0047 decisions 30 and 32):
/// its `stalled` asks nobody closed are closed with `answer` (an open one
/// answered by the runtime, `runtime_closed: true`), and each detection
/// with no end recorded gets its `stall_resolved` of outcome `run_ended`,
/// once: one already ended by the watch or an earlier close gets none.
pub(super) fn end_stalled_detections(
    tx: &Connection,
    id: &RunId,
    answer: &str,
    now: i64,
) -> Result<Vec<crate::domain::Ask>> {
    // Events are stamped by SQLite's clock, so their ages are too.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    // The answers of its `stalled` asks tell a `wait` from a person
    // stepping in; the events carry only the option answered.
    let answers: std::collections::HashMap<i64, String> = tx
        .prepare(
            "SELECT id, answer FROM asks WHERE run_id=?1 AND kind='stalled'
             AND answer IS NOT NULL",
        )?
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let ends = crate::domain::stats::thresholds::run_ended_resolutions(
        &run_events_of(tx, id)?,
        &|ask| answers.get(&ask).cloned(),
        now_ms,
    );
    let closed = crate::infrastructure::asks::close_asks_in(
        tx,
        id,
        None,
        crate::domain::AskKind::Stalled,
        answer,
        now,
    )?;
    for end in ends {
        run_event(tx, id, EventKind::StallResolved, end)?;
    }
    Ok(closed)
}

/// Lease a `needs_session` run to `token` for a resume or its skip, inside
/// the caller's transaction: no lease but a stale one (which is replaced)
/// and no session still heartbeating; with `clear_processes` (a resumed
/// session registers in their place) the previous session's process rows
/// are cleared (their history stays in the run's events), and `token`
/// becomes its supervisor. `Ok(Some(previous lease token))` once leased,
/// `Ok(None)` when the run is not free to take.
fn lease_parked_run(
    tx: &Connection,
    id: &RunId,
    token: &LeaseToken,
    now: i64,
    clear_processes: bool,
) -> Result<Option<Option<LeaseToken>>> {
    let parked: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM task_runs WHERE id=?1 AND status='needs_session')",
        [id],
        |r| r.get(0),
    )?;
    if !parked {
        return Ok(None);
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
    // A session still heartbeating is left alone.
    let live: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_processes WHERE run_id=?1
         AND exited_at IS NULL AND heartbeat_at >= ?2-?3)",
        params![id, now, HEARTBEAT_TIMEOUT_SECS],
        |r| r.get(0),
    )?;
    if live {
        return Ok(None);
    }
    if clear_processes {
        tx.execute("DELETE FROM run_processes WHERE run_id=?1", [id])?;
    }
    tx.execute(
        "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,?2,?3,?4)",
        params![id, token, std::process::id(), now],
    )?;
    tx.execute(
        "UPDATE task_runs SET supervisor_token=?2 WHERE id=?1",
        params![id, token],
    )?;
    Ok(Some(lease.map(|l| l.token)))
}

pub(super) fn run_events_of(conn: &Connection, id: &RunId) -> Result<Vec<RunEvent>> {
    Ok(conn
        .prepare("SELECT * FROM run_events WHERE run_id=?1 ORDER BY id")?
        .query_map([id], event_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// The run's resumes, counted as ADR-0047 decision 24 says.
fn resume_count(conn: &Connection, id: &RunId) -> Result<ResumeCount> {
    Ok(ResumeCount::of(&run_events_of(conn, id)?))
}

fn process_row(r: &Row<'_>) -> rusqlite::Result<RunProcess> {
    Ok(RunProcess {
        run_id: r.get("run_id")?,
        role: r.get("role")?,
        pid: r.get("pid")?,
        heartbeat_at: r.get("heartbeat_at")?,
        exited_at: r.get("exited_at")?,
        exit_code: r.get("exit_code")?,
    })
}

pub(super) fn processes_for_task(conn: &Connection, task_id: TaskId) -> Result<Vec<RunProcess>> {
    Ok(conn.prepare("SELECT p.* FROM run_processes p JOIN task_runs r ON r.id=p.run_id WHERE r.task_id=?1 ORDER BY r.rowid,p.role")?
        .query_map([task_id], process_row)?.collect::<rusqlite::Result<_>>()?)
}

/// Opens the queue at `db` with `generators` for each connection the
/// supervisor needs.
pub struct SqliteOpener {
    pub db: std::path::PathBuf,
    pub generators: crate::application::Generators,
    /// The actor the events of each connection record; `None` is the
    /// process's (ADR-t728-1 decision 4).
    pub actor: Option<crate::domain::actor::ActorContext>,
}

impl crate::application::QueueOpener for SqliteOpener {
    fn open(&self) -> Result<Box<dyn crate::application::Queue + Send>> {
        let mut queue = SqliteQueue::open(&self.db)?.with_generators(self.generators.clone());
        if let Some(actor) = &self.actor {
            queue = queue.with_actor(actor.clone());
        }
        Ok(Box::new(queue))
    }
}
