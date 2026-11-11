//! A window's edges among the omitted queue events (ADR-t4644-1 decision
//! 12, docs/design/measurement.md "共通の読み取り関数"): the events of the
//! kinds a coverage holds ([`Coverage`]) whose row [`owner::owner`] says is
//! the queue's, in its (C, S], have no queue row, so what a window takes of
//! the rows' records for its edges (`super`'s module doc, "The edges of a
//! window") misses them. [`OmittedEdges`] reads them of the EventStore and
//! the coverage's summaries and points instead, of the same snapshot as the
//! rows, and gives for each edge what the omitted events add to it, for the
//! window to take with the rows' (the latest the larger, a first time the
//! smaller, a cursor's event that event itself).
//!
//! Only (C, S] of the coverage the store holds is read, of the kinds of its
//! summaries (not this version's [`crate::domain::ledger::queue::OMITTED_KINDS`]):
//! S and the summaries are written together over the same range, so a read
//! of the coverage the store holds reads all it covers, whatever kinds the
//! version that reads it omits. An event of those kinds of a run's, task's or
//! session's row is none of them (it enters a window by its row's records
//! alone); one past S, or before C or at it (its row holds it), is none
//! either. An omitted event whose time does not parse is the latest
//! ([`OmittedEdges::latest`]) and no time's answer.
//!
//! What each edge reads, of a kind (L, T, M, D its summary's, K the events of
//! the kind of any row between two points,
//! [`crate::domain::ledger::session::written::omitted::POINT_EVERY`]):
//!
//! - (a) The latest ([`OmittedEdges::latest`]): L. Nothing read.
//! - (b) The time of an event by its id ([`OmittedEdges::time_of`]): the
//!   event, by its key, when it is in (C, S]; its time when it is of a kind
//!   held and the queue's.
//! - (c) The last omitted event of a time at or before t
//!   ([`OmittedEdges::at_or_before`]): T when M ≤ t. Else the last point P
//!   of an M at or before t, by the index of M: every timed omitted event up
//!   to P is of a time ≤ M of P ≤ t, so T of P is the answer so far (L is
//!   not, an event of no time). From P (from C without one) on, between two
//!   points whose T is the same no timed omitted event stands, so those are
//!   not read: the next point Q whose T is past the one so far is sought
//!   ([`first_point_past`], by the points' key), and the events of the kind
//!   after the point before Q up to Q (K at most) are read by the kind's
//!   index. A timed omitted event of a time ≤ t is the answer so far; one
//!   past t + D stops the read, as does S: M never falls, and a timed
//!   omitted event after one is of a time M − D or later, past t. An event
//!   of no time is neither an answer nor a stop.
//! - (d) The first time of the omitted events of ids in (a, b]
//!   ([`OmittedEdges::first_ms`]): of (a, b] within (C, S], read as (c)
//!   from the point before the first whose T is past a, the least time m of
//!   the timed omitted events read; one of a time past m + D stops it, as do
//!   b and S, as above.
//! - (e) The latest time of the omitted events of ids up to u
//!   ([`OmittedEdges::max_ms_through`]) and (f) the last of them, of no
//!   time too ([`OmittedEdges::last_through`]): M and L at u at or past S;
//!   else M and L of the last point P at or before u, and the events of
//!   the kind after P (after C without one) up to u, fewer than K, the
//!   next point being past u. Of u at or before C, none. (f) is the last
//!   event an event id `--until` holds, (e) the time `kpi` takes of an
//!   event id cursor ([`super::bounds`]).
//!
//! So (a) and (b) read a constant, (e) and (f) a span of K events, and
//! (c) and (d) the events of the points'
//! spans that hold the timed omitted events within D of the edge's time,
//! K each, besides the seeks of the points (by key and by M, a few per
//! span). How long the history before or after the window is, of the
//! kind's events of a run's, task's or session's or of no time too, adds no
//! event to them. Pure: it reads through [`OmittedRead`].

use crate::domain::ledger::owner::{self, Owner};
use crate::domain::ledger::session::written::omitted::{Coverage, KindSummary, Point};
use crate::domain::stats::{Cursor, timestamp_millis};
use crate::domain::{EventId, RunEvent};

/// How many events of a kind [`OmittedEdges`] asks for at a time: a span
/// between two points holds this many at most.
pub const EVENTS_PER_READ: usize = 256;

