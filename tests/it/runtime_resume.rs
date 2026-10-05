//! Runtime tests: Runs that need a session: conflicts and the resumed sessions.
use crate::runtime_support;
use dagq::domain::EventKind;
use dagq::domain::LeaseToken;
use dagq::infrastructure::git_binary::git_executable;

use runtime_support::*;

/// Both runs change the same file: the second cannot be rebased by the
/// runtime and waits for a session, which resolves, reruns verification and
/// rewrites the receipt; then it lands like any other run.
#[test]
fn conflicting_run_needs_a_session_and_lands_after_the_session_resolves_it() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let first_landed = git_out(&repo, &["rev-parse", "main"]);

    let run = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    let source = run.result_commit().cloned().unwrap();
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    assert_eq!(outcome["main"], json!(first_landed));
    let reason = outcome["reason"].as_str().unwrap();
    assert!(reason.contains("conflicted in change.txt"), "{reason}");
    assert!(
        reason.contains(&format!("git rebase {first_landed}")),
        "{reason}"
    );
    let parked = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_eq!(parked.status(), RunStatus::NeedsSession);
    assert_eq!(parked.last_error(), Some(reason));
    assert_eq!(
        parked.result_commit().map(CommitSha::as_str),
        Some(source.as_str())
    );
    // A parked run is reviewable against its own base.
    let review = runtime::review(&db, TaskId::new(2)).unwrap();
    assert_eq!(review["head"], json!(source));
    assert_eq!(review["base"], json!(seed));
    // The rebase was aborted: the worktree is back on its validated head, clean.
    assert_eq!(git_out(&worktree, &["rev-parse", "HEAD"]), source);
    assert_eq!(git_out(&worktree, &["status", "--porcelain"]), "");
    assert!(!worktree.join(".git").join("rebase-merge").exists());
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);
    let detail = queue.show(TaskId::new(2)).unwrap();
    let deferred = detail
        .events
        .iter()
        .find(|e| e.kind == "integration_deferred")
        .unwrap();
    assert_eq!(deferred.payload["status"], "needs_session");
    assert_eq!(deferred.payload["code"], "rebase_conflict");
    assert_eq!(deferred.payload["conflicts"], json!(["change.txt"]));
    assert_eq!(deferred.payload["aborted"], true);
    assert!(
        deferred.payload["output_tail"]
            .as_str()
            .unwrap()
            .contains("CONFLICT")
    );
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert!(queue.run_leases().unwrap().is_empty());
    // A parked run still owns its task and is not picked by --next.
    assert!(
        queue
            .transition(TaskId::new(2), TaskAction::BypassReview)
            .is_err()
    );
    assert_eq!(integrate_next(&db, &repo)["outcome"], "no_run_awaiting");
    assert!(queue.candidates().unwrap().is_empty());
    // Listed while it waits for its session (goal 98), not for `recover`.
    let doctor = runtime::doctor(&db, true).unwrap();
    let listed = doctor["runs"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "{doctor}");
    assert_eq!(listed[0]["status"], "needs_session");
    assert_eq!(listed[0]["recoverable"], false);

    // Nothing changed in the worktree: the runtime tries again and parks it again.
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");

    // The session resolves the conflict on top of main.
    let rebase = Command::new(git_executable().expect("git executable"))
        .arg("-C")
        .arg(&worktree)
        .args(["rebase", &first_landed])
        .bounded_output()
        .unwrap();
    assert!(!rebase.status.success());
    fs::write(worktree.join("change.txt"), "resolved by the session\n").unwrap();
    git(&worktree, &["add", "change.txt"]);
    let status = Command::new(git_executable().expect("git executable"))
        .arg("-C")
        .arg(&worktree)
        .env("GIT_EDITOR", "true")
        .args(["rebase", "--continue"])
        .bounded_status()
        .unwrap();
    assert!(status.success());
    let resolved = git_out(&worktree, &["rev-parse", "HEAD"]);
    assert_ne!(resolved, source);

    // Until the receipt names the new head, the session is not done.
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let reason = outcome["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!("receipt commit {source} is not the head")),
        "{reason}"
    );
    assert_eq!(git_out(&worktree, &["rev-parse", "HEAD"]), resolved); // Left as the session made it.
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);

    // Every receipt that passed its checks was recorded, even when the
    // landing then stopped: the stale one names the validated head.
    let detail = queue.show(TaskId::new(2)).unwrap();
    let recorded = integration_receipts(&detail);
    assert_eq!(recorded.len(), 3, "{recorded:?}");
    assert!(recorded.iter().all(|p| p["commit"] == json!(source)));
    assert!(recorded.iter().all(|p| p["main"] == json!(first_landed)));

    let mut rewritten = session_receipt(&parked, &resolved, "succeeded", "resolved");
    rewritten["tests"]["evidence_or_reason"] = json!("cargo test after the rebase: 12 passed");
    rewritten["follow_ups"] = json!([
        {"title": "dedupe change.txt", "description": "both tasks wrote it"}
    ]);
    write_receipt_json(&parked, rewritten.clone());
    // The session's head sits on the landed main, so the review is taken
    // against that main and leaves out the first task's landing.
    let review = runtime::review(&db, TaskId::new(2)).unwrap();
    assert_eq!(review["base"], json!(first_landed));
    assert_eq!(review["head"], json!(resolved));
    assert_eq!(review["files_changed"], json!(1));
    let text = fs::read_to_string(review["path"].as_str().unwrap()).unwrap();
    assert!(text.contains("+resolved by the session"), "{text}");
    let commits = &text[text.find("## Commits").unwrap()..text.find("## Diffstat").unwrap()];
    let listed = &commits[commits.find("```").unwrap()..];
    assert_eq!(listed.matches(" work\n").count(), 1, "{commits}");
    assert!(!listed.contains(&first_landed[..7]), "{commits}");
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    // The session rebased the branch itself, so this rebase is a no-op; the
    // verification commands run here all the same, as on every landing.
    assert_eq!(outcome["verification_skipped"], json!(false), "{outcome}");
    let landed = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_landed(&repo, &landed, "second", &first_landed);
    // The receipt the session rewrote is what the DB keeps for the landing,
    // while validation_finished still holds the one from before the conflict.
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert!(!event_kinds(&detail).contains(&"integration_verification_skipped"));
    let rebased = detail
        .events
        .iter()
        .rev()
        .find(|e| e.kind == "integration_rebased")
        .unwrap();
    assert_eq!(rebased.payload["head_before"], json!(resolved));
    assert_eq!(rebased.payload["head_after"], json!(resolved));
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(verifications[0]["exit_code"], 0);
    let recorded = integration_receipts(&detail);
    assert_eq!(recorded.len(), 4, "{recorded:?}");
    let last = recorded[3];
    assert_eq!(last["commit"], json!(resolved));
    assert_eq!(last["main"], json!(first_landed));
    assert_eq!(last["receipt"], rewritten);
    assert_eq!(last["receipt"]["commit"], json!(resolved));
    assert_eq!(
        last["receipt"]["tests"]["evidence_or_reason"],
        json!("cargo test after the rebase: 12 passed")
    );
    assert_eq!(
        last["receipt"]["follow_ups"],
        json!([{"title": "dedupe change.txt", "description": "both tasks wrote it"}])
    );
    let validated = detail
        .events
        .iter()
        .find(|e| e.kind == "validation_finished")
        .unwrap();
    assert_eq!(validated.payload["receipt"]["commit"], json!(source));
    assert!(validated.payload["receipt"].get("follow_ups").is_none());
    assert_eq!(
        git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", run.id())]
        ),
        resolved
    );
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        "resolved by the session\n"
    );
    assert_eq!(
        git_out(&repo, &["rev-list", "--count", &format!("{seed}..main")]),
        "2"
    );
    assert_eq!(
        git_out(&repo, &["log", "-1", "--format=%b", "main"])
            .lines()
            .next(),
        Some("resolved")
    );
    assert_eq!(
        queue.show(TaskId::new(2)).unwrap().task.status(),
        TaskStatus::Completed
    );
    // The parked attempts were before the landing; the reason is cleared.
    assert!(landed.last_error().is_none());
    let detail = queue.show(TaskId::new(2)).unwrap();
    let kinds = event_kinds(&detail);
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "integration_deferred")
            .count(),
        3
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "integration_started")
            .count(),
        4
    );
    assert_eq!(kinds.iter().filter(|k| **k == "run_integrated").count(), 1);
}

