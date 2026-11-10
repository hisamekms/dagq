use std::collections::HashMap;
use std::convert::Infallible;

use serde_json::json;

use super::*;
use crate::domain::kpi::{KpiConfig, KpiInput, KpiQuery, kpi};
use crate::domain::ledger::testkit::omitted::EdgeStore;
use crate::domain::ledger::testkit::{Log, T0, ms, run_id};
use crate::domain::ledger::window::{WindowQuery, select, select_with};
use crate::domain::ledger::{LedgerKind, LedgerRow, build};
use crate::domain::marks::utc_text;
use crate::domain::stats::{self, LiveSnapshot, SlotSnapshot, StatsQuery};

const KIND: &str = "authorization_denied";

/// A point every this many events of a kind.
const EVERY: u64 = 4;

fn ok<T>(result: Result<T, Infallible>) -> T {
    match result {
        Ok(value) => value,
        Err(never) => match never {},
    }
}

const NOW_MS: i64 = (T0 + 100_000) * 1000;

/// The ledger of `events`, whole, and without the queue rows of the
/// omitted events of (0, `through`]: those of [`KIND`] the queue owns. Run
/// 9's row is none of either: the store holds none of a run whose life it
/// has not seen end.
fn ledgers(events: &[RunEvent], through: EventId) -> (Vec<LedgerRow>, Vec<LedgerRow>) {
    let mut whole = build(events, NOW_MS, "built");
    whole.retain(|row| !(row.kind == LedgerKind::Run && row.key == run_id(9)));
    let left = whole
        .iter()
        .filter(|row| {
            row.kind != LedgerKind::Queue
                || row
                    .subject
                    .as_ref()
                    .is_none_or(|subject| subject.kind.as_str() != KIND)
                || row.records.iter().any(|event| event.id > through)
        })
        .cloned()
        .collect();
    (whole, left)
}

/// `stats --full` of `window` for `query`: its `next_cursor` and
/// `window_ms`, and the whole output.
fn stats_of(window: &LedgerWindow, query: &StatsQuery) -> (i64, (i64, i64), serde_json::Value) {
    let stats = stats::stats_answering(
        window.answers(),
        window.events(),
        &HashMap::new(),
        NOW_MS / 1000,
        SlotSnapshot::default(),
        query,
        &LiveSnapshot::default(),
    );
    (
        stats.next_cursor.as_i64(),
        stats.window_ms,
        serde_json::to_value(&stats).unwrap(),
    )
}

