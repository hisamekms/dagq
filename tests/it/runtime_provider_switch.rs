//! Runtime tests: a worker moves to the other provider when its own cannot
//! be used (ADR-t813-2). A task whose provider's executable is missing
//! starts on the other one; a headless turn at its provider's usage limit
//! has its call made again on the other provider, in a new session of the
//! same worktree; Codex's hold opens no ask, Claude's opens the queue's
//! hold ask for the Claude-only jobs; with neither provider the run waits
//! in the hold ask; a run switches twice at most.
use crate::common;
use crate::runtime_support;

use dagq::domain::{
    AskReason, Provider, queue_hold::USAGE_LIMIT_SUBJECT, turn::session_name, worker::WorkerMode,
};
use runtime_support::*;

const TASK: TaskId = TaskId::new(2);

/// A turn that commits and writes a receipt naming the new head.
const FINISH: &str = r#"commit work; receipt "$(git rev-parse HEAD)"; say finished"#;

/// Claude Code's words for its usage limit, as a turn's error result.
const CLAUDE_LIMIT: &str = r#"fail "Claude AI usage limit reached|1790535600""#;

/// Codex's report of its usage limit (the turn is stopped at it).
const CODEX_LIMIT: &str =
    r#"error "unexpected status 429 Too Many Requests: You have hit your usage limit."; sleep 30"#;

/// A task for a headless worker of `provider`, ready, depending on
/// `dependencies`.
fn add_task(db: &Path, provider: Provider, dependencies: &[TaskId]) -> TaskId {
    let mut queue = SqliteQueue::open(db).unwrap();
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
            dependencies: dependencies.to_vec(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: Some(provider),
            worker_mode: Some(WorkerMode::Headless),
        })
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    task.id()
}

/// The fixture's task, canceled, and in its place task 2 for a headless
/// worker of `provider`; the backend runs Claude's turns with the stub
/// `claude` and, with `codex`, Codex's with the stub `codex`, which the
/// supervisor is given too (without it, the supervisor has no Codex).
fn switch_fixture(
    provider: Provider,
    codex: bool,
) -> (Fixture, PathBuf, PathBuf, TestWorkspace, Option<PathBuf>) {
    let (dir, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    assert_eq!(add_task(&db, provider, &[]), TASK);
    let mut backend = TestWorkspace::new(&db, false, "exit 99");
    backend.headless = Some(headless_claude(dir.path(), &db));
    let codex = codex.then(|| headless_codex(dir.path(), &db));
    backend.codex = codex.clone();
    (dir, repo, db, backend, codex)
}

/// Supervise on a thread, with the stub `codex` when given and reviews of
/// `verdicts` in order.
fn supervise_thread(
    db: &Path,
    repo: &Path,
    backend: Arc<TestWorkspace>,
    codex: Option<&Path>,
    verdicts: &[String],
) -> thread::JoinHandle<Result<Value>> {
    let reviewer = TestReviewer::new(verdicts);
    let mut options = supervise_options(4, true);
    if let Some(codex) = codex {
        options.codex = codex.to_owned();
    }
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

fn detail(db: &Path, task: TaskId) -> dagq::domain::TaskDetail {
    SqliteQueue::open(db).unwrap().show(task).unwrap()
}

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

/// The open hold asks.
fn hold_asks(db: &Path) -> Vec<dagq::domain::Ask> {
    SqliteQueue::open(db)
        .unwrap()
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .filter(|ask| ask.kind == AskKind::QueueHold && ask.is_open())
        .collect()
}

/// Wait for the one open hold ask that `matches`.
fn wait_for_hold(db: &Path, matches: impl Fn(&dagq::domain::Ask) -> bool) -> dagq::domain::Ask {
    wait_until(db, common::STEP_LIMIT, |_| {
        hold_asks(db).iter().any(&matches)
    });
    hold_asks(db).into_iter().find(matches).unwrap()
}

/// The `(from, to, reason, phase)` of each of the run's switches.
fn switches(detail: &dagq::domain::TaskDetail) -> Vec<(Value, Value, Value, Value)> {
    payloads(detail, "provider_switched")
        .into_iter()
        .map(|p| {
            (
                p["from"].clone(),
                p["to"].clone(),
                p["reason"].clone(),
                p["phase"].clone(),
            )
        })
        .collect()
}

/// Acceptance (1): a Codex task on a supervisor whose `codex` is not found
/// is claimed onto headless Claude (not deferred): the run asks for Codex
/// and runs on Claude, `provider_switched` says why (`executable_missing`,
/// phase `start`), the first turn is Claude's in a session of its own name,
/// and the run lands.
#[test]
fn a_codex_task_without_codex_starts_on_claude_and_lands() {
    let (dir, repo, db, backend, _) = switch_fixture(Provider::Codex, false);
    set_turns(dir.path(), FINISH);
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let supervisor = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        None,
        &[verdict("pass", &[], "fine")],
    );
    finished(&db, &backend, supervisor);
    let detail = detail(&db, TASK);
    let run = &detail.runs[0];
    assert_landed_run(run, &repo, &base);
    assert_eq!(run.requested_provider(), Provider::Codex);
    assert_eq!(run.actual_provider(), Provider::Claude);
    assert_eq!(run.worker_mode(), WorkerMode::Headless);
    assert_eq!(
        switches(&detail),
        [(
            json!("codex"),
            json!("claude"),
            json!("executable_missing"),
            json!("start")
        )]
    );
    let claimed = payloads(&detail, "run_claimed");
    assert_eq!(claimed[0]["provider"], "claude");
    assert_eq!(claimed[0]["requested_provider"], "codex");
    // A session of its own name: the run's id is kept for a session that
    // never switched.
    let calls = stub_calls(run);
    assert!(
        calls[0].starts_with(&format!(
            "start {} You are executing dagq task 2",
            session_name(run.id().as_str(), 1)
        )),
        "{calls:?}"
    );
    assert!(queue_events(&db, "claim_deferred").is_empty());
    // `timeline` lists the switch next to the providers.
    let timeline = dagq::compose::timeline(&db, run.id(), 300, false).unwrap();
    assert_eq!(timeline["requested_provider"], "codex");
    assert_eq!(timeline["actual_provider"], "claude");
    assert_eq!(
        timeline["provider_switches"][0]["reason"],
        "executable_missing"
    );
    // `stats` counts it.
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["provider_switches"]["count"], 1, "{stats}");
    assert_eq!(
        stats["provider_switches"]["by_direction"]["codex->claude"],
        1
    );
}

