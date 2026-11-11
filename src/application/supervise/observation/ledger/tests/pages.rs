//! A new builder's first pass reads the events a page at a time and
//! sweeps each page's session and queue rows as it reads it: whatever
//! the size of a page, the passes write the rows a read of every event at
//! once writes, over an empty store and over one that holds the rows of
//! an earlier supervisor settled, later events reaching some of them.

use super::*;

/// Passes at `secs` on `builder`, each reading the events `per_read` at a
/// time, until one leaves no mark.
fn catch_up(memory: &Memory, builder: &mut Builder, secs: i64, per_read: usize) {
    for _ in 0..DRAIN_PASSES {
        let built = build_pass_reading(
            memory,
            builder,
            (ms(secs), BUILT),
            Retain::default(),
            None,
            per_read,
        );
        assert_eq!(built.failure, None);
        if built.backlog == 0 {
            return;
        }
    }
    panic!("marks left after {DRAIN_PASSES} passes at {secs}");
}

/// The sizes of a page the passes read: one event, a few, and every one
/// at once.
const PAGES: [usize; 5] = [1, 2, 3, 7, usize::MAX];

/// [`in_time_order`], and then rows left open across every later page: a
/// deferral, a hold on the claims, a slots full, a stream and a
/// supervisor's life of `s2`, and a session that closes; with
/// `later`, the events past `LATER` that end them and that reach rows
/// settled by then: a late turn of a closed session, an event of the
/// supervisor whose stop ended its stream and its hold.
fn crossing(later: bool) -> Log {
    let mut log = in_time_order();
    log.add(
        Some(6),
        None,
        "claim_deferred",
        json!({"supervisor": "s2", "reason": "paths"}),
        3400,
    )
    .add(
        None,
        None,
        "live_alert_baseline",
        json!({"supervisor": "s2", "version": 3}),
        3410,
    )
    .add(
        None,
        None,
        "slots_full_started",
        json!({"supervisor": "s2", "class": "light"}),
        3420,
    )
    .add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": 9, "supervisor": "s2"}),
        3430,
    );
    let opened = log.last().as_i64();
    log.add(
        None,
        None,
        "session_closed",
        json!({"kind": "runtime_planner", "planner_id": 9, "opened_event_id": opened}),
        3440,
    )
    .add(None, None, "claim_held", json!({"supervisor": "s2"}), 3450)
    .add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s2"}),
        3460,
    );
    if !later {
        return log;
    }
    log.add(
        None,
        None,
        "session_turns",
        json!({"kind": "runtime_planner", "planner_id": 5, "opened_event_id": 7}),
        LATER + 10,
    )
    .add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s1"}),
        LATER + 20,
    )
    .add(
        None,
        None,
        "live_alert_input_changed",
        json!({"supervisor": "s2", "version": 4}),
        LATER + 30,
    )
    .add(
        None,
        None,
        "slots_full_ended",
        json!({"supervisor": "s2", "class": "light"}),
        LATER + 40,
    )
    .add(
        Some(6),
        None,
        "claim_deferral_ended",
        json!({"supervisor": "s2", "why": "free"}),
        LATER + 50,
    )
    .add(
        None,
        None,
        "claim_resumed",
        json!({"supervisor": "s2"}),
        LATER + 60,
    )
    .landed(7, &run_id(7), LATER + 70)
    .add(None, None, "mark_added", json!({"mark": "n"}), LATER + 80)
    .add(
        None,
        None,
        "supervisor_stopped",
        json!({"supervisor": "s2"}),
        LATER + 90,
    );
    log
}

/// The session and queue rows of the ledger's fold of every event of
/// `log` at `secs`, by kind and key.
fn whole_rows(log: &Log, secs: i64) -> BTreeMap<(LedgerKind, String), LedgerRow> {
    crate::domain::ledger::build(&log.events, ms(secs), BUILT)
        .into_iter()
        .filter(|row| matches!(row.kind, LedgerKind::Session | LedgerKind::Queue))
        .map(|row| ((row.kind, row.key.clone()), row))
        .collect()
}

