//! What a run's landings went through (task 466): how often `integrate`
//! tried it and why each attempt was deferred, which landing on main broke
//! it, and how each resume ended. Derived from `run_events` like the rest
//! of `stats`: a deferral's `main` (or, without one, that of the attempt's
//! `integration_started`) is the commit a landing's `run_integrated`
//! recorded as its `result_commit`, which ties the two runs. A
//! `verification_failed` deferral onto a landing that passed the same
//! command at its own landing does not name it (task 974): the failed tests
//! passed on that main, so it is kept in `rebased_onto` instead.
use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Serialize;
use serde_json::Value;

use super::{RunEvent, RunId, Summary, TaskId, landing::p90, median, payload_status};
use crate::domain::reason::event_code;

/// The deferral codes main moving under a run can cause: a rebase that
/// conflicted, left nothing, or a tree that no longer passes verification.
pub const BREAKING_CODES: [&str; 3] = ["rebase_conflict", "verification_failed", "rebase_empty"];

/// The events that park a run for a resume, whose code is the resume's
/// reason (as the supervisor's `resume_reason` picks them).
const PARKING: [&str; 7] = [
    "integration_deferred",
    "integration_error",
    "evidence_missing",
    "scope_violation",
    "landing_decided",
    "triage_finished",
    "triage_decided",
];

/// The reason of a deferral or resume whose event carries no code.
pub const UNKNOWN: &str = "unknown";

/// A landing on main after which this run's landing was deferred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrokenBy {
    pub task_id: TaskId,
    pub run_id: RunId,
    /// When the landing was recorded (`run_integrated`).
    pub landed_at: String,
    /// The main commit the landing made, which this run was rebased onto.
    pub main: String,
    /// The code of this run's first deferral onto that commit.
    pub code: String,
}

/// A landing this run was rebased onto and failed a verification command
/// that the landing itself passed at its own landing (task 974): not named
/// in `broken_by`, only read as where the run was rebased to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RebasedOnto {
    pub task_id: TaskId,
    pub run_id: RunId,
    pub landed_at: String,
    pub main: String,
    /// The code of this run's first such deferral (`verification_failed`).
    pub code: String,
    /// The tests that deferral names as failed; empty when it names none.
    pub failed_tests: Vec<String>,
}

/// One `resume_started` of a run and how it ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResumeAttempt {
    pub attempt: i64,
    /// The code of the event that parked the run before it (`unknown`
    /// when that event carries none).
    pub reason: String,
    pub started_at: String,
    /// `resume_started` → `resume_finished`; null without a finish.
    pub secs: Option<i64>,
    /// Whether this one attempt resolved the run: its session resolved
    /// (`outcome: resolved`) and the run was not parked for a session
    /// again afterwards. Null without a `resume_finished`.
    pub resolved: Option<bool>,
    /// The session (`model/effort`) before this resume raised it a step
    /// after a failure the task caused (ADR-0079 decision 5), and the one
    /// it opened; both null when it was not raised.
    pub escalated_from: Option<String>,
    pub escalated_to: Option<String>,
}

/// A `revise_requested` that raised the live session a step (ADR-0079
/// decision 5), or would have but could not switch it; one withdrawn by a
/// `revise_unsent` is left out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviseEscalation {
    pub attempt: i64,
    /// `model/effort` before and after the raise.
    pub from: String,
    pub to: String,
    /// Whether the session was switched; false when it went on as it was
    /// (`escalation_skipped`).
    pub switched: bool,
}

/// The landings of one run, as its events and the other runs' landings
/// tell them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Retries {
    /// `integration_started` events.
    pub integrate_attempts: i64,
    /// `integration_deferred` events per code.
    pub deferrals: BTreeMap<String, i64>,
    /// The files the deferrals' rebases conflicted in, sorted.
    pub conflict_files: Vec<String>,
    /// The landings the run was deferred after (a deferral whose code is
    /// in [`BREAKING_CODES`] onto the main commit another run landed), in
    /// order, one per landing. A `verification_failed` deferral onto a
    /// landing that passed the same command does not name it.
    pub broken_by: Vec<BrokenBy>,
    /// The landings the run was rebased onto and failed a verification
    /// command they had passed, in order, one per landing and none that
    /// `broken_by` names.
    pub rebased_onto: Vec<RebasedOnto>,
    /// How many other runs this run's landing broke.
    pub broke_runs: i64,
    pub resume_attempts: Vec<ResumeAttempt>,
    /// The revises that raised the live session, or tried to.
    pub revise_escalations: Vec<ReviseEscalation>,
}

