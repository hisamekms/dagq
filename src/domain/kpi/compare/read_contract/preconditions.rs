//! The read contract of [`super`] tried against three more states
//! (docs/plans/ledger-window-read-contract-spike.md, (vi) to (viii)): a
//! clock that turns back or runs apart from the queue's, the forecast's
//! scoring, and the finishes `--compare`'s groups are cut by. Not wired:
//! the domain's functions ([`marks::marks`], [`super::super::routine`],
//! [`score::score`], [`stats::run_stats`]) are given the few events
//! [`Store`] picks instead of every one, and each test compares the answer
//! with that of every event and the events read with a small and a large
//! settled history outside the range.
//!
//! [`Store`]'s events keep their ids in the order they were written, and
//! their times are what each clock said: a time may go back as the ids go
//! on. Each read names the index it stands for.

use std::cell::Cell;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::{DERIVED, claim_value, created_ms, dedup, effective_ms, is_queue_event, mark_ms};
use crate::domain::forecast::score::{self, Scoring};
use crate::domain::marks::{self, Mark};
use crate::domain::stats::{self, Cursor, window::ends_run};
use crate::domain::{EventId, EventKind, GoalId, RunEvent, RunId, TaskId};

/// The kinds of a forecast target's finish ([`score::score`]).
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

/// `score`'s private `TASK_MARK_KINDS`.
const TASK_MARK_KINDS: [EventKind; 5] = [
    EventKind::TaskPriorityChanged,
    EventKind::DependencyAdded,
    EventKind::DependencyRemoved,
    EventKind::GoalDependencyAdded,
    EventKind::GoalDependencyRemoved,
];

/// An in-memory store of the events (ascending id, any times) whose reads
/// count what they return, or for a scan what it walked.
struct Store {
    events: Vec<RunEvent>,
    read: Cell<usize>,
}

impl Store {
    fn new(events: Vec<RunEvent>) -> Self {
        Self {
            events,
            read: Cell::new(0),
        }
    }

    fn count(&self, n: usize) {
        self.read.set(self.read.get() + n);
    }

    fn pick(&self, keep: impl Fn(&RunEvent) -> bool) -> Vec<RunEvent> {
        let picked: Vec<RunEvent> = self.events.iter().filter(|e| keep(e)).cloned().collect();
        self.count(picked.len());
        picked
    }

    fn seek(&self, found: Option<&RunEvent>) -> Option<RunEvent> {
        self.count(usize::from(found.is_some()));
        found.cloned()
    }

    fn newest_ms(&self) -> i64 {
        self.events.iter().map(created_ms).max().unwrap_or(i64::MIN)
    }

    /// Needs the effective time's index (`run_events(<the effective time>)`
    /// partial to the queue's recorded marks, [`effective_ms`]): those that
    /// took effect in `(from, to]`, whenever and by whatever clock written.
    fn recorded_effective(&self, from: i64, to: i64) -> Vec<RunEvent> {
        self.pick(|e| {
            is_queue_event(e)
                && e.kind.is_in(&marks::RECORDED_KINDS)
                && effective_ms(e) > from
                && effective_ms(e) <= to
        })
    }

    /// Needs `run_events(kind, created_at, id)` (or `events_by_kind` read
    /// by id when the clocks are taken as one): the events of `kinds`
    /// written in `(from, to]`, `keep` among them.
    fn written(
        &self,
        kinds: &[EventKind],
        from: i64,
        to: i64,
        keep: impl Fn(&RunEvent) -> bool,
    ) -> Vec<RunEvent> {
        self.pick(|e| e.kind.is_in(kinds) && created_ms(e) > from && created_ms(e) <= to && keep(e))
    }

    /// Today's `events_by_kind(kind, id)`: the events of `kinds` after
    /// `after`, up to now.
    fn after_id(&self, kinds: &[EventKind], after: EventId) -> Vec<RunEvent> {
        self.pick(|e| e.kind.is_in(kinds) && e.id > after)
    }

    /// Needs `run_events(kind, json_extract(payload, '$.mark'), id)`.
    fn retractions_of(&self, mark: EventId) -> Vec<RunEvent> {
        self.pick(|e| {
            e.kind == EventKind::MarkRetracted
                && is_queue_event(e)
                && e.payload.get("mark").and_then(Value::as_i64) == Some(mark.as_i64())
        })
    }

    /// The claims written in `(from, to]`.
    fn claims_written(&self, from: i64, to: i64) -> Vec<RunEvent> {
        self.written(&[EventKind::RunClaimed], from, to, |e| e.run_id.is_some())
    }

    /// Needs a partial index per attribute (`run_events(kind, id) WHERE
    /// json_extract(payload, '$.<attribute>') IS NOT NULL`): one seek back
    /// by id from `before`.
    fn previous_claim_with(&self, attribute: &str, before: EventId) -> Option<RunEvent> {
        self.seek(self.events.iter().rev().find(|e| {
            e.id < before
                && e.kind == EventKind::RunClaimed
                && e.run_id.is_some()
                && claim_value(&e.payload, attribute).is_some()
        }))
    }

    /// Needs `run_events(kind, json_extract(payload, '$.<key>'), id)`.
    fn start_announcing(
        &self,
        key: &str,
        value: &str,
        after: EventId,
        before: EventId,
    ) -> Option<RunEvent> {
        self.seek(self.events.iter().find(|e| {
            e.kind == EventKind::SupervisorStarted
                && is_queue_event(e)
                && e.id > after
                && e.id < before
                && claim_value(&e.payload, key).as_deref() == Some(value)
        }))
    }

