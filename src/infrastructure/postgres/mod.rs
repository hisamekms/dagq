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
//! The transactions run no statements of the store yet: they do not
//! implement the execution port ([`super::sql_exec`]).

#[cfg(feature = "postgres")]
pub mod client;

use super::sql_dialect::SqlDialect;
use super::sql_exec::TransactionEntries;
use super::write_leader::{IDENTITY_SQL, WriteIdentity, millis, renew_sql};
use crate::application::NotLeader;
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

/// `sql` with the parameters spelled as the shared SQL spells them (`?1`,
/// `?`) spelled as Postgres does (`$1`), and a `?` taking the number after
/// the highest so far as SQLite's does. What is quoted is left as it is.
fn numbered_parameters(sql: &str) -> String {
    let mut numbered = String::with_capacity(sql.len());
    let mut highest = 0_u64;
    let mut quote = None;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, '\'' | '"') => quote = Some(c),
            (Some(open), _) if open == c => quote = None,
            (None, '?') => {
                let mut digits = String::new();
                while let Some(digit) = chars.next_if(char::is_ascii_digit) {
                    digits.push(digit);
                }
                let number = digits.parse().unwrap_or(highest + 1);
                highest = highest.max(number);
                numbered.push('$');
                numbered.push_str(&number.to_string());
                continue;
            }
            _ => {}
        }
        numbered.push(c);
    }
    numbered
}

/// What a driver answers for the entries.
pub trait PostgresSession {
    /// Run `sql`, a statement without parameters.
    fn run(&self, sql: &str) -> Result<()>;

    /// The holder and generation the query `sql` reads from its one row.
    fn leader_identity(&self, sql: &str) -> Result<(Option<String>, i64)>;

    /// Run the fence `sql` with its parameters and return the rows changed.
    fn fence(&self, sql: &str, holder: Option<&str>, generation: i64, lease_ms: i64)
    -> Result<u64>;
}

/// The Postgres store on one session, which writes as the identity it read
/// when it opened: like the SQLite store, it never refreshes it on a write.
pub struct PostgresStore<S> {
    session: S,
    identity: WriteIdentity,
}

impl<S: PostgresSession> PostgresStore<S> {
    /// The store on the newly connected `session`.
    pub fn open(session: S) -> Result<Self> {
        session.run(SESSION_SETUP_SQL)?;
        let (holder, generation) = session
            .leader_identity(IDENTITY_SQL)
            .context("read the write leader's row")?;
        Ok(Self {
            session,
            identity: WriteIdentity { holder, generation },
        })
    }
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
        let tx = PostgresTx::begin(&self.session, name, BEGIN_WRITE_SQL)?;
        let changed = self.session.fence(
            &fence_sql(),
            self.identity.holder.as_deref(),
            self.identity.generation,
            millis(crate::domain::write_leader::LEASE)?,
        )?;
        if changed == 0 {
            return Err(NotLeader.into());
        }
        Ok(tx)
    }

    fn read_entry(&self, name: &'static str) -> Result<PostgresTx<'_, S>> {
        PostgresTx::begin(&self.session, name, BEGIN_READ_SQL)
    }
}

/// A transaction of an entry, which rolls back unless it commits.
pub struct PostgresTx<'s, S: PostgresSession> {
    session: &'s S,
    name: &'static str,
    open: bool,
}

impl<'s, S: PostgresSession> PostgresTx<'s, S> {
    fn begin(session: &'s S, name: &'static str, begin: &str) -> Result<Self> {
        session
            .run(begin)
            .with_context(|| format!("begin the transaction {name}"))?;
        Ok(Self {
            session,
            name,
            open: true,
        })
    }

    pub fn commit(mut self) -> Result<()> {
        self.open = false;
        self.session
            .run(COMMIT_SQL)
            .with_context(|| format!("commit the transaction {}", self.name))
    }
}

impl<S: PostgresSession> Drop for PostgresTx<'_, S> {
    fn drop(&mut self) {
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
    use std::cell::{Cell, RefCell};

    /// A session that records its statements and whose fence changes
    /// `changed` rows.
    #[derive(Default)]
    struct Recorded {
        statements: RefCell<Vec<String>>,
        fences: RefCell<Vec<(Option<String>, i64, i64)>>,
        changed: Cell<u64>,
        begin_fails: Cell<bool>,
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
            Ok(())
        }

        fn leader_identity(&self, sql: &str) -> Result<(Option<String>, i64)> {
            self.statements.borrow_mut().push(sql.to_owned());
            Ok((Some("service-1".to_owned()), 7))
        }

        fn fence(
            &self,
            sql: &str,
            holder: Option<&str>,
            generation: i64,
            lease_ms: i64,
        ) -> Result<u64> {
            self.statements.borrow_mut().push(sql.to_owned());
            self.fences
                .borrow_mut()
                .push((holder.map(str::to_owned), generation, lease_ms));
            Ok(self.changed.get())
        }
    }

    /// The statements after the store opened.
    fn after_open(session: &Recorded) -> Vec<String> {
        session.statements.borrow()[2..].to_vec()
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
                 WHERE singleton = 1 AND (holder = $1 OR (holder IS NULL AND $1 IS NULL)) \
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
            *session.fences.borrow(),
            [(
                Some("service-1".to_owned()),
                7,
                i64::try_from(LEASE.as_millis()).unwrap()
            )]
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
        assert!(session.fences.borrow().is_empty());
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
    fn opening_sets_the_session_s_zone_before_it_reads_the_leader_s_row() {
        let session = Recorded::default();
        PostgresStore::open(&session).unwrap();
        assert_eq!(
            *session.statements.borrow(),
            ["SET TIME ZONE 'UTC'", IDENTITY_SQL]
        );
    }

    #[test]
    fn parameters_are_numbered_as_sqlite_numbers_them_outside_quotes() {
        assert_eq!(
            numbered_parameters("a = ?2 AND b = ?2"),
            "a = $2 AND b = $2"
        );
        assert_eq!(numbered_parameters("(?, ?4, ?)"), "($1, $4, $5)");
        assert_eq!(
            numbered_parameters("x = '?1' AND \"y?\" = ?1"),
            "x = '?1' AND \"y?\" = $1"
        );
        assert_eq!(
            numbered_parameters("x = 'it''s?' AND y = ?"),
            "x = 'it''s?' AND y = $1"
        );
    }
}
