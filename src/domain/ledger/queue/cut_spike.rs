//! Spike (docs/plans/ledger-cut-at-position-spike.md): whether the queue
//! rows the store holds, each written by a pass at a position `P`, can be
//! cut to an earlier position `at` from their heads, their records and
//! reads bounded per row, so that the rows cut equal the rows a pass at
//! `at` writes (the fold of the events up to `at`). A prototype of the
//! cut per kind of row, the reads it makes counted, and alternatives where
//! a bounded read does not settle a kind. Nothing here is on the
//! production path.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::*;
use crate::domain::ledger::{
    owner::{Owner, owner},
    testkit::{Log, built_and_settled, finding_record, queue_across, run_id, waits_across},
};
use crate::domain::marks::utc_text;
use crate::domain::pre_claim::rows::{Since, holds};
use crate::domain::supervisor_life::{LifeEnd, SILENT_AFTER_MS};

/// The reads a cut made, by what they read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Reads {
    /// Records read with their bodies by their row (`LedgerStore::ledger_row`).
    row_records: usize,
    /// Events of one run read (`LedgerStore::ledger_run_events`, cut at `at`).
    run_events: usize,
    /// Calls of the read the store lacks: the last event naming a
    /// supervisor at or before `at` (one indexed lookup each).
    last_naming: usize,
    /// Events read by an id range (`MarkLog::events_in_range`): the reads
    /// whose size is not bounded per row.
    range_events: usize,
}

impl std::ops::AddAssign for Reads {
    fn add_assign(&mut self, other: Self) {
        self.row_records += other.row_records;
        self.run_events += other.run_events;
        self.last_naming += other.last_naming;
        self.range_events += other.range_events;
    }
}

/// How a cut finds what it does not hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Way {
    /// The bounded reads, with the last event naming a supervisor by an
    /// indexed read the store would add (alternative iii), and a landing
    /// slot's through taken from `P` as it is (it overshoots `at`).
    Bounded,
    /// The same, but the last event naming a supervisor and a landing
    /// slot's through found by scanning the id range back from `at`.
    Range,
}

/// The store as a cut reads it: the events (for the reads it serves) and
/// the reads counted.
struct Store<'a> {
    events: &'a [RunEvent],
    reads: RefCell<Reads>,
}

impl<'a> Store<'a> {
    fn new(events: &'a [RunEvent]) -> Self {
        Self {
            events,
            reads: RefCell::new(Reads::default()),
        }
    }

    /// The records of the row of `row` with their bodies, as the store's
    /// row holds them.
    fn records(&self, row: &LedgerRow) -> Vec<RunEvent> {
        self.reads.borrow_mut().row_records += row.records.len();
        row.records.clone()
    }

    /// The events of `run` with an id at or before `at`.
    fn run_events(&self, run: &str, at: EventId) -> Vec<RunEvent> {
        let events: Vec<RunEvent> = self
            .events
            .iter()
            .filter(|event| event.id <= at && event.run_id.as_ref().map(RunId::as_str) == Some(run))
            .cloned()
            .collect();
        self.reads.borrow_mut().run_events += events.len();
        events
    }

    /// The last event naming `supervisor` at or before `at`, by the way
    /// `way` reads it.
    fn last_naming(&self, supervisor: &str, at: EventId, way: Way) -> Option<RunEvent> {
        let found = self
            .events
            .iter()
            .rev()
            .filter(|event| event.id <= at)
            .find(|event| supervisor_of(event) == Some(supervisor))
            .cloned();
        let mut reads = self.reads.borrow_mut();
        match way {
            Way::Bounded => reads.last_naming += 1,
            Way::Range => {
                let from = found.as_ref().map_or(0, |event| event.id.as_i64());
                reads.range_events += usize::try_from(at.as_i64() - from + 1).unwrap_or(0);
            }
        }
        found
    }

    /// The greatest id in `[from, at]` whose time is at or before `end`,
    /// by a scan of the range.
    fn greatest_by(&self, from: EventId, at: EventId, end: i64) -> Option<EventId> {
        let range: Vec<&RunEvent> = self
            .events
            .iter()
            .filter(|event| event.id >= from && event.id <= at)
            .collect();
        self.reads.borrow_mut().range_events += range.len();
        range
            .into_iter()
            .filter(|event| timestamp_millis(&event.created_at).is_some_and(|t| t <= end))
            .map(|event| event.id)
            .max()
    }

    fn take(&self) -> Reads {
        std::mem::take(&mut *self.reads.borrow_mut())
    }
}

/// The supervisors' lives at `at` as a cut makes them: of each, the last
/// event naming it at or before `at` and the stops at or before `at` of
/// the stops the queue rows record (`LedgerStore::ledger_supervisor_stops`).
struct LivesAt<'s, 'a> {
    store: &'s Store<'a>,
    stops: Vec<RunEvent>,
    at: EventId,
    way: Way,
}

