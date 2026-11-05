use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::super::{At, QueueBook, QueueHead};
use super::*;
use crate::domain::EventKind;
use crate::domain::ledger::{
    LedgerKind, LedgerRow,
    owner::{Owner, owner},
    testkit::{built_and_settled, cut_log, ms, queue_across, two_late_log, waits_across},
};
use crate::domain::pre_claim::PreClaimBook;
use crate::domain::pre_claim::rows::{Since, books, holds, holds_at, living};
use crate::domain::supervisor_life::SILENT_AFTER_MS;

const BUILT: &str = "2030-01-01T00:00:00.000Z";

/// The histories cut, each with the times its rows are folded at: soon
/// after its last event and past every silence.
fn histories() -> Vec<(&'static str, Vec<RunEvent>, Vec<i64>)> {
    let last = |events: &[RunEvent]| timestamp_millis(&events.last().unwrap().created_at).unwrap();
    let times =
        |events: &[RunEvent]| vec![last(events) + 1_000, last(events) + 10 * SILENT_AFTER_MS];
    let mut out = Vec::new();
    for (name, events) in [
        ("cut", cut_log(1).events),
        ("queue_across", queue_across().0.events),
        ("waits_across", waits_across().0.events),
        ("two_late", two_late_log().events),
    ] {
        let nows = times(&events);
        out.push((name, events, nows));
    }
    out
}

fn upto(events: &[RunEvent], at: EventId) -> Vec<RunEvent> {
    events
        .iter()
        .filter(|event| event.id <= at)
        .cloned()
        .collect()
}

fn queue_of(rows: &[LedgerRow]) -> Vec<LedgerRow> {
    rows.iter()
        .filter(|row| row.kind == LedgerKind::Queue)
        .cloned()
        .collect()
}

fn heads_of(rows: &[LedgerRow]) -> Vec<QueueHead> {
    queue_of(rows).iter().map(QueueHead::of).collect()
}

/// The stops the queue rows record (`LedgerStore::ledger_supervisor_stops`).
fn stops_of(rows: &[LedgerRow]) -> Vec<RunEvent> {
    let mut stops: Vec<RunEvent> = queue_of(rows)
        .into_iter()
        .flat_map(|row| row.records)
        .filter(|event| event.kind == EventKind::SupervisorStopped)
        .collect();
    stops.sort_by_key(|event| event.id);
    stops
}

/// What [`Needs`] names, read of `rows` (the store's) and `events` as the
/// store gives them, and the last event naming each supervisor by `at`.
fn read(
    needs: &Needs,
    rows: &[LedgerRow],
    events: &[RunEvent],
    at: EventId,
) -> (Reads, Vec<RunEvent>) {
    let mut reads = Reads::default();
    for key in &needs.rows {
        let row = rows
            .iter()
            .find(|row| row.kind == LedgerKind::Queue && &row.key == key)
            .unwrap();
        reads.records.insert(key.clone(), row.records.clone());
    }
    for run in &needs.runs {
        let of_run: Vec<RunEvent> = events
            .iter()
            .filter(|event| event.run_id.as_ref().map(RunId::as_str) == Some(run.as_str()))
            .cloned()
            .collect();
        reads.runs.insert(run.clone(), of_run);
    }
    let last = needs
        .supervisors
        .iter()
        .filter_map(|supervisor| {
            events
                .iter()
                .rev()
                .find(|event| event.id <= at && supervisor_of(event) == Some(supervisor.as_str()))
                .cloned()
        })
        .collect();
    (reads, last)
}

/// [`Since`] for `at` of the task rows `rows` written later, as
/// `LedgerStore::ledger_tasks_open_since_at` gives it.
fn since_at(rows: &[LedgerRow], at: EventId) -> Since {
    let cut: Vec<LedgerRow> = rows
        .iter()
        .filter(|row| row.kind == LedgerKind::Task && row.inputs_from <= at)
        .map(|row| LedgerRow {
            is_final: row.is_final && row.inputs_through.last() <= at,
            ..row.clone()
        })
        .collect();
    Since::of(&cut)
}

fn pre_claim_of(events: &[RunEvent]) -> PreClaimBook {
    let mut book = PreClaimBook::default();
    for event in events {
        book.add(event);
    }
    book
}

/// The rows the store holds written at the last of `events`, cut to `at`:
/// the lives and the waits made again there ([`books`], of the rows
/// [`holds_at`] keeps without their records, held from the position's
/// [`Since`]), and the queue book ([`QueueBook::from_heads_at`]) of the
/// waits made again and the lives at `at`.
struct MadeAt {
    lives: Lives,
    pre_claim: PreClaimBook,
    queue: QueueBook,
    since: Since,
}

