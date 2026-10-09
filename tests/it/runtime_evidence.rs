//! Runtime tests: Required evidence and declared paths.
use crate::runtime_support;

use runtime_support::*;

/// A fixture whose only ready task requires `evidence` in the receipt.
fn evidence_fixture(evidence: &[EvidenceCheck]) -> (Fixture, PathBuf, PathBuf) {
    let (dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let task = queue
        .add(NewTask {
            title: "needs evidence".into(),
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
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap();
    assert_eq!(task.id(), TaskId::new(2));
    assert_eq!(task.required_evidence(), evidence);
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    (dir, repo, db)
}

/// A receipt function for scripts: `receipt_tests COMMIT STATUS EVIDENCE`
/// claims success with the given `tests` check.
const RECEIPT_TESTS: &str = r#"receipt_tests() {
  printf '{"run_id":"%s","result":"succeeded","commit":"%s","tests":{"status":"%s","evidence_or_reason":"%s"},"e2e":{"status":"not_applicable","evidence_or_reason":"the runtime runs the e2e"},"subagent_review":{"status":"passed","evidence_or_reason":"reviewed"},"summary":"done"}' "$RUN_ID" "$1" "$2" "$3" > "$RECEIPT.tmp"
  mv "$RECEIPT.tmp" "$RECEIPT"
}
"#;

/// A task that requires `tests` evidence gets a receipt without it parked
/// as `needs_session` (`evidence_missing`), not failed; the supervisor
/// resumes the session with the evidence request, and the rewritten
/// receipt with the evidence brings the run to `awaiting_integration`.
#[test]
fn missing_required_evidence_parks_the_run_for_a_resumed_session() {
    let (_dir, repo, db) = evidence_fixture(&[EvidenceCheck::Tests]);
    // The worker claims the tests passed but gives no evidence: without
    // the requirement that fails the receipt, with it the run waits.
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!("{RECEIPT_TESTS}commit work; receipt_tests \"$(git rev-parse HEAD)\" passed ' '"),
    );
    backend.resume_script_for(
        2,
        &format!(
            "{RECEIPT_TESTS}await_message; receipt_tests \"$(git rev-parse HEAD)\" passed 'cargo test: 3 passed'; idle; await_exit"
        ),
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    let run = &detail.runs[0];
    // The worker knew up front.
    let prompt = read_prompt(run);
    assert!(prompt.contains("Required evidence: tests ("), "{prompt}");
    // Validation parked it instead of failing it; the resolved resume is
    // validated again with its session open (ADR-0027 decision 3).
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 2);
    assert_eq!(validated[1]["status"], "awaiting_integration");
    assert_eq!(validated[0]["status"], "needs_session");
    assert_eq!(validated[0]["accepted"], false);
    assert_eq!(validated[0]["reason"], "evidence missing: tests");
    assert_eq!(validated[0]["evidence_missing"], json!(["tests"]));
    assert_eq!(validated[0]["code"], "evidence_missing");
    assert_eq!(validated[1].get("code"), None);
    assert!(validated[0]["result_commit"].is_string());
    assert_eq!(
        payloads(&detail, "evidence_missing"),
        [
            &json!({"code": "evidence_missing", "checks": ["tests"], "reason": "evidence missing: tests"})
        ]
    );
    // The worker's session was closed: the resume opens its own.
    assert!(event_kinds(&detail).contains(&"workspace_closed"));
    assert!(backend.closed().contains(&background_session(run)));
    // The resume asked for the missing check, not a rebase.
    let text = &session_texts(run)[0];
    assert!(
        text.contains("found required evidence missing from the receipt"),
        "{text}"
    );
    assert!(text.contains("Reason: evidence missing: tests"), "{text}");
    assert!(!text.contains("git rebase"), "{text}");
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["outcome"], "resolved");
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(queue.run_leases().unwrap().is_empty());
    // It lands now that the receipt carries the evidence.
    let landed = integrate(&db, 2, &repo).unwrap();
    assert_eq!(landed["outcome"], "integrated", "{landed}");
}

