//! The queue's SQLite store of tasks, goals, dependencies, claims and lists
//! (ADR-0013). A row is restored into its domain aggregate (`Task`, `Goal`,
//! `TaskRun`), handed to a domain command, and the aggregate that comes back
//! is written; the store decides nothing a command decides. A row the
//! aggregate refuses to restore is a read error, never a default.
//!
//! Every state change runs in one `BEGIN IMMEDIATE` transaction with the
//! events it records. A `WHERE status=<the status read>` or
//! `AND supervisor_token=<token>` on an update only detects a concurrent
//! change (which the immediate transaction normally rules out); it is not
//! where a transition is allowed. A new row takes its ID from `next_id`
//! inside the same transaction, so a refused insert rolls back without
//! spending an ID. The only rule left in SQL is cycle detection over the
//! whole dependency graph (a recursive CTE over task, goal and membership
//! edges); whether to refuse, and the error, are the domain's.
//!
//! A planning command passes the task state it was authorized on; the
//! `*_authorized` writes compare it right after the transaction opens and
//! refuse without writing when it changed. Writers that are not authorized
//! per command (the supervisor's landing, draft planners) skip the check.
//!
//! The row helpers other store modules share live in [`super::store_rows`]
//! on the execution port; those of the same name here take rusqlite's types
//! for a module that has not moved to it, and only hand on to them, as do
//! those here of [`super::store_writes`], the writes they share.
use super::sql_exec::sqlite::{
    attach_event_actors, attach_event_bodies, is_contention, native_error, port_tx, with_row,
};
use super::store_rows;
use super::store_writes;
use crate::domain::event_kind::EventKind;
use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

#[cfg(test)]
use crate::domain::{ActorRole, RequestId, plan_request::PriorityBy, task};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior};
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::{
    application::{Generators, GraphInput, IdGenerator, TaskPage, TaskQuery, TaskReads, TaskStore},
    domain::{
        ChangeSet, ClaimOutcome, CommitSha, DomainError, Goal, GoalDetail, GoalEdit, GoalId,
        GoalPredecessor, GoalSummary, GoalVerdict, LintInput, NewGoal, NewNote, NewTask, NotePage,
        NoteQuery, Predecessor, Priority, Proposal, ProposalId, RunEvent, RunId, Submission,
        TagSet, Task, TaskAction, TaskAuthorization, TaskDetail, TaskEdit, TaskId, TaskRun,
        TaskStatus, TitlePrefixes, actor::ActorContext, cancel_reason::NewCancel,
        provider_switch::WorkerRoute, scope::validate_path_globs, worker_model::WorkerTrial,
    },
    infrastructure::{
        clock,
        event_actor::process_actor,
        location::runs_dir,
        proposals,
        schema::{self, BINARY_SCHEMA, MIGRATIONS},
    },
};

pub(super) use super::store_rows::replan_guarding;
pub use super::write_leader::WriteIdentity;

const APPLICATION_ID: i64 = 0x43545131;

/// How long a [`WriteTx`] may hold the queue's write lock before it warns
/// and records `queue_write_held_long` with its name, how long it held the
/// lock, how long it ran before it had the lock, and the rows it wrote.
/// Under [`crate::application::QUEUE_BUSY_TIMEOUT`], so a writer that holds
/// the lock long enough to fail another connection's write (the
/// supervisor's heartbeat) is named in the log and the queue's events.
pub const WRITE_HOLD_WARN: Duration = Duration::from_secs(2);
const _: () =
    assert!(WRITE_HOLD_WARN.as_nanos() < crate::application::QUEUE_BUSY_TIMEOUT.as_nanos());

/// The times of one write transaction, each since just before its `BEGIN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteTxTimes {
    /// When it is known to have had the write lock: when `BEGIN IMMEDIATE`
    /// returned, or when a deferred transaction's
    /// first write statement through [`WriteTx::execute`] returned, if the
    /// transaction had no lock just before it. That statement takes the
    /// lock and may wait in it for another writer, so its end is the first
    /// time known to have the lock without that wait; the hold drops the
    /// statement's own run. A deferred one whose lock a write through the
    /// transaction it derefs to took has no such time: one whose first
    /// write was that, or was reported with [`WriteTx::wrote`] (which comes
    /// after the statement ran), is held from its `BEGIN` (`Some` of
    /// zero), and one seen to have had the lock only by the state of its
    /// transaction (`sqlite3_txn_state` of `main` is `Write`) just before
    /// its commit or rollback is `None`, held from its `BEGIN` too. Either
    /// approximation also counts its reads and any wait for another writer
    /// before its first write. Also `None` for one that never had the lock.
    pub locked: Option<Duration>,
    /// Whether it held the write lock: a write one ([`WriteTx::write`]) from
    /// its `BEGIN`, read or not; a read one ([`WriteTx::read`]) once a write statement took
    /// it, one that changed no row included (an `UPDATE` or `DELETE` of no
    /// row, an `INSERT ... ON CONFLICT DO NOTHING` takes it too). A
    /// read one that only read did not.
    pub wrote: bool,
    /// When its commit or rollback returned.
    pub ended: Duration,
    /// Business rows (excluding the leader fence) its write statements through [`WriteTx::execute`] and
    /// [`WriteTx::wrote`] changed; those through the transaction it derefs
    /// to are not counted.
    pub rows: usize,
}

/// A write transaction that held the write lock past the threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LongHold {
    /// From having the lock (or its `BEGIN`, when that is not known) to the
    /// end of its commit or rollback.
    pub held: Duration,
    /// Before it had the lock: its wait for another writer, and in a
    /// deferred one its reads before its first write; not counted as held.
    pub before_lock: Duration,
    pub rows: usize,
}

/// The queues this process refuses to open ([`refuse_opening`]): none in
/// production.
static REFUSED: std::sync::LazyLock<std::sync::Mutex<Vec<PathBuf>>> =
    std::sync::LazyLock::new(Default::default);

/// While the guard lives, this process refuses to open the queue at `db`
/// (a test's proof that a path of the control side reaches it only
/// through the queue service, which runs in another process): every
/// connection to it fails, whatever opened it.
pub fn refuse_opening(db: &Path) -> RefuseOpening {
    let db = db.canonicalize().unwrap_or_else(|_| db.to_path_buf());
    REFUSED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(db.clone());
    RefuseOpening(db)
}

/// The refusal of [`refuse_opening`], lifted when dropped.
pub struct RefuseOpening(PathBuf);

impl Drop for RefuseOpening {
    fn drop(&mut self) {
        let mut refused = REFUSED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(at) = refused.iter().position(|db| *db == self.0) {
            refused.remove(at);
        }
    }
}

std::thread_local! {
    /// How many [`AnotherProcess`] guards this thread holds.
    static ANOTHER_PROCESS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// While the guard lives, this thread stands for another process (a
/// test's double of a session wrapper) and opens a queue
/// [`refuse_opening`] refuses to the rest of the process.
pub fn as_another_process() -> AnotherProcess {
    ANOTHER_PROCESS.with(|held| held.set(held.get() + 1));
    AnotherProcess(std::marker::PhantomData)
}

/// The guard of [`as_another_process`], of the thread that took it.
pub struct AnotherProcess(std::marker::PhantomData<*const ()>);

impl Drop for AnotherProcess {
    fn drop(&mut self) {
        ANOTHER_PROCESS.with(|held| held.set(held.get().saturating_sub(1)));
    }
}

fn ensure_not_refused(path: &Path) -> Result<()> {
    if ANOTHER_PROCESS.with(std::cell::Cell::get) > 0 {
        return Ok(());
    }
    let refused = REFUSED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if refused.is_empty() {
        return Ok(());
    }
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    anyhow::ensure!(
        !refused.contains(&path),
        "this process refuses to open the queue at {}",
        path.display()
    );
    Ok(())
}

/// The [`LongHold`] of a transaction of `times` that held the lock longer
/// than `threshold`; `None` for one within it or one that never had the
/// lock. One whose lock time is not known is held from its `BEGIN`.
pub fn long_hold(times: WriteTxTimes, threshold: Duration) -> Option<LongHold> {
    if !times.wrote {
        return None;
    }
    let locked = times.locked.unwrap_or(Duration::ZERO);
    let held = times.ended.saturating_sub(locked);
    (held > threshold).then_some(LongHold {
        held,
        before_lock: locked,
        rows: times.rows,
    })
}

/// The payload of `queue_write_held_long` (and the fields of its WARN) for
/// `hold` of the transaction `name`, in a process acting as `role`, running
/// `command` (`None` where none was set) as `pid`.
pub fn held_long_payload(
    name: &str,
    hold: LongHold,
    role: &str,
    command: Option<&str>,
    pid: u32,
) -> serde_json::Value {
    let millis = |duration: Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    json!({
        "transaction": name,
        "held_ms": millis(hold.held),
        "before_lock_ms": millis(hold.before_lock),
        "rows": hold.rows,
        "role": role,
        "command": command,
        "pid": pid,
    })
}

/// A named write transaction on the queue that, when it ends by its commit
/// or by a drop (rollback), judges whether it held the write lock past its
/// threshold ([`long_hold`]). One that did warns `queue write lock held
/// long` with the fields of [`held_long_payload`] and, once its commit or
/// rollback returned and so the lock is no longer held, records
/// `queue_write_held_long` with them in a short write of its own on the
/// same connection ([`WriteTx::recorded`]); the queue service's are on the
/// connections it writes through. A failure of that record is a WARN and
/// changes nothing of the transaction's result.
///
/// Ordinary transactions begin at [`WriteTx::write`] or [`WriteTx::read`].
/// The leader's conditional mutations and schema migration use
/// [`WriteTx::leader`] for their own first update; the boundary test restricts
/// its callers (`context_boundaries::write_tx`). A write one holds the lock
/// from its `BEGIN`. A read one has it once a write statement took it: the first through
/// [`WriteTx::execute`] tells when, when no write before it took the lock;
/// one through the transaction it derefs to, reported with
/// [`WriteTx::wrote`] or not (then seen by the state of its transaction
/// just before it ends), is held from its `BEGIN`
/// ([`WriteTxTimes::locked`]).
pub struct WriteTx<'c> {
    tx: Option<rusqlite::Transaction<'c>>,
    conn: &'c Connection,
    name: &'static str,
    threshold: Duration,
    began: std::time::Instant,
    locked: std::cell::Cell<Option<std::time::Instant>>,
    wrote: std::cell::Cell<bool>,
    rows: std::cell::Cell<usize>,
    record: bool,
    identity: Option<WriteIdentity>,
}

impl<'c> WriteTx<'c> {
    /// The write entry: begin `name` on `conn` as `BEGIN IMMEDIATE`, which
    /// has the write lock when it returns, so a writer that loses the race
    /// waits (or is told busy) at its `BEGIN` and not at its first write.
    /// Its first statement fences `identity` and extends its lease; a
    /// mismatch rolls back and returns `NotLeader`. Only the leader port
    /// and schema bootstrap/migration use their own conditional first update.
    /// It warns of a long hold and records none until [`WriteTx::recorded`].
    pub fn write(
        conn: &'c Connection,
        name: &'static str,
        threshold: Duration,
        identity: &WriteIdentity,
    ) -> Result<Self> {
        let tx = Self::leader(conn, name, threshold)?;
        super::write_leader::fence(&tx, identity).map_err(native_error)?;
        tx.rows.set(0);
        let mut tx = tx;
        tx.identity = Some(identity.clone());
        Ok(tx)
    }

    /// Unfenced start only for the leader port's conditional mutations and
    /// schema bootstrap/migration, which supply their own first statement.
    /// The boundary test restricts every call to these implementations.
    pub(super) fn leader(
        conn: &'c Connection,
        name: &'static str,
        threshold: Duration,
    ) -> rusqlite::Result<Self> {
        Self::begin(conn, name, TransactionBehavior::Immediate, threshold)
    }

    /// The read entry: begin `name` on `conn` as a deferred transaction,
    /// which takes no lock at its `BEGIN`, for reads that see one snapshot.
    /// A write in it still takes the lock then, and is warned of and
    /// recorded as one in a [`WriteTx::write`] is. It warns of a long hold
    /// and records none until [`WriteTx::recorded`].
    pub fn read(
        conn: &'c Connection,
        name: &'static str,
        threshold: Duration,
    ) -> rusqlite::Result<Self> {
        Self::begin(conn, name, TransactionBehavior::Deferred, threshold)
    }

    /// Begin `name` on `conn` as `behavior`: the one place a queue DB
    /// transaction begins, reached through [`WriteTx::leader`] (the write
    /// entry and conditional leader/schema updates) or [`WriteTx::read`].
    /// An immediate one has the lock when its `BEGIN` returns.
    fn begin(
        conn: &'c Connection,
        name: &'static str,
        behavior: TransactionBehavior,
        threshold: Duration,
    ) -> rusqlite::Result<Self> {
        let began = std::time::Instant::now();
        let tx = rusqlite::Transaction::new_unchecked(conn, behavior)?;
        let locked =
            matches!(behavior, TransactionBehavior::Immediate).then(std::time::Instant::now);
        Ok(Self {
            tx: Some(tx),
            conn,
            name,
            threshold,
            began,
            wrote: std::cell::Cell::new(locked.is_some()),
            locked: std::cell::Cell::new(locked),
            rows: std::cell::Cell::new(0),
            record: false,
            identity: None,
        })
    }

    /// Also record a long hold as `queue_write_held_long` on its
    /// connection, which writes queue events (a [`SqliteQueue`]'s).
    pub fn recorded(mut self) -> Self {
        self.record = true;
        self
    }

