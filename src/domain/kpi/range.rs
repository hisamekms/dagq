//! The range of the ledger a `kpi` question needs (goal 271): when it
//! starts and, if it ends before now, when it ends, decided of the
//! [`KpiQuery`], now and the `[kpi]` settings alone, and the few events
//! past the ledger's rows of that range that the answer looks back or
//! forward to: the change marks of the range ([`range_events`]), the
//! forecast's scoring of the finishes in it ([`forecast_events`]) and the
//! groups of a comparison ([`cohort`]).
//!
//! Not wired yet: the reads are the store's small reads (the
//! `LedgerWindowRead` port's `mark_events` to `comparison_events`), passed
//! in as [`RangeReads`]. What the events
//! answer is decided by the functions that answer of every event
//! ([`marks::marks`], [`super::compare`]'s routine and groups,
//! [`score::score`]): this module only selects which events they are
//! given, so no rule is written twice.
//!
//! # The clocks
//!
//! Each event keeps the time its writer's clock gave, so a time may go
//! back as the ids go on, a CLI's clock may run ahead of the DB's, and a
//! pruned stop takes effect at a heartbeat before it was written. No read
//! here takes a time for an id or an id for a time: what [`marks::marks`]
//! orders by time is read by time (a recorded mark by when it took effect,
//! a claim by when it was written, routine's previous start by `(time,
//! id)`), and what it orders by id is sought by id from each event of the
//! range (a claim's previous claim, the start between them, a mark's
//! retraction). The answer is therefore that of every event however the
//! clocks ran, with no precondition on them to accept. The one read that
//! does turn an id into a time, an event id cursor ([`RangeReads::cursor_time`],
//! the latest visible time at or before the id), is found by the latest
//! events, else by the aggregate of the times at or before the id, else by
//! a walk of a fixed number of candidates past invisible ones; only when
//! that walk runs out is the range [`Undecided`] and the caller reads every
//! row, as today.
use std::collections::{BTreeMap, HashMap};

use super::{KpiConfig, KpiQuery, compare, period_count, period_start_back};
use crate::domain::forecast::score::{self, Scoring, TASK_MARK_KINDS};
use crate::domain::marks::{self, DERIVED, Mark, claim_value};
use crate::domain::stats::Cursor;
use crate::domain::{EventId, GoalId, RunEvent, TaskId};

/// Why a question has no range: the caller reads every row of the ledger,
/// which answers it as it does today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Undecided {
    /// [`KpiQuery::read_whole_ledger`] asks for every row.
    WholeLedger,
    /// `--last 0`, which `kpi` refuses.
    NoPeriods,
    /// `--until` without `--since`: the window starts at the first event,
    /// which only the whole ledger knows.
    FromFirstEvent,
    /// An event id cursor names no event (`kpi` refuses it).
    NoEvent(EventId),
    /// An event id cursor's time could not be found: neither among the
    /// latest events nor by the aggregate of the times at or before it (the
    /// latest of those is invisible to the snapshot), and the walk past the
    /// invisible ones ran out of its candidates. A past cursor found by the
    /// aggregate, however much was written after it, is not this.
    OutOfTimeOrder(EventId),
    /// The spans of the question are ones `kpi` refuses (a window that
    /// does not start before it ends).
    Refused,
    /// A small read could not answer (its limit, or a visibility it could
    /// not decide).
    Fallback,
}

/// A read of [`RangeReads`]: its answer, or why the whole ledger must be
/// read instead.
pub type Read<T> = Result<T, Undecided>;

/// The range of time (unix ms) a question's answer needs, the start
/// exclusive: every period, window and comparison window it computes
/// lies in `(from_ms, until_ms]`, `until_ms` being none when the range
/// reaches now (and what is recorded after it, as a mark backdated into
/// the range).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadRange {
    pub from_ms: i64,
    pub until_ms: Option<i64>,
}

impl ReadRange {
    /// The end to read the range's small reads to.
    pub fn to_ms(self) -> i64 {
        self.until_ms.unwrap_or(i64::MAX)
    }

    /// The range that holds both.
    fn union(self, other: Self) -> Self {
        Self {
            from_ms: self.from_ms.min(other.from_ms),
            until_ms: self.until_ms.zip(other.until_ms).map(|(a, b)| a.max(b)),
        }
    }
}

