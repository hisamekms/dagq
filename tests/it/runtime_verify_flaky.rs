//! Flaky-only verification retries once per landing with nextest flaky-result=pass.
use crate::runtime_support;
use dagq::domain::LeaseToken;

use runtime_support::*;

/// A run awaiting integration, its repository, and the queue.
fn awaiting() -> (Fixture, PathBuf, PathBuf) {
    let (dir, db, detail) = run_agent(
        "echo a > a.txt && git add a.txt && git commit -q -m a; receipt \"$(git rev-parse HEAD)\"",
    );
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    let repo = Path::new(&db).parent().unwrap().join("repo's directory");
    (dir, db, repo)
}

fn set_commands(db: &Path, commands: Value) {
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE tasks SET verification_commands=?1 WHERE id=1",
            [commands.to_string()],
        )
        .unwrap();
}

fn show(db: &Path) -> dagq::domain::TaskDetail {
    SqliteQueue::open(db).unwrap().show(TaskId::new(1)).unwrap()
}

/// nextest's output of a run where `runtime_x::flaky` failed and passed on
/// its retry and, when `broken`, `runtime_x::broken` failed both times.
fn nextest_log(broken: bool) -> String {
    let mut lines = vec![
        "  TRY 1 FAIL [   0.010s] (───) dagq::it runtime_x::flaky",
        "  stderr ───",
        "    thread 'runtime_x::flaky' (4242) panicked at tests/it/runtime_x.rs:8:9:",
        "  TRY 2 PASS [   0.007s] (1/3) dagq::it runtime_x::flaky",
    ];
    if broken {
        lines.extend([
            "  TRY 1 FAIL [   0.006s] (───) dagq::it runtime_x::broken",
            "  TRY 2 FAIL [   0.006s] (2/3) dagq::it runtime_x::broken",
        ]);
    }
    lines.extend([
        "────────────",
        "     Summary [   0.017s] 3 tests run: 1 passed, 2 failed, 0 skipped",
    ]);
    if broken {
        lines.push("  TRY 2 FAIL [   0.006s] (2/3) dagq::it runtime_x::broken");
    }
    lines.extend([
        " FLKY-FL 2/2 [   0.007s] (1/3) dagq::it runtime_x::flaky",
        "error: test run failed",
    ]);
    lines.join("\n") + "\n"
}

/// A command that prints `log` and exits 100 as nextest does, or passes
/// with a FLAKY summary when the retry explicitly allows flakes.
fn nextest_command(dir: &Path, name: &str, log: &str, pass_after: bool) -> String {
    let output = dir.join(format!("{name}.log"));
    fs::write(&output, log).unwrap();
    if pass_after {
        format!(
            "if [ \"$NEXTEST_FLAKY_RESULT\" = pass ]; then echo ' FLAKY 2/2 [ 0.007s] dagq::it runtime_x::retry_flaky'; exit 0; fi; cat {}; exit 100",
            shell_path(&output)
        )
    } else {
        format!("cat {}; exit 100", shell_path(&output))
    }
}

