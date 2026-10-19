use serde_json::{Value, json};

use super::*;
use crate::domain::event_kind;
use crate::domain::ledger::{
    owner::{Owner, owner},
    testkit::{Log, ms, run_id},
};
use crate::domain::pre_claim::{SlotClass, pre_claim_intervals};
use crate::domain::stats::landing_utilization;

/// The landing slots as every attempt read all the events again: the
/// greatest id from its start whose time is at or before its end.
fn landing_slots_by_rescan(all: &[RunEvent], now_ms: i64, built_at: &str) -> Vec<LedgerRow> {
    let (attempts, _) = landing_utilization::spans(all, now_ms);
    let starts = all.iter().filter(|event| {
        event.kind == event_kind::INTEGRATION_STARTED
            && event.run_id.is_some()
            && timestamp_millis(&event.created_at).is_some()
    });
    starts
        .zip(attempts)
        .map(|(start, attempt)| {
            let mut row = LedgerRow::new(
                LedgerKind::Queue,
                start.id.to_string(),
                attempt.task_id,
                built_at,
            );
            row.inputs_from = start.id;
            row.inputs_through.events = if attempt.open {
                all.last().map_or(start.id, |last| last.id)
            } else {
                all.iter()
                    .filter(|event| {
                        event.id >= start.id
                            && timestamp_millis(&event.created_at)
                                .is_some_and(|at| at <= attempt.end)
                    })
                    .map(|event| event.id)
                    .max()
                    .unwrap_or(start.id)
            };
            row.started_at = Some(attempt.start);
            row.ended_at = (!attempt.open).then_some(attempt.end);
            row.outcome =
                (!attempt.open).then(|| if attempt.landed { "landed" } else { "left" }.to_owned());
            row.subject = Some(Subject {
                kind: "landing_slot".into(),
                id: start
                    .run_id
                    .as_ref()
                    .map(|run| run.as_str().to_owned())
                    .unwrap_or_default(),
                ..Subject::default()
            });
            row
        })
        .collect()
}

/// Three attempts: one landed, one left by a deferral whose clock ran
/// behind, and one still open; an event with an unreadable time and a
/// slots-full interval of the queue in between.
fn log(open: bool) -> Log {
    let (a, b, c) = (run_id(1), run_id(2), run_id(3));
    let mut log = Log::default();
    log.add(Some(1), Some(&a), "landing_queued", json!({}), 0)
        .add(Some(1), Some(&a), "integration_started", json!({}), 10)
        .add(
            None,
            None,
            "slots_full_started",
            json!({"supervisor": "s1", "class": "heavy"}),
            15,
        )
        .add(Some(1), Some(&a), "run_integrated", json!({}), 40)
        .add(Some(2), Some(&b), "integration_started", json!({}), 50)
        .add(Some(9), None, "task_created", json!({}), 45)
        .add(
            None,
            None,
            "slots_full_ended",
            json!({"supervisor": "s1", "class": "heavy"}),
            55,
        )
        .add(Some(2), Some(&b), "integration_deferred", json!({}), 48)
        .add(Some(2), Some(&b), "run_integrated", json!({}), 70)
        .add(None, None, "mark_added", json!({"mark": "m"}), 72)
        .add(
            None,
            None,
            "slots_full_started",
            json!({"supervisor": "s2", "class": "light"}),
            74,
        );
    log.events[5].created_at = "not a time".into();
    if open {
        log.add(Some(3), Some(&c), "integration_started", json!({}), 80)
            .add(Some(3), Some(&c), "integration_started", json!({}), 85);
        log.events.last_mut().unwrap().created_at = "garbled".into();
        log.add(Some(9), None, "task_created", json!({}), 90);
    }
    log
}

fn queue_owned(log: &Log) -> Vec<RunEvent> {
    log.events
        .iter()
        .filter(|event| event.run_id.is_none() && event.task_id.is_none())
        .cloned()
        .collect()
}

