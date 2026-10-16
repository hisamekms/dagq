//! Runtime tests: a run review's `follow_ups` (ADR-t2352-1). The whole
//! review and each agent job may list stale documents the diff did not
//! touch with any verdict; the supervisor records each document's passage
//! once per run as a `docs_drift` follow_up draft of the run's task, with
//! the run and the goal as its source, and they never send the run back.
use crate::runtime_support;

use dagq::domain::{DraftOrigin, GoalId, NewGoal, ReviewVerdict};
use runtime_support::*;

/// `design` reviews `change.txt`, the stand-in worker's change.
const CONFIG: &str = "[review.subagents.design]\npaths = [\"change.txt\"]\n";
const DEFINITION: &str = "---\ndescription: design checks\n---\nCheck the design.\n";

/// Main with [`CONFIG`] and `design`'s definition, and task 1 in a goal;
/// the goal.
fn design_on_main(repo: &Path, db: &Path) -> GoalId {
    fs::write(repo.join("dagq.toml"), CONFIG).unwrap();
    fs::create_dir_all(repo.join(".dagq/agents/design")).unwrap();
    fs::write(repo.join(".dagq/agents/design/AGENT.md"), DEFINITION).unwrap();
    git(repo, &["add", "dagq.toml", ".dagq/agents/design/AGENT.md"]);
    git(repo, &["commit", "-q", "-m", "review subagents"]);
    let mut queue = SqliteQueue::open(db).unwrap();
    let goal = queue
        .add_goal(NewGoal {
            title: "the goal".into(),
            ..Default::default()
        })
        .unwrap()
        .id();
    queue.set_goal(TaskId::new(1), Some(goal)).unwrap();
    goal
}

/// A whole review's verdict script with `follow_ups`.
fn verdict_with(decision: &str, reasons: &[&str], follow_ups: Value) -> String {
    let json = json!({"verdict": decision, "reasons": reasons, "summary": "whole looked",
                      "follow_ups": follow_ups});
    format!("printf '%s\\n' '{json}'")
}

/// `design`'s passing result with `follow_ups`.
fn design_passes_with(follow_ups: Value) -> String {
    agent_answers(
        json!({"agent": "design", "status": "completed", "verdict": "pass",
                         "reasons": [], "summary": "design looked", "follow_ups": follow_ups}),
    )
}

