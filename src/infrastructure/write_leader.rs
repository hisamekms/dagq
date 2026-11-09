//! The write leader's row of the queue ([`WriteLeaderStore`], ADR-t3986-1
//! decisions 2 and 4; the table's columns are its migration's comment).
//! The statements are the shared SQL that SQLite and the later Postgres
//! store run alike: only the DB's clock now comes from the dialect
//! ([`super::sql_dialect::SqlDialect::now_millis`]), and each statement reads
//! it where it runs, so a lease's end is judged by the DB's clock and not the
//! process's.
//!
//! The takeover's condition is the one [`crate::domain::write_leader::takeover`]
//! decides by, applied at the DB, so of two processes racing for the row
//! one changes no row and is told so.

use super::sql_exec::{OptionalRow, SqlRead, SqlWrite, TransactionEntries, sql_params};
use super::sqlite::{SqliteQueue, WriteTx};
use crate::application::WriteLeaderStore;
use crate::domain::write_leader::WriteLeader;
use anyhow::{Context, Result};
use std::time::Duration;

/// The caller's identity, retained by the service that opened the store.
/// Until startup takes over explicitly, init and open inherit the current row,
/// including the unheld generation zero. It is never refreshed by a write.
#[derive(Clone, Debug)]
pub struct WriteIdentity {
    pub holder: Option<String>,
    pub generation: i64,
}

/// The identity's columns of the row, in the order of [`WriteIdentity`].
pub(super) const IDENTITY_SQL: &str =
    "SELECT holder, generation FROM write_leader WHERE singleton = 1";

impl WriteIdentity {
    pub(super) fn read(tx: &impl SqlRead) -> Result<Self> {
        Ok(tx.query_row(IDENTITY_SQL, &[], |r| {
            Ok(Self {
                holder: r.get(0)?,
                generation: r.get(1)?,
            })
        })?)
    }
}

/// The first statement after BEGIN: match the caller's holder and generation,
/// and extend from the DB clock. Expiry is deliberately absent: only takeover
/// fences an old writer. NULL equality admits the unheld generation zero.
/// A zero-row update drops (rolls back) the transaction before returning the
/// retryable application error; it must never record a diagnostic write.
pub(super) fn fence(tx: &impl SqlWrite, identity: &WriteIdentity) -> Result<()> {
    let changed = tx.execute(
        &renew_sql(&tx.dialect().now_millis()),
        sql_params![
            identity.holder,
            identity.generation,
            millis(crate::domain::write_leader::LEASE)?
        ],
    )?;
    if changed == 0 {
        return Err(crate::application::NotLeader.into());
    }
    Ok(())
}

/// Migration locks the row it read outside the transaction, without extending
/// another service's lease. A takeover in between returns false, so the caller
/// rolls back and rereads. Even an unheld generation zero is locked this way.
pub(super) fn lock_migration(tx: &impl SqlWrite, identity: &WriteIdentity) -> Result<bool> {
    Ok(tx.execute(
        LOCK_MIGRATION_SQL,
        sql_params![identity.holder, identity.generation],
    )? == 1)
}

/// Lock the row of `?1` at generation `?2` without changing it; on either
/// store.
pub(super) const LOCK_MIGRATION_SQL: &str = "UPDATE write_leader SET generation = generation \
     WHERE singleton = 1 AND (holder = ?1 OR (holder IS NULL AND CAST(?1 AS TEXT) IS NULL)) AND generation = ?2";

/// The row and the DB's clock now.
fn read_sql(now: &str) -> String {
    format!(
        "SELECT holder, generation, lease_until_ms, acquired_at_ms, {now} \
         FROM write_leader WHERE singleton = 1",
    )
}

/// Take the row as `?1` for `?2` milliseconds, when its lease ran out or
/// was released, or when `?3` (the local proof) holds.
fn take_sql(now: &str) -> String {
    format!(
        "UPDATE write_leader \
         SET holder = ?1, generation = generation + 1, \
             lease_until_ms = {now} + ?2, acquired_at_ms = {now} \
         WHERE singleton = 1 AND (lease_until_ms <= {now} OR ?3)",
    )
}

/// The generation the takeover just took.
const TAKEN_SQL: &str = "SELECT generation FROM write_leader WHERE singleton = 1";