/// An event id `--until` ends at the last omitted event at or before it:
/// C 0, S 6, the queue's refusals 3 (B), 4 (B + 10) and 6 (B + 30), and 5
/// the token of run 9 of no row (claimed, its task canceled, then the
/// token). `--full --until 5` is `next_cursor` 4 and `window_ms` [B, B +
/// 10] of every row, and so of the rows without the omitted events'. So it
/// is at a refusal of no time (the last of the window is that one), at an
/// event of a row (the last omitted before it still counts) and past S
/// (L).
#[test]
fn an_until_event_ends_at_the_last_omitted_event_at_or_before_it() {
    let b = 1000;
    let mut log = Log::default();
    log.add(
        Some(9),
        Some(&run_id(9)),
        "run_claimed",
        json!({"from": "ready", "to": "in_progress", "provider": "claude"}),
        b,
    )
    .add(
        Some(9),
        None,
        "task_status_changed",
        json!({"from": "in_progress", "to": "canceled"}),
        b,
    )
    .add(None, None, KIND, json!({}), b)
    .add(None, None, KIND, json!({}), b + 10)
    .add(
        None,
        Some(&run_id(9)),
        "telemetry_token_issued",
        json!({}),
        b + 20,
    )
    .add(None, None, KIND, json!({}), b + 30);
    let token = EventId::new(5);
    let s = EventId::new(6);
    let (whole, left) = ledgers(&log.events, s);
    assert!(
        whole
            .iter()
            .all(|row| row.records.iter().all(|event| event.id != token)),
        "the token of a run of no row"
    );
    let store = EdgeStore::of(log.events.clone(), &[KIND], (EventId::new(0), s), EVERY);
    let edges = ok(OmittedEdges::of(&store));
    let query = |until: i64| {
        (
            WindowQuery {
                until: Some(Cursor::Event(EventId::new(until))),
                now_ms: NOW_MS,
                ..WindowQuery::default()
            },
            StatsQuery {
                until: Some(Cursor::Event(EventId::new(until))),
                full: true,
                ..StatsQuery::default()
            },
        )
    };
    let (window, stats) = query(5);
    let before = stats_of(&select(whole.clone(), &window), &stats);
    assert_eq!((before.0, before.1), (4, (ms(b), ms(b + 10))));
    let after = stats_of(&ok(select_with(left.clone(), &window, &edges)), &stats);
    assert_eq!(after, before);

    // At a refusal of no time, at an event of a row, past S.
    log.add(None, None, KIND, json!({}), b + 40);
    log.events.last_mut().unwrap().created_at = "not a time".into();
    log.add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s1"}),
        b + 50,
    )
    .add(None, None, KIND, json!({}), b + 60)
    .add(None, None, KIND, json!({}), b + 70);
    for s in [8, 9] {
        let s = EventId::new(s);
        let (whole, left) = ledgers(&log.events, s);
        let store = EdgeStore::of(log.events.clone(), &[KIND], (EventId::new(0), s), EVERY);
        let edges = ok(OmittedEdges::of(&store));
        for until in [5, 7, 8, 9, 10, 20] {
            let (window, stats) = query(until);
            let before = stats_of(&select(whole.clone(), &window), &stats);
            let after = stats_of(&ok(select_with(left.clone(), &window, &edges)), &stats);
            assert_eq!(after, before, "S {s:?} --until {until}");
        }
    }
}

/// The answers of `query`'s bounds of `input`, as a read answers them of
/// the events alone, `set` before the rest: a reader's input of a window
/// whose answers are those.
fn answered(
    input: &KpiInput<'_>,
    query: &KpiQuery,
    set: &[(Bound, EventId, Option<i64>)],
) -> Answers {
    let mut answers = Answers::default();
    for (bound, id, ms) in set {
        answers.set(*bound, *id, *ms);
    }
    for _ in 0..ROUNDS {
        let bounds = crate::domain::kpi::bounds(
            &KpiInput {
                answers: Some(&answers),
                ..*input
            },
            query,
        );
        let new: Vec<Bound> = bounds
            .into_iter()
            .filter(|bound| !answers.has(*bound))
            .collect();
        for bound in new {
            let events = input.events;
            match bound {
                Bound::LastAt(ms) => answers.set(bound, Cursor::Time(ms).event_id(events), None),
                Bound::TimeThrough(id) => answers.set(
                    bound,
                    id,
                    crate::domain::kpi::cursor_ms(Cursor::Event(id), events),
                ),
                Bound::FirstIn(after, upto) => answers.set(
                    bound,
                    after,
                    events
                        .iter()
                        .filter(|event| event.id > after && event.id <= upto)
                        .filter_map(|event| timestamp_millis(&event.created_at))
                        .min(),
                ),
            }
        }
    }
    answers
}

