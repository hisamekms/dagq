//! A read of the ledger from a time on the real store: what its statements
//! select and sum, and that they read one state of the rows.

use std::collections::BTreeSet;

use super::*;
use crate::domain::RunId;
use crate::domain::host_metrics::HOST_NODE;
use crate::domain::ledger::node;
use crate::domain::ledger::testkit::{
    DAY, READ_AT_DAY, T0, ms, rows_read_from_a_clock_turned_back, rows_read_from_a_time, run_id,
};
use crate::domain::ledger::window::{
    LedgerWindow, ROWS_PER_PAGE, WindowQuery, WindowRows, select_from_time,
};

const NOW: i64 = T0 + READ_AT_DAY * DAY;

fn read_from() -> WindowQuery {
    WindowQuery {
        now_ms: NOW * 1000 + 999,
        read_from_ms: Some(ms((READ_AT_DAY - 9) * DAY)),
        ..WindowQuery::default()
    }
}

fn queue_of_rows() -> (tempfile::TempDir, SqliteQueue, Vec<LedgerRow>) {
    queue_of(rows_read_from_a_time())
}

fn queue_of(rows: Vec<LedgerRow>) -> (tempfile::TempDir, SqliteQueue, Vec<LedgerRow>) {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    for row in &rows {
        assert!(queue.write_ledger_row(row).unwrap());
    }
    (dir, queue, rows)
}

/// [`RowsFrom`] of a read, its queue rows taken as a read does.
fn rows_from(queue: &SqliteQueue, query: &WindowQuery) -> RowsFrom {
    let mut read = WindowRows::new(query, 0);
    read.take_pages(|after, limit| queue.ledger_rows_after(LedgerKind::Queue, after, limit))
        .unwrap();
    read.rows_from().unwrap()
}

/// What a window holds of its records and rows, to compare two reads by.
fn held(window: &LedgerWindow) -> (Vec<EventId>, Vec<String>, Vec<EventId>, BTreeSet<String>) {
    let mut firsts: Vec<String> = window
        .earlier()
        .executions
        .iter()
        .map(|first| format!("{} {} {}", first.actor, first.provider, first.at))
        .collect();
    firsts.sort();
    (
        window.events().iter().map(|event| event.id).collect(),
        firsts,
        window
            .earlier()
            .claims
            .iter()
            .map(|claim| claim.id)
            .collect(),
        window.rows.iter().map(|row| row.key.clone()).collect(),
    )
}

/// The rows of `kind` a statement must leave out or read, by what they are.
fn named_rows(kind: LedgerKind) -> Vec<(String, bool)> {
    match kind {
        LedgerKind::Run => vec![
            // Landed long ago, nothing since.
            (run_id(1), false),
            // Its last record came long after its end.
            (run_id(3), true),
            // Landed in the periods.
            (run_id(12), true),
            // Legacy, from before the switch.
            (run_id(9), true),
            // A time missing, each way.
            (run_id(30), true),
            (run_id(31), true),
        ],
        LedgerKind::Task => vec![("1".to_owned(), false), ("2".to_owned(), true)],
        LedgerKind::Node => vec![
            (node::key(HOST_NODE, node::bucket_of(T0 + DAY)), false),
            (node::key(HOST_NODE, node::bucket_of(NOW - 3600)), true),
        ],
        _ => Vec::new(),
    }
}

/// The statement leaves out what [`RowsFrom::leaves_out`] does, of every kind
/// and whatever the page's size.
#[test]
fn the_store_returns_no_row_the_read_leaves_out() {
    let (_dir, queue, rows) = queue_of_rows();
    the_store_returns_as_the_read_leaves_out(&queue, &rows);
    // Whichever of a row's id and its times says it is of the periods.
    let (_dir, queue, rows) = queue_of(rows_read_from_a_clock_turned_back());
    let read = the_store_returns_as_the_read_leaves_out(&queue, &rows);
    for run in [40, 41, 44] {
        assert!(read.contains(&run_id(run)), "{run}");
    }
}

