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
//! Beside it the store keeps the marks R ([`RowsWritten`], a row of
//! [`ROWS_KIND`]), independently for queue rows and for run, task, session
//! and node rows, each cursor its own ([`Cursors`]): a pass leaves some of
//! those rows to later passes past its bound of writes, so their heads do
//! not say it either. It is written with W in the same transaction
//! ([`Marks`]), its events no later than W, and a store with W alone gives
//! none ([`rows_written_through`]). R holds the latest time of an event read
//! up to its events, as a builder going on from it needs it and no event
//! says it; a mark that does not hold it for its position gives a builder
//! nothing to go on from for that kind ([`rows_to_resume`]).

use serde::{Deserialize, Serialize};

use crate::domain::EventId;
use crate::domain::ledger::{LEDGER_V, LedgerHead, LedgerKind};

pub mod omitted;

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
/// every input up to its kind's position, each cursor its own: every row
/// an input up to it made dirty is written. A pass writes it with the
/// session rows' mark ([`SessionsWritten`]) in one transaction, its
/// events no later than that mark's; each position is the pass's, or the
/// position before the first unrecorded input of a row the pass left
/// marked (past its bound of writes, a write that failed, a fold behind),
/// whichever is earlier, so that a builder going on from it reads that
/// input again and marks the row again
/// ([`crate::domain::ledger::builder::Builder::sessions_mark`]). A mark
/// that comes due with time (a row's grace over, a supervisor's silence)
/// holds no position: the rows not settled the store holds give it back.
///
/// The shared `through` is the conservative minimum for compatibility;
/// kind-specific positions and times are independent ([`RowPosition`]).
/// `read_ms` is the latest time (unix ms) of an event read up to
/// `through.events`, as the builder that wrote the mark held it at that
/// position ([`crate::domain::ledger::builder::Builder::folds_at`]; the
/// lowest `i64` with no event read). Events in the order of their ids need
/// not be in the order of their times, so the event at the position does
/// not say it, and every event up to it would. None in a mark written
/// before it was held: [`rows_to_go_on_from`] gives no position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowsWritten {
    /// Conservative position shared by both kinds; retained for old marks
    /// and readers. New certificates keep each independent position below.
    pub through: Cursors,
    /// Run, task, session and node certificate. Missing in a legacy mark:
    /// both kinds then start at `through` with `read_ms`, without a version
    /// change or rewriting rows. Each certificate covers only its own rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core: Option<RowPosition>,
    /// Queue certificate, independent of pending core writes and vice versa.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<RowPosition>,
    pub ledger_v: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_ms: Option<i64>,
    /// Rank of the next mark to check on restart, modulo the restored set.
    /// This schedules work across process lifetimes and certifies no input.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub take_offset: usize,
    /// Clock work left by a bounded prefix. Open rows must reach at least
    /// this horizon once the event snapshot is caught up; it holds no input
    /// cursor and remains fixed across restarts until that work drains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_pending: Option<i64>,
}

/// One kind's complete input prefix and the latest event time in that
/// prefix. Time travels with the position: event timestamps can go back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowPosition {
    pub through: Cursors,
    pub read_ms: i64,
}

impl RowsWritten {
    /// The usable restart certificate of this kind, falling back to the
    /// single R of a legacy mark through serde's absent-field defaults.
    pub fn position(self, queue: bool) -> Option<RowPosition> {
        (if queue { self.queue } else { self.core }).or_else(|| {
            self.read_ms.map(|read_ms| RowPosition {
                through: self.through,
                read_ms,
            })
        })
    }
}

