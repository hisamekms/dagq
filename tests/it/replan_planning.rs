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
//! decline with `--continue-run` continues the run. The trusted runtime
//! then applies an approved replacement at once (ADR-t1521-2 decision 5):
//! the task and its run end `replaced`, every task that waits for the task
//! (one that came to since included) waits for the children it needs, the
//! children become ready, and a failure midway, a stale plan, a loop
//! through a goal or a lost snapshot writes nothing and holds the saved
//! work: a failed write is checked again and applied once it goes, and a
//! plan that no longer holds goes back to a planner on a new request. An
//! apply a restart left `applying` is finished once, a second apply, the
//! old proposal and the replaced run's late receipt write nothing, a child
//! claimed after the apply is handed the saved head, and a replaced task
//! holds its goal and its recurring key until every task its replacement
//! reaches has ended.
//! The backend and the stub reviewer are plan review's
//! (`runtime_support::plan_review`).

use crate::common::Bounded;
use crate::common::cli::invoke_with;
use crate::common::queue::{base, new_task};
use crate::runtime_support::plan_review::{
    Fixture, PlanWorkspace, StubReviewer, completed_spike, fixture, open_goal, options,
    planner_prompt,
};
use crate::runtime_support::planner_turns::{exit_requested, idle, take_turns, turn_requests};
use anyhow::Result;
use dagq::{
    application::{TaskReads, TaskStore},
    domain::{
        AskConfidence, AskId, AskKind, AskReason, ClaimOutcome, GoalEdit, GoalId, GoalVerdict,
        LeaseToken, NewAsk, NewTask, PlannerId, PlannerOrigin, PlannerOwner, Proposal,
        ProposalStatus, ReplanId, RequestId, RunId, RunStatus, Submission, TaskAction, TaskEdit,
        TaskId, TaskStatus,
        plan_request::NewPlanRequest,
        replan_apply::ApplyOutcome,
        replan_plan::{ReplacementPlan, deletion_token},
        replan_snapshot::{SavedFile, SavedKind, SnapshotManifest, pinned_ref},
        task_replan::{AcceptanceItem, NewReplan, ReplanMove, ReplanOrigin, ReplanStatus},
    },
    infrastructure::{location::replans_dir, sqlite::SqliteQueue},
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
    /// The task's other successors ([`parked_with`]).
    others: Vec<TaskId>,
    /// The successors that came to wait for the task after its operation
    /// fixed its conditions, before the pause's fence ([`parked_with`]).
    late: Vec<TaskId>,
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
    parked_with(before, &[], &[])
}

