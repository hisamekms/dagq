//! The supervisor's heartbeat over a queue another connection locks: a
//! busy write is tried again, and only a lost registration or busy writes
//! that would let the leases go stale stop it (task 1119). A write
//! transaction that held the lock past its threshold is named in a WARN.
use crate::common::{self, queue::fixture};
use dagq::application::supervise::{Heartbeat, HeartbeatPolicy};
use dagq::application::{NodeSampleStore, Queue, QueueOpener};
use dagq::domain::host_metrics::{HOST_NODE, HostSample};
use dagq::domain::{EventKind, LeaseToken};
use dagq::infrastructure::sqlite::{SqliteQueue, WriteTx};
use rusqlite::{Connection, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

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

/// Counts the heartbeat's WARNs that a write met the lock, which its
/// thread reports to the subscriber of the thread that started it: the
/// only sign a write was tried while the test held the lock.
#[derive(Clone, Default)]
struct BusyWrites(Arc<AtomicU64>);

impl BusyWrites {
    fn count(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl<S: tracing::Subscriber> Layer<S> for BusyWrites {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        if message.0.starts_with("supervisor heartbeat busy") {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// Wait for `done`, polling, up to `limit`.
fn until(limit: Duration, what: &str, done: impl FnMut() -> bool) {
    let _waiting = common::within(limit + Duration::from_secs(5), what);
    assert!(wait(limit, done), "timed out waiting for {what}");
}

/// Whether `done` held, polling, within `limit`.
fn wait(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while !done() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
    true
}

#[test]
fn a_busy_heartbeat_is_tried_again_and_its_failures_are_recorded() {
    let _test = common::test();
    let (_dir, db, queue) = registered();
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE supervisors SET heartbeat_at=0", [])
        .unwrap();
    let busy = BusyWrites::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(busy.clone()));
    // With one dispatcher registered, tracing asks the default of the
    // thread that first reaches a callsite; another test's heartbeat
    // thread, with no subscriber, meeting its own lock first would switch
    // the busy WARN off for good, and none would be counted here. A second
    // one makes every callsite ask both.
    let _second = tracing::Dispatch::new(tracing_subscriber::registry());
    tracing::callsite::rebuild_interest_cache();
    let heartbeat = start(&db, policy(Duration::from_secs(60)));
    // The heartbeat runs before the lock is taken.
    until(Duration::from_secs(20), "the first heartbeat", || {
        queue.supervisors().unwrap()[0].heartbeat_at > 0
    });
    let held = lock(&db);
    // Writes fail on the lock (each waits 100ms, 50ms apart) and the
    // supervisor goes on: the lock is held until two writes failed on it,
    // not for a fixed time a loaded host may let pass with none tried.
    let mut stopped = None;
    let waited = {
        let what = "two heartbeat writes to fail on the lock";
        let _waiting = common::within(Duration::from_secs(25), what);
        wait(Duration::from_secs(20), || {
            stopped = heartbeat.check().err().map(|error| format!("{error:#}"));
            stopped.is_some() || busy.count() >= 2
        })
    };
    // What the heartbeat did, read past the lock: a reader is not blocked.
    let heartbeat_at = queue.supervisors().unwrap()[0].heartbeat_at;
    assert!(
        waited && stopped.is_none(),
        "two heartbeat writes to fail on the lock: {} busy WARNs counted, \
         heartbeat_at {heartbeat_at}, heartbeat stopped: {stopped:?}",
        busy.count()
    );
    drop(held);
    let mut retried = None;
    until(
        Duration::from_secs(20),
        "the heartbeat written again",
        || {
            // A heartbeat that stopped records nothing more: say why.
            heartbeat.check().unwrap();
            retried = queue
                .latest_event_of(EventKind::SupervisorHeartbeatRetried.as_str())
                .unwrap();
            retried.is_some()
        },
    );
    heartbeat.check().unwrap();
    let payload = retried.unwrap().payload;
    assert_eq!(payload["token"], "sv", "{payload}");
    // No write went through while the lock was held: one streak has them all.
    assert!(payload["failures"].as_u64().unwrap() >= 2, "{payload}");
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
    // A subscriber of its own: a heartbeat thread with none, first at the
    // busy WARN while another test's is the only dispatcher registered,
    // makes tracing cache the WARN as unheard, and that test
    // (`a_busy_heartbeat_is_tried_again...`) would never count it.
    let _subscriber = tracing::subscriber::set_default(tracing_subscriber::registry());
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

/// The WARNs that a write transaction held the lock long, each as its
/// `transaction` and `held_ms`.
#[derive(Clone, Default)]
struct LongHolds(Arc<Mutex<Vec<(String, u64)>>>);

impl LongHolds {
    fn named(&self, name: &str) -> Vec<u64> {
        let holds = self.0.lock().unwrap();
        holds
            .iter()
            .filter(|(transaction, _)| transaction == name)
            .map(|(_, held)| *held)
            .collect()
    }
}

impl<S: tracing::Subscriber> Layer<S> for LongHolds {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        #[derive(Default)]
        struct Fields {
            message: String,
            transaction: String,
            held_ms: Option<u64>,
        }
        impl tracing::field::Visit for Fields {
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                if field.name() == "held_ms" {
                    self.held_ms = Some(value);
                }
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "transaction" {
                    self.transaction = value.to_owned();
                }
            }
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.message = format!("{value:?}");
                }
            }
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields.message == "queue write lock held long" {
            let held = fields.held_ms.expect("a long hold's WARN has held_ms");
            self.0.lock().unwrap().push((fields.transaction, held));
        }
    }
}

const IMPORT: &str = "node_samples_csv_import";

/// The samples of one CSV file, a second apart.
fn samples(count: i64) -> Vec<HostSample> {
    (0..count)
        .map(|n| HostSample::new(1_900_000_000 + n))
        .collect()
}

/// A CSV import that held the lock past the threshold (lowered to none, so
/// any import that wrote does) is named in a WARN with how long it held it,
/// once for each of its transactions.
#[test]
fn an_import_that_held_the_lock_past_the_threshold_is_named_in_a_warn() {
    let _test = common::test();
    let (_dir, db, _queue) = registered();
    let holds = LongHolds::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(holds.clone()));
    let queue = SqliteQueue::open(&db)
        .unwrap()
        .with_write_hold_warn(Duration::ZERO);
    let added = queue
        .import_node_samples(HOST_NODE, "metrics-x.csv", 1, &samples(500), 1)
        .unwrap();
    assert_eq!(added, 500);
    let batches = 500_usize.div_ceil(dagq::infrastructure::runtime_store::ROWS_PER_TRANSACTION);
    assert_eq!(holds.named(IMPORT).len(), batches, "{:?}", holds.0);
    // The same import within the threshold warns of nothing.
    let quiet = SqliteQueue::open(&db).unwrap();
    quiet
        .import_node_samples(HOST_NODE, "metrics-y.csv", 1, &samples(5), 1)
        .unwrap();
    assert_eq!(holds.named(IMPORT).len(), batches, "{:?}", holds.0);
}

/// An import that started while another connection held the lock waits in
/// its first write past the threshold, but holds the lock only from then:
/// it is not named as the holder.
#[test]
fn an_import_that_waited_for_the_lock_past_the_threshold_is_not_named() {
    let _test = common::test();
    let (_dir, db, _queue) = registered();
    let threshold = Duration::from_secs(1);
    let held = lock(&db);
    let holds = LongHolds::default();
    let (started, starts) = std::sync::mpsc::channel();
    let import = {
        let (db, holds) = (db.clone(), holds.clone());
        thread::spawn(move || {
            let _subscriber =
                tracing::subscriber::set_default(tracing_subscriber::registry().with(holds));
            let queue = SqliteQueue::open(&db)
                .unwrap()
                .with_write_hold_warn(threshold);
            started.send(()).unwrap();
            let began = Instant::now();
            queue
                .import_node_samples(HOST_NODE, "metrics-x.csv", 1, &samples(5), 1)
                .unwrap();
            began.elapsed()
        })
    };
    {
        let _waiting = common::within(common::STEP_LIMIT, "the import thread to start");
        starts.recv().unwrap();
    }
    thread::sleep(threshold + Duration::from_millis(500));
    drop(held);
    let took = {
        let _waiting = common::within(common::STEP_LIMIT, "the import thread to return");
        import.join().unwrap()
    };
    assert!(took > threshold, "the import waited only {took:?}");
    assert!(holds.named(IMPORT).is_empty(), "{:?}", holds.0);
}

/// Over a real connection and clock, a [`WriteTx`] past a threshold of
/// none is named once it ran a write statement, one that changed no row
/// included: a deferred one from its first write, an immediate one from
/// its `BEGIN`. One that only read is not, deferred or immediate, committed
/// or rolled back.
#[test]
fn a_write_tx_is_named_once_it_wrote_and_not_when_it_only_read() {
    let _test = common::test();
    let holds = LongHolds::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(holds.clone()));
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t(x)").unwrap();
    let read = |tx: &WriteTx<'_>| {
        let _: i64 = tx
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
    };
    let begin = |name, behavior| WriteTx::begin(&conn, name, behavior, Duration::ZERO).unwrap();

    let tx = begin("deferred_read", TransactionBehavior::Deferred);
    read(&tx);
    tx.commit().unwrap();
    let tx = begin("immediate_read", TransactionBehavior::Immediate);
    read(&tx);
    drop(tx);
    let tx = begin("deferred_write", TransactionBehavior::Deferred);
    read(&tx);
    assert_eq!(tx.execute("INSERT INTO t VALUES (1),(2)", []).unwrap(), 2);
    tx.execute("INSERT INTO t VALUES (3)", []).unwrap();
    tx.commit().unwrap();
    let tx = begin("immediate_unchanged", TransactionBehavior::Immediate);
    assert_eq!(tx.execute("DELETE FROM t WHERE x = 9", []).unwrap(), 0);
    drop(tx);

    for name in ["deferred_read", "immediate_read"] {
        assert!(holds.named(name).is_empty(), "{name}: {:?}", holds.0);
    }
    for name in ["deferred_write", "immediate_unchanged"] {
        assert_eq!(holds.named(name).len(), 1, "{name}: {:?}", holds.0);
    }
    let count: i64 = conn
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 3);
}
