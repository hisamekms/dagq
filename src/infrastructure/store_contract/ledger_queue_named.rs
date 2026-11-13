//! The queue rows of main's record, CI's, the goals', the baselines' marks
//! and the findings read by their range with the store's views, what the
//! range names of them before it sought
//! ([`crate::application::LedgerWindowRead::queue_sought`]), against the
//! read of every row, driven identically by SQLite's memdb and the
//! Postgres CI example (ADR-t3986-1 decision 10): the answers of `kpi`,
//! `stats` and the report's diagram are those of every row, what the store
//! returns does not grow with the settled history before the range, and a
//! store whose views cannot be trusted reads those kinds whole again
//! (docs/plans/ledger-window-earlier-visibility-spike.md, "追試 (ii) の残り
//! （Spike 4811）" "判定" (B) to (F)).
use std::cell::Cell;

use super::ContractStore;
use super::ledger_earlier::{
    NOW, clear, differences, fill, history, kpi_questions, lay_down, run, time_query, whole_query,
};
use super::ledger_open::taken_over;
use crate::application::kpi::{Host, of_window as kpi_of_window};
use crate::application::stats::{StatsSources, of_window as stats_of_window};
use crate::application::{LedgerWindowRead, RunFinish, WindowRead, ledger_window_in};
use crate::domain::areas::AreaMap;
use crate::domain::kpi::{KpiConfig, KpiQuery};
use crate::domain::ledger::queue_views::SUMMARY_EVERY;
use crate::domain::ledger::session::written::omitted::{Coverage, Point};
use crate::domain::ledger::testkit::{DAY, Log, READ_AT_DAY, T0, finding_record};
use crate::domain::ledger::window::queue_named::{
    MainBefore, QueueSought, QueueWants, SUMMARIZED_KINDS,
};
use crate::domain::ledger::window::{EarlierRecords, LedgerWindow, RowsFrom, WindowQuery};
use crate::domain::ledger::{LedgerKind, LedgerRow, attributes, findings};
use crate::domain::stats::{Cursor, StatsQuery};
use crate::domain::{EventId, GoalId, RunEvent, RunId};
use crate::infrastructure::runtime_store::{ledger_open_in as view, ledger_views_in as views};
use crate::infrastructure::sql_exec::{SqlRead, SqlWrite, sql_params};
use anyhow::{Result, ensure};
use serde_json::{Value, json};

/// The range's day, and the day before the units' runs end.
const NEAR: i64 = (READ_AT_DAY - 1) * DAY;
const BEFORE: i64 = 150 * DAY;
/// The time `stats --since` reads from (unix seconds after T0).
const SINCE: i64 = (READ_AT_DAY - 2) * DAY;
/// The goal each settled unit changes, named by a task of the range.
const GOAL: i64 = 700;
/// A goal whose last replacement before the range is a baseline whose
/// group's mark is before it too.
const BASELINED_GOAL: i64 = 702;
/// A goal achieved before the range that a task of the range, known by its
/// baseline, waits for.
const DEPENDED_GOAL: i64 = 704;

fn sha(k: usize) -> String {
    format!("{k:040x}")
}

fn goal_record(title: &str, priority: &str) -> Value {
    json!({"title": title, "status": "open", "priority": priority})
}

fn of_goal(log: &mut Log, goal: i64, kind: &str, payload: Value, secs: i64) {
    log.add(None, None, kind, payload, secs);
    log.events.last_mut().unwrap().goal_id = Some(GoalId::new(goal));
}

fn commits(log: &mut Log, after: Option<&str>, commits: Value, secs: i64) {
    log.add(
        None,
        None,
        "main_commits_recorded",
        json!({"after": after, "commits": commits}),
        secs,
    );
}

fn observed(log: &mut Log, head: &str, secs: i64) {
    log.add(
        None,
        None,
        "main_observed",
        json!({"head": head, "at": T0 + secs, "since": T0 + 10 * DAY, "reverts": true}),
        secs,
    );
}

/// The finding `finding`'s change to `to`, its values with it.
fn finding_change(log: &mut Log, finding: i64, from: &str, to: &str, secs: i64) {
    let mut change = finding_record(to);
    change["finding_id"] = json!(finding);
    change[findings::VALUES]["id"] = json!(finding);
    change["from"] = json!(from);
    change["to"] = json!(to);
    log.add(None, None, "finding_status_changed", change, secs);
}

/// The settled units outside the range, each adding a row of every kind
/// read by the views: main's commit, its paths and its reach, CI's check
/// and turn to red on a second branch every second unit, a change of goal
/// 700, a baseline's group of a goal and of a finding with their marks,
/// and a finding linked to a proposal of its own and resolved. (A task's
/// baselines are its row's records, which the earlier records give by the
/// task when the range names it: the units add none.)
fn named_units(log: &mut Log, settled: usize) {
    for k in 0..settled {
        let t = 11 * DAY + i64::try_from(k).unwrap() * 139 * DAY / i64::try_from(settled).unwrap();
        let unit = i64::try_from(k).unwrap();
        let priority = if k % 2 == 0 { "high" } else { "normal" };
        let after = if k == 0 {
            "base".to_owned()
        } else {
            sha(k - 1)
        };
        let mut changes = vec![json!({"path": format!("src/u{k}.rs")})];
        if k > 0 {
            changes.push(json!({"path": format!("src/u{}.rs", k - 1), "deleted": true}));
        }
        commits(
            log,
            Some(&after),
            json!([{"sha": sha(k), "at": T0 + t + 1400, "changes": changes, "reverts": null}]),
            t + 1400,
        );
        log.add(
            None,
            None,
            "main_paths_recorded",
            json!({"head": sha(k), "since": T0 + 10 * DAY, "part": 0, "parts": 1,
                   "paths": ["src/base.rs", format!("src/u{k}.rs")]}),
            t + 1405,
        );
        observed(log, &sha(k), t + 1410);
        if k % 2 == 1 {
            log.add(
                None,
                None,
                "ci_checked",
                json!({"branch": "release"}),
                t + 1425,
            )
            .add(
                None,
                None,
                "ci_turned_red",
                json!({"sha": sha(k), "branch": "release"}),
                t + 1435,
            );
        }
        // Closed and reopened before its update: the closes before the
        // goal's last replacement are as many as the units.
        of_goal(
            log,
            GOAL,
            "goal_closed",
            json!({"verdict": "achieved"}),
            t + 1212,
        );
        of_goal(log, GOAL, "goal_reopened", json!({}), t + 1213);
        of_goal(
            log,
            GOAL,
            "goal_updated",
            json!({"new": goal_record(&format!("goal 700 v{k}"), priority)}),
            t + 1215,
        );
        of_goal(
            log,
            600_000 + unit,
            "attributes_baseline",
            json!({"goal": goal_record(&format!("goal {}", 600_000 + unit), "low")}),
            t + 1220,
        );
        log.add(None, None, "attributes_baseline", json!({}), t + 1230);
        let mut baseline = finding_record("resolved");
        baseline["finding_id"] = json!(10_000 + unit);
        baseline[findings::VALUES]["id"] = json!(10_000 + unit);
        log.add(None, None, "finding_baseline", baseline, t + 1240)
            .add(None, None, "finding_baseline", json!({}), t + 1250);
        let finding = 20_000 + unit;
        let mut recorded = finding_record("open");
        recorded["finding_id"] = json!(finding);
        recorded[findings::VALUES]["id"] = json!(finding);
        log.add(None, None, "finding_recorded", recorded, t + 1300);
        finding_change(log, finding, "open", "proposed", t + 1301);
        log.events.last_mut().unwrap().payload["proposal_id"] = json!(500_000 + unit);
        finding_change(log, finding, "proposed", "resolved", t + 1302);
    }
}

