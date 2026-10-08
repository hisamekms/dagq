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
use dagq::{application::RunFiles, runtime::RunFilesPort};
use runtime_support::*;
use std::io;

pub(crate) const TASK: TaskId = TaskId::new(2);

/// A turn that commits and writes a receipt naming the new head.
pub(crate) const FINISH: &str = r#"commit work; receipt "$(git rev-parse HEAD)"; say finished"#;

/// The fixture's task, canceled, and in its place task 2 (`test task`) for
/// a Codex worker; the backend runs its turns with the stub `codex` of
/// [`headless_codex`], which the supervisor is given too (so that it
/// claims the task).
pub(crate) fn codex_fixture() -> (Fixture, PathBuf, PathBuf, TestWorkspace, PathBuf) {
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
            wait_for_build: false,
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

pub(crate) fn detail(db: &Path) -> dagq::domain::TaskDetail {
    SqliteQueue::open(db).unwrap().show(TASK).unwrap()
}

/// Supervise with the stub `codex` on a thread, with reviews of
/// `verdicts` in order.
pub(crate) fn supervise_thread(
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
pub(crate) fn finished(
    backend: &TestWorkspace,
    supervisor: thread::JoinHandle<Result<Value>>,
) -> Value {
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    outcome
}

/// The one open ask of `kind`'s reason once it opened.
pub(crate) fn open_ask(
    db: &Path,
    matches: impl Fn(&dagq::domain::Ask) -> bool,
) -> dagq::domain::Ask {
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

/// The supervisor returned no error and its one run landed. Either message
/// carries the result and the task's events, one line each (time, kind,
/// payload), so that a run that ends otherwise under load shows what its
/// review and landing did (task 1584).
#[track_caller]
fn assert_integrated(result: &Value, db: &Path) {
    let events = || {
        detail(db)
            .events
            .iter()
            .map(|event| format!("{} {} {}", event.created_at, event.kind, event.payload))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(result["errors"], json!([]), "{result}\n{}", events());
    assert_eq!(
        result["runs"][0]["status"],
        "integrated",
        "{result}\n{}",
        events()
    );
}

/// The person's `~/.codex/config.toml`, which no run may change
/// (ADR-t813-3 decision 6); `None` when there is none.
fn codex_config() -> Option<Vec<u8>> {
    let home = std::env::var_os("HOME")?;
    fs::read(Path::new(&home).join(".codex/config.toml")).ok()
}

/// What a Codex review job at its usage limit prints.
const USAGE_LIMIT_REVIEW: &str = "{\"type\":\"error\",\"message\":\"unexpected status 429 Too Many Requests: You have hit your usage limit. Try again at 3pm.\"}\n";

fn select_codex_review(repo: &Path) {
    fs::write(
        repo.join("dagq.toml"),
        "[roles.review]\nprovider = 'codex'\n",
    )
    .unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-m", "select Codex for review"]);
}

fn codex_review_reply(dir: &Path, call: usize, verdict: Value) {
    let reply = json!({"type":"item.completed","item":{"id":"review","type":"agent_message","text":verdict.to_string()}});
    fs::write(
        dir.join(format!("codex-review-{call}.jsonl")),
        format!("{reply}\n"),
    )
    .unwrap();
}

/// The thread the stub `codex` names for a review.
const REVIEW_THREAD: &str = "codex-review-thread";

/// The model the stub `codex` writes to a review's rollout.
const REVIEW_MODEL: &str = "gpt-test-review";

/// Have the stub `codex` write [`REVIEW_MODEL`] to each review's rollout.
fn set_codex_review_model(dir: &Path) {
    fs::write(dir.join("codex-review-model"), REVIEW_MODEL).unwrap();
}

/// The `session_closed` of the run's review spans.
fn review_spans(detail: &dagq::domain::TaskDetail) -> Vec<&Value> {
    payloads(detail, "session_closed")
        .into_iter()
        .filter(|span| span["kind"] == "review")
        .collect()
}

/// The end of a Codex review with no rollout to read its model from: its
/// thread, no model and why (ADR-t1063-1 decision 6), on the end's event
/// and on its span.
fn assert_thread_without_model(end: &Value, span: &Value) {
    for recorded in [end, span] {
        assert_eq!(recorded["session_id"], REVIEW_THREAD, "{recorded}");
        assert!(recorded["model"].is_null(), "{recorded}");
        assert!(recorded["model_unknown"].is_string(), "{recorded}");
    }
}

/// A Codex review's end records its thread and the model of its rollout
/// as a goal or plan review's does (ADR-t1063-1 decision 6): its
/// `review_finished`, its span's `session_closed` and `stats`'s jobs by
/// model.
#[test]
fn no_claude_run_uses_a_read_only_codex_review_and_lands() {
    let (dir, repo, db, backend, codex) = codex_fixture();
    select_codex_review(&repo);
    set_turns(dir.path(), FINISH);
    set_codex_review_model(dir.path());
    let mut options = supervise_options(1, true);
    options.codex = codex;
    options.no_claude = true;
    options.codex_home = Some(dir.path().join(CODEX_HOME));
    let reviewer = TestReviewer::new(&[]);
    let result = runtime::supervise_with_reviewer(
        &db,
        &repo,
        &backend,
        &claude_stub(&db),
        &reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options,
    )
    .unwrap();
    backend.join();
    assert_eq!(result["errors"], json!([]), "{result}");
    let detail = detail(&db);
    assert_eq!(
        result["runs"][0]["status"],
        "integrated",
        "{result}; review failed: {:?}",
        payloads(&detail, "review_failed")
    );
    assert_eq!(
        payloads(&detail, "review_started")[0]["launch"]["provider"],
        "codex"
    );
    let finished = &payloads(&detail, "review_finished")[0];
    assert_eq!(finished["verdict"], "pass");
    assert_eq!(finished["session_id"], REVIEW_THREAD);
    assert_eq!(finished["model"], REVIEW_MODEL);
    assert!(finished.get("model_unknown").is_none(), "{finished}");
    assert!(payloads(&detail, "review_failed").is_empty());
    let spans = review_spans(&detail);
    assert_eq!(spans.len(), 1, "{spans:?}");
    assert_eq!(spans[0]["session_id"], REVIEW_THREAD);
    assert_eq!(spans[0]["model"], REVIEW_MODEL);
    let jobs = &common::cli::ok(&db, &["stats", "--full"])["jobs"]["review"];
    assert_eq!(
        jobs["by_provider"]["codex"]["verdicts"]["pass"], 1,
        "{jobs}"
    );
    assert_eq!(jobs["by_model"][REVIEW_MODEL]["count"], 1, "{jobs}");
    assert!(jobs["by_model"].get("unknown").is_none(), "{jobs}");
    let args = fs::read_to_string(dir.path().join("codex-review-args.log")).unwrap();
    // Read-only, in the profile that lets its `dagq` reach the queue
    // service's socket in place of `--sandbox read-only` (ADR-t1233-5
    // decision 4).
    assert!(
        args.contains(r#" -c| permissions.dagq_job.extends=":read-only"|"#),
        "{args}"
    );
    assert!(!args.contains("--sandbox|"), "{args}");
    // The worktree is untrusted, so that the worker's `.codex` does not
    // reach its review (ADR-t1570-1).
    assert!(args.contains(r#"={trust_level="untrusted"}}|"#), "{args}");
    assert!(!args.contains(r#"trust_level="trusted""#), "{args}");
    let actors = fs::read_to_string(dir.path().join("codex-review-actors.log")).unwrap();
    assert!(actors.contains("review-job review-job:"), "{actors}");
}

#[test]
fn no_claude_codex_review_revise_reaches_the_worker_and_passes() {
    let (dir, repo, db, backend, codex) = codex_fixture();
    select_codex_review(&repo);
    set_turns(
        dir.path(),
        "case \"$TURN\" in 1) commit work; receipt \"$(git rev-parse HEAD)\"; say first ;; *) commit fixed; receipt \"$(git rev-parse HEAD)\"; say fixed ;; esac",
    );
    codex_review_reply(
        dir.path(),
        1,
        json!({"verdict":"revise","reasons":[{"text":"fix the detail","codes":["code_defect"]}],"summary":"revise"}),
    );
    codex_review_reply(
        dir.path(),
        2,
        json!({"verdict":"pass","reasons":[],"summary":"fixed"}),
    );
    let mut options = supervise_options(1, true);
    options.codex = codex;
    options.no_claude = true;
    options.codex_home = Some(dir.path().join(CODEX_HOME));
    let reviewer = TestReviewer::new(&[]);
    let result = runtime::supervise_with_reviewer(
        &db,
        &repo,
        &backend,
        &claude_stub(&db),
        &reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options,
    )
    .unwrap();
    backend.join();
    assert_integrated(&result, &db);
    let detail = detail(&db);
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished.len(), 2, "{finished:?}");
    assert_eq!(finished[0]["verdict"], "revise");
    assert_eq!(finished[0]["primary_code"], "code_defect");
    assert_eq!(finished[1]["verdict"], "pass");
    assert_eq!(payloads(&detail, "revise_requested").len(), 1);
    assert!(payloads(&detail, "review_failed").is_empty());
}

/// Under `--no-claude` a Codex review whose job fails (a general failure,
/// not one that holds Codex) is reviewed once more on Codex (task 1984);
/// when that fails too it asks a person, and Claude never starts.
#[test]
fn no_claude_codex_review_failure_asks_without_starting_claude() {
    let (dir, repo, db, backend, codex) = codex_fixture();
    select_codex_review(&repo);
    set_turns(dir.path(), FINISH);
    set_codex_review_model(dir.path());
    fs::write(
        dir.path().join("codex-review-failure.jsonl"),
        "{\"type\":\"turn.failed\",\"error\":{\"message\":\"review crashed\"}}\n",
    )
    .unwrap();
    let mut options = supervise_options(1, true);
    options.codex = codex;
    options.no_claude = true;
    options.codex_home = Some(dir.path().join(CODEX_HOME));
    let reviewer = TestReviewer::new(&[]);
    let result = runtime::supervise_with_reviewer(
        &db,
        &repo,
        &backend,
        &claude_stub(&db),
        &reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options,
    )
    .unwrap();
    backend.join();
    assert_eq!(result["errors"], json!([]), "{result}");
    assert_eq!(result["runs"][0]["status"], "awaiting_integration");
    let detail = detail(&db);
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["code"], "job_failed");
    assert!(!failed[0]["error"].as_str().unwrap().contains("Claude"));
    // The failed review's thread and model, on its end and its span, which
    // closed when the job ended.
    assert_eq!(failed[0]["session_id"], REVIEW_THREAD);
    assert_eq!(failed[0]["model"], REVIEW_MODEL);
    assert_eq!(failed[0]["attempt"], 2);
    let retried = payloads(&detail, "review_retried");
    assert_eq!(retried.len(), 1, "{retried:?}");
    assert_eq!(retried[0]["cause"], "job_failed");
    assert_eq!(retried[0]["session_id"], REVIEW_THREAD);
    let spans = review_spans(&detail);
    assert_eq!(spans.len(), 2, "{spans:?}");
    for span in &spans {
        assert_eq!(span["session_id"], REVIEW_THREAD);
        assert_eq!(span["model"], REVIEW_MODEL);
    }
    let jobs = &common::cli::ok(&db, &["stats", "--full"])["jobs"]["review"];
    // The stats count the review's end, as before the retry.
    assert_eq!(jobs["by_model"][REVIEW_MODEL]["failed"], 1, "{jobs}");
    let started = payloads(&detail, "review_started");
    assert_eq!(started.len(), 2);
    for started in started {
        assert_eq!(started["launch"]["provider"], "codex");
    }
    assert!(reviewer.prompts().is_empty());
    assert_eq!(payloads(&detail, "ask_opened").len(), 1);
    assert!(payloads(&detail, "review_finished").is_empty());
}

/// A Codex that passed the supervisor's preflight but does not start for
/// the review, while Claude may run: Codex is held and the review starts
/// again on Claude (ADR-t1063-1 decisions 4 and 5); the run lands.
#[test]
fn a_codex_review_whose_codex_does_not_start_moves_to_claude() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, repo, db) = fixture();
    select_codex_review(&repo);
    let codex = dir.path().join("codex-gone");
    fs::write(
        &codex,
        "#!/bin/sh\n[ \"$1\" = --version ] && { echo 'codex-cli 0.46.0'; exit 0; }\nexit 2\n",
    )
    .unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let options = SuperviseOptions {
        codex: codex.clone(),
        ..supervise_options(1, true)
    };
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            let reviewer = TestReviewer::new(&[verdict("pass", &[], "from Claude")]);
            let outcome = runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            );
            (outcome, reviewer.prompts().len())
        })
    };
    // Codex passed the preflight before the claim; it is gone by the review.
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !queue.show(TaskId::new(1)).unwrap().runs.is_empty()
    });
    fs::remove_file(&codex).unwrap();
    let (outcome, prompts) = joined(supervisor, "the supervisor thread to return");
    backend.join();
    let outcome = outcome.unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(prompts, 1, "Claude reviewed the run");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let started = payloads(&detail, "review_started");
    assert_eq!(started.len(), 2, "{started:?}");
    assert_eq!(started[0]["launch"]["provider"], "codex");
    assert_eq!(started[1]["launch"]["provider"], "claude");
    assert_eq!(started[1]["launch"]["switch_reason"], "executable_missing");
    let retried = payloads(&detail, "review_retried");
    assert_eq!(retried.len(), 1, "{retried:?}");
    assert_eq!(retried[0]["provider"], "codex");
    assert!(payloads(&detail, "review_failed").is_empty());
    let held = queue_events(&db, "provider_held");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["provider"], "codex");
}

