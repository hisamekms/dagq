//! The places under `--runtime-planners` (task 884): a planner of the
//! runtime's that took the answer of its `planner_question` and stopped
//! is ended even when the ask closed after it stopped; a revise with no
//! planner that waits at the limit tells the inbox which planners hold
//! it and why, and past the planner timeout frees a place held by an idle
//! planner nobody waits on. Without the flag, `[supervisor]
//! runtime_planners` of `dagq.toml` sets the limit (task 941).

use crate::plan_review::{
    PlanWorkspace, StubReviewer, add, fixture, open_goal, options, runtime_draft, submit,
    supervise_with,
};
use crate::runtime_support::planner_turns::{
    block_next_request, exit_requested, take_turns, turn_requests,
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

    fn monotonic(&self) -> std::time::Instant {
        std::time::Instant::now()
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

/// A draft of the runtime's gets its headless planner (its wrapper parked)
/// at clock 0, which asks a `planner_question`, stops, and is sent the
/// person's answer as its next turn's request at clock 10. Returns the
/// planner and its directory.
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
    queue
        .register_planner_agent(planner.id, std::process::id(), std::process::id())
        .unwrap();
    heartbeat_ahead(&fx.db);
    let asked = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::PlannerQuestion,
            task_id: Some(draft),
            run_id: None,
            question: "is this in the goal?".into(),
            options: vec!["adopt".into(), "cancel".into(), "keep_draft".into()],
            asked_by: "planner".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask;
    let dir = planners_dir(&fx.db).join(planner.id.to_string());
    touch_ahead(&planner_idle_marker(&dir), 1);
    queue.answer(asked.id, "adopt").unwrap();
    supervise(fx, backend, reviewer, clock, 10);
    let requests = turn_requests(&fx.db, planner.id);
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(
        requests[0]["prompt"],
        format!("answer to ask {}: adopt", asked.id)
    );
    (planner.id, dir)
}

/// The wrapper took the requests waiting in `dir`'s `turns/`, and no turn
/// of them is recorded yet.
fn taken_without_a_turn(dir: &Path) {
    let turns = dir.join("turns");
    for entry in fs::read_dir(&turns).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        if let Some((seq, false)) = dagq::domain::turn::request_seq(&name) {
            fs::rename(&path, dagq::domain::turn::taken_path(dir, seq)).unwrap();
        }
    }
}

/// Ask 150 and planner 338 of 2026-09-27: the answer was claimed and sent
/// as the planner's next turn, the planner took it up in that turn and
/// stopped, and the ask closed after that. The planner is done: asked to
/// exit, and its row closes once its session ends. One whose request with
/// the answer waits in its `turns/` is at work and left alone.
#[test]
fn a_planner_that_took_its_answer_in_a_turn_is_ended_even_when_the_ask_closed_later() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let clock = Arc::new(Ahead::default());
    let (planner, _) = answered_draft_planner(&fx, &mut queue, &backend, &reviewer, &clock);

    // The request with the answer waits: it is at work.
    supervise(&fx, &backend, &reviewer, &clock, 20);
    assert!(!exit_requested(&fx.db, planner));

    // It took the answer up in a turn and stopped; the ask closed four
    // seconds after the claim.
    take_turns(&queue, &fx.db, planner);
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
    assert!(exit_requested(&fx.db, planner));

    // Its session ends: the row closes.
    queue
        .planner_exited(planner, std::process::id(), 0)
        .unwrap();
    supervise(&fx, &backend, &reviewer, &clock, 21);
    assert!(queue.planner(planner).unwrap().closed_at.is_some());
    assert!(queue.planners(false).unwrap().is_empty());
}

