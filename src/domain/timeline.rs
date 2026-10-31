//! `timeline RUN` (ADR-0044 decision 22): the gaps in a run's events and
//! why the run was waiting through each. No LLM and no run directory: the
//! reason of a gap is the phase the run was in where it starts, from the
//! one fold of the run's events into intervals
//! ([`crate::domain::ledger::fold::fold_run`]: the phases the supervisor
//! recorded as `run_phase_changed`, which the ledger, `status` and the
//! forecast read too), with its tags (`blocker`, `holds`) and the kind of
//! its attempt (`phase`: `first`, `revise`, `conflict` or `resume`).
//!
//! - A phase's name (`worker`, `review`, `waiting`, `landing_answer`,
//!   `held`, ...): the interval the gap starts in. `held` is the run's
//!   wait for its task's hold (ADR-t1879-1), from its record to the next.
//! - `unattributed`: time no record accounts for (from the claim to a
//!   first record that came after it, a record without tags it can read).
//! - `ended`: after its life ended (its push, a failure, a cancel).
//! - `legacy`: every gap of a run with no `run_phase_changed` (one from
//!   before the phases were recorded), which is not folded: its events are
//!   listed as they are, and its gaps are not given a reason again.
//!
//! A gap in a phase a person decides (blocker `human`) lists the asks open
//! on the run that hold it then (every kind but the observer's `blocked`
//! and the planner's `planner_question`).
//!
//! The events `timeline` shows also hold the steps of the automatic update
//! (`update_*`, queue events) in the run's span ([`merged`]); the gaps are
//! the run's own.
//!
//! Next to the gaps, `commands` lists the heavy commands (e2e, llvm-cov,
//! test, build/clippy, chains) the run's sessions ran, from the `work` their
//! `session_closed` recorded (task 514).
use crate::domain::EventKind;
use crate::domain::run::{AskOpened, restore_payload};
use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::Value;

use super::ledger::fold::{Interval, RunLife, fold_run};
use super::run_phase::{AttemptKind, Blocker, Holds};
use super::{AskKind, EventId, RunEvent, stats::timestamp_millis};

/// The run's moves to the other provider (`provider_switched`,
/// ADR-t813-2), oldest first: each with its event and time.
pub fn provider_switches(events: &[RunEvent]) -> Vec<Value> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::ProviderSwitched)
        .map(|e| {
            let mut switch = e.payload.clone();
            if let Some(object) = switch.as_object_mut() {
                object.insert("event_id".to_owned(), serde_json::json!(e.id));
                object.insert("at".to_owned(), serde_json::json!(e.created_at));
            }
            switch
        })
        .collect()
}

/// The steps of the replanning of the run (docs/plans/task-replanning.md),
/// oldest first: the operations opened on it (`replan_requested`, with
/// their origin), their diagnosis jobs and what the runtime made of each
/// (`replan_diagnosis_started`, `replan_diagnosed` with its outcome,
/// `replan_diagnosis_failed`), a person's resolution of one held
/// `blocked` (`replan_resolved`, with the choice and who made it), and
/// their moves (`replan_status_changed`), each with its event, time and
/// operation.
pub fn replan_steps(events: &[RunEvent]) -> Vec<Value> {
    const FIELDS: [&str; 11] = [
        "origin",
        "attempt",
        "outcome",
        "why",
        "ask_id",
        "resolution",
        "by",
        "from",
        "to",
        "reason",
        "error",
    ];
    events
        .iter()
        .filter(|e| {
            e.kind.is_in(&[
                EventKind::ReplanRequested,
                EventKind::ReplanDiagnosisStarted,
                EventKind::ReplanDiagnosed,
                EventKind::ReplanDiagnosisFailed,
                EventKind::ReplanResolved,
                EventKind::ReplanStatusChanged,
            ])
        })
        .map(|e| {
            let mut step = serde_json::Map::new();
            step.insert("event_id".into(), serde_json::json!(e.id));
            step.insert("at".into(), serde_json::json!(e.created_at));
            step.insert("kind".into(), serde_json::json!(e.kind.as_str()));
            step.insert("operation_id".into(), e.payload["operation_id"].clone());
            for field in FIELDS {
                if let Some(value) = e.payload.get(field).filter(|v| !v.is_null()) {
                    step.insert(field.into(), value.clone());
                }
            }
            Value::Object(step)
        })
        .collect()
}

