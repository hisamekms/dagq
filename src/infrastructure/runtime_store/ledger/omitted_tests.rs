//! The coverage of the omitted queue events in queue.db: written with the
//! marks only over one it supersedes, never removed, and its points sought
//! by their key and the index of M, on a file and on SQLite's memory.

use super::*;
use crate::domain::ledger::session::written::Cursors;
use rusqlite::Connection;

const KIND: &str = "authorization_denied";

fn point(at: i64, max_ms: i64) -> Point {
    Point {
        kind: KIND.to_owned(),
        at: EventId::new(at),
        max_ms: Some(max_ms),
        last: Some(EventId::new(at - 1)),
        timed: Some(EventId::new(at - 2)),
    }
}

/// A coverage from 0 to `through`, its kind's L at it.
fn coverage(through: i64) -> Coverage {
    let mut coverage = Coverage::start(&[KIND.to_owned()], EventId::new(0));
    coverage.through = EventId::new(through);
    let summary = coverage.summaries.get_mut(KIND).unwrap();
    summary.last = Some(EventId::new(through));
    coverage
}

fn marks(w: i64, omitted: Option<Omitted>) -> Marks {
    Marks {
        sessions: SessionsWritten::at(EventId::new(w)),
        rows: Some(RowsWritten::at(
            Cursors {
                events: EventId::new(w),
                ..Cursors::default()
            },
            0,
        )),
        omitted,
    }
}

fn held(queue: &SqliteQueue) -> Option<Coverage> {
    queue
        .ledger_marks()
        .unwrap()
        .and_then(|marks| marks.omitted)
        .map(|omitted| omitted.coverage)
}

/// The store's W.
fn w(queue: &SqliteQueue) -> EventId {
    queue.ledger_marks().unwrap().unwrap().sessions.through
}

fn all_points(queue: &SqliteQueue) -> Vec<Point> {
    queue
        .ledger_omitted_points_after(KIND, EventId::new(0), usize::MAX)
        .unwrap()
}

/// What `case` checks, on a queue.db file and on one in SQLite's memory.
fn on_each(case: impl Fn(&SqliteQueue)) {
    let dir = tempfile::tempdir().unwrap();
    case(&SqliteQueue::init(dir.path().join("q.db")).unwrap());
    let uri = format!("file:/ledger-omitted-{}?vfs=memdb", uuid::Uuid::new_v4());
    case(&SqliteQueue::init(&uri).unwrap());
}

/// The coverage goes with the marks in their transaction, and only over a
/// lower S of its C and kinds: marks of none and of W cut back to 0 leave
/// it and its points as they are; marks of a lower S or another C are
/// refused whole (W and R too), as are rows written since their read.
#[test]
fn the_coverage_is_written_with_the_marks_and_never_lowered() {
    on_each(|queue| {
        let first = Omitted {
            coverage: coverage(10),
            points: vec![point(4, 100), point(8, 100)],
        };
        queue.write_ledger_marks(&marks(10, Some(first))).unwrap();
        assert_eq!(held(queue), Some(coverage(10)));
        assert_eq!(all_points(queue), vec![point(4, 100), point(8, 100)]);

        let other_c = Coverage {
            from: EventId::new(1),
            ..coverage(30)
        };
        for (written, accepted) in [(marks(0, None), true), (marks(12, None), true)] {
            assert_eq!(queue.write_ledger_marks(&written).unwrap(), accepted);
            assert_eq!(w(queue), written.sessions.through);
            assert_eq!(held(queue), Some(coverage(10)));
        }
        for written in [
            marks(
                14,
                Some(Omitted {
                    coverage: coverage(9),
                    points: vec![point(9, 1)],
                }),
            ),
            marks(
                30,
                Some(Omitted {
                    coverage: other_c,
                    points: vec![point(20, 1)],
                }),
            ),
        ] {
            assert!(!queue.write_ledger_marks(&written).unwrap());
            assert_eq!(w(queue), EventId::new(12));
            assert_eq!(held(queue), Some(coverage(10)));
            assert_eq!(all_points(queue), vec![point(4, 100), point(8, 100)]);
        }
        // The same coverage again goes with its marks, writing nothing of it.
        let again = Omitted {
            coverage: coverage(10),
            points: Vec::new(),
        };
        assert!(queue.write_ledger_marks(&marks(13, Some(again))).unwrap());
        assert_eq!(w(queue), EventId::new(13));

        // A write of the session rows refused writes no coverage either.
        // (the row read is one the store does not hold).
        let (log, _) = crate::domain::ledger::testkit::sessions_across();
        let row = crate::domain::ledger::session::session_rows(&log.events, BUILT_AT).remove(0);
        let stale = Some(row.clone());
        let refused = marks(
            20,
            Some(Omitted {
                coverage: coverage(20),
                points: vec![point(16, 200)],
            }),
        );
        assert!(
            !queue
                .write_session_rows_marked(&[(row, stale)], Some(&refused))
                .unwrap()
        );
        assert_eq!(held(queue), Some(coverage(10)));

        // A further S: the point held again stays the one held.
        let further = Omitted {
            coverage: coverage(20),
            points: vec![point(8, 999), point(12, 150), point(16, 200)],
        };
        queue.write_ledger_marks(&marks(20, Some(further))).unwrap();
        assert_eq!(held(queue), Some(coverage(20)));
        assert_eq!(
            all_points(queue),
            vec![point(4, 100), point(8, 100), point(12, 150), point(16, 200)]
        );
    });
}

