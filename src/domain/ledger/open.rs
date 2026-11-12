//! What a run row leaves open at its end: the slot, the asks, the sessions
//! and the landing's wait a run still holds however long ago its row
//! ended, so that a read from a time finds the run rows it leaves out and
//! still has to read whole without walking the settled history
//! (docs/plans/ledger-window-earlier-visibility-spike.md, "読みの約束" 3).
//!
//! The store keeps these in `ledger_open`, one row per thing left open of
//! a run row, written with the row each time the row is written: a close
//! that comes in a later pass writes the row again and so removes it. It is
//! a view of the events (ADR-t1662-2 decisions 1, 7 and 8): rebuilt of the
//! rows alone, it changes no row and no `ledger_v`. A store's mark
//! ([`Filled`]) says whether the rows written before it kept the table were
//! taken in: a reader of a store not filled reads every run row's earlier
//! records as before. The mark is kept through the generations of the
//! write leader whose writer kept the table: every process that writes the
//! queue takes the write leader over first, so a writer that keeps no table
//! (an older binary), whatever it wrote of the rows or the marks and
//! wherever it stopped, leaves a generation the mark is not kept through,
//! and the mark counts as none and the filling takes every run row in again.

use std::collections::{BTreeSet, HashSet};

use super::window::ends_run_occupancy;
use super::{LedgerKind, LedgerRow};
use crate::domain::run::{AskRef, restore_payload};
use crate::domain::sessions::SpanRef;
use crate::domain::stats::landing_utilization;
use crate::domain::{EventId, EventKind, RunEvent};

/// The run rows a part of the filling looks at, by key, at most: their
/// keys alone are read before any row.
pub const ROWS_PER_FILL: usize = 200;

/// The time a part of the filling takes its rows in for, in its write
/// transaction (ADR-t4652-1 decision 1): a share of the pass's
/// [`super::builder::PASS_TIME`], so that the filling holds the queue's
/// writes no longer than a part of what a pass spends on its own writes.
pub const FILL_TIME: std::time::Duration = std::time::Duration::from_millis(100);

/// The bytes of the records a part of the filling reads, at most past the
/// row that reaches it (ADR-t4652-1 decision 2): those of a normal group of
/// the session rows' writes ([`super::builder::SESSION_BYTES_PER_WRITE`]).
/// A row alone over it is taken alone, never skipped.
pub const FILL_BYTES: usize = super::builder::SESSION_BYTES_PER_WRITE;

/// Whether a part of the filling that read `bytes` of records over
/// `elapsed` ends after the row it took last: past [`FILL_BYTES`] or
/// [`FILL_TIME`].
pub fn part_spent(bytes: usize, elapsed: std::time::Duration) -> bool {
    bytes >= FILL_BYTES || elapsed >= FILL_TIME
}

/// One thing a run row leaves open at its end.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Left {
    /// A claim with no end of its slot ([`ends_run_occupancy`]).
    Slot,
    /// An ask with no answer, by [`AskRef::key`] (`None`: one naming no
    /// id), as [`super::window::WindowRows::take_earlier`] pairs them.
    Ask(Option<String>),
    /// A `session_opened` no `session_closed` of the row names by its
    /// `opened_event_id`.
    Session(EventId),
    /// A landing's wait or attempt the row's own records leave open: the
    /// last of them [`landing_utilization::reads`] queues or starts one.
    /// An attempt whose end was never recorded stays so here, though
    /// [`landing_utilization`]'s spans end it at the next run's start: a
    /// reader takes these beside the last start before its time.
    Landing,
}

impl Left {
    /// The text the store keys it by beside the row's kind and key: one
    /// per thing left open, read back by [`Self::of_text`].
    pub fn text(&self) -> String {
        match self {
            Self::Slot => "slot".to_owned(),
            Self::Ask(None) => "ask".to_owned(),
            Self::Ask(Some(key)) => format!("ask:{key}"),
            Self::Session(id) => format!("session:{}", id.as_i64()),
            Self::Landing => "landing".to_owned(),
        }
    }

    /// What [`Self::text`] wrote, `None` for text it does not write.
    pub fn of_text(text: &str) -> Option<Self> {
        match text {
            "slot" => Some(Self::Slot),
            "ask" => Some(Self::Ask(None)),
            "landing" => Some(Self::Landing),
            _ => {
                if let Some(key) = text.strip_prefix("ask:") {
                    Some(Self::Ask(Some(key.to_owned())))
                } else {
                    let id = text.strip_prefix("session:")?.parse().ok()?;
                    Some(Self::Session(EventId::new(id)))
                }
            }
        }
    }

    /// Whether a read from a time reads the run whole for it as
    /// [`super::window::WindowRows::take_earlier`] does: the landing's wait
    /// is read of the landings instead.
    pub const fn wants_the_run(&self) -> bool {
        !matches!(self, Self::Landing)
    }
}

