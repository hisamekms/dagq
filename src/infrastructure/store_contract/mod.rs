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
pub(crate) mod ledger_report;
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
    WHOLE_STATES, WholeState, ledger_queue_named_answers_as_every_row,
    ledger_queue_named_goes_back_whole_of, ledger_queue_named_past_a_clock_set_back,
    ledger_queue_named_past_an_old_writer,
    ledger_queue_named_with_only_ci_after_a_future_main_event,
};
pub use ledger_queue_views::*;
pub use ledger_range::ledger_range_answers_as_every_row;
pub use ledger_report::{
    QUEUE_AND_CLOCKS, QueueAndClocks, ledger_report_answers_as_every_row,
    ledger_report_keeps_an_old_snapshot, ledger_report_past_a_named_task_s_history,
    ledger_report_past_a_target_s_ends, ledger_report_past_the_queue_and_the_clocks_of,
};
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
    sequence(&cases::<S, F>(), |case| case(open))
}

/// A case of [`run`].
pub(crate) type Case<F> = fn(&F) -> Result<()>;

/// [`run`]'s cases with their names, in its order.
pub(crate) fn cases<S: ContractStore, F: Fn() -> Result<S>>() -> Vec<(&'static str, Case<F>)> {
    vec![
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
        ("ledger queue named goes back whole of no mark", |open| {
            ledger_queue_named_goes_back_whole_of(open, WholeState::NoMark)
        }),
        (
            "ledger queue named goes back whole of a mark of no generation",
            |open| ledger_queue_named_goes_back_whole_of(open, WholeState::NoGeneration),
        ),
        (
            "ledger queue named goes back whole of a mark of another generation",
            |open| ledger_queue_named_goes_back_whole_of(open, WholeState::AnotherGeneration),
        ),
        (
            "ledger queue named goes back whole of a mark filled part way",
            |open| ledger_queue_named_goes_back_whole_of(open, WholeState::PartWay),
        ),
        (
            "ledger queue named goes back whole of a summary without its record",
            |open| ledger_queue_named_goes_back_whole_of(open, WholeState::SummaryWithoutRecord),
        ),
        (
            "ledger queue named goes back whole of a record without ci_by_time",
            |open| ledger_queue_named_goes_back_whole_of(open, WholeState::NoCiByTime),
        ),
        (
            "ledger queue named goes back whole of a record without main_by_time",
            |open| ledger_queue_named_goes_back_whole_of(open, WholeState::NoMainByTime),
        ),
        (
            "ledger queue named goes back whole of no marks of the rows",
            |open| ledger_queue_named_goes_back_whole_of(open, WholeState::NoRowMarks),
        ),
        ("ledger queue named goes back whole of no tables", |open| {
            ledger_queue_named_goes_back_whole_of(open, WholeState::NoTables)
        }),
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
        (
            "ledger report answers as every row",
            ledger_report_answers_as_every_row::<S, F>,
        ),
        (
            "ledger report past the queue and the clocks of lives shared by the units",
            |open| {
                ledger_report_past_the_queue_and_the_clocks_of(open, QueueAndClocks::SharedLives)
            },
        ),
        (
            "ledger report past the queue and the clocks of a life of each unit",
            |open| {
                ledger_report_past_the_queue_and_the_clocks_of(open, QueueAndClocks::UnsharedLives)
            },
        ),
        (
            "ledger report past the queue and the clocks of a clock set back",
            |open| {
                ledger_report_past_the_queue_and_the_clocks_of(open, QueueAndClocks::ClockSetBack)
            },
        ),
        (
            "ledger report past the queue and the clocks of the clocks in the range",
            |open| {
                ledger_report_past_the_queue_and_the_clocks_of(
                    open,
                    QueueAndClocks::ClocksInTheRange,
                )
            },
        ),
        (
            "ledger report past the queue and the clocks of CI alone after a future main event",
            |open| ledger_report_past_the_queue_and_the_clocks_of(open, QueueAndClocks::CiOnly),
        ),
        (
            "ledger report past the queue and the clocks of the range's clocks",
            |open| {
                ledger_report_past_the_queue_and_the_clocks_of(open, QueueAndClocks::RangeClocks)
            },
        ),
        (
            "ledger report old snapshot",
            ledger_report_keeps_an_old_snapshot::<S, F>,
        ),
        (
            "ledger report past a named task's history",
            ledger_report_past_a_named_task_s_history::<S, F>,
        ),
        (
            "ledger report past a target's ends",
            ledger_report_past_a_target_s_ends::<S, F>,
        ),
        (
            "kpi compare groups",
            ledger_queue_named::kpi_compare_groups_read_by_their_range::<S, F>,
        ),
        (
            "kpi forecast through the entry",
            ledger_queue_lives_forecast_through_kpi::<S, F>,
        ),
        ("shared spellings", shared_spellings::<S, F>),
        ("large ids", large_ids::<S, F>),
        ("source search writes", source_search_writes::<S, F>),
        ("search", search_cases::<S, F>),
        ("related", related_cases::<S, F>),
        ("short non-ASCII", short_non_ascii::<S, F>),
    ]
}

