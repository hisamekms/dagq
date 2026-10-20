//! Runtime tests: runs that wait for a person outside the slots (ADR-0062).
use crate::common;
use crate::runtime_support;
use dagq::domain::LeaseToken;

use runtime_support::*;

/// A worker that asks a `worker_question` and ends its turn; the turn of
/// the answer commits the answer it got (its first line).
const ASKING_AGENT: &str = r#"
case "$PROMPT" in
"answer to ask "*)
  printf '%s\n' "$PROMPT" | head -n 1 | tr -d '\n' > answer.txt
  git add answer.txt
  git commit -q -m answer
  receipt "$(git rev-parse HEAD)" ;;
*) "$DAGQ" ask --run "$RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question 'Which word?' --cmux /usr/bin/true > /dev/null || exit 70 ;;
esac
"#;

/// What the supervisor sent `run`'s worker, without the headless turn's
/// note after it: the first line of each text.
fn sent(run: &TaskRun) -> Vec<String> {
    session_texts(run)
        .into_iter()
        .map(|text| text.lines().next().unwrap_or_default().to_owned())
        .collect()
}

fn run_of(queue: &mut SqliteQueue, task: i64) -> Option<TaskRun> {
    queue.show(TaskId::new(task)).unwrap().runs.first().cloned()
}

fn open_ask_of(queue: &mut SqliteQueue, run: &TaskRun, kind: AskKind) -> Option<AskId> {
    queue
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.run_id.as_ref() == Some(run.id()) && ask.kind == kind && ask.is_open())
        .map(|ask| ask.id)
}

fn kinds_of(db: &Path, task: i64) -> Vec<String> {
    SqliteQueue::open(db)
        .unwrap()
        .show(TaskId::new(task))
        .unwrap()
        .events
        .into_iter()
        .map(|e| e.kind.to_string())
        .collect()
}

fn supervise_in_thread(
    db: &Path,
    repo: &Path,
    backend: &Arc<TestWorkspace>,
    options: SuperviseOptions,
) -> thread::JoinHandle<Result<Value>> {
    let (db, repo, backend) = (db.to_owned(), repo.to_owned(), backend.clone());
    thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
}

/// With one slot, a run whose worker waits for the answer of its
/// `worker_question` leaves the slot and another task is claimed and
/// finished meanwhile (acceptance 1). `status` shows the wait and the
/// slot's use. The answer ends the wait; the run goes back to its free slot
/// and the answer is delivered from there as before (acceptance 3), and
/// `stats` counts the wait (acceptance 6).
#[test]
fn a_run_waiting_for_its_answer_leaves_the_slot_to_another_task() {
    let (_dir, repo, db) = fixture();
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "second", &[]);
    let backend = TestWorkspace::new(&db, false, ASKING_AGENT);
    backend.script_for(2, VALID_AGENT);
    let backend = Arc::new(backend);
    let supervisor = supervise_in_thread(&db, &repo, &backend, supervise_options(1, true));
    // The second task runs to its rest while the first one's ask is open.
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 2).is_some_and(|run| queue.run_lease(run.id()).unwrap().is_none())
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let first = run_of(&mut queue, 1).unwrap();
    assert_eq!(first.status(), RunStatus::Running);
    let ask = open_ask_of(&mut queue, &first, AskKind::WorkerQuestion).unwrap();
    let started = events_of(&db, first.id(), "run_waiting_started");
    assert_eq!(
        started,
        vec![
            json!({"ask_id": ask, "ask_kind": "worker_question", "phase": "session",
                    "status": "running", "waiting": 1, "limit": 4})
        ]
    );
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        status["supervisors"][0]["slots"],
        json!({"used": 0, "landing_queue": 0, "parallel": 1, "source": "flag"}),
        "{status}"
    );
    assert_eq!(
        status["supervisors"][0]["waiting"],
        json!({"count": 1, "returning": 0, "limit": 4, "source": "default"})
    );
    let waiting = &status["waiting"][0];
    assert_eq!(waiting["run_id"], json!(first.id()));
    assert_eq!(waiting["state"], "waiting");
    assert_eq!(waiting["phase"], "session");
    assert_eq!(
        waiting["asks"],
        json!([{"id": ask, "kind": "worker_question"}])
    );
    // Nothing was sent to the waiting session.
    assert!(sent(&first).is_empty());

    queue.answer(ask, "blue").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let first = run_of(&mut queue, 1).unwrap();
    assert_eq!(first.status(), RunStatus::AwaitingIntegration);
    assert_eq!(sent(&first), [format!("answer to ask {ask}: blue")]);
    let ended = events_of(&db, first.id(), "run_waiting_ended");
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["cause"], "answered");
    assert_eq!(ended[0]["ask_id"], json!(ask));
    let regained = events_of(&db, first.id(), "run_slot_regained");
    assert_eq!(regained.len(), 1);
    assert_eq!(regained[0]["over_parallel"], false);
    // The wait came before the delivery.
    let kinds = kinds_of(&db, 1);
    let at = |kind: &str| kinds.iter().position(|k| k == kind).unwrap();
    assert!(at("run_slot_regained") < at("ask_delivered"), "{kinds:?}");
    // Nothing waits any more.
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["waiting"], json!([]));
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["waiting"]["started"]["worker_question"], 1, "{stats}");
    assert_eq!(stats["waiting"]["waited"]["worker_question"]["count"], 1);
    assert_eq!(stats["waiting"]["slot_wait"]["count"], 1);
    // The wait counts under the route its run was claimed on (task 1370).
    let routes = stats["waiting"]["by_route"].as_object().unwrap();
    assert_eq!(routes.len(), 1, "{stats}");
    let (route, waits) = routes.iter().next().unwrap();
    assert!(
        ["interactive", "headless"].contains(&route.as_str()),
        "{stats}"
    );
    assert_eq!(waits["started"], 1);
    assert_eq!(waits["waited"]["count"], 1);
    assert_eq!(stats["waiting"]["over_parallel"], 0);
    assert_eq!(stats["waiting"]["deferred"], 0);
}