    /// Run the write statement `sql` and count what it changed. In a
    /// deferred transaction that had no lock before it, the lock is known
    /// from its end; one that already had it (a write through the
    /// transaction it derefs to took it) had it from a time not known.
    pub fn execute<P: rusqlite::Params>(&self, sql: &str, params: P) -> rusqlite::Result<usize> {
        self.written(|tx| {
            tx.prepare_cached(sql)?
                .execute(params)
                .map(|rows| (rows, rows))
        })
    }

    /// Run the insert `sql` as [`WriteTx::execute`] does and return the ID
    /// of the row it inserted ([`insert_id`]).
    pub fn insert_id<P: rusqlite::Params>(&self, sql: &str, params: P) -> rusqlite::Result<i64> {
        self.written(|tx| insert_counted(tx, sql, params))
    }

    /// Run the insert `sql` as [`WriteTx::execute`] does and return the ID
    /// of the row it inserted, or `None` when it inserted none
    /// ([`insert_id`]).
    pub fn insert_id_if_inserted<P: rusqlite::Params>(
        &self,
        sql: &str,
        params: P,
    ) -> rusqlite::Result<Option<i64>> {
        self.written(|tx| {
            let (rows, id) = insert_counted(tx, sql, params)?;
            Ok((rows, (rows > 0).then_some(id)))
        })
    }

    /// Run the write `write`, which returns the rows it changed and its
    /// result, and count those rows; see [`WriteTx::execute`].
    fn written<T>(
        &self,
        write: impl FnOnce(&rusqlite::Transaction<'c>) -> rusqlite::Result<(usize, T)>,
    ) -> rusqlite::Result<T> {
        let unlocked = self.locked.get().is_none() && !self.holds_write_lock();
        let (rows, result) = write(self.inner())?;
        if unlocked {
            self.locked.set(Some(std::time::Instant::now()));
        }
        self.counted(rows);
        Ok(result)
    }

    /// A write statement run on the transaction this derefs to returned,
    /// changing `rows`. Whether a write before it had taken the lock is not
    /// known once it ran, so a deferred transaction whose first reported
    /// write this is is held from its `BEGIN`.
    pub fn wrote(&self, rows: usize) {
        self.counted(rows);
    }

    /// Count `rows` of a write statement that returned; a lock time still
    /// unset is not known, and counted from the `BEGIN`.
    fn counted(&self, rows: usize) {
        if self.locked.get().is_none() {
            self.locked.set(Some(self.began));
        }
        self.wrote.set(true);
        self.rows.set(self.rows.get() + rows);
    }

    pub fn commit(mut self) -> rusqlite::Result<()> {
        self.read_lock_state();
        let result = match self.tx.take() {
            Some(tx) => tx.commit(),
            None => Ok(()),
        };
        // Finish lock-hold accounting before callbacks do file I/O.
        drop(self);
        if result.is_ok() {
            super::sql_exec::flush_after_commit();
        }
        result
    }

    /// End it as its transaction's drop behavior says (a rollback unless
    /// set otherwise), returning that end's result as it is, an error
    /// included. Its long hold is judged as a commit's is.
    pub fn finish(mut self) -> rusqlite::Result<()> {
        self.read_lock_state();
        match self.tx.take() {
            Some(tx) => tx.finish(),
            None => Ok(()),
        }
    }

    /// A savepoint in it, which commits or, when dropped, rolls back only
    /// its own writes. Those take the write lock of this transaction's
    /// connection, so a deferred one that wrote only in a savepoint is seen
    /// to hold the lock just before it ends. The only mutable way in: the
    /// transaction itself is never handed out to be replaced or ended.
    pub fn savepoint(&mut self) -> rusqlite::Result<rusqlite::Savepoint<'_>> {
        self.tx
            .as_mut()
            .expect("a write transaction is open until it ends")
            .savepoint()
    }

    /// Whether a deferred transaction that reported no write holds the
    /// write lock now (a write through the transaction it derefs to took
    /// it), read just before it ends.
    fn read_lock_state(&self) {
        if !self.wrote.get() && self.holds_write_lock() {
            self.wrote.set(true);
        }
    }

    /// Whether the transaction holds the write lock now: its state of
    /// `main` is `Write` (`sqlite3_txn_state`). A state that cannot be read
    /// is taken as no lock.
    fn holds_write_lock(&self) -> bool {
        self.tx.as_ref().is_some_and(|tx| {
            matches!(
                tx.transaction_state(Some(rusqlite::MAIN_DB)),
                Ok(rusqlite::TransactionState::Write)
            )
        })
    }

    /// The rows it counted, for a test of what writes through it.
    #[cfg(test)]
    pub(super) fn rows_written(&self) -> usize {
        self.rows.get()
    }

    fn inner(&self) -> &rusqlite::Transaction<'c> {
        self.tx
            .as_ref()
            .expect("a write transaction is open until it ends")
    }
}

/// Run the insert `sql` on `conn` and return the ID of the row it inserted:
/// the one way the store reads a new row's ID (ADR-t3982-1 decision 4; in
/// a transaction, [`WriteTx::insert_id`]; on the execution port,
/// [`super::sql_exec::SqlWrite::insert_id`]). This form takes rusqlite's
/// parameters, for a module that has not moved to the port. SQLite gives it as the
/// connection's last inserted rowid, read right after the statement, so an
/// insert that inserts no row (`INSERT .. SELECT .. WHERE NOT EXISTS`)
/// returns the ID of the connection's insert before it; such a caller
/// checks the rows changed first ([`WriteTx::insert_id_if_inserted`]).
// Kept for store modules migrating independently; current callers use the port.
#[allow(dead_code)]
pub(super) fn insert_id<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> rusqlite::Result<i64> {
    insert_counted(conn, sql, params).map(|(_, id)| id)
}

/// The rows the insert `sql` changed and the last inserted rowid after it:
/// the only call of `last_insert_rowid` outside test code
/// (`context_boundaries::inserted_ids`).
pub(super) fn insert_counted<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> rusqlite::Result<(usize, i64)> {
    let rows = conn.prepare_cached(sql)?.execute(params)?;
    Ok((rows, conn.last_insert_rowid()))
}

impl<'c> std::ops::Deref for WriteTx<'c> {
    type Target = rusqlite::Transaction<'c>;
    fn deref(&self) -> &Self::Target {
        self.inner()
    }
}

impl Drop for WriteTx<'_> {
    fn drop(&mut self) {
        self.read_lock_state();
        // A transaction left open rolls back here, before the end is read.
        drop(self.tx.take());
        let since = |at: std::time::Instant| at.saturating_duration_since(self.began);
        let times = WriteTxTimes {
            locked: self.locked.get().map(since),
            ended: self.began.elapsed(),
            wrote: self.wrote.get(),
            rows: self.rows.get(),
        };
        let Some(hold) = long_hold(times, self.threshold) else {
            return;
        };
        let actor = process_actor();
        let command = super::event_actor::process_command();
        let pid = std::process::id();
        tracing::warn!(
            transaction = self.name,
            held_ms = u64::try_from(hold.held.as_millis()).unwrap_or(u64::MAX),
            before_lock_ms = u64::try_from(hold.before_lock.as_millis()).unwrap_or(u64::MAX),
            rows = hold.rows,
            role = actor.role().as_str(),
            command,
            pid,
            "queue write lock held long"
        );
        if !self.record {
            return;
        }
        let payload = held_long_payload(self.name, hold, actor.role().as_str(), command, pid);
        let record = || -> Result<()> {
            let identity = self
                .identity
                .as_ref()
                .context("read transaction has no writer identity")?;
            let tx = WriteTx::write(self.conn, "queue_write_held_long", Duration::MAX, identity)?;
            record_queue_event_in(&tx, EventKind::QueueWriteHeldLong, &payload)?;
            tx.commit()?;
            Ok(())
        };
        if let Err(error) = record() {
            tracing::warn!(
                transaction = self.name,
                error = %format_args!("{error:#}"),
                "the long hold of a queue write lock could not be recorded: {error:#}"
            );
        }
    }
}

/// The repository's rules a planning command holds the queue to
/// ([`SqliteQueue::hold_planning_to`]): each `Some` sets that rule (to
/// none with `Some(None)`), each `None` leaves it as it is.
#[derive(Debug, Clone, Default)]
pub struct PlanningRules {
    pub changes: Option<Option<ChangeSet>>,
    pub goal_tags: Option<Option<TagSet>>,
    pub max_spike_cycles: Option<Option<usize>>,
    pub title_prefixes: Option<Option<TitlePrefixes>>,
    pub replacement: Option<Option<crate::domain::replan_plan::ReplacementPlan>>,
}

/// The connection of a [`SqliteQueue`]: its own, closed with it, or the one
/// a [`WriteConnection`] lent it, which goes back when the queue goes.
pub(super) struct QueueConnection {
    conn: Option<Connection>,
    lent_by: Option<(Arc<Mutex<WriteSlot>>, FileId)>,
}

impl std::ops::Deref for QueueConnection {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.conn
            .as_ref()
            .expect("a queue's connection until it drops")
    }
}

impl std::ops::DerefMut for QueueConnection {
    fn deref_mut(&mut self) -> &mut Connection {
        self.conn
            .as_mut()
            .expect("a queue's connection until it drops")
    }
}

impl Drop for QueueConnection {
    fn drop(&mut self) {
        if let (Some(conn), Some((slot, file))) = (self.conn.take(), self.lent_by.take()) {
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                WriteSlot::Idle(Box::new(IdleWrite { conn, file }));
        }
    }
}

/// The device and inode of the queue's DB: what tells a connection still on
/// the queue's file from one left on a file that was replaced.
type FileId = (u64, u64);

fn file_id(db: &Path) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    let file = std::fs::metadata(db).ok()?;
    Some((file.dev(), file.ino()))
}

/// The bytes of WAL a checkpoint leaves behind the write connection: it stays
/// open, so no last close removes a WAL a long read let grow.
const WRITE_WAL_LIMIT: i64 = 64 * 1024 * 1024;

/// The write connection between two of the write line's requests.
struct IdleWrite {
    conn: Connection,
    file: FileId,
}

/// Where the write connection is.
#[derive(Default)]
enum WriteSlot {
    /// Not opened yet.
    #[default]
    Empty,
    Idle(Box<IdleWrite>),
    Lent,
}

/// The one connection the queue service's write line writes on, lent to one
/// request at a time ([`SqliteQueue::write_connection`]). Each loan is a
/// fresh [`SqliteQueue`] (the rules and the actor of a request do not reach
/// the next), held to the same check as [`SqliteQueue::open`], so a queue a
/// `migrate` is still to bring up is refused as it was. A DB file that was
/// replaced, or a connection a request left in a transaction, is opened
/// again. A loan while another is out is refused: there is one connection,
/// and a request hands back what it holds before it opens the queue again.
///
/// The [`WriteIdentity`] of the first open is kept for all later loans on
/// that file and never read again: that is what lets the fence of
/// [`WriteTx::write`] refuse this service's writes once another takes the
/// row over.
pub struct WriteConnection {
    db: PathBuf,
    slot: Arc<Mutex<WriteSlot>>,
    identity: Mutex<Option<(FileId, WriteIdentity)>>,
}

impl WriteConnection {
    /// The queue on the write connection, until it is dropped.
    pub fn lend(&self) -> Result<SqliteQueue> {
        ensure_not_refused(&self.db)?;
        let file = file_id(&self.db);
        let idle = {
            let mut slot = self
                .slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ensure!(
                !matches!(*slot, WriteSlot::Lent),
                "the queue's write connection is out on another loan"
            );
            match std::mem::replace(&mut *slot, WriteSlot::Lent) {
                WriteSlot::Idle(idle) => Some(*idle),
                _ => None,
            }
        };
        // What fails from here leaves no connection out.
        let back = NotLent(&self.slot);
        let idle = idle.filter(|idle| Some(idle.file) == file && idle.conn.is_autocommit());
        let conn = match idle {
            Some(idle) => idle.conn,
            None => {
                let conn = SqliteQueue::open_connection(
                    &self.db,
                    OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?;
                conn.pragma_update(None, "journal_size_limit", WRITE_WAL_LIMIT)?;
                conn
            }
        };
        let file = file.context("the queue's DB was gone as its connection opened")?;
        let mut queue = SqliteQueue::on(
            QueueConnection {
                conn: Some(conn),
                lent_by: Some((self.slot.clone(), file)),
            },
            &self.db,
        )?;
        queue
            .state()?
            .context("queue is not initialized; use init first")?
            .check_opens()?;
        let mut kept = self
            .identity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match (&*kept, &queue.write_identity) {
            (Some((on, identity)), _) if *on == file => {
                queue.write_identity = Some(identity.clone());
            }
            (_, Some(identity)) => *kept = Some((file, identity.clone())),
            (_, None) => {}
        }
        drop(kept);
        std::mem::forget(back);
        Ok(queue)
    }
}

/// A loan that did not come to be: its slot is free again, unless the queue
/// it did make took its connection back there first.
struct NotLent<'a>(&'a Mutex<WriteSlot>);

impl Drop for NotLent<'_> {
    fn drop(&mut self) {
        let mut slot = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*slot, WriteSlot::Lent) {
            *slot = WriteSlot::Empty;
        }
    }
}

/// The local queue: shared state with SQLite's named transaction entries.
pub type SqliteQueue = super::store_queue::StoreQueue<SqliteEntries>;

pub use super::store_queue::DenialRecorder;

/// SQLite's connection and transaction policy. Shared queue state lives on
/// [`super::store_queue::StoreQueue`], independently of this adapter.
pub struct SqliteEntries {
    pub(super) conn: QueueConnection,
    /// The opening service's identity; never reread by an ordinary write.
    /// None only while opening a schema predating the leader table.
    pub(super) write_identity: Option<WriteIdentity>,
    /// How long a [`WriteTx`] may hold the write lock before it warns.
    pub(super) write_hold_warn: Duration,
}