/// The store's small reads of the events (the `LedgerWindowRead` port's),
/// each of the events visible in the same snapshot as the rows. Each says
/// which index it stands for; none reads by the other order (see the
/// module's "The clocks").
pub trait RangeReads {
    /// (a) The queue's recorded marks ([`marks::RECORDED_KINDS`]) that took
    /// effect in `(from_ms, to_ms]`, whenever and by whatever clock written:
    /// a mark by its `at`, a pruned stop by its `last_heartbeat_at`, the
    /// others by their time.
    fn recorded_effective(&self, from_ms: i64, to_ms: i64) -> Read<Vec<RunEvent>>;

    /// (b) The retractions that name `mark`, whenever written.
    fn retractions_of(&self, mark: EventId) -> Read<Vec<RunEvent>>;

    /// (c) The runs' claims written in `(from_ms, to_ms]`.
    fn claims_written(&self, from_ms: i64, to_ms: i64) -> Read<Vec<RunEvent>>;

    /// (d) The last claim with an id before `before` that records
    /// `attribute` ([`marks::claim_value`]).
    fn previous_claim_with(&self, attribute: &str, before: EventId) -> Read<Option<RunEvent>>;

    /// (d) The first supervisor's start with an id in `(after, before)`
    /// whose `key` is `value`, whatever its time.
    fn start_announcing(
        &self,
        key: &str,
        value: &str,
        after: EventId,
        before: EventId,
    ) -> Read<Option<RunEvent>>;

    /// (e) The supervisor's start last in `(time, id)` at or before `ms`.
    fn start_before_in_time(&self, ms: i64) -> Read<Option<RunEvent>>;

    /// (f) The latest time of the visible events with an id at or before
    /// `cursor` ([`marks::cursor_millis`] of the events the snapshot sees):
    /// first among a fixed number of the latest events by `(time, id)`;
    /// else the aggregate `MAX(time)` of the events at or before `cursor`,
    /// taken when a visible event has that time (so a past cursor with any
    /// history after it is found); else a walk down the time index among
    /// the events at or before `cursor`, past invisible ones, at most the
    /// same number of candidates. `None` when no visible event is at or
    /// before it, and [`Undecided::OutOfTimeOrder`] only when that walk ran
    /// out.
    fn cursor_time(&self, cursor: EventId) -> Read<Option<i64>>;

    /// (g) The forecast targets' finishes (a task completed or canceled, a
    /// goal closed) written in `(from_ms, to_ms]`.
    fn finishes_written(&self, from_ms: i64, to_ms: i64) -> Read<Vec<RunEvent>>;

    /// (g) The last finish of `finish`'s target with an id before it.
    fn previous_finish(&self, finish: &RunEvent) -> Read<Option<RunEvent>>;

    /// (g) The first event of `finish`'s target (no snapshot forecasts a
    /// target before it exists).
    fn first_of_target(&self, finish: &RunEvent) -> Read<Option<RunEvent>>;

    /// (g) The forecast snapshots with an id in `(after, before)`.
    fn snapshots_between(&self, after: EventId, before: EventId) -> Read<Vec<RunEvent>>;

    /// (g) The changes of a task's plan ([`TASK_MARK_KINDS`]) written in
    /// `(from_ms, to_ms]`.
    fn plan_changes_written(&self, from_ms: i64, to_ms: i64) -> Read<Vec<RunEvent>>;

    /// (g) The queue's recorded marks and the changes of a task's plan with
    /// an id after `after`, up to now.
    fn marks_after(&self, after: EventId) -> Read<Vec<RunEvent>>;

    /// (h) Each run's first end ([`crate::domain::stats::window::ends_run`],
    /// the `finished_event_id` of `stats`): its task and its time, over the
    /// whole history.
    fn run_finishes(&self) -> Read<Vec<(TaskId, i64)>>;

    /// (i) Every event of [`marks::READ_KINDS`], over the whole history.
    fn comparison_events(&self) -> Read<Vec<RunEvent>>;
}

/// What the read of a question is: its range, and for `--compare` the
/// cohort its groups are cut of ([`super::kpi_with`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub range: ReadRange,
    pub cohort: Option<Cohort>,
}

/// Every mark and every run's finish (ascending; of `--goal`'s tasks only
/// with it), as [`super::compare`] cuts groups of them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cohort {
    pub marks: Vec<Mark>,
    pub finishes: Vec<i64>,
}

