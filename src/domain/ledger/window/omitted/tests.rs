use std::convert::Infallible;

use serde_json::json;

use super::*;
use crate::domain::ledger::testkit::omitted::EdgeStore as Store;
use crate::domain::ledger::testkit::{Log, T0, ms, run_id};
use crate::domain::ledger::window::{
    LedgerWindow, WindowQuery, WindowRows, select, select_from_time, select_from_time_with,
    select_with,
};
use crate::domain::ledger::{LedgerKind, LedgerRow, build};
use crate::domain::stats::Cursor;

const KIND: &str = "authorization_denied";

/// A point every this many events of a kind.
const EVERY: u64 = 4;

fn ok<T>(result: Result<T, Infallible>) -> T {
    match result {
        Ok(value) => value,
        Err(never) => match never {},
    }
}

/// What the edges answer, counted straight from the omitted events.
fn oracle_at_or_before(store: &Store, at: i64) -> Option<EventId> {
    store
        .omitted()
        .iter()
        .filter(|event| omitted_ms(event).is_some_and(|time| time <= at))
        .map(|event| event.id)
        .max()
}

fn oracle_first_ms(store: &Store, after: EventId, upto: EventId) -> Option<i64> {
    store
        .omitted()
        .iter()
        .filter(|event| event.id > after && event.id <= upto)
        .filter_map(|event| omitted_ms(event))
        .min()
}

/// How a history's units are made, each of its events at `secs`.
#[derive(Debug, Clone, Copy)]
enum Shape {
    /// One of the queue's and one of another kind, the clock rising.
    Rising,
    /// Two of the queue's, the second five seconds before the first.
    Back,
    /// One of the queue's and thirty of the kind of a run's.
    Sparse,
    /// One of the queue's timed and twenty of no time.
    Timeless,
}

impl Shape {
    const ALL: [Self; 4] = [Self::Rising, Self::Back, Self::Sparse, Self::Timeless];

    /// The events of a unit at `secs`, timed (`timeless` none of no time).
    fn unit(self, log: &mut Log, secs: i64, timeless: bool) {
        let refusal = json!({"capability": "telemetry.report"});
        match self {
            Self::Rising => {
                log.add(None, None, KIND, refusal, secs).add(
                    None,
                    None,
                    "supervisor_alive",
                    json!({"supervisor": "s1"}),
                    secs,
                );
            }
            Self::Back => {
                log.add(None, None, KIND, refusal.clone(), secs).add(
                    None,
                    None,
                    KIND,
                    refusal,
                    secs - 5,
                );
            }
            Self::Sparse => {
                log.add(None, None, KIND, refusal.clone(), secs);
                for _ in 0..30 {
                    log.add(None, Some(&run_id(5)), KIND, refusal.clone(), secs);
                }
            }
            Self::Timeless => {
                log.add(None, None, KIND, refusal.clone(), secs);
                if !timeless {
                    for _ in 0..20 {
                        log.add(None, None, KIND, refusal.clone(), secs);
                        log.events.last_mut().unwrap().created_at = "not a time".into();
                    }
                }
            }
        }
    }
}

/// The window's units, fixed whatever the history around it.
const WINDOW: i64 = 10;

/// A history of `shape`: `before` units, the window's ten, `after` units,
/// ten seconds apart; the window's first and last seconds and the last id
/// before it and its last.
fn history(
    shape: Shape,
    before: i64,
    after: i64,
) -> (Vec<RunEvent>, (i64, i64), (EventId, EventId)) {
    let mut log = Log::default();
    for n in 0..before {
        shape.unit(&mut log, n * 10, false);
    }
    let a = log.last();
    for n in before..before + WINDOW {
        shape.unit(&mut log, n * 10, true);
    }
    let b = log.last();
    for n in before + WINDOW..before + WINDOW + after {
        shape.unit(&mut log, n * 10, false);
    }
    (
        log.events,
        (before * 10, (before + WINDOW - 1) * 10),
        (a, b),
    )
}

