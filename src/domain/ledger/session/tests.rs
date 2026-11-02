use super::*;
use crate::domain::ledger::testkit::sessions_across as across;

const BUILT: &str = "2030-01-01T00:00:00.000Z";

/// The book of `events` read one at a time.
fn book_of(events: &[RunEvent]) -> SessionBook {
    let mut book = SessionBook::default();
    for event in events {
        book.add(event, &mut BTreeSet::new());
    }
    book
}

/// The book made again from the rows the store holds of `events`, as the
/// store gives their heads and anchors.
fn book_from_rows(events: &[RunEvent]) -> SessionBook {
    let held = session_rows(events, BUILT);
    let heads: Vec<LedgerHead> = held.iter().map(LedgerHead::of).collect();
    SessionBook::from_heads(&heads, &anchors(&held))
}

/// The book made again from the rows the store holds of the events up to a
/// position is the book of those events, and going on from it with the
/// rest gives the same rows to fold, and the same rows, as the fold of
/// every event: the planner's next session is its round 2, a late record
/// of its closed session goes to that session's row, the records of none
/// to the one row of round 0, and the first job takes its model from the
/// close before the position, the second from the close after it.
#[test]
fn a_book_made_again_from_the_rows_goes_on_as_the_fold_of_every_event() {
    let (log, position) = across();
    let (before, after) = log.events.split_at(position);
    let mut rebuilt = book_from_rows(before);
    let mut whole = book_of(before);
    assert_eq!(rebuilt, whole);
    for event in after {
        let (mut by_rows, mut by_all) = (BTreeSet::new(), BTreeSet::new());
        rebuilt.add(event, &mut by_rows);
        whole.add(event, &mut by_all);
        assert_eq!(by_rows, by_all, "{:?}", event.kind);
    }
    let rows: Vec<LedgerRow> = rebuilt
        .keys()
        .filter_map(|key| rebuilt.row(key, &log.events, BUILT))
        .collect();
    assert_eq!(rows, session_rows(&log.events, BUILT));

    let row = |key: &str| {
        rows.iter()
            .find(|row| row.key == key)
            .unwrap_or_else(|| panic!("no {key}"))
    };
    let ids = |row: &LedgerRow| -> Vec<i64> {
        row.records.iter().map(|event| event.id.as_i64()).collect()
    };
    assert_eq!(ids(row("runtime_planner/1/session/1")), [1, 2, 8]);
    assert_eq!(ids(row("runtime_planner/1/session/2")), [9, 10]);
    assert_eq!(ids(row("session//events/0")), [3, 11]);
    let first_job = row("plan_review/9/job/1");
    assert_eq!(
        first_job.subject.as_ref().unwrap().model.as_deref(),
        Some("review-model")
    );
    assert_eq!(first_job.inputs_through.events.as_i64(), 12);
    let second_job = row("plan_review/10/job/1");
    assert_eq!(
        second_job.subject.as_ref().unwrap().model.as_deref(),
        Some("later-model")
    );
    assert_eq!(second_job.inputs_through.events.as_i64(), 13);
    assert_eq!(ids(row("plan_review/9/job/2")), [14]);
}

/// A book of only the events after the position gets each case wrong: the
/// planner's next session takes round 1, the key of the row the store
/// holds settled, and the late record goes to the records of none.
#[test]
fn a_book_of_the_events_after_the_position_alone_collides_with_the_rows_held() {
    let (log, position) = across();
    let after = book_of(&log.events[position..]);
    assert_eq!(
        after.records_of("runtime_planner/1/session/1"),
        Some([EventId::new(9), EventId::new(10)].as_slice())
    );
    assert_eq!(
        after.records_of("session//events/0"),
        Some([EventId::new(8), EventId::new(11)].as_slice())
    );
}

