//! The Spike's measurement of the breach reads' plan
//! (`docs/plans/kpi-breach-read-plan-spike.md`): for each count `n` of
//! events without a run of other kinds (and as many with a run, a task and
//! a goal), each candidate statement's query plan, VM steps, full-scan
//! steps, sorts and time, and `record_kpi_breach`'s time on its read-only
//! path and on a new start. The test keeps a small `n`; the Spike's sizes
//! are given to [`run`] in its place (its doc comment). The statements
//! the queue runs ([`BREACH_EVENTS`], [`LATEST_PUSH_ATTENTION`]) are held
//! to `events_by_kind` and to steps that do not grow with those events.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, StatementStatus, params};
use serde_json::{Value, json};

use dagq::domain::EventKind;
use dagq::domain::kpi::push::{KPI_BREACH_KINDS, KPI_PUSH_ATTENTION_KINDS};
use dagq::infrastructure::kpi_push::{BREACH_EVENTS, LATEST_PUSH_ATTENTION};
use dagq::infrastructure::sqlite::SqliteQueue;

use crate::common;

const ROUNDS: usize = 3;
/// The breach events without a run, as many as the live queue holds.
const BREACHES: usize = 11;
const FILTER: &str = "run_id IS NULL AND task_id IS NULL AND goal_id IS NULL";

/// The statements measured: (name, sql, binds).
fn candidates() -> Vec<(&'static str, String, [&'static str; 2])> {
    let breach = |from: &str, filter: &str| {
        format!(
            "SELECT id, kind, payload FROM {from} WHERE kind IN (?1, ?2) AND {filter} ORDER BY id"
        )
    };
    let abandoned = |from: &str, filter: &str| {
        format!(
            "SELECT kind FROM {from} WHERE kind IN (?1, ?2) AND {filter} ORDER BY id DESC LIMIT 1"
        )
    };
    let plus_run = "+run_id IS NULL AND task_id IS NULL AND goal_id IS NULL";
    let plus_all = "+run_id IS NULL AND +task_id IS NULL AND +goal_id IS NULL";
    let by_kind = "run_events INDEXED BY events_by_kind";
    vec![
        ("breach (i)", breach("run_events", FILTER), KPI_BREACH_KINDS),
        (
            "breach (ii)",
            breach("run_events", plus_run),
            KPI_BREACH_KINDS,
        ),
        (
            "breach (iii)",
            breach("run_events", plus_all),
            KPI_BREACH_KINDS,
        ),
        ("breach (iv)", breach(by_kind, FILTER), KPI_BREACH_KINDS),
        (
            "abandoned (i)",
            abandoned("run_events", FILTER),
            KPI_PUSH_ATTENTION_KINDS,
        ),
        (
            "abandoned (ii)",
            abandoned("run_events", plus_run),
            KPI_PUSH_ATTENTION_KINDS,
        ),
        (
            "abandoned (iv)",
            abandoned(by_kind, FILTER),
            KPI_PUSH_ATTENTION_KINDS,
        ),
    ]
}

fn breach(kpi: &str) -> Value {
    json!({"period": "day", "kpi": kpi, "stratum": "all"})
}

/// A queue at the latest schema with `n` events without a run of other
/// kinds and `n` with a run, a task and a goal, the breach events and one
/// `kpi_push_sent` spread among them; closed, so its file is the template.
fn template(dir: &Path, n: usize) -> PathBuf {
    let path = dir.join(format!("template-{n}.db"));
    drop(SqliteQueue::init(&path).unwrap());
    let mut conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "foreign_keys", false).unwrap();
    let tx = conn.transaction().unwrap();
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO run_events(task_id, goal_id, run_id, kind, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .unwrap();
        let padding = "x".repeat(160);
        let others = ["supervisor_heartbeat", "queue_write_held_long", "kpi_daily"];
        let total = 2 * n + BREACHES + 1;
        let special = |k: usize| k * total / (BREACHES + 1);
        let mut breaches = 0;
        for i in 0..total {
            if breaches <= BREACHES && i == special(breaches) {
                if breaches == BREACHES {
                    insert
                        .execute(params![
                            None::<i64>,
                            None::<i64>,
                            None::<String>,
                            KPI_PUSH_ATTENTION_KINDS[0],
                            "{}"
                        ])
                        .unwrap();
                } else {
                    // Five breaches started and resolved, the last one open.
                    let kind = KPI_BREACH_KINDS[breaches % 2];
                    let mut payload = breach(&format!("kpi-{}", breaches / 2));
                    payload["pushed"] = json!(true);
                    payload["day"] = json!(1);
                    insert
                        .execute(params![
                            None::<i64>,
                            None::<i64>,
                            None::<String>,
                            kind,
                            payload.to_string()
                        ])
                        .unwrap();
                }
                breaches += 1;
            } else if i % 2 == 0 {
                let payload = json!({"pad": padding}).to_string();
                insert
                    .execute(params![
                        None::<i64>,
                        None::<i64>,
                        None::<String>,
                        others[i % 3],
                        payload
                    ])
                    .unwrap();
            } else {
                let payload = json!({"pad": padding}).to_string();
                insert
                    .execute(params![
                        (i % 50) as i64 + 1,
                        (i % 7) as i64 + 1,
                        format!("run-{}", i % 50),
                        "turn_requested",
                        payload
                    ])
                    .unwrap();
            }
        }
        assert_eq!(breaches, BREACHES + 1);
    }
    tx.commit().unwrap();
    drop(conn);
    assert!(!path.with_extension("db-wal").exists());
    path
}