/// [`the_store_returns_no_row_the_read_leaves_out`] of the rows of `queue`,
/// with the run rows it read.
fn the_store_returns_as_the_read_leaves_out(
    queue: &SqliteQueue,
    rows: &[LedgerRow],
) -> BTreeSet<String> {
    let from = rows_from(queue, &read_from());
    assert!(from.inputs_through.as_i64() > 0);
    let mut runs = BTreeSet::new();
    for kind in LedgerKind::ALL {
        let mut read = BTreeSet::new();
        let mut after: Option<String> = None;
        loop {
            let page = queue
                .ledger_rows_from(kind, after.as_deref(), ROWS_PER_PAGE.min(3), &from)
                .unwrap();
            let Some(last) = page.last() else {
                break;
            };
            after = Some(last.key.clone());
            read.extend(page.into_iter().map(|row| row.key));
        }
        let want: BTreeSet<String> = rows
            .iter()
            .filter(|row| row.kind == kind && !from.leaves_out(row))
            .map(|row| row.key.clone())
            .collect();
        assert_eq!(read, want, "{kind:?}");
        for (key, read_it) in named_rows(kind) {
            assert_eq!(read.contains(&key), read_it, "{kind:?} {key}");
        }
        let out = rows
            .iter()
            .any(|row| row.kind == kind && from.leaves_out(row));
        assert_eq!(out, kind != LedgerKind::Queue, "{kind:?}");
        if kind == LedgerKind::Run {
            runs = read;
        }
    }
    runs
}

/// The statements read the times of a row's records as
/// [`RowsFrom::leaves_out`] does: what they leave out of the rows is what they
/// pick of, whatever the form of a time and whatever else its record says.
#[test]
fn the_statements_read_the_times_of_the_records_as_the_read_in_memory_does() {
    let from = RowsFrom {
        ended_before_ms: ms(1000),
        inputs_through: EventId::new(1_000),
    };
    let earlier = crate::domain::marks::utc_text(from.ended_before_ms - 1);
    let time = from.time_text();
    let claim = serde_json::json!({"from": "ready", "to": "in_progress"});
    let (recorded_at, at): (Vec<&str>, Vec<Option<serde_json::Value>>) = [
        (earlier.as_str(), None),
        (earlier.as_str(), Some(serde_json::json!(earlier))),
        (earlier.as_str(), Some(serde_json::Value::Null)),
        (time.as_str(), None),
        (earlier.as_str(), Some(serde_json::json!(time))),
        ("1970-01-01T00:00:00Z", None),
        ("1970-01-01 00:00:00.000Z", None),
        (earlier.as_str(), Some(serde_json::json!(1))),
    ]
    .into_iter()
    .unzip();
    let rows: Vec<LedgerRow> = recorded_at
        .iter()
        .zip(at)
        .enumerate()
        .map(|(index, (recorded_at, at))| {
            let run = run_id(u8::try_from(index + 1).unwrap());
            let mut log = crate::domain::ledger::testkit::Log::default();
            let mut payload = claim.clone();
            if let Some(at) = at {
                payload["at"] = at;
            }
            log.add(Some(1), Some(&run), "run_claimed", payload, 0);
            log.events[0].created_at = (*recorded_at).to_owned();
            let mut row = crate::domain::ledger::testkit::run_row(&run, None, Vec::new());
            row.started_at = Some(0);
            row.ended_at = Some(from.ended_before_ms - 1);
            row.records = log.events;
            row
        })
        .collect();
    let (_dir, queue, rows) = queue_of(rows);
    let left_out: BTreeSet<String> = rows
        .iter()
        .filter(|row| from.leaves_out(row))
        .map(|row| row.key.clone())
        .collect();
    assert_eq!(left_out.len(), 3, "{left_out:?}");
    let read: BTreeSet<String> = queue
        .ledger_rows_from(LedgerKind::Run, None, ROWS_PER_PAGE, &from)
        .unwrap()
        .into_iter()
        .map(|row| row.key)
        .collect();
    let every: BTreeSet<String> = rows.iter().map(|row| row.key.clone()).collect();
    assert_eq!(read, &every - &left_out);
    let picked: BTreeSet<String> = queue
        .ledger_earlier(&from)
        .unwrap()
        .records
        .iter()
        .filter_map(|event| event.run_id.as_ref())
        .map(|run| run.as_str().to_owned())
        .collect();
    assert_eq!(picked, left_out);
}

/// The store's statements give what the read of the same rows in memory
/// makes of them: what the rows left out give, picked and summed by SQL.
#[test]
fn the_store_picks_of_the_rows_left_out_what_the_read_in_memory_does() {
    let query = read_from();
    for rows in [
        rows_read_from_a_time(),
        rows_read_from_a_clock_turned_back(),
    ] {
        let (_dir, queue, rows) = queue_of(rows);
        let stored = queue.ledger_window(&query).unwrap();
        assert!(!stored.earlier().claims.is_empty());
        assert!(!stored.earlier().executions.is_empty());
        assert_eq!(held(&stored), held(&select_from_time(rows, &query)));
    }
}

