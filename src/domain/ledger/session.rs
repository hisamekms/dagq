//! The ledger's session rows: one per session or job of no run (a span
//! `session_opened` / `session_closed` recorded, of the runtime's planner,
//! the inbox, a person's planner or a job; a plan review, a goal review,
//! a throughput review or the observer from its start to its end), keyed
//! by the kind and id of what it is of and which of its sessions or jobs
//! it is. A row keeps its time, its route, provider and model, its result,
//! what its kind records (`detail.items`) and its Executions and an
//! interactive session's cuts with their tokens and context
//! (`detail.executions`, [`Execution::entry`]). [`run_sessions`] gives a
//! run's row the same of the sessions and jobs on it.
//!
//! The tokens come from the records of the Executions and the cuts alone
//! (ADR-t1486-1), never from the steps. Reads nothing but the events.

use std::collections::{BTreeSet, HashMap};

use serde_json::{Map, Value, json};

use super::{LEDGER_V, LedgerHead, LedgerKind, LedgerRow, Subject};
use crate::domain::{
    EventId, EventKind, RunEvent,
    headless_job::JobRef,
    sessions::{
        GOAL_REVIEW, HOOK_KINDS, INFERRED, OBSERVER, PLAN_REVIEW, SpanRef, THROUGHPUT_REVIEW,
    },
    stats::{
        executions::{Execution, executions},
        jobs::{DEFAULT_PROVIDER, JobLife, job_lives, job_lives_with, job_of, starts_job},
        sessions::{Span, spans},
        timestamp_millis,
    },
};

/// What a row says of a span: its open and close, kind, route and model,
/// whether its close was inferred and its active time.
fn span_items(span: &Span, provider: Option<&str>) -> Value {
    json!({
        "what": "session",
        "opened": span.opened,
        "closed": span.closed,
        "kind": span.kind,
        "route": span.route,
        "provider": provider,
        "model": span.model,
        "started_at": span.start,
        "ended_at": span.end,
        "inferred": span.inferred,
        "active_secs": span.active,
        "planner_id": span.planner_id,
        "goal_ids": span.goal_ids,
        "tokens": span.tokens,
    })
}

/// What a row says of a job.
fn job_items(job: &JobLife) -> Value {
    let mut items = json!(job);
    items["what"] = json!("job");
    items
}

/// The result of a span: how it closed (`closed`, `inferred`), none while
/// it is open.
fn span_result(span: &Span) -> Option<String> {
    span.closed?;
    Some(if span.inferred { INFERRED } else { "closed" }.to_owned())
}

/// The result of a job: `failed`, its verdict, or `finished`; none while
/// it runs.
fn job_result(job: &JobLife) -> Option<String> {
    job.ended?;
    Some(if job.failed {
        "failed".to_owned()
    } else {
        job.verdict.clone().unwrap_or_else(|| "finished".to_owned())
    })
}

/// The provider a span ran on: the one its open's `launch` names, else
/// Claude Code for a kind the plugin's hook records (only Claude Code has
/// the hook); none otherwise.
fn span_provider(open: Option<&RunEvent>, span: &Span) -> Option<String> {
    open.and_then(|open| SpanRef::of(open).launch_provider)
        .or_else(|| {
            HOOK_KINDS
                .contains(&span.kind.as_str())
                .then(|| DEFAULT_PROVIDER.to_owned())
        })
}

fn entries(executions: &[Execution]) -> Value {
    Value::Array(executions.iter().map(Execution::entry).collect())
}