#[test]
fn the_landing_slots_are_the_rows_a_rescan_of_every_attempt_gives() {
    for open in [false, true] {
        let log = log(open);
        let now = ms(100);
        let slots = queue_rows(&[], &log.events, now, "b");
        assert_eq!(
            slots,
            landing_slots_by_rescan(&log.events, now, "b"),
            "{open}"
        );
        assert_eq!(slots.len(), if open { 3 } else { 2 });
        assert_eq!(slots[0].outcome.as_deref(), Some("landed"));
        assert_eq!(slots[0].inputs_through.events, EventId::new(4));
        assert_eq!(slots[1].outcome.as_deref(), Some("left"));
        assert_eq!(slots[1].ended_at, Some(ms(50)));
        if open {
            assert_eq!(slots[2].outcome, None);
            assert_eq!(slots[2].inputs_through.events, log.last());
        }
    }
}

/// The whole row a queue event `start` of `log` folds to, owning
/// `records` (ids), ended at `ended_at` with `outcome`.
fn expected_row(
    log: &Log,
    start: i64,
    records: &[i64],
    subject: (&str, &str),
    ended_at: Option<i64>,
    outcome: Option<&str>,
    detail: Value,
) -> LedgerRow {
    let event = |id: i64| log.events[usize::try_from(id - 1).unwrap()].clone();
    let mut row = LedgerRow::new(LedgerKind::Queue, start.to_string(), None, "b");
    row.started_at = timestamp_millis(&event(start).created_at);
    row.ended_at = ended_at;
    row.outcome = outcome.map(str::to_owned);
    row.detail = detail;
    row.subject = Some(Subject {
        kind: subject.0.into(),
        id: subject.1.into(),
        ..Subject::default()
    });
    row.inputs_from = EventId::new(*records.first().unwrap());
    row.inputs_through.events = EventId::new(*records.last().unwrap());
    row.records = records.iter().map(|id| event(*id)).collect();
    row
}

/// Every row `queue_rows` gives is the one the rescan gave: the landing
/// slots as [`landing_slots_by_rescan`], an interval with the end it owns
/// (and an open one with none), and an event of its own, each with its
/// records and the range of its inputs.
#[test]
fn the_queue_rows_are_the_rows_a_rescan_of_every_attempt_gives() {
    for open in [false, true] {
        let log = log(open);
        let now = ms(100);
        let interval = |id: i64| {
            pre_claim_intervals(&log.events, now)
                .into_iter()
                .find(|interval| interval.id == EventId::new(id))
                .unwrap()
        };
        let (freed, waiting) = (interval(3), interval(11));
        let mut expected = vec![
            expected_row(
                &log,
                3,
                &[3, 7],
                ("slots_full", "s1"),
                Some(ms(55)),
                freed.end.as_deref(),
                json!({"interval": freed}),
            ),
            expected_row(
                &log,
                10,
                &[10],
                ("mark_added", ""),
                Some(ms(72)),
                None,
                json!({}),
            ),
            expected_row(
                &log,
                11,
                &[11],
                ("slots_full", "s2"),
                waiting.ended_at.as_deref().and_then(timestamp_millis),
                waiting.end.as_deref(),
                json!({"interval": waiting}),
            ),
        ];
        expected.extend(landing_slots_by_rescan(&log.events, now, "b"));
        expected.sort_by_key(|row| row.inputs_from);
        let rows = queue_rows(&queue_owned(&log), &log.events, now, "b");
        assert_eq!(rows, expected, "{open}");
        assert_eq!(rows.len(), if open { 6 } else { 5 });
    }
}