/// A new builder over an empty store, reading the events a page at a time
/// (rows open across the page's edges among them), writes the rows one
/// that reads every event at once writes: the run, task, session and
/// queue rows, their `inputs_through`, `final` and `ledger_v`.
#[test]
fn a_new_builder_reading_a_page_at_a_time_writes_the_rows_of_a_read_of_every_event() {
    for log in [in_time_order(), every_kind_of_row().0, crossing(true)] {
        let mut rows = Vec::new();
        for per_read in PAGES {
            let memory = Memory::of(&log);
            let mut builder = Builder::default();
            catch_up(&memory, &mut builder, LATER, per_read);
            rows.push((per_read, memory.rows.borrow().clone()));
        }
        let (_, every) = rows.pop().unwrap();
        assert!(every.keys().any(|(kind, _)| *kind == LedgerKind::Queue));
        for (per_read, paged) in rows {
            assert_eq!(paged, every, "{per_read} a page");
        }
    }
}

/// A supervisor started again over a store that holds the rows an earlier
/// one wrote and settled, events having come in since: some end the rows
/// left open across the pages (a deferral, a hold, a slots full, a
/// stream), some reach rows settled (a late turn of a closed session, an
/// event of a supervisor whose stop ended its stream and its hold). Its
/// builder, reading a page at a time and sweeping each page, drops the
/// records of the rows settled and gives them back to those a later page
/// reaches, and writes the rows one that reads every event at once
/// writes; their records, ends and details are those of the fold of every
/// event.
#[test]
fn a_restarted_builder_reading_a_page_at_a_time_writes_the_rows_of_a_read_of_every_event() {
    let log = crossing(true);
    let restarted = |per_read: usize| {
        let memory = Memory::of(&crossing(false));
        let mut before = Builder::default();
        catch_up(&memory, &mut before, LATER, usize::MAX);
        assert!(memory.rows.borrow().values().any(|row| row.is_final));
        memory.events.replace(log.events.clone());
        let mut builder = Builder::default();
        catch_up(&memory, &mut builder, 2 * LATER, per_read);
        // The next pass's due drops what the last one found settled.
        let kept = pass(&memory, &mut builder, 2 * LATER).kept;
        (memory.rows.borrow().clone(), kept)
    };
    let (every, every_kept) = restarted(usize::MAX);
    let whole = whole_rows(&log, 2 * LATER);
    for (key, row) in whole {
        let held = &every[&key];
        assert_eq!(held.records, row.records, "{key:?}");
        assert_eq!(held.ended_at, row.ended_at, "{key:?}");
        assert_eq!(held.detail, row.detail, "{key:?}");
    }
    for per_read in &PAGES[..PAGES.len() - 1] {
        let (paged, kept) = restarted(*per_read);
        assert_eq!(paged, every, "{per_read} a page");
        assert_eq!(kept.events(), every_kept.events(), "{per_read} a page");
    }
}

/// While a new builder's first pass reads a page at a time, what it keeps
/// of the rows the store holds settled goes with the page, not with every
/// event read: after one more page of settled rows than another, it keeps
/// as many events and as many queue rows assigned.
#[test]
fn a_page_swept_keeps_no_event_or_queue_row_of_the_rows_the_store_holds_settled() {
    let kept_after_restart = |settled: i64| {
        let log = settled_sessions(settled);
        let memory = Memory::of(&log);
        let mut before = Builder::default();
        catch_up(&memory, &mut before, LATER, usize::MAX);
        let mut builder = Builder::default();
        let first = build_pass_reading(
            &memory,
            &mut builder,
            (ms(LATER), BUILT),
            Retain::default(),
            None,
            3,
        );
        assert_eq!(first.failure, None);
        first.kept
    };
    let (few, many) = (kept_after_restart(3), kept_after_restart(300));
    assert_eq!(few.events(), many.events());
    assert_eq!(few.queue_rows, many.queue_rows);
    assert!(many.dropped_rows > few.dropped_rows);
}

