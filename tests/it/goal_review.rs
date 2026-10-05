//! Goal review (ADR-0047 decision 43) through the supervisor loop, with the
//! headless goal review played by the plan review tests' stub provider,
//! which prints a scripted verdict. The goal's tasks are completed in the
//! queue directly; no run is claimed.

use crate::common::WithoutActor;
use crate::plan_review::{
    Fixture, PlanWorkspace, StubReviewer, add, fixture, job_actors, options, supervise_with,
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
pub(crate) fn goal_done(fx: &Fixture) -> (GoalId, TaskId) {
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = queue
        .add_goal(NewGoal {
            priority: Default::default(),
            title: "faster landings".into(),
            description: "land in half the time".into(),
            acceptance: "(1) the median landing is under 5 minutes (2) docs say how".into(),
            constraints: String::new(),
            doc: None,
            draft: false,
            tags: Vec::new(),
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

pub(crate) fn goal_events(queue: &mut SqliteQueue, goal: GoalId, kind: &str) -> Vec<Value> {
    queue
        .show_goal(goal)
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.payload)
        .collect()
}

/// The `prompt_bytes` a goal review's end records: the prompt the job was
/// given, within the goal review's limit, section by section (task 1571).
fn assert_prompt_bytes(recorded: &Value, prompt: &str) {
    let bytes = &recorded["prompt_bytes"];
    assert_eq!(bytes["total"], prompt.len(), "{recorded}");
    assert_eq!(
        bytes["limit"],
        dagq::application::prompt::GOAL_REVIEW_PROMPT_LIMIT,
        "{recorded}"
    );
    let sections: u64 = bytes["sections"]
        .as_object()
        .unwrap()
        .values()
        .map(|bytes| bytes.as_u64().unwrap())
        .sum();
    assert_eq!(sections, prompt.len() as u64, "{recorded}");
    assert!(
        bytes["sections"]["tasks"].as_u64().unwrap() > 0,
        "{recorded}"
    );
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
    // The job ran as the goal review job of the goal (ADR-t728-1).
    assert_eq!(
        job_actors(&fx.db),
        [format!("goal-review-job goal-review-job:{goal}:1")]
    );

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
    // What its prompt took (task 1571, ADR-t1566-1 decision 6).
    assert_prompt_bytes(&finished[0], &prompts[0]);

    // A closed goal is no candidate.
    let again = StubReviewer::new(&[json!({"verdict": "achieved"})]);
    supervise(&fx, &again);
    assert!(again.prompts().is_empty());
}

/// Without `[roles.goal_review]` the goal review starts as before (no
/// model or effort given); `goal_review_started` records the launch with
/// its provider and the session id the runtime gave the job, and the job's
/// span opens and closes on the goal's first task (task 1062). What a role
/// table gives the job is `supervise::goal_review::tests::
/// a_role_table_gives_the_goal_review_its_model_and_effort`.
#[test]
fn a_goal_review_records_its_launch_and_session() {
    let achieved = json!({"verdict": "achieved", "criteria": [], "summary": "done"});
    let fx = fixture();
    let (goal, done) = goal_done(&fx);
    let reviewer = StubReviewer::new(&[achieved]);
    supervise(&fx, &reviewer);
    assert_eq!(reviewer.models(), []);
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let started = &goal_events(&mut queue, goal, "goal_review_started")[0];
    assert_eq!(
        started["launch"],
        json!({"role": "goal_review", "provider": "claude", "model": null, "effort": null,
               "source": "default"})
    );
    assert_eq!(
        started["cwd"],
        fx.repo.canonicalize().unwrap().to_str().unwrap()
    );
    let session = started["session_id"].as_str().unwrap();
    assert!(!session.is_empty());
    let task_events = |queue: &mut SqliteQueue, kind: &str| -> Vec<Value> {
        queue
            .show(done)
            .unwrap()
            .events
            .into_iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.payload)
            .collect()
    };
    let opened = task_events(&mut queue, "session_opened");
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0]["kind"], "goal_review");
    assert_eq!(opened[0]["session_id"], session);
    assert_eq!(opened[0]["goal_id"], json!(goal));
    assert_eq!(opened[0]["goal_review_id"], started["goal_review_id"]);
    assert_eq!(opened[0]["launch"], started["launch"]);
    let closed = task_events(&mut queue, "session_closed");
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["kind"], "goal_review");
    assert_eq!(closed[0]["session_id"], session);
    assert_eq!(closed[0]["reason"], "job_finished");
    // `stats` counts the job under its provider (goal 73).
    let jobs = &crate::common::cli::ok(&fx.db, &["stats", "--full"])["jobs"]["goal_review"];
    assert_eq!((&jobs["count"], &jobs["failed"]), (&json!(1), &json!(0)));
    assert_eq!(jobs["by_provider"]["claude"]["verdicts"]["achieved"], 1);
    assert_eq!(jobs["by_provider"]["claude"]["secs"]["count"], 1);
}

