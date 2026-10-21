//! The parallel run of stage 2 (ADR-t1662-1 L3 and D7,
//! docs/design/measurement.md "並走の比較と照合"): what `stats --ledger`
//! puts side by side, pure. Today's `stats` values against those the
//! ledger makes of the same meaning, each difference in seconds with the
//! reasons it falls under ([`run`], [`fields`], [`host`]), and the ledger
//! against its inputs: what ended in a window, the rows it has of it, and
//! whether each row is folded up to what its inputs reach now
//! ([`reconcile`]). Stage 3 makes today's values of the ledger and drops
//! this read.
//!
//! The ledger's values are those of [`read`]: `wait_to_land` up to
//! `run_integrated`, as `stats` counts it, and the push after it on its
//! own ([`RunComparison::push`]), which today's `wait_to_land` has none
//! of. The fields of no run or task (`jobs`, `sessions`, ...) are today's
//! function of `stats` over the events the ledger's rows give back
//! ([`super::window::LedgerWindow::events`]), against the same function
//! over the EventStore; `sessions` lists besides each span whose active
//! seconds differ ([`spans`]). The host's load is the summary of the node rows'
//! samples against the summary of today's CSV files, both the
//! summary `stats`' `host` and `kpi`'s `host`, `cpu_per_landing`,
//! `load_per_core` and `health.disk` are made of.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

use super::builder::pending_push;
use super::read::read;
use super::{LEDGER_V, LedgerKind, LedgerRow};
use crate::domain::host_metrics::{self, HostSample, HostSummary, NodeSample};
use crate::domain::stats::landing::PHASES;
use crate::domain::stats::sessions::SpanInWindow;
use crate::domain::stats::{RunStats, timestamp_millis};
use crate::domain::{EventId, RunEvent, TaskId};

/// How far apart (seconds) two values may be for where an interval starts
/// or ends (a second cut differently, an event a moment apart) to account
/// for it.
pub const BOUNDARY_SECS: i64 = 2;

/// Why a value of a run differs between today's `stats` and the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// The ledger has no row of the run (not written yet).
    NoRow,
    /// A legacy row: its values are those `stats` gave when it was
    /// written, so `stats` has changed since.
    Legacy,
    /// Today's `exit` held a wait for a person's answer the ledger has as
    /// `ask` (`LandClock` forgets an open ask once another phase's event
    /// comes).
    ExitHeldAsk,
    /// The row has time no record accounts for, which today's names read
    /// as `exit` (or as `work` before its first record).
    Unattributed,
    /// The two cut an interval's start or end apart by at most
    /// [`BOUNDARY_SECS`].
    Boundary,
    /// None of these.
    Unexplained,
}

/// One value of a run, today's and the ledger's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diff {
    pub field: String,
    pub stats: Option<i64>,
    pub ledger: Option<i64>,
    /// `ledger - stats`, none when either is none.
    pub diff: Option<i64>,
    /// Empty when they agree.
    pub reasons: Vec<Reason>,
}

/// One run of today's `stats` beside its ledger row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunComparison {
    pub run_id: String,
    pub task_id: TaskId,
    /// Whether the ledger has a row of it, and whether that is legacy.
    pub row: bool,
    pub legacy: bool,
    /// `work`, `wait_to_land`, `startup` and `land_phases.<phase>`.
    pub fields: Vec<Diff>,
    /// The ledger's push after `run_integrated` (seconds), which today's
    /// `wait_to_land` does not hold.
    pub push: Option<i64>,
    /// The row's time no record accounts for (seconds).
    pub unattributed: Option<i64>,
}

impl RunComparison {
    /// Whether every value agrees.
    pub fn agrees(&self) -> bool {
        self.fields.iter().all(|diff| diff.reasons.is_empty())
    }
}

/// The value of `land_phases.<phase>` of `phases`.
fn phase(phases: Option<&crate::domain::stats::landing::LandPhases>, name: &str) -> Option<i64> {
    let index = PHASES.iter().position(|phase| *phase == name)?;
    phases.map(|phases| phases.secs[index])
}

