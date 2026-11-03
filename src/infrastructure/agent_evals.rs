//! The eval's events on the queue (ADR-t1728-1 decisions 4 and 5): queue
//! events of the `agent_eval_*` kinds, recorded and read through the run
//! log. No table of their own: a round's state is read back from them.
//! The production sets' events (ADR-t1728-1 (g)) are kept the same way.

use anyhow::Result;
use serde_json::Value;

use crate::application::EventStore;
use crate::application::agent_cases::CaseSetStore;
use std::path::Path;

use crate::application::agent_eval::{EvalStore, LaunchSettings};
use crate::domain::agent_eval::production;
use crate::domain::agent_eval::record::KINDS;
use crate::domain::{EventId, EventKind, RunEvent};
use crate::infrastructure::sqlite::SqliteQueue;

impl EvalStore for SqliteQueue {
    fn record_eval_event(&self, kind: EventKind, payload: Value) -> Result<EventId> {
        EventStore::record_queue_event(self, kind, payload)
    }

    fn eval_events(&self) -> Result<Vec<RunEvent>> {
        let kinds: Vec<&str> = KINDS.iter().map(|kind| kind.as_str()).collect();
        let upto = EventStore::latest_event_id(self)?;
        // Every one: the read takes its limit as SQLite's integer.
        let every = usize::try_from(i64::MAX).unwrap_or(usize::MAX);
        EventStore::events_of_between(self, &kinds, EventId::new(0), upto, every)
    }

    fn launch_settings(&self, agent: &str) -> Result<Option<LaunchSettings>> {
        bound_launch_settings(self.repository_binding()?.as_deref(), agent)
    }
}

/// What the `dagq.toml` in the main checkout of the repository whose Git
/// common directory is `binding` gives `agent`'s launch; none for a queue
/// bound to no repository or one whose checkout is not found.
pub fn bound_launch_settings(binding: Option<&str>, agent: &str) -> Result<Option<LaunchSettings>> {
    let Some(checkout) = binding.and_then(|common_dir| {
        crate::infrastructure::adapters::main_checkout_from_files(Path::new(common_dir))
    }) else {
        return Ok(None);
    };
    crate::infrastructure::run_env::load_eval_launch_settings(&checkout, agent).map(Some)
}

impl CaseSetStore for SqliteQueue {
    fn record_cases_event(&self, kind: EventKind, payload: Value) -> Result<EventId> {
        EventStore::record_queue_event(self, kind, payload)
    }

    fn cases_events(&self) -> Result<Vec<RunEvent>> {
        let kinds: Vec<&str> = production::KINDS.iter().map(|kind| kind.as_str()).collect();
        let upto = EventStore::latest_event_id(self)?;
        let every = usize::try_from(i64::MAX).unwrap_or(usize::MAX);
        EventStore::events_of_between(self, &kinds, EventId::new(0), upto, every)
    }
}
