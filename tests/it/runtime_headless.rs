//! Runtime tests: a headless worker (ADR-t813-1). A stub `claude` takes
//! `claude -p`'s arguments and prints stream-json; the session wrapper runs
//! one call per turn, the first with `--session-id <run>` and every later
//! one with `--resume <run>`, and the supervisor writes what it would type
//! into an interactive session as the next turn's request.
use crate::common;
use crate::runtime_support;

use dagq::domain::{AskReason, EventKind, Provider, worker::WorkerMode};
use runtime_support::*;

/// The fixture's task, canceled, and in its place task 2 (`test task`) for
/// a headless Claude worker that requires `evidence`; the backend runs its
/// turns with the stub `claude` of [`headless_claude`] (a turn does `say
/// working` until the test sets its turns).
fn headless_fixture(evidence: &[EvidenceCheck]) -> (Fixture, PathBuf, PathBuf, TestWorkspace) {
    let (dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let task = queue
        .add(NewTask {
            title: "test task".into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: evidence.to_vec(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: Some(Provider::Claude),
            worker_mode: Some(WorkerMode::Headless),
        })
        .unwrap();
    assert_eq!(task.id(), TASK);
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    let mut backend = TestWorkspace::new(&db, false, "exit 99");
    backend.headless = Some(headless_claude(dir.path(), &db));
    (dir, repo, db, backend)
}

const TASK: TaskId = TaskId::new(2);

/// A turn that commits and writes a receipt naming the new head.
const FINISH: &str = r#"commit work; receipt "$(git rev-parse HEAD)"; say finished"#;

fn detail(db: &Path) -> dagq::domain::TaskDetail {
    SqliteQueue::open(db).unwrap().show(TASK).unwrap()
}

/// Supervise the task on a thread with `stall` and the recovery jobs'
/// `recoveries`, and a review that passes.
fn supervise_thread(
    db: &Path,
    repo: &Path,
    backend: Arc<TestWorkspace>,
    stall: dagq::domain::stall::StallConfig,
    recoveries: &[String],
) -> (Arc<TestReviewer>, thread::JoinHandle<Result<Value>>) {
    supervise_thread_in(db, repo, backend, stall, recoveries, 4)
}

/// [`supervise_thread`] with `parallel` slots.
fn supervise_thread_in(
    db: &Path,
    repo: &Path,
    backend: Arc<TestWorkspace>,
    stall: dagq::domain::stall::StallConfig,
    recoveries: &[String],
    parallel: usize,
) -> (Arc<TestReviewer>, thread::JoinHandle<Result<Value>>) {
    let reviewer =
        Arc::new(TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(recoveries));
    let options = SuperviseOptions {
        stall: Some(stall),
        ..supervise_options(parallel, true)
    };
    let supervisor = {
        let (db, repo, backend, reviewer) = (
            db.to_owned(),
            repo.to_owned(),
            backend.clone(),
            reviewer.clone(),
        );
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
    (reviewer, supervisor)
}

/// Wait for the supervisor thread and its sessions; the run landed.
fn landed(
    db: &Path,
    repo: &Path,
    base: &str,
    backend: &TestWorkspace,
    supervisor: thread::JoinHandle<Result<Value>>,
) -> dagq::domain::TaskDetail {
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    // A run the outcome names more than once (parked, then landed) is last
    // as it ended.
    let last = outcome["runs"].as_array().unwrap().last().unwrap();
    assert_eq!(last["status"], "integrated", "{outcome}");
    let detail = detail(db);
    assert_landed_run(&detail.runs[0], repo, base);
    detail
}

/// The `(turn, resume)` of each `turn_started`, and the `(turn, outcome)`
/// of each `turn_finished`.
/// Pairs of two fields of each event of a kind.
type Pairs = Vec<(Value, Value)>;

fn turns(detail: &dagq::domain::TaskDetail) -> (Pairs, Pairs) {
    let started = payloads(detail, "turn_started")
        .into_iter()
        .map(|p| (p["turn"].clone(), p["resume"].clone()))
        .collect();
    let finished = payloads(detail, "turn_finished")
        .into_iter()
        .map(|p| (p["turn"].clone(), p["outcome"].clone()))
        .collect();
    (started, finished)
}

/// Acceptance (1) and (3): the first turn is `claude -p --output-format
/// stream-json --verbose --session-id <run> --permission-mode auto` with the
/// run's settings; it commits and writes the receipt, the wrapper records
/// the turn's result and writes the idle marker, and the run goes through
/// validating and review and lands. Nothing is typed into the workspace:
/// the exit is the exit request in `turns/`.
#[test]
fn a_headless_run_lands_after_its_first_turn() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), FINISH);
    let base = git_out(&repo, &["rev-parse", "main"]);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_eq!(run.worker_mode(), WorkerMode::Headless);
    assert_landed_run(run, &repo, &base);
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(
        calls[0].starts_with(&format!(
            "start {} You are executing dagq task 2, run {}.",
            run.id(),
            run.id()
        )),
        "{calls:?}"
    );
    let run_dir = Path::new(run.run_dir().unwrap());
    let args = fs::read_to_string(run_dir.join("stub-args.log")).unwrap();
    for part in [
        "-p --output-format stream-json --verbose --session-id",
        "--permission-mode auto",
        "--settings",
        "--model",
    ] {
        assert!(args.contains(part), "{part}: {args}");
    }
    // The run's settings deny signals by name, and have no hook.
    let settings: Value = serde_json::from_str(
        &fs::read_to_string(run_dir.join("claude-headless-settings.json")).unwrap(),
    )
    .unwrap();
    assert!(
        settings["permissions"]["deny"]
            .as_array()
            .unwrap()
            .contains(&json!("Bash(pkill:*)")),
        "{settings}"
    );
    assert!(settings.get("hooks").is_none(), "{settings}");
    // The turn's result is on the run's events.
    assert_eq!(
        turns(&detail),
        (
            vec![(json!(1), json!(false))],
            vec![(json!(1), json!("succeeded"))]
        )
    );
    let finished = payloads(&detail, "turn_finished")[0].clone();
    assert_eq!(finished["session_id"], json!(run.id()));
    assert_eq!(finished["failure"], Value::Null);
    assert_eq!(finished["usage"]["input_tokens"], 7);
    assert_eq!(finished["cost_usd"], 0.01);
    assert_eq!(finished["num_turns"], 2);
    assert_eq!(finished["permission_denials"], 0);
    assert_eq!(finished["session_created"], true);
    // The runtime's tokens of the turn, with Claude's cost (ADR-t813-2
    // decision 7), and the claim's provider and route.
    assert_eq!(finished["provider"], "claude");
    assert_eq!(
        finished["tokens"],
        json!({"input": 7, "output": 3, "cache_read": 0, "cache_creation": 0,
               "messages": 1, "cost_usd": 0.01})
    );
    let claimed = payloads(&detail, "run_claimed")[0];
    assert_eq!(claimed["provider"], "claude", "{claimed}");
    assert_eq!(claimed["worker_mode"], "headless", "{claimed}");
    // The stub is no versioned install of Claude Code.
    assert_eq!(claimed["provider_version"], Value::Null, "{claimed}");
    let worker = payloads(&detail, "session_closed")
        .into_iter()
        .find(|span| span["kind"] == "worker")
        .unwrap();
    assert_eq!(worker["active"], "recorded", "{worker}");
    assert_eq!(worker["tokens"], finished["tokens"], "{worker}");
    let marker: Value =
        serde_json::from_str(&fs::read_to_string(run.idle_marker_path().unwrap()).unwrap())
            .unwrap();
    assert_eq!(marker["dagq_turn"]["outcome"], "succeeded");
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "turn_finished") < position(&kinds, "session_idle_observed"));
    assert!(position(&kinds, "review_finished") < position(&kinds, "exit_requested"));
    // The exit went to the session's turns, nothing to its terminal.
    assert!(run_dir.join("turns/exit").exists());
    assert!(backend.texts().is_empty());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    assert_eq!(
        backend.captures.load(Ordering::SeqCst),
        1,
        "only the final screen"
    );
    // The worker was told it runs headless.
    assert!(read_prompt(run).contains(runtime::HEADLESS_WORKER));
}