#[derive(Default)]
struct Walk {
    retries: Retries,
    conflicts: BTreeSet<String>,
    /// The main of the current `integrate` attempt.
    main: Option<String>,
    /// Each deferral onto a main.
    deferred: Vec<Deferral>,
    /// The verification commands the current `integrate` attempt passed.
    passed: BTreeSet<String>,
    /// The code of the latest parking event.
    parked: Option<String>,
    /// The latest resume's start (unix milliseconds), while it is open.
    resume_start: Option<i64>,
    /// The resume that resolved, until the run is parked again.
    watching: Option<usize>,
}

/// An `integration_deferred` onto a main: its code, and for a failed
/// verification the command and the tests it named.
struct Deferral {
    main: String,
    code: String,
    command: Option<String>,
    failed_tests: Vec<String>,
}

/// A landing on main: its run, task, time, and the verification commands
/// its landing attempt passed.
struct Landed {
    run_id: RunId,
    task_id: TaskId,
    at: String,
    passed: BTreeSet<String>,
}

fn text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

/// `model/effort` of `value` (an object with both). A Codex run's session
/// names its step by `ladder_model`, the rung the raise moved on (task 892).
fn session_label(value: &Value) -> Option<String> {
    Some(format!(
        "{}/{}",
        value["model"]
            .as_str()
            .or_else(|| value["ladder_model"].as_str())?,
        value["effort"].as_str()?
    ))
}

/// The raise a session's event records: `escalated_from` and the session
/// it opened; `None` without one.
fn escalation(payload: &Value) -> Option<(String, String)> {
    Some((
        session_label(&payload["escalated_from"])?,
        session_label(payload)?,
    ))
}

