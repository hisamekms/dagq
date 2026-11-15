//! `kpi` (ADR-0051 decision 9): the queue's reads that
//! [`crate::domain::kpi::kpi`] derives the KPIs from. Reads only.
use std::cell::RefCell;
use std::collections::HashMap;

use anyhow::{Result, anyhow};

use super::{LedgerStore, PlannedRead, WindowRead};
use crate::domain::areas::{AreaMap, run_areas};
use crate::domain::kpi::range::{self, Cohort, Plan, RangeReads, Read, Undecided};
use crate::domain::kpi::{
    HostReader, Kpi, KpiConfig, KpiInput, KpiQuery, bounds_with as kpi_bounds, kpi_with as derive,
};
use crate::domain::ledger::attributes::{self, Folded};
use crate::domain::ledger::window::{
    LedgerWindow, WindowQuery,
    bounds::{Bound, DeclareBounds, Declared},
};
use crate::domain::{EventId, GoalId, RunEvent, TaskId};

/// Where the host is: the time zone at `now` (seconds east of UTC) and the
/// logical cores; and whether the profiles of the queue's repository
/// record the Rust toolchain
/// ([`crate::domain::toolchain::Toolchains::records_rust_toolchain`]), for
/// the `toolchain` axis.
#[derive(Debug, Clone, Copy)]
pub struct Host {
    pub utc_offset_secs: i64,
    pub cores: Option<usize>,
    pub rust_toolchain: bool,
}

/// The KPIs `query` asks for, at the unix second `now`, judged by
/// `config`, from the ledger's rows alone (ADR-t1662-2 D1): the events the
/// rows own, the tasks' goals, changes and drafts' origins of the
/// attribute records, the landed runs' areas `areas` (`[areas]`) gives
/// main's history the supervisor recorded, the supervisors' lives of
/// their own records, each window's host load of the node rows, and the
/// reverts and CI failures after the landings the window's common read
/// matches to the runs ([`LedgerWindow::post_landing_book`]), and the
/// Executions its rows keep ([`LedgerWindow::executions`]).
///
/// # The range it reads
///
/// The window is read of one snapshot ([`super::ledger_window_planned`]),
/// from the start of the range [`range::plan`] decides of `query`, now and
/// `config` ([`WindowQuery::read_from_ms`]; the store's read from a time,
/// whose doc at [`LedgerStore::ledger_window`] says what of before it is
/// still read), for `--period` and `--last`, `--at` (a time or an event
/// id), `--since` / `--until`, `--by` and `--goal` alike; the answer is
/// that of every row. An event id's time is the one the window's bound of
/// it answers, read of the same snapshot ([`PlannedRead::cursor_time`]).
/// No upper bound is read by: the window reaches now, as a read from a
/// time does. Of the forecast's KPIs, what a target that finishes in the
/// range looks back to (its snapshots and the marks recorded after them)
/// is the store's to give with the range (its read of the snapshots and
/// marks of before it), so a snapshot older than the range brings the
/// marks between it and the range: that read grows with how long the
/// target was open, the one read here that is not bounded by the range.
///
/// `--compare` (`At` and `Windows`, with `--goal` too) cuts its groups of
/// the cohort ([`range::cohort`]): every event of the marks' kinds and
/// every run's first end, read whole by kind and not by the ledger's rows
/// (Spike 4135's (v): a group's extent depends on the marks and finishes
/// outside any range), so that read grows with the marks and runs of the
/// whole history while the ledger's rows read stay those of the range;
/// the windows the groups give join the range. With `--goal`, the finishes
/// are those of the tasks in the goal by every task's goal, folded of the
/// events that put a task in one ([`super::LedgerWindowRead::goal_events`], read
/// whole by kind as the cohort's, of every task), as a window of every row
/// puts them: a task that finished before the range is in its goal too.
///
/// Every row is read, as before, when the range is undecided: when
/// [`range::plan`] gives [`Undecided`] (`--until` without `--since`,
/// `--last 0`, [`KpiQuery::read_whole_ledger`], a refused span, an event id
/// that names no event or whose time the snapshot's reads cannot order or
/// find, a walk to the latest record at or before now past one of a clock
/// ahead included, and a small read that could not answer), and when a
/// comparison's run ends first past now. The cohort holds no mark or
/// goal's event written past now, as a window to now holds none. Which of
/// the queue's rows a read from a time still reads every one of is the
/// store's to decide by its own tables and marks (a store without them or
/// whose marks do not stand, or whose visibility cannot be judged).
pub fn kpi(
    queue: &(impl LedgerStore + ?Sized),
    now: i64,
    host: Host,
    config: &KpiConfig,
    areas: Option<&AreaMap>,
    query: &KpiQuery,
) -> Result<Kpi> {
    let (window, cohort) = super::ledger_window_planned(queue, |read| {
        planned_window(read, now, host, config, areas, query)
    })?;
    of_window_with(&window, now, host, config, areas, query, cohort.as_ref())
}