/// A supervisor that hands off keeps the waiting run's lease, and the
/// process that continues it takes the wait over from the run's events
/// without starting it again (acceptance 5); the answer then reaches the
/// worker as before.
#[test]
fn a_wait_is_taken_over_after_a_handoff() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, ASKING_AGENT));
    let supervisor = supervise_in_thread(&db, &repo, &backend, supervise_options(4, true));
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 1)
            .is_some_and(|run| !events_of(&db, run.id(), "run_waiting_started").is_empty())
    });
    let token = SqliteQueue::open(&db).unwrap().supervisors().unwrap()[0]
        .token
        .clone();
    let queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.request_handoff(&token, "/next/dagq").unwrap());
    let outcome = joined(supervisor, "the supervisor asked to hand off").unwrap();
    assert_eq!(outcome["outcome"], "handoff", "{outcome}");
    assert_eq!(outcome["handed_over"], 1);

    let next = supervise_in_thread(
        &db,
        &repo,
        &backend,
        SuperviseOptions {
            handoff_token: Some(token.clone()),
            ..supervise_options(4, true)
        },
    );
    wait_until(&db, Duration::from_secs(30), |queue| {
        queue
            .supervisors()
            .unwrap()
            .first()
            .is_some_and(|r| r.handoff_binary.is_none())
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = run_of(&mut queue, 1).unwrap();
    // Still waiting, under the same wait.
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["waiting"][0]["state"], "waiting", "{status}");
    let ask = open_ask_of(&mut queue, &run, AskKind::WorkerQuestion).unwrap();
    queue.answer(ask, "blue").unwrap();
    let outcome = joined(next, "the supervisor after the handoff").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(
        run_of(&mut queue, 1).unwrap().status(),
        RunStatus::AwaitingIntegration
    );
    assert_eq!(events_of(&db, run.id(), "run_waiting_started").len(), 1);
    assert_eq!(events_of(&db, run.id(), "run_waiting_ended").len(), 1);
    assert_eq!(events_of(&db, run.id(), "run_slot_regained").len(), 1);
    assert_eq!(sent(&run), [format!("answer to ask {ask}: blue")]);
}

