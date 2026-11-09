//! The Postgres store's adapter: its session and the named entries of its
//! transactions (ADR-t3982-1 decisions 1 and 2, ADR-t3986-1 decision 2).
//!
//! What a transaction runs is built here in every build, on the
//! [`PostgresSession`] a driver answers, so the unit tests see each statement
//! without a DB. The driver itself ([`client`]) is behind the cargo feature
//! `postgres`.
//!
//! A write begins as `READ COMMITTED` and its first statement is the shared
//! fence of [`super::write_leader`]: writers across processes queue on that
//! one row, and a stale generation changes no row and writes nothing. A read
//! begins as `REPEATABLE READ`, which is the one snapshot SQLite's read entry
//! reads, and read-only.
//!
//! A transaction is the execution port ([`super::sql_exec`]) on Postgres: it
//! runs a statement of the shared SQL as [`exec`] maps its parameters' spelling
//! and its values, so a store module holds no statement of its own for this
//! dialect. A read entry hands out the same type as a write entry, as SQLite's
//! does; the read-only transaction refuses a write.

#[cfg(feature = "postgres")]
pub mod client;
mod event_actor;
pub mod exec;
mod leader;
pub mod migrate;
pub(crate) mod search;
pub(crate) mod search_write;
pub mod tls;

use self::exec::{PostgresRow, numbered_parameters, returning_id, statement_was_rejected};
use super::event_actor::{EventActors, process_actor};
use super::sql_dialect::SqlDialect;
use super::sql_exec::{
    OptionalRow, SqlError, SqlRead, SqlResult, SqlRow, SqlSavepoint, SqlSavepoints, SqlValue,
    SqlWrite, TransactionEntries, sql_params,
};
use super::write_leader::{IDENTITY_SQL, WriteIdentity, millis, renew_sql};
use crate::application::NotLeader;
use crate::domain::actor::ActorContext;
use anyhow::{Context, Result};
use std::cell::Cell;

/// What every connection runs first: a time without a zone reads as UTC, as
/// in SQLite, whatever the server's default.
const SESSION_SETUP_SQL: &str = "SET TIME ZONE 'UTC'";
const BEGIN_WRITE_SQL: &str = "BEGIN ISOLATION LEVEL READ COMMITTED";
const BEGIN_READ_SQL: &str = "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY";
const COMMIT_SQL: &str = "COMMIT";
const ROLLBACK_SQL: &str = "ROLLBACK";

/// The fence every write begins with: the SQLite store's statement, with
/// Postgres's clock and parameters.
fn fence_sql() -> String {
    numbered_parameters(&renew_sql(&SqlDialect::Postgres.now_millis()))
}

/// The shared queue state with Postgres's connection and transaction entries.
pub type PostgresQueue<S> = super::store_queue::StoreQueue<PostgresStore<S>>;

impl<S: PostgresSession> PostgresQueue<S> {
    /// Open the session's schema without migrating it, and resolve run files
    /// under the control side's supplied run directory.
    pub fn open(session: S, runs_dir: std::path::PathBuf) -> Result<Self> {
        let entries = PostgresStore::open(session)?;
        let actors = entries.actors.clone();
        Ok(Self {
            entries,
            runs_dir,
            actors,
            generators: super::clock::system(),
            changes: None,
            title_prefixes: None,
            goal_tags: None,
            max_spike_cycles: None,
            replacement: None,
            denial_log: None,
        })
    }
}

/// What a driver answers for the store. Its statements' parameters are
/// already numbered as Postgres spells them. Driver errors from query and
/// execute use [`exec::session_error`] to distinguish server rejections from
/// local conversions.
pub trait PostgresSession {
    /// Run `sql`, a statement without parameters.
    fn run(&self, sql: &str) -> Result<()>;

    /// The rows of `sql` with `params`, all of them: a store module may run
    /// another statement while it visits a row, and the one connection cannot
    /// while it is still reading rows.
    fn query(&self, sql: &str, params: &[SqlValue]) -> SqlResult<Vec<PostgresRow>>;

    /// Run the write `sql` with `params` and return the rows it changed.
    fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<u64>;
}

/// The Postgres store on one session, which writes as the identity it read
/// when it opened: like the SQLite store, it never refreshes it on a write.
pub struct PostgresStore<S> {
    session: S,
    identity: WriteIdentity,
    actors: EventActors,
}

impl<S: PostgresSession> PostgresStore<S> {
    /// Use the identity acquired at startup, retained even after takeover.
    pub fn with_write_identity(mut self, identity: WriteIdentity) -> Self {
        self.identity = identity;
        self
    }

    /// Write events as this actor, including statements in an open transaction.
    pub fn set_actor(&self, actor: ActorContext) {
        self.actors.set(actor);
    }

    /// Replace the requester until the caller restores the returned value.
    pub fn request(&self, requested_by: Option<String>) -> Option<String> {
        self.actors.request(requested_by)
    }

    /// Only conditional leader mutations use this unfenced write entry.
    /// It is private to the Postgres adapter; ordinary writes fence first.
    fn leader_entry(&self, name: &'static str) -> Result<PostgresTx<'_, S>> {
        let mut tx = PostgresTx::begin(&self.session, name, BEGIN_WRITE_SQL)?;
        tx.actors = Some(&self.actors);
        tx.sync_actors()?;
        Ok(tx)
    }

    /// The store on the newly connected `session`, refused unless this
    /// binary opens its schema as it is ([`migrate::check_opens`]).
    pub fn open(session: S) -> Result<Self> {
        session.run(SESSION_SETUP_SQL)?;
        migrate::check_opens(&session)?;
        let identity = read_identity(&session).context("read the write leader's row")?;
        Ok(Self {
            session,
            identity,
            actors: EventActors::new(process_actor()),
        })
    }
}

fn read_identity(session: &impl PostgresSession) -> SqlResult<WriteIdentity> {
    let rows = session.query(IDENTITY_SQL, &[])?;
    let row = SqlRow::new(rows.first().ok_or(SqlError::NoRows)?);
    Ok(WriteIdentity {
        holder: row.get(0)?,
        generation: row.get(1)?,
    })
}

impl<S: PostgresSession> TransactionEntries for PostgresStore<S> {
    type Write<'store>
        = PostgresTx<'store, S>
    where
        S: 'store;
    type Read<'store>
        = PostgresTx<'store, S>
    where
        S: 'store;