/// What a run's row keeps of the sessions and jobs on it (worker, resume,
/// revise, review, recovery), from the run's own `events` (ascending id):
/// `sessions` (each span as a session row says it), `jobs` (each job) and
/// `executions` (its Executions, [`Execution::entry`]).
pub fn run_sessions(events: &[RunEvent]) -> Map<String, Value> {
    let opens: HashMap<EventId, &RunEvent> = events
        .iter()
        .filter(|event| event.kind == EventKind::SessionOpened)
        .map(|event| (event.id, event))
        .collect();
    let mut kept = Map::new();
    kept.insert(
        "sessions".into(),
        spans(events)
            .iter()
            .map(|span| {
                let mut items = span_items(
                    span,
                    span_provider(opens.get(&span.opened).copied(), span).as_deref(),
                );
                items["result"] = json!(span_result(span));
                items
            })
            .collect(),
    );
    kept.insert(
        "jobs".into(),
        job_lives(events)
            .iter()
            .map(|job| {
                let mut items = job_items(job);
                items["result"] = json!(job_result(job));
                items
            })
            .collect(),
    );
    kept.insert("executions".into(), entries(&executions(events)));
    kept
}

/// The Executions a run's or a session's row keeps, oldest first; none
/// for another kind.
pub fn row_executions(row: &LedgerRow) -> Vec<Execution> {
    row.detail["executions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Execution::from_entry)
        .collect()
}

/// One session row as it is folded.
struct Building {
    subject: Subject,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    outcome: Option<String>,
    items: Value,
    records: Vec<RunEvent>,
}

impl Building {
    fn new(kind: &str, id: &str, round: i64) -> Self {
        Self {
            subject: Subject {
                kind: kind.to_owned(),
                id: id.to_owned(),
                round: Some(round),
                ..Subject::default()
            },
            started_at: None,
            ended_at: None,
            outcome: None,
            items: json!({"what": "events"}),
            records: Vec::new(),
        }
    }

    fn key(&self) -> String {
        let what = self.items["what"].as_str().unwrap_or("events");
        format!(
            "{}/{}/{what}/{}",
            self.subject.kind,
            self.subject.id,
            self.subject.round.unwrap_or(0)
        )
    }
}

/// The id a span is of: its planner's, its plan review's or goal review's,
/// else none (the inbox, a person's planner).
fn span_target(open: &RunEvent) -> String {
    let job = JobRef::of(&open.payload);
    SpanRef::of(open)
        .planner_id
        .map(|id| id.to_string())
        .or(job.plan_review_id)
        .or(job.goal_review_id)
        .unwrap_or_default()
}

/// The kinds of the observer's and the throughput reviews' events.
const OBSERVER_KINDS: [EventKind; 2] = [EventKind::ObserveStarted, EventKind::ObserveFinished];
const THROUGHPUT_REVIEW_KINDS: [EventKind; 3] = [
    EventKind::ThroughputReviewStarted,
    EventKind::ThroughputReviewFinished,
    EventKind::ThroughputReviewReported,
];

/// The session rows of `events`: the session's and job's events of no run
/// ([`super::owner::Owner::Session`]), ascending id, built at
/// `built_at`, as [`SessionBook`] assigns them.
pub fn session_rows(events: &[RunEvent], built_at: &str) -> Vec<LedgerRow> {
    let mut book = SessionBook::default();
    for event in events {
        book.add(event, &mut BTreeSet::new());
    }
    // Read by id: the order they came in is the fold's.
    let mut by_id = events.to_vec();
    by_id.sort_by_key(|event| event.id);
    book.keys()
        .filter_map(|key| book.row(key, &by_id, built_at))
        .collect()
}

/// The records of the session rows of `rows` a [`SessionBook`] is made
/// again by ([`SessionBook::from_heads`]), each with its row's key: each
/// row's first and every `session_closed`. The store reads the same of
/// the rows it holds without the rest of their records.
pub fn anchors(rows: &[LedgerRow]) -> Vec<(String, RunEvent)> {
    rows.iter()
        .filter(|row| row.kind == LedgerKind::Session)
        .flat_map(|row| {
            row.records
                .iter()
                .enumerate()
                .filter(|(at, event)| *at == 0 || event.kind == EventKind::SessionClosed)
                .map(|(_, event)| (row.key.clone(), event.clone()))
        })
        .collect()
}

/// The session rows the store holds as they were at the event `through`:
/// each head's records up to it (a row with none is not one yet) and the
/// anchors up to it, so that the book made of them
/// ([`SessionBook::from_heads`]) is the book of the events up to
/// `through`. A row the store holds settled by this [`LEDGER_V`] with no
/// record after `through` is `settled` (what builds the ledger keeps only
/// its records' ids and gives its records back from the store's row when
/// a later event reaches it); every other row is `open`, its records up
/// to `through` kept: one not settled then, and one settled by an older
/// version, which is folded again as a row of an older version is. It is the fold of every event only when the store's
/// session rows record every session event up to `through`: a row the
/// store has not written yet, or a record it has not written to a row,
/// is not in the heads, and a builder going on from `through` misses it.
/// Which `through` the store has written up to it does not find.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionsHeld {
    pub through: EventId,
    pub book: SessionBook,
    /// The keys of the rows settled at `through`.
    pub settled: Vec<String>,
    /// The keys of the rows not settled at `through` or folded by an
    /// older version.
    pub open: Vec<String>,
}

