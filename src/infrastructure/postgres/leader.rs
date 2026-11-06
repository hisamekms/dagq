//! The Postgres leader port: unfenced conditional mutations of the one row.
//! A machine sharing the store has no local proof, even if a caller supplies it.
use super::{PostgresSession, PostgresStore};
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
        tx.commit()?;
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
