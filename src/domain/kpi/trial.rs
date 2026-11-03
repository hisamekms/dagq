//! The set a model and effort trial's groups are compared by
//! (ADR-t3957-1 decision 5), per stratum of the landed runs like every
//! run KPI, so that `--by group`, `--by model` and `--by effort` read it
//! per group, model and effort. `first_pass_rate` and `revise_rate` are
//! already run KPIs, and `tokens` and `tokens_per_landing` put a run's
//! Executions in its `--by` layers but the tokens' own (`model=` is the
//! Execution's model, not the claim's, and so is `provider=`); this adds:
//!
//! - [`REVISES_PER_LANDING`]: the revises the landed runs were sent back
//!   for, over the landed runs.
//! - [`RUN_DURATION`]: the spread of a landed run's time, claim to the end
//!   of its push, from its ledger row; and [`RUN_DURATION_WITHOUT_HUMAN`],
//!   the same less the time its row's intervals give the `human` blocker
//!   (a person's answer). A run without a row or with a legacy row (no
//!   intervals) is no sample.
//! - [`REVERTS`] and [`CI_FAILURES`]: how many reverts of the landed
//!   commits and turns to red of the landing branch's CI on them arrived;
//!   [`REVERT_RATE`] and [`CI_FAILURE_RATE`]: the share of the landed runs
//!   with at least one. They arrive after the run's row is final, so they
//!   are read from the ledger window's common read
//!   ([`crate::domain::ledger::window::LedgerWindow::post_landing_book`]),
//!   as of that window's end: a later reading counts what arrived since.
//!   Only a run the record covers is a sample ([`Coverage::Recorded`]); a
//!   stratum with landed runs and none covered has no value, and its
//!   [`Arrival`] names why ([`Arrival::reason`]). Next to the values,
//!   [`Arrival`] gives how long after the period's end the window was read
//!   and how far the record reached. `--compare` keeps both on each side
//!   ([`super::compare::WindowSpan::post_landing`] and a side's reason,
//!   [`super::compare::Side::before_missing`]).
//!
//! Reads the run rows and the common read it is given only: no Git,
//! StateStore or run dir.

use serde::Serialize;

use super::{Measure, float};
use crate::domain::{
    RunId,
    ledger::{
        LedgerRow,
        post_landing::{Counted, Coverage, PostLandingBook},
    },
    run_phase::Blocker,
};

pub const REVISES_PER_LANDING: &str = "revises_per_landing";
pub const RUN_DURATION: &str = "run_duration";
pub const RUN_DURATION_WITHOUT_HUMAN: &str = "run_duration_without_human";
pub const REVERTS: &str = "post_landing.reverts";
pub const REVERT_RATE: &str = "post_landing.revert_rate";
pub const CI_FAILURES: &str = "post_landing.ci_failures";
pub const CI_FAILURE_RATE: &str = "post_landing.ci_failure_rate";

/// Why a stratum's late quality has no value: its landed runs landed
/// before the record began, before an import that could not read a line
/// (`incomplete`), or there is no record.
pub const BEFORE_RECORD: &str = "before_record";
pub const INCOMPLETE: &str = "incomplete";
pub const NOT_RECORDED: &str = "not_recorded";

/// One landed run of a stratum.
pub struct Landed<'a> {
    pub run_id: &'a RunId,
    /// The revises it was sent back for.
    pub revises: usize,
    /// Its ledger row, if any.
    pub row: Option<&'a LedgerRow>,
}

/// How the record of one late quality (reverts or CI failures) stands to
/// a stratum's landed runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Arrival {
    /// The landed runs, and how many of them the record covers, landed
    /// before it began, landed before an import with unread lines, or have
    /// no record.
    pub landed: usize,
    pub recorded: usize,
    pub before_record: usize,
    pub incomplete: usize,
    pub not_recorded: usize,
    /// The window's end the values are read at (unix milliseconds).
    pub read_at: i64,
    /// From the period's end to `read_at`, in milliseconds: how long the
    /// period's last landing had for a revert or a failure to arrive.
    pub after_period_ms: i64,
    /// The earliest reach of the record among the covered runs (unix
    /// milliseconds), and how long before `read_at` it was.
    pub reach_at: Option<i64>,
    pub reach_before_end_ms: Option<i64>,
    /// Why there is no value: [`BEFORE_RECORD`], [`INCOMPLETE`] or
    /// [`NOT_RECORDED`]; none with a value or without landed runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

/// The late quality of a stratum: reverts and CI failures.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PostLandingArrival {
    pub reverts: Arrival,
    pub ci_failures: Arrival,
}