impl SessionsHeld {
    /// The session rows of `heads` and their `anchors`
    /// (`LedgerStore::ledger_session_anchors`) as they were at `through`.
    pub fn at(heads: &[LedgerHead], anchors: &[(String, RunEvent)], through: EventId) -> Self {
        let mut held = Self {
            through,
            ..Self::default()
        };
        let mut cut = Vec::new();
        for head in heads
            .iter()
            .filter(|head| head.kind == LedgerKind::Session && parse_key(&head.key).is_some())
        {
            let records: Vec<EventId> = head
                .records
                .iter()
                .copied()
                .filter(|id| *id <= through)
                .collect();
            if records.is_empty() {
                continue;
            }
            let settled =
                head.is_final && head.ledger_v == LEDGER_V && records.len() == head.records.len();
            if settled {
                held.settled.push(head.key.clone());
            } else {
                held.open.push(head.key.clone());
            }
            cut.push(LedgerHead {
                records,
                is_final: settled,
                ..head.clone()
            });
        }
        let anchors: Vec<(String, RunEvent)> = anchors
            .iter()
            .filter(|(_, event)| event.id <= through)
            .cloned()
            .collect();
        held.book = SessionBook::from_heads(&cut, &anchors);
        held
    }

    /// The ids of the records of the rows not settled, those what builds
    /// the ledger keeps.
    pub fn open_records(&self) -> BTreeSet<EventId> {
        self.open
            .iter()
            .filter_map(|key| self.book.records_of(key))
            .flatten()
            .copied()
            .collect()
    }
}

/// Which session row each event read so far is a record of, assigned one
/// event at a time and kept, so that a reader that holds it reads no
/// event twice and folds only the rows it asks for ([`Self::row`]). A span
/// is a row from its open; its turns, cuts and close go to it by
/// `opened_event_id`. A job is a row from its start to its end, a later
/// event of its kind and key to its last row. A runtime planner's event
/// goes to its span open last, an event of none of these to the row of
/// round 0 of what it is of. Rounds count from 1 per kind and id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionBook {
    /// In the order they were made.
    rows: Vec<Assigned>,
    by_key: HashMap<String, usize>,
    rounds: HashMap<(String, String, &'static str), i64>,
    by_open: HashMap<EventId, usize>,
    by_planner: HashMap<i64, usize>,
    by_job: HashMap<(String, String), usize>,
    outside: HashMap<(String, String), usize>,
    /// The sessions' models by their closes
    /// ([`crate::domain::stats::jobs::session_models`]).
    models: HashMap<String, (String, EventId)>,
    /// The job rows by the session whose close gives their model.
    jobs_by_session: HashMap<String, Vec<usize>>,
}

/// One session row as it is assigned: what it is of and its records' ids.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Assigned {
    key: String,
    kind: String,
    id: String,
    round: i64,
    what: &'static str,
    records: Vec<EventId>,
}

