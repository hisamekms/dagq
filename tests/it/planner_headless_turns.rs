//! How the turns of a headless planner of the runtime's end, as the
//! supervisor acts on them (ADR-t1394-2 decisions 3 and 5), on the
//! boundary of the background wrapper, its `turns/` and the supervisor's
//! pass: a turn that failed at Claude's usage limit waits in the queue's
//! hold and goes on with the same call, carrying the answer, once the hold
//! is answered (and a request written during that turn waits behind the
//! retry), a turn stopped at its limit tells the inbox, a planning
//! request's planner ends in `proposed` and is closed at the end of its
//! turns, a live one takes the next revise of its proposal as its next
//! turn, and the sweep tells of a stopped turn of a planner it closes.
//! The decisions are unit tests (task 1711): the wall's step, hold and
//! `provider_waiting` in `application::supervise::planner_turns`, where an
//! answer goes and when in `domain::planner::answer_waits` and
//! `domain::plan_request`, and `planner_closed` in `application::planner`.

use crate::plan_review::{
    PlanWorkspace, StubReviewer, add, open_goal, options, runtime_draft, supervise_with,
};
use crate::planner_headless::{diagnose_planner, headless_fixture, queue_events, supervise_until};
use dagq::application::{PlanRequestStore, TaskStore};
use dagq::{
    domain::{
        AskKind, DraftOrigin, PlannerId, PlannerRoute, Priority, RequestId, TaskId, TaskStatus,
        plan_request::{NewPlanRequest, RequestStatus},
    },
    infrastructure::{location::planners_dir, sqlite::SqliteQueue},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

/// A request the inbox recorded.
fn record(queue: &mut SqliteQueue, text: &str) -> RequestId {
    queue
        .record_plan_request(
            &NewPlanRequest {
                text: text.into(),
                note: None,
                refs: Vec::new(),
            },
            "inbox",
            "inbox",
        )
        .unwrap()
        .id
}

/// The turn requests planner `id` took, in order.
fn taken(db: &Path, id: PlannerId) -> Vec<Value> {
    let turns = planners_dir(db).join(id.to_string()).join("turns");
    let mut names: Vec<String> = fs::read_dir(&turns)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".taken.json"))
        .collect();
    names.sort();
    names
        .iter()
        .map(|name| serde_json::from_str(&fs::read_to_string(turns.join(name)).unwrap()).unwrap())
        .collect()
}

/// Acceptance (task 1596): a follow-up request written while the turn of
/// the answer runs waits behind it when that turn fails at Claude's usage
/// limit: the wrapper takes nothing before the `provider retry`, so the
/// planner is idle at the wall and waits in the queue's hold, and once the
/// hold is answered the retry carries the answer and the follow-up comes
/// after it; the follow-up's turn finishes the draft and the planner is
/// closed.
#[test]
fn a_request_written_during_a_turn_at_the_usage_limit_waits_for_the_retry_of_that_turn() {
    let fx = headless_fixture(
        r#"case "$TURN" in
1) "$DAGQ" --db "$DB" ask --kind planner_question --because scope --task 2 --question "in the goal?" --cmux true >> "$RUN_DIR/ask.log" 2>&1 ;;
2) await_file "$RUN_DIR/turns/request-000002.json"
  fail "Claude AI usage limit reached|1790535600" ;;
