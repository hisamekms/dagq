//! How the workers got on per route (task 1371): after Claude's worker
//! went headless by default (task 1340), the turns' outcomes and failures,
//! the nudges, the `stalled` alerts by reason, the moves to the other
//! provider, Claude's cost per turn and the waits for a person, each
//! counted under the route (`worker_mode`: `interactive` / `headless`,
//! `unknown` before claims recorded it) the run was on when it happened.
use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use serde_json::Value;

use super::{EventId, RunEvent, TaskId, asks::UNKNOWN};
use crate::domain::{
    event_kind::{PROVIDER_SWITCHED, RECOVERY_REQUESTED, RUN_CLAIMED, STALL_NUDGED, TURN_FINISHED},
    waiting::{RouteWaits, UNKNOWN_ROUTE},
};

/// The `alert` of a `recovery_requested` counted in [`RouteHealth::stalled`].
const STALLED: &str = "stalled";
/// The provider whose turns' cost [`RouteHealth::claude_cost_usd`] sums.
const CLAUDE: &str = "claude";

/// One route's health in a window.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RouteHealth {
    /// The headless turns that finished (`turn_finished`).
    pub turns: i64,
    /// By `outcome` (`succeeded`, `failed`, `stopped`, ...).
    pub turn_outcomes: BTreeMap<String, i64>,
    /// By `failure`, for the turns that had one.
    pub turn_failures: BTreeMap<String, i64>,
    /// The nudges of a session idle without a receipt (`stall_nudged`).
    pub stall_nudged: i64,
    /// The `stalled` alerts by `reason` (`turn_without_receipt`,
    /// `permission_denied`, and the interactive route's
    /// `idle_without_receipt`, `send_unconfirmed`).
    pub stalled: BTreeMap<String, i64>,
    /// The moves to the other provider by `reason`, counted under the
    /// route the run was on before it moved.
    pub provider_switches: BTreeMap<String, i64>,
    /// Claude's cost of each turn that ran on Claude.
    pub claude_cost_usd: Cost,
    /// The waits for a person outside the slots
    /// ([`super::super::waiting::WaitingStats::by_route`]).
    pub waiting: RouteWaits,
}

/// The cost of the turns that gave one, in US dollars.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Cost {
    pub turns: i64,
    pub total: f64,
    pub median: Option<f64>,
    pub max: Option<f64>,
}

impl Cost {
    fn of(mut costs: Vec<f64>) -> Self {
        costs.sort_by(f64::total_cmp);
        let round = |value: f64| (value * 1e4).round() / 1e4;
        Self {
            turns: i64::try_from(costs.len()).unwrap_or(i64::MAX),
            total: round(costs.iter().sum()),
            median: super::median_f64(&mut costs.clone()).map(round),
            max: costs.last().copied().map(round),
        }
    }
}