// Keep connection access available to SQLite's adapter and existing tests,
// but never to a StoreQueue whose entries are still generic.
impl std::ops::Deref for SqliteQueue {
    type Target = SqliteEntries;

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl std::ops::DerefMut for SqliteQueue {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.entries
    }
}

impl super::sql_exec::TransactionEntries for SqliteEntries {
    type Write<'store> = WriteTx<'store>;
    type Read<'store> = WriteTx<'store>;

    fn write_entry(&self, name: &'static str) -> anyhow::Result<WriteTx<'_>> {
        Ok(WriteTx::write(
            &self.conn,
            name,
            self.write_hold_warn,
            self.write_identity
                .as_ref()
                .context("queue has no write leader identity")?,
        )?
        .recorded())
    }

    fn read_entry(&self, name: &'static str) -> anyhow::Result<WriteTx<'_>> {
        let mut tx = WriteTx::read(&self.conn, name, self.write_hold_warn)?.recorded();
        tx.identity = self.write_identity.clone();
        Ok(tx)
    }
}

impl SqliteQueue {
    /// Select the independent body store used by new event writes on this
    /// connection. Existing events and run files are never passed to it.
    pub fn with_event_bodies(
        self,
        bodies: std::sync::Arc<dyn crate::application::EventBodies>,
    ) -> Result<Self> {
        attach_event_bodies(&self.conn, bodies)?;
        Ok(self)
    }

    /// `user_version` a fully migrated queue reports.
    pub const SCHEMA_VERSION: i64 = BINARY_SCHEMA;

    /// Explicit initialization is the only operation that creates a database
    /// file, and the only one besides [`SqliteQueue::migrate`] that applies
    /// migrations: an empty file becomes a queue at the latest schema. An
    /// existing queue is checked as [`SqliteQueue::open`] checks it, never
    /// migrated (ADR-0045 decision 5).
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        let mut queue = Self::connect(path.as_ref(), true)?;
        queue.initialize()?;
        Ok(queue)
    }

    /// Make a queue at the latest schema where there is no file at `path`
    /// (ADR-t3102-2 decision 1: `init`'s only open of the DB): the file is
    /// created for this process alone, so one that is there, or comes
    /// between, is refused and left as it is, to the queue's service.
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| {
                format!(
                    "create the queue at {} (an existing queue is initialized by its service)",
                    path.display()
                )
            })?;
        let made = Self::connect(path, false).and_then(|mut queue| {
            queue.initialize()?;
            Ok(queue)
        });
        // A queue not made leaves no file the next `init` would take for one.
        if made.is_err() {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
        }
        made
    }

    /// Apply every migration to an empty queue, check one that is there,
    /// and turn on its WAL.
    fn initialize(&mut self) -> Result<()> {
        let queue = self;
        if queue.state()?.is_none() {
            queue.apply(0, None)?;
        }
        queue
            .state()?
            .context("queue is not initialized; use init first")?
            .check_opens()?;
        queue.write_identity = Some(WriteIdentity::read(&*queue.conn)?);
        queue.conn.pragma_update(None, "journal_mode", "WAL")?;
        Ok(())
    }

    /// Opens an initialized queue without changing its schema. A queue older
    /// than this binary is refused with a pointer to `dagq migrate`; a newer
    /// one is accepted while its floor is at most this binary's schema, and
    /// this binary then leaves the tables and columns it does not know alone
    /// (ADR-0045 decisions 5, 7).
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let queue = Self::connect(path.as_ref(), false)?;
        queue
            .state()?
            .context("queue is not initialized; use init first")?
            .check_opens()?;
        Ok(queue)
    }

    /// The write connection of the queue at `path`, which opens at its
    /// first loan ([`WriteConnection::lend`]).
    pub fn write_connection(path: impl AsRef<Path>) -> WriteConnection {
        WriteConnection {
            db: path.as_ref().to_path_buf(),
            slot: Arc::default(),
            identity: Mutex::new(None),
        }
    }

    /// Opens the live queue read-only for polling (`watch`): older schemas
    /// must be migrated first, as for [`Self::open_read_only`].
    pub fn open_watch(path: impl AsRef<Path>) -> Result<Self> {
        let queue = Self::connect_with(
            path.as_ref(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        queue
            .state()?
            .context("queue is not initialized; use init first")?
            .check_opens()?;
        Ok(queue)
    }

    /// Whether SQLite opened the queue database read-only.
    pub fn is_read_only(&self) -> Result<bool> {
        Ok(self.conn.is_readonly(rusqlite::MAIN_DB)?)
    }

    /// Opens an initialized queue for a command that only reads (`status`,
    /// `show`, `list`, `graph`, `stats` and the like, ADR-0045 decision 18):
    /// the connection is read-only, so opening writes no pragma, event or
    /// schema, and a write through it fails. A queue at this binary's schema
    /// or a newer one within the floor is read as it is; an older one is
    /// refused as [`Self::open`] refuses it, with nothing migrated, not even
    /// a copy in memory (ADR-t3102-2 decision 4: only the queue service
    /// reads the queue, and its build migrates it when asked).
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        match Self::inspect_read_only(path)?.1 {
            ReadOnlyQueue::Readable(queue) => Ok(queue),
            ReadOnlyQueue::Refused { error, .. } => Err(error),
        }
    }

    /// [`Self::open_read_only`] for `doctor`, which reports the schema even
    /// of a queue that refuses this binary (ADR-0045 decision 5): the
    /// queue's schema as [`Self::schema`] reports it, read on the same
    /// connection, and the queue, or the connection of one this binary
    /// does not read as it is (an older schema, or a floor that refuses
    /// it) with the reason. The file is opened once either way.
    pub fn inspect_read_only(path: impl AsRef<Path>) -> Result<(SchemaState, ReadOnlyQueue)> {
        let queue = Self::connect_with(
            path.as_ref(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let state = queue
            .state()?
            .context("queue is not initialized; use init first")?;
        let schema = state.report();
        Ok(match state.check_opens() {
            Ok(()) => (schema, ReadOnlyQueue::Readable(queue)),
            Err(error) => (
                schema,
                ReadOnlyQueue::Refused {
                    binding: queue,
                    error,
                },
            ),
        })
    }

    /// The schema of the queue at `path` as this binary sees it, without
    /// changing it.
    pub fn schema(path: impl AsRef<Path>) -> Result<SchemaState> {
        let queue = Self::connect_with(
            path.as_ref(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let state = queue
            .state()?
            .context("queue is not initialized; use init first")?;
        Ok(state.report())
    }

    /// The Git common directory the queue at `path` is bound to, read on a
    /// read-only connection of the file as it is, whatever its schema
    /// within the binary's: a service's replacement checks it before it
    /// takes over, with nothing migrated, even in memory.
    pub fn binding_at(path: impl AsRef<Path>) -> Result<Option<String>> {
        let queue = Self::connect_with(
            path.as_ref(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        queue.repository_binding()
    }

    /// Apply the migrations this binary knows and the queue at `path`
    /// lacks when every one of them is compatible, as the start of `up`
    /// does (ADR-0045 decisions 5, 15): a supervisor and the wrappers of an
    /// older binary go on with the migrated queue. A breaking one is refused
    /// ([`SchemaState::refuse_breaking`]); `None` when there is no queue at
    /// `path` yet or nothing is pending.
    pub fn migrate_compatible(
        path: &Path,
        alive: Option<&dyn Fn(u32) -> bool>,
        now: i64,
    ) -> Result<Option<MigrationReport>> {
        if !path.is_file() {
            return Ok(None);
        }
        let state = Self::schema(path)?;
        if state.pending.is_empty() {
            return Ok(None);
        }
        state.refuse_breaking()?;
        Self::migrate(path, alive, now).map(Some)
    }

    /// Apply the migrations this binary knows and the queue lacks, with
    /// `user_version` and the floor in one transaction: what `dagq migrate`
    /// and a service a replacement starts apply (ADR-t3102-2 decision 2).
    /// Before a breaking migration it refuses a queue in use — a
    /// registration of a live supervisor, an unfinished run, a live wrapper
    /// ([`queue_users`], liveness per `alive`; `None` skips the check, for
    /// tests and tools that know the queue is idle) — because their binaries
    /// could not open the queue afterwards, and copies the database to
    /// `backups/` next to it (ADR-0045 decisions 8, 9): the check, the copy
    /// and the migrations under the one write lock of the transaction, so
    /// no claim or registration comes between them. `now` (UNIX seconds)
    /// names the copy.
    pub fn migrate(
        path: impl AsRef<Path>,
        alive: Option<&dyn Fn(u32) -> bool>,
        now: i64,
    ) -> Result<MigrationReport> {
        let path = path.as_ref();
        let mut queue = Self::connect(path, false)?;
        let state = queue
            .state()?
            .context("queue is not initialized; use init first")?;
        state.check_floor()?;
        let mut applied_from = state.version;
        let backup = if pending_migrations(state.version).is_empty() {
            None
        } else {
            let (backup, version) = queue.apply(
                state.version,
                Some(&BreakingGuard {
                    alive,
                    db: path,
                    now,
                }),
            )?;
            applied_from = version;
            backup
        };
        let after = queue
            .state()?
            .context("queue is not initialized; use init first")?;
        Ok(MigrationReport {
            previous_version: state.version,
            schema_version: after.version,
            binary_schema_version: BINARY_SCHEMA,
            floor: after.floor,
            applied: pending_migrations(applied_from),
            backup,
        })
    }

    /// Refuse, reading the queue at `path` alone, the breaking migrations
    /// this binary would apply to it while it is in use ([`queue_users`]):
    /// the look before a replacement's service takes over, which
    /// [`Self::migrate`] repeats in its transaction.
    pub fn refuse_breaking_in_use(path: &Path, alive: &dyn Fn(u32) -> bool) -> Result<()> {
        let queue = Self::connect_with(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let state = queue
            .state()?
            .context("queue is not initialized; use init first")?;
        let pending = pending_migrations(state.version);
        if pending.iter().all(|m| m.compatible) {
            return Ok(());
        }
        refuse_in_use(&queue_use(&queue.conn)?, alive, &pending)
    }

    /// The queue reading the time and creating IDs through `generators`
    /// instead of the system clock and random UUIDs.
    pub fn with_generators(mut self, generators: Generators) -> Self {
        self.generators = generators;
        self
    }

    /// The queue holding the tasks to the repository's set of changes
    /// (ADR-t980-1); `None` holds them to none.
    pub fn with_changes(mut self, changes: Option<ChangeSet>) -> Self {
        self.changes = changes;
        self
    }

    /// The queue holding the tasks' titles to the repository's prefixes;
    /// `None` holds them to none.
    pub fn with_title_prefixes(mut self, prefixes: Option<TitlePrefixes>) -> Self {
        self.title_prefixes = prefixes;
        self
    }

    /// The queue holding the goals to the repository's set of tags
    /// (ADR-t1639-1 decision 6); `None` holds them to none.
    /// With `[goals] max_spike_cycles` (ADR-t1487-1 decision 8).
    pub fn with_max_spike_cycles(mut self, max: Option<usize>) -> Self {
        self.max_spike_cycles = max;
        self
    }

    /// The queue whose next `submit` goes with the replacement plan `plan`
    /// of a replanning operation's request (ADR-t1521-2 decision 4).
    pub fn with_replacement_plan(
        mut self,
        plan: Option<crate::domain::replan_plan::ReplacementPlan>,
    ) -> Self {
        self.replacement = plan;
        self
    }

    pub fn with_goal_tags(mut self, tags: Option<TagSet>) -> Self {
        self.goal_tags = tags;
        self
    }

    /// [`Self::with_changes`], [`Self::with_goal_tags`],
    /// [`Self::with_max_spike_cycles`], [`Self::with_title_prefixes`] and
    /// [`Self::with_replacement_plan`] on
    /// a queue held by reference, each set only when given: the rules a
    /// planning command holds its tasks and goals to.
    pub fn hold_planning_to(&mut self, rules: PlanningRules) {
        if let Some(changes) = rules.changes {
            self.changes = changes;
        }
        if let Some(tags) = rules.goal_tags {
            self.goal_tags = tags;
        }
        if let Some(max) = rules.max_spike_cycles {
            self.max_spike_cycles = max;
        }
        if let Some(prefixes) = rules.title_prefixes {
            self.title_prefixes = prefixes;
        }
        if let Some(plan) = rules.replacement {
            self.replacement = plan;
        }
    }

    /// The queue warning of, and recording, a [`WriteTx`] that held the
    /// write lock past `threshold` instead of [`WRITE_HOLD_WARN`].
    pub fn with_write_hold_warn(mut self, threshold: Duration) -> Self {
        self.write_hold_warn = threshold;
        self
    }

    /// Record the refusals of the policy with `log`: for a read-only
    /// connection, which cannot write them.
    pub fn with_denial_log(mut self, log: DenialRecorder) -> Self {
        self.denial_log = Some(log);
        self
    }

    /// Use the identity acquired by the service that opens this store.
    /// Startup owns takeover; ordinary writes retain this value even after
    /// another service advances the generation.
    pub fn with_write_identity(mut self, identity: WriteIdentity) -> Self {
        self.write_identity = Some(identity);
        self
    }

    /// A transaction that writes, named `name` on this connection
    /// ([`WriteTx::write`], `BEGIN IMMEDIATE`), which records its long hold
    /// on this connection.
    pub fn write_tx(&self, name: &'static str) -> Result<WriteTx<'_>> {
        Ok(WriteTx::write(
            &self.conn,
            name,
            self.write_hold_warn,
            self.write_identity
                .as_ref()
                .context("queue has no write leader identity")?,
        )?
        .recorded())
    }

    /// A single-statement write still uses the named, fenced transaction
    /// entry, with exactly the statement's affected-row result.
    pub(super) fn write_statement<P: rusqlite::Params>(
        &self,
        name: &'static str,
        sql: &str,
        params: P,
    ) -> Result<usize> {
        let tx = self.write_tx(name)?;
        let changed = tx.execute(sql, params)?;
        tx.commit()?;
        Ok(changed)
    }

    /// [`Self::write_statement`] with the arguments of the execution port.
    pub(super) fn write_sql(
        &self,
        name: &'static str,
        sql: &str,
        params: &[super::sql_exec::SqlValue],
    ) -> Result<usize> {
        let tx = self.write_tx(name)?;
        let changed = super::sql_exec::SqlWrite::execute(&tx, sql, params)?;
        tx.commit()?;
        Ok(changed)
    }

    /// Read transcripts through the session execution port before a write
    /// entry holds its lock.
    pub(super) fn read_session_transcripts_before(
        &self,
        closing: super::sessions::Closing<'_>,
    ) -> Result<super::sessions::ReadBefore> {
        self.read_before(closing)
    }

    /// A transaction that reads one snapshot, named `name` on this
    /// connection ([`WriteTx::read`], deferred), which records a long hold
    /// of a write in it on this connection.
    pub fn read_tx(&self, name: &'static str) -> Result<WriteTx<'_>> {
        let mut tx = WriteTx::read(&self.conn, name, self.write_hold_warn)?.recorded();
        tx.identity = self.write_identity.clone();
        Ok(tx)
    }

    pub fn generators(&self) -> &Generators {
        &self.generators
    }

    /// The queue writing its events as `actor` instead of the process's
    /// actor ([`process_actor`]).
    pub fn with_actor(self, actor: ActorContext) -> Self {
        self.actors.set(actor);
        self
    }

    /// The actor this queue's events record.
    pub fn actor(&self) -> ActorContext {
        self.actors.get()
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    fn connect(path: &Path, create: bool) -> Result<Self> {
        let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        if create {
            flags |= OpenFlags::SQLITE_OPEN_CREATE;
        }
        Self::connect_with(path, flags)
    }

    /// Wait `timeout` for another connection's write lock instead of
    /// [`crate::application::QUEUE_BUSY_TIMEOUT`].
    pub fn set_busy_timeout(&self, timeout: Duration) -> Result<()> {
        Ok(self.conn.busy_timeout(timeout)?)
    }

    fn connect_with(path: &Path, flags: OpenFlags) -> Result<Self> {
        Self::on(
            QueueConnection {
                conn: Some(Self::open_connection(path, flags)?),
                lent_by: None,
            },
            path,
        )
    }

    /// A connection to the queue at `path`, set up as every queue's is.
    fn open_connection(path: &Path, flags: OpenFlags) -> Result<Connection> {
        ensure_not_refused(path)?;
        let conn = Connection::open_with_flags(path, flags)
            .with_context(|| format!("open queue at {} (use init to create it)", path.display()))?;
        conn.busy_timeout(crate::application::QUEUE_BUSY_TIMEOUT)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        // The `worktime.jsonl` lines of the session closes written through
        // it follow its commits (task 1334).
        super::sql_exec::sqlite::watch_commits(&conn)?;
        Ok(conn)
    }

    /// The queue at `path` on `conn`, with the actor of this process.
    fn on(conn: QueueConnection, path: &Path) -> Result<Self> {
        // Canonical, like the paths `supervise` plans under, so a relative or
        // symlinked `--db` still names the queue's real `runs/`.
        let db = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let actors = attach_event_actors(&conn, process_actor())?;
        attach_event_bodies(
            &conn,
            std::sync::Arc::new(super::event_bodies::InlineEventBodies),
        )?;
        let write_identity = has_table(&conn, "write_leader")?
            .then(|| WriteIdentity::read(&*conn))
            .transpose()?;
        Ok(Self {
            entries: SqliteEntries {
                conn,
                write_identity,
                write_hold_warn: WRITE_HOLD_WARN,
            },
            runs_dir: runs_dir(&db),
            generators: clock::system(),
            actors,
            changes: None,
            title_prefixes: None,
            goal_tags: None,
            max_spike_cycles: None,
            replacement: None,
            denial_log: None,
        })
    }

    /// The queue's `user_version` and floor; `None` for an empty file that
    /// `init` may turn into a queue. Another application's database, or a
    /// file with tables but no dagq header, is an error.
    fn state(&self) -> Result<Option<QueueSchema>> {
        let app: i64 = self
            .conn
            .pragma_query_value(None, "application_id", |r| r.get(0))?;
        let version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if app == 0 && version == 0 {
            let objects: i64 = self.conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )?;
            ensure!(objects == 0, "database is not an empty dagq queue");
            return Ok(None);
        }
        ensure!(app == APPLICATION_ID, "database is not a dagq queue");
        ensure!(version >= 1, "unsupported queue schema version {version}");
        // Read before judging user_version: the floor, not the version,
        // decides whether this binary may use a newer queue.
        let floor = if has_table(&self.conn, "schema_floor")? {
            self.conn
                .query_row("SELECT floor FROM schema_floor", [], |r| r.get(0))
                .optional()?
                .unwrap_or(version)
        } else {
            version
        };
        Ok(Some(QueueSchema { version, floor }))
    }

    /// Apply the migrations after `from` in one transaction; before a
    /// breaking one, what `guard` asks for in the same transaction: the
    /// copy it took, if any.
    fn apply(
        &mut self,
        from: i64,
        guard: Option<&BreakingGuard>,
    ) -> Result<(Option<PathBuf>, i64)> {
        // Table rebuilds drop and rename tables that other rows reference, so
        // enforcement is off during migration (a no-op inside a transaction)
        // and integrity is checked explicitly before commit.
        self.conn.pragma_update(None, "foreign_keys", false)?;
        let result = self.apply_migrations(from, guard);
        self.conn.pragma_update(None, "foreign_keys", true)?;
        result
    }

    /// Read the current holder outside BEGIN and retry if it moved before
    /// the first UPDATE. The callback is empty in production; adapter tests
    /// interleave a takeover here without threads, waits or the real clock.
    fn migration_tx(&self, mut after_read: impl FnMut() -> Result<()>) -> Result<WriteTx<'_>> {
        loop {
            let identity = has_table(&self.conn, "write_leader")?
                .then(|| WriteIdentity::read(&*self.conn))
                .transpose()?;
            after_read()?;
            let tx = WriteTx::leader(&self.conn, "schema_migrations", self.write_hold_warn)?;
            if let Some(identity) = identity
                && !super::write_leader::lock_migration(&tx, &identity)?
            {
                drop(tx);
                continue;
            }
            // Before the leader table exists, bootstrap must create it in
            // this transaction. All later migrations lock its current row.
            return Ok(tx);
        }
    }

    fn apply_migrations(
        &mut self,
        from: i64,
        guard: Option<&BreakingGuard>,
    ) -> Result<(Option<PathBuf>, i64)> {
        // Warns of a long hold but records none: the queue's events are
        // written at the schema this transaction changes, which a rolled
        // back or an empty queue does not have.
        let tx = self.migration_tx(|| Ok(()))?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if from == 0 && version != 0 {
            // Another `init` created the queue first; the caller checks it.
            return Ok((None, version));
        }
        if version >= BINARY_SCHEMA {
            return Ok((None, version));
        }
        ensure!(
            version >= from,
            "queue schema changed from {from} to {version} while migrating; run migrate again"
        );
        let pending = pending_migrations(version);
        let mut backup = None;
        if let Some(guard) = guard.filter(|_| pending.iter().any(|m| !m.compatible)) {
            if let Some(alive) = guard.alive {
                refuse_in_use(&queue_use(&tx)?, alive, &pending)?;
            }
            backup = Some(back_up(guard.db, version, guard.now)?);
        }
        for (index, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            tx.execute_batch(migration)
                .with_context(|| format!("apply queue migration {}", index + 1))?;
            tx.pragma_update(None, "user_version", (index + 1) as i64)?;
        }
        tx.execute(
            "INSERT INTO schema_floor(singleton, floor) VALUES (1, ?1)
             ON CONFLICT(singleton) DO UPDATE SET floor = excluded.floor",
            [schema::recorded_floor(BINARY_SCHEMA)?],
        )?;
        tx.pragma_update(None, "application_id", APPLICATION_ID)?;
        let violations: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })?;
        ensure!(
            violations == 0,
            "queue migration would break {violations} foreign key references"
        );
        tx.commit()?;
        Ok((backup, version))
    }
}

/// What [`SqliteQueue::migrate`] does before a breaking migration, in the
/// transaction that applies it.
struct BreakingGuard<'a> {
    /// Liveness of the queue's users; `None` checks none.
    alive: Option<&'a dyn Fn(u32) -> bool>,
    /// The queue's database, copied first.
    db: &'a Path,
    /// What names the copy.
    now: i64,
}

/// Copy the queue at `db`, at schema `version`, to `backups/` next to it,
/// through a connection of its own: called while the migrating transaction
/// holds the write lock and has written nothing, so the copy is the queue
/// the check judged.
fn back_up(db: &Path, version: i64, now: i64) -> Result<PathBuf> {
    let dir = db
        .canonicalize()
        .unwrap_or_else(|_| db.to_path_buf())
        .with_file_name("backups");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    // A rerun within the same second keeps the earlier copy.
    let copy = (0..)
        .map(|n| {
            let suffix = if n == 0 {
                String::new()
            } else {
                format!("-{n}")
            };
            dir.join(format!("queue-{version}-{now}{suffix}.sqlite3"))
        })
        .find(|copy| !copy.exists())
        .expect("an unused backup name");
    let reader = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {} to back it up", db.display()))?;
    reader
        .execute("VACUUM INTO ?1", [path_text(&copy)?])
        .with_context(|| format!("back up the queue to {}", copy.display()))?;
    Ok(copy)
}

/// A queue's schema as its header and floor table record it.
struct QueueSchema {
    version: i64,
    floor: i64,
}

impl QueueSchema {
    /// Refuses a queue that refuses binaries of this binary's schema.
    fn check_floor(&self) -> Result<()> {
        ensure!(
            self.floor <= BINARY_SCHEMA,
            "unsupported queue schema version {}: the queue refuses binaries older than schema {} \
             and this binary knows schema {BINARY_SCHEMA}; install a newer dagq",
            self.version,
            self.floor
        );
        Ok(())
    }

    /// Whether this binary may use the queue as it is.
    fn check_opens(&self) -> Result<()> {
        self.check_floor()?;
        ensure!(
            self.version >= BINARY_SCHEMA,
            "queue schema version {} is older than this binary's schema {BINARY_SCHEMA}; \
             run `dagq migrate` with the queue's binary to apply the {} pending migration(s)",
            self.version,
            BINARY_SCHEMA - self.version
        );
        Ok(())
    }

    fn report(&self) -> SchemaState {
        SchemaState {
            schema_version: self.version,
            binary_schema_version: BINARY_SCHEMA,
            floor: self.floor,
            pending: pending_migrations(self.version),
            opens: self.check_opens().is_ok(),
        }
    }
}

/// A queue opened read-only by [`SqliteQueue::inspect_read_only`].
pub enum ReadOnlyQueue {
    /// A queue this binary reads as it is.
    Readable(SqliteQueue),
    /// A queue older than this binary's schema, or whose floor refuses
    /// this binary: `binding` reads only its
    /// repository binding ([`SqliteQueue::assert_repository`]), and `error`
    /// is what [`SqliteQueue::open_read_only`] fails with.
    Refused {
        binding: SqliteQueue,
        error: anyhow::Error,
    },
}

/// A queue's schema as `migrate --check` and `doctor` report it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SchemaState {
    pub schema_version: i64,
    /// The schema this binary knows.
    pub binary_schema_version: i64,
    /// Binaries knowing an older schema than this are refused.
    pub floor: i64,
    /// What `migrate` would apply.
    pub pending: Vec<SchemaMigration>,
    /// Whether this binary's other commands open the queue as it is.
    pub opens: bool,
}

impl SchemaState {
    /// The schema of a queue at `version` whose floor is `floor`, as this
    /// binary sees it: what a client reports of the facts its queue's
    /// service read (`migrate --check`), which may be of another build.
    pub fn of(version: i64, floor: i64) -> Self {
        QueueSchema { version, floor }.report()
    }

    /// Whether the queue's floor refuses this binary, so that not even the
    /// commands that only read open it.
    pub fn refuses_binary(&self) -> bool {
        self.floor > self.binary_schema_version
    }

    /// Refuse the queue when its floor refuses this binary, with the error
    /// [`SqliteQueue::open`] gives.
    pub fn check_floor(&self) -> Result<()> {
        QueueSchema {
            version: self.schema_version,
            floor: self.floor,
        }
        .check_floor()
    }

    /// Refuse the queue unless this binary's other commands open it as it
    /// is, with the error [`SqliteQueue::open`] gives.
    pub fn check_opens(&self) -> Result<()> {
        QueueSchema {
            version: self.schema_version,
            floor: self.floor,
        }
        .check_opens()
    }

    /// Refuse the pending migrations when any of them is breaking (ADR-0045
    /// decisions 5, 15): a supervisor or run of the older binary could not
    /// open the queue afterwards, so `up` and `install` apply only
    /// compatible ones, and the error says the way to a breaking one.
    pub fn refuse_breaking(&self) -> Result<()> {
        let breaking: Vec<String> = self
            .pending
            .iter()
            .filter(|migration| !migration.compatible)
            .map(|migration| migration.version.to_string())
            .collect();
        ensure!(
            breaking.is_empty(),
            "the queue needs breaking migration(s) {} before this binary can run it, and a \
supervisor or run of the older binary could not open it afterwards: stop the supervisor \
(`down --wait`), run `dagq migrate`, then `up`; or let `dagq install --allow-breaking` do the \
same in one step",
            breaking.join(", ")
        );
        Ok(())
    }
}

/// One migration of this binary and its declaration.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SchemaMigration {
    pub version: i64,
    pub compatible: bool,
}

/// What `migrate` did.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MigrationReport {
    pub previous_version: i64,
    pub schema_version: i64,
    pub binary_schema_version: i64,
    pub floor: i64,
    pub applied: Vec<SchemaMigration>,
    /// The copy taken before a breaking migration.
    pub backup: Option<PathBuf>,
}

/// The migrations this binary knows beyond `version`.
fn pending_migrations(version: i64) -> Vec<SchemaMigration> {
    MIGRATIONS
        .iter()
        .enumerate()
        .skip(usize::try_from(version).unwrap_or(0))
        .map(|(index, migration)| SchemaMigration {
            version: index as i64 + 1,
            compatible: schema::is_compatible(migration),
        })
        .collect()
}

fn has_table(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Refuses a breaking migration while anything that runs an older binary
/// uses the queue: a registered supervisor whose PID is alive, an unfinished
/// run, a wrapper whose PID is alive (ADR-0045 decision 8). Reads only what
/// every schema since the tables were added has, so it runs on the old schema.
/// What uses a queue as a breaking migration judges it: the registered
/// supervisors (token, pid), the runs not finished (id, status) and the
/// wrappers not exited (run, pid).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct QueueUse {
    supervisors: Vec<(String, i64)>,
    runs: Vec<(String, String)>,
    wrappers: Vec<(String, i64)>,
}

/// The statuses of a run whose wrapper or supervisor uses the queue.
const RUNS_IN_USE: [&str; 5] = [
    "claimed",
    "starting",
    "running",
    "validating",
    "integrating",
];

fn queue_use(conn: &Connection) -> Result<QueueUse> {
    let mut found = QueueUse::default();
    if has_table(conn, "supervisors")? {
        let mut rows = conn.prepare("SELECT token, pid FROM supervisors ORDER BY token")?;
        for row in rows.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))? {
            found.supervisors.push(row?);
        }
    }
    if has_table(conn, "task_runs")? {
        let mut rows = conn.prepare(&format!(
            "SELECT id, status FROM task_runs WHERE status IN ({}) ORDER BY {}",
            RUNS_IN_USE.map(|status| format!("'{status}'")).join(","),
            crate::infrastructure::sql_dialect::SqlDialect::Sqlite.insertion_order("")
        ))?;
        for row in rows.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))? {
            found.runs.push(row?);
        }
    }
    if has_table(conn, "run_processes")? {
        let mut rows = conn.prepare(
            "SELECT run_id, pid FROM run_processes
             WHERE role='wrapper' AND exited_at IS NULL ORDER BY run_id",
        )?;
        for row in rows.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))? {
            found.wrappers.push(row?);
        }
    }
    Ok(found)
}

