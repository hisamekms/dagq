//! Bundles of drafts (ADR-t807-1): the drafts the same piece of work made
//! that wait at once (the follow_ups of one run's receipt, the gaps of one
//! goal review) get one planner of the runtime's, whose prompt shows them
//! all; what it leaves undecided makes the next bundle, a draft is
//! exhausted after its third planner, the runtime's limits on submitting
//! stay per draft, and the bundle, its key and what became of each draft
//! are recorded and read by `show`, `planners`, `events --run` and `stats`.

use crate::common::cli::{ok, submit_from};
use crate::plan_review::{
    Fixture, PlanWorkspace, StubReviewer, add, events, fixture, open_goal, options, planner_prompt,
    runtime_draft, supervise_with,
};
use dagq::{
    application::TaskStore,
    domain::{DraftOrigin, PlannerId, PlannerOrigin, PlannerSession, Priority, TaskId},
    infrastructure::sqlite::SqliteQueue,
};
use serde_json::{Value, json};
use std::time::Duration;

fn pass(fx: &Fixture, backend: &PlanWorkspace, reviewer: &StubReviewer) {
    supervise_with(
        fx,
        backend,
        reviewer,
        &options(3, Duration::from_secs(3600)),
    );
}

fn follow_up(
    queue: &mut SqliteQueue,
    title: &str,
    goal: dagq::domain::GoalId,
    source: TaskId,
    run: &str,
    index: i64,
) -> TaskId {
    runtime_draft(
        queue,
        title,
        Some(goal),
        DraftOrigin::FollowUp,
        json!({"source_task_id": source.as_i64(), "source_run_id": run, "index": index}),
    )
}

/// The planner of the runtime's working on `draft`, if one is open.
fn planner_of(queue: &SqliteQueue, draft: TaskId) -> Option<PlannerSession> {
    queue
        .planners(false)
        .unwrap()
        .into_iter()
        .find(|p| p.origin == PlannerOrigin::Runtime && p.draft_task_id == Some(draft))
}

/// End `planner`'s session as if its agent exited, and run two passes: one
/// closes it, the next opens what follows.
fn end(fx: &Fixture, backend: &PlanWorkspace, reviewer: &StubReviewer, planner: PlannerId) {
    let queue = SqliteQueue::open(&fx.db).unwrap();
    queue.register_planner_wrapper(planner, 1).unwrap();
    queue.planner_exited(planner, 1, 0).unwrap();
    pass(fx, backend, reviewer);
    pass(fx, backend, reviewer);
}

fn ids(value: &Value) -> Vec<i64> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect()
}

