//! A run's resources its executor measured from inside in its ledger row
//! on queue.db (docs/design/measurement-environment.md "台帳との順序"):
//! the ledger's pass reads the samples after its sample cursor, folds them
//! into the intervals the supervisor measured nothing of from outside,
//! each interval and the row saying where theirs come from, folds the row
//! again for a sample that came in after it settled but not for one sent
//! again, leaves the rows alone for a target of no run, and keeps what it
//! folded once the retention removed the samples. The decisions are the
//! unit tests of `domain::environment` and `application::supervise::ledger`.
use crate::common;

use dagq::application::{
    LedgerStore, RunResourceStore, SessionOwners, SessionResourceStore, SessionStepStore,
    supervise::{Retain, build_pass},
};
use dagq::domain::RunId;
use dagq::domain::environment::{DETAIL_KEY, EnvKind, EnvironmentAttached, Resources, RunResource};
use dagq::domain::ledger::{LedgerKind, LedgerRow, builder::Builder, settle::SETTLE_GRACE_MS};
use dagq::domain::marks::utc_text;
use dagq::domain::session_step::{ResourceSample, StepTarget, TargetKind};
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

/// Record `kind` on the run at `secs` (of no task: the queue holds none);
/// its id.
fn event(db: &Connection, kind: &str, payload: Value, secs: i64) -> i64 {
    db.execute(
        "INSERT INTO run_events(run_id, kind, payload, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![RUN, kind, payload.to_string(), utc_text(ms(secs))],
    )
    .unwrap();
    db.last_insert_rowid()
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

/// A window the supervisor measured from outside under `attachment`, from
/// `from` to `until` (seconds after [`T0`]), of `cpu_seconds` (`None`: it
/// could measure nothing).
fn window(
    attachment: i64,
    (from, until): (i64, i64),
    cpu_seconds: Option<f64>,
    last: bool,
) -> RunResource {
    RunResource {
        id: 0,
        run_id: RUN.to_owned(),
        attachment,
        node_id: "host".to_owned(),
        from_ms: ms(from),
        until_ms: ms(until),
        resources: Resources {
            cpu_seconds,
            ..Resources::default()
        },
        last,
    }
}

/// A sample of `seq` from `offset` for `duration` (seconds after the
/// session's start) of `cpu_seconds`.
fn sample(seq: i64, (offset, duration): (i64, i64), cpu_seconds: f64) -> ResourceSample {
    let mut sample = ResourceSample::new(seq, offset * 1000, duration * 1000);
    sample.cpu_seconds = Some(cpu_seconds);
    sample
}

/// One pass at `secs` after [`T0`], with no retention.
fn pass(queue: &SqliteQueue, builder: &mut Builder, secs: i64) {
    let built = build_pass(queue, builder, (ms(secs), BUILT), Retain::default(), None);
    assert_eq!(built.failure, None);
}

fn row(queue: &SqliteQueue) -> LedgerRow {
    queue.ledger_row(LedgerKind::Run, RUN).unwrap().unwrap()
}

/// The CPU seconds and the source of `row`'s interval of `phase`.
fn interval(row: &LedgerRow, phase: &str) -> (Value, Value) {
    let index = row.intervals.iter().position(|i| i.phase == phase).unwrap();
    let resources = &row.detail[DETAIL_KEY];
    (
        resources["intervals"][index]["cpu_seconds"].clone(),
        resources["sources"][index].clone(),
    )
}

/// A run worked from 0 s to 600 s and validated to 900 s on an
/// environment that measured nothing of its worker from outside: its
/// executor's sample folds into the worker, the window from outside into
/// the validation, the row mixed. It settles; a sample sent again leaves
/// the row, a later one folds it again; a planner's sample changes no
/// row; the retention removes the samples and a later sample keeps what
/// the row folded.
#[test]
fn a_runs_samples_from_inside_fold_where_nothing_was_measured_from_outside() {
    let (dir, queue) = fixture();
    let db = Connection::open(dir.path().join("queue.db")).unwrap();
    event(&db, "run_claimed", json!({}), 0);
    let attached = EnvironmentAttached {
        env_kind: EnvKind::Host,
        node_id: "host".to_owned(),
        image_digest: None,
        cpu_limit: Some(8.0),
        mem_limit: None,
        arch: "aarch64".to_owned(),
    };
    let attachment = event(&db, "environment_attached", attached.payload(), 0);
    phase(&db, "worker", 0);
    phase(&db, "validating", 600);
    phase(&db, "ended", 900);
    let target = StepTarget::run(&RunId::new(RUN).unwrap());
    queue.register_session(&target, "s1", ms(0)).unwrap();
    let first = [sample(1, (0, 300), 20.0), sample(2, (300, 300), 10.0)];
    queue
        .record_resources(&target, "s1", &first, ms(300))
        .unwrap();
    queue
        .record_run_resource(&window(attachment, (0, 600), None, false))
        .unwrap();
    queue
        .record_run_resource(&window(attachment, (600, 900), Some(7.0), true))
        .unwrap();
    let mut builder = Builder::default();
    let settled = 900 + SETTLE_GRACE_MS / 1000;
    pass(&queue, &mut builder, settled);
    let written = row(&queue);
    assert!(written.is_final);
    assert_eq!(written.detail[DETAIL_KEY]["source"], "mixed");
    assert_eq!(interval(&written, "worker"), (json!(30.0), json!("inside")));
    assert_eq!(
        interval(&written, "validating"),
        (json!(7.0), json!("outside"))
    );
    assert_eq!(written.detail[DETAIL_KEY]["total"]["cpu_seconds"], 37.0);

    // Sent again: the store adds none, the row stays.
    let again = queue
        .record_resources(&target, "s1", &first, ms(settled + 1))
        .unwrap();
    assert_eq!(again.added, 0);
    pass(&queue, &mut builder, settled + 1);
    assert_eq!(row(&queue), written);

    // Late, after it settled: folded again.
    queue
        .record_resources(&target, "s1", &[sample(3, (500, 50), 4.0)], ms(settled + 2))
        .unwrap();
    pass(&queue, &mut builder, settled + 2);
    let late = row(&queue);
    assert!(late.inputs_through.inside_resources > written.inputs_through.inside_resources);
    assert_eq!(interval(&late, "worker"), (json!(34.0), json!("inside")));
    assert!(late.is_final);

    // A planner's sample: no row changes.
    let planner = StepTarget {
        kind: TargetKind::Planner,
        id: "12".to_owned(),
    };
    queue.register_session(&planner, "p1", ms(0)).unwrap();
    queue
        .record_resources(&planner, "p1", &[sample(1, (0, 10), 9.0)], ms(settled + 3))
        .unwrap();
    let tasks = queue.ledger_rows(LedgerKind::Task).unwrap();
    pass(&queue, &mut builder, settled + 3);
    assert_eq!(row(&queue), late);
    assert_eq!(queue.ledger_rows(LedgerKind::Task).unwrap(), tasks);

    // The retention removes every sample; a later one folds onto what the
    // row counted, nothing goes down, the removed ones counted once.
    queue.remove_steps(ms(10_000_000)).unwrap();
    assert!(queue.target_resources(&target).unwrap().is_empty());
    event(&db, "review_finished", json!({}), settled + 4);
    pass(&queue, &mut builder, settled + 4);
    assert_eq!(row(&queue).detail[DETAIL_KEY], late.detail[DETAIL_KEY]);
    queue
        .record_resources(
            &target,
            "s1",
            &[sample(1, (0, 300), 20.0), sample(4, (550, 10), 1.0)],
            ms(settled + 5),
        )
        .unwrap();
    pass(&queue, &mut builder, settled + 5);
    let kept = row(&queue);
    assert_eq!(interval(&kept, "worker"), (json!(35.0), json!("inside")));
    assert_eq!(kept.detail[DETAIL_KEY]["total"]["cpu_seconds"], 42.0);
}
