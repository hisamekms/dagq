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
pub(crate) mod search_write;
pub mod tls;

use self::exec::{PostgresRow, numbered_parameters, returning_id};
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
/// already numbered as Postgres spells them.
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
pub struct PostgresTx<'s, S: PostgresSession> {
    session: &'s S,
    actors: Option<&'s EventActors>,
    name: &'static str,
    open: bool,
    actions: crate::infrastructure::sql_exec::CommitActions,
    writing: bool,
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
        })
    }

    // Repeat before each statement, including after a savepoint rollback: its
    // SET LOCAL effects may have rolled back while the connection state did not.
    fn sync_actors(&self) -> SqlResult<()> {
        if let Some(actors) = self.actors {
            let (sql, values) = event_actor::settings(actors);
            self.session.query(&sql, &values)?;
        }
        Ok(())
    }

    pub fn commit(mut self) -> Result<()> {
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
        for row in self.session.query(&numbered_parameters(sql), params)? {
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
        let changed = self.session.execute(&numbered_parameters(sql), params)?;
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
}

impl<'s, S: PostgresSession> SqlSavepoints for PostgresTx<'s, S> {
    type Savepoint<'tx>
        = PostgresSavepoint<'tx, 's, S>
    where
        Self: 'tx;

    fn savepoint(&mut self) -> Result<Self::Savepoint<'_>> {
        self.session.run("SAVEPOINT dagq_savepoint")?;
        let checkpoint = self.actions.checkpoint();
        Ok(PostgresSavepoint {
            checkpoint,
            tx: self,
            open: true,
        })
    }
}

impl<S: PostgresSession> SqlSavepoint for PostgresSavepoint<'_, '_, S> {
    fn commit(mut self) -> Result<()> {
        self.tx.session.run("RELEASE SAVEPOINT dagq_savepoint")?;
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
                if let Err(error) = self.tx.session.run(sql) {
                    tracing::warn!(transaction = self.tx.name, error = %error, sql, "could not roll back the savepoint");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::write_leader::LEASE;
    use crate::infrastructure::schema::BINARY_SCHEMA;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

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
        schema: Cell<Option<(i64, i64)>>,
    }

    impl Recorded {
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
            if self.commit_fails.get() && sql == COMMIT_SQL {
                anyhow::bail!("commit failed");
            }
            Ok(())
        }

        fn query(&self, sql: &str, params: &[SqlValue]) -> SqlResult<Vec<PostgresRow>> {
            if sql == event_actor::settings(&EventActors::new(process_actor())).0 {
                self.settings
                    .borrow_mut()
                    .push((self.statements.borrow().len(), params.to_vec()));
                return Ok(vec![]);
            }
            self.statements.borrow_mut().push(sql.to_owned());
            if let Some(rows) = schema_answer(sql, self.schema.get()) {
                return Ok(rows);
            }
            if sql == IDENTITY_SQL {
                let columns: Rc<[String]> = ["holder".to_owned(), "generation".to_owned()].into();
                let values = vec![SqlValue::Text("service-1".to_owned()), SqlValue::Integer(7)];
                return Ok(vec![PostgresRow::new(columns, values)]);
            }
            self.bound.borrow_mut().push(params.to_vec());
            Ok(self.answers.take())
        }

        fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<u64> {
            self.statements.borrow_mut().push(sql.to_owned());
            self.bound.borrow_mut().push(params.to_vec());
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
