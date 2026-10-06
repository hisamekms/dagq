//! The inbox's watcher (ADR-t906-1): a `watch --role inbox` leaves its
//! record under the queue's directory, and `status` and `doctor` judge it
//! by the freshness of its heartbeat.

use crate::common::{self, cli::*};
use dagq::{
    application::{Clock, Generators},
    compose::OneShot,
    domain::SessionRole,
    infrastructure::clock::UuidGenerator,
};
use serde_json::Value;
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// The wall clock moved on by a fixed number of seconds.
struct Later(u64);

impl Clock for Later {
    fn system_time(&self) -> SystemTime {
        SystemTime::now() + Duration::from_secs(self.0)
    }

    fn monotonic(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
}

fn later(secs: u64) -> OneShot {
    OneShot::new(Generators {
        clock: Arc::new(Later(secs)),
        ids: Arc::new(UuidGenerator),
    })
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// `watch --role inbox` in the background.
fn spawn_watch(db: &Path, timeout: &str) -> Child {
    spawn_watch_with(db, &["--timeout", timeout], Stdio::null())
}

/// `watch --role inbox --interval 1` with `extra` in the background.
fn spawn_watch_with(db: &Path, extra: &[&str], stdout: Stdio) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    common::WithoutActor::without_actor_env(&mut command);
    command
        .arg("--db")
        .arg(db)
        .args(["watch", "--role", "inbox", "--interval", "1"])
        .args(extra)
        .stdout(stdout)
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// Opens a blocked ask, an attention for the inbox.
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

/// Polls `status --role inbox` until its watcher satisfies `done`.
fn watcher_until(db: &Path, what: &str, done: impl Fn(&Value) -> bool) -> Value {
    let _waiting = common::within(common::STEP_LIMIT, what.to_owned());
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let watcher = ok(db, &["status", "--role", "inbox"])["inbox_watcher"].clone();
        if done(&watcher) {
            return watcher;
        }
        assert!(Instant::now() < deadline, "{what}: {watcher}");
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn status_and_doctor_show_the_watcher_alive_while_it_watches_and_absent_after_the_grace() {
    let (_dir, db) = queue();
    let before = ok(&db, &["status", "--role", "inbox"])["inbox_watcher"].clone();
    assert_eq!(before["state"], "absent", "{before}");
    assert_eq!(before["watching"], 0);
    assert!(before["last_seen_at"].is_null(), "{before}");
    assert!(before["absent_secs"].is_null(), "{before}");
    assert_eq!(before["grace_secs"], 120);
    // The planner's status has none; a status for every role has it.
    assert!(ok(&db, &["status", "--role", "planner"])["inbox_watcher"].is_null());
    assert_eq!(ok(&db, &["status"])["inbox_watcher"]["state"], "absent");

    // A long timeout, so that a slow host does not end the watch before it
    // is seen; an ask (an attention for the inbox) ends it instead.
    let mut child = spawn_watch(&db, "120");
    let alive = watcher_until(&db, "the watch to be watching", |w| w["watching"] == 1);
    assert_eq!(alive["state"], "alive", "{alive}");
    assert!(alive["absent_secs"].is_null(), "{alive}");
    let doctor = ok(&db, &["doctor"])["inbox_watcher"].clone();
    assert_eq!(doctor["state"], "alive", "{doctor}");
    assert_eq!(doctor["watching"], 1, "{doctor}");
    open_ask(&db);
    {
        let _waiting = common::within(common::STEP_LIMIT, "the watch to return on the ask");
        assert!(child.wait().unwrap().success());
    }

    // Returned: no watch is running, and the grace keeps the inbox watched.
    let ended = ok(&db, &["status", "--role", "inbox"])["inbox_watcher"].clone();
    assert_eq!(ended["state"], "alive", "{ended}");
    assert_eq!(ended["watching"], 0, "{ended}");
    let seen = ended["last_seen_at"].as_i64().unwrap();
    assert!((now() - 60..=now()).contains(&seen), "{ended}");

    // Past the grace it is absent, with how long.
    let past = later(200);
    let status = past.status_for(&db, Some(SessionRole::Inbox)).unwrap();
    let watcher = &status["inbox_watcher"];
    assert_eq!(watcher["state"], "absent", "{watcher}");
    assert_eq!(watcher["last_seen_at"], seen);
    assert!(watcher["absent_secs"].as_i64().unwrap() >= 200, "{watcher}");
    let doctor = past.doctor(&db, false, None).unwrap();
    assert_eq!(doctor["inbox_watcher"]["state"], "absent");
    // The same but for `absent_secs`: `later` reads the wall clock on each
    // call, so doctor may be a second past status.
    let mut same = doctor["inbox_watcher"].clone();
    let absent = same["absent_secs"].take().as_i64().unwrap();
    let expected = watcher["absent_secs"].as_i64().unwrap();
    assert!((expected..=expected + 2).contains(&absent), "{doctor}");
    let mut watcher = watcher.clone();
    watcher["absent_secs"] = serde_json::Value::Null;
    assert_eq!(same, watcher);
}

#[test]
fn a_watch_until_attention_outlasts_empty_reads_and_returns_only_on_an_attention() {
    let (dir, db) = queue();
    let cursor = ok(&db, &["status", "--role", "inbox"])["cursor"].clone();
    let after = cursor.as_i64().unwrap().to_string();
    let mut child = common::KillOnDrop::new(
        spawn_watch_with(
            &db,
            &["--until-attention", "--after", &after],
            Stdio::piped(),
        ),
        "the watch until attention",
    );
    let alive = watcher_until(&db, "the watch to be watching", |w| w["watching"] == 1);
    assert_eq!(alive["state"], "alive", "{alive}");
    // Its record says it has no timeout.
    let records = dagq::infrastructure::inbox_watchers::read(&dir.path().join("inbox-watchers"));
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].timeout_secs, None);
    // Several intervals of nothing: it is still waiting and still watching.
    thread::sleep(Duration::from_secs(3));
    assert!(
        child.child().try_wait().unwrap().is_none(),
        "returned with nothing"
    );
    assert_eq!(
        ok(&db, &["status", "--role", "inbox"])["inbox_watcher"]["watching"],
        1
    );

    open_ask(&db);
    let output = {
        let _waiting = common::within(common::STEP_LIMIT, "the watch to return on the ask");
        child.wait_with_output().unwrap()
    };
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let events = value["events"].as_array().unwrap();
    assert_eq!(events.len(), 1, "{value}");
    assert_eq!(events[0]["kind"], "ask_opened", "{value}");
    assert_eq!(value["supervisors_changed"], false);
    assert!(value["supervisors"].is_array(), "{value}");
    assert!(value["cursor"].as_i64().unwrap() > cursor.as_i64().unwrap());
    let ended = dagq::infrastructure::inbox_watchers::read(&dir.path().join("inbox-watchers"));
    assert!(ended[0].ended_at.is_some(), "{ended:?}");
}

#[test]
fn a_watch_until_attention_refuses_a_timeout_and_a_queue_it_cannot_open() {
    let (_dir, db) = queue();
    let output = invoke(
        &db,
        &[
            "watch",
            "--until-attention",
            "--timeout",
            "5",
            "--role",
            "inbox",
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot be used with"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // No queue: it ends at once with the error, never waiting for one.
    let missing = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let output = invoke(
        &missing.path().join("queue.db"),
        &[
            "watch",
            "--until-attention",
            "--role",
            "inbox",
            "--interval",
            "1",
        ],
    );
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(error["error"].is_string(), "{error}");
    assert!(started.elapsed() < Duration::from_secs(30));
}

#[test]
fn a_running_watch_with_a_stale_heartbeat_is_absent() {
    let (_dir, db) = queue();
    let mut child = spawn_watch(&db, "60");
    let alive = watcher_until(&db, "the watch to be watching", |w| w["watching"] == 1);
    assert_eq!(alive["state"], "alive");
    // Its heartbeat renews every second, and 3 intervals plus 10 seconds
    // is the limit: 30 seconds on, the same process counts as absent.
    let status = later(30).status_for(&db, Some(SessionRole::Inbox)).unwrap();
    let watcher = &status["inbox_watcher"];
    assert_eq!(watcher["state"], "absent", "{watcher}");
    assert_eq!(watcher["watching"], 0, "{watcher}");
    assert!(watcher["absent_secs"].as_i64().unwrap() >= 17, "{watcher}");
    // The test's own child, by its handle.
    child.kill().unwrap();
    let _waiting = common::within(common::STEP_LIMIT, "the killed watch to exit");
    child.wait().unwrap();
}

#[test]
fn a_watch_killed_without_writing_its_end_is_not_watching_before_its_heartbeat_goes_stale() {
    let (dir, db) = queue();
    let mut child = spawn_watch(&db, "120");
    watcher_until(&db, "the watch to be watching", |w| w["watching"] == 1);
    // SIGKILL, the way Claude Code's /clear or KillShell stops it: no end
    // is written. The test's own child, by its handle, reaped at once.
    child.kill().unwrap();
    {
        let _waiting = common::within(common::STEP_LIMIT, "the killed watch to exit");
        child.wait().unwrap();
    }
    let records = dagq::infrastructure::inbox_watchers::read(&dir.path().join("inbox-watchers"));
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].ended_at, None, "{records:?}");
    let watcher = ok(&db, &["status", "--role", "inbox"])["inbox_watcher"].clone();
    let judged = now();
    // Its heartbeat is still inside the limit (unless a loaded host took
    // longer than the limit to get here, which the rest still holds for),
    // yet it is not watching, and with no grace the inbox is absent since
    // the last heartbeat.
    if judged - records[0].heartbeat_at > records[0].stale_after_secs() {
        eprintln!("the heartbeat went stale before the status: {records:?} at {judged}");
    }
    assert_eq!(watcher["watching"], 0, "{watcher}");
    assert_eq!(watcher["state"], "absent", "{watcher}");
    assert_eq!(
        watcher["last_seen_at"], records[0].heartbeat_at,
        "{watcher}"
    );
    assert_eq!(
        ok(&db, &["doctor"])["inbox_watcher"]["watching"],
        0,
        "the doctor judges the same"
    );
}
