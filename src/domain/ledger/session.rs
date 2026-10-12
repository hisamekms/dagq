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

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::{LedgerKind, LedgerRow, Subject};
use crate::domain::{
    EventId, EventKind, RunEvent,
    headless_job::JobRef,
    sessions::{
        GOAL_REVIEW, HOOK_KINDS, INFERRED, OBSERVER, PLAN_REVIEW, SESSION_OPENED, SpanRef,
        THROUGHPUT_REVIEW,
    },
    stats::{
        executions::{Execution, executions},
        jobs::{
            DEFAULT_PROVIDER, JobLife, job_lives, job_lives_with, job_of, session_models,
            starts_job,
        },
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
        .filter(|event| event.kind == SESSION_OPENED)
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
const OBSERVER_KINDS: [&str; 2] = [
    EventKind::ObserveStarted.as_str(),
    EventKind::ObserveFinished.as_str(),
];
const THROUGHPUT_REVIEW_KINDS: [&str; 3] = [
    EventKind::ThroughputReviewStarted.as_str(),
    EventKind::ThroughputReviewFinished.as_str(),
    EventKind::ThroughputReviewReported.as_str(),
];

/// The session rows of `events`: the session's and job's events of no run
/// ([`super::owner::Owner::Session`]), ascending id, built at
/// `built_at`. A span is a row from its open; its turns, cuts and close
/// go to it by `opened_event_id`. A job is a row from its start to its end,
/// a later event of its kind and key to its last row. A runtime planner's
/// event goes to its span open last, an event of none of these to the
/// row of round 0 of what it is of. Rounds count from 1 per kind and id.
pub fn session_rows(events: &[RunEvent], built_at: &str) -> Vec<LedgerRow> {
    let mut rows: Vec<Building> = Vec::new();
    let mut rounds: HashMap<(String, String, &'static str), i64> = HashMap::new();
    let mut by_open: HashMap<EventId, usize> = HashMap::new();
    let mut by_planner: HashMap<i64, usize> = HashMap::new();
    let mut by_job: HashMap<(&'static str, String), usize> = HashMap::new();
    let mut outside: HashMap<(String, String), usize> = HashMap::new();
    let mut next = |kind: &str, id: &str, what: &'static str| -> i64 {
        let round = rounds
            .entry((kind.to_owned(), id.to_owned(), what))
            .or_default();
        *round += 1;
        *round
    };
    for event in events {
        let span = SpanRef::of(event);
        let at = if event.kind == SESSION_OPENED {
            let kind = span.kind.as_deref().unwrap_or_default();
            let id = span_target(event);
            let mut row = Building::new(kind, &id, next(kind, &id, "session"));
            row.items = json!({"what": "session"});
            by_open.insert(event.id, rows.len());
            if let Some(planner) = span.planner_id {
                by_planner.insert(planner, rows.len());
            }
            rows.push(row);
            rows.len() - 1
        } else if let Some(at) = span.opened_event_id.and_then(|opened| by_open.get(&opened)) {
            *at
        } else if starts_job(event)
            && let Some((kind, key)) = job_of(event)
        {
            let mut row = Building::new(kind, &key, next(kind, &key, "job"));
            row.items = json!({"what": "job"});
            by_job.insert((kind, key), rows.len());
            rows.push(row);
            rows.len() - 1
        } else if let Some(at) = job_of(event).and_then(|key| by_job.get(&key)) {
            *at
        } else if let Some(at) = span.planner_id.and_then(|planner| by_planner.get(&planner)) {
            *at
        } else {
            let (kind, id) = match (span.planner_id, job_of(event)) {
                (Some(planner), _) => ("runtime_planner".to_owned(), planner.to_string()),
                (None, Some((kind, key))) => (kind.to_owned(), key),
                (None, None) => outside_of(event),
            };
            match outside.get(&(kind.clone(), id.clone())) {
                Some(at) => *at,
                None => {
                    outside.insert((kind.clone(), id.clone()), rows.len());
                    rows.push(Building::new(&kind, &id, 0));
                    rows.len() - 1
                }
            }
        };
        rows[at].records.push(event.clone());
    }
    // A job's model is its session's close, which is that span's row.
    let models = session_models(events);
    rows.into_iter()
        .map(|row| finish(row, &models, built_at))
        .collect()
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
    if OBSERVER_KINDS.contains(&event.kind.as_str()) {
        return (OBSERVER.into(), String::new());
    }
    if THROUGHPUT_REVIEW_KINDS.contains(&event.kind.as_str()) {
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
