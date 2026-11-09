//! How fast the ledger's rows are written to a queue in a file
//! (docs/plans/ledger-write-budget-spike.md "書く速さ"): rows of the shape
//! the builder writes (queue rows of one event each, queue rows of an
//! interval and run rows), `N` at a time, (A) each in a transaction of its
//! own after its own read, as [`crate::application::LedgerStore::write_ledger_row_over`]
//! is called a row at a time by the pass, and (B) all of them in one
//! transaction after one read of them all
//! ([`crate::application::LedgerStore::ledger_rows_of`],
//! [`crate::application::LedgerStore::write_session_rows_marked`]). A
//! measurement, not a check: it runs only when named with `--ignored`.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::*;
use crate::domain::ledger::testkit::{Log, built_and_settled, queue_across, run_id};

/// The rows written at a time.
const SIZES: [usize; 3] = [200, 1_000, 5_000];

/// The rounds of each shape and size, the first of them a warm up.
const ROUNDS: usize = 5;

/// The rounds when those of [`ROUNDS`] spread by more than twice.
const MORE_ROUNDS: usize = 10;

/// The rows the store holds before a round writes: the queue's ledger
/// holds hundreds of thousands, so a write goes into an index of depth.
const PREFILL: usize = 100_000;

/// Of every 20 rows written: queue rows of one event, of an interval, run
/// rows (the sample of the production queue: 91% of its events are of no
/// run and no task, of one kind each).
const MIX: [usize; 3] = [18, 1, 1];

/// A queue row of one refusal, a queue row of an interval and a run row of
/// some hundred events, as the builder folds them.
fn templates() -> [LedgerRow; 3] {
    let mut log = Log::default();
    log.add(
        None,
        None,
        "authorization_denied",
        json!({"event": "authorization_denied", "role": "executor",
               "capability": "telemetry.report", "reason": "not_on_resource",
               "resource": {"kind": "run", "id": run_id(9)}}),
        -100,
    );
    for event in queue_across().0.events {
        let id = EventId::new(i64::try_from(log.events.len()).unwrap() + 1);
        log.events.push(RunEvent { id, ..event });
    }
    let run = run_id(7);
    log.add(Some(7), Some(&run), "run_claimed", json!({}), 10);
    for n in 0..100 {
        let phase = if n % 2 == 0 { "working" } else { "validating" };
        log.add(
            Some(7),
            Some(&run),
            "run_phase_changed",
            json!({"phase": phase, "blocker": "ai", "holds": "worker_slot",
                   "attempt": {"kind": "first", "n": 1}, "cause": "x", "v": 1}),
            20 + n,
        );
    }
    let rows = built_and_settled(&log.events, 1_000_000);
    let single = rows
        .iter()
        .find(|row| row.kind == LedgerKind::Queue && row.key == "1")
        .expect("the refusal's row");
    let interval = rows
        .iter()
        .find(|row| {
            row.kind == LedgerKind::Queue
                && row
                    .subject
                    .as_ref()
                    .is_some_and(|subject| subject.kind == "slots_full")
        })
        .expect("an interval's row");
    let run = rows
        .iter()
        .find(|row| row.kind == LedgerKind::Run && row.key == run)
        .expect("the run's row");
    assert!(run.records.len() > 100, "the run's row holds its events");
    [single.clone(), interval.clone(), run.clone()]
}

/// `n` rows of the templates in [`MIX`], keyed from `from` on: a queue row
/// by an event id after those held, a run row by an id of its own.
fn rows(templates: &[LedgerRow; 3], n: usize, from: usize) -> Vec<LedgerRow> {
    let period: usize = MIX.iter().sum();
    (0..n)
        .map(|i| {
            let at = i % period;
            let which = if at < MIX[0] {
                0
            } else if at < MIX[0] + MIX[1] {
                1
            } else {
                2
            };
            let mut row = templates[which].clone();
            let id = from + i;
            row.key = if row.kind == LedgerKind::Run {
                format!("{id:08x}-2222-4222-8222-222222222222")
            } else {
                id.to_string()
            };
            row
        })
        .collect()
}

