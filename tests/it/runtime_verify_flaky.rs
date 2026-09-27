//! Runtime tests: a verification command of `integrate` whose failed tests
//! all passed when nextest ran them again (`FLKY-FL`, `retries = 1` and
//! `flaky-result = "fail"`) is still a failure, but the landing is done
//! once more instead of resuming the worker, once per run; a second
//! failure and a failure that is not flaky are resumed as before (task
//! 768, ADR-t768-1). The commands print nextest's output as 0.9.146 does.
use crate::runtime_support;

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
/// once `pass_after` exists (it is created on the first run).
fn nextest_command(dir: &Path, name: &str, log: &str, pass_after: bool) -> String {
    let output = dir.join(format!("{name}.log"));
    fs::write(&output, log).unwrap();
    let marker = dir.join(format!("{name}.ran"));
    if pass_after {
        format!(
            "if [ -f '{0}' ]; then exit 0; fi; touch '{0}'; cat '{1}'; exit 100",
            marker.display(),
            output.display()
        )
    } else {
        format!("cat '{}'; exit 100", output.display())
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
        assert_eq!(again["flaky_tests"], json!([]));
    }
    assert!(
        verifications[3]["log_path"]
            .as_str()
            .unwrap()
            .ends_with("integrate-2-verify-2.log")
    );
    let retried = payloads(&detail, "integration_retried");
    assert_eq!(retried.len(), 1, "{:?}", event_kinds(&detail));
    let retried = retried[0];
    assert_eq!(retried["code"], "verification_flaky");
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
    assert_eq!(candidates.as_array().unwrap().len(), 1, "{stats}");
    assert_eq!(candidates[0]["name"], "runtime_x::flaky");
    assert_eq!(candidates[0]["flaky"], 1);
    assert_eq!(candidates[0]["integrate_runs"], 1);
}

/// The landing is done once more once per run: flaky again, the run is
/// parked for a resume as before, and a later landing of the same run is
/// not done again either.
#[test]
fn a_second_flaky_failure_is_resumed() {
    let (dir, db, repo) = awaiting();
    set_commands(
        &db,
        json!([nextest_command(
            dir.path(),
            "flaky",
            &nextest_log(false),
            false
        )]),
    );
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let detail = show(&db);
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 2, "{verifications:?}");
    assert_eq!(payloads(&detail, "integration_retried").len(), 1);
    let deferred = payloads(&detail, "integration_deferred");
    assert_eq!(deferred.len(), 1);
    assert_eq!(deferred[0]["code"], "verification_failed");
    assert_eq!(deferred[0]["failure"]["class"], "flaky");
    assert_eq!(deferred[0]["flaky_tests"], json!(["runtime_x::flaky"]));
    assert!(
        deferred[0]["reason"]
            .as_str()
            .unwrap()
            .contains("integrate-2-verify-1.log"),
        "{}",
        deferred[0]
    );

    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let detail = show(&db);
    assert_eq!(integration_verifications(&detail).len(), 3);
    assert_eq!(payloads(&detail, "integration_retried").len(), 1);
    assert_eq!(payloads(&detail, "integration_deferred").len(), 2);
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(
        stats["verification_failures"],
        json!([{"class": "flaky", "count": 3, "runs": 1, "retried": 1, "retry_passed": 0, "retry_failed": 1}]),
        "{stats}"
    );
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
