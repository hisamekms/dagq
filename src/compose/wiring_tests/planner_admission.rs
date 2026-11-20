//! Admission through the supervisor, service and planner stores.
use super::{agent::Script, harness::Wiring, host::Hook};
use crate::{
    application::{DraftPlannerStore, TaskStore, supervisor_service::ServiceCaller},
    domain::{
        AskKind, AskReason, DraftOrigin, FindingId, FindingStatus, FindingTarget, GoalId,
        LeaseToken, NewAsk, NewFinding, NewGoal, NewTask, PlannerOrigin, PlannerOwner, Priority,
        RequestId, Submission, TaskId, plan_request::NewPlanRequest, queue_service::UseCase,
    },
    infrastructure::sqlite::SqliteQueue,
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

fn fixture(name: &str) -> Wiring {
    Wiring::new(
        name,
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    )
}

fn plan(wiring: &Wiring, slots: usize) {
    wiring.supervise_planning(slots, Duration::from_secs(3600));
}

fn goal(queue: &mut SqliteQueue, priority: Priority) -> GoalId {
    queue
        .add_goal(NewGoal {
            title: format!("A {} goal", priority.as_str()),
            description: "Plan the change".into(),
            acceptance: "The change is planned".into(),
            priority: Some(priority),
            ..Default::default()
        })
        .unwrap()
        .id()
}

fn task(queue: &mut SqliteQueue, title: &str, goal_id: Option<GoalId>) -> TaskId {
    queue
        .add(NewTask {
            title: title.into(),
            description: "Implement a small change".into(),
            acceptance: "Its regression test passes".into(),
            verification_commands: vec!["cargo test".into()],
            dependencies: vec![],
            goal_dependencies: vec![],
            goal_id,
            context: String::new(),
            required_evidence: vec![],
            paths: vec![],
            priority: None,
            change: None,
            provider: None,
            worker_mode: None,
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap()
        .id()
}

fn draft(queue: &mut SqliteQueue) -> TaskId {
    let draft = task(queue, "Runtime draft", None);
    queue
        .record_draft_origin(draft, DraftOrigin::FollowUp, &json!({}))
        .unwrap();
    draft
}

fn request(queue: &mut SqliteQueue, priority: Priority) -> RequestId {
    queue
        .record_plan_request(
            &NewPlanRequest {
                text: "Plan a change".into(),
                note: None,
                refs: vec![],
                priority: Some(priority),
            },
            "inbox",
            "inbox",
        )
        .unwrap()
        .id
}

#[test]
fn an_interrupt_revise_gets_the_slot_before_a_normal_request_and_draft() {
    let wiring = fixture("planner-admission-priority");
    let mut queue = wiring.queue();
    let request = request(&mut queue, Priority::Normal);
    let draft = draft(&mut queue);
    let goal = goal(&mut queue, Priority::Interrupt);
    let child = task(
        &mut queue,
        "Revise the draft in the interrupt goal",
        Some(goal),
    );
    let proposal = queue
        .submit(Submission {
            tasks: vec![child],
            goals: vec![],
            proposal: None,
            owner: PlannerOwner {
                origin: PlannerOrigin::Runtime,
                workspace_id: None,
            },
        })
        .unwrap()
        .id();
    queue.send_back_proposal(proposal).unwrap();

    plan(&wiring, 1);
    let open = queue.planners(false).unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].proposal_id, Some(proposal));
    assert_eq!(queue.request_by_id(request).unwrap().planners, 0);
    assert_eq!(queue.planner_drafts().unwrap()[0].task.id(), draft);
}

#[test]
fn a_normal_request_then_a_normal_draft_use_successive_free_slots() {
    let wiring = fixture("planner-admission-next-slot")
        .with_continuous_passes()
        .with_tick(Duration::from_millis(2))
        .with_supervisor_file(Arc::new(|| {
            Ok(Some(crate::domain::slot_limits::SupervisorConfig {
                runtime_planners: Some(1),
                ..Default::default()
            }))
        }));
    let mut queue = wiring.queue();
    // The draft is older. The request still gets the first free slot.
    let draft = draft(&mut queue);
    let request = request(&mut queue, Priority::Normal);
    let mut observed = Vec::new();
    let mut last = None;
    let outcome = wiring.supervise_once_while_stoppable(|queue, stop| {
        let open = queue.planners(false).unwrap();
        let Some(slot) = open.first().filter(|slot| slot.workspace_id.is_some()) else {
            return false;
        };
        if last == Some(slot.id) {
            return false;
        }
        last = Some(slot.id);
        observed.push((
            open.len(),
            slot.request_id,
            queue.planner_draft_tasks(slot.id).unwrap(),
        ));
        if observed.len() == 1 && slot.request_id == Some(request) {
            queue
                .decline_request(request, "No change is needed", "planner", Some(slot.id))
                .unwrap();
            queue.close_planner(slot.id, None).unwrap();
            return false;
        }
        stop();
        true
    });
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(
        observed,
        [(1, Some(request), vec![]), (1, None, vec![draft])]
    );
}

fn marked_finding(
    queue: &mut SqliteQueue,
    kind: &str,
    goal: Option<GoalId>,
    by: &str,
) -> FindingId {
    queue
        .record_finding(NewFinding {
            kind: kind.into(),
            target: goal.map_or(FindingTarget::Queue, FindingTarget::Goal),
            subject: kind.into(),
            summary: "Investigate this finding".into(),
            detail: None,
            impact: None,
            evidence: vec![],
            propose: Some("Needs a plan".into()),
            by: by.into(),
        })
        .unwrap()
        .finding
        .id
}

#[test]
fn a_blocked_improvement_yields_to_an_answer_repair_and_watch_in_the_same_pass() {
    let wiring = fixture("planner-admission-improvement-limit");
    let mut queue = wiring.queue();
    assert!(queue.improvements(0).unwrap().reached());
    let urgent = goal(&mut queue, Priority::Interrupt);
    let improvement = marked_finding(&mut queue, "stalled", Some(urgent), "observer");
    // Put the watch before the CI repair in insertion order.
    let watch = marked_finding(
        &mut queue,
        crate::domain::agent_eval::watch::WATCH_FINDING_KIND,
        None,
        "supervisor",
    );
    let repair = marked_finding(
        &mut queue,
        crate::domain::ci_watch::FINDING_KIND,
        None,
        "supervisor",
    );
    let answering = marked_finding(&mut queue, "capacity", None, "observer");
    // Only an answer for a finding that had a runtime planner returns
    // to the runtime. Seed that attempt before the improvement limit binds.
    let crate::application::FindingPlannerStart::Opened { planner, .. } =
        queue.open_finding_planner(answering, None, 1).unwrap()
    else {
        panic!("expected the finding's first planner");
    };
    queue.close_planner(planner.id, None).unwrap();
    let ask = queue
        .ask(NewAsk {
            kind: AskKind::PlannerQuestion,
            task_id: None,
            run_id: None,
            question: "Should this be automated?".into(),
            options: vec!["yes".into(), "no".into()],
            asked_by: "planner".into(),
            reason_category: AskReason::Scope,
            topics: vec![],
            recommendation: Some("yes".into()),
            confidence: None,
            finding_id: Some(answering),
            request_id: None,
        })
        .unwrap()
        .ask
        .id;
    queue.answer(ask, "yes").unwrap();

    plan(&wiring, 3);
    let mut opened: Vec<_> = queue
        .planners(false)
        .unwrap()
        .into_iter()
        .map(|planner| (planner.id, planner.finding_id.unwrap()))
        .collect();
    opened.sort_unstable();
    assert_eq!(
        opened
            .into_iter()
            .map(|(_, finding)| finding)
            .collect::<Vec<_>>(),
        [answering, repair, watch]
    );
    assert!(queue.read_ask(ask).unwrap().closed_at.is_some());
    assert_eq!(queue.planner_findings().unwrap()[0].id, improvement);
}

#[derive(Debug, PartialEq)]
enum FindingAdmission {
    Collect,
    TryOpen(FindingId, Value),
}

#[derive(Default)]
struct AsFindingOpens {
    resolve: Mutex<Option<Hook>>,
    operations: Mutex<Vec<FindingAdmission>>,
}

struct Caller(Arc<dyn ServiceCaller>, Arc<AsFindingOpens>);

impl ServiceCaller for Caller {
    fn call(&self, case: UseCase, params: Value) -> Result<Value> {
        match params["op"].as_str() {
            Some("planner_findings") => {
                self.1
                    .operations
                    .lock()
                    .unwrap()
                    .push(FindingAdmission::Collect);
            }
            Some("open_finding_planner") => {
                let id = serde_json::from_value(params["args"][0].clone())?;
                if let Some(resolve) = self.1.resolve.lock().unwrap().take() {
                    resolve();
                }
                let result = self.0.call(case, params)?;
                self.1
                    .operations
                    .lock()
                    .unwrap()
                    .push(FindingAdmission::TryOpen(id, result.clone()));
                return Ok(result);
            }
            _ => {}
        }
        self.0.call(case, params)
    }

    fn with_token(&self, path: &Path) -> Result<Arc<dyn ServiceCaller>> {
        self.0.with_token(path)
    }

    fn sign_in(&self, token: &LeaseToken, pid: u32) -> Result<()> {
        self.0.sign_in(token, pid)
    }

    fn take_back(&self, token: &LeaseToken, pid: u32) -> Result<()> {
        self.0.take_back(token, pid)
    }

    fn hand_over(&self) {
        self.0.hand_over();
    }

    fn sign_out(&self) {
        self.0.sign_out();
    }

    fn credential(&self) -> Option<PathBuf> {
        self.0.credential()
    }
}

#[test]
fn a_finding_resolved_at_opening_yields_to_a_repair_in_the_same_pass_at_the_improvement_limit() {
    let at_open = Arc::new(AsFindingOpens::default());
    let hook = at_open.clone();
    let wiring = fixture("planner-admission-finding-moved")
        .with_service_wrap(Arc::new(move |inner| Arc::new(Caller(inner, hook.clone()))));
    let mut queue = wiring.queue();
    assert!(queue.improvements(0).unwrap().reached());
    assert!(queue.planners(false).unwrap().is_empty());

    let urgent = goal(&mut queue, Priority::Interrupt);
    // The watch leads the queue and can open at the improvement limit.
    let first = marked_finding(
        &mut queue,
        crate::domain::agent_eval::watch::WATCH_FINDING_KIND,
        Some(urgent),
        "supervisor",
    );
    let blocked = marked_finding(&mut queue, "stalled", Some(urgent), "observer");
    let next = marked_finding(
        &mut queue,
        crate::domain::ci_watch::FINDING_KIND,
        None,
        "supervisor",
    );

    let db = wiring.db.clone();
    let generators = wiring.generators();
    *at_open.resolve.lock().unwrap() = Some(Box::new(move || {
        SqliteQueue::open(&db)
            .unwrap()
            .with_generators(generators)
            .set_finding_status(
                first,
                FindingStatus::Resolved,
                "Resolved before opening",
                "supervisor",
            )
            .unwrap();
    }));

    plan(&wiring, 1);
    // All three outcomes must occur before another pass collects findings.
    let operations = at_open.operations.lock().unwrap();
    assert!(
        operations.windows(4).any(|ops| matches!(
            ops,
            [
                FindingAdmission::Collect,
                FindingAdmission::TryOpen(resolved, skipped),
                FindingAdmission::TryOpen(improvement, at_limit),
                FindingAdmission::TryOpen(repair, opened),
            ] if *resolved == first && *skipped == json!("Skipped")
                && *improvement == blocked && at_limit["AtLimit"]["limit"] == 0
                && *repair == next && opened.get("Opened").is_some()
        )),
        "{operations:?}"
    );
    assert_eq!(
        queue.finding_view(first).unwrap().finding.status,
        FindingStatus::Resolved
    );
    assert_eq!(queue.planner_findings().unwrap()[0].id, blocked);
    let open = queue.planners(false).unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].finding_id, Some(next));
}