/// [`LedgerStore`] of `queue` whose rows are written over by another
/// connection once the rows of the kinds are read: the reads after it are
/// what a builder could change under a read of many statements.
struct WrittenOverAfterThePages<'a> {
    queue: &'a SqliteQueue,
    writer: &'a SqliteQueue,
    row: LedgerRow,
}

impl LedgerStore for WrittenOverAfterThePages<'_> {
    fn ledger_read_snapshot(&self, read: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        self.queue.ledger_read_snapshot(read)
    }
    fn ledger_rows_after(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LedgerRow>> {
        self.queue.ledger_rows_after(kind, after, limit)
    }
    fn ledger_rows_from(
        &self,
        kind: LedgerKind,
        after: Option<&str>,
        limit: usize,
        from: &RowsFrom,
    ) -> Result<Vec<LedgerRow>> {
        self.queue.ledger_rows_from(kind, after, limit, from)
    }
    fn ledger_earlier(&self, from: &RowsFrom) -> Result<EarlierRecords> {
        assert!(self.writer.write_ledger_row(&self.row).unwrap());
        self.queue.ledger_earlier(from)
    }
    fn ledger_row(&self, kind: LedgerKind, key: &str) -> Result<Option<LedgerRow>> {
        self.queue.ledger_row(kind, key)
    }
    fn ledger_run_events(&self, run: &RunId) -> Result<Vec<RunEvent>> {
        self.queue.ledger_run_events(run)
    }
    fn write_ledger_row(&self, _: &LedgerRow) -> Result<bool> {
        unreachable!("only the reads of a window here")
    }
    fn write_ledger_row_over(&self, _: &LedgerRow, _: Option<&LedgerRow>) -> Result<Option<bool>> {
        unreachable!("only the reads of a window here")
    }
    fn write_session_rows_over(
        &self,
        _: &[(LedgerRow, Option<LedgerRow>)],
        _: Option<&SessionsWritten>,
    ) -> Result<bool> {
        unreachable!("only the reads of a window here")
    }
    fn sessions_written(&self) -> Result<Option<SessionsWritten>> {
        unreachable!("only the reads of a window here")
    }
    fn ledger_rows(&self, _: LedgerKind) -> Result<Vec<LedgerRow>> {
        unreachable!("only the reads of a window here")
    }
    fn ledger_legacy_runs_without_row(&self) -> Result<Vec<RunId>> {
        unreachable!("only the reads of a window here")
    }
    fn ledger_runs_claimed(&self, _: crate::domain::TaskId) -> Result<Vec<RunId>> {
        unreachable!("only the reads of a window here")
    }
    fn ledger_runs_past_their_rows(&self, _: EventId) -> Result<Vec<RunId>> {
        unreachable!("only the reads of a window here")
    }
    fn remove_node_rows(&self, _: i64) -> Result<usize> {
        unreachable!("only the reads of a window here")
    }
    fn last_event_naming_supervisor(&self, _: &str, _: EventId) -> Result<Option<RunEvent>> {
        unreachable!("only the reads of a window here")
    }
}

/// A row the builder moves past the bound between the read of the rows and
/// that of what they leave out is in the window as the read before it had it:
/// both reads are of one state.
#[test]
fn a_row_written_over_during_the_read_is_not_lost() {
    let (dir, queue, rows) = queue_of_rows();
    let query = read_from();
    let from = rows_from(&queue, &query);
    let session = rows
        .iter()
        .find(|row| row.kind == LedgerKind::Session && from.leaves_out(row))
        .unwrap();
    let mut moved = session.clone();
    moved.inputs_through.events = EventId::new(from.inputs_through.as_i64() + 1);
    let writer = SqliteQueue::open(dir.path().join("q.db")).unwrap();
    let before = held(&queue.ledger_window(&query).unwrap());
    let read = WrittenOverAfterThePages {
        queue: &queue,
        writer: &writer,
        row: moved,
    };
    let window = read.ledger_window(&query).unwrap();
    assert_eq!(held(&window), before);
    assert!(
        session
            .records
            .iter()
            .all(|record| window.events().iter().any(|event| event.id == record.id))
    );
}