impl LivesAt<'_, '_> {
    fn of(&self, supervisor: &str) -> Lives {
        let mut lives = Lives::default();
        if let Some(last) = self.store.last_naming(supervisor, self.at, self.way) {
            lives.go_on(supervisor, last.id, timestamp_millis(&last.created_at));
        }
        for stop in self
            .stops
            .iter()
            .filter(|stop| stop.id <= self.at && supervisor_of(stop) == Some(supervisor))
        {
            lives.add(stop);
        }
        lives
    }
}

fn kind_of(row: &LedgerRow) -> &str {
    row.subject
        .as_ref()
        .map_or("", |subject| subject.kind.as_str())
}

fn key_id(row: &LedgerRow) -> EventId {
    EventId::new(row.key.parse().unwrap_or(i64::MAX))
}

/// The row of a wait or a hold as open at `at`: its start its only record.
fn unended(row: &LedgerRow, mut interval: PreClaimInterval, at_row: &mut LedgerRow) {
    interval.ended_at = None;
    interval.end = None;
    interval.ended_by = None;
    at_row.detail = json!({"interval": interval});
    at_row.ended_at = None;
    at_row.outcome = None;
    at_row.records = row
        .records
        .iter()
        .filter(|event| event.id == key_id(row))
        .cloned()
        .collect();
    at_row.inputs_through.events = key_id(row);
    at_row.inputs_through.supervisor = None;
}

/// The rows `rows` (the queue rows a pass at `P` wrote, at `now_ms`) cut
/// to `at`, by `way`, from their heads and records and the reads `store`
/// counts.
fn cut(
    rows: &[LedgerRow],
    slots: &[EventId],
    stops: &[RunEvent],
    store: &Store<'_>,
    at: EventId,
    now_ms: i64,
    way: Way,
) -> Vec<LedgerRow> {
    let lives = LivesAt {
        store,
        stops: stops.to_vec(),
        at,
        way,
    };
    let mut out = Vec::new();
    for row in rows.iter().filter(|row| key_id(row) <= at) {
        let mut cut = row.clone();
        let through = row.inputs_through.events;
        match kind_of(row) {
            "landing_slot" => {
                let next = slots.iter().find(|key| **key > key_id(row)).copied();
                if !landing_slot(row, store, at, next, way, &mut cut) {
                    continue;
                }
            }
            "slots_full" | "claim_hold" | "claim_deferral" | "landing_hold" => {
                let Ok(interval) =
                    serde_json::from_value::<PreClaimInterval>(row.detail["interval"].clone())
                else {
                    continue;
                };
                let freed_or_own = interval.end.is_some()
                    && interval.end.as_deref() != Some(claim_hold::ENDED_SUPERVISOR_GONE);
                let stopped_hold = kind_of(row) != "slots_full"
                    && interval.end.as_deref() == Some(claim_hold::ENDED_SUPERVISOR_GONE);
                if (freed_or_own || stopped_hold) && through <= at {
                    // Ended by an event of its own by `at`: as written,
                    // but a hold its stop ended reads its supervisor's
                    // life up to `at`.
                    if stopped_hold && let Some(supervisor) = interval.supervisor.as_deref() {
                        cut.inputs_through.supervisor = lives.of(supervisor).last_of(supervisor);
                    }
                } else if kind_of(row) == "slots_full" {
                    let Some(supervisor) = interval.supervisor.clone() else {
                        continue;
                    };
                    unended(row, interval.clone(), &mut cut);
                    let life = lives.of(&supervisor);
                    let reach = life.last_of(&supervisor).unwrap_or(key_id(row));
                    cut.inputs_through.events = reach.max(key_id(row));
                    if let Some(end) = life.end(&supervisor, now_ms) {
                        let mut interval = interval;
                        let started = timestamp_millis(&interval.started_at).unwrap_or(end.at_ms());
                        let ended = end.at_ms().max(started);
                        interval.ended_at = Some(utc_text(ended));
                        interval.end = Some(claim_hold::ENDED_SUPERVISOR_GONE.to_owned());
                        interval.ended_by = match end {
                            LifeEnd::Stopped { .. } => interval.supervisor.clone(),
                            LifeEnd::Silent { .. } => None,
                        };
                        cut.detail = json!({"interval": interval});
                        cut.ended_at = Some(ended);
                        cut.outcome = Some(claim_hold::ENDED_SUPERVISOR_GONE.to_owned());
                        cut.inputs_through.supervisor = life.last_of(&supervisor);
                    }
                } else {
                    unended(row, interval, &mut cut);
                }
            }
            "live_alert_stream" => {
                let replaced = row.outcome.as_deref() == Some("replaced") && through <= at;
                if !replaced {
                    stream(row, store, &lives, at, now_ms, &mut cut);
                }
            }
            // An alert or a finding with no record after `at` is as
            // written.
            "live_alert" if row.records.iter().any(|event| event.id > at) => {
                cut.records.retain(|event| event.id <= at);
                cut.ended_at = None;
                cut.outcome = None;
                cut.inputs_through.events = key_id(row);
            }
            "finding" if row.records.iter().any(|event| event.id > at) => {
                let records: Vec<RunEvent> = store
                    .records(row)
                    .into_iter()
                    .filter(|event| event.id <= at)
                    .collect();
                let Some(last) = records.last() else {
                    continue;
                };
                let values = findings::values_of(last);
                cut.outcome = values.as_ref().map(|v| v.status.as_str().to_owned());
                cut.ended_at = values
                    .as_ref()
                    .filter(|v| !v.status.is_unsettled())
                    .and_then(|_| timestamp_millis(&last.created_at));
                cut.detail = last
                    .payload
                    .get(findings::VALUES)
                    .map_or(json!({}), |values| json!({"finding": values}));
                cut.task_id = records.iter().find_map(|event| event.task_id);
                cut.inputs_through.events = last.id;
                cut.records = records;
            }
            // One event of its own: as written.
            _ => {}
        }
        out.push(cut);
    }
    out
}

