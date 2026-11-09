//! The read contract of a range of the change marks, tried against the
//! read of every event (docs/plans/ledger-window-read-contract-spike.md).
//! Not wired: [`marks::marks`], [`super::routine`] and
//! [`super::overlapping_groups`] are given the few events [`Reads`] picks
//! for a range instead of every one, and each test compares the answer with
//! the answer of every event and the events read with a small and a large
//! settled history outside the range.
//!
//! [`Reads`] stands for a store's indexed reads: each returns only what its
//! index finds and counts it. The reads `existing_*` are those today's
//! indexes serve (`events_by_kind(kind, id)` with a range of ids for a
//! range of times); the others need an index today's migrations lack, named
//! on each.

mod preconditions;

use std::cell::Cell;
use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::domain::marks::{self, Mark};
use crate::domain::stats::{Cursor, timestamp_millis};
use crate::domain::{EventId, EventKind, RunEvent, RunId, TaskId};

/// The attributes a derived mark is read off and the key a start announces
/// it by (a copy of `marks`' private `DERIVED`).
const DERIVED: [(&str, Option<&str>); 5] = [
    ("dagq_version", Some("dagq_version")),
    ("claude_version", None),
    ("codex_version", None),
    ("parallel", Some("parallel")),
    ("toolchain", None),
];

/// A copy of `marks`' private `claim_value`.
fn claim_value(payload: &Value, attribute: &str) -> Option<String> {
    let value = |key: &str| match payload.get(key)? {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    };
    if attribute == "toolchain" {
        return Some(format!(
            "{} {}",
            value("rustc_release")?,
            value("rustc_host")?
        ));
    }
    value(attribute)
}

fn is_queue_event(event: &RunEvent) -> bool {
    event.task_id.is_none() && event.goal_id.is_none() && event.run_id.is_none()
}

fn created_ms(event: &RunEvent) -> i64 {
    timestamp_millis(&event.created_at).unwrap_or(i64::MIN)
}

/// When a recorded mark took effect, as [`marks::marks`] reads it: the
/// expression an index of the effective time would be on.
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
            .and_then(|secs| secs.checked_mul(1000))
            .and_then(|ms| timestamp_millis(&marks::utc_text(ms))),
        _ => None,
    }
    .unwrap_or_else(|| created_ms(event))
}

/// An in-memory store of the events (ascending id, times ascending with the
/// ids) and the runs' finish times, whose reads count what they return.
struct Reads {
    events: Vec<RunEvent>,
    finishes: Vec<i64>,
    read: Cell<usize>,
}

impl Reads {
    fn new(events: Vec<RunEvent>, finishes: Vec<i64>) -> Self {
        let mut finishes = finishes;
        finishes.sort_unstable();
        Self {
            events,
            finishes,
            read: Cell::new(0),
        }
    }

    fn take(&self, picked: Vec<RunEvent>) -> Vec<RunEvent> {
        self.read.set(self.read.get() + picked.len());
        picked
    }

    fn pick(&self, keep: impl Fn(&RunEvent) -> bool) -> Vec<RunEvent> {
        self.take(self.events.iter().filter(|e| keep(e)).cloned().collect())
    }

    /// The first id after `ms` (a seek, not counted: the id range of a time
    /// range, as the ledger's rows give it by `inputs_from`/`inputs_through`).
    fn id_after(&self, ms: i64) -> EventId {
        self.events
            .iter()
            .find(|event| created_ms(event) > ms)
            .map_or(EventId::new(i64::MAX), |event| event.id)
    }

    /// The first time of the history: of an event, a finish, or the time a
    /// recorded mark took effect, which may precede every event (`--at`
    /// has no lower bound). One seek on the effective time's index, not
    /// counted.
    fn earliest_ms(&self) -> i64 {
        let event = self.events.first().map_or(i64::MAX, created_ms);
        let effective = self
            .events
            .iter()
            .filter(|e| is_queue_event(e) && e.kind.is_in(&marks::RECORDED_KINDS))
            .map(effective_ms)
            .min()
            .unwrap_or(i64::MAX);
        event
            .min(effective)
            .min(self.finishes.first().copied().unwrap_or(i64::MAX))
    }

    /// The last time of the history, of an event or a finish.
    fn newest_ms(&self) -> i64 {
        let event = self.events.last().map_or(i64::MIN, created_ms);
        event.max(self.finishes.last().copied().unwrap_or(i64::MIN))
    }

    /// Today's index: the recorded marks written in `(from, to]`
    /// (`events_by_kind(kind, id)` over the ids of the times).
    fn existing_recorded_written(&self, from: i64, to: i64) -> Vec<RunEvent> {
        self.pick(|e| {
            is_queue_event(e)
                && e.kind.is_in(&marks::RECORDED_KINDS)
                && created_ms(e) > from
                && created_ms(e) <= to
        })
    }