/// What [`OmittedEdges`] reads of a store, all of one snapshot with the rows.
pub trait OmittedRead {
    type Error;
    /// The coverage the store holds, if any.
    fn coverage(&self) -> Result<Option<Coverage>, Self::Error>;
    /// The first point of `kind` past the position `after`, by key.
    fn point_after(&self, kind: &str, after: EventId) -> Result<Option<Point>, Self::Error>;
    /// The last point of `kind` at the position `at` or before it, by key.
    fn point_at_or_before(&self, kind: &str, at: EventId) -> Result<Option<Point>, Self::Error>;
    /// The last point of `kind` whose M is at `ms` or before it, by the
    /// index of M.
    fn point_by_ms(&self, kind: &str, ms: i64) -> Result<Option<Point>, Self::Error>;
    /// The event of `id`, by its key.
    fn event(&self, id: EventId) -> Result<Option<RunEvent>, Self::Error>;
    /// The events of `kind` with `after < id <= upto`, oldest first, at most
    /// `limit`, by the index of kind and id.
    fn events_of_kind(
        &self,
        kind: &str,
        after: EventId,
        upto: EventId,
        limit: usize,
    ) -> Result<Vec<RunEvent>, Self::Error>;
}

/// What the omitted events of the coverage a store holds add to a window's
/// edges (module doc). Of no coverage, nothing: nothing is read.
pub struct OmittedEdges<'a, E> {
    read: Option<&'a dyn OmittedRead<Error = E>>,
    coverage: Option<Coverage>,
}

impl<E> OmittedEdges<'static, E> {
    /// Of no coverage: no edge has anything added.
    pub const fn none() -> Self {
        Self {
            read: None,
            coverage: None,
        }
    }
}

impl<'a, E> OmittedEdges<'a, E> {
    /// Of the coverage `read` holds.
    pub fn of(read: &'a dyn OmittedRead<Error = E>) -> Result<Self, E> {
        Ok(Self {
            coverage: read.coverage()?,
            read: Some(read),
        })
    }

    /// The read and the coverage, when one is held.
    fn held(&self) -> Option<(&'a dyn OmittedRead<Error = E>, &Coverage)> {
        self.read.zip(self.coverage.as_ref())
    }

    /// (a) The last omitted event, of no time too.
    pub fn latest(&self) -> Option<EventId> {
        let (_, coverage) = self.held()?;
        summaries(coverage)
            .filter_map(|(_, summary)| summary.last)
            .max()
    }

    /// The event of `id` when it is an omitted one.
    pub fn event_of(&self, id: EventId) -> Result<Option<RunEvent>, E> {
        let Some((read, coverage)) = self.held() else {
            return Ok(None);
        };
        if id <= coverage.from || id > coverage.through {
            return Ok(None);
        }
        Ok(read.event(id)?.filter(|event| covered(coverage, event)))
    }

    /// (b) The time of the event `id` when it is an omitted one.
    pub fn time_of(&self, id: EventId) -> Result<Option<i64>, E> {
        Ok(self
            .event_of(id)?
            .and_then(|event| timestamp_millis(&event.created_at)))
    }

    /// (c) The last omitted event of a time at or before `ms`.
    pub fn at_or_before(&self, ms: i64) -> Result<Option<EventId>, E> {
        let mut answer = None;
        if let Some((read, coverage)) = self.held() {
            for (kind, summary) in summaries(coverage) {
                answer = answer.max(kind_at_or_before(read, coverage, kind, summary, ms)?);
            }
        }
        Ok(answer)
    }

    /// (c) among the ids up to `through`: the last omitted event of a time
    /// at or before `ms` and an id at or before `through`, and the latest
    /// time of those, read as (c) is.
    pub fn at_or_before_through(
        &self,
        ms: i64,
        through: EventId,
    ) -> Result<(Option<EventId>, Option<i64>), E> {
        let (mut id, mut latest) = (None, None);
        if let Some((read, coverage)) = self.held() {
            for (kind, summary) in summaries(coverage) {
                let (of_kind, at) =
                    kind_at_or_before_through(read, coverage, kind, summary, ms, through)?;
                id = id.max(of_kind);
                latest = latest.max(at);
            }
        }
        Ok((id, latest))
    }

    /// (d) The first time of the omitted events of ids in (`after`, `upto`]
    /// and the first event of that time.
    pub fn first(&self, after: EventId, upto: EventId) -> Result<Option<(i64, EventId)>, E> {
        let mut first: Option<(i64, EventId)> = None;
        if let Some((read, coverage)) = self.held() {
            for (kind, summary) in summaries(coverage) {
                if let Some(of_kind) = kind_first(read, coverage, kind, summary, (after, upto))? {
                    first = Some(first.map_or(of_kind, |first| first.min(of_kind)));
                }
            }
        }
        Ok(first)
    }

