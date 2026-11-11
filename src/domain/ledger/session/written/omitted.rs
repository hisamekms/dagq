//! What the ledger holds of the queue's events of the kinds it does not
//! copy to queue rows ([`crate::domain::ledger::queue::OMITTED_KINDS`],
//! ADR-t4644-1 decision 2, docs/design/measurement.md "台帳を作る係"): an
//! event of those kinds whose row [`owner::owner`] says is the queue's
//! (below "omitted") needs no row, so what builds the ledger takes it as
//! recorded as it reads it, and the store holds instead what a reader of a
//! window's edges needs of those after the position C ([`Coverage`]).
//!
//! - **C** (`from`): where the first pass that held no coverage started to
//!   read (0 for a builder from its cursors' start, R for one gone on from
//!   R), lowered to zero only when a complete rebuild is published.
//! - **S** (`through`): the position the builder read up to, and with it the
//!   summary of each kind over (C, S] ([`KindSummary`]) and its points
//!   ([`Point`]). S and the summaries are written together, in the
//!   transaction of the marks W and R ([`super::Marks::omitted`]), so that
//!   they always cover the same range: the store writes them only over a
//!   coverage of the same C and kinds and a lower S, or a private
//!   rebuild further along. A complete rebuild alone lowers C to zero and
//!   replaces all public points (ADR-t4665-1 decision 1)
//!   ([`Coverage::supersedes`]), refuses the whole write (its rows, W and
//!   R too) over one it conflicts with ([`Coverage::conflicts`]: another
//!   builder's of another C, or further), and never removes them, whatever the
//!   [`crate::domain::ledger::LEDGER_V`], W and R (a raised version, a W cut
//!   back, a mark with no R keep them).
//! - **Stopped** ([`stops`]): a store whose R is past its S (a builder of a
//!   version that knows no S moved R) or whose kinds are not the builder's
//!   keeps C, S and the summaries as they are, and the builder neither moves
//!   nor lowers them ([`Covering::stopped`]): the omitted events in (C, S]
//!   stay without rows, those after S are queue rows' records as before
//!   ([`Covering::unwritten`]). Nothing here ends a stop.
//!
//! A builder that finds the store's coverage another than its own (another
//! builder wrote first) takes the store's and reads again the events from
//! its S up to where it read ([`Covering::behind`]) before it writes, so
//! that S never moves past events its summaries lack, and it keeps the
//! points it has not written until the store holds them.
//!
//! The summaries and points read only the events after S, so an event read
//! again (a builder gone on from R, or from its cursors' start) adds to
//! none twice. Pure: it reads and writes nothing.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::ledger::owner::{self, Owner};
use crate::domain::ledger::queue::OMITTED_KINDS;
use crate::domain::stats::timestamp_millis;
use crate::domain::{EventId, RunEvent};

/// The kind of the coverage's row in the ledger's table: no
/// [`crate::domain::ledger::LedgerKind`], its key [`super::KEY`].
pub const KIND: &str = "omitted_written";

/// A point ([`Point`]) is placed at every `POINT_EVERY`th event of a kind
/// read after C, of whichever row: so that between two points of a kind
/// there are at most that many of its events, whatever share of them is
/// a run's, a task's or a session's (owner reads the payload too, so no
/// index of the store tells them apart). A reader of the omitted events
/// near an edge reads at most that many events past a point.
pub const POINT_EVERY: u64 = 256;

/// The kinds a builder does not copy to queue rows and the spacing of its
/// points: the version's ([`Self::of_version`]), a test's set otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Omission {
    /// Sorted, each once.
    pub kinds: Vec<String>,
    pub every: u64,
}

impl Omission {
    /// The kinds of this version ([`OMITTED_KINDS`]), a point every
    /// [`POINT_EVERY`].
    pub fn of_version() -> Self {
        Self::of(OMITTED_KINDS.iter().map(|kind| kind.as_str()), POINT_EVERY)
    }

    /// The set of `kinds`, a point every `every` (at least 1) of a kind.
    pub fn of<'a>(kinds: impl IntoIterator<Item = &'a str>, every: u64) -> Self {
        let mut kinds: Vec<String> = kinds.into_iter().map(str::to_owned).collect();
        kinds.sort();
        kinds.dedup();
        Self {
            kinds,
            every: every.max(1),
        }
    }
}

/// What the store holds of a kind's omitted events in (C, S]. `last` (L)
/// is the last one's id, those of no time that parses too; `timed` (T) the
/// last one whose time parses; `max_ms` (M) the latest of their times in
/// the order of their ids; `back_ms` (D) the most a time fell behind the M
/// before it (0 or more), of those whose time parses only. An event of no
/// time is the latest of a window (the window's latest id) and no time's
/// answer (a time cursor, `queue_before`, a window's first time), so it
/// moves L only. `seen` counts the kind's events in (C, S] of every row,
/// a run's, a task's and a session's too, for its points.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timed: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ms: Option<i64>,
    #[serde(default)]
    pub back_ms: i64,
    #[serde(default)]
    pub seen: u64,
}

