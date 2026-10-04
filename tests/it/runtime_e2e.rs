//! Runtime tests: the e2e the runtime runs on the host after a run's review
//! passes and before it lands (ADR-t1233-2), with a stub in place of
//! `cargo test --locked --test e2e -- --ignored`.
use crate::runtime_support;

use runtime_support::*;

/// Write `[e2e] paths` of `dagq.toml` in the main checkout and commit it
/// (ADR-t963-1 decision 2).
pub(crate) fn with_e2e_paths(repo: &Path, globs: &str) {
    fs::write(repo.join("dagq.toml"), format!("[e2e]\npaths = {globs}\n")).unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-q", "-m", "e2e paths"]);
}

/// A stub e2e: it appends what it ran on (the directory, the head and the
/// tests rerun by name) to `ran`, passes in a worktree with `fixed.txt`
/// and otherwise fails `a_test` as libtest prints it.
pub(crate) fn stub_e2e(ran: &Path) -> String {
    format!(
        "printf 'pwd=%s head=%s rerun=%s\\n' \"$PWD\" \"$(git rev-parse HEAD)\" \"${{DAGQ_E2E_RERUN:-}}\" >> {ran}; \
         if [ -f fixed.txt ]; then echo 'test result: ok. 1 passed; 0 failed'; exit 0; fi; \
         echo 'test a_test ... FAILED'; echo; echo 'test result: FAILED. 0 passed; 1 failed'; exit 101",
        ran = shell_join(&[ran.display().to_string()]),
    )
}

pub(crate) fn e2e_options(command: String) -> SuperviseOptions {
    SuperviseOptions {
        run_e2e: runtime::RunE2eOptions {
            command: Some(command),
            ..Default::default()
        },
        ..supervise_options(4, true)
    }
}

fn lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A run whose diff touches `[e2e] paths` passes its review, runs the e2e
/// in its worktree at the reviewed commit with its output in the run
/// directory, and lands once it passes; the worker was told not to run it
/// and its receipt backs none.
#[test]
fn a_passed_run_that_needs_the_e2e_runs_it_on_the_host_and_lands() {
    let (dir, repo, db) = fixture();
    with_e2e_paths(&repo, "[\"change.txt\"]");
    let ran = dir.path().join("ran");
    let backend = TestWorkspace::new(
        &db,
        false,
        "printf 'fixed\\n' > fixed.txt; git add fixed.txt; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed_with(
        &db,
        &repo,
        &backend,
        &reviewer,
        &e2e_options(stub_e2e(&ran)),
    );
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let run = &detail.runs[0];
    let prompt = read_prompt(run);
    assert!(
        prompt.contains("E2E: do not run the e2e (tests/e2e.rs) yourself."),
        "{prompt}"
    );
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(
        validated[0]["e2e_requirement"],
        json!({"required": true, "source": "paths", "paths": ["change.txt"]})
    );
    assert!(!event_kinds(&detail).contains(&"evidence_missing"));
    let kinds = event_kinds(&detail);
    for (earlier, later) in [
        ("review_finished", "run_e2e_started"),
        ("run_e2e_started", "run_e2e_finished"),
        ("run_e2e_finished", "integration_started"),
    ] {
        assert!(
            position(&kinds, earlier) < position(&kinds, later),
            "{earlier} before {later}: {kinds:?}"
        );
    }
    // The reviewed commit, before the landing's rebase.
    let commit = payloads(&detail, "validation_finished")[0]["result_commit"]
        .as_str()
        .unwrap()
        .to_owned();
    let worktree = run.worktree_path().unwrap().to_owned();
    let log = Path::new(run.run_dir().unwrap()).join("e2e-1.log");
    let started = payloads(&detail, "run_e2e_started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["attempt"], 1);
    assert_eq!(started[0]["commit"], commit);
    assert_eq!(started[0]["log"], json!(log));
    let finished = payloads(&detail, "run_e2e_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["outcome"], "passed");
    assert_eq!(finished[0]["commit"], commit);
    assert_eq!(finished[0]["flaky"], json!([]));
    // It ran once, in the run's worktree (removed since it landed) at the
    // reviewed commit.
    let ran = lines(&ran);
    let name = Path::new(&worktree).file_name().unwrap().to_string_lossy();
    assert_eq!(ran.len(), 1, "{ran:?}");
    assert!(
        ran[0].ends_with(&format!("/{name} head={commit} rerun=")),
        "{ran:?}"
    );
    let output = fs::read_to_string(&log).unwrap();
    assert!(output.contains("test result: ok. 1 passed"), "{output}");
}

/// A run that needs no e2e (no `[e2e] paths` touched, no `--evidence e2e`)
/// lands without one; a repository with no e2e the runtime knows lands a
/// run that needs one, recording that none ran.
#[test]
fn a_run_that_needs_no_e2e_or_has_none_lands_without_running_one() {
    let (dir, repo, db) = fixture();
    with_e2e_paths(&repo, "[\"src/**\"]");
    let ran = dir.path().join("ran");
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "ok")]);
    let outcome = supervise_reviewed_with(
        &db,
        &repo,
        &backend,
        &reviewer,
        &e2e_options(stub_e2e(&ran)),
    );
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert!(!event_kinds(&detail).contains(&"run_e2e_started"));
    assert!(!ran.exists());

    // The fixture's repository is not dagq's source: without a command
    // there is no e2e to run.
    let (_dir, repo, db) = fixture();
    with_e2e_paths(&repo, "[\"change.txt\"]");
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let finished = payloads(&detail, "run_e2e_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["outcome"], "not_configured");
    assert!(!event_kinds(&detail).contains(&"run_e2e_started"));
}

