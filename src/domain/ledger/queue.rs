//! The ledger's queue rows: one per interval or event of the queue as a
//! whole, keyed by the id of the event that starts it. The kinds
//! (`subject.kind`):
//!
//! - `slots_full`, `claim_hold`, `claim_deferral`: the waits before a claim
//!   as [`crate::domain::pre_claim::pre_claim_intervals`] reads them; `landing_hold`: a hold on the
//!   landings by the same rule ([`claim_hold::hold_spans`]). Its start and
//!   its end are its records; an end of no record of its own (the claim
//!   that ends a deferral, the next hold) is read up to all the same
//!   (`inputs_through`), as is a stream's next baseline.
//! - `live_alert_stream`: a supervisor's stream of the record of the
//!   judgments of now from its baseline (or its first record) to its next
//!   baseline: the inputs and their versions and its reaches, folded per
//!   stream; `live_alert`: one alert from its start to its end (an
//!   interval only, never the alert's value).
//! - `finding`: one finding from its first record, with its values now.
//! - `landing_slot`: one `integrate` attempt holding the integration slot
//!   ([`crate::domain::stats::landing_utilization::spans`]); its records
//!   are its run's.
//! - any other kind: one event (main's history, a goal's attributes, an
//!   update's step, a mark, a backend's failure), its kind the row's.
//!
//! An interval with no end yet has no `ended_at` and is not settled
//! ([`super::settle`]). Reads nothing but the events.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{LedgerHead, LedgerKind, LedgerRow, Subject, findings};
use crate::domain::{
    EventId, EventKind, RunEvent, RunId, TaskId, claim_hold,
    live_alerts::{self, AlertKey, StreamRecord},
    pre_claim::{ENDED_FREED, IntervalKind, Moved, PreClaimBook, PreClaimInterval, Started},
    stats::{
        landing_utilization::{Attempt, Attempts},
        timestamp_millis,
    },
    supervisor_life::{Lives, supervisor_of},
};

/// What a queue row the store holds says of itself that [`QueueBook`] is
/// made again by ([`QueueBook::from_heads`]): its head ([`LedgerHead`],
/// the ids of its records and not the records), its task, its subject's
/// kind and id, its times, its outcome and its detail. The store reads
/// it of the rows it holds without their records.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueHead {
    pub head: LedgerHead,
    pub task_id: Option<TaskId>,
    pub subject_kind: String,
    pub subject_id: String,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub outcome: Option<String>,
    pub detail: Value,
}

impl QueueHead {
    /// The queue head of `row`.
    pub fn of(row: &LedgerRow) -> Self {
        let subject = row.subject.as_ref();
        Self {
            head: LedgerHead::of(row),
            task_id: row.task_id,
            subject_kind: subject.map(|s| s.kind.clone()).unwrap_or_default(),
            subject_id: subject.map(|s| s.id.clone()).unwrap_or_default(),
            started_at: row.started_at,
            ended_at: row.ended_at,
            outcome: row.outcome.clone(),
            detail: row.detail.clone(),
        }
    }
}

/// What a queue row of the subject's `kind` is, by the kinds [`QueueBook::row`]
/// gives the rows; another kind is one event of its own.
fn of_kind(kind: &str) -> Of {
    match kind {
        "slots_full" => Of::Interval(Started::PreClaim(IntervalKind::SlotsFull)),
        "claim_hold" => Of::Interval(Started::PreClaim(IntervalKind::ClaimHold)),
        "claim_deferral" => Of::Interval(Started::PreClaim(IntervalKind::ClaimDeferral)),
        "landing_hold" => Of::Interval(Started::LandingHold),
        "live_alert_stream" => Of::Stream,
        "live_alert" => Of::Alert,
        "finding" => Of::Finding,
        _ => Of::Event,
    }
}

/// A queue row as it is folded.
struct Building {
    start: EventId,
    kind: String,
    about: String,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    outcome: Option<String>,
    detail: Value,
    records: Vec<EventId>,
    /// The supervisor whose life's end ended it, when that is its end.
    life: Option<String>,
}

const fn interval_kind(kind: IntervalKind) -> &'static str {
    match kind {
        IntervalKind::SlotsFull => "slots_full",
        IntervalKind::ClaimHold => "claim_hold",
        IntervalKind::ClaimDeferral => "claim_deferral",
    }
}

/// The interval row of a wait before a claim or a hold on the landings.
fn interval_row(
    start: &Assigned,
    id: EventId,
    kind: &str,
    interval: &PreClaimInterval,
) -> Building {
    let about = interval
        .supervisor
        .clone()
        .or_else(|| interval.task_id.map(|task| task.to_string()))
        .unwrap_or_default();
    Building {
        start: id,
        kind: kind.to_owned(),
        about,
        started_at: start.started_at,
        ended_at: interval.ended_at.as_deref().and_then(timestamp_millis),
        outcome: interval.end.clone(),
        detail: json!({"interval": interval}),
        records: start.records.clone(),
        life: (interval.end.as_deref() == Some(claim_hold::ENDED_SUPERVISOR_GONE))
            .then(|| interval.supervisor.clone())
            .flatten(),
    }
}

