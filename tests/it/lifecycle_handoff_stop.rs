//! `lifecycle::hand_off` and a supervisor that drains for a stop request
//! (`supervisor_draining`, task 1277): it fails as stopping at once,
//! however the stop falls among the handoff's reads of the queue.

use crate::common;
use common::lifecycle::*;
use dagq::application::Clock;
use dagq::domain::{EventKind, LeaseToken, SupervisorMode};
use dagq::infrastructure::sqlite::SqliteQueue;
use dagq::lifecycle::Handed;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant, SystemTime},
};

/// Far longer than a test may take: only the stop ends the wait.
const TIMEOUT: Duration = Duration::from_secs(600);

/// The system's time, which also has the supervisor `old` record its stop
/// and deregister (its drain ended) at the `at`-th reading of the clock.
struct StopAt {
    db: PathBuf,
    at: usize,
    reads: AtomicUsize,
}

impl Clock for StopAt {
    fn system_time(&self) -> SystemTime {
        if self.reads.fetch_add(1, Ordering::SeqCst) + 1 == self.at {
            let queue = SqliteQueue::open(&self.db).unwrap();
            queue
                .record_queue_event(
                    EventKind::SupervisorDraining,
                    json!({"supervisor": "old", "reason": "stop_requested"}),
                )
                .unwrap();
            queue
                .deregister_supervisor(&LeaseToken::new("old"))
                .unwrap();
        }
        SystemTime::now()
    }
}

/// Hand `old` off while it stops and deregisters at the `at`-th reading of
/// the clock.
fn hand_off_stopping_at(at: usize) -> (Handed, Option<String>) {
    let fixture = fixture();
    let queue = handoff_supervisor(&fixture, "old", SupervisorMode::InCmux);
    let registration = queue.supervisors().unwrap().remove(0);
    let clock = StopAt {
        db: fixture.location.db.clone(),
        at,
        reads: AtomicUsize::new(0),
    };
    let started = Instant::now();
    let handed = dagq::lifecycle::hand_off(
        &queue,
        &FakeProcesses::default(),
        &clock,
        std::slice::from_ref(&registration),
        Path::new("/opt/bin/dagq"),
        dagq::VERSION,
        TIMEOUT,
        Duration::from_millis(20),
    )
    .unwrap();
    assert!(started.elapsed() < TIMEOUT);
    let request = queue.handoff_request(&LeaseToken::new("old")).unwrap();
    (handed.into_iter().next().unwrap(), request)
}

fn assert_stopping(handed: &Handed) {
    assert!(handed.stopping, "{handed:?}");
    assert_eq!(handed.now, None);
    let error = handed.error.as_deref().unwrap();
    assert!(error.contains("supervisor old (pid "), "{error}");
    assert!(
        error.contains("is stopping (a stop request wins over the handoff) and was not handed off to /opt/bin/dagq"),
        "{error}"
    );
    assert_eq!(handed.report()["stopping"], true);
}

/// The clock is read once when the handoff is asked for (before the
/// request is written), then in each look between its read of the stops
/// and its read of the registrations. A supervisor that recorded its stop
/// and deregistered right there, in the first look, is gone from the
/// registrations while the first read of the stops missed it: it still
/// fails as stopping, not as deregistered, and no request is left.
#[test]
fn a_supervisor_that_stops_and_deregisters_between_the_reads_fails_as_stopping() {
    let (handed, request) = hand_off_stopping_at(2);
    assert_stopping(&handed);
    assert_eq!(request, None);
    let error = handed.error.as_deref().unwrap();
    assert!(!error.contains("deregistered"), "{error}");
}

/// A supervisor still asked that recorded its stop fails as stopping at
/// once, its request withdrawn, without running to the timeout.
#[test]
fn a_supervisor_still_asked_that_stops_fails_as_stopping() {
    let fixture = fixture();
    let queue = handoff_supervisor(&fixture, "old", SupervisorMode::InCmux);
    let registration = queue.supervisors().unwrap().remove(0);
    queue
        .record_queue_event(
            EventKind::SupervisorDraining,
            json!({"supervisor": "old", "reason": "stop_requested"}),
        )
        .unwrap();
    let handed = dagq::lifecycle::hand_off(
        &queue,
        &FakeProcesses::default(),
        &dagq::infrastructure::clock::SystemClock,
        std::slice::from_ref(&registration),
        Path::new("/opt/bin/dagq"),
        dagq::VERSION,
        TIMEOUT,
        Duration::from_millis(20),
    )
    .unwrap();
    assert_stopping(&handed[0]);
    assert_eq!(
        queue.handoff_request(&LeaseToken::new("old")).unwrap(),
        None
    );
    // Still registered: it drains on under its old binary.
    assert_eq!(queue.supervisors().unwrap().len(), 1);
}
