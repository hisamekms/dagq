//! Store boundary contracts, shared by SQLite's memdb unit tests and the
//! Postgres CI example (ADR-t3986-1 decision 10). No application decisions.
//! Each factory opens the same migrated, otherwise unused database. Cases
//! run sequentially: business writes roll back except the fenced-write case,
//! which removes its row; leader cases release their last holder.
mod cases;
mod delegates;
mod goal_landing_marks;
mod goal_owners;
pub(crate) mod ledger_earlier;
mod ledger_first_executions;
mod ledger_open;
pub(crate) mod ledger_queue_lives;
pub(crate) mod ledger_queue_named;
mod ledger_queue_views;
pub use ledger_queue_lives::*;
pub(crate) mod ledger_range;
mod related;
mod runner_leases;
mod search;
mod search_writes;
pub use cases::*;
pub use delegates::*;
pub use goal_landing_marks::*;
pub use goal_owners::*;
pub use ledger_earlier::{ledger_earlier_answers_as_every_row, ledger_earlier_past_an_old_writer};
pub use ledger_first_executions::*;
pub use ledger_open::*;
pub use ledger_queue_named::{
    ledger_queue_named_answers_as_every_row, ledger_queue_named_goes_back_whole,
    ledger_queue_named_past_a_clock_set_back, ledger_queue_named_past_an_old_writer,
    ledger_queue_named_with_only_ci_after_a_future_main_event,
};
pub use ledger_queue_views::*;
pub use ledger_range::ledger_range_answers_as_every_row;
pub use related::related_cases;
pub use runner_leases::*;
pub use search::{search_cases, short_non_ascii};
pub use search_writes::{postgres_search_writes, source_search_writes};

use super::sql_exec::TransactionEntries;
use super::sqlite::WriteIdentity;
use crate::application::WriteLeaderStore;
use anyhow::{Context, Result};

/// The adapter's ownership operations around the execution and leader ports.
/// Opening another connection retains its own identity, as a second service
/// would. This is test plumbing, not an alternative transaction entry.
pub trait ContractStore:
    TransactionEntries
    + WriteLeaderStore
    + crate::application::queue_service::MemberStore
    + crate::application::RunnerStore
    + crate::application::RunnerLeaseStore
    + Sized
{
    fn related_in(
        &self,
        tx: &Self::Write<'_>,
        task: crate::domain::TaskId,
        statuses: &[String],
        limit: usize,
    ) -> Result<crate::domain::related::RelatedPage>;
    fn search_in(
        &self,
        tx: &Self::Write<'_>,
        query: &crate::domain::search::SearchQuery,
    ) -> Result<crate::domain::search::SearchPage>;
    fn with_identity(self, identity: WriteIdentity) -> Self;
    fn set_actor(&self, actor: crate::domain::actor::ActorContext);
    fn request(&self, requested_by: Option<String>) -> Option<String>;
    fn commit(tx: Self::Write<'_>) -> Result<()>;
}

impl<S: super::postgres::PostgresSession> ContractStore for super::postgres::PostgresStore<S> {
    fn related_in(
        &self,
        tx: &Self::Write<'_>,
        task: crate::domain::TaskId,
        statuses: &[String],
        limit: usize,
    ) -> Result<crate::domain::related::RelatedPage> {
        super::postgres::search::related(tx, task, statuses, limit)
    }
    fn search_in(
        &self,
        tx: &Self::Write<'_>,
        query: &crate::domain::search::SearchQuery,
    ) -> Result<crate::domain::search::SearchPage> {
        super::postgres::search::search(tx, query)
    }

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
        ("member credentials", member_credentials::<S, F>),
        ("goal owner records", goal_owner_records::<S, F>),
        ("goal owner moves", goal_owner_moves::<S, F>),
        ("goal owner move undone", goal_owner_move_undone::<S, F>),
        ("goal landing marks", goal_landing_marks_records::<S, F>),
        (
            "goal landing marks survive tags",
            goal_landing_marks_survive_tags::<S, F>,
        ),
        (
            "goal landing marks change with event",
            goal_landing_marks_change_with_event::<S, F>,
        ),
        (
            "goal landing marks change undone",
            goal_landing_marks_change_undone::<S, F>,
        ),
        ("delegates", delegates_records::<S, F>),
        ("delegates refused", delegates_refused::<S, F>),
        ("delegates change undone", delegates_change_undone::<S, F>),
        ("runner credentials", runner_credentials::<S, F>),
        ("runner leases", runner_leases::<S, F>),
        ("image digests", image_digests::<S, F>),
        (
            "ledger open written with its rows",
            ledger_open_written_with_its_rows::<S, F>,
        ),
        (
            "ledger open counts the runs left open",
            ledger_open_counts_the_runs_left_open::<S, F>,
        ),
        (
            "ledger open filled in parts",
            ledger_open_filled_in_parts::<S, F>,
        ),
        (
            "ledger open parts bounded by bytes",
            ledger_open_parts_bounded_by_bytes::<S, F>,
        ),
        (
            "ledger open kept through generations",
            ledger_open_kept_through_generations::<S, F>,
        ),
        (
            "first executions generations",
            ledger_first_executions_generations::<S, F>,
        ),
        (
            "first executions parts",
            ledger_first_executions_parts::<S, F>,
        ),
        (
            "ledger earlier answers as every row",
            ledger_earlier_answers_as_every_row::<S, F>,
        ),
        (
            "ledger earlier past an old writer",
            ledger_earlier_past_an_old_writer::<S, F>,
        ),
        (
            "ledger queue views written with their rows",
            ledger_queue_views_written_with_their_rows::<S, F>,
        ),
        (
            "ledger queue views count what a read names",
            ledger_queue_views_count_what_a_read_names::<S, F>,
        ),
        (
            "ledger queue views filled in parts",
            ledger_queue_views_filled_in_parts::<S, F>,
        ),
        (
            "ledger queue views kept through generations",
            ledger_queue_views_kept_through_generations::<S, F>,
        ),
        (
            "ledger queue views pair asks whichever comes first",
            ledger_queue_views_pair_asks_whichever_comes_first::<S, F>,
        ),
        (
            "ledger range answers as every row",
            ledger_range_answers_as_every_row::<S, F>,
        ),
        (
            "ledger queue named answers as every row",
            ledger_queue_named_answers_as_every_row::<S, F>,
        ),
        (
            "ledger queue named goes back whole",
            ledger_queue_named_goes_back_whole::<S, F>,
        ),
        (
            "ledger queue named past an old writer",
            ledger_queue_named_past_an_old_writer::<S, F>,
        ),
        (
            "ledger queue named past a clock set back",
            ledger_queue_named_past_a_clock_set_back::<S, F>,
        ),
        (
            "ledger queue named with only CI after a future main event",
            ledger_queue_named_with_only_ci_after_a_future_main_event::<S, F>,
        ),
        (
            "ledger named queue",
            ledger_queue_lives_answers_as_every_row::<S, F>,
        ),
        (
            "ledger named queue past old writer",
            ledger_queue_lives_past_an_old_writer::<S, F>,
        ),
        (
            "ledger named queue old snapshot",
            ledger_queue_lives_keeps_an_old_snapshot::<S, F>,
        ),
        ("shared spellings", shared_spellings::<S, F>),
        ("large ids", large_ids::<S, F>),
        ("source search writes", source_search_writes::<S, F>),
        ("search", search_cases::<S, F>),
        ("related", related_cases::<S, F>),
        ("short non-ASCII", short_non_ascii::<S, F>),
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
