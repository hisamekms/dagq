//! How much a pass writes is the time it takes ([`PASS_TIME`], on the
//! store's clock: [`Costed`]), and while a pass leaves the ledger behind
//! the next starts at once ([`goes_on`], [`pass_due`]).

use super::*;
use crate::application::ports::frozen::values;
use crate::domain::ledger::builder::INTERVAL_SECS;
use crate::domain::ledger::testkit::frozen::{Step as FrozenStep, inputs};

/// `n` tasks, each a row of one event.
fn tasks(n: i64) -> Log {
    let mut log = Log::default();
    for task in 1..=n {
        log.add(Some(task), None, "task_created", json!({}), task);
    }
    log
}

/// The tasks the store holds no row of.
fn unwritten(memory: &Memory, log: &Log) -> Vec<RunEvent> {
    log.events
        .iter()
        .filter(|event| {
            let task = event.task_id.unwrap().to_string();
            memory
                .rows
                .borrow()
                .get(&(LedgerKind::Task, task))
                .is_none()
        })
        .cloned()
        .collect()
}

/// The same events and marks, the writes taking another time: a pass
/// writes as many rows as fit in [`PASS_TIME`], not a count, stops there
/// and leaves the rest marked for the next, R before the first left; the
/// passes after it write them, none skipped.
#[test]
fn a_pass_writes_until_its_time_and_leaves_the_rest_to_the_next() {
    let log = tasks(600);
    for (per_write, writes) in [(10, 100), (4, 250)] {
        let memory = Memory::of(&log);
        memory.clock.costs(Duration::from_millis(per_write));
        let mut builder = Builder::default();
        let mut written = 0;
        let mut passes = 0;
        loop {
            let built = pass(&memory, &mut builder, LATER);
            assert_eq!(built.failure, None);
            passes += 1;
            written += built.written;
            assert_eq!(memory.count(LedgerKind::Task), written);
            let left = unwritten(&memory, &log);
            let Some(first) = left.first() else {
                break;
            };
            assert_eq!(built.written, writes, "{per_write} ms a write");
            assert!(built.backlog >= left.len(), "every row left stays marked");
            // No input is skipped: R stays before the first task left.
            assert!(r_of(&memory).events < first.id, "{per_write} ms a write");
        }
        assert_eq!(written, 600);
        assert_eq!(passes, 600_usize.div_ceil(writes));
        assert_eq!(r_of(&memory).events, log.last());
    }
}

/// A clock each of whose reads is [`PASS_TIME`] after the one before: a
/// pass on it has taken its time by its first look, before any write.
struct Leaping(std::sync::Mutex<Instant>);

impl crate::application::Clock for Leaping {
    fn system_time(&self) -> std::time::SystemTime {
        std::time::UNIX_EPOCH
    }

    fn monotonic(&self) -> Instant {
        let mut next = self.0.lock().unwrap();
        let at = *next;
        *next += PASS_TIME;
        at
    }
}

/// A pass stops at [`PASS_TIME`] whatever marks it took first
/// ([`HOLDING_R_PER_PASS`]): one past it by its first write writes no
/// second, and one whose reads alone took it writes none. It leaves every
/// mark it did not write, R where it was before them and says it was cut,
/// and the next pass writes them.
#[test]
fn a_pass_past_its_time_writes_no_more_and_leaves_the_rest_and_r() {
    let log = tasks(100);
    let memory = Memory::of(&log);
    memory.clock.costs(PASS_TIME * 2);
    let mut builder = Builder::default();
    let built = pass(&memory, &mut builder, LATER);
    assert_eq!((built.failure, built.written, built.cut), (None, 1, true));
    assert_eq!(built.backlog, 99);
    assert!(r_of(&memory).events < log.events[1].id);

    let r = r_of(&memory);
    let leaping = Leaping(std::sync::Mutex::new(Instant::now()));
    let read_alone = build_pass_timed(
        &memory,
        &mut builder,
        (ms(LATER), BUILT),
        Retain::default(),
        None,
        (EVENTS_PER_READ, SESSION_BYTES_PER_WRITE),
        Timing(Some(&leaping)),
    );
    assert_eq!(
        (read_alone.failure, read_alone.written, read_alone.cut),
        (None, 0, true)
    );
    assert_eq!(read_alone.backlog, 99);
    assert_eq!(r_of(&memory).events, r.events, "R stays before the marks");

    memory.clock.costs(Duration::from_millis(1));
    let next = pass(&memory, &mut builder, LATER);
    assert_eq!((next.written, next.backlog, next.cut), (99, 0, false));
    assert_eq!(r_of(&memory).events, log.last());
}