/// Acceptance (2) and (3): a headless Claude turn at Claude's usage limit
/// (read by the words every Claude job's output is read by, task 438) has
/// its call made again on Codex, as the first turn of a new thread with
/// the task's prompt and the switch's text, in the same worktree. Claude's
/// hold ask opens for the Claude-only jobs without the run in it; the
/// review waits for it, and once a person answers `done` the run lands.
#[test]
fn a_claude_turn_at_its_usage_limit_moves_to_codex_and_lands() {
    let (dir, repo, db, backend, codex) = switch_fixture(Provider::Claude, true);
    set_codex_model(dir.path(), "gpt-test-codex");
    set_turns(
        dir.path(),
        &format!(
            r#"case "$TURN" in
1) {CLAUDE_LIMIT} ;;
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
        codex.as_deref(),
        &[verdict("pass", &[], "fine")],
    );
    let ask = wait_for_hold(&db, |ask| ask.reason_category == AskReason::Cost);
    assert_eq!(ask.subject.as_deref(), Some(USAGE_LIMIT_SUBJECT));
    // The run went on to Codex: the review waits for Claude's ask.
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue.show(TASK).unwrap().runs[0].status() == RunStatus::AwaitingIntegration
    });
    let held = hold_asks(&db);
    assert_eq!(held.len(), 1, "{held:?}");
    assert!(held[0].affected.is_empty(), "{:?}", held[0].affected);
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "done")
        .unwrap();
    finished(&db, &backend, supervisor);
    let detail = detail(&db, TASK);
    let run = &detail.runs[0];
    assert_landed_run(run, &repo, &base);
    assert_eq!(run.requested_provider(), Provider::Claude);
    assert_eq!(run.actual_provider(), Provider::Codex);
    assert_eq!(
        switches(&detail),
        [(
            json!("claude"),
            json!("codex"),
            json!("usage_limit"),
            json!("start")
        )]
    );
    let switched = payloads(&detail, "provider_switched");
    assert_eq!(switched[0]["turn"], 1);
    assert_eq!(switched[0]["count"], 1);
    let turns = payloads(&detail, "turn_finished");
    assert_eq!(turns[0]["failure"], "usage_limit", "{turns:?}");
    assert_eq!(turns[1]["outcome"], "succeeded");
    // The Codex turn after the switch records the model Codex used, not
    // the claim's Claude model (task 892).
    assert_eq!(turns[1]["provider"], "codex");
    assert_eq!(turns[1]["model"], "gpt-test-codex", "{turns:?}");
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls[0].starts_with(&format!("start {} ", run.id())),
        "{calls:?}"
    );
    assert!(
        calls[1].starts_with("start codex-thread-2 You are executing dagq task 2"),
        "{calls:?}"
    );
    let requested: Vec<&Value> = payloads(&detail, "turn_requested")
        .into_iter()
        .map(|p| &p["what"])
        .collect();
    assert_eq!(requested, [&json!("provider switch")]);
    let limited = payloads(&detail, "usage_limited");
    assert_eq!(limited[0]["switched_to"], "codex", "{limited:?}");
    assert!(queue_events(&db, "provider_held").is_empty());
    // `stats` keeps the requested provider on the run, beside the one that
    // did its work and how often it moved, splits its turns by the
    // provider that ran them, and groups it under the actual provider
    // (ADR-t813-2 decision 7).
    let stats = common::cli::ok(&db, &["stats", "--full"]);
    let row = &stats["runs"][0];
    assert_eq!(row["provider"], "claude", "{row}");
    assert_eq!(row["actual_provider"], "codex", "{row}");
    assert_eq!(row["provider_switches"], 1, "{row}");
    assert_eq!(row["route"], "headless", "{row}");
    let by_provider = &row["turns"]["by_provider"];
    assert_eq!(by_provider["claude"]["count"], 1, "{row}");
    assert_eq!(by_provider["claude"]["failed"], 1, "{row}");
    assert_eq!(by_provider["codex"]["count"], 1, "{row}");
    assert_eq!(by_provider["codex"]["failed"], 0, "{row}");
    assert_eq!(row["turns"]["count"], 2, "{row}");
    let versions = &stats["versions"]["provider"];
    assert_eq!(versions.as_array().unwrap().len(), 1, "{versions}");
    assert_eq!(versions[0]["version"], "codex", "{versions}");
}

