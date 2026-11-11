use super::*;
use crate::application::LedgerWindowRead;
use crate::domain::ledger::session::written::{Cursors, RowPosition};
use crate::domain::ledger::{
    build,
    window::{WindowQuery, run_on_its_way, runs_on_their_way, select},
};
use std::cell::Cell;

const NOW: i64 = 1_900_000_000_000;
const BUILT: &str = "2030-03-17T17:46:40.000Z";

fn run(n: i64) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

#[derive(Default)]
struct Log(Vec<RunEvent>);
impl Log {
    fn add(
        &mut self,
        task: Option<i64>,
        run_id: Option<i64>,
        kind: &str,
        payload: Value,
    ) -> EventId {
        let id = self.0.len() as i64 + 1;
        self.0.push(RunEvent {
            id: EventId::new(id),
            task_id: task.map(TaskId::new),
            goal_id: None,
            run_id: run_id.map(|n| RunId::new(run(n)).unwrap()),
            kind: kind.into(),
            payload,
            created_at: crate::domain::marks::utc_text(NOW - 100_000 + id),
            actor: None,
        });
        EventId::new(id)
    }
    fn landed(&mut self, n: i64) {
        self.add(Some(n), None, "task_created", json!({}));
        self.add(
            Some(n),
            Some(n),
            "run_claimed",
            json!({"from":"ready", "to":"in_progress"}),
        );
        self.add(Some(n), Some(n), "run_integrated", json!({}));
        self.add(
            Some(n),
            Some(n),
            "push_finished",
            json!({"remote":"origin", "branch":"main"}),
        );
        self.add(
            Some(n),
            Some(n),
            "run_phase_changed",
            json!({"phase":"ended"}),
        );
        self.add(
            Some(n),
            None,
            "task_status_changed",
            json!({"from":"in_progress", "to":"completed"}),
        );
    }
    fn planner(&mut self, n: i64, kind: &str) {
        self.add(
            None,
            None,
            kind,
            json!({"kind":"runtime_planner", "planner_id":n}),
        );
    }
    fn rows(&self, through: EventId, kind: LedgerKind) -> Vec<LedgerRow> {
        build(&self.0[..through.as_i64() as usize], NOW, BUILT)
            .into_iter()
            .filter(|r| r.kind == kind)
            .collect()
    }
}

fn marks(w: EventId, queue: EventId, core: EventId) -> Marks {
    let position = |events| RowPosition {
        through: Cursors {
            events,
            ..Cursors::default()
        },
        read_ms: NOW,
    };
    Marks {
        sessions: SessionsWritten::at(w),
        rows: Some(RowsWritten {
            queue: Some(position(queue)),
            core: Some(position(core)),
            ..RowsWritten::at(position(core).through, NOW)
        }),
        omitted: None,
    }
}

fn queue() -> SqliteQueue {
    let uri = format!("file:/ledger-visibility-{}?vfs=memdb", uuid::Uuid::new_v4());
    SqliteQueue::init(uri).unwrap()
}

fn save(queue: &SqliteQueue, rows: &[LedgerRow], log: &Log, marks: &Marks) {
    for row in rows {
        queue.write_ledger_row(row).unwrap();
    }
    // Synthetic EventStore only: measurement does not read StateStore.
    queue
        .conn
        .pragma_update(None, "foreign_keys", false)
        .unwrap();
    for e in &log.0 {
        queue.conn.execute("INSERT INTO run_events(id,task_id,run_id,kind,payload,created_at) VALUES (?1,?2,?3,?4,?5,?6)",
            rusqlite::params![e.id.as_i64(), e.task_id.map(TaskId::as_i64), e.run_id.as_ref().map(RunId::as_str), e.kind.as_str(), e.payload.to_string(), e.created_at]).unwrap();
    }
    queue
        .conn
        .pragma_update(None, "foreign_keys", true)
        .unwrap();
    assert!(queue.write_ledger_marks(marks).unwrap());
}

