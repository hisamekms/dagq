//! The ledger's rows in queue.db (`LedgerStore`, docs/design/measurement.md
//! "台帳の形"): every kind written with its version, its inputs, its marks
//! and its subject, read back as written and once per object, and a legacy
//! row whose values stay frozen while its records and their Executions
//! follow the events; a node row folded from the NodeSampleStore, its
//! marks and its retention.
use crate::common;

use dagq::application::{LedgerStore, NodeSampleStore};
use dagq::domain::host_metrics::{HOST_NODE, HostSample, SampleSource, summarize};
use dagq::domain::ledger::{
    self, LEDGER_V, LedgerKind, LedgerRow,
    node::{self, BUCKET_SECS},
    settle::{SETTLE_GRACE_MS, node_settlement},
    window::WindowQuery,
};
use dagq::domain::marks::utc_text;
use dagq::domain::stats::Cursor;
use dagq::domain::{EventId, RunEvent, RunId, TaskId};
use rusqlite::Connection;
use serde_json::{Value, json};

use common::queue::*;

const RUN: &str = "11111111-1111-4111-8111-111111111111";
const T0: i64 = 1_900_000_000;
const BUILT: &str = "2030-01-01T00:00:00.000Z";

fn event(id: i64, run: bool, kind: &str, payload: Value, secs: i64) -> RunEvent {
    RunEvent {
        id: EventId::new(id),
        task_id: run.then(|| TaskId::new(7)),
        goal_id: None,
        run_id: run.then(|| RunId::new(RUN).unwrap()),
        kind: kind.to_owned(),
        payload,
        created_at: utc_text((T0 + secs) * 1000),
        actor: None,
    }
}

fn phase(id: i64, phase: &str, blocker: &str, holds: &str, secs: i64) -> RunEvent {
    event(
        id,
        true,
        "run_phase_changed",
        json!({"phase": phase, "blocker": blocker, "holds": holds,
               "attempt": {"kind": "first", "n": 1}, "cause": "x", "v": 1}),
        secs,
    )
}

fn count(db: &std::path::Path, sql: &str) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row(sql, [], |r| r.get(0))
        .unwrap()
}

/// A sample of the host at `T0 + secs` with values of more decimals than
/// a JSON number reads back, and an empty cell.
fn host_sample(secs: i64) -> HostSample {
    let mut sample = HostSample::new(T0 + secs);
    #[allow(clippy::cast_precision_loss)]
    let x = secs as f64;
    sample.set("load1", Some(0.1 + 0.2 + x / 7.0));
    sample.set("cpu_total", Some(100.0 / 3.0 + x));
    sample.set("disk_free_bytes", Some(1e10 - x));
    sample
}

/// Record `secs` of the host in `queue`'s NodeSampleStore on `day`.
fn record(queue: &impl NodeSampleStore, day: i64, secs: i64) {
    assert!(
        queue
            .record_node_sample(HOST_NODE, day, &host_sample(secs), SampleSource::Sampled, 0)
            .unwrap()
    );
}

