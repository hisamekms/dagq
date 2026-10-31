//! dagq calls no cmux (ADR-t2159-1 decision 1), the one place the tests
//! keep a cmux: the command lines an earlier binary registered or started
//! carry `--cmux <path>` (ADR-t2159-1 decision 6), and here it names a
//! cmux that records its calls, which is also first on PATH. The
//! `supervise` argv of a registered supervisor starts on the new binary and
//! finishes its pass, and the service, the observer, the reads and an ask
//! given such an argv do their work, and neither cmux is run. That every
//! command accepts the flag and hides it from its help is the unit test
//! `tests::a_registered_argv_with_a_cmux_parses_and_no_help_shows_it` of
//! `src/main.rs`.

use crate::common::{Bounded, actor::WithoutActor, service::OwnedByTest};
use crate::plan_review::fixture;
use dagq::{
    application::TaskStore,
    domain::{TaskAction, TaskId},
    infrastructure::sqlite::SqliteQueue,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// A cmux in `bin` beside `db` that appends its arguments to `calls` there,
/// and the PATH with `bin` first.
fn recording_cmux(db: &Path) -> (PathBuf, std::ffi::OsString) {
    let bin = db.parent().unwrap().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let cmux = bin.join("cmux");
    crate::common::template::script(
        &cmux,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"${0%/*}/calls\"\n",
    );
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    (cmux, path)
}

/// What the recording cmux beside `db` was called with.
fn calls(db: &Path) -> String {
    fs::read_to_string(db.parent().unwrap().join("bin").join("calls")).unwrap_or_default()
}

/// `dagq --db db ARGS --cmux CMUX` with `path` as PATH, as a person runs it.
fn with_cmux(db: &Path, path: &std::ffi::OsStr, cmux: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .owned_by_test()
        .env("PATH", path)
        .arg("--db")
        .arg(db)
        .args(args)
        .arg("--cmux")
        .arg(cmux)
        .bounded_output()
        .unwrap()
}

fn succeeded(args: &[&str], output: &Output) {
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_supervise_argv_with_a_cmux_starts_and_finishes_its_pass_without_running_it() {
    let fx = fixture();
    SqliteQueue::open(&fx.db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let (cmux, path) = recording_cmux(&fx.db);
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
    assert_eq!(calls(&fx.db), "", "the supervisor ran cmux");
}

/// `service start`, the observer's dry run, `stats`, `planners` and an ask
/// given a registered argv's `--cmux` do their work, and no cmux is run:
/// the ask is told by the inbox's watch (ADR-t2159-1 decision 4).
#[test]
fn the_service_the_observer_the_reads_and_an_ask_with_a_cmux_run_none() {
    let fx = fixture();
    let (cmux, path) = recording_cmux(&fx.db);
    for args in [
        &["service", "start"][..],
        &["observe", "--dry-run", "--daily"],
        // Stopped while it reads every event of the queue (ADR-t3745-1): task 1669 takes this cfg off.
        #[cfg(any())]
        &["stats", "--full"],
        &["planners"],
        &[
            "ask",
            "--kind",
            "decide",
            "--because",
            "scope",
            "--task",
            "1",
            "--question",
            "which way?",
            "--option",
            "a",
        ],
    ] {
        succeeded(args, &with_cmux(&fx.db, &path, &cmux, args));
    }
    let asks = crate::common::cli::ok(&fx.db, &["asks", "--all"]);
    assert_eq!(asks["asks"][0]["question"], "which way?", "{asks}");
    let stopped = crate::common::cli::invoke(&fx.db, &["service", "stop"]);
    succeeded(&["service", "stop"], &stopped);
    assert_eq!(calls(&fx.db), "", "a command ran cmux");
}
