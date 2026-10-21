//! The waits before a claim, by supervisor (ADR-t1662-1 decision 6,
//! docs/design/measurement.md "claimの前"): a supervisor's slots all full
//! (`slots_full_started` / `slots_full_ended`, by class), the queue's hold
//! on the claims (`claim_held` / `claim_resumed`) and a task's deferred
//! claim (`claim_deferred` / `claim_deferral_ended`). [`pre_claim_intervals`]
//! makes their rows from the events alone, each row belonging to the
//! supervisor that recorded its start; the events and what their readers
//! make of them stay as they are, and the holds and deferrals end by the
//! rules `stats` reads them by ([`claim_hold::hold_spans`],
//! [`claim_defer::deferral_spans`]).

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{Value, json};

use super::{
    EventId, EventKind, LeaseToken, RunEvent, TaskId, claim_defer, claim_hold,
    light_slots::ClaimRoom,
    marks::utc_text,
    stats::timestamp_millis,
    supervisor_life::{self, LifeEnd},
};

/// The slots a class of tasks needs (ADR-t1591-1): a heavy task needs a
/// slot with the landing queue in it, a light one only the room the
/// landing queue leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotClass {
    Heavy,
    Light,
}

impl SlotClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Heavy => "heavy",
            Self::Light => "light",
        }
    }

    fn of(text: &str) -> Option<Self> {
        match text {
            "heavy" => Some(Self::Heavy),
            "light" => Some(Self::Light),
            _ => None,
        }
    }
}

/// The classes whose slots are all full when the next claim has `room`:
/// `heavy` without a free slot, `light` (only when the supervisor has
/// light changes) without even the light room.
pub fn full_classes(room: ClaimRoom, light: bool) -> BTreeSet<SlotClass> {
    let mut full = BTreeSet::new();
    if room != ClaimRoom::Any {
        full.insert(SlotClass::Heavy);
    }
    if light && room == ClaimRoom::None {
        full.insert(SlotClass::Light);
    }
    full
}

/// What a supervisor whose open intervals are of `open` records when its
/// pass finds the classes of `full` full: `slots_full_started` for each
/// class newly full, `slots_full_ended` for each one free again.
pub fn slots_full_changes(
    open: &BTreeSet<SlotClass>,
    full: &BTreeSet<SlotClass>,
) -> Vec<(EventKind, SlotClass)> {
    full.difference(open)
        .map(|class| (EventKind::SlotsFullStarted, *class))
        .chain(
            open.difference(full)
                .map(|class| (EventKind::SlotsFullEnded, *class)),
        )
        .collect()
}

/// The payload of a `slots_full_*` of the supervisor `token`: the class,
/// the classes whose intervals its records leave open once this one is
/// recorded (`open`, read back by [`recorded_open`]), the slots in use and
/// the slots it has.
pub fn slots_full_payload(
    token: &LeaseToken,
    class: SlotClass,
    open: &BTreeSet<SlotClass>,
    running: usize,
    slots: usize,
) -> Value {
    json!({
        "supervisor": token,
        "class": class.as_str(),
        "open": open.iter().map(|class| class.as_str()).collect::<Vec<_>>(),
        "running": running,
        "slots": slots,
    })
}

/// Record the changes from `open` to `full` ([`slots_full_changes`]) by
/// `record`, which takes each kind and class with the classes left open
/// once it is recorded and says whether it was. `open` follows each record
/// made and stays as it was for one that was not, which a later pass
/// records again: the newest record carries what the supervisor's records
/// leave open, whichever of them failed in between.
pub fn record_slots_full(
    open: &mut BTreeSet<SlotClass>,
    full: &BTreeSet<SlotClass>,
    mut record: impl FnMut(EventKind, SlotClass, &BTreeSet<SlotClass>) -> bool,
) {
    for (kind, class) in slots_full_changes(open, full) {
        let mut after = open.clone();
        if kind == EventKind::SlotsFullStarted {
            after.insert(class);
        } else {
            after.remove(&class);
        }
        if record(kind, class, &after) {
            *open = after;
        }
    }
}

/// The classes a `slots_full_*` says its supervisor's records leave open
/// ([`slots_full_payload`]'s `open`); `None` for a record without them,
/// of an earlier binary.
pub fn recorded_open(event: &RunEvent) -> Option<BTreeSet<SlotClass>> {
    event
        .payload
        .get("open")?
        .as_array()?
        .iter()
        .map(|class| class.as_str().and_then(SlotClass::of))
        .collect()
}

/// Whether the supervisor `token` has a heavy slot free (a slot with the
/// landing queue in it, so its claim may take any task), from the newest
/// `slots_full_*` of each supervisor (`newest`, however old): the heavy
/// class is free unless that record leaves it open ([`recorded_open`]).
/// A supervisor with no record never had its heavy slots full. A newest
/// record without `open` is of a supervisor still running an earlier
/// binary (a process of this one records every class at its start,
/// [`open_at_start`]): a heavy one says it by its kind, a light start says
/// the heavy slots full too, and a light end leaves them unknown, read as
/// full so that what that supervisor recorded holds as recorded.
pub fn heavy_slots_free(newest: &[RunEvent], token: &str) -> bool {
    let Some(event) = newest
        .iter()
        .filter(|event| text(event, "supervisor") == Some(token))
        .max_by_key(|event| event.id)
    else {
        return true;
    };
    match recorded_open(event) {
        Some(open) => !open.contains(&SlotClass::Heavy),
        None => {
            text(event, "class").and_then(SlotClass::of) == Some(SlotClass::Heavy)
                && event.kind == EventKind::SlotsFullEnded
        }
    }
}

/// What a process of a supervisor goes on from at its start, before its
/// first pass records the classes of `full`: what its newest record
/// (`newest`, however old) leaves open when it says so ([`recorded_open`]),
/// and none when it has no record (its slots were never full). A record of
/// an earlier binary says nothing of it, nor does a read that failed
/// (`newest` `None`): then every class is taken as the opposite of `full`,
/// so that pass records each class once, every record with what it leaves
/// open, and the newest record tells the state again. A start already open
/// goes on and an end with none open ends nothing ([`PreClaimBook::add`]).
pub fn open_at_start(
    newest: Option<Option<&RunEvent>>,
    full: &BTreeSet<SlotClass>,
) -> BTreeSet<SlotClass> {
    match newest {
        Some(None) => BTreeSet::new(),
        Some(Some(event)) if let Some(open) = recorded_open(event) => open,
        _ => [SlotClass::Heavy, SlotClass::Light]
            .into_iter()
            .filter(|class| !full.contains(class))
            .collect(),
    }
}

