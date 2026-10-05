//! Runtime tests: The session of a run: its receipt, the validation of the
//! receipt and the exit request.
use crate::common;
use crate::runtime_support;
use dagq::infrastructure::git_binary::git_executable;

use runtime_support::*;

/// A stub agent and the children it started die with the test's fixture
/// when the test ends while the stub still runs, here by a panic, and the
/// stub holds none of the test process's streams (task 317). To check a
/// whole run by hand: after `cargo test --locked`,
/// `ps -axo pid,ppid,command | grep -e 'test -f seed.txt' -e await_exit`
/// lists no stub.
#[test]
fn a_stub_agent_dies_with_its_fixture_and_holds_no_stream_of_the_test() {
    let out = tempfile::tempdir().unwrap();
    let (pids, streams) = (out.path().join("pids"), out.path().join("streams"));
    let (stub_pid, db) = {
        let (pids, streams) = (pids.clone(), streams.clone());
        let stub = Arc::new(Mutex::new(None));
        let started = stub.clone();
        let panicked = std::panic::catch_unwind(move || {
            let (_dir, _repo, db) = fixture();
            let mut spec = CommandSpec::new("/bin/sh");
            spec.env("PIDS", &pids)
                .env("STREAMS", &streams)
                .arg("-c")
                .arg(concat!(
                    watchdog!(),
                    r#"
for fd in 0 1 2; do [ /dev/fd/$fd -ef /dev/null ] && printf '%s ' null >> "$STREAMS.tmp"; done
mv "$STREAMS.tmp" "$STREAMS"
sleep 300 &
printf '%s %s\n' $$ $! > "$PIDS.tmp"
mv "$PIDS.tmp" "$PIDS"
while :; do sleep 0.05; done
"#
                ));
            let child = StubSpawner { db: db.clone() }
                .spawn(&spec, Streams::Inherit)
                .unwrap();
            *started.lock().unwrap() = Some((child.id(), db));
            let begun = Instant::now();
            while !pids.exists() {
                assert!(begun.elapsed() < Duration::from_secs(30));
                thread::sleep(Duration::from_millis(20));
            }
            panic!("the test fails while its stub runs");
        });
        assert!(panicked.is_err());
        stub.lock().unwrap().take().unwrap()
    };
    assert_eq!(fs::read_to_string(&streams).unwrap(), "null null null ");
    let text = fs::read_to_string(&pids).unwrap();
    let (shell, sleep) = text.trim().split_once(' ').unwrap();
    assert_eq!(shell, stub_pid.to_string());
    let sleep: u32 = sleep.parse().unwrap();
    // The shell is this process's child: reaped here once killed.
    let begun = Instant::now();
    loop {
        // SAFETY: waitpid(2) with a null status pointer writes nothing.
        let reaped =
            unsafe { libc::waitpid(stub_pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) };
        if reaped == stub_pid as libc::pid_t {
            break;
        }
        assert!(
            begun.elapsed() < Duration::from_secs(10),
            "stub {stub_pid} lives on"
        );
        thread::sleep(Duration::from_millis(20));
    }
    while running(sleep) {
        assert!(
            begun.elapsed() < Duration::from_secs(10),
            "sleep {sleep} lives on"
        );
        thread::sleep(Duration::from_millis(20));
    }
    // The fixture's queue starts no more stubs.
    let error = StubSpawner { db }
        .spawn(
            CommandSpec::new("/bin/sh").arg("-c").arg("exit 0"),
            Streams::Inherit,
        )
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "the test's fixture is gone");
}

/// While the agent works, a later binary applies a compatible migration
/// (ADR-0045 decision 6): the run's wrapper, the supervisor and the CLI the
/// worker runs are then older than the queue, and carry the run to its
/// receipt anyway.
#[test]
fn a_compatible_migration_during_a_run_leaves_the_run_working() {
    let newer = SqliteQueue::SCHEMA_VERSION + 1;
    let script = format!(
        "sqlite3 -cmd '.timeout 5000' \"$DB\" \\
           'ALTER TABLE task_runs ADD COLUMN future_hint TEXT;
            CREATE TABLE future_things (id INTEGER PRIMARY KEY);
            PRAGMA user_version = {newer};' || exit 97
         \"$DAGQ\" show 1 > /dev/null || exit 98
         {VALID_AGENT}"
    );
    let (_dir, db, detail) = run_agent(&script);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(run.last_error().is_none());
    assert!(
        detail
            .processes
            .iter()
            .all(|p| p.exited_at.is_some() && p.exit_code == Some(0))
    );
    assert_eq!(
        SqliteQueue::open(&db).unwrap().schema_version().unwrap(),
        newer
    );
}