    /// Needs `run_events(kind, created_at, id)` for the starts: one seek
    /// back in time, the start last in `(time, id)` at or before `ms`.
    fn start_before_in_time(&self, ms: i64) -> Option<RunEvent> {
        self.seek(
            self.events
                .iter()
                .filter(|e| {
                    e.kind == EventKind::SupervisorStarted
                        && is_queue_event(e)
                        && created_ms(e) <= ms
                })
                .max_by_key(|e| (created_ms(e), e.id)),
        )
    }

    /// Today's `events_by_kind('supervisor_started', id)`: one seek back by
    /// id from `before` (Spike 4135's (iv)).
    fn start_before_in_id(&self, before: EventId) -> Option<RunEvent> {
        self.seek(
            self.events.iter().rev().find(|e| {
                e.id < before && e.kind == EventKind::SupervisorStarted && is_queue_event(e)
            }),
        )
    }

    /// The latest time of the events at or before `id`
    /// ([`marks::cursor_millis`]) by `run_events(created_at, id)` walked
    /// down from the latest time to the first event at or before `id`,
    /// counting every entry walked.
    fn latest_time_through(&self, id: EventId) -> i64 {
        let mut by_time: Vec<&RunEvent> = self.events.iter().collect();
        by_time.sort_by_key(|e| Reverse((created_ms(e), e.id)));
        let mut walked = 0;
        for event in by_time {
            walked += 1;
            if event.id <= id {
                self.count(walked);
                return created_ms(event);
            }
        }
        self.count(walked);
        i64::MIN
    }

    /// The same walk stopped after `limit` entries (`SELECT MAX(created_at)
    /// FROM (SELECT id, created_at FROM run_events ORDER BY created_at
    /// DESC, id DESC LIMIT ?limit) WHERE id <= ?id`): the first entry at or
    /// before `id` is the latest time of those, whatever the clocks did;
    /// `None` when none of them is, and the read falls back to every event.
    fn latest_time_within(&self, id: EventId, limit: usize) -> Option<i64> {
        let mut by_time: Vec<&RunEvent> = self.events.iter().collect();
        by_time.sort_by_key(|e| Reverse((created_ms(e), e.id)));
        let found = by_time.iter().take(limit).position(|e| e.id <= id);
        self.count(found.map_or(limit.min(by_time.len()), |at| at + 1));
        found.map(|at| created_ms(by_time[at]))
    }

    /// `Cursor::Time(ms).event_id` (the largest id of an event written at
    /// or before `ms`, which `stats` picks a period's runs by) by the id
    /// index walked down from the newest to the first event written at or
    /// before `ms`, counting every entry walked: every event walked is
    /// later in id and written after `ms`.
    fn last_id_through(&self, ms: i64) -> EventId {
        let mut walked = 0;
        for event in self.events.iter().rev() {
            walked += 1;
            if created_ms(event) <= ms {
                self.count(walked);
                return event.id;
            }
        }
        self.count(walked);
        EventId::new(0)
    }

    /// The same walk stopped after `limit` entries
    /// (`SELECT MAX(id) FROM (SELECT id, created_at FROM run_events ORDER
    /// BY id DESC LIMIT ?limit) WHERE created_at <= ?ms`): `None` when the
    /// first event written at or before `ms` is not among them, which a
    /// clock turned back, a DB's clock ahead of the reader's or a range in
    /// the past with more written after it all give.
    fn last_id_within(&self, ms: i64, limit: usize) -> Option<EventId> {
        let found = self
            .events
            .iter()
            .rev()
            .take(limit)
            .position(|event| created_ms(event) <= ms);
        self.count(found.map_or(limit.min(self.events.len()), |at| at + 1));
        found.map(|at| self.events[self.events.len() - 1 - at].id)
    }

    /// Needs a partial index of the finishes by their target
    /// (`run_events(task_id, id)` and `(goal_id, id)` where [`is_finish`]):
    /// the target's last finish before `before`.
    fn previous_finish(&self, of: &RunEvent, before: EventId) -> Option<RunEvent> {
        self.seek(self.events.iter().rev().find(|e| {
            e.id < before
                && is_finish(e)
                && match of.task_id {
                    Some(task) => e.task_id == Some(task),
                    None => e.task_id.is_none() && e.goal_id == of.goal_id,
                }
        }))
    }

    /// Today's `events_by_task(task_id, id)` or `events_by_goal(goal_id,
    /// id)`: the first event of a finish's target, one seek (no snapshot
    /// forecasts a target before it exists).
    fn first_of(&self, finish: &RunEvent) -> Option<RunEvent> {
        self.seek(self.events.iter().find(|e| match finish.goal_id {
            Some(goal) => e.goal_id == Some(goal),
            None => e.task_id == finish.task_id,
        }))
    }

    /// Today's `events_by_kind('forecast_recorded', id)` over `(after,
    /// before)`: SQLite has no index of a snapshot's targets, so every
    /// snapshot of the ids is read.
    fn snapshots_between(&self, after: EventId, before: EventId) -> Vec<RunEvent> {
        self.pick(|e| e.kind == EventKind::ForecastRecorded && e.id > after && e.id < before)
    }

