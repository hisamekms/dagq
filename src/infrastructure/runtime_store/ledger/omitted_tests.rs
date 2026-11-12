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
            KIND_HAS_AFTER_SQL,
            "SEARCH run_events USING COVERING INDEX events_by_kind (kind=? AND id>? AND id<?)",
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

/// Private checkpoints and a publication share W/R's transaction. Stale
/// checkpoints cannot erase progress, and a failed row CAS publishes none
/// of the replacement points or summary, even when S itself stays put.
#[test]
fn a_rebuild_checkpoint_and_publication_are_atomic_with_the_marks() {
    use crate::domain::ledger::session::written::omitted::{Covering, Omission};
    use crate::domain::ledger::testkit::Log;
    on_each(|queue| {
        let omission = Omission::of([KIND], 2);
        let mut log = Log::default();
        for secs in [200, 50, 1, 100, 40] {
            log.add(None, None, KIND, serde_json::json!({}), secs);
        }
        insert_events(queue, &log.events);
        assert!(
            queue
                .ledger_kind_has_after(KIND, EventId::new(3), EventId::new(4))
                .unwrap()
        );
        assert!(
            !queue
                .ledger_kind_has_after(KIND, EventId::new(4), EventId::new(4))
                .unwrap()
        );
        assert!(
            !queue
                .ledger_kind_has_after("telemetry_token_revoked", EventId::new(0), log.last())
                .unwrap()
        );
        assert_eq!(
            queue
                .events_of_between(&[KIND], EventId::new(1), EventId::new(4), 2)
                .unwrap(),
            log.events[1..3]
        );
        let mut public = Coverage::start(&omission.kinds, EventId::new(1));
        let mut points = Vec::new();
        public.read(&log.events[1..], omission.every, log.last(), &mut points);
        queue
            .write_ledger_marks(&marks(
                5,
                Some(Omitted {
                    coverage: public.clone(),
                    points,
                }),
            ))
            .unwrap();
        let mut covering = Covering::default();
        covering.begin(
            &omission,
            (Some(&public), Some(EventId::new(5))),
            EventId::new(5),
        );
        let advance = |covering: &mut Covering| {
            covering
                .rebuild(omission.every, 2, |kind, after, upto, limit| {
                    let events: Vec<_> = log
                        .events
                        .iter()
                        .filter(|e| e.kind.as_str() == kind && e.id > after && e.id <= upto)
                        .take(limit)
                        .cloned()
                        .collect();
                    let complete = log
                        .events
                        .iter()
                        .filter(|e| e.kind.as_str() == kind && e.id <= upto)
                        .map(|e| e.id)
                        .max()
                        <= events.last().map(|e| e.id);
                    Ok::<_, std::convert::Infallible>((events, complete))
                })
                .unwrap();
        };
        advance(&mut covering);
        let first = covering.to_write().unwrap();
        assert!(
            queue
                .write_ledger_marks(&marks(5, Some(first.clone())))
                .unwrap()
        );
        covering.wrote(EventId::new(5));
        assert_eq!(held(queue), Some(first.coverage.clone()));
        let original_points = all_points(queue);
        // No coverage in an old-version/W-only/no-R write preserves it.
        let mut rollback = marks(0, None);
        rollback.sessions.ledger_v -= 1;
        rollback.rows = None;
        queue.write_ledger_marks(&rollback).unwrap();
        assert_eq!(held(queue), Some(first.coverage.clone()));
        advance(&mut covering);
        let second = covering.to_write().unwrap();
        assert!(
            queue
                .write_ledger_marks(&marks(5, Some(second.clone())))
                .unwrap()
        );
        covering.wrote(EventId::new(5));
        assert!(!queue.write_ledger_marks(&marks(77, Some(first))).unwrap());
        assert_eq!(w(queue), EventId::new(5));
        advance(&mut covering);
        // The service serializes the candidate before its atomic write.
        let publication: Omitted =
            serde_json::from_value(serde_json::to_value(covering.to_write().unwrap()).unwrap())
                .unwrap();
        assert_eq!(publication.coverage.from, EventId::new(0));
        let (sessions, _) = crate::domain::ledger::testkit::sessions_across();
        let mut row =
            crate::domain::ledger::session::session_rows(&sessions.events, BUILT_AT).remove(0);
        // An incorrect expected row forces the whole transaction to fail.
        row.built_at = "expected but absent".into();
        assert!(
            !queue
                .write_session_rows_marked(
                    &[(row.clone(), Some(row))],
                    Some(&marks(9, Some(publication.clone())))
                )
                .unwrap()
        );
        assert_eq!(held(queue), Some(second.coverage));
        assert_eq!(all_points(queue), original_points);
        assert_eq!(w(queue), EventId::new(5));
        assert!(
            queue
                .write_ledger_marks(&marks(5, Some(publication)))
                .unwrap()
        );
        let final_coverage = held(queue).unwrap();
        assert_eq!(final_coverage.from, EventId::new(0));
        assert!(final_coverage.rebuilding.is_none());
        assert!(final_coverage.replacing.is_none());
        assert_eq!(final_coverage.summaries[KIND].back_ms, 199_000);
        assert_eq!(
            all_points(queue).iter().map(|p| p.at).collect::<Vec<_>>(),
            vec![EventId::new(2), EventId::new(4)]
        );
    });
}