/// Extend the lease of `?1` at generation `?2` to `?3` milliseconds from
/// now; the same update every write transaction starts with, on either
/// store.
pub(super) fn renew_sql(now: &str) -> String {
    format!(
        "UPDATE write_leader SET lease_until_ms = {now} + ?3 \
         WHERE singleton = 1 AND (holder = ?1 OR (holder IS NULL AND CAST(?1 AS TEXT) IS NULL)) AND generation = ?2",
    )
}

/// Release the row of `?1` at generation `?2`.
const RELEASE_SQL: &str = "UPDATE write_leader SET lease_until_ms = 0 \
     WHERE singleton = 1 AND holder = ?1 AND generation = ?2";

pub(super) fn millis(lease: Duration) -> Result<i64> {
    i64::try_from(lease.as_millis()).context("a write leader's lease too long to record")
}

/// Read the leader and the DB clock in the caller's snapshot. `now` is
/// the dialect's clock expression (a fixed expression in adapter tests).
pub(super) fn read(tx: &impl SqlRead, now: &str) -> Result<(WriteLeader, i64)> {
    let read = tx
        .query_row(&read_sql(now), &[], |r| {
            Ok((
                WriteLeader {
                    holder: r.get(0)?,
                    generation: r.get(1)?,
                    lease_until_ms: r.get(2)?,
                    acquired_at_ms: r.get(3)?,
                },
                r.get(4)?,
            ))
        })
        .optional()?
        .context("the queue has no write leader's row")?;
    Ok(read)
}

/// Conditional takeover in an unfenced transaction; the caller commits it.
pub(super) fn take(
    tx: &impl SqlWrite,
    now: &str,
    holder: &str,
    lease: Duration,
    local_proof: bool,
) -> Result<Option<i64>> {
    if tx.execute(
        &take_sql(now),
        sql_params![holder, millis(lease)?, local_proof],
    )? == 0
    {
        return Ok(None);
    }
    Ok(Some(tx.query_row(TAKEN_SQL, &[], |r| r.get(0))?))
}

/// A breaking migration owns a fresh generation while it applies schema;
/// a compatible migration only locks the row, without changing its lease.
/// The caller has already checked drain under that lock. Bootstrap has no
/// row to take, so it creates the leader table with the rest of the schema.
pub(super) fn take_for_migration(
    tx: &impl SqlWrite,
    breaking: bool,
    row_exists: bool,
    local_proof: bool,
) -> Result<Option<(String, i64)>> {
    if !crate::domain::write_leader::migration_takes_leader(breaking, row_exists) {
        return Ok(None);
    }
    let holder = uuid::Uuid::new_v4().to_string();
    let generation = take(
        tx,
        &tx.dialect().now_millis(),
        &holder,
        crate::domain::write_leader::LEASE,
        local_proof,
    )?
    .ok_or(crate::application::NotLeader)?;
    Ok(Some((holder, generation)))
}

/// Conditional lease extension in an unfenced transaction.
pub(super) fn renew(
    tx: &impl SqlWrite,
    now: &str,
    holder: &str,
    generation: i64,
    lease: Duration,
) -> Result<bool> {
    Ok(tx.execute(
        &renew_sql(now),
        sql_params![holder, generation, millis(lease)?],
    )? == 1)
}

/// Conditional release in an unfenced transaction.
pub(super) fn release(tx: &impl SqlWrite, holder: &str, generation: i64) -> Result<bool> {
    Ok(tx.execute(RELEASE_SQL, sql_params![holder, generation])? == 1)
}

/// The statements of the row on `queue`, each reading the clock now as the
/// SQL expression `now`: the DB's clock
/// ([`super::sql_dialect::SqlDialect::now_millis`]) for the port, a
/// fixed value for the tests, which judge the lease's end against it
/// deterministically. Only the clock differs; the statements are the same.
struct LeaderRow<'q> {
    queue: &'q SqliteQueue,
    now: String,
}

impl<'q> LeaderRow<'q> {
    /// The row on `queue` read against the DB's clock.
    fn at_db_clock(queue: &'q SqliteQueue) -> Self {
        Self {
            queue,
            now: super::sql_dialect::SqlDialect::Sqlite.now_millis(),
        }
    }

    fn read(&self) -> Result<(WriteLeader, i64)> {
        let tx = self.queue.read_entry("write_leader")?;
        let read = read(&tx, &self.now)?;
        tx.commit()?;
        Ok(read)
    }

    fn take(&self, holder: &str, lease: Duration, local_proof: bool) -> Result<Option<i64>> {
        let tx = WriteTx::leader(
            &self.queue.conn,
            "take_write_leader",
            self.queue.write_hold_warn,
        )?;
        let generation = take(&tx, &self.now, holder, lease, local_proof)?;
        if generation.is_some() {
            tx.commit()?;
        } else {
            tx.finish()?;
        }
        Ok(generation)
    }