/// The window [`kpi`] reads of `read` for `query`, and the cohort of its
/// comparison.
fn planned_window(
    read: &PlannedRead<'_>,
    now: i64,
    host: Host,
    config: &KpiConfig,
    areas: Option<&AreaMap>,
    query: &KpiQuery,
) -> Result<(LedgerWindow, Option<Cohort>)> {
    let now_ms = super::stats::now_ms(now);
    let reads = Ranges::new(read, now_ms);
    let window_query = |from: Option<i64>, cohort: Option<&Cohort>| WindowQuery {
        now_ms,
        read_from_ms: from,
        bounds: declared_with(vec![query.clone()], now, host, config, areas, cohort),
        ..WindowQuery::default()
    };
    let plan = |query: &KpiQuery, goals: &HashMap<TaskId, Option<GoalId>>| {
        let plan = range::plan(query, config, now, host.utc_offset_secs, &reads, goals);
        reads.failed()?;
        Ok::<_, anyhow::Error>(plan.ok())
    };
    let whole = || Ok((read.window(&window_query(None, None))?, None));
    // `--goal`'s cohort counts the finishes of the goal's tasks, whenever
    // they finished: every task's goal, of the events that put it there.
    let goals = if query.compare.is_some() && query.goal_id.is_some() {
        match reads.task_goals() {
            Some(goals) => goals,
            None => {
                reads.failed()?;
                return whole();
            }
        }
    } else {
        HashMap::new()
    };
    let Some(Plan { range, cohort }) = plan(query, &goals)? else {
        return whole();
    };
    let window = read.window(&window_query(Some(range.from_ms), cohort.as_ref()))?;
    Ok((window, cohort))
}

/// The small reads [`range::plan`] takes of a snapshot: an event id's time
/// ([`PlannedRead::cursor_time`]) and the cohort's (i) and (h), each read
/// once, of the events a window of every row to now holds (none written
/// past `now_ms`). The plan reads nothing else of them (its range's marks and the
/// forecast's are the window's to read). A read that failed leaves the
/// range undecided, its error kept for [`Self::failed`].
struct Ranges<'a, 'b> {
    read: &'a PlannedRead<'b>,
    now_ms: i64,
    error: RefCell<Option<anyhow::Error>>,
    comparison: RefCell<Option<Vec<RunEvent>>>,
    finishes: RefCell<Option<Vec<(TaskId, i64)>>>,
}

impl<'a, 'b> Ranges<'a, 'b> {
    fn new(read: &'a PlannedRead<'b>, now_ms: i64) -> Self {
        Self {
            read,
            now_ms,
            error: RefCell::new(None),
            comparison: RefCell::new(None),
            finishes: RefCell::new(None),
        }
    }

    /// Every task's goal, as a window of every row to now folds it
    /// ([`attributes::task_goals_of`] at now: a record past now is not of
    /// it, a baseline's records go with their group's mark) of the events
    /// that put a task in one ([`super::LedgerWindowRead::goal_events`]);
    /// `None` when the read could not answer (its error kept for
    /// [`Self::failed`]).
    fn task_goals(&self) -> Option<HashMap<TaskId, Option<GoalId>>> {
        let events = self
            .of(self.read.snapshot().goal_events(), Undecided::Fallback)
            .ok()?;
        Some(attributes::task_goals_of(&events, Some(self.now_ms)))
    }

    /// The error of a read that failed, if one did.
    fn failed(&self) -> Result<()> {
        self.error.borrow_mut().take().map_or(Ok(()), Err)
    }

    /// `read`'s answer, or why the range is undecided.
    fn of<T>(&self, read: Result<WindowRead<T>>, undecided: Undecided) -> Read<T> {
        match read {
            Ok(WindowRead::Complete(value)) => Ok(value),
            Ok(WindowRead::Exhausted | WindowRead::OffTimeForm) => Err(undecided),
            Ok(WindowRead::Unavailable) => Err(Undecided::Fallback),
            Err(error) => {
                self.error.borrow_mut().get_or_insert(error);
                Err(Undecided::Fallback)
            }
        }
    }
}

