//! The set the provider pair trial's groups are compared by (ADR-t4005-1
//! decision 8, ADR-t4118-1 and ADR-t4163-1 for the strata). The groups are the
//! claim's attribute [`provider_trial::GROUP`], and `--by pair_group` splits
//! the runs of a group into four strata ([`strata`], [`Standing`]):
//!
//! - The primary, `pair_group=<group>` ([`PRIMARY`] is its empty prefix): the
//!   runs that ran as their group assigned them ([`ran_as_assigned`]) and
//!   whose failure was not judged the environment's.
//! - The switched, `switched:pair_group=<group>` ([`SWITCHED`]): the runs the
//!   records show moved, the worker or a review job, even if the other side
//!   is unknown. They stay in their assigned group: one that moved on both
//!   sides ran as the other group's pair and is still not in its primary.
//! - The environment's, `environment:pair_group=<group>` ([`ENVIRONMENT`]):
//!   the runs a recovery job judged the environment caused the failure of
//!   ([`FailureCause::Environment`], the ledger row's). A run with any other
//!   cause, or none recorded, stays where the rest of this says.
//! - The secondary, `assigned:pair_group=<group>` ([`ASSIGNED`]): every run of
//!   the group, all of those and the runs that cannot be judged (intention to
//!   treat).
//!
//! A run that cannot be judged has an unknown side and no switch known on the
//! other: it is in the secondary alone unless it is the environment's, counted
//! in no switched run and by its group in [`PairDetail::unrecorded`]. A run
//! with no group stays in `pair_group=unknown`, the attribute's layer like any
//! other's.
//!
//! The environment's cause is an axis apart from the switch: it takes a run
//! out of the primary and adds the environment's stratum, and leaves the rest
//! as it was. So a run the environment caused that ran as assigned is in the
//! environment's and the secondary, one that switched in the environment's,
//! the switched and the secondary, and one that cannot be judged in the
//! environment's and the secondary. The environment's and the switched overlap,
//! and a run that cannot be judged and is not the environment's is in neither,
//! so the counts of the strata need not add up to the secondary's.
//!
//! Read a group's switched runs and those that cannot be judged in its
//! secondary, which holds them all: an environment's stratum's same items count
//! only the runs in it. The share of switched runs is over the secondary's runs
//! less its [`PairDetail::unrecorded`], the runs of the group that can be
//! judged, which the environment's stratum does not change.
//!
//! All get every run KPI, [`super::trial`]'s set (revises, the time with and
//! without a person's answers, the reverts and CI failures after the
//! landings, over the stratum's landed runs) and this one's, over all the
//! stratum's finished runs (landed, failed or interrupted alike: what a run
//! that did not land used and waited for is a cost of its group too):
//!
//! - [`SWITCHED_WORKER`] and [`SWITCHED_REVIEW`]: the runs whose worker
//!   ended on another provider than its claim assigned it
//!   ([`provider_trial::WORKER`] against the facts after the claim), and
//!   those with a review job that moved off its planned provider (the
//!   ledger row's [`ProviderUse::review_switches`]). A run that moved on
//!   both sides is in both counts. The primary holds neither, so they are 0
//!   there.
//! - [`WORKER_TOKENS`] and [`REVIEW_TOKENS`]: per run, the tokens of its
//!   worker's turns (the actors of the run's own sessions) and of its
//!   review's jobs (the `review` actor), from the Executions its ledger row
//!   keeps ([`crate::domain::ledger::window::LedgerWindow::executions`]):
//!   the spread over the runs, with their sum as the value. A run with no
//!   Execution of that side, or one not counted, is no sample: its count
//!   is [`SideDetail::without_tokens`].
//! - [`WORKER_COST`] and [`REVIEW_COST`]: the same in dollars: a Claude
//!   Execution's recorded `cost_usd`, a Codex one's tokens at the
//!   repository's Codex prices ([`super::KpiConfig::codex_prices`],
//!   [`ProviderCost::usd`]). Without the prices a stratum with a Codex
//!   Execution has no value and its reason [`NO_PRICES`]; a stratum none of
//!   whose runs has a cost, [`NO_COST`].
//! - [`LIMIT_WAITS`] and [`LIMIT_WAIT`]: the worker's waits for its
//!   provider for a usage limit (`usage_limit`), their count and per run
//!   their time (a part of `run_duration`, which still holds it); and
//!   [`LIMIT_SWITCHES`], its review's jobs that moved off their provider
//!   for one. Only a run whose row records its waits (for the switches,
//!   its use of providers: one whose review named no job's launch had no
//!   job to move, as [`review_switched`] reads it) is a sample.
//!
//! The counts of the stratum's runs behind them are in
//! `details.provider_pair` ([`PairDetail`]).
//!
//! Reads the run rows, their values and Executions it is given only: no
//! Git, StateStore, run dir or events.