/// Acceptance (2) and (4): the worker asks and ends its turn; the run
/// leaves its slot while it waits, and the answer goes to the same session
/// as the prompt of a resume, after which the run lands.
#[test]
fn a_worker_question_is_answered_by_a_resume_of_the_same_session() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$TURN" in
1) ask "which file"; say asked ;;
*) case "$PROMPT" in "answer to ask "*) {FINISH} ;; *) say lost ;; esac ;;
esac"#
        ),
    );
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .show(TASK)
            .unwrap()
            .events
            .iter()
            .any(|e| e.kind == "run_waiting_started")
    });
    let ask = SqliteQueue::open(&db)
        .unwrap()
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.kind == AskKind::WorkerQuestion)
        .unwrap();
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "change.txt")
        .unwrap();
    let detail = landed(&db, &repo, &base, &backend, supervisor);
    let run = &detail.runs[0];
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(
        calls[1],
        format!("resume {} answer to ask {}: change.txt", run.id(), ask.id)
    );
    assert_eq!(
        turns(&detail).0,
        [(json!(1), json!(false)), (json!(2), json!(true))]
    );
    let requested = payloads(&detail, "turn_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["what"], format!("answer of ask {}", ask.id));
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "run_waiting_started") < position(&kinds, "run_waiting_ended"));
    assert!(position(&kinds, "run_waiting_ended") < position(&kinds, "ask_delivered"));
    assert!(backend.texts().is_empty());
}

