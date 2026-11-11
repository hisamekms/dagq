use super::*;
use crate::application::{LedgerStore, LedgerWindowRead};
use crate::domain::forecast::score::{self, Scoring};
use crate::domain::ledger::session::written::{Cursors, RowPosition};
use crate::domain::{
    GoalId,
    ledger::{
        build,
        window::{WindowQuery, run_on_its_way, runs_on_their_way, select},
    },
    stats::{self, Cursor},
};
use std::cell::Cell;
const MIN: i64 = 60_000;
const T0: i64 = 1_790_812_800_000;
#[derive(Default)]
struct Log {
    events: Vec<RunEvent>,
    next_run: i64,
}

impl Log {
    fn push(
        &mut self,
        task: Option<i64>,
        goal: Option<i64>,
        run: Option<RunId>,
        kind: EventKind,
        payload: Value,
        at: i64,
    ) -> EventId {
        let id = EventId::new(self.events.len() as i64 + 1);
        self.events.push(RunEvent {
            id,
            task_id: task.map(TaskId::new),
            goal_id: goal.map(GoalId::new),
            run_id: run,
            kind: kind.as_str().into(),
            payload,
            created_at: marks::utc_text(at),
            actor: None,
        });
        id
    }

    fn last_id(&self) -> i64 {
        self.events.last().map_or(0, |e| e.id.as_i64())
    }

    fn queue(&mut self, kind: EventKind, payload: Value, at: i64) -> EventId {
        self.push(None, None, None, kind, payload, at)
    }

    /// A new run of task `task` (one of its own when none) claimed at
    /// `at`.
    fn claim(&mut self, task: Option<i64>, payload: Value, at: i64) -> (i64, RunId) {
        self.next_run += 1;
        let n = self.next_run;
        let task = task.unwrap_or(100_000 + n);
        let run = RunId::new(format!("00000000-0000-4000-8000-{n:012}")).unwrap();
        self.push(
            Some(task),
            None,
            Some(run.clone()),
            EventKind::RunClaimed,
            payload,
            at,
        );
        (task, run)
    }

    fn end(&mut self, (task, run): &(i64, RunId), kind: EventKind, payload: Value, at: i64) {
        self.push(Some(*task), None, Some(run.clone()), kind, payload, at);
    }

    /// A run claimed at `at` and landed a minute later.
    fn run(&mut self, task: Option<i64>, payload: Value, at: i64) -> (i64, RunId) {
        let run = self.claim(task, payload, at);
        self.end(&run, EventKind::RunIntegrated, json!({}), at + MIN);
        run
    }

    fn start(&mut self, version: &str, parallel: i64, at: i64) -> EventId {
        self.queue(
            EventKind::SupervisorStarted,
            json!({"dagq_version": version, "parallel": parallel, "handoff": false}),
            at,
        )
    }

    fn mark(&mut self, label: &str, effective: Option<i64>, at: i64) -> EventId {
        self.queue(
            EventKind::MarkRecorded,
            json!({"label": label, "at": effective.map(marks::utc_text), "by": "human"}),
            at,
        )
    }

    fn retract(&mut self, mark: EventId, at: i64) {
        self.queue(
            EventKind::MarkRetracted,
            json!({"mark": mark.as_i64(), "kind": "mark_recorded", "label": "x", "by": "human"}),
            at,
        );
    }

    /// A snapshot at `at` that forecast `tasks` and `goals`, having
    /// looked at every event written so far.
    fn snapshot(&mut self, tasks: &[i64], goals: &[i64], at: i64) {
        let rows = |ids: &[i64]| -> Vec<Value> {
            ids.iter()
                .map(|id| json!({"id": id, "p50_secs": 3600, "p90_secs": 7200}))
                .collect()
        };
        let through = self.last_id();
        self.queue(
            EventKind::ForecastRecorded,
            json!({"at_secs": at / 1000, "triggers_through": through, "method": 1,
                       "tasks": rows(tasks), "goals": rows(goals)}),
            at,
        );
    }

    /// A snapshot without `triggers_through`: a mark is after it by
    /// time alone.
    fn snapshot_by_time(&mut self, tasks: &[i64], at: i64) {
        let rows: Vec<Value> = tasks
            .iter()
            .map(|id| json!({"id": id, "p50_secs": 3600, "p90_secs": 7200}))
            .collect();
        self.queue(
            EventKind::ForecastRecorded,
            json!({"at_secs": at / 1000, "method": 1, "tasks": rows, "goals": []}),
            at,
        );
    }

    fn task_status(&mut self, task: i64, to: &str, at: i64) {
        self.push(
            Some(task),
            None,
            None,
            EventKind::TaskStatusChanged,
            json!({"from": "in_progress", "to": to}),
            at,
        );
    }

    fn goal_closed(&mut self, goal: i64, at: i64) {
        self.push(
            None,
            Some(goal),
            None,
            EventKind::GoalClosed,
            json!({"verdict": "achieved"}),
            at,
        );
    }

    fn priority(&mut self, task: i64, at: i64) {
        self.push(
            Some(task),
            None,
            None,
            EventKind::TaskPriorityChanged,
            json!({"from": "normal", "to": "high"}),
            at,
        );
    }

    /// `units` settled runs from `at`, ten minutes apart, a routine
    /// start before every fifth, a mark retracted at once before every
    /// tenth, a snapshot of nothing open before every tenth: the
    /// history outside a range the tests grow. Returns the time after.
    fn settled(&mut self, units: i64, at: i64) -> i64 {
        for n in 0..units {
            let t = at + n * 10 * MIN;
            if n % 5 == 0 {
                self.start("v1", 4, t);
            }
            if n % 10 == 0 {
                let mark = self.mark("settled", None, t + 1);
                self.retract(mark, t + 2);
                self.snapshot(&[], &[], t + 3);
            }
            self.run(None, attributes("v1", 4), t + 4);
        }
        at + units * 10 * MIN
    }
}

fn attributes(version: &str, parallel: i64) -> Value {
    json!({
        "dagq_version": version,
        "claude_version": "2.1.0",
        "rustc_release": "1.99.1",
        "rustc_host": "aarch64-apple-darwin",
        "parallel": parallel,
    })
}

const TASK: i64 = 900;
const OTHER: i64 = 901;
const GOAL: i64 = 50;
const AHEAD: i64 = 902;
const CANCELED: i64 = 903;

