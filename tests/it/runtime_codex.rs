//! Runtime tests: a headless Codex worker (ADR-t813-1, ADR-t813-3). A stub
//! `codex` takes `codex exec --json` and `codex exec resume --json`'s
//! arguments and prints Codex's JSONL; the session wrapper starts the
//! first turn with `codex exec`, records the thread Codex names in
//! `thread.started`, and resumes that thread with `codex exec resume
//! <thread>` for every later turn, with the same sandbox given as `-c` on
//! each call.
use crate::common;
use crate::runtime_support;

use dagq::domain::{AskReason, Provider, queue_hold::USAGE_LIMIT_SUBJECT, worker::WorkerMode};
use runtime_support::*;

const TASK: TaskId = TaskId::new(2);

/// A turn that commits and writes a receipt naming the new head.
const FINISH: &str = r#"commit work; receipt "$(git rev-parse HEAD)"; say finished"#;

/// The fixture's task, canceled, and in its place task 2 (`test task`) for
/// a Codex worker; the backend runs its turns with the stub `codex` of
/// [`headless_codex`], which the supervisor is given too (so that it
/// claims the task).
fn codex_fixture() -> (Fixture, PathBuf, PathBuf, TestWorkspace, PathBuf) {
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
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: Some(Provider::Codex),
            worker_mode: Some(WorkerMode::Headless),
        })
        .unwrap();
    assert_eq!(task.id(), TASK);
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    let codex = headless_codex(dir.path(), &db);
    let mut backend = TestWorkspace::new(&db, false, "exit 99");
    backend.codex = Some(codex.clone());
    (dir, repo, db, backend, codex)
}

fn detail(db: &Path) -> dagq::domain::TaskDetail {
    SqliteQueue::open(db).unwrap().show(TASK).unwrap()
}

/// Supervise with the stub `codex` on a thread, with reviews of
/// `verdicts` in order.
fn supervise_thread(
    db: &Path,
    repo: &Path,
    backend: Arc<TestWorkspace>,
    codex: &Path,
    verdicts: &[String],
) -> thread::JoinHandle<Result<Value>> {
    let reviewer = TestReviewer::new(verdicts);
    let options = SuperviseOptions {
        codex: codex.to_owned(),
        ..supervise_options(4, true)
    };
    let (db, repo) = (db.to_owned(), repo.to_owned());
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
    })
}

/// Wait for the supervisor thread and its sessions.
fn finished(backend: &TestWorkspace, supervisor: thread::JoinHandle<Result<Value>>) -> Value {
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    outcome
}

/// The one open ask of `kind`'s reason once it opened.
fn open_ask(db: &Path, matches: impl Fn(&dagq::domain::Ask) -> bool) -> dagq::domain::Ask {
    wait_until(db, common::STEP_LIMIT, |queue| {
        queue
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .any(&matches)
    });
    SqliteQueue::open(db)
        .unwrap()
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(matches)
        .unwrap()
}

/// The person's `~/.codex/config.toml`, which no run may change
/// (ADR-t813-3 decision 6); `None` when there is none.
fn codex_config() -> Option<Vec<u8>> {
    let home = std::env::var_os("HOME")?;
    fs::read(Path::new(&home).join(".codex/config.toml")).ok()
}

