//! The owners' delegates (ADR-t3989-1 decision 3) through the store's
//! functions on the execution port: read back as written, one row per
//! owner and delegate, the registry read in the same transaction, and each
//! change written with its event in one transaction. Who may change them
//! and what a change does are domain decisions, tested in `delegate`.
use super::ContractStore;
use crate::domain::RunEvent;
use crate::domain::actor::{ActorContext, ActorRole};
use crate::domain::delegate::{DelegateChange, DelegateRefusal};
use crate::domain::event_kind::EventKind;
use crate::domain::goal_owner::Owner;
use crate::domain::oidc::OidcIdentity;
use crate::domain::queue_service::{Member, TeamRole};
use crate::infrastructure::delegates::{change, change_recorded, read};
use crate::infrastructure::sql_exec::{SqlRead, SqlWrite, sql_params};
use crate::infrastructure::store_rows::event_row;
use anyhow::{Context, Result, ensure};
use serde_json::json;

const ISSUER: &str = "https://contract-delegates.example";

fn person(subject: &str) -> OidcIdentity {
    OidcIdentity {
        issuer: ISSUER.into(),
        subject: subject.into(),
    }
}

/// The owner, as the service authenticates its person, and its actor.
fn owner() -> (Owner, ActorContext) {
    let actor = ActorContext::new(
        ActorRole::User,
        format!(
            "human:{}",
            serde_json::to_string(&person("owner")).expect("JSON")
        ),
    );
    (Owner::new(person("owner")), actor)
}

/// Register `bob` and `carol`, and `dora` disabled, committed by the
/// registry's own port.
fn register<S: ContractStore>(store: &S) -> Result<()> {
    for (subject, disabled_at) in [("bob", None), ("carol", None), ("dora", Some(5))] {
        store.add_member(&Member {
            identity: person(subject),
            role: TeamRole::Member,
            on_call: false,
            display_name: None,
            created_at: 1,
            disabled_at,
        })?;
    }
    Ok(())
}

/// The events of the change of delegates the cases wrote, in order.
fn delegate_events(tx: &impl SqlRead) -> Result<Vec<RunEvent>> {
    let events = tx.query_map(
        "SELECT * FROM run_events WHERE kind=?1 ORDER BY id",
        sql_params![EventKind::DelegatesChanged.as_str()],
        event_row,
    )?;
    Ok(events
        .into_iter()
        .filter(|event| event.payload["owner"]["issuer"] == ISSUER)
        .collect())
}

/// Remove the rows, events and registry rows a committed case wrote.
fn remove(tx: &impl SqlWrite) -> Result<()> {
    tx.execute(
        "DELETE FROM delegates WHERE owner_issuer = ?1",
        sql_params![ISSUER],
    )
    .context("remove the case's delegates")?;
    for event in delegate_events(tx)? {
        tx.execute(
            "DELETE FROM run_events WHERE id = ?1",
            sql_params![event.id],
        )
        .context("remove the case's events")?;
    }
    tx.execute("DELETE FROM members WHERE issuer = ?1", sql_params![ISSUER])
        .context("remove the case's people")?;
    Ok(())
}

/// (a) An add reads back, sorted, and a second add of the same person adds
/// no row and no event; a remove takes it off. Each change that changes
/// something writes one event with the owner, the delegates before and
/// after, and the actor who changed them. Committed, both are there; the
/// rows are removed at the end.
pub fn delegates_records<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    register(&store)?;
    let (owner, actor) = owner();
    store.set_actor(actor.clone());
    let tx = store.write_entry("contract_delegates_records")?;
    let add = |subject: &str| DelegateChange::Add(person(subject));
    ensure!(change(&tx, &owner, &add("carol"), 10)?.changed);
    ensure!(change(&tx, &owner, &add("bob"), 11)?.changed);
    ensure!(!change(&tx, &owner, &add("bob"), 12)?.changed);
    let rows: i64 = tx.query_row(
        "SELECT count(*) FROM delegates WHERE owner_issuer = ?1 AND delegate_subject = 'bob'",
        sql_params![ISSUER],
        |row| row.get(0),
    )?;
    ensure!(rows == 1, "a second add wrote another row: {rows}");
    ensure!(read(&tx, &owner)? == [person("bob"), person("carol")]);
    ensure!(change(&tx, &owner, &DelegateChange::Remove(person("carol")), 13)?.changed);
    ensure!(read(&tx, &owner)? == [person("bob")]);
    // Another owner's delegates are its own.
    ensure!(read(&tx, &Owner::new(person("bob")))?.is_empty());
    S::commit(tx)?;
    let tx = store.write_entry("contract_delegates_records_read")?;
    ensure!(read(&tx, &owner)? == [person("bob")]);
    let events = delegate_events(&tx)?;
    let payloads: Vec<_> = events.iter().map(|event| event.payload.clone()).collect();
    let o = json!(person("owner"));
    ensure!(
        payloads
            == [
                json!({"owner": o, "change": "add", "delegate": person("carol"),
                    "before": [], "after": [person("carol")]}),
                json!({"owner": o, "change": "add", "delegate": person("bob"),
                    "before": [person("carol")], "after": [person("bob"), person("carol")]}),
                json!({"owner": o, "change": "remove", "delegate": person("carol"),
                    "before": [person("bob"), person("carol")], "after": [person("bob")]}),
            ],
        "{payloads:?}"
    );
    for event in &events {
        let recorded = event.actor.as_ref().context("an event without its actor")?;
        ensure!(recorded.role == "user" && recorded.id == actor.actor_id());
    }
    remove(&tx)?;
    S::commit(tx)
}