/// A revise whose owner is gone waits at the limit behind an idle planner
/// of the runtime's the supervisor takes for busy (its wrapper took the
/// request with the answer, and no turn of it is recorded). Past the
/// timeout the inbox is told which planner holds the limit and why, the
/// planner is asked to exit, and once it ended the revise gets a planner
/// of the runtime's.
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
    taken_without_a_turn(&dir);
    touch_ahead(&planner_idle_marker(&dir), 5);

    let task = add(&mut queue, "split", &[TaskId::new(1)], Priority::Normal);
    let proposal = submit(&mut queue, &[task], Some("GONE"));
    supervise(&fx, &backend, &reviewer, &clock, 20);
    assert_eq!(
        queue.show_proposal(proposal).unwrap().status(),
        ProposalStatus::Revising
    );
    assert_eq!(backend.launched().len(), 1);
    assert!(!exit_requested(&fx.db, planner));

    // Within the timeout, the revise waits and the planner is kept.
    supervise(&fx, &backend, &reviewer, &clock, 60);
    assert!(events(&queue, "planner_unresponsive").is_empty());
    assert!(!exit_requested(&fx.db, planner));

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
    assert!(exit_requested(&fx.db, planner));
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
    assert_eq!(backend.launched().len(), 2);
}

/// A `planner_question` about draft `task` by its planner.
fn question(queue: &mut SqliteQueue, task: TaskId, text: &str) -> dagq::domain::Ask {
    queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::PlannerQuestion,
            task_id: Some(task),
            run_id: None,
            question: text.into(),
            options: vec!["adopt".into(), "cancel".into(), "keep_draft".into()],
            asked_by: "planner".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask
}

/// ADR-t1704-1 decision 1 (acceptance (a)): a headless planner of the
/// runtime's that is idle with its `planner_question` not answered and
/// nothing else to do is asked to exit (`planner_answer_wait`, its row's
/// `answer_wait_at`); once its session ended its row closes as
/// `runtime_answer_wait` and its place under `--runtime-planners` goes to
/// the revise that waited at the limit. The ask stays open, and no planner
/// is opened for the draft without its answer.
#[test]
fn a_planner_waiting_only_on_a_person_is_ended_and_its_place_goes_to_a_waiting_revise() {
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
    queue
        .register_planner_agent(planner.id, std::process::id(), std::process::id())
        .unwrap();
    heartbeat_ahead(&fx.db);
    // The revise of a proposal whose planner is gone waits at the limit.
    let task = add(&mut queue, "split", &[TaskId::new(1)], Priority::Normal);
    let proposal = submit(&mut queue, &[task], Some("GONE"));
    supervise(&fx, &backend, &reviewer, &clock, 5);
    assert_eq!(
        queue.show_proposal(proposal).unwrap().status(),
        ProposalStatus::Revising
    );
    assert_eq!(backend.launched().len(), 1);

    let asked = question(&mut queue, draft, "is this in the goal?");
    let dir = planners_dir(&fx.db).join(planner.id.to_string());
    touch_ahead(&planner_idle_marker(&dir), 11);
    supervise(&fx, &backend, &reviewer, &clock, 10);
    assert!(exit_requested(&fx.db, planner.id));
    let waited = events(&queue, "planner_answer_wait");
    assert_eq!(waited.len(), 1, "{waited:?}");
    assert_eq!(waited[0]["planner_id"], json!(planner.id));
    assert_eq!(waited[0]["asks"], json!([asked.id]));
    assert_eq!(waited[0]["draft_task_id"], json!(draft));
    assert_eq!(waited[0]["revise"], false);
    assert!(queue.planner(planner.id).unwrap().answer_wait_at.is_some());
    // Asked once: the next pass waits for its exit.
    supervise(&fx, &backend, &reviewer, &clock, 11);
    assert_eq!(events(&queue, "planner_answer_wait").len(), 1);
    assert_eq!(backend.launched().len(), 1, "its place is not free yet");

    // Its session ended: the row closes, and the revise gets its place.
    queue
        .planner_exited(planner.id, std::process::id(), 0)
        .unwrap();
    supervise(&fx, &backend, &reviewer, &clock, 12);
    let closed = events(&queue, "planner_closed");
    assert_eq!(closed[0]["planner_id"], json!(planner.id));
    assert_eq!(closed[0]["code"], "runtime_answer_wait", "{closed:?}");
    supervise(&fx, &backend, &reviewer, &clock, 13);
    let open = queue.planners(false).unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].proposal_id, Some(proposal));
    assert_eq!(backend.launched().len(), 2);
    // The ask stays open for the person, and the draft waits for it.
    let ask = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.id == asked.id)
        .expect("still open");
    assert_eq!(ask.answered_at, None);
    assert!(events(&queue, "planner_unresponsive").is_empty());
    let bundle = queue.draft_bundle(planner.id).unwrap().unwrap();
    assert_eq!(bundle.members[0].outcome.as_deref(), Some("answer_wait"));
}

