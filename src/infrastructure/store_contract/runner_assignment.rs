//! Exercise the shared assignment entry with real adapter transactions, not
//! the lease helpers alone. Each case owns a private runner and task so the
//! committed fixtures also compose with the other contracts on one database.
use super::ContractStore;
use crate::{
    application::{Clock, Generators},
    domain::{
        CommitSha,
        runner_assignment::{Assignment, ClaimInputs, LeaseReport, Operation, Outcome, Poll},
        runner_lease::LEASE,
    },
    infrastructure::{
        event_actor::{EventActors, process_actor},
        runtime_store,
        sql_exec::{CommitTransaction, SqlRead, SqlWrite, TransactionEntries, sql_params},
        store_queue::StoreQueue,
    },
};
use anyhow::{Context, Result, ensure};
use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct At(i64);
impl Clock for At {
    fn system_time(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.0 as u64)
    }
    fn monotonic(&self) -> Instant {
        unreachable!("assignment uses only the control side's wall clock")
    }
}
fn generators(now: i64) -> Generators {
    let mut generators = crate::infrastructure::clock::system();
    generators.clock = Arc::new(At(now));
    generators
}
const NOW: i64 = 1_000_000_000;

fn fixture<S: ContractStore>(store: S, name: &str) -> Result<StoreQueue<S>> {
    let tx = store.write_entry("contract_assignment_fixture")?;
    tx.execute("INSERT INTO members(issuer,subject,role,on_call,created_at) VALUES (?1,'owner','admin',0,0)", sql_params![name])?;
    for id in [name.to_owned(), format!("{name}-other")] {
        tx.execute("INSERT INTO runners(id,owner_issuer,owner_subject,created_at,declaration) VALUES (?1,?2,'owner',0,?3)",
            sql_params![id, name, r#"{"os":"linux","arch":"aarch64","container":true,"credentials":[]}"#])?;
        tx.execute("INSERT INTO runner_tokens(hash,runner_id,expires_at) VALUES (?1,?1,9223372036854775807)", sql_params![id])?;
    }
    tx.execute("INSERT INTO tasks(title,description,acceptance,verification_commands,status,owner_issuer,owner_subject) VALUES (?1,'','', '[]','ready',?1,'owner')", sql_params![name])?;
    S::commit(tx)?;
    Ok(StoreQueue {
        entries: store,
        runs_dir: std::path::PathBuf::from("contract-runs"),
        generators: generators(NOW),
        actors: EventActors::new(process_actor()),
        changes: None,
        title_prefixes: None,
        goal_tags: None,
        max_spike_cycles: None,
        replacement: None,
        denial_log: None,
        audit_use_case: None,
    })
}
fn pull<S: ContractStore>(queue: &StoreQueue<S>, token: &str, now: i64) -> Result<Assignment> {
    let outcome = queue.runner_operation(
        token,
        &Operation::Poll(Poll {}),
        Some(&ClaimInputs {
            base: CommitSha::parse("a".repeat(40), "contract base")?,
            limits: Default::default(),
        }),
        &generators(now),
    )?;
    let Outcome::Assignment(Some(assignment)) = outcome else {
        anyhow::bail!("expected a runner assignment");
    };
    Ok(*assignment)
}
fn lease_report(assignment: &Assignment) -> LeaseReport {
    LeaseReport {
        run_id: assignment.run.id().to_string(),
        generation: assignment.generation,
    }
}
fn result_report(assignment: &Assignment, generation: i64) -> Result<Operation> {
    Ok(Operation::Report(serde_json::from_value(
        serde_json::json!({
            "run_id": assignment.run.id(), "generation": generation, "head": "b".repeat(40),
            "state": {"head": "b".repeat(40), "branch": assignment.run.branch()},
            "receipt": {"result": "succeeded"}, "exit_code": 0,
        }),
    )?))
}

/// A heartbeat extends the persisted deadline without changing the run or
/// accepting a head; reopening sees the committed extension.
pub fn assignment_heartbeat<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let name = "contract-assignment-heartbeat";
    let queue = fixture(open()?, name)?;
    let assigned = pull(&queue, name, NOW)?;
    let id = assigned.run.id().as_str();
    let before = queue.entries.runner_lease(id)?.context("lease")?;
    ensure!(matches!(
        queue.runner_operation(
            name,
            &Operation::Heartbeat(lease_report(&assigned)),
            None,
            &generators(NOW + 1_000)
        )?,
        Outcome::Renewed { .. }
    ));
    let after = open()?.runner_lease(id)?.context("renewed lease")?;
    ensure!(after.expires_at_ms == NOW + 1_000 + LEASE.as_millis() as i64);
    ensure!(after.expires_at_ms > before.expires_at_ms);
    ensure!(
        after.generation == before.generation
            && after.runner_id == before.runner_id
            && after.state == before.state
    );
    ensure!(queue.entries.accepted_head(id)?.is_none());
    let tx = queue.read_entry("contract_heartbeat_run")?;
    ensure!(
        serde_json::to_value(runtime_store::stored_run(&tx, assigned.run.id())?.context("run")?)?
            == serde_json::to_value(&assigned.run)?
    );
    Ok(())
}

/// A stale result from the same runner is fenced before any run, event or
/// accepted-head write. The current generation's same result does commit.
pub fn assignment_stale_report<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let name = "contract-assignment-stale";
    let queue = fixture(open()?, name)?;
    let first = pull(&queue, name, NOW)?;
    let now = NOW + LEASE.as_millis() as i64;
    let second = pull(&queue, name, now)?;
    let id = second.run.id().as_str();
    let before = queue.entries.runner_lease(id)?;
    let events = || -> Result<i64> {
        Ok(queue.read_entry("contract_report_events")?.query_row(
            "SELECT count(*) FROM run_events WHERE run_id=?1",
            sql_params![id],
            |r| r.get(0),
        )?)
    };
    let count = events()?;
    ensure!(matches!(
        queue.runner_operation(
            name,
            &result_report(&second, first.generation)?,
            None,
            &generators(now)
        )?,
        Outcome::LeaseLost
    ));
    ensure!(open()?.runner_lease(id)? == before);
    ensure!(queue.entries.accepted_head(id)?.is_none());
    ensure!(events()? == count);
    let tx = queue.read_entry("contract_stale_run")?;
    ensure!(
        serde_json::to_value(runtime_store::stored_run(&tx, second.run.id())?.context("run")?)?
            == serde_json::to_value(&second.run)?
    );
    drop(tx);
    ensure!(matches!(
        queue.runner_operation(
            name,
            &result_report(&second, second.generation)?,
            None,
            &generators(now)
        )?,
        Outcome::Renewed { .. }
    ));
    ensure!(open()?.accepted_head(id)?.context("accepted head")?.head == "b".repeat(40));
    ensure!(events()? > count);
    Ok(())
}