/// Cut a landing slot's row; whether its attempt started by `at`.
fn landing_slot(
    row: &LedgerRow,
    store: &Store<'_>,
    at: EventId,
    next: Option<EventId>,
    way: Way,
    cut: &mut LedgerRow,
) -> bool {
    let start = key_id(row);
    let Some(end) = row.ended_at else {
        // Open at `P`, open at `at`: it reads every event read.
        cut.inputs_through.events = at;
        return true;
    };
    // Ended by `at`: the next attempt started by then, or an end of its
    // own run's by then.
    let run = row.subject.as_ref().map_or("", |s| s.id.as_str());
    let mut end_id = next.filter(|next| *next <= at);
    if end_id.is_none() || way == Way::Range {
        let mut attempts = Attempts::from_parts(
            vec![Attempt {
                task_id: row.task_id,
                start: row.started_at.unwrap_or_default(),
                end: row.started_at.unwrap_or_default(),
                landed: false,
                open: true,
            }],
            RunId::new(run.to_owned()).ok().map(|run| (run, 0)),
        );
        for event in store.run_events(run, at).iter().filter(|e| e.id > start) {
            let (Some(run), Some(t)) = (&event.run_id, timestamp_millis(&event.created_at)) else {
                continue;
            };
            if attempts.add(event, run, t).ended.is_some() {
                end_id = Some(end_id.map_or(event.id, |id| id.min(event.id)));
                break;
            }
        }
    }
    let Some(end_id) = end_id else {
        cut.ended_at = None;
        cut.outcome = None;
        cut.inputs_through.events = at;
        return true;
    };
    if cut.inputs_through.events > at && way == Way::Range {
        cut.inputs_through.events = store
            .greatest_by(end_id, at, end)
            .map_or(start, |through| through.max(start));
    }
    true
}

/// Cut a stream not replaced by `at`: its records up to `at`, its version
/// the last of them carried, read up to the last event naming its
/// supervisor and ended by its life at `now_ms`.
fn stream(
    row: &LedgerRow,
    store: &Store<'_>,
    lives: &LivesAt<'_, '_>,
    at: EventId,
    now_ms: i64,
    cut: &mut LedgerRow,
) {
    let supervisor = row
        .subject
        .as_ref()
        .map_or("", |s| s.id.as_str())
        .to_owned();
    let records: Vec<RunEvent> = if row.records.iter().any(|event| event.id > at) {
        store
            .records(row)
            .into_iter()
            .filter(|event| event.id <= at)
            .collect()
    } else {
        row.records.clone()
    };
    let mut detail = json!({"baseline": row.detail["baseline"].as_bool().unwrap_or(false)});
    if let Some(version) = records
        .iter()
        .rev()
        .find_map(|event| live_alerts::StreamRecord::of(event).and_then(|r| r.version))
    {
        detail["version"] = json!(version);
    }
    let life = lives.of(&supervisor);
    let last = records.last().map_or(key_id(row), |event| event.id);
    cut.records = records;
    cut.detail = detail;
    cut.ended_at = None;
    cut.outcome = None;
    cut.inputs_through.supervisor = None;
    cut.inputs_through.events = last.max(life.last_of(&supervisor).unwrap_or(last));
    if let Some(end) = life.end(&supervisor, now_ms) {
        let started = row.started_at.unwrap_or(end.at_ms());
        cut.ended_at = Some(end.at_ms().max(started));
        cut.outcome = Some(claim_hold::ENDED_SUPERVISOR_GONE.into());
        cut.inputs_through.supervisor = life.last_of(&supervisor);
    }
}

/// The stops the queue rows `rows` record
/// (`LedgerStore::ledger_supervisor_stops`).
fn stops_of(rows: &[LedgerRow]) -> Vec<RunEvent> {
    rows.iter()
        .flat_map(|row| row.records.iter())
        .filter(|event| event.kind == EventKind::SupervisorStopped)
        .cloned()
        .collect()
}

/// The keys of the landing slot's rows (the store's heads give them).
fn slot_keys(rows: &[LedgerRow]) -> Vec<EventId> {
    rows.iter()
        .filter(|row| kind_of(row) == "landing_slot")
        .map(key_id)
        .collect()
}

