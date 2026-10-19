//! Runtime tests: the answers of `approve_landing` asks are applied on the
//! next pass whatever the slots, and a `land` answer queues the run, which
//! lands once there is room, before new claims (task 949).
use crate::common;
use crate::runtime_support;

use runtime_support::*;
use std::time::Instant;

/// The agent of task `n`: a change of its own (so the landings do not
/// conflict), its receipt, then idle until the supervisor's `/exit`.
fn own_change(n: i64) -> String {
    format!(
        "printf '{n}\\n' > f{n}.txt && git add f{n}.txt && git commit -q -m t{n}; receipt \"$(git rev-parse HEAD)\"; idle; await_exit"
    )
}

fn supervise_parallel(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    reviewer: &TestReviewer,
    parallel: usize,
) -> Value {
    let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
    let outcome = runtime::supervise_with_reviewer(
        db,
        repo,
        backend,
        &claude_stub(db),
        reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &supervise_options(parallel, true),
    )
    .unwrap();
    backend.join();
    outcome
}

/// The ID of the first event of `kind` of the task's latest run.
fn first_event(queue: &mut SqliteQueue, task: i64, kind: &str) -> i64 {
    queue
        .show(TaskId::new(task))
        .unwrap()
        .events
        .iter()
        .find(|e| e.kind == kind)
        .unwrap_or_else(|| panic!("task {task} has no {kind}"))
        .id
        .as_i64()
}

/// Poll `check` every tick until it holds, for at most 20 seconds.
fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(TEST_TICK);
    }
}

/// A `land` answer given while the only slot runs another task is applied
/// on the next pass: the ask closes, the approval and `landing_queued`
/// (`via: approve`) are recorded, and the run waits unleased. Once the
/// slot and the integration slot are free, it lands before the next task
/// is claimed. The answer gives a reason (`land: <reason>`), which chooses
/// `land` as the bare option does (`runtime_delivers`) and stays in the
/// ask's answer.
#[test]
fn a_land_answer_is_applied_while_the_slot_is_taken_and_lands_before_a_new_claim() {
    const LAND_WITH_A_REASON: &str = "land: review 3, the findings are docs only";
    let (fixture, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.script_for(1, &own_change(1));
    let reviewer = TestReviewer::new(&[
        verdict("concern", &["scope"], "a concern"),
        verdict("pass", &[], "fine"),
        verdict("pass", &[], "fine"),
    ]);
    let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 1);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue.asks(Default::default()).unwrap()[0].clone();
    assert_eq!(ask.kind, dagq::domain::AskKind::ApproveLanding);

    let marker = fixture.dir.path().join("go");
    add_ready_task(&mut queue, "second", &[]);
    add_ready_task(&mut queue, "third", &[]);
    backend.script_for(
        2,
        &format!("await_file {}; {}", shell_path(&marker), own_change(2)),
    );
    backend.script_for(3, &own_change(3));
    let answerer = {
        let db = db.clone();
        thread::spawn(move || {
            let mut queue = SqliteQueue::open(&db).unwrap();
            wait_until("task 2 to run", || {
                queue
                    .show(TaskId::new(2))
                    .unwrap()
                    .runs
                    .first()
                    .is_some_and(|run| run.status() == RunStatus::Running)
            });
            queue.answer(ask.id, LAND_WITH_A_REASON).unwrap();
            let delivered = delivers(&mut queue, ask.id);
            wait_until("the answer to be applied", || {
                queue.read_ask(ask.id).unwrap().closed_at.is_some()
            });
            // Applied while task 2 still holds the only slot.
            let second = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
            let first = queue.show(TaskId::new(1)).unwrap();
            let observed = (
                second.status(),
                first.runs[0].status(),
                payloads(&first, "landing_queued")
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>(),
                queue.run_lease(first.runs[0].id()).unwrap().is_some(),
                runtime::status(&db).unwrap(),
                delivered,
                queue.read_ask(ask.id).unwrap().answer,
            );
            fs::write(&marker, "").unwrap();
            observed
        })
    };
    let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 1);
    let (second, first, queued, leased, status, delivered, answer) = answerer.join().unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(second, RunStatus::Running);
    assert_eq!(first, RunStatus::AwaitingIntegration);
    assert_eq!(queued, [json!({"via": "approve", "ask_id": ask.id})]);
    assert!(!leased);
    assert_eq!(delivered, true);
    assert_eq!(answer.as_deref(), Some(LAND_WITH_A_REASON));
    let entry = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["task_id"] == 1)
        .unwrap_or_else(|| panic!("{status}"));
    assert_eq!(entry["next"], "queued to land (runtime)");
    assert_eq!(entry["kind"], "landing_queued");

    let mut queue = SqliteQueue::open(&db).unwrap();
    for task in 1..=3 {
        let detail = queue.show(TaskId::new(task)).unwrap();
        assert_eq!(detail.runs[0].status(), RunStatus::Integrated, "{task}");
        assert_eq!(payloads(&detail, "integration_started").len(), 1, "{task}");
    }
    let approved = payloads(&queue.show(TaskId::new(1)).unwrap(), "integration_approved")
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(approved.len(), 1);
    assert_eq!(approved[0]["ask_id"], ask.id.as_i64());
    // Task 2 held the slot and landed first; the approved run landed
    // before task 3 was claimed.
    assert!(
        first_event(&mut queue, 2, "run_integrated")
            < first_event(&mut queue, 1, "integration_started")
    );
    assert!(
        first_event(&mut queue, 1, "run_integrated") < first_event(&mut queue, 3, "run_claimed")
    );
    assert_eq!(queue.asks(Default::default()).unwrap().len(), 0);
    assert_eq!(reviewer.prompts().len(), 3);
}

