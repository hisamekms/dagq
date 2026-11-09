//! Shared SQL for runner leases and their per-generation records. Every
//! change runs in the adapter's fenced write entry: it reads the run's lease,
//! asks [`crate::domain::runner_lease`] whether the call is of the current
//! generation, and updates on the generation it read. A refused call rolls
//! back without writing anything.
use super::sql_exec::{OptionalRow, SqlRead, SqlWrite, TransactionEntries, sql_params};
use crate::application::RunnerLeaseStore;
use crate::domain::runner_lease::*;
use anyhow::{Context, Result};

pub(super) fn lease(tx: &impl SqlRead, run_id: &str) -> Result<Option<RunnerLease>> {
    let row = tx
        .query_row(
            "SELECT generation, runner_id, expires_at_ms, state FROM runner_leases WHERE run_id = ?1",
            sql_params![run_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    row.map(|(generation, runner_id, expires_at_ms, state)| {
        Ok(RunnerLease {
            run_id: run_id.into(),
            generation,
            runner_id,
            expires_at_ms,
            state: LeaseState::parse(&state)
                .with_context(|| format!("runner lease of {run_id} has state {state:?}"))?,
        })
    })
    .transpose()
}

pub(super) fn records(tx: &impl SqlRead, run_id: &str) -> Result<Vec<GenerationRecord>> {
    Ok(tx.query_map(
        "SELECT generation, accepted_head, verified_ref_head FROM runner_lease_records WHERE run_id = ?1",
        sql_params![run_id],
        |r| {
            Ok(GenerationRecord {
                generation: r.get(0)?,
                accepted_head: r.get(1)?,
                verified_ref_head: r.get(2)?,
            })
        },
    )?)
}

/// Extend the held lease of `generation` granted to `runner_id`, if it has
/// not run out at `now_ms`: the rows changed, 0 for any other lease. Public
/// to the store contract, which shows an old generation changes nothing.
pub(crate) fn extend_held(
    tx: &impl SqlWrite,
    run_id: &str,
    report: Report<'_>,
    expires_at_ms: i64,
    now_ms: i64,
) -> Result<usize> {
    Ok(tx.execute(
        "UPDATE runner_leases SET expires_at_ms = ?4 WHERE run_id = ?1 AND generation = ?2 \
         AND runner_id = ?3 AND state = 'held' AND expires_at_ms > ?5",
        sql_params![
            run_id,
            report.generation,
            report.runner_id,
            expires_at_ms,
            now_ms
        ],
    )?)
}

pub(super) fn grant_lease(
    tx: &impl SqlWrite,
    run_id: &str,
    runner_id: &str,
    now_ms: i64,
) -> Result<Grant> {
    let current = lease(tx, run_id)?;
    let decision = grant(current.as_ref(), now_ms);
    let Grant::Grant {
        generation,
        expires_at_ms,
    } = decision
    else {
        return Ok(decision);
    };
    let changed = match &current {
        None => tx.execute(
            "INSERT INTO runner_leases(run_id, generation, runner_id, expires_at_ms, state) \
             VALUES (?1, ?2, ?3, ?4, 'held')",
            sql_params![run_id, generation, runner_id, expires_at_ms],
        )?,
        Some(old) => tx.execute(
            "UPDATE runner_leases SET generation = ?2, runner_id = ?3, expires_at_ms = ?4, \
             state = 'held' WHERE run_id = ?1 AND generation = ?5",
            sql_params![run_id, generation, runner_id, expires_at_ms, old.generation],
        )?,
    };
    anyhow::ensure!(
        changed == 1,
        "runner lease of {run_id} changed during its grant"
    );
    Ok(decision)
}

pub(super) fn report_lease(
    tx: &impl SqlWrite,
    run_id: &str,
    report: Report<'_>,
    head: Option<&str>,
    now_ms: i64,
) -> Result<Verdict> {
    let verdict = judge(lease(tx, run_id)?.as_ref(), report, now_ms);
    let Verdict::Accept { expires_at_ms } = verdict else {
        return Ok(verdict);
    };
    if extend_held(tx, run_id, report, expires_at_ms, now_ms)? != 1 {
        return Ok(Verdict::LeaseLost);
    }
    if let Some(head) = head_to_record(verdict, head) {
        tx.execute(
            "INSERT INTO runner_lease_records(run_id, generation, accepted_head, accepted_at_ms) \
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT (run_id, generation) DO UPDATE \
             SET accepted_head = excluded.accepted_head, accepted_at_ms = excluded.accepted_at_ms",
            sql_params![run_id, report.generation, head, now_ms],
        )?;
    }
    Ok(verdict)
}

pub(super) fn release_lease(
    tx: &impl SqlWrite,
    run_id: &str,
    report: Report<'_>,
    now_ms: i64,
) -> Result<Verdict> {
    let verdict = judge(lease(tx, run_id)?.as_ref(), report, now_ms);
    if verdict == Verdict::LeaseLost {
        return Ok(verdict);
    }
    let changed = tx.execute(
        "UPDATE runner_leases SET state = 'released' WHERE run_id = ?1 AND generation = ?2 \
         AND runner_id = ?3 AND state = 'held' AND expires_at_ms > ?4",
        sql_params![run_id, report.generation, report.runner_id, now_ms],
    )?;
    Ok(if changed == 1 {
        verdict
    } else {
        Verdict::LeaseLost
    })
}

fn lose_lease(tx: &impl SqlWrite, run_id: &str, generation: i64) -> Result<Verdict> {
    let verdict = lose(lease(tx, run_id)?.as_ref(), generation);
    if verdict == Verdict::LeaseLost {
        return Ok(verdict);
    }
    let changed = tx.execute(
        "UPDATE runner_leases SET state = 'lost' WHERE run_id = ?1 AND generation = ?2 AND state = 'held'",
        sql_params![run_id, generation],
    )?;
    Ok(if changed == 1 {
        verdict
    } else {
        Verdict::LeaseLost
    })
}

fn verify_ref(
    tx: &impl SqlWrite,
    run_id: &str,
    generation: i64,
    head: &str,
    now_ms: i64,
) -> Result<()> {
    anyhow::ensure!(
        ref_generation_allowed(lease(tx, run_id)?.as_ref(), generation),
        "a ref of generation {generation} is past run {run_id}'s next generation"
    );
    tx.execute(
        "INSERT INTO runner_lease_records(run_id, generation, verified_ref_head, verified_at_ms) \
         VALUES (?1, ?2, ?3, ?4) ON CONFLICT (run_id, generation) DO UPDATE \
         SET verified_ref_head = excluded.verified_ref_head, verified_at_ms = excluded.verified_at_ms",
        sql_params![run_id, generation, head, now_ms],
    )?;
    Ok(())
}

macro_rules! runner_lease_store {
    ($store:ty $(, $session:ident : $bound:path)?) => {
        impl $(<$session: $bound>)? RunnerLeaseStore for $store {
            fn grant_runner_lease(&self, run_id: &str, runner_id: &str, now_ms: i64) -> Result<Grant> {
                let tx = self.write_entry("runner_lease_grant")?;
                let decision = grant_lease(&tx, run_id, runner_id, now_ms)?;
                if matches!(decision, Grant::Grant { .. }) {
                    tx.commit()?;
                }
                Ok(decision)
            }
            fn report_runner_lease(&self, run_id: &str, report: Report<'_>, head: Option<&str>, now_ms: i64) -> Result<Verdict> {
                let tx = self.write_entry("runner_lease_report")?;
                let verdict = report_lease(&tx, run_id, report, head, now_ms)?;
                if verdict != Verdict::LeaseLost {
                    tx.commit()?;
                }
                Ok(verdict)
            }
            fn release_runner_lease(&self, run_id: &str, report: Report<'_>, now_ms: i64) -> Result<Verdict> {
                let tx = self.write_entry("runner_lease_release")?;
                let verdict = release_lease(&tx, run_id, report, now_ms)?;
                if verdict != Verdict::LeaseLost {
                    tx.commit()?;
                }
                Ok(verdict)
            }
            fn lose_runner_lease(&self, run_id: &str, generation: i64) -> Result<Verdict> {
                let tx = self.write_entry("runner_lease_lose")?;
                let verdict = lose_lease(&tx, run_id, generation)?;
                if verdict != Verdict::LeaseLost {
                    tx.commit()?;
                }
                Ok(verdict)
            }
            fn verify_runner_ref(&self, run_id: &str, generation: i64, head: &str, now_ms: i64) -> Result<()> {
                let tx = self.write_entry("runner_ref_verify")?;
                verify_ref(&tx, run_id, generation, head, now_ms)?;
                tx.commit()?;
                Ok(())
            }
            fn runner_lease(&self, run_id: &str) -> Result<Option<RunnerLease>> {
                lease(&self.read_entry("runner_lease")?, run_id)
            }
            fn accepted_head(&self, run_id: &str) -> Result<Option<AcceptedHead>> {
                Ok(newest_accepted_head(&records(&self.read_entry("runner_accepted_head")?, run_id)?))
            }
            fn verified_ref(&self, run_id: &str) -> Result<Option<VerifiedRef>> {
                Ok(newest_verified_ref(&records(&self.read_entry("runner_verified_ref")?, run_id)?))
            }
        }
    };
}
runner_lease_store!(super::sqlite::SqliteQueue);
runner_lease_store!(super::postgres::PostgresStore<S>, S: super::postgres::PostgresSession);