/// An e2e whose failed test fails its rerun by name too parks the run for
/// a resume that names the test and the logs; a mark the worker put in its
/// worktree does not pass it. The resumed session's commit is validated,
/// reviewed and its e2e run again, and the run lands once it passes.
#[test]
fn a_failed_e2e_parks_the_run_for_a_resume_and_its_fix_lands() {
    let (dir, repo, db) = fixture();
    with_e2e_paths(&repo, "[\"change.txt\"]");
    let ran = dir.path().join("ran");
    let worker_mark = "mkdir -p .config; printf '[[test]]\\nname = \"a_test\"\\nreason = \"flaky\"\\ntask = 9\\nuntil = 2999-12-31\\n' > .config/e2e-quarantine.toml; git add .config; ";
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!("{worker_mark}commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit"),
    );
    backend.resume_script_for(
        1,
        "await_message; printf 'fixed\\n' > fixed.txt; unlocked git add fixed.txt; unlocked git commit -q -m fix; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "ok")]);
    let outcome = supervise_reviewed_with(
        &db,
        &repo,
        &backend,
        &reviewer,
        &e2e_options(stub_e2e(&ran)),
    );
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(
        detail.task.status(),
        TaskStatus::Completed,
        "{:?}",
        event_kinds(&detail)
    );
    let run = &detail.runs[0];
    let failed = payloads(&detail, "run_e2e_failed");
    assert_eq!(failed.len(), 1, "{:?}", event_kinds(&detail));
    let failed = failed[0];
    assert_eq!(failed["code"], "e2e_failed");
    assert_eq!(failed["status"], "needs_session");
    assert_eq!(failed["attempt"], 1);
    assert_eq!(failed["failed_tests"], json!(["a_test"]));
    assert_eq!(failed["rerun"]["failed"], json!(["a_test"]));
    assert_eq!(failed["quarantined"], json!([]));
    // The worker's own mark is not read: main has none.
    assert!(failed.get("quarantine").is_none(), "{failed}");
    let reason = failed["reason"].as_str().unwrap();
    let run_dir = Path::new(run.run_dir().unwrap());
    for part in [
        "the e2e failed: a_test",
        "the rerun by name failed too: a_test",
        &run_dir.join("e2e-1.log").display().to_string(),
        &run_dir.join("e2e-1.rerun.log").display().to_string(),
    ] {
        assert!(reason.contains(part), "{part}: {reason}");
    }
    // The resume asked to fix the failed tests.
    let text = &session_texts(run)[0];
    assert!(
        text.contains("the e2e the runtime ran on the host before landing it failed"),
        "{text}"
    );
    assert!(text.contains(&format!("Reason: {reason}")), "{text}");
    // Validated, reviewed and run again for the fixed commit.
    assert_eq!(payloads(&detail, "validation_finished").len(), 2);
    assert_eq!(payloads(&detail, "review_finished").len(), 2);
    let finished = payloads(&detail, "run_e2e_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["outcome"], "passed");
    assert_eq!(finished[0]["attempt"], 2);
    assert_eq!(
        finished[0]["commit"],
        payloads(&detail, "validation_finished")[1]["result_commit"]
    );
    assert_ne!(finished[0]["commit"], failed["commit"]);
    let ran = lines(&ran);
    assert_eq!(ran.len(), 3, "{ran:?}");
    assert!(ran[1].ends_with("rerun=a_test"), "{ran:?}");
}

/// A test that fails its rerun under a mark of `.config/e2e-quarantine.toml`
/// committed on main passes the e2e (ADR-t1233-2 decision 5): the run
/// lands, and the event names it quarantined.
#[test]
fn a_mark_committed_on_main_passes_a_test_that_fails_its_rerun() {
    let (dir, repo, db) = fixture();
    fs::create_dir_all(repo.join(".config")).unwrap();
    fs::write(
        repo.join(".config/e2e-quarantine.toml"),
        "[[test]]\nname = \"a_test\"\nreason = \"flaky on a busy host\"\ntask = 41\nuntil = 2999-12-31\n",
    )
    .unwrap();
    git(&repo, &["add", ".config"]);
    with_e2e_paths(&repo, "[\"change.txt\"]");
    let ran = dir.path().join("ran");
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "ok")]);
    let outcome = supervise_reviewed_with(
        &db,
        &repo,
        &backend,
        &reviewer,
        &e2e_options(stub_e2e(&ran)),
    );
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert!(!event_kinds(&detail).contains(&"run_e2e_failed"));
    let finished = payloads(&detail, "run_e2e_finished");
    assert_eq!(finished[0]["outcome"], "passed");
    assert_eq!(finished[0]["quarantined"], json!(["a_test"]));
    assert_eq!(finished[0]["quarantine"]["marks"][0]["task"], 41);
}