    /// Needs a partial index of the runs' ends ([`ends_run`]) by their time
    /// (`run_events(created_at, id) WHERE <ends_run>`): the ends written
    /// in `(from, to]`.
    fn ends_written(&self, from: i64, to: i64) -> Vec<RunEvent> {
        self.pick(|e| ends_run(e) && created_ms(e) > from && created_ms(e) <= to)
    }

    /// Needs the same predicate by run (`run_events(run_id, id) WHERE
    /// <ends_run>`): a run's first end, one seek.
    fn first_end(&self, run: &RunId) -> Option<RunEvent> {
        self.seek(
            self.events
                .iter()
                .find(|e| e.run_id.as_ref() == Some(run) && ends_run(e)),
        )
    }
}

/// The events of the derived marks of `claims`: each claim's previous
/// claim by id per attribute, one seek each however the clocks ran, and a
/// start between the two that announces the new value.
fn derived_reads(store: &Store, claims: &[RunEvent]) -> Vec<RunEvent> {
    let mut earlier = Vec::new();
    for claim in claims {
        for (attribute, start_key) in DERIVED {
            let Some(value) = claim_value(&claim.payload, attribute) else {
                continue;
            };
            let Some(previous) = store.previous_claim_with(attribute, claim.id) else {
                continue;
            };
            if let Some(key) = start_key
                && claim_value(&previous.payload, attribute).as_ref() != Some(&value)
            {
                earlier.extend(store.start_announcing(key, &value, previous.id, claim.id));
            }
            earlier.push(previous);
        }
    }
    earlier
}

/// The contract for the marks of `(from, to]` with any clocks: the
/// recorded marks by their effective time, their retractions by the mark,
/// the claims by their time with each one's previous claim by id, and the
/// start last in time at or before `from`.
fn range_events(store: &Store, from: i64, to: i64) -> Vec<RunEvent> {
    let recorded = store.recorded_effective(from, to);
    let retractions: Vec<RunEvent> = recorded
        .iter()
        .filter(|mark| mark.kind.is_in(&marks::RETRACTABLE_KINDS))
        .flat_map(|mark| store.retractions_of(mark.id))
        .collect();
    let claims = store.claims_written(from, to);
    let earlier = derived_reads(store, &claims);
    dedup(
        recorded
            .into_iter()
            .chain(retractions)
            .chain(claims)
            .chain(earlier)
            .chain(store.start_before_in_time(from)),
    )
}

fn marks_in(store: &Store, from: i64, to: i64) -> Vec<Mark> {
    marks::marks(
        &range_events(store, from, to),
        Some(Cursor::Time(from)),
        Some(Cursor::Time(to)),
    )
}

/// The splitting marks of `(from, to]` of `events`, each with whether it
/// is routine.
fn classified(events: &[RunEvent], from: i64, to: i64) -> Vec<(Mark, bool)> {
    let all = marks::marks(events, None, Some(Cursor::Time(to)));
    super::super::classified(&all)
        .into_iter()
        .filter(|(mark, _)| mark_ms(mark) > from)
        .map(|(mark, routine)| (mark.clone(), routine))
        .collect()
}

/// The forecast's scoring cut to the finishes in `(from, to]`, as
/// `forecast_kpis` keeps it.
fn scoring_in(scoring: Scoring, from: i64, to: i64) -> Scoring {
    let within = |ms: i64| ms > from && ms <= to;
    Scoring {
        samples: scoring
            .samples
            .into_iter()
            .filter(|sample| within(sample.finished_ms))
            .collect(),
        excluded: scoring
            .excluded
            .into_iter()
            .filter(|(_, ms, _)| within(*ms))
            .collect(),
    }
}

/// The contract for the forecast's samples that finished in `(from, to]`,
/// in two steps. First the finishes written in the range and each one's
/// target's previous finish (by id). Then, from those, a second range: the
/// snapshots between the earliest previous finish and the latest finish,
/// every recorded mark and change of a task's plan after the earliest
/// snapshot's `triggers_through` up to now (a mark is after a snapshot by
/// its id however far back it took effect), those that took effect or
/// were written from the earliest snapshot's time to `to`, the
/// retractions of the marks, and the claims of that span with their
/// previous ones.
fn forecast_events(store: &Store, from: i64, to: i64) -> Vec<RunEvent> {
    let finishes = store.written(
        &[EventKind::TaskStatusChanged, EventKind::GoalClosed],
        from,
        to,
        is_finish,
    );
    let Some(high) = finishes.iter().map(|finish| finish.id).max() else {
        return Vec::new();
    };
    let mut previous = Vec::new();
    let mut low = high;
    for finish in &finishes {
        // A target's snapshots of this finish follow its previous finish,
        // or its first event when it has none.
        let bound = match store.previous_finish(finish, finish.id) {
            Some(before) => {
                let id = before.id;
                previous.push(before);
                id
            }
            None => store
                .first_of(finish)
                .map_or(EventId::new(0), |first| EventId::new(first.id.as_i64() - 1)),
        };
        low = low.min(bound);
    }
    let snapshots = store.snapshots_between(low, EventId::new(high.as_i64() + 1));
    let Some(at) = snapshots
        .iter()
        .filter_map(|s| s.payload["at_secs"].as_i64())
        .min()
    else {
        return dedup(finishes.into_iter().chain(previous));
    };
    let at = at * 1000;
    let seen = snapshots
        .iter()
        .filter_map(|s| s.payload["triggers_through"].as_i64())
        .filter(|&through| through > 0)
        .min();
    let mut recorded = store.recorded_effective(at, to);
    let mut task_marks = store.written(&TASK_MARK_KINDS, at, to, |e| e.task_id.is_some());
    if let Some(seen) = seen {
        recorded.extend(
            store
                .after_id(&marks::RECORDED_KINDS, EventId::new(seen))
                .into_iter()
                .filter(is_queue_event),
        );
        task_marks.extend(store.after_id(&TASK_MARK_KINDS, EventId::new(seen)));
    }
    let recorded = dedup(recorded);
    let retractions: Vec<RunEvent> = recorded
        .iter()
        .filter(|mark| mark.kind.is_in(&marks::RETRACTABLE_KINDS))
        .flat_map(|mark| store.retractions_of(mark.id))
        .collect();
    let claims = store.claims_written(at, to);
    let earlier = derived_reads(store, &claims);
    dedup(
        finishes
            .into_iter()
            .chain(previous)
            .chain(snapshots)
            .chain(recorded)
            .chain(retractions)
            .chain(task_marks)
            .chain(claims)
            .chain(earlier),
    )
}

