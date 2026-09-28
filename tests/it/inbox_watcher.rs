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
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    common::WithoutActor::without_actor_env(&mut command);
    command
        .arg("--db")
        .arg(db)
        .args([
            "watch",
            "--role",
            "inbox",
            "--timeout",
            timeout,
            "--interval",
            "1",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
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
        ],
    );
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
    assert_eq!(doctor["inbox_watcher"], *watcher);
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