/// The range `query` needs at `now` (unix seconds) with `config`, and for
/// a comparison its cohort, or why it has none.
///
/// The periods (no `--since` / `--until`) start at the start of the
/// oldest period `kpi` computes ([`period_count`]: those listed, the
/// lookback of `baseline_7d` and twice a target's streak), counted back
/// from the period that holds `--at` (now without), and end with that
/// period when `--at` is given; without it they reach now. `--since` and
/// `--until` are one window and the one as long before it. An event id as
/// `--at`, `--since`, `--until` or a `--compare` cursor is the time
/// [`RangeReads::cursor_time`] finds for it. `--by` and `--goal` do not
/// change the range: the layers are of the runs of the range, and `--goal`
/// keeps the tasks the window's attributes map to it (`task_goals`),
/// whatever their time.
///
/// A comparison's groups are not of a range: they are cut of every mark
/// and every run's finish ([`cohort`]); the windows they give (around the
/// group a cursor names, or the two `--compare` windows) join the range.
///
/// [`Undecided`] when the question starts at the first event (`--until`
/// without `--since`), when an event cursor's time is not found within
/// the time index's limit, when a small read of the cohort fell back, and
/// for what `kpi` refuses or [`KpiQuery::read_whole_ledger`] asks.
pub fn plan(
    query: &KpiQuery,
    config: &KpiConfig,
    now: i64,
    utc_offset_secs: i64,
    reads: &impl RangeReads,
    goals: &HashMap<TaskId, Option<GoalId>>,
) -> Result<Plan, Undecided> {
    if query.read_whole_ledger {
        return Err(Undecided::WholeLedger);
    }
    if query.last == 0 {
        return Err(Undecided::NoPeriods);
    }
    let now_ms = now * 1000;
    let offset_ms = utc_offset_secs * 1000;
    let time = |cursor: Cursor| -> Read<i64> {
        match cursor {
            Cursor::Time(ms) => Ok(ms),
            Cursor::Event(id) => reads.cursor_time(id)?.ok_or(Undecided::NoEvent(id)),
        }
    };
    let mut range = if query.since.is_some() || query.until.is_some() {
        let Some(since) = query.since else {
            return Err(Undecided::FromFirstEvent);
        };
        let end = query.until.map(time).transpose()?;
        let start = time(since)?;
        let end_ms = end.unwrap_or(now_ms);
        if start >= end_ms {
            return Err(Undecided::Refused);
        }
        ReadRange {
            from_ms: start - (end_ms - start),
            until_ms: end,
        }
    } else {
        let anchor = query.at.map(time).transpose()?;
        let at = anchor.unwrap_or(now_ms);
        let oldest = period_count(query, config) - 1;
        ReadRange {
            from_ms: period_start_back(query.period, at, offset_ms, oldest),
            until_ms: anchor.map(|at| {
                query
                    .period
                    .start(at, offset_ms)
                    .saturating_add(query.period.length())
            }),
        }
    };
    let Some(spec) = query.compare else {
        return Ok(Plan {
            range,
            cohort: None,
        });
    };
    let cohort = cohort(reads, query.goal_id, goals)?;
    let cursor_error = std::cell::Cell::new(None);
    let windows = compare::split_windows(
        &cohort.marks,
        &cohort.finishes,
        config.min_samples,
        spec,
        query,
        |cursor| match time(cursor) {
            Ok(ms) => Some(ms),
            Err(undecided) => {
                cursor_error.set(cursor_error.get().or(Some(undecided)));
                None
            }
        },
    );
    if let Some(undecided) = cursor_error.get() {
        return Err(undecided);
    }
    let (_, before, after) = windows.map_err(|_| Undecided::Refused)?;
    range = range.union(ReadRange {
        from_ms: before.0,
        until_ms: Some(after.1),
    });
    Ok(Plan {
        range,
        cohort: Some(cohort),
    })
}

