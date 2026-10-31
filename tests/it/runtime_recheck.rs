//! Runtime tests: the landing recheck of the runs that wait to land
//! (ADR-0068). After a landing moves main, a run waiting for a person's
//! answer is checked against it (`git merge-tree`, then the `[recheck]
//! command` of `dagq.toml`), and one that no longer lands is resumed
//! without waiting for the answer, which its ask then tells the person.
//! A run the supervisor holds in a slot to land is parked when it would
//! land instead. When a recheck is due, which runs it checks, the landing
//! it names and what it records are decided in
//! `dagq::application::supervise::recheck` and `dagq::domain::recheck`
//! (their unit tests); these tests keep Git, the command, SQLite, the
//! queue's lock and the supervisor's wiring.
use crate::runtime_support;

use dagq::domain::EventKind;
// Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
#[cfg(any())]
use dagq::domain::stats::StatsQuery;
use runtime_support::*;

/// The resumed session of the waiting run: rebase onto the main the
/// request names, resolving `change.txt`, then rewrite the receipt.
const RESOLVING_RESUME: &str =
    "await_message; resolve || exit 1; receipt \"$(git rev-parse HEAD)\"; idle; await_exit";

/// Task 1 waits for a person on a `concern`; then task 2 is added and
/// supervised, and its review passes, so it lands.
fn waiting_then_landing(
    backend: &TestWorkspace,
    repo: &Path,
    db: &Path,
) -> (TaskRun, dagq::domain::Ask, TestReviewer) {
    let reviewer = TestReviewer::new(&[
        verdict("concern", &["a person should look"], "unsure"),
        verdict("pass", &[], "meets the acceptance"),
    ]);
    let outcome = supervise_reviewed(db, repo, backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(db).unwrap();
    let waiting = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(waiting.status(), RunStatus::AwaitingIntegration);
    let ask = queue.asks(Default::default()).unwrap()[0].clone();
    assert_eq!(ask.kind, AskKind::ApproveLanding);
    add_ready_task(&mut queue, "second task", &[]);
    (waiting, ask, reviewer)
}

/// A run waiting on its `approve_landing` ask conflicts with the run that
/// lands after it (both change `change.txt`): the recheck after that
/// landing finds it with `git merge-tree`, records `landing_recheck_failed`,
/// parks the run and resumes it at once with the recheck's request, without
/// counting the resume. An answer given while the run is resumed (`land:
/// <reason>`, taken as `land`) is not delivered then (`runtime_delivers:
/// false`, the run is not awaiting integration); the ask, which the
/// recheck's paragraph now explains, stays unclosed, and once the rebased
/// run waits again after its validation the supervisor applies it, and the
/// run lands without a third review. `status` shows the recheck.
#[test]
fn a_waiting_run_that_a_landing_conflicts_with_is_resumed_before_its_answer() {
    let (dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let (waiting, ask, reviewer) = waiting_then_landing(&backend, &repo, &db);
    let marker = dir.path().join("answered");
    backend.resume_script_for(
        1,
        &format!("await_file {}; {RESOLVING_RESUME}", shell_path(&marker)),
    );
    let answerer = {
        let (db, marker) = (db.clone(), marker.clone());
        thread::spawn(move || {
            wait_until(&db, Duration::from_secs(20), |queue| {
                !payloads(&queue.show(TaskId::new(1)).unwrap(), "resume_started").is_empty()
            });
            let mut queue = SqliteQueue::open(&db).unwrap();
            let noted = queue.read_ask(ask.id).unwrap();
            queue.answer(ask.id, "land: the rebase is fine").unwrap();
            let detail = queue.show(TaskId::new(1)).unwrap();
            let delivers = detail
                .events
                .iter()
                .find(|e| e.kind == EventKind::AskAnswered && e.payload["ask_id"] == json!(ask.id))
                .unwrap()
                .payload["runtime_delivers"]
                .clone();
            fs::write(&marker, "").unwrap();
            (detail.runs[0].status(), delivers, noted)
        })
    };
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let (answered_in, delivers, noted) = joined(answerer, "the answerer to answer");
    assert_ne!(answered_in, RunStatus::AwaitingIntegration);
    assert_eq!(delivers, false);
    // A later pass applies it if the first supervisor ended before it.
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let mut queue = SqliteQueue::open(&db).unwrap();
    let second = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(second.runs[0].status(), RunStatus::Integrated);
    let main = events_of(&db, second.runs[0].id(), "run_integrated")[0]["commit"].clone();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    let found = payloads(&detail, "landing_recheck_failed");
    assert_eq!(found.len(), 1, "{:?}", event_kinds(&detail));
    let found = found[0];
    assert_eq!(found["code"], "rebase_conflict");
    assert_eq!(found["action"], "resumed");
    assert_eq!(found["status"], "needs_session");
    assert_eq!(found["main"], main);
    assert_eq!(found["head"], json!(waiting.result_commit().unwrap()));
    assert_eq!(found["conflicts"], json!(["change.txt"]));
    assert_eq!(found["landed_task_id"], 2);
    assert_eq!(found["landed_run_id"], json!(second.runs[0].id()));
    let reason = found["reason"].as_str().unwrap();
    assert!(reason.starts_with("after task 2 (run "), "{reason}");
    // Resumed at once, without counting toward MAX_RESUME_ATTEMPTS.
    let started = payloads(&detail, "resume_started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["counted"], false);
    // After the recheck: the note on the ask, the resume, and a validation
    // of the rewritten receipt with no review after it, then the answer
    // applied.
    let kinds = [
        "landing_recheck_failed",
        "ask_updated",
        "resume_started",
        "ask_answered",
        "resume_finished",
        "validation_finished",
        "review_started",
        "integration_approved",
        "landing_queued",
        "run_integrated",
    ];
    assert_eq!(
        kinds_from(&detail, "landing_recheck_failed", &kinds),
        [
            "landing_recheck_failed",
            "ask_updated",
            "resume_started",
            "ask_answered",
            "resume_finished",
            "validation_finished",
            "integration_approved",
            "landing_queued",
            "run_integrated",
        ]
    );
    let text = &session_texts(&run)[0];
    assert!(
        text.contains("the supervisor's landing recheck found that it no longer lands"),
        "{text}"
    );
    assert!(text.contains(&format!("Reason: {reason}")), "{text}");
    assert!(noted.answered_at.is_none() && noted.closed_at.is_none());
    assert!(
        noted
            .question
            .ends_with(&format!("Landing recheck: {reason}. The supervisor resumes the run to bring it onto main without waiting for this answer; once it waits again, the answer applies to the rebased run.")),
        "{}",
        noted.question
    );
    assert_eq!(
        payloads(&detail, "ask_updated")[0],
        &json!({"ask_id": ask.id, "kind": "approve_landing", "why": "landing_recheck_failed"})
    );

    let status = runtime::status(&db).unwrap();
    let recheck = &status["landing_recheck"];
    assert_eq!(recheck["main"], main);
    assert_eq!(recheck["landed_task_id"], 2);
    assert_eq!(recheck["checked"], 1);
    assert_eq!(recheck["conflicts"], 1);
    assert_eq!(recheck["resumed"], 1);
    assert_eq!(recheck["command"], Value::Null);
    assert_eq!(
        recheck["failed_runs"],
        json!([{"run_id": run.id(), "code": "rebase_conflict", "action": "resumed"}])
    );
    // Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
    #[cfg(any())]
    {
        let stats = runtime::stats(
            &db,
            &StatsQuery {
                full: true,
                ..Default::default()
            },
        )
        .unwrap();
        let rechecks = &stats["landing_rechecks"];
        assert_eq!(rechecks["rechecks"], 1);
        assert_eq!(rechecks["runs_checked"], 1);
        assert_eq!(rechecks["conflicts"], 1);
        assert_eq!(rechecks["check_failures"], 0);
        assert_eq!(rechecks["resumed"], 1);
    }

    // The rebased run lands from its answer.
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(run.status(), RunStatus::Integrated);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let approved = payloads(&detail, "integration_approved");
    assert_eq!(approved.len(), 1);
    assert_eq!(approved[0]["ask_id"], ask.id.as_i64());
    assert_eq!(
        payloads(&detail, "landing_queued"),
        [&json!({"via": "approve", "ask_id": ask.id})]
    );
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        "resolved by the resumed session\n"
    );
    assert_eq!(reviewer.prompts().len(), 2);

    // The run that waited in its `approve_landing` ask landed after the
    // recheck found its conflict; the landing that moved main did not wait.
    // Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
    #[cfg(any())]
    {
        let stats = runtime::stats(
            &db,
            &StatsQuery {
                full: true,
                ..Default::default()
            },
        )
        .unwrap();
        let waits = &stats["landing_waits"];
        let waited = &waits["waited"];
        assert_eq!(waited["landed"], 1, "{waits}");
        assert_eq!(waited["conflicted"], 1);
        assert_eq!(waited["conflicted_ratio"], 1.0);
        assert_eq!(waited["recheck_conflicts"], 1);
        assert_eq!(waited["rebase_conflicts"], 0);
        assert_eq!(waited["wait_secs"]["count"], 1);
        assert_eq!(waited["by_wait"], json!({"approve_landing": 1}));
        assert_eq!(waited["runs"][0]["run_id"], json!(run.id()));
        let not_waited = &waits["not_waited"];
        assert_eq!(not_waited["landed"], 1);
        assert_eq!(not_waited["conflicted"], 0);
        assert_eq!(not_waited["conflicted_ratio"], 0.0);
        assert!(not_waited.get("wait_secs").is_none(), "{not_waited}");
    }
}

/// The events of `kinds` of `detail`, in order, from the first of them
/// that is `from`.
fn kinds_from<'a>(
    detail: &'a dagq::domain::TaskDetail,
    from: &str,
    kinds: &[&str],
) -> Vec<&'a str> {
    detail
        .events
        .iter()
        .map(|e| e.kind.as_str())
        .filter(|k| kinds.contains(k))
        .skip_while(|k| *k != from)
        .collect()
}