*) case "$PROMPT" in *"a request for you"*) "$DAGQ" --db "$DB" cancel 2 >> "$RUN_DIR/cancel.log" 2>&1 ;; esac ;;
esac
say "turn $TURN""#,
    );
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let draft = runtime_draft(
        &mut queue,
        "gap",
        Some(goal),
        DraftOrigin::FollowUp,
        json!({"source_task_id": 1, "source_run_id": null, "index": 0}),
    );
    assert_eq!(draft, TaskId::new(2));
    let reviewer = StubReviewer::new(&[]);
    let backend = PlanWorkspace::running();
    let open_ask = |kind: AskKind| {
        SqliteQueue::open(&fx.db)
            .unwrap()
            .asks(Default::default())
            .unwrap()
            .into_iter()
            .find(|ask| ask.kind == kind)
    };
    supervise_until(
        &fx,
        &backend,
        &reviewer,
        || open_ask(AskKind::PlannerQuestion).is_some(),
        || diagnose_planner(&fx.db),
    );
    let asked = open_ask(AskKind::PlannerQuestion).unwrap();
    queue.answer(asked.id, "keep it").unwrap();
    // The answer's turn runs; the inbox hands a follow-up meanwhile.
    supervise_until(
        &fx,
        &backend,
        &reviewer,
        || queue_events(&fx.db, "turn_started").len() == 2,
        || diagnose_planner(&fx.db),
    );
    let handed = crate::common::cli::ok_as(
        "inbox",
        &fx.db,
        &[
            "planner",
            "request",
            "1",
            "--text",
            "also split the docs",
            "--cmux",
            "true",
        ],
    );
    assert_eq!(handed["seq"], 2, "{handed}");
    supervise_until(
        &fx,
        &backend,
        &reviewer,
        || open_ask(AskKind::QueueHold).is_some(),
        || diagnose_planner(&fx.db),
    );
    for _ in 0..3 {
        supervise_with(
            &fx,
            &backend,
            &reviewer,
            &options(1, Duration::from_secs(3600)),
        );
    }
    // At the wall: the follow-up is not run against the Claude that cannot
    // be used, and the planner waits in the hold, not ended.
    let dir = planners_dir(&fx.db).join("1");
    let turns = dir.join("turns");
    assert!(turns.join("request-000002.json").is_file(), "not taken");
    assert_eq!(queue_events(&fx.db, "turn_started").len(), 2);
    assert!(!turns.join("exit").exists(), "not ended");
    assert!(
        queue
            .planner(PlannerId::new(1))
            .unwrap()
            .closed_at
            .is_none()
    );
    let waiting = queue_events(&fx.db, "provider_waiting");
    assert_eq!(waiting.len(), 1, "{waiting:?}");
    assert_eq!(waiting[0]["planner_id"], 1);
    assert_eq!(waiting[0]["turn"], 2);
    assert_eq!(waiting[0]["reason"], "usage_limit");
    let hold = open_ask(AskKind::QueueHold).unwrap();
    assert_eq!(waiting[0]["ask_id"], hold.id.as_i64());

    queue.answer(hold.id, "done").unwrap();
    supervise_until(
        &fx,
        &backend,
        &reviewer,
        || !queue_events(&fx.db, "planner_closed").is_empty(),
        || diagnose_planner(&fx.db),
    );
    // The answer, its retry, then the follow-up: nothing is lost.
    let requests = taken(&fx.db, PlannerId::new(1));
    let whats: Vec<&str> = requests
        .iter()
        .map(|r| r["what"].as_str().unwrap())
        .collect();
    assert_eq!(
        whats,
        [
            format!("answer of ask {}", asked.id).as_str(),
            "follow-up request followup-1",
            "provider retry",
        ],
        "by seq"
    );
    let started: Vec<Value> = queue_events(&fx.db, "turn_started");
    let order: Vec<&str> = started
        .iter()
        .map(|s| s["what"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(started.len(), 4, "{started:?}");
    assert_eq!(order[2], "provider retry", "{order:?}");
    assert_eq!(order[3], "follow-up request followup-1", "{order:?}");
    let retry = requests[2]["prompt"].as_str().unwrap();
    assert!(retry.contains("can be used again"), "{retry}");
    assert!(
        retry.contains(&format!("answer to ask {}: keep it", asked.id)),
        "{retry}"
    );
    assert_eq!(
        queue.show(draft).unwrap().task.status(),
        TaskStatus::Canceled
    );
    assert_eq!(queue.planners(true).unwrap().len(), 1);
    let closed = queue_events(&fx.db, "planner_closed");
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0]["code"], "runtime_exited");
    assert!(queue_events(&fx.db, "planner_unresponsive").is_empty());
    assert!(backend.texts().is_empty(), "nothing typed");
}

/// Acceptance (ADR-t1394-2 decision 3): a headless planner's turn that runs
/// past `[stall].turn_limit_secs` is stopped by its wrapper, the inbox is
/// told once with `planner_unresponsive` naming the planner and the turn,
/// and the planner, its session over, is closed as exited.
#[test]
fn a_headless_planners_turn_stopped_at_its_limit_tells_the_inbox_and_the_planner_closes() {
    let fx = headless_fixture(
        r#"if [ ! -e "$DB.slept" ]; then : > "$DB.slept"; sleep 30; fi
say "turn $TURN""#,
    );
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let request = record(&mut queue, "plan the landing rate back");
    let reviewer = StubReviewer::new(&[]);
    let backend = PlanWorkspace::running();
    let settings = dagq::runtime::SuperviseOptions {
        stall: Some(
            dagq::domain::stall::StallConfig::default().with_millis("turn_limit_secs", 500),
        ),
        ..options(1, Duration::from_secs(3600))
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    let first_closed = || {
        queue_events(&fx.db, "planner_closed")
            .iter()
            .any(|closed| closed["planner_id"] == 1)
    };
    while !first_closed() {
        supervise_with(&fx, &backend, &reviewer, &settings);
        assert!(Instant::now() < deadline, "{}", diagnose_planner(&fx.db));
        thread::sleep(Duration::from_millis(50));
    }
    let finished = queue_events(&fx.db, "turn_finished");
    assert_eq!(finished[0]["planner_id"], 1);
    assert_eq!(finished[0]["outcome"], "timed_out", "{finished:?}");
    let told = queue_events(&fx.db, "planner_unresponsive");
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["subject"], "planner");
    assert_eq!(told[0]["planner_id"], 1);
    assert_eq!(told[0]["route"], "headless");
    assert_eq!(told[0]["request_id"], request.as_i64());
    assert_eq!(told[0]["turn"], 1);
    assert_eq!(told[0]["outcome"], "timed_out");
    let closed = queue_events(&fx.db, "planner_closed");
    let closed = closed.iter().find(|c| c["planner_id"] == 1).unwrap();
    assert_eq!(closed["code"], "runtime_exited", "{closed}");
    assert_eq!(closed["request_id"], request.as_i64());
    // Its request is still open: the planner ended undecided.
    assert_ne!(
        queue.plan_request(request).unwrap().status,
        RequestStatus::Exhausted
    );
}

