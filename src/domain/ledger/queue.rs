//! The ledger's queue rows: one per interval or event of the queue as a
//! whole, keyed by the id of the event that starts it. The kinds
//! (`subject.kind`):
//!
//! - `slots_full`, `claim_hold`, `claim_deferral`: the waits before a claim
//!   as [`pre_claim_intervals`] reads them; `landing_hold`: a hold on the
//!   landings by the same rule ([`claim_hold::hold_spans`]). Its start and
//!   its end are its records.
//! - `live_alert_stream`: a supervisor's stream of the record of the
//!   judgments of now from its baseline (or its first record) to its next
//!   baseline: the inputs and their versions and its reaches, folded per
//!   stream; `live_alert`: one alert from its start to its end (an
//!   interval only, never the alert's value).
//! - `finding`: one finding from its first record, with its values now.
//! - `landing_slot`: one `integrate` attempt holding the integration slot
//!   ([`landing_utilization::spans`]); its records are its run's.
//! - any other kind: one event (main's history, a goal's attributes, an
//!   update's step, a mark, a backend's failure), its kind the row's.
//!
//! An interval with no end yet has no `ended_at` and is not settled
//! ([`super::settle`]). Reads nothing but the events.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

use super::{LedgerKind, LedgerRow, Subject, findings};
use crate::domain::{
    EventId, EventKind, RunEvent, claim_defer, claim_hold,
    event_kind::{self, SLOTS_FULL_ENDED, SLOTS_FULL_STARTED},
    live_alerts::{self, AlertKey, StreamRecord},
    pre_claim::{
        IntervalKind, PreClaimInterval, SlotClass, landing_hold_intervals, pre_claim_intervals,
        slots_full_of,
    },
    stats::{landing_utilization, timestamp_millis},
    supervisor_life::{supervisor_life_end, supervisor_of},
};

const LIVE_ALERT_BASELINE: &str = EventKind::LiveAlertBaseline.as_str();
const LIVE_ALERT_STARTED: &str = EventKind::LiveAlertStarted.as_str();
const LIVE_ALERT_ENDED: &str = EventKind::LiveAlertEnded.as_str();

/// A queue row as it is folded.
struct Building {
    start: EventId,
    kind: String,
    about: String,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    outcome: Option<String>,
    detail: Value,
    records: Vec<EventId>,
    /// The supervisor whose life's end ended it, when that is its end.
    life: Option<String>,
}

impl Building {
    fn new(start: &RunEvent, kind: &str, about: String) -> Self {
        Self {
            start: start.id,
            kind: kind.to_owned(),
            about,
            started_at: timestamp_millis(&start.created_at),
            ended_at: None,
            outcome: None,
            detail: json!({}),
            records: vec![start.id],
            life: None,
        }
    }
}

const fn interval_kind(kind: IntervalKind) -> &'static str {
    match kind {
        IntervalKind::SlotsFull => "slots_full",
        IntervalKind::ClaimHold => "claim_hold",
        IntervalKind::ClaimDeferral => "claim_deferral",
    }
}

/// The end event of each slots-full interval, by its start: its
/// supervisor's next `slots_full_ended` of its class (the rule of
/// [`pre_claim_intervals`]).
fn slots_full_ends(events: &[RunEvent]) -> HashMap<EventId, EventId> {
    let mut open: BTreeMap<(String, SlotClass), EventId> = BTreeMap::new();
    let mut ends = HashMap::new();
    for event in events {
        let Some(key) = slots_full_of(event) else {
            continue;
        };
        match event.kind.as_str() {
            SLOTS_FULL_STARTED => {
                open.entry(key).or_insert(event.id);
            }
            SLOTS_FULL_ENDED => {
                if let Some(start) = open.remove(&key) {
                    ends.insert(start, event.id);
                }
            }
            _ => {}
        }
    }
    ends
}

/// The end event of each hold and deferral, by its start.
fn span_ends(events: &[RunEvent]) -> HashMap<EventId, EventId> {
    claim_hold::hold_spans(claim_hold::CLAIMS, events)
        .into_iter()
        .chain(claim_defer::deferral_spans(events))
        .filter_map(|span| Some((span.start.id, span.end?.event.id)))
        .collect()
}

/// The interval row of a wait before a claim or a hold on the landings.
fn interval_row(start: &RunEvent, kind: &str, interval: &PreClaimInterval) -> Building {
    let about = interval
        .supervisor
        .clone()
        .or_else(|| interval.task_id.map(|task| task.to_string()))
        .unwrap_or_default();
    let mut row = Building::new(start, kind, about);
    row.ended_at = interval.ended_at.as_deref().and_then(timestamp_millis);
    row.outcome.clone_from(&interval.end);
    if interval.end.as_deref() == Some(claim_hold::ENDED_SUPERVISOR_GONE) {
        row.life.clone_from(&interval.supervisor);
    }
    row.detail = json!({"interval": interval});
    row
}

