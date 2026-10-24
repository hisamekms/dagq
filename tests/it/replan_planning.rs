//! The planning of a replanning operation (ADR-t1521-2 decisions 3 and 4)
//! through the supervisor loop: the request a person's `replan request`
//! recorded gets one planner of the runtime's once the run is parked,
//! within the limit it shares with the other requests; the planner
//! submits child tasks with a replacement plan, which submit refuses when
//! it leaves an acceptance item unmapped, weakens a verification, closes a
//! loop or is stale, and plan review checks again; a pass leaves the
//! children `submitted`, so no claim takes them before the replacement is
//! applied; a revise, a concern, a withdrawn proposal and a withdrawn
//! operation keep the run parked with its saved work; and a planner's
//! decline with `--continue-run` continues the run. The backend and the
//! stub reviewer are plan review's (`runtime_support::plan_review`).

use crate::common::queue::{base, new_task};
use crate::runtime_support::plan_review::{
    Fixture, PlanWorkspace, StubReviewer, completed_spike, fixture, open_goal, options,
    planner_prompt,
};
use anyhow::Result;
use dagq::{
    application::{TaskReads, TaskStore},
    domain::{
        AskConfidence, AskId, AskKind, AskReason, ClaimOutcome, GoalEdit, GoalId, NewAsk, NewTask,
        PlannerId, PlannerOrigin, PlannerOwner, Proposal, ProposalStatus, ReplanId, RequestId,
        RunId, RunStatus, Submission, TaskAction, TaskEdit, TaskId, TaskStatus,
        plan_request::NewPlanRequest,
        replan_plan::{ReplacementPlan, deletion_token},
        task_replan::{AcceptanceItem, NewReplan, ReplanMove, ReplanOrigin, ReplanStatus},
    },
    infrastructure::sqlite::SqliteQueue,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::time::Duration;

/// Task A of a goal (two acceptance items, `cargo test`, paths) whose run
/// is parked for a person's replanning request, and task B that waits for
/// it.
struct Parked {
    fx: Fixture,
    goal: GoalId,
    task: TaskId,
    successor: TaskId,
    run: RunId,
    op: ReplanId,
    request: RequestId,
}

fn task_in(goal: GoalId, title: &str, acceptance: &str, deps: &[TaskId]) -> NewTask {
    NewTask {
        acceptance: acceptance.into(),
        goal_id: Some(goal),
        paths: vec!["src/**".into()],
        verification_commands: vec!["cargo test".into()],
        dependencies: deps.to_vec(),
        worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
        ..new_task(title)
    }
}

/// A parked operation, with `before` planning requests recorded ahead of
/// its own.
fn parked(before: usize) -> Parked {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let task = queue
        .add(task_in(
            goal,
            "A",
            "the API behaves\nthe operations are documented",
            &[],
        ))
        .unwrap()
        .id();
    let successor = queue
        .add(task_in(goal, "B", "B uses the API", &[task]))
        .unwrap()
        .id();
    for id in [task, successor] {
        queue.transition(id, TaskAction::BypassReview).unwrap();
    }
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!("claim")
    };
    assert_eq!(run.task_id(), task);
    let run = run.id().clone();
    for n in 0..before {
        queue
            .record_plan_request(
                &NewPlanRequest {
                    text: format!("plan something else {n}"),
                    note: None,
                    refs: Vec::new(),
                    priority: None,
                },
                "inbox",
                "inbox",
            )
            .unwrap();
    }
    let (op, request) = park(&mut queue, &fx.db, &run);
    Parked {
        fx,
        goal,
        task,
        successor,
        run,
        op,
        request,
    }
}

/// Record a person's replanning request for the claimed `run` and park
/// it as the runtime does: the operation `parked` on snapshot `snap-1`,
/// the run `needs_session` with its lease given back.
fn park(queue: &mut SqliteQueue, db: &std::path::Path, run: &RunId) -> (ReplanId, RequestId) {
    let recorded = queue
        .record_replan(
            &NewReplan {
                run: run.clone(),
                reason: "the review keeps finding two unrelated parts".into(),
                evidence: Vec::new(),
            },
            ReplanOrigin::Manual,
            "inbox",
            "inbox",
        )
        .unwrap();
    let request = recorded.request.unwrap().id;
    let mut op = recorded.operation;
    for (to, snapshot) in [
        (ReplanStatus::PauseRequested, None),
        (ReplanStatus::Snapshotting, None),
        (ReplanStatus::Parked, Some("snap-1".to_owned())),
    ] {
        op = queue
            .move_replan(
                op.id,
                op.generation,
                &ReplanMove {
                    to,
                    reason: format!("the test moves it to {}", to.as_str()),
                    snapshot_id: snapshot,
                    diagnosis: None,
                },
            )
            .unwrap();
    }
    // As the park leaves the run: needs_session, its lease given back.
    let connection = Connection::open(db).unwrap();
    connection
        .execute(
            "UPDATE task_runs SET status='needs_session' WHERE id=?1",
            [run.as_str()],
        )
        .unwrap();
    connection
        .execute("DELETE FROM run_leases WHERE run_id=?1", [run.as_str()])
        .unwrap();
    (op.id, request)
}