/// The queue rows of `owned` (the events of the queue as a whole,
/// [`super::owner::Owner::Queue`], ascending id), read against `all` (the
/// whole EventStore: the supervisors' lives end intervals, and the runs'
/// events give the landing slot's use) at `now_ms`, built at `built_at`,
/// as [`QueueBook`] assigns them. Each of `owned` is the records of
/// exactly one row.
pub fn queue_rows(
    owned: &[RunEvent],
    all: &[RunEvent],
    now_ms: i64,
    built_at: &str,
) -> Vec<LedgerRow> {
    let owns: HashSet<EventId> = owned.iter().map(|event| event.id).collect();
    let (mut lives, mut pre_claim, mut book) = (
        Lives::default(),
        PreClaimBook::default(),
        QueueBook::default(),
    );
    let mut touched = BTreeSet::new();
    for event in all {
        lives.add(event);
        let moved = pre_claim.add(event);
        book.add(event, owns.contains(&event.id), moved, &mut touched);
    }
    // Read by id: the order they came in is the fold's.
    let mut by_id = owned.to_vec();
    by_id.sort_by_key(|event| event.id);
    let at = At {
        owned: &by_id,
        pre_claim: &pre_claim,
        lives: &lives,
        now_ms,
        built_at,
    };
    let mut rows: Vec<LedgerRow> = book.keys().filter_map(|key| book.row(&key, &at)).collect();
    rows.sort_by_key(|row| row.inputs_from);
    rows
}

/// What a queue row is folded against: the queue's events (`owned`,
/// ascending id), the waits before a claim and the supervisors' lives of
/// every event read, and the time.
pub struct At<'a> {
    pub owned: &'a [RunEvent],
    pub pre_claim: &'a PreClaimBook,
    pub lives: &'a Lives,
    pub now_ms: i64,
    pub built_at: &'a str,
}

/// What a queue row is, as it is assigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Of {
    /// A wait before a claim or a hold on the landings, as
    /// [`PreClaimBook`] holds it.
    Interval(Started),
    /// A supervisor's stream of the record of the judgments of now.
    Stream,
    /// One alert.
    Alert,
    /// One finding.
    Finding,
    /// One event of its own.
    Event,
}

/// One queue row as it is assigned: what it is of, its records' ids and
/// what its records gave so far.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Assigned {
    of: Of,
    about: String,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    outcome: Option<String>,
    /// A stream's: whether it started at a baseline, and the input
    /// version its records carried last.
    baseline: bool,
    version: Option<u64>,
    records: Vec<EventId>,
    /// The event that ended it when that is not one of its records (a
    /// claim that ends a deferral, the next hold or baseline): an input
    /// of it all the same.
    ended_by: Option<EventId>,
}

impl Assigned {
    fn new(of: Of, start: &RunEvent, about: String) -> Self {
        Self {
            of,
            about,
            started_at: timestamp_millis(&start.created_at),
            ended_at: None,
            outcome: None,
            baseline: false,
            version: None,
            records: vec![start.id],
            ended_by: None,
        }
    }
}

/// One `integrate` attempt's row as it is kept: its `integration_started`
/// and its run. Once it ended it reads up to the greatest id of the events
/// read whose time is at or before its end ([`Throughs`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Slot {
    start: EventId,
    run: String,
}

/// The greatest id each ended attempt reads up to, kept as runs of
/// attempts: an event reaches every attempt ended at or after its time,
/// which are the last of those ended ([`Attempts::ended_since`]), so it
/// sets one run from the first of them on (ids ascend) and drops the runs
/// that started at or after it. Each run is set once and dropped at most
/// once, so an event costs a binary search and its share of the runs, not
/// the attempts it reaches; and the attempts it reaches go into `touched`
/// only where no event since `touched` was last emptied put them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Throughs {
    /// `(first attempt, greatest id)`, the firsts ascending: an attempt
    /// reads up to the id of the last run starting at or before it.
    runs: Vec<(usize, EventId)>,
    /// The attempts put into `touched` since it was last emptied, as far
    /// as this knows: from the first to those ended at the last event.
    touched: Option<std::ops::Range<usize>>,
}