fn owned_of(events: &[RunEvent]) -> Vec<RunEvent> {
    events
        .iter()
        .filter(|event| owner(event) == Owner::Queue)
        .cloned()
        .collect()
}

/// The queue rows a pass at the last of `events` writes at `now_ms`.
fn written(events: &[RunEvent], now_ms: i64) -> Vec<LedgerRow> {
    queue_rows(&owned_of(events), events, now_ms, "b")
}

/// What the cuts of one history gave: per kind, the positions at which a
/// cut row differed from the row the pass at `at` writes, and the reads.
#[derive(Debug, Default)]
struct Outcome {
    differ: BTreeMap<String, Vec<(i64, String)>>,
    rows: BTreeMap<String, usize>,
    reads: BTreeMap<String, Reads>,
    max_reads: BTreeMap<String, Reads>,
}

fn max_reads(a: Reads, b: Reads) -> Reads {
    Reads {
        row_records: a.row_records.max(b.row_records),
        run_events: a.run_events.max(b.run_events),
        last_naming: a.last_naming.max(b.last_naming),
        range_events: a.range_events.max(b.range_events),
    }
}

/// Cut the rows a pass at the last event writes to every position in
/// `ats` (each one row of each kind at a time, so the reads are the
/// kind's) and compare each with the row the pass at it writes.
fn cut_all(events: &[RunEvent], ats: &[EventId], now_ms: i64, way: Way) -> Outcome {
    let at_p = written(events, now_ms);
    let slots = slot_keys(&at_p);
    let stops = stops_of(&at_p);
    let store = Store::new(events);
    let mut outcome = Outcome::default();
    for &at in ats {
        let upto: Vec<RunEvent> = events.iter().filter(|e| e.id <= at).cloned().collect();
        let want: BTreeMap<String, LedgerRow> = written(&upto, now_ms)
            .into_iter()
            .map(|row| (row.key.clone(), row))
            .collect();
        let mut got_keys = BTreeSet::new();
        for row in &at_p {
            let kind = match kind_of(row) {
                k @ ("landing_slot" | "slots_full" | "claim_hold" | "claim_deferral"
                | "landing_hold" | "live_alert_stream" | "live_alert" | "finding") => k,
                _ => "event",
            }
            .to_owned();
            store.take();
            let cut_rows = cut(
                std::slice::from_ref(row),
                &slots,
                &stops,
                &store,
                at,
                now_ms,
                way,
            );
            let reads = store.take();
            *outcome.reads.entry(kind.clone()).or_default() += reads;
            let max = outcome.max_reads.entry(kind.clone()).or_default();
            *max = max_reads(*max, reads);
            for got in cut_rows {
                *outcome.rows.entry(kind.clone()).or_default() += 1;
                got_keys.insert(got.key.clone());
                let same = want
                    .get(&got.key)
                    .is_some_and(|want| QueueHead::of(want) == QueueHead::of(&got));
                // A landing slot's row that differs in more than its
                // through is a difference of another kind.
                let besides_through = kind == "landing_slot"
                    && want.get(&got.key).is_none_or(|want| {
                        let mut got = got.clone();
                        got.inputs_through.events = want.inputs_through.events;
                        QueueHead::of(want) != QueueHead::of(&got)
                    });
                if besides_through {
                    outcome
                        .differ
                        .entry("landing_slot_besides_through".into())
                        .or_default()
                        .push((at.as_i64(), got.key.clone()));
                }
                if !same {
                    outcome
                        .differ
                        .entry(kind.clone())
                        .or_default()
                        .push((at.as_i64(), got.key.clone()));
                }
            }
        }
        for key in want.keys().filter(|key| !got_keys.contains(*key)) {
            outcome
                .differ
                .entry("missing".into())
                .or_default()
                .push((at.as_i64(), key.clone()));
        }
    }
    outcome
}

