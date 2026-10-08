//! The recovery job on Codex (task 1225, as the goal review of
//! ADR-t1063-1): `[roles.recovery]`'s `provider = "codex"` starts the job
//! of a run that ended (`triage_started`) and of a live run
//! (`recovery_requested`) as `codex exec --json` in the read-only sandbox,
//! played by the stub `codex` of [`headless_codex`], and its verdict goes
//! through the same checks as Claude's: the actions the alert allows,
//! `repair` applied only with confidence `high` and its preconditions held
//! now, and anything else asked. A Codex job that fails goes to the same
//! person (`triage_failed`, the `recovery_failed` ask), and under
//! `--no-claude` never to Claude.
use crate::common;
use crate::runtime_support;

use dagq::application::RunLog;
use dagq::domain::provider_switch::{ProviderHold, SwitchReason};
use dagq::domain::{AskReason, EventKind, Provider};
use runtime_support::*;
use std::time::{SystemTime, UNIX_EPOCH};

/// The thread the stub `codex` names for a read-only job.
const THREAD: &str = "codex-review-thread";

/// The model the stub `codex` writes to a read-only job's rollout.
const MODEL: &str = "gpt-test-recovery";

/// Commit `[roles.recovery]` with Codex, its model and effort, to the
/// main checkout.
fn select_codex_recovery(repo: &Path) {
    fs::write(
        repo.join("dagq.toml"),
        "[roles.recovery]\nprovider = 'codex'\nmodel = 'gpt-6-astra'\neffort = 'high'\n",
    )
    .unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-m", "select Codex for recovery"]);
}

/// What a Codex job whose login ran out prints.
const CODEX_AUTH_FAILURE: &str = "{\"type\":\"error\",\"message\":\"unexpected status 401 Unauthorized: Missing bearer or basic authentication in header\"}\n{\"type\":\"turn.failed\",\"error\":{\"message\":\"unexpected status 401 Unauthorized\"}}\n";

/// Have the stub `codex` in `dir` answer its `call`th read-only job with
/// `verdict`, and write [`MODEL`] to the job's rollout.
fn codex_verdict(dir: &Path, call: usize, verdict: Value) {
    codex_reply(dir, call, verdict);
    fs::write(dir.join("codex-review-model"), MODEL).unwrap();
}

/// Have the stub `codex` in `dir` answer its `call`th read-only job with
/// `verdict`, writing no rollout.
fn codex_reply(dir: &Path, call: usize, verdict: Value) {
    let reply = json!({"type": "item.completed", "item": {"id": "recovery", "type": "agent_message", "text": verdict.to_string()}});
    fs::write(
        dir.join(format!("codex-review-{call}.jsonl")),
        format!("{reply}\n"),
    )
    .unwrap();
}

/// The stub `codex` and the supervisor's options with it as the Codex of
/// the jobs, its home next to it.
fn codex_options(dir: &Path, db: &Path, parallel: usize) -> (PathBuf, SuperviseOptions) {
    let codex = headless_codex(dir, db);
    let options = SuperviseOptions {
        codex: codex.clone(),
        codex_home: Some(dir.join(headless::CODEX_HOME)),
        ..supervise_options(parallel, true)
    };
    (codex, options)
}

/// A worker script that fails its first run without a commit and lands
/// every later one.
fn fails_once(dir: &Path) -> String {
    let mark = dir.join("failed-once");
    format!(
        "if [ ! -f {mark} ]; then : > {mark}; exit 7; fi; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
        mark = shell_path(&mark)
    )
}

/// The events of `kind` of `run`, in order.
fn run_payloads(detail: &dagq::domain::TaskDetail, run: &TaskRun, kind: &str) -> Vec<Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == kind && e.run_id.as_ref() == Some(run.id()))
        .map(|e| e.payload.clone())
        .collect()
}

