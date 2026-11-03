//! The write leader's row of the queue ([`WriteLeaderStore`], ADR-t3986-1
//! decisions 2 and 4; the table's columns are its migration's comment).
//! The statements are the shared SQL that SQLite and the later Postgres
//! store run alike: only the DB's clock now comes from the dialect
//! ([`now_millis`]), and each statement reads it where it runs, so a
//! lease's end is judged by the DB's clock and not the process's.
//!
//! The takeover's condition is the one [`crate::domain::write_leader::takeover`]
//! decides by, applied at the DB, so of two processes racing for the row
//! one changes no row and is told so.

use super::sql_dialect::now_millis;
use super::sqlite::SqliteQueue;
use crate::application::WriteLeaderStore;
use crate::domain::write_leader::WriteLeader;
use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, params};
use std::time::Duration;

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
/// now; the same update every write transaction will start with.
fn renew_sql(now: &str) -> String {
    format!(
        "UPDATE write_leader SET lease_until_ms = {now} + ?3 \
         WHERE singleton = 1 AND holder = ?1 AND generation = ?2",
    )
}

/// Release the row of `?1` at generation `?2`.
const RELEASE_SQL: &str = "UPDATE write_leader SET lease_until_ms = 0 \
     WHERE singleton = 1 AND holder = ?1 AND generation = ?2";

fn millis(lease: Duration) -> Result<i64> {
    i64::try_from(lease.as_millis()).context("a write leader's lease too long to record")
}

/// The statements of the row on `queue`, each reading the clock now as the
/// SQL expression `now`: the DB's clock ([`now_millis`]) for the port, a
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
            now: now_millis(),
        }
    }

    fn read(&self) -> Result<(WriteLeader, i64)> {
        let tx = self.queue.read_tx("write_leader")?;
        let read = tx
            .query_row(&read_sql(&self.now), [], |r| {
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
        tx.commit()?;
        Ok(read)
    }

    fn take(&self, holder: &str, lease: Duration, local_proof: bool) -> Result<Option<i64>> {
        let tx = self.queue.write_tx("take_write_leader")?;
        let taken = tx.execute(
            &take_sql(&self.now),
            params![holder, millis(lease)?, local_proof],
        )?;
        if taken == 0 {
            tx.finish()?;
            return Ok(None);
        }
        let generation = tx.query_row(TAKEN_SQL, [], |r| r.get(0))?;
        tx.commit()?;
        Ok(Some(generation))
    }

    fn renew(&self, holder: &str, generation: i64, lease: Duration) -> Result<bool> {
        let tx = self.queue.write_tx("renew_write_leader")?;
        let renewed = tx.execute(
            &renew_sql(&self.now),
            params![holder, generation, millis(lease)?],
        )?;
        tx.commit()?;
        Ok(renewed == 1)
    }

    fn release(&self, holder: &str, generation: i64) -> Result<bool> {
        let tx = self.queue.write_tx("release_write_leader")?;
        let released = tx.execute(RELEASE_SQL, params![holder, generation])?;
        tx.commit()?;
        Ok(released == 1)
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

    fn insert(queue: &SqliteQueue, singleton: i64) -> rusqlite::Result<usize> {
        let tx = queue.write_tx("test_insert").unwrap();
        tx.execute(
            "INSERT INTO write_leader (singleton, generation, lease_until_ms) VALUES (?1, 0, 0)",
            [singleton],
        )
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
        let db_clock = now_millis();
        assert_eq!(LeaderRow::at_db_clock(&queue).now, db_clock);
        // Each statement that reads the clock reads it where it runs.
        assert!(read_sql(&db_clock).contains(&format!("acquired_at_ms, {db_clock} FROM")));
        assert!(take_sql(&db_clock).contains(&format!("lease_until_ms <= {db_clock} OR ?3")));
        assert!(take_sql(&db_clock).contains(&format!("acquired_at_ms = {db_clock} ")));
        assert!(renew_sql(&db_clock).contains(&format!("lease_until_ms = {db_clock} + ?3")));
    }

    #[test]
    fn a_second_row_is_refused_whatever_its_singleton() {
        let queue = queue();
        for singleton in [1, 2, 0, -1] {
            let refused = insert(&queue, singleton).unwrap_err();
            assert!(
                matches!(
                    refused.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::ConstraintViolation)
                ),
                "{singleton}: {refused}"
            );
        }
        let tx = queue.read_tx("count").unwrap();
        let rows: i64 = tx
            .query_row("SELECT count(*) FROM write_leader", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
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
    fn a_released_row_is_taken_over_without_waiting() {
        let queue = queue();
        assert_eq!(at(&queue, 10_000).take("a", LEASE, false).unwrap(), Some(1));
        assert!(at(&queue, 10_001).release("a", 1).unwrap());
        let (row, _) = at(&queue, 10_002).read().unwrap();
        assert_eq!((row.generation, row.lease_until_ms), (1, 0));
        assert_eq!(at(&queue, 10_002).take("b", LEASE, false).unwrap(), Some(2));
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
}