/// A supervisor whose owner (a test's process) is gone waits for no
/// person's answer, even in a drain asked for before: a drain without its
/// owner gone waits for the run's `worker_question` to be answered, and
/// once the owner is SIGKILLed the loop ends with the reason `owner_gone`
/// and no registration left. The run keeps its status, its open ask and
/// its lease, which goes stale with the supervisor for another one to
/// adopt (a run without a lease is never adopted).
#[test]
fn a_supervisor_whose_owner_is_gone_lets_a_waiting_run_go_and_stops() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, ASKING_AGENT));
    let mut owner = common::service::StandInOwner::start();
    let options = SuperviseOptions {
        owner: Some(owner.pid()),
        ..supervise_options(1, false)
    };
    let (stop, passes) = (options.stop.clone(), options.passes.clone());
    let supervisor = supervise_in_thread(&db, &repo, &backend, options);
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 1)
            .is_some_and(|run| !events_of(&db, run.id(), "run_waiting_started").is_empty())
    });
    let token = SqliteQueue::open(&db).unwrap().supervisors().unwrap()[0]
        .token
        .clone();
    // A drain waits for the answer while the owner lives.
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    wait_until(&db, Duration::from_secs(60), |_| {
        !common::service::events(&db, "supervisor_draining").is_empty()
    });
    let drained_from = passes.load(std::sync::atomic::Ordering::SeqCst);
    wait_until(&db, Duration::from_secs(60), |_| {
        passes.load(std::sync::atomic::Ordering::SeqCst) >= drained_from + 5
    });
    assert!(!supervisor.is_finished(), "the drain did not wait");

    owner.kill();
    let outcome = joined(supervisor, "the supervisor whose owner is gone");
    let outcome = outcome.unwrap();
    assert_eq!(outcome["outcome"], "owner_gone", "{outcome}");

    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = run_of(&mut queue, 1).unwrap();
    assert_eq!(run.status(), RunStatus::Running);
    assert!(open_ask_of(&mut queue, &run, AskKind::WorkerQuestion).is_some());
    let lease = queue
        .run_lease(run.id())
        .unwrap()
        .expect("the lease is kept");
    assert_eq!(lease.token, token);
    assert!(queue.supervisors().unwrap().is_empty());
    let draining = common::service::events(&db, "supervisor_draining");
    assert_eq!(draining.len(), 1, "{draining:?}");
    assert_eq!(draining[0]["payload"]["reason"], "stop_requested");
    assert_eq!(draining[0]["payload"]["runs"], json!([run.id()]));
    let stopped = common::service::events(&db, "supervisor_stopped");
    assert_eq!(stopped[0]["payload"]["reason"], "owner_gone", "{stopped:?}");
    // Nothing was sent to the waiting session: its answer is left to the
    // supervisor that adopts it.
    assert!(sent(&run).is_empty());
}

/// Point the run's wrapper row at `pid` with a heartbeat long expired, as
/// if the wrapper stopped heartbeating: the real wrapper's heartbeats no
/// longer match the row. A heartbeat already past its pid check still
/// writes `heartbeat_at` (the check and the write are two statements), so
/// the heartbeat is expired only once such a write has landed.
fn stop_wrapper_heartbeat(db: &Path, run: &TaskRun, pid: u32) {
    let raw = Connection::open(db).unwrap();
    raw.execute(
        "UPDATE run_processes SET pid=?2 WHERE run_id=?1 AND role='wrapper'",
        rusqlite::params![run.id(), pid],
    )
    .unwrap();
    // How long a heartbeat in flight takes to write: fixed, whatever the
    // test tick.
    thread::sleep(Duration::from_millis(200));
    raw.execute(
        "UPDATE run_processes SET heartbeat_at=0 WHERE run_id=?1 AND role='wrapper'",
        [run.id()],
    )
    .unwrap();
}