/// The edges of each shape answer as the omitted events counted straight
/// do, at every time of the history and over ranges of every start: the
/// clock turned back (D), the runs' events of the kind (no answer, but in
/// the points' count), events of no time (L alone), and a C past the first
/// event.
#[test]
fn the_edges_answer_as_the_omitted_events_counted() {
    for shape in Shape::ALL {
        for c in [0, 7] {
            let (events, _, _) = history(shape, 8, 8);
            let last = events.last().unwrap().id;
            let store = Store::of(events, &[KIND], (EventId::new(c), last), EVERY);
            let edges = ok(OmittedEdges::of(&store));
            assert_eq!(
                edges.latest(),
                store.omitted().last().map(|event| event.id),
                "{shape:?}"
            );
            for secs in (-20..300).step_by(3) {
                let at = ms(secs);
                assert_eq!(
                    ok(edges.at_or_before(at)),
                    oracle_at_or_before(&store, at),
                    "{shape:?} c {c} t {secs}"
                );
            }
            for after in (0..last.as_i64()).step_by(5) {
                for upto in [after + 1, after + 9, last.as_i64()] {
                    let range = (EventId::new(after), EventId::new(upto));
                    assert_eq!(
                        ok(edges.first_ms(range.0, range.1)),
                        oracle_first_ms(&store, range.0, range.1),
                        "{shape:?} c {c} ({after}, {upto}]"
                    );
                }
            }
            for id in 1..=last.as_i64() {
                let id = EventId::new(id);
                let omitted = store.omitted().into_iter().find(|event| event.id == id);
                assert_eq!(
                    ok(edges.time_of(id)),
                    omitted.and_then(omitted_ms),
                    "{shape:?} {id:?}"
                );
            }
        }
    }
}

/// The point that ends a span is an event of no time and the one before it
/// is timed at t: t's answer is the timed one, never the one of no time,
/// whether M is t (the summary answers) or past it (the point does). The
/// one of no time is the latest until a later one.
#[test]
fn an_event_of_no_time_ending_a_span_is_no_answer() {
    let mut log = Log::default();
    for n in 0..6 {
        log.add(None, None, KIND, json!({}), n);
    }
    log.add(None, None, KIND, json!({}), 10);
    log.add(None, None, KIND, json!({}), 10);
    log.events.last_mut().unwrap().created_at = "not a time".into();
    let timed = log.events[log.events.len() - 2].id;
    let timeless = log.last();
    for later in [false, true] {
        let mut log = log.clone();
        if later {
            log.add(None, None, KIND, json!({}), 20);
        }
        let last = log.last();
        let store = Store::of(log.events, &[KIND], (EventId::new(0), last), EVERY);
        assert!(store.points.iter().any(|point| point.at == timeless));
        let edges = ok(OmittedEdges::of(&store));
        assert_eq!(edges.latest(), Some(last));
        assert_eq!(ok(edges.at_or_before(ms(10))), Some(timed), "{later}");
        assert_eq!(ok(edges.at_or_before(ms(19))), Some(timed), "{later}");
        assert_eq!(ok(edges.time_of(timeless)), None);
        assert_eq!(ok(edges.first_ms(timed, timeless)), None);
    }
}

/// No coverage: nothing is added and nothing read.
#[test]
fn a_store_of_no_coverage_adds_nothing() {
    let mut store = Store::of(
        history(Shape::Rising, 4, 4).0,
        &[KIND],
        (EventId::new(0), EventId::new(9)),
        EVERY,
    );
    store.coverage = None;
    let edges = ok(OmittedEdges::of(&store));
    assert_eq!(edges.latest(), None);
    assert_eq!(ok(edges.at_or_before(ms(1_000))), None);
    assert_eq!(ok(edges.first_ms(EventId::new(0), EventId::new(99))), None);
    assert_eq!(ok(edges.time_of(EventId::new(3))), None);
    assert_eq!(store.taken(), 0);
}

/// An event past S, at C or before it, of another kind or of a run's is no
/// omitted event: not the latest, no time of a cursor's event.
#[test]
fn only_the_queue_events_of_the_kinds_in_c_to_s_are_added() {
    let mut log = Log::default();
    log.add(None, None, KIND, json!({}), 1)
        .add(None, None, KIND, json!({}), 2)
        .add(None, Some(&run_id(3)), KIND, json!({}), 3)
        .add(None, None, "telemetry_token_issued", json!({}), 4)
        .add(None, None, KIND, json!({}), 5)
        .add(None, None, KIND, json!({}), 6);
    let store = Store::of(
        log.events,
        &[KIND],
        (EventId::new(1), EventId::new(5)),
        EVERY,
    );
    let edges = ok(OmittedEdges::of(&store));
    assert_eq!(edges.latest(), Some(EventId::new(5)));
    for id in [1, 3, 4, 6] {
        assert_eq!(ok(edges.time_of(EventId::new(id))), None, "{id}");
    }
    assert_eq!(ok(edges.time_of(EventId::new(2))), Some(ms(2)));
    assert_eq!(ok(edges.at_or_before(ms(4))), Some(EventId::new(2)));
    assert_eq!(ok(edges.at_or_before(ms(9))), Some(EventId::new(5)));
    assert_eq!(ok(edges.at_or_before(ms(1))), None);
}

