//! The lives of runs as the tests of what builds and compares the ledger
//! lay them down: events with ids in order, at seconds from [`T0`].

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::domain::marks::utc_text;
use crate::domain::run_phase::{Attempt, Phase, PhaseChange};
use crate::domain::{EventId, RunEvent, RunId, TaskId};

pub(crate) mod frozen;
pub(crate) mod omitted;

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

/// The cases a book of the supervisors' lives and the waits before a claim
/// of only the events after a position gets wrong (ask 1373), with the
/// position (the length of the first part) and the time (seconds) of the
/// pass at it. Before it: a slots full freed before task 1 was created
/// (`d`), settled by the pass; task 1 ready, and a slots full freed over its time ready (`e`);
/// a slots full and a hold on the claims of `b`, which stops (its life
/// closes the slots full, its stop ends the hold); a light slots full of
/// `c`, gone silent by the pass; and `a`, alive, with a slots full, a hold
/// on the claims, a deferral of task 1 and a hold on the landings, all
/// open; and `f`, alive with a slots full, whose silence comes after the
/// position with no event of it there. After it: an event of `b` after its stop and one of `c` after its
/// silence (each opens its slots full again), the hold, the deferral and
/// the hold on the landings ended, and the last event of `a` before its
/// silence. Task 1 stays ready throughout, so its overlays read them all.
pub(crate) fn waits_across() -> (Log, usize, i64) {
    let mut log = Log::default();
    let at = |supervisor: &str| json!({"supervisor": supervisor, "class": "heavy"});
    log.add(None, None, "slots_full_started", at("d"), 0)
        .add(None, None, "slots_full_ended", at("d"), 50)
        .add(
            Some(1),
            None,
            "task_created",
            json!({"status": "draft"}),
            100,
        )
        .add(
            Some(1),
            None,
            "task_status_changed",
            json!({"from": "draft", "to": "ready"}),
            110,
        )
        .add(None, None, "slots_full_started", at("e"), 120)
        .add(None, None, "slots_full_ended", at("e"), 130)
        .add(None, None, "slots_full_started", at("b"), 140)
        .add(
            None,
            None,
            "claim_held",
            json!({"supervisor": "b", "reason": "disk"}),
            145,
        )
        .add(
            None,
            None,
            "supervisor_stopped",
            json!({"supervisor": "b", "last_heartbeat_at": T0 + 148}),
            150,
        )
        .add(
            None,
            None,
            "slots_full_started",
            json!({"supervisor": "c", "class": "light"}),
            160,
        )
        .add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "c"}),
            170,
        )
        .add(
            None,
            None,
            "supervisor_started",
            json!({"supervisor": "a"}),
            200,
        )
        .add(None, None, "slots_full_started", at("a"), 210)
        .add(
            None,
            None,
            "claim_held",
            json!({"supervisor": "a", "reason": "load"}),
            220,
        )
        .add(
            Some(1),
            None,
            "claim_deferred",
            json!({"supervisor": "a", "reason": "paths"}),
            230,
        )
        .add(
            None,
            None,
            "landing_held",
            json!({"supervisor": "a", "reason": "ci"}),
            240,
        )
        .add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "a"}),
            500,
        )
        .add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "a"}),
            800,
        )
        .add(None, None, "slots_full_started", at("f"), 810)
        .add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "f"}),
            820,
        );
    let position = log.events.len();
    log.add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "b"}),
        1100,
    )
    .add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "c"}),
        1110,
    )
    .add(
        None,
        None,
        "claim_resumed",
        json!({"supervisor": "a"}),
        1200,
    )
    .add(
        Some(1),
        None,
        "claim_deferral_ended",
        json!({"supervisor": "a", "why": "paths_free"}),
        1300,
    )
    .add(
        None,
        None,
        "landing_resumed",
        json!({"supervisor": "a"}),
        1350,
    )
    .add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "a"}),
        1400,
    );
    (log, position, 1_000)
}

/// The rows of the fold of `events` at `now_ms`, the session and queue
/// rows settled as a pass at that time marks them ([`super::settle`]).
pub(crate) fn built_and_settled(events: &[RunEvent], now_ms: i64) -> Vec<super::LedgerRow> {
    super::build(events, now_ms, "2030-01-01T00:00:00.000Z")
        .into_iter()
        .map(|mut row| {
            row.is_final =
                super::settle::settlement(&row, row.inputs_through.last(), now_ms).settles;
            row
        })
        .collect()
}