/// A pass without a clock (a fold called directly) writes every mark it
/// takes.
#[test]
fn a_pass_without_a_clock_is_not_bounded_by_time() {
    let log = tasks(i64::try_from(WRITES_PER_PASS).unwrap() * 3);
    let memory = Memory::of(&log);
    memory.clock.costs(PASS_TIME);
    let built = build_pass(
        &memory,
        &mut Builder::default(),
        (ms(LATER), BUILT),
        Retain::default(),
        None,
    );
    assert_eq!((built.written, built.backlog), (WRITES_PER_PASS * 3, 0));
}

/// A pass that stopped at a bound with work left and left R before the
/// latest event goes on at once, whether R moved or not; one that stopped
/// at no bound, caught up or failed waits for the interval.
#[test]
fn a_pass_goes_on_while_it_stops_at_a_bound_and_leaves_the_ledger_behind() {
    let behind = |r: i64, latest: i64, cut: bool| Built {
        written: 1,
        marks: MarkObservation {
            rows: Some(RowsWritten::at(
                Cursors {
                    events: EventId::new(r),
                    ..Cursors::default()
                },
                0,
            )),
            ..MarkObservation::default()
        },
        work: PassWork {
            latest,
            ..PassWork::default()
        },
        cut,
        ..Built::default()
    };
    assert!(goes_on(&behind(5, 10, true)));
    assert!(!goes_on(&behind(5, 10, false)), "stopped at no bound");
    assert!(!goes_on(&behind(10, 10, true)), "caught up");
    let failed = Built {
        failure: Some("busy".to_owned()),
        ..behind(5, 10, true)
    };
    assert!(!goes_on(&failed));
    let idle = Built {
        written: 0,
        ..behind(5, 10, true)
    };
    assert!(!goes_on(&idle), "wrote nothing and read no event");
    let read = Built {
        written: 0,
        work: PassWork {
            latest: 10,
            events: 3,
            ..PassWork::default()
        },
        ..behind(5, 10, true)
    };
    assert!(goes_on(&read), "read events");
    let no_r = Built {
        written: 1,
        work: PassWork {
            latest: 10,
            ..PassWork::default()
        },
        cut: true,
        ..Built::default()
    };
    assert!(goes_on(&no_r), "a builder of no R is behind");

    let interval = Duration::from_secs(INTERVAL_SECS);
    let at = Instant::now();
    assert!(pass_due(None, at, interval, false));
    assert!(!pass_due(Some(at), at + interval / 2, interval, false));
    assert!(pass_due(Some(at), at + interval / 2, interval, true));
    assert!(pass_due(Some(at), at + interval, interval, false));
}

/// A refusal of a telemetry report: a queue row of its own.
fn denied(log: &mut Log, secs: i64) {
    log.add(
        None,
        None,
        "authorization_denied",
        json!({"event": "authorization_denied", "role": "executor",
               "capability": "telemetry.report", "reason": "not_on_resource",
               "resource": {"kind": "run", "id": run_id(9)}}),
        secs,
    );
}

