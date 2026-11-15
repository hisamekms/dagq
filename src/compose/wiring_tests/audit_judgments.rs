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
        ActorContext::new(ActorRole::User, "person-b").with_direct_human(Some(
            crate::domain::direct_human::test_support::evidence(
                &json!({"id": asks[2].id, "text": "land"}),
            ),
        )),
    ];
    // The protected-path producer owns this classification; the answer
    // entry must use the stored ask, not the client's answer text.
    sql.execute(
        "UPDATE asks SET subject='protected_paths' WHERE id=?1",
        [asks[2].id.as_i64()],
    )
    .unwrap();
    let mut relayed = wiring
        .queue()
        .with_actor(ActorContext::new(ActorRole::Inbox, "inbox-b"));
    assert!(
        crate::compose::wiring_run_command_with_policy(
            &mut relayed,
            Path::new("/wiring/queue"),
            &QueueCommand::Answer(AnswerParams {
                id: asks[2].id.as_i64(),
                text: "land".into(),
                original_text: None
            }),
            &AnswerPolicy::new(Some(false), 100),
        )
        .is_err()
    );
    assert!(queue.read_ask(asks[2].id).unwrap().answer.is_none());
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
        AnsweredVia::HumanAttested,
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
        if actor.actor_id() == "person-b" {
            let evidence: serde_json::Value =
                serde_json::from_str(&answered.attestation.evidence["direct_human"]).unwrap();
            assert_eq!(evidence["terminal"], "/dev/ttys009");
        }
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
    assert_eq!(denials.len(), 2);
    assert!(denials.iter().any(|r| {
        r.principal.role == "inbox"
            && r.authority
                .rule
                .as_deref()
                .unwrap()
                .contains("direct human")
    }));
    let denials: Vec<_> = denials
        .into_iter()
        .filter(|r| r.principal.role == "worker")
        .collect();
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

#[test]
fn direct_host_decisions_and_relay_refusals_reach_production_audit_writers() {
    use crate::application::queue_service::{Peer, QueueService};
    use crate::domain::direct_human::test_support::evidence;
    use crate::domain::queue_service::{API_VERSION, ServiceRequest, UseCase};
    let wiring = Wiring::new(
        "direct_human",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    );
    let backend = wiring.service_backend();
    let service = QueueService {
        backend: &*backend,
        build: "test",
        pid: 1,
    };
    let sql = super::harness::connect(&wiring.db);
    let (pid, start) = wiring.processes().start();
    let inbox = crate::application::queue_service::ServiceAccess::issue(
        wiring.tokens(),
        &wiring.db,
        &crate::domain::queue_service::Principal::inbox(
            crate::domain::queue_service::SessionProcess { pid, start },
        ),
    )
    .unwrap();
    let inbox_token = wiring.tokens().read(&inbox.file).unwrap();
    let task = wiring.ready_task(&["true"]);
    sql.execute(
        "UPDATE tasks SET status='draft' WHERE id=?1",
        [task.as_i64()],
    )
    .unwrap();
    let calls = [
        (UseCase::Ready, json!({"id": task, "bypass_review": true})),
        (
            UseCase::Control,
            json!({"control": {"command": "down", "yes": false, "pid": 31},
            "call": {"op": "audit_privileged_operation", "args": ["down_force"]}}),
        ),
        (
            UseCase::Control,
            json!({"control": {"command": "inbox", "yes": false, "pid": 31},
            "call": {"op": "audit_privileged_operation", "args": ["guardrail_disable"]}}),
        ),
    ];
    for (use_case, params) in calls {
        let mut req = ServiceRequest {
            caller: None,
            api_version: API_VERSION,
            token: None,
            env_role: None,
            queue: None,
            binding: None,
            use_case,
            params: params.clone(),
        };
        let refused = service.handle_from_process(&req, Peer::Owner, Some(31));
        assert_eq!(
            refused.error.unwrap().code,
            crate::domain::queue_service::ServiceErrorCode::AuthorizationDenied
        );
        // An inbox token (and its DAGQ_ROLE) cannot upgrade itself even
        // by sending plausible host confirmation metadata.
        req.params["_direct_human"] = json!(evidence(&params));
        req.token = Some(inbox_token.clone());
        req.env_role = Some("inbox".into());
        let relayed = service.handle_from_process(&req, Peer::Owner, Some(pid));
        assert_eq!(
            relayed.error.unwrap().code,
            crate::domain::queue_service::ServiceErrorCode::AuthorizationDenied
        );
        req.token = None;
        req.env_role = None;
        let accepted = service.handle_from_process(&req, Peer::Owner, Some(31));
        assert!(accepted.ok, "{accepted:?}");
    }
    let records: Vec<AuditRecord> = sql
        .prepare("SELECT record FROM audit_records ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|s| serde_json::from_str(&s.unwrap()).unwrap())
        .collect();
    assert_eq!(
        records
            .iter()
            .filter(|r| r.action == AuditAction::AuthzDenied)
            .count(),
        6
    );
    let grants: Vec<_> = records
        .iter()
        .filter(|r| r.action == AuditAction::AuthzPrivileged)
        .collect();
    assert_eq!(grants.len(), 3);
    for grant in grants {
        assert_eq!(grant.answered_via, Some(AnsweredVia::HumanAttested));
        let evidence: serde_json::Value =
            serde_json::from_str(&grant.attestation.evidence["direct_human"]).unwrap();
        assert_eq!(evidence["terminal"], "/dev/ttys009");
        assert_eq!(evidence["parent_chain"], json!([31, 17, 1]));
        assert_eq!(evidence["elapsed_ms"], 400);
        assert!(evidence["socket_peer"].as_str().unwrap().contains("31"));
    }
}