/// The history the spike cuts: of every kind of queue row, ends just
/// before and after positions, a supervisor's stop and silence, a claim
/// that ends a deferral, a hold replaced by the next, and an event timed
/// before an attempt's end that comes after it (a slot's through past its
/// end event). `fill` settled rows of their own (marks, holds on the
/// deferrals of tasks of their own a claim ended) go between the
/// first attempt's end and the late event, and as many more in the
/// silence of `s2`.
fn spike_log(fill: usize) -> Log {
    let alert = json!({"supervisor": "s1", "judgment": "slots", "kind": "idle"});
    let (a, b, c, d) = (run_id(1), run_id(2), run_id(3), run_id(4));
    let mut log = Log::default();
    let filler = |log: &mut Log, secs: i64| {
        for n in 0..fill {
            let task = 100 + i64::try_from(n).unwrap_or_default();
            let run = format!("{:08x}-2222-4222-8222-222222222222", task);
            log.add(None, None, "mark_added", json!({"mark": "f"}), secs)
                .add(
                    Some(task),
                    None,
                    "claim_deferred",
                    json!({"supervisor": "f", "reason": "paths"}),
                    secs,
                )
                .add(Some(task), Some(&run), "run_claimed", json!({}), secs);
        }
    };
    log.add(
        None,
        None,
        "supervisor_started",
        json!({"supervisor": "s1"}),
        0,
    )
    .add(
        None,
        None,
        "live_alert_baseline",
        json!({"supervisor": "s1", "version": 1}),
        1,
    )
    .add(
        None,
        None,
        "slots_full_started",
        json!({"supervisor": "s2", "class": "heavy"}),
        2,
    )
    .add(None, None, "live_alert_started", alert.clone(), 3)
    .add(None, None, "claim_held", json!({"supervisor": "s3"}), 4)
    .add(
        Some(5),
        None,
        "claim_deferred",
        json!({"supervisor": "s1", "reason": "paths"}),
        5,
    )
    .add(None, None, "landing_held", json!({"supervisor": "s1"}), 6)
    .add(None, None, "finding_recorded", finding_record("open"), 7)
    .add(Some(1), Some(&a), "integration_started", json!({}), 8)
    .add(Some(1), Some(&a), "run_integrated", json!({}), 20)
    .add(Some(2), Some(&b), "integration_started", json!({}), 21);
    filler(&mut log, 22);
    log.add(None, None, "mark_added", json!({"mark": "g"}), 23)
        .add(Some(9), None, "task_created", json!({}), 15)
        .add(
            None,
            None,
            "live_alert_input_changed",
            json!({"supervisor": "s1", "version": 2}),
            30,
        )
        .add(
            None,
            None,
            "supervisor_stopped",
            json!({"supervisor": "s3"}),
            31,
        )
        .add(None, None, "live_alert_ended", alert, 32)
        .add(
            None,
            None,
            "finding_recorded",
            finding_record("resolved"),
            33,
        )
        .add(None, None, "landing_held", json!({"supervisor": "s1"}), 34)
        .add(Some(3), Some(&c), "integration_started", json!({}), 35)
        .add(Some(5), Some(&d), "run_claimed", json!({}), 36)
        .add(
            None,
            None,
            "slots_full_started",
            json!({"supervisor": "s4", "class": "light"}),
            38,
        )
        .add(
            None,
            None,
            "slots_full_ended",
            json!({"supervisor": "s4", "class": "light"}),
            39,
        )
        .add(
            None,
            None,
            "live_alert_baseline",
            json!({"supervisor": "s1", "version": 3}),
            40,
        );
    let silence = 2 + SILENT_AFTER_MS / 1000;
    filler(&mut log, silence - 10);
    log.add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s1"}),
        silence - 5,
    )
    .add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s4"}),
        silence + 5,
    )
    .add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s2"}),
        silence + 20,
    )
    .add(
        None,
        None,
        "landing_resumed",
        json!({"supervisor": "s1"}),
        silence + 21,
    )
    .add(Some(3), Some(&c), "run_integrated", json!({}), silence + 22)
    .add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s1"}),
        silence + 25,
    );
    log
}

/// The ids of the events of `log` that are not fillers: the positions
/// the cuts are compared at, the same in a small and a large history.
fn base_ats(log: &Log) -> Vec<EventId> {
    log.events
        .iter()
        .filter(|e| e.payload.get("supervisor").and_then(Value::as_str) != Some("f"))
        .filter(|e| e.payload.get("mark").and_then(Value::as_str) != Some("f"))
        .filter(|e| e.task_id.is_none_or(|task| task.as_i64() < 100))
        .map(|e| e.id)
        .collect()
}

fn all_ats(log: &Log) -> Vec<EventId> {
    log.events.iter().map(|e| e.id).collect()
}

/// The histories and times the spike cuts at: its own (small), and the
/// ledger's tests' logs of a position.
fn histories() -> Vec<(&'static str, Log, Vec<i64>)> {
    let last = |log: &Log| timestamp_millis(&log.events.last().unwrap().created_at).unwrap();
    let spike = spike_log(1);
    let (across, _) = queue_across();
    let (waits, _, _) = waits_across();
    vec![
        (
            "spike",
            spike.clone(),
            vec![last(&spike) + 1_000, last(&spike) + 10 * SILENT_AFTER_MS],
        ),
        (
            "queue_across",
            across.clone(),
            vec![last(&across) + 1_000, last(&across) + 10 * SILENT_AFTER_MS],
        ),
        (
            "waits_across",
            waits.clone(),
            vec![last(&waits) + 1_000, last(&waits) + 10 * SILENT_AFTER_MS],
        ),
    ]
}

