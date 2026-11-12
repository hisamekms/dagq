//! [`super`]'s range and reads against the answer of every event. The
//! store ([`Store`]) keeps the events in the order they were written (their
//! ids) with the time each clock gave, which may go back; each of its reads
//! counts what it returns (a walk, what it walked). Every test builds its
//! history with a small and a large settled history outside the range and
//! compares the answers with every event's and the counts with each other.
use std::cell::Cell;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::*;
use crate::domain::forecast::score::{self, Scoring};
use crate::domain::kpi::{CompareSpec, DAY_MS, KpiInput, Period, kpi, kpi_with};
use crate::domain::ledger::window::Earlier;
use crate::domain::ledger::window::bounds::{Answers, Bound};
use crate::domain::marks::{self, Mark};
use crate::domain::stats::{Cursor, timestamp_millis, window::ends_run};
use crate::domain::{EventId, EventKind, GoalId, RunEvent, RunId, TaskId};

const MIN: i64 = 60_000;
const HOUR: i64 = 60 * MIN;
/// 2026-10-01T00:00:00Z.
const T0: i64 = 1_790_812_800_000;
/// The entries the time index's search for an event cursor stops after
/// (the port's `WINDOW_READ_CANDIDATES`).
const WALK: usize = 100;

fn created_ms(event: &RunEvent) -> i64 {
    timestamp_millis(&event.created_at).unwrap_or(i64::MIN)
}

fn is_queue_event(event: &RunEvent) -> bool {
    event.task_id.is_none() && event.goal_id.is_none() && event.run_id.is_none()
}

/// When a recorded mark took effect, as [`marks::marks`] reads it.
fn effective_ms(event: &RunEvent) -> i64 {
    match event.kind.known() {
        Some(EventKind::MarkRecorded) => event
            .payload
            .get("at")
            .and_then(Value::as_str)
            .and_then(timestamp_millis),
        Some(EventKind::SupervisorStopped) if event.payload["outcome"] == "pruned" => event
            .payload
            .get("last_heartbeat_at")
            .and_then(Value::as_i64)
            .map(|secs| secs * 1000),
        _ => None,
    }
    .unwrap_or_else(|| created_ms(event))
}

/// A forecast target's finish ([`score::score`]).
fn is_finish(event: &RunEvent) -> bool {
    match event.kind.known() {
        Some(EventKind::TaskStatusChanged) => {
            event.task_id.is_some()
                && matches!(event.payload["to"].as_str(), Some("completed" | "canceled"))
        }
        Some(EventKind::GoalClosed) => event.goal_id.is_some(),
        _ => false,
    }
}

fn mark_ms(mark: &Mark) -> i64 {
    timestamp_millis(&mark.at).unwrap_or(i64::MIN)
}

/// The store's small reads of events in memory, counting what they return.
/// Events it holds but the snapshot of the ledger does not see (`hidden`:
/// a run on its way, an event written after the snapshot) are never
/// returned, as the store's reads pass only what `visible_events` keeps.
struct Store {
    events: Vec<RunEvent>,
    hidden: HashSet<EventId>,
    read: Cell<usize>,
}

impl Store {
    fn new(events: Vec<RunEvent>) -> Self {
        Self::hiding(events, HashSet::new())
    }

    fn hiding(events: Vec<RunEvent>, hidden: HashSet<EventId>) -> Self {
        Self {
            events,
            hidden,
            read: Cell::new(0),
        }
    }

    /// The events the snapshot sees, ascending id.
    fn visible(&self) -> impl DoubleEndedIterator<Item = &RunEvent> {
        self.events.iter().filter(|e| !self.hidden.contains(&e.id))
    }

    fn count(&self, n: usize) {
        self.read.set(self.read.get() + n);
    }

    fn pick(&self, keep: impl Fn(&RunEvent) -> bool) -> Vec<RunEvent> {
        let picked: Vec<RunEvent> = self.visible().filter(|e| keep(e)).cloned().collect();
        self.count(picked.len());
        picked
    }

    fn seek(&self, found: Option<&RunEvent>) -> Option<RunEvent> {
        self.count(usize::from(found.is_some()));
        found.cloned()
    }
}

impl RangeReads for Store {
    fn recorded_effective(&self, from_ms: i64, to_ms: i64) -> Read<Vec<RunEvent>> {
        Ok(self.pick(|e| {
            is_queue_event(e)
                && e.kind.is_in(&marks::RECORDED_KINDS)
                && effective_ms(e) > from_ms
                && effective_ms(e) <= to_ms
        }))
    }

    fn retractions_of(&self, mark: EventId) -> Read<Vec<RunEvent>> {
        Ok(self.pick(|e| {
            e.kind == EventKind::MarkRetracted
                && is_queue_event(e)
                && e.payload.get("mark").and_then(Value::as_i64) == Some(mark.as_i64())
        }))
    }

    fn claims_written(&self, from_ms: i64, to_ms: i64) -> Read<Vec<RunEvent>> {
        Ok(self.pick(|e| {
            e.kind == EventKind::RunClaimed
                && e.run_id.is_some()
                && created_ms(e) > from_ms
                && created_ms(e) <= to_ms
        }))
    }

