//! The latest time of an event read up to R, which a builder going on from
//! R takes from the mark ([`rows_to_go_on_from`]) and reads no event for,
//! and the rows that come due with time or a wait after a start from R.

use super::super::waits::whole;
use super::*;
use crate::domain::ledger::settle::SETTLE_GRACE_MS;

/// A log of tasks created at `times` (secs), one event each.
fn tasks_at(times: &[i64]) -> Log {
    let mut log = Log::default();
    for (task, secs) in times.iter().enumerate() {
        let task = i64::try_from(task).unwrap() + 1;
        log.add(Some(task), None, "task_created", json!({}), *secs);
    }
    log
}

/// The time R's mark holds.
fn read_ms_of(memory: &Memory) -> Option<i64> {
    memory.rows_mark.borrow().and_then(|rows| rows.read_ms)
}

/// A store drained at R, whose last event is timed `order`, and a pass at a
/// time before R's latest: a started builder folds at the time the build of
/// every event up to R folds at, as a builder at the cursors' start on the
/// same store does, and writes the same rows, none left behind the store's.
#[test]
fn a_started_builder_folds_at_the_time_read_up_to_r_as_the_build_of_every_event() {
    for (order, times, before) in [
        ("the event at R timed before an earlier one", [200, 100], 50),
        ("in the order of their ids", [100, 200], 150),
    ] {
        let log = tasks_at(&times);
        let memory = drained_store(&log, &[], log.events.len(), 300);
        let r = r_of(&memory);
        assert_eq!(r.events, log.last(), "{order}");
        let whole = read_ms_at(&log, log.last());
        assert_eq!(whole, ms(times[0].max(times[1])), "{order}");
        assert_eq!(read_ms_of(&memory), Some(whole), "{order}");

        let expected = from_the_start_on(&memory, &log, before);
        let mut from_start = Builder::default();
        pass(&Memory::of(&log), &mut from_start, 300);
        let mut started = start_builder(&memory, ms(before)).unwrap();
        assert_eq!(started.cursors(), r, "{order}");
        assert_eq!(
            started.folds_at(ms(before)),
            from_start.folds_at(ms(before)),
            "{order}"
        );
        assert_eq!(started.folds_at(ms(before)), whole, "{order}");
        let built = pass(&memory, &mut started, before);
        assert_eq!((built.backlog, built.failure), (0, None), "{order}");
        assert_eq!(
            differing(&memory.rows.borrow(), &expected),
            Vec::new(),
            "{order}"
        );
    }
}

