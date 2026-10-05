//! Runtime tests: The `[run.env]` of `dagq.toml` and the programs it needs.
use crate::runtime_support;

use runtime_support::headless::resume_launches;
use runtime_support::*;

#[test]
fn dagq_toml_run_env_reaches_the_workspace_and_the_verification_commands() {
    let (_dir, repo, db) = fixture();
    fs::write(
        repo.join("dagq.toml"),
        "[run.env]\nSHARED = '${DAGQ_QUEUE_DIR}/target'\nRUN_TMP = \"${DAGQ_RUN_DIR}\"\n",
    )
    .unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-m", "run env"]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let task = queue
        .add(NewTask {
            title: "env task".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: vec![
                r#"printf '%s %s\n' "$SHARED" "$RUN_TMP" >> "$RUN_TMP/verify-env.txt""#.into(),
            ],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            dependencies: vec![],
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            wait_for_build: false,
        })
        .unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    drop(queue);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = SqliteQueue::open(&db)
        .unwrap()
        .show(task.id())
        .unwrap()
        .runs[0]
        .clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let canonical = db.canonicalize().unwrap();
    let queue_dir = canonical.parent().unwrap().to_str().unwrap().to_owned();
    let run_dir = run.run_dir().unwrap().to_owned();
    // The background wrapper gets the expanded table after the runtime's
    // own names.
    assert_eq!(
        backend.launched.lock().unwrap()[0].env,
        vec![
            // No queue path: the worker's dagq goes to the queue service
            // (goal 82's stage (3)).
            ("DAGQ_ROLE".to_owned(), "worker".to_owned()),
            ("DAGQ_ACTOR_ID".to_owned(), format!("worker:{}", run.id())),
            ("DAGQ_RUN_ID".to_owned(), run.id().to_string()),
            ("DAGQ_TASK_ID".to_owned(), task.id().to_string()),
            ("SHARED".to_owned(), format!("{queue_dir}/target")),
            ("RUN_TMP".to_owned(), run_dir.clone()),
        ]
    );
    // Validation runs no verification command (ADR-0023 decision 1).
    let seen = Path::new(&run_dir).join("verify-env.txt");
    assert!(!seen.exists());

    // `integrate` runs the command once after its rebase, with the env,
    // even when called from the run's own worktree.
    fs::write(repo.join("other.txt"), "main moved\n").unwrap();
    git(&repo, &["add", "other.txt"]);
    git(&repo, &["commit", "-m", "main moved"]);
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    let outcome = integrate(&db, task.id().as_i64(), &worktree).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    let line = format!("{queue_dir}/target {run_dir}\n");
    assert_eq!(fs::read_to_string(&seen).unwrap(), line);
}

/// Task 303: the wrapper a `needs_session` resume starts gets the same
/// `[run.env]` as the worker's, after the runtime's own names.
#[test]
fn a_resumed_session_gets_the_run_env_too() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    // Written after the claims: only the resume can have read it.
    fs::write(
        repo.join("dagq.toml"),
        "[run.env]\nSHARED = '${DAGQ_QUEUE_DIR}/target'\nRUN_TMP = \"${DAGQ_RUN_DIR}\"\n",
    )
    .unwrap();
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    let canonical = db.canonicalize().unwrap();
    let queue_dir = canonical.parent().unwrap().to_str().unwrap().to_owned();
    let tags = resume_launches(&backend);
    assert_eq!(tags.len(), 1);
    assert_eq!(
        tags[0].env,
        vec![
            // No queue path: the worker's dagq goes to the queue service
            // (goal 82's stage (3)).
            ("DAGQ_ROLE".to_owned(), "worker".to_owned()),
            ("DAGQ_ACTOR_ID".to_owned(), format!("worker:{}", run.id())),
            ("DAGQ_RUN_ID".to_owned(), run.id().to_string()),
            ("DAGQ_TASK_ID".to_owned(), run.task_id().to_string()),
            ("SHARED".to_owned(), format!("{queue_dir}/target")),
            ("RUN_TMP".to_owned(), run.run_dir().unwrap().to_owned()),
        ]
    );
}

