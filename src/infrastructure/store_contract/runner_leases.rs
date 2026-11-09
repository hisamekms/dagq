//! Runner leases and their per-generation records through the execution
//! port (ADR-t3987-1 decisions 1 and 2). Which report is accepted is the
//! domain's decision, tested in `runner_lease`; these cases show the store
//! writes only what an accepted call changes and fences on the generation.
use super::ContractStore;
use crate::domain::runner_lease::{
    AcceptedHead, Grant, LEASE, LeaseState, Report, Verdict, VerifiedRef,
};
use crate::infrastructure::runner_leases::extend_held;
use anyhow::{Context, Result, ensure};

const RUN: &str = "contract-lease-run";

fn of(runner_id: &str, generation: i64) -> Report<'_> {
    Report {
        runner_id,
        generation,
    }
}

fn head(generation: i64, head: &str) -> Option<AcceptedHead> {
    Some(AcceptedHead {
        generation,
        head: head.into(),
    })
}

/// Grant, report, release, lose and verify on one run, persisting what an
/// accepted call changes. One refused report stands for every refusal: it
/// leaves the lease and the accepted head as they were, and the update
/// conditioned on the old generation changes no row.
pub fn runner_leases<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    let lease_ms = LEASE.as_millis() as i64;
    ensure!(store.runner_lease(RUN)?.is_none() && store.accepted_head(RUN)?.is_none());
    ensure!(
        store.grant_runner_lease(RUN, "runner-a", 1_000)?
            == Grant::Grant {
                generation: 1,
                expires_at_ms: 1_000 + lease_ms
            }
    );
    ensure!(matches!(
        open()?.grant_runner_lease(RUN, "runner-b", 2_000)?,
        Grant::Held { generation: 1, .. }
    ));
    ensure!(matches!(
        store.report_runner_lease(RUN, of("runner-a", 1), Some("h1"), 2_000)?,
        Verdict::Accept { .. }
    ));
    ensure!(open()?.accepted_head(RUN)? == head(1, "h1"));
    let held = store.runner_lease(RUN)?.context("granted lease")?;
    ensure!(held.expires_at_ms == 2_000 + lease_ms);
    // Past its end a grant passes the generation.
    let after = held.expires_at_ms;
    ensure!(matches!(
        store.grant_runner_lease(RUN, "runner-b", after)?,
        Grant::Grant { generation: 2, .. }
    ));
    let second = store.runner_lease(RUN)?.context("second lease")?;
    ensure!(second.generation == 2 && second.runner_id == "runner-b");
    // The old generation's report is refused and records nothing.
    ensure!(
        store.report_runner_lease(RUN, of("runner-a", 1), Some("stale"), after + 1)?
            == Verdict::LeaseLost
    );
    ensure!(store.runner_lease(RUN)?.as_ref() == Some(&second));
    ensure!(store.accepted_head(RUN)? == head(1, "h1"));
    // The update conditioned on the old generation changes no row.
    let tx = store.write_entry("contract_runner_lease")?;
    ensure!(extend_held(&tx, RUN, of("runner-a", 1), after + lease_ms, after + 1)? == 0);
    drop(tx);
    ensure!(store.runner_lease(RUN)?.as_ref() == Some(&second));
    // A liveness report of the new generation leaves the older head standing.
    ensure!(matches!(
        store.report_runner_lease(RUN, of("runner-b", 2), None, after + 1)?,
        Verdict::Accept { .. }
    ));
    ensure!(store.accepted_head(RUN)? == head(1, "h1"));
    ensure!(matches!(
        store.report_runner_lease(RUN, of("runner-b", 2), Some("h2"), after + 2)?,
        Verdict::Accept { .. }
    ));
    ensure!(open()?.accepted_head(RUN)? == head(2, "h2"));
    // A returned and a lost lease are stored as such.
    ensure!(matches!(
        store.release_runner_lease(RUN, of("runner-b", 2), after + 3)?,
        Verdict::Accept { .. }
    ));
    ensure!(store.runner_lease(RUN)?.context("released")?.state == LeaseState::Released);
    ensure!(matches!(
        store.grant_runner_lease(RUN, "runner-a", after + 5)?,
        Grant::Grant { generation: 3, .. }
    ));
    ensure!(matches!(
        store.lose_runner_lease(RUN, 3)?,
        Verdict::Accept { .. }
    ));
    ensure!(open()?.runner_lease(RUN)?.context("lost")?.state == LeaseState::Lost);
    ensure!(store.accepted_head(RUN)? == head(2, "h2"));
    // Verified refs: a generation's record without the mark is not one.
    ensure!(store.verified_ref(RUN)?.is_none());
    store.verify_runner_ref(RUN, 1, "h1", after + 7)?;
    ensure!(
        open()?.verified_ref(RUN)?
            == Some(VerifiedRef {
                generation: 1,
                head: "h1".into()
            })
    );
    store.verify_runner_ref(RUN, 4, "h2", after + 8)?;
    ensure!(
        store.verified_ref(RUN)?
            == Some(VerifiedRef {
                generation: 4,
                head: "h2".into()
            })
    );
    ensure!(store.accepted_head(RUN)? == head(2, "h2"));
    Ok(())
}
