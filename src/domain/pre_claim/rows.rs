//! The supervisors' lives ([`Lives`]) and the waits before a claim
//! ([`PreClaimBook`]) made again from the queue rows the store holds
//! (docs/design/measurement.md "台帳を作る係"), so that what builds the
//! ledger goes on from a position with the books of every event up to it
//! and reads no event of a wait or a life settled. A queue row of a wait
//! keeps the wait as it was folded (`detail.interval`) and the last event
//! it reads (`inputs_through`); a row a supervisor's life reaches keeps the
//! last event naming it (`inputs_through.last()`); a stop is a record of
//! some queue row. What the books keep of them:
//!
//! - **The waits open, and every slots full not freed**: a slots full its
//!   supervisor's life ended is kept closed by that life, as the pass that
//!   folded its row found it, and a later event naming the supervisor opens
//!   it again. A slots full not freed reads up to the last event naming its
//!   supervisor, the row's `inputs_through.events`.
//! - **The waits whose rows are not settled**: their grace folds them
//!   again, as the book gives them.
//! - **The waits an event ended, from [`Since`] on**: the waits that bear
//!   on the time ready of a task not settled ([`crate::domain::ledger::fold::fold_task`]),
//!   those that ended at or after the earliest start of such a task or by
//!   an event at or after its first; and the holds a stop ended, which bear
//!   on every time ready after their start. A wait ended before is a
//!   settled task's, which its row keeps.
//! - **The lives**: each supervisor that stopped, by its stops, and each
//!   supervisor of a row its life reaches (a slots full not freed, a stream
//!   not replaced, a hold its stop ended), by the last event naming it; the
//!   latest time of its events is the end of its silence when a row says
//!   its life ended, else that of its last event ([`living`], read by id).
//!   A supervisor of none of them goes on from the events after the
//!   position.
//!
//! So the books are those of every event but for the waits that ended
//! before [`Since`], and the book says it holds from there
//! ([`PreClaimBook::held_from`]). A task whose fold may read such a wait
//! (one that starts before [`Since`]: a row settled that a later event of
//! its own folds again, a task that has no row at the position because a
//! builder's writes lag its cursor, or one that becomes ready after it at
//! the very time a wait ended before it) reads the waits settled over its
//! own time from the store's rows ([`lacks`], [`waits`]) as it is folded,
//! not kept between folds, so its row is that of every event. The latest
//! times of a life are those of its last event, whose clock was not set
//! back.
//!
//! The rows may be written at a later position than the one the books are
//! made at ([`books`]'s `at`): a row with something past it is first cut
//! there ([`crate::domain::ledger::queue::cut`]), the lives then going on
//! from the last event naming each supervisor by `at` and the stops by
//! then. The rows taken are those [`holds_at`] keeps for the [`Since`] of
//! the position, which is no later than the exact one, so the books may
//! hold besides waits that ended before the exact [`Since`].

use std::collections::{BTreeMap, BTreeSet};

use super::{ENDED_FREED, IntervalKind, PreClaimBook, PreClaimInterval, Started};
use crate::domain::claim_hold::ENDED_SUPERVISOR_GONE;
use crate::domain::ledger::queue::{QueueHead, cut};
use crate::domain::ledger::{LedgerKind, LedgerRow};
use crate::domain::stats::timestamp_millis;
use crate::domain::supervisor_life::{Lives, supervisor_of};
use crate::domain::{EventId, EventKind, RunEvent};

/// The subject kind of a supervisor's stream of the judgments of now.
const STREAM: &str = "live_alert_stream";

/// From where the waits an event ended bear on a task not settled: the
/// earliest start (unix ms) and the first event of the task rows the store
/// holds not settled. With none, no wait ended so far bears on a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Since {
    pub ms: i64,
    pub id: EventId,
}

impl Since {
    /// None bears.
    pub const NONE: Self = Self {
        ms: i64::MAX,
        id: EventId::new(i64::MAX),
    };