/// Today's `stats` of a run beside its ledger row `row` (none when the
/// ledger has none).
pub fn run(stats: &RunStats, row: Option<&LedgerRow>) -> RunComparison {
    let values = row.map(read);
    let ledger_phases = values.as_ref().and_then(|v| v.land_phases.as_ref());
    let stats_phases = stats.land_phases.as_ref();
    let mut pairs: Vec<(String, Option<i64>, Option<i64>)> = vec![
        (
            "work".to_owned(),
            stats.work,
            values.as_ref().and_then(|v| v.work),
        ),
        (
            "wait_to_land".to_owned(),
            stats.wait_to_land,
            values.as_ref().and_then(|v| v.wait_to_land),
        ),
        (
            "startup".to_owned(),
            stats.startup,
            values.as_ref().and_then(|v| v.startup),
        ),
    ];
    for name in PHASES {
        pairs.push((
            format!("land_phases.{name}"),
            phase(stats_phases, name),
            phase(ledger_phases, name),
        ));
    }
    let unattributed = row
        .filter(|row| !row.is_legacy())
        .map(|row| row.totals.unattributed_ms / 1000);
    let held_ask = {
        let exit = |p| phase(p, "exit").unwrap_or(0);
        let ask = |p| phase(p, "ask").unwrap_or(0);
        exit(stats_phases) - exit(ledger_phases) > BOUNDARY_SECS
            && ask(ledger_phases) - ask(stats_phases) > BOUNDARY_SECS
    };
    let fields = pairs
        .into_iter()
        .map(|(field, today, ledger)| {
            let diff = today.zip(ledger).map(|(today, ledger)| ledger - today);
            let mut reasons = Vec::new();
            if today != ledger {
                match row {
                    None => reasons.push(Reason::NoRow),
                    Some(row) if row.is_legacy() => reasons.push(Reason::Legacy),
                    Some(_) => {
                        if held_ask
                            && matches!(field.as_str(), "land_phases.exit" | "land_phases.ask")
                        {
                            reasons.push(Reason::ExitHeldAsk);
                        }
                        if unattributed.is_some_and(|secs| secs > 0)
                            && matches!(
                                field.as_str(),
                                "land_phases.exit" | "work" | "wait_to_land"
                            )
                        {
                            reasons.push(Reason::Unattributed);
                        }
                        if diff.is_some_and(|diff| diff.abs() <= BOUNDARY_SECS) {
                            reasons.push(Reason::Boundary);
                        }
                        if reasons.is_empty() {
                            reasons.push(Reason::Unexplained);
                        }
                    }
                }
            }
            Diff {
                field,
                stats: today,
                ledger,
                diff,
                reasons,
            }
        })
        .collect();
    RunComparison {
        run_id: stats.run_id.to_string(),
        task_id: stats.task_id,
        row: row.is_some(),
        legacy: row.is_some_and(LedgerRow::is_legacy),
        fields,
        push: ledger_phases.and_then(|phases| phases.push),
        unattributed,
    }
}

/// Why a field of no run or task differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldReason {
    /// Session or queue rows of the window are missing: their events are
    /// not among the rows' records yet.
    RowsMissing,
    /// Session or queue rows of the window are dirty: an event came after
    /// what they were folded of.
    RowsDirty,
    /// Every row is there and up to date: the rows give other events than
    /// the EventStore (an event of no row, a row's records cut).
    Records,
    /// Of `sessions`' active time: a span open at the window's end has
    /// turns its events record after the end, of time in the window
    /// (a turn is recorded after it ends, so one running at the end comes
    /// after it). Today's `stats`
    /// reads every event of the EventStore and counts them; the ledger's
    /// window gives back the events to its end only
    /// ([`super::window::LedgerWindow::events`]), so it has fewer turns
    /// of the span, or none and leaves it out of `active.count`. Each
    /// such span is a [`SpanDiff`] of [`SpanReason::TurnsAfterEnd`]; a
    /// window that ends now has no event after it.
    TurnsAfterEnd,
}

/// Why a span's active seconds differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanReason {
    /// Open at the window's end in today's `stats`, and the ledger's
    /// active is less (or none) and that of today's turns recorded by the
    /// window's last event ([`SpanInWindow::active_through_end`]): the
    /// turns recorded after the end make the whole difference
    /// ([`FieldReason::TurnsAfterEnd`]).
    TurnsAfterEnd,
    /// None of these: a span on one side only, one closed in the window,
    /// one with no less active time in the ledger's, or one whose ledger
    /// lacks turns recorded by the window's end too (a row not folded up
    /// to them).
    Unexplained,
}

