//! Where the store's session rows record every session event up to (W,
//! docs/design/measurement.md "台帳を作る係"): a builder going on from the
//! session rows the store holds ([`super::SessionsHeld`],
//! [`crate::domain::ledger::builder::Builder::restore_sessions`]) writes the rows of the
//! fold of every event only from such a position, and the rows' heads do
//! not say which one that is: a session row not written yet, or a record
//! not written to a row yet, is in no head, as an event of no session is.
//!
//! So the store keeps a mark of it ([`SessionsWritten`]), one row of its
//! own kind ([`KIND`]) in the ledger's table, which no reader of the rows
//! by their kind reads. A pass writes every session row it changed, in
//! transactions of a bounded number of rows
//! (`LedgerStore::write_session_rows_over`), and the mark of the position
//! it read up to with the last of them, so that the mark moves only once
//! all of them are written (one written before a later one fails only adds
//! records: the mark held from before stays true); only a builder whose
//! session book is the fold of every session event (one from its cursors'
//! start, or one gone on from a mark) writes the mark
//! ([`crate::domain::ledger::builder::Builder::sessions_mark`]). A store with no mark (one
//! the rows were written one by one into, by an older binary or before the
//! mark) gives no position ([`written_through`]): a builder then folds the
//! session events from its cursors' start, and its first pass's writes
//! fill what the store's rows lack and write the mark.
//!
//! Beside it the store keeps the mark R ([`RowsWritten`], a row of
//! [`ROWS_KIND`]) of where its run, task, queue and node rows record every
//! input up to, each cursor its own ([`Cursors`]): a pass leaves some of
//! those rows to later passes past its bound of writes, so their heads do
//! not say it either. It is written with W in the same transaction
//! ([`Marks`]), its events no later than W, and a store with W alone gives
//! none ([`rows_written_through`]).

use serde::{Deserialize, Serialize};

use crate::domain::EventId;
use crate::domain::ledger::{LEDGER_V, LedgerHead, LedgerKind};

/// The kind of the mark's row in the ledger's table: no [`LedgerKind`], so
/// that no reader of the rows of a kind reads it.
pub const KIND: &str = "sessions_written";

/// The key of the mark's row: the store holds one.
pub const KEY: &str = "";

/// The store's mark that its session rows record every session event up
/// to the event `through`, written by a fold of version `ledger_v` in the
/// transaction that wrote the session rows of a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionsWritten {
    pub through: EventId,
    pub ledger_v: u32,
}

impl SessionsWritten {
    /// The mark of the event `through` by this [`LEDGER_V`].
    pub const fn at(through: EventId) -> Self {
        Self {
            through,
            ledger_v: LEDGER_V,
        }
    }
}

/// The position W the store's session rows record every session event up
/// to, by its `mark` and the heads of its session rows (`heads`): the
/// mark's position, when there is a mark by this [`LEDGER_V`] and no
/// session row was folded by a later version (a fold whose rows this one
/// does not know). `None` otherwise: a store with no mark, whatever its
/// rows' `inputs_through`, is not taken to record every event up to any
/// of them (a session row the store lacks has no head). On `None` a
/// builder does not go on from the rows' heads: it folds the session
/// events from its cursors' start (cursor 0). Going on from `Some(W)` is
/// safe whatever was written after the mark: [`super::SessionsHeld::at`]
/// cuts each row's records after W, and the builder reads the events
/// after W again.
pub fn written_through(mark: Option<&SessionsWritten>, heads: &[LedgerHead]) -> Option<EventId> {
    let mark = mark?;
    let later = heads
        .iter()
        .any(|head| head.kind == LedgerKind::Session && head.ledger_v > LEDGER_V);
    (mark.ledger_v == LEDGER_V && !later).then_some(mark.through)
}

/// The kind of the row of the store's mark of where its run, task, queue
/// and node rows record every input up to ([`RowsWritten`]): no
/// [`LedgerKind`] either, its key [`KEY`].
pub const ROWS_KIND: &str = "rows_written";

/// A position in each input what builds the ledger reads, after its
/// cursors ([`crate::domain::ledger::builder::Builder`]): an EventStore
/// event id, a NodeSampleStore id, a SessionStepStore id, and the ids of
/// the runs' resources measured from outside and from inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Cursors {
    pub events: EventId,
    pub samples: i64,
    pub steps: i64,
    pub resources: i64,
    pub inside_resources: i64,
}

/// The store's mark (R) that its run, task, queue and node rows record
/// every input up to `through`, each cursor its own position: every row
/// an input up to it made dirty is written. A pass writes it with the
/// session rows' mark ([`SessionsWritten`]) in one transaction, its
/// events no later than that mark's; each position is the pass's, or the
/// position before the first input that made dirty a row the pass left
/// marked (past its bound of writes, a write that failed, a fold behind),
/// whichever is earlier, so that a builder going on from it reads that
/// input again and marks the row again
/// ([`crate::domain::ledger::builder::Builder::sessions_mark`]). A mark
/// that comes due with time (a row's grace over, a supervisor's silence)
/// holds no position: the rows not settled the store holds give it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowsWritten {
    pub through: Cursors,
    pub ledger_v: u32,
}

impl RowsWritten {
    /// The mark of `through` by this [`LEDGER_V`].
    pub const fn at(through: Cursors) -> Self {
        Self {
            through,
            ledger_v: LEDGER_V,
        }
    }
}

/// The store's marks a pass writes with the session rows in one
/// transaction: the session rows' (W), and with it the other rows' (R)
/// when the builder gives one. As it crosses to the queue service it
/// reads as the session rows' mark alone too (R's fields beside W's), so
/// that a reader of W alone reads it and one that gives no R is taken to
/// give none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marks {
    #[serde(flatten)]
    pub sessions: SessionsWritten,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<RowsWritten>,
}

/// The position R each of whose cursors the store's run, task, queue and
/// node rows record every input up to, by its `marks` and the heads of
/// its session rows (`heads`): R's positions, its events no later than
/// the session rows' W ([`written_through`]), when there is a W and an R
/// by this [`LEDGER_V`]. `None` otherwise: a store with no mark, or with
/// the session rows' mark alone (written before R was, or by a builder
/// whose books are not the fold of every input), gives no position, and a
/// builder then folds from its cursors' start. Pure: it reads nothing.
pub fn rows_written_through(marks: Option<&Marks>, heads: &[LedgerHead]) -> Option<Cursors> {
    let marks = marks?;
    let w = written_through(Some(&marks.sessions), heads)?;
    let rows = marks.rows.filter(|rows| rows.ledger_v == LEDGER_V)?;
    Some(Cursors {
        events: rows.through.events.min(w),
        ..rows.through
    })
}

#[cfg(test)]
mod tests;
