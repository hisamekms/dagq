//! What wakes `watch --role inbox` (ADR-t1418-1): `update_installed` and
//! the hourly `throughput_review_reported` do not on their own; the next
//! event that does brings them back with it, oldest first. A watch for
//! every role still wakes on each.

use crate::common::{self, cli::*};
use dagq::{application::RunLog, domain::EventKind, infrastructure::sqlite::SqliteQueue};
use serde_json::{Value, json};
use std::path::Path;

fn record(db: &Path, kind: EventKind, payload: Value) -> i64 {
    RunLog::record_queue_event(&SqliteQueue::open(db).unwrap(), kind, payload)
        .unwrap()
        .as_i64()
}

fn installed(db: &Path) -> i64 {
    record(
        db,
        EventKind::UpdateInstalled,
        json!({"commit": "0123456789abcdef0123456789abcdef01234567"}),
    )
}

fn reported(db: &Path, mode: &str) -> i64 {
    record(
        db,
        EventKind::ThroughputReviewReported,
        json!({"mode": mode, "period": "2026-10-02T12", "conclusion": "steady"}),
    )
}

/// `watch` with `args` and a one-second timeout, waited on with a bound.
fn watch(db: &Path, args: &[&str]) -> Value {
    let _waiting = common::within(common::STEP_LIMIT, format!("watch {args:?} to return"));
    let mut words = vec!["watch", "--timeout", "1", "--interval", "1"];
    words.extend_from_slice(args);
    ok(db, &words)
}

