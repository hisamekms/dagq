//! A draft's revisit time (ADR-t1540-1): `dagq revisit ID --at <time>`
//! (the user's, the inbox's and a planner's; workers, jobs and the observer
//! are refused) makes the draft a target of the runtime's draft planners
//! again once the time comes, past a `keep_draft` answer and for a draft a
//! person added, and the headless planner the supervisor opens for it
//! reads the last decision about it in its first turn's prompt. No planner
//! workspace, screen or cmux double is used: the planners run headless in
//! the background with the stub agent of the headless tests.

use crate::common::{
    cli::{invoke_with, ok, ok_as, queue},
    queue::runtime_draft,
};
use crate::plan_review::{PlanWorkspace, StubReviewer, open_goal, options, supervise_with};
use crate::planner_headless::{diagnose_planner, headless_fixture, queue_events, supervise_until};
use dagq::application::TaskStore;
use dagq::domain::{AskKind, AskReason, DraftOrigin, NewAsk, NewTask, PlannerRoute, TaskId};
use dagq::infrastructure::{location::planners_dir, sqlite::SqliteQueue};
use serde_json::{Value, json};
use std::{fs, path::Path, time::Duration};

/// The turn of the stub planner: keep its prompt, then cancel draft 2.
const CANCEL_DRAFT_2: &str = r#"printf '%s' "$PROMPT" > "$RUN_DIR/prompt-turn-$TURN.txt"
"$DAGQ" --db "$DB" cancel 2 >> "$RUN_DIR/cancel.log" 2>&1
say "turn $TURN""#;

/// A time long gone, and one far ahead.
const PAST: &str = "2026-01-01T00:00:00Z";
const FUTURE: &str = "2099-01-01T00:00:00Z";

/// Supervise one pass: one that opens no planner for a draft shows it is
/// no target (a target gets its planner in the pass that sees it).
fn pass(fx: &crate::plan_review::Fixture, backend: &PlanWorkspace, reviewer: &StubReviewer) {
    supervise_with(
        fx,
        backend,
        reviewer,
        &options(1, Duration::from_secs(3600)),
    );
}

