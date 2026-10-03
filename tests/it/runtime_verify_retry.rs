//! Runtime tests: a verification command of `integrate` that failed on the
//! host (a full disk, a kill, a timeout) is retried once in the same
//! attempt instead of resuming the worker, and waits for a person when it
//! fails so again (task 639); a failure of the code is still resumed.
use crate::runtime_support;

use dagq::domain::disk::DiskConfig;
use runtime_support::*;
use std::sync::atomic::AtomicUsize;

const GIB: u64 = 1 << 30;

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

/// A command that fails on a full disk the first time and passes the
/// next: its marker file lives outside the worktree.
fn full_disk_once(marker: &Path) -> String {
    format!(
        "if [ -f {0} ]; then exit 0; fi; touch {0}; echo 'error: failed to write: No space left on device (os error 28)'; exit 1",
        shell_path(marker)
    )
}

/// A command that failed on a full disk is run once more in the same
/// attempt, with its own log and marked as the retry on its event; it
/// passes, and the run lands without a resume. `stats` counts the retry
/// under the class it retried.
#[test]
fn a_command_that_failed_on_the_host_is_retried_and_lands_when_it_passes() {
    headless_workers();
    let (dir, db, repo) = awaiting();
    let marker = dir.path().join("full once");
    set_commands(&db, json!(["true", full_disk_once(&marker)]));
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    let detail = show(&db);
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 3, "{verifications:?}");
    let (first, retry) = (verifications[1], verifications[2]);
    assert_eq!(first["failure"]["class"], "disk_full");
    assert!(first.get("retry").is_none(), "{first}");
    assert_eq!(retry["retry"], true);
    assert_eq!(retry["index"], 2);
    assert_eq!(retry["attempt"], first["attempt"]);
    assert_eq!(retry["exit_code"], 0);
    assert_eq!(retry["failure"], Value::Null);
    assert_eq!(retry["retry_of"]["failure"], first["failure"]);
    assert_eq!(retry["retry_of"]["log_path"], first["log_path"]);
    let first_log = first["log_path"].as_str().unwrap();
    let retry_log = retry["log_path"].as_str().unwrap();
    assert!(
        retry_log.ends_with("integrate-1-verify-2-retry.log"),
        "{retry_log}"
    );
    // The first failure's log is kept as it was.
    assert!(
        fs::read_to_string(first_log)
            .unwrap()
            .contains("No space left on device")
    );
    assert!(payloads(&detail, "integration_deferred").is_empty());
    assert!(payloads(&detail, "resume_started").is_empty());
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(
        stats["verification_failures"],
        json!([{"class": "disk_full", "count": 1, "runs": 1, "retried": 1, "retry_passed": 1, "retry_failed": 0}]),
        "{stats}"
    );
}

/// Under the supervisor, a command killed again on its retry leaves the
/// run awaiting integration without a resume: `integration_held` names
/// both failures and logs, and the inbox sees one attention for the run
/// (`review and integrate`) with the cause. A person's `integrate` lands
/// it once the host is fixed.
#[test]
fn a_command_that_fails_on_the_host_again_waits_for_a_person_without_a_resume() {
    headless_workers();
    let (_dir, repo, db) = fixture();
    set_commands(&db, json!(["kill -TERM $$"]));
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = show(&db);
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 2, "{verifications:?}");
    assert_eq!(verifications[1]["retry"], true);
    let held = payloads(&detail, "integration_held");
    assert_eq!(held.len(), 1, "{:?}", event_kinds(&detail));
    let held = held[0];
    let killed = json!({"class": "killed", "evidence": "killed by signal 15 (SIGTERM)"});
    assert_eq!(held["code"], "verification_environment");
    assert_eq!(held["status"], "awaiting_integration");
    assert_eq!(held["index"], 1);
    assert_eq!(held["retried"], true);
    assert_eq!(held["failure"], killed);
    assert_eq!(held["first_failure"], killed);
    assert_eq!(held["log_path"], verifications[1]["log_path"]);
    assert_eq!(held["first_log_path"], verifications[0]["log_path"]);
    assert_eq!(held["disk"], Value::Null);
    // No resume: nothing parked it for a session.
    for kind in ["integration_deferred", "resume_started"] {
        assert!(payloads(&detail, kind).is_empty(), "{kind}");
    }
    let status = runtime::status(&db).unwrap();
    let attentions: Vec<&Value> = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["run_id"] == run.id().as_str())
        .collect();
    assert_eq!(attentions.len(), 1, "{status}");
    let attention = attentions[0];
    assert_eq!(attention["next"], "review and integrate");
    assert_eq!(attention["kind"], "integration_held");
    assert_eq!(attention["last_error_code"], "verification_environment");
    let shown = attention["last_error"].as_str().unwrap();
    assert!(shown.contains("killed: killed by signal 15"), "{shown}");
    // The whole reason is the run's `last_error` (status cuts it short).
    let last_error = run.last_error().unwrap();
    assert!(
        last_error.contains(held["log_path"].as_str().unwrap()),
        "{last_error}"
    );
    assert!(last_error.contains("dagq integrate 1"), "{last_error}");
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(
        stats["verification_failures"],
        json!([{"class": "killed", "count": 2, "runs": 1, "retried": 1, "retry_passed": 0, "retry_failed": 1}]),
        "{stats}"
    );

    // The host is fixed: a person lands it.
    set_commands(&db, json!(["true"]));
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
}