/// An attempt reads up to the greatest id whose time is at or before its
/// end, an event that came in later with an earlier time too.
#[test]
fn an_attempt_reads_a_later_event_of_an_earlier_time() {
    let mut log = log(false);
    let before = queue_rows(&[], &log.events, ms(100), "b");
    log.add(None, None, "mark_added", json!({"mark": "late"}), 39);
    let after = queue_rows(&[], &log.events, ms(100), "b");
    assert_eq!(after, landing_slots_by_rescan(&log.events, ms(100), "b"));
    assert_eq!(before[0].inputs_through.events, EventId::new(4));
    assert_eq!(after[0].inputs_through.events, log.last());
    assert_eq!(before[1].inputs_through.events, EventId::new(8));
    assert_eq!(after[1].inputs_through.events, log.last());
}

/// The queue's events of `log`, as the ledger's owner gives them.
fn owned_of(events: &[RunEvent]) -> Vec<RunEvent> {
    events
        .iter()
        .filter(|event| owner(event) == Owner::Queue)
        .cloned()
        .collect()
}

/// A book given the events a part at a time folds every row as the book
/// of all of them at once does, and each part names every row whose
/// inputs it reached.
#[test]
fn a_book_read_a_part_at_a_time_folds_the_rows_of_all_at_once() {
    let mut late = log(true);
    late.add(None, None, "mark_added", json!({"mark": "late"}), 39);
    for log in [log(false), late, gone_log()] {
        let now = ms(10_000);
        let whole = queue_rows(&owned_of(&log.events), &log.events, now, "b");
        for split in 0..=log.events.len() {
            let (mut lives, mut pre_claim, mut book) = (
                Lives::default(),
                PreClaimBook::default(),
                QueueBook::default(),
            );
            let mut touched = BTreeSet::new();
            let mut read = |events: &[RunEvent], touched: &mut BTreeSet<_>| {
                for event in events {
                    lives.add(event);
                    let moved = pre_claim.add(event);
                    book.add(event, owner(event) == Owner::Queue, moved, touched);
                }
            };
            read(&log.events[..split], &mut touched);
            touched.clear();
            read(&log.events[split..], &mut touched);
            let owned = owned_of(&log.events);
            let at = At {
                owned: &owned,
                pre_claim: &pre_claim,
                lives: &lives,
                now_ms: now,
                built_at: "b",
            };
            let mut rows: Vec<LedgerRow> =
                book.keys().filter_map(|key| book.row(&key, &at)).collect();
            rows.sort_by_key(|row| row.inputs_from);
            assert_eq!(rows, whole, "{split}");
            let after = log
                .events
                .get(split.wrapping_sub(1))
                .map_or(EventId::new(0), |e| e.id);
            for row in &rows {
                if row.inputs_through.last() > after {
                    assert!(
                        touched.contains(&(LedgerKind::Queue, row.key.clone()))
                            || book.open_slot().as_deref() == Some(row.key.as_str()),
                        "{split}: {}",
                        row.key
                    );
                }
            }
        }
    }
}

/// Two supervisors whose lives end, each with a stream and a slots-full
/// interval open at the end: `s3` stops (its heartbeat before its stop,
/// and one more of its events after it), `s4` falls silent; `s5` lives on
/// with its stream open.
fn gone_log() -> Log {
    let mut log = Log::default();
    log.add(
        None,
        None,
        "supervisor_started",
        json!({"supervisor": "s3"}),
        0,
    )
    .add(
        None,
        None,
        "live_alert_baseline",
        json!({"supervisor": "s3", "version": 1}),
        5,
    )
    .add(
        None,
        None,
        "slots_full_started",
        json!({"supervisor": "s3", "class": "heavy"}),
        6,
    )
    .add(
        None,
        None,
        "live_alert_input_changed",
        json!({"supervisor": "s3", "version": 2}),
        8,
    )
    .add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s4"}),
        9,
    )
    .add(
        None,
        None,
        "live_alert_baseline",
        json!({"supervisor": "s4"}),
        10,
    )
    .add(
        None,
        None,
        "slots_full_started",
        json!({"supervisor": "s4", "class": "light"}),
        11,
    )
    .add(
        None,
        None,
        "supervisor_stopped",
        json!({"supervisor": "s3", "last_heartbeat_at": ms(7) / 1000}),
        20,
    )
    .add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s3"}),
        25,
    )
    .add(
        None,
        None,
        "live_alert_baseline",
        json!({"supervisor": "s5"}),
        9_990,
    )
    .add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s5"}),
        9_995,
    );
    log
}