/// Acceptance (2): a review's revise goes to the same session as a resume;
/// its receipt is reviewed again and the run lands.
#[test]
fn a_revise_is_sent_to_the_same_session_as_a_resume() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$MODE" in
start) {FINISH} ;;
resume) printf 'fix\n' >> change.txt; git commit -q -am fix; receipt "$(git rev-parse HEAD)"; say fixed ;;
esac"#
        ),
    );
    let base = git_out(&repo, &["rev-parse", "main"]);
    let reviewer = TestReviewer::new(&[
        verdict("revise", &["name the file"], "almost"),
        verdict("pass", &[], "meets the acceptance"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_landed_run(run, &repo, &base);
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls[1].starts_with(&format!("resume {} ", run.id())),
        "{calls:?}"
    );
    let requested = payloads(&detail, "turn_requested");
    assert_eq!(requested[0]["what"], "revise request", "{requested:?}");
    assert_eq!(reviewer.prompts().len(), 2);
    assert!(backend.texts().is_empty());
}

/// Acceptance (2): a run parked `needs_session` (its receipt lacks the
/// required evidence) is resumed in a workspace of its own whose wrapper
/// takes the resolution request as a resume of the same session.
#[test]
fn a_parked_run_is_resumed_with_a_turn_of_the_same_session() {
    let (dir, repo, db, backend) = headless_fixture(&[EvidenceCheck::E2e]);
    set_turns(
        dir.path(),
        r#"case "$MODE" in
start) commit work; receipt "$(git rev-parse HEAD)"; say finished ;;
resume) receipt "$(git rev-parse HEAD)" succeeded passed; say evidence ;;
esac"#,
    );
    let base = git_out(&repo, &["rev-parse", "main"]);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(
        outcome["runs"].as_array().unwrap().last().unwrap()["status"],
        "integrated",
        "{outcome}"
    );
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_landed_run(run, &repo, &base);
    assert_eq!(backend.resumes.lock().unwrap().len(), 1);
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls[1].starts_with(&format!("resume {} ", run.id())),
        "{calls:?}"
    );
    assert_eq!(
        turns(&detail).0,
        [(json!(1), json!(false)), (json!(2), json!(true))]
    );
    let requested = payloads(&detail, "turn_requested");
    assert_eq!(requested[0]["what"], "resolution request", "{requested:?}");
    assert!(event_kinds(&detail).contains(&"evidence_missing"));
}

/// Acceptance (5) and (5b): a turn silent past `[stall].turn_silence_secs`
/// is stopped with what it runs; the session ends and the run goes to its
/// recovery job as a run that failed, whose `resume` with an instruction
/// is a resume of the same session, and the run lands.
#[test]
fn a_silent_turn_is_stopped_and_its_recovery_job_resumes_the_session() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$MODE" in
start) say starting; sleep 60 ;;
resume) {FINISH} ;;
esac"#
        ),
    );
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let (reviewer, supervisor) = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        dagq::domain::stall::StallConfig {
            turn_silence_secs: 5,
            ..Default::default()
        },
        &[repair(
            json!({"action": "resume", "instruction": "commit your work and write the receipt"}),
            "the turn hung",
        )],
    );
    let detail = landed(&db, &repo, &base, &backend, supervisor);
    let run = &detail.runs[0];
    let finished = payloads(&detail, "turn_finished");
    assert_eq!(finished[0]["outcome"], "silent", "{finished:?}");
    assert_eq!(finished[0]["stopped"], "no output for 5s");
    assert_eq!(finished[1]["outcome"], "succeeded");
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["alert"], "failed");
    let prompts = reviewer.triage_prompts();
    assert!(
        prompts[0].0.contains("\"outcome\":\"silent\""),
        "{}",
        prompts[0].0
    );
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls[1].starts_with(&format!("resume {} ", run.id())),
        "{calls:?}"
    );
    let args =
        fs::read_to_string(Path::new(run.run_dir().unwrap()).join("turns/turn-000002.jsonl"))
            .unwrap();
    assert!(args.contains("\"type\":\"result\""), "{args}");
}