/// SQL narrows by owner and the shared kind list, preserves other owners,
/// and rechecks selected keys after a competing row write. Batches are
/// tested directly, so no production pause or real-time wait is involved.
#[test]
fn omitted_removal_filters_subjects_batches_keys_and_rechecks_the_snapshot() {
    use crate::domain::ledger::testkit::{Log, ms};
    use crate::domain::ledger::{self, queue::OMITTED_KINDS};
    on_each(|queue| {
        let mut log = Log::default();
        for kind in OMITTED_KINDS {
            for n in 0..=ROWS_PER_TRANSACTION {
                log.add(
                    None,
                    None,
                    kind.as_str(),
                    serde_json::json!({}),
                    i64::try_from(n).unwrap(),
                );
            }
        }
        let mut rows = ledger::build(&log.events, ms(10_000), BUILT_AT);
        for row in &mut rows {
            row.is_final = true;
        }
        // Equal record ids across a page boundary still have distinct keys.
        let mut duplicate = rows[0].clone();
        duplicate.key = "duplicate".into();
        rows.push(duplicate);
        let count = rows.len();
        let mut unknown = rows[0].clone();
        unknown.key = "unknown".into();
        unknown.subject.as_mut().unwrap().kind = "not_omitted".into();
        let mut kept = vec![unknown];
        for kind in [
            LedgerKind::Run,
            LedgerKind::Task,
            LedgerKind::Session,
            LedgerKind::Node,
        ] {
            let mut other = rows[0].clone();
            other.kind = kind;
            kept.push(other);
        }
        for row in rows.iter().chain(&kept) {
            queue.write_ledger_row(row).unwrap();
        }
        assert!(queue.omitted_queue_keys().unwrap().is_empty());
        let kinds: Vec<_> = OMITTED_KINDS
            .iter()
            .map(|kind| kind.as_str().to_owned())
            .collect();
        let mut public = Coverage::start(&kinds, EventId::new(0));
        let mut points = Vec::new();
        public.read(&log.events, 4, log.last(), &mut points);
        queue
            .write_ledger_marks(&marks(
                1,
                Some(Omitted {
                    coverage: public,
                    points,
                }),
            ))
            .unwrap();
        let keys = queue.omitted_queue_keys().unwrap();
        assert_eq!(keys.len(), count);
        assert_eq!(
            keys.iter().collect::<std::collections::BTreeSet<_>>().len(),
            count
        );
        // A selected row changes before deletion starts. Each write must
        // read the current row rather than trust the unlocked selection.
        let mut changed = rows[0].clone();
        changed.subject.as_mut().unwrap().kind = "not_omitted".into();
        queue.write_ledger_row(&changed).unwrap();
        kept.push(changed);
        let saved_marks = queue.ledger_marks().unwrap();
        let saved_points = all_points(queue);
        {
            let tx = queue.write_tx("test_bound").unwrap();
            assert!(
                SqliteQueue::remove_omitted_queue_batch(&tx, &keys[..=ROWS_PER_TRANSACTION])
                    .is_err()
            );
        }
        let mut removed = 0;
        let mut batches = 0;
        for batch in keys.chunks(ROWS_PER_TRANSACTION) {
            let tx = queue.write_tx("test_batch").unwrap();
            let n = SqliteQueue::remove_omitted_queue_batch(&tx, batch).unwrap();
            assert!(n <= ROWS_PER_TRANSACTION);
            removed += n;
            batches += 1;
            tx.commit().unwrap();
        }
        assert!(batches > 1);
        assert_eq!(removed, count - 1);
        assert!(queue.omitted_queue_keys().unwrap().is_empty());
        let tx = queue.write_tx("test_repeated_batch").unwrap();
        assert_eq!(
            SqliteQueue::remove_omitted_queue_batch(&tx, &keys[..ROWS_PER_TRANSACTION]).unwrap(),
            0
        );
        tx.commit().unwrap();
        for row in &kept {
            assert_eq!(
                queue.ledger_row(row.kind, &row.key).unwrap(),
                Some(row.clone())
            );
        }
        assert_eq!(queue.ledger_rows(LedgerKind::Queue).unwrap().len(), 2);
        assert_eq!(queue.ledger_marks().unwrap(), saved_marks);
        assert_eq!(all_points(queue), saved_points);
    });
}