impl Throughs {
    /// Let `id` reach the ended attempts of `reached` (the last of those
    /// ended) and put their slots' keys into `touched`.
    fn reach(
        &mut self,
        reached: std::ops::Range<usize>,
        id: EventId,
        slots: &[Slot],
        touched: &mut BTreeSet<(LedgerKind, String)>,
    ) {
        let (from, ended) = (reached.start, reached.end);
        if from >= ended {
            return;
        }
        while self.runs.last().is_some_and(|run| run.0 >= from) {
            self.runs.pop();
            scanned(1);
        }
        self.runs.push((from, id));
        let key = |index: usize| (LedgerKind::Queue, slots[index].start.to_string());
        // An emptied `touched` holds none of the first: put them all again.
        let held = self
            .touched
            .clone()
            .filter(|held| !held.is_empty() && touched.contains(&key(held.start)));
        let (put, kept) = match held {
            Some(held) => (
                (from..held.start).chain(held.end.max(from)..ended),
                from.min(held.start)..ended,
            ),
            None => ((from..ended).chain(0..0), from..ended),
        };
        for index in put {
            touched.insert(key(index));
            scanned(1);
        }
        self.touched = Some(kept);
    }

    /// The greatest id the ended attempt of `index` reads up to; none
    /// before an event reached it.
    fn of(&self, index: usize) -> Option<EventId> {
        let at = self.runs.partition_point(|run| run.0 <= index);
        at.checked_sub(1).map(|at| self.runs[at].1)
    }
}

/// Count `n` attempts or runs of them looked at, for the tests that bound
/// the work of the landing slot's rows.
#[cfg(test)]
fn scanned(n: usize) {
    SCANNED.with(|count| count.set(count.get() + n));
}

#[cfg(not(test))]
const fn scanned(_: usize) {}

#[cfg(test)]
thread_local! {
    static SCANNED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Which queue row each event of the queue read so far is a record of,
/// assigned one event at a time and kept, so that a reader that holds it
/// reads no event twice and folds only the rows it asks for
/// ([`Self::row`]). An interval's start and the end it records are its
/// records (an end that starts the next one is not); a stream runs from a
/// baseline (or its supervisor's first record) to the next baseline; an
/// alert from its `live_alert_started` to the `live_alert_ended` of its
/// key; a finding holds every record of it; any other event is a row of
/// its own. The landing slot's rows are the attempts of every event read
/// ([`Attempts`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueBook {
    rows: BTreeMap<EventId, Assigned>,
    /// The open stream of each supervisor, by its start.
    streams: HashMap<String, EventId>,
    /// The open alert of each supervisor and key, by its start.
    alerts: BTreeMap<(String, AlertKey), EventId>,
    findings: HashMap<i64, EventId>,
    /// The rows a supervisor's life may end, or whose end its life
    /// reached: its open stream and slots full, and its holds its stop
    /// ended.
    lives_of: HashMap<String, BTreeSet<EventId>>,
    /// How many supervisors' `lives_of` hold each row: a row one holds
    /// is reached by that supervisor's later events ([`Self::closed`]).
    held: HashMap<EventId, u32>,
    /// The supervisors with an open stream an event named since the last
    /// [`Self::close_lives`], and the ends it keeps of the rest: the time
    /// from which each one's life ends ([`Lives::ends_from`]), earliest
    /// first.
    stream_named: BTreeSet<String>,
    stream_ends: BTreeSet<(i64, String)>,
    stream_end_of: HashMap<String, i64>,
    attempts: Attempts,
    slots: Vec<Slot>,
    throughs: Throughs,
    slot_of: HashMap<EventId, usize>,
    /// The last event read.
    last: EventId,
}

