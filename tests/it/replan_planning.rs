//! File-backed transaction rollback, interrupted apply adoption and real
//! Git reachability of the saved head handed to a replacement child.
//! Supervisor decisions are exercised in compose's wiring test layer.

use crate::common::Bounded;
use crate::common::queue::{base, new_task};
use crate::runtime_support::plan_review::{
    Fixture, PlanWorkspace, StubReviewer, fixture, open_goal, options,
};
use anyhow::Result;
use dagq::{
    application::{TaskReads, TaskStore},
    domain::{
        ClaimOutcome, GoalId, NewTask, PlannerId, PlannerOrigin, PlannerOwner, Proposal, ReplanId,
        RequestId, RunId, RunStatus, Submission, TaskAction, TaskId, TaskStatus,
        plan_request::NewPlanRequest,
        replan_apply::ApplyOutcome,
        replan_plan::ReplacementPlan,
        replan_snapshot::{SavedFile, SavedKind, SnapshotManifest, pinned_ref},
        task_replan::{NewReplan, ReplanMove, ReplanOrigin, ReplanStatus},
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
///
/// `between` changes the queue after the request and before the pause's fence.
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
