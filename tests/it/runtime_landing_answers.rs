//! Runtime tests: the answers of `approve_landing` asks are applied on the
//! next pass whatever the slots, and a `land` answer queues the run, which
//! lands once there is room, before new claims (task 949).
use crate::common;
use crate::runtime_support;
use dagq::domain::EventKind;

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
        .find(|e| e.kind.as_str() == kind)
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
    // Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
    #[cfg(any())]
    {
        let stats = runtime::stats(&db, &Default::default()).unwrap();
        let via = &stats["overall"]["land_phases"]["landing_queue_via"];
        assert_eq!(via[0]["via"], "approve", "{via}");
        assert_eq!(via[0]["count"], 2, "{via}");
        assert!(
            stats["asks"]["times"]["by_kind"]["approve_landing"]["answer_to_apply_without_slot_wait"]
                ["count"]
                == 2,
            "{}",
            stats["asks"]
        );
    }
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
        .find(|e| e.kind == EventKind::AskAnswered && e.payload["ask_id"] == json!(ask))
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

/// Commit on main `[review.subagents.design]`, which reviews
/// `change.txt` (the stand-in worker's change), and its definition.
fn design_reviews_the_change(repo: &Path) {
    fs::write(
        repo.join("dagq.toml"),
        "[review.subagents.design]\npaths = [\"change.txt\"]\n",
    )
    .unwrap();
    fs::create_dir_all(repo.join(".dagq/agents/design")).unwrap();
    fs::write(
        repo.join(".dagq/agents/design/AGENT.md"),
        "---\ndescription: design checks\n---\nCheck the design.\n",
    )
    .unwrap();
    git(repo, &["add", "dagq.toml", ".dagq/agents/design/AGENT.md"]);
    git(repo, &["commit", "-q", "-m", "review subagents"]);
}

/// Every whole review passes with no reason of its own; `design` sends
/// task 1's run back on its first four reviews with a finding without
/// codes (past the run's revises, a code finding that asks a person), then
/// passes, task 2's review and the last of task 1 alike.
fn design_sends_back_under_a_pass() -> TestReviewer {
    let back = agent_says("design", "revise", &["add a unit test"]);
    let mut design = vec![back; 4];
    design.push(agent_says("design", "pass", &[]));
    TestReviewer::new(&[verdict("pass", &[], "looks fine")])
        .with_review_agents(&[("design", design)])
}

/// The one open ask of the queue, an `approve_landing`.
fn open_landing_ask(queue: &mut SqliteQueue) -> dagq::domain::Ask {
    let open: Vec<dagq::domain::Ask> = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .filter(|ask| ask.closed_at.is_none())
        .collect();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].kind, dagq::domain::AskKind::ApproveLanding);
    open[0].clone()
}

/// The reason a bare `send_back` to ask `ask` parks the run with: the
/// subagent's finding the ask showed, under its name.
fn design_sent_back(ask: dagq::domain::AskId) -> String {
    format!("the review's findings were sent back by ask {ask}: design: add a unit test")
}

/// The resumed session of task 1's run: rebase onto the main the request
/// names, resolving `change.txt` when it conflicts, then rewrite the
/// receipt.
const RESOLVING_RESUME: &str =
    "await_message; resolve || exit 1; receipt \"$(git rev-parse HEAD)\"; idle; await_exit";