/// Each pass pays a cost before it writes (most of [`PASS_TIME`]) and an
/// event comes in before each, so that each pass marks the rows it read
/// again: a pass folds only the rows it writes and those the new event
/// changed, not again those the passes before left marked, and the later
/// passes write the rest of 5,000 queue rows of one page a share each until
/// R reaches the events.
#[test]
fn later_passes_go_on_whatever_each_pays_before_it_writes() {
    let mut log = Log::default();
    for secs in 1..=i64::try_from(EVENTS_PER_READ).unwrap() {
        denied(&mut log, secs);
    }
    let memory = Memory::of(&log);
    memory.clock.costs_each_pass(PASS_TIME * 3 / 5);
    memory.clock.costs(Duration::from_millis(4));
    let mut builder = Builder::default();
    let first = pass(&memory, &mut builder, LATER);
    assert_eq!(first.failure, None);
    let mut written = first.written;
    let mut each = Vec::new();
    let mut passes = 1;
    let mut secs = i64::try_from(EVENTS_PER_READ).unwrap();
    while builder.backlog() > 0 {
        secs += 1;
        denied(&mut log, secs);
        memory.events.replace(log.events.clone());
        let built = pass(&memory, &mut builder, LATER);
        assert_eq!(built.failure, None);
        passes += 1;
        each.push(built.written);
        assert!(
            built.work.folded <= built.written + 3,
            "pass {passes}: {} folded for {} written",
            built.work.folded,
            built.written
        );
        written += built.written;
        assert!(passes < 80, "{written} written");
    }
    // 0.4 s left of each second at 4 ms a write: 100 rows a pass.
    let (last, before) = each.split_last().unwrap();
    assert!(before.iter().all(|&rows| rows == 100), "{each:?}");
    assert!((1..=100).contains(last), "{each:?}");
    assert!(written >= EVENTS_PER_READ, "{written}");
    let last = pass(&memory, &mut builder, LATER);
    assert_eq!((last.failure, last.backlog), (None, 0));
    assert_eq!(r_of(&memory).events, log.last());
}

/// The rows of one page of events: 5,000 queue rows of one event each,
/// read in one page, hold R at the same place until the last of them is
/// written, and the passes writing them ([`CHECKS_PER_PASS`] a pass) go on
/// back to back all the same, R not moving between the first two.
#[test]
fn passes_go_on_over_the_queue_rows_of_one_page_while_r_stays() {
    let mut log = Log::default();
    for secs in 1..=i64::try_from(EVENTS_PER_READ).unwrap() {
        denied(&mut log, secs);
    }
    let memory = Memory::of(&log);
    memory.clock.costs(Duration::from_micros(500));
    let mut builder = Builder::default();
    let interval = Duration::from_secs(INTERVAL_SECS);
    let origin = Instant::now();
    let mut watch = LedgerWatch::default();
    let (mut starts, mut written, mut rs) = (Vec::new(), Vec::new(), Vec::new());
    for turn in 0..100_u64 {
        if !watch.starts(origin + Duration::from_secs(turn), interval) {
            continue;
        }
        starts.push(turn);
        let built = pass(&memory, &mut builder, LATER);
        assert_eq!(built.failure, None);
        written.push(built.written);
        rs.push(r_of(&memory).events);
        watch.reaped(&built);
    }
    assert_eq!(written[..3], [CHECKS_PER_PASS, CHECKS_PER_PASS, 1_000]);
    assert_eq!(rs[0], rs[1], "R stays while the page's rows are written");
    assert!(rs[0] < log.last());
    assert_eq!(rs[2], log.last());
    assert_eq!(starts[..4], [0, 1, 2, 62]);
}