/// Acceptance (2) and (3): a Codex turn at Codex's usage limit holds Codex
/// (`provider_held`) without an ask, and has its call made again on Claude
/// in a new session; the run lands. While Codex is held, a Codex task that
/// became ready meanwhile is claimed onto Claude (phase `start`, reason
/// `usage_limit`) and lands too: Claude's work goes on through Codex's
/// hold.
#[test]
fn a_codex_turn_at_its_usage_limit_moves_to_claude_without_an_ask() {
    let (dir, repo, db, backend, codex) = switch_fixture(Provider::Codex, true);
    let next = add_task(&db, Provider::Codex, &[TASK]);
    set_turns(
        dir.path(),
        &format!(r#"if [ -n "$THREAD" ]; then {CODEX_LIMIT}; else {FINISH}; fi"#),
    );
    let backend = Arc::new(backend);
    let supervisor = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        codex.as_deref(),
        &[verdict("pass", &[], "fine"), verdict("pass", &[], "fine")],
    );
    finished(&db, &backend, supervisor);
    let first = detail(&db, TASK);
    let run = &first.runs[0];
    assert_eq!(run.status(), RunStatus::Integrated, "{:?}", first.events);
    assert_eq!(run.actual_provider(), Provider::Claude);
    assert_eq!(
        switches(&first),
        [(
            json!("codex"),
            json!("claude"),
            json!("usage_limit"),
            json!("start")
        )]
    );
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[0].starts_with("start codex-thread-1 "), "{calls:?}");
    assert!(
        calls[1].starts_with(&format!(
            "start {} You are executing dagq task 2",
            session_name(run.id().as_str(), 1)
        )),
        "{calls:?}"
    );
    // Codex's hold opened no ask.
    let held = queue_events(&db, "provider_held");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["provider"], "codex");
    assert_eq!(held[0]["reason"], "usage_limit");
    assert!(
        SqliteQueue::open(&db)
            .unwrap()
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .all(|ask| ask.kind != AskKind::QueueHold)
    );
    // The next Codex task started on Claude while Codex was held.
    let second = detail(&db, next);
    assert_eq!(second.runs[0].status(), RunStatus::Integrated);
    assert_eq!(second.runs[0].actual_provider(), Provider::Claude);
    assert_eq!(
        switches(&second),
        [(
            json!("codex"),
            json!("claude"),
            json!("usage_limit"),
            json!("start")
        )]
    );
}

