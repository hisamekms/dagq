//! The eval's events on the queue (ADR-t1728-1 decisions 4 and 5): queue
//! events of the `agent_eval_*` kinds, recorded and read through the run
//! log. No table of their own: a round's state is read back from them.

use anyhow::Result;
use serde_json::Value;

use crate::application::EventStore;
use crate::application::agent_eval::EvalStore;
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
}
