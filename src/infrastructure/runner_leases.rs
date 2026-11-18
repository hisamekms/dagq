//! Shared SQL for runner leases and their per-generation records. Every
//! change runs in the adapter's fenced write entry: it reads the run's lease,
//! asks [`crate::domain::runner_lease`] whether the call is of the current
//! generation, and updates on the generation it read. A refused call rolls
//! back without writing anything.
use super::sql_exec::{
    CommitTransaction, OptionalRow, SqlRead, SqlWrite, TransactionEntries, sql_params,
};
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

/// A previous holder is authority only for this exact run and generation.
/// Missing history (including generations predating the migration) fails closed.
pub(super) fn recorded_runner(
    tx: &impl SqlRead,
    run_id: &str,
    generation: i64,
) -> Result<Option<String>> {
    Ok(tx
        .query_row(
            "SELECT runner_id FROM runner_lease_records WHERE run_id=?1 AND generation=?2",
            sql_params![run_id, generation],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

fn remember_runner(
    tx: &impl SqlWrite,
    run_id: &str,
    generation: i64,
    runner_id: &str,
) -> Result<()> {
    let changed = tx.execute("INSERT INTO runner_lease_records(run_id,generation,runner_id) VALUES (?1,?2,?3) \
        ON CONFLICT (run_id,generation) DO UPDATE SET runner_id=excluded.runner_id \
        WHERE runner_lease_records.runner_id IS NULL OR runner_lease_records.runner_id=excluded.runner_id",
        sql_params![run_id,generation,runner_id])?;
    anyhow::ensure!(changed == 1, "a generation's recorded runner cannot change");
    Ok(())
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
    // Preserve an older binary's current holder too, before overwriting it.
    if let Some(old) = &current {
        remember_runner(tx, run_id, old.generation, &old.runner_id)?;
    }
    remember_runner(tx, run_id, generation, runner_id)?;
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
runner_lease_store!(super::store_queue::StoreQueue<E>, E: TransactionEntries);
runner_lease_store!(super::postgres::PostgresStore<S>, S: super::postgres::PostgresSession);

impl<E: TransactionEntries> crate::application::RunnerTakeoverStore
    for super::store_queue::StoreQueue<E>
where
    Self: RunnerLeaseStore,
{
    fn runner_ref_ends(&self) -> Result<Vec<crate::domain::run_refs::End>> {
        let tx = self.read_entry("runner_ref_ends")?;
        tx.query_map(
            "SELECT r.id, r.status, t.status, l.generation FROM runner_leases l JOIN task_runs r ON r.id=l.run_id JOIN tasks t ON t.id=r.task_id ORDER BY r.id",
            sql_params![],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?)),
        )?.into_iter().map(|(id, status, task, generation)| Ok(crate::domain::run_refs::End {run: id, status: status.parse()?, task_canceled: task == "canceled", generation})).collect()
    }
    fn runner_takeovers(&self) -> Result<Vec<crate::domain::runner_takeover::Takeover>> {
        let tx = self.read_entry("runner_takeovers")?;
        let ids: Vec<String> = tx.query_map(
            "SELECT run_id FROM runner_leases ORDER BY run_id",
            sql_params![],
            |r| r.get(0),
        )?;
        ids.into_iter()
            .map(|id| {
                let run =
                    super::runtime_store::stored_run(&tx, &crate::domain::RunId::new(id.clone())?)?
                        .context("runner run missing")?;
                Ok(crate::domain::runner_takeover::Takeover {
                    lease: lease(&tx, &id)?.context("runner lease missing")?,
                    status: run.status(),
                    records: records(&tx, &id)?,
                })
            })
            .collect()
    }
    fn runner_registration(
        &self,
        id: &str,
    ) -> Result<Option<crate::domain::runner_registration::Runner>> {
        super::runner_registration::runner(&self.read_entry("takeover_runner")?, id)
    }
    fn runner_owner(
        &self,
        runner: &crate::domain::runner_registration::Runner,
    ) -> Result<Option<crate::domain::queue_service::Member>> {
        super::members::read(&self.read_entry("takeover_owner")?, &runner.owner)
    }
    fn lose_runner_if_due(
        &self,
        run: &str,
        generation: i64,
        now_ms: i64,
    ) -> Result<Option<LossReason>> {
        let tx = self.write_entry("runner_loss")?;
        let Some(current) = lease(&tx, run)?.filter(|l| l.generation == generation) else {
            return Ok(None);
        };
        let runner = super::runner_registration::runner(&tx, &current.runner_id)?;
        let owner = runner
            .as_ref()
            .map(|r| super::members::read(&tx, &r.owner))
            .transpose()?
            .flatten();
        let reason = loss(&current, runner.as_ref(), owner.as_ref(), now_ms);
        if reason.is_some() {
            lose_lease(&tx, run, generation)?;
            tx.commit()?;
        }
        Ok(reason)
    }
    fn recover_runner(&mut self, lease: &RunnerLease) -> Result<()> {
        let id = crate::domain::RunId::new(lease.run_id.clone())?;
        let processes = crate::application::CoordinationReads::processes(self, &id)?;
        self.recover_checked(
            &id,
            processes.len(),
            serde_json::json!({"by":"runner", "generation":lease.generation}),
            Some(lease),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_keeps_heads_and_grant_preserves_only_known_holders() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        let migration = |statement: &str| {
            crate::infrastructure::schema::MIGRATIONS
                .iter()
                .find(|sql| sql.contains(statement))
                .unwrap()
        };
        conn.execute_batch(migration("CREATE TABLE runner_leases ("))
            .unwrap();
        conn.execute_batch("INSERT INTO runner_leases VALUES ('r',2,'b',100,'lost'),('unreported',1,'a',100,'held');
            INSERT INTO runner_lease_records(run_id,generation,accepted_head) VALUES ('r',1,'H');
            INSERT INTO runner_lease_records(run_id,generation,verified_ref_head) VALUES ('r',2,'H');").unwrap();
        conn.execute_batch(migration(
            "ALTER TABLE runner_lease_records ADD COLUMN runner_id",
        ))
        .unwrap();
        // Adding the column neither rewrites history nor invents evidence.
        for (run, generation) in [("r", 1), ("r", 2), ("unreported", 1)] {
            assert_eq!(recorded_runner(&conn, run, generation).unwrap(), None);
        }
        let current = lease(&conn, "unreported").unwrap().unwrap();
        assert!(
            crate::domain::runner_assignment::check_report("a", Some(&current), None, 1, 99)
                .is_ok()
        );
        // The first post-migration grants preserve the known current holders,
        // including a lease whose generation has no record yet.
        let tx = conn.transaction().unwrap();
        let port = super::super::sql_exec::sqlite::port_tx(&tx);
        assert!(matches!(
            grant_lease(&port, "r", "c", 100).unwrap(),
            Grant::Grant { generation: 3, .. }
        ));
        assert!(matches!(
            grant_lease(&port, "unreported", "b", 100).unwrap(),
            Grant::Grant { generation: 2, .. }
        ));
        tx.commit().unwrap();
        assert_eq!(recorded_runner(&conn, "r", 1).unwrap(), None);
        for (run, generation, runner) in [
            ("r", 2, "b"),
            ("r", 3, "c"),
            ("unreported", 1, "a"),
            ("unreported", 2, "b"),
        ] {
            assert_eq!(
                recorded_runner(&conn, run, generation).unwrap().as_deref(),
                Some(runner)
            );
        }
        let head: String = conn.query_row("SELECT verified_ref_head FROM runner_lease_records WHERE run_id='r' AND generation=2",[],|r|r.get(0)).unwrap();
        assert_eq!(head, "H");
        let head: String = conn
            .query_row(
                "SELECT accepted_head FROM runner_lease_records WHERE run_id='r' AND generation=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(head, "H");
    }
}
