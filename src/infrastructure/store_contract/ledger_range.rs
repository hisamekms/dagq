//! The read of the ledger's window by its range against the read of every
//! row, driven identically by SQLite's memdb and the Postgres CI example:
//! with the time S of `--since` (or of `kpi`'s periods) the store gives the
//! rows that can bear on (S, E] and the queue's rows kind by kind
//! ([`crate::domain::ledger::window::queue_from`]), and the answers of
//! `stats` and of [`LedgerWindow::execution_tokens`] are those of every
//! row, of a history whose clocks turned back and drifted too; and what the
//! store returns does not grow with the settled history before the range
//! (docs/plans/ledger-window-earlier-visibility-spike.md, "読みの約束" 1 and
//! 6).
use std::cell::Cell;
use std::collections::HashMap;

use super::ContractStore;
use super::ledger_earlier::{NOW, clear, differences, history, lay_down, run, turn, whole_query};
use crate::application::stats::{StatsSources, of_window as stats_of_window};
use crate::application::{LedgerWindowRead, RunFinish, WindowRead, ledger_window_in};
use crate::domain::ledger::session::written::omitted::{Coverage, Point};
use crate::domain::ledger::testkit::{DAY, Log, READ_AT_DAY, T0};
use crate::domain::ledger::window::queue_from::{BY_RANGE, QueueRead, kind_of, read_of};
use crate::domain::ledger::window::{EarlierRecords, LedgerWindow, RowsFrom, WindowQuery};
use crate::domain::ledger::{LedgerKind, LedgerRow};
use crate::domain::marks::utc_text;
use crate::domain::stats::{Cursor, StatsQuery};
use crate::domain::{EventActor, EventId, RunEvent, RunId};
use crate::infrastructure::runtime_store::ledger_open_in as view;
use crate::infrastructure::sql_exec::SqlRead;
use anyhow::{Result, ensure};
use serde_json::{Value, json};

/// The start S of the range that reaches now: two days before it.
const S: i64 = (READ_AT_DAY - 2) * DAY;
/// The end E of a range: a day before now.
const E: i64 = (READ_AT_DAY - 1) * DAY;
/// A past range, both ends days before the latest events.
const PAST_S: i64 = (READ_AT_DAY - 6) * DAY;
const PAST_E: i64 = (READ_AT_DAY - 4) * DAY;

/// The unix ms of `secs` after [`T0`].
const fn ms(secs: i64) -> i64 {
    (T0 + secs) * 1000
}

const ALERT: &str = r#"{"supervisor": "s9", "judgment": "slots", "kind": "idle"}"#;

fn alert() -> Value {
    serde_json::from_str(ALERT).unwrap_or_default()
}

/// The queue rows of the kinds read by their range, `settled` of each,
/// all ended long before every range: a deferral ended of a task
/// canceled, and an alert started and ended.
fn settled_queue_units(log: &mut Log, settled: usize) {
    let span = 100 * DAY;
    log.add(
        None,
        None,
        "live_alert_baseline",
        json!({"supervisor": "s9", "version": 1}),
        10 * DAY,
    );
    for k in 0..settled {
        let n = i64::try_from(k).unwrap_or(0);
        let t = 20 * DAY + n * span / i64::try_from(settled.max(1)).unwrap_or(1);
        log.add(None, None, "live_alert_started", alert(), t).add(
            None,
            None,
            "live_alert_ended",
            alert(),
            t + 60,
        );
        let task = 500_000 + n;
        log.add(Some(task), None, "task_created", json!({}), t + 100)
            .add(
                Some(task),
                None,
                "claim_deferred",
                json!({"supervisor": "s9", "reason": "paths"}),
                t + 110,
            )
            .add(
                Some(task),
                None,
                "claim_deferral_ended",
                json!({"supervisor": "s9", "why": "paths_free"}),
                t + 150,
            )
            .add(
                Some(task),
                None,
                "task_status_changed",
                json!({"from": "ready", "to": "canceled"}),
                t + 200,
            );
    }
}