/// Acceptance (5): a turn past `[stall].turn_limit_secs` is stopped and
/// recorded as `timed_out`, and its run goes to its recovery job.
#[test]
fn a_turn_past_its_limit_is_stopped() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        r#"i=0; while [ $i -lt 300 ]; do say tick; sleep 0.1; i=$((i + 1)); done"#,
    );
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        dagq::domain::stall::StallConfig {
            turn_limit_secs: 1,
            ..Default::default()
        },
        &[],
    );
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    let finished = payloads(&detail, "turn_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["outcome"], "timed_out");
    assert_eq!(finished[0]["stopped"], "the turn ran past 1s");
    assert_eq!(payloads(&detail, "turn_started")[0]["limit_secs"], 1);
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested[0]["alert"], "failed", "{requested:?}");
}

/// Acceptance (5b): a turn that ends with neither a receipt nor a question
/// is nudged twice, each a resume; still without one, the recovery job
/// (`stalled`, reason `turn_without_receipt`) is asked, and its
/// `send_instruction` of high confidence is the next resume. No ask opens.
#[test]
fn a_turn_without_a_receipt_is_nudged_then_its_recovery_job_instructs_it() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$PROMPT" in
*"recovery job for run"*) {FINISH} ;;
*) say thinking ;;
esac"#
        ),
    );
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let (reviewer, supervisor) = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        Default::default(),
        &[repair(
            json!({"action": "send_instruction", "instruction": "write the receipt"}),
            "the worker forgot its receipt",
        )],
    );
    let detail = landed(&db, &repo, &base, &backend, supervisor);
    let run = &detail.runs[0];
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 4, "{calls:?}");
    for call in &calls[1..] {
        assert!(
            call.starts_with(&format!("resume {} ", run.id())),
            "{calls:?}"
        );
    }
    assert!(calls[3].contains("recovery job"), "{calls:?}");
    assert_eq!(payloads(&detail, "stall_nudged").len(), 2);
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["alert"], "stalled");
    assert_eq!(requested[0]["reason"], "turn_without_receipt");
    assert_eq!(requested[0]["nudges"], 2);
    assert_eq!(requested[0]["turn"]["outcome"], "succeeded");
    let prompts = reviewer.triage_prompts();
    let prompt = &prompts[0].0;
    // The job reads the turns instead of a screen, and may not answer a
    // dialog.
    assert!(
        prompt.contains("a headless session has no screen"),
        "{prompt}"
    );
    assert!(!prompt.contains("answer_known_dialog"), "{prompt}");
    let repaired = payloads(&detail, "auto_repaired");
    assert_eq!(repaired[0]["repair"], "send_instruction", "{repaired:?}");
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished[0]["reason"], "turn_without_receipt");
    assert!(stalled_asks(&SqliteQueue::open(&db).unwrap()).is_empty());
}

/// Acceptance (5b): a turn refused three permissions goes to the recovery
/// job at once (reason `permission_denied`, no nudge); the job escalates,
/// so one `stalled` ask opens with its reason category, and a person's
/// answer is the session's next turn, after which the run lands.
#[test]
fn refused_permissions_go_to_the_recovery_job_whose_escalation_asks_a_person() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$PROMPT" in
"answer to ask "*) {FINISH} ;;
*) denied; say refused ;;
esac"#
        ),
    );
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !stalled_asks(queue).is_empty()
    });
    let ask = stalled_asks(&SqliteQueue::open(&db).unwrap()).remove(0);
    assert!(
        ask.question.contains("headless session"),
        "{}",
        ask.question
    );
    assert!(
        ask.question.contains("permission denial"),
        "{}",
        ask.question
    );
    assert_eq!(ask.reason_category, AskReason::RecoveryFailed);
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "use the tools you are allowed")
        .unwrap();
    let detail = landed(&db, &repo, &base, &backend, supervisor);
    let run = &detail.runs[0];
    assert!(payloads(&detail, "stall_nudged").is_empty());
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["reason"], "permission_denied");
    assert_eq!(requested[0]["turn"]["permission_denials"], 3);
    let finished = payloads(&detail, "turn_finished");
    assert_eq!(finished[0]["denied_tools"], json!(["Bash", "Bash", "Edit"]));
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(
        calls[1],
        format!(
            "resume {} answer to ask {}: use the tools you are allowed",
            run.id(),
            ask.id
        )
    );
    let resolved = payloads(&detail, "stall_resolved");
    assert!(
        resolved
            .iter()
            .any(|p| p["detection"] == "ask" && p["outcome"] == "answered_instruction"),
        "{resolved:?}"
    );
}

