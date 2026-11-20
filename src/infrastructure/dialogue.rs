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
    fn recipients(&self, ask: &Ask) -> Result<Vec<crate::domain::ask_routing::Recipient>> {
        self.queue.ask_recipients(ask)
    }
    fn read_ask(&self, id: AskId) -> Result<Ask> {
        self.queue.read_ask(id)
    }

    fn request_planner(&self, request: RequestId) -> Result<Option<PlannerId>> {
        crate::application::commands::requests::RequestStore::request_planner(&*self.queue, request)
    }

    fn open_ask(&mut self, ask: NewAsk) -> Result<Value> {
        crate::application::ask::ask(self.queue, ask)
    }

    fn answer(
        &mut self,
        id: AskId,
        text: &str,
        answerer: Answerer,
        actor: &crate::domain::ActorContext,
        team: Option<bool>,
    ) -> Result<Ask> {
        self.queue
            .answer_for(id, text, answerer, Some((actor, team)))
    }

    fn close_ask(
        &mut self,
        id: AskId,
        actor: &crate::domain::ActorContext,
        team: Option<bool>,
    ) -> Result<Ask> {
        self.queue.close_for(id, Some((actor, team)))
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

#[cfg(test)]
mod recipient_tests {
    use super::*;
    use crate::application::commands::dialogue::Dialogue;
    use crate::domain::{
        ActorContext, ActorRole, AskKind, AskReason, TaskId,
        actor::HumanRole,
        authorization::AnswerPolicy,
        delegate::DelegateChange,
        goal_owner::Owner,
        oidc::OidcIdentity,
        queue_service::{Member, TeamRole},
    };
    use crate::infrastructure::{
        delegates, members,
        sql_exec::{SqlRead, SqlWrite, sql_params},
        sqlite::SqliteQueue,
        store_writes,
    };
    use serde_json::json;

    #[test]
    fn human_dialogue_records_identity_and_runtime_close_keeps_legacy_payload() {
        let mut queue = SqliteQueue::init(format!(
            "file:/dialogue-routing-{}?vfs=memdb",
            uuid::Uuid::new_v4()
        ))
        .unwrap();
        let identity = |subject: &str| OidcIdentity {
            issuer: "https://id.example".into(),
            subject: subject.into(),
        };
        let role = HumanRole {
            role: TeamRole::Member,
            on_call: false,
        };
        let owner = Owner::new(identity("owner"));
        let user = ActorContext::new(
            ActorRole::User,
            format!("human:{}", serde_json::to_string(owner.identity()).unwrap()),
        )
        .with_human_role(role);
        let inbox =
            ActorContext::new(ActorRole::Inbox, "inbox").with_parent_role(identity("proxy"), role);
        let tx = queue.write_entry("routing_fixture").unwrap();
        for subject in ["owner", "proxy"] {
            members::add(
                &tx,
                &Member {
                    identity: identity(subject),
                    role: TeamRole::Member,
                    on_call: false,
                    display_name: None,
                    created_at: 1,
                    disabled_at: None,
                },
            )
            .unwrap();
        }
        let task = TaskId::new(SqlWrite::insert_id(&tx, "INSERT INTO tasks(title, description, acceptance, verification_commands, status) VALUES ('routing', '', '', '[]', 'draft')", sql_params![]).unwrap());
        store_writes::set_task_owner(&tx, task, Some(&owner)).unwrap();
        delegates::change(&tx, &owner, &DelegateChange::Add(identity("proxy")), 1).unwrap();
        tx.commit().unwrap();
        let open = |queue: &mut SqliteQueue| {
            queue
                .ask(NewAsk {
                    kind: AskKind::WorkerQuestion,
                    task_id: Some(task),
                    run_id: None,
                    question: "q".into(),
                    options: vec![],
                    asked_by: "worker".into(),
                    reason_category: AskReason::Scope,
                    finding_id: None,
                    request_id: None,
                    recommendation: None,
                    confidence: None,
                    topics: vec!["task_overlap".into()],
                })
                .unwrap()
                .ask
        };
        let events = |queue: &SqliteQueue, kind: &str| -> Vec<Value> {
            let tx = queue.read_entry("routing_events").unwrap();
            tx.query_map(
                "SELECT payload FROM run_events WHERE kind=?1 ORDER BY id",
                sql_params![kind],
                |r| {
                    let text: String = r.get(0)?;
                    Ok(serde_json::from_str(&text).unwrap())
                },
            )
            .unwrap()
        };
        let policy = AnswerPolicy::new(Some(true), 100);
        let ask = open(&mut queue);
        let answered = Dialogue::new(&mut DialogueQueue { queue: &mut queue }, &inbox, &policy)
            .answer(ask.id, "free answer")
            .unwrap();
        assert_eq!(
            answered.answer_authority,
            Some(crate::domain::AnswerAuthority::Delegated)
        );
        assert_eq!(
            events(&queue, "ask_answered")[0]["human_action"],
            json!({"issuer": "https://id.example", "subject": "proxy", "reason": "delegate"})
        );
        let tx = queue.write_entry("remove_proxy").unwrap();
        delegates::change(&tx, &owner, &DelegateChange::Remove(identity("proxy")), 2).unwrap();
        tx.commit().unwrap();
        let error = Dialogue::new(&mut DialogueQueue { queue: &mut queue }, &inbox, &policy)
            .close(ask.id)
            .unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::domain::AuthorizationError>()
                .is_some()
        );
        assert!(queue.read_ask(ask.id).unwrap().closed_at.is_none());
        assert!(events(&queue, "ask_closed").is_empty());
        Dialogue::new(&mut DialogueQueue { queue: &mut queue }, &user, &policy)
            .close(ask.id)
            .unwrap();
        assert_eq!(
            events(&queue, "ask_closed")[0]["human_action"],
            json!({"issuer": "https://id.example", "subject": "owner", "reason": "owner"})
        );

        let ask = open(&mut queue);
        Dialogue::new(&mut DialogueQueue { queue: &mut queue }, &user, &policy)
            .answer(ask.id, "next")
            .unwrap();
        let tx = queue.write_entry("unowned_before_runtime_close").unwrap();
        store_writes::set_task_owner(&tx, task, None).unwrap();
        tx.commit().unwrap();
        crate::application::AskStore::close_ask(&mut queue, ask.id).unwrap();
        assert_eq!(
            events(&queue, "ask_closed")[1],
            json!({"ask_id": ask.id, "kind": ask.kind})
        );

        assert_eq!(
            events(&queue, "ask_answered")[1]["human_action"],
            json!({"issuer": "https://id.example", "subject": "owner", "reason": "owner"})
        );

        // The same authenticated inbox on an issuer-less deployment retains
        // its authority and the exact legacy event shape (no human fields).
        let ask = open(&mut queue);
        let before = serde_json::to_value(queue.read_ask(ask.id).unwrap()).unwrap();
        assert!(
            Dialogue::new(&mut DialogueQueue { queue: &mut queue }, &user, &policy)
                .answer(ask.id, "not addressed")
                .is_err()
        );
        assert_eq!(
            serde_json::to_value(queue.read_ask(ask.id).unwrap()).unwrap(),
            before
        );
        assert_eq!(events(&queue, "ask_answered").len(), 2);
        let local = AnswerPolicy::new(Some(false), 100);
        Dialogue::new(&mut DialogueQueue { queue: &mut queue }, &inbox, &local)
            .answer(ask.id, "local")
            .unwrap();
        Dialogue::new(&mut DialogueQueue { queue: &mut queue }, &inbox, &local)
            .close(ask.id)
            .unwrap();
        assert!(
            events(&queue, "ask_answered")[2]
                .get("human_action")
                .is_none()
        );
        assert_eq!(
            events(&queue, "ask_closed")[2],
            json!({"ask_id": ask.id, "kind": ask.kind})
        );
        assert_eq!(
            queue.read_ask(ask.id).unwrap().answer_authority,
            Some(crate::domain::AnswerAuthority::Delegated)
        );
    }
}