/// Acceptance (ADR-t1394-1 decision 6 on the headless route): a request's
/// headless planner that submits a proposal in its turn makes the request
/// `proposed` with the proposal linked, and, idle with nothing left, gets
/// the exit request and is closed with the request's ID.
#[test]
fn a_headless_request_planners_submission_proposes_the_request_and_its_end_closes_it() {
    let fx = headless_fixture(
        r#""$DAGQ" --db "$DB" submit 2 >> "$RUN_DIR/submit.log" 2>&1
say "turn $TURN""#,
    );
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(
        &mut queue,
        "bring it back",
        &[TaskId::new(1)],
        Priority::Normal,
    );
    assert_eq!(task, TaskId::new(2));
    let request = record(&mut queue, "bring the landing rate back");
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let backend = PlanWorkspace::running();
    supervise_until(
        &fx,
        &backend,
        &reviewer,
        || !queue_events(&fx.db, "planner_closed").is_empty(),
        || diagnose_planner(&fx.db),
    );
    let proposed = queue.plan_request(request).unwrap();
    assert_eq!(
        proposed.status,
        RequestStatus::Proposed,
        "{}",
        diagnose_planner(&fx.db)
    );
    let told = queue_events(&fx.db, "request_proposed");
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["planner_id"], 1);
    let planner = queue.planner(PlannerId::new(1)).unwrap();
    assert_eq!(planner.route, PlannerRoute::Headless);
    assert_eq!(planner.request_id, Some(request));
    assert_eq!(planner.exit_code, Some(0));
    let turns = planners_dir(&fx.db).join("1").join("turns");
    assert!(turns.join("exit").is_file());
    assert!(turns.join("turn-000001.jsonl").is_file());
    assert!(!turns.join("turn-000002.jsonl").exists());
    let closed = queue_events(&fx.db, "planner_closed");
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0]["code"], "runtime_exited");
    assert_eq!(closed[0]["request_id"], request.as_i64());
    assert_eq!(queue.planners(true).unwrap().len(), 1, "no second planner");
    assert!(backend.texts().is_empty(), "nothing typed");
}

