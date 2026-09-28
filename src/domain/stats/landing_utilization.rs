//! The use of the single integration slot (goal 72): landings run one at
//! a time, so the share of a window the `integrate` attempts held the
//! Integrator caps the landings per hour. An attempt holds it from its
//! `integration_started` to its end (`run_integrated`, a deferral, a
//! hold, an error or a failure; see [`ENDS`]), landed or not. The next
//! `integration_started` of any run also ends the one before, so the
//! attempts never overlap and their sum never exceeds the window. Next to
//! it: each attempt's time, and how many runs waited for the slot
//! (`landing_queued` to their `integration_started`).
use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;

use super::landing::p90;
use super::{RunEvent, median, payload_status, timestamp_millis};
use crate::domain::{RunId, TaskId, marks::utc_text};

const HOUR_MS: i64 = 3_600_000;

/// The events that end an `integrate` attempt of their run (the
/// timeline's `integrating` gap ends at the same ones).
const ENDS: [&str; 9] = [
    "run_integrated",
    "integration_deferred",
    "integration_error",
    "integration_held",
    "integration_failed",
    "runtime_error",
    "run_adopted",
    "run_recovered",
    "resume_started",
];

/// The events that end a run's wait for the slot besides its
/// `integration_started`: it landed, left for a session, or was taken over.
const WAIT_ENDS: [&str; 5] = [
    "run_integrated",
    "resume_started",
    "revise_requested",
    "run_adopted",
    "run_recovered",
];

/// A status that ends a wait or an attempt whatever event carries it.
fn leaves(payload: &Value) -> bool {
    matches!(
        payload_status(payload),
        Some("needs_session" | "failed" | "interrupted" | "canceled")
    )
}

/// One `integrate` attempt, in unix ms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub task_id: Option<TaskId>,
    pub start: i64,
    pub end: i64,
    pub landed: bool,
    /// Still running (it ends now).
    pub open: bool,
}

/// A time a run waited for the slot, in unix ms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wait {
    pub run_id: RunId,
    pub task_id: Option<TaskId>,
    pub start: i64,
    pub end: i64,
}

/// Every `integrate` attempt and every wait for the slot of `events`, the
/// open ones ending at `now_ms`.
pub fn spans(events: &[RunEvent], now_ms: i64) -> (Vec<Attempt>, Vec<Wait>) {
    let mut attempts: Vec<Attempt> = Vec::new();
    let mut open: Option<(RunId, usize)> = None;
    let mut waits: Vec<Wait> = Vec::new();
    let mut waiting: HashMap<RunId, usize> = HashMap::new();
    for event in events {
        let (Some(run_id), Some(at)) = (&event.run_id, timestamp_millis(&event.created_at)) else {
            continue;
        };
        let kind = event.kind.as_str();
        if kind == "integration_started" || WAIT_ENDS.contains(&kind) || leaves(&event.payload) {
            if let Some(index) = waiting.remove(run_id) {
                waits[index].end = at.max(waits[index].start);
            }
        } else if kind == "landing_queued" && !waiting.contains_key(run_id) {
            waiting.insert(run_id.clone(), waits.len());
            waits.push(Wait {
                run_id: run_id.clone(),
                task_id: event.task_id,
                start: at,
                end: now_ms,
            });
        }
        let ends_own = ENDS.contains(&kind) || leaves(&event.payload);
        if (kind == "integration_started"
            || (ends_own && open.as_ref().is_some_and(|o| o.0 == *run_id)))
            && let Some((_, index)) = open.take()
        {
            let attempt = &mut attempts[index];
            attempt.end = at.max(attempt.start);
            attempt.landed = kind == "run_integrated";
            attempt.open = false;
        }
        if kind == "integration_started" {
            // After the one before, even if the clocks disagree.
            let start = attempts.last().map_or(at, |last| at.max(last.end));
            open = Some((run_id.clone(), attempts.len()));
            attempts.push(Attempt {
                task_id: event.task_id,
                start,
                end: now_ms.max(start),
                landed: false,
                open: true,
            });
        }
    }
    (attempts, waits)
}

fn overlap((from, to): (i64, i64), start: i64, end: i64) -> i64 {
    (to.min(end) - from.max(start)).max(0)
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

#[allow(clippy::cast_precision_loss)]
fn ratio(part: i64, whole: i64) -> Option<f64> {
    (whole > 0).then(|| round3(part as f64 / whole as f64))
}

/// The hour of the window the attempts held the slot most of.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PeakHour {
    /// When the hour starts (UTC).
    pub start: String,
    pub utilization: f64,
}

/// The seconds of the attempts that ended in the window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AttemptSecs {
    pub count: usize,
    pub min: Option<i64>,
    pub median: Option<i64>,
    pub p90: Option<i64>,
    pub max: Option<i64>,
}

