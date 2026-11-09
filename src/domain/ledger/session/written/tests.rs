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

/// The time read up to the R of [`rows_at`].
const READ_MS: i64 = 1_900_000_000_000;

/// R at `events` and the other cursors at 7, by this version.
fn rows_at(events: i64) -> RowsWritten {
    RowsWritten::at(
        Cursors {
            events: EventId::new(events),
            samples: 7,
            steps: 7,
            resources: 7,
            inside_resources: 7,
        },
        READ_MS,
    )
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
        omitted: None,
    };
    assert_eq!(rows_written_through(Some(&alone), &heads), None);
    let marks = Marks {
        rows: Some(rows_at(4)),
        ..alone.clone()
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
        ..alone.clone()
    };
    assert_eq!(rows_written_through(Some(&older), &heads), None);
    let no_w = Marks {
        sessions: SessionsWritten {
            ledger_v: LEDGER_V + 1,
            ..alone.sessions
        },
        ..marks.clone()
    };
    assert_eq!(rows_written_through(Some(&no_w), &heads), None);
    // An R past W (written before a W set back) is cut back to W.
    let past = Marks {
        rows: Some(rows_at(9)),
        ..alone.clone()
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
        omitted: None,
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
            ..marks.clone()
        }
    );
}

/// A builder goes on from an R whose mark holds the time read up to its
/// position: as the mark gives it, the lowest time (no event read) too. An
/// R that holds none, or whose events W cuts (the time it holds is of a
/// later position), gives a builder none, though it is R.
#[test]
fn a_builder_goes_on_only_from_an_r_that_holds_the_time_read_at_its_position() {
    let heads = heads();
    let alone = Marks {
        sessions: SessionsWritten::at(EventId::new(5)),
        rows: None,
        omitted: None,
    };
    assert_eq!(rows_to_go_on_from(None, &heads), None);
    assert_eq!(rows_to_go_on_from(Some(&alone), &heads), None);
    let held = Marks {
        rows: Some(rows_at(4)),
        ..alone.clone()
    };
    assert_eq!(
        rows_to_go_on_from(Some(&held), &heads),
        Some((rows_at(4).through, READ_MS))
    );
    let at_w = Marks {
        rows: Some(rows_at(5)),
        ..alone.clone()
    };
    assert_eq!(
        rows_to_go_on_from(Some(&at_w), &heads),
        Some((rows_at(5).through, READ_MS))
    );
    let none_read = Marks {
        rows: Some(RowsWritten::at(rows_at(4).through, i64::MIN)),
        ..alone.clone()
    };
    assert_eq!(
        rows_to_go_on_from(Some(&none_read), &heads),
        Some((rows_at(4).through, i64::MIN))
    );
    let no_time = Marks {
        rows: Some(RowsWritten {
            read_ms: None,
            ..rows_at(4)
        }),
        ..alone.clone()
    };
    assert!(rows_written_through(Some(&no_time), &heads).is_some());
    assert_eq!(rows_to_go_on_from(Some(&no_time), &heads), None);
    let cut = Marks {
        rows: Some(rows_at(9)),
        ..alone.clone()
    };
    assert!(rows_written_through(Some(&cut), &heads).is_some());
    assert_eq!(rows_to_go_on_from(Some(&cut), &heads), None);
}

/// Clock work changes neither input certificates nor check rotation; a
/// usable certificate's existing horizon can only advance.
#[test]
fn clock_work_keeps_a_usable_certificate_and_never_retreats() {
    let marks = Marks {
        sessions: SessionsWritten::at(EventId::new(5)),
        rows: Some(RowsWritten {
            take_offset: 1700,
            time_pending: Some(READ_MS + 100),
            ..rows_at(4)
        }),
        omitted: None,
    };
    let mut heads = heads();
    // Older session rows are also compatible with the current mark.
    heads[0].ledger_v = LEDGER_V - 1;
    for (until, pending) in [
        (READ_MS + 50, READ_MS + 100),
        (READ_MS + 200, READ_MS + 200),
    ] {
        let held = marks_with_time_work(Some(&marks), &heads, until);
        assert_eq!(
            held,
            Marks {
                rows: Some(RowsWritten {
                    time_pending: Some(pending),
                    ..marks.rows.unwrap()
                }),
                ..marks.clone()
            }
        );
        assert_eq!(
            rows_to_go_on_from(Some(&held), &heads),
            Some((rows_at(4).through, READ_MS))
        );
        assert_eq!(marks_with_time_work(Some(&held), &heads, until), held);
    }
}