/// A history whose queue rows a cut to a position meets of every kind
/// ([`super::queue::cut`]): of every kind of queue row, ends just
/// before and after positions, a supervisor's stop and silence, a claim
/// that ends a deferral, a hold replaced by the next, and an event timed
/// before an attempt's end that comes after it (a slot's through past its
/// end event). `fill` settled rows of their own (marks, holds on the
/// deferrals of tasks of their own a claim ended) go between the
/// first attempt's end and the late event, and as many more in the
/// silence of `s2`.
pub(crate) fn cut_log(fill: usize) -> Log {
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
    let silence = 2 + crate::domain::supervisor_life::SILENT_AFTER_MS / 1000;
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

/// [`queue_across`]'s history and then `fill` of each kind of queue row
/// [`super::queue::QueueHead::forgotten_at`] leaves out once settled: a
/// mark, a stream its next baseline replaced and an alert ended, of a
/// supervisor of their own, and a last mark to end on.
pub(crate) fn forgotten_log(fill: usize) -> Log {
    let alert = json!({"supervisor": "f", "judgment": "slots", "kind": "idle"});
    let (mut log, _) = queue_across();
    for n in 0..fill {
        let secs = 100 + i64::try_from(n).unwrap_or_default();
        log.add(None, None, "mark_added", json!({"mark": "f"}), secs)
            .add(
                None,
                None,
                "live_alert_baseline",
                json!({"supervisor": "f", "version": 1}),
                secs,
            )
            .add(None, None, "live_alert_started", alert.clone(), secs)
            .add(None, None, "live_alert_ended", alert.clone(), secs);
    }
    log.add(None, None, "mark_added", json!({"mark": "end"}), 100 + 100);
    log
}

/// [`queue_across`]'s history and then `fill` of each kind of wait an event
/// ends, of a supervisor of their own (a slots full freed, a hold on the
/// claims and one on the landings resumed, a deferral of a task of its own
/// a claim ended): what [`super::queue::QueueHead::forgotten_at`] leaves out
/// once settled and before the [`crate::domain::pre_claim::rows::Since`] of
/// the tasks not settled; and a last mark to end on.
pub(crate) fn settled_waits_log(fill: usize) -> Log {
    let (mut log, _) = queue_across();
    let by = json!({"supervisor": "f"});
    let full = json!({"supervisor": "f", "class": "heavy"});
    for n in 0..fill {
        let secs = 100 + i64::try_from(n).unwrap_or_default();
        let task = 200 + i64::try_from(n).unwrap_or_default();
        let run = format!("{task:08x}-2222-4222-8222-222222222222");
        log.add(None, None, "slots_full_started", full.clone(), secs)
            .add(None, None, "slots_full_ended", full.clone(), secs)
            .add(None, None, "claim_held", by.clone(), secs)
            .add(None, None, "claim_resumed", by.clone(), secs)
            .add(None, None, "landing_held", by.clone(), secs)
            .add(None, None, "landing_resumed", by.clone(), secs)
            .add(Some(task), None, "claim_deferred", by.clone(), secs)
            .add(Some(task), Some(&run), "run_claimed", json!({}), secs);
    }
    log.add(None, None, "mark_added", json!({"mark": "end"}), 100 + 100);
    log
}

/// The ids of the events of `log` that are not fillers: the positions
/// cuts are compared at, the same in a small and a large history.
pub(crate) fn base_ats(log: &Log) -> Vec<EventId> {
    log.events
        .iter()
        .filter(|e| e.payload.get("supervisor").and_then(Value::as_str) != Some("f"))
        .filter(|e| e.payload.get("mark").and_then(Value::as_str) != Some("f"))
        .filter(|e| e.task_id.is_none_or(|task| task.as_i64() < 100))
        .map(|e| e.id)
        .collect()
}

/// One attempt and two events after its end timed before it (`A`, then
/// `B`), with an event between that reaches no attempt: the history in
/// which a through cut from `P` = `B` goes back to `A` during the replay.
pub(crate) fn two_late_log() -> Log {
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

impl Log {
    /// The push of `run` of `task` to `branch` failed at `secs`: pending.
    pub(crate) fn push_failed_to(
        &mut self,
        task: i64,
        run: &str,
        branch: &str,
        secs: i64,
    ) -> &mut Self {
        self.add(
            Some(task),
            Some(run),
            "push_failed",
            json!({"remote": "origin", "branch": branch, "error": "x"}),
            secs,
        )
        .phase(task, run, secs, Phase::PushPending, "push_failed")
    }

    /// The push of `run` of `task` to `branch` finished at `secs`, the run
    /// and the task ended with it.
    pub(crate) fn pushed_to(&mut self, task: i64, run: &str, branch: &str, secs: i64) -> &mut Self {
        self.add(
            Some(task),
            Some(run),
            "push_finished",
            json!({"remote": "origin", "branch": branch, "commit": "c"}),
            secs,
        )
        .phase(task, run, secs, Phase::Ended, "pushed")
        .completed(task, secs)
    }

    /// `task` completed at `secs`.
    pub(crate) fn completed(&mut self, task: i64, secs: i64) -> &mut Self {
        self.add(
            Some(task),
            None,
            "task_status_changed",
            json!({"from": "in_progress", "to": "completed"}),
            secs,
        )
    }
}

/// The pushes the summaries of what builds the ledger follow
/// (`builder::pushes`), after `settled` runs whose failed push the next
/// run's push closed long before (they and their tasks settle), and the
/// time (seconds) of a pass after which only the last push's are not
/// settled. Run 1 (task 1) fails its push and completes; run 3 (task 3)
/// fails one to another branch, which no push closes; run 2's landing
/// (task 2) closes run 1's; run 4 (task 4) fails one to another branch,
/// its task is canceled, and it lands again and fails again, pending again
/// (on its way meanwhile, its row that of its first pending push and its
/// task ended); run 7 (task 7) fails one to a branch and goes back to its
/// landing (its row that of the pending push), before run 6's push to that
/// branch, which closes it not; run 5 (task 5) fails one to a
/// branch, which run 6's push (task 6) closes at the end, within the grace
/// of the pass.
pub(crate) fn pushes_across(settled: usize) -> (Log, i64) {
    pushes_across_with(settled, 0)
}

/// [`pushes_across`] after `of_open_task` runs of task 90, which does not
/// end, each ended without a landing long before the pass (settled).
pub(crate) fn pushes_across_with(settled: usize, of_open_task: usize) -> (Log, i64) {
    let mut log = Log::default();
    let id =
        |n: usize, side: u8| format!("{n:08x}-{side}{side}{side}{side}-4333-8333-333333333333");
    let mut base = 0;
    for k in 0..of_open_task {
        let run = id(k, 5);
        log.worked(90, &run, base)
            .phase(90, &run, base + 700, Phase::Ended, "failed");
        base += 1000;
    }
    for k in 0..settled {
        let task = 100 + 2 * i64::try_from(k).unwrap();
        let (failed, closing) = (id(k, 3), id(k, 4));
        log.worked(task, &failed, base)
            .integrated(task, &failed, base + 700)
            .push_failed(task, &failed, base + 910)
            .completed(task, base + 910)
            .landed(task + 1, &closing, base + 1000);
        base += 2000;
    }
    let (a, b, c, d, e, f, g) = (
        run_id(1),
        run_id(2),
        run_id(3),
        run_id(4),
        run_id(5),
        run_id(6),
        run_id(7),
    );
    log.worked(1, &a, base)
        .integrated(1, &a, base + 700)
        .push_failed(1, &a, base + 910)
        .completed(1, base + 910)
        .worked(3, &c, base)
        .integrated(3, &c, base + 1000)
        .push_failed_to(3, &c, "other", base + 1210)
        .worked(4, &d, base)
        .integrated(4, &d, base + 1300)
        .push_failed_to(4, &d, "next", base + 1510)
        .landed(2, &b, base + 2000)
        .add(
            Some(4),
            None,
            "task_status_changed",
            json!({"from": "in_progress", "to": "canceled"}),
            base + 2500,
        )
        .phase(4, &d, base + 3000, Phase::Landing, "x")
        .push_failed_to(4, &d, "next", base + 3100)
        .worked(7, &g, base + 2200)
        .integrated(7, &g, base + 2900)
        .push_failed_to(7, &g, "late", base + 3150)
        .phase(7, &g, base + 3170, Phase::Landing, "x")
        .worked(5, &e, base + 3200)
        .integrated(5, &e, base + 3900)
        .push_failed_to(5, &e, "late", base + 4110)
        .completed(5, base + 4110)
        .worked(6, &f, base + 4200)
        .integrated(6, &f, base + 4900)
        .pushed_to(6, &f, "late", base + 5130);
    (log, base + 5130 + 300)
}

/// One day in seconds, for [`reads_from_a_time`].
pub(crate) const DAY: i64 = 86_400;

/// What a read of `kpi`'s periods at [`READ_AT_DAY`] must still answer of,
/// from days before every period of a day and of a week: a run landed long
/// ago, a task retried across the time, a first Execution and a claim of an
/// earlier build, old findings and baselines, supervisors stopped long ago,
/// a record after a run's end, what a plan review predicted, a planner's
/// session, a follow-up's draft, a plan's change on an old forecast's task,
/// a run that holds its slot to this day, one with an ask still open and one
/// with a session never closed.
pub(crate) const READ_AT_DAY: i64 = 200;

pub(crate) fn reads_from_a_time() -> Log {
    let mut log = log_of_a_time();
    log.events.sort_by_key(|event| event.created_at.clone());
    numbered(log)
}

/// [`reads_from_a_time`] with records of a clock that was turned back, each
/// id as it was given, none put in the order of the times; each row below
/// would be left out by what one of its id and its time says, not the other:
///
/// - A turn finished in the periods, past its run's end, and then a record
///   of the queue from long ago with a later id.
/// - A run of long ago with tokens of a time in the periods: the time its
///   Execution counts by is not the time it was recorded.
/// - A run of long ago with a record after the last one before a listed
///   period, and so past every record of the queue of the read's time: that
///   period's first event id is that record's, and the landing recorded
///   before it, of a time in that period, is in none.
/// - A landing of long ago, before the time of a day's read, and after it the
///   start of another run's landing of a time before its end, which lands in
///   the periods: its attempt starts at the end of the one before.
pub(crate) fn reads_from_a_clock_turned_back() -> Log {
    let at = |day: i64| day * DAY;
    let near = at(READ_AT_DAY - 1);
    let hour = 3600;
    // The start of the oldest period of a day's read.
    let from = at(READ_AT_DAY - 9) + (DAY - T0.rem_euclid(DAY));
    let mut landings = Log::default();
    landings
        .worked(45, &run_id(45), from - 6 * hour)
        .add(
            Some(45),
            Some(&run_id(45)),
            "integration_started",
            json!({}),
            from - 3 * hour,
        )
        .add(
            Some(45),
            Some(&run_id(45)),
            "run_integrated",
            json!({"from": "validating", "to": "integrated", "status": "integrated"}),
            from - hour,
        )
        .pushed(45, &run_id(45), from - hour + 10)
        .worked(46, &run_id(46), from - 7 * hour)
        .add(
            Some(46),
            Some(&run_id(46)),
            "integration_started",
            json!({}),
            from - 2 * hour,
        );
    let tokens = json!({"input": 11, "output": 13, "cache_read": 0, "cache_creation": 0});
    let mut late = Log::default();
    late.landed(40, &run_id(40), at(5))
        .add(
            Some(40),
            Some(&run_id(40)),
            "turn_finished",
            json!({"turn": 1, "provider": "claude", "tokens": tokens,
                   "tokens_source": "model_usage", "tokens_by_model": []}),
            near + 500,
        )
        .add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "s1"}),
            at(6),
        )
        .landed(44, &run_id(44), at(6))
        .add(
            Some(44),
            Some(&run_id(44)),
            "session_tokens",
            json!({"kind": "inbox", "at": utc_text(ms(near + 600)), "final": true,
                   "tokens": tokens, "tokens_source": "transcript", "tokens_by_model": []}),
            at(6) + 10,
        );
    let mut past_the_queue = Log::default();
    past_the_queue
        .landed(41, &run_id(41), at(7))
        .landed(43, &run_id(43), near + 1500)
        .add(
            Some(41),
            Some(&run_id(41)),
            "note_added",
            json!({}),
            at(7) + 2000,
        );
    // Run 46 lands in the periods before another landing starts.
    let mut landed = Log::default();
    landed
        .add(
            Some(46),
            Some(&run_id(46)),
            "run_integrated",
            json!({"from": "validating", "to": "integrated", "status": "integrated"}),
            near + 2500,
        )
        .pushed(46, &run_id(46), near + 2600);
    landed.events.append(&mut past_the_queue.events);
    let past_the_queue = landed;
    let mut log = log_of_a_time();
    log.events.sort_by_key(|event| event.created_at.clone());
    let index = |events: &[RunEvent], secs: i64| {
        let text = utc_text(ms(secs));
        events.partition_point(|event| event.created_at <= text)
    };
    // Each place in the events in the order of the times, the latest first.
    let last = index(&log.events, at(READ_AT_DAY - 2));
    let landing = index(&log.events, from - 6 * hour - 1800);
    let first = index(&log.events, at(10));
    log.events.splice(last..last, past_the_queue.events);
    log.events.splice(landing..landing, landings.events);
    log.events.splice(first..first, late.events);
    numbered(log)
}