/// The slots-full interval of `supervisor` started by `start` of `log`,
/// ended when its supervisor's life ended at `end_secs`.
fn gone_interval(
    log: &Log,
    start: i64,
    supervisor: &str,
    class: SlotClass,
    end_secs: i64,
    ended_by: Option<&str>,
) -> PreClaimInterval {
    PreClaimInterval {
        id: EventId::new(start),
        kind: IntervalKind::SlotsFull,
        supervisor: Some(supervisor.into()),
        task_id: None,
        reason: None,
        class: Some(class),
        started_at: log.events[usize::try_from(start - 1).unwrap()]
            .created_at
            .clone(),
        ended_at: Some(crate::domain::marks::utc_text(ms(end_secs))),
        end: Some(claim_hold::ENDED_SUPERVISOR_GONE.into()),
        ended_by: ended_by.map(str::to_owned),
    }
}

/// The streams and slots-full intervals a supervisor's life ended are the
/// rows each supervisor's own scan of every event gave, written out whole:
/// ended at its stop (its heartbeat) or its last event, read up to the
/// last event naming it; a supervisor that lives keeps its stream open.
#[test]
fn the_rows_a_supervisors_end_ends_are_the_rows_a_scan_per_supervisor_gives() {
    let log = gone_log();
    let now = ms(10_000);
    let gone = Some(claim_hold::ENDED_SUPERVISOR_GONE);
    let plain = |id: i64, kind: &str, about: &str| {
        let at = timestamp_millis(&log.events[usize::try_from(id - 1).unwrap()].created_at);
        expected_row(&log, id, &[id], (kind, about), at, None, json!({}))
    };
    let life = |mut row: LedgerRow, through: i64| {
        row.inputs_through.supervisor = Some(EventId::new(through));
        row
    };
    let s3_interval = gone_interval(&log, 3, "s3", SlotClass::Heavy, 7, Some("s3"));
    let s4_interval = gone_interval(&log, 7, "s4", SlotClass::Light, 11, None);
    let expected = vec![
        plain(1, "supervisor_started", "s3"),
        life(
            expected_row(
                &log,
                2,
                &[2, 4],
                ("live_alert_stream", "s3"),
                Some(ms(7)),
                gone,
                json!({"baseline": true, "version": 2}),
            ),
            9,
        ),
        life(
            expected_row(
                &log,
                3,
                &[3],
                ("slots_full", "s3"),
                Some(ms(7)),
                gone,
                json!({"interval": s3_interval}),
            ),
            9,
        ),
        plain(5, "supervisor_alive", "s4"),
        life(
            expected_row(
                &log,
                6,
                &[6],
                ("live_alert_stream", "s4"),
                Some(ms(11)),
                gone,
                json!({"baseline": true}),
            ),
            7,
        ),
        life(
            expected_row(
                &log,
                7,
                &[7],
                ("slots_full", "s4"),
                Some(ms(11)),
                gone,
                json!({"interval": s4_interval}),
            ),
            7,
        ),
        plain(8, "supervisor_stopped", "s3"),
        plain(9, "supervisor_alive", "s3"),
        expected_row(
            &log,
            10,
            &[10],
            ("live_alert_stream", "s5"),
            None,
            None,
            json!({"baseline": true}),
        ),
        plain(11, "supervisor_alive", "s5"),
    ];
    let rows = queue_rows(&log.events, &log.events, now, "b");
    assert_eq!(rows, expected);
}