/// A `gaps` verdict registers its gaps as drafts of the open goal, whose
/// draft keeps it from review (the SQLite rows; when it is reviewed again
/// is `domain::goal_review::tests::a_goal_is_reviewed_once_per_input_unless_rearmed`).
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
}

/// A fourth `gaps` in a row asks a person: the store counts the goal's
/// review rows (the limit is `domain::goal_review::tests::
/// gaps_are_counted_back_from_the_newest_decision`), and the supervisor's
/// ask lists the gaps, whose `gaps` answer registers them.
#[test]
fn a_fourth_gaps_in_a_row_asks_a_person() {
    use dagq::application::GoalReviewApply;
    use dagq::domain::{
        LeaseToken,
        actor_model::{ActorLaunch, ModelRole},
        goal_review::{GoalReviewDecision, GoalReviewVerdict},
    };
    let fx = fixture();
    let (goal, _) = goal_done(&fx);
    // Three gaps in a row, applied through the store as a supervisor
    // applies a review's, each gap's draft canceled after.
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let token = LeaseToken::new("earlier-supervisor");
    for round in 0..3 {
        let job = queue
            .begin_goal_review(
                goal,
                &token,
                &fx.repo.join("goal-reviews"),
                &fx.repo,
                &ActorLaunch::default_of(ModelRole::GoalReview),
            )
            .unwrap()
            .unwrap();
        let verdict = gaps_verdict(&format!("gap {round}"));
        let applied = queue
            .finish_goal_review(
                &job,
                &token,
                &GoalReviewApply {
                    verdict: GoalReviewVerdict::parse(&verdict.to_string()).unwrap(),
                    decision: GoalReviewDecision::Gaps,
                    overridden: None,
                    ask: None,
                    duration_secs: 0,
                    session: None,
                    prompt_bytes: None,
                },
            )
            .unwrap();
        queue
            .transition(applied.gap_tasks[0], TaskAction::Cancel)
            .unwrap();
    }
    let reviewer = StubReviewer::new(&[gaps_verdict("gap 3")]);
    supervise(&fx, &reviewer);
    // The earlier reviews are shown to the job.
    let prompts = reviewer.prompts();
    assert_eq!(prompts.len(), 1);
    assert!(
        prompts[0].contains("the docs are missing"),
        "earlier review shown"
    );
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
            .goal_answers()
            .unwrap()
            .iter()
            .any(|answer| answer.id == asks[0].id)
    );
    supervise(&fx, &StubReviewer::new(&[json!({"verdict": "achieved"})]));
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let decided = goal_events(&mut queue, goal, "goal_decided");
    assert_eq!(decided[0]["decision"], "gaps");
    let gap = TaskId::new(decided[0]["gap_tasks"][0].as_i64().unwrap());
    assert_eq!(queue.show(gap).unwrap().task.title(), "gap 3");
    assert!(queue.read_ask(asks[0].id).unwrap().closed_at.is_some());
}