#[test]
fn valid_receipt_is_verified_and_awaits_integration() {
    let (_dir, db, detail) = run_agent(VALID_AGENT);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(run.last_error().is_none());
    let commit = run.result_commit().unwrap();
    assert_ne!(commit, run.base_commit());
    let head = Command::new(git_executable().expect("git executable"))
        .arg("-C")
        .arg(run.worktree_path().unwrap())
        .args(["rev-parse", "HEAD"])
        .bounded_output()
        .unwrap();
    assert_eq!(String::from_utf8(head.stdout).unwrap().trim(), commit);
    // The session's log is in the run directory.
    assert!(
        run.log_path().unwrap().starts_with(run.run_dir().unwrap()),
        "{:?}",
        run.log_path()
    );
    assert!(Path::new(run.run_dir().unwrap()).join("runner").exists());
    assert_eq!(detail.processes.len(), 2);
    assert!(
        detail
            .processes
            .iter()
            .all(|p| p.exited_at.is_some() && p.exit_code == Some(0))
    );
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert!(kinds.contains(&"agent_started"));
    assert!(kinds.contains(&"receipt_observed"));
    // The only task has no goal, no context, no predecessor, and nothing else was in progress.
    let prompt = read_prompt(run);
    assert!(
        prompt.contains("Goal: none, this task stands alone\n"),
        "{prompt}"
    );
    assert!(prompt.contains("Context: none\n"), "{prompt}");
    assert!(prompt.contains("Predecessor tasks: none\n"), "{prompt}");
    assert!(
        prompt.contains("Sibling tasks in progress: none\n"),
        "{prompt}"
    );
    // Validation checks only the receipt: the verification commands wait
    // for integrate's rebase (ADR-0023 decision 1).
    assert!(!kinds.contains(&"verification_command"), "{kinds:?}");
    assert!(
        !Path::new(run.run_dir().unwrap())
            .join("verify-1.log")
            .exists()
    );
    let finished = detail
        .events
        .iter()
        .find(|e| e.kind == "validation_finished")
        .unwrap();
    assert_eq!(finished.payload["status"], "awaiting_integration");
    assert_eq!(finished.payload["result_commit"], json!(commit));
    assert_eq!(
        finished.payload["receipt"]["e2e"]["status"],
        "not_applicable"
    );
    // The workspace is closed only after validation succeeded; the branch stays.
    let closed = detail
        .events
        .iter()
        .find(|e| e.kind == "workspace_closed")
        .unwrap();
    assert!(closed.id > finished.id);
    assert_eq!(closed.payload["workspace_id"], background_session(run));
    assert_eq!(
        closed.payload["closed_at"],
        json!(run.workspace_closed_at().unwrap())
    );
    let branch = Command::new(git_executable().expect("git executable"))
        .arg("-C")
        .arg(run.worktree_path().unwrap())
        .args(["symbolic-ref", "HEAD"])
        .bounded_output()
        .unwrap();
    assert_eq!(
        String::from_utf8(branch.stdout).unwrap().trim(),
        format!("refs/heads/{}", run.branch().unwrap())
    );
    // The task still owns its slot until integration; no second run starts.
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(queue.show(TaskId::new(1)).unwrap().runs.len(), 1);
    assert!(queue.candidates().unwrap().is_empty());
}