/// Task 1104: a headless run's `stalled` ask offers `stop`; a person's
/// `stop` is sent as the session's exit request (never as a turn), the ask
/// closes with `answered_stop`, and the run, ended without a receipt, fails
/// and goes to its recovery job (alert `failed`).
#[test]
fn a_stop_answer_ends_the_headless_session_and_its_run_goes_to_recovery() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), "denied; say refused");
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !stalled_asks(queue).is_empty()
    });
    let ask = stalled_asks(&SqliteQueue::open(&db).unwrap()).remove(0);
    assert_eq!(ask.options, ["wait", "intervene", "stop", "propose"]);
    assert!(
        ask.question
            .contains("`stop` to have the supervisor end the session"),
        "{}",
        ask.question
    );
    assert!(
        ask.question.contains("goes to its recovery job"),
        "{}",
        ask.question
    );
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "stop")
        .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Failed);
    // Only the first turn ran: `stop` was no turn's prompt.
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 1, "{calls:?}");
    let turns = Path::new(run.run_dir().unwrap()).join("turns");
    assert!(turns.join("exit").exists());
    for entry in fs::read_dir(&turns).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        if name.starts_with("request") {
            let text = fs::read_to_string(&path).unwrap();
            assert!(!text.contains("answer to ask"), "{name}: {text}");
        }
    }
    assert!(payloads(&detail, "turn_requested").is_empty());
    let resolved = payloads(&detail, "stall_resolved");
    let stops: Vec<_> = resolved
        .iter()
        .filter(|p| p["detection"] == "ask" && p["outcome"] == "answered_stop")
        .collect();
    assert_eq!(stops.len(), 1, "{resolved:?}");
    assert_eq!(stops[0]["ask_id"], json!(ask.id));
    let closed = stalled_asks(&SqliteQueue::open(&db).unwrap()).remove(0);
    assert!(closed.closed_at.is_some(), "{closed:?}");
    let requested = payloads(&detail, "recovery_requested");
    assert!(
        requested.iter().any(|p| p["alert"] == "failed"),
        "{requested:?}"
    );
}

/// Task 1104: a headless run waiting outside its one slot for its
/// `stalled` ask gets the `stop` only once it is back in the slot: while
/// another task holds the slot the exit is not requested, and the ask stays
/// answered and open; once the slot is free the run goes back, the exit is
/// requested and the ask closed (`answered_stop`), and the run fails and
/// goes to its recovery job.
#[test]
fn a_stop_answer_to_a_run_out_of_its_slot_waits_for_the_slot() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), "denied; say refused");
    // The task that takes the slot meanwhile finishes once the test lets it.
    let gate = dir.path().join("gate");
    backend.script_for(
        3,
        &format!(
            "while [ ! -f '{}' ]; do sleep 0.05; done\n{VALID_AGENT}",
            gate.display()
        ),
    );
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread_in(&db, &repo, backend.clone(), Default::default(), &[], 1);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .show(TASK)
            .unwrap()
            .events
            .iter()
            .any(|e| e.kind == "run_waiting_started")
    });
    let ask = stalled_asks(&SqliteQueue::open(&db).unwrap()).remove(0);
    assert_eq!(ask.options, ["wait", "intervene", "stop", "propose"]);
    // Added now, so the headless task took the slot first.
    let other = add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "other", &[]);
    assert_eq!(other, TaskId::new(3));
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !queue.show(other).unwrap().runs.is_empty()
    });
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "stop")
        .unwrap();
    let run = detail(&db).runs[0].clone();
    // The answer ends the wait; the run waits for the slot to go back.
    wait_until(&db, common::STEP_LIMIT, |_| {
        !events_of(&db, run.id(), "run_waiting_ended").is_empty()
    });
    let ended = events_of(&db, run.id(), "run_waiting_ended");
    assert_eq!(ended[0]["cause"], "answered", "{ended:?}");
    assert_eq!(ended[0]["ask_id"], json!(ask.id));
    thread::sleep(Duration::from_millis(500));
    let exit = Path::new(run.run_dir().unwrap()).join("turns").join("exit");
    assert!(!exit.exists(), "the exit was requested out of the slot");
    assert!(
        events_of(&db, run.id(), "stall_resolved")
            .iter()
            .all(|p| p["outcome"] != "answered_stop")
    );
    let open = SqliteQueue::open(&db).unwrap().read_ask(ask.id).unwrap();
    assert!(open.closed_at.is_none(), "{open:?}");

    fs::write(&gate, "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Failed);
    assert!(exit.exists());
    assert_eq!(stub_calls(run).len(), 1, "{:?}", stub_calls(run));
    assert!(payloads(&detail, "turn_requested").is_empty());
    let kinds = event_kinds(&detail);
    let stop = detail
        .events
        .iter()
        .position(|e| e.kind == "stall_resolved" && e.payload["outcome"] == "answered_stop")
        .expect("answered_stop");
    assert!(position(&kinds, "run_slot_regained") < stop, "{kinds:?}");
    assert!(
        payloads(&detail, "recovery_requested")
            .iter()
            .any(|p| p["alert"] == "failed")
    );
}

