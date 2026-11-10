//! The landing marks of a goal (ADR-t3988-1 decision 4) through the store's
//! functions on the execution port: kept apart from the goal's tags, and
//! written with the event of their change in one transaction. What a change
//! does to the marks and who may make it are domain decisions, tested in
//! `goal` and `authorization`.
use super::ContractStore;
use crate::domain::actor::{ActorContext, ActorRole};
use crate::domain::event_kind::EventKind;
use crate::domain::goal::LandingMarkChange;
use crate::domain::goal_owner::Owner;
use crate::domain::landing_approval::GoalMark;
use crate::domain::oidc::OidcIdentity;
use crate::domain::{GoalEdit, GoalId, GoalTag, RunEvent};
use crate::infrastructure::sql_exec::{SqlRead, SqlWrite, sql_params};
use crate::infrastructure::store_rows::{event_row, goal_landing_marks, read_goal};
use crate::infrastructure::store_writes::set_goal_owner;
use crate::infrastructure::task_store::{self, change_landing_mark};
use anyhow::{Context, Result, ensure};
use serde_json::json;

fn mark(name: &str) -> GoalMark {
    GoalMark::new(name.into()).expect("a mark that is not blank")
}

fn marks(names: &[&str]) -> Vec<GoalMark> {
    names.iter().map(|name| mark(name)).collect()
}

fn goal(tx: &impl SqlWrite, title: &str) -> Result<GoalId> {
    Ok(GoalId::new(tx.insert_id(
        "INSERT INTO goals(title) VALUES (?1)",
        sql_params![title],
    )?))
}

/// The goal's events of the change of its landing marks, in order.
fn mark_events(tx: &impl SqlRead, goal: GoalId) -> Result<Vec<RunEvent>> {
    Ok(tx.query_map(
        "SELECT * FROM run_events WHERE goal_id=?1 AND kind=?2 ORDER BY id",
        sql_params![goal, EventKind::GoalLandingMarksChanged.as_str()],
        event_row,
    )?)
}

/// A person of the team version, as the service authenticates one.
fn person() -> (Owner, ActorContext) {
    let identity = OidcIdentity {
        issuer: "https://contract.example".into(),
        subject: "alice".into(),
    };
    let actor = ActorContext::new(
        ActorRole::User,
        format!("human:{}", serde_json::to_string(&identity).expect("JSON")),
    );
    (Owner::new(identity), actor)
}

/// (a) Marks put on a goal read back, and one taken off no longer does;
/// a goal without marks reads none, and a change on a goal the queue does
/// not hold writes nothing. What a change does to the marks is the domain's
/// (`goal::change_landing_marks`). All rows roll back.
pub fn goal_landing_marks_records<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    store.set_actor(ActorContext::user());
    let tx = store.write_entry("contract_goal_landing_marks_records")?;
    let (marked, unmarked) = (goal(&tx, "marked")?, goal(&tx, "unmarked")?);
    let put = |name: &str| LandingMarkChange::Put(mark(name));
    ensure!(change_landing_mark(&tx, marked, &put("zeta"), None)?.changed);
    ensure!(change_landing_mark(&tx, marked, &put("alpha"), None)?.changed);
    ensure!(goal_landing_marks(&tx, marked)? == marks(&["alpha", "zeta"]));
    let off = LandingMarkChange::TakeOff(mark("zeta"));
    ensure!(change_landing_mark(&tx, marked, &off, None)?.changed);
    ensure!(goal_landing_marks(&tx, marked)? == marks(&["alpha"]));
    ensure!(goal_landing_marks(&tx, unmarked)?.is_empty());
    ensure!(mark_events(&tx, unmarked)?.is_empty());
    // A change on a goal the queue does not hold writes nothing.
    let missing = GoalId::new(unmarked.as_i64() + 1000);
    ensure!(change_landing_mark(&tx, missing, &put("alpha"), None).is_err());
    ensure!(goal_landing_marks(&tx, missing)?.is_empty());
    Ok(())
}

/// (b) The write of `goal edit` that replaces the tags (`--tag`, and
/// `--no-tags`'s empty list), even with a tag of a mark's name, leaves the
/// marks as they were. All rows roll back.
pub fn goal_landing_marks_survive_tags<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let store = open()?;
    store.set_actor(ActorContext::user());
    let tx = store.write_entry("contract_goal_landing_marks_tags")?;
    let id = goal(&tx, "tagged")?;
    let put = LandingMarkChange::Put(mark("human-landing"));
    change_landing_mark(&tx, id, &put, None)?;
    // A fixed time: the case compares no timestamps, and reads no clock.
    struct At(std::time::SystemTime);
    impl crate::application::Clock for At {
        fn system_time(&self) -> std::time::SystemTime {
            self.0
        }
        fn monotonic(&self) -> std::time::Instant {
            unreachable!("the goal's edit reads no monotonic time")
        }
    }
    let generators = crate::application::Generators {
        clock: std::sync::Arc::new(At(
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_791_072_000)
        )),
        ids: std::sync::Arc::new(crate::infrastructure::clock::UuidGenerator),
    };
    let mut writes = task_store::Store {
        tx: &tx,
        runs_dir: std::path::Path::new("runs"),
        generators: &generators,
        changes: &None,
        title_prefixes: &None,
        goal_tags: &None,
    };
    let tag: GoalTag = "human-landing".parse()?;
    for tags in [vec![tag], Vec::new()] {
        let edit = GoalEdit {
            tags: Some(tags.clone()),
            ..GoalEdit::default()
        };
        writes.edit_goal(id, edit)?;
        ensure!(read_goal(&tx, id)?.tags() == tags.as_slice());
        ensure!(goal_landing_marks(&tx, id)? == marks(&["human-landing"]));
    }
    Ok(())
}