/// One span whose active seconds in the window differ between today's
/// `stats` and the ledger's rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpanDiff {
    /// The span's `session_opened` event id.
    pub session: EventId,
    pub kind: String,
    pub run_id: Option<String>,
    /// Its active seconds in the window, none when that side has no
    /// active time of it (or no span).
    pub stats: Option<i64>,
    pub ledger: Option<i64>,
    /// `ledger - stats`, none when either is none.
    pub diff: Option<i64>,
    pub reason: SpanReason,
}

/// The spans of `today` (today's `stats`' `sessions`) and `ledger` (the
/// same of the ledger's rows) whose active seconds differ or that one side
/// has alone, by their `session_opened`.
pub fn spans(today: &[SpanInWindow], ledger: &[SpanInWindow]) -> Vec<SpanDiff> {
    let by_id = |spans: &[SpanInWindow]| -> BTreeMap<EventId, SpanInWindow> {
        spans
            .iter()
            .map(|span| (span.opened, span.clone()))
            .collect()
    };
    let (today, ledger) = (by_id(today), by_id(ledger));
    let ids: BTreeSet<EventId> = today.keys().chain(ledger.keys()).copied().collect();
    ids.into_iter()
        .filter_map(|id| {
            let (a, b) = (today.get(&id), ledger.get(&id));
            let (stats, ledger) = (a.and_then(|a| a.active), b.and_then(|b| b.active));
            if a.is_some() && b.is_some() && stats == ledger {
                return None;
            }
            let span = a.or(b)?;
            let after_end = a.is_some_and(|a| {
                !a.closed_in_window && ledger < stats && ledger == a.active_through_end
            });
            let reason = if b.is_some() && after_end {
                SpanReason::TurnsAfterEnd
            } else {
                SpanReason::Unexplained
            };
            Some(SpanDiff {
                session: id,
                kind: span.kind.clone(),
                run_id: span.run_id.as_ref().map(ToString::to_string),
                stats,
                ledger,
                diff: stats.zip(ledger).map(|(stats, ledger)| ledger - stats),
                reason,
            })
        })
        .collect()
}

/// Whether a value of `sessions` at `path` is of the spans' active time.
fn of_active(path: &str) -> bool {
    path.split('.')
        .any(|part| matches!(part, "active" | "active_ratio"))
}

/// A value of a field that differs, by its path in the field's JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LeafDiff {
    pub path: String,
    pub stats: Value,
    pub ledger: Value,
    /// `ledger - stats` for two numbers.
    pub diff: Option<f64>,
}

/// One field of no run or task, today's and of the ledger's rows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldComparison {
    pub field: String,
    pub equal: bool,
    /// The values that differ, the first [`MAX_LEAVES`].
    pub differences: Vec<LeafDiff>,
    /// How many differ in all.
    pub differing: usize,
    pub reasons: Vec<FieldReason>,
    /// Of `sessions`: each span whose active seconds differ ([`spans`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub spans: Vec<SpanDiff>,
}

/// How many differing values of a field are listed.
pub const MAX_LEAVES: usize = 20;

/// The fields of `stats` of no run or task the comparison lists: those
/// the session and queue rows hold the records of.
pub const RUNLESS_FIELDS: [&str; 13] = [
    "jobs",
    "sessions",
    "planner_routes",
    "updates",
    "landing_utilization",
    "claim_holds",
    "landing_holds",
    "claim_deferrals",
    "execution_tokens",
    "backend_failures",
    "reason_codes",
    "auto_repairs",
    "draft_flow",
];

fn leaves(path: &str, today: &Value, ledger: &Value, out: &mut Vec<LeafDiff>) {
    match (today, ledger) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for key in keys {
                let null = Value::Null;
                leaves(
                    &format!("{path}.{key}"),
                    a.get(key).unwrap_or(&null),
                    b.get(key).unwrap_or(&null),
                    out,
                );
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (index, (a, b)) in a.iter().zip(b).enumerate() {
                leaves(&format!("{path}[{index}]"), a, b, out);
            }
        }
        (a, b) if a != b => out.push(LeafDiff {
            path: path.to_owned(),
            stats: a.clone(),
            ledger: b.clone(),
            diff: a.as_f64().zip(b.as_f64()).map(|(a, b)| b - a),
        }),
        _ => {}
    }
}

