//! `dagq broker status|start|stop|logs|audit` without a real podman (docs/design/broker.md
//! "container and Podman machine"): no podman is a structured error, and a
//! stand-in podman that knows no machine drives the steps that need no
//! machine. The tests on a real podman are in `broker_podman.rs`.

use crate::common;
use dagq::infrastructure::git_binary::git_executable;

use common::{Bounded, WithoutActor};

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use serde_json::Value;

fn dagq(dir: &Path, data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .env("XDG_DATA_HOME", data)
        .args(args)
        .current_dir(dir)
        .bounded_output()
        .unwrap()
}

fn json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(bytes)))
}

/// A podman that knows no machine and succeeds at everything else,
/// logging its arguments.
fn fake_podman(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("podman");
    crate::common::template::script(
        &path,
        "#!/bin/sh\necho \"$*\" >> \"${0%/*}/podman.log\"\ncase \"$*\" in\n  'machine list'*) echo '[]' ;;\nesac\nexit 0\n",
    );

    path
}

#[test]
fn without_podman_status_reports_it_and_start_fails_with_its_code() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q").join("queue.db");
    let missing = dir.path().join("no-podman");
    let (db, missing) = (db.to_str().unwrap(), missing.to_str().unwrap());
    let status = dagq(
        dir.path(),
        dir.path(),
        &["--db", db, "broker", "status", "--podman", missing],
    );
    assert!(status.status.success(), "{status:?}");
    let status = json(&status.stdout);
    assert_eq!(status["state"], "podman_missing");
    assert_eq!(status["error"]["code"], "podman_missing");
    for command in ["start", "stop"] {
        let output = dagq(
            dir.path(),
            dir.path(),
            &["--db", db, "broker", command, "--podman", missing],
        );
        assert!(!output.status.success(), "{command}");
        let error = json(&output.stderr);
        assert_eq!(error["broker"]["code"], "podman_missing", "{command}");
        assert!(
            error["error"]
                .as_str()
                .unwrap()
                .contains("brew install podman"),
            "{error}"
        );
    }
    // A queue named by --db has no repository to mount.
    let podman = fake_podman(dir.path());
    let podman = podman.to_str().unwrap();
    let status = json(
        &dagq(
            dir.path(),
            dir.path(),
            &["--db", db, "broker", "status", "--podman", podman],
        )
        .stdout,
    );
    assert_eq!(status["state"], "repository_unknown");
    let output = dagq(
        dir.path(),
        dir.path(),
        &["--db", db, "broker", "start", "--podman", podman],
    );
    assert!(!output.status.success());
    assert_eq!(json(&output.stderr)["broker"]["code"], "repository_unknown");
}

#[test]
fn a_repository_queue_walks_the_steps_and_records_the_failure() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, data) = (dir.path().join("repo"), dir.path().join("data"));
    fs::create_dir_all(&repo).unwrap();
    let git = Command::new(git_executable().expect("git executable"))
        .args(["init", "-q"])
        .current_dir(&repo)
        .bounded_status()
        .unwrap();
    assert!(git.success());
    let podman = fake_podman(dir.path());
    let podman = podman.to_str().unwrap();

    let status = dagq(&repo, &data, &["broker", "status", "--podman", podman]);
    assert!(status.status.success(), "{status:?}");
    let status = json(&status.stdout);
    assert_eq!(status["state"], "machine_missing");
    assert_eq!(status["machine"]["name"], "dagq");

    // The stand-in "inits" the machine but never lists it.
    let output = dagq(
        &repo,
        &data,
        &["broker", "start", "--podman", podman, "--port", "45999"],
    );
    assert!(!output.status.success());
    assert_eq!(json(&output.stderr)["broker"]["code"], "machine_failed");
    let log = fs::read_to_string(dir.path().join("podman.log")).unwrap();
    assert!(
        log.contains(
            "machine init --cpus 1 --memory 1024 --disk-size 10 --update-connection=false dagq"
        ),
        "{log}"
    );
    assert!(!log.contains("system connection"), "{log}");
    // What the container would mount was made, and the failure recorded.
    let located = json(&dagq(&repo, &data, &["locate"]).stdout);
    let queue_dir = Path::new(located["queue_dir"].as_str().unwrap());
    assert!(queue_dir.join("broker/key").is_file());
    assert!(queue_dir.join("broker/active").is_dir());
    assert!(queue_dir.join("broker/audit").is_dir());
    let state = json(&fs::read(queue_dir.join("broker/state.json")).unwrap());
    assert_eq!(state["state"], "machine_failed");
    assert_eq!(state["port"], 45999);
    let status = json(&dagq(&repo, &data, &["broker", "status", "--podman", podman]).stdout);
    assert_eq!(status["recorded"]["port"], 45999);

    // Stop: no machine, so nothing is stopped.
    let stopped = dagq(&repo, &data, &["broker", "stop", "--podman", podman]);
    assert!(stopped.status.success(), "{stopped:?}");
    let stopped = json(&stopped.stdout);
    assert_eq!(stopped["stop"]["container_stopped"], false);
    assert_eq!(stopped["stop"]["machine_stopped"], false);
    assert!(
        !fs::read_to_string(dir.path().join("podman.log"))
            .unwrap()
            .contains("machine stop")
    );
}