/// The cases of (B) to (F) before the range and in it.
fn named_cases(log: &mut Log) {
    // Main's record from its base, CI on `release` before every unit.
    commits(
        log,
        None,
        json!([{"sha": "base", "at": T0 + 10 * DAY, "changes": [{"path": "src/base.rs"}],
                "reverts": null},
               {"sha": "human-target", "at": T0 + 10 * DAY, "changes": [], "reverts": null}]),
        10 * DAY,
    );
    commits(
        log,
        Some("human-target"),
        json!([{"sha": "human-before", "at": T0 + 10 * DAY + 5, "changes": [], "reverts": null}]),
        10 * DAY + 5,
    );
    commits(
        log,
        Some("human-before"),
        json!([{"sha": "human-for-seek", "at": T0 + 10 * DAY + 6, "changes": [], "reverts": null}]),
        10 * DAY + 6,
    );
    // Longer than the former three seek passes: each fetched event exposes
    // another target, and all returned Facts need the same reason as whole.
    for n in 1..=5 {
        let target = if n == 1 {
            "human-for-seek".to_owned()
        } else {
            format!("chain{}", n - 1)
        };
        commits(
            log,
            None,
            json!([{"sha": format!("chain{n}"), "at": T0 + 10 * DAY + 10 + n,
                    "changes": [], "reverts": target}]),
            10 * DAY + 10 + n,
        );
    }
    log.add(
        None,
        None,
        "ci_checked",
        json!({"branch": "release"}),
        10 * DAY + 20,
    );
    of_goal(
        log,
        GOAL,
        "goal_created",
        json!({"goal": goal_record("goal 700", "normal")}),
        10 * DAY + 30,
    );
    // (D) Run 60's c60, landed before the range, turned red before it, then
    // another sha did; a revert of c60 before the range and one in it, its
    // turn to red again in it; CI's checks of two branches either side.
    commits(
        log,
        Some("base"),
        json!([{"sha": "c60", "at": T0 + BEFORE + 750, "changes": [{"path": "src/c60.rs"}],
                "reverts": null}]),
        BEFORE + 800,
    );
    // c60's seek returns the whole compound event, including this unrelated
    // revert whose target is not otherwise named by the range.
    commits(
        log,
        Some("c60"),
        json!([{"sha": "c60", "at": T0 + BEFORE + 750, "changes": [{"path": "src/c60.rs"}], "reverts": null},
               {"sha": "sought-revert", "at": T0 + BEFORE + 810, "changes": [], "reverts": "chain5"}]),
        BEFORE + 810,
    );
    log.add(None, None, "main_reverts_imported",
        json!({"since": T0 + 10 * DAY, "at": T0 + BEFORE + 815, "unread": [],
               "reverts": [{"sha": "import-before", "at": T0 + BEFORE + 812, "reverts": "human-before"}]}),
        BEFORE + 815);
    log.add(
        None,
        None,
        "ci_checked",
        json!({"branch": "main"}),
        BEFORE + 900,
    )
    .add(
        None,
        None,
        "ci_turned_red",
        json!({"sha": "c60", "branch": "main"}),
        156 * DAY + 30,
    )
    .add(
        None,
        None,
        "ci_turned_red",
        json!({"sha": "x1", "branch": "main"}),
        157 * DAY,
    );
    commits(
        log,
        Some("c60"),
        json!([{"sha": "rv60b", "at": T0 + 157 * DAY + 100, "changes": [],
                "reverts": "c60"}]),
        157 * DAY + 100,
    );
    observed(log, "rv60b", 158 * DAY);
    commits(
        log,
        Some("rv60b"),
        json!([{"sha": "c12", "at": T0 + NEAR + 840, "changes": [{"path": "src/c12.rs"}],
                "reverts": null}]),
        NEAR + 850,
    );
    log.add(
        None,
        None,
        "ci_turned_red",
        json!({"sha": "c60", "branch": "main"}),
        NEAR + 900,
    )
    .add(
        None,
        None,
        "ci_checked",
        json!({"branch": "main"}),
        NEAR + 950,
    )
    .add(
        None,
        None,
        "ci_checked",
        json!({"branch": "release"}),
        NEAR + 960,
    );
    commits(
        log,
        Some("c12"),
        json!([{"sha": "rv60", "at": T0 + NEAR + 1000, "changes": [], "reverts": "c60"}]),
        NEAR + 1000,
    );
    observed(log, "rv60", NEAR + 1010);
    // An imported revert of a human's commit recorded before the summary.
    // Its target must be sought too, to distinguish not_a_landing from
    // before_record in the returned Fact, even though it names no run.
    log.add(
        None,
        None,
        "main_reverts_imported",
        json!({"since": T0 + 10 * DAY, "at": T0 + NEAR + 1020, "unread": [],
               "reverts": [{"sha": "imported-revert", "at": T0 + NEAR + 1015,
                            "reverts": "human-target"}]}),
        NEAR + 1020,
    );
    // (E) Goal 700's status after its last change; (B) its baseline and
    // task 2's before the range in a group whose mark is in it, after the
    // `--at` window.
    of_goal(
        log,
        GOAL,
        "goal_status_changed",
        json!({"from": "open", "to": "draft"}),
        156 * DAY + 500,
    );
    of_goal(
        log,
        GOAL,
        "attributes_baseline",
        json!({"goal": goal_record("goal 700 baseline", "low")}),
        157 * DAY + 600,
    );
    log.add(
        Some(2),
        None,
        "attributes_baseline",
        json!({"title": "t2 baseline", "status": "ready", "priority": "low"}),
        157 * DAY + 650,
    );
    log.add(None, None, "attributes_baseline", json!({}), NEAR + 3100);
    // (E) Goal 702 replaced by a baseline whose mark is before the range,
    // its status changed after; both goals' tasks are ready in the range,
    // so the diagram draws them in their goals' frames.
    // A forecast's snapshot of goal 702, its close and reopening before the
    // goal's last replacement, and its close again in the range: the
    // forecast's score takes the first close after the snapshot.
    log.add(
        None,
        None,
        "forecast_recorded",
        json!({"at_secs": T0 + 153 * DAY, "method": 1, "tasks": [],
               "goals": [{"id": BASELINED_GOAL, "p50_secs": 3600, "p90_secs": 7200}]}),
        153 * DAY,
    );
    of_goal(
        log,
        BASELINED_GOAL,
        "goal_closed",
        json!({"verdict": "achieved"}),
        153 * DAY + 200,
    );
    of_goal(
        log,
        BASELINED_GOAL,
        "goal_reopened",
        json!({}),
        153 * DAY + 300,
    );
    of_goal(
        log,
        BASELINED_GOAL,
        "goal_updated",
        json!({"new": goal_record("goal 702 old", "low")}),
        154 * DAY,
    );
    of_goal(
        log,
        BASELINED_GOAL,
        "goal_closed",
        json!({"verdict": "achieved"}),
        NEAR + 1500,
    );
    of_goal(
        log,
        BASELINED_GOAL,
        "attributes_baseline",
        json!({"goal": goal_record("goal 702", "high")}),
        155 * DAY,
    );
    log.add(None, None, "attributes_baseline", json!({}), 155 * DAY + 10);
    of_goal(
        log,
        BASELINED_GOAL,
        "goal_status_changed",
        json!({"from": "open", "to": "open"}),
        156 * DAY,
    );
    for (task, goal) in [(900, GOAL), (901, BASELINED_GOAL)] {
        log.add(
            Some(task),
            None,
            "task_created",
            json!({"title": format!("t{task}"), "status": "ready", "priority": "high",
                   "goal_id": goal}),
            NEAR + 50,
        );
    }
    // Task 902 of the range, known by its baseline (an older binary's),
    // waits for goal 704, achieved before the range: the task is ready to
    // be claimed only as goal 704's verdict is read.
    of_goal(
        log,
        DEPENDED_GOAL,
        "goal_created",
        json!({"goal": goal_record("goal 704", "normal")}),
        140 * DAY,
    );
    of_goal(
        log,
        DEPENDED_GOAL,
        "goal_closed",
        json!({"verdict": "achieved"}),
        140 * DAY + 100,
    );
    log.add(
        Some(902),
        None,
        "attributes_baseline",
        json!({"status": "ready", "goal_id": GOAL, "title": "t902", "change": null,
               "execution_class": null, "priority": "high", "priority_by": null,
               "origin": null, "depends_on": [], "goal_dependencies": [DEPENDED_GOAL],
               "unfinished_run": null}),
        NEAR + 60,
    );
    // (F) The findings linked before the range resolved before it: the
    // proposals they link run on their goals' tasks (51, 52, 53).
    for (finding, secs) in [
        (51, BEFORE + 60_500),
        (52, BEFORE + 70_500),
        (53, BEFORE + 90_500),
    ] {
        finding_change(log, finding, "proposed", "resolved", secs);
    }
}