/// An inbox's session of no run, opened and closed long before every
/// range with its tokens: the first Execution of `inbox`, which no row read
/// holds, so that `execution_tokens`' `recorded_from`, coverage and
/// providers of it are those of every row only if the read gives it. Its
/// ids are the first of `log`'s (-1 to -3 once [`range_history`] gives
/// them), its tokens and close naming its open.
fn inbox_session(log: &mut Log) {
    let at = 30 * DAY;
    let tokens = json!({"input": 3, "output": 4, "cache_read": 0, "cache_creation": 0});
    log.add(None, None, "session_opened", json!({"kind": "inbox"}), at)
        .add(
            None,
            None,
            "session_tokens",
            json!({"kind": "inbox", "at": utc_text(ms(at + 10)), "final": true,
                   "tokens": tokens, "tokens_source": "transcript", "tokens_by_model": [],
                   "opened_event_id": -1}),
            at + 10,
        )
        .add(
            None,
            None,
            "session_closed",
            json!({"kind": "inbox", "opened_event_id": -1}),
            at + 20,
        );
}

/// What reaches the ranges of the kinds read by their range: an alert
/// started before S that ends in the range and one open to now, and a
/// deferral open at S.
fn queue_cases(log: &mut Log) {
    log.add(None, None, "live_alert_started", alert(), S - 3000)
        .add(None, None, "live_alert_ended", alert(), S + 3000);
    let mut late = alert();
    late["kind"] = json!("late");
    log.add(None, None, "live_alert_started", late.clone(), S - 400)
        .add(None, None, "live_alert_ended", late, S - 200);
    let mut open = alert();
    open["kind"] = json!("busy");
    log.add(None, None, "live_alert_started", open, S + 4000);
    log.add(Some(75), None, "task_created", json!({}), S - 5000)
        .add(
            Some(75),
            None,
            "claim_deferred",
            json!({"supervisor": "s9", "reason": "paths"}),
            S - 4000,
        );
}

/// An event of `kind` on `task` and `run` of the time `secs`, its id given
/// later.
fn event(task: Option<i64>, run: Option<&str>, kind: &str, payload: Value, secs: i64) -> RunEvent {
    RunEvent {
        id: EventId::new(0),
        task_id: task.map(crate::domain::TaskId::new),
        goal_id: None,
        run_id: run.and_then(|run| RunId::new(run).ok()),
        kind: kind.into(),
        payload,
        created_at: utc_text(ms(secs)),
        actor: None,
    }
}

/// The events of `log` landing run `n` of task `n` at `secs`.
fn landed(n: u8, secs: i64) -> Vec<RunEvent> {
    let mut log = Log::default();
    log.landed(i64::from(n), &run(n), secs);
    log.events
}

/// The histories of a clock turned back or drifting, each put among the
/// events of `history` where its ids fall (Spike 4670's (vi) and 4754's
/// (iii-d)):
///
/// - Run 76's records are all of a time after E, yet its ids come before an
///   event of a time before E: its end is at or before
///   `Cursor::Time(E).event_id`, so every row puts it in the period.
/// - Run 77 lands with the latest ids and times in the range, as when a
///   clock turned back just before the read.
/// - An alert started after S by a clock ahead and ended before S, before
///   the last alert ended before S.
/// - Task 78's run, on its way, claimed after E with no Execution on its
///   task's row, and a worker's turn on codex with its `tokens_source`.
fn clock_cases(events: &mut Vec<RunEvent>) {
    let at = events.partition_point(|e| e.created_at <= utc_text(ms(E - 200)));
    let mut ahead = landed(76, E + 3000);
    ahead.push(event(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s9"}),
        E - 100,
    ));
    events.splice(at..at, ahead);
    // An alert started by a clock ahead (after S) and ended before S by a
    // clock right again, before the last alert ended before S.
    let mut skewed = alert();
    skewed["kind"] = json!("skewed");
    let at = events.partition_point(|e| e.created_at <= utc_text(ms(S - 600)));
    events.splice(
        at..at,
        [
            event(None, None, "live_alert_started", skewed.clone(), S + 100),
            event(None, None, "live_alert_ended", skewed, S - 500),
        ],
    );
    events.extend(landed(77, S + 500));
    let r = run(78);
    let mut on_its_way = Log::default();
    on_its_way
        .add(Some(78), None, "task_created", json!({}), E + 400)
        .add(
            Some(78),
            Some(&r),
            "run_claimed",
            json!({"from": "ready", "to": "in_progress", "provider": "codex"}),
            E + 500,
        );
    turn(&mut on_its_way, 78, &r, "codex", E + 600);
    for event in &mut on_its_way.events {
        if event.kind.as_str() == "turn_finished" {
            event.actor = Some(EventActor {
                role: "worker".into(),
                id: r.clone(),
                requested_by: None,
            });
        }
    }
    events.extend(on_its_way.events);
}

