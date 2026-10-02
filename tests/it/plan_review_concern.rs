//! A plan review's `concern` carries a recommendation, a confidence and a
//! reason a person is needed (ADR-t451-1 decision 4): the runtime applies
//! a sure `ready` as a pass and a sure `send_back` as a revise, leaves a
//! low confidence, a `scope`, a `discard` (cancel), a `send_back` past the
//! revise limit and the old shape to a person in an `approve_plan` ask
//! with the job's recommendation, and records `plan_concern_decided`.

use crate::plan_review::{
    PlanWorkspace, StubReviewer, add, events, fixture, status, submit, supervise,
};
use dagq::{
    application::TaskStore,
    domain::{
        AskConfidence, AskKind, AskReason, FindingTarget, NewFinding, PlannerOrigin, PlannerOwner,
        Priority, ProposalId, ProposalStatus, Submission, TaskId, TaskStatus,
    },
    infrastructure::sqlite::SqliteQueue,
    runtime,
};
use rusqlite::Connection;
use serde_json::{Value, json};

fn concern(recommendation: Value, confidence: Value, reason_category: Value) -> Value {
    json!({
        "verdict": "concern", "reasons": ["looks already implemented"], "summary": "maybe done",
        "recommendation": recommendation, "confidence": confidence,
        "reason_category": reason_category
    })
}

fn revise_reasons(db: &std::path::Path, proposal: ProposalId) -> Vec<String> {
    let text: String = Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT revise_reasons FROM proposals WHERE id=?1",
            [proposal.as_i64()],
            |r| r.get(0),
        )
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

#[test]
fn a_sure_ready_concern_is_applied_as_a_pass_with_its_actions_and_counted() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    let two = add(&mut queue, "two", &[blocker], Priority::High);
    let three = add(&mut queue, "three", &[blocker], Priority::Normal);
    // An improvement: the proposal remedies a finding, so a pass lowers
    // its high task to normal (ADR-0051 decision 26).
    let finding = queue
        .record_finding(NewFinding {
            kind: "conflict_hotspot".into(),
            target: FindingTarget::Queue,
            subject: "src/a.rs".into(),
            summary: "src/a.rs conflicts in most landings".into(),
            detail: None,
            impact: None,
            evidence: Vec::new(),
            propose: None,
            by: "observer".into(),
        })
        .unwrap()
        .finding
        .id;
    let proposal = queue
        .submit_linking(
            Submission {
                tasks: vec![two, three],
                goals: Vec::new(),
                proposal: None,
                owner: PlannerOwner {
                    origin: PlannerOrigin::Person,
                    workspace_id: None,
                },
            },
            &[finding],
        )
        .unwrap()
        .id();
    let mut verdict = concern(json!("ready"), json!("high"), Value::Null);
    verdict["actions"] = json!([{"action": "add_dependency", "task_id": three, "depends_on": two}]);
    let reviewer = StubReviewer::new(&[verdict]);
    let backend = PlanWorkspace::default();
    let outcome = supervise(&fx, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(status(&mut queue, two), TaskStatus::Ready);
    assert_eq!(status(&mut queue, three), TaskStatus::Ready);
    assert_eq!(
        queue.show_proposal(proposal).unwrap().status(),
        ProposalStatus::Accepted
    );
    assert_eq!(queue.show(three).unwrap().dependencies, [blocker, two]);
    assert_eq!(queue.show(two).unwrap().task.priority(), Priority::Normal);
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    assert!(backend.notifications().is_empty());
    let finished = &events(&mut queue, two, "plan_review_finished")[0];
    assert_eq!(finished["verdict"], "concern");
    assert_eq!(finished["decision"], "pass");
    let decided = events(&mut queue, two, "plan_concern_decided");
    assert_eq!(
        decided,
        [json!({
            "proposal_id": proposal, "plan_review_id": finished["plan_review_id"],
            "recommendation": "ready", "confidence": "high", "reason_category": null,
            "applied": true, "decision": "pass", "escalated_because": null, "ask_id": null
        })]
    );
    let stats = runtime::stats(&fx.db, &Default::default()).unwrap();
    assert_eq!(
        stats["recommendations"]["decided_without_ask"]["approve_plan"], 1,
        "{}",
        stats["recommendations"]
    );
}

#[test]
fn a_sure_send_back_is_a_revise_counted_toward_the_limit_then_a_person_decides() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, "stubborn", &[TaskId::new(1)], Priority::Normal);
    let proposal = submit(&mut queue, &[task], None);
    let reviewer = StubReviewer::new(&[concern(json!("send_back"), json!("high"), Value::Null)]);
    let backend = PlanWorkspace::default();
    let again = |queue: &mut SqliteQueue| {
        queue
            .submit(Submission {
                tasks: Vec::new(),
                goals: Vec::new(),
                proposal: Some(proposal),
                owner: PlannerOwner {
                    origin: PlannerOrigin::Runtime,
                    workspace_id: None,
                },
            })
            .unwrap();
    };
    supervise(&fx, &backend, &reviewer);
    let revising = queue.show_proposal(proposal).unwrap();
    assert_eq!(revising.status(), ProposalStatus::Revising);
    assert_eq!(revising.revise_count(), 1);
    assert_eq!(status(&mut queue, task), TaskStatus::Draft);
    assert_eq!(
        revise_reasons(&fx.db, proposal),
        ["looks already implemented"]
    );
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    let decided = events(&mut queue, task, "plan_concern_decided");
    assert_eq!(decided[0]["applied"], true);
    assert_eq!(decided[0]["decision"], "revise");
    assert_eq!(
        events(&mut queue, task, "plan_review_finished")[0]["decision"],
        "revise"
    );

    again(&mut queue);
    supervise(&fx, &backend, &reviewer);
    assert_eq!(queue.show_proposal(proposal).unwrap().revise_count(), 2);
    again(&mut queue);
    supervise(&fx, &backend, &reviewer);
    // Past the limit, a person decides with the job's recommendation.
    assert_eq!(status(&mut queue, task), TaskStatus::Submitted);
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].kind, AskKind::ApprovePlan);
    assert_eq!(asks[0].recommendation.as_deref(), Some("send_back"));
    assert_eq!(asks[0].confidence, Some(AskConfidence::High));
    assert_eq!(asks[0].reason_category, AskReason::Scope);
    assert!(
        asks[0]
            .question
            .contains("It recommends send_back (confidence high); left to a person: revise_limit."),
        "{}",
        asks[0].question
    );
    let decided = events(&mut queue, task, "plan_concern_decided");
    assert_eq!(decided.len(), 3);
    assert_eq!(decided[2]["applied"], false);
    assert_eq!(decided[2]["decision"], Value::Null);
    assert_eq!(decided[2]["escalated_because"], "revise_limit");
    assert_eq!(decided[2]["ask_id"], json!(asks[0].id));
    let stats = runtime::stats(&fx.db, &Default::default()).unwrap();
    assert_eq!(
        stats["recommendations"]["decided_without_ask"]["approve_plan"],
        2
    );
}

