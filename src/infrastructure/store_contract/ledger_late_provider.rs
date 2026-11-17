//! `kpi`'s windows that end before now, read by their range through `kpi`'s
//! entry ([`crate::application::kpi::kpi`]), against the read of every row,
//! on a store whose first-Execution table is ready and holds a provider
//! whose first Execution is after the windows' end E: each window answers
//! as every row does, and neither counts that provider's Executions in the
//! window's tokens (`tokens`, `tokens_per_landing`, `details.tokens`).
//! Driven identically by SQLite's memdb (one test of each window, apart, so
//! that they run in parallel) and the Postgres CI example (the case whole).
use super::ContractStore;
use super::ledger_earlier::{
    OLD_WRITER_RUN, differences, kpi_read, kpi_value, lay_down, run, turn, whole_query,
};
use crate::domain::kpi::{CompareSpec, KpiQuery, Period};
use crate::domain::ledger::testkit::{DAY, Log, READ_AT_DAY, T0, log_of_a_time, numbered};
use crate::domain::stats::{Cursor, timestamp_millis};
use crate::domain::{EventActor, EventId};
use crate::infrastructure::runtime_store::ledger_open_in as view;
use crate::infrastructure::sql_exec::{SqlRead, sql_params};
use anyhow::{Result, ensure};
use serde_json::Value;

/// The end E of the windows: three days before now.
const E: i64 = (READ_AT_DAY - 3) * DAY;
/// The start S of `--since`: two days before E.
const S: i64 = E - 2 * DAY;
/// When the late runs land: after the end of the day that holds E (the
/// period `--at E` lists last) and before now.
const LATE: i64 = E + DAY + DAY / 2;
/// The provider whose first Execution is after E.
const LATE_PROVIDER: &str = "p-late";
/// The provider that has Executions before E and after it.
const KEPT_PROVIDER: &str = "claude";

const fn ms(secs: i64) -> i64 {
    (T0 + secs) * 1000
}

/// A worker's turn of run `n` (task `n`) on `provider` at `secs`.
fn worker_turn(log: &mut Log, n: u8, provider: &str, secs: i64) {
    let r = run(n);
    turn(log, i64::from(n), &r, provider, secs);
    log.events.last_mut().unwrap().actor = Some(EventActor {
        role: "worker".into(),
        id: r,
        requested_by: None,
    });
}

/// [`log_of_a_time`] (days of work, with `claude`'s first Execution long
/// before every window) with run 94 landed in [S, E] on [`KEPT_PROVIDER`],
/// and runs 95 (on [`LATE_PROVIDER`], its first Execution) and 96 (on
/// [`KEPT_PROVIDER`]) landed after E, numbered in time order; and the run
/// whose row [`lay_down`] leaves to an older writer, landed long before.
pub(crate) fn late_provider_history() -> Log {
    let mut log = log_of_a_time();
    log.landed(i64::from(OLD_WRITER_RUN), &run(OLD_WRITER_RUN), 5 * DAY);
    log.landed(94, &run(94), E - DAY / 2);
    worker_turn(&mut log, 94, KEPT_PROVIDER, E - DAY / 2 + 10);
    log.landed(95, &run(95), LATE);
    worker_turn(&mut log, 95, LATE_PROVIDER, LATE + 10);
    log.landed(96, &run(96), LATE + 100);
    worker_turn(&mut log, 96, KEPT_PROVIDER, LATE + 110);
    log.events.sort_by_key(|event| event.created_at.clone());
    numbered(log)
}

/// A window of `kpi` the contract asks, each ending at E or before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LateWindow {
    /// `--at` the time E, the last two days.
    AtTime,
    /// `--at` the event id of the last event of a time at or before E.
    AtEvent,
    /// `--since` S `--until` E, of the times.
    SinceUntilTimes,
    /// `--since` / `--until` of the event ids of S and E.
    SinceUntilEvents,
    /// `--compare` a day before E (each side a day) with `--at` E.
    CompareAt,
}

/// Every window of the contract, in the order the whole case asks them.
pub const LATE_WINDOWS: &[LateWindow] = &[
    LateWindow::AtTime,
    LateWindow::AtEvent,
    LateWindow::SinceUntilTimes,
    LateWindow::SinceUntilEvents,
    LateWindow::CompareAt,
];

