//! The coverage of the omitted queue events counted straight from the
//! events, for the tests of what builds it
//! ([`crate::domain::ledger::session::written::omitted`]) to compare with.

use std::collections::BTreeMap;

use crate::domain::ledger::owner::{self, Owner};
use crate::domain::ledger::session::written::omitted::{Coverage, KindSummary, Point};
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
