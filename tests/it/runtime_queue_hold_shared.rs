//! Runtime tests: the answer of an authentication `queue_hold` ask with
//! two supervisors on one queue (task 754). Each supervisor applies the
//! answer to the held runs in its own slots, once each
//! (`hold_answer_applied`), and the ask closes once every run in it was
//! applied: `cancel_affected` gives up the run the other supervisor
//! watches too, and `done` types the text to go on into its session.
use crate::runtime_support;

use dagq::domain::queue_hold::CONTINUE_TEXT;
use runtime_support::*;
use std::sync::atomic::AtomicBool;

fn queue_events(db: &Path, kind: &str) -> Vec<Value> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == kind)
        .map(|event| event.payload)
        .collect()
}

/// A supervisor with one slot that stops when `stop` is set.
fn one_slot(stop: &Arc<AtomicBool>) -> SuperviseOptions {
    SuperviseOptions {
        stop: stop.clone(),
        stall: Some(
            dagq::domain::stall::StallConfig::default()
                .with_millis("idle_without_receipt_secs", 200),
        ),
        ..supervise_options(1, false)
    }
}

/// Two supervisors with one slot each, a task each, and both sessions at
/// a login that ran out: the two runs join one authentication ask.
struct TwoHeld {
    _dir: Fixture,
    db: PathBuf,
    backend: Arc<TestWorkspace>,
    stop: Arc<AtomicBool>,
    supervisors: Vec<thread::JoinHandle<Result<Value>>>,
    runs: Vec<TaskRun>,
    ask: dagq::domain::Ask,
}

fn two_held(script: &str) -> TwoHeld {
    let (dir, repo, db) = fixture();
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "second task", &[]);
    // The sessions work (no idle, a working screen) until both runs are
    // claimed: a login seen before would hold the second claim.
    let gate = db.with_extension("gate");
    let script = format!(
        "while [ ! -f \"{}\" ]; do sleep 0.05; done\n{script}",
        gate.display()
    );
    let backend = TestWorkspace::new(&db, false, &script);
    *backend.screen.lock().unwrap() = WORK_SCREEN.into();
    let backend = Arc::new(backend);
    let stop = Arc::new(AtomicBool::new(false));
    let start = || {
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), one_slot(&stop));
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    // The second starts once the first run has its worktree: two `git
    // worktree add` at once in one repository can collide.
    let first = start();
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"worktree_created")
    });
    let supervisors = vec![first, start()];
    wait_until(&db, Duration::from_secs(30), |queue| {
        [1, 2].iter().all(|task| {
            event_kinds(&queue.show(TaskId::new(*task)).unwrap()).contains(&"agent_started")
        })
    });
    *backend.screen.lock().unwrap() = LOGIN_SCREEN.into();
    fs::write(&gate, "").unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        [1, 2].iter().all(|task| {
            event_kinds(&queue.show(TaskId::new(*task)).unwrap()).contains(&"auth_required")
        })
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let runs: Vec<TaskRun> = [1, 2]
        .iter()
        .map(|task| queue.show(TaskId::new(*task)).unwrap().runs[0].clone())
        .collect();
    let ask = queue.hold_of(runs[0].id()).unwrap().unwrap();
    let mut affected = ask.affected.clone();
    affected.sort();
    let mut ids: Vec<String> = runs.iter().map(|run| run.id().to_string()).collect();
    ids.sort();
    assert_eq!(affected, ids, "{ask:?}");
    // Each supervisor watches one of them.
    assert_ne!(
        supervisor_token_of(&db, &runs[0]),
        supervisor_token_of(&db, &runs[1])
    );
    TwoHeld {
        _dir: dir,
        db,
        backend,
        stop,
        supervisors,
        runs,
        ask,
    }
}

/// The `hold_answer_applied` of each run, in the order of `runs`.
fn applied_to_each(db: &Path, runs: &[TaskRun]) -> Vec<Value> {
    let mut queue = SqliteQueue::open(db).unwrap();
    runs.iter()
        .map(|run| {
            let detail = queue.show(run.task_id()).unwrap();
            let applied = payloads(&detail, "hold_answer_applied");
            assert_eq!(applied.len(), 1, "{applied:?}");
            applied[0].clone()
        })
        .collect()
}

fn sorted(runs: &[TaskRun]) -> Value {
    let mut ids: Vec<String> = runs.iter().map(|run| run.id().to_string()).collect();
    ids.sort();
    json!(ids)
}

