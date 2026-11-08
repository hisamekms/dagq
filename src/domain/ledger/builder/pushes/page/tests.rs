use std::cell::RefCell;

use super::*;
use crate::domain::RunId;
use crate::domain::ledger::testkit::run_row;

/// A run's record `id` of `kind`, with more in its payload than the cut
/// keeps.
fn record(id: i64, kind: EventKind) -> RunEvent {
    RunEvent {
        id: EventId::new(id),
        task_id: None,
        goal_id: None,
        run_id: Some(RunId::new("00000000-1111-4111-8111-111111111111").unwrap()),
        kind: kind.into(),
        payload: serde_json::json!({"phase": "p", "remote": "o", "branch": "b", "extra": id}),
        created_at: "2026-11-01T00:00:00Z".to_owned(),
        actor: None,
    }
}

/// Run rows of every mix, by key: `push_finished` alone, a `push_failed`
/// with many phases, phases alone, none, and the ids interleaved across
/// them.
fn rows() -> Vec<LedgerRow> {
    use EventKind::{PushFailed, PushFinished, RunClaimed, RunPhaseChanged};
    let row = |key: &str, records: Vec<(i64, EventKind)>| LedgerRow {
        records: records
            .into_iter()
            .map(|(id, kind)| record(id, kind))
            .collect(),
        ..run_row(key, None, Vec::new())
    };
    vec![
        row("a", vec![(1, RunClaimed), (7, PushFinished)]),
        row(
            "b",
            (20..30)
                .map(|id| (id, RunPhaseChanged))
                .chain([(2, PushFailed), (40, PushFinished)])
                .collect(),
        ),
        row("c", vec![(3, RunPhaseChanged), (8, RunClaimed)]),
        row("d", vec![(4, RunClaimed)]),
        row("e", vec![(5, PushFinished), (6, RunPhaseChanged)]),
    ]
}

/// A read of the pages of `rows`, as a store answers them, refusing a page
/// of more than `limit` records as past the answer's limit (`None`), and
/// counting the rows each page answered read.
fn pages_of(
    rows: &[LedgerRow],
    limit: usize,
    read: &RefCell<usize>,
) -> impl FnMut(&PushRecordsRead) -> Result<PushRecordsPage, Option<()>> {
    move |page| {
        let after = |row: &&LedgerRow, after: &Option<String>| {
            after.as_ref().is_none_or(|after| row.key > *after)
        };
        let answer = match page {
            PushRecordsRead::Rows {
                after: from,
                rows: n,
            } => {
                let held: Vec<LedgerRow> = rows
                    .iter()
                    .filter(|row| after(row, from))
                    .take(*n)
                    .cloned()
                    .collect();
                PushRecordsPage::of_rows(&held)
            }
            PushRecordsRead::FirstOfRow {
                after: from,
                records,
            } => match rows.iter().find(|row| after(row, from)) {
                Some(row) => {
                    PushRecordsPage::in_row(row.key.clone(), 1, records_of_row(row), *records)
                }
                None => PushRecordsPage::default(),
            },
            PushRecordsRead::RestOfRow {
                key,
                after,
                records,
            } => {
                let row = rows.iter().find(|row| row.key == *key).unwrap();
                let rest = records_of_row(row)
                    .into_iter()
                    .filter(|r| r.id > *after)
                    .collect();
                PushRecordsPage::in_row(key.clone(), 1, rest, *records)
            }
        };
        if answer.records.len() > limit {
            return Err(None);
        }
        *read.borrow_mut() += answer.rows;
        Ok(answer)
    }
}

/// Every push record of the rows, by id, as the whole read gives them.
fn whole(rows: &[LedgerRow]) -> Vec<RunEvent> {
    let mut records: Vec<RunEvent> = rows.iter().flat_map(records_of_row).collect();
    records.sort_by_key(|record| record.id);
    records
}

/// A row keeps every `push_finished`, and its `push_failed` and phases
/// only when it records a `push_failed`, cut and by id.
#[test]
fn a_row_keeps_its_phases_only_with_a_failed_push() {
    let rows = rows();
    let ids = |row: &LedgerRow| -> Vec<i64> {
        records_of_row(row).iter().map(|r| r.id.as_i64()).collect()
    };
    assert_eq!(ids(&rows[0]), [7]);
    assert_eq!(
        ids(&rows[1]),
        [2, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 40]
    );
    assert!(ids(&rows[2]).is_empty());
    assert_eq!(ids(&rows[4]), [5]);
    assert!(
        records_of_row(&rows[1])
            .iter()
            .all(|r| r.payload.get("extra").is_none())
    );
}

/// Gathered a page of rows at a time, the records are the whole read's,
/// whatever the rows a page asks for, and each row is read once: the rows
/// without a push record (`c`, `d`) end no read early. A page past the
/// limit is asked again with fewer rows, and the row past it alone (`b`)
/// in parts from where each part ended, after which whole rows go on.
#[test]
fn the_pages_gathered_are_the_whole_read_each_row_read_once() {
    let rows = rows();
    for rows_page in [1, 2, 3, 5, 1_000] {
        let read = RefCell::new(0);
        let got = gather(
            rows_page,
            4,
            pages_of(&rows, usize::MAX, &read),
            Option::is_none,
        )
        .unwrap();
        assert_eq!(got, whole(&rows), "{rows_page}");
        assert_eq!(*read.borrow(), rows.len(), "{rows_page}");
    }
    let (read, asked) = (RefCell::new(0), RefCell::new(Vec::new()));
    let mut pages = pages_of(&rows, 4, &read);
    let got = gather(
        8,
        16,
        |page| {
            asked.borrow_mut().push(page.clone());
            pages(page)
        },
        Option::is_none,
    )
    .unwrap();
    assert_eq!(got, whole(&rows));
    let asked = asked.into_inner();
    let kinds: Vec<&str> = asked
        .iter()
        .map(|page| match page {
            PushRecordsRead::Rows { .. } => "rows",
            PushRecordsRead::FirstOfRow { .. } => "first",
            PushRecordsRead::RestOfRow { .. } => "rest",
        })
        .collect();
    assert_eq!(
        asked[0],
        PushRecordsRead::Rows {
            after: None,
            rows: 8
        }
    );
    assert!(
        kinds.contains(&"first") && kinds.contains(&"rest"),
        "{asked:?}"
    );
    assert_eq!(
        asked.last(),
        Some(&PushRecordsRead::Rows {
            after: Some("b".to_owned()),
            rows: 8
        }),
        "{asked:?}"
    );
    // Row b read in its three parts, every other row once.
    assert_eq!(*read.borrow(), rows.len() + 2);
}

/// A part of one record still past the limit is the error, not a loop.
#[test]
fn a_record_past_the_limit_alone_is_the_error() {
    let rows = rows();
    let read = RefCell::new(0);
    assert_eq!(
        gather(4, 4, pages_of(&rows, 0, &read), Option::is_none),
        Err(None)
    );
}

/// No row is an empty read of one page.
#[test]
fn no_row_is_one_empty_page() {
    let read = RefCell::new(0);
    let mut asked = 0;
    let mut pages = pages_of(&[], usize::MAX, &read);
    let got = gather(
        10,
        10,
        |page| {
            asked += 1;
            pages(page)
        },
        Option::is_none,
    )
    .unwrap();
    assert!(got.is_empty());
    assert_eq!(asked, 1);
}
