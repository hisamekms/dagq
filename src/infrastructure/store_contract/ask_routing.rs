//! Current recipient reads and human writes on the shared SQL port.
use super::ContractStore;
use crate::domain::{
    ActorContext, ActorRole, Answerer, Ask, AskKind, AskReason, Capability, GoalId, NewAsk, TaskId,
};
use crate::domain::{
    actor::HumanRole,
    ask_routing::{Recipient, RecipientReason},
    goal_owner::Owner,
    oidc::OidcIdentity,
    queue_service::{Member, TeamRole},
};
use crate::infrastructure::sql_exec::{SqlRead, SqlWrite, sql_params};
use crate::infrastructure::{asks, delegates, members, store_writes};
use anyhow::{Result, ensure};
use serde_json::{Value, json};

const ISSUER: &str = "https://ask-contract.example";
fn identity(subject: &str) -> OidcIdentity {
    OidcIdentity {
        issuer: ISSUER.into(),
        subject: subject.into(),
    }
}
fn expected(subject: &str, reason: RecipientReason) -> Recipient {
    Recipient {
        identity: identity(subject),
        reason,
    }
}
fn owner_and_proxy() -> Vec<Recipient> {
    vec![
        expected("owner", RecipientReason::Owner),
        expected("proxy", RecipientReason::Delegate),
    ]
}
fn actor(subject: &str, inbox: bool) -> ActorContext {
    let role = HumanRole {
        role: TeamRole::Member,
        on_call: false,
    };
    if inbox {
        ActorContext::new(ActorRole::Inbox, "inbox").with_parent_role(identity(subject), role)
    } else {
        ActorContext::new(
            ActorRole::User,
            format!(
                "human:{}",
                serde_json::to_string(&identity(subject)).unwrap()
            ),
        )
        .with_human_role(role)
    }
}
fn open(
    tx: &impl SqlWrite,
    task: Option<TaskId>,
    run: Option<crate::domain::RunId>,
    kind: AskKind,
) -> Result<Ask> {
    Ok(asks::insert_ask(
        tx,
        &NewAsk {
            kind,
            task_id: task,
            run_id: run,
            question: "contract routing".into(),
            options: vec![],
            asked_by: "supervisor".into(),
            reason_category: AskReason::Scope,
            finding_id: None,
            request_id: None,
            recommendation: None,
            confidence: None,
            topics: vec![],
        },
    )?
    .ask)
}
fn payloads(tx: &impl SqlRead, task: TaskId, kind: &str) -> Result<Vec<Value>> {
    Ok(tx.query_map(
        "SELECT payload FROM run_events WHERE task_id=?1 AND kind=?2 ORDER BY id",
        sql_params![task, kind],
        |r| {
            let text: String = r.get(0)?;
            Ok(serde_json::from_str::<Value>(&text).expect("event JSON"))
        },
    )?)
}
fn answer(tx: &impl SqlWrite, ask: &Ask, actor: &ActorContext) -> Result<()> {
    let human = asks::authorize_human(tx, ask, Some((actor, Some(true))), Capability::AskAnswer)?;
    let mut payload = json!({"ask_id": ask.id, "kind": ask.kind});
    asks::write_answer(
        tx,
        ask,
        "free answer",
        if actor.role() == ActorRole::Inbox {
            Answerer::INBOX
        } else {
            Answerer::PERSON
        },
        100,
        &mut payload,
    )?;
    asks::record_human(&mut payload, human.as_ref());
    asks::ask_event(
        tx,
        ask.task_id,
        ask.run_id.as_ref(),
        crate::domain::EventKind::AskAnswered,
        payload,
    )
}

