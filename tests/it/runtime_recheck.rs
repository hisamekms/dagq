//! Runtime tests: the landing recheck of the runs that wait to land
//! (ADR-0068). After a landing moves main, a run waiting for a person's
//! answer is checked against it (`git merge-tree`, then the `[recheck]
//! command` of `dagq.toml`), and one that no longer lands is resumed
//! without waiting for the answer, which its ask then tells the person.
//! A run the supervisor holds in a slot to land is parked when it would
//! land instead.
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
/// it. `status` and `stats` show the recheck.
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
    let text = &backend.texts()[0].1;
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
        "[recheck]\ncommand = 'echo \"target=$CARGO_TARGET_DIR\"; if [ -f a.txt ]; then test -f b.txt; fi'\n",
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
    let text = &backend.texts()[0].1;
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
        "echo \"target=$CARGO_TARGET_DIR\"; if [ -f a.txt ]; then test -f b.txt; fi"
    );
}

/// A waiting run that still lands on the new main is left as it is: the
/// recheck is recorded on the queue as clean, and nothing on the run.
#[test]
fn a_waiting_run_that_still_lands_is_left_waiting() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.script_for(
        2,
        "printf 'other\\n' > other.txt && git add other.txt && git commit -q -m other; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let (waiting, ask, reviewer) = waiting_then_landing(&backend, &repo, &db);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert_eq!(detail.runs[0].result_commit(), waiting.result_commit());
    assert!(payloads(&detail, "landing_recheck_failed").is_empty());
    assert!(payloads(&detail, "resume_started").is_empty());
    assert_eq!(queue.read_ask(ask.id).unwrap().question, ask.question);
    let recheck = &runtime::status(&db).unwrap()["landing_recheck"];
    assert_eq!(recheck["checked"], 1, "{recheck}");
    assert_eq!(recheck["clean"], 1);
    assert_eq!(recheck["failed_runs"], json!([]));
    queue.answer(ask.id, "land").unwrap();
    supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Completed
    );
}

/// A run this supervisor holds in its slot to land (its session holds the
/// `/exit` back after the review passed) conflicts with the run that lands
/// meanwhile: the recheck after that landing records `landing_recheck_failed`
/// with `action: held` and leaves it in the slot. Once its session exits,
/// the run is parked instead of landing (`repeat: true`, `needs_session`,
/// no integration tried) and resumed with the recheck's request.
#[test]
fn a_run_held_in_its_slot_that_a_landing_conflicts_with_is_parked_before_it_lands() {
    let (_dir, repo, db) = fixture();
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "second", &[]);
    let backend = Arc::new(TestWorkspace::new(&db, false, HELD_AGENT));
    backend.script_for(2, PROMPTED_AGENT);
    backend.resume_script_for(1, RESOLVING_RESUME);
    let reviewer = Arc::new(TestReviewer::new(&[verdict(
        "pass",
        &[],
        "meets the acceptance",
    )]));
    let run_of = |task: i64| {
        SqliteQueue::open(&db)
            .unwrap()
            .show(TaskId::new(task))
            .unwrap()
            .runs
            .first()
            .cloned()
    };
    let has_event = |task: i64, kind: &str| {
        run_of(task).is_some_and(|run| !events_of(&db, run.id(), kind).is_empty())
    };
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
                &supervise_options(4, true),
            )
        })
    };
    // Task 1 is reviewed and waits in its slot for its session's exit
    // before task 2's session commits anything.
    wait_until(&db, Duration::from_secs(60), |_| {
        has_event(1, "exit_requested") && run_of(2).is_some()
    });
    let second = run_of(2).unwrap();
    fs::write(
        exit_request_path(second.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    wait_until(&db, Duration::from_secs(60), |_| {
        has_event(1, "landing_recheck_failed")
    });
    release_held_session(run_of(1).unwrap().run_dir().unwrap());
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
    assert!(first("exit_requested") < recorded, "{kinds:?}");
    // Held in its slot until its session exited, then parked, not landed.
    let exited = at(recorded, "session_exited");
    let released = at(exited, "lease_released");
    assert!(released < parked_at, "{kinds:?}");
    assert_eq!(
        payloads(&detail, "lease_released")[0],
        &json!({"reason": "landing_recheck_failed"})
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