/// The default shortest gap `timeline` reports, in seconds.
pub const DEFAULT_GAP_SECS: i64 = 300;

/// One gap between two events of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Gap {
    /// The event the gap starts after.
    pub after_event: EventId,
    /// The event that ends it; `None` for the gap up to now of a run that is
    /// not finished.
    pub before_event: Option<EventId>,
    pub from: String,
    /// `None` with `before_event`.
    pub until: Option<String>,
    pub secs: i64,
    /// The phase the run was in where the gap starts, `unattributed`,
    /// `ended` or `legacy` (the module's rules).
    pub reason: String,
    /// The kind of the attempt of that phase: `first`, `revise`,
    /// `conflict` or `resume`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<&'static str>,
    /// For a phase a person decides: the open asks that hold the run.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ask_ids: Vec<i64>,
    /// The phase's tags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocker: Option<Blocker>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holds: Option<Holds>,
}

/// The reason of a gap of a run that is not folded.
pub const LEGACY: &str = "legacy";
/// The reason of a gap after the run's life ended.
pub const ENDED: &str = "ended";

/// The asks open on the run that hold it, after the events up to some
/// point.
#[derive(Debug, Default)]
struct OpenAsks(BTreeSet<i64>);

impl OpenAsks {
    fn apply(&mut self, event: &RunEvent) {
        let payload = &event.payload;
        match event.kind.known() {
            Some(EventKind::AskOpened) if holds_the_run(&restore_payload(payload)) => {
                if let Some(id) = ask_id(payload) {
                    self.0.insert(id);
                }
            }
            Some(EventKind::AskAnswered) => {
                if let Some(id) = ask_id(payload) {
                    self.0.remove(&id);
                }
            }
            _ => {}
        }
    }
}

/// The interval of `life` a gap starting at unix milliseconds `at` starts
/// in: the last that starts by then and has not ended (a later one that
/// starts at the same time wins), `None` after the last one or before the
/// first.
fn interval_at(life: &RunLife, at: i64) -> Option<&Interval> {
    life.intervals
        .iter()
        .rev()
        .find(|interval| interval.start <= at && (at < interval.end || interval.start == at))
}

/// The name of an attempt's kind.
fn attempt_name(kind: AttemptKind) -> &'static str {
    match kind {
        AttemptKind::First => "first",
        AttemptKind::Revise => "revise",
        AttemptKind::Conflict => "conflict",
        AttemptKind::Resume => "resume",
    }
}

fn ask_id(payload: &Value) -> Option<i64> {
    payload
        .get("ask_id")
        .or_else(|| payload.get("id"))
        .and_then(Value::as_i64)
}

/// Whether an ask holds the run it is opened on: the observer's `blocked`
/// and the planner's question do not.
fn holds_the_run(opened: &AskOpened<'_>) -> bool {
    !matches!(
        opened.kind,
        Some(kind) if kind == AskKind::Blocked.as_str() || kind == AskKind::PlannerQuestion.as_str()
    )
}

/// A run's `events` with the queue's `others` (the steps of the automatic
/// update in its span) among them, by id; an event in both shows once.
pub fn merged(events: &[RunEvent], others: &[RunEvent]) -> Vec<RunEvent> {
    let mut all: Vec<RunEvent> = events.iter().chain(others).cloned().collect();
    all.sort_by_key(|event| event.id);
    all.dedup_by_key(|event| event.id);
    all
}