/// The provider of each recovery job's `headless_jobs` row.
fn recovery_providers(db: &Path) -> Vec<String> {
    Connection::open(db)
        .unwrap()
        .prepare("SELECT provider FROM headless_jobs WHERE kind='recovery' ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// The arguments of each read-only job the stub ran: `codex exec --json`
/// in the read-only sandbox (the job profile that extends `:read-only`
/// and reaches only the queue service's socket, or `--sandbox
/// read-only`), with the role's model and effort, and nothing that writes
/// or bypasses it.
fn assert_read_only_calls(dir: &Path, calls: usize) {
    let args = fs::read_to_string(dir.join("codex-review-args.log")).unwrap_or_default();
    let lines: Vec<&str> = args.lines().collect();
    assert_eq!(lines.len(), calls, "{args}");
    for line in lines {
        assert!(line.starts_with(" exec| --json|"), "{line}");
        assert!(
            line.contains(r#" -c| permissions.dagq_job.extends=":read-only"|"#)
                || line.contains(" --sandbox| read-only|"),
            "{line}"
        );
        assert!(line.contains(" -m| gpt-6-astra|"), "{line}");
        assert!(
            line.contains(r#" -c| model_reasoning_effort="high"|"#),
            "{line}"
        );
        for refused in [
            "writable_roots",
            "workspace-write",
            "danger",
            "bypass",
            "--session-id",
        ] {
            assert!(!line.contains(refused), "{refused}: {line}");
        }
    }
}

/// Acceptance (1) and (2): the recovery job of a run that failed runs on
/// Codex, read-only, from the launch `triage_started` recorded; its
/// `retry` of high confidence passes the checks of a run that ended (its
/// branch holds no commit of its own) and is applied as a Claude job's
/// is, and the next run lands. The launch says Codex with no Claude
/// session id; the round's end, its `recovery_finished` and its span
/// carry the thread and the model; `headless_jobs`, `stats` and `kpi`
/// split the job by provider.
#[test]
fn a_codex_recovery_job_of_a_failed_run_is_read_only_and_its_retry_is_applied() {
    let (dir, repo, db) = fixture();
    select_codex_recovery(&repo);
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &fails_once(db.parent().unwrap()));
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let (_codex, options) = codex_options(dir.path(), &db, 4);
    codex_verdict(
        dir.path(),
        1,
        json!({"verdict": "repair", "confidence": "high",
               "diagnosis": "the session died on its own",
               "actions": [{"action": "retry"}]}),
    );
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2);
    let first = &detail.runs[0];
    assert_eq!(first.status(), RunStatus::Failed);
    assert_landed_run(&detail.runs[1], &repo, &base);
    assert!(reviewer.triage_prompts().is_empty(), "Claude ran no job");

    let started = &run_payloads(&detail, first, "triage_started")[0];
    assert_eq!(
        started["launch"],
        json!({"role": "recovery", "provider": "codex", "model": "gpt-6-astra",
               "effort": "high", "source": "dagq.toml"})
    );
    assert!(started["session_id"].is_null(), "Codex names its thread");
    let finished = &run_payloads(&detail, first, "triage_finished")[0];
    assert_eq!(finished["action"], "retry");
    assert_eq!(finished["verdict"], "repair");
    assert_eq!(finished["session_id"], THREAD);
    assert_eq!(finished["model"], MODEL);
    let recovered = &run_payloads(&detail, first, "recovery_finished")[0];
    assert_eq!(recovered["applied"], json!(["retry"]));
    assert_eq!(recovered["session_id"], THREAD);
    assert_eq!(recovered["model"], MODEL);
    let repaired = &run_payloads(&detail, first, "auto_repaired")[0];
    assert_eq!(repaired["layer"], "recovery");
    assert_eq!(repaired["conditions"]["own_commits"], false);
    // The round's span has the thread and the model, not a transcript.
    let spans: Vec<Value> = run_payloads(&detail, first, "session_closed")
        .into_iter()
        .filter(|span| span["kind"] == "triage")
        .collect();
    assert_eq!(spans.len(), 1, "{spans:?}");
    assert_eq!(spans[0]["session_id"], THREAD);
    assert_eq!(spans[0]["model"], MODEL);

    assert_read_only_calls(dir.path(), 1);
    let actors = fs::read_to_string(dir.path().join("codex-review-actors.log")).unwrap();
    assert_eq!(
        actors.trim(),
        format!("recovery-job recovery-job:{}:failed:1", first.id())
    );
    // The job's material is next to the run, its reply in its stdout.
    let run_dir = Path::new(first.run_dir().unwrap());
    assert!(run_dir.join("recovery-failed-1.prompt.txt").is_file());
    assert!(
        fs::read_to_string(run_dir.join("recovery-failed-1.out"))
            .unwrap()
            .contains("thread.started")
    );

    assert_eq!(recovery_providers(&db), ["codex"]);
    let jobs = &common::cli::ok(&db, &["stats", "--full"])["jobs"]["recovery"];
    assert_eq!(jobs["by_provider"]["codex"]["count"], 1, "{jobs}");
    assert_eq!(
        jobs["by_provider"]["codex"]["verdicts"]["repair"], 1,
        "{jobs}"
    );
    assert!(jobs["by_provider"].get("claude").is_none(), "{jobs}");
    assert_eq!(jobs["by_model"][MODEL]["count"], 1, "{jobs}");
    let kpi = common::cli::ok(&db, &["kpi", "--last", "1"]);
    let count = &kpi["periods"][0]["kpis"]["job.count.recovery"];
    assert_eq!(count["provider=codex"]["value"], 1.0, "{count}");
}

/// Acceptance (1): a Codex verdict is held to the checks of a Claude one
/// (the same `plan_ended`, whose cases are unit-tested): `stop_processes`,
/// which a run that ended does not take, is not applied, and the round
/// asks a person (`decide`) with the job's diagnosis; its end records the
/// job's thread and model. A `repair` of confidence `low` is the live
/// run's case below.
#[test]
fn a_codex_verdict_with_an_action_a_run_that_ended_does_not_take_is_asked() {
    let (dir, repo, db) = fixture();
    select_codex_recovery(&repo);
    let backend = TestWorkspace::new(&db, false, &fails_once(db.parent().unwrap()));
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "ok")]);
    let (_codex, options) = codex_options(dir.path(), &db, 4);
    codex_verdict(
        dir.path(),
        1,
        json!({"verdict": "repair", "confidence": "high", "diagnosis": "a test hangs",
               "actions": [{"action": "stop_processes", "pids": [1]}]}),
    );
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 1, "not retried");
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Failed);
    assert!(reviewer.triage_prompts().is_empty());
    let finished = &run_payloads(&detail, run, "triage_finished")[0];
    assert_eq!(finished["action"], "ask");
    assert_eq!(finished["session_id"], THREAD);
    assert!(
        finished["reason"]
            .as_str()
            .unwrap()
            .contains("stop_processes does not apply to a run that ended"),
        "{finished}"
    );
    let recovered = &run_payloads(&detail, run, "recovery_finished")[0];
    assert_eq!(recovered["applied"], json!([]));
    assert_eq!(recovered["escalated"], true);
    assert_eq!(recovered["model"], MODEL);
    assert!(run_payloads(&detail, run, "auto_repaired").is_empty());
    let asks = other_asks(&mut queue, false);
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::Decide);
    assert!(
        asks[0].question.contains("a test hangs"),
        "{}",
        asks[0].question
    );
    assert_eq!(recovery_providers(&db), ["codex"]);
}

