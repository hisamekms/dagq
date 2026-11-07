//! Durable per-run supervisor ownership and one-shot wrapper registration.
//!
//! One module per port of the run store: `transitions` and `recovery`
//! save the run aggregate as it moves, `coordination` and
//! `session_registry` hold the state processes coordinate through,
//! `run_log` reads runs and events, `queue_records` serves reports and
//! `planning_records` the lookups of the planning tables.
//! The helpers they share stay here.
//!
//! The traits of the execution port are not imported here: a module that
//! has not moved globs this one, and with [`sql_exec::SqlRead`] in scope its
//! rusqlite `query_row` on a `WriteTx` would resolve to the port's. A moved
//! module imports them itself.
use super::sql_exec::{self, OptionalRow, SqlResult, SqlRow, sql_params};
use super::store_rows::{enum_col, event_row, read_task, run_row, stored_run_row};
use super::store_writes::event;
use super::task_store::claim_task;
use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Value, json};

use super::{adapters::process_alive, sessions::Closing, sqlite::SqliteQueue};
use crate::application::{
    AskStore, EventStore, Generators, HeartbeatWrite, InboxWatchLog, PlanningRecords, QueueRecords,
    RunCoordination, RunReads, RunRecovery, RunTransitions, SessionRegistry, SupervisorRegistry,
    timestamp, unix_seconds,
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
    resume::{self, ResumeCount},
    run,
};
use crate::domain::{EventKind, LeaseToken, run_phase};
use crate::domain::{provider_switch::WorkerRoute, worker::Worker};

pub use crate::application::{
    EndedRunWorkspace, EndedRunWorktree, Exhaustion, Landing, LeasedRun, ResumeCandidate,
    TRIAGE_ASKER, TriageAction, Validation, WorktreeCleanup,
};
pub use crate::domain::{HEARTBEAT_TIMEOUT_SECS, RunPlan};
pub use session_steps::{RETENTION_BATCH_READS, RETENTION_CANDIDATE_READS};

pub use crate::application::WrittenBy;

/// Whether a lease no longer has a working process behind it: its pid is
/// dead or its heartbeat is older than `HEARTBEAT_TIMEOUT_SECS`. The rule
/// `status` / `doctor` report as `stale` and the one adoption re-checks.
pub fn lease_is_stale(lease: &RunLease, now: i64) -> bool {
    !process_alive(lease.pid) || now - lease.heartbeat_at > HEARTBEAT_TIMEOUT_SECS
}

mod ask_store;
mod attributes;
mod coordination;
#[cfg(any(test, feature = "store-contract"))]
pub(crate) use coordination::BACKEND_PARALLEL_SQL;
mod eval_rounds;
mod ledger;
mod node_samples;
mod planning_records;
#[cfg(test)]
mod queue_event_read_plan_tests;
mod queue_records;
mod recovery;
mod run_log;
mod run_resources;
mod session_registry;
mod session_steps;
mod transitions;

// The names the moved functions had beside their rusqlite wrappers: a module
// on another branch calls them, or takes the wrappers' helpers through this
// module's glob, whichever landed first (ADR-t3982-1).
pub(super) use self::{
    end_stalled_detections as end_stalled_detections_port, run_events_of as run_events_of_port,
    run_lease_of as run_lease_of_port, supervisors_of as supervisors_of_port,
};
#[allow(unused_imports)]
use self::{
    lease_row as lease_row_port, process_row as process_row_port, run_event as run_event_port,
    save_run as save_run_port, stored_run as stored_run_port,
    supervisor_row as supervisor_row_port,
};
#[allow(unused_imports)]
use super::sql_exec::sqlite::{native_error, port_tx, with_row};

/// The run as stored (its paths not relocated), for a command to start from.
fn stored_run(conn: &impl sql_exec::SqlRead, id: &RunId) -> Result<Option<TaskRun>> {
    Ok(conn
        .query_row(
            "SELECT * FROM task_runs WHERE id=?1",
            sql_params![id],
            stored_run_row,
        )
        .optional()?)
}

/// The run as stored if `token` is its supervisor.
fn supervised_run(
    conn: &impl sql_exec::SqlRead,
    id: &RunId,
    token: &LeaseToken,
) -> Result<Option<TaskRun>> {
    Ok(conn
        .query_row(
            "SELECT * FROM task_runs WHERE id=?1 AND supervisor_token=?2",
            sql_params![id, token],
            stored_run_row,
        )
        .optional()?)
}

