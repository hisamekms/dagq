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
use rusqlite::{Connection, Transaction, TransactionBehavior};
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
    // Why it stopped, as the loop records it: the queue stayed busy.
    let failure = heartbeat.failure().unwrap();
    assert_eq!(failure.cause, "busy", "{failure:?}");
    assert!(failure.failures >= 1, "{failure:?}");
    assert!(failure.secs > 0.0, "{failure:?}");
    assert!(failure.error.contains("database is locked"), "{failure:?}");
    let payload = failure.payload(&LeaseToken::new("sv"));
    assert_eq!(payload["supervisor"], "sv", "{payload}");
    assert_eq!(payload["cause"], "busy", "{payload}");
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
    assert_eq!(heartbeat.failure().unwrap().cause, "registration_gone");
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
/// none is named once it held the write lock, whatever wrote: an immediate
/// one from its `BEGIN`, one that only read included, and a deferred one
/// once a write statement took the lock, through [`WriteTx::execute`] or
/// only through the transaction it derefs to, one that changed no row
/// included, committed or rolled back. A deferred one that only read is
/// not.
#[test]
fn a_write_tx_is_named_once_it_held_the_lock_and_not_when_it_only_read() {
    let _test = common::test();
    let holds = LongHolds::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(holds.clone()));
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t(x UNIQUE); INSERT INTO t VALUES (0)")
        .unwrap();
    let read = |tx: &WriteTx<'_>| {
        let _: i64 = tx
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
    };
    // A write through the transaction the WriteTx derefs to, as
    // `write_ledger_row_over` writes: it reports nothing to the WriteTx.
    let deref_write = |tx: &WriteTx<'_>, sql: &str| {
        let tx: &Transaction<'_> = tx;
        tx.execute(sql, []).unwrap()
    };
    let begin = |name, behavior| WriteTx::begin(&conn, name, behavior, Duration::ZERO).unwrap();

    let tx = begin("deferred_read", TransactionBehavior::Deferred);
    read(&tx);
    tx.commit().unwrap();
    let tx = begin("deferred_read_dropped", TransactionBehavior::Deferred);
    read(&tx);
    drop(tx);
    let tx = begin("immediate_read", TransactionBehavior::Immediate);
    read(&tx);
    drop(tx);
    let tx = begin("deferred_write", TransactionBehavior::Deferred);
    read(&tx);
    assert_eq!(tx.execute("INSERT INTO t VALUES (1),(2)", []).unwrap(), 2);
    tx.execute("INSERT INTO t VALUES (3)", []).unwrap();
    tx.commit().unwrap();
    let tx = begin("deferred_update_none", TransactionBehavior::Deferred);
    read(&tx);
    assert_eq!(deref_write(&tx, "UPDATE t SET x = 10 WHERE x = 9"), 0);
    tx.commit().unwrap();
    let tx = begin("deferred_delete_none", TransactionBehavior::Deferred);
    assert_eq!(deref_write(&tx, "DELETE FROM t WHERE x = 9"), 0);
    drop(tx);
    let tx = begin("deferred_insert_nothing", TransactionBehavior::Deferred);
    assert_eq!(
        deref_write(&tx, "INSERT INTO t VALUES (0) ON CONFLICT DO NOTHING"),
        0
    );
    tx.commit().unwrap();

    for name in ["deferred_read", "deferred_read_dropped"] {
        assert!(holds.named(name).is_empty(), "{name}: {:?}", holds.0);
    }
    for name in [
        "immediate_read",
        "deferred_write",
        "deferred_update_none",
        "deferred_delete_none",
        "deferred_insert_nothing",
    ] {
        assert_eq!(holds.named(name).len(), 1, "{name}: {:?}", holds.0);
    }
    let count: i64 = conn
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 4);
}

/// A deferred transaction whose lock a write through the transaction it
/// derefs to took (one that changed no row) and that held it past a
/// threshold before a write through [`WriteTx::execute`] is named, ended by
/// its commit or by a drop: the later write does not move its lock time
/// past the hold.
#[test]
fn a_deferred_hold_taken_through_the_deref_is_named_after_a_later_counted_write() {
    let _test = common::test();
    let holds = LongHolds::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(holds.clone()));
    let threshold = Duration::from_millis(100);
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t(x)").unwrap();
    let hold = |name| {
        let tx = WriteTx::begin(&conn, name, TransactionBehavior::Deferred, threshold).unwrap();
        let deref: &Transaction<'_> = &tx;
        assert_eq!(
            deref.execute("UPDATE t SET x = 1 WHERE x = 9", []).unwrap(),
            0
        );
        thread::sleep(threshold * 3);
        assert_eq!(tx.execute("DELETE FROM t WHERE x = 9", []).unwrap(), 0);
        tx
    };
    hold("committed").commit().unwrap();
    drop(hold("dropped"));
    for name in ["committed", "dropped"] {
        let held = holds.named(name);
        assert_eq!(held.len(), 1, "{name}: {:?}", holds.0);
        assert!(
            held[0] >= u64::try_from((threshold * 3).as_millis()).unwrap(),
            "{name}: {held:?}"
        );
    }
}

/// The `queue_write_held_long` events of the queue at `db`, as payloads.
fn held_long(db: &Path) -> Vec<serde_json::Value> {
    let conn = Connection::open(db).unwrap();
    let mut statement = conn
        .prepare("SELECT payload FROM run_events WHERE kind = 'queue_write_held_long' ORDER BY id")
        .unwrap();
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
        .collect()
}