/// What `row` leaves open at its end, of a run row's records with a run
/// (none of another kind of row): the rules of
/// [`super::window::WindowRows::take_earlier`] over the same records, a
/// session paired by the `opened_event_id` of its close as
/// [`crate::domain::stats::sessions::spans`] pairs it (a close naming none,
/// or one closed already, closes nothing more), and the landing's wait of
/// the row's own records ([`Left::Landing`]).
pub fn left_open(row: &LedgerRow) -> BTreeSet<Left> {
    left_open_of(row.kind, &row.records)
}

/// [`left_open`] of a row of `kind` whose records are `records` (ascending
/// id): those the store reads of it for the kinds and statuses
/// [`super::window::EarlierRecords`] picks of the run rows suffice.
pub fn left_open_of(kind: LedgerKind, records: &[RunEvent]) -> BTreeSet<Left> {
    let mut left = BTreeSet::new();
    if kind != LedgerKind::Run {
        return left;
    }
    let records = || records.iter().filter(|event| event.run_id.is_some());
    let (mut claimed, mut ended, mut waiting) = (false, false, false);
    let mut asked = BTreeSet::new();
    let mut answered = HashSet::new();
    let closed: HashSet<EventId> = records()
        .filter(|event| event.kind == EventKind::SessionClosed)
        .filter_map(|event| SpanRef::of(event).opened_event_id)
        .collect();
    for event in records() {
        ended |= ends_run_occupancy(event);
        if landing_utilization::reads(event) {
            waiting = matches!(
                event.kind.known(),
                Some(EventKind::LandingQueued | EventKind::IntegrationStarted)
            );
        }
        match event.kind.known() {
            Some(EventKind::RunClaimed) => claimed = true,
            // An answer closes its ask whichever comes first, as the
            // window's pairing keeps an answered ask closed.
            Some(EventKind::AskOpened) => {
                asked.insert(restore_payload::<AskRef>(&event.payload).key());
            }
            Some(EventKind::AskAnswered) => {
                answered.insert(restore_payload::<AskRef>(&event.payload).key());
            }
            Some(EventKind::SessionOpened) if !closed.contains(&event.id) => {
                left.insert(Left::Session(event.id));
            }
            _ => {}
        }
    }
    if claimed && !ended {
        left.insert(Left::Slot);
    }
    left.extend(
        asked
            .into_iter()
            .filter(|key| !answered.contains(key))
            .map(Left::Ask),
    );
    if waiting {
        left.insert(Left::Landing);
    }
    left
}

/// How far the store took the run rows written before it kept
/// `ledger_open` into it: none yet, the rows up to a key, or all of them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Filled {
    /// No run row taken in yet: a reader reads every run row's records.
    Not,
    /// The run rows up to this key, by key, taken in.
    Through(String),
    /// Every run row taken in: the table is that of the rows.
    Done,
}

impl Filled {
    /// Whether a reader may take the table for that of every run row.
    pub const fn is_done(&self) -> bool {
        matches!(self, Self::Done)
    }

    /// The key past which the next pass of the filling takes the rows in,
    /// `None` from the first; none once done.
    pub fn after(&self) -> Option<Option<&str>> {
        match self {
            Self::Not => Some(None),
            Self::Through(key) => Some(Some(key)),
            Self::Done => None,
        }
    }

    /// The mark a store holds (how far, and the last generation of the
    /// write leader it is kept through, if any) as a reader takes it while
    /// the write leader is of the generation `leader`: none but one kept
    /// through that generation. A mark of no generation, or of another,
    /// counts as none, so that a writer that kept no view in between
    /// leaves the reader to the whole read.
    pub fn held(mark: Option<(Self, Option<i64>)>, leader: i64) -> Self {
        match mark {
            Some((filled, Some(generation))) if generation == leader => filled,
            _ => Self::Not,
        }
    }

    /// What a writer of the generation `leader` that keeps the view does
    /// with the mark it holds on its write: one of its own generation is
    /// held, one of the generation just before carried on to it (no writer
    /// came between), any other removed (none, or one that kept no view,
    /// came between).
    pub fn kept(mark: Option<(Self, Option<i64>)>, leader: i64) -> Kept {
        match mark {
            None => Kept::Held(Self::Not),
            Some((filled, Some(generation))) if generation == leader => Kept::Held(filled),
            Some((filled, Some(generation))) if generation.checked_add(1) == Some(leader) => {
                Kept::CarriedOn(filled)
            }
            Some(_) => Kept::Behind,
        }
    }

    /// What a part of the filling leaves that read `keys` keys past
    /// [`Self::after`] and took in the first `taken` of them, the last
    /// `last`: done when it took every key it read and they were fewer
    /// than [`ROWS_PER_FILL`], as no row is past them; else through the
    /// last it took.
    pub fn next(keys: usize, taken: usize, last: Option<&str>) -> Self {
        match last {
            Some(key) if taken < keys || keys >= ROWS_PER_FILL => Self::Through(key.to_owned()),
            _ => Self::Done,
        }
    }
}

/// What [`Filled::kept`] does with a mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kept {
    /// The mark stays as it is (none: none).
    Held(Filled),
    /// The mark is carried on to the writer's generation.
    CarriedOn(Filled),
    /// The mark is removed and the filling begins again.
    Behind,
}

#[cfg(test)]
pub(crate) mod tests;