    fn renew(&self, holder: &str, generation: i64, lease: Duration) -> Result<bool> {
        let tx = WriteTx::leader(
            &self.queue.conn,
            "renew_write_leader",
            self.queue.write_hold_warn,
        )?;
        let renewed = renew(&tx, &self.now, holder, generation, lease)?;
        tx.commit()?;
        Ok(renewed)
    }

    fn release(&self, holder: &str, generation: i64) -> Result<bool> {
        let tx = WriteTx::leader(
            &self.queue.conn,
            "release_write_leader",
            self.queue.write_hold_warn,
        )?;
        let released = release(&tx, holder, generation)?;
        tx.commit()?;
        Ok(released)
    }
}

impl WriteLeaderStore for SqliteQueue {
    fn write_leader(&self) -> Result<(WriteLeader, i64)> {
        LeaderRow::at_db_clock(self).read()
    }

    fn take_write_leader(
        &self,
        holder: &str,
        lease: Duration,
        local_proof: bool,
    ) -> Result<Option<i64>> {
        LeaderRow::at_db_clock(self).take(holder, lease, local_proof)
    }

    fn renew_write_leader(&self, holder: &str, generation: i64, lease: Duration) -> Result<bool> {
        LeaderRow::at_db_clock(self).renew(holder, generation, lease)
    }