/// The run's files, which remove the run's worktree as the prompt of
/// review 1 is written: after its receipt was accepted, before its review
/// starts.
#[derive(Default)]
struct WorktreeGoneAtReview {
    removed: Mutex<Option<PathBuf>>,
}

impl RunFiles for WorktreeGoneAtReview {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        LocalRunFiles.create_dir_all(dir)
    }
    fn create_new_dir(&self, dir: &Path) -> io::Result<()> {
        LocalRunFiles.create_new_dir(dir)
    }
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        if path
            .file_name()
            .is_some_and(|name| name == "review-prompt-1.txt")
        {
            let worktree = path.with_file_name("worktree");
            fs::remove_dir_all(&worktree)?;
            *self.removed.lock().unwrap() = Some(worktree);
        }
        LocalRunFiles.write(path, contents)
    }
    fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        LocalRunFiles.copy(from, to)
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        LocalRunFiles.read(path)
    }
    fn read_from(&self, path: &Path, offset: u64) -> io::Result<Vec<u8>> {
        LocalRunFiles.read_from(path, offset)
    }
    fn read_range(&self, path: &Path, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        LocalRunFiles.read_range(path, offset, len)
    }
    fn read_tail(&self, path: &Path, bytes: u64) -> io::Result<Vec<u8>> {
        LocalRunFiles.read_tail(path, bytes)
    }
    fn size(&self, path: &Path) -> io::Result<u64> {
        LocalRunFiles.size(path)
    }
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        LocalRunFiles.read_to_string(path)
    }
    fn try_lock(&self, path: &Path) -> io::Result<Option<Box<dyn std::any::Any + Send>>> {
        LocalRunFiles.try_lock(path)
    }
    fn modified(&self, path: &Path) -> io::Result<SystemTime> {
        LocalRunFiles.modified(path)
    }
    fn read_stamped(&self, path: &Path) -> Result<Option<(SystemTime, Vec<u8>)>> {
        LocalRunFiles.read_stamped(path)
    }
    fn is_file(&self, path: &Path) -> bool {
        LocalRunFiles.is_file(path)
    }
    fn is_dir(&self, path: &Path) -> bool {
        LocalRunFiles.is_dir(path)
    }
    fn exists(&self, path: &Path) -> bool {
        LocalRunFiles.exists(path)
    }
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        LocalRunFiles.read_dir(dir)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        LocalRunFiles.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        LocalRunFiles.remove_file(path)
    }
    fn tree_size(&self, dir: &Path) -> io::Result<Option<u64>> {
        LocalRunFiles.tree_size(dir)
    }
    fn remove_dir_all(&self, dir: &Path) -> io::Result<()> {
        LocalRunFiles.remove_dir_all(dir)
    }
    fn append_line(&self, path: &Path, line: &str) -> io::Result<()> {
        LocalRunFiles.append_line(path, line)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        LocalRunFiles.canonicalize(path)
    }
    fn write_fenced(&self, path: &Path, text: &str, info: &str, body: &Path) -> Result<()> {
        LocalRunFiles.write_fenced(path, text, info, body)
    }
    fn now(&self) -> SystemTime {
        LocalRunFiles.now()
    }
}

