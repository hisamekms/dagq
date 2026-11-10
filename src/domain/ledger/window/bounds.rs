//! The bounds a reader takes inside a window (ADR-t4644-1 "保つ保証と値の
//! 比較", docs/design/measurement.md "共通の読み取り関数"): the times and
//! positions a reader works out as it folds, past the window's own edges
//! (`super`'s module doc, "The edges of a window"), such as each period of
//! `kpi` and its comparison, the time of an event id cursor of `kpi`, and
//! the first time of the page of `stats` without `--full` and `--since`.
//! The omitted queue events of (C, S] are not among the window's records
//! ([`super::omitted`]), so a bound worked out again of
//! [`LedgerWindow::events`] misses those it stands on when a clock turned
//! back. A reader declares its bounds with the window's query
//! ([`super::WindowQuery::bounds`], [`DeclareBounds`]); the common read
//! answers each of the same snapshot as the rows, of the records and of
//! the omitted events by the reads of [`super::omitted`] ((c) for a time,
//! (d) for a range of ids, and the latest time through an id), and the
//! reader takes the window's answer ([`LedgerWindow::answers`]) rather
//! than working it out again. Each answer reads what one of those reads
//! reads, a constant however long the history around the window, so a
//! window reads as many as the bounds declared, the periods' and the
//! comparisons' a few times over.
//!
//! The events an answer stands on are taken among the window's records
//! too, as the window's edges' are, so that what a reader reads of the
//! records at a bound (the time of the event a period ends at, the events
//! it counts) is the same as of the rows that held them.
//!
//! A reader that comes to need a bound it did not declare has no answer
//! for it: it declares every bound it takes, and one added later is
//! declared with them (a test of a declaring read fails on one missing,
//! [`missing`]).

use std::collections::BTreeMap;
use std::sync::Arc;

use super::LedgerWindow;
use super::omitted::OmittedEdges;
use crate::domain::stats::{Cursor, timestamp_millis};
use crate::domain::{EventId, RunEvent};

/// A bound a reader takes inside a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bound {
    /// The last event of a time at or before this (unix ms) among the
    /// window's events, as [`Cursor::event_id`] of a time gives it: 0
    /// when none is. (c) of the omitted events.
    LastAt(i64),
    /// The latest time of the window's events of an id at or before this,
    /// as `kpi::cursor_ms` of an event id gives it: none when none is.
    /// Of the omitted events, M at the last point at or before the id and
    /// the events after it up to the id.
    TimeThrough(EventId),
    /// The first time of the window's events of ids after the first and
    /// up to the second: none when none is. (d) of the omitted events.
    FirstIn(EventId, EventId),
}

/// What declares the bounds a reader takes of a window. It is asked
/// again once the bounds it gave are answered, with the window holding the
/// answers ([`LedgerWindow::answers`]), as a bound may be worked out of
/// another's answer (a period of `kpi` of its `--at`'s time): it gives
/// every bound it takes of that window, and the read stops once it gives
/// none that has no answer, after [`ROUNDS`] at most.
pub trait DeclareBounds: Send + Sync {
    fn bounds(&self, window: &LedgerWindow) -> Vec<Bound>;
    /// How many times it is asked at most: one for a declaration whose
    /// bounds are worked out of no answer, so that it is not asked again
    /// only to give the same.
    fn rounds(&self) -> usize {
        ROUNDS
    }
}

/// How many times [`answer`] asks a declaration at most: once for the
/// cursors, once for what is worked out of them, once to see that nothing
/// is left, and one to spare.
pub const ROUNDS: usize = 4;

/// The declaration of a window's query: none declares no bound and reads
/// none (the window then holds no answers).
#[derive(Clone, Default)]
pub struct Declared(pub Option<Arc<dyn DeclareBounds>>);

impl Declared {
    pub fn of(declare: impl DeclareBounds + 'static) -> Self {
        Self(Some(Arc::new(declare)))
    }
}

impl std::fmt::Debug for Declared {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(if self.0.is_some() {
            "Declared(..)"
        } else {
            "Declared(none)"
        })
    }
}

/// The answers of the bounds a reader declared ([`Bound`]), each of the
/// window's records and the omitted events of the same snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Answers {
    last_at: BTreeMap<i64, EventId>,
    time_through: BTreeMap<EventId, Option<i64>>,
    first_in: BTreeMap<(EventId, EventId), Option<i64>>,
}

impl Answers {
    /// [`Bound::LastAt`]'s answer, if it was declared.
    pub fn last_at(&self, ms: i64) -> Option<EventId> {
        self.last_at.get(&ms).copied()
    }

    /// [`Bound::TimeThrough`]'s answer, if it was declared.
    pub fn time_through(&self, id: EventId) -> Option<Option<i64>> {
        self.time_through.get(&id).copied()
    }

    /// [`Bound::FirstIn`]'s answer, if it was declared.
    pub fn first_in(&self, after: EventId, upto: EventId) -> Option<Option<i64>> {
        self.first_in.get(&(after, upto)).copied()
    }

    /// Whether `bound` has an answer.
    pub fn has(&self, bound: Bound) -> bool {
        match bound {
            Bound::LastAt(ms) => self.last_at.contains_key(&ms),
            Bound::TimeThrough(id) => self.time_through.contains_key(&id),
            Bound::FirstIn(after, upto) => self.first_in.contains_key(&(after, upto)),
        }
    }

