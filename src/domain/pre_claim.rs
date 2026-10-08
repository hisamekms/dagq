//! The waits before a claim, by supervisor (ADR-t1662-1 decision 6,
//! docs/design/measurement.md "claimの前"): a supervisor's slots all full
//! (`slots_full_started` / `slots_full_ended`, by class), the queue's hold
//! on the claims (`claim_held` / `claim_resumed`) and a task's deferred
//! claim (`claim_deferred` / `claim_deferral_ended`). [`pre_claim_intervals`]
//! makes their rows from the events alone, each row belonging to the
//! supervisor that recorded its start; the events and what their readers
//! make of them stay as they are, and the holds and deferrals end by the
//! rules `stats` reads them by ([`claim_hold::hold_spans`],
//! [`claim_defer::deferral_spans`]).

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{Value, json};

use super::{
    EventId, EventKind, LeaseToken, RunEvent, TaskId, claim_defer,
    claim_hold::{self, HoldSpan},
    event_kind::{SLOTS_FULL_ENDED, SLOTS_FULL_STARTED},
    light_slots::ClaimRoom,
    marks::utc_text,
    stats::timestamp_millis,
    supervisor_life::{self, LifeEnd},
};

/// The slots a class of tasks needs (ADR-t1591-1): a heavy task needs a
/// slot with the landing queue in it, a light one only the room the
/// landing queue leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotClass {
    Heavy,
    Light,
}

impl SlotClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Heavy => "heavy",
            Self::Light => "light",
        }
    }

    fn of(text: &str) -> Option<Self> {
        match text {
            "heavy" => Some(Self::Heavy),
            "light" => Some(Self::Light),
            _ => None,
        }
    }
}

/// The classes whose slots are all full when the next claim has `room`:
/// `heavy` without a free slot, `light` (only when the supervisor has
/// light changes) without even the light room.
pub fn full_classes(room: ClaimRoom, light: bool) -> BTreeSet<SlotClass> {
    let mut full = BTreeSet::new();
    if room != ClaimRoom::Any {
        full.insert(SlotClass::Heavy);
    }
    if light && room == ClaimRoom::None {
        full.insert(SlotClass::Light);
    }
    full
}

/// What a supervisor whose open intervals are of `open` records when its
/// pass finds the classes of `full` full: `slots_full_started` for each
/// class newly full, `slots_full_ended` for each one free again.
pub fn slots_full_changes(
    open: &BTreeSet<SlotClass>,
    full: &BTreeSet<SlotClass>,
) -> Vec<(EventKind, SlotClass)> {
    full.difference(open)
        .map(|class| (EventKind::SlotsFullStarted, *class))
        .chain(
            open.difference(full)
                .map(|class| (EventKind::SlotsFullEnded, *class)),
        )
        .collect()
}

/// The payload of a `slots_full_*` of the supervisor `token`: the class,
/// the slots in use and the slots it has.
pub fn slots_full_payload(
    token: &LeaseToken,
    class: SlotClass,
    running: usize,
    slots: usize,
) -> Value {
    json!({
        "supervisor": token,
        "class": class.as_str(),
        "running": running,
        "slots": slots,
    })
}

/// The classes the supervisor `token` left open in `events` (any order):
/// what a process that takes over its token goes on from.
pub fn open_slots_full(events: &[RunEvent], token: &str) -> BTreeSet<SlotClass> {
    let mut events: Vec<&RunEvent> = events
        .iter()
        .filter(|event| text(event, "supervisor") == Some(token))
        .collect();
    events.sort_by_key(|event| event.id);
    let mut open = BTreeSet::new();
    for event in events {
        let Some(class) = text(event, "class").and_then(SlotClass::of) else {
            continue;
        };
        match event.kind.as_str() {
            SLOTS_FULL_STARTED => {
                open.insert(class);
            }
            SLOTS_FULL_ENDED => {
                open.remove(&class);
            }
            _ => {}
        }
    }
    open
}

/// The kind of a wait before a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntervalKind {
    SlotsFull,
    ClaimHold,
    ClaimDeferral,
}

/// A slots-full interval ended by its supervisor's pass that found a slot.
pub const ENDED_FREED: &str = "freed";