/// A new store in a file of its own, holding [`PREFILL`] queue rows of
/// one event (most of the rows the queue's ledger holds).
fn store(templates: &[LedgerRow; 3]) -> (tempfile::TempDir, SqliteQueue) {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    let held: Vec<LedgerRow> = (1..=PREFILL)
        .map(|id| LedgerRow {
            key: id.to_string(),
            ..templates[0].clone()
        })
        .collect();
    for chunk in held.chunks(10_000) {
        let pairs: Vec<(LedgerRow, Option<LedgerRow>)> =
            chunk.iter().map(|row| (row.clone(), None)).collect();
        assert!(queue.write_session_rows_marked(&pairs, None).unwrap());
    }
    (dir, queue)
}

/// What a write takes: the whole, the part on the queue service's write
/// line (as `write_over` and `write_sessions_over` of the service's
/// dispatch do it, after the pass's read) and the transactions in it.
#[derive(Default, Clone, Copy)]
struct Took {
    all: Duration,
    line: Duration,
    tx: Duration,
}

/// (A): as the pass and the service do a row at a time: the pass reads
/// the row and sends it with the fingerprint read as one text, and the write of it on the line reads it again, checks its
/// fingerprint and writes over it in a transaction of its own.
fn write_each(queue: &SqliteQueue, rows: &[LedgerRow]) -> Took {
    let mut took = Took::default();
    let started = Instant::now();
    for row in rows {
        let read = queue
            .ledger_row(row.kind, &row.key)
            .unwrap()
            .map(|row| row.fingerprint());
        let text = serde_json::to_string(&(row, &read)).unwrap();
        let line = Instant::now();
        let (row, read): (LedgerRow, Option<String>) = serde_json::from_str(&text).unwrap();
        let row = &row;
        let held = queue.ledger_row(row.kind, &row.key).unwrap();
        assert_eq!(held.as_ref().map(LedgerRow::fingerprint), read);
        let tx = Instant::now();
        assert!(
            queue
                .write_ledger_row_over(row, held.as_ref())
                .unwrap()
                .is_some()
        );
        took.tx += tx.elapsed();
        took.line += line.elapsed();
    }
    took.all = started.elapsed();
    took
}

/// (B): the pass reads the rows in one statement and sends them with the
/// fingerprints read as one text; on the line the text is read, each row
/// read again and its fingerprint checked, and all written over them in
/// one transaction.
fn write_together(queue: &SqliteQueue, rows: &[LedgerRow]) -> Took {
    let started = Instant::now();
    let keys: Vec<(LedgerKind, String)> =
        rows.iter().map(|row| (row.kind, row.key.clone())).collect();
    let mut stored: BTreeMap<(LedgerKind, String), String> = queue
        .ledger_rows_of(&keys)
        .unwrap()
        .into_iter()
        .map(|row| ((row.kind, row.key.clone()), row.fingerprint()))
        .collect();
    let sent: Vec<(&LedgerRow, Option<String>)> = rows
        .iter()
        .map(|row| (row, stored.remove(&(row.kind, row.key.clone()))))
        .collect();
    let text = serde_json::to_string(&sent).unwrap();
    let line = Instant::now();
    let read: Vec<(LedgerRow, Option<String>)> = serde_json::from_str(&text).unwrap();
    let mut over = Vec::with_capacity(read.len());
    for (row, fingerprint) in read {
        let held = queue.ledger_row(row.kind, &row.key).unwrap();
        assert_eq!(held.as_ref().map(LedgerRow::fingerprint), fingerprint);
        over.push((row, held));
    }
    let tx = Instant::now();
    assert!(queue.write_session_rows_marked(&over, None).unwrap());
    Took {
        all: started.elapsed(),
        line: line.elapsed(),
        tx: tx.elapsed(),
    }
}

