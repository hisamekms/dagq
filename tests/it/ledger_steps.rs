//! A run's session steps in its ledger row on queue.db
//! (docs/design/measurement.md "台帳を作る係"): the ledger's pass reads the
//! steps after its step cursor, folds a run's into its row as the inner
//! layer of its intervals, folds the row again for a step that came in
//! after it was written but not for one sent again, settles it only once
//! its telemetry token's revocation is recorded, keeps what it folded once
//! the retention removed the steps (and adds a late one), and leaves the rows alone for a target
//! of no run. The decisions are the unit tests of
//! `domain::ledger::builder` and `application::supervise::ledger`.
use crate::common;

use dagq::application::{
    LedgerStore, SessionOwners, SessionStepStore,
    supervise::{Retain, build_pass},
};
use dagq::domain::RunId;
use dagq::domain::ledger::{
    LedgerKind, LedgerRow, builder::Builder, settle::SETTLE_GRACE_MS, steps::DETAIL_KEY,
};
use dagq::domain::marks::utc_text;
use dagq::domain::session_step::{Step, StepKind, StepTarget, TargetKind};
use dagq::infrastructure::sqlite::SqliteQueue;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use common::queue::fixture;

const RUN: &str = "11111111-1111-4111-8111-111111111111";
const T0: i64 = 1_900_000_000;
const BUILT: &str = "2030-01-01T00:00:00.000Z";

/// The unix milliseconds of `secs` after [`T0`].
const fn ms(secs: i64) -> i64 {
    (T0 + secs) * 1000
}

/// Record `kind` on the run at `secs` (of no task: the queue holds none).
fn event(db: &Connection, kind: &str, payload: Value, secs: i64) {
    db.execute(
        "INSERT INTO run_events(run_id, kind, payload, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![RUN, kind, payload.to_string(), utc_text(ms(secs))],
    )
    .unwrap();
}

fn phase(db: &Connection, phase: &str, secs: i64) {
    event(
        db,
        "run_phase_changed",
        json!({"phase": phase, "blocker": "ai", "holds": "worker_slot",
               "attempt": {"kind": "first", "n": 1}, "cause": "x", "v": 1}),
        secs,
    );
}

/// One pass at `secs` after [`T0`], with no retention.
fn pass(queue: &SqliteQueue, builder: &mut Builder, secs: i64) {
    let built = build_pass(queue, builder, ms(secs), BUILT, Retain::default());
    assert_eq!(built.failure, None);
}

fn row(queue: &SqliteQueue) -> LedgerRow {
    queue.ledger_row(LedgerKind::Run, RUN).unwrap().unwrap()
}