/// A review whose subagent sends the run back under a pass with no reason
/// of its own asks a person past the run's revises. A bare `send_back` to
/// that ask, given while the run waits, names the subagent's finding the
/// ask showed, under its name, as `landing_decided`'s reason and the
/// prompt of the resume it starts. The review after that resume asks
/// again; a bare `send_back` given while the landing recheck parks the
/// waiting run for a conflict (the supervisor stands still where the
/// recheck found it) applies once the rebased run waits again, the same
/// way. Which findings a send_back names is `supervise::landing::tests::
/// a_send_back_names_the_findings_its_ask_showed`.
#[test]
fn a_bare_send_back_of_a_subagents_send_back_carries_its_findings_with_or_without_a_recheck() {
    let (_fixture, repo, db) = fixture();
    design_reviews_the_change(&repo);
    let backend = TestWorkspace::new(&db, false, &crate::runtime_review::revising_agent(2));
    backend.resume_script_for(1, RESOLVING_RESUME);
    let reviewer = design_sends_back_under_a_pass();
    let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 1);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let reviewed = queue.show(TaskId::new(1)).unwrap();
    let finished = payloads(&reviewed, "review_finished");
    assert_eq!(finished.len(), 3, "{finished:?}");
    assert_eq!(finished[2]["verdict"], "pass");
    assert_eq!(finished[2]["reasons"], json!([]));
    assert_eq!(finished[2]["route"]["destination"], "send_back");
    let first = open_landing_ask(&mut queue);
    assert!(
        first.question.contains("\n- design: add a unit test"),
        "{}",
        first.question
    );

    // While the run waits.
    queue.answer(first.id, "send_back").unwrap();
    let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 1);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let decided = payloads(&detail, "landing_decided");
    assert_eq!(decided.len(), 1, "{decided:?}");
    let reason = design_sent_back(first.id);
    assert_eq!(decided[0]["reason"], reason);
    let texts = session_texts(&detail.runs[0]);
    assert!(texts.iter().any(|text| text.contains(&reason)), "{texts:?}");
    let second = open_landing_ask(&mut queue);
    assert_ne!(second.id, first.id);

    // While the landing recheck parks it: task 2 changes `change.txt` too
    // and lands, so the recheck finds the waiting run conflicting.
    add_ready_task(&mut queue, "second task", &[]);
    let answered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pauses = {
        let (db, answered) = (db.clone(), answered.clone());
        runtime::PausesPort(Arc::new(move |at, _run| {
            if at == runtime::PausePoint::RecheckFailure
                && !answered.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                SqliteQueue::open(&db)
                    .unwrap()
                    .answer(second.id, "send_back")
                    .unwrap();
            }
        }))
    };
    let options = SuperviseOptions {
        pauses: Some(pauses),
        ..supervise_options(4, true)
    };
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(answered.load(std::sync::atomic::Ordering::SeqCst));
    // A later pass applies it if the first supervisor ended before it.
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "landing_recheck_failed").len(), 1);
    let decided = payloads(&detail, "landing_decided");
    assert_eq!(decided.len(), 2, "{decided:?}");
    let reason = design_sent_back(second.id);
    assert_eq!(decided[1]["reason"], reason);
    let texts = session_texts(&detail.runs[0]);
    assert!(texts.iter().any(|text| text.contains(&reason)), "{texts:?}");
    assert!(queue.read_ask(second.id).unwrap().closed_at.is_some());
}

/// [`supervise_options`] of a supervisor that drains from its first pass,
/// as one asked to stop does.
fn draining_options(parallel: usize) -> SuperviseOptions {
    let options = supervise_options(parallel, true);
    options.stop.store(true, Ordering::SeqCst);
    options
}

/// Supervise with `options`, given the `dagq` the test owns
/// ([`common::service::owned_executable`]), so a service the supervisor
/// starts stops once the test's process is gone.
fn supervise_owned(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    reviewer: &TestReviewer,
    options: &SuperviseOptions,
) -> Value {
    let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
    let outcome = runtime::supervise_with_reviewer(
        db,
        repo,
        backend,
        &claude_stub(db),
        reviewer,
        &common::service::owned_executable(db.parent().unwrap()),
        options,
    )
    .unwrap();
    backend.join();
    outcome
}