/// Who of `found` uses the queue, liveness per `alive`: a supervisor or a
/// wrapper whose process lives, and every run not finished (its wrapper
/// may be on another host's view of the pids).
fn queue_users(found: &QueueUse, alive: &dyn Fn(u32) -> bool) -> Vec<String> {
    let lives = |pid: i64| u32::try_from(pid).is_ok_and(alive);
    let supervisors = found
        .supervisors
        .iter()
        .filter(|(_, pid)| lives(*pid))
        .map(|(token, pid)| format!("supervisor {token} (pid {pid})"));
    let runs = found
        .runs
        .iter()
        .filter(|(_, status)| RUNS_IN_USE.contains(&status.as_str()))
        .map(|(id, status)| format!("run {id} ({status})"));
    let wrappers = found
        .wrappers
        .iter()
        .filter(|(_, pid)| lives(*pid))
        .map(|(run, pid)| format!("wrapper of run {run} (pid {pid})"));
    supervisors.chain(runs).chain(wrappers).collect()
}

/// Refuse the breaking ones of `pending` while [`queue_users`] finds the
/// queue in use.
fn refuse_in_use(
    found: &QueueUse,
    alive: &dyn Fn(u32) -> bool,
    pending: &[SchemaMigration],
) -> Result<()> {
    let users = queue_users(found, alive);
    if users.is_empty() {
        return Ok(());
    }
    let breaking: Vec<String> = pending
        .iter()
        .filter(|m| !m.compatible)
        .map(|m| m.version.to_string())
        .collect();
    anyhow::bail!(
        "migrate refuses breaking migration(s) {} while the queue is in use by {}: their binaries \
         could not open the queue afterwards; stop the supervisor with `down --wait`, let the \
         runs finish or `recover` them, then migrate again",
        breaking.join(", "),
        users.join(", ")
    )
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("path is not UTF-8: {}", path.display()))
}