/// A turn that says it started in another permission mode than it was
/// asked (ADR-t813-1 decision 8) is stopped, and the run goes to its
/// recovery job as one that failed.
#[test]
fn a_turn_started_in_another_permission_mode_is_stopped() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), "sleep 30");
    // The stub says `default`, as haiku does for `auto`.
    let stub = backend.headless.clone().unwrap();
    let text = fs::read_to_string(&stub).unwrap();
    fs::write(
        &stub,
        text.replacen("#!/bin/sh\n", "#!/bin/sh\nPERMISSION_SAID=default\n", 1),
    )
    .unwrap();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    let finished = payloads(&detail, "turn_finished");
    assert_eq!(finished[0]["outcome"], "launch_mismatch", "{finished:?}");
    assert_eq!(
        finished[0]["stopped"],
        "the agent started in permission mode default instead of auto"
    );
}

/// A turn that failed at the provider's login holds the queue's one
/// authentication ask instead of a nudge; `done` has the session go on
/// with a resume, and the run lands.
#[test]
fn a_turn_that_failed_at_the_login_holds_the_queue_until_a_person_logs_in() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$TURN" in
1) printf '%s\n' '{{"type":"assistant","error":"authentication_failed","message":{{"model":"<synthetic>","content":[{{"type":"text","text":"Not logged in"}}]}}}}'; fail "Not logged in" ;;
*) {FINISH} ;;
esac"#
        ),
    );
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .any(|ask| ask.reason_category == AskReason::Authentication)
    });
    let ask = SqliteQueue::open(&db)
        .unwrap()
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.reason_category == AskReason::Authentication)
        .unwrap();
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "done")
        .unwrap();
    let detail = landed(&db, &repo, &base, &backend, supervisor);
    let finished = payloads(&detail, "turn_finished");
    assert_eq!(finished[0]["outcome"], "failed", "{finished:?}");
    assert_eq!(finished[0]["failure"], "authentication");
    assert!(payloads(&detail, "stall_nudged").is_empty());
    let calls = stub_calls(&detail.runs[0]);
    // The failed turn never answered: the session is started again.
    assert!(calls[1].starts_with("start "), "{calls:?}");
}

/// Task 862: each turn is a process of its own, and the one running is the
/// run's agent. The second turn (the answer to a question) starts a quiet
/// helper as its child, like an MCP server; while it runs the queue's agent
/// is that turn's process, not the first turn's, so the `idle_process`
/// watch takes the helper for the session's own and raises no alert. The
/// run's status and `agent_started` are the first turn's.
#[test]
fn a_later_turn_is_the_runs_agent_and_its_helper_no_idle_process() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            r#"echo $$ > "$RUN_DIR/agent-$TURN.pid"
case "$TURN" in
1) ask "which file"; say asked ;;
*) sleep 300 >/dev/null 2>&1 &
   echo $! > "$RUN_DIR/helper.pid.tmp"; mv "$RUN_DIR/helper.pid.tmp" "$RUN_DIR/helper.pid"
   i=0; while [ ! -e "$RUN_DIR/go" ] && [ $i -lt 1200 ]; do sleep 0.05; i=$((i + 1)); done
   kill $(cat "$RUN_DIR/helper.pid"); {FINISH} ;;
esac"#
        ),
    );
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let stall = dagq::domain::stall::StallConfig::default().with_millis("idle_process_secs", 1000);
    let reviewer = Arc::new(TestReviewer::new(&[verdict("pass", &[], "fine")]));
    let options = SuperviseOptions {
        stall: Some(stall),
        processes: Some(runtime::ProcessesPort(Arc::new(DetachedStubs {
            db: db.clone(),
        }))),
        ..supervise_options(4, true)
    };
    let supervisor = {
        let (db, repo, backend, reviewer) = (
            db.to_owned(),
            repo.to_owned(),
            backend.clone(),
            reviewer.clone(),
        );
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
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .show(TASK)
            .unwrap()
            .events
            .iter()
            .any(|e| e.kind == "run_waiting_started")
    });
    let ask = SqliteQueue::open(&db)
        .unwrap()
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.kind == AskKind::WorkerQuestion)
        .unwrap();
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "change.txt")
        .unwrap();
    let run = detail(&db).runs[0].clone();
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    wait_until(&db, common::STEP_LIMIT, |_| {
        run_dir.join("helper.pid").exists()
    });
    let pid = |name: &str| -> u32 {
        fs::read_to_string(run_dir.join(name))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    };
    let (first, second) = (pid("agent-1.pid"), pid("agent-2.pid"));
    let agents: Vec<_> = SqliteQueue::open(&db)
        .unwrap()
        .processes(run.id())
        .unwrap()
        .into_iter()
        .filter(|p| p.role == "agent" && p.exited_at.is_none())
        .collect();
    assert_eq!(agents.len(), 1, "{agents:?}");
    assert_eq!(agents[0].pid, second, "turn 1 was {first}");
    assert_ne!(first, second);
    // The helper is the turn's child, as an MCP server is Claude's.
    let parent = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid("helper.pid").to_string()])
        .bounded_output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&parent.stdout).trim(),
        second.to_string()
    );
    // Several samples (one a second) past the threshold with the quiet
    // helper alive. The threshold is a second, well past the moment
    // between a turn's start and its registration as the agent.
    thread::sleep(Duration::from_millis(3500));
    fs::write(run_dir.join("go"), "").unwrap();
    let detail = landed(&db, &repo, &base, &backend, supervisor);
    assert!(
        payloads(&detail, "recovery_requested").is_empty(),
        "{:?}",
        payloads(&detail, "recovery_requested")
    );
    assert!(!pid_alive(pid("helper.pid")));
    let started = payloads(&detail, "agent_started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["pid"], first);
    assert_eq!(
        turns(&detail).0,
        [(json!(1), json!(false)), (json!(2), json!(true))]
    );
}

