//! Goal review (ADR-0047 decision 43) through the supervisor loop, with the
//! headless goal review played by the plan review tests' stub provider,
//! which prints a scripted verdict. The goal's tasks are completed in the
//! queue directly; no run is claimed.

use crate::plan_review::{
    Fixture, PlanWorkspace, StubReviewer, add, fixture, options, supervise_with,
};

use dagq::{
    application::{GoalReviewStore, TaskStore},
    domain::{
        AskKind, AskReason, DraftOrigin, GoalId, GoalVerdict, NewGoal, Priority, TaskAction,
        TaskId, TaskStatus,
    },
    infrastructure::sqlite::SqliteQueue,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::time::Duration;

/// Supervise once without planners of the runtime's, so a gap's draft
/// stays as it is registered.
fn supervise(fx: &Fixture, reviewer: &StubReviewer) -> Value {
    supervise_with(
        fx,
        &PlanWorkspace::default(),
        reviewer,
        &options(0, Duration::from_secs(3600)),
    )
}

/// An open goal with two tasks, one completed and one canceled.
fn goal_done(fx: &Fixture) -> (GoalId, TaskId) {
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = queue
        .add_goal(NewGoal {
            title: "faster landings".into(),
            description: "land in half the time".into(),
            acceptance: "(1) the median landing is under 5 minutes (2) docs say how".into(),
            constraints: String::new(),
            doc: None,
            draft: false,
        })
        .unwrap()
        .id();
    let done = add(&mut queue, "cache the build", &[], Priority::Normal);
    let dropped = add(&mut queue, "drop the lock", &[], Priority::Normal);
    for task in [done, dropped] {
        queue.set_goal(task, Some(goal)).unwrap();
    }
    set_status(fx, done, TaskStatus::Completed);
    queue.transition(dropped, TaskAction::Cancel).unwrap();
    (goal, done)
}

fn set_status(fx: &Fixture, task: TaskId, status: TaskStatus) {
    Connection::open(&fx.db)
        .unwrap()
        .execute(
            "UPDATE tasks SET status=?2 WHERE id=?1",
            rusqlite::params![task.as_i64(), status.as_str()],
        )
        .unwrap();
}

fn goal_events(queue: &mut SqliteQueue, goal: GoalId, kind: &str) -> Vec<Value> {
    queue
        .show_goal(goal)
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.payload)
        .collect()
}

fn gaps_verdict(title: &str) -> Value {
    json!({
        "verdict": "gaps",
        "criteria": [
            {"criterion": "(1) median under 5 minutes", "met": true, "evidence": ["stats"]},
            {"criterion": "(2) docs", "met": false, "evidence": []},
        ],
        "gaps": [{"title": title, "description": "document the cache", "criterion": "(2) docs"}],
        "summary": "the docs are missing",
    })
}

#[test]
fn an_achieved_goal_is_closed_with_its_evidence() {
    let fx = fixture();
    let (goal, done) = goal_done(&fx);
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "achieved",
        "criteria": [
            {"criterion": "(1) median under 5 minutes", "met": true, "evidence": ["dagq stats: 4m"]},
            {"criterion": "(2) docs", "met": true, "evidence": ["docs/landing.md"]},
        ],
        "summary": "both items landed",
    })]);
    supervise(&fx, &reviewer);
    let prompts = reviewer.prompts();
    assert_eq!(prompts.len(), 1, "one review of the goal");
    assert!(prompts[0].contains("the median landing is under 5 minutes"));
    assert!(prompts[0].contains("cache the build"));
    assert!(prompts[0].contains(&format!("\"id\":{done}")));

    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let detail = queue.show_goal(goal).unwrap();
    assert_eq!(detail.goal.verdict(), Some(GoalVerdict::Achieved));
    let closed = goal_events(&mut queue, goal, "goal_closed");
    assert_eq!(closed[0]["by"], "goal_review");
    assert_eq!(closed[0]["reason"], "both items landed");
    let finished = goal_events(&mut queue, goal, "goal_review_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["decision"], "achieved");
    assert_eq!(finished[0]["criteria"][1]["evidence"][0], "docs/landing.md");
    assert_eq!(
        goal_events(&mut queue, goal, "goal_review_started").len(),
        1
    );

    // A closed goal is no candidate.
    let again = StubReviewer::new(&[json!({"verdict": "achieved"})]);
    supervise(&fx, &again);
    assert!(again.prompts().is_empty());
}