/// The gaps of at least `min_secs` between consecutive `events` of one run
/// (ascending id), with their reasons from the run's fold. With `now_ms`,
/// the run is not finished and the time from its last event to now is a
/// gap too.
pub fn gaps(events: &[RunEvent], min_secs: i64, now_ms: Option<i64>) -> Vec<Gap> {
    let last_ms = events
        .iter()
        .rev()
        .find_map(|event| timestamp_millis(&event.created_at));
    let recorded = events
        .iter()
        .any(|event| event.kind == EventKind::RunPhaseChanged);
    let life = recorded
        .then(|| fold_run(events, &[], now_ms.or(last_ms).unwrap_or(0)))
        .flatten();
    let mut asks = OpenAsks::default();
    let mut found = Vec::new();
    for (i, event) in events.iter().enumerate() {
        asks.apply(event);
        let next = events.get(i + 1);
        let Some(start) = timestamp_millis(&event.created_at) else {
            continue;
        };
        let end = match next {
            Some(next) => timestamp_millis(&next.created_at),
            None => now_ms,
        };
        let Some(end) = end else { continue };
        let secs = (end - start) / 1000;
        if secs < min_secs.max(1) {
            continue;
        }
        let mut gap = Gap {
            after_event: event.id,
            before_event: next.map(|e| e.id),
            from: event.created_at.clone(),
            until: next.map(|e| e.created_at.clone()),
            secs,
            reason: LEGACY.to_owned(),
            phase: None,
            ask_ids: Vec::new(),
            blocker: None,
            holds: None,
        };
        if let Some(life) = &life {
            match interval_at(life, start) {
                Some(interval) => {
                    gap.reason = interval.phase.clone();
                    gap.phase = interval.attempt.map(|attempt| attempt_name(attempt.kind));
                    gap.blocker = interval.blocker;
                    gap.holds = interval.holds;
                    if interval.blocker == Some(Blocker::Human) {
                        gap.ask_ids = asks.0.iter().copied().collect();
                    }
                }
                None if life.end.is_some_and(|ended| start >= ended) => {
                    gap.reason = ENDED.to_owned();
                }
                None => gap.reason = super::ledger::fold::UNATTRIBUTED.to_owned(),
            }
        }
        found.push(gap);
    }
    found
}

/// A heavy command a run's session ran, as `timeline` lists it: its
/// category only, not the command line (the run directory's
/// `worktime.jsonl` has that). A span whose breakdown was not recorded (no
/// transcript read, a Codex span with `work_unavailable`) gives no row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HeavyCommand {
    /// The span's kind: `worker`, `resume` or `revise`.
    pub session: String,
    /// The `session_closed` that recorded it.
    pub event: EventId,
    pub category: String,
    pub from: String,
    pub until: String,
    pub secs: i64,
    pub background: bool,
    /// Whether its end was seen; otherwise it is cut at the session's end.
    pub finished: bool,
    /// `None` when its outcome is unknown.
    pub failed: Option<bool>,
}

