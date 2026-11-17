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
mod ledger_late_provider;
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
pub use ledger_late_provider::{
    LATE_WINDOWS, LateWindow, ledger_late_provider_answers_as_every_row,
    ledger_late_provider_answers_as_every_row_of,
};
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

/// All contracts against `open`, at the original volume counts; errors
/// name the failing case. Postgres and SQLite's volume suite share this
/// case list, order and sizes.
pub fn run<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    sequence(&cases::<S, F>(), |case| case.run(open))
}

/// A case of [`run`], carrying the fixture arguments the runner passes
/// to it. Sizes describe the executed fixture, never a separate label.
pub(crate) enum Case<F> {
    Fixed(fn(&F) -> Result<()>),
    History {
        run: fn(&F, usize) -> Result<()>,
        units: usize,
    },
    Histories {
        run: fn(&F, usize, usize) -> Result<()>,
        first: usize,
        second: usize,
    },
    Parts {
        run: fn(&F, usize, usize) -> Result<()>,
        rows: usize,
        bytes: usize,
    },
}

impl<F> Case<F> {
    fn run(&self, open: &F) -> Result<()> {
        match self {
            Self::Fixed(run) => run(open),
            Self::History { run, units } => run(open, *units),
            Self::Histories { run, first, second } => run(open, *first, *second),
            Self::Parts { run, rows, bytes } => run(open, *rows, *bytes),
        }
    }
}

/// [`run`]'s cases with their names, in its order.
pub(crate) fn cases<S: ContractStore, F: Fn() -> Result<S>>() -> Vec<(&'static str, Case<F>)> {
    cases_sized::<S, F, false>()
}

thread_local! {
    /// Fixed background units in named fixtures, scoped to a small apart
    /// case alone. Committing predecessors always use the original count.
    static FIXED_HISTORY_UNITS: std::cell::Cell<usize> = const { std::cell::Cell::new(30) };
}

pub(crate) fn fixed_history_units() -> usize {
    FIXED_HISTORY_UNITS.get()
}

/// Size only the selected rollback case, never the committing prefix or
/// its writes-alone replay. Restore the fixture size on errors and panics,
/// too, and keep parallel test threads independent.
fn at_case_size<T>(small: bool, apart: &[&str], name: &str, run: impl FnOnce() -> T) -> T {
    if !small || !apart.contains(&name) {
        return run();
    }
    struct Restore(usize);
    impl Drop for Restore {
        fn drop(&mut self) {
            FIXED_HISTORY_UNITS.set(self.0);
        }
    }
    let _restore = Restore(FIXED_HISTORY_UNITS.replace(2));
    run()
}