/// Acceptance (1) to (4): the first turn is `codex exec --json -C
/// <worktree>` with the sandbox of ADR-t813-3 as `-c`; its thread is
/// recorded on the run; the worker's question is answered and the
/// review's revise sent as `codex exec resume <thread>` with the same `-c`;
/// every turn's usage, and a command the sandbox refused, are on the run's
/// events; the run lands, and the person's Codex settings are unchanged.
#[test]
fn a_codex_run_answers_and_revises_through_resumes_of_its_thread_and_lands() {
    let config = codex_config();
    let (dir, repo, db, backend, codex) = codex_fixture();
    set_codex_model(dir.path(), "gpt-test-codex");
    set_turns(
        dir.path(),
        &format!(
            r#"case "$TURN" in
1) ask "which file"; say asked ;;
2) case "$PROMPT" in "answer to ask "*) refused "touch ~/x"; {FINISH} ;; *) say lost ;; esac ;;
*) printf 'fix\n' >> change.txt; git commit -q -am fix; receipt "$(git rev-parse HEAD)"; say fixed ;;
esac"#
        ),
    );
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let supervisor = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        &codex,
        &[
            verdict("revise", &["name the file"], "almost"),
            verdict("pass", &[], "meets the acceptance"),
        ],
    );
    let ask = open_ask(&db, |ask| ask.kind == AskKind::WorkerQuestion);
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "change.txt")
        .unwrap();
    let outcome = finished(&backend, supervisor);
    assert_eq!(
        outcome["runs"].as_array().unwrap().last().unwrap()["status"],
        "integrated",
        "{outcome}"
    );
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_landed_run(run, &repo, &base);
    assert_eq!(run.requested_provider(), Provider::Codex);
    assert_eq!(run.actual_provider(), Provider::Codex);
    assert_eq!(run.worker_mode(), WorkerMode::Headless);

    // One thread, started once and resumed for the answer and the revise.
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert!(
        calls[0].starts_with(&format!(
            "start codex-thread-1 You are executing dagq task 2, run {}.",
            run.id()
        )),
        "{calls:?}"
    );
    assert_eq!(
        calls[1],
        format!("resume codex-thread-1 answer to ask {}: change.txt", ask.id)
    );
    assert!(calls[2].starts_with("resume codex-thread-1 "), "{calls:?}");
    let identified = payloads(&detail, "turn_session_identified");
    assert_eq!(identified.len(), 1, "{identified:?}");
    assert_eq!(identified[0]["session_id"], "codex-thread-1");
    assert_eq!(identified[0]["provider"], "codex");
    assert_eq!(identified[0]["turn"], 1);
    let started: Vec<(Value, Value)> = payloads(&detail, "turn_started")
        .into_iter()
        .map(|p| (p["resume"].clone(), p["session_id"].clone()))
        .collect();
    assert_eq!(
        started,
        [
            (json!(false), Value::Null),
            (json!(true), json!("codex-thread-1")),
            (json!(true), json!("codex-thread-1")),
        ]
    );
    let requested: Vec<&Value> = payloads(&detail, "turn_requested")
        .into_iter()
        .map(|p| &p["what"])
        .collect();
    assert_eq!(
        requested,
        [
            &json!(format!("answer of ask {}", ask.id)),
            &json!("revise request")
        ]
    );

    // The same sandbox on every call; the worktree by `-C` only on the
    // first (resume has none).
    let args = stub_args(run);
    let calls_args: Vec<&String> = args
        .iter()
        .filter(|line| line.starts_with(" exec|"))
        .collect();
    assert_eq!(calls_args.len(), 3, "{args:?}");
    let run_dir = run.run_dir().unwrap();
    for (at, line) in calls_args.iter().enumerate() {
        for part in [
            r#" -c| sandbox_mode="workspace-write"|"#,
            r#" -c| approval_policy="never"|"#,
            " -c| sandbox_workspace_write.network_access=true|",
            " --json|",
            " model_reasoning_effort=",
        ] {
            assert!(line.contains(part), "call {at}, {part}: {line}");
        }
        let roots = line
            .split('|')
            .find(|arg| arg.contains("sandbox_workspace_write.writable_roots="))
            .unwrap();
        for root in [
            "/worktrees/",
            "/objects\"",
            "/refs/heads/dagq\"",
            "/logs/refs/heads/dagq\"",
            "/registry\"",
        ] {
            assert!(roots.contains(root), "{root}: {roots}");
        }
        assert!(roots.contains(&format!("\"{run_dir}\"")), "{roots}");
        assert!(
            !roots.contains("/.git\""),
            "not the whole common dir: {roots}"
        );
        assert_eq!(line.contains(" resume|"), at > 0, "{line}");
        assert_eq!(line.contains(" -C|"), at == 0, "{line}");
    }
    assert!(
        calls_args[1].contains(" --| codex-thread-1| answer to ask"),
        "{}",
        calls_args[1]
    );

    // Every turn's result and usage are recorded; the refusal of the
    // sandbox is one of the turn's denials.
    let turns = payloads(&detail, "turn_finished");
    assert_eq!(turns.len(), 3, "{turns:?}");
    // Codex's usage is the thread's running total: the stub's grows by the
    // same amount each turn.
    for (n, turn) in (1..).zip(&turns) {
        assert_eq!(turn["outcome"], "succeeded", "{turn}");
        assert_eq!(turn["failure"], Value::Null);
        assert_eq!(turn["session_id"], "codex-thread-1");
        assert_eq!(turn["usage"]["input_tokens"], 11 * n);
        assert_eq!(turn["usage"]["cached_input_tokens"], 4 * n);
        assert_eq!(turn["usage"]["reasoning_output_tokens"], 2 * n);
        assert_eq!(turn["tokens_total"]["input"], 7 * n, "{turn}");
    }
    assert_eq!(turns[1]["denied_tools"], json!(["sandbox: touch ~/x"]));
    assert_eq!(turns[2]["message"], "fixed");
    // Each turn's own, in the runtime's kinds of token (ADR-t813-2
    // decision 7): what it added to the total, the cached input apart, the
    // reasoning in the output, no cost.
    for turn in &turns {
        assert_eq!(turn["provider"], "codex");
        assert_eq!(
            turn["tokens"],
            json!({"input": 7, "output": 5, "cache_read": 4, "cache_creation": 0, "messages": 1})
        );
    }
    // Each turn records the model Codex used, read from the thread's
    // rollout (task 892).
    for turn in &turns {
        assert_eq!(turn["model"], "gpt-test-codex", "{turn}");
        assert_eq!(turn["model_unknown"], Value::Null, "{turn}");
    }
    // The claim names the provider, the route and Codex's version, and no
    // Claude model: its step is `ladder_model`, and why the model is not
    // known yet is `model_unknown`; it is in no trial group.
    let claimed = payloads(&detail, "run_claimed")[0];
    assert_eq!(claimed["model"], Value::Null, "{claimed}");
    assert_eq!(claimed["ladder_model"], "claude-opus-5-5", "{claimed}");
    assert_eq!(claimed["group"], Value::Null, "{claimed}");
    assert_eq!(
        claimed["model_unknown"],
        dagq::domain::worker_model::CODEX_MODEL_UNKNOWN
    );
    // So does the revise's session.
    let revise = payloads(&detail, "revise_requested")[0];
    assert_eq!(revise["model"], Value::Null, "{revise}");
    assert!(revise["ladder_model"].is_string(), "{revise}");
    assert_eq!(claimed["provider"], "codex", "{claimed}");
    assert_eq!(claimed["worker_mode"], "headless", "{claimed}");
    assert_eq!(claimed["codex_version"], "0.46.0", "{claimed}");
    assert_eq!(claimed["provider_version"], "0.46.0", "{claimed}");
    // The run's spans take their time and tokens from its turns.
    let spans: Vec<&Value> = payloads(&detail, "session_closed")
        .into_iter()
        .filter(|span| span["kind"] != "review")
        .collect();
    assert!(!spans.is_empty());
    let input: i64 = spans
        .iter()
        .map(|span| {
            assert_eq!(span["active"], "recorded", "{span}");
            span["tokens"]["input"].as_i64().unwrap_or(0)
        })
        .sum();
    assert_eq!(input, 21, "{spans:?}");
    // `stats` shows them on the run, and per provider and route.
    let stats = common::cli::ok(&db, &["stats", "--full"]);
    let row = &stats["runs"][0];
    assert_eq!(row["provider"], "codex", "{row}");
    assert_eq!(row["route"], "headless", "{row}");
    assert_eq!(row["codex_version"], "0.46.0", "{row}");
    assert_eq!(row["worker_model"], "gpt-test-codex", "{row}");
    assert_eq!(row["claude_version"], Value::Null, "{row}");
    assert_eq!(row["trial_group"], Value::Null, "{row}");
    assert_eq!(row["turns"]["count"], 3, "{row}");
    assert_eq!(row["tokens"]["input"], 21, "{row}");
    assert_eq!(row["tokens"]["cache_read"], 12, "{row}");
    assert_eq!(stats["versions"]["provider"][0]["version"], "codex");
    assert_eq!(stats["versions"]["route"][0]["version"], "headless");
    assert_eq!(stats["versions"]["codex"][0]["version"], "0.46.0");

    // The rules against pkill / killall are kept out of Git.
    let exclude = fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
    assert!(
        exclude.contains(dagq::infrastructure::codex::RULES_PATH),
        "{exclude}"
    );
    assert!(read_prompt(run).contains(runtime::HEADLESS_WORKER));
    assert!(backend.texts().is_empty());
    assert_eq!(codex_config(), config, "~/.codex/config.toml is unchanged");
}