#[test]
fn failed_workspace_close_is_recorded_without_changing_run_status() {
    let (_dir, _db, detail) = run_agent_with(VALID_AGENT, true);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(run.result_commit().is_some());
    let error = run.last_error().unwrap();
    assert!(error.contains("injected session stop failure"), "{error}");
    let session = background_session(run);
    assert!(error.contains(&session));
    let failed = detail
        .events
        .iter()
        .find(|e| e.kind == "cleanup_failed")
        .unwrap();
    assert_eq!(failed.payload["workspace_id"], session);
    assert_eq!(failed.payload["message"], json!(error));
    // Validation itself was accepted; the failure is confined to cleanup.
    let finished = detail
        .events
        .iter()
        .find(|e| e.kind == "validation_finished")
        .unwrap();
    assert_eq!(finished.payload["accepted"], true);
    assert!(failed.id > finished.id);
}

#[test]
fn run_id_mismatch_fails_validation() {
    let (_dir, _db, detail) = run_agent("commit work; receipt \"$(git rev-parse HEAD)\" other-run");
    assert!(rejection_reason(&detail).contains("run_id other-run does not match"));
    assert!(detail.runs[0].result_commit().is_none());
}

#[test]
fn receipt_without_new_commit_fails_validation() {
    let (_dir, _db, detail) = run_agent("receipt \"$BASE\"");
    assert!(rejection_reason(&detail).contains("no commit was made"));
}

#[test]
fn receipt_commit_that_is_not_branch_head_fails_validation() {
    let (_dir, _db, detail) = run_agent("commit work; receipt \"$BASE\"");
    assert!(rejection_reason(&detail).contains("is not the head of dagq/"));
}

#[test]
fn dirty_worktree_fails_validation() {
    let (_dir, _db, detail) = run_agent(
        "commit work; printf 'scratch\n' > untracked.txt; receipt \"$(git rev-parse HEAD)\"",
    );
    let reason = rejection_reason(&detail);
    assert!(reason.contains("worktree is not clean"));
    assert!(reason.contains("untracked.txt"));
    // The verified commit is still recorded for inspection.
    assert!(detail.runs[0].result_commit().is_some());
}

#[test]
fn receipt_structure_is_checked_before_git() {
    use dagq::domain::Receipt;
    let valid = r#"{"run_id":"r","result":"succeeded","commit":"0123456789abcdef0123456789abcdef01234567",
        "tests":{"status":"passed","evidence_or_reason":"cargo test"},
        "e2e":{"status":"not_applicable","evidence_or_reason":"library only"},
        "subagent_review":{"status":"passed","evidence_or_reason":"no findings"},"summary":"ok"}"#;
    Receipt::parse(valid)
        .unwrap()
        .check(&RunId::new("r").unwrap())
        .unwrap();
    let cases = [
        (valid.replace("\"r\"", "\"other\""), "does not match"),
        (
            valid.replace("succeeded", "failed"),
            "agent reported result failed",
        ),
        (
            valid.replace("library only", " "),
            "e2e is not_applicable without evidence or reason",
        ),
        (
            valid.replace(
                "\"passed\",\"evidence_or_reason\":\"no findings\"",
                "\"failed\",\"evidence_or_reason\":\"bug\"",
            ),
            "subagent_review as failed: bug",
        ),
        (
            valid.replace("0123456789abcdef0123456789abcdef01234567", "0123456"),
            "receipt commit",
        ),
    ];
    for (text, expected) in cases {
        let error = format!(
            "{:#}",
            Receipt::parse(&text)
                .unwrap()
                .check(&RunId::new("r").unwrap())
                .unwrap_err()
        );
        assert!(error.contains(expected), "{error}");
    }
    assert!(Receipt::parse("{\"run_id\":\"r\"}").is_err());
    assert!(Receipt::parse(&valid.replace("passed", "maybe")).is_err());
    // follow_ups is optional and only its shape is checked.
    let without = Receipt::parse(valid).unwrap();
    assert!(without.follow_ups().is_none());
    assert!(
        !serde_json::to_string(&without)
            .unwrap()
            .contains("follow_ups")
    );
    let with = valid.replace(
        "\"summary\":\"ok\"",
        "\"summary\":\"ok\",\"follow_ups\":[{\"title\":\"next\",\"description\":\"later\"}]",
    );
    let receipt = Receipt::parse(&with).unwrap();
    receipt.check(&RunId::new("r").unwrap()).unwrap();
    assert_eq!(receipt.follow_ups().unwrap()[0]["title"], "next");
    assert_eq!(
        serde_json::to_value(&receipt).unwrap()["follow_ups"][0]["description"],
        "later"
    );
    Receipt::parse(&valid.replace("\"summary\":\"ok\"", "\"summary\":\"ok\",\"follow_ups\":[]"))
        .unwrap()
        .check(&RunId::new("r").unwrap())
        .unwrap();
    let error = format!(
        "{:#}",
        Receipt::parse(&valid.replace(
            "\"summary\":\"ok\"",
            "\"summary\":\"ok\",\"follow_ups\":{\"title\":\"next\"}"
        ))
        .unwrap()
        .check(&RunId::new("r").unwrap())
        .unwrap_err()
    );
    assert!(error.contains("follow_ups must be an array"), "{error}");
}