impl RangeReads for Ranges<'_, '_> {
    fn recorded_effective(&self, _: i64, _: i64) -> Read<Vec<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn retractions_of(&self, _: EventId) -> Read<Vec<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn claims_written(&self, _: i64, _: i64) -> Read<Vec<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn previous_claim_with(&self, _: &str, _: EventId) -> Read<Option<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn start_announcing(&self, _: &str, _: &str, _: EventId, _: EventId) -> Read<Option<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn start_before_in_time(&self, _: i64) -> Read<Option<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn cursor_time(&self, cursor: EventId) -> Read<Option<i64>> {
        self.of(
            self.read.cursor_time(cursor, self.now_ms),
            Undecided::OutOfTimeOrder(cursor),
        )
    }
    fn finishes_written(&self, _: i64, _: i64) -> Read<Vec<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn previous_finish(&self, _: &RunEvent) -> Read<Option<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn first_of_target(&self, _: &RunEvent) -> Read<Option<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn snapshots_between(&self, _: EventId, _: EventId) -> Read<Vec<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn plan_changes_written(&self, _: i64, _: i64) -> Read<Vec<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn marks_after(&self, _: EventId) -> Read<Vec<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn run_finishes(&self) -> Read<Vec<(TaskId, i64)>> {
        if let Some(finishes) = self.finishes.borrow().as_ref() {
            return Ok(finishes.clone());
        }
        let finishes: Vec<(TaskId, i64)> = self
            .of(self.read.snapshot().run_finishes(None), Undecided::Fallback)?
            .into_iter()
            .map(|finish| (finish.task, finish.millis))
            .collect();
        // A window of every row holds no event written past now: a run whose
        // first end is past it may end within it by a later end, which this
        // read does not give, so every row is read.
        if finishes.iter().any(|(_, ms)| *ms > self.now_ms) {
            return Err(Undecided::Fallback);
        }
        *self.finishes.borrow_mut() = Some(finishes.clone());
        Ok(finishes)
    }
    fn comparison_events(&self) -> Read<Vec<RunEvent>> {
        if let Some(events) = self.comparison.borrow().as_ref() {
            return Ok(events.clone());
        }
        // Of those a window of every row holds: none written past now.
        let mut events = self.of(
            self.read.snapshot().comparison_events(),
            Undecided::Fallback,
        )?;
        events.retain(|event| {
            crate::domain::stats::timestamp_millis(&event.created_at)
                .is_some_and(|ms| ms <= self.now_ms)
        });
        *self.comparison.borrow_mut() = Some(events.clone());
        Ok(events)
    }
}

/// The bounds [`of_window`] of each of `queries` takes inside the window
/// ([`crate::domain::kpi::bounds`]), for the read of its window to answer
/// ([`WindowQuery::bounds`]): each reader of the KPIs (`kpi`, `report`,
/// the check of the targets, the observer's input) declares those of the
/// queries it makes of the window it reads.
pub fn declared(
    queries: Vec<KpiQuery>,
    now: i64,
    host: Host,
    config: &KpiConfig,
    areas: Option<&AreaMap>,
) -> Declared {
    declared_with(queries, now, host, config, areas, None)
}

/// [`declared`] of queries whose comparisons cut their groups of `cohort`
/// ([`of_window_with`]).
fn declared_with(
    queries: Vec<KpiQuery>,
    now: i64,
    host: Host,
    config: &KpiConfig,
    areas: Option<&AreaMap>,
    cohort: Option<&Cohort>,
) -> Declared {
    Declared::of(KpiBounds {
        queries,
        now,
        host,
        config: config.clone(),
        areas: areas.cloned(),
        cohort: cohort.cloned(),
    })
}

/// [`declared`]'s declaration.
struct KpiBounds {
    queries: Vec<KpiQuery>,
    now: i64,
    host: Host,
    config: KpiConfig,
    areas: Option<AreaMap>,
    cohort: Option<Cohort>,
}

impl DeclareBounds for KpiBounds {
    fn bounds(&self, window: &LedgerWindow) -> Vec<Bound> {
        let mut bounds = Vec::new();
        for query in &self.queries {
            // Only a comparison reads what the input holds past the events
            // (its marks and finishes); the periods read the events alone.
            if query.compare.is_some() {
                with_input(
                    window,
                    self.now,
                    self.host,
                    &self.config,
                    self.areas.as_ref(),
                    |input| bounds.extend(kpi_bounds(input, query, self.cohort.as_ref())),
                );
            } else {
                bounds.extend(kpi_bounds(
                    &KpiInput {
                        events: window.events(),
                        goals: &std::collections::HashMap::new(),
                        changes: &std::collections::HashMap::new(),
                        areas: None,
                        heartbeats: &std::collections::HashMap::new(),
                        draft_origins: &std::collections::HashMap::new(),
                        now: self.now,
                        utc_offset_secs: self.host.utc_offset_secs,
                        cores: self.host.cores,
                        rust_toolchain: self.host.rust_toolchain,
                        config: &self.config,
                        host: None,
                        ledger: None,
                        rows: &[],
                        post_landing: None,
                        executions: None,
                        earlier: None,
                        answers: window.answers(),
                    },
                    query,
                    None,
                ));
            }
        }
        bounds
    }
}