/// A retained history creates no write entry at all, regardless of its
/// page count. Removing the writer identity makes any attempted write
/// fail immediately; no timing assertion or production sleep is needed.
#[test]
fn a_retained_omitted_history_takes_no_write_entry_or_pause() {
    use crate::domain::ledger::testkit::{Log, ms};
    use crate::domain::ledger::{self, queue::OMITTED_KINDS};
    let uri = format!(
        "file:/ledger-omitted-no-write-{}?vfs=memdb",
        uuid::Uuid::new_v4()
    );
    let mut queue = SqliteQueue::init(&uri).unwrap();
    let mut log = Log::default();
    for n in 0..(3 * ROWS_PER_TRANSACTION + 1) {
        log.add(
            None,
            None,
            OMITTED_KINDS[0].as_str(),
            serde_json::json!({}),
            i64::try_from(n).unwrap(),
        );
    }
    for mut row in ledger::build(&log.events, ms(10_000), BUILT_AT) {
        row.is_final = true;
        queue.write_ledger_row(&row).unwrap();
    }
    let kinds = OMITTED_KINDS
        .iter()
        .map(|kind| kind.as_str().to_owned())
        .collect::<Vec<_>>();
    queue
        .write_ledger_marks(&marks(
            log.last().as_i64(),
            Some(Omitted {
                coverage: Coverage::start(&kinds, log.last()),
                points: Vec::new(),
            }),
        ))
        .unwrap();
    queue.entries.write_identity = None;
    assert!(queue.write_tx("would_fail").is_err());
    assert_eq!(queue.remove_omitted_queue_rows().unwrap(), 0);
    assert_eq!(queue.remove_omitted_queue_rows().unwrap(), 0);
    assert_eq!(
        queue.ledger_rows(LedgerKind::Queue).unwrap().len(),
        log.events.len()
    );
}