/// [`history`] of 30 units (the cases of the earlier records, whose
/// settled units are another read's) with `settled` units and the cases of
/// the kinds the views read, and main's events before the cases padded to
/// a whole number of summaries ([`SUMMARY_EVERY`]), so that the summary
/// before the range is as far from it whatever the units.
pub(crate) fn named_history(settled: usize) -> Log {
    let mut log = history(30, 0);
    let mut added = Log::default();
    named_cases(&mut added);
    named_units(&mut added, settled);
    let summarized = |e: &RunEvent| SUMMARIZED_KINDS.iter().any(|kind| e.kind == *kind);
    let before_the_cases = added
        .events
        .iter()
        .filter(|e| {
            summarized(e)
                && e.created_at < crate::domain::marks::utc_text((T0 + BEFORE + 800) * 1000)
        })
        .count();
    let pad = (SUMMARY_EVERY as usize - before_the_cases % SUMMARY_EVERY as usize)
        % SUMMARY_EVERY as usize;
    for n in 0..pad {
        observed(
            &mut added,
            "base",
            10 * DAY + 100 + i64::try_from(n).unwrap(),
        );
    }
    let mut events = log.events.split_off(0);
    for (n, e) in added.events.iter_mut().enumerate() {
        e.id = EventId::new(-1 - i64::try_from(n).unwrap_or(0));
    }
    for e in added.events {
        let at = events.partition_point(|held| held.created_at <= e.created_at);
        events.insert(at, e);
    }
    // The runs land their commits: run 60 c60, run 12 c12, each unit's its sha.
    for e in &mut events {
        if e.kind.as_str() != "run_integrated" {
            continue;
        }
        let Some(run) = e.run_id.as_ref().map(|run| run.as_str().to_owned()) else {
            continue;
        };
        let commit = if run == super::ledger_earlier::run(60) {
            Some("c60".to_owned())
        } else if run == super::ledger_earlier::run(12) {
            Some("c12".to_owned())
        } else {
            run.strip_suffix("-2222-4222-8222-222222222222")
                .and_then(|k| usize::from_str_radix(k, 16).ok())
                .map(sha)
        };
        if let Some(commit) = commit {
            e.payload["result_commit"] = json!(commit);
        }
    }
    renumbered(events)
}

fn renumbered(mut events: Vec<RunEvent>) -> Log {
    let ids: std::collections::HashMap<i64, i64> = events
        .iter()
        .enumerate()
        .map(|(index, e)| (e.id.as_i64(), i64::try_from(index).unwrap_or(0) + 1))
        .collect();
    for (index, e) in events.iter_mut().enumerate() {
        e.id = EventId::new(i64::try_from(index).unwrap_or(0) + 1);
        if let Some(opened) = e.payload.get("opened_event_id").and_then(Value::as_i64) {
            e.payload["opened_event_id"] = json!(ids.get(&opened));
        }
    }
    Log { events }
}

/// Lay `log` down on `tx` with the views of the queue rows filled.
pub(crate) fn lay_down_named(tx: &impl SqlWrite, log: &Log) -> Result<super::ledger_earlier::Laid> {
    let laid = lay_down(tx, log)?;
    fill_views(tx)?;
    Ok(laid)
}

/// Fill the views of the queue rows to their end, and the other tables.
pub(crate) fn fill_views(tx: &impl SqlWrite) -> Result<()> {
    fill(tx)?;
    for _ in 0..10_000 {
        if views::fill(tx)?.is_done() {
            return Ok(());
        }
    }
    anyhow::bail!("the views' filling did not finish")
}

/// What a read returns of the store (Spike 4678's `Reads`): the run, task,
/// session and node rows, the earlier records and first Executions, the
/// queue's rows read whole and by their range, and of the views, main's
/// summary and the rows after it apart, the records and rows the seeks
/// give; with whether the views were read at all.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Returned {
    pub(crate) rows: usize,
    pub(crate) earlier: usize,
    pub(crate) whole: usize,
    pub(crate) by_range: usize,
    pub(crate) summaries: usize,
    pub(crate) after_summary: usize,
    pub(crate) sought: usize,
    /// The events of the small reads: a run's on its way, one by its id,
    /// and those of a kind at a window's edge or a declared bound.
    pub(crate) small: usize,
    pub(crate) views: bool,
}