/// Whether a case checks what it reads (`Run`) or makes its writes alone
/// (`WritesAlone`): the same writes in the same transactions and commits,
/// the write leader taken over as the same holders, and none of the reads
/// that only its checks use. A case of [`writes_alone`] runs so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Checks {
    Run,
    WritesAlone,
}

impl Checks {
    /// `check`, which only reads and compares, when the checks run.
    pub(crate) fn check(self, check: impl FnOnce() -> Result<()>) -> Result<()> {
        self.read(check).map(|_| ())
    }

    /// What `read`, which only reads, gives the checks, when they run.
    pub(crate) fn read<T>(self, read: impl FnOnce() -> Result<T>) -> Result<Option<T>> {
        match self {
            Self::Run => read().map(Some),
            Self::WritesAlone => Ok(None),
        }
    }
}

/// The cases of [`run`] that commit what the cases after them meet and
/// that no test may run apart (the cases past an older writer, whose
/// writers take the write leader over and commit, moving its generation,
/// the counters of the ids and the search's index), each with the same
/// case of its writes alone ([`Checks::WritesAlone`]).
pub(crate) fn writes_alone<S: ContractStore, F: Fn() -> Result<S>>() -> Vec<(&'static str, Case<F>)>
{
    vec![
        ("ledger earlier past an old writer", |open| {
            ledger_earlier::ledger_earlier_past_an_old_writer_of(open, Checks::WritesAlone)
        }),
        ("ledger queue named past an old writer", |open| {
            ledger_queue_named::ledger_queue_named_past_an_old_writer_of(open, Checks::WritesAlone)
        }),
        ("ledger named queue past old writer", |open| {
            ledger_queue_lives::ledger_queue_lives_past_an_old_writer_of(open, Checks::WritesAlone)
        }),
    ]
}

/// [`run`] in its order on one database, but each case named in `apart`
/// runs only when it is `full`, and then must leave the database as it
/// found it (`state`, the whole content of the database, the same before
/// and after it), and is the last to run. A case of `apart` thus changes
/// nothing the cases after it read, and each case meets the database the
/// whole of [`run`] gives it: a store's tests may run [`run`] as one test
/// of each case of `apart` (`full`) and one of none (`full` of `None`, every
/// case not apart), in parallel. Errors name the failing case, as [`run`]'s
/// do, and a name of `apart` that is no case.
///
/// A case of [`writes_alone`] runs with its checks in the test of none and
/// as its writes alone in the test of a case of `apart` after it: the
/// database it leaves is the same of either, as its writes are, so that a
/// case of `apart` meets what the whole of [`run`] gives it without the
/// reads that the test of none checks once.
pub(crate) fn run_apart<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    apart: &[&str],
    full: Option<&str>,
    state: &dyn Fn() -> Result<String>,
) -> Result<()> {
    sequence_apart(
        &cases::<S, F>(),
        apart,
        &writes_alone::<S, F>(),
        full,
        state,
        |case| case(open),
    )
}