/// A legacy single-R certificate left before P holds R's time, not P's
/// or that of the event at R. A started builder
/// folds at the time of the build of every event up to R, and so does each
/// pass as it reads (R,P] again, up to each position, and its rows end as
/// those of a builder from its cursors' start. The same with each event
/// timed before the one before it.
#[test]
fn a_legacy_r_left_before_p_holds_its_time_and_a_start_follows_each_position() {
    let bound = WRITES_PER_PASS;
    let count = bound + 30;
    // The first the latest up to R, one past R later than all, the rest
    // earlier: the time at P is not the time at R, and the event at R tells
    // neither.
    let mixed: Vec<i64> = (0..count)
        .map(|n| match n {
            0 => 1_000,
            n if n == bound + 10 => 2_000,
            n if n >= bound => 300,
            _ => 500,
        })
        .collect();
    let falling: Vec<i64> = (0..count)
        .map(|n| 5_000 - i64::try_from(n).unwrap())
        .collect();
    for (order, times) in [("mixed", mixed), ("falling", falling)] {
        let log = tasks_at(&times);
        let memory = Memory::of(&log);
        let mut first = Builder::default();
        pass(&memory, &mut first, 6_000);
        let r = r_of(&memory);
        assert_eq!(r.events, log.events[bound - 1].id, "{order}");
        let at_r = read_ms_at(&log, r.events);
        assert_eq!(read_ms_of(&memory), Some(at_r), "{order}");
        if order == "mixed" {
            assert_eq!(at_r, ms(1_000));
            assert_eq!(first.folds_at(i64::MIN), ms(2_000), "P's time");
            assert_eq!(times[bound - 1], 500, "the time of the event at R");
        }

        // The build of every event on a copy of the store, by the same passes.
        let whole = Memory::of(&log);
        whole.rows.replace(memory.rows.borrow().clone());
        let mut from_start = Builder::default();
        // Exercise a stored single R explicitly. Split certificates replay
        // core during startup when the queue certificate is already at P.
        {
            let mut held = memory.rows_mark.borrow_mut();
            let mark = held.as_mut().unwrap();
            mark.core = None;
            mark.queue = None;
        }
        let mut started = start_builder(&memory, ms(50)).unwrap();
        assert_eq!(started.cursors(), r, "{order}");
        assert_eq!(started.folds_at(ms(50)), at_r, "{order}");
        // (R,P] read again a few at a time, at a time before all of it.
        for upto in (bound + 5..=count).step_by(5) {
            for (store, builder) in [(&memory, &mut started), (&whole, &mut from_start)] {
                store.events.replace(log.events[..upto].to_vec());
                let built =
                    build_pass_reading(store, builder, (ms(50), BUILT), Retain::default(), None, 2);
                assert_eq!(built.failure, None, "{order} {upto}");
            }
            assert_eq!(
                started.folds_at(ms(50)),
                read_ms_at(&log, id_at(&log, upto)),
                "{order} {upto}"
            );
            // What it wrote holds the time at the position it wrote.
            let held = memory.rows_mark.borrow().unwrap();
            assert_eq!(
                held.read_ms,
                Some(read_ms_at(&log, held.through.events)),
                "{order} {upto}"
            );
        }
        drain(&memory, &mut started, 6_000);
        drain(&whole, &mut from_start, 6_000);
        assert_eq!(
            differing(&memory.rows.borrow(), &whole.rows.borrow()),
            Vec::new(),
            "{order}"
        );
        assert_eq!(read_ms_of(&memory), Some(read_ms_at(&log, log.last())));
    }
}

/// The moves of a wait of each case, after a log that ends with an event
/// timed before the latest: a move read after R, timed before the latest
/// up to R though after the event at R, is out of its order.
fn out_of_order_cases() -> Vec<(&'static str, Log, &'static str, i64)> {
    let ready_past = || {
        let mut log = Log::default();
        log.add(Some(8), None, "task_created", json!({"status": "draft"}), 0)
            .add(
                Some(8),
                None,
                "task_status_changed",
                json!({"from": "draft", "to": "ready"}),
                5,
            )
            .add(
                Some(8),
                None,
                "task_status_changed",
                json!({"from": "ready", "to": "draft"}),
                30,
            );
        log
    };
    let mut started = ready_past();
    started.add(Some(9), None, "task_created", json!({}), 10);
    let mut to_end = ready_past();
    to_end
        .add(
            None,
            None,
            "slots_full_started",
            json!({"supervisor": "s", "class": "heavy"}),
            35,
        )
        .add(Some(9), None, "task_created", json!({}), 10);
    vec![
        ("a start", started, "slots_full_started", 20),
        ("an end", to_end, "slots_full_ended", 20),
    ]
}

/// A task ready past with no wait on it: after R comes a move of a wait
/// timed before the latest event up to R and after the event at R. A started
/// builder takes it out of its order and marks the task as the build of
/// every event does, and writes what that build writes.
#[test]
fn a_started_builder_tells_a_move_of_a_wait_out_of_its_order_as_the_build_of_every_event() {
    for (name, mut log, kind, secs) in out_of_order_cases() {
        let upto = log.events.len();
        let memory = drained_store(&log, &[], upto, 40);
        let whole = Memory::of(&log);
        whole.rows.replace(memory.rows.borrow().clone());
        let mut from_start = Builder::default();
        drain(&whole, &mut from_start, 40);
        let mut started = start_builder(&memory, ms(40)).unwrap();
        // The rows not settled looked at, the task tracked.
        drain(&memory, &mut started, 40);
        assert!(!started.marked(), "{name}");

        log.add(
            None,
            None,
            kind,
            json!({"supervisor": "s", "class": "heavy"}),
            secs,
        );
        let new = log.events[upto..].to_vec();
        let (mut marks_of_start, mut marks_of_whole) = (started.clone(), from_start.clone());
        marks_of_start.mark_events(&new);
        marks_of_whole.mark_events(&new);
        let (marked, expected) = (
            marks_of_start.take(usize::MAX),
            marks_of_whole.take(usize::MAX),
        );
        assert!(
            expected.contains(&Target::Task(TaskId::new(8))),
            "{name} {expected:?}"
        );
        assert_eq!(marked, expected, "{name}");

        memory.events.replace(log.events.clone());
        whole.events.replace(log.events.clone());
        drain(&memory, &mut started, 45);
        drain(&whole, &mut from_start, 45);
        assert_eq!(
            differing(&memory.rows.borrow(), &whole.rows.borrow()),
            Vec::new(),
            "{name}"
        );
    }
}