/// A snapshot that counts what it returns ([`Returned`]), reading through
/// the store's.
struct Counted<'a> {
    snapshot: &'a dyn LedgerWindowRead,
    returned: Cell<Returned>,
}

impl Counted<'_> {
    fn add(&self, f: impl FnOnce(&mut Returned)) {
        let mut returned = self.returned.get();
        f(&mut returned);
        self.returned.set(returned);
    }

    fn rows(&self, rows: &[LedgerRow], by_range: bool) {
        self.add(|r| {
            for row in rows {
                match row.kind {
                    LedgerKind::Queue if by_range => r.by_range += 1,
                    LedgerKind::Queue => r.whole += 1,
                    _ => r.rows += 1,
                }
            }
        });
    }

    fn earlier_counted(&self, earlier: EarlierRecords) -> EarlierRecords {
        self.add(|r| r.earlier += earlier.records.len() + earlier.executions.len());
        earlier
    }
}

impl LedgerWindowRead for Counted<'_> {
    fn ledger_marks(&self) -> Result<Option<crate::domain::ledger::session::written::Marks>> {
        self.snapshot.ledger_marks()
    }
    fn visible_events(&self, events: &[RunEvent]) -> Result<Option<Vec<RunEvent>>> {
        self.snapshot.visible_events(events)
    }
    fn mark_events(&self, start_ms: i64, end_ms: i64) -> Result<WindowRead<Vec<RunEvent>>> {
        self.snapshot.mark_events(start_ms, end_ms)
    }
    fn cursor_record_time(
        &self,
        cursor: EventId,
    ) -> Result<WindowRead<crate::application::CursorRecordTime>> {
        self.snapshot.cursor_record_time(cursor)
    }
    fn forecast_events(&self, start_ms: i64, end_ms: i64) -> Result<WindowRead<Vec<RunEvent>>> {
        self.snapshot.forecast_events(start_ms, end_ms)
    }
    fn run_finishes(&self, range: Option<(i64, i64)>) -> Result<WindowRead<Vec<RunFinish>>> {
        self.snapshot.run_finishes(range)
    }
    fn comparison_events(&self) -> Result<WindowRead<Vec<RunEvent>>> {
        self.snapshot.comparison_events()
    }
    fn rows_after(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LedgerRow>> {
        let rows = self.snapshot.rows_after(kind, after, limit)?;
        self.rows(&rows, false);
        Ok(rows)
    }
    fn rows_from(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
        from: &RowsFrom,
    ) -> Result<Vec<LedgerRow>> {
        let rows = self.snapshot.rows_from(kind, after, limit, from)?;
        self.rows(&rows, false);
        Ok(rows)
    }
    fn earlier(&self, from: &RowsFrom) -> Result<EarlierRecords> {
        Ok(self.earlier_counted(self.snapshot.earlier(from)?))
    }
    fn earlier_named(
        &self,
        from: &RowsFrom,
        named: &crate::domain::ledger::window::named::Named,
    ) -> Result<EarlierRecords> {
        Ok(self.earlier_counted(self.snapshot.earlier_named(from, named)?))
    }
    fn row(&self, kind: LedgerKind, key: &str) -> Result<Option<LedgerRow>> {
        let row = self.snapshot.row(kind, key)?;
        self.rows(row.as_slice(), false);
        Ok(row)
    }
    fn run_events(&self, run: &RunId) -> Result<Vec<RunEvent>> {
        let events = self.snapshot.run_events(run)?;
        self.add(|r| r.small += events.len());
        Ok(events)
    }
    fn omitted_coverage(&self) -> Result<Option<Coverage>> {
        self.snapshot.omitted_coverage()
    }
    fn omitted_points_after(&self, kind: &str, after: EventId, limit: usize) -> Result<Vec<Point>> {
        self.snapshot.omitted_points_after(kind, after, limit)
    }
    fn omitted_point_at_or_before(&self, kind: &str, at: EventId) -> Result<Option<Point>> {
        self.snapshot.omitted_point_at_or_before(kind, at)
    }
    fn omitted_point_before_ms(&self, kind: &str, ms: i64) -> Result<Option<Point>> {
        self.snapshot.omitted_point_before_ms(kind, ms)
    }
    fn event(&self, id: EventId) -> Result<Option<RunEvent>> {
        let event = self.snapshot.event(id)?;
        self.add(|r| r.small += usize::from(event.is_some()));
        Ok(event)
    }
    fn events_of_kind(
        &self,
        kind: &str,
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        let events = self.snapshot.events_of_kind(kind, after, upto, limit)?;
        self.add(|r| r.small += events.len());
        Ok(events)
    }
    fn reads_queue_by_kind(&self) -> bool {
        self.snapshot.reads_queue_by_kind()
    }
    fn reads_named_queue(&self) -> Result<bool> {
        self.snapshot.reads_named_queue()
    }
    fn named_queue_records(
        &self,
        from: &RowsFrom,
        names: &crate::domain::ledger::window::queue_lives::Names,
        end_ms: i64,
    ) -> Result<Option<Vec<RunEvent>>> {
        let records = self.snapshot.named_queue_records(from, names, end_ms)?;
        self.add(|r| r.small += records.as_ref().map_or(0, Vec::len));
        Ok(records)
    }
    fn queue_rows_whole(&self, after: Option<&str>, limit: usize) -> Result<Vec<LedgerRow>> {
        let rows = self.snapshot.queue_rows_whole(after, limit)?;
        self.rows(&rows, false);
        Ok(rows)
    }
    fn queue_rows_reaching(&self, kind: &str, from_ms: i64) -> Result<Vec<LedgerRow>> {
        let rows = self.snapshot.queue_rows_reaching(kind, from_ms)?;
        self.rows(&rows, true);
        Ok(rows)
    }
    fn queue_rows_past(
        &self,
        kind: &str,
        from_ms: i64,
        queue_before: EventId,
    ) -> Result<Vec<LedgerRow>> {
        let rows = self.snapshot.queue_rows_past(kind, from_ms, queue_before)?;
        self.rows(&rows, true);
        Ok(rows)
    }
    fn queue_views_usable(&self) -> Result<bool> {
        self.snapshot.queue_views_usable()
    }
    fn queue_rows_whole_by_views(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LedgerRow>> {
        let rows = self.snapshot.queue_rows_whole_by_views(after, limit)?;
        self.rows(&rows, false);
        Ok(rows)
    }
    fn queue_main_before(&self, queue_before: EventId, from_ms: i64) -> Result<MainBefore> {
        let before = self.snapshot.queue_main_before(queue_before, from_ms)?;
        self.add(|r| {
            r.views = true;
            r.summaries += before.summaries_read;
            r.after_summary += before.rows.len();
        });
        Ok(before)
    }
    fn queue_sought(&self, from: &RowsFrom, wants: &QueueWants) -> Result<QueueSought> {
        let sought = self.snapshot.queue_sought(from, wants)?;
        self.add(|r| {
            r.views = true;
            r.sought += sought.records.len() + sought.rows.len();
        });
        Ok(sought)
    }
}

/// The window `query` asks of `tx`, read as the store reads it, with what
/// the store returned.
pub(crate) fn counted_window(
    tx: &impl SqlRead,
    query: &WindowQuery,
) -> Result<(LedgerWindow, Returned)> {
    view::with_snapshot(tx, |snapshot| {
        let counted = Counted {
            snapshot,
            returned: Cell::new(Returned::default()),
        };
        let window = ledger_window_in(&counted, query)?;
        Ok((window, counted.returned.get()))
    })
}

const HOST: Host = Host {
    utc_offset_secs: 0,
    cores: Some(4),
    rust_toolchain: false,
};

fn areas() -> AreaMap {
    AreaMap::new(vec![
        ("core".to_owned(), vec!["src/c*.rs".to_owned()]),
        ("base".to_owned(), vec!["src/base.rs".to_owned()]),
    ])
    .expect("the areas")
}

/// The questions of `kpi`: Spike 4678's and one `--by area`.
fn kpi_asked() -> Vec<KpiQuery> {
    let mut asked = kpi_questions();
    asked.push(KpiQuery {
        by: vec!["area".parse().unwrap()],
        ..asked[0].clone()
    });
    asked
}

/// The questions of `stats` with a time `--since`, in full: to now and to
/// a day before it.
fn stats_asked() -> Vec<StatsQuery> {
    let time = |secs: i64| Some(Cursor::Time((T0 + secs) * 1000));
    vec![
        StatsQuery {
            since: time(SINCE),
            full: true,
            ..StatsQuery::default()
        },
        StatsQuery {
            since: time(SINCE),
            until: time(NEAR + 2000),
            full: true,
            ..StatsQuery::default()
        },
    ]
}

/// What a window answers the questions of `kpi` (`n`), of `stats` and of
/// the report's diagram with: the JSON of each, and the goals' summaries
/// of the goals its tasks are in (those the report's diagram titles).
fn answers(
    window: &LedgerWindow,
    kpi: Option<&KpiQuery>,
    stats: Option<&StatsQuery>,
) -> Result<Value> {
    let areas = areas();
    let mut answer = json!({});
    if let Some(query) = kpi {
        let kpi = kpi_of_window(
            window,
            NOW,
            HOST,
            &KpiConfig::default(),
            Some(&areas),
            query,
        )?;
        answer["kpi"] = serde_json::to_value(kpi)?;
    }
    if let Some(query) = stats {
        let sources = StatsSources {
            config_file: &|| Ok(None),
            conflicts_file: &|| Ok(None),
            areas: Some(&areas),
            utc_offset_secs: 0,
            counts_checks: true,
            rust_toolchain: false,
        };
        answer["stats"] = serde_json::to_value(stats_of_window(window, NOW, query, &sources)?)?;
        answer["main_covers"] = json!(format!("{:?}", window.main_covers()));
    }
    let book = window.post_landing_book();
    answer["ci_records"] = json!({
        "main": book.ci_record("main"),
        "release": book.ci_record("release"),
        "ci-only": book.ci_record("ci-only"),
    });
    answer["diagram"] = serde_json::to_value(crate::application::report::diagram_draft(window))?;
    if let attributes::Folded::Attributes(known) = window.attributes() {
        let goals: std::collections::BTreeSet<GoalId> = known
            .tasks
            .values()
            .filter_map(|task| task.goal_id)
            .collect();
        let summaries: Vec<Value> = known
            .goal_summaries()
            .into_iter()
            .filter(|goal| goals.contains(&goal.id))
            .map(|goal| json!(format!("{goal:?}")))
            .collect();
        answer["goal_summaries"] = Value::Array(summaries);
    }
    Ok(answer)
}

/// Whether the questions read by their range on `tx` answer as every row
/// does, with what each returned (the questions of `kpi`, then those of
/// `stats`): every question when `all`, else `kpi`'s default, `--at` and
/// `--by area` and `stats --since` to now (each of every row's reads
/// folds the whole history again).
pub(crate) fn named_as_every_row(tx: &impl SqlRead, all: bool) -> Result<Vec<Returned>> {
    let whole = view::window(tx, &whole_query())?;
    let mut returned = Vec::new();
    let asked = |n: usize| all || matches!(n, 0 | 3 | 4);
    for (n, query) in kpi_asked().iter().enumerate().filter(|(n, _)| asked(*n)) {
        let expected = answers(
            &whole,
            Some(&KpiQuery {
                read_whole_ledger: true,
                ..query.clone()
            }),
            None,
        )?;
        let (window, read) = counted_window(tx, &time_query(query))?;
        let got = answers(&window, Some(query), None)?;
        let mut misses = Vec::new();
        differences(&got, &expected, &format!("kpi[{n}]"), &mut misses);
        ensure!(misses.is_empty(), "{misses:?}");
        returned.push(read);
    }
    for (n, query) in stats_asked()
        .iter()
        .enumerate()
        .filter(|(n, _)| all || *n == 0)
    {
        let range = crate::application::stats::window_query(query, NOW);
        ensure!(
            range.read_from_ms.is_some(),
            "stats[{n}] reads by its range"
        );
        let mut every = range.clone();
        every.read_from_ms = None;
        let expected = answers(&view::window(tx, &every)?, None, Some(query))?;
        let (window, read) = counted_window(tx, &range)?;
        let got = answers(&window, None, Some(query))?;
        let mut misses = Vec::new();
        differences(&got, &expected, &format!("stats[{n}]"), &mut misses);
        ensure!(misses.is_empty(), "{misses:?}");
        returned.push(read);
    }
    Ok(returned)
}

/// What the cases of the history give every row's read: the counterexamples
/// the answers are compared on are there to be compared.
fn the_cases_are_there(tx: &impl SqlRead) -> Result<()> {
    let whole = view::window(tx, &whole_query())?;
    let run60 = RunId::new(run(60))?;
    let book = whole.post_landing_book().of_run(&run60);
    ensure!(
        book.reverts.count == 2 && book.ci_failures.count == 2,
        "run 60's reverts and failures: {book:?}"
    );
    ensure!(
        matches!(
            crate::application::report::diagram_draft(&whole),
            crate::application::report::DiagramDraft::Source { .. }
        ),
        "the diagram is drawn"
    );
    let kpi = answers(
        &whole,
        Some(&KpiQuery {
            read_whole_ledger: true,
            ..kpi_questions()[0].clone()
        }),
        None,
    )?;
    ensure!(
        kpi.to_string().contains("improvement_proposals"),
        "improvement_proposals in the answer"
    );
    Ok(())
}

/// What the read by the views of the day's question takes of before the
/// range: the cases the seeks find, and none of the settled units'.
fn the_seeks_find_the_cases(tx: &impl SqlRead) -> Result<()> {
    let query = time_query(&kpi_questions()[0]);
    let from = query.read_from_ms.unwrap_or_default();
    let (window, read) = counted_window(tx, &query)?;
    ensure!(read.views, "{read:?}");
    let whole = view::window(tx, &whole_query())?;
    let book = whole.post_landing_book();
    let got = window.post_landing_book();
    for sha in [
        "imported-revert",
        "import-before",
        "sought-revert",
        "chain5",
        "chain4",
        "chain3",
        "chain2",
        "chain1",
    ] {
        let expected = book
            .reverts
            .iter()
            .find(|fact| fact.sha == sha)
            .ok_or_else(|| anyhow::anyhow!("the revert {sha}"))?;
        ensure!(expected.reason == Some("not_a_landing"), "{expected:?}");
        ensure!(
            got.reverts.iter().find(|fact| fact.sha == sha) == Some(expected),
            "{sha}'s whole Fact, including its target's reason: {:?} of {expected:?}",
            got.reverts
        );
    }
    let before = |e: &RunEvent| {
        crate::domain::stats::timestamp_millis(&e.created_at).is_some_and(|at| at < from)
    };
    let holds = |f: &dyn Fn(&RunEvent) -> bool| -> Result<bool> {
        Ok(window.events().iter().any(|e| before(e) && f(e)))
    };
    let red = |sha: &str| {
        let sha = sha.to_owned();
        move |e: &RunEvent| e.kind.as_str() == "ci_turned_red" && e.payload["sha"] == sha.as_str()
    };
    let commit = |sha: &str| {
        let sha = sha.to_owned();
        move |e: &RunEvent| {
            e.kind.as_str() == "main_commits_recorded"
                && e.payload["commits"][0]["sha"] == sha.as_str()
        }
    };
    let of_goal = |goal: i64, kind: &'static str| {
        move |e: &RunEvent| e.goal_id == Some(GoalId::new(goal)) && e.kind.as_str() == kind
    };
    let resolved = |finding: i64| {
        move |e: &RunEvent| {
            e.kind.as_str() == "finding_status_changed"
                && e.payload["finding_id"] == finding
                && e.payload["to"] == "resolved"
        }
    };
    for (what, found) in [
        ("c60's turn to red", holds(&red("c60"))?),
        ("c60's record", holds(&commit("c60"))?),
        ("c60's revert", holds(&commit("rv60b"))?),
        (
            "goal 700's status",
            holds(&of_goal(GOAL, "goal_status_changed"))?,
        ),
        (
            "goal 702's baseline",
            holds(&of_goal(BASELINED_GOAL, "attributes_baseline"))?,
        ),
        (
            "goal 702's close before its last replacement",
            holds(&of_goal(BASELINED_GOAL, "goal_closed"))?,
        ),
        ("finding 51", holds(&resolved(51))?),
        ("finding 52", holds(&resolved(52))?),
        ("finding 53", holds(&resolved(53))?),
    ] {
        ensure!(found, "the read takes {what}");
    }
    for (what, found) in [
        ("a unit's turn to red", holds(&red(&sha(1)))?),
        ("a unit's commit", holds(&commit(&sha(1)))?),
        (
            "goal 700's changes before its last",
            holds(&|e: &RunEvent| {
                of_goal(GOAL, "goal_updated")(e) && e.payload["new"]["title"] == "goal 700 v0"
            })?,
        ),
        ("a unit's finding", holds(&resolved(20_000))?),
    ] {
        ensure!(!found, "the read takes no {what}");
    }
    Ok(())
}