    fn previous_claim_with(&self, attribute: &str, before: EventId) -> Read<Option<RunEvent>> {
        Ok(self.seek(self.visible().rev().find(|e| {
            e.id < before
                && e.kind == EventKind::RunClaimed
                && e.run_id.is_some()
                && marks::claim_value(&e.payload, attribute).is_some()
        })))
    }

    fn start_announcing(
        &self,
        key: &str,
        value: &str,
        after: EventId,
        before: EventId,
    ) -> Read<Option<RunEvent>> {
        Ok(self.seek(self.visible().find(|e| {
            e.kind == EventKind::SupervisorStarted
                && is_queue_event(e)
                && e.id > after
                && e.id < before
                && marks::claim_value(&e.payload, key).as_deref() == Some(value)
        })))
    }

    fn start_before_in_time(&self, ms: i64) -> Read<Option<RunEvent>> {
        Ok(self.seek(
            self.visible()
                .filter(|e| {
                    e.kind == EventKind::SupervisorStarted
                        && is_queue_event(e)
                        && created_ms(e) <= ms
                })
                .max_by_key(|e| (created_ms(e), e.id)),
        ))
    }

    /// The port's three paths (`CursorTimePath`): the [`WALK`] latest
    /// events by `(time, id)`, whose first visible one at or before
    /// `cursor` holds the latest time of those; else the aggregate
    /// `MAX(created_at)` of the events at or before `cursor`, taken when a
    /// visible event has that time; else the walk down the time index among
    /// the events at or before `cursor`, at most [`WALK`] entries, to the
    /// first visible one. Only that walk's exhaustion leaves the time
    /// undecided.
    fn cursor_time(&self, cursor: EventId) -> Read<Option<i64>> {
        let visible = |e: &RunEvent| !self.hidden.contains(&e.id);
        let mut by_time: Vec<&RunEvent> = self.events.iter().collect();
        by_time.sort_by_key(|e| Reverse((created_ms(e), e.id)));
        let recent = by_time
            .iter()
            .take(WALK)
            .position(|e| e.id <= cursor && visible(e));
        self.count(recent.map_or(WALK.min(by_time.len()), |at| at + 1));
        if let Some(at) = recent {
            return Ok(Some(created_ms(by_time[at])));
        }
        let through: Vec<&RunEvent> = by_time.into_iter().filter(|e| e.id <= cursor).collect();
        let Some(max) = through.first().map(|e| created_ms(e)) else {
            return Ok(None);
        };
        self.count(1);
        if through
            .iter()
            .take_while(|e| created_ms(e) == max)
            .any(|e| visible(e))
        {
            return Ok(Some(max));
        }
        let walked = through.iter().take(WALK).position(|e| visible(e));
        self.count(walked.map_or(WALK.min(through.len()), |at| at + 1));
        match walked {
            Some(at) => Ok(Some(created_ms(through[at]))),
            None if through.len() <= WALK => Ok(None),
            None => Err(Undecided::OutOfTimeOrder(cursor)),
        }
    }

    fn finishes_written(&self, from_ms: i64, to_ms: i64) -> Read<Vec<RunEvent>> {
        Ok(self.pick(|e| is_finish(e) && created_ms(e) > from_ms && created_ms(e) <= to_ms))
    }

    fn previous_finish(&self, finish: &RunEvent) -> Read<Option<RunEvent>> {
        Ok(self.seek(self.visible().rev().find(|e| {
            e.id < finish.id
                && is_finish(e)
                && match finish.task_id {
                    Some(task) => e.task_id == Some(task),
                    None => e.task_id.is_none() && e.goal_id == finish.goal_id,
                }
        })))
    }

    fn first_of_target(&self, finish: &RunEvent) -> Read<Option<RunEvent>> {
        Ok(self.seek(self.visible().find(|e| match finish.task_id {
            Some(task) => e.task_id == Some(task),
            None => e.goal_id == finish.goal_id,
        })))
    }

    fn snapshots_between(&self, after: EventId, before: EventId) -> Read<Vec<RunEvent>> {
        Ok(self.pick(|e| e.kind == EventKind::ForecastRecorded && e.id > after && e.id < before))
    }

    fn plan_changes_written(&self, from_ms: i64, to_ms: i64) -> Read<Vec<RunEvent>> {
        Ok(self.pick(|e| {
            e.kind.is_in(&score::TASK_MARK_KINDS)
                && e.task_id.is_some()
                && created_ms(e) > from_ms
                && created_ms(e) <= to_ms
        }))
    }

    fn marks_after(&self, after: EventId) -> Read<Vec<RunEvent>> {
        Ok(self.pick(|e| {
            e.id > after
                && ((is_queue_event(e) && e.kind.is_in(&marks::RECORDED_KINDS))
                    || e.kind.is_in(&score::TASK_MARK_KINDS))
        }))
    }

