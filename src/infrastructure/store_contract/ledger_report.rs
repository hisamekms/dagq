//! The report's read of the ledger ([`crate::application::report::window`])
//! against the read of every row, driven identically by SQLite's memdb and
//! the Postgres CI example (ADR-t3986-1 decision 10): for each caller of it
//! (a report of today and of this week, the backfill the supervisor owes,
//! a report at an event id, the observer's days and weeks, the check of the
//! targets and `report_inputs`' diagram), the KPIs with the forecast's and
//! the targets, the reports with their findings unsettled, the near-term
//! diagram, the goals' summaries and the landed runs' post-landing record
//! are those of every row, and what the store returns does not grow with the
//! settled history before the range: on the histories of the earlier records
//! and the views (Spike 4678, 4755, 4811's (B) to (F)), of the queue rows'
//! waits, lives, retractions, snapshots and run-less asks (Spike 4756), and
//! of the clocks (a clock set back early, CI alone after a main event ahead,
//! a clock turned back and one ahead in the range). A forecast snapshot from
//! before the settled history makes the read grow with the marks of its
//! target's open period alone. (Which queries have no range and read every
//! row is `application::report`'s decision, tested there.)
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use super::ContractStore;
use super::ledger_earlier::{NOW, clear, differences};
use super::ledger_queue_named::{Returned, areas, counted};
use crate::application::kpi::{Host, of_window as kpi_of_window};
use crate::application::report::{self, DiagramRenderer, ReportSetup};
use crate::domain::kpi::config::{KpiSettings, Target};
use crate::domain::kpi::report::DiagramSection;
use crate::domain::kpi::{KpiConfig, KpiQuery, Period};
use crate::domain::ledger::testkit::{DAY, Log, T0};
use crate::domain::ledger::window::{LedgerWindow, WindowQuery};
use crate::domain::ledger::{attributes, findings};
use crate::domain::stats::Cursor;
use crate::domain::{EventId, GoalId, RunId};
use crate::infrastructure::runtime_store::ledger_open_in as view;
use crate::infrastructure::sql_exec::{SqlRead, SqlWrite};
use anyhow::{Result, ensure};
use serde_json::{Value, json};

/// The day before the cases outside the ranges (of `ledger_earlier`'s
/// history), seconds after its first day.
const BEFORE: i64 = 150 * DAY;

/// The settled units of the larger history of the queue's and the clocks'
/// cases and of the old snapshot: as many again as the smaller's five times
/// over shows the reads do not grow (the views' history is compared with
/// 300, and with 3000 on SQLite).
const LARGE: usize = 150;

const HOST: Host = Host {
    utc_offset_secs: 0,
    cores: Some(4),
    rust_toolchain: false,
};

/// The report's settings: a target to check, judged of a sample each.
fn setup() -> ReportSetup {
    ReportSetup {
        root: std::path::PathBuf::new(),
        host: HOST,
        config: KpiConfig::merge(
            Some(&KpiSettings {
                min_samples: Some(1),
                targets: vec![Target {
                    kpi: "run_duration".into(),
                    change: None,
                    area: None,
                    stat: None,
                    min: None,
                    max: Some(100.0),
                }],
                ..KpiSettings::default()
            }),
            None,
        ),
        keep: crate::domain::kpi::report::Keep::default(),
        build: "contract".into(),
        diagram: DiagramRenderer(Arc::new(|_| anyhow::bail!("not drawn"))),
        areas: Some(areas()),
    }
}

fn section() -> DiagramSection {
    DiagramSection {
        tasks: Vec::new(),
        d2_source: false,
        reason: Some("not drawn".into()),
        svg: None,
    }
}

/// Whether `query` is one of a report ([`report::report_query`]).
fn of_a_report(query: &KpiQuery) -> bool {
    *query == report::report_query(query.period, query.at)
}

