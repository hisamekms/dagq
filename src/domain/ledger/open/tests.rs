//! What a run row leaves open at its end is what the whole read of the
//! earlier records leaves open of the same events, whatever the settled
//! history beside it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::json;

use super::*;
use crate::domain::ledger::build;
use crate::domain::ledger::testkit::{DAY, Log, READ_AT_DAY, T0, log_of_a_time, numbered, run_id};
use crate::domain::ledger::window::{EarlierRecords, WindowQuery, WindowRows};

const NOW: i64 = T0 + READ_AT_DAY * DAY;
const BEFORE: i64 = 150 * DAY;
/// The time run 73's ask is answered and run 74's session closed: the
/// events before it are those of an earlier pass.
pub(crate) const CLOSED_AT: i64 = (READ_AT_DAY - 1) * DAY;

/// The run id of the settled unit `k`.
fn unit_run(k: usize) -> String {
    format!("{k:08x}-3333-4333-8333-333333333333")
}

/// [`log_of_a_time`] (run 7 holds its slot after its task's cancel, run 8
/// ended with an ask open, run 10 with a session open) with: run 70 whose
/// session is closed by an old close naming no open, run 71 with two
/// sessions and the second closed twice, run 72 whose landing waits, run 73
/// whose ask and run 74 whose session close at [`CLOSED_AT`], and
/// `settled` units outside the range, each a run landed with an ask
/// answered and a session closed. Sorted by time and numbered, the closes
/// of runs 70 and 71 then named as written.
pub(crate) fn log_left_open(settled: usize) -> Log {
    let mut log = log_of_a_time();
    let run = |n: u8| run_id(n);
    let session = |tag: &str| json!({"kind": "review", "provider": "claude", "tag": tag});
    let close = |closes: &str| json!({"kind": "review", "provider": "claude", "closes": closes});
    log.landed(70, &run(70), BEFORE)
        .add(
            Some(70),
            Some(&run(70)),
            "session_opened",
            session("70"),
            BEFORE + 1,
        )
        .add(
            Some(70),
            Some(&run(70)),
            "session_closed",
            close(""),
            BEFORE + 2,
        );
    log.landed(71, &run(71), BEFORE + 5000)
        .add(
            Some(71),
            Some(&run(71)),
            "session_opened",
            session("71a"),
            BEFORE + 5001,
        )
        .add(
            Some(71),
            Some(&run(71)),
            "session_opened",
            session("71b"),
            BEFORE + 5002,
        )
        .add(
            Some(71),
            Some(&run(71)),
            "session_closed",
            close("71b"),
            BEFORE + 5003,
        )
        .add(
            Some(71),
            Some(&run(71)),
            "session_closed",
            close("71b"),
            BEFORE + 5004,
        );
    log.worked(72, &run(72), BEFORE + 10_000);
    log.landed(73, &run(73), BEFORE + 20_000)
        .add(
            Some(73),
            Some(&run(73)),
            "ask_opened",
            json!({"ask_id": 7073, "kind": "worker_question"}),
            BEFORE + 21_000,
        )
        .add(
            Some(73),
            Some(&run(73)),
            "ask_answered",
            json!({"ask_id": 7073}),
            CLOSED_AT,
        );
    log.landed(74, &run(74), BEFORE + 30_000)
        .add(
            Some(74),
            Some(&run(74)),
            "session_opened",
            json!({"kind": "probe", "provider": "claude", "tag": "74"}),
            BEFORE + 31_000,
        )
        .add(
            Some(74),
            Some(&run(74)),
            "session_closed",
            json!({"kind": "probe", "provider": "claude", "closes": "74"}),
            CLOSED_AT + 1,
        );
    let span = 139 * DAY;
    for k in 0..settled {
        let t = 11 * DAY + i64::try_from(k).unwrap() * span / i64::try_from(settled).unwrap();
        let task = 100_000 + i64::try_from(k).unwrap();
        let r = unit_run(k);
        log.landed(task, &r, t)
            .add(
                Some(task),
                Some(&r),
                "ask_opened",
                json!({"ask_id": 900_000 + task, "kind": "worker_question"}),
                t + 1000,
            )
            .add(
                Some(task),
                Some(&r),
                "ask_answered",
                json!({"ask_id": 900_000 + task}),
                t + 1050,
            )
            .add(
                Some(task),
                Some(&r),
                "session_opened",
                json!({"kind": "worker", "provider": "claude"}),
                t + 1100,
            )
            .add(
                Some(task),
                Some(&r),
                "session_closed",
                json!({"kind": "worker", "provider": "claude"}),
                t + 1200,
            );
    }
    log.events.sort_by_key(|event| event.created_at.clone());
    let mut log = numbered(log);
    let opens: HashMap<String, i64> = log
        .events
        .iter()
        .filter(|e| e.kind == EventKind::SessionOpened)
        .filter_map(|e| Some((e.payload["tag"].as_str()?.to_owned(), e.id.as_i64())))
        .collect();
    for event in &mut log.events {
        let Some(closes) = event.payload["closes"].as_str().map(str::to_owned) else {
            continue;
        };
        let payload = event.payload.as_object_mut().unwrap();
        match opens.get(&closes) {
            Some(id) => payload.insert("opened_event_id".into(), json!(id)),
            None => payload.remove("opened_event_id"),
        };
    }
    log
}

