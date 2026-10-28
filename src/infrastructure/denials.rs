//! Where the host's `dagq` records what it refused itself
//! ([`crate::application::commands::Gate`],
//! [`crate::application::commands::operations`]): the queue service of
//! the queue at a path, sent the refusal as the caller's principal (its
//! token, or the user as the socket's peer), which records it only as its
//! own (`denial_record`). The queue is never opened for it. A caller the
//! service does not authenticate (an actor named with no token) is
//! recorded by the service as unauthenticated; a service that does not
//! answer, or no queue there, leaves the refusal unrecorded, and it is
//! still a refusal.

use std::{path::Path, time::Duration};

use anyhow::{Result, ensure};
use serde_json::Value;

use super::queue_service::Client;
use crate::application::commands::DenialLog;
use crate::domain::queue_service::UseCase;

/// How long a refusal waits for the service to take its record: a refusal
/// is returned at once whether it is recorded or not, and a service that
/// does not answer within it leaves it unrecorded.
const RECORD_TIMEOUT: Duration = Duration::from_secs(5);

/// The service of the queue at `db`, bound to the repository whose Git
/// common directory is `binding` for a repository queue, as the principal
/// `env` names.
pub struct ServiceDenials<'a> {
    pub db: &'a Path,
    pub binding: Option<&'a str>,
    pub env: &'a dyn Fn(&str) -> Option<String>,
}

impl DenialLog for ServiceDenials<'_> {
    fn record_denial(&self, payload: Value) -> Result<()> {
        ensure!(self.db.is_file(), "no queue at {}", self.db.display());
        // The DB a link names is its target's: its directory's service
        // serves it.
        let db = std::fs::canonicalize(self.db).unwrap_or_else(|_| self.db.to_path_buf());
        Client::host(&db, self.binding, self.env)
            .call_within(UseCase::DenialRecord, payload, RECORD_TIMEOUT)
            .map(drop)
    }
}
