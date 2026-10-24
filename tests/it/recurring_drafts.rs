//! Recurring drafts (ADR-0072): a `[[recurring]]` table of the main
//! checkout's `dagq.toml` makes the supervisor register one draft of origin
//! `recurring` when its interval came, and the headless planner it opens
//! for that draft decides it. While the draft is not ended, and until its
//! interval came again, no other is registered; `status` shows when the
//! next one is due and `doctor` the mistake of a table.

use crate::common::{
    self,
    cli::{ok, queue as cli_queue},
    service::owned_executable,
};
use crate::plan_review::{Fixture, PlanWorkspace, StubReviewer, git, options};
use crate::planner_headless::{diagnose_planner, headless_fixture, queue_events};
use dagq::application::{Clock, DraftPlannerStore, Generators, TaskReads, TaskStore};
use dagq::domain::{
    DraftOrigin, EvidenceCheck, GoalId, NewGoal, TaskId, TaskStatus,
    cancel_reason::NewCancel,
    recurring::{DAY_SECS, RecurringDraft, RecurringOutcome},
};
use dagq::infrastructure::{location::planners_dir, sqlite::SqliteQueue};
use dagq::runtime;
use serde_json::Value;
use std::{
    fs,
    path::Path,
    sync::Arc,
    sync::Mutex,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// A clock the test moves.
struct Moved(Mutex<i64>);

impl Clock for Moved {
    fn system_time(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs((*self.0.lock().unwrap()).cast_unsigned())
    }

    fn monotonic(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
}

const START: i64 = 1_800_000_000;

fn rust_item(goal: Option<GoalId>) -> RecurringDraft {
    RecurringDraft {
        key: "rust-toolchain".into(),
        interval_days: 42,
        title: "chore: raise Rust".into(),
        description: "check the latest stable".into(),
        acceptance: "fmt, clippy and test pass".into(),
        verification: vec!["cargo test".into()],
        paths: vec!["rust-toolchain.toml".into()],
        evidence: vec![EvidenceCheck::Tests],
        goal,
    }
}

fn registered(outcome: RecurringOutcome) -> TaskId {
    match outcome {
        RecurringOutcome::Registered { task_id } => task_id,
        other => panic!("not registered: {other:?}"),
    }
}

/// Acceptance (the store's boundary): the registration writes the draft
/// in its open goal with its `recurring` origin and material, which makes
/// it a target of the runtime's planners; the queue's history of the key
/// holds the next back while the draft is not ended and until its interval
/// passed, and the next names the previous registration.
#[test]
fn the_queue_keeps_the_history_that_holds_the_next_draft_back() {
    let (_dir, db) = cli_queue();
    let clock = Arc::new(Moved(Mutex::new(START)));
    let queue = SqliteQueue::open(&db).unwrap();
    let ids = queue.generators().ids.clone();
    let mut queue = queue.with_generators(Generators {
        clock: clock.clone(),
        ids,
    });
    let goal = queue
        .add_goal(NewGoal {
            priority: Default::default(),
            title: "g".into(),
            description: String::new(),
            acceptance: String::new(),
            constraints: String::new(),
            doc: None,
            draft: false,
            tags: Vec::new(),
        })
        .unwrap()
        .id();
    let first = registered(
        queue
            .register_recurring_draft(&rust_item(Some(goal)))
            .unwrap(),
    );
    let shown = queue.show(first).unwrap().task;
    assert_eq!(shown.status(), TaskStatus::Draft);
    assert_eq!(shown.goal_id(), Some(goal));
    assert_eq!(shown.paths(), ["rust-toolchain.toml"]);
    let (origin, material) = queue.draft_origin(first).unwrap().unwrap();
    assert_eq!(origin, DraftOrigin::Recurring);
    assert_eq!(material["key"], "rust-toolchain");
    assert_eq!(material["previous_task_id"], Value::Null);
    let targets = DraftPlannerStore::planner_drafts(&queue).unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].task.id(), first);
    // Not ended: held back even past its interval.
    *clock.0.lock().unwrap() = START + 50 * DAY_SECS;
    assert_eq!(
        queue.register_recurring_draft(&rust_item(None)).unwrap(),
        RecurringOutcome::Open { task_id: first }
    );
    TaskStore::cancel(
        &mut queue,
        first,
        NewCancel::by_hand(Some("not_worth".into()), None, Some("nothing new".into())),
    )
    .unwrap();
    // Ended and past its interval: the next one, naming the previous.
    let second = registered(queue.register_recurring_draft(&rust_item(None)).unwrap());
    let (_, material) = queue.draft_origin(second).unwrap().unwrap();
    assert_eq!(material["previous_task_id"], first.as_i64());
    assert_eq!(material["previous_registered_at"], START);
    TaskStore::cancel(
        &mut queue,
        second,
        NewCancel::by_hand(Some("not_worth".into()), None, None),
    )
    .unwrap();
    // Its interval counts from the second registration.
    let next = START + 50 * DAY_SECS + 42 * DAY_SECS;
    assert_eq!(
        queue.register_recurring_draft(&rust_item(None)).unwrap(),
        RecurringOutcome::NotDue { due_at: next }
    );
    let schedule = queue.recurring_schedule(&[rust_item(None)]).unwrap();
    assert_eq!(schedule[0].last_task_id, Some(second));
    assert_eq!(schedule[0].open_task_id, None);
    assert_eq!(schedule[0].next_at, Some(next));
}