/// The kind of a wait before a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntervalKind {
    SlotsFull,
    ClaimHold,
    ClaimDeferral,
}

/// A slots-full interval ended by its supervisor's pass that found a slot.
pub const ENDED_FREED: &str = "freed";

/// One wait before a claim: the event that started it (`id`), the
/// supervisor that recorded it, its task (a deferral's), its reason (a
/// hold's or a deferral's) or class (slots full), and its end: when, how
/// and which supervisor recorded it (none for a silent supervisor's end
/// and for a deferral its `run_claimed` ended, which names no supervisor).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreClaimInterval {
    pub id: EventId,
    pub kind: IntervalKind,
    pub supervisor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<SlotClass>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub end: Option<String>,
    pub ended_by: Option<String>,
}

/// The waits before a claim in `events` (ascending id) at `now_ms`, by the
/// id of the event that started each. Slots full ends at its supervisor's
/// `slots_full_ended` ([`ENDED_FREED`]) or, without one, at the end of the
/// supervisor's life ([`supervisor_life::supervisor_life_end`],
/// [`claim_hold::ENDED_SUPERVISOR_GONE`]); another supervisor neither
/// starts nor ends it. A hold and a deferral end as `stats` reads them,
/// whichever supervisor recorded the end. Reads no state: the events and
/// `now_ms` are all. [`PreClaimBook`] holds the same as it is read.
pub fn pre_claim_intervals(events: &[RunEvent], now_ms: i64) -> Vec<PreClaimInterval> {
    let book = PreClaimBook::of(events);
    let lives = supervisor_life::Lives::new(events);
    book.intervals(&lives, now_ms)
        .map(Cow::into_owned)
        .collect()
}

/// The supervisor and the class a `slots_full_*` names; none for another
/// event, or one that names neither.
pub fn slots_full_of(event: &RunEvent) -> Option<(String, SlotClass)> {
    if event.kind != EventKind::SlotsFullStarted && event.kind != EventKind::SlotsFullEnded {
        return None;
    }
    Some((
        text(event, "supervisor")?.to_owned(),
        text(event, "class").and_then(SlotClass::of)?,
    ))
}

/// The holds on the landings of `events` (ascending id) as intervals in
/// the shape of the waits before a claim, each with the event that ended
/// it, by the same rule ([`claim_hold::hold_spans`]).
pub fn landing_hold_intervals(events: &[RunEvent]) -> Vec<(PreClaimInterval, Option<EventId>)> {
    PreClaimBook::of(events)
        .landing_holds()
        .map(|(interval, end)| (interval.clone(), end))
        .collect()
}

/// What an interval an event started is: a wait before a claim, or a hold
/// on the landings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Started {
    PreClaim(IntervalKind),
    LandingHold,
}

/// What one event did to the intervals ([`PreClaimBook::add`]): the one it
/// started (by its own id), and the starts of those its own record ended:
/// a wait before a claim's, then a hold on the landings'.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Moved {
    pub started: Option<Started>,
    pub ended: [Option<EventId>; 2],
}

/// The waits before a claim ([`pre_claim_intervals`]) and the holds on the
/// landings ([`landing_hold_intervals`]) of the events read so far, by the
/// id of the event that started each, read one event at a time and kept:
/// a reader that holds it reads no event twice. A slots-full interval
/// still open is ended by its supervisor's life as it is read
/// ([`Self::intervals`]), at that time, or once [`Self::close_lives`]
/// closed it at a time its supervisor's life had ended by: a later event
/// naming the supervisor opens it again, as its life may go on, and a read
/// at a time its life had not ended by (a clock set back) reads it open.
/// The book keeps when each supervisor with a slots full open ends at the
/// earliest, so that a close looks only at the supervisors an event named
/// since the last close and those whose end came.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreClaimBook {
    /// The waits ended, with the event of their own that ended each (none
    /// for slots full its supervisor's life ended).
    closed: BTreeMap<EventId, (PreClaimInterval, Option<EventId>)>,
    /// The waits open.
    open: BTreeMap<EventId, PreClaimInterval>,
    /// The open slots full of each supervisor and class, the open hold on
    /// the claims and each task's open deferral, by their starts.
    slots: BTreeMap<(String, SlotClass), EventId>,
    claim_hold: Option<EventId>,
    deferrals: BTreeMap<TaskId, EventId>,
    /// The holds on the landings, each with the event that ended it, and
    /// the one open.
    landings: BTreeMap<EventId, (PreClaimInterval, Option<EventId>)>,
    landing_hold: Option<EventId>,
    /// When the life of each supervisor with a slots full open ends at the
    /// earliest ([`supervisor_life::Lives::ends_from`]) as of the last
    /// close, by that time and by the supervisor; and the supervisors with
    /// one that an event named since, whose end the next close reads again.
    life_ends: BTreeSet<(i64, String)>,
    life_end_of: BTreeMap<String, i64>,
    named: BTreeSet<String>,
    /// The latest time a close was at: a read at an earlier one reads
    /// again whether the lives ended the waits they closed.
    closed_to: i64,
}

impl PreClaimBook {
    /// How many waits it holds, ended and open (the holds on the landings
    /// among them).
    pub fn waits(&self) -> usize {
        self.closed.len() + self.open.len() + self.landings.len()
    }

    /// Roughly how many bytes it holds: its waits with their texts, and
    /// what it finds them by.
    pub fn approx_bytes(&self) -> usize {
        let wait = |wait: &PreClaimInterval| {
            size_of::<(EventId, (PreClaimInterval, Option<EventId>))>()
                + [
                    wait.supervisor.as_deref(),
                    wait.reason.as_deref(),
                    Some(wait.started_at.as_str()),
                    wait.ended_at.as_deref(),
                    wait.end.as_deref(),
                    wait.ended_by.as_deref(),
                ]
                .into_iter()
                .flatten()
                .map(str::len)
                .sum::<usize>()
        };
        let finders = self.slots.len() + self.deferrals.len() + self.life_ends.len();
        self.closed
            .values()
            .chain(self.landings.values())
            .map(|(interval, _)| wait(interval))
            .chain(self.open.values().map(wait))
            .sum::<usize>()
            + finders * size_of::<(String, i64)>()
    }

