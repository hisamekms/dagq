//! The coverage of the omitted queue events what builds the ledger writes
//! with its marks ([`crate::domain::ledger::session::written::omitted`]):
//! C once, S and the summaries over (C, S] together, never lowered, and
//! held as they are on a store that stops it.

use super::*;
use crate::domain::ledger::session::written::omitted::Omission;
use crate::domain::ledger::testkit::omitted::{counted, covers};

const DENIED: &str = "authorization_denied";
const ISSUED: &str = "telemetry_token_issued";
const EVERY: u64 = 2;

/// The queue's refusals (every third timed 30 seconds back) and job tokens,
/// a task's refusals and a run's tokens, marks and a landed run between.
fn log() -> Log {
    let mut log = Log::default();
    log.landed(1, &run_id(1), 0);
    for n in 0..24_i64 {
        let secs = 100 + 10 * n;
        let back = if n % 3 == 2 { 30 } else { 0 };
        log.add(None, None, DENIED, json!({}), secs - back);
        log.add(Some(n + 2), None, DENIED, json!({}), secs + 1);
        log.add(
            Some(1),
            Some(&run_id(1)),
            ISSUED,
            json!({"target": "run"}),
            secs + 2,
        );
        log.add(None, None, ISSUED, json!({"target": "job:1"}), secs + 3);
        log.add(None, None, "mark_added", json!({"mark": "m"}), secs + 4);
    }
    log
}

fn omission() -> Omission {
    Omission::of([DENIED, ISSUED], EVERY)
}

fn omitting(mut builder: Builder) -> Builder {
    builder.set_omission(omission());
    builder
}

/// `builder` omitting no kind, as the builder before the version's set.
fn of_none(mut builder: Builder) -> Builder {
    builder.set_omission(Omission::of([], EVERY));
    builder
}

fn arrive(memory: &Memory, log: &Log, n: usize) {
    memory.events.replace(log.events[..n].to_vec());
}

fn coverage(memory: &Memory) -> Option<Coverage> {
    memory.omitted.borrow().clone()
}

/// The store's coverage covers (C, S] of its events as counted, its
/// points too, and S is W: written with it.
#[track_caller]
fn covers_c_to_s(memory: &Memory) -> Coverage {
    let coverage = coverage(memory).expect("a coverage");
    let events = memory.events.borrow();
    assert!(covers(&coverage, &events, EVERY));
    let (_, points) = counted(
        &events,
        &coverage.kinds,
        (coverage.from, coverage.through),
        EVERY,
    );
    let held: Vec<Point> = memory.points.borrow().values().cloned().collect();
    assert_eq!(held, points);
    assert_eq!(memory.mark.borrow().unwrap().through, coverage.through);
    coverage
}

/// A builder of no kind writes none; one of some kind writes C at where its
/// first pass reads from, once: 0 from its cursors' start, R on a store of
/// R and no coverage, and never moves it after.
#[test]
fn c_is_where_the_first_pass_reads_from_written_once() {
    let log = log();
    let half = log.events.len() / 2;

    let memory = Memory::default();
    arrive(&memory, &log, log.events.len());
    drain(&memory, &mut of_none(Builder::default()), 2_000);
    assert_eq!(coverage(&memory), None);
    assert!(memory.points.borrow().is_empty());

    let memory = Memory::default();
    arrive(&memory, &log, half);
    let mut builder = omitting(Builder::default());
    drain(&memory, &mut builder, 2_000);
    assert_eq!(covers_c_to_s(&memory).from, EventId::new(0));
    arrive(&memory, &log, log.events.len());
    drain(&memory, &mut builder, 2_000);
    let held = covers_c_to_s(&memory);
    assert_eq!((held.from, held.through), (EventId::new(0), log.last()));

    // R and no coverage: a builder gone on from R covers from R.
    let memory = Memory::default();
    arrive(&memory, &log, half);
    drain(&memory, &mut of_none(Builder::default()), 2_000);
    let r = r_of(&memory).events;
    assert_eq!(r, log.events[half - 1].id);
    let mut builder = omitting(start_builder(&memory, ms(2_000)).unwrap());
    arrive(&memory, &log, half + 7);
    drain(&memory, &mut builder, 2_000);
    assert_eq!(covers_c_to_s(&memory).from, r);
    for n in [half + 20, log.events.len()] {
        arrive(&memory, &log, n);
        let mut builder = omitting(start_builder(&memory, ms(2_000)).unwrap());
        drain(&memory, &mut builder, 2_000);
        let held = covers_c_to_s(&memory);
        assert_eq!((held.from, held.through), (r, log.events[n - 1].id));
    }
}

