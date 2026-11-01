//! The lives of runs as the tests of what builds and compares the ledger
//! lay them down: events with ids in order, at seconds from [`T0`].

use serde_json::{Value, json};

use crate::domain::marks::utc_text;
use crate::domain::run_phase::{Attempt, Phase, PhaseChange};
use crate::domain::{EventId, RunEvent, RunId, TaskId};

pub(crate) const T0: i64 = 1_900_000_000;

/// The unix milliseconds of `secs` after [`T0`].
pub(crate) const fn ms(secs: i64) -> i64 {
    (T0 + secs) * 1000
}

/// A run id of `n`.
pub(crate) fn run_id(n: u8) -> String {
    format!("{n:08x}-1111-4111-8111-111111111111")
}

/// The events of a queue, ascending id.
#[derive(Debug, Clone, Default)]
pub(crate) struct Log {
    pub(crate) events: Vec<RunEvent>,
}

impl Log {
    /// Add an event of `kind` on `task` and `run` at `secs`.
    pub(crate) fn add(
        &mut self,
        task: Option<i64>,
        run: Option<&str>,
        kind: &str,
        payload: Value,
        secs: i64,
    ) -> &mut Self {
        let id = EventId::new(i64::try_from(self.events.len()).unwrap() + 1);
        self.events.push(RunEvent {
            id,
            task_id: task.map(TaskId::new),
            goal_id: None,
            run_id: run.map(|run| RunId::new(run).unwrap()),
            kind: kind.into(),
            payload,
            created_at: utc_text(ms(secs)),
            actor: None,
        });
        self
    }

    pub(crate) fn last(&self) -> EventId {
        self.events.last().map_or(EventId::new(0), |event| event.id)
    }

    /// The events after `after`.
    pub(crate) fn after(&self, after: EventId) -> Vec<RunEvent> {
        self.events
            .iter()
            .filter(|event| event.id > after)
            .cloned()
            .collect()
    }

    pub(crate) fn phase(
        &mut self,
        task: i64,
        run: &str,
        secs: i64,
        phase: Phase,
        cause: &str,
    ) -> &mut Self {
        self.phase_in(task, run, secs, phase, Attempt::FIRST, cause)
    }

    /// `run` of `task` moved to `phase` in `attempt` at `secs`.
    pub(crate) fn phase_in(
        &mut self,
        task: i64,
        run: &str,
        secs: i64,
        phase: Phase,
        attempt: Attempt,
        cause: &str,
    ) -> &mut Self {
        self.add(
            Some(task),
            Some(run),
            "run_phase_changed",
            PhaseChange::new(phase, attempt, cause).payload(),
            secs,
        )
    }

    /// `task` created at `secs` and `run` of it claimed, worked and
    /// validated from `secs` to `secs + 660`, then queued for the slot.
    pub(crate) fn worked(&mut self, task: i64, run: &str, secs: i64) -> &mut Self {
        self.add(Some(task), None, "task_created", json!({}), secs)
            .add(
                Some(task),
                Some(run),
                "run_claimed",
                json!({"from": "ready", "to": "in_progress", "provider": "claude"}),
                secs,
            )
            .phase(task, run, secs, Phase::Provisioning, "x")
            .add(Some(task), Some(run), "agent_started", json!({}), secs + 5)
            .phase(task, run, secs + 10, Phase::Worker, "x")
            .add(
                Some(task),
                Some(run),
                "first_commit_observed",
                json!({}),
                secs + 70,
            )
            .add(
                Some(task),
                Some(run),
                "receipt_observed",
                json!({}),
                secs + 600,
            )
            .phase(task, run, secs + 600, Phase::Validating, "x")
            .add(
                Some(task),
                Some(run),
                "validation_finished",
                json!({"status": "awaiting_integration"}),
                secs + 660,
            )
            .add(
                Some(task),
                Some(run),
                "landing_queued",
                json!({"via": "exit"}),
                secs + 660,
            )
            .phase(task, run, secs + 660, Phase::AwaitingSlot, "x")
    }

    /// `run` of `task` lands from `secs`: the landing, `run_integrated` at
    /// `secs + 200` and its push.
    pub(crate) fn integrated(&mut self, task: i64, run: &str, secs: i64) -> &mut Self {
        self.phase(task, run, secs, Phase::Landing, "x")
            .add(
                Some(task),
                Some(run),
                "integration_started",
                json!({}),
                secs,
            )
            .add(
                Some(task),
                Some(run),
                "integration_rebased",
                json!({}),
                secs,
            )
            .add(
                Some(task),
                Some(run),
                "run_integrated",
                json!({"from": "validating", "to": "integrated", "status": "integrated"}),
                secs + 200,
            )
            .phase(task, run, secs + 200, Phase::Push { in_slot: true }, "x")
    }