/// The queue rows of `owned` (the events of the queue as a whole,
/// [`super::owner::Owner::Queue`], ascending id), read against `all` (the
/// whole EventStore: the supervisors' lives end intervals, and the runs'
/// events give the landing slot's use) at `now_ms`, built at `built_at`.
/// Each of `owned` is the records of exactly one row.
pub fn queue_rows(
    owned: &[RunEvent],
    all: &[RunEvent],
    now_ms: i64,
    built_at: &str,
) -> Vec<LedgerRow> {
    let by_id: HashMap<EventId, &RunEvent> = owned.iter().map(|event| (event.id, event)).collect();
    let mut rows: Vec<Building> = Vec::new();
    let mut taken: HashSet<EventId> = HashSet::new();
    let mut ends: HashMap<EventId, EventId> = slots_full_ends(all);
    ends.extend(span_ends(all));
    let mut intervals: Vec<(PreClaimInterval, &'static str, Option<EventId>)> =
        pre_claim_intervals(all, now_ms)
            .into_iter()
            .map(|interval| {
                let end = ends.get(&interval.id).copied();
                let kind = interval_kind(interval.kind);
                (interval, kind, end)
            })
            .collect();
    intervals.extend(
        landing_hold_intervals(all)
            .into_iter()
            .map(|(interval, end)| (interval, "landing_hold", end)),
    );
    for (interval, kind, _) in &intervals {
        if let Some(start) = by_id.get(&interval.id) {
            taken.insert(start.id);
            rows.push(interval_row(start, kind, interval));
        }
    }
    // An end is its interval's unless it starts one itself (a hold the
    // next one replaced).
    for (interval, _, end) in &intervals {
        if let Some(end) = end.filter(|end| by_id.contains_key(end) && !taken.contains(end))
            && let Some(row) = rows.iter_mut().find(|row| row.start == interval.id)
        {
            taken.insert(end);
            row.records.push(end);
        }
    }
    streams(owned, all, now_ms, &mut rows, &mut taken);
    alerts(owned, &mut rows, &mut taken);
    finding_rows(owned, &mut rows, &mut taken);
    for event in owned.iter().filter(|event| !taken.contains(&event.id)) {
        let about = supervisor_of(event)
            .map(str::to_owned)
            .or_else(|| event.goal_id.map(|goal| goal.to_string()))
            .unwrap_or_default();
        let mut row = Building::new(event, &event.kind, about);
        row.ended_at = row.started_at;
        rows.push(row);
    }
    // The last event naming each supervisor: what a life's end was read
    // from (`supervisor_life_end` reads only those and the time).
    let mut lives: HashMap<&str, EventId> = HashMap::new();
    for event in all {
        if let Some(supervisor) = supervisor_of(event) {
            lives.insert(supervisor, event.id);
        }
    }
    let mut ledger: Vec<LedgerRow> = rows
        .into_iter()
        .map(|row| {
            let life = row.life.as_deref().and_then(|s| lives.get(s)).copied();
            let mut ledger = finish(row, &by_id, built_at);
            ledger.inputs_through.supervisor = life;
            ledger
        })
        .collect();
    ledger.extend(landing_slots(all, now_ms, built_at));
    ledger.sort_by_key(|row| row.inputs_from);
    ledger
}

/// The streams of the record of the judgments of now: per supervisor,
/// from a baseline (or its first record) to its next baseline, ended
/// there (`replaced`) or by the end of its supervisor's life.
fn streams(
    owned: &[RunEvent],
    all: &[RunEvent],
    now_ms: i64,
    rows: &mut Vec<Building>,
    taken: &mut HashSet<EventId>,
) {
    let mut open: HashMap<String, usize> = HashMap::new();
    for event in owned {
        let kind = event.kind.as_str();
        if !live_alerts::RECORD_KINDS.contains(&kind)
            || [LIVE_ALERT_STARTED, LIVE_ALERT_ENDED].contains(&kind)
        {
            continue;
        }
        let Some(record) = StreamRecord::of(event) else {
            continue;
        };
        let supervisor = record.supervisor.as_str();
        let current = open.get(supervisor).copied();
        let at = match current {
            Some(at) if kind != LIVE_ALERT_BASELINE => at,
            _ => {
                if let Some(at) = current {
                    rows[at].ended_at = timestamp_millis(&event.created_at);
                    rows[at].outcome = Some("replaced".into());
                }
                let mut row = Building::new(event, "live_alert_stream", supervisor.to_owned());
                row.records.clear();
                row.detail = json!({"baseline": kind == LIVE_ALERT_BASELINE});
                open.insert(supervisor.to_owned(), rows.len());
                rows.push(row);
                rows.len() - 1
            }
        };
        if let Some(version) = record.version {
            rows[at].detail["version"] = json!(version);
        }
        rows[at].records.push(event.id);
        taken.insert(event.id);
    }
    for (supervisor, at) in open {
        if let Some(end) = supervisor_life_end(all, &supervisor, now_ms) {
            let started = rows[at].started_at.unwrap_or(end.at_ms());
            rows[at].ended_at = Some(end.at_ms().max(started));
            rows[at].outcome = Some(claim_hold::ENDED_SUPERVISOR_GONE.into());
            rows[at].life = Some(supervisor);
        }
    }
}

/// One alert from its `live_alert_started` to the `live_alert_ended` of
/// the same key; an end with no start is an event row.
fn alerts(owned: &[RunEvent], rows: &mut Vec<Building>, taken: &mut HashSet<EventId>) {
    let mut open: BTreeMap<(String, AlertKey), usize> = BTreeMap::new();
    for event in owned {
        let Some(StreamRecord {
            supervisor,
            alert: Some(alert),
            ..
        }) = StreamRecord::of(event)
        else {
            continue;
        };
        let about = alert.run_id.clone().unwrap_or_else(|| alert.kind.clone());
        let key = (supervisor, alert);
        match event.kind.as_str() {
            LIVE_ALERT_STARTED => {
                let mut row = Building::new(event, "live_alert", about);
                row.detail = json!({"alert": event.payload});
                open.insert(key, rows.len());
                rows.push(row);
                taken.insert(event.id);
            }
            LIVE_ALERT_ENDED => {
                if let Some(at) = open.remove(&key) {
                    rows[at].ended_at = timestamp_millis(&event.created_at);
                    rows[at].outcome = Some("ended".into());
                    rows[at].records.push(event.id);
                    taken.insert(event.id);
                }
            }
            _ => {}
        }
    }
}

/// One finding from its first record: every record of its values, the
/// latest values and status, ended while it is settled.
fn finding_rows(owned: &[RunEvent], rows: &mut Vec<Building>, taken: &mut HashSet<EventId>) {
    let mut by_finding: HashMap<i64, usize> = HashMap::new();
    for event in owned {
        if !findings::KINDS.contains(&event.kind.as_str()) {
            continue;
        }
        let (Some(finding), Some(values)) = (
            findings::values_of(event),
            event.payload.get(findings::VALUES),
        ) else {
            continue;
        };
        let id = finding.id.as_i64();
        let at = *by_finding.entry(id).or_insert_with(|| {
            let mut row = Building::new(event, "finding", id.to_string());
            row.records.clear();
            rows.push(row);
            rows.len() - 1
        });
        let row = &mut rows[at];
        row.records.push(event.id);
        row.outcome = Some(finding.status.as_str().to_owned());
        let settled = !finding.status.is_unsettled();
        row.ended_at = settled
            .then(|| timestamp_millis(&event.created_at))
            .flatten();
        row.detail = json!({"finding": values});
        taken.insert(event.id);
    }
}

/// The integration slot's use: one row per `integrate` attempt, keyed by
/// its `integration_started`, whose records are its run's.
fn landing_slots(all: &[RunEvent], now_ms: i64, built_at: &str) -> Vec<LedgerRow> {
    let (attempts, _) = landing_utilization::spans(all, now_ms);
    let starts = all.iter().filter(|event| {
        event.kind == event_kind::INTEGRATION_STARTED
            && event.run_id.is_some()
            && timestamp_millis(&event.created_at).is_some()
    });
    starts
        .zip(attempts)
        .map(|(start, attempt)| {
            let mut row = LedgerRow::new(
                LedgerKind::Queue,
                start.id.to_string(),
                attempt.task_id,
                built_at,
            );
            row.inputs_from = start.id;
            row.inputs_through.events = if attempt.open {
                all.last().map_or(start.id, |last| last.id)
            } else {
                all.iter()
                    .filter(|event| {
                        event.id >= start.id
                            && timestamp_millis(&event.created_at)
                                .is_some_and(|at| at <= attempt.end)
                    })
                    .map(|event| event.id)
                    .max()
                    .unwrap_or(start.id)
            };
            row.started_at = Some(attempt.start);
            row.ended_at = (!attempt.open).then_some(attempt.end);
            row.outcome =
                (!attempt.open).then(|| if attempt.landed { "landed" } else { "left" }.to_owned());
            row.subject = Some(Subject {
                kind: "landing_slot".into(),
                id: start
                    .run_id
                    .as_ref()
                    .map(|run| run.as_str().to_owned())
                    .unwrap_or_default(),
                ..Subject::default()
            });
            row
        })
        .collect()
}

fn finish(row: Building, by_id: &HashMap<EventId, &RunEvent>, built_at: &str) -> LedgerRow {
    let mut records: Vec<RunEvent> = row
        .records
        .iter()
        .filter_map(|id| by_id.get(id).map(|event| (*event).clone()))
        .collect();
    records.sort_by_key(|event| event.id);
    let mut ledger = LedgerRow::new(
        LedgerKind::Queue,
        row.start.to_string(),
        records.iter().find_map(|event| event.task_id),
        built_at,
    );
    ledger.started_at = row.started_at;
    ledger.ended_at = row.ended_at;
    ledger.outcome = row.outcome;
    ledger.detail = row.detail;
    ledger.subject = Some(Subject {
        kind: row.kind,
        id: row.about,
        ..Subject::default()
    });
    ledger.own(records);
    ledger
}