/// A deferred transaction of a queue that changed no row through the
/// transaction it derefs to, past the threshold, is recorded as
/// `queue_write_held_long` with its name, hold, role and pid after its
/// commit and after its rollback; one that only read, and one within the
/// threshold, are not.
#[test]
fn a_queue_s_deferred_hold_with_no_row_changed_is_recorded_as_an_event() {
    let _test = common::test();
    let (_dir, db, _queue) = registered();
    let queue = SqliteQueue::open(&db)
        .unwrap()
        .with_write_hold_warn(Duration::ZERO);
    let deref_write = |tx: &WriteTx<'_>, sql: &str| {
        let tx: &Transaction<'_> = tx;
        tx.execute(sql, []).unwrap()
    };
    let tx = queue
        .write_tx("update_none", TransactionBehavior::Deferred)
        .unwrap();
    assert_eq!(
        deref_write(&tx, "UPDATE tasks SET title = title WHERE id = -1"),
        0
    );
    tx.commit().unwrap();
    let tx = queue
        .write_tx("delete_none", TransactionBehavior::Deferred)
        .unwrap();
    assert_eq!(deref_write(&tx, "DELETE FROM tasks WHERE id = -1"), 0);
    drop(tx);
    let tx = queue
        .write_tx("read_only", TransactionBehavior::Deferred)
        .unwrap();
    let _: i64 = tx
        .query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))
        .unwrap();
    tx.commit().unwrap();
    // The same write within the default threshold.
    let quiet = SqliteQueue::open(&db).unwrap();
    let tx = quiet
        .write_tx("within", TransactionBehavior::Deferred)
        .unwrap();
    deref_write(&tx, "DELETE FROM tasks WHERE id = -1");
    tx.commit().unwrap();

    let recorded = held_long(&db);
    let names: Vec<&str> = recorded
        .iter()
        .map(|payload| payload["transaction"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["update_none", "delete_none"]);
    for payload in &recorded {
        assert!(payload["held_ms"].is_u64(), "{payload}");
        assert!(payload["before_lock_ms"].is_u64(), "{payload}");
        assert_eq!(payload["role"], "user", "{payload}");
        assert_eq!(payload["pid"], std::process::id(), "{payload}");
    }
}

/// A long hold whose record fails (a connection with no queue's events)
/// warns of the failure and leaves the transaction's result as it was:
/// its commit returns and what it wrote stays.
#[test]
fn a_failed_record_of_a_long_hold_leaves_the_transaction_as_it_was() {
    let _test = common::test();
    let holds = LongHolds::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(holds.clone()));
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t(x)").unwrap();
    let tx = WriteTx::begin(
        &conn,
        "unrecorded",
        TransactionBehavior::Immediate,
        Duration::ZERO,
    )
    .unwrap()
    .recorded();
    tx.execute("INSERT INTO t VALUES (1)", []).unwrap();
    tx.commit().unwrap();
    assert_eq!(holds.named("unrecorded").len(), 1, "{:?}", holds.0);
    let count: i64 = conn
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

/// A supervisor that ended on a heartbeat a busy queue stopped is told
/// apart from any other end where the inbox reads: `watch` returns its
/// `supervisor_heartbeat_failed` with the cause, the failures and the
/// seconds and its `next`, and `status` gives its stale registration's
/// attention the same reason with `lease_lost`.
#[test]
fn status_and_watch_tell_a_supervisor_that_ended_on_a_busy_queue() {
    use crate::common::cli::ok;
    let _test = common::test();
    let (_dir, db, queue) = registered();
    // Stale: its last heartbeat is long past.
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE supervisors SET heartbeat_at=0", [])
        .unwrap();
    queue
        .record_queue_event(
            EventKind::SupervisorHeartbeatFailed,
            serde_json::json!({
                "supervisor": "sv", "cause": "busy", "failures": 4, "secs": 27.0,
                "error": "4 heartbeat writes failed over 28.1s since the last one written: database is locked",
            }),
        )
        .unwrap();
    let watched = ok(&db, &["watch", "--after", "0", "--timeout", "1"]);
    let told: Vec<&serde_json::Value> = watched["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "supervisor_heartbeat_failed")
        .collect();
    assert_eq!(told.len(), 1, "{watched:#}");
    assert_eq!(told[0]["supervisor"], "sv", "{watched:#}");
    assert_eq!(told[0]["cause"], "busy", "{watched:#}");
    assert_eq!(told[0]["failures"], 4, "{watched:#}");
    assert_eq!(told[0]["secs"], 27.0, "{watched:#}");
    assert_eq!(
        told[0]["next"], "report the supervisor's end",
        "{watched:#}"
    );
    let status = ok(&db, &["status"]);
    let stale: Vec<&serde_json::Value> = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["kind"] == "supervisor_stale")
        .collect();
    assert_eq!(stale.len(), 1, "{status:#}");
    let reason = stale[0]["last_error"].as_str().unwrap();
    assert!(
        reason.starts_with("heartbeat stopped (busy): 4 writes failed over 27.0s: "),
        "{reason}"
    );
    assert!(reason.contains("database is locked"), "{reason}");
    assert_eq!(stale[0]["last_error_code"], "lease_lost", "{status:#}");
}