#[test]
fn gaps_become_drafts_of_the_open_goal_and_a_review_waits_for_them() {
    let fx = fixture();
    let (goal, _) = goal_done(&fx);
    let reviewer = StubReviewer::new(&[gaps_verdict("document the cache")]);
    supervise(&fx, &reviewer);
    assert_eq!(reviewer.prompts().len(), 1);

    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let detail = queue.show_goal(goal).unwrap();
    assert!(!detail.closed, "the goal stays open");
    let finished = goal_events(&mut queue, goal, "goal_review_finished");
    let gap = TaskId::new(finished[0]["gap_tasks"][0].as_i64().unwrap());
    let task = queue.show(gap).unwrap().task;
    assert_eq!(task.status(), TaskStatus::Draft);
    assert_eq!(task.title(), "document the cache");
    assert_eq!(task.goal_id(), Some(goal));
    assert!(task.description().contains("(2) docs"));
    let (origin, material) = queue.draft_origin(gap).unwrap().unwrap();
    assert_eq!(origin, DraftOrigin::GoalGap);
    assert_eq!(material["goal_id"], goal.as_i64());
    assert_eq!(material["summary"], "the docs are missing");

    // The draft keeps the goal from review; so do unchanged tasks.
    assert!(queue.goal_review_candidates().unwrap().is_empty());
    let idle = StubReviewer::new(&[json!({"verdict": "achieved"})]);
    supervise(&fx, &idle);
    assert!(idle.prompts().is_empty());

    // Once the gap's task ended, the goal is reviewed again.
    set_status(&fx, gap, TaskStatus::Completed);
    let next = StubReviewer::new(&[json!({"verdict": "achieved", "summary": "docs landed"})]);
    supervise(&fx, &next);
    assert_eq!(next.prompts().len(), 1);
    assert!(
        next.prompts()[0].contains("the docs are missing"),
        "earlier review shown"
    );
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    assert_eq!(
        queue.show_goal(goal).unwrap().goal.verdict(),
        Some(GoalVerdict::Achieved)
    );
}

#[test]
fn a_fourth_gaps_in_a_row_asks_a_person() {
    let fx = fixture();
    let (goal, _) = goal_done(&fx);
    for round in 0..3 {
        let reviewer = StubReviewer::new(&[gaps_verdict(&format!("gap {round}"))]);
        supervise(&fx, &reviewer);
        assert_eq!(reviewer.prompts().len(), 1, "round {round}");
        let mut queue = SqliteQueue::open(&fx.db).unwrap();
        let finished = goal_events(&mut queue, goal, "goal_review_finished");
        let gap = finished.last().unwrap()["gap_tasks"][0].as_i64().unwrap();
        queue
            .transition(TaskId::new(gap), TaskAction::Cancel)
            .unwrap();
    }
    let reviewer = StubReviewer::new(&[gaps_verdict("gap 3")]);
    supervise(&fx, &reviewer);
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let finished = goal_events(&mut queue, goal, "goal_review_finished");
    assert_eq!(finished.len(), 4);
    assert_eq!(finished[3]["verdict"], "gaps");
    assert_eq!(finished[3]["decision"], "ask");
    assert!(
        finished[3]["overridden"]
            .as_str()
            .unwrap()
            .contains("3 times")
    );
    assert_eq!(finished[3]["gap_tasks"], json!([]));
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].kind, AskKind::ApproveGoal);
    assert!(asks[0].question.contains("gap 3"));

    // `gaps` registers the gaps the review listed, and closes the ask.
    queue.answer(asks[0].id, "gaps").unwrap();
    assert!(
        queue
            .applies_goal_answer(&queue.read_ask(asks[0].id).unwrap())
            .unwrap()
    );
    supervise(&fx, &StubReviewer::new(&[json!({"verdict": "achieved"})]));
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let decided = goal_events(&mut queue, goal, "goal_decided");
    assert_eq!(decided[0]["decision"], "gaps");
    let gap = TaskId::new(decided[0]["gap_tasks"][0].as_i64().unwrap());
    assert_eq!(queue.show(gap).unwrap().task.title(), "gap 3");
    assert!(queue.read_ask(asks[0].id).unwrap().closed_at.is_some());
}