/// ADR-t1704-1 decision 1 (acceptance (a)): a planner whose question waits
/// on a person is not ended while it has more to wait for: a follow-up
/// request a person handed it waits in its `turns/`, or the answer of
/// another question was sent to it and not read yet.
#[test]
fn a_planner_with_a_follow_up_request_or_an_unread_answer_is_not_ended_for_its_question() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[]);
    let clock = Arc::new(Ahead::default());
    // An answer sent and not read yet (its request waits in `turns/`), and
    // a second question not answered.
    let (planner, dir) = answered_draft_planner(&fx, &mut queue, &backend, &reviewer, &clock);
    let draft = queue.planner(planner).unwrap().draft_task_id.unwrap();
    question(&mut queue, draft, "and the order?");
    taken_without_a_turn(&dir);
    touch_ahead(&planner_idle_marker(&dir), 25);
    supervise(&fx, &backend, &reviewer, &clock, 20);
    assert!(!exit_requested(&fx.db, planner));
    assert!(events(&queue, "planner_answer_wait").is_empty());

    // It read the answer in the turn of its request; a follow-up request
    // waits for its next turn.
    let seq = turn_requests(&fx.db, planner)[0]["seq"].clone();
    for (kind, payload) in [
        (
            dagq::domain::EventKind::TurnStarted,
            json!({"planner_id": planner, "turn": 1, "request": seq}),
        ),
        (
            dagq::domain::EventKind::TurnFinished,
            json!({"planner_id": planner, "turn": 1, "outcome": "succeeded"}),
        ),
    ] {
        queue.record_queue_event(kind, payload).unwrap();
    }
    take_turns(&queue, &fx.db, planner);
    crate::common::cli::ok(
        &fx.db,
        &[
            "planner",
            "request",
            &planner.to_string(),
            "--text",
            "also weigh the order",
        ],
    );
    supervise(&fx, &backend, &reviewer, &clock, 30);
    assert!(!exit_requested(&fx.db, planner));
    assert!(events(&queue, "planner_answer_wait").is_empty());

    // Once it took the request in a turn, only the person is left.
    take_turns(&queue, &fx.db, planner);
    supervise(&fx, &backend, &reviewer, &clock, 40);
    assert!(exit_requested(&fx.db, planner));
    assert_eq!(events(&queue, "planner_answer_wait").len(), 1);
}