/// The callers of the report's read and their queries; `at_event` is the
/// event id a report is asked at.
fn callers(at_event: EventId) -> Vec<(&'static str, Vec<KpiQuery>)> {
    let periods = |period, last| KpiQuery {
        period,
        last,
        ..KpiQuery::default()
    };
    let owed: Vec<KpiQuery> = crate::domain::kpi::report::due(NOW * 1000, 0, &HashSet::new())
        .into_iter()
        .map(|(period, _, at)| report::report_query(period, Some(Cursor::Time(at))))
        .collect();
    vec![
        ("report day", vec![report::report_query(Period::Day, None)]),
        (
            "report week",
            vec![report::report_query(Period::Week, None)],
        ),
        ("backfill", owed),
        (
            "report at an event",
            vec![report::report_query(
                Period::Day,
                Some(Cursor::Event(at_event)),
            )],
        ),
        (
            "observer",
            vec![periods(Period::Day, 7), periods(Period::Week, 4)],
        ),
        (
            "targets",
            vec![periods(Period::Day, 2), periods(Period::Week, 2)],
        ),
    ]
}

/// The event a report is asked at: the last of the day before now.
fn at_event(tx: &impl SqlRead) -> Result<EventId> {
    let before = crate::domain::marks::utc_text((NOW - DAY) * 1000);
    let id: Option<i64> = tx.query_row(
        "SELECT MAX(id) FROM run_events WHERE created_at <= ?1",
        crate::infrastructure::sql_exec::sql_params![before],
        |r| r.get(0),
    )?;
    id.map(EventId::new)
        .ok_or_else(|| anyhow::anyhow!("no event before the day before now"))
}

/// What a window answers the queries of a caller: each one's KPIs (the
/// forecast's and the targets' with them), each report's (with its findings
/// unsettled), and of the window's end the findings, the near-term diagram,
/// the summaries of the goals its tasks are in and the post-landing record
/// of the runs `landed`.
fn answers(window: &LedgerWindow, queries: &[KpiQuery], landed: &[RunId]) -> Result<Value> {
    let setup = setup();
    let mut kpis = Vec::new();
    let mut reports = Vec::new();
    for query in queries {
        kpis.push(serde_json::to_value(kpi_of_window(
            window,
            NOW,
            setup.host,
            &setup.config,
            setup.areas.as_ref(),
            query,
        )?)?);
        if of_a_report(query) {
            reports.push(serde_json::to_value(report::make_with(
                window,
                &setup,
                NOW,
                query.period,
                query.at,
                section(),
            )?)?);
        }
    }
    let findings = match window.findings() {
        findings::Folded::Findings(found) => json!(format!("{:?}", found.unsettled())),
        findings::Folded::BeforeBaseline => json!("before the baseline"),
    };
    let goal_summaries = match window.attributes() {
        attributes::Folded::Attributes(known) => {
            let goals: BTreeSet<GoalId> = known
                .tasks
                .values()
                .filter_map(|task| task.goal_id)
                .collect();
            Value::Array(
                known
                    .goal_summaries()
                    .into_iter()
                    .filter(|goal| goals.contains(&goal.id))
                    .map(|goal| json!(format!("{goal:?}")))
                    .collect(),
            )
        }
        attributes::Folded::BeforeBaseline => json!("before the baseline"),
    };
    let book = window.post_landing_book();
    let post_landing: Vec<Value> = landed
        .iter()
        .map(|run| json!(format!("{run}: {:?}", book.of_run(run))))
        .collect();
    Ok(json!({
        "kpis": kpis,
        "reports": reports,
        "findings": findings,
        "diagram": serde_json::to_value(report::diagram_draft(window))?,
        "goal_summaries": goal_summaries,
        "post_landing": post_landing,
        "ci_records": {
            "main": book.ci_record("main"),
            "release": book.ci_record("release"),
        },
    }))
}

/// The window of every row with the bounds of `queries` declared, as the
/// report read before its range.
fn whole(tx: &impl SqlRead, queries: &[KpiQuery]) -> Result<LedgerWindow> {
    let setup = setup();
    view::window(
        tx,
        &WindowQuery {
            now_ms: crate::application::stats::now_ms(NOW),
            bounds: crate::application::kpi::declared(
                queries.to_vec(),
                NOW,
                setup.host,
                &setup.config,
                setup.areas.as_ref(),
            ),
            ..WindowQuery::default()
        },
    )
}