/// A read of the store that fails while a page is swept stops the pass
/// and leaves every mark: once the store reads again, the passes write the
/// rows a read of every event at once writes.
#[test]
fn a_page_swept_whose_read_fails_leaves_its_rows_marked() {
    let log = crossing(true);
    let every = {
        let memory = Memory::of(&log);
        catch_up(&memory, &mut Builder::default(), LATER, usize::MAX);
        memory.rows.borrow().clone()
    };
    let queue: Vec<String> = every
        .keys()
        .filter(|(kind, _)| *kind == LedgerKind::Queue)
        .map(|(_, key)| key.clone())
        .collect();
    for key in queue {
        let memory = Memory::of(&log);
        memory
            .broken
            .replace(Some((LedgerKind::Queue, key.clone())));
        let mut builder = Builder::default();
        let failed = build_pass_reading(
            &memory,
            &mut builder,
            (ms(LATER), BUILT),
            Retain::default(),
            None,
            3,
        );
        assert!(failed.failure.is_some(), "{key}");
        memory.broken.replace(None);
        catch_up(&memory, &mut builder, LATER, 3);
        assert_eq!(*memory.rows.borrow(), every, "{key}");
    }
}

/// Old and unsettled rows already contain these inputs. Restarting after
/// each bounded pass must not replay their history forever.
#[test]
fn covered_marks_do_not_pin_restarts_past_the_check_bound() {
    let mut log = Log::default();
    for _ in 0..CHECKS_PER_PASS + 17 {
        log.add(None, None, ONE_EVENT, json!({}), 1);
    }
    let memory = Memory::of(&log);
    let rows = crate::domain::ledger::build(&log.events, ms(2), BUILT);
    for mut row in rows {
        row.ledger_v = LEDGER_V - 1;
        row.is_final = false;
        memory
            .rows
            .borrow_mut()
            .insert((row.kind, row.key.clone()), row);
    }
    // Inputs absent from the store must still hold R until written.
    for _ in 0..37 {
        log.add(None, None, ONE_EVENT, json!({}), 1);
    }
    memory.events.replace(log.events.clone());
    let mut previous = EventId::new(0);
    for _ in 0..20 {
        let mut builder = start_builder(&memory, ms(2)).unwrap();
        let built = build_pass(
            &memory,
            &mut builder,
            (ms(2), BUILT),
            Retain::default(),
            None,
        );
        assert_eq!(built.failure, None);
        let r = memory.rows_mark.borrow().unwrap().through.events;
        assert!(r >= previous);
        previous = r;
        if r == log.last() {
            break;
        }
    }
    assert_eq!(previous, log.last());
    memory.scans.borrow_mut().clear();
    let mut builder = start_builder(&memory, ms(2)).unwrap();
    let built = build_pass(
        &memory,
        &mut builder,
        (ms(2), BUILT),
        Retain::default(),
        None,
    );
    assert_eq!(built.failure, None);
    assert_eq!(built.work.events, 0);
    // Each restart can finish a share of the old rows and settle them.
    // The same full fold must result regardless of where processes ended.
    let inputs = Inputs::new(&log.events, ms(LATER), BUILT, true);
    let expected: BTreeMap<_, _> = inputs
        .rows
        .keys()
        .map(|(kind, key)| {
            let target = Target::Row(*kind, key.clone());
            ((*kind, key.clone()), build(&target, &inputs, &[]).unwrap())
        })
        .collect();
    for _ in 0..30 {
        let mut builder = start_builder(&memory, ms(LATER)).unwrap();
        let built = build_pass(
            &memory,
            &mut builder,
            (ms(LATER), BUILT),
            Retain::default(),
            None,
        );
        assert_eq!(built.failure, None);
        if *memory.rows.borrow() == expected {
            return;
        }
    }
    assert_eq!(
        memory.rows.borrow().keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>()
    );
    for (key, row) in expected {
        assert_eq!(memory.row(key.0, &key.1), row, "{key:?}");
    }
}

