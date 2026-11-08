//! The push records of the run rows read a page of rows at a time, by
//! the rows' keys ([`PushRecordsRead`], [`gather`]): each page reads the
//! rows of a range of keys, or the rest of one row whose records one page
//! does not hold, and no page reads the rows of another again. The records
//! are those [`records_of_row`] keeps, cut ([`super::cut`]); gathered,
//! they are put in the order of their ids.

use serde::{Deserialize, Serialize};

use super::{cut, is_push_record};
use crate::domain::ledger::LedgerRow;
use crate::domain::{EventId, EventKind, RunEvent};

/// Where a page of the push records of the run rows ended, and the next
/// one goes on from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushRecordsAt {
    /// The key of the last run row the page read.
    pub key: String,
    /// `Some(id)`: the page cut that row after its push record `id`, and
    /// its records after it are still to read
    /// ([`PushRecordsRead::RestOfRow`]); `None`: the page read the whole
    /// row, and the next page starts at the rows after it.
    pub cut: Option<EventId>,
}

/// Which push records of the run rows a page reads (the supervisor's
/// operation `ledger_push_records_page`): rows by key, or one row in parts
/// when its records past one answer. Every limit counts what one answer
/// holds: rows for whole rows, records within a row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushRecordsRead {
    /// The push records of the first `rows` run rows after the key
    /// `after` (`None`: from the first row), by key; a row without a push
    /// record counts among them.
    Rows { after: Option<String>, rows: usize },
    /// The first `records` push records of the first run row after the key
    /// `after` (`None`: the first row), by id.
    FirstOfRow {
        after: Option<String>,
        records: usize,
    },
    /// The next `records` push records after the record `after` of the run
    /// row `key`, by id.
    RestOfRow {
        key: String,
        after: EventId,
        records: usize,
    },
}

/// A page of the push records of the run rows: the records, cut, by id
/// within the page, how many rows it read (those without a push record
/// too, so the end of the rows is the end of the records whatever the
/// rows hold) and where it ended.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushRecordsPage {
    pub records: Vec<RunEvent>,
    /// The run rows read: fewer than [`PushRecordsRead::Rows`] asked for,
    /// or none of [`PushRecordsRead::FirstOfRow`], is the end of the rows.
    pub rows: usize,
    /// `None` when the page read no row.
    pub at: Option<PushRecordsAt>,
}

/// Of the run row `row`, the records the summaries of the pushes are made
/// again from, each cut, by id: every `push_finished`, and when the row
/// records a `push_failed` its `push_failed` and `run_phase_changed` too
/// (no other run waits on a push).
pub fn records_of_row(row: &LedgerRow) -> Vec<RunEvent> {
    let failed = row
        .records
        .iter()
        .any(|event| event.kind == EventKind::PushFailed);
    let mut records: Vec<RunEvent> = row
        .records
        .iter()
        .filter(|event| is_push_record(event) && (failed || event.kind == EventKind::PushFinished))
        .map(cut)
        .collect();
    records.sort_by_key(|event| event.id);
    records
}

impl PushRecordsPage {
    /// The page of the whole rows `rows`, by key: their records, and the
    /// last row read whole.
    pub fn of_rows(rows: &[LedgerRow]) -> Self {
        let mut records: Vec<RunEvent> = rows.iter().flat_map(records_of_row).collect();
        records.sort_by_key(|event| event.id);
        Self {
            records,
            rows: rows.len(),
            at: rows.last().map(|row| PushRecordsAt {
                key: row.key.clone(),
                cut: None,
            }),
        }
    }

    /// The page of the row `key` (`rows`: 1 when it was read, 0 when it is
    /// gone) of at most `limit` of its records `part` (by id, each after
    /// where the page starts; more of them than `limit` say that the row
    /// goes on after the page).
    pub fn in_row(key: String, rows: usize, mut part: Vec<RunEvent>, limit: usize) -> Self {
        let limit = limit.max(1);
        let cut = (part.len() > limit).then(|| {
            part.truncate(limit);
            part[limit - 1].id
        });
        Self {
            records: part,
            rows,
            at: Some(PushRecordsAt { key, cut }),
        }
    }
}

/// Every push record of the run rows, by id, read by `read` a page at a
/// time: `rows_page` whole rows a page, a page past the answer's limit
/// (`too_large`) asked again at a quarter of its rows, and a row past it
/// alone read in parts of `row_part` records (a quarter of it again for a
/// part past the limit) from where the part before ended, after which the
/// pages of `rows_page` rows go on. Each page starts after the row or the
/// record the page before ended at, so the rows are read once, but for a
/// row read in parts, read once a part. The records are put in the order
/// of their ids once all are read.
pub fn gather<E>(
    rows_page: usize,
    row_part: usize,
    mut read: impl FnMut(&PushRecordsRead) -> Result<PushRecordsPage, E>,
    too_large: impl Fn(&E) -> bool,
) -> Result<Vec<RunEvent>, E> {
    let (mut records, mut at, mut rows, mut part) = (
        Vec::new(),
        None::<PushRecordsAt>,
        rows_page.max(1),
        None::<usize>,
    );
    loop {
        let after = at.as_ref().map(|at| at.key.clone());
        let ask = match (&at, part) {
            (
                Some(PushRecordsAt {
                    key,
                    cut: Some(cut),
                }),
                _,
            ) => PushRecordsRead::RestOfRow {
                key: key.clone(),
                after: *cut,
                records: part.unwrap_or(row_part).max(1),
            },
            (_, Some(records)) => PushRecordsRead::FirstOfRow { after, records },
            (_, None) => PushRecordsRead::Rows { after, rows },
        };
        let page = match read(&ask) {
            Err(error) if too_large(&error) => {
                match ask {
                    PushRecordsRead::Rows { rows: asked, .. } if asked > 1 => {
                        rows = (asked / 4).max(1);
                    }
                    PushRecordsRead::Rows { .. } => part = Some(row_part.max(1)),
                    PushRecordsRead::FirstOfRow { records: asked, .. }
                    | PushRecordsRead::RestOfRow { records: asked, .. }
                        if asked > 1 =>
                    {
                        part = Some((asked / 4).max(1))
                    }
                    _ => return Err(error),
                }
                continue;
            }
            page => page?,
        };
        let ended = match &ask {
            PushRecordsRead::Rows { rows: asked, .. } => page.rows < *asked || page.at.is_none(),
            PushRecordsRead::FirstOfRow { .. } => page.rows == 0 || page.at.is_none(),
            PushRecordsRead::RestOfRow { .. } => false,
        };
        records.extend(page.records);
        if ended {
            records.sort_by_key(|event: &RunEvent| event.id);
            return Ok(records);
        }
        at = match (ask, page.at) {
            (_, Some(next)) => Some(next),
            (PushRecordsRead::RestOfRow { key, .. }, None) => {
                Some(PushRecordsAt { key, cut: None })
            }
            (_, None) => at,
        };
        if part.is_some() && at.as_ref().is_some_and(|at| at.cut.is_none()) {
            (part, rows) = (None, rows_page.max(1));
        }
    }
}

#[cfg(test)]
mod tests;