/// The runs the window of every row has landed in the last three days,
/// within the range of every caller.
fn landed_runs(whole: &LedgerWindow) -> Result<Vec<RunId>> {
    let from = crate::domain::marks::utc_text((NOW - 3 * DAY) * 1000);
    let runs: BTreeSet<RunId> = whole
        .events()
        .iter()
        .filter(|e| e.kind.as_str() == "run_integrated" && e.created_at > from)
        .filter_map(|e| e.run_id.clone())
        .collect();
    ensure!(!runs.is_empty(), "no run landed in the range");
    Ok(runs.into_iter().collect())
}

/// The report's read of `queries` of `tx`, with what the store returned.
fn read(tx: &impl SqlRead, queries: &[KpiQuery]) -> Result<(LedgerWindow, Returned)> {
    let setup = setup();
    counted(tx, |snapshot| {
        report::window_in(snapshot, NOW, &setup, queries, 0)
    })
}

/// Whether each caller's read on `tx` answers as every row does, with what
/// each returned, in the order of [`callers`], then the read of
/// `report_inputs`' diagram ([`report::diagram_window`]'s query).
fn as_every_row(tx: &impl SqlRead) -> Result<Vec<Returned>> {
    // One read of every row with the bounds of every caller declared:
    // each caller's answers take its own.
    let asked = callers(at_event(tx)?);
    let all: Vec<KpiQuery> = asked
        .iter()
        .flat_map(|(_, queries)| queries.clone())
        .collect();
    let every = whole(tx, &all)?;
    let landed = landed_runs(&every)?;
    let mut returned = Vec::new();
    for (caller, queries) in asked {
        let expected = answers(&every, &queries, &landed)?;
        let (window, read) = read(tx, &queries)?;
        let got = answers(&window, &queries, &landed)?;
        let mut misses = Vec::new();
        differences(&got, &expected, caller, &mut misses);
        ensure!(misses.is_empty(), "{caller}: {misses:?}");
        returned.push(read);
    }
    // `report_inputs`' diagram: a fold of the window's end, read with the
    // setup and the query `report::diagram_window` reads it with.
    let (window, read) = counted(tx, |snapshot| {
        report::window_in(
            snapshot,
            NOW,
            &report::diagram_setup(),
            &report::diagram_queries(),
            0,
        )
    })?;
    ensure!(
        window.answers().is_some(),
        "the diagram's window declares its query's bounds"
    );
    ensure!(
        report::diagram_draft(&window) == report::diagram_draft(&every),
        "report_inputs' diagram"
    );
    returned.push(read);
    Ok(returned)
}

/// How a history is laid down: with the views of the queue rows filled
/// ([`super::ledger_queue_named::lay_down_named`]), with the queue's lives
/// filled ([`super::ledger_queue_lives::lay_down`]) or as the range's.
#[derive(Clone, Copy)]
enum Fixture {
    Named(fn(usize) -> Log),
    Lives(fn(usize) -> Log),
    Range,
}

impl Fixture {
    fn lay_down(self, tx: &impl SqlWrite, settled: usize) -> Result<super::ledger_earlier::Laid> {
        match self {
            Self::Named(history) => {
                super::ledger_queue_named::lay_down_named(tx, &history(settled))
            }
            Self::Lives(history) => {
                let laid = super::ledger_queue_lives::lay_down(tx, &history(settled))?;
                super::ledger_queue_lives::fill(tx)?;
                Ok(laid)
            }
            Self::Range => {
                super::ledger_earlier::lay_down(tx, &super::ledger_range::range_history(settled))
            }
        }
    }

    fn clear(self, tx: &impl SqlWrite, laid: &super::ledger_earlier::Laid) -> Result<()> {
        clear(tx, laid)?;
        if matches!(self, Self::Lives(_)) {
            super::ledger_queue_lives::reset(tx)?;
        }
        Ok(())
    }
}

