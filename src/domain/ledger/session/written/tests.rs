use super::*;
use crate::domain::ledger::session::session_rows;
use crate::domain::ledger::testkit::sessions_across;

const BUILT: &str = "2030-01-01T00:00:00.000Z";

/// The heads of the session rows of the events of [`sessions_across`].
fn heads() -> Vec<LedgerHead> {
    let (log, _) = sessions_across();
    session_rows(&log.events, BUILT)
        .iter()
        .map(LedgerHead::of)
        .collect()
}

/// With no mark there is no position, however far the session rows'
/// `inputs_through` go: a session row the store lacks has no head, so the
/// largest of them says nothing of what the rows record.
#[test]
fn a_store_with_no_mark_gives_no_position_whatever_its_rows_read() {
    let heads = heads();
    assert!(
        heads
            .iter()
            .any(|head| head.inputs_through.events > EventId::new(0))
    );
    assert_eq!(written_through(None, &heads), None);
    assert_eq!(written_through(None, &[]), None);
}

/// A mark by this version gives its position, whatever the rows read up
/// to (the rows written after the mark go past it, and are cut back to it
/// as a builder goes on from it).
#[test]
fn a_mark_by_this_version_gives_its_position() {
    let heads = heads();
    let mark = SessionsWritten::at(EventId::new(3));
    assert_eq!(mark.ledger_v, LEDGER_V);
    assert_eq!(written_through(Some(&mark), &heads), Some(EventId::new(3)));
    assert_eq!(written_through(Some(&mark), &[]), Some(EventId::new(3)));
}

/// A mark by another version, and a mark with a session row folded by a
/// later version than this one, give no position: the fold of every event
/// starts from the cursors' start.
#[test]
fn a_mark_by_another_version_or_a_row_of_a_later_one_gives_no_position() {
    let mut heads = heads();
    let older = SessionsWritten {
        through: EventId::new(3),
        ledger_v: LEDGER_V - 1,
    };
    assert_eq!(written_through(Some(&older), &heads), None);
    let later = SessionsWritten {
        ledger_v: LEDGER_V + 1,
        ..older
    };
    assert_eq!(written_through(Some(&later), &heads), None);
    let mark = SessionsWritten::at(EventId::new(3));
    heads[0].ledger_v = LEDGER_V + 1;
    assert_eq!(written_through(Some(&mark), &heads), None);
    // One of an older version is folded again as it is gone on from.
    heads[0].ledger_v = LEDGER_V - 1;
    assert_eq!(written_through(Some(&mark), &heads), Some(EventId::new(3)));
}

/// The mark reads back as it was written: its position and version.
#[test]
fn the_mark_reads_back_as_written() {
    let mark = SessionsWritten::at(EventId::new(42));
    let text = serde_json::to_string(&mark).unwrap();
    assert_eq!(
        serde_json::from_str::<SessionsWritten>(&text).unwrap(),
        mark
    );
}

/// R at `events` and the other cursors at 7, by this version.
fn rows_at(events: i64) -> RowsWritten {
    RowsWritten::at(Cursors {
        events: EventId::new(events),
        samples: 7,
        steps: 7,
        resources: 7,
        inside_resources: 7,
    })
}

/// A store with no mark, or with the session rows' mark alone, gives no
/// R; nor does an R by another version, or a session rows' mark that gives
/// no W. An R gives its positions, its events no later than W.
#[test]
fn r_is_given_only_with_a_w_and_an_r_of_this_version_and_no_later_than_w() {
    let heads = heads();
    assert_eq!(rows_written_through(None, &heads), None);
    let alone = Marks {
        sessions: SessionsWritten::at(EventId::new(5)),
        rows: None,
    };
    assert_eq!(rows_written_through(Some(&alone), &heads), None);
    let marks = Marks {
        rows: Some(rows_at(4)),
        ..alone
    };
    assert_eq!(
        rows_written_through(Some(&marks), &heads),
        Some(rows_at(4).through)
    );
    let older = Marks {
        rows: Some(RowsWritten {
            ledger_v: LEDGER_V - 1,
            ..rows_at(4)
        }),
        ..alone
    };
    assert_eq!(rows_written_through(Some(&older), &heads), None);
    let no_w = Marks {
        sessions: SessionsWritten {
            ledger_v: LEDGER_V + 1,
            ..alone.sessions
        },
        ..marks
    };
    assert_eq!(rows_written_through(Some(&no_w), &heads), None);
    // An R past W (written before a W set back) is cut back to W.
    let past = Marks {
        rows: Some(rows_at(9)),
        ..alone
    };
    let r = rows_written_through(Some(&past), &heads).unwrap();
    assert_eq!(r.events, EventId::new(5));
    assert_eq!(r.samples, 7);
}

/// The marks read as the session rows' mark alone, and that mark as marks
/// with no R: a reader of either version reads the other's.
#[test]
fn the_marks_and_the_session_rows_mark_read_as_each_other() {
    let marks = Marks {
        sessions: SessionsWritten::at(EventId::new(5)),
        rows: Some(rows_at(4)),
    };
    let text = serde_json::to_string(&marks).unwrap();
    assert_eq!(serde_json::from_str::<Marks>(&text).unwrap(), marks);
    assert_eq!(
        serde_json::from_str::<SessionsWritten>(&text).unwrap(),
        marks.sessions
    );
    let alone = serde_json::to_string(&marks.sessions).unwrap();
    assert_eq!(
        serde_json::from_str::<Marks>(&alone).unwrap(),
        Marks {
            rows: None,
            ..marks
        }
    );
}