/// Save what a domain command made of a run read with [`stored_run`].
/// `status=from` (the status the command started from) and, when given,
/// `supervisor_token=token` only detect a concurrent change: the domain
/// decided the move. Whether a row was updated. The record the domain
/// gives a move into or out of the unfinished statuses
/// ([`run::unfinished_change`]) is written in the same transaction.
fn save_run(
    conn: &impl crate::infrastructure::sql_exec::SqlWrite,
    run: &TaskRun,
    from: RunStatus,
    token: Option<&LeaseToken>,
) -> Result<bool> {
    let saved = save_run_row(conn, run, from, token)?;
    if saved && let Some(change) = run::unfinished_change(from, run.status()) {
        event(
            conn,
            run.task_id(),
            Some(run.id()),
            change.kind,
            change.payload,
        )?;
    }
    // A run that comes to rest with nobody holding it settles its task's
    // hold in the same transaction.
    if saved && from != run.status() {
        crate::infrastructure::task_holds::settle_at_rest_in(conn, run.id(), false)?;
    }
    Ok(saved)
}

fn save_run_row(
    conn: &impl sql_exec::SqlWrite,
    run: &TaskRun,
    from: RunStatus,
    token: Option<&LeaseToken>,
) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE task_runs SET status=?3,repo_path=?4,run_dir=?5,branch=?6,worktree_path=?7,
         receipt_path=?8,log_path=?9,workspace_id=?10,result_commit=?11,last_error=?12,
         workspace_closed_at=?13
         WHERE id=?1 AND status=?2 AND (CAST(?14 AS TEXT) IS NULL OR supervisor_token=?14)",
        sql_params![
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

/// End run `id` for the release of its task's hold that chose a new run
/// (ADR-t1879-1), inside the caller's transaction: `failed` with `reason`
/// ([`run::end_for_release`]), and, when the next run carries its work
/// over, `triage_finished` with `carried` as its payload (a retry by hand,
/// `retry_inherit` with the role that released it as `by`, which the
/// automatic retry's once-per-task count leaves out). The stored run comes
/// back.
pub(super) fn end_released_run(
    conn: &impl crate::infrastructure::sql_exec::SqlWrite,
    id: &RunId,
    reason: String,
    carried: Option<serde_json::Value>,
) -> Result<TaskRun> {
    let run = stored_run(conn, id)?.with_context(|| format!("run {id} does not exist"))?;
    let from = run.status();
    let run = run::end_for_release(run, reason)?;
    ensure!(
        save_run(conn, &run, from, None)?,
        "run {id} changed while it was released; nothing is released, try again"
    );
    if let Some(payload) = carried {
        run_event(conn, id, EventKind::TriageFinished, payload)?;
    }
    Ok(run)
}

/// End run `id`, parked for a replanning, as the apply of its replacement
/// replaced it ([`run::end_for_replacement`]), inside the apply's
/// transaction. The stored run comes back.
pub(super) fn end_replaced_run(
    conn: &impl crate::infrastructure::sql_exec::SqlWrite,
    id: &RunId,
) -> Result<TaskRun> {
    let run = stored_run(conn, id)?.with_context(|| format!("run {id} does not exist"))?;
    let from = run.status();
    let run = run::end_for_replacement(run)?;
    ensure!(
        save_run(conn, &run, from, None)?,
        "run {id} changed while its replacement was applied; nothing is applied"
    );
    Ok(run)
}

/// Read the run, apply the domain `command` and save the result, inside
/// the caller's transaction; the stored (not relocated) run comes back.
/// A missing run, a command the domain refuses and a row that changed
/// meanwhile all fail with `refusal`, the message the store has always
/// given for the operation. The reason of a domain refusal is not part of
/// that message; it is kept in `refusals` instead.
fn apply(
    conn: &impl crate::infrastructure::sql_exec::SqlWrite,
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
    conn: &impl crate::infrastructure::sql_exec::SqlWrite,
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
fn record_events(
    conn: &impl crate::infrastructure::sql_exec::SqlWrite,
    id: &RunId,
    events: Vec<run::NewRunEvent>,
) -> Result<()> {
    for event in events {
        run_event(conn, id, event.kind, event.payload)?;
    }
    Ok(())
}

/// The most rows a background job's bulk write (the node rows' retention,
/// the import of a CSV file of the host's load) puts in one transaction:
/// each batch is committed on its own, so the write lock is held a short
/// while (a few hundred small statements) next to a heartbeat's wait of
/// [`crate::application::QUEUE_BUSY_TIMEOUT`], and a job stopped between
/// two leaves what it committed for the next one to go on from.
pub const ROWS_PER_TRANSACTION: usize = 200;

/// How long such a job waits between two of its transactions, with no
/// lock held: longer than the longest step of SQLite's busy wait (100 ms),
/// so a writer waiting on the lock (a heartbeat) gets it before the next
/// batch takes it again.
pub const PAUSE_BETWEEN_TRANSACTIONS: std::time::Duration = std::time::Duration::from_millis(120);

/// Run `write` on each batch of at most [`ROWS_PER_TRANSACTION`] of
/// `items` in its own immediate transaction named `name`
/// ([`SqliteQueue::write_tx`]), committed before the next starts,
/// [`PAUSE_BETWEEN_TRANSACTIONS`] apart; `items` empty runs it once on no
/// items. `write` is told whether its batch is the last. A failure stops
/// at its batch: the batches before it stay committed.
fn in_batches<T>(
    queue: &SqliteQueue,
    name: &'static str,
    items: &[T],
    mut write: impl FnMut(&super::sqlite::WriteTx<'_>, &[T], bool) -> Result<()>,
) -> Result<()> {
    let batches = items.len().div_ceil(ROWS_PER_TRANSACTION).max(1);
    for index in 0..batches {
        if index > 0 {
            std::thread::sleep(PAUSE_BETWEEN_TRANSACTIONS);
        }
        let start = (index * ROWS_PER_TRANSACTION).min(items.len());
        let end = (start + ROWS_PER_TRANSACTION).min(items.len());
        let tx = queue.write_tx(name)?;
        write(&tx, &items[start..end], index + 1 == batches)?;
        tx.commit()?;
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
    "run is not awaiting integration, a session or an already_satisfied claim with an open workspace under this supervisor"
        .to_owned()
}

/// Renew `token`'s lease on the run at `now`, inside the caller's
/// `BEGIN IMMEDIATE` transaction, or refuse when the lease row is gone or
/// carries another token (ADR-0039 decision 7). A heartbeat older than
/// `HEARTBEAT_TIMEOUT_SECS` is no reason to refuse: after a host sleep the
/// row is still this supervisor's until an adopter swaps the token or
/// `recover` removes it, and SQLite serializes that write with this one.
fn renew_lease(
    conn: &impl sql_exec::SqlWrite,
    id: &RunId,
    token: &LeaseToken,
    now: i64,
) -> Result<()> {
    let renewed = conn.execute(
        "UPDATE run_leases SET heartbeat_at=?3 WHERE run_id=?1 AND token=?2",
        sql_params![id, token, now],
    )?;
    ensure!(
        renewed == 1,
        "run lease is missing or held by another supervisor"
    );
    Ok(())
}

/// The pid a lease of `token` records: that of the supervisor registered
/// under `token`, as the queue service writes for a supervisor in another
/// process, or this process's when `token` has no registration.
pub(super) fn holder_pid(conn: &impl sql_exec::SqlRead, token: &LeaseToken) -> Result<u32> {
    Ok(conn
        .query_row(
            "SELECT pid FROM supervisors WHERE token=?1",
            sql_params![token],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(std::process::id))
}

fn lease_row(r: &SqlRow<'_>) -> SqlResult<RunLease> {
    Ok(RunLease {
        run_id: r.get(0)?,
        token: r.get(1)?,
        pid: r.get(2)?,
        heartbeat_at: r.get(3)?,
    })
}

/// The lease of run `id` on `conn`, if any.
pub(super) fn run_lease_of(conn: &impl sql_exec::SqlRead, id: &RunId) -> Result<Option<RunLease>> {
    Ok(conn
        .query_row(
            "SELECT run_id,token,pid,heartbeat_at FROM run_leases WHERE run_id=?1",
            sql_params![id],
            lease_row,
        )
        .optional()?)
}

/// Compare the stored flags before mapping to bool: SQLite historically
/// accepts exactly 1, not every nonzero integer; Postgres stores BOOLEAN.
const SUPERVISORS_SELECT: &str = "SELECT *, handoff_accepted=TRUE AS accepts_handoff,
    auto_update=TRUE AS updates_automatically FROM supervisors";

/// Every registered supervisor on `conn`, oldest registration first.
pub(super) fn supervisors_of(conn: &impl sql_exec::SqlRead) -> Result<Vec<SupervisorRegistration>> {
    Ok(conn.query_map(
        &format!(
            "{SUPERVISORS_SELECT} ORDER BY started_at, {}",
            conn.dialect().insertion_order("")
        ),
        sql_params![],
        supervisor_row,
    )?)
}

fn supervisor_row(r: &SqlRow<'_>) -> SqlResult<SupervisorRegistration> {
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
        handoff_accepted: r
            .get::<_, Option<bool>>("accepts_handoff")?
            .unwrap_or(false),
        handoff_binary: r.get("handoff_binary")?,
        auto_update: r
            .get::<_, Option<bool>>("updates_automatically")?
            .unwrap_or(false),
        max_waiting: r.get("max_waiting")?,
        parallel_source: source(r, "parallel_source")?,
        max_waiting_source: source(r, "max_waiting_source")?,
        runtime_planners: r.get("runtime_planners")?,
        runtime_planners_source: source(r, "runtime_planners_source")?,
        claim_spacing: r.get("claim_spacing")?,
        claim_spacing_source: source(r, "claim_spacing_source")?,
        max_spikes: r.get("max_spikes")?,
        max_spikes_source: source(r, "max_spikes_source")?,
        class_aging: r.get("class_aging")?,
        class_aging_source: source(r, "class_aging_source")?,
        max_load: r.get("max_load")?,
        max_load_source: source(r, "max_load_source")?,
        // Unreadable JSON (another binary's shape) reads as none.
        providers: r
            .get::<_, Option<String>>("providers")?
            .and_then(|json| serde_json::from_str(&json).ok()),
    })
}

/// A `*_source` column of `supervisors` (task 698); an unknown text reads
/// as none.
fn source(r: &SqlRow<'_>, column: &str) -> SqlResult<Option<SettingSource>> {
    Ok(r.get::<_, Option<String>>(column)?
        .as_deref()
        .and_then(SettingSource::parse))
}

fn assert_wrapper(conn: &impl sql_exec::SqlRead, id: &RunId, pid: u32) -> Result<()> {
    let valid: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM run_processes WHERE run_id=?1 AND role='wrapper' AND pid=?2 AND exited_at IS NULL)",
        sql_params![id,pid], |r| r.get(0))?;
    ensure!(valid, "wrapper is not the live owner of this run");
    Ok(())
}

/// Record `kind` for run `id`. A lease given back or a recovery ends the
/// step a supervisor held the run for: a hold of its task that waited for
/// that step settles in the same transaction once the run rests
/// ([`crate::infrastructure::task_holds::settle_at_rest_in`]).
fn run_event(
    conn: &impl crate::infrastructure::sql_exec::SqlWrite,
    id: &RunId,
    kind: EventKind,
    payload: serde_json::Value,
) -> Result<()> {
    let task_id: TaskId = conn.query_row(
        "SELECT task_id FROM task_runs WHERE id=?1",
        sql_params![id],
        |r| r.get(0),
    )?;
    event(conn, task_id, Some(id), kind, payload)?;
    if matches!(kind, EventKind::LeaseReleased | EventKind::RunRecovered) {
        super::task_holds::settle_at_rest_in(conn, id, false)?;
    }
    Ok(())
}

/// Record, inside the caller's transaction, that the run moved to `phase`
/// (`run_phase_changed`, ADR-t1662-1 decision 2) in the attempt last
/// recorded of it (the first when it cannot be read: the record never
/// fails the transition), `cause` the kind of the event the same
/// transaction wrote.
fn phase_event(
    conn: &impl crate::infrastructure::sql_exec::SqlWrite,
    id: &RunId,
    phase: run_phase::Phase,
    cause: EventKind,
) -> Result<()> {
    let attempt = conn
        .query_row(
            "SELECT payload FROM run_events WHERE run_id=?1 AND kind=?2 ORDER BY id DESC LIMIT 1",
            sql_params![id, EventKind::RunPhaseChanged.as_str()],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|payload| serde_json::from_str::<Value>(&payload).ok())
        .and_then(|payload| run_phase::Recorded::of(&payload))
        .map_or(run_phase::Attempt::FIRST, |recorded| recorded.attempt);
    run_event(
        conn,
        id,
        EventKind::RunPhaseChanged,
        run_phase::PhaseChange::new(phase, attempt, cause.as_str()).payload(),
    )
}

/// End the run's stalled detections inside the caller's transaction, the
/// run being taken out of its session by no watch of its own (`recover`,
/// the supervisor's abandon, the sweep; ADR-0047 decisions 30 and 32):
/// its `stalled` asks nobody closed are closed with `answer` (an open one
/// answered by the runtime, `runtime_closed: true`), and each detection
/// with no end recorded gets its `stall_resolved` of outcome `run_ended`,
/// once: one already ended by the watch or an earlier close gets none.
pub(super) fn end_stalled_detections(
    tx: &impl crate::infrastructure::sql_exec::SqlWrite,
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
        .query_map(
            "SELECT id, answer FROM asks WHERE run_id=?1 AND kind='stalled'
             AND answer IS NOT NULL",
            sql_params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?
        .into_iter()
        .collect();
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
    tx: &impl sql_exec::SqlWrite,
    id: &RunId,
    token: &LeaseToken,
    now: i64,
    clear_processes: bool,
) -> Result<Option<Option<LeaseToken>>> {
    let parked: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM task_runs WHERE id=?1 AND status='needs_session')",
        sql_params![id],
        |r| r.get(0),
    )?;
    if !parked {
        return Ok(None);
    }
    let lease = tx
        .query_row(
            "SELECT run_id,token,pid,heartbeat_at FROM run_leases WHERE run_id=?1",
            sql_params![id],
            lease_row,
        )
        .optional()?;
    if let Some(lease) = &lease {
        if !lease_is_stale(lease, now) {
            return Ok(None);
        }
        tx.execute("DELETE FROM run_leases WHERE run_id=?1", sql_params![id])?;
    }
    // A session still heartbeating is left alone.
    let live: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_processes WHERE run_id=?1
         AND exited_at IS NULL AND heartbeat_at >= ?2-?3)",
        sql_params![id, now, HEARTBEAT_TIMEOUT_SECS],
        |r| r.get(0),
    )?;
    if live {
        return Ok(None);
    }
    if clear_processes {
        tx.execute("DELETE FROM run_processes WHERE run_id=?1", sql_params![id])?;
    }
    tx.execute(
        "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,?2,?3,?4)",
        sql_params![id, token, holder_pid(tx, token)?, now],
    )?;
    tx.execute(
        "UPDATE task_runs SET supervisor_token=?2 WHERE id=?1",
        sql_params![id, token],
    )?;
    Ok(Some(lease.map(|l| l.token)))
}