/// An `ask` verdict opens an `approve_goal` ask on the goal's first task;
/// an option the runtime does not apply waits for the inbox, and an answer
/// to a goal closed meanwhile is closed (the SQLite rows; which answers
/// apply is `domain::goal_review::tests::an_answer_applies_when_the_goal_allows_it`).
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
    assert!(queue.goal_review_candidates().unwrap().is_empty());

    // An option the runtime does not apply stays for the inbox to read.
    queue.answer(ask.id, "lower_target").unwrap();
    assert!(
        !queue
            .goal_answers()
            .unwrap()
            .iter()
            .any(|answer| answer.id == ask.id)
    );
    assert_eq!(queue.decide_goal(ask.id).unwrap().map(|d| d.goal_id), None);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_none());
    let answered = goal_answered(&mut queue, anchor);
    assert_eq!(answered["runtime_delivers"], false);

    // An answered ask of a goal closed meanwhile is closed unapplied and
    // records `ask_closed` (task 568).
    queue.close_goal(goal, GoalVerdict::Achieved).unwrap();
    assert_eq!(queue.decide_goal(ask.id).unwrap().map(|d| d.goal_id), None);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert!(goal_events(&mut queue, goal, "goal_decided").is_empty());
    assert_eq!(
        crate::plan_review::events(&mut queue, anchor, "ask_closed"),
        [json!({"ask_id": ask.id, "kind": "approve_goal"})]
    );
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

/// A failed goal review holds the goal for a person, whom the inbox's
/// `status` tells and whose `goal review ID` rearms it (the CLI and the
/// SQLite rows; when a goal waits is `domain::goal_review::tests::
/// a_goal_is_reviewed_once_per_input_unless_rearmed`).
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
    assert_prompt_bytes(&failed[0], &failing.prompts()[0]);
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
    assert!(queue.goal_review_candidates().unwrap().is_empty());

    // The inbox shows it as a goal review to do by hand.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .args(["--db", fx.db.to_str().unwrap(), "status", "--role", "inbox"])
        .current_dir(&fx.repo)
        .output()
        .unwrap();
    let status = String::from_utf8_lossy(&out.stdout);
    assert!(status.contains("goal review by hand"), "{status}");
    assert!(status.contains("goal_review_failed"), "{status}");

    // `goal review ID` rearms it; the next review closes it. A person
    // does, not the actor of the session running the tests.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
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

