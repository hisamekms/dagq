//! Where the runtime operations record what they refused
//! ([`crate::application::commands::operations`]): the queue at a path,
//! opened only to write the refusal as the refused actor. The operations
//! open no queue before they are authorized (`init` and `migrate` may find
//! none, or one this binary cannot open); a refusal then goes unrecorded
//! and is still a refusal.

use crate::domain::EventKind;
use std::path::Path;

use anyhow::{Result, ensure};
use serde_json::Value;

use super::sqlite::SqliteQueue;
use crate::application::RunLog;
use crate::application::commands::DenialLog;
use crate::domain::ActorContext;

/// The queue at `db`, written as `actor`.
pub struct QueueDenials<'a> {
    pub db: &'a Path,
    pub actor: &'a ActorContext,
}

impl DenialLog for QueueDenials<'_> {
    fn record_denial(&self, payload: Value) -> Result<()> {
        ensure!(self.db.is_file(), "no queue at {}", self.db.display());
        let queue = SqliteQueue::open(self.db)?.with_actor(self.actor.clone());
        RunLog::record_queue_event(&queue, EventKind::AuthorizationDenied, payload).map(drop)
    }
}