    fn release_write_leader(&self, holder: &str, generation: i64) -> Result<bool> {
        LeaderRow::at_db_clock(self).release(holder, generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::write_leader::LEASE;
    use crate::infrastructure::sql_dialect::SqlDialect;

    /// A migrated queue in a memdb of its own, kept while the queue is open.
    fn queue() -> SqliteQueue {
        let uri = format!("file:/write-leader-{}?vfs=memdb", uuid::Uuid::new_v4());
        SqliteQueue::init(uri).expect("migrate a memdb queue")
    }

    /// The row on `queue` with the clock fixed at `now_ms`.
    fn at(queue: &SqliteQueue, now_ms: i64) -> LeaderRow<'_> {
        LeaderRow {
            queue,
            now: now_ms.to_string(),
        }
    }

    /// [`LEASE`] in milliseconds.
    const LEASE_MS: i64 = LEASE.as_millis() as i64;

    #[test]
    fn breaking_migration_takes_applies_and_releases_but_compatible_only_locks() {
        let queue = queue();
        let generation = queue
            .take_write_leader("old", LEASE, true)
            .unwrap()
            .unwrap();
        let before = queue.write_leader().unwrap().0;
        let identity = WriteIdentity::read(&*queue.conn).unwrap();
        let tx = WriteTx::leader(&queue.conn, "migration_test", queue.write_hold_warn).unwrap();
        assert!(lock_migration(&tx, &identity).unwrap());
        assert!(
            take_for_migration(&tx, false, true, true)
                .unwrap()
                .is_none()
        );
        tx.commit().unwrap();
        assert_eq!(queue.write_leader().unwrap().0, before);
        let tx = WriteTx::leader(&queue.conn, "migration_test", queue.write_hold_warn).unwrap();
        assert!(lock_migration(&tx, &identity).unwrap());
        let (holder, taken) = take_for_migration(&tx, true, true, true).unwrap().unwrap();
        assert_eq!(taken, generation + 1);
        tx.execute_batch("CREATE TABLE migrated_under_leader(value INTEGER)")
            .unwrap();
        assert!(release(&tx, &holder, taken).unwrap());
        tx.commit().unwrap();
        let row = queue.write_leader().unwrap().0;
        assert_eq!(row.generation, taken);
        assert_eq!(row.lease_until_ms, 0);
        assert_eq!(
            queue
                .conn
                .query_row("SELECT count(*) FROM migrated_under_leader", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn the_migration_leaves_one_row_no_process_has_led() {
        let queue = queue();
        let (row, now) = at(&queue, 10_000).read().unwrap();
        assert_eq!(
            row,
            WriteLeader {
                holder: None,
                generation: 0,
                lease_until_ms: 0,
                acquired_at_ms: None,
            }
        );
        assert_eq!(now, 10_000);
    }

    #[test]
    fn the_port_judges_the_lease_by_the_dialects_db_clock() {
        let queue = queue();
        let db_clock = SqlDialect::Sqlite.now_millis();
        assert_eq!(LeaderRow::at_db_clock(&queue).now, db_clock);
        // Each statement that reads the clock reads it where it runs.
        assert!(read_sql(&db_clock).contains(&format!("acquired_at_ms, {db_clock} FROM")));
        assert!(take_sql(&db_clock).contains(&format!("lease_until_ms <= {db_clock} OR ?3")));
        assert!(take_sql(&db_clock).contains(&format!("acquired_at_ms = {db_clock} ")));
        assert!(renew_sql(&db_clock).contains(&format!("lease_until_ms = {db_clock} + ?3")));
    }

    #[test]
    fn a_takeover_advances_the_generation_only_once_the_lease_ran_out() {
        let queue = queue();
        assert_eq!(at(&queue, 10_000).take("a", LEASE, false).unwrap(), Some(1));
        let (row, _) = at(&queue, 10_000).read().unwrap();
        assert_eq!(
            row,
            WriteLeader {
                holder: Some("a".to_owned()),
                generation: 1,
                lease_until_ms: 10_000 + LEASE_MS,
                acquired_at_ms: Some(10_000),
            }
        );

        // The lease still runs, to its last millisecond: no row changes.
        for now in [10_000, 10_000 + LEASE_MS - 1] {
            assert_eq!(at(&queue, now).take("b", LEASE, false).unwrap(), None);
            assert_eq!(at(&queue, now).read().unwrap().0, row);
        }

        // At its end it has run out.
        let end = 10_000 + LEASE_MS;
        assert_eq!(at(&queue, end).take("b", LEASE, false).unwrap(), Some(2));
        let (row, _) = at(&queue, end).read().unwrap();
        assert_eq!(row.holder.as_deref(), Some("b"));
        assert_eq!(row.lease_until_ms, end + LEASE_MS);
        assert_eq!(row.acquired_at_ms, Some(end));
    }

    #[test]
    fn the_local_proof_takes_over_a_lease_still_running() {
        let queue = queue();
        assert_eq!(at(&queue, 10_000).take("a", LEASE, false).unwrap(), Some(1));
        assert_eq!(at(&queue, 10_001).take("b", LEASE, true).unwrap(), Some(2));
        let (row, _) = at(&queue, 10_001).read().unwrap();
        assert_eq!((row.holder.as_deref(), row.generation), (Some("b"), 2));
        assert_eq!(row.lease_until_ms, 10_001 + LEASE_MS);
    }

    #[test]
    fn only_the_holder_at_its_generation_renews_or_releases() {
        let queue = queue();
        assert_eq!(at(&queue, 10_000).take("a", LEASE, false).unwrap(), Some(1));
        assert_eq!(at(&queue, 10_000).take("b", LEASE, true).unwrap(), Some(2));
        let before = at(&queue, 10_000).read().unwrap().0;

        // The older generation, another holder, and the holder at another
        // generation change nothing.
        let later = at(&queue, 20_000);
        for (holder, generation) in [("a", 1), ("a", 2), ("b", 1), ("b", 3)] {
            assert!(!later.renew(holder, generation, LEASE).unwrap());
            assert!(!later.release(holder, generation).unwrap());
        }
        assert_eq!(later.read().unwrap().0, before);

        // The leader extends its lease from the clock now, even past its
        // end when nobody took over.
        assert!(later.renew("b", 2, LEASE).unwrap());
        assert_eq!(later.read().unwrap().0.lease_until_ms, 20_000 + LEASE_MS);
        let past_end = at(&queue, 90_000);
        assert!(past_end.renew("b", 2, LEASE).unwrap());
        let renewed = past_end.read().unwrap().0;
        assert_eq!(renewed.lease_until_ms, 90_000 + LEASE_MS);
        assert_eq!(renewed.generation, 2);

        assert!(past_end.release("b", 2).unwrap());
        assert_eq!(past_end.read().unwrap().0.lease_until_ms, 0);
    }

    #[test]
    fn an_implicit_write_also_refuses_the_stale_identity_without_changes() {
        let mut queue = queue();
        at(&queue, 10_000).take("a", LEASE, false).unwrap();
        queue.write_identity = Some(WriteIdentity::read(&*queue.conn).unwrap());
        at(&queue, 10_001).take("a", LEASE, true).unwrap();
        let changes = queue.conn.total_changes();
        let error = queue.fill_commit_message(1, "stale").unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::application::NotLeader>()
                .is_some()
        );
        assert_eq!(queue.conn.total_changes(), changes);
    }
}