/// (c) A change writes its row and its event, with the actor and the marks
/// before and after, in the one transaction; committed, both are there.
/// A change refused because the goal's owner is not the one authorized
/// writes neither. The rows are removed at the end.
pub fn goal_landing_marks_change_with_event<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let store = open()?;
    let (owner, actor) = person();
    store.set_actor(actor.clone());
    let tx = store.write_entry("contract_goal_landing_marks_event")?;
    let id = goal(&tx, "audited")?;
    set_goal_owner(&tx, id, Some(&owner))?;
    let put = LandingMarkChange::Put(mark("human-landing"));
    ensure!(
        change_landing_mark(&tx, id, &put, None).is_err(),
        "a change authorized with another owner was written"
    );
    ensure!(goal_landing_marks(&tx, id)?.is_empty() && mark_events(&tx, id)?.is_empty());
    change_landing_mark(&tx, id, &put, Some(&owner))?;
    let off = LandingMarkChange::TakeOff(mark("human-landing"));
    change_landing_mark(&tx, id, &off, Some(&owner))?;
    change_landing_mark(&tx, id, &put, Some(&owner))?;
    S::commit(tx)?;
    let tx = store.write_entry("contract_goal_landing_marks_event_read")?;
    ensure!(goal_landing_marks(&tx, id)? == marks(&["human-landing"]));
    let events = mark_events(&tx, id)?;
    let payloads: Vec<_> = events.iter().map(|event| event.payload.clone()).collect();
    ensure!(
        payloads
            == [
                json!({"change": "put", "mark": "human-landing", "before": [], "after": ["human-landing"]}),
                json!({"change": "take_off", "mark": "human-landing", "before": ["human-landing"], "after": []}),
                json!({"change": "put", "mark": "human-landing", "before": [], "after": ["human-landing"]}),
            ],
        "{payloads:?}"
    );
    for event in &events {
        let recorded = event.actor.as_ref().context("an event without its actor")?;
        ensure!(recorded.role == "user" && recorded.id == actor.actor_id());
    }
    remove(&tx, id)?;
    S::commit(tx)
}

/// (d) The change of (c) in a transaction let go of without a commit
/// leaves the marks and the events as they were. The rows it starts from
/// are committed and removed at the end.
pub fn goal_landing_marks_change_undone<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let store = open()?;
    store.set_actor(ActorContext::user());
    let tx = store.write_entry("contract_goal_landing_marks_setup")?;
    let id = goal(&tx, "undone")?;
    change_landing_mark(&tx, id, &LandingMarkChange::Put(mark("kept")), None)?;
    S::commit(tx)?;
    let tx = store.write_entry("contract_goal_landing_marks_undone")?;
    change_landing_mark(&tx, id, &LandingMarkChange::Put(mark("added")), None)?;
    change_landing_mark(&tx, id, &LandingMarkChange::TakeOff(mark("kept")), None)?;
    ensure!(goal_landing_marks(&tx, id)? == marks(&["added"]));
    ensure!(mark_events(&tx, id)?.len() == 3);
    drop(tx);
    let tx = store.write_entry("contract_goal_landing_marks_cleanup")?;
    ensure!(
        goal_landing_marks(&tx, id)? == marks(&["kept"]),
        "the marks changed"
    );
    ensure!(
        mark_events(&tx, id)?.len() == 1,
        "an event of the undone change stayed"
    );
    remove(&tx, id)?;
    S::commit(tx)
}

/// Remove a committed case's goal, its marks and its events.
fn remove(tx: &impl SqlWrite, id: GoalId) -> Result<()> {
    tx.execute(
        "DELETE FROM goal_landing_marks WHERE goal_id = ?1",
        sql_params![id],
    )
    .context("remove the case's marks")?;
    tx.execute("DELETE FROM run_events WHERE goal_id = ?1", sql_params![id])
        .context("remove the case's events")?;
    tx.execute("DELETE FROM goals WHERE id = ?1", sql_params![id])
        .context("remove the case's goal")?;
    Ok(())
}