/// Review task 1's first run with the whole review's `verdicts` and
/// `design`'s `agents`, its worker running `worker`.
fn reviewed(
    repo: &Path,
    db: &Path,
    worker: &str,
    verdicts: &[String],
    design: Vec<String>,
) -> dagq::domain::TaskDetail {
    let backend = TestWorkspace::new(db, false, worker);
    let reviewer = TestReviewer::new(verdicts).with_review_agents(&[("design", design)]);
    let outcome = supervise_reviewed(db, repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    SqliteQueue::open(db).unwrap().show(TaskId::new(1)).unwrap()
}

/// The follow_up drafts the run's reviews registered: (path, section,
/// task) of each `follow_up_registered` from a review, in order.
fn registered(detail: &dagq::domain::TaskDetail) -> Vec<(String, String, i64)> {
    payloads(detail, "follow_up_registered")
        .iter()
        .filter(|p| p["from"] == "review")
        .map(|p| {
            (
                p["path"].as_str().unwrap().to_owned(),
                p["section"].as_str().unwrap().to_owned(),
                p["task_id"].as_i64().unwrap(),
            )
        })
        .collect()
}

/// Each draft is a `docs_drift` follow_up draft of the goal, its origin
/// naming the source run, task and goal and the document.
fn assert_drafts(db: &Path, run: &dagq::domain::TaskRun, goal: GoalId, drafts: &[(&str, &str)]) {
    let mut queue = SqliteQueue::open(db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let found = registered(&detail);
    assert_eq!(
        found
            .iter()
            .map(|(path, section, _)| (path.as_str(), section.as_str()))
            .collect::<Vec<_>>(),
        drafts
    );
    // The source task's `show` lists them among its follow_up drafts,
    // with no receipt's index.
    assert_eq!(
        detail
            .follow_up_drafts
            .iter()
            .map(|d| (d.task_id.as_i64(), d.run_id.as_deref(), d.index))
            .collect::<Vec<_>>(),
        found
            .iter()
            .map(|(_, _, task)| (*task, Some(run.id().as_str()), None))
            .collect::<Vec<_>>()
    );
    for (path, section, task) in found {
        let draft = queue.show(TaskId::new(task)).unwrap().task;
        assert_eq!(draft.status(), TaskStatus::Draft);
        assert_eq!(draft.goal_id(), Some(goal));
        assert!(
            draft.title().starts_with("docs_drift: "),
            "{}",
            draft.title()
        );
        let (origin, material) = queue.draft_origin(draft.id()).unwrap().unwrap();
        assert_eq!(origin, DraftOrigin::FollowUp);
        assert_eq!(material["source_run_id"], run.id().as_str());
        assert_eq!(material["source_task_id"], 1);
        assert_eq!(material["source_goal_id"], goal.as_i64());
        assert_eq!(material["source_goal_state"], "open");
        assert_eq!(material["category"], "docs_drift");
        assert_eq!(material["from"], "review");
        assert_eq!(
            (material["path"].as_str(), material["section"].as_str()),
            (Some(path.as_str()), Some(section.as_str()))
        );
        assert!(material.get("index").is_none(), "{material}");
    }
}

/// (2b, 2c) A pass whose whole review and agent give follow_ups lands
/// without going back (no revise, the route lands), and each document's
/// passage becomes one draft of the run's task: the passage both jobs
/// found is one draft. Judging the recorded verdict once more, as a
/// supervisor that stopped before recording it would, adds no draft.
#[test]
fn a_pass_with_follow_ups_lands_and_records_each_passage_once_as_a_draft() {
    let (_dir, repo, db) = fixture();
    let goal = design_on_main(&repo, &db);
    let detail = reviewed(
        &repo,
        &db,
        IDLE_AGENT,
        &[verdict_with(
            "pass",
            &[],
            json!([{"text": "the flow is stale", "path": "docs/a.md", "section": "流れ", "codes": ["docs_drift"]}]),
        )],
        vec![design_passes_with(json!([
            {"text": "the same passage", "path": "./docs/a.md", "section": "#流れ", "codes": []},
            {"text": "the boundary is stale", "path": "docs/b.md", "codes": ["docs_drift"]},
        ]))],
    );
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Integrated);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert!(payloads(&detail, "revise_requested").is_empty());
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["verdict"], "pass");
    assert_eq!(finished[0]["route"]["destination"], "land");
    assert_eq!(finished[0]["follow_ups"][0]["path"], "docs/a.md");
    assert_eq!(
        finished[0]["agents"][0]["follow_ups"][1]["path"],
        "docs/b.md"
    );
    assert_drafts(&db, &run, goal, &[("docs/a.md", "流れ"), ("docs/b.md", "")]);
    let kinds = event_kinds(&detail);
    assert!(
        position(&kinds, "follow_up_registered") < position(&kinds, "review_finished"),
        "{kinds:?}"
    );

    // The recorded verdict judged again on a queue opened anew.
    let verdict: ReviewVerdict = serde_json::from_value(finished[0].clone()).unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let task = queue.show(TaskId::new(1)).unwrap().task;
    assert!(
        dagq::application::integrate::register_review_follow_ups(
            &mut queue,
            &task,
            run.id(),
            1,
            &verdict
        )
        .is_empty()
    );
    assert_drafts(&db, &run, goal, &[("docs/a.md", "流れ"), ("docs/b.md", "")]);
}

/// (2b, 2c) A revise's follow_ups are recorded and are not among the
/// reasons sent back; the run's next review that reports the same passage
/// again adds no draft, and only its new passage becomes one.
#[test]
fn a_revise_with_follow_ups_records_them_once_across_the_runs_reviews() {
    let (_dir, repo, db) = fixture();
    let goal = design_on_main(&repo, &db);
    let stale = json!({"text": "the flow is stale", "path": "docs/a.md", "section": "流れ"});
    let detail = reviewed(
        &repo,
        &db,
        &crate::runtime_review::revising_agent(1),
        &[
            verdict_with("revise", &["add a line to change.txt"], json!([stale])),
            verdict_with(
                "pass",
                &[],
                json!([stale, {"text": "another", "path": "docs/c.md", "section": "境界"}]),
            ),
        ],
        vec![design_passes_with(json!([stale]))],
    );
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Integrated);
    let requested = payloads(&detail, "revise_requested");
    assert_eq!(requested.len(), 1);
    assert_eq!(requested[0]["reasons"], json!(["add a line to change.txt"]));
    assert_eq!(payloads(&detail, "review_finished").len(), 2);
    assert_drafts(
        &db,
        &run,
        goal,
        &[("docs/a.md", "流れ"), ("docs/c.md", "境界")],
    );
    // The draft's text is the first review's.
    let mut queue = SqliteQueue::open(&db).unwrap();
    let first = registered(&detail)[0].2;
    let draft = queue.show(TaskId::new(first)).unwrap().task;
    assert!(
        draft
            .description()
            .starts_with("the flow is stale\n\nFound by the whole review of review 1 of run "),
        "{}",
        draft.description()
    );
}