/// Acceptance (3): with Claude held (its hold ask open) and no Codex, no
/// worker can run: claims are held (`claim_held`) and each candidate is
/// deferred as `provider_unavailable`; once a person answers `done`, the
/// task is claimed.
#[test]
fn with_neither_provider_a_task_waits_until_one_can_take_it() {
    let (dir, repo, db, backend, _) = switch_fixture(Provider::Codex, false);
    set_turns(dir.path(), FINISH);
    let ask = open_hold_ask(&db, AskReason::Authentication, None);
    let backend = Arc::new(backend);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        ..supervise_options(2, false)
    };
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &TestReviewer::new(&[verdict("pass", &[], "fine")]),
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
        })
    };
    wait_until(&db, common::STEP_LIMIT, |_| {
        !queue_events(&db, "claim_deferred").is_empty()
    });
    let held = queue_events(&db, "claim_held");
    assert_eq!(held[0]["reason"], "authentication", "{held:?}");
    let deferred = queue_events(&db, "claim_deferred");
    assert_eq!(deferred.len(), 1, "{deferred:?}");
    assert_eq!(deferred[0]["reason"], "provider_unavailable");
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        status["claim_deferrals"][0]["reason"], "provider_unavailable",
        "{status}"
    );
    assert!(detail(&db, TASK).runs.is_empty());
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "done")
        .unwrap();
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .show(TASK)
            .unwrap()
            .runs
            .first()
            .is_some_and(|run| run.status() == RunStatus::Integrated)
    });
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    finished(&db, &backend, supervisor);
    let ended = queue_events(&db, "claim_deferral_ended");
    assert_eq!(ended[0]["why"], "cleared", "{ended:?}");
    assert_eq!(
        detail(&db, TASK).runs[0].actual_provider(),
        Provider::Claude
    );
}

/// Acceptance (3), (5): a run moves at most twice. Claude's limit moves it
/// to Codex (1); Codex's limit while Claude's ask is open cannot move it
/// back, so it joins the ask and waits, not failed; after `done`, Codex's
/// limit again moves it to Claude (2) in a new session; Claude's limit
/// then cannot move it again, and it waits in the new ask; after `done` it
/// goes on on Claude and lands.
#[test]
fn a_run_switches_twice_at_most_and_then_waits_in_the_hold_ask() {
    let (dir, repo, db, backend, codex) = switch_fixture(Provider::Claude, true);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$TURN" in
1|4) {CLAUDE_LIMIT} ;;
2|3) {CODEX_LIMIT} ;;
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
        codex.as_deref(),
        &[verdict("pass", &[], "fine")],
    );
    let run_id = || {
        detail(&db, TASK)
            .runs
            .first()
            .map(|run| run.id().to_string())
            .unwrap_or_default()
    };
    // Codex at its limit while Claude is held: the run joins Claude's ask.
    let first = wait_for_hold(&db, |ask| {
        let run = run_id();
        !run.is_empty() && ask.affected.contains(&run)
    });
    let run = &detail(&db, TASK).runs[0];
    assert_eq!(run.status(), RunStatus::Running);
    assert_eq!(run.actual_provider(), Provider::Codex);
    SqliteQueue::open(&db)
        .unwrap()
        .answer(first.id, "done")
        .unwrap();
    // Back on Claude, whose limit cannot move it again: a new ask.
    let second = wait_for_hold(&db, |ask| {
        ask.id != first.id && ask.affected.contains(&run_id())
    });
    let now = detail(&db, TASK);
    assert_eq!(
        switches(&now),
        [
            (
                json!("claude"),
                json!("codex"),
                json!("usage_limit"),
                json!("start")
            ),
            (
                json!("codex"),
                json!("claude"),
                json!("usage_limit"),
                json!("resume")
            ),
        ]
    );
    assert_eq!(now.runs[0].status(), RunStatus::Running);
    assert_eq!(now.runs[0].actual_provider(), Provider::Claude);
    SqliteQueue::open(&db)
        .unwrap()
        .answer(second.id, "done")
        .unwrap();
    finished(&db, &backend, supervisor);
    let detail = detail(&db, TASK);
    let run = &detail.runs[0];
    assert_landed_run(run, &repo, &base);
    assert_eq!(switches(&detail).len(), 2);
    let calls = stub_calls(run);
    let second_claude = session_name(run.id().as_str(), 2);
    assert!(
        calls[3].starts_with(&format!("start {second_claude} You are executing")),
        "{calls:?}"
    );
    assert!(calls[4].contains(&second_claude), "{calls:?}");
    // Codex's hold ended with the person's `done`.
    let released = queue_events(&db, "provider_released");
    assert_eq!(released[0]["why"], "done", "{released:?}");
}