#[test]
fn an_ask_waits_for_a_person_whose_answer_is_applied() {
    let fx = fixture();
    let (goal, anchor) = goal_done(&fx);
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "ask",
        "criteria": [{"criterion": "(1) median under 5 minutes", "met": false, "evidence": []}],
        "summary": "the target cannot be met on this host",
        "question": "Lower the target to 8 minutes, or abandon the goal?",
        "options": ["lower_target"],
        "reason_category": "discard",
    })]);
    supervise(&fx, &reviewer);
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    let ask = &asks[0];
    assert_eq!(ask.kind, AskKind::ApproveGoal);
    assert_eq!(ask.task_id, Some(anchor));
    assert_eq!(ask.reason_category, AskReason::Discard);
    assert_eq!(
        ask.options,
        ["achieved", "abandoned", "gaps", "keep_open", "lower_target"]
    );
    assert!(
        ask.question
            .starts_with(&format!("Goal {goal}: Lower the target"))
    );
    assert!(!queue.show_goal(goal).unwrap().closed);

    // The open ask keeps the goal from another review.
    let idle = StubReviewer::new(&[json!({"verdict": "achieved"})]);
    supervise(&fx, &idle);
    assert!(idle.prompts().is_empty());

    // An option the runtime does not apply stays for the inbox to read.
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    queue.answer(ask.id, "lower_target").unwrap();
    assert!(
        !queue
            .applies_goal_answer(&queue.read_ask(ask.id).unwrap())
            .unwrap()
    );
    assert_eq!(queue.decide_goal(ask.id).unwrap().map(|d| d.goal_id), None);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_none());
    let answered = goal_answered(&mut queue, anchor);
    assert_eq!(answered["runtime_delivers"], false);
}

#[test]
fn abandoned_and_keep_open_answers_are_applied() {
    let fx = fixture();
    let (goal, anchor) = goal_done(&fx);
    let ask_verdict = json!({"verdict": "ask", "summary": "split the goal?"});
    supervise(&fx, &StubReviewer::new(std::slice::from_ref(&ask_verdict)));
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let ask = queue.asks(Default::default()).unwrap().remove(0);
    assert_eq!(ask.reason_category, AskReason::Scope);
    queue.answer(ask.id, "keep_open").unwrap();
    assert_eq!(goal_answered(&mut queue, anchor)["runtime_delivers"], true);
    let idle = StubReviewer::new(&[ask_verdict]);
    supervise(&fx, &idle);
    // keep_open: no review until the tasks change.
    assert!(idle.prompts().is_empty());
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert!(!queue.show_goal(goal).unwrap().closed);
    assert_eq!(
        goal_events(&mut queue, goal, "goal_decided")[0]["decision"],
        "keep_open"
    );

    // A person has it reviewed again; this time it asks again and a
    // person abandons it.
    let rearmed = queue.rearm_goal_review(goal).unwrap();
    assert_eq!(rearmed["reviewable"], true);
    let again = StubReviewer::new(&[json!({"verdict": "ask", "summary": "still split?"})]);
    supervise(&fx, &again);
    assert_eq!(again.prompts().len(), 1);
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let ask = queue.asks(Default::default()).unwrap().remove(0);
    queue.answer(ask.id, "abandoned").unwrap();
    supervise(&fx, &StubReviewer::new(&[json!({"verdict": "achieved"})]));
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    assert_eq!(
        queue.show_goal(goal).unwrap().goal.verdict(),
        Some(GoalVerdict::Abandoned)
    );
    let closed = goal_events(&mut queue, goal, "goal_closed");
    assert_eq!(closed[0]["by"], "person");
    assert_eq!(closed[0]["ask_id"], ask.id.as_i64());
}

