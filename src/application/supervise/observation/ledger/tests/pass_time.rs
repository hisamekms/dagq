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
pub(super) struct Leaping(pub(super) std::sync::Mutex<Instant>);

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

/// A queue row of its own, of a kind copied to queue rows ([`ONE_EVENT`]),
/// in the payload of a refusal of a telemetry report.
fn denied(log: &mut Log, secs: i64) {
    log.add(
        None,
        None,
        ONE_EVENT,
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

/// A clock each of whose reads is `.1` after the one before.
struct Stepping(std::sync::Mutex<Instant>, Duration);

impl crate::application::Clock for Stepping {
    fn system_time(&self) -> std::time::SystemTime {
        std::time::UNIX_EPOCH
    }

    fn monotonic(&self) -> Instant {
        let mut next = self.0.lock().unwrap();
        let at = *next;
        *next += self.1;
        at
    }
}

/// 100 events, tasks and queue rows in turn, read 10 a page.
fn tasks_and_queue_rows() -> Log {
    let mut log = Log::default();
    for secs in 1..=100 {
        if secs % 2 == 0 {
            log.add(Some(secs), None, "task_created", json!({}), secs);
        } else {
            log.add(None, None, ONE_EVENT, json!({}), secs);
        }
    }
    log
}

/// One pass of the builder read 10 events a page, of the events' bound of
/// production, on `clock`.
fn paged_pass(
    memory: &Memory,
    builder: &mut Builder,
    clock: &dyn crate::application::Clock,
) -> Built {
    build_pass_limited(
        memory,
        builder,
        (ms(LATER), BUILT),
        Retain::default(),
        None,
        (10, EVENTS_PER_PASS, SESSION_BYTES_PER_WRITE),
        Timing(Some(clock)),
    )
}

/// The rows a builder with no clock writes from `memory`'s inputs up to
/// `through` alone (its events and node samples), run until it caught up:
/// what a store whose R is at `through` must record of them.
fn rows_up_to(memory: &Memory, through: Cursors) -> BTreeMap<(LedgerKind, String), LedgerRow> {
    let reference = Memory::default();
    reference.events.replace(
        memory
            .events
            .borrow()
            .iter()
            .filter(|event| event.id <= through.events)
            .cloned()
            .collect(),
    );
    reference.samples.replace(
        memory
            .samples
            .borrow()
            .iter()
            .filter(|sample| sample.id <= through.samples)
            .cloned()
            .collect(),
    );
    let mut builder = Builder::default();
    for _ in 0..20 {
        let built = build_pass(
            &reference,
            &mut builder,
            (ms(LATER), BUILT),
            Retain::default(),
            None,
        );
        assert_eq!(built.failure, None);
        if builder.backlog() == 0 {
            break;
        }
    }
    assert_eq!(builder.backlog(), 0, "the reference caught up");
    reference.rows.take()
}

/// R of each kind records every input up to it: for the queue rows'
/// position and for the run, task, session and node rows' one (each the
/// position both share when it has none of its own), every row of that
/// kind a builder with no clock writes from the inputs up to it alone
/// ([`rows_up_to`]) is held by `memory` with its inputs no fewer. Whether R
/// of each kind is held, and where.
#[track_caller]
fn r_records_every_input(memory: &Memory, label: &str) -> Option<(Cursors, Cursors)> {
    let rows = (*memory.rows_mark.borrow())?;
    let core = rows.core.map_or(rows.through, |position| position.through);
    let queue = rows.queue.map_or(rows.through, |position| position.through);
    for (through, kinds) in [
        (
            core,
            &[
                LedgerKind::Run,
                LedgerKind::Task,
                LedgerKind::Session,
                LedgerKind::Node,
            ][..],
        ),
        (queue, &[LedgerKind::Queue][..]),
    ] {
        let held = memory.rows.borrow();
        for (key, row) in rows_up_to(memory, through) {
            if !kinds.contains(&key.0) {
                continue;
            }
            let stored = held.get(&key);
            assert!(
                stored.is_some_and(|stored| {
                    stored.inputs_through.events >= row.inputs_through.events
                        && stored.inputs_through.node_samples >= row.inputs_through.node_samples
                }),
                "{label}: R {through:?} past an input of {key:?} not recorded: {:?} of {:?}",
                stored.map(|stored| &stored.inputs_through),
                row.inputs_through
            );
        }
    }
    Some((core, queue))
}

/// The normal read of the events stops at the pass's time, whatever its
/// count of events allows: on a clock that takes [`PASS_TIME`] at each
/// look, every pass reads one page, short of the latest event, and is cut
/// so that the next starts at once ([`goes_on`], [`pass_due`]) and reads on
/// from where it stopped. R of each kind records every input up to it, and
/// on a clock that leaves the pass its time the passes catch up: R of each
/// kind at the latest event and every task's row written, none skipped. On
/// that clock a fresh builder reads all 100 in one pass and is not cut.
#[test]
fn the_read_of_the_events_stops_at_the_pass_time_and_the_next_reads_on() {
    let log = tasks_and_queue_rows();
    let interval = Duration::from_secs(INTERVAL_SECS);
    let leaping = Leaping(std::sync::Mutex::new(Instant::now()));
    let memory = Memory::of(&log);
    let mut builder = Builder::default();
    for page in 1..=4_usize {
        let before = builder.events;
        let built = paged_pass(&memory, &mut builder, &leaping);
        let label = format!("page {page}");
        assert_eq!(built.failure, None, "{label}");
        assert_eq!(built.work.events, 10, "{label}: one page, not the count's");
        assert_eq!(built.work.pages, 1, "{label}");
        assert!(
            builder.events > before,
            "{label}: reads on from where it stopped"
        );
        assert_eq!(builder.events, log.events[page * 10 - 1].id, "{label}");
        assert!(built.cut, "{label}");
        assert_eq!(built.work.latest, log.last().as_i64(), "{label}");
        assert!(goes_on(&built), "{label}: {:?}", built.marks.rows);
        let at = Instant::now();
        assert!(pass_due(Some(at), at, interval, goes_on(&built)), "{label}");
        r_records_every_input(&memory, &label);
    }

    let stepping = Stepping(
        std::sync::Mutex::new(Instant::now()),
        Duration::from_micros(100),
    );
    let mut passes = 0;
    loop {
        let built = paged_pass(&memory, &mut builder, &stepping);
        assert_eq!(built.failure, None);
        r_records_every_input(&memory, "catching up");
        passes += 1;
        assert!(passes < 10, "passes {passes}");
        if !goes_on(&built) {
            break;
        }
    }
    assert_eq!(builder.events, log.last());
    let rows = memory.rows_mark.borrow().expect("R");
    // A kind of no position of its own is at the one both share.
    for position in [rows.core, rows.queue] {
        let through = position.map_or(rows.through, |position| position.through);
        assert_eq!(through.events, log.last(), "{rows:?}");
    }
    assert_eq!(memory.count(LedgerKind::Task), 50);

    let memory = Memory::of(&log);
    let mut fresh = Builder::default();
    let stepping = Stepping(
        std::sync::Mutex::new(Instant::now()),
        Duration::from_micros(100),
    );
    let built = paged_pass(&memory, &mut fresh, &stepping);
    assert_eq!(built.failure, None);
    assert_eq!((built.work.events, built.work.pages), (100, 10));
    assert!(!built.cut);
    assert!(!goes_on(&built));
    assert_eq!(fresh.events, log.last());
    assert_eq!(r_of(&memory).events, log.last());
}

/// Runs worked, planners' sessions, queue rows and node samples, more than
/// one pass on [`Stepping`] reads: the passes it cuts at their time, those
/// whose reads took it short of the latest event and the one whose writes
/// took it after reading up to there, leave R of each kind
/// (the queue rows' and the run, task, session and node rows') where every
/// input up to it is recorded by a stored row, whatever they left unread
/// or unwritten; the next passes read on from where each stopped and catch
/// up, R of each kind at the latest event and the last sample, every row
/// of the inputs written.
#[test]
fn a_pass_cut_at_its_time_leaves_r_of_each_kind_before_what_it_did_not_record() {
    let mut log = Log::default();
    let start = node::bucket_of(T0);
    let memory = Memory::default();
    for n in 1..=12_u8 {
        let secs = i64::from(n) * 1_000;
        let run = run_id(n);
        log.worked(i64::from(n), &run, secs)
            .integrated(i64::from(n), &run, secs + 700)
            .pushed(i64::from(n), &run, secs + 930);
        log.add(
            None,
            None,
            "claim_held",
            json!({"supervisor": "s1"}),
            secs + 940,
        );
        log.add(None, None, ONE_EVENT, json!({}), secs + 945);
        log.add(
            None,
            None,
            "session_opened",
            json!({"kind": "runtime_planner", "planner_id": n}),
            secs + 950,
        );
        let opened = log.last().as_i64();
        log.add(
            None,
            None,
            "session_closed",
            json!({"kind": "runtime_planner", "planner_id": n,
                   "opened_event_id": opened, "session_id": format!("p-{n}")}),
            secs + 960,
        );
        memory
            .samples
            .borrow_mut()
            .push(sample(i64::from(n), start + i64::from(n) * 600));
    }
    memory.events.replace(log.events.clone());
    let stepping = Stepping(std::sync::Mutex::new(Instant::now()), PASS_TIME / 40);
    let mut builder = Builder::default();
    let (mut passes, mut cut_reading, mut cut_writing) = (0, 0, 0);
    loop {
        let built = paged_pass(&memory, &mut builder, &stepping);
        let label = format!("pass {passes}");
        assert_eq!(built.failure, None, "{label}");
        passes += 1;
        assert!(passes < 100, "{label}");
        let r = r_records_every_input(&memory, &label);
        if !goes_on(&built) {
            break;
        }
        assert!(built.cut, "{label}");
        let behind = r.is_some_and(|(core, queue)| core.events.min(queue.events) < log.last());
        assert!(behind, "{label}: {r:?}");
        if builder.events < log.last() {
            // Its reads took its time: short of the latest event.
            assert!(built.work.events > 0, "{label}");
            cut_reading += 1;
        } else if built.written > 0 {
            // Read up to the latest, its writes took the rest.
            cut_writing += 1;
        }
    }
    assert!(cut_reading >= 2, "passes cut reading: {cut_reading}");
    assert!(cut_writing >= 1, "passes cut writing: {cut_writing}");
    assert_eq!(builder.events, log.last());
    let (core, queue) = r_records_every_input(&memory, "caught up").expect("R");
    assert_eq!((core.events, queue.events), (log.last(), log.last()));
    assert_eq!(core.samples, 12);
    let reference = rows_up_to(&memory, core);
    for kind in [
        LedgerKind::Run,
        LedgerKind::Task,
        LedgerKind::Session,
        LedgerKind::Queue,
        LedgerKind::Node,
    ] {
        let of_kind = reference.keys().filter(|key| key.0 == kind).count();
        assert!(of_kind > 0, "{kind:?}");
        assert_eq!(memory.count(kind), of_kind, "{kind:?}");
    }
}