/// ADR-t1704-1 decision 2 (acceptance (b)): an answer given after the
/// planner was asked to exit for it is neither sent to that planner nor
/// lost: it waits until the row is closed and the place is free, and then
/// one new planner carries it, once, with the question, the answer and the
/// planner's note.
#[test]
fn an_answer_given_as_its_planner_is_ended_goes_once_to_the_next_planner() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[]);
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
    queue
        .register_planner_agent(planner.id, std::process::id(), std::process::id())
        .unwrap();
    heartbeat_ahead(&fx.db);
    use dagq::domain::actor::{ActorContext, ActorRole};
    SqliteQueue::open(&fx.db)
        .unwrap()
        .with_actor(ActorContext::instance(ActorRole::Planner, planner.id))
        .add_note(dagq::domain::NewNote {
            target: dagq::domain::NoteTarget::Task(draft),
            text: "decided: adopt unless out of the goal".into(),
            kind: None,
            by: "planner".into(),
        })
        .unwrap();
    let asked = question(&mut queue, draft, "is this in the goal?");
    let dir = planners_dir(&fx.db).join(planner.id.to_string());
    touch_ahead(&planner_idle_marker(&dir), 2);
    supervise(&fx, &backend, &reviewer, &clock, 1);
    assert!(exit_requested(&fx.db, planner.id));

    // Answered before its session ended: nothing goes to it.
    queue.answer(asked.id, "adopt").unwrap();
    for at in [2, 3] {
        supervise(&fx, &backend, &reviewer, &clock, at);
    }
    assert!(turn_requests(&fx.db, planner.id).is_empty());
    assert_eq!(queue.planners(false).unwrap().len(), 1);
    assert!(events(&queue, "planner_answer_claimed").is_empty());

    queue
        .planner_exited(planner.id, std::process::id(), 0)
        .unwrap();
    for at in [4, 5, 6] {
        supervise(&fx, &backend, &reviewer, &clock, at);
    }
    let open = queue.planners(false).unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_ne!(open[0].id, planner.id);
    assert_eq!(open[0].draft_task_id, Some(draft));
    let prompt = crate::plan_review::planner_prompt(&fx.db, open[0].id);
    for carried in [
        "is this in the goal?",
        &format!("answer to ask {}: adopt", asked.id),
        "decided: adopt unless out of the goal",
    ] {
        assert!(prompt.contains(carried), "{carried}: {prompt}");
    }
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    let delivered: Vec<Value> = queue
        .show(draft)
        .unwrap()
        .events
        .into_iter()
        .filter(|event| event.kind == "ask_delivered")
        .map(|event| event.payload)
        .collect();
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert_eq!(backend.launched().len(), 2);
    assert!(turn_requests(&fx.db, open[0].id).is_empty());
}

/// An answer whose sending to its draft's planner fails (the request
/// cannot be written to its `turns/`) is not left to the inbox: the
/// failure is recorded once as `ask_delivery_failed`, the planner is marked
/// as one ended for the answer (`planner_answer_wait` naming the ask) and
/// asked to exit, and is not sent the answer again. Once its session ended
/// its row closes as `runtime_answer_wait`, its place under
/// `--runtime-planners` (1 here) is free, and one new planner carries the
/// answer and the planner's note; no `ask close` is needed.
#[test]
fn an_answer_that_could_not_be_sent_to_its_planner_goes_once_to_a_new_planner_and_frees_the_place()
{
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[]);
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
    queue
        .register_planner_agent(planner.id, std::process::id(), std::process::id())
        .unwrap();
    heartbeat_ahead(&fx.db);
    use dagq::domain::actor::{ActorContext, ActorRole};
    SqliteQueue::open(&fx.db)
        .unwrap()
        .with_actor(ActorContext::instance(ActorRole::Planner, planner.id))
        .add_note(dagq::domain::NewNote {
            target: dagq::domain::NoteTarget::Task(draft),
            text: "decided: adopt unless out of the goal".into(),
            kind: None,
            by: "planner".into(),
        })
        .unwrap();
    let asked = question(&mut queue, draft, "is this in the goal?");
    let dir = planners_dir(&fx.db).join(planner.id.to_string());
    touch_ahead(&planner_idle_marker(&dir), 1);
    queue.answer(asked.id, "adopt").unwrap();
    block_next_request(&fx.db, planner.id, 0);
    supervise(&fx, &backend, &reviewer, &clock, 10);

    let failed: Vec<Value> = queue
        .show(draft)
        .unwrap()
        .events
        .into_iter()
        .filter(|event| event.kind == "ask_delivery_failed")
        .map(|event| event.payload)
        .collect();
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["ask_id"], json!(asked.id));
    assert_eq!(failed[0]["planner_id"], json!(planner.id));
    assert!(turn_requests(&fx.db, planner.id).is_empty());
    let waited = events(&queue, "planner_answer_wait");
    assert_eq!(waited.len(), 1, "{waited:?}");
    assert_eq!(waited[0]["planner_id"], json!(planner.id));
    assert_eq!(waited[0]["asks"], json!([asked.id]));
    assert!(queue.planner(planner.id).unwrap().answer_wait_at.is_some());
    assert!(exit_requested(&fx.db, planner.id));

    // Not sent again, nor recorded as failed again, while it exits.
    supervise(&fx, &backend, &reviewer, &clock, 11);
    assert!(turn_requests(&fx.db, planner.id).is_empty());
    assert_eq!(
        queue
            .show(draft)
            .unwrap()
            .events
            .iter()
            .filter(|event| event.kind == "ask_delivery_failed")
            .count(),
        1
    );
    assert_eq!(backend.launched().len(), 1, "its place is not free yet");

    // Its session ended: the row closes, and one new planner carries the
    // answer in its place.
    queue
        .planner_exited(planner.id, std::process::id(), 0)
        .unwrap();
    for at in [12, 13] {
        supervise(&fx, &backend, &reviewer, &clock, at);
    }
    let closed = events(&queue, "planner_closed");
    assert_eq!(closed[0]["planner_id"], json!(planner.id));
    assert_eq!(closed[0]["code"], "runtime_answer_wait", "{closed:?}");
    let open = queue.planners(false).unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_ne!(open[0].id, planner.id);
    assert_eq!(open[0].draft_task_id, Some(draft));
    let prompt = crate::plan_review::planner_prompt(&fx.db, open[0].id);
    for carried in [
        &format!("answer to ask {}: adopt", asked.id),
        "decided: adopt unless out of the goal",
    ] {
        assert!(prompt.contains(carried), "{carried}: {prompt}");
    }
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    let delivered = queue
        .show(draft)
        .unwrap()
        .events
        .into_iter()
        .filter(|event| event.kind == "ask_delivered")
        .count();
    assert_eq!(delivered, 1);
    assert_eq!(backend.launched().len(), 2);
    assert!(turn_requests(&fx.db, open[0].id).is_empty());
}