impl PostLandingArrival {
    /// Why `kpi`, one of the late quality's, has no value: its record's
    /// [`Arrival::reason`]; none for another KPI.
    pub fn reason(&self, kpi: &str) -> Option<&'static str> {
        match kpi {
            REVERTS | REVERT_RATE => self.reverts.reason,
            CI_FAILURES | CI_FAILURE_RATE => self.ci_failures.reason,
            _ => None,
        }
    }
}

/// The comparison set of `landed` (a stratum's landed runs of a period
/// ending at `period_end_ms`): the measures, and the arrival of the late
/// quality when `book` (the common read) is given.
pub fn kpis(
    landed: &[Landed<'_>],
    book: Option<&PostLandingBook>,
    period_end_ms: i64,
) -> (Vec<(&'static str, Measure)>, Option<PostLandingArrival>) {
    let revises: usize = landed.iter().map(|run| run.revises).sum();
    let human = serde_json::to_value(Blocker::Human)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    let rows: Vec<&LedgerRow> = landed
        .iter()
        .filter_map(|run| run.row.filter(|row| !row.is_legacy()))
        .collect();
    let mut measures = vec![
        (
            REVISES_PER_LANDING,
            Measure::ratio(
                float(i64::try_from(revises).unwrap_or(i64::MAX)),
                landed.len(),
            ),
        ),
        (
            RUN_DURATION,
            Measure::secs(rows.iter().map(|row| row.totals.all.duration_ms / 1000)),
        ),
        (
            RUN_DURATION_WITHOUT_HUMAN,
            Measure::secs(rows.iter().map(|row| {
                let waited = row
                    .totals
                    .by_blocker
                    .get(&human)
                    .map_or(0, |measure| measure.duration_ms);
                (row.totals.all.duration_ms - waited).max(0) / 1000
            })),
        ),
    ];
    let Some(book) = book else {
        return (measures, None);
    };
    let read: Vec<_> = landed.iter().map(|run| book.of_run(run.run_id)).collect();
    let mut late = |counts: Vec<&Counted>, count: &'static str, rate: &'static str| {
        let arrival = arrival(&counts, book.end_ms(), period_end_ms);
        let covered: Vec<&&Counted> = counts
            .iter()
            .filter(|counted| counted.coverage == Coverage::Recorded)
            .collect();
        let total: usize = covered.iter().map(|counted| counted.count).sum();
        let hit = covered.iter().filter(|counted| counted.count > 0).count();
        measures.push((
            count,
            Measure::total(
                (!covered.is_empty()).then(|| float(i64::try_from(total).unwrap_or(i64::MAX))),
                covered.len(),
            ),
        ));
        measures.push((
            rate,
            Measure::ratio(float(i64::try_from(hit).unwrap_or(i64::MAX)), covered.len()),
        ));
        arrival
    };
    let reverts = late(
        read.iter().map(|run| &run.reverts).collect(),
        REVERTS,
        REVERT_RATE,
    );
    let ci_failures = late(
        read.iter().map(|run| &run.ci_failures).collect(),
        CI_FAILURES,
        CI_FAILURE_RATE,
    );
    (
        measures,
        Some(PostLandingArrival {
            reverts,
            ci_failures,
        }),
    )
}

/// How the record stands to `counts`, read at `read_at`.
fn arrival(counts: &[&Counted], read_at: i64, period_end_ms: i64) -> Arrival {
    let of = |coverage: Coverage| {
        counts
            .iter()
            .filter(|counted| counted.coverage == coverage)
            .count()
    };
    let reach_at = counts
        .iter()
        .filter(|counted| counted.coverage == Coverage::Recorded)
        .filter_map(|counted| counted.reach_at)
        .min();
    let mut arrival = Arrival {
        landed: counts.len(),
        recorded: of(Coverage::Recorded),
        before_record: of(Coverage::BeforeRecord),
        incomplete: of(Coverage::Incomplete),
        not_recorded: of(Coverage::NotRecorded) + of(Coverage::NotLanded),
        read_at,
        after_period_ms: (read_at - period_end_ms).max(0),
        reach_at,
        reach_before_end_ms: reach_at.map(|at| (read_at - at).max(0)),
        reason: None,
    };
    if arrival.landed > 0 && arrival.recorded == 0 {
        arrival.reason = Some(if arrival.before_record > 0 {
            BEFORE_RECORD
        } else if arrival.incomplete > 0 {
            INCOMPLETE
        } else {
            NOT_RECORDED
        });
    }
    arrival
}