/// Another task of `p`'s goal, claimed and parked like `p`'s, for a
/// request of its own.
fn park_another(p: &Parked, title: &str) -> (TaskId, RequestId) {
    let mut queue = queue(p);
    let task = queue
        .add(task_in(p.goal, title, &format!("{title} holds"), &[]))
        .unwrap()
        .id();
    queue.transition(task, TaskAction::BypassReview).unwrap();
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!("claim")
    };
    assert_eq!(run.task_id(), task);
    let (_, request) = park(&mut queue, &p.fx.db, run.id());
    (task, request)
}

/// One supervise with `planners` planners of the runtime's, given the
/// `dagq` the test owns, so a service it starts stops with the test.
fn supervise(p: &Parked, backend: &PlanWorkspace, reviewer: &StubReviewer, planners: usize) {
    supervise_timed(p, backend, reviewer, planners, Duration::from_secs(3600));
}

/// [`supervise`] with the planner timeout `timeout`.
fn supervise_timed(
    p: &Parked,
    backend: &PlanWorkspace,
    reviewer: &StubReviewer,
    planners: usize,
    timeout: Duration,
) {
    let _waiting = crate::common::within(crate::common::STEP_LIMIT, "supervise to return");
    let outcome = dagq::runtime::supervise_with_reviewer(
        &p.fx.db,
        &p.fx.repo,
        backend,
        &p.fx.claude,
        reviewer,
        &crate::common::service::owned_executable(p.fx.db.parent().unwrap()),
        &options(planners, timeout),
    )
    .unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
}

fn queue(p: &Parked) -> SqliteQueue {
    SqliteQueue::open(&p.fx.db).unwrap()
}

/// The planner of the runtime's open for `request` and its background
/// handle.
fn request_planner(p: &Parked, request: RequestId) -> Option<(PlannerId, String)> {
    queue(p)
        .planners(false)
        .unwrap()
        .into_iter()
        .find(|planner| planner.request_id == Some(request))
        .map(|planner| (planner.id, planner.workspace_id.unwrap()))
}

/// End `planner` as a session that exited without deciding anything.
fn exit(p: &Parked, planner: PlannerId) {
    let queue = queue(p);
    queue.register_planner_wrapper(planner, 1).unwrap();
    queue.register_planner_agent(planner, 1, 1).unwrap();
    queue.planner_exited(planner, 1, 0).unwrap();
}

/// A child of the goal holding one acceptance item, `cargo test` and paths.
fn child(p: &Parked, title: &str, deps: &[TaskId]) -> TaskId {
    let mut queue = queue(p);
    queue
        .add(task_in(p.goal, title, &format!("{title} holds"), deps))
        .unwrap()
        .id()
}

/// The plan that splits the task's two items into `children` (one each,
/// the first taking the run's commits), with the successor waiting for
/// both.
fn plan(p: &Parked, children: [TaskId; 2]) -> Value {
    let op = queue(p).replan_operation(p.op).unwrap();
    let item = |ordinal: usize, child: TaskId| {
        json!({"ordinal": ordinal, "hash": op.conditions.acceptance[ordinal - 1].hash,
               "disposition": "split", "children": [{"task_id": child, "ordinal": 1}]})
    };
    json!({
        "operation_id": op.id,
        "task_version": op.task_version,
        "goal_acceptance_version": op.goal_acceptance_version,
        "choice": "split",
        "rationale": "the API and the documents are independent and each review mixed them",
        "children": [
            {"task_id": children[0], "carry": "snapshot_head", "order": 1},
            {"task_id": children[1], "carry": "base"},
        ],
        "mappings": [item(1, children[0]), item(2, children[1])],
        "successors": [{"task_id": p.successor, "needs": children}],
    })
}

/// A scope `planner_question` about the operation's request offering to
/// delete `item` or keep it, as the planner asks a person.
fn scope_ask(p: &Parked, item: &AcceptanceItem) -> AskId {
    queue(p)
        .ask(NewAsk {
            recommendation: Some("keep".into()),
            confidence: Some(AskConfidence::Low),
            topics: Vec::new(),
            kind: AskKind::PlannerQuestion,
            task_id: None,
            run_id: None,
            question: format!("drop acceptance item {}?", item.ordinal),
            options: vec![deletion_token(item), "keep".into()],
            asked_by: "planner".into(),
            reason_category: AskReason::Scope,
            finding_id: None,
            request_id: Some(p.request),
        })
        .unwrap()
        .ask
        .id
}

/// Submit `tasks` (or proposal `again` once sent back) from the planner's
/// handle `handle` with `plan`.
fn submit(
    p: &Parked,
    handle: &str,
    tasks: &[TaskId],
    again: Option<dagq::domain::ProposalId>,
    plan: Option<&Value>,
) -> Result<Proposal> {
    let plan = plan
        .map(|plan| ReplacementPlan::parse(&plan.to_string()))
        .transpose()?;
    queue(p).with_replacement_plan(plan).submit_linking(
        Submission {
            tasks: tasks.to_vec(),
            goals: Vec::new(),
            proposal: again,
            owner: PlannerOwner {
                origin: PlannerOrigin::Runtime,
                workspace_id: Some(handle.to_owned()),
            },
        },
        &[],
    )
}

