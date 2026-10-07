//! The queue the dialogue and record commands work on
//! ([`crate::application::commands::dialogue`]): the asks, notes, marks
//! and findings of a [`SqliteQueue`] (a mark through
//! [`crate::application::marks`], on the queue's injected clock). A new
//! ask notifies nobody here: the inbox's watch tells the person
//! (ADR-t1433-1 decision 2).

use crate::domain::EventKind;
use anyhow::Result;
use serde_json::Value;

use super::sqlite::SqliteQueue;
use crate::application::commands::DenialLog;
use crate::application::commands::dialogue::{DialogueStore, MarkChange};
use crate::application::{RunLog, TaskStore, marks};
use crate::domain::{
    Answerer, Ask, AskId, Finding, FindingId, FindingOutcome, FindingStatus, NewAsk, NewFinding,
    NewNote, PlannerId, RequestId, RunEvent,
};

/// A queue the dialogue commands work on.
pub struct DialogueQueue<'a> {
    pub queue: &'a mut SqliteQueue,
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

    fn request_planner(&self, request: RequestId) -> Result<Option<PlannerId>> {
        crate::application::commands::requests::RequestStore::request_planner(&*self.queue, request)
    }

    fn open_ask(&mut self, ask: NewAsk) -> Result<Value> {
        crate::application::ask::ask(self.queue, ask)
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
        let queue = &*self.queue;
        match change {
            MarkChange::Record { label, note, at } => marks::record_mark(
                queue,
                queue.generators().clock.as_ref(),
                &label,
                note.as_deref(),
                at,
                by,
            ),
            MarkChange::Retract(target) => marks::retract_mark(queue, target, by),
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
        covered_by: Option<crate::domain::TaskId>,
    ) -> Result<Finding> {
        self.queue
            .set_finding_status_covered(id, to, reason, by, covered_by)
    }
}
