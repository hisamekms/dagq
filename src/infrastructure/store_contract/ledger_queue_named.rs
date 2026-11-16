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

use super::ledger_earlier::{
    NOW, clear, differences, fill, history, kpi_questions, lay_down, run, time_query, whole_query,
};
use super::ledger_open::taken_over;
use super::{Checks, ContractStore};
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
use crate::domain::{EventId, GoalId, RunEvent, RunId, TaskId};
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
        let t = 11 * DAY
            + i64::try_from(k).unwrap() * super::ledger_earlier::units_span(139 * DAY)
                / i64::try_from(settled).unwrap();
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

/// [`history`] of the scoped background units (30 in the full contract) (the cases of the earlier records, whose
/// settled units are another read's) with `settled` units and the cases of
/// the kinds the views read, and main's events before the cases padded to
/// a whole number of summaries ([`SUMMARY_EVERY`]), so that the summary
/// before the range is as far from it whatever the units.
pub(crate) fn named_history(settled: usize) -> Log {
    named_history_with(settled, 0)
}

/// [`named_history`] with `member_changes` changes of the plan of task 66
/// ([`history`]'s, of a proposal the range names).
pub(crate) fn named_history_with(settled: usize, member_changes: usize) -> Log {
    named_history_of(
        history(super::fixed_history_units(), member_changes),
        settled,
    )
}

/// [`named_history`] as `kpi`'s entry compares it at two sizes. The member
/// task's own changes of its plan before the range grow with the units too
/// (Spike 4755's case of a named proposal's task). The snapshots forecast
/// none of the tasks, as in the lives' fixture: the marks and plan changes
/// after an old snapshot of a target that finishes in the range are the one
/// read the range does not bound, and the forecast's contract
/// ([`super::ledger_queue_lives`]) counts them.
pub(crate) fn kpi_named_history(settled: usize) -> Log {
    let mut log = history(super::fixed_history_units(), settled);
    for event in &mut log.events {
        if event.kind == crate::domain::EventKind::ForecastRecorded {
            event.payload["tasks"] = json!([]);
        }
    }
    named_history_of(log, settled)
}