fn made_at(rows: &[LedgerRow], events: &[RunEvent], at: EventId, now: i64) -> MadeAt {
    let heads = heads_of(rows);
    let needs = Needs::of(&heads, at);
    let (reads, last) = read(&needs, rows, events, at);
    let stops = stops_of(rows);
    let since = since_at(rows, at);
    let held: Vec<LedgerRow> = queue_of(rows)
        .into_iter()
        .filter(|row| holds_at(row, since, at))
        .map(|row| LedgerRow {
            records: Vec::new(),
            ..row
        })
        .collect();
    let (lives, mut pre_claim) = books(&held, &stops, &last, now, at);
    pre_claim.held_from(since);
    let cut_lives = lives_at(&last, &stops, at);
    let queue = QueueBook::from_heads_at(
        &heads,
        &pre_claim,
        &Cut {
            at,
            now_ms: now,
            lives: &cut_lives,
            reads: &reads,
        },
    );
    MadeAt {
        lives,
        pre_claim,
        queue,
        since,
    }
}

/// `rows` with the task rows settled when `settled`: a store whose task
/// rows all settled, where a task row read past a position starts that
/// position's [`Since`] earlier than the exact one.
fn tasks_settled(mut rows: Vec<LedgerRow>, settled: bool) -> Vec<LedgerRow> {
    for row in &mut rows {
        if row.kind == LedgerKind::Task && settled {
            row.is_final = true;
        }
    }
    rows
}

/// The waits `made` holds that the books of the rows written at `at`
/// (`at_rows`, of the events `before`, held from their exact [`Since`])
/// do not: each ended before that exact [`Since`].
fn extra_waits(
    made: &MadeAt,
    at_rows: &[LedgerRow],
    before: &[RunEvent],
    now: i64,
    at: EventId,
) -> Vec<EventId> {
    let exact = Since::of(at_rows);
    let held: Vec<LedgerRow> = queue_of(at_rows)
        .into_iter()
        .filter(|row| holds(row, exact))
        .collect();
    let stops = stops_of(at_rows);
    let last: Vec<RunEvent> = living(&held, &stops)
        .iter()
        .filter_map(|id| before.iter().find(|event| event.id == *id).cloned())
        .collect();
    let (_, book) = books(&held, &stops, &last, now, at);
    made.pre_claim
        .intervals(&made.lives, now)
        .filter(|wait| !book.holds(wait.id))
        .map(|wait| {
            let ended = wait.ended_at.as_deref().and_then(timestamp_millis);
            assert!(ended.is_some_and(|end| end < exact.ms), "{at}: {}", wait.id);
            wait.id
        })
        .collect()
}

/// The supervisors the rows `rows` name: their subjects' and their
/// waits'.
fn named(rows: &[LedgerRow]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for row in rows {
        if let Some(subject) = &row.subject {
            names.insert(subject.id.clone());
        }
        if let Some(supervisor) = row.detail["interval"]["supervisor"].as_str() {
            names.insert(supervisor.to_owned());
        }
    }
    names
}