/// Two answers go to one planner in a pass and only the second cannot be
/// sent: the planner is not asked to exit before it took the first up in
/// a turn (the exit would drop that request, whose ask is closed). Once
/// it did, it is ended for the second, its mark naming it, and a new
/// planner carries that answer.
#[test]
fn a_planner_is_ended_for_an_answer_it_could_not_be_sent_only_after_reading_the_one_sent() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[]);
    let clock = Arc::new(Ahead::default());
    let goal = open_goal(&mut queue);
    let draft = runtime_draft(
        &mut queue,
        "gap",
        Some(goal),
        DraftOrigin::FollowUp,
        json!({"source_task_id": 1, "source_run_id": "run-a", "index": 0}),
    );
    let other = runtime_draft(
        &mut queue,
        "order",
        Some(goal),
        DraftOrigin::FollowUp,
        json!({"source_task_id": 1, "source_run_id": "run-a", "index": 1}),
    );
    supervise(&fx, &backend, &reviewer, &clock, 0);
    let planner = queue.planners(false).unwrap().remove(0);
    assert_eq!(queue.planners(false).unwrap().len(), 1, "one bundle");
    queue
        .register_planner_wrapper(planner.id, std::process::id())
        .unwrap();
    queue
        .register_planner_agent(planner.id, std::process::id(), std::process::id())
        .unwrap();
    heartbeat_ahead(&fx.db);
    let first = question(&mut queue, draft, "is this in the goal?");
    let second = question(&mut queue, other, "and the order?");
    let dir = planners_dir(&fx.db).join(planner.id.to_string());
    touch_ahead(&planner_idle_marker(&dir), 1);
    queue.answer(first.id, "adopt").unwrap();
    queue.answer(second.id, "cancel").unwrap();
    block_next_request(&fx.db, planner.id, 1);
    supervise(&fx, &backend, &reviewer, &clock, 10);
    let requests = turn_requests(&fx.db, planner.id);
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(
        requests[0]["prompt"],
        format!("answer to ask {}: adopt", first.id)
    );
    let failed = queue
        .show(other)
        .unwrap()
        .events
        .into_iter()
        .filter(|event| event.kind == "ask_delivery_failed")
        .count();
    assert_eq!(failed, 1);
    assert!(!exit_requested(&fx.db, planner.id));
    assert!(events(&queue, "planner_answer_wait").is_empty());

    // It took the first up: ended for the second.
    take_turns(&queue, &fx.db, planner.id);
    touch_ahead(&planner_idle_marker(&dir), 21);
    supervise(&fx, &backend, &reviewer, &clock, 20);
    assert!(exit_requested(&fx.db, planner.id));
    let waited = events(&queue, "planner_answer_wait");
    assert_eq!(waited.len(), 1, "{waited:?}");
    assert_eq!(waited[0]["asks"], json!([second.id]));
    assert_eq!(turn_requests(&fx.db, planner.id).len(), 1);

    queue
        .planner_exited(planner.id, std::process::id(), 0)
        .unwrap();
    for at in [22, 23] {
        supervise(&fx, &backend, &reviewer, &clock, at);
    }
    let open = queue.planners(false).unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_ne!(open[0].id, planner.id);
    let prompt = crate::plan_review::planner_prompt(&fx.db, open[0].id);
    assert!(
        prompt.contains(&format!("answer to ask {}: cancel", second.id)),
        "{prompt}"
    );
    assert!(queue.asks(Default::default()).unwrap().is_empty());
}