/// [`run_apart`] of named `cases`, each run by `run`, and of the cases of
/// `alone`, run in place of the cases of their names in the test of a case
/// of `apart`.
fn sequence_apart<T>(
    cases: &[(&str, T)],
    apart: &[&str],
    alone: &[(&str, T)],
    full: Option<&str>,
    state: &dyn Fn() -> Result<String>,
    mut run: impl FnMut(&T) -> Result<()>,
) -> Result<()> {
    for name in apart
        .iter()
        .chain(full.iter())
        .chain(alone.iter().map(|(name, _)| name))
    {
        anyhow::ensure!(
            cases.iter().any(|(case, _)| case == name),
            "store contract: no case {name}"
        );
    }
    anyhow::ensure!(
        full.is_none_or(|full| apart.contains(&full)),
        "store contract: {full:?} is not apart"
    );
    for (name, _) in alone {
        anyhow::ensure!(
            !apart.contains(name),
            "store contract: {name} runs its writes alone and is apart"
        );
    }
    for (name, case) in cases {
        if !apart.contains(name) {
            let case = match alone.iter().find(|(of, _)| of == name) {
                Some((_, writes)) if full.is_some() => writes,
                _ => case,
            };
            run(case).with_context(|| format!("store contract: {name}"))?;
        } else if full == Some(*name) {
            let before = state()?;
            run(case).with_context(|| format!("store contract: {name}"))?;
            let after = state()?;
            let changed: Vec<&str> = after
                .lines()
                .filter(|line| !before.lines().any(|was| was == *line))
                .collect();
            anyhow::ensure!(
                after == before,
                "store contract: {name} leaves the database other than it found it: {changed:?}"
            );
            return Ok(());
        }
    }
    Ok(())
}

/// What [`OfSnapshot`] opens: `read` of one snapshot of a contract's
/// transaction, as a store's [`crate::application::LedgerStore::ledger_read_snapshot`]
/// gives it.
pub(crate) type OpenSnapshot<'a> = dyn Fn(&mut dyn FnMut(&dyn crate::application::LedgerWindowRead) -> Result<()>) -> Result<()>
    + 'a;

/// A ledger store whose one read is a snapshot of a contract's transaction
/// (`open`, through a counting read when the contract counts what is
/// returned): what `kpi`'s entry ([`crate::application::kpi::kpi`]) is
/// called on, so that the contract drives the entry's own read
/// ([`crate::application::ledger_window_planned`] and
/// `ledger_read_snapshot`). It writes nothing and reads nothing apart from
/// its snapshot.
pub(crate) struct OfSnapshot<'a> {
    pub(crate) open: &'a OpenSnapshot<'a>,
}

impl crate::application::LedgerStore for OfSnapshot<'_> {
    fn ledger_read_snapshot(
        &self,
        read: &mut dyn FnMut(&dyn crate::application::LedgerWindowRead) -> Result<()>,
    ) -> Result<()> {
        (self.open)(read)
    }
    fn write_ledger_row(&self, _: &crate::domain::ledger::LedgerRow) -> Result<bool> {
        anyhow::bail!("a snapshot writes nothing")
    }
    fn write_ledger_row_over(
        &self,
        _: &crate::domain::ledger::LedgerRow,
        _: Option<&crate::domain::ledger::LedgerRow>,
    ) -> Result<Option<bool>> {
        anyhow::bail!("a snapshot writes nothing")
    }
    fn write_session_rows_over(
        &self,
        _: &[(
            crate::domain::ledger::LedgerRow,
            Option<crate::domain::ledger::LedgerRow>,
        )],
        _: Option<&crate::domain::ledger::session::written::SessionsWritten>,
    ) -> Result<bool> {
        anyhow::bail!("a snapshot writes nothing")
    }
    fn sessions_written(
        &self,
    ) -> Result<Option<crate::domain::ledger::session::written::SessionsWritten>> {
        anyhow::bail!("read only through its snapshot")
    }
    fn ledger_row(
        &self,
        _: crate::domain::ledger::LedgerKind,
        _: &str,
    ) -> Result<Option<crate::domain::ledger::LedgerRow>> {
        anyhow::bail!("read only through its snapshot")
    }
    fn ledger_rows(
        &self,
        _: crate::domain::ledger::LedgerKind,
    ) -> Result<Vec<crate::domain::ledger::LedgerRow>> {
        anyhow::bail!("read only through its snapshot")
    }
    fn ledger_legacy_runs_without_row(&self) -> Result<Vec<crate::domain::RunId>> {
        anyhow::bail!("read only through its snapshot")
    }
    fn ledger_runs_claimed(&self, _: crate::domain::TaskId) -> Result<Vec<crate::domain::RunId>> {
        anyhow::bail!("read only through its snapshot")
    }
    fn ledger_runs_past_their_rows(
        &self,
        _: crate::domain::EventId,
    ) -> Result<Vec<crate::domain::RunId>> {
        anyhow::bail!("read only through its snapshot")
    }
    fn remove_node_rows(&self, _: i64) -> Result<usize> {
        anyhow::bail!("a snapshot writes nothing")
    }
    fn remove_omitted_queue_rows(&self) -> Result<usize> {
        anyhow::bail!("a snapshot writes nothing")
    }
    fn ledger_run_events(&self, _: &crate::domain::RunId) -> Result<Vec<crate::domain::RunEvent>> {
        anyhow::bail!("read only through its snapshot")
    }
    fn last_event_naming_supervisor(
        &self,
        _: &str,
        _: crate::domain::EventId,
    ) -> Result<Option<crate::domain::RunEvent>> {
        anyhow::bail!("read only through its snapshot")
    }
}

