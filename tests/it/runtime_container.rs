//! Runtime tests: the worker and the verification of `integrate` set to the
//! container by `dagq.toml` (goal 38). No part of the container runtime is
//! built, so what is set to it waits and never runs on the host instead.
use crate::runtime_support;

use runtime_support::headless::resume_launches;
use runtime_support::*;

fn set_runtimes(repo: &Path, text: &str) {
    fs::write(repo.join("dagq.toml"), text).unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-m", "runtimes"]);
}

fn queue_kinds(db: &Path) -> Vec<String> {
    Connection::open(db)
        .unwrap()
        .prepare("SELECT kind FROM run_events WHERE kind LIKE 'container_runtime_%' ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn container_attention(status: &Value) -> Option<Value> {
    status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "container_runtime_unavailable")
        .cloned()
}

#[test]
fn a_worker_set_to_an_unusable_container_is_not_claimed_and_never_runs_on_the_host() {
    let (_fixture, repo, db) = fixture();
    set_runtimes(&repo, "[roles.worker]\nruntime = \"container\"\n");
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    for _ in 0..2 {
        let outcome = supervise(&db, &repo, &backend).unwrap();
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        assert_eq!(outcome["runs"], json!([]), "{outcome}");
    }
    // No agent started, on the host or anywhere.
    assert!(backend.launched.lock().unwrap().is_empty());
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert!(detail.runs.is_empty());
    // The claim's deferral says why, once.
    let deferred = payloads(&detail, "claim_deferred");
    assert_eq!(deferred.len(), 1, "{deferred:?}");
    assert_eq!(deferred[0]["reason"], "container_unavailable");
    assert!(
        deferred[0]["message"]
            .as_str()
            .unwrap()
            .contains("never on the host"),
        "{}",
        deferred[0]
    );
    // The inbox is told once, with why and what to do.
    assert_eq!(queue_kinds(&db), ["container_runtime_unavailable"]);
    let status = runtime::status(&db).unwrap();
    let attention = container_attention(&status).unwrap_or_else(|| panic!("{status}"));
    assert_eq!(attention["next"], "fix dagq.toml");
    assert!(
        attention["last_error"].as_str().unwrap().contains("worker"),
        "{attention}"
    );
    // `status` and `doctor` show where each target runs and how far that
    // holds it.
    let runtimes = json!([
        {"target": "worker", "runtime": "container", "enforcement": "sandbox", "sandboxed": true},
        {"target": "verification", "runtime": "host", "enforcement": "advisory", "sandboxed": false},
    ]);
    assert_eq!(status["runtimes"], runtimes, "{status}");
    let worker = |report: &Value| {
        report["actors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|actor| actor["role"] == "worker")
            .cloned()
            .unwrap()
    };
    assert_eq!(worker(&status)["backend"], "container");
    assert_eq!(worker(&status)["enforcement"], "sandbox");
    for full in [false, true] {
        let doctor = runtime::doctor(&db, full).unwrap();
        assert_eq!(doctor["runtimes"], runtimes, "{doctor}");
        assert_eq!(worker(&doctor)["backend"], "container", "{doctor}");
    }

    // The worker back on the host and the verification on the container:
    // the task runs, and the inbox is told again that the verification
    // waits now.
    set_runtimes(
        &repo,
        "[roles.worker]\nruntime = \"host\"\n[roles.verification]\nruntime = \"container\"\n",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(
        queue_kinds(&db),
        [
            "container_runtime_unavailable",
            "container_runtime_unavailable"
        ]
    );
    let status = runtime::status(&db).unwrap();
    let attention = container_attention(&status).unwrap_or_else(|| panic!("{status}"));
    let told = attention["last_error"].as_str().unwrap();
    assert!(
        told.contains("verification (") && !told.contains("worker ("),
        "{attention}"
    );
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert_eq!(
        payloads(&detail, "claim_deferral_ended")[0]["why"],
        "cleared"
    );
    assert_eq!(status["runtimes"][0]["runtime"], "host");
    assert_eq!(status["runtimes"][1]["runtime"], "container");

    // Both on the host: the attention ends.
    set_runtimes(&repo, "[roles.verification]\nruntime = \"host\"\n");
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(
        queue_kinds(&db).last().map(String::as_str),
        Some("container_runtime_available")
    );
    assert_eq!(container_attention(&runtime::status(&db).unwrap()), None);
}

#[test]
fn a_worker_set_to_an_unusable_container_starts_no_resume() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let _ = parked_conflict(&repo, &db, &backend);
    // Written after the claims, in the main checkout's working file.
    fs::write(
        repo.join("dagq.toml"),
        "[roles.worker]\nruntime = \"container\"\n",
    )
    .unwrap();
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(resume_launches(&backend).is_empty());
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    assert!(payloads(&detail, "resume_started").is_empty());
    assert_eq!(queue_kinds(&db), ["container_runtime_unavailable"]);
}

#[test]
fn a_verification_set_to_an_unusable_container_runs_no_command_on_the_host() {
    let (_fixture, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    // The run is made on the host, before any setting.
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    // No dagq.toml in the main checkout: every target is on the host.
    assert!(!repo.join("dagq.toml").exists());
    assert!(
        runtime::status(&db).unwrap()["runtimes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["runtime"] == "host"),
    );
    set_runtimes(&repo, "[roles.verification]\nruntime = 'container'\n");
    assert!(queue_kinds(&db).is_empty());

    // `integrate`, with no supervisor to have looked, refuses before its
    // verification commands: none runs, the run goes back where it was, and
    // the inbox is told.
    let error = format!("{:#}", integrate(&db, 1, &repo).unwrap_err());
    assert!(
        error.contains("verification's runtime to the container")
            && error.contains("returned to awaiting_integration"),
        "{error}"
    );
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    assert!(!run_dir.join("integrate-1-verify-1.log").exists());
    assert_eq!(
        SqliteQueue::open(&db)
            .unwrap()
            .run(run.id())
            .unwrap()
            .status(),
        RunStatus::AwaitingIntegration
    );
    assert_eq!(queue_kinds(&db), ["container_runtime_unavailable"]);
    let status = runtime::status(&db).unwrap();
    let attention = container_attention(&status).unwrap_or_else(|| panic!("{status}"));
    assert_eq!(attention["next"], "fix dagq.toml");
    assert!(
        attention["last_error"]
            .as_str()
            .unwrap()
            .contains("verification"),
        "{attention}"
    );
    assert_eq!(status["runtimes"][1]["runtime"], "container", "{status}");
    assert_eq!(status["runtimes"][1]["enforcement"], "sandbox", "{status}");

    // The supervisor holds its landing too, and tells nothing new.
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(
        SqliteQueue::open(&db)
            .unwrap()
            .run(run.id())
            .unwrap()
            .status(),
        RunStatus::AwaitingIntegration
    );
    assert!(!run_dir.join("integrate-1-verify-1.log").exists());
    assert_eq!(queue_kinds(&db), ["container_runtime_unavailable"]);
}

/// A run that started on the host waits for an answer; `dagq.toml` is
/// then written as `settings` and the answer given: the wrapper refuses to
/// start the answer's turn with an error that says `refused`, and only
/// the first turn ever ran.
fn host_run_takes_no_turn_after(settings: &str, refused: &str) {
    use runtime_support::headless::{TASK, headless_fixture};
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        r#"case "$TURN" in
1) ask "which file"; say asked ;;
*) say answered ;;
esac"#,
    );
    let backend = Arc::new(backend);
    let reviewer = Arc::new(TestReviewer::new(&[verdict("pass", &[], "fine")]));
    let options = supervise_options(4, false);
    let stop = options.stop.clone();
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
        })
    };
    wait_until(&db, crate::common::STEP_LIMIT, |queue| {
        queue
            .show(TASK)
            .unwrap()
            .events
            .iter()
            .any(|e| e.kind == "run_waiting_started")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TASK).unwrap().runs[0].clone();
    let ask = queue
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.kind == AskKind::WorkerQuestion)
        .unwrap();
    // The main checkout's working file, as the supervisor and the wrapper
    // read it.
    fs::write(repo.join("dagq.toml"), settings).unwrap();
    queue.answer(ask.id, "change.txt").unwrap();
    drop(queue);
    // The answer goes to the session as its next request; the wrapper
    // refuses to start the turn.
    wait_until(&db, crate::common::STEP_LIMIT, |queue| {
        queue
            .show(TASK)
            .unwrap()
            .events
            .iter()
            .any(|e| e.kind == "runtime_error" && e.payload.to_string().contains(refused))
    });
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = joined(supervisor, "the supervisor thread to return");
    // The wrapper ended on that refusal (its error is the one awaited
    // above), so it is not joined as one that must succeed.
    wait_until(&db, crate::common::STEP_LIMIT, |queue| {
        queue
            .show(TASK)
            .unwrap()
            .events
            .iter()
            .any(|e| e.kind == "session_exited")
    });
    let detail = SqliteQueue::open(&db).unwrap().show(TASK).unwrap();
    assert!(!payloads(&detail, "turn_requested").is_empty());
    // Only the first turn ever ran.
    let calls = stub_calls(&run);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(
        payloads(&detail, "turn_started")
            .iter()
            .all(|turn| turn["turn"] != 2),
        "{:?}",
        payloads(&detail, "turn_started")
    );
}

/// A run that started on the host and waits for an answer when `dagq.toml`
/// sets the worker to the container: the answer's turn is refused where
/// the wrapper starts it, and no agent runs it on the host.
#[test]
fn a_host_run_switched_to_the_container_takes_no_next_turn_on_the_host() {
    host_run_takes_no_turn_after(
        "[roles.worker]\nruntime = \"container\"\n",
        "does not start an actor set to the container",
    );
}

/// The same with a `dagq.toml` that cannot be read (here, one that sets the
/// worker to the container and has another mistake): the turn is not
/// started on the host for want of the setting either.
#[test]
fn a_host_run_whose_dagq_toml_cannot_be_read_takes_no_next_turn_on_the_host() {
    host_run_takes_no_turn_after(
        "[roles.worker]\nruntime = \"container\"\n[build]\n",
        "runtimes of dagq.toml could not be read",
    );
}