/// A runtime's planner whose wrapper runs but has not recorded its agent's
/// pid is `opening`, whatever its idle marker shows, and is not asked to
/// exit; once the agent is recorded it is `idle` with its pid (task 1329:
/// `dagq planners` showed idle planners without `agent_pid`). Moved here
/// from `planner_screen_idle` by task 1441, without the screen.
#[test]
fn a_runtime_planner_is_idle_only_once_its_agent_is_recorded() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    runtime_draft(
        &mut queue,
        "gap",
        Some(goal),
        DraftOrigin::GoalGap,
        json!({"findings": ["the acceptance names a check nobody runs"]}),
    );
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[]);
    let clock = Arc::new(Ahead::default());
    supervise(&fx, &backend, &reviewer, &clock, 0);
    let planner = queue.planners(false).unwrap().remove(0);
    assert_eq!(planner.origin, PlannerOrigin::Runtime);
    let me = std::process::id();
    queue.register_planner_wrapper(planner.id, me).unwrap();
    heartbeat_ahead(&fx.db);
    let dir = planners_dir(&fx.db).join(planner.id.to_string());
    fs::write(planner_idle_marker(&dir), "{}").unwrap();
    let listed =
        || dagq::lifecycle::planners(&fx.db, &backend, false).unwrap()["planners"][0].clone();

    for at in [1, 7] {
        supervise(&fx, &backend, &reviewer, &clock, at);
        let view = listed();
        assert_eq!(view["state"], "opening", "{view}");
        assert_eq!(view["alive"], true);
        assert_eq!(view["idle_since"], Value::Null);
        assert_eq!(view["agent_pid"], Value::Null);
    }
    assert!(!exit_requested(&fx.db, planner.id));

    queue.register_planner_agent(planner.id, me, me).unwrap();
    let view = listed();
    assert_eq!(view["state"], "idle", "{view}");
    assert!(view["idle_since"].is_i64(), "{view}");
    assert_eq!(view["agent_pid"], json!(me));
}

/// Without `--runtime-planners`, `[supervisor] runtime_planners` of the
/// main checkout's `dagq.toml` is the limit on the runtime's planners; the
/// flag wins over it (task 941).
#[test]
fn the_supervisor_table_sets_the_limit_of_the_runtimes_planners() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    for title in ["one", "two", "three"] {
        runtime_draft(
            &mut queue,
            title,
            Some(goal),
            DraftOrigin::GoalGap,
            json!({"findings": [format!("{title} is not checked")]}),
        );
    }
    fs::write(
        fx.repo.join("dagq.toml"),
        "[supervisor]\nruntime_planners = 2\n",
    )
    .unwrap();
    let reviewer = StubReviewer::new(&[]);
    let backend = PlanWorkspace::default();
    let from_file = SuperviseOptions {
        runtime_planners: None,
        ..options(1, Duration::from_secs(3600))
    };
    supervise_with(&fx, &backend, &reviewer, &from_file);
    assert_eq!(queue.planners(false).unwrap().len(), 2);
    // The limit holds while both are at work.
    supervise_with(&fx, &backend, &reviewer, &from_file);
    assert_eq!(queue.planners(false).unwrap().len(), 2);

    // The flag wins over the table.
    supervise_with(
        &fx,
        &backend,
        &reviewer,
        &options(3, Duration::from_secs(3600)),
    );
    assert_eq!(queue.planners(false).unwrap().len(), 3);
}