/// The cohort of a comparison's groups ([`super::compare`]'s
/// `overlapping_groups` and `named_change`): the marks of every event of
/// [`marks::READ_KINDS`] (read (i)) and every run's first end (read (h)),
/// those of `goal`'s tasks by `goals` (the window's `task_goals`) with it.
///
/// Whole, not a range: a group joins a mark to the one before it while
/// fewer than `min_samples` runs finished between them, so where a group
/// begins and ends depends on marks and finishes outside any range. Spike
/// 4135's (v) found that even a search that widens the range until
/// `min_samples` finishes close the group reads, between those finishes
/// and the group, the claims of runs not finished and the routine marks,
/// as many as the history outside the range holds; so the cohort is read
/// whole, of the kinds alone (no ledger row's records), and scales with
/// every claim and mark.
pub fn cohort(
    reads: &impl RangeReads,
    goal: Option<GoalId>,
    goals: &HashMap<TaskId, Option<GoalId>>,
) -> Read<Cohort> {
    let events = reads.comparison_events()?;
    let mut finishes: Vec<i64> = reads
        .run_finishes()?
        .into_iter()
        .filter(|(task, _)| {
            goal.is_none_or(|goal| goals.get(task).copied().flatten() == Some(goal))
        })
        .map(|(_, ms)| ms)
        .collect();
    finishes.sort_unstable();
    Ok(Cohort {
        marks: marks::marks(&events, None, None),
        finishes,
    })
}

/// The events by id, once each.
fn dedup(events: impl IntoIterator<Item = RunEvent>) -> Vec<RunEvent> {
    let by_id: BTreeMap<EventId, RunEvent> = events.into_iter().map(|e| (e.id, e)).collect();
    by_id.into_values().collect()
}

/// What the derived marks of `claims` are read off besides them: for each
/// claim and each attribute it records, the last claim before it by id
/// that records the attribute, and when the value changed, the start
/// between the two by id that announces the new one (a change a start
/// announced is the start's mark, not a derived one).
fn derived_reads(reads: &impl RangeReads, claims: &[RunEvent]) -> Read<Vec<RunEvent>> {
    let mut earlier = Vec::new();
    for claim in claims {
        for (attribute, start_key) in DERIVED {
            let Some(value) = claim_value(&claim.payload, attribute) else {
                continue;
            };
            let Some(previous) = reads.previous_claim_with(attribute, claim.id)? else {
                continue;
            };
            if let Some(key) = start_key
                && claim_value(&previous.payload, attribute).as_ref() != Some(&value)
            {
                earlier.extend(reads.start_announcing(key, &value, previous.id, claim.id)?);
            }
            earlier.push(previous);
        }
    }
    Ok(earlier)
}

/// The recorded marks of `recorded` with their retractions.
fn with_retractions(reads: &impl RangeReads, recorded: Vec<RunEvent>) -> Read<Vec<RunEvent>> {
    let mut events = Vec::new();
    for mark in &recorded {
        if mark.kind.is_in(&marks::RETRACTABLE_KINDS) {
            events.extend(reads.retractions_of(mark.id)?);
        }
    }
    events.extend(recorded);
    Ok(events)
}

/// The events [`marks::marks`] of any span inside `(from_ms, to_ms]` and
/// [`super::compare`]'s routine of its marks need, ascending id: the
/// recorded marks that took effect in it (a) with their retractions (b),
/// the claims written in it (c), for each of those its previous claim per
/// attribute and the start between them that announced the new value (d),
/// and the start last in time at or before `from_ms`, which routine
/// compares the range's first start with (e).
///
/// Given these, the marks and routine are those of every event: none of
/// the reads depends on the history outside the range beyond one seek per
/// event of the range, and none takes an id's order for a time's or the
/// other way round, so a clock turned back, a CLI's clock ahead of the DB
/// or a heartbeat before the stop changes no answer (the module's "The
/// clocks").
pub fn range_events(reads: &impl RangeReads, from_ms: i64, to_ms: i64) -> Read<Vec<RunEvent>> {
    let recorded = with_retractions(reads, reads.recorded_effective(from_ms, to_ms)?)?;
    let claims = reads.claims_written(from_ms, to_ms)?;
    let earlier = derived_reads(reads, &claims)?;
    let start = reads.start_before_in_time(from_ms)?;
    Ok(dedup(
        recorded
            .into_iter()
            .chain(claims)
            .chain(earlier)
            .chain(start),
    ))
}