/// `land` answers are applied while a `[run.env]` program is missing, but
/// nothing lands until it is found; then the approved runs land in the
/// order they were approved, each once, under a supervisor other than the
/// one that applied the answers, without asking again or reviewing again.
#[test]
fn approved_runs_wait_for_the_landing_and_land_oldest_approval_first_once() {
    let (fixture, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.script_for(1, &own_change(1));
    backend.script_for(2, &own_change(2));
    let reviewer = TestReviewer::new(&[
        verdict("concern", &["scope"], "a concern"),
        verdict("concern", &["scope"], "a concern"),
    ]);
    let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 4);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 2);
    let mut ask_of = |task: i64| {
        let run = queue.show(TaskId::new(task)).unwrap().runs[0].clone();
        asks.iter()
            .find(|ask| ask.run_id.as_ref() == Some(run.id()))
            .unwrap()
            .id
    };
    let (first_ask, second_ask) = (ask_of(1), ask_of(2));

    // The main checkout's `dagq.toml` names a program that is not there.
    let tool = fixture.dir.path().join("bin").join("sccache");
    fs::write(
        repo.join("dagq.toml"),
        format!("[run.env]\nRUSTC_WRAPPER = '{}'\n", tool.display()),
    )
    .unwrap();
    // Task 2's run is approved first, then task 1's, each by a supervisor
    // of its own.
    for ask in [second_ask, first_ask] {
        queue.answer(ask, "land").unwrap();
        let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 4);
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        assert!(queue.read_ask(ask).unwrap().closed_at.is_some());
    }
    for task in [1, 2] {
        let detail = queue.show(TaskId::new(task)).unwrap();
        assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
        assert!(queue.run_lease(detail.runs[0].id()).unwrap().is_none());
        assert!(payloads(&detail, "integration_started").is_empty());
    }

    fs::create_dir_all(tool.parent().unwrap()).unwrap();
    crate::common::template::script(&tool, "#!/bin/sh\nexec \"$@\"\n");

    let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 4);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    for task in [1, 2] {
        let detail = queue.show(TaskId::new(task)).unwrap();
        assert_eq!(detail.runs[0].status(), RunStatus::Integrated, "{task}");
        assert_eq!(payloads(&detail, "integration_started").len(), 1);
        assert_eq!(payloads(&detail, "integration_approved").len(), 1);
        assert_eq!(payloads(&detail, "ask_opened").len(), 1);
    }
    assert!(
        first_event(&mut queue, 2, "run_integrated")
            < first_event(&mut queue, 1, "integration_started")
    );
    assert_eq!(reviewer.prompts().len(), 2);

    // `stats` counts the wait from the answer to the landing as the
    // approve's `landing_queue`.
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    let via = &stats["overall"]["land_phases"]["landing_queue_via"];
    assert_eq!(via[0]["via"], "approve", "{via}");
    assert_eq!(via[0]["count"], 2, "{via}");
    assert!(
        stats["asks"]["times"]["by_kind"]["approve_landing"]["answer_to_apply_without_slot_wait"]["count"]
            == 2,
        "{}",
        stats["asks"]
    );
}