/// A Codex review whose run's worktree is gone before it starts fails as
/// the review's own preparation, not as its provider's start
/// (ADR-t1063-1 decision 4, ADR-t1207-1): Codex is not held, the review
/// does not move to Claude, and `review_failed` opens the
/// `approve_landing` ask. Without the check, the spawn in a missing
/// directory would read as Codex's missing executable, as in
/// [`a_codex_review_whose_codex_does_not_start_moves_to_claude`].
#[test]
fn a_review_whose_worktree_is_gone_fails_without_holding_its_provider() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, repo, db) = fixture();
    select_codex_review(&repo);
    // Passes the supervisor's preflight; no review reaches it.
    let codex = dir.path().join("codex-unused");
    fs::write(
        &codex,
        "#!/bin/sh\n[ \"$1\" = --version ] && { echo 'codex-cli 0.46.0'; exit 0; }\nexit 2\n",
    )
    .unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let files = Arc::new(WorktreeGoneAtReview::default());
    let options = SuperviseOptions {
        codex,
        files: Some(RunFilesPort(files.clone())),
        ..supervise_options(1, true)
    };
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "from Claude")]);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert!(reviewer.prompts().is_empty(), "Claude reviewed the run");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    assert_eq!(files.removed.lock().unwrap().as_ref(), Some(&worktree));
    let kinds = event_kinds(&detail);
    assert!(
        position(&kinds, "receipt_observed") < position(&kinds, "review_started"),
        "{kinds:?}"
    );
    let started = payloads(&detail, "review_started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["launch"]["provider"], "codex");
    assert!(
        started[0]["launch"]["switch_reason"].is_null(),
        "{started:?}"
    );
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["attempt"], 1);
    let error = failed[0]["error"].as_str().unwrap();
    assert!(
        error.contains(&format!(
            "the run's worktree {} is gone",
            worktree.display()
        )),
        "{error}"
    );
    // Neither held nor switched: the provider was never tried.
    assert!(payloads(&detail, "review_retried").is_empty(), "{kinds:?}");
    assert!(queue_events(&db, "provider_held").is_empty());
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::ApproveLanding);
    assert_eq!(asks[0].run_id.as_ref(), Some(run.id()));
    assert_eq!(failed[0]["ask_id"], json!(asks[0].id));
}