/// Only flaky tests failed: the landing's verification is done once more
/// on the same rebased head (main moved since the run, so the rebase moved
/// the branch past the receipt's commit), a new attempt with its own logs,
/// recorded as `integration_retried`; it passes and the run lands without
/// a resume. The flaky test is named, marked, and a flaky candidate of
/// `stats` from its first failure.
#[test]
fn only_flaky_tests_failing_lands_the_run_once_more_without_a_resume() {
    let (dir, db, repo) = awaiting();
    fs::write(repo.join("b.txt"), "b\n").unwrap();
    git(&repo, &["add", "b.txt"]);
    git(&repo, &["commit", "-q", "-m", "b on main"]);
    let flaky = nextest_command(dir.path(), "flaky", &nextest_log(false), true);
    set_commands(&db, json!(["true", flaky]));
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    let detail = show(&db);
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 4, "{verifications:?}");
    let first = verifications[1];
    assert_eq!(first["attempt"], 1);
    assert_eq!(first["failure"]["class"], "flaky");
    assert_eq!(
        first["failure"]["evidence"],
        "FLKY-FL 2/2 [   0.007s] (1/3) dagq::it runtime_x::flaky"
    );
    assert_eq!(first["failed_tests"], json!(["runtime_x::flaky"]));
    assert_eq!(first["flaky_tests"], json!(["runtime_x::flaky"]));
    // The second landing ran every command again.
    for again in &verifications[2..] {
        assert_eq!(again["attempt"], 2);
        assert_eq!(again["exit_code"], 0);
    }
    assert!(
        verifications[3]["log_path"]
            .as_str()
            .unwrap()
            .ends_with("integrate-2-verify-2.log")
    );
    assert_eq!(
        verifications[3]["flaky_tests"],
        json!(["runtime_x::retry_flaky"])
    );
    assert_eq!(
        verifications[3]["failed_tests"],
        json!(["runtime_x::retry_flaky"])
    );
    let retried = payloads(&detail, "integration_retried");
    assert_eq!(retried.len(), 1, "{:?}", event_kinds(&detail));
    let retried = retried[0];
    assert_eq!(retried["code"], "verification_flaky");
    assert_eq!(retried["flaky_result"], "pass");
    assert_eq!(retried["index"], 2);
    assert_eq!(retried["attempt"], 1);
    assert_eq!(retried["failure"], first["failure"]);
    assert_eq!(retried["flaky_tests"], json!(["runtime_x::flaky"]));
    assert_eq!(retried["log_path"], first["log_path"]);
    assert_eq!(
        retried["head"],
        payloads(&detail, "integration_rebased")[0]["head_after"]
    );
    assert_ne!(
        payloads(&detail, "integration_rebased")[0]["head_before"],
        retried["head"]
    );
    // One landing: one receipt read, one rebase.
    assert_eq!(integration_receipts(&detail).len(), 1);
    for kind in ["integration_deferred", "integration_held", "resume_started"] {
        assert!(payloads(&detail, kind).is_empty(), "{kind}");
    }
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(
        stats["verification_failures"],
        json!([{"class": "flaky", "count": 1, "runs": 1, "retried": 1, "retry_passed": 1, "retry_failed": 0}]),
        "{stats}"
    );
    let candidates = &stats["failed_tests"]["flaky_candidates"];
    assert_eq!(candidates.as_array().unwrap().len(), 2, "{stats}");
    assert_eq!(candidates[0]["name"], "runtime_x::flaky");
    assert_eq!(candidates[0]["flaky"], 1);
    assert_eq!(candidates[0]["integrate_runs"], 1);
    assert_eq!(candidates[1]["name"], "runtime_x::retry_flaky");
    assert_eq!(candidates[1]["flaky"], 1);
}

/// A real code failure on the retry needs a resume. Once resolved, a new
/// landing gets its own retry even though this run has already used one.
#[test]
fn a_failed_retry_resumes_and_a_later_landing_can_retry_again() {
    let (dir, db, repo) = awaiting();
    let flaky = nextest_command(dir.path(), "flaky", &nextest_log(false), false);
    let broken = nextest_command(dir.path(), "broken", &nextest_log(true), false);
    set_commands(
        &db,
        json!([format!(
            "if [ \"$NEXTEST_FLAKY_RESULT\" = pass ]; then {broken}; else {flaky}; fi"
        )]),
    );
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let detail = show(&db);
    assert_eq!(integration_verifications(&detail).len(), 2);
    assert_eq!(payloads(&detail, "integration_retried").len(), 1);
    let deferred = payloads(&detail, "integration_deferred");
    assert_eq!(deferred.len(), 1);
    assert_eq!(deferred[0]["code"], "verification_failed");
    assert_eq!(deferred[0]["failure"]["class"], "test_failure");

    let run = &detail.runs[0];
    let token = LeaseToken::new("resolved-code-failure");
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .begin_resume(
            run.id(),
            &token,
            &sha(deferred[0]["main"].as_str().unwrap()),
            None,
            Default::default(),
        )
        .unwrap()
        .unwrap();
    queue
        .finish_resume(
            run.id(),
            &token,
            Some(RunStatus::AwaitingIntegration),
            None,
            false,
            json!({"attempt": 1, "outcome": "resolved"}),
        )
        .unwrap();
    drop(queue);
    set_commands(
        &db,
        json!([nextest_command(
            dir.path(),
            "resolved",
            &nextest_log(false),
            true
        )]),
    );
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    let detail = show(&db);
    assert_eq!(integration_verifications(&detail).len(), 4);
    assert_eq!(payloads(&detail, "integration_retried").len(), 2);
    assert_eq!(payloads(&detail, "integration_deferred").len(), 1);
    assert_eq!(payloads(&detail, "resume_started").len(), 1);
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    let flaky = stats["verification_failures"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["class"] == "flaky")
        .unwrap();
    assert_eq!(flaky["retried"], 2);
    assert_eq!(flaky["retry_passed"], 1);
    assert_eq!(flaky["retry_failed"], 1);
}

