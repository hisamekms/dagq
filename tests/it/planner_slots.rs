//! The places under `--runtime-planners` (task 884): a planner of the
//! runtime's that took the answer of its `planner_question` and stopped
//! is ended even when the ask closed after it stopped; a revise with no
//! planner that waits at the limit tells the inbox which planners hold
//! it and why, and past the planner timeout frees a place held by an idle
//! planner nobody waits on.

use crate::plan_review::{
    PlanWorkspace, StubReviewer, add, fixture, open_goal, options, runtime_draft, submit,
};
use dagq::{
    application::{Clock, Generators, TaskStore, planner_idle_marker},
    domain::{
        AskKind, DraftOrigin, NewAsk, PlannerId, PlannerOrigin, Priority, ProposalStatus, TaskId,
        stall::StallConfig,
    },
    infrastructure::{location::planners_dir, sqlite::SqliteQueue},
    runtime::{self, SuperviseOptions},
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::{Duration, SystemTime},
};

/// The planner timeout in these tests.
const TIMEOUT_SECS: u64 = 60;

/// The wall clock moved on by a number of seconds the test sets.
#[derive(Default)]
struct Ahead(AtomicI64);

impl Clock for Ahead {
    fn system_time(&self) -> SystemTime {
        SystemTime::now() + Duration::from_secs(self.0.load(Ordering::SeqCst) as u64)
    }
}

fn supervise(
    fx: &crate::plan_review::Fixture,
    backend: &PlanWorkspace,
    reviewer: &StubReviewer,
    clock: &Arc<Ahead>,
    at: i64,
) {
    clock.0.store(at, Ordering::SeqCst);
    let options = SuperviseOptions {
        generators: Generators {
            clock: clock.clone(),
            ids: dagq::infrastructure::clock::system().ids,
        },
        stall: Some(StallConfig {
            screen_idle_secs: 5,
            ..Default::default()
        }),
        ..options(1, Duration::from_secs(TIMEOUT_SECS))
    };
    runtime::supervise_with_reviewer(
        &fx.db,
        &fx.repo,
        backend,
        &fx.claude,
        reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options,
    )
    .unwrap();
}

/// Move the planners' heartbeats far ahead, so the clock the test moves on
/// never finds their wrappers lost.
fn heartbeat_ahead(db: &Path) {
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE planners SET heartbeat_at = heartbeat_at + 100000 WHERE heartbeat_at IS NOT NULL",
            [],
        )
        .unwrap();
}

/// Set a file's modification time `secs` ahead of now, as if it was
/// written when the test's clock reads that.
fn touch_ahead(path: &Path, secs: u64) {
    fs::write(path, "{}").unwrap();
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(secs))
        .unwrap();
}

fn events(queue: &SqliteQueue, kind: &str) -> Vec<Value> {
    queue
        .latest_events_of(kind, 10)
        .unwrap()
        .into_iter()
        .map(|event| event.payload)
        .collect()
}

/// A draft of the runtime's gets its planner (in workspace `RT1`) at
/// clock 0, which asks a `planner_question`, stops, and is typed the
/// person's answer at clock 10. Returns the planner and its directory.
fn answered_draft_planner(
    fx: &crate::plan_review::Fixture,
    queue: &mut SqliteQueue,
    backend: &PlanWorkspace,
    reviewer: &StubReviewer,
    clock: &Arc<Ahead>,
) -> (PlannerId, PathBuf) {
    let goal = open_goal(queue);
    let draft = runtime_draft(
        queue,
        "gap",
        Some(goal),
        DraftOrigin::FollowUp,
        json!({"source_task_id": 1, "source_run_id": null, "index": 0}),
    );
    supervise(fx, backend, reviewer, clock, 0);
    let planner = queue.planners(false).unwrap().remove(0);
    assert_eq!(planner.origin, PlannerOrigin::Runtime);
    assert_eq!(planner.draft_task_id, Some(draft));
    queue
        .register_planner_wrapper(planner.id, std::process::id())
        .unwrap();
    heartbeat_ahead(&fx.db);
    let asked = queue
        .ask(NewAsk {
            kind: AskKind::PlannerQuestion,
            task_id: Some(draft),
            run_id: None,
            question: "is this in the goal?".into(),
            options: vec!["adopt".into(), "cancel".into(), "keep_draft".into()],
            asked_by: "planner".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
        })
        .unwrap()
        .ask;
    let dir = planners_dir(&fx.db).join(planner.id.to_string());
    touch_ahead(&planner_idle_marker(&dir), 1);
    queue.answer(asked.id, "adopt").unwrap();
    supervise(fx, backend, reviewer, clock, 10);
    assert_eq!(
        backend.texts(),
        [(
            "RT1".to_owned(),
            format!("answer to ask {}: adopt", asked.id)
        )]
    );
    (planner.id, dir)
}