/// Acceptance (3): a Codex recovery job whose turn failed is the round's
/// `triage_failed`, the attention a person recovers the run from by hand,
/// as a Claude job's failure is, and its end records the thread; no Claude
/// job starts.
#[test]
fn a_failed_codex_recovery_job_fails_its_round_to_a_person() {
    let (dir, repo, db) = fixture();
    select_codex_recovery(&repo);
    let backend = TestWorkspace::new(&db, false, &fails_once(db.parent().unwrap()));
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "ok")]);
    fs::write(
        dir.path().join("codex-review-failure.jsonl"),
        "{\"type\":\"turn.failed\",\"error\":{\"message\":\"recovery crashed\"}}\n",
    )
    .unwrap();
    fs::write(dir.path().join("codex-review-model"), MODEL).unwrap();
    let (_codex, options) = codex_options(dir.path(), &db, 4);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Failed);
    assert!(reviewer.triage_prompts().is_empty());
    let failed = run_payloads(&detail, run, "triage_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["code"], "job_failed");
    let error = failed[0]["error"].as_str().unwrap();
    assert!(error.contains("exited with"), "{error}");
    assert_eq!(failed[0]["session_id"], THREAD);
    assert!(failed[0].get("provider_unusable").is_none());
    let recovered = &run_payloads(&detail, run, "recovery_finished")[0];
    assert_eq!(recovered["outcome"], "job_failed");
    assert_eq!(recovered["reason_category"], "recovery_failed");
    assert_eq!(recovered["session_id"], THREAD);
    assert!(run_payloads(&detail, run, "triage_finished").is_empty());
    let jobs = &common::cli::ok(&db, &["stats", "--full"])["jobs"]["recovery"];
    assert_eq!(jobs["by_provider"]["codex"]["failed"], 1, "{jobs}");
}

/// Acceptance (3) and the description's (iv): a Codex recovery job past
/// the time limit fails its round as any failed job does, and is stopped
/// by pid with the command it ran in a process group of its own (tasks
/// 1085 and 1115).
#[test]
fn a_timed_out_codex_recovery_job_is_stopped_with_its_command() {
    let (dir, repo, db) = fixture();
    select_codex_recovery(&repo);
    let backend = TestWorkspace::new(&db, false, &fails_once(db.parent().unwrap()));
    let mut reviewer = TestReviewer::new(&[verdict("pass", &[], "ok")]);
    reviewer.timeout = Duration::from_secs(2);
    fs::write(dir.path().join("codex-review-hang"), "").unwrap();
    let pid_file = dir.path().join("codex-review-child.pid");
    // Even a failing assertion or the time limit's exit must not leave the
    // stub's command running (it also ends once this test is gone).
    let _command = StubCommand::guard(&pid_file);
    let (_codex, options) = codex_options(dir.path(), &db, 4);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Failed);
    let failed = run_payloads(&detail, run, "triage_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    let error = failed[0]["error"].as_str().unwrap();
    assert!(error.contains("did not finish within 2 seconds"), "{error}");
    assert_eq!(failed[0]["session_id"], THREAD);
    let child = stub_command_pid(&pid_file)
        .expect("the command records its pid after entering a separate process group");
    // A zombie not reaped yet is gone too (`running` reads `ps`'s state).
    let deadline = Instant::now() + Duration::from_secs(5);
    while running(child) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!running(child), "the job's command {child} outlived it");
    // The command is gone: disarm the guard so it cannot hit a reused pid.
    fs::remove_file(&pid_file).unwrap();
}