/// What a session row's key says it is of: its subject's kind and id,
/// what it is (`session`, `job` or `events`) and its round; none for a key
/// of another shape. The id is what lies between the kind and the last two
/// parts (a throughput review's has slashes of its own).
fn parse_key(key: &str) -> Option<(&str, &str, &'static str, i64)> {
    let (kind, rest) = key.split_once('/')?;
    let mut back = rest.rsplitn(3, '/');
    let round = back.next()?.parse().ok()?;
    let what = match back.next()? {
        "session" => "session",
        "job" => "job",
        "events" => "events",
        _ => return None,
    };
    Some((kind, back.next()?, what, round))
}

impl SessionBook {
    /// The book made again from the session rows the store holds (`heads`,
    /// [`LedgerHead`]) and `anchors`, the records of theirs it finds its
    /// rows by, each with its row's key: each row's first (a span's open,
    /// a job's start) and every `session_closed`
    /// (`LedgerStore::ledger_session_anchors`). It is the book of every
    /// event those rows record, read one at a time ([`Self::add`]): the
    /// rows in the order they were made (by their first record), the
    /// rounds of each kind and id so far, the opens, planners and jobs
    /// their later events go to (the latest of each), the rows of round 0,
    /// the sessions' models by their latest close, and the jobs by the
    /// session their start names. It reads no event of the EventStore, so
    /// a builder that goes on from the store's rows gives a new session
    /// its next round and a late record its row as the fold of every event
    /// does. A row of another kind or a key of another shape is not one of
    /// its rows.
    pub fn from_heads(heads: &[LedgerHead], anchors: &[(String, RunEvent)]) -> Self {
        let mut heads: Vec<&LedgerHead> = heads
            .iter()
            .filter(|head| head.kind == LedgerKind::Session && parse_key(&head.key).is_some())
            .collect();
        heads.sort_by_key(|head| head.records.first().copied());
        let anchor: HashMap<EventId, &RunEvent> =
            anchors.iter().map(|(_, event)| (event.id, event)).collect();
        let mut book = Self::default();
        for head in heads {
            let Some((kind, id, what, round)) = parse_key(&head.key) else {
                continue;
            };
            let at = book.rows.len();
            book.by_key.insert(head.key.clone(), at);
            book.rows.push(Assigned {
                key: head.key.clone(),
                kind: kind.to_owned(),
                id: id.to_owned(),
                round,
                what,
                records: head.records.clone(),
            });
            let first = head.records.first().copied();
            let opened = first
                .and_then(|first| anchor.get(&first))
                .map(|open| SpanRef::of(open));
            match what {
                "events" => {
                    book.outside.insert((kind.to_owned(), id.to_owned()), at);
                    continue;
                }
                "session" => {
                    if let Some(first) = first {
                        book.by_open.insert(first, at);
                    }
                    if let Some(planner) = opened.as_ref().and_then(|open| open.planner_id) {
                        book.by_planner.insert(planner, at);
                    }
                }
                _ => {
                    book.by_job.insert((kind.to_owned(), id.to_owned()), at);
                    if let Some(session) = opened.and_then(|start| start.session_id) {
                        book.jobs_by_session.entry(session).or_default().push(at);
                    }
                }
            }
            let rounds = book
                .rounds
                .entry((kind.to_owned(), id.to_owned(), what))
                .or_default();
            *rounds = (*rounds).max(round);
        }
        let mut closes: Vec<&RunEvent> = anchors
            .iter()
            .map(|(_, event)| event)
            .filter(|event| event.kind == EventKind::SessionClosed)
            .collect();
        closes.sort_by_key(|close| close.id);
        for close in closes {
            let span = SpanRef::of(close);
            if let (Some(session), Some(model)) = (span.session_id, span.model) {
                book.models.insert(session, (model, close.id));
            }
        }
        book
    }

