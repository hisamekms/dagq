//! What the earlier records sought by what the range names read of
//! SQLite: how many the read returns whatever the settled history, and
//! the plans of its seeks. The answers and the parts read whole again are
//! the store contract's ([`crate::infrastructure::store_contract::ledger_earlier`]).
use std::cell::Cell;

use super::earlier::{
    BY_TASK_SQL, STARTS_DOWN_SQL, by_payload_sql, by_task_kind_down_sql, by_task_kind_sql,
    session_range_sql,
};
use super::*;
use crate::application::LedgerWindowRead;
use crate::domain::ledger::window::named::{GOAL_TASK_KINDS, MEMBER_END_KINDS, MEMBER_KINDS};
use crate::infrastructure::store_contract::ledger_earlier::{
    history, kpi_questions, kpi_value, lay_down, time_query,
};

/// What a read returns of the run, task and session rows, the earlier
/// records and the first Executions (Spike 4678's `Reads`), the queue's
/// rows apart: which of those a read takes is another read's (Spike 4756,
/// task 4122), and this one reads them all.
struct Counted<'a> {
    snapshot: &'a dyn LedgerWindowRead,
    /// Whether it seeks by what is named: a store that keeps no tables
    /// reads [`LedgerWindowRead::earlier`] instead.
    seeks: bool,
    rows: Cell<usize>,
    earlier: Cell<usize>,
    executions: Cell<usize>,
}

impl Counted<'_> {
    fn sum(&self) -> (usize, usize, usize) {
        (self.rows.get(), self.earlier.get(), self.executions.get())
    }
}