/// The ledger of `events` (C 0, S the last) with the queue rows of the
/// omitted events and without them.
fn ledgers(events: &[RunEvent]) -> (Vec<LedgerRow>, Vec<LedgerRow>) {
    let now = (T0 + 100_000) * 1000;
    let whole = build(events, now, "built");
    let left = whole
        .iter()
        .filter(|row| {
            row.kind != LedgerKind::Queue
                || row
                    .subject
                    .as_ref()
                    .is_none_or(|subject| subject.kind.as_str() != KIND)
        })
        .cloned()
        .collect();
    (whole, left)
}

/// A window's edges, as the tests compare them.
fn edges_of(window: &LedgerWindow) -> (EventId, EventId, Option<i64>, i64, i64) {
    (
        window.after,
        window.upto,
        window.start_ms,
        window.end_ms,
        window.host_from_ms(),
    )
}

/// The last of the queue's records of the time `at` or before it a read
/// from it stops the rows left out at, of `rows`' queue rows and `edges`.
fn queue_before(rows: &[LedgerRow], at: i64, edges: &OmittedEdges<'_, Infallible>) -> EventId {
    let query = WindowQuery {
        read_from_ms: Some(at),
        ..WindowQuery::default()
    };
    let mut read = WindowRows::new(&query, 0);
    for row in rows.iter().filter(|row| row.kind == LedgerKind::Queue) {
        read.take(row.clone());
    }
    ok(read.take_omitted_before(edges));
    read.rows_from().unwrap().inputs_through
}

/// The short windows of the test of the reads: by time `--since` and
/// `--until`, and without `--since` up to a time, of `(start, end)`.
fn queries((start, end): (i64, i64)) -> Vec<WindowQuery> {
    let now_ms = (T0 + 100_000) * 1000;
    vec![
        WindowQuery {
            since: Some(Cursor::Time(ms(start))),
            until: Some(Cursor::Time(ms(end))),
            now_ms,
            ..WindowQuery::default()
        },
        WindowQuery {
            until: Some(Cursor::Time(ms(end))),
            now_ms,
            ..WindowQuery::default()
        },
        WindowQuery {
            read_from_ms: Some(ms(start)),
            now_ms,
            ..WindowQuery::default()
        },
    ]
}

/// The events (c) and (d) read for the short windows of each shape are as
/// many whatever the history before and after them (the omitted events, the
/// runs' of their kind, those of no time, the points among them), and the
/// window without the omitted events' rows is that of every row: with no
/// queue row left too (the history is of the omitted kind alone in the
/// shapes but the rising one).
#[test]
fn the_edges_read_as_many_events_whatever_the_history_around_the_window() {
    for shape in Shape::ALL {
        let mut counts = Vec::new();
        for (before, after) in [(8, 8), (80, 160)] {
            let (events, times, (a, b)) = history(shape, before, after);
            let (whole, left) = ledgers(&events);
            if !matches!(shape, Shape::Rising | Shape::Sparse) {
                assert!(
                    left.iter().all(|row| row.kind != LedgerKind::Queue),
                    "{shape:?}"
                );
            }
            let last = events.last().unwrap().id;
            let store = Store::of(events, &[KIND], (EventId::new(0), last), EVERY);
            let edges = ok(OmittedEdges::of(&store));
            let mut read = Vec::new();
            for query in queries(times) {
                let (now, then) = if query.read_from_ms.is_some() {
                    (
                        select_from_time(whole.clone(), &query),
                        ok(select_from_time_with(left.clone(), &query, &edges)),
                    )
                } else {
                    (
                        select(whole.clone(), &query),
                        ok(select_with(left.clone(), &query, &edges)),
                    )
                };
                assert_eq!(edges_of(&now), edges_of(&then), "{shape:?} {query:?}");
                read.push(store.taken());
            }
            let none = OmittedEdges::<Infallible>::none();
            assert_eq!(
                queue_before(&whole, ms(times.0), &none),
                queue_before(&left, ms(times.0), &edges),
                "{shape:?}"
            );
            read.push(store.taken());
            ok(edges.at_or_before(ms(times.0)));
            ok(edges.at_or_before(ms(times.1)));
            read.push(store.taken());
            ok(edges.first_ms(a, b));
            read.push(store.taken());
            counts.push(read);
        }
        assert_eq!(counts[0], counts[1], "{shape:?}");
        assert!(counts[0].iter().any(|read| *read > 0), "{shape:?}");
        // A span of the window is read, not the history.
        assert!(
            counts[0].iter().all(|read| *read <= 16 * EVERY as usize),
            "{shape:?} {counts:?}"
        );
    }
}
