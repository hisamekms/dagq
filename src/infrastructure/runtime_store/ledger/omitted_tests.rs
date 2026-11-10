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
        assert_eq!(
            queue
                .ledger_omitted_point_at_or_before(KIND, EventId::new(27))
                .unwrap(),
            Some(points[4].clone())
        );
        assert_eq!(
            queue
                .ledger_omitted_point_at_or_before(KIND, EventId::new(25))
                .unwrap(),
            Some(points[4].clone())
        );
        assert_eq!(
            queue
                .ledger_omitted_point_at_or_before(KIND, EventId::new(4))
                .unwrap(),
            None
        );
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
        (
            POINT_AT_OR_BEFORE_SQL,
            "SEARCH ledger_omitted_points USING INDEX sqlite_autoindex_ledger_omitted_points_1 (kind=? AND at<?)",
        ),
        (
            EVENT_BY_ID_SQL,
            "SEARCH run_events USING INTEGER PRIMARY KEY (rowid=?)",
        ),
        (
            crate::infrastructure::runtime_store::run_log::EVENTS_OF_KIND_BETWEEN_SQL,
            "SEARCH run_events USING INDEX events_by_kind (kind=? AND id>? AND id<?)",
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

/// The events of `log` written to `queue`'s EventStore as they are.
fn insert_events(queue: &SqliteQueue, events: &[RunEvent]) {
    queue
        .conn
        .pragma_update(None, "foreign_keys", false)
        .unwrap();
    for event in events {
        queue
            .conn
            .execute(
                "INSERT INTO run_events(id, task_id, run_id, kind, payload, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    event.id.as_i64(),
                    event.task_id.map(|task| task.as_i64()),
                    event.run_id.as_ref().map(|run| run.as_str()),
                    event.kind.as_str(),
                    event.payload.to_string(),
                    event.created_at,
                ],
            )
            .unwrap();
    }
}

/// What a read of an event gives that a test compares.
fn seen(event: &RunEvent) -> (EventId, String, Option<String>, String) {
    (
        event.id,
        event.kind.as_str().to_owned(),
        event.run_id.as_ref().map(|run| run.as_str().to_owned()),
        event.created_at.clone(),
    )
}

/// The queue's events of the kind, a run's of the same kind, one of no time
/// every seventh and another kind's every fifth.
fn edge_log() -> crate::domain::ledger::testkit::Log {
    use crate::domain::ledger::testkit::{Log, run_id};
    let mut log = Log::default();
    for n in 0..30 {
        log.add(None, None, KIND, serde_json::json!({}), n * 10);
        log.add(
            None,
            Some(&run_id(4)),
            KIND,
            serde_json::json!({}),
            n * 10 + 1,
        );
        if n % 7 == 3 {
            log.add(None, None, KIND, serde_json::json!({}), n * 10 + 2);
            log.events.last_mut().unwrap().created_at = "not a time".into();
        }
        if n % 5 == 0 {
            log.add(
                None,
                None,
                "supervisor_alive",
                serde_json::json!({"supervisor": "s1"}),
                n * 10 + 3,
            );
        }
    }
    log
}