/// The supervisor resumes a `needs_session` run whose `integrate` was
/// called (ADR-0019 decision 1): it opens a workspace named like the worker's
/// with the worker's wrapper, types the resolution request, sends `/exit`
/// once the session rewrote its receipt for the worktree head and went
/// idle, closes the workspace and lands the run. The first attempt only
/// rewrites the receipt, so the landing conflicts again and the run comes
/// back for a second attempt, which resolves it.
#[test]
fn approved_needs_session_run_is_resumed_until_the_runtime_lands_it() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let reason = run.last_error().unwrap().to_owned();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(
        payloads(&detail, "integration_approved"),
        [&json!({"status": "awaiting_integration", "pid": std::process::id(), "push": true})]
    );
    // The inbox is told the runtime takes it from here.
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        run_attention_of(&status, run.id()).unwrap()["next"],
        "resuming (runtime)"
    );

    // Task 696: a parked run keeps its runner for the resume, which copies
    // it again anyway: one gone before is there for each resumed session.
    let run_dir = Path::new(run.run_dir().unwrap());
    assert!(run_dir.join("runner").is_file());
    fs::remove_file(run_dir.join("runner")).unwrap();
    backend.resume_script_for(
        2,
        "await_message; dir=\"$(dirname \"$RECEIPT\")\"; [ -f \"$dir/runner\" ] && echo x >> \"$dir/runner-seen\"; mark=\"$dir/attempted\"; if [ -f \"$mark\" ]; then resolve; else : > \"$mark\"; fi; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let cursor = queue.latest_event_id().unwrap().as_i64();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let detail = queue.show(TaskId::new(2)).unwrap();
    let landed = detail.runs[0].clone();
    assert_landed(&repo, &landed, "second", &first_landed);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        "resolved by the resumed session\n"
    );
    assert!(queue.run_leases().unwrap().is_empty());
    let started = payloads(&detail, "resume_started");
    assert_eq!(started.len(), 2, "{:?}", event_kinds(&detail));
    assert_eq!(
        started[0],
        &json!({"attempt": 1, "counted": false, "reason": reason, "main": first_landed,
                // The claim's session (ADR-0079 decision 3).
                "model": "claude-opus-5-5", "effort": "medium", "group": null})
    );
    // Each conflict-only resume left out of the count is a repair.
    let uncounted: Vec<&Value> = payloads(&detail, "auto_repaired")
        .into_iter()
        .filter(|p| p["repair"] == "conflict_resume_uncounted")
        .collect();
    assert_eq!(uncounted.len(), 2, "{:?}", event_kinds(&detail));
    assert_eq!(uncounted[0]["conditions"]["conflict_only_resumes"], 1);
    assert_eq!(uncounted[1]["conditions"]["conflict_only_resumes"], 2);
    assert_eq!(uncounted[0]["detail"]["attempt"], 1);
    assert_eq!(started[1]["attempt"], 2);
    assert!(
        started[1]["reason"]
            .as_str()
            .unwrap()
            .contains("conflicted in change.txt")
    );
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 2);
    for (index, payload) in finished.iter().enumerate() {
        assert_eq!(payload["attempt"], index + 1);
        assert_eq!(payload["outcome"], "resolved");
        assert_eq!(payload["status"], "needs_session");
        assert_eq!(payload["approved"], true);
        assert_eq!(payload["workspace_closed"], true);
    }
    assert_eq!(finished[0]["head"], json!(run.result_commit()));
    assert_eq!(
        finished[1]["head"],
        json!(git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", run.id())]
        ))
    );
    // Each resume ends before its landing starts; one landing conflicted.
    let kinds = event_kinds(&detail);
    let resumed = kinds
        .iter()
        .skip_while(|k| **k != "resume_started")
        .filter(|k| {
            matches!(
                **k,
                "resume_started"
                    | "resume_finished"
                    | "integration_started"
                    | "integration_deferred"
                    | "run_integrated"
            )
        })
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(
        resumed,
        [
            "resume_started",
            "resume_finished",
            "integration_started",
            "integration_deferred",
            "resume_started",
            "resume_finished",
            "integration_started",
            "run_integrated",
        ]
    );
    // Each resumed session found the runner; once the run landed, it went
    // with nothing running it, and the run directory stays.
    assert_eq!(
        fs::read_to_string(run_dir.join("runner-seen")).unwrap(),
        "x\nx\n"
    );
    assert!(!run_dir.join("runner").exists());
    assert!(run_dir.join("receipt.json").is_file());
    // Same wrapper and runtime snapshot as the worker, in its worktree.
    let resumes = runtime_support::headless::resume_launches(&backend);
    assert_eq!(resumes.len(), 2);
    for launch in &resumes {
        let command = &launch.command;
        assert_eq!(launch.cwd, Path::new(run.worktree_path().unwrap()));
        assert!(
            command.contains(&shell_join(&[
                "session".into(),
                "--run".into(),
                run.id().to_string()
            ])),
            "{command}"
        );
        assert!(
            command.starts_with(&shell_join(&[run_dir
                .join("runner")
                .to_string_lossy()
                .into_owned()])),
            "{command}"
        );
    }
    let texts = session_texts(&run);
    assert_eq!(texts.len(), 2);
    let text = &texts[0];
    for expected in [
        format!(
            "dagq: integrate could not land run {} (task 2) and returned needs_session.",
            run.id()
        ),
        format!("Reason: {reason}"),
        format!(
            "main is now {first_landed} (your base commit was {}).",
            run.base_commit()
        ),
        "Tasks landed on main since your base:\n- task 1: test task; summary: done".to_owned(),
        format!("git rebase {first_landed}"),
        "[\"test -f seed.txt\"]".to_owned(),
        format!(
            "Rewrite the receipt at {} with the new head commit",
            run.receipt_path().unwrap()
        ),
        "result failed".to_owned(),
        // The headless worker's own lines: its turn is its reply.
        "4. Before you end the turn, stop every process you started".to_owned(),
        "Do not merge or push. Follow the repository's instructions for a worker".to_owned(),
    ] {
        assert!(text.contains(&expected), "{expected:?} not in {text}");
    }
    assert_eq!(
        &fs::read_to_string(run_dir.join("resume-1.txt")).unwrap(),
        text
    );
    assert!(!run_dir.join("terminal-resume-2.txt").exists());
    // An exit request per session (the first and the two resumed); both
    // resume workspaces were closed.
    assert_exit_sent(&backend, &run, 3);
    let kinds = event_kinds(&detail);
    let resumed = position(&kinds, "resume_started");
    assert_eq!(
        kinds[resumed..]
            .iter()
            .filter(|k| **k == "exit_requested")
            .count(),
        2,
        "{kinds:?}"
    );
    let closed = backend.closed();
    let resumed: Vec<&Value> = payloads(&detail, "resume_finished")
        .iter()
        .map(|p| &p["workspace_id"])
        .collect();
    assert_eq!(resumed.len(), 2, "{resumed:?}");
    assert!(
        resumed
            .iter()
            .all(|workspace| closed.contains(&workspace.as_str().unwrap().to_owned())),
        "{resumed:?} {closed:?}"
    );
    // Nothing waits for a person: the watch sees no attention.
    assert_eq!(
        dagq::compose::events(&db, EventId::new(cursor), 100, false).unwrap()["events"],
        json!([])
    );
    assert!(run_attention_of(&runtime::status(&db).unwrap(), run.id()).is_none());
}