/// A worker's first session whose wrapper goes silent during a wait (its
/// heartbeat expired while its process lives) is detected as before
/// (acceptance 4): `wrapper_heartbeat_expired` is recorded, the wait ends
/// `wrapper_silent` and the run goes back to its slot at once, where the
/// supervisor sends the session its `/exit`. When the wrapper then dies,
/// the run is given up as before.
#[test]
fn a_wrapper_that_goes_silent_during_a_wait_sends_the_run_back_for_its_exit() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        r#"
"$DAGQ" ask --run "$RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question 'Which word?' --cmux /usr/bin/true > /dev/null || exit 70
idle
await_exit
"#,
    ));
    let supervisor = supervise_in_thread(&db, &repo, &backend, supervise_options(1, true));
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 1)
            .is_some_and(|run| !events_of(&db, run.id(), "run_waiting_started").is_empty())
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = run_of(&mut queue, 1).unwrap();
    let mut silent = sleeper();
    stop_wrapper_heartbeat(&db, &run, silent.id());
    wait_until(&db, Duration::from_secs(30), |_| {
        !events_of(&db, run.id(), "exit_requested").is_empty()
    });
    let ended = events_of(&db, run.id(), "run_waiting_ended");
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["cause"], "wrapper_silent");
    let regained = events_of(&db, run.id(), "run_slot_regained");
    assert_eq!(regained.len(), 1);
    assert_eq!(regained[0]["slot_wait_secs"], 0);
    let expired = events_of(&db, run.id(), "wrapper_heartbeat_expired");
    assert_eq!(expired.len(), 1, "{expired:?}");
    assert_eq!(expired[0]["pid"], silent.id());
    let kinds = kinds_of(&db, 1);
    let at = |kind: &str| kinds.iter().position(|k| k == kind).unwrap();
    assert!(at("run_slot_regained") < at("exit_requested"), "{kinds:?}");
    // The wrapper dies without recording its exit: the run is given up.
    silent.kill().unwrap();
    silent.wait().unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    assert_eq!(outcome["errors"][0]["run_id"], json!(run.id()), "{outcome}");
    assert!(
        outcome["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("wrapper heartbeat expired"),
        "{outcome}"
    );
}

/// Bring the run's wrapper row back to its own pid with a fresh heartbeat
/// once the wait ends, as if the silent wrapper beat again before the
/// run's slot looked at it.
fn restore_heartbeat_when_the_wait_ends(db: &Path, run: &TaskRun, pid: i64) {
    Connection::open(db)
        .unwrap()
        .execute_batch(&format!(
            "CREATE TRIGGER heartbeat_back AFTER INSERT ON run_events
             WHEN NEW.run_id = '{run}' AND NEW.kind = 'run_waiting_ended'
             BEGIN
               UPDATE run_processes SET pid = {pid},
                 heartbeat_at = CAST(strftime('%s','now') AS INTEGER) + 3600
               WHERE run_id = '{run}' AND role = 'wrapper';
             END;",
            run = run.id()
        ))
        .unwrap();
}

/// A wrapper whose heartbeat stops during a wait and comes back before the
/// run's slot looks at it leaves no silence behind (task 606; moved to a
/// headless run by task 1437): the wait ends `wrapper_silent`, the run goes
/// on without an exit request, the answer's turn asks again and that
/// `worker_question` waits outside the slot again, and a second silence is
/// recorded as `wrapper_heartbeat_expired` again before the exit request.
#[test]
fn a_wrapper_heartbeat_that_comes_back_lets_the_run_wait_again() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        r#"
case "$PROMPT" in
"answer to ask "*) "$DAGQ" ask --run "$RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question 'Which colour?' --cmux /usr/bin/true > /dev/null || exit 70 ;;
*) "$DAGQ" ask --run "$RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question 'Which word?' --cmux /usr/bin/true > /dev/null || exit 70 ;;
esac
"#,
    ));
    let supervisor = supervise_in_thread(&db, &repo, &backend, supervise_options(1, true));
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 1)
            .is_some_and(|run| !events_of(&db, run.id(), "run_waiting_started").is_empty())
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = run_of(&mut queue, 1).unwrap();
    let first = open_ask_of(&mut queue, &run, AskKind::WorkerQuestion).unwrap();
    let wrapper: i64 = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT pid FROM run_processes WHERE run_id=?1 AND role='wrapper'",
            [run.id()],
            |row| row.get(0),
        )
        .unwrap();
    let mut silent = sleeper();
    restore_heartbeat_when_the_wait_ends(&db, &run, wrapper);
    stop_wrapper_heartbeat(&db, &run, silent.id());
    wait_until(&db, Duration::from_secs(30), |_| {
        !events_of(&db, run.id(), "run_slot_regained").is_empty()
    });
    let ended = events_of(&db, run.id(), "run_waiting_ended");
    assert_eq!(ended[0]["cause"], "wrapper_silent");
    assert_eq!(
        events_of(&db, run.id(), "wrapper_heartbeat_expired").len(),
        1
    );

    // The run goes on: its answer is the next turn, and that turn's
    // question waits outside the slot again.
    queue.answer(first, "blue").unwrap();
    wait_until(&db, Duration::from_secs(60), |_| {
        events_of(&db, run.id(), "run_waiting_started").len() == 2
    });
    let second = open_ask_of(&mut queue, &run, AskKind::WorkerQuestion).unwrap();
    assert_ne!(second, first);
    assert_eq!(
        events_of(&db, run.id(), "run_waiting_started")[1]["ask_id"],
        json!(second)
    );
    assert!(events_of(&db, run.id(), "exit_requested").is_empty());

    // A second silence is recorded again, and this one gets the exit
    // request.
    Connection::open(&db)
        .unwrap()
        .execute_batch("DROP TRIGGER heartbeat_back")
        .unwrap();
    stop_wrapper_heartbeat(&db, &run, silent.id());
    wait_until(&db, Duration::from_secs(30), |_| {
        !events_of(&db, run.id(), "exit_requested").is_empty()
    });
    let expired = events_of(&db, run.id(), "wrapper_heartbeat_expired");
    assert_eq!(expired.len(), 2, "{expired:?}");
    let ended = events_of(&db, run.id(), "run_waiting_ended");
    assert_eq!(ended.len(), 2);
    assert_eq!(ended[1]["cause"], "wrapper_silent");
    // The wrapper dies without recording its exit: the run is given up.
    silent.kill().unwrap();
    silent.wait().unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    assert_eq!(outcome["errors"][0]["run_id"], json!(run.id()), "{outcome}");
}