/// The answers of every row read by the views on a history with the
/// cases of (B) to (F) and settled units of every kind they read, and
/// what the store returns the same with `small` and `large` units: main's
/// summary and the rows after it apart (the units are padded to whole
/// summaries, so those are as many too, at most a summary's span and the
/// range's), and the views read by every question.
pub fn ledger_queue_named_answers_as_every_row<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    ledger_queue_named_with(open, 30, 300)
}

/// [`ledger_queue_named_answers_as_every_row`] with `small` and `large` units.
pub(crate) fn ledger_queue_named_with<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    small: usize,
    large: usize,
) -> Result<()> {
    compare_named_histories(open, small, large, named_history)
}

fn compare_named_histories<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    small: usize,
    large: usize,
    history: fn(usize) -> Log,
) -> Result<()> {
    let mut reads = Vec::new();
    for settled in [small, large] {
        let store = open()?;
        let tx = store.write_entry("contract_ledger_queue_named")?;
        let laid = lay_down_named(&tx, &history(settled))?;
        ensure!(views::usable(&tx)?, "the views are trusted");
        the_cases_are_there(&tx)?;
        let returned = named_as_every_row(&tx, settled == small)?;
        the_seeks_find_the_cases(&tx)?;
        for (n, read) in returned.iter().enumerate() {
            ensure!(
                read.views && read.summaries == 1,
                "[{n}] reads by the views: {read:?}"
            );
            ensure!(
                read.after_summary < SUMMARY_EVERY as usize + 32,
                "[{n}] main's rows after the summary: {read:?}"
            );
        }
        reads.push(returned);
        clear(&tx, &laid)?;
        drop(tx);
    }
    // The questions the large history is asked, of the small one's.
    let small_reads: Vec<Returned> = reads[0]
        .iter()
        .enumerate()
        .filter(|(n, _)| matches!(n, 0 | 3 | 4 | 5))
        .map(|(_, read)| *read)
        .collect();
    ensure!(small_reads.len() == reads[1].len());
    for (n, (a, b)) in small_reads.iter().zip(&reads[1]).enumerate() {
        ensure!(
            a == b,
            "[{n}] returns {a:?} of {small} units and {b:?} of {large}"
        );
    }
    Ok(())
}