/// The events [`score::score`] needs for the samples and exclusions of
/// the finishes in `(from_ms, to_ms]`, ascending id, in two steps.
///
/// First, the targets' finishes written in the range and each one's
/// previous finish of the same target by id (or its target's first event
/// when it has none): a snapshot counts toward a finish only after the
/// finish before it. Second, of those: every snapshot with an id between
/// the earliest previous finish and the latest finish of the range; the
/// recorded marks and the changes of a task's plan recorded after the
/// earliest positive `triggers_through` of those snapshots (a mark is
/// after a snapshot by its id however far back it took effect, so this
/// reaches now, past the range); those that took effect or were written
/// from the earliest snapshot's time to `to_ms` (a snapshot without
/// `triggers_through`, and a derived mark, count by time); the
/// retractions of the marks; and the claims written in that span with
/// what their derived marks are read off ([`range_events`]'s (d)).
///
/// The second step's span is set by where the first step's finishes'
/// targets were last finished or created, not by the question, so no
/// [`KpiQuery`] bounds it. What it reads is bounded by the open periods
/// of the targets that finish in the range (from each one's previous
/// finish, or its creation, to the range's end): the snapshots, claims
/// and marks of those periods, and the marks recorded after the earliest
/// snapshot up to now. A target's snapshot far before the range therefore
/// brings the marks between it and the range, as many as were recorded in
/// that time (Spike 4756's (ii-a)): those are counted by the sample's
/// `marks_between`, so they are read here rather than leaving the range
/// undecided. Nothing before those periods is read: neither the history
/// before a finishing target's previous finish or creation, nor that of
/// a target that does not finish in the range.
pub fn forecast_events(reads: &impl RangeReads, from_ms: i64, to_ms: i64) -> Read<Vec<RunEvent>> {
    let finishes = reads.finishes_written(from_ms, to_ms)?;
    let Some(high) = finishes.iter().map(|finish| finish.id).max() else {
        return Ok(Vec::new());
    };
    let mut previous = Vec::new();
    let mut low = high;
    for finish in &finishes {
        let bound = match reads.previous_finish(finish)? {
            Some(before) => {
                let id = before.id;
                previous.push(before);
                id
            }
            None => reads
                .first_of_target(finish)?
                .map_or(EventId::new(0), |first| EventId::new(first.id.as_i64() - 1)),
        };
        low = low.min(bound);
    }
    let snapshots = reads.snapshots_between(low, EventId::new(high.as_i64() + 1))?;
    let Some(at_secs) = snapshots
        .iter()
        .filter_map(|snapshot| snapshot.payload["at_secs"].as_i64())
        .min()
    else {
        return Ok(dedup(finishes.into_iter().chain(previous)));
    };
    let at = at_secs.saturating_mul(1000);
    let seen = snapshots
        .iter()
        .filter_map(|snapshot| snapshot.payload["triggers_through"].as_i64())
        .filter(|&through| through > 0)
        .min();
    let mut recorded = reads.recorded_effective(at, to_ms)?;
    let mut plan_changes = reads.plan_changes_written(at, to_ms)?;
    if let Some(seen) = seen {
        for event in reads.marks_after(EventId::new(seen))? {
            if event.kind.is_in(&TASK_MARK_KINDS) {
                plan_changes.push(event);
            } else {
                recorded.push(event);
            }
        }
    }
    let recorded = with_retractions(reads, dedup(recorded))?;
    let claims = reads.claims_written(at, to_ms)?;
    let earlier = derived_reads(reads, &claims)?;
    Ok(dedup(
        finishes
            .into_iter()
            .chain(previous)
            .chain(snapshots)
            .chain(recorded)
            .chain(plan_changes)
            .chain(claims)
            .chain(earlier),
    ))
}

/// The forecast's scoring of the finishes in `(from_ms, to_ms]` of
/// [`forecast_events`]: [`score::score`] of them and their marks, cut to
/// the samples and exclusions that finished in the range, as a window's
/// forecast KPIs keep them.
pub fn forecast_scoring(reads: &impl RangeReads, from_ms: i64, to_ms: i64) -> Read<Scoring> {
    let events = forecast_events(reads, from_ms, to_ms)?;
    let marks = marks::marks(&events, None, None);
    let scoring = score::score(&events, &marks);
    let within = |ms: i64| ms > from_ms && ms <= to_ms;
    Ok(Scoring {
        samples: scoring
            .samples
            .into_iter()
            .filter(|sample| within(sample.finished_ms))
            .collect(),
        excluded: scoring
            .excluded
            .into_iter()
            .filter(|(_, ms, _)| within(*ms))
            .collect(),
    })
}

#[cfg(test)]
mod tests;
