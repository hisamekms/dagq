//! The queue the dialogue and record commands work on
//! ([`crate::application::commands::dialogue`]): the asks, notes, marks
//! and findings of a [`SqliteQueue`], with the checkout that names the
//! repository and the backend that notifies the inbox of a new ask.

use crate::domain::EventKind;
use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use super::sqlite::SqliteQueue;
use crate::application::commands::DenialLog;
use crate::application::commands::dialogue::{DialogueStore, MarkChange};
use crate::application::{RunLog, TaskStore, WorkspaceBackend};
use crate::domain::{
    Answerer, Ask, AskId, Finding, FindingId, FindingOutcome, FindingStatus, NewAsk, NewFinding,
    NewNote, RunEvent,
};

/// A queue, with what opening an ask needs besides it.
pub struct DialogueQueue<'a> {
    pub queue: &'a mut SqliteQueue,
    pub checkout: &'a Path,
    pub cmux: &'a dyn WorkspaceBackend,
}

impl DenialLog for DialogueQueue<'_> {
    fn record_denial(&self, payload: Value) -> Result<()> {
        RunLog::record_queue_event(&*self.queue, EventKind::AuthorizationDenied, payload).map(drop)
    }
}

impl DialogueStore for DialogueQueue<'_> {
    fn read_ask(&self, id: AskId) -> Result<Ask> {
        self.queue.read_ask(id)
    }

    fn open_ask(&mut self, ask: NewAsk) -> Result<Value> {
        let binding = self
            .queue
            .repository_binding()?
            .map(std::path::PathBuf::from);
        let checkout = super::adapters::naming_checkout(binding.as_deref(), self.checkout);
        crate::application::ask::ask(self.queue, &checkout, ask, self.cmux)
    }

    fn answer(&mut self, id: AskId, text: &str, answerer: Answerer) -> Result<Ask> {
        self.queue.answer_as(id, text, answerer)
    }

    fn close_ask(&mut self, id: AskId) -> Result<Ask> {
        self.queue.close_ask(id)
    }

    fn add_note(&mut self, note: NewNote) -> Result<RunEvent> {
        TaskStore::add_note(self.queue, note)
    }

    fn mark(&mut self, change: MarkChange, by: &str) -> Result<Value> {
        match change {
            MarkChange::Record { label, note, at } => {
                crate::compose::record_mark(self.queue, &label, note.as_deref(), at, by)
            }
            MarkChange::Retract(target) => crate::compose::retract_mark(self.queue, target, by),
        }
    }

    fn record_finding(&mut self, finding: NewFinding) -> Result<FindingOutcome> {
        self.queue.record_finding(finding)
    }

    fn set_finding_status(
        &mut self,
        id: FindingId,
        to: FindingStatus,
        reason: &str,
        by: &str,
    ) -> Result<Finding> {
        self.queue.set_finding_status(id, to, reason, by)
    }
}