/// One run's three follow_ups get one planner whose prompt shows the three
/// and the source once; another run's follow_up and a goal review's gap are
/// bundles of their own; a follow_up of the first run registered while its
/// bundle's planner works waits for it.
#[test]
fn one_runs_follow_ups_are_one_bundle_for_one_planner() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let source = add(&mut queue, "source", &[], Priority::Normal);
    let a: Vec<TaskId> = (0..3)
        .map(|i| follow_up(&mut queue, &format!("a{i}"), goal, source, "run-a", i))
        .collect();
    let b = follow_up(&mut queue, "b0", goal, source, "run-b", 0);
    let gap = runtime_draft(
        &mut queue,
        "gap",
        Some(goal),
        DraftOrigin::GoalGap,
        json!({"goal_id": goal.as_i64(), "goal_review_id": 7, "criterion": "c", "summary": "s"}),
    );
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let backend = PlanWorkspace::default();
    pass(&fx, &backend, &reviewer);

    let planners = queue.planners(false).unwrap();
    assert_eq!(
        planners.iter().map(|p| p.draft_task_id).collect::<Vec<_>>(),
        [Some(a[0]), Some(b), Some(gap)],
        "{planners:?}"
    );
    let bundle = &planners[0];
    let prompt = planner_prompt(&fx.db, bundle.id);
    for expected in [
        format!("draft tasks {}, {}, {}", a[0], a[1], a[2]),
        "(source_run_id run-a)".to_owned(),
        "a0: found outside the task".to_owned(),
        "a1: found outside the task".to_owned(),
        "a2: found outside the task".to_owned(),
        format!("## Draft {} (planner 1 for it)", a[2]),
        "--duplicate-of <the one you keep>".to_owned(),
        "`dagq dependency add`".to_owned(),
        "`dagq submit ID ID ...`".to_owned(),
        format!("- task {b} (draft): b0"),
    ] {
        assert!(prompt.contains(&expected), "{expected}\n{prompt}");
    }
    assert_eq!(prompt.matches("### Source task").count(), 1, "{prompt}");
    assert!(
        !prompt.contains(&format!("- task {} (draft)", a[1])),
        "{prompt}"
    );
    for (attempt_of, &draft) in a.iter().enumerate() {
        let opened = events(&mut queue, draft, "draft_planner_opened");
        assert_eq!(opened.len(), 1, "{attempt_of}: {opened:?}");
        assert_eq!(opened[0]["planner_id"], bundle.id.as_i64());
        assert_eq!(opened[0]["attempt"], 1);
        assert_eq!(
            ids(&opened[0]["members"]),
            a.iter().map(|t| t.as_i64()).collect::<Vec<_>>()
        );
        assert_eq!(
            opened[0]["bundle_key"],
            json!({"kind": "source_run_id", "value": "run-a"})
        );
        assert_eq!(opened[0]["source_run_id"], "run-a");
    }
    let opened = events(&mut queue, gap, "draft_planner_opened");
    assert_eq!(
        opened[0]["bundle_key"],
        json!({"kind": "goal_review_id", "value": "7"})
    );
    assert_eq!(ids(&opened[0]["members"]), [gap.as_i64()]);

    // A follow_up of run-a registered late waits while the bundle works.
    let late = follow_up(&mut queue, "a3", goal, source, "run-a", 3);
    pass(&fx, &backend, &reviewer);
    assert!(events(&mut queue, late, "draft_planner_opened").is_empty());
    assert_eq!(queue.planners(false).unwrap().len(), 3);

    // `planners` shows each planner's bundle.
    let listed = dagq::compose::planners(&fx.db, &backend, false).unwrap();
    let first = &listed["planners"][0]["bundle"];
    assert_eq!(first["key_kind"], "source_run_id");
    assert_eq!(first["key_value"], "run-a");
    assert_eq!(
        first["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["task_id"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        a.iter().map(|t| t.as_i64()).collect::<Vec<_>>()
    );
    assert!(first["members"][0]["outcome"].is_null());
}