/// A point of a kind at the position `at` (p), an event of the kind of any
/// row: its summary's M, L and T up to and including it. Each only rises
/// with `at`, so the store holds the points where a read seeks them by
/// position or by time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub kind: String,
    pub at: EventId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timed: Option<EventId>,
}

/// C, S and the summary of each of `kinds` over (C, S] (every kind of the
/// set, those of no event in it too).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    /// Unpublished fold from the first event. Window readers ignore it.
    /// Kept in the coverage row's detail, apart from the public points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebuilding: Option<Rebuilding>,
    /// A publication's expected C, carried across the service boundary.
    /// The store clears it before persisting the coverage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacing: Option<EventId>,
    pub kinds: Vec<String>,
    pub from: EventId,
    pub through: EventId,
    pub summaries: BTreeMap<String, KindSummary>,
}

impl Coverage {
    /// The coverage of nothing yet from `from` (C = S).
    pub fn start(kinds: &[String], from: EventId) -> Self {
        Self {
            rebuilding: None,
            replacing: None,
            kinds: kinds.to_vec(),
            from,
            through: from,
            summaries: kinds
                .iter()
                .map(|kind| (kind.clone(), KindSummary::default()))
                .collect(),
        }
    }

    /// What a store gives for a coverage it holds and cannot read (one of
    /// a later shape): no kind's, its S past every position, so that it
    /// stops every builder ([`stops`]) and no coverage supersedes it.
    pub fn unreadable() -> Self {
        Self {
            rebuilding: None,
            replacing: None,
            kinds: vec![String::new()],
            from: EventId::new(0),
            through: EventId::new(i64::MAX),
            summaries: BTreeMap::new(),
        }
    }

    /// Fold `events` (ascending id) after S into the summaries, the points
    /// placed into `points`, and move S to `to` (the position read, no
    /// lower than the last of them) if it is past S.
    pub fn read(&mut self, events: &[RunEvent], every: u64, to: EventId, points: &mut Vec<Point>) {
        for event in events.iter().filter(|event| event.id > self.through) {
            let Some(summary) = self.summaries.get_mut(event.kind.as_str()) else {
                continue;
            };
            if owner::owner(event) == Owner::Queue {
                summary.last = Some(event.id);
                if let Some(at) = timestamp_millis(&event.created_at) {
                    if let Some(max) = summary.max_ms {
                        summary.back_ms = summary.back_ms.max(max.saturating_sub(at));
                    }
                    summary.max_ms = Some(summary.max_ms.map_or(at, |max| max.max(at)));
                    summary.timed = Some(event.id);
                }
            }
            summary.seen += 1;
            if summary.seen % every.max(1) == 0 {
                points.push(Point {
                    kind: event.kind.as_str().to_owned(),
                    at: event.id,
                    max_ms: summary.max_ms,
                    last: summary.last,
                    timed: summary.timed,
                });
            }
        }
        let last = events.last().map_or(self.through, |event| event.id);
        self.through = self.through.max(to).max(last);
    }

    /// Refuse another C (except an explicit publication over that C),
    /// another set, a lower S, or a missing/backwards private checkpoint.
    /// A stale writer therefore cannot undo either public or private work.
    pub fn conflicts(&self, stored: Option<&Self>) -> bool {
        stored.is_some_and(|stored| {
            let publication = self.from == EventId::new(0) && self.replacing == Some(stored.from);
            (!publication && stored.from != self.from)
                || stored.kinds != self.kinds
                || stored.through > self.through
                || (!publication
                    && stored.rebuilding.as_ref().is_some_and(|held| {
                        self.rebuilding.as_ref().is_none_or(|next| {
                            held.positions
                                .iter()
                                .any(|(kind, at)| next.positions.get(kind) < Some(at))
                        })
                    }))
        })
    }

    /// Write a nonconflicting advance of S or the private checkpoint,
    /// or a publication. Equal public S still permits private progress.
    pub fn supersedes(&self, stored: Option<&Self>) -> bool {
        !self.conflicts(stored)
            && stored.is_none_or(|stored| {
                self.from != stored.from
                    || stored.through < self.through
                    || stored.rebuilding != self.rebuilding
            })
    }
}

/// Private rebuild checkpoints: each kind's position and the same fold
/// as the public coverage, with its own points. No reader uses these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rebuilding {
    pub positions: BTreeMap<String, EventId>,
    pub summaries: BTreeMap<String, KindSummary>,
    pub points: Vec<Point>,
}