/// Acceptance (3): a turn that failed says why on the run's events: a
/// model Codex cannot use is `model`, the session ends and the run goes to
/// its recovery job as one that failed. It is no reason to move the worker
/// to Claude (ADR-t813-2 decision 3), which could take it.
#[test]
fn a_codex_turn_that_failed_is_classified_on_the_run() {
    let (dir, repo, db, backend, codex) = codex_fixture();
    set_turns(
        dir.path(),
        r#"error "Reconnecting... 1/5 (stream disconnected)"; fail "unexpected status 400 Bad Request: model nope does not exist""#,
    );
    let backend = Arc::new(backend);
    let supervisor = supervise_thread(&db, &repo, backend.clone(), &codex, &[]);
    finished(&backend, supervisor);
    let detail = detail(&db);
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    let turns = payloads(&detail, "turn_finished");
    assert_eq!(turns.len(), 1, "{turns:?}");
    assert_eq!(turns[0]["outcome"], "failed");
    assert_eq!(turns[0]["failure"], "model");
    assert_eq!(turns[0]["exit_code"], 1);
    // No rollout named the model: it is not known, and why is said.
    assert_eq!(turns[0]["model"], Value::Null);
    assert!(
        turns[0]["model_unknown"]
            .as_str()
            .unwrap()
            .contains("no rollout of thread codex-thread-1"),
        "{}",
        turns[0]
    );
    assert!(
        turns[0]["message"]
            .as_str()
            .unwrap()
            .contains("does not exist"),
        "{}",
        turns[0]
    );
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested[0]["alert"], "failed", "{requested:?}");
    assert!(payloads(&detail, "provider_switched").is_empty());
    assert_eq!(detail.runs[0].actual_provider(), Provider::Codex);
}

