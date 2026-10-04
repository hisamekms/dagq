//! The landings that waited for a person or a recover after their review,
//! against those that did not (goal 39, task 1312): how many of each
//! conflicted on the way to main, whether the landing recheck found the
//! conflict ahead of the landing or the landing's rebase did, and how long
//! the waits took. Main moves while a reviewed run waits, and the landing's
//! rebase then conflicts; this shows whether that still happens.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::asks::Spread;
use super::{EventId, RunEvent, TaskId, timestamp_millis};
use crate::domain::event_kind::{
    ASK_ANSWERED, ASK_CLOSED, ASK_OPENED, INTEGRATION_DEFERRED, INTEGRATION_HELD,
    INTEGRATION_STARTED, LANDING_RECHECK_FAILED, RESUME_STARTED, REVIEW_FAILED, REVIEW_FINISHED,
    RUN_INTEGRATED, RUN_RECOVERED, RUNTIME_ERROR, VALIDATION_FINISHED,
};
use crate::domain::{AskKind, ReasonCode, RunId, RunStatus};

/// Whether a reviewed run waits in an ask of `kind` for a person.
fn waits_in(kind: &AskKind) -> bool {
    matches!(kind, AskKind::ApproveLanding | AskKind::StuckExit)
}
/// The code of a conflict with main.
const REBASE_CONFLICT: &str = ReasonCode::RebaseConflict.as_str();

/// The runs landed in the window, by whether they waited after their review.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LandingWaits {
    pub waited: WaitGroup,
    pub not_waited: WaitGroup,
}

/// One group of landed runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct WaitGroup {
    /// Runs landed (`run_integrated`) in the window.
    pub landed: usize,
    /// Of them, the runs that conflicted at least once between their
    /// review and their landing, found by either check.
    pub conflicted: usize,
    /// `conflicted / landed` to three decimals; null when none landed.
    pub conflicted_ratio: Option<f64>,
    /// The runs the landing recheck found conflicting
    /// (`landing_recheck_failed`, `rebase_conflict`): ahead of the landing.
    pub recheck_conflicts: usize,
    /// The runs whose landing's rebase conflicted (`integration_deferred`,
    /// `rebase_conflict`, into `needs_session`). A run can count in both.
    pub rebase_conflicts: usize,
    /// The waited group only: the seconds each run waited (the union of
    /// its waits), and its runs per what they waited in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_secs: Option<Spread>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_wait: Option<BTreeMap<String, usize>>,
    /// The waited group only: each run, oldest landing first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runs: Option<Vec<WaitedRun>>,
}

/// A landed run that waited after its review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WaitedRun {
    pub task_id: Option<TaskId>,
    pub run_id: RunId,
    /// What it waited in: the ask kinds, `runtime_error`, `integration_held`.
    pub waits: Vec<String>,
    pub wait_secs: i64,
    pub recheck_conflicts: usize,
    pub rebase_conflicts: usize,
}

/// What one landed run went through between its review and its landing.
#[derive(Default)]
struct Landed {
    waits: Vec<String>,
    /// The waits, as `(from, to)` in milliseconds.
    spans: Vec<(i64, i64)>,
    recheck_conflicts: usize,
    rebase_conflicts: usize,
}