/// The host runs one e2e at a time (ADR-t1233-2 decision 4): of two runs
/// that pass their review together, the second waits for the first's e2e,
/// and both land.
#[test]
fn the_runs_of_a_supervisor_run_their_e2e_one_at_a_time() {
    let (dir, repo, db) = fixture();
    with_e2e_paths(&repo, "[\"change-*.txt\"]");
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "second task", &[]);
    }
    let (busy, log) = (dir.path().join("busy"), dir.path().join("e2e-order"));
    let quote = |path: &Path| shell_join(&[path.display().to_string()]);
    // Overlapping e2e would find the other's directory.
    let command = format!(
        "if mkdir {busy}; then echo start >> {log}; sleep 1; echo end >> {log}; rmdir {busy}; else echo overlap >> {log}; fi; echo 'test result: ok. 1 passed'",
        busy = quote(&busy),
        log = quote(&log),
    );
    // Each run changes a file of its own, so both land.
    let backend = TestWorkspace::new(
        &db,
        false,
        "printf 'x\\n' > \"change-$RUN_ID.txt\"; git add \"change-$RUN_ID.txt\"; git commit -q -m work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "ok")]);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &e2e_options(command));
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    for task in [1, 2] {
        let detail = queue.show(TaskId::new(task)).unwrap();
        assert_eq!(detail.task.status(), TaskStatus::Completed, "task {task}");
        assert_eq!(
            payloads(&detail, "run_e2e_finished")[0]["outcome"],
            "passed"
        );
    }
    assert_eq!(lines(&log), ["start", "end", "start", "end"]);
}

/// An e2e past its timeout, and a rerun by name past it, tell nothing of
/// the change (ADR-t1233-2 decision 3): the run is not sent back to its
/// worker but waits in its slot and its e2e runs again; the third in a row
/// that could not run raises the inbox's attention, and the run lands once
/// its e2e passes.
#[test]
fn an_e2e_past_its_timeout_is_run_again_instead_of_sending_the_run_back() {
    let (dir, repo, db) = fixture();
    with_e2e_paths(&repo, "[\"change.txt\"]");
    let count = shell_join(&[dir.path().join("count").display().to_string()]);
    // 1: past the timeout; 2: a_test fails, and 3, its rerun, is past the
    // timeout; 4: past the timeout again; 5: passes.
    let command = format!(
        "n=$(cat {count} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {count}; \
         case $n in 1|3|4) sleep 30 ;; \
         2) echo 'test a_test ... FAILED'; echo; echo 'test result: FAILED. 0 passed; 1 failed'; exit 101 ;; \
         *) echo 'test result: ok. 1 passed' ;; esac"
    );
    let options = SuperviseOptions {
        run_e2e: runtime::RunE2eOptions {
            command: Some(command),
            timeout: Some(Duration::from_secs(1)),
            retry: Some(Duration::ZERO),
            ..Default::default()
        },
        ..supervise_options(4, true)
    };
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "ok")]);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"run_e2e_failed"), "{kinds:?}");
    assert!(!kinds.contains(&"resume_started"), "{kinds:?}");
    let finished = payloads(&detail, "run_e2e_finished");
    let outcomes: Vec<&str> = finished
        .iter()
        .map(|f| f["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(
        outcomes,
        ["unavailable", "unavailable", "unavailable", "passed"]
    );
    assert_eq!(finished[0]["timed_out"], true);
    assert!(
        finished[0]["error"]
            .as_str()
            .unwrap()
            .contains("did not finish within 1s"),
        "{}",
        finished[0]
    );
    assert_eq!(finished[1]["timed_out"], false);
    assert_eq!(finished[1]["failed_tests"], json!(["a_test"]));
    assert_eq!(finished[1]["rerun"]["timed_out"], true);
    assert!(
        finished[1]["error"]
            .as_str()
            .unwrap()
            .contains("the rerun by name of a_test did not finish within 1s"),
        "{}",
        finished[1]
    );
    let in_a_row: Vec<&Value> = finished[..3].iter().map(|f| &f["in_a_row"]).collect();
    assert_eq!(in_a_row, [&json!(1), &json!(2), &json!(3)]);
    assert!(finished[1].get("attention").is_none(), "{}", finished[1]);
    assert_eq!(finished[2]["attention"], true);
    assert_eq!(finished[3]["attempt"], 4);
}