/// The planning commands' changes of a task (task 1609): each refuses,
/// before it writes, a task whose status, proposal, proposal status or
/// proposal owner in its transaction is not `authorized`, what the caller
/// authorized it with. The [`TaskStore`] methods of the same names pass
/// `None` and check nothing more.
impl SqliteQueue {
    /// Configuration and execution transaction of a shared store operation.
    fn store<'a, T>(&'a self, tx: &'a T) -> super::task_store::Store<'a, T> {
        super::task_store::Store {
            tx,
            runs_dir: &self.runs_dir,
            generators: &self.generators,
            changes: &self.changes,
            title_prefixes: &self.title_prefixes,
            goal_tags: &self.goal_tags,
        }
    }
}

impl SqliteQueue {
    pub(super) fn transition_authorized(
        &mut self,
        task_id: TaskId,
        action: TaskAction,
        authorized: Option<&TaskAuthorization>,
    ) -> Result<Task> {
        let tx = self.write_tx("transition_authorized")?;
        let result = self
            .store(&tx)
            .transition_authorized(task_id, action, authorized)
            .map_err(native_error)?;
        tx.commit()?;
        Ok(result)
    }

    pub(super) fn cancel_authorized(
        &mut self,
        task_id: TaskId,
        cancel: &NewCancel,
        authorized: Option<&TaskAuthorization>,
    ) -> Result<Task> {
        let tx = self.write_tx("cancel_authorized")?;
        let result = self
            .store(&tx)
            .cancel_authorized(task_id, cancel, authorized)
            .map_err(native_error)?;
        tx.commit()?;
        Ok(result)
    }

    pub(super) fn add_dependency_authorized(
        &mut self,
        task_id: TaskId,
        predecessor_id: TaskId,
        authorized: Option<&TaskAuthorization>,
    ) -> Result<()> {
        let tx = self.write_tx("add_dependency_authorized")?;
        self.store(&tx)
            .add_dependency_authorized(task_id, predecessor_id, authorized)
            .map_err(native_error)?;
        tx.commit()?;
        Ok(())
    }

    pub(super) fn remove_dependency_authorized(
        &mut self,
        task_id: TaskId,
        predecessor_id: TaskId,
        authorized: Option<&TaskAuthorization>,
    ) -> Result<()> {
        let tx = self.write_tx("remove_dependency_authorized")?;
        self.store(&tx)
            .remove_dependency_authorized(task_id, predecessor_id, authorized)
            .map_err(native_error)?;
        tx.commit()?;
        Ok(())
    }

    pub(super) fn add_goal_dependency_authorized(
        &mut self,
        task_id: TaskId,
        goal_id: GoalId,
        authorized: Option<&TaskAuthorization>,
    ) -> Result<()> {
        let tx = self.write_tx("add_goal_dependency_authorized")?;
        self.store(&tx)
            .add_goal_dependency_authorized(task_id, goal_id, authorized)
            .map_err(native_error)?;
        tx.commit()?;
        Ok(())
    }

    pub(super) fn remove_goal_dependency_authorized(
        &mut self,
        task_id: TaskId,
        goal_id: GoalId,
        authorized: Option<&TaskAuthorization>,
    ) -> Result<()> {
        let tx = self.write_tx("remove_goal_dependency_authorized")?;
        self.store(&tx)
            .remove_goal_dependency_authorized(task_id, goal_id, authorized)
            .map_err(native_error)?;
        tx.commit()?;
        Ok(())
    }

    pub(super) fn set_goal_authorized(
        &mut self,
        task_id: TaskId,
        goal_id: Option<GoalId>,
        authorized: Option<&TaskAuthorization>,
    ) -> Result<Task> {
        let tx = self.write_tx("set_goal_authorized")?;
        let result = self
            .store(&tx)
            .set_goal_authorized(task_id, goal_id, authorized)
            .map_err(native_error)?;
        tx.commit()?;
        Ok(result)
    }

    pub(super) fn set_paths_authorized(
        &mut self,
        task_id: TaskId,
        paths: Vec<String>,
        authorized: Option<&TaskAuthorization>,
    ) -> Result<Task> {
        // Checked before the task is read, so a bad glob is reported first.
        validate_path_globs(&paths)?;

        let tx = self.write_tx("set_paths_authorized")?;
        let result = self
            .store(&tx)
            .set_paths_authorized(task_id, paths, authorized)
            .map_err(native_error)?;
        tx.commit()?;
        Ok(result)
    }

    pub(super) fn set_priority_authorized(
        &mut self,
        task_id: TaskId,
        priority: Option<Priority>,
        authorized: Option<&TaskAuthorization>,
    ) -> Result<Task> {
        let tx = self.write_tx("set_priority_authorized")?;
        let result = self
            .store(&tx)
            .set_priority_authorized(task_id, priority, authorized)
            .map_err(native_error)?;
        tx.commit()?;
        Ok(result)
    }

    /// [`TaskStore::edit_task`], refusing too a task whose proposal, its
    /// status or its owner is not `full`'s (`None` checks the status only).
    pub(super) fn edit_task_authorized(
        &mut self,
        task_id: TaskId,
        edit: TaskEdit,
        authorized: TaskStatus,
        full: Option<&TaskAuthorization>,
    ) -> Result<Task> {
        ensure!(!edit.is_empty(), "task edit changes nothing");
        // Checked before the task is read, so a bad value is reported first.
        edit.validate()?;
        if let (Some(changes), Some(change)) = (&self.changes, &edit.change) {
            changes.check(change)?;
        }

        let tx = self.write_tx("edit_task_authorized")?;
        let result = self
            .store(&tx)
            .edit_task_authorized(task_id, edit, authorized, full)
            .map_err(native_error)?;
        tx.commit()?;
        Ok(result)
    }
}

impl TaskStore for SqliteQueue {
    fn add(&mut self, new: NewTask) -> Result<Task> {
        // Rejected before taking the write lock; `new` checks it again.
        new.validate()?;
        if let (Some(changes), Some(change)) = (&self.changes, &new.change) {
            changes.check(change)?;
        }

        let tx = self.write_tx("task_add")?;
        let result = self.store(&tx).add(new).map_err(native_error)?;
        tx.commit()?;
        Ok(result)
    }

    fn transition(&mut self, task_id: TaskId, action: TaskAction) -> Result<Task> {
        self.transition_authorized(task_id, action, None)
    }

    fn cancel(&mut self, task_id: TaskId, cancel: NewCancel) -> Result<Task> {
        self.cancel_authorized(task_id, &cancel, None)
    }

    fn add_dependency(&mut self, task_id: TaskId, predecessor_id: TaskId) -> Result<()> {
        self.add_dependency_authorized(task_id, predecessor_id, None)
    }

    fn remove_dependency(&mut self, task_id: TaskId, predecessor_id: TaskId) -> Result<()> {
        self.remove_dependency_authorized(task_id, predecessor_id, None)
    }

    fn add_goal_dependency(&mut self, task_id: TaskId, goal_id: GoalId) -> Result<()> {
        self.add_goal_dependency_authorized(task_id, goal_id, None)
    }

    fn remove_goal_dependency(&mut self, task_id: TaskId, goal_id: GoalId) -> Result<()> {
        self.remove_goal_dependency_authorized(task_id, goal_id, None)
    }

    fn investigations(&mut self) -> Result<crate::domain::investigation::Investigations> {
        let tx = self.write_tx("investigations")?;
        let investigations = super::investigations::refresh(&tx, self.generators.clock.now())?;
        tx.commit()?;
        Ok(investigations)
    }

    fn ready_at(&self, tasks: &[TaskId]) -> Result<std::collections::HashMap<TaskId, i64>> {
        let tx = self.read_tx("ready_at")?;
        let result = self.store(&tx).ready_at(tasks).map_err(native_error)?;
        Ok(result)
    }

    fn claim(&mut self, base_commit: &CommitSha) -> Result<ClaimOutcome> {
        let tx = self.write_tx("task_claim")?;
        let result = self.store(&tx).claim(base_commit).map_err(native_error)?;
        tx.commit()?;
        Ok(result)
    }

    fn add_goal(&mut self, new: NewGoal) -> Result<Goal> {
        // Rejected before taking the write lock; `new` checks it again.
        new.validate()?;
        if let Some(tags) = &self.goal_tags {
            tags.check(&new.tags)?;
        }

        let tx = self.write_tx("add_goal")?;
        let result = self.store(&tx).add_goal(new).map_err(native_error)?;
        tx.commit()?;
        Ok(result)
    }