/// Permitting FLAKY does not bypass the coverage gate or host handling.
#[test]
fn a_flaky_retry_keeps_coverage_and_host_failures() {
    for (log, class, outcome_name) in [
        ("TOTAL 70%", "coverage_below", "needs_session"),
        ("No space left on device", "disk_full", "held"),
    ] {
        let (dir, db, repo) = awaiting();
        let flaky = nextest_command(dir.path(), "flaky", &nextest_log(false), false);
        // Include the coverage flag in the shell comment, as in the real command.
        set_commands(
            &db,
            json!([format!(
                "if [ \"$NEXTEST_FLAKY_RESULT\" = pass ]; then echo ' FLAKY 2/2 [ 0.007s] dagq::it runtime_x::flaky'; echo '{log}'; exit 1; else {flaky}; fi # --fail-under-lines 80"
            )]),
        );
        let outcome = integrate(&db, 1, &repo).unwrap();
        assert_eq!(outcome["outcome"], outcome_name, "{outcome}");
        let detail = show(&db);
        let verifications = integration_verifications(&detail);
        assert_eq!(verifications.last().unwrap()["failure"]["class"], class);
        assert_eq!(payloads(&detail, "integration_retried").len(), 1);
        assert!(payloads(&detail, "run_integrated").is_empty());
        assert!(payloads(&detail, "resume_started").is_empty());
    }
}

/// A test that failed its retry too is not flaky: the run is resumed at
/// once, and the flaky test of the same command is still marked.
#[test]
fn a_failure_that_is_not_flaky_is_resumed_at_once() {
    let (dir, db, repo) = awaiting();
    set_commands(
        &db,
        json!([nextest_command(
            dir.path(),
            "broken",
            &nextest_log(true),
            true
        )]),
    );
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let detail = show(&db);
    assert_eq!(integration_verifications(&detail).len(), 1);
    assert!(payloads(&detail, "integration_retried").is_empty());
    let deferred = payloads(&detail, "integration_deferred");
    assert_eq!(deferred[0]["failure"]["class"], "test_failure");
    assert_eq!(
        deferred[0]["failed_tests"],
        json!(["runtime_x::flaky", "runtime_x::broken"])
    );
    assert_eq!(deferred[0]["flaky_tests"], json!(["runtime_x::flaky"]));
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    let candidates = &stats["failed_tests"]["flaky_candidates"];
    assert_eq!(candidates.as_array().unwrap().len(), 1, "{stats}");
    assert_eq!(candidates[0]["name"], "runtime_x::flaky");
}

/// Under the supervisor the landing is done once more the same way: the
/// run lands after its passed review without a resume.
#[test]
fn the_supervisor_lands_a_run_with_only_flaky_failures_once_more() {
    let (dir, repo, db) = fixture();
    let flaky = nextest_command(dir.path(), "flaky", &nextest_log(false), true);
    set_commands(&db, json!([flaky]));
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = show(&db);
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert_eq!(payloads(&detail, "integration_retried").len(), 1);
    assert_eq!(integration_verifications(&detail).len(), 2);
    for kind in ["integration_deferred", "resume_started"] {
        assert!(payloads(&detail, kind).is_empty(), "{kind}");
    }
}