/// S moves to the position read with W and R, its summaries and points
/// with it from the events after S only: passes a page at a time, a write
/// that fails, a new process from R and a builder from its cursors' start
/// reading again cover (C, S] as counted after each, and add nothing twice.
#[test]
fn s_and_the_summaries_cover_c_to_s_after_each_pass() {
    let log = log();
    let memory = Memory::default();
    let mut builder = omitting(Builder::default());
    let mut last = EventId::new(0);
    for n in (10..=log.events.len())
        .step_by(17)
        .chain([log.events.len()])
    {
        arrive(&memory, &log, n);
        let built = build_pass_limited(
            &memory,
            &mut builder,
            (ms(2_000), BUILT),
            Retain::default(),
            None,
            (7, 14, SESSION_BYTES_PER_WRITE),
            Timing::default(),
        );
        assert_eq!(built.failure, None);
        let held = covers_c_to_s(&memory);
        assert!(held.through >= last);
        assert_eq!(held.through, builder.events);
        last = held.through;
    }
    drain(&memory, &mut builder, 2_000);
    let whole = covers_c_to_s(&memory);
    assert_eq!(whole.through, log.last());
    assert!(whole.summaries[DENIED].back_ms >= 20_000);
    assert_eq!(whole.summaries[ISSUED].seen, 48);

    memory.batch_fails.set(1);
    let mut again = omitting(Builder::default());
    assert!(pass(&memory, &mut again, 2_000).failure.is_some());
    assert_eq!(coverage(&memory), Some(whole.clone()));
    drain(&memory, &mut again, 2_000);
    assert_eq!(covers_c_to_s(&memory), whole);
    let mut from_r = omitting(start_builder(&memory, ms(2_000)).unwrap());
    drain(&memory, &mut from_r, 2_000);
    assert_eq!(covers_c_to_s(&memory), whole);
}

/// A raised version (marks of another version: no R, a builder from its
/// cursors' start), W cut back to 0, a mark with no R and a coverage of a
/// lower S leave C, S and the summaries as they were, and the passes after
/// them only move S on.
#[test]
fn nothing_lowers_or_removes_the_coverage() {
    let log = log();
    let memory = Memory::default();
    let half = log.events.len() / 2;
    arrive(&memory, &log, half);
    let mut builder = omitting(Builder::default());
    drain(&memory, &mut builder, 2_000);
    let held = covers_c_to_s(&memory);
    let points = memory.points.borrow().clone();

    let lower = {
        let mut lower = held.clone();
        lower.through = EventId::new(3);
        lower
    };
    let marks = [
        Marks {
            sessions: SessionsWritten::at(EventId::new(0)),
            rows: Some(RowsWritten::at(Cursors::default(), i64::MIN)),
            omitted: None,
        },
        Marks {
            sessions: SessionsWritten::at(EventId::new(5)),
            rows: None,
            omitted: None,
        },
        Marks {
            sessions: SessionsWritten::at(EventId::new(5)),
            rows: None,
            omitted: Some(Omitted {
                coverage: lower,
                points: Vec::new(),
            }),
        },
    ];
    for marks in &marks {
        memory.write_ledger_marks(marks).unwrap();
        assert_eq!(coverage(&memory), Some(held.clone()));
        assert_eq!(*memory.points.borrow(), points);
    }
    // A raised version: the marks of another version give no R.
    let mut sessions = memory.mark.borrow().unwrap();
    sessions.ledger_v = LEDGER_V - 1;
    memory.mark.replace(Some(sessions));
    memory.rows_mark.replace(None);
    let mut builder = omitting(start_builder(&memory, ms(2_000)).unwrap());
    assert_eq!(builder.events, EventId::new(0));
    pass(&memory, &mut builder, 2_000);
    assert_eq!(coverage(&memory), Some(held.clone()));
    arrive(&memory, &log, log.events.len());
    drain(&memory, &mut builder, 2_000);
    let after = covers_c_to_s(&memory);
    assert_eq!((after.from, after.through), (held.from, log.last()));
}