    /// Of the task rows `rows`: their earliest start and first event of
    /// those not settled.
    pub fn of<'r>(rows: impl IntoIterator<Item = &'r LedgerRow>) -> Self {
        rows.into_iter()
            .filter(|row| row.kind == LedgerKind::Task && !row.is_final)
            .fold(Self::NONE, |since, row| Self {
                ms: since.ms.min(row.started_at.unwrap_or(i64::MIN)),
                id: since.id.min(row.inputs_from),
            })
    }
}

/// The position of the rows as they were written, whatever it is: no row
/// is past it, so [`books`] cuts none.
pub const AS_WRITTEN: EventId = EventId::new(i64::MAX);

/// What a queue row of a wait is of, by its subject's kind.
fn started(kind: &str) -> Option<Started> {
    Some(match kind {
        "slots_full" => Started::PreClaim(IntervalKind::SlotsFull),
        "claim_hold" => Started::PreClaim(IntervalKind::ClaimHold),
        "claim_deferral" => Started::PreClaim(IntervalKind::ClaimDeferral),
        "landing_hold" => Started::LandingHold,
        _ => return None,
    })
}

fn subject_kind(row: &LedgerRow) -> Option<&str> {
    row.subject.as_ref().map(|subject| subject.kind.as_str())
}

fn life_ended(row: &LedgerRow) -> bool {
    row.outcome.as_deref() == Some(ENDED_SUPERVISOR_GONE)
}

/// Whether the books are made again from `row` ([`books`]): a queue row
/// of a wait open, not settled (its grace folds it again), ended by its
/// supervisor's life, or ended from `since` on; or of a stream not
/// replaced. The store gives these, without their
/// records ([`crate::application::LedgerStore::ledger_wait_rows`]).
pub fn holds(row: &LedgerRow, since: Since) -> bool {
    if row.kind != LedgerKind::Queue {
        return false;
    }
    match subject_kind(row) {
        Some(STREAM) => row.outcome.is_none() || life_ended(row),
        Some(kind) if started(kind).is_some() => {
            !row.is_final
                || row.ended_at.is_none_or(|end| end >= since.ms)
                || life_ended(row)
                || row.inputs_through.events >= since.id
        }
        _ => false,
    }
}

/// Whether the books made again at the position `at` ([`books`]) are made
/// from `row`, one of the rows the store holds written at a later
/// position: one [`holds`] keeps for `since` (that of the position,
/// [`crate::application::LedgerStore::ledger_tasks_open_since_at`]), or a
/// wait's or a stream's that read past `at`, which may be open there.
pub fn holds_at(row: &LedgerRow, since: Since, at: EventId) -> bool {
    holds(row, since)
        || (row.kind == LedgerKind::Queue
            && subject_kind(row).is_some_and(|kind| kind == STREAM || started(kind).is_some())
            && row.inputs_through.last() > at)
}

/// Whether `row` is of a wait before a claim settled that a book made
/// again from the rows [`holds`] keeps from `held` lacks ([`books`]: ended
/// by an event before it, by time and by id) and that bears on the time
/// ready of a task from `from` (its earliest time and first event) up to
/// `until_ms` (the end of its time, or the time folded to): one that
/// started by then and ended at or after the task's start or by an event
/// at or after its first, as [`crate::domain::ledger::fold::fold_task`]
/// reads them. The store gives these, without their records
/// ([`crate::application::LedgerStore::ledger_settled_waits`]), for the
/// fold of a task that started before `held`, so that its row is that of
/// every event though the book keeps none of them.
pub fn lacks(row: &LedgerRow, held: Since, from: Since, until_ms: i64) -> bool {
    row.kind == LedgerKind::Queue
        && matches!(
            subject_kind(row).and_then(started),
            Some(Started::PreClaim(_))
        )
        && !holds(row, held)
        && row.started_at.is_some_and(|start| start <= until_ms)
        && (row.ended_at.is_some_and(|end| end >= from.ms) || row.inputs_through.events >= from.id)
}

/// The waits of `rows` (queue rows of waits before a claim, those [`lacks`]
/// keeps), as a book gives a wait an event ended: the last event each
/// reads is its row's.
pub fn waits(rows: &[LedgerRow]) -> Vec<PreClaimInterval> {
    rows.iter()
        .filter(|row| {
            matches!(
                subject_kind(row).and_then(started),
                Some(Started::PreClaim(_))
            )
        })
        .filter_map(wait_of)
        .collect()
}