/// A clock set back before all settled units: an early check of `release`
/// bears a time after every window's end. It must neither enter CI's record
/// before that time nor make main's summary walk back across the units.
pub fn ledger_queue_named_past_a_clock_set_back<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    ledger_queue_named_clock_with(open, 30, 300)
}

/// A future main observation followed only by settled CI checks. No main
/// event between the checks can supply a clock alternative on their behalf.
pub fn ledger_queue_named_with_only_ci_after_a_future_main_event<
    S: ContractStore,
    F: Fn() -> Result<S>,
>(
    open: &F,
) -> Result<()> {
    ledger_queue_named_ci_only_with(open, 30, 300)
}

fn ci_only_history(settled: usize) -> Log {
    let mut log = named_history(30);
    observed(&mut log, "future-ci-only", NOW - T0 + DAY);
    for n in 0..settled {
        // The same latest boundary precedes S at either volume, after the
        // earlier checks of the base fixture. It puts every unit before
        // queue_before, rather than exercising the separate rule that
        // reads every row whose inputs pass that boundary.
        let secs = BEFORE + i64::try_from(n + 1).unwrap() * DAY / i64::try_from(settled).unwrap();
        log.add(
            None,
            None,
            "ci_checked",
            json!({"branch": "ci-only", "sha": "base", "conclusion": "success"}),
            secs,
        );
    }
    log
}