/// One wait before a claim: the event that started it (`id`), the
/// supervisor that recorded it, its task (a deferral's), its reason (a
/// hold's or a deferral's) or class (slots full), and its end: when, how
/// and which supervisor recorded it (none for a silent supervisor's end
/// and for a deferral its `run_claimed` ended, which names no supervisor).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PreClaimInterval {
    pub id: EventId,
    pub kind: IntervalKind,
    pub supervisor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<SlotClass>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub end: Option<String>,
    pub ended_by: Option<String>,
}

/// The waits before a claim in `events` (ascending id) at `now_ms`, by the
/// id of the event that started each. Slots full ends at its supervisor's
/// `slots_full_ended` ([`ENDED_FREED`]) or, without one, at the end of the
/// supervisor's life ([`supervisor_life::supervisor_life_end`],
/// [`claim_hold::ENDED_SUPERVISOR_GONE`]); another supervisor neither
/// starts nor ends it. A hold and a deferral end as `stats` reads them,
/// whichever supervisor recorded the end. Reads no state: the events and
/// `now_ms` are all.
pub fn pre_claim_intervals(events: &[RunEvent], now_ms: i64) -> Vec<PreClaimInterval> {
    let mut rows = slots_full_intervals(events, now_ms);
    rows.extend(
        claim_hold::hold_spans(claim_hold::CLAIMS, events)
            .iter()
            .map(|span| from_span(IntervalKind::ClaimHold, span)),
    );
    rows.extend(
        claim_defer::deferral_spans(events)
            .iter()
            .map(|span| from_span(IntervalKind::ClaimDeferral, span)),
    );
    rows.sort_by_key(|row| row.id);
    rows
}

fn from_span(kind: IntervalKind, span: &HoldSpan<'_>) -> PreClaimInterval {
    PreClaimInterval {
        id: span.start.id,
        kind,
        supervisor: text(span.start, "supervisor").map(str::to_owned),
        task_id: (kind == IntervalKind::ClaimDeferral)
            .then_some(span.start.task_id)
            .flatten(),
        reason: text(span.start, "reason").map(str::to_owned),
        class: None,
        started_at: span.start.created_at.clone(),
        ended_at: span.end.as_ref().map(|end| end.event.created_at.clone()),
        end: span.end.as_ref().map(|end| end.why.clone()),
        ended_by: span
            .end
            .as_ref()
            .and_then(|end| text(end.event, "supervisor"))
            .map(str::to_owned),
    }
}

fn slots_full_intervals(events: &[RunEvent], now_ms: i64) -> Vec<PreClaimInterval> {
    let mut rows = Vec::new();
    let mut open: BTreeMap<(String, SlotClass), PreClaimInterval> = BTreeMap::new();
    for event in events {
        let (Some(supervisor), Some(class)) = (
            text(event, "supervisor"),
            text(event, "class").and_then(SlotClass::of),
        ) else {
            continue;
        };
        let key = (supervisor.to_owned(), class);
        match event.kind.as_str() {
            // One already open goes on (a process that took over the
            // token and found it open records no new start).
            SLOTS_FULL_STARTED => {
                open.entry(key).or_insert_with(|| PreClaimInterval {
                    id: event.id,
                    kind: IntervalKind::SlotsFull,
                    supervisor: Some(supervisor.to_owned()),
                    task_id: None,
                    reason: None,
                    class: Some(class),
                    started_at: event.created_at.clone(),
                    ended_at: None,
                    end: None,
                    ended_by: None,
                });
            }
            SLOTS_FULL_ENDED => {
                if let Some(mut row) = open.remove(&key) {
                    row.ended_at = Some(event.created_at.clone());
                    row.end = Some(ENDED_FREED.to_owned());
                    row.ended_by = Some(supervisor.to_owned());
                    rows.push(row);
                }
            }
            _ => {}
        }
    }
    for ((supervisor, _), mut row) in open {
        if let Some(life) = supervisor_life::supervisor_life_end(events, &supervisor, now_ms) {
            let started = timestamp_millis(&row.started_at).unwrap_or(life.at_ms());
            row.ended_at = Some(utc_text(life.at_ms().max(started)));
            row.end = Some(claim_hold::ENDED_SUPERVISOR_GONE.to_owned());
            row.ended_by = match life {
                LifeEnd::Stopped { .. } => Some(supervisor),
                LifeEnd::Silent { .. } => None,
            };
        }
        rows.push(row);
    }
    rows
}