/// A different runner receives the same expired run with a new generation,
/// rather than claiming a new run or retaining the old runner's lease.
pub fn assignment_expired_takeover<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let name = "contract-assignment-takeover";
    let queue = fixture(open()?, name)?;
    let first = pull(&queue, name, NOW)?;
    let token = format!("{name}-other");
    let now = NOW + LEASE.as_millis() as i64;
    let second = pull(&queue, &token, now)?;
    ensure!(second.run.id() == first.run.id());
    ensure!(second.generation == first.generation + 1);
    let lease = open()?
        .runner_lease(second.run.id().as_str())?
        .context("successor lease")?;
    ensure!(lease.runner_id == token && lease.generation == second.generation);
    ensure!(lease.expires_at_ms == now + LEASE.as_millis() as i64);
    Ok(())
}

/// An already-open queue rechecks revoked tokens for every operation; no
/// heartbeat, release, result or pull may retain its earlier authentication.
pub fn assignment_invalid_token<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let name = "contract-assignment-revoked";
    let queue = fixture(open()?, name)?;
    let assigned = pull(&queue, name, NOW)?;
    let id = assigned.run.id().as_str();
    let before = queue.entries.runner_lease(id)?;
    let tx = queue.write_entry("contract_revoke_token")?;
    tx.execute("DELETE FROM runner_tokens WHERE hash=?1", sql_params![name])?;
    tx.commit()?;
    for operation in [
        Operation::Heartbeat(lease_report(&assigned)),
        Operation::Release(lease_report(&assigned)),
        result_report(&assigned, assigned.generation)?,
        Operation::Poll(Poll {}),
    ] {
        ensure!(matches!(
            queue.runner_operation(name, &operation, None, &generators(NOW + 1_000))?,
            Outcome::Unauthenticated
        ));
    }
    ensure!(open()?.runner_lease(id)? == before);
    ensure!(queue.entries.accepted_head(id)?.is_none());
    let tx = queue.read_entry("contract_revoked_run")?;
    ensure!(
        serde_json::to_value(runtime_store::stored_run(&tx, assigned.run.id())?.context("run")?)?
            == serde_json::to_value(&assigned.run)?
    );
    Ok(())
}