/// The pid of the stub's command that `pid_file` names, if it wrote one.
fn stub_command_pid(pid_file: &Path) -> Option<u32> {
    fs::read_to_string(pid_file)
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Kills the stub's command named in a pid file when the test ends, and
/// when the test's time limit exits the process, which runs no `Drop`
/// (`common::on_timeout`).
struct StubCommand {
    pid_file: PathBuf,
    _on_timeout: common::Cleanup,
}

impl StubCommand {
    fn guard(pid_file: &Path) -> Self {
        let file = pid_file.to_owned();
        let on_timeout = common::on_timeout(common::STEP_LIMIT, "the stub's command", move || {
            Self::kill(&file)
        });
        Self {
            pid_file: pid_file.to_owned(),
            _on_timeout: on_timeout,
        }
    }

    fn kill(pid_file: &Path) {
        if let Some(pid) = stub_command_pid(pid_file) {
            // SAFETY: kill takes no pointers; this is the stub's command.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
    }
}

impl Drop for StubCommand {
    fn drop(&mut self) {
        Self::kill(&self.pid_file);
    }
}

/// The fixture's Codex task, failing at its first turn, under a
/// `--no-claude` supervisor with `[roles.recovery]` on Codex; its job's
/// reply is `prepare`'s. No Claude job runs. Returns the fixture (which
/// holds the queue), the queue and the run's task detail.
fn no_claude_recovery(prepare: impl FnOnce(&Path)) -> (Fixture, PathBuf, dagq::domain::TaskDetail) {
    let (dir, repo, db, backend, codex) = crate::runtime_codex::codex_fixture();
    select_codex_recovery(&repo);
    set_turns(dir.path(), "exit 1");
    prepare(dir.path());
    let reviewer = TestReviewer::new(&[]);
    let options = SuperviseOptions {
        no_claude: true,
        codex,
        codex_home: Some(dir.path().join(headless::CODEX_HOME)),
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
    let detail = crate::runtime_codex::detail(&db);
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    assert!(reviewer.triage_prompts().is_empty(), "no Claude job");
    (dir, db, detail)
}

/// Acceptance (3) under `--no-claude` (ADR-t1204-1): the recovery job of a
/// Codex run that failed starts on Codex rather than going to a person,
/// and its `wait` is applied. Its thread wrote no rollout, so its end
/// records the thread and why its model is unknown.
#[test]
fn a_no_claude_supervisor_runs_the_codex_recovery_job() {
    let (_dir, db, detail) = no_claude_recovery(|dir| {
        codex_reply(
            dir,
            1,
            json!({"verdict": "repair", "confidence": "high",
                   "diagnosis": "the provider was down for a moment",
                   "actions": [{"action": "wait", "recheck_after_secs": 3600}]}),
        )
    });
    let run = &detail.runs[0];
    let started = run_payloads(&detail, run, "triage_started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["launch"]["provider"], "codex");
    assert_eq!(recovery_providers(&db), ["codex"]);
    let finished = &run_payloads(&detail, run, "triage_finished")[0];
    assert_eq!(finished["action"], "wait");
    let recovered = &run_payloads(&detail, run, "recovery_finished")[0];
    for end in [finished, recovered] {
        assert_eq!(end["session_id"], THREAD, "{end}");
        assert!(end["model"].is_null(), "{end}");
        assert!(end["model_unknown"].is_string(), "{end}");
    }
    assert!(run_payloads(&detail, run, "triage_failed").is_empty());
}

/// Acceptance (3) under `--no-claude`: a Codex job that fails at its login
/// holds Codex; its round is due again, and with no provider left the
/// next round starts no job and fails to a person told why. No Claude job
/// is started.
#[test]
fn a_no_claude_codex_recovery_job_that_cannot_log_in_goes_to_a_person_never_to_claude() {
    let (_dir, db, detail) = no_claude_recovery(|dir| {
        fs::write(dir.join("codex-review-failure.jsonl"), CODEX_AUTH_FAILURE).unwrap()
    });
    let run = &detail.runs[0];
    let started = run_payloads(&detail, run, "triage_started");
    assert_eq!(started.len(), 2, "{started:?}");
    for start in &started {
        assert_eq!(start["launch"]["provider"], "codex");
    }
    assert_eq!(recovery_providers(&db), ["codex"], "one job ran");
    let failed = run_payloads(&detail, run, "triage_failed");
    assert_eq!(failed.len(), 2, "{failed:?}");
    assert_eq!(
        failed[0]["provider_unusable"],
        json!({"provider": "codex", "reason": "authentication"})
    );
    assert!(
        !failed[0]["error"].as_str().unwrap().contains("Claude"),
        "{}",
        failed[0]
    );
    // The next round: no provider left, a person told why.
    assert!(failed[1].get("provider_unusable").is_none());
    let error = failed[1]["error"].as_str().unwrap();
    assert!(
        error.contains("provider_disabled")
            && error.contains("codex cannot be used (authentication)"),
        "{error}"
    );
    let held = queue_events(&db, "provider_held");
    assert!(
        held.iter()
            .any(|h| h["provider"] == "codex" && h["reason"] == "authentication"),
        "{held:?}"
    );
}

/// Turns that end with neither a receipt nor a question, so the run is
/// nudged and then goes to its recovery job (`stalled`, reason
/// `turn_without_receipt`), until one whose prompt matches the shell
/// pattern `finish`, which finishes the task.
fn thinking_until(finish: &str) -> String {
    format!(
        r#"case "$PROMPT" in
{finish}) {FINISH} ;;
*) say thinking ;;
esac"#,
        FINISH = headless::FINISH
    )
}

/// Supervise the headless fixture's task on a thread with `options`, a
/// review that passes and the Claude recovery jobs of `claude_jobs`.
fn supervise_live(
    db: &Path,
    repo: &Path,
    backend: Arc<TestWorkspace>,
    options: SuperviseOptions,
    claude_jobs: &[String],
) -> (Arc<TestReviewer>, thread::JoinHandle<Result<Value>>) {
    let reviewer =
        Arc::new(TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(claude_jobs));
    let (db, repo, judge) = (db.to_owned(), repo.to_owned(), reviewer.clone());
    let supervisor = thread::spawn(move || {
        runtime::supervise_with_reviewer(
            &db,
            &repo,
            &*backend,
            &claude_stub(&db),
            &*judge,
            Path::new(env!("CARGO_BIN_EXE_dagq")),
            &options,
        )
    });
    (reviewer, supervisor)
}

/// Acceptance (1) and (2) for a live run: the recovery job of a headless
/// run that stalled runs on Codex, read-only, with the role's launch in
/// its `recovery_requested`; its `resume` of high confidence passes the
/// live checks and parks the run, which is resumed and lands. Its
/// `recovery_finished` carries the thread and the model, and the job is
/// Codex's in `headless_jobs` and `stats`.
#[test]
fn a_codex_recovery_job_of_a_stalled_run_resumes_it_and_it_lands() {
    let (dir, repo, db, backend) = headless::headless_fixture(&[]);
    select_codex_recovery(&repo);
    // The stub `codex` resets the turns it shares with the stub `claude`.
    let (_codex, options) = codex_options(dir.path(), &db, 4);
    set_turns(
        dir.path(),
        &thinking_until(r#"*"restart the hung tests in a fresh session"*"#),
    );
    codex_verdict(
        dir.path(),
        1,
        json!({"verdict": "repair", "confidence": "high",
               "diagnosis": "the session hangs on its tests",
               "actions": [{"action": "resume", "instruction": "restart the hung tests in a fresh session"}]}),
    );
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let (reviewer, supervisor) = supervise_live(&db, &repo, backend.clone(), options, &[]);
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = headless::detail(&db);
    assert_landed_run(&detail.runs[0], &repo, &base);
    assert!(reviewer.triage_prompts().is_empty(), "Claude ran no job");
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["alert"], "stalled");
    assert_eq!(requested[0]["launch"]["provider"], "codex");
    assert_eq!(requested[0]["launch"]["model"], "gpt-6-astra");
    let finished = payloads(&detail, "recovery_finished");
    let applied: Vec<&Value> = finished
        .iter()
        .copied()
        .filter(|f| f["applied"] == json!(["resume"]))
        .collect();
    assert_eq!(applied.len(), 1, "{finished:?}");
    assert_eq!(applied[0]["session_id"], THREAD);
    assert_eq!(applied[0]["model"], MODEL);
    let repaired = payloads(&detail, "auto_repaired");
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert_eq!(repaired[0]["repair"], "resume");
    assert_eq!(payloads(&detail, "recovery_parked").len(), 1);
    assert!(stalled_asks(&SqliteQueue::open(&db).unwrap()).is_empty());
    assert_read_only_calls(dir.path(), 1);
    let actors = fs::read_to_string(dir.path().join("codex-review-actors.log")).unwrap();
    assert_eq!(
        actors.trim(),
        format!(
            "recovery-job recovery-job:{}:stalled:1",
            detail.runs[0].id()
        )
    );
    assert_eq!(recovery_providers(&db), ["codex"]);
    let jobs = &common::cli::ok(&db, &["stats", "--full"])["jobs"]["recovery"];
    assert_eq!(
        jobs["by_provider"]["codex"]["verdicts"]["repair"], 1,
        "{jobs}"
    );
    assert_eq!(jobs["by_model"][MODEL]["count"], 1, "{jobs}");
}

/// A headless run that stalls (its turns end with neither a receipt nor a
/// question) gets its recovery job on Codex, which answers `verdict`, or
/// fails when it is `None`. The job's verdict is not applied: the
/// `stalled` ask opens with `recovery_failed`, nothing of the job's is sent
/// to the session, and the job's `recovery_finished` records its thread
/// and model; `stop` ends the run. Returns the ask.
fn live_escalation(verdict: Option<Value>) -> dagq::domain::Ask {
    let (dir, repo, db, backend) = headless::headless_fixture(&[]);
    select_codex_recovery(&repo);
    let (_codex, options) = codex_options(dir.path(), &db, 4);
    set_turns(dir.path(), &thinking_until(r#""answer to ask "*"#));
    let failed = verdict.is_none();
    match verdict {
        Some(verdict) => codex_verdict(dir.path(), 1, verdict),
        None => {
            fs::write(
                dir.path().join("codex-review-failure.jsonl"),
                "{\"type\":\"turn.failed\",\"error\":{\"message\":\"recovery crashed\"}}\n",
            )
            .unwrap();
            fs::write(dir.path().join("codex-review-model"), MODEL).unwrap();
        }
    }
    let backend = Arc::new(backend);
    let (reviewer, supervisor) = supervise_live(&db, &repo, backend.clone(), options, &[]);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !stalled_asks(queue).is_empty()
            && !payloads(&queue.show(headless::TASK).unwrap(), "recovery_finished").is_empty()
    });
    let ask = stalled_asks(&SqliteQueue::open(&db).unwrap()).remove(0);
    assert_eq!(ask.reason_category, AskReason::RecoveryFailed, "{ask:?}");
    let asked = headless::detail(&db);
    let finished = payloads(&asked, "recovery_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["escalated"], true);
    assert_eq!(finished[0]["applied"], json!([]));
    assert_eq!(finished[0]["session_id"], THREAD);
    assert_eq!(finished[0]["model"], MODEL);
    assert_eq!(finished[0]["outcome"] == "job_failed", failed);
    assert!(payloads(&asked, "auto_repaired").is_empty());
    // Only the nudges were sent: no instruction of the job's.
    assert!(
        payloads(&asked, "turn_requested")
            .iter()
            .all(|turn| turn["what"] != "recovery instruction")
    );
    assert_eq!(
        payloads(&asked, "recovery_requested")[0]["launch"]["provider"],
        "codex"
    );
    assert!(reviewer.triage_prompts().is_empty());

    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "stop")
        .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(headless::detail(&db).runs[0].status(), RunStatus::Failed);
    ask
}

/// Acceptance (1) for a live run: a Codex `repair` of confidence `low` is
/// not applied but asked, with its actions as the recommendation.
#[test]
fn a_codex_repair_of_low_confidence_for_a_stalled_run_is_asked() {
    let ask = live_escalation(Some(json!({
        "verdict": "repair", "confidence": "low", "diagnosis": "maybe a missing permission",
        "actions": [{"action": "send_instruction", "instruction": "go on"}],
    })));
    for part in [
        "confidence low",
        "Recommended: [{\"action\":\"send_instruction\"",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
}

/// Acceptance (3) for a live run: a Codex recovery job that failed is the
/// alert's `recovery_failed` ask (ADR-t609-1), as a Claude job's failure
/// is, and no Claude job starts.
#[test]
fn a_failed_codex_recovery_job_of_a_stalled_run_asks_a_person() {
    let ask = live_escalation(None);
    assert!(
        ask.question.contains("the recovery job failed"),
        "{}",
        ask.question
    );
}

/// Acceptance (3) under `--no-claude` for a live run: the stalled alert of
/// a Codex run comes while Codex is held (`authentication`), so with no
/// provider left its recovery job starts on neither Codex nor Claude, and
/// the `stalled` ask (`recovery_failed`) tells a person why. The run's
/// first turn waits until the hold is recorded, so that the alert comes
/// after it.
#[test]
fn a_no_claude_live_run_with_codex_held_asks_a_person_and_starts_no_recovery_job() {
    let (dir, repo, db, backend, codex) = crate::runtime_codex::codex_fixture();
    select_codex_recovery(&repo);
    let gate = dir.path().join("codex-held");
    set_turns(
        dir.path(),
        &format!(
            "i=0; while [ ! -f {gate} ] && [ $i -lt 1200 ]; do sleep 0.05; i=$((i + 1)); done; say thinking",
            gate = shell_path(&gate)
        ),
    );
    let options = SuperviseOptions {
        no_claude: true,
        codex: codex.clone(),
        codex_home: Some(dir.path().join(headless::CODEX_HOME)),
        ..supervise_options(1, true)
    };
    let reviewer = Arc::new(TestReviewer::new(&[]));
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend, judge) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        let claude = dir.path().join("missing-claude");
        thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude,
                &*judge,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
        })
    };
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .show(crate::runtime_codex::TASK)
            .unwrap()
            .runs
            .first()
            .is_some_and(|run| run.status() == RunStatus::Running)
    });
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let hold = ProviderHold::new(Provider::Codex, SwitchReason::Authentication, now);
    RunLog::record_queue_event(
        &SqliteQueue::open(&db).unwrap(),
        EventKind::ProviderHeld,
        hold.held_payload(None),
    )
    .unwrap();
    fs::write(&gate, "").unwrap();
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !stalled_asks(queue).is_empty()
    });
    let ask = stalled_asks(&SqliteQueue::open(&db).unwrap()).remove(0);
    assert_eq!(ask.reason_category, AskReason::RecoveryFailed, "{ask:?}");
    assert!(
        ask.question.contains(
            "provider_disabled: Claude is disabled by --no-claude and codex cannot be used (authentication)"
        ),
        "{}",
        ask.question
    );
    let asked = crate::runtime_codex::detail(&db);
    let requested = payloads(&asked, "recovery_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["alert"], "stalled");
    assert_eq!(requested[0]["launch"]["provider"], "codex");
    assert!(recovery_providers(&db).is_empty(), "no recovery job ran");
    assert!(
        !dir.path().join("codex-review-args.log").exists(),
        "Codex ran no job"
    );
    assert!(reviewer.triage_prompts().is_empty(), "no Claude job");

    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "stop")
        .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(
        crate::runtime_codex::detail(&db).runs[0].status(),
        RunStatus::Failed
    );
}

