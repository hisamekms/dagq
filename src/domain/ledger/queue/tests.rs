use serde_json::{Value, json};

use super::*;
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
        event.kind == EventKind::IntegrationStarted
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
        reach: EventId::new(start),
    }
}

/// The streams and slots-full intervals a supervisor's life ended are the
/// rows each supervisor's own scan of every event gave, written out whole:
/// ended at its stop (its heartbeat) or its last event, read up to the
/// last event naming it; a supervisor that lives keeps its stream open,
/// read up to the last event naming it too.
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
    // An open slots full or stream reads up to the last event naming its
    // supervisor, its life after its stop too.
    let reach = |mut row: LedgerRow, through: i64| {
        row.inputs_through.events = EventId::new(through);
        row
    };
    let s3_interval = gone_interval(&log, 3, "s3", SlotClass::Heavy, 7, Some("s3"));
    let s4_interval = gone_interval(&log, 7, "s4", SlotClass::Light, 11, None);
    let expected = vec![
        plain(1, "supervisor_started", "s3"),
        reach(
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
            9,
        ),
        reach(
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
            9,
        ),
        plain(5, "supervisor_alive", "s4"),
        reach(
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
        reach(
            expected_row(
                &log,
                10,
                &[10],
                ("live_alert_stream", "s5"),
                None,
                None,
                json!({"baseline": true}),
            ),
            11,
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

/// The queue book and the waits before a claim of `log`, read as the
/// builder reads its events.
fn books_of(log: &Log) -> (QueueBook, PreClaimBook) {
    let (mut pre_claim, mut book) = (PreClaimBook::default(), QueueBook::default());
    let mut touched = BTreeSet::new();
    for event in &log.events {
        let moved = pre_claim.add(event);
        book.add(event, owner(event) == Owner::Queue, moved, &mut touched);
    }
    (book, pre_claim)
}

/// A finding's record of `status`.
fn finding_record(status: &str) -> Value {
    json!({"finding_id": 1, findings::VALUES: {"id": 1, "target": "queue", "task_id": null,
        "run_id": null, "goal_id": null, "kind": "conflict_hotspot", "subject": "src/a.rs",
        "summary": "a", "impact": "normal", "status": status, "occurrences": 1, "last_seen_at": 0}})
}

/// A queue row no later event reaches is closed, so that the builder may
/// forget it once the store holds it settled: an event of its own, a
/// stream its next baseline replaced, an alert ended, a wait or a hold an
/// event of its own ended. An open stream, alert or wait, a finding, a
/// slots full or a hold a supervisor's life ended or reaches, a landing
/// slot's row and a key of no row are not.
#[test]
fn a_queue_row_is_closed_once_no_later_event_reaches_it() {
    let alert = json!({"supervisor": "s1", "judgment": "slots", "kind": "idle"});
    let mut log = Log::default();
    log.add(None, None, "mark_added", json!({"mark": "m"}), 0) // 1
        .add(
            None,
            None,
            "live_alert_baseline",
            json!({"supervisor": "s1", "version": 1}),
            1,
        ) // 2: replaced by 4
        .add(None, None, "live_alert_started", alert.clone(), 2) // 3: ended by 5
        .add(
            None,
            None,
            "live_alert_baseline",
            json!({"supervisor": "s1", "version": 2}),
            3,
        ) // 4: open
        .add(None, None, "live_alert_ended", alert, 4)
        .add(
            None,
            None,
            "live_alert_started",
            json!({"supervisor": "s1", "judgment": "running", "kind": "stall"}),
            5,
        ) // 6: open
        .add(None, None, "claim_held", json!({"supervisor": "s1"}), 6) // 7: ended by 8
        .add(None, None, "claim_resumed", json!({"supervisor": "s1"}), 7)
        .add(
            Some(5),
            None,
            "claim_deferred",
            json!({"supervisor": "s1", "reason": "paths"}),
            8,
        ) // 9: open
        .add(
            None,
            None,
            "slots_full_started",
            json!({"supervisor": "s1", "class": "heavy"}),
            9,
        ) // 10: freed by 11
        .add(
            None,
            None,
            "slots_full_ended",
            json!({"supervisor": "s1", "class": "heavy"}),
            10,
        )
        .add(
            None,
            None,
            "slots_full_started",
            json!({"supervisor": "s2", "class": "heavy"}),
            11,
        ) // 12: open, its supervisor's life may end it
        .add(None, None, "claim_held", json!({"supervisor": "s3"}), 12) // 13: its stop ends it
        .add(
            None,
            None,
            "supervisor_stopped",
            json!({"supervisor": "s3"}),
            13,
        )
        .add(
            None,
            None,
            "finding_recorded",
            finding_record("resolved"),
            14,
        ) // 15
        .add(None, None, "landing_held", json!({"supervisor": "s1"}), 15) // 16: ended by 17
        .add(
            None,
            None,
            "landing_resumed",
            json!({"supervisor": "s1"}),
            16,
        )
        .add(None, None, "landing_held", json!({"supervisor": "s1"}), 17) // 18: open
        .add(
            Some(1),
            Some(&run_id(1)),
            "integration_started",
            json!({}),
            18,
        ); // 19: a slot
    let (book, pre_claim) = books_of(&log);
    let closed: Vec<i64> = (1..=20)
        .filter(|id| book.closed(&id.to_string(), &pre_claim))
        .collect();
    // An end (a resume, a freed slots full, the stop that ended a hold)
    // is its interval's record and no row of its own.
    assert_eq!(closed, vec![1, 2, 3, 7, 10, 16]);
    assert!(!book.closed("x", &pre_claim));

    let mut forgot = book.clone();
    for id in &closed {
        forgot.forget(&id.to_string());
    }
    let owned = owned_of(&log.events);
    let at = At {
        owned: &owned,
        pre_claim: &pre_claim,
        lives: &Lives::default(),
        now_ms: ms(100),
        built_at: "b",
    };
    for id in 1..=20 {
        let key = id.to_string();
        let row = forgot.row(&key, &at);
        assert_eq!(
            row.is_none(),
            closed.contains(&id) || book.row(&key, &at).is_none()
        );
    }
}

/// The heads of the rows the store holds of `events` folded at `now`
/// ([`QueueHead::of`]), each settled with `settled`.
fn heads_held(events: &[RunEvent], now: i64, settled: bool) -> Vec<QueueHead> {
    queue_rows(&owned_of(events), events, now, "b")
        .iter()
        .map(|row| {
            let mut head = QueueHead::of(row);
            head.head.is_final = settled;
            head
        })
        .collect()
}

/// A stream whose row the store holds settled and ended is the fold of its
/// life's end as it is: the first close of a book made again does not name
/// it for a fold, so that a start holds the records of no such row for each
/// life of its history. One not settled, or settled with no end, is named,
/// and an event naming the supervisor has a settled one read again, as the
/// fold of every event does.
#[test]
fn a_stream_settled_and_ended_is_not_closed_again_until_an_event_names_its_supervisor() {
    let mut log = Log::default();
    log.add(
        None,
        None,
        "live_alert_baseline",
        json!({"supervisor": "s1", "version": 1}),
        0,
    )
    .add(
        None,
        None,
        "supervisor_stopped",
        json!({"supervisor": "s1"}),
        1,
    );
    let now = ms(40);
    let start = log.events[0].id;
    let last = log.events.last().unwrap().id;
    let mut lives = Lives::default();
    log.events.iter().for_each(|event| lives.add(event));
    let (_, pre_claim) = books_of(&log);
    let book_of = |settled: bool, ended: bool| {
        let heads: Vec<QueueHead> = heads_held(&log.events, now, settled)
            .into_iter()
            .map(|mut head| {
                assert!(head.ended_at.is_some(), "{}", head.head.key);
                if !ended {
                    head.ended_at = None;
                }
                head
            })
            .collect();
        QueueBook::from_heads(&heads, &pre_claim, last)
    };

    assert_eq!(book_of(false, true).close_lives(&lives, now), vec![start]);
    assert_eq!(book_of(true, false).close_lives(&lives, now), vec![start]);
    let mut settled = book_of(true, true);
    assert_eq!(settled.close_lives(&lives, now), Vec::new());

    let mut named = log.clone();
    named.add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s1"}),
        2,
    );
    named.add(
        None,
        None,
        "supervisor_stopped",
        json!({"supervisor": "s1"}),
        3,
    );
    for event in &named.events[log.events.len()..] {
        lives.add(event);
        settled.add(event, false, Moved::default(), &mut BTreeSet::new());
    }
    assert_eq!(settled.close_lives(&lives, now), vec![start]);
}

/// The book made again from the rows the store holds of the events up to a
/// position, settled or not, goes on with the rest as the book of every
/// event: each event after it changes the same rows, and the rows are the
/// fold of every event, but the rows settled no later event reaches, which
/// it forgot ([`crate::domain::ledger::testkit::queue_across`]): the
/// finding's next record goes to its row, the alert's end and the stream's
/// next baseline to the alert and the stream open across the position, an
/// event timed before the first landing's end reaches it, the lives of the
/// slots full's and the stopped hold's supervisors reach them, and the
/// deferral's claim and the landing's end close what was open.
#[test]
fn a_book_made_again_from_the_rows_goes_on_as_the_fold_of_every_event() {
    let (log, position) = crate::domain::ledger::testkit::queue_across();
    let (before, after) = log.events.split_at(position);
    let now = ms(40);
    let owned = owned_of(&log.events);
    let whole_rows = queue_rows(&owned, &log.events, now, "b");
    for settled in [false, true] {
        let (mut lives, mut pre_claim, mut whole) = (
            Lives::default(),
            PreClaimBook::default(),
            QueueBook::default(),
        );
        for event in before {
            lives.add(event);
            let moved = pre_claim.add(event);
            whole.add(
                event,
                owner(event) == Owner::Queue,
                moved,
                &mut BTreeSet::new(),
            );
        }
        let held = heads_held(before, now, settled);
        let mut rebuilt = QueueBook::from_heads(&held, &pre_claim, before.last().unwrap().id);
        for event in after {
            lives.add(event);
            let moved = pre_claim.add(event);
            let owned = owner(event) == Owner::Queue;
            let (mut by_rows, mut by_all) = (BTreeSet::new(), BTreeSet::new());
            rebuilt.add(event, owned, moved, &mut by_rows);
            whole.add(event, owned, moved, &mut by_all);
            assert_eq!(by_rows, by_all, "{settled}: {}", event.id);
        }
        assert_eq!(
            rebuilt.close_lives(&lives, now),
            whole.close_lives(&lives, now)
        );
        let at = At {
            owned: &owned,
            pre_claim: &pre_claim,
            lives: &lives,
            now_ms: now,
            built_at: "b",
        };
        let rows: BTreeMap<String, LedgerRow> = rebuilt
            .keys()
            .filter_map(|key| Some((key.clone(), rebuilt.row(&key, &at)?)))
            .collect();
        for row in &whole_rows {
            match rows.get(&row.key) {
                Some(rebuilt) => assert_eq!(rebuilt, row, "{settled}: {}", row.key),
                // Forgotten: settled, and no later event reached it.
                None => {
                    assert!(settled, "{}", row.key);
                    assert!(row.inputs_through.last() <= before.last().unwrap().id);
                }
            }
        }
        assert!(
            rows.keys()
                .all(|key| whole_rows.iter().any(|row| &row.key == key))
        );
        let forgot: Vec<&str> = whole_rows
            .iter()
            .filter(|row| !rows.contains_key(&row.key))
            .map(|row| row.key.as_str())
            .collect();
        assert_eq!(
            forgot,
            if settled { vec!["1", "12"] } else { vec![] },
            "{settled}"
        );

        let key_of = |id: i64| id.to_string();
        let row = |id: i64| &rows[&key_of(id)];
        let ids = |row: &LedgerRow| -> Vec<i64> {
            row.records.iter().map(|event| event.id.as_i64()).collect()
        };
        assert_eq!(ids(row(4)), [4, 17]);
        assert_eq!(row(4).outcome.as_deref(), Some("resolved"));
        assert_eq!(ids(row(3)), [3, 16]);
        assert_eq!(row(2).outcome.as_deref(), Some("replaced"));
        assert_eq!(ids(row(2)), [2, 13, 15]);
        assert_eq!(row(2).inputs_through.events.as_i64(), 22);
        assert_eq!(row(9).inputs_through.events.as_i64(), 14);
        assert_eq!(row(11).outcome.as_deref(), Some("landed"));
        assert_eq!(row(8).inputs_through.events.as_i64(), 21);
    }
}

/// A book of only the events after the position gets the cases wrong: the
/// finding's next record starts a row of its own, the alert's end and the
/// event timed before the first landing's end find no row.
#[test]
fn a_queue_book_of_the_events_after_the_position_alone_misses_the_rows_held() {
    let (log, position) = crate::domain::ledger::testkit::queue_across();
    let (mut pre_claim, mut book) = (PreClaimBook::default(), QueueBook::default());
    for event in &log.events[position..] {
        let moved = pre_claim.add(event);
        book.add(
            event,
            owner(event) == Owner::Queue,
            moved,
            &mut BTreeSet::new(),
        );
    }
    assert!(book.records_of("4").is_none());
    assert_eq!(book.records_of("17"), Some([EventId::new(17)].as_slice()));
    assert!(book.records_of("3").is_none());
    assert!(book.open_slot().is_none());
}

/// A row of another kind and a key of no event id are no rows of the book.
#[test]
fn the_queue_book_takes_only_the_queue_rows_of_its_keys() {
    let (log, position) = crate::domain::ledger::testkit::queue_across();
    let mut heads = heads_held(&log.events[..position], ms(40), false);
    let mut other = heads[0].clone();
    other.head.kind = LedgerKind::Session;
    other.head.key = "99".into();
    let mut garbled = heads[0].clone();
    garbled.head.key = "x".into();
    heads.extend([other, garbled]);
    let book = QueueBook::from_heads(&heads, &PreClaimBook::default(), log.last());
    assert!(book.records_of("99").is_none());
    assert!(
        book.keys()
            .all(|key| key.parse::<i64>().is_ok() && key != "99")
    );
}

/// A head is left out only settled, of nothing past the position, and of
/// one event of its own, a stream replaced or an alert ended; the rest are
/// read whatever they are.
#[test]
fn a_head_is_forgotten_only_of_a_row_settled_and_closed_with_nothing_past_the_position() {
    let at = EventId::new(10);
    let of = |kind: &str, outcome: Option<&str>, ended: Option<i64>| {
        let mut row = LedgerRow::new(LedgerKind::Queue, "5".into(), None, "b");
        row.is_final = true;
        row.inputs_through.events = EventId::new(5);
        row.outcome = outcome.map(str::to_owned);
        row.ended_at = ended;
        row.subject = Some(Subject {
            kind: kind.to_owned(),
            ..Subject::default()
        });
        QueueHead::of(&row)
    };
    let held_all = Since {
        ms: i64::MIN,
        id: EventId::new(0),
    };
    for (head, forgotten) in [
        (of("mark_added", None, Some(1)), true),
        (of("", None, None), true),
        (of("live_alert_stream", Some("replaced"), Some(1)), true),
        (of("live_alert", Some("ended"), Some(1)), true),
        (of("live_alert_stream", None, None), false),
        (
            of(
                "live_alert_stream",
                Some(claim_hold::ENDED_SUPERVISOR_GONE),
                Some(1),
            ),
            false,
        ),
        (of("live_alert", None, None), false),
        (of("finding", Some("resolved"), Some(1)), false),
        (of("landing_slot", Some("landed"), Some(1)), false),
        (of("slots_full", Some(ENDED_FREED), Some(1)), false),
        (of("claim_hold", Some("x"), Some(1)), false),
        (of("claim_deferral", Some("x"), Some(1)), false),
        (of("landing_hold", Some("x"), Some(1)), false),
    ] {
        assert_eq!(head.forgotten_at(at, held_all), forgotten, "{head:?}");
    }
    let closed = of("mark_added", None, Some(1));
    let changed = |change: fn(&mut QueueHead)| {
        let mut head = closed.clone();
        change(&mut head);
        head.forgotten_at(at, held_all)
    };
    assert!(!changed(|head| head.head.is_final = false));
    assert!(!changed(|head| head.head.kind = LedgerKind::Session));
    assert!(!changed(
        |head| head.head.inputs_through.events = EventId::new(11)
    ));
    assert!(!changed(
        |head| head.head.inputs_through.supervisor = Some(EventId::new(11))
    ));
    assert!(!changed(|head| head.head.records = vec![EventId::new(11)]));
    assert!(changed(|head| head.head.records = vec![EventId::new(10)]));
    assert!(!changed(|head| head.head.ledger_v = LEDGER_V - 1));
}

/// A settled row no later event reaches stays in a book made of its head
/// while it is of an older version, its fold to be written again; the same
/// row of this version does not.
#[test]
fn a_book_keeps_a_closed_row_of_an_older_version() {
    let mut row = LedgerRow::new(LedgerKind::Queue, "5".into(), None, "b");
    row.is_final = true;
    row.inputs_through.events = EventId::new(5);
    row.records = vec![];
    row.subject = Some(Subject {
        kind: "mark_added".to_owned(),
        ..Subject::default()
    });
    let book = |row: &LedgerRow| {
        QueueBook::from_heads(
            &[QueueHead::of(row)],
            &PreClaimBook::default(),
            EventId::new(10),
        )
    };
    assert_eq!(book(&row).records_of("5"), None);
    row.ledger_v = LEDGER_V - 1;
    assert_eq!(book(&row).records_of("5"), Some(&[][..]));
}

/// A settled wait ended long before the position and the [`crate::domain::pre_claim::rows::Since`] is one
/// the books at the position are made of only while it is of an older
/// version, whose row folds out of them; a stream replaced is of neither.
#[test]
fn the_books_at_a_position_hold_the_waits_of_an_older_version() {
    use crate::domain::pre_claim::rows::{Since, holds_at};
    let row = |kind: &str, outcome: &str, ledger_v: u32| {
        let mut row = LedgerRow::new(LedgerKind::Queue, "5".into(), None, "b");
        row.is_final = true;
        row.ledger_v = ledger_v;
        row.ended_at = Some(1);
        row.inputs_through.events = EventId::new(5);
        row.outcome = Some(outcome.to_owned());
        row.subject = Some(Subject {
            kind: kind.to_owned(),
            ..Default::default()
        });
        row
    };
    let since = Since {
        ms: 1_000,
        id: EventId::new(100),
    };
    let at = EventId::new(200);
    assert!(!holds_at(&row("claim_deferral", "x", LEDGER_V), since, at));
    assert!(holds_at(
        &row("claim_deferral", "x", LEDGER_V - 1),
        since,
        at
    ));
    assert!(!holds_at(
        &row("live_alert_stream", "replaced", LEDGER_V - 1),
        since,
        at
    ));
}

/// A wait is left out by the [`Since`] of its position only settled, of
/// nothing past the position, ended before it by time and by id, and held
/// by no life; the rest are read as before.
#[test]
fn a_wait_is_forgotten_only_settled_before_the_since_and_held_by_no_life() {
    let at = EventId::new(10);
    let wait = |kind: &str, end: &str| {
        let mut row = LedgerRow::new(LedgerKind::Queue, "5".into(), None, "b");
        row.is_final = true;
        row.inputs_through.events = EventId::new(6);
        row.outcome = Some(end.to_owned());
        row.ended_at = Some(100);
        row.detail = json!({"interval": {"end": end, "supervisor": "s", "ended_by": "s"}});
        row.subject = Some(Subject {
            kind: kind.to_owned(),
            ..Subject::default()
        });
        QueueHead::of(&row)
    };
    let since = Since {
        ms: 200,
        id: EventId::new(7),
    };
    let ended = [
        wait("slots_full", ENDED_FREED),
        wait("claim_hold", "resumed"),
        wait("claim_deferral", "claimed"),
        wait("landing_hold", "resumed"),
    ];
    for head in &ended {
        assert!(head.forgotten_at(at, since), "{head:?}");
        let kept = |change: fn(&mut QueueHead)| {
            let mut head = head.clone();
            change(&mut head);
            !head.forgotten_at(at, since)
        };
        assert!(kept(|head| head.head.is_final = false), "not settled");
        assert!(kept(|head| head.ended_at = None), "open");
        assert!(
            kept(|head| head.outcome = Some(claim_hold::ENDED_SUPERVISOR_GONE.into())),
            "ended by a life"
        );
        assert!(
            kept(|head| head.head.inputs_through.events = EventId::new(11)),
            "read past the position"
        );
        assert!(
            kept(|head| head.head.inputs_through.supervisor = Some(EventId::new(11))),
            "read past the position by a life"
        );
        assert!(kept(|head| head.ended_at = Some(200)), "ended at the time");
        assert!(
            kept(|head| head.head.inputs_through.events = EventId::new(7)),
            "ended by the event"
        );
    }
    // What a life holds by the detail, though the outcome says otherwise.
    assert!(!wait("slots_full", "other").forgotten_at(at, since));
    let mut stopped = wait("claim_hold", claim_hold::ENDED_SUPERVISOR_GONE);
    stopped.outcome = Some("resumed".into());
    assert!(!stopped.forgotten_at(at, since));
}

/// A book held from a [`Since`] past the waits drops the settled one of each
/// kind an event ended, though it does not hold them, and keeps the other
/// waits as before (open, not settled, those a life reaches). A book of no
/// [`Since`] drops as it did: a wait it holds an event ended, and none it
/// does not hold.
#[test]
fn a_book_held_from_a_since_drops_the_waits_it_left_out_and_no_other() {
    let log = crate::domain::ledger::testkit::settled_waits_log(1);
    let rows = crate::domain::ledger::testkit::built_and_settled(&log.events, ms(100_000));
    let heads: Vec<QueueHead> = rows
        .iter()
        .filter(|row| row.kind == LedgerKind::Queue)
        .map(QueueHead::of)
        .collect();
    let settled_waits: Vec<&QueueHead> = heads
        .iter()
        .filter(|head| {
            head.head.is_final
                && head.ended_at.is_some()
                && head.outcome.as_deref() != Some(claim_hold::ENDED_SUPERVISOR_GONE)
                && wait_life_of(head).is_none()
                && matches!(of_kind(&head.subject_kind), Of::Interval(_))
        })
        .collect();
    let kinds: BTreeSet<&str> = settled_waits
        .iter()
        .map(|head| head.subject_kind.as_str())
        .collect();
    assert_eq!(
        kinds.into_iter().collect::<Vec<_>>(),
        ["claim_deferral", "claim_hold", "landing_hold", "slots_full"]
    );
    let has = |book: &QueueBook, head: &QueueHead| book.records_of(&head.head.key).is_some();

    // No Since: an empty book drops none of them, and the book of every
    // event every one.
    let none = QueueBook::from_heads(&heads, &PreClaimBook::default(), log.last());
    let every = QueueBook::from_heads(&heads, &PreClaimBook::of(&log.events), log.last());
    for head in &settled_waits {
        assert!(has(&none, head), "{head:?}");
        assert!(!has(&every, head), "{head:?}");
    }

    // Held from a Since past them: it drops them though it does not hold
    // them, and what it holds or what a life reaches as before.
    let mut held = PreClaimBook::default();
    held.held_from(Since::NONE);
    let book = QueueBook::from_heads(&heads, &held, log.last());
    for head in &settled_waits {
        assert!(!has(&book, head), "{head:?}");
        assert!(!book.closed(&head.head.key, &held), "no row: {head:?}");
    }
    let wait_heads = heads
        .iter()
        .filter(|head| matches!(of_kind(&head.subject_kind), Of::Interval(_)));
    for head in wait_heads {
        let dropped = settled_waits
            .iter()
            .any(|settled| settled.head.key == head.head.key);
        assert_eq!(has(&book, head), !dropped, "{head:?}");
    }

    // A wait the book holds is not left out, nor a hold on the landings,
    // which it does not count among its waits; and a book of no Since
    // leaves out none.
    let mut of_every = PreClaimBook::of(&log.events);
    let none_left_out = |book: &PreClaimBook, head: &QueueHead| {
        !book.left_out(EventId::new(head.head.key.parse().unwrap()))
    };
    for head in &settled_waits {
        assert!(none_left_out(&of_every, head), "{head:?}");
    }
    of_every.held_from(Since::NONE);
    for head in &settled_waits {
        assert!(none_left_out(&of_every, head), "{head:?}");
        assert!(!none_left_out(&held, head), "{head:?}");
    }
    let mut landing = Log::default();
    landing.add(None, None, "landing_held", json!({"supervisor": "s"}), 1);
    let start = landing.last();
    let mut open = PreClaimBook::of(&landing.events);
    assert!(!open.left_out(start));
    open.held_from(Since::NONE);
    assert!(!open.holds(start));
    assert!(!open.left_out(start));
    assert!(!PreClaimBook::default().left_out(start));
}

fn wait_life_of(head: &QueueHead) -> Option<&str> {
    match of_kind(&head.subject_kind) {
        Of::Interval(started) => wait_life(started, &head.detail["interval"]),
        _ => None,
    }
}