use std::collections::BTreeMap;

use serde::Serialize;

use super::{Measure, RunLayers, float};
use crate::domain::{
    Provider,
    agent_eval::round::ProviderCost,
    ledger::provider_use::ProviderUse,
    provider_trial,
    recovery::FailureCause,
    sessions::{REVIEW, RUN_SESSION},
    stats::executions::Execution,
};

/// The prefix of a primary stratum: none, `pair_group=<group>`.
pub const PRIMARY: &str = "";
/// The prefix of a stratum of switched runs: `switched:pair_group=<group>`.
pub const SWITCHED: &str = "switched:";
/// The prefix of a stratum of the runs the environment caused the failure
/// of: `environment:pair_group=<group>`.
pub const ENVIRONMENT: &str = "environment:";
/// The prefix of a secondary stratum: `assigned:pair_group=<group>`.
pub const ASSIGNED: &str = "assigned:";

pub const SWITCHED_WORKER: &str = "pair.switched_runs.worker";
pub const SWITCHED_REVIEW: &str = "pair.switched_runs.review";
pub const WORKER_TOKENS: &str = "pair.tokens.worker";
pub const REVIEW_TOKENS: &str = "pair.tokens.review";
pub const WORKER_COST: &str = "pair.cost_usd.worker";
pub const REVIEW_COST: &str = "pair.cost_usd.review";
pub const LIMIT_WAITS: &str = "pair.usage_limit.worker_waits";
pub const LIMIT_WAIT: &str = "pair.usage_limit.worker_wait";
pub const LIMIT_SWITCHES: &str = "pair.usage_limit.review_switches";

/// Why a side's cost has no value: a Codex Execution and no Codex prices
/// to convert its tokens at, or no run with a cost.
pub const NO_PRICES: &str = "no_prices";
pub const NO_COST: &str = "no_cost";

/// The reason of a wait or a switch for a usage limit.
const USAGE_LIMIT: &str = "usage_limit";

/// `stratum` without the prefix of a switched, environment's or secondary
/// stratum.
pub fn without_prefix(stratum: &str) -> &str {
    stratum
        .strip_prefix(SWITCHED)
        .or_else(|| stratum.strip_prefix(ENVIRONMENT))
        .or_else(|| stratum.strip_prefix(ASSIGNED))
        .unwrap_or(stratum)
}

/// Whether `stratum` is one this set is computed for: a group of the
/// provider pair trial, in any of its strata.
pub fn is_pair_stratum(stratum: &str) -> bool {
    without_prefix(stratum)
        .strip_prefix(provider_trial::GROUP)
        .is_some_and(|rest| rest.starts_with('='))
}

/// Whether the run's worker ended on another provider than its claim
/// assigned it; none when the claim assigned none or the end is unknown.
pub fn worker_switched(layers: &RunLayers) -> Option<bool> {
    let assigned = layers.attribute(provider_trial::WORKER)?;
    let last = layers.after_claim.last_provider.as_ref()?;
    Some(*last != assigned)
}

/// Whether one of the run's review jobs moved off its planned provider;
/// none when its row has no record of it. A run whose row records the use
/// of providers but no review job's launch had no job to move.
pub fn review_switched(uses: Option<&ProviderUse>) -> Option<bool> {
    let uses = uses?;
    Some(
        uses.review_switches
            .as_ref()
            .is_some_and(|switches| switches.values().sum::<i64>() > 0),
    )
}