impl QueueBook {
    /// The book made again from the queue rows the store holds (`heads`,
    /// [`QueueHead`]), `pre_claim` the waits before a claim of the same
    /// events and `last` the last of them, reading no event: it is the book
    /// of those events, read one at a time ([`Self::add`]), less the rows
    /// the store holds settled that no later event reaches
    /// ([`Self::closed`]), which a builder forgets. Each row assigns the
    /// records its row holds; a stream its next baseline did not replace
    /// is its supervisor's open one, an alert not ended its supervisor's
    /// and key's (from the start its detail holds), and a finding's row
    /// the one of its id. A supervisor's life reaches its open stream, its
    /// slots full not freed and the holds its stop ended (by the waits'
    /// details), and the next close reads the ends of the supervisors with
    /// a stream open. The landing slot's rows are the attempts, each ended
    /// one reading up to its row's `inputs_through`, and the last open.
    /// What a row read past its records (an end of no record of its own, a
    /// reach) stays an input of it, as its row read it. A row of another
    /// kind or a key of no event id is not one of its rows.
    pub fn from_heads(heads: &[QueueHead], pre_claim: &PreClaimBook, last: EventId) -> Self {
        let mut heads: Vec<(EventId, &QueueHead)> = heads
            .iter()
            .filter(|head| head.head.kind == LedgerKind::Queue)
            .filter_map(|head| Some((EventId::new(head.head.key.parse().ok()?), head)))
            .collect();
        heads.sort_by_key(|(id, _)| *id);
        let mut book = Self {
            last,
            ..Self::default()
        };
        let mut attempts = Vec::new();
        let mut open = None;
        let mut settled = Vec::new();
        for (id, head) in heads {
            if head.subject_kind == "landing_slot" {
                book.slot(id, head, &mut attempts, &mut open);
                continue;
            }
            if head.head.is_final {
                settled.push(id);
            }
            let of = of_kind(&head.subject_kind);
            let mut row = Assigned {
                of,
                about: String::new(),
                started_at: head.started_at,
                ended_at: None,
                outcome: None,
                baseline: false,
                version: None,
                records: head.head.records.clone(),
                ended_by: None,
            };
            match of {
                Of::Interval(started) => {
                    row.ended_by = Some(head.head.inputs_through.events);
                    book.hold_interval(id, started, &head.detail["interval"]);
                }
                Of::Stream => {
                    row.about.clone_from(&head.subject_id);
                    row.baseline = head.detail["baseline"].as_bool().unwrap_or(false);
                    row.version = head.detail["version"].as_u64();
                    row.ended_by = Some(head.head.inputs_through.events);
                    if head.outcome.as_deref() == Some("replaced") {
                        row.ended_at = head.ended_at;
                        row.outcome.clone_from(&head.outcome);
                    } else {
                        book.streams.insert(head.subject_id.clone(), id);
                    }
                }
                Of::Alert | Of::Finding | Of::Event => {
                    row.about.clone_from(&head.subject_id);
                    row.ended_at = head.ended_at;
                    row.outcome.clone_from(&head.outcome);
                }
            }
            if of == Of::Alert && row.ended_at.is_none() {
                let alert = &head.detail["alert"];
                if let (Some(supervisor), Ok(key)) = (
                    alert.get("supervisor").and_then(Value::as_str),
                    AlertKey::deserialize(alert),
                ) {
                    book.alerts.insert((supervisor.to_owned(), key), id);
                }
            }
            if of == Of::Finding
                && let Ok(finding) = head.subject_id.parse::<i64>()
            {
                book.findings.entry(finding).or_insert(id);
            }
            book.rows.insert(id, row);
        }
        let streams: Vec<(String, EventId)> = book
            .streams
            .iter()
            .map(|(supervisor, start)| (supervisor.clone(), *start))
            .collect();
        for (supervisor, start) in streams {
            book.hold(&supervisor, start);
            book.stream_named.insert(supervisor);
        }
        book.attempts = Attempts::from_parts(attempts, open);
        for id in settled {
            if book.closed(&id.to_string(), pre_claim) {
                book.rows.remove(&id);
            }
        }
        book
    }

    /// Take the landing slot's row `head` of the attempt `start` began as
    /// the next attempt: an ended one reads up to its row's
    /// `inputs_through`, kept as runs of the attempts reading up to the
    /// same ([`Throughs`]).
    fn slot(
        &mut self,
        start: EventId,
        head: &QueueHead,
        attempts: &mut Vec<Attempt>,
        open: &mut Option<(RunId, usize)>,
    ) {
        let index = self.slots.len();
        let begun = head.started_at.unwrap_or_default();
        let ended = head.ended_at;
        attempts.push(Attempt {
            task_id: head.task_id,
            start: begun,
            end: ended.unwrap_or(begun),
            landed: head.outcome.as_deref() == Some("landed"),
            open: ended.is_none(),
        });
        match ended {
            None => {
                *open = RunId::new(head.subject_id.clone())
                    .ok()
                    .map(|run| (run, index))
            }
            Some(_) => {
                let through = head.head.inputs_through.events;
                if self.throughs.runs.last().map(|run| run.1) != Some(through) {
                    self.throughs.runs.push((index, through));
                }
            }
        }
        self.slot_of.insert(start, index);
        self.slots.push(Slot {
            start,
            run: head.subject_id.clone(),
        });
    }

    /// Note the wait `start` began among those a supervisor's life reaches
    /// as [`Self::interval`] left it, by `interval` (its row's detail): a
    /// slots full its supervisor's, unless its `slots_full_ended` freed
    /// it, and a hold or a deferral a supervisor's stop ended that
    /// supervisor's.
    fn hold_interval(&mut self, start: EventId, started: Started, interval: &Value) {
        let text = |key: &str| interval.get(key).and_then(Value::as_str);
        let supervisor = match started {
            Started::PreClaim(IntervalKind::SlotsFull) => {
                (text("end") != Some(ENDED_FREED)).then(|| text("supervisor"))
            }
            _ => (text("end") == Some(claim_hold::ENDED_SUPERVISOR_GONE)).then(|| text("ended_by")),
        };
        if let Some(supervisor) = supervisor.flatten() {
            self.hold(supervisor, start);
        }
    }

