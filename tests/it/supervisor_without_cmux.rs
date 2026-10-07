//! The supervisor calls no cmux (ADR-t1433-1): a `supervise` argv a
//! registered supervisor has, its `--cmux` naming one that records its
//! calls (and a `cmux` first on PATH that does too), starts on the new
//! binary, finishes its pass and runs neither (registered argv keep
//! working, ADR-t1433-4).

use crate::common::{Bounded, actor::WithoutActor};
use crate::plan_review::fixture;
use dagq::{
    application::TaskStore,
    domain::{TaskAction, TaskId},
    infrastructure::sqlite::SqliteQueue,
};
use std::{fs, process::Command};

#[test]
fn a_supervise_argv_with_a_cmux_starts_and_finishes_its_pass_without_running_it() {
    let fx = fixture();
    SqliteQueue::open(&fx.db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let bin = fx.db.parent().unwrap().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let cmux = bin.join("cmux");
    crate::common::template::script(
        &cmux,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"${0%/*}/calls\"\n",
    );
    let path = std::env::join_paths(
        std::iter::once(bin.clone())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .env("PATH", path)
        .args([
            "--db",
            fx.db.to_str().unwrap(),
            "supervise",
            "--once",
            "--no-claude",
        ])
        .args(["--repo", fx.repo.to_str().unwrap()])
        .args(["--cmux", cmux.to_str().unwrap()])
        .args(["--observe-interval", "0", "--throughput-review", "false"])
        .args(["--report-daily", "false", "--forecast-snapshots", "false"])
        .args(["--host-metrics-interval", "0", "--max-load", "0"])
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let outcome: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["outcome"], "finished", "{outcome}");
    assert_eq!(outcome["errors"], serde_json::json!([]), "{outcome}");
    let calls = fs::read_to_string(bin.join("calls")).unwrap_or_default();
    assert_eq!(calls, "", "the supervisor ran cmux");
}