/// `log` with `settled` units and the cases of the kinds the views read
/// added, as [`named_history`] makes it.
fn named_history_of(mut log: Log, settled: usize) -> Log {
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
    // The claims of a provider pair's trial, every other one treated, so
    // that `--by pair_group` has its strata as assigned and as run.
    let mut claims = 0_usize;
    for e in &mut events {
        if e.kind.as_str() == "run_claimed" {
            let group = if claims.is_multiple_of(2) {
                "treatment"
            } else {
                "control"
            };
            claims += 1;
            e.payload["attributes"] = json!({"pair_group": group, "pair_trial": "provider-pair",
                "pair_worker": "claude", "pair_review": "claude"});
        }
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

pub(crate) fn renumbered(mut events: Vec<RunEvent>) -> Log {
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
    /// The events of `--compare --goal`'s read of every task's goal
    /// ([`LedgerWindowRead::goal_events`]): of every task, whole by kind as
    /// the cohort's marks and runs' ends are, not of the ledger's rows.
    pub(crate) goal_events: usize,
    pub(crate) views: bool,
}

impl Returned {
    /// What it returned but the goals' events, which grow with the tasks of
    /// the whole history by design ([`Self::goal_events`]).
    fn of_the_range(self) -> Self {
        Self {
            goal_events: 0,
            ..self
        }
    }
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
    fn cursor_record_time_through(
        &self,
        cursor: EventId,
        end_ms: i64,
    ) -> Result<WindowRead<crate::application::CursorRecordTime>> {
        self.snapshot.cursor_record_time_through(cursor, end_ms)
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
    fn goal_events(&self) -> Result<WindowRead<Vec<RunEvent>>> {
        let read = self.snapshot.goal_events()?;
        if let WindowRead::Complete(events) = &read {
            self.add(|r| r.goal_events += events.len());
        }
        Ok(read)
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
    counted(tx, |snapshot| ledger_window_in(snapshot, query))
}

/// What `read` gives of a snapshot of `tx`, with what the store returned
/// to it.
pub(crate) fn counted<T>(
    tx: &impl SqlRead,
    read: impl FnOnce(&dyn LedgerWindowRead) -> Result<T>,
) -> Result<(T, Returned)> {
    view::with_snapshot(tx, |snapshot| {
        let counted = Counted {
            snapshot,
            returned: Cell::new(Returned::default()),
        };
        let taken = read(&counted)?;
        Ok((taken, counted.returned.get()))
    })
}

/// The KPIs of `query` read of `tx` by `kpi`'s entry
/// ([`crate::application::kpi::kpi`]), on a store whose snapshot counts what
/// it returns ([`super::OfSnapshot`]), with what was returned. The cohort of
/// a comparison (every event of the marks' kinds and every run's first
/// end) is not counted: it is read whole by design, of the events and not
/// of the ledger's rows.
pub(crate) fn counted_kpi(tx: &impl SqlRead, query: &KpiQuery) -> Result<(Value, Returned)> {
    let areas = areas();
    let returned = Cell::new(Returned::default());
    let open = |read: &mut dyn FnMut(&dyn LedgerWindowRead) -> Result<()>| {
        view::with_snapshot(tx, |snapshot| {
            let counted = Counted {
                snapshot,
                returned: Cell::new(Returned::default()),
            };
            let result = read(&counted);
            returned.set(counted.returned.get());
            result
        })
    };
    let kpi = crate::application::kpi::kpi(
        &super::OfSnapshot { open: &open },
        NOW,
        HOST,
        &config(),
        Some(&areas),
        query,
    )?;
    Ok((serde_json::to_value(kpi)?, returned.get()))
}

/// The `[kpi]` settings the questions are judged by: the defaults with a
/// target on the landings, so that each answer judges it.
fn config() -> KpiConfig {
    use crate::domain::kpi::config::{SourcedTarget, Target};
    KpiConfig {
        targets: vec![SourcedTarget {
            target: Target {
                kpi: "landings".to_owned(),
                change: None,
                area: None,
                stat: None,
                min: Some(1000.0),
                max: None,
            },
            source: "repository",
        }],
        ..KpiConfig::default()
    }
}

/// What a question's read must be: by its range, or refused as every
/// row's read refuses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Expect {
    Range,
    Refused,
}

const HOST: Host = Host {
    utc_offset_secs: 0,
    cores: Some(4),
    rust_toolchain: false,
};

pub(crate) fn areas() -> AreaMap {
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
        let kpi = kpi_of_window(window, NOW, HOST, &config(), Some(&areas), query)?;
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
/// folds the whole history again); then, every time, the questions of
/// [`kpi_ranged`]. `kpi`'s are read as `kpi` reads them ([`counted_kpi`]).
/// What `kpi` answered each is pushed onto `answered`, in the same order
/// (null of `stats`').
pub(crate) fn named_as_every_row(
    tx: &impl SqlRead,
    all: bool,
    answered: &mut Vec<Value>,
) -> Result<Vec<(Returned, Expect)>> {
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
        let (kpi, read) = counted_kpi(tx, query)?;
        ensure!(
            time_query(query).read_from_ms.is_some(),
            "kpi[{n}] reads by its range"
        );
        // What else the window answers, of the window of the same range.
        let (window, _) = counted_window(tx, &time_query(query))?;
        let mut got = answers(&window, None, None)?;
        got["kpi"] = kpi;
        let mut misses = Vec::new();
        differences(&got, &expected, &format!("kpi[{n}]"), &mut misses);
        ensure!(misses.is_empty(), "{misses:?}");
        ensure!(read.by_range > 0, "kpi[{n}] reads by its range: {read:?}");
        returned.push((read, Expect::Range));
        answered.push(got["kpi"].clone());
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
        returned.push((read, Expect::Range));
        // `stats`' answers name the units' runs; `kpi`'s are compared.
        answered.push(Value::Null);
    }
    // Every question of [`kpi_ranged`], of either history, against an
    // independent read of every row; what each must read (by its range
    // unless every row's read refuses it, a cursor past a record timed
    // after now included) is returned with the read for the caller to
    // check of trusted views ([`compare_named_histories`]).
    for (n, query) in kpi_ranged(&whole).iter().enumerate() {
        let expected = kpi_of_window(
            &whole,
            NOW,
            HOST,
            &config(),
            Some(&areas()),
            &KpiQuery {
                read_whole_ledger: true,
                ..query.clone()
            },
        );
        let got = counted_kpi(tx, query);
        let expect = match &expected {
            Err(_) => Expect::Refused,
            Ok(_) => Expect::Range,
        };
        let read = match (got, expected) {
            (Ok((got, read)), Ok(expected)) => {
                let expected = serde_json::to_value(expected)?;
                if serde_json::to_value(&query.by)?
                    .to_string()
                    .contains("pair_group")
                {
                    ensure!(
                        expected.to_string().contains("pair_group=treatment"),
                        "ranged[{n}]: the pair's strata"
                    );
                }
                ensure!(
                    expected["targets"]
                        .as_array()
                        .is_some_and(|t| !t.is_empty()),
                    "ranged[{n}]: the targets judged"
                );
                let mut misses = Vec::new();
                differences(&got, &expected, &format!("ranged[{n}]"), &mut misses);
                ensure!(misses.is_empty(), "{query:?}: {misses:?}");
                answered.push(got);
                read
            }
            (Err(got), Err(expected)) => {
                ensure!(
                    got.to_string() == expected.to_string(),
                    "ranged[{n}] {query:?}: {got} of {expected}"
                );
                answered.push(json!({"refused": got.to_string()}));
                Returned::default()
            }
            (got, expected) => anyhow::bail!(
                "ranged[{n}] {query:?}: {:?} of {:?}",
                got.map(|_| ()),
                expected.map(|_| ())
            ),
        };
        returned.push((read, expect));
    }
    Ok(returned)
}

/// The questions `kpi` reads by the range [`crate::domain::kpi::range::plan`]
/// decides past the periods' (whose read from a time
/// [`crate::domain::kpi::read_from_ms`] had none): `--since` / `--until` of
/// a time and of an event id, `--at` an event id, `--goal`, `--by area`,
/// and `--compare` at a time and of two windows, with `--goal` too. The
/// event ids are those of `whole`'s events at the times, so that each
/// history asks of its own ids.
fn kpi_ranged(whole: &LedgerWindow) -> Vec<KpiQuery> {
    use crate::domain::kpi::{CompareSpec, Period};
    let time = |secs: i64| Cursor::Time((T0 + secs) * 1000);
    let id = |secs: i64| Cursor::Event(time(secs).event_id(whole.events()));
    let day_two = KpiQuery {
        period: Period::Day,
        last: 2,
        ..KpiQuery::default()
    };
    let goal = landed_goal(whole);
    vec![
        KpiQuery {
            since: Some(time(SINCE)),
            until: Some(time(NEAR + 2000)),
            ..KpiQuery::default()
        },
        KpiQuery {
            since: Some(id(SINCE)),
            by: vec!["area".parse().unwrap()],
            ..KpiQuery::default()
        },
        KpiQuery {
            since: Some(time(SINCE)),
            until: Some(id(NEAR)),
            ..KpiQuery::default()
        },
        KpiQuery {
            at: Some(id(NEAR)),
            ..day_two.clone()
        },
        KpiQuery {
            period: Period::Week,
            last: 1,
            at: Some(id(NEAR)),
            ..KpiQuery::default()
        },
        KpiQuery {
            goal_id: goal,
            ..day_two.clone()
        },
        KpiQuery {
            since: Some(time(SINCE)),
            goal_id: goal,
            by: vec!["provider".parse().unwrap()],
            ..KpiQuery::default()
        },
        KpiQuery {
            compare: Some(CompareSpec::At(time(NEAR))),
            ..day_two.clone()
        },
        KpiQuery {
            compare: Some(CompareSpec::Windows([
                (time(SINCE - DAY), time(SINCE)),
                (time(SINCE), id(NEAR)),
            ])),
            ..day_two.clone()
        },
        KpiQuery {
            compare: Some(CompareSpec::At(id(NEAR))),
            goal_id: goal,
            ..day_two.clone()
        },
        KpiQuery {
            by: vec!["pair_group".parse().unwrap()],
            ..day_two
        },
    ]
}

/// `kpi` itself (`crate::application::kpi::kpi`, the entry the CLI and the
/// queue service call) on `store`, laid down with the named history and
/// committed: each question of `kpi_asked` and [`kpi_ranged`] answers as
/// every row, and the entry reads by the range as [`counted_kpi`] does.
#[cfg(test)]
pub(crate) fn kpi_entry_answers_as_every_row<S, F>(open: &F) -> Result<()>
where
    S: ContractStore + crate::application::LedgerStore,
    F: Fn() -> Result<S>,
{
    let store = open()?;
    let tx = store.write_entry("contract_kpi_entry")?;
    lay_down_named(&tx, &kpi_named_history(30))?;
    S::commit(tx)?;
    let tx = store.write_entry("contract_kpi_entry_read")?;
    let whole = view::window(&tx, &whole_query())?;
    let mut asked = kpi_asked();
    asked.extend(kpi_ranged(&whole));
    let mut expected = Vec::new();
    for (n, query) in asked.iter().enumerate() {
        let (_, read) = counted_kpi(&tx, query)?;
        ensure!(
            read.views && read.by_range > 0,
            "[{n}] by its range: {read:?}"
        );
        expected.push(serde_json::to_value(kpi_of_window(
            &whole,
            NOW,
            HOST,
            &config(),
            Some(&areas()),
            &KpiQuery {
                read_whole_ledger: true,
                ..query.clone()
            },
        )?)?);
    }
    drop(tx);
    for (n, (query, expected)) in asked.iter().zip(&expected).enumerate() {
        let got = serde_json::to_value(crate::application::kpi::kpi(
            &store,
            NOW,
            HOST,
            &config(),
            Some(&areas()),
            query,
        )?)?;
        let mut misses = Vec::new();
        differences(&got, expected, &format!("entry[{n}]"), &mut misses);
        ensure!(misses.is_empty(), "{query:?}: {misses:?}");
    }
    Ok(())
}

/// The goal of every other task of [`chain_history`]'s chain.
const CHAIN_GOAL: i64 = 990;

/// The marks of [`chain_history`]'s chain: five, two days apart from 170
/// days after [`T0`] (the times after it, in seconds).
const CHAIN_MARKS: [i64; 5] = [170 * DAY, 172 * DAY, 174 * DAY, 176 * DAY, 178 * DAY];

/// How many runs land in each span of the chain: before its first mark,
/// between each two marks and after the last. A group joins two marks
/// while fewer than `min_samples` (5) runs finished between them, so the
/// gaps of 4 join and those of 5 split: groups {0, 1} and {2, 3}, and {4}
/// alone. Of `--goal` [`CHAIN_GOAL`], every other run, the gaps are 2 and
/// 3 and every mark is in one group, which crosses both ends of each
/// comparison's windows; the 5 of its runs before the first mark keep the
/// settled units' marks (of no run of the goal between them) out of it.
const CHAIN_FINISHES: [usize; 6] = [10, 4, 5, 4, 5, 5];

/// [`kpi_named_history`] of `settled` units with a chain of marks and landed
/// runs between them (domain's `chain` of the cohort's test, laid down as
/// the store holds it), put among its events by time.
fn chain_history(settled: usize) -> Log {
    let mut chain = Log::default();
    let mut n: u8 = 0;
    for (span, finishes) in CHAIN_FINISHES.iter().enumerate() {
        let start = if span == 0 {
            CHAIN_MARKS[0] - DAY
        } else {
            CHAIN_MARKS[span - 1]
        };
        for k in 0..*finishes {
            let task = 1300 + i64::from(n);
            let run = super::ledger_earlier::run(200 + n);
            chain.landed(
                task,
                &run,
                start + 1000 + i64::try_from(k).unwrap_or(0) * 3000,
            );
            if n.is_multiple_of(2) {
                for event in &mut chain.events {
                    if event.task_id.is_some_and(|t| t.as_i64() == task)
                        && event.kind.as_str() == "task_created"
                    {
                        event.payload = json!({"title": format!("t{task}"),
                            "goal_id": CHAIN_GOAL, "status": "ready"});
                    }
                }
            }
            n += 1;
        }
    }
    for (k, at) in CHAIN_MARKS.iter().enumerate() {
        chain.add(
            None,
            None,
            "mark_recorded",
            json!({"label": format!("chain{k}"), "by": "person"}),
            *at,
        );
    }
    let mut events = kpi_named_history(settled).events;
    for (n, e) in chain.events.iter_mut().enumerate() {
        e.id = EventId::new(-1 - i64::try_from(n).unwrap_or(0));
    }
    for e in chain.events {
        let at = events.partition_point(|held| held.created_at <= e.created_at);
        events.insert(at, e);
    }
    renumbered(events)
}

/// `--compare`'s groups read through `kpi`'s entry on [`chain_history`]:
/// the cohort's whole read cuts the groups whose windows the range then
/// reads. At a time and at a mark's event id, of two windows, and each with
/// `--goal` (whose finishes join every mark), the answers are those of an
/// independent read of every row with `small` and `large` units, each
/// reads by its range, and what each reads of the ledger's rows is the same
/// of both (the cohort's reads, of the marks' kinds and the runs' first
/// ends over the whole history, are not counted: [`counted_kpi`]).
pub(crate) fn kpi_compare_groups_with<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    small: usize,
    large: usize,
) -> Result<()> {
    use crate::domain::kpi::{CompareSpec, Period};
    let mut reads = Vec::new();
    let mut answers = Vec::new();
    for settled in [small, large] {
        let store = open()?;
        let tx = store.write_entry("contract_kpi_compare_groups")?;
        let laid = lay_down_named(&tx, &chain_history(settled))?;
        let whole = view::window(&tx, &whole_query())?;
        // The store's events that put a task in a goal give every task's
        // goal as every row's attributes do, those of the chain's tasks
        // that finished before any range among them.
        let goal_events = match view::with_snapshot(&tx, |snapshot| snapshot.goal_events())? {
            WindowRead::Complete(events) => events,
            other => anyhow::bail!("the goals' events: {other:?}"),
        };
        ensure!(
            goal_events
                .iter()
                .all(|e| e.kind.is_in(&attributes::GOAL_KINDS)),
            "only the goals' kinds"
        );
        let every_row = match whole.attributes() {
            attributes::Folded::Attributes(known) => known.task_goals(),
            attributes::Folded::BeforeBaseline => anyhow::bail!("no baseline"),
        };
        let of_goals =
            attributes::task_goals_of(&goal_events, Some(crate::application::stats::now_ms(NOW)));
        ensure!(
            of_goals == every_row,
            "the goals of the events: {} tasks of {}",
            of_goals.len(),
            every_row.len()
        );
        ensure!(
            (0..CHAIN_FINISHES[0]).step_by(2).all(|n| of_goals
                .get(&TaskId::new(1300 + i64::try_from(n).unwrap_or(0)))
                == Some(&Some(GoalId::new(CHAIN_GOAL)))),
            "the goal's tasks before the chain"
        );
        // The runs of each span of the chain are there, and no other ends
        // between its marks.
        let ends: Vec<i64> = whole
            .events()
            .iter()
            .filter(|e| e.kind.as_str() == "run_integrated")
            .filter_map(|e| crate::domain::stats::timestamp_millis(&e.created_at))
            .map(|ms| ms / 1000 - T0)
            .collect();
        for gap in 0..CHAIN_MARKS.len() - 1 {
            let between = ends
                .iter()
                .filter(|at| **at > CHAIN_MARKS[gap] && **at <= CHAIN_MARKS[gap + 1])
                .count();
            ensure!(
                between == CHAIN_FINISHES[gap + 1],
                "{between} runs end between marks {gap} and {}",
                gap + 1
            );
        }
        let time = |secs: i64| Cursor::Time((T0 + secs) * 1000);
        let mark = |k: usize| -> Result<Cursor> {
            whole
                .events()
                .iter()
                .find(|e| {
                    e.kind.as_str() == "mark_recorded" && e.payload["label"] == format!("chain{k}")
                })
                .map(|e| Cursor::Event(e.id))
                .ok_or_else(|| anyhow::anyhow!("chain mark {k}"))
        };
        let day_two = KpiQuery {
            period: Period::Day,
            last: 2,
            ..KpiQuery::default()
        };
        let windows = CompareSpec::Windows([
            (time(CHAIN_MARKS[1] + 100), time(CHAIN_MARKS[2] + 100)),
            (time(CHAIN_MARKS[2] + 100), time(CHAIN_MARKS[3] + 100)),
        ]);
        let goal = Some(GoalId::new(CHAIN_GOAL));
        let questions = [
            KpiQuery {
                compare: Some(CompareSpec::At(time(CHAIN_MARKS[2] + 10))),
                ..day_two.clone()
            },
            KpiQuery {
                compare: Some(CompareSpec::At(mark(3)?)),
                ..day_two.clone()
            },
            KpiQuery {
                compare: Some(windows),
                ..day_two.clone()
            },
            KpiQuery {
                compare: Some(CompareSpec::At(mark(2)?)),
                goal_id: goal,
                ..day_two.clone()
            },
            KpiQuery {
                compare: Some(windows),
                goal_id: goal,
                ..day_two
            },
        ];
        let mut returned = Vec::new();
        let mut answered = Vec::new();
        for (n, query) in questions.iter().enumerate() {
            let expected = serde_json::to_value(kpi_of_window(
                &whole,
                NOW,
                HOST,
                &config(),
                Some(&areas()),
                &KpiQuery {
                    read_whole_ledger: true,
                    ..query.clone()
                },
            )?)?;
            ensure!(!expected["compare"].is_null(), "compare[{n}] compares");
            let (got, read) = counted_kpi(&tx, query)?;
            ensure!(
                read.by_range > 0 && read.views,
                "compare[{n}] by its range: {read:?}"
            );
            // `--goal` reads every task's goal, the goals' events of the
            // whole history and no more (ask 1756: a read of every task as
            // the cohort's of every run's end); without it, none.
            let goal_read = if query.goal_id.is_some() {
                goal_events.len()
            } else {
                0
            };
            ensure!(
                read.goal_events == goal_read,
                "compare[{n}] reads {} goals' events of {goal_read}",
                read.goal_events
            );
            let mut misses = Vec::new();
            differences(&got, &expected, &format!("compare[{n}]"), &mut misses);
            ensure!(misses.is_empty(), "{query:?}: {misses:?}");
            returned.push(read);
            answered.push(got);
        }
        reads.push(returned);
        answers.push(answered);
        clear(&tx, &laid)?;
        drop(tx);
    }
    // The ledger's rows and the small reads are the same of both sizes; the
    // goals' events, of every task, are those the history holds (above).
    for (n, (a, b)) in reads[0].iter().zip(&reads[1]).enumerate() {
        ensure!(
            a.of_the_range() == b.of_the_range(),
            "compare[{n}] returns {a:?} of {small} units and {b:?} of {large}"
        );
    }
    // The settled units are before the chain: each comparison answers of
    // the large history as of the small one, as well as as every row.
    super::same_answers(&answers[0], &answers[1], small, large)
}

/// The goal the most landed runs of `whole` are of (by its attributes'
/// `task_goals`), so that `--goal` keeps some and leaves some out.
fn landed_goal(whole: &LedgerWindow) -> Option<GoalId> {
    let attributes::Folded::Attributes(known) = whole.attributes() else {
        return None;
    };
    let goals = known.task_goals();
    let mut counts: std::collections::BTreeMap<GoalId, usize> = Default::default();
    for event in whole.events() {
        if event.kind == crate::domain::EventKind::RunIntegrated
            && let Some(goal) = event
                .task_id
                .and_then(|task| goals.get(&task).copied().flatten())
        {
            *counts.entry(goal).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(goal, _)| goal)
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
    compare_named_histories(open, small, large, kpi_named_history)
}

fn compare_named_histories<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    small: usize,
    large: usize,
    history: fn(usize) -> Log,
) -> Result<()> {
    let mut reads = Vec::new();
    let mut answers = Vec::new();
    for settled in [small, large] {
        let store = open()?;
        let tx = store.write_entry("contract_ledger_queue_named")?;
        let laid = lay_down_named(&tx, &history(settled))?;
        ensure!(views::usable(&tx)?, "the views are trusted");
        the_cases_are_there(&tx)?;
        let mut answered = Vec::new();
        let returned = named_as_every_row(&tx, settled == small, &mut answered)?;
        answers.push(answered);
        // Of trusted views, each question not refused reads by its range (a
        // store whose views or visibility cannot be trusted reads every row
        // as it may: `reads_as`).
        for (n, (read, expect)) in returned.iter().enumerate() {
            if *expect == Expect::Range {
                ensure!(read.by_range > 0, "[{n}] reads by its range: {read:?}");
            }
        }
        the_seeks_find_the_cases(&tx)?;
        for (n, (read, _)) in returned
            .iter()
            .enumerate()
            .filter(|(_, (_, expect))| *expect == Expect::Range)
        {
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
    let asked_of_both = |n: usize| matches!(n, 0 | 3 | 4 | 5) || n >= 7;
    let small_reads: Vec<(Returned, Expect)> = reads[0]
        .iter()
        .enumerate()
        .filter(|(n, _)| asked_of_both(*n))
        .map(|(_, read)| *read)
        .collect();
    ensure!(small_reads.len() == reads[1].len());
    // The settled units are before every range: each question answers of
    // the large history as of the small one, as well as as every row.
    let small_answers: Vec<Value> = answers[0]
        .iter()
        .enumerate()
        .filter(|(n, _)| asked_of_both(*n))
        .map(|(_, answer)| answer.clone())
        .collect();
    super::same_answers(&small_answers, &answers[1], small, large)?;
    // What a question must read is the same of both, and so is what it
    // reads by its range (the goals' events of `--compare --goal` aside:
    // [`Returned::goal_events`], checked by [`kpi_compare_groups_with`]).
    for (n, ((a, x), (b, y))) in small_reads.iter().zip(&reads[1]).enumerate() {
        ensure!(
            x == y,
            "[{n}] must read {x:?} of {small} units and {y:?} of {large}"
        );
        ensure!(
            *x != Expect::Range || a.of_the_range() == b.of_the_range(),
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

pub(crate) fn ci_only_history(settled: usize) -> Log {
    let mut log = named_history(super::fixed_history_units());
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

pub(crate) fn clock_history(settled: usize) -> Log {
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
    for (n, (read, _)) in named_as_every_row(tx, true, &mut Vec::new())?
        .iter()
        .enumerate()
        .filter(|(_, (_, expect))| *expect == Expect::Range)
    {
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
/// Each state is one of [`WHOLE_STATES`], laid on a history of its own
/// (one fresh, trusted history each) by
/// [`ledger_queue_named_goes_back_whole_of`]; [`super::run`] runs them one
/// after another, and a store's tests may run them apart and in parallel.
#[derive(Clone, Copy, Debug)]
pub enum WholeState {
    /// No row of the views' mark; marked again, by the views.
    NoMark,
    /// A mark of no generation; marked again, by the views.
    NoGeneration,
    /// A mark of another generation; marked again, by the views.
    AnotherGeneration,
    /// A mark filled part way (through the first queue row); marked again,
    /// by the views.
    PartWay,
    /// Main's latest summary as an older binary wrote it, with no record;
    /// once filled again from the first row, by the views.
    SummaryWithoutRecord,
    /// The record of main's latest summary without its `ci_by_time` (an
    /// older clock representation); once filled again, by the views.
    NoCiByTime,
    /// The record without its `main_by_time`; once filled again, by the
    /// views.
    NoMainByTime,
    /// No marks of the rows (the visibility of what is sought cannot be
    /// judged); once they are written again, by the views.
    NoRowMarks,
    /// No tables of the views (a schema before them).
    NoTables,
}

/// Every [`WholeState`], in the order [`super::run`] runs them.
pub const WHOLE_STATES: &[WholeState] = &[
    WholeState::NoMark,
    WholeState::NoGeneration,
    WholeState::AnotherGeneration,
    WholeState::PartWay,
    WholeState::SummaryWithoutRecord,
    WholeState::NoCiByTime,
    WholeState::NoMainByTime,
    WholeState::NoRowMarks,
    WholeState::NoTables,
];

/// The case of `state` alone ([`WholeState`]): on a fresh
/// history whose views are filled, the store left in `state` reads the
/// kinds of the views whole and answers as every row, and once trusted
/// again reads by the views and answers so (of no tables, the history as
/// laid down reads by the views before they go). Its writes roll back.
pub fn ledger_queue_named_goes_back_whole_of<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    state: WholeState,
) -> Result<()> {
    ledger_queue_named_goes_back_whole_with(open, state, 30)
}

/// The same fallback states with an explicit settled history size.
pub(crate) fn ledger_queue_named_goes_back_whole_with<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    state: WholeState,
    settled: usize,
) -> Result<()> {
    let store = open()?;
    let tx = store.write_entry("contract_ledger_queue_named_whole")?;
    let laid = lay_down_named(&tx, &named_history(settled))?;
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
    // Main's latest summary with `edit` made to it, as an older binary
    // wrote it.
    let older_summary = |edit: &dyn Fn(&mut Value)| -> Result<()> {
        let latest: String = tx.query_row(
            "SELECT summary FROM ledger_main_summaries ORDER BY through DESC LIMIT 1",
            sql_params![],
            |r| r.get(0),
        )?;
        let mut old: Value = serde_json::from_str(&latest)?;
        edit(&mut old);
        tx.execute(
            "UPDATE ledger_main_summaries SET summary=?1 WHERE through=(SELECT MAX(through) FROM ledger_main_summaries)",
            sql_params![old.to_string()],
        )?;
        Ok(())
    };
    match state {
        WholeState::NoMark => {
            tx.execute("DELETE FROM ledger_queue_views_filled", sql_params![])?;
            reads_as(&tx, false)?;
            mark(None, generation)?;
        }
        WholeState::NoGeneration => {
            mark(None, None)?;
            reads_as(&tx, false)?;
            mark(None, generation)?;
        }
        WholeState::AnotherGeneration => {
            mark(None, Some(generation.unwrap_or(0) + 7))?;
            reads_as(&tx, false)?;
            mark(None, generation)?;
        }
        WholeState::PartWay => {
            let first: String = tx.query_row(
                "SELECT MIN(key) FROM ledger_rows WHERE kind='queue'",
                sql_params![],
                |r| r.get(0),
            )?;
            mark(Some(&first), generation)?;
            reads_as(&tx, false)?;
            mark(None, generation)?;
        }
        // Summaries an older binary wrote hold no record: whole, until the
        // filling makes them again from the first row.
        WholeState::SummaryWithoutRecord => {
            older_summary(&|old| {
                old.as_object_mut().unwrap().remove("record");
            })?;
            reads_as(&tx, false)?;
            fill_views(&tx)?;
        }
        // Either older clock representation is untrusted even with a record.
        WholeState::NoCiByTime | WholeState::NoMainByTime => {
            let field = if matches!(state, WholeState::NoCiByTime) {
                "ci_by_time"
            } else {
                "main_by_time"
            };
            older_summary(&|old| {
                old["record"].as_object_mut().unwrap().remove(field);
            })?;
            reads_as(&tx, false)?;
            fill_views(&tx)?;
        }
        WholeState::NoRowMarks => {
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
        }
        // Of the history as laid down it reads by the views (the other
        // states read so once trusted again).
        WholeState::NoTables => {
            reads_as(&tx, true)?;
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
            return Ok(());
        }
    }
    reads_as(&tx, true)?;
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
    ledger_queue_named_past_an_old_writer_of(open, Checks::Run)
}

/// [`ledger_queue_named_past_an_old_writer`] with its reads checked, or its
/// writes alone (`checks`).
pub(crate) fn ledger_queue_named_past_an_old_writer_of<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    checks: Checks,
) -> Result<()> {
    let (first, a) = taken_over(open, None, "queue-named-1")?;
    let tx = first.write_entry("contract_queue_named_init")?;
    let laid = lay_down_named(&tx, &named_history(30))?;
    checks.check(|| reads_as(&tx, true))?;
    let mark_of = |tx: &S::Write<'_>| -> Result<Vec<(Option<String>, Option<i64>)>> {
        Ok(tx.query_map(
            "SELECT through, generation FROM ledger_queue_views_filled",
            sql_params![],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    };
    let before = checks.read(|| {
        Ok((
            mark_of(&tx)?,
            view::with_snapshot(&tx, |snapshot| snapshot.ledger_marks())?,
        ))
    })?;
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
    if let Some((mark, r)) = before {
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
    }
    S::commit(tx)?;

    let (current, c) = taken_over(open, Some(("queue-named-old", b)), "queue-named-current")?;
    let tx = current.write_entry("contract_queue_named_current")?;
    checks.check(|| reads_as(&tx, false))?;
    fill_views(&tx)?;
    checks.check(|| {
        ensure!(views::usable(&tx)?);
        reads_as(&tx, true)
    })?;
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