fn text<'e>(event: &'e RunEvent, key: &str) -> Option<&'e str> {
    event.payload.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::claim_defer::claim_deferrals;
    use crate::domain::claim_hold::claim_holds;
    use crate::domain::supervisor_life::SUPERVISOR_ALIVE_INTERVAL_SECS;

    const MIN: i64 = 60_000;

    fn event(id: i64, task: Option<i64>, kind: &str, payload: Value, minute: i64) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: task.map(TaskId::new),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: utc_text(minute * MIN),
            actor: None,
        }
    }

    fn full(id: i64, kind: &str, supervisor: &str, class: &str, minute: i64) -> RunEvent {
        event(
            id,
            None,
            kind,
            json!({"supervisor": supervisor, "class": class, "running": 2, "slots": 2}),
            minute,
        )
    }

    fn alive(id: i64, supervisor: &str, minute: i64) -> RunEvent {
        event(
            id,
            None,
            "supervisor_alive",
            json!({"supervisor": supervisor}),
            minute,
        )
    }

    /// (kind, supervisor, class or reason, started, ended, end, ended_by)
    type Row = (
        IntervalKind,
        String,
        String,
        i64,
        Option<i64>,
        Option<String>,
        Option<String>,
    );

    fn rows(rows: &[PreClaimInterval]) -> Vec<Row> {
        rows.iter()
            .map(|row| {
                (
                    row.kind,
                    row.supervisor.clone().unwrap_or_default(),
                    row.class
                        .map(|class| class.as_str().to_owned())
                        .or_else(|| row.reason.clone())
                        .unwrap_or_default(),
                    timestamp_millis(&row.started_at).unwrap() / MIN,
                    row.ended_at
                        .as_deref()
                        .map(|at| timestamp_millis(at).unwrap() / MIN),
                    row.end.clone(),
                    row.ended_by.clone(),
                )
            })
            .collect()
    }

    fn some(text: &str) -> Option<String> {
        Some(text.to_owned())
    }

    #[test]
    fn the_full_classes_follow_the_claim_room() {
        use SlotClass::{Heavy, Light};
        assert!(full_classes(ClaimRoom::Any, true).is_empty());
        assert_eq!(
            full_classes(ClaimRoom::LightOnly, true),
            BTreeSet::from([Heavy])
        );
        assert_eq!(
            full_classes(ClaimRoom::None, true),
            BTreeSet::from([Heavy, Light])
        );
        // Without light changes there is no light class.
        assert_eq!(
            full_classes(ClaimRoom::None, false),
            BTreeSet::from([Heavy])
        );
        let open = BTreeSet::from([Heavy]);
        assert_eq!(
            slots_full_changes(&open, &BTreeSet::from([Heavy, Light])),
            [(EventKind::SlotsFullStarted, Light)]
        );
        assert_eq!(
            slots_full_changes(&open, &BTreeSet::new()),
            [(EventKind::SlotsFullEnded, Heavy)]
        );
        assert!(slots_full_changes(&open, &open).is_empty());
        let payload = slots_full_payload(&LeaseToken::new("s"), Light, 3, 3);
        assert_eq!(
            payload,
            json!({"supervisor": "s", "class": "light", "running": 3, "slots": 3})
        );
    }

    /// A process that takes the token over goes on from the classes its
    /// token left open; another supervisor's records are not its.
    #[test]
    fn a_token_goes_on_from_the_classes_it_left_open() {
        let events = [
            full(1, SLOTS_FULL_STARTED, "s", "heavy", 0),
            full(2, SLOTS_FULL_STARTED, "s", "light", 1),
            full(3, SLOTS_FULL_ENDED, "s", "light", 2),
            full(4, SLOTS_FULL_STARTED, "t", "light", 3),
        ];
        assert_eq!(
            open_slots_full(&events, "s"),
            BTreeSet::from([SlotClass::Heavy])
        );
        assert_eq!(
            open_slots_full(&events, "t"),
            BTreeSet::from([SlotClass::Light])
        );
    }

    /// One supervisor's classes open and close apart; a second supervisor
    /// running beside it whose slots never fill has no row, and a start
    /// repeated while open goes on with the first.
    #[test]
    fn slots_full_is_its_supervisors_by_class() {
        let events = [
            alive(1, "t", 0),
            full(2, SLOTS_FULL_STARTED, "s", "heavy", 1),
            full(3, SLOTS_FULL_STARTED, "s", "light", 2),
            full(4, SLOTS_FULL_ENDED, "s", "light", 4),
            full(5, SLOTS_FULL_STARTED, "s", "heavy", 5),
            full(6, SLOTS_FULL_ENDED, "s", "heavy", 6),
            alive(7, "t", 5),
            alive(8, "s", 6),
        ];
        let rows = rows(&pre_claim_intervals(&events, 7 * MIN));
        assert_eq!(
            rows,
            [
                (
                    IntervalKind::SlotsFull,
                    "s".to_owned(),
                    "heavy".to_owned(),
                    1,
                    Some(6),
                    some(ENDED_FREED),
                    some("s")
                ),
                (
                    IntervalKind::SlotsFull,
                    "s".to_owned(),
                    "light".to_owned(),
                    2,
                    Some(4),
                    some(ENDED_FREED),
                    some("s")
                ),
            ]
        );
    }

    /// (i) A supervisor that died with its slots full and no stop recorded
    /// ends silent at its last evidence, and at its last heartbeat once the
    /// prune's stop comes; (ii) one that stopped ends at its stop; (iii)
    /// one alive that records nothing else stays open. Another
    /// supervisor's start does not end it.
    #[test]
    fn slots_full_without_its_end_closes_with_its_supervisors_life() {
        use crate::domain::HEARTBEAT_TIMEOUT_SECS;
        let silent_after = (SUPERVISOR_ALIVE_INTERVAL_SECS + HEARTBEAT_TIMEOUT_SECS) * 1000;
        // (i)
        let mut died = vec![
            event(1, None, "supervisor_started", json!({"supervisor": "s"}), 0),
            full(2, SLOTS_FULL_STARTED, "s", "heavy", 1),
            alive(3, "s", 5),
            event(4, None, "supervisor_started", json!({"supervisor": "t"}), 6),
        ];
        let open = rows(&pre_claim_intervals(&died, 5 * MIN + silent_after));
        assert_eq!(open[0].4, None, "{open:?}");
        let silent = rows(&pre_claim_intervals(&died, 5 * MIN + silent_after + 1));
        assert_eq!(
            silent[0],
            (
                IntervalKind::SlotsFull,
                "s".to_owned(),
                "heavy".to_owned(),
                1,
                Some(5),
                some(claim_hold::ENDED_SUPERVISOR_GONE),
                None
            )
        );
        died.push(event(
            5,
            None,
            "supervisor_stopped",
            json!({"supervisor": "s", "outcome": "pruned", "last_heartbeat_at": 7 * 60}),
            20,
        ));
        let pruned = rows(&pre_claim_intervals(&died, 21 * MIN));
        assert_eq!(pruned[0].4, Some(7));
        assert_eq!(pruned[0].6, some("s"));

        // (ii)
        let stopped = [
            full(1, SLOTS_FULL_STARTED, "s", "heavy", 1),
            event(
                2,
                None,
                "supervisor_stopped",
                json!({"supervisor": "s", "outcome": "stopped"}),
                3,
            ),
        ];
        let stopped = rows(&pre_claim_intervals(&stopped, 4 * MIN));
        assert_eq!(stopped[0].4, Some(3));
        assert_eq!(stopped[0].5, some(claim_hold::ENDED_SUPERVISOR_GONE));

        // (iii)
        let mut alive_only = vec![full(1, SLOTS_FULL_STARTED, "s", "heavy", 0)];
        alive_only.extend((1..20).map(|n| alive(n + 1, "s", n * 5)));
        let open = rows(&pre_claim_intervals(&alive_only, 96 * MIN));
        assert_eq!((open[0].4, open[0].5.clone()), (None, None));
    }

    /// A hold belongs to the supervisor that recorded it: another's
    /// `claim_resumed` ends it and is its `ended_by`, the next hold
    /// replaces it, and its own stop ends it.
    #[test]
    fn a_hold_ends_by_any_supervisor_and_keeps_its_own() {
        let held = |id: i64, by: &str, reason: &str, minute: i64| {
            event(
                id,
                None,
                "claim_held",
                json!({"supervisor": by, "reason": reason}),
                minute,
            )
        };
        let events = [
            held(1, "s", "load", 0),
            event(
                2,
                None,
                "claim_resumed",
                json!({"supervisor": "t", "reason": "load"}),
                2,
            ),
            held(3, "s", "load", 3),
            held(4, "t", "disk", 4),
            event(5, None, "supervisor_stopped", json!({"supervisor": "s"}), 5),
            event(6, None, "supervisor_stopped", json!({"supervisor": "t"}), 6),
        ];
        let rows = rows(&pre_claim_intervals(&events, 10 * MIN));
        let hold = |by: &str, reason: &str, start, end, why: &str, ended_by: &str| {
            (
                IntervalKind::ClaimHold,
                by.to_owned(),
                reason.to_owned(),
                start,
                Some(end),
                some(why),
                some(ended_by),
            )
        };
        assert_eq!(
            rows,
            [
                hold("s", "load", 0, 2, claim_hold::ENDED_RESUMED, "t"),
                hold("s", "load", 3, 4, claim_hold::ENDED_REPLACED, "t"),
                hold("t", "disk", 4, 6, claim_hold::ENDED_SUPERVISOR_GONE, "t"),
            ]
        );
    }

    /// A deferral ends at its `claim_deferral_ended` (its `why`), at its
    /// task's `run_claimed` or at its task's next deferral, whichever
    /// supervisor recorded it.
    #[test]
    fn a_deferral_ends_three_ways() {
        let deferred = |id: i64, task: i64, minute: i64| {
            event(
                id,
                Some(task),
                "claim_deferred",
                json!({"supervisor": "s", "reason": "hot_files", "files": ["a"]}),
                minute,
            )
        };
        let events = [
            deferred(1, 10, 0),
            deferred(2, 11, 0),
            deferred(3, 12, 0),
            event(
                4,
                Some(10),
                "claim_deferral_ended",
                json!({"supervisor": "t", "why": "landed"}),
                2,
            ),
            event(5, Some(11), "run_claimed", json!({}), 3),
            deferred(6, 12, 4),
        ];
        let intervals = pre_claim_intervals(&events, 10 * MIN);
        let ends: Vec<(Option<TaskId>, Option<String>, Option<String>)> = intervals
            .iter()
            .map(|row| (row.task_id, row.end.clone(), row.ended_by.clone()))
            .collect();
        assert_eq!(
            ends,
            [
                (Some(TaskId::new(10)), some("landed"), some("t")),
                (
                    Some(TaskId::new(11)),
                    some(claim_defer::ENDED_CLAIMED),
                    None
                ),
                (
                    Some(TaskId::new(12)),
                    some(claim_defer::ENDED_SUPERSEDED),
                    some("s")
                ),
                (Some(TaskId::new(12)), None, None),
            ]
        );
        assert!(
            intervals
                .iter()
                .all(|row| row.kind == IntervalKind::ClaimDeferral)
        );
    }

    /// The rows' ends are the ends `stats` counts: the holds and the
    /// deferrals add up to the same seconds by reason and by end.
    #[test]
    fn the_rows_end_as_the_stats_count() {
        let events = [
            event(
                1,
                None,
                "claim_held",
                json!({"supervisor": "s", "reason": "load"}),
                0,
            ),
            event(
                2,
                Some(7),
                "claim_deferred",
                json!({"supervisor": "s", "files": ["a"]}),
                1,
            ),
            event(
                3,
                None,
                "claim_resumed",
                json!({"supervisor": "t", "reason": "load"}),
                3,
            ),
            event(4, Some(7), "run_claimed", json!({}), 5),
            event(
                5,
                None,
                "claim_held",
                json!({"supervisor": "t", "reason": "disk"}),
                6,
            ),
        ];
        let end_ms = 10 * MIN;
        let holds = claim_holds(&events, EventId::new(0), EventId::new(9), end_ms, |_| true);
        let deferrals =
            claim_deferrals(&events, EventId::new(0), EventId::new(9), end_ms, |_| true);
        let rows = pre_claim_intervals(&events, end_ms);
        let secs = |kind: IntervalKind| -> i64 {
            rows.iter()
                .filter(|row| row.kind == kind)
                .map(|row| {
                    let end = row
                        .ended_at
                        .as_deref()
                        .and_then(timestamp_millis)
                        .unwrap_or(end_ms);
                    (end - timestamp_millis(&row.started_at).unwrap()) / 1000
                })
                .sum()
        };
        assert_eq!((holds.count, holds.secs), (2, 3 * 60 + 4 * 60));
        assert_eq!(secs(IntervalKind::ClaimHold), holds.secs);
        assert_eq!(deferrals.by_end["claimed"].secs, 4 * 60);
        assert_eq!(secs(IntervalKind::ClaimDeferral), deferrals.secs);
        assert_eq!(holds.held.unwrap().reason, "disk");
    }
}