/// A resumed session that comes back without the evidence has not resolved
/// the run: every attempt is `unresolved`, and once the resumes are used up
/// the run is `failed` and goes to its recovery job (`resume_exhausted`),
/// which this provider cannot run: it waits to be recovered by hand. An
/// `integrate`
/// of such a run does not land either: it defers the run with the missing
/// `checks`.
#[test]
fn a_resume_or_integrate_without_the_required_evidence_does_not_land() {
    let (_dir, repo, db) = evidence_fixture(&[EvidenceCheck::Tests]);
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "{RECEIPT_TESTS}commit work; receipt_tests \"$(git rev-parse HEAD)\" not_applicable 'not run'"
        ),
    );
    backend.resume_script_for(
        2,
        &format!(
            "{RECEIPT_TESTS}await_message; receipt_tests \"$(git rev-parse HEAD)\" not_applicable 'still not run'; idle; await_exit"
        ),
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 3);
    assert!(finished.iter().all(|f| f["outcome"] == "unresolved"));
    assert_eq!(
        payloads(&detail, "recovery_requested")[0]["alert"],
        "resume_exhausted"
    );
    assert_eq!(payloads(&detail, "triage_failed").len(), 1);
    assert!(queue.asks(AskQuery::default()).unwrap().is_empty());
    // Parked for a session again (as an older runtime left it), the run is
    // still not landed by `integrate`.
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='needs_session' WHERE id=?1",
            [&detail.runs[0].id()],
        )
        .unwrap();
    let before = git_out(&repo, &["rev-parse", "main"]);
    let deferred = integrate(&db, 2, &repo).unwrap();
    assert_eq!(deferred["outcome"], "needs_session", "{deferred}");
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), before);
    let detail = queue.show(TaskId::new(2)).unwrap();
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::NeedsSession);
    assert_eq!(run.last_error(), Some("evidence missing: tests"));
    let parked = payloads(&detail, "integration_deferred");
    assert_eq!(parked.last().unwrap()["checks"], json!(["tests"]));
}

/// Write `[e2e] paths` of `dagq.toml` in the main checkout and commit it
/// (ADR-t963-1 decision 2).
fn with_e2e_paths(repo: &Path, globs: &str) {
    fs::write(repo.join("dagq.toml"), format!("[e2e]\npaths = {globs}\n")).unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-q", "-m", "e2e paths"]);
}

/// Whether a run needs the e2e is read from its diff against the `[e2e]
/// paths` of the main checkout's `dagq.toml` (ADR-t963-1 decision 2) and
/// recorded with its validation, but no receipt backs it: the runtime runs
/// it after the review (ADR-t1233-2). A receipt reporting `e2e`
/// `not_applicable` is accepted at once, and the worker is told not to run
/// it. A diff outside the paths and a task's own `e2e` are judged by
/// `domain::validation`'s unit tests.
#[test]
fn the_e2e_a_run_needs_is_recorded_and_its_receipt_backs_none() {
    let (_dir, repo, db) = evidence_fixture(&[]);
    with_e2e_paths(&repo, "[\"change.txt\", 'tests/e2e.rs']");
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "{RECEIPT_TESTS}commit work; receipt_tests \"$(git rev-parse HEAD)\" passed 'tests: 3 passed'"
        ),
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    let run = &detail.runs[0];
    let prompt = read_prompt(run);
    assert!(prompt.contains("E2E: do not run the e2e"), "{prompt}");
    assert!(!prompt.contains("E2E evidence"), "{prompt}");
    assert!(!prompt.contains("Required evidence:"), "{prompt}");
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 1);
    assert_eq!(validated[0]["status"], "awaiting_integration");
    assert_eq!(
        validated[0]["e2e_requirement"],
        json!({"required": true, "source": "paths", "paths": ["change.txt"]})
    );
    assert!(!event_kinds(&detail).contains(&"evidence_missing"));
    assert!(!event_kinds(&detail).contains(&"resume_started"));
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
}