fn forecast_in(store: &Store, from: i64, to: i64) -> Scoring {
    let events = forecast_events(store, from, to);
    let marks = marks::marks(&events, None, None);
    scoring_in(score::score(&events, &marks), from, to)
}

/// The finish times of the runs whose first end ([`ends_run`], the
/// `finished_event_id` of `stats`) is in `(from, to]`, of the runs `counts`
/// keeps: the ends written in the range, and for each run of them its
/// first end by one seek, kept when that one is in the range.
fn finishes_in(
    store: &Store,
    from: i64,
    to: i64,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> Vec<i64> {
    let mut runs = HashSet::new();
    let mut finishes: Vec<i64> = store
        .ends_written(from, to)
        .into_iter()
        .filter(|end| counts(end.task_id))
        .filter_map(|end| end.run_id)
        .filter(|run| runs.insert(run.clone()))
        .filter_map(|run| store.first_end(&run))
        .map(|first| created_ms(&first))
        .filter(|&ms| ms > from && ms <= to)
        .collect();
    finishes.sort_unstable();
    finishes
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;
    /// 2026-10-01T00:00:00Z.
    const T0: i64 = 1_790_812_800_000;
    const MIN_SAMPLES: usize = 5;

    /// A history in the order it was written; each event carries the time
    /// its clock gave, which may go back.
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

        fn store(&self) -> Store {
            Store::new(self.events.clone())
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

    fn whole_marks(store: &Store, from: i64, to: i64) -> Vec<Mark> {
        marks::marks(
            &store.events,
            Some(Cursor::Time(from)),
            Some(Cursor::Time(to)),
        )
    }

    /// Runs `case` on the history `build` makes with 30 and with 300
    /// settled units and returns what each read.
    fn reads_with<T>(
        build: impl Fn(i64) -> (Log, i64, i64, T),
        case: impl Fn(&Store, i64, i64, &T),
    ) -> (usize, usize) {
        let mut counts = [0, 0];
        for (slot, units) in [(0, 30), (1, 300)] {
            let (log, from, to, extra) = build(units);
            let store = log.store();
            case(&store, from, to, &extra);
            counts[slot] = store.read.get();
        }
        (counts[0], counts[1])
    }

    // (vi) --------------------------------------------------------------

    /// The description's example: starts written (0 min, parallel 1),
    /// (20 min, 1), (10 min, 2), the clock turned back before the third.
    /// In `(15, 25]` the start of 20 min follows the one of 10 min in time,
    /// whose `parallel` differs: not routine. By id its previous start is
    /// the first, which would make it routine.
    #[test]
    fn vi_routine_takes_the_start_before_it_in_time_not_in_id() {
        let build = |units| {
            let mut log = Log::default();
            let base = log.settled(units, T0);
            log.start("v1", 1, base);
            let second = log.start("v1", 1, base + 20 * MIN);
            log.start("v1", 2, base + 10 * MIN);
            (log, base + 15 * MIN, base + 25 * MIN, second)
        };
        let (small, large) = reads_with(build, |store, from, to, second| {
            let whole = classified(&store.events, from, to);
            assert_eq!(whole.len(), 1);
            assert!(!whole[0].1, "not routine of every event");
            let events = range_events(store, from, to);
            assert_eq!(classified(&events, from, to), whole);
            // Spike 4135's (iv): the previous start by id.
            let mut by_id = store.claims_written(from, to);
            by_id.extend(store.recorded_effective(from, to));
            by_id.extend(store.start_before_in_id(*second));
            let by_id = dedup(by_id);
            assert!(classified(&by_id, from, to)[0].1, "routine by id");
        });
        assert_eq!(small, large);
    }

    /// Claims written v1 (in the range), v2 (the clock turned back out of
    /// it), v2 (in it): of every event the third follows the second in id
    /// and is no change. Spike 4135's read (the last claim before the
    /// range's first id, then the range's claims) compares the third with
    /// the first and finds a change; one seek back by id per claim does not.
    #[test]
    fn vi_a_claim_is_compared_with_the_claim_before_it_in_id() {
        let build = |units| {
            let mut log = Log::default();
            let base = log.settled(units, T0);
            log.run(None, attributes("v1", 4), base + 100 * MIN);
            log.run(None, attributes("v2", 4), base - 50 * MIN);
            log.run(None, attributes("v2", 4), base + 110 * MIN);
            (log, base + 90 * MIN, base + 120 * MIN, ())
        };
        let (small, large) = reads_with(build, |store, from, to, ()| {
            let whole = whole_marks(store, from, to);
            assert!(whole.is_empty(), "{whole:?}");
            assert_eq!(marks_in(store, from, to), whole);
            let reads = super::super::Reads::new(store.events.clone(), Vec::new());
            let by_first_id = marks::marks(
                &super::super::range_events(&reads, from, to),
                Some(Cursor::Time(from)),
                Some(Cursor::Time(to)),
            );
            assert_eq!(by_first_id.len(), 1, "{by_first_id:?}");
        });
        assert_eq!(small, large);
    }

    /// A start written between a claim before the range and a claim of it
    /// by id, but by a clock ahead past the range's end, announces the
    /// range claim's new build: of every event that change is the start's
    /// and derives no mark. The start is in no read by time (not in the
    /// range, not the last start before it); the seek of a start that
    /// announces the value between the two claims by id finds it, and
    /// without it the read would derive `dagq_version v1 → v2`.
    #[test]
    fn vi_a_start_between_the_claims_by_id_is_sought_whatever_its_time() {
        let build = |units| {
            let mut log = Log::default();
            let from = log.settled(units, T0);
            let to = from + 60 * MIN;
            let start = log.start("v2", 4, to + 60 * MIN);
            log.run(None, attributes("v2", 4), from + 10 * MIN);
            (log, from, to, start)
        };
        let (small, large) = reads_with(build, |store, from, to, start| {
            let whole = whole_marks(store, from, to);
            assert!(whole.is_empty(), "{whole:?}");
            let events = range_events(store, from, to);
            assert!(events.iter().any(|e| e.id == *start));
            assert_eq!(
                marks::marks(&events, Some(Cursor::Time(from)), Some(Cursor::Time(to))),
                whole
            );
            let without: Vec<RunEvent> = events.into_iter().filter(|e| e.id != *start).collect();
            let derived = marks::marks(&without, Some(Cursor::Time(from)), Some(Cursor::Time(to)));
            let labels: Vec<&str> = derived.iter().map(|m| m.label.as_str()).collect();
            assert_eq!(labels, ["dagq_version v1 → v2"]);
        });
        assert_eq!(small, large);
    }

    /// A mark written before the range by the DB's clock with `--at` in it
    /// (the CLI's clock ahead), a pruned stop whose heartbeat (the
    /// supervisor's clock) is in the range, written before it, and a mark
    /// whose `--at` is after the DB's now. Read by the effective time they
    /// are found; Spike 4135's read of a range that reaches now (the marks
    /// written from the range's start to now) misses the first two.
    #[test]
    fn vi_a_clock_ahead_of_the_db_is_read_by_the_effective_time() {
        let build = |units| {
            let mut log = Log::default();
            let from = log.settled(units, T0);
            log.mark("cli ahead", Some(from + 2 * MIN), from - MIN);
            log.queue(
                EventKind::SupervisorStopped,
                json!({"supervisor": "s", "outcome": "pruned", "dagq_version": "v1",
                       "last_heartbeat_at": (from + 3 * MIN) / 1000}),
                from - MIN / 2,
            );
            log.run(None, attributes("v1", 4), from + 5 * MIN);
            let now = from + 10 * MIN;
            log.mark("after the db's now", Some(now + 5 * MIN), now);
            // The CLI's now, which is the range's end, is ahead too.
            (log, from, now + 10 * MIN, ())
        };
        let (small, large) = reads_with(build, |store, from, to, ()| {
            let whole = whole_marks(store, from, to);
            let labels: Vec<&str> = whole.iter().map(|m| m.label.as_str()).collect();
            assert_eq!(
                labels,
                ["cli ahead", "supervisor stopped: v1", "after the db's now"]
            );
            assert_eq!(marks_in(store, from, to), whole);
            let written = store.written(&marks::RECORDED_KINDS, from, store.newest_ms(), |_| true);
            assert_eq!(written.len(), 1, "only the last is written from the start");
        });
        assert_eq!(small, large);
    }

    /// A mark of the range retracted later by id but by a clock turned back
    /// before the range: read by the mark it names it is found; a read of
    /// the retractions written from the range's start on misses it.
    #[test]
    fn vi_a_retraction_by_a_clock_turned_back_is_read_by_the_mark() {
        let build = |units| {
            let mut log = Log::default();
            let from = log.settled(units, T0);
            let mark = log.mark("retracted", None, from + 10 * MIN);
            log.retract(mark, from - 30 * MIN);
            (log, from, from + 60 * MIN, ())
        };
        let (small, large) = reads_with(build, |store, from, to, ()| {
            let whole = whole_marks(store, from, to);
            assert!(whole[0].retracted_by.is_some());
            assert_eq!(marks_in(store, from, to), whole);
            let written = store.written(&[EventKind::MarkRetracted], from, to, |_| true);
            assert!(written.is_empty());
        });
        assert_eq!(small, large);
    }

    /// An event cursor stands for the latest time at or before its id,
    /// which a clock turned back makes another event's. Walking the time
    /// index down to it reads every event later in time: few for a cursor
    /// at now, as many as the history after it for one in the past. The
    /// walk stopped after [`WALK`] entries finds that time for a cursor
    /// near now and for one with 30 units after it, and gives up with 300
    /// (the read falls back to every event); it reads at most [`WALK`].
    #[test]
    fn vi_an_event_cursor_reads_what_is_later_in_time() {
        // Near now the history before the cursor grows; in the past, the
        // history after it.
        let build = |past: bool| {
            move |units| {
                let mut log = Log::default();
                let base = log.settled(if past { 30 } else { units }, T0);
                log.mark("ahead", None, base + 60 * MIN);
                let cursor = log.mark("behind", None, base + 10 * MIN);
                if past {
                    log.settled(units, base + 120 * MIN);
                }
                (log, 0, 0, cursor)
            }
        };
        for past in [false, true] {
            let (small, large) = reads_with(build(past), |store, _, _, cursor| {
                let whole = marks::cursor_millis(Cursor::Event(*cursor), &store.events);
                assert_eq!(store.latest_time_through(*cursor), whole);
                let own = store.events[cursor.as_i64() as usize - 1]
                    .created_at
                    .clone();
                assert!(whole > crate::domain::stats::timestamp_millis(&own).unwrap());
            });
            if past {
                // 300 units against 30: a read in proportion to the history it grows
                // reads at least one more for each of the 270 units more, and one that
                // does not grow reads as many with both and fails (a ratio of 10 times
                // does not hold once a fixed part is added).
                assert!(large >= small + 270, "{small} {large}");
            } else {
                assert_eq!(small, large);
            }
            let (small, large) = reads_with(build(past), |store, _, _, cursor| {
                let whole = marks::cursor_millis(Cursor::Event(*cursor), &store.events);
                let later = store.events.len() - cursor.as_i64() as usize;
                match store.latest_time_within(*cursor, WALK) {
                    Some(found) => assert_eq!(found, whole),
                    None => assert!(past && later >= WALK, "gave up near now"),
                }
            });
            assert!(small <= WALK && large <= WALK, "{small} {large}");
            if !past {
                assert_eq!(small, large);
            }
        }
        // With 300 units after it the walk gives up.
        let (log, ..) = build(true)(300);
        let store = log.store();
        let cursor = store
            .events
            .iter()
            .find(|e| e.payload["label"] == "behind")
            .unwrap()
            .id;
        assert_eq!(store.latest_time_within(cursor, WALK), None);
    }

    /// A run written before a claim of the range but by a clock ahead past
    /// the range's end: its claim is the range claim's previous one by id.
    /// Sought by id, every event's marks come; derived from the claims of
    /// the rows that have a record at or before the end, they do not.
    #[test]
    fn vi_a_previous_claim_is_sought_not_taken_from_the_rows_read() {
        let build = |units| {
            let mut log = Log::default();
            let from = log.settled(units, T0);
            let to = from + 60 * MIN;
            log.run(None, attributes("v2", 4), to + 30 * MIN);
            log.run(None, attributes("v2", 4), from + 10 * MIN);
            log.settled(units, to + 120 * MIN);
            (log, from, to, ())
        };
        let (small, large) = reads_with(build, |store, from, to, ()| {
            let whole = whole_marks(store, from, to);
            assert!(whole.is_empty(), "the change is the earlier claim's");
            assert_eq!(marks_in(store, from, to), whole);
            // The rows not after the end in time, and a seek back from the
            // range's first claim alone.
            let kept: HashSet<&RunId> = store
                .events
                .iter()
                .filter(|e| created_ms(e) <= to)
                .filter_map(|e| e.run_id.as_ref())
                .collect();
            let claims: Vec<RunEvent> = store
                .events
                .iter()
                .filter(|e| e.run_id.as_ref().is_some_and(|run| kept.contains(run)))
                .cloned()
                .collect();
            let derived = marks::marks(&claims, Some(Cursor::Time(from)), Some(Cursor::Time(to)));
            assert_eq!(derived.len(), 1, "{derived:?}");
        });
        assert_eq!(small, large);
    }

    /// The runs of `(from, to]` as `stats` picks them: those whose
    /// `finished_event_id` is after `Cursor::Time(from).event_id` and at or
    /// before `Cursor::Time(to).event_id` of `events`.
    fn period_runs(events: &[RunEvent], from: i64, to: i64) -> Vec<RunId> {
        let since = Cursor::Time(from).event_id(events);
        let until = Cursor::Time(to).event_id(events);
        stats::run_stats(events)
            .into_iter()
            .filter(|run| {
                run.finished_event_id
                    .is_some_and(|finish| finish > since && finish <= until)
            })
            .map(|run| run.run_id)
            .collect()
    }

    /// The events of the rows not left out above `to`: the queue's, and
    /// each run's whole when `keep` keeps that run's events.
    fn rows_kept(events: &[RunEvent], keep: impl Fn(&[&RunEvent]) -> bool) -> Vec<RunEvent> {
        let mut runs: HashMap<&RunId, Vec<&RunEvent>> = HashMap::new();
        for event in events {
            if let Some(run) = &event.run_id {
                runs.entry(run).or_default().push(event);
            }
        }
        let kept: HashSet<&RunId> = runs
            .iter()
            .filter(|(_, records)| keep(records))
            .map(|(run, _)| *run)
            .collect();
        events
            .iter()
            .filter(|e| e.run_id.as_ref().is_none_or(|run| kept.contains(run)))
            .cloned()
            .collect()
    }

    /// The entries the walk for `Cursor::Time(E).event_id` stops after.
    const WALK: usize = 100;

    /// The events read for a period ending at `to`: the rows whose first id
    /// is after `Cursor::Time(to).event_id` and whose records are all after
    /// `to` are left out when the walk of [`WALK`] entries finds that id,
    /// and none is left out when it does not.
    fn read_up_to(store: &Store, to: i64) -> Vec<RunEvent> {
        match store.last_id_within(to, WALK) {
            Some(through) => rows_kept(&store.events, |records| {
                records
                    .iter()
                    .any(|e| created_ms(e) <= to || e.id <= through)
            }),
            None => store.events.clone(),
        }
    }

    /// The review's example: a claim at 30 and its landing at 31, then a
    /// queue event at 20 (the clock turned back), with `(15, 25]`.
    /// `Cursor::Time(25).event_id` is the queue event's id, after the
    /// landing's, so every event's period holds the run though its records
    /// are all after 25. Leaving the rows after the end out by time alone
    /// drops it; leaving out only the rows whose first id is after that id
    /// keeps it. The walk for that id stops after [`WALK`] entries: near now
    /// it finds it, in the past with more written after the range it gives
    /// up and no row is left out; either way the answer is every event's
    /// and the walk reads at most [`WALK`].
    #[test]
    fn vi_a_row_after_the_end_in_time_is_in_the_period_by_id() {
        let build = |past: bool| {
            move |units| {
                let mut log = Log::default();
                let base = log.settled(if past { 30 } else { units }, T0);
                let (from, to) = (base + 15 * MIN, base + 25 * MIN);
                log.run(None, attributes("v1", 4), base + 30 * MIN);
                log.mark("written back", None, base + 20 * MIN);
                if past {
                    log.settled(units, base + 60 * MIN);
                }
                (log, from, to, ())
            }
        };
        for past in [false, true] {
            let (small, large) = reads_with(build(past), |store, from, to, ()| {
                let whole = period_runs(&store.events, from, to);
                assert_eq!(whole.len(), 1, "the run after 25 in time");
                let by_time = rows_kept(&store.events, |records| {
                    records.iter().any(|e| created_ms(e) <= to)
                });
                assert!(period_runs(&by_time, from, to).is_empty());
                assert_eq!(period_runs(&read_up_to(store, to), from, to), whole);
            });
            assert!(small <= WALK && large <= WALK, "{small} {large}");
            if !past {
                assert_eq!(small, large);
            }
        }
    }

    /// A run landed at 20, then the settled history written from 30 on,
    /// then the clock turned back to 25 before the read, which asks for
    /// `(15, 25]` up to now: walking the ids down to the first event at or
    /// before 25 goes back through the whole settled history. Stopped after
    /// [`WALK`] entries, the walk finds it with 30 units and gives up with
    /// 300, where no row is left out (every row is read, as today). The
    /// answer is every event's either way. A DB's clock ahead of the
    /// reader's writes the same history.
    #[test]
    fn vi_a_clock_turned_back_before_the_read_stops_the_walk_and_reads_every_row() {
        let build = |units| {
            let mut log = Log::default();
            log.run(None, attributes("v1", 4), T0 + 16 * MIN);
            log.settled(units, T0 + 30 * MIN);
            (log, T0 + 15 * MIN, T0 + 25 * MIN, ())
        };
        let (small, large) = reads_with(build, |store, from, to, ()| {
            let whole = period_runs(&store.events, from, to);
            assert_eq!(whole.len(), 1);
            assert_eq!(period_runs(&read_up_to(store, to), from, to), whole);
            let gave_up = store.last_id_within(to, WALK).is_none();
            assert_eq!(gave_up, store.events.len() > WALK);
        });
        assert!(small <= 2 * WALK && large <= 2 * WALK, "{small} {large}");
        // The walk without a stop reads the whole settled history.
        let (small, large) = reads_with(build, |store, _, to, ()| {
            store.last_id_through(to);
        });
        // 300 units against 30: a read in proportion to the history it grows
        // reads at least one more for each of the 270 units more, and one that
        // does not grow reads as many with both and fails (a ratio of 10 times
        // does not hold once a fixed part is added).
        assert!(large >= small + 270, "{small} {large}");
    }

    // (vii) -------------------------------------------------------------

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

    fn whole_scoring(store: &Store, from: i64, to: i64) -> Scoring {
        let marks = marks::marks(&store.events, None, None);
        scoring_in(score::score(&store.events, &marks), from, to)
    }

    fn check_forecast(store: &Store, from: i64, to: i64) {
        let whole = whole_scoring(store, from, to);
        let targets: Vec<&str> = whole.samples.iter().map(|s| s.target.as_str()).collect();
        assert_eq!(
            targets,
            ["task", "goal", "task"],
            "task 900 after reopening (by id and by time), goal 50; not task 902"
        );
        assert_eq!(whole.excluded.len(), 1, "task 903 canceled");
        assert!(whole.samples.iter().all(|sample| sample.marks > 0));
        assert_eq!(forecast_in(store, from, to), whole);
    }

    /// The settled history before the targets' snapshots is not read.
    #[test]
    fn vii_the_history_before_the_snapshots_is_not_read() {
        let (small, large) = reads_with(
            |units| forecast_history(units, 30),
            |s, f, t, ()| {
                check_forecast(s, f, t);
            },
        );
        assert_eq!(small, large);
    }

    /// The history between the snapshots and the range is read: the
    /// snapshots of it, its claims, and the marks after the snapshots.
    #[test]
    fn vii_the_history_between_the_snapshots_and_the_range_is_read() {
        let (small, large) = reads_with(
            |units| forecast_history(30, units),
            |s, f, t, ()| {
                check_forecast(s, f, t);
            },
        );
        // 300 units against 30: a read in proportion to the history it grows
        // reads at least one more for each of the 270 units more, and one that
        // does not grow reads as many with both and fails (a ratio of 10 times
        // does not hold once a fixed part is added).
        assert!(large >= small + 270, "{small} {large}");
    }

    /// The marks after the range count on a sample by their ids: marks
    /// recorded after the range grow the read.
    #[test]
    fn vii_the_marks_written_after_the_range_are_read() {
        let build = |units| {
            let (mut log, from, to, ()) = forecast_history(30, 30);
            for n in 0..units {
                let mark = log.mark("later", None, to + (30 + n) * MIN);
                log.retract(mark, to + (30 + n) * MIN + 1);
            }
            (log, from, to, ())
        };
        let (small, large) = reads_with(build, |s, f, t, ()| check_forecast(s, f, t));
        // 300 units against 30: a read in proportion to the history it grows
        // reads at least one more for each of the 270 units more, and one that
        // does not grow reads as many with both and fails (a ratio of 10 times
        // does not hold once a fixed part is added).
        assert!(large >= small + 270, "{small} {large}");
    }

    /// A range over the settled runs just before the finishes, with
    /// snapshots and marks in it and no finish: nothing is read past the
    /// finishes.
    #[test]
    fn vii_a_snapshot_whose_targets_do_not_finish_in_the_range_is_not_read() {
        let build = |units| {
            let (log, from, _, ()) = forecast_history(units, units);
            (log, from - 200 * MIN, from, ())
        };
        let (small, large) = reads_with(build, |store, from, to, ()| {
            let whole = whole_scoring(store, from, to);
            assert!(whole.samples.is_empty() && whole.excluded.is_empty());
            assert_eq!(forecast_in(store, from, to), whole);
        });
        assert_eq!(small, large);
    }

    // (viii) ------------------------------------------------------------

    /// The finish times of every event's runs (`Context::finishes`: the
    /// time of each run's `finished_event_id`) in `(from, to]`.
    fn whole_finishes(
        store: &Store,
        from: i64,
        to: i64,
        counts: impl Fn(Option<TaskId>) -> bool,
    ) -> Vec<i64> {
        let times: HashMap<EventId, i64> =
            store.events.iter().map(|e| (e.id, created_ms(e))).collect();
        let mut finishes: Vec<i64> = stats::run_stats(&store.events)
            .iter()
            .filter(|run| counts(Some(run.task_id)))
            .filter_map(|run| times.get(&run.finished_event_id?).copied())
            .filter(|&ms| ms > from && ms <= to)
            .collect();
        finishes.sort_unstable();
        finishes
    }

    /// Two marks with `gap` finishes between them, among them a run that
    /// failed a verification (its first end) before the range and landed
    /// in it, and one whose first end was written by a clock turned back
    /// before the range, then landed in it; runs of goal 7's task and of
    /// others. Returns the marks' range.
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

    /// The finishes between the marks of every event, of all runs and of
    /// goal 7's (whose map names task 70 alone), and the groups they cut
    /// with `min_samples` − 1 and `min_samples` of them.
    #[test]
    fn viii_the_finishes_are_the_runs_first_ends_read_by_time() {
        let goals: HashMap<TaskId, GoalId> = [(TaskId::new(70), GoalId::new(7))].into();
        let in_goal = |task: Option<TaskId>| task.and_then(|t| goals.get(&t)).is_some();
        for gap in [MIN_SAMPLES - 1, MIN_SAMPLES] {
            let (small, large) = reads_with(
                |units| finish_history(units, gap),
                |store, from, to, ()| {
                    let whole = whole_finishes(store, from, to, |_| true);
                    // The resumed run and the one turned back ended
                    // before the range.
                    assert_eq!(whole.len(), gap);
                    assert_eq!(finishes_in(store, from, to, |_| true), whole);
                    let of_goal = whole_finishes(store, from, to, in_goal);
                    assert_eq!(of_goal.len(), gap.div_ceil(2));
                    assert_eq!(finishes_in(store, from, to, in_goal), of_goal);
                    // The groups of every mark, cut by every finish read
                    // so (on a store of its own: a read of every run).
                    let all = marks::marks(&store.events, None, None);
                    let cut = |finishes: &[i64]| {
                        super::super::super::overlapping_groups(&all, finishes, MIN_SAMPLES)
                            .into_iter()
                            .filter(|g| g.iter().any(|m| mark_ms(m) > from && mark_ms(m) <= to))
                            .map(|g| g.len())
                            .collect::<Vec<usize>>()
                    };
                    let every = Store::new(store.events.clone());
                    let read = finishes_in(&every, i64::MIN, i64::MAX, |_| true);
                    let expected = cut(&whole_finishes(store, i64::MIN, i64::MAX, |_| true));
                    assert_eq!(cut(&read), expected);
                    let sizes: &[usize] = if gap < MIN_SAMPLES { &[2] } else { &[1, 1] };
                    assert_eq!(expected, sizes, "gap {gap}");
                },
            );
            assert_eq!(small, large, "gap {gap}");
        }
    }
}