#[test]
fn low_scope_discard_and_the_old_shape_wait_for_a_person_with_the_recommendation() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    let cases = [
        (
            concern(json!("ready"), json!("low"), Value::Null),
            Some("ready"),
            Some(AskConfidence::Low),
            AskReason::Scope,
            "low_confidence",
        ),
        (
            concern(json!("ready"), json!("high"), json!("scope")),
            Some("ready"),
            Some(AskConfidence::High),
            AskReason::Scope,
            "scope",
        ),
        (
            concern(json!("cancel"), json!("high"), json!("discard")),
            Some("cancel"),
            Some(AskConfidence::High),
            AskReason::Discard,
            "discard",
        ),
        (
            json!({"verdict": "concern", "reasons": ["looks already implemented"], "summary": "maybe done"}),
            None,
            None,
            AskReason::Scope,
            "no_recommendation",
        ),
    ];
    let tasks: Vec<TaskId> = (0..cases.len())
        .map(|n| {
            let task = add(
                &mut queue,
                &format!("task {n}"),
                &[blocker],
                Priority::Normal,
            );
            submit(&mut queue, &[task], None);
            task
        })
        .collect();
    let verdicts: Vec<Value> = cases.iter().map(|case| case.0.clone()).collect();
    let reviewer = StubReviewer::new(&verdicts);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    assert_eq!(reviewer.prompts().len(), cases.len());
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), cases.len());
    for ((_, recommendation, confidence, reason, because), task) in cases.iter().zip(&tasks) {
        let ask = asks
            .iter()
            .find(|ask| ask.task_id == Some(*task))
            .unwrap_or_else(|| panic!("no ask for task {task}: {asks:?}"));
        assert_eq!(ask.kind, AskKind::ApprovePlan);
        assert_eq!(ask.options, ["ready", "send_back", "cancel"]);
        assert_eq!(ask.recommendation.as_deref(), *recommendation, "{because}");
        assert_eq!(ask.confidence, *confidence, "{because}");
        assert_eq!(ask.reason_category, *reason, "{because}");
        assert!(
            ask.question
                .contains(&format!("left to a person: {because}.")),
            "{}",
            ask.question
        );
        assert_eq!(status(&mut queue, *task), TaskStatus::Submitted);
        let decided = events(&mut queue, *task, "plan_concern_decided");
        assert_eq!(decided.len(), 1, "{because}");
        assert_eq!(decided[0]["applied"], false);
        assert_eq!(decided[0]["escalated_because"], *because);
        assert_eq!(decided[0]["recommendation"], json!(recommendation));
        assert_eq!(decided[0]["ask_id"], json!(ask.id));
        assert_eq!(
            events(&mut queue, *task, "plan_review_finished")[0]["decision"],
            "concern"
        );
    }
    let stats = runtime::stats(&fx.db, &Default::default()).unwrap();
    assert!(
        stats["recommendations"]["decided_without_ask"]
            .get("approve_plan")
            .is_none(),
        "{}",
        stats["recommendations"]
    );
    // A person's answer matching the recommendation counts as matched.
    queue.answer(asks[0].id, "ready").unwrap();
    supervise(&fx, &backend, &reviewer);
    let stats = runtime::stats(&fx.db, &Default::default()).unwrap();
    let approve = &stats["recommendations"]["by_kind"]["approve_plan"];
    assert_eq!(approve["answered"], 1, "{}", stats["recommendations"]);
    assert_eq!(approve["matched"], 1);
}

#[test]
fn a_sure_ready_with_an_action_that_does_not_hold_fails_as_a_pass_would() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, "self", &[TaskId::new(1)], Priority::Normal);
    let proposal = submit(&mut queue, &[task], None);
    let mut verdict = concern(json!("ready"), json!("high"), Value::Null);
    verdict["actions"] =
        json!([{"action": "cancel_duplicate", "task_id": task, "duplicate_of": task}]);
    let reviewer = StubReviewer::new(&[verdict]);
    let backend = PlanWorkspace::default();
    supervise(&fx, &backend, &reviewer);
    assert_eq!(status(&mut queue, task), TaskStatus::Submitted);
    assert_eq!(
        queue.show_proposal(proposal).unwrap().status(),
        ProposalStatus::Submitted
    );
    let failed = events(&mut queue, task, "plan_review_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(events(&mut queue, task, "plan_concern_decided").is_empty());
    assert!(queue.asks(Default::default()).unwrap().is_empty());
}