/// The queue's events of `kind` (a provider's hold is the queue's).
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

/// A Codex review at its usage limit, while Claude may run: Codex is held
/// without an ask (ADR-t1063-1 decision 5), and the review starts again
/// on Claude (decision 4), recorded as `review_retried`; the run lands.
#[test]
fn a_codex_review_at_its_usage_limit_holds_codex_and_moves_to_claude() {
    let (dir, repo, db, backend, codex) = codex_fixture();
    select_codex_review(&repo);
    set_turns(dir.path(), FINISH);
    fs::write(
        dir.path().join("codex-review-failure.jsonl"),
        USAGE_LIMIT_REVIEW,
    )
    .unwrap();
    let mut options = supervise_options(1, true);
    options.codex = codex;
    options.codex_home = Some(dir.path().join(CODEX_HOME));
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "from Claude")]);
    let result = {
        let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
        runtime::supervise_with_reviewer(
            &db,
            &repo,
            &backend,
            &claude_stub(&db),
            &reviewer,
            Path::new(env!("CARGO_BIN_EXE_dagq")),
            &options,
        )
        .unwrap()
    };
    backend.join();
    assert_eq!(result["errors"], json!([]), "{result}");
    assert_eq!(result["runs"][0]["status"], "integrated", "{result}");
    assert_eq!(reviewer.prompts().len(), 1, "Claude reviewed the run");
    let detail = detail(&db);
    let started = payloads(&detail, "review_started");
    assert_eq!(started.len(), 2, "{started:?}");
    assert_eq!(started[0]["launch"]["provider"], "codex");
    assert_eq!(started[1]["launch"]["provider"], "claude");
    assert_eq!(started[1]["launch"]["switched_from"], "codex");
    assert_eq!(started[1]["launch"]["switch_reason"], "usage_limit");
    let retried = payloads(&detail, "review_retried");
    assert_eq!(retried.len(), 1, "{retried:?}");
    assert_eq!(retried[0]["provider"], "codex");
    assert_eq!(retried[0]["switch_reason"], "usage_limit");
    // The Codex review that moved names its thread on its end and its
    // span; the Claude review's span keeps its own session.
    let spans = review_spans(&detail);
    assert_eq!(spans.len(), 2, "{spans:?}");
    assert_thread_without_model(retried[0], spans[0]);
    assert_eq!(spans[1]["session_id"], started[1]["session_id"]);
    assert!(payloads(&detail, "review_failed").is_empty());
    let held = queue_events(&db, "provider_held");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["provider"], "codex");
    assert_eq!(held[0]["reason"], "usage_limit");
    assert!(
        SqliteQueue::open(&db)
            .unwrap()
            .asks(AskQuery::default())
            .unwrap()
            .is_empty(),
        "Codex's hold opened no ask"
    );
}