/// `log` with the ids of the places of its events, and each close of a session
/// naming the last open of the kind before it.
pub(crate) fn numbered(mut log: Log) -> Log {
    for (index, event) in log.events.iter_mut().enumerate() {
        event.id = EventId::new(i64::try_from(index).unwrap() + 1);
    }
    let mut opened: HashMap<String, i64> = HashMap::new();
    for event in &mut log.events {
        let kind = event.payload["kind"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        match event.kind.as_str() {
            "session_opened" => {
                opened.insert(kind, event.id.as_i64());
            }
            "session_closed" | "session_tokens" => {
                event.payload["opened_event_id"] = json!(opened.get(&kind));
            }
            _ => {}
        }
    }
    log
}

/// The events of [`reads_from_a_time`], in the order they were added.
pub(crate) fn log_of_a_time() -> Log {
    let mut log = Log::default();
    let at = |day: i64| day * DAY;
    let tokens = json!({"input": 5, "output": 7, "cache_read": 0, "cache_creation": 0});
    let alive = |log: &mut Log, supervisor: &str, secs: i64| {
        log.add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": supervisor}),
            secs,
        );
    };
    log.add(None, None, "attributes_baseline", json!({}), 0)
        .add(None, None, "finding_baseline", json!({}), 1)
        .add(
            None,
            None,
            "finding_recorded",
            finding_record("open"),
            at(2),
        );
    log.add(
        None,
        None,
        "supervisor_started",
        json!({"supervisor": "s1", "parallel": 2}),
        2,
    )
    .add(
        None,
        None,
        "supervisor_stopped",
        json!({"supervisor": "s1"}),
        at(9),
    );
    log.add(None, None, "inbox_watcher_absent", json!({}), at(4));
    log.landed(1, &run_id(1), at(2));
    log.add(
        Some(1),
        Some(&run_id(1)),
        "turn_finished",
        json!({"turn": 1, "provider": "claude", "tokens": tokens,
               "tokens_source": "model_usage", "tokens_by_model": []}),
        at(2) + 20,
    )
    .add(
        Some(1),
        Some(&run_id(1)),
        "follow_up_registered",
        json!({"task_id": 6, "category": "defect", "index": 0}),
        at(2) + 30,
    );
    // A retry across the time.
    log.add(
        Some(2),
        None,
        "task_status_changed",
        json!({"from": "draft", "to": "ready"}),
        at(3),
    );
    log.worked(2, &run_id(2), at(3));
    log.add(
        Some(2),
        Some(&run_id(2)),
        "validation_finished",
        json!({"status": "failed"}),
        at(3) + 700,
    );
    log.phase(2, &run_id(2), at(3) + 700, Phase::Ended, "failed");
    log.add(
        None,
        None,
        "forecast_recorded",
        json!({"at_secs": T0 + at(3), "method": 1, "goals": [],
               "tasks": [{"id": 2, "p50_secs": 3600, "p90_secs": 7200},
                         {"id": 1, "p50_secs": 3600, "p90_secs": 7200}]}),
        at(3),
    );
    log.add(
        Some(1),
        None,
        "task_priority_changed",
        json!({"priority": "high"}),
        at(4),
    );
    // A record after the end of a run of long ago.
    log.landed(3, &run_id(3), at(4));
    // What a plan review predicted of a task claimed long after, and the
    // planner's session.
    log.add(
        Some(4),
        None,
        "task_weight_predicted",
        json!({"plan_review_id": 9,
               "prediction": {"nature": "mechanical", "expected_output_tokens": 100}}),
        at(8),
    )
    .add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 5}),
        at(8),
    )
    .add(
        None,
        None,
        "session_closed",
        json!({"kind": "runtime_planner", "planner_id": 5, "session_id": "p-5"}),
        at(9),
    )
    .add(
        None,
        None,
        "plan_review_finished",
        json!({"plan_review_id": 9}),
        at(9),
    );
    // A draft of long ago whose follow-up was registered on run 1.
    log.add(
        Some(6),
        None,
        "task_created",
        json!({"status": "draft"}),
        at(2) + 31,
    );
    // A run that never ended, of a task that did: it holds its slot to now.
    log.add(Some(7), None, "task_created", json!({}), at(5))
        .add(
            Some(7),
            Some(&run_id(7)),
            "run_claimed",
            json!({"from": "ready", "to": "in_progress", "provider": "claude"}),
            at(5),
        )
        .add(
            Some(7),
            None,
            "task_status_changed",
            json!({"from": "in_progress", "to": "canceled"}),
            at(5) + 9,
        );
    // A run ended with an ask still open, while the watcher was away.
    log.landed(8, &run_id(8), at(5));
    log.add(
        Some(8),
        Some(&run_id(8)),
        "ask_opened",
        json!({"ask_id": 77, "kind": "worker_question"}),
        at(5) + 940,
    );
    log.legacy(9, &run_id(9), at(5));
    // A run ended with its session never closed: it counts as open to now.
    log.landed(10, &run_id(10), at(5));
    log.add(
        Some(10),
        Some(&run_id(10)),
        "session_opened",
        json!({"kind": "resume", "provider": "claude"}),
        at(5) + 1,
    );
    // From here on is the periods'.
    for day in 10..=READ_AT_DAY {
        alive(&mut log, "s2", at(day));
    }
    log.add(
        None,
        None,
        "supervisor_started",
        json!({"supervisor": "s2", "parallel": 4}),
        at(10),
    );
    let near = at(READ_AT_DAY - 1);
    log.add(
        Some(3),
        Some(&run_id(3)),
        "note_added",
        json!({"late": true}),
        near,
    );
    log.add(None, None, "inbox_watcher_returned", json!({}), near);
    log.landed(2, &run_id(12), near + 100);
    log.add(
        Some(2),
        None,
        "task_status_changed",
        json!({"from": "in_progress", "to": "completed"}),
        near + 1100,
    );
    log.landed(4, &run_id(14), near + 200);
    log.add(
        Some(6),
        None,
        "task_status_changed",
        json!({"from": "draft", "to": "ready"}),
        near + 300,
    );
    log.landed(6, &run_id(16), near + 400);
    log.add(
        Some(6),
        None,
        "task_status_changed",
        json!({"from": "in_progress", "to": "completed"}),
        near + 1400,
    );
    let from_near = utc_text(ms(near));
    for event in &mut log.events {
        if event.kind.as_str() == "run_claimed" {
            let build = if event.created_at >= from_near {
                "v2"
            } else {
                "v1"
            };
            event.payload["dagq_version"] = json!(build);
        }
    }
    log
}