const BUILT_AT: &str = "2030-01-01T00:00:00.000Z";

/// A kind's points are sought by position past a point and by time before
/// one, their p, M, L and T rising in order, through the key and the
/// index of M: no read walks the table or sorts.
#[test]
fn the_points_are_sought_by_their_key_and_the_index_of_m() {
    on_each(|queue| {
        let points: Vec<Point> = (1..=20).map(|n| point(5 * n, 1_000 * (n / 2))).collect();
        let mut other = point(3, 50);
        other.kind = "telemetry_token_issued".to_owned();
        let mut sent = points.clone();
        sent.push(other.clone());
        let omitted = Omitted {
            coverage: coverage(100),
            points: sent,
        };
        queue
            .write_ledger_marks(&marks(100, Some(omitted)))
            .unwrap();
        let read = all_points(queue);
        assert_eq!(read, points);
        for pair in read.windows(2) {
            assert!(pair[0].at < pair[1].at);
            assert!(pair[0].max_ms <= pair[1].max_ms);
            assert!(pair[0].last <= pair[1].last);
            assert!(pair[0].timed <= pair[1].timed);
        }
        assert_eq!(
            queue
                .ledger_omitted_points_after(KIND, EventId::new(25), 3)
                .unwrap(),
            points[5..8].to_vec()
        );
        assert_eq!(
            queue
                .ledger_omitted_points_after("telemetry_token_issued", EventId::new(0), 9)
                .unwrap(),
            vec![other.clone()]
        );
        // M 4000 is the 8th and 9th points': the last before it is the 7th.
        assert_eq!(
            queue.ledger_omitted_point_before_ms(KIND, 4_000).unwrap(),
            Some(points[6].clone())
        );
        assert_eq!(queue.ledger_omitted_point_before_ms(KIND, 0).unwrap(), None);
    });
    let dir = tempfile::tempdir().unwrap();
    SqliteQueue::init(dir.path().join("q.db")).unwrap();
    let conn = Connection::open(dir.path().join("q.db")).unwrap();
    for (sql, uses) in [
        (
            POINTS_AFTER_SQL,
            "SEARCH ledger_omitted_points USING INDEX sqlite_autoindex_ledger_omitted_points_1 (kind=? AND at>?)",
        ),
        (
            POINT_BEFORE_MS_SQL,
            "SEARCH ledger_omitted_points USING INDEX ledger_omitted_points_by_ms (kind=? AND max_ms<?)",
        ),
    ] {
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let nulls = vec![rusqlite::types::Null; statement.parameter_count()];
        let plan: Vec<String> = statement
            .query_map(rusqlite::params_from_iter(nulls), |r| r.get("detail"))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(plan, vec![uses.to_owned()], "{sql}");
    }
}

/// A coverage row that does not read (a later shape) reads as one that
/// stops every builder and that nothing writes over: the marks still read.
#[test]
fn a_coverage_that_does_not_read_stops_and_stays() {
    on_each(|queue| {
        queue
            .write_ledger_marks(&marks(
                10,
                Some(Omitted {
                    coverage: coverage(10),
                    points: Vec::new(),
                }),
            ))
            .unwrap();
        let tx = queue.write_tx("test").unwrap();
        SqlWrite::execute(
            &tx,
            "UPDATE ledger_rows SET detail='{\"later\": 1}' WHERE kind=?1",
            sql_params![omitted::KIND],
        )
        .unwrap();
        tx.commit().unwrap();
        let unreadable = held(queue).unwrap();
        assert_eq!(unreadable, Coverage::unreadable());
        assert!(omitted::stops(Some(&unreadable), None, &[KIND.to_owned()]));
        let refused = marks(
            30,
            Some(Omitted {
                coverage: coverage(30),
                points: Vec::new(),
            }),
        );
        assert!(!queue.write_ledger_marks(&refused).unwrap());
        assert_eq!(w(queue), EventId::new(10));
        assert!(queue.write_ledger_marks(&marks(30, None)).unwrap());
        assert_eq!(held(queue), Some(Coverage::unreadable()));
        assert_eq!(w(queue), EventId::new(30));
    });
}