/// [`parked`] with more successors of the task, titled `others`, and
/// `late` ones added once the operation fixed the task's conditions and
/// before its fence (the guard refuses a new dependency on the task from
/// then on).
fn parked_with(before: usize, others: &[&str], late: &[&str]) -> Parked {
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
    let others: Vec<TaskId> = others
        .iter()
        .map(|title| {
            queue
                .add(task_in(
                    goal,
                    title,
                    &format!("{title} uses the API"),
                    &[task],
                ))
                .unwrap()
                .id()
        })
        .collect();
    for &id in [task, successor].iter().chain(&others) {
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
    let mut added = Vec::new();
    let (op, request) = park_between(&mut queue, &fx.db, &run, &mut |queue| {
        for title in late {
            let id = queue
                .add(task_in(
                    goal,
                    title,
                    &format!("{title} uses the API"),
                    &[task],
                ))
                .unwrap()
                .id();
            queue.transition(id, TaskAction::BypassReview).unwrap();
            added.push(id);
        }
    });
    Parked {
        fx,
        goal,
        task,
        successor,
        others,
        late: added,
        run,
        op,
        request,
    }
}

/// Record a person's replanning request for the claimed `run` and park
/// it as the runtime does: the operation `parked` on snapshot `snap-1`,
/// the run `needs_session` with its lease given back.
fn park(queue: &mut SqliteQueue, db: &std::path::Path, run: &RunId) -> (ReplanId, RequestId) {
    park_between(queue, db, run, &mut |_| {})
}

/// [`park`], `between` changing the queue once the request is recorded
/// and before the pause's fence.
fn park_between(
    queue: &mut SqliteQueue,
    db: &std::path::Path,
    run: &RunId,
    between: &mut dyn FnMut(&mut SqliteQueue),
) -> (ReplanId, RequestId) {
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
    between(queue);
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
    // none of them while the runtime's apply is held back.
    hold_apply(&p);
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
    // Once the apply goes, the snapshot the park named is not there (this
    // test saved none): nothing is applied, and the operation is held
    // `blocked` with the run as the park left it.
    sql(&p, "DROP TRIGGER test_holds_the_apply");
    supervise(&p, &backend, &reviewer, 1);
    let op = operation(&p);
    assert_eq!(op.status, ReplanStatus::Blocked, "{op:?}");
    let reason = op.status_reason.clone().unwrap_or_default();
    assert!(reason.contains("snapshot snap-1"), "{reason}");
    assert!(op.replaced_by.is_empty(), "{op:?}");
    for id in [api, docs] {
        assert_eq!(status(&p, id), TaskStatus::Submitted);
    }
    assert_eq!(waits_for(&p, p.successor), vec![p.task]);
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

/// What `planner` of `p`'s queue asks as the planner role on the dagq
/// CLI, failing the test when the CLI refuses it: the ID of the ask it
/// opened.
fn ask_as_planner(p: &Parked, planner: PlannerId, about: &[&str], question: &str) -> AskId {
    let actor = format!("planner:{planner}");
    let mut args = vec![
        "ask",
        "--kind",
        "planner_question",
        "--because",
        "scope",
        "--question",
        question,
    ];
    args.extend_from_slice(about);
    let output = invoke_with(
        &[("DAGQ_ROLE", "planner"), ("DAGQ_ACTOR_ID", &actor)],
        &p.fx.db,
        &args,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    queue(p)
        .asks(dagq::application::AskQuery {
            all: true,
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .find(|ask| ask.question == question)
        .unwrap()
        .id
}

/// Mark `planner` idle again, as a turn of it stopped.
fn stopped(p: &Parked, planner: PlannerId) {
    let dir = dagq::infrastructure::location::planners_dir(&p.fx.db).join(planner.to_string());
    std::fs::write(dagq::application::planner_idle_marker(&dir), "{}").unwrap();
}

/// The plan of [`plan`] with item 2 `deleted` on the authority of
/// `authority`, its child `docs` holding item 1 too.
fn deleting_plan(p: &Parked, children: [TaskId; 2], authority: AskId) -> Value {
    let mut deleting = plan(p, children);
    let hash = deleting["mappings"][1]["hash"].clone();
    deleting["mappings"][1] = json!({"ordinal": 2, "hash": hash,
                                     "disposition": "deleted", "authority_ask_id": authority});
    deleting["mappings"][0]["children"]
        .as_array_mut()
        .unwrap()
        .push(json!({"task_id": children[1], "ordinal": 1}));
    deleting
}

/// A revise of a replacement proposal opened after its request's planner
/// ended, as far as the planner it went to, open.
fn revising_replacement(
    p: &Parked,
    backend: &PlanWorkspace,
    reviewer: &StubReviewer,
) -> (dagq::domain::ProposalId, [TaskId; 2], PlannerId) {
    supervise(p, backend, reviewer, 1);
    let (first, handle) = request_planner(p, p.request).unwrap();
    let api = child(p, "API", &[]);
    let docs = child(p, "docs", &[]);
    let proposal = submit(p, &handle, &[api, docs], None, Some(&plan(p, [api, docs])))
        .unwrap()
        .id();
    queue(p).close_planner(first, None).unwrap();
    supervise(p, backend, reviewer, 1);
    let planner = revise_planner(p, proposal).expect("a planner for the revise");
    (proposal, [api, docs], planner)
}

/// The planner of a replacement's revise, open, asks a person about the
/// parked task to delete an item as its prompt says, and the answer is
/// typed to it as its next turn (ADR-t1521-2 decision 3, ADR-t1704-1
/// decision 2).
#[test]
fn the_deletion_ask_about_the_parked_task_reaches_the_open_revise_planner() {
    let p = parked(0);
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[
        json!({"verdict": "revise", "reasons": ["the docs are not needed"], "summary": "s"}),
    ]);
    let (_, _, planner) = revising_replacement(&p, &backend, &reviewer);
    let item = operation(&p).conditions.acceptance[1].clone();
    let token = deletion_token(&item);
    let prompt = planner_prompt(&p.fx.db, planner);
    for expected in [
        format!(
            "`dagq ask --task {} --kind planner_question --because scope",
            p.task
        ),
        "--option delete:<ordinal>:<the first 12 characters of its hash> --option keep`".to_owned(),
        "An ask about a child, or one asked before the operation began, allows no deletion."
            .to_owned(),
    ] {
        assert!(prompt.contains(&expected), "{expected}\n{prompt}");
    }
    let task = p.task.to_string();
    let asked = ask_as_planner(
        &p,
        planner,
        &["--task", &task, "--option", &token, "--option", "keep"],
        "drop the documents item?",
    );
    idle(&queue(&p), &p.fx.db, planner);
    queue(&p).answer(asked, &token).unwrap();
    supervise(&p, &backend, &reviewer, 1);
    assert!(!exit_requested(&p.fx.db, planner));
    let requests = turn_requests(&p.fx.db, planner);
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(
        requests[0]["prompt"],
        format!("answer to ask {asked}: {token}")
    );
    assert!(queue(&p).read_ask(asked).unwrap().closed_at.is_some());
    assert_kept(&p);
}

/// The planner of a replacement's revise that waits only for a person's
/// answer to its deletion ask about the parked task is ended for it, and
/// no planner is opened without the answer; the answer then goes with the
/// revise to one new planner, whose resubmit deleting the item on that ask
/// is accepted, while one on an ask about a child is refused
/// (ADR-t1521-1 decision 3, ADR-t1521-2 decisions 3 and 4, ADR-t1704-1
/// decisions 1 and 4).
#[test]
fn a_revise_planner_ended_for_its_deletion_ask_hands_the_answer_to_one_new_planner() {
    let p = parked(0);
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[
        json!({"verdict": "revise", "reasons": ["the docs are not needed"], "summary": "s"}),
    ]);
    let (proposal, [api, docs], planner) = revising_replacement(&p, &backend, &reviewer);
    let item = operation(&p).conditions.acceptance[1].clone();
    let token = deletion_token(&item);
    let (task, docs_text) = (p.task.to_string(), docs.to_string());
    let about_child = ask_as_planner(
        &p,
        planner,
        &["--task", &docs_text, "--option", &token, "--option", "keep"],
        "drop the documents item, asked about the child?",
    );
    idle(&queue(&p), &p.fx.db, planner);
    queue(&p).answer(about_child, &token).unwrap();
    supervise(&p, &backend, &reviewer, 1);
    assert_eq!(turn_requests(&p.fx.db, planner).len(), 1);
    take_turns(&queue(&p), &p.fx.db, planner);
    let asked = ask_as_planner(
        &p,
        planner,
        &["--task", &task, "--option", &token, "--option", "keep"],
        "drop the documents item?",
    );
    stopped(&p, planner);
    supervise(&p, &backend, &reviewer, 1);
    assert!(exit_requested(&p.fx.db, planner));
    let waited: Vec<Value> = queue(&p)
        .latest_events_of("planner_answer_wait", 10)
        .unwrap()
        .into_iter()
        .map(|event| event.payload)
        .collect();
    assert_eq!(waited.len(), 1, "{waited:?}");
    assert_eq!(waited[0]["asks"], json!([asked]), "{waited:?}");
    assert_eq!(waited[0]["revise"], true, "{waited:?}");
    queue(&p)
        .planner_exited(planner, std::process::id(), 0)
        .unwrap();
    let launched = backend.launched().len();
    supervise(&p, &backend, &reviewer, 1);
    supervise(&p, &backend, &reviewer, 1);
    assert!(revise_planner(&p, proposal).is_none());
    assert_eq!(backend.launched().len(), launched, "not opened unanswered");

    queue(&p).answer(asked, &token).unwrap();
    supervise(&p, &backend, &reviewer, 1);
    supervise(&p, &backend, &reviewer, 1);
    let next = revise_planner(&p, proposal).expect("a planner carries the answer");
    assert_ne!(next, planner);
    assert_eq!(backend.launched().len(), launched + 1, "one planner");
    let prompt = planner_prompt(&p.fx.db, next);
    for expected in [
        format!(
            "asked a person (ask {asked}) about task {}, which this proposal replaces,",
            p.task
        ),
        "drop the documents item?".to_owned(),
        format!("answer to ask {asked}: {token}"),
        format!("dagq submit --proposal {proposal} --replacement <plan.json>"),
    ] {
        assert!(prompt.contains(&expected), "{expected}\n{prompt}");
    }
    assert!(queue(&p).read_ask(asked).unwrap().closed_at.is_some());
    let handle = queue(&p)
        .planners(false)
        .unwrap()
        .into_iter()
        .find(|row| row.id == next)
        .and_then(|row| row.workspace_id)
        .unwrap();
    // An ask about a child allows no deletion.
    let error = format!(
        "{:#}",
        submit(
            &p,
            &handle,
            &[],
            Some(proposal),
            Some(&deleting_plan(&p, [api, docs], about_child)),
        )
        .unwrap_err()
    );
    assert!(error.contains("deletion_unauthorized"), "{error}");
    submit(
        &p,
        &handle,
        &[],
        Some(proposal),
        Some(&deleting_plan(&p, [api, docs], asked)),
    )
    .unwrap();
    let submitted = events(&p, p.task, "replan_plan_submitted");
    assert_eq!(submitted.len(), 2, "{submitted:?}");
    assert_eq!(operation(&p).status, ReplanStatus::Reviewing);
    assert_kept(&p);
}

/// Save snapshot `snap-1` of `p`'s parked run as the park leaves it: a
/// manifest of the operation and its run with one staged diff, and the
/// run's head (a commit on top of main) pinned by the operation's ref. The
/// head.
fn save_snapshot(p: &Parked) -> String {
    let head = {
        let base = git(p, &["rev-parse", "HEAD"]);
        let head = git(
            p,
            &[
                "commit-tree",
                "HEAD^{tree}",
                "-p",
                "HEAD",
                "-m",
                "the parked run's work",
            ],
        );
        git(p, &["update-ref", &pinned_ref(p.op), &head]);
        (base, head)
    };
    let dir = replans_dir(&p.fx.db).join("snap-1");
    std::fs::create_dir_all(&dir).unwrap();
    let diff = b"diff --git a/notes.txt b/notes.txt\n";
    std::fs::write(dir.join("index.diff"), diff).unwrap();
    let op = operation(p);
    let manifest = SnapshotManifest {
        id: "snap-1".into(),
        operation: p.op,
        generation: op.generation,
        run_id: p.run.clone(),
        task_id: p.task,
        base_commit: head.0,
        head: head.1.clone(),
        branch: None,
        pinned_ref: pinned_ref(p.op),
        worktree: "worktree".into(),
        dirty: true,
        files: vec![SavedFile::of(
            "index.diff",
            SavedKind::Index,
            "git diff --cached --binary",
            diff,
        )],
        excluded: Vec::new(),
        conditions_version: op.task_version.clone(),
        remaining: op.conditions.acceptance.clone(),
        saved_at: 0,
    };
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    head.1
}

/// The output of `git args` in `p`'s repository, which must succeed.
fn git(p: &Parked, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&p.fx.repo)
        .args(args)
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Whether the snapshot and the ref pinning `head` are still where the
/// park left them.
fn assert_snapshot_kept(p: &Parked, head: &str) {
    assert!(replans_dir(&p.fx.db).join("snap-1/manifest.json").is_file());
    assert_eq!(git(p, &["rev-parse", &pinned_ref(p.op)]), head);
}

fn sql(p: &Parked, statements: &str) {
    Connection::open(&p.fx.db)
        .unwrap()
        .execute_batch(statements)
        .unwrap();
}

/// What `task` waits for now, in ID order.
fn waits_for(p: &Parked, task: TaskId) -> Vec<TaskId> {
    Connection::open(&p.fx.db)
        .unwrap()
        .prepare("SELECT predecessor_id FROM task_dependencies WHERE task_id=?1 ORDER BY 1")
        .unwrap()
        .query_map([task], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// Hold the runtime's apply back: the move to `applying` fails, so an
/// approved replacement waits as plan review left it.
fn hold_apply(p: &Parked) {
    sql(
        p,
        "CREATE TRIGGER test_holds_the_apply BEFORE UPDATE OF status ON replan_operations
         WHEN NEW.status='applying' BEGIN SELECT RAISE(ABORT, 'the test holds the apply'); END;",
    );
}

/// Two children of `p`'s goal, `docs` waiting for `api`, both held from a
/// claim by the draft task 1, and the plan that splits the task between
/// them.
fn children(p: &Parked) -> (TaskId, TaskId, Value) {
    let blocker = TaskId::new(1);
    let api = child(p, "API", &[blocker]);
    let docs = child(p, "docs", &[blocker, api]);
    (api, docs, plan(p, [api, docs]))
}

/// Bring `p`'s replacement by `tasks` with `plan` through plan review's
/// pass with the apply held back: the operation `reviewing`, its plan
/// approved, nothing applied. Its proposal.
fn approved(
    p: &Parked,
    backend: &PlanWorkspace,
    reviewer: &StubReviewer,
    tasks: &[TaskId],
    plan: &Value,
) -> dagq::domain::ProposalId {
    supervise(p, backend, reviewer, 1);
    let (_, handle) = request_planner(p, p.request).unwrap();
    let proposal = submit(p, &handle, tasks, None, Some(plan)).unwrap();
    hold_apply(p);
    for _ in 0..3 {
        if !events(p, p.task, "replan_plan_approved").is_empty() {
            break;
        }
        supervise(p, backend, reviewer, 1);
    }
    sql(p, "DROP TRIGGER test_holds_the_apply");
    assert_eq!(events(p, p.task, "replan_plan_approved").len(), 1);
    assert_eq!(operation(p).status, ReplanStatus::Reviewing);
    proposal.id()
}

/// Nothing of the replacement is written: the task, its run and its saved
/// work are as the park left them, the children submitted and every
/// successor still waiting for the task.
fn assert_unapplied(p: &Parked, children: &[TaskId], head: &str) {
    assert_kept(p);
    let op = operation(p);
    assert!(op.replaced_by.is_empty(), "{op:?}");
    assert!(op.mappings.is_empty(), "{op:?}");
    for &id in children {
        assert_eq!(status(p, id), TaskStatus::Submitted);
    }
    for &successor in std::iter::once(&p.successor)
        .chain(&p.others)
        .chain(&p.late)
    {
        assert_eq!(waits_for(p, successor), vec![p.task]);
    }
    assert!(events(p, p.task, "replan_applied").is_empty());
    assert_snapshot_kept(p, head);
}

/// The trusted runtime applies an approved replacement at once: the task
/// and its parked run end `replaced` (never completed), the children hold
/// the task's items and become ready, each successor waits for the
/// children it needs in the task's place (all of them for one the plan
/// does not list, which came to wait after it), the saved work stays, and
/// nothing that waited for the task can be claimed before the children
/// land. The goal cannot close as achieved, and neither a second apply nor
/// the replaced run's late landing changes anything.
#[test]
fn an_approved_replacement_is_applied_at_once_and_every_successor_waits_for_its_children() {
    let p = parked_with(0, &["D"], &["E"]);
    let (d, late) = (p.others[0], p.late[0]);
    let head = save_snapshot(&p);
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let (api, docs, mut plan) = children(&p);
    plan["successors"]
        .as_array_mut()
        .unwrap()
        .push(json!({"task_id": d, "needs": [docs], "reason": "D reads the documents only"}));
    approved(&p, &backend, &reviewer, &[api, docs], &plan);
    supervise(&p, &backend, &reviewer, 1);
    let op = operation(&p);
    assert_eq!(op.status, ReplanStatus::Applied, "{op:?}");
    assert_eq!(op.replaced_by, vec![api, docs]);
    let mapped: Vec<(usize, Option<TaskId>)> = op
        .mappings
        .iter()
        .map(|mapping| (mapping.ordinal, mapping.child_task_id))
        .collect();
    assert_eq!(mapped, vec![(1, Some(api)), (2, Some(docs))]);
    let mut read = queue(&p);
    let detail = read.show(p.task).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Replaced);
    assert_eq!(detail.runs.len(), 1);
    assert_eq!(detail.runs[0].status(), RunStatus::Replaced);
    assert!(read.replan_guard(p.task).unwrap().is_none());
    for id in [api, docs] {
        assert_eq!(status(&p, id), TaskStatus::Ready);
    }
    assert_eq!(waits_for(&p, p.successor), vec![api, docs]);
    assert_eq!(waits_for(&p, d), vec![docs]);
    assert_eq!(waits_for(&p, late), vec![api, docs]);
    assert_eq!(waits_for(&p, docs), vec![TaskId::new(1), api]);
    let applied = events(&p, p.task, "replan_applied");
    assert_eq!(applied.len(), 1, "{applied:?}");
    assert_eq!(applied[0]["children"], json!([api, docs]));
    assert_eq!(
        applied[0]["successors"],
        json!([
            {"task_id": p.successor, "before": [p.task], "after": [api, docs]},
            {"task_id": d, "before": [p.task], "after": [docs]},
            {"task_id": late, "before": [p.task], "after": [api, docs]},
        ])
    );
    assert_snapshot_kept(&p, &head);
    // What each child's claim hands over is what the plan named: the
    // first the run's commits from the snapshot, the second nothing.
    let carry = queue(&p).child_carry(api).unwrap().unwrap();
    assert_eq!(
        (
            carry.operation,
            carry.from_run.clone(),
            carry.snapshot_id.as_str()
        ),
        (p.op, p.run.clone(), "snap-1")
    );
    assert_eq!(carry.carry.as_str(), "snapshot_head");
    assert_eq!(carry.order, Some(1));
    assert!(queue(&p).child_carry(docs).unwrap().is_none());
    // Nothing is claimable: the children wait for task 1, and what waited
    // for the task waits for them.
    assert!(matches!(
        queue(&p).claim(&base()).unwrap(),
        ClaimOutcome::NoReadyTask
    ));
    // The goal's open children keep it from closing as achieved.
    assert!(queue(&p).close_goal(p.goal, GoalVerdict::Achieved).is_err());
    // A second apply and another pass write nothing.
    assert_eq!(
        queue(&p)
            .apply_replacement(p.op, op.generation, None)
            .unwrap(),
        ApplyOutcome::AlreadyApplied
    );
    supervise(&p, &backend, &reviewer, 1);
    assert_eq!(events(&p, p.task, "replan_applied").len(), 1);
    assert_eq!(operation(&p).replaced_by, vec![api, docs]);
    assert_eq!(waits_for(&p, p.successor), vec![api, docs]);
    // The replaced run's late landing and a retry of its task are refused.
    assert!(
        queue(&p)
            .begin_integration(&p.run, &LeaseToken::new("a late supervisor"), &base())
            .is_err()
    );
    assert!(queue(&p).transition(p.task, TaskAction::Ready).is_err());
    let detail = queue(&p).show(p.task).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Replaced);
    assert_eq!(detail.runs[0].status(), RunStatus::Replaced);
    // No task comes to wait for the replaced task: it never completes.
    let error = queue(&p)
        .add_dependency(late, p.task)
        .unwrap_err()
        .to_string();
    assert!(error.contains("replaced"), "{error}");
    // With one child landed (as its run's landing leaves it), what waits
    // for both still waits; once both landed, it is claimed.
    let landed = |task: TaskId| {
        sql(
            &p,
            &format!("UPDATE tasks SET status='completed' WHERE id={task}"),
        );
    };
    landed(api);
    assert!(matches!(
        queue(&p).claim(&base()).unwrap(),
        ClaimOutcome::NoReadyTask
    ));
    landed(docs);
    let ClaimOutcome::Claimed { run } = queue(&p).claim(&base()).unwrap() else {
        panic!("a successor is claimed once both children landed")
    };
    assert!([p.successor, d, late].contains(&run.task_id()), "{run:?}");
}

/// A write that fails midway through the apply rolls all of it back: the
/// operation is held `blocked` with why, and the task, its parked run,
/// the children, every dependency and the saved work are as before. The
/// runtime checks it again at each pass, writing nothing while the write
/// still fails, and applies it once the failure is gone.
#[test]
fn an_apply_that_fails_midway_writes_nothing_and_is_applied_once_the_failure_is_gone() {
    let p = parked(0);
    let head = save_snapshot(&p);
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let (api, docs, plan) = children(&p);
    approved(&p, &backend, &reviewer, &[api, docs], &plan);
    // The successor's new dependency on the second child is about the
    // last write: the task, the run, the children's rows and mappings
    // are written before it.
    sql(
        &p,
        &format!(
            "CREATE TRIGGER test_fails_the_apply BEFORE INSERT ON task_dependencies
             WHEN NEW.task_id={} AND NEW.predecessor_id={docs}
             BEGIN SELECT RAISE(ABORT, 'the test fails the apply'); END;",
            p.successor
        ),
    );
    supervise(&p, &backend, &reviewer, 1);
    let op = operation(&p);
    assert_eq!(op.status, ReplanStatus::Blocked, "{op:?}");
    assert_eq!(op.return_phase, Some(ReplanStatus::Applying));
    let reason = op.status_reason.unwrap_or_default();
    assert!(reason.contains("rolled back"), "{reason}");
    assert!(reason.contains("the test fails the apply"), "{reason}");
    assert_unapplied(&p, &[api, docs], &head);
    let moves = events(&p, p.task, "replan_status_changed").len();
    supervise(&p, &backend, &reviewer, 1);
    assert_eq!(operation(&p).status, ReplanStatus::Blocked);
    assert_eq!(events(&p, p.task, "replan_status_changed").len(), moves);
    assert!(events(&p, p.task, "replan_apply_rechecked").is_empty());
    assert_unapplied(&p, &[api, docs], &head);
    // A check again that finds another reason (the snapshot gone) keeps
    // it, once, for the person it now needs.
    let manifest = replans_dir(&p.fx.db).join("snap-1/manifest.json");
    let aside = manifest.with_extension("aside");
    std::fs::rename(&manifest, &aside).unwrap();
    supervise(&p, &backend, &reviewer, 1);
    supervise(&p, &backend, &reviewer, 1);
    let rechecked = events(&p, p.task, "replan_apply_rechecked");
    assert_eq!(rechecked.len(), 1, "{rechecked:?}");
    assert_eq!(rechecked[0]["recovery"], "person");
    let op = operation(&p);
    assert_eq!(op.status, ReplanStatus::Blocked);
    assert!(
        op.status_reason
            .unwrap_or_default()
            .contains("snapshot snap-1"),
        "{rechecked:?}"
    );
    assert_eq!(events(&p, p.task, "replan_status_changed").len(), moves);
    std::fs::rename(&aside, &manifest).unwrap();
    sql(&p, "DROP TRIGGER test_fails_the_apply");
    supervise(&p, &backend, &reviewer, 1);
    let op = operation(&p);
    assert_eq!(op.status, ReplanStatus::Applied, "{op:?}");
    assert_eq!(op.replaced_by, vec![api, docs]);
    assert_eq!(waits_for(&p, p.successor), vec![api, docs]);
    assert_eq!(events(&p, p.task, "replan_applied").len(), 1);
}

/// An apply a stopped supervisor left `applying` is finished once by the
/// next one.
#[test]
fn an_apply_a_stopped_supervisor_left_applying_is_finished_once() {
    let p = parked(0);
    let head = save_snapshot(&p);
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let (api, docs, plan) = children(&p);
    approved(&p, &backend, &reviewer, &[api, docs], &plan);
    // An apply the runtime did not begin writes nothing: the operation is
    // left to the move that took it (here, still `reviewing`).
    let generation = operation(&p).generation;
    assert!(matches!(
        queue(&p).apply_replacement(p.op, generation, None).unwrap(),
        ApplyOutcome::Refused {
            recorded: false,
            ..
        }
    ));
    assert_eq!(operation(&p).status, ReplanStatus::Reviewing);
    // As a supervisor stopped after it began the apply leaves it: still
    // guarding the task, the children submitted.
    let begun = queue(&p).begin_replan_apply(p.op, generation).unwrap();
    assert_eq!(begun.status, ReplanStatus::Applying);
    assert_unapplied(&p, &[api, docs], &head);
    supervise(&p, &backend, &reviewer, 1);
    assert_eq!(operation(&p).status, ReplanStatus::Applied);
    assert_eq!(events(&p, p.task, "replan_applied").len(), 1);
    assert_eq!(waits_for(&p, p.successor), vec![api, docs]);
}

/// A replacement whose goal's acceptance changed since its pass writes
/// nothing: the plan is stale, so the operation goes back to a planner on
/// a new request with the saved work and the children released, and that
/// planner's choice (here to continue the run) ends it.
#[test]
fn an_apply_of_a_plan_gone_stale_since_its_pass_goes_back_to_a_planner() {
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let stale = parked(0);
    let head = save_snapshot(&stale);
    let (api, docs, plan) = children(&stale);
    approved(&stale, &backend, &reviewer, &[api, docs], &plan);
    queue(&stale)
        .edit_goal(
            stale.goal,
            GoalEdit {
                acceptance: Some("every draft is decided, and documented".into()),
                ..GoalEdit::default()
            },
        )
        .unwrap();
    if let Some((planner, _)) = request_planner(&stale, stale.request) {
        exit(&stale, planner);
    }
    supervise(&stale, &backend, &reviewer, 1);
    let held = moved_to(&stale, "blocked");
    assert_eq!(held.len(), 1, "{held:?}");
    assert!(
        held[0]["reason"]
            .as_str()
            .unwrap()
            .contains("version_stale"),
        "{held:?}"
    );
    let request = assert_replanned(&stale, &[api, docs], &head);
    for _ in 0..3 {
        if request_planner(&stale, request).is_some() {
            break;
        }
        supervise(&stale, &backend, &reviewer, 1);
    }
    let (planner, _) = request_planner(&stale, request).unwrap();
    assert_eq!(operation(&stale).status, ReplanStatus::Planning);
    queue(&stale)
        .decline_request_choosing(
            request,
            "the goal changed under the plan: finish the run as it is",
            "planner",
            Some(planner),
            true,
        )
        .unwrap();
    supervise(&stale, &backend, &reviewer, 1);
    let op = operation(&stale);
    assert_eq!(op.status, ReplanStatus::ContinuingAfterPark, "{op:?}");
    assert!(op.replaced_by.is_empty(), "{op:?}");
    assert_eq!(op.snapshot_id.as_deref(), Some("snap-1"));
    assert_eq!(waits_for(&stale, stale.successor), vec![stale.task]);
}

/// The payloads of the operation's moves to `to` on `p`'s task.
fn moved_to(p: &Parked, to: &str) -> Vec<Value> {
    events(p, p.task, "replan_status_changed")
        .into_iter()
        .filter(|event| event["to"] == to)
        .collect()
}

/// Nothing of the replacement is written and the operation went back to a
/// planner with the saved work (ADR-t1521-2 decision 2): `parked` (or
/// `planning` once that planner opened) on a new request, the task and its
/// run as the park left them, the children released to draft and every
/// successor still waiting for the task. The new request.
fn assert_replanned(p: &Parked, children: &[TaskId], head: &str) -> RequestId {
    assert_kept(p);
    let op = operation(p);
    assert!(
        matches!(op.status, ReplanStatus::Parked | ReplanStatus::Planning),
        "{op:?}"
    );
    assert!(op.replaced_by.is_empty(), "{op:?}");
    assert!(op.mappings.is_empty(), "{op:?}");
    let request = op.request_id.unwrap();
    assert_ne!(request, p.request);
    for &id in children {
        assert_eq!(status(p, id), TaskStatus::Draft);
    }
    assert_eq!(events(p, p.task, "replan_children_released").len(), 1);
    assert!(events(p, p.task, "replan_applied").is_empty());
    assert_eq!(moved_to(p, "parked").len(), 2, "the park and the replan");
    assert_snapshot_kept(p, head);
    request
}

/// The apply is checked on the whole wait graph, goals included
/// (ADR-0038): once a successor joins the goal a child waits for (after
/// the pass), the successor would wait for itself through that goal when
/// it waits for the child. Nothing is applied, the operation goes back to
/// a planner with the saved work, and that planner's submit of the same
/// plan is refused, since the check of a plan reads the same graph.
#[test]
fn an_apply_that_closes_a_loop_through_a_goal_writes_nothing() {
    let p = parked_with(0, &["D"], &[]);
    let d = p.others[0];
    let head = save_snapshot(&p);
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let other = open_goal(&mut queue(&p));
    let (api, docs, mut plan) = children(&p);
    plan["successors"]
        .as_array_mut()
        .unwrap()
        .push(json!({"task_id": d, "needs": [api, docs]}));
    queue(&p).add_goal_dependency(api, other).unwrap();
    approved(&p, &backend, &reviewer, &[api, docs], &plan);
    queue(&p).set_goal(d, Some(other)).unwrap();
    if let Some((planner, _)) = request_planner(&p, p.request) {
        exit(&p, planner);
    }
    supervise(&p, &backend, &reviewer, 1);
    let held = moved_to(&p, "blocked");
    assert_eq!(held.len(), 1, "{held:?}");
    let reason = held[0]["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!("task {d} would wait for itself")),
        "{reason}"
    );
    assert_eq!(waits_for(&p, d), vec![p.task]);
    assert_eq!(waits_for(&p, p.successor), vec![p.task]);
    let request = assert_replanned(&p, &[api, docs], &head);
    for _ in 0..3 {
        if request_planner(&p, request).is_some() {
            break;
        }
        supervise(&p, &backend, &reviewer, 1);
    }
    let (_, handle) = request_planner(&p, request).unwrap();
    let error = submit(&p, &handle, &[api, docs], None, Some(&plan))
        .unwrap_err()
        .to_string();
    assert!(error.contains("would wait for itself"), "{error}");
}

/// A child edited after plan review approved the plan, and before the
/// runtime applied it, is not the child the review approved: the apply
/// compares each child's conditions with their version at the approval,
/// writes nothing, and the operation goes back to a planner with the saved
/// work. A task that only came to wait for a child changes nothing the
/// child does, and refuses nothing.
#[test]
fn a_child_edited_between_the_approval_and_the_apply_refuses_the_apply() {
    let p = parked(0);
    let head = save_snapshot(&p);
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let (api, docs, plan) = children(&p);
    approved(&p, &backend, &reviewer, &[api, docs], &plan);
    queue(&p)
        .add(task_in(p.goal, "later", "later uses the API", &[api]))
        .unwrap();
    // The same number of items, another text: the plan alone still holds.
    queue(&p)
        .edit_task(
            docs,
            TaskEdit {
                acceptance: Some("docs holds something else".into()),
                ..TaskEdit::default()
            },
            TaskStatus::Submitted,
        )
        .unwrap();
    if let Some((planner, _)) = request_planner(&p, p.request) {
        exit(&p, planner);
    }
    supervise(&p, &backend, &reviewer, 1);
    let held = moved_to(&p, "blocked");
    assert_eq!(held.len(), 1, "{held:?}");
    let reason = held[0]["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!(
            "child task {docs} changed since plan review approved the plan"
        )),
        "{reason}"
    );
    assert!(
        !reason.contains(&format!("child task {api} changed")),
        "{reason}"
    );
    assert_eq!(waits_for(&p, p.successor), vec![p.task]);
    assert_replanned(&p, &[api, docs], &head);
}

/// A child that stops waiting for a goal, or whose run is to go another
/// way, after plan review approved the plan is not the child the review
/// approved either: the apply writes nothing, the operation goes back to a
/// planner with the saved work, and neither child becomes ready but goes
/// back to draft.
#[test]
fn a_childs_goal_dependency_or_run_condition_changed_after_the_approval_refuses_the_apply() {
    let p = parked(0);
    let head = save_snapshot(&p);
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let (api, docs, plan) = children(&p);
    let other = open_goal(&mut queue(&p));
    queue(&p).add_goal_dependency(api, other).unwrap();
    approved(&p, &backend, &reviewer, &[api, docs], &plan);
    queue(&p).remove_goal_dependency(api, other).unwrap();
    queue(&p)
        .edit_task(
            docs,
            TaskEdit {
                wait_for_build: Some(true),
                ..TaskEdit::default()
            },
            TaskStatus::Submitted,
        )
        .unwrap();
    if let Some((planner, _)) = request_planner(&p, p.request) {
        exit(&p, planner);
    }
    supervise(&p, &backend, &reviewer, 1);
    let held = moved_to(&p, "blocked");
    assert_eq!(held.len(), 1, "{held:?}");
    let reason = held[0]["reason"].as_str().unwrap();
    for child in [api, docs] {
        assert!(
            reason.contains(&format!(
                "child task {child} changed since plan review approved the plan"
            )),
            "{reason}"
        );
    }
    assert_eq!(waits_for(&p, p.successor), vec![p.task]);
    assert_replanned(&p, &[api, docs], &head);
}

/// A goal whose task a replanning replaced is reviewed with what replaced
/// it (ADR-t1521-2 decision 5): the operation, the task's items, the
/// children and the mappings, and a child moved to another goal with its
/// own task. A review that began before a mapping changed is stale: its
/// achieved verdict closes nothing, and the next review reads the
/// mapping as it is.
#[test]
fn a_goal_review_reads_the_replacement_and_a_changed_mapping_stales_its_verdict() {
    use dagq::application::{GoalReviewApply, GoalReviewStore};
    use dagq::domain::{
        actor_model::{ActorLaunch, ModelRole},
        goal_review::{GoalReviewDecision, GoalReviewVerdict},
    };
    let p = parked(0);
    save_snapshot(&p);
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let (api, docs, plan) = children(&p);
    approved(&p, &backend, &reviewer, &[api, docs], &plan);
    supervise(&p, &backend, &reviewer, 1);
    assert_eq!(operation(&p).status, ReplanStatus::Applied);
    let other = open_goal(&mut queue(&p));
    queue(&p).set_goal(docs, Some(other)).unwrap();
    for task in [api, docs, p.successor] {
        sql(
            &p,
            &format!("UPDATE tasks SET status='completed' WHERE id={task}"),
        );
    }
    // A review begun, then a mapping changed: its achieved verdict is stale.
    let token = LeaseToken::new("goal-review-owner");
    let mut q = queue(&p);
    let job = q
        .begin_goal_review(
            p.goal,
            &token,
            &p.fx.repo.join("goal-reviews"),
            &p.fx.repo,
            &ActorLaunch::default_of(ModelRole::GoalReview),
            &|_| false,
        )
        .unwrap()
        .unwrap();
    sql(
        &p,
        &format!(
            "UPDATE replan_mappings SET child_task_id={api} WHERE operation_id={} AND ordinal=2",
            p.op
        ),
    );
    let applied = q
        .finish_goal_review(
            &job,
            &token,
            &GoalReviewApply {
                verdict: GoalReviewVerdict::parse(r#"{"verdict":"achieved","summary":"done"}"#)
                    .unwrap(),
                decision: GoalReviewDecision::Achieved,
                overridden: None,
                ask: None,
                duration_secs: 1,
                session: None,
                prompt_bytes: None,
            },
        )
        .unwrap();
    assert!(applied.stale, "{applied:?}");
    assert!(!q.show_goal(p.goal).unwrap().goal.is_closed());
    // The next review is given the replacement and the moved child.
    let goal_reviewer = StubReviewer::new(&[json!({
        "verdict": "achieved",
        "criteria": [],
        "summary": "the children landed both items",
    })]);
    supervise(&p, &backend, &goal_reviewer, 1);
    // The goal the child moved to is reviewed too, on its own.
    let prompts = goal_reviewer.prompts();
    let ours = format!("Every task of goal {} ended", p.goal);
    let prompt = prompts
        .iter()
        .find(|prompt| prompt.contains(&ours))
        .unwrap_or_else(|| panic!("{prompts:?}"));
    let op = operation(&p);
    for expected in [
        format!(r#""operation":{}"#, p.op),
        format!(r#""replaced_task":{}"#, p.task),
        format!(r#""children":[{api},{docs}]"#),
        format!(r#""hash":"{}""#, op.conditions.acceptance[1].hash),
        "the operations are documented".to_owned(),
        format!(r#""child_task_id":{api}"#),
        format!(r#""id":{docs}"#),
        r#""replaces_in_this_goal":true"#.to_owned(),
    ] {
        assert!(prompt.contains(&expected), "{expected}\n{prompt}");
    }
    assert!(queue(&p).show_goal(p.goal).unwrap().goal.is_closed());
}

/// Once the replacement is applied, what the replaced run and its planner
/// still hold moves nothing: the old proposal sent again, a second apply
/// under the generation the pass read, and the run's late receipt with the
/// lease it held, delivered through the store's own steps of a session's
/// end, a rewritten receipt and a landing, are refused, and the
/// replacement, the children, the dependencies and main stay as the apply
/// left them.
#[test]
fn an_old_proposal_and_the_replaced_runs_late_receipt_change_nothing() {
    let p = parked(0);
    let head = save_snapshot(&p);
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let (api, docs, plan) = children(&p);
    let proposal = approved(&p, &backend, &reviewer, &[api, docs], &plan);
    let (_, handle) = request_planner(&p, p.request).unwrap();
    let generation = operation(&p).generation;
    // The run as its provisioning left it, with a directory for its
    // receipt.
    let run_dir = dagq::infrastructure::location::runs_dir(&p.fx.db).join(p.run.as_str());
    std::fs::create_dir_all(&run_dir).unwrap();
    sql(
        &p,
        &format!(
            "UPDATE task_runs SET run_dir='{dir}', receipt_path='{dir}/receipt.json' WHERE id='{run}'",
            dir = run_dir.display(),
            run = p.run
        ),
    );
    supervise(&p, &backend, &reviewer, 1);
    assert_eq!(operation(&p).status, ReplanStatus::Applied);
    let main = git(&p, &["rev-parse", "main"]);
    let check = |p: &Parked| {
        let op = operation(p);
        assert_eq!(op.status, ReplanStatus::Applied, "{op:?}");
        assert_eq!(op.replaced_by, vec![api, docs]);
        assert_eq!(op.proposal_id, Some(proposal));
        let detail = queue(p).show(p.task).unwrap();
        assert_eq!(detail.task.status(), TaskStatus::Replaced);
        assert_eq!(detail.runs.len(), 1);
        assert_eq!(detail.runs[0].status(), RunStatus::Replaced);
        for id in [api, docs] {
            assert_eq!(status(p, id), TaskStatus::Ready);
            assert!(queue(p).show(id).unwrap().runs.is_empty());
        }
        assert_eq!(waits_for(p, p.successor), vec![api, docs]);
        assert_eq!(events(p, p.task, "replan_applied").len(), 1);
        for kind in [
            "validation_started",
            "validation_finished",
            "run_integrated",
        ] {
            assert!(events(p, p.task, kind).is_empty(), "{kind}");
        }
        assert_eq!(git(p, &["rev-parse", "main"]), main);
    };
    // The old proposal, sent again by its planner with another plan.
    let mut again = plan.clone();
    again["rationale"] = json!("one more try at the same split");
    assert!(submit(&p, &handle, &[api, docs], Some(proposal), Some(&again)).is_err());
    assert_eq!(
        queue(&p).apply_replacement(p.op, generation, None).unwrap(),
        ApplyOutcome::AlreadyApplied
    );
    check(&p);
    // The replaced run's receipt, written late for the saved head, with
    // the lease a supervisor of the old run still holds and a wrapper that
    // reported its exit.
    let token = LeaseToken::new("the old run supervisor");
    std::fs::write(
        run_dir.join("receipt.json"),
        json!({"run_id": p.run, "result": "succeeded", "commit": head,
               "tests": {"status": "passed", "evidence_or_reason": "late"},
               "e2e": {"status": "not_applicable", "evidence_or_reason": "late"},
               "subagent_review": {"status": "not_applicable", "evidence_or_reason": "late"},
               "summary": "the old worker finished after the park"})
        .to_string(),
    )
    .unwrap();
    sql(
        &p,
        &format!(
            "INSERT OR REPLACE INTO run_leases(run_id, token, pid) VALUES ('{run}', '{token}', 1);
             INSERT OR REPLACE INTO run_processes(run_id, role, pid, exited_at, exit_code)
               VALUES ('{run}', 'wrapper', 1, unixepoch(), 0);",
            run = p.run,
            token = token.as_str()
        ),
    );
    let mut late = queue(&p);
    assert!(late.finish_supervision(&p.run, &token, true).is_err());
    assert!(late.finish_supervision_live(&p.run, &token).is_err());
    assert!(late.restart_validation(&p.run, &token).is_err());
    assert!(late.begin_integration(&p.run, &token, &base()).is_err());
    check(&p);
    sql(
        &p,
        &format!(
            "DELETE FROM run_leases WHERE run_id='{run}';
             DELETE FROM run_processes WHERE run_id='{run}';",
            run = p.run
        ),
    );
    // A pass with the receipt in place takes nothing up either.
    supervise(&p, &backend, &reviewer, 1);
    check(&p);
}

/// The child that takes the saved head, claimed once the runtime applied
/// the replacement, is handed the head the snapshot's ref pins: its run
/// records what it inherits from the replaced run, its prompt names the
/// ref and the commits, and its worktree reaches them. The child that
/// takes nothing waits for it.
#[test]
fn a_child_claimed_after_the_apply_is_handed_the_saved_head() {
    let p = parked(0);
    let head = save_snapshot(&p);
    // The child's session never starts: its parked wrapper is given up at
    // once, after the claim provisioned its run.
    let backend = PlanWorkspace::giving_up_after(Duration::ZERO);
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let api = child(&p, "API", &[]);
    let docs = child(&p, "docs", &[api]);
    let plan = plan(&p, [api, docs]);
    approved(&p, &backend, &reviewer, &[api, docs], &plan);
    for _ in 0..3 {
        if !queue(&p).show(api).unwrap().runs.is_empty() {
            break;
        }
        let _waiting = crate::common::within(crate::common::STEP_LIMIT, "supervise to return");
        let outcome = dagq::runtime::supervise_with_reviewer(
            &p.fx.db,
            &p.fx.repo,
            &backend,
            &p.fx.claude,
            &reviewer,
            &crate::common::service::owned_executable(p.fx.db.parent().unwrap()),
            &options(1, Duration::from_secs(3600)),
        )
        .unwrap();
        let errors = outcome["errors"].as_array().unwrap();
        assert!(
            errors.iter().all(|error| error["message"]
                .as_str()
                .unwrap()
                .contains("did not register")),
            "{outcome}"
        );
    }
    assert_eq!(operation(&p).status, ReplanStatus::Applied);
    let detail = queue(&p).show(api).unwrap();
    assert_eq!(detail.runs.len(), 1, "{:?}", detail.runs);
    let run = detail.runs[0].clone();
    let inherited: Vec<Value> = detail
        .events
        .iter()
        .filter(|e| e.kind.as_str() == "run_inherited" && e.run_id.as_ref() == Some(run.id()))
        .map(|e| e.payload.clone())
        .collect();
    assert_eq!(inherited.len(), 1, "{inherited:?}");
    assert_eq!(inherited[0]["inherit_from_run"], json!(p.run));
    assert_eq!(inherited[0]["replan_operation_id"], json!(p.op));
    assert_eq!(inherited[0]["carry"], "snapshot_head");
    assert_eq!(inherited[0]["head"], json!(head));
    let prompt =
        std::fs::read_to_string(std::path::Path::new(run.run_dir().unwrap()).join("prompt.txt"))
            .unwrap();
    for expected in [
        format!("replan operation {} replaced with this task", p.op),
        format!("kept as {}", pinned_ref(p.op)),
        format!("..{head}"),
    ] {
        assert!(prompt.contains(&expected), "{expected}\n{prompt}");
    }
    let worktree = run.worktree_path().unwrap();
    let reached = std::process::Command::new("git")
        .args([
            "-C",
            worktree,
            "cat-file",
            "-e",
            &format!("{head}^{{commit}}"),
        ])
        .bounded_output()
        .unwrap();
    assert!(reached.status.success(), "{reached:?}");
    assert!(queue(&p).show(docs).unwrap().runs.is_empty());
    assert_eq!(waits_for(&p, docs), vec![api]);
}

/// A replaced task is resolved only once every task its replacement
/// reaches has ended, wherever it belongs now (ADR-t1521-2 decision 5):
/// with a child moved to another goal, the goal neither becomes a goal
/// review's candidate nor closes as achieved until that child lands too.
#[test]
fn a_child_moved_to_another_goal_holds_the_goal_back_until_it_lands() {
    use dagq::application::GoalReviewStore;
    let p = parked(0);
    save_snapshot(&p);
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let (api, docs, plan) = children(&p);
    approved(&p, &backend, &reviewer, &[api, docs], &plan);
    supervise(&p, &backend, &reviewer, 1);
    assert_eq!(operation(&p).status, ReplanStatus::Applied);
    let other = open_goal(&mut queue(&p));
    queue(&p).set_goal(docs, Some(other)).unwrap();
    let landed = |task: TaskId| {
        sql(
            &p,
            &format!("UPDATE tasks SET status='completed' WHERE id={task}"),
        );
    };
    landed(api);
    landed(p.successor);
    let error = queue(&p)
        .close_goal(p.goal, GoalVerdict::Achieved)
        .unwrap_err()
        .to_string();
    assert!(error.contains("ready"), "{error}");
    assert!(
        !queue(&p)
            .goal_review_candidates()
            .unwrap()
            .contains(&p.goal)
    );
    landed(docs);
    assert!(
        queue(&p)
            .goal_review_candidates()
            .unwrap()
            .contains(&p.goal)
    );
    queue(&p).close_goal(p.goal, GoalVerdict::Achieved).unwrap();
}

/// A goal whose children all moved to another goal is left with no task of
/// its own that completed: their landings end its replaced task, but do
/// not count as the completed task a goal review needs, which only the
/// goal's own tasks give. The other goal, where they belong, is reviewed.
#[test]
fn a_goal_whose_children_all_moved_away_is_not_reviewed_on_their_landings() {
    use dagq::application::GoalReviewStore;
    let p = parked(0);
    save_snapshot(&p);
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let (api, docs, plan) = children(&p);
    approved(&p, &backend, &reviewer, &[api, docs], &plan);
    supervise(&p, &backend, &reviewer, 1);
    assert_eq!(operation(&p).status, ReplanStatus::Applied);
    let other = open_goal(&mut queue(&p));
    for child in [api, docs] {
        queue(&p).set_goal(child, Some(other)).unwrap();
    }
    for (task, status) in [
        (api, "completed"),
        (docs, "completed"),
        (p.successor, "canceled"),
    ] {
        sql(
            &p,
            &format!("UPDATE tasks SET status='{status}' WHERE id={task}"),
        );
    }
    let candidates = queue(&p).goal_review_candidates().unwrap();
    assert!(!candidates.contains(&p.goal), "{candidates:?}");
    assert!(candidates.contains(&other), "{candidates:?}");
}

/// A recurring task a replanning replaced holds the next registration of
/// its key back while a task its replacement reaches has not ended, and
/// lets it go once they all have.
#[test]
fn a_replaced_recurring_task_holds_its_key_until_its_children_end() {
    use dagq::domain::recurring::{RecurringDraft, RecurringOutcome};
    let p = parked(0);
    save_snapshot(&p);
    sql(
        &p,
        &format!(
            "INSERT INTO draft_origins(task_id, origin, material, created_at)
             VALUES ({}, 'recurring', '{{\"key\":\"weekly\"}}', 0)",
            p.task
        ),
    );
    let item = RecurringDraft {
        key: "weekly".into(),
        interval_days: 1,
        title: "chore: the weekly check".into(),
        description: "d".into(),
        acceptance: "it is checked".into(),
        verification: vec!["cargo test".into()],
        paths: vec!["src/**".into()],
        evidence: Vec::new(),
        goal: None,
    };
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let (api, docs, plan) = children(&p);
    approved(&p, &backend, &reviewer, &[api, docs], &plan);
    supervise(&p, &backend, &reviewer, 1);
    assert_eq!(status(&p, p.task), TaskStatus::Replaced);
    assert_eq!(
        queue(&p).register_recurring_draft(&item).unwrap(),
        RecurringOutcome::Open { task_id: p.task }
    );
    for task in [api, docs] {
        sql(
            &p,
            &format!("UPDATE tasks SET status='completed' WHERE id={task}"),
        );
    }
    assert!(matches!(
        queue(&p).register_recurring_draft(&item).unwrap(),
        RecurringOutcome::Registered { .. }
    ));
}