/// Goal review is an actor of its own, the goal-review-job (ADR-t728-1):
/// its verdict is data the supervisor applies and records as its own, at
/// the job's request. An output it cannot read, or one with a field it
/// does not know (the verdict, a criterion or a gap), closes nothing and
/// registers nothing: the goal waits for a person (`goal review by hand`).
#[test]
fn a_goal_review_verdict_is_applied_at_its_jobs_request_and_a_broken_one_fails_closed() {
    for (broken, expected) in [
        (json!("no verdict here"), "printed no verdict JSON"),
        (
            json!({"verdict": "achieved", "summary": "done", "close": true}),
            "unknown field `close`",
        ),
        (
            json!({"verdict": "achieved", "criteria": [
                {"criterion": "(1)", "met": true, "evidence": [], "override": true}]}),
            "unknown field `override`",
        ),
        (
            json!({"verdict": "gaps", "gaps": [{"title": "docs", "priority": "urgent"}]}),
            "unknown field `priority`",
        ),
    ] {
        let fx = fixture();
        let (goal, _) = goal_done(&fx);
        supervise(&fx, &StubReviewer::new(&[broken]));
        let mut queue = SqliteQueue::open(&fx.db).unwrap();
        let detail = queue.show_goal(goal).unwrap();
        assert!(!detail.closed, "{expected}");
        assert!(
            !detail
                .events
                .iter()
                .any(|e| e.kind == "goal_review_finished" || e.kind == "goal_closed"),
            "{expected}"
        );
        let failed = goal_events(&mut queue, goal, "goal_review_failed");
        assert_eq!(failed.len(), 1, "{expected}");
        let error = failed[0]["error"].as_str().unwrap();
        assert!(error.contains(expected), "{error}");
        assert_eq!(failed[0]["reason_category"], "recovery_failed");
        assert_eq!(queue.goal_review_holds().unwrap().len(), 1, "{expected}");
        let status = dagq::runtime::status(&fx.db).unwrap();
        assert!(
            status["attention"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["kind"] == "goal_review_failed" && a["next"] == "goal review by hand"),
            "{status}"
        );
        // No gap became a draft of the goal.
        assert_eq!(queue.show_goal(goal).unwrap().tasks.len(), 2, "{expected}");
    }

    let fx = fixture();
    let (goal, _) = goal_done(&fx);
    supervise(
        &fx,
        &StubReviewer::new(&[json!({"verdict": "achieved", "summary": "done"})]),
    );
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let detail = queue.show_goal(goal).unwrap();
    assert!(detail.closed);
    let supervisor = format!("supervisor:{}", std::process::id());
    let job = format!("goal-review-job:{goal}:1");
    for kind in ["goal_review_finished", "goal_closed"] {
        let event = detail
            .events
            .iter()
            .find(|e| e.kind == kind)
            .unwrap_or_else(|| panic!("no {kind}"));
        let actor = event.actor.clone().expect("an actor");
        assert_eq!(
            (actor.role.as_str(), actor.id.as_str(), actor.requested_by),
            ("supervisor", supervisor.as_str(), Some(job.clone())),
            "{kind}"
        );
    }
}

/// Simulate the rows left by prepare_handoff, before the same pid/token
/// enters its first pass. Both review kinds must close without a candidate.
#[test]
fn handoff_closes_review_rows_and_spans_without_candidates() {
    handoff_reviews(false);
}

#[test]
fn handoff_reviews_candidates_again_without_counting_interrupted_attempts() {
    handoff_reviews(true);
}