    /// The keys of the rows that hold records, ascending: those a builder
    /// that takes the book made again ([`Self::from_heads`]) gives their
    /// records back from the rows the store holds before it folds them.
    pub fn keys_with_records(&self) -> impl Iterator<Item = String> + '_ {
        self.rows
            .iter()
            .filter(|(_, row)| !row.records.is_empty())
            .map(|(id, _)| id.to_string())
    }

    /// Read `event`, after every one read before: the queue's own
    /// (`owned`) are assigned, `moved` is what it did to the waits before
    /// a claim ([`PreClaimBook::add`]), and every event reads into the
    /// landing slot's attempts. The key of each row whose fold it changes
    /// goes into `touched`.
    pub fn add(
        &mut self,
        event: &RunEvent,
        owned: bool,
        moved: Moved,
        touched: &mut BTreeSet<(LedgerKind, String)>,
    ) {
        self.last = self.last.max(event.id);
        self.attempt(event, touched);
        if let Some(supervisor) = supervisor_of(event) {
            if let Some(rows) = self.lives_of.get(supervisor) {
                touched.extend(rows.iter().map(|row| (LedgerKind::Queue, row.to_string())));
            }
            if self.streams.contains_key(supervisor) && !self.stream_named.contains(supervisor) {
                self.stream_named.insert(supervisor.to_owned());
            }
        }
        // An end is an input of its interval whoever's event it is.
        for start in moved.ended.into_iter().flatten() {
            if let Some(row) = self.rows.get_mut(&start) {
                row.ended_by = Some(event.id);
                touched.insert((LedgerKind::Queue, start.to_string()));
            }
        }
        if !owned {
            return;
        }
        let mut taken = self.interval(event, moved, touched);
        taken |= self.stream(event, touched);
        taken |= self.alert(event, touched);
        taken |= self.finding(event, touched);
        if !taken {
            let about = supervisor_of(event)
                .map(str::to_owned)
                .or_else(|| event.goal_id.map(|goal| goal.to_string()))
                .unwrap_or_default();
            let mut row = Assigned::new(Of::Event, event, about);
            row.ended_at = row.started_at;
            self.rows.insert(event.id, row);
            touched.insert((LedgerKind::Queue, event.id.to_string()));
        }
    }

    fn attempt(&mut self, event: &RunEvent, touched: &mut BTreeSet<(LedgerKind, String)>) {
        let Some(at) = timestamp_millis(&event.created_at) else {
            return;
        };
        if let Some(run) = &event.run_id {
            let stepped = self.attempts.add(event, run, at);
            if stepped.started.is_some() {
                self.slot_of.insert(event.id, self.slots.len());
                self.slots.push(Slot {
                    start: event.id,
                    run: run.as_str().to_owned(),
                });
            }
        }
        // The attempts ended at or after its time read up to it.
        self.throughs.reach(
            self.attempts.ended_since(at),
            event.id,
            &self.slots,
            touched,
        );
        if let Some(open) = self.attempts.open() {
            touched.insert((LedgerKind::Queue, self.slots[open].start.to_string()));
        }
    }

    /// Assign `event` to the interval it starts or the one whose end it
    /// records; whether it did.
    fn interval(
        &mut self,
        event: &RunEvent,
        moved: Moved,
        touched: &mut BTreeSet<(LedgerKind, String)>,
    ) -> bool {
        if let Some(started) = moved.started {
            self.rows.insert(
                event.id,
                Assigned::new(Of::Interval(started), event, String::new()),
            );
            if started == Started::PreClaim(IntervalKind::SlotsFull)
                && let Some(supervisor) = supervisor_of(event)
            {
                self.hold(supervisor, event.id);
            }
            touched.insert((LedgerKind::Queue, event.id.to_string()));
        }
        for start in moved.ended.into_iter().flatten() {
            // An end the slots full of its supervisor recorded: its life
            // no longer ends it.
            if let Some(supervisor) = supervisor_of(event)
                && event.kind == EventKind::SlotsFullEnded
            {
                self.release(supervisor, start);
            }
            // A hold its supervisor's stop ended reads its later life.
            if event.kind == EventKind::SupervisorStopped
                && let Some(supervisor) = supervisor_of(event)
            {
                self.hold(supervisor, start);
            }
            touched.insert((LedgerKind::Queue, start.to_string()));
        }
        if moved.started.is_some() {
            return true;
        }
        // An end is its interval's unless it starts one itself (a hold the
        // next one replaced).
        let Some(start) = moved
            .ended
            .into_iter()
            .flatten()
            .find(|start| self.rows.contains_key(start))
        else {
            return false;
        };
        if let Some(row) = self.rows.get_mut(&start) {
            row.records.push(event.id);
        }
        true
    }

    /// Assign a record of a stream of the judgments of now: per
    /// supervisor, from a baseline (or its first record) to its next
    /// baseline, ended there (`replaced`) or by the end of its
    /// supervisor's life; whether it is one.
    fn stream(&mut self, event: &RunEvent, touched: &mut BTreeSet<(LedgerKind, String)>) -> bool {
        let kind = &event.kind;
        if !kind.is_in(&live_alerts::RECORD_KINDS)
            || kind.is_in(&[EventKind::LiveAlertStarted, EventKind::LiveAlertEnded])
        {
            return false;
        }
        let Some(record) = StreamRecord::of(event) else {
            return false;
        };
        let supervisor = record.supervisor;
        let current = self.streams.get(&supervisor).copied();
        let at = match current {
            Some(at) if *kind != EventKind::LiveAlertBaseline => at,
            _ => {
                if let Some(at) = current
                    && let Some(row) = self.rows.get_mut(&at)
                {
                    row.ended_at = timestamp_millis(&event.created_at);
                    row.outcome = Some("replaced".into());
                    row.ended_by = Some(event.id);
                    touched.insert((LedgerKind::Queue, at.to_string()));
                    self.release(&supervisor, at);
                }
                let mut row = Assigned::new(Of::Stream, event, supervisor.clone());
                row.records.clear();
                row.baseline = *kind == EventKind::LiveAlertBaseline;
                self.rows.insert(event.id, row);
                self.hold(&supervisor, event.id);
                self.stream_named.insert(supervisor.clone());
                self.streams.insert(supervisor, event.id);
                event.id
            }
        };
        if let Some(row) = self.rows.get_mut(&at) {
            if record.version.is_some() {
                row.version = record.version;
            }
            row.records.push(event.id);
        }
        touched.insert((LedgerKind::Queue, at.to_string()));
        true
    }

    /// Assign an alert's start or end: one alert from its
    /// `live_alert_started` to the `live_alert_ended` of the same key; an
    /// end with no start is not one.
    fn alert(&mut self, event: &RunEvent, touched: &mut BTreeSet<(LedgerKind, String)>) -> bool {
        let Some(StreamRecord {
            supervisor,
            alert: Some(alert),
            ..
        }) = StreamRecord::of(event)
        else {
            return false;
        };
        let about = alert.run_id.clone().unwrap_or_else(|| alert.kind.clone());
        let key = (supervisor, alert);
        match event.kind.known() {
            Some(EventKind::LiveAlertStarted) => {
                self.rows
                    .insert(event.id, Assigned::new(Of::Alert, event, about));
                self.alerts.insert(key, event.id);
                touched.insert((LedgerKind::Queue, event.id.to_string()));
                true
            }
            Some(EventKind::LiveAlertEnded) => {
                let Some(at) = self.alerts.remove(&key) else {
                    return false;
                };
                if let Some(row) = self.rows.get_mut(&at) {
                    row.ended_at = timestamp_millis(&event.created_at);
                    row.outcome = Some("ended".into());
                    row.records.push(event.id);
                }
                touched.insert((LedgerKind::Queue, at.to_string()));
                true
            }
            _ => false,
        }
    }

    /// Assign a record of a finding: one finding from its first record,
    /// every record of its values; whether it is one.
    fn finding(&mut self, event: &RunEvent, touched: &mut BTreeSet<(LedgerKind, String)>) -> bool {
        if !event.kind.is_in(findings::KINDS) || event.payload.get(findings::VALUES).is_none() {
            return false;
        }
        let Some(finding) = findings::values_of(event) else {
            return false;
        };
        let id = finding.id.as_i64();
        let at = *self.findings.entry(id).or_insert(event.id);
        let row = self.rows.entry(at).or_insert_with(|| {
            let mut row = Assigned::new(Of::Finding, event, id.to_string());
            row.records.clear();
            row
        });
        row.records.push(event.id);
        row.outcome = Some(finding.status.as_str().to_owned());
        row.ended_at = (!finding.status.is_unsettled())
            .then(|| timestamp_millis(&event.created_at))
            .flatten();
        touched.insert((LedgerKind::Queue, at.to_string()));
        true
    }

    /// Note the row of `row` among those `supervisor`'s life reaches.
    fn hold(&mut self, supervisor: &str, row: EventId) {
        if self
            .lives_of
            .entry(supervisor.to_owned())
            .or_default()
            .insert(row)
        {
            *self.held.entry(row).or_default() += 1;
        }
    }

    /// Note the row of `row` no longer among those `supervisor`'s life
    /// reaches.
    fn release(&mut self, supervisor: &str, row: EventId) {
        if let Some(rows) = self.lives_of.get_mut(supervisor)
            && rows.remove(&row)
            && let Some(count) = self.held.get_mut(&row)
        {
            *count -= 1;
            if *count == 0 {
                self.held.remove(&row);
            }
        }
    }

    /// Whether no event after those read can reach the row of `key`, so
    /// that once the store holds it settled what assigns it may be
    /// forgotten ([`Self::forget`]) rather than kept to be given its
    /// records back: no supervisor's life reaches it, and it is one event
    /// of its own, a stream its next baseline replaced, an alert ended, or
    /// a wait or hold an event of its own ended (`pre_claim`, of the same
    /// events: one a supervisor's life ended may open again). An open
    /// interval, stream or alert, a finding (any later record of it is
    /// its), a landing slot's (an event timed before its end reaches it)
    /// and a key of no row are not.
    pub fn closed(&self, key: &str, pre_claim: &PreClaimBook) -> bool {
        let Ok(id) = key.parse().map(EventId::new) else {
            return false;
        };
        let Some(row) = self.rows.get(&id) else {
            return false;
        };
        if self.held.contains_key(&id) || self.slot_of.contains_key(&id) {
            return false;
        }
        match row.of {
            Of::Event => true,
            Of::Stream => self.streams.get(&row.about) != Some(&id),
            Of::Alert => row.ended_at.is_some(),
            Of::Finding => false,
            Of::Interval(_) => pre_claim.ended_by_event(id),
        }
    }

    /// Forget the row of `key`, [`Self::closed`]: no later read folds it.
    pub fn forget(&mut self, key: &str) {
        if let Ok(id) = key.parse().map(EventId::new) {
            self.rows.remove(&id);
        }
    }

    /// The starts of the open streams whose supervisor's life ended by
    /// `now_ms` (`lives`, of the same events) with no event of it since the
    /// last close that found it ended, ascending: a silence ends a stream
    /// with no event, so its row is folded again though nothing touched
    /// it. It reads the ends of the supervisors named since the last close
    /// and keeps those not come yet; an event naming the supervisor later
    /// (one that opens the stream again) has it read again.
    pub fn close_lives(&mut self, lives: &Lives, now_ms: i64) -> Vec<EventId> {
        for supervisor in std::mem::take(&mut self.stream_named) {
            if let Some(at) = self.stream_end_of.remove(&supervisor) {
                self.stream_ends.remove(&(at, supervisor.clone()));
            }
            if let Some(at) = lives.ends_from(&supervisor) {
                self.stream_end_of.insert(supervisor.clone(), at);
                self.stream_ends.insert((at, supervisor));
            }
        }
        let mut closed = Vec::new();
        while self
            .stream_ends
            .first()
            .is_some_and(|(at, _)| *at <= now_ms)
        {
            let Some((_, supervisor)) = self.stream_ends.pop_first() else {
                break;
            };
            self.stream_end_of.remove(&supervisor);
            if lives.end(&supervisor, now_ms).is_some()
                && let Some(&start) = self.streams.get(&supervisor)
            {
                closed.push(start);
            }
        }
        closed.sort_unstable();
        closed
    }

    /// How many rows it assigns: the queue's events' and the landing
    /// slot's.
    pub fn row_count(&self) -> usize {
        self.rows.len() + self.slots.len()
    }

    /// The ids of the records of the row of `key`, ascending; none for a
    /// key of no row, and none of a landing slot's, which owns none.
    pub fn records_of(&self, key: &str) -> Option<&[EventId]> {
        let id = EventId::new(key.parse().ok()?);
        self.rows.get(&id).map(|row| row.records.as_slice())
    }

    /// Roughly how many bytes it holds: each row with what it is about and
    /// its records' ids, the landing slot's attempts, and what it finds
    /// the rows by.
    pub fn approx_bytes(&self) -> usize {
        let rows: usize = self
            .rows
            .values()
            .map(|row| {
                size_of::<(EventId, Assigned)>()
                    + row.about.len()
                    + row.outcome.as_ref().map_or(0, String::len)
                    + row.records.len() * size_of::<EventId>()
            })
            .sum();
        let slots: usize = self
            .slots
            .iter()
            .map(|slot| 2 * size_of::<Slot>() + slot.run.len())
            .sum();
        let finders = self.streams.len()
            + self.stream_named.len()
            + 2 * self.stream_ends.len()
            + self.alerts.len()
            + self.findings.len()
            + self.lives_of.values().map(BTreeSet::len).sum::<usize>()
            + self.held.len()
            + self.slot_of.len()
            + self.throughs.runs.len();
        rows + slots + finders * size_of::<(String, EventId)>()
    }

    /// The keys of every row: the queue's events', then the landing
    /// slot's.
    pub fn keys(&self) -> impl Iterator<Item = String> + '_ {
        self.rows
            .keys()
            .chain(self.slots.iter().map(|slot| &slot.start))
            .map(EventId::to_string)
    }

    /// The row of `key` folded against `at`; none for a key of no row.
    pub fn row(&self, key: &str, at: &At<'_>) -> Option<LedgerRow> {
        let id = EventId::new(key.parse().ok()?);
        if let Some(&slot) = self.slot_of.get(&id) {
            return self.landing_slot(slot, at);
        }
        let assigned = self.rows.get(&id)?;
        // A slots full reads up to the last event naming its supervisor
        // ([`PreClaimInterval::reach`]), and so does the open stream of a
        // supervisor: one that opens it again after its supervisor's
        // silence ended it is an input past that end.
        let mut reach = (assigned.of == Of::Stream
            && self.streams.get(&assigned.about) == Some(&id))
        .then(|| at.lives.last_of(&assigned.about))
        .flatten();
        let mut row = match assigned.of {
            Of::Interval(Started::PreClaim(kind)) => {
                let (interval, _) = at.pre_claim.interval(id, at.lives, at.now_ms)?;
                reach = Some(interval.reach);
                interval_row(assigned, id, interval_kind(kind), &interval)
            }
            Of::Interval(Started::LandingHold) => {
                let (interval, _) = at.pre_claim.landing_hold(id)?;
                interval_row(assigned, id, "landing_hold", interval)
            }
            of => {
                let mut row = Building {
                    start: id,
                    kind: String::new(),
                    about: assigned.about.clone(),
                    started_at: assigned.started_at,
                    ended_at: assigned.ended_at,
                    outcome: assigned.outcome.clone(),
                    detail: json!({}),
                    records: assigned.records.clone(),
                    life: None,
                };
                self.fill(&mut row, of, at);
                row
            }
        };
        if row.kind.is_empty() {
            row.kind = event(at.owned, id)?.kind.to_string();
        }
        let life = row.life.as_deref().and_then(|s| at.lives.last_of(s));
        let mut ledger = finish(row, at.owned, at.built_at);
        for input in [assigned.ended_by, reach].into_iter().flatten() {
            ledger.inputs_through.events = ledger.inputs_through.events.max(input);
        }
        ledger.inputs_through.supervisor = life;
        Some(ledger)
    }

    /// What a stream, an alert or a finding is besides its records: its
    /// kind and detail, and a stream's end by its supervisor's life.
    fn fill(&self, row: &mut Building, of: Of, at: &At<'_>) {
        let Some(assigned) = self.rows.get(&row.start) else {
            return;
        };
        match of {
            Of::Stream => {
                row.kind = "live_alert_stream".into();
                row.detail = json!({"baseline": assigned.baseline});
                if let Some(version) = assigned.version {
                    row.detail["version"] = json!(version);
                }
                if self.streams.get(&row.about) == Some(&row.start)
                    && let Some(end) = at.lives.end(&row.about, at.now_ms)
                {
                    let started = row.started_at.unwrap_or(end.at_ms());
                    row.ended_at = Some(end.at_ms().max(started));
                    row.outcome = Some(claim_hold::ENDED_SUPERVISOR_GONE.into());
                    row.life = Some(row.about.clone());
                }
            }
            Of::Alert => {
                row.kind = "live_alert".into();
                if let Some(start) = event(at.owned, row.start) {
                    row.detail = json!({"alert": start.payload});
                }
            }
            Of::Finding => {
                row.kind = "finding".into();
                if let Some(values) = row
                    .records
                    .last()
                    .and_then(|last| event(at.owned, *last))
                    .and_then(|last| last.payload.get(findings::VALUES))
                {
                    row.detail = json!({"finding": values});
                }
            }
            Of::Interval(_) | Of::Event => {}
        }
    }

    /// The row of the attempt of `index`: its records are its run's.
    fn landing_slot(&self, index: usize, at: &At<'_>) -> Option<LedgerRow> {
        let slot = self.slots.get(index)?;
        let attempt = self.attempts.get(index, at.now_ms)?;
        let mut row = LedgerRow::new(
            LedgerKind::Queue,
            slot.start.to_string(),
            attempt.task_id,
            at.built_at,
        );
        row.inputs_from = slot.start;
        row.inputs_through.events = if attempt.open {
            self.last
        } else {
            self.throughs
                .of(index)
                .map_or(slot.start, |through| through.max(slot.start))
        };
        row.started_at = Some(attempt.start);
        row.ended_at = (!attempt.open).then_some(attempt.end);
        row.outcome =
            (!attempt.open).then(|| if attempt.landed { "landed" } else { "left" }.to_owned());
        row.subject = Some(Subject {
            kind: "landing_slot".into(),
            id: slot.run.clone(),
            ..Subject::default()
        });
        Some(row)
    }

    /// The key of the landing slot's row open now: its inputs reach every
    /// event read.
    pub fn open_slot(&self) -> Option<String> {
        self.attempts
            .open()
            .and_then(|index| self.slots.get(index))
            .map(|slot| slot.start.to_string())
    }
}

/// The event of `id` among `events` (ascending id).
fn event(events: &[RunEvent], id: EventId) -> Option<&RunEvent> {
    events
        .binary_search_by_key(&id, |event| event.id)
        .ok()
        .map(|at| &events[at])
}

fn finish(row: Building, owned: &[RunEvent], built_at: &str) -> LedgerRow {
    let mut records: Vec<RunEvent> = row
        .records
        .iter()
        .filter_map(|id| event(owned, *id).cloned())
        .collect();
    records.sort_by_key(|event| event.id);
    let mut ledger = LedgerRow::new(
        LedgerKind::Queue,
        row.start.to_string(),
        records.iter().find_map(|event| event.task_id),
        built_at,
    );
    ledger.started_at = row.started_at;
    ledger.ended_at = row.ended_at;
    ledger.outcome = row.outcome;
    ledger.detail = row.detail;
    ledger.subject = Some(Subject {
        kind: row.kind,
        id: row.about,
        ..Subject::default()
    });
    ledger.own(records);
    ledger
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod cut_spike;