    /// (d) The first time alone.
    pub fn first_ms(&self, after: EventId, upto: EventId) -> Result<Option<i64>, E> {
        Ok(self.first(after, upto)?.map(|(ms, _)| ms))
    }

    /// (e) The latest time of the omitted events of an id at or before
    /// `through`.
    pub fn max_ms_through(&self, through: EventId) -> Result<Option<i64>, E> {
        let mut answer = None;
        if let Some((read, coverage)) = self.held() {
            for (kind, summary) in summaries(coverage) {
                answer = answer.max(kind_through(read, coverage, kind, summary, through)?.1);
            }
        }
        Ok(answer)
    }

    /// (f) The last omitted event of an id at or before `through`, of no
    /// time too.
    pub fn last_through(&self, through: EventId) -> Result<Option<EventId>, E> {
        let mut answer = None;
        if let Some((read, coverage)) = self.held() {
            for (kind, summary) in summaries(coverage) {
                answer = answer.max(kind_through(read, coverage, kind, summary, through)?.0);
            }
        }
        Ok(answer)
    }

    /// The omitted events a window of `query` takes for its edges with the
    /// records `records` of its rows (any order): (a) the latest; each
    /// cursor's event, by its id (b) or the last of a time at or before its
    /// time (c); for an event id `--until`, the last of an id at or before
    /// it (f); the last of a time at or before the window's end (c), the
    /// latest of what it holds, unless it ends at an event; and without a
    /// time to start from, the first of the least time of the window's ids
    /// (d). The window is resolved as [`super::LedgerWindow`]'s, of the
    /// records and the events taken before.
    /// Each once, of the kinds and (C, S] of the coverage; none of none.
    pub fn events_for(
        &self,
        query: &super::WindowQuery,
        records: &[&[RunEvent]],
    ) -> Result<Vec<RunEvent>, E> {
        if self.held().is_none() {
            return Ok(Vec::new());
        }
        let mut ids: Vec<EventId> = self.latest().into_iter().collect();
        for cursor in [query.since, query.until].into_iter().flatten() {
            match cursor {
                Cursor::Event(id) => ids.push(id),
                Cursor::Time(ms) => ids.extend(self.at_or_before(ms)?),
            }
        }
        // The last the end of an event `--until` holds: the latest of the
        // window and the event it stops at.
        if let Some(Cursor::Event(id)) = query.until {
            ids.extend(self.last_through(id)?);
        }
        let mut events = Vec::new();
        for id in ids {
            if !events.iter().any(|event: &RunEvent| event.id == id)
                && let Some(event) = self.event_of(id)?
            {
                events.push(event);
            }
        }
        let positions = {
            let every = || records.iter().flat_map(|part| part.iter()).chain(&events);
            super::Positions::of(every(), query, |id| {
                every()
                    .find(|event| event.id == id)
                    .and_then(|event| timestamp_millis(&event.created_at))
            })
        };
        let mut ids = Vec::new();
        // The latest the end holds: of a time at or before its time, unless
        // it is an event.
        if !matches!(query.until, Some(Cursor::Event(_)))
            && let Some(id) = self.at_or_before(positions.end_ms)?
            && id <= positions.upto
        {
            ids.push(id);
        }
        if positions.start_ms.is_none()
            && let Some((_, id)) = self.first(positions.after, positions.upto)?
        {
            ids.push(id);
        }
        for id in ids {
            if !events.iter().any(|event| event.id == id)
                && let Some(event) = self.event_of(id)?
            {
                events.push(event);
            }
        }
        Ok(events)
    }
}

/// Each kind of `coverage` with its summary.
fn summaries(coverage: &Coverage) -> impl Iterator<Item = (&str, &KindSummary)> {
    coverage
        .kinds
        .iter()
        .filter_map(|kind| Some((kind.as_str(), coverage.summaries.get(kind)?)))
}

/// Whether `event` is omitted by this coverage: its id is in (C, S],
/// its kind is covered and its owner is the queue. Shared with visibility
/// reads, which exclude these events and leave supplementation to the edges.
pub(crate) fn covered(coverage: &Coverage, event: &RunEvent) -> bool {
    event.id > coverage.from && event.id <= coverage.through && omitted(coverage, event)
}

