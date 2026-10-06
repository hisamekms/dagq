//! A planner of the runtime's as the plan review tests play it (ADR-t1433-2):
//! a runtime planner's row an older binary opened in a cmux workspace,
//! which the supervisor closes without cmux; and the model a planner
//! opened for a revise starts with. Split from `plan_review` (task 1441)
//! to keep that file within its line limit; the helpers that play a
//! planner's turns are in `runtime_support::planner_turns`.

use crate::plan_review::{
    PlanWorkspace, StubReviewer, add, events, fixture, git, submit, supervise,
};
use crate::runtime_support::planner_turns::idle;
use dagq::{
    application::TaskStore,
    domain::{PlannerOrigin, PlannerOwner, Priority, Submission, TaskId},
    infrastructure::{location::planners_dir, sqlite::SqliteQueue},
};
use serde_json::{Value, json};
use std::fs;

/// A planner of the runtime's an older binary opened in a cmux workspace
/// (ADR-t1433-2 decision 3): its live wrapper notwithstanding, the runtime
/// neither looks its workspace up nor types into it nor closes it; it
/// closes the row as `runtime_session_gone`, and the revise of the
/// proposal it owns goes to a new headless planner in the background.
#[test]
fn a_runtime_planner_an_older_binary_opened_in_a_workspace_is_closed_without_cmux() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let old = queue.open_planner(PlannerOrigin::Runtime, None).unwrap();
    queue.planner_workspace_created(old.id, "OLD").unwrap();
    fs::create_dir_all(planners_dir(&fx.db).join(old.id.to_string())).unwrap();
    idle(&queue, &fx.db, old.id);
    let task = add(&mut queue, "retype", &[TaskId::new(1)], Priority::Normal);
    let proposal = queue
        .submit(Submission {
            tasks: vec![task],
            goals: Vec::new(),
            proposal: None,
            owner: PlannerOwner {
                origin: PlannerOrigin::Runtime,
                workspace_id: Some("OLD".into()),
            },
        })
        .unwrap()
        .id();
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "revise", "reasons": ["split it"], "summary": "too big"
    })]);
    // cmux still lists the old workspace; the runtime does not ask.
    let backend = PlanWorkspace::listing(&["OLD"]);
    supervise(&fx, &backend, &reviewer);
    supervise(&fx, &backend, &reviewer);
    assert!(queue.planner(old.id).unwrap().closed_at.is_some());
    let closes: Vec<Value> = queue
        .latest_events_of("planner_closed", 10)
        .unwrap()
        .into_iter()
        .map(|event| event.payload)
        .filter(|payload| payload["planner_id"] == old.id.as_i64())
        .collect();
    assert_eq!(closes.len(), 1, "{closes:?}");
    assert_eq!(closes[0]["code"], "runtime_session_gone");
    assert_eq!(closes[0]["workspace_closed"], false);
    assert!(backend.texts().is_empty(), "nothing typed");
    assert!(backend.exits.lock().unwrap().is_empty());
    assert!(backend.closed().is_empty(), "no workspace closed");
    // The revise went to a new planner of the runtime's, headless.
    let open = queue.planners(false).unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].proposal_id, Some(proposal));
    assert_eq!(open[0].route, dagq::domain::PlannerRoute::Headless);
    assert_eq!(backend.launched(), [open[0].workspace_id.clone().unwrap()]);
}

/// `[roles.plan_review]` and `[roles.runtime_planner]` of `dagq.toml`
/// (ADR-0079 decision 7): the plan review is given its model and effort,
/// and the planner opened for its revise starts one step above its role's
/// effort, `xhigh` at most; its span records the same.
#[test]
fn role_tables_set_the_plan_review_and_raise_the_revise_planner_from_them() {
    let fx = fixture();
    fs::write(
        fx.repo.join("dagq.toml"),
        "[roles.plan_review]\neffort = \"high\"\n\n[roles.runtime_planner]\nmodel = \"claude-sonnet-5\"\neffort = \"high\"\n",
    )
    .unwrap();
    git(&fx.repo, &["add", "dagq.toml"]);
    git(&fx.repo, &["commit", "-m", "roles"]);
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, "change", &[TaskId::new(1)], Priority::Normal);
    submit(&mut queue, &[task], None);
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "revise", "reasons": ["split it"], "summary": "too big"
    })]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    assert_eq!(
        reviewer.models(),
        [("claude-opus-5-5".to_owned(), "high".to_owned())]
    );
    let started = &events(&mut queue, task, "plan_review_started")[0];
    assert_eq!(
        started["launch"],
        json!({"role": "plan_review", "provider": "claude", "model": "claude-opus-5-5", "effort": "high",
               "source": "dagq.toml"})
    );
    let span = &events(&mut queue, task, "session_opened")[0];
    assert_eq!(span["kind"], "plan_review");
    assert_eq!(span["launch"], started["launch"]);
    let sent = &events(&mut queue, task, "plan_revise_sent")[0];
    assert_eq!(sent["launch"]["model"], "claude-sonnet-5");
    assert_eq!(sent["launch"]["effort"], "xhigh");
    assert_eq!(sent["launch"]["escalated_from"], "high");
    assert_eq!(backend.launched().len(), 1);
}