/// A supervisor that drains for a stop applies the `land`, `send_back`,
/// `cancel` and `retry_review` answers of runs nobody leases on its passes
/// and closes their asks (ADR-t3330-1), but starts no new work: no claim,
/// no resume of the run sent back, no review of the run queued to be
/// reviewed again, and, with no run of its own left, no landing of the
/// approved run, which stays queued.
#[test]
fn a_draining_supervisor_applies_the_landing_answers_and_starts_no_new_work() {
    let (_fixture, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    for title in ["second", "third", "fourth"] {
        add_ready_task(&mut queue, title, &[]);
    }
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    for task in 1..=4 {
        backend.script_for(task, &own_change(task));
    }
    // One slot: the tasks are reviewed one after another, task 4's review
    // failing.
    let reviewer = TestReviewer::new(&[
        verdict("concern", &["scope"], "a concern"),
        verdict("concern", &["scope"], "a concern"),
        verdict("concern", &["scope"], "a concern"),
        NO_VERDICT.to_owned(),
        NO_VERDICT.to_owned(),
    ]);
    let outcome = supervise_owned(&db, &repo, &backend, &reviewer, &supervise_options(1, true));
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let reviews = reviewer.prompts().len();
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 4, "{asks:?}");
    let mut ask_of = |task: i64| {
        let run = queue.show(TaskId::new(task)).unwrap().runs[0].clone();
        asks.iter()
            .find(|ask| ask.run_id.as_ref() == Some(run.id()))
            .unwrap()
            .id
    };
    let answers = [
        (ask_of(1), "land"),
        (ask_of(2), "send_back"),
        (ask_of(3), "cancel"),
        (ask_of(4), "retry_review"),
    ];
    for (ask, answer) in answers {
        queue.answer(ask, answer).unwrap();
    }
    let ready = add_ready_task(&mut queue, "fifth", &[]);

    let outcome = supervise_owned(&db, &repo, &backend, &reviewer, &draining_options(4));
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    for (ask, _) in answers {
        assert!(queue.read_ask(ask).unwrap().closed_at.is_some(), "{ask}");
    }
    let landed = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(landed.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(queue.run_lease(landed.runs[0].id()).unwrap().is_none());
    assert_eq!(payloads(&landed, "integration_approved").len(), 1);
    assert_eq!(payloads(&landed, "landing_queued").len(), 1);
    assert!(payloads(&landed, "integration_started").is_empty());
    let sent_back = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(sent_back.runs[0].status(), RunStatus::NeedsSession);
    assert_eq!(payloads(&sent_back, "landing_decided").len(), 1);
    assert!(payloads(&sent_back, "resume_started").is_empty());
    let canceled = queue.show(TaskId::new(3)).unwrap();
    assert_eq!(canceled.runs[0].status(), RunStatus::Failed);
    assert_eq!(canceled.task.status(), TaskStatus::Canceled);
    let retried = queue.show(TaskId::new(4)).unwrap();
    assert_eq!(payloads(&retried, "review_retry_queued").len(), 1);
    assert_eq!(payloads(&retried, "review_started").len(), 1);
    assert!(queue.show(ready).unwrap().runs.is_empty());
    assert_eq!(reviewer.prompts().len(), reviews);
}

/// A `land` answer given while a supervisor drains for a stop, its own run
/// still at work, is applied and the approved run lands in that drain,
/// before the drain ends with its own run (ADR-t3330-1). The approved run
/// is not reviewed again.
#[test]
fn a_drain_lands_an_approved_run_while_its_own_run_keeps_it_going() {
    let (fixture, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    backend.script_for(1, &own_change(1));
    let reviewer = TestReviewer::new(&[
        verdict("concern", &["scope"], "a concern"),
        verdict("pass", &[], "fine"),
    ]);
    let outcome = supervise_owned(&db, &repo, &backend, &reviewer, &supervise_options(1, true));
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = landing_ask(&mut queue);

    let marker = fixture.dir.path().join("go");
    let own = add_ready_task(&mut queue, "its own", &[]);
    backend.script_for(
        own.as_i64(),
        &format!("await_file {}; {}", shell_path(&marker), own_change(2)),
    );
    let options = SuperviseOptions {
        stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        ..supervise_options(2, false)
    };
    let stop = options.stop.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &reviewer,
                &common::service::owned_executable(db.parent().unwrap()),
                &options,
            )
            .map(|outcome| (outcome, reviewer.prompts().len()))
        })
    };
    let running = |queue: &mut SqliteQueue| {
        queue
            .show(own)
            .unwrap()
            .runs
            .first()
            .is_some_and(|run| run.status() == RunStatus::Running)
    };
    wait_until("its own run to run", || running(&mut queue));
    stop.store(true, Ordering::SeqCst);
    // Answered once the drain is recorded: no pass at work applies it.
    wait_until("the drain to be recorded", || {
        !common::service::events(&db, "supervisor_draining").is_empty()
    });
    queue.answer(ask.id, "land").unwrap();
    wait_until("the approved run to land", || {
        queue.show(TaskId::new(1)).unwrap().runs[0].status() == RunStatus::Integrated
    });
    // Landed while the drain went on for its own run.
    assert!(running(&mut queue));
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    fs::write(&marker, "").unwrap();
    let (outcome, prompts) = joined(supervisor, "the supervisor to drain").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "integration_approved").len(), 1);
    assert_eq!(payloads(&detail, "integration_started").len(), 1);
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert_eq!(
        queue.show(own).unwrap().runs[0].status(),
        RunStatus::Integrated
    );
    assert_eq!(prompts, 2);
}

/// A drain with no run of its own applies a `land` answer and ends without
/// landing it; the next supervisor lands the approved run from its events,
/// without reviewing or asking again (ADR-t3330-1).
#[test]
fn a_drain_with_no_run_of_its_own_leaves_the_approved_run_to_the_next_supervisor() {
    let (_fixture, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.script_for(1, &own_change(1));
    let reviewer = TestReviewer::new(&[verdict("concern", &["scope"], "a concern")]);
    let outcome = supervise_owned(&db, &repo, &backend, &reviewer, &supervise_options(1, true));
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = landing_ask(&mut queue);
    queue.answer(ask.id, "land").unwrap();

    let outcome = supervise_owned(&db, &repo, &backend, &reviewer, &draining_options(4));
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert_eq!(payloads(&detail, "landing_queued").len(), 1);
    assert!(payloads(&detail, "integration_started").is_empty());

    let outcome = supervise_owned(&db, &repo, &backend, &reviewer, &supervise_options(1, true));
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert_eq!(payloads(&detail, "integration_approved").len(), 1);
    assert_eq!(payloads(&detail, "integration_started").len(), 1);
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert_eq!(payloads(&detail, "ask_opened").len(), 1);
    assert_eq!(reviewer.prompts().len(), 1);
}