/// The turn of the stub planner: keep its prompt, then cancel draft 2 as
/// not needed.
const CANCEL_DRAFT_2: &str = r#"printf '%s' "$PROMPT" > "$RUN_DIR/prompt-turn-$TURN.txt"
"$DAGQ" --db "$DB" cancel 2 --reason not_worth --note 'no new stable' >> "$RUN_DIR/cancel.log" 2>&1
say "turn $TURN""#;

const RUST: &str = "
[[recurring]]
key = \"rust-toolchain\"
interval_days = 42
title = \"chore: raise Rust to the latest stable\"
description = \"Check the latest stable Rust against rust-toolchain.toml.\"
acceptance = \"fmt, clippy and test pass on the new version\"
verification = [\"cargo test\"]
paths = [\"rust-toolchain.toml\", \"Cargo.toml\"]
evidence = [\"tests\"]
";

/// One pass of the supervisor in the test's process, given the `dagq` the
/// test owns ([`owned_executable`]), so a service it starts stops once the
/// test's process is gone, however the test ends.
fn supervise_owned(fx: &Fixture, backend: &PlanWorkspace, reviewer: &StubReviewer) {
    let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
    runtime::supervise_with_reviewer(
        &fx.db,
        &fx.repo,
        backend,
        &fx.claude,
        reviewer,
        &owned_executable(fx.db.parent().unwrap()),
        &options(1, Duration::from_secs(3600)),
    )
    .unwrap();
}

/// [`supervise_owned`] one pass at a time until `done` holds; the
/// diagnosis of planner 1 when it does not in time.
fn supervise_owned_until(
    fx: &Fixture,
    backend: &PlanWorkspace,
    reviewer: &StubReviewer,
    mut done: impl FnMut() -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        supervise_owned(fx, backend, reviewer);
        if done() {
            return;
        }
        assert!(Instant::now() < deadline, "{}", diagnose_planner(&fx.db));
        thread::sleep(Duration::from_millis(50));
    }
}

/// Commit `extra` after the main checkout's `dagq.toml`.
fn append_config(repo: &Path, extra: &str) {
    let path = repo.join("dagq.toml");
    let text = fs::read_to_string(&path).unwrap_or_default();
    fs::write(&path, format!("{text}{extra}")).unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-qm", "recurring"]);
}

/// Acceptance: the due key gets exactly one draft and a headless planner
/// for it, whose prompt reads the key and the setting; the draft canceled,
/// the next pass registers none before the interval; `status` shows the
/// next registration 42 days after the first, and `doctor` a mistake of
/// the table with its line.
#[test]
fn a_due_recurring_key_gets_one_draft_and_a_planner() {
    let fx = headless_fixture(CANCEL_DRAFT_2);
    append_config(&fx.repo, RUST);
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let reviewer = StubReviewer::new(&[]);
    let backend = PlanWorkspace::running();
    supervise_owned_until(&fx, &backend, &reviewer, || {
        !queue_events(&fx.db, "planner_closed").is_empty()
    });
    let draft = TaskId::new(2);
    let shown = queue.show(draft).unwrap().task;
    assert_eq!(shown.title(), "chore: raise Rust to the latest stable");
    assert_eq!(shown.status(), TaskStatus::Canceled);
    let planners = queue.planners(true).unwrap();
    assert_eq!(planners.len(), 1, "{planners:?}");
    assert_eq!(planners[0].draft_task_id, Some(draft));
    let dir = planners_dir(&fx.db).join(planners[0].id.to_string());
    let first = fs::read_to_string(dir.join("prompt-turn-1.txt")).unwrap();
    for part in [
        "## Where it came from: recurring",
        "recurring proposal with key `rust-toolchain`",
        "it is the first draft of the key",
    ] {
        assert!(first.contains(part), "{part} in {first}");
    }
    // Canceled, it is not due again before its interval.
    supervise_owned(&fx, &backend, &reviewer);
    assert!(queue.show(TaskId::new(3)).is_err(), "no second draft");
    let status = ok(&fx.db, &["status"]);
    let schedule = &status["recurring"]["schedule"][0];
    assert_eq!(schedule["key"], "rust-toolchain", "{status}");
    assert_eq!(schedule["last_task_id"], 2, "{status}");
    assert_eq!(
        schedule["next_at"].as_i64().unwrap() - schedule["last_registered_at"].as_i64().unwrap(),
        42 * 86_400
    );
    // A mistake of a table: `doctor` names it with its line.
    fs::write(
        fx.repo.join("dagq.toml"),
        fs::read_to_string(fx.repo.join("dagq.toml"))
            .unwrap()
            .replace("interval_days = 42", "interval_days = 0"),
    )
    .unwrap();
    let report = ok(&fx.db, &["doctor"]);
    let error = report["recurring"]["error"].as_str().unwrap_or_default();
    assert!(error.contains("value of interval_days"), "{report}");
}