/// (b) The registry is read in the writing transaction, by the exact
/// pair: a change the domain refuses on what it read (a disabled person who
/// has a row, and a subject with blanks around a registered one's) writes neither a row nor an event and returns the
/// domain's refusal. Which people are refused, and why, is the domain's
/// (`delegate::check_delegate`). The rows are removed at the end.
pub fn delegates_refused<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    register(&store)?;
    let (owner, actor) = owner();
    store.set_actor(actor);
    let tx = store.write_entry("contract_delegates_refused")?;
    tx.execute(
        "INSERT INTO delegates(owner_issuer, owner_subject, delegate_issuer, delegate_subject, \
         created_at) VALUES (?1, 'owner', ?1, 'dora', 1)",
        sql_params![ISSUER],
    )?;
    let error = change(&tx, &owner, &DelegateChange::Remove(person("dora")), 10)
        .err()
        .context("a change refused on the registry was written")?;
    ensure!(
        error.downcast_ref::<DelegateRefusal>().is_some(),
        "{error:#}"
    );
    // The read compares the subject as it is: one with blanks around the
    // registered `bob` finds no row.
    let error = change(&tx, &owner, &DelegateChange::Add(person(" bob ")), 10)
        .err()
        .context("a subject with blanks was taken for a registered one")?;
    ensure!(
        error.downcast_ref::<DelegateRefusal>().is_some(),
        "{error:#}"
    );
    ensure!(read(&tx, &owner)? == [person("dora")] && delegate_events(&tx)?.is_empty());
    remove(&tx)?;
    S::commit(tx)
}

/// (c) A change whose event cannot be written fails, and a change in a
/// transaction let go of without a commit leaves the rows and the events as
/// they were. The rows it starts from are committed and removed at the end.
pub fn delegates_change_undone<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    register(&store)?;
    let (owner, actor) = owner();
    store.set_actor(actor);
    let tx = store.write_entry("contract_delegates_setup")?;
    change(&tx, &owner, &DelegateChange::Add(person("bob")), 10)?;
    S::commit(tx)?;
    let tx = store.write_entry("contract_delegates_event_fails")?;
    let failed = change_recorded(
        &tx,
        &owner,
        &DelegateChange::Add(person("carol")),
        11,
        |_, _| anyhow::bail!("the event could not be written"),
    );
    ensure!(failed.is_err(), "a change without its event succeeded");
    drop(tx);
    let tx = store.write_entry("contract_delegates_undone")?;
    change(&tx, &owner, &DelegateChange::Add(person("carol")), 12)?;
    change(&tx, &owner, &DelegateChange::Remove(person("bob")), 13)?;
    ensure!(read(&tx, &owner)? == [person("carol")]);
    ensure!(delegate_events(&tx)?.len() == 3);
    drop(tx);
    let tx = store.write_entry("contract_delegates_cleanup")?;
    ensure!(
        read(&tx, &owner)? == [person("bob")],
        "the delegates changed"
    );
    ensure!(
        delegate_events(&tx)?.len() == 1,
        "an event of an undone change stayed"
    );
    remove(&tx)?;
    S::commit(tx)
}