/// Ask 150 and planner 338 of 2026-09-27: the answer was claimed and typed,
/// the planner took it up and stopped, and the ask closed two seconds
/// after that. The planner is done: asked to exit, and its row closes once
/// its session ends. One idle since before the typing (its stamp lost) has
/// not taken the answer up and is left alone.
#[test]
fn a_planner_idle_after_its_typed_answer_is_ended_even_when_the_ask_closed_later() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let clock = Arc::new(Ahead::default());
    let (planner, dir) = answered_draft_planner(&fx, &mut queue, &backend, &reviewer, &clock);

    // Idle since before the typing (clock 5, the claim at 10), the stamp
    // of the typing lost: it has not taken the answer up.
    fs::remove_file(dir.join("supervisor-input.json")).unwrap();
    touch_ahead(&planner_idle_marker(&dir), 5);
    supervise(&fx, &backend, &reviewer, &clock, 20);
    assert!(backend.exits.lock().unwrap().is_empty());

    // It took the answer up and stopped at clock 12; the ask closed at 14.
    touch_ahead(&planner_idle_marker(&dir), 12);
    let claimed = events(&queue, "planner_answer_claimed")[0]["claimed_at"]
        .as_i64()
        .unwrap();
    Connection::open(&fx.db)
        .unwrap()
        .execute(
            "UPDATE asks SET closed_at=?1 WHERE kind='planner_question'",
            [claimed + 4],
        )
        .unwrap();
    supervise(&fx, &backend, &reviewer, &clock, 20);
    assert_eq!(*backend.exits.lock().unwrap(), ["RT1".to_owned()]);

    // Its session ends: the row closes.
    queue
        .planner_exited(planner, std::process::id(), 0)
        .unwrap();
    supervise(&fx, &backend, &reviewer, &clock, 21);
    assert!(queue.planner(planner).unwrap().closed_at.is_some());
    assert!(queue.planners(false).unwrap().is_empty());
}