/// An R that holds no time, as one written before it did, and an R whose
/// events W cuts: the whole start is from the cursors' start, whose pass
/// writes an R with the time, from which the next start goes on with it.
#[test]
fn an_r_with_no_time_at_its_position_starts_from_the_cursors_start_and_the_next_goes_on() {
    let log = tasks_at(&[200, 100, 150]);
    let no_time = |memory: &Memory| {
        let rows = memory.rows_mark.borrow().unwrap();
        memory.rows_mark.replace(Some(RowsWritten {
            read_ms: None,
            ..rows
        }));
    };
    let cut_by_w = |memory: &Memory| {
        memory
            .mark
            .replace(Some(SessionsWritten::at(log.events[0].id)));
    };
    for (name, change) in [
        ("no time", &no_time as &dyn Fn(&Memory)),
        ("cut by W", &cut_by_w),
    ] {
        let memory = drained_store(&log, &[], log.events.len(), 300);
        change(&memory);
        assert!(rows_written(&memory).unwrap().is_some(), "{name}");
        let mut builder = start_builder(&memory, ms(300)).unwrap();
        assert_eq!(builder.cursors(), Cursors::default(), "{name}");
        assert_eq!(builder.folds_at(ms(50)), ms(50), "{name}");
        memory.scans.borrow_mut().clear();
        drain(&memory, &mut builder, 300);
        assert_eq!(
            memory.scans.borrow()[0],
            ("events", 0, log.events.len()),
            "{name}"
        );
        assert_eq!(read_ms_of(&memory), Some(ms(200)), "{name}");

        memory.scans.borrow_mut().clear();
        let next = start_builder(&memory, ms(300)).unwrap();
        assert_eq!(next.cursors(), r_of(&memory), "{name}");
        assert_eq!(next.events, log.last(), "{name}");
        assert_eq!(next.folds_at(ms(50)), ms(200), "{name}");
        assert!(!scans_of(&memory).contains_key("events"), "{name}");
    }
}

/// Getting the time back reads no event: a start's reads of the events are
/// as many however many runs settled before R, and it holds the time of the
/// build of every event all the same.
#[test]
fn the_time_of_a_start_comes_from_the_mark_however_many_rows_settled() {
    let reads = |settled: usize| {
        let memory = Memory::default();
        let log = settled_and_open(settled, &memory);
        memory.events.replace(log.events.clone());
        drain(&memory, &mut Builder::default(), LATER);
        let mark = memory.rows_mark.borrow().unwrap();
        assert_eq!(mark.through.events, log.last(), "{settled}");
        memory.forget_reads();
        memory.read.set(0);
        memory.read_all.set(0);
        memory.scans.borrow_mut().clear();
        let started = start_builder(&memory, ms(0)).unwrap();
        assert_eq!(
            started.folds_at(i64::MIN),
            read_ms_at(&log, log.last()),
            "{settled}"
        );
        (
            memory.read.get(),
            memory.read_all.get(),
            memory.read_one.borrow().len(),
            memory.scans.borrow().len(),
        )
    };
    let few = reads(2);
    assert_eq!(few, reads(40));
    assert_eq!(few.3, 0, "no scan of the events");
}

