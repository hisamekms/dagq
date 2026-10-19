//! Running the `dagq` binary against a queue for the CLI tests
//! (`tests/cli_*.rs`), and the queue they start from.

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;
use tempfile::TempDir;

use super::{Bounded, WithoutActor, service::OwnedByTest};

/// A fresh queue, `queue.db` in a new temporary directory, as `init` with
/// `--db` leaves it (migrated, bound to no repository), copied from the
/// template (task 1060). The directory lives as long as the returned guard.
pub fn queue() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    super::template::queue(&db);
    (dir, db)
}

pub fn invoke(db: &Path, args: &[&str]) -> Output {
    invoke_as(None, db, args)
}

/// Run with `DAGQ_ROLE` set to `role`, or unset: the tests do not inherit
/// the role of the session running them. The `bin` directory next to the
/// queue comes first on PATH, for the stub programs a test puts there.
/// `XDG_CONFIG_HOME` is a directory beside the queue, so the host-wide
/// `host.toml` of the person running the tests (its `[push]`) is never
/// read; a test that wants a `[push]` gives the queue one with
/// [`push_stub`].
pub fn invoke_as(role: Option<&str>, db: &Path, args: &[&str]) -> Output {
    match role {
        Some(role) => invoke_with(&[("DAGQ_ROLE", role)], db, args),
        None => invoke_with(&[], db, args),
    }
}

/// [`invoke_as`] with the actor's variables (`DAGQ_ROLE`, `DAGQ_ACTOR_ID`,
/// `DAGQ_RUN_ID`, `DAGQ_TASK_ID`, and a session's `DAGQ_SESSION_KIND`,
/// `DAGQ_PLANNER_ID`, `DAGQ_PLANNER_ORIGIN`) that `env` sets, and no other.
pub fn invoke_with(env: &[(&str, &str)], db: &Path, args: &[&str]) -> Output {
    let bin = db.parent().unwrap().join("bin");
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command.without_actor_env().owned_by_test();
    command.env("PATH", path);
    command.env("XDG_CONFIG_HOME", db.parent().unwrap().join("config"));
    command.envs(env.iter().copied());
    command
        .arg("--db")
        .arg(db)
        .args(args)
        .bounded_output()
        .unwrap()
}

/// Give the queue at `db` a `[push]` (its `host.toml`) whose command
/// appends each message it reads on stdin to [`notifications`].
pub fn push_stub(db: &Path) {
    let dir = db.parent().unwrap();
    let stub = dir.join("bin").join("push");
    std::fs::create_dir_all(stub.parent().unwrap()).unwrap();
    crate::common::template::script(&stub, "#!/bin/sh\ncat >> \"${0%/*}/notifications\"\n");
    std::fs::write(
        dir.join("host.toml"),
        format!("[push]\ncommand = [\"{}\"]\n", stub.display()),
    )
    .unwrap();
}

/// Every message the [`push_stub`] of `db`'s queue was given, one JSON
/// object per line.
pub fn notifications(db: &Path) -> String {
    std::fs::read_to_string(db.parent().unwrap().join("bin/notifications")).unwrap_or_default()
}

/// The messages of [`notifications`], parsed.
pub fn pushed(db: &Path) -> Vec<Value> {
    notifications(db)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

pub fn ok_as(role: &str, db: &Path, args: &[&str]) -> Value {
    let output = invoke_as(Some(role), db, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

pub fn ok(db: &Path, args: &[&str]) -> Value {
    let output = invoke(db, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The domain's refusal of `args`, as the CLI prints it.
pub fn refused(db: &Path, args: &[&str]) -> String {
    let output = invoke(db, args);
    assert!(!output.status.success(), "{args:?} succeeded");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    error["error"].as_str().unwrap().to_owned()
}

/// `submit` as a planner session would run it: as the planner `planner`
/// (`DAGQ_PLANNER_ID`) when given, with the planner's origin when the
/// runtime opened it.
pub fn submit_from(
    db: &Path,
    planner: Option<&str>,
    origin: Option<&str>,
    args: &[&str],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command.without_actor_env();
    if let Some(planner) = planner {
        command.env("DAGQ_PLANNER_ID", planner);
    }
    if let Some(origin) = origin {
        command.env("DAGQ_PLANNER_ORIGIN", origin);
    }
    command
        .arg("--db")
        .arg(db)
        .arg("submit")
        .args(args)
        .bounded_output()
        .unwrap()
}