/// The retries of every run of `events` (ascending id).
pub fn retries(events: &[RunEvent]) -> HashMap<RunId, Retries> {
    let mut walks: HashMap<RunId, Walk> = HashMap::new();
    let mut landings: HashMap<String, Landed> = HashMap::new();
    for event in events {
        let (Some(run_id), Some(task_id)) = (&event.run_id, event.task_id) else {
            continue;
        };
        let walk = walks.entry(run_id.clone()).or_default();
        let at = super::timestamp_millis(&event.created_at);
        let code = || event_code(event).map_or(UNKNOWN, |code| code.as_str());
        let kind = event.kind.as_str();
        if PARKING.contains(&kind) {
            walk.parked = Some(code().to_owned());
        }
        if payload_status(&event.payload) == Some("needs_session")
            && !matches!(kind, "integration_error" | "resume_finished")
            && let Some(index) = walk.watching.take()
        {
            walk.retries.resume_attempts[index].resolved = Some(false);
        }
        match kind {
            "integration_started" => {
                walk.retries.integrate_attempts += 1;
                walk.main = text(&event.payload["main"]);
                walk.passed.clear();
            }
            "verification_command"
                if event.payload["phase"] == "integration" && event.payload["exit_code"] == 0 =>
            {
                walk.passed.extend(text(&event.payload["command"]));
            }
            "integration_deferred" => {
                *walk.retries.deferrals.entry(code().to_owned()).or_default() += 1;
                if let Some(files) = event.payload["conflicts"].as_array() {
                    walk.conflicts.extend(files.iter().filter_map(text));
                }
                if let Some(main) = text(&event.payload["main"]).or_else(|| walk.main.clone()) {
                    walk.deferred.push(Deferral {
                        main,
                        code: code().to_owned(),
                        command: text(&event.payload["command"]),
                        failed_tests: event.payload["failed_tests"]
                            .as_array()
                            .map(|names| names.iter().filter_map(text).collect())
                            .unwrap_or_default(),
                    });
                }
            }
            "run_integrated" => {
                if let Some(commit) =
                    text(&event.payload["result_commit"]).or_else(|| text(&event.payload["commit"]))
                {
                    landings.insert(
                        commit,
                        Landed {
                            run_id: run_id.clone(),
                            task_id,
                            at: event.created_at.clone(),
                            passed: std::mem::take(&mut walk.passed),
                        },
                    );
                }
            }
            "resume_started" => {
                let attempts = &mut walk.retries.resume_attempts;
                let (escalated_from, escalated_to) = escalation(&event.payload).unzip();
                attempts.push(ResumeAttempt {
                    attempt: event.payload["attempt"]
                        .as_i64()
                        .unwrap_or(attempts.len() as i64 + 1),
                    reason: walk.parked.clone().unwrap_or_else(|| UNKNOWN.to_owned()),
                    started_at: event.created_at.clone(),
                    secs: None,
                    resolved: None,
                    escalated_from,
                    escalated_to,
                });
                walk.resume_start = at;
                walk.watching = None;
            }
            "revise_requested" => {
                let payload = &event.payload;
                let raise = escalation(payload)
                    .map(|(from, to)| (from, to, true))
                    .or_else(|| {
                        Some((
                            session_label(payload)?,
                            session_label(&payload["escalation_skipped"])?,
                            false,
                        ))
                    });
                if let Some((from, to, switched)) = raise {
                    walk.retries.revise_escalations.push(ReviseEscalation {
                        attempt: payload["attempt"].as_i64().unwrap_or_default(),
                        from,
                        to,
                        switched,
                    });
                }
            }
            // A withdrawn revise never reached the session.
            "revise_unsent" => {
                let attempt = event.payload["attempt"].as_i64().unwrap_or_default();
                walk.retries
                    .revise_escalations
                    .retain(|revise| revise.attempt != attempt);
            }
            "resume_finished" => {
                let index = walk.retries.resume_attempts.len().checked_sub(1);
                if let (Some(index), Some(start)) = (index, walk.resume_start.take()) {
                    let attempt = &mut walk.retries.resume_attempts[index];
                    attempt.secs = at.map(|at| (at - start).max(0) / 1000);
                    let resolved = event.payload["outcome"] == "resolved";
                    attempt.resolved = Some(resolved);
                    walk.watching = resolved.then_some(index);
                }
            }
            _ => {}
        }
    }
    let mut broke: HashMap<RunId, BTreeSet<RunId>> = HashMap::new();
    for (run_id, walk) in &mut walks {
        walk.retries.conflict_files = std::mem::take(&mut walk.conflicts).into_iter().collect();
        for deferral in &walk.deferred {
            let code = &deferral.code;
            if !BREAKING_CODES.contains(&code.as_str()) {
                continue;
            }
            let Some(landed) = landings.get(&deferral.main) else {
                continue;
            };
            let retries = &mut walk.retries;
            if landed.run_id == *run_id
                || retries
                    .broken_by
                    .iter()
                    .any(|by| by.run_id == landed.run_id)
            {
                continue;
            }
            // The landing passed the command that failed here: the failure
            // is this run's change or its meeting with main, not the landing's.
            let passed = code == "verification_failed"
                && deferral
                    .command
                    .as_ref()
                    .is_some_and(|command| landed.passed.contains(command));
            if passed {
                if !retries
                    .rebased_onto
                    .iter()
                    .any(|onto| onto.run_id == landed.run_id)
                {
                    retries.rebased_onto.push(RebasedOnto {
                        task_id: landed.task_id,
                        run_id: landed.run_id.clone(),
                        landed_at: landed.at.clone(),
                        main: deferral.main.clone(),
                        code: code.clone(),
                        failed_tests: deferral.failed_tests.clone(),
                    });
                }
                continue;
            }
            // Named by a later deferral: named only, not also rebased onto.
            retries
                .rebased_onto
                .retain(|onto| onto.run_id != landed.run_id);
            retries.broken_by.push(BrokenBy {
                task_id: landed.task_id,
                run_id: landed.run_id.clone(),
                landed_at: landed.at.clone(),
                main: deferral.main.clone(),
                code: code.clone(),
            });
            broke
                .entry(landed.run_id.clone())
                .or_default()
                .insert(run_id.clone());
        }
    }
    walks
        .into_iter()
        .map(|(run_id, mut walk)| {
            walk.retries.broke_runs = broke.get(&run_id).map_or(0, |runs| runs.len() as i64);
            (run_id, walk.retries)
        })
        .collect()
}

/// Seconds over a set of resumes: `Summary`'s count, sum and median, the
/// 90th percentile and the maximum.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Durations {
    #[serde(flatten)]
    pub summary: Summary,
    pub p90: Option<i64>,
    pub max: Option<i64>,
}

