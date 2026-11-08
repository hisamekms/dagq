//! Store boundary contracts, shared by SQLite's memdb unit tests and the
//! Postgres CI example (ADR-t3986-1 decision 10). No application decisions.
//! Each factory opens the same migrated, otherwise unused database. Cases
//! run sequentially: business writes roll back except the fenced-write case,
//! which removes its row; leader cases release their last holder.
mod cases;
mod search_writes;
pub use cases::*;
pub use search_writes::postgres_search_writes;

use super::sql_exec::TransactionEntries;
use super::sqlite::WriteIdentity;
use crate::application::WriteLeaderStore;
use anyhow::{Context, Result};

/// The adapter's ownership operations around the execution and leader ports.
/// Opening another connection retains its own identity, as a second service
/// would. This is test plumbing, not an alternative transaction entry.
pub trait ContractStore:
    TransactionEntries + WriteLeaderStore + crate::application::queue_service::MemberStore + Sized
{
    fn with_identity(self, identity: WriteIdentity) -> Self;
    fn set_actor(&self, actor: crate::domain::actor::ActorContext);
    fn request(&self, requested_by: Option<String>) -> Option<String>;
    fn commit(tx: Self::Write<'_>) -> Result<()>;
}

impl<S: super::postgres::PostgresSession> ContractStore for super::postgres::PostgresStore<S> {
    fn with_identity(self, identity: WriteIdentity) -> Self {
        self.with_write_identity(identity)
    }
    fn set_actor(&self, actor: crate::domain::actor::ActorContext) {
        self.set_actor(actor);
    }
    fn request(&self, requested_by: Option<String>) -> Option<String> {
        self.request(requested_by)
    }
    fn commit(tx: Self::Write<'_>) -> Result<()> {
        tx.commit()
    }
}

/// Refuse a missing connection before invoking any setup or case. The
/// example propagates errors to its exit status, including the first failed
/// case, rather than silently skipping an unavailable Postgres boundary.
pub fn run_configured(
    connection: Option<&str>,
    run: impl FnOnce(&str) -> Result<()>,
) -> Result<()> {
    run(connection.context("DAGQ_TEST_POSTGRES_URL is required")?)
}

/// Run each named case in order and stop at the first failure.
fn sequence<T>(cases: &[(&str, T)], mut run: impl FnMut(&T) -> Result<()>) -> Result<()> {
    for (name, case) in cases {
        run(case).with_context(|| format!("store contract: {name}"))?;
    }
    Ok(())
}

/// All contracts against `open`; errors name the failing case.
pub fn run<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    type Case<F> = fn(&F) -> Result<()>;
    let cases: &[(&str, Case<F>)] = &[
        ("leader lifecycle", leader_lifecycle::<S, F>),
        ("leader singleton", leader_singleton::<S, F>),
        ("stale writer", stale_writer::<S, F>),
        ("unique indexes", unique_indexes::<S, F>),
        ("base types", base_types::<S, F>),
        ("json set field", json_set_field::<S, F>),
        ("event actors", event_actors::<S, F>),
        ("event bodies", event_bodies::<S, F>),
        ("typed conditions", typed_conditions::<S, F>),
        ("backend parallel", backend_parallel::<S, F>),
        ("members", members::<S, F>),
    ];
    sequence(cases, |case| case(open))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_connection_runs_neither_setup_nor_cases() {
        let error = run_configured(None, |_| panic!("must not run")).unwrap_err();
        assert!(error.to_string().contains("DAGQ_TEST_POSTGRES_URL"));
    }

    #[test]
    fn a_failed_case_fails_the_run_and_stops_the_sequence() {
        let mut seen = Vec::new();
        let result = run_configured(Some("configured"), |url| {
            assert_eq!(url, "configured");
            sequence(&[("first", 1), ("second", 2), ("third", 3)], |n| {
                seen.push(*n);
                anyhow::ensure!(*n != 2, "case failed");
                Ok(())
            })
        });
        assert_eq!(seen, [1, 2]);
        assert!(format!("{:#}", result.unwrap_err()).contains("second"));
    }

    #[test]
    fn a_passing_sequence_runs_every_case_in_order() {
        let mut seen = Vec::new();
        run_configured(Some("configured"), |_| {
            sequence(&[("first", 1), ("second", 2)], |n| {
                seen.push(*n);
                Ok(())
            })
        })
        .unwrap();
        assert_eq!(seen, [1, 2]);
    }
}