    /// The book of `events` (ascending id).
    pub fn of(events: &[RunEvent]) -> Self {
        let mut book = Self::default();
        for event in events {
            book.add(event);
        }
        book
    }

    /// Read `event`, after every one read before.
    pub fn add(&mut self, event: &RunEvent) -> Moved {
        let mut moved = Moved::default();
        if let Some(supervisor) = text(event, "supervisor") {
            self.reopen(supervisor);
        }
        let mut ended: Option<EventId> = None;
        if let Some(key) = slots_full_of(event) {
            if event.kind == EventKind::SlotsFullStarted {
                // One already open goes on (a process that took over the
                // token and found it open records no new start).
                if let std::collections::btree_map::Entry::Vacant(open) = self.slots.entry(key) {
                    let (supervisor, class) = open.key().clone();
                    open.insert(event.id);
                    self.open.insert(
                        event.id,
                        PreClaimInterval {
                            id: event.id,
                            kind: IntervalKind::SlotsFull,
                            supervisor: Some(supervisor),
                            task_id: None,
                            reason: None,
                            class: Some(class),
                            started_at: event.created_at.clone(),
                            ended_at: None,
                            end: None,
                            ended_by: None,
                        },
                    );
                    moved.started = Some(Started::PreClaim(IntervalKind::SlotsFull));
                }
            } else if let Some(start) = self.slots.remove(&key) {
                self.close(start, event, ENDED_FREED, Some(key.0));
                ended = Some(ended.map_or(start, |first| first.min(start)));
            }
        }
        let opener = self
            .claim_hold
            .map(|start| self.open.get(&start).and_then(|o| o.supervisor.as_deref()));
        if let Some(why) = claim_hold::hold_end(claim_hold::CLAIMS, opener, event) {
            if let Some(start) = self.claim_hold.take() {
                let by = text(event, "supervisor").map(str::to_owned);
                self.close(start, event, why, by);
                ended = Some(ended.map_or(start, |first| first.min(start)));
            }
            if event.kind == claim_hold::CLAIMS.held {
                self.open
                    .insert(event.id, opened(IntervalKind::ClaimHold, event));
                self.claim_hold = Some(event.id);
                moved.started = Some(Started::PreClaim(IntervalKind::ClaimHold));
            }
        }
        if let Some((task, why)) = claim_defer::deferral_end(event) {
            if let Some(start) = self.deferrals.remove(&task) {
                let by = text(event, "supervisor").map(str::to_owned);
                self.close(start, event, why, by);
                ended = Some(ended.map_or(start, |first| first.min(start)));
            }
            if event.kind == EventKind::ClaimDeferred {
                self.open
                    .insert(event.id, opened(IntervalKind::ClaimDeferral, event));
                self.deferrals.insert(task, event.id);
                moved.started = Some(Started::PreClaim(IntervalKind::ClaimDeferral));
            }
        }
        moved.ended[0] = ended;
        if let Some(supervisor) = text(event, "supervisor")
            && !self.named.contains(supervisor)
            && (self.life_end_of.contains_key(supervisor) || self.has_slots_full(supervisor))
        {
            self.named.insert(supervisor.to_owned());
        }
        let opener = self.landing_hold.map(|start| {
            self.landings
                .get(&start)
                .and_then(|(o, _)| o.supervisor.as_deref())
        });
        if let Some(why) = claim_hold::hold_end(claim_hold::LANDINGS, opener, event) {
            if let Some(start) = self.landing_hold.take()
                && let Some((interval, end)) = self.landings.get_mut(&start)
            {
                ended_by(
                    interval,
                    event,
                    why,
                    text(event, "supervisor").map(str::to_owned),
                );
                *end = Some(event.id);
                moved.ended[1] = Some(start);
            }
            if event.kind == claim_hold::LANDINGS.held {
                self.landings
                    .insert(event.id, (opened(IntervalKind::ClaimHold, event), None));
                self.landing_hold = Some(event.id);
                moved.started = Some(Started::LandingHold);
            }
        }
        moved
    }

    fn close(&mut self, start: EventId, event: &RunEvent, why: &str, by: Option<String>) {
        if let Some(mut interval) = self.open.remove(&start) {
            ended_by(&mut interval, event, why, by);
            self.closed.insert(start, (interval, Some(event.id)));
        }
    }

    /// Open again the slots full of `supervisor` its life closed: an event
    /// naming it may move or undo that end.
    fn reopen(&mut self, supervisor: &str) {
        let mut key = (supervisor.to_owned(), SlotClass::Heavy);
        for class in [SlotClass::Heavy, SlotClass::Light] {
            key.1 = class;
            let Some(&start) = self.slots.get(&key) else {
                continue;
            };
            if matches!(self.closed.get(&start), Some((_, None)))
                && let Some((mut interval, _)) = self.closed.remove(&start)
            {
                unend(&mut interval);
                self.open.insert(start, interval);
            }
        }
    }

    /// Whether `supervisor` has a slots full not freed.
    fn has_slots_full(&self, supervisor: &str) -> bool {
        let mut key = (supervisor.to_owned(), SlotClass::Heavy);
        [SlotClass::Heavy, SlotClass::Light]
            .into_iter()
            .any(|class| {
                key.1 = class;
                self.slots.contains_key(&key)
            })
    }

