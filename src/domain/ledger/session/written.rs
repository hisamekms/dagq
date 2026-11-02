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

#[cfg(test)]
mod tests;