/// ADR-t1704-1 decision 4 (acceptance (d)): the planner a revise went to
/// asks a person about a task of the proposal and has nothing else it can
/// fix: it is ended (`planner_answer_wait` with `revise`), and the revise
/// it left is not given to a new planner without the answer, even past
/// the planner timeout, nor told to the inbox as one with no planner. The
/// answer and the revise go together to one new planner.
#[test]
fn a_revise_stopped_at_a_question_waits_for_the_answer_and_goes_with_it_to_one_new_planner() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let backend = PlanWorkspace::default();
    let reviewer = StubReviewer::new(&[json!({
        "verdict": "revise", "reasons": ["split it"], "summary": "too big"
    })]);
    let clock = Arc::new(Ahead::default());
    let task = add(&mut queue, "split", &[TaskId::new(1)], Priority::Normal);
    let proposal = submit(&mut queue, &[task], Some("GONE"));
    supervise(&fx, &backend, &reviewer, &clock, 0);
    supervise(&fx, &backend, &reviewer, &clock, 1);
    let planner = queue.planners(false).unwrap().remove(0);
    assert_eq!(planner.proposal_id, Some(proposal));
    queue
        .register_planner_wrapper(planner.id, std::process::id())
        .unwrap();
    queue
        .register_planner_agent(planner.id, std::process::id(), std::process::id())
        .unwrap();
    heartbeat_ahead(&fx.db);
    let asked = question(&mut queue, task, "split by layer or by feature?");
    let dir = planners_dir(&fx.db).join(planner.id.to_string());
    touch_ahead(&planner_idle_marker(&dir), 3);
    supervise(&fx, &backend, &reviewer, &clock, 2);
    assert!(exit_requested(&fx.db, planner.id));
    let waited = events(&queue, "planner_answer_wait");
    assert_eq!(waited.len(), 1, "{waited:?}");
    assert_eq!(waited[0]["revise"], true);
    assert_eq!(waited[0]["proposal_id"], json!(proposal));
    queue
        .planner_exited(planner.id, std::process::id(), 0)
        .unwrap();
    for at in [3, TIMEOUT_SECS as i64 + 10] {
        supervise(&fx, &backend, &reviewer, &clock, at);
    }
    assert!(queue.planners(false).unwrap().is_empty());
    assert_eq!(backend.launched().len(), 1, "not started again unanswered");
    assert!(
        events(&queue, "planner_unresponsive").is_empty(),
        "{:?}",
        events(&queue, "planner_unresponsive")
    );
    assert_eq!(
        queue.show_proposal(proposal).unwrap().status(),
        ProposalStatus::Revising
    );

    queue.answer(asked.id, "by layer").unwrap();
    let at = TIMEOUT_SECS as i64 + 12;
    supervise(&fx, &backend, &reviewer, &clock, at);
    supervise(&fx, &backend, &reviewer, &clock, at + 1);
    let open = queue.planners(false).unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].proposal_id, Some(proposal));
    assert_eq!(backend.launched().len(), 2);
    let prompt = crate::plan_review::planner_prompt(&fx.db, open[0].id);
    for carried in [
        "split it",
        "split by layer or by feature?",
        &format!("answer to ask {}: by layer", asked.id),
        &format!("What planner {} before you left", planner.id),
    ] {
        assert!(prompt.contains(carried), "{carried}: {prompt}");
    }
    assert!(queue.asks(Default::default()).unwrap().is_empty());
}