/// Acceptance (1): a Codex whose agent does not start (its executable is
/// gone from under the wrapper) fails its first turn to start (`launch`):
/// Codex is held (`launch_failed`) and the call goes to Claude, and the run
/// lands.
#[test]
fn a_codex_that_does_not_start_moves_its_run_to_claude() {
    let (dir, repo, db, mut backend, codex) = switch_fixture(Provider::Codex, true);
    // The supervisor finds `codex`; the wrapper's is missing.
    backend.codex = None;
    set_turns(dir.path(), FINISH);
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let supervisor = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        codex.as_deref(),
        &[verdict("pass", &[], "fine")],
    );
    finished(&db, &backend, supervisor);
    let detail = detail(&db, TASK);
    let run = &detail.runs[0];
    assert_landed_run(run, &repo, &base);
    assert_eq!(run.actual_provider(), Provider::Claude);
    let turns = payloads(&detail, "turn_finished");
    assert_eq!(turns[0]["outcome"], "failed", "{turns:?}");
    assert_eq!(turns[0]["failure"], "launch");
    assert_eq!(
        switches(&detail),
        [(
            json!("codex"),
            json!("claude"),
            json!("launch_failed"),
            json!("start")
        )]
    );
    let held = queue_events(&db, "provider_held");
    assert_eq!(held[0]["reason"], "launch_failed", "{held:?}");
}

/// A session stopped at Claude Code's usage limit.
const LIMIT_SCREEN: &str = "\
⏺ Bash(cargo test)
  ⎿  5-hour limit reached ∙ resets 3pm
     /upgrade to increase your usage limit.

│ ❯
  ? for shortcuts
";

/// ADR-t813-2 decision 5: an interactive Claude worker idle at Claude's
/// usage limit moves to headless Codex: its session is parked and asked to
/// exit, Claude's hold ask opens without it, and the run is resumed in the
/// same worktree as a new Codex thread, which finishes it; after `done` the
/// review passes and the run lands.
#[test]
fn an_interactive_claude_session_at_its_limit_moves_to_headless_codex() {
    interactive_workers();
    let (dir, repo, db) = fixture();
    let codex = headless_codex(dir.path(), &db);
    set_turns(dir.path(), FINISH);
    let mut backend = TestWorkspace::new(&db, false, "commit work; idle; await_exit");
    backend.codex = Some(codex.clone());
    *backend.screen.lock().unwrap() = LIMIT_SCREEN.into();
    let backend = Arc::new(backend);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        codex: codex.clone(),
        stall: Some(dagq::domain::stall::StallConfig {
            idle_without_receipt_secs: 1,
            ..Default::default()
        }),
        ..supervise_options(2, false)
    };
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &TestReviewer::new(&[verdict("pass", &[], "fine")]),
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
        })
    };
    let task = TaskId::new(1);
    let ask = wait_for_hold(&db, |ask| ask.reason_category == AskReason::Cost);
    assert!(ask.affected.is_empty(), "{:?}", ask.affected);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue.show(task).unwrap().runs[0].status() == RunStatus::AwaitingIntegration
    });
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "done")
        .unwrap();
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue.show(task).unwrap().runs[0].status() == RunStatus::Integrated
    });
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    finished(&db, &backend, supervisor);
    let detail = detail(&db, task);
    let run = &detail.runs[0];
    assert_eq!(run.requested_provider(), Provider::Claude);
    assert_eq!(run.actual_provider(), Provider::Codex);
    assert_eq!(run.worker_mode(), WorkerMode::Headless);
    assert_eq!(
        switches(&detail),
        [(
            json!("claude"),
            json!("codex"),
            json!("usage_limit"),
            json!("resume")
        )]
    );
    assert!(!payloads(&detail, "recovery_parked").is_empty());
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(
        calls[0].starts_with("start codex-thread-1 You are executing dagq task 1"),
        "{calls:?}"
    );
}