/// The worker interval's kinds and coverage, and the counts of the steps.
fn worker_and_counts(row: &LedgerRow) -> (Value, Value) {
    let steps = &row.detail[DETAIL_KEY];
    let worker = steps["parents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|parent| parent["phase"] == "worker")
        .unwrap()
        .clone();
    (worker, steps["fold"]["counts"].clone())
}

/// A run worked from 0 s to 600 s, its session registered at 0 s: its
/// steps fold into its row per kind with the worker's coverage; a step
/// sent again leaves the row, a later one folds it again; it is settled
/// only once its telemetry token's revocation is recorded; a planner's
/// steps change no row; the retention removes the steps and the session's
/// owner, a later event keeps what the row folded, and a late step that
/// fills a gap is added, placed by the start the row kept.
#[test]
fn a_runs_steps_fold_into_its_row_which_settles_after_its_tokens_revocation() {
    let (dir, queue) = fixture();
    let db = Connection::open(dir.path().join("queue.db")).unwrap();
    event(&db, "run_claimed", json!({}), 0);
    event(&db, "telemetry_token_issued", json!({}), 0);
    phase(&db, "worker", 0);
    phase(&db, "ended", 600);
    let target = StepTarget::run(&RunId::new(RUN).unwrap());
    queue.register_session(&target, "s1", ms(0)).unwrap();
    let first = [
        Step::new(1, StepKind::ModelTurn, 0, 60_000),
        Step::new(2, StepKind::ToolCall, 60_000, 60_000),
        Step::new(3, StepKind::Subagent, 60_000, 120_000),
    ];
    queue.record_steps(&target, "s1", &first, ms(300)).unwrap();
    let mut builder = Builder::default();
    let settled = 600 + SETTLE_GRACE_MS / 1000;
    pass(&queue, &mut builder, settled);
    let written = row(&queue);
    let (worker, counts) = worker_and_counts(&written);
    assert_eq!(
        worker["by_kind"],
        json!({"model_turn": 60_000, "tool_call": 60_000, "subagent": 120_000, "wait": 0})
    );
    assert_eq!(worker["covered_ms"], 180_000);
    assert!((worker["coverage"].as_f64().unwrap() - 0.3).abs() < 1e-9);
    assert_eq!(
        counts,
        json!({"model_turn": 1, "tool_call": 1, "subagent": 1})
    );
    // Past its grace, but its token is not revoked.
    assert!(!written.is_final);

    // Sent again: the store adds none, the row stays.
    queue.record_steps(&target, "s1", &first, ms(700)).unwrap();
    pass(&queue, &mut builder, settled + 1);
    assert_eq!(row(&queue), written);
    // A later step folds it again; seq 4 has not come in (a gap).
    queue
        .record_steps(
            &target,
            "s1",
            &[Step::new(5, StepKind::Wait, 200_000, 100_000)],
            ms(700),
        )
        .unwrap();
    pass(&queue, &mut builder, settled + 2);
    let later = row(&queue);
    assert!(later.inputs_through.steps > written.inputs_through.steps);
    assert_eq!(worker_and_counts(&later).0["covered_ms"], 280_000);
    assert_eq!(later.detail[DETAIL_KEY]["missing"], 1);
    assert!(!later.is_final);

    event(
        &db,
        "telemetry_token_revoked",
        json!({"reason": "run_ended"}),
        settled + 3,
    );
    pass(&queue, &mut builder, settled + 3);
    let revoked = row(&queue);
    assert!(revoked.is_final);

    // A planner's steps: no row changes.
    let planner = StepTarget {
        kind: TargetKind::Planner,
        id: "12".to_owned(),
    };
    queue.register_session(&planner, "p1", ms(0)).unwrap();
    queue.record_steps(&planner, "p1", &first, ms(800)).unwrap();
    let tasks = queue.ledger_rows(LedgerKind::Task).unwrap();
    pass(&queue, &mut builder, settled + 4);
    assert_eq!(row(&queue), revoked);
    assert_eq!(queue.ledger_rows(LedgerKind::Task).unwrap(), tasks);

    // The retention removes every step and the sessions' owners; a later
    // event keeps the steps. The run's four and the planner's three.
    assert_eq!(queue.remove_steps(ms(10_000_000)).unwrap(), 7);
    assert_eq!(queue.session_owner("s1").unwrap(), None);
    event(&db, "review_finished", json!({}), settled + 5);
    pass(&queue, &mut builder, settled + 5);
    let kept = row(&queue);
    assert!(kept.inputs_through.events > revoked.inputs_through.events);
    assert_eq!(kept.detail[DETAIL_KEY], revoked.detail[DETAIL_KEY]);
    assert_eq!(kept.inputs_through.steps, revoked.inputs_through.steps);

    // Seq 4 comes in late and seq 3 is sent again: seq 4 fills the gap,
    // placed by the session's start the row kept, and nothing goes down.
    queue
        .record_steps(
            &target,
            "s1",
            &[
                Step::new(3, StepKind::Subagent, 60_000, 120_000),
                Step::new(4, StepKind::ModelTurn, 300_000, 60_000),
            ],
            ms(settled + 6),
        )
        .unwrap();
    pass(&queue, &mut builder, settled + 6);
    let late = row(&queue);
    let (worker, counts) = worker_and_counts(&late);
    assert_eq!(
        counts,
        json!({"model_turn": 2, "tool_call": 1, "subagent": 1, "wait": 1})
    );
    assert_eq!(
        worker["by_kind"],
        json!({"model_turn": 120_000, "tool_call": 60_000, "subagent": 120_000, "wait": 100_000})
    );
    assert_eq!(worker["covered_ms"], 340_000);
    assert_eq!(late.detail[DETAIL_KEY]["missing"], 0);
    assert_eq!(late.detail[DETAIL_KEY]["fold"]["unplaced"], json!([]));
}