    /// Close where they are kept the slots full whose supervisor's life
    /// ended by `now_ms` (`lives`, of the same events) as
    /// [`Self::intervals`] ends them then, so that a later read makes
    /// nothing of them; a later `now_ms` ends them the same way until an
    /// event names the supervisor again, and an earlier one reads them
    /// open while their lives had not ended then. It reads the ends of the
    /// supervisors named since the last close, and closes those of the
    /// supervisors whose end came by `now_ms`: none of the rest.
    pub fn close_lives(&mut self, lives: &supervisor_life::Lives, now_ms: i64) {
        self.closed_to = self.closed_to.max(now_ms);
        for supervisor in std::mem::take(&mut self.named) {
            let supervisor = match self.life_end_of.remove(&supervisor) {
                Some(at) => {
                    let kept = (at, supervisor);
                    self.life_ends.remove(&kept);
                    kept.1
                }
                None => supervisor,
            };
            if self.has_slots_full(&supervisor)
                && let Some(at) = lives.ends_from(&supervisor)
            {
                self.life_end_of.insert(supervisor.clone(), at);
                self.life_ends.insert((at, supervisor));
            }
        }
        while self.life_ends.first().is_some_and(|(at, _)| *at <= now_ms) {
            let Some((_, supervisor)) = self.life_ends.pop_first() else {
                break;
            };
            self.life_end_of.remove(&supervisor);
            let Some(life) = lives.end(&supervisor, now_ms) else {
                continue;
            };
            let mut key = (supervisor, SlotClass::Heavy);
            for class in [SlotClass::Heavy, SlotClass::Light] {
                key.1 = class;
                if let Some(&start) = self.slots.get(&key)
                    && let Some(mut interval) = self.open.remove(&start)
                {
                    end_with_life(&mut interval, life);
                    self.closed.insert(start, (interval, None));
                }
            }
        }
    }

    /// Every wait before a claim at `now_ms`, by the id of its start: a
    /// slots-full interval still open ends with its supervisor's life
    /// (`lives`, of the same events).
    pub fn intervals<'b>(
        &'b self,
        lives: &'b supervisor_life::Lives,
        now_ms: i64,
    ) -> impl Iterator<Item = Cow<'b, PreClaimInterval>> + 'b {
        let mut closed = self.closed.values().peekable();
        let mut open = self.open.values().peekable();
        std::iter::from_fn(move || {
            let from_closed = match (closed.peek(), open.peek()) {
                (Some((a, _)), Some(b)) => a.id < b.id,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => return None,
            };
            Some(if from_closed {
                let (interval, end) = closed.next()?;
                self.closed_at(interval, *end, lives, now_ms)
            } else {
                at_life(open.next()?, lives, now_ms)
            })
        })
    }

    /// The wait before a claim started by `start` at `now_ms`, as
    /// [`Self::intervals`] gives it, with the event that ended it.
    pub fn interval<'b>(
        &'b self,
        start: EventId,
        lives: &'b supervisor_life::Lives,
        now_ms: i64,
    ) -> Option<(Cow<'b, PreClaimInterval>, Option<EventId>)> {
        if let Some((interval, end)) = self.closed.get(&start) {
            return Some((self.closed_at(interval, *end, lives, now_ms), *end));
        }
        let open = self.open.get(&start)?;
        Some((at_life(open, lives, now_ms), None))
    }

    /// `interval`, kept closed by `end` (none when its supervisor's life
    /// closed it), at `now_ms`: one a life closed is open while that life
    /// has not ended by `now_ms`, which only a time before the latest close
    /// can find, as no event named the supervisor since.
    fn closed_at<'b>(
        &self,
        interval: &'b PreClaimInterval,
        end: Option<EventId>,
        lives: &supervisor_life::Lives,
        now_ms: i64,
    ) -> Cow<'b, PreClaimInterval> {
        let open = end.is_none()
            && now_ms < self.closed_to
            && interval
                .supervisor
                .as_deref()
                .is_some_and(|supervisor| lives.end(supervisor, now_ms).is_none());
        if !open {
            return Cow::Borrowed(interval);
        }
        let mut row = interval.clone();
        unend(&mut row);
        Cow::Owned(row)
    }

    /// The holds on the landings in the order they started, each with the
    /// event that ended it.
    pub fn landing_holds(&self) -> impl Iterator<Item = (&PreClaimInterval, Option<EventId>)> {
        self.landings
            .values()
            .map(|(interval, end)| (interval, *end))
    }

    /// The hold on the landings started by `start`, with the event that
    /// ended it.
    pub fn landing_hold(&self, start: EventId) -> Option<(&PreClaimInterval, Option<EventId>)> {
        self.landings
            .get(&start)
            .map(|(interval, end)| (interval, *end))
    }
}

/// A hold or a deferral `start` began, open.
fn opened(kind: IntervalKind, start: &RunEvent) -> PreClaimInterval {
    PreClaimInterval {
        id: start.id,
        kind,
        supervisor: text(start, "supervisor").map(str::to_owned),
        task_id: (kind == IntervalKind::ClaimDeferral)
            .then_some(start.task_id)
            .flatten(),
        reason: text(start, "reason").map(str::to_owned),
        class: None,
        started_at: start.created_at.clone(),
        ended_at: None,
        end: None,
        ended_by: None,
    }
}

/// `interval` as open again.
fn unend(interval: &mut PreClaimInterval) {
    interval.ended_at = None;
    interval.end = None;
    interval.ended_by = None;
}

fn ended_by(interval: &mut PreClaimInterval, event: &RunEvent, why: &str, by: Option<String>) {
    interval.ended_at = Some(event.created_at.clone());
    interval.end = Some(why.to_owned());
    interval.ended_by = by;
}

/// `interval`, open, at `now_ms`: a slots-full one ended at the end of its
/// supervisor's life, when that ended; else as it is.
fn at_life<'b>(
    interval: &'b PreClaimInterval,
    lives: &supervisor_life::Lives,
    now_ms: i64,
) -> Cow<'b, PreClaimInterval> {
    let life = match (&interval.kind, &interval.supervisor) {
        (IntervalKind::SlotsFull, Some(supervisor)) => lives.end(supervisor, now_ms),
        _ => None,
    };
    let Some(life) = life else {
        return Cow::Borrowed(interval);
    };
    let mut row = interval.clone();
    end_with_life(&mut row, life);
    Cow::Owned(row)
}

/// End `interval`, a slots-full one open, at the end of its supervisor's
/// `life`.
fn end_with_life(interval: &mut PreClaimInterval, life: LifeEnd) {
    let started = timestamp_millis(&interval.started_at).unwrap_or(life.at_ms());
    interval.ended_at = Some(utc_text(life.at_ms().max(started)));
    interval.end = Some(claim_hold::ENDED_SUPERVISOR_GONE.to_owned());
    interval.ended_by = match life {
        LifeEnd::Stopped { .. } => interval.supervisor.clone(),
        LifeEnd::Silent { .. } => None,
    };
}