/// Acceptance (3): a Codex that does not start while Claude is held (its
/// usage-limit ask open) cannot move its run: the run is not failed but
/// waits (`provider_waiting`), in the hold ask with its session kept.
/// `done` ends both holds and has the session go on; Codex fails to start
/// again, and now the run moves to Claude and lands.
#[test]
fn a_codex_that_does_not_start_while_claude_is_held_waits_in_the_hold_ask() {
    let (dir, repo, db, mut backend, codex) = switch_fixture(Provider::Codex, true);
    backend.codex = None;
    set_turns(dir.path(), FINISH);
    open_hold_ask(&db, AskReason::Cost, Some(USAGE_LIMIT_SUBJECT));
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let supervisor = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        codex.as_deref(),
        &[verdict("pass", &[], "fine")],
    );
    let run_id = || {
        detail(&db, TASK)
            .runs
            .first()
            .map(|run| run.id().to_string())
            .unwrap_or_default()
    };
    let ask = wait_for_hold(&db, |ask| {
        let run = run_id();
        !run.is_empty() && ask.affected.contains(&run)
    });
    let now = detail(&db, TASK);
    // Not failed: its session waits (no agent ever started in it, so the
    // run is still `starting`).
    assert_eq!(now.runs[0].status(), RunStatus::Starting);
    assert_eq!(now.runs[0].actual_provider(), Provider::Codex);
    let waiting = payloads(&now, "provider_waiting");
    assert_eq!(waiting.len(), 1, "{waiting:?}");
    assert_eq!(waiting[0]["reason"], "launch_failed");
    assert_eq!(waiting[0]["provider"], "codex");
    assert!(switches(&now).is_empty());
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "done")
        .unwrap();
    finished(&db, &backend, supervisor);
    let detail = detail(&db, TASK);
    let run = &detail.runs[0];
    assert_landed_run(run, &repo, &base);
    assert_eq!(run.actual_provider(), Provider::Claude);
    assert_eq!(
        switches(&detail),
        [(
            json!("codex"),
            json!("claude"),
            json!("launch_failed"),
            json!("resume")
        )]
    );
    let released = queue_events(&db, "provider_released");
    assert!(released.iter().any(|r| r["why"] == "done"), "{released:?}");
}