/// Every kind but the landing slot's cuts to every position, at a time
/// soon after the last event and one past every silence, to the row the
/// pass at that position writes, with the bounded reads (the last event
/// naming a supervisor by the indexed read the store lacks); a landing
/// slot's differs only in its through, where an event after `at` timed
/// before its end moved it past `at`.
#[test]
fn spike_cut_rows_match_the_rows_written_at_the_position() {
    for (name, log, nows) in histories() {
        for now in nows {
            let outcome = cut_all(&log.events, &all_ats(&log), now, Way::Bounded);
            let differ: BTreeMap<_, _> = outcome
                .differ
                .iter()
                .filter(|(kind, _)| kind.as_str() != "landing_slot")
                .collect();
            assert!(differ.is_empty(), "{name} {now}: {differ:?}");
            eprintln!("{name} now={now} rows={:?}", outcome.rows);
            eprintln!(
                "{name} landing_slot differs at {:?}",
                outcome.differ.get("landing_slot")
            );
        }
    }
}

/// The landing slot's through: taken from `P` it overshoots `at` exactly
/// where an event after `at` timed before the attempt's end is the
/// greatest; found by a scan of the range from the end event it is the
/// row written at `at`.
#[test]
fn spike_landing_slot_through_needs_a_range_read() {
    let log = spike_log(1);
    let now = timestamp_millis(&log.events.last().unwrap().created_at).unwrap() + 1_000;
    let bounded = cut_all(&log.events, &all_ats(&log), now, Way::Bounded);
    let differ = bounded
        .differ
        .get("landing_slot")
        .cloned()
        .unwrap_or_default();
    // The first attempt (key 9) ends at 10; the late event comes after
    // the fillers.
    assert!(!differ.is_empty());
    assert!(differ.iter().all(|(_, key)| key == "9"), "{differ:?}");
    let ranged = cut_all(&log.events, &all_ats(&log), now, Way::Range);
    assert_eq!(
        ranged.differ.get("landing_slot"),
        None,
        "{:?}",
        ranged.differ
    );
}

/// The reads of each kind's cut, the same positions cut in a small and a
/// large history (more settled rows of their own between the first
/// attempt's end and the late event, and in `s2`'s silence): the bounded
/// reads do not grow; the range reads (a supervisor's last event and a
/// slot's through found by a scan back from `at`) grow with the history.
#[test]
fn spike_reads_per_cut_do_not_grow_with_settled_rows() {
    let measure = |fill: usize, way: Way| {
        let log = spike_log(fill);
        let now = timestamp_millis(&log.events.last().unwrap().created_at).unwrap() + 1_000;
        let outcome = cut_all(&log.events, &base_ats(&log), now, way);
        assert!(
            outcome.differ.keys().all(|k| k == "landing_slot") || way == Way::Range,
            "{:?}",
            outcome.differ
        );
        if way == Way::Range {
            assert!(outcome.differ.is_empty(), "{:?}", outcome.differ);
        }
        (outcome.reads, outcome.max_reads)
    };
    let (small, small_max) = measure(1, Way::Bounded);
    let (large, large_max) = measure(50, Way::Bounded);
    eprintln!("bounded small total {small:?}\nbounded large total {large:?}");
    eprintln!("bounded small max {small_max:?}\nbounded large max {large_max:?}");
    assert_eq!(small, large);
    let (small_r, small_rmax) = measure(1, Way::Range);
    let (large_r, large_rmax) = measure(50, Way::Range);
    eprintln!("range small total {small_r:?}\nrange large total {large_r:?}");
    eprintln!("range small max {small_rmax:?}\nrange large max {large_rmax:?}");
    let ranged =
        |reads: &BTreeMap<String, Reads>| -> usize { reads.values().map(|r| r.range_events).sum() };
    assert!(ranged(&large_r) > ranged(&small_r));
    for kind in ["slots_full", "live_alert_stream", "landing_slot"] {
        assert!(
            large_rmax[kind].range_events > small_rmax[kind].range_events,
            "{kind}"
        );
    }
}

/// [`Since`] cut: of the task rows a pass at `P` wrote, those that began
/// by `at` and were not settled at `P` or read past `at`. It is never
/// later than the [`Since`] of the task rows a pass at `at` writes, so the
/// wait rows it keeps ([`holds`]) are a superset of those that pass keeps.
#[test]
fn spike_since_cut_is_no_later_than_the_since_at_the_position() {
    for (name, log, nows) in histories() {
        for now in nows {
            let at_p = built_and_settled(&log.events, now);
            for at in all_ats(&log) {
                let upto: Vec<RunEvent> =
                    log.events.iter().filter(|e| e.id <= at).cloned().collect();
                let at_rows = built_and_settled(&upto, now);
                let exact = Since::of(&at_rows);
                let cut = Since::of(
                    at_p.iter()
                        .filter(|row| row.inputs_from <= at)
                        .map(|row| {
                            let mut row = row.clone();
                            if row.inputs_through.last() > at {
                                row.is_final = false;
                            }
                            row
                        })
                        .collect::<Vec<_>>()
                        .iter(),
                );
                assert!(
                    cut.ms <= exact.ms && cut.id <= exact.id,
                    "{name} {at}: {cut:?} {exact:?}"
                );
                let queue: Vec<&LedgerRow> = at_rows
                    .iter()
                    .filter(|r| r.kind == LedgerKind::Queue)
                    .collect();
                for row in queue {
                    assert!(
                        !holds(row, exact) || holds(row, cut),
                        "{name} {at}: {}",
                        row.key
                    );
                }
            }
        }
    }
}