/// Make draft `task`'s only verification `command`.
fn verify(p: &Parked, task: TaskId, command: &str) {
    queue(p)
        .edit_task(
            task,
            TaskEdit {
                verification_commands: Some(vec![command.into()]),
                ..TaskEdit::default()
            },
            TaskStatus::Draft,
        )
        .unwrap();
}

fn operation(p: &Parked) -> dagq::domain::task_replan::ReplanOperation {
    queue(p).replan_operation(p.op).unwrap()
}

fn status(p: &Parked, task: TaskId) -> TaskStatus {
    queue(p).show(task).unwrap().task.status()
}

/// The payloads of `kind` on task `task`.
fn events(p: &Parked, task: TaskId, kind: &str) -> Vec<Value> {
    queue(p)
        .show(task)
        .unwrap()
        .events
        .into_iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| event.payload)
        .collect()
}

/// The parked run and its saved work are as the park left them: the task
/// in progress and guarded, the run needs_session with no new run, the
/// snapshot still named.
fn assert_kept(p: &Parked) {
    let mut queue = queue(p);
    let detail = queue.show(p.task).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert_eq!(detail.runs.len(), 1);
    assert_eq!(detail.runs[0].id(), &p.run);
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    let op = queue.replan_operation(p.op).unwrap();
    assert_eq!(op.snapshot_id.as_deref(), Some("snap-1"));
    assert!(queue.replan_guard(p.task).unwrap().is_some(), "{op:?}");
}

#[test]
fn a_parked_operations_request_waits_for_a_planner_slot_and_gets_one_planner_with_its_material() {
    let p = parked(1);
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    // The one slot goes to the older request: the operation waits parked.
    supervise(&p, &backend, &reviewer, 1);
    assert!(request_planner(&p, p.request).is_none());
    assert_eq!(operation(&p).status, ReplanStatus::Parked);
    let older = queue(&p).planners(false).unwrap()[0].id;
    exit(&p, older);
    // The parked run's reviews: one that passed on its own verdict but
    // that its agents sent back, and one that landed.
    let connection = Connection::open(&p.fx.db).unwrap();
    for review in [
        json!({"verdict": "pass", "reasons": [], "summary": "fine", "attempt": 1,
            "agents": [
                {"agent": "test-rules", "status": "completed", "verdict": "revise",
                    "reasons": ["add a unit test"], "summary": "s"},
                {"agent": "design", "status": "completed", "verdict": "pass",
                    "reasons": [], "summary": "ok"},
            ],
            "route": {"destination": "send_back", "parent": "land"}}),
        json!({"verdict": "pass", "reasons": ["a landed note"], "summary": "ok", "attempt": 2,
            "agents": [], "route": {"destination": "land", "parent": "land"}}),
    ] {
        connection
            .execute(
                "INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (?1,?2,'review_finished',?3)",
                rusqlite::params![p.task, p.run.as_str(), review.to_string()],
            )
            .unwrap();
    }
    // Once it ends, the operation's request gets one planner, and the
    // operation is with it.
    supervise(&p, &backend, &reviewer, 1);
    let (planner, _) = request_planner(&p, p.request).expect("a planner");
    assert_eq!(operation(&p).status, ReplanStatus::Planning);
    // Never a second: another pass and a higher limit leave the one.
    supervise(&p, &backend, &reviewer, 3);
    let planners: Vec<_> = queue(&p)
        .planners(true)
        .unwrap()
        .into_iter()
        .filter(|row| row.request_id == Some(p.request))
        .collect();
    assert_eq!(planners.len(), 1, "{planners:?}");
    let prompt = planner_prompt(&p.fx.db, planner);
    let op = operation(&p);
    for expected in [
        format!("## Replan operation {} of task {}", op.id, p.task),
        op.task_version.clone(),
        format!("- 1 `{}` the API behaves", op.conditions.acceptance[0].hash),
        format!(
            "- 2 `{}` the operations are documented",
            op.conditions.acceptance[1].hash
        ),
        "snap-1".to_owned(),
        format!("- task {} (ready): B", p.successor),
        "--replacement <plan.json>".to_owned(),
        format!("dagq request decline {} --continue-run", p.request),
        "--because scope".to_owned(),
        "- revise: test-rules: add a unit test\n".to_owned(),
    ] {
        assert!(prompt.contains(&expected), "{expected}\n{prompt}");
    }
    assert!(!prompt.contains("a landed note"), "{prompt}");
    assert_kept(&p);
}

