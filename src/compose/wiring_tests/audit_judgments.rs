//! Judgment facts travel through production asks, dialogue and the supervisor
//! into the audit service on a memdb queue, without consulting event history.
use super::{
    agent::{Script, succeeded},
    harness::Wiring,
};
use crate::application::queue_commands::{AnswerParams, QueueCommand};
use crate::domain::{
    ActorContext, ActorRole, AskKind, StaticPolicy,
    audit::{AnsweredVia, AuditAction, AuditRecord, OutcomeStatus},
    authorization::AnswerPolicy,
};
use serde_json::json;
use std::path::Path;

#[test]
fn ask_answer_application_and_worker_refusal_reach_the_audit_service() {
    let wiring = Wiring::new(
        "audit_judgments",
        Script {
            receipts: vec![succeeded, succeeded, succeeded],
            verdicts: vec![
                json!({"verdict":"concern", "reasons":[], "summary":"person decides", "recommendation":"land", "confidence":"low"}),
            ],
        },
    );
    let tasks = [
        wiring.ready_task(&["true"]),
        wiring.ready_task(&["true"]),
        wiring.ready_task(&["true"]),
    ];
    wiring.supervise_once();
    let queue = wiring.queue();
    let sql = super::harness::connect(&wiring.db);
    let asks: Vec<_> = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .filter(|a| a.kind == AskKind::ApproveLanding)
        .collect();
    assert_eq!(asks.len(), 3);
    let actors = [
        ActorContext::new(ActorRole::User, "person-a"),
        ActorContext::new(ActorRole::Inbox, "inbox-a"),
        ActorContext::new(ActorRole::Inbox, "inbox-b"),
    ];
    for ((ask, actor), original) in asks.iter().zip(&actors).zip([None, Some("abc"), None]) {
        let mut writer = wiring.queue().with_actor(actor.clone());
        crate::compose::wiring_run_command_with_policy(
            &mut writer,
            Path::new("/wiring/queue"),
            &QueueCommand::Answer(AnswerParams {
                id: ask.id.as_i64(),
                text: "land".into(),
                original_text: original.map(str::to_owned),
            }),
            &AnswerPolicy::new(Some(false), 100),
        )
        .unwrap();
    }
    // Refuse the application state write, after the real supervisor selected
    // the answer. Its failed transaction must leave no successful audit fact.
    sql.execute_batch(&format!("CREATE TRIGGER hold_answer_apply BEFORE INSERT ON run_events WHEN NEW.kind='integration_approved' AND NEW.task_id={} BEGIN SELECT RAISE(ABORT, 'application unavailable'); END", asks[0].task_id.unwrap().as_i64())).unwrap();
    wiring.supervise_once();
    assert!(queue.read_ask(asks[0].id).unwrap().closed_at.is_none());
    sql.execute_batch("DROP TRIGGER hold_answer_apply").unwrap();
    wiring.supervise_once();
    for ask in &asks {
        assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    }
    let worker = ActorContext::new(ActorRole::Worker, "worker-a");
    let mut refused = wiring.queue().with_actor(worker.clone());
    assert!(
        crate::compose::wiring_run_command_with_policy(
            &mut refused,
            Path::new("/wiring/queue"),
            &QueueCommand::Answer(AnswerParams {
                id: asks[0].id.as_i64(),
                text: "land".into(),
                original_text: None
            }),
            &AnswerPolicy::new(Some(false), 100)
        )
        .is_err()
    );
    let records: Vec<AuditRecord> = sql
        .prepare("SELECT record FROM audit_records ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|s| serde_json::from_str(&s.unwrap()).unwrap())
        .collect();
    for ((ask, actor), via) in asks.iter().zip(&actors).zip([
        AnsweredVia::HumanAttested,
        AnsweredVia::RelayedByInbox,
        AnsweredVia::RelayedByInbox,
    ]) {
        let of_ask: Vec<_> = records
            .iter()
            .filter(|r| r.resource.kind == "ask" && r.resource.id == ask.id.to_string())
            .collect();
        let opened = of_ask
            .iter()
            .find(|r| {
                r.change
                    .as_ref()
                    .and_then(|c| c.after.as_ref())
                    .is_some_and(|v| v["phase"] == "opened")
            })
            .unwrap();
        let facts = opened.change.as_ref().unwrap().after.as_ref().unwrap();
        assert_eq!(
            facts["question_hash"],
            crate::application::ask::digest(&ask.question)
        );
        assert_eq!(facts["options"], json!(["land", "send_back", "cancel"]));
        assert_eq!(facts["recommendation"], "land");
        assert_eq!(facts["confidence"], "low");
        let answered = of_ask.iter().find(|r| r.answered_via == Some(via)).unwrap();
        assert_eq!(answered.principal.id, actor.actor_id());
        let hash = &answered.change.as_ref().unwrap().after.as_ref().unwrap()["original_text_hash"];
        if actor.actor_id() == "inbox-a" {
            assert_eq!(
                hash,
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            );
        } else {
            assert!(hash.is_null());
        }
        assert!(of_ask.iter().any(|r| r.action == AuditAction::AnswerApplied
            && r.outcome.status == OutcomeStatus::Succeeded
            && r.answered_via == Some(via)));
    }
    assert!(
        records
            .iter()
            .any(|r| r.action == AuditAction::AnswerApplied
                && r.resource.id == asks[0].id.to_string()
                && r.outcome.status == OutcomeStatus::Failed)
    );
    let denials: Vec<_> = records
        .iter()
        .filter(|r| r.action == AuditAction::AuthzDenied)
        .collect();
    assert_eq!(denials.len(), 1);
    assert_eq!(denials[0].principal.role, "worker");
    assert_eq!(denials[0].principal.id, "worker-a");
    assert_eq!(denials[0].resource.id, "answer");
    assert_eq!(denials[0].authority.rule.as_deref(), Some("not granted"));
    assert_eq!(
        denials[0].authority.policy_version.as_ref().unwrap().len(),
        64
    );
    assert!(tasks.iter().all(|task| {
        records
            .iter()
            .any(|r| r.correlation.task_id == Some(task.as_i64()))
    }));
    // Reading through the ordinary authorization gate creates no privileged grant.
    let before = records.len();
    crate::application::commands::Gate {
        actor: &ActorContext::user(),
        authorizer: &StaticPolicy,
    }
    .authorize(
        &queue,
        crate::domain::Capability::QueueRead,
        &crate::domain::Resource::Queue,
    )
    .unwrap();
    assert_eq!(
        sql.query_row("SELECT count(*) FROM audit_records", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        i64::try_from(before).unwrap()
    );
}