/// Acceptance (3): a turn at Codex's usage limit is stopped at the first
/// error that says it; with Claude held too (its usage-limit ask open), the
/// run cannot move to Claude (ADR-t813-2) and joins the ask (`cost`)
/// rather than failing; `done` has the session go on with a resume of the
/// same thread, and the run lands.
#[test]
fn a_codex_turn_at_its_usage_limit_holds_the_queue_then_resumes_its_thread() {
    let (dir, repo, db, backend, codex) = codex_fixture();
    open_hold_ask(&db, AskReason::Cost, Some(USAGE_LIMIT_SUBJECT));
    set_turns(
        dir.path(),
        &format!(
            r#"case "$TURN" in
1) error "unexpected status 429 Too Many Requests: You have hit your usage limit. Try again at 3pm."; sleep 30 ;;
*) {FINISH} ;;
esac"#
        ),
    );
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let supervisor = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        &codex,
        &[verdict("pass", &[], "meets the acceptance")],
    );
    let ask = open_ask(&db, |ask| {
        ask.reason_category == AskReason::Cost && !ask.affected.is_empty()
    });
    // Waiting, not failed, on Codex.
    let run = &detail(&db).runs[0];
    assert_eq!(run.status(), RunStatus::Running);
    assert_eq!(run.actual_provider(), Provider::Codex);
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "done")
        .unwrap();
    finished(&backend, supervisor);
    let detail = detail(&db);
    assert_landed_run(&detail.runs[0], &repo, &base);
    assert!(payloads(&detail, "provider_switched").is_empty());
    let turns = payloads(&detail, "turn_finished");
    assert_eq!(turns[0]["outcome"], "failed", "{turns:?}");
    assert_eq!(turns[0]["failure"], "usage_limit");
    assert!(
        turns[0]["stopped"]
            .as_str()
            .unwrap()
            .contains("usage limit"),
        "{}",
        turns[0]
    );
    // The thread was named before the limit: it is resumed.
    let calls = stub_calls(&detail.runs[0]);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[1].starts_with("resume codex-thread-1 "), "{calls:?}");
}

/// A thread Codex did not keep (the turn that named it was stopped before
/// it was saved) fails its resume with `no rollout found`: the wrapper
/// forgets it and runs the same request as a new thread, with the task's
/// prompt before it, and the run lands.
#[test]
fn a_resume_of_a_thread_codex_does_not_have_starts_a_new_one() {
    let (dir, repo, db, backend, codex) = codex_fixture();
    // Claude held too: the run waits for `done` on Codex (ADR-t813-2).
    open_hold_ask(&db, AskReason::Cost, Some(USAGE_LIMIT_SUBJECT));
    set_turns(
        dir.path(),
        &format!(
            r#"case "$TURN" in
1) error "unexpected status 429 Too Many Requests: You have hit your usage limit."; sleep 30 ;;
2) echo "Error: thread/resume: thread/resume failed: no rollout found for thread id $THREAD (code -32600)" >&2; exit 1 ;;
*) {FINISH} ;;
esac"#
        ),
    );
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let supervisor = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        &codex,
        &[verdict("pass", &[], "meets the acceptance")],
    );
    let ask = open_ask(&db, |ask| {
        ask.reason_category == AskReason::Cost && !ask.affected.is_empty()
    });
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "done")
        .unwrap();
    finished(&backend, supervisor);
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_landed_run(run, &repo, &base);
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert!(calls[1].starts_with("resume codex-thread-1 "), "{calls:?}");
    assert!(
        calls[2].starts_with("start codex-thread-3 You are executing dagq task 2"),
        "{calls:?}"
    );
    let identified: Vec<(Value, Value)> = payloads(&detail, "turn_session_identified")
        .into_iter()
        .map(|p| (p["session_id"].clone(), p["missing"].clone()))
        .collect();
    assert_eq!(
        identified,
        [
            (json!("codex-thread-1"), Value::Null),
            (Value::Null, json!("codex-thread-1")),
            (json!("codex-thread-3"), Value::Null),
        ]
    );
    let turns = payloads(&detail, "turn_finished");
    assert_eq!(turns[1]["outcome"], "failed", "{turns:?}");
    assert_eq!(turns[2]["outcome"], "succeeded");
}