    /// Edit a goal and record `goal_updated` with the goal before and after,
    /// and the goal's acceptance version before (`old_acceptance_version`)
    /// and after (`new_acceptance_version`) the edit, which differ only when
    /// the acceptance text changed.
    fn edit_goal(&mut self, goal_id: GoalId, edit: GoalEdit) -> Result<Goal> {
        ensure!(!edit.is_empty(), "goal edit changes nothing");
        if let (Some(set), Some(tags)) = (&self.goal_tags, &edit.tags) {
            set.check(tags)?;
        }

        let tx = self.write_tx("edit_goal")?;
        let result = self
            .store(&tx)
            .edit_goal(goal_id, edit)
            .map_err(native_error)?;
        tx.commit()?;
        Ok(result)
    }

    fn close_goal(&mut self, goal_id: GoalId, verdict: GoalVerdict) -> Result<Goal> {
        let tx = self.write_tx("close_goal")?;
        let result = self
            .store(&tx)
            .close_goal(goal_id, verdict)
            .map_err(native_error)?;
        tx.commit()?;
        Ok(result)
    }

    fn submit(&mut self, submission: Submission) -> Result<Proposal> {
        self.submit_linking(submission, &[])
    }

    fn approve_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal> {
        let tx = self.write_tx("approve_proposal")?;
        let proposal = proposals::approve(&tx, proposal_id, &self.generators.clock.timestamp())?;
        tx.commit()?;
        Ok(proposal)
    }

    fn send_back_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal> {
        let tx = self.write_tx("send_back_proposal")?;
        let proposal = proposals::send_back(&tx, proposal_id, &self.generators.clock.timestamp())?;
        tx.commit()?;
        Ok(proposal)
    }

    fn withdraw_proposal(&mut self, proposal_id: ProposalId) -> Result<Proposal> {
        let tx = self.write_tx("withdraw_proposal")?;
        let proposal = proposals::withdraw(
            &tx,
            proposal_id,
            &self.generators.clock.timestamp(),
            self.generators.clock.now(),
        )?;
        tx.commit()?;
        Ok(proposal)
    }

    fn ready_goal(&mut self, goal_id: GoalId) -> Result<Goal> {
        let tx = self.write_tx("ready_goal")?;
        let result = self.store(&tx).ready_goal(goal_id).map_err(native_error)?;
        tx.commit()?;
        Ok(result)
    }

    fn add_note(&mut self, note: NewNote) -> Result<RunEvent> {
        note.validate()?;

        let tx = self.write_tx("add_note")?;
        let result = self.store(&tx).add_note(note).map_err(native_error)?;
        tx.commit()?;
        Ok(result)
    }

    fn set_goal(&mut self, task_id: TaskId, goal_id: Option<GoalId>) -> Result<Task> {
        self.set_goal_authorized(task_id, goal_id, None)
    }

    fn set_paths(&mut self, task_id: TaskId, paths: Vec<String>) -> Result<Task> {
        self.set_paths_authorized(task_id, paths, None)
    }

    fn edit_task(
        &mut self,
        task_id: TaskId,
        edit: TaskEdit,
        authorized: TaskStatus,
    ) -> Result<Task> {
        self.edit_task_authorized(task_id, edit, authorized, None)
    }

    fn set_priority(&mut self, task_id: TaskId, priority: Option<Priority>) -> Result<Task> {
        self.set_priority_authorized(task_id, priority, None)
    }
}

impl TaskReads for SqliteQueue {
    fn list(&self, query: &TaskQuery) -> Result<TaskPage> {
        let tx = self.read_tx("task_list")?;
        let result = self.store(&tx).list(query).map_err(native_error)?;
        Ok(result)
    }

    fn show(&mut self, task_id: TaskId) -> Result<TaskDetail> {
        // One read snapshot keeps task status, run history and events consistent.

        let tx = self.read_tx("task_show")?;
        let result = self.store(&tx).show(task_id).map_err(native_error)?;
        tx.commit()?;
        Ok(result)
    }

    fn candidates(&self) -> Result<Vec<Task>> {
        let tx = self.read_tx("candidates")?;
        let result = self.store(&tx).candidates().map_err(native_error)?;
        Ok(result)
    }

    fn build_waits(
        &self,
    ) -> Result<std::collections::HashMap<TaskId, Vec<crate::domain::Landing>>> {
        let tx = &*self.conn;
        let result = self.store(tx).build_waits().map_err(native_error)?;
        Ok(result)
    }

    fn graph_input(&self) -> Result<GraphInput> {
        let tx = self.read_tx("graph_input")?;
        let result = self.store(&tx).graph_input().map_err(native_error)?;
        Ok(result)
    }

    fn predecessors(&self, task_id: TaskId) -> Result<Vec<Predecessor>> {
        let tx = &*self.conn;
        let result = self.store(tx).predecessors(task_id).map_err(native_error)?;
        Ok(result)
    }

    fn goal_predecessors(&self, task_id: TaskId) -> Result<Vec<GoalPredecessor>> {
        let tx = &*self.conn;
        let result = self
            .store(tx)
            .goal_predecessors(task_id)
            .map_err(native_error)?;
        Ok(result)
    }

    fn task_fenced(&self, task: TaskId) -> Result<bool> {
        Ok(SqliteQueue::turn_fence(self, task)?.is_some())
    }
    fn tasks_in_progress(&self) -> Result<Vec<Task>> {
        let tx = &*self.conn;
        let result = self.store(tx).tasks_in_progress().map_err(native_error)?;
        Ok(result)
    }

    fn list_goals(&self) -> Result<Vec<GoalSummary>> {
        let tx = &*self.conn;
        let result = self.store(tx).list_goals().map_err(native_error)?;
        Ok(result)
    }

    fn show_goal(&mut self, goal_id: GoalId) -> Result<GoalDetail> {
        let tx = self.read_tx("show_goal")?;
        let result = self.store(&tx).show_goal(goal_id).map_err(native_error)?;
        tx.commit()?;
        Ok(result)
    }

    fn show_proposal(&self, proposal_id: ProposalId) -> Result<Proposal> {
        let tx = self.read_tx("show_proposal")?;
        proposals::read(&tx, proposal_id)
    }

    fn proposals(&self, all: bool) -> Result<Vec<Proposal>> {
        let tx = self.read_tx("proposals")?;
        proposals::list(&tx, all)
    }

    fn lint_input(&self, tasks: &[TaskId]) -> Result<LintInput> {
        let tx = self.read_tx("lint_input")?;
        let result = self.store(&tx).lint_input(tasks).map_err(native_error)?;
        Ok(result)
    }

    fn notes(&self, query: &NoteQuery) -> Result<NotePage> {
        let tx = &*self.conn;
        let result = self.store(tx).notes(query).map_err(native_error)?;
        Ok(result)
    }
}

/// `error` with [`crate::application::QueueBusy`] as its context when a
/// statement in its chain failed because another connection held the lock
/// (SQLite's busy or locked), so the application can tell it from a failure
/// that does not pass (task 1119).
pub(crate) fn tag_busy(error: anyhow::Error) -> anyhow::Error {
    let busy = error.chain().any(|cause| {
        cause
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(is_contention)
            || cause
                .downcast_ref::<super::sql_exec::SqlError>()
                .is_some_and(super::sql_exec::SqlError::is_contention)
    });
    if busy {
        error.context(crate::application::QueueBusy)
    } else {
        error
    }
}

// The row-mapping tests compare the port with this SQLite compatibility path.
#[cfg(test)]
pub(super) fn read_goal(conn: &Connection, goal_id: GoalId) -> Result<Goal> {
    store_rows::read_goal(&port_tx(conn), goal_id).map_err(native_error)
}

// Kept for store modules that have not moved; the moves land in any order
// (ADR-t3982-1).
#[allow(dead_code)]
pub(super) fn goal_event(
    conn: &Connection,
    goal_id: GoalId,
    kind: EventKind,
    payload: serde_json::Value,
) -> Result<()> {
    super::task_store::goal_event(&port_tx(conn), goal_id, kind, payload).map_err(native_error)
}

// No caller here any more, kept for a store module on another branch that
// has not moved (ADR-t3982-1: the moves land in any order).
#[allow(dead_code)]
pub(super) fn held_claims(conn: &Connection) -> Result<Vec<crate::domain::task_hold::HoldGuard>> {
    store_rows::held_claims(&port_tx(conn)).map_err(native_error)
}

/// Reserve a dependency-ready task inside the caller's write transaction:
/// the first task of `order` that is still a candidate, or the first
/// candidate in claim order when none of them is (an empty `order` means
/// claim order). There
/// is no queue-wide execution slot; `one_unfinished_run_per_task` is the
/// only limit, so concurrent claims take different tasks. The run is
/// created at `at` with an ID from `ids`. `attributes` (an object: what a
/// supervisor measured at the claim, task 197) go into `run_claimed`
/// beside its transition, and so does the worker session `trial` chooses
/// for the task (ADR-0079 decisions 3 and 4): chosen here, in the claim's
/// transaction, so two claims never take the same turn of the trial. A
/// task whose worker has none of `routes` is not claimed (ADR-t813-2):
/// the run requests the provider of its task's worker and runs on its
/// route's, recorded as `provider_switched` (phase `start`) when they
/// differ.
#[allow(clippy::too_many_arguments)]
// No caller here any more, kept for a store module on another branch that
// has not moved (ADR-t3982-1: the moves land in any order).
#[allow(dead_code)]
pub(super) fn claim_task(
    tx: &Connection,
    runs_dir: &Path,
    ids: &dyn IdGenerator,
    at: SystemTime,
    base_commit: &CommitSha,
    order: &[TaskId],
    held: &crate::domain::claim_order::ClaimHolds,
    attributes: Option<&serde_json::Value>,
    trial: &WorkerTrial,
    routes: &[WorkerRoute],
) -> Result<ClaimOutcome> {
    super::task_store::claim_task(
        &port_tx(tx),
        runs_dir,
        ids,
        at,
        base_commit,
        order,
        held,
        attributes,
        trial,
        routes,
    )
    .map_err(native_error)
}

/// Executing, awaiting or undergoing integration, or waiting for a session;
/// the same set as `one_unfinished_run_per_task`.
/// Apply `action` to the task inside the caller's transaction, recording
/// `task_status_changed`: what `transition` does, and what the triage does
/// to the task of the run it triaged.
// Kept for store modules migrating independently; current callers use the port.
#[allow(dead_code)]
pub(super) fn transition_task(
    conn: &Connection,
    task_id: TaskId,
    action: TaskAction,
    now: &str,
) -> Result<Task> {
    super::task_store::transition_task(&port_tx(conn), task_id, action, now).map_err(native_error)
}

/// Complete the task of `run` inside the caller's transaction:
/// `in_progress` → `completed`, recorded as `task_status_changed` on the
/// run, with a completed Spike's replanning request (`now_secs`). The
/// planning side's write of the task, which the run's end calls rather
/// than writing `tasks` itself: its landing (T2 of
/// `src/context_boundaries/ownership.rs`) or the close of a verified
/// `already_satisfied` claim without one (T12, ADR-t1818-1 decision 5).
// No caller here any more, kept for a store module on another branch that
// has not moved (ADR-t3982-1: the moves land in any order).
#[allow(dead_code)]
pub(super) fn complete_task(
    conn: &Connection,
    task_id: TaskId,
    run: &RunId,
    now: &str,
    now_secs: i64,
) -> Result<Task> {
    super::task_store::complete_task(&port_tx(conn), task_id, run, now, now_secs)
        .map_err(native_error)
}

/// Cancel `task_id` inside the caller's transaction with the reason the
/// domain decides of `cancel` (ADR-t947-4), recorded in the payload of its
/// `task_status_changed`: what `cancel` does, plan review's
/// `cancel_duplicate` and the `cancel` answers of the asks the runtime
/// applies. A task named as the one that holds the content is checked by
/// [`super::task_store::check_duplicate`] and gives the reason of a `--duplicate-of` alone.
// Kept for store modules migrating independently; current callers use the port.
#[allow(dead_code)]
pub(super) fn cancel_task(
    conn: &Connection,
    task_id: TaskId,
    cancel: &NewCancel,
    now: &str,
) -> Result<Task> {
    super::task_store::cancel_task(&port_tx(conn), task_id, cancel, now).map_err(native_error)
}

#[cfg(test)]
pub(super) fn cancel_of(
    conn: &Connection,
    task_id: TaskId,
) -> Result<Option<store_rows::RecordedCancel>> {
    store_rows::cancel_of(&port_tx(conn), task_id).map_err(native_error)
}

// Kept for store modules migrating independently; current callers use the port.
#[allow(dead_code)]
pub(super) fn read_task(conn: &Connection, task_id: TaskId) -> Result<Task> {
    store_rows::read_task(&port_tx(conn), task_id).map_err(native_error)
}

/// The status of `task` and the proposal it is in, as a planning command
/// is authorized with them; a missing task is an error.
pub(super) fn task_authorization(conn: &Connection, task: TaskId) -> Result<TaskAuthorization> {
    super::task_store::task_authorization(&port_tx(conn), task).map_err(native_error)
}

/// [`store_writes::record_queue_event_in`] on a rusqlite connection.
pub(super) fn record_queue_event_in(
    conn: &Connection,
    kind: EventKind,
    payload: &serde_json::Value,
) -> Result<()> {
    store_writes::record_queue_event_in(&port_tx(conn), kind, payload).map_err(native_error)
}

/// [`store_writes::event`] on a rusqlite connection.
// Kept for store modules that have not moved; the moves land in any order
// (ADR-t3982-1).
#[allow(dead_code)]
pub(super) fn event(
    conn: &Connection,
    task_id: TaskId,
    run_id: Option<&RunId>,
    kind: EventKind,
    payload: serde_json::Value,
) -> Result<()> {
    store_writes::event(&port_tx(conn), task_id, run_id, kind, payload).map_err(native_error)
}