/// Under `--no-claude`, a Codex review that printed no readable verdict
/// but shows its usage limit: Codex is held, no provider is left to review
/// it again, so it fails as it is (`job_failed`), its output named, rather
/// than as a review no agent ran (ADR-t1207-1).
#[test]
fn no_claude_unreadable_codex_review_at_its_limit_fails_with_its_output() {
    let (dir, repo, db, backend, codex) = codex_fixture();
    select_codex_review(&repo);
    set_turns(dir.path(), FINISH);
    let said = json!({"type":"item.completed","item":{"id":"review","type":"agent_message","text":"no verdict here"}});
    fs::write(
        dir.path().join("codex-review-1.jsonl"),
        format!("{}{said}\n", USAGE_LIMIT_REVIEW),
    )
    .unwrap();
    let mut options = supervise_options(1, true);
    options.codex = codex;
    options.no_claude = true;
    options.codex_home = Some(dir.path().join(CODEX_HOME));
    let reviewer = TestReviewer::new(&[]);
    let result = runtime::supervise_with_reviewer(
        &db,
        &repo,
        &backend,
        &claude_stub(&db),
        &reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options,
    )
    .unwrap();
    backend.join();
    assert_eq!(result["errors"], json!([]), "{result}");
    assert_eq!(result["runs"][0]["status"], "awaiting_integration");
    let detail = detail(&db);
    assert!(payloads(&detail, "review_retried").is_empty());
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["code"], "job_failed");
    assert_eq!(failed[0]["attempt"], 1);
    assert_thread_without_model(failed[0], review_spans(&detail)[0]);
    let asks = SqliteQueue::open(&db)
        .unwrap()
        .asks(AskQuery::default())
        .unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert!(
        asks[0].question.contains("review-1.out"),
        "{}",
        asks[0].question
    );
    assert!(!asks[0].question.contains("No review agent ran"));
    assert_eq!(queue_events(&db, "provider_held").len(), 1);
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
        // The worktree trusted on every call, so that Codex does not
        // persist a trust in the person's config (task 1174).
        let worktree = run.worktree_path().unwrap();
        assert!(
            line.contains(&format!(
                r#" -c| projects={{"{worktree}"={{trust_level="trusted"}}}}|"#
            )),
            "call {at}: {line}"
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