    /// Assign `event`, after every one assigned before, and add to
    /// `touched` the key of each row whose fold it changes: the one it is
    /// a record of, and a job's whose model its close gives.
    pub fn add(&mut self, event: &RunEvent, touched: &mut BTreeSet<(LedgerKind, String)>) {
        let span = SpanRef::of(event);
        let at = if event.kind == EventKind::SessionOpened {
            let kind = span.kind.as_deref().unwrap_or_default();
            let id = span_target(event);
            let at = self.open_row(kind, &id, "session");
            self.by_open.insert(event.id, at);
            if let Some(planner) = span.planner_id {
                self.by_planner.insert(planner, at);
            }
            at
        } else if let Some(at) = span
            .opened_event_id
            .and_then(|opened| self.by_open.get(&opened))
        {
            *at
        } else if starts_job(event)
            && let Some((kind, key)) = job_of(event)
        {
            let at = self.open_row(kind, &key, "job");
            self.by_job.insert((kind.to_owned(), key), at);
            if let Some(session) = span.session_id.clone() {
                self.jobs_by_session.entry(session).or_default().push(at);
            }
            at
        } else if let Some(at) =
            job_of(event).and_then(|(kind, key)| self.by_job.get(&(kind.to_owned(), key)))
        {
            *at
        } else if let Some(at) = span
            .planner_id
            .and_then(|planner| self.by_planner.get(&planner))
        {
            *at
        } else {
            let (kind, id) = match (span.planner_id, job_of(event)) {
                (Some(planner), _) => ("runtime_planner".to_owned(), planner.to_string()),
                (None, Some((kind, key))) => (kind.to_owned(), key),
                (None, None) => outside_of(event),
            };
            match self.outside.get(&(kind.clone(), id.clone())) {
                Some(at) => *at,
                None => {
                    let at = self.push(&kind, &id, 0, "events");
                    self.outside.insert((kind, id), at);
                    at
                }
            }
        };
        self.rows[at].records.push(event.id);
        touched.insert((LedgerKind::Session, self.rows[at].key.clone()));
        if event.kind == EventKind::SessionClosed
            && let (Some(session), Some(model)) = (span.session_id, span.model)
        {
            for job in self.jobs_by_session.get(&session).into_iter().flatten() {
                touched.insert((LedgerKind::Session, self.rows[*job].key.clone()));
            }
            self.models.insert(session, (model, event.id));
        }
    }

    /// A new row of a span or a job of `kind` and `id`, its next round.
    fn open_row(&mut self, kind: &str, id: &str, what: &'static str) -> usize {
        let round = self
            .rounds
            .entry((kind.to_owned(), id.to_owned(), what))
            .or_default();
        *round += 1;
        let round = *round;
        self.push(kind, id, round, what)
    }

    fn push(&mut self, kind: &str, id: &str, round: i64, what: &'static str) -> usize {
        let key = format!("{kind}/{id}/{what}/{round}");
        self.by_key.insert(key.clone(), self.rows.len());
        self.rows.push(Assigned {
            key,
            kind: kind.to_owned(),
            id: id.to_owned(),
            round,
            what,
            records: Vec::new(),
        });
        self.rows.len() - 1
    }

    /// How many rows it assigns.
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The ids of the records of the row of `key`, ascending; none for a
    /// key of no row.
    pub fn records_of(&self, key: &str) -> Option<&[EventId]> {
        Some(&self.rows[*self.by_key.get(key)?].records)
    }

    /// Roughly how many bytes it holds: each row's key, what it is of and
    /// its records' ids, and what it finds the rows by.
    pub fn approx_bytes(&self) -> usize {
        let rows: usize = self
            .rows
            .iter()
            .map(|row| {
                size_of::<Assigned>()
                    + size_of::<(String, usize)>()
                    + 2 * row.key.len()
                    + row.kind.len()
                    + row.id.len()
                    + row.records.len() * size_of::<EventId>()
            })
            .sum();
        let finders = self.rounds.len()
            + self.by_open.len()
            + self.by_planner.len()
            + self.by_job.len()
            + self.outside.len()
            + self.models.len()
            + self.jobs_by_session.len();
        rows + finders * size_of::<(String, String, usize)>()
    }