/// The fields `names` of today's `stats` (`today`, its JSON) beside those
/// of the ledger's rows (`ledger`), with the reasons the window's session
/// and queue rows give (`rows`: their reconciliation). `sessions` takes
/// `span_diffs` ([`spans`]): when every one is
/// [`SpanReason::TurnsAfterEnd`], its values of active time are of
/// [`FieldReason::TurnsAfterEnd`] in place of [`FieldReason::Records`]
/// (missing or dirty rows are named still, for they can cut turns too).
pub fn fields(
    today: &Value,
    ledger: &Value,
    names: &[&str],
    rows: &[KindCount],
    span_diffs: &[SpanDiff],
) -> Vec<FieldComparison> {
    let after_end = !span_diffs.is_empty()
        && span_diffs
            .iter()
            .all(|span| span.reason == SpanReason::TurnsAfterEnd);
    let runless = rows
        .iter()
        .filter(|count| matches!(count.kind, LedgerKind::Session | LedgerKind::Queue));
    let (mut missing, mut dirty) = (false, false);
    for count in runless {
        missing |= !count.missing.is_empty();
        dirty |= !count.dirty.is_empty();
    }
    names
        .iter()
        .map(|name| {
            let null = Value::Null;
            let mut differences = Vec::new();
            leaves(
                name,
                today.get(name).unwrap_or(&null),
                ledger.get(name).unwrap_or(&null),
                &mut differences,
            );
            let differing = differences.len();
            let sessions = *name == "sessions";
            let records =
                !(sessions && after_end) || differences.iter().any(|leaf| !of_active(&leaf.path));
            differences.truncate(MAX_LEAVES);
            let mut reasons = Vec::new();
            if sessions && after_end && differing > 0 {
                reasons.push(FieldReason::TurnsAfterEnd);
            }
            if differing > 0 {
                if missing {
                    reasons.push(FieldReason::RowsMissing);
                }
                if dirty {
                    reasons.push(FieldReason::RowsDirty);
                }
                if !(missing || dirty) && records {
                    reasons.push(FieldReason::Records);
                }
            }
            FieldComparison {
                field: (*name).to_owned(),
                equal: differing == 0,
                differences,
                differing,
                reasons,
                spans: if sessions {
                    span_diffs.to_vec()
                } else {
                    Vec::new()
                },
            }
        })
        .collect()
}

/// Why a sample is in one summary of the host's load and not the other.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SampleDiff {
    /// In both, by the second.
    pub both: usize,
    /// Only in the CSV files, on a day the node rows have samples of: a
    /// row the store kept out (too near another of the same day, as when
    /// two supervisors opened a new day's file at once and each wrote its
    /// header and a row).
    pub csv_only: usize,
    /// Only in the CSV files, on a day the node rows have none of: not
    /// imported into the store, or not folded into rows yet.
    pub import_missing: usize,
    /// Only in the node rows (the files lost it, or were not written).
    pub store_only: usize,
    /// On a day before the oldest the retention keeps: one side removed it
    /// first.
    pub retention: usize,
}

/// One column's statistic of the two summaries that differs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ColumnDiff {
    pub column: String,
    pub stat: &'static str,
    pub csv: Option<f64>,
    pub ledger: Option<f64>,
    pub diff: Option<f64>,
}

/// One window's summary of the host's load from the node rows beside
/// today's from the CSV files.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HostComparison {
    /// `stats` (its window) or a kpi day's label.
    pub window: String,
    /// The unix seconds, both inclusive.
    pub from: i64,
    pub until: i64,
    /// The statistics that differ ([`HostSummary`]'s samples, each
    /// column's min, mean, median, max and p90, the CPU seconds).
    pub columns: Vec<ColumnDiff>,
    pub samples: SampleDiff,
}

fn summary_values(summary: &HostSummary) -> BTreeMap<(String, &'static str), Option<f64>> {
    #[allow(clippy::cast_precision_loss)]
    let count = |n: usize| Some(n as f64);
    let mut values = BTreeMap::new();
    values.insert(("all".to_owned(), "samples"), count(summary.samples));
    for (column, stats) in &summary.metrics {
        let of = |pick: fn(&host_metrics::Summary) -> f64| stats.as_ref().map(pick);
        values.insert(
            ((*column).to_owned(), "samples"),
            stats.as_ref().and_then(|s| count(s.samples)),
        );
        values.insert(((*column).to_owned(), "min"), of(|s| s.min));
        values.insert(((*column).to_owned(), "mean"), of(|s| s.mean));
        values.insert(((*column).to_owned(), "median"), of(|s| s.median));
        values.insert(((*column).to_owned(), "max"), of(|s| s.max));
        values.insert(((*column).to_owned(), "p90"), of(|s| s.p90));
    }
    let cpu = summary.cpu_secs.as_ref();
    values.insert(("cpu_secs".to_owned(), "total"), cpu.map(|c| c.total));
    #[allow(clippy::cast_precision_loss)]
    values.insert(
        ("cpu_secs".to_owned(), "covered_secs"),
        cpu.map(|c| c.covered_secs as f64),
    );
    values
}