/// The bundle's planner closes one draft as a duplicate of another, submits
/// that one and leaves the third: the outcomes are recorded, the third and a
/// late one make the next bundle, and a draft is exhausted after its third
/// planner. The limits on submitting stay per draft. `show`, `events --run`
/// and `stats` read it all.
#[test]
fn what_a_bundle_leaves_undecided_makes_the_next_and_the_outcomes_are_read() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let source = add(&mut queue, "source", &[], Priority::Normal);
    let a: Vec<TaskId> = (0..3)
        .map(|i| follow_up(&mut queue, &format!("a{i}"), goal, source, "run-a", i))
        .collect();
    // Three follow-ups from a person: not submitted without a person's
    // adopt, in a bundle too.
    queue.set_follow_up_depth(a[2], 3).unwrap();
    let mine = add(&mut queue, "mine", &[], Priority::Normal);
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let backend = PlanWorkspace::default();
    pass(&fx, &backend, &reviewer);
    let first = planner_of(&queue, a[0]).unwrap();
    let workspace = first.workspace_id.clone().unwrap();

    let refused = submit_from(
        &fx.db,
        Some(&workspace),
        Some("runtime"),
        &[&a[2].to_string()],
    );
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("planner_question"),
        "{refused:?}"
    );
    queue.cancel_duplicate(a[0], a[1]).unwrap();
    // It waits on the fixture's blocker once ready, so no run starts.
    queue.add_dependency(a[1], TaskId::new(1)).unwrap();
    let submitted = submit_from(
        &fx.db,
        Some(&workspace),
        Some("runtime"),
        &[&a[1].to_string()],
    );
    assert!(submitted.status.success(), "{submitted:?}");
    let adopted = events(&mut queue, a[1], "follow_up_adopted");
    assert_eq!(adopted[0]["planner_id"], first.id.as_i64());
    assert_eq!(
        adopted[0]["bundle_key"],
        json!({"kind": "source_run_id", "value": "run-a"})
    );
    let canceled = events(&mut queue, a[0], "task_status_changed");
    assert_eq!(canceled[0]["duplicate_of"], a[1].as_i64());
    assert_eq!(canceled[0]["source_run_id"], "run-a");
    let late = follow_up(&mut queue, "a3", goal, source, "run-a", 3);

    // It ends: each draft's outcome is recorded with the planner.
    end(&fx, &backend, &reviewer, first.id);
    assert!(queue.planner(first.id).unwrap().closed_at.is_some());
    let settled: Vec<(String, Value)> = a
        .iter()
        .map(|&t| {
            let e = events(&mut queue, t, "draft_planner_settled");
            assert_eq!(e.len(), 1, "{e:?}");
            assert_eq!(e[0]["planner_id"], first.id.as_i64());
            (e[0]["outcome"].as_str().unwrap().to_owned(), e[0].clone())
        })
        .collect();
    assert_eq!(settled[0].0, "duplicate");
    assert_eq!(settled[0].1["duplicate_of"], a[1].as_i64());
    assert_eq!(settled[1].0, "submitted");
    assert!(settled[1].1["proposal_id"].is_i64());
    assert_eq!(settled[2].0, "undecided");

    // The undecided draft and the late one are the next bundle.
    let second = planner_of(&queue, a[2]).unwrap();
    let opened = events(&mut queue, a[2], "draft_planner_opened");
    assert_eq!(opened.len(), 2);
    assert_eq!(opened[1]["attempt"], 2);
    assert_eq!(ids(&opened[1]["members"]), [a[2].as_i64(), late.as_i64()]);
    assert_eq!(
        events(&mut queue, late, "draft_planner_opened")[0]["attempt"],
        1
    );
    let prompt = planner_prompt(&fx.db, second.id);
    assert!(
        prompt.contains(&format!("## Draft {} (planner 2 for it)", a[2])),
        "{prompt}"
    );

    // Each ends undecided: the older draft is exhausted after its third
    // planner, the late one after its own third.
    end(&fx, &backend, &reviewer, second.id);
    let third = planner_of(&queue, a[2]).unwrap();
    end(&fx, &backend, &reviewer, third.id);
    assert_eq!(events(&mut queue, a[2], "draft_planner_opened").len(), 3);
    assert_eq!(events(&mut queue, a[2], "draft_planner_exhausted").len(), 1);
    let fourth = planner_of(&queue, late).unwrap();
    assert_eq!(
        ids(&events(&mut queue, late, "draft_planner_opened")[2]["members"]),
        [late.as_i64()]
    );
    end(&fx, &backend, &reviewer, fourth.id);
    assert_eq!(events(&mut queue, late, "draft_planner_exhausted").len(), 1);
    assert!(queue.planners(false).unwrap().is_empty());

    // `show` of a draft: where it came from and the bundles that took it.
    let shown = ok(&fx.db, &["show", &a[2].to_string()]);
    let origin = &shown["origin"];
    assert_eq!(origin["origin"], "follow_up");
    assert_eq!(origin["source_task_id"], source.as_i64());
    assert_eq!(origin["source_run_id"], "run-a");
    assert_eq!(origin["index"], 2);
    assert_eq!(
        origin["bundle_key"],
        json!({"kind": "source_run_id", "value": "run-a"})
    );
    let bundles = origin["bundles"].as_array().unwrap();
    assert_eq!(bundles.len(), 3);
    assert_eq!(bundles[0]["planner_id"], first.id.as_i64());
    let outcomes: Vec<(i64, Value)> = bundles[0]["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["task_id"].as_i64().unwrap(), m["outcome"].clone()))
        .collect();
    assert_eq!(
        outcomes,
        [
            (a[0].as_i64(), json!("duplicate")),
            (a[1].as_i64(), json!("submitted")),
            (a[2].as_i64(), json!("undecided")),
        ]
    );
    // A person's task has none.
    assert!(ok(&fx.db, &["show", &mine.to_string()])["origin"].is_null());
    // `show` of the source: the drafts its run's receipt proposed.
    let drafts: Vec<(i64, String, String)> =
        ok(&fx.db, &["show", &source.to_string()])["follow_up_drafts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| {
                (
                    d["task_id"].as_i64().unwrap(),
                    d["run_id"].as_str().unwrap().to_owned(),
                    d["status"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
    assert_eq!(
        drafts,
        [
            (a[0].as_i64(), "run-a".into(), "canceled".into()),
            (a[1].as_i64(), "run-a".into(), "ready".into()),
            (a[2].as_i64(), "run-a".into(), "draft".into()),
            (late.as_i64(), "run-a".into(), "draft".into()),
        ]
    );

    // `events --run` leads from the run to each draft's end.
    let listed = ok(
        &fx.db,
        &[
            "events", "--full", "--all", "--limit", "1000", "--run", "run-a",
        ],
    );
    let kinds: Vec<(i64, String)> = listed["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["task_id"].as_i64().unwrap(),
                e["kind"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    for expected in [
        (a[0].as_i64(), "draft_planner_opened"),
        (a[0].as_i64(), "task_status_changed"),
        (a[0].as_i64(), "draft_planner_settled"),
        (a[1].as_i64(), "follow_up_adopted"),
        (a[2].as_i64(), "draft_planner_settled"),
        (late.as_i64(), "draft_planner_opened"),
    ] {
        assert!(
            kinds.contains(&(expected.0, expected.1.to_owned())),
            "{expected:?}: {kinds:?}"
        );
    }

    // `stats` counts the bundles and their sizes.
    let flow = &ok(&fx.db, &["stats", "--full"])["draft_flow"];
    assert_eq!(flow["bundles"], 4, "{flow}");
    assert_eq!(
        flow["bundle_sizes"],
        json!({"1": 1, "2": 2, "3": 1}),
        "{flow}"
    );
}

/// A `planner_question` stays per draft: one about a draft of the bundle
/// other than its first holds the bundle's planner from exiting and its
/// answer is typed into that planner's workspace.
#[test]
fn a_question_about_any_draft_of_the_bundle_goes_to_its_planner() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let source = add(&mut queue, "source", &[], Priority::Normal);
    let a: Vec<TaskId> = (0..2)
        .map(|i| follow_up(&mut queue, &format!("a{i}"), goal, source, "run-a", i))
        .collect();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let backend = PlanWorkspace::default();
    pass(&fx, &backend, &reviewer);
    let planner = planner_of(&queue, a[0]).unwrap();
    let asked = queue
        .ask(dagq::domain::NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: dagq::domain::AskKind::PlannerQuestion,
            task_id: Some(a[1]),
            run_id: None,
            question: "is a1 in the goal?".into(),
            options: vec!["adopt".into(), "cancel".into(), "keep_draft".into()],
            asked_by: "planner".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
        })
        .unwrap()
        .ask;
    crate::plan_review::idle(&queue, &fx.db, planner.id);
    pass(&fx, &backend, &reviewer);
    assert!(backend.exits.lock().unwrap().is_empty());
    queue.answer(asked.id, "keep_draft").unwrap();
    pass(&fx, &backend, &reviewer);
    let workspace = planner.workspace_id.clone().unwrap();
    assert_eq!(
        backend.texts(),
        [(workspace, format!("answer to ask {}: keep_draft", asked.id))]
    );
    assert_eq!(
        events(&mut queue, a[1], "planner_answer_claimed")[0]["planner_id"],
        planner.id.as_i64()
    );
    // No planner of its own is opened for the draft asked about.
    assert_eq!(queue.planners(false).unwrap().len(), 1);
}