fn sorted_value(list: &Value) -> Value {
    let mut ids: Vec<String> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    json!(ids)
}

/// `cancel_affected`: whichever supervisor reads the answer first, both
/// runs are given up as an abandon does, each by the supervisor that
/// watches it, and the ask closes with one `queue_hold_applied` that lists
/// both as released, and which supervisor released each.
#[test]
fn cancel_affected_gives_up_the_held_runs_of_every_supervisor() {
    let held = two_held(
        r#"
commit work; idle
while [ ! -f "$EXIT.go" ]; do sleep 0.05; done
"#,
    );
    let (db, runs) = (&held.db, &held.runs);
    SqliteQueue::open(db)
        .unwrap()
        .answer(held.ask.id, "cancel_affected")
        .unwrap();
    wait_until(db, Duration::from_secs(30), |queue| {
        runs.iter()
            .all(|run| queue.run_lease(run.id()).unwrap().is_none())
            && queue.read_ask(held.ask.id).unwrap().closed_at.is_some()
    });
    held.stop.store(true, Ordering::SeqCst);
    for supervisor in held.supervisors {
        joined(supervisor, "a supervisor to stop").unwrap();
    }
    for run in runs {
        fs::write(
            exit_request_path(run.run_dir().unwrap()).with_extension("go"),
            "",
        )
        .unwrap();
    }
    held.backend.join();
    let mut queue = SqliteQueue::open(db).unwrap();
    for run in runs {
        let detail = queue.show(run.task_id()).unwrap();
        let errors = payloads(&detail, "runtime_error");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0]["code"], "hold_canceled");
        assert_eq!(errors[0]["lease_released"], true);
    }
    let applied = applied_to_each(db, runs);
    for (payload, run) in applied.iter().zip(runs) {
        assert_eq!(payload["outcome"], "released", "{payload}");
        assert_eq!(payload["answer"], "cancel_affected", "{payload}");
        assert_eq!(payload["supervisor"], supervisor_token_of(db, run));
    }
    assert_ne!(applied[0]["supervisor"], applied[1]["supervisor"]);
    let events = queue_events(db, "queue_hold_applied");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["answer"], "cancel_affected");
    assert_eq!(sorted_value(&events[0]["released"]), sorted(runs));
    for list in ["elsewhere", "unwatched", "moved_on", "continued"] {
        assert_eq!(events[0][list], json!([]), "{list}: {events:?}");
    }
    assert_eq!(events[0]["runs"].as_array().unwrap().len(), 2, "{events:?}");
}

/// `done`: each session gets the text to go on exactly once, typed by
/// the supervisor that watches it, and the ask closes with one
/// `queue_hold_applied` that lists both as continued.
#[test]
fn done_tells_the_held_sessions_of_every_supervisor_to_go_on() {
    let held = two_held(
        r#"
commit work; idle
while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
while [ ! -f "$EXIT.go" ]; do sleep 0.05; done
"#,
    );
    let (db, runs) = (&held.db, &held.runs);
    SqliteQueue::open(db)
        .unwrap()
        .answer(held.ask.id, "done")
        .unwrap();
    wait_until(db, Duration::from_secs(30), |queue| {
        runs.iter().all(|run| {
            event_kinds(&queue.show(run.task_id()).unwrap()).contains(&"hold_continue_sent")
        }) && queue.read_ask(held.ask.id).unwrap().closed_at.is_some()
    });
    // A few more passes: nothing is applied twice.
    thread::sleep(TEST_TICK * 10);
    held.stop.store(true, Ordering::SeqCst);
    for run in runs {
        fs::write(
            exit_request_path(run.run_dir().unwrap()).with_extension("go"),
            "",
        )
        .unwrap();
    }
    for supervisor in held.supervisors {
        joined(supervisor, "a supervisor to stop").unwrap();
    }
    held.backend.join();
    let texts = held.backend.texts();
    let continues: Vec<_> = texts
        .iter()
        .filter(|(_, text)| text == CONTINUE_TEXT)
        .collect();
    assert_eq!(continues.len(), 2, "{texts:?}");
    assert_ne!(continues[0].0, continues[1].0, "{texts:?}");
    let mut queue = SqliteQueue::open(db).unwrap();
    for run in runs {
        let detail = queue.show(run.task_id()).unwrap();
        let sent = payloads(&detail, "hold_continue_sent");
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0]["ask_id"], json!(held.ask.id));
        assert!(payloads(&detail, "runtime_error").is_empty());
    }
    let applied = applied_to_each(db, runs);
    for (payload, run) in applied.iter().zip(runs) {
        assert_eq!(payload["outcome"], "continued", "{payload}");
        assert_eq!(payload["supervisor"], supervisor_token_of(db, run));
    }
    let events = queue_events(db, "queue_hold_applied");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["answer"], "done");
    assert_eq!(sorted_value(&events[0]["continued"]), sorted(runs));
    for list in ["elsewhere", "unwatched", "moved_on", "released"] {
        assert_eq!(events[0][list], json!([]), "{list}: {events:?}");
    }
}