pub(crate) fn rows_of(log: &Log) -> Vec<LedgerRow> {
    build(&log.events, NOW * 1000, "2030-01-01T00:00:00.000Z")
}

/// What each run row of `rows` leaves open, of those that leave any.
pub(crate) fn table_of(rows: &[LedgerRow]) -> BTreeMap<String, BTreeSet<Left>> {
    rows.iter()
        .map(|row| (row.key.clone(), left_open(row)))
        .filter(|(_, left)| !left.is_empty())
        .collect()
}

/// The runs [`WindowRows::take_earlier`] reads whole of every run row's
/// records, as a read from a time that leaves every row out.
fn wanted_by_the_whole_read(rows: &[LedgerRow]) -> BTreeSet<String> {
    let query = WindowQuery {
        read_from_ms: Some(NOW * 1000),
        ..WindowQuery::default()
    };
    let mut window = WindowRows::new(&query, 0);
    let records = rows
        .iter()
        .filter(|row| row.kind == LedgerKind::Run)
        .flat_map(|row| row.records.iter().cloned())
        .collect();
    window.take_earlier(EarlierRecords {
        records,
        executions: Vec::new(),
    });
    window
        .rows_wanted()
        .into_iter()
        .filter(|(kind, _)| *kind == LedgerKind::Run)
        .map(|(_, key)| key)
        .collect()
}

fn session_of(rows: &[LedgerRow], run: &str, tag: &str) -> Left {
    let row = rows.iter().find(|row| row.key == run).unwrap();
    let id = row
        .records
        .iter()
        .find(|e| e.kind == EventKind::SessionOpened && e.payload["tag"] == tag)
        .unwrap()
        .id;
    Left::Session(id)
}

#[test]
fn the_runs_left_open_are_those_the_whole_read_of_the_earlier_records_reads_whole() {
    let rows = rows_of(&log_left_open(30));
    let table = table_of(&rows);
    let wants: BTreeSet<String> = table
        .iter()
        .filter(|(_, left)| left.iter().any(Left::wants_the_run))
        .map(|(key, _)| key.clone())
        .collect();
    assert_eq!(wants, wanted_by_the_whole_read(&rows));
    let of = |n: u8| table.get(&run_id(n)).cloned().unwrap_or_default();
    // Run 7 holds its slot after its task's cancel, run 8 ended with an
    // ask open, run 10 with a session never closed.
    assert_eq!(of(7), BTreeSet::from([Left::Slot]));
    assert_eq!(of(8), BTreeSet::from([Left::Ask(Some("77".to_owned()))]));
    assert!(matches!(
        of(10).into_iter().collect::<Vec<_>>().as_slice(),
        [Left::Session(_)]
    ));
    // An old close names no open and closes none; a second close of the
    // same open closes nothing more: both stay open as the whole read pairs them.
    assert_eq!(
        of(70),
        BTreeSet::from([session_of(&rows, &run_id(70), "70")])
    );
    assert_eq!(
        of(71),
        BTreeSet::from([session_of(&rows, &run_id(71), "71a")])
    );
    // A landing that waits, its slot held as it waits.
    assert_eq!(of(72), BTreeSet::from([Left::Slot, Left::Landing]));
    // Closed in the end, and the settled units: nothing left open.
    assert!(of(73).is_empty() && of(74).is_empty());
    assert!(table.keys().all(|key| !key.contains("-3333-")));
}