/// A `needs_session` run that no `integrate` approved (here standing in for
/// validation's `evidence_missing`) is resumed with the evidence request
/// and, resolved, keeps its resumed session open through validation and the
/// supervisor's review like the worker's (ADR-0027 decision 3). The
/// stand-in `claude` prints no verdict, so the review is retried, fails,
/// and the run waits in an `approve_landing` ask.
#[test]
fn unapproved_resumed_run_is_validated_and_reviewed_with_its_session_open() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    let mut queue = SqliteQueue::open(&db).unwrap();
    Connection::open(&db)
        .unwrap()
        .execute(
            "DELETE FROM run_events WHERE run_id=?1 AND kind='integration_approved'",
            [&run.id()],
        )
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::EvidenceMissing,
            json!({"status": "needs_session", "reason": "e2e has no evidence"}),
        )
        .unwrap();
    backend.resume_script_for(
        2,
        "await_message; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let cursor = queue.latest_event_id().unwrap().as_i64();
    let outcome = supervise_retrying(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");

    let detail = queue.show(TaskId::new(2)).unwrap();
    let back = detail.runs[0].clone();
    assert_eq!(back.status(), RunStatus::AwaitingIntegration);
    assert_eq!(back.result_commit(), run.result_commit());
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["outcome"], "resolved");
    assert_eq!(finished[0]["status"], "validating");
    assert_eq!(finished[0]["approved"], false);
    assert_eq!(finished[0]["workspace_closed"], false);
    assert_eq!(finished[0]["session_live"], true);
    let resume_workspace = finished[0]["workspace_id"].as_str().unwrap().to_owned();
    let kinds = event_kinds(&detail);
    let after: Vec<&str> = kinds
        .iter()
        .skip_while(|k| **k != "resume_finished")
        .filter(|k| {
            matches!(
                **k,
                "validation_finished"
                    | "review_started"
                    | "review_retried"
                    | "exit_requested"
                    | "session_exited"
                    | "workspace_closed"
                    | "review_failed"
            )
        })
        .copied()
        .collect();
    assert_eq!(
        after,
        [
            "validation_finished",
            "review_started",
            "review_retried",
            "review_started",
            "exit_requested",
            "session_exited",
            "workspace_closed",
            "review_failed",
        ]
    );
    assert!(backend.closed().contains(&resume_workspace));
    assert_eq!(
        payloads(&detail, "review_started").last().unwrap()["workspace_id"],
        json!(resume_workspace)
    );
    assert!(
        !event_kinds(&detail).contains(&"integration_started") || {
            // Only the two landings by hand before the resume.
            payloads(&detail, "integration_started").len() == 1
        }
    );
    let text = &session_texts(&detail.runs[0])[0];
    assert!(
        text.contains("found required evidence missing from the receipt"),
        "{text}"
    );
    assert!(text.contains("Reason: e2e has no evidence"), "{text}");
    assert!(
        text.contains("Run the checks the reason names as missing"),
        "{text}"
    );
    assert!(!text.contains("git rebase"), "{text}");
    assert!(
        text.contains("Before you end the turn, stop every process you started"),
        "{text}"
    );
    // The inbox is woken only by the ask of the failed review (task 328).
    let events = dagq::compose::events(&db, EventId::new(cursor), 100, false).unwrap();
    assert_eq!(events["events"].as_array().unwrap().len(), 1, "{events}");
    assert_eq!(events["events"][0]["kind"], "ask_opened", "{events}");
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
    // Integrating it now is the approval.
    assert_eq!(
        integrate(&db, 2, &repo).unwrap()["outcome"],
        "needs_session"
    );
    assert_eq!(
        payloads(&queue.show(TaskId::new(2)).unwrap(), "integration_approved"),
        [&json!({"status": "awaiting_integration", "pid": std::process::id(), "push": true})]
    );
}

/// Stand in for an earlier resume the supervisor judged `unresolved`
/// (task 122): one `resume_started` / `resume_finished` pair under another
/// token after the run was parked.
pub(crate) fn unresolved_attempt(db: &Path, run: &TaskRun, main: &str) {
    let mut queue = SqliteQueue::open(db).unwrap();
    let (_, attempt) = queue
        .begin_resume(
            run.id(),
            &LeaseToken::new("earlier"),
            &sha(main),
            None,
            Default::default(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(attempt, 1);
    queue
        .finish_resume(
            run.id(),
            &LeaseToken::new("earlier"),
            None,
            None,
            false,
            json!({"attempt": 1, "outcome": "unresolved", "exhausted": false}),
        )
        .unwrap();
}

/// Resolve the parked conflict in the run's worktree on top of `main` as
/// that session did, and return the new head.
pub(crate) fn resolve_in_worktree(run: &TaskRun, main: &str) -> String {
    let worktree = Path::new(run.worktree_path().unwrap());
    let rebase = Command::new(git_executable().expect("git executable"))
        .arg("-C")
        .arg(worktree)
        .args(["rebase", main])
        .bounded_output()
        .unwrap();
    assert!(!rebase.status.success());
    fs::write(worktree.join("change.txt"), "resolved by the session\n").unwrap();
    git(worktree, &["add", "change.txt"]);
    let status = Command::new(git_executable().expect("git executable"))
        .arg("-C")
        .arg(worktree)
        .env("GIT_EDITOR", "true")
        .args(["rebase", "--continue"])
        .bounded_status()
        .unwrap();
    assert!(status.success());
    git_out(worktree, &["rev-parse", "HEAD"])
}

/// A parked run whose earlier resume already rebased it onto main and
/// rewrote the receipt for its clean head (judged `unresolved` all the
/// same): the supervisor opens no session and uses no attempt, records
/// `resume_skipped` and, its integrate approved, lands it. The backend has
/// no resume script, so a resume would have failed.
#[test]
fn an_approved_run_resolved_by_an_earlier_resume_lands_without_a_session() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    unresolved_attempt(&db, &run, &first_landed);
    let resolved = resolve_in_worktree(&run, &first_landed);
    write_receipt(&run, &resolved, "succeeded", "resolved");

    let mut queue = SqliteQueue::open(&db).unwrap();
    let cursor = queue.latest_event_id().unwrap().as_i64();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert!(runtime_support::headless::resume_launches(&backend).is_empty());
    assert!(session_texts(&detail.runs[0]).is_empty());
    assert_eq!(payloads(&detail, "resume_started").len(), 1);
    assert_eq!(
        payloads(&detail, "resume_skipped"),
        [
            &json!({"head": resolved, "main": first_landed, "approved": true, "status": "needs_session"})
        ]
    );
    let kinds = event_kinds(&detail);
    let after: Vec<&str> = kinds
        .iter()
        .skip_while(|k| **k != "resume_finished")
        .filter(|k| {
            matches!(
                **k,
                "resume_finished"
                    | "resume_skipped"
                    | "resume_started"
                    | "integration_started"
                    | "run_integrated"
            )
        })
        .copied()
        .collect();
    assert_eq!(
        after,
        [
            "resume_finished",
            "resume_skipped",
            "integration_started",
            "run_integrated"
        ]
    );
    assert!(queue.run_leases().unwrap().is_empty());
    assert_eq!(
        dagq::compose::events(&db, EventId::new(cursor), 100, false).unwrap()["events"],
        json!([])
    );
}

/// The same run without an approving `integrate` is validated and reviewed
/// without a session: the stand-in `claude` prints no verdict, so it waits
/// in `awaiting_integration` for a person's answer to the `approve_landing`
/// ask of its failed review.
#[test]
fn an_unapproved_run_resolved_by_an_earlier_resume_is_validated_without_a_session() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    Connection::open(&db)
        .unwrap()
        .execute(
            "DELETE FROM run_events WHERE run_id=?1 AND kind='integration_approved'",
            [&run.id()],
        )
        .unwrap();
    unresolved_attempt(&db, &run, &first_landed);
    let resolved = resolve_in_worktree(&run, &first_landed);
    write_receipt(&run, &resolved, "succeeded", "resolved");

    let mut queue = SqliteQueue::open(&db).unwrap();
    let outcome = supervise_retrying(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let detail = queue.show(TaskId::new(2)).unwrap();
    let back = &detail.runs[0];
    assert_eq!(back.status(), RunStatus::AwaitingIntegration);
    assert_eq!(
        back.result_commit().map(CommitSha::as_str),
        Some(resolved.as_str())
    );
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);
    assert!(runtime_support::headless::resume_launches(&backend).is_empty());
    assert_eq!(payloads(&detail, "resume_started").len(), 1);
    assert_eq!(
        payloads(&detail, "resume_skipped"),
        [
            &json!({"head": resolved, "main": first_landed, "approved": false, "status": "validating"})
        ]
    );
    let kinds = event_kinds(&detail);
    let after: Vec<&str> = kinds
        .iter()
        .skip_while(|k| **k != "resume_skipped")
        .filter(|k| {
            matches!(
                **k,
                "resume_skipped"
                    | "validation_finished"
                    | "review_started"
                    | "review_retried"
                    | "review_failed"
            )
        })
        .copied()
        .collect();
    // The unreadable verdict is reviewed once more (task 328).
    assert_eq!(
        after,
        [
            "resume_skipped",
            "validation_finished",
            "review_started",
            "review_retried",
            "review_started",
            "review_failed"
        ]
    );
    assert_eq!(
        payloads(&detail, "review_started").last().unwrap()["workspace_id"],
        Value::Null
    );
    assert!(queue.run_leases().unwrap().is_empty());
}