/// The landing slots as each event scanned the attempts ended at or after
/// its time, latest first while they were, and set them to read up to it:
/// each slot's greatest id read (the open one's none), the keys each part
/// of `events` cut at `splits` put into `touched`, and how many attempts
/// the scan looked at.
fn slots_by_scan(
    events: &[RunEvent],
    splits: &[usize],
) -> (Vec<Option<EventId>>, Vec<BTreeSet<String>>, usize) {
    let mut attempts = landing_utilization::Attempts::default();
    let mut slots: Vec<(EventId, EventId)> = Vec::new();
    let (mut parts, mut looked) = (vec![BTreeSet::new()], 0);
    for (index, event) in events.iter().enumerate() {
        if splits.contains(&index) {
            parts.push(BTreeSet::new());
        }
        let touched = parts.last_mut().unwrap();
        let Some(at) = timestamp_millis(&event.created_at) else {
            continue;
        };
        if let Some(run) = &event.run_id
            && attempts.add(event, run, at).started.is_some()
        {
            slots.push((event.id, event.id));
        }
        for index in (0..slots.len()).rev() {
            if attempts.open() == Some(index) {
                continue;
            }
            looked += 1;
            if attempts.get(index, 0).unwrap().end < at {
                break;
            }
            slots[index].1 = slots[index].1.max(event.id);
            touched.insert(slots[index].0.to_string());
        }
        if let Some(open) = attempts.open() {
            touched.insert(slots[open].0.to_string());
        }
    }
    let throughs = slots
        .iter()
        .enumerate()
        .map(|(index, slot)| (attempts.open() != Some(index)).then_some(slot.1))
        .collect();
    (throughs, parts, looked)
}

/// Attempts that end at one time, events that go back in time, and an
/// attempt still open, among the queue's own events.
fn tangled_log() -> Log {
    let mut log = Log::default();
    let runs: Vec<String> = (1..=7).map(run_id).collect();
    for (task, run) in runs.iter().take(4).enumerate() {
        let task = Some(i64::try_from(task).unwrap() + 1);
        log.add(task, Some(run), "integration_started", json!({}), 10)
            .add(task, Some(run), "run_integrated", json!({}), 10);
    }
    let mark = |log: &mut Log, secs: i64| {
        log.add(None, None, "mark_added", json!({"mark": "m"}), secs);
    };
    mark(&mut log, 10);
    mark(&mut log, 10);
    log.add(Some(5), Some(&runs[4]), "integration_started", json!({}), 8)
        .add(Some(5), Some(&runs[4]), "integration_held", json!({}), 7);
    mark(&mut log, 3);
    log.add(
        Some(6),
        Some(&runs[5]),
        "integration_started",
        json!({}),
        12,
    )
    .add(Some(6), Some(&runs[5]), "run_integrated", json!({}), 11);
    mark(&mut log, 10);
    mark(&mut log, 20);
    log.add(
        Some(7),
        Some(&runs[6]),
        "integration_started",
        json!({}),
        25,
    );
    mark(&mut log, 9);
    log
}

/// The queue rows and the slots' keys a book read a part at a time put
/// into `touched` (emptied between the parts cut at `splits`).
fn book_by_parts(log: &Log, splits: &[usize], now: i64) -> (Vec<LedgerRow>, Vec<BTreeSet<String>>) {
    let (mut lives, mut pre_claim, mut book) = (
        Lives::default(),
        PreClaimBook::default(),
        QueueBook::default(),
    );
    let mut parts = vec![BTreeSet::new()];
    for (index, event) in log.events.iter().enumerate() {
        if splits.contains(&index) {
            parts.push(BTreeSet::new());
        }
        lives.add(event);
        let moved = pre_claim.add(event);
        book.add(
            event,
            owner(event) == Owner::Queue,
            moved,
            parts.last_mut().unwrap(),
        );
    }
    let slots: BTreeSet<String> = book
        .slots
        .iter()
        .map(|slot| slot.start.to_string())
        .collect();
    let parts = parts
        .into_iter()
        .map(|touched| {
            touched
                .into_iter()
                .filter(|(_, key)| slots.contains(key))
                .map(|(_, key)| key)
                .collect()
        })
        .collect();
    let owned = owned_of(&log.events);
    let at = At {
        owned: &owned,
        pre_claim: &pre_claim,
        lives: &lives,
        now_ms: now,
        built_at: "b",
    };
    let mut rows: Vec<LedgerRow> = book.keys().filter_map(|key| book.row(&key, &at)).collect();
    rows.sort_by_key(|row| row.inputs_from);
    (rows, parts)
}