    /// The bounds answered, in order.
    pub fn bounds(&self) -> Vec<Bound> {
        let mut bounds: Vec<Bound> = self.last_at.keys().map(|ms| Bound::LastAt(*ms)).collect();
        bounds.extend(self.time_through.keys().map(|id| Bound::TimeThrough(*id)));
        bounds.extend(
            self.first_in
                .keys()
                .map(|(after, upto)| Bound::FirstIn(*after, *upto)),
        );
        bounds
    }

    /// Set `bound`'s answer: `id` for [`Bound::LastAt`], `ms` for the
    /// others (a test's hand-made answers).
    #[cfg(test)]
    pub fn set(&mut self, bound: Bound, id: EventId, ms: Option<i64>) {
        match bound {
            Bound::LastAt(at) => {
                self.last_at.insert(at, id);
            }
            Bound::TimeThrough(through) => {
                self.time_through.insert(through, ms);
            }
            Bound::FirstIn(after, upto) => {
                self.first_in.insert((after, upto), ms);
            }
        }
    }
}

/// A reader that declared its bounds asks for `bound`, which has none:
/// fails a test (debug builds), and the reader works it out of the events.
#[track_caller]
pub fn missing(bound: Bound) {
    debug_assert!(false, "{bound:?} was not declared with the window's query");
}

/// Answer the bounds `query` declares of `window` with the omitted events
/// of `edges` (of the same snapshot as its rows), and take the events the
/// answers stand on among its records. None declared: nothing is read and
/// the window holds no answers.
pub fn answer<E>(
    window: &mut LedgerWindow,
    query: &super::WindowQuery,
    edges: &OmittedEdges<'_, E>,
) -> Result<(), E> {
    let Some(declare) = query.bounds.0.as_ref() else {
        return Ok(());
    };
    window.answers = Some(Answers::default());
    for _ in 0..declare.rounds().min(ROUNDS) {
        let answers = window
            .answers
            .as_ref()
            .map_or_else(Answers::default, Clone::clone);
        let mut new: Vec<Bound> = declare
            .bounds(window)
            .into_iter()
            .filter(|bound| !answers.has(*bound))
            .collect();
        new.sort_unstable();
        new.dedup();
        if new.is_empty() {
            break;
        }
        let mut answers = answers;
        let mut taken = Vec::new();
        for bound in new {
            window.answer_bound(bound, edges, &mut answers, &mut taken)?;
        }
        window.take_records(taken);
        window.answers = Some(answers);
    }
    Ok(())
}

impl LedgerWindow {
    /// The answers of the bounds the window's query declared
    /// ([`super::WindowQuery::bounds`]); none when it declared none.
    pub const fn answers(&self) -> Option<&Answers> {
        self.answers.as_ref()
    }

    /// Answer `bound` of the records up to the window's end and the omitted
    /// events of `edges` the end holds, and the omitted event the answer
    /// stands on into `taken`.
    fn answer_bound<E>(
        &self,
        bound: Bound,
        edges: &OmittedEdges<'_, E>,
        answers: &mut Answers,
        taken: &mut Vec<RunEvent>,
    ) -> Result<(), E> {
        let events = self.events();
        let end_ms = self.end.ms;
        let held_ms = |ms: i64| end_ms.is_none_or(|end| ms <= end);
        match bound {
            Bound::LastAt(ms) => {
                let of_records = Cursor::Time(ms).event_id(events);
                // An event past the end's time is not the window's.
                let at = end_ms.map_or(ms, |end| ms.min(end));
                let omitted = edges.at_or_before_through(at, self.upto)?.0;
                if let Some(id) = omitted
                    && let Some(event) = edges.event_of(id)?
                {
                    taken.push(event);
                }
                answers
                    .last_at
                    .insert(ms, omitted.map_or(of_records, |id| id.max(of_records)));
            }
            Bound::TimeThrough(through) => {
                let of_records = events
                    .iter()
                    .filter(|event| event.id <= through)
                    .filter_map(|event| timestamp_millis(&event.created_at))
                    .max();
                // An omitted time past the end's is none of the window's (a
                // clock ahead of now): then the latest of those at or before
                // the end, read as (c) of the end's time.
                let held = through.min(self.upto);
                let omitted = match (edges.max_ms_through(held)?, end_ms) {
                    (Some(ms), Some(end)) if ms > end => edges.at_or_before_through(end, held)?.1,
                    (ms, _) => ms,
                };
                answers
                    .time_through
                    .insert(through, of_records.max(omitted));
            }
            Bound::FirstIn(after, upto) => {
                let upto_held = upto.min(self.upto);
                let of_records = events
                    .iter()
                    .filter(|event| event.id > after && event.id <= upto_held)
                    .filter_map(|event| timestamp_millis(&event.created_at))
                    .min();
                let omitted = edges
                    .first(after, upto_held)?
                    .filter(|(ms, _)| held_ms(*ms));
                if let Some((_, id)) = omitted
                    && let Some(event) = edges.event_of(id)?
                {
                    taken.push(event);
                }
                let first = match (of_records, omitted.map(|(ms, _)| ms)) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                answers.first_in.insert((after, upto), first);
            }
        }
        Ok(())
    }

    /// Take `events` among the records, each once: none of them moves an
    /// edge of the window, as each edge took the omitted events it stands
    /// on already (the latest, a time's last, the first time).
    fn take_records(&mut self, events: Vec<RunEvent>) {
        if events.is_empty() {
            return;
        }
        self.records.extend(events);
        self.records.sort_by_key(|event| event.id);
        self.records.dedup_by_key(|event| event.id);
        self.held = super::Held::of(&self.records, self.end);
    }
}

#[cfg(test)]
mod tests;