/// The queue book made again from the rows the store holds written at
/// `P` (the last event), cut to each position `at` of the history, is the
/// book made again from the rows written at `at`, but for the through of
/// a landing slot's attempt ended by then, which is `P`'s and may be past
/// `at`; the lives and the waits made again there hold every wait the
/// books of the rows written at `at` (held from their exact [`Since`])
/// hold, as they hold it, and besides only waits ended before that
/// [`Since`]; and the life of each supervisor those rows name that has
/// one there ends, ends from and reads up to as there, before and after
/// its silence, whether the task rows settled or not. The histories hold findings, alerts, streams, waits and
/// attempts that open and end after positions, supervisors stopped and
/// gone silent, and waits and lives open across a claim.
#[test]
fn the_books_of_the_rows_cut_to_a_position_are_those_of_the_rows_written_there() {
    let mut overshot = BTreeMap::<&str, usize>::new();
    let mut extra = 0;
    for (name, events, nows) in histories() {
        // The task rows as the fold leaves them (none settled), and as a
        // store whose task rows all settled holds them: a task row read
        // past the position then starts the position's Since earlier than
        // the exact one.
        for (now, settled) in nows.iter().flat_map(|now| [(*now, false), (*now, true)]) {
            let settle = |rows: Vec<LedgerRow>| tasks_settled(rows, settled);
            let rows = settle(built_and_settled(&events, now));
            for at in events.iter().map(|event| event.id) {
                let before = upto(&events, at);
                let at_rows = settle(built_and_settled(&before, now));
                let made = made_at(&rows, &events, at, now);

                // (a) the queue book, but for the throughs of the ended
                // attempts.
                let fold = pre_claim_of(&before);
                let heads = heads_of(&rows);
                let (reads, last) = read(&Needs::of(&heads, at), &rows, &events, at);
                let cut_lives = lives_at(&last, &stops_of(&rows), at);
                let mut queue = QueueBook::from_heads_at(
                    &heads,
                    &fold,
                    &Cut {
                        at,
                        now_ms: now,
                        lives: &cut_lives,
                        reads: &reads,
                    },
                );
                let there = QueueBook::from_heads(&heads_of(&at_rows), &fold, at);
                // An open attempt reads up to the position, not its
                // through.
                let ended = (0..there.slots.len())
                    .filter(|index| !there.attempts.get(*index, now).unwrap().open);
                for index in ended {
                    let (got, want) = (queue.throughs.of(index), there.throughs.of(index));
                    if got != want {
                        assert!(got > want && got > Some(at), "{name} {now} {at}: {index}");
                        *overshot.entry(name).or_default() += 1;
                    }
                }
                queue.throughs = there.throughs.clone();
                // A cut row is settled no more, so the next close may fold
                // again a stream the rows written there hold settled and
                // ended: its row comes out the same.
                assert!(
                    queue.stream_named.is_superset(&there.stream_named),
                    "{name} {now} {at}"
                );
                queue.stream_named.clone_from(&there.stream_named);
                assert_eq!(queue, there, "{name} {now} {at}");

                // (b) the waits.
                let exact = Since::of(&at_rows);
                let held: Vec<LedgerRow> = queue_of(&at_rows)
                    .into_iter()
                    .filter(|row| holds(row, exact))
                    .collect();
                let stops: Vec<RunEvent> = stops_of(&at_rows);
                let last: Vec<RunEvent> = living(&held, &stops)
                    .iter()
                    .filter_map(|id| before.iter().find(|event| event.id == *id).cloned())
                    .collect();
                let (lives, book) = books(&held, &stops, &last, now, at);
                let waits: BTreeMap<EventId, PreClaimInterval> = made
                    .pre_claim
                    .intervals(&made.lives, now)
                    .map(|wait| (wait.id, wait.into_owned()))
                    .collect();
                for wait in book.intervals(&lives, now) {
                    assert_eq!(waits.get(&wait.id), Some(&*wait), "{name} {now} {at}");
                }
                for (id, wait) in &waits {
                    if !book.holds(*id) {
                        let ended = wait.ended_at.as_deref().and_then(timestamp_millis);
                        assert!(
                            ended.is_some_and(|end| end < exact.ms),
                            "{name} {now} {at}: {id}"
                        );
                        extra += 1;
                    }
                }
                assert!(made.since.ms <= exact.ms && made.since.id <= exact.id);

                // (c) the lives.
                for supervisor in named(&at_rows) {
                    if lives.last_of(&supervisor).is_none() {
                        continue;
                    }
                    assert_eq!(
                        made.lives.last_of(&supervisor),
                        lives.last_of(&supervisor),
                        "{name} {now} {at}: {supervisor}"
                    );
                    assert_eq!(
                        made.lives.ends_from(&supervisor),
                        lives.ends_from(&supervisor),
                        "{name} {now} {at}: {supervisor}"
                    );
                    for time in [now, now + SILENT_AFTER_MS, now + 10 * SILENT_AFTER_MS] {
                        assert_eq!(
                            made.lives.end(&supervisor, time),
                            lives.end(&supervisor, time),
                            "{name} {now} {at}: {supervisor} {time}"
                        );
                    }
                }
            }
        }
    }
    // The cases the exceptions are for came up.
    assert!(
        overshot.get("two_late").is_some_and(|n| *n > 0),
        "{overshot:?}"
    );
    assert!(overshot.get("cut").is_some_and(|n| *n > 0), "{overshot:?}");
    assert!(extra > 0);
}

