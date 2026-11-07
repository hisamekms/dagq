//! Replanning through the production supervisor with memdb and fake host
//! ports: planner allocation, replacement review, answers, apply and the
//! resulting task, goal and recurring state. Planner turns are played by
//! the test through the same files and queue records as their wrappers.

use super::{
    agent::Script,
    harness::{Wiring, connect},
};
use crate::application::{Repository, RunFiles};
use crate::{
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
use anyhow::Result;
use serde_json::{Value, json};
use std::cell::Cell;
use std::time::Duration;

/// Task A of a goal (two acceptance items, `cargo test`, paths) whose run
/// is parked for a person's replanning request, and task B that waits for
/// it.
struct Parked {
    fx: Wiring,
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
        worker_mode: Some(crate::domain::worker::WorkerMode::Headless),
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
    let mut queue = fx.queue();
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
    let connection = connect(db);
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

/// The in-process supervisor with `planners` slots for runtime planners.
fn supervise(p: &Parked, reviewer: &StubReviewer, planners: usize) {
    supervise_timed(p, reviewer, planners, Duration::from_secs(3600));
}

/// [`supervise`] with the planner timeout `timeout`.
fn supervise_timed(p: &Parked, reviewer: &StubReviewer, planners: usize, timeout: Duration) {
    if !reviewer.installed.replace(true) {
        p.fx.agent.scripted_jobs(reviewer.verdicts.clone());
    }
    p.fx.supervise_planning(planners, timeout);
}

fn queue(p: &Parked) -> SqliteQueue {
    p.fx.queue()
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
    again: Option<crate::domain::ProposalId>,
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

fn operation(p: &Parked) -> crate::domain::task_replan::ReplanOperation {
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

    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    // The one slot goes to the older request: the operation waits parked.
    supervise(&p, &reviewer, 1);
    assert!(request_planner(&p, p.request).is_none());
    assert_eq!(operation(&p).status, ReplanStatus::Parked);
    let older = queue(&p).planners(false).unwrap()[0].id;
    exit(&p, older);
    // The parked run's reviews: one that passed on its own verdict but
    // that its agents sent back, and one that landed.
    let connection = connect(&p.fx.db);
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
    supervise(&p, &reviewer, 1);
    let (planner, _) = request_planner(&p, p.request).expect("a planner");
    assert_eq!(operation(&p).status, ReplanStatus::Planning);
    // Never a second: another pass and a higher limit leave the one.
    supervise(&p, &reviewer, 3);
    let planners: Vec<_> = queue(&p)
        .planners(true)
        .unwrap()
        .into_iter()
        .filter(|row| row.request_id == Some(p.request))
        .collect();
    assert_eq!(planners.len(), 1, "{planners:?}");
    let prompt = planner_prompt(&p, planner);
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

    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    supervise(&p, &reviewer, 1);
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
    supervise(&p, &reviewer, 1);
    supervise(&p, &reviewer, 1);
    let mut read = queue(&p);
    assert_eq!(
        read.show_proposal(proposal.id()).unwrap().status(),
        ProposalStatus::Accepted
    );
    let approved = events(&p, p.task, "replan_plan_approved");
    assert_eq!(approved.len(), 1, "{approved:?}");
    // The review was given the plan, the conditions it maps and the saved
    // work, with what to check of its meaning.
    let prompts =
        p.fx.agent
            .recovery_prompts()
            .into_iter()
            .map(|(prompt, _)| prompt)
            .collect::<Vec<_>>();
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
    // The replans read model includes the plan and its current problems.
    let brief = queue(&p).replan_brief(p.op).unwrap();
    assert!(brief.problems.is_empty());
    let revision = brief.plan.unwrap();
    assert_eq!(revision.revision, 1);
    assert_eq!(revision.proposal_id, proposal.id());
    assert_eq!(revision.plan.children[0].task_id, api);
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
    supervise(&p, &reviewer, 1);
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

    supervise(&p, &StubReviewer::new(&[]), 1);
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
    connect(&p.fx.db)
        .execute(
            "UPDATE proposals SET revise_count=2 WHERE id=?1",
            [proposal.as_i64()],
        )
        .unwrap();
    supervise(&p, &reviewer, 1);
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

    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    supervise(&p, &reviewer, 1);
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

    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    supervise(&p, &reviewer, 1);
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
    supervise(&p, &reviewer, 1);
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
    supervise(&p, &reviewer, 1);
    supervise(&p, &reviewer, 1);
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

    let reviewer = StubReviewer::new(&[json!({
        "verdict": "concern", "reasons": ["needs a person"], "summary": "doubtful"
    })]);
    supervise(&p, &reviewer, 1);
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
    supervise(&p, &reviewer, 1);
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
    supervise(&p, &reviewer, 1);
    assert!(queue(&p).read_ask(ask.id).unwrap().closed_at.is_some());
    assert_eq!(
        queue(&p).show_proposal(proposal.id()).unwrap().status(),
        ProposalStatus::Revising
    );
    let reasons: String = connect(&p.fx.db)
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
    completed_spike(&p, spike);

    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let spike_request = || {
        connect(&p.fx.db)
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
    supervise(&p, &reviewer, 3);
    let (first, _) = request_planner(&p, p.request).expect("the oldest request's planner");
    assert!(!open(second));
    assert!(!open(spike_request()));
    // Its planner asks a person and its row closes: the goal goes to the
    // next request, the second operation's.
    let ask = scope_ask(&p, &operation(&p).conditions.acceptance[1]);
    queue(&p).close_planner(first, None).unwrap();
    supervise(&p, &reviewer, 3);
    let (next, _) = request_planner(&p, second).expect("the second operation's planner");
    assert!(!open(p.request));
    assert!(!open(spike_request()));
    // The answer would open a new planner of the first request: it waits
    // for the goal too, and stays undelivered.
    queue(&p).answer(ask, "keep").unwrap();
    supervise(&p, &reviewer, 3);
    assert!(!open(p.request));
    assert!(!open(spike_request()));
    // Once that planner declines its request and its row closes, the
    // answer opens the first request's planner, and the Spike's still
    // waits.
    queue(&p)
        .decline_request_choosing(second, "no split is needed", "planner", Some(next), false)
        .unwrap();
    queue(&p).close_planner(next, None).unwrap();
    supervise(&p, &reviewer, 3);
    let opened: Vec<Value> = connect(&p.fx.db)
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
fn revise_planner(p: &Parked, proposal: crate::domain::ProposalId) -> Option<PlannerId> {
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

    let reviewer = StubReviewer::new(&[
        json!({"verdict": "revise", "reasons": ["say why the docs wait"], "summary": "s"}),
    ]);
    supervise(&p, &reviewer, 3);
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
    supervise(&p, &reviewer, 3);
    let (other, _) = request_planner(&p, second).expect("a planner for the other request");
    assert_eq!(
        queue(&p).show_proposal(proposal).unwrap().status(),
        ProposalStatus::Revising
    );
    assert!(revise_planner(&p, proposal).is_none());
    let connection = connect(&p.fx.db);
    connection
        .execute(
            "UPDATE proposals SET revised_at=revised_at-7200 WHERE id=?1",
            [proposal.as_i64()],
        )
        .unwrap();
    supervise_timed(&p, &reviewer, 1, Duration::from_secs(60));
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
    supervise(&p, &reviewer, 3);
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

    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    supervise(&p, &reviewer, 2);
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
    supervise(&p, &reviewer, 2);
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

    let reviewer = StubReviewer::new(&[
        json!({"verdict": "revise", "reasons": ["say why the docs wait"], "summary": "s"}),
    ]);
    supervise(&p, &reviewer, 1);
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
    supervise(&p, &reviewer, 1);
    let planner = revise_planner(&p, proposal).expect("a planner for the revise");
    let op = operation(&p);
    let prompt = planner_prompt(&p, planner);
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

/// Ask through the CLI's dialogue use case as the planner actor, including
/// its authorization and ownership checks.
fn ask_as_planner(p: &Parked, planner: PlannerId, about: &[&str], question: &str) -> AskId {
    let task = about
        .windows(2)
        .find(|w| w[0] == "--task")
        .map(|w| TaskId::new(w[1].parse().unwrap()));
    let options = about
        .windows(2)
        .filter(|w| w[0] == "--option")
        .map(|w| w[1].to_owned())
        .collect();
    let actor = crate::domain::actor::ActorContext::instance(
        crate::domain::actor::ActorRole::Planner,
        planner,
    );
    let mut queue = queue(p).with_actor(actor.clone());
    let mut store = crate::infrastructure::dialogue::DialogueQueue { queue: &mut queue };
    let opened = crate::application::commands::dialogue::Dialogue::new(
        &mut store,
        &actor,
        &crate::domain::StaticPolicy,
    )
    .ask(NewAsk {
        kind: AskKind::PlannerQuestion,
        task_id: task,
        run_id: None,
        question: question.into(),
        options,
        asked_by: String::new(),
        reason_category: AskReason::Scope,
        recommendation: None,
        confidence: None,
        topics: Vec::new(),
        finding_id: None,
        request_id: None,
    })
    .unwrap();
    serde_json::from_value::<crate::domain::AskOutcome>(opened)
        .unwrap()
        .ask
        .id
}

/// Mark `planner` idle again, as a turn of it stopped.
fn stopped(p: &Parked, planner: PlannerId) {
    let dir = crate::infrastructure::location::planners_dir(&p.fx.db).join(planner.to_string());
    // SQLite stamps asks itself. Derive the turn's end from the
    // recorded inputs, without sampling the host's clock or sleeping.
    let queue = queue(p);
    let after = queue
        .asks(crate::application::AskQuery {
            all: true,
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .flat_map(|ask| [Some(ask.created_at), ask.answered_at])
        .flatten()
        .chain([queue.planner(planner).unwrap().created_at])
        .max()
        .unwrap();
    p.fx.files.put(
        &crate::application::planner_idle_marker(&dir),
        std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(u64::try_from(after + 1).unwrap()),
        "{}",
    );
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
    reviewer: &StubReviewer,
) -> (crate::domain::ProposalId, [TaskId; 2], PlannerId) {
    supervise(p, reviewer, 1);
    let (first, handle) = request_planner(p, p.request).unwrap();
    let api = child(p, "API", &[]);
    let docs = child(p, "docs", &[]);
    let proposal = submit(p, &handle, &[api, docs], None, Some(&plan(p, [api, docs])))
        .unwrap()
        .id();
    queue(p).close_planner(first, None).unwrap();
    supervise(p, reviewer, 1);
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

    let reviewer = StubReviewer::new(&[
        json!({"verdict": "revise", "reasons": ["the docs are not needed"], "summary": "s"}),
    ]);
    let (_, _, planner) = revising_replacement(&p, &reviewer);
    let item = operation(&p).conditions.acceptance[1].clone();
    let token = deletion_token(&item);
    let prompt = planner_prompt(&p, planner);
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
    idle(&p, planner);
    queue(&p).answer(asked, &token).unwrap();
    supervise(&p, &reviewer, 1);
    assert!(!exit_requested(&p, planner));
    let requests = turn_requests(&p, planner);
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

    let reviewer = StubReviewer::new(&[
        json!({"verdict": "revise", "reasons": ["the docs are not needed"], "summary": "s"}),
    ]);
    let (proposal, [api, docs], planner) = revising_replacement(&p, &reviewer);
    let item = operation(&p).conditions.acceptance[1].clone();
    let token = deletion_token(&item);
    let (task, docs_text) = (p.task.to_string(), docs.to_string());
    let about_child = ask_as_planner(
        &p,
        planner,
        &["--task", &docs_text, "--option", &token, "--option", "keep"],
        "drop the documents item, asked about the child?",
    );
    idle(&p, planner);
    queue(&p).answer(about_child, &token).unwrap();
    supervise(&p, &reviewer, 1);
    assert_eq!(turn_requests(&p, planner).len(), 1);
    take_turns(&p, planner);
    let asked = ask_as_planner(
        &p,
        planner,
        &["--task", &task, "--option", &token, "--option", "keep"],
        "drop the documents item?",
    );
    stopped(&p, planner);
    supervise(&p, &reviewer, 1);
    assert!(exit_requested(&p, planner));
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
    let launched = p.fx.sessions.planner_launches();
    supervise(&p, &reviewer, 1);
    supervise(&p, &reviewer, 1);
    assert!(revise_planner(&p, proposal).is_none());
    assert_eq!(
        p.fx.sessions.planner_launches(),
        launched,
        "not opened unanswered"
    );
    queue(&p).answer(asked, &token).unwrap();
    supervise(&p, &reviewer, 1);
    supervise(&p, &reviewer, 1);
    let next = revise_planner(&p, proposal).expect("a planner carries the answer");
    assert_ne!(next, planner);
    assert_eq!(
        p.fx.sessions.planner_launches(),
        launched + 1,
        "one planner"
    );
    let prompt = planner_prompt(&p, next);
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
    p.fx.files.create_dir_all(&dir).unwrap();
    let diff = b"diff --git a/notes.txt b/notes.txt\n";
    p.fx.files.write(&dir.join("index.diff"), diff).unwrap();
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
    p.fx.files
        .write(
            &dir.join("manifest.json"),
            &serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    head.1
}

/// The output of `git args` in `p`'s repository, which must succeed.
fn git(p: &Parked, args: &[&str]) -> String {
    match args {
        ["rev-parse", "HEAD" | "main"] => p.fx.git.main().to_string(),
        ["commit-tree", ..] => {
            p.fx.git
                .commit_tree("tree", p.fx.git.main().as_str(), &[])
                .unwrap()
                .to_string()
        }
        ["update-ref", name, head] => {
            p.fx.git.update_ref(name, head).unwrap();
            String::new()
        }
        ["rev-parse", name] => p.fx.git.ref_value(name).unwrap().unwrap().to_string(),
        _ => panic!("unexpected Git operation: {args:?}"),
    }
}

/// Whether the snapshot and the ref pinning `head` are still where the
/// park left them.
fn assert_snapshot_kept(p: &Parked, head: &str) {
    assert!(
        p.fx.files
            .exists(&replans_dir(&p.fx.db).join("snap-1/manifest.json"))
    );
    assert_eq!(git(p, &["rev-parse", &pinned_ref(p.op)]), head);
}

fn sql(p: &Parked, statements: &str) {
    connect(&p.fx.db).execute_batch(statements).unwrap();
}

/// What `task` waits for now, in ID order.
fn waits_for(p: &Parked, task: TaskId) -> Vec<TaskId> {
    connect(&p.fx.db)
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
    reviewer: &StubReviewer,
    tasks: &[TaskId],
    plan: &Value,
) -> crate::domain::ProposalId {
    supervise(p, reviewer, 1);
    let (_, handle) = request_planner(p, p.request).unwrap();
    let proposal = submit(p, &handle, tasks, None, Some(plan)).unwrap();
    hold_apply(p);
    for _ in 0..3 {
        if !events(p, p.task, "replan_plan_approved").is_empty() {
            break;
        }
        supervise(p, reviewer, 1);
    }
    sql(p, "DROP TRIGGER test_holds_the_apply");
    assert_eq!(events(p, p.task, "replan_plan_approved").len(), 1);
    assert_eq!(operation(p).status, ReplanStatus::Reviewing);
    proposal.id()
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

    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let (api, docs, mut plan) = children(&p);
    plan["successors"]
        .as_array_mut()
        .unwrap()
        .push(json!({"task_id": d, "needs": [docs], "reason": "D reads the documents only"}));
    approved(&p, &reviewer, &[api, docs], &plan);
    supervise(&p, &reviewer, 1);
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
    supervise(&p, &reviewer, 1);
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

/// A replacement whose goal's acceptance changed since its pass writes
/// nothing: the plan is stale, so the operation goes back to a planner on
/// a new request with the saved work and the children released, and that
/// planner's choice (here to continue the run) ends it.
#[test]
fn an_apply_of_a_plan_gone_stale_since_its_pass_goes_back_to_a_planner() {
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let stale = parked(0);
    let head = save_snapshot(&stale);
    let (api, docs, plan) = children(&stale);
    approved(&stale, &reviewer, &[api, docs], &plan);
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
    supervise(&stale, &reviewer, 1);
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
        supervise(&stale, &reviewer, 1);
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
    supervise(&stale, &reviewer, 1);
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

    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let other = open_goal(&mut queue(&p));
    let (api, docs, mut plan) = children(&p);
    plan["successors"]
        .as_array_mut()
        .unwrap()
        .push(json!({"task_id": d, "needs": [api, docs]}));
    queue(&p).add_goal_dependency(api, other).unwrap();
    approved(&p, &reviewer, &[api, docs], &plan);
    queue(&p).set_goal(d, Some(other)).unwrap();
    if let Some((planner, _)) = request_planner(&p, p.request) {
        exit(&p, planner);
    }
    supervise(&p, &reviewer, 1);
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
        supervise(&p, &reviewer, 1);
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

    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let (api, docs, plan) = children(&p);
    approved(&p, &reviewer, &[api, docs], &plan);
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
    supervise(&p, &reviewer, 1);
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

    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let (api, docs, plan) = children(&p);
    let other = open_goal(&mut queue(&p));
    queue(&p).add_goal_dependency(api, other).unwrap();
    approved(&p, &reviewer, &[api, docs], &plan);
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
    supervise(&p, &reviewer, 1);
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
    use crate::application::{GoalReviewApply, GoalReviewStore};
    use crate::domain::{
        actor_model::{ActorLaunch, ModelRole},
        goal_review::{GoalReviewDecision, GoalReviewVerdict},
    };
    let p = parked(0);
    save_snapshot(&p);

    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let (api, docs, plan) = children(&p);
    approved(&p, &reviewer, &[api, docs], &plan);
    supervise(&p, &reviewer, 1);
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
            &std::path::PathBuf::from("/wiring/repo").join("goal-reviews"),
            &std::path::PathBuf::from("/wiring/repo"),
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
    supervise(&p, &goal_reviewer, 1);
    // The goal the child moved to is reviewed too, on its own.
    let prompts =
        p.fx.agent
            .recovery_prompts()
            .into_iter()
            .map(|(prompt, _)| prompt)
            .collect::<Vec<_>>();
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

    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let (api, docs, plan) = children(&p);
    let proposal = approved(&p, &reviewer, &[api, docs], &plan);
    let (_, handle) = request_planner(&p, p.request).unwrap();
    let generation = operation(&p).generation;
    // The run as its provisioning left it, with a directory for its
    // receipt.
    let run_dir = crate::infrastructure::location::runs_dir(&p.fx.db).join(p.run.as_str());
    p.fx.files.create_dir_all(&run_dir).unwrap();
    sql(
        &p,
        &format!(
            "UPDATE task_runs SET run_dir='{dir}', receipt_path='{dir}/receipt.json' WHERE id='{run}'",
            dir = run_dir.display(),
            run = p.run
        ),
    );
    supervise(&p, &reviewer, 1);
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
    p.fx.files
        .write(
            &run_dir.join("receipt.json"),
            json!({"run_id": p.run, "result": "succeeded", "commit": head,
               "tests": {"status": "passed", "evidence_or_reason": "late"},
               "e2e": {"status": "not_applicable", "evidence_or_reason": "late"},
               "subagent_review": {"status": "not_applicable", "evidence_or_reason": "late"},
               "summary": "the old worker finished after the park"})
            .to_string()
            .as_bytes(),
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
    supervise(&p, &reviewer, 1);
    check(&p);
}

/// A replaced task is resolved only once every task its replacement
/// reaches has ended, wherever it belongs now (ADR-t1521-2 decision 5):
/// with a child moved to another goal, the goal neither becomes a goal
/// review's candidate nor closes as achieved until that child lands too.
#[test]
fn a_child_moved_to_another_goal_holds_the_goal_back_until_it_lands() {
    use crate::application::GoalReviewStore;
    let p = parked(0);
    save_snapshot(&p);

    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let (api, docs, plan) = children(&p);
    approved(&p, &reviewer, &[api, docs], &plan);
    supervise(&p, &reviewer, 1);
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
    use crate::application::GoalReviewStore;
    let p = parked(0);
    save_snapshot(&p);

    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let (api, docs, plan) = children(&p);
    approved(&p, &reviewer, &[api, docs], &plan);
    supervise(&p, &reviewer, 1);
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
    use crate::domain::recurring::{RecurringDraft, RecurringOutcome};
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

    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let (api, docs, plan) = children(&p);
    approved(&p, &reviewer, &[api, docs], &plan);
    supervise(&p, &reviewer, 1);
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

/// A replaced task whose replacement record is empty, names a task that is
/// not there or loops is never resolved (ADR-t1521-2 decision 5): with its
/// children landed, each way to break the record keeps the goal from a goal
/// review's start, stales the achieved verdict of a review begun before it
/// broke, refuses `goal close --verdict achieved`, holds the task's
/// recurring key and stands in `status` (the inbox) on the replaced task;
/// the walk ends on a loop. One apply serves every way: the record is
/// broken and made whole again in turn, and whole, the replacement is
/// resolved. An `achieved` answer to an `approve_goal` ask given after the
/// record broke is left to the inbox.
#[test]
fn a_broken_replacement_record_holds_the_goal_and_its_key_and_is_shown() {
    use crate::application::{GoalReviewApply, GoalReviewStore};
    use crate::domain::{
        actor_model::{ActorLaunch, ModelRole},
        goal_review::{GoalReviewDecision, GoalReviewVerdict},
        recurring::{RecurringDraft, RecurringOutcome},
    };
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

    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let (api, docs, plan) = children(&p);
    approved(&p, &reviewer, &[api, docs], &plan);
    supervise(&p, &reviewer, 1);
    assert_eq!(operation(&p).status, ReplanStatus::Applied);
    for task in [api, docs, p.successor] {
        sql(
            &p,
            &format!("UPDATE tasks SET status='completed' WHERE id={task}"),
        );
    }
    let candidate = || {
        queue(&p)
            .goal_review_candidates()
            .unwrap()
            .contains(&p.goal)
    };
    let shown = || -> Vec<Value> {
        p.fx.status()["attention"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["kind"] == "replacement_unresolved")
            .cloned()
            .collect()
    };
    assert!(candidate());
    assert!(shown().is_empty());
    let op = p.op;
    let task = p.task;
    let missing = 999_999;
    // The record broken, the task it is shown on, why, and the record
    // made whole again.
    let broken = [
        (
            format!(
                "CREATE TABLE kept AS SELECT * FROM replan_replacements WHERE operation_id={op};
                 DELETE FROM replan_replacements WHERE operation_id={op};"
            ),
            task,
            "its replacement names no task".to_owned(),
            "INSERT INTO replan_replacements SELECT * FROM kept; DROP TABLE kept;".to_owned(),
        ),
        (
            format!("UPDATE replan_replacements SET to_task_id={missing} WHERE to_task_id={docs};"),
            task,
            format!("its replacement names task {missing}, which is not there"),
            format!("UPDATE replan_replacements SET to_task_id={docs} WHERE to_task_id={missing};"),
        ),
        (
            format!(
                "UPDATE tasks SET status='replaced' WHERE id={api};
                 INSERT INTO replan_replacements(operation_id, from_task_id, to_task_id)
                 VALUES ({op}, {api}, {task});"
            ),
            api,
            format!("its replacements loop back to task {task}"),
            format!(
                "DELETE FROM replan_replacements WHERE from_task_id={api};
                 UPDATE tasks SET status='completed' WHERE id={api};"
            ),
        ),
    ];
    let token = LeaseToken::new("goal-review-owner");
    for (break_it, on, why, restore) in broken {
        // A review begun on the whole record, then the record broken.
        let mut q = queue(&p);
        let job = q
            .begin_goal_review(
                p.goal,
                &token,
                &std::path::PathBuf::from("/wiring/repo").join("goal-reviews"),
                &std::path::PathBuf::from("/wiring/repo"),
                &ActorLaunch::default_of(ModelRole::GoalReview),
                &|_| false,
            )
            .unwrap()
            .unwrap();
        sql(&p, &break_it);
        assert!(!candidate(), "{why}");
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
        assert!(applied.stale, "{why}: {applied:?}");
        assert!(!q.show_goal(p.goal).unwrap().goal.is_closed(), "{why}");
        let error = q
            .close_goal(p.goal, GoalVerdict::Achieved)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("cannot be closed as achieved") && error.contains(&why),
            "{error}"
        );
        assert!(
            q.begin_goal_review(
                p.goal,
                &token,
                &std::path::PathBuf::from("/wiring/repo").join("goal-reviews"),
                &std::path::PathBuf::from("/wiring/repo"),
                &ActorLaunch::default_of(ModelRole::GoalReview),
                &|_| false,
            )
            .unwrap()
            .is_none(),
            "{why}"
        );
        drop(q);
        assert_eq!(
            queue(&p).register_recurring_draft(&item).unwrap(),
            RecurringOutcome::Open { task_id: task },
            "{why}"
        );
        let entries = shown();
        assert_eq!(entries.len(), 1, "{why}: {entries:?}");
        assert_eq!(entries[0]["task_id"], json!(on), "{why}");
        assert_eq!(entries[0]["next"], "goal review by hand", "{why}");
        let error = entries[0]["last_error"].as_str().unwrap();
        assert!(
            error.contains(&format!("goal {}", p.goal)) && error.contains(&why),
            "{error}"
        );
        sql(&p, &restore);
        assert!(candidate(), "{why}");
        assert!(shown().is_empty(), "{why}");
    }
    // An `approve_goal` ask opened on the whole record, then the record
    // broken: an `achieved` answer is not the runtime's to apply, so it is
    // the inbox's (`runtime_delivers: false`), and nothing closes the goal.
    let asking = StubReviewer::new(&[json!({"verdict": "ask", "summary": "close it?"})]);
    supervise(&p, &asking, 1);
    let ask = queue(&p)
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.kind == crate::domain::AskKind::ApproveGoal && ask.closed_at.is_none())
        .unwrap();
    sql(
        &p,
        &format!("UPDATE replan_replacements SET to_task_id={missing} WHERE to_task_id={docs};"),
    );
    queue(&p).answer(ask.id, "achieved").unwrap();
    let answered = events(&p, ask.task_id.unwrap(), "ask_answered");
    assert_eq!(answered.last().unwrap()["runtime_delivers"], false);
    assert!(
        !queue(&p)
            .goal_answers()
            .unwrap()
            .iter()
            .any(|answer| answer.id == ask.id)
    );
    assert!(queue(&p).decide_goal(ask.id).unwrap().is_none());
    assert!(queue(&p).read_ask(ask.id).unwrap().closed_at.is_none());
    assert!(!queue(&p).show_goal(p.goal).unwrap().goal.is_closed());
    sql(
        &p,
        &format!("UPDATE replan_replacements SET to_task_id={docs} WHERE to_task_id={missing};"),
    );
    assert!(matches!(
        queue(&p).register_recurring_draft(&item).unwrap(),
        RecurringOutcome::Registered { .. }
    ));
    queue(&p).close_goal(p.goal, GoalVerdict::Achieved).unwrap();
}

/// Scripted plan and goal review answers; installed on the fake once so
/// repeated supervisor passes consume one sequence of verdicts.
struct StubReviewer {
    verdicts: Vec<Value>,
    installed: Cell<bool>,
}
impl StubReviewer {
    fn new(verdicts: &[Value]) -> Self {
        Self {
            verdicts: verdicts.to_vec(),
            installed: Cell::new(false),
        }
    }
}

fn fixture() -> Wiring {
    let fx = Wiring::new(
        "replan-planning",
        Script {
            receipts: Vec::new(),
            verdicts: Vec::new(),
        },
    );
    assert_eq!(
        fx.queue().add(new_task("blocker")).unwrap().id(),
        TaskId::new(1)
    );
    fx
}

fn base() -> crate::domain::CommitSha {
    crate::domain::CommitSha::try_from("0000000000000000000000000000000000000001").unwrap()
}

fn open_goal(queue: &mut SqliteQueue) -> GoalId {
    queue
        .add_goal(crate::domain::NewGoal {
            priority: Default::default(),
            title: "tidy the queue".into(),
            description: "d".into(),
            acceptance: "every draft is decided".into(),
            constraints: "no new tables".into(),
            doc: None,
            draft: false,
            tags: Vec::new(),
        })
        .unwrap()
        .id()
}

fn planner_prompt(p: &Parked, planner: PlannerId) -> String {
    p.fx.files
        .read_to_string(
            &crate::infrastructure::location::planners_dir(&p.fx.db)
                .join(planner.to_string())
                .join("prompt.txt"),
        )
        .unwrap()
}

fn exit_requested(p: &Parked, planner: PlannerId) -> bool {
    p.fx.files.exists(&crate::domain::turn::exit_path(
        &crate::infrastructure::location::planners_dir(&p.fx.db).join(planner.to_string()),
    ))
}

fn idle(p: &Parked, planner: PlannerId) {
    queue(p)
        .register_planner_wrapper(planner, std::process::id())
        .unwrap();
    queue(p)
        .register_planner_agent(planner, std::process::id(), std::process::id())
        .unwrap();
    stopped(p, planner);
}

fn turn_requests(p: &Parked, planner: PlannerId) -> Vec<Value> {
    let dir = crate::infrastructure::location::planners_dir(&p.fx.db).join(planner.to_string());
    let mut requests: Vec<Value> =
        p.fx.files
            .read_dir(&crate::domain::turn::turns_dir(&dir))
            .unwrap_or_default()
            .into_iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|s| s.to_str())
                    .and_then(crate::domain::turn::request_seq)
                    .is_some()
                    && path.extension().is_some_and(|s| s == "json")
            })
            .map(|path| serde_json::from_str(&p.fx.files.read_to_string(&path).unwrap()).unwrap())
            .collect();
    requests.sort_by_key(|request| request["seq"].as_u64());
    requests
}

fn take_turns(p: &Parked, planner: PlannerId) {
    let dir = crate::infrastructure::location::planners_dir(&p.fx.db).join(planner.to_string());
    for request in turn_requests(p, planner) {
        let seq = request["seq"].as_u64().unwrap();
        let path = crate::domain::turn::request_path(&dir, seq);
        if !p.fx.files.exists(&path) {
            continue;
        }
        p.fx.files
            .rename(&path, &crate::domain::turn::taken_path(&dir, seq))
            .unwrap();
        for (kind, payload) in [
            (
                crate::domain::EventKind::TurnStarted,
                json!({"planner_id": planner, "turn": seq + 1, "request": seq, "what": request["what"]}),
            ),
            (
                crate::domain::EventKind::TurnFinished,
                json!({"planner_id": planner, "turn": seq + 1, "outcome": "succeeded"}),
            ),
        ] {
            queue(p).record_queue_event(kind, payload).unwrap();
        }
    }
    stopped(p, planner);
}

fn new_task(title: &str) -> NewTask {
    NewTask {
        title: title.into(),
        description: "A small development task".into(),
        acceptance: "The regression test passes".into(),
        verification_commands: vec!["cargo test".into()],
        required_evidence: Vec::new(),
        paths: Vec::new(),
        priority: Default::default(),
        change: None,
        dependencies: vec![],
        goal_dependencies: Vec::new(),
        goal_id: None,
        context: String::new(),
        provider: None,
        worker_mode: Some(crate::domain::worker::WorkerMode::Interactive),
        wait_for_build: false,
        execution_class: Default::default(),
    }
}

fn completed_spike(p: &Parked, task: TaskId) {
    let connection = connect(&p.fx.db);
    let goal = Some(p.goal);
    let verdict = "holds";
    connection
        .execute(
            "UPDATE tasks SET execution_class='spike', status='completed', goal_id=?2 WHERE id=?1",
            rusqlite::params![task.as_i64(), goal.map(GoalId::as_i64)],
        )
        .unwrap();
    let receipt = json!({"receipt": {
        "summary": format!("measured: {verdict}"),
        "follow_ups": [{"title": format!("tidy after {task}"), "description": "d", "category": "improvement"}],
        "spike_result": {
            "verdict": verdict,
            "grounds": format!("grounds of {task}"),
            "evidence": "docs/plans/x.md",
            "conditions": {"commit": "0123abc", "tools": "cargo 1.90",
                "provider": "claude headless worker", "environment": "macOS"},
            "spent": "40 minutes",
        },
    }});
    connection
        .execute(
            "INSERT INTO run_events(task_id, kind, payload) VALUES (?1, 'integration_receipt', ?2)",
            rusqlite::params![task.as_i64(), receipt.to_string()],
        )
        .unwrap();
}