/// A skipped run the landing parks again (its verification fails on the
/// resolved head) is resumed with a session next, not skipped again.
#[test]
fn a_run_parked_again_after_a_skip_is_resumed_not_skipped() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    unresolved_attempt(&db, &run, &first_landed);
    let resolved = resolve_in_worktree(&run, &first_landed);
    write_receipt(&run, &resolved, "succeeded", "resolved");
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE tasks SET verification_commands='[\"false\"]' WHERE id=2",
            [],
        )
        .unwrap();

    // The resumes after the second park fail (the backend has no script)
    // until the attempts are used up; none is skipped. Whether one pass of
    // `--once` makes both depends on whether it reads the run parked again
    // before it reaps the landing's slot, so passes are run until the last
    // attempt started (each pass makes at least one).
    let mut queue = SqliteQueue::open(&db).unwrap();
    let mut errors = Vec::new();
    for _ in 0..MAX_RESUME_ATTEMPTS {
        let outcome = supervise(&db, &repo, &backend).unwrap();
        errors.extend(outcome["errors"].as_array().unwrap().iter().cloned());
        let detail = queue.show(TaskId::new(2)).unwrap();
        if payloads(&detail, "resume_started")
            .last()
            .is_some_and(|p| p["attempt"] == MAX_RESUME_ATTEMPTS)
        {
            break;
        }
    }
    assert_eq!(errors.len(), 2, "{errors:?}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);
    assert_eq!(payloads(&detail, "resume_skipped").len(), 1);
    let kinds = event_kinds(&detail);
    let after: Vec<&str> = kinds
        .iter()
        .skip_while(|k| **k != "resume_skipped")
        .filter(|k| {
            matches!(
                **k,
                "resume_skipped" | "integration_deferred" | "resume_started" | "resume_finished"
            )
        })
        .copied()
        .collect();
    assert_eq!(
        after,
        [
            "resume_skipped",
            "integration_deferred",
            "resume_started",
            "resume_finished",
            "resume_started",
            "resume_finished"
        ]
    );
    assert_eq!(
        payloads(&detail, "resume_started").last().unwrap()["attempt"],
        3
    );
}

/// An unapproved run a supervisor moved on by `resume_skipped` and then
/// died holding (no session, no wrapper registration since) is adopted by
/// the next supervisor and validated and reviewed with no session.
#[test]
fn a_skipped_run_whose_supervisor_died_is_adopted() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    Connection::open(&db)
        .unwrap()
        .execute(
            "DELETE FROM run_events WHERE run_id=?1 AND kind='integration_approved'",
            [&run.id()],
        )
        .unwrap();
    // The attempt clears the worker's process rows: no wrapper is left.
    unresolved_attempt(&db, &run, &first_landed);
    let resolved = resolve_in_worktree(&run, &first_landed);
    write_receipt(&run, &resolved, "succeeded", "resolved");
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.processes(run.id()).unwrap().is_empty());
    let skipped = queue
        .skip_resume(
            run.id(),
            &LeaseToken::new("dead"),
            &sha(&resolved),
            &sha(&first_landed),
            false,
        )
        .unwrap()
        .unwrap();
    assert_eq!(skipped.status(), RunStatus::Validating);
    // Taken already: a second skip or resume finds it leased.
    assert!(
        queue
            .skip_resume(
                run.id(),
                &LeaseToken::new("other"),
                &sha(&resolved),
                &sha(&first_landed),
                false
            )
            .unwrap()
            .is_none()
    );
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET pid=?2 WHERE run_id=?1",
            rusqlite::params![run.id(), dead_pid()],
        )
        .unwrap();

    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(runtime_support::headless::resume_launches(&backend).is_empty());
    let adopted = payloads(&detail, "run_adopted");
    assert_eq!(adopted.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(adopted[0]["wrapper"], Value::Null);
    assert_eq!(
        payloads(&detail, "review_started").last().unwrap()["workspace_id"],
        Value::Null
    );
    assert!(queue.run_leases().unwrap().is_empty());
}

/// A parked run lacking a condition of the skip is resumed as before: the
/// resume uses an attempt and no `resume_skipped` is recorded. Which
/// conditions the skip needs is the unit test
/// `supervise::resume::tests::a_run_is_skipped_only_when_every_condition_of_the_skip_holds`;
/// this one reads them from the real worktree, its dirty status here. The
/// backend has no resume script, so the resume cannot start: it ends as an
/// `error` with the failed cmux call recorded on the run (task 109), and
/// `last_error` keeps why the run was parked. The resume opens a workspace,
/// whose `create_resume` is the call recorded (task 1439).
#[test]
fn a_run_with_a_dirty_worktree_is_resumed() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    let reason = run.last_error().unwrap().to_owned();
    unresolved_attempt(&db, &run, &first_landed);
    let resolved = resolve_in_worktree(&run, &first_landed);
    write_receipt(&run, &resolved, "succeeded", "resolved");
    let worktree = Path::new(run.worktree_path().unwrap());
    fs::write(worktree.join("stray.txt"), "left over\n").unwrap();

    let options = in_workspaces(supervise_options(4, true));
    let outcome = supervise_with(&db, &repo, &backend, &options).unwrap();
    assert_eq!(outcome["errors"].as_array().unwrap().len(), 1, "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert!(
        payloads(&detail, "resume_skipped").is_empty(),
        "{:?}",
        event_kinds(&detail)
    );
    assert_eq!(payloads(&detail, "resume_started").len(), 2);
    let finished = payloads(&detail, "resume_finished");
    let last = finished.last().unwrap();
    assert_eq!(last["outcome"], "error");
    assert_eq!(last["exhausted"], false);
    assert!(last["error"].as_str().unwrap().contains("no resume script"));
    let failures = backend_failures(&detail);
    assert_eq!(failures.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(failures[0].run_id.as_ref(), Some(run.id()));
    assert_eq!(failures[0].payload["op"], "create_resume");
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    assert_eq!(detail.runs[0].last_error(), Some(reason.as_str()));
    assert!(queue.run_leases().unwrap().is_empty());
}

/// Record `resumes` resumes of the run that started and ended
/// `unresolved` as a supervisor records them, without their sessions: the
/// attempts a test of the last one starts from.
fn earlier_resumes(queue: &mut SqliteQueue, run: &TaskRun, resumes: usize) {
    for attempt in 1..=resumes {
        queue
            .record_runtime_event(
                run.id(),
                EventKind::ResumeStarted,
                json!({"attempt": attempt}),
            )
            .unwrap();
        queue
            .record_runtime_event(
                run.id(),
                EventKind::ResumeFinished,
                json!({"attempt": attempt, "outcome": "unresolved", "status": "needs_session"}),
            )
            .unwrap();
    }
}