fn copy_of(template: &Path, name: &str) -> PathBuf {
    let path = template.with_file_name(name);
    std::fs::copy(template, &path).unwrap();
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    path
}

/// One statement's read: plan, VM steps, full-scan steps, sorts, rows,
/// time.
struct Read {
    plan: String,
    vm: i32,
    fullscan: i32,
    sort: i32,
    rows: usize,
    time: Duration,
}

fn read(conn: &Connection, sql: &str, binds: [&str; 2]) -> Read {
    let plan = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(params![binds[0], binds[1]], |r| r.get::<_, String>(3))
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>()
        .join("; ");
    let started = Instant::now();
    let mut statement = conn.prepare(sql).unwrap();
    let mut rows = 0;
    {
        // Every row is stepped to its end and read; an error fails the read.
        let mut cursor = statement.query(params![binds[0], binds[1]]).unwrap();
        while let Some(row) = cursor.next().unwrap() {
            row.get_ref(0).unwrap();
            rows += 1;
        }
    }
    let time = started.elapsed();
    Read {
        plan,
        vm: statement.get_status(StatementStatus::VmStep),
        fullscan: statement.get_status(StatementStatus::FullscanStep),
        sort: statement.get_status(StatementStatus::Sort),
        rows,
        time,
    }
}

fn median(mut times: Vec<Duration>) -> f64 {
    times.sort();
    times[times.len() / 2].as_secs_f64() * 1000.0
}

/// `n`'s measurement, as table rows: (statement, n, plan, vm, fullscan,
/// sort, rows, median ms).
type Row = (String, usize, String, i32, i32, i32, usize, f64);

fn measure(dir: &Path, n: usize, log: &mut Vec<String>) -> Vec<Row> {
    let template = template(dir, n);
    let statements = candidates();
    let groups = [vec![0, 1, 2, 3], vec![4, 5, 6]];
    let mut reads: Vec<Vec<Read>> = statements.iter().map(|_| Vec::new()).collect();
    {
        // The queue opens and checks the schema; the reads run on a plain
        // connection to the same file, which sets no pragma of its own.
        drop(SqliteQueue::open(&template).unwrap());
        let conn = Connection::open(&template).unwrap();
        for round in 0..ROUNDS {
            for group in &groups {
                let mut order = group.clone();
                order.rotate_left(round % group.len());
                let (_, sql, binds) = &statements[order[0]];
                read(&conn, sql, *binds);
                log.push(format!(
                    "n={n} round={} order={}",
                    round + 1,
                    order
                        .iter()
                        .map(|&i| statements[i].0)
                        .collect::<Vec<_>>()
                        .join(" -> ")
                ));
                for i in order {
                    let (_, sql, binds) = &statements[i];
                    reads[i].push(read(&conn, sql, *binds));
                }
            }
        }
    }
    let mut rows: Vec<Row> = statements
        .iter()
        .zip(reads)
        .map(|((name, _, _), reads)| {
            let first = &reads[0];
            for other in &reads {
                assert_eq!(
                    (other.vm, other.plan.as_str()),
                    (first.vm, first.plan.as_str())
                );
            }
            (
                (*name).to_owned(),
                n,
                first.plan.clone(),
                first.vm,
                first.fullscan,
                first.sort,
                first.rows,
                median(reads.iter().map(|r| r.time).collect()),
            )
        })
        .collect();
    rows.extend(record_kpi_breach_times(&template, n, log));
    rows
}