/// The rows the book made again at a position (the queue rows cut there
/// and the lives and the waits made of the rows cut, with the waits ended
/// before the [`Since`] besides) writes once it read the events after the
/// position, are the rows of the fold of every event, whole: the events
/// between the position and the rows' are read once. A row it forgot is
/// one the store holds settled as that fold's. The book made again of
/// the rows as written, not cut, at a position before theirs writes
/// other rows. A landing slot's attempt that two events after its end
/// timed before it reach is among them, and so are books that hold waits
/// ended before the exact [`Since`] (a store whose task rows settled).
#[test]
fn the_books_of_the_rows_cut_go_on_from_the_position_as_the_fold_of_every_event() {
    let (mut uncut_differs, mut replayed_with_extra) = (0, 0);
    for (name, events, nows) in histories() {
        let owned: Vec<RunEvent> = events
            .iter()
            .filter(|event| owner(event) == Owner::Queue)
            .cloned()
            .collect();
        // Whether the task rows settled or not, as in the test above: the
        // books made again then hold waits ended before the exact Since.
        for (now, settled) in nows.iter().flat_map(|now| [(*now, false), (*now, true)]) {
            let rows = tasks_settled(built_and_settled(&events, now), settled);
            let whole: BTreeMap<String, LedgerRow> =
                queue_of(&crate::domain::ledger::build(&events, now, BUILT))
                    .into_iter()
                    .map(|row| (row.key.clone(), row))
                    .collect();
            let p = events.last().unwrap().id;
            for at in events.iter().map(|event| event.id) {
                let made = made_at(&rows, &events, at, now);
                let before = upto(&events, at);
                let at_rows = tasks_settled(built_and_settled(&before, now), settled);
                let extra = extra_waits(&made, &at_rows, &before, now, at);
                let uncut = QueueBook::from_heads(&heads_of(&rows), &made.pre_claim, at);
                let go_on = |mut queue: QueueBook| {
                    let (mut lives, mut pre_claim) = (made.lives.clone(), made.pre_claim.clone());
                    for event in events.iter().filter(|event| event.id > at) {
                        lives.add(event);
                        let moved = pre_claim.add(event);
                        queue.add(
                            event,
                            owner(event) == Owner::Queue,
                            moved,
                            &mut BTreeSet::new(),
                        );
                    }
                    let fold = At {
                        owned: &owned,
                        pre_claim: &pre_claim,
                        lives: &lives,
                        now_ms: now,
                        built_at: BUILT,
                    };
                    queue
                        .keys()
                        .filter_map(|key| Some((key.clone(), queue.row(&key, &fold)?)))
                        .collect::<BTreeMap<String, LedgerRow>>()
                };
                let written = go_on(made.queue.clone());
                for (key, row) in &written {
                    assert_eq!(Some(row), whole.get(key), "{name} {now} {at}: {key}");
                }
                for (key, row) in &whole {
                    if !written.contains_key(key) {
                        let held = rows
                            .iter()
                            .find(|held| held.kind == LedgerKind::Queue && &held.key == key)
                            .unwrap();
                        assert!(held.is_final, "{name} {now} {at}: {key}");
                        assert!(held.inputs_through.last() <= at);
                        assert_eq!(
                            &LedgerRow {
                                is_final: false,
                                ..held.clone()
                            },
                            row
                        );
                    }
                }
                // The books with the extra waits went on as the fold too.
                if !extra.is_empty() {
                    replayed_with_extra += 1;
                }
                if at < p && go_on(uncut) != written {
                    uncut_differs += 1;
                }
            }
        }
    }
    assert!(uncut_differs > 0);
    assert!(replayed_with_extra > 0);
}

/// A cut of nothing past the position is the rows as written, and reads
/// nothing; one of a row past it names only what that row needs.
#[test]
fn a_cut_at_the_position_of_the_rows_is_the_rows_as_written() {
    for (_, events, nows) in histories() {
        for now in nows {
            let rows = built_and_settled(&events, now);
            let heads = heads_of(&rows);
            let p = events.last().unwrap().id;
            let needs = Needs::of(&heads, p);
            assert!(needs.rows.is_empty() && needs.runs.is_empty());
            let reads = Reads::default();
            let lives = Lives::default();
            let cut = Cut {
                at: p,
                now_ms: now,
                lives: &lives,
                reads: &reads,
            };
            assert_eq!(cut.heads(&heads), heads);
            let pre_claim = pre_claim_of(&events);
            assert_eq!(
                QueueBook::from_heads_at(&heads, &pre_claim, &cut),
                QueueBook::from_heads(&heads, &pre_claim, p)
            );
        }
    }
}