/// `kpi` takes its periods' bounds and its cursors' times of the window's
/// answers, not of the events: an answer that differs from the events'
/// moves the window it starts and what it counts.
#[test]
fn kpi_takes_its_bounds_of_the_windows_answers() {
    let mut log = Log::default();
    log.landed(1, &run_id(1), 100);
    let events = log.events.clone();
    let config = KpiConfig::default();
    let (goals, changes, heartbeats, origins) = (
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
    );
    let input = KpiInput {
        events: &events,
        goals: &goals,
        changes: &changes,
        areas: None,
        heartbeats: &heartbeats,
        draft_origins: &origins,
        now: T0 + 5000,
        utc_offset_secs: 0,
        cores: None,
        rust_toolchain: false,
        config: &config,
        host: None,
        ledger: None,
        rows: &[],
        post_landing: None,
        executions: None,
        earlier: None,
        answers: None,
    };
    let since = EventId::new(2);
    let query = KpiQuery {
        since: Some(Cursor::Event(since)),
        ..KpiQuery::default()
    };
    let runs = |kpi: &crate::domain::kpi::Kpi| kpi.periods[0].window.runs;
    let of_events = kpi(&input, &query).unwrap();
    assert_eq!(of_events.periods[0].start, utc_text(ms(100)));
    assert_eq!(runs(&of_events), 1);
    // The same, answered as the events answer.
    let answers = answered(&input, &query, &[]);
    let same = kpi(
        &KpiInput {
            answers: Some(&answers),
            ..input
        },
        &query,
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&same).unwrap(),
        serde_json::to_value(&of_events).unwrap()
    );
    // The cursor's time is the window's.
    let answers = answered(
        &input,
        &query,
        &[(Bound::TimeThrough(since), since, Some(ms(50)))],
    );
    let moved = kpi(
        &KpiInput {
            answers: Some(&answers),
            ..input
        },
        &query,
    )
    .unwrap();
    assert_eq!(moved.periods[0].start, utc_text(ms(50)));
    // The window's last event at its end is the window's: before the run.
    let end = (T0 + 5000) * 1000;
    let answers = answered(
        &input,
        &query,
        &[(Bound::LastAt(end), EventId::new(1), None)],
    );
    let cut = kpi(
        &KpiInput {
            answers: Some(&answers),
            ..input
        },
        &query,
    )
    .unwrap();
    assert_eq!(runs(&cut), 0);
}

/// `stats` without `--full` and `--since` takes the first time of its page
/// of the window's answer for the page's ids, not of the events.
#[test]
fn stats_takes_its_pages_first_time_of_the_windows_answer() {
    let mut log = Log::default();
    log.landed(1, &run_id(1), 100).landed(2, &run_id(2), 2000);
    let events = log.events;
    let query = StatsQuery::default();
    let (after, upto) = stats::page_range(&events, &HashMap::new(), &query).unwrap();
    assert_eq!(upto, events.last().unwrap().id);
    let of = |answers: Option<&Answers>| {
        stats::stats_answering(
            answers,
            &events,
            &HashMap::new(),
            T0 + 5000,
            SlotSnapshot::default(),
            &query,
            &LiveSnapshot::default(),
        )
        .window_ms
    };
    let of_events = of(None);
    assert_eq!(of_events.0, ms(100));
    let mut answers = Answers::default();
    answers.set(Bound::FirstIn(after, upto), after, Some(ms(50)));
    assert_eq!(of(Some(&answers)).0, ms(50));
    answers.set(Bound::FirstIn(after, upto), after, Some(ms(100)));
    assert_eq!(of(Some(&answers)), of_events);
    // `--full` and `--since` take no answer: the page is the window's.
    for query in [
        StatsQuery {
            full: true,
            ..StatsQuery::default()
        },
        StatsQuery {
            since: Some(Cursor::Time(ms(0))),
            ..StatsQuery::default()
        },
    ] {
        assert_eq!(stats::page_range(&events, &HashMap::new(), &query), None);
    }
}

/// The page's ids [`stats::page_range`] gives are those `stats` takes its
/// first time of: answered of the events, the same output.
#[test]
fn the_pages_range_is_that_of_stats() {
    let mut log = Log::default();
    for n in 0..25u8 {
        log.landed(i64::from(n) + 1, &run_id(n + 1), i64::from(n) * 100);
    }
    let events = log.events;
    for query in [
        StatsQuery::default(),
        StatsQuery {
            until: Some(Cursor::Event(EventId::new(200))),
            ..StatsQuery::default()
        },
    ] {
        let (after, upto) = stats::page_range(&events, &HashMap::new(), &query).unwrap();
        let first = events
            .iter()
            .filter(|event| event.id > after && event.id <= upto)
            .filter_map(|event| timestamp_millis(&event.created_at))
            .min();
        let mut answers = Answers::default();
        answers.set(Bound::FirstIn(after, upto), after, first);
        let of = |answers: Option<&Answers>| {
            serde_json::to_value(stats::stats_answering(
                answers,
                &events,
                &HashMap::new(),
                T0 + 50_000,
                SlotSnapshot::default(),
                &query,
                &LiveSnapshot::default(),
            ))
            .unwrap()
        };
        assert_eq!(of(Some(&answers)), of(None), "{query:?}");
    }
}