/// A run in its grace at P, on a store with R and no input after it: a new
/// builder's first pass looks at its row and writes nothing, and no pass
/// marks it again before the grace is over; the pass at the end marks it
/// and writes what a build from the cursors' start on the same inputs does.
#[test]
fn a_run_in_its_grace_at_p_settles_at_its_end_after_a_start_from_r() {
    let run = run_id(1);
    let mut log = Log::default();
    log.landed(1, &run, 0);
    let upto = log.events.len();
    let stopped_at = last_secs(&log, upto) + 1;
    let memory = drained_store(&log, &[], upto, stopped_at);
    let held = memory.row(LedgerKind::Run, &run);
    assert!(!held.is_final);
    let grace_over = held.ended_at.unwrap() + SETTLE_GRACE_MS;
    let over_secs = (grace_over - ms(0)).div_euclid(1000) + 1;
    let expected = from_the_start_on(&memory, &log, over_secs);
    assert!(expected[&(LedgerKind::Run, run.clone())].is_final);

    let mut started = start_builder(&memory, ms(stopped_at)).unwrap();
    memory.written_keys.borrow_mut().clear();
    let built = pass(&memory, &mut started, stopped_at);
    assert_eq!((built.backlog, built.failure), (0, None));
    assert_eq!(memory.written_keys.borrow().as_slice(), &[]);
    assert!(!started.marked());
    // Just before its end (a queue row's grace is shorter).
    let before = over_secs - 2;
    let mut due = started.clone();
    due.due(ms(before));
    assert!(!due.take(usize::MAX).contains(&Target::Run(run.clone())));
    let built = pass(&memory, &mut started, before);
    assert_eq!((built.backlog, built.failure), (0, None));
    assert!(
        !memory
            .written_keys
            .borrow()
            .contains(&(LedgerKind::Run, run.clone()))
    );
    assert!(!memory.row(LedgerKind::Run, &run).is_final);

    let mut due = started.clone();
    due.due(ms(over_secs));
    assert!(due.take(usize::MAX).contains(&Target::Run(run.clone())));
    drain(&memory, &mut started, over_secs);
    assert!(
        memory
            .written_keys
            .borrow()
            .contains(&(LedgerKind::Run, run.clone()))
    );
    assert!(memory.row(LedgerKind::Run, &run).is_final);
    assert_eq!(differing(&memory.rows.borrow(), &expected), Vec::new());
}

/// A task ready at P, not settled, on a store with R: a move of a wait after
/// R marks it, read with the start's first pass or after it, and its row
/// is that of a build of every event.
#[test]
fn a_ready_tasks_row_at_p_follows_a_wait_after_a_start_from_r() {
    let mut log = Log::default();
    log.add(Some(8), None, "task_created", json!({"status": "draft"}), 0)
        .add(
            Some(8),
            None,
            "task_status_changed",
            json!({"from": "draft", "to": "ready"}),
            5,
        );
    let upto = log.events.len();
    log.add(
        None,
        None,
        "slots_full_started",
        json!({"supervisor": "s", "class": "heavy"}),
        20,
    );

    // With the first pass.
    let memory = drained_store(&log, &[], upto, 10);
    let mut started = start_builder(&memory, ms(25)).unwrap();
    memory.events.replace(log.events.clone());
    drain(&memory, &mut started, 25);
    assert_eq!(memory.row(LedgerKind::Task, "8"), whole(&log, 8, 25));

    // After it: only the move marks it.
    let memory = drained_store(&log, &[], upto, 10);
    let mut started = start_builder(&memory, ms(15)).unwrap();
    drain(&memory, &mut started, 15);
    assert!(!started.marked());
    let mut marked = started.clone();
    marked.mark_events(&log.events[upto..]);
    assert!(
        marked
            .take(usize::MAX)
            .contains(&Target::Task(TaskId::new(8)))
    );
    memory.events.replace(log.events.clone());
    drain(&memory, &mut started, 25);
    assert_eq!(memory.row(LedgerKind::Task, "8"), whole(&log, 8, 25));
    assert_eq!(
        memory.row(LedgerKind::Task, "8").detail["overlays"][0]["start"],
        json!(ms(20))
    );
}