/// Commits once, waits for `$EXIT.go` before a second commit and the receipt.
const TWO_COMMIT_AGENT: &str = "commit first; await_file \"$EXIT.go\"; printf 'more\\n' >> change.txt; git commit -q -am second; receipt \"$(git rev-parse HEAD)\"";

/// The supervisor records `first_commit_observed` once, while the session
/// still works, when the worktree's HEAD first leaves the base commit; a
/// later commit records nothing more. It sits between `agent_started` and
/// `receipt_observed`, which is what `stats` reads as `startup`.
#[test]
fn the_first_commit_is_observed_once_while_the_session_works() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, TWO_COMMIT_AGENT));
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    let observed = |queue: &mut SqliteQueue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap())
            .iter()
            .filter(|k| **k == "first_commit_observed")
            .count()
    };
    wait_until(&db, Duration::from_secs(30), |queue| observed(queue) == 1);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let worktree = PathBuf::from(run.worktree_path().unwrap());
    let first = git_out(&worktree, &["rev-parse", "HEAD"]);
    assert_ne!(first, *run.base_commit());
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(!event_kinds(&detail).contains(&"receipt_observed"));

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");

    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert_eq!(observed(&mut queue), 1, "{kinds:?}");
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("agent_started") < position("first_commit_observed"));
    assert!(position("first_commit_observed") < position("receipt_observed"));
    let payload = events_of(&db, run.id(), "first_commit_observed").remove(0);
    assert_eq!(payload["commit"], first.as_str());
    assert_eq!(payload["base_commit"], run.base_commit().as_str());
    let head = queue.show(TaskId::new(1)).unwrap().runs[0]
        .result_commit()
        .cloned()
        .unwrap();
    assert_ne!(head, first, "the second commit is the result");
}