/// Acceptance (3) and ADR-t1063-1 decision 4: a Codex recovery job of a
/// run that ended that fails at its login holds Codex and is no person's
/// (`triage_failed` with `provider_unusable`, no `triage by hand`): the
/// next round starts on Claude, whose launch says from which provider and
/// why, and its `retry` lands the task.
#[test]
fn a_codex_recovery_job_that_cannot_log_in_moves_the_round_to_claude() {
    let (dir, repo, db) = fixture();
    select_codex_recovery(&repo);
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &fails_once(db.parent().unwrap()));
    let reviewer =
        TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]).with_triages(&[repair(
            json!({"action": "retry"}),
            "the session died on its own",
        )]);
    fs::write(
        dir.path().join("codex-review-failure.jsonl"),
        CODEX_AUTH_FAILURE,
    )
    .unwrap();
    let (_codex, options) = codex_options(dir.path(), &db, 4);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.runs.len(), 2);
    let first = &detail.runs[0];
    assert_landed_run(&detail.runs[1], &repo, &base);
    assert_eq!(
        reviewer.triage_prompts().len(),
        1,
        "Claude ran the next round"
    );
    let started = run_payloads(&detail, first, "triage_started");
    assert_eq!(started.len(), 2, "{started:?}");
    assert_eq!(started[0]["launch"]["provider"], "codex");
    assert_eq!(started[1]["launch"]["provider"], "claude");
    assert_eq!(started[1]["launch"]["switched_from"], "codex");
    assert_eq!(started[1]["launch"]["switch_reason"], "authentication");
    let failed = run_payloads(&detail, first, "triage_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(
        failed[0]["provider_unusable"],
        json!({"provider": "codex", "reason": "authentication"})
    );
    assert_eq!(failed[0]["session_id"], THREAD);
    assert_eq!(
        run_payloads(&detail, first, "triage_finished")[0]["action"],
        "retry"
    );
    let held = queue_events(&db, "provider_held");
    assert!(
        held.iter()
            .any(|h| h["provider"] == "codex" && h["reason"] == "authentication"),
        "{held:?}"
    );
    assert_eq!(recovery_providers(&db), ["codex", "claude"]);
    let jobs = &common::cli::ok(&db, &["stats", "--full"])["jobs"]["recovery"];
    assert_eq!(jobs["by_provider"]["codex"]["failed"], 1, "{jobs}");
    assert_eq!(
        jobs["by_provider"]["claude"]["verdicts"]["repair"], 1,
        "{jobs}"
    );
    // The failed Codex round was no person's.
    let events = dagq::compose::events(&db, dagq::domain::EventId::new(0), 1000, false).unwrap();
    assert!(!events.to_string().contains("triage by hand"), "{events}");
}