/// The watch's turns a second apart, decided by its own steps
/// ([`LedgerWatch::starts`], [`LedgerWatch::reaped`]): behind, a pass starts on the turn
/// after the last, not [`INTERVAL_SECS`] later, until R reaches the
/// events; then every [`INTERVAL_SECS`] again, and behind again after
/// more events come, at once again.
#[test]
fn passes_go_on_while_behind_and_come_every_interval_once_caught_up() {
    let mut log = tasks(1_000);
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    let interval = Duration::from_secs(INTERVAL_SECS);
    let origin = Instant::now();
    let mut watch = LedgerWatch::default();
    let mut starts = Vec::new();
    for turn in 0..400_u64 {
        if turn == 200 {
            for task in 1_001..=1_500 {
                log.add(Some(task), None, "task_created", json!({}), task);
            }
            memory.events.replace(log.events.clone());
        }
        let at = origin + Duration::from_secs(turn);
        if !watch.starts(at, interval) {
            continue;
        }
        starts.push(turn);
        let built = pass(&memory, &mut builder, LATER);
        assert_eq!(built.failure, None);
        watch.reaped(&built);
    }
    // 200 rows a pass: five passes back to back for the first 1,000, then
    // the interval; the 500 more come in at turn 200, which the pass of
    // the interval at 244 finds, and three passes write them.
    assert_eq!(
        starts,
        [0, 1, 2, 3, 4, 64, 124, 184, 244, 245, 246, 306, 366]
    );
    assert_eq!(r_of(&memory).events, log.last());

    // A failed pass waits for the interval.
    watch.goes_on = true;
    let failed = Built {
        failure: Some("busy".to_owned()),
        cut: true,
        ..Built::default()
    };
    watch.reaped(&failed);
    assert!(!watch.goes_on);
}

/// Over each input of the values fixed, the builder bounded by a count of
/// rows (the bound of before: [`WRITES_PER_PASS`] a pass, as the store's
/// clock reproduces it) and the builder bounded by time with writes as fast
/// as they are (about half a millisecond each) step by step: after each
/// step the time's has recorded every input the count's has (R no earlier,
/// each row's inputs no fewer), and once both caught up their rows are the
/// same and so are the values of stats, kpi and report: read of each input
/// on which the two went apart, the others' stores being the same after
/// every step (their rows and marks, all the readers read besides the
/// events).
#[test]
fn the_time_bound_records_what_the_count_did_and_makes_the_same_ledger() {
    let fast = Duration::from_micros(500);
    // Some step where the time's R is past the count's: a pass the count
    // cut, which the time did not.
    let mut ahead = false;
    for input in inputs() {
        let mut apart = false;
        let mut caught = input.clone();
        caught.steps.push(FrozenStep::Drain(LATER));
        let mut counted = Vec::new();
        let count = super::frozen::built_on(Memory::default(), &caught, None, |memory, _| {
            counted.push((
                rows_written(memory).unwrap(),
                memory.rows.borrow().clone(),
                *memory.mark.borrow(),
                *memory.rows_mark.borrow(),
            ));
        });
        let timed_memory = Memory::default();
        timed_memory.clock.costs(fast);
        let mut step = 0;
        let time = super::frozen::built_on(timed_memory, &caught, None, |memory, _| {
            let (r, rows, mark, rows_mark) = &counted[step];
            step += 1;
            apart |= *memory.rows.borrow() != *rows
                || *memory.mark.borrow() != *mark
                || *memory.rows_mark.borrow() != *rows_mark;
            if let Some(r) = r {
                let timed = rows_written(memory).unwrap();
                ahead |= timed.is_some_and(|timed| timed.events > r.events);
                assert!(
                    timed.is_some_and(|timed| timed.events >= r.events),
                    "{} step {step}: {timed:?} before {r:?}",
                    input.name
                );
            }
            let held = memory.rows.borrow();
            for (key, row) in rows {
                let timed = held.get(key);
                assert!(
                    timed.is_some_and(
                        |timed| timed.inputs_through.events >= row.inputs_through.events
                    ),
                    "{} step {step}: {key:?}",
                    input.name
                );
            }
        });
        assert_eq!(step, counted.len());
        assert_eq!(*time.rows.borrow(), *count.rows.borrow(), "{}", input.name);
        if apart {
            assert_eq!(
                values(&time, &caught),
                values(&count, &caught),
                "{}",
                input.name
            );
        }
    }
    assert!(ahead);
}