/// The window `[from, until]` (unix seconds) named `window` of the host's
/// load: the summary of `csv` (today's files' samples) beside that of
/// `ledger` (the node rows' samples, each with its local day), the
/// statistics that differ, and each sample in one and not the other by
/// why: by its local day at `offset_secs` for a file's, before
/// `oldest_kept` (the retention's oldest day) or not.
pub fn host(
    window: &str,
    (from, until): (i64, i64),
    csv: &[HostSample],
    ledger: &[NodeSample],
    offset_secs: i64,
    oldest_kept: Option<i64>,
) -> HostComparison {
    let csv: Vec<&HostSample> = csv
        .iter()
        .filter(|sample| (from..=until).contains(&sample.unix))
        .collect();
    let ledger: Vec<&NodeSample> = ledger
        .iter()
        .filter(|sample| (from..=until).contains(&sample.sample.unix))
        .collect();
    let a = host_metrics::summarize(
        &csv.iter().map(|s| (*s).clone()).collect::<Vec<_>>(),
        from,
        until,
    );
    let b = host_metrics::summarize(
        &ledger.iter().map(|s| s.sample.clone()).collect::<Vec<_>>(),
        from,
        until,
    );
    let (a, b) = (summary_values(&a), summary_values(&b));
    let keys: BTreeSet<&(String, &'static str)> = a.keys().chain(b.keys()).collect();
    let columns = keys
        .into_iter()
        .filter_map(|key| {
            let (csv, ledger) = (a.get(key).copied().flatten(), b.get(key).copied().flatten());
            let differ = match (csv, ledger) {
                (Some(x), Some(y)) => (x - y).abs() > 1e-9 * x.abs().max(1.0),
                (x, y) => x.is_some() != y.is_some(),
            };
            differ.then(|| ColumnDiff {
                column: key.0.clone(),
                stat: key.1,
                csv,
                ledger,
                diff: csv.zip(ledger).map(|(x, y)| y - x),
            })
        })
        .collect();
    let in_ledger: BTreeSet<i64> = ledger.iter().map(|s| s.sample.unix).collect();
    let ledger_days: BTreeSet<i64> = ledger.iter().map(|s| s.local_day).collect();
    let in_csv: BTreeSet<i64> = csv.iter().map(|s| s.unix).collect();
    let before = |day: i64| oldest_kept.is_some_and(|oldest| day < oldest);
    let mut samples = SampleDiff::default();
    for unix in &in_csv {
        let day = host_metrics::local_day(*unix, offset_secs);
        if in_ledger.contains(unix) {
            samples.both += 1;
        } else if before(day) {
            samples.retention += 1;
        } else if ledger_days.contains(&day) {
            samples.csv_only += 1;
        } else {
            samples.import_missing += 1;
        }
    }
    for sample in &ledger {
        if in_csv.contains(&sample.sample.unix) {
            continue;
        }
        if before(sample.local_day) {
            samples.retention += 1;
        } else {
            samples.store_only += 1;
        }
    }
    HostComparison {
        window: window.to_owned(),
        from,
        until,
        columns,
        samples,
    }
}

/// One row the ledger is to have, as the reconciliation finds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RowState {
    pub kind: LedgerKind,
    pub key: String,
    /// Whether the ledger has it.
    pub row: bool,
    pub is_final: bool,
    /// Pending on its push (a run or task): not settled by rule.
    pub push_pending: bool,
    /// Whether the row is folded up to what its inputs reach now: its
    /// `inputs_through` at the last input of its object (the last event,
    /// the push that closed a pending one, a supervisor's later event, the
    /// last sample of its hour), of this [`LEDGER_V`] (a legacy row's
    /// version aside).
    pub up_to_date: bool,
    /// What its inputs reach now and what the row holds: the last event
    /// id, or for a node row the last NodeSampleStore id.
    pub reach: i64,
    pub folded: Option<i64>,
}