/// A review job that prints no verdict, which runs once more and fails.
const NO_VERDICT: &str = "echo 'no verdict here'";

/// The `approve_landing` ask of task 1's run, its run and the run's
/// events of `kind`.
fn landing_ask(queue: &mut SqliteQueue) -> dagq::domain::Ask {
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, dagq::domain::AskKind::ApproveLanding);
    asks[0].clone()
}

/// The `next` of the attention of ask `ask` in `status`.
fn ask_next(db: &Path, ask: dagq::domain::AskId) -> Value {
    let status = runtime::status(db).unwrap();
    status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["ask_id"] == json!(ask))
        .unwrap_or_else(|| panic!("{status}"))["next"]
        .clone()
}

/// The `runtime_delivers` of the answer of ask `ask`.
fn delivers(queue: &mut SqliteQueue, ask: dagq::domain::AskId) -> Value {
    let answered = queue
        .show(TaskId::new(1))
        .unwrap()
        .events
        .into_iter()
        .find(|e| e.kind == "ask_answered" && e.payload["ask_id"] == json!(ask))
        .unwrap();
    answered.payload["runtime_delivers"].clone()
}

/// A failed review's ask offers `retry_review` (ADR-t2521-1): the runtime
/// takes the answer (`runtime_delivers`, `applying the answer`), closes
/// the ask, records `review_retry_queued` once, and reviews the run again
/// as the next attempt on the receipt it has, without resuming the session
/// or asking for a receipt; the passing review lands it. Later passes start
/// no review again.
#[test]
fn a_retry_review_answer_reviews_a_failed_review_s_run_again_without_a_resume() {
    let (_fixture, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.script_for(1, &own_change(1));
    let reviewer = TestReviewer::new(&[
        NO_VERDICT.to_owned(),
        NO_VERDICT.to_owned(),
        verdict("pass", &[], "fine"),
    ]);
    let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 1);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = landing_ask(&mut queue);
    assert!(ask.options.iter().any(|o| o == "retry_review"), "{ask:?}");
    assert!(
        ask.question.contains(
            "retry_review: run the review again on the same run and receipt (no change, no resume of the session)"
        ),
        "{}",
        ask.question
    );

    queue.answer(ask.id, "retry_review").unwrap();
    assert_eq!(delivers(&mut queue, ask.id), true);
    assert_eq!(
        ask_next(&db, ask.id),
        format!("applying the answer of ask {} (runtime)", ask.id)
    );
    let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 1);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    // Another supervisor's passes start no review again.
    let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 1);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 1);
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let queued = payloads(&detail, "review_retry_queued");
    assert_eq!(
        queued,
        [&json!({"ask_id": ask.id, "answer": "retry_review"})]
    );
    let attempts: Vec<_> = payloads(&detail, "review_started")
        .iter()
        .map(|p| p["attempt"].clone())
        .collect();
    assert_eq!(attempts, [json!(1), json!(2)]);
    assert!(payloads(&detail, "resume_started").is_empty());
    assert!(payloads(&detail, "landing_decided").is_empty());
    assert!(payloads(&detail, "review_outcome").is_empty());
    assert_eq!(payloads(&detail, "receipt_observed").len(), 1);
    assert_eq!(reviewer.prompts().len(), 3);
}