#[test]
fn a_close_a_later_pass_reads_takes_back_what_it_closed() {
    let log = log_left_open(3);
    let before: Vec<_> = log
        .events
        .iter()
        .filter(|e| {
            crate::domain::stats::timestamp_millis(&e.created_at) < Some((T0 + CLOSED_AT) * 1000)
        })
        .cloned()
        .collect();
    let earlier = table_of(&build(&before, NOW * 1000, "2030-01-01T00:00:00.000Z"));
    let later = table_of(&rows_of(&log));
    assert_eq!(
        earlier.get(&run_id(73)),
        Some(&BTreeSet::from([Left::Ask(Some("7073".to_owned()))]))
    );
    assert!(matches!(
        earlier
            .get(&run_id(74))
            .map(|left| left.iter().collect::<Vec<_>>())
            .as_deref(),
        Some([Left::Session(_)])
    ));
    assert_eq!(later.get(&run_id(73)), None);
    assert_eq!(later.get(&run_id(74)), None);
}

#[test]
fn the_runs_left_open_do_not_grow_with_the_settled_history() {
    // The same runs and the same things left open of them; a session by
    // its open's id, which the settled units before it move.
    let shape = |settled: usize| -> Vec<(String, Vec<String>)> {
        table_of(&rows_of(&log_left_open(settled)))
            .into_iter()
            .map(|(key, left)| {
                let left = left
                    .iter()
                    .map(|left| match left {
                        Left::Session(_) => "session".to_owned(),
                        other => other.text(),
                    })
                    .collect();
                (key, left)
            })
            .collect()
    };
    let small = shape(30);
    assert_eq!(small.len(), 6, "{small:?}");
    assert_eq!(small, shape(300));
}

#[test]
fn each_left_reads_back_from_its_text_and_another_row_leaves_nothing() {
    for left in [
        Left::Slot,
        Left::Ask(None),
        Left::Ask(Some("ask:7".to_owned())),
        Left::Session(EventId::new(42)),
        Left::Landing,
    ] {
        assert_eq!(Left::of_text(&left.text()), Some(left));
    }
    assert_eq!(Left::of_text("session:x"), None);
    assert_eq!(Left::of_text("other"), None);
    let rows = rows_of(&log_left_open(3));
    assert!(
        rows.iter()
            .filter(|row| row.kind != LedgerKind::Run)
            .all(|row| left_open(row).is_empty())
    );
}

#[test]
fn the_filling_goes_on_past_a_full_part_and_is_done_past_a_short_one() {
    assert_eq!(Filled::Not.after(), Some(None));
    assert_eq!(Filled::Through("k".to_owned()).after(), Some(Some("k")));
    assert_eq!(Filled::Done.after(), None);
    // A full page of keys, or a part cut short of its keys, goes on.
    assert_eq!(
        Filled::next(ROWS_PER_FILL, ROWS_PER_FILL, Some("k")),
        Filled::Through("k".to_owned())
    );
    assert_eq!(
        Filled::next(5, 2, Some("k")),
        Filled::Through("k".to_owned())
    );
    assert_eq!(Filled::next(5, 5, Some("k")), Filled::Done);
    assert_eq!(Filled::next(0, 0, None), Filled::Done);
    assert!(!part_spent(FILL_BYTES - 1, FILL_TIME / 2));
    assert!(part_spent(FILL_BYTES, std::time::Duration::ZERO));
    assert!(part_spent(0, FILL_TIME));
    assert!(Filled::Done.is_done() && !Filled::Not.is_done());
}