/// [`kpi`] of `window`, the ledger up to `now` (all of it, or read from the
/// time of [`crate::domain::kpi::read_from_ms`] for `query`). Its bounds
/// inside the window are the window's answers when its query declared them
/// ([`declared`]).
pub fn of_window(
    window: &LedgerWindow,
    now: i64,
    host: Host,
    config: &KpiConfig,
    areas: Option<&AreaMap>,
    query: &KpiQuery,
) -> Result<Kpi> {
    of_window_with(window, now, host, config, areas, query, None)
}

/// [`of_window`] whose comparison cuts its groups of `cohort`
/// ([`crate::domain::kpi::kpi_with`]); `None` is [`of_window`].
fn of_window_with(
    window: &LedgerWindow,
    now: i64,
    host: Host,
    config: &KpiConfig,
    areas: Option<&AreaMap>,
    query: &KpiQuery,
    cohort: Option<&Cohort>,
) -> Result<Kpi> {
    with_input(window, now, host, config, areas, |input| {
        derive(input, query, cohort)
    })
    .map_err(|error| anyhow!(error))
}

/// `f` of the input of the KPIs of `window`.
fn with_input<T>(
    window: &LedgerWindow,
    now: i64,
    host: Host,
    config: &KpiConfig,
    areas: Option<&AreaMap>,
    f: impl FnOnce(&KpiInput<'_>) -> T,
) -> T {
    let events = window.events();
    let run_areas = areas.map(|map| run_areas(map, events, &window.main_log().changed));
    let (goals, changes, draft_origins) = match window.attributes() {
        Folded::Attributes(known) => (
            known.task_goals(),
            known.task_changes(),
            known.draft_origins(),
        ),
        Folded::BeforeBaseline => Default::default(),
    };
    let heartbeats = crate::domain::supervisor_life::heartbeats(events, now);
    let ledger = window.run_times();
    let read = |from: i64, until: i64| window.host_between(from * 1000 - 1000, until * 1000);
    let rows: Vec<&crate::domain::ledger::LedgerRow> =
        window.of(crate::domain::ledger::LedgerKind::Run).collect();
    let post_landing = window.post_landing_book();
    let executions = window.executions();
    f(&KpiInput {
        events,
        goals: &goals,
        changes: &changes,
        areas: run_areas.as_ref(),
        heartbeats: &heartbeats,
        draft_origins: &draft_origins,
        now,
        utc_offset_secs: host.utc_offset_secs,
        cores: host.cores,
        rust_toolchain: host.rust_toolchain,
        config,
        host: Some(HostReader(&read)),
        ledger: Some(&ledger),
        rows: &rows,
        post_landing: Some(&post_landing),
        executions: Some(&executions),
        earlier: Some(window.earlier()),
        answers: window.answers(),
    })
}

/// [`improvements`] against `configured`, `[kpi]`'s
/// `max_improvement_proposals` of the bound checkout's `dagq.toml`, or its
/// default for a queue bound to none or a file without it (ADR-0051
/// decision 25).
pub fn improvements_of_setting(
    store: &dyn super::DraftPlannerReads,
    configured: Option<usize>,
) -> Result<serde_json::Value> {
    improvements(
        store,
        configured.unwrap_or(crate::domain::kpi::config::DEFAULT_MAX_IMPROVEMENT_PROPOSALS),
    )
}

/// The improvement proposals running against `limit`, `[kpi]`'s
/// `max_improvement_proposals` (ADR-0051 decision 25), and when they
/// reached it, the findings waiting for a planner because of it (none
/// that is no improvement, [`crate::domain::counts_as_improvement`]: a
/// `ci_failure` or `agent_eval_watch_due` one the supervisor recorded does
/// not wait for it, ADR-t2928-1 and ADR-t2971-1).
pub fn improvements(
    store: &dyn super::DraftPlannerReads,
    limit: usize,
) -> Result<serde_json::Value> {
    let improvements = store.improvements(limit)?;
    let waiting: Vec<serde_json::Value> = if improvements.reached() {
        store
            .planner_findings()?
            .iter()
            .filter(|finding| crate::domain::counts_as_improvement(&finding.kind, &finding.recorded_by))
            .map(|finding| serde_json::json!({"finding_id": finding.id, "reason": "improvement_limit"}))
            .collect()
    } else {
        Vec::new()
    };
    Ok(serde_json::json!({
        "running": improvements.running,
        "limit": improvements.limit,
        "reached": improvements.reached(),
        "waiting": waiting,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::ledger::{
        build,
        testkit::{Log, T0, ms, run_id},
        window::select,
    };
    use crate::domain::stats::Cursor;
    use crate::domain::{EventId, TaskId};

    /// A snapshot of an empty ledger whose cohort's reads give `comparison`
    /// and `finishes`, counting the reads of the rows a read from a time
    /// leaves out.
    #[derive(Default)]
    struct Fake {
        comparison: Vec<crate::domain::RunEvent>,
        finishes: Vec<crate::application::RunFinish>,
        rows_from: std::cell::Cell<usize>,
    }

    impl crate::application::LedgerWindowRead for Fake {
        fn comparison_events(&self) -> Result<WindowRead<Vec<crate::domain::RunEvent>>> {
            Ok(WindowRead::Complete(self.comparison.clone()))
        }
        fn run_finishes(
            &self,
            _: Option<(i64, i64)>,
        ) -> Result<WindowRead<Vec<crate::application::RunFinish>>> {
            Ok(WindowRead::Complete(self.finishes.clone()))
        }
        fn rows_after(
            &self,
            _: crate::domain::ledger::LedgerKind,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<crate::domain::ledger::LedgerRow>> {
            Ok(Vec::new())
        }
        fn rows_from(
            &self,
            _: crate::domain::ledger::LedgerKind,
            _: Option<&str>,
            _: usize,
            _: &crate::domain::ledger::window::RowsFrom,
        ) -> Result<Vec<crate::domain::ledger::LedgerRow>> {
            self.rows_from.set(self.rows_from.get() + 1);
            Ok(Vec::new())
        }
        fn earlier(
            &self,
            _: &crate::domain::ledger::window::RowsFrom,
        ) -> Result<crate::domain::ledger::window::EarlierRecords> {
            Ok(crate::domain::ledger::window::EarlierRecords::default())
        }
        fn row(
            &self,
            _: crate::domain::ledger::LedgerKind,
            _: &str,
        ) -> Result<Option<crate::domain::ledger::LedgerRow>> {
            Ok(None)
        }
        fn run_events(&self, _: &crate::domain::RunId) -> Result<Vec<crate::domain::RunEvent>> {
            Ok(Vec::new())
        }
    }

    const HOST: Host = Host {
        utc_offset_secs: 0,
        cores: None,
        rust_toolchain: false,
    };

    /// A mark of the time `ms`.
    fn mark_at(id: i64, ms: i64) -> crate::domain::RunEvent {
        crate::domain::RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: "mark_recorded".into(),
            payload: json!({"label": format!("m{id}"), "by": "person"}),
            created_at: crate::domain::marks::utc_text(ms),
            actor: None,
        }
    }

    /// A run's first end of the time `ms`.
    fn finish_at(n: u8, ms: i64) -> crate::application::RunFinish {
        crate::application::RunFinish {
            run: crate::domain::RunId::new(run_id(n)).unwrap(),
            task: TaskId::new(i64::from(n)),
            event: EventId::new(i64::from(n)),
            millis: ms,
        }
    }

    /// The cohort's reads hold what a window of every row to now holds:
    /// a mark written past now is left out, and a run's first end past now
    /// (which a later end may put within it) leaves the range undecided.
    #[test]
    fn the_cohort_holds_nothing_written_past_now() {
        let now_ms = super::super::stats::now_ms(T0 + 1000);
        let fake = Fake {
            comparison: vec![mark_at(1, now_ms - 5000), mark_at(2, now_ms + 5000)],
            finishes: vec![finish_at(1, now_ms - 1)],
            ..Fake::default()
        };
        let reads = super::super::planned_in(&fake, 0, &mut |read| {
            let ranges = Ranges::new(read, now_ms);
            let marks = ranges.comparison_events();
            let finishes = ranges.run_finishes();
            Ok((marks, finishes))
        })
        .unwrap();
        assert_eq!(
            reads
                .0
                .unwrap()
                .iter()
                .map(|e| e.id.as_i64())
                .collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(reads.1.unwrap().len(), 1);
        let ahead = Fake {
            finishes: vec![finish_at(1, now_ms - 1), finish_at(2, now_ms + 1)],
            ..Fake::default()
        };
        let finishes = super::super::planned_in(&ahead, 0, &mut |read| {
            Ok(Ranges::new(read, now_ms).run_finishes())
        })
        .unwrap();
        assert_eq!(finishes, Err(Undecided::Fallback));
    }

    /// A question of no range reads every row (no row is left out), and one
    /// of a range reads from its time: `--until` alone and `--whole` the
    /// former, `--since` and the periods the latter.
    #[test]
    fn a_question_of_no_range_reads_every_row() {
        let now = T0 + 10 * 86_400;
        let config = KpiConfig::default();
        let left_out = |query: &KpiQuery| {
            let fake = Fake::default();
            super::super::planned_in(&fake, 0, &mut |read| {
                planned_window(read, now, HOST, &config, None, query).map(|_| ())
            })
            .unwrap();
            fake.rows_from.get()
        };
        let time = |ms: i64| Some(Cursor::Time(ms));
        for query in [
            KpiQuery {
                until: time(ms(5 * 86_400)),
                ..KpiQuery::default()
            },
            KpiQuery {
                read_whole_ledger: true,
                ..KpiQuery::default()
            },
        ] {
            assert_eq!(left_out(&query), 0, "{query:?}");
        }
        for query in [
            KpiQuery {
                since: time(ms(5 * 86_400)),
                ..KpiQuery::default()
            },
            KpiQuery::default(),
        ] {
            assert!(left_out(&query) > 0, "{query:?}");
        }
    }

    /// From the ledger's window to the KPIs: a landed run's row gives its
    /// time, and the window's common read the revert of its commit.
    #[test]
    fn the_window_gives_the_time_and_the_quality_after_landing() {
        let mut log = Log::default();
        log.landed(1, &run_id(1), 0);
        for event in &mut log.events {
            if event.kind.as_str() == "run_integrated" {
                event.payload["result_commit"] = json!("c1");
            }
        }
        log.add(
            None,
            None,
            "main_commits_recorded",
            json!({"after": null, "commits": [
                {"sha": "c1", "at": T0 + 900, "reverts": null},
                {"sha": "r1", "at": T0 + 1500, "reverts": "c1"},
            ]}),
            1600,
        )
        .add(
            None,
            None,
            "main_observed",
            json!({"head": "r1", "at": T0 + 1600, "since": T0 - 1, "reverts": true}),
            1600,
        );
        let now_ms = ms(2000);
        let window = select(
            build(&log.events, now_ms, "2030-01-01T00:00:00.000Z"),
            &WindowQuery {
                until: Some(Cursor::Time(now_ms)),
                now_ms,
                ..WindowQuery::default()
            },
        );
        let kpi = of_window(
            &window,
            now_ms / 1000,
            Host {
                utc_offset_secs: 0,
                cores: None,
                rust_toolchain: false,
            },
            &KpiConfig::default(),
            None,
            &KpiQuery {
                since: Some(Cursor::Time(ms(-1))),
                until: Some(Cursor::Time(now_ms)),
                ..KpiQuery::default()
            },
        )
        .unwrap();
        let kpis = &kpi.periods[0].window.kpis;
        assert_eq!(kpis["post_landing.revert_rate"]["all"].value, Some(1.0));
        assert_eq!(kpis["post_landing.reverts"]["all"].value, Some(1.0));
        assert_eq!(kpis["run_duration"]["all"].n, 1);
        assert_eq!(kpis["run_duration_without_human"]["all"].n, 1);
    }

    /// `kpi` of a window whose end drops events answers as of a queue that
    /// holds only the events up to that end: what is past it reaches no
    /// answer, whichever way the window lends what is within it.
    #[test]
    fn the_window_answers_of_the_events_up_to_its_end() {
        let mut upto = Log::default();
        upto.landed(1, &run_id(1), 0);
        let until = ms(1500);
        let mut more = upto.events.clone();
        let mut past = Log::default();
        past.landed(2, &run_id(2), 2000);
        for mut event in past.events {
            event.id = crate::domain::EventId::new(i64::try_from(more.len()).unwrap() + 1);
            more.push(event);
        }
        // What the store's rows hold at the time of the read, read `until`.
        let now_ms = ms(4000);
        let answer = |events: &[crate::domain::RunEvent]| {
            let window = select(
                build(events, now_ms, "2030-01-01T00:00:00.000Z"),
                &WindowQuery {
                    until: Some(Cursor::Time(until)),
                    now_ms,
                    ..WindowQuery::default()
                },
            );
            assert_eq!(window.events(), upto.events);
            let kpi = of_window(
                &window,
                now_ms / 1000,
                Host {
                    utc_offset_secs: 0,
                    cores: None,
                    rust_toolchain: false,
                },
                &KpiConfig::default(),
                None,
                &KpiQuery {
                    since: Some(Cursor::Time(ms(-1))),
                    until: Some(Cursor::Time(until)),
                    ..KpiQuery::default()
                },
            )
            .unwrap();
            let landed = kpi.periods[0].window.kpis["run_duration"]["all"].n;
            (landed, serde_json::to_value(kpi).unwrap())
        };
        let within = answer(&upto.events);
        assert_eq!(answer(&more), within);
        // The one landing within the end is what it answers of.
        assert_eq!(within.0, 1);
    }

    /// `kpi` of a window folds its events more than once and holds no copy
    /// of their payloads for that.
    #[test]
    fn the_window_gives_the_kpis_with_no_copy_of_the_payloads() {
        const PAD: usize = 16 * 1024;
        let mut log = Log::default();
        log.landed(1, &run_id(1), 0);
        for at in 0..150 {
            log.add(
                None,
                None,
                "main_observed",
                json!({"head": "c", "at": T0 + at, "since": T0 - 1, "pad": "x".repeat(PAD)}),
                at,
            );
        }
        log.events.sort_by_key(|event| event.created_at.clone());
        for (index, event) in log.events.iter_mut().enumerate() {
            event.id = crate::domain::EventId::new(i64::try_from(index).unwrap() + 1);
        }
        let now_ms = ms(2000);
        let window = select(
            build(&log.events, now_ms, "2030-01-01T00:00:00.000Z"),
            &WindowQuery {
                until: Some(Cursor::Time(now_ms)),
                now_ms,
                ..WindowQuery::default()
            },
        );
        let query = KpiQuery {
            since: Some(Cursor::Time(ms(-1))),
            until: Some(Cursor::Time(now_ms)),
            ..KpiQuery::default()
        };
        let peak = crate::domain::ledger::allocation_count::peak_of(|| {
            of_window(
                &window,
                now_ms / 1000,
                Host {
                    utc_offset_secs: 0,
                    cores: None,
                    rust_toolchain: false,
                },
                &KpiConfig::default(),
                None,
                &query,
            )
            .unwrap()
        });
        let payloads = 150 * PAD;
        assert!(peak < payloads, "{peak} of {payloads}");
    }

    /// From the ledger's window to the provider pair's set: a treatment
    /// run claimed on Codex whose review ran on Claude as its group planned
    /// is in its group as assigned and as it ran, with its worker's tokens
    /// from its Codex turn's records and its review's cost as recorded.
    #[test]
    fn the_window_gives_the_pair_groups_tokens_and_cost() {
        let mut log = Log::default();
        let run = run_id(1);
        log.landed(1, &run, 0);
        for event in &mut log.events {
            if event.kind.as_str() == "run_claimed" {
                let pair = json!({"pair_group": "treatment", "pair_trial": "provider-pair",
                    "pair_worker": "codex", "pair_review": "claude"});
                event.payload = json!({"provider": "codex", "requested_provider": "codex",
                    "attributes": pair});
            }
        }
        let tokens = |output: i64, usd: Option<f64>| {
            json!({"input": 0, "output": output, "cache_read": 0, "cache_creation": 0,
                   "cost_usd": usd})
        };
        log.add(Some(1), Some(&run), "turn_started", json!({"turn": 1}), 20)
            .add(
                Some(1),
                Some(&run),
                "turn_finished",
                json!({"turn": 1, "provider": "codex", "tokens": tokens(1_000_000, None),
                       "tokens_source": "token_usage_record", "tokens_by_model": []}),
                300,
            )
            .add(
                Some(1),
                Some(&run),
                "review_job_started",
                json!({"attempt": 1, "agent": null, "launch": {"provider": "claude"}}),
                610,
            )
            .add(
                Some(1),
                Some(&run),
                "review_job_finished",
                json!({"attempt": 1, "agent": null, "provider": "claude",
                       "tokens": tokens(100, Some(0.25)), "tokens_source": "model_usage",
                       "tokens_by_model": []}),
                650,
            );
        log.events.sort_by_key(|event| event.created_at.clone());
        for (index, event) in log.events.iter_mut().enumerate() {
            event.id = crate::domain::EventId::new(i64::try_from(index).unwrap() + 1);
        }
        let now_ms = ms(2000);
        let window = select(
            build(&log.events, now_ms, "2030-01-01T00:00:00.000Z"),
            &WindowQuery {
                until: Some(Cursor::Time(now_ms)),
                now_ms,
                ..WindowQuery::default()
            },
        );
        let config = KpiConfig {
            codex_prices: Some(crate::domain::agent_eval::round::ProviderCost {
                input_usd_per_mtok: Some(2.0),
                output_usd_per_mtok: Some(10.0),
                ..Default::default()
            }),
            ..KpiConfig::default()
        };
        let kpi = of_window(
            &window,
            now_ms / 1000,
            Host {
                utc_offset_secs: 0,
                cores: None,
                rust_toolchain: false,
            },
            &config,
            None,
            &KpiQuery {
                since: Some(Cursor::Time(ms(-1))),
                until: Some(Cursor::Time(now_ms)),
                by: vec!["pair_group".parse().unwrap()],
                ..KpiQuery::default()
            },
        )
        .unwrap();
        let kpis = &kpi.periods[0].window.kpis;
        use crate::domain::kpi::pair;
        for stratum in ["pair_group=treatment", "assigned:pair_group=treatment"] {
            assert_eq!(kpis["landings"][stratum].value, Some(1.0), "{stratum}");
            assert_eq!(kpis[pair::WORKER_TOKENS][stratum].value, Some(1_000_000.0));
            assert_eq!(kpis[pair::WORKER_COST][stratum].value, Some(10.0));
            assert_eq!(kpis[pair::REVIEW_COST][stratum].value, Some(0.25));
            assert_eq!(kpis[pair::LIMIT_WAITS][stratum].value, Some(0.0));
        }
        assert_eq!(
            kpis[pair::SWITCHED_REVIEW]["pair_group=treatment"].value,
            Some(0.0)
        );
    }

    /// A read from a time answers as the read of every row, a day's and a
    /// week's, with strata and without, of what looks back past the time:
    /// the rows left out alone hold the first Execution, the claim before a
    /// change of build, a retried task's first run and what a plan review
    /// predicted.
    #[test]
    fn a_read_from_a_time_answers_as_a_read_of_every_row() {
        use crate::domain::kpi::{Period, read_from_ms};
        use crate::domain::ledger::LedgerRow;
        use crate::domain::ledger::testkit::{
            DAY, READ_AT_DAY, rows_read_from_a_clock_turned_back, rows_read_from_a_time,
        };
        use crate::domain::ledger::window::select_from_time;
        let now = T0 + READ_AT_DAY * DAY;
        let host = Host {
            utc_offset_secs: 0,
            cores: Some(4),
            rust_toolchain: false,
        };
        let config = KpiConfig::default();
        let answer_of = |rows: &[LedgerRow], query: &KpiQuery| {
            let window = select_from_time(
                rows.to_vec(),
                &WindowQuery {
                    now_ms: crate::application::stats::now_ms(now),
                    read_from_ms: read_from_ms(query, &config, now, host.utc_offset_secs),
                    ..WindowQuery::default()
                },
            );
            serde_json::to_value(of_window(&window, now, host, &config, None, query).unwrap())
                .unwrap()
        };
        let rows = rows_read_from_a_time();
        let turned_back = rows_read_from_a_clock_turned_back();
        let answer = |query: &KpiQuery| answer_of(&rows, query);
        let day_two = KpiQuery {
            period: Period::Day,
            last: 2,
            ..KpiQuery::default()
        };
        let questions = [
            day_two.clone(),
            KpiQuery {
                period: Period::Week,
                last: 2,
                ..KpiQuery::default()
            },
            KpiQuery {
                by: vec!["nature".parse().unwrap(), "provider".parse().unwrap()],
                changes: vec!["unknown".to_owned()],
                ..day_two.clone()
            },
            KpiQuery {
                period: Period::Week,
                last: 2,
                by: vec!["nature".parse().unwrap()],
                changes: vec!["unknown".to_owned()],
                ..KpiQuery::default()
            },
            KpiQuery {
                at: Some(Cursor::Time((now - 2 * DAY) * 1000)),
                ..day_two.clone()
            },
        ];
        for query in questions {
            assert!(read_from_ms(&query, &config, now, 0).is_some(), "{query:?}");
            let whole = answer(&KpiQuery {
                read_whole_ledger: true,
                ..query.clone()
            });
            assert_eq!(answer(&query), whole, "{query:?}");
            let whole = answer_of(
                &turned_back,
                &KpiQuery {
                    read_whole_ledger: true,
                    ..query.clone()
                },
            );
            assert_eq!(answer_of(&turned_back, &query), whole, "{query:?}");
        }
        // What only the times of the rows of long ago put in the periods is
        // counted, and what only the ids put out of them is not: the landing
        // recorded before the first event id of its period, and the landing
        // whose attempt starts at the end of one of long ago.
        let yesterday = answer_of(&turned_back, &day_two)["periods"][0].clone();
        let by_actor = &yesterday["details"]["tokens"]["by_actor"];
        assert_eq!(by_actor["worker"]["input"], 11, "{by_actor}");
        assert_eq!(by_actor["inbox"]["input"], 11, "{by_actor}");
        let landings = |answer: &serde_json::Value| answer["kpis"]["landings"]["all"]["n"].as_u64();
        assert_eq!(
            landings(&yesterday),
            landings(&answer(&day_two)["periods"][0])
        );
        let answered = answer(&day_two);
        let tokens = &answered["periods"][1]["details"]["tokens"];
        assert_eq!(tokens["coverage"], json!("full"), "{tokens}");
        assert_eq!(
            tokens["recorded_from"],
            json!(crate::domain::marks::utc_text(ms(2 * DAY + 20)))
        );
        // Three completed, one of them retried across the time.
        let first_pass = &answered["periods"][0]["kpis"]["first_pass_rate"]["all"];
        assert_eq!(first_pass["n"], 3, "{first_pass}");
        assert!(first_pass["value"].as_f64().unwrap() < 1.0, "{first_pass}");
        let marks: Vec<&str> = answered["periods"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|period| period["marks"].as_array().unwrap())
            .filter_map(|mark| mark["label"].as_str())
            .collect();
        assert!(
            marks.iter().any(|label| label.contains("v1 → v2")),
            "{marks:?}"
        );
        let by_nature = answer(&KpiQuery {
            by: vec!["nature".parse().unwrap()],
            ..day_two
        });
        let landings = &by_nature["periods"][0]["kpis"]["landings"];
        assert!(landings.get("nature=mechanical").is_some(), "{landings}");
    }
}