/// This kind's R, only with a W covering its event position. Neither kind
/// borrows the other's position or latest time when the certificates differ.
pub fn rows_to_resume(
    marks: Option<&Marks>,
    heads: &[LedgerHead],
    queue: bool,
) -> Option<RowPosition> {
    let marks = marks?;
    let w = written_through(Some(&marks.sessions), heads)?;
    let rows = marks.rows.filter(|rows| rows.ledger_v == LEDGER_V)?;
    rows.position(queue).filter(|at| at.through.events <= w)
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

impl RowsWritten {
    /// The mark of `through` by this [`LEDGER_V`], `read_ms` the latest
    /// time of an event read up to its events.
    pub const fn at(through: Cursors, read_ms: i64) -> Self {
        Self {
            through,
            core: None,
            queue: None,
            ledger_v: LEDGER_V,
            read_ms: Some(read_ms),
            take_offset: 0,
            time_pending: None,
        }
    }
}

/// The store's marks a pass writes with the session rows in one
/// transaction: the session rows' (W), and with it the other rows' (R)
/// when the builder gives one. As it crosses to the queue service it
/// reads as the session rows' mark alone too (R's fields beside W's), so
/// that a reader of W alone reads it and one that gives no R is taken to
/// give none.
///
/// With them, when the builder holds one, goes the coverage of the
/// omitted queue events ([`omitted::Omitted`]): the store writes it in the
/// same transaction only over a coverage it supersedes
/// ([`omitted::Coverage::supersedes`]), and gives it back with the marks
/// it reads (its points apart).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marks {
    #[serde(flatten)]
    pub sessions: SessionsWritten,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<RowsWritten>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted: Option<omitted::Omitted>,
}

/// The conservative shared R (legacy readers), each of whose cursors the store's run, task, queue and
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

/// The position R a builder goes on from, with the latest time of an event
/// read up to its events: R ([`rows_written_through`]) when its mark holds
/// that time for that position. `None` otherwise, as for no R, when the
/// mark holds none (written before it did) or when W cut its events back
/// (the time it holds is of a later position). No event is read to find it
/// and no lower value is taken for it, so a builder then folds from its
/// cursors' start, and its writes write a mark that holds it. Pure.
pub fn rows_to_go_on_from(marks: Option<&Marks>, heads: &[LedgerHead]) -> Option<(Cursors, i64)> {
    let r = rows_written_through(marks, heads)?;
    let rows = marks?.rows?;
    let read_ms = rows.read_ms?;
    (r.events == rows.through.events).then_some((r, read_ms))
}

/// Attach clock work to the store's last usable certificate before prefix
/// rows are written. Preserve its positions and scheduling rank only when
/// startup could resume there ([`rows_to_go_on_from`]); otherwise certify
/// zero for W and every R input, with no event time read. W alone, marks of
/// another version, or a time of another position cannot certify a prefix.
/// The horizon never retreats within a usable certificate. Pure: the caller
/// writes the returned marks only when they differ from the stored ones.
pub fn marks_with_time_work(marks: Option<&Marks>, heads: &[LedgerHead], until: i64) -> Marks {
    let mut held = marks
        .cloned()
        .filter(|marks| rows_to_go_on_from(Some(marks), heads).is_some())
        .unwrap_or_else(|| Marks {
            sessions: SessionsWritten::at(EventId::new(0)),
            rows: Some(RowsWritten::at(Cursors::default(), i64::MIN)),
            omitted: None,
        });
    // The coverage moves with the reads only; the store keeps its own.
    held.omitted = None;
    let rows = held.rows.as_mut().expect("a usable R or zero");
    rows.time_pending = Some(
        rows.time_pending
            .map_or(until, |pending| pending.max(until)),
    );
    held
}

/// Why startup cannot resume at R. Labels describe absent marks, version
/// mismatch, missing read time, or W cutting R's time to another position.
/// None means the mark supplies the exact position and time for resumption.
pub fn restart_reason(marks: Option<&Marks>, heads: &[LedgerHead]) -> Option<&'static str> {
    let Some(marks) = marks else {
        return Some("no_marks");
    };
    let Some(rows) = marks.rows else {
        return Some("no_rows_mark");
    };
    if rows.ledger_v != LEDGER_V
        || marks.sessions.ledger_v != LEDGER_V
        || heads.iter().any(|head| head.ledger_v > LEDGER_V)
    {
        return Some("version_mismatch");
    }
    if rows.read_ms.is_none() {
        return Some("no_read_ms");
    }
    if rows_to_go_on_from(Some(marks), heads).is_none() {
        return Some("w_cuts_r");
    }
    None
}

#[cfg(test)]
mod tests;
