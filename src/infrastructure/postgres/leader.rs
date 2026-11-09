//! The Postgres leader port: unfenced conditional mutations of the one row.
//! A machine sharing the store has no local proof, even if a caller supplies it.
//!
//! The leader's port is the one store port each adapter implements on its
//! own (with the search index): take, renew and release begin at the
//! adapter's unfenced entry ([`PostgresStore::leader_entry`]), which no
//! shared store implementation can reach, and run the shared statements and
//! judgements of [`write_leader`]. The queue type delegates to its entries.
use super::{PostgresQueue, PostgresSession, PostgresStore};
use crate::application::WriteLeaderStore;
use crate::domain::write_leader::WriteLeader;
use crate::infrastructure::sql_exec::{SqlRead, TransactionEntries};
use crate::infrastructure::write_leader;
use anyhow::Result;
use std::time::Duration;

impl<S: PostgresSession> WriteLeaderStore for PostgresStore<S> {
    fn write_leader(&self) -> Result<(WriteLeader, i64)> {
        let tx = self.read_entry("write_leader")?;
        let row = write_leader::read(&tx, &tx.dialect().now_millis())?;
        tx.commit()?;
        Ok(row)
    }

    fn take_write_leader(
        &self,
        holder: &str,
        lease: Duration,
        _local_proof: bool,
    ) -> Result<Option<i64>> {
        let tx = self.leader_entry("take_write_leader")?;
        let taken = write_leader::take(&tx, &tx.dialect().now_millis(), holder, lease, false)?;
        // A takeover that changed no row rolls back as the transaction drops.
        if taken.is_some() {
            tx.commit()?;
        }
        Ok(taken)
    }

    fn renew_write_leader(&self, holder: &str, generation: i64, lease: Duration) -> Result<bool> {
        let tx = self.leader_entry("renew_write_leader")?;
        let renewed =
            write_leader::renew(&tx, &tx.dialect().now_millis(), holder, generation, lease)?;
        tx.commit()?;
        Ok(renewed)
    }

    fn release_write_leader(&self, holder: &str, generation: i64) -> Result<bool> {
        let tx = self.leader_entry("release_write_leader")?;
        let released = write_leader::release(&tx, holder, generation)?;
        tx.commit()?;
        Ok(released)
    }
}

impl<S: PostgresSession> WriteLeaderStore for PostgresQueue<S> {
    fn write_leader(&self) -> Result<(WriteLeader, i64)> {
        self.entries.write_leader()
    }

    fn take_write_leader(
        &self,
        holder: &str,
        lease: Duration,
        local_proof: bool,
    ) -> Result<Option<i64>> {
        self.entries.take_write_leader(holder, lease, local_proof)
    }

    fn renew_write_leader(&self, holder: &str, generation: i64, lease: Duration) -> Result<bool> {
        self.entries.renew_write_leader(holder, generation, lease)
    }

    fn release_write_leader(&self, holder: &str, generation: i64) -> Result<bool> {
        self.entries.release_write_leader(holder, generation)
    }
}