    /// The fence's zero rows drop the transaction, which rolls it back
    /// before the retryable [`NotLeader`] returns.
    fn write_entry(&self, name: &'static str) -> Result<PostgresTx<'_, S>> {
        let mut tx = PostgresTx::begin(&self.session, name, BEGIN_WRITE_SQL)?;
        tx.actors = Some(&self.actors);
        let changed = self.session.execute(
            &fence_sql(),
            sql_params![
                self.identity.holder.as_deref(),
                self.identity.generation,
                millis(crate::domain::write_leader::LEASE)?,
            ],
        )?;
        if changed == 0 {
            return Err(NotLeader.into());
        }
        tx.sync_actors()?;
        Ok(tx)
    }

    fn read_entry(&self, name: &'static str) -> Result<PostgresTx<'_, S>> {
        let mut tx = PostgresTx::begin(&self.session, name, BEGIN_READ_SQL)?;
        tx.actors = Some(&self.actors);
        tx.sync_actors()?;
        Ok(tx)
    }
}

/// A transaction of an entry, which rolls back unless it commits.
/// PostgreSQL answers COMMIT on an aborted transaction with a ROLLBACK tag,
/// which the driver does not expose. Track server-rejected session statements,
/// so a caller cannot swallow their errors and report unwritten changes as saved.
/// Row absence, visitor errors and driver-local encoding/decoding failures do
/// not abort it; only server errors carrying SQLSTATE mark it as failed.
/// A successful savepoint rollback restores the earlier failure state: PostgreSQL
/// clears the inner abort too, letting recovery commit its discard record.
pub struct PostgresTx<'s, S: PostgresSession> {
    session: &'s S,
    actors: Option<&'s EventActors>,
    name: &'static str,
    open: bool,
    actions: crate::infrastructure::sql_exec::CommitActions,
    writing: bool,
    statement_failed: Cell<bool>,
}

impl<'s, S: PostgresSession> PostgresTx<'s, S> {
    // Migration transactions have no store connection state to propagate.
    fn begin(session: &'s S, name: &'static str, begin: &str) -> Result<Self> {
        session
            .run(begin)
            .with_context(|| format!("begin the transaction {name}"))?;
        Ok(Self {
            session,
            actors: None,
            name,
            open: true,
            actions: crate::infrastructure::sql_exec::CommitActions::new(),
            writing: begin == BEGIN_WRITE_SQL,
            statement_failed: Cell::new(false),
        })
    }

    fn statement<T>(&self, result: SqlResult<T>) -> SqlResult<T> {
        if result.as_ref().is_err_and(statement_was_rejected) {
            self.statement_failed.set(true);
        }
        result
    }

    // Repeat before each statement, including after a savepoint rollback: its
    // SET LOCAL effects may have rolled back while the connection state did not.
    fn sync_actors(&self) -> SqlResult<()> {
        if let Some(actors) = self.actors {
            let (sql, values) = event_actor::settings(actors);
            self.statement(self.session.query(&sql, &values))?;
        }
        Ok(())
    }

    pub fn commit(mut self) -> Result<()> {
        if self.statement_failed.get() {
            // Drop rolls back, including commit actions, even if rollback fails.
            anyhow::bail!(
                "cannot commit the transaction {}: a statement failed",
                self.name
            );
        }
        self.session
            .run(COMMIT_SQL)
            .with_context(|| format!("commit the transaction {}", self.name))?;
        self.open = false;
        self.actions.committed();
        Ok(())
    }
}

impl<S: PostgresSession> SqlRead for PostgresTx<'_, S> {
    fn in_write_transaction(&self) -> bool {
        self.writing
    }

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Postgres
    }

    fn visit_rows(
        &self,
        sql: &str,
        params: &[SqlValue],
        visit: &mut dyn FnMut(&SqlRow<'_>) -> SqlResult<bool>,
    ) -> SqlResult<()> {
        self.sync_actors()?;
        for row in self.statement(self.session.query(&numbered_parameters(sql), params))? {
            if !visit(&SqlRow::new(&row))? {
                break;
            }
        }
        Ok(())
    }
}

impl<S: PostgresSession> SqlWrite for PostgresTx<'_, S> {
    fn after_commit(&self, action: Box<dyn FnOnce()>) -> SqlResult<()> {
        self.actions.push(action);
        Ok(())
    }

    fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<usize> {
        self.sync_actors()?;
        let changed = self.statement(self.session.execute(&numbered_parameters(sql), params))?;
        Ok(usize::try_from(changed).unwrap_or(usize::MAX))
    }

    fn insert_id(&self, sql: &str, params: &[SqlValue]) -> SqlResult<i64> {
        self.insert_id_if_inserted(sql, params)?
            .ok_or(SqlError::NoRows)
    }

    fn insert_id_if_inserted(&self, sql: &str, params: &[SqlValue]) -> SqlResult<Option<i64>> {
        self.query_row(&returning_id(sql), params, |row| row.get(0))
            .optional()
    }
}