/// A store whose R is past its S (a builder that knows no S moved R) or
/// whose kinds are not the builder's stops it: C, S and the summaries stay
/// as they are whatever it reads, and [`Builder::omission_stopped`] says so.
#[test]
fn a_store_of_r_past_s_or_other_kinds_holds_the_coverage() {
    let log = log();
    let half = log.events.len() / 2;
    for case in ["r past s", "other kinds"] {
        let memory = Memory::default();
        arrive(&memory, &log, half);
        drain(&memory, &mut omitting(Builder::default()), 2_000);
        let held = covers_c_to_s(&memory);
        let mut builder = match case {
            "r past s" => {
                arrive(&memory, &log, half + 10);
                drain(&memory, &mut of_none(Builder::default()), 2_000);
                assert!(r_of(&memory).events > held.through);
                omitting(start_builder(&memory, ms(2_000)).unwrap())
            }
            _ => {
                let mut builder = of_none(Builder::default());
                builder.set_omission(Omission::of([DENIED], EVERY));
                builder
            }
        };
        assert!(!builder.omission_stopped(), "{case}");
        arrive(&memory, &log, log.events.len());
        drain(&memory, &mut builder, 2_000);
        assert!(builder.omission_stopped(), "{case}");
        assert_eq!(builder.coverage(), None, "{case}");
        assert_eq!(coverage(&memory), Some(held.clone()), "{case}");
        let mut again = omitting(start_builder(&memory, ms(2_000)).unwrap());
        pass(&memory, &mut again, 2_000);
        assert!(again.omission_stopped(), "{case}");
        assert_eq!(coverage(&memory), Some(held), "{case}");
    }
}

/// The store in memory seeks a kind's points by position and by time, as
/// `ledger_omitted_points` does: each point's p, M, L and T rise in order.
#[test]
fn the_points_rise_and_are_sought_by_position_and_time() {
    let log = log();
    let memory = Memory::default();
    arrive(&memory, &log, log.events.len());
    drain(&memory, &mut omitting(Builder::default()), 2_000);
    for kind in [DENIED, ISSUED] {
        let all = memory
            .ledger_omitted_points_after(kind, EventId::new(0), usize::MAX)
            .unwrap();
        assert!(all.len() > 10, "{kind}");
        for pair in all.windows(2) {
            assert!(pair[0].at < pair[1].at);
            assert!(pair[0].max_ms <= pair[1].max_ms);
            assert!(pair[0].last <= pair[1].last);
            assert!(pair[0].timed <= pair[1].timed);
        }
        let after = all[3].at;
        assert_eq!(
            memory.ledger_omitted_points_after(kind, after, 2).unwrap(),
            all[4..6].to_vec()
        );
        let ms = all[5].max_ms.unwrap();
        let before = memory.ledger_omitted_point_before_ms(kind, ms).unwrap();
        let expected = all.iter().rev().find(|point| point.max_ms < Some(ms));
        assert_eq!(before.as_ref(), expected);
    }
}

/// Two builders on a store of R and no coverage, one from its cursors'
/// start (C 0) and one gone on from R (C at R): the one gone on from R reads
/// first, the other writes its coverage before it writes its own. The store
/// refuses that write whole (its W and R stay those of the coverage it
/// holds, so R is never past S and nothing stops), and its next pass takes
/// the store's coverage, reads again the events from its S up to where it
/// read, and writes S on with the summaries and points counted of (0, S].
#[test]
fn a_builder_whose_coverage_another_wrote_first_takes_it_and_goes_on() {
    let log = log();
    let half = log.events.len() / 2;
    let memory = Memory::default();
    arrive(&memory, &log, half);
    drain(&memory, &mut of_none(Builder::default()), 2_000);
    let r = r_of(&memory).events;
    let mut from_r = omitting(start_builder(&memory, ms(2_000)).unwrap());
    arrive(&memory, &log, half + 40);
    // The other builder's one pass, cut short of where this one reads.
    memory
        .before_steps
        .replace(Some(Box::new(move |memory: &Memory| {
            let mut from_start = omitting(Builder::default());
            let built = build_pass_limited(
                memory,
                &mut from_start,
                (ms(2_000), BUILT),
                Retain::default(),
                None,
                (half, half + 10, SESSION_BYTES_PER_WRITE),
                Timing::default(),
            );
            assert_eq!(built.failure, None);
        })));
    pass(&memory, &mut from_r, 2_000);
    assert!(memory.before_steps.borrow().is_none(), "the other pass ran");
    let first = covers_c_to_s(&memory);
    assert_eq!(first.from, EventId::new(0));
    assert!(first.through < from_r.events);
    assert!(r_of(&memory).events <= first.through);
    assert!(first.through > r);

    arrive(&memory, &log, log.events.len());
    drain(&memory, &mut from_r, 2_000);
    assert!(!from_r.omission_stopped());
    let held = covers_c_to_s(&memory);
    assert_eq!((held.from, held.through), (EventId::new(0), log.last()));
    assert_eq!(r_of(&memory).events, log.last());
}