#[test]
fn claude_stop_hook_settings_publish_the_idle_marker() {
    use dagq::application::execution::permission_deny;
    use dagq::domain::ActorRole;
    use dagq::infrastructure::adapters::{
        ClaudeCode, runtime_session_settings, stop_hook_settings,
    };
    let deny = permission_deny(ActorRole::Worker);
    let dir = tempfile::tempdir().unwrap();
    let run_dir = dir.path().join("run's dir");
    fs::create_dir(&run_dir).unwrap();
    let run = TaskRun::restore(dagq::domain::RunRecord {
        id: RunId::new("11111111-2222-4333-8444-555555555555").unwrap(),
        task_id: TaskId::new(1),
        status: RunStatus::Starting,
        requested_provider: dagq::domain::Provider::Claude,
        actual_provider: dagq::domain::Provider::Claude,
        worker_mode: dagq::domain::worker::WorkerMode::Interactive,
        base_commit: sha("0123456789abcdef0123456789abcdef01234567"),
        branch: Some("dagq/x".into()),
        worktree_path: Some(dir.path().to_str().unwrap().into()),
        workspace_id: None,
        receipt_path: Some(run_dir.join("receipt.json").to_str().unwrap().into()),
        log_path: Some(run_dir.join("claude.debug.log").to_str().unwrap().into()),
        result_commit: None,
        repo_path: None,
        run_dir: Some(run_dir.to_str().unwrap().into()),
        last_error: None,
        workspace_closed_at: None,
        created_at: String::new(),
    })
    .unwrap();
    let command = ClaudeCode {
        executable: "claude".into(),
    }
    .command(&run, "prompt")
    .unwrap();
    let args: Vec<String> = command
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let settings = run_dir.join("claude-settings.json");
    assert!(args.contains(&"--settings".to_string()));
    assert!(args.contains(&settings.to_str().unwrap().to_string()));
    let text = fs::read_to_string(&settings).unwrap();
    assert_eq!(
        text,
        runtime_session_settings(&run.idle_marker_path().unwrap(), &deny).unwrap()
    );
    let parsed: Value = serde_json::from_str(&text).unwrap();
    // Nobody types in a worker's session: Claude Code's prompt suggestions,
    // grey text in the input box that reads like a half-typed message, are
    // off (goal 48). Otherwise the settings are the Stop hook's.
    assert_eq!(parsed["promptSuggestionEnabled"], json!(false));
    let mut hooks_only = parsed.clone();
    hooks_only
        .as_object_mut()
        .unwrap()
        .remove("promptSuggestionEnabled");
    let expected: Value =
        serde_json::from_str(&stop_hook_settings(&run.idle_marker_path().unwrap(), &deny).unwrap())
            .unwrap();
    assert_eq!(hooks_only, expected);
    // The worker's policy becomes its `permissions.deny`, after the signals
    // by name: no landing, answering, readying or rewriting its role.
    let denied = parsed["permissions"]["deny"].as_array().unwrap();
    assert_eq!(
        denied[..2],
        [json!("Bash(pkill:*)"), json!("Bash(killall:*)")]
    );
    for rule in [
        "Bash(dagq integrate:*)",
        "Bash(dagq answer:*)",
        "Bash(dagq ready:*)",
        "Bash(DAGQ_ROLE=*)",
    ] {
        assert!(denied.contains(&json!(rule)), "{rule}: {denied:?}");
    }
    assert!(!denied.contains(&json!("Bash(dagq ask:*)")));
    // The model and effort go among the options (ADR-0079 decision 3).
    let claude = ClaudeCode {
        executable: "claude".into(),
    };
    let mut chosen = claude.command(&run, "prompt").unwrap();
    claude.select_model(&mut chosen, "claude-sonnet-5", "medium");
    let chosen: Vec<String> = chosen
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        chosen[chosen.len() - 6..],
        [
            "--model",
            "claude-sonnet-5",
            "--effort",
            "medium",
            "--",
            "prompt"
        ]
    );
    // The resumed session writes the same settings.
    fs::remove_file(&settings).unwrap();
    let mut resumed = claude.resume_command(&run).unwrap();
    assert_eq!(fs::read_to_string(&settings).unwrap(), text);
    claude.select_model(&mut resumed, "claude-opus-5-5", "medium");
    let resumed: Vec<String> = resumed
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        resumed[resumed.len() - 4..],
        ["--model", "claude-opus-5-5", "--effort", "medium"]
    );
    // A non-empty auto mode environment from flag settings keeps the
    // "Teach auto mode" dialog away; `$defaults` keeps the built-in entries.
    assert_eq!(parsed["autoMode"]["environment"], json!(["$defaults"]));
    // The session never signals processes by name or pattern: other runs'
    // sessions carry their prompts, and so the checks' names, in their
    // command lines (task 359). The worker's policy follows.
    assert_eq!(
        parsed["permissions"]["deny"],
        json!(
            ["Bash(pkill:*)", "Bash(killall:*)"]
                .into_iter()
                .map(str::to_owned)
                .chain(deny.iter().cloned())
                .collect::<Vec<_>>()
        )
    );
    let hook = &parsed["hooks"]["Stop"][0]["hooks"][0];
    assert_eq!(hook["type"], "command");
    // Run the hook exactly as Claude would: shell command, event JSON on stdin.
    let payload =
        r#"{"session_id":"11111111-2222-4333-8444-555555555555","hook_event_name":"Stop"}"#;
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(hook["command"].as_str().unwrap())
        .stdin(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            let _waiting = common::within(common::STEP_LIMIT, "the Stop hook to exit");
            child.stdin.take().unwrap().write_all(payload.as_bytes())?;
            child.wait()
        })
        .unwrap();
    assert!(status.success());
    assert_eq!(
        fs::read_to_string(run.idle_marker_path().unwrap()).unwrap(),
        payload
    );
    assert!(!run_dir.join("idle.json.tmp").exists());
    // Each marker is also appended, with the time, to the log next to it.
    let log = fs::read_to_string(run_dir.join("idle.log")).unwrap();
    let (secs, marker) = log.strip_suffix('\n').unwrap().split_once('\t').unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(now.abs_diff(secs.parse().unwrap()) < 60, "{log}");
    assert_eq!(marker, payload);
    // The log keeps only the current streak (task 422): a marker listing a
    // running background task is appended, one listing none (whatever its
    // message says) ends every streak and replaces the log.
    let stop = hook_command(&parsed["hooks"]["Stop"][0]["hooks"][0]);
    let running = r#"{"last_assistant_message":"waiting","background_tasks":[{"id":"b1","status":"running"}]}"#;
    let quoted = r#"{"last_assistant_message":"it said \"status\": \"running\"","background_tasks":[{"id":"b1", "status" : "running"}]}"#;
    // Pretty-printed JSON with the pair split over lines still counts.
    let split = "{\"background_tasks\": [{\"id\": \"b1\", \"status\":\n  \"running\"}]}";
    let ended = r#"{"last_assistant_message":"{\"status\":\"running\"}","background_tasks":[{"id":"b1","status":"completed"}]}"#;
    let log_lines = || {
        fs::read_to_string(run_dir.join("idle.log"))
            .unwrap()
            .lines()
            .map(|line| line.split_once('\t').unwrap().1.to_owned())
            .collect::<Vec<_>>()
    };
    for (payload, expected) in [
        (running, vec![payload, running]),
        (quoted, vec![payload, running, quoted]),
        (
            split,
            vec![payload, running, quoted, &split.replace('\n', "")],
        ),
        (ended, vec![ended]),
        (running, vec![ended, running]),
    ] {
        run_hook(&stop, payload);
        assert_eq!(log_lines(), expected);
        assert_eq!(
            fs::read_to_string(run.idle_marker_path().unwrap()).unwrap(),
            payload
        );
    }
    assert!(!run_dir.join("idle.log.tmp").exists());
    // A log that cannot be appended to does not keep the marker back.
    fs::remove_file(run_dir.join("idle.log")).unwrap();
    fs::create_dir(run_dir.join("idle.log")).unwrap();
    run_hook(&stop, running);
    assert_eq!(
        fs::read_to_string(run.idle_marker_path().unwrap()).unwrap(),
        running
    );
    fs::remove_dir_all(run_dir.join("idle.log")).unwrap();
    // The UserPromptSubmit hook publishes each input it takes as the input
    // marker next to the idle marker (ADR-0043 decision 2), printing
    // nothing into the agent's context.
    let hook = &parsed["hooks"]["UserPromptSubmit"][0]["hooks"][0];
    assert_eq!(hook["type"], "command");
    let payload = r#"{"hook_event_name":"UserPromptSubmit","prompt":"it's \"quoted\""}"#;
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(hook["command"].as_str().unwrap())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            let _waiting = common::within(common::STEP_LIMIT, "the UserPromptSubmit hook to exit");
            child.stdin.take().unwrap().write_all(payload.as_bytes())?;
            child.wait_with_output()
        })
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        fs::read_to_string(run_dir.join("prompt-submit.json")).unwrap(),
        payload
    );
    assert!(!run_dir.join("prompt-submit.json.tmp").exists());
}