/// The values a stream's or a finding's row is cut to come of its records
/// read with their bodies; without them the cut keeps the values written.
#[test]
fn a_stream_and_a_finding_cut_read_their_records_bodies() {
    let events = queue_across().0.events;
    let now = ms(40);
    let rows = built_and_settled(&events, now);
    let heads = heads_of(&rows);
    // The stream from 2 records 13 (version 2) and 15 (version 3); the
    // finding 4 is resolved at 17.
    let at = EventId::new(14);
    let needs = Needs::of(&heads, at);
    assert!(needs.rows.contains(&"2".to_owned()) && needs.rows.contains(&"4".to_owned()));
    let (reads, last) = read(&needs, &rows, &events, at);
    let lives = lives_at(&last, &stops_of(&rows), at);
    let head = |cut: &[QueueHead], key: &str| {
        cut.iter()
            .find(|head| head.head.key == key)
            .cloned()
            .unwrap()
    };
    let cut = Cut {
        at,
        now_ms: now,
        lives: &lives,
        reads: &reads,
    };
    let read_cut = cut.heads(&heads);
    assert_eq!(head(&read_cut, "2").detail["version"], Value::from(2));
    assert_eq!(head(&read_cut, "4").outcome.as_deref(), Some("open"));
    let unread = Cut {
        reads: &Reads::default(),
        ..cut
    }
    .heads(&heads);
    assert_eq!(head(&unread, "2").detail["version"], Value::from(3));
    assert_eq!(head(&unread, "4").outcome.as_deref(), Some("resolved"));
}

/// Leaving out the heads of [`QueueHead::forgotten_at`] a position and its
/// [`Since`] change nothing made there, for each kind left out: the reads a
/// cut names, the rows the books of the waits and the lives are made of, and
/// the queue book, at every position of every history, folded soon after it
/// and past every silence, whether the task rows settled or not (all
/// settled, the waits before the position are left out).
#[test]
fn the_heads_forgotten_at_a_position_change_nothing_made_there() {
    let mut cases = histories();
    let events = crate::domain::ledger::testkit::forgotten_log(3).events;
    let last = timestamp_millis(&events.last().unwrap().created_at).unwrap();
    cases.push((
        "forgotten",
        events,
        vec![last + 1_000, last + 10 * SILENT_AFTER_MS],
    ));
    let events = crate::domain::ledger::testkit::settled_waits_log(2).events;
    let last = timestamp_millis(&events.last().unwrap().created_at).unwrap();
    cases.push((
        "settled waits",
        events,
        vec![last + 1_000, last + 10 * SILENT_AFTER_MS],
    ));
    let mut left_out: BTreeMap<String, usize> = BTreeMap::new();
    for (name, events, nows) in cases {
        for (now, settled) in nows.iter().flat_map(|now| [(*now, false), (*now, true)]) {
            let rows = tasks_settled(built_and_settled(&events, now), settled);
            let heads = heads_of(&rows);
            for at in events.iter().map(|event| event.id) {
                let since = since_at(&rows, at);
                let (kept, gone): (Vec<QueueHead>, Vec<QueueHead>) = heads
                    .iter()
                    .cloned()
                    .partition(|head| !head.forgotten_at(at, since));
                for head in &gone {
                    let kind = if head.subject_kind.starts_with("live_alert")
                        || is_wait(&head.subject_kind)
                    {
                        head.subject_kind.as_str()
                    } else {
                        "one event"
                    };
                    *left_out.entry(kind.to_owned()).or_default() += 1;
                }
                let needs = Needs::of(&heads, at);
                assert_eq!(Needs::of(&kept, at), needs, "{name} {at}");
                let earliest = Since {
                    ms: i64::MIN,
                    id: EventId::new(0),
                };
                for head in &gone {
                    // Of the waits, those not made for this position's
                    // Since; of the rest, none whatever the Since.
                    let of = if is_wait(&head.subject_kind) {
                        since
                    } else {
                        earliest
                    };
                    assert!(!holds_at(&head.row(), of, at), "{name} {at}");
                }
                let made = made_at(&rows, &events, at, now);
                let (reads, last) = read(&needs, &rows, &events, at);
                let lives = lives_at(&last, &stops_of(&rows), at);
                let cut = Cut {
                    at,
                    now_ms: now,
                    lives: &lives,
                    reads: &reads,
                };
                assert_eq!(
                    QueueBook::from_heads_at(&kept, &made.pre_claim, &cut),
                    made.queue,
                    "{name} {at}"
                );
            }
        }
    }
    assert_eq!(
        left_out.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "claim_deferral",
            "claim_hold",
            "landing_hold",
            "live_alert",
            "live_alert_stream",
            "one event",
            "slots_full"
        ]
    );
}