/// Task 900 finished once and reopened, snapshots of it before and
/// after that finish, of goal 50 and of task 901 (which does not finish
/// in the range); between the snapshots and the range a new build in a
/// claim, a recorded mark retracted after the range, a change of task
/// 900's priority and `between` settled units; in the range task 900
/// and goal 50 finish; after it a mark backdated before the snapshots
/// (after them by id). `before` settled units precede it all.
fn forecast_history(before: i64, between: i64) -> (Log, i64, i64, ()) {
    let mut log = Log::default();
    let t = log.settled(before, T0);
    log.push(
        None,
        Some(GOAL),
        None,
        EventKind::GoalCreated,
        json!({"title": "g"}),
        t,
    );
    log.push(
        Some(TASK),
        None,
        None,
        EventKind::TaskCreated,
        json!({"title": "t"}),
        t,
    );
    for task in [AHEAD, CANCELED] {
        log.push(
            Some(task),
            None,
            None,
            EventKind::TaskCreated,
            json!({"title": "t"}),
            t,
        );
    }
    // Written before the snapshots by a clock ahead: after the one
    // without `triggers_through` by time alone.
    log.priority(TASK, t + 7 * MIN);
    log.mark("clock ahead", None, t + 7 * MIN);
    log.snapshot(&[TASK], &[], t);
    log.task_status(TASK, "completed", t + MIN);
    log.task_status(TASK, "ready", t + 2 * MIN);
    log.snapshot(&[TASK, OTHER, AHEAD, CANCELED], &[GOAL], t + 3 * MIN);
    log.snapshot_by_time(&[TASK], t + 3 * MIN + MIN / 2);
    log.run(None, attributes("v2", 4), t + 4 * MIN);
    let mark = log.mark("between", None, t + 5 * MIN);
    log.priority(TASK, t + 6 * MIN);
    let t = log.settled(between, t + 10 * MIN);
    let from = t + 10 * MIN;
    let to = from + 60 * MIN;
    // Task 902 finished by a clock ahead past the range, then again in
    // it: the snapshot's first finish of it is the one past the range.
    log.task_status(AHEAD, "completed", to + 60 * MIN);
    log.task_status(AHEAD, "ready", from + MIN);
    log.task_status(AHEAD, "completed", from + 30 * MIN);
    log.task_status(CANCELED, "canceled", from + 40 * MIN);
    log.task_status(TASK, "completed", from + 10 * MIN);
    log.goal_closed(GOAL, from + 20 * MIN);
    log.retract(mark, to + 10 * MIN);
    log.mark("backdated", Some(T0 - 60 * MIN), to + 20 * MIN);
    (log, from, to, ())
}

fn finish_history(units: i64, gap: usize) -> (Log, i64, i64, ()) {
    let mut log = Log::default();
    let from = log.settled(units, T0);
    log.mark("first", None, from + MIN);
    let resumed = log.claim(Some(70), attributes("v1", 4), from - 5 * MIN);
    log.end(
        &resumed,
        EventKind::ValidationFinished,
        json!({"status": "failed"}),
        from - 4 * MIN,
    );
    log.end(
        &resumed,
        EventKind::RunIntegrated,
        json!({}),
        from + 2 * MIN,
    );
    let back = log.claim(Some(71), attributes("v1", 4), from + 2 * MIN);
    log.end(
        &back,
        EventKind::ValidationFinished,
        json!({"status": "failed"}),
        from - 3 * MIN,
    );
    log.end(&back, EventKind::RunIntegrated, json!({}), from + 3 * MIN);
    for n in 0..gap as i64 {
        log.run(Some(70 + n % 2), attributes("v1", 4), from + (4 + n) * MIN);
    }
    let to = from + 30 * MIN;
    log.mark("second", None, to);
    log.settled(units, to + 10 * MIN);
    (log, from, to, ())
}

fn complete<T: std::fmt::Debug>(result: WindowRead<T>) -> T {
    match result {
        WindowRead::Complete(value) => value,
        other => panic!("{other:?}"),
    }
}
fn marks_at(events: &[RunEvent], start: i64, end: i64) -> Vec<marks::Mark> {
    marks::marks(events, Some(Cursor::Time(start)), Some(Cursor::Time(end)))
}
fn scored(events: &[RunEvent], start: i64, end: i64) -> Scoring {
    let mut scoring = score::score(events, &marks::marks(events, None, None));
    scoring
        .samples
        .retain(|s| s.finished_ms > start && s.finished_ms <= end);
    scoring
        .excluded
        .retain(|(_, ms, _)| *ms > start && *ms <= end);
    scoring
}
struct Counted<'a, T> {
    conn: &'a T,
    rows: Cell<usize>,
}
impl<T: SqlRead> SqlRead for Counted<'_, T> {
    fn dialect(&self) -> SqlDialect {
        self.conn.dialect()
    }
    fn visit_rows(
        &self,
        sql: &str,
        params: &[SqlValue],
        visit: &mut dyn FnMut(&SqlRow<'_>) -> SqlResult<bool>,
    ) -> SqlResult<()> {
        self.conn.visit_rows(sql, params, &mut |r| {
            self.rows.set(self.rows.get() + 1);
            visit(r)
        })
    }
}
fn certificates(id: EventId) -> Marks {
    let position = RowPosition {
        through: Cursors {
            events: id,
            ..Cursors::default()
        },
        read_ms: T0,
    };
    Marks {
        sessions: SessionsWritten::at(id),
        rows: Some(RowsWritten {
            core: Some(position),
            queue: Some(position),
            ..RowsWritten::at(position.through, T0)
        }),
        omitted: None,
    }
}
/// Deliberately absent run rows also have ended task rows: they cannot be
/// supplemented as running runs. The whole view is assembled independently.
fn save(log: &Log, hidden: &[EventId]) -> SqliteQueue {
    let queue =
        SqliteQueue::init(format!("file:/small-{}?vfs=memdb", uuid::Uuid::new_v4())).unwrap();
    let hidden_runs: BTreeSet<_> = log
        .events
        .iter()
        .filter(|e| hidden.contains(&e.id))
        .filter_map(|e| e.run_id.clone())
        .collect();
    let held: Vec<_> = log
        .events
        .iter()
        .filter(|e| {
            !hidden.contains(&e.id) && e.run_id.as_ref().is_none_or(|r| !hidden_runs.contains(r))
        })
        .cloned()
        .collect();
    let mut rows = build(&held, T0 + 1_000_000_000_000, &marks::utc_text(T0));
    for row in &mut rows {
        if row.kind == LedgerKind::Task && !hidden_runs.is_empty() {
            row.ended_at = Some(T0);
        }
        queue.write_ledger_row(row).unwrap();
    }
    queue
        .conn
        .pragma_update(None, "foreign_keys", false)
        .unwrap();
    for e in &log.events {
        queue.conn.execute("INSERT INTO run_events(id,task_id,goal_id,run_id,kind,payload,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)", rusqlite::params![e.id.as_i64(),e.task_id.map(TaskId::as_i64),e.goal_id.map(GoalId::as_i64),e.run_id.as_ref().map(RunId::as_str),e.kind.as_str(),e.payload.to_string(),e.created_at]).unwrap();
    }
    queue
        .conn
        .pragma_update(None, "foreign_keys", true)
        .unwrap();
    queue
        .write_ledger_marks(&certificates(EventId::new(log.last_id())))
        .unwrap();
    queue
}
fn whole(read: &dyn LedgerWindowRead) -> Vec<RunEvent> {
    let mut rows = Vec::new();
    for kind in LedgerKind::ALL {
        rows.extend(read.rows_after(kind, None, usize::MAX).unwrap());
    }
    for run in runs_on_their_way(&rows) {
        rows.extend(run_on_its_way(
            &run,
            &read.run_events(&run).unwrap(),
            T0 + 1_000_000_000_000,
        ));
    }
    select(
        rows,
        &WindowQuery {
            now_ms: T0 + 1_000_000_000_000,
            ..WindowQuery::default()
        },
    )
    .events()
    .to_vec()
}