/// A landing run holds its supervisor's slot as the supervisor counts it
/// (ADR-t610-1): `status`'s `slots.used` counts it and `stats`'s
/// `idle_slots` has one free slot fewer, and so does the run back in review
/// under that lease. A landing under a token no supervisor
/// registered (a person's `integrate`) holds none.
#[test]
fn a_landing_run_fills_its_supervisors_slot_in_status_and_stats() {
    let idle_slots = |db: &Path| {
        runtime::stats(db, &Default::default()).unwrap()["alerts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|alert| alert["kind"] == "idle_slots")
            .map(|alert| alert["value"].clone())
    };
    for (token, used, free) in [("live", 1, 1), ("by-hand", 0, 2)] {
        let (_dir, repo, db, run) = awaiting_run();
        let mut queue = SqliteQueue::open(&db).unwrap();
        queue
            .register_supervisor(&LeaseToken::new("live"), std::process::id(), 2, "0.0.1")
            .unwrap();
        let main = git_out(&repo, &["rev-parse", "main"]);
        let landing = queue
            .begin_integration(run.id(), &LeaseToken::new(token), &sha(&main))
            .unwrap();
        assert_eq!(landing.status(), RunStatus::Integrating);
        let status = runtime::status(&db).unwrap();
        assert_eq!(
            status["supervisors"][0]["slots"],
            json!({"used": used, "landing_queue": 0, "parallel": 2, "source": null}),
            "{token}: {status}"
        );
        // The dependent task is ready but blocked by the landing one.
        assert_eq!(idle_slots(&db), Some(json!(free)), "{token}");
        // Back in review under the same lease, as the supervisor holds a run
        // it reviews or that waits its turn to land, the run still holds the
        // slot: it is no longer unfinished, but leased.
        let back = queue
            .abort_integration(
                run.id(),
                &LeaseToken::new(token),
                "awaiting_integration",
                "back to review",
                &dagq::domain::Reason::new(ReasonCode::Other),
            )
            .unwrap();
        assert_eq!(back.status(), RunStatus::AwaitingIntegration);
        Connection::open(&db)
            .unwrap()
            .execute(
                "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,?2,?3,unixepoch())",
                rusqlite::params![run.id().to_string(), token, std::process::id()],
            )
            .unwrap();
        let status = runtime::status(&db).unwrap();
        assert_eq!(
            status["supervisors"][0]["slots"],
            json!({"used": used, "landing_queue": 0, "parallel": 2, "source": null}),
            "{token}: {status}"
        );
        assert_eq!(idle_slots(&db), Some(json!(free)), "{token}");
    }
}