/// What a pass writes of the coverage with the marks: the coverage, and
/// the points placed since the one the store holds (none as a store reads
/// the coverage back, the points being read apart).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Omitted {
    pub coverage: Coverage,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub points: Vec<Point>,
}

/// Whether a builder of `kinds` stops on a store holding `stored` and the
/// mark R at `r`: it holds a coverage, and its kinds are not `kinds` or R
/// is past its S. A store of no coverage stops none.
pub fn stops(stored: Option<&Coverage>, r: Option<EventId>, kinds: &[String]) -> bool {
    stored.is_some_and(|stored| stored.kinds != kinds || r.is_some_and(|r| r > stored.through))
}

/// What a builder holds of the coverage between passes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Covering {
    state: State,
    /// The points placed past the S the store holds.
    unwritten: Vec<Point>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum State {
    /// No pass began yet.
    #[default]
    Unknown,
    /// No kind omitted and no coverage held: nothing to write.
    Off,
    /// [`stops`]: nothing written. The store's coverage as it stopped it,
    /// whose (C, S] stays without rows ([`Covering::unwritten`]).
    Stopped(Coverage),
    Held(Coverage),
}

impl Covering {
    /// At the start of a pass that reads after `at`, by the store's
    /// coverage `stored` and mark R at `r`: stop ([`stops`]); go on with
    /// the one held when the store's is of its C and no further; else take
    /// the store's (a new builder, or another writer's), to be read up to
    /// `at` ([`Self::behind`]) when its S is before it; start one at `at`
    /// (C) when the store holds none and some kind is omitted. A stop
    /// stays.
    pub fn begin(
        &mut self,
        omission: &Omission,
        (stored, r): (Option<&Coverage>, Option<EventId>),
        at: EventId,
    ) {
        if self.stopped() {
            return;
        }
        if let Some(stored) = stored.filter(|stored| stops(Some(stored), r, &omission.kinds)) {
            self.state = State::Stopped(stored.clone());
            self.unwritten.clear();
            return;
        }
        let held = match &self.state {
            State::Held(held) => Some(held),
            _ => None,
        };
        self.state = match (stored, held) {
            (None, None) if omission.kinds.is_empty() => State::Off,
            (None, None) => State::Held(Coverage::start(&omission.kinds, at)),
            (None, Some(held)) => State::Held(held.clone()),
            (Some(stored), Some(held))
                if !held.conflicts(Some(stored)) && held.through >= stored.through =>
            {
                State::Held(held.clone())
            }
            (Some(stored), _) => {
                self.unwritten.clear();
                State::Held(stored.clone())
            }
        };
        if let Some(stored) =
            stored.filter(|stored| self.held().is_some_and(|held| held.from == stored.from))
        {
            self.wrote(stored.through);
        }
    }

    /// The S of the coverage held when it is before `at`, where the builder
    /// read up to: the events after it up to `at` are to be read again
    /// ([`Self::read`]) before anything is written.
    pub fn behind(&self, at: EventId) -> Option<EventId> {
        self.held()
            .map(|held| held.through)
            .filter(|through| *through < at)
    }

    /// Fold `events` read up to `to` ([`Coverage::read`]) while one is held.
    pub fn read(&mut self, events: &[RunEvent], every: u64, to: EventId) {
        if let State::Held(held) = &mut self.state {
            held.read(events, every, to, &mut self.unwritten);
        }
    }

    /// What the next write of the marks carries: the coverage held with the
    /// points not written yet.
    pub fn to_write(&self) -> Option<Omitted> {
        match &self.state {
            State::Held(coverage) => Some(Omitted {
                coverage: coverage.clone(),
                points: self.unwritten.clone(),
            }),
            _ => None,
        }
    }