/// Acceptance (3) and ADR-t1063-1 decision 4 for a live run: a Codex
/// recovery job that fails at its login holds Codex and opens no ask; the
/// alert's next job starts on Claude, whose `resume` parks the run, which
/// is resumed and lands.
#[test]
fn a_live_codex_recovery_job_that_cannot_log_in_moves_to_claude() {
    let (dir, repo, db, backend) = headless::headless_fixture(&[]);
    select_codex_recovery(&repo);
    let (_codex, options) = codex_options(dir.path(), &db, 4);
    set_turns(
        dir.path(),
        &thinking_until(r#"*"restart the hung tests in a fresh session"*"#),
    );
    fs::write(
        dir.path().join("codex-review-failure.jsonl"),
        CODEX_AUTH_FAILURE,
    )
    .unwrap();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let (reviewer, supervisor) = supervise_live(
        &db,
        &repo,
        backend.clone(),
        options,
        &[repair(
            json!({"action": "resume", "instruction": "restart the hung tests in a fresh session"}),
            "the session hangs on its tests",
        )],
    );
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = headless::detail(&db);
    assert_landed_run(&detail.runs[0], &repo, &base);
    assert_eq!(
        reviewer.triage_prompts().len(),
        1,
        "Claude ran the next job"
    );
    assert!(stalled_asks(&SqliteQueue::open(&db).unwrap()).is_empty());
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 2, "{requested:?}");
    assert_eq!(requested[0]["launch"]["provider"], "codex");
    assert_eq!(requested[1]["launch"]["provider"], "claude");
    assert_eq!(requested[1]["launch"]["switched_from"], "codex");
    let finished = payloads(&detail, "recovery_finished");
    let failed: Vec<&Value> = finished
        .iter()
        .copied()
        .filter(|f| f["outcome"] == "job_failed")
        .collect();
    assert_eq!(failed.len(), 1, "{finished:?}");
    assert_eq!(failed[0]["escalated"], false);
    assert_eq!(failed[0]["session_id"], THREAD);
    assert_eq!(
        failed[0]["provider_unusable"],
        json!({"provider": "codex", "reason": "authentication"})
    );
    assert!(
        finished.iter().any(|f| f["applied"] == json!(["resume"])),
        "{finished:?}"
    );
    assert_eq!(recovery_providers(&db), ["codex", "claude"]);
}