    /// Needs an index on the effective time (`run_events(kind, <the
    /// effective time>)`, partial to the recorded marks): the recorded
    /// marks that took effect in `(from, to]`, whenever written.
    fn recorded_effective(&self, from: i64, to: i64) -> Vec<RunEvent> {
        self.pick(|e| {
            is_queue_event(e)
                && e.kind.is_in(&marks::RECORDED_KINDS)
                && effective_ms(e) > from
                && effective_ms(e) <= to
        })
    }

    /// Today's index: every retraction written after `mark`
    /// (`events_by_kind('mark_retracted', id > mark)`).
    fn existing_retractions_after(&self, mark: EventId) -> Vec<RunEvent> {
        self.pick(|e| e.kind == EventKind::MarkRetracted && is_queue_event(e) && e.id > mark)
    }

    /// Needs `run_events(kind, json_extract(payload, '$.mark'))` (as
    /// `events_by_opened_event`): the retractions naming `mark`.
    fn retractions_of(&self, mark: EventId) -> Vec<RunEvent> {
        self.pick(|e| {
            e.kind == EventKind::MarkRetracted
                && is_queue_event(e)
                && e.payload.get("mark").and_then(Value::as_i64) == Some(mark.as_i64())
        })
    }

    /// The claims written in `(from, to]`: with the run rows of the range.
    fn claims_written(&self, from: i64, to: i64) -> Vec<RunEvent> {
        self.pick(|e| {
            e.kind == EventKind::RunClaimed
                && e.run_id.is_some()
                && created_ms(e) > from
                && created_ms(e) <= to
        })
    }

    /// Today's index: the claims before `before` walked back from it
    /// (`events_by_kind('run_claimed', id < before) ORDER BY id DESC`)
    /// until one records `attribute`, counting every claim walked.
    fn existing_last_claim_with(&self, attribute: &str, before: EventId) -> Option<RunEvent> {
        let mut walked = Vec::new();
        let mut found = None;
        for event in self
            .events
            .iter()
            .rev()
            .filter(|e| e.id < before && e.kind == EventKind::RunClaimed && e.run_id.is_some())
        {
            walked.push(event.clone());
            if claim_value(&event.payload, attribute).is_some() {
                found = Some(event.clone());
                break;
            }
        }
        self.take(walked);
        found
    }

    /// Needs a partial index per attribute (`run_events(kind, id) WHERE
    /// json_extract(payload, '$.<attribute>') IS NOT NULL`, both `rustc_*`
    /// for the toolchain): the last claim before `before` that records it.
    fn last_claim_with(&self, attribute: &str, before: EventId) -> Option<RunEvent> {
        let found = self
            .events
            .iter()
            .rev()
            .find(|e| {
                e.id < before
                    && e.kind == EventKind::RunClaimed
                    && e.run_id.is_some()
                    && claim_value(&e.payload, attribute).is_some()
            })
            .cloned();
        self.take(found.iter().cloned().collect());
        found
    }

    /// Needs `run_events(kind, json_extract(payload, '$.<key>'), id)` for
    /// the starts: one start between `after` and `before` (ids) that
    /// announces `value`.
    fn start_announcing(
        &self,
        key: &str,
        value: &str,
        after: EventId,
        before: EventId,
    ) -> Option<RunEvent> {
        let found = self
            .events
            .iter()
            .find(|e| {
                e.kind == EventKind::SupervisorStarted
                    && is_queue_event(e)
                    && e.id > after
                    && e.id < before
                    && claim_value(&e.payload, key).as_deref() == Some(value)
            })
            .cloned();
        self.take(found.iter().cloned().collect());
        found
    }

    /// Today's index: one seek back (`events_by_kind('supervisor_started',
    /// id < before) ORDER BY id DESC LIMIT 1`).
    fn last_start_before(&self, before: EventId) -> Option<RunEvent> {
        let found = self
            .events
            .iter()
            .rev()
            .find(|e| e.id < before && e.kind == EventKind::SupervisorStarted && is_queue_event(e))
            .cloned();
        self.take(found.iter().cloned().collect());
        found
    }

    /// Needs an index of the runs by their finish (`ORDER BY` the finish
    /// `DESC LIMIT count`): the `count`-th finish at or before `at`, reading
    /// at most `count`; `None` when fewer finished.
    fn finish_back(&self, at: i64, count: usize) -> Option<i64> {
        let picked: Vec<i64> = self
            .finishes
            .iter()
            .rev()
            .copied()
            .filter(|&t| t <= at)
            .take(count)
            .collect();
        self.read.set(self.read.get() + picked.len());
        (picked.len() == count).then(|| picked[count - 1])
    }