fn apart(settled: i64) -> (SqliteQueue, Log, Marks, usize) {
    let mut log = Log::default();
    for n in 1000..1000 + settled {
        log.landed(n);
    }
    let start = log.0.len();
    log.landed(3);
    log.landed(12);
    log.add(Some(70), None, "task_created", json!({}));
    log.add(
        Some(70),
        Some(70),
        "run_claimed",
        json!({"from":"ready", "to":"in_progress"}),
    );
    log.planner(990001, "session_opened");
    log.planner(990003, "session_opened");
    // A queue event at or before the queue R, held by its row.
    log.add(None, None, "mark_added", json!({"mark":"m-0"}));
    let core = log.0.last().unwrap().id;
    log.add(None, Some(70), "first_commit_observed", json!({}));
    log.add(
        Some(70),
        None,
        "task_priority_changed",
        json!({"priority":"high"}),
    );
    let q = log.add(Some(3), Some(3), "agent_started", json!({}));
    log.planner(990004, "session_opened");
    log.add(None, None, "mark_added", json!({"mark":"m-b"}));
    let w = log.add(
        None,
        None,
        "claim_deferred",
        json!({"supervisor":"s2", "reason":"paths"}),
    );
    log.planner(990001, "session_closed");
    log.planner(990003, "session_closed");
    log.planner(990002, "session_opened");
    log.planner(990002, "session_closed");
    log.add(None, None, "mark_added", json!({"mark":"m-a"}));
    log.add(Some(999), Some(70), "receipt_observed", json!({}));
    let end = log.add(None, None, "inbox_watcher_absent", json!({}));
    let mut rows = log.rows(core, LedgerKind::Task);
    rows.extend(
        log.rows(core, LedgerKind::Run)
            .into_iter()
            .filter(|r| r.key != run(12) && r.key != run(70)),
    );
    rows.extend(log.rows(w, LedgerKind::Session));
    rows.extend(log.rows(q, LedgerKind::Queue));
    for r in build(&log.0, NOW, BUILT).into_iter().filter(|r| {
        (r.kind == LedgerKind::Session && r.key.contains("990001"))
            || (r.kind == LedgerKind::Queue && r.records.iter().any(|e| e.payload["mark"] == "m-a"))
    }) {
        rows.retain(|old| old.kind != r.kind || old.key != r.key);
        rows.push(r);
    }
    assert!(end > w && w > q && q > core);
    assert!(
        rows.iter()
            .filter(|r| r.kind == LedgerKind::Task && r.key != "70")
            .all(|r| r.ended_at.is_some()),
        "settled task rows must have ended"
    );
    let marks = marks(w, q, core);
    let queue = queue();
    save(&queue, &rows, &log, &marks);
    (queue, log, marks, start)
}

fn ids(events: &[RunEvent]) -> BTreeSet<EventId> {
    events.iter().map(|e| e.id).collect()
}

/// Whole row read with the coverage from the same snapshot, as the covered
/// row port returns it. Edge supplementation is deliberately the caller's
/// separate operation: visibility compares the rows' records alone.
fn whole(read: &dyn LedgerWindowRead) -> BTreeSet<EventId> {
    let mut rows = Vec::new();
    for kind in [
        LedgerKind::Run,
        LedgerKind::Task,
        LedgerKind::Session,
        LedgerKind::Queue,
    ] {
        rows.extend(read.rows_after(kind, None, usize::MAX).unwrap());
    }
    let _coverage = read.omitted_coverage().unwrap();
    for run in runs_on_their_way(&rows) {
        rows.extend(run_on_its_way(&run, &read.run_events(&run).unwrap(), NOW));
    }
    ids(select(
        rows,
        &WindowQuery {
            now_ms: NOW,
            ..WindowQuery::default()
        },
    )
    .events())
}