/// Acceptance (3), (5): a run whose switches are used up waits when Codex
/// hits its usage limit while Claude could take it: not failed, and no ask
/// (only Codex is held). Codex's hold ends at the reset its text says
/// (`try again in 2 seconds`, ADR-t813-2 decision 6), and the call is made
/// again on Codex in the same thread (`provider retry`); the run lands.
#[test]
fn a_run_out_of_switches_waits_on_codexs_hold_and_retries_at_its_reset() {
    let (dir, repo, db, backend, codex) = switch_fixture(Provider::Codex, true);
    let gate = dir.path().join("gate");
    set_turns(
        dir.path(),
        &format!(
            r#"case "$TURN" in
1) error "unexpected status 429 Too Many Requests: You have hit your usage limit. Try again in 0 seconds."; sleep 30 ;;
2) {CLAUDE_LIMIT} ;;
3) while [ ! -f {gate} ]; do sleep 0.05; done; error "unexpected status 429 Too Many Requests: You have hit your usage limit. Try again in 2 seconds."; sleep 30 ;;
*) {FINISH} ;;
esac"#,
            gate = gate.display()
        ),
    );
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let supervisor = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        codex.as_deref(),
        &[verdict("pass", &[], "fine")],
    );
    // Codex (1) -> Claude, whose limit opens its ask -> Codex (2).
    let ask = wait_for_hold(&db, |ask| ask.reason_category == AskReason::Cost);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        switches_of(&queue.show(TASK).unwrap()) == 2
    });
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "done")
        .unwrap();
    wait_until(&db, common::STEP_LIMIT, |_| hold_asks(&db).is_empty());
    fs::write(&gate, "").unwrap();
    finished(&db, &backend, supervisor);
    let detail = detail(&db, TASK);
    let run = &detail.runs[0];
    assert_landed_run(run, &repo, &base);
    assert_eq!(run.actual_provider(), Provider::Codex);
    assert_eq!(switches(&detail).len(), 2);
    let waiting = payloads(&detail, "provider_waiting");
    assert_eq!(waiting.len(), 1, "{waiting:?}");
    assert_eq!(waiting[0]["turn"], 3);
    assert_eq!(waiting[0]["provider"], "codex");
    let requested: Vec<&Value> = payloads(&detail, "turn_requested")
        .into_iter()
        .map(|p| &p["what"])
        .collect();
    assert_eq!(
        requested.last(),
        Some(&&json!("provider retry")),
        "{requested:?}"
    );
    // Its turn resumed the same thread.
    let calls = stub_calls(run);
    assert!(calls[3].starts_with("resume codex-thread-3 "), "{calls:?}");
    // The hold ended at the reset its text said, and no ask opened for it.
    let held = queue_events(&db, "provider_held");
    let last = held.last().unwrap();
    assert_eq!(last["reset_read"], true, "{held:?}");
    assert_eq!(
        last["retry_at"].as_i64().unwrap() - last["since"].as_i64().unwrap(),
        2
    );
    assert!(hold_asks(&db).is_empty());
}

fn switches_of(detail: &dagq::domain::TaskDetail) -> usize {
    payloads(detail, "provider_switched").len()
}

/// Explicit policy is tested with a missing Claude binary, not a fake login/limit.
/// Codex completes the task, leaves the lease for a manual review, and the
/// existing manual integration path can land it.
#[test]
fn no_claude_routes_a_worker_to_codex_and_releases_it_for_manual_landing() {
    let (dir, repo, db, backend, codex) = switch_fixture(Provider::Claude, true);
    let held = open_hold_ask(&db, AskReason::Cost, Some(USAGE_LIMIT_SUBJECT));
    set_turns(dir.path(), FINISH);
    set_codex_model(dir.path(), "gpt-test-codex");
    let reviewer = TestReviewer::new(&[]);
    let options = SuperviseOptions {
        no_claude: true,
        codex: codex.unwrap(),
        ..supervise_options(1, true)
    };
    let base = git_out(&repo, &["rev-parse", "main"]);
    let outcome = runtime::supervise_with_reviewer(
        &db,
        &repo,
        &backend,
        &dir.path().join("missing-claude"),
        &reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options,
    )
    .unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db, TASK);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert_eq!(run.actual_provider(), Provider::Codex);
    assert_eq!(switches(&detail)[0].2, "provider_disabled");
    assert!(reviewer.prompts().is_empty());
    assert!(reviewer.triage_prompts().is_empty());
    assert!(payloads(&detail, "review_started").is_empty());
    assert_eq!(
        payloads(&detail, "review_failed")[0]["code"],
        "provider_disabled"
    );
    assert_eq!(hold_asks(&db)[0].id, held.id);
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    let asks: Vec<_> = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .filter(|a| a.kind == AskKind::ApproveLanding)
        .collect();
    assert_eq!(asks.len(), 1);
    assert!(asks[0].question.contains("provider_disabled"));
    // Sending the manually reviewed Codex work back resumes Codex, then releases
    // the lease for another manual review, without starting a Claude review job.
    queue.answer(asks[0].id, "send_back").unwrap();
    set_turns(
        dir.path(),
        r#"commit revised; receipt "$(git rev-parse HEAD)"; say revised"#,
    );
    runtime::supervise_with_reviewer(
        &db,
        &repo,
        &backend,
        &dir.path().join("missing-claude"),
        &reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options,
    )
    .unwrap();
    backend.join();
    let resumed = queue.show(TASK).unwrap();
    assert_eq!(resumed.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(!payloads(&resumed, "resume_started").is_empty());
    assert!(payloads(&resumed, "review_started").is_empty());
    assert!(reviewer.prompts().is_empty());
    let landed = integrate(&db, 2, &repo).unwrap();
    assert_eq!(landed["outcome"], "integrated", "{landed}");
    assert_landed_run(&queue.show(TASK).unwrap().runs[0], &repo, &base);
}