/// `events` numbered by their place, each `opened_event_id` following its
/// open's new id.
fn renumbered(mut events: Vec<RunEvent>) -> Log {
    let ids: HashMap<i64, i64> = events
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

/// [`history`] of `settled` units with `settled` queue units of each kind
/// read by its range, the queue's cases and the clocks' cases.
pub(crate) fn range_history(settled: usize) -> Log {
    let mut log = history(settled, 0);
    let mut queue = Log::default();
    inbox_session(&mut queue);
    settled_queue_units(&mut queue, settled);
    queue_cases(&mut queue);
    let mut events = log.events.split_off(0);
    // Ids of their own until they are put among the others by their time.
    for (n, e) in queue.events.iter_mut().enumerate() {
        e.id = EventId::new(-1 - i64::try_from(n).unwrap_or(0));
    }
    for e in queue.events {
        let at = events.partition_point(|held| held.created_at <= e.created_at);
        events.insert(at, e);
    }
    clock_cases(&mut events);
    renumbered(events)
}

const SOURCES: StatsSources<'static> = StatsSources {
    config_file: &|| Ok(None),
    conflicts_file: &|| Ok(None),
    areas: None,
    utc_offset_secs: 0,
    counts_checks: true,
    rust_toolchain: false,
};

/// The questions of `stats` with a time `--since`: a range to now and one
/// to E, a page and in full, a past range, and a range wholly before every
/// event (no event at or before its end).
pub(crate) fn stats_questions() -> Vec<StatsQuery> {
    let time = |secs: i64| Some(Cursor::Time(ms(secs)));
    let query = |since: i64, until: Option<i64>, full: bool| StatsQuery {
        since: time(since),
        until: until.and_then(time),
        full,
        ..StatsQuery::default()
    };
    vec![
        query(S, None, true),
        query(S, Some(E), true),
        query(S, Some(E), false),
        query(PAST_S, Some(PAST_E), true),
        query(-2 * DAY, Some(-DAY), true),
    ]
}

/// The questions of `stats` whose window is cut down before their time
/// `--since`: one past its `--until` and one past now. They read every row
/// ([`crate::application::stats::read_from_ms`]).
pub(crate) fn stats_questions_past_their_end() -> Vec<StatsQuery> {
    let time = |secs: i64| Some(Cursor::Time(ms(secs)));
    vec![
        StatsQuery {
            since: time(E),
            until: time(S),
            full: true,
            ..StatsQuery::default()
        },
        StatsQuery {
            since: time(READ_AT_DAY * DAY + DAY),
            full: true,
            ..StatsQuery::default()
        },
    ]
}

/// The window `stats` asks for `query`, read whole when `whole`.
fn stats_window(query: &StatsQuery, whole: bool) -> WindowQuery {
    let mut window = crate::application::stats::window_query(query, NOW);
    if whole {
        window.read_from_ms = None;
    }
    window
}

/// What a read returns of the store: the run, task, session and node rows,
/// the earlier records and first Executions, and the queue's rows of the
/// kinds read by their range and of those read whole, apart.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Returned {
    pub(crate) rows: usize,
    pub(crate) earlier: usize,
    pub(crate) by_range: usize,
    pub(crate) whole: usize,
}

/// A snapshot that counts what it returns ([`Returned`]), reading through
/// the store's.
struct Counted<'a> {
    snapshot: &'a dyn LedgerWindowRead,
    rows: Cell<usize>,
    earlier: Cell<usize>,
    by_range: Cell<usize>,
    whole: Cell<usize>,
}

impl<'a> Counted<'a> {
    fn new(snapshot: &'a dyn LedgerWindowRead) -> Self {
        Self {
            snapshot,
            rows: Cell::new(0),
            earlier: Cell::new(0),
            by_range: Cell::new(0),
            whole: Cell::new(0),
        }
    }

    fn returned(&self) -> Returned {
        Returned {
            rows: self.rows.get(),
            earlier: self.earlier.get(),
            by_range: self.by_range.get(),
            whole: self.whole.get(),
        }
    }

