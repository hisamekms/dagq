//! The Postgres store's schema: applying its series and judging its version
//! (ADR-t3986-1 decision 7).
//!
//! `dagq_schema` holds what SQLite's `user_version` and `schema_floor` hold,
//! in the same numbers, so [`SchemaState`] judges both stores alike: the
//! declarations and the floor are the SQLite migrations', and a Postgres file
//! only says what its version means here. An empty database gets the base and
//! everything after it in one transaction; `build.rs` has checked that the
//! series reaches this binary's schema.
//!
//! A database with a schema is migrated in the migration's order of the write
//! leader's row: it reads the holder and generation outside the transaction,
//! and the transaction's first statement locks the row as it read it, so the
//! leader's writes wait on that row and a takeover in between starts again
//! with nothing applied. No advisory lock, as for the leader's own writes.

use super::{BEGIN_WRITE_SQL, PostgresSession, PostgresTx, numbered_parameters, read_identity};
use crate::infrastructure::schema::{BINARY_SCHEMA, recorded_floor};
use crate::infrastructure::sql_exec::{SqlError, SqlRow, sql_params};
use crate::infrastructure::sqlite::{MigrationReport, SchemaState};
use crate::infrastructure::write_leader::{LOCK_MIGRATION_SQL, WriteIdentity};
use anyhow::{Context, Result, ensure};

/// The Postgres series as `build.rs` lists it.
pub struct PostgresSeries {
    /// The SQLite version [`Self::base`] makes.
    pub base_version: i64,
    pub base: &'static str,
    /// The file of each SQLite migration after the base, in their order.
    pub after: &'static [&'static str],
}

impl PostgresSeries {
    /// The version the whole series reaches.
    fn latest(&self) -> i64 {
        self.base_version + self.after.len() as i64
    }
}

/// This binary's series.
pub const SERIES: PostgresSeries = include!(concat!(env!("OUT_DIR"), "/postgres_migrations.rs"));

pub(super) const HAS_SCHEMA_TABLE_SQL: &str = "SELECT count(*) FROM pg_catalog.pg_class c \
     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
     WHERE n.nspname = current_schema() AND c.relname = 'dagq_schema'";
const OBJECTS_SQL: &str = "SELECT count(*) FROM pg_catalog.pg_class c \
     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
     WHERE n.nspname = current_schema() AND c.relkind IN ('r', 'p', 'v', 'm', 'f')";
pub(super) const READ_SCHEMA_SQL: &str = "SELECT version, floor FROM dagq_schema";

/// The one integer the query `sql` reads.
fn count<S: PostgresSession>(session: &S, sql: &str) -> Result<i64> {
    let rows = session.query(sql, &[])?;
    Ok(SqlRow::new(rows.first().ok_or(SqlError::NoRows)?).get(0)?)
}

/// The version and floor `dagq_schema` holds, if it has its row.
fn schema_row<S: PostgresSession>(session: &S) -> Result<Option<(i64, i64)>> {
    let rows = session.query(READ_SCHEMA_SQL, &[])?;
    Ok(rows
        .first()
        .map(|row| {
            let row = SqlRow::new(row);
            Ok::<_, SqlError>((row.get(0)?, row.get(1)?))
        })
        .transpose()?)
}

/// The schema of the database on `session`; `None` for an empty one that may
/// become a queue. Anything else without the version's table is an error.
fn read_schema<S: PostgresSession>(session: &S) -> Result<Option<SchemaState>> {
    if count(session, HAS_SCHEMA_TABLE_SQL)? == 0 {
        ensure!(
            count(session, OBJECTS_SQL)? == 0,
            "database is not an empty dagq queue"
        );
        return Ok(None);
    }
    let (version, floor) = schema_row(session)?.context("the queue's dagq_schema has no row")?;
    Ok(Some(SchemaState::of(version, floor)))
}

/// Refuse the database on `session` unless this binary opens it as it is:
/// one with no schema, one older than this binary's and one whose floor
/// refuses this binary.
pub fn check_opens<S: PostgresSession>(session: &S) -> Result<()> {
    read_schema(session)?
        .context("the database has no dagq schema; run init to create it")?
        .check_opens()
}