/// A set of resumes: how many, how many resolved the run in one attempt
/// and how many did not (those without a finish are neither), the share
/// that did among those judged, and their durations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ResumeSummary {
    pub attempts: usize,
    pub resolved: usize,
    pub unresolved: usize,
    /// `resolved` in percent of `resolved + unresolved`, rounded down;
    /// null when neither.
    pub resolved_percent: Option<i64>,
    pub secs: Durations,
}

/// The resumes of a set of runs, together and per reason.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ResumeBreakdown {
    #[serde(flatten)]
    pub all: ResumeSummary,
    pub by_reason: BTreeMap<String, ResumeSummary>,
}

pub(super) fn resume_summary(attempts: &[&ResumeAttempt]) -> ResumeSummary {
    let mut secs: Vec<i64> = attempts.iter().filter_map(|a| a.secs).collect();
    let resolved = attempts.iter().filter(|a| a.resolved == Some(true)).count();
    let unresolved = attempts
        .iter()
        .filter(|a| a.resolved == Some(false))
        .count();
    let judged = resolved + unresolved;
    ResumeSummary {
        attempts: attempts.len(),
        resolved,
        unresolved,
        resolved_percent: (judged > 0).then(|| (resolved * 100 / judged) as i64),
        secs: Durations {
            summary: Summary {
                count: secs.len(),
                total: secs.iter().sum(),
                median: median(&mut secs),
            },
            p90: p90(&mut secs),
            max: secs.iter().copied().max(),
        },
    }
}