/// Task 303: a resume would give its session the `[run.env]` naming a
/// program that is missing, so it waits with the claims until it is found.
#[test]
fn a_missing_run_env_program_holds_resumes_until_it_is_found() {
    let (fixture, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (_run, first_landed) = parked_conflict(&repo, &db, &backend);
    let tool = fixture.dir.path().join("bin").join("sccache");
    fs::write(
        repo.join("dagq.toml"),
        format!("[run.env]\nRUSTC_WRAPPER = '{}'\n", tool.display()),
    )
    .unwrap();
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(resume_launches(&backend).is_empty());
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    assert!(payloads(&detail, "resume_started").is_empty());

    fs::create_dir_all(tool.parent().unwrap()).unwrap();
    crate::common::template::script(&tool, "#!/bin/sh\nexec \"$@\"\n");

    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    let tags = resume_launches(&backend);
    assert_eq!(tags.len(), 1);
    assert!(
        tags[0]
            .env
            .contains(&("RUSTC_WRAPPER".to_owned(), tool.display().to_string())),
        "{:?}",
        tags[0].env
    );
}

#[test]
fn a_broken_dagq_toml_stops_provisioning_before_the_workspace() {
    let (_dir, repo, db) = fixture();
    fs::write(repo.join("dagq.toml"), "[build]\n").unwrap();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    // Like any provisioning failure it stops claiming; no wrapper starts.
    let error = format!("{:#}", supervise(&db, &repo, &backend).unwrap_err());
    assert!(error.contains("unknown table [build]"), "{error}");
    assert!(backend.launched.lock().unwrap().is_empty());
}

#[test]
fn a_missing_run_env_program_stops_claims_and_landings_until_it_is_found() {
    let (fixture, repo, db) = fixture();
    let tool = fixture.dir.path().join("bin").join("sccache");
    fs::write(
        repo.join("dagq.toml"),
        format!(
            "[run.env]\nRUSTC_WRAPPER = '{}'\nSCCACHE_IGNORE_SERVER_IO_ERROR = '1'\n",
            tool.display()
        ),
    )
    .unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-m", "run env"]);
    let kinds = |db: &Path| -> Vec<String> {
        Connection::open(db)
            .unwrap()
            .prepare("SELECT kind FROM run_events WHERE kind LIKE 'run_env_program_%' ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };

    // Missing: nothing is claimed, the change is recorded once, and the
    // inbox is told to install the tool.
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    for _ in 0..2 {
        let outcome = supervise(&db, &repo, &backend).unwrap();
        assert_eq!(outcome["runs"], json!([]), "{outcome}");
    }
    assert!(backend.launched.lock().unwrap().is_empty());
    assert_eq!(kinds(&db), ["run_env_program_missing"]);
    let status = runtime::status(&db).unwrap();
    let install = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "run_env_program_missing")
        .unwrap_or_else(|| panic!("{status}"))
        .clone();
    assert_eq!(install["next"], "install tool");
    assert!(
        install["last_error"]
            .as_str()
            .unwrap()
            .contains("RUSTC_WRAPPER"),
        "{install}"
    );
    let doctor = runtime::doctor(&db, false).unwrap();
    assert_eq!(doctor["run_env"]["missing"], 1, "{doctor}");
    assert_eq!(
        doctor["run_env"]["programs"][0]["variable"],
        "RUSTC_WRAPPER"
    );
    assert_eq!(
        doctor["run_env"]["supervisor_last"]["kind"],
        "run_env_program_missing"
    );
    let full = runtime::doctor(&db, true).unwrap();
    assert_eq!(full["run_env"]["programs"][0]["resolved"], Value::Null);

    // Found: the change is recorded, the attention ends and the task runs.
    fs::create_dir_all(tool.parent().unwrap()).unwrap();
    crate::common::template::script(&tool, "#!/bin/sh\nexec \"$@\"\n");

    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(
        kinds(&db),
        ["run_env_program_missing", "run_env_program_found"]
    );
    let status = runtime::status(&db).unwrap();
    assert!(
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["kind"] != "run_env_program_missing"),
        "{status}"
    );
    assert_eq!(
        runtime::doctor(&db, false).unwrap()["run_env"]["programs"][0]["resolved"],
        tool.to_str().unwrap()
    );
    let run = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);

    // `integrate` checks again before the verification commands: without
    // the tool it runs none and the run goes back where it was.
    fs::remove_file(&tool).unwrap();
    let error = format!("{:#}", integrate(&db, 1, &repo).unwrap_err());
    assert!(
        error.contains("RUSTC_WRAPPER") && error.contains("returned to awaiting_integration"),
        "{error}"
    );
    let run_dir = PathBuf::from(run.run_dir().unwrap());
    assert!(!run_dir.join("integrate-1-verify-1.log").exists());
    let queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::AwaitingIntegration
    );
}

#[test]
fn a_repository_without_dagq_toml_checks_no_program() {
    // An unbound queue has no repository to read, and a bound one without
    // dagq.toml has nothing to check: doctor adds nothing either way.
    let (_fixture, repo, db) = fixture();
    let doctor = runtime::doctor(&db, true).unwrap();
    assert_eq!(doctor.get("run_env"), None, "{doctor}");
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    for full in [false, true] {
        let doctor = runtime::doctor(&db, full).unwrap();
        assert_eq!(doctor.get("run_env"), None, "{doctor}");
    }
}

