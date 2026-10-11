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
/// counting the resume. The rebased run is not reviewed again: it waits
/// for the answer, which the ask's question now explains, and `land` lands
/// it. `status` and `stats` show the recheck, and `stats` the landing in
/// the group of runs that waited after their review, with its conflict.
#[test]
fn a_waiting_run_that_a_landing_conflicts_with_is_resumed_before_its_answer() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let (waiting, ask, reviewer) = waiting_then_landing(&backend, &repo, &db);
    backend.resume_script_for(1, RESOLVING_RESUME);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let second = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(second.runs[0].status(), RunStatus::Integrated);
    let main = git_out(&repo, &["rev-parse", "main"]);

    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let found = payloads(&detail, "landing_recheck_failed");
    assert_eq!(found.len(), 1, "{:?}", event_kinds(&detail));
    let found = found[0];
    assert_eq!(found["code"], "rebase_conflict");
    assert_eq!(found["action"], "resumed");
    assert_eq!(found["status"], "needs_session");
    assert_eq!(found["main"], json!(main));
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
    // After the recheck: the note on the ask, the resume, and a
    // validation of the rewritten receipt with no review after it.
    let kinds = event_kinds(&detail);
    let after: Vec<&str> = kinds
        .iter()
        .skip_while(|k| **k != "landing_recheck_failed")
        .filter(|k| {
            matches!(
                **k,
                "landing_recheck_failed"
                    | "ask_updated"
                    | "resume_started"
                    | "resume_finished"
                    | "validation_finished"
                    | "review_started"
                    | "exit_requested"
                    | "workspace_closed"
                    | "lease_released"
            )
        })
        .copied()
        .collect();
    assert_eq!(
        after,
        [
            "landing_recheck_failed",
            "ask_updated",
            "resume_started",
            "resume_finished",
            "validation_finished",
            "exit_requested",
            "workspace_closed",
            "lease_released",
        ]
    );
    let text = &session_texts(&run)[0];
    assert!(
        text.contains("the supervisor's landing recheck found that it no longer lands"),
        "{text}"
    );
    assert!(text.contains(&format!("Reason: {reason}")), "{text}");
    // The rebased run waits for the answer instead of a third review.
    assert_eq!(reviewer.prompts().len(), 2);
    assert_ne!(run.result_commit(), waiting.result_commit());
    assert_eq!(
        git_out(
            &repo,
            &["rev-parse", &format!("{}~1", run.result_commit().unwrap())]
        ),
        main
    );
    assert!(queue.run_leases().unwrap().is_empty());
    let noted = queue.read_ask(ask.id).unwrap();
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
    assert_eq!(recheck["main"], json!(main));
    assert_eq!(recheck["landed_task_id"], 2);
    assert_eq!(recheck["checked"], 1);
    assert_eq!(recheck["conflicts"], 1);
    assert_eq!(recheck["resumed"], 1);
    assert_eq!(recheck["command"], Value::Null);
    assert_eq!(
        recheck["failed_runs"],
        json!([{"run_id": run.id(), "code": "rebase_conflict", "action": "resumed"}])
    );
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

    queue.answer(ask.id, "land").unwrap();
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        "resolved by the resumed session\n"
    );
    assert_eq!(reviewer.prompts().len(), 2);

    // The run that waited in its `approve_landing` ask landed after the
    // recheck found its conflict; the landing that moved main did not wait.
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