/// Acceptance (ADR-t1394-2 decision 2 for a revise to a live planner): a
/// headless planner that submitted its proposal again and is still alive
/// gets the next revise of that proposal as its next turn's request
/// (`what: revise`, recorded as `turn_requested`), not a new planner: its
/// first turn waits until plan review sent the proposal back, so it is
/// alive and at work when the revise comes, and idle when it is delivered.
/// Its second turn submits again, plan review accepts, and the exit
/// request ends it.
#[test]
fn a_live_headless_planner_takes_the_next_revise_of_its_proposal_as_its_next_turn() {
    let fx = headless_fixture(
        r#""$DAGQ" --db "$DB" submit --proposal 1 >> "$RUN_DIR/submit.log" 2>&1
if [ "$TURN" = 1 ]; then
  i=0
  while [ $i -lt 600 ]; do
    "$DAGQ" --db "$DB" proposal show 1 2>> "$RUN_DIR/show.log" | grep -q '"revising"' && break
    sleep 0.1; i=$((i + 1))
  done
fi
say "turn $TURN""#,
    );
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, "change", &[TaskId::new(1)], Priority::Normal);
    let proposal = crate::plan_review::submit(&mut queue, &[task], None);
    assert_eq!(proposal.as_i64(), 1);
    let reviewer = StubReviewer::new(&[
        json!({"verdict": "revise", "reasons": ["split it"], "summary": "not yet"}),
        json!({"verdict": "revise", "reasons": ["name the test"], "summary": "not yet"}),
        json!({"verdict": "pass", "reasons": [], "summary": "ok"}),
    ]);
    let backend = PlanWorkspace::running();
    supervise_until(
        &fx,
        &backend,
        &reviewer,
        || {
            !queue_events(&fx.db, "planner_closed").is_empty()
                && queue.show_proposal(proposal).unwrap().status()
                    == dagq::domain::ProposalStatus::Accepted
        },
        || diagnose_planner(&fx.db),
    );

    // One planner took both revises: the first in its prompt, the second
    // as the request of its next turn.
    let planners = queue.planners(true).unwrap();
    assert_eq!(planners.len(), 1, "{planners:?}");
    let planner = planners[0].clone();
    assert_eq!(planner.route, PlannerRoute::Headless);
    let dir = planners_dir(&fx.db).join(planner.id.to_string());
    let prompt = fs::read_to_string(dir.join("prompt.txt")).unwrap();
    assert!(prompt.contains("split it"), "{prompt}");
    let requests = taken(&fx.db, planner.id);
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0]["what"], "revise");
    let revise = requests[0]["prompt"].as_str().unwrap();
    assert!(revise.contains("name the test"), "{revise}");
    let requested = queue_events(&fx.db, "turn_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["planner_id"], planner.id.as_i64());
    assert_eq!(requested[0]["what"], "revise");
    assert_eq!(
        requested[0]["workspace_id"],
        planner.workspace_id.clone().unwrap()
    );
    let started = queue_events(&fx.db, "turn_started");
    assert_eq!(started.len(), 2, "{started:?}");
    assert_eq!(started[1]["resume"], true);
    assert_eq!(started[1]["request"], 1);
    let sent = crate::plan_review::events(&mut queue, task, "plan_revise_sent");
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[0]["opened"], true);
    assert_ne!(sent[1]["opened"], true, "{sent:?}");
    assert_eq!(sent[1]["planner_id"], planner.id.as_i64());
    // Then the exit request ended it.
    assert!(dir.join("turns").join("exit").is_file());
    let closed = queue_events(&fx.db, "planner_closed");
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0]["code"], "runtime_exited");
    assert_eq!(queue.planner(planner.id).unwrap().exit_code, Some(0));
    assert!(backend.texts().is_empty(), "nothing typed");
}

/// Acceptance (ADR-t1394-2 decision 3, the sweep's path): a headless
/// planner whose wrapper stopped its last turn at its limit and ended is
/// closed by the supervisor's sweep, and the sweep tells the inbox of the
/// stopped turn once. Under `--no-claude` the pass tends no planner, so
/// only the sweep can tell.
#[test]
fn the_sweep_tells_of_a_headless_planners_turn_stopped_at_its_limit_as_it_closes_it() {
    use dagq::application::WorkspaceBackend;
    let fx = crate::plan_review::fixture();
    let queue = SqliteQueue::open(&fx.db).unwrap();
    let backend = PlanWorkspace::running();
    let log = fx.db.parent().unwrap().join("wrapper.log");
    let handle = backend
        .launch_background(fx.db.parent().unwrap(), "sleep 600", &[], &log)
        .unwrap();
    let wrapper = dagq::domain::background_wrapper::BackgroundHandle::parse(&handle)
        .unwrap()
        .pid;
    let planner = queue
        .open_planner(dagq::domain::PlannerOrigin::Runtime, None)
        .unwrap();
    queue
        .set_planner_route(planner.id, PlannerRoute::Headless)
        .unwrap();
    queue
        .planner_workspace_created(planner.id, &handle)
        .unwrap();
    queue.register_planner_wrapper(planner.id, wrapper).unwrap();
    // Its turn 1 was stopped past its limit, and its wrapper ended.
    let dir = planners_dir(&fx.db).join(planner.id.to_string());
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("idle.json"),
        json!({"dagq_turn": {"turn": 1, "outcome": "timed_out", "failure": null, "permission_denials": 0}})
            .to_string(),
    )
    .unwrap();
    backend.close(&handle).unwrap();
    queue.planner_exited(planner.id, wrapper, 1).unwrap();
    let reviewer = StubReviewer::new(&[]);
    let settings = dagq::runtime::SuperviseOptions {
        no_claude: true,
        ..options(1, Duration::from_secs(3600))
    };
    supervise_with(&fx, &backend, &reviewer, &settings);
    let closed = queue_events(&fx.db, "planner_closed");
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0]["code"], "runtime_exited");
    let told = queue_events(&fx.db, "planner_unresponsive");
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["subject"], "planner");
    assert_eq!(told[0]["planner_id"], planner.id.as_i64());
    assert_eq!(told[0]["state"], "closed");
    assert_eq!(told[0]["turn"], 1);
    assert_eq!(told[0]["outcome"], "timed_out");
    // Once only.
    supervise_with(&fx, &backend, &reviewer, &settings);
    assert_eq!(queue_events(&fx.db, "planner_unresponsive").len(), 1);
}