/// A turn's line that starts a `sleep` in a process group of its own, as
/// Codex runs its commands (task 1061: the command's pgid is its own pid),
/// and writes its pid to `outside.pid` in the run directory.
const OUTSIDE_THE_GROUP: &str = r#"perl -e 'setpgrp(0, 0); exec "sleep", "600"' >/dev/null 2>&1 &
echo $! > "$RUN_DIR/outside.pid""#;

/// The pid the turn wrote to `name` in `run`'s directory.
fn written_pid(run: &TaskRun, name: &str) -> u32 {
    fs::read_to_string(Path::new(run.run_dir().unwrap()).join(name))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// Kills `pid` when dropped, so that a test that fails leaves no `sleep`.
struct Reaped(u32);

impl Drop for Reaped {
    fn drop(&mut self) {
        // Only while it runs: a pid that ended may be another's by now.
        if running(self.0) {
            // SAFETY: kill(2) takes no pointer; the pid is the test's own
            // sleep.
            unsafe { libc::kill(self.0 as libc::pid_t, libc::SIGKILL) };
        }
    }
}

/// Whether `pid` still runs after a few seconds' wait for it to end.
fn still_running(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while running(pid) {
        if Instant::now() >= deadline {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Task 1085 (1): a turn past its limit is stopped with the command it runs
/// in a process group of its own, which a signal to the turn's group does
/// not reach.
#[test]
fn a_turn_past_its_limit_is_stopped_with_its_command_outside_its_group() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            "{OUTSIDE_THE_GROUP}\ni=0; while [ $i -lt 300 ]; do say tick; sleep 0.1; i=$((i + 1)); done"
        ),
    );
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        dagq::domain::stall::StallConfig {
            turn_limit_secs: 2,
            ..Default::default()
        },
        &[],
    );
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    let outside = Reaped(written_pid(&detail.runs[0], "outside.pid"));
    let finished = payloads(&detail, "turn_finished");
    assert_eq!(finished[0]["outcome"], "timed_out", "{finished:?}");
    assert!(
        !still_running(outside.0),
        "the command outside the turn's group outlived the turn"
    );
}

/// Task 1085 (1): the exit request stops a running turn with the command it
/// runs in a process group of its own.
#[test]
fn the_exit_request_stops_a_turn_with_its_command_outside_its_group() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!("{OUTSIDE_THE_GROUP}\nsay working; : > \"$RUN_DIR/turns/exit\"; sleep 30"),
    );
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    let outside = Reaped(written_pid(&detail.runs[0], "outside.pid"));
    let finished = payloads(&detail, "turn_finished");
    assert_eq!(finished[0]["outcome"], "stopped", "{finished:?}");
    assert_eq!(
        finished[0]["stopped"],
        "the supervisor asked the session to exit"
    );
    assert!(
        !still_running(outside.0),
        "the command outside the turn's group outlived the turn"
    );
}

/// Task 1085 (2): a turn that ends by itself has what it left in its group
/// stopped, and what it left in a group of its own left running: its
/// parent is 1 by then, so it is no longer found as the turn's
/// (headless-worker.md).
#[test]
fn a_turn_that_ends_by_itself_leaves_what_runs_outside_its_group() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            "{OUTSIDE_THE_GROUP}\nsleep 600 >/dev/null 2>&1 &\necho $! > \"$RUN_DIR/inside.pid\"\n{FINISH}"
        ),
    );
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    let detail = landed(&db, &repo, &base, &backend, supervisor);
    let run = &detail.runs[0];
    let outside = Reaped(written_pid(run, "outside.pid"));
    let inside = Reaped(written_pid(run, "inside.pid"));
    assert!(!still_running(inside.0), "the turn's group outlived it");
    assert!(running(outside.0));
}

