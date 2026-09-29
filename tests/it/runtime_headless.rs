//! Runtime tests: a headless worker (ADR-t813-1). A stub `claude` takes
//! `claude -p`'s arguments and prints stream-json; the session wrapper runs
//! one call per turn, the first with `--session-id <run>` and every later
//! one with `--resume <run>`, and the supervisor writes what it would type
//! into an interactive session as the next turn's request.
use crate::common;
use crate::runtime_support;

use dagq::domain::{AskReason, Provider, worker::WorkerMode};
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
    let reviewer =
        Arc::new(TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(recoveries));
    let options = SuperviseOptions {
        stall: Some(stall),
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