/// The identical named sequence at two sizes. With `SMALL`, rolling-back
/// histories use 2/4 settled units (both even, keeping the alternating
/// commit/revert tail); a single history uses 2 units. In particular:
///
/// - open/queue-view counts, range/named/clock/CI-only reads, lives, report,
///   every report queue/clock state, report named-task/target-ends/old-snapshot,
///   KPI compare and forecast: 2/4 instead of 30/300 (report variants 30/150).
/// - earlier answers: 2 settled units and 2 member changes instead of 30/30;
///   named fallback states and lives' old snapshot: 2 instead of 30.
/// - open/first-Execution/queue-view parts: 2 rows per fill and 1024 bytes,
///   instead of ROWS_PER_FILL/FILL_BYTES. First-Execution writes 3 rows;
///   open writes 4 settled runs plus its fixed boundary runs; queue writes
///   1 settled unit plus its fixed boundary events. The byte cases pad a
///   row past 1024 bytes, with a 2-row page so bytes, not the page, stop it.
///
/// All assertions and fixed boundary events remain. Apart cases also use
/// 2 instead of 30 background earlier units; CI-only and report member-change
/// fixtures use 2 background named units too. Summary padding and forecast's
/// 10/20/40 open-period marks remain. The clock-set-back case and report of
/// that clock use 30/32 named units: below a kept summary after the clock
/// anomaly, CI's out-of-range checks still grow the alternative's prefix read.
/// The three old-writer cases keep 30 units and the identical writes-alone
/// replay. All committing cases and other fixed fixtures stay unchanged:
/// later cases meet identical rows, generations, ids and indexes. Only
/// rollback work shrinks, so interference is not weakened.
/// With `SMALL=false`, this is exactly [`run`]'s sequence and original sizes,
/// shared by SQLite's volume tests and Postgres, never a second case list.
fn cases_sized<S: ContractStore, F: Fn() -> Result<S>, const SMALL: bool>()
-> Vec<(&'static str, Case<F>)> {
    let histories = |run| Case::Histories {
        run,
        first: if SMALL { 2 } else { 30 },
        second: if SMALL { 4 } else { 300 },
    };
    let report = |run| Case::Histories {
        run,
        first: if SMALL { 2 } else { 30 },
        second: if SMALL { 4 } else { 150 },
    };
    let history = |run| Case::History {
        run,
        units: if SMALL { 2 } else { 30 },
    };
    let parts = |run| Case::Parts {
        run,
        rows: if SMALL {
            2
        } else {
            crate::domain::ledger::open::ROWS_PER_FILL
        },
        bytes: if SMALL {
            1024
        } else {
            crate::domain::ledger::open::FILL_BYTES
        },
    };
    vec![
        ("leader lifecycle", Case::Fixed(leader_lifecycle::<S, F>)),
        ("leader singleton", Case::Fixed(leader_singleton::<S, F>)),
        ("stale writer", Case::Fixed(stale_writer::<S, F>)),
        ("unique indexes", Case::Fixed(unique_indexes::<S, F>)),
        ("base types", Case::Fixed(base_types::<S, F>)),
        ("json set field", Case::Fixed(json_set_field::<S, F>)),
        ("event actors", Case::Fixed(event_actors::<S, F>)),
        ("event bodies", Case::Fixed(event_bodies::<S, F>)),
        ("typed conditions", Case::Fixed(typed_conditions::<S, F>)),
        ("backend parallel", Case::Fixed(backend_parallel::<S, F>)),
        ("members", Case::Fixed(members::<S, F>)),
        (
            "member credentials",
            Case::Fixed(member_credentials::<S, F>),
        ),
        (
            "goal owner records",
            Case::Fixed(goal_owner_records::<S, F>),
        ),
        ("goal owner moves", Case::Fixed(goal_owner_moves::<S, F>)),
        (
            "goal owner move undone",
            Case::Fixed(goal_owner_move_undone::<S, F>),
        ),
        (
            "goal landing marks",
            Case::Fixed(goal_landing_marks_records::<S, F>),
        ),
        (
            "goal landing marks survive tags",
            Case::Fixed(goal_landing_marks_survive_tags::<S, F>),
        ),
        (
            "goal landing marks change with event",
            Case::Fixed(goal_landing_marks_change_with_event::<S, F>),
        ),
        (
            "goal landing marks change undone",
            Case::Fixed(goal_landing_marks_change_undone::<S, F>),
        ),
        ("delegates", Case::Fixed(delegates_records::<S, F>)),
        ("delegates refused", Case::Fixed(delegates_refused::<S, F>)),
        (
            "delegates change undone",
            Case::Fixed(delegates_change_undone::<S, F>),
        ),
        (
            "runner credentials",
            Case::Fixed(runner_credentials::<S, F>),
        ),
        ("runner leases", Case::Fixed(runner_leases::<S, F>)),
        ("image digests", Case::Fixed(image_digests::<S, F>)),
        (
            "ledger open written with its rows",
            Case::Fixed(ledger_open_written_with_its_rows::<S, F>),
        ),
        (
            "ledger open counts the runs left open",
            histories(|open, a, b| {
                ledger_open::ledger_open_counts_the_runs_left_open_with(open, a as u64, b as u64)
            }),
        ),
        (
            "ledger open filled in parts",
            parts(ledger_open::ledger_open_filled_in_parts_with::<S, F>),
        ),
        (
            "ledger open parts bounded by bytes",
            parts(ledger_open::ledger_open_parts_bounded_by_bytes_with::<S, F>),
        ),
        (
            "ledger open kept through generations",
            Case::Fixed(ledger_open_kept_through_generations::<S, F>),
        ),
        (
            "first executions generations",
            Case::Fixed(ledger_first_executions_generations::<S, F>),
        ),
        (
            "first executions parts",
            parts(ledger_first_executions::ledger_first_executions_parts_with::<S, F>),
        ),
        (
            "ledger earlier answers as every row",
            Case::Histories {
                run: ledger_earlier::ledger_earlier_answers_as_every_row_with::<S, F>,
                first: if SMALL { 2 } else { 30 },
                second: if SMALL { 2 } else { 30 },
            },
        ),
        (
            "ledger earlier past an old writer",
            Case::Fixed(ledger_earlier_past_an_old_writer::<S, F>),
        ),
        (
            "ledger queue views written with their rows",
            Case::Fixed(ledger_queue_views_written_with_their_rows::<S, F>),
        ),
        (
            "ledger queue views count what a read names",
            histories(ledger_queue_views::ledger_queue_views_count_what_a_read_names_with::<S, F>),
        ),
        (
            "ledger queue views filled in parts",
            parts(ledger_queue_views::ledger_queue_views_filled_in_parts_with::<S, F>),
        ),
        (
            "ledger queue views kept through generations",
            Case::Fixed(ledger_queue_views_kept_through_generations::<S, F>),
        ),
        (
            "ledger queue views pair asks whichever comes first",
            Case::Fixed(ledger_queue_views_pair_asks_whichever_comes_first::<S, F>),
        ),
        (
            "ledger late provider answers as every row",
            Case::Fixed(ledger_late_provider_answers_as_every_row::<S, F>),
        ),
        (
            "ledger range answers as every row",
            histories(ledger_range::ledger_range_with::<S, F>),
        ),
        (
            "ledger queue named answers as every row",
            histories(ledger_queue_named::ledger_queue_named_with::<S, F>),
        ),
        (
            "ledger queue named goes back whole of no mark",
            history(|open, units| {
                ledger_queue_named::ledger_queue_named_goes_back_whole_with(
                    open,
                    WholeState::NoMark,
                    units,
                )
            }),
        ),
        (
            "ledger queue named goes back whole of a mark of no generation",
            history(|open, units| {
                ledger_queue_named::ledger_queue_named_goes_back_whole_with(
                    open,
                    WholeState::NoGeneration,
                    units,
                )
            }),
        ),
        (
            "ledger queue named goes back whole of a mark of another generation",
            history(|open, units| {
                ledger_queue_named::ledger_queue_named_goes_back_whole_with(
                    open,
                    WholeState::AnotherGeneration,
                    units,
                )
            }),
        ),
        (
            "ledger queue named goes back whole of a mark filled part way",
            history(|open, units| {
                ledger_queue_named::ledger_queue_named_goes_back_whole_with(
                    open,
                    WholeState::PartWay,
                    units,
                )
            }),
        ),
        (
            "ledger queue named goes back whole of a summary without its record",
            history(|open, units| {
                ledger_queue_named::ledger_queue_named_goes_back_whole_with(
                    open,
                    WholeState::SummaryWithoutRecord,
                    units,
                )
            }),
        ),
        (
            "ledger queue named goes back whole of a record without ci_by_time",
            history(|open, units| {
                ledger_queue_named::ledger_queue_named_goes_back_whole_with(
                    open,
                    WholeState::NoCiByTime,
                    units,
                )
            }),
        ),
        (
            "ledger queue named goes back whole of a record without main_by_time",
            history(|open, units| {
                ledger_queue_named::ledger_queue_named_goes_back_whole_with(
                    open,
                    WholeState::NoMainByTime,
                    units,
                )
            }),
        ),
        (
            "ledger queue named goes back whole of no marks of the rows",
            history(|open, units| {
                ledger_queue_named::ledger_queue_named_goes_back_whole_with(
                    open,
                    WholeState::NoRowMarks,
                    units,
                )
            }),
        ),
        (
            "ledger queue named goes back whole of no tables",
            history(|open, units| {
                ledger_queue_named::ledger_queue_named_goes_back_whole_with(
                    open,
                    WholeState::NoTables,
                    units,
                )
            }),
        ),
        (
            "ledger queue named past an old writer",
            Case::Fixed(ledger_queue_named_past_an_old_writer::<S, F>),
        ),
        (
            "ledger queue named past a clock set back",
            Case::Histories {
                run: ledger_queue_named::ledger_queue_named_clock_with::<S, F>,
                first: 30,
                second: if SMALL { 32 } else { 300 },
            },
        ),
        (
            "ledger queue named with only CI after a future main event",
            histories(ledger_queue_named::ledger_queue_named_ci_only_with::<S, F>),
        ),
        (
            "ledger named queue",
            histories(ledger_queue_lives::ledger_queue_lives_sizes::<S, F>),
        ),
        (
            "ledger named queue past old writer",
            Case::Fixed(ledger_queue_lives_past_an_old_writer::<S, F>),
        ),
        (
            "ledger named queue old snapshot",
            history(ledger_queue_lives::ledger_queue_lives_keeps_an_old_snapshot_with::<S, F>),
        ),
        (
            "ledger report answers as every row",
            report(ledger_report::ledger_report_with::<S, F>),
        ),
        (
            "ledger report past the queue and the clocks of lives shared by the units",
            report(|open, a, b| {
                ledger_report::ledger_report_past_the_queue_and_the_clocks_with(
                    open,
                    QueueAndClocks::SharedLives,
                    a,
                    b,
                )
            }),
        ),
        (
            "ledger report past the queue and the clocks of a life of each unit",
            report(|open, a, b| {
                ledger_report::ledger_report_past_the_queue_and_the_clocks_with(
                    open,
                    QueueAndClocks::UnsharedLives,
                    a,
                    b,
                )
            }),
        ),
        (
            "ledger report past the queue and the clocks of a clock set back",
            Case::Histories {
                run: |open, a, b| {
                    ledger_report::ledger_report_past_the_queue_and_the_clocks_with(
                        open,
                        QueueAndClocks::ClockSetBack,
                        a,
                        b,
                    )
                },
                first: 30,
                second: if SMALL { 32 } else { 150 },
            },
        ),
        (
            "ledger report past the queue and the clocks of the clocks in the range",
            report(|open, a, b| {
                ledger_report::ledger_report_past_the_queue_and_the_clocks_with(
                    open,
                    QueueAndClocks::ClocksInTheRange,
                    a,
                    b,
                )
            }),
        ),
        (
            "ledger report past the queue and the clocks of CI alone after a future main event",
            report(|open, a, b| {
                ledger_report::ledger_report_past_the_queue_and_the_clocks_with(
                    open,
                    QueueAndClocks::CiOnly,
                    a,
                    b,
                )
            }),
        ),
        (
            "ledger report past the queue and the clocks of the range's clocks",
            report(|open, a, b| {
                ledger_report::ledger_report_past_the_queue_and_the_clocks_with(
                    open,
                    QueueAndClocks::RangeClocks,
                    a,
                    b,
                )
            }),
        ),
        (
            "ledger report old snapshot",
            report(ledger_report::ledger_report_keeps_an_old_snapshot_with::<S, F>),
        ),
        (
            "ledger report past a named task's history",
            report(ledger_report::ledger_report_past_a_named_task_s_history_with::<S, F>),
        ),
        (
            "ledger report past a target's ends",
            report(ledger_report::ledger_report_past_a_target_s_ends_with::<S, F>),
        ),
        (
            "kpi compare groups",
            histories(ledger_queue_named::kpi_compare_groups_with::<S, F>),
        ),
        (
            "kpi forecast through the entry",
            histories(ledger_queue_lives::ledger_queue_lives_forecast_with::<S, F>),
        ),
        ("shared spellings", Case::Fixed(shared_spellings::<S, F>)),
        ("large ids", Case::Fixed(large_ids::<S, F>)),
        (
            "source search writes",
            Case::Fixed(source_search_writes::<S, F>),
        ),
        ("search", Case::Fixed(search_cases::<S, F>)),
        ("related", Case::Fixed(related_cases::<S, F>)),
        ("short non-ASCII", Case::Fixed(short_non_ascii::<S, F>)),
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
        (
            "ledger earlier past an old writer",
            Case::Fixed(|open| {
                ledger_earlier::ledger_earlier_past_an_old_writer_of(open, Checks::WritesAlone)
            }),
        ),
        (
            "ledger queue named past an old writer",
            Case::Fixed(|open| {
                ledger_queue_named::ledger_queue_named_past_an_old_writer_of(
                    open,
                    Checks::WritesAlone,
                )
            }),
        ),
        (
            "ledger named queue past old writer",
            Case::Fixed(|open| {
                ledger_queue_lives::ledger_queue_lives_past_an_old_writer_of(
                    open,
                    Checks::WritesAlone,
                )
            }),
        ),
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
/// `small` selects the rollback-only fixture sizes of [`cases_sized`];
/// false uses [`run`]'s original counts. Committing cases and their replay
/// are unchanged at either size.
pub(crate) fn run_apart<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    apart: &[&str],
    full: Option<&str>,
    state: &dyn Fn() -> Result<String>,
    small: bool,
) -> Result<()> {
    let cases = if small {
        cases_sized::<S, F, true>()
    } else {
        cases::<S, F>()
    };
    sequence_apart(
        &cases,
        apart,
        &writes_alone::<S, F>(),
        full,
        state,
        |name, case| at_case_size(small, apart, name, || case.run(open)),
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
    mut run: impl FnMut(&str, &T) -> Result<()>,
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
            run(name, case).with_context(|| format!("store contract: {name}"))?;
        } else if full == Some(*name) {
            let before = state()?;
            run(name, case).with_context(|| format!("store contract: {name}"))?;
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
    fn the_small_fixture_size_applies_to_apart_cases_and_is_restored() {
        assert_eq!(fixed_history_units(), 30);
        at_case_size(true, &["apart"], "old writer", || {
            assert_eq!(fixed_history_units(), 30);
        });
        at_case_size(false, &["apart"], "apart", || {
            assert_eq!(fixed_history_units(), 30);
        });
        let error: Result<()> = at_case_size(true, &["apart"], "apart", || {
            assert_eq!(fixed_history_units(), 2);
            anyhow::bail!("case error")
        });
        assert!(error.is_err());
        assert_eq!(fixed_history_units(), 30);
        let panic = std::panic::catch_unwind(|| {
            at_case_size(true, &["apart"], "apart", || {
                assert_eq!(fixed_history_units(), 2);
                panic!("case panic");
            });
        });
        assert!(panic.is_err());
        assert_eq!(fixed_history_units(), 30);
    }

    /// Frozen names and numeric fixture arguments, independent of either
    /// backend's case builder and of the production fill constants.
    #[test]
    fn the_case_names_and_sizes_match_the_frozen_contract() {
        use crate::infrastructure::postgres::exec::PostgresRow;
        use crate::infrastructure::postgres::{PostgresSession, PostgresStore};
        use crate::infrastructure::sql_exec::{SqlResult, SqlValue};
        use crate::infrastructure::sqlite::SqliteQueue;

        struct NoConnection;
        impl PostgresSession for NoConnection {
            fn run(&self, _: &str) -> Result<()> {
                panic!("manifest opens no connection")
            }
            fn query(&self, _: &str, _: &[SqlValue]) -> SqlResult<Vec<PostgresRow>> {
                panic!("manifest opens no connection")
            }
            fn execute(&self, _: &str, _: &[SqlValue]) -> SqlResult<u64> {
                panic!("manifest opens no connection")
            }
        }
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Fixture {
            Fixed,
            History(usize),
            Histories(usize, usize),
            Parts(usize, usize),
        }
        use Fixture::*;
        // These are the arguments consumed by Case::run, not shadow
        // metadata alongside a function that could select other sizes.
        fn snapshot<F>(cases: Vec<(&'static str, Case<F>)>) -> Vec<(&'static str, Fixture)> {
            cases
                .into_iter()
                .map(|(name, case)| {
                    (
                        name,
                        match case {
                            Case::Fixed(_) => Fixed,
                            Case::History { units, .. } => History(units),
                            Case::Histories { first, second, .. } => Histories(first, second),
                            Case::Parts { rows, bytes, .. } => Parts(rows, bytes),
                        },
                    )
                })
                .collect()
        }
        const EXPECTED: [(&str, Fixture, Fixture); 75] = [
            ("leader lifecycle", Fixed, Fixed),
            ("leader singleton", Fixed, Fixed),
            ("stale writer", Fixed, Fixed),
            ("unique indexes", Fixed, Fixed),
            ("base types", Fixed, Fixed),
            ("json set field", Fixed, Fixed),
            ("event actors", Fixed, Fixed),
            ("event bodies", Fixed, Fixed),
            ("typed conditions", Fixed, Fixed),
            ("backend parallel", Fixed, Fixed),
            ("members", Fixed, Fixed),
            ("member credentials", Fixed, Fixed),
            ("goal owner records", Fixed, Fixed),
            ("goal owner moves", Fixed, Fixed),
            ("goal owner move undone", Fixed, Fixed),
            ("goal landing marks", Fixed, Fixed),
            ("goal landing marks survive tags", Fixed, Fixed),
            ("goal landing marks change with event", Fixed, Fixed),
            ("goal landing marks change undone", Fixed, Fixed),
            ("delegates", Fixed, Fixed),
            ("delegates refused", Fixed, Fixed),
            ("delegates change undone", Fixed, Fixed),
            ("runner credentials", Fixed, Fixed),
            ("runner leases", Fixed, Fixed),
            ("image digests", Fixed, Fixed),
            ("ledger open written with its rows", Fixed, Fixed),
            (
                "ledger open counts the runs left open",
                Histories(30, 300),
                Histories(2, 4),
            ),
            (
                "ledger open filled in parts",
                Parts(200, 524_288),
                Parts(2, 1024),
            ),
            (
                "ledger open parts bounded by bytes",
                Parts(200, 524_288),
                Parts(2, 1024),
            ),
            ("ledger open kept through generations", Fixed, Fixed),
            ("first executions generations", Fixed, Fixed),
            (
                "first executions parts",
                Parts(200, 524_288),
                Parts(2, 1024),
            ),
            (
                "ledger earlier answers as every row",
                Histories(30, 30),
                Histories(2, 2),
            ),
            ("ledger earlier past an old writer", Fixed, Fixed),
            ("ledger queue views written with their rows", Fixed, Fixed),
            (
                "ledger queue views count what a read names",
                Histories(30, 300),
                Histories(2, 4),
            ),
            (
                "ledger queue views filled in parts",
                Parts(200, 524_288),
                Parts(2, 1024),
            ),
            ("ledger queue views kept through generations", Fixed, Fixed),
            (
                "ledger queue views pair asks whichever comes first",
                Fixed,
                Fixed,
            ),
            ("ledger late provider answers as every row", Fixed, Fixed),
            (
                "ledger range answers as every row",
                Histories(30, 300),
                Histories(2, 4),
            ),
            (
                "ledger queue named answers as every row",
                Histories(30, 300),
                Histories(2, 4),
            ),
            (
                "ledger queue named goes back whole of no mark",
                History(30),
                History(2),
            ),
            (
                "ledger queue named goes back whole of a mark of no generation",
                History(30),
                History(2),
            ),
            (
                "ledger queue named goes back whole of a mark of another generation",
                History(30),
                History(2),
            ),
            (
                "ledger queue named goes back whole of a mark filled part way",
                History(30),
                History(2),
            ),
            (
                "ledger queue named goes back whole of a summary without its record",
                History(30),
                History(2),
            ),
            (
                "ledger queue named goes back whole of a record without ci_by_time",
                History(30),
                History(2),
            ),
            (
                "ledger queue named goes back whole of a record without main_by_time",
                History(30),
                History(2),
            ),
            (
                "ledger queue named goes back whole of no marks of the rows",
                History(30),
                History(2),
            ),
            (
                "ledger queue named goes back whole of no tables",
                History(30),
                History(2),
            ),
            ("ledger queue named past an old writer", Fixed, Fixed),
            (
                "ledger queue named past a clock set back",
                Histories(30, 300),
                Histories(30, 32),
            ),
            (
                "ledger queue named with only CI after a future main event",
                Histories(30, 300),
                Histories(2, 4),
            ),
            ("ledger named queue", Histories(30, 300), Histories(2, 4)),
            ("ledger named queue past old writer", Fixed, Fixed),
            ("ledger named queue old snapshot", History(30), History(2)),
            (
                "ledger report answers as every row",
                Histories(30, 150),
                Histories(2, 4),
            ),
            (
                "ledger report past the queue and the clocks of lives shared by the units",
                Histories(30, 150),
                Histories(2, 4),
            ),
            (
                "ledger report past the queue and the clocks of a life of each unit",
                Histories(30, 150),
                Histories(2, 4),
            ),
            (
                "ledger report past the queue and the clocks of a clock set back",
                Histories(30, 150),
                Histories(30, 32),
            ),
            (
                "ledger report past the queue and the clocks of the clocks in the range",
                Histories(30, 150),
                Histories(2, 4),
            ),
            (
                "ledger report past the queue and the clocks of CI alone after a future main event",
                Histories(30, 150),
                Histories(2, 4),
            ),
            (
                "ledger report past the queue and the clocks of the range's clocks",
                Histories(30, 150),
                Histories(2, 4),
            ),
            (
                "ledger report old snapshot",
                Histories(30, 150),
                Histories(2, 4),
            ),
            (
                "ledger report past a named task's history",
                Histories(30, 150),
                Histories(2, 4),
            ),
            (
                "ledger report past a target's ends",
                Histories(30, 150),
                Histories(2, 4),
            ),
            ("kpi compare groups", Histories(30, 300), Histories(2, 4)),
            (
                "kpi forecast through the entry",
                Histories(30, 300),
                Histories(2, 4),
            ),
            ("shared spellings", Fixed, Fixed),
            ("large ids", Fixed, Fixed),
            ("source search writes", Fixed, Fixed),
            ("search", Fixed, Fixed),
            ("related", Fixed, Fixed),
            ("short non-ASCII", Fixed, Fixed),
        ];
        let full: Vec<_> = EXPECTED
            .iter()
            .map(|(name, full, _)| (*name, *full))
            .collect();
        let small: Vec<_> = EXPECTED
            .iter()
            .map(|(name, _, small)| (*name, *small))
            .collect();
        type SqliteOpen = fn() -> Result<SqliteQueue>;
        type Postgres = PostgresStore<NoConnection>;
        type PostgresOpen = fn() -> Result<Postgres>;
        let sqlite_volume = cases::<SqliteQueue, SqliteOpen>();
        let postgres = cases::<Postgres, PostgresOpen>();
        let sqlite_small = cases_sized::<SqliteQueue, SqliteOpen, true>();
        assert_eq!(sqlite_volume.len(), 75);
        assert_eq!(postgres.len(), 75);
        assert_eq!(sqlite_small.len(), 75);
        assert_eq!(snapshot(sqlite_volume), full, "SQLite volume");
        assert_eq!(snapshot(postgres), full, "Postgres run");
        assert_eq!(snapshot(sqlite_small), small, "SQLite small interference");
    }

    #[test]
    fn case_sizes_are_the_arguments_the_runner_executes() {
        use std::cell::RefCell;
        type Seen = RefCell<Vec<(usize, usize)>>;
        let seen = RefCell::new(Vec::new());
        let cases: [Case<Seen>; 4] = [
            Case::Fixed(|seen| {
                seen.borrow_mut().push((0, 0));
                Ok(())
            }),
            Case::History {
                run: |seen, units| {
                    seen.borrow_mut().push((units, 0));
                    Ok(())
                },
                units: 3,
            },
            Case::Histories {
                run: |seen, first, second| {
                    seen.borrow_mut().push((first, second));
                    Ok(())
                },
                first: 5,
                second: 7,
            },
            Case::Parts {
                run: |seen, rows, bytes| {
                    seen.borrow_mut().push((rows, bytes));
                    Ok(())
                },
                rows: 11,
                bytes: 13,
            },
        ];
        for case in cases {
            case.run(&seen).unwrap();
        }
        assert_eq!(*seen.borrow(), [(0, 0), (3, 0), (5, 7), (11, 13)]);
    }

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
            |_, n| {
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
                |_, _| Ok(()),
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