#[test]
fn logs_only_read_and_fail_with_a_code_without_podman_or_a_machine() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, data) = (dir.path().join("repo"), dir.path().join("data"));
    fs::create_dir_all(&repo).unwrap();
    let git = Command::new(git_executable().expect("git executable"))
        .args(["init", "-q"])
        .current_dir(&repo)
        .bounded_status()
        .unwrap();
    assert!(git.success());
    let missing = dir.path().join("no-podman");
    let output = dagq(
        &repo,
        &data,
        &["broker", "logs", "--podman", missing.to_str().unwrap()],
    );
    assert!(!output.status.success());
    assert_eq!(json(&output.stderr)["broker"]["code"], "podman_missing");

    let podman = fake_podman(dir.path());
    let output = dagq(
        &repo,
        &data,
        &[
            "broker",
            "logs",
            "--tail",
            "5",
            "--podman",
            podman.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert_eq!(json(&output.stderr)["broker"]["code"], "machine_missing");
    // Only the machine was asked about: nothing was inited or started.
    let log = fs::read_to_string(dir.path().join("podman.log")).unwrap();
    assert_eq!(log.trim(), "machine list --format json");
}

#[test]
fn audit_reads_the_day_files_as_any_role_without_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q").join("queue.db");
    let audit = dir.path().join("q/broker/audit");
    let line = |ts: &str, run: &str| {
        format!(
            r#"{{"ts":"{ts}","request_id":"x","jti":null,"run_id":"{run}","task_id":7,"actor_id":null,"backend":null,"op":"health","capability":null,"path":null,"program":null,"argc":null,"argv_sha256":null,"result":"ok","exit_code":null,"duration_ms":0,"bytes_in":0,"bytes_out":2}}"#
        )
    };
    let args = |extra: &[&str]| -> Vec<String> {
        ["--db", db.to_str().unwrap(), "broker", "audit"]
            .iter()
            .chain(extra)
            .map(|word| (*word).to_owned())
            .collect()
    };
    let run = |extra: &[&str], role: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
        command
            .without_actor_env()
            .env("XDG_DATA_HOME", dir.path())
            .args(args(extra))
            .current_dir(dir.path());
        if let Some(role) = role {
            command.envs([
                ("DAGQ_ROLE", role),
                ("DAGQ_ACTOR_ID", "worker:r1"),
                ("DAGQ_RUN_ID", "r1"),
                ("DAGQ_TASK_ID", "7"),
            ]);
        }
        command.bounded_output().unwrap()
    };

    // No audit yet: no line, and no queue DB is made.
    let output = run(&[], None);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(json(&output.stdout)["entries"], serde_json::json!([]));
    assert!(!db.exists());

    fs::create_dir_all(&audit).unwrap();
    fs::write(
        audit.join("2026-09-28.jsonl"),
        format!("{}\n{{broken\n", line("2026-09-28T10:00:00.000Z", "r1")),
    )
    .unwrap();
    fs::write(
        audit.join("2026-09-29.jsonl"),
        format!(
            "{}\n{}\n",
            line("2026-09-29T10:00:00.000Z", "r2"),
            line("2026-09-29T11:00:00.000Z", "r1")
        ),
    )
    .unwrap();
    let output = run(
        &["--run", "r1", "--task", "7", "--since", "2026-09-28"],
        Some("worker"),
    );
    assert!(output.status.success(), "{output:?}");
    let report = json(&output.stdout);
    let times: Vec<&str> = report["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["ts"].as_str().unwrap())
        .collect();
    assert_eq!(
        times,
        ["2026-09-28T10:00:00.000Z", "2026-09-29T11:00:00.000Z"]
    );
    assert_eq!(report["skipped"], 1);
    assert_eq!(
        report["entries"][0],
        json(line("2026-09-28T10:00:00.000Z", "r1").as_bytes())
    );
    let output = run(&["--until", "2026-09-29", "--limit", "1"], None);
    let report = json(&output.stdout);
    assert_eq!(report["entries"].as_array().unwrap().len(), 1);
    assert_eq!(report["dropped"], 0);
    assert!(!db.exists());

    let output = run(&["--since", "yesterday"], None);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not a UTC time"),
        "{output:?}"
    );
}