/// The callers on `fixture`'s history with `small` and with `large`
/// settled units: the answers of every row, and each caller's read returns
/// the same of both (`same` of its index in [`callers`] and the two
/// [`Returned`]). The units are spread over half their span, so that they
/// are before the weeks a report of this week reads too (its `--last 7`
/// and the streaks before them).
fn compare<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    small: usize,
    large: usize,
    fixture: Fixture,
    same: impl Fn(usize, &Returned, &Returned) -> bool,
) -> Result<Vec<Vec<Returned>>> {
    let mut reads = Vec::new();
    for settled in [small, large] {
        let store = open()?;
        let tx = store.write_entry("contract_ledger_report")?;
        let laid =
            super::ledger_earlier::with_units_nearer(1, 2, || fixture.lay_down(&tx, settled))?;
        reads.push(as_every_row(&tx)?);
        fixture.clear(&tx, &laid)?;
        drop(tx);
    }
    for (n, (a, b)) in reads[0].iter().zip(&reads[1]).enumerate() {
        ensure!(
            same(n, a, b),
            "[{n}] returns {a:?} of {small} units and {b:?} of {large}"
        );
    }
    Ok(reads)
}

fn equal(_: usize, a: &Returned, b: &Returned) -> bool {
    a == b
}

/// [`super::ledger_queue_named::named_history`] with the clocks' cases
/// in the range written last: a run landed in the range whose claim (of
/// another provider than the run before it, a derived mark) bears a time
/// turned back before every range, a mark a CLI clock ahead of the DB's
/// wrote after now for a time in the range, and, just before the read, a
/// clock turned back two days (a mark and a supervisor's life).
fn clocks_in_the_range(settled: usize) -> Log {
    let mut log = super::ledger_queue_named::named_history(settled);
    let now = NOW - crate::domain::ledger::testkit::T0;
    let run = super::ledger_earlier::run(91);
    log.landed(91, &run, now - DAY - 3000);
    let claim = log
        .events
        .iter_mut()
        .rev()
        .find(|e| e.kind.as_str() == "run_claimed")
        .expect("the run's claim");
    claim.payload["provider"] = json!("codex");
    claim.created_at = crate::domain::marks::utc_text((NOW - 40 * DAY) * 1000);
    log.add(
        None,
        None,
        "mark_recorded",
        json!({"label": "ahead", "note": null, "by": "human",
               "at": crate::domain::marks::utc_text((NOW - 3 * 3600) * 1000)}),
        now + 2 * 3600,
    )
    .add(
        None,
        None,
        "mark_recorded",
        json!({"label": "turned back", "note": null, "at": null, "by": "human"}),
        now - 2 * DAY,
    )
    .add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s1"}),
        now - 2 * DAY + 1,
    );
    log
}

/// [`super::ledger_queue_named::named_history_with`] of 30 settled units
/// and `changes` changes of the plan of task 66, a task of proposal 8 that
/// a task of the range names, with task 66's history (its changes, its run
/// landed and its turns) moved to its second day: before the range of every
/// caller, the weeks of this week's report, the backfill's and the
/// observer's too (Spike 4755's history of a named task of its own before
/// the range), and before the history's forecast snapshot of its third day,
/// whose second step reads the plan changes from it to the range (the one
/// exception, [`ledger_report_keeps_an_old_snapshot`]'s).
fn member_changes(changes: usize) -> Log {
    let log = super::ledger_queue_named::named_history_with(30, changes);
    let task = crate::domain::TaskId::new(66);
    let run = super::ledger_earlier::run(66);
    let mut events = log.events;
    for e in &mut events {
        let of_66 = e.task_id == Some(task) || e.run_id.as_ref().is_some_and(|r| r.as_str() == run);
        if of_66 && let Some(at) = crate::domain::stats::timestamp_millis(&e.created_at) {
            e.created_at = crate::domain::marks::utc_text(at - (BEFORE - DAY) * 1000);
        }
    }
    events.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    let before = crate::domain::marks::utc_text((T0 + 3 * DAY) * 1000);
    let moved: Vec<&crate::domain::RunEvent> = events
        .iter()
        .filter(|e| e.task_id == Some(task) && e.kind.as_str() == "task_priority_changed")
        .collect();
    assert!(
        moved.len() == changes && moved.iter().all(|e| e.created_at < before),
        "task 66's {changes} changes before every range and snapshot"
    );
    super::ledger_queue_named::renumbered(events)
}

