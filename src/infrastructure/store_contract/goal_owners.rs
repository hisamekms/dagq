//! The owner records of goals and of tasks without a goal, and the move of
//! a task's owner with its membership, through the store's functions on the
//! execution port (ADR-t3989-1 decisions 1 and 2). Which owner a task reads
//! and what a move writes are domain decisions, tested in `goal_owner`.
use super::ContractStore;
use crate::domain::actor::ActorContext;
use crate::domain::goal_owner::{Owner, TaskOwner};
use crate::domain::oidc::OidcIdentity;
use crate::domain::task::MovedBy;
use crate::domain::{GoalId, TaskId};
use crate::infrastructure::sql_exec::{SqlWrite, sql_params};
use crate::infrastructure::store_rows::{goal_owner, read_goal, read_task, task_own_owner};
use crate::infrastructure::store_writes::{set_goal_owner, set_task_owner};
use crate::infrastructure::task_store::set_goal_in;
use anyhow::{Context, Result, ensure};

fn person(subject: &str) -> Owner {
    Owner::new(OidcIdentity {
        issuer: "https://contract.example".into(),
        subject: subject.into(),
    })
}

fn goal(tx: &impl SqlWrite, title: &str) -> Result<GoalId> {
    Ok(GoalId::new(tx.insert_id(
        "INSERT INTO goals(title) VALUES (?1)",
        sql_params![title],
    )?))
}

fn task(tx: &impl SqlWrite, goal: Option<GoalId>) -> Result<TaskId> {
    Ok(TaskId::new(tx.insert_id(
        "INSERT INTO tasks(title, description, acceptance, verification_commands, status, goal_id) VALUES ('contract owner', '', '', '[]', 'draft', ?1)",
        sql_params![goal],
    )?))
}

fn owner_of(tx: &impl SqlWrite, task: TaskId) -> Result<Option<Owner>> {
    Ok(read_task(tx, task)?.owner().person().cloned())
}

/// (a) A goal's owner is written and read back, by the owner read and with
/// the goal; a goal without one reads none. (b) A task without a goal reads
/// back its own owner. All rows roll back.
pub fn goal_owner_records<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    let tx = store.write_entry("contract_goal_owner_records")?;
    let (owned, unowned) = (goal(&tx, "owned")?, goal(&tx, "unowned")?);
    let alice = person("alice");
    set_goal_owner(&tx, owned, Some(&alice))?;
    ensure!(goal_owner(&tx, owned)? == Some(alice.clone()));
    ensure!(read_goal(&tx, owned)?.owner() == Some(&alice));
    ensure!(goal_owner(&tx, unowned)?.is_none());
    ensure!(read_goal(&tx, unowned)?.owner().is_none());
    let alone = task(&tx, None)?;
    ensure!(read_task(&tx, alone)?.owner() == TaskOwner::HostUser);
    let bob = person("bob");
    set_task_owner(&tx, alone, Some(&bob))?;
    ensure!(task_own_owner(&tx, alone)? == Some(bob.clone()));
    ensure!(owner_of(&tx, alone)? == Some(bob));
    // A task in a goal records no owner of its own.
    let member = task(&tx, Some(owned))?;
    ensure!(owner_of(&tx, member)? == Some(alice.clone()));
    ensure!(
        set_task_owner(&tx, member, Some(&alice)).is_err(),
        "a task in a goal took an owner"
    );
    ensure!(task_own_owner(&tx, member)?.is_none());
    Ok(())
}

/// (c) A task without a goal that has an owner, moved into a goal, keeps no
/// owner of its own and reads the goal's; moved on, it reads the next
/// goal's. (d) Moved out, it keeps the owner of the goal it left. All rows
/// roll back.
pub fn goal_owner_moves<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    store.set_actor(ActorContext::user());
    let tx = store.write_entry("contract_goal_owner_moves")?;
    let (first, second) = (goal(&tx, "first")?, goal(&tx, "second")?);
    let (alice, bob, carol) = (person("alice"), person("bob"), person("carol"));
    set_goal_owner(&tx, first, Some(&alice))?;
    set_goal_owner(&tx, second, Some(&bob))?;
    let moved = task(&tx, None)?;
    set_task_owner(&tx, moved, Some(&carol))?;
    set_goal_in(&tx, moved, Some(first), "t1", MovedBy::Person)?;
    ensure!(task_own_owner(&tx, moved)?.is_none());
    ensure!(owner_of(&tx, moved)? == Some(alice));
    set_goal_in(&tx, moved, Some(second), "t2", MovedBy::Person)?;
    ensure!(task_own_owner(&tx, moved)?.is_none());
    ensure!(owner_of(&tx, moved)? == Some(bob.clone()));
    set_goal_in(&tx, moved, None, "t3", MovedBy::Person)?;
    ensure!(read_task(&tx, moved)?.goal_id().is_none());
    ensure!(task_own_owner(&tx, moved)? == Some(bob.clone()));
    ensure!(owner_of(&tx, moved)? == Some(bob));
    Ok(())
}

/// (e) The move out of (d), in a transaction let go of without a commit,
/// leaves the membership and both owner records as they were. The rows
/// it starts from are committed and removed at the end.
pub fn goal_owner_move_undone<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    store.set_actor(ActorContext::user());
    let alice = person("alice");
    let tx = store.write_entry("contract_goal_owner_setup")?;
    let owned = goal(&tx, "undone")?;
    set_goal_owner(&tx, owned, Some(&alice))?;
    let member = task(&tx, Some(owned))?;
    S::commit(tx)?;
    let tx = store.write_entry("contract_goal_owner_undone")?;
    set_goal_in(&tx, member, None, "t1", MovedBy::Person)?;
    ensure!(task_own_owner(&tx, member)? == Some(alice.clone()));
    drop(tx);
    let tx = store.write_entry("contract_goal_owner_cleanup")?;
    let task = read_task(&tx, member)?;
    ensure!(task.goal_id() == Some(owned), "the membership moved");
    ensure!(task_own_owner(&tx, member)?.is_none());
    ensure!(owner_of(&tx, member)? == Some(alice.clone()));
    ensure!(goal_owner(&tx, owned)? == Some(alice));
    tx.execute("DELETE FROM tasks WHERE id = ?1", sql_params![member])
        .context("remove the case's task")?;
    tx.execute("DELETE FROM goals WHERE id = ?1", sql_params![owned])
        .context("remove the case's goal")?;
    S::commit(tx)?;
    Ok(())
}