/// Whether the answers of a history of `small` settled units before every
/// range are, question by question, those of `large` units, but for the
/// event ids the units shift ([`without_event_ids`]): the units change
/// neither what a question reads ([`ledger_queue_named::Returned`]) nor
/// what it answers.
pub(crate) fn same_answers(
    small: &[serde_json::Value],
    large: &[serde_json::Value],
    small_units: usize,
    large_units: usize,
) -> Result<()> {
    anyhow::ensure!(
        small.len() == large.len(),
        "{} of {}",
        small.len(),
        large.len()
    );
    for (n, (a, b)) in small.iter().zip(large).enumerate() {
        let mut misses = Vec::new();
        ledger_earlier::differences(
            &without_event_ids(a),
            &without_event_ids(b),
            &format!("[{n}] of {small_units} and {large_units} units"),
            &mut misses,
        );
        anyhow::ensure!(misses.is_empty(), "{misses:?}");
    }
    Ok(())
}

/// `value` without the event ids an answer names (a mark's `id`, its
/// claim's `claim_event` and any `*event_id`), which the settled units
/// before them shift.
fn without_event_ids(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| {
                    !matches!(key.as_str(), "id" | "claim_event") && !key.ends_with("event_id")
                })
                .map(|(key, value)| (key.clone(), without_event_ids(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_event_ids).collect()),
        other => other.clone(),
    }
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

    /// The cases of `[("a", 1), ("b", 2), ("c", 3), ("d", 4)]` with "b"
    /// and "d" apart and "c" of its writes alone as 30, of `full`, against
    /// a database whose content is the cases run that wrote (`writes`):
    /// what ran, or the error.
    fn apart_of(full: Option<&str>, writes: &[i32]) -> Result<Vec<i32>> {
        let seen = std::cell::RefCell::new(Vec::new());
        let written = std::cell::RefCell::new(Vec::new());
        sequence_apart(
            &[("a", 1), ("b", 2), ("c", 3), ("d", 4)],
            &["b", "d"],
            &[("c", 30)],
            full,
            &|| Ok(format!("{:?}", written.borrow())),
            |n| {
                seen.borrow_mut().push(*n);
                if writes.contains(n) {
                    written.borrow_mut().push(*n);
                }
                Ok(())
            },
        )
        .map(|()| seen.into_inner())
    }

    /// The test of none runs a case of its writes alone in full, and the
    /// test of a case apart after it its writes alone.
    #[test]
    fn the_cases_apart_run_alone_after_every_case_before_them() {
        assert_eq!(apart_of(None, &[]).unwrap(), [1, 3]);
        assert_eq!(apart_of(Some("b"), &[]).unwrap(), [1, 2]);
        assert_eq!(apart_of(Some("d"), &[1, 30]).unwrap(), [1, 30, 4]);
    }

    #[test]
    fn a_case_apart_that_changes_the_database_fails_the_run() {
        let error = apart_of(Some("d"), &[4]).unwrap_err();
        assert!(
            error.to_string().contains("d leaves the database"),
            "{error}"
        );
        assert!(apart_of(Some("b"), &[1, 3]).is_ok());
    }

    #[test]
    fn a_name_apart_that_is_no_case_fails_the_run() {
        let run = |apart: &[&str], alone: &[(&str, i32)], full| {
            sequence_apart(
                &[("a", 1)],
                apart,
                alone,
                full,
                &|| Ok(String::new()),
                |_| Ok(()),
            )
        };
        assert!(
            run(&["z"], &[], None)
                .unwrap_err()
                .to_string()
                .contains("no case z")
        );
        assert!(
            run(&[], &[("z", 2)], None)
                .unwrap_err()
                .to_string()
                .contains("no case z")
        );
        assert!(
            run(&["a"], &[("a", 2)], None)
                .unwrap_err()
                .to_string()
                .contains("runs its writes alone and is apart")
        );
        assert!(
            run(&[], &[], Some("a"))
                .unwrap_err()
                .to_string()
                .contains("not apart")
        );
        assert!(run(&["a"], &[], Some("a")).is_ok());
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