/// The headless run's supervisor died with its first turn ended and its
/// `stalled` ask answered with an instruction: after writing the answer's
/// request when `written` (and before closing the ask), before writing it
/// otherwise (task 863). The supervisor that adopts the run closes the ask
/// and sends the answer as a turn exactly once: never again when the dead
/// supervisor had written it, and itself when it had not. The turn of the
/// answer waits for the test's `go` file, so the adopter's watch meets the
/// ask while it runs.
fn adopted_stalled_answer(written: bool) {
    use dagq::domain::turn::{TurnRequest, next_seq, request_path, turns_dir};
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$PROMPT" in
"answer to ask "*) while [ ! -f "$RUN_DIR/go" ]; do sleep 0.05; done; {FINISH} ;;
*) say working ;;
esac"#
        ),
    );
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !payloads(&queue.show(TASK).unwrap(), "turn_finished").is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue
        .ask(NewAsk {
            topics: Vec::new(),
            kind: AskKind::Stalled,
            task_id: None,
            run_id: Some(run.id().clone()),
            question: "the headless session ended its turns without a receipt".into(),
            options: vec!["wait".into(), "intervene".into()],
            asked_by: "supervisor".into(),
            reason_category: AskReason::RecoveryFailed,
            finding_id: None,
        })
        .unwrap()
        .ask;
    queue.answer(ask.id, "go on").unwrap();
    let what = format!("answer of the stalled ask {}", ask.id);
    if written {
        // What the dead supervisor wrote and recorded before it stopped.
        let turns = turns_dir(&run_dir);
        fs::create_dir_all(&turns).unwrap();
        let names: Vec<String> = fs::read_dir(&turns)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        let seq = next_seq(names.iter().map(String::as_str));
        let request = TurnRequest {
            seq,
            what: what.clone(),
            prompt: format!("answer to ask {}: go on", ask.id),
        };
        let path = request_path(&run_dir, seq);
        fs::write(
            path.with_extension("json.tmp"),
            serde_json::to_string(&request).unwrap(),
        )
        .unwrap();
        fs::rename(path.with_extension("json.tmp"), &path).unwrap();
        queue
            .record_runtime_event(
                run.id(),
                EventKind::TurnRequested,
                json!({"seq": seq, "what": what, "workspace_id": run.workspace_id()}),
            )
            .unwrap();
        // The session took it.
        wait_until(&db, common::STEP_LIMIT, |_| stub_calls(&run).len() == 2);
    }
    age_lease(&db, &run, 31);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue.read_ask(ask.id).unwrap().closed_at.is_some()
    });
    fs::write(run_dir.join("go"), "").unwrap();
    let detail = landed(&db, &repo, &base, &backend, supervisor);
    let run = &detail.runs[0];
    let calls = stub_calls(run);
    let answers: Vec<&String> = calls
        .iter()
        .filter(|call| call.contains(&format!("answer to ask {}", ask.id)))
        .collect();
    assert_eq!(answers.len(), 1, "{calls:?}");
    let requested: Vec<&Value> = payloads(&detail, "turn_requested")
        .into_iter()
        .filter(|p| p["what"] == what.as_str())
        .collect();
    assert_eq!(requested.len(), 1, "{requested:?}");
    let requests = fs::read_dir(turns_dir(&run_dir))
        .unwrap()
        .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap_or_default())
        .filter(|content| content.contains(&what))
        .count();
    assert_eq!(requests, 1);
    let resolved = payloads(&detail, "stall_resolved");
    let of_ask: Vec<&Value> = resolved
        .iter()
        .copied()
        .filter(|p| p["detection"] == "ask" && p["ask_id"] == json!(ask.id))
        .collect();
    assert_eq!(of_ask.len(), 1, "{resolved:?}");
    assert_eq!(of_ask[0]["outcome"], "answered_instruction");
    let closed = SqliteQueue::open(&db).unwrap().read_ask(ask.id).unwrap();
    assert_eq!(closed.answer.as_deref(), Some("go on"));
}

/// Task 863 (1): the dead supervisor wrote the answer's request; the
/// adopter does not write another.
#[test]
fn an_adopter_does_not_send_a_stalled_answer_already_requested() {
    adopted_stalled_answer(true);
}

/// Task 863 (2): the dead supervisor stopped before writing the answer's
/// request; the adopter sends it once.
#[test]
fn an_adopter_sends_a_stalled_answer_not_yet_requested() {
    adopted_stalled_answer(false);
}
