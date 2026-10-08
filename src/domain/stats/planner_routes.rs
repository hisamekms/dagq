//! How the runtime's planners got on per route (ADR-t1394-2 decision 4),
//! the planners' counterpart of the workers' [`super::routes`]: the
//! planners opened by what they were opened for, the time from a revise
//! sent to a planner to the proposal's next plan review, from a draft's,
//! a finding's or a planning request's planner opening to what became of
//! it, the `planner_question` asks they opened, their turns' outcomes and
//! failures, and the tokens of their sessions, each counted under the
//! route of the planner it is about (`interactive` / `headless`, `unknown`
//! when nothing recorded which).
use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use serde_json::Value;

use super::{
    EventId, RunEvent, TaskId, asks::UNKNOWN, executions::ExecutionTotals, sessions::INTERACTIVE,
    sessions::TimeSummary, timestamp_millis,
};
use crate::domain::{
    event_kind::{
        ASK_OPENED, DRAFT_PLANNER_OPENED, DRAFT_PLANNER_SETTLED, FINDING_PLANNER_OPENED,
        FINDING_STATUS_CHANGED, PLAN_REVIEW_STARTED, PLAN_REVISE_SENT, REQUEST_DECLINED,
        REQUEST_PLANNER_OPENED, REQUEST_PROPOSED, SESSION_OPENED, TURN_FINISHED, TURN_REQUESTED,
        TURN_SESSION_IDENTIFIED, TURN_STARTED,
    },
    sessions::{HEADLESS_ROUTE, RUNTIME_PLANNER},
};

/// The ask kind a planner asks a person with.
const PLANNER_QUESTION: &str = "planner_question";

/// One route's planners in a window.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PlannerRouteHealth {
    /// The planners opened, by what for: `revise` (a plan review sent the
    /// proposal back and a planner was opened for it), `draft`, `finding`
    /// and `request`.
    pub opened: BTreeMap<String, i64>,
    /// Seconds from each `plan_revise_sent` to the proposal's next
    /// `plan_review_started`, counted when that start is in the window.
    pub revise_to_review: TimeSummary,
    /// Seconds from a draft planner's opening to each of its drafts'
    /// `draft_planner_settled`, counted when that is in the window.
    pub draft_settle: TimeSummary,
    /// Seconds from a finding planner's opening to the finding's next
    /// status change, counted when that is in the window.
    pub finding_settle: TimeSummary,
    /// Seconds from a request planner's opening to the request's proposal
    /// or decline, counted when that is in the window.
    pub request_settle: TimeSummary,
    /// The `planner_question` asks its planners opened.
    pub planner_questions: i64,
    /// The headless turns that finished (`turn_finished`).
    pub turns: i64,
    /// By `outcome`.
    pub turn_outcomes: BTreeMap<String, i64>,
    /// By `failure`, for the turns that had one.
    pub turn_failures: BTreeMap<String, i64>,
    /// The tokens of its `runtime_planner` sessions' Executions that ended
    /// in the window: `stats`' `sessions.by_route.runtime_planner`'s.
    pub tokens: ExecutionTotals,
}

