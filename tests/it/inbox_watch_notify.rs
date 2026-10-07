//! The notification of a new ask is the inbox's watch's (ADR-t1433-1
//! decision 2): `ask` notifies nobody, and the `watch --role inbox` that
//! returns its `ask_opened` sends one `cmux notify` through the cmux of the
//! inbox's session (here the stub `cmux` [`cli::invoke`] puts first on
//! PATH), aimed at the inbox's workspace `up` recorded. A watch of another
//! role notifies nobody, nor does the next watch past the same ask.

use crate::common::{self, cli};
use dagq::{domain::SessionRole, infrastructure::sqlite::SqliteQueue};
use std::path::Path;

/// `watch` past `after` with `args`, waited on with a bound.
fn watch(db: &Path, after: i64, args: &[&str]) -> serde_json::Value {
    let _waiting = common::within(common::STEP_LIMIT, format!("watch {args:?} to return"));
    let after = after.to_string();
    let mut words = vec![
        "watch",
        "--after",
        &after,
        "--timeout",
        "1",
        "--interval",
        "1",
    ];
    words.extend_from_slice(args);
    cli::ok(db, &words)
}

#[test]
fn the_inbox_watch_notifies_a_new_ask_once_and_ask_notifies_nobody() {
    let (_dir, db) = cli::queue();
    cli::ok(&db, &["add", "first"]);
    SqliteQueue::open(&db)
        .unwrap()
        .register_session_workspace(SessionRole::Inbox, "INBOX-UUID")
        .unwrap();
    let start = cli::ok(&db, &["status", "--role", "inbox"])["cursor"]
        .as_i64()
        .unwrap();
    let asked = cli::ok(
        &db,
        &[
            "ask",
            "--kind",
            "decide",
            "--because",
            "recovery_failed",
            "--question",
            "which way?",
            "--task",
            "1",
            "--cmux",
            "/nonexistent/cmux",
        ],
    );
    assert_eq!(asked["created"], true);
    assert_eq!(asked.get("notified"), None, "{asked}");
    assert_eq!(cli::notifications(&db), "", "ask notifies nobody");
    // A watch of another role tells nobody.
    let all = watch(&db, start, &[]);
    assert!(!all["events"].as_array().unwrap().is_empty(), "{all}");
    assert_eq!(cli::notifications(&db), "");
    // The inbox's watch tells the person once, in the inbox's workspace.
    let inbox = watch(&db, start, &["--role", "inbox"]);
    assert!(
        inbox["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "ask_opened"),
        "{inbox}"
    );
    let sent = cli::notifications(&db);
    assert_eq!(sent.matches("notify\n").count(), 1, "{sent}");
    let id = asked["id"].as_i64().unwrap();
    assert!(sent.contains(&format!("ask #{id} decide")), "{sent}");
    assert!(sent.contains("--body\nwhich way?\ntask 1\n"), "{sent}");
    assert!(sent.contains("--workspace\nINBOX-UUID\n"), "{sent}");
    // The next watch past the cursor does not tell it again.
    let cursor = inbox["cursor"].as_i64().unwrap();
    watch(&db, cursor, &["--role", "inbox"]);
    assert_eq!(cli::notifications(&db).matches("notify\n").count(), 1);
}