pub(crate) fn ledger_queue_named_ci_only_with<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    small: usize,
    large: usize,
) -> Result<()> {
    compare_named_histories(open, small, large, ci_only_history)
}

fn clock_history(settled: usize) -> Log {
    let mut log = named_history(settled);
    let first = log
        .events
        .iter_mut()
        .find(|e| e.kind.as_str() == "ci_checked" && e.payload["branch"] == "release")
        .expect("the early release check");
    first.created_at = crate::domain::marks::utc_text((NOW + DAY) * 1000);
    // A ref and its short spelling share the same branch, even when the
    // earliest and latest boundaries have different spellings.
    first.payload["branch"] = json!("refs/heads/release");
    // The valid earliest boundary is a full ref too, and the latest is
    // short: a seek must combine both spellings by time.
    log.events
        .iter_mut()
        .find(|e| e.kind.as_str() == "ci_checked" && e.payload["branch"] == "release")
        .expect("a settled check")
        .payload["branch"] = json!("refs/heads/release");
    // Main's anomaly precedes every settled unit, not just the range's
    // tail. A paths record also crosses the same clock boundary, so the
    // alternative must retain main's paths as well as its last observation.
    // Past the --at window's E, but between S and E for the current window:
    // both sides must use the original id order of the eligible prefix.
    let future = crate::domain::marks::utc_text((T0 + NEAR + 500) * 1000);
    log.events
        .iter_mut()
        .find(|e| e.kind.as_str() == "main_observed")
        .expect("main's early observation")
        .created_at = future.clone();
    log.events
        .iter_mut()
        .find(|e| e.kind.as_str() == "main_paths_recorded")
        .expect("main's first paths")
        .created_at = future;
    let at = log
        .events
        .iter()
        .position(|e| e.kind.as_str() == "main_observed")
        .unwrap();
    let mut rewrite = log.events[at].clone();
    rewrite.id = EventId::new(-999_999);
    rewrite.kind = "main_rewritten".into();
    rewrite.payload = json!({"base": "base", "head": "base"});
    log.events.insert(at, rewrite);
    let mut commit = log.events[at].clone();
    commit.id = EventId::new(-999_998);
    commit.kind = "main_commits_recorded".into();
    commit.payload = json!({"commits": [{"sha": "future-clock-commit", "at": T0 + 10 * DAY,
                                        "changes": [], "reverts": null}]});
    log.events.insert(at, commit);
    // No later recovery masks this change: the current window must see
    // the failure, while --at (before its timestamp) must remain readable.
    let mut failed = log.events[at].clone();
    failed.id = EventId::new(-999_997);
    failed.kind = "main_read_failed".into();
    failed.payload = json!({"reason": "clock failure", "at": T0 + NEAR + 500});
    log.events.insert(at, failed);
    renumbered(log.events)
}

pub(crate) fn ledger_queue_named_clock_with<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    small: usize,
    large: usize,
) -> Result<()> {
    compare_named_histories(open, small, large, clock_history)?;
    let store = open()?;
    let tx = store.write_entry("contract_ledger_queue_named_clock")?;
    let log = clock_history(small);
    let laid = lay_down_named(&tx, &log)?;
    let late = laid
        .log
        .events
        .iter()
        .find(|e| e.kind.as_str() == "ci_checked" && e.payload["branch"] == "refs/heads/release")
        .ok_or_else(|| anyhow::anyhow!("CI's early check of release before the units"))?
        .id;
    let latest: String = tx.query_row(
        "SELECT summary FROM ledger_main_summaries ORDER BY through DESC LIMIT 1",
        sql_params![],
        |r| r.get(0),
    )?;
    let latest: crate::domain::ledger::queue_views::MainSummary = serde_json::from_str(&latest)?;
    ensure!(
        latest.through > Some(late) && latest.ci["release"].latest.id == late,
        "the summaries after the late check fold it: {:?}",
        latest.through
    );
    reads_as(&tx, true)?;
    // CI's record of each branch, as the folds from the summary give it.
    for query in kpi_asked() {
        let range = time_query(&query);
        let mut every = range.clone();
        every.read_from_ms = None;
        let (window, read) = counted_window(&tx, &range)?;
        ensure!(read.views, "reads by the views: {read:?}");
        let whole = view::window(&tx, &every)?;
        let (got, expected) = (window.post_landing_book(), whole.post_landing_book());
        for branch in ["main", "release"] {
            ensure!(
                expected.ci_record(branch).is_some()
                    && got.ci_record(branch) == expected.ci_record(branch),
                "CI's record of {branch} past the late check: {:?} of {:?}",
                got.ci_record(branch),
                expected.ci_record(branch)
            );
        }
    }
    clear(&tx, &laid)?;
    Ok(())
}

/// Whether the questions answer as every row with the views read (`views`)
/// or not, those kinds read whole by every page.
fn reads_as(tx: &impl SqlRead, views: bool) -> Result<()> {
    for (n, read) in named_as_every_row(tx, true)?.iter().enumerate() {
        ensure!(read.views == views, "[{n}] views {views}: {read:?}");
        if !views {
            ensure!(
                read.after_summary == 0 && read.sought == 0,
                "[{n}]: {read:?}"
            );
        }
    }
    Ok(())
}