/// A key of another shape and a row of another kind are no rows of the
/// book; a throughput review's id keeps its slashes.
#[test]
fn the_book_takes_only_the_session_rows_of_its_keys() {
    let head = |kind: LedgerKind, key: &str, records: &[i64]| LedgerHead {
        kind,
        key: key.to_owned(),
        inputs_from: EventId::new(0),
        inputs_through: crate::domain::ledger::InputsThrough {
            events: EventId::new(0),
            supervisor: None,
            life_ended: Vec::new(),
            node_samples: None,
            steps: None,
            resources: None,
            inside_resources: None,
        },
        is_final: true,
        ledger_v: crate::domain::ledger::LEDGER_V,
        records: records.iter().copied().map(EventId::new).collect(),
    };
    let book = SessionBook::from_heads(
        &[
            head(
                LedgerKind::Session,
                "throughput_review/daily/2030-01-01/d/job/2",
                &[5],
            ),
            head(LedgerKind::Session, "inbox//turns/1", &[6]),
            head(LedgerKind::Session, "inbox//session/x", &[7]),
            head(LedgerKind::Queue, "8", &[8]),
        ],
        &[],
    );
    assert_eq!(
        book.keys().collect::<Vec<_>>(),
        ["throughput_review/daily/2030-01-01/d/job/2"]
    );
    assert_eq!(
        book.rounds.get(&(
            "throughput_review".to_owned(),
            "daily/2030-01-01/d".to_owned(),
            "job"
        )),
        Some(&2)
    );
}

/// The session rows held as they were at an event before the last they
/// record give the book of the events up to it: the rows past it cut back
/// (a row with no record up to it is not one), a row settled with a record
/// after it not settled, and the closes after it give no model.
#[test]
fn the_rows_held_as_they_were_at_an_event_give_the_book_of_the_events_up_to_it() {
    let (log, position) = across();
    let mut rows = session_rows(&log.events, BUILT);
    for row in &mut rows {
        row.is_final = row.ended_at.is_some();
    }
    let heads: Vec<LedgerHead> = rows.iter().map(LedgerHead::of).collect();
    let through = log.events[position - 1].id;
    let held = SessionsHeld::at(&heads, &anchors(&rows), through);
    assert_eq!(held.book, book_of(&log.events[..position]));
    assert!(
        held.open
            .contains(&"runtime_planner/1/session/1".to_owned())
    );
    assert!(held.settled.contains(&"plan_review/9/session/1".to_owned()));
    assert!(
        !held
            .open
            .iter()
            .chain(&held.settled)
            .any(|key| key == "runtime_planner/1/session/2")
    );
    assert!(held.open_records().iter().all(|id| *id <= through));
    assert_eq!(held.open.len() + held.settled.len(), held.book.row_count());
}

/// A row the store holds settled by an older version is not settled for a
/// builder that goes on from the rows: it is open, so that it is folded
/// again by this version.
#[test]
fn a_row_settled_by_an_older_version_is_held_open() {
    let (log, position) = across();
    let mut rows = session_rows(&log.events, BUILT);
    for row in &mut rows {
        row.is_final = row.ended_at.is_some();
    }
    let key = "plan_review/9/session/1";
    let through = log.events[position - 1].id;
    let held_by = |rows: &[LedgerRow]| {
        let heads: Vec<LedgerHead> = rows.iter().map(LedgerHead::of).collect();
        SessionsHeld::at(&heads, &anchors(rows), through)
    };
    assert!(held_by(&rows).settled.contains(&key.to_owned()));
    let row = rows.iter_mut().find(|row| row.key == key).unwrap();
    row.ledger_v = LEDGER_V - 1;
    let held = held_by(&rows);
    assert!(held.open.contains(&key.to_owned()));
    assert!(!held.settled.contains(&key.to_owned()));
    assert_eq!(held.book, book_of(&log.events[..position]));
}

/// The rows held at a position no later than the store's mark W are the
/// fold of every session event up to it (W itself, or a position before
/// it such as R), held as [`SessionsHeld::at`] holds them; past W, or with
/// no W, they are not.
#[test]
fn the_rows_held_no_later_than_w_are_the_fold_of_every_event() {
    let (log, position) = across();
    let rows = session_rows(&log.events, BUILT);
    let heads: Vec<LedgerHead> = rows.iter().map(LedgerHead::of).collect();
    let anchors = anchors(&rows);
    let w = log.events[position].id;
    for (upto, whole) in [
        (position - 1, true),
        (position + 1, true),
        (position + 2, false),
    ] {
        let through = log.events[upto - 1].id;
        let held = SessionsHeld::within(&heads, &anchors, through, Some(w));
        assert_eq!(held.written, whole, "{through:?}");
        assert_eq!(held.book, book_of(&log.events[..upto]), "{through:?}");
        assert_eq!(
            held,
            SessionsHeld {
                written: whole,
                ..SessionsHeld::at(&heads, &anchors, through)
            }
        );
        assert!(!SessionsHeld::within(&heads, &anchors, through, None).written);
    }
}
