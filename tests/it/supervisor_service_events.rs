//! The queue's events the supervisor reads through the queue service
//! (ADR-t3102-3 decisions 3 to 5, docs/design/queue-service.md): the read
//! answers as SQLite gives it, while the test refuses the queue to its own
//! process ([`refuse_opening`]).

use crate::common;
use crate::runtime_support::fixture;

use std::sync::Arc;

use dagq::{
    application::{
        QueueOpener,
        supervisor_service::{ServiceCaller, SupervisorQueues},
    },
    domain::{ActorContext, ActorRole, LeaseToken},
    infrastructure::{queue_service::SocketCaller, sqlite::refuse_opening},
};

/// The queue's earliest event is the least `created_at` that reads as a
/// time, not the smallest id's, read by SQLite from that column alone and
/// answered the supervisor through the service as the least of every
/// event's time; an empty queue has none.
#[test]
fn the_earliest_event_is_the_least_time_served_without_reading_every_event() {
    use dagq::application::EventStore;
    use dagq::domain::stats::timestamp_millis;
    use dagq::infrastructure::sqlite::SqliteQueue;
    let empty = tempfile::tempdir().unwrap();
    let none = SqliteQueue::init(empty.path().join("q.db")).unwrap();
    assert_eq!(EventStore::earliest_event_at(&none).unwrap(), None);

    let (_fixture, _repo, db) = fixture();
    let conn = rusqlite::Connection::open(&db).unwrap();
    // The newest event's time is set before every other's, and the oldest
    // event's to a date before it that reads as no time of the day.
    conn.execute(
        "UPDATE run_events SET created_at='2000-01-02T03:04:05.678Z'
         WHERE id=(SELECT max(id) FROM run_events)",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE run_events SET created_at='1999-01-01'
         WHERE id=(SELECT min(id) FROM run_events)",
        [],
    )
    .unwrap();
    let least = SqliteQueue::open(&db)
        .unwrap()
        .all_events()
        .unwrap()
        .iter()
        .filter_map(|event| timestamp_millis(&event.created_at))
        .min();
    assert_eq!(least, timestamp_millis("2000-01-02T03:04:05.678Z"));
    let direct = SqliteQueue::open(&db).unwrap();
    assert_eq!(EventStore::earliest_event_at(&direct).unwrap(), least);

    let _served = common::service::Served::start(&db);
    let pid = std::process::id();
    let caller: Arc<dyn ServiceCaller> = Arc::new(SocketCaller::new(&db));
    let queues = SupervisorQueues::new(
        caller.clone(),
        ActorContext::instance(ActorRole::Supervisor, pid),
    );
    caller
        .sign_in(&LeaseToken::new("supervisor-earliest"), pid)
        .unwrap();
    let queue = queues.open().unwrap();
    let refused = refuse_opening(&db);
    assert_eq!(EventStore::earliest_event_at(&*queue).unwrap(), least);
    drop(refused);
    caller.sign_out();
}
