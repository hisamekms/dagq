//! `stats --ledger`: the parallel run of stage 2 (ADR-t1662-1 L3 and D7,
//! docs/design/measurement.md "並走の比較と照合"). Today's `stats` of the
//! window beside what the ledger makes of the same meaning, each
//! difference with why ([`crate::domain::ledger::compare`]), and the
//! ledger reconciled with its inputs. Read only; stage 3 drops it once
//! today's values are made of the ledger.

use anyhow::Result;
use serde::Serialize;

use super::stats::{StatsSources, inputs};
use super::{
    CoordinationReads, EventStore, LedgerStore, NodeSampleStore, PlanningRecords, ProcessControl,
    RunReads, SupervisorReads, TaskReads,
};
use crate::domain::host_metrics::{
    DEFAULT_RETENTION_DAYS, HOST_NODE, HostSample, local_day, oldest_kept_day,
};
use crate::domain::kpi::{DAY_MS, Period, cursor_ms};
use crate::domain::ledger::compare::{
    self, FieldComparison, HostComparison, RUNLESS_FIELDS, Reconciliation, RunComparison,
};
use crate::domain::ledger::window::{WindowQuery, select};
use crate::domain::ledger::{LedgerKind, LedgerRow, build, node};
use crate::domain::stats::StatsQuery;

/// How many of `kpi`'s local days (today's last) the host's load is
/// compared over, beside `stats`' window: `kpi`'s default `--last`.
pub const HOST_DAYS: usize = 7;

/// What `stats --ledger` gives.
#[derive(Debug, Serialize)]
pub struct LedgerCheck {
    /// `stats`' window, unix milliseconds.
    pub window_ms: (i64, i64),
    /// Each run `stats` lists, beside its ledger row.
    pub runs: Vec<RunComparison>,
    /// The fields of no run or task.
    pub fields: Vec<FieldComparison>,
    /// The host's load over `stats`' window and over each of `kpi`'s days.
    pub host: Vec<HostComparison>,
    pub reconcile: Reconciliation,
}

/// `stats --ledger` of `queue` at `now` (unix seconds) for `query`, the
/// host's load of today's CSV files read by `csv` (unix seconds, both
/// inclusive).
pub fn ledger_check(
    queue: &(
         impl EventStore
         + PlanningRecords
         + CoordinationReads
         + SupervisorReads
         + RunReads
         + TaskReads
         + LedgerStore
         + NodeSampleStore
         + ?Sized
     ),
    processes: &dyn ProcessControl,
    now: i64,
    query: &StatsQuery,
    sources: &StatsSources<'_>,
    csv: &dyn Fn(i64, i64) -> Result<Vec<HostSample>>,
) -> Result<LedgerCheck> {
    let now_ms = now * 1000;
    let inputs = inputs(queue, processes, now, sources)?;
    let events = &inputs.events;
    let today = inputs.aggregate(events, now, query);
    let mut rows: Vec<LedgerRow> = Vec::new();
    for kind in LedgerKind::ALL {
        rows.extend(queue.ledger_rows(kind)?);
    }
    let window = select(
        rows.clone(),
        &WindowQuery {
            since: query.since,
            until: query.until,
            goal_id: query.goal_id,
            now_ms,
            ..WindowQuery::default()
        },
    );
    let ledger = inputs.aggregate(&window.events(), now, query);
    let run_rows: std::collections::HashMap<&str, &LedgerRow> = rows
        .iter()
        .filter(|row| row.kind == LedgerKind::Run)
        .map(|row| (row.key.as_str(), row))
        .collect();
    let runs = today
        .runs
        .iter()
        .map(|run| compare::run(run, run_rows.get(run.run_id.as_str()).copied()))
        .collect();

    let samples = queue.node_samples(HOST_NODE, 0, now)?;
    let mut expected = build(events, now_ms, "");
    expected.extend(node::node_rows(&samples, ""));
    let from_ms = query.since.and_then(|cursor| cursor_ms(cursor, events));
    let reconcile =
        compare::reconcile(&expected, &rows, events, from_ms, today.window_ms.1, now_ms);
    let fields = compare::fields(
        &serde_json::to_value(&today)?,
        &serde_json::to_value(&ledger)?,
        &RUNLESS_FIELDS,
        &reconcile.kinds,
        &compare::spans(&today.sessions.spans, &ledger.sessions.spans),
    );

    let ledger_samples: Vec<_> = rows
        .iter()
        .filter(|row| row.kind == LedgerKind::Node)
        .flat_map(node::samples)
        .collect();
    let offset = sources.utc_offset_secs;
    let oldest = oldest_kept_day(local_day(now, offset), DEFAULT_RETENTION_DAYS);
    let mut windows = vec![(
        "stats".to_owned(),
        (
            today.window_ms.0.div_euclid(1000),
            today.window_ms.1.div_euclid(1000),
        ),
    )];
    let offset_ms = offset * 1000;
    let mut start = Period::Day.start(now_ms, offset_ms);
    let mut end = now_ms;
    for _ in 0..HOST_DAYS {
        // As `kpi::HostReader::between`: after the period's start, to its end.
        windows.push((
            Period::Day.label(start, offset_ms),
            (start.div_euclid(1000) + 1, end.div_euclid(1000)),
        ));
        end = start;
        start -= DAY_MS;
    }
    let host = windows
        .into_iter()
        .map(|(label, (from, until))| {
            let read = csv(from, until)?;
            Ok(compare::host(
                &label,
                (from, until),
                &read,
                &ledger_samples,
                offset,
                oldest,
            ))
        })
        .collect::<Result<_>>()?;
    Ok(LedgerCheck {
        window_ms: today.window_ms,
        runs,
        fields,
        host,
        reconcile,
    })
}
