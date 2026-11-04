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