/// The heavy commands the run's sessions ran, by start.
pub fn heavy_commands(events: &[RunEvent]) -> Vec<HeavyCommand> {
    let mut commands: Vec<HeavyCommand> = events
        .iter()
        .filter(|event| event.kind == EventKind::SessionClosed)
        .flat_map(|event| {
            let heavy = event.payload["work"]["heavy"].as_array().cloned();
            heavy.into_iter().flatten().filter_map(move |row| {
                let text = |key: &str| row[key].as_str().map(str::to_owned);
                Some(HeavyCommand {
                    session: event.payload["kind"].as_str()?.to_owned(),
                    event: event.id,
                    category: text("category")?,
                    from: text("start")?,
                    until: text("end")?,
                    secs: row["secs"].as_i64()?,
                    background: row["background"].as_bool().unwrap_or(false),
                    finished: row["finished"].as_bool().unwrap_or(true),
                    failed: row["failed"].as_bool(),
                })
            })
        })
        .collect();
    commands.sort_by(|a, b| a.from.cmp(&b.from));
    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{RunId, TaskId};
    use serde_json::json;

    fn events(list: &[(&str, Value, &str)]) -> Vec<RunEvent> {
        list.iter()
            .enumerate()
            .map(|(i, (kind, payload, at))| RunEvent {
                id: EventId::new(i as i64 + 1),
                task_id: Some(TaskId::new(1)),
                goal_id: None,
                run_id: Some(RunId::new("r").unwrap()),
                kind: (*kind).into(),
                payload: payload.clone(),
                created_at: format!("2026-09-24T{at}Z"),
                actor: None,
            })
            .collect()
    }

    fn reasons(gaps: &[Gap]) -> Vec<(i64, String)> {
        gaps.iter()
            .map(|gap| (gap.after_event.as_i64(), gap.reason.clone()))
            .collect()
    }

    #[test]
    fn the_queue_s_update_steps_show_among_the_run_s_events_by_id() {
        let run = events(&[
            ("run_claimed", json!({}), "00:00:00.000"),
            ("agent_started", json!({}), "00:01:00.000"),
        ]);
        let mut update = run[0].clone();
        update.id = EventId::new(2);
        update.run_id = None;
        update.task_id = None;
        update.kind = "update_installed".into();
        let mut run = run;
        run[1].id = EventId::new(3);
        let all = merged(&run, &[update.clone(), run[0].clone()]);
        let ids: Vec<i64> = all.iter().map(|event| event.id.as_i64()).collect();
        assert_eq!(ids, [1, 2, 3]);
        assert_eq!(all[1].kind, EventKind::UpdateInstalled);
        assert_eq!(merged(&run, &[]).len(), 2);
    }

    /// The replanning's steps of the run, each with its operation and what
    /// its event says of it; other events are left out.
    #[test]
    fn the_replanning_steps_of_a_run_are_listed_in_order() {
        let run = events(&[
            (
                "replan_requested",
                json!({"operation_id": 4, "origin": "runtime", "created": true}),
                "10:00:00.000",
            ),
            (
                "turn_started",
                json!({"what": "revise request"}),
                "10:00:01.000",
            ),
            (
                "replan_diagnosis_started",
                json!({"operation_id": 4, "attempt": 1}),
                "10:00:02.000",
            ),
            (
                "replan_diagnosed",
                json!({"operation_id": 4, "attempt": 1, "outcome": "ask", "why": "low_confidence",
                       "ask_id": 9, "verdict": {"choice": "replan"}}),
                "10:00:03.000",
            ),
            (
                "replan_status_changed",
                json!({"operation_id": 4, "from": "diagnosing", "to": "pause_requested",
                       "reason": "a person chose replan in ask 9"}),
                "10:00:04.000",
            ),
        ]);
        let steps = replan_steps(&run);
        assert_eq!(steps.len(), 4);
        assert_eq!(
            steps[0],
            json!({"event_id": 1, "at": "2026-09-24T10:00:00.000Z", "kind": "replan_requested",
                   "operation_id": 4, "origin": "runtime"})
        );
        assert_eq!(steps[2]["outcome"], "ask");
        assert_eq!(steps[2]["why"], "low_confidence");
        assert!(steps[2].get("verdict").is_none());
        assert_eq!(steps[3]["to"], "pause_requested");
    }

    /// A run from before the phases were recorded keeps its events, and
    /// every gap of it is `legacy`: not given a reason again.
    #[test]
    fn a_run_with_no_phase_recorded_has_legacy_gaps() {
        let run = events(&[
            ("run_claimed", json!({}), "12:32:30.123"),
            ("agent_started", json!({}), "12:32:32.219"),
            ("receipt_observed", json!({}), "23:07:38.104"),
            (
                "validation_finished",
                json!({"accepted": true}),
                "23:08:00.000",
            ),
            (
                "run_integrated",
                json!({"status": "integrated"}),
                "23:30:00.000",
            ),
        ]);
        assert_eq!(
            reasons(&gaps(&run, 300, None)),
            [(2, LEGACY.to_owned()), (4, LEGACY.to_owned())]
        );
        // Up to now too, while it moves on.
        let now = timestamp_millis("2026-09-24T23:40:00.000Z");
        assert_eq!(gaps(&run, 300, now).last().unwrap().reason, LEGACY);
    }

    /// The reason of a gap is the phase the run recorded where it starts,
    /// with its tags and the kind of its attempt; a revise's gap is the
    /// revise's, and after the end of its life a gap is `ended`.
    #[test]
    fn a_gap_has_the_phase_it_starts_in() {
        use crate::domain::ledger::testkit::{Log, run_id};
        use crate::domain::run_phase::{Attempt, Phase};
        let r = run_id(1);
        let revise = Attempt::of(AttemptKind::Revise, 1);
        let mut log = Log::default();
        log.add(Some(1), Some(&r), "run_claimed", json!({}), 0)
            .phase(1, &r, 0, Phase::Worker, "x")
            .add(Some(1), Some(&r), "receipt_observed", json!({}), 600)
            .phase(1, &r, 600, Phase::Validating, "x")
            .phase(1, &r, 660, Phase::Review, "x")
            .phase_in(1, &r, 1000, Phase::Revise, revise, "x")
            .phase_in(1, &r, 2000, Phase::Validating, revise, "x")
            .phase_in(1, &r, 2010, Phase::Landing, revise, "x")
            .phase_in(1, &r, 2100, Phase::Push { in_slot: true }, revise, "x")
            .phase_in(1, &r, 2110, Phase::Ended, revise, "pushed")
            .add(Some(1), Some(&r), "run_cleaned", json!({}), 3000);
        let found = gaps(&log.events, 60, None);
        let shown: Vec<(&str, Option<&str>)> = found
            .iter()
            .map(|gap| (gap.reason.as_str(), gap.phase))
            .collect();
        assert_eq!(
            shown,
            [
                ("worker", Some("first")),
                ("validating", Some("first")),
                ("review", Some("first")),
                ("revise", Some("revise")),
                ("landing", Some("revise")),
                ("ended", None),
            ]
        );
        assert_eq!(found[3].blocker, Some(Blocker::Ai));
        assert_eq!(found[4].holds, Some(Holds::LandingSlot));
    }

    /// A gap in a phase a person decides lists the asks open on the run
    /// that hold it; the observer's `blocked` does not.
    #[test]
    fn a_gap_waiting_for_a_person_lists_its_open_asks() {
        use crate::domain::ledger::testkit::{Log, run_id};
        use crate::domain::run_phase::Phase;
        let r = run_id(2);
        let mut log = Log::default();
        log.add(Some(1), Some(&r), "run_claimed", json!({}), 0)
            .phase(1, &r, 0, Phase::Worker, "x")
            .add(
                Some(1),
                Some(&r),
                "ask_opened",
                json!({"ask_id": 4, "kind": "approve_landing"}),
                100,
            )
            .add(
                Some(1),
                Some(&r),
                "ask_opened",
                json!({"ask_id": 5, "kind": "blocked"}),
                100,
            )
            .phase(1, &r, 100, Phase::LandingAnswer, "approve_landing")
            .add(
                Some(1),
                Some(&r),
                "ask_answered",
                json!({"ask_id": 4}),
                4000,
            )
            .phase(1, &r, 4000, Phase::LandingQueue, "approved");
        let found = gaps(&log.events, 60, None);
        let waiting = found
            .iter()
            .find(|gap| gap.reason == "landing_answer")
            .unwrap();
        assert_eq!(waiting.ask_ids, [4]);
        assert_eq!(waiting.blocker, Some(Blocker::Human));
        assert!(found[0].ask_ids.is_empty());
    }

    /// The run's wait for its task's hold (ADR-t1879-1) is `held` from its
    /// record to the next one; the gap after `task_held` and before that
    /// record has the phase the run still went on in.
    #[test]
    fn a_holds_wait_is_held_from_its_record_to_its_release() {
        use crate::domain::ledger::testkit::{Log, run_id};
        use crate::domain::run_phase::Phase;
        let r = run_id(3);
        let mut log = Log::default();
        log.add(Some(1), Some(&r), "run_claimed", json!({}), 0)
            .phase(1, &r, 0, Phase::Worker, "x")
            .add(Some(1), None, "task_held", json!({"hold_id": 1}), 600)
            .phase(1, &r, 1860, Phase::Held, "task_held")
            .add(Some(1), None, "task_released", json!({"hold_id": 1}), 7800)
            .phase(1, &r, 7800, Phase::Worker, "task_released");
        let found = gaps(&log.events, 300, None);
        let shown: Vec<(i64, &str)> = found
            .iter()
            .map(|gap| (gap.after_event.as_i64(), gap.reason.as_str()))
            .collect();
        assert_eq!(shown, [(2, "worker"), (3, "worker"), (4, "held")]);
        assert_eq!(found[2].secs, 7800 - 1860);
        assert_eq!(found[2].blocker, Some(Blocker::Human));
    }

    #[test]
    fn heavy_commands_come_from_the_closed_sessions_work() {
        let heavy = |category: &str, start: &str| {
            json!({"category": category, "start": start, "end": "2026-09-24T01:10:00.000Z",
                   "secs": 60, "background": true, "finished": true, "failed": false})
        };
        let run = events(&[
            ("session_opened", json!({"kind": "worker"}), "01:00:00"),
            (
                "session_closed",
                json!({"kind": "worker", "work": {"heavy": [
                    heavy("test", "2026-09-24T01:05:00.000Z"),
                    {"category": "e2e"},
                ]}}),
                "01:20:00",
            ),
            (
                "session_closed",
                json!({"kind": "resume", "work": {"heavy": [heavy("llvm_cov", "2026-09-24T01:01:00.000Z")]}}),
                "01:30:00",
            ),
            ("session_closed", json!({"kind": "review"}), "01:40:00"),
        ]);
        let commands = heavy_commands(&run);
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].category, "llvm_cov");
        assert_eq!(commands[0].session, "resume");
        assert_eq!(commands[1].event, EventId::new(2));
        assert_eq!(commands[1].failed, Some(false));
        assert!(commands[1].background);
    }
}