fn millis_of(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        f64::midpoint(values[mid - 1], values[mid])
    } else {
        values[mid]
    }
}

/// One round of a shape: a new store, the rows written, the times (ms).
fn round(templates: &[LedgerRow; 3], together: bool, n: usize) -> [f64; 3] {
    let (_dir, queue) = store(templates);
    let rows = rows(templates, n, PREFILL + 1);
    let took = if together {
        write_together(&queue, &rows)
    } else {
        write_each(&queue, &rows)
    };
    [took.all, took.line, took.tx].map(millis_of)
}

/// The table of the rounds and of their medians, printed as Markdown.
#[test]
#[ignore = "a measurement for docs/plans/ledger-write-budget-spike.md, run by name"]
fn ledger_write_budget_spike() {
    let templates = templates();
    let sizes: Vec<(usize, usize)> = templates
        .iter()
        .map(|row| (row.records.len(), serde_json::to_string(row).unwrap().len()))
        .collect();
    println!("records and JSON bytes of the templates (single, interval, run): {sizes:?}");
    println!(
        "| 書き方 | N | 周 | 全体（ms） | 列（ms） | transaction（ms） | 1行あたり（ms） | 1秒あたりの行数 |"
    );
    println!("|---|---|---|---|---|---|---|---|");
    let mut medians = Vec::new();
    for n in SIZES {
        let mut times: [Vec<[f64; 3]>; 2] = [Vec::new(), Vec::new()];
        let mut rounds = ROUNDS;
        let mut done = 0;
        while done < rounds {
            for (shape, together) in [(0, false), (1, true)] {
                let took = round(&templates, together, n);
                let [all, line, tx] = took;
                let name = ["A", "B"][shape];
                let warm = if done == 0 { "（暖機）" } else { "" };
                println!(
                    "| {name} | {n} | {}{warm} | {all:.1} | {line:.1} | {tx:.1} | {:.4} | {:.0} |",
                    done + 1,
                    all / n as f64,
                    n as f64 / (all / 1000.0),
                );
                times[shape].push(took);
            }
            done += 1;
            if done == rounds && rounds == ROUNDS {
                let spread = times.iter().any(|kept| {
                    let counted: Vec<f64> = kept[1..].iter().map(|took| took[0]).collect();
                    let max = counted.iter().copied().fold(f64::MIN, f64::max);
                    let min = counted.iter().copied().fold(f64::MAX, f64::min);
                    max / min > 2.0
                });
                if spread {
                    println!(
                        "| (N={n}: 最大÷最小が2を超えたので周を{MORE_ROUNDS}に増やす) ||||||||"
                    );
                    rounds = MORE_ROUNDS;
                }
            }
        }
        for (shape, kept) in times.iter().enumerate() {
            let of = |at: usize| -> Vec<f64> { kept[1..].iter().map(|took| took[at]).collect() };
            let (mut all, mut line, mut tx) = (of(0), of(1), of(2));
            let min = all.iter().copied().fold(f64::MAX, f64::min);
            let max = all.iter().copied().fold(f64::MIN, f64::max);
            medians.push((
                shape,
                n,
                [median(&mut all), median(&mut line), median(&mut tx)],
                (min, max),
            ));
        }
    }
    println!();
    println!(
        "| 書き方 | N | 全体の中央値（ms） | 最小 | 最大 | 最大÷最小 | 列の中央値（ms） | transactionの中央値（ms） | 1行あたり（ms） | 1秒あたりの行数 |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|");
    for (shape, n, [all, line, tx], (min, max)) in medians {
        println!(
            "| {} | {n} | {all:.1} | {min:.1} | {max:.1} | {:.2} | {line:.1} | {tx:.1} | {:.4} | {:.0} |",
            ["A", "B"][shape],
            max / min,
            all / n as f64,
            n as f64 / (all / 1000.0),
        );
    }
}