#[test]
fn marks_and_routine_preserve_time_and_id_order_with_bounded_reads() {
    let mut counts = Vec::new();
    for units in [30, 3000] {
        let mut log = Log::default();
        let s = log.settled(units, T0);
        log.start("v1", 1, s);
        log.start("v1", 1, s + 20 * MIN);
        log.start("v1", 2, s + 10 * MIN);
        log.run(None, attributes("v1", 2), s + 16 * MIN);
        log.run(None, attributes("v2", 2), s - 50 * MIN);
        log.run(None, attributes("v2", 2), s + 17 * MIN);
        for _ in 0..120 {
            log.run(None, json!({}), s - MIN);
        }
        log.start("v3", 2, s + 100 * MIN);
        log.run(None, attributes("v3", 2), s + 18 * MIN);
        log.mark("far backdate", Some(s + 19 * MIN), s + 300 * MIN);
        let mark = log.mark("cli ahead", Some(s + 21 * MIN), s - MIN);
        log.retract(mark, s - 100 * MIN);
        let later = log.mark("withdrawn later", Some(s + 23 * MIN), s + 300 * MIN);
        log.retract(later, s + 400 * MIN);
        log.queue(
            EventKind::SupervisorStopped,
            json!({"outcome":"pruned","last_heartbeat_at":(s+22*MIN)/1000}),
            s + 500 * MIN,
        );
        let end = s + 25 * MIN;
        let queue = save(&log, &[]);
        queue
            .ledger_read_snapshot(&mut |read| {
                let all = whole(read);
                let selected = complete(read.mark_events(s + 15 * MIN, end)?);
                assert_eq!(
                    marks_at(&selected, s + 15 * MIN, end),
                    marks_at(&all, s + 15 * MIN, end)
                );
                assert_eq!(
                    crate::domain::kpi::compare::classified(&marks::marks(
                        &selected,
                        None,
                        Some(Cursor::Time(end))
                    ))
                    .into_iter()
                    .filter(|(m, _)| timestamp_millis(&m.at).unwrap() > s + 15 * MIN)
                    .map(|(m, r)| (m.clone(), r))
                    .collect::<Vec<_>>(),
                    crate::domain::kpi::compare::classified(&marks::marks(
                        &all,
                        None,
                        Some(Cursor::Time(end))
                    ))
                    .into_iter()
                    .filter(|(m, _)| timestamp_millis(&m.at).unwrap() > s + 15 * MIN)
                    .map(|(m, r)| (m.clone(), r))
                    .collect::<Vec<_>>()
                );
                Ok(())
            })
            .unwrap();
        let conn = Counted {
            conn: &*queue.conn,
            rows: Cell::new(0),
        };
        complete(mark_events(&conn, s + 15 * MIN, end).unwrap());
        counts.push(conn.rows.get());
    }
    // Includes candidates discarded by K walks and rows read by visibility.
    assert_eq!(counts[0], counts[1]);
}

#[test]
fn forecast_two_stages_match_score_and_ignore_history_before_open_periods() {
    let mut counts = Vec::new();
    for units in [30, 3000] {
        let (log, s, e, ()) = forecast_history(units, 30);
        let queue = save(&log, &[]);
        queue
            .ledger_read_snapshot(&mut |read| {
                let all = whole(read);
                let selected = complete(read.forecast_events(s, e)?);
                let expected = scored(&all, s, e);
                assert_eq!(expected.samples.len(), 3);
                assert_eq!(expected.excluded.len(), 1);
                assert_eq!(scored(&selected, s, e), expected);
                assert!(complete(read.forecast_events(s - 5 * MIN, s)?).is_empty());
                Ok(())
            })
            .unwrap();
        let conn = Counted {
            conn: &*queue.conn,
            rows: Cell::new(0),
        };
        complete(forecast_events(&conn, s, e).unwrap());
        counts.push(conn.rows.get());
    }
    // Growth inside an open period or after snapshot.triggers_through is
    // intentionally proportional; targets that never finish open no period.
    assert_eq!(counts[0], counts[1]);
}

#[test]
fn forecast_ignores_history_of_a_target_that_does_not_finish_in_the_range() {
    let mut counts = Vec::new();
    for units in [30, 3000] {
        let mut log = Log::default();
        log.push(
            Some(OTHER),
            None,
            None,
            EventKind::TaskCreated,
            json!({"title":"other target"}),
            T0,
        );
        for n in 0..units {
            let at = T0 + n * 10 * MIN;
            log.run(Some(OTHER), attributes("v1", 1), at);
            log.snapshot(&[OTHER], &[], at + MIN);
            log.priority(OTHER, at + 2 * MIN);
            log.task_status(OTHER, "completed", at + 3 * MIN);
            log.task_status(OTHER, "ready", at + 4 * MIN);
        }
        let at = T0 + units * 10 * MIN;
        log.push(
            Some(TASK),
            None,
            None,
            EventKind::TaskCreated,
            json!({"title":"finishing target"}),
            at,
        );
        // OTHER is forecast here too, but its varying history and all its
        // earlier finishes precede the finishing target's open period. This
        // does not exempt history IN that period or marks after the snapshot
        // from the proportional read asserted by the separate scaling test.
        log.snapshot(&[TASK, OTHER], &[], at + MIN);
        log.run(Some(TASK), attributes("v2", 1), at + 2 * MIN);
        log.task_status(TASK, "completed", at + 10 * MIN);
        let (start, end) = (at + 9 * MIN, at + 11 * MIN);
        let queue = save(&log, &[]);
        queue
            .ledger_read_snapshot(&mut |read| {
                let all = whole(read);
                let selected = complete(read.forecast_events(start, end)?);
                let expected = scored(&all, start, end);
                assert_eq!(expected.samples.len(), 1);
                assert_eq!(
                    expected.samples[0].target,
                    score::Target::Task(TaskId::new(TASK))
                );
                assert!(expected.excluded.is_empty());
                assert_eq!(scored(&selected, start, end), expected);
                assert!(
                    !all.iter()
                        .any(|event| event.task_id == Some(TaskId::new(OTHER))
                            && event.kind == EventKind::TaskStatusChanged
                            && event.payload["to"] == "completed"
                            && ms(event) > start
                            && ms(event) <= end)
                );
                Ok(())
            })
            .unwrap();
        let conn = Counted {
            conn: &*queue.conn,
            rows: Cell::new(0),
        };
        complete(forecast_events(&conn, start, end).unwrap());
        counts.push(conn.rows.get());
    }
    // Includes SQL candidate events discarded by selection and all rows
    // fetched by visibility, not only the events the read finally returns.
    assert_eq!(counts[0], counts[1]);
}