/// The events of run `?1`, oldest first, by `events_by_run` (goal 103).
pub(crate) const RUN_EVENTS_SQL: &str = "SELECT * FROM run_events WHERE run_id=?1 ORDER BY id";

/// Whether run `?1` has an event of kind `?2`, by `events_by_run`.
pub(crate) const HAS_RUN_EVENT_SQL: &str =
    "SELECT EXISTS(SELECT 1 FROM run_events WHERE run_id=?1 AND kind=?2)";

/// The id of the latest `resume_started` of run `?1`, or 0, by
/// `events_by_run`.
pub(crate) fn last_resume_started_sql() -> String {
    format!(
        "SELECT coalesce(max(id),0) FROM run_events WHERE run_id=?1 AND kind='{}'",
        crate::domain::event_kind::RESUME_STARTED
    )
}

pub(super) fn run_events_of(conn: &impl sql_exec::SqlRead, id: &RunId) -> Result<Vec<RunEvent>> {
    Ok(conn.query_map(RUN_EVENTS_SQL, sql_params![id], event_row)?)
}

/// The run's resumes, counted as ADR-0047 decision 24 says.
fn resume_count(conn: &impl sql_exec::SqlRead, id: &RunId) -> Result<ResumeCount> {
    Ok(ResumeCount::of(&run_events_of(conn, id)?))
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

impl SqliteOpener {
    /// One connection, as [`crate::application::QueueOpener::open`] and
    /// [`SqlitePorts`] open it.
    fn connect(&self) -> Result<SqliteQueue> {
        let mut queue = SqliteQueue::open(&self.db)
            .map_err(super::sqlite::tag_busy)?
            .with_generators(self.generators.clone());
        if let Some(actor) = &self.actor {
            queue = queue.with_actor(actor.clone());
        }
        Ok(queue)
    }
}

impl crate::application::QueueOpener for SqliteOpener {
    fn open(&self) -> Result<Box<dyn crate::application::Queue + Send>> {
        Ok(Box::new(self.connect()?))
    }
}

/// Opens the connections of `opener` as only the ports `P` a use case
/// takes: `keep` boxes each as `P`.
pub struct SqlitePorts<P: ?Sized> {
    pub opener: SqliteOpener,
    pub keep: fn(SqliteQueue) -> Box<P>,
}

impl<P: ?Sized> crate::application::QueueOpener<P> for SqlitePorts<P> {
    fn open(&self) -> Result<Box<P>> {
        self.opener.connect().map(self.keep)
    }
}

pub(super) fn processes_for_task_port(
    conn: &impl sql_exec::SqlRead,
    task_id: TaskId,
) -> Result<Vec<RunProcess>> {
    Ok(conn.query_map(&format!("SELECT p.* FROM run_processes p JOIN task_runs r ON r.id=p.run_id WHERE r.task_id=?1 ORDER BY {},p.role", conn.dialect().insertion_order("r")), sql_params![task_id], process_row)?)
}

fn process_row(r: &SqlRow<'_>) -> SqlResult<RunProcess> {
    Ok(RunProcess {
        run_id: r.get("run_id")?,
        role: r.get("role")?,
        pid: r.get("pid")?,
        heartbeat_at: r.get("heartbeat_at")?,
        exited_at: r.get("exited_at")?,
        exit_code: r.get("exit_code")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{LeaseToken, RunId, RunLease};

    fn lease(pid: u32, heartbeat_at: i64) -> RunLease {
        RunLease {
            run_id: RunId::new("11111111-1111-4111-8111-111111111111").unwrap(),
            token: LeaseToken::new("first"),
            pid,
            heartbeat_at,
        }
    }

    /// A lease of a live process turns stale one second past
    /// `HEARTBEAT_TIMEOUT_SECS` (moved from `tests/it/runtime_claim.rs`,
    /// task 1709); a pid that cannot be a process is stale at once.
    #[test]
    fn a_lease_turns_stale_one_second_past_the_heartbeat_timeout() {
        const T: i64 = 1_900_000_000;
        let live = lease(std::process::id(), T);
        assert!(!lease_is_stale(&live, T));
        assert!(!lease_is_stale(&live, T + HEARTBEAT_TIMEOUT_SECS));
        assert!(lease_is_stale(&live, T + HEARTBEAT_TIMEOUT_SECS + 1));
        assert!(lease_is_stale(&lease(u32::MAX, T), T));
    }

    /// The unix second the queue of [`failed_run`] stops at.
    const STOPPED: i64 = 1_900_000_000;

    /// A queue whose clock is stopped at [`STOPPED`], with one task in
    /// progress whose one run failed, and an answered `decide` ask of that
    /// run's triage (`retry`, `resume`, `cancel`) answered `answer`.
    fn failed_run(
        dir: &std::path::Path,
        answer: &str,
    ) -> (
        SqliteQueue,
        crate::domain::TaskId,
        RunId,
        crate::domain::Ask,
    ) {
        use crate::application::TaskStore;
        use crate::domain::NewTask;
        /// The queue's clock stopped at one time: what the lease's
        /// freshness is judged against does not move while the test runs.
        struct Stopped(std::time::Instant);
        impl crate::application::Clock for Stopped {
            fn system_time(&self) -> std::time::SystemTime {
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(STOPPED.unsigned_abs())
            }
            fn monotonic(&self) -> std::time::Instant {
                self.0
            }
        }
        let mut queue = SqliteQueue::init(dir.join("q.db"))
            .unwrap()
            .with_generators(crate::application::Generators {
                clock: std::sync::Arc::new(Stopped(std::time::Instant::now())),
                ..crate::infrastructure::clock::system()
            });
        assert_eq!(queue.generators.clock.now(), STOPPED);
        let task = queue
            .add(NewTask {
                title: "t".into(),
                description: String::new(),
                acceptance: String::new(),
                verification_commands: Vec::new(),
                required_evidence: Vec::new(),
                paths: Vec::new(),
                dependencies: Vec::new(),
                goal_dependencies: Vec::new(),
                priority: Default::default(),
                goal_id: None,
                context: String::new(),
                change: None,
                provider: None,
                worker_mode: None,
                wait_for_build: false,
                execution_class: Default::default(),
            })
            .unwrap()
            .id();
        let run = RunId::new("11111111-1111-4111-8111-111111111111").unwrap();
        queue
            .conn
            .execute_batch(&format!(
                "UPDATE tasks SET status='in_progress' WHERE id={task};
                 INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,worktree_path,workspace_id,run_dir)
                 VALUES ('{run}',{task},'failed','claude','claude','0000000000000000000000000000000000000000','/wt','W','/run');"
            ))
            .unwrap();
        let ask = decide_ask(&mut queue, &run, answer);
        (queue, task, run, ask)
    }

    /// A `decide` ask of `run`'s triage, answered `answer`.
    fn decide_ask(queue: &mut SqliteQueue, run: &RunId, answer: &str) -> crate::domain::Ask {
        use crate::domain::{AskKind, AskReason, NewAsk};
        let ask = queue
            .ask(NewAsk {
                recommendation: None,
                confidence: None,
                topics: Vec::new(),
                kind: AskKind::Decide,
                task_id: None,
                run_id: Some(run.clone()),
                question: "what now?".into(),
                options: vec!["retry".into(), "resume".into(), "cancel".into()],
                asked_by: "supervisor".into(),
                reason_category: AskReason::RecoveryFailed,
                finding_id: None,
                request_id: None,
            })
            .unwrap()
            .ask;
        queue.answer(ask.id, answer).unwrap();
        ask
    }

    /// The answer of a triage's ask, applied by the queue in one
    /// transaction, by what its SQL reads: an answer refused after the
    /// transaction wrote (a `resume` of a run whose task ended, refused
    /// once the stale lease of the run went) moves nothing, the lease
    /// included; the task's latest run is the one inserted last, so a run
    /// with a later one takes no answer; `resume` parks the run for a
    /// session with the reason, takes the stale lease away and closes the
    /// ask, which leaves the answers to apply, and the ask it closed is not
    /// applied again. Which answers are refused is
    /// `domain::recovery::triage_answer_refusal`'s, `triage_answer`'s and
    /// `domain::telemetry::run_revivable`'s, whose unit tests hold the
    /// cases.
    #[test]
    fn a_refused_triage_answer_moves_nothing_and_a_resume_parks_the_run() {
        use crate::application::TaskReads;
        use crate::domain::{RunStatus, TaskStatus};
        let dir = tempfile::tempdir().unwrap();
        let (mut queue, task, run, ask) = failed_run(dir.path(), "resume");
        assert_eq!(queue.triage_answers().unwrap()[0].id, ask.id);
        queue
            .conn
            .execute(
                "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,'stalled',?2,?3)",
                rusqlite::params![
                    run,
                    std::process::id(),
                    STOPPED - HEARTBEAT_TIMEOUT_SECS - 1
                ],
            )
            .unwrap();
        let set_task = |queue: &SqliteQueue, status: TaskStatus| {
            queue
                .conn
                .execute(
                    "UPDATE tasks SET status=?1 WHERE id=?2",
                    rusqlite::params![status.as_str(), task.as_i64()],
                )
                .unwrap();
        };
        set_task(&queue, TaskStatus::Canceled);
        let error = queue
            .decide_triage(&run, ask.id, "resume", "fix the test")
            .unwrap_err()
            .to_string();
        assert!(error.contains("no longer the latest run"), "{error}");
        assert_eq!(queue.run(&run).unwrap().status(), RunStatus::Failed);
        assert_eq!(
            queue.show(task).unwrap().task.status(),
            TaskStatus::Canceled
        );
        assert!(queue.read_ask(ask.id).unwrap().closed_at.is_none());
        assert!(queue.run_lease(&run).unwrap().is_some());
        // The task goes on again but has a later run, inserted after this
        // one: refused, nothing moves.
        set_task(&queue, TaskStatus::InProgress);
        queue
            .conn
            .execute_batch(&format!(
                "CREATE TEMP TABLE later AS SELECT * FROM task_runs WHERE id='{run}';
                 UPDATE later SET id='later', status='claimed', branch=NULL,
                   worktree_path=NULL, workspace_id=NULL;
                 INSERT INTO task_runs SELECT * FROM later;"
            ))
            .unwrap();
        let error = queue
            .decide_triage(&run, ask.id, "resume", "fix the test")
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            format!("run {run} is no longer the latest run of task {task}")
        );
        assert_eq!(queue.run(&run).unwrap().status(), RunStatus::Failed);
        assert!(queue.read_ask(ask.id).unwrap().closed_at.is_none());
        // The run its task's latest again: the same answer is applied.
        queue
            .conn
            .execute("DELETE FROM task_runs WHERE id='later'", [])
            .unwrap();
        let parked = queue
            .decide_triage(&run, ask.id, "resume", "fix the test")
            .unwrap();
        assert_eq!(parked.status(), RunStatus::NeedsSession);
        assert_eq!(parked.last_error(), Some("fix the test"));
        assert!(queue.run_lease(&run).unwrap().is_none());
        assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
        assert!(queue.triage_answers().unwrap().is_empty());
        // The ask it closed is not applied again, the run failed once more.
        queue
            .conn
            .execute("UPDATE task_runs SET status='failed' WHERE id=?1", [&run])
            .unwrap();
        let closed = queue
            .decide_triage(&run, ask.id, "retry", "x")
            .unwrap_err()
            .to_string();
        assert!(closed.contains("not an answered, unclosed ask"), "{closed}");
        assert_eq!(queue.run(&run).unwrap().status(), RunStatus::Failed);
        assert_eq!(
            queue.show(task).unwrap().task.status(),
            TaskStatus::InProgress
        );
    }

    /// A person's answer to a triage's ask is refused while a lease that is
    /// not stale holds the run; a stale one (a stalled supervisor's) goes
    /// with the answer in its transaction, so that its holder, woken up,
    /// cannot write after the decision (ADR-0039 decision 7).
    #[test]
    fn a_triage_answer_takes_the_stale_lease_of_its_run_away() {
        use crate::application::TaskReads;
        use crate::domain::TaskStatus;
        let dir = tempfile::tempdir().unwrap();
        let (mut queue, task, run, ask) = failed_run(dir.path(), "retry");
        queue
            .conn
            .execute(
                "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,'stalled',?2,?3)",
                rusqlite::params![run, std::process::id(), STOPPED],
            )
            .unwrap();
        let leased = queue
            .decide_triage(&run, ask.id, "retry", "x")
            .unwrap_err()
            .to_string();
        assert_eq!(leased, format!("run {run} is leased"));
        queue
            .conn
            .execute(
                "UPDATE run_leases SET heartbeat_at=?1",
                [STOPPED - HEARTBEAT_TIMEOUT_SECS - 1],
            )
            .unwrap();
        queue.decide_triage(&run, ask.id, "retry", "x").unwrap();
        assert_eq!(queue.show(task).unwrap().task.status(), TaskStatus::Ready);
        assert!(queue.run_lease(&run).unwrap().is_none());
        let late = queue
            .finish_triage(
                &run,
                &LeaseToken::new("stalled"),
                &crate::application::TriageAction::Retry,
                serde_json::json!({}),
                Vec::new(),
            )
            .unwrap_err()
            .to_string();
        assert!(late.contains("run lease is missing"), "{late}");
    }

    /// A lease written for a registered supervisor records the
    /// registration's pid, not that of the process writing it (the queue
    /// service); a token with no registration records the writer's own.
    #[test]
    fn a_lease_records_the_pid_its_token_registered() {
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let token = LeaseToken::new("registered");
        queue.register_supervisor(&token, 4242, 1, "0.0.1").unwrap();
        assert_eq!(holder_pid(&*queue.conn, &token).unwrap(), 4242);
        assert_eq!(
            holder_pid(&*queue.conn, &LeaseToken::new("unregistered")).unwrap(),
            std::process::id()
        );
    }
}