    /// Fold at most `limit` events of each kind (the runtime uses 256)
    /// after its saved P, through the current pass's updated S only. The
    /// indexed kind read excludes P, so no event is missed or folded twice;
    /// the reader certifies the empty remainder through S. P never exceeds
    /// S. With no arrivals it takes ceil(kind events / limit) passes (at least
    /// one for an empty kind); a bounded tail probe certifies a full final
    /// page: SQL seeks just an id; service clients read at most one extra
    /// event, never folded. Thus a pass reads at most 257 events per kind
    /// (771 for the three kinds). Each fold starts strictly after saved P,
    /// so the probe cannot duplicate a fold.
    /// With continuing arrivals it catches up when each kind's arrivals per
    /// pass are fewer than the limit; otherwise only publication waits.
    /// The public fold still advances normally. No prefix/suffix merge is
    /// possible from its summaries. All kinds publish together, replacing
    /// the public points, in the same transaction as S, W and R. A stopped
    /// coverage does nothing; an error leaves the entire rebuild unchanged.
    pub fn rebuild<E>(
        &mut self,
        every: u64,
        limit: usize,
        mut read: impl FnMut(&str, EventId, EventId, usize) -> Result<(Vec<RunEvent>, bool), E>,
    ) -> Result<(), E> {
        let State::Held(held) = &self.state else {
            return Ok(());
        };
        if held.from == EventId::new(0) || limit == 0 {
            return Ok(());
        }
        let mut next = held.clone();
        let rebuilding = next.rebuilding.get_or_insert_with(|| Rebuilding {
            positions: next
                .kinds
                .iter()
                .map(|k| (k.clone(), EventId::new(0)))
                .collect(),
            summaries: next
                .kinds
                .iter()
                .map(|k| (k.clone(), KindSummary::default()))
                .collect(),
            points: Vec::new(),
        });
        for kind in &next.kinds {
            let at = rebuilding.positions[kind];
            if at == next.through {
                continue;
            }
            let (events, complete) = read(kind, at, next.through, limit)?;
            let to = if complete {
                next.through
            } else {
                events.last().expect("full page").id
            };
            let mut fold = Coverage::start(std::slice::from_ref(kind), EventId::new(0));
            fold.through = at;
            fold.summaries
                .insert(kind.clone(), rebuilding.summaries[kind].clone());
            fold.read(&events, every, to, &mut rebuilding.points);
            rebuilding.positions.insert(kind.clone(), to);
            rebuilding
                .summaries
                .insert(kind.clone(), fold.summaries.remove(kind).expect("kind"));
        }
        if rebuilding.positions.values().all(|at| *at == next.through) {
            let rebuilding = next.rebuilding.take().expect("rebuild");
            next.replacing = Some(next.from);
            next.from = EventId::new(0);
            next.summaries = rebuilding.summaries;
            self.unwritten = rebuilding.points;
        }
        self.state = State::Held(next);
        Ok(())
    }

    /// Note that the store holds the coverage up to `through` and its points.
    pub fn wrote(&mut self, through: EventId) {
        self.unwritten.retain(|point| point.at > through);
        if let State::Held(held) = &mut self.state {
            held.replacing = None;
        }
    }

    /// Whether it stopped: it moves and lowers nothing the store holds.
    pub fn stopped(&self) -> bool {
        matches!(self.state, State::Stopped(_))
    }

    /// Which of the queue's events read no queue row records: of the
    /// coverage held, those after its C; of the coverage a stop holds, those
    /// in its (C, S] (those after S are rows again); none before a pass
    /// began, or when no kind is omitted and none is held.
    pub fn unwritten(&self) -> Unwritten {
        match &self.state {
            State::Held(coverage) => Unwritten {
                kinds: coverage.kinds.clone(),
                from: coverage.from,
                through: None,
            },
            State::Stopped(coverage) => Unwritten {
                kinds: coverage.kinds.clone(),
                from: coverage.from,
                through: Some(coverage.through),
            },
            State::Unknown | State::Off => Unwritten::default(),
        }
    }

    /// The coverage held, if one is.
    pub fn held(&self) -> Option<&Coverage> {
        match &self.state {
            State::Held(coverage) => Some(coverage),
            _ => None,
        }
    }
}

/// Which of the queue's events no queue row records (ADR-t4644-1 decision
/// 1): those [`owner::owner`] says are the queue's, of one of `kinds`, after
/// C (`from`) and, while the builder stops, up to S (`through`). The rest
/// are rows' records as before. The default is none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Unwritten {
    kinds: Vec<String>,
    from: EventId,
    through: Option<EventId>,
}

impl Unwritten {
    /// Those of `kinds` after `from`, up to `through` when given.
    pub fn of<'a>(
        kinds: impl IntoIterator<Item = &'a str>,
        from: EventId,
        through: Option<EventId>,
    ) -> Self {
        Self {
            kinds: kinds.into_iter().map(str::to_owned).collect(),
            from,
            through,
        }
    }

    /// Its C, when it omits some kind: no event up to it is omitted.
    pub fn from(&self) -> Option<EventId> {
        (!self.kinds.is_empty()).then_some(self.from)
    }

    /// Whether no queue row records `event`.
    pub fn omits(&self, event: &RunEvent) -> bool {
        !self.kinds.is_empty()
            && event.id > self.from
            && self.through.is_none_or(|through| event.id <= through)
            && self.kinds.iter().any(|kind| kind == event.kind.as_str())
            && owner::owner(event) == Owner::Queue
    }
}

#[cfg(test)]
mod tests;