/// A failure after a page checkpoint discards the builder and its pass
/// result, but not R. Startup scans only the tail, then new input only.
#[test]
fn interrupted_catch_up_resumes_after_the_committed_page() {
    let mut log = Log::default();
    for _ in 0..EVENTS_PER_READ + 3 {
        log.add(None, None, ONE_EVENT, json!({}), 1);
    }
    let memory = Memory::of(&log);
    for mut row in crate::domain::ledger::build(&log.events[..EVENTS_PER_READ], ms(LATER), BUILT) {
        row.is_final = true;
        memory
            .rows
            .borrow_mut()
            .insert((row.kind, row.key.clone()), row);
    }
    let checkpoint = log.events[EVENTS_PER_READ - 1].id;
    memory.broken.replace(Some((
        LedgerKind::Queue,
        log.events[EVENTS_PER_READ].id.to_string(),
    )));
    let mut builder = start_builder(&memory, ms(LATER)).unwrap();
    let discarded = pass(&memory, &mut builder, LATER);
    assert!(discarded.failure.unwrap().contains("does not parse"));
    assert_eq!(discarded.marks.cursors, builder.cursors());
    assert!(discarded.marks.oldest.is_some());
    assert_eq!(
        memory.rows_mark.borrow().unwrap().through.events,
        checkpoint
    );
    assert_eq!(memory.row_reads.get(), EVENTS_PER_READ + 1);
    drop(builder);
    memory.broken.replace(None);
    memory.scans.borrow_mut().clear();
    let mut builder = start_builder(&memory, ms(LATER)).unwrap();
    let built = pass(&memory, &mut builder, LATER);
    assert_eq!(built.failure, None);
    assert_eq!(built.work.events, 3);
    assert!(
        memory
            .scans
            .borrow()
            .iter()
            .filter(|(kind, _, _)| *kind == "events")
            .all(|(_, after, _)| *after >= checkpoint.as_i64())
    );
    // The same tail on an empty store measures the normal read/fold/CAS
    // calls, separately from the covered history's one read per row.
    let tail = Memory::of(&Log {
        events: log.events[EVENTS_PER_READ..].to_vec(),
    });
    let tail_built = pass(&tail, &mut Builder::default(), LATER);
    assert_eq!(tail_built.failure, None);
    assert!(memory.row_reads.get() <= EVENTS_PER_READ + 1 + tail.row_reads.get());
    let inputs = Inputs::new(&log.events, ms(LATER), BUILT, true);
    let expected: BTreeMap<_, _> = inputs
        .rows
        .keys()
        .map(|(kind, key)| {
            let target = Target::Row(*kind, key.clone());
            ((*kind, key.clone()), build(&target, &inputs, &[]).unwrap())
        })
        .collect();
    assert_eq!(
        memory.rows.borrow().keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>()
    );
    for (key, row) in expected {
        assert_eq!(memory.row(key.0, &key.1), row, "{key:?}");
    }
    log.add(None, None, ONE_EVENT, json!({}), 2);
    memory.events.replace(log.events.clone());
    let mut builder = start_builder(&memory, ms(LATER)).unwrap();
    let built = pass(&memory, &mut builder, LATER);
    assert_eq!(built.failure, None);
    assert_eq!(built.work.events, 1);
    assert_eq!(
        memory.rows_mark.borrow().unwrap().through.events,
        log.last()
    );
}