/// A failure of the code is not retried and parks the run for a resume as
/// before; so does a retry that fails in the code after a kill.
#[test]
fn a_failure_of_the_code_is_resumed_as_before() {
    headless_workers();
    let (dir, db, repo) = awaiting();
    set_commands(
        &db,
        json!([
            r#"echo 'error[E0425]: cannot find value `x`'; echo '  --> src/lib.rs:1:1'; exit 101"#
        ]),
    );
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let detail = show(&db);
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 1, "{verifications:?}");
    let deferred = payloads(&detail, "integration_deferred");
    assert_eq!(deferred[0]["code"], "verification_failed");
    assert_eq!(deferred[0]["failure"]["class"], "build_error");
    assert!(payloads(&detail, "integration_held").is_empty());

    // Killed first, then a failing test on the retry: the code's failure.
    let marker = dir.path().join("killed once");
    set_commands(
        &db,
        json!([format!(
            "if [ -f {0} ]; then echo 'test a::b ... FAILED'; echo 'test result: FAILED. 0 passed; 1 failed'; exit 101; fi; touch {0}; kill -KILL $$",
            shell_path(&marker)
        )]),
    );
    let outcome = integrate(&db, 1, &repo).unwrap();
    assert_eq!(outcome["outcome"], "needs_session", "{outcome}");
    let detail = show(&db);
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 3, "{verifications:?}");
    assert_eq!(verifications[1]["failure"]["class"], "killed");
    assert_eq!(verifications[2]["retry"], true);
    let deferred = payloads(&detail, "integration_deferred");
    assert_eq!(deferred[1]["failure"]["class"], "test_failure");
    assert_eq!(deferred[1]["failed_tests"], json!(["a::b"]));
    assert!(
        deferred[1]["reason"]
            .as_str()
            .unwrap()
            .contains("-retry.log"),
        "{}",
        deferred[1]
    );
    assert!(payloads(&detail, "integration_held").is_empty());
}

/// A command that runs past its limit for the whole command is killed and
/// recorded as a `timeout` failure of the command, not a landing error: it
/// is retried, and held for a person when it runs out of time again.
#[test]
fn a_command_past_its_whole_limit_is_a_timeout_failure() {
    headless_workers();
    let (_dir, db, repo) = awaiting();
    set_commands(&db, json!(["sleep 30"]));
    let outcome = runtime::OneShot {
        verification_timeout: Duration::from_secs(1),
        ..runtime::OneShot::system()
    }
    .integrate(&db, IntegrateTarget::Task(TaskId::new(1)), &repo, None)
    .unwrap();
    assert_eq!(outcome["outcome"], "held", "{outcome}");
    let detail = show(&db);
    assert!(payloads(&detail, "integration_error").is_empty());
    let timeout = json!({"class": "timeout", "evidence": "the command ran past its 1 s limit and was killed"});
    let verifications = integration_verifications(&detail);
    assert_eq!(verifications.len(), 2, "{verifications:?}");
    for verification in &verifications {
        assert_eq!(verification["failure"], timeout);
        assert_eq!(verification["exit_code"], 128);
        assert_eq!(verification["signal"], 9);
    }
    let held = payloads(&detail, "integration_held");
    assert_eq!(held[0]["failure"], timeout);
    assert_eq!(held[0]["code"], "verification_environment");
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
}

/// The reads of the free space in the next test: room for the approval,
/// short afterwards.
static READS: AtomicUsize = AtomicUsize::new(0);

fn room_then_short(_: &Path) -> Option<u64> {
    Some(if READS.fetch_add(1, Ordering::SeqCst) == 0 {
        2 * GIB
    } else {
        GIB / 2
    })
}

/// A command that failed on a full disk is not retried while the free
/// space is short of a landing's threshold: the run is held at once, with
/// what is free and what is needed.
#[test]
fn a_full_disk_without_room_is_held_without_a_retry() {
    headless_workers();
    let (dir, db, repo) = awaiting();
    READS.store(0, Ordering::SeqCst);
    set_commands(&db, json!([full_disk_once(&dir.path().join("full once"))]));
    let outcome = runtime::OneShot {
        disk: Some(DiskConfig {
            min_free_bytes: Some(GIB),
            ..DiskConfig::default()
        }),
        free_space: room_then_short,
        ..runtime::OneShot::system()
    }
    .integrate(&db, IntegrateTarget::Task(TaskId::new(1)), &repo, None)
    .unwrap();
    assert_eq!(outcome["outcome"], "held", "{outcome}");
    let reason = outcome["reason"].as_str().unwrap();
    assert!(
        reason.contains("it was not retried: 0.5 GiB free"),
        "{reason}"
    );
    let detail = show(&db);
    assert_eq!(integration_verifications(&detail).len(), 1);
    let held = payloads(&detail, "integration_held");
    assert_eq!(held[0]["retried"], false);
    assert_eq!(held[0]["failure"]["class"], "disk_full");
    assert_eq!(held[0]["first_failure"], Value::Null);
    assert_eq!(
        held[0]["disk"],
        json!({"free_bytes": GIB / 2, "needed_bytes": GIB, "largest_build_bytes": null})
    );
}