/// A run another live supervisor leases keeps the answered ask open until
/// that supervisor applied the answer or let the run go; a run without a
/// lease has nothing to apply it to. Once no run waits, the ask closes and
/// `queue_hold_applied` lists those runs as `unwatched`.
#[test]
fn the_answered_ask_waits_for_the_runs_another_live_supervisor_leases() {
    use dagq::domain::{AskReason, ClaimOutcome, HOLD_OPTIONS, LeaseToken, NewHold};
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "second task", &[]);
    add_ready_task(&mut queue, "third task", &[]);
    let base = sha("0123456789abcdef0123456789abcdef01234567");
    let mut claim = |token: &str| {
        let ClaimOutcome::Claimed { run } = queue
            .claim_for_supervisor(&base, &LeaseToken::new(token))
            .unwrap()
        else {
            panic!("nothing to claim")
        };
        run
    };
    // `other` is alive (this process, a fresh heartbeat); `gone` let its
    // run go.
    let other = claim("other");
    let gone = claim("gone");
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_leases WHERE run_id=?1", [gone.id()])
        .unwrap();
    let backend = TestWorkspace::new(
        &db,
        false,
        r#"
commit work; idle
while [ ! -f "$EXIT.go" ]; do sleep 0.05; done
"#,
    );
    *backend.screen.lock().unwrap() = LOGIN_SCREEN.into();
    let backend = Arc::new(backend);
    let stop = Arc::new(AtomicBool::new(false));
    let supervisor = {
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), one_slot(&stop));
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(3)).unwrap()).contains(&"auth_required")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let own = queue.show(TaskId::new(3)).unwrap().runs[0].clone();
    for run in [&other, &gone] {
        queue
            .hold(NewHold {
                reason_category: AskReason::Authentication,
                subject: None,
                run_id: Some(run.id().clone()),
                job: None,
                question: "the login ran out".into(),
                options: HOLD_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
                asked_by: "supervisor".into(),
            })
            .unwrap();
    }
    let ask = queue.hold_of(own.id()).unwrap().unwrap();
    assert_eq!(ask.affected.len(), 3, "{ask:?}");
    queue.answer(ask.id, "cancel_affected").unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        queue.run_lease(own.id()).unwrap().is_none()
    });
    // `other` has not applied it: the ask stays open for it.
    thread::sleep(TEST_TICK * 10);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_none());
    assert!(queue_events(&db, "queue_hold_applied").is_empty());
    // `other` lets its run go: nothing waits any more.
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_leases WHERE run_id=?1", [other.id()])
        .unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        queue.read_ask(ask.id).unwrap().closed_at.is_some()
    });
    stop.store(true, Ordering::SeqCst);
    joined(supervisor, "the supervisor to stop").unwrap();
    fs::write(
        exit_request_path(own.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    backend.join();
    let events = queue_events(&db, "queue_hold_applied");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["released"], json!([own.id()]), "{events:?}");
    assert_eq!(
        sorted_value(&events[0]["unwatched"]),
        sorted(&[(*other).clone(), (*gone).clone()]),
        "{events:?}"
    );
    assert_eq!(events[0]["elsewhere"], json!([]), "{events:?}");
    let runs = events[0]["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 3, "{events:?}");
    assert!(
        runs.iter()
            .filter(|r| r["outcome"] == "unwatched")
            .all(|r| r["supervisor"].is_null()),
        "{events:?}"
    );
    // Only its own run was given up.
    for run in [&other, &gone] {
        let detail = queue.show(run.task_id()).unwrap();
        assert!(payloads(&detail, "hold_answer_applied").is_empty());
        assert!(payloads(&detail, "runtime_error").is_empty());
    }
}