/// Restored, unsettled task rows ahead of dirty task marks must not win
/// the same first checks after every restart. This reads the stored mark,
/// through the production observer/session writer, rather than rows_mark.
/// A checkpoint followed by failure before the first check must preserve
/// the restored rank as well as the covered inputs.
#[test]
fn task_check_rank_is_written_and_rotates_through_one_pass_restarts() {
    let count = CHECKS_PER_PASS + 17;
    let mut log = Log::default();
    for id in 1..=count {
        log.add(
            Some(id as i64),
            None,
            "task_created",
            json!({"status": "draft"}),
            1,
        );
    }
    let inputs = Inputs::new(&log.events, ms(2), BUILT, true);
    let memory = Memory::of(&log);
    for id in 1..count {
        let row = build(&Target::Task(TaskId::new(id as i64)), &inputs, &[]).unwrap();
        memory
            .rows
            .borrow_mut()
            .insert((row.kind, row.key.clone()), row);
    }
    let mut builder = start_builder(&memory, ms(2)).unwrap();
    assert_eq!(pass(&memory, &mut builder, 2).failure, None);
    let first = memory.rows_mark.borrow().unwrap();
    assert_eq!(first.take_offset, CHECKS_PER_PASS);
    assert_eq!(first.through.events, log.events[CHECKS_PER_PASS - 1].id);
    drop(builder);

    // Covered queue rows let catch-up checkpoint before it takes any task
    // marks. Fail the next page and discard the builder and pass result.
    for _ in 0..EVENTS_PER_READ + 3 {
        log.add(None, None, ONE_EVENT, json!({}), 1);
    }
    memory.events.replace(log.events.clone());
    for mut row in crate::domain::ledger::build(&log.events[count..], ms(2), BUILT) {
        row.is_final = true;
        memory
            .rows
            .borrow_mut()
            .insert((row.kind, row.key.clone()), row);
    }
    memory.broken.replace(Some((
        LedgerKind::Queue,
        log.events
            [first.position(true).unwrap().through.events.as_i64() as usize + EVENTS_PER_READ]
            .id
            .to_string(),
    )));
    let mut builder = start_builder(&memory, ms(2)).unwrap();
    assert!(
        pass(&memory, &mut builder, 2)
            .failure
            .unwrap()
            .contains("does not parse")
    );
    let checkpoint = memory.rows_mark.borrow().unwrap();
    assert_eq!(checkpoint.take_offset, first.take_offset);
    assert_eq!(checkpoint.through.events, first.through.events);
    assert_eq!(builder.events, log.last());
    drop(builder);
    memory.broken.replace(None);

    let mut builder = start_builder(&memory, ms(2)).unwrap();
    let second = pass(&memory, &mut builder, 2);
    assert_eq!(second.failure, None);
    assert_eq!(
        second.work.events as i64,
        log.last().as_i64() - checkpoint.position(true).unwrap().through.events.as_i64()
    );
    let mark = memory.rows_mark.borrow().unwrap();
    assert_eq!(mark.take_offset, (CHECKS_PER_PASS * 2) % count);
    assert!(mark.through.events > first.through.events);
    assert_eq!(mark.through.events, log.last());
    for id in 1..=count {
        let row = build(&Target::Task(TaskId::new(id as i64)), &inputs, &[]).unwrap();
        assert_eq!(memory.row(row.kind, &row.key), row, "task {id}");
    }
    let mut builder = start_builder(&memory, ms(2)).unwrap();
    let caught_up = pass(&memory, &mut builder, 2);
    assert_eq!(caught_up.failure, None);
    assert_eq!(caught_up.work.events, 0);
}

/// A wall clock days ahead of the unread heartbeats is not evidence of
/// silence. A task whose own cancellation is in the first prefix must
/// stay unsettled until the catch-up reads the wait's later records.
#[test]
fn bounded_catch_up_uses_read_time_for_waits_tasks_and_settlement() {
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
            None,
            None,
            "slots_full_started",
            json!({"supervisor": "s", "class": "heavy"}),
            10,
        )
        .add(
            None,
            None,
            "live_alert_baseline",
            json!({"supervisor": "s", "version": 3}),
            15,
        )
        .add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "s"}),
            20,
        )
        .add(
            Some(9),
            None,
            "task_created",
            json!({"status": "draft"}),
            25,
        )
        .add(
            Some(11),
            Some(&run_id(11)),
            "push_failed",
            json!({"remote": "other", "branch": "main"}),
            27,
        )
        .phase(
            11,
            &run_id(11),
            28,
            crate::domain::run_phase::Phase::PushPending,
            "pending",
        )
        .add(
            Some(8),
            None,
            "task_status_changed",
            json!({"from": "ready", "to": "canceled"}),
            30,
        )
        .add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "s"}),
            200,
        )
        .add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "s"}),
            400,
        )
        .add(
            None,
            None,
            "slots_full_ended",
            json!({"supervisor": "s", "class": "heavy"}),
            500,
        )
        .add(
            None,
            None,
            "supervisor_stopped",
            json!({"supervisor": "s"}),
            510,
        )
        .landed(10, &run_id(10), 600);
    let wall = LATER * 10;
    let whole = Memory::of(&log);
    drain(&whole, &mut Builder::default(), wall);
    let expected = whole.rows.borrow().clone();
    assert!(expected[&(LedgerKind::Task, "8".to_owned())].is_final);
    for budget in [9, 10, 12] {
        for restart in [false, true] {
            let memory = Memory::of(&log);
            let mut builder = start_builder(&memory, ms(wall)).unwrap();
            let first = build_pass_limited(
                &memory,
                &mut builder,
                (ms(wall), BUILT),
                Retain::default(),
                None,
                (3, budget, SESSION_BYTES_PER_WRITE),
                Timing::default(),
            );
            assert_eq!(first.failure, None);
            assert_eq!(first.work.events, budget);
            assert!(builder.events < log.last());
            assert!(builder.sweeps_pages());
            assert_eq!(
                memory.rows_mark.borrow().unwrap().time_pending,
                Some(ms(wall))
            );
            assert!(!memory.row(LedgerKind::Task, "8").is_final);
            if budget == 9 {
                // The unread heartbeat at 200 is within the live window.
                let row = memory.row(LedgerKind::Queue, "3");
                assert!(row.ended_at.is_none());
            }
            for _ in 0..30 {
                if restart {
                    builder = start_builder(&memory, ms(wall)).unwrap();
                }
                let built = build_pass_limited(
                    &memory,
                    &mut builder,
                    (ms(wall), BUILT),
                    Retain::default(),
                    None,
                    (3, budget, SESSION_BYTES_PER_WRITE),
                    Timing::default(),
                );
                assert_eq!(built.failure, None);
                if builder.events == log.last() && built.backlog == 0 {
                    break;
                }
            }
            assert_eq!(
                memory.rows_mark.borrow().unwrap().through.events,
                log.last()
            );
            assert_eq!(
                memory.rows.borrow().keys().collect::<Vec<_>>(),
                expected.keys().collect::<Vec<_>>()
            );
            for (key, row) in &expected {
                assert_eq!(
                    memory.row(key.0, &key.1),
                    *row,
                    "budget {budget}, restart {restart}, {key:?}"
                );
            }
        }
    }
}