#[test]
fn supervisor_starts_and_run_env_changes_are_recorded_as_marks_once() {
    use dagq::domain::marks::{RUN_ENV_CHANGED, SUPERVISOR_STARTED, SUPERVISOR_STOPPED, marks};
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let events = |kind: &str| -> Vec<Value> {
        SqliteQueue::open(&db)
            .unwrap()
            .all_events()
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == kind)
            .inspect(|event| assert!(event.task_id.is_none() && event.run_id.is_none()))
            .map(|event| event.payload)
            .collect()
    };

    // A queue without `[run.env]` gets the start and stop marks only. The
    // start carries the mode `up` passed (`supervise --mode`).
    supervise_with(
        &db,
        &repo,
        &backend,
        &SuperviseOptions {
            mode: Some(dagq::domain::SupervisorMode::InCmux),
            ..supervise_options(4, true)
        },
    )
    .unwrap();
    let started = events(SUPERVISOR_STARTED);
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["mode"], json!("in_cmux"));
    assert_eq!(started[0]["dagq_version"], json!(dagq::VERSION));
    assert_eq!(started[0]["parallel"], json!(4));
    assert_eq!(started[0]["handoff"], json!(false));
    assert_eq!(started[0]["auto_update"], json!(false));
    let stopped = events(SUPERVISOR_STOPPED);
    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0]["outcome"], json!("stopped"));
    assert_eq!(stopped[0]["supervisor"], started[0]["supervisor"]);
    assert!(events(RUN_ENV_CHANGED).is_empty());

    // Adding `[run.env]` records one mark naming the keys, not the values.
    fs::write(
        repo.join("dagq.toml"),
        "[run.env]\nCARGO_BUILD_JOBS = '4'\nSCCACHE_DIR = 'secret-dir'\n\n[stall]\nsend_confirm_secs = 5\n",
    )
    .unwrap();
    supervise(&db, &repo, &backend).unwrap();
    let changes = events(RUN_ENV_CHANGED);
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(
        changes[0]["changed"],
        json!(["CARGO_BUILD_JOBS", "SCCACHE_DIR"])
    );
    assert_eq!(changes[0]["previous_hash"], Value::Null);
    assert!(!changes[0].to_string().contains("secret-dir"));
    // The hashes are keyed with the queue's salt (outside the events), so
    // guessing a value and hashing it finds nothing.
    let salt = fs::read_to_string(db.parent().unwrap().join("run-env-salt")).unwrap();
    let plain = |text: &str| {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(text.as_bytes()))[..16].to_owned()
    };
    let payload = changes[0].to_string();
    for guess in [
        "CARGO_BUILD_JOBS=4",
        "SCCACHE_DIR=secret-dir",
        "CARGO_BUILD_JOBS=4\nSCCACHE_DIR=secret-dir\n",
    ] {
        assert!(!payload.contains(&plain(guess)), "{payload}");
    }
    assert!(!payload.contains(salt.trim()));

    // The same table, or another table changed, records nothing more, however
    // often the supervisor starts again with the same build and parallel.
    fs::write(
        repo.join("dagq.toml"),
        "[stall]\nsend_confirm_secs = 9\n\n[run.env]\nSCCACHE_DIR = 'secret-dir'\nCARGO_BUILD_JOBS = '4'\n",
    )
    .unwrap();
    supervise(&db, &repo, &backend).unwrap();
    supervise(&db, &repo, &backend).unwrap();
    assert_eq!(events(RUN_ENV_CHANGED).len(), 1);
    assert_eq!(events(SUPERVISOR_STARTED).len(), 4);

    // A changed value is one more mark, naming that key.
    fs::write(
        repo.join("dagq.toml"),
        "[run.env]\nSCCACHE_DIR = 'secret-dir'\nCARGO_BUILD_JOBS = '2'\n",
    )
    .unwrap();
    supervise(&db, &repo, &backend).unwrap();
    let changes = events(RUN_ENV_CHANGED);
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[1]["changed"], json!(["CARGO_BUILD_JOBS"]));
    assert_eq!(changes[1]["previous_hash"], changes[0]["hash"]);

    // A missing file (a checkout rewriting it) records nothing; an empty
    // table is the change that removes the keys.
    fs::remove_file(repo.join("dagq.toml")).unwrap();
    supervise(&db, &repo, &backend).unwrap();
    assert_eq!(events(RUN_ENV_CHANGED).len(), 2);
    fs::write(repo.join("dagq.toml"), "[run.env]\n").unwrap();
    supervise(&db, &repo, &backend).unwrap();
    let changes = events(RUN_ENV_CHANGED);
    assert_eq!(changes.len(), 3);
    assert_eq!(
        changes[2]["changed"],
        json!(["CARGO_BUILD_JOBS", "SCCACHE_DIR"])
    );

    // They are listed as marks; restarts with the same build derive none.
    let listed = marks(
        &SqliteQueue::open(&db).unwrap().all_events().unwrap(),
        None,
        None,
    );
    let kinds: Vec<&str> = listed.iter().map(|mark| mark.kind.as_str()).collect();
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == RUN_ENV_CHANGED)
            .count(),
        3
    );
    assert!(
        kinds.iter().all(|kind| !kind.starts_with("derived:")),
        "{kinds:?}"
    );
}