    /// The `count`-th finish after `at`, reading at most `count`.
    fn finish_forward(&self, at: i64, count: usize) -> Option<i64> {
        let picked: Vec<i64> = self
            .finishes
            .iter()
            .copied()
            .filter(|&t| t > at)
            .take(count)
            .collect();
        self.read.set(self.read.get() + picked.len());
        (picked.len() == count).then(|| picked[count - 1])
    }

    /// The finishes in `(from, to]`, counted as read.
    fn finishes_in(&self, from: i64, to: i64) -> Vec<i64> {
        let picked: Vec<i64> = self
            .finishes
            .iter()
            .copied()
            .filter(|&t| t > from && t <= to)
            .collect();
        self.read.set(self.read.get() + picked.len());
        picked
    }
}

fn dedup(events: impl IntoIterator<Item = RunEvent>) -> Vec<RunEvent> {
    let by_id: BTreeMap<EventId, RunEvent> = events.into_iter().map(|e| (e.id, e)).collect();
    by_id.into_values().collect()
}

/// The events of the contract for the marks of `(from, to]`: the recorded
/// marks that took effect in it (i), the retractions of those (ii), the
/// claims written in it, each attribute's last claim before it and a start
/// between that one and the first claim after it that announces the value
/// (iii), and the last start before it (iv).
fn range_events(reads: &Reads, from: i64, to: i64) -> Vec<RunEvent> {
    let recorded = reads.recorded_effective(from, to);
    let retractions: Vec<RunEvent> = recorded
        .iter()
        .filter(|mark| mark.kind.is_in(&marks::RETRACTABLE_KINDS))
        .flat_map(|mark| reads.retractions_of(mark.id))
        .collect();
    let claims = reads.claims_written(from, to);
    let first = reads.id_after(from);
    let mut earlier = Vec::new();
    for (attribute, start_key) in DERIVED {
        let Some(previous) = reads.last_claim_with(attribute, first) else {
            continue;
        };
        let mut last = (claim_value(&previous.payload, attribute), previous.id);
        if let Some(key) = start_key {
            for claim in &claims {
                let Some(value) = claim_value(&claim.payload, attribute) else {
                    continue;
                };
                if last.0.as_ref() != Some(&value) {
                    earlier.extend(reads.start_announcing(key, &value, last.1, claim.id));
                }
                last = (Some(value), claim.id);
            }
        }
        earlier.push(previous);
    }
    earlier.extend(reads.last_start_before(first));
    dedup(
        recorded
            .into_iter()
            .chain(retractions)
            .chain(claims)
            .chain(earlier),
    )
}

/// The marks that took effect in `(from, to]` by the contract.
fn marks_in(reads: &Reads, from: i64, to: i64) -> Vec<Mark> {
    marks::marks(
        &range_events(reads, from, to),
        Some(Cursor::Time(from)),
        Some(Cursor::Time(to)),
    )
}

fn mark_ms(mark: &Mark) -> i64 {
    timestamp_millis(&mark.at).unwrap_or(i64::MIN)
}

/// The splitting marks of `(from, to]` with whether each is routine: the
/// last start before the range is read with it (iv) and classified, then
/// left out.
fn classified_in(reads: &Reads, from: i64, to: i64) -> Vec<(Mark, bool)> {
    let events = range_events(reads, from, to);
    let all = marks::marks(&events, None, Some(Cursor::Time(to)));
    let splitting = super::splitting(&all);
    let routine = super::routine(&splitting);
    splitting
        .into_iter()
        .zip(routine)
        .filter(|(mark, _)| mark_ms(mark) > from)
        .map(|(mark, routine)| (mark.clone(), routine))
        .collect()
}

/// [`super::overlapping_groups`]' loop on marks already classified.
fn groups_of(classified: &[(Mark, bool)], finishes: &[i64], min_samples: usize) -> Vec<Vec<Mark>> {
    let mut groups: Vec<Vec<Mark>> = Vec::new();
    for (mark, _) in classified.iter().filter(|(_, routine)| !routine) {
        let at = mark_ms(mark);
        match groups.last_mut() {
            Some(group)
                if {
                    let previous = mark_ms(group.last().expect("groups are not empty"));
                    finishes.partition_point(|&t| t <= at)
                        - finishes.partition_point(|&t| t <= previous)
                        < min_samples
                } =>
            {
                group.push(mark.clone());
            }
            _ => groups.push(vec![mark.clone()]),
        }
    }
    groups
}