/// Without an exact, usable R, preserving clock work must not turn W
/// alone, a different version or a mismatched event time into a certified
/// input prefix. The zero fallback is resumable even after a failed pass.
#[test]
fn clock_work_without_a_usable_certificate_starts_every_input_at_zero() {
    let valid = Marks {
        sessions: SessionsWritten::at(EventId::new(5)),
        rows: Some(RowsWritten {
            take_offset: 1700,
            time_pending: Some(READ_MS + 100),
            ..rows_at(4)
        }),
        omitted: None,
    };
    let until = READ_MS + 10;
    let zero = Marks {
        sessions: SessionsWritten::at(EventId::new(0)),
        rows: Some(RowsWritten {
            time_pending: Some(until),
            ..RowsWritten::at(Cursors::default(), i64::MIN)
        }),
        omitted: None,
    };
    let mut cases = vec![
        ("no marks", None),
        (
            "W alone",
            Some(Marks {
                rows: None,
                ..valid.clone()
            }),
        ),
        (
            "no read time",
            Some(Marks {
                rows: Some(RowsWritten {
                    read_ms: None,
                    ..valid.rows.unwrap()
                }),
                ..valid.clone()
            }),
        ),
        (
            "W cuts R",
            Some(Marks {
                sessions: SessionsWritten::at(EventId::new(3)),
                ..valid.clone()
            }),
        ),
    ];
    for version in [LEDGER_V - 1, LEDGER_V + 1] {
        cases.push((
            "other W version",
            Some(Marks {
                sessions: SessionsWritten {
                    ledger_v: version,
                    ..valid.sessions
                },
                ..valid.clone()
            }),
        ));
        cases.push((
            "other R version",
            Some(Marks {
                rows: Some(RowsWritten {
                    ledger_v: version,
                    ..valid.rows.unwrap()
                }),
                ..valid.clone()
            }),
        ));
    }
    let heads = heads();
    for (name, marks) in cases {
        assert_eq!(
            marks_with_time_work(marks.as_ref(), &heads, until),
            zero,
            "{name}"
        );
    }
    assert_eq!(
        rows_to_go_on_from(Some(&zero), &heads),
        Some((Cursors::default(), i64::MIN))
    );
    let mut later_heads = heads;
    later_heads[0].ledger_v = LEDGER_V + 1;
    assert_eq!(
        marks_with_time_work(Some(&valid), &later_heads, until),
        zero
    );
}

/// The time crosses in the marks, and an R of no time (written before it
/// was held) reads as one, written with no such field.
#[test]
fn the_time_read_crosses_with_r_and_an_r_with_none_reads_as_none() {
    let marks = Marks {
        sessions: SessionsWritten::at(EventId::new(5)),
        rows: Some(RowsWritten::at(rows_at(4).through, i64::MIN)),
        omitted: None,
    };
    let text = serde_json::to_string(&marks).unwrap();
    assert_eq!(serde_json::from_str::<Marks>(&text).unwrap(), marks);
    let none = RowsWritten {
        read_ms: None,
        ..rows_at(4)
    };
    let text = serde_json::to_string(&none).unwrap();
    assert!(!text.contains("read_ms"), "{text}");
    assert_eq!(serde_json::from_str::<RowsWritten>(&text).unwrap(), none);
}

#[test]
fn restart_reasons_explain_why_r_cannot_be_used() {
    assert_eq!(restart_reason(None, &[]), Some("no_marks"));
    let mut marks = Marks {
        sessions: SessionsWritten::at(EventId::new(20)),
        rows: None,
        omitted: None,
    };
    assert_eq!(restart_reason(Some(&marks), &[]), Some("no_rows_mark"));
    marks.rows = Some(rows_at(10));
    assert_eq!(restart_reason(Some(&marks), &[]), None);
    marks.rows.as_mut().unwrap().ledger_v = LEDGER_V - 1;
    assert_eq!(restart_reason(Some(&marks), &[]), Some("version_mismatch"));
    marks.rows = Some(rows_at(10));
    marks.rows.as_mut().unwrap().read_ms = None;
    assert_eq!(restart_reason(Some(&marks), &[]), Some("no_read_ms"));
    marks.rows = Some(rows_at(30));
    assert_eq!(restart_reason(Some(&marks), &[]), Some("w_cuts_r"));
}

/// Old marks start at the first check; new marks carry scheduling state
/// independently of their certified positions and latest event time.
#[test]
fn take_offset_round_trips_and_defaults_for_existing_marks() {
    let original = rows_at(10);
    let text = serde_json::to_string(&original).unwrap();
    assert!(!text.contains("take_offset"));
    assert_eq!(
        serde_json::from_str::<RowsWritten>(&text)
            .unwrap()
            .take_offset,
        0
    );
    let rotated = RowsWritten {
        take_offset: 1700,
        time_pending: Some(1_900_000_100_000),
        ..original
    };
    let text = serde_json::to_string(&rotated).unwrap();
    assert_eq!(serde_json::from_str::<RowsWritten>(&text).unwrap(), rotated);
    let marks = Marks {
        sessions: SessionsWritten::at(EventId::new(10)),
        rows: Some(rotated),
        omitted: None,
    };
    assert_eq!(
        rows_to_go_on_from(Some(&marks), &[]),
        Some((original.through, original.read_ms.unwrap()))
    );
}