/// The question of `kpi` of `window`, its event ids those of `events`.
fn question(window: LateWindow, events: &[crate::domain::RunEvent]) -> KpiQuery {
    let time = |secs: i64| Cursor::Time(ms(secs));
    // The last event by id of a time at or before `secs`.
    let id = |secs: i64| {
        events
            .iter()
            .rfind(|e| timestamp_millis(&e.created_at).is_some_and(|at| at <= ms(secs)))
            .map_or(Cursor::Event(EventId::new(0)), |e| Cursor::Event(e.id))
    };
    let day_two = KpiQuery {
        period: Period::Day,
        last: 2,
        ..KpiQuery::default()
    };
    match window {
        LateWindow::AtTime => KpiQuery {
            at: Some(time(E)),
            ..day_two
        },
        LateWindow::AtEvent => KpiQuery {
            at: Some(id(E)),
            ..day_two
        },
        LateWindow::SinceUntilTimes => KpiQuery {
            since: Some(time(S)),
            until: Some(time(E)),
            ..KpiQuery::default()
        },
        LateWindow::SinceUntilEvents => KpiQuery {
            since: Some(id(S)),
            until: Some(id(E)),
            ..KpiQuery::default()
        },
        LateWindow::CompareAt => KpiQuery {
            at: Some(time(E)),
            compare: Some(CompareSpec::At(time(E - DAY))),
            window_days: 1,
            ..day_two
        },
    }
}

/// The window answers of `kpi` (each period's and each comparison side's):
/// the `tokens` and `tokens_per_landing` KPIs and `details.tokens`.
fn token_answers(kpi: &Value) -> Vec<(String, Value)> {
    let mut answers = Vec::new();
    for (n, period) in kpi["periods"].as_array().into_iter().flatten().enumerate() {
        for name in ["tokens", "tokens_per_landing"] {
            answers.push((
                format!("periods[{n}].kpis.{name}"),
                period["kpis"][name].clone(),
            ));
        }
        let tokens = &period["details"]["tokens"];
        for name in ["by_actor", "recorded_from", "coverage"] {
            answers.push((
                format!("periods[{n}].details.tokens.{name}"),
                tokens[name].clone(),
            ));
        }
    }
    for name in ["tokens", "tokens_per_landing"] {
        answers.push((
            format!("compare.strata.{name}"),
            kpi["compare"]["strata"][name].clone(),
        ));
    }
    answers
}

/// The Executions and tokens `provider` holds in the periods of `kpi`.
fn counted_of(kpi: &Value, provider: &str) -> (i64, i64) {
    let mut counted = (0, 0);
    for period in kpi["periods"].as_array().into_iter().flatten() {
        for actor in period["details"]["tokens"]["by_actor"]
            .as_object()
            .into_iter()
            .flat_map(|actors| actors.values())
        {
            let of = &actor["by_provider"][provider];
            counted.0 += of["executions"].as_i64().unwrap_or(0);
            counted.1 += of["total"].as_i64().unwrap_or(0);
        }
    }
    counted
}

/// Whether `period`'s tokens add up without [`LATE_PROVIDER`]: each actor's
/// Executions and total are those of its other providers, and the window's
/// total and the `tokens` KPI are those of the actors.
fn counts_without_the_late_provider(period: &Value) -> Result<()> {
    let tokens = &period["details"]["tokens"];
    let mut window = (0, 0);
    for (name, actor) in tokens["by_actor"].as_object().into_iter().flatten() {
        let late = &actor["by_provider"][LATE_PROVIDER];
        ensure!(
            late["executions"].as_i64().unwrap_or(0) == 0
                && late["total"].as_i64().unwrap_or(0) == 0,
            "{name} counts {LATE_PROVIDER}: {late}"
        );
        let others = actor["by_provider"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(provider, _)| provider.as_str() != LATE_PROVIDER)
            .fold((0, 0), |(executions, total), (_, of)| {
                (
                    executions + of["executions"].as_i64().unwrap_or(0),
                    total + of["total"].as_i64().unwrap_or(0),
                )
            });
        let own = (
            actor["executions"].as_i64().unwrap_or(0),
            actor["total"].as_i64().unwrap_or(0),
        );
        ensure!(own == others, "{name}: {own:?} of its providers {others:?}");
        window = (window.0 + own.0, window.1 + own.1);
    }
    let own = (
        tokens["executions"].as_i64().unwrap_or(0),
        tokens["total"].as_i64().unwrap_or(0),
    );
    ensure!(own == window, "the window {own:?} of its actors {window:?}");
    let kpi = &period["kpis"]["tokens"]["all"];
    ensure!(
        kpi["value"].as_f64().unwrap_or(0.0) == window.1 as f64,
        "the tokens KPI {kpi} of the window's {window:?}"
    );
    Ok(())
}