/// An exclusively borrowed transaction's savepoint. The fixed name is safe
/// because no other guard can be opened on that transaction while it lives.
/// Rollback must release too: ROLLBACK TO alone leaves the savepoint open.
pub struct PostgresSavepoint<'tx, 's, S: PostgresSession> {
    tx: &'tx mut PostgresTx<'s, S>,
    open: bool,
    checkpoint: usize,
    statement_failed_before: bool,
}

impl<'s, S: PostgresSession> SqlSavepoints for PostgresTx<'s, S> {
    type Savepoint<'tx>
        = PostgresSavepoint<'tx, 's, S>
    where
        Self: 'tx;

    fn savepoint(&mut self) -> Result<Self::Savepoint<'_>> {
        if let Err(error) = self.session.run("SAVEPOINT dagq_savepoint") {
            self.statement_failed.set(true);
            return Err(error);
        }
        let checkpoint = self.actions.checkpoint();
        Ok(PostgresSavepoint {
            checkpoint,
            statement_failed_before: self.statement_failed.get(),
            tx: self,
            open: true,
        })
    }
}

impl<S: PostgresSession> SqlSavepoint for PostgresSavepoint<'_, '_, S> {
    fn commit(mut self) -> Result<()> {
        if let Err(error) = self.tx.session.run("RELEASE SAVEPOINT dagq_savepoint") {
            // Drop still cleans up, but a failed commit must not erase the error.
            self.statement_failed_before = true;
            self.tx.statement_failed.set(true);
            return Err(error);
        }
        self.open = false;
        Ok(())
    }
}

impl<S: PostgresSession> SqlRead for PostgresSavepoint<'_, '_, S> {
    fn in_write_transaction(&self) -> bool {
        self.tx.in_write_transaction()
    }

    fn dialect(&self) -> SqlDialect {
        self.tx.dialect()
    }

    fn visit_rows(
        &self,
        sql: &str,
        params: &[SqlValue],
        visit: &mut dyn FnMut(&SqlRow<'_>) -> SqlResult<bool>,
    ) -> SqlResult<()> {
        self.tx.visit_rows(sql, params, visit)
    }
}

impl<S: PostgresSession> SqlWrite for PostgresSavepoint<'_, '_, S> {
    fn after_commit(&self, action: Box<dyn FnOnce()>) -> SqlResult<()> {
        self.tx.after_commit(action)
    }

    fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<usize> {
        self.tx.execute(sql, params)
    }

    fn insert_id(&self, sql: &str, params: &[SqlValue]) -> SqlResult<i64> {
        self.tx.insert_id(sql, params)
    }

    fn insert_id_if_inserted(&self, sql: &str, params: &[SqlValue]) -> SqlResult<Option<i64>> {
        self.tx.insert_id_if_inserted(sql, params)
    }
}

impl<S: PostgresSession> Drop for PostgresSavepoint<'_, '_, S> {
    fn drop(&mut self) {
        if self.open {
            self.tx.actions.rollback_to(self.checkpoint);
            for sql in [
                "ROLLBACK TO SAVEPOINT dagq_savepoint",
                "RELEASE SAVEPOINT dagq_savepoint",
            ] {
                match self.tx.session.run(sql) {
                    Ok(()) if sql.starts_with("ROLLBACK TO") => {
                        self.tx.statement_failed.set(self.statement_failed_before);
                    }
                    Ok(()) => {}
                    Err(error) => {
                        self.tx.statement_failed.set(true);
                        tracing::warn!(transaction = self.tx.name, error = %error, sql, "could not roll back the savepoint");
                    }
                }
            }
        }
    }
}

impl<S: PostgresSession> Drop for PostgresTx<'_, S> {
    fn drop(&mut self) {
        if self.open {
            self.actions.rollback_to(0);
        }
        if self.open
            && let Err(error) = self.session.run(ROLLBACK_SQL)
        {
            tracing::warn!(
                transaction = self.name,
                error = %error,
                "could not roll back the transaction"
            );
        }
    }
}