    /// Count `rows`, the queue's of a kind read by its range by
    /// `by_range`'s reads (those of the reads of a kind read whole and of
    /// every queue row as whole).
    fn count(&self, rows: &[LedgerRow], by_range: bool) {
        for row in rows {
            let cell = match row.kind {
                LedgerKind::Queue if by_range => &self.by_range,
                LedgerKind::Queue => &self.whole,
                _ => &self.rows,
            };
            cell.set(cell.get() + 1);
        }
    }

    fn earlier_counted(&self, earlier: EarlierRecords) -> EarlierRecords {
        self.earlier
            .set(self.earlier.get() + earlier.records.len() + earlier.executions.len());
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
        self.count(&rows, false);
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
        self.count(&rows, false);
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
        self.count(row.as_slice(), false);
        Ok(row)
    }
    fn run_events(&self, run: &RunId) -> Result<Vec<RunEvent>> {
        self.snapshot.run_events(run)
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
        self.snapshot.event(id)
    }
    fn events_of_kind(
        &self,
        kind: &str,
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        self.snapshot.events_of_kind(kind, after, upto, limit)
    }
    fn reads_queue_by_kind(&self) -> bool {
        self.snapshot.reads_queue_by_kind()
    }
    fn queue_rows_whole(&self, after: Option<&str>, limit: usize) -> Result<Vec<LedgerRow>> {
        let rows = self.snapshot.queue_rows_whole(after, limit)?;
        self.count(&rows, false);
        Ok(rows)
    }
    fn queue_rows_reaching(&self, kind: &str, from_ms: i64) -> Result<Vec<LedgerRow>> {
        let rows = self.snapshot.queue_rows_reaching(kind, from_ms)?;
        // Each row of these is of `kind`, one read by its range.
        debug_assert!(
            rows.iter()
                .all(|row| read_of(kind_of(row)) == QueueRead::Range)
        );
        self.count(&rows, true);
        Ok(rows)
    }
    fn queue_rows_past(
        &self,
        kind: &str,
        from_ms: i64,
        queue_before: EventId,
    ) -> Result<Vec<LedgerRow>> {
        let rows = self.snapshot.queue_rows_past(kind, from_ms, queue_before)?;
        self.count(&rows, true);
        Ok(rows)
    }
}

/// The window `query` asks of `tx`, read as the store reads it, with what
/// the store returned.
pub(crate) fn counted_window(
    tx: &impl SqlRead,
    query: &WindowQuery,
) -> Result<(LedgerWindow, Returned)> {
    view::with_snapshot(tx, |snapshot| {
        let counted = Counted::new(snapshot);
        let window = ledger_window_in(&counted, query)?;
        Ok((window, counted.returned()))
    })
}

/// The answers a window gives `stats` of `query`: its JSON and the tokens
/// of the Executions ([`LedgerWindow::execution_tokens`]).
fn answers(window: &LedgerWindow, query: &StatsQuery) -> Result<(Value, Value)> {
    let stats = serde_json::to_value(stats_of_window(window, NOW, query, &SOURCES)?)?;
    let tokens = serde_json::to_value(window.execution_tokens())?;
    Ok((stats, tokens))
}

/// Whether each question of `stats` read by its range on `tx` answers as
/// every row does, with what each returned, in the order of the questions.
pub(crate) fn stats_as_every_row(tx: &impl SqlRead) -> Result<Vec<Returned>> {
    let mut returned = Vec::new();
    for (n, query) in stats_questions().iter().enumerate() {
        let range = stats_window(query, false);
        ensure!(
            range.read_from_ms.is_some(),
            "stats[{n}] reads by its range"
        );
        let (expected, expected_tokens) =
            answers(&view::window(tx, &stats_window(query, true))?, query)?;
        // The inbox's session left out gives its first Execution to the
        // read of every row (the case the range read must keep).
        ensure!(
            n == 4 || expected_tokens["by_actor"]["inbox"]["recorded_from"].is_string(),
            "stats[{n}]: the inbox's first Execution: {}",
            expected_tokens["by_actor"]
        );
        let (window, read) = counted_window(tx, &range)?;
        let (got, tokens) = answers(&window, query)?;
        let mut misses = Vec::new();
        differences(&got, &expected, &format!("stats[{n}]"), &mut misses);
        differences(
            &tokens,
            &expected_tokens,
            &format!("tokens[{n}]"),
            &mut misses,
        );
        ensure!(misses.is_empty(), "{misses:?}");
        returned.push(read);
    }
    for (n, query) in stats_questions_past_their_end().iter().enumerate() {
        let window = stats_window(query, false);
        ensure!(
            window.read_from_ms.is_none(),
            "stats past its end [{n}] reads every row"
        );
        let expected = answers(&view::window(tx, &stats_window(query, true))?, query)?;
        let (got, read) = counted_window(tx, &window)?;
        ensure!(read.by_range == 0, "stats past its end [{n}]: {read:?}");
        ensure!(
            answers(&got, query)? == expected,
            "stats past its end [{n}]"
        );
    }
    Ok(returned)
}

