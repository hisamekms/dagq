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
/// is claimed.
#[test]
fn a_land_answer_is_applied_while_the_slot_is_taken_and_lands_before_a_new_claim() {
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
        &format!(
            "while [ ! -f '{}' ]; do sleep 0.05; done; {}",
            marker.display(),
            own_change(2)
        ),
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
            queue.answer(ask.id, "land").unwrap();
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
            );
            fs::write(&marker, "").unwrap();
            observed
        })
    };
    let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 1);
    let (second, first, queued, leased, status) = answerer.join().unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(second, RunStatus::Running);
    assert_eq!(first, RunStatus::AwaitingIntegration);
    assert_eq!(queued, [json!({"via": "approve", "ask_id": ask.id})]);
    assert!(!leased);
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
    use std::os::unix::fs::PermissionsExt;
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
    fs::write(&tool, "#!/bin/sh\nexec \"$@\"\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
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

/// A `cancel` answer given while the only slot runs another task is
/// applied on the next pass, as a `send_back` is: the run fails and its
/// task is canceled while the other session still holds the slot.
#[test]
fn a_cancel_answer_is_applied_while_the_slot_is_taken() {
    let (fixture, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.script_for(1, &own_change(1));
    let reviewer = TestReviewer::new(&[
        verdict("concern", &["scope"], "a concern"),
        verdict("pass", &[], "fine"),
    ]);
    let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 1);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue.asks(Default::default()).unwrap()[0].clone();
    let marker = fixture.dir.path().join("go");
    add_ready_task(&mut queue, "second", &[]);
    backend.script_for(
        2,
        &format!(
            "while [ ! -f '{}' ]; do sleep 0.05; done; {}",
            marker.display(),
            own_change(2)
        ),
    );
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
            queue.answer(ask.id, "cancel").unwrap();
            wait_until("the answer to be applied", || {
                queue.read_ask(ask.id).unwrap().closed_at.is_some()
            });
            let observed = (
                queue.show(TaskId::new(2)).unwrap().runs[0].status(),
                queue.show(TaskId::new(1)).unwrap(),
            );
            fs::write(&marker, "").unwrap();
            observed
        })
    };
    let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 1);
    let (second, first) = answerer.join().unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(second, RunStatus::Running);
    assert_eq!(first.runs[0].status(), RunStatus::Failed);
    assert_eq!(first.task.status(), TaskStatus::Canceled);
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.show(TaskId::new(2)).unwrap().runs[0].status(),
        RunStatus::Integrated
    );
}

/// A `land` answer given while another run is `integrating` (its
/// verification waits for the test) is applied on the next pass with slots
/// free: the ask closes and the run is queued while the other lands, and it
/// starts its own landing only after that one ended.
#[test]
fn a_land_answer_is_applied_while_another_run_integrates() {
    let (fixture, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.script_for(1, &own_change(1));
    let reviewer = TestReviewer::new(&[
        verdict("concern", &["scope"], "a concern"),
        verdict("pass", &[], "fine"),
    ]);
    let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 4);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue.asks(Default::default()).unwrap()[0].clone();

    // Task 2's landing holds the integration slot until the test lets its
    // verification finish.
    let marker = fixture.dir.path().join("verified");
    let second = queue
        .add(NewTask {
            title: "second".into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec![format!(
                "while [ ! -f '{}' ]; do sleep 0.05; done",
                marker.display()
            )],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: None,
        })
        .unwrap()
        .id();
    queue.transition(second, TaskAction::BypassReview).unwrap();
    backend.script_for(second.as_i64(), &own_change(2));
    let answerer = {
        let db = db.clone();
        thread::spawn(move || {
            let mut queue = SqliteQueue::open(&db).unwrap();
            wait_until("task 2 to integrate", || {
                queue
                    .show(second)
                    .unwrap()
                    .runs
                    .first()
                    .is_some_and(|run| run.status() == RunStatus::Integrating)
            });
            queue.answer(ask.id, "land").unwrap();
            wait_until("the answer to be applied", || {
                queue.read_ask(ask.id).unwrap().closed_at.is_some()
            });
            // Applied while task 2 still integrates; task 1 waits queued.
            let integrating = queue.show(second).unwrap().runs[0].status();
            let first = queue.show(TaskId::new(1)).unwrap();
            let observed = (
                integrating,
                first.runs[0].status(),
                payloads(&first, "landing_queued")
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>(),
                payloads(&first, "integration_started").len(),
            );
            fs::write(&marker, "").unwrap();
            observed
        })
    };
    let outcome = supervise_parallel(&db, &repo, &backend, &reviewer, 4);
    let (integrating, first, queued, started) = answerer.join().unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(integrating, RunStatus::Integrating);
    assert_eq!(first, RunStatus::AwaitingIntegration);
    assert_eq!(queued, [json!({"via": "approve", "ask_id": ask.id})]);
    assert_eq!(started, 0);

    let mut queue = SqliteQueue::open(&db).unwrap();
    for task in [1, second.as_i64()] {
        let detail = queue.show(TaskId::new(task)).unwrap();
        assert_eq!(detail.runs[0].status(), RunStatus::Integrated, "{task}");
        assert_eq!(payloads(&detail, "integration_started").len(), 1, "{task}");
    }
    assert!(
        first_event(&mut queue, second.as_i64(), "run_integrated")
            < first_event(&mut queue, 1, "integration_started")
    );
    assert_eq!(reviewer.prompts().len(), 2);
}