#[test]
fn a_failed_goal_review_waits_for_a_person_until_rearmed() {
    let fx = fixture();
    let (goal, anchor) = goal_done(&fx);
    let failing = StubReviewer::failing();
    supervise(&fx, &failing);
    assert_eq!(failing.prompts().len(), 1);
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let failed = goal_events(&mut queue, goal, "goal_review_failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["reason_category"], "recovery_failed");
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap()
            .contains("model unavailable")
    );
    assert!(!queue.show_goal(goal).unwrap().closed);
    let holds = queue.goal_review_holds().unwrap();
    assert_eq!(holds.len(), 1);
    assert_eq!(holds[0].goal_id, goal);
    assert_eq!(holds[0].anchor, Some(anchor));

    // Not reviewed again by itself.
    let idle = StubReviewer::new(&[json!({"verdict": "achieved"})]);
    supervise(&fx, &idle);
    assert!(idle.prompts().is_empty());

    // The inbox shows it as a goal review to do by hand.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_dagq"))
        .args(["--db", fx.db.to_str().unwrap(), "status", "--role", "inbox"])
        .current_dir(&fx.repo)
        .output()
        .unwrap();
    let status = String::from_utf8_lossy(&out.stdout);
    assert!(status.contains("goal review by hand"), "{status}");
    assert!(status.contains("goal_review_failed"), "{status}");

    // `goal review ID` rearms it; the next review closes it.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_dagq"))
        .args([
            "--db",
            fx.db.to_str().unwrap(),
            "goal",
            "review",
            &goal.to_string(),
        ])
        .current_dir(&fx.repo)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    assert!(queue.goal_review_holds().unwrap().is_empty());
    assert_eq!(
        goal_events(&mut queue, goal, "goal_review_rearmed").len(),
        1
    );
    let next = StubReviewer::new(&[json!({"verdict": "achieved", "summary": "done"})]);
    supervise(&fx, &next);
    assert_eq!(next.prompts().len(), 1);
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    assert!(queue.show_goal(goal).unwrap().closed);
}

#[test]
fn only_goals_whose_tasks_ended_are_reviewed() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let new_goal = |queue: &mut SqliteQueue, title: &str| {
        queue
            .add_goal(NewGoal {
                title: title.into(),
                description: String::new(),
                acceptance: String::new(),
                constraints: String::new(),
                doc: None,
                draft: false,
            })
            .unwrap()
            .id()
    };
    // No task; only canceled tasks; a task still draft.
    new_goal(&mut queue, "empty");
    let canceled = new_goal(&mut queue, "canceled");
    let task = add(&mut queue, "c", &[], Priority::Normal);
    queue.set_goal(task, Some(canceled)).unwrap();
    queue.transition(task, TaskAction::Cancel).unwrap();
    let pending = new_goal(&mut queue, "pending");
    let done = add(&mut queue, "d", &[], Priority::Normal);
    let open = add(&mut queue, "o", &[], Priority::Normal);
    queue.set_goal(done, Some(pending)).unwrap();
    queue.set_goal(open, Some(pending)).unwrap();
    set_status(&fx, done, TaskStatus::Completed);
    assert!(queue.goal_review_candidates().unwrap().is_empty());
    let idle = StubReviewer::new(&[json!({"verdict": "achieved"})]);
    supervise(&fx, &idle);
    assert!(idle.prompts().is_empty());
    // A closed goal cannot be rearmed.
    queue.close_goal(canceled, GoalVerdict::Abandoned).unwrap();
    assert!(queue.rearm_goal_review(canceled).is_err());
    assert!(queue.rearm_goal_review(GoalId::new(99)).is_err());
}

/// An answered `approve_goal` ask of a goal closed meanwhile is closed
/// unapplied and records `ask_closed` (task 568).
#[test]
fn an_answer_to_a_goal_closed_meanwhile_is_closed_with_ask_closed() {
    let fx = fixture();
    let (goal, anchor) = goal_done(&fx);
    supervise(
        &fx,
        &StubReviewer::new(&[json!({"verdict": "ask", "summary": "split the goal?"})]),
    );
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let ask = queue.asks(Default::default()).unwrap().remove(0);
    queue.answer(ask.id, "keep_open").unwrap();
    queue.close_goal(goal, GoalVerdict::Achieved).unwrap();
    assert_eq!(queue.decide_goal(ask.id).unwrap().map(|d| d.goal_id), None);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert!(goal_events(&mut queue, goal, "goal_decided").is_empty());
    assert_eq!(
        crate::plan_review::events(&mut queue, anchor, "ask_closed"),
        [json!({"ask_id": ask.id, "kind": "approve_goal"})]
    );
}

/// The payload of the latest `ask_answered` on `task`.
fn goal_answered(queue: &mut SqliteQueue, task: TaskId) -> Value {
    queue
        .show(task)
        .unwrap()
        .events
        .into_iter()
        .rev()
        .find(|e| e.kind == "ask_answered")
        .unwrap()
        .payload
}
