//! The notification of a new ask is the inbox's watch's, through the
//! host's `[push]` (ADR-t2159-1 decision 4): `ask` notifies nobody, and the
//! `watch --role inbox` that returns `ask_opened` hands the `[push]` command
//! one `ask_opened` message with the count and nothing of the ask. Without
//! a `[push]` nothing is sent, and a command that fails is reported in the
//! watch's output without stopping it.

use crate::common::{self, cli};
use std::path::Path;

/// `watch --role inbox` past `after`, waited on with a bound.
fn watch(db: &Path, after: i64) -> serde_json::Value {
    let _waiting = common::within(common::STEP_LIMIT, "watch to return");
    let after = after.to_string();
    cli::ok(
        db,
        &[
            "watch",
            "--role",
            "inbox",
            "--after",
            &after,
            "--timeout",
            "1",
            "--interval",
            "1",
        ],
    )
}

/// A `decide` ask on the task `task`.
fn ask(db: &Path, task: &str, question: &str) {
    cli::ok(
        db,
        &[
            "ask",
            "--kind",
            "decide",
            "--because",
            "recovery_failed",
            "--question",
            question,
            "--task",
            task,
        ],
    );
}

#[test]
fn the_inbox_watch_pushes_a_new_ask_once_and_only_with_a_push_that_may_fail() {
    let (_dir, db) = cli::queue();
    cli::ok(&db, &["add", "first"]);
    cli::ok(&db, &["add", "second"]);
    cli::ok(&db, &["add", "third"]);
    let start = cli::ok(&db, &["status", "--role", "inbox"])["cursor"]
        .as_i64()
        .unwrap();
    // No `[push]`: the watch returns the ask and sends nothing.
    ask(&db, "1", "which way?");
    let unpushed = watch(&db, start);
    assert_eq!(unpushed["events"][0]["kind"], "ask_opened", "{unpushed}");
    assert_eq!(unpushed.get("ask_push"), None, "{unpushed}");

    cli::push_stub(&db);
    let pushed = watch(&db, start);
    assert_eq!(
        pushed["ask_push"],
        serde_json::json!({"asks": 1, "sent": true}),
        "{pushed}"
    );
    let sent = cli::pushed(&db);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0]["kind"], "ask_opened");
    assert_eq!(sent[0]["new_asks"], 1);
    assert_eq!(sent[0]["queue"], db.display().to_string());
    assert!(!cli::notifications(&db).contains("which way"), "{sent:?}");
    // The next watch past the cursor does not send it again.
    let cursor = pushed["cursor"].as_i64().unwrap();
    let quiet = watch(&db, cursor);
    assert_eq!(quiet.get("ask_push"), None, "{quiet}");
    assert_eq!(cli::pushed(&db).len(), 1);

    // `--cmux`, which an older inbox may pass, is accepted and ignored.
    let _waiting = common::within(common::STEP_LIMIT, "watch --cmux to return");
    let ignored = cli::ok(
        &db,
        &[
            "watch",
            "--role",
            "inbox",
            "--after",
            &cursor.to_string(),
            "--timeout",
            "0",
            "--cmux",
            "/nonexistent/cmux",
        ],
    );
    assert_eq!(ignored.get("ask_push"), None, "{ignored}");
    drop(_waiting);
    // A command that fails leaves the watch's output with why, the
    // command's arguments masked.
    std::fs::write(
        db.parent().unwrap().join("host.toml"),
        "[push]\ncommand = [\"/bin/sh\", \"-c\", \"echo refused $0 >&2; exit 3\", \"https://hooks.example/secret\"]\n",
    )
    .unwrap();
    ask(&db, "2", "another way?");
    let failed = watch(&db, cursor);
    assert_eq!(failed["events"][0]["kind"], "ask_opened", "{failed}");
    assert_eq!(failed["ask_push"]["sent"], false, "{failed}");
    let error = failed["ask_push"]["error"].as_str().unwrap();
    assert!(error.contains("exit code 3"), "{error}");
    assert!(error.contains("refused [argument]"), "{error}");
    assert!(!error.contains("hooks.example"), "{error}");

    // A `[push]` that cannot be read fails the push saying so, without
    // the file's text (which may quote the command's arguments).
    let cursor = failed["cursor"].as_i64().unwrap();
    std::fs::write(
        db.parent().unwrap().join("host.toml"),
        "[push]\ncommand = [\"/bin/push\"] \"https://hooks.example/secret\"\n",
    )
    .unwrap();
    ask(&db, "3", "a third way?");
    let unreadable = watch(&db, cursor);
    assert_eq!(unreadable["ask_push"]["sent"], false, "{unreadable}");
    let error = unreadable["ask_push"]["error"].as_str().unwrap();
    assert!(
        error.contains("[push] could not be read or is not valid"),
        "{error}"
    );
    assert!(!error.contains("hooks.example"), "{error}");
}

/// A queue bound to a repository is named after that repository in the
/// inbox's notification, also when its watch runs in another repository
/// and names the queue with `--db`, which binds nothing: the binding comes
/// with the service's answer to each look, as the queue records it.
#[test]
fn a_bound_queue_s_push_names_its_repository_when_watched_from_another() {
    use crate::common::{Bounded, WithoutActor, service::OwnedByTest};
    let (dir, db) = cli::queue();
    let bound = dir.path().join("bound-repo");
    let elsewhere = dir.path().join("elsewhere");
    for repo in [&bound, &elsewhere] {
        std::fs::create_dir(repo).unwrap();
        common::template::repository(repo, "seed");
    }
    let common_dir = std::fs::canonicalize(bound.join(".git")).unwrap();
    dagq::infrastructure::sqlite::SqliteQueue::open(&db)
        .unwrap()
        .bind_repository(common_dir.to_str().unwrap())
        .unwrap();
    cli::ok(&db, &["add", "first"]);
    let start = cli::ok(&db, &["status", "--role", "inbox"])["cursor"]
        .as_i64()
        .unwrap();
    cli::push_stub(&db);
    ask(&db, "1", "which way?");

    let after = start.to_string();
    let args = [
        "watch",
        "--role",
        "inbox",
        "--after",
        &after,
        "--timeout",
        "1",
        "--interval",
        "1",
    ];
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_dagq"));
    command
        .without_actor_env()
        .owned_by_test()
        .current_dir(&elsewhere)
        .env("DAGQ_ROLE", "inbox")
        .env("XDG_CONFIG_HOME", dir.path().join("config"));
    cli::reach(&mut command, &db, &args);
    let output = command
        .arg("--db")
        .arg(&db)
        .args(args)
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let watched: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        watched["ask_push"],
        serde_json::json!({"asks": 1, "sent": true}),
        "{watched}"
    );
    let sent = cli::pushed(&db);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(
        sent[0]["title"], "bound-repo: 1 new ask(s) for the inbox",
        "{sent:?}"
    );
    assert_eq!(sent[0]["queue"], db.display().to_string());
}