    /// The push of `run` of `task` finished at `secs`, the run and the task
    /// ended with it.
    pub(crate) fn pushed(&mut self, task: i64, run: &str, secs: i64) -> &mut Self {
        self.add(
            Some(task),
            Some(run),
            "push_finished",
            json!({"remote": "origin", "branch": "main"}),
            secs,
        )
        .phase(task, run, secs, Phase::Ended, "pushed")
        .add(
            Some(task),
            None,
            "task_status_changed",
            json!({"from": "in_progress", "to": "completed"}),
            secs,
        )
    }

    /// The push of `run` of `task` failed at `secs`: pending.
    pub(crate) fn push_failed(&mut self, task: i64, run: &str, secs: i64) -> &mut Self {
        self.add(
            Some(task),
            Some(run),
            "push_failed",
            json!({"remote": "origin", "branch": "main"}),
            secs,
        )
        .phase(task, run, secs, Phase::PushPending, "push_failed")
    }

    /// A run that lands: worked from `secs`, landing at `secs + 700`,
    /// pushed at `secs + 930`.
    pub(crate) fn landed(&mut self, task: i64, run: &str, secs: i64) -> &mut Self {
        self.worked(task, run, secs)
            .integrated(task, run, secs + 700)
            .pushed(task, run, secs + 930)
    }

    /// A run of `task` from before the switch: no `run_phase_changed`,
    /// landed at `secs + 1200`.
    pub(crate) fn legacy(&mut self, task: i64, run: &str, secs: i64) -> &mut Self {
        let at = |log: &mut Self, offset: i64, kind: &str, payload: Value| {
            log.add(Some(task), Some(run), kind, payload, secs + offset);
        };
        at(self, 0, "run_claimed", json!({"provider": "claude"}));
        at(self, 5, "agent_started", json!({}));
        at(self, 70, "first_commit_observed", json!({}));
        at(self, 600, "receipt_observed", json!({}));
        at(
            self,
            660,
            "validation_finished",
            json!({"status": "awaiting_integration"}),
        );
        at(self, 1000, "integration_started", json!({}));
        at(
            self,
            1200,
            "run_integrated",
            json!({"status": "integrated"}),
        );
        at(self, 1230, "push_finished", json!({}));
        self
    }
}

/// A run row of `run` (its task `task`) with `intervals` and their sums,
/// as the builder writes one, for the readers of the rows.
pub(crate) fn run_row(
    run: &str,
    task: Option<TaskId>,
    intervals: Vec<super::fold::Interval>,
) -> super::LedgerRow {
    let mut row = super::LedgerRow::new(super::LedgerKind::Run, run.to_owned(), task, "t");
    row.totals = super::Totals::of(&intervals);
    row.intervals = intervals;
    row
}

/// The cases a book of only the events after a position gets wrong (ask
/// 1373), the position the length of the first part: a planner's session
/// closed before it, a record of none, a plan review's job whose session
/// closed before it and one whose session closes after it; then after it
/// a late record of the closed session, the planner's next session and an
/// event naming only the planner, another record of none, the end of the
/// first job (its model from the close before), the close of the second
/// job's session and the plan review's next job.
pub(crate) fn sessions_across() -> (Log, usize) {
    let mut log = Log::default();
    log.add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 1}),
        0,
    )
    .add(
        None,
        None,
        "session_closed",
        json!({"kind": "runtime_planner", "planner_id": 1, "opened_event_id": 1,
               "session_id": "p-1", "model": "planner-model"}),
        10,
    )
    .add(
        None,
        None,
        "session_closed",
        json!({"session_id": "s-out"}),
        15,
    )
    .add(
        None,
        None,
        "plan_review_started",
        json!({"plan_review_id": "9", "session_id": "r-9"}),
        20,
    )
    .add(
        None,
        None,
        "session_opened",
        json!({"kind": "plan_review", "plan_review_id": "9", "session_id": "r-9"}),
        21,
    )
    .add(
        None,
        None,
        "session_closed",
        json!({"kind": "plan_review", "plan_review_id": "9", "opened_event_id": 5,
               "session_id": "r-9", "model": "review-model"}),
        30,
    )
    .add(
        None,
        None,
        "plan_review_started",
        json!({"plan_review_id": "10", "session_id": "r-10"}),
        40,
    );
    let position = log.events.len();
    log.add(
        None,
        None,
        "session_turns",
        json!({"opened_event_id": 1, "turns": []}),
        50,
    )
    .add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 1}),
        60,
    )
    .add(None, None, "planner_note", json!({"planner_id": 1}), 65)
    .add(
        None,
        None,
        "session_closed",
        json!({"session_id": "s-out-2"}),
        70,
    )
    .add(
        None,
        None,
        "plan_review_finished",
        json!({"plan_review_id": "9", "verdict": "pass"}),
        80,
    )
    .add(
        None,
        None,
        "session_closed",
        json!({"plan_review_id": "10", "session_id": "r-10", "model": "later-model"}),
        90,
    )
    .add(
        None,
        None,
        "plan_review_started",
        json!({"plan_review_id": "9", "session_id": "r-9b"}),
        100,
    );
    (log, position)
}

