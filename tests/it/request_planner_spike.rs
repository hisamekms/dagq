//! A Spike's replanning request (ADR-t1487-1 decisions 5 and 6) through
//! the supervisor loop: the pass records the request of a completed Spike
//! that has none, the request gets a planner of the runtime's within
//! `runtime_planners` (one per goal at a time), whose prompt carries the
//! Spike's result, the goal's other Spikes' results, the five ways on and
//! the check of the evidence's conditions; a worker is claimed beside it,
//! and a supervisor started again opens no second request or planner. The
//! backend (the wrappers parked in the background) and the stub reviewer
//! are plan review's.

use crate::plan_review::{
    PlanWorkspace, StubReviewer, add, completed_spike, fixture, open_goal, options, planner_prompt,
    supervise_with,
};
use dagq::{
    domain::{EventId, PlannerId, Priority, TaskId},
    infrastructure::sqlite::SqliteQueue,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

/// Each open planner's request.
fn planners(queue: &SqliteQueue) -> Vec<(PlannerId, Option<i64>)> {
    queue
        .planners(false)
        .unwrap()
        .into_iter()
        .map(|planner| (planner.id, planner.request_id.map(|id| id.as_i64())))
        .collect()
}

/// The Spike of each request, by request ID.
fn requests(db: &Path) -> Vec<(i64, i64, String)> {
    let connection = Connection::open(db).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT id, spike_task_id, status FROM plan_requests WHERE spike_task_id IS NOT NULL
             ORDER BY id",
        )
        .unwrap();
    statement
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// End `planner` as a session that exited.
fn exit(queue: &SqliteQueue, planner: PlannerId) {
    queue.register_planner_wrapper(planner, 1).unwrap();
    queue.register_planner_agent(planner, 1, 1).unwrap();
    queue.planner_exited(planner, 1, 0).unwrap();
}

fn attention(db: &Path, kind: &str) -> Vec<Value> {
    dagq::compose::events(db, EventId::new(0), 500, false).unwrap()["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == kind)
        .cloned()
        .collect()
}

/// Spikes 2 and 3 of one goal (holds, does_not_hold) and goalless Spike 4
/// (unresolved) completed without their requests, with one place for the
/// runtime's planners. The first pass records the three requests and opens one planner,
/// Spike 2's, whose prompt has its result, Spike 3's among the goal's other
/// Spikes, the five ways on and the check of the evidence's conditions;
/// the others wait, `open`. Started again, the supervisor records and opens
/// nothing more. Each planner that stops by declining (no attention:
/// nobody asked) and exits gives its place to the next request, never two
/// at once nor two of one goal, until each Spike's planner has had its
/// result.
#[test]
fn a_completed_spike_s_request_gets_a_planner_within_the_limit_one_per_goal() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let spikes: Vec<TaskId> = (0..3)
        .map(|n| add(&mut queue, &format!("spike {n}"), &[], Priority::Normal))
        .collect();
    completed_spike(&fx.db, spikes[0], Some(goal), "holds");
    completed_spike(&fx.db, spikes[1], Some(goal), "does_not_hold");
    completed_spike(&fx.db, spikes[2], None, "unresolved");
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let backend = PlanWorkspace::default();
    let limits = options(1, Duration::from_secs(3600));
    supervise_with(&fx, &backend, &reviewer, &limits);
    let recorded = requests(&fx.db);
    assert_eq!(
        recorded
            .iter()
            .map(|(_, spike, status)| (*spike, status.as_str()))
            .collect::<Vec<_>>(),
        spikes
            .iter()
            .map(|spike| (spike.as_i64(), "open"))
            .collect::<Vec<_>>()
    );
    let request_of = |spike: TaskId| {
        recorded
            .iter()
            .find(|(_, of, _)| *of == spike.as_i64())
            .unwrap()
            .0
    };
    let open = planners(&queue);
    assert_eq!(
        open.iter().map(|(_, request)| *request).collect::<Vec<_>>(),
        [Some(request_of(spikes[0]))],
        "{open:?}"
    );
    let first = planner_prompt(&fx.db, open[0].0);
    for expected in [
        format!("Spike task {} completed", spikes[0]),
        "no person asked".to_owned(),
        "\"verdict\": \"holds\"".to_owned(),
        format!("grounds of {}", spikes[0]),
        format!("tidy after {}", spikes[0]),
        "## The goal's other Spikes".to_owned(),
        "\"verdict\": \"does_not_hold\"".to_owned(),
        "git log --oneline <conditions.commit>..main".to_owned(),
        "conditions.tools".to_owned(),
        "1. Go on to the implementation".to_owned(),
        "2. A further Spike".to_owned(),
        "3. An alternative, or 4. a smaller scope".to_owned(),
        "5. Stop".to_owned(),
        "registered as drafts by the runtime".to_owned(),
        format!("dagq request decline {}", request_of(spikes[0])),
    ] {
        assert!(first.contains(&expected), "{expected}\n{first}");
    }
    assert!(!first.contains("the person's own words"), "{first}");

    // Started again: no second request, no second planner.
    supervise_with(&fx, &backend, &reviewer, &limits);
    assert_eq!(requests(&fx.db).len(), 3);
    assert_eq!(planners(&queue), open);

    // Each planner declines and exits; the next request gets the place.
    let goal_requests = [request_of(spikes[0]), request_of(spikes[1])];
    let mut prompts = vec![(request_of(spikes[0]), first)];
    let mut current = open[0];
    while prompts.len() < 3 {
        queue
            .decline_request(
                dagq::domain::RequestId::new(current.1.unwrap()),
                "nothing is left to plan",
                "planner",
                Some(current.0),
            )
            .unwrap();
        exit(&queue, current.0);
        let mut next = None;
        for _ in 0..3 {
            supervise_with(&fx, &backend, &reviewer, &limits);
            let now = planners(&queue);
            assert!(now.len() <= 1, "{now:?}");
            if let Some(&(planner, request)) = now.first()
                && planner != current.0
            {
                next = Some((planner, request));
                break;
            }
        }
        let (planner, request) = next.expect("the next request gets a planner");
        assert!(
            !prompts.iter().any(|(seen, _)| Some(*seen) == request),
            "{request:?}"
        );
        prompts.push((request.unwrap(), planner_prompt(&fx.db, planner)));
        current = (planner, request);
    }
    assert!(attention(&fx.db, "request_declined").is_empty());
    let prompt_of = |request: i64| &prompts.iter().find(|(r, _)| *r == request).unwrap().1;
    let second = prompt_of(goal_requests[1]);
    assert!(
        second.contains("\"verdict\": \"does_not_hold\""),
        "{second}"
    );
    assert!(
        second.contains(&format!("grounds of {}", spikes[0])),
        "{second}"
    );
    let unresolved = prompt_of(request_of(spikes[2]));
    assert!(
        unresolved.contains("\"verdict\": \"unresolved\""),
        "{unresolved}"
    );
    assert!(
        !unresolved.contains("## The goal's other Spikes"),
        "{unresolved}"
    );
}

/// Two Spikes of one goal completed and a place left: one planner replans
/// the goal at a time, so the later request waits for the earlier's
/// planner while another goal's request takes the place.
#[test]
fn a_goal_s_spike_requests_get_one_planner_at_a_time() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let spikes: Vec<TaskId> = (0..3)
        .map(|n| add(&mut queue, &format!("spike {n}"), &[], Priority::Normal))
        .collect();
    completed_spike(&fx.db, spikes[0], Some(goal), "holds");
    completed_spike(&fx.db, spikes[1], Some(goal), "holds");
    completed_spike(&fx.db, spikes[2], None, "holds");
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let backend = PlanWorkspace::default();
    let limits = options(3, Duration::from_secs(3600));
    for _ in 0..2 {
        supervise_with(&fx, &backend, &reviewer, &limits);
        let recorded = requests(&fx.db);
        let open: Vec<Option<i64>> = planners(&queue)
            .into_iter()
            .map(|(_, request)| request)
            .collect();
        assert_eq!(open, [Some(recorded[0].0), Some(recorded[2].0)], "{open:?}");
        assert_eq!(recorded[1].2, "open");
    }
}