struct Counted<'a, T> {
    conn: &'a T,
    rows: Cell<usize>,
    owner_queries: Cell<usize>,
}
impl<T: SqlRead> SqlRead for Counted<'_, T> {
    fn dialect(&self) -> SqlDialect {
        self.conn.dialect()
    }
    fn visit_rows(
        &self,
        sql: &str,
        params: &[SqlValue],
        visit: &mut dyn FnMut(&SqlRow<'_>) -> SqlResult<bool>,
    ) -> SqlResult<()> {
        if sql.starts_with("SELECT * FROM ledger_rows WHERE kind=?1 AND key=?2") {
            self.owner_queries.set(self.owner_queries.get() + 1);
        }
        self.conn.visit_rows(sql, params, &mut |r| {
            self.rows.set(self.rows.get() + 1);
            visit(r)
        })
    }
}

#[test]
fn visibility_matches_whole_rows_and_reads_do_not_grow_with_settled_history() {
    let mut counts = Vec::new();
    for settled in [30, 3000] {
        let (queue, log, marks, start) = apart(settled);
        queue
            .ledger_read_snapshot(&mut |read| {
                assert_eq!(read.ledger_marks()?.unwrap().rows, marks.rows);
                let expected = whole(read);
                let got = read.visible_events(&log.0)?.unwrap();
                assert_eq!(ids(&got), expected);
                for e in &log.0 {
                    if e.run_id.as_ref().is_some_and(|r| r.as_str() == run(12)) {
                        assert!(!expected.contains(&e.id));
                    }
                    if e.run_id.as_ref().is_some_and(|r| r.as_str() == run(70)) {
                        assert!(expected.contains(&e.id));
                    }
                }
                let unseen: Vec<_> = log
                    .0
                    .iter()
                    .filter(|e| e.id > marks.sessions.through && !expected.contains(&e.id))
                    .collect();
                assert!(unseen.iter().any(|e| owner(e) == Owner::Session));
                assert!(unseen.iter().any(|e| owner(e) == Owner::Queue));
                assert!(
                    log.0
                        .iter()
                        .any(|e| e.payload["planner_id"] == 990004 && expected.contains(&e.id))
                );
                Ok(())
            })
            .unwrap();
        let tx = queue.read_entry("visibility_counts").unwrap();
        let counted = Counted {
            conn: &tx,
            rows: Cell::new(0),
            owner_queries: Cell::new(0),
        };
        let candidates = &log.0[start..];
        visible_in(&counted, candidates).unwrap().unwrap();
        // Reads counts returned ledger rows and candidate events as Spike
        // 4678 does (plus the fixed certificates). Post-mark rows scale with
        // builder lag; missing-run checks scale with unfinished task rows.
        counts.push(counted.rows.get() + candidates.len());
    }
    assert_eq!(counts[0], counts[1]);
}

#[test]
fn r_spares_task_owner_rows_but_not_run_owner_rows() {
    let (queue, log, marks, _) = apart(30);
    let tx = queue.read_entry("visibility_counts").unwrap();
    let counted = Counted {
        conn: &tx,
        rows: Cell::new(0),
        owner_queries: Cell::new(0),
    };
    let task: Vec<_> = log
        .0
        .iter()
        .filter(|e| {
            owner(e) == Owner::Task && e.id <= marks.rows.unwrap().core.unwrap().through.events
        })
        .cloned()
        .collect();
    assert!(!task.is_empty());
    assert_eq!(
        visible_in(&counted, &task).unwrap().unwrap().len(),
        task.len()
    );
    assert_eq!(counted.owner_queries.get(), 0);
    assert_eq!(counted.rows.get(), 2); // W and R only; no owner row read.
    let run = log
        .0
        .iter()
        .find(|e| owner(e) == Owner::Run)
        .unwrap()
        .clone();
    let before = counted.rows.get();
    visible_in(&counted, &[run.clone(), run]).unwrap().unwrap();
    assert_eq!(counted.owner_queries.get(), 1); // repeated owner is cached.
    assert_eq!(counted.rows.get() - before, 3); // W, R and the run row.
}

