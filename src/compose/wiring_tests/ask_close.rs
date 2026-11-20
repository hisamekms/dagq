//! The shared CLI/service command dispatch must carry deployment policy to
//! close, including the current recipients and the human event annotation.
use super::{agent::Script, harness::Wiring};
use crate::{
    application::{
        TaskReads,
        queue_commands::{AnswerParams, AskCloseParams, QueueCommand},
    },
    domain::{
        ActorContext, ActorRole, AskKind, AskReason, AuthorizationError, NewAsk,
        actor::HumanRole,
        authorization::{AnswerPolicy, DenyReason},
        delegate::DelegateChange,
        goal_owner::Owner,
        oidc::OidcIdentity,
        queue_service::{Member, TeamRole},
    },
    infrastructure::{
        delegates, members,
        sql_exec::{SqlWrite, TransactionEntries, sql_params},
    },
};
use serde_json::{Value, json};

#[test]
fn command_close_rechecks_removed_proxy_and_records_the_current_human() {
    let wiring = Wiring::new(
        "command_close_rechecks_removed_proxy_and_records_the_current_human",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    );
    let task = wiring.ready_task(&["true"]);
    let identity = |subject: &str| OidcIdentity {
        issuer: "https://id.example".into(),
        subject: subject.into(),
    };
    let role = HumanRole {
        role: TeamRole::Member,
        on_call: false,
    };
    let user = |subject: &str| {
        ActorContext::new(
            ActorRole::User,
            format!(
                "human:{}",
                serde_json::to_string(&identity(subject)).unwrap()
            ),
        )
        .with_human_role(role)
    };
    let owner = Owner::new(identity("owner"));
    let mut queue = wiring.queue();
    let tx = queue.write_entry("command_close_fixture").unwrap();
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
    SqlWrite::execute(
        &tx,
        "UPDATE tasks SET owner_issuer=?2, owner_subject=?3 WHERE id=?1",
        sql_params![task, "https://id.example", "owner"],
    )
    .unwrap();
    delegates::change(&tx, &owner, &DelegateChange::Add(identity("proxy")), 1).unwrap();
    tx.commit().unwrap();
    let ask = queue
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
        .ask;
    queue = queue.with_actor(user("proxy"));
    let policy = AnswerPolicy::new(Some(true), 100);
    crate::compose::wiring_run_command_with_policy(
        &mut queue,
        &wiring.db,
        &QueueCommand::Answer(AnswerParams {
            id: ask.id.as_i64(),
            text: "answered while delegated".into(),
            original_text: None,
        }),
        &policy,
    )
    .unwrap();
    let tx = queue
        .write_entry("remove_proxy_before_command_close")
        .unwrap();
    delegates::change(&tx, &owner, &DelegateChange::Remove(identity("proxy")), 2).unwrap();
    tx.commit().unwrap();
    let before = serde_json::to_value(queue.read_ask(ask.id).unwrap()).unwrap();
    let close = QueueCommand::AskClose(AskCloseParams {
        id: ask.id.as_i64(),
    });
    for actor in [
        user("proxy"),
        ActorContext::new(ActorRole::Inbox, "inbox").with_parent_role(identity("proxy"), role),
    ] {
        queue = queue.with_actor(actor);
        let error =
            crate::compose::wiring_run_command_with_policy(&mut queue, &wiring.db, &close, &policy)
                .unwrap_err();
        assert_eq!(
            error.downcast_ref::<AuthorizationError>().unwrap().reason,
            DenyReason::Resource
        );
        assert_eq!(
            serde_json::to_value(queue.read_ask(ask.id).unwrap()).unwrap(),
            before
        );
        assert!(
            queue
                .show(task)
                .unwrap()
                .events
                .iter()
                .all(|e| e.kind.as_str() != "ask_closed")
        );
    }
    queue = queue.with_actor(user("owner"));
    crate::compose::wiring_run_command_with_policy(&mut queue, &wiring.db, &close, &policy)
        .unwrap();
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let closed: Vec<Value> = queue
        .show(task)
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.kind.as_str() == "ask_closed")
        .map(|e| e.payload)
        .collect();
    assert_eq!(
        closed,
        [json!({
            "ask_id": ask.id, "kind": "worker_question",
            "human_action": {"issuer": "https://id.example", "subject": "owner", "reason": "owner"}
        })]
    );
}