/// Apply what this binary knows and the database lacks, an empty database
/// included. What it did, `None` when nothing was pending. A breaking migration
/// on a database with a schema is refused
/// ([`SchemaState::refuse_breaking`]): a leader of the older binary may go on
/// through a compatible one, and nothing here stops a leader or takes over its
/// row first.
pub fn migrate<S: PostgresSession>(session: &S) -> Result<Option<MigrationReport>> {
    migrate_series(session, &SERIES, SchemaState::refuse_breaking)
}

/// [`migrate`] of `series`, `judge_pending` judging what a database with a
/// schema lacks before anything is applied.
fn migrate_series<S: PostgresSession>(
    session: &S,
    series: &PostgresSeries,
    judge_pending: fn(&SchemaState) -> Result<()>,
) -> Result<Option<MigrationReport>> {
    loop {
        let Some(state) = read_schema(session)? else {
            match create(session, series) {
                Ok(()) => return report(series, 0).map(Some),
                // Another init created the queue first: judge what it made.
                Err(error) => {
                    if read_schema(session)?.is_some() {
                        continue;
                    }
                    return Err(error);
                }
            }
        };
        state.check_floor()?;
        if state.schema_version >= series.latest() {
            return Ok(None);
        }
        ensure!(
            state.schema_version >= series.base_version,
            "queue schema version {} is older than the base of this binary's series, version {}",
            state.schema_version,
            series.base_version
        );
        judge_pending(&state)?;
        let WriteIdentity { holder, generation } =
            read_identity(session).context("read the write leader's row")?;
        let tx = PostgresTx::begin(session, "schema_migrations", BEGIN_WRITE_SQL)?;
        let lock = numbered_parameters(LOCK_MIGRATION_SQL);
        if session.execute(&lock, sql_params![holder.as_deref(), generation])? != 1 {
            // A takeover since the read: read the row again.
            continue;
        }
        // Read again under the lock: another migrate may have gone first.
        let (version, _) = schema_row(session)?.context("the queue's dagq_schema has no row")?;
        if version >= series.latest() {
            return Ok(None);
        }
        apply_after(session, series, version)?;
        tx.commit()?;
        return report(series, version).map(Some);
    }
}

/// Make the whole series on an empty database, in one transaction.
fn create<S: PostgresSession>(session: &S, series: &PostgresSeries) -> Result<()> {
    let tx = PostgresTx::begin(session, "schema_migrations", BEGIN_WRITE_SQL)?;
    session
        .run(series.base)
        .context("apply the Postgres base")?;
    session.run(&format!(
        "INSERT INTO dagq_schema (version, floor) VALUES ({}, {})",
        series.base_version,
        recorded_floor(series.base_version)?
    ))?;
    apply_after(session, series, series.base_version)?;
    tx.commit()
}

/// Apply the files after version `from` and record the version and floor
/// they reach, in the transaction open on `session`.
fn apply_after<S: PostgresSession>(session: &S, series: &PostgresSeries, from: i64) -> Result<()> {
    let applied = usize::try_from(from - series.base_version)?;
    for (index, migration) in series.after.iter().enumerate().skip(applied) {
        let version = series.base_version + index as i64 + 1;
        session
            .run(migration)
            .with_context(|| format!("apply queue migration {version} on Postgres"))?;
    }
    session.run(&format!(
        "UPDATE dagq_schema SET version = {}, floor = {}",
        series.latest(),
        recorded_floor(series.latest())?
    ))
}

fn report(series: &PostgresSeries, previous_version: i64) -> Result<MigrationReport> {
    let floor = recorded_floor(series.latest())?;
    Ok(MigrationReport {
        previous_version,
        schema_version: series.latest(),
        binary_schema_version: BINARY_SCHEMA,
        floor,
        applied: SchemaState::of(previous_version, floor).pending,
        backup: None,
    })
}

#[cfg(test)]
mod tests;