/// The runs waiting for the slot over the window.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct QueueDepth {
    /// The time-weighted mean of the runs waiting; null for an empty window.
    pub mean: Option<f64>,
    /// The most that waited at once.
    pub max: usize,
    /// The runs that waited in the window.
    pub runs: usize,
}

/// The use of the integration slot over one window of time.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LandingUtilization {
    pub window_secs: i64,
    /// The time the attempts held the slot in the window.
    pub busy_secs: i64,
    /// `busy_secs / window_secs`; null for an empty window.
    pub utilization: Option<f64>,
    /// The window's whole hours from its start (the whole window when it
    /// is shorter than one), the busiest; null for an empty window.
    pub peak_hour: Option<PeakHour>,
    /// The attempts that held the slot in the window, landed or not.
    pub attempts: usize,
    /// Those of them that landed.
    pub landed: usize,
    pub attempt_secs: AttemptSecs,
    pub queue: QueueDepth,
}

/// The use of the slot from `from` to `to` (unix ms) by the attempts and
/// waits of the tasks `counts` keeps.
pub fn landing_utilization(
    events: &[RunEvent],
    from: i64,
    to: i64,
    now_ms: i64,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> LandingUtilization {
    let to = to.min(now_ms).max(from);
    let (attempts, waits) = spans(events, now_ms);
    let attempts: Vec<&Attempt> = attempts
        .iter()
        .filter(|attempt| counts(attempt.task_id))
        .filter(|attempt| overlap((attempt.start, attempt.end), from, to) > 0)
        .collect();
    let busy = |start: i64, end: i64| -> i64 {
        attempts
            .iter()
            .map(|attempt| overlap((attempt.start, attempt.end), start, end))
            .sum()
    };
    let window = to - from;
    let busy_ms = busy(from, to);
    let hours: Vec<(i64, i64)> = if window < HOUR_MS {
        vec![(from, to)]
    } else {
        (0..window / HOUR_MS)
            .map(|hour| (from + hour * HOUR_MS, from + (hour + 1) * HOUR_MS))
            .collect()
    };
    let peak_hour = hours
        .iter()
        .filter(|(start, end)| end > start)
        .map(|&(start, end)| (start, busy(start, end), end - start))
        .max_by(|a, b| (a.1 * b.2).cmp(&(b.1 * a.2)).then(b.0.cmp(&a.0)))
        .map(|(start, held, length)| PeakHour {
            start: utc_text(start),
            utilization: ratio(held, length).unwrap_or(0.0),
        });
    let mut secs: Vec<i64> = attempts
        .iter()
        .filter(|attempt| !attempt.open && attempt.end > from && attempt.end <= to)
        .map(|attempt| (attempt.end - attempt.start) / 1000)
        .collect();
    let attempt_secs = AttemptSecs {
        count: secs.len(),
        min: secs.iter().copied().min(),
        median: median(&mut secs),
        p90: p90(&mut secs),
        max: secs.iter().copied().max(),
    };
    let waits: Vec<&Wait> = waits
        .iter()
        .filter(|wait| counts(wait.task_id))
        .filter(|wait| overlap((wait.start, wait.end), from, to) > 0)
        .collect();
    let waited: i64 = waits
        .iter()
        .map(|wait| overlap((wait.start, wait.end), from, to))
        .sum();
    // The most waiting at once: +1 at each start, -1 at each end.
    let mut steps: Vec<(i64, i64)> = waits
        .iter()
        .flat_map(|wait| [(wait.start.max(from), 1), (wait.end.min(to), -1)])
        .collect();
    steps.sort_unstable();
    let mut depth = 0_i64;
    let mut deepest = 0_i64;
    for (_, step) in steps {
        depth += step;
        deepest = deepest.max(depth);
    }
    LandingUtilization {
        window_secs: window / 1000,
        busy_secs: busy_ms / 1000,
        utilization: ratio(busy_ms, window),
        peak_hour: (window > 0).then_some(peak_hour).flatten(),
        attempts: attempts.len(),
        landed: attempts
            .iter()
            .filter(|attempt| attempt.landed && attempt.end <= to)
            .count(),
        attempt_secs,
        queue: QueueDepth {
            mean: ratio(waited, window),
            max: usize::try_from(deepest).unwrap_or(0),
            runs: waits
                .iter()
                .map(|wait| &wait.run_id)
                .collect::<std::collections::HashSet<_>>()
                .len(),
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::EventId;

    const RUNS: [&str; 3] = [
        "00000001-0000-4000-8000-000000000001",
        "00000002-0000-4000-8000-000000000002",
        "00000003-0000-4000-8000-000000000003",
    ];

    /// Events of `(run index, kind, payload, secs)`.
    fn events(rows: &[(usize, &str, Value, i64)]) -> Vec<RunEvent> {
        rows.iter()
            .enumerate()
            .map(|(index, (run, kind, payload, secs))| RunEvent {
                id: EventId::new(index as i64 + 1),
                task_id: Some(TaskId::new(*run as i64 + 1)),
                goal_id: None,
                run_id: Some(RunId::new(RUNS[*run]).unwrap()),
                kind: (*kind).to_owned(),
                payload: payload.clone(),
                created_at: utc_text(secs * 1000),
                actor: None,
            })
            .collect()
    }

    #[test]
    fn attempts_landed_or_not_hold_the_slot_and_never_overlap() {
        let events = events(&[
            (0, "landing_queued", json!({"via": "exit"}), 0),
            (1, "landing_queued", json!({"via": "exit"}), 100),
            (0, "integration_started", json!({}), 600),
            // Deferred: it held the slot all the same.
            (
                0,
                "integration_deferred",
                json!({"status": "needs_session"}),
                900,
            ),
            (1, "integration_started", json!({}), 1200),
            // Its end was never recorded: the next start ends it.
            (2, "integration_started", json!({}), 1800),
            (2, "run_integrated", json!({"status": "integrated"}), 2400),
        ]);
        let (attempts, waits) = spans(&events, 10_000_000);
        let held: Vec<(i64, i64, bool)> = attempts
            .iter()
            .map(|a| (a.start / 1000, a.end / 1000, a.landed))
            .collect();
        assert_eq!(
            held,
            [(600, 900, false), (1200, 1800, false), (1800, 2400, true)]
        );
        assert_eq!(
            waits
                .iter()
                .map(|w| (w.start / 1000, w.end / 1000))
                .collect::<Vec<_>>(),
            [(0, 600), (100, 1200)]
        );

        let used = landing_utilization(&events, 0, 3_600_000, 10_000_000, |_| true);
        assert_eq!(used.window_secs, 3600);
        assert_eq!(used.busy_secs, 300 + 600 + 600);
        assert!(used.busy_secs <= used.window_secs);
        assert_eq!(used.utilization, Some(round3(1500.0 / 3600.0)));
        assert_eq!(used.attempts, 3);
        assert_eq!(used.landed, 1);
        assert_eq!(
            used.attempt_secs,
            AttemptSecs {
                count: 3,
                min: Some(300),
                median: Some(600),
                p90: Some(600),
                max: Some(600)
            }
        );
        assert_eq!(used.queue.runs, 2);
        assert_eq!(used.queue.max, 2);
        assert_eq!(used.queue.mean, Some(round3(1700.0 / 3600.0)));
        let peak = used.peak_hour.unwrap();
        assert_eq!(peak.utilization, used.utilization.unwrap());
        assert_eq!(peak.start, utc_text(0));

        // Only the goal's tasks.
        let first = landing_utilization(&events, 0, 3_600_000, 10_000_000, |task| {
            task == Some(TaskId::new(1))
        });
        assert_eq!((first.busy_secs, first.attempts), (300, 1));
    }

    #[test]
    fn the_peak_is_the_busiest_whole_hour_and_an_open_attempt_ends_now() {
        let events = events(&[
            (0, "integration_started", json!({}), 3600),
            (
                0,
                "run_integrated",
                json!({"status": "integrated"}),
                3600 + 2700,
            ),
            // Clocks that disagree: it starts after the one before.
            (1, "integration_started", json!({}), 3600 + 2600),
        ]);
        let now = (3 * 3600 + 1800) * 1000;
        let used = landing_utilization(&events, 0, 4 * 3_600_000, now, |_| true);
        // Up to now only: three and a half hours.
        assert_eq!(used.window_secs, 3 * 3600 + 1800);
        let (attempts, _) = spans(&events, now);
        assert_eq!(attempts[1].start, (3600 + 2700) * 1000);
        assert!(attempts[1].open);
        assert_eq!(used.busy_secs, 2700 + (3 * 3600 + 1800 - 3600 - 2700));
        assert!(used.busy_secs <= used.window_secs);
        let peak = used.peak_hour.unwrap();
        // Two whole hours are full: the earlier is the peak.
        assert_eq!(peak.utilization, 1.0);
        assert_eq!(peak.start, utc_text(3_600_000));
        // The open attempt has no time yet.
        assert_eq!(used.attempt_secs.count, 1);
        assert_eq!(used.attempt_secs.median, Some(2700));

        let empty = landing_utilization(&events, 5, 5, now, |_| true);
        assert_eq!(
            (empty.utilization, empty.peak_hour, empty.queue.mean),
            (None, None, None)
        );
    }
}