/// The range read of `stats` and of `kpi`'s periods answers as every row on
/// a history with the clocks' cases, a run on its way and the queue's
/// cases, and the questions read by their range read the run on its way,
/// the run whose end is in the period by id alone and the queue's rows
/// that reach S; a window of no range, `WindowQuery::default`'s, answers
/// as before. Then what the store returns is the same with `small` and
/// with `large` settled units before the range, for the range to now and
/// the past one alike, and of the queue's kinds read by their range; the
/// rows after the end are read as they are, with no bound, so the units are
/// put before both ranges and none after.
pub fn ledger_range_answers_as_every_row<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
) -> Result<()> {
    ledger_range_with(open, 30, 300)
}

/// [`ledger_range_answers_as_every_row`] with `small` and `large` units.
pub(crate) fn ledger_range_with<S: ContractStore, F: Fn() -> Result<S>>(
    open: &F,
    small: usize,
    large: usize,
) -> Result<()> {
    let mut reads = Vec::new();
    for settled in [small, large] {
        let store = open()?;
        let tx = store.write_entry("contract_ledger_range")?;
        let laid = lay_down(&tx, &range_history(settled))?;
        let returned = stats_as_every_row(&tx)?;
        // `kpi`'s periods, read from their time through the same read, answer
        // as every row on this history too.
        super::ledger_earlier::answers_as_every_row(&tx)?;
        // A read of no range (`WindowQuery::default`'s and `stats` with no
        // `--since`) reads every queue row as before, by its pages.
        let queue_rows: i64 = tx.query_row(
            "SELECT COUNT(*) FROM ledger_rows WHERE kind=?1",
            crate::infrastructure::sql_exec::sql_params![LedgerKind::Queue.as_str()],
            |r| r.get(0),
        )?;
        for query in [WindowQuery::default(), whole_query()] {
            let (_, read) = counted_window(&tx, &query)?;
            ensure!(
                read.by_range == 0 && i64::try_from(read.whole)? == queue_rows,
                "a read of no range reads every queue row whole: {read:?} of {queue_rows}"
            );
        }
        ensure!(
            crate::application::stats::window_query(&StatsQuery::default(), NOW)
                .read_from_ms
                .is_none()
        );
        let (window, _) = counted_window(&tx, &stats_window(&stats_questions()[0], false))?;
        let runs: Vec<&str> = window
            .of(LedgerKind::Run)
            .map(|row| row.key.as_str())
            .collect();
        for n in [76, 77, 78] {
            ensure!(runs.contains(&run(n).as_str()), "run {n} in {runs:?}");
        }
        // The alert a clock ahead started after S, ended before the last end
        // before S, is read by its start.
        ensure!(
            window
                .events()
                .iter()
                .any(|e| e.kind.as_str() == "live_alert_started" && e.payload["kind"] == "skewed"),
            "the skewed alert's start"
        );
        reads.push(returned);
        clear(&tx, &laid)?;
        drop(tx);
    }
    let (small_reads, large_reads) = (&reads[0], &reads[1]);
    // The last question's range is before every event, so the settled
    // units are after it, read as they are: its answers are compared, not
    // what it returns.
    let before_the_units = stats_questions().len() - 1;
    for (n, (a, b)) in small_reads
        .iter()
        .zip(large_reads)
        .enumerate()
        .take(before_the_units)
    {
        ensure!(
            (a.rows, a.earlier, a.by_range) == (b.rows, b.earlier, b.by_range),
            "stats[{n}] returns {a:?} of {small} units and {b:?} of {large}"
        );
    }
    // The kinds read by their range are read so, a few rows each.
    ensure!(
        small_reads.iter().all(|read| read.by_range > 0),
        "{small_reads:?}"
    );
    ensure!(!BY_RANGE.is_empty());
    Ok(())
}