/// A builder of no kind (the version's) on a store holding a coverage of
/// some kinds stops: [`Builder::omission_stopped`] says so, from its first
/// pass, a new process's from R and one from its cursors' start alike, and
/// it writes no C, S, summary or point, the store's left as they were.
#[test]
fn a_builder_of_no_kind_on_a_store_of_a_coverage_stops() {
    let log = log();
    let half = log.events.len() / 2;
    let memory = Memory::default();
    arrive(&memory, &log, half);
    drain(&memory, &mut omitting(Builder::default()), 2_000);
    let held = covers_c_to_s(&memory);
    let points = memory.points.borrow().clone();
    arrive(&memory, &log, log.events.len());
    let mut from_start = of_none(Builder::default());
    assert!(!from_start.omission_stopped());
    drain(&memory, &mut from_start, 2_000);
    assert!(from_start.omission_stopped());
    assert_eq!(from_start.coverage(), None);
    let mut from_r = of_none(start_builder(&memory, ms(2_000)).unwrap());
    pass(&memory, &mut from_r, 2_000);
    assert!(from_r.omission_stopped());
    assert_eq!(r_of(&memory).events, log.last());
    assert_eq!(coverage(&memory), Some(held));
    assert_eq!(*memory.points.borrow(), points);
}

/// A window read of a store whose coverage holds the queue's refusals takes
/// the omitted events for its edges: with the queue rows of the omitted
/// events gone (as a builder that omits them writes none), its positions,
/// `stats`' edges, the latest event and the queue's last record before a
/// time are those of the rows that hold them, whole and from a time, with a
/// time cursor and an event id `--until`. The run's events of the kind and
/// the events of no time stay out of every answer but the latest, as there.
#[test]
fn a_window_takes_the_omitted_events_of_the_store_for_its_edges() {
    use crate::domain::ledger::testkit::ms;
    use crate::domain::ledger::window::{WindowQuery, WindowRows, select, select_from_time};
    use crate::domain::stats::Cursor;
    let mut log = Log::default();
    for n in 0..30 {
        log.add(None, None, DENIED, json!({}), n * 10);
        log.add(None, Some(&run_id(4)), DENIED, json!({}), n * 10 + 1);
        if n % 7 == 3 {
            log.add(None, None, DENIED, json!({}), n * 10 + 2);
            log.events.last_mut().unwrap().created_at = "not a time".into();
        }
        if n % 5 == 0 {
            log.add(
                None,
                None,
                "supervisor_alive",
                json!({"supervisor": "s1"}),
                n * 10 + 3,
            );
        }
    }
    let now = (crate::domain::ledger::testkit::T0 + 10_000) * 1000;
    let whole = crate::domain::ledger::build(&log.events, now, "built");
    let omitted = |row: &LedgerRow| {
        row.kind == LedgerKind::Queue
            && row
                .subject
                .as_ref()
                .is_some_and(|subject| subject.kind.as_str() == DENIED)
    };
    assert!(whole.iter().any(omitted));
    let last = log.last();
    let mut points = Vec::new();
    let mut covered = Coverage::start(&[DENIED.to_owned()], EventId::new(0));
    covered.read(&log.events, 4, last, &mut points);
    let memory = Memory::of(&log);
    for row in whole.iter().filter(|row| !omitted(row)) {
        memory
            .rows
            .borrow_mut()
            .insert((row.kind, row.key.clone()), row.clone());
    }
    memory.write_omitted(&Omitted {
        coverage: covered,
        points,
    });
    let queries = [
        WindowQuery {
            now_ms: now,
            ..WindowQuery::default()
        },
        WindowQuery {
            since: Some(Cursor::Time(ms(95))),
            until: Some(Cursor::Time(ms(201))),
            now_ms: now,
            ..WindowQuery::default()
        },
        WindowQuery {
            until: Some(Cursor::Event(EventId::new(40))),
            now_ms: now,
            ..WindowQuery::default()
        },
        WindowQuery {
            read_from_ms: Some(ms(150)),
            now_ms: now,
            ..WindowQuery::default()
        },
    ];
    for query in &queries {
        let stored = memory.ledger_window(query).unwrap();
        let expected = if query.read_from_ms.is_some() {
            select_from_time(whole.clone(), query)
        } else {
            select(whole.clone(), query)
        };
        let edges = |window: &crate::domain::ledger::window::LedgerWindow| {
            let latest = window.events().iter().map(|event| event.id).max();
            (
                window.after,
                window.upto,
                window.start_ms,
                window.end_ms,
                window.host_from_ms(),
                latest,
            )
        };
        assert_eq!(edges(&stored), edges(&expected), "{query:?}");
    }
    let before = crate::application::rows_from_of(&memory, &queries[3])
        .unwrap()
        .unwrap();
    let mut read = WindowRows::new(&queries[3], 0);
    for row in whole.iter().filter(|row| row.kind == LedgerKind::Queue) {
        read.take(row.clone());
    }
    assert_eq!(before, read.rows_from().unwrap());
}