/// A conflict Git does not see: the waiting run adds `a.txt`, which needs
/// `b.txt`, and the landing run deletes `b.txt`. `git merge-tree` merges
/// them cleanly, but the `[recheck] command` fails on main's tree with the
/// run merged in, in the queue's scratch worktree with its one target
/// directory; the run is parked and resumed with the command's failure,
/// and that resume counts.
#[test]
fn a_waiting_run_whose_check_fails_on_the_new_main_is_resumed() {
    let (dir, repo, db) = fixture();
    fs::write(repo.join("b.txt"), "b\n").unwrap();
    fs::write(
        repo.join("dagq.toml"),
        "[recheck]\ncommand = 'echo \"target=$CARGO_TARGET_DIR\"; if [ -f a.txt ]; then test -f b.txt || { echo error[E0063]: missing_b; exit 1; }; fi'\n",
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
    let log = Path::new(found["log_path"].as_str().unwrap());
    assert!(log.starts_with(waiting.run_dir().unwrap()), "{log:?}");
    assert_eq!(
        log.file_name().unwrap().to_str().unwrap(),
        format!("recheck-{}.log", &main[..12])
    );
    assert!(recheck_dir.join("worktree").join("a.txt").is_file());
    assert!(!recheck_dir.join("worktree").join("b.txt").exists());
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

/// Commit `[recheck]` with a command that always fails and `paths`.
fn failing_recheck_with_paths(repo: &Path, paths: &str) {
    fs::write(
        repo.join("dagq.toml"),
        format!("[recheck]\ncommand = 'exit 1'\npaths = {paths}\n"),
    )
    .unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-q", "-m", "the recheck"]);
}

/// With `[recheck] paths` that the waiting run's diff against main touches
/// none of (ADR-t2032-1), the recheck checks the merge only: the command,
/// which would fail, is not run, no scratch worktree is checked out, and
/// the run is recorded clean with a null command, told apart as skipped
/// by the paths, and its ask says no command was run.
#[test]
fn a_waiting_run_whose_diff_misses_the_recheck_paths_runs_no_command() {
    let (dir, repo, db) = fixture();
    failing_recheck_with_paths(&repo, "['**/*.rs', 'Cargo.lock']");
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.script_for(
        2,
        "printf 'fn f() {}\\n' > lib.rs && git add lib.rs && git commit -q -m rust; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let (waiting, ask, reviewer) = waiting_then_landing(&backend, &repo, &db);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let second = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_eq!(second.status(), RunStatus::Integrated);
    let main = git_out(&repo, &["rev-parse", "main"]);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(payloads(&detail, "landing_recheck_failed").is_empty());
    assert_eq!(
        payloads(&detail, "landing_recheck_clean"),
        [&json!({
            "main": main,
            "head": waiting.result_commit().unwrap(),
            "command": null,
            "command_skipped": "paths",
            "landed_run_id": second.id(),
            "landed_task_id": 2,
        })]
    );
    let question = queue.read_ask(ask.id).unwrap().question;
    assert!(
        question.ends_with("git merges it without a conflict (no command was run)."),
        "{question}"
    );
    let recheck_dir = fs::canonicalize(dir.path()).unwrap().join("recheck");
    assert!(!recheck_dir.join("worktree").exists());
    let recheck = &runtime::status(&db).unwrap()["landing_recheck"];
    assert_eq!(recheck["command"], "exit 1", "{recheck}");
    assert_eq!(recheck["clean"], 1);
    assert_eq!(recheck["command_skipped"], 1);
    assert_eq!(recheck["check_failed"], 0);
}

/// With `[recheck] paths` that the waiting run's diff against main touches,
/// the command runs as without them: here it fails, and the run is parked
/// and resumed with the command's failure.
#[test]
fn a_waiting_run_whose_diff_touches_the_recheck_paths_runs_the_command() {
    let (_dir, repo, db) = fixture();
    failing_recheck_with_paths(&repo, "['**/*.rs', '*.txt']");
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.script_for(
        2,
        "printf 'other\\n' > other.md && git add other.md && git commit -q -m other; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    backend.resume_script_for(1, "await_message; idle; await_exit");
    let (_waiting, _ask, reviewer) = waiting_then_landing(&backend, &repo, &db);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(payloads(&detail, "landing_recheck_clean").is_empty());
    let found = payloads(&detail, "landing_recheck_failed");
    assert_eq!(found.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(found[0]["code"], "verification_failed");
    assert_eq!(found[0]["command"], "exit 1");
    let recheck = &runtime::status(&db).unwrap()["landing_recheck"];
    assert_eq!(recheck["check_failed"], 1, "{recheck}");
    assert_eq!(recheck["command_skipped"], 0);
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

/// Main moves twice while a run waits, and both rechecks find it still
/// landing: each records `landing_recheck_clean`, and the ask's recheck
/// paragraph is the latest main's alone (ADR-t1311-1).
#[test]
fn a_second_clean_recheck_replaces_the_asks_recheck_paragraph() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let (waiting, ask, reviewer) = one_waiting(&backend, &repo, &db);
    let first = move_main(&repo, "one.txt", "one\n");
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let question = queue.read_ask(ask.id).unwrap().question;
    assert_eq!(
        recheck_paragraphs(&question),
        [format!(
            "Landing recheck: after main moved without a dagq landing, the landing recheck found that the run still lands cleanly on main {}: git merges it without a conflict (no command was run).",
            &first[..12]
        )]
    );

    let second = move_main(&repo, "two.txt", "two\n");
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    let clean = payloads(&detail, "landing_recheck_clean");
    assert_eq!(clean.len(), 2, "{:?}", event_kinds(&detail));
    assert_eq!(clean[0]["main"], json!(first));
    assert_eq!(clean[1]["main"], json!(second));
    assert_eq!(clean[1]["head"], json!(waiting.result_commit().unwrap()));
    let question = queue.read_ask(ask.id).unwrap().question;
    let paragraphs = recheck_paragraphs(&question);
    assert_eq!(paragraphs.len(), 1, "{question}");
    assert!(paragraphs[0].contains(&second[..12]), "{question}");
    assert!(!question.contains(&first[..12]), "{question}");
    assert!(question.starts_with(&ask.question), "{question}");
    assert_eq!(payloads(&detail, "ask_updated").len(), 2);
}

/// A run found clean, then in conflict after main moves again: the ask's
/// recheck paragraph becomes the failure's alone, and the run is resumed as
/// before (`landing_recheck_failed` with `action: resumed`).
#[test]
fn a_conflict_after_a_clean_recheck_replaces_the_asks_recheck_paragraph() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let (waiting, ask, reviewer) = one_waiting(&backend, &repo, &db);
    let first = move_main(&repo, "one.txt", "one\n");
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        payloads(
            &queue.show(TaskId::new(1)).unwrap(),
            "landing_recheck_clean"
        )
        .len(),
        1
    );

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
    assert_eq!(payloads(&detail, "resume_started").len(), 1);
    let reason = found[0]["reason"].as_str().unwrap();
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

/// Main moves while task 1 waits, without a dagq landing (a push outside
/// dagq), and no supervisor runs meanwhile: the next supervisor to start,
/// which never saw the move, rechecks the waiting run on its first pass.
/// With no dagq landing behind main, the landed run and task are null and
/// `landing_recheck_finished` goes on the run it checked.
#[test]
fn a_new_supervisor_rechecks_against_a_main_that_moved_before_it_started() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let (waiting, ask, reviewer) = one_waiting(&backend, &repo, &db);
    fs::write(repo.join("change.txt"), "changed on main by hand\n").unwrap();
    git(&repo, &["add", "change.txt"]);
    git(&repo, &["commit", "-q", "-m", "by hand"]);
    let main = git_out(&repo, &["rev-parse", "main"]);

    backend.resume_script_for(1, RESOLVING_RESUME);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let found = payloads(&detail, "landing_recheck_failed");
    assert_eq!(found.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(found[0]["code"], "rebase_conflict");
    assert_eq!(found[0]["action"], "resumed");
    assert_eq!(found[0]["main"], json!(main));
    assert_eq!(found[0]["landed_task_id"], Value::Null);
    assert_eq!(found[0]["landed_run_id"], Value::Null);
    let reason = found[0]["reason"].as_str().unwrap();
    assert!(
        reason.starts_with("after main moved without a dagq landing"),
        "{reason}"
    );
    let finished = events_of(&db, waiting.id(), "landing_recheck_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["main"], json!(main));
    assert_eq!(finished[0]["landed_run_id"], Value::Null);
    assert_eq!(finished[0]["checked"], 1);
    assert!(
        queue
            .read_ask(ask.id)
            .unwrap()
            .question
            .contains(&format!("Landing recheck: {reason}."))
    );
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
    crate::runtime_e2e::e2e_options(format!(
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
    crate::runtime_e2e::with_e2e_paths(&repo, "[\"e2e.txt\"]");
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

/// Another supervisor's recheck of a main settles only the runs it could
/// see. Here this supervisor holds task 1's run in its slot to land (it
/// waits for its e2e) when main moves outside it and into the run; before
/// this supervisor can look (the test holds the recheck lock), another
/// supervisor records its recheck of that main, which could not see the
/// held run. This supervisor still checks the run it holds against that
/// main: the conflict holds it, and it is parked instead of landing.
/// Moved from the interactive
/// `another_supervisors_recheck_of_main_leaves_the_runs_this_one_holds_to_check`,
/// which held the run with its session's `/exit` and which task 1437
/// deleted.
#[test]
fn another_supervisors_recheck_of_main_leaves_the_run_held_for_its_e2e_to_check() {
    let (dir, repo, db) = fixture();
    crate::runtime_e2e::with_e2e_paths(&repo, "[\"e2e.txt\"]");
    let backend = Arc::new(TestWorkspace::new(&db, false, E2E_AGENT));
    backend.resume_script_for(1, RESOLVING_RESUME);
    let reviewer = Arc::new(TestReviewer::new(&[verdict(
        "pass",
        &[],
        "meets the acceptance",
    )]));
    let release = dir.path().join("e2e-release");
    let supervisor = supervising_with(&db, &repo, &backend, &reviewer, held_e2e(&release));
    wait_until(&db, Duration::from_secs(60), |_| {
        has_event(&db, 1, "run_e2e_started")
    });
    let held = run_of(&db, 1).unwrap();
    let head = held.result_commit().unwrap().clone();
    // Main moves into the held run while another supervisor's recheck
    // holds the lock, and that recheck records main as checked.
    let lock = hold_recheck_lock(&db);
    fs::write(repo.join("change.txt"), "main moved\n").unwrap();
    git(&repo, &["add", "change.txt"]);
    git(&repo, &["commit", "-q", "-m", "main moves"]);
    let main = git_out(&repo, &["rev-parse", "main"]);
    SqliteQueue::open(&db)
        .unwrap()
        .record_runtime_event(
            held.id(),
            dagq::domain::EventKind::LandingRecheckFinished,
            json!({
                "main": main,
                "landed_run_id": null,
                "landed_task_id": null,
                "command": null,
                "checked": 0,
                "clean": 0,
                "conflicts": 0,
                "check_failed": 0,
                "errors": 0,
                "resumed": 0,
                "held": 0,
                "failed_runs": [],
                "duration_secs": 0,
                "supervisor": "another-supervisor",
            }),
        )
        .unwrap();
    drop(lock);
    wait_until(&db, Duration::from_secs(60), |_| {
        !events_of(&db, held.id(), "landing_recheck_failed").is_empty()
    });
    fs::write(&release, "").unwrap();
    let outcome = joined(supervisor, "the supervisor to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let found = events_of(&db, held.id(), "landing_recheck_failed");
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[0]["action"], "held");
    assert_eq!(found[0]["code"], "rebase_conflict");
    assert_eq!(found[0]["main"], json!(main));
    assert_eq!(found[0]["head"], json!(head));
    // Parked when it would land, not landed onto that main.
    assert_eq!(found[1]["action"], "resumed");
    assert_eq!(found[1]["repeat"], true);
    let finished = events_of(&db, held.id(), "landing_recheck_finished");
    assert_eq!(finished.len(), 2, "{finished:?}");
    let own = &finished[1];
    assert_ne!(own["supervisor"], "another-supervisor");
    assert_eq!(own["main"], json!(main));
    assert_eq!(own["checked"], 1);
    assert_eq!(own["held"], 1);
    // Resumed onto main and landed from there.
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        "resolved by the resumed session\n"
    );
}

/// The real integration hold survives a later recheck's host failure,
/// including its last_error and the inbox attention that reports the hold.
#[test]
fn an_integration_held_run_is_not_resumed_by_a_full_disk_recheck() {
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
    move_main(
        &repo,
        "dagq.toml",
        "[recheck]\ncommand = 'echo No space left on device; exit 1'\n",
    );
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_environmental_recheck(&db, "disk_full");
    let after = run_of(&db, 1).unwrap();
    assert_eq!(after.result_commit(), before.result_commit());
    assert_eq!(after.last_error(), before.last_error());
    assert_eq!(events_of(&db, after.id(), "integration_held").len(), 1);
    assert!(
        runtime::status(&db).unwrap()["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["run_id"] == after.id().as_str() && a["kind"] == "integration_held")
    );
}

/// A verifier's deadline is recorded as timeout, rather than Finding::Error.
#[test]
fn a_recheck_command_cut_off_by_its_deadline_is_an_environmental_failure() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("concern", &["a person should look"], "unsure")]);
    supervise_reviewed(&db, &repo, &backend, &reviewer);
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
    let waiting = run_of(&db, 1).unwrap();
    let found = events_of(&db, waiting.id(), "landing_recheck_failed");
    assert!(
        found[0]["evidence"]
            .as_str()
            .unwrap()
            .contains("limit and was killed")
    );
    assert_eq!(
        runtime::status(&db).unwrap()["landing_recheck"]["errors"],
        0
    );
    let ask = SqliteQueue::open(&db)
        .unwrap()
        .asks(Default::default())
        .unwrap()[0]
        .clone();
    assert!(
        ask.question
            .contains("The host kept the command from judging the code")
    );
    assert!(!ask.question.contains("The supervisor resumes"));
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

/// A slot retains its lease and can land after its e2e: the skipped
/// finding must not become a held finding that park_held_by_recheck uses.
#[test]
fn a_slot_run_with_a_killed_recheck_is_not_parked_before_landing() {
    let (dir, repo, db) = fixture();
    crate::runtime_e2e::with_e2e_paths(&repo, "[\"e2e.txt\"]");
    let mut config = fs::read_to_string(repo.join("dagq.toml")).unwrap();
    config.push_str("\n[recheck]\ncommand = 'kill -TERM $$'\n");
    move_main(&repo, "dagq.toml", &config);
    let backend = Arc::new(TestWorkspace::new(&db, false, E2E_AGENT));
    let reviewer = Arc::new(TestReviewer::new(&[verdict(
        "pass",
        &[],
        "meets the acceptance",
    )]));
    let release = dir.path().join("e2e-release");
    let supervisor = supervising_with(&db, &repo, &backend, &reviewer, held_e2e(&release));
    wait_until(&db, Duration::from_secs(60), |_| {
        has_event(&db, 1, "run_e2e_started")
    });
    move_main(&repo, "other.txt", "unrelated change\n");
    wait_until(&db, Duration::from_secs(60), |_| {
        runtime::status(&db).unwrap()["landing_recheck"]["environment"] == 1
    });
    assert_environmental_recheck(&db, "killed");
    let run = run_of(&db, 1).unwrap();
    assert!(
        SqliteQueue::open(&db)
            .unwrap()
            .run_lease(run.id())
            .unwrap()
            .is_some()
    );
    fs::write(&release, "").unwrap();
    let outcome = joined(supervisor, "the supervisor to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = run_of(&db, 1).unwrap();
    assert_eq!(run.status(), RunStatus::Integrated);
    assert!(events_of(&db, run.id(), "resume_started").is_empty());
    assert_eq!(events_of(&db, run.id(), "landing_recheck_failed").len(), 1);
}