/// With attempts that end at one time and events that go back in time,
/// the book read whole or a part at a time folds the queue rows the scan
/// of every ended attempt per event gives, and each part puts into
/// `touched` the slots that scan put.
#[test]
fn the_slots_reached_by_runs_are_those_the_scan_per_event_reached() {
    let log = tangled_log();
    let now = ms(100);
    let whole = queue_rows(&owned_of(&log.events), &log.events, now, "b");
    let slot_rows: Vec<&LedgerRow> = whole
        .iter()
        .filter(|row| {
            row.subject
                .as_ref()
                .is_some_and(|s| s.kind == "landing_slot")
        })
        .collect();
    let rescan = landing_slots_by_rescan(&log.events, now, "b");
    assert_eq!(slot_rows, rescan.iter().collect::<Vec<_>>());
    let len = log.events.len();
    let mut cuts: Vec<Vec<usize>> = (0..=len).map(|split| vec![split]).collect();
    cuts.push((1..len).collect());
    cuts.push((1..len).step_by(3).collect());
    for splits in cuts {
        let (rows, parts) = book_by_parts(&log, &splits, now);
        assert_eq!(rows, whole, "{splits:?}");
        let (throughs, scanned_parts, _) = slots_by_scan(&log.events, &splits);
        assert_eq!(parts, scanned_parts, "{splits:?}");
        let reached: Vec<Option<EventId>> = slot_rows
            .iter()
            .map(|row| row.ended_at.map(|_| row.inputs_through.events))
            .collect();
        assert_eq!(reached, throughs, "{splits:?}");
    }
    // Reached back: the attempts at 10 read the mark at 9.
    assert_eq!(slot_rows[0].inputs_through.events, log.last());
}

/// `attempts` attempts that end at one time, then `events` events at it
/// and before it: every event reaches every attempt.
fn bunched_log(attempts: u8, events: usize) -> Log {
    let mut log = Log::default();
    for n in 1..=attempts {
        let run = run_id(n);
        log.add(
            Some(i64::from(n)),
            Some(&run),
            "integration_started",
            json!({}),
            10,
        )
        .add(
            Some(i64::from(n)),
            Some(&run),
            "run_integrated",
            json!({}),
            10,
        );
    }
    for secs in (0..events).map(|n| 10 - i64::try_from(n % 10).unwrap()) {
        log.add(None, None, "mark_added", json!({"mark": "m"}), secs);
    }
    log
}

fn scanned_count() -> usize {
    SCANNED.with(std::cell::Cell::get)
}

/// The attempts and runs of them the book looks at stay within a bound
/// linear in the events, read whole or in two parts, where the scan per
/// event looked at every attempt for every event.
#[test]
fn the_slots_cost_a_bound_linear_in_the_events() {
    for (attempts, events) in [(20, 20), (60, 60), (120, 240)] {
        let log = bunched_log(attempts, events);
        let len = log.events.len();
        let (_, _, looked) = slots_by_scan(&log.events, &[]);
        assert!(looked >= usize::from(attempts) * events, "{looked}");

        let before = scanned_count();
        queue_rows(&[], &log.events, ms(100), "b");
        let whole = scanned_count() - before;
        assert!(
            whole <= 2 * len,
            "{attempts}×{events}: {whole} > {}",
            2 * len
        );

        let before = scanned_count();
        book_by_parts(&log, &[len / 2], ms(100));
        let parts = scanned_count() - before;
        assert!(
            parts <= 3 * len,
            "{attempts}×{events}: {parts} > {}",
            3 * len
        );
    }
}