/// A fixture whose only ready task declares `paths` (ADR-0029).
fn scope_fixture(paths: &[&str]) -> (Fixture, PathBuf, PathBuf) {
    let (dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let task = queue
        .add(NewTask {
            title: "scoped".into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: Vec::new(),
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            priority: Default::default(),
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap();
    assert_eq!(task.id(), TaskId::new(2));
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    (dir, repo, db)
}

/// A run of a task declaring `docs/**` that changes `change.txt` is parked
/// by validation as `needs_session` (`scope_violation`, with the paths),
/// not accepted; the supervisor resumes the session with a request to take
/// the path out, and the resolved run is validated again and lands.
#[test]
fn a_change_outside_the_declared_paths_parks_the_run_for_a_resumed_session() {
    let (_dir, repo, db) = scope_fixture(&["docs/**", "*.md"]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.resume_script_for(
        2,
        "await_message; unlocked git rm -q change.txt && mkdir -p docs && printf 'doc\\n' > docs/a.md && unlocked git add docs && unlocked git commit -q -m 'keep to docs'; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    let run = &detail.runs[0];
    let prompt = read_prompt(run);
    assert!(
        prompt.contains("Paths you may change (globs from the repository root; `*` stays in one directory, `**` spans any depth): docs/**, *.md."),
        "{prompt}"
    );
    let reason = "changed paths outside the task's --paths: change.txt";
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 2);
    assert_eq!(validated[0]["status"], "needs_session");
    assert_eq!(validated[0]["accepted"], false);
    assert_eq!(validated[0]["reason"], reason);
    assert_eq!(validated[0]["scope_violation"], json!(["change.txt"]));
    assert_eq!(validated[0]["allowed_paths"], json!(["docs/**", "*.md"]));
    assert!(validated[0]["result_commit"].is_string());
    assert_eq!(
        payloads(&detail, "scope_violation"),
        [
            &json!({"code": "scope_violation", "paths": ["change.txt"], "allowed": ["docs/**", "*.md"], "reason": reason})
        ]
    );
    assert!(!event_kinds(&detail).contains(&"evidence_missing"));
    // The resume asked to take the path out, not for a rebase or evidence.
    let text = &session_texts(run)[0];
    assert!(
        text.contains("changes paths outside the task's --paths (docs/**, *.md)"),
        "{text}"
    );
    assert!(text.contains(&format!("Reason: {reason}")), "{text}");
    assert!(text.contains("Take the changes to the paths"), "{text}");
    assert_eq!(
        payloads(&detail, "resume_finished")[0]["outcome"],
        "resolved"
    );
    assert_eq!(validated[1]["status"], "awaiting_integration");
    assert!(
        !validated[1]
            .as_object()
            .unwrap()
            .contains_key("scope_violation")
    );
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let landed = integrate(&db, 2, &repo).unwrap();
    assert_eq!(landed["outcome"], "integrated", "{landed}");
    assert_eq!(
        git_out(&repo, &["show", "--name-only", "--format=", "main"]).trim(),
        "docs/a.md"
    );
}

/// Validation diffs from where the branch forked from the current main,
/// not from the base commit: a branch rebased onto a main that gained
/// `src/lib.rs` from another task changes only `change.txt` itself.
#[test]
fn a_branch_rebased_onto_a_moved_main_is_held_only_to_its_own_changes() {
    let (_dir, repo, db) = scope_fixture(&["*.txt"]);
    // Another task lands src/lib.rs on main while the worker runs, and the
    // worker rebases onto it before its receipt.
    let backend = TestWorkspace::new(
        &db,
        false,
        "git switch -q -c side main && mkdir -p src && printf 'x\\n' > src/lib.rs && git add src && git commit -q -m other && git update-ref refs/heads/main HEAD && git switch -q - && git branch -q -D side && commit work && git rebase -q main; receipt \"$(git rev-parse HEAD)\"",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    assert!(!event_kinds(&detail).contains(&"scope_violation"));
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    let landed = integrate(&db, 2, &repo).unwrap();
    assert_eq!(landed["outcome"], "integrated", "{landed}");
    assert_eq!(
        git_out(&repo, &["show", "--name-only", "--format=", "main"]).trim(),
        "change.txt"
    );
}

/// `integrate` holds the diff it squashes (main..rebased head) to the
/// task's paths after its rebase: a commit outside them that a session
/// added after validation defers the run to `needs_session` without moving
/// main or running the verification commands, and the supervisor's resume
/// asks to take it out and then lands the approved run.
#[test]
fn integrate_refuses_a_rebased_diff_outside_the_declared_paths() {
    let (_dir, repo, db) = scope_fixture(&["*.txt"]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    // main moves, so the landing rebases.
    fs::write(repo.join("other.txt"), "main moved\n").unwrap();
    git(&repo, &["add", "other.txt"]);
    git(&repo, &["commit", "-m", "main moved"]);
    let main = git_out(&repo, &["rev-parse", "main"]);
    // After validation the branch gains a path outside `*.txt`.
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    fs::create_dir_all(worktree.join("src")).unwrap();
    fs::write(worktree.join("src/lib.rs"), "// out of scope\n").unwrap();
    git(&worktree, &["add", "src"]);
    git(&worktree, &["commit", "-m", "outside"]);
    write_receipt(
        &run,
        &git_out(&worktree, &["rev-parse", "HEAD"]),
        "succeeded",
        "more",
    );

    let deferred = integrate(&db, 2, &repo).unwrap();
    assert_eq!(deferred["outcome"], "needs_session", "{deferred}");
    assert_eq!(git_out(&repo, &["rev-parse", "main"]), main);
    let detail = queue.show(TaskId::new(2)).unwrap();
    let parked = &detail.runs[0];
    assert_eq!(parked.status(), RunStatus::NeedsSession);
    let reason = parked.last_error().unwrap();
    assert!(
        reason.starts_with(&format!(
            "changed paths outside the task's --paths: src/lib.rs after the rebase onto main {}",
            main.trim()
        )),
        "{reason}"
    );
    assert!(event_kinds(&detail).contains(&"integration_rebased"));
    let payload = payloads(&detail, "integration_deferred")[0];
    assert_eq!(payload["scope_violation"], json!(["src/lib.rs"]));
    assert_eq!(payload["allowed"], json!(["*.txt"]));
    assert!(integration_verifications(&detail).is_empty());

    // The supervisor resumes it with the scope request and lands it.
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.resume_script_for(
        2,
        "await_message; unlocked git rm -q -r src && unlocked git commit -q -m 'back to scope'; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let text = &session_texts(&run)[0];
    assert!(
        text.contains("changes paths outside the task's --paths (*.txt)"),
        "{text}"
    );
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(
        detail.task.status(),
        TaskStatus::Completed,
        "{:?}",
        event_kinds(&detail)
    );
    assert_eq!(
        git_out(&repo, &["show", "--name-only", "--format=", "main"]).trim(),
        "change.txt"
    );
}