/// Aggregate resumes, together and per reason.
pub fn resume_breakdown<'a>(attempts: impl Iterator<Item = &'a ResumeAttempt>) -> ResumeBreakdown {
    let attempts: Vec<&ResumeAttempt> = attempts.collect();
    let mut by_reason: BTreeMap<String, Vec<&ResumeAttempt>> = BTreeMap::new();
    for attempt in &attempts {
        by_reason
            .entry(attempt.reason.clone())
            .or_default()
            .push(attempt);
    }
    ResumeBreakdown {
        all: resume_summary(&attempts),
        by_reason: by_reason
            .into_iter()
            .map(|(reason, attempts)| (reason, resume_summary(&attempts)))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;
    use serde_json::json;

    const A: &str = "11111111-1111-4111-8111-111111111111";
    const B: &str = "22222222-2222-4222-8222-222222222222";
    const C: &str = "33333333-3333-4333-8333-333333333333";

    fn event(id: i64, run: &str, task: i64, kind: &str, payload: Value, secs: i64) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(task)),
            goal_id: None,
            run_id: Some(RunId::new(run).unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: format!("2026-09-26T00:{:02}:{:02}.000Z", secs / 60, secs % 60),
            actor: None,
        }
    }

    #[test]
    fn deferrals_are_tied_to_the_landing_that_broke_them() {
        let events = [
            event(
                1,
                A,
                1,
                "run_integrated",
                json!({"result_commit": "m1"}),
                10,
            ),
            event(2, B, 2, "integration_started", json!({"main": "m1"}), 20),
            event(
                3,
                B,
                2,
                "integration_deferred",
                json!({"code": "rebase_conflict", "main": "m1", "conflicts": ["b.rs", "a.rs"], "status": "needs_session"}),
                21,
            ),
            event(4, B, 2, "resume_started", json!({"attempt": 1}), 30),
            event(
                5,
                B,
                2,
                "resume_finished",
                json!({"outcome": "resolved", "status": "needs_session"}),
                90,
            ),
            // Onto the same main again: the same landing, listed once.
            event(6, B, 2, "integration_started", json!({"main": "m1"}), 91),
            event(
                7,
                B,
                2,
                "integration_deferred",
                json!({"code": "verification_failed", "status": "needs_session"}),
                95,
            ),
            event(8, B, 2, "resume_started", json!({"attempt": 2}), 100),
            event(
                9,
                B,
                2,
                "resume_finished",
                json!({"outcome": "resolved", "status": "needs_session"}),
                200,
            ),
            event(10, B, 2, "run_integrated", json!({"commit": "m2"}), 210),
            // Evidence is not main's doing; a deferral onto its own landing
            // or an unknown main is not tied.
            event(11, C, 3, "integration_started", json!({"main": "m2"}), 220),
            event(
                12,
                C,
                3,
                "integration_deferred",
                json!({"code": "evidence_missing", "status": "needs_session"}),
                221,
            ),
            event(13, C, 3, "integration_started", json!({"main": "mx"}), 222),
            event(
                14,
                C,
                3,
                "integration_deferred",
                json!({"code": "rebase_conflict"}),
                223,
            ),
            event(15, C, 3, "integration_deferred", json!({}), 224),
            event(16, C, 3, "resume_started", json!({}), 230),
            event(
                17,
                C,
                3,
                "resume_finished",
                json!({"outcome": "unresolved"}),
                240,
            ),
            event(18, C, 3, "resume_started", json!({}), 250),
        ];
        let all = retries(&events);
        let a = &all[&RunId::new(A).unwrap()];
        assert_eq!(a.broke_runs, 1);
        assert_eq!(a.integrate_attempts, 0);
        let b = &all[&RunId::new(B).unwrap()];
        assert_eq!(b.integrate_attempts, 2);
        assert_eq!(
            b.deferrals,
            BTreeMap::from([
                ("rebase_conflict".to_owned(), 1),
                ("verification_failed".to_owned(), 1)
            ])
        );
        assert_eq!(b.conflict_files, ["a.rs", "b.rs"]);
        assert_eq!(
            b.broken_by,
            [BrokenBy {
                task_id: TaskId::new(1),
                run_id: RunId::new(A).unwrap(),
                landed_at: "2026-09-26T00:00:10.000Z".to_owned(),
                main: "m1".to_owned(),
                code: "rebase_conflict".to_owned(),
            }]
        );
        assert_eq!(b.broke_runs, 0);
        let resumes: Vec<_> = b
            .resume_attempts
            .iter()
            .map(|r| (r.attempt, r.reason.as_str(), r.secs, r.resolved))
            .collect();
        assert_eq!(
            resumes,
            [
                (1, "rebase_conflict", Some(60), Some(false)),
                (2, "verification_failed", Some(100), Some(true)),
            ]
        );
        let c = &all[&RunId::new(C).unwrap()];
        assert!(c.broken_by.is_empty());
        assert_eq!(c.deferrals["unknown"], 1);
        let resumes: Vec<_> = c
            .resume_attempts
            .iter()
            .map(|r| (r.attempt, r.reason.as_str(), r.secs, r.resolved))
            .collect();
        assert_eq!(
            resumes,
            [
                (1, "unknown", Some(10), Some(false)),
                (2, "unknown", None, None)
            ]
        );

        let breakdown = resume_breakdown(all.values().flat_map(|r| &r.resume_attempts));
        assert_eq!(breakdown.all.attempts, 4);
        assert_eq!((breakdown.all.resolved, breakdown.all.unresolved), (1, 2));
        assert_eq!(breakdown.all.resolved_percent, Some(33));
        assert_eq!(breakdown.all.secs.summary.count, 3);
        assert_eq!(breakdown.all.secs.max, Some(100));
        let conflict = &breakdown.by_reason["rebase_conflict"];
        assert_eq!(conflict.resolved_percent, Some(0));
        assert_eq!(conflict.secs.p90, Some(60));
        let json = serde_json::to_value(&breakdown).unwrap();
        assert_eq!(json["attempts"], 4);
        assert_eq!(
            json["by_reason"]["verification_failed"]["resolved_percent"],
            100
        );
        assert_eq!(json["by_reason"]["unknown"]["secs"]["count"], 1);
        assert_eq!(
            resume_breakdown(std::iter::empty()).all.resolved_percent,
            None
        );
    }

    /// Task 974: a landing first kept as rebased onto (it passed the
    /// failed command) and later named by a conflict onto it is named only.
    #[test]
    fn a_landing_named_later_leaves_rebased_onto() {
        let passed = json!({"phase": "integration", "command": "t", "exit_code": 0});
        let events = [
            event(1, A, 1, "integration_started", json!({"main": "m0"}), 1),
            // A pass of an earlier attempt does not count for the landing.
            event(2, A, 1, "verification_command", passed.clone(), 2),
            event(3, A, 1, "integration_started", json!({"main": "m0"}), 3),
            event(4, A, 1, "verification_command", passed, 4),
            event(5, A, 1, "run_integrated", json!({"result_commit": "m1"}), 5),
            event(6, B, 2, "integration_started", json!({"main": "m1"}), 6),
            event(
                7,
                B,
                2,
                "integration_deferred",
                json!({"code": "verification_failed", "command": "t"}),
                7,
            ),
            event(
                8,
                B,
                2,
                "integration_deferred",
                json!({"code": "rebase_conflict", "main": "m1"}),
                8,
            ),
        ];
        let all = retries(&events);
        let b = &all[&RunId::new(B).unwrap()];
        assert!(b.rebased_onto.is_empty());
        assert_eq!(b.broken_by.len(), 1);
        assert_eq!(b.broken_by[0].code, "rebase_conflict");
        assert_eq!(all[&RunId::new(A).unwrap()].broke_runs, 1);
        // Without the conflict, B only rebased onto A.
        let all = retries(&events[..7]);
        let b = &all[&RunId::new(B).unwrap()];
        assert!(b.broken_by.is_empty());
        assert_eq!(b.rebased_onto[0].run_id, RunId::new(A).unwrap());
        assert!(b.rebased_onto[0].failed_tests.is_empty());
        assert_eq!(all[&RunId::new(A).unwrap()].broke_runs, 0);
    }

    #[test]
    fn raised_resumes_and_revises_are_read() {
        let opus = |effort: &str| json!({"model": "claude-opus-5-5", "effort": effort});
        let events = [
            event(
                1,
                A,
                1,
                "integration_deferred",
                json!({"code": "verification_failed", "status": "needs_session"}),
                10,
            ),
            event(
                2,
                A,
                1,
                "resume_started",
                json!({"attempt": 1, "model": "claude-opus-5-5", "effort": "high",
                       "escalated_from": opus("medium"), "escalation_reason": "verification_failed"}),
                20,
            ),
            event(
                3,
                A,
                1,
                "resume_finished",
                json!({"outcome": "resolved"}),
                30,
            ),
            event(
                4,
                A,
                1,
                "revise_requested",
                json!({"attempt": 1, "model": "claude-opus-5-5", "effort": "xhigh",
                       "escalated_from": opus("high"), "escalation_reason": "revise"}),
                40,
            ),
            event(
                5,
                A,
                1,
                "revise_requested",
                json!({"attempt": 2, "model": "claude-opus-5-5", "effort": "xhigh",
                       "escalation_skipped": {"model": "x", "effort": "y", "why": "no"}}),
                50,
            ),
            // At the top, or before the raises were recorded: nothing.
            event(6, A, 1, "revise_requested", json!({"attempt": 3}), 60),
            // Withdrawn: not a raise.
            event(
                7,
                A,
                1,
                "revise_requested",
                json!({"attempt": 4, "model": "claude-opus-5-5", "effort": "xhigh",
                       "escalated_from": opus("high")}),
                70,
            ),
            event(8, A, 1, "revise_unsent", json!({"attempt": 4}), 71),
        ];
        let all = retries(&events);
        let a = &all[&RunId::new(A).unwrap()];
        let resume = &a.resume_attempts[0];
        assert_eq!(
            (
                resume.escalated_from.as_deref(),
                resume.escalated_to.as_deref(),
                resume.resolved
            ),
            (
                Some("claude-opus-5-5/medium"),
                Some("claude-opus-5-5/high"),
                Some(true)
            )
        );
        assert_eq!(
            a.revise_escalations,
            [
                ReviseEscalation {
                    attempt: 1,
                    from: "claude-opus-5-5/high".into(),
                    to: "claude-opus-5-5/xhigh".into(),
                    switched: true,
                },
                ReviseEscalation {
                    attempt: 2,
                    from: "claude-opus-5-5/xhigh".into(),
                    to: "x/y".into(),
                    switched: false,
                },
            ]
        );
    }

    /// Task 892: a Codex run's raise is named by its step.
    #[test]
    fn a_codex_raise_is_named_by_its_step() {
        let payload = json!({"model": null, "ladder_model": "claude-opus-5-5", "effort": "high",
                             "escalated_from": {"model": null, "ladder_model": "claude-opus-5-5", "effort": "medium"}});
        assert_eq!(
            escalation(&payload),
            Some((
                "claude-opus-5-5/medium".to_owned(),
                "claude-opus-5-5/high".to_owned()
            ))
        );
        assert_eq!(
            session_label(&json!({"model": null, "effort": "high"})),
            None
        );
    }
}