/// A declaration's bounds are answered in rounds, each of the answers
/// before it, and a window of none declared holds no answers.
#[test]
fn a_declaration_is_asked_again_with_its_answers() {
    struct Chain;
    impl DeclareBounds for Chain {
        fn bounds(&self, window: &LedgerWindow) -> Vec<Bound> {
            let answers = window.answers().unwrap();
            match answers.time_through(EventId::new(3)) {
                None => vec![Bound::TimeThrough(EventId::new(3))],
                Some(time) => vec![
                    Bound::TimeThrough(EventId::new(3)),
                    Bound::LastAt(time.unwrap()),
                ],
            }
        }
    }
    let mut log = Log::default();
    log.add(None, None, KIND, json!({}), 30)
        .add(None, None, KIND, json!({}), 10)
        .add(None, None, KIND, json!({}), 20)
        .add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "s1"}),
            40,
        );
    let s = EventId::new(3);
    let (whole, left) = ledgers(&log.events, s);
    let store = EdgeStore::of(log.events.clone(), &[KIND], (EventId::new(0), s), EVERY);
    let edges = ok(OmittedEdges::of(&store));
    let query = WindowQuery {
        now_ms: NOW_MS,
        bounds: Declared::of(Chain),
        ..WindowQuery::default()
    };
    let every_row = select(whole, &query);
    let without = ok(select_with(left, &query, &edges));
    let answers = without.answers().unwrap();
    assert_eq!(answers, every_row.answers().unwrap());
    assert_eq!(answers.time_through(s), Some(Some(ms(30))));
    // The last at or before 30 is 3 (20), past 1 (30).
    assert_eq!(answers.last_at(ms(30)), Some(EventId::new(3)));
    assert_eq!(
        answers.bounds(),
        vec![Bound::LastAt(ms(30)), Bound::TimeThrough(s)]
    );
    let none = select(
        build(&log.events, NOW_MS, "built"),
        &WindowQuery {
            now_ms: NOW_MS,
            ..WindowQuery::default()
        },
    );
    assert!(none.answers().is_none());
}

/// The latest time through an id of a window whose end is before an
/// omitted event's time (a clock ahead of now) is the latest of those at
/// or before the end, as of every row: not the rows' own alone.
#[test]
fn an_omitted_time_past_the_end_leaves_the_latest_before_it() {
    struct Through(EventId);
    impl DeclareBounds for Through {
        fn bounds(&self, _: &LedgerWindow) -> Vec<Bound> {
            vec![Bound::TimeThrough(self.0), Bound::LastAt(ms(500))]
        }
    }
    let mut log = Log::default();
    log.add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s1"}),
        10,
    )
    .add(None, None, KIND, json!({}), 2000)
    .add(None, None, KIND, json!({}), 400)
    .add(None, None, KIND, json!({}), 450)
    .add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s1"}),
        600,
    );
    let s = EventId::new(4);
    let (whole, left) = ledgers(&log.events, s);
    let store = EdgeStore::of(log.events.clone(), &[KIND], (EventId::new(0), s), EVERY);
    let edges = ok(OmittedEdges::of(&store));
    for through in 1..=6 {
        let query = WindowQuery {
            now_ms: ms(1000),
            bounds: Declared::of(Through(EventId::new(through))),
            ..WindowQuery::default()
        };
        let every_row = select(whole.clone(), &query);
        let without = ok(select_with(left.clone(), &query, &edges));
        assert_eq!(without.answers(), every_row.answers(), "{through}");
    }
    let query = WindowQuery {
        now_ms: ms(1000),
        bounds: Declared::of(Through(EventId::new(4))),
        ..WindowQuery::default()
    };
    let answers = ok(select_with(left, &query, &edges));
    assert_eq!(
        answers.answers().unwrap().time_through(EventId::new(4)),
        Some(Some(ms(450)))
    );
}
