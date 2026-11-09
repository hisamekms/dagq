//! The messages of landed commits recorded without one (ADR-0063 decision 3),
//! on the execution port: `migrate`'s client reads them with Git and sends
//! them back. Filling one updates its search copy in the same transaction
//! ([`super::search_writes`]), which SQLite's trigger keeps already.

use anyhow::Result;

use super::search_writes::{SearchWrite, maintain};
use super::sql_exec::CommitTransaction as _;
use super::sql_exec::{SqlRead, SqlWrite, TransactionEntries, sql_params};
use super::store_queue::StoreQueue;

/// A landed commit recorded without its message
/// ([`SqliteQueue::missing_commit_messages`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MissingCommitMessage {
    pub id: i64,
    pub commit: String,
    pub git_common_dir: Option<String>,
}

/// The landed commits without their message, oldest first.
pub(super) fn missing(tx: &impl SqlRead) -> Result<Vec<MissingCommitMessage>> {
    Ok(tx.query_map(
        "SELECT id, commit_sha, git_common_dir FROM landed_commits
         WHERE message IS NULL ORDER BY id",
        &[],
        |row| {
            Ok(MissingCommitMessage {
                id: row.get(0)?,
                commit: row.get(1)?,
                git_common_dir: row.get(2)?,
            })
        },
    )?)
}

/// Fill the message of the landed commit `id` when it still has none, and
/// its search copy: how many were filled (0 or 1).
pub(super) fn fill(tx: &impl SqlWrite, id: i64, message: &str) -> Result<usize> {
    let filled = tx.execute(
        "UPDATE landed_commits SET message = ?2 WHERE id = ?1 AND message IS NULL",
        sql_params![id, message],
    )?;
    if filled != 0 {
        maintain(tx, SearchWrite::Commit(id))?;
    }
    Ok(filled)
}

impl<E: TransactionEntries> StoreQueue<E> {
    /// Fill the message of each landed commit recorded without one, read by
    /// `read(git_common_dir, commit)` (the Git directory the landing
    /// recorded, if any). Returns how many were filled; a commit `read`
    /// cannot find keeps waiting.
    pub fn fill_commit_messages(
        &mut self,
        read: impl Fn(Option<&str>, &str) -> Option<String>,
    ) -> Result<usize> {
        let mut filled = 0;
        for missing in self.missing_commit_messages()? {
            if let Some(message) = read(missing.git_common_dir.as_deref(), &missing.commit) {
                filled += self.fill_commit_message(missing.id, &message)?;
            }
        }
        Ok(filled)
    }

    /// The landed commits recorded without their message, oldest first:
    /// what `migrate`'s client reads with Git and sends back
    /// ([`Self::fill_commit_message`]).
    pub fn missing_commit_messages(&self) -> Result<Vec<MissingCommitMessage>> {
        let tx = self.read_entry("missing_commit_messages")?;
        missing(&tx)
    }

    /// Fill the message of the landed commit `id` when it still has none,
    /// in the fenced write entry: how many were filled (0 or 1).
    pub fn fill_commit_message(&mut self, id: i64, message: &str) -> Result<usize> {
        let tx = self.write_entry("fill_commit_message")?;
        let filled = fill(&tx, id, message)?;
        tx.commit()?;
        Ok(filled)
    }
}