    /// The keys of every row, in the order they were made.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.rows.iter().map(|row| row.key.as_str())
    }

    /// The row of `key` folded from its records among `events` (the
    /// session events assigned, ascending id), built at `built_at`; none
    /// for a key of no row.
    pub fn row(&self, key: &str, events: &[RunEvent], built_at: &str) -> Option<LedgerRow> {
        let assigned = &self.rows[*self.by_key.get(key)?];
        let mut row = Building::new(&assigned.kind, &assigned.id, assigned.round);
        if assigned.what != "events" {
            row.items = json!({"what": assigned.what});
        }
        row.records = assigned
            .records
            .iter()
            .filter_map(|id| {
                let at = events.binary_search_by_key(id, |event| event.id).ok()?;
                Some(events[at].clone())
            })
            .collect();
        Some(finish(row, &self.models, built_at))
    }
}

/// What an event of a session of no row is of: a plan review's or goal
/// review's by its id, the observer's or a throughput review's by its
/// kind, else the queue's sessions.
fn outside_of(event: &RunEvent) -> (String, String) {
    let job = JobRef::of(&event.payload);
    if let Some(id) = job.plan_review_id {
        return (PLAN_REVIEW.into(), id);
    }
    if let Some(id) = job.goal_review_id {
        return (GOAL_REVIEW.into(), id);
    }
    if event.kind.is_in(&OBSERVER_KINDS) {
        return (OBSERVER.into(), String::new());
    }
    if event.kind.is_in(&THROUGHPUT_REVIEW_KINDS) {
        return (THROUGHPUT_REVIEW.into(), String::new());
    }
    // A job its supervisor stopped names only its kind and what it was of.
    if event.kind == EventKind::HeadlessJobStopped
        && let Some(kind) = job.kind
    {
        return (kind, job.of.unwrap_or_default());
    }
    ("session".into(), String::new())
}

/// The row of a folded session or job: its span or job folded from its
/// records (a job's model from its session's close in `models`, which is
/// the span's row), and its Executions.
fn finish(
    mut row: Building,
    models: &HashMap<String, (String, EventId)>,
    built_at: &str,
) -> LedgerRow {
    let mut reads = None;
    match row.items["what"].as_str() {
        Some("session") => {
            if let Some(span) = spans(&row.records).first() {
                let provider = span_provider(row.records.first(), span);
                row.items = span_items(span, provider.as_deref());
                row.started_at = Some(span.start);
                row.ended_at = span.end;
                row.outcome = span_result(span);
                row.subject.route.clone_from(&span.route);
                row.subject.model.clone_from(&span.model);
                row.subject.provider = provider;
            }
        }
        Some("job") => {
            if let Some(job) = job_lives_with(&row.records, models).first() {
                reads = job.model_from;
                row.items = job_items(job);
                row.started_at = job.started_at;
                row.ended_at = job.ended_at;
                row.outcome = job_result(job);
                row.subject.provider = Some(job.provider.clone());
                row.subject.model = Some(job.model.clone());
            }
        }
        _ => {
            let times: Vec<i64> = row
                .records
                .iter()
                .filter_map(|event| timestamp_millis(&event.created_at))
                .collect();
            row.started_at = times.iter().min().copied();
        }
    }
    let mut ledger = LedgerRow::new(
        LedgerKind::Session,
        row.key(),
        row.records.iter().find_map(|event| event.task_id),
        built_at,
    );
    ledger.started_at = row.started_at;
    ledger.ended_at = row.ended_at;
    ledger.outcome = row.outcome;
    ledger.detail = json!({
        "items": row.items,
        "executions": entries(&executions(&row.records)),
    });
    ledger.subject = Some(row.subject);
    ledger.own(row.records);
    // A job's inputs reach the close of its session it read the model of,
    // so that a close recorded later makes the row dirty.
    if let Some(close) = reads {
        ledger.inputs_through.events = ledger.inputs_through.events.max(close);
    }
    ledger
}

/// Whether a session row is a span's (not a job's or events outside one).
pub(super) fn is_span(row: &LedgerRow) -> bool {
    row.detail["items"]["what"] == "session"
}

#[cfg(test)]
mod tests;
