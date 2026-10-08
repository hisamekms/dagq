//! A plan review's `concern` carries a recommendation, a confidence and a
//! reason a person is needed (ADR-t451-1 decision 4). Which concern the
//! runtime applies and what the ask of the rest says are decided by
//! `domain::plan_review::decide_verdict` and the supervisor's `plan_ask`,
//! whose unit tests hold the cases; this file keeps the wiring through the
//! queue: a sure `ready` applied as a pass, and, with the proposal's
//! revise count read when the job starts, a `send_back` and a `revise` past
//! the revise limit left to a person, with `plan_concern_decided` and
//! `plan_review_finished` recording why.

use crate::plan_review::{
    PlanWorkspace, StubReviewer, add, events, fixture, status, submit, supervise,
};
use dagq::{
    application::TaskStore,
    domain::{
        AskConfidence, AskKind, FindingTarget, NewFinding, PlannerOrigin, PlannerOwner, Priority,
        ProposalStatus, Submission, TaskId, TaskStatus,
    },
    infrastructure::sqlite::SqliteQueue,
    runtime,
};
use serde_json::{Value, json};

fn concern(recommendation: Value, confidence: Value, reason_category: Value) -> Value {
    json!({
        "verdict": "concern", "reasons": ["looks already implemented"], "summary": "maybe done",
        "recommendation": recommendation, "confidence": confidence,
        "reason_category": reason_category
    })
}

#[test]
fn a_sure_ready_concern_is_applied_as_a_pass_with_its_actions_and_counted() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let blocker = TaskId::new(1);
    let two = add(&mut queue, "two", &[blocker], Priority::High);
    let three = add(&mut queue, "three", &[blocker], Priority::Normal);
    // The proposal remedies a finding but a person submitted it, so a pass
    // keeps its high task's priority (ADR-t1971-1 decision 3).
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
    // Two more proposals already sent back to the limit.
    let limited = add(&mut queue, "limited", &[blocker], Priority::Normal);
    let stubborn = add(&mut queue, "stubborn", &[blocker], Priority::Normal);
    let at_limit = [limited, stubborn].map(|task| submit(&mut queue, &[task], None));
    for id in at_limit {
        rusqlite::Connection::open(&fx.db)
            .unwrap()
            .execute(
                "UPDATE proposals SET revise_count=2 WHERE id=?1",
                [id.as_i64()],
            )
            .unwrap();
    }
    let mut verdict = concern(json!("ready"), json!("high"), Value::Null);
    verdict["actions"] = json!([{"action": "add_dependency", "task_id": three, "depends_on": two}]);
    let reviewer = StubReviewer::new(&[
        verdict,
        concern(json!("send_back"), json!("high"), Value::Null),
        json!({"verdict": "revise", "reasons": ["still vague"], "summary": "vague"}),
    ]);
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
    assert_eq!(queue.show(two).unwrap().task.priority(), Priority::High);
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
    // Past the limit, a person decides, with the job's recommendation.
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 2, "{asks:?}");
    // The supervisor notifies nobody: the inbox's watch tells of each ask.
    let sent_back = asks.iter().find(|a| a.task_id == Some(limited)).unwrap();
    assert_eq!(sent_back.kind, AskKind::ApprovePlan);
    assert_eq!(sent_back.recommendation.as_deref(), Some("send_back"));
    assert_eq!(sent_back.confidence, Some(AskConfidence::High));
    assert!(
        sent_back
            .question
            .contains("It recommends send_back (confidence high); left to a person: revise_limit."),
        "{}",
        sent_back.question
    );
    assert_eq!(status(&mut queue, limited), TaskStatus::Submitted);
    let finished = &events(&mut queue, limited, "plan_review_finished")[0];
    assert_eq!(finished["decision"], "concern");
    assert_eq!(
        events(&mut queue, limited, "plan_concern_decided"),
        [json!({
            "proposal_id": at_limit[0], "plan_review_id": finished["plan_review_id"],
            "recommendation": "send_back", "confidence": "high", "reason_category": null,
            "applied": false, "decision": null, "escalated_because": "revise_limit",
            "ask_id": sent_back.id
        })]
    );
    let revised = asks.iter().find(|a| a.task_id == Some(stubborn)).unwrap();
    let overridden = format!(
        "proposal {} was sent back 2 times already (at most 2)",
        at_limit[1]
    );
    assert!(
        revised
            .question
            .contains(&format!("It answered revise, but {overridden}.")),
        "{}",
        revised.question
    );
    let finished = &events(&mut queue, stubborn, "plan_review_finished")[0];
    assert_eq!(
        (
            &finished["verdict"],
            &finished["decision"],
            &finished["overridden"]
        ),
        (&json!("revise"), &json!("concern"), &json!(overridden))
    );
    assert_eq!(status(&mut queue, stubborn), TaskStatus::Submitted);
    let stats = runtime::stats(&fx.db, &Default::default()).unwrap();
    assert_eq!(
        stats["recommendations"]["decided_without_ask"]["approve_plan"], 1,
        "{}",
        stats["recommendations"]
    );
}