impl<S: PostgresSession> super::sql_exec::CommitTransaction for PostgresTx<'_, S> {
    fn commit(self) -> Result<()> {
        PostgresTx::commit(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::write_leader::LEASE;
    use crate::infrastructure::schema::BINARY_SCHEMA;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    #[test]
    fn session_intake_requires_only_transaction_entries() {
        use crate::domain::sessions::{HookEvent, INBOX, SessionHook};
        use crate::infrastructure::sessions::{self, Closing};
        fn intake(entries: &impl TransactionEntries, hook: &SessionHook) {
            let _read = sessions::read_before(entries, Closing::Spans(&[])).unwrap();
            assert_eq!(sessions::record_open_turns(entries).unwrap(), 0);
            assert!(sessions::hook_spans(entries).unwrap().is_empty());
            assert_eq!(
                sessions::close_inferred_hook_spans(entries, &[]).unwrap(),
                0
            );
            // This fake returns no clock or headless-planner row. These paths
            // still compile against the same entry-only boundary.
            assert!(sessions::record_hook(entries, hook).is_err());
            assert!(crate::infrastructure::session_tokens::record_session_tokens(entries).is_err());
        }
        let session = Recorded::taken();
        let store = PostgresStore::open(&session).unwrap();
        let hook = SessionHook {
            event: HookEvent::Start {
                source: "startup".into(),
            },
            kind: INBOX,
            model: None,
            session_id: "s".into(),
            transcript_path: None,
            cwd: None,
            planner_id: None,
            launch: None,
        };
        intake(&store, &hook);
    }

    /// A row of the one column `name`.
    fn one_column(name: &str, value: SqlValue) -> PostgresRow {
        PostgresRow::new(Rc::from([name.to_owned()]), vec![value])
    }

    /// The answer to `sql` when it reads the schema, which is at `schema`
    /// (this binary's when `None`).
    fn schema_answer(sql: &str, schema: Option<(i64, i64)>) -> Option<Vec<PostgresRow>> {
        if sql == migrate::HAS_SCHEMA_TABLE_SQL {
            return Some(vec![one_column("count", SqlValue::Integer(1))]);
        }
        (sql == migrate::READ_SCHEMA_SQL).then(|| {
            let (version, floor) = schema.unwrap_or((BINARY_SCHEMA, 1));
            let columns: Rc<[String]> = ["version".to_owned(), "floor".to_owned()].into();
            let values = vec![SqlValue::Integer(version), SqlValue::Integer(floor)];
            vec![PostgresRow::new(columns, values)]
        })
    }

    /// A session that records its statements and arguments, whose writes
    /// change `changed` rows and whose queries answer `answers`.
    #[derive(Default)]
    struct Recorded {
        statements: RefCell<Vec<String>>,
        bound: RefCell<Vec<Vec<SqlValue>>>,
        answers: RefCell<Vec<PostgresRow>>,
        // Settings record their position among business statements separately.
        settings: RefCell<Vec<(usize, Vec<SqlValue>)>>,
        changed: Cell<u64>,
        begin_fails: Cell<bool>,
        commit_fails: Cell<bool>,
        rejected: RefCell<Option<String>>,
        local_failure: RefCell<Option<SqlError>>,
        schema: Cell<Option<(i64, i64)>>,
    }

    impl Recorded {
        fn reject(&self, sql: &str) -> SqlResult<()> {
            if self.rejected.borrow().as_deref() == Some(sql) {
                return Err(exec::session_error(
                    "statement rejected".into(),
                    Some("23505"),
                ));
            }
            Ok(())
        }

        fn taken() -> Self {
            let session = Self::default();
            session.changed.set(1);
            session
        }
    }

    impl PostgresSession for &Recorded {
        fn run(&self, sql: &str) -> Result<()> {
            if self.begin_fails.get() && sql.starts_with("BEGIN") {
                anyhow::bail!("the connection is gone");
            }
            self.statements.borrow_mut().push(sql.to_owned());
            self.reject(sql)?;
            if self.commit_fails.get() && sql == COMMIT_SQL {
                anyhow::bail!("commit failed");
            }
            Ok(())
        }

        fn query(&self, sql: &str, params: &[SqlValue]) -> SqlResult<Vec<PostgresRow>> {
            if sql == event_actor::settings(&EventActors::new(process_actor())).0 {
                self.reject(sql)?;
                self.settings
                    .borrow_mut()
                    .push((self.statements.borrow().len(), params.to_vec()));
                return Ok(vec![]);
            }
            self.statements.borrow_mut().push(sql.to_owned());
            self.reject(sql)?;
            if let Some(rows) = schema_answer(sql, self.schema.get()) {
                return Ok(rows);
            }
            if sql == IDENTITY_SQL {
                let columns: Rc<[String]> = ["holder".to_owned(), "generation".to_owned()].into();
                let values = vec![SqlValue::Text("service-1".to_owned()), SqlValue::Integer(7)];
                return Ok(vec![PostgresRow::new(columns, values)]);
            }
            self.bound.borrow_mut().push(params.to_vec());
            if let Some(error) = self.local_failure.take() {
                return Err(error);
            }
            Ok(self.answers.take())
        }

        fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<u64> {
            self.statements.borrow_mut().push(sql.to_owned());
            self.reject(sql)?;
            self.bound.borrow_mut().push(params.to_vec());
            if let Some(error) = self.local_failure.take() {
                return Err(error);
            }
            Ok(self.changed.get())
        }
    }

    /// The statements after the store opened.
    fn after_open(session: &Recorded) -> Vec<String> {
        session.statements.borrow()[4..].to_vec()
    }

    #[test]
    fn the_shared_queue_delegates_fenced_writes_and_snapshot_reads() {
        let session = Recorded::taken();
        let queue = PostgresQueue::open(&session, "runs".into()).unwrap();
        let tx = queue.write_entry("queue-write").unwrap();
        tx.commit().unwrap();
        let read = queue.read_entry("queue-read").unwrap();
        drop(read);
        assert_eq!(
            after_open(&session),
            [
                BEGIN_WRITE_SQL.to_owned(),
                fence_sql(),
                COMMIT_SQL.to_owned(),
                BEGIN_READ_SQL.to_owned(),
                ROLLBACK_SQL.to_owned(),
            ]
        );
        assert_eq!(
            session.bound.borrow()[0][0],
            SqlValue::Text("service-1".into())
        );
        assert_eq!(session.bound.borrow()[0][1], SqlValue::Integer(7));
    }

    /// The fake session exercises successful, dropped and failed commits,
    /// with both release and rollback of the savepoint's registrations.
    #[test]
    fn commit_actions_follow_the_transaction_and_savepoint_outcome() {
        for release in [false, true] {
            for outcome in ["commit", "rollback", "failure"] {
                let session = Recorded::taken();
                let store = PostgresStore::open(&session).unwrap();
                let called = Rc::new(RefCell::new(Vec::new()));
                let mut tx = store.write_entry("actions").unwrap();
                assert!(tx.in_write_transaction());
                let action = called.clone();
                tx.after_commit(Box::new(move || action.borrow_mut().push("before")))
                    .unwrap();
                {
                    let savepoint = tx.savepoint().unwrap();
                    let action = called.clone();
                    savepoint
                        .after_commit(Box::new(move || action.borrow_mut().push("inside")))
                        .unwrap();
                    if release {
                        savepoint.commit().unwrap();
                    }
                }
                let action = called.clone();
                tx.after_commit(Box::new(move || action.borrow_mut().push("after")))
                    .unwrap();
                assert!(called.borrow().is_empty());
                match outcome {
                    "commit" => tx.commit().unwrap(),
                    "failure" => {
                        session.commit_fails.set(true);
                        assert!(tx.commit().is_err());
                    }
                    _ => drop(tx),
                }
                crate::infrastructure::sql_exec::flush_after_commit();
                assert_eq!(
                    *called.borrow(),
                    if outcome != "commit" {
                        vec![]
                    } else if release {
                        vec!["before", "inside", "after"]
                    } else {
                        vec!["before", "after"]
                    }
                );
                session.commit_fails.set(false);
                let read = store.read_entry("read-actions").unwrap();
                assert!(!read.in_write_transaction());
            }
        }
    }

    #[test]
    fn entries_load_actor_settings_and_refresh_them_before_each_statement() {
        use crate::domain::actor::ActorRole;
        for write in [false, true] {
            let session = Recorded::taken();
            let store = PostgresStore::open(&session).unwrap();
            store.set_actor(ActorContext::instance(ActorRole::Supervisor, 42));
            let tx = if write {
                store.write_entry("actor")
            } else {
                store.read_entry("actor")
            }
            .unwrap();
            let initial = event_actor::settings(&store.actors).1;
            // BEGIN, then the fence for a write, precede the initial settings.
            assert_eq!(
                *session.settings.borrow(),
                [(if write { 6 } else { 5 }, initial)]
            );
            let previous = store.request(Some("job'日本語".into()));
            store.set_actor(ActorContext::user());
            tx.visit_rows("SELECT dagq_actor_role()", &[], &mut |_| Ok(true))
                .unwrap();
            assert_eq!(
                session.settings.borrow().last().unwrap().1,
                sql_params!["user", "user", "job'日本語"]
            );
            store.request(previous);
            if write {
                tx.execute("UPDATE t SET v=1", &[]).unwrap();
            } else {
                tx.visit_rows("SELECT dagq_requested_by()", &[], &mut |_| Ok(true))
                    .unwrap();
            }
            assert_eq!(
                session.settings.borrow().last().unwrap().1,
                sql_params!["user", "user", ""]
            );
            let start = if write { 6 } else { 5 };
            assert_eq!(
                session
                    .settings
                    .borrow()
                    .iter()
                    .map(|(at, _)| *at)
                    .collect::<Vec<_>>(),
                [start, start, start + 1]
            );
            drop(tx);
        }
    }

    #[test]
    fn a_savepoint_releases_or_rolls_back_then_releases_before_the_outer_commit() {
        for commit in [false, true] {
            let session = Recorded::taken();
            let store = PostgresStore::open(&session).unwrap();
            let mut tx = store.write_entry("savepoint").unwrap();
            session.statements.borrow_mut().clear();
            {
                let savepoint = tx.savepoint().unwrap();
                assert_eq!(savepoint.dialect(), SqlDialect::Postgres);
                savepoint
                    .execute("UPDATE t SET v=?1", sql_params!["inside"])
                    .unwrap();
                if commit {
                    savepoint.commit().unwrap();
                }
            }
            tx.execute("UPDATE t SET v=?1", sql_params!["after"])
                .unwrap();
            tx.commit().unwrap();
            let mut expected = vec!["SAVEPOINT dagq_savepoint", "UPDATE t SET v=$1"];
            if !commit {
                expected.push("ROLLBACK TO SAVEPOINT dagq_savepoint");
            }
            expected.extend([
                "RELEASE SAVEPOINT dagq_savepoint",
                "UPDATE t SET v=$1",
                "COMMIT",
            ]);
            assert_eq!(*session.statements.borrow(), expected);
            assert_eq!(
                *session.bound.borrow().last().unwrap(),
                sql_params!["after"]
            );
        }
    }

    #[test]
    fn rejected_statements_roll_back_instead_of_committing_both_entries() {
        for write in [false, true] {
            for operation in ["query", "execute", "insert"] {
                let session = Recorded::taken();
                let store = PostgresStore::open(&session).unwrap();
                let tx = if write {
                    store.write_entry("failed-write")
                } else {
                    store.read_entry("failed-read")
                }
                .unwrap();
                let sql = match operation {
                    "insert" => "INSERT INTO t DEFAULT VALUES RETURNING id",
                    _ => "rejected statement",
                };
                session.rejected.replace(Some(sql.into()));
                let result = match operation {
                    "query" => tx.visit_rows(sql, &[], &mut |_| Ok(true)),
                    "execute" => tx.execute(sql, &[]).map(|_| ()),
                    _ => tx
                        .insert_id("INSERT INTO t DEFAULT VALUES", &[])
                        .map(|_| ()),
                };
                assert!(result.is_err());
                session.rejected.replace(None);
                let called = Rc::new(Cell::new(false));
                let action = called.clone();
                tx.after_commit(Box::new(move || action.set(true))).unwrap();
                let error = tx.commit().unwrap_err().to_string();
                assert!(error.contains(if write { "failed-write" } else { "failed-read" }));
                assert!(error.contains("a statement failed"));
                let statements = session.statements.borrow();
                assert_eq!(statements.last().unwrap(), ROLLBACK_SQL);
                assert!(!statements.iter().any(|sql| sql == COMMIT_SQL));
                crate::infrastructure::sql_exec::flush_after_commit();
                assert!(!called.get());
            }
        }
    }

    #[test]
    fn absent_rows_and_visitor_errors_do_not_abort_a_transaction() {
        for write in [false, true] {
            let session = Recorded::taken();
            let store = PostgresStore::open(&session).unwrap();
            let tx = if write {
                store.write_entry("row-errors")
            } else {
                store.read_entry("row-errors")
            }
            .unwrap();
            assert!(matches!(
                tx.query_row("SELECT n FROM t", &[], |r| r.get::<_, i64>(0)),
                Err(SqlError::NoRows)
            ));
            assert!(
                tx.query_row("SELECT n FROM t", &[], |r| r.get::<_, i64>(0))
                    .optional()
                    .unwrap()
                    .is_none()
            );
            session
                .answers
                .replace(vec![one_column("n", SqlValue::Text("text".into()))]);
            assert!(matches!(
                tx.query_row("SELECT n FROM t", &[], |r| r.get::<_, i64>(0)),
                Err(SqlError::WrongType { .. })
            ));
            session
                .answers
                .replace(vec![one_column("n", SqlValue::Integer(1))]);
            assert!(matches!(
                tx.visit_rows("SELECT n FROM t", &[], &mut |_| Err(SqlError::Conversion {
                    index: 0,
                    stored: crate::infrastructure::sql_exec::ValueKind::Integer,
                    cause: "invalid domain value".into(),
                })),
                Err(SqlError::Conversion { .. })
            ));
            tx.commit().unwrap();
            assert_eq!(session.statements.borrow().last().unwrap(), COMMIT_SQL);
        }
    }

    #[test]
    fn driver_local_conversions_leave_both_entries_committable() {
        for write in [false, true] {
            for operation in ["decode", "encode-query", "encode-execute"] {
                let session = Recorded::taken();
                let store = PostgresStore::open(&session).unwrap();
                let tx = if write {
                    store.write_entry("local-conversion")
                } else {
                    store.read_entry("local-conversion")
                }
                .unwrap();
                if write {
                    tx.execute("INSERT INTO t VALUES (1)", &[]).unwrap();
                }
                // The driver uses this same classification after ToSql/FromSql
                // fail: no server SQLSTATE, even though the driver returned Err.
                let source = if operation == "decode" {
                    exec::decode_column("numeric", Some(&[])).unwrap_err()
                } else {
                    exec::encode_parameter(&SqlValue::Text("invalid".into()), "int8").unwrap_err()
                };
                session
                    .local_failure
                    .replace(Some(exec::session_error(source, None)));
                let result = if operation == "encode-execute" {
                    tx.execute("INSERT INTO t VALUES (?)", sql_params!["invalid"])
                        .map(|_| ())
                } else if operation == "decode" {
                    tx.visit_rows("SELECT 1::numeric", &[], &mut |_| Ok(true))
                } else {
                    tx.visit_rows("SELECT ?::bigint", sql_params!["invalid"], &mut |_| {
                        Ok(true)
                    })
                };
                assert!(result.is_err());
                tx.commit().unwrap();
                let statements = session.statements.borrow();
                assert_eq!(statements.last().unwrap(), COMMIT_SQL);
                assert!(!statements.iter().any(|sql| sql == ROLLBACK_SQL));
            }
        }
    }

    #[test]
    fn savepoint_rollback_restores_only_failures_since_it_opened() {
        for failed_before in [false, true] {
            for outcome in ["rollback", "release-failure", "rollback-failure"] {
                let session = Recorded::taken();
                let store = PostgresStore::open(&session).unwrap();
                let mut tx = store.write_entry("savepoint-failure").unwrap();
                if failed_before {
                    session.rejected.replace(Some("before".into()));
                    assert!(tx.execute("before", &[]).is_err());
                }
                session.rejected.replace(None);
                {
                    let savepoint = tx.savepoint().unwrap();
                    session.rejected.replace(Some("inside".into()));
                    assert!(savepoint.execute("inside", &[]).is_err());
                    session.rejected.replace(match outcome {
                        "release-failure" => Some("RELEASE SAVEPOINT dagq_savepoint".into()),
                        "rollback-failure" => Some("ROLLBACK TO SAVEPOINT dagq_savepoint".into()),
                        _ => None,
                    });
                    if outcome == "release-failure" {
                        assert!(savepoint.commit().is_err());
                    }
                }
                session.rejected.replace(None);
                let should_commit = !failed_before && outcome == "rollback";
                assert_eq!(tx.commit().is_ok(), should_commit);
                let statements = session.statements.borrow();
                assert_eq!(
                    statements.last().unwrap(),
                    if should_commit {
                        COMMIT_SQL
                    } else {
                        ROLLBACK_SQL
                    }
                );
                assert_eq!(
                    statements.iter().any(|sql| sql == COMMIT_SQL),
                    should_commit
                );
            }
        }
    }

    #[test]
    fn failed_actor_settings_also_prevent_commit() {
        let session = Recorded::taken();
        let store = PostgresStore::open(&session).unwrap();
        let tx = store.read_entry("actor-failure").unwrap();
        session
            .rejected
            .replace(Some(event_actor::settings(&store.actors).0));
        assert!(tx.visit_rows("SELECT 1", &[], &mut |_| Ok(true)).is_err());
        assert!(tx.commit().is_err());
        assert_eq!(session.statements.borrow().last().unwrap(), ROLLBACK_SQL);
    }

    /// The fence is the statement the SQLite store runs: the shared text,
    /// but for the clock's expression and the parameters' spelling.
    #[test]
    fn the_fence_is_the_shared_statement_with_the_postgres_clock_and_parameters() {
        let now = SqlDialect::Postgres.now_millis();
        assert_eq!(
            fence_sql(),
            format!(
                "UPDATE write_leader SET lease_until_ms = {now} + $3 \
                 WHERE singleton = 1 AND (holder = $1 OR (holder IS NULL AND CAST($1 AS TEXT) IS NULL)) \
                 AND generation = $2"
            )
        );
        assert_eq!(fence_sql(), numbered_parameters(&renew_sql(&now)));
    }

    #[test]
    fn a_write_begins_read_committed_and_fences_first_with_the_identity_it_read() {
        let session = Recorded::taken();
        let store = PostgresStore::open(&session).unwrap();
        let tx = store.write_entry("a_write").unwrap();
        assert_eq!(
            after_open(&session),
            [
                "BEGIN ISOLATION LEVEL READ COMMITTED".to_owned(),
                fence_sql()
            ]
        );
        assert_eq!(
            *session.bound.borrow(),
            [[
                SqlValue::Text("service-1".to_owned()),
                SqlValue::Integer(7),
                SqlValue::Integer(i64::try_from(LEASE.as_millis()).unwrap())
            ]]
        );
        tx.commit().unwrap();
        assert_eq!(after_open(&session).last().unwrap(), "COMMIT");
        assert_eq!(after_open(&session).len(), 3);
    }

    #[test]
    fn a_fence_of_no_rows_is_not_leader_and_rolls_back_having_written_nothing() {
        let session = Recorded::default();
        let store = PostgresStore::open(&session).unwrap();
        let error = store.write_entry("a_write").err().unwrap();
        assert!(error.downcast_ref::<NotLeader>().is_some(), "{error:#}");
        assert_eq!(
            after_open(&session),
            [
                "BEGIN ISOLATION LEVEL READ COMMITTED".to_owned(),
                fence_sql(),
                "ROLLBACK".to_owned()
            ]
        );
    }

    #[test]
    fn a_read_begins_read_only_on_one_snapshot_and_does_not_fence() {
        let session = Recorded::default();
        let store = PostgresStore::open(&session).unwrap();
        let tx = store.read_entry("a_read").unwrap();
        assert_eq!(
            after_open(&session),
            ["BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY"]
        );
        drop(tx);
        assert!(session.bound.borrow().is_empty());
        assert_eq!(after_open(&session).last().unwrap(), "ROLLBACK");
    }

    #[test]
    fn a_transaction_that_never_began_does_not_roll_back() {
        let session = Recorded::taken();
        let store = PostgresStore::open(&session).unwrap();
        session.begin_fails.set(true);
        let error = store.write_entry("a_write").err().unwrap();
        assert!(format!("{error:#}").contains("begin the transaction a_write"));
        assert!(after_open(&session).is_empty());
    }

    #[test]
    fn opening_sets_the_session_s_zone_and_judges_the_schema_before_it_reads_the_leader_s_row() {
        let session = Recorded::default();
        PostgresStore::open(&session).unwrap();
        let statements = session.statements.borrow();
        assert_eq!(statements.first().unwrap(), "SET TIME ZONE 'UTC'");
        assert_eq!(statements.last().unwrap(), IDENTITY_SQL);
        assert_eq!(statements.len(), 4, "{statements:?}");
    }

    #[test]
    fn a_schema_this_binary_does_not_open_is_refused_before_the_leader_s_row_is_read() {
        for schema in [
            (BINARY_SCHEMA - 1, 1),
            (BINARY_SCHEMA + 1, BINARY_SCHEMA + 1),
        ] {
            let session = Recorded::default();
            session.schema.set(Some(schema));
            assert!(PostgresStore::open(&session).is_err(), "{schema:?}");
            assert!(
                !session
                    .statements
                    .borrow()
                    .contains(&IDENTITY_SQL.to_owned()),
                "{schema:?}"
            );
        }
    }

    /// A holder of `NULL` is bound as one, which the shared fence compares.
    #[test]
    fn a_leader_row_with_no_holder_fences_with_null() {
        struct NoHolder(Recorded);
        impl PostgresSession for &NoHolder {
            fn run(&self, sql: &str) -> Result<()> {
                (&self.0).run(sql)
            }
            fn query(&self, sql: &str, _: &[SqlValue]) -> SqlResult<Vec<PostgresRow>> {
                if let Some(rows) = schema_answer(sql, None) {
                    return Ok(rows);
                }
                let columns: Rc<[String]> = ["holder".to_owned(), "generation".to_owned()].into();
                Ok(vec![PostgresRow::new(
                    columns,
                    vec![SqlValue::Null, SqlValue::Integer(0)],
                )])
            }
            fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<u64> {
                (&self.0).execute(sql, params)
            }
        }
        let session = NoHolder(Recorded::taken());
        let store = PostgresStore::open(&session).unwrap();
        store.write_entry("a_write").unwrap().commit().unwrap();
        assert_eq!(session.0.bound.borrow()[0][0], SqlValue::Null);
    }

    #[test]
    fn opening_with_no_leader_row_fails_saying_so() {
        struct Empty;
        impl PostgresSession for Empty {
            fn run(&self, _: &str) -> Result<()> {
                Ok(())
            }
            fn query(&self, sql: &str, _: &[SqlValue]) -> SqlResult<Vec<PostgresRow>> {
                Ok(schema_answer(sql, None).unwrap_or_default())
            }
            fn execute(&self, _: &str, _: &[SqlValue]) -> SqlResult<u64> {
                Ok(0)
            }
        }
        let error = PostgresStore::open(Empty).err().unwrap();
        assert!(format!("{error:#}").contains("read the write leader's row"));
    }

    /// The entries hand out the execution port, in the Postgres dialect.
    #[test]
    fn a_transaction_runs_the_shared_sql_with_its_parameters_numbered() {
        let session = Recorded::taken();
        let store = PostgresStore::open(&session).unwrap();
        let tx = store.read_entry("a_read").unwrap();
        assert_eq!(tx.dialect(), SqlDialect::Postgres);
        session.answers.replace(vec![
            one_column("load", SqlValue::Real(0.5)),
            one_column("load", SqlValue::Null),
        ]);
        let loads = tx
            .query_map(
                "SELECT load FROM t WHERE a = ?1 AND b = '?'",
                sql_params!["x"],
                |row| row.get::<_, Option<f64>>("load"),
            )
            .unwrap();
        assert_eq!(loads, [Some(0.5), None]);
        drop(tx);
        assert_eq!(
            after_open(&session),
            [
                "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY",
                "SELECT load FROM t WHERE a = $1 AND b = '?'",
                "ROLLBACK"
            ]
        );
        assert_eq!(*session.bound.borrow(), [[SqlValue::Text("x".to_owned())]]);
    }

    #[test]
    fn query_row_stops_at_the_first_row() {
        let session = Recorded::taken();
        let store = PostgresStore::open(&session).unwrap();
        let tx = store.read_entry("a_read").unwrap();
        session.answers.replace(vec![
            one_column("n", SqlValue::Integer(1)),
            one_column("n", SqlValue::Integer(2)),
        ]);
        let first = tx.query_row("SELECT n FROM t", &[], |row| row.get::<_, i64>(0));
        assert_eq!(first.unwrap(), 1);
    }

    #[test]
    fn a_write_returns_the_rows_it_changed() {
        let session = Recorded::taken();
        let store = PostgresStore::open(&session).unwrap();
        let tx = store.write_entry("a_write").unwrap();
        session.changed.set(3);
        let changed = SqlWrite::execute(&tx, "UPDATE t SET a = ?", sql_params![1_i64]).unwrap();
        assert_eq!(changed, 3);
        assert_eq!(after_open(&session).last().unwrap(), "UPDATE t SET a = $1");
    }

    #[test]
    fn an_insert_reads_its_id_from_returning_and_no_row_is_none_or_an_error() {
        let session = Recorded::taken();
        let store = PostgresStore::open(&session).unwrap();
        let tx = store.write_entry("a_write").unwrap();
        let insert = "INSERT INTO asks (run) VALUES (?1)";
        let returning = "INSERT INTO asks (run) VALUES ($1) RETURNING id";

        session
            .answers
            .replace(vec![one_column("id", SqlValue::Integer(9))]);
        assert_eq!(tx.insert_id(insert, sql_params!["r"]).unwrap(), 9);
        assert_eq!(after_open(&session).last().unwrap(), returning);

        session
            .answers
            .replace(vec![one_column("id", SqlValue::Integer(10))]);
        assert_eq!(
            tx.insert_id_if_inserted(insert, sql_params!["r"]).unwrap(),
            Some(10)
        );
        assert_eq!(
            tx.insert_id_if_inserted(insert, sql_params!["r"]).unwrap(),
            None
        );
        assert!(matches!(
            tx.insert_id(insert, sql_params!["r"]),
            Err(SqlError::NoRows)
        ));
        assert_eq!(after_open(&session).last().unwrap(), returning);
    }
    #[test]
    fn leader_mutations_begin_unfenced_and_postgres_never_uses_local_proof() {
        use crate::application::WriteLeaderStore;
        let session = Recorded::taken();
        let store = PostgresStore::open(&session).unwrap();
        session
            .answers
            .replace(vec![one_column("generation", SqlValue::Integer(8))]);
        assert_eq!(
            store.take_write_leader("next", LEASE, true).unwrap(),
            Some(8)
        );
        assert_eq!(session.bound.borrow()[0][2], SqlValue::Bool(false));
        let statements = after_open(&session);
        assert_eq!(statements[0], BEGIN_WRITE_SQL);
        assert!(statements[1].contains("generation = generation + 1"));
        assert_eq!(
            statements[2],
            "SELECT generation FROM write_leader WHERE singleton = 1"
        );
        assert_eq!(statements[3], COMMIT_SQL);
        for release in [false, true] {
            let start = session.statements.borrow().len();
            let changed = if release {
                store.release_write_leader("next", 8).unwrap()
            } else {
                store.renew_write_leader("next", 8, LEASE).unwrap()
            };
            assert!(changed);
            let statements = session.statements.borrow();
            assert_eq!(statements[start], BEGIN_WRITE_SQL);
            assert_eq!(statements[start + 2], COMMIT_SQL);
            assert_eq!(statements.len(), start + 3);
        }
        session.changed.set(0);
        assert_eq!(
            store.take_write_leader("other", LEASE, false).unwrap(),
            None
        );
        assert!(!store.release_write_leader("next", 7).unwrap());
        assert!(!store.renew_write_leader("next", 7, LEASE).unwrap());
    }

    #[test]
    fn the_leader_reads_through_the_snapshot_entry_and_startup_identity_fences_writes() {
        use crate::application::WriteLeaderStore;
        let session = Recorded::taken();
        let store = PostgresStore::open(&session).unwrap();
        session.answers.replace(vec![PostgresRow::new(
            [
                "holder",
                "generation",
                "lease_until_ms",
                "acquired_at_ms",
                "now",
            ]
            .map(str::to_owned)
            .into(),
            vec![
                SqlValue::Text("next".into()),
                SqlValue::Integer(8),
                SqlValue::Integer(99),
                SqlValue::Integer(1),
                SqlValue::Integer(2),
            ],
        )]);
        let (row, now) = store.write_leader().unwrap();
        assert_eq!(
            (row.holder.as_deref(), row.generation, now),
            (Some("next"), 8, 2)
        );
        let statements = after_open(&session);
        assert_eq!(statements[0], BEGIN_READ_SQL);
        assert_eq!(statements.last().unwrap(), COMMIT_SQL);
        assert_eq!(statements.len(), 3);
        let store = store.with_write_identity(WriteIdentity {
            holder: Some("next".into()),
            generation: 8,
        });
        store
            .write_entry("startup_identity")
            .unwrap()
            .commit()
            .unwrap();
        assert_eq!(
            session.bound.borrow().last().unwrap()[..2],
            [SqlValue::Text("next".into()), SqlValue::Integer(8)]
        );
    }
}