/// A revise whose owner is gone waits at the limit behind an idle planner
/// of the runtime's the supervisor takes for busy (idle since before the
/// typing of its answer, the stamp lost). Past the timeout the inbox is
/// told which planner holds the limit and why, the planner is asked to
/// exit, and once it ended the revise gets a planner of the runtime's.
#[test]
fn a_revise_at_the_limit_names_its_holders_and_frees_a_place_past_the_timeout() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "revise", "reasons": ["split it"], "summary": "too big"
    })]);
    let clock = Arc::new(Ahead::default());
    let (planner, dir) = answered_draft_planner(&fx, &mut queue, &backend, &reviewer, &clock);
    fs::remove_file(dir.join("supervisor-input.json")).unwrap();
    touch_ahead(&planner_idle_marker(&dir), 5);

    let task = add(&mut queue, "split", &[TaskId::new(1)], Priority::Normal);
    let proposal = submit(&mut queue, &[task], Some("GONE"));
    supervise(&fx, &backend, &reviewer, &clock, 20);
    assert_eq!(
        queue.show_proposal(proposal).unwrap().status(),
        ProposalStatus::Revising
    );
    assert_eq!(backend.opened().len(), 1);
    assert!(backend.exits.lock().unwrap().is_empty());

    // Within the timeout, the revise waits and the planner is kept.
    supervise(&fx, &backend, &reviewer, &clock, 60);
    assert!(events(&queue, "planner_unresponsive").is_empty());
    assert!(backend.exits.lock().unwrap().is_empty());

    // Past it: the inbox is told of the holder and why it holds.
    supervise(
        &fx,
        &backend,
        &reviewer,
        &clock,
        20 + TIMEOUT_SECS as i64 + 5,
    );
    let told = events(&queue, "planner_unresponsive");
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["planner_id"], Value::Null);
    assert_eq!(told[0]["proposal_id"], json!(proposal));
    let holders = told[0]["holders"].as_array().unwrap();
    assert_eq!(holders.len(), 1, "{told:?}");
    assert_eq!(holders[0]["planner_id"], json!(planner));
    assert_eq!(holders[0]["state"], "idle");
    assert_eq!(holders[0]["busy"], json!(["planner_answer_typed"]));
    let reason = told[0]["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!("planner {planner} (planner_answer_typed)")),
        "{reason}"
    );
    // And the idle planner is asked to exit to free its place.
    assert_eq!(*backend.exits.lock().unwrap(), ["RT1".to_owned()]);
    let released = events(&queue, "planner_released");
    assert_eq!(released.len(), 1, "{released:?}");
    assert_eq!(released[0]["planner_id"], json!(planner));
    assert_eq!(released[0]["proposal_id"], json!(proposal));
    assert_eq!(released[0]["busy"], json!(["planner_answer_typed"]));

    // Once it ended, the revise goes to a planner of the runtime's.
    queue
        .planner_exited(planner, std::process::id(), 0)
        .unwrap();
    supervise(&fx, &backend, &reviewer, &clock, 90);
    supervise(&fx, &backend, &reviewer, &clock, 91);
    let open = queue.planners(false).unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].proposal_id, Some(proposal));
    assert_eq!(backend.opened().len(), 2);
}

/// A planner whose `planner_question` waits on a person holds its place
/// past the timeout: the revise waits and the inbox is told why.
#[test]
fn a_planner_waiting_on_a_person_keeps_its_place_past_the_timeout() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "revise", "reasons": ["split it"], "summary": "too big"
    })]);
    let clock = Arc::new(Ahead::default());
    let goal = open_goal(&mut queue);
    let draft = runtime_draft(
        &mut queue,
        "gap",
        Some(goal),
        DraftOrigin::FollowUp,
        json!({"source_task_id": 1, "source_run_id": null, "index": 0}),
    );
    supervise(&fx, &backend, &reviewer, &clock, 0);
    let planner = queue.planners(false).unwrap().remove(0);
    queue
        .register_planner_wrapper(planner.id, std::process::id())
        .unwrap();
    heartbeat_ahead(&fx.db);
    queue
        .ask(NewAsk {
            kind: AskKind::PlannerQuestion,
            task_id: Some(draft),
            run_id: None,
            question: "is this in the goal?".into(),
            options: vec!["adopt".into(), "cancel".into()],
            asked_by: "planner".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
        })
        .unwrap();
    let dir = planners_dir(&fx.db).join(planner.id.to_string());
    touch_ahead(&planner_idle_marker(&dir), 1);

    let task = add(&mut queue, "split", &[TaskId::new(1)], Priority::Normal);
    let proposal = submit(&mut queue, &[task], Some("GONE"));
    supervise(&fx, &backend, &reviewer, &clock, 10);
    supervise(
        &fx,
        &backend,
        &reviewer,
        &clock,
        10 + TIMEOUT_SECS as i64 + 5,
    );
    let told = events(&queue, "planner_unresponsive");
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["proposal_id"], json!(proposal));
    assert_eq!(told[0]["holders"][0]["planner_id"], json!(planner.id));
    assert_eq!(
        told[0]["holders"][0]["busy"],
        json!(["planner_question_open"])
    );
    assert!(backend.exits.lock().unwrap().is_empty());
    assert!(events(&queue, "planner_released").is_empty());
    assert_eq!(backend.opened().len(), 1);
}