/// Group the runs landed with `after < id <= upto` whose task `counts`
/// accepts. Each run's events before its landing are read whatever the
/// window: a run that waited before the window and landed in it is counted.
pub fn landing_waits(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> LandingWaits {
    let mut by_run: BTreeMap<&str, Vec<&RunEvent>> = BTreeMap::new();
    for event in events {
        if let Some(run) = &event.run_id {
            by_run.entry(run.as_str()).or_default().push(event);
        }
    }
    let mut groups = LandingWaits::default();
    let mut waited_secs = Vec::new();
    let mut by_wait: BTreeMap<String, usize> = BTreeMap::new();
    let mut waited_runs = Vec::new();
    for landing in events.iter().filter(|event| {
        event.kind == RUN_INTEGRATED
            && event.id > after
            && event.id <= upto
            && counts(event.task_id)
    }) {
        let Some(run_id) = &landing.run_id else {
            continue;
        };
        let run_events = by_run.get(run_id.as_str()).map_or(&[][..], Vec::as_slice);
        let landed = walk(run_events, landing);
        let waited = !landed.waits.is_empty();
        let group = if waited {
            &mut groups.waited
        } else {
            &mut groups.not_waited
        };
        group.landed += 1;
        group.recheck_conflicts += usize::from(landed.recheck_conflicts > 0);
        group.rebase_conflicts += usize::from(landed.rebase_conflicts > 0);
        group.conflicted +=
            usize::from(landed.recheck_conflicts > 0 || landed.rebase_conflicts > 0);
        if waited {
            let secs = union_millis(landed.spans) / 1000;
            waited_secs.push(secs);
            for wait in &landed.waits {
                *by_wait.entry(wait.clone()).or_default() += 1;
            }
            waited_runs.push(WaitedRun {
                task_id: landing.task_id,
                run_id: run_id.clone(),
                waits: landed.waits,
                wait_secs: secs,
                recheck_conflicts: landed.recheck_conflicts,
                rebase_conflicts: landed.rebase_conflicts,
            });
        }
    }
    for group in [&mut groups.waited, &mut groups.not_waited] {
        group.conflicted_ratio = (group.landed > 0)
            .then(|| (group.conflicted as f64 / group.landed as f64 * 1000.0).round() / 1000.0);
    }
    groups.waited.wait_secs = Some(Spread::of(waited_secs));
    groups.waited.by_wait = Some(by_wait);
    groups.waited.runs = Some(waited_runs);
    groups
}

/// The waits and conflicts of one run up to `landing`, from its first
/// review verdict (`review_finished` or `review_failed`; without one, its
/// first `validation_finished`; without that, all its events).
fn walk(run: &[&RunEvent], landing: &RunEvent) -> Landed {
    let before: Vec<&RunEvent> = run
        .iter()
        .copied()
        .filter(|event| event.id < landing.id)
        .collect();
    let boundary = before
        .iter()
        .find(|event| matches!(event.kind.as_str(), REVIEW_FINISHED | REVIEW_FAILED))
        .or_else(|| {
            before
                .iter()
                .find(|event| event.kind == VALIDATION_FINISHED)
        })
        .map_or(EventId::new(0), |event| event.id);
    let landed_ms = timestamp_millis(&landing.created_at);
    let millis = |event: &RunEvent| timestamp_millis(&event.created_at);
    let mut landed = Landed::default();
    for (index, event) in before.iter().enumerate() {
        let payload = &event.payload;
        match event.kind.as_str() {
            // `approve_landing` opens only after a verdict, at times in the
            // same transaction as a failed review: counted wherever it falls
            // before the landing. A `stuck_exit` of the first session's
            // `/exit` (old interactive runs) is no wait after the review.
            ASK_OPENED => {
                let kind = AskKind::read(payload["kind"].as_str().unwrap_or_default());
                if !waits_in(&kind) || (kind == AskKind::StuckExit && event.id <= boundary) {
                    continue;
                }
                push_wait(&mut landed, kind.as_str());
                let id = ask_id(payload);
                // Up to the ask's first answer or close (recorded on the
                // run), by its id; else the landing.
                let end = before[index + 1..]
                    .iter()
                    .find(|other| {
                        matches!(other.kind.as_str(), ASK_ANSWERED | ASK_CLOSED)
                            && id.is_some()
                            && ask_id(&other.payload) == id
                    })
                    .map_or(landed_ms, |other| timestamp_millis(&other.created_at));
                if let (Some(from), Some(to)) = (millis(event), end) {
                    landed.spans.push((from, to));
                }
            }
            // The lease released to wait for a recover, or a person's look
            // at a verification that failed again: up to the run's next
            // `run_recovered`, `integration_started` or `resume_started`,
            // else the landing.
            RUNTIME_ERROR | INTEGRATION_HELD if event.id > boundary => {
                if event.kind == RUNTIME_ERROR && payload["lease_released"] != true {
                    continue;
                }
                push_wait(&mut landed, &event.kind);
                let end = before[index + 1..]
                    .iter()
                    .find(|other| {
                        matches!(
                            other.kind.as_str(),
                            RUN_RECOVERED | INTEGRATION_STARTED | RESUME_STARTED
                        )
                    })
                    .map_or(landed_ms, |other| timestamp_millis(&other.created_at));
                if let (Some(from), Some(to)) = (millis(event), end) {
                    landed.spans.push((from, to));
                }
            }
            // A held run's parking repeats the finding (`repeat: true`).
            LANDING_RECHECK_FAILED
                if event.id > boundary
                    && payload["code"] == REBASE_CONFLICT
                    && payload["repeat"] != true =>
            {
                landed.recheck_conflicts += 1;
            }
            INTEGRATION_DEFERRED
                if event.id > boundary
                    && payload["code"] == REBASE_CONFLICT
                    && payload["status"]
                        .as_str()
                        .is_none_or(|s| s == RunStatus::NeedsSession.as_str()) =>
            {
                landed.rebase_conflicts += 1;
            }
            _ => {}
        }
    }
    landed
}

/// Note what the run waited in, once per kind.
fn push_wait(landed: &mut Landed, wait: &str) {
    if !landed.waits.iter().any(|known| known == wait) {
        landed.waits.push(wait.to_owned());
    }
}

/// An ask's id in an ask event, as text.
fn ask_id(payload: &Value) -> Option<String> {
    payload
        .get("ask_id")
        .filter(|id| !id.is_null())
        .map(|id| id.as_str().map_or_else(|| id.to_string(), str::to_owned))
}

/// The milliseconds the spans cover together, overlaps counted once.
fn union_millis(mut spans: Vec<(i64, i64)>) -> i64 {
    spans.sort_unstable();
    let mut total = 0;
    let mut current: Option<(i64, i64)> = None;
    for (from, to) in spans.into_iter().filter(|(from, to)| to > from) {
        current = match current {
            Some((start, end)) if from <= end => Some((start, end.max(to))),
            Some((start, end)) => {
                total += end - start;
                Some((from, to))
            }
            None => Some((from, to)),
        };
    }
    total + current.map_or(0, |(start, end)| end - start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const R1: &str = "11111111-1111-4111-8111-111111111111";
    const R2: &str = "22222222-2222-4222-8222-222222222222";
    const R3: &str = "33333333-3333-4333-8333-333333333333";
    const R4: &str = "44444444-4444-4444-8444-444444444444";
    const R5: &str = "55555555-5555-4555-8555-555555555555";
    const R6: &str = "66666666-6666-4666-8666-666666666666";

    /// Builds the events of a queue, numbering them in order.
    #[derive(Default)]
    struct Events(Vec<RunEvent>);

    impl Events {
        fn add(&mut self, task: i64, run: &str, kind: &str, payload: Value, secs: i64) {
            let id = self.0.len() as i64 + 1;
            self.0.push(RunEvent {
                id: EventId::new(id),
                task_id: Some(TaskId::new(task)),
                goal_id: None,
                run_id: Some(RunId::new(run).unwrap()),
                kind: kind.to_owned(),
                payload,
                created_at: crate::domain::marks::utc_text(secs * 1000),
                actor: None,
            });
        }

        fn last(&self) -> EventId {
            self.0.last().unwrap().id
        }
    }

    fn all(events: &[RunEvent]) -> LandingWaits {
        landing_waits(events, EventId::new(0), events.last().unwrap().id, |_| true)
    }

    fn pass() -> Value {
        json!({"verdict": "pass"})
    }

    fn conflict(status: &str) -> Value {
        json!({"code": "rebase_conflict", "status": status})
    }

    /// An `approve_landing` ask, a `stuck_exit` ask and a lease released
    /// for a recover after the review put a run in the waited group, with
    /// the seconds up to the answer, the close and the recover; a run that
    /// went from its review to its landing without them, one whose
    /// `runtime_error` kept the lease, and one whose abandon or `stuck_exit`
    /// came before its review are in the other group.
    #[test]
    fn approve_landing_stuck_exit_and_recover_waits_join_the_waited_group() {
        let mut events = Events::default();
        events.add(1, R1, "review_finished", pass(), 0);
        events.add(
            1,
            R1,
            "ask_opened",
            json!({"ask_id": 7, "kind": "approve_landing"}),
            10,
        );
        events.add(1, R1, "ask_answered", json!({"ask_id": 7}), 110);
        events.add(1, R1, "run_integrated", json!({}), 200);
        events.add(2, R2, "review_finished", pass(), 0);
        events.add(
            2,
            R2,
            "ask_opened",
            json!({"ask_id": "8", "kind": "stuck_exit"}),
            20,
        );
        events.add(2, R2, "ask_closed", json!({"ask_id": "8"}), 70);
        events.add(2, R2, "run_integrated", json!({}), 300);
        events.add(3, R3, "review_finished", json!({"verdict": "concern"}), 0);
        events.add(3, R3, "runtime_error", json!({"lease_released": true}), 30);
        events.add(3, R3, "run_recovered", json!({}), 230);
        events.add(3, R3, "run_integrated", json!({}), 400);
        events.add(4, R4, "review_finished", pass(), 0);
        events.add(4, R4, "landing_queued", json!({}), 5);
        events.add(4, R4, "run_integrated", json!({}), 500);
        events.add(
            5,
            R5,
            "ask_opened",
            json!({"ask_id": 10, "kind": "stuck_exit"}),
            0,
        );
        events.add(5, R5, "review_finished", pass(), 0);
        events.add(5, R5, "runtime_error", json!({"lease_released": false}), 5);
        events.add(5, R5, "run_integrated", json!({}), 600);
        events.add(6, R6, "runtime_error", json!({"lease_released": true}), 0);
        events.add(6, R6, "run_recovered", json!({}), 50);
        events.add(6, R6, "review_finished", pass(), 60);
        events.add(6, R6, "run_integrated", json!({}), 700);
        // An ask of another kind is no wait after the review.
        events.add(
            6,
            R6,
            "ask_opened",
            json!({"ask_id": 9, "kind": "decide"}),
            710,
        );

        let groups = all(&events.0);
        assert_eq!(groups.waited.landed, 3);
        assert_eq!(groups.not_waited.landed, 3);
        assert_eq!(
            groups.waited.wait_secs,
            Some(Spread {
                count: 3,
                median: Some(100),
                p90: Some(200),
                max: Some(200),
            })
        );
        assert_eq!(
            groups.waited.by_wait,
            Some(BTreeMap::from([
                ("approve_landing".to_owned(), 1),
                ("runtime_error".to_owned(), 1),
                ("stuck_exit".to_owned(), 1),
            ]))
        );
        let runs = groups.waited.runs.unwrap();
        assert_eq!(
            runs.iter()
                .map(|run| (run.run_id.as_str(), run.waits.clone(), run.wait_secs))
                .collect::<Vec<_>>(),
            [
                (R1, vec!["approve_landing".to_owned()], 100),
                (R2, vec!["stuck_exit".to_owned()], 50),
                (R3, vec!["runtime_error".to_owned()], 200),
            ]
        );
        assert_eq!(groups.not_waited.wait_secs, None);
        assert_eq!(groups.not_waited.runs, None);
    }

    /// A wait not answered by the landing lasts until it; overlapping
    /// waits of one run count once, and a held landing waits for the next
    /// attempt.
    #[test]
    fn overlapping_waits_count_once_and_an_open_ask_waits_to_the_landing() {
        let mut events = Events::default();
        events.add(1, R1, "review_finished", pass(), 0);
        events.add(
            1,
            R1,
            "ask_opened",
            json!({"ask_id": 1, "kind": "approve_landing"}),
            100,
        );
        events.add(1, R1, "runtime_error", json!({"lease_released": true}), 150);
        events.add(1, R1, "integration_started", json!({}), 250);
        events.add(1, R1, "run_integrated", json!({}), 400);
        events.add(2, R2, "review_finished", pass(), 0);
        events.add(
            2,
            R2,
            "integration_held",
            json!({"status": "awaiting_integration"}),
            10,
        );
        events.add(2, R2, "integration_started", json!({}), 70);
        events.add(2, R2, "run_integrated", json!({}), 80);

        let runs = all(&events.0).waited.runs.unwrap();
        assert_eq!(runs[0].wait_secs, 300);
        assert_eq!(runs[0].waits, ["approve_landing", "runtime_error"]);
        assert_eq!(runs[1].wait_secs, 60);
        assert_eq!(runs[1].waits, ["integration_held"]);
    }

    /// Per group, a run conflicted when the landing recheck or the
    /// landing's rebase found a conflict between its review and its
    /// landing, and each check counts the runs it found: a run found by
    /// both counts once in `conflicted`. A held run's repeated finding, a
    /// verification failure and a conflict before the review are not
    /// counted.
    #[test]
    fn conflicts_count_per_group_by_the_check_that_found_them() {
        let mut events = Events::default();
        let approve = |id: i64| json!({"ask_id": id, "kind": "approve_landing"});
        // Waited, and the recheck found the conflict ahead of the landing.
        events.add(1, R1, "review_finished", pass(), 0);
        events.add(1, R1, "ask_opened", approve(1), 1);
        events.add(
            1,
            R1,
            "landing_recheck_failed",
            json!({"code": "rebase_conflict", "action": "held"}),
            2,
        );
        events.add(
            1,
            R1,
            "landing_recheck_failed",
            json!({"code": "rebase_conflict", "action": "resumed", "repeat": true}),
            3,
        );
        events.add(1, R1, "run_integrated", json!({}), 10);
        // Waited, and the landing's rebase conflicted.
        events.add(2, R2, "review_finished", pass(), 0);
        events.add(2, R2, "ask_opened", approve(2), 1);
        events.add(2, R2, "integration_deferred", conflict("needs_session"), 2);
        events.add(2, R2, "run_integrated", json!({}), 10);
        // Waited, no conflict: a failed check is not one.
        events.add(3, R3, "review_finished", pass(), 0);
        events.add(3, R3, "ask_opened", approve(3), 1);
        events.add(
            3,
            R3,
            "landing_recheck_failed",
            json!({"code": "verification_failed"}),
            2,
        );
        events.add(
            3,
            R3,
            "integration_deferred",
            json!({"code": "verification_failed", "status": "needs_session"}),
            3,
        );
        events.add(3, R3, "run_integrated", json!({}), 10);
        // Did not wait; both checks found it.
        events.add(4, R4, "review_finished", pass(), 0);
        events.add(
            4,
            R4,
            "landing_recheck_failed",
            json!({"code": "rebase_conflict"}),
            1,
        );
        events.add(4, R4, "integration_deferred", conflict("needs_session"), 2);
        events.add(4, R4, "run_integrated", json!({}), 10);
        // Did not wait; the conflict came before the review.
        events.add(5, R5, "integration_deferred", conflict("needs_session"), 0);
        events.add(5, R5, "review_finished", pass(), 1);
        events.add(5, R5, "run_integrated", json!({}), 10);

        let groups = all(&events.0);
        let waited = &groups.waited;
        assert_eq!(
            (
                waited.landed,
                waited.conflicted,
                waited.recheck_conflicts,
                waited.rebase_conflicts
            ),
            (3, 2, 1, 1)
        );
        assert_eq!(waited.conflicted_ratio, Some(0.667));
        let runs = waited.runs.as_ref().unwrap();
        assert_eq!(
            runs.iter()
                .map(|run| (run.recheck_conflicts, run.rebase_conflicts))
                .collect::<Vec<_>>(),
            [(1, 0), (0, 1), (0, 0)]
        );
        let not_waited = &groups.not_waited;
        assert_eq!(
            (
                not_waited.landed,
                not_waited.conflicted,
                not_waited.recheck_conflicts,
                not_waited.rebase_conflicts
            ),
            (2, 1, 1, 1)
        );
        assert_eq!(not_waited.conflicted_ratio, Some(0.5));
    }

    /// Only the landings in the window, of the tasks counted, are grouped:
    /// a run landed before or after it, and a run that waited and
    /// conflicted but has not landed, are not. A run that waited before
    /// the window and landed in it is.
    #[test]
    fn landings_outside_the_window_and_runs_not_landed_are_not_counted() {
        let mut events = Events::default();
        events.add(1, R1, "review_finished", pass(), 0);
        events.add(1, R1, "run_integrated", json!({}), 1);
        events.add(2, R2, "review_finished", pass(), 2);
        events.add(
            2,
            R2,
            "ask_opened",
            json!({"ask_id": 1, "kind": "approve_landing"}),
            3,
        );
        let after = events.last();
        events.add(2, R2, "ask_answered", json!({"ask_id": 1}), 13);
        events.add(2, R2, "run_integrated", json!({}), 20);
        events.add(3, R3, "review_finished", pass(), 21);
        events.add(
            3,
            R3,
            "ask_opened",
            json!({"ask_id": 2, "kind": "approve_landing"}),
            22,
        );
        events.add(3, R3, "integration_deferred", conflict("needs_session"), 23);
        events.add(4, R4, "review_finished", pass(), 24);
        events.add(4, R4, "run_integrated", json!({}), 25);
        let upto = events.last();
        events.add(5, R5, "review_finished", pass(), 26);
        events.add(5, R5, "run_integrated", json!({}), 27);

        let groups = landing_waits(&events.0, after, upto, |_| true);
        assert_eq!(groups.waited.landed, 1);
        assert_eq!(groups.waited.runs.as_ref().unwrap()[0].wait_secs, 10);
        assert_eq!(groups.waited.conflicted, 0);
        assert_eq!(groups.not_waited.landed, 1);
        assert_eq!(groups.not_waited.conflicted, 0);

        let goal = landing_waits(&events.0, after, upto, |task| task == Some(TaskId::new(4)));
        assert_eq!((goal.waited.landed, goal.not_waited.landed), (0, 1));
        assert_eq!(goal.waited.conflicted_ratio, None);
        assert_eq!(goal.waited.wait_secs, Some(Spread::default()));
    }
}