/// The callers on [`member_changes`] with 30 and [`LARGE`] changes of the
/// named task's plan before every range: the answers of every row and as
/// many returned of both, for every caller.
pub fn ledger_report_past_a_named_task_s_history<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let reads = compare(open, 30, LARGE, Fixture::Named(member_changes), equal)?;
    ensure!(
        reads[0].iter().all(|read| read.views),
        "every caller reads by the views: {:?}",
        reads[0]
    );
    Ok(())
}

/// The callers on [`super::ledger_queue_lives::ends_log`] with 30 and
/// [`LARGE`] units: a task closed and reopened in each unit before the
/// range, with a snapshot of it before each close, and closed again in the
/// range; a goal forecast alone (no task) before the range, another
/// snapshot after it, and the goal's first close in the range. The answers
/// are every row's, the forecast's samples of that task and that goal among
/// them, and each caller returns as many of both, its earlier records too:
/// the snapshots of the periods the task ended before are not read, nor the
/// marks from them, nor the task's own records again (its row, read from
/// the time, holds them).
pub fn ledger_report_past_a_target_s_ends<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    past_a_target_s_ends(
        open,
        Fixture::Lives(super::ledger_queue_lives::ends_log),
        30,
        LARGE,
    )
}

/// [`ledger_report_past_a_target_s_ends`] of
/// [`super::ledger_queue_lives::cycles_log`] with 30 and `large` closes and
/// reopenings of the task before the range, and no other unit.
pub(crate) fn ledger_report_past_a_task_s_cycles<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    large: usize,
) -> Result<()> {
    past_a_target_s_ends(
        open,
        Fixture::Lives(super::ledger_queue_lives::cycles_log),
        30,
        large,
    )
}

/// The callers on `fixture` with `small` and `large` units: as every row
/// answers, as many returned of both, and the task's and the goal's samples
/// of the range scored.
fn past_a_target_s_ends<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    fixture: Fixture,
    small: usize,
    large: usize,
) -> Result<()> {
    use super::ledger_queue_lives::{FORECAST_GOAL, REOPENED_TASK, REVISING_PLANNER};
    compare(open, small, large, fixture, equal)?;
    // The samples the comparison holds: the task's and the goal's of
    // their closes in the range, scored as every row scores them.
    let store = open()?;
    let tx = store.write_entry("contract_ledger_report_ends")?;
    let laid = super::ledger_earlier::with_units_nearer(1, 2, || fixture.lay_down(&tx, small))?;
    let every = whole(&tx, &[report::report_query(Period::Week, None)])?;
    let scoring = crate::domain::forecast::score::score(
        every.events(),
        &crate::domain::marks::marks(every.events(), None, None),
    );
    let near = (NOW - DAY) * 1000;
    for target in [
        crate::domain::forecast::score::Target::Task(crate::domain::TaskId::new(REOPENED_TASK)),
        crate::domain::forecast::score::Target::Goal(GoalId::new(FORECAST_GOAL)),
    ] {
        ensure!(
            scoring
                .samples
                .iter()
                .any(|sample| sample.target == target && sample.finished_ms > near),
            "a sample of {target:?} closed in the range: {:?}",
            scoring.samples
        );
    }
    // The task's row is read, and its proposal's revision before the range,
    // a record of the planner's session row the read leaves out, names that
    // planner: the window holds the session's records as every row does.
    let planner: Vec<EventId> = every
        .events()
        .iter()
        .filter(|e| e.payload["planner_id"] == json!(REVISING_PLANNER))
        .map(|e| e.id)
        .collect();
    ensure!(planner.len() == 3, "the planner's session: {planner:?}");
    let mut held = Vec::new();
    for (caller, queries) in callers(at_event(&tx)?) {
        let (window, _) = read(&tx, &queries)?;
        let task = crate::domain::TaskId::new(REOPENED_TASK);
        if !window
            .events()
            .iter()
            .any(|e| e.task_id == Some(task) && e.kind.as_str() == "task_created")
        {
            continue;
        }
        let ids: HashSet<EventId> = window.events().iter().map(|e| e.id).collect();
        ensure!(
            planner.iter().all(|id| ids.contains(id)),
            "{caller}: the session records of the planner of the task's revision"
        );
        held.push(caller);
    }
    ensure!(
        held.contains(&"report day"),
        "a caller after the session holds the task's row: {held:?}"
    );
    fixture.clear(&tx, &laid)?;
    drop(tx);
    Ok(())
}