/// Whether `event` is one of `coverage`'s omitted events, its id aside.
fn omitted(coverage: &Coverage, event: &RunEvent) -> bool {
    coverage
        .kinds
        .iter()
        .any(|kind| kind == event.kind.as_str())
        && owner::owner(event) == Owner::Queue
}

/// The time of `event` when it is an omitted one of a time that parses.
fn omitted_ms(event: &RunEvent) -> Option<i64> {
    (owner::owner(event) == Owner::Queue)
        .then(|| timestamp_millis(&event.created_at))
        .flatten()
}

/// The points of (C, S] alone: a store whose reads are not of one
/// transaction (the queue service's) may give one written past the S read.
fn within(coverage: &Coverage, point: Option<Point>) -> Option<Point> {
    point.filter(|point| point.at <= coverage.through)
}

fn point_after<E>(
    read: &dyn OmittedRead<Error = E>,
    coverage: &Coverage,
    kind: &str,
    after: EventId,
) -> Result<Option<Point>, E> {
    Ok(within(coverage, read.point_after(kind, after)?))
}

fn point_at_or_before<E>(
    read: &dyn OmittedRead<Error = E>,
    coverage: &Coverage,
    kind: &str,
    at: EventId,
) -> Result<Option<Point>, E> {
    Ok(within(
        coverage,
        read.point_at_or_before(kind, at.min(coverage.through))?,
    ))
}

fn point_by_ms<E>(
    read: &dyn OmittedRead<Error = E>,
    coverage: &Coverage,
    kind: &str,
    ms: i64,
) -> Result<Option<Point>, E> {
    Ok(within(coverage, read.point_by_ms(kind, ms)?))
}