/// Whether the run ran as its group assigned it (ADR-t4005-1 decision 7):
/// its worker stayed on the assigned provider and none of its review's
/// jobs moved off the provider planned for it. A job is not judged against
/// the group's review provider, so a review whose jobs ran on different
/// providers as configured ran as assigned.
pub fn ran_as_assigned(layers: &RunLayers, uses: Option<&ProviderUse>) -> bool {
    worker_switched(layers) == Some(false) && review_switched(uses) == Some(false)
}

/// What the records say of a run against its assigned pair (ADR-t4118-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// It ran as assigned ([`ran_as_assigned`]).
    AsAssigned,
    /// Its worker or a review job moved, whatever the other side did.
    Switched,
    /// A side is unknown and no switch is known on the other.
    Unjudged,
}

/// The [`Standing`] of a run.
pub fn standing(layers: &RunLayers, uses: Option<&ProviderUse>) -> Standing {
    if worker_switched(layers) == Some(true) || review_switched(uses) == Some(true) {
        Standing::Switched
    } else if ran_as_assigned(layers, uses) {
        Standing::AsAssigned
    } else {
        Standing::Unjudged
    }
}

/// The strata of a run of the group whose stratum is `group`
/// (`pair_group=<group>`), by its [`standing`] and its failure's `cause`: the
/// secondary, the switched when it switched, and the environment's in place
/// of the primary when the environment caused its failure.
pub fn strata(
    group: &str,
    layers: &RunLayers,
    uses: Option<&ProviderUse>,
    cause: FailureCause,
) -> Vec<String> {
    let standing = standing(layers, uses);
    let environment = cause == FailureCause::Environment;
    let mut strata = vec![format!("{ASSIGNED}{group}")];
    if standing == Standing::Switched {
        strata.push(format!("{SWITCHED}{group}"));
    }
    if environment {
        strata.push(format!("{ENVIRONMENT}{group}"));
    } else if standing == Standing::AsAssigned {
        strata.push(format!("{PRIMARY}{group}"));
    }
    strata
}

/// One finished run of a stratum.
pub struct PairRun<'a> {
    pub layers: &'a RunLayers,
    /// Its row's use of providers ([`ProviderUse`]); none without a row or
    /// a record of it.
    pub uses: Option<&'a ProviderUse>,
    /// The Executions its row keeps.
    pub executions: &'a [&'a Execution],
}

/// The runs of a stratum behind the set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PairDetail {
    /// The finished runs.
    pub runs: usize,
    /// Of them, those that cannot be judged ([`Standing::Unjudged`]). A
    /// secondary stratum's count is the group's; an environment's stratum
    /// holds those of them it has too; the others hold none.
    pub unrecorded: usize,
    pub worker: SideDetail,
    pub review: SideDetail,
}

/// One side (the worker or the review) of a stratum's runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SideDetail {
    /// The runs whose tokens of the side were summed, and those left out:
    /// no Execution of the side, or one not counted.
    pub with_tokens: usize,
    pub without_tokens: usize,
    /// The runs whose cost of the side was summed.
    pub with_cost: usize,
    /// Why the cost has no value: [`NO_PRICES`] or [`NO_COST`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_reason: Option<&'static str>,
}

/// The tokens and the dollars of one side of a run: none when it has no
/// Execution of the side or one was not counted; the dollars none when one
/// has none, and whether that was for the Codex prices.
struct Side {
    tokens: Option<i64>,
    usd: Option<f64>,
    unpriced: bool,
}

fn side(executions: &[&Execution], prices: Option<&ProviderCost>) -> Side {
    let mut side = Side {
        tokens: None,
        usd: None,
        unpriced: false,
    };
    if executions.is_empty() {
        return side;
    }
    let (mut tokens, mut usd, mut priced) = (0, 0.0, true);
    for execution in executions {
        let Some(counts) = execution.tokens else {
            return side;
        };
        tokens += counts.total();
        let cost = if execution.provider == Provider::Codex.as_str() {
            let cost = prices.and_then(|prices| {
                prices.usd(
                    counts.input,
                    counts.cache_read + counts.cache_creation,
                    counts.output,
                )
            });
            side.unpriced |= cost.is_none();
            cost
        } else {
            counts.cost_usd.map(|usd| usd.dollars())
        };
        match cost {
            Some(cost) => usd += cost,
            None => priced = false,
        }
    }
    side.tokens = Some(tokens);
    side.usd = priced.then_some(usd);
    side
}