fn text<'e>(event: &'e RunEvent, key: &str) -> Option<&'e str> {
    event.payload.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::claim_defer::claim_deferrals;
    use crate::domain::claim_hold::claim_holds;
    use crate::domain::event_kind::SLOTS_FULL_ENDED;
    use crate::domain::event_kind::SLOTS_FULL_STARTED;
    use crate::domain::supervisor_life::SUPERVISOR_ALIVE_INTERVAL_SECS;

    const MIN: i64 = 60_000;

    fn event(id: i64, task: Option<i64>, kind: &str, payload: Value, minute: i64) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: task.map(TaskId::new),
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: utc_text(minute * MIN),
            actor: None,
        }
    }

    fn full(id: i64, kind: &str, supervisor: &str, class: &str, minute: i64) -> RunEvent {
        event(
            id,
            None,
            kind,
            json!({"supervisor": supervisor, "class": class, "running": 2, "slots": 2}),
            minute,
        )
    }

    fn alive(id: i64, supervisor: &str, minute: i64) -> RunEvent {
        event(
            id,
            None,
            "supervisor_alive",
            json!({"supervisor": supervisor}),
            minute,
        )
    }

    /// (kind, supervisor, class or reason, started, ended, end, ended_by)
    type Row = (
        IntervalKind,
        String,
        String,
        i64,
        Option<i64>,
        Option<String>,
        Option<String>,
    );

    fn rows(rows: &[PreClaimInterval]) -> Vec<Row> {
        rows.iter()
            .map(|row| {
                (
                    row.kind,
                    row.supervisor.clone().unwrap_or_default(),
                    row.class
                        .map(|class| class.as_str().to_owned())
                        .or_else(|| row.reason.clone())
                        .unwrap_or_default(),
                    timestamp_millis(&row.started_at).unwrap() / MIN,
                    row.ended_at
                        .as_deref()
                        .map(|at| timestamp_millis(at).unwrap() / MIN),
                    row.end.clone(),
                    row.ended_by.clone(),
                )
            })
            .collect()
    }

    fn some(text: &str) -> Option<String> {
        Some(text.to_owned())
    }

    #[test]
    fn the_full_classes_follow_the_claim_room() {
        use SlotClass::{Heavy, Light};
        assert!(full_classes(ClaimRoom::Any, true).is_empty());
        assert_eq!(
            full_classes(ClaimRoom::LightOnly, true),
            BTreeSet::from([Heavy])
        );
        assert_eq!(
            full_classes(ClaimRoom::None, true),
            BTreeSet::from([Heavy, Light])
        );
        // Without light changes there is no light class.
        assert_eq!(
            full_classes(ClaimRoom::None, false),
            BTreeSet::from([Heavy])
        );
        let open = BTreeSet::from([Heavy]);
        assert_eq!(
            slots_full_changes(&open, &BTreeSet::from([Heavy, Light])),
            [(EventKind::SlotsFullStarted, Light)]
        );
        assert_eq!(
            slots_full_changes(&open, &BTreeSet::new()),
            [(EventKind::SlotsFullEnded, Heavy)]
        );
        assert!(slots_full_changes(&open, &open).is_empty());
        let payload = slots_full_payload(
            &LeaseToken::new("s"),
            Light,
            &BTreeSet::from([Heavy, Light]),
            3,
            3,
        );
        assert_eq!(
            payload,
            json!({"supervisor": "s", "class": "light", "open": ["heavy", "light"], "running": 3, "slots": 3})
        );
    }

    /// The supervisor `s`'s passes in `rooms`, recorded into `records` by
    /// [`record_slots_full`] with the records `fails` names (by pass and
    /// order within it) failing. After each pass the newest record carries
    /// what `s`'s records leave open, so it tells the heavy slots free
    /// exactly when they are free in the records, and as the room of the
    /// pass whenever that pass's records were all made.
    fn passes_through(
        rooms: &[(ClaimRoom, bool)],
        fails: &[(usize, usize)],
        records: &mut Vec<RunEvent>,
    ) {
        let mut open = BTreeSet::new();
        for (pass, &(room, light)) in rooms.iter().enumerate() {
            let full = full_classes(room, light);
            let mut index = 0;
            let mut failed = false;
            record_slots_full(&mut open, &full, |kind, class, after| {
                let fails_now = fails.contains(&(pass, index));
                index += 1;
                if fails_now {
                    failed = true;
                    return false;
                }
                let id = i64::try_from(records.len()).unwrap() + 1;
                records.push(event(
                    id,
                    None,
                    kind.as_str(),
                    slots_full_payload(&LeaseToken::new("s"), class, after, 1, 1),
                    id,
                ));
                true
            });
            let newest = records
                .iter()
                .rev()
                .find(|record| text(record, "supervisor") == Some("s"));
            if let Some(newest) = newest {
                assert_eq!(recorded_open(newest).as_ref(), Some(&open), "{pass}");
            }
            assert_eq!(
                heavy_slots_free(records, "s"),
                !open.contains(&SlotClass::Heavy),
                "{pass} {fails:?}"
            );
            if !failed {
                assert_eq!(open, full, "{pass} {fails:?}");
                assert_eq!(heavy_slots_free(records, "s"), room == ClaimRoom::Any);
            }
        }
    }

    /// Through every turn of the rooms, with the light changes on, off or
    /// turned off, and with any one or two of the records failing, the
    /// newest record says whether the heavy slots are free; another
    /// supervisor's records say nothing of it, and one with none has them
    /// free. Among the failures: the light class's start recorded and the
    /// heavy one's not, then a free slot before it is recorded again,
    /// whose light end leaves the heavy slots free.
    #[test]
    fn the_newest_record_tells_whether_the_heavy_slots_are_free() {
        use ClaimRoom::{Any, LightOnly, None as NoRoom};
        let other = vec![full(1, "slots_full_started", "other", "heavy", 0)];
        assert!(heavy_slots_free(&other, "s"));
        assert!(!heavy_slots_free(&other, "other"));
        // A supervisor still running an earlier binary, without `open`: a
        // heavy end frees the heavy slots, a light start fills them, and a
        // light end leaves them unknown, read as full.
        let earlier = [
            full(1, "slots_full_ended", "a", "heavy", 0),
            full(2, "slots_full_started", "b", "light", 0),
            full(3, "slots_full_ended", "c", "light", 0),
        ];
        assert!(heavy_slots_free(&earlier, "a"));
        assert!(!heavy_slots_free(&earlier, "b"));
        assert!(!heavy_slots_free(&earlier, "c"));
        let rooms = [
            (Any, true),
            (LightOnly, true),
            (NoRoom, true),
            (Any, true),
            (NoRoom, true),
            (LightOnly, true),
            (Any, true),
            (NoRoom, true),
            // The light changes turned off while no room is left.
            (NoRoom, false),
            (Any, false),
            (NoRoom, false),
            (LightOnly, true),
        ];
        // The heavy start fails, the light one is recorded, and the slot
        // frees before the heavy one is recorded again.
        let mut records = other.clone();
        passes_through(
            &[(Any, true), (NoRoom, true), (Any, true)],
            &[(1, 0)],
            &mut records,
        );
        assert!(heavy_slots_free(&records, "s"));
        let kinds: Vec<(&str, &str)> = records[1..]
            .iter()
            .map(|record| (record.kind.as_str(), text(record, "class").unwrap()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("slots_full_started", "light"),
                ("slots_full_ended", "light")
            ]
        );
        // Every one or two failures over the turns (a pass makes two
        // records at most).
        for first in 0..rooms.len() * 2 {
            for second in first..rooms.len() * 2 {
                let fails = [(first / 2, first % 2), (second / 2, second % 2)];
                passes_through(&rooms, &fails, &mut other.clone());
            }
        }
    }

    /// A process that takes the token over goes on from what its newest
    /// record leaves open; with none it has nothing open. After an earlier
    /// binary's records, here a free slot that recorded the heavy end then
    /// the light one, or a read that failed, its first pass records every
    /// class, and its newest record tells the heavy slots free.
    #[test]
    fn a_token_goes_on_from_its_newest_record_or_records_every_class() {
        use SlotClass::{Heavy, Light};
        let mut records = Vec::new();
        record_slots_full(
            &mut BTreeSet::new(),
            &BTreeSet::from([Heavy]),
            |kind, class, after| {
                let id = i64::try_from(records.len()).unwrap() + 1;
                let payload = slots_full_payload(&LeaseToken::new("s"), class, after, 1, 1);
                records.push(event(id, None, kind.as_str(), payload, id));
                true
            },
        );
        let any = BTreeSet::new();
        assert_eq!(
            open_at_start(Some(records.last()), &any),
            BTreeSet::from([Heavy])
        );
        assert_eq!(open_at_start(Some(None), &any), BTreeSet::new());
        let earlier = [
            full(1, SLOTS_FULL_ENDED, "s", "heavy", 0),
            full(2, SLOTS_FULL_ENDED, "s", "light", 0),
        ];
        assert!(!heavy_slots_free(&earlier, "s"));
        for newest in [Some(earlier.last()), None] {
            let mut open = open_at_start(newest, &any);
            assert_eq!(open, BTreeSet::from([Heavy, Light]));
            let mut records = earlier.to_vec();
            record_slots_full(&mut open, &any, |kind, class, after| {
                let id = i64::try_from(records.len()).unwrap() + 1;
                let payload = slots_full_payload(&LeaseToken::new("s"), class, after, 0, 1);
                records.push(event(id, None, kind.as_str(), payload, id));
                true
            });
            assert!(open.is_empty());
            assert_eq!(records.len(), 4);
            assert!(heavy_slots_free(&records, "s"));
        }
        // In the light room the first pass starts the heavy class again,
        // which goes on, and ends the light one.
        let light = BTreeSet::from([Heavy]);
        assert_eq!(
            slots_full_changes(&open_at_start(None, &light), &light),
            [
                (EventKind::SlotsFullStarted, Heavy),
                (EventKind::SlotsFullEnded, Light)
            ]
        );
    }

    /// One supervisor's classes open and close apart; a second supervisor
    /// running beside it whose slots never fill has no row, and a start
    /// repeated while open goes on with the first.
    #[test]
    fn slots_full_is_its_supervisors_by_class() {
        let events = [
            alive(1, "t", 0),
            full(2, SLOTS_FULL_STARTED, "s", "heavy", 1),
            full(3, SLOTS_FULL_STARTED, "s", "light", 2),
            full(4, SLOTS_FULL_ENDED, "s", "light", 4),
            full(5, SLOTS_FULL_STARTED, "s", "heavy", 5),
            full(6, SLOTS_FULL_ENDED, "s", "heavy", 6),
            alive(7, "t", 5),
            alive(8, "s", 6),
        ];
        let rows = rows(&pre_claim_intervals(&events, 7 * MIN));
        assert_eq!(
            rows,
            [
                (
                    IntervalKind::SlotsFull,
                    "s".to_owned(),
                    "heavy".to_owned(),
                    1,
                    Some(6),
                    some(ENDED_FREED),
                    some("s")
                ),
                (
                    IntervalKind::SlotsFull,
                    "s".to_owned(),
                    "light".to_owned(),
                    2,
                    Some(4),
                    some(ENDED_FREED),
                    some("s")
                ),
            ]
        );
    }

    /// Slots full its supervisor's life closed in the book read as the
    /// events read then, at that time and later, and are made by no read;
    /// more of a silent supervisor's life opens it again, and its own end
    /// still ends it.
    #[test]
    fn slots_full_its_life_closed_reads_as_the_events_and_opens_again() {
        let mut events = vec![
            full(1, "slots_full_started", "a", "heavy", 0),
            alive(2, "a", 1),
            full(3, "slots_full_started", "b", "light", 2),
            event(4, None, "supervisor_stopped", json!({"supervisor": "b"}), 3),
        ];
        let (mut book, mut lives) = (
            PreClaimBook::of(&events),
            supervisor_life::Lives::new(&events),
        );
        let read = |book: &PreClaimBook, lives: &supervisor_life::Lives, now: i64| {
            book.intervals(lives, now)
                .map(Cow::into_owned)
                .collect::<Vec<_>>()
        };
        let silent = 100 * MIN;
        book.close_lives(&lives, silent);
        for now in [silent, 2 * silent] {
            assert_eq!(read(&book, &lives, now), pre_claim_intervals(&events, now));
            assert!(
                book.intervals(&lives, now)
                    .all(|wait| matches!(wait, Cow::Borrowed(_)))
            );
        }
        for (more, now) in [
            (alive(5, "a", 150), 151 * MIN),
            (full(6, "slots_full_ended", "a", "heavy", 152), 153 * MIN),
        ] {
            book.add(&more);
            lives.add(&more);
            events.push(more);
            book.close_lives(&lives, now);
            assert_eq!(read(&book, &lives, now), pre_claim_intervals(&events, now));
        }
        assert_eq!(
            book.interval(EventId::new(1), &lives, 153 * MIN)
                .and_then(|(wait, _)| wait.end.clone())
                .as_deref(),
            Some(ENDED_FREED)
        );
    }

    /// A read at a time before the latest close, as a clock set back gives
    /// it, reads the slots full that close ended with its supervisor's
    /// silent life open again until the silence's bound, as the events read
    /// then, whether a close at that time came first or not; one its
    /// supervisor's stop ended stays ended either side of the bound.
    #[test]
    fn slots_full_its_silent_life_closed_reads_open_at_a_time_set_back() {
        use crate::domain::HEARTBEAT_TIMEOUT_SECS;
        let silent_after = (SUPERVISOR_ALIVE_INTERVAL_SECS + HEARTBEAT_TIMEOUT_SECS) * 1000;
        let events = [
            full(1, SLOTS_FULL_STARTED, "a", "heavy", 0),
            alive(2, "a", 1),
            full(3, SLOTS_FULL_STARTED, "b", "light", 2),
            event(4, None, "supervisor_stopped", json!({"supervisor": "b"}), 3),
        ];
        let (mut book, lives) = (
            PreClaimBook::of(&events),
            supervisor_life::Lives::new(&events),
        );
        // The last time a's life goes on.
        let bound = MIN + silent_after;
        assert_eq!(pre_claim_intervals(&events, bound)[0].end, None);
        assert_eq!(
            pre_claim_intervals(&events, bound + 1)[0].end.as_deref(),
            Some(claim_hold::ENDED_SUPERVISOR_GONE)
        );
        for now in [bound + 1, bound, bound + 10, bound + 1, bound, MIN] {
            for close in [false, true] {
                if close {
                    book.close_lives(&lives, now);
                }
                let whole = pre_claim_intervals(&events, now);
                assert_eq!(
                    book.intervals(&lives, now)
                        .map(Cow::into_owned)
                        .collect::<Vec<_>>(),
                    whole,
                    "at {now}, closed then: {close}"
                );
                for wait in &whole {
                    assert_eq!(
                        book.interval(wait.id, &lives, now)
                            .map(|(read, _)| read.into_owned())
                            .as_ref(),
                        Some(wait),
                        "at {now}, closed then: {close}"
                    );
                }
            }
        }
    }

    /// A close reads again the ends of only the supervisors with a slots
    /// full open that an event named since the last, and keeps an end for
    /// only those: one freed or of no slots full has none, and the end of
    /// one closed by it goes.
    #[test]
    fn a_close_reads_only_the_ends_of_the_supervisors_named_since() {
        use crate::domain::HEARTBEAT_TIMEOUT_SECS;
        let silent_after = (SUPERVISOR_ALIVE_INTERVAL_SECS + HEARTBEAT_TIMEOUT_SECS) * 1000;
        let mut events = vec![
            full(1, SLOTS_FULL_STARTED, "a", "heavy", 0),
            full(2, SLOTS_FULL_STARTED, "b", "light", 1),
            full(3, SLOTS_FULL_ENDED, "b", "light", 2),
            alive(4, "c", 3),
        ];
        let (mut book, mut lives) = (
            PreClaimBook::of(&events),
            supervisor_life::Lives::new(&events),
        );
        let ends = |book: &PreClaimBook| {
            book.life_ends
                .iter()
                .map(|(at, supervisor)| (*at, supervisor.clone()))
                .collect::<Vec<_>>()
        };
        book.close_lives(&lives, 4 * MIN);
        assert!(book.named.is_empty());
        assert_eq!(ends(&book), [(silent_after + 1, "a".to_owned())]);
        for more in [alive(5, "c", 5), alive(6, "a", 6)] {
            book.add(&more);
            lives.add(&more);
            events.push(more);
        }
        assert_eq!(book.named, BTreeSet::from(["a".to_owned()]));
        book.close_lives(&lives, 7 * MIN);
        assert_eq!(ends(&book), [(6 * MIN + silent_after + 1, "a".to_owned())]);
        let silent = 6 * MIN + silent_after + 1;
        book.close_lives(&lives, silent);
        assert!(book.life_ends.is_empty() && book.life_end_of.is_empty());
        assert_eq!(
            book.intervals(&lives, silent)
                .map(Cow::into_owned)
                .collect::<Vec<_>>(),
            pre_claim_intervals(&events, silent)
        );
    }

    /// (i) A supervisor that died with its slots full and no stop recorded
    /// ends silent at its last evidence, and at its last heartbeat once the
    /// prune's stop comes; (ii) one that stopped ends at its stop; (iii)
    /// one alive that records nothing else stays open. Another
    /// supervisor's start does not end it.
    #[test]
    fn slots_full_without_its_end_closes_with_its_supervisors_life() {
        use crate::domain::HEARTBEAT_TIMEOUT_SECS;
        let silent_after = (SUPERVISOR_ALIVE_INTERVAL_SECS + HEARTBEAT_TIMEOUT_SECS) * 1000;
        // (i)
        let mut died = vec![
            event(1, None, "supervisor_started", json!({"supervisor": "s"}), 0),
            full(2, SLOTS_FULL_STARTED, "s", "heavy", 1),
            alive(3, "s", 5),
            event(4, None, "supervisor_started", json!({"supervisor": "t"}), 6),
        ];
        let open = rows(&pre_claim_intervals(&died, 5 * MIN + silent_after));
        assert_eq!(open[0].4, None, "{open:?}");
        let silent = rows(&pre_claim_intervals(&died, 5 * MIN + silent_after + 1));
        assert_eq!(
            silent[0],
            (
                IntervalKind::SlotsFull,
                "s".to_owned(),
                "heavy".to_owned(),
                1,
                Some(5),
                some(claim_hold::ENDED_SUPERVISOR_GONE),
                None
            )
        );
        died.push(event(
            5,
            None,
            "supervisor_stopped",
            json!({"supervisor": "s", "outcome": "pruned", "last_heartbeat_at": 7 * 60}),
            20,
        ));
        let pruned = rows(&pre_claim_intervals(&died, 21 * MIN));
        assert_eq!(pruned[0].4, Some(7));
        assert_eq!(pruned[0].6, some("s"));

        // (ii)
        let stopped = [
            full(1, SLOTS_FULL_STARTED, "s", "heavy", 1),
            event(
                2,
                None,
                "supervisor_stopped",
                json!({"supervisor": "s", "outcome": "stopped"}),
                3,
            ),
        ];
        let stopped = rows(&pre_claim_intervals(&stopped, 4 * MIN));
        assert_eq!(stopped[0].4, Some(3));
        assert_eq!(stopped[0].5, some(claim_hold::ENDED_SUPERVISOR_GONE));

        // (iii)
        let mut alive_only = vec![full(1, SLOTS_FULL_STARTED, "s", "heavy", 0)];
        alive_only.extend((1..20).map(|n| alive(n + 1, "s", n * 5)));
        let open = rows(&pre_claim_intervals(&alive_only, 96 * MIN));
        assert_eq!((open[0].4, open[0].5.clone()), (None, None));
    }

    /// A hold belongs to the supervisor that recorded it: another's
    /// `claim_resumed` ends it and is its `ended_by`, the next hold
    /// replaces it, and its own stop ends it.
    #[test]
    fn a_hold_ends_by_any_supervisor_and_keeps_its_own() {
        let held = |id: i64, by: &str, reason: &str, minute: i64| {
            event(
                id,
                None,
                "claim_held",
                json!({"supervisor": by, "reason": reason}),
                minute,
            )
        };
        let events = [
            held(1, "s", "load", 0),
            event(
                2,
                None,
                "claim_resumed",
                json!({"supervisor": "t", "reason": "load"}),
                2,
            ),
            held(3, "s", "load", 3),
            held(4, "t", "disk", 4),
            event(5, None, "supervisor_stopped", json!({"supervisor": "s"}), 5),
            event(6, None, "supervisor_stopped", json!({"supervisor": "t"}), 6),
        ];
        let rows = rows(&pre_claim_intervals(&events, 10 * MIN));
        let hold = |by: &str, reason: &str, start, end, why: &str, ended_by: &str| {
            (
                IntervalKind::ClaimHold,
                by.to_owned(),
                reason.to_owned(),
                start,
                Some(end),
                some(why),
                some(ended_by),
            )
        };
        assert_eq!(
            rows,
            [
                hold("s", "load", 0, 2, claim_hold::ENDED_RESUMED, "t"),
                hold("s", "load", 3, 4, claim_hold::ENDED_REPLACED, "t"),
                hold("t", "disk", 4, 6, claim_hold::ENDED_SUPERVISOR_GONE, "t"),
            ]
        );
    }

    /// A deferral ends at its `claim_deferral_ended` (its `why`), at its
    /// task's `run_claimed` or at its task's next deferral, whichever
    /// supervisor recorded it.
    #[test]
    fn a_deferral_ends_three_ways() {
        let deferred = |id: i64, task: i64, minute: i64| {
            event(
                id,
                Some(task),
                "claim_deferred",
                json!({"supervisor": "s", "reason": "hot_files", "files": ["a"]}),
                minute,
            )
        };
        let events = [
            deferred(1, 10, 0),
            deferred(2, 11, 0),
            deferred(3, 12, 0),
            event(
                4,
                Some(10),
                "claim_deferral_ended",
                json!({"supervisor": "t", "why": "landed"}),
                2,
            ),
            event(5, Some(11), "run_claimed", json!({}), 3),
            deferred(6, 12, 4),
        ];
        let intervals = pre_claim_intervals(&events, 10 * MIN);
        let ends: Vec<(Option<TaskId>, Option<String>, Option<String>)> = intervals
            .iter()
            .map(|row| (row.task_id, row.end.clone(), row.ended_by.clone()))
            .collect();
        assert_eq!(
            ends,
            [
                (Some(TaskId::new(10)), some("landed"), some("t")),
                (
                    Some(TaskId::new(11)),
                    some(claim_defer::ENDED_CLAIMED),
                    None
                ),
                (
                    Some(TaskId::new(12)),
                    some(claim_defer::ENDED_SUPERSEDED),
                    some("s")
                ),
                (Some(TaskId::new(12)), None, None),
            ]
        );
        assert!(
            intervals
                .iter()
                .all(|row| row.kind == IntervalKind::ClaimDeferral)
        );
    }

    /// The rows' ends are the ends `stats` counts: the holds and the
    /// deferrals add up to the same seconds by reason and by end.
    #[test]
    fn the_rows_end_as_the_stats_count() {
        let events = [
            event(
                1,
                None,
                "claim_held",
                json!({"supervisor": "s", "reason": "load"}),
                0,
            ),
            event(
                2,
                Some(7),
                "claim_deferred",
                json!({"supervisor": "s", "files": ["a"]}),
                1,
            ),
            event(
                3,
                None,
                "claim_resumed",
                json!({"supervisor": "t", "reason": "load"}),
                3,
            ),
            event(4, Some(7), "run_claimed", json!({}), 5),
            event(
                5,
                None,
                "claim_held",
                json!({"supervisor": "t", "reason": "disk"}),
                6,
            ),
        ];
        let end_ms = 10 * MIN;
        let holds = claim_holds(&events, EventId::new(0), EventId::new(9), end_ms, |_| true);
        let deferrals =
            claim_deferrals(&events, EventId::new(0), EventId::new(9), end_ms, |_| true);
        let rows = pre_claim_intervals(&events, end_ms);
        let secs = |kind: IntervalKind| -> i64 {
            rows.iter()
                .filter(|row| row.kind == kind)
                .map(|row| {
                    let end = row
                        .ended_at
                        .as_deref()
                        .and_then(timestamp_millis)
                        .unwrap_or(end_ms);
                    (end - timestamp_millis(&row.started_at).unwrap()) / 1000
                })
                .sum()
        };
        assert_eq!((holds.count, holds.secs), (2, 3 * 60 + 4 * 60));
        assert_eq!(secs(IntervalKind::ClaimHold), holds.secs);
        assert_eq!(deferrals.by_end["claimed"].secs, 4 * 60);
        assert_eq!(secs(IntervalKind::ClaimDeferral), deferrals.secs);
        assert_eq!(holds.held.unwrap().reason, "disk");
    }
}