/// The first point of `kind` past `from` whose T is past `past`, and where
/// the span up to it starts: the point before it, or `from` when it is the
/// first. No point: none, the span from `from` to S. T only rises with the
/// position, so the points between are passed over by a search of their
/// key, never read one by one.
fn first_point_past<E>(
    read: &dyn OmittedRead<Error = E>,
    coverage: &Coverage,
    kind: &str,
    from: EventId,
    past: Option<EventId>,
) -> Result<(EventId, Option<Point>), E> {
    let Some(first) = point_after(read, coverage, kind, from)? else {
        return Ok((from, None));
    };
    if first.timed > past {
        return Ok((from, Some(first)));
    }
    // The most common: the one after it.
    let Some(second) = point_after(read, coverage, kind, first.at)? else {
        return Ok((first.at, None));
    };
    if second.timed > past {
        return Ok((first.at, Some(second)));
    }
    // Every point at `low` or before is of a T at `past` or before.
    let (mut low, mut high) = (second.at.as_i64(), coverage.through.as_i64());
    while low < high {
        let mid = low + (high - low + 1) / 2;
        let below = point_at_or_before(read, coverage, kind, EventId::new(mid))?;
        if below.is_some_and(|point| point.timed <= past) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let low = EventId::new(low);
    let start = point_at_or_before(read, coverage, kind, low)?.map_or(second.at, |point| point.at);
    Ok((start, point_after(read, coverage, kind, low)?))
}

/// Each event of `kind` in (`after`, `upto`], oldest first, until `each`
/// says to stop; whether it stopped.
fn each_of_kind<E>(
    read: &dyn OmittedRead<Error = E>,
    kind: &str,
    (mut after, upto): (EventId, EventId),
    mut each: impl FnMut(&RunEvent) -> bool,
) -> Result<bool, E> {
    while after < upto {
        let events = read.events_of_kind(kind, after, upto, EVENTS_PER_READ)?;
        for event in &events {
            if !each(event) {
                return Ok(true);
            }
        }
        match events.last() {
            Some(last) if events.len() == EVENTS_PER_READ => after = last.id,
            _ => break,
        }
    }
    Ok(false)
}

/// (c) of one kind.
fn kind_at_or_before<E>(
    read: &dyn OmittedRead<Error = E>,
    coverage: &Coverage,
    kind: &str,
    summary: &KindSummary,
    ms: i64,
) -> Result<Option<EventId>, E> {
    Ok(kind_at_or_before_through(read, coverage, kind, summary, ms, coverage.through)?.0)
}

/// (c) of one kind among the ids up to `through`, with the latest time of
/// those: the last timed omitted event of a time at or before `ms` and an
/// id at or before `through`, and the latest of their times. Read as (c)
/// is, from the last point of an M at or before `ms` (or the last at or
/// before `through` when that one is past it: M only rises, so its M is at
/// or before `ms` too), its T and M the answer so far, up to a timed one
/// past `ms` + D, `through` or S.
fn kind_at_or_before_through<E>(
    read: &dyn OmittedRead<Error = E>,
    coverage: &Coverage,
    kind: &str,
    summary: &KindSummary,
    ms: i64,
    through: EventId,
) -> Result<(Option<EventId>, Option<i64>), E> {
    let (Some(timed), Some(max)) = (summary.timed, summary.max_ms) else {
        return Ok((None, None));
    };
    let through = through.min(coverage.through);
    if through <= coverage.from {
        return Ok((None, None));
    }
    if max <= ms && timed <= through {
        return Ok((Some(timed), Some(max)));
    }
    let point = match point_by_ms(read, coverage, kind, ms)? {
        Some(point) if point.at > through => point_at_or_before(read, coverage, kind, through)?,
        point => point,
    };
    let (mut at, mut so_far) = point
        .as_ref()
        .map_or((coverage.from, None), |point| (point.at, point.timed));
    let mut answer = so_far;
    let mut latest = point.and_then(|point| point.max_ms);
    let past = ms.saturating_add(summary.back_ms);
    loop {
        let (start, next) = first_point_past(read, coverage, kind, at, so_far)?;
        let end = next
            .as_ref()
            .map_or(coverage.through, |point| point.at)
            .min(through);
        let stopped = each_of_kind(read, kind, (start, end), |event| match omitted_ms(event) {
            Some(time) if time <= ms => {
                answer = answer.max(Some(event.id));
                latest = latest.max(Some(time));
                true
            }
            Some(time) => time <= past,
            None => true,
        })?;
        match next {
            Some(point) if !stopped && end < through => (at, so_far) = (point.at, point.timed),
            _ => return Ok((answer, latest)),
        }
    }
}

/// (e) and (f) of one kind: the last omitted event of an id at or before
/// `through` (of no time too) and the latest time of those. Of a position
/// at or past S, L and M; else the last point P at or before it, its L and
/// M the answer so far, and the events of the kind after P (after C
/// without one) up to the position, fewer than K (the next point is past
/// it).
fn kind_through<E>(
    read: &dyn OmittedRead<Error = E>,
    coverage: &Coverage,
    kind: &str,
    summary: &KindSummary,
    through: EventId,
) -> Result<(Option<EventId>, Option<i64>), E> {
    if through <= coverage.from {
        return Ok((None, None));
    }
    if through >= coverage.through {
        return Ok((summary.last, summary.max_ms));
    }
    let point = point_at_or_before(read, coverage, kind, through)?;
    let (mut last, mut max) = point
        .as_ref()
        .map_or((None, None), |point| (point.last, point.max_ms));
    let from = point.map_or(coverage.from, |point| point.at);
    each_of_kind(read, kind, (from, through), |event| {
        if owner::owner(event) == Owner::Queue {
            last = last.max(Some(event.id));
            max = max.max(timestamp_millis(&event.created_at));
        }
        true
    })?;
    Ok((last, max))
}

/// (d) of one kind: the least time and the first event of it.
fn kind_first<E>(
    read: &dyn OmittedRead<Error = E>,
    coverage: &Coverage,
    kind: &str,
    summary: &KindSummary,
    (after, upto): (EventId, EventId),
) -> Result<Option<(i64, EventId)>, E> {
    let (low, high) = (after.max(coverage.from), upto.min(coverage.through));
    if low >= high || summary.timed.is_none_or(|timed| timed <= low) {
        return Ok(None);
    }
    // A point at `low` or before is of a T at `low` or before.
    let mut at =
        point_at_or_before(read, coverage, kind, low)?.map_or(coverage.from, |point| point.at);
    let mut so_far = Some(low);
    let mut first: Option<(i64, EventId)> = None;
    loop {
        let (start, next) = first_point_past(read, coverage, kind, at, so_far)?;
        let end = next
            .as_ref()
            .map_or(coverage.through, |point| point.at)
            .min(high);
        let stopped = each_of_kind(read, kind, (start.max(low), end), |event| match omitted_ms(
            event,
        ) {
            Some(time) if first.is_some_and(|(m, _)| time > m.saturating_add(summary.back_ms)) => {
                false
            }
            Some(time) => {
                if first.is_none_or(|(m, _)| time < m) {
                    first = Some((time, event.id));
                }
                true
            }
            None => true,
        })?;
        match next {
            Some(point) if !stopped && end < high => (at, so_far) = (point.at, point.timed),
            _ => return Ok(first),
        }
    }
}

#[cfg(test)]
mod tests;
