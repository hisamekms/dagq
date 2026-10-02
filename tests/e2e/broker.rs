//! End-to-end happy path of the resource broker (goal 58, ADR-t827-4
//! decision 1): with `[broker] mode = "preferred"` in a disposable
//! repository, the host worker (the stub) does its task through the broker
//! in dagq's Podman machine, and the run lands. Needs podman as well as
//! cmux; the first run builds the broker's image in the machine.
use super::*;

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
/// running these tests.
fn client_next_to_dagq() -> PathBuf {
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
    if let Some(run_dir) = outcome["runs"][0]["run_dir"].as_str() {
        for entry in fs::read_dir(run_dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if name.starts_with("broker") && path.is_file() {
                eprintln!("{name}: {}", fs::read_to_string(&path).unwrap_or_default());
            }
        }
        eprintln!("{}", pass.stderr);
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