/// Candidate SQL seeks by subject, version and the record's id, skipping
/// retained prefix/suffix, old versions and unfinished rows in the index.
/// VM work and returned rows stay bounded when that history grows; invalid
/// detail there also proves the production selector never decodes it.
#[test]
fn omitted_candidates_seek_the_public_range_without_reading_retained_history() {
    use crate::domain::ledger::testkit::{Log, ms};
    use crate::domain::ledger::{self, LEDGER_V};
    use rusqlite::StatementStatus;

    // Exercise one-row pages, including two keys with the same event id.
    let read = |queue: &SqliteQueue| {
        let mut after: Option<(i64, String)> = None;
        let mut keys = Vec::new();
        let mut steps = 0;
        loop {
            let sql = omitted_queue_candidates_sql(SqlDialect::Sqlite, after.is_some());
            let mut statement = queue.conn.prepare(&sql).unwrap();
            let params: Vec<rusqlite::types::Value> = vec![
                "queue".to_owned().into(),
                KIND.to_owned().into(),
                i64::from(LEDGER_V).into(),
                after.as_ref().map_or(1_000, |(id, _)| *id).into(),
                2_000.into(),
                1.into(),
            ];
            let mut params = params;
            if let Some((_, key)) = &after {
                params.push(key.clone().into());
            }
            let rows = statement
                .query_map(rusqlite::params_from_iter(params), |r| {
                    Ok((r.get::<_, String>("key")?, r.get::<_, String>("records")?))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            steps += statement.get_status(StatementStatus::VmStep);
            let Some((key, records)) = rows.last() else {
                break;
            };
            let records: serde_json::Value = serde_json::from_str(records).unwrap();
            after = Some((records[0]["id"].as_i64().unwrap(), key.clone()));
            keys.extend(rows.into_iter().map(|(key, _)| key));
        }
        (keys, steps)
    };
    on_each(|queue| {
        let mut log = Log::default();
        log.add(None, None, KIND, serde_json::json!({}), 0);
        let mut template = ledger::build(&log.events, ms(10_000), BUILT_AT).remove(0);
        template.is_final = true;
        let row_at = |id: i64, key: String| {
            let mut row = template.clone();
            row.key = key;
            row.records[0].id = EventId::new(id);
            row.inputs_from = EventId::new(id);
            row.inputs_through.events = EventId::new(id);
            row
        };
        for (id, key) in [(1_100, "first"), (1_100, "second"), (1_500, "third")] {
            queue.write_ledger_row(&row_at(id, key.into())).unwrap();
        }
        let mut public = coverage(2_000);
        public.from = EventId::new(1_000);
        queue
            .write_ledger_marks(&marks(
                2_000,
                Some(Omitted {
                    coverage: public,
                    points: Vec::new(),
                }),
            ))
            .unwrap();
        let before = read(queue);
        assert_eq!(before.0, vec!["first", "second", "third"]);
        for n in 0..=ROWS_PER_TRANSACTION {
            let n = i64::try_from(n).unwrap();
            for (class, id) in [
                ("prefix", n + 1),
                ("suffix", n + 2_001),
                ("old", n + 1_200),
                ("unfinished", n + 1_200),
            ] {
                let mut row = row_at(id, format!("kept-{class}-{n}"));
                if class == "old" {
                    row.ledger_v -= 1;
                }
                if class == "unfinished" {
                    row.is_final = false;
                }
                queue.write_ledger_row(&row).unwrap();
            }
        }
        // These rows must not reach ledger_row's JSON decoding.
        queue
            .conn
            .execute(
                "UPDATE ledger_rows SET detail='not json' WHERE key LIKE 'kept-%'",
                [],
            )
            .unwrap();
        let after = read(queue);
        assert_eq!(after.0, before.0);
        assert!(
            after.1 <= before.1 + 100,
            "VM steps grew with retained history: {before:?} -> {after:?}"
        );
        assert_eq!(queue.omitted_queue_keys().unwrap(), before.0);
        for after in [false, true] {
            let sql = omitted_queue_candidates_sql(SqlDialect::Sqlite, after);
            let mut statement = queue
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let nulls = vec![rusqlite::types::Null; statement.parameter_count()];
            let plan = statement
                .query_map(rusqlite::params_from_iter(nulls), |r| {
                    r.get::<_, String>("detail")
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(plan.len(), 1, "{plan:?}");
            assert!(plan[0].contains("SEARCH ledger_rows USING INDEX ledger_rows_omitted_removal (kind=? AND subject_kind=? AND ledger_v=? AND <expr>>? AND <expr><?)"), "{plan:?}");
        }
        // The migration and selector use exactly the dialect's expressions.
        let pg = omitted_queue_candidates_sql(SqlDialect::Postgres, false);
        let migration = include_str!("../../../../migrations-postgres/ledger_omitted_removal.sql");
        assert!(migration.contains(&SqlDialect::Postgres.json_field(
            "records",
            "$[0].id",
            JsonType::Integer
        )));
        assert!(pg.contains(&SqlDialect::Postgres.json_array_length("records")));
    });
}
