use super::*;
use crate::domain::ledger::testkit::{built_and_settled, ms, waits_across};
use crate::domain::supervisor_life::SILENT_AFTER_MS;

/// The books made again from the rows of the events up to the position of
/// [`waits_across`], folded at the pass there, and the books of those
/// events closed at that pass.
fn made_and_whole() -> ((Lives, PreClaimBook), (Lives, PreClaimBook), i64) {
    let (log, position, at) = waits_across();
    let events = &log.events[..position];
    let now = ms(at);
    let rows = built_and_settled(events, now);
    let since = Since::of(&rows);
    let held: Vec<LedgerRow> = rows.into_iter().filter(|row| holds(row, since)).collect();
    let stops: Vec<RunEvent> = events
        .iter()
        .filter(|event| event.kind == EventKind::SupervisorStopped)
        .cloned()
        .collect();
    let last: Vec<RunEvent> = living(&held, &stops)
        .iter()
        .filter_map(|id| events.iter().find(|event| event.id == *id).cloned())
        .collect();
    let made = books(&held, &stops, &last, now);
    let lives = Lives::new(events);
    let mut book = PreClaimBook::of(events);
    book.close_lives(&lives, now);
    (made, (lives, book), now)
}

/// The lives made again end, end from and read up to where the lives of
/// every event do for each supervisor a row reaches: those alive (their
/// last events read by id), one stopped and one silent. The book holds the waits
/// a task not settled reads, as the book of every event gives them at any
/// time from the pass on, and each open, closed by a life or by an event
/// as there; not the slots full freed before the task's start.
#[test]
fn the_books_made_again_from_the_rows_are_those_of_every_event() {
    let ((lives, book), (all_lives, all_book), now) = made_and_whole();
    assert_eq!(living(&[], &[]), Vec::<EventId>::new());
    for supervisor in ["a", "b", "c", "f"] {
        assert_eq!(lives.last_of(supervisor), all_lives.last_of(supervisor));
        assert_eq!(lives.ends_from(supervisor), all_lives.ends_from(supervisor));
        for at in [now, now + SILENT_AFTER_MS, now + 10 * SILENT_AFTER_MS] {
            assert_eq!(lives.end(supervisor, at), all_lives.end(supervisor, at));
        }
    }
    assert!(lives.last_of("d").is_none() && lives.last_of("e").is_none());
    let starts: Vec<EventId> = book.intervals(&lives, now).map(|wait| wait.id).collect();
    let all_starts: Vec<EventId> = all_book
        .intervals(&all_lives, now)
        .map(|wait| wait.id)
        .collect();
    assert_eq!(all_starts.first(), Some(&EventId::new(1)));
    assert_eq!(starts, all_starts[1..]);
    for at in [now, now + 10 * SILENT_AFTER_MS] {
        for start in &starts {
            assert_eq!(
                book.interval(*start, &lives, at),
                all_book.interval(*start, &all_lives, at),
                "{start}"
            );
            assert_eq!(book.ended_by_event(*start), all_book.ended_by_event(*start));
        }
    }
    let holds: Vec<_> = book.landing_holds().collect();
    assert_eq!(holds, all_book.landing_holds().collect::<Vec<_>>());
    assert_eq!(book.slots, all_book.slots);
    assert_eq!(book.claim_hold, all_book.claim_hold);
    assert_eq!(book.deferrals, all_book.deferrals);
    assert_eq!(book.landing_hold, all_book.landing_hold);
    assert_eq!(book.life_ends, all_book.life_ends);
    assert_eq!(book.life_end_of, all_book.life_end_of);
    assert_eq!(book.named, all_book.named);
    assert_eq!(book.open, all_book.open);
}

/// From where the waits ended bear: the earliest start and first event of
/// the task rows not settled, none without one; a wait ended before it is
/// not held once settled, one open or a life ended is, whenever it
/// started, and one not settled is.
#[test]
fn the_waits_held_are_those_open_a_life_ended_or_ended_since_a_task_not_settled() {
    let (log, position, at) = waits_across();
    let rows = built_and_settled(&log.events[..position], ms(at));
    let since = Since::of(&rows);
    assert_eq!(since.id, EventId::new(3));
    assert_eq!(since.ms, ms(100));
    assert_eq!(Since::of(&[]), Since::NONE);
    // Not settled, d's freed slots full (1) would be held whatever the
    // task rows: its grace folds it again.
    let queue_row = |rows: &[LedgerRow], key: &str| {
        rows.iter()
            .find(|row| row.kind == LedgerKind::Queue && row.key == key)
            .cloned()
            .unwrap()
    };
    let row_1 = queue_row(&rows, "1");
    assert!(row_1.is_final && !holds(&row_1, Since::NONE));
    let unsettled = LedgerRow {
        is_final: false,
        ..row_1
    };
    assert!(holds(&unsettled, Since::NONE));
    let rows: Vec<LedgerRow> = rows
        .into_iter()
        .map(|row| LedgerRow {
            is_final: true,
            ..row
        })
        .collect();
    let held: BTreeSet<&str> = rows
        .iter()
        .filter(|row| holds(row, since))
        .map(|row| row.key.as_str())
        .collect();
    // Settled: not d's slots full (1) freed before task 1; e's (5) over its
    // time ready;
    // b's (7) its stop closed, b's hold (8) its stop ended, c's (10) its
    // silence closed, and a's (13, 14, 15, 16) and f's (19) open.
    assert_eq!(
        held,
        BTreeSet::from(["5", "7", "8", "10", "13", "14", "15", "16", "19"])
    );
    let none: Vec<&LedgerRow> = rows.iter().filter(|row| holds(row, Since::NONE)).collect();
    assert_eq!(none.len(), held.len() - 1);
    // An end by an event at or after the task's first holds it, whatever
    // its time.
    let row_5 = queue_row(&rows, "5");
    let by_id = |id| Since {
        ms: i64::MAX,
        id: EventId::new(id),
    };
    assert!(holds(&row_5, by_id(6)) && !holds(&row_5, by_id(7)));
    assert!(
        !rows
            .iter()
            .any(|row| row.kind == LedgerKind::Task && holds(row, since))
    );
}