#[test]
fn no_marks_and_default_port_cannot_decide() {
    let queue = queue();
    queue
        .ledger_read_snapshot(&mut |read| {
            assert!(read.ledger_marks()?.is_none());
            assert!(read.visible_events(&[])?.is_none());
            Ok(())
        })
        .unwrap();
    // The blanket port for stores such as SupervisorQueue has no capability.
    let default: &dyn LedgerWindowRead = &&queue;
    assert!(default.ledger_marks().unwrap().is_none());
    assert!(default.visible_events(&[]).unwrap().is_none());
}

#[test]
fn post_mark_query_seeks_the_expression_index() {
    let queue = queue();
    let sql = past_mark_sql(SqlDialect::Sqlite);
    let mut stmt = queue
        .conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap();
    let plan: Vec<String> = stmt
        .query_map(rusqlite::params!["session", 12], |r| r.get(3))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(
        plan.iter().any(|line| line.contains("SEARCH")
            && line.contains("ledger_rows_by_inputs_through")
            && line.contains("<expr>>?")),
        "{plan:?}"
    );
    let pg = past_mark_sql(SqlDialect::Postgres);
    assert!(pg.contains(&SqlDialect::Postgres.json_field(
        "inputs_through",
        "$.events",
        JsonType::Integer
    )));
}