/// The health per route of the events with `after < id <= upto` whose task
/// `counts` accepts, with the waits of `waits` (by route) put in.
pub fn route_health(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
    waits: &BTreeMap<String, RouteWaits>,
) -> BTreeMap<String, RouteHealth> {
    let mut routes: BTreeMap<String, RouteHealth> = BTreeMap::new();
    let mut costs: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    // Each run's route now.
    let mut current: HashMap<&str, String> = HashMap::new();
    let text = |payload: &Value, key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or(UNKNOWN)
            .to_owned()
    };
    for event in events {
        let run = event.run_id.as_ref().map_or("", |id| id.as_str());
        let route = current
            .get(run)
            .cloned()
            .unwrap_or_else(|| UNKNOWN_ROUTE.to_owned());
        let inside = event.id > after && event.id <= upto && counts(event.task_id);
        let payload = &event.payload;
        match event.kind.as_str() {
            // A headless planner's turns are the queue's, not a run's
            // (ADR-t1394-2).
            TURN_FINISHED if inside && event.run_id.is_some() => {
                let health = routes.entry(route.clone()).or_default();
                health.turns += 1;
                *health
                    .turn_outcomes
                    .entry(text(payload, "outcome"))
                    .or_default() += 1;
                if let Some(failure) = payload["failure"].as_str() {
                    *health.turn_failures.entry(failure.to_owned()).or_default() += 1;
                }
                if payload["provider"].as_str() == Some(CLAUDE)
                    && let Some(cost) = payload["cost_usd"].as_f64()
                {
                    costs.entry(route.clone()).or_default().push(cost);
                }
            }
            STALL_NUDGED if inside => routes.entry(route.clone()).or_default().stall_nudged += 1,
            RECOVERY_REQUESTED if inside && payload["alert"] == STALLED => {
                *routes
                    .entry(route.clone())
                    .or_default()
                    .stalled
                    .entry(text(payload, "reason"))
                    .or_default() += 1;
            }
            _ => {}
        }
        if matches!(event.kind.as_str(), RUN_CLAIMED | PROVIDER_SWITCHED) {
            if event.kind == PROVIDER_SWITCHED && inside {
                *routes
                    .entry(route.clone())
                    .or_default()
                    .provider_switches
                    .entry(text(payload, "reason"))
                    .or_default() += 1;
            }
            if let Some(mode) = payload["worker_mode"].as_str() {
                current.insert(run, mode.to_owned());
            } else if event.kind == RUN_CLAIMED {
                current.remove(run);
            }
        }
    }
    for (route, costs) in costs {
        routes.entry(route).or_default().claude_cost_usd = Cost::of(costs);
    }
    for (route, waits) in waits {
        routes.entry(route.clone()).or_default().waiting = waits.clone();
    }
    routes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{RunId, waiting::Durations};
    use serde_json::json;

    fn event(id: i64, run: &str, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: Some(RunId::new(run).unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: "2026-10-02T00:00:00Z".to_owned(),
            actor: None,
        }
    }

    fn turn(id: i64, run: &str, outcome: &str, failure: Option<&str>, cost: f64) -> RunEvent {
        event(
            id,
            run,
            TURN_FINISHED,
            json!({"outcome": outcome, "failure": failure, "provider": "claude", "cost_usd": cost}),
        )
    }

    #[test]
    fn the_turns_nudges_stalls_switches_and_costs_are_split_by_route() {
        let events = [
            event(1, "h", RUN_CLAIMED, json!({"worker_mode": "headless"})),
            event(2, "i", RUN_CLAIMED, json!({"worker_mode": "interactive"})),
            event(3, "old", RUN_CLAIMED, json!({})),
            turn(4, "h", "succeeded", None, 0.5),
            turn(5, "h", "failed", Some("usage_limit"), 0.25),
            turn(6, "h", "succeeded", None, 1.0),
            // A Codex turn has no Claude cost.
            event(
                7,
                "h",
                TURN_FINISHED,
                json!({"outcome": "succeeded", "provider": "codex", "cost_usd": null}),
            ),
            event(8, "h", STALL_NUDGED, json!({})),
            event(9, "h", STALL_NUDGED, json!({})),
            event(
                10,
                "h",
                RECOVERY_REQUESTED,
                json!({"alert": "stalled", "reason": "turn_without_receipt"}),
            ),
            event(
                11,
                "h",
                RECOVERY_REQUESTED,
                json!({"alert": "stalled", "reason": "permission_denied"}),
            ),
            // Another alert is not a stall.
            event(12, "h", RECOVERY_REQUESTED, json!({"alert": "failed"})),
            event(13, "i", STALL_NUDGED, json!({})),
            event(
                14,
                "i",
                RECOVERY_REQUESTED,
                json!({"alert": "stalled", "reason": "idle_without_receipt"}),
            ),
            // The interactive run moves to Codex, headless: the switch
            // counts under interactive, the turn after it under headless.
            event(
                15,
                "i",
                PROVIDER_SWITCHED,
                json!({"reason": "usage_limit", "worker_mode": "headless"}),
            ),
            turn(16, "i", "succeeded", None, 2.0),
            event(17, "old", STALL_NUDGED, json!({})),
        ];
        let waits = BTreeMap::from([(
            "headless".to_owned(),
            RouteWaits {
                started: 2,
                waited: Durations {
                    count: 2,
                    total_secs: 90,
                    median_secs: Some(30),
                    max_secs: Some(60),
                },
            },
        )]);
        let routes = route_health(&events, EventId::new(0), EventId::new(17), |_| true, &waits);
        let headless = &routes["headless"];
        assert_eq!(headless.turns, 5);
        assert_eq!(
            headless.turn_outcomes,
            BTreeMap::from([("failed".to_owned(), 1), ("succeeded".to_owned(), 4)])
        );
        assert_eq!(
            headless.turn_failures,
            BTreeMap::from([("usage_limit".to_owned(), 1)])
        );
        assert_eq!(headless.stall_nudged, 2);
        assert_eq!(
            headless.stalled,
            BTreeMap::from([
                ("permission_denied".to_owned(), 1),
                ("turn_without_receipt".to_owned(), 1)
            ])
        );
        assert_eq!(
            headless.claude_cost_usd,
            Cost {
                turns: 4,
                total: 3.75,
                median: Some(0.75),
                max: Some(2.0),
            }
        );
        assert_eq!(headless.waiting.started, 2);
        let interactive = &routes["interactive"];
        assert_eq!((interactive.turns, interactive.stall_nudged), (0, 1));
        assert_eq!(interactive.stalled["idle_without_receipt"], 1);
        assert_eq!(interactive.provider_switches["usage_limit"], 1);
        assert_eq!(interactive.claude_cost_usd, Cost::default());
        assert_eq!(routes[UNKNOWN_ROUTE].stall_nudged, 1);

        // The window leaves out what is before it, but still follows the
        // routes the claims before it set.
        let late = route_health(
            &events,
            EventId::new(15),
            EventId::new(17),
            |_| true,
            &BTreeMap::new(),
        );
        assert_eq!(late["headless"].turns, 1);
        assert!(!late.contains_key("interactive"));
        // A task not counted counts nothing.
        assert!(
            route_health(
                &events,
                EventId::new(0),
                EventId::new(17),
                |_| false,
                &BTreeMap::new()
            )
            .is_empty()
        );
    }
}