/// On a store whose first-Execution table is ready and holds
/// [`LATE_PROVIDER`]'s first Execution after E, `kpi`'s windows that end at
/// E or before (`--at`, `--since` / `--until`, `--compare`) read by their
/// range answer as every row: their tokens, `tokens_per_landing` and
/// `details.tokens`, and their whole answer; and neither counts
/// [`LATE_PROVIDER`]'s Executions in the window's tokens, which the read of
/// every row up to now counts. Whether a window names a provider first seen
/// after its end (with nothing counted) is not asserted.
pub fn ledger_late_provider_answers_as_every_row<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    ledger_late_provider_answers_as_every_row_of(open, LATE_WINDOWS)
}

/// [`ledger_late_provider_answers_as_every_row`] of `windows` alone, on a
/// history laid down for them.
pub fn ledger_late_provider_answers_as_every_row_of<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    windows: &[LateWindow],
) -> Result<()> {
    let store = open()?;
    let tx = store.write_entry("contract_ledger_late_provider")?;
    let laid = lay_down(&tx, &late_provider_history())?;
    ensure!(view::first_held(&tx)?, "the first-Execution table is ready");
    let firsts: Vec<(String, String, i64)> = SqlRead::query_map(
        &tx,
        "SELECT actor, provider, at FROM ledger_first_executions",
        sql_params![],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let first_of = |provider: &str| {
        firsts
            .iter()
            .find(|(actor, p, _)| actor == "worker" && p == provider)
            .map(|(_, _, at)| *at)
    };
    ensure!(
        first_of(LATE_PROVIDER).is_some_and(|at| at > ms(E)),
        "{LATE_PROVIDER}'s first Execution after E: {firsts:?}"
    );
    ensure!(
        first_of(KEPT_PROVIDER).is_some_and(|at| at <= ms(E)),
        "{KEPT_PROVIDER}'s first Execution at or before E: {firsts:?}"
    );
    let whole = view::window(&tx, &whole_query())?;
    // The read of every row up to now counts the late provider.
    let to_now = serde_json::to_value(whole.execution_tokens())?;
    let late = &to_now["by_actor"]["worker"]["by_provider"][LATE_PROVIDER];
    ensure!(
        late["executions"].as_i64().unwrap_or(0) > 0,
        "every row up to now counts {LATE_PROVIDER}: {late}"
    );
    for window in windows {
        let n = format!("{window:?}");
        let query = &question(*window, whole.events());
        let expected = kpi_value(
            &whole,
            &KpiQuery {
                read_whole_ledger: true,
                ..query.clone()
            },
        )?;
        let got = kpi_read(&tx, query)?;
        let mut misses = Vec::new();
        for ((path, a), (_, b)) in token_answers(&got).iter().zip(token_answers(&expected)) {
            differences(a, &b, &format!("kpi[{n}].{path}"), &mut misses);
        }
        ensure!(misses.is_empty(), "{query:?}: tokens {misses:?}");
        let last = &got["periods"][got["periods"].as_array().map_or(0, Vec::len).max(1) - 1];
        ensure!(
            !last["kpis"]["tokens"].is_null() && last["details"]["tokens"]["by_actor"].is_object(),
            "kpi[{n}]'s last period has tokens: {}",
            last["kpis"]["tokens"]
        );
        differences(&got, &expected, &format!("kpi[{n}]"), &mut misses);
        ensure!(misses.is_empty(), "{query:?}: {misses:?}");
        for (answer, of) in [(&expected, "every row"), (&got, "the range")] {
            ensure!(
                counted_of(answer, KEPT_PROVIDER).0 > 0,
                "kpi[{n}] of {of} counts {KEPT_PROVIDER}: {:?}",
                counted_of(answer, KEPT_PROVIDER)
            );
        }
        for (answer, of) in [(&expected, "every row"), (&got, "the range")] {
            for (k, period) in answer["periods"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                counts_without_the_late_provider(period)
                    .map_err(|e| anyhow::anyhow!("kpi[{n}] of {of}, period {k}: {e}"))?;
            }
        }
        if query.compare.is_some() {
            ensure!(
                !got["compare"]["strata"]["tokens"].is_null(),
                "kpi[{n}] compares the tokens"
            );
        }
    }
    super::ledger_earlier::clear(&tx, &laid)
}