/// The planners' health per route of the events with `after < id <= upto`
/// whose task `counts` accepts (an event on no task, a queue's, is counted
/// only without `--goal`: `counts(None)`), with the tokens per route of
/// `tokens` put in.
pub fn planner_routes(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
    tokens: Option<&BTreeMap<String, super::sessions::KindSessions>>,
) -> BTreeMap<String, PlannerRouteHealth> {
    let routes_of = planner_route_map(events);
    let route = |planner: Option<i64>| {
        planner
            .and_then(|planner| routes_of.get(&planner).cloned())
            .unwrap_or_else(|| UNKNOWN.to_owned())
    };
    let mut routes: BTreeMap<String, PlannerRouteHealth> = BTreeMap::new();
    let mut durations: BTreeMap<(String, &'static str), Vec<i64>> = BTreeMap::new();
    // When each was sent or opened (unix milliseconds) and by which
    // planner: the revise of a proposal, a draft's, a finding's and a
    // request's planner.
    let mut revises: HashMap<i64, (i64, Option<i64>)> = HashMap::new();
    let mut drafts: HashMap<(i64, Option<TaskId>), i64> = HashMap::new();
    let mut findings: HashMap<i64, (i64, Option<i64>)> = HashMap::new();
    let mut requests: HashMap<i64, (i64, Option<i64>)> = HashMap::new();
    let mut opened_drafts: Vec<i64> = Vec::new();
    for event in events {
        let inside = event.id > after && event.id <= upto && counts(event.task_id);
        let payload = &event.payload;
        let planner = payload["planner_id"].as_i64();
        let Some(at) = timestamp_millis(&event.created_at) else {
            continue;
        };
        let mut took = |key: &'static str, planner: Option<i64>, from: i64| {
            if inside {
                durations
                    .entry((route(planner), key))
                    .or_default()
                    .push((at - from).max(0) / 1000);
            }
        };
        let mut opened = |what: &str, planner: Option<i64>| {
            if inside {
                *routes
                    .entry(route(planner))
                    .or_default()
                    .opened
                    .entry(what.to_owned())
                    .or_default() += 1;
            }
        };
        match event.kind.as_str() {
            PLAN_REVISE_SENT => {
                if let Some(proposal) = payload["proposal_id"].as_i64() {
                    revises.insert(proposal, (at, planner));
                }
                if payload["opened"] == true {
                    opened("revise", planner);
                }
            }
            PLAN_REVIEW_STARTED => {
                let sent = payload["proposal_id"]
                    .as_i64()
                    .and_then(|proposal| revises.remove(&proposal));
                if let Some((from, planner)) = sent {
                    took("revise_to_review", planner, from);
                }
            }
            DRAFT_PLANNER_OPENED => {
                if let Some(planner) = planner {
                    drafts.insert((planner, event.task_id), at);
                    // One planner opens for a bundle of drafts, one event
                    // each.
                    if !opened_drafts.contains(&planner) {
                        opened_drafts.push(planner);
                        opened("draft", Some(planner));
                    }
                }
            }
            DRAFT_PLANNER_SETTLED => {
                let from = planner.and_then(|planner| drafts.remove(&(planner, event.task_id)));
                if let Some(from) = from {
                    took("draft_settle", planner, from);
                }
            }
            FINDING_PLANNER_OPENED => {
                if let Some(finding) = payload["finding_id"].as_i64() {
                    findings.insert(finding, (at, planner));
                }
                opened("finding", planner);
            }
            FINDING_STATUS_CHANGED => {
                let from = payload["finding_id"]
                    .as_i64()
                    .and_then(|finding| findings.remove(&finding));
                if let Some((from, planner)) = from {
                    took("finding_settle", planner, from);
                }
            }
            REQUEST_PLANNER_OPENED => {
                if let Some(request) = payload["request_id"].as_i64() {
                    requests.insert(request, (at, planner));
                }
                opened("request", planner);
            }
            REQUEST_PROPOSED | REQUEST_DECLINED => {
                let from = payload["request_id"]
                    .as_i64()
                    .and_then(|request| requests.remove(&request));
                if let Some((from, planner)) = from {
                    took("request_settle", planner, from);
                }
            }
            ASK_OPENED if inside && payload["kind"] == PLANNER_QUESTION => {
                let asker = event
                    .actor
                    .as_ref()
                    .map(|actor| actor.id.as_str())
                    .or_else(|| payload["asked_by"].as_str());
                routes
                    .entry(route(asker.and_then(planner_of_actor)))
                    .or_default()
                    .planner_questions += 1;
            }
            TURN_FINISHED if inside && event.run_id.is_none() && planner.is_some() => {
                let health = routes.entry(route(planner)).or_default();
                health.turns += 1;
                *health
                    .turn_outcomes
                    .entry(text(payload, "outcome"))
                    .or_default() += 1;
                if let Some(failure) = payload["failure"].as_str() {
                    *health.turn_failures.entry(failure.to_owned()).or_default() += 1;
                }
            }
            _ => {}
        }
    }
    for ((route, key), secs) in durations {
        let health = routes.entry(route).or_default();
        let summary = TimeSummary::of(secs);
        match key {
            "revise_to_review" => health.revise_to_review = summary,
            "draft_settle" => health.draft_settle = summary,
            "finding_settle" => health.finding_settle = summary,
            _ => health.request_settle = summary,
        }
    }
    for (route, sessions) in tokens.into_iter().flatten() {
        if sessions.tokens != ExecutionTotals::default() {
            routes.entry(route.clone()).or_default().tokens = sessions.tokens.clone();
        }
    }
    routes
}