/// The reads a window's edges make of the store in its snapshot
/// (`LedgerSnapshot`) give what was written, on a file and in memory: the
/// coverage; its points by key, past a position and at or before one, and
/// by the index of M; an event by its key; and a kind's events of a range
/// by the index of kind and id, oldest first within a limit, the run's and
/// those of no time among them (which of them are omitted is the domain's).
#[test]
fn the_snapshot_reads_the_coverage_its_points_and_events_as_written() {
    use crate::domain::ledger::testkit::ms;
    let log = edge_log();
    let last = log.last();
    let mut points = Vec::new();
    let mut covered = Coverage::start(&[KIND.to_owned()], EventId::new(0));
    covered.read(&log.events, 4, last, &mut points);
    assert!(points.len() > 3);
    on_each(|queue| {
        insert_events(queue, &log.events);
        queue
            .write_ledger_marks(&marks(
                last.as_i64(),
                Some(Omitted {
                    coverage: covered.clone(),
                    points: points.clone(),
                }),
            ))
            .unwrap();
        queue
            .ledger_read_snapshot(&mut |snapshot| {
                assert_eq!(snapshot.omitted_coverage()?, Some(covered.clone()));
                assert_eq!(
                    snapshot.omitted_points_after(KIND, EventId::new(0), usize::MAX)?,
                    points
                );
                assert_eq!(
                    snapshot.omitted_points_after(KIND, points[1].at, 2)?,
                    points[2..4]
                );
                assert_eq!(
                    snapshot.omitted_point_at_or_before(KIND, points[2].at)?,
                    Some(points[2].clone())
                );
                assert_eq!(
                    snapshot.omitted_point_at_or_before(
                        KIND,
                        EventId::new(points[2].at.as_i64() - 1)
                    )?,
                    Some(points[1].clone())
                );
                let m = points[2].max_ms.unwrap();
                let before: Vec<&Point> = points
                    .iter()
                    .filter(|point| point.max_ms.is_some_and(|max| max < m))
                    .collect();
                assert_eq!(
                    snapshot.omitted_point_before_ms(KIND, m)?.as_ref(),
                    before.last().copied()
                );
                assert_eq!(snapshot.omitted_point_before_ms(KIND, ms(-1))?, None);
                for event in log.events.iter().step_by(5) {
                    assert_eq!(
                        snapshot.event(event.id)?.as_ref().map(seen),
                        Some(seen(event))
                    );
                }
                assert_eq!(snapshot.event(EventId::new(last.as_i64() + 1))?, None);
                let (after, upto) = (EventId::new(10), EventId::new(60));
                let of_kind: Vec<_> = log
                    .events
                    .iter()
                    .filter(|event| {
                        event.kind.as_str() == KIND && event.id > after && event.id <= upto
                    })
                    .map(seen)
                    .collect();
                assert!(of_kind.iter().any(|event| event.2.is_some()));
                assert!(of_kind.iter().any(|event| event.3 == "not a time"));
                let read = snapshot.events_of_kind(KIND, after, upto, usize::MAX)?;
                assert_eq!(read.iter().map(seen).collect::<Vec<_>>(), of_kind);
                let read = snapshot.events_of_kind(KIND, after, upto, 3)?;
                assert_eq!(read.iter().map(seen).collect::<Vec<_>>(), of_kind[..3]);
                assert!(
                    snapshot
                        .events_of_kind("telemetry_token_revoked", after, upto, 9)?
                        .is_empty()
                );
                Ok(())
            })
            .unwrap();
    });
}

/// A window read of a queue.db file reads the coverage and the events of
/// one state: a builder's pass that another connection writes while the
/// snapshot is open (an event, a further S and the rows' marks) is not seen
/// in it, and is seen by the next read.
#[test]
fn a_pass_written_while_the_snapshot_is_open_is_not_seen_in_it() {
    use crate::domain::ledger::testkit::Log;
    let mut log = Log::default();
    for n in 0..8 {
        log.add(None, None, KIND, serde_json::json!({}), n * 10);
    }
    let last = log.last();
    let mut points = Vec::new();
    let mut covered = Coverage::start(&[KIND.to_owned()], EventId::new(0));
    covered.read(&log.events, 4, last, &mut points);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("q.db");
    let queue = SqliteQueue::init(&path).unwrap();
    insert_events(&queue, &log.events);
    queue
        .write_ledger_marks(&marks(
            last.as_i64(),
            Some(Omitted {
                coverage: covered.clone(),
                points,
            }),
        ))
        .unwrap();
    log.add(None, None, KIND, serde_json::json!({}), 100);
    let next = log.events.last().unwrap().clone();
    let mut further = covered.clone();
    let mut more = Vec::new();
    further.read(std::slice::from_ref(&next), 4, next.id, &mut more);
    let other = SqliteQueue::open(&path).unwrap();
    queue
        .ledger_read_snapshot(&mut |snapshot| {
            assert_eq!(snapshot.omitted_coverage()?, Some(covered.clone()));
            insert_events(&other, std::slice::from_ref(&next));
            other.write_ledger_marks(&marks(
                next.id.as_i64(),
                Some(Omitted {
                    coverage: further.clone(),
                    points: more.clone(),
                }),
            ))?;
            assert_eq!(snapshot.omitted_coverage()?, Some(covered.clone()));
            assert_eq!(snapshot.event(next.id)?, None);
            assert!(snapshot.events_of_kind(KIND, last, next.id, 9)?.is_empty());
            Ok(())
        })
        .unwrap();
    assert_eq!(held(&queue), Some(further));
    assert_eq!(w(&queue), next.id);
}