/// The book made again from the rows cut to `at` (the landing slot's
/// through taken from `P`, overshooting) and given the events after `at`
/// folds the rows of every event: the overshoot is read again.
#[test]
fn spike_book_of_rows_cut_goes_on_as_the_fold_of_every_event() {
    for (name, log, nows) in histories() {
        for now in nows {
            let at_p = written(&log.events, now);
            let whole = written(&log.events, now);
            let owned = owned_of(&log.events);
            for at in all_ats(&log) {
                let store = Store::new(&log.events);
                let rows = cut(
                    &at_p,
                    &slot_keys(&at_p),
                    &stops_of(&at_p),
                    &store,
                    at,
                    now,
                    Way::Bounded,
                );
                let heads: Vec<QueueHead> = rows.iter().map(QueueHead::of).collect();
                let (before, after): (Vec<&RunEvent>, Vec<&RunEvent>) =
                    log.events.iter().partition(|e| e.id <= at);
                let (mut lives, mut pre_claim) = (Lives::default(), PreClaimBook::default());
                for event in &before {
                    lives.add(event);
                    pre_claim.add(event);
                }
                let mut book = QueueBook::from_heads(&heads, &pre_claim, at);
                for event in after {
                    lives.add(event);
                    let moved = pre_claim.add(event);
                    book.add(
                        event,
                        owner(event) == Owner::Queue,
                        moved,
                        &mut BTreeSet::new(),
                    );
                }
                let at_ = At {
                    owned: &owned,
                    pre_claim: &pre_claim,
                    lives: &lives,
                    now_ms: now,
                    built_at: "b",
                };
                let mut got: Vec<LedgerRow> =
                    book.keys().filter_map(|key| book.row(&key, &at_)).collect();
                got.sort_by_key(|row| row.inputs_from);
                assert_eq!(got, whole, "{name} {now} {at}");
            }
        }
    }
}

/// The supervisors' lives and the waits before a claim made again
/// ([`crate::domain::pre_claim::rows::books`]) from the rows cut to `at`,
/// kept by the [`Since`] cut, hold every wait the books made from the
/// rows a pass at `at` writes (kept by its exact [`Since`]) hold, as they
/// hold it, and end each supervisor's life as those books do; what they
/// hold besides are waits that ended before the exact [`Since`].
#[test]
fn spike_wait_books_of_rows_cut_are_those_of_the_rows_at_the_position() {
    use crate::domain::pre_claim::rows::{books, living};
    let by_id = |events: &[RunEvent], ids: &[EventId]| -> Vec<RunEvent> {
        events
            .iter()
            .filter(|event| ids.contains(&event.id))
            .cloned()
            .collect()
    };
    for (name, log, nows) in histories() {
        for now in nows {
            let at_p = written(&log.events, now);
            let tasks_p = built_and_settled(&log.events, now);
            for at in all_ats(&log) {
                let upto: Vec<RunEvent> =
                    log.events.iter().filter(|e| e.id <= at).cloned().collect();
                let exact_since = Since::of(&built_and_settled(&upto, now));
                let cut_since = Since::of(
                    tasks_p
                        .iter()
                        .filter(|row| row.inputs_from <= at)
                        .map(|row| LedgerRow {
                            is_final: row.is_final && row.inputs_through.last() <= at,
                            ..row.clone()
                        })
                        .collect::<Vec<_>>()
                        .iter(),
                );
                let store = Store::new(&log.events);
                let stops = stops_of(&at_p);
                let cut_rows = cut(
                    &at_p,
                    &slot_keys(&at_p),
                    &stops,
                    &store,
                    at,
                    now,
                    Way::Bounded,
                );
                let at_rows = written(&upto, now);
                let book_of = |rows: &[LedgerRow], since: Since| {
                    let held: Vec<LedgerRow> = rows
                        .iter()
                        .filter(|row| holds(row, since))
                        .cloned()
                        .collect();
                    let stops: Vec<RunEvent> =
                        stops.iter().filter(|stop| stop.id <= at).cloned().collect();
                    let last = by_id(&upto, &living(&held, &stops));
                    books(&held, &stops, &last, now)
                };
                let (cut_lives, cut_book) = book_of(&cut_rows, cut_since);
                let (at_lives, at_book) = book_of(&at_rows, exact_since);
                let cut_waits: BTreeMap<EventId, PreClaimInterval> = cut_book
                    .intervals(&cut_lives, now)
                    .map(|w| (w.id, w.into_owned()))
                    .collect();
                for wait in at_book.intervals(&at_lives, now) {
                    assert_eq!(
                        cut_waits.get(&wait.id),
                        Some(&*wait),
                        "{name} {now} {at}: {}",
                        wait.id
                    );
                }
                for (id, wait) in &cut_waits {
                    if !at_book.holds(*id) {
                        let ended = wait.ended_at.as_deref().and_then(timestamp_millis);
                        assert!(
                            ended.is_some_and(|end| end < exact_since.ms),
                            "{name} {now} {at}: {id}"
                        );
                    }
                }
                let supervisors: BTreeSet<String> = at_rows
                    .iter()
                    .filter_map(|row| row.subject.as_ref().map(|s| s.id.clone()))
                    .collect();
                for supervisor in &supervisors {
                    if at_lives.last_of(supervisor).is_some() {
                        assert_eq!(
                            (
                                cut_lives.end(supervisor, now),
                                cut_lives.last_of(supervisor),
                                cut_lives.ends_from(supervisor)
                            ),
                            (
                                at_lives.end(supervisor, now),
                                at_lives.last_of(supervisor),
                                at_lives.ends_from(supervisor)
                            ),
                            "{name} {now} {at}: {supervisor}"
                        );
                    }
                }
            }
        }
    }
}

