//! The supervisor's heartbeat over a queue another connection locks: a
//! busy write is tried again, and only a lost registration or busy writes
//! that would let the leases go stale stop it (task 1119).
use crate::common::{self, queue::fixture};
use dagq::application::supervise::{Heartbeat, HeartbeatPolicy};
use dagq::application::{Queue, QueueOpener};
use dagq::domain::{EventKind, LeaseToken};
use dagq::infrastructure::sqlite::SqliteQueue;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// How long one heartbeat write waits for the test's lock.
const ATTEMPT: Duration = Duration::from_millis(100);

/// Opens the queue with a busy timeout of [`ATTEMPT`], so a locked write
/// fails in a tenth of a second instead of five.
struct ShortWait(PathBuf);

impl QueueOpener for ShortWait {
    fn open(&self) -> anyhow::Result<Box<dyn Queue + Send>> {
        let queue = SqliteQueue::open(&self.0)?;
        queue.set_busy_timeout(ATTEMPT)?;
        Ok(Box::new(queue))
    }
}

fn policy(stale_after: Duration) -> HeartbeatPolicy {
    HeartbeatPolicy {
        interval: Duration::from_millis(50),
        stale_after,
        attempt: ATTEMPT,
        registered: true,
    }
}

/// A queue with the supervisor `sv` registered.
fn registered() -> (tempfile::TempDir, PathBuf, SqliteQueue) {
    let (dir, mut queue) = fixture();
    queue
        .register_supervisor(&LeaseToken::new("sv"), std::process::id(), 1, "test")
        .unwrap();
    let db = dir.path().join("queue.db");
    (dir, db, queue)
}

fn start(db: &Path, policy: HeartbeatPolicy) -> Heartbeat {
    Heartbeat::start(
        Arc::new(ShortWait(db.to_path_buf())),
        LeaseToken::new("sv"),
        policy,
    )
}

/// Another connection holding the queue's write lock until it is dropped.
fn lock(db: &Path) -> Connection {
    let conn = Connection::open(db).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    conn
}

/// Wait for `done`, polling, up to `limit`.
fn until(limit: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let _waiting = common::within(limit + Duration::from_secs(5), what);
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_busy_heartbeat_is_tried_again_and_its_failures_are_recorded() {
    let _test = common::test();
    let (_dir, db, queue) = registered();
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE supervisors SET heartbeat_at=0", [])
        .unwrap();
    let heartbeat = start(&db, policy(Duration::from_secs(60)));
    // The heartbeat runs before the lock is taken.
    until(Duration::from_secs(20), "the first heartbeat", || {
        queue.supervisors().unwrap()[0].heartbeat_at > 0
    });
    let held = lock(&db);
    // Writes fail on the lock (each waits 100ms, 50ms apart); the
    // supervisor goes on.
    thread::sleep(Duration::from_millis(700));
    heartbeat.check().unwrap();
    drop(held);
    let mut retried = None;
    until(
        Duration::from_secs(20),
        "the heartbeat written again",
        || {
            retried = queue
                .latest_event_of(EventKind::SupervisorHeartbeatRetried.as_str())
                .unwrap();
            retried.is_some()
        },
    );
    heartbeat.check().unwrap();
    let payload = retried.unwrap().payload;
    assert_eq!(payload["token"], "sv", "{payload}");
    assert!(payload["failures"].as_u64().unwrap() >= 1, "{payload}");
    assert!(payload["secs"].as_f64().unwrap() > 0.0, "{payload}");
    assert!(
        payload["error"]
            .as_str()
            .unwrap()
            .contains("database is locked"),
        "{payload}"
    );
}

#[test]
fn busy_writes_that_would_let_the_leases_go_stale_stop_the_supervisor() {
    let _test = common::test();
    let (_dir, db, queue) = registered();
    // The leases would go stale a second after the last write: the next
    // write must start within 1s - 50ms - 100ms of it.
    let heartbeat = start(&db, policy(Duration::from_secs(1)));
    let held = lock(&db);
    until(Duration::from_secs(20), "the heartbeat to give up", || {
        heartbeat.check().is_err()
    });
    drop(held);
    let error = format!("{:#}", heartbeat.check().unwrap_err());
    assert!(error.contains("preserving runs for inspection"), "{error}");
    assert!(error.contains("heartbeat writes failed over"), "{error}");
    assert!(error.contains("database is locked"), "{error}");
    // Nothing was recorded as retried: the heartbeat stopped.
    assert!(
        queue
            .latest_event_of(EventKind::SupervisorHeartbeatRetried.as_str())
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_registration_gone_from_the_queue_stops_the_supervisor() {
    let _test = common::test();
    let (_dir, db, queue) = registered();
    let heartbeat = start(&db, policy(Duration::from_secs(60)));
    // A process with leases and no registration (`integrate`) goes on.
    let leases = Heartbeat::start(
        Arc::new(ShortWait(db.clone())),
        LeaseToken::new("integrate"),
        HeartbeatPolicy {
            registered: false,
            ..policy(Duration::from_secs(60))
        },
    );
    thread::sleep(Duration::from_millis(200));
    heartbeat.check().unwrap();
    assert!(queue.deregister_supervisor(&LeaseToken::new("sv")).unwrap());
    until(Duration::from_secs(20), "the heartbeat to stop", || {
        heartbeat.check().is_err()
    });
    let error = format!("{:#}", heartbeat.check().unwrap_err());
    assert!(error.contains("registration is gone"), "{error}");
    leases.check().unwrap();
}

#[test]
fn the_supervisor_policy_retries_until_the_leases_would_go_stale() {
    let policy = HeartbeatPolicy::supervisor(Duration::from_secs(2));
    assert_eq!(policy.stale_after, Duration::from_secs(30));
    assert_eq!(policy.attempt, Duration::from_secs(5));
    assert!(policy.retries(true, Duration::ZERO));
    assert!(policy.retries(true, Duration::from_secs(22)));
    assert!(!policy.retries(true, Duration::from_secs(23)));
    // A failure that does not pass is not tried again.
    assert!(!policy.retries(false, Duration::ZERO));
    assert!(!HeartbeatPolicy::leases(Duration::from_secs(2)).registered);
}