#[test]
fn a_replacement_by_two_children_passes_and_leaves_them_unclaimed_until_it_is_applied() {
    let p = parked(0);
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    supervise(&p, &backend, &reviewer, 1);
    let (_, handle) = request_planner(&p, p.request).unwrap();
    let api = child(&p, "API", &[]);
    let docs = child(&p, "docs", &[api]);
    let proposal = submit(
        &p,
        &handle,
        &[api, docs],
        None,
        Some(&plan(&p, [api, docs])),
    )
    .unwrap();
    let op = operation(&p);
    assert_eq!(op.status, ReplanStatus::Reviewing);
    assert_eq!(op.proposal_id, Some(proposal.id()));
    let submitted = events(&p, p.task, "replan_plan_submitted");
    assert_eq!(submitted.len(), 1, "{submitted:?}");
    assert_eq!(submitted[0]["children"], json!([api, docs]));
    // Before the pass, the children are submitted and nothing readies them.
    for id in [api, docs] {
        assert_eq!(status(&p, id), TaskStatus::Submitted);
        let error = queue(&p)
            .transition(id, TaskAction::BypassReview)
            .unwrap_err()
            .to_string();
        assert!(error.contains("applies the replacement"), "{error}");
    }
    // Plan review passes: the proposal is accepted, its plan approved, and
    // the children stay submitted; further passes with free slots claim
    // none of them.
    supervise(&p, &backend, &reviewer, 1);
    supervise(&p, &backend, &reviewer, 1);
    let mut read = queue(&p);
    assert_eq!(
        read.show_proposal(proposal.id()).unwrap().status(),
        ProposalStatus::Accepted
    );
    let approved = events(&p, p.task, "replan_plan_approved");
    assert_eq!(approved.len(), 1, "{approved:?}");
    // The review was given the plan, the conditions it maps and the saved
    // work, with what to check of its meaning.
    let prompts = reviewer.prompts();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    let review = &prompts[0];
    for expected in [
        format!("replaces task {} of replan operation {}", p.task, p.op),
        format!("- 1 `{}` the API behaves", op.conditions.acceptance[0].hash),
        "snap-1".to_owned(),
        "\"carry\": \"snapshot_head\"".to_owned(),
        format!("successor: task {}", p.successor),
        "Check its meaning".to_owned(),
    ] {
        assert!(review.contains(&expected), "{expected}\n{review}");
    }
    // `replans ID` reads the plan and its problems now.
    let listed = crate::common::cli::ok(&p.fx.db, &["replans", &p.op.to_string()]);
    let plan = &listed["replans"][0]["plan"];
    assert_eq!(plan["revision"], 1, "{listed}");
    assert_eq!(plan["proposal_id"], proposal.id().as_i64(), "{listed}");
    assert_eq!(plan["plan"]["children"][0]["task_id"], api.as_i64());
    assert_eq!(listed["replans"][0]["plan_problems"], json!([]));
    for id in [api, docs] {
        let detail = read.show(id).unwrap();
        assert_eq!(detail.task.status(), TaskStatus::Submitted);
        assert!(detail.runs.is_empty(), "{:?}", detail.runs);
        assert!(queue(&p).transition(id, TaskAction::BypassReview).is_err());
    }
    assert_eq!(status(&p, p.successor), TaskStatus::Ready);
    assert_eq!(operation(&p).status, ReplanStatus::Reviewing);
    assert_kept(&p);
}