// Kept for store modules that have not moved; the moves land in any order
// (ADR-t3982-1).
#[allow(dead_code)]
pub(super) fn enum_col<T: FromStr<Err = DomainError>>(
    row: &Row<'_>,
    name: &str,
) -> rusqlite::Result<T> {
    with_row(row, |row| store_rows::enum_col(row, name))
}

// SQLite compatibility entry for store modules migrating independently.
#[allow(dead_code)]
pub(super) fn json_col<T: DeserializeOwned>(row: &Row<'_>, name: &str) -> rusqlite::Result<T> {
    with_row(row, |row| store_rows::json_col(row, name))
}

pub(super) fn optional_enum_col<T: std::str::FromStr<Err = DomainError>>(
    row: &Row<'_>,
    name: &str,
) -> rusqlite::Result<Option<T>> {
    with_row(row, |row| store_rows::optional_enum_col(row, name))
}

// Kept for a store module on another branch that has not moved
// (ADR-t3982-1: the moves land in any order).
#[allow(dead_code)]
pub(super) fn run_row(runs_dir: &Path) -> impl Fn(&Row<'_>) -> rusqlite::Result<TaskRun> + '_ {
    let read = store_rows::run_row(runs_dir);
    move |row| with_row(row, &read)
}

// No caller here any more, kept for a store module on another branch that
// has not moved (ADR-t3982-1: the moves land in any order).
#[allow(dead_code)]
pub(super) fn stored_run_row(row: &Row<'_>) -> rusqlite::Result<TaskRun> {
    with_row(row, store_rows::stored_run_row)
}

// SQLite compatibility entry for store modules migrating independently.
#[allow(dead_code)]
pub(super) fn event_row(row: &Row<'_>) -> rusqlite::Result<RunEvent> {
    with_row(row, store_rows::event_row)
}

/// Move the task to `goal_id`, or out of any goal, as `by` moves it
/// ([`task::set_goal`]): a person's priority it took from its old goal and
/// an AI's move would carry off is kept as its own, recorded as
/// `task_priority_changed` with `kept_on_move` before the move's
/// `task_goal_changed` (ADR-t1975-1 decision 4). A person's priority it
/// took from its old goal that the AI's `out_of_scope` judgement lets go
/// of is recorded the same way with `dropped_on_move`, the value it had
/// and `from_by: human` (ADR-t2373-1 decision 4).
#[cfg(test)]
pub(super) fn set_goal_in(
    conn: &Connection,
    task_id: TaskId,
    goal_id: Option<GoalId>,
    timestamp: &str,
    by: task::MovedBy,
) -> Result<Task> {
    super::task_store::set_goal_in(&port_tx(conn), task_id, goal_id, timestamp, by)
        .map_err(native_error)
}

#[cfg(test)]
mod schema_state_tests {
    use super::*;

    fn state(pending: &[(i64, bool)]) -> SchemaState {
        SchemaState {
            schema_version: 1,
            binary_schema_version: 3,
            floor: 0,
            pending: pending
                .iter()
                .map(|&(version, compatible)| SchemaMigration {
                    version,
                    compatible,
                })
                .collect(),
            opens: true,
        }
    }

    #[test]
    fn migration_rereads_after_takeover_and_an_applied_version_does_nothing() {
        use crate::application::WriteLeaderStore;
        use crate::domain::write_leader::LEASE;
        let uri = format!("file:/migration-fencing-{}?vfs=memdb", uuid::Uuid::new_v4());
        let mut queue = SqliteQueue::init(uri).unwrap();
        // An unheld generation zero is locked too, without renewing its lease.
        queue.migration_tx(|| Ok(())).unwrap().commit().unwrap();
        assert_eq!(queue.write_leader().unwrap().0.lease_until_ms, 0);
        let mut reads = 0;
        let before_schema = queue
            .conn
            .query_row("SELECT count(*) FROM sqlite_master", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap();
        let tx = queue
            .migration_tx(|| {
                reads += 1;
                assert!(reads <= 2, "migration failed to lock the reread leader row");
                if reads == 1 {
                    queue.take_write_leader("replacement", LEASE, true)?;
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(reads, 2);
        let generation: i64 = tx
            .query_row("SELECT generation FROM write_leader", [], |r| r.get(0))
            .unwrap();
        assert_eq!(generation, 1);
        tx.execute_batch("CREATE TABLE migration_probe (id INTEGER PRIMARY KEY)")
            .unwrap();
        tx.commit().unwrap();
        let before = queue.conn.total_changes();
        assert_eq!(
            queue.apply_migrations(BINARY_SCHEMA - 1, None).unwrap(),
            (None, BINARY_SCHEMA)
        );
        // Only the row lock UPDATE, no schema_floor or migration writes.
        assert_eq!(queue.conn.total_changes() - before, 1);
        assert_eq!(
            queue
                .conn
                .query_row("SELECT count(*) FROM sqlite_master", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            before_schema + 1
        );
    }

    /// `up` and `install` apply pending migrations only when every one is
    /// compatible; a breaking one is refused with its version and the way
    /// to it.
    #[test]
    fn only_compatible_pending_migrations_pass() {
        state(&[]).refuse_breaking().unwrap();
        state(&[(2, true), (3, true)]).refuse_breaking().unwrap();
        let error = state(&[(2, true), (3, false)])
            .refuse_breaking()
            .unwrap_err()
            .to_string();
        assert!(error.contains("breaking migration(s) 3 "), "{error}");
        assert!(error.contains("dagq install --allow-breaking"), "{error}");
    }

    /// With no queue yet, or a queue at this binary's schema, nothing is
    /// migrated.
    #[test]
    fn nothing_is_migrated_without_a_queue_or_a_pending_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.db");
        assert_eq!(
            SqliteQueue::migrate_compatible(&path, None, 0).unwrap(),
            None
        );
        SqliteQueue::init(&path).unwrap();
        assert_eq!(
            SqliteQueue::migrate_compatible(&path, None, 0).unwrap(),
            None
        );
    }
}

#[cfg(test)]
mod queue_use_tests {
    use super::*;

    /// A breaking migration judges the queue in use by a supervisor or a
    /// wrapper whose process lives and by every run not finished; a dead
    /// process and a finished run use it no longer.
    #[test]
    fn the_queue_is_in_use_by_live_processes_and_unfinished_runs() {
        let alive = |pid: u32| pid == 7;
        assert!(queue_users(&QueueUse::default(), &alive).is_empty());
        let found = QueueUse {
            supervisors: vec![("live".into(), 7), ("gone".into(), 8)],
            runs: vec![
                ("r1".into(), "running".into()),
                ("r2".into(), "merged".into()),
            ],
            wrappers: vec![("r1".into(), 7), ("r3".into(), -1)],
        };
        assert_eq!(
            queue_users(&found, &alive),
            [
                "supervisor live (pid 7)",
                "run r1 (running)",
                "wrapper of run r1 (pid 7)",
            ]
        );
        let idle = QueueUse {
            supervisors: vec![("gone".into(), 8)],
            runs: vec![("r2".into(), "merged".into())],
            wrappers: vec![("r1".into(), 9)],
        };
        assert!(queue_users(&idle, &alive).is_empty());
        let pending = [
            SchemaMigration {
                version: 40,
                compatible: true,
            },
            SchemaMigration {
                version: 41,
                compatible: false,
            },
        ];
        let error = format!("{:#}", refuse_in_use(&found, &alive, &pending).unwrap_err());
        assert!(error.contains("breaking migration(s) 41 while"), "{error}");
        assert!(refuse_in_use(&idle, &alive, &pending).is_ok());
    }
}

#[cfg(test)]
mod origin_migration_tests {
    use super::*;
    use crate::domain::plan_request::{Origin, OriginKind};

    /// The migration that records origins fills the rows already queued
    /// from their records (ADR-t1975-1 decision 6): the creation event's
    /// actor and planner, the request a proposal is linked to, a draft's
    /// origin; a row none of them decides is `unknown`. A priority is the
    /// AI's only when the row is and its recorded setter is; no value
    /// changes.
    #[test]
    fn the_migration_fills_origins_and_who_set_priorities_from_the_records() {
        let at = MIGRATIONS
            .iter()
            .position(|migration| migration.contains("ALTER TABLE goals ADD COLUMN origin "))
            .unwrap();
        let before = i64::try_from(at).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.db");
        let raw = Connection::open(&path).unwrap();
        for migration in &MIGRATIONS[..at] {
            raw.execute_batch(migration).unwrap();
        }
        raw.execute_batch(&format!(
            "PRAGMA foreign_keys = OFF;
             PRAGMA application_id = 1129599281; PRAGMA user_version = {before};
             INSERT INTO schema_floor(singleton, floor) VALUES (1, {floor});
             INSERT INTO plan_requests(id, text, requested_by, requested_by_id, created_at,
                 updated_at) VALUES (44, 'plan it', 'inbox', 'inbox', 0, 0);
             INSERT INTO proposals(id, status, owner_origin, submitted_at, created_at, updated_at)
                 VALUES (7, 'submitted', 'runtime', '', '', '');
             INSERT INTO plan_request_proposals(request_id, proposal_id, created_at)
                 VALUES (44, 7, 0);
             INSERT INTO planners(id, origin, request_id, finding_id, draft_task_id, proposal_id,
                 created_at) VALUES
                 (1, 'runtime', 44, NULL, NULL, NULL, 0),
                 (2, 'runtime', NULL, 5, NULL, NULL, 0),
                 (3, 'runtime', NULL, NULL, 10, NULL, 0),
                 (4, 'runtime', NULL, NULL, NULL, 7, 0),
                 (5, 'runtime', NULL, NULL, NULL, NULL, 0);
             INSERT INTO goals(id, title, description, acceptance, constraints, status, priority)
             VALUES (1, 'request', '', '', '', 'open', 4), (2, 'person', '', '', '', 'open', 2),
                    (3, 'planner', '', '', '', 'open', 2), (4, 'old', '', '', '', 'open', 1);
             INSERT INTO tasks(id, title, description, acceptance, verification_commands, status,
                 priority, proposal_id) VALUES
                 (1, 'request', '', '', '[]', 'draft', NULL, NULL),
                 (2, 'finding', '', '', '[]', 'draft', 2, NULL),
                 (3, 'follow-up planner', '', '', '[]', 'draft', NULL, NULL),
                 (4, 'revised', '', '', '[]', 'draft', NULL, NULL),
                 (5, 'follow-up', '', '', '[]', 'draft', 3, NULL),
                 (6, 'planner', '', '', '[]', 'draft', 3, NULL),
                 (7, 'linked', '', '', '[]', 'submitted', NULL, 7),
                 (8, 'old', '', '', '[]', 'ready', 2, NULL),
                 (9, 'inbox', '', '', '[]', 'ready', NULL, NULL),
                 (10, 'drafted', '', '', '[]', 'draft', NULL, NULL),
                 (11, 'no planner row', '', '', '[]', 'draft', 2, NULL),
                 (12, 'reopened later', '', '', '[]', 'draft', NULL, NULL);
             INSERT INTO draft_reopens(task_id, material, created_at) VALUES (12, '{{}}', 0);
             INSERT INTO draft_origins(task_id, origin, material, created_at)
                 VALUES (5, 'follow_up', '{{}}', 0), (10, 'follow_up', '{{}}', 0);
             INSERT INTO run_events(goal_id, kind, payload, actor_role, actor_id) VALUES
                 (1, 'goal_created', '{{\"goal\":{{\"priority\":\"interrupt\"}}}}', 'planner', 'planner:1'),
                 (2, 'goal_created', '{{\"goal\":{{\"priority\":\"high\"}}}}', 'user', 'user'),
                 (3, 'goal_created', '{{\"goal\":{{\"priority\":\"high\"}}}}', 'planner', 'planner:5');
             INSERT INTO run_events(task_id, kind, payload, actor_role, actor_id) VALUES
                 (1, 'task_created', '{{}}', 'planner', 'planner:1'),
                 (2, 'task_created', '{{}}', 'planner', 'planner:2'),
                 (3, 'task_created', '{{}}', 'planner', 'planner:3'),
                 (4, 'task_created', '{{}}', 'planner', 'planner:4'),
                 (5, 'task_created', '{{}}', 'supervisor', 'supervisor'),
                 (5, 'task_priority_changed', '{{}}', 'user', 'user'),
                 (6, 'task_created', '{{}}', 'planner', 'planner:5'),
                 (9, 'task_created', '{{}}', 'inbox', 'inbox'),
                 (11, 'task_created', '{{}}', 'planner', 'planner');",
            floor = schema::floor_for(before),
        ))
        .unwrap();
        drop(raw);
        SqliteQueue::migrate(&path, None, 0).unwrap();
        let mut queue = SqliteQueue::open(&path).unwrap();
        let request = Some(RequestId::new(44));
        let human = |kind| (Origin::Human, Some(kind));
        let ai = |kind| (Origin::Ai, Some(kind));
        for (id, (origin, kind), from, priority_by, priority) in [
            (
                1,
                human(OriginKind::Request),
                request,
                PriorityBy::Ai,
                Priority::Normal,
            ),
            (
                2,
                ai(OriginKind::Finding),
                None,
                PriorityBy::Ai,
                Priority::High,
            ),
            (
                3,
                ai(OriginKind::FollowUp),
                None,
                PriorityBy::Ai,
                Priority::Normal,
            ),
            (
                4,
                human(OriginKind::Request),
                request,
                PriorityBy::Ai,
                Priority::Normal,
            ),
            // A person set its priority after the runtime registered it.
            (
                5,
                ai(OriginKind::FollowUp),
                None,
                PriorityBy::Human,
                Priority::Urgent,
            ),
            (
                6,
                ai(OriginKind::Planner),
                None,
                PriorityBy::Ai,
                Priority::Urgent,
            ),
            (
                7,
                human(OriginKind::Request),
                request,
                PriorityBy::Ai,
                Priority::Normal,
            ),
            // Nobody can tell: a person's, so plan review keeps it.
            (
                8,
                (Origin::Unknown, None),
                None,
                PriorityBy::Human,
                Priority::High,
            ),
            (
                9,
                human(OriginKind::Person),
                None,
                PriorityBy::Ai,
                Priority::Normal,
            ),
            // A planner without its row, and a reopen without a recorded
            // creator: who made them is unknown.
            (
                11,
                (Origin::Unknown, None),
                None,
                PriorityBy::Human,
                Priority::High,
            ),
            (
                12,
                (Origin::Unknown, None),
                None,
                PriorityBy::Ai,
                Priority::Normal,
            ),
        ] {
            let task = queue.show(TaskId::new(id)).unwrap().task;
            let recorded = task.origin();
            assert_eq!(
                (recorded.origin, recorded.kind, recorded.request_id),
                (origin, kind, from),
                "task {id}"
            );
            assert_eq!(task.priority_by(), priority_by, "task {id}");
            assert_eq!(task.priority(), priority, "task {id}");
        }
        for (id, (origin, kind), from, priority_by, priority) in [
            (
                1,
                human(OriginKind::Request),
                request,
                PriorityBy::Human,
                Priority::Interrupt,
            ),
            (
                2,
                human(OriginKind::Person),
                None,
                PriorityBy::Human,
                Priority::High,
            ),
            (
                3,
                ai(OriginKind::Planner),
                None,
                PriorityBy::Ai,
                Priority::High,
            ),
            (
                4,
                (Origin::Unknown, None),
                None,
                PriorityBy::Human,
                Priority::Normal,
            ),
        ] {
            let goal = queue.show_goal(GoalId::new(id)).unwrap().goal;
            let recorded = goal.origin();
            assert_eq!(
                (recorded.origin, recorded.kind, recorded.request_id),
                (origin, kind, from),
                "goal {id}"
            );
            assert_eq!(goal.priority_by(), priority_by, "goal {id}");
            assert_eq!(goal.priority(), priority, "goal {id}");
        }
    }
}

#[cfg(test)]
mod persons_priority_tests {
    use super::*;
    use crate::domain::task::MovedBy;

    fn new_goal(title: &str, priority: Priority) -> NewGoal {
        NewGoal {
            title: title.into(),
            description: "d".into(),
            acceptance: "a".into(),
            constraints: String::new(),
            doc: None,
            draft: false,
            priority: Some(priority),
            tags: Vec::new(),
        }
    }

    fn new_task(goal: GoalId) -> NewTask {
        NewTask {
            title: "t".into(),
            description: "d".into(),
            acceptance: "a".into(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: None,
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: Some(goal),
            context: String::new(),
            provider: None,
            worker_mode: None,
            wait_for_build: false,
            execution_class: Default::default(),
        }
    }

    /// The store keeps a person's priority from the AI's writes in their
    /// transaction (ADR-t1975-1 decisions 3 and 4): `set-priority` and
    /// `goal edit` by a planner are refused naming the value, the
    /// planner's move keeps the value as the task's own (recorded as
    /// `kept_on_move`), and the user's move takes the new goal's.
    #[test]
    fn the_ais_writes_keep_a_persons_priority_and_a_persons_move_takes_the_goals() {
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("queue.db")).unwrap();
        let persons = queue
            .add_goal(new_goal("a person's", Priority::Interrupt))
            .unwrap();
        let task = TaskStore::add(&mut queue, new_task(persons.id()))
            .unwrap()
            .id();
        let other = TaskStore::add(&mut queue, new_task(persons.id()))
            .unwrap()
            .id();
        let mut queue = queue.with_actor(ActorContext::instance(ActorRole::Planner, 1));
        let ais = queue
            .add_goal(new_goal("the AI's", Priority::Normal))
            .unwrap();
        assert_eq!(ais.priority_by(), PriorityBy::Ai);
        let refused = queue
            .set_priority_authorized(task, Some(Priority::Low), None)
            .unwrap_err();
        assert!(
            matches!(
                refused.downcast_ref::<DomainError>(),
                Some(DomainError::PersonsPriority {
                    priority: Priority::Interrupt,
                    role: ActorRole::Planner,
                    ..
                })
            ),
            "{refused:#}"
        );
        let edit = GoalEdit {
            priority: Some(Priority::Low),
            ..GoalEdit::default()
        };
        assert!(queue.edit_goal(persons.id(), edit.clone()).is_err());
        // The AI's goal is the AI's to change.
        queue.edit_goal(ais.id(), edit).unwrap();
        let moved = queue
            .set_goal_authorized(task, Some(ais.id()), None)
            .unwrap();
        assert_eq!(
            (moved.priority(), moved.own_priority_by(), moved.goal_id()),
            (Priority::Interrupt, Some(PriorityBy::Human), Some(ais.id()))
        );
        let kept = queue
            .conn
            .query_row(
                "SELECT payload FROM run_events WHERE task_id=?1 AND kind='task_priority_changed'",
                [task],
                |r| r.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&kept).unwrap(),
            json!({"from": "interrupt", "to": "interrupt", "from_source": "goal",
                "to_source": "task", "kept_on_move": true, "own": "interrupt",
                "own_by": "human"})
        );
        // A person's move, as a follow_up's judge or a correct_goal answer
        // passes it, takes the goal's.
        let tx = queue.conn.transaction().unwrap();
        let moved = set_goal_in(&tx, other, Some(ais.id()), "now", MovedBy::Person).unwrap();
        assert_eq!(
            (moved.priority(), moved.own_priority(), moved.priority_by()),
            (Priority::Low, None, PriorityBy::Ai)
        );
    }
}

#[cfg(test)]
mod write_hold_tests {
    use super::*;

    const THRESHOLD: Duration = Duration::from_secs(2);

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    fn times(locked: Option<u64>, ended: u64, rows: usize) -> WriteTxTimes {
        WriteTxTimes {
            locked: locked.map(ms),
            ended: ms(ended),
            wrote: locked.is_some(),
            rows,
        }
    }

    /// A hold past the threshold warns with how long it held, how long it
    /// ran before the lock and its rows; one within it does not.
    #[test]
    fn a_hold_past_the_threshold_is_a_long_hold_and_one_within_it_is_not() {
        assert_eq!(
            long_hold(times(Some(100), 2_600, 40), THRESHOLD),
            Some(LongHold {
                held: ms(2_500),
                before_lock: ms(100),
                rows: 40,
            })
        );
        assert_eq!(long_hold(times(Some(100), 2_100, 40), THRESHOLD), None);
        assert_eq!(long_hold(times(Some(0), 1_999, 1), THRESHOLD), None);
    }

    /// The wait for another writer and the reads after `BEGIN` come before
    /// the lock, so a long one with a short hold is no long hold.
    #[test]
    fn the_wait_and_the_reads_before_the_lock_are_not_held() {
        assert_eq!(long_hold(times(Some(4_900), 5_200, 3), THRESHOLD), None);
        assert_eq!(long_hold(times(Some(30_000), 30_001, 1), THRESHOLD), None);
    }

    /// A transaction that never had the write lock (a deferred one that only
    /// read) has no hold, however long it ran.
    #[test]
    fn a_transaction_that_never_had_the_lock_is_no_long_hold() {
        assert_eq!(long_hold(times(None, 60_000, 0), THRESHOLD), None);
    }

    /// A transaction that held the lock with no time known for it (a
    /// deferred one that wrote only through the transaction it derefs to)
    /// is held from its `BEGIN`, whatever rows it counted.
    #[test]
    fn a_hold_with_no_lock_time_is_held_from_the_begin() {
        let unknown = WriteTxTimes {
            locked: None,
            wrote: true,
            ended: ms(9_000),
            rows: 0,
        };
        assert_eq!(
            long_hold(unknown, THRESHOLD),
            Some(LongHold {
                held: ms(9_000),
                before_lock: Duration::ZERO,
                rows: 0,
            })
        );
        let short = WriteTxTimes {
            ended: ms(1_500),
            ..unknown
        };
        assert_eq!(long_hold(short, THRESHOLD), None);
    }

    /// The record of a long hold names the transaction, its times and rows,
    /// and the process that held it.
    #[test]
    fn a_long_hold_s_payload_names_the_transaction_and_the_process() {
        let hold = LongHold {
            held: ms(2_500),
            before_lock: ms(100),
            rows: 3,
        };
        assert_eq!(
            held_long_payload("ledger_row_over", hold, "supervisor", Some("supervise"), 42),
            json!({
                "transaction": "ledger_row_over",
                "held_ms": 2_500,
                "before_lock_ms": 100,
                "rows": 3,
                "role": "supervisor",
                "command": "supervise",
                "pid": 42,
            })
        );
        assert_eq!(
            held_long_payload("x", hold, "user", None, 1)["command"],
            serde_json::Value::Null
        );
    }

    /// The threshold names a writer before its hold fails another
    /// connection's write.
    #[test]
    fn the_threshold_is_under_the_busy_timeout() {
        assert!(WRITE_HOLD_WARN < crate::application::QUEUE_BUSY_TIMEOUT);
    }
}

#[cfg(test)]
mod inserted_id_tests {
    use super::*;

    fn table() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT UNIQUE)")
            .unwrap();
        conn
    }

    /// SQLite's insert returns the row's ID as rusqlite's
    /// `last_insert_rowid` gives it right after the statement.
    #[test]
    fn an_insert_returns_the_id_rusqlite_gives_the_inserted_row() {
        let conn = table();
        conn.execute("INSERT INTO t(id, v) VALUES (41, 'a')", [])
            .unwrap();
        let id = insert_id(&conn, "INSERT INTO t(v) VALUES (?1)", ["b"]).unwrap();
        assert_eq!(id, 42);
        assert_eq!(id, conn.last_insert_rowid());
        let v: String = conn
            .query_row("SELECT v FROM t WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "b");
    }

    /// An insert that inserts no row has no ID; one that does has its own.
    #[test]
    fn an_insert_that_inserts_no_row_has_no_id() {
        let conn = table();
        let sql = "INSERT INTO t(v) SELECT ?1 WHERE NOT EXISTS(SELECT 1 FROM t WHERE v=?1)";
        let tx = port_tx(&conn);
        let insert = |value: &str| {
            crate::infrastructure::sql_exec::SqlWrite::insert_id_if_inserted(
                &tx,
                sql,
                &[crate::infrastructure::sql_exec::SqlValue::Text(
                    value.to_owned(),
                )],
            )
            .unwrap()
        };
        assert_eq!(insert("a"), Some(1));
        assert_eq!(insert("a"), None);
        assert_eq!(insert("b"), Some(2));
    }

    /// In a transaction, the insert returns the ID as outside one and is
    /// counted as a write of the transaction, as `execute` is.
    #[test]
    fn a_transaction_s_insert_returns_the_id_and_counts_its_row() {
        let conn = table();
        let tx = WriteTx::read(&conn, "insert_id_test", Duration::from_secs(60)).unwrap();
        let id = tx.insert_id("INSERT INTO t(v) VALUES (?1)", ["a"]).unwrap();
        assert_eq!(id, tx.last_insert_rowid());
        assert!(tx.wrote.get());
        assert_eq!(tx.rows.get(), 1);
        assert!(tx.locked.get().is_some());
        tx.commit().unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM t WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}

#[cfg(test)]
mod store_contract_tests {
    use super::*;
    use crate::infrastructure::store_contract::{self, ContractStore};

    impl ContractStore for SqliteQueue {
        fn with_identity(self, identity: WriteIdentity) -> Self {
            self.with_write_identity(identity)
        }
        fn set_actor(&self, actor: ActorContext) {
            self.actors.set(actor);
        }
        fn request(&self, requested_by: Option<String>) -> Option<String> {
            self.actors.request(requested_by)
        }
        fn commit(tx: WriteTx<'_>) -> Result<()> {
            Ok(tx.commit()?)
        }
    }

    // Keep the named memdb alive across the connections opened by a case.
    fn contract(case: impl FnOnce(&dyn Fn() -> Result<SqliteQueue>) -> Result<()>) {
        let uri = format!("file:/store-contract-{}?vfs=memdb", uuid::Uuid::new_v4());
        let _keeper = SqliteQueue::init(&uri).unwrap();
        case(&|| SqliteQueue::open(&uri)).unwrap();
    }

    #[test]
    fn leader_lifecycle() {
        contract(|open| store_contract::leader_lifecycle(&open));
    }
    #[test]
    fn leader_singleton() {
        contract(|open| store_contract::leader_singleton(&open));
    }
    #[test]
    fn stale_writer() {
        contract(|open| store_contract::stale_writer(&open));
    }
    #[test]
    fn unique_indexes() {
        contract(|open| store_contract::unique_indexes(&open));
    }
    #[test]
    fn event_actors() {
        contract(|open| store_contract::event_actors(&open));
    }
    #[test]
    fn event_bodies() {
        contract(|open| store_contract::event_bodies(&open));
    }
    #[test]
    fn base_types() {
        contract(|open| store_contract::base_types(&open));
    }
    #[test]
    fn typed_conditions() {
        contract(|open| store_contract::typed_conditions(&open));
    }
    #[test]
    fn json_set_field() {
        contract(|open| store_contract::json_set_field(&open));
    }
    #[test]
    fn all_cases_run_in_order_on_the_same_database() {
        contract(|open| store_contract::run(&open));
    }
}
