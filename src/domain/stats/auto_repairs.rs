//! The irregularities the runtime and the recovery job repaired without a
//! person (ADR-0047 decisions 38 and 45, task 362): the `auto_repaired`
//! events of the window by `layer` and `repair`, and per day next to the
//! asks opened that day, so that whether fewer irregularities reach the
//! inbox can be read from one place. Next to them, what the recovery jobs
//! decided (`recovery_finished`, task 557): by verdict, confidence, alert
//! and outcome, and how many were applied or escalated. Derived from
//! `auto_repaired` / `ask_opened` / `recovery_finished` like the rest of
//! `stats`.
use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::{EventId, RunEvent, TaskId, asks::UNKNOWN};

/// The repairs of a window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AutoRepairStats {
    pub count: i64,
    /// Per `layer` (`runtime` / `recovery`), the repairs by `repair`.
    pub by_layer: BTreeMap<String, LayerRepairs>,
    /// The recovery jobs that finished in the window.
    pub recovery_jobs: RecoveryJobs,
    /// Per UTC day (`YYYY-MM-DD`) of the window, the repairs and the asks
    /// opened, side by side.
    pub by_day: BTreeMap<String, DayCounts>,
}

/// The repairs of one layer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LayerRepairs {
    pub count: i64,
    pub by_repair: BTreeMap<String, i64>,
}

/// The `recovery_finished` events of a window: every one is counted, and
/// a key the payload lacks (no `verdict` or `confidence` of a failed or
/// stopped job, no `outcome` of a job whose verdict was carried out) is
/// [`NONE`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RecoveryJobs {
    pub count: i64,
    /// Those whose `applied` names an action.
    pub applied: i64,
    /// Those that opened an ask (`escalated: true`).
    pub escalated: i64,
    /// Per `verdict` (`repair` / `escalate`).
    pub by_verdict: BTreeMap<String, i64>,
    /// Per `confidence` (`high` / `low`).
    pub by_confidence: BTreeMap<String, i64>,
    /// Per `outcome` (`already_asked`, `job_failed`, a stopped job's
    /// reason such as `dialog_cleared`).
    pub by_outcome: BTreeMap<String, i64>,
    /// Per `alert` (`failed`, `interrupted`, `resume_exhausted`,
    /// `stuck_exit`, `prompt_waiting`, `long_background`, ...).
    pub by_alert: BTreeMap<String, AlertJobs>,
}

/// The recovery jobs of one alert.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AlertJobs {
    pub count: i64,
    pub applied: i64,
    pub escalated: i64,
    pub by_verdict: BTreeMap<String, i64>,
}

/// The key of a `recovery_finished` field the payload lacks.
pub const NONE: &str = "none";

/// One day of the window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DayCounts {
    /// The `auto_repaired` events of the day.
    pub auto_repaired: i64,
    /// Those by `repair`.
    pub by_repair: BTreeMap<String, i64>,
    /// The `ask_opened` events of the day: what reached a person.
    pub asks_opened: i64,
    /// Those by why a person was needed (`reason_category`), `unknown` for
    /// an ask opened before the reason was kept.
    pub asks_by_reason: BTreeMap<String, i64>,
    /// The `recovery_finished` events of the day.
    pub recovery_jobs: i64,
    /// Those that were applied.
    pub recovery_applied: i64,
    /// Those that opened an ask.
    pub recovery_escalated: i64,
}