impl LedgerWindowRead for Counted<'_> {
    fn rows_after(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LedgerRow>> {
        let rows = self.snapshot.rows_after(kind, after, limit)?;
        if kind != LedgerKind::Queue {
            self.rows.set(self.rows.get() + rows.len());
        }
        Ok(rows)
    }
    fn rows_from(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
        from: &RowsFrom,
    ) -> Result<Vec<LedgerRow>> {
        let rows = self.snapshot.rows_from(kind, after, limit, from)?;
        if kind != LedgerKind::Queue {
            self.rows.set(self.rows.get() + rows.len());
        }
        Ok(rows)
    }
    fn earlier(&self, from: &RowsFrom) -> Result<EarlierRecords> {
        let earlier = self.snapshot.earlier(from)?;
        self.earlier.set(self.earlier.get() + earlier.records.len());
        self.executions
            .set(self.executions.get() + earlier.executions.len());
        Ok(earlier)
    }
    fn earlier_named(
        &self,
        from: &RowsFrom,
        named: &crate::domain::ledger::window::named::Named,
    ) -> Result<EarlierRecords> {
        if !self.seeks {
            return self.earlier(from);
        }
        let earlier = self.snapshot.earlier_named(from, named)?;
        self.earlier.set(self.earlier.get() + earlier.records.len());
        self.executions
            .set(self.executions.get() + earlier.executions.len());
        Ok(earlier)
    }
    fn row(&self, kind: LedgerKind, key: &str) -> Result<Option<LedgerRow>> {
        let row = self.snapshot.row(kind, key)?;
        self.rows.set(self.rows.get() + usize::from(row.is_some()));
        Ok(row)
    }
    fn run_events(&self, run: &crate::domain::RunId) -> Result<Vec<RunEvent>> {
        self.snapshot.run_events(run)
    }
    fn omitted_coverage(&self) -> Result<Option<Coverage>> {
        self.snapshot.omitted_coverage()
    }
    fn omitted_points_after(&self, kind: &str, after: EventId, limit: usize) -> Result<Vec<Point>> {
        self.snapshot.omitted_points_after(kind, after, limit)
    }
    fn omitted_point_at_or_before(&self, kind: &str, at: EventId) -> Result<Option<Point>> {
        self.snapshot.omitted_point_at_or_before(kind, at)
    }
    fn omitted_point_before_ms(&self, kind: &str, ms: i64) -> Result<Option<Point>> {
        self.snapshot.omitted_point_before_ms(kind, ms)
    }
    fn event(&self, id: EventId) -> Result<Option<RunEvent>> {
        self.snapshot.event(id)
    }
    fn events_of_kind(
        &self,
        kind: &str,
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>> {
        self.snapshot.events_of_kind(kind, after, upto, limit)
    }
}

/// A store of a memdb of its own.
fn queue() -> SqliteQueue {
    let uri = format!("file:/ledger-earlier-{}?vfs=memdb", uuid::Uuid::new_v4());
    SqliteQueue::init(uri).unwrap()
}

/// The reads of the day's question of the history of `settled` units and
/// `changes` of the plan of task 66, sought by what is named; and, as
/// the store read them before (the earlier records of every row left out),
/// the earlier records alone.
fn reads_of(settled: usize, changes: usize) -> ((usize, usize, usize), usize) {
    let queue = queue();
    let tx = queue.write_entry("earlier_reads").unwrap();
    lay_down(&tx, &history(settled, changes)).unwrap();
    let question = &kpi_questions()[0];
    let query = time_query(question);
    let snapshot = LedgerSnapshot { tx: &tx };
    let counted = Counted {
        snapshot: &snapshot,
        seeks: true,
        rows: Cell::new(0),
        earlier: Cell::new(0),
        executions: Cell::new(0),
    };
    // The answers are the store contract's, of 30 units.
    crate::application::ledger_window_in(&counted, &query).unwrap();
    let from = ledger_open_in::named(&tx, &query).unwrap().0;
    let before = earlier_in(&tx, &from).unwrap().records.len();
    (counted.sum(), before)
}

/// The run, task and session rows, the earlier records and the first
/// Executions a read returns are as many with 30 and 3000 settled units
/// outside the range, and with 30 and 3000 changes of the plan of a task
/// of a proposal the range names that ended before it (its ends alone are
/// read), while the earlier records of every row left out grow with them.
#[test]
fn the_reads_do_not_grow_with_the_settled_history() {
    let (few, few_before) = reads_of(30, 30);
    let (many, many_before) = reads_of(3000, 30);
    assert_eq!(few, many);
    assert!(
        many_before > few_before + 2970 * 4,
        "{few_before} {many_before}"
    );
    let (own_few, own_few_before) = reads_of(30, 3000);
    assert_eq!(own_few, few);
    assert!(
        own_few_before >= few_before + 2970,
        "{few_before} {own_few_before}"
    );
}

/// The plan of `sql` (its parameters NULL) has no full scan of a table:
/// each seek walks an index's range or the primary key.
fn assert_no_scan(conn: &rusqlite::Connection, sql: &str) {
    let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let nulls = vec![rusqlite::types::Null; statement.parameter_count()];
    let steps: Vec<String> = statement
        .query_map(rusqlite::params_from_iter(nulls), |r| r.get("detail"))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(
        steps.iter().any(|step| step.starts_with("SEARCH ")),
        "{sql}: {steps:?}"
    );
    for step in &steps {
        let table = step
            .strip_prefix("SCAN ")
            .and_then(|rest| rest.split_whitespace().next());
        assert!(
            !matches!(table, Some("run_events" | "ledger_rows" | "r")),
            "{sql}: {steps:?}"
        );
    }
}

/// Each seek of the read by what is named (the session rows by the range of
/// their keys, a task's ends by its task and kind, a goal's tasks by their
/// creation's, baseline's and change's goal, a proposal's tasks and goals,
/// the follow-ups by their draft, a plan review's records, the run rows by
/// their keys and the last landing's start) walks an index.
#[test]
fn the_seeks_by_what_is_named_walk_no_table_whole() {
    let queue = queue();
    let dialect = SqlDialect::Sqlite;
    let mut statements = vec![
        session_range_sql(dialect),
        BY_TASK_SQL.to_owned(),
        STARTS_DOWN_SQL.to_owned(),
        super::earlier::run_row_sql(dialect),
        by_payload_sql(dialect, None, "plan_review_id"),
        by_payload_sql(dialect, Some(EventKind::FollowUpRegistered), "task_id"),
        by_payload_sql(dialect, Some(EventKind::GoalSubmitted), "proposal_id"),
    ];
    for kind in MEMBER_KINDS {
        statements.push(by_payload_sql(dialect, Some(kind), "proposal_id"));
    }
    for (kind, key) in GOAL_TASK_KINDS {
        statements.push(by_payload_sql(dialect, Some(kind), key));
    }
    for kind in MEMBER_END_KINDS {
        statements.push(by_task_kind_sql(kind));
        statements.push(by_task_kind_down_sql(kind));
    }
    for sql in statements {
        assert_no_scan(&queue.conn, &sql);
    }
}

/// A store that keeps none of the tables (it reads no earlier records by
/// what is named) reads every row left out as before, and answers as
/// every row.
#[test]
fn a_store_of_no_tables_reads_the_rows_left_out_whole_and_answers_alike() {
    let queue = queue();
    let tx = queue.write_entry("earlier_no_tables").unwrap();
    lay_down(&tx, &history(30, 30)).unwrap();
    let snapshot = LedgerSnapshot { tx: &tx };
    let whole = crate::application::ledger_window_in(
        &snapshot,
        &crate::domain::ledger::window::WindowQuery {
            read_from_ms: None,
            ..time_query(&kpi_questions()[0])
        },
    )
    .unwrap();
    for question in kpi_questions() {
        let query = time_query(&question);
        let no_tables = Counted {
            snapshot: &snapshot,
            seeks: false,
            rows: Cell::new(0),
            earlier: Cell::new(0),
            executions: Cell::new(0),
        };
        let window = crate::application::ledger_window_in(&no_tables, &query).unwrap();
        let from = ledger_open_in::named(&tx, &query).unwrap().0;
        assert_eq!(
            no_tables.earlier.get(),
            earlier_in(&tx, &from).unwrap().records.len()
        );
        let whole_question = crate::domain::kpi::KpiQuery {
            read_whole_ledger: true,
            ..question.clone()
        };
        assert_eq!(
            kpi_value(&window, &question).unwrap(),
            kpi_value(&whole, &whole_question).unwrap()
        );
    }
}