/// The same target reads on both adapters, with live goal membership,
/// delegates and registry state; a task-less goal review uses its goal link.
/// Writes and events roll back together, for answer and close separately.
pub fn ask_routing<S: ContractStore, F: Fn() -> Result<S>>(factory: &F) -> Result<()> {
    let store = factory()?;
    store.set_actor(ActorContext::user());
    let tx = store.write_entry("contract_ask_routing_setup")?;
    let event_floor: i64 = tx.query_row(
        "SELECT COALESCE(MAX(id),0) FROM run_events",
        sql_params![],
        |r| r.get(0),
    )?;
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
        )?;
    }
    let goal = GoalId::new(tx.insert_id(
        "INSERT INTO goals(title) VALUES ('ask routing')",
        sql_params![],
    )?);
    let task = TaskId::new(tx.insert_id("INSERT INTO tasks(title, description, acceptance, verification_commands, status, goal_id) VALUES ('ask routing', '', '', '[]', 'draft', ?1)", sql_params![goal])?);
    let owner = Owner::new(identity("owner"));
    store_writes::set_goal_owner(&tx, goal, Some(&owner))?;
    delegates::change(
        &tx,
        &owner,
        &crate::domain::delegate::DelegateChange::Add(identity("proxy")),
        1,
    )?;
    let ask = open(&tx, Some(task), None, AskKind::Decide)?;
    ensure!(asks::read_recipients(&tx, &ask)? == owner_and_proxy());
    let run = crate::domain::RunId::new("ask-routing-contract")?;
    tx.execute("INSERT INTO task_runs(id, task_id, status, requested_provider, actual_provider, base_commit) VALUES (?1, ?2, 'succeeded', 'codex', 'codex', 'base')", sql_params![run, task])?;
    let run_ask = open(&tx, None, Some(run.clone()), AskKind::ApproveLanding)?;
    ensure!(asks::read_recipients(&tx, &run_ask)? == owner_and_proxy());
    let goal_ask = open(&tx, None, None, AskKind::Blocked)?;
    tx.execute("INSERT INTO goal_reviews(goal_id, attempt, supervisor_token, fingerprint, started_at, ask_id) VALUES (?1, 1, 'contract', 'f', 1, ?2)", sql_params![goal, goal_ask.id])?;
    ensure!(asks::read_recipients(&tx, &goal_ask)? == owner_and_proxy());
    store_writes::set_goal_owner(&tx, goal, Some(&Owner::new(identity("proxy"))))?;
    let changed = asks::read_recipients(&tx, &ask)?;
    ensure!(changed == vec![expected("proxy", RecipientReason::Owner)]);
    ensure!(asks::read_recipients(&tx, &goal_ask)? == changed);
    store_writes::set_goal_owner(&tx, goal, Some(&owner))?;
    let mut unrelated = goal_ask.clone();
    unrelated.finding_id = Some(crate::domain::FindingId::new(99));
    ensure!(asks::read_recipients(&tx, &unrelated)?.is_empty());
    unrelated.finding_id = None;
    unrelated.request_id = Some(crate::domain::RequestId::new(99));
    ensure!(asks::read_recipients(&tx, &unrelated)?.is_empty());
    // Out of the goal, task ownership is now read from the task itself.
    tx.execute(
        "UPDATE tasks SET goal_id=NULL WHERE id=?1",
        sql_params![task],
    )?;
    ensure!(asks::read_recipients(&tx, &ask)?.is_empty());
    store_writes::set_task_owner(&tx, task, Some(&owner))?;
    ensure!(asks::read_recipients(&tx, &run_ask)? == owner_and_proxy());
    let worker_task = open(&tx, Some(task), None, AskKind::WorkerQuestion)?;
    let worker_run = open(&tx, None, Some(run.clone()), AskKind::WorkerQuestion)?;
    ensure!(asks::read_recipients(&tx, &worker_task)? == owner_and_proxy());
    ensure!(asks::read_recipients(&tx, &worker_run)? == owner_and_proxy());
    members::disable(&tx, &identity("proxy"), 2)?;
    ensure!(asks::read_recipients(&tx, &ask)? == vec![expected("owner", RecipientReason::Owner)]);
    tx.execute(
        "UPDATE members SET disabled_at=NULL WHERE issuer=?1",
        sql_params![ISSUER],
    )?;
    S::commit(tx)?;

    let proxy = actor("proxy", true);
    let tx = store.write_entry("contract_ask_answer_undone")?;
    answer(&tx, &ask, &proxy)?;
    ensure!(
        payloads(&tx, task, "ask_answered")?[0]["human_action"]
            == json!({"issuer": ISSUER, "subject": "proxy", "reason": "delegate"})
    );
    drop(tx);
    let tx = store.write_entry("contract_ask_answer")?;
    ensure!(asks::read_ask(&tx, ask.id)?.answer.is_none());
    ensure!(payloads(&tx, task, "ask_answered")?.is_empty());
    answer(&tx, &ask, &proxy)?;
    S::commit(tx)?;

    let tx = store.write_entry("contract_ask_close_undone")?;
    let answered = asks::read_ask(&tx, ask.id)?;
    asks::close_in(&tx, &answered, 101, Some((&proxy, Some(true))))?;
    ensure!(
        payloads(&tx, task, "ask_closed")?[0]["human_action"]
            == json!({"issuer": ISSUER, "subject": "proxy", "reason": "delegate"})
    );
    drop(tx);
    let tx = store.write_entry("contract_ask_close")?;
    ensure!(asks::read_ask(&tx, ask.id)?.closed_at.is_none());
    ensure!(payloads(&tx, task, "ask_closed")?.is_empty());
    delegates::change(
        &tx,
        &owner,
        &crate::domain::delegate::DelegateChange::Remove(identity("proxy")),
        102,
    )?;
    ensure!(asks::close_in(&tx, &answered, 103, Some((&proxy, Some(true)))).is_err());
    ensure!(asks::read_ask(&tx, ask.id)?.closed_at.is_none());
    ensure!(payloads(&tx, task, "ask_closed")?.is_empty());
    asks::close_in(
        &tx,
        &answered,
        104,
        Some((&actor("owner", false), Some(true))),
    )?;
    ensure!(
        payloads(&tx, task, "ask_closed")?[0]["human_action"]
            == json!({"issuer": ISSUER, "subject": "owner", "reason": "owner"})
    );
    // Runtime close bypasses recipients, and records exactly the old shape.
    asks::write_answer(
        &tx,
        &run_ask,
        "cancel",
        Answerer::RUNTIME,
        105,
        &mut json!({}),
    )?;
    let runtime_ask = asks::read_ask(&tx, run_ask.id)?;
    asks::close_in(&tx, &runtime_ask, 106, None)?;
    ensure!(payloads(&tx, task, "ask_closed")?.len() == 2);
    let all: Vec<String> = tx.query_map(
        "SELECT payload FROM run_events WHERE run_id=?1 AND kind='ask_closed'",
        sql_params![run],
        |r| r.get(0),
    )?;
    ensure!(
        serde_json::from_str::<Value>(&all[0])?
            == json!({"ask_id": run_ask.id, "kind": run_ask.kind})
    );
    S::commit(tx)?;

    let tx = store.write_entry("contract_ask_routing_cleanup")?;
    tx.execute(
        "DELETE FROM run_events WHERE id>?1",
        sql_params![event_floor],
    )?;
    for id in [
        ask.id,
        run_ask.id,
        goal_ask.id,
        worker_task.id,
        worker_run.id,
    ] {
        tx.execute("DELETE FROM asks WHERE id=?1", sql_params![id])?;
    }
    tx.execute(
        "DELETE FROM goal_reviews WHERE goal_id=?1",
        sql_params![goal],
    )?;
    tx.execute("DELETE FROM task_runs WHERE id=?1", sql_params![run])?;
    tx.execute("DELETE FROM tasks WHERE id=?1", sql_params![task])?;
    tx.execute("DELETE FROM goals WHERE id=?1", sql_params![goal])?;
    tx.execute(
        "DELETE FROM delegates WHERE owner_issuer=?1",
        sql_params![ISSUER],
    )?;
    tx.execute("DELETE FROM members WHERE issuer=?1", sql_params![ISSUER])?;
    S::commit(tx)?;
    Ok(())
}