/// The supervisor a row's life reaches, when one does: a slots full not
/// freed, a hold its stop ended (their supervisor) and a stream not
/// replaced (its own).
fn reached_by(row: &LedgerRow, interval: Option<&PreClaimInterval>) -> Option<String> {
    match subject_kind(row)? {
        STREAM => (row.outcome.is_none() || life_ended(row))
            .then(|| row.subject.as_ref().map(|subject| subject.id.clone()))
            .flatten(),
        "slots_full" => interval
            .filter(|wait| wait.end.as_deref() != Some(ENDED_FREED))
            .and_then(|wait| wait.supervisor.clone()),
        _ if life_ended(row) => interval.and_then(|wait| wait.supervisor.clone()),
        _ => None,
    }
}

/// The wait a row folded, as the book holds it: the last event it reads
/// is the row's.
fn wait_of(row: &LedgerRow) -> Option<PreClaimInterval> {
    let mut wait: PreClaimInterval =
        serde_json::from_value(row.detail.get("interval")?.clone()).ok()?;
    wait.reach = row.inputs_through.events;
    Some(wait)
}

/// What a supervisor's rows tell of its life: the last event naming it,
/// and the latest time of its events when a row says its life ended.
#[derive(Debug, Default)]
struct Told {
    last_id: EventId,
    ended_ms: Option<i64>,
}

fn told(rows: &[LedgerRow]) -> BTreeMap<String, Told> {
    let mut by = BTreeMap::<String, Told>::new();
    for row in rows {
        let wait = wait_of(row);
        let Some(supervisor) = reached_by(row, wait.as_ref()) else {
            continue;
        };
        let told = by.entry(supervisor).or_default();
        told.last_id = told.last_id.max(row.inputs_through.last());
        if life_ended(row) {
            told.ended_ms = told.ended_ms.max(row.ended_at);
        }
    }
    by
}

fn stopped(stops: &[RunEvent]) -> BTreeSet<&str> {
    stops
        .iter()
        .filter(|stop| timestamp_millis(&stop.created_at).is_some())
        .filter_map(supervisor_of)
        .collect()
}

/// The last events naming the supervisors whose lives go on at the
/// position: those `rows` reaches that did not stop (`stops`) and whose
/// rows do not say their lives ended. Their times are the latest of their
/// lives ([`books`]); a reader reads them by id.
pub fn living(rows: &[LedgerRow], stops: &[RunEvent]) -> Vec<EventId> {
    let stopped = stopped(stops);
    told(rows)
        .into_iter()
        .filter(|(supervisor, told)| {
            told.ended_ms.is_none() && !stopped.contains(supervisor.as_str())
        })
        .map(|(_, told)| told.last_id)
        .collect()
}

