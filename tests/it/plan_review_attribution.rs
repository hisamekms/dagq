//! A queue-hold restart preserves the planner that submitted the unchanged proposal.
//! Reuses the usage-limit restart boundary from plan_review, with a closed planner span.
use crate::plan_review::{
    PlanWorkspace, StubReviewer, add, events, fixture, status, submit, supervise,
};
use dagq::{
    domain::{AskKind, PlannerOrigin, Priority, TaskId, TaskStatus},
    infrastructure::sqlite::SqliteQueue,
    runtime,
};
use rusqlite::Connection;
use serde_json::{Value, json};

/// A plan review stopped at the usage limit is no `plan review by hand`
/// (task 438): the job joins the queue's usage-limit `cost` ask, listed in
/// its `affected`, and `usage_limited` is recorded on the queue. While the
/// ask is open no plan review starts; `done` submits the proposal again.
#[test]
fn a_plan_review_at_the_usage_limit_joins_the_cost_ask_and_starts_again_after_done() {
    use dagq::domain::{
        ActorContext, ActorRole, EventId,
        plan_quality::{plan_quality, planner_submission},
    };
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, "limited", &[TaskId::new(1)], Priority::Normal);
    // The planner finishes before the supervisor retries the unchanged proposal.
    let planner = queue.open_planner(PlannerOrigin::Person, None).unwrap();
    let conn = Connection::open(&fx.db).unwrap();
    conn.execute(
        "INSERT INTO run_events(kind,payload) VALUES ('session_opened',?1)",
        [json!({"kind":"planner","planner_id":planner.id}).to_string()],
    )
    .unwrap();
    let opened = conn.last_insert_rowid();
    let proposal = submit(
        &mut SqliteQueue::open(&fx.db)
            .unwrap()
            .with_actor(ActorContext::instance(ActorRole::Planner, planner.id)),
        &[task],
        None,
    );
    conn.execute(
        "INSERT INTO run_events(kind,payload) VALUES ('session_closed',?1)",
        [json!({"kind":"planner","opened_event_id":opened,"model":"planner-opus","effort":"high"}).to_string()],
    ).unwrap();
    let reviewer = StubReviewer::limited_then(
        &json!({"verdict": "pass", "reasons": [], "summary": "fits the goal"}),
    );
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    assert_eq!(reviewer.prompts().len(), 1);
    assert_eq!(events(&mut queue, task, "plan_review_failed").len(), 1);
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = &asks[0];
    assert_eq!(ask.kind, AskKind::QueueHold);
    assert_eq!(ask.reason_category, dagq::domain::AskReason::Cost);
    assert_eq!(ask.subject.as_deref(), Some("usage_limit"));
    let entry = format!("plan_review job of proposal {proposal}");
    assert_eq!(ask.affected, std::slice::from_ref(&entry));
    assert!(
        ask.question.ends_with(&format!("\n\nAffected: {entry}")),
        "{}",
        ask.question
    );
    let limited: Vec<_> = queue
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "usage_limited")
        .collect();
    assert_eq!(limited.len(), 1, "{limited:?}");
    assert_eq!(limited[0].task_id, None);
    assert_eq!(limited[0].payload["job"], "plan_review");
    assert_eq!(limited[0].payload["ask_id"], json!(ask.id));
    // The ask is the attention, not the failed plan review.
    let status_now = runtime::status(&fx.db).unwrap();
    let kinds: Vec<&Value> = status_now["attention"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| &a["kind"])
        .collect();
    assert!(
        !kinds.contains(&&json!("plan_review_failed")),
        "{status_now}"
    );
    assert!(kinds.contains(&&json!("ask_opened")), "{status_now}");
    // Held: no plan review starts.
    supervise(&fx, &backend, &reviewer);
    assert_eq!(reviewer.prompts().len(), 1);
    queue.answer(ask.id, "done").unwrap();
    supervise(&fx, &backend, &reviewer);
    assert_eq!(reviewer.prompts().len(), 2);
    assert_eq!(status(&mut queue, task), TaskStatus::Ready);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let applied: Vec<_> = queue
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "queue_hold_applied")
        .collect();
    assert_eq!(applied[0].payload["jobs"], json!([entry]));
    assert_eq!(
        applied[0].payload["restarted"],
        json!([{"job": "plan_review", "proposal_id": proposal}])
    );
    let history = queue.all_events().unwrap();
    let links = events(&mut queue, task, "proposal_submitted");
    assert_eq!(links.len(), 2);
    assert_eq!(links[0]["planner_id"], json!(planner.id));
    assert!(links[1]["planner_id"].is_null());
    assert!(links[1]["opened_event_id"].is_null());
    let restarted = history
        .iter()
        .filter(|e| e.kind == "plan_review_started")
        .max_by_key(|e| e.id)
        .unwrap();
    let attributed = planner_submission(&history, proposal.as_i64(), restarted.id);
    assert_eq!(attributed.planner_id, Some(planner.id.as_i64()));
    assert_eq!(attributed.opened_event_id, Some(EventId::new(opened)));
    assert_eq!(attributed.model.as_deref(), Some("planner-opus"));
    assert_eq!(attributed.effort.as_deref(), Some("high"));
    let quality = plan_quality(&history, EventId::new(0), EventId::new(i64::MAX), |_| true);
    assert_eq!(quality["all"].reviews, 1);
    assert_eq!(quality["all"].proposals, 1);
    assert_eq!(quality["planner_model=planner-opus"], quality["all"]);
    assert_eq!(quality["planner_effort=high"], quality["all"]);
    assert!(!quality.contains_key("planner_model=unknown"));
}