    fn run_finishes(&self) -> Read<Vec<(TaskId, i64)>> {
        let mut seen = HashSet::new();
        let finishes: Vec<(TaskId, i64)> = self
            .visible()
            .filter(|e| ends_run(e))
            .filter(|e| {
                e.run_id
                    .as_ref()
                    .is_some_and(|run| seen.insert(run.clone()))
            })
            .filter_map(|e| Some((e.task_id?, created_ms(e))))
            .collect();
        self.count(finishes.len());
        Ok(finishes)
    }

    fn comparison_events(&self) -> Read<Vec<RunEvent>> {
        Ok(self.pick(|e| e.kind.is_in(&marks::READ_KINDS)))
    }
}

/// A history in the order it was written; each event carries the time its
/// clock gave, which may go back.
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

    /// A new run of `task` (one of its own when none) claimed at `at`.
    fn claim(&mut self, task: Option<i64>, payload: Value, at: i64) -> (i64, RunId, EventId) {
        self.next_run += 1;
        let n = self.next_run;
        let task = task.unwrap_or(100_000 + n);
        let run = RunId::new(format!("00000000-0000-4000-8000-{n:012}")).unwrap();
        let id = self.push(
            Some(task),
            None,
            Some(run.clone()),
            EventKind::RunClaimed,
            payload,
            at,
        );
        (task, run, id)
    }

    fn end(&mut self, (task, run): (i64, &RunId), kind: EventKind, payload: Value, at: i64) {
        self.push(Some(task), None, Some(run.clone()), kind, payload, at);
    }

    /// A run claimed at `at` and landed `took` later.
    fn run_for(&mut self, task: Option<i64>, payload: Value, at: i64, took: i64) -> EventId {
        let (task, run, claim) = self.claim(task, payload, at);
        self.end(
            (task, &run),
            EventKind::RunIntegrated,
            json!({"commit": format!("c{}", self.next_run)}),
            at + took,
        );
        claim
    }

    fn run(&mut self, task: Option<i64>, payload: Value, at: i64) -> EventId {
        self.run_for(task, payload, at, MIN)
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

    /// A snapshot at `at` that forecast `tasks` and `goals`, having looked
    /// at every event written so far.
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

    /// A snapshot without `triggers_through`: a mark is after it by time.
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

    fn created(&mut self, task: i64, at: i64) {
        self.push(
            Some(task),
            None,
            None,
            EventKind::TaskCreated,
            json!({"title": "t"}),
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

    /// `units` settled runs ending at `end`, ten minutes apart, a routine
    /// start before every fifth, a mark retracted at once and a snapshot of
    /// nothing open before every tenth: the history outside a range the
    /// tests grow. Returns `end`.
    fn settled_until(&mut self, units: i64, end: i64) -> i64 {
        let at = end - units * 10 * MIN;
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
            self.run(None, attributes("v1", 4, None), t + 4);
        }
        end
    }

    /// The same from `at`; returns the time after it.
    fn settled(&mut self, units: i64, at: i64) -> i64 {
        self.settled_until(units, at + units * 10 * MIN)
    }

    fn store(&self) -> Store {
        Store::new(self.events.clone())
    }
}

fn attributes(version: &str, parallel: i64, codex: Option<&str>) -> Value {
    json!({
        "dagq_version": version,
        "claude_version": "2.1.0",
        "codex_version": codex,
        "rustc_release": "1.99.1",
        "rustc_host": "aarch64-apple-darwin",
        "parallel": parallel,
    })
}

fn marks_in(store: &Store, from: i64, to: i64) -> Vec<Mark> {
    marks::marks(
        &range_events(store, from, to).unwrap(),
        Some(Cursor::Time(from)),
        Some(Cursor::Time(to)),
    )
}

fn whole_marks(store: &Store, from: i64, to: i64) -> Vec<Mark> {
    marks::marks(
        &store.events,
        Some(Cursor::Time(from)),
        Some(Cursor::Time(to)),
    )
}

/// The splitting marks of `(from, to]` of `events`, each with whether it
/// is routine.
fn classified(events: &[RunEvent], from: i64, to: i64) -> Vec<(Mark, bool)> {
    let all = marks::marks(events, None, Some(Cursor::Time(to)));
    compare::classified(&all)
        .into_iter()
        .filter(|(mark, _)| mark_ms(mark) > from)
        .map(|(mark, routine)| (mark.clone(), routine))
        .collect()
}

/// Runs `case` on the history `build` makes with 30 and with 3000 settled
/// units outside the range and returns what each read.
fn reads_with<T>(
    build: impl Fn(i64) -> (Log, i64, i64, T),
    case: impl Fn(&Store, i64, i64, &T),
) -> (usize, usize) {
    let mut counts = [0, 0];
    for (slot, units) in [(0, 30), (1, 3000)] {
        let (log, from, to, extra) = build(units);
        let store = log.store();
        case(&store, from, to, &extra);
        counts[slot] = store.read.get();
    }
    (counts[0], counts[1])
}

mod forecast_and_cohort;
mod kpi_answers;
mod marks_and_cursors;