/// The rows of [`reads_from_a_time`] at [`READ_AT_DAY`], with a run row of a
/// time missing each way and the host's load of long ago and of now.
pub(crate) fn rows_read_from_a_time() -> Vec<super::LedgerRow> {
    rows_at_the_read(&reads_from_a_time().events)
}

/// The rows of [`reads_from_a_clock_turned_back`], as
/// [`rows_read_from_a_time`]'s.
pub(crate) fn rows_read_from_a_clock_turned_back() -> Vec<super::LedgerRow> {
    rows_at_the_read(&reads_from_a_clock_turned_back().events)
}

fn rows_at_the_read(events: &[RunEvent]) -> Vec<super::LedgerRow> {
    use crate::domain::host_metrics::{HOST_NODE, HostSample, NodeSample, SampleSource};
    let now = T0 + READ_AT_DAY * DAY;
    let mut rows = super::build(events, now * 1000, "2030-01-01T00:00:00.000Z");
    let no_time = |run: u8| {
        let mut row = run_row(&run_id(run), None, Vec::new());
        row.started_at = None;
        row.ended_at = None;
        row
    };
    let mut no_start = no_time(30);
    no_start.ended_at = Some(ms(DAY));
    let mut no_end = no_time(31);
    no_end.started_at = Some(ms(DAY));
    rows.extend([no_start, no_end]);
    let sample = |id: i64, unix: i64| {
        let mut sample = HostSample::new(unix);
        sample.set("load1", Some(1.5));
        NodeSample {
            resources: Default::default(),
            id,
            node_id: HOST_NODE.to_owned(),
            local_day: unix / DAY,
            source: SampleSource::Sampled,
            sample,
        }
    };
    rows.extend(super::node::node_rows(
        &[sample(1, T0 + DAY), sample(2, now - 3600)],
        "2030-01-01T00:00:00.000Z",
    ));
    rows
}