/// A pass whose action cancels a child as a duplicate would leave the
/// plan giving an item, the saved work and a successor's wait to a
/// canceled task: it is judged on the plan as its actions leave it, and
/// none of its actions applies. With the proposal's revises used up, the
/// supervisor's refusal makes it a concern a person answers, as a revise
/// is then. Which count is past the limit, and the revise within it, are
/// `refuse_replacement`'s (unit test).
#[test]
fn a_pass_that_cancels_a_child_as_a_duplicate_is_held_for_a_person_past_the_revise_limit() {
    let p = parked(0);
    let backend = PlanWorkspace::default();
    supervise(&p, &backend, &StubReviewer::new(&[]), 1);
    let (_, handle) = request_planner(&p, p.request).unwrap();
    let api = child(&p, "API", &[]);
    let docs = child(&p, "docs", &[]);
    let cancel = json!({"verdict": "pass", "reasons": [], "summary": "ok",
        "actions": [{"action": "cancel_duplicate", "task_id": docs, "duplicate_of": api}]});
    let reviewer = StubReviewer::new(&[cancel]);
    let proposal = submit(
        &p,
        &handle,
        &[api, docs],
        None,
        Some(&plan(&p, [api, docs])),
    )
    .unwrap()
    .id();
    // Counted as a second send-back would.
    Connection::open(&p.fx.db)
        .unwrap()
        .execute(
            "UPDATE proposals SET revise_count=2 WHERE id=?1",
            [proposal.as_i64()],
        )
        .unwrap();
    supervise(&p, &backend, &reviewer, 1);
    let finished = events(&p, api, "plan_review_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    let held = &finished[0];
    assert_eq!(held["decision"], "concern", "{held}");
    assert!(
        held["overridden"]
            .as_str()
            .unwrap()
            .contains("was sent back 2 times already"),
        "{held}"
    );
    let problems = held["replacement_problems"].to_string();
    for expected in [
        "the pass's actions would leave it".to_owned(),
        format!("child_not_pending: child task {docs} is canceled"),
    ] {
        assert!(problems.contains(&expected), "{expected}\n{problems}");
    }
    let ask = queue(&p)
        .read_ask(AskId::new(held["ask_id"].as_i64().unwrap()))
        .unwrap();
    assert_eq!(ask.kind, AskKind::ApprovePlan);
    assert!(
        ask.question.contains("child_not_pending"),
        "{}",
        ask.question
    );
    let read = queue(&p).show_proposal(proposal).unwrap();
    assert_eq!(read.status(), ProposalStatus::Submitted);
    assert_eq!(read.revise_count(), 2);
    for id in [api, docs] {
        assert_eq!(status(&p, id), TaskStatus::Submitted);
    }
    assert!(events(&p, p.task, "replan_plan_approved").is_empty());
    assert_eq!(operation(&p).status, ReplanStatus::AwaitingAnswer);
    assert_kept(&p);
}

#[test]
fn submit_refuses_a_plan_that_drops_a_condition_weakens_a_check_loops_or_is_stale() {
    let p = parked(0);
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    supervise(&p, &backend, &reviewer, 1);
    let (_, handle) = request_planner(&p, p.request).unwrap();
    let api = child(&p, "API", &[]);
    let docs = child(&p, "docs", &[]);
    let refused = |plan: Option<&Value>, code: &str| {
        let error = format!(
            "{:#}",
            submit(&p, &handle, &[api, docs], None, plan).unwrap_err()
        );
        assert!(error.contains(code), "{code}: {error}");
        for id in [api, docs] {
            assert_eq!(status(&p, id), TaskStatus::Draft, "{code}");
        }
        assert_eq!(operation(&p).status, ReplanStatus::Planning, "{code}");
    };
    // No plan at all.
    refused(None, "--replacement");
    // An acceptance item mapped to no child.
    let mut unmapped = plan(&p, [api, docs]);
    unmapped["mappings"].as_array_mut().unwrap().pop();
    refused(Some(&unmapped), "item_unmapped");
    // A deletion without a person's scope answer.
    let mut deleted = plan(&p, [api, docs]);
    deleted["mappings"][1] = json!({"ordinal": 2, "hash": deleted["mappings"][1]["hash"],
                                    "disposition": "deleted"});
    deleted["mappings"][0]["children"]
        .as_array_mut()
        .unwrap()
        .push(json!({"task_id": docs, "ordinal": 1}));
    refused(Some(&deleted), "deletion_unauthorized");
    // A child whose verification no longer runs the task's.
    verify(&p, docs, "true");
    refused(Some(&plan(&p, [api, docs])), "verification_weakened");
    verify(&p, docs, "cargo test");
    // A child that waits for the task's successor: a loop once the
    // successor waits for it.
    queue(&p).add_dependency(docs, p.successor).unwrap();
    refused(Some(&plan(&p, [api, docs])), "dependency_cycle");
    queue(&p).remove_dependency(docs, p.successor).unwrap();
    // A deletion of item 2 whose scope ask a person answered `keep` is not
    // authorized: the queue reads the answer for submit's check (which
    // answers approve a deletion is the unit test's).
    let items = operation(&p).conditions.acceptance;
    let ask = scope_ask(&p, &items[1]);
    queue(&p).answer(ask, "keep").unwrap();
    let mut deleting = plan(&p, [api, docs]);
    deleting["mappings"][1] = json!({"ordinal": 2, "hash": items[1].hash,
                                     "disposition": "deleted", "authority_ask_id": ask});
    deleting["mappings"][0]["children"]
        .as_array_mut()
        .unwrap()
        .push(json!({"task_id": docs, "ordinal": 1}));
    refused(Some(&deleting), "deletion_unauthorized");
    // A plan made on conditions that changed since: the task's version,
    // or the goal's acceptance.
    let mut stale = plan(&p, [api, docs]);
    stale["task_version"] = json!("an older version");
    refused(Some(&stale), "version_stale");
    let fresh = plan(&p, [api, docs]);
    queue(&p)
        .edit_goal(
            p.goal,
            GoalEdit {
                acceptance: Some("every draft is decided, and documented".into()),
                ..GoalEdit::default()
            },
        )
        .unwrap();
    refused(Some(&fresh), "version_stale");
    // The request is still the planner's to decide, and the run stays
    // parked.
    assert!(events(&p, p.task, "replan_plan_submitted").is_empty());
    assert_kept(&p);
}

#[test]
fn a_plan_stale_by_its_review_is_sent_back_and_revise_concern_and_withdrawals_keep_the_saved_work()
{
    let p = parked(0);
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    supervise(&p, &backend, &reviewer, 1);
    let (planner, handle) = request_planner(&p, p.request).unwrap();
    let api = child(&p, "API", &[]);
    let docs = child(&p, "docs", &[]);
    let proposal = submit(
        &p,
        &handle,
        &[api, docs],
        None,
        Some(&plan(&p, [api, docs])),
    )
    .unwrap();
    // The goal's acceptance changes before the review: its pass is turned
    // into a revise naming why, and the children go back to drafts.
    queue(&p)
        .edit_goal(
            p.goal,
            GoalEdit {
                acceptance: Some("every draft is decided, and documented".into()),
                ..GoalEdit::default()
            },
        )
        .unwrap();
    supervise(&p, &backend, &reviewer, 1);
    let finished = events(&p, api, "plan_review_finished");
    assert_eq!(finished[0]["decision"], "revise", "{finished:?}");
    let problems = finished[0]["replacement_problems"].to_string();
    assert!(problems.contains("version_stale"), "{problems}");
    for id in [api, docs] {
        assert_eq!(status(&p, id), TaskStatus::Draft);
    }
    assert_eq!(operation(&p).status, ReplanStatus::Reviewing);
    assert!(events(&p, p.task, "replan_plan_approved").is_empty());
    assert_kept(&p);
    // Submitted again after the revise with a child's verification
    // weakened, it is no retry of a proposal held for a person: without a
    // plan submit asks for one, and it checks the one it gets.
    verify(&p, docs, "true");
    let error = format!(
        "{:#}",
        submit(&p, &handle, &[], Some(proposal.id()), None).unwrap_err()
    );
    assert!(error.contains("--replacement"), "{error}");
    let error = format!(
        "{:#}",
        submit(
            &p,
            &handle,
            &[],
            Some(proposal.id()),
            Some(&plan(&p, [api, docs]))
        )
        .unwrap_err()
    );
    assert!(error.contains("verification_weakened"), "{error}");
    verify(&p, docs, "cargo test");
    for id in [api, docs] {
        assert_eq!(status(&p, id), TaskStatus::Draft);
    }
    // The planner cannot change the goal's acceptance back for its plan:
    // the operation fixed the version, which only rises.
    queue(&p)
        .edit_goal(
            p.goal,
            GoalEdit {
                acceptance: Some("every draft is decided".into()),
                ..GoalEdit::default()
            },
        )
        .unwrap();
    let restored = queue(&p).replan_operation(p.op).unwrap();
    let mut again = plan(&p, [api, docs]);
    again["goal_acceptance_version"] = json!(restored.goal_acceptance_version);
    let error = format!(
        "{:#}",
        submit(&p, &handle, &[], Some(proposal.id()), Some(&again)).unwrap_err()
    );
    assert!(error.contains("version_stale"), "{error}");
    assert_kept(&p);
    // The planner withdraws the proposal: the operation goes back to it
    // with the run parked.
    queue(&p).withdraw_proposal(proposal.id()).unwrap();
    assert_eq!(operation(&p).status, ReplanStatus::Planning);
    assert_kept(&p);
    // It ends without another: once its row closes, the operation waits
    // blocked for a person, the run still parked.
    exit(&p, planner);
    supervise(&p, &backend, &reviewer, 1);
    supervise(&p, &backend, &reviewer, 1);
    let op = operation(&p);
    assert_eq!(op.status, ReplanStatus::Blocked, "{op:?}");
    assert!(request_planner(&p, p.request).is_none());
    assert_kept(&p);
    // The runtime withdraws the operation: the children are left drafts,
    // the snapshot stays named, and the run is the task's to continue.
    let op = operation(&p);
    queue(&p)
        .move_replan(
            op.id,
            op.generation,
            &ReplanMove {
                to: ReplanStatus::Withdrawn,
                reason: "the test withdraws it".into(),
                snapshot_id: None,
                diagnosis: None,
            },
        )
        .unwrap();
    let op = operation(&p);
    assert_eq!(op.snapshot_id.as_deref(), Some("snap-1"));
    for id in [api, docs] {
        assert_eq!(status(&p, id), TaskStatus::Draft);
    }
    assert_eq!(status(&p, p.task), TaskStatus::InProgress);
}

#[test]
fn a_concern_holds_the_operation_for_a_person_and_a_stale_ready_answer_sends_it_back() {
    let p = parked(0);
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "concern", "reasons": ["needs a person"], "summary": "doubtful"
    })]);
    supervise(&p, &backend, &reviewer, 1);
    let (_, handle) = request_planner(&p, p.request).unwrap();
    let api = child(&p, "API", &[]);
    let docs = child(&p, "docs", &[]);
    let proposal = submit(
        &p,
        &handle,
        &[api, docs],
        None,
        Some(&plan(&p, [api, docs])),
    )
    .unwrap();
    supervise(&p, &backend, &reviewer, 1);
    assert_eq!(operation(&p).status, ReplanStatus::AwaitingAnswer);
    for id in [api, docs] {
        assert_eq!(status(&p, id), TaskStatus::Submitted);
        assert!(queue(&p).show(id).unwrap().runs.is_empty());
    }
    assert_kept(&p);
    // The goal's acceptance changes before the person answers ready: the
    // answer is applied as a send back naming why, the ask closes, and the
    // planner gets the problems to revise.
    queue(&p)
        .edit_goal(
            p.goal,
            GoalEdit {
                acceptance: Some("every draft is decided, and documented".into()),
                ..GoalEdit::default()
            },
        )
        .unwrap();
    let ask = queue(&p)
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.kind == AskKind::ApprovePlan)
        .expect("an approve_plan ask");
    queue(&p).answer(ask.id, "ready").unwrap();
    supervise(&p, &backend, &reviewer, 1);
    assert!(queue(&p).read_ask(ask.id).unwrap().closed_at.is_some());
    assert_eq!(
        queue(&p).show_proposal(proposal.id()).unwrap().status(),
        ProposalStatus::Revising
    );
    let reasons: String = Connection::open(&p.fx.db)
        .unwrap()
        .query_row(
            "SELECT revise_reasons FROM proposals WHERE id=?1",
            [proposal.id().as_i64()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(reasons.contains("version_stale"), "{reasons}");
    for id in [api, docs] {
        assert_eq!(status(&p, id), TaskStatus::Draft);
    }
    assert_eq!(operation(&p).status, ReplanStatus::Reviewing);
    assert!(events(&p, p.task, "replan_plan_approved").is_empty());
    assert_kept(&p);
}

/// The planners that replan one goal open one at a time (ADR-t1521-2
/// decision 3, with ADR-t1487-1 decision 5): a second replanning
/// operation's request and a Spike's request of the same goal wait while
/// a planner of another is open, whether a pass would open theirs or a
/// person's answer would, though slots are free.
#[test]
fn a_goals_replanning_requests_get_one_planner_at_a_time_also_for_an_answer() {
    let p = parked(0);
    let (_, second) = park_another(&p, "C");
    let spike = child(&p, "S", &[]);
    completed_spike(&p.fx.db, spike, Some(p.goal), "holds");
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let spike_request = || {
        Connection::open(&p.fx.db)
            .unwrap()
            .query_row(
                "SELECT id FROM plan_requests WHERE spike_task_id=?1",
                [spike.as_i64()],
                |r| r.get::<_, i64>(0),
            )
            .map(RequestId::new)
            .unwrap()
    };
    let open = |request: RequestId| request_planner(&p, request).is_some();
    // Three slots: the oldest request of the goal gets the one planner.
    supervise(&p, &backend, &reviewer, 3);
    let (first, _) = request_planner(&p, p.request).expect("the oldest request's planner");
    assert!(!open(second));
    assert!(!open(spike_request()));
    // Its planner asks a person and its row closes: the goal goes to the
    // next request, the second operation's.
    let ask = scope_ask(&p, &operation(&p).conditions.acceptance[1]);
    queue(&p).close_planner(first, None).unwrap();
    supervise(&p, &backend, &reviewer, 3);
    let (next, _) = request_planner(&p, second).expect("the second operation's planner");
    assert!(!open(p.request));
    assert!(!open(spike_request()));
    // The answer would open a new planner of the first request: it waits
    // for the goal too, and stays undelivered.
    queue(&p).answer(ask, "keep").unwrap();
    supervise(&p, &backend, &reviewer, 3);
    assert!(!open(p.request));
    assert!(!open(spike_request()));
    // Once that planner declines its request and its row closes, the
    // answer opens the first request's planner, and the Spike's still
    // waits.
    queue(&p)
        .decline_request_choosing(second, "no split is needed", "planner", Some(next), false)
        .unwrap();
    queue(&p).close_planner(next, None).unwrap();
    supervise(&p, &backend, &reviewer, 3);
    let opened: Vec<Value> = Connection::open(&p.fx.db)
        .unwrap()
        .prepare(
            "SELECT payload FROM run_events WHERE kind='request_planner_opened'
             AND json_extract(payload,'$.request_id')=?1 ORDER BY id",
        )
        .unwrap()
        .query_map([p.request.as_i64()], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
        .collect();
    assert_eq!(opened.len(), 2, "{opened:?}");
    assert_eq!(opened[1]["ask_id"], ask.as_i64(), "{opened:?}");
    assert!(open(p.request));
    assert!(!open(spike_request()));
    assert_kept(&p);
}

/// The planner open for `proposal` alone, as a revise opens one once the
/// planner that submitted it ended.
fn revise_planner(p: &Parked, proposal: dagq::domain::ProposalId) -> Option<PlannerId> {
    queue(p)
        .planners(false)
        .unwrap()
        .into_iter()
        .find(|planner| planner.proposal_id == Some(proposal) && planner.request_id.is_none())
        .map(|planner| planner.id)
}

/// A revise whose request's planner ended goes to a planner opened for its
/// proposal alone, which replans the goal as the request's own did: a
/// revise waits for another request of the goal's planner, not told of as
/// unanswered, and once it ends gets its planner first, for which that
/// other request waits (ADR-t1521-2 decision 3). That the wait for the
/// goal frees no place at the limit either is `planner_wait_start`'s
/// (unit test).
#[test]
fn a_revise_after_its_planner_ended_and_another_request_of_the_goal_take_turns() {
    let p = parked(0);
    let (_, second) = park_another(&p, "C");
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[
        json!({"verdict": "revise", "reasons": ["say why the docs wait"], "summary": "s"}),
    ]);
    supervise(&p, &backend, &reviewer, 3);
    let (first, handle) = request_planner(&p, p.request).unwrap();
    assert!(request_planner(&p, second).is_none());
    let api = child(&p, "API", &[]);
    let docs = child(&p, "docs", &[]);
    let proposal = submit(
        &p,
        &handle,
        &[api, docs],
        None,
        Some(&plan(&p, [api, docs])),
    )
    .unwrap()
    .id();
    // Its planner ends after the submit: the other request's planner opens
    // while plan review sends the proposal back, and the revise waits for
    // it, past the planner timeout and at the limit.
    queue(&p).close_planner(first, None).unwrap();
    supervise(&p, &backend, &reviewer, 3);
    let (other, _) = request_planner(&p, second).expect("a planner for the other request");
    assert_eq!(
        queue(&p).show_proposal(proposal).unwrap().status(),
        ProposalStatus::Revising
    );
    assert!(revise_planner(&p, proposal).is_none());
    let connection = Connection::open(&p.fx.db).unwrap();
    connection
        .execute(
            "UPDATE proposals SET revised_at=revised_at-7200 WHERE id=?1",
            [proposal.as_i64()],
        )
        .unwrap();
    supervise_timed(&p, &backend, &reviewer, 1, Duration::from_secs(60));
    let told = || -> i64 {
        connection
            .query_row(
                "SELECT COUNT(*) FROM run_events
                 WHERE kind IN ('planner_unresponsive','planner_released')",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(told(), 0);
    assert_eq!(request_planner(&p, second).map(|(id, _)| id), Some(other));
    assert!(revise_planner(&p, proposal).is_none());
    // Once it ends, the revise gets its planner, untold though it was
    // recorded longer ago than the timeout, and the other request waits
    // for that one.
    queue(&p).close_planner(other, None).unwrap();
    supervise(&p, &backend, &reviewer, 3);
    assert_eq!(told(), 0);
    assert!(revise_planner(&p, proposal).is_some());
    assert!(request_planner(&p, p.request).is_none());
    assert!(request_planner(&p, second).is_none());
    assert_kept(&p);
}

/// The choice between continuing and `blocked` is
/// `replan_plan::planning_end`'s (unit tests): here the planner's
/// `--continue-run` reaches the operation through the supervisor.
#[test]
fn a_planners_decline_with_continue_run_continues_the_run() {
    let p = parked(1);
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    supervise(&p, &backend, &reviewer, 2);
    let (planner, _) = request_planner(&p, p.request).unwrap();
    queue(&p)
        .decline_request_choosing(
            p.request,
            "a local defect: one more revise finishes it",
            "planner",
            Some(planner),
            true,
        )
        .unwrap();
    supervise(&p, &backend, &reviewer, 2);
    let moved: Vec<Value> = events(&p, p.task, "replan_status_changed")
        .into_iter()
        .filter(|event| event["from"] == "planning")
        .collect();
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0]["to"], "continuing_after_park", "{moved:?}");
    assert_eq!(operation(&p).snapshot_id.as_deref(), Some("snap-1"));
    // `--continue-run` is refused on a request of no operation.
    let error = queue(&p)
        .decline_request_choosing(RequestId::new(1), "continue", "inbox", None, true)
        .unwrap_err()
        .to_string();
    assert!(error.contains("--continue-run"), "{error}");
}

/// A revise of a replacement proposal whose request's planner ended goes
/// to a planner opened for the proposal alone with the operation's
/// material and the steps of submitting it again with `--replacement`
/// (ADR-t1521-2 decisions 3 and 4): that planner's submit without the
/// plan is refused, and with it the plan is checked and kept as the next
/// revision.
#[test]
fn a_revise_planner_of_a_replacement_resubmits_it_with_its_plan() {
    let p = parked(0);
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[
        json!({"verdict": "revise", "reasons": ["say why the docs wait"], "summary": "s"}),
    ]);
    supervise(&p, &backend, &reviewer, 1);
    let (first, handle) = request_planner(&p, p.request).unwrap();
    let api = child(&p, "API", &[]);
    let docs = child(&p, "docs", &[]);
    let proposal = submit(
        &p,
        &handle,
        &[api, docs],
        None,
        Some(&plan(&p, [api, docs])),
    )
    .unwrap()
    .id();
    queue(&p).close_planner(first, None).unwrap();
    supervise(&p, &backend, &reviewer, 1);
    let planner = revise_planner(&p, proposal).expect("a planner for the revise");
    let op = operation(&p);
    let prompt = planner_prompt(&p.fx.db, planner);
    for expected in [
        "say why the docs wait".to_owned(),
        format!("## Replan operation {} of task {}", op.id, p.task),
        op.task_version.clone(),
        format!("- 1 `{}` the API behaves", op.conditions.acceptance[0].hash),
        "snap-1".to_owned(),
        format!("The latest plan (revision 1, proposal {proposal}"),
        format!("dagq replans {}", op.id),
        format!("dagq submit --proposal {proposal} --replacement <plan.json>"),
    ] {
        assert!(prompt.contains(&expected), "{expected}\n{prompt}");
    }
    let handle = queue(&p)
        .planners(false)
        .unwrap()
        .into_iter()
        .find(|row| row.id == planner)
        .and_then(|row| row.workspace_id)
        .unwrap();
    let error = format!(
        "{:#}",
        submit(&p, &handle, &[], Some(proposal), None).unwrap_err()
    );
    assert!(error.contains("--replacement"), "{error}");
    submit(
        &p,
        &handle,
        &[],
        Some(proposal),
        Some(&plan(&p, [api, docs])),
    )
    .unwrap();
    let submitted = events(&p, p.task, "replan_plan_submitted");
    assert_eq!(submitted.len(), 2, "{submitted:?}");
    assert_eq!(submitted[1]["proposal_id"], json!(proposal));
    assert_eq!(submitted[1]["revision"], json!(2));
    assert_eq!(operation(&p).status, ReplanStatus::Reviewing);
    assert_kept(&p);
}
