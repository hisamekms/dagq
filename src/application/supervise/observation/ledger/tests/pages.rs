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