/// The spread of `values` with their sum as the value.
fn summed(values: Vec<f64>) -> Measure {
    let sum: f64 = values.iter().sum();
    let any = !values.is_empty();
    Measure::spread(values).with_value(any.then_some(sum))
}

/// The set of `runs` (a pair stratum's finished runs), the Codex Executions
/// converted at `prices`.
pub fn kpis(
    runs: &[PairRun<'_>],
    prices: Option<&ProviderCost>,
) -> (Vec<(&'static str, Measure)>, PairDetail) {
    let mut detail = PairDetail {
        runs: runs.len(),
        unrecorded: runs
            .iter()
            .filter(|run| standing(run.layers, run.uses) == Standing::Unjudged)
            .count(),
        ..PairDetail::default()
    };
    let switched_worker = runs
        .iter()
        .filter(|run| worker_switched(run.layers) == Some(true))
        .count();
    let switched_review = runs
        .iter()
        .filter(|run| review_switched(run.uses) == Some(true))
        .count();
    let mut measures = vec![
        (SWITCHED_WORKER, Measure::count(switched_worker)),
        (SWITCHED_REVIEW, Measure::count(switched_review)),
    ];
    let of_side = |review: bool| {
        runs.iter().map(move |run| {
            let executions: Vec<&Execution> = run
                .executions
                .iter()
                .copied()
                .filter(|execution| {
                    if review {
                        execution.actor == REVIEW
                    } else {
                        RUN_SESSION.contains(&execution.actor)
                    }
                })
                .collect();
            side(&executions, prices)
        })
    };
    for (review, tokens_name, cost_name) in [
        (false, WORKER_TOKENS, WORKER_COST),
        (true, REVIEW_TOKENS, REVIEW_COST),
    ] {
        let sides: Vec<Side> = of_side(review).collect();
        let tokens: Vec<f64> = sides
            .iter()
            .filter_map(|side| side.tokens.map(float))
            .collect();
        let costs: Vec<f64> = sides.iter().filter_map(|side| side.usd).collect();
        let unpriced = sides.iter().any(|side| side.unpriced);
        let of = SideDetail {
            with_tokens: tokens.len(),
            without_tokens: sides.len() - tokens.len(),
            with_cost: if unpriced { 0 } else { costs.len() },
            cost_reason: if unpriced {
                Some(NO_PRICES)
            } else if costs.is_empty() && !tokens.is_empty() {
                Some(NO_COST)
            } else {
                None
            },
        };
        measures.push((tokens_name, summed(tokens)));
        // A sum without the Codex runs would read as the stratum's cost.
        measures.push((cost_name, summed(if unpriced { Vec::new() } else { costs })));
        if review {
            detail.review = of;
        } else {
            detail.worker = of;
        }
    }
    let waits: Vec<(i64, i64)> = runs
        .iter()
        .filter_map(|run| run.uses?.worker_waits.as_ref())
        .map(|waits| {
            waits
                .get(USAGE_LIMIT)
                .map_or((0, 0), |waits| (waits.count, waits.millis))
        })
        .collect();
    let count: i64 = waits.iter().map(|(count, _)| count).sum();
    measures.push((
        LIMIT_WAITS,
        Measure::total((!waits.is_empty()).then(|| float(count)), waits.len()),
    ));
    measures.push((
        LIMIT_WAIT,
        Measure::secs(waits.iter().map(|(_, millis)| millis / 1000)),
    ));
    let switches: Vec<i64> = runs
        .iter()
        .filter_map(|run| run.uses)
        .map(|uses| {
            uses.review_switches
                .as_ref()
                .and_then(|switches| switches.get(USAGE_LIMIT).copied())
                .unwrap_or(0)
        })
        .collect();
    measures.push((
        LIMIT_SWITCHES,
        Measure::total(
            (!switches.is_empty()).then(|| float(switches.iter().sum())),
            switches.len(),
        ),
    ));
    (measures, detail)
}

/// The details of every pair stratum, for `details.provider_pair`.
pub type PairDetails = BTreeMap<String, PairDetail>;