fn kinds_and_ids(value: &Value) -> Vec<(String, i64)> {
    value["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["kind"].as_str().unwrap().to_owned(),
                e["id"].as_i64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn the_inbox_watch_sleeps_through_notices_and_brings_them_with_the_next_ask() {
    let (_dir, db) = queue();
    let start = ok(&db, &["status", "--role", "inbox"])["cursor"]
        .as_i64()
        .unwrap();
    let after = start.to_string();
    let update = installed(&db);
    let hourly = reported(&db, "hourly");

    // The notices alone: a timeout with no events and the cursor kept.
    let slept = watch(&db, &["--role", "inbox", "--after", &after]);
    assert_eq!(slept["events"], json!([]), "{slept}");
    assert_eq!(slept["cursor"], start, "{slept}");
    assert_eq!(slept["supervisors_changed"], false, "{slept}");

    // A watch for every role and `events` still see them.
    let every = watch(&db, &["--after", &after]);
    assert_eq!(
        kinds_and_ids(&every),
        [
            ("update_installed".to_owned(), update),
            ("throughput_review_reported".to_owned(), hourly),
        ],
        "{every}"
    );
    let events = ok(&db, &["events", "--after", &after]);
    assert_eq!(kinds_and_ids(&events), kinds_and_ids(&every), "{events}");
    // The planner's watch has none, as before.
    let planner = watch(&db, &["--role", "planner", "--after", &after]);
    assert_eq!(planner["events"], json!([]), "{planner}");

    // An ask wakes it, with the notices before it, oldest first.
    ok(
        &db,
        &[
            "ask",
            "--kind",
            "blocked",
            "--because",
            "scope",
            "--question",
            "stuck?",
            "--option",
            "wait",
            "--recommend",
            "wait",
        ],
    );
    let woken = watch(&db, &["--role", "inbox", "--after", &after]);
    let got = kinds_and_ids(&woken);
    assert_eq!(got.len(), 3, "{woken}");
    assert_eq!(got[0], ("update_installed".to_owned(), update));
    assert_eq!(got[1], ("throughput_review_reported".to_owned(), hourly));
    assert_eq!(got[2].0, "ask_opened");
    assert_eq!(woken["cursor"], got[2].1, "{woken}");
}

#[test]
fn the_daily_and_weekly_reviews_and_a_failed_hourly_one_wake_the_inbox() {
    let (_dir, db) = queue();
    for mode in ["daily", "weekly"] {
        let cursor = ok(&db, &["status", "--role", "inbox"])["cursor"]
            .as_i64()
            .unwrap();
        let hourly = reported(&db, "hourly");
        let review = reported(&db, mode);
        let woken = watch(&db, &["--role", "inbox", "--after", &cursor.to_string()]);
        assert_eq!(
            kinds_and_ids(&woken),
            [
                ("throughput_review_reported".to_owned(), hourly),
                ("throughput_review_reported".to_owned(), review),
            ],
            "{mode}: {woken}"
        );
        assert_eq!(woken["cursor"], review, "{woken}");
    }
    let cursor = ok(&db, &["status", "--role", "inbox"])["cursor"]
        .as_i64()
        .unwrap();
    let failed = record(
        &db,
        EventKind::ThroughputReviewFinished,
        json!({"mode": "hourly", "period": "2026-10-02T13", "outcome": "failed"}),
    );
    let woken = watch(&db, &["--role", "inbox", "--after", &cursor.to_string()]);
    assert_eq!(
        kinds_and_ids(&woken),
        [("throughput_review_finished".to_owned(), failed)],
        "{woken}"
    );
}

fn open_ask(db: &Path) {
    ok(
        db,
        &[
            "ask",
            "--kind",
            "blocked",
            "--because",
            "scope",
            "--question",
            "stuck?",
            "--option",
            "wait",
            "--recommend",
            "wait",
        ],
    );
}

/// The kinds of a watch's events, in order.
fn kinds(value: &Value) -> Vec<String> {
    kinds_and_ids(value)
        .into_iter()
        .map(|(kind, _)| kind)
        .collect()
}

#[test]
fn every_notice_past_the_cursor_comes_back_before_or_after_what_woke_it() {
    let (_dir, db) = queue();
    let start = ok(&db, &["status", "--role", "inbox"])["cursor"]
        .as_i64()
        .unwrap();
    let after = start.to_string();
    // More notices than one page, before an ask.
    let mut notices = Vec::new();
    for _ in 0..101 {
        notices.push(installed(&db));
    }
    let slept = watch(&db, &["--role", "inbox", "--after", &after]);
    assert_eq!(slept["events"], json!([]), "{slept}");
    assert_eq!(slept["cursor"], start, "{slept}");
    open_ask(&db);
    let woken = watch(&db, &["--role", "inbox", "--after", &after]);
    let got = kinds_and_ids(&woken);
    assert_eq!(got.len(), 102, "{woken}");
    assert_eq!(
        got[..101].iter().map(|(_, id)| *id).collect::<Vec<_>>(),
        notices
    );
    assert_eq!(got[101].0, "ask_opened");
    assert_eq!(woken["cursor"], got[101].1, "{woken}");

    // A daily review (an event that wakes it too) followed by more notices
    // than one page: all of them too.
    let after = got[101].1.to_string();
    let daily = reported(&db, "daily");
    let mut later = Vec::new();
    for _ in 0..101 {
        later.push(reported(&db, "hourly"));
    }
    let woken = watch(&db, &["--role", "inbox", "--after", &after]);
    let got = kinds_and_ids(&woken);
    assert_eq!(got.len(), 102, "{woken}");
    assert_eq!(got[0], ("throughput_review_reported".to_owned(), daily));
    assert_eq!(
        got[1..].iter().map(|(_, id)| *id).collect::<Vec<_>>(),
        later
    );
    assert_eq!(woken["cursor"], *later.last().unwrap(), "{woken}");
    // A watch with no role keeps its page of 100.
    let every = watch(&db, &["--after", &after]);
    assert_eq!(kinds(&every).len(), 100, "{every}");
    assert_eq!(every["cursor"], later[98], "{every}");
}

#[test]
fn the_supervisors_health_wakes_the_inbox_with_every_notice_held() {
    use dagq::{VERSION, domain::LeaseToken};
    use std::{
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };
    let (_dir, db) = queue();
    let start = ok(&db, &["status", "--role", "inbox"])["cursor"]
        .as_i64()
        .unwrap();
    let mut notices = Vec::new();
    for _ in 0..101 {
        notices.push(installed(&db));
    }
    notices.push(reported(&db, "hourly"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    common::WithoutActor::without_actor_env(&mut command);
    let child = command
        .arg("--db")
        .arg(&db)
        .args([
            "watch",
            "--role",
            "inbox",
            "--until-attention",
            "--interval",
            "1",
        ])
        .args(["--after", &start.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut child = common::KillOnDrop::new(child, "the inbox watch");
    {
        // The watch is up (its baseline taken) once it says it watches.
        let _waiting = common::within(common::STEP_LIMIT, "the watch to be watching");
        let deadline = Instant::now() + Duration::from_secs(60);
        while ok(&db, &["status", "--role", "inbox"])["inbox_watcher"]["watching"] != 1 {
            assert!(Instant::now() < deadline, "the watch never watched");
            thread::sleep(Duration::from_millis(100));
        }
    }
    // The notices alone keep it waiting.
    thread::sleep(Duration::from_secs(2));
    assert!(
        child.child().try_wait().unwrap().is_none(),
        "woke on notices"
    );
    SqliteQueue::open(&db)
        .unwrap()
        .register_supervisor(&LeaseToken::new("first"), std::process::id(), 2, VERSION)
        .unwrap();
    let output = {
        let _waiting = common::within(common::STEP_LIMIT, "the watch to return on the health");
        child.wait_with_output().unwrap()
    };
    assert!(output.status.success());
    let woken: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(woken["supervisors_changed"], true, "{woken}");
    let got = kinds_and_ids(&woken);
    assert_eq!(
        got.iter().map(|(_, id)| *id).collect::<Vec<_>>(),
        notices,
        "{woken}"
    );
    assert_eq!(woken["cursor"], *notices.last().unwrap(), "{woken}");
}

/// The observer's second failure in a row is a notice that wakes the
/// inbox's watch, with what the inbox shows the person, and `events`
/// returns it; the first and the third are none, and `status` keeps none
/// of them (task 1574).
#[test]
fn the_second_failure_of_the_observer_wakes_the_inbox_once() {
    let (_dir, db) = queue();
    let cursor = ok(&db, &["status", "--role", "inbox"])["cursor"]
        .as_i64()
        .unwrap();
    let failure = |count: i64| {
        record(
            &db,
            EventKind::ObserveFinished,
            json!({
                "mode": "hourly",
                "outcome": "error",
                "consecutive_failures": count,
                "error": "failed to spawn the observer: Argument list too long (os error 7)",
                "exit_code": null,
                "dir": "/q/observer/1",
            }),
        )
    };
    failure(1);
    let after = cursor.to_string();
    // The first alone does not wake it.
    let slept = watch(&db, &["--role", "inbox", "--after", &after]);
    assert_eq!(slept["events"], json!([]), "{slept}");
    let second = failure(2);
    let woken = watch(&db, &["--role", "inbox", "--after", &after]);
    assert_eq!(
        kinds_and_ids(&woken),
        [("observe_finished".to_owned(), second)],
        "{woken}"
    );
    let notice = &woken["events"][0];
    assert_eq!(notice["next"], "check the failed observer", "{notice}");
    assert_eq!(notice["mode"], "hourly", "{notice}");
    assert_eq!(notice["outcome"], "error", "{notice}");
    assert_eq!(notice["consecutive_failures"], 2, "{notice}");
    assert_eq!(notice["dir"], "/q/observer/1", "{notice}");
    assert!(
        notice["reason"]
            .as_str()
            .unwrap()
            .contains("Argument list too long"),
        "{notice}"
    );
    // `events` returns it as the only attention; `--all` has the first
    // too, with no next.
    let events = ok(&db, &["events", "--after", &after]);
    assert_eq!(kinds_and_ids(&events), kinds_and_ids(&woken), "{events}");
    assert_eq!(events["events"][0]["next"], "check the failed observer");
    let all = ok(&db, &["events", "--all", "--after", &after]);
    let nexts = all["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["next"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        nexts,
        [Value::Null, json!("check the failed observer")],
        "{all}"
    );
    // The third does not wake it again.
    let after = second.to_string();
    failure(3);
    let slept = watch(&db, &["--role", "inbox", "--after", &after]);
    assert_eq!(slept["events"], json!([]), "{slept}");
    // `status` holds what is unsettled, and no notice is.
    let status = ok(&db, &["status", "--role", "inbox"]);
    assert!(!status.to_string().contains("observe_finished"), "{status}");
}