/// ADR-0047 decision 24: a run whose landing was approved and that waits
/// only because its rebase conflicts with main is resumed without using up
/// one of the three attempts, up to the conflict-only limit (how each is
/// counted and recorded is the unit test
/// `domain::resume::tests::a_conflict_only_resume_is_an_uncounted_repair`).
/// Past it, the run is not handed to a person: it fails, its head is kept
/// under `refs/dagq/runs/<run-id>`, the task is ready again, and the next
/// run's prompt asks to bring that commit onto the current main. The
/// resumes before the last are recorded without their sessions.
#[test]
fn a_run_whose_conflict_only_resumes_are_used_up_is_retried_with_its_branch() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.resume_timeout = Duration::from_secs(1);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    let source = run.result_commit().cloned().unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    earlier_resumes(&mut queue, &run, CONFLICT_ONLY_RESUME_LIMIT - 1);
    // The last session does not resolve it: it never goes idle and exits
    // at the /exit of the resume timeout.
    backend.resume_script_for(2, "await_exit");
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let detail = queue.show(TaskId::new(2)).unwrap();
    let first = detail.runs[0].clone();
    assert_eq!(first.id(), run.id());
    let started: Vec<&Value> = detail
        .events
        .iter()
        .filter(|e| e.kind == "resume_started" && e.run_id.as_ref() == Some(run.id()))
        .map(|e| &e.payload)
        .collect();
    assert_eq!(
        started.len(),
        CONFLICT_ONLY_RESUME_LIMIT,
        "{:?}",
        event_kinds(&detail)
    );
    assert_eq!(started.last().unwrap()["counted"], false, "{started:?}");
    assert_eq!(
        started.last().unwrap()["attempt"],
        CONFLICT_ONLY_RESUME_LIMIT
    );
    assert_eq!(
        payloads(&detail, "resume_finished").last().unwrap()["exhausted"],
        true
    );
    assert_eq!(first.status(), RunStatus::Failed);
    let last_error = first.last_error().unwrap();
    assert!(
        last_error.starts_with(
            "resumed 5 times (0 of at most 3 counted, and 5 of at most 5 for conflicts only after its review passed) and still needs a session: rebase onto main"
        ),
        "{last_error}"
    );
    // Retried by the runtime, with nobody asked.
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(finished[0]["by"], "runtime");
    assert_eq!(finished[0]["code"], "resume_exhausted");
    assert_eq!(finished[0]["action"], "retry_inherit");
    assert_eq!(finished[0]["counted_resumes"], 0);
    assert_eq!(finished[0]["conflict_only_resumes"], 5);
    assert_eq!(finished[0]["inherit"]["head"], json!(source));
    assert_eq!(
        finished[0]["inherit"]["branch"],
        json!(format!("dagq/{}", run.id()))
    );
    assert!(finished[0].get("ask_id").is_none());
    // The last conflict-only resume was left out of the count, then the
    // retry carried the branch over: both repairs (ADR-0047 decision 38).
    let repaired = payloads(&detail, "auto_repaired");
    let repairs: Vec<&str> = repaired
        .iter()
        .map(|p| p["repair"].as_str().unwrap())
        .collect();
    assert_eq!(
        repairs,
        ["conflict_resume_uncounted", "inherit_retry"],
        "{repaired:?}"
    );
    assert_eq!(
        repaired[0]["conditions"]["conflict_only_resumes"],
        CONFLICT_ONLY_RESUME_LIMIT
    );
    assert_eq!(repaired[0]["detail"]["main"], json!(first_landed));
    let repaired = &repaired[1..];
    assert_eq!(repaired[0]["layer"], "runtime");
    assert_eq!(repaired[0]["conditions"]["review"], "pass");
    assert_eq!(repaired[0]["conditions"]["parked"], "rebase_conflict");
    assert!(
        other_asks(&mut queue, true)
            .iter()
            .all(|ask| ask.kind != AskKind::Decide),
        "{:?}",
        other_asks(&mut queue, true)
    );
    assert_eq!(
        git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", run.id())]
        ),
        source.as_str()
    );

    // The next run carries the work over.
    assert_eq!(detail.runs.len(), 2, "{:?}", event_kinds(&detail));
    let next = detail.runs[1].clone();
    let inherited: Vec<&Value> = detail
        .events
        .iter()
        .filter(|e| e.kind == "run_inherited")
        .map(|e| &e.payload)
        .collect();
    assert_eq!(inherited.len(), 1);
    assert_eq!(inherited[0]["inherit_from_run"], json!(run.id()));
    assert_eq!(inherited[0]["head"], json!(source));
    assert!(
        detail
            .events
            .iter()
            .any(|e| e.kind == "run_inherited" && e.run_id.as_ref() == Some(next.id()))
    );
    let prompt = fs::read_to_string(Path::new(next.run_dir().unwrap()).join("prompt.txt")).unwrap();
    // Its own commits start at the merge base of its head and the new
    // run's base (the landed main): here the base the first run was made on.
    let base = run.base_commit();
    assert!(
        prompt.contains(&format!(
            "Carried over from run {}: its review passed, but its landing kept conflicting with the landing branch until its resumes were used up, so this run starts from its work instead of from scratch. Its work is commit {source} (kept as refs/dagq/runs/{id}, branch dagq/{id}); its own commits are {base}..{source}. Bring them onto your base",
            run.id(),
            id = run.id()
        )),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("`git cherry-pick {base}..{source}`")),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!(
            "Its receipt ({}) summary: ",
            run.receipt_path().unwrap()
        )),
        "{prompt}"
    );
    assert_eq!(next.base_commit().as_str(), first_landed);
}

/// A session that cannot resolve the run uses up an attempt; after the
/// third the supervisor stops resuming it: the run becomes `failed` with
/// its `resume_exhausted` alert recorded (`recovery_requested`, ADR-0047
/// decision 39), and the recovery job takes it. Its escalation is a
/// `decide` ask for the inbox (`retry` or `cancel` and the job's options:
/// resuming is no longer one), and the answer is applied like any other of
/// the job's asks. The session behaves like Claude: it never exits by
/// itself, so the supervisor sends `/exit` once it goes idle without a
/// resolving receipt. The conflict that parked the run is made to count (a
/// conflict-only resume does not; see the test above). The first two
/// attempts are recorded without their sessions (a resume that cannot
/// start is `a_run_with_a_dirty_worktree_is_resumed`, the used-up reason
/// the unit test `supervise::resume::tests::resumed_text_counts_every_kind_of_resume`).
#[test]
fn resuming_stops_after_three_attempts() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    count_resumes_of_parked(&db);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let reason = run.last_error().unwrap().to_owned();
    earlier_resumes(&mut queue, &run, MAX_RESUME_ATTEMPTS - 1);

    // The last attempt answers without resolving and goes idle; it does
    // not exit until it is asked to.
    backend.resume_script_for(2, "await_message; idle; await_exit");
    let cursor = queue.latest_event_id().unwrap().as_i64();
    let reviewer =
        TestReviewer::new(&[verdict("pass", &[], "unused")]).with_triages(&[recovery(json!({
            "verdict": "escalate",
            "confidence": "high",
            "diagnosis": "the conflict needs a decision on the design",
            "options": ["split the task"],
        }))]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    assert_eq!(
        detail.runs[0].last_error(),
        Some(format!("resumed 3 times (at most 3) and still needs a session: {reason}").as_str())
    );
    let started = payloads(&detail, "resume_started");
    assert_eq!(
        started
            .iter()
            .map(|p| p["attempt"].clone())
            .collect::<Vec<_>>(),
        [json!(1), json!(2), json!(3)]
    );
    assert_eq!(started[2]["reason"], json!(reason));
    assert_eq!(started[2]["counted"], true);
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 3);
    assert_eq!(finished[2]["outcome"], "unresolved");
    assert_eq!(finished[2]["exhausted"], true);
    assert_eq!(finished[2]["status"], "needs_session");
    assert!(queue.run_leases().unwrap().is_empty());
    // The first session's and the last resume's.
    assert_exit_sent(&backend, &detail.runs[0], 2);
    assert_eq!(backend.closed().len(), 2 + 1); // two workers, one resume
    // The used-up run is the recovery job's (`resume_exhausted`), which
    // escalates it as a `decide` ask.
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(requested[0]["alert"], "resume_exhausted");
    assert_eq!(requested[0]["attempt"], 1);
    assert_eq!(requested[0]["by"], "runtime");
    assert_eq!(requested[0]["code"], "resume_exhausted");
    assert_eq!(requested[0]["previous_status"], "needs_session");
    assert_eq!(requested[0]["status"], "failed");
    assert_eq!(payloads(&detail, "triage_started").len(), 1);
    let (prompt, _) = &reviewer.triage_prompts()[0];
    assert!(
        prompt.contains("raised the alert resume_exhausted"),
        "{prompt}"
    );
    assert!(prompt.contains("do not choose resume"), "{prompt}");
    let asks = other_asks(&mut queue, false);
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].clone();
    assert_eq!(ask.kind, AskKind::Decide);
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    assert_eq!(ask.asked_by, "supervisor");
    assert_eq!(
        ask.options,
        [
            "retry",
            "cancel",
            "split the task",
            "edit the task's --verify, then retry_inherit"
        ]
    );
    // Its parks count as failed verifications, so the verify fix is offered.
    assert!(ask.question.contains("dagq edit"), "{}", ask.question);
    for part in [
        "alert: resume_exhausted",
        "Diagnosis: the conflict needs a decision on the design",
        "resumed 3 times (at most 3) and still needs a session",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    assert!(ask.question.contains(&reason), "{}", ask.question);
    assert!(!ask.question.contains(" resume: "), "{}", ask.question);
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(finished[0]["alert"], "resume_exhausted");
    assert_eq!(finished[0]["action"], "ask");
    assert_eq!(finished[0]["ask_id"], json!(ask.id));
    assert_eq!(finished[0]["status"], "failed");
    // The ask is the one attention; the exhausted resume is none.
    let events = dagq::compose::events(&db, EventId::new(cursor), 100, false).unwrap();
    let listed = events["events"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "{events}");
    assert_eq!(listed[0]["kind"], "ask_opened");
    assert_eq!(listed[0]["next"], format!("answer ask {}", ask.id));
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");
    // No fourth attempt, no second ask.
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    assert_eq!(
        payloads(&queue.show(TaskId::new(2)).unwrap(), "resume_started").len(),
        3
    );
    assert_eq!(other_asks(&mut queue, false).len(), 1);

    // The person cancels the task; the supervisor applies it.
    queue.answer(ask.id, "cancel").unwrap();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    assert_eq!(
        payloads(&detail, "triage_decided")[0]["answer"],
        json!("cancel")
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
}

/// ADR-0047 decisions 39 and 40: a run whose counted resumes are used up
/// is not the runtime's automatic retry (its last park was not a conflict
/// alone), so its `resume_exhausted` alert goes to the recovery job, whose
/// `retry_inherit` holds (its branch has the reviewed commit, and the task
/// was not retried that way before): the task is ready again and the next
/// run carries the branch over.
#[test]
fn a_used_up_run_is_retried_with_its_branch_by_its_recovery_job() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    count_resumes_of_parked(&db);
    let mut queue = SqliteQueue::open(&db).unwrap();
    earlier_resumes(&mut queue, &run, MAX_RESUME_ATTEMPTS);
    // Its attempts used up, no resume begins.
    assert!(
        queue
            .begin_resume(
                run.id(),
                &LeaseToken::new("t"),
                run.base_commit(),
                None,
                Default::default()
            )
            .unwrap()
            .is_none()
    );
    // The next run of the task fails at once and is left to a person.
    backend.script_for(2, "exit 7");
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "unused")]).with_triages(&[repair(
        json!({"action": "retry_inherit"}),
        "the reviewed work only needs rebasing onto the new main",
    )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.runs.len(), 2, "{:?}", event_kinds(&detail));
    let events = queue.run_events(run.id()).unwrap();
    let of = |kind: &str| -> Vec<Value> {
        events
            .iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.payload.clone())
            .collect()
    };
    let requested = of("recovery_requested");
    assert_eq!(requested.len(), 1);
    assert_eq!(requested[0]["alert"], "resume_exhausted");
    let finished = of("triage_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["action"], "retry_inherit");
    assert_eq!(finished[0]["alert"], "resume_exhausted");
    assert_eq!(finished[0]["inherit"]["head"], json!(run.result_commit()));
    let repaired = of("auto_repaired");
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert_eq!(repaired[0]["layer"], "recovery");
    assert_eq!(repaired[0]["repair"], "retry_inherit");
    assert_eq!(repaired[0]["alert"], "resume_exhausted");
    assert_eq!(
        of("recovery_finished")[0]["applied"],
        json!(["retry_inherit"])
    );
    let next = &detail.runs[1];
    let inherited: Vec<&Value> = detail
        .events
        .iter()
        .filter(|e| e.kind == "run_inherited" && e.run_id.as_ref() == Some(next.id()))
        .map(|e| &e.payload)
        .collect();
    assert_eq!(inherited[0]["inherit_from_run"], json!(run.id()));
    assert!(
        other_asks(&mut queue, true)
            .iter()
            .all(|ask| ask.kind != AskKind::Decide)
    );
}