#[test]
fn covered_queue_events_are_hidden_only_inside_the_coverage() {
    let kinds = [
        "authorization_denied",
        "telemetry_token_issued",
        "telemetry_token_revoked",
    ];
    let mut log = Log::default();
    log.landed(3);
    let mut outside = Vec::new();
    for kind in kinds {
        outside.push(log.add(None, None, kind, json!({})));
    }
    let c = log.0.last().unwrap().id;
    let mut omitted = Vec::new();
    let mut other_owners = Vec::new();
    for kind in kinds {
        omitted.push(log.add(None, None, kind, json!({})));
        other_owners.push(log.add(Some(3), Some(3), kind, json!({})));
        other_owners.push(log.add(Some(3), None, kind, json!({})));
        other_owners.push(log.add(None, None, kind, json!({"planner_id":990001})));
    }
    let ordinary = log.add(None, None, "inbox_watcher_absent", json!({}));
    let s = log.add(None, None, kinds[0], json!({}));
    omitted.push(s);
    for kind in kinds {
        outside.push(log.add(None, None, kind, json!({})));
    }
    let end = log.0.last().unwrap().id;
    let mut coverage = Coverage::start(&kinds.map(str::to_owned), c);
    let mut points = Vec::new();
    coverage.read(&log.0[..s.as_i64() as usize], 100, s, &mut points);
    for covered in [false, true] {
        let queue = queue();
        let mut rows = build(&log.0, NOW, BUILT);
        if covered {
            // The saved view omits these queue records, as the queue writer
            // does; run/task/session records of the same kinds remain.
            for row in &mut rows {
                row.records.retain(|e| !omitted.contains(&e.id));
            }
            rows.retain(|row| row.kind != LedgerKind::Queue || !row.records.is_empty());
        }
        // Before C uses R; after S uses the saved post-R queue records.
        let mut marks = marks(end, s, end);
        if covered {
            marks.omitted = Some(Omitted {
                coverage: coverage.clone(),
                points: points.clone(),
            });
        }
        save(&queue, &rows, &log, &marks);
        queue
            .ledger_read_snapshot(&mut |read| {
                assert_eq!(read.omitted_coverage()?.is_some(), covered);
                let got = ids(&read.visible_events(&log.0)?.unwrap());
                assert_eq!(got, whole(read));
                for id in &omitted {
                    assert_eq!(got.contains(id), !covered);
                }
                for id in outside.iter().chain(&other_owners).chain([&ordinary]) {
                    assert!(got.contains(id), "{id}");
                }
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn marks_coverage_rows_and_visibility_remain_in_the_same_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    let writer = SqliteQueue::open(dir.path().join("q.db")).unwrap();
    let mut log = Log::default();
    let first = log.add(None, None, "mark_added", json!({"mark":"before"}));
    let last = log.add(None, None, "mark_added", json!({"mark":"after"}));
    save(
        &queue,
        &log.rows(first, LedgerKind::Queue),
        &log,
        &marks(first, first, first),
    );
    queue
        .ledger_read_snapshot(&mut |read| {
            assert_eq!(read.ledger_marks()?.unwrap().sessions.through, first);
            let mut later = marks(last, last, last);
            let mut coverage = Coverage::start(&["authorization_denied".into()], first);
            coverage.through = last;
            later.omitted = Some(Omitted {
                coverage,
                points: Vec::new(),
            });
            for row in log.rows(last, LedgerKind::Queue) {
                writer.write_ledger_row(&row)?;
            }
            writer.write_ledger_marks(&later)?;
            assert_eq!(read.ledger_marks()?.unwrap().sessions.through, first);
            assert!(read.omitted_coverage()?.is_none());
            assert_eq!(
                ids(&read.visible_events(&log.0)?.unwrap()),
                BTreeSet::from([first])
            );
            assert_eq!(whole(read), BTreeSet::from([first]));
            Ok(())
        })
        .unwrap();
    queue
        .ledger_read_snapshot(&mut |read| {
            assert_eq!(read.ledger_marks()?.unwrap().sessions.through, last);
            assert!(read.omitted_coverage()?.is_some());
            assert_eq!(
                ids(&read.visible_events(&log.0)?.unwrap()),
                BTreeSet::from([first, last])
            );
            Ok(())
        })
        .unwrap();
}

/// The plan of `sql` on `queue`, its parameters NULL, one line per step
/// that reads a table or an index.
fn reads_of(queue: &SqliteQueue, sql: &str) -> Vec<String> {
    let mut stmt = queue
        .conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap();
    let nulls = vec![rusqlite::types::Null; stmt.parameter_count()];
    stmt.query_map(rusqlite::params_from_iter(nulls), |r| {
        r.get::<_, String>("detail")
    })
    .unwrap()
    .map(Result::unwrap)
    .filter(|step| step.starts_with("SEARCH") || step.starts_with("SCAN"))
    .collect()
}

const POSTGRES_MIGRATION: &str =
    include_str!("../../../../../migrations-postgres/ledger_visibility_indexes.sql");

/// The unfinished task rows and the rows of another ledger_v are each read
/// from an index that holds them alone, with no statistics: no read walks
/// the settled task rows or the rows of this ledger_v. No other index seeks
/// the unfinished task rows by ended_at (ledger_rows_by_subject_end puts
/// subject_kind, NULL on a task row, before it): each takes `kind=` alone,
/// every task row. Both series make the same indexes, on the same terms in
/// both dialects.
#[test]
fn open_task_rows_and_other_versions_seek_their_indexes() {
    let queue = queue();
    for index in [
        "sqlite_autoindex_ledger_rows_1",
        "ledger_rows_by_subject_end",
    ] {
        let other = reads_of(
            &queue,
            &format!(
                "SELECT * FROM ledger_rows INDEXED BY {index} WHERE kind='task' AND ended_at IS NULL"
            ),
        );
        assert!(
            other.iter().all(|step| !step.contains("ended_at")),
            "{index} seeks ended_at: {other:?}"
        );
    }
    let open = reads_of(&queue, &open_tasks_sql(SqlDialect::Sqlite));
    assert_eq!(open.len(), 1, "{open:?}");
    assert!(open[0].contains("INDEX ledger_rows_open_tasks"), "{open:?}");
    for side in ["<", ">"] {
        let other = reads_of(&queue, &other_version_sql(SqlDialect::Sqlite, side));
        assert_eq!(other.len(), 1, "{other:?}");
        assert!(
            other[0].starts_with("SEARCH")
                && other[0].contains(&format!("INDEX ledger_rows_by_version (ledger_v{side}?)")),
            "{other:?}"
        );
        assert!(!other_version_sql(SqlDialect::Postgres, side).contains("INDEXED BY"));
    }
    // Postgres has no hint and keeps the same terms as the partial index.
    assert!(!open_tasks_sql(SqlDialect::Postgres).contains("INDEXED BY"));
    let statements = |migration: &str| -> Vec<String> {
        let text: String = migration
            .lines()
            .map(|line| line.split("--").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join(" ");
        text.split(';')
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|s| !s.is_empty())
            .collect()
    };
    let sqlite = crate::infrastructure::schema::MIGRATIONS
        .iter()
        .find(|migration| migration.contains("CREATE INDEX ledger_rows_open_tasks"))
        .unwrap();
    assert_eq!(statements(sqlite), statements(POSTGRES_MIGRATION));
    assert!(
        statements(POSTGRES_MIGRATION).contains(
            &"CREATE INDEX ledger_rows_open_tasks ON ledger_rows(kind) WHERE kind = 'task' AND ended_at IS NULL"
                .to_owned()
        )
    );
}

/// Rows of another ledger_v, earlier or later, whole or with fewer records
/// than the marks promise, past W and the queue R or at or before them. A
/// fold of another version goes on from its cursor 0 and overwrites rows of
/// every kind with what it has folded so far, so a row of another version
/// may lack an event at or before W or R that the whole read then does not
/// see (the opening of 990001 at or before W, a queue record at or before
/// R): a verdict that took every event up to the marks would keep it. Any
/// row of another version voids the marks, the read falls back to the
/// whole read, and so returns what the whole read sees. A legacy row keeps
/// its version for good and leaves the verdict to the marks, equal to the
/// whole read.
#[test]
fn rows_of_other_versions_are_seen_as_the_whole_read_sees_them() {
    let later = crate::domain::ledger::LEDGER_V + 1;
    let earlier = crate::domain::ledger::LEDGER_V - 1;
    let full = |log: &Log| build(&log.0, NOW, BUILT);
    let session_row = |log: &Log| {
        full(log)
            .into_iter()
            .find(|r| r.kind == LedgerKind::Session && r.key.contains("990001"))
            .unwrap()
    };
    let queue_row = |log: &Log, marks: &Marks| {
        let r = marks.rows.unwrap().queue.unwrap().through.events;
        full(log)
            .into_iter()
            .find(|row| row.kind == LedgerKind::Queue && row.records.iter().any(|e| e.id <= r))
            .unwrap()
    };
    let mut lost_any = 0;
    for v in [earlier, later] {
        for (kind, lose) in [
            (LedgerKind::Session, false),
            (LedgerKind::Session, true),
            (LedgerKind::Queue, false),
            (LedgerKind::Queue, true),
        ] {
            let (queue, log, marks, _) = apart(30);
            let (mut row, mark) = if kind == LedgerKind::Session {
                (session_row(&log), marks.sessions.through)
            } else {
                (
                    queue_row(&log, &marks),
                    marks.rows.unwrap().queue.unwrap().through.events,
                )
            };
            let lost = row.records.iter().find(|e| e.id <= mark).unwrap().id;
            if lose {
                row.records.retain(|e| e.id != lost);
            }
            row.ledger_v = v;
            queue.write_ledger_row(&row).unwrap();
            queue
                .ledger_read_snapshot(&mut |read| {
                    let expected = whole(read);
                    assert_eq!(expected.contains(&lost), !lose, "{v} {kind:?} {lose}");
                    if lose {
                        lost_any += 1;
                    }
                    // None: the caller reads the whole ledger, the same events.
                    assert_eq!(read.visible_events(&log.0)?, None, "{v} {kind:?} {lose}");
                    Ok(())
                })
                .unwrap();
        }
    }
    assert_eq!(lost_any, 4);

    // A legacy row of an earlier version does not void the marks.
    let (queue, log, _, _) = apart(30);
    let mut row = full(&log)
        .into_iter()
        .find(|r| r.kind == LedgerKind::Task && r.key == "1000")
        .unwrap();
    row.ledger_v = earlier;
    row.legacy = Some(json!({}));
    queue.write_ledger_row(&row).unwrap();
    queue
        .ledger_read_snapshot(&mut |read| {
            let got = read.visible_events(&log.0)?.unwrap();
            assert_eq!(ids(&got), whole(read));
            Ok(())
        })
        .unwrap();
}