/// One kind's count of what ended in the window against the ledger's
/// rows of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KindCount {
    pub kind: LedgerKind,
    /// What ended in the window: runs and tasks (a pending push counts as
    /// ended), closed sessions and jobs, ended queue intervals and events,
    /// node hours past with samples.
    pub ended: usize,
    /// Those the ledger has a row of.
    pub rows: usize,
    pub missing: Vec<String>,
    /// Rows not settled, a pending push aside.
    pub not_final: Vec<String>,
    pub push_pending: Vec<String>,
    /// Rows not up to date.
    pub dirty: Vec<String>,
}

/// The reconciliation of a window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reconciliation {
    /// The window, unix milliseconds (from none: from the start).
    pub from_ms: Option<i64>,
    pub until_ms: i64,
    pub kinds: Vec<KindCount>,
    /// Each row the ledger is to have, in the order of `kinds`.
    pub rows: Vec<RowState>,
}

/// When `row` (as the inputs fold it now) ended, for the window: its end;
/// a pending push's start of its wait; a legacy row's last input; none
/// while it is on its way.
fn ended_at(row: &LedgerRow, events: &BTreeMap<EventId, i64>) -> Option<i64> {
    if let Some(end) = row.ended_at {
        return Some(end);
    }
    if pending_push(row) {
        return row.intervals.last().map(|last| last.start);
    }
    if row.is_legacy() && matches!(row.kind, LedgerKind::Run | LedgerKind::Task) {
        return events.get(&row.inputs_through.events).copied();
    }
    None
}

/// What the row's inputs reach, and what `stored` holds of them.
fn reach(row: &LedgerRow) -> i64 {
    match row.kind {
        LedgerKind::Node => row.inputs_through.node_samples.unwrap_or(0),
        _ => row.inputs_through.last().as_i64(),
    }
}

/// The reconciliation of the ledger's `stored` rows (every kind) against
/// what the inputs fold to now, `expected` (the rows of
/// [`super::build`] over the EventStore at `now_ms`, and of
/// [`super::node::node_rows`] over the NodeSampleStore): of what ended in
/// `[from_ms, until_ms]` (`events` give a legacy row's time), how many,
/// how many have a row, and per row whether it is there, settled, pending
/// on its push and up to date. A node hour counts once it has passed.
pub fn reconcile(
    expected: &[LedgerRow],
    stored: &[LedgerRow],
    events: &[RunEvent],
    from_ms: Option<i64>,
    until_ms: i64,
    now_ms: i64,
) -> Reconciliation {
    let times: BTreeMap<EventId, i64> = events
        .iter()
        .filter_map(|event| Some((event.id, timestamp_millis(&event.created_at)?)))
        .collect();
    let by_key: BTreeMap<(LedgerKind, &str), &LedgerRow> = stored
        .iter()
        .map(|row| ((row.kind, row.key.as_str()), row))
        .collect();
    let mut kinds: Vec<KindCount> = Vec::new();
    let mut rows = Vec::new();
    for kind in LedgerKind::ALL {
        let mut count = KindCount {
            kind,
            ended: 0,
            rows: 0,
            missing: Vec::new(),
            not_final: Vec::new(),
            push_pending: Vec::new(),
            dirty: Vec::new(),
        };
        for row in expected.iter().filter(|row| row.kind == kind) {
            let Some(end) = ended_at(row, &times) else {
                continue;
            };
            if end > until_ms
                || from_ms.is_some_and(|from| end < from)
                || (kind == LedgerKind::Node && end > now_ms)
            {
                continue;
            }
            count.ended += 1;
            let have = by_key.get(&(kind, row.key.as_str())).copied();
            let pending = pending_push(row);
            let state = RowState {
                kind,
                key: row.key.clone(),
                row: have.is_some(),
                is_final: have.is_some_and(|have| have.is_final),
                push_pending: pending,
                up_to_date: have.is_some_and(|have| {
                    (have.is_legacy() || have.ledger_v == LEDGER_V) && reach(have) >= reach(row)
                }),
                reach: reach(row),
                folded: have.map(reach),
            };
            match have {
                None => count.missing.push(row.key.clone()),
                Some(_) => {
                    count.rows += 1;
                    if pending {
                        count.push_pending.push(row.key.clone());
                    } else if !state.is_final {
                        count.not_final.push(row.key.clone());
                    }
                    if !state.up_to_date {
                        count.dirty.push(row.key.clone());
                    }
                }
            }
            rows.push(state);
        }
        kinds.push(count);
    }
    Reconciliation {
        from_ms,
        until_ms,
        kinds,
        rows,
    }
}

#[cfg(test)]
mod tests;