/// The supervisor never resumes a run next to the live session of a
/// supervisor that died mid-resume: the run shows as `resuming (runtime)`,
/// keeps a person's `integrate` out, and once that session exited the next
/// supervisor resumes and lands the run.
#[test]
fn a_session_nobody_watches_blocks_the_resume_until_it_ends() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    let mut queue = SqliteQueue::open(&db).unwrap();
    // A supervisor started a resume, its session registered, and then the
    // supervisor died: its lease goes stale while the session lives on.
    let (_, attempt) = queue
        .begin_resume(
            run.id(),
            &LeaseToken::new("dead-supervisor"),
            &sha(&first_landed),
            None,
            Default::default(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(attempt, 1);
    assert!(
        queue
            .begin_resume(
                run.id(),
                &LeaseToken::new("another"),
                &sha(&first_landed),
                None,
                Default::default()
            )
            .unwrap()
            .is_none()
    );
    queue
        .register_resume_wrapper(
            run.id(),
            &LeaseToken::new("dead-supervisor"),
            std::process::id(),
        )
        .unwrap();
    assert!(
        queue
            .register_resume_wrapper(
                run.id(),
                &LeaseToken::new("dead-supervisor"),
                std::process::id()
            )
            .is_err()
    );
    age_lease(&db, &run, 60);
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        run_attention_of(&status, run.id()).unwrap()["next"],
        "resuming (runtime)"
    );
    let refused = integrate(&db, 2, &repo).unwrap_err();
    assert!(
        format!("{refused:#}").contains("run is still leased"),
        "{refused:#}"
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    assert_eq!(
        payloads(&queue.show(TaskId::new(2)).unwrap(), "resume_started").len(),
        1
    );

    // Its session ends; the next supervisor takes the stale lease over.
    queue
        .wrapper_exited(run.id(), std::process::id(), 0)
        .unwrap();
    assert_eq!(
        run_attention_of(&runtime::status(&db).unwrap(), run.id()).unwrap()["next"],
        "resuming (runtime)"
    );
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    let started = payloads(&detail, "resume_started");
    assert_eq!(
        started.len(),
        2,
        "{:?}",
        detail
            .events
            .iter()
            .map(|e| (&e.kind, &e.payload))
            .collect::<Vec<_>>()
    );
    assert_eq!(started[1]["attempt"], 2);
    let acquired = detail
        .events
        .iter()
        .filter(|e| e.kind == "lease_acquired" && e.payload["reason"] == "resume")
        .map(|e| e.payload["previous_token"].clone())
        .collect::<Vec<_>>();
    assert_eq!(acquired, [json!(null), json!("dead-supervisor")]);
}

/// A resumed session that finds the change no longer needed writes a failed
/// receipt; the run ends `failed` without landing.
#[test]
fn resumed_session_with_a_failed_receipt_fails_the_run() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        "await_message; receipt \"$(git rev-parse HEAD)\" failed 'already on main'; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["runs"][0]["status"], "failed", "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    assert_eq!(
        detail.runs[0].last_error(),
        Some("session reported the run as failed: already on main")
    );
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished[0]["outcome"], "failed");
    assert_eq!(finished[0]["status"], "failed");
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), first_landed);
    assert!(Path::new(run.worktree_path().unwrap()).exists());
    // The failed run goes to the triage; the stub `claude` prints no
    // verdict, so the triage fails and the run waits for a person.
    let failed = payloads(&detail, "triage_failed");
    assert_eq!(failed.len(), 1);
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap()
            .contains("no verdict JSON"),
        "{}",
        failed[0]
    );
    assert_eq!(
        run_attention_of(&runtime::status(&db).unwrap(), run.id()).unwrap()["next"],
        "triage by hand"
    );
}

/// A session that finds the change no longer needed writes a failed receipt
/// with the reason; the run ends without touching main and the task can be
/// retried or canceled.
#[test]
fn failed_receipt_from_a_session_ends_the_run_without_landing() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let main = git_out(&repo, &["rev-parse", "main"]);
    assert_eq!(
        integrate(&db, 2, &repo).unwrap()["outcome"],
        "needs_session"
    );
    let run = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    write_receipt(
        &run,
        run.result_commit().map(CommitSha::as_str).unwrap(),
        "failed",
        "already covered by task 1",
    );
    let outcome = integrate(&db, 2, &repo).unwrap();
    assert_eq!(outcome["outcome"], "failed", "{outcome}");
    let reason = outcome["reason"].as_str().unwrap();
    assert!(reason.contains("already covered by task 1"), "{reason}");
    let failed = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_eq!(failed.status(), RunStatus::Failed);
    assert_eq!(failed.last_error(), Some(reason));
    assert!(Path::new(failed.worktree_path().unwrap()).exists());
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), main);
    assert!(
        git_out(&repo, &["for-each-ref", "refs/dagq/runs/"])
            .lines()
            .count()
            == 1
    );
    assert_eq!(
        queue.show(TaskId::new(2)).unwrap().task.status(),
        TaskStatus::InProgress
    );
    assert!(queue.run_leases().unwrap().is_empty());
    let detail = queue.show(TaskId::new(2)).unwrap();
    let failed_event = detail
        .events
        .iter()
        .find(|e| e.kind == "integration_failed")
        .unwrap();
    assert_eq!(failed_event.payload["status"], "failed");
    assert_eq!(failed_event.payload["reason"], json!(reason));
    assert_eq!(
        failed_event.payload["receipt"],
        session_receipt(
            &run,
            run.result_commit().map(CommitSha::as_str).unwrap(),
            "failed",
            "already covered by task 1"
        )
    );
    // A failed receipt is not a receipt for a landing: only the first
    // (conflicting) attempt recorded one.
    assert_eq!(integration_receipts(&detail).len(), 1);
    // Retry or give up is a person's call, as after any failed run.
    queue
        .transition(TaskId::new(2), TaskAction::Cancel)
        .unwrap();
}

