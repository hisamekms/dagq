//! The coverage of the omitted queue events counted straight from the
//! events, for the tests of what builds it
//! ([`crate::domain::ledger::session::written::omitted`]) to compare with,
//! and a store of it in memory for the tests of what reads it
//! ([`crate::domain::ledger::window::omitted`]).

use std::cell::Cell;
use std::collections::BTreeMap;
use std::convert::Infallible;

use crate::domain::ledger::owner::{self, Owner};
use crate::domain::ledger::session::written::omitted::{Coverage, KindSummary, Omission, Point};
use crate::domain::ledger::window::omitted::OmittedRead;
use crate::domain::stats::timestamp_millis;
use crate::domain::{EventId, RunEvent};

/// The summary of each of `kinds` and its points (by kind and position)
/// over the events of `events` in (`from`, `through`], a point every
/// `every` events of a kind: L, T, M and D of the queue's events of the
/// kind alone, each point's of those up to it.
pub(crate) fn counted(
    events: &[RunEvent],
    kinds: &[String],
    (from, through): (EventId, EventId),
    every: u64,
) -> (BTreeMap<String, KindSummary>, Vec<Point>) {
    let mut summaries = BTreeMap::new();
    let mut points = Vec::new();
    for kind in kinds {
        let of_kind: Vec<&RunEvent> = events
            .iter()
            .filter(|event| event.id > from && event.id <= through)
            .filter(|event| event.kind.as_str() == kind)
            .collect();
        let up_to = |at: EventId| {
            let queue: Vec<&RunEvent> = of_kind
                .iter()
                .copied()
                .filter(|event| event.id <= at && owner::owner(event) == Owner::Queue)
                .collect();
            let timed: Vec<(EventId, i64)> = queue
                .iter()
                .filter_map(|event| timestamp_millis(&event.created_at).map(|ms| (event.id, ms)))
                .collect();
            let back = (0..timed.len())
                .filter_map(|n| {
                    let before = timed[..n].iter().map(|(_, ms)| *ms).max()?;
                    Some(before - timed[n].1)
                })
                .max()
                .unwrap_or(0)
                .max(0);
            KindSummary {
                last: queue.last().map(|event| event.id),
                timed: timed.last().map(|(id, _)| *id),
                max_ms: timed.iter().map(|(_, ms)| *ms).max(),
                back_ms: back,
                seen: u64::try_from(of_kind.iter().filter(|event| event.id <= at).count()).unwrap(),
            }
        };
        summaries.insert(kind.clone(), up_to(through));
        let every = usize::try_from(every).unwrap();
        for event in of_kind.iter().skip(every - 1).step_by(every) {
            let at = up_to(event.id);
            points.push(Point {
                kind: kind.clone(),
                at: event.id,
                max_ms: at.max_ms,
                last: at.last,
                timed: at.timed,
            });
        }
    }
    points.sort_by(|a, b| (&a.kind, a.at).cmp(&(&b.kind, b.at)));
    (summaries, points)
}

/// Whether `coverage`'s summaries are those [`counted`] over its (C, S] of
/// `events`.
pub(crate) fn covers(coverage: &Coverage, events: &[RunEvent], every: u64) -> bool {
    let (summaries, _) = counted(
        events,
        &coverage.kinds,
        (coverage.from, coverage.through),
        every,
    );
    summaries == coverage.summaries
}

/// A store in memory of `events`, the coverage of `kinds` over (C, S] as
/// the builder writes it, and its points, counting the events it gives.
pub(crate) struct EdgeStore {
    pub(crate) events: Vec<RunEvent>,
    pub(crate) coverage: Option<Coverage>,
    pub(crate) points: Vec<Point>,
    read: Cell<usize>,
}

impl EdgeStore {
    /// The coverage of `kinds` over (`from`, `through`] of `events`, a point
    /// every `every` events of a kind.
    pub(crate) fn of(
        events: Vec<RunEvent>,
        kinds: &[&str],
        (from, through): (EventId, EventId),
        every: u64,
    ) -> Self {
        let kinds: Vec<String> = Omission::of(kinds.iter().copied(), every).kinds;
        let mut coverage = Coverage::start(&kinds, from);
        let mut points = Vec::new();
        let read: Vec<RunEvent> = events
            .iter()
            .filter(|event| event.id <= through)
            .cloned()
            .collect();
        coverage.read(&read, every, through, &mut points);
        Self {
            events,
            coverage: Some(coverage),
            points,
            read: Cell::new(0),
        }
    }

    /// How many events it gave, and none counted after.
    pub(crate) fn taken(&self) -> usize {
        self.read.replace(0)
    }

    /// The omitted events: of the kinds, the queue's, in (C, S].
    pub(crate) fn omitted(&self) -> Vec<&RunEvent> {
        let coverage = self.coverage.as_ref().unwrap();
        self.events
            .iter()
            .filter(|event| event.id > coverage.from && event.id <= coverage.through)
            .filter(|event| {
                coverage
                    .kinds
                    .iter()
                    .any(|kind| kind == event.kind.as_str())
                    && owner::owner(event) == Owner::Queue
            })
            .collect()
    }
}

impl OmittedRead for EdgeStore {
    type Error = Infallible;
    fn coverage(&self) -> Result<Option<Coverage>, Infallible> {
        Ok(self.coverage.clone())
    }
    fn point_after(&self, kind: &str, after: EventId) -> Result<Option<Point>, Infallible> {
        Ok(self
            .points
            .iter()
            .filter(|point| point.kind == kind && point.at > after)
            .min_by_key(|point| point.at)
            .cloned())
    }
    fn point_at_or_before(&self, kind: &str, at: EventId) -> Result<Option<Point>, Infallible> {
        Ok(self
            .points
            .iter()
            .filter(|point| point.kind == kind && point.at <= at)
            .max_by_key(|point| point.at)
            .cloned())
    }
    fn point_by_ms(&self, kind: &str, ms: i64) -> Result<Option<Point>, Infallible> {
        Ok(self
            .points
            .iter()
            .filter(|point| point.kind == kind && point.max_ms.is_some_and(|max| max <= ms))
            .max_by_key(|point| (point.max_ms, point.at))
            .cloned())
    }
    fn event(&self, id: EventId) -> Result<Option<RunEvent>, Infallible> {
        let event = self.events.iter().find(|event| event.id == id).cloned();
        self.read
            .set(self.read.get() + usize::from(event.is_some()));
        Ok(event)
    }
    fn events_of_kind(
        &self,
        kind: &str,
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>, Infallible> {
        let events: Vec<RunEvent> = self
            .events
            .iter()
            .filter(|event| event.kind.as_str() == kind && event.id > after && event.id <= upto)
            .take(limit)
            .cloned()
            .collect();
        self.read.set(self.read.get() + events.len());
        Ok(events)
    }
}