/// A failed final mark write must not erase the pending clock of a row
/// already written at a bounded prefix, even if the next restart can read
/// the entire remaining suffix in one pass.
#[test]
fn prefix_clock_work_survives_a_failed_final_mark_write() {
    let wall = LATER;
    let mut log = Log::default();
    log.add(Some(1), None, "task_created", json!({"status": "draft"}), 1)
        .add(Some(2), None, "task_created", json!({"status": "draft"}), 2)
        .add(
            None,
            None,
            "supervisor_alive",
            json!({"supervisor": "s"}),
            3,
        )
        .add(None, None, "mark_added", json!({"mark": "a"}), 4)
        .add(None, None, "mark_added", json!({"mark": "b"}), 5);
    let memory = Memory::of(&log);
    let inputs = Inputs::new(&log.events[..1], ms(wall), BUILT, true);
    let row = build(&Target::Task(TaskId::new(1)), &inputs, &[]).unwrap();
    memory
        .rows
        .borrow_mut()
        .insert((row.kind, row.key.clone()), row);
    memory
        .mark
        .replace(Some(SessionsWritten::at(log.events[0].id)));
    memory.rows_mark.replace(Some(RowsWritten::at(
        Cursors {
            events: log.events[0].id,
            ..Cursors::default()
        },
        ms(1),
    )));
    // Queue-row errors may be isolated and retried. Refuse the marks
    // throughout this pass so the final certificate really cannot commit.
    memory.refuses_batch_marks.set(true);
    let mut builder = start_builder(&memory, ms(wall)).unwrap();
    let first = build_pass_limited(
        &memory,
        &mut builder,
        (ms(wall), BUILT),
        Retain::default(),
        None,
        (3, 3, SESSION_BYTES_PER_WRITE),
        Timing::default(),
    );
    assert!(first.failure.unwrap().contains("transaction failed"));
    let saved = memory.rows_mark.borrow().unwrap();
    assert_eq!(saved.through.events, log.events[0].id);
    assert_eq!(saved.time_pending, Some(ms(wall)));
    assert!(
        memory
            .row(LedgerKind::Task, "2")
            .intervals
            .last()
            .unwrap()
            .end
            < ms(wall)
    );
    memory.refuses_batch_marks.set(false);
    let mut builder = start_builder(&memory, ms(wall)).unwrap();
    let recovered = pass(&memory, &mut builder, wall);
    assert_eq!(recovered.failure, None);
    assert_eq!(recovered.work.events, 4);
    let whole = Memory::of(&log);
    drain(&whole, &mut Builder::default(), wall);
    assert_eq!(*memory.rows.borrow(), *whole.rows.borrow());
    assert_eq!(
        memory.rows_mark.borrow().unwrap().through.events,
        log.last()
    );
}