/// Task 466: `stats` ties the conflict that parked the second run to the
/// first run's landing, whose main it was rebased onto, and counts the
/// resumes by reason: the first resolved in its session but was deferred
/// again, the second landed the run. A time window narrows the runs.
#[test]
fn stats_ties_a_deferred_landing_to_the_landing_that_broke_it() {
    use dagq::domain::stats::{Cursor, StatsQuery};
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        "await_message; mark=\"$(dirname \"$RECEIPT\")/attempted\"; if [ -f \"$mark\" ]; then resolve; else : > \"$mark\"; fi; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let first = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();

    let query = |since: Option<Cursor>, until: Option<Cursor>| {
        runtime::stats(
            &db,
            &StatsQuery {
                since,
                until,
                full: true,
                ..StatsQuery::default()
            },
        )
        .unwrap()
    };
    let stats = query(None, None);
    let runs = stats["runs"].as_array().unwrap();
    let row = |id: &RunId| {
        runs.iter()
            .find(|row| row["run_id"] == id.as_str())
            .unwrap_or_else(|| panic!("no run {id} in {stats}"))
    };
    let (landing, broken) = (row(first.id()), row(run.id()));
    assert_eq!(landing["title"], "test task");
    assert_eq!(broken["title"], "second");
    assert_eq!(landing["broke_runs"], 1);
    assert_eq!(landing["integrate_attempts"], 1);
    assert_eq!(broken["broke_runs"], 0);
    // Deferred at the first landing and after the first resume, landed at the third.
    assert_eq!(broken["integrate_attempts"], 3);
    assert_eq!(broken["deferrals"], json!({"rebase_conflict": 2}));
    assert_eq!(broken["conflict_files"], json!(["change.txt"]));
    assert_eq!(
        broken["broken_by"],
        json!([{
            "task_id": 1,
            "run_id": first.id().as_str(),
            "landed_at": landing["landed_at"],
            "main": first_landed,
            "code": "rebase_conflict",
        }])
    );
    assert_eq!(broken["rebased_onto"], json!([]));
    let attempts = broken["resume_attempts"].as_array().unwrap();
    assert_eq!(
        attempts
            .iter()
            .map(|a| (
                a["attempt"].clone(),
                a["reason"].clone(),
                a["resolved"].clone()
            ))
            .collect::<Vec<_>>(),
        [
            (json!(1), json!("rebase_conflict"), json!(false)),
            (json!(2), json!("rebase_conflict"), json!(true)),
        ]
    );
    assert!(attempts.iter().all(|a| a["secs"].is_i64()), "{attempts:?}");
    for row in [landing, broken] {
        for key in ["claimed_at", "validated_at", "landed_at"] {
            assert!(
                row[key].as_str().is_some_and(|at| at.ends_with('Z')),
                "{key} of {row}"
            );
        }
    }
    let outcomes = &stats["overall"]["resume_outcomes"];
    assert_eq!(outcomes["attempts"], 2);
    let conflict = &outcomes["by_reason"]["rebase_conflict"];
    assert_eq!(
        (
            &conflict["resolved"],
            &conflict["unresolved"],
            &conflict["resolved_percent"]
        ),
        (&json!(1), &json!(1), &json!(50))
    );
    assert_eq!(conflict["secs"]["count"], 2);

    // Up to the first landing's time only it finished; after it, only the second.
    let landed_at: Cursor = landing["landed_at"].as_str().unwrap().parse().unwrap();
    let ids = |stats: &Value| {
        stats["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["run_id"].clone())
            .collect::<Vec<_>>()
    };
    let until = query(None, Some(landed_at));
    assert_eq!(ids(&until), [json!(first.id().as_str())]);
    assert!(until["next_cursor"].as_i64() < stats["next_cursor"].as_i64());
    assert_eq!(
        ids(&query(Some(landed_at), None)),
        [json!(run.id().as_str())]
    );
}

#[test]
fn no_claude_leaves_an_old_claude_resume_for_manual_recovery() {
    let (dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    let reviewer = TestReviewer::new(&[]);
    let options = SuperviseOptions {
        no_claude: true,
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
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(run.task_id()).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    assert!(payloads(&detail, "resume_started").is_empty());
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert!(reviewer.prompts().is_empty());
    // A live registration makes the manual policy and this run's next step visible.
    let token = LeaseToken::new("no-claude-status");
    queue
        .register_supervisor(&token, std::process::id(), 1, VERSION)
        .unwrap();
    queue
        .set_supervisor_providers(
            &token,
            &[dagq::domain::worker::ProviderCheck {
                provider: dagq::domain::Provider::Claude,
                executable: "missing".into(),
                found: false,
                error: Some("provider_disabled".into()),
                modes: vec![],
            }],
        )
        .unwrap();
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        run_attention_of(&status, run.id()).unwrap()["next"],
        "recover by hand"
    );
    assert!(
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["kind"] == "provider_disabled")
    );
}

/// A resume whose lease another token took while its session ended
/// unwatched (as a stalled supervisor's resume looks to the next one) is
/// not resumed again into a second slot by the supervisor that still
/// drives it: its slot is dropped first, writing nothing about the
/// attempt, and only a later pass starts the next attempt, once, from the
/// lease it took over (task 1361).
#[test]
fn a_resume_whose_lease_moved_is_resumed_again_only_after_its_slot_is_dropped() {
    let (_dir, repo, db) = fixture();
    let _dump = EventsOnPanic(db.clone());
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let (run, _) = parked_conflict(&repo, &db, &backend);
    // Each of the two attempts works until the test lets it go (attempt n
    // at `$EXIT.go<n>`), then gives up.
    backend.resume_script_for(
        2,
        "n=$(($(cat \"$RUN_DIR/attempts\" 2>/dev/null || echo 0) + 1)); echo $n > \"$RUN_DIR/attempts\"; \
         if [ $n -le 2 ]; then await_file \"$EXIT.go$n\"; fi; say 'gave up'",
    );
    // A long tick: the lease most likely moves while the supervisor sleeps
    // between passes, so the next pass judges the resume before it steps
    // the slot that lost the lease.
    let options = SuperviseOptions {
        tick: Duration::from_millis(500),
        ..supervise_options(2, false)
    };
    let supervisor = {
        let (db, repo, backend, options) =
            (db.clone(), repo.clone(), backend.clone(), options.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        let events = queue.run_events(run.id()).unwrap();
        let started = events.iter().position(|e| e.kind == "resume_started");
        started.is_some_and(|at| events[at..].iter().any(|e| e.kind == "turn_started"))
            && wrapper_lives(&db, &run)
    });
    let moved = take_lease(&db, &run, true);
    let mut queue = SqliteQueue::open(&db).unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        payloads(&queue.show(TaskId::new(2)).unwrap(), "resume_started").len() == 2
    });
    options.stop.store(true, Ordering::SeqCst);
    // One slot resumes the run, not the dropped one beside the new attempt.
    assert_eq!(draining_runs(&db), json!([run.id()]));
    let run_dir = Path::new(run.run_dir().unwrap());
    fs::write(run_dir.join("exit-requested.go2"), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    // The first attempt's session ends last, unwatched, once the second's
    // wrapper recorded its exit: the test's wrappers share the test's pid,
    // so the first's exit, recorded meanwhile, would land on the second's
    // row. What the first's wrapper returns is not this test's subject: its
    // run was taken from it (here its row is gone or exited).
    fs::write(run_dir.join("exit-requested.go1"), "").unwrap();
    let first = workspace_of_attempt(&queue.show(TaskId::new(2)).unwrap(), 1);
    let wrapper = backend
        .sessions
        .lock()
        .unwrap()
        .iter_mut()
        .find(|(id, _)| *id == first)
        .and_then(|(_, session)| session.worker.take())
        .unwrap();
    let _ = joined(wrapper, "the first attempt's wrapper to return");
    backend.join();
    assert_eq!(outcome["outcome"], "stopped", "{outcome}");
    let disowned: Vec<&Value> = outcome["errors"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| {
            e["message"]
                .as_str()
                .unwrap()
                .contains("held by another process")
        })
        .collect();
    assert_eq!(disowned.len(), 1, "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    let started: Vec<&Value> = payloads(&detail, "resume_started")
        .into_iter()
        .map(|p| &p["attempt"])
        .collect();
    assert_eq!(started, [&json!(1), &json!(2)]);
    // Nothing more about the first attempt once its lease moved.
    let finished: Vec<&Value> = payloads(&detail, "resume_finished")
        .into_iter()
        .map(|p| &p["attempt"])
        .collect();
    assert_eq!(
        finished,
        [&json!(2)],
        "{:?}",
        run_events_after(&queue, &run, moved)
    );
    let acquired = detail
        .events
        .iter()
        .filter(|e| e.kind == "lease_acquired" && e.payload["reason"] == "resume")
        .map(|e| e.payload["previous_token"].clone())
        .collect::<Vec<_>>();
    // The second attempt replaced the moved, stale lease.
    assert_eq!(acquired.len(), 2, "{acquired:?}");
    assert_eq!(acquired[1], "taken");
}

/// The workspace the resume `attempt` of the task's run opened.
fn workspace_of_attempt(detail: &dagq::domain::TaskDetail, attempt: i64) -> String {
    payloads(detail, "workspace_created")
        .into_iter()
        .find(|p| p["resume_attempt"] == attempt)
        .unwrap()["workspace_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Whether `run`'s wrapper registered and has not exited.
fn wrapper_lives(db: &Path, run: &TaskRun) -> bool {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM run_processes WHERE run_id=?1 AND role='wrapper' AND exited_at IS NULL",
            [&run.id()],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        == 1
}

#[test]
fn a_historical_interactive_run_resumes_headless_and_records_its_conversion() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET worker_mode='interactive' WHERE id=?1",
            [run.id()],
        )
        .unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let token = LeaseToken::new("converted-resume");
    let (resumed, _) = queue
        .begin_resume(
            run.id(),
            &token,
            &sha(&first_landed),
            None,
            Default::default(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        resumed.worker_mode(),
        dagq::domain::worker::WorkerMode::Headless
    );
    assert_eq!(
        queue.run(run.id()).unwrap().worker_mode(),
        resumed.worker_mode()
    );
    let events = queue.run_events(run.id()).unwrap();
    let converted: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "worker_mode_converted")
        .collect();
    assert_eq!(converted.len(), 1);
    assert_eq!(
        converted[0].payload,
        json!({"phase":"resume", "from":"interactive", "to":"headless", "reason":"interactive_worker_removed"})
    );
    assert!(
        queue
            .begin_resume(
                run.id(),
                &LeaseToken::new("another"),
                &sha(&first_landed),
                None,
                Default::default()
            )
            .unwrap()
            .is_none()
    );
    assert_eq!(
        queue
            .run_events(run.id())
            .unwrap()
            .iter()
            .filter(|e| e.kind == "worker_mode_converted")
            .count(),
        1
    );
}

/// Hold the turns the session wrappers of `backend` start in their start
/// until the test writes the returned marker: a wrapper held there runs no
/// turn, heartbeats no more and does not read its exit request, as a
/// session that ignores that request. A turn whose script runs the
/// returned command first writes the marker itself and is not held.
fn hold_turn_starts(dir: &Path, backend: &mut TestWorkspace) -> (PathBuf, String) {
    let release = runtime_support::headless::ready_turn(dir, backend);
    (backend.headless_ready.clone().unwrap(), release)
}

/// The `exit_requested` events of the resume `attempt` of the task's run.
fn exit_requests_of_attempt(detail: &dagq::domain::TaskDetail, attempt: i64) -> usize {
    payloads(detail, "exit_requested")
        .into_iter()
        .filter(|p| p["resume_attempt"] == attempt)
        .count()
}

/// A resumed session that does not exit at its exit request is let go
/// after the exit timeout: the resume ends `unresolved` with
/// `exit_timed_out`, its lease is given back but its workspace stays open,
/// and no pass resumes the run next to the session while it runs; once it
/// ended, the next attempt closes that workspace and lands the run. The
/// session's wrapper is held in the start of its turn, so it neither works
/// nor reads the exit request the resume timeout made the supervisor write.
/// Moved from the interactive `a_resumed_session_that_ignores_exit_is_let_go`
/// (task 1437).
#[test]
fn a_resumed_session_that_ignores_its_exit_request_is_let_go_after_the_exit_timeout() {
    let (dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    let (held, release) = hold_turn_starts(dir.path(), &mut backend);
    let timeouts = (backend.exit_timeout, backend.resume_timeout);
    backend.exit_timeout = Duration::from_millis(500);
    backend.resume_timeout = Duration::from_secs(1);
    backend.resume_script_for(2, &format!("await_message; {HOLD}"));
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(finished[0]["outcome"], "unresolved");
    assert_eq!(finished[0]["exit_timed_out"], true);
    assert_eq!(finished[0]["workspace_closed"], false);
    // Asked once to exit, and let go without a second request.
    assert_eq!(exit_requests_of_attempt(&detail, 1), 1);
    // The first session's and the resume's, written once each.
    assert_exit_sent(&backend, &run, 2);
    assert!(queue.run_leases().unwrap().is_empty());
    let kept = workspace_of_attempt(&detail, 1);
    assert_eq!(finished[0]["workspace_id"], json!(kept));
    assert!(!backend.closed().contains(&kept));
    assert!(wrapper_lives(&db, &run));
    assert_eq!(
        run_attention_of(&runtime::status(&db).unwrap(), run.id()).unwrap()["next"],
        "resuming (runtime)"
    );
    // A pass while the session still runs does not resume the run.
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    assert_eq!(
        payloads(&queue.show(TaskId::new(2)).unwrap(), "resume_started").len(),
        1
    );

    // The session goes on, finds the exit request and ends; the workspace
    // it left no longer blocks the run.
    fs::write(&held, "").unwrap();
    backend.join();
    (backend.exit_timeout, backend.resume_timeout) = timeouts;
    backend.resume_script_for(
        2,
        &format!("{release}; await_message; resolve; receipt \"$(git rev-parse HEAD)\""),
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(backend.closed().contains(&kept));
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    assert_eq!(payloads(&detail, "resume_started").len(), 2);
}

/// A resumed session whose wrapper stops heartbeating while its process
/// lives on is asked to exit once, the silence recorded once as
/// `wrapper_heartbeat_expired`, and, as it does not exit, let go after the
/// exit timeout like a session that ignores its exit request: `unresolved`
/// with `exit_timed_out` and its workspace kept. Moved from the interactive
/// `a_resumed_session_with_a_silent_wrapper_is_asked_to_exit_then_let_go`
/// (task 1437).
#[test]
fn a_resumed_session_whose_wrapper_goes_silent_is_asked_to_exit_once_then_let_go() {
    let (dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    let (held, _) = hold_turn_starts(dir.path(), &mut backend);
    backend.exit_timeout = Duration::from_millis(500);
    let started = Path::new(run.run_dir().unwrap()).join("resume-turn-started");
    backend.resume_script_for(
        2,
        &format!(": > \"$RUN_DIR/resume-turn-started\"; await_message; {HOLD}"),
    );
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    // The turn of the request runs while its wrapper, held in the turn's
    // start, heartbeats no more: its last heartbeat is made older than the
    // timeout.
    wait_until(&db, Duration::from_secs(30), |_| started.exists());
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_processes SET heartbeat_at=unixepoch()-?2
             WHERE run_id=?1 AND role='wrapper' AND exited_at IS NULL",
            rusqlite::params![run.id(), dagq::domain::HEARTBEAT_TIMEOUT_SECS + 5],
        )
        .unwrap();
    // The silence, not the resume timeout (120 seconds), asks for the exit.
    wait_until(&db, Duration::from_secs(30), |queue| {
        !payloads(&queue.show(TaskId::new(2)).unwrap(), "resume_finished").is_empty()
    });
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    assert_eq!(
        payloads(&detail, "wrapper_heartbeat_expired").len(),
        1,
        "{kinds:?}"
    );
    assert_eq!(exit_requests_of_attempt(&detail, 1), 1, "{kinds:?}");
    assert!(
        position(&kinds, "wrapper_heartbeat_expired")
            < kinds.iter().rposition(|k| *k == "exit_requested").unwrap()
    );
    // The first session's and the resume's.
    assert_exit_sent(&backend, &run, 2);
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1, "{kinds:?}");
    assert_eq!(finished[0]["outcome"], "unresolved");
    assert_eq!(finished[0]["exit_timed_out"], true);
    assert_eq!(finished[0]["workspace_closed"], false);
    assert!(!backend.closed().contains(&workspace_of_attempt(&detail, 1)));
    fs::write(&held, "").unwrap();
    backend.join();
}
