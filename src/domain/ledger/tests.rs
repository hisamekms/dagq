use serde_json::json;

use super::fold::{self, DEPENDENCY_WAIT, PLAN_REVIEW, READY, RunLife, TaskFold, UNATTRIBUTED};
use super::read::read;
use super::*;
use crate::domain::marks::utc_text;
use crate::domain::pre_claim::{IntervalKind, SlotClass};
use crate::domain::run_phase::{Attempt, AttemptKind, Phase, PhaseChange};
use crate::domain::{RunId, stats};

const T0: i64 = 1_900_000_000;
const A: &str = "11111111-1111-4111-8111-111111111111";
const B: &str = "22222222-2222-4222-8222-222222222222";
const TASK: i64 = 7;

fn ms(secs: i64) -> i64 {
    (T0 + secs) * 1000
}

/// The events of a queue as a test lays them down, with ids in order.
#[derive(Default)]
struct Log {
    events: Vec<RunEvent>,
}

impl Log {
    fn add(&mut self, run: Option<&str>, kind: &str, payload: Value, secs: i64) -> &mut Self {
        let id = EventId::new(i64::try_from(self.events.len()).unwrap() + 1);
        self.events.push(RunEvent {
            id,
            task_id: Some(TaskId::new(TASK)),
            goal_id: None,
            run_id: run.map(|run| RunId::new(run).unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: utc_text(ms(secs)),
            actor: None,
        });
        self
    }

    fn at(&mut self, secs: i64, kind: &str, payload: Value) -> &mut Self {
        self.add(Some(A), kind, payload, secs)
    }

    fn phase(&mut self, secs: i64, phase: Phase, attempt: Attempt, cause: &str) -> &mut Self {
        self.at(
            secs,
            "run_phase_changed",
            PhaseChange::new(phase, attempt, cause).payload(),
        )
    }

    fn first(&mut self, secs: i64, phase: Phase) -> &mut Self {
        self.phase(secs, phase, Attempt::FIRST, "x")
    }

    fn claim(&mut self, secs: i64) -> &mut Self {
        self.at(
            secs,
            "run_claimed",
            json!({"from": "ready", "to": "in_progress", "provider": "claude"}),
        )
    }

    fn of(&self, run: &str) -> Vec<RunEvent> {
        self.events
            .iter()
            .filter(|event| event.run_id.as_ref().is_some_and(|id| id.as_str() == run))
            .cloned()
            .collect()
    }

    fn pushes(&self) -> Vec<RunEvent> {
        self.events
            .iter()
            .filter(|event| event.kind == "push_finished")
            .cloned()
            .collect()
    }

    fn fold(&self, now: i64) -> RunLife {
        fold::fold_run(&self.of(A), &self.pushes(), ms(now)).expect("a folded run")
    }
}

/// The intervals follow each other from the start with no gap or overlap,
/// up to the end (or `now` while not settled), and add up to it.
fn assert_covers(life: &RunLife, now: i64) {
    let mut cursor = life.start;
    for interval in &life.intervals {
        assert_eq!(interval.start, cursor, "{interval:?}");
        assert!(interval.end >= interval.start, "{interval:?}");
        cursor = interval.end;
    }
    let end = life.end.unwrap_or(ms(now));
    assert_eq!(cursor, end);
    let sum: i64 = life.intervals.iter().map(fold::Interval::millis).sum();
    assert_eq!(sum, end - life.start);
    assert_eq!(
        Totals::of(&life.intervals).all.duration_ms,
        end - life.start
    );
}

fn phases(life: &RunLife) -> Vec<&str> {
    life.intervals.iter().map(|i| i.phase.as_str()).collect()
}

/// The way to a landing every run shares: claim at 0, provisioning at 2
/// (the 2 seconds before it unattributed), the worker from 10, its receipt
/// validated from 600 to 660.
fn worked(log: &mut Log) -> &mut Log {
    log.claim(0)
        .first(2, Phase::Provisioning)
        .at(5, "agent_started", json!({}))
        .first(10, Phase::Worker)
        .at(70, "first_commit_observed", json!({}))
        .at(600, "receipt_observed", json!({}))
        .first(600, Phase::Validating)
        .at(660, "validation_finished", json!({}))
}

fn landed(log: &mut Log, secs: i64) -> &mut Log {
    log.first(secs, Phase::Landing)
        .at(secs + 200, "run_integrated", json!({}))
        .first(secs + 200, Phase::Push { in_slot: true })
}

#[test]
fn a_run_landed_first_time_ends_with_its_push() {
    let mut log = Log::default();
    worked(&mut log)
        .first(660, Phase::Review)
        .first(900, Phase::AwaitingSlot);
    landed(&mut log, 1000)
        .at(
            1230,
            "push_finished",
            json!({"remote": "origin", "branch": "main"}),
        )
        .phase(1230, Phase::Ended, Attempt::FIRST, "pushed");
    let life = log.fold(5000);
    assert_covers(&life, 5000);
    assert_eq!(
        (life.end, life.outcome.as_deref()),
        (Some(ms(1230)), Some("pushed"))
    );
    assert_eq!(
        phases(&life),
        [
            UNATTRIBUTED,
            "provisioning",
            "worker",
            "validating",
            "review",
            "awaiting_slot",
            "landing",
            "push"
        ]
    );
    let totals = Totals::of(&life.intervals);
    assert_eq!(totals.unattributed_ms, 2000);
    // Every phase but the unattributed start held a slot.
    assert_eq!(totals.all.slot_ms, ms(1230) - ms(2));
    assert_eq!(totals.by_holds["landing_slot"].duration_ms, 200_000);
    assert_eq!(totals.attempts, 1);

    let row = run_row(A, &log.of(A), &[], ms(5000), "built");
    let values = read(&row);
    assert!(!values.legacy);
    assert_eq!(
        (
            values.work,
            values.validate,
            values.wait_to_land,
            values.startup
        ),
        (Some(600), Some(60), Some(540), Some(65))
    );
    let land = values.land_phases.unwrap();
    assert_eq!(land.push, Some(30));
    assert_eq!(land.secs.iter().sum::<i64>(), 540);
}

#[test]
fn a_skipped_push_ends_the_run() {
    let mut log = Log::default();
    worked(&mut log);
    landed(&mut log, 700)
        .at(910, "push_skipped", json!({"reason": "no remote"}))
        .phase(910, Phase::Ended, Attempt::FIRST, "push_skipped");
    let life = log.fold(5000);
    assert_covers(&life, 5000);
    assert_eq!(
        (life.end, life.outcome.as_deref()),
        (Some(ms(910)), Some("push_skipped"))
    );
}

fn push_failed(log: &mut Log) -> &mut Log {
    worked(log);
    landed(log, 700)
        .at(
            910,
            "push_failed",
            json!({"remote": "origin", "branch": "main"}),
        )
        .phase(910, Phase::PushPending, Attempt::FIRST, "push_failed")
}

/// A pending push is closed by a later push to the same remote and branch,
/// another run's; a push to another branch does not close it.
#[test]
fn a_pending_push_ends_at_a_later_push_of_the_same_branch() {
    let mut log = Log::default();
    push_failed(&mut log)
        .add(
            Some(B),
            "push_finished",
            json!({"remote": "origin", "branch": "other"}),
            1000,
        )
        .add(
            Some(B),
            "push_finished",
            json!({"remote": "origin", "branch": "main"}),
            2000,
        );
    let life = log.fold(9000);
    assert_covers(&life, 9000);
    assert_eq!(
        (life.end, life.outcome.as_deref()),
        (Some(ms(2000)), Some("pushed"))
    );
    assert_eq!(life.closed_by.as_deref(), Some(B));
    assert_eq!(life.inputs_through, log.events.last().unwrap().id);
    let pending = life.intervals.last().unwrap();
    assert_eq!(
        (pending.phase.as_str(), pending.millis()),
        ("push_pending", 1_090_000)
    );
    assert_eq!(
        Totals::of(&life.intervals).by_blocker["human"].duration_ms,
        1_090_000
    );
}

/// Without a later push the run is not settled: it reads up to now.
#[test]
fn a_pending_push_with_no_later_push_is_not_settled() {
    let mut log = Log::default();
    push_failed(&mut log);
    let life = log.fold(3000);
    assert_covers(&life, 3000);
    assert_eq!((life.end, life.outcome), (None, None));
    assert_eq!(life.intervals.last().unwrap().end, ms(3000));
    let row = run_row(A, &log.of(A), &[], ms(3000), "built");
    assert_eq!(row.ended_at, None);
}

/// A running run folds up to now and is not settled.
#[test]
fn a_running_run_folds_up_to_now() {
    let mut log = Log::default();
    log.claim(0)
        .first(0, Phase::Provisioning)
        .first(10, Phase::Worker);
    let life = log.fold(100);
    assert_covers(&life, 100);
    assert_eq!(life.end, None);
    assert_eq!(phases(&life), ["provisioning", "worker"]);
    assert_eq!(life.intervals[1].millis(), 90_000);
}

#[test]
fn a_revise_is_its_own_attempt() {
    let revise = Attempt::of(AttemptKind::Revise, 1);
    let mut log = Log::default();
    worked(&mut log)
        .first(660, Phase::Review)
        .phase(800, Phase::Revise, revise, "revise_requested")
        .phase(1000, Phase::Validating, revise, "receipt_observed")
        .phase(1050, Phase::Review, revise, "validation_finished")
        .phase(1200, Phase::LandingQueue, revise, "review_finished")
        .phase(1300, Phase::Landing, revise, "x")
        .phase(
            1400,
            Phase::Push { in_slot: true },
            revise,
            "run_integrated",
        )
        .phase(1420, Phase::Ended, revise, "pushed");
    let life = log.fold(5000);
    assert_covers(&life, 5000);
    let totals = Totals::of(&life.intervals);
    assert_eq!(totals.attempts, 2);
    assert_eq!(totals.by_attempt["revise.1"].duration_ms, 620_000);
    // The wait for the landing turn holds no slot.
    assert_eq!(totals.by_attempt["revise.1"].slot_ms, 520_000);
    let land = read(&run_row(A, &log.of(A), &[], ms(5000), "built"))
        .land_phases
        .unwrap();
    let secs = |name| {
        land.secs[stats::landing::PHASES
            .iter()
            .position(|p| *p == name)
            .unwrap()]
    };
    assert_eq!(
        (secs("revise"), secs("landing_queue"), secs("verify")),
        (200, 100, 100)
    );
}

#[test]
fn a_resume_waits_then_works_again() {
    let resume = Attempt::of(AttemptKind::Resume, 1);
    let mut log = Log::default();
    worked(&mut log)
        .first(660, Phase::NeedsSession)
        .phase(2000, Phase::Resume, resume, "resume_started")
        .phase(2300, Phase::Validating, resume, "receipt_observed")
        .phase(2350, Phase::LandingQueue, resume, "x");
    let life = log.fold(2500);
    assert_covers(&life, 2500);
    let totals = Totals::of(&life.intervals);
    assert_eq!(totals.by_blocker["queue"].duration_ms, 1_340_000 + 150_000);
    assert_eq!(totals.by_attempt["resume.1"].duration_ms, 500_000);
    assert_eq!(life.end, None);
}

/// The wait for an `approve_landing` answer is a person's and holds no
/// slot: it is `ask` under today's names.
#[test]
fn an_approve_landing_ask_is_a_persons_wait() {
    let mut log = Log::default();
    worked(&mut log)
        .first(660, Phase::LandingAnswer)
        .at(700, "ask_opened", json!({"kind": "approve_landing"}))
        .at(3660, "ask_answered", json!({}))
        .first(3660, Phase::LandingQueue);
    landed(&mut log, 3700).phase(3910, Phase::Ended, Attempt::FIRST, "pushed");
    let life = log.fold(9000);
    assert_covers(&life, 9000);
    let totals = Totals::of(&life.intervals);
    assert_eq!(
        totals.by_blocker["human"],
        Measure {
            duration_ms: 3_000_000,
            slot_ms: 0
        }
    );
    let values = read(&run_row(A, &log.of(A), &[], ms(9000), "built"));
    let land = values.land_phases.unwrap();
    assert_eq!(
        land.secs[stats::landing::PHASES
            .iter()
            .position(|p| *p == "ask")
            .unwrap()],
        3000
    );
}

/// A conflict at the landing sends the run back to its session, and the
/// wait for the landing turn names the run that held the landing slot.
#[test]
fn a_landing_conflict_is_its_own_attempt_and_names_who_held_the_slot() {
    let conflict = Attempt::of(AttemptKind::Conflict, 1);
    let mut log = Log::default();
    worked(&mut log)
        .first(660, Phase::Landing)
        .phase(700, Phase::Revise, conflict, "conflict_precheck")
        .phase(900, Phase::Validating, conflict, "receipt_observed")
        .at(
            950,
            "run_phase_changed",
            PhaseChange::new(Phase::LandingQueue, conflict, "x")
                .blocked_by(Some(B.to_owned()))
                .payload(),
        );
    landed(&mut log, 1100).phase(1310, Phase::Ended, conflict, "pushed");
    let life = log.fold(5000);
    assert_covers(&life, 5000);
    let queue = life
        .intervals
        .iter()
        .find(|i| i.phase == "landing_queue")
        .unwrap();
    assert_eq!(queue.blocked_by.as_deref(), Some(B));
    assert_eq!(
        Totals::of(&life.intervals).by_attempt["conflict.1"].duration_ms,
        400_000
    );
    let land = read(&run_row(A, &log.of(A), &[], ms(5000), "built"))
        .land_phases
        .unwrap();
    assert_eq!(
        land.secs[stats::landing::PHASES
            .iter()
            .position(|p| *p == "conflict")
            .unwrap()],
        200
    );
}

#[test]
fn a_canceled_run_ends_canceled() {
    let mut log = Log::default();
    log.claim(0)
        .first(0, Phase::Provisioning)
        .first(10, Phase::Worker)
        .phase(300, Phase::Ended, Attempt::FIRST, "canceled");
    let life = log.fold(5000);
    assert_covers(&life, 5000);
    assert_eq!(
        (life.end, life.outcome.as_deref()),
        (Some(ms(300)), Some("canceled"))
    );
    // A record whose tags it cannot read accounts for nothing.
    let mut odd = Log::default();
    odd.claim(0)
        .at(0, "run_phase_changed", json!({"phase": "new_phase"}));
    let life = odd.fold(50);
    assert_covers(&life, 50);
    assert_eq!(Totals::of(&life.intervals).unattributed_ms, 50_000);
}

fn wait(kind: IntervalKind, task: Option<i64>, from: i64, to: i64) -> PreClaimInterval {
    PreClaimInterval {
        id: EventId::new(1000 + from),
        kind,
        supervisor: Some("s1".to_owned()),
        task_id: task.map(TaskId::new),
        reason: None,
        class: (kind == IntervalKind::SlotsFull).then_some(SlotClass::Heavy),
        started_at: utc_text(ms(from)),
        ended_at: Some(utc_text(ms(to))),
        end: None,
        ended_by: None,
    }
}

fn task_status(log: &mut Log, secs: i64, from: &str, to: &str) {
    log.add(
        None,
        "task_status_changed",
        json!({"from": from, "to": to}),
        secs,
    );
}

/// A task from its creation: a draft, under plan review, waiting for its
/// dependencies (until the `ready_at` its claim recorded), ready with the
/// supervisors' waits laid over that time, then its run to the end of its
/// push.
#[test]
fn a_task_folds_its_waits_before_the_claim_and_its_run() {
    let mut log = Log::default();
    log.add(None, "task_created", json!({}), 0);
    task_status(&mut log, 100, "draft", "submitted");
    task_status(&mut log, 400, "submitted", "ready");
    log.at(
        1000,
        "run_claimed",
        json!({"from": "ready", "to": "in_progress", "ready_at": utc_text(ms(700))}),
    )
    .first(1000, Phase::Provisioning)
    .first(1010, Phase::Worker)
    .first(1500, Phase::Landing)
    .at(1600, "run_integrated", json!({}))
    .first(1600, Phase::Push { in_slot: true });
    task_status(&mut log, 1600, "in_progress", "completed");
    log.at(1650, "push_finished", json!({}))
        .phase(1650, Phase::Ended, Attempt::FIRST, "pushed");
    let waits = [
        wait(IntervalKind::SlotsFull, None, 800, 900),
        wait(IntervalKind::ClaimHold, None, 850, 950),
        wait(IntervalKind::ClaimDeferral, Some(TASK), 600, 750),
        wait(IntervalKind::ClaimDeferral, Some(99), 700, 1000),
        // Before the task was ready: not its wait.
        wait(IntervalKind::SlotsFull, None, 200, 300),
    ];
    let TaskFold::Life(life) =
        fold::fold_task(TaskId::new(TASK), &log.events, &[], &waits, ms(9000))
    else {
        panic!("a folded task");
    };
    assert_eq!((life.start, life.end), (ms(0), Some(ms(1650))));
    assert_eq!(life.outcome.as_deref(), Some("completed"));
    let names: Vec<_> = life.intervals.iter().map(|i| i.phase.as_str()).collect();
    assert_eq!(
        names,
        [
            "draft",
            PLAN_REVIEW,
            DEPENDENCY_WAIT,
            READY,
            "provisioning",
            "worker",
            "landing",
            "push"
        ]
    );
    let mut cursor = life.start;
    for interval in &life.intervals {
        assert_eq!(interval.start, cursor);
        cursor = interval.end;
    }
    assert_eq!(cursor, ms(1650));
    assert!(
        life.intervals[4..]
            .iter()
            .all(|i| i.run_id.as_deref() == Some(A))
    );
    assert_eq!(life.ready_at, Some(ms(700)));
    assert_eq!(life.first_claimed, Some(ms(1000)));
    assert_eq!(
        life.overlay_millis,
        BTreeMap::from([
            ("slots_full".to_owned(), 100_000),
            ("claim_hold".to_owned(), 100_000),
            ("claim_deferral".to_owned(), 50_000),
        ])
    );
    let totals = Totals::of(&life.intervals);
    assert_eq!(totals.all.duration_ms, ms(1650) - ms(0));
    assert_eq!(totals.unattributed_ms, 0);
    assert_eq!(totals.by_blocker["queue"].duration_ms, 600_000);

    let row = task_row(
        TaskId::new(TASK),
        &log.events,
        &[],
        &waits,
        ms(9000),
        "built",
    );
    assert_eq!(row.kind, LedgerKind::Task);
    assert_eq!(row.detail["overlay_ms"]["slots_full"], 100_000);
    let values = read(&row);
    assert_eq!(values.breakdown.unwrap().all.duration_ms, 1_650_000);
    assert_eq!(values.work, None);
}

/// A task not yet claimed is ready up to now; one with a run from before
/// the switch is not folded.
#[test]
fn a_waiting_task_folds_up_to_now_and_a_legacy_run_leaves_the_task_legacy() {
    let mut log = Log::default();
    log.add(None, "task_created", json!({}), 0);
    task_status(&mut log, 100, "submitted", "ready");
    let TaskFold::Life(life) = fold::fold_task(TaskId::new(TASK), &log.events, &[], &[], ms(500))
    else {
        panic!("a folded task");
    };
    assert_eq!(life.end, None);
    let names: Vec<_> = life
        .intervals
        .iter()
        .map(|i| (i.phase.as_str(), i.millis()))
        .collect();
    assert_eq!(names, [(PLAN_REVIEW, 100_000), (READY, 400_000)]);

    log.claim(600)
        .at(700, "receipt_observed", json!({}))
        .at(800, "run_integrated", json!({}));
    let row = task_row(TaskId::new(TASK), &log.events, &[], &[], ms(900), "built");
    assert!(row.is_legacy());
    assert_eq!(row.legacy.unwrap()["runs"], json!([A]));
}

/// The events of a run from before the switch: no `run_phase_changed`.
fn legacy_events() -> Log {
    let mut log = Log::default();
    log.at(
        0,
        "run_claimed",
        json!({"provider": "claude", "dagq_version": "0.9", "claude_version": "2.1",
               "worker_mode": "headless", "parallel": 3, "slots": 4, "load_avg": 1.5,
               "model": "opus", "effort": "high", "group": "b", "rustc_release": "1.98"}),
    )
    .at(5, "agent_started", json!({}))
    .at(70, "first_commit_observed", json!({}))
    .at(600, "receipt_observed", json!({}))
    .at(
        660,
        "validation_finished",
        json!({"status": "awaiting_integration"}),
    )
    .at(700, "review_started", json!({}))
    .at(900, "landing_queued", json!({"via": "exit"}))
    .at(1000, "integration_started", json!({}))
    .at(1100, "integration_rebased", json!({}))
    .at(1200, "run_integrated", json!({}))
    .at(1230, "push_finished", json!({}));
    log
}

/// A legacy row holds `stats`' own values for the run, frozen: it reads
/// back under the same names, marked; it is built again as the events go
/// on only so that its records follow, its values kept by the store.
#[test]
fn a_run_before_the_switch_is_a_frozen_legacy_row_of_stats_values() {
    let log = legacy_events();
    let events = log.of(A);
    assert!(fold::fold_run(&events, &[], ms(5000)).is_none());
    let row = run_row(A, &events, &[], ms(5000), "built");
    assert!(row.is_legacy());
    assert!(row.intervals.is_empty());
    let stats = stats::run_stats(&events).remove(0);
    let values = read(&row);
    assert!(values.legacy);
    assert_eq!(values.breakdown, None);
    assert_eq!(
        (
            values.work,
            values.validate,
            values.wait_to_land,
            values.startup
        ),
        (
            stats.work,
            stats.validate,
            stats.wait_to_land,
            stats.startup
        )
    );
    assert!(stats.land_phases.is_some());
    assert_eq!(values.land_phases, stats.land_phases);
    assert_eq!(
        serde_json::to_value(&values.land_phases).unwrap(),
        serde_json::to_value(&stats.land_phases).unwrap()
    );

    // Built again for a later event or a new fold, so that its records
    // follow; the store keeps its values.
    let last = row.inputs_through.events;
    assert!(!rebuilds(Some(&row), last, None));
    assert!(rebuilds(Some(&row), EventId::new(last.as_i64() + 1), None));
    let mut old = row.clone();
    old.ledger_v = LEDGER_V - 1;
    assert!(rebuilds(Some(&old), last, None));
}

#[test]
fn a_new_row_is_built_again_for_a_new_fold_or_a_later_event() {
    let mut log = Log::default();
    log.claim(0).first(0, Phase::Provisioning);
    let row = run_row(A, &log.of(A), &[], ms(10), "built");
    let last = row.inputs_through.events;
    assert!(rebuilds(None, last, None));
    assert!(!rebuilds(Some(&row), last, None));
    assert!(rebuilds(Some(&row), EventId::new(last.as_i64() + 1), None));
    let mut old = row.clone();
    old.ledger_v = LEDGER_V + 1;
    assert!(rebuilds(Some(&old), last, None));
}

/// The reading names the same values in the same shape for a new and a
/// legacy row; only the mark and the breakdown tell them apart.
#[test]
fn new_and_legacy_rows_read_under_the_same_names() {
    let legacy = read(&run_row(A, &legacy_events().of(A), &[], ms(5000), "built"));
    let mut log = Log::default();
    worked(&mut log);
    landed(&mut log, 700).phase(910, Phase::Ended, Attempt::FIRST, "pushed");
    let new = read(&run_row(A, &log.of(A), &[], ms(5000), "built"));
    let keys = |values: &read::LedgerValues| {
        serde_json::to_value(values)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(keys(&legacy), keys(&new));
    let land_keys = |values: &read::LedgerValues| {
        serde_json::to_value(&values.land_phases)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(land_keys(&legacy), land_keys(&new));
    assert!(legacy.legacy && !new.legacy);
    assert!(new.breakdown.is_some());
}

/// The claim's attributes are carried whatever their keys.
#[test]
fn the_claims_attributes_are_carried_with_keys_unknown() {
    let mut log = Log::default();
    log.at(
        0,
        "run_claimed",
        json!({"attributes": {"build": "1.2", "instructions_prompt": "abc", "zz.new_key": "v"}}),
    )
    .first(0, Phase::Provisioning);
    let row = run_row(A, &log.of(A), &[], ms(10), "built");
    let values = read(&row);
    assert_eq!(
        Value::Object(values.attributes),
        json!({"build": "1.2", "instructions_prompt": "abc", "zz.new_key": "v"})
    );
}

/// A legacy run's attributes are the keys `run_claimed` would have held,
/// from its measures; one whose claim recorded them keeps them as they are.
#[test]
fn a_legacy_run_has_the_attribute_keys_from_its_measures() {
    let log = legacy_events();
    let row = run_row(A, &log.of(A), &[], ms(5000), "built");
    let mut expected = claim_facts::attributes(log.events[0].payload.as_object().unwrap());
    // Its measures do not keep the claim's own provider.
    expected.remove("claim_provider");
    assert_eq!(row.attributes, expected);
    assert_eq!(row.attributes["build"], "0.9");
    assert_eq!(row.attributes["claim_model"], "opus");

    let mut recorded = legacy_events();
    recorded.events[0].payload["attributes"] = json!({"build": "0.9", "zz": "kept"});
    let row = run_row(A, &recorded.of(A), &[], ms(5000), "built");
    assert_eq!(
        Value::Object(row.attributes),
        json!({"build": "0.9", "zz": "kept"})
    );
}

/// The facts after the claim of a folded run, and of the same events as a
/// legacy run, are what `stats`' measures make of them.
fn assert_after_claim(log: &Log, expected: AfterClaim) {
    let events = log.of(A);
    let measures = stats::run_stats(&events).remove(0).measures;
    assert_eq!(
        AfterClaim {
            last_provider: measures.actual_provider.clone(),
            on_codex: measures.on_codex,
            codex_model: measures.worker_model.clone().filter(|_| measures.on_codex),
        },
        expected
    );
    let mut folded = Log {
        events: events.clone(),
    };
    folded.first(1, Phase::Provisioning);
    assert_eq!(folded.fold(100).after_claim, expected);
    let mut ended = Log { events };
    ended.at(50, "run_integrated", json!({}));
    let legacy = run_row(A, &ended.events, &[], ms(100), "built");
    assert!(legacy.is_legacy());
    assert_eq!(legacy.after_claim, Some(expected));
}

fn facts(last: &str, on_codex: bool, model: Option<&str>) -> AfterClaim {
    AfterClaim {
        last_provider: Some(last.to_owned()),
        on_codex,
        codex_model: model.map(str::to_owned),
    }
}

#[test]
fn the_facts_after_the_claim_are_stats_measures() {
    // A move at the claim, recorded before it.
    let mut moved = Log::default();
    moved
        .at(
            0,
            "provider_switched",
            json!({"from": "claude", "to": "codex"}),
        )
        .at(
            0,
            "run_claimed",
            json!({"provider": "codex", "requested_provider": "claude", "model": "opus"}),
        )
        .at(
            10,
            "turn_finished",
            json!({"provider": "codex", "model": "gpt-5", "outcome": "succeeded"}),
        );
    assert_after_claim(&moved, facts("codex", true, Some("gpt-5")));

    // Claude, then Codex: the model is Codex's, not the claim's.
    let mut to_codex = Log::default();
    to_codex
        .at(
            0,
            "run_claimed",
            json!({"provider": "claude", "model": "opus"}),
        )
        .at(
            10,
            "turn_finished",
            json!({"provider": "claude", "model": "opus", "outcome": "succeeded"}),
        )
        .at(
            20,
            "provider_switched",
            json!({"from": "claude", "to": "codex"}),
        )
        .at(
            30,
            "turn_finished",
            json!({"provider": "codex", "model": "gpt-5.5", "outcome": "succeeded"}),
        )
        .at(
            40,
            "turn_finished",
            json!({"provider": "codex", "model": "gpt-6", "outcome": "succeeded"}),
        );
    assert_after_claim(&to_codex, facts("codex", true, Some("gpt-5.5")));

    // Codex, then Claude: still a Codex run, last on Claude.
    let mut to_claude = Log::default();
    to_claude
        .at(
            0,
            "run_claimed",
            json!({"provider": "codex", "model": "gpt-5"}),
        )
        .at(
            10,
            "turn_finished",
            json!({"provider": "codex", "model": "gpt-5-codex", "outcome": "failed"}),
        )
        .at(
            20,
            "provider_switched",
            json!({"from": "codex", "to": "claude"}),
        )
        .at(
            30,
            "turn_finished",
            json!({"provider": "claude", "model": "opus", "outcome": "succeeded"}),
        );
    assert_after_claim(&to_claude, facts("claude", true, Some("gpt-5-codex")));

    // A turn with no provider ran on the one the run was on.
    let mut unnamed = Log::default();
    unnamed
        .at(
            0,
            "run_claimed",
            json!({"provider": "claude", "model": "opus"}),
        )
        .at(
            5,
            "provider_switched",
            json!({"from": "claude", "to": "codex"}),
        )
        .at(
            10,
            "turn_finished",
            json!({"model": "o4", "outcome": "succeeded"}),
        );
    assert_after_claim(&unnamed, facts("codex", true, Some("o4")));

    // Never on Codex.
    let mut claude = Log::default();
    claude
        .at(
            0,
            "run_claimed",
            json!({"provider": "claude", "model": "opus"}),
        )
        .at(
            10,
            "turn_finished",
            json!({"model": "opus", "outcome": "succeeded"}),
        );
    assert_after_claim(&claude, facts("claude", false, None));
}

/// A run on its way whose first record is not written yet is not legacy:
/// it is unattributed up to now, and folds once its records come.
#[test]
fn a_run_with_no_record_yet_is_unattributed_not_legacy() {
    let mut log = Log::default();
    log.claim(0).at(5, "agent_started", json!({}));
    let life = log.fold(30);
    assert_covers(&life, 30);
    assert_eq!(phases(&life), [UNATTRIBUTED]);
    let row = run_row(A, &log.of(A), &[], ms(30), "built");
    assert!(!row.is_legacy());
    assert!(rebuilds(
        Some(&row),
        EventId::new(row.inputs_through.events.as_i64() + 1),
        None
    ));
}

/// A row left pending on its push is built again when a later push
/// arrives, though the run itself records nothing more.
#[test]
fn a_pending_push_row_is_built_again_after_a_later_push() {
    let mut log = Log::default();
    push_failed(&mut log);
    let row = run_row(A, &log.of(A), &[], ms(3000), "built");
    let last = row.inputs_through.events;
    assert!(!rebuilds(Some(&row), last, Some(last)));
    assert!(rebuilds(
        Some(&row),
        last,
        Some(EventId::new(last.as_i64() + 1))
    ));
    let mut ended = Log::default();
    worked(&mut ended).phase(700, Phase::Ended, Attempt::FIRST, "canceled");
    let row = run_row(A, &ended.of(A), &[], ms(3000), "built");
    let last = row.inputs_through.events;
    assert!(!rebuilds(
        Some(&row),
        last,
        Some(EventId::new(last.as_i64() + 1))
    ));
}

/// Status moves timed out of their order do not overlap.
#[test]
fn task_statuses_out_of_order_do_not_overlap() {
    let mut log = Log::default();
    log.add(None, "task_created", json!({}), 0);
    task_status(&mut log, 10, "draft", "submitted");
    task_status(&mut log, 5, "submitted", "ready");
    task_status(&mut log, 20, "ready", "canceled");
    let TaskFold::Life(life) = fold::fold_task(TaskId::new(TASK), &log.events, &[], &[], ms(100))
    else {
        panic!("a folded task");
    };
    assert_eq!(life.end, Some(ms(20)));
    assert_eq!(Totals::of(&life.intervals).all.duration_ms, 20_000);
    let mut cursor = life.start;
    for interval in &life.intervals {
        assert_eq!(interval.start, cursor);
        cursor = interval.end;
    }
}