fn handoff_reviews(keep_candidates: bool) {
    use crate::plan_review::{events, submit};
    use dagq::application::PlanReviewStore;
    use dagq::domain::{
        LeaseToken,
        actor_model::{ActorLaunch, ModelRole},
    };

    let fx = fixture();
    let (goal, goal_task) = goal_done(&fx);
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(
        &mut queue,
        "review this plan",
        &[TaskId::new(1)],
        Priority::Normal,
    );
    let proposal = submit(&mut queue, &[task], None);
    let token = LeaseToken::new("handoff-review-owner");
    queue
        .register_supervisor(&token, std::process::id(), 2, "previous")
        .unwrap();
    let goal_job = queue
        .begin_goal_review(
            goal,
            &token,
            &fx.repo.join("goal-reviews"),
            &fx.repo,
            &ActorLaunch::default_of(ModelRole::GoalReview),
        )
        .unwrap()
        .unwrap();
    let plan_job = queue
        .begin_plan_review(
            proposal,
            &token,
            &fx.repo.join("plan-reviews"),
            &fx.repo,
            &ActorLaunch::default_of(ModelRole::PlanReview),
        )
        .unwrap()
        .unwrap();

    // Even with an old heartbeat, a different token's handoff cannot
    // finish these rows. This path never uses the stale-owner heuristic.
    let conn = Connection::open(&fx.db).unwrap();
    conn.execute(
        "UPDATE supervisors SET heartbeat_at=0 WHERE token=?1",
        [&token],
    )
    .unwrap();
    let other = LeaseToken::new("another-supervisor");
    queue.interrupt_goal_reviews_for_handoff(&other).unwrap();
    queue.interrupt_plan_reviews_for_handoff(&other).unwrap();
    for table in ["goal_reviews", "plan_reviews"] {
        let count: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE finished_at IS NULL"),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
    if !keep_candidates {
        // The goal was closed and the proposal withdrawn while exec ran.
        conn.execute(
            "UPDATE goals SET status='closed', closed_at=unixepoch() WHERE id=?1",
            [goal],
        )
        .unwrap();
        queue.transition(task, TaskAction::Cancel).unwrap();
        assert!(queue.goal_review_candidates().unwrap().is_empty());
        assert!(queue.plan_review_candidates().unwrap().is_empty());
    }
    let reviewer = StubReviewer::new(&[
        json!({"verdict": "pass", "reasons": [], "actions": []}),
        json!({"verdict": "achieved", "criteria": [], "summary": "done"}),
    ]);
    let mut opts = options(0, Duration::from_secs(3600));
    opts.handoff_token = Some(token.clone());
    supervise_with(&fx, &PlanWorkspace::default(), &reviewer, &opts);
    for (table, id) in [("goal_reviews", goal_job.id), ("plan_reviews", plan_job.id)] {
        let (outcome, error, finished): (String, String, bool) = conn
            .query_row(
                &format!("SELECT outcome, error, finished_at IS NOT NULL FROM {table} WHERE id=?1"),
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(outcome, "interrupted");
        assert_eq!(error, "stopped for the supervisor handoff");
        assert!(finished);
        let attempts: Vec<i64> = conn
            .prepare(&format!("SELECT attempt FROM {table} ORDER BY id"))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(attempts, if keep_candidates { vec![1, 1] } else { vec![1] });
    }
    for (anchor, kind) in [(goal_task, "goal_review"), (task, "plan_review")] {
        let opened = events(&mut queue, anchor, "session_opened");
        let closed = events(&mut queue, anchor, "session_closed");
        let opened: Vec<_> = opened.iter().filter(|e| e["kind"] == kind).collect();
        let closed: Vec<_> = closed.iter().filter(|e| e["kind"] == kind).collect();
        assert_eq!(closed.len(), opened.len(), "no open {kind} span remains");
        assert_eq!(closed[0]["reason"], "job_finished");
    }
    assert_eq!(
        reviewer.prompts().len(),
        if keep_candidates { 2 } else { 0 }
    );
    // Cleanup is idempotent, including after the registration was removed.
    queue.interrupt_goal_reviews_for_handoff(&token).unwrap();
    queue.interrupt_plan_reviews_for_handoff(&token).unwrap();
    assert_eq!(
        events(&mut queue, goal_task, "session_closed").len(),
        if keep_candidates { 2 } else { 1 }
    );
}

/// ADR-t1504-2 decision 9: a follow-up registered after its goal closed as
/// achieved and judged required opens a `correct_goal` ask; the supervisor
/// applies the person's `reopen`, which opens the goal again with the
/// follow-up in it and leaves the close in the history.
#[test]
fn the_supervisor_applies_a_reopen_answer_to_a_correction() {
    use dagq::domain::follow_up::{MembershipClassification, MembershipJudgement};
    let fx = fixture();
    let (goal, _) = goal_done(&fx);
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    queue.close_goal(goal, GoalVerdict::Achieved).unwrap();
    let follow_up = add(&mut queue, "missed requirement", &[], Priority::Normal);
    assert_eq!(
        queue.show(follow_up).unwrap().task.status(),
        TaskStatus::Draft
    );
    queue
        .record_draft_origin(
            follow_up,
            DraftOrigin::FollowUp,
            &json!({"source_goal_id": goal, "source_goal_state": "closed",
                "source_goal_provenance": "recorded"}),
        )
        .unwrap();
    let row = queue
        .judge_follow_up(
            follow_up,
            MembershipJudgement {
                classification: MembershipClassification::Required,
                acceptance_items: vec!["(2) docs say how".into()],
                reason: "the docs were never written".into(),
                evidence: vec!["task:1".into()],
                destination_goal_id: None,
                source_goal_id: None,
                corrects: None,
            },
            "planner",
        )
        .unwrap();
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].kind, AskKind::CorrectGoal);
    assert_eq!(
        asks[0].id.as_i64(),
        row["correction_ask_id"].as_i64().unwrap()
    );
    queue.answer(asks[0].id, "reopen").unwrap();
    let answered = queue
        .show(follow_up)
        .unwrap()
        .events
        .into_iter()
        .rev()
        .find(|e| e.kind == "ask_answered")
        .unwrap()
        .payload;
    assert_eq!(answered["runtime_delivers"], true);
    // `status` shows it as the runtime's to apply, not the inbox's.
    let status_now = dagq::runtime::status(&fx.db).unwrap();
    let entry = status_now["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["ask_id"] == json!(asks[0].id))
        .cloned()
        .unwrap();
    assert_eq!(
        entry["next"],
        format!("applying the answer of ask {} (runtime)", asks[0].id)
    );

    let idle = StubReviewer::new(&[json!({"verdict": "achieved"})]);
    supervise(&fx, &idle);
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    assert!(queue.read_ask(asks[0].id).unwrap().closed_at.is_some());
    let detail = queue.show_goal(goal).unwrap();
    assert!(!detail.closed);
    assert_eq!(goal_events(&mut queue, goal, "goal_closed").len(), 1);
    assert_eq!(goal_events(&mut queue, goal, "goal_reopened").len(), 1);
    assert_eq!(
        goal_events(&mut queue, goal, "goal_correction_decided")[0]["decision"],
        "reopen"
    );
    assert_eq!(queue.show(follow_up).unwrap().task.goal_id(), Some(goal));
    // The goal now waits for its follow-up, so it is not reviewed.
    assert!(idle.prompts().is_empty());
}

/// Status uses the same recorded selection as the supervisor, even after
/// a goal changes from permitting the answer to refusing it, or vice versa.
#[test]
fn goal_answer_attention_uses_the_recorded_delivery_decision() {
    for runtime_delivers in [false, true] {
        let fx = fixture();
        let (goal, anchor) = goal_done(&fx);
        supervise(&fx, &StubReviewer::new(&[json!({"verdict": "ask"})]));
        let mut queue = SqliteQueue::open(&fx.db).unwrap();
        let ask = queue.asks(Default::default()).unwrap().remove(0);
        set_status(
            &fx,
            anchor,
            if runtime_delivers {
                TaskStatus::Completed
            } else {
                TaskStatus::Ready
            },
        );
        queue.answer(ask.id, "achieved").unwrap();
        assert_eq!(
            goal_answered(&mut queue, anchor)["runtime_delivers"],
            runtime_delivers
        );
        // A true answer still belongs to the runtime if the goal closes;
        // a false answer stays with the inbox if its tasks later finish.
        if runtime_delivers {
            queue.close_goal(goal, GoalVerdict::Achieved).unwrap();
        } else {
            set_status(&fx, anchor, TaskStatus::Completed);
        }
        assert_eq!(
            queue.goal_answers().unwrap().iter().any(|a| a.id == ask.id),
            runtime_delivers
        );
        let status = dagq::runtime::status(&fx.db).unwrap();
        let entry = status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["ask_id"] == json!(ask.id))
            .unwrap();
        assert_eq!(
            entry["next"],
            if runtime_delivers {
                format!("applying the answer of ask {} (runtime)", ask.id)
            } else {
                format!("read the answer of ask {} and close it", ask.id)
            }
        );
        queue.close_ask(ask.id).unwrap();
        assert!(queue.goal_answers().unwrap().is_empty());
        assert!(
            dagq::runtime::status(&fx.db).unwrap()["attention"]
                .as_array()
                .unwrap()
                .iter()
                .all(|a| a["ask_id"] != json!(ask.id))
        );
    }
}
