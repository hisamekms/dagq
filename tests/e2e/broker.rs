//! End-to-end paths of the resource broker (goal 58, ADR-t827-4 decision
//! 1; goal 59, ADR-t838-1): with `[broker] mode = "preferred"` or
//! `"required"` in a disposable repository, the host worker (the stub) does
//! its task through the broker in dagq's Podman machine, and the run lands;
//! with `required`, a supervisor whose broker is stopped claims nothing and
//! tells the inbox until it has made the broker ready. Needs podman as well
//! as cmux; the first run builds the broker's image in the machine.
use super::*;
use std::sync::OnceLock;

/// Long enough for dagq's machine to init and for the first build of the
/// broker's image in it.
const BROKER_START_LIMIT: Duration = Duration::from_secs(3600);

/// Stops the queue's broker (the container, then dagq's machine when no
/// other container runs on it) when the test ends, however it ends.
struct BrokerGuard<'a>(&'a Env);

impl Drop for BrokerGuard<'_> {
    fn drop(&mut self) {
        let output = dagq_output(self.0, &[], &["broker", "stop"]);
        eprintln!(
            "broker stop: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Put the broker's client of this build next to the dagq under test, as
/// `install` does (ADR-t827-1 decision 5): built from this checkout into a
/// target dir of its own, so the build never waits on the lock of the one
/// running these tests. Built once for the tests of this binary.
fn client_next_to_dagq() -> PathBuf {
    static CLIENT: OnceLock<PathBuf> = OnceLock::new();
    CLIENT.get_or_init(build_client).clone()
}

fn build_client() -> PathBuf {
    let target = tempfile::tempdir().unwrap();
    let _waiting = common::within(BROKER_START_LIMIT, "the broker's client to build");
    let status = Command::new(env!("CARGO"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "build",
            "--locked",
            "-p",
            "dagq-broker-client",
            "--target-dir",
        ])
        .arg(target.path())
        .status()
        .unwrap();
    assert!(status.success(), "cargo build -p dagq-broker-client");
    let client = Path::new(BIN).with_file_name("dagq-broker-client");
    let temporary = client.with_extension("e2e-tmp");
    fs::copy(target.path().join("debug/dagq-broker-client"), &temporary).unwrap();
    fs::rename(&temporary, &client).unwrap();
    let version = Command::new(&client).arg("--version").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("dagq-broker-client {VERSION}"),
        "the client names dagq's build"
    );
    client
}

/// `dagq broker start`: dagq's machine, the image and the queue's container
/// made ready (task 836), however long the first build of the image takes.
fn broker_start(env: &Env) {
    let started = {
        let _waiting = common::within(BROKER_START_LIMIT, "dagq broker start");
        let mut command = Command::new(BIN);
        command.without_actor_env();
        command
            .current_dir(&env.repo)
            .env("XDG_DATA_HOME", &env.data_home)
            .env_remove("CLAUDE_CONFIG_DIR")
            .args(["broker", "start"])
            .output()
            .unwrap()
    };
    let started = checked(&["broker", "start"], started);
    assert_eq!(started["state"], "running", "{started}");
}

/// Print what the stub's calls of the client answered (the run dir's
/// `broker*` files) and the supervisor's log, for a failure to show.
fn print_broker_files(run_dir: Option<&str>, stderr: &str) {
    if let Some(run_dir) = run_dir {
        for entry in fs::read_dir(run_dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if name.starts_with("broker") && path.is_file() {
                eprintln!("{name}: {}", fs::read_to_string(&path).unwrap_or_default());
            }
        }
    }
    eprintln!("{stderr}");
}

/// The `[broker]` of a `required` queue (ADR-t838-1): `sh` for the stub's
/// `exec`, and one package command (task 840) that the image's `touch`
/// runs.
fn required_dagq_toml() -> String {
    format!(
        "{E2E_DAGQ_TOML}\n[broker]\nmode = \"required\"\nexec_allow = [\"sh\"]\n\n[broker.package]\ne2e-touch = [\"touch\", \"package.txt\"]\n"
    )
}

/// The ready task the stub worker does only through the broker's tools of
/// a `required` run, and the review passes; the landing's verification
/// finds every file it wrote through the broker.
fn add_required_task(env: &Env, title: &str) -> String {
    add_ready_task_described(
        env,
        title,
        "Add e2e.txt, exec.txt and package.txt through the broker. E2E-BROKER E2E-REVIEW-PASS",
        &[],
        &["test -f exec.txt", "test -f package.txt"],
    )
}

/// What a landed `required` run left (ADR-t838-1): the files written
/// through the broker on main, the run marked, its configuration gone with
/// the token it named, which was issued with the claim and revoked with the
/// landing, the broker's audit holding each of the run's operations under
/// that token, and no built-in tool counted. The `run_id` of the run.
fn assert_required_run_landed(fixture: &Fixture, task_id: &str) -> String {
    let Fixture { repo, env, .. } = fixture;
    let detail = dagq(env, &["show", task_id, "--full"]);
    assert_eq!(detail["task"]["status"], "completed", "{detail}");
    let events = detail["events"].as_array().unwrap();
    let of = |kind: &str| -> Vec<&Value> {
        events
            .iter()
            .filter(|event| event["kind"] == kind)
            .map(|event| &event["payload"])
            .collect()
    };
    assert!(of("broker_unavailable").is_empty(), "{events:?}");
    let run = &detail["runs"][0];
    let run_id = run["id"].as_str().unwrap().to_owned();
    let run_dir = Path::new(run["run_dir"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(repo.join("e2e.txt")).unwrap(),
        format!("written through the required broker for {run_id}")
    );
    assert_eq!(
        fs::read_to_string(repo.join("exec.txt")).unwrap(),
        "run by the broker\n"
    );
    assert!(repo.join("package.txt").is_file());
    assert!(
        git(repo, &["log", "-1", "--format=%s", "main"])
            .contains(detail["task"]["title"].as_str().unwrap())
    );
    // The turn the stub checked was `required`'s (ADR-t838-1): `dontAsk`,
    // the broker's server alone, no setting sources.
    let turn = fs::read_to_string(run_dir.join("broker-required-turn.txt")).unwrap();
    assert!(
        turn.starts_with("required turn: --permission-mode dontAsk --mcp-config "),
        "{turn}"
    );
    assert!(
        turn.ends_with("--strict-mcp-config --setting-sources \"\"\n"),
        "{turn}"
    );
    assert!(run_dir.join("broker-required").exists());
    assert!(!run_dir.join("broker/mcp.json").exists());
    let issued = of("broker_token_issued");
    assert_eq!(issued.len(), 1, "{events:?}");
    let jti = issued[0]["jti"].as_str().unwrap();
    assert_eq!(
        of("broker_token_revoked"),
        [&json!({"jti": jti, "reason": "integrated"})]
    );
    let ops = [
        "fs.read",
        "fs.write",
        "process.exec",
        "package.install",
        "git.add",
        "git.commit",
    ];
    let audit = dagq(env, &["broker", "audit", "--run", &run_id]);
    let entries = audit["entries"].as_array().unwrap();
    for op in ops {
        let entry = entries
            .iter()
            .find(|entry| entry["op"] == op)
            .unwrap_or_else(|| panic!("no {op} of run {run_id} in the audit: {audit}"));
        assert_eq!(entry["result"], "ok", "{entry}");
        assert_eq!(entry["jti"], jti, "{entry}");
    }
    // Counted with the revoke (task 839): every operation was the broker's.
    let usage = of("broker_tool_use");
    assert_eq!(usage.len(), 1, "{events:?}");
    assert_eq!(usage[0]["direct"], 0, "{usage:?}");
    for op in ops {
        assert!(
            usage[0]["brokered_by_op"][op].as_u64() >= Some(1),
            "{usage:?}"
        );
    }
    let status = dagq(env, &["status"]);
    assert_eq!(status["broker"]["mode"], "required", "{status}");
    assert_eq!(status["broker"]["health"]["state"], "healthy", "{status}");
    assert_eq!(status["broker"]["active_tokens"], 0, "{status}");
    run_id
}

/// A ready task for the stub worker with the broker's tools: it writes
/// e2e.txt with `fs.write`, exec.txt with `process.exec` (`sh`, which
/// `exec_allow` lets run) and commits both with `git.add` and
/// `git.commit`; the review passes it. The verification finds both files
/// on the landed commit, so the work reached the run branch through the
/// broker.
#[test]
#[ignore = "needs a running cmux and podman; builds the broker's image; run with --ignored"]
fn a_preferred_worker_does_its_task_through_the_broker_and_lands() {
    let fixture = fixture();
    let Fixture {
        cmux, repo, env, ..
    } = &fixture;
    let client = client_next_to_dagq();
    fs::write(
        repo.join("dagq.toml"),
        format!("{E2E_DAGQ_TOML}\n[broker]\nmode = \"preferred\"\nexec_allow = [\"sh\"]\n"),
    )
    .unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-q", "-m", "broker preferred"]);
    assert_eq!(dagq(env, &["status"])["broker"]["mode"], "preferred");

    // The runtime makes dagq's machine, the image and the container ready
    // (task 836); the supervisor's claim finds it running.
    let _broker = BrokerGuard(env);
    broker_start(env);

    let task_id = add_ready_task_described(
        env,
        "e2e broker task",
        "Add e2e.txt and exec.txt through the broker. E2E-BROKER E2E-REVIEW-PASS",
        &[],
        &["test -f exec.txt"],
    );
    let mut guard = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };
    let pass = supervise_once(&fixture, &[], &[&task_id], &mut guard);
    let outcome = &pass.outcome;
    // What the stub's calls of the client answered, for a failure to show.
    if outcome["runs"][0]["run_dir"].is_string() {
        print_broker_files(outcome["runs"][0]["run_dir"].as_str(), &pass.stderr);
    }
    // A worker given no tools (the claim found the broker unusable) is the
    // first thing a failure names, before what it led to (task 1255).
    let detail = dagq(env, &["show", &task_id, "--full"]);
    let events = detail["events"].as_array().unwrap();
    let of = |kind: &str| -> Vec<&Value> {
        events
            .iter()
            .filter(|event| event["kind"] == kind)
            .map(|event| &event["payload"])
            .collect()
    };
    assert!(
        of("broker_unavailable").is_empty(),
        "the worker was given no broker tools: {:?}; {outcome}",
        of("broker_unavailable")
    );
    assert_eq!(outcome["errors"], Value::Array(vec![]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");

    assert_eq!(detail["task"]["status"], "completed");
    let run = &detail["runs"][0];
    let run_id = run["id"].as_str().unwrap();
    assert_eq!(
        fs::read_to_string(repo.join("e2e.txt")).unwrap(),
        format!("written through the broker for {run_id}")
    );
    assert_eq!(
        fs::read_to_string(repo.join("exec.txt")).unwrap(),
        "run by the broker\n"
    );
    assert!(git(repo, &["log", "-1", "--format=%s", "main"]).contains("e2e broker task"));
    // The token was issued with the claim and revoked with the landing.
    let issued = of("broker_token_issued");
    assert_eq!(issued.len(), 1, "{events:?}");
    let jti = issued[0]["jti"].as_str().unwrap();
    assert_eq!(
        of("broker_token_revoked"),
        [&json!({"jti": jti, "reason": "integrated"})]
    );

    // The broker's audit holds the run's operations, each with its result.
    let audit = dagq(env, &["broker", "audit", "--run", run_id]);
    let entries = audit["entries"].as_array().unwrap();
    for op in ["fs.write", "process.exec", "git.add", "git.commit"] {
        let entry = entries
            .iter()
            .find(|entry| entry["op"] == op)
            .unwrap_or_else(|| panic!("no {op} of run {run_id} in the audit: {audit}"));
        assert_eq!(entry["result"], "ok", "{entry}");
        assert_eq!(entry["jti"], jti, "{entry}");
    }
    let status = dagq(env, &["status"]);
    assert_eq!(status["broker"]["health"]["state"], "healthy", "{status}");
    assert_eq!(status["broker"]["active_tokens"], 0, "{status}");
    assert!(client.exists());
    wait_until_not_listed(cmux, &pass.workspaces[0].1);
}

/// `[broker] mode = "required"` (ADR-t838-1, goal 59): the stub worker's
/// turn is `required`'s (`dontAsk`, the broker's server alone, no setting
/// sources, the built-in file tools denied) and does the task only through
/// the client's MCP server: it reads seed.txt, writes e2e.txt, runs a
/// command that writes exec.txt, runs the package command that writes
/// package.txt, stages and commits them, and writes its receipt with
/// `write_receipt`. The run lands, the broker's audit holds each of those
/// operations of the run, no built-in tool is counted, and the run's token
/// is revoked with its landing.
#[test]
#[ignore = "needs a running cmux and podman; builds the broker's image; run with --ignored"]
fn a_required_worker_does_its_task_only_through_the_broker_and_lands() {
    let fixture = fixture();
    let Fixture {
        cmux, repo, env, ..
    } = &fixture;
    client_next_to_dagq();
    fs::write(repo.join("dagq.toml"), required_dagq_toml()).unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-q", "-m", "broker required"]);
    assert_eq!(dagq(env, &["status"])["broker"]["mode"], "required");

    let _broker = BrokerGuard(env);
    broker_start(env);
    let task_id = add_required_task(env, "e2e required broker task");
    let mut guard = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };
    let pass = supervise_once(&fixture, &[], &[&task_id], &mut guard);
    let outcome = &pass.outcome;
    print_broker_files(outcome["runs"][0]["run_dir"].as_str(), &pass.stderr);
    assert_eq!(outcome["errors"], Value::Array(vec![]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_required_run_landed(&fixture, &task_id);
    // The broker was usable when the pass looked: the claims were never
    // held.
    let held = dagq(env, &["events", "--kind", "broker_claims_held"]);
    assert_eq!(held["events"], json!([]), "{held}");
    wait_until_not_listed(cmux, &pass.workspaces[0].1);
}

/// `[broker] mode = "required"` with the queue's broker stopped (ADR-t838-1,
/// goal 59): a resident supervisor claims nothing and the inbox sees the
/// attention `broker_claims_held` (`not_ready`, next `dagq broker
/// status`) while the task stays ready with no run, so no worker starts,
/// with the built-in tools or any other. The supervisor makes the broker
/// ready itself (task 923), the hold ends (`broker_claims_resumed`) before
/// the claim, and the run lands through the broker as above.
#[test]
#[ignore = "needs a running cmux and podman; builds the broker's image; run with --ignored"]
fn a_required_queue_claims_nothing_while_its_broker_is_stopped_and_tells_the_inbox() {
    let fixture = fixture();
    let Fixture {
        cmux, repo, env, ..
    } = &fixture;
    client_next_to_dagq();
    fs::write(repo.join("dagq.toml"), required_dagq_toml()).unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-q", "-m", "broker required"]);
    let _broker = BrokerGuard(env);
    // Stopped: this queue's broker was never started.
    let before = dagq(env, &["broker", "status"]);
    assert_ne!(before["state"], "running", "{before}");
    let task_id = add_required_task(env, "e2e required broker stopped");
    let mut guard = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };

    let mut supervisor = ChildGuard::new(
        Command::new(BIN)
            // A person's supervisor, not the session's running the tests.
            .without_actor_env()
            .current_dir(&fixture.repo)
            .env("XDG_DATA_HOME", &fixture.env.data_home)
            .args(["supervise", "--parallel", "1"])
            .args(NO_LOAD_HOLD)
            // Only the claims and the broker: no job of its own runs the
            // stub while the broker is made ready.
            .args(["--observe-interval", "0", "--observe-daily", "false"])
            .args(["--throughput-review", "false", "--report-daily", "false"])
            .args([
                "--forecast-snapshots",
                "false",
                "--host-metrics-interval",
                "0",
            ])
            .arg("--cmux")
            .arg(&fixture.cmux)
            .arg("--claude")
            .arg(&fixture.stub)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = reader(supervisor.0.stderr.take().unwrap());
    let runs = |env: &Env| dagq(env, &["show", &task_id, "--full"])["runs"].clone();

    // While the supervisor makes the broker ready, the inbox is told and
    // nothing is claimed. The runs are read before the status: a hold seen
    // after no run was seen means none was claimed before it. A broker the
    // supervisor makes ready within one look (dagq's machine running and
    // the image built by another test) can end the hold before it is seen;
    // the queue's events below show it then.
    let deadline = Instant::now() + WAIT_LIMIT;
    let attention = loop {
        if runs(env) != json!([]) {
            eprintln!("the hold ended before a look saw it; its events tell");
            break None;
        }
        let status = dagq(env, &["status", "--role", "inbox"]);
        let held = status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["kind"] == "broker_claims_held")
            .cloned();
        if held.is_some() {
            break held;
        }
        assert!(
            Instant::now() < deadline && supervisor.0.try_wait().unwrap().is_none(),
            "no broker_claims_held attention: {status}"
        );
        thread::sleep(Duration::from_millis(100));
    };
    if let Some(attention) = &attention {
        assert_eq!(attention["status"], "not_ready", "{attention}");
        assert_eq!(attention["next"], "dagq broker status", "{attention}");
    }

    // The supervisor's own start of the broker ends the hold; the run is
    // claimed after it and lands.
    let deadline = Instant::now() + BROKER_START_LIMIT;
    loop {
        if let Some(id) = runs(env)
            .as_array()
            .unwrap()
            .last()
            .and_then(|run| run["workspace_id"].as_str())
        {
            guard.record(id);
        }
        if dagq(env, &["show", &task_id])["task"]["status"] == "completed" {
            break;
        }
        if Instant::now() >= deadline || supervisor.0.try_wait().unwrap().is_some() {
            let _ = supervisor.0.kill();
            let _ = supervisor.0.wait();
            supervisor.reaped();
            let log = joined(stderr, "the supervisor's stderr reader");
            let runs = runs(env);
            print_broker_files(runs[0]["run_dir"].as_str(), &log);
            panic!("the task did not land within {BROKER_START_LIMIT:?}; runs: {runs}");
        }
        thread::sleep(Duration::from_millis(500));
    }
    // A stop request (SIGINT): the supervisor drains and exits.
    // SAFETY: kill(2) takes no pointers.
    unsafe { libc::kill(supervisor.0.id() as libc::pid_t, libc::SIGINT) };
    let exited = {
        let _waiting = common::within(common::STEP_LIMIT, "the supervisor to stop");
        supervisor.0.wait().unwrap()
    };
    supervisor.reaped();
    let log = joined(stderr, "the supervisor's stderr reader");
    assert!(exited.success(), "supervise failed ({exited}): {log}");
    assert_required_run_landed(&fixture, &task_id);

    let claimed = dagq(
        env,
        &[
            "events",
            "--kind",
            "broker_claims_held",
            "--kind",
            "broker_started",
            "--kind",
            "broker_claims_resumed",
            "--kind",
            "run_claimed",
            "--limit",
            "1000",
            "--full",
        ],
    );
    let events = claimed["events"].as_array().unwrap();
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| event["kind"].as_str().unwrap())
        .collect();
    let at = |kind: &str| {
        let found: Vec<usize> = (0..kinds.len()).filter(|&i| kinds[i] == kind).collect();
        assert_eq!(found.len(), 1, "one {kind}: {claimed}");
        found[0]
    };
    // Held on the first pass, before the broker was ready; resumed before
    // the claim. `broker_started` is recorded when a pass reaps the
    // supervisor's start, which may come one pass after a look at the
    // running broker resumed the claims.
    assert_eq!(at("broker_claims_held"), 0, "{claimed}");
    assert_eq!(events[0]["payload"]["reason"], "not_ready", "{claimed}");
    assert!(at("broker_claims_held") < at("broker_started"), "{claimed}");
    assert!(at("broker_claims_resumed") < at("run_claimed"), "{claimed}");
    let status = dagq(env, &["status", "--role", "inbox"]);
    assert!(
        !status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["kind"] == "broker_claims_held"),
        "{status}"
    );
    for id in guard.ids.clone() {
        wait_until_not_listed(cmux, &id);
    }
}