/// Every kind of row (a run's, a task's, a session's, the queue's and a
/// node's) is
/// written with its version, its inputs, its marks of settled and legacy
/// and its subject, reads back as written, and writing the same rows again
/// keeps one row each; the window read through the store gives back the
/// events.
#[test]
fn every_kind_of_row_is_written_once_and_reads_back() {
    let (dir, queue) = fixture();
    let db = dir.path().join("queue.db");
    let events = vec![
        event(1, true, "run_claimed", json!({}), 0),
        phase(2, "worker", "ai", "worker_slot", 0),
        phase(3, "ended", "runtime", "none", 60),
        event(
            4,
            false,
            "session_opened",
            json!({"kind": "runtime_planner", "planner_id": 5, "route": "headless"}),
            61,
        ),
        event(
            5,
            false,
            "plan_review_started",
            json!({"plan_review_id": 9}),
            62,
        ),
        event(
            6,
            false,
            "plan_review_finished",
            json!({"plan_review_id": 9, "verdict": "pass"}),
            70,
        ),
        event(
            7,
            false,
            "slots_full_started",
            json!({"supervisor": "s", "class": "heavy", "running": 1, "slots": 1}),
            80,
        ),
        event(8, false, "update_installed", json!({"supervisor": "s"}), 90),
    ];
    let now_ms = (T0 + 100) * 1000;
    let mut built = ledger::build(&events, now_ms, BUILT);
    record(&queue, 1, 10);
    built.extend(node::node_rows(
        &queue.node_samples(HOST_NODE, 0, T0 + 100).unwrap(),
        BUILT,
    ));
    let mut kinds: Vec<LedgerKind> = built.iter().map(|row| row.kind).collect();
    kinds.dedup();
    assert_eq!(kinds, LedgerKind::ALL);
    built[0].is_final = true;
    for _ in 0..2 {
        for row in &built {
            assert!(queue.write_ledger_row(row).unwrap());
        }
    }
    assert_eq!(
        count(&db, "SELECT count(*) FROM ledger_rows"),
        i64::try_from(built.len()).unwrap()
    );
    let mut read: Vec<LedgerRow> = LedgerKind::ALL
        .into_iter()
        .flat_map(|kind| queue.ledger_rows(kind).unwrap())
        .collect();
    let order = |row: &LedgerRow| (row.kind, row.key.clone());
    read.sort_by_key(order);
    built.sort_by_key(order);
    assert_eq!(read, built);
    let session =
        "SELECT {} FROM ledger_rows WHERE kind='session' AND key='runtime_planner/5/session/1'";
    let of = |column: &str| session.replace("{}", column);
    assert_eq!(count(&db, &of("round")), 1);
    assert_eq!(count(&db, &of("inputs_from")), 4);
    assert_eq!(count(&db, &of("ledger_v")), i64::from(LEDGER_V));
    assert_eq!(count(&db, &of("final")), 0);
    assert_eq!(
        count(
            &db,
            &of("subject_kind = 'runtime_planner' AND route = 'headless'")
        ),
        1
    );
    assert_eq!(
        count(&db, "SELECT final FROM ledger_rows WHERE kind='run'"),
        1
    );
    assert_eq!(count(&db, "SELECT sum(legacy) FROM ledger_rows"), 0);
    let window = queue
        .ledger_window(&WindowQuery {
            now_ms,
            ..WindowQuery::default()
        })
        .unwrap();
    assert_eq!(window.events(), events);
}

/// A legacy run's stored row keeps the values `stats` gave it once, while
/// an Execution that arrives after it was written becomes its record and
/// is read by the window, with its tokens and context.
#[test]
fn a_stored_legacy_row_follows_its_records_and_executions() {
    let (_dir, queue) = fixture();
    let mut events = vec![
        event(1, true, "run_claimed", json!({}), 0),
        event(2, true, "receipt_observed", json!({}), 60),
        event(3, true, "run_integrated", json!({}), 90),
    ];
    let first = ledger::run_row(RUN, &events, &[], 0, BUILT);
    assert!(first.is_legacy());
    assert!(queue.write_ledger_row(&first).unwrap());
    events.push(event(
        4,
        true,
        "turn_finished",
        json!({"provider": "claude", "tokens_source": "model_usage",
               "tokens": {"input": 9, "output": 1, "cache_read": 0, "cache_creation": 0},
               "tokens_by_model": [], "model": "opus",
               "peak_context": null, "context_window": 200000, "compactions": null,
               "context_reason": "rollout_missing"}),
        95,
    ));
    let later = ledger::run_row(RUN, &events, &[], 0, "2030-01-02T00:00:00.000Z");
    assert!(!queue.write_ledger_row(&later).unwrap());
    let read = queue.ledger_row(LedgerKind::Run, RUN).unwrap().unwrap();
    assert_eq!(read.legacy, first.legacy);
    assert_eq!(read.legacy.as_ref().unwrap()["work"], 60);
    assert_eq!(read.ledger_v, first.ledger_v);
    assert_eq!(read.records, events);
    assert_eq!(read.inputs_through.events, EventId::new(4));
    let window = queue
        .ledger_window(&WindowQuery {
            now_ms: (T0 + 100) * 1000,
            ..WindowQuery::default()
        })
        .unwrap();
    let executions = window.executions();
    assert_eq!(executions.len(), 1);
    assert_eq!(executions[0].tokens.unwrap().input, 9);
    let context = executions[0].context.as_ref().unwrap();
    assert_eq!(
        (context.peak, context.reason.as_deref()),
        (None, Some("rollout_missing"))
    );
}

