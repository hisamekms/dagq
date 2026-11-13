//! The queue the dialogue and record commands work on
//! ([`crate::application::commands::dialogue`]): the asks, notes, marks
//! and findings of a [`StoreQueue`] (a mark through
//! [`crate::application::marks`], on the queue's injected clock). A new
//! ask notifies nobody here: the inbox's watch tells the person
//! (ADR-t1433-1 decision 2).

use anyhow::Result;
use serde_json::Value;

use crate::infrastructure::{sql_exec::TransactionEntries, store_queue::StoreQueue};

use crate::application::commands::DenialLog;
use crate::application::commands::dialogue::{DialogueStore, MarkChange};
use crate::application::{TaskStore, marks};
use crate::domain::{
    Answerer, Ask, AskId, Finding, FindingId, FindingOutcome, FindingStatus, NewAsk, NewFinding,
    NewNote, PlannerId, RequestId, RunEvent,
};

/// A queue the dialogue commands work on.
pub struct DialogueQueue<'a, E: TransactionEntries> {
    pub queue: &'a mut StoreQueue<E>,
}

impl<E: TransactionEntries> DenialLog for DialogueQueue<'_, E> {
    fn record_privileged(&self, payload: Value) -> Result<()> {
        DenialLog::record_privileged(&*self.queue, payload)
    }

    fn record_denial(&self, payload: Value) -> Result<()> {
        DenialLog::record_denial(&*self.queue, payload)
    }
}

impl<E: TransactionEntries> DialogueStore for DialogueQueue<'_, E> {
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

/// The direct-DB policy reads deployment settings itself, independently of
/// the caller's actor. Invalid/unavailable settings stay unknown and deny
/// landing; only a successful read with no issuer establishes local mode.
pub fn answer_policy(
    clock: &dyn crate::application::Clock,
) -> crate::domain::authorization::AnswerPolicy {
    policy_from_deployment(
        super::deployment_config::DeploymentConfig::from_process().ok(),
        clock.now(),
    )
}

fn policy_from_deployment(
    config: Option<super::deployment_config::DeploymentConfig>,
    now: i64,
) -> crate::domain::authorization::AnswerPolicy {
    crate::domain::authorization::AnswerPolicy::new(
        config.map(|config| config.value("DAGQ_OIDC_ISSUER").is_some()),
        now,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ActorContext, ActorRole, AskKind, Authorizer};
    use crate::infrastructure::deployment_config::DeploymentConfig;

    #[test]
    fn deployment_policy_reads_issuer_and_file_settings_and_fails_closed() {
        for source in ["local", "issuer", "file", "unreadable"] {
            let config = DeploymentConfig::read(
                |name| match (source, name) {
                    ("issuer", "DAGQ_OIDC_ISSUER") => Some("https://idp.example".into()),
                    ("file" | "unreadable", "DAGQ_OIDC_ISSUER_FILE") => Some("/issuer".into()),
                    ("issuer" | "file" | "unreadable", "DAGQ_OIDC_AUDIENCE") => Some("cli".into()),
                    _ => None,
                },
                |_| {
                    if source == "unreadable" {
                        Err(())
                    } else {
                        Ok(b"https://idp.example".to_vec())
                    }
                },
            );
            let policy = policy_from_deployment(config.ok(), 100);
            // No actor carries any deployment context.
            for actor in [
                ActorContext::user(),
                ActorContext::new(ActorRole::Inbox, "inbox"),
            ] {
                assert_eq!(
                    policy
                        .authorize_answer(&actor, &AskKind::ApproveLanding, "land: reviewed")
                        .is_ok(),
                    source == "local",
                    "{source}"
                );
                assert!(
                    policy
                        .authorize_answer(&actor, &AskKind::ApproveLanding, "cancel")
                        .is_ok()
                );
            }
        }
    }
}