#[test]
fn finishes_match_first_finished_event_and_compare_cohorts() {
    let mut counts = Vec::new();
    for units in [30, 3000] {
        let (mut log, s, e, ()) = finish_history(units, 5);
        for task in [70, 71] {
            log.push(
                Some(task),
                None,
                None,
                EventKind::TaskCreated,
                json!({"title":"goal's task", "goal_id":7}),
                s - MIN,
            );
        }
        log.push(
            Some(71),
            None,
            None,
            EventKind::TaskGoalChanged,
            json!({"to":8}),
            s + 20 * MIN,
        );
        let queue = save(&log, &[]);
        queue
            .ledger_read_snapshot(&mut |read| {
                let all = whole(read);
                let expected = |range: Option<(i64, i64)>| {
                    let mut ends: Vec<_> = stats::run_stats(&all)
                        .into_iter()
                        .filter_map(|r| {
                            let event = all.iter().find(|e| Some(e.id) == r.finished_event_id)?;
                            let millis = ms(event);
                            range
                                .is_none_or(|(s, e)| millis > s && millis <= e)
                                .then_some(RunFinish {
                                    run: r.run_id,
                                    task: r.task_id,
                                    event: event.id,
                                    millis,
                                })
                        })
                        .collect();
                    ends.sort_by(|a, b| a.run.cmp(&b.run));
                    ends
                };
                assert_eq!(
                    complete(read.run_finishes(Some((s, e)))?),
                    expected(Some((s, e)))
                );
                assert_eq!(complete(read.run_finishes(None)?), expected(None));
                let attributes = match select(
                    build(&all, e, &marks::utc_text(e)),
                    &WindowQuery {
                        now_ms: e,
                        ..WindowQuery::default()
                    },
                )
                .attributes()
                {
                    crate::domain::ledger::attributes::Folded::Attributes(attributes) => attributes,
                    _ => panic!("creation records carry attributes"),
                };
                let goals = attributes.task_goals();
                assert_eq!(goals.get(&TaskId::new(71)), Some(&Some(GoalId::new(8))));
                let in_goal =
                    |finish: &RunFinish| goals.get(&finish.task) == Some(&Some(GoalId::new(7)));
                assert_eq!(
                    complete(read.run_finishes(Some((s, e)))?)
                        .into_iter()
                        .filter(in_goal)
                        .collect::<Vec<_>>(),
                    expected(Some((s, e)))
                        .into_iter()
                        .filter(in_goal)
                        .collect::<Vec<_>>()
                );
                let selected = complete(read.comparison_events()?);
                assert_eq!(
                    selected,
                    all.iter()
                        .filter(|e| e.kind.is_in(&marks::READ_KINDS))
                        .cloned()
                        .collect::<Vec<_>>()
                );
                Ok(())
            })
            .unwrap();
        let conn = Counted {
            conn: &*queue.conn,
            rows: Cell::new(0),
        };
        complete(run_finishes(&conn, Some((s, e))).unwrap());
        counts.push(conn.rows.get());
    }
    // Full-period finishes and READ_KINDS are cohort reads, excluded from this
    // bound: the former is proportional to run count, without run records.
    assert_eq!(counts[0], counts[1]);
}

#[test]
fn cursors_preserve_clock_reversals_and_report_the_search_path() {
    let mut log = Log::default();
    log.mark("later time", None, T0 + 100 * MIN);
    let cursor = log.mark("clock back", None, T0 + 10 * MIN);
    let queue = save(&log, &[]);
    queue
        .ledger_read_snapshot(&mut |read| {
            let answer = complete(read.cursor_record_time(cursor)?);
            assert_eq!(answer.path, CursorTimePath::Recent);
            assert_eq!(
                answer.millis,
                marks::cursor_millis(Cursor::Event(cursor), &whole(read))
            );
            Ok(())
        })
        .unwrap();
    for n in 0..150 {
        log.mark("after cursor", None, T0 + (200 + n) * MIN);
    }
    let queue = save(&log, &[]);
    queue
        .ledger_read_snapshot(&mut |read| {
            let answer = complete(read.cursor_record_time(cursor)?);
            assert_eq!(answer.path, CursorTimePath::Aggregate);
            assert_eq!(
                answer.millis,
                marks::cursor_millis(Cursor::Event(cursor), &whole(read))
            );
            Ok(())
        })
        .unwrap();
    // A missing ended run has the greatest time <= cursor, but that time
    // belongs to no event in LedgerWindow::events.
    let hidden = log.claim(Some(88), attributes("v9", 9), T0 + 10_000 * MIN);
    log.end(
        &hidden,
        EventKind::RunIntegrated,
        json!({}),
        T0 + 10_001 * MIN,
    );
    let cursor = EventId::new(log.last_id());
    let hidden_ids: Vec<_> = log
        .events
        .iter()
        .filter(|e| e.run_id.as_ref() == Some(&hidden.1))
        .map(|e| e.id)
        .collect();
    for n in 0..150 {
        log.mark("newer than hidden", None, T0 + (20_000 + n) * MIN);
    }
    let queue = save(&log, &hidden_ids);
    queue
        .ledger_read_snapshot(&mut |read| {
            let answer = complete(read.cursor_record_time(cursor)?);
            assert_eq!(answer.path, CursorTimePath::VisibleWalk);
            assert_eq!(
                answer.millis,
                marks::cursor_millis(Cursor::Event(cursor), &whole(read))
            );
            Ok(())
        })
        .unwrap();
    queue
        .conn
        .execute(
            "UPDATE run_events SET created_at='2026-10-01T00:00:00.000z' WHERE id=1",
            [],
        )
        .unwrap();
    queue
        .ledger_read_snapshot(&mut |read| {
            assert_eq!(read.cursor_record_time(cursor)?, WindowRead::OffTimeForm);
            Ok(())
        })
        .unwrap();
}