/// The route of each planner of the runtime's: `headless` once its turns
/// or its span say so, else `interactive` once the hook recorded its
/// session; a planner of neither is not listed.
fn planner_route_map(events: &[RunEvent]) -> HashMap<i64, String> {
    let mut routes: HashMap<i64, String> = HashMap::new();
    for event in events {
        let payload = &event.payload;
        let Some(planner) = payload["planner_id"].as_i64() else {
            continue;
        };
        let route = match event.kind.as_str() {
            TURN_REQUESTED | TURN_STARTED | TURN_FINISHED | TURN_SESSION_IDENTIFIED
                if event.run_id.is_none() =>
            {
                HEADLESS_ROUTE
            }
            SESSION_OPENED if payload["kind"] == RUNTIME_PLANNER => {
                payload["route"].as_str().unwrap_or(INTERACTIVE)
            }
            _ => continue,
        };
        // A headless planner's turns outrank the absence of a route.
        if route == HEADLESS_ROUTE || !routes.contains_key(&planner) {
            routes.insert(planner, route.to_owned());
        }
    }
    routes
}

/// The planner an actor id `planner:<id>` names.
fn planner_of_actor(actor: &str) -> Option<i64> {
    actor.strip_prefix("planner:")?.parse().ok()
}

fn text(payload: &Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or(UNKNOWN)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventActor, RunId, stats::sessions::KindSessions};
    use serde_json::json;

    fn event(id: i64, task: Option<i64>, kind: &str, payload: Value, secs: i64) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: task.map(TaskId::new),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: format!("2026-10-03T00:{:02}:{:02}Z", secs / 60, secs % 60),
            actor: None,
        }
    }

    /// Planner 1 is headless (its turns), planner 2 interactive (the
    /// hook's span), planner 3 neither.
    fn fixture() -> Vec<RunEvent> {
        let mut question = event(
            12,
            None,
            ASK_OPENED,
            json!({"kind": "planner_question"}),
            300,
        );
        question.actor = Some(EventActor {
            role: "planner".into(),
            id: "planner:1".into(),
            requested_by: None,
        });
        vec![
            event(
                1,
                Some(10),
                PLAN_REVISE_SENT,
                json!({"proposal_id": 5, "planner_id": 1, "opened": true}),
                0,
            ),
            event(
                2,
                None,
                TURN_STARTED,
                json!({"planner_id": 1, "turn": 1}),
                1,
            ),
            event(
                3,
                None,
                TURN_FINISHED,
                json!({"planner_id": 1, "outcome": "succeeded"}),
                60,
            ),
            event(
                4,
                None,
                TURN_FINISHED,
                json!({"planner_id": 1, "outcome": "failed", "failure": "usage_limit"}),
                70,
            ),
            event(
                5,
                Some(10),
                PLAN_REVIEW_STARTED,
                json!({"proposal_id": 5}),
                120,
            ),
            event(
                6,
                Some(20),
                DRAFT_PLANNER_OPENED,
                json!({"planner_id": 2}),
                130,
            ),
            event(
                7,
                Some(21),
                DRAFT_PLANNER_OPENED,
                json!({"planner_id": 2}),
                130,
            ),
            event(
                8,
                None,
                SESSION_OPENED,
                json!({"kind": "runtime_planner", "planner_id": 2}),
                131,
            ),
            event(
                9,
                Some(20),
                DRAFT_PLANNER_SETTLED,
                json!({"planner_id": 2}),
                190,
            ),
            event(
                10,
                None,
                FINDING_PLANNER_OPENED,
                json!({"planner_id": 3, "finding_id": 7}),
                200,
            ),
            event(
                11,
                None,
                FINDING_STATUS_CHANGED,
                json!({"finding_id": 7}),
                230,
            ),
            question,
            event(
                13,
                None,
                REQUEST_PLANNER_OPENED,
                json!({"planner_id": 1, "request_id": 4}),
                310,
            ),
            event(
                14,
                None,
                REQUEST_DECLINED,
                json!({"planner_id": 1, "request_id": 4}),
                400,
            ),
            // A worker's turn is the workers' routes'.
            RunEvent {
                run_id: Some(RunId::new("r").unwrap()),
                ..event(
                    15,
                    Some(10),
                    TURN_FINISHED,
                    json!({"outcome": "failed"}),
                    410,
                )
            },
        ]
    }

    #[test]
    fn the_planners_are_counted_under_their_route() {
        let events = fixture();
        let tokens = BTreeMap::from([(
            HEADLESS_ROUTE.to_owned(),
            KindSessions {
                tokens: ExecutionTotals {
                    executions: 1,
                    input: 10,
                    ..ExecutionTotals::default()
                },
                ..KindSessions::default()
            },
        )]);
        let routes = planner_routes(
            &events,
            EventId::new(0),
            EventId::new(15),
            |_| true,
            Some(&tokens),
        );
        let headless = &routes[HEADLESS_ROUTE];
        assert_eq!(
            headless.opened,
            BTreeMap::from([("request".to_owned(), 1), ("revise".to_owned(), 1)])
        );
        assert_eq!(headless.revise_to_review.summary.total, 120);
        assert_eq!(headless.request_settle.max, Some(90));
        assert_eq!(headless.planner_questions, 1);
        assert_eq!(headless.turns, 2);
        assert_eq!(headless.turn_failures["usage_limit"], 1);
        assert_eq!(headless.tokens.input, 10);
        let interactive = &routes[INTERACTIVE];
        assert_eq!(interactive.opened["draft"], 1);
        assert_eq!(interactive.draft_settle.summary.count, 1);
        assert_eq!(interactive.draft_settle.summary.total, 60);
        assert_eq!(interactive.turns, 0);
        assert_eq!(routes[UNKNOWN].finding_settle.summary.total, 30);
        assert_eq!(routes[UNKNOWN].opened["finding"], 1);

        // The window counts what ends in it, from wherever it started.
        let late = planner_routes(&events, EventId::new(4), EventId::new(5), |_| true, None);
        assert_eq!(late[HEADLESS_ROUTE].revise_to_review.summary.count, 1);
        assert!(late[HEADLESS_ROUTE].opened.is_empty());
        assert_eq!(late[HEADLESS_ROUTE].turns, 0);
        // An event on no task is not a goal's.
        let goal = planner_routes(
            &events,
            EventId::new(0),
            EventId::new(15),
            |task| task.is_some(),
            None,
        );
        assert_eq!(goal[HEADLESS_ROUTE].turns, 0);
        assert_eq!(goal[HEADLESS_ROUTE].revise_to_review.summary.count, 1);
    }

    #[test]
    fn a_planner_is_named_by_its_actor() {
        assert_eq!(planner_of_actor("planner:12"), Some(12));
        assert_eq!(planner_of_actor("inbox"), None);
        assert_eq!(planner_of_actor("planner:x"), None);
    }
}
