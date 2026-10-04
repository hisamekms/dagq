//! The helpers of the tests of a supervisor that loses the lease of a run
//! it still drives in a slot (task 1361): the lease moved to another
//! token as an adopter's would, and what the supervisor wrote and kept
//! after it.
use super::*;

/// Move `run`'s lease to the token `taken`, stale, as a killed
/// supervisor's would look to an adopter, and with `session_ended` mark
/// the exit of its wrapper and agent too, as a resumed session that ended
/// unwatched leaves them: in one transaction, so that a supervisor's pass
/// sees both or neither. The id of the last event before it.
pub fn take_lease(db: &Path, run: &TaskRun, session_ended: bool) -> i64 {
    let mut connection = Connection::open(db).unwrap();
    connection.busy_timeout(Duration::from_secs(10)).unwrap();
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let last = transaction
        .query_row("SELECT COALESCE(MAX(id), 0) FROM run_events", [], |r| {
            r.get(0)
        })
        .unwrap();
    let moved = transaction
        .execute(
            "UPDATE run_leases SET token='taken', heartbeat_at=unixepoch()-31 WHERE run_id=?1",
            [&run.id()],
        )
        .unwrap();
    assert_eq!(moved, 1, "the lease of {}", run.id());
    if session_ended {
        let exited = transaction
            .execute(
                "UPDATE run_processes SET exited_at=unixepoch(), exit_code=0
                 WHERE run_id=?1 AND exited_at IS NULL",
                [&run.id()],
            )
            .unwrap();
        assert!(exited >= 1, "the live processes of {}", run.id());
    }
    transaction.commit().unwrap();
    last
}

/// The kinds of `run`'s events after the event `after`.
pub fn run_events_after(queue: &SqliteQueue, run: &TaskRun, after: i64) -> Vec<String> {
    queue
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.run_id.as_ref() == Some(run.id()) && event.id.as_i64() > after)
        .map(|event| event.kind)
        .collect()
}

/// The runs of the slots the supervisor on `db` drained with, once it
/// recorded `supervisor_draining` for its stop.
pub fn draining_runs(db: &Path) -> Value {
    let mut draining = None;
    wait_until(db, Duration::from_secs(20), |queue| {
        draining = queue
            .all_events()
            .unwrap()
            .into_iter()
            .find(|event| event.kind == "supervisor_draining");
        draining.is_some()
    });
    draining.unwrap().payload["runs"].clone()
}

/// A failed assert prints the queue's events and open asks, to read who did
/// what to the run and when.
pub struct EventsOnPanic(pub PathBuf);

impl Drop for EventsOnPanic {
    fn drop(&mut self) {
        if thread::panicking() {
            print_queue_events(&self.0);
        }
    }
}