/// A store whose views cannot be trusted reads the kinds of the views
/// whole and answers as every row: of no mark, a mark of no generation, of
/// another generation and filled part way, of summaries an older binary
/// wrote, of no marks of the rows (whose events' visibility then cannot be
/// judged) and of no tables; and once filled again, by the views again.
pub fn ledger_queue_named_goes_back_whole<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let store = open()?;
    let tx = store.write_entry("contract_ledger_queue_named_whole")?;
    let laid = lay_down_named(&tx, &named_history(30))?;
    reads_as(&tx, true)?;
    let mark = |through: Option<&str>, generation: Option<i64>| -> Result<()> {
        tx.execute("DELETE FROM ledger_queue_views_filled", sql_params![])?;
        tx.execute(
            "INSERT INTO ledger_queue_views_filled(through, generation) VALUES (?1, ?2)",
            sql_params![through, generation],
        )?;
        Ok(())
    };
    let generation: Option<i64> = tx.query_row(
        "SELECT generation FROM ledger_queue_views_filled",
        sql_params![],
        |r| r.get(0),
    )?;
    tx.execute("DELETE FROM ledger_queue_views_filled", sql_params![])?;
    reads_as(&tx, false)?;
    mark(None, None)?;
    reads_as(&tx, false)?;
    mark(None, Some(generation.unwrap_or(0) + 7))?;
    reads_as(&tx, false)?;
    let first: String = tx.query_row(
        "SELECT MIN(key) FROM ledger_rows WHERE kind='queue'",
        sql_params![],
        |r| r.get(0),
    )?;
    mark(Some(&first), generation)?;
    reads_as(&tx, false)?;
    mark(None, generation)?;
    reads_as(&tx, true)?;
    // Summaries an older binary wrote hold no record: whole, until the
    // filling makes them again from the first row.
    let latest: String = tx.query_row(
        "SELECT summary FROM ledger_main_summaries ORDER BY through DESC LIMIT 1",
        sql_params![],
        |r| r.get(0),
    )?;
    let mut old: Value = serde_json::from_str(&latest)?;
    old.as_object_mut().unwrap().remove("record");
    tx.execute(
        "UPDATE ledger_main_summaries SET summary=?1 WHERE through=(SELECT MAX(through) FROM ledger_main_summaries)",
        sql_params![old.to_string()],
    )?;
    reads_as(&tx, false)?;
    fill_views(&tx)?;
    reads_as(&tx, true)?;
    // Either older clock representation is untrusted even with a record.
    for field in ["ci_by_time", "main_by_time"] {
        let latest: String = tx.query_row(
            "SELECT summary FROM ledger_main_summaries ORDER BY through DESC LIMIT 1",
            sql_params![],
            |r| r.get(0),
        )?;
        let mut old: Value = serde_json::from_str(&latest)?;
        old["record"].as_object_mut().unwrap().remove(field);
        tx.execute(
        "UPDATE ledger_main_summaries SET summary=?1 WHERE through=(SELECT MAX(through) FROM ledger_main_summaries)",
        sql_params![old.to_string()],
    )?;
        reads_as(&tx, false)?;
        fill_views(&tx)?;
        reads_as(&tx, true)?;
    }
    // No marks of the rows: the visibility of what is sought cannot be
    // judged.
    let marks = view::with_snapshot(&tx, |snapshot| snapshot.ledger_marks())?
        .ok_or_else(|| anyhow::anyhow!("the marks"))?;
    for kind in [
        crate::domain::ledger::session::written::KIND,
        crate::domain::ledger::session::written::ROWS_KIND,
    ] {
        tx.execute("DELETE FROM ledger_rows WHERE kind=?1", sql_params![kind])?;
    }
    reads_as(&tx, false)?;
    view::write_marks(&tx, &marks)?;
    reads_as(&tx, true)?;
    // No tables (a schema before them).
    for table in [
        "ledger_main_summaries",
        "ledger_main_commits",
        "ledger_queue_views_filled",
    ] {
        tx.execute(&format!("DROP TABLE {table}"), sql_params![])?;
    }
    reads_as(&tx, false)?;
    drop(laid);
    drop(tx);
    Ok(())
}

/// An older binary takes the write leader over and writes queue rows of
/// the kinds the views read without the views, R and the mark's row as
/// they were: a commit of main before the range with CI's turn to red on
/// it and main's reach after it (a pass of the summary's span), and a
/// change of goal 700. The reads trust no view, whatever the mark's
/// position against R, and answer as every row; once this binary takes
/// the leader over and fills them again, they read the views again, and
/// still answer so.
pub fn ledger_queue_named_past_an_old_writer<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    let (first, a) = taken_over(open, None, "queue-named-1")?;
    let tx = first.write_entry("contract_queue_named_init")?;
    let laid = lay_down_named(&tx, &named_history(30))?;
    reads_as(&tx, true)?;
    let mark_of = |tx: &S::Write<'_>| -> Result<Vec<(Option<String>, Option<i64>)>> {
        Ok(tx.query_map(
            "SELECT through, generation FROM ledger_queue_views_filled",
            sql_params![],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    };
    let mark = mark_of(&tx)?;
    let r = view::with_snapshot(&tx, |snapshot| snapshot.ledger_marks())?;
    S::commit(tx)?;

    let (older, b) = taken_over(open, Some(("queue-named-1", a)), "queue-named-old")?;
    let tx = older.write_entry("contract_queue_named_old")?;
    let mut added = Log::default();
    commits(
        &mut added,
        Some("rv60b"),
        json!([{"sha": "c77", "at": T0 + 157 * DAY + 200, "changes": [{"path": "src/c77.rs"}],
                "reverts": "c60"}]),
        157 * DAY + 200,
    );
    added.add(
        None,
        None,
        "ci_turned_red",
        json!({"sha": "c60", "branch": "main"}),
        157 * DAY + 300,
    );
    observed(&mut added, "c77", 157 * DAY + 400);
    of_goal(
        &mut added,
        GOAL,
        "goal_updated",
        json!({"new": goal_record("goal 700 old writer", "high")}),
        157 * DAY + 500,
    );
    let past: i64 = tx.query_row("SELECT MAX(id) FROM run_events", sql_params![], |r| {
        r.get(0)
    })?;
    for e in &mut added.events {
        e.id = EventId::new(e.id.as_i64() + past);
        super::ledger_earlier::insert_event(&tx, e)?;
    }
    let rows = crate::domain::ledger::build(&added.events, NOW * 1000, "2030-01-01T00:00:00.000Z");
    for row in rows.iter().filter(|row| row.kind == LedgerKind::Queue) {
        views::write_row_alone(&tx, row)?;
    }
    ensure!(mark_of(&tx)? == mark, "the mark's row is as it was");
    ensure!(
        view::with_snapshot(&tx, |snapshot| snapshot.ledger_marks())? == r,
        "R is as it was"
    );
    ensure!(
        !views::usable(&tx)?,
        "an older writer's generation leaves the mark behind"
    );
    reads_as(&tx, false)?;
    S::commit(tx)?;

    let (current, c) = taken_over(open, Some(("queue-named-old", b)), "queue-named-current")?;
    let tx = current.write_entry("contract_queue_named_current")?;
    reads_as(&tx, false)?;
    fill_views(&tx)?;
    ensure!(views::usable(&tx)?);
    reads_as(&tx, true)?;
    views::clear_views(&tx)?;
    for e in &added.events {
        tx.execute(
            "DELETE FROM run_events WHERE id=?1",
            sql_params![e.id.as_i64()],
        )?;
    }
    // The landed commits the runs' landings put in the search's table.
    let runs: std::collections::BTreeSet<String> = laid
        .log
        .events
        .iter()
        .filter(|e| e.kind.as_str() == "run_integrated")
        .filter_map(|e| e.run_id.as_ref().map(|run| run.as_str().to_owned()))
        .collect();
    for run in &runs {
        tx.execute(
            "DELETE FROM landed_commits WHERE run_id=?1",
            sql_params![run.as_str()],
        )?;
    }
    clear(&tx, &laid)?;
    S::commit(tx)?;
    ensure!(current.release_write_leader("queue-named-current", c)?);
    Ok(())
}