/// One attempt and two events after its end timed before it (`A`, then
/// `B`), with an event between that reaches no attempt: the history in
/// which a through cut from `P` = `B` goes back to `A` during the replay.
fn two_late_log() -> Log {
    let a = run_id(1);
    let mut log = Log::default();
    log.add(Some(1), Some(&a), "integration_started", json!({}), 8)
        .add(Some(1), Some(&a), "run_integrated", json!({}), 20)
        .add(None, None, "mark_added", json!({"mark": "m"}), 21)
        .add(Some(7), None, "task_created", json!({}), 15)
        .add(Some(8), None, "task_created", json!({}), 16)
        .add(None, None, "mark_added", json!({"mark": "n"}), 30);
    log
}

/// What a landing slot's row cut from `P` (its through kept) is at each
/// step of the replay of (at, P], against the row the fold of the events
/// up to that step gives: until a replayed event reaches its attempt it
/// is ahead (`P`'s through, past the fold's; else the same row), and from
/// then on it is the fold's, so a through `P` set past an event that
/// reaches it later goes back to that event's id. No other difference.
#[test]
fn spike_landing_slot_through_during_the_replay() {
    let mut cases = histories();
    cases.push((
        "two_late",
        two_late_log(),
        vec![crate::domain::ledger::testkit::ms(1_000)],
    ));
    let mut seen = BTreeMap::<&str, (usize, usize)>::new();
    for (name, log, nows) in cases {
        for now in nows {
            let at_p = written(&log.events, now);
            let owned = owned_of(&log.events);
            for at in all_ats(&log) {
                let store = Store::new(&log.events);
                let rows = cut(
                    &at_p,
                    &slot_keys(&at_p),
                    &stops_of(&at_p),
                    &store,
                    at,
                    now,
                    Way::Bounded,
                );
                let heads: Vec<QueueHead> = rows.iter().map(QueueHead::of).collect();
                let (mut lives, mut pre_claim) = (Lives::default(), PreClaimBook::default());
                for event in log.events.iter().filter(|e| e.id <= at) {
                    lives.add(event);
                    pre_claim.add(event);
                }
                let mut book = QueueBook::from_heads(&heads, &pre_claim, at);
                let mut before: BTreeMap<String, EventId> = BTreeMap::new();
                for event in log.events.iter().filter(|e| e.id > at) {
                    lives.add(event);
                    let moved = pre_claim.add(event);
                    book.add(
                        event,
                        owner(event) == Owner::Queue,
                        moved,
                        &mut BTreeSet::new(),
                    );
                    let upto: Vec<RunEvent> = log
                        .events
                        .iter()
                        .filter(|e| e.id <= event.id)
                        .cloned()
                        .collect();
                    let want: BTreeMap<String, LedgerRow> = written(&upto, now)
                        .into_iter()
                        .filter(|row| kind_of(row) == "landing_slot")
                        .map(|row| (row.key.clone(), row))
                        .collect();
                    let fold = At {
                        owned: &owned,
                        pre_claim: &pre_claim,
                        lives: &lives,
                        now_ms: now,
                        built_at: "b",
                    };
                    for (key, want) in &want {
                        let got = book.row(key, &fold).expect("a slot's row");
                        let through = got.inputs_through.events;
                        if through != want.inputs_through.events {
                            let mut same = got.clone();
                            same.inputs_through.events = want.inputs_through.events;
                            assert_eq!(&same, want, "{name} {at} {}: {key}", event.id);
                            assert!(through > want.inputs_through.events, "{name} {at}: {key}");
                            seen.entry(name).or_default().0 += 1;
                        }
                        if before.get(key).is_some_and(|was| *was > through) {
                            seen.entry(name).or_default().1 += 1;
                        }
                        before.insert(key.clone(), through);
                    }
                }
            }
        }
    }
    eprintln!("(ahead steps, went-back steps) by history: {seen:?}");
    assert!(
        seen.get("two_late")
            .is_some_and(|(ahead, back)| *ahead > 0 && *back > 0)
    );
}