/// The events of `kind` on `task`, each with its actor.
fn events_of(db: &Path, task: TaskId, kind: &str) -> Vec<Value> {
    ok(db, &["show", &task.to_string(), "--full"])["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == kind)
        .cloned()
        .collect()
}

/// Acceptance: a follow_up draft a `keep_draft` answer took out of the
/// runtime's planners gets none at a revisit time still to come, and at a
/// past one gets a headless planner in the next pass, whose first turn's
/// prompt carries the draft's question, its recommendation and answer, its
/// note and the revisit time. Setting, changing and the time coming are
/// events with their actor, and `show` gives the revisit time.
#[test]
fn a_kept_draft_gets_a_headless_planner_at_its_revisit_time_with_the_last_decision() {
    let fx = headless_fixture(CANCEL_DRAFT_2);
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let draft = runtime_draft(
        &mut queue,
        "measure goal 90 again",
        Some(goal),
        DraftOrigin::FollowUp,
        json!({"source_task_id": 1, "source_run_id": null, "index": 0}),
    );
    assert_eq!(draft, TaskId::new(2));
    // A planner before asked, was answered keep_draft and noted when.
    let asked = queue
        .ask(NewAsk {
            recommendation: Some("keep_draft".into()),
            confidence: Some("high".parse().unwrap()),
            topics: Vec::new(),
            kind: AskKind::PlannerQuestion,
            task_id: Some(draft),
            run_id: None,
            question: "measure it before the next landings?".into(),
            options: vec!["adopt".into(), "cancel".into(), "keep_draft".into()],
            asked_by: "planner".into(),
            reason_category: AskReason::Scope,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask;
    queue.answer(asked.id, "keep_draft").unwrap();
    queue
        .close_planner_answer(asked.id, "its draft is kept")
        .unwrap();
    ok(
        &fx.db,
        &["note", "--task", "2", "--text", "fill it in at noon UTC"],
    );
    let reviewer = StubReviewer::new(&[]);
    let backend = PlanWorkspace::running();
    pass(&fx, &backend, &reviewer);
    assert!(queue.planners(true).unwrap().is_empty(), "kept: no planner");

    // A time to come: still none.
    let set = ok_as(
        "inbox",
        &fx.db,
        &[
            "revisit",
            "2",
            "--at",
            FUTURE,
            "--note",
            "after task 1429 lands",
        ],
    );
    assert_eq!(set["revisit"]["revisit_at_utc"], "2099-01-01T00:00:00.000Z");
    pass(&fx, &backend, &reviewer);
    assert!(queue.planners(true).unwrap().is_empty(), "not yet");

    // Changed to a past time: the next pass opens one.
    ok_as(
        "inbox",
        &fx.db,
        &[
            "revisit",
            "2",
            "--at",
            PAST,
            "--note",
            "after task 1429 lands",
        ],
    );
    supervise_until(
        &fx,
        &backend,
        &reviewer,
        || !queue_events(&fx.db, "planner_closed").is_empty(),
        || diagnose_planner(&fx.db),
    );
    let planners = queue.planners(true).unwrap();
    assert_eq!(planners.len(), 1, "{planners:?}");
    let planner = &planners[0];
    assert_eq!(planner.route, PlannerRoute::Headless);
    assert_eq!(planner.draft_task_id, Some(draft));
    let dir = planners_dir(&fx.db).join(planner.id.to_string());
    let first = fs::read_to_string(dir.join("prompt-turn-1.txt")).unwrap();
    assert_eq!(
        first,
        fs::read_to_string(dir.join("prompt.txt")).unwrap(),
        "the first turn ran the prompt the runtime wrote"
    );
    assert!(dir.join("turns").join("turn-000001.jsonl").is_file());
    for part in [
        "## Revisit of draft 2",
        "Its revisit time came: 2026-01-01T00:00:00.000Z (set by inbox",
        "What to look at then: after task 1429 lands",
        &format!(
            "- ask {}: measure it before the next landings?\n  Recommended: keep_draft (high). Answer: keep_draft",
            asked.id
        ),
        "- fill it in at noon UTC",
    ] {
        assert!(first.contains(part), "{part} in {first}");
    }
    assert_eq!(
        queue.show(draft).unwrap().task.status(),
        dagq::domain::TaskStatus::Canceled
    );
    // Set twice and come due, each with its actor; `show` gives the time.
    let set = events_of(&fx.db, draft, "draft_revisit_set");
    assert_eq!(set.len(), 2, "{set:?}");
    assert!(set.iter().all(|e| e["actor"]["role"] == "inbox"), "{set:?}");
    assert_eq!(
        set[1]["payload"]["from"], "2099-01-01T00:00:00.000Z",
        "{set:?}"
    );
    let due = events_of(&fx.db, draft, "draft_revisit_due");
    assert_eq!(due.len(), 1, "{due:?}");
    assert_eq!(due[0]["payload"]["planner_id"], planner.id.as_i64());
    assert_eq!(due[0]["actor"]["role"], "supervisor");
    let shown = ok(&fx.db, &["show", "2"]);
    assert_eq!(
        shown["revisit"]["revisit_at_utc"], "2026-01-01T00:00:00.000Z",
        "{shown}"
    );
    assert_eq!(shown["revisit"]["planner_id"], planner.id.as_i64());
}

/// Acceptance: a draft a person added (no `draft_origins` row) gets a
/// headless planner once its revisit time came, as origin `revisit`, and a
/// person's draft without one still gets none.
#[test]
fn a_persons_draft_gets_a_planner_only_at_its_revisit_time() {
    let fx = headless_fixture(CANCEL_DRAFT_2);
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let mine = |title: &str| NewTask {
        title: title.into(),
        description: format!("{title}: a person's"),
        acceptance: String::new(),
        verification_commands: Vec::new(),
        required_evidence: Vec::new(),
        paths: Vec::new(),
        dependencies: Vec::new(),
        goal_dependencies: Vec::new(),
        priority: None,
        change: None,
        goal_id: None,
        context: String::new(),
        provider: None,
        worker_mode: None,
        wait_for_build: false,
    };
    let revisited = queue.add(mine("revisit me")).unwrap().id();
    let left = queue.add(mine("leave me")).unwrap().id();
    assert_eq!(revisited, TaskId::new(2));
    let reviewer = StubReviewer::new(&[]);
    let backend = PlanWorkspace::running();
    pass(&fx, &backend, &reviewer);
    assert!(queue.planners(true).unwrap().is_empty(), "a person's draft");

    ok(&fx.db, &["revisit", "2", "--at", PAST]);
    supervise_until(
        &fx,
        &backend,
        &reviewer,
        || !queue_events(&fx.db, "planner_closed").is_empty(),
        || diagnose_planner(&fx.db),
    );
    pass(&fx, &backend, &reviewer);
    let planners = queue.planners(true).unwrap();
    assert_eq!(planners.len(), 1, "{planners:?}");
    assert_eq!(planners[0].draft_task_id, Some(revisited));
    let opened = events_of(&fx.db, revisited, "draft_planner_opened");
    assert_eq!(opened[0]["payload"]["origin"], "revisit", "{opened:?}");
    assert_eq!(opened[0]["payload"]["revisit_set_by"], "user");
    let dir = planners_dir(&fx.db).join(planners[0].id.to_string());
    let first = fs::read_to_string(dir.join("prompt-turn-1.txt")).unwrap();
    assert!(
        first.contains("A person added it (the runtime and the jobs did not)"),
        "{first}"
    );
    assert!(first.contains("## Revisit of draft 2"), "{first}");
    // The person's draft without a revisit time got none.
    assert!(events_of(&fx.db, left, "draft_planner_opened").is_empty());
    assert_eq!(
        ok(&fx.db, &["show", &left.to_string()])["revisit"],
        json!(null)
    );
    assert_eq!(ok(&fx.db, &["show", "2"])["origin"], json!(null));
}

/// Acceptance: the user, the inbox and a planner set, change and clear a
/// draft's revisit time; a worker, the observer and the jobs are refused
/// (`not granted`, recorded as `authorization_denied`), and so is a time
/// that is not RFC 3339.
#[test]
fn people_the_inbox_and_planners_revisit_and_workers_jobs_and_the_observer_may_not() {
    let (_dir, db) = queue();
    ok(&db, &["add", "draft"]);
    let worker = [
        ("DAGQ_ROLE", "worker"),
        ("DAGQ_ACTOR_ID", "worker:r1"),
        ("DAGQ_RUN_ID", "r1"),
        ("DAGQ_TASK_ID", "1"),
    ];
    let mut refused_roles: Vec<Vec<(&str, &str)>> = vec![worker.to_vec()];
    for role in [
        "observer",
        "review-job",
        "recovery-job",
        "plan-review-job",
        "goal-review-job",
        "throughput-review-job",
    ] {
        refused_roles.push(vec![("DAGQ_ROLE", role)]);
    }
    for env in &refused_roles {
        for args in [
            &["revisit", "1", "--at", PAST][..],
            &["revisit", "1", "--clear"],
        ] {
            let output = invoke_with(env, &db, args);
            assert!(!output.status.success(), "{env:?} {args:?}");
            let error: Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(error["denied"]["capability"], "task.write", "{error}");
            assert_eq!(error["denied"]["reason"], "not granted", "{error}");
        }
    }
    assert_eq!(ok(&db, &["show", "1"])["revisit"], json!(null));
    let denied = ok(
        &db,
        &[
            "events",
            "--after",
            "0",
            "--full",
            "--kind",
            "authorization_denied",
        ],
    )["events"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(denied, refused_roles.len() * 2);

    let planner = [("DAGQ_ROLE", "planner"), ("DAGQ_ACTOR_ID", "planner:4")];
    let output = invoke_with(&planner, &db, &["revisit", "1", "--at", FUTURE]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    ok_as(
        "inbox",
        &db,
        &["revisit", "1", "--at", PAST, "--note", "now"],
    );
    ok(&db, &["revisit", "1", "--clear"]);
    assert_eq!(ok(&db, &["show", "1"])["revisit"], json!(null));
    let output = invoke_with(&[], &db, &["revisit", "1", "--at", "tomorrow"]);
    assert!(!output.status.success());
    let set = events_of(&db, TaskId::new(1), "draft_revisit_set");
    let roles: Vec<&Value> = set.iter().map(|e| &e["actor"]["role"]).collect();
    assert_eq!(roles, [&json!("planner"), &json!("inbox")]);
    assert_eq!(set[0]["actor"]["id"], "planner:4");
    let cleared = events_of(&db, TaskId::new(1), "draft_revisit_cleared");
    assert_eq!(cleared.len(), 1);
    assert_eq!(cleared[0]["actor"]["role"], "user");
    assert_eq!(cleared[0]["payload"]["set_by"], "inbox");
}