/// The lives and the waits before a claim of the events up to the
/// position `at`, made of `rows` (those [`holds`] keeps, or at a position
/// before theirs those [`holds_at`] keeps), `stops` (every
/// `supervisor_stopped` the queue rows record) and `last` (the events
/// [`living`] names, or at a position before the rows' the last event
/// naming each supervisor by `at` that
/// [`crate::domain::ledger::queue::cut::Needs`] names): a builder that goes
/// on from there with them writes the rows of the fold of every event.
/// `closed_to` is the time of the pass that folded the rows, as the latest
/// time its lives closed a wait. A row with something past `at` is cut
/// there first ([`crate::domain::ledger::queue::cut`]), and a stop after it
/// is none; the books then hold every wait the books of the rows written
/// at `at` hold, as those hold it, and besides those only waits that
/// ended before the exact [`Since`] of the position (they are held as the
/// rows are taken by a bound no later than it).
pub fn books(
    rows: &[LedgerRow],
    stops: &[RunEvent],
    last: &[RunEvent],
    closed_to: i64,
    at: EventId,
) -> (Lives, PreClaimBook) {
    let past = |row: &LedgerRow| {
        row.inputs_through.last() > at || row.records.iter().any(|event| event.id > at)
    };
    let cut_rows: Vec<LedgerRow>;
    let rows = if rows.iter().any(past) {
        let lives = cut::lives_at(last, stops, at);
        let reads = cut::Reads::default();
        let cut = cut::Cut {
            at,
            now_ms: closed_to,
            lives: &lives,
            reads: &reads,
        };
        let heads: Vec<QueueHead> = rows.iter().map(QueueHead::of).collect();
        cut_rows = cut.heads(&heads).iter().map(QueueHead::row).collect();
        &cut_rows
    } else {
        rows
    };
    let mut lives = Lives::default();
    let last_ms: BTreeMap<EventId, i64> = last
        .iter()
        .filter_map(|event| Some((event.id, timestamp_millis(&event.created_at)?)))
        .collect();
    for (supervisor, told) in told(rows) {
        let latest = told
            .ended_ms
            .or_else(|| last_ms.get(&told.last_id).copied());
        lives.go_on(&supervisor, told.last_id, latest);
    }
    let mut stops: Vec<&RunEvent> = stops
        .iter()
        .filter(|stop| stop.kind == EventKind::SupervisorStopped && stop.id <= at)
        .collect();
    stops.sort_by_key(|stop| stop.id);
    for stop in stops {
        lives.add(stop);
    }
    let book = book(rows, &lives, closed_to);
    (lives, book)
}

/// The book of the waits of `rows`, their supervisors' lives `lives`.
fn book(rows: &[LedgerRow], lives: &Lives, closed_to: i64) -> PreClaimBook {
    let mut book = PreClaimBook {
        closed_to,
        ..PreClaimBook::default()
    };
    let mut by_start: Vec<(&LedgerRow, Started)> = rows
        .iter()
        .filter_map(|row| Some((row, started(subject_kind(row)?)?)))
        .collect();
    by_start.sort_by_key(|(row, _)| row.key.parse::<i64>().unwrap_or_default());
    for (row, started) in by_start {
        let Some(wait) = wait_of(row) else {
            continue;
        };
        let (id, end) = (wait.id, row.inputs_through.events);
        let open = wait.ended_at.is_none();
        match started {
            Started::PreClaim(IntervalKind::SlotsFull) => {
                let (Some(supervisor), Some(class)) = (wait.supervisor.clone(), wait.class) else {
                    continue;
                };
                if wait.end.as_deref() == Some(ENDED_FREED) {
                    book.closed.insert(id, (wait, Some(end)));
                    continue;
                }
                book.slots.insert((supervisor, class), id);
                if open {
                    book.open.insert(id, wait);
                } else {
                    book.closed.insert(id, (wait, None));
                }
            }
            Started::PreClaim(kind) => {
                if !open {
                    book.closed.insert(id, (wait, Some(end)));
                    continue;
                }
                match kind {
                    IntervalKind::ClaimHold => book.claim_hold = Some(id),
                    IntervalKind::ClaimDeferral => {
                        if let Some(task) = wait.task_id {
                            book.deferrals.insert(task, id);
                        }
                    }
                    IntervalKind::SlotsFull => {}
                }
                book.open.insert(id, wait);
            }
            Started::LandingHold => {
                if open {
                    book.landing_hold = Some(id);
                    book.landings.insert(id, (wait, None));
                } else {
                    book.landings.insert(id, (wait, Some(end)));
                }
            }
        }
    }
    // The supervisors with a slots full open wait on their lives' ends, as
    // the last close left them; one their lives closed waits on an event
    // naming them.
    let open_slots: BTreeSet<String> = book
        .slots
        .iter()
        .filter(|(_, start)| book.open.contains_key(start))
        .map(|((supervisor, _), _)| supervisor.clone())
        .collect();
    for supervisor in open_slots {
        if let Some(at) = lives.ends_from(&supervisor) {
            book.life_end_of.insert(supervisor.clone(), at);
            book.life_ends.insert((at, supervisor));
        }
    }
    book
}

#[cfg(test)]
mod tests;