/// Count the `auto_repaired`, `ask_opened` and `recovery_finished` events
/// with `after < id <= upto` whose task `counts` accepts.
pub fn auto_repairs(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> AutoRepairStats {
    let mut stats = AutoRepairStats::default();
    let text = |payload: &Value, key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or(UNKNOWN)
            .to_owned()
    };
    for event in events
        .iter()
        .filter(|event| event.id > after && event.id <= upto && counts(event.task_id))
    {
        let day = || event.created_at.get(..10).unwrap_or(UNKNOWN).to_owned();
        let payload = &event.payload;
        match event.kind.as_str() {
            "auto_repaired" => {
                let repair = text(payload, "repair");
                stats.count += 1;
                let layer = stats.by_layer.entry(text(payload, "layer")).or_default();
                layer.count += 1;
                *layer.by_repair.entry(repair.clone()).or_default() += 1;
                let day = stats.by_day.entry(day()).or_default();
                day.auto_repaired += 1;
                *day.by_repair.entry(repair).or_default() += 1;
            }
            "ask_opened" => {
                let day = stats.by_day.entry(day()).or_default();
                day.asks_opened += 1;
                *day.asks_by_reason
                    .entry(text(payload, "reason_category"))
                    .or_default() += 1;
            }
            "recovery_finished" => {
                let key = |key: &str| {
                    payload
                        .get(key)
                        .and_then(Value::as_str)
                        .unwrap_or(NONE)
                        .to_owned()
                };
                let applied = payload
                    .get("applied")
                    .and_then(Value::as_array)
                    .is_some_and(|applied| !applied.is_empty());
                let escalated = payload.get("escalated").and_then(Value::as_bool) == Some(true);
                let verdict = key("verdict");
                let jobs = &mut stats.recovery_jobs;
                jobs.count += 1;
                jobs.applied += i64::from(applied);
                jobs.escalated += i64::from(escalated);
                *jobs.by_verdict.entry(verdict.clone()).or_default() += 1;
                *jobs.by_confidence.entry(key("confidence")).or_default() += 1;
                *jobs.by_outcome.entry(key("outcome")).or_default() += 1;
                let alert = jobs.by_alert.entry(text(payload, "alert")).or_default();
                alert.count += 1;
                alert.applied += i64::from(applied);
                alert.escalated += i64::from(escalated);
                *alert.by_verdict.entry(verdict).or_default() += 1;
                let day = stats.by_day.entry(day()).or_default();
                day.recovery_jobs += 1;
                day.recovery_applied += i64::from(applied);
                day.recovery_escalated += i64::from(escalated);
            }
            _ => {}
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(id: i64, task_id: Option<i64>, kind: &str, payload: Value, at: &str) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: task_id.map(TaskId::new),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: at.to_owned(),
            actor: None,
        }
    }

    /// Repairs count by layer and repair, and per day next to the asks
    /// opened that day; events outside the window or of a task not
    /// counted are left out.
    #[test]
    fn counts_the_repairs_by_layer_repair_and_day_next_to_the_asks() {
        let day1 = "2026-09-25T10:00:00.000Z";
        let day2 = "2026-09-26T01:00:00.000Z";
        let repaired = |layer: &str, repair: &str| json!({"layer": layer, "repair": repair});
        let events = [
            event(
                1,
                Some(1),
                "auto_repaired",
                repaired("runtime", "dialog_answered"),
                day1,
            ),
            event(
                2,
                Some(1),
                "auto_repaired",
                repaired("runtime", "submit_enter_retry"),
                day1,
            ),
            event(
                3,
                Some(2),
                "auto_repaired",
                repaired("recovery", "stop_processes"),
                day2,
            ),
            event(
                4,
                Some(1),
                "ask_opened",
                json!({"kind": "decide", "reason_category": "recovery_failed"}),
                day1,
            ),
            event(5, None, "ask_opened", json!({"kind": "blocked"}), day2),
            event(6, Some(1), "run_claimed", json!({}), day2),
            event(7, Some(1), "auto_repaired", json!({}), day2),
            event(
                8,
                Some(1),
                "auto_repaired",
                repaired("runtime", "inherit_retry"),
                day2,
            ),
        ];
        let all = auto_repairs(&events, EventId::new(0), EventId::new(7), |_| true);
        assert_eq!(all.count, 4);
        assert_eq!(all.by_layer["runtime"].count, 2);
        assert_eq!(all.by_layer["runtime"].by_repair["submit_enter_retry"], 1);
        assert_eq!(all.by_layer["recovery"].by_repair["stop_processes"], 1);
        assert_eq!(all.by_layer[UNKNOWN].by_repair[UNKNOWN], 1);
        let first = &all.by_day["2026-09-25"];
        assert_eq!((first.auto_repaired, first.asks_opened), (2, 1));
        assert_eq!(first.asks_by_reason["recovery_failed"], 1);
        let second = &all.by_day["2026-09-26"];
        assert_eq!((second.auto_repaired, second.asks_opened), (2, 1));
        assert_eq!(second.asks_by_reason[UNKNOWN], 1);
        assert_eq!(second.by_repair["stop_processes"], 1);

        let task_one = auto_repairs(&events, EventId::new(1), EventId::new(8), |task| {
            task == Some(TaskId::new(1))
        });
        assert_eq!(task_one.count, 3);
        assert!(!task_one.by_layer.contains_key("recovery"));
        assert_eq!(task_one.by_day["2026-09-26"].asks_opened, 0);
        assert_eq!(task_one.by_day["2026-09-26"].by_repair["inherit_retry"], 1);
    }

    /// Every `recovery_finished` is counted by verdict, confidence,
    /// outcome and alert, a missing key as `none`, with the applied and
    /// escalated ones, per day and under the same filter as the repairs.
    #[test]
    fn counts_the_recovery_jobs_by_verdict_confidence_alert_and_outcome() {
        let day1 = "2026-09-25T10:00:00.000Z";
        let day2 = "2026-09-26T01:00:00.000Z";
        let events = [
            // An ended run's repair, applied.
            event(
                1,
                Some(1),
                "recovery_finished",
                json!({"alert": "failed", "attempt": 1, "verdict": "repair",
                       "confidence": "high", "applied": ["retry"], "escalated": false}),
                day1,
            ),
            // A low-confidence repair escalated to a decide ask.
            event(
                2,
                Some(1),
                "recovery_finished",
                json!({"alert": "failed", "attempt": 2, "verdict": "repair",
                       "confidence": "low", "applied": [], "escalated": true, "ask_id": 4}),
                day1,
            ),
            // An escalation of a live alert.
            event(
                3,
                Some(2),
                "recovery_finished",
                json!({"alert": "stuck_exit", "attempt": 1, "verdict": "escalate",
                       "confidence": "high", "applied": [], "escalated": true, "ask_id": 5}),
                day2,
            ),
            // A failed job: no verdict, no confidence.
            event(
                4,
                Some(2),
                "recovery_finished",
                json!({"alert": "resume_exhausted", "attempt": 1, "outcome": "job_failed",
                       "escalated": false, "error": "exit 1"}),
                day2,
            ),
            // An open ask already has a person looking.
            event(
                5,
                Some(1),
                "recovery_finished",
                json!({"alert": "prompt_waiting", "attempt": 1, "verdict": null,
                       "confidence": null, "applied": [], "escalated": false,
                       "outcome": "already_asked"}),
                day2,
            ),
            // A job stopped when the dialog went.
            event(
                6,
                Some(1),
                "recovery_finished",
                json!({"alert": "prompt_waiting", "attempt": 2, "outcome": "dialog_cleared"}),
                day2,
            ),
            // A wait applied with nothing named, and an old payload with
            // no alert.
            event(
                7,
                Some(1),
                "recovery_finished",
                json!({"alert": "interrupted", "verdict": "repair", "confidence": "high",
                       "applied": ["wait"], "escalated": false}),
                day2,
            ),
            event(8, Some(1), "recovery_finished", json!({}), day2),
            // Outside the window.
            event(
                9,
                Some(1),
                "recovery_finished",
                json!({"alert": "failed", "verdict": "repair"}),
                day2,
            ),
        ];
        let all = auto_repairs(&events, EventId::new(0), EventId::new(8), |_| true);
        let jobs = &all.recovery_jobs;
        assert_eq!((jobs.count, jobs.applied, jobs.escalated), (8, 2, 2));
        assert_eq!(
            jobs.by_verdict,
            BTreeMap::from([
                ("repair".to_owned(), 3),
                ("escalate".to_owned(), 1),
                (NONE.to_owned(), 4),
            ])
        );
        assert_eq!(
            jobs.by_confidence,
            BTreeMap::from([
                ("high".to_owned(), 3),
                ("low".to_owned(), 1),
                (NONE.to_owned(), 4),
            ])
        );
        assert_eq!(
            jobs.by_outcome,
            BTreeMap::from([
                (NONE.to_owned(), 5),
                ("job_failed".to_owned(), 1),
                ("already_asked".to_owned(), 1),
                ("dialog_cleared".to_owned(), 1),
            ])
        );
        let failed = &jobs.by_alert["failed"];
        assert_eq!((failed.count, failed.applied, failed.escalated), (2, 1, 1));
        assert_eq!(failed.by_verdict["repair"], 2);
        assert_eq!(jobs.by_alert["stuck_exit"].by_verdict["escalate"], 1);
        assert_eq!(jobs.by_alert["resume_exhausted"].by_verdict[NONE], 1);
        assert_eq!(jobs.by_alert["prompt_waiting"].count, 2);
        assert_eq!(jobs.by_alert["interrupted"].applied, 1);
        assert_eq!(jobs.by_alert[UNKNOWN].count, 1);
        // Not a repair: `auto_repaired` stays its own count.
        assert_eq!(all.count, 0);
        let first = &all.by_day["2026-09-25"];
        assert_eq!(
            (
                first.recovery_jobs,
                first.recovery_applied,
                first.recovery_escalated
            ),
            (2, 1, 1)
        );
        let second = &all.by_day["2026-09-26"];
        assert_eq!(
            (
                second.recovery_jobs,
                second.recovery_applied,
                second.recovery_escalated
            ),
            (6, 1, 1)
        );

        let task_two = auto_repairs(&events, EventId::new(0), EventId::new(9), |task| {
            task == Some(TaskId::new(2))
        });
        assert_eq!(task_two.recovery_jobs.count, 2);
        assert_eq!(task_two.recovery_jobs.escalated, 1);
        assert!(!task_two.recovery_jobs.by_alert.contains_key("failed"));
        assert!(!task_two.by_day.contains_key("2026-09-25"));
    }
}