fn shared_lives(settled: usize) -> Log {
    super::ledger_queue_lives::log(settled, true, true)
}

fn unshared_lives(settled: usize) -> Log {
    super::ledger_queue_lives::log(settled, false, false)
}

/// The report's callers on the history of the earlier records and of the
/// views (Spike 4678's and 4755's cases, 4811's (B) to (F)) with 30 and 300
/// settled units: the answers of every row and as many returned of both.
pub fn ledger_report_answers_as_every_row<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    ledger_report_with(open, 30, 300)
}

/// [`ledger_report_answers_as_every_row`] with `small` and `large` units.
pub(crate) fn ledger_report_with<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    small: usize,
    large: usize,
) -> Result<()> {
    let reads = compare(
        open,
        small,
        large,
        Fixture::Named(super::ledger_queue_named::named_history),
        equal,
    )?;
    ensure!(
        reads[0].iter().all(|read| read.views),
        "every caller reads by the views: {:?}",
        reads[0]
    );
    Ok(())
}

/// The report's callers on the histories of the queue rows' waits, lives,
/// retractions, snapshots, samples, failures and run-less asks (shared and
/// one life per unit) and of the clocks (a clock set back before the units,
/// CI alone after a main event ahead of now, the range's clock turned back
/// and ahead, and [`clocks_in_the_range`]), with 30 and [`LARGE`] settled units: the answers of every
/// row and as many returned of both (of the clocks' range history, the
/// rows, earlier records and queue rows by range: its queue units are of
/// the kinds read whole).
pub fn ledger_report_past_the_queue_and_the_clocks<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    for history in [shared_lives, unshared_lives] {
        compare(open, 30, LARGE, Fixture::Lives(history), equal)?;
    }
    compare(
        open,
        30,
        LARGE,
        Fixture::Named(super::ledger_queue_named::clock_history),
        equal,
    )?;
    compare(open, 30, LARGE, Fixture::Named(clocks_in_the_range), equal)?;
    // CI's checks alone, written after main's event ahead of now with times
    // before it: the report's event cursor stands behind that event, and
    // its time is that of the events up to the window's end. The checks are
    // a day after the cases, before the days' ranges but inside the weeks of
    // this week's report, the backfill's week and the observer's (their
    // range's history, not the settled history before it): of the others.
    compare(
        open,
        30,
        LARGE,
        Fixture::Named(super::ledger_queue_named::ci_only_history),
        |n, a, b| matches!(n, 1 | 2 | 4) || a == b,
    )?;
    compare(open, 30, LARGE, Fixture::Range, |_, a, b| {
        (a.rows, a.earlier, a.by_range) == (b.rows, b.earlier, b.by_range)
    })?;
    Ok(())
}

/// A forecast snapshot of a task open in the range from before every
/// settled unit: the callers answer as every row with 30 and [`LARGE`] units, and
/// what the read of the KPIs returns grows with the marks of that task's
/// open period alone (each unit's mark and its retraction), not past them
/// (Spike 4756's (ii-a), planner decision A: the second step of the
/// forecast's read).
pub fn ledger_report_keeps_an_old_snapshot<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let (small, large) = (30, LARGE);
    let reads = compare(
        open,
        small,
        large,
        Fixture::Lives(super::ledger_queue_lives::old_snapshot_log),
        |_, _, _| true,
    )?;
    let size = |read: &Returned| read.rows + read.earlier + read.by_range + read.whole + read.small;
    // The callers that read up to now: their periods hold task 2's finish.
    let marks = 2 * (large - small);
    for (n, (a, b)) in reads[0].iter().zip(&reads[1]).enumerate() {
        ensure!(
            size(b) >= size(a) && size(b) - size(a) <= marks,
            "[{n}] grows past the open period's marks ({marks}): {a:?} of {small}, {b:?} of {large}"
        );
    }
    ensure!(
        size(&reads[1][0]) > size(&reads[0][0]),
        "the open period's marks are read: {:?}",
        (reads[0][0], reads[1][0])
    );
    Ok(())
}