/// A node row folded from the NodeSampleStore (a sampled row and an
/// imported one, with an empty cell) is written with its version, how far
/// into the store it was folded and its mark of settled, once however
/// often it is written; it reads back so that the window's host's load is
/// what the store's samples summarize to; a sample that comes in after it
/// makes it dirty, and the retention removes it with the store's rows.
#[test]
fn a_node_row_is_written_once_and_kept_as_the_node_samples() {
    let (dir, queue) = fixture();
    let db = dir.path().join("queue.db");
    let hour = node::bucket_of(T0);
    for secs in [hour - T0 + 30, hour - T0 + 60, hour - T0 + BUCKET_SECS + 5] {
        record(&queue, 1, secs);
    }
    let mut imported = host_sample(hour - T0 + 90);
    imported.values.remove("cpu_total");
    assert_eq!(
        queue
            .import_node_samples(HOST_NODE, "metrics-x.csv", 2, &[imported], 1)
            .unwrap(),
        1
    );
    let stored = queue.node_samples(HOST_NODE, 0, i64::MAX).unwrap();
    let mut rows = node::node_rows(&stored, BUILT);
    assert_eq!(rows.len(), 2);
    let end = (hour + BUCKET_SECS) * 1000;
    let last = stored.iter().map(|row| row.id).max();
    // The first hour's: two sampled rows and the imported one.
    let first_last = stored[..3].iter().map(|row| row.id).max();
    assert!(node_settlement(&rows[0], first_last, end + SETTLE_GRACE_MS).settles);
    rows[0].is_final = true;
    for _ in 0..2 {
        for row in &rows {
            assert!(queue.write_ledger_row(row).unwrap());
        }
    }
    let node_rows = "SELECT count(*) FROM ledger_rows WHERE kind='node'";
    assert_eq!(count(&db, node_rows), 2);
    let read = queue.ledger_rows(LedgerKind::Node).unwrap();
    assert_eq!(read, rows);
    assert_eq!(read[0].ledger_v, LEDGER_V);
    assert!(read[0].is_final && !read[1].is_final);
    assert_eq!(read[0].inputs_through.node_samples, first_last);
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM ledger_rows WHERE kind='node' AND subject_kind='node' AND subject_id='host'"
        ),
        2
    );
    let samples: Vec<HostSample> = stored.iter().map(|row| row.sample.clone()).collect();
    let (from, until) = (hour + 1, hour + BUCKET_SECS + 10);
    let window = queue
        .ledger_window(&WindowQuery {
            since: Some(Cursor::Time(from * 1000)),
            until: Some(Cursor::Time(until * 1000)),
            now_ms: until * 1000,
            ..WindowQuery::default()
        })
        .unwrap();
    assert_eq!(window.host(), summarize(&samples, from, until));
    assert_eq!(window.host().samples, 4);
    // A sample of the first hour that came in after it was folded.
    record(&queue, 3, hour - T0 + 120);
    let later = queue
        .node_samples(HOST_NODE, hour, hour + BUCKET_SECS - 1)
        .unwrap();
    let late = later.iter().map(|row| row.id).max();
    assert!(late > last);
    assert!(node_settlement(&read[0], late, end + SETTLE_GRACE_MS).dirty);
    // The retention: the store's and the ledger's by the same day.
    assert_eq!(queue.remove_node_samples(2).unwrap(), 3);
    assert_eq!(queue.remove_node_rows(2).unwrap(), 1);
    let kept = queue.ledger_rows(LedgerKind::Node).unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(node::samples(&kept[0]).len(), 1);
    assert_eq!(node::samples(&kept[0])[0].local_day, 2);
    assert_eq!(queue.remove_node_rows(2).unwrap(), 0);
    assert_eq!(queue.remove_node_rows(4).unwrap(), 1);
    assert_eq!(count(&db, node_rows), 0);
}