/// A finding's record of `status` (finding 1).
pub(crate) fn finding_record(status: &str) -> Value {
    json!({"finding_id": 1, super::findings::VALUES: {"id": 1, "target": "queue",
        "task_id": null, "run_id": null, "goal_id": null, "kind": "conflict_hotspot",
        "subject": "src/a.rs", "summary": "a", "impact": "normal", "status": status,
        "occurrences": 1, "last_seen_at": 0}})
}

/// The cases a queue book of only the events after a position gets wrong
/// (ask 1373), the position the length of the first part: before it a
/// stream of `s1` open from its baseline and a record of it, an alert of
/// `s1` open, a finding's first record, a slots full of `s2` open, a hold
/// on the claims its supervisor `s3` stopped, a deferral open, a landing
/// that ended, another landing open and a mark; after it an event of a
/// task timed before the first landing's end (which reaches it), a
/// record of the stream, the end of the alert, the finding's next record,
/// events of `s2` and `s3` (whose lives reach their slots full and hold),
/// the end of the open landing, the claim that ends the deferral, the next
/// baseline of `s1` (which replaces its stream) and the end of the slots
/// full.
pub(crate) fn queue_across() -> (Log, usize) {
    let alert = json!({"supervisor": "s1", "judgment": "slots", "kind": "idle"});
    let (a, b, c) = (run_id(1), run_id(2), run_id(3));
    let mut log = Log::default();
    log.add(
        None,
        None,
        "supervisor_alive",
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
    .add(None, None, "live_alert_started", alert.clone(), 2)
    .add(None, None, "finding_recorded", finding_record("open"), 3)
    .add(
        None,
        None,
        "slots_full_started",
        json!({"supervisor": "s2", "class": "heavy"}),
        4,
    )
    .add(None, None, "claim_held", json!({"supervisor": "s3"}), 5)
    .add(
        None,
        None,
        "supervisor_stopped",
        json!({"supervisor": "s3"}),
        6,
    )
    .add(
        Some(5),
        None,
        "claim_deferred",
        json!({"supervisor": "s1", "reason": "paths"}),
        7,
    )
    .add(Some(1), Some(&a), "integration_started", json!({}), 8)
    .add(Some(1), Some(&a), "run_integrated", json!({}), 20)
    .add(Some(2), Some(&b), "integration_started", json!({}), 21)
    .add(None, None, "mark_added", json!({"mark": "m"}), 22)
    .add(
        None,
        None,
        "live_alert_input_changed",
        json!({"supervisor": "s1", "version": 2}),
        23,
    );
    let position = log.events.len();
    log.add(Some(7), None, "task_created", json!({}), 15)
        .add(
            None,
            None,
            "live_alert_input_changed",
            json!({"supervisor": "s1", "version": 3}),
            30,
        )
        .add(None, None, "live_alert_ended", alert, 31)
        .add(
            None,
            None,
            "finding_recorded",
            finding_record("resolved"),
            32,
        )
        .add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "s2"}),
            33,
        )
        .add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "s3"}),
            34,
        )
        .add(Some(2), Some(&b), "run_integrated", json!({}), 35)
        .add(Some(5), Some(&c), "run_claimed", json!({}), 36)
        .add(
            None,
            None,
            "live_alert_baseline",
            json!({"supervisor": "s1", "version": 4}),
            37,
        )
        .add(
            None,
            None,
            "slots_full_ended",
            json!({"supervisor": "s2", "class": "heavy"}),
            38,
        );
    (log, position)
}