/// `retry_review` is no answer of the ask of a verdict (a `concern`): the
/// runtime does not take it (`runtime_delivers: false`, `read the
/// answer`), and the ask stays open with no review started again.
#[test]
fn a_retry_review_answer_to_a_concern_s_ask_is_left_to_the_inbox() {
    let (_fixture, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.script_for(1, &own_change(1));
    let reviewer = TestReviewer::new(&[verdict("concern", &["scope"], "a concern")]);
    let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 1);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = landing_ask(&mut queue);
    assert!(!ask.options.iter().any(|o| o == "retry_review"), "{ask:?}");

    queue.answer(ask.id, "retry_review").unwrap();
    assert_eq!(delivers(&mut queue, ask.id), false);
    assert_eq!(
        ask_next(&db, ask.id),
        format!("read the answer of ask {} and close it", ask.id)
    );
    let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 1);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_none());
    assert!(payloads(&detail, "review_retry_queued").is_empty());
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert_eq!(reviewer.prompts().len(), 1);
}

/// `cancel: <reason>` chooses `cancel` as the bare option does: the run
/// fails, its task is canceled and the ask closes, the answer with its
/// reason recorded on `landing_decided`.
#[test]
fn a_cancel_answer_with_a_reason_is_applied_as_cancel() {
    let (_fixture, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.script_for(1, &own_change(1));
    let reviewer = TestReviewer::new(&[verdict("concern", &["scope"], "a concern")]);
    let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 1);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = landing_ask(&mut queue);

    queue.answer(ask.id, "cancel: not needed").unwrap();
    assert_eq!(delivers(&mut queue, ask.id), true);
    let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 1);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let decided = payloads(&detail, "landing_decided");
    assert_eq!(decided.len(), 1, "{decided:?}");
    assert_eq!(decided[0]["answer"], "cancel: not needed", "{}", decided[0]);
    assert!(payloads(&detail, "integration_approved").is_empty());
}

/// A `retry_review` answered while the queue's hold ask holds Claude is
/// applied, and the review it queues waits held (no `review_started`)
/// until the hold is released; then it starts once.
#[test]
fn a_retried_review_waits_for_the_held_provider() {
    let (_fixture, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    backend.script_for(1, &own_change(1));
    let reviewer = TestReviewer::new(&[
        NO_VERDICT.to_owned(),
        NO_VERDICT.to_owned(),
        verdict("pass", &[], "fine"),
    ]);
    let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 1);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = landing_ask(&mut queue);
    let hold = queue
        .hold(dagq::domain::NewHold {
            reason_category: dagq::domain::AskReason::Cost,
            subject: Some(dagq::domain::queue_hold::USAGE_LIMIT_SUBJECT.to_owned()),
            run_id: None,
            job: None,
            question: "the usage limit".into(),
            options: dagq::domain::HOLD_OPTIONS
                .iter()
                .map(|o| (*o).to_owned())
                .collect(),
            asked_by: "supervisor".into(),
        })
        .unwrap()
        .ask;
    queue.answer(ask.id, "retry_review").unwrap();

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        ..supervise_options(1, false)
    };
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
            .map(|outcome| (outcome, reviewer.prompts().len()))
        })
    };
    wait_until("the answer to be applied", || {
        queue.read_ask(ask.id).unwrap().closed_at.is_some()
    });
    await_passes(&passes, SOME_PASSES);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "review_retry_queued").len(), 1);
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert_eq!(payloads(&detail, "review_failed").len(), 1);

    queue.answer(hold.id, "done").unwrap();
    wait_until("the run to land", || {
        queue.show(TaskId::new(1)).unwrap().runs[0].status() == RunStatus::Integrated
    });
    stop.store(true, Ordering::SeqCst);
    let (outcome, prompts) = joined(supervisor, "the supervisor to drain").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "review_started").len(), 2);
    assert!(payloads(&detail, "resume_started").is_empty());
    assert_eq!(prompts, 3);
}