/// What a Codex job stopped at its usage limit prints.
const CODEX_LIMIT_FAILURE: &str = "{\"type\":\"error\",\"message\":\"You have hit your usage limit. Try again later.\"}\n{\"type\":\"turn.failed\",\"error\":{\"message\":\"You have hit your usage limit.\"}}\n";

/// With `[provider_fallback] jobs = false` (ADR-t1857-1), a Codex recovery
/// job of a run that ended that stops at the usage limit holds Codex and
/// is no person's, but its next round does not move to Claude: nothing
/// starts while Codex is held, and once the hold ends the round starts on
/// Codex again, whose `retry` lands the task.
#[test]
fn with_the_fallback_off_a_codex_recovery_job_at_its_limit_waits_and_retries_codex() {
    let (dir, repo, db) = fixture();
    fs::write(
        repo.join("dagq.toml"),
        "[roles.recovery]\nprovider = 'codex'\nmodel = 'gpt-6-astra'\neffort = 'high'\n\n[provider_fallback]\njobs = false\n",
    )
    .unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(
        &repo,
        &["commit", "-m", "select Codex for recovery, no fallback"],
    );
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &fails_once(db.parent().unwrap()));
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")])
        .with_triages(&[repair(json!({"action": "retry"}), "Claude must not run")]);
    let failure = dir.path().join("codex-review-failure.jsonl");
    fs::write(&failure, CODEX_LIMIT_FAILURE).unwrap();
    let (_codex, options) = codex_options(dir.path(), &db, 4);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.runs.len(), 1);
    let first = detail.runs[0].clone();
    assert_eq!(first.status(), RunStatus::Failed);
    let failed = run_payloads(&detail, &first, "triage_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(
        failed[0]["provider_unusable"],
        json!({"provider": "codex", "reason": "usage_limit"})
    );
    let held = queue_events(&db, "provider_held");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["provider"], "codex");
    assert_eq!(held[0]["reason"], "usage_limit");
    assert!(reviewer.triage_prompts().is_empty(), "Claude ran no job");
    assert_eq!(recovery_providers(&db), ["codex"]);
    // A pass while Codex is held starts nothing: not on Claude, not on
    // Codex.
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(run_payloads(&detail, &first, "triage_started").len(), 1);
    assert!(reviewer.triage_prompts().is_empty(), "Claude ran no job");
    assert_eq!(recovery_providers(&db), ["codex"]);
    // Codex's hold ends (its time is up), and Codex can be used again.
    fs::remove_file(&failure).unwrap();
    codex_verdict(
        dir.path(),
        2,
        json!({"verdict": "repair", "confidence": "high",
               "diagnosis": "the session died on its own",
               "actions": [{"action": "retry"}]}),
    );
    SqliteQueue::open(&db)
        .unwrap()
        .record_queue_event(
            EventKind::ProviderReleased,
            json!({"provider": "codex", "reason": "usage_limit",
                   "since": held[0]["since"], "why": "retry_due"}),
        )
        .unwrap();
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.runs.len(), 2);
    assert_landed_run(&detail.runs[1], &repo, &base);
    let started = run_payloads(&detail, &first, "triage_started");
    assert_eq!(started.len(), 2, "{started:?}");
    for start in &started {
        assert_eq!(start["launch"]["provider"], "codex", "{start}");
        assert!(start["launch"].get("switched_from").is_none(), "{start}");
    }
    assert_eq!(
        run_payloads(&detail, &first, "triage_finished")[0]["action"],
        "retry"
    );
    assert!(reviewer.triage_prompts().is_empty(), "Claude ran no job");
    assert_eq!(recovery_providers(&db), ["codex", "codex"]);
}

/// The payloads of the queue's events of `kind`, oldest first.
fn queue_events(db: &Path, kind: &str) -> Vec<Value> {
    let connection = Connection::open(db).unwrap();
    let mut statement = connection
        .prepare("SELECT payload FROM run_events WHERE kind=?1 ORDER BY id")
        .unwrap();
    statement
        .query_map([kind], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
        .collect()
}