/// `record_kpi_breach` on the path that only reads (the breach already
/// open, rows 0) and on a new start (rows 1, from a fresh copy of the
/// template each round), alternated with the first swapped each round.
fn record_kpi_breach_times(template: &Path, n: usize, log: &mut Vec<String>) -> Vec<Row> {
    let open = copy_of(template, &format!("open-{n}.db"));
    let target = breach("target");
    {
        let queue = SqliteQueue::open(&open).unwrap();
        assert!(
            queue
                .record_kpi_breach(EventKind::KpiBreachStarted, target.clone(), Some((1, 99)))
                .unwrap()
                .is_some()
        );
    }
    let read_only = SqliteQueue::open(&open).unwrap();
    let (mut read_times, mut start_times) = (Vec::new(), Vec::new());
    for round in 0..ROUNDS {
        let read_first = round % 2 == 0;
        for path in if read_first {
            [true, false]
        } else {
            [false, true]
        } {
            if path {
                let open_before = read_only.kpi_breaches_open().unwrap();
                assert!(open_before.iter().any(|b| b["kpi"] == "target"));
                let started = Instant::now();
                let written = read_only
                    .record_kpi_breach(EventKind::KpiBreachStarted, target.clone(), Some((1, 99)))
                    .unwrap();
                read_times.push(started.elapsed());
                assert!(written.is_none());
                log.push(format!(
                    "n={n} round={} read-only: target open before, rows 0",
                    round + 1
                ));
            } else {
                let copy = copy_of(template, &format!("start-{n}-{round}.db"));
                let queue = SqliteQueue::open(&copy).unwrap();
                let before = queue.kpi_breaches_open().unwrap();
                assert!(!before.iter().any(|b| b["kpi"] == "target"));
                let started = Instant::now();
                let written = queue
                    .record_kpi_breach(EventKind::KpiBreachStarted, target.clone(), Some((1, 99)))
                    .unwrap();
                start_times.push(started.elapsed());
                assert!(written.is_some());
                assert!(
                    queue
                        .kpi_breaches_open()
                        .unwrap()
                        .iter()
                        .any(|b| b["kpi"] == "target")
                );
                drop(queue);
                std::fs::remove_file(&copy).unwrap();
                log.push(format!(
                    "n={n} round={} new start: target not open before, rows 1, copy {} dropped",
                    round + 1,
                    copy.display()
                ));
            }
        }
    }
    let row = |name: &str, rows, times| {
        (
            name.to_owned(),
            n,
            String::new(),
            0,
            0,
            0,
            rows,
            median(times),
        )
    };
    vec![
        row("record_kpi_breach read-only", 0, read_times),
        row("record_kpi_breach new start", 1, start_times),
    ]
}

/// Each of `ns` measured, printed as the Spike's table. The Spike ran it
/// with `[0, 10_000, 100_000]` in place of the test's sizes and
/// `--nocapture`; those sizes take tens of seconds, too slow for a test
/// that always runs.
fn run(ns: &[usize]) -> Vec<Row> {
    let (dir, _owner) = common::cleanup::owned_temp_dir();
    let mut log = Vec::new();
    let mut rows = Vec::new();
    for &n in ns {
        rows.extend(measure(dir.path(), n, &mut log));
    }
    for line in &log {
        println!("{line}");
    }
    println!("| statement | N | plan | VmStep | FullscanStep | Sort | rows | ms (median) |");
    for (name, n, plan, vm, fullscan, sort, count, ms) in &rows {
        println!("| {name} | {n} | {plan} | {vm} | {fullscan} | {sort} | {count} | {ms:.3} |");
    }
    rows
}

/// The measurement at a small `n`: every statement reads the breaches
/// (or the latest attention), and both paths of `record_kpi_breach` run.
#[test]
fn the_breach_reads_are_measured_at_a_small_size() {
    let rows = run(&[0, 50]);
    for (name, _, _, _, _, _, count, _) in &rows {
        let expected = match name.as_str() {
            n if n.starts_with("breach") => BREACHES,
            n if n.starts_with("abandoned") => 1,
            "record_kpi_breach read-only" => 0,
            _ => 1,
        };
        assert_eq!(*count, expected, "{name}");
    }
}

/// The queue's breach and attention reads go by `events_by_kind`, and
/// their VM steps stay flat when events without a run of other kinds (and
/// as many with a run, a task and a goal) are added: under the write lock
/// they read the events of their two kinds, not every event without a
/// goal.
#[test]
fn the_queue_breach_reads_go_by_kind_and_do_not_grow_with_other_events() {
    let (dir, _owner) = common::cleanup::owned_temp_dir();
    let (small, large) = (template(dir.path(), 0), template(dir.path(), 2_000));
    let (small, large) = (
        Connection::open(&small).unwrap(),
        Connection::open(&large).unwrap(),
    );
    for (sql, binds, rows) in [
        (BREACH_EVENTS, KPI_BREACH_KINDS, BREACHES),
        (LATEST_PUSH_ATTENTION, KPI_PUSH_ATTENTION_KINDS, 1),
    ] {
        let (before, after) = (read(&small, sql, binds), read(&large, sql, binds));
        for read in [&before, &after] {
            assert!(read.plan.contains("events_by_kind"), "{}", read.plan);
            for other in [
                "events_by_goal",
                "events_by_run",
                "events_by_task",
                "SCAN run_events",
            ] {
                assert!(!read.plan.contains(other), "{}", read.plan);
            }
            assert_eq!(read.rows, rows, "{sql}");
        }
        let ratio = f64::from(after.vm) / f64::from(before.vm);
        assert!(
            ratio < 2.0,
            "{sql}: VmStep {} -> {} ({ratio})",
            before.vm,
            after.vm
        );
    }
}
