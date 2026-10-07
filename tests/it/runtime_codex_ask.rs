//! Runtime tests: a Codex worker's `dagq ask` goes to the queue service in
//! client mode (ADR-t1233-5 decision 5, amending ADR-t813-3 decision 3),
//! which opens it at once for the worker's principal.
use crate::common;
use crate::runtime_codex::{FINISH, codex_fixture, detail, finished, open_ask, supervise_thread};
use crate::runtime_support;

use dagq::domain::AskReason;
use runtime_support::*;

/// Wait, up to a step's limit, until `done`.
fn wait_for(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(
            started.elapsed() < common::STEP_LIMIT,
            "{what}: not within {:?}",
            common::STEP_LIMIT
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn worker_questions(db: &Path) -> Vec<dagq::domain::Ask> {
    SqliteQueue::open(db)
        .unwrap()
        .asks(AskQuery {
            all: true,
            ..AskQuery::default()
        })
        .unwrap()
        .into_iter()
        .filter(|ask| ask.kind == AskKind::WorkerQuestion)
        .collect()
}

/// The worker's `dagq ask` opens the worker's `worker_question` through
/// the queue service, with no queue path in the turn's environment; the
/// inbox's answer is the next turn's
/// `codex exec resume` and the run lands. The turn's idle marker may be
/// seconds older than the ask (made so here): the answer still goes out,
/// the session being between turns.
#[test]
fn a_codex_workers_ask_goes_to_the_queue_service() {
    let (dir, repo, db, backend, codex) = codex_fixture();
    set_turns(
        dir.path(),
        &format!(
            r#"case "$TURN" in
1) env > "$RUN_DIR/turn-env.txt"; "$DAGQ" ask --run "$DAGQ_RUN_ID" --kind worker_question --because scope --topic design_choice --question "which file" --option a.txt --option b.txt > "$RUN_DIR/ask-output.json" 2>&1 || fail "ask failed: $(cat "$RUN_DIR/ask-output.json")"; say asked ;;
2) case "$PROMPT" in "answer to ask "*) {FINISH} ;; *) say lost ;; esac ;;
esac"#
        ),
    );
    let backend = Arc::new(backend);
    let supervisor = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        &codex,
        &[verdict("pass", &[], "meets the acceptance")],
    );
    let ask = open_ask(&db, |ask| ask.kind == AskKind::WorkerQuestion);
    // The turn that asked has ended and written its idle marker (the
    // run's first, through a rename).
    let marker = detail(&db).runs[0].idle_marker_path().unwrap();
    wait_for("the asking turn's idle marker", || marker.is_file());
    fs::File::options()
        .write(true)
        .open(&marker)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(10))
        .unwrap();
    assert_eq!(ask.question, "which file");
    assert_eq!(ask.options, ["a.txt", "b.txt"]);
    assert_eq!(ask.asked_by, "worker");
    assert_eq!(ask.reason_category, AskReason::Scope);
    assert_eq!(ask.topics, ["design_choice"]);
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "a.txt")
        .unwrap();
    let outcome = finished(&backend, supervisor);
    assert_eq!(
        outcome["runs"].as_array().unwrap().last().unwrap()["status"],
        "integrated",
        "{outcome}"
    );
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    // On the run and told to the inbox, as an ask the command opens.
    let opened = payloads(&detail, "ask_opened");
    assert!(
        opened
            .iter()
            .any(|p| p["ask_id"] == json!(ask.id) && p["asked_by"] == "worker"),
        "{opened:?}"
    );
    // The service notified nobody and ran no cmux: the inbox's watch
    // tells of the ask (ADR-t1433-1 decision 2).
    let calls = fs::read_to_string(fake_cmux_dir(&db).join("calls")).unwrap_or_default();
    assert!(!calls.contains("notify"), "{calls}");

    // The command printed the ask the service opened, and its turn was
    // given the service, not the queue's path.
    let run_dir = Path::new(run.run_dir().unwrap());
    let output: Value =
        serde_json::from_slice(&fs::read(run_dir.join("ask-output.json")).unwrap()).unwrap();
    assert_eq!(output["id"], json!(ask.id), "{output}");
    let env = fs::read_to_string(run_dir.join("turn-env.txt")).unwrap();
    assert!(env.contains("DAGQ_SERVICE_SOCKET="), "{env}");
    assert!(env.contains("DAGQ_SERVICE_CREDENTIAL_FILE="), "{env}");
    for line in env.lines().filter(|line| !line.starts_with("DB=")) {
        assert!(
            !line.contains("queue's data.db") && !line.starts_with("DAGQ_QUEUE="),
            "{line}"
        );
    }
    assert_eq!(worker_questions(&db).len(), 1);

    // The answer went as the next turn, a resume of the same thread.
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(
        calls[1],
        format!("resume codex-thread-1 answer to ask {}: a.txt", ask.id)
    );
}