/// A `revise` verdict whose request cannot be written (a directory is in
/// the way of `revise-1.txt` in the run directory, the parent of the
/// worktree the reviewer runs in): the step fails in the review phase,
/// with the worker's headless session open between its turns. `before`
/// runs first in the reviewer's shell.
fn unwritable_revise(before: &str) -> TestReviewer {
    TestReviewer::new(&[format!(
        "mkdir ../revise-1.txt; {before}{}",
        verdict("revise", &["add a line"], "one gap")
    )])
}

/// One `supervise` that reviews with `reviewer`, without waiting for the
/// sessions it leaves behind.
fn supervise_leaving_sessions(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    reviewer: &TestReviewer,
) -> Value {
    let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
    runtime::supervise_with_reviewer(
        db,
        repo,
        backend,
        &claude_stub(db),
        reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &supervise_options(4, true),
    )
    .unwrap()
}

/// The supervisor asks the headless session of a run it gives up on, in
/// its review, to exit (task 237): the `runtime_error` records the lease
/// released and `session.exit` `sent` with the session's workspace, the
/// exit request is written before the supervisor returns, the session ends
/// on it, and the run waits for a person's review and integrate. Moved
/// from the interactive `runtime_abandon` test of the same judgment when
/// the interactive worker went (task 1437).
#[test]
fn a_run_given_up_in_its_review_asks_its_headless_session_to_exit() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = unwritable_revise("");
    let outcome = supervise_leaving_sessions(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"].as_array().unwrap().len(), 1, "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    let errors = payloads(&detail, "runtime_error");
    assert_eq!(errors.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(errors[0]["lease_released"], true);
    assert_eq!(errors[0]["session"]["exit"], "sent", "{}", errors[0]);
    assert_eq!(
        errors[0]["session"]["workspace_id"],
        json!(run.workspace_id())
    );
    // Written before the supervisor returned: nothing else would end the
    // session, which waits for its next request. A run given up records
    // its request in the runtime_error's session, not as exit_requested.
    let exit = dagq::domain::turn::exit_path(Path::new(run.run_dir().unwrap()));
    assert!(exit.exists(), "{}", exit.display());
    backend.join();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    // The session was open through the review and ended at the request.
    assert!(
        position(&kinds, "review_finished") < position(&kinds, "session_exited"),
        "{kinds:?}"
    );
    let status = runtime::status(&db).unwrap();
    let attention = run_attention_of(&status, run.id()).unwrap();
    assert_eq!(attention["next"], "review and integrate", "{status}");
}

/// An exit request that cannot be written (a directory is in the way of
/// the run's `turns/exit`) leaves the headless session to a person: the
/// `runtime_error` records `session.exit` `failed` with the error, the
/// run's attention is `exit the session` while the session waits, and
/// `review and integrate` comes back once the session exits. Moved from
/// the interactive `runtime_abandon` test of the same judgment when the
/// interactive worker went (task 1437).
#[test]
fn a_headless_session_whose_exit_request_cannot_be_written_is_a_persons_to_end() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = unwritable_revise("mkdir ../turns/exit; ");
    let outcome = supervise_leaving_sessions(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"].as_array().unwrap().len(), 1, "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let errors = payloads(&detail, "runtime_error");
    assert_eq!(errors.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(errors[0]["lease_released"], true);
    assert_eq!(errors[0]["session"]["exit"], "failed", "{}", errors[0]);
    assert!(
        !errors[0]["session"]["error"].as_str().unwrap().is_empty(),
        "{}",
        errors[0]
    );
    assert!(!event_kinds(&detail).contains(&"session_exited"));
    let status = runtime::status(&db).unwrap();
    let attention = run_attention_of(&status, run.id()).unwrap();
    assert_eq!(attention["next"], "exit the session", "{status}");
    assert_eq!(attention["kind"], "runtime_error");

    // A person ends the session: the run waits for its review again.
    let exit = dagq::domain::turn::exit_path(Path::new(run.run_dir().unwrap()));
    fs::remove_dir(&exit).unwrap();
    fs::write(&exit, "").unwrap();
    backend.join();
    let status = runtime::status(&db).unwrap();
    let attention = run_attention_of(&status, run.id()).unwrap();
    assert_eq!(attention["next"], "review and integrate", "{status}");
}

fn hook_command(hook: &Value) -> String {
    assert_eq!(hook["type"], "command");
    hook["command"].as_str().unwrap().to_owned()
}

/// Runs a hook the way Claude Code does: its command in a shell, the event
/// JSON on stdin; it must exit 0.
fn run_hook(command: &str, payload: &str) {
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            let _waiting = common::within(common::STEP_LIMIT, "the hook to exit");
            child.stdin.take().unwrap().write_all(payload.as_bytes())?;
            child.wait()
        })
        .unwrap();
    assert!(status.success());
}