/// Every group of overlapping marks with a mark in `(from, to]`, whole (v).
/// A mark `p` before the first mark `f` of the first group of the range
/// joins it when fewer than `min_samples` runs finished in `(p, f]`, so only
/// a mark at or after the `min_samples`-th finish back from `f` can: the
/// slice widens back to that finish (to the start of the history, the
/// earliest effective time, when fewer finished), and again from the new
/// first mark while the group grows. Forward the same from the last mark,
/// to before the `min_samples`-th finish after it (to now when fewer). The
/// finishes are read by their count; what the slices hold besides the
/// marks (the claims a derived mark is read off, the routine marks) is
/// read by time.
fn groups_touching(reads: &Reads, from: i64, to: i64, min_samples: usize) -> Vec<Vec<Mark>> {
    let (earliest, newest) = (reads.earliest_ms(), reads.newest_ms());
    let (mut lo, mut hi) = (from, to);
    let mut classified = classified_in(reads, lo, hi);
    let mut finishes = reads.finishes_in(lo, hi);
    loop {
        let groups = groups_of(&classified, &finishes, min_samples);
        let touching: Vec<&Vec<Mark>> = groups
            .iter()
            .filter(|group| {
                group
                    .iter()
                    .any(|mark| mark_ms(mark) > from && mark_ms(mark) <= to)
            })
            .collect();
        let (Some(first), Some(last)) = (touching.first(), touching.last()) else {
            return Vec::new();
        };
        let first_ms = mark_ms(&first[0]);
        let last_ms = mark_ms(&last[last.len() - 1]);
        // `(back, lo]` and `(hi, forward]` are still to read.
        let back = reads
            .finish_back(first_ms, min_samples)
            .map_or(earliest - 1, |finish| finish - 1);
        let forward = reads
            .finish_forward(last_ms, min_samples)
            .map_or(newest, |finish| finish - 1);
        let mut widened = false;
        if back < lo {
            let mut earlier = classified_in(reads, back, lo);
            earlier.append(&mut classified);
            classified = earlier;
            let mut earlier = reads.finishes_in(back, lo);
            earlier.append(&mut finishes);
            finishes = earlier;
            lo = back;
            widened = true;
        }
        if forward > hi {
            classified.extend(classified_in(reads, hi, forward));
            finishes.extend(reads.finishes_in(hi, forward));
            hi = forward;
            widened = true;
        }
        if !widened {
            return touching.into_iter().cloned().collect();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;
    /// 2026-10-01T00:00:00Z.
    const T0: i64 = 1_790_812_800_000;
    const MIN_SAMPLES: usize = 5;

    /// A history built in time order.
    #[derive(Default)]
    struct History {
        events: Vec<RunEvent>,
        finishes: Vec<i64>,
        next_run: i64,
    }

    impl History {
        fn push(&mut self, run: Option<(TaskId, RunId)>, kind: EventKind, payload: Value, at: i64) {
            let id = EventId::new(self.events.len() as i64 + 1);
            self.events.push(RunEvent {
                id,
                task_id: run.as_ref().map(|r| r.0),
                goal_id: None,
                run_id: run.map(|r| r.1),
                kind: kind.as_str().into(),
                payload,
                created_at: marks::utc_text(at),
                actor: None,
            });
        }

        fn queue(&mut self, kind: EventKind, payload: Value, at: i64) -> EventId {
            self.push(None, kind, payload, at);
            self.events.last().unwrap().id
        }

        fn claim(&mut self, payload: Value, at: i64) {
            self.next_run += 1;
            let n = self.next_run;
            let run = (
                TaskId::new(n),
                RunId::new(format!("00000000-0000-4000-8000-{n:012}")).unwrap(),
            );
            self.push(Some(run), EventKind::RunClaimed, payload, at);
        }

        /// A run claimed at `at` and finished a minute later.
        fn run(&mut self, payload: Value, at: i64) {
            self.claim(payload, at);
            self.finishes.push(at + MIN);
        }

        fn start(&mut self, version: &str, parallel: i64, at: i64) {
            self.queue(
                EventKind::SupervisorStarted,
                json!({"dagq_version": version, "parallel": parallel, "handoff": false}),
                at,
            );
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

        /// `units` settled runs from `at`, ten minutes apart, each claim
        /// recording no Codex, every fifth after a routine start, and every
        /// tenth with a mark retracted at once: the history outside a range
        /// the tests grow. Returns the time after it.
        fn settled(&mut self, units: i64, at: i64) -> i64 {
            for n in 0..units {
                let t = at + n * 10 * MIN;
                if n % 5 == 0 {
                    self.start("v1", 4, t);
                }
                if n % 10 == 0 {
                    let mark = self.mark("settled", None, t + 1);
                    self.retract(mark, t + 2);
                }
                self.run(attributes("v1", 4, None), t + 3);
            }
            at + units * 10 * MIN
        }

        fn reads(&self) -> Reads {
            Reads::new(self.events.clone(), self.finishes.clone())
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

    /// The marks of `(from, to]` of every event.
    fn whole_marks(reads: &Reads, from: i64, to: i64) -> Vec<Mark> {
        marks::marks(
            &reads.events,
            Some(Cursor::Time(from)),
            Some(Cursor::Time(to)),
        )
    }

    fn whole_classified(reads: &Reads, from: i64, to: i64) -> Vec<(Mark, bool)> {
        let all = marks::marks(&reads.events, None, None);
        super::super::classified(&all)
            .into_iter()
            .filter(|(m, _)| mark_ms(m) > from && mark_ms(m) <= to)
            .map(|(m, r)| (m.clone(), r))
            .collect()
    }

    fn whole_groups(reads: &Reads, from: i64, to: i64) -> Vec<Vec<Mark>> {
        let all = marks::marks(&reads.events, None, None);
        super::super::overlapping_groups(&all, &reads.finishes, MIN_SAMPLES)
            .into_iter()
            .filter(|g| g.iter().any(|m| mark_ms(m) > from && mark_ms(m) <= to))
            .collect()
    }

    /// Runs `case` on the history `build` makes with `small` and with
    /// `large` settled units outside the range and returns the events read
    /// of each.
    fn reads_with(
        build: impl Fn(i64) -> (History, i64, i64),
        case: impl Fn(&Reads, i64, i64),
    ) -> (usize, usize) {
        let mut counts = [0, 0];
        for (slot, units) in [(0, 30), (1, 3000)] {
            let (history, from, to) = build(units);
            let reads = history.reads();
            reads.read.set(0);
            case(&reads, from, to);
            counts[slot] = reads.read.get();
        }
        (counts[0], counts[1])
    }

    /// The range is three hours after `settled` units; `after` more units
    /// follow it (a range in the past).
    fn around(
        units: i64,
        inside: impl FnOnce(&mut History, i64),
        after: bool,
    ) -> (History, i64, i64) {
        let mut history = History::default();
        history.run(attributes("v0", 4, Some("0.45.0")), T0);
        history.start("v1", 4, T0 + MIN);
        let from = history.settled(units, T0 + 2 * MIN);
        let to = from + 180 * MIN;
        inside(&mut history, from);
        if after {
            history.settled(units, to + MIN);
        }
        (history, from, to)
    }

    // (i) ---------------------------------------------------------------

    /// A mark of the range written after it, backdated far into the range,
    /// and a pruned stop whose heartbeat is in the range, recorded after;
    /// a mark written in the range but backdated far before it is not of it.
    #[test]
    fn i_a_backdated_mark_is_read_by_its_effective_time_whatever_the_history() {
        let build = |units| {
            let (mut history, from, to) = around(
                units,
                |h, from| {
                    h.run(attributes("v1", 4, None), from + 10 * MIN);
                    h.mark(
                        "in range, backdated out",
                        Some(T0 + 5 * MIN),
                        from + 20 * MIN,
                    );
                },
                true,
            );
            let last = history.events.last().map(created_ms).unwrap();
            history.mark(
                "backdated into the range",
                Some(from + 30 * MIN),
                last + MIN,
            );
            history.queue(
                EventKind::SupervisorStopped,
                json!({"supervisor": "s", "outcome": "pruned", "dagq_version": "v1",
                       "last_heartbeat_at": (from + 40 * MIN) / 1000}),
                last + 2 * MIN,
            );
            (history, from, to)
        };
        let (small, large) = reads_with(build, |reads, from, to| {
            let whole = whole_marks(reads, from, to);
            assert_eq!(marks_in(reads, from, to), whole);
            let labels: Vec<&str> = whole.iter().map(|m| m.label.as_str()).collect();
            assert!(labels.contains(&"backdated into the range"), "{labels:?}");
            assert!(labels.contains(&"supervisor stopped: v1"), "{labels:?}");
            assert!(!labels.contains(&"in range, backdated out"), "{labels:?}");
        });
        assert_eq!(small, large);
    }

    /// Today's index reads the marks by when they were written: within the
    /// range it misses the backdated ones, and from the range to now it
    /// reads what was written after the range.
    #[test]
    fn i_the_written_time_misses_a_backdated_mark_or_reads_what_follows() {
        let build = |units| {
            let (mut history, from, to) = around(units, |_, _| {}, true);
            let last = history.events.last().map(created_ms).unwrap();
            history.mark(
                "backdated into the range",
                Some(from + 30 * MIN),
                last + MIN,
            );
            (history, from, to)
        };
        let (small, large) = reads_with(build, |reads, from, to| {
            let written = reads.existing_recorded_written(from, to);
            assert!(
                written
                    .iter()
                    .all(|e| e.payload["label"] != "backdated into the range")
            );
            assert_eq!(whole_marks(reads, from, to).len(), 1);
            let to_now = reads.existing_recorded_written(from, reads.newest_ms());
            assert!(
                to_now
                    .iter()
                    .any(|e| e.payload["label"] == "backdated into the range")
            );
        });
        assert!(large > small * 10, "{small} {large}");
    }

    // (ii) --------------------------------------------------------------

    /// A mark of the range retracted after the range, with other marks
    /// retracted after it too: the retraction is read by the mark it names.
    #[test]
    fn ii_a_later_retraction_is_read_by_the_mark_it_names() {
        let build = |units| {
            let mut mark = EventId::new(0);
            let (mut history, from, to) = around(
                units,
                |h, from| {
                    mark = h.mark("retracted later", None, from + 10 * MIN);
                    h.mark("kept", None, from + 20 * MIN);
                },
                true,
            );
            let last = history.events.last().map(created_ms).unwrap();
            history.retract(mark, last + MIN);
            (history, from, to, mark)
        };
        let (small, large) = reads_with(
            |units| {
                let (h, f, t, _) = build(units);
                (h, f, t)
            },
            |reads, from, to| {
                let whole = whole_marks(reads, from, to);
                assert_eq!(marks_in(reads, from, to), whole);
                assert!(
                    whole
                        .iter()
                        .any(|m| m.label == "retracted later" && m.retracted_by.is_some())
                );
                assert!(
                    whole
                        .iter()
                        .any(|m| m.label == "kept" && m.retracted_by.is_none())
                );
            },
        );
        assert_eq!(small, large);
        // Today's index reads every retraction written after the mark.
        let (small, large) = reads_with(
            |units| {
                let (h, f, t, _) = build(units);
                (h, f, t)
            },
            |reads, from, _| {
                let mark = reads
                    .events
                    .iter()
                    .find(|e| e.payload["label"] == "retracted later" && created_ms(e) > from)
                    .map(|e| e.id)
                    .unwrap();
                assert!(
                    reads
                        .existing_retractions_after(mark)
                        .iter()
                        .any(|e| e.payload["mark"] == mark.as_i64())
                );
            },
        );
        assert!(large > small * 10, "{small} {large}");
    }

    // (iii) -------------------------------------------------------------

    /// Codex's version last recorded at the start of the history, then a
    /// long run of claims without it, then a new version in the range; a
    /// new build announced by a start just before the range.
    #[test]
    fn iii_each_attributes_last_value_is_read_however_old() {
        let build = |units| {
            let mut history = History::default();
            history.run(attributes("v0", 4, Some("0.45.0")), T0);
            history.start("v1", 4, T0 + MIN);
            let from = history.settled(units, T0 + 2 * MIN);
            // A new build announced by its start before the range, and a
            // later start of another build: the announcing start is not
            // the last start before the range.
            history.start("v2", 4, from - MIN);
            history.start("v9", 4, from - MIN / 2);
            let to = from + 180 * MIN;
            history.run(attributes("v2", 4, Some("0.46.0")), from + 10 * MIN);
            history.run(attributes("v2", 4, None), from + 20 * MIN);
            history.run(attributes("v3", 4, Some("0.46.0")), from + 30 * MIN);
            (history, from, to)
        };
        let (small, large) = reads_with(build, |reads, from, to| {
            let whole = whole_marks(reads, from, to);
            assert_eq!(marks_in(reads, from, to), whole);
            let kinds: Vec<&str> = whole.iter().map(|m| m.label.as_str()).collect();
            assert_eq!(
                kinds,
                ["codex_version 0.45.0 → 0.46.0", "dagq_version v2 → v3"],
                "the start announced v1 → v2"
            );
        });
        assert_eq!(small, large);
        // Today's index walks back every claim without Codex's version.
        let (small, large) = reads_with(build, |reads, from, _| {
            let first = reads.id_after(from);
            assert!(
                reads
                    .existing_last_claim_with("codex_version", first)
                    .is_some()
            );
        });
        assert!(large > small * 10, "{small} {large}");
    }

    // (iv) --------------------------------------------------------------

    /// The first start of the range after one before it with the same
    /// `parallel` is routine, after one with another is not.
    #[test]
    fn iv_routine_reads_the_last_start_before_the_range() {
        for (before, routine) in [(4, true), (3, false)] {
            let build = |units| {
                let (mut history, from, to) = around(units, |_, _| {}, false);
                history.start("v1", before, from - MIN / 2);
                history.start("v1", 4, from + 10 * MIN);
                history.mark("a change", None, from + 20 * MIN);
                (history, from, to)
            };
            let (small, large) = reads_with(build, |reads, from, to| {
                let whole = whole_classified(reads, from, to);
                assert_eq!(classified_in(reads, from, to), whole);
                assert_eq!(whole[0].1, routine, "{before}");
            });
            assert_eq!(small, large);
        }
    }

    // (v) ---------------------------------------------------------------

    /// Marks `gap` finishes apart from before the range to after it: with
    /// fewer than `min_samples` they are one group reaching past both ends,
    /// with `min_samples` each is its own.
    #[test]
    fn v_a_group_is_found_whole_and_the_search_stops_at_min_samples() {
        for gap in [MIN_SAMPLES - 1, MIN_SAMPLES] {
            let build = |units| {
                let mut history = History::default();
                history.run(attributes("v0", 4, Some("0.45.0")), T0);
                history.start("v1", 4, T0 + MIN);
                let mut t = history.settled(units, T0 + 2 * MIN);
                let mut times = Vec::new();
                for n in 0..6 {
                    history.mark(&format!("chain {n}"), None, t);
                    times.push(t);
                    for _ in 0..gap {
                        t += 2 * MIN;
                        history.run(attributes("v1", 4, None), t);
                    }
                    t += 3 * MIN;
                }
                // The range holds the middle marks of the chain only.
                let (from, to) = (times[2] - 1, times[3]);
                history.settled(units, t + 10 * MIN);
                (history, from, to)
            };
            let (small, large) = reads_with(build, |reads, from, to| {
                let whole = whole_groups(reads, from, to);
                assert_eq!(groups_touching(reads, from, to, MIN_SAMPLES), whole);
                let sizes: Vec<usize> = whole.iter().map(Vec::len).collect();
                if gap < MIN_SAMPLES {
                    assert_eq!(sizes, [6], "one group past both ends");
                } else {
                    assert_eq!(sizes, [1, 1], "each its own");
                }
            });
            assert_eq!(small, large, "gap {gap}");
        }
    }

    /// A mark backdated before the first event of the history groups with
    /// a mark of the range: the slice widens back to the earliest
    /// effective time, not the first event.
    #[test]
    fn v_the_search_back_reaches_a_mark_backdated_before_the_history() {
        let build = |units| {
            let mut history = History::default();
            history.mark("before the history", Some(T0 - 24 * 60 * MIN), T0);
            history.mark("in the range", None, T0 + 2 * MIN);
            let (from, to) = (T0 + MIN, T0 + 3 * MIN);
            // Enough finishes before the first start (not routine) of the
            // settled history that it is a group of its own.
            for n in 0..MIN_SAMPLES as i64 {
                history.run(attributes("v1", 4, None), T0 + (4 + n) * MIN);
            }
            history.settled(units, T0 + 20 * MIN);
            (history, from, to)
        };
        let (small, large) = reads_with(build, |reads, from, to| {
            let whole = whole_groups(reads, from, to);
            assert_eq!(groups_touching(reads, from, to, MIN_SAMPLES), whole);
            let labels: Vec<Vec<&str>> = whole
                .iter()
                .map(|g| g.iter().map(|m| m.label.as_str()).collect())
                .collect();
            assert_eq!(labels, [["before the history", "in the range"]]);
        });
        assert_eq!(small, large);
    }

    /// Two marks and no finish between them, the first at the first time of
    /// the history: the search back reads it, the bound of a slice being
    /// exclusive.
    #[test]
    fn v_a_mark_at_the_first_time_of_the_history_is_read() {
        let build = |units| {
            let mut history = History::default();
            history.mark("at the first time", None, T0);
            history.mark("in the range", None, T0 + 2 * MIN);
            let (from, to) = (T0 + MIN, T0 + 3 * MIN);
            for n in 0..MIN_SAMPLES as i64 {
                history.run(attributes("v1", 4, None), T0 + (4 + n) * MIN);
            }
            history.settled(units, T0 + 20 * MIN);
            (history, from, to)
        };
        let (small, large) = reads_with(build, |reads, from, to| {
            let whole = whole_groups(reads, from, to);
            assert_eq!(groups_touching(reads, from, to, MIN_SAMPLES), whole);
            let labels: Vec<Vec<&str>> = whole
                .iter()
                .map(|g| g.iter().map(|m| m.label.as_str()).collect())
                .collect();
            assert_eq!(labels, [["at the first time", "in the range"]]);
        });
        assert_eq!(small, large);
    }

    /// A range `(10 min, 11 min]` with a mark at 10.5 min, `min_samples`
    /// finishes from 9.3 to 9.7 min, and `units` claims of runs not
    /// finished at `claims` (minutes): the finishes close the group.
    fn gap_history(units: i64, claims: (i64, i64)) -> (History, i64, i64) {
        let sec = 1000;
        let mut history = History::default();
        history.start("v1", 4, T0);
        // Runs finished a minute after their claims: 9.3 to 9.7 min.
        for n in 0..MIN_SAMPLES as i64 {
            history.run(attributes("v1", 4, None), T0 + 8 * MIN + (18 + n * 6) * sec);
        }
        for n in 0..units {
            history.claim(
                attributes("v1", 4, None),
                claims.0 + (claims.1 - claims.0) * n / units,
            );
        }
        // Put the events in time order again, with their ids.
        history.events.sort_by_key(created_ms);
        for (n, event) in history.events.iter_mut().enumerate() {
            event.id = EventId::new(n as i64 + 1);
        }
        history.mark("in the range", None, T0 + 10 * MIN + 30 * sec);
        // Runs after the range, enough to close the group forward.
        for n in 0..MIN_SAMPLES as i64 {
            history.run(attributes("v1", 4, None), T0 + (12 + n) * MIN);
        }
        (history, T0 + 10 * MIN, T0 + 11 * MIN)
    }

    /// Claims settled before the `min_samples`-th finish back are not read,
    /// however many.
    #[test]
    fn v_claims_before_the_finishes_that_close_the_group_are_not_read() {
        let sec = 1000;
        let claims = (T0 + 9 * MIN + 6 * sec, T0 + 9 * MIN + 12 * sec);
        let (small, large) = reads_with(
            |units| gap_history(units, claims),
            |reads, from, to| {
                let whole = whole_groups(reads, from, to);
                assert_eq!(groups_touching(reads, from, to, MIN_SAMPLES), whole);
                assert_eq!(whole.len(), 1);
            },
        );
        assert_eq!(small, large);
    }

    /// Claims of runs not finished between those finishes and the group's
    /// first mark are read, as many as there are: the read is bounded by
    /// the finishes, not by the claims in the gap.
    #[test]
    fn v_claims_between_the_finishes_and_the_group_are_read() {
        let sec = 1000;
        let claims = (T0 + 9 * MIN + 48 * sec, T0 + 10 * MIN + 24 * sec);
        let (small, large) = reads_with(
            |units| gap_history(units, claims),
            |reads, from, to| {
                let whole = whole_groups(reads, from, to);
                assert_eq!(groups_touching(reads, from, to, MIN_SAMPLES), whole);
                assert_eq!(whole.len(), 1);
            },
        );
        assert!(large > small * 10, "{small} {large}");
    }

    /// A mark recorded after the slice and backdated between two marks of
    /// a group, and a later retraction of one of them, change the group;
    /// the read by the effective time and by the mark finds both.
    #[test]
    fn v_a_backdated_mark_and_a_later_retraction_change_the_group_read() {
        for retract in [false, true] {
            let build = |units| {
                let mut history = History::default();
                history.run(attributes("v0", 4, Some("0.45.0")), T0);
                history.start("v1", 4, T0 + MIN);
                let t = history.settled(units, T0 + 2 * MIN);
                let first = history.mark("first", None, t);
                for n in 1..=MIN_SAMPLES as i64 {
                    history.run(attributes("v1", 4, None), t + n * 2 * MIN);
                }
                let second = t + 20 * MIN;
                history.mark("second", None, second);
                let (from, to) = (second - 1, second);
                let after = history.settled(units, second + 10 * MIN);
                // Backdated to between them: three finishes from each.
                history.mark("between", Some(t + 7 * MIN), after);
                if retract {
                    history.retract(first, after + MIN);
                }
                (history, from, to)
            };
            let (small, large) = reads_with(build, |reads, from, to| {
                let whole = whole_groups(reads, from, to);
                assert_eq!(groups_touching(reads, from, to, MIN_SAMPLES), whole);
                let labels: Vec<Vec<&str>> = whole
                    .iter()
                    .map(|g| g.iter().map(|m| m.label.as_str()).collect())
                    .collect();
                if retract {
                    assert_eq!(labels, [["between", "second"]]);
                } else {
                    assert_eq!(labels, [["first", "between", "second"]]);
                }
            });
            assert_eq!(small, large, "retract {retract}");
        }
    }
}