/// A conflict Git does not see: the waiting run adds `a.txt`, which needs
/// `b.txt`, and the landing run deletes `b.txt`. `git merge-tree` merges
/// them cleanly, but the `[recheck] command` fails on main's tree with the
/// run merged in, in the queue's scratch worktree with the target
/// directory of the Rust profile the repository declares; the run is
/// parked and resumed with the command's failure, and that resume counts.
#[test]
fn a_waiting_run_whose_check_fails_on_the_new_main_is_resumed() {
    let (dir, repo, db) = fixture();
    fs::write(repo.join("b.txt"), "b\n").unwrap();
    fs::write(
        repo.join("dagq.toml"),
        format!("{RUST_DECLARED}[recheck]\ncommand = 'echo \"target=$CARGO_TARGET_DIR\"; if [ -f a.txt ]; then test -f b.txt || {{ echo error[E0063]: missing_b; exit 1; }}; fi'\n"),
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "b and the recheck"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.script_for(
        1,
        "printf 'uses b\\n' > a.txt && git add a.txt && git commit -q -m uses; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    backend.script_for(
        2,
        "git rm -q b.txt && git commit -q -m drop; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    backend.resume_script_for(
        1,
        "await_message; unlocked git rebase -q \"$MAIN\" || exit 1; printf 'b\\n' > b.txt; unlocked git add b.txt; unlocked git commit -q -m restore; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let (waiting, _ask, reviewer) = waiting_then_landing(&backend, &repo, &db);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.show(TaskId::new(2)).unwrap().runs[0].status(),
        RunStatus::Integrated
    );
    let main = git_out(&repo, &["rev-parse", "main"]);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let found = payloads(&detail, "landing_recheck_failed");
    assert_eq!(found.len(), 1, "{:?}", event_kinds(&detail));
    let found = found[0];
    assert_eq!(found["code"], "verification_failed");
    assert_eq!(found["action"], "resumed");
    assert_eq!(found["head"], json!(waiting.result_commit().unwrap()));
    assert_eq!(found["exit_code"], 1);
    assert_eq!(found["class"], "build_error");
    assert!(found.get("conflicts").is_none());
    let recheck_dir = fs::canonicalize(dir.path()).unwrap().join("recheck");
    let tail = found["output_tail"].as_str().unwrap();
    assert!(
        tail.contains(&format!("target={}", recheck_dir.join("target").display())),
        "{tail}"
    );
    // The Rust profile declared under its name builds in its own directory.
    assert!(recheck_dir.join("target").is_dir());
    let log = Path::new(found["log_path"].as_str().unwrap());
    assert!(log.starts_with(waiting.run_dir().unwrap()), "{log:?}");
    assert_eq!(
        log.file_name().unwrap().to_str().unwrap(),
        format!("recheck-{}.log", &main[..12])
    );
    // Main alone passed the command after it (ADR-t2141-1), in the same
    // scratch worktree, which it left on main's tree.
    assert!(
        recheck_dir
            .join(format!("main-{}.log", &main[..12]))
            .is_file()
    );
    assert!(!recheck_scratch(&recheck_dir).join("a.txt").exists());
    assert!(!recheck_scratch(&recheck_dir).join("b.txt").exists());
    assert_eq!(status_main_check(&db), "passed");
    // Main's pass with no failure of it before is not recorded on the queue.
    assert!(main_checks(&db).is_empty());
    let reason = found["reason"].as_str().unwrap();
    assert!(reason.contains("exits with 1 on main"), "{reason}");
    let started = payloads(&detail, "resume_started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["counted"], true);
    let text = &session_texts(&run)[0];
    assert!(
        text.contains("run that command in the worktree after the rebase"),
        "{text}"
    );
    // Rebased onto the main without b.txt, and b.txt put back.
    let head = run.result_commit().unwrap().as_str();
    assert_eq!(git_out(&repo, &["rev-parse", &format!("{head}~2")]), main);
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["landing_recheck"]["check_failed"], 1);
    assert_eq!(
        status["landing_recheck"]["command"],
        "echo \"target=$CARGO_TARGET_DIR\"; if [ -f a.txt ]; then test -f b.txt || { echo error[E0063]: missing_b; exit 1; }; fi"
    );
}

/// `main_check` of the latest `landing_recheck_finished`, as `status`
/// shows it.
fn status_main_check(db: &Path) -> Value {
    runtime::status(db).unwrap()["landing_recheck"]["main_check"].clone()
}

/// Declarations of the Rust profile (ADR-t2268-1 decision 6): each one
/// whose paths match the merged tree builds in a directory of its own,
/// named after the declaration, in a run of the command of its own (one
/// variable has one value in a run), and the one that does not match
/// makes none; none is the profile's own `target`. The command passes in
/// the first run and fails in the second, whose failure is the finding.
/// A scratch worktree of another name the recheck had (the old layout's
/// `recheck/worktree`, or one named after the queue's path before it
/// moved) goes with its record.
#[test]
fn each_declaration_that_applies_builds_in_a_directory_of_its_own() {
    let (dir, repo, db) = fixture();
    fs::write(
        repo.join("dagq.toml"),
        r#"[recheck]
command = 'touch "$CARGO_TARGET_DIR/built"; echo "target=$CARGO_TARGET_DIR"; [ "${CARGO_TARGET_DIR##*/}" != target-tools ]'

[toolchains.core]
profile = "rust"
paths = ["**/*.rs"]

[toolchains.docs]
profile = "rust"
paths = ["**/*.md"]

[toolchains.tools]
profile = "rust"
"#,
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &["commit", "-q", "-m", "the recheck and three declarations"],
    );
    let recheck = fs::canonicalize(dir.path()).unwrap().join("recheck");
    let old_scratches = [
        recheck.join("worktree"),
        recheck.join("worktree-recheck-0000000000000000"),
    ];
    for old in &old_scratches {
        git(
            &repo,
            &["worktree", "add", "-q", "--detach", old.to_str().unwrap()],
        );
    }
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.script_for(
        2,
        "printf 'other\\n' > other.md && git add other.md && git commit -q -m other; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    backend.resume_script_for(1, "await_message; idle; await_exit");
    let (_waiting, _ask, reviewer) = waiting_then_landing(&backend, &repo, &db);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let found = payloads(&detail, "landing_recheck_failed");
    assert_eq!(found.len(), 1, "{:?}", event_kinds(&detail));
    let tail = found[0]["output_tail"].as_str().unwrap();
    let recheck_dir = fs::canonicalize(dir.path()).unwrap().join("recheck");
    assert!(
        tail.contains(&format!(
            "target={}",
            recheck_dir.join("target-tools").display()
        )),
        "{tail}"
    );
    assert!(recheck_dir.join("target-docs/built").is_file());
    assert!(recheck_dir.join("target-tools/built").is_file());
    assert!(!recheck_dir.join("target-core").exists());
    assert!(!recheck_dir.join("target").exists());
    assert!(recheck_scratch(&recheck_dir).is_dir());
    for old in &old_scratches {
        assert!(!old.exists(), "{old:?}");
        let name = old.file_name().unwrap();
        assert!(!repo.join(".git/worktrees").join(name).exists(), "{name:?}");
    }
}

/// Commit `file` on main by hand, outside dagq; the new main.
fn move_main(repo: &Path, file: &str, text: &str) -> String {
    fs::write(repo.join(file), text).unwrap();
    git(repo, &["add", file]);
    git(repo, &["commit", "-q", "-m", file]);
    git_out(repo, &["rev-parse", "main"])
}

/// The ask's recheck paragraphs.
fn recheck_paragraphs(question: &str) -> Vec<&str> {
    question
        .split("\n\n")
        .filter(|p| p.starts_with("Landing recheck: "))
        .collect()
}

/// Main moves while a run waits, outside dagq (a push outside dagq), and
/// each next supervisor, which never saw the move, rechecks the run on its
/// first pass. The first recheck finds the run clean and the ask's recheck
/// paragraph says so (ADR-t1311-1); main moves again into the run, and the
/// recheck's paragraph becomes the failure's alone, and the run is resumed
/// (`landing_recheck_failed` with `action: resumed`). With no dagq landing
/// behind main, the landed run and task are null and
/// `landing_recheck_finished` goes on the run it checked.
#[test]
fn a_conflict_after_a_clean_recheck_replaces_the_asks_recheck_paragraph() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let (waiting, ask, reviewer) = one_waiting(&backend, &repo, &db);
    let first = move_main(&repo, "one.txt", "one\n");
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let clean = payloads(
        &queue.show(TaskId::new(1)).unwrap(),
        "landing_recheck_clean",
    )
    .into_iter()
    .cloned()
    .collect::<Vec<_>>();
    assert_eq!(clean.len(), 1);
    assert_eq!(clean[0]["main"], json!(first));
    assert_eq!(clean[0]["head"], json!(waiting.result_commit().unwrap()));
    let question = queue.read_ask(ask.id).unwrap().question;
    assert_eq!(
        recheck_paragraphs(&question),
        [format!(
            "Landing recheck: after main moved without a dagq landing, the landing recheck found that the run still lands cleanly on main {}: git merges it without a conflict (no command was run).",
            &first[..12]
        )]
    );
    assert!(question.starts_with(&ask.question), "{question}");

    let second = move_main(&repo, "change.txt", "changed on main by hand\n");
    backend.resume_script_for(1, RESOLVING_RESUME);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let found = payloads(&detail, "landing_recheck_failed");
    assert_eq!(found.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(found[0]["code"], "rebase_conflict");
    assert_eq!(found[0]["action"], "resumed");
    assert_eq!(found[0]["status"], "needs_session");
    assert_eq!(found[0]["main"], json!(second));
    assert_eq!(found[0]["head"], json!(waiting.result_commit().unwrap()));
    assert_eq!(found[0]["landed_task_id"], Value::Null);
    assert_eq!(found[0]["landed_run_id"], Value::Null);
    assert_eq!(payloads(&detail, "resume_started").len(), 1);
    let reason = found[0]["reason"].as_str().unwrap();
    assert!(
        reason.starts_with("after main moved without a dagq landing"),
        "{reason}"
    );
    let finished = events_of(&db, waiting.id(), "landing_recheck_finished");
    assert_eq!(finished.len(), 2, "{finished:?}");
    assert_eq!(finished[1]["main"], json!(second));
    assert_eq!(finished[1]["landed_run_id"], Value::Null);
    assert_eq!(finished[1]["checked"], 1);
    let question = queue.read_ask(ask.id).unwrap().question;
    let paragraphs = recheck_paragraphs(&question);
    assert_eq!(paragraphs.len(), 1, "{question}");
    assert!(
        paragraphs[0].starts_with(&format!(
            "Landing recheck: {reason}. The supervisor resumes"
        )),
        "{question}"
    );
    assert!(!question.contains(&first[..12]), "{question}");
    let why: Vec<&Value> = payloads(&detail, "ask_updated")
        .iter()
        .map(|p| &p["why"])
        .collect();
    assert_eq!(
        why,
        [
            &json!("landing_recheck_clean"),
            &json!("landing_recheck_failed")
        ]
    );
}

/// Task 1 waits for a person on a `concern`, with no other task.
fn one_waiting(
    backend: &TestWorkspace,
    repo: &Path,
    db: &Path,
) -> (TaskRun, dagq::domain::Ask, TestReviewer) {
    let reviewer = TestReviewer::new(&[verdict("concern", &["a person should look"], "unsure")]);
    let outcome = supervise_reviewed(db, repo, backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(db).unwrap();
    let waiting = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(waiting.status(), RunStatus::AwaitingIntegration);
    let ask = queue.asks(Default::default()).unwrap()[0].clone();
    assert_eq!(ask.kind, AskKind::ApproveLanding);
    (waiting, ask, reviewer)
}

/// The number of landing rechecks that ended, from `stats`.
// Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
#[cfg(any())]
fn rechecks_finished(db: &Path) -> Value {
    runtime::stats(
        db,
        &StatsQuery {
            full: true,
            ..Default::default()
        },
    )
    .unwrap()["landing_rechecks"]["rechecks"]
        .clone()
}

/// `supervise --once` on a thread, for a test that acts while it runs.
fn supervising(
    db: &Path,
    repo: &Path,
    backend: &Arc<TestWorkspace>,
    reviewer: &Arc<TestReviewer>,
) -> thread::JoinHandle<Result<Value>> {
    supervising_with(db, repo, backend, reviewer, supervise_options(4, true))
}

/// [`supervising`] with `options`.
fn supervising_with(
    db: &Path,
    repo: &Path,
    backend: &Arc<TestWorkspace>,
    reviewer: &Arc<TestReviewer>,
    options: SuperviseOptions,
) -> thread::JoinHandle<Result<Value>> {
    let (db, repo, backend, reviewer) = (
        db.to_path_buf(),
        repo.to_path_buf(),
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
}

/// Hold the queue's recheck lock as another supervisor's recheck would.
fn hold_recheck_lock(db: &Path) -> Box<dyn std::any::Any + Send> {
    use dagq::application::RunFiles;
    let dir = db.parent().unwrap().join("recheck");
    fs::create_dir_all(&dir).unwrap();
    dagq::infrastructure::run_files::LocalRunFiles
        .try_lock(&dir.join("lock"))
        .unwrap()
        .expect("the recheck lock is free")
}

fn integrated(queue: &mut SqliteQueue, task: i64) -> bool {
    queue
        .show(TaskId::new(task))
        .unwrap()
        .runs
        .first()
        .is_some_and(|run| run.status() == RunStatus::Integrated)
}

/// The queue's supervisors share one lock on the recheck (its scratch
/// worktree and target), for the recheck after a landing as for the one
/// after a main moved (ADR-t1310-1): while another holder has it, here the
/// test standing in for another supervisor's recheck, none starts, nothing
/// is recorded, and the due recheck waits. Once it is free, the supervisor
/// checks the main its landing moved, naming the landing.
#[test]
fn no_recheck_starts_while_another_supervisor_holds_the_recheck_lock() {
    use dagq::application::RunFiles;
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let (waiting, _ask, reviewer) = waiting_then_landing(&backend, &repo, &db);
    let reviewer = Arc::new(reviewer);
    backend.resume_script_for(1, RESOLVING_RESUME);
    let lock = hold_recheck_lock(&db);
    let supervisor = supervising(&db, &repo, &backend, &reviewer);
    wait_until(&db, Duration::from_secs(60), |queue| integrated(queue, 2));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(payloads(&detail, "landing_recheck_failed").is_empty());
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    // Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
    #[cfg(any())]
    assert_eq!(rechecks_finished(&db), 0);
    assert!(
        !supervisor.is_finished(),
        "the due recheck waits for the lock"
    );

    drop(lock);
    let outcome = joined(supervisor, "the supervisor to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let second = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    let main = events_of(&db, second.id(), "run_integrated")[0]["commit"].clone();
    // Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
    #[cfg(any())]
    assert_eq!(rechecks_finished(&db), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let found = payloads(&detail, "landing_recheck_failed");
    assert_eq!(found.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(found[0]["main"], main);
    assert_eq!(found[0]["head"], json!(waiting.result_commit().unwrap()));
    assert_eq!(found[0]["landed_task_id"], 2);
    assert_eq!(found[0]["action"], "resumed");
    let finished = events_of(&db, second.id(), "landing_recheck_finished");
    assert_eq!(finished.len(), 1);
    // Free again once the recheck is recorded.
    let dir = db.parent().unwrap().join("recheck");
    assert!(
        dagq::infrastructure::run_files::LocalRunFiles
            .try_lock(&dir.join("lock"))
            .unwrap()
            .is_some()
    );
}

/// A worker that adds `e2e.txt`, which `[e2e] paths` names, with its change:
/// its run passes its review and then waits in its slot for its e2e before
/// it lands.
const E2E_AGENT: &str = "printf 'e2e\\n' > e2e.txt && git add e2e.txt; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit";

/// Options whose e2e (the stand-in for `cargo test --test e2e`) passes only
/// once the test writes `release`: until then the run that needs it is held
/// in its slot to land, as a headless worker's run is (its session exits as
/// soon as its review passes, so nothing else holds it there).
fn held_e2e(release: &Path) -> SuperviseOptions {
    e2e_options(format!(
        "{}; echo 'test result: ok. 1 passed; 0 failed'",
        crate::common::await_path(release)
    ))
}

/// The run of `task`, once it has one.
fn run_of(db: &Path, task: i64) -> Option<TaskRun> {
    SqliteQueue::open(db)
        .unwrap()
        .show(TaskId::new(task))
        .unwrap()
        .runs
        .first()
        .cloned()
}

/// Whether the run of `task` has an event of `kind`.
fn has_event(db: &Path, task: i64, kind: &str) -> bool {
    run_of(db, task).is_some_and(|run| !events_of(db, run.id(), kind).is_empty())
}

/// A run this supervisor holds in its slot to land (here waiting for its
/// e2e after its review passed) conflicts with the run that lands
/// meanwhile: the recheck after that landing records
/// `landing_recheck_failed` with `action: held` and leaves it in the slot.
/// Once its e2e passed, the run is parked instead of landing (`repeat:
/// true`, `needs_session`, no integration tried) and resumed with the
/// recheck's request, uncounted, and lands from there. Moved from the
/// interactive `a_run_held_in_its_slot_that_a_landing_conflicts_with_is_parked_before_it_lands`,
/// which held the run with its session's `/exit` and which task 1437
/// deleted.
#[test]
fn a_run_held_in_its_slot_for_its_e2e_that_a_landing_conflicts_with_is_parked_before_it_lands() {
    let (dir, repo, db) = fixture();
    with_e2e_paths(&repo, "[\"e2e.txt\"]");
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "second", &[]);
    let backend = Arc::new(TestWorkspace::new(&db, false, E2E_AGENT));
    backend.script_for(2, GATED_AGENT);
    backend.resume_script_for(1, RESOLVING_RESUME);
    let reviewer = Arc::new(TestReviewer::new(&[verdict(
        "pass",
        &[],
        "meets the acceptance",
    )]));
    let release = dir.path().join("e2e-release");
    let supervisor = supervising_with(&db, &repo, &backend, &reviewer, held_e2e(&release));
    // Task 1 is reviewed and waits in its slot for its e2e before task 2's
    // session commits anything.
    wait_until(&db, Duration::from_secs(60), |_| {
        has_event(&db, 1, "run_e2e_started") && run_of(&db, 2).is_some()
    });
    let second = run_of(&db, 2).unwrap();
    fs::write(
        exit_request_path(second.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    wait_until(&db, Duration::from_secs(60), |_| {
        has_event(&db, 1, "landing_recheck_failed")
    });
    assert!(!has_event(&db, 1, "run_e2e_finished"));
    fs::write(&release, "").unwrap();
    let outcome = joined(supervisor, "the supervisor to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let second = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_eq!(second.status(), RunStatus::Integrated);
    let landed = events_of(&db, second.id(), "run_integrated");
    let main = landed[0]["commit"].as_str().unwrap();

    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    let head = payloads(&detail, "validation_finished")[0]["receipt"]["commit"].clone();
    let found = payloads(&detail, "landing_recheck_failed");
    assert_eq!(found.len(), 2, "{:?}", event_kinds(&detail));
    let (held, parked) = (found[0], found[1]);
    assert_eq!(held["action"], "held");
    assert_eq!(held["code"], "rebase_conflict");
    assert_eq!(held["conflicts"], json!(["change.txt"]));
    assert_eq!(held["main"], main);
    assert_eq!(held["head"], head);
    assert_eq!(held["landed_task_id"], 2);
    assert_eq!(held["landed_run_id"], json!(second.id()));
    assert!(held.get("status").is_none(), "{held}");
    assert!(held.get("repeat").is_none(), "{held}");
    // Parked when it would land, against the same main and head.
    assert_eq!(parked["action"], "resumed");
    assert_eq!(parked["repeat"], true);
    assert_eq!(parked["status"], "needs_session");
    assert_eq!(parked["code"], "rebase_conflict");
    assert_eq!(parked["main"], main);
    assert_eq!(parked["head"], head);
    assert_eq!(parked["reason"], held["reason"]);
    let kinds = event_kinds(&detail);
    let first = |kind: &str| position(&kinds, kind);
    let at = |from: usize, kind: &str| from + position(&kinds[from..], kind);
    let recorded = first("landing_recheck_failed");
    let parked_at = at(recorded + 1, "landing_recheck_failed");
    // Held in its slot through its e2e, then parked, not landed.
    assert!(first("run_e2e_started") < recorded, "{kinds:?}");
    let e2e_passed = at(recorded, "run_e2e_finished");
    assert!(e2e_passed < parked_at, "{kinds:?}");
    assert_eq!(
        payloads(&detail, "run_e2e_finished")[0]["outcome"],
        "passed"
    );
    let resumed = at(parked_at, "resume_started");
    assert!(first("integration_started") > resumed, "{kinds:?}");
    assert_eq!(payloads(&detail, "resume_started")[0]["counted"], false);
    let recheck = &runtime::status(&db).unwrap()["landing_recheck"];
    assert_eq!(recheck["main"], main, "{recheck}");
    assert_eq!(recheck["checked"], 1);
    assert_eq!(recheck["conflicts"], 1);
    assert_eq!(recheck["held"], 1);
    assert_eq!(recheck["resumed"], 0);
    assert_eq!(
        recheck["failed_runs"],
        json!([{"run_id": run.id(), "code": "rebase_conflict", "action": "held"}])
    );
    // The resumed session brought it onto main, and it landed from there.
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(run.status(), RunStatus::Integrated);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        "resolved by the resumed session\n"
    );
}

/// A recheck command cut off by its deadline is the host's failure
/// (`timeout`), not `Finding::Error`: the finding is skipped and the run,
/// held from its landing by integrate's control of a host failure, keeps
/// its hold (its head, its `last_error` and the inbox's attention) and is
/// not resumed.
#[test]
fn a_recheck_command_cut_off_by_its_deadline_is_an_environmental_failure() {
    let (_dir, repo, db) = fixture();
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE tasks SET verification_commands=?1 WHERE id=1",
            [json!(["kill -TERM $$"]).to_string()],
        )
        .unwrap();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let before = run_of(&db, 1).unwrap();
    assert!(has_event(&db, 1, "integration_held"));
    // Outlives the deadline, and ends once the test (the shell's parent)
    // is gone or after a minute, however the test ends.
    move_main(
        &repo,
        "dagq.toml",
        "[recheck]\ncommand = 'i=0; while [ $i -lt 600 ] && kill -0 $PPID 2>/dev/null; do sleep 0.1; i=$((i+1)); done'\n",
    );
    let mut options = supervise_options(4, true);
    options.verification_timeout = Duration::from_millis(20);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_environmental_recheck(&db, "timeout");
    let after = run_of(&db, 1).unwrap();
    let found = events_of(&db, after.id(), "landing_recheck_failed");
    assert!(
        found[0]["evidence"]
            .as_str()
            .unwrap()
            .contains("limit and was killed")
    );
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["landing_recheck"]["errors"], 0);
    assert_eq!(after.result_commit(), before.result_commit());
    assert_eq!(after.last_error(), before.last_error());
    assert_eq!(events_of(&db, after.id(), "integration_held").len(), 1);
    assert!(
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["run_id"] == after.id().as_str() && a["kind"] == "integration_held")
    );
}

fn assert_environmental_recheck(db: &Path, class: &str) {
    let run = run_of(db, 1).unwrap();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(events_of(db, run.id(), "resume_started").is_empty());
    let found = events_of(db, run.id(), "landing_recheck_failed");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["action"], "skipped");
    assert_eq!(found[0]["class"], class);
    assert_eq!(found[0]["status"], "awaiting_integration");
    assert!(Path::new(found[0]["log_path"].as_str().unwrap()).is_file());
    assert!(!found[0]["evidence"].as_str().unwrap().is_empty());
    let status = runtime::status(db).unwrap();
    let counts = &status["landing_recheck"];
    assert_eq!(counts["environment"], 1, "{counts}");
    assert_eq!(counts["resumed"], 0);
    assert_eq!(counts["held"], 0);
    assert_eq!(counts["check_failed"], 0);
}

/// The queue's events of `kind`, oldest first.
fn queue_events(db: &Path, kind: &str) -> Vec<Value> {
    Connection::open(db)
        .unwrap()
        .prepare("SELECT payload FROM run_events WHERE kind=?1 ORDER BY id")
        .unwrap()
        .query_map([kind], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|p| serde_json::from_str(&p.unwrap()).unwrap())
        .collect()
}

/// Make the recheck's build output `name` (`<queue dir>/recheck/<name>`)
/// unwritable, or writable again.
fn output_writable(db: &Path, name: &str, writable: bool) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let target = db.parent().unwrap().join("recheck").join(name);
    fs::create_dir_all(&target).unwrap();
    let mode = if writable { 0o755 } else { 0o555 };
    fs::set_permissions(&target, fs::Permissions::from_mode(mode)).unwrap();
    target
}

/// The attention items `status` shows for an unwritable recheck target.
fn target_attention(db: &Path) -> Vec<Value> {
    runtime::status(db).unwrap()["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["kind"] == "landing_recheck_target_unwritable")
        .cloned()
        .collect()
}

/// The recheck cannot write a build output: the pass runs no command.
/// The runs whose command would run (no conflict, their diff on
/// `[recheck] paths`) get no record, are not resumed and keep waiting to
/// land, while in the same pass the run that conflicts is recorded and
/// resumed and the run off the paths is recorded clean as skipped by them,
/// its ask saying no command was run.
/// The unwritable output is the declaration's that applies only to the
/// last run: the first run, to which none applies, runs no command either,
/// since every output is checked before any command runs.
/// `landing_recheck_finished` says why no command ran and how many runs
/// went unrecorded, and the queue holds one attention with the path and
/// the OS's error.
#[test]
fn a_recheck_that_cannot_write_its_build_output_runs_no_command_and_records_no_run() {
    let (_dir, repo, db) = fixture();
    fs::write(
        repo.join("dagq.toml"),
        "[toolchains.core]\nprofile = \"rust\"\npaths = [\"**/*.rs\"]\n\n[recheck]\ncommand = 'exit 1'\npaths = ['*.txt', '**/*.rs']\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "the recheck"]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "off the paths", &[]);
    add_ready_task(&mut queue, "conflicting", &[]);
    add_ready_task(&mut queue, "rust", &[]);
    drop(queue);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.script_for(
        1,
        "printf 'a\\n' > a.txt && git add a.txt && git commit -q -m a; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    backend.script_for(
        2,
        "printf 'b\\n' > b.md && git add b.md && git commit -q -m b; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    backend.script_for(
        4,
        "printf 'fn f() {}\\n' > lib.rs && git add lib.rs && git commit -q -m rust; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    backend.resume_script_for(3, RESOLVING_RESUME);
    let concern = || verdict("concern", &["a person should look"], "unsure");
    let reviewer = TestReviewer::new(&[concern(), concern(), concern(), concern()]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let target = output_writable(&db, "target-core", false);
    let main = move_main(&repo, "change.txt", "changed on main by hand\n");

    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    output_writable(&db, "target-core", true);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    for task in [1, 4] {
        let unrecorded = queue.show(TaskId::new(task)).unwrap();
        assert_eq!(unrecorded.runs[0].status(), RunStatus::AwaitingIntegration);
        for kind in [
            "landing_recheck_failed",
            "landing_recheck_clean",
            "resume_started",
        ] {
            assert!(
                payloads(&unrecorded, kind).is_empty(),
                "task {task} {kind}: {:?}",
                event_kinds(&unrecorded)
            );
        }
    }
    let off_paths = queue.show(TaskId::new(2)).unwrap();
    let clean = payloads(&off_paths, "landing_recheck_clean");
    assert_eq!(clean.len(), 1, "{:?}", event_kinds(&off_paths));
    assert_eq!(clean[0]["command_skipped"], "paths");
    assert_eq!(clean[0]["command"], Value::Null);
    let off_paths_ask = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.run_id.as_ref() == Some(off_paths.runs[0].id()))
        .unwrap();
    assert!(
        off_paths_ask
            .question
            .ends_with("git merges it without a conflict (no command was run)."),
        "{}",
        off_paths_ask.question
    );
    let conflicting = queue.show(TaskId::new(3)).unwrap();
    let failed = payloads(&conflicting, "landing_recheck_failed");
    assert_eq!(failed.len(), 1, "{:?}", event_kinds(&conflicting));
    assert_eq!(failed[0]["code"], "rebase_conflict");
    assert_eq!(failed[0]["action"], "resumed");
    assert_eq!(payloads(&conflicting, "resume_started").len(), 1);

    let finished = queue_events(&db, "landing_recheck_finished");
    let finished = finished.last().unwrap();
    assert_eq!(finished["main"], json!(main));
    assert_eq!(finished["command_skipped_reason"], "target_unwritable");
    assert_eq!(finished["target_unwritable"], 2, "{finished}");
    assert_eq!(finished["command_skipped"], 1);
    assert_eq!(finished["conflicts"], 1);
    let told = queue_events(&db, "landing_recheck_target_unwritable");
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["main"], json!(main));
    assert_eq!(told[0]["unrecorded"], 2);
    let path = told[0]["path"].as_str().unwrap();
    assert!(
        Path::new(path).starts_with(fs::canonicalize(&target).unwrap()),
        "{path}"
    );
    assert!(
        told[0]["error"]
            .as_str()
            .unwrap()
            .contains("Permission denied"),
        "{}",
        told[0]
    );
    assert!(queue_events(&db, "landing_recheck_target_writable").is_empty());
    let attention = target_attention(&db);
    assert_eq!(attention.len(), 1, "{attention:?}");
    assert_eq!(attention[0]["next"], "fix the recheck target");
}

/// The queue's records of the recheck's command on main alone
/// (ADR-t2141-1), oldest first: each one's kind and payload.
fn main_checks(db: &Path) -> Vec<(String, Value)> {
    Connection::open(db)
        .unwrap()
        .prepare(
            "SELECT kind, payload FROM run_events WHERE kind LIKE 'landing_recheck_main_%' \
             AND run_id IS NULL ORDER BY id",
        )
        .unwrap()
        .query_map([], |row| {
            let payload: String = row.get(1)?;
            Ok((row.get(0)?, serde_json::from_str(&payload).unwrap()))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// The logs of main's checks in the recheck's directory `recheck_dir`,
/// by name.
fn main_logs(recheck_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(recheck_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("main-") && name.ends_with(".log"))
        .collect();
    names.sort();
    names
}

/// The path of the log of the check of `main` in `recheck_dir`.
fn kept_log(recheck_dir: &Path, main: &str) -> String {
    recheck_dir
        .join(format!("main-{}.log", &main[..12]))
        .display()
        .to_string()
}

/// A recheck command that fails on a tree with `broken.txt` (main broken
/// for every run) or with a `change.txt` a run wrote (the run's code).
const MAIN_OR_RUN_FAILS: &str = "[recheck]\ncommand = 'if [ -f broken.txt ] || grep -q \"change by\" change.txt; then echo \"error[E0063]: broken\"; exit 1; fi'\n";

/// A recheck command that fails on a tree with `broken.txt` alone.
const MAIN_BROKEN_FAILS: &str =
    "[recheck]\ncommand = 'if [ -f broken.txt ]; then echo \"error[E0063]: broken\"; exit 1; fi'\n";

/// Commit `files` on main by hand, outside dagq; the new main.
fn move_main_with(repo: &Path, files: &[(&str, Option<&str>)]) -> String {
    for (file, text) in files {
        match text {
            Some(text) => {
                fs::write(repo.join(file), text).unwrap();
                git(repo, &["add", file]);
            }
            None => git(repo, &["rm", "-q", file]),
        }
    }
    git(repo, &["commit", "-q", "-m", "main moves"]);
    git_out(repo, &["rev-parse", "main"])
}

/// The run's command fails for the code, and so does main's tree alone
/// (ADR-t2141-1): the run is neither parked nor held and uses no resume,
/// its finding is skipped with main's failure and log, and the queue
/// tells the inbox once. Main still failing on the next recheck tells it
/// nothing more; once main is fixed, the pass is recorded though no run
/// failed. That a main that breaks again is told again is the unit test's
/// of `MainCheck::transition`; that a run that fails on a main that passes
/// is resumed is `a_waiting_run_whose_check_fails_on_the_new_main_is_resumed`'s.
#[test]
fn a_recheck_whose_command_fails_on_main_alone_too_resumes_no_run_and_tells_the_inbox_once() {
    let (dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let (waiting, _ask, reviewer) = one_waiting(&backend, &repo, &db);
    let main = move_main_with(
        &repo,
        &[
            ("dagq.toml", Some(MAIN_OR_RUN_FAILS)),
            ("broken.txt", Some("broken\n")),
        ],
    );
    // An earlier check's log nobody points at goes; one that cannot be
    // removed (a directory) is warned of and judges nothing; what is not
    // a log of main's check stays.
    let recheck_dir = fs::canonicalize(dir.path()).unwrap().join("recheck");
    fs::create_dir_all(recheck_dir.join("main-feedfacecafe.log")).unwrap();
    fs::write(recheck_dir.join("main-0123456789ab.log"), "old\n").unwrap();
    fs::write(recheck_dir.join("notes.txt"), "kept\n").unwrap();
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut left = vec![
        format!("main-{}.log", &main[..12]),
        "main-feedfacecafe.log".to_owned(),
    ];
    left.sort();
    assert_eq!(main_logs(&recheck_dir), left);
    assert!(recheck_dir.join("notes.txt").is_file());
    assert!(recheck_dir.join("lock").exists());
    assert!(recheck_scratch(&recheck_dir).is_dir());
    fs::remove_dir(recheck_dir.join("main-feedfacecafe.log")).unwrap();
    let run = run_of(&db, 1).unwrap();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert_eq!(run.result_commit(), waiting.result_commit());
    assert!(events_of(&db, run.id(), "resume_started").is_empty());
    let found = events_of(&db, run.id(), "landing_recheck_failed");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["action"], "skipped");
    assert_eq!(found[0]["code"], "verification_failed");
    assert_eq!(found[0]["main_failed"], true);
    let main_log = fs::canonicalize(dir.path())
        .unwrap()
        .join("recheck")
        .join(format!("main-{}.log", &main[..12]));
    assert_eq!(
        found[0]["main_log_path"],
        json!(main_log.display().to_string())
    );
    assert!(
        fs::read_to_string(&main_log)
            .unwrap()
            .contains("error[E0063]: broken")
    );
    let checks = main_checks(&db);
    assert_eq!(checks.len(), 1, "{checks:?}");
    let (kind, payload) = &checks[0];
    assert_eq!(kind, "landing_recheck_main_failed");
    assert_eq!(payload["main"], json!(main));
    assert_eq!(payload["log_path"], found[0]["main_log_path"]);
    assert_eq!(payload["class"], found[0]["class"]);
    assert_eq!(payload["exit_code"], 1);
    assert_eq!(payload["failed_runs"], 1);
    assert_eq!(
        dagq::domain::event_attention(&EventKind::LandingRecheckMainFailed.into(), payload)
            .map(|next| next.to_string()),
        Some("fix main or the recheck environment".to_owned())
    );
    let counts = &runtime::status(&db).unwrap()["landing_recheck"];
    assert_eq!(counts["main_check"], "failed", "{counts}");
    assert_eq!(counts["main_failed"], 1);
    assert_eq!(counts["check_failed"], 1);
    assert_eq!(counts["resumed"], 0);
    assert_eq!(counts["held"], 0);

    // Main moves and is still broken: the run is skipped again, and the
    // inbox is not told again. The attention's log and the new one, which
    // the ask points at, are kept.
    let still = move_main_with(&repo, &[("other.txt", Some("other\n"))]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut kept = vec![
        format!("main-{}.log", &main[..12]),
        format!("main-{}.log", &still[..12]),
    ];
    kept.sort();
    assert_eq!(main_logs(&recheck_dir), kept);
    let question = &SqliteQueue::open(&db)
        .unwrap()
        .asks(Default::default())
        .unwrap()[0]
        .question;
    assert!(
        question.contains(&kept_log(&recheck_dir, &still)),
        "{question}"
    );
    let run = run_of(&db, 1).unwrap();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(events_of(&db, run.id(), "resume_started").is_empty());
    let found = events_of(&db, run.id(), "landing_recheck_failed");
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[1]["main_failed"], true);
    assert_eq!(main_checks(&db).len(), 1);

    // Main is fixed and the run passes the command: no run failed it, but
    // with the attention open main is checked too, and its pass recorded.
    // Nobody points at the failures' logs any more.
    let fixed = move_main_with(
        &repo,
        &[("dagq.toml", Some(MAIN_BROKEN_FAILS)), ("broken.txt", None)],
    );
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(
        main_logs(&recheck_dir),
        [format!("main-{}.log", &fixed[..12])]
    );
    let run = run_of(&db, 1).unwrap();
    assert_eq!(events_of(&db, run.id(), "landing_recheck_failed").len(), 2);
    assert_eq!(events_of(&db, run.id(), "landing_recheck_clean").len(), 1);
    assert_eq!(status_main_check(&db), "passed");
    let checks = main_checks(&db);
    assert_eq!(
        checks
            .iter()
            .map(|(kind, _)| kind.as_str())
            .collect::<Vec<_>>(),
        ["landing_recheck_main_failed", "landing_recheck_main_passed"]
    );
    assert_eq!(
        dagq::domain::event_attention(&EventKind::LandingRecheckMainPassed.into(), &checks[1].1),
        None
    );
}