#[test]
fn no_claude_without_codex_claims_nothing_and_reports_manual_policy() {
    let (dir, repo, db, backend, _) = switch_fixture(Provider::Codex, false);
    let reviewer = TestReviewer::new(&[]);
    let options = SuperviseOptions {
        no_claude: true,
        codex: dir.path().join("missing-codex"),
        ..supervise_options(1, true)
    };
    runtime::supervise_with_reviewer(
        &db,
        &repo,
        &backend,
        &dir.path().join("missing-claude"),
        &reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options,
    )
    .unwrap();
    assert!(detail(&db, TASK).runs.is_empty());
    assert!(reviewer.prompts().is_empty());
    assert!(reviewer.triage_prompts().is_empty());
    assert!(hold_asks(&db).is_empty());
    // The registration remains as stopped history after --once.
    let started = queue_events(&db, "supervisor_started");
    assert_eq!(started[0]["no_claude"], true);
}

#[test]
fn no_claude_hands_ended_recovery_to_a_person_without_calling_a_job() {
    let (dir, repo, db, backend, codex) = switch_fixture(Provider::Codex, true);
    set_turns(dir.path(), "exit 1");
    let reviewer = TestReviewer::new(&[]);
    let options = SuperviseOptions {
        no_claude: true,
        codex: codex.unwrap(),
        ..supervise_options(1, true)
    };
    runtime::supervise_with_reviewer(
        &db,
        &repo,
        &backend,
        &dir.path().join("missing-claude"),
        &reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options,
    )
    .unwrap();
    backend.join();
    let detail = detail(&db, TASK);
    assert!(reviewer.triage_prompts().is_empty());
    assert!(reviewer.prompts().is_empty());
    let events = serde_json::to_string(&detail).unwrap();
    assert!(events.contains("provider_disabled"), "{events}");
    assert!(
        SqliteQueue::open(&db)
            .unwrap()
            .run_lease(detail.runs[0].id())
            .unwrap()
            .is_none()
    );
    assert!(hold_asks(&db).is_empty());
}

#[test]
fn no_claude_waits_on_codex_limit_then_retries_codex_without_a_claude_hold() {
    let (dir, repo, db, backend, codex) = switch_fixture(Provider::Codex, true);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$TURN" in
1) error "unexpected status 429 Too Many Requests: You have hit your usage limit. Try again in 2 seconds."; sleep 30 ;;
*) {FINISH} ;;
esac"#
        ),
    );
    let reviewer = TestReviewer::new(&[]);
    let options = SuperviseOptions {
        no_claude: true,
        codex: codex.unwrap(),
        ..supervise_options(1, true)
    };
    runtime::supervise_with_reviewer(
        &db,
        &repo,
        &backend,
        &dir.path().join("missing-claude"),
        &reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options,
    )
    .unwrap();
    backend.join();
    let detail = detail(&db, TASK);
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert_eq!(detail.runs[0].actual_provider(), Provider::Codex);
    assert!(switches(&detail).is_empty());
    assert!(!payloads(&detail, "provider_waiting").is_empty());
    assert!(
        queue_events(&db, "ask_opened")
            .iter()
            .all(|p| p["kind"] != "queue_hold")
    );
    assert!(
        queue_events(&db, "provider_held")
            .iter()
            .all(|p| p["provider"] == "codex")
    );
    assert_eq!(stub_calls(&detail.runs[0]).len(), 2);
}