#[test]
fn invisible_claims_and_starts_are_not_predecessors() {
    let mut log = Log::default();
    let a = log.run(Some(1), attributes("v1", 1), T0);
    let b = log.run(Some(2), attributes("v2", 2), T0 + MIN);
    let hidden_claim = log
        .events
        .iter()
        .find(|e| e.run_id.as_ref() == Some(&b.1))
        .unwrap()
        .id;
    let invisible_start = log.start("v2", 2, T0 + 6 * MIN + MIN / 2);
    log.run(Some(3), attributes("v2", 2), T0 + 10 * MIN);
    log.start("v1", 1, T0 + 6 * MIN);
    let queue = save(&log, &[hidden_claim, invisible_start]);
    // Put queue R before these starts: only actually saved post-R records
    // certify them. The hidden start has no saved queue record.
    let mut cert = certificates(EventId::new(log.last_id()));
    cert.rows
        .as_mut()
        .unwrap()
        .queue
        .as_mut()
        .unwrap()
        .through
        .events = EventId::new(0);
    queue.write_ledger_marks(&cert).unwrap();
    queue
        .ledger_read_snapshot(&mut |read| {
            let all = whole(read);
            assert!(all.iter().any(|e| e.run_id.as_ref() == Some(&a.1)));
            assert!(
                !all.iter()
                    .any(|e| e.id == hidden_claim || e.id == invisible_start)
            );
            let selected = complete(read.mark_events(T0 + 7 * MIN, T0 + 15 * MIN)?);
            assert_eq!(
                marks_at(&selected, T0 + 7 * MIN, T0 + 15 * MIN),
                marks_at(&all, T0 + 7 * MIN, T0 + 15 * MIN)
            );
            assert!(
                marks_at(&selected, T0 + 7 * MIN, T0 + 15 * MIN)
                    .iter()
                    .any(|m| m.kind == "derived:dagq_version")
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn ordered_seeks_skip_invisible_candidates_and_distinguish_absence_from_limit() {
    // These exercise the actual shared seek for all selection orders and
    // partial indexes, using run-owned events of missing ended runs as hidden
    // candidates. Visibility is orthogonal to the event kind/index.
    for hidden_count in [1, WINDOW_READ_CANDIDATES, WINDOW_READ_CANDIDATES + 1] {
        let mut log = Log::default();
        let mut hidden = Vec::new();
        for n in 0..hidden_count {
            let pair = log.claim(Some(100 + n as i64), attributes("v1", 1), T0 + MIN);
            hidden.push(EventId::new(log.last_id()));
            for kind in [
                EventKind::SupervisorStarted,
                EventKind::TaskStatusChanged,
                EventKind::GoalClosed,
                EventKind::RunIntegrated,
            ] {
                log.push(
                    Some(pair.0),
                    Some(50),
                    Some(pair.1.clone()),
                    kind,
                    json!({"parallel":1,"dagq_version":"v1","to":"completed"}),
                    T0 + MIN,
                );
                hidden.push(EventId::new(log.last_id()));
            }
        }
        let visible = log.claim(Some(9), attributes("v1", 1), T0);
        log.end(&visible, EventKind::RunIntegrated, json!({}), T0 + 2 * MIN);
        let queue = save(&log, &hidden);
        let reader = Reader { tx: &*queue.conn };
        let ends = format!("{} AND created_at >= ''", reader.end_condition());
        let result =
            answer(reader.first("run_ends_by_time", &ends, "created_at, id", sql_params![]))
                .unwrap();
        if hidden_count < WINDOW_READ_CANDIDATES {
            assert_eq!(complete(result).unwrap().run_id, Some(visible.1.clone()));
        } else {
            assert_eq!(result, WindowRead::Exhausted);
        }
        // Restrict to the hidden candidates. Exactly K invisible entries
        // with none remaining means absence; K+1 means fallback.
        let result=answer(reader.first("claims_with_parallel","kind = 'run_claimed' AND json_extract(payload, '$.parallel') IS NOT NULL AND task_id != 9 AND id >= -9223372036854775808","id",sql_params![])).unwrap();
        if hidden_count <= WINDOW_READ_CANDIDATES {
            assert_eq!(complete(result), None);
        } else {
            assert_eq!(result, WindowRead::Exhausted);
        }
    }
}

#[test]
fn unavailable_snapshot_ports_require_the_whole_read() {
    let log = Log::default();
    let queue = save(&log, &[]);
    let read: &dyn LedgerWindowRead = &&queue;
    assert_eq!(
        read.mark_events(T0, T0 + MIN).unwrap(),
        WindowRead::Unavailable
    );
    assert_eq!(
        read.forecast_events(T0, T0 + MIN).unwrap(),
        WindowRead::Unavailable
    );
    assert_eq!(
        read.cursor_record_time(EventId::new(1)).unwrap(),
        WindowRead::Unavailable
    );
    assert_eq!(read.run_finishes(None).unwrap(), WindowRead::Unavailable);
    assert_eq!(read.comparison_events().unwrap(), WindowRead::Unavailable);
}

struct EdgesRead<'a>(&'a dyn LedgerWindowRead);
impl crate::domain::ledger::window::omitted::OmittedRead for EdgesRead<'_> {
    type Error = anyhow::Error;
    fn coverage(&self) -> Result<Option<Coverage>> {
        self.0.omitted_coverage()
    }
    fn point_after(&self, kind: &str, after: EventId) -> Result<Option<Point>> {
        Ok(self
            .0
            .omitted_points_after(kind, after, 1)?
            .into_iter()
            .next())
    }
    fn point_at_or_before(&self, kind: &str, at: EventId) -> Result<Option<Point>> {
        self.0.omitted_point_at_or_before(kind, at)
    }
    fn point_by_ms(&self, kind: &str, ms: i64) -> Result<Option<Point>> {
        self.0.omitted_point_before_ms(kind, ms + 1)
    }
    fn event(&self, id: EventId) -> Result<Option<RunEvent>> {
        self.0.event(id)
    }
    fn events_of_kind(
        &self,
        kind: &str,
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        self.0.events_of_kind(kind, after, upto, limit)
    }
}

#[test]
fn cursor_event_time_is_combined_with_omitted_coverage_as_answer_bound_does() {
    use crate::domain::ledger::window::{
        LedgerWindow,
        bounds::{Bound, DeclareBounds, Declared},
        omitted::OmittedEdges,
    };
    struct Through(EventId);
    impl DeclareBounds for Through {
        fn bounds(&self, _: &LedgerWindow) -> Vec<Bound> {
            vec![Bound::TimeThrough(self.0)]
        }
    }
    for ahead in [false, true] {
        let mut log = Log::default();
        log.mark("visible", None, T0 + 10 * MIN);
        log.queue(
            EventKind::AuthorizationDenied,
            json!({}),
            T0 + if ahead { 2000 * MIN } else { 200 * MIN },
        );
        log.queue(EventKind::AuthorizationDenied, json!({}), T0 + 100 * MIN);
        let through = EventId::new(log.last_id());
        let hidden = vec![EventId::new(2), EventId::new(3)];
        let queue = save(&log, &hidden);
        let mut coverage = Coverage::start(&["authorization_denied".to_owned()], EventId::new(0));
        let mut points = Vec::new();
        coverage.read(&log.events, 2, through, &mut points);
        let mut cert = certificates(through);
        cert.omitted = Some(Omitted { coverage, points });
        queue.write_ledger_marks(&cert).unwrap();
        let end = T0 + 1000 * MIN;
        let query = WindowQuery {
            now_ms: end,
            bounds: Declared::of(Through(through)),
            ..WindowQuery::default()
        };
        let whole_window = queue.ledger_window(&query).unwrap();
        let reference = select(build(&log.events, end, &marks::utc_text(end)), &query);
        assert_eq!(whole_window.answers(), reference.answers());
        queue
            .ledger_read_snapshot(&mut |read| {
                let events_time = complete(read.cursor_record_time(through)?).millis;
                assert_eq!(events_time, T0 + 10 * MIN);
                let reads = EdgesRead(read);
                let edges = OmittedEdges::of(&reads)?;
                let held = through.min(whole_window.upto);
                let omitted = match edges.max_ms_through(held)? {
                    Some(ms) if ms > end => edges.at_or_before_through(end, held)?.1,
                    ms => ms,
                };
                assert!(omitted.unwrap() > events_time);
                assert_eq!(
                    Some(Some(events_time).max(omitted)),
                    whole_window.answers().unwrap().time_through(through)
                );
                Ok(())
            })
            .unwrap();
    }
}

fn lower_queue_mark(queue: &SqliteQueue, through: EventId) {
    let mut cert = certificates(through);
    cert.rows
        .as_mut()
        .unwrap()
        .queue
        .as_mut()
        .unwrap()
        .through
        .events = EventId::new(0);
    cert.rows
        .as_mut()
        .unwrap()
        .core
        .as_mut()
        .unwrap()
        .through
        .events = EventId::new(0);
    cert.sessions = SessionsWritten::at(EventId::new(0));
    queue.write_ledger_marks(&cert).unwrap();
}

#[test]
fn predecessor_claim_start_and_routine_walk_past_hidden_candidates_or_fall_back() {
    for n in [1, WINDOW_READ_CANDIDATES + 1] {
        // Previous claim: B is absent, so C must compare against A.
        let mut log = Log::default();
        log.run(Some(1), attributes("v1", 1), T0);
        let mut hidden = Vec::new();
        for _ in 0..n {
            log.run(None, attributes("v2", 2), T0 + MIN);
            hidden.push(EventId::new(log.last_id() - 1));
        }
        log.run(Some(3), attributes("v2", 2), T0 + 10 * MIN);
        let queue = save(&log, &hidden);
        queue
            .ledger_read_snapshot(&mut |read| {
                let result = read.mark_events(T0 + 9 * MIN, T0 + 11 * MIN)?;
                if n > WINDOW_READ_CANDIDATES {
                    assert_eq!(result, WindowRead::Exhausted);
                } else {
                    assert_eq!(
                        marks_at(&complete(result), T0 + 9 * MIN, T0 + 11 * MIN),
                        marks_at(&whole(read), T0 + 9 * MIN, T0 + 11 * MIN)
                    );
                }
                Ok(())
            })
            .unwrap();
        // The id-ordered announcing-start seek must pass invisible starts.
        let mut log = Log::default();
        log.run(Some(1), attributes("v1", 1), T0);
        let mut hidden = Vec::new();
        for _ in 0..n {
            hidden.push(log.start("v2", 2, T0 + 100 * MIN));
        }
        log.start("v2", 2, T0 + 100 * MIN);
        log.run(Some(3), attributes("v2", 2), T0 + 10 * MIN);
        let queue = save(&log, &hidden);
        lower_queue_mark(&queue, EventId::new(log.last_id()));
        queue
            .ledger_read_snapshot(&mut |read| {
                let result = read.mark_events(T0 + 9 * MIN, T0 + 11 * MIN)?;
                if n > WINDOW_READ_CANDIDATES {
                    assert_eq!(result, WindowRead::Exhausted);
                } else {
                    assert_eq!(
                        marks_at(&complete(result), T0 + 9 * MIN, T0 + 11 * MIN),
                        marks_at(&whole(read), T0 + 9 * MIN, T0 + 11 * MIN)
                    );
                }
                Ok(())
            })
            .unwrap();
        // Routine is ordered by time; its closest starts are not saved.
        let mut log = Log::default();
        log.start("v1", 1, T0);
        let mut hidden = Vec::new();
        for _ in 0..n {
            hidden.push(log.start("v2", 2, T0 + MIN));
        }
        log.start("v1", 1, T0 + 10 * MIN);
        let queue = save(&log, &hidden);
        lower_queue_mark(&queue, EventId::new(log.last_id()));
        queue
            .ledger_read_snapshot(&mut |read| {
                let result = read.mark_events(T0 + 9 * MIN, T0 + 11 * MIN)?;
                if n > WINDOW_READ_CANDIDATES {
                    assert_eq!(result, WindowRead::Exhausted);
                } else {
                    let events = complete(result);
                    assert!(!events.iter().any(|e| hidden.contains(&e.id)));
                    assert_eq!(events, whole(read));
                }
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn forecast_and_both_finish_reads_handle_hidden_ends_and_snapshot_extrema() {
    for n in [1, WINDOW_READ_CANDIDATES, WINDOW_READ_CANDIDATES + 1] {
        let mut log = Log::default();
        log.push(Some(9), None, None, EventKind::TaskCreated, json!({}), T0);
        log.snapshot(&[9], &[], T0 + MIN);
        let mut hidden = Vec::new();
        // One missing run, with multiple end events. Both range and full
        // cohort reads must seek it, even though its aggregate MIN is hidden.
        let missing = log.claim(Some(99), attributes("v1", 1), T0 + 2 * MIN);
        hidden.push(EventId::new(log.last_id()));
        for _ in 0..n {
            log.end(&missing, EventKind::RunIntegrated, json!({}), T0 + 10 * MIN);
            hidden.push(EventId::new(log.last_id()));
            log.push(
                Some(9),
                None,
                Some(missing.1.clone()),
                EventKind::TaskStatusChanged,
                json!({"to":"completed"}),
                T0 + 2 * MIN,
            );
            hidden.push(EventId::new(log.last_id()));
        }
        log.task_status(9, "completed", T0 + 10 * MIN);
        let queue = save(&log, &hidden);
        queue
            .ledger_read_snapshot(&mut |read| {
                let expected = scored(&whole(read), T0 + 9 * MIN, T0 + 11 * MIN);
                let selected = read.forecast_events(T0 + 9 * MIN, T0 + 11 * MIN)?;
                if n > WINDOW_READ_CANDIDATES {
                    assert_eq!(selected, WindowRead::Exhausted);
                } else {
                    assert_eq!(
                        scored(&complete(selected), T0 + 9 * MIN, T0 + 11 * MIN),
                        expected
                    );
                }
                for range in [Some((T0 + 9 * MIN, T0 + 11 * MIN)), None] {
                    let result = read.run_finishes(range)?;
                    if n > WINDOW_READ_CANDIDATES {
                        assert_eq!(result, WindowRead::Exhausted);
                    } else {
                        assert!(complete(result).is_empty());
                    }
                }
                Ok(())
            })
            .unwrap();
    }
    // Repair the maximum end id on visible ends too, before the second stage.
    for count in [1, WINDOW_READ_CANDIDATES + 1] {
        let mut log = Log::default();
        log.push(
            Some(9),
            None,
            None,
            EventKind::TaskCreated,
            json!({"title":"target"}),
            T0,
        );
        log.snapshot(&[9], &[], T0 + MIN);
        log.task_status(9, "completed", T0 + 10 * MIN);
        let missing = log.claim(Some(99), attributes("v1", 1), T0);
        let hidden = EventId::new(log.last_id());
        for _ in 0..count {
            log.push(
                Some(9),
                None,
                Some(missing.1.clone()),
                EventKind::TaskStatusChanged,
                json!({"to":"completed"}),
                T0 + 10 * MIN,
            );
        }
        let queue = save(&log, &[hidden]);
        queue
            .ledger_read_snapshot(&mut |read| {
                let result = read.forecast_events(T0 + 9 * MIN, T0 + 11 * MIN)?;
                if count > WINDOW_READ_CANDIDATES {
                    assert_eq!(result, WindowRead::Exhausted);
                } else {
                    assert_eq!(
                        scored(&complete(result), T0 + 9 * MIN, T0 + 11 * MIN),
                        scored(&whole(read), T0 + 9 * MIN, T0 + 11 * MIN)
                    );
                }
                Ok(())
            })
            .unwrap();
    }
    // An end can be queue-owned (ends_run deliberately accepts a status of
    // failed on any kind), while the next end belongs to a saved run row.
    // Its invisible aggregate minimum must not hide the visible first end.
    let mut log = Log::default();
    let run = log.claim(Some(9), attributes("v1", 1), T0);
    log.end(
        &run,
        EventKind::SupervisorStopped,
        json!({"status":"failed"}),
        T0 + MIN,
    );
    let hidden = EventId::new(log.last_id());
    log.end(&run, EventKind::RunIntegrated, json!({}), T0 + 10 * MIN);
    let queue = save(&log, &[]);
    // Keep the run's saved records but omit the queue-owned first end.
    queue
        .conn
        .execute("DELETE FROM ledger_rows WHERE kind='queue'", [])
        .unwrap();
    lower_queue_mark(&queue, EventId::new(log.last_id()));
    queue
        .ledger_read_snapshot(&mut |read| {
            let all = whole(read);
            assert!(!all.iter().any(|e| e.id == hidden));
            for range in [Some((T0 + 9 * MIN, T0 + 11 * MIN)), None] {
                let ends = complete(read.run_finishes(range)?);
                assert_eq!(ends.len(), 1);
                assert_eq!(ends[0].event, EventId::new(log.last_id()));
                assert_eq!(ends[0].millis, T0 + 10 * MIN);
            }
            Ok(())
        })
        .unwrap();
    // Invisible snapshots must not supply either minimum. The two orderings
    // are exercised independently, including a K-limited repair with a visible
    // snapshot remaining. Reading snapshots for scoring still scales with the
    // target's open period, but does not waive the aggregate's cutoff.
    for key in ["at_secs", "triggers_through"] {
        for count in [1, WINDOW_READ_CANDIDATES + 1] {
            let (mut log, s, e, ()) = forecast_history(30, 30);
            let at = log
                .events
                .iter()
                .position(|event| {
                    event.kind == EventKind::ForecastRecorded
                        && event.payload["tasks"]
                            .as_array()
                            .is_some_and(|a| !a.is_empty())
                })
                .unwrap();
            let mut payload = json!({"at_secs":(e+MIN)/1000,"triggers_through":i64::MAX,"tasks":[{"id":900,"p50_secs":1}]});
            payload[key] = json!(if key == "at_secs" { 0 } else { 1 });
            let extra: Vec<_> = (0..count)
                .map(|_| RunEvent {
                    id: EventId::new(0),
                    task_id: None,
                    goal_id: None,
                    run_id: None,
                    kind: EventKind::ForecastRecorded.into(),
                    payload: payload.clone(),
                    created_at: marks::utc_text(T0),
                    actor: None,
                })
                .collect();
            log.events.splice(at..at, extra);
            for (n, event) in log.events.iter_mut().enumerate() {
                for field in ["mark", "triggers_through"] {
                    if let Some(id) = event.payload[field].as_i64()
                        && id > at as i64
                        && id != i64::MAX
                    {
                        event.payload[field] = json!(id + count as i64);
                    }
                }
                event.id = EventId::new(n as i64 + 1);
            }
            let hidden: Vec<_> = log.events[at..at + count]
                .iter()
                .map(|event| event.id)
                .collect();
            let queue = save(&log, &hidden);
            lower_queue_mark(&queue, EventId::new(log.last_id()));
            queue
                .ledger_read_snapshot(&mut |read| {
                    let result = read.forecast_events(s, e)?;
                    if count > WINDOW_READ_CANDIDATES {
                        assert_eq!(result, WindowRead::Exhausted);
                    } else {
                        assert_eq!(scored(&complete(result), s, e), scored(&whole(read), s, e));
                    }
                    Ok(())
                })
                .unwrap();
        }
    }
}

#[test]
fn small_reads_share_whole_visibility_for_running_runs_and_post_certificate_events() {
    use crate::domain::ledger::owner::{Owner, owner};
    let mut log = Log::default();
    log.push(Some(70), None, None, EventKind::TaskCreated, json!({}), T0);
    log.push(
        Some(TASK),
        None,
        None,
        EventKind::TaskCreated,
        json!({"title":"forecast target"}),
        T0,
    );
    let running = log.claim(Some(70), attributes("v1", 1), T0 + MIN);
    log.snapshot(&[TASK], &[], T0 + 2 * MIN);
    // Still this run's claim, regardless of the absent task_id.
    let running_claim = log.push(
        None,
        None,
        Some(running.1.clone()),
        EventKind::RunClaimed,
        attributes("v2", 2),
        T0 + 10 * MIN,
    );
    // An end may precede the builder's run row while the saved task remains
    // unfinished and names the run. Both finish shapes must supplement it.
    log.end(
        &running,
        EventKind::ValidationFinished,
        json!({"status":"failed"}),
        T0 + 11 * MIN,
    );
    let running_end = EventId::new(log.last_id());
    log.push(Some(12), None, None, EventKind::TaskCreated, json!({}), T0);
    let missing = log.run(Some(12), attributes("hidden", 3), T0 + 11 * MIN);
    let hidden_claim = EventId::new(log.last_id() - 1);
    log.task_status(12, "completed", T0 + 12 * MIN + MIN / 2);
    let hidden_start = log.start("hidden", 3, T0 + 12 * MIN);
    let hidden_session = log.queue(EventKind::MarkRecorded,
        json!({"kind":"runtime_planner","planner_id":99,"label":"hidden session", "status":"failed"}), T0 + 13 * MIN);
    let hidden_snapshot = log.queue(
        EventKind::ForecastRecorded,
        json!({"kind":"runtime_planner","planner_id":98,"at_secs":(T0+3*MIN)/1000,
            "triggers_through":0,"tasks":[{"id":TASK,"p50_secs":1}]}),
        T0 + 3 * MIN,
    );
    // Queue-owned even with run/task ids; it meets ends_run, so the finish
    // reads must reject it through post-R visibility, not a kind filter.
    log.end(
        &missing,
        EventKind::SupervisorStopped,
        json!({"status":"failed"}),
        T0 + 13 * MIN + MIN / 2,
    );
    let hidden_queue_end = EventId::new(log.last_id());
    log.task_status(TASK, "completed", T0 + 14 * MIN);
    let hidden = [
        hidden_claim,
        hidden_start,
        hidden_session,
        hidden_snapshot,
        hidden_queue_end,
    ];
    for id in [hidden_session, hidden_snapshot] {
        let event = &log.events[(id.as_i64() - 1) as usize];
        assert_eq!(owner(event), Owner::Session);
        assert!(!stats::window::ends_run(event));
    }
    let queue_end = &log.events[(hidden_queue_end.as_i64() - 1) as usize];
    assert_eq!(owner(queue_end), Owner::Queue);
    assert!(stats::window::ends_run(queue_end));
    let queue = save(&log, &hidden);
    queue
        .conn
        .execute(
            "DELETE FROM ledger_rows WHERE kind='run' AND key=?1",
            [running.1.as_str()],
        )
        .unwrap();
    // The builder has not folded the validation failure into this task's
    // saved row yet. Folding this synthetic failure without execution records
    // would instead make a legacy row, whose legacy.runs cannot name a live
    // run for visibility. Preserve the actual pre-failure detail.runs view.
    let lagged = build(
        &log.events[..running_claim.as_i64() as usize],
        T0 + 1_000_000_000_000,
        &marks::utc_text(T0),
    )
    .into_iter()
    .find(|row| row.kind == LedgerKind::Task && row.key == "70")
    .unwrap();
    queue
        .conn
        .execute("DELETE FROM ledger_rows WHERE kind='task' AND key='70'", [])
        .unwrap();
    queue.write_ledger_row(&lagged).unwrap();
    // A completed task retains the name of its old run even when that run
    // has no row yet. Scanning all task rows instead of unfinished ones
    // would wrongly supplement this run.
    let mut ended = queue.ledger_row(LedgerKind::Task, "12").unwrap().unwrap();
    ended.detail["runs"] = json!([missing.1.as_str()]);
    queue.write_ledger_row(&ended).unwrap();
    lower_queue_mark(&queue, EventId::new(log.last_id()));
    queue
        .ledger_read_snapshot(&mut |read| {
            assert!(read.row(LedgerKind::Run, running.1.as_str())?.is_none());
            let task = read.row(LedgerKind::Task, "70")?.unwrap();
            assert!(task.ended_at.is_none());
            assert!(
                crate::domain::ledger::window::runs_named(&task).contains(&running.1.to_string())
            );
            assert!(read.row(LedgerKind::Run, missing.1.as_str())?.is_none());
            let ended = read.row(LedgerKind::Task, "12")?.unwrap();
            assert!(ended.ended_at.is_some());
            assert!(
                crate::domain::ledger::window::runs_named(&ended).contains(&missing.1.to_string())
            );
            let all = whole(read);
            assert!(all.iter().any(|event| event.id == running_claim));
            assert!(all.iter().any(|event| event.id == running_end));
            assert!(!all.iter().any(
                |event| event.run_id.as_ref() == Some(&missing.1) || hidden.contains(&event.id)
            ));
            let (start, end) = (T0 + 9 * MIN, T0 + 15 * MIN);
            let selected = complete(read.mark_events(start, end)?);
            assert_eq!(marks_at(&selected, start, end), marks_at(&all, start, end));
            let selected = complete(read.forecast_events(start, end)?);
            assert!(selected.iter().any(|event| event.id == running_claim));
            assert!(!selected.iter().any(|event| hidden.contains(&event.id)));
            let expected = scored(&all, start, end);
            assert_eq!(expected.samples.len(), 1);
            assert!(
                expected.samples[0].marks > 0,
                "the running run changes the forecast's marks"
            );
            assert_eq!(scored(&selected, start, end), expected);
            for range in [Some((start, end)), None] {
                let selected = complete(read.run_finishes(range)?);
                let mut expected: Vec<_> = stats::run_stats(&all)
                    .into_iter()
                    .filter_map(|run| {
                        let event = all
                            .iter()
                            .find(|event| Some(event.id) == run.finished_event_id)?;
                        let millis = ms(event);
                        range
                            .is_none_or(|(start, end)| millis > start && millis <= end)
                            .then_some(RunFinish {
                                run: run.run_id,
                                task: run.task_id,
                                event: event.id,
                                millis,
                            })
                    })
                    .collect();
                expected.sort_by(|a, b| a.run.cmp(&b.run));
                assert_eq!(expected.len(), 1);
                assert_eq!(expected[0].event, running_end);
                // A session-owner event has no run id and cannot satisfy
                // ends_run. It is kept in this shared fixture to verify that
                // neither shape leaks it; the hidden queue end DOES qualify.
                assert!(!selected.iter().any(|finish| hidden.contains(&finish.event)));
                assert_eq!(selected, expected);
            }
            let selected = complete(read.comparison_events()?);
            assert_eq!(
                selected,
                all.iter()
                    .filter(|event| event.kind.is_in(&marks::READ_KINDS))
                    .cloned()
                    .collect::<Vec<_>>()
            );
            let cursor = EventId::new(log.last_id());
            assert_eq!(
                complete(read.cursor_record_time(cursor)?).millis,
                marks::cursor_millis(Cursor::Event(cursor), &all)
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn a_recent_cursor_reads_the_same_number_with_different_settled_histories() {
    let mut counts = Vec::new();
    for units in [30, 3000] {
        let mut log = Log::default();
        log.settled(units, T0);
        let id = EventId::new(log.last_id());
        let queue = save(&log, &[]);
        let conn = Counted {
            conn: &*queue.conn,
            rows: Cell::new(0),
        };
        let result = complete(cursor_time(&conn, id).unwrap());
        assert_eq!(result.path, CursorTimePath::Recent);
        assert_eq!(
            result.millis,
            marks::cursor_millis(Cursor::Event(id), &log.events)
        );
        counts.push(conn.rows.get());
    }
    assert_eq!(counts[0], counts[1]);
}

#[test]
fn forecast_reads_scale_with_open_periods_and_marks_after_snapshots() {
    for after in [false, true] {
        let mut counts = Vec::new();
        for units in [30, 3000] {
            let (mut log, s, e, ()) = forecast_history(30, if after { 30 } else { units });
            if after {
                for n in 0..units {
                    let mark = log.mark("later", None, e + (30 + n) * MIN);
                    log.retract(mark, e + (30 + n) * MIN + 1);
                }
            }
            let queue = save(&log, &[]);
            let conn = Counted {
                conn: &*queue.conn,
                rows: Cell::new(0),
            };
            let selected = complete(forecast_events(&conn, s, e).unwrap());
            assert_eq!(scored(&selected, s, e), scored(&log.events, s, e));
            counts.push(conn.rows.get());
        }
        assert!(counts[1] > counts[0] * 10, "{counts:?}");
    }
}

#[test]
fn cursor_walk_reports_exhaustion_when_the_maximum_is_hidden() {
    let mut log = Log::default();
    log.mark("visible", None, T0);
    let missing = log.claim(Some(12), attributes("v1", 1), T0 + 100 * MIN);
    let hidden = EventId::new(log.last_id());
    for _ in 0..WINDOW_READ_CANDIDATES + 1 {
        log.end(
            &missing,
            EventKind::RunIntegrated,
            json!({}),
            T0 + 100 * MIN,
        );
    }
    let cursor = EventId::new(log.last_id());
    for n in 0..150 {
        log.mark("future", None, T0 + (1000 + n) * MIN);
    }
    let queue = save(&log, &[hidden]);
    queue
        .ledger_read_snapshot(&mut |read| {
            assert_eq!(read.cursor_record_time(cursor)?, WindowRead::Exhausted);
            assert_eq!(
                marks::cursor_millis(Cursor::Event(cursor), &whole(read)),
                T0
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn forecasts_seek_the_first_visible_target_event_when_no_previous_end_exists() {
    for count in [1, WINDOW_READ_CANDIDATES + 1] {
        let mut log = Log::default();
        let mut hidden = Vec::new();
        for _ in 0..count {
            log.claim(Some(9), attributes("v1", 1), T0);
            hidden.push(EventId::new(log.last_id()));
        }
        log.push(
            Some(9),
            None,
            None,
            EventKind::TaskCreated,
            json!({"title":"target"}),
            T0 + MIN,
        );
        log.snapshot(&[9], &[], T0 + 2 * MIN);
        log.task_status(9, "completed", T0 + 10 * MIN);
        let queue = save(&log, &hidden);
        queue
            .ledger_read_snapshot(&mut |read| {
                let result = read.forecast_events(T0 + 9 * MIN, T0 + 11 * MIN)?;
                if count > WINDOW_READ_CANDIDATES {
                    assert_eq!(result, WindowRead::Exhausted);
                } else {
                    let expected = scored(&whole(read), T0 + 9 * MIN, T0 + 11 * MIN);
                    assert_eq!(expected.samples.len(), 1);
                    assert_eq!(
                        scored(&complete(result), T0 + 9 * MIN, T0 + 11 * MIN),
                        expected
                    );
                }
                Ok(())
            })
            .unwrap();
    }
}
