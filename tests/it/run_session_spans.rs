//! The spans of a run's own sessions, written by SQLite with the events that
//! open them: a revise's carries its request's attempt, and a withdrawal
//! reopens the last request standing. Which attempt that is is
//! `domain::sessions::revise_attempt`'s unit test.
use crate::common::{self, cli::ok};

use dagq::{
    application::TaskReads,
    domain::{ClaimOutcome, CommitSha, EventKind, LeaseToken, TaskId},
    infrastructure::sqlite::SqliteQueue,
};
use serde_json::{Value, json};

/// After revise 1, revise 2 withdrawn, revise 3 and revise 4 withdrawn, the
/// span reopened is revise 3's, not the 2 of the requests standing.
#[test]
fn a_withdrawn_revise_reopens_the_attempt_of_the_last_request_standing() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    common::template::queue(&db);
    ok(&db, &["add", "revised"]);
    ok(&db, &["ready", "1", "--bypass-review"]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&base, &LeaseToken::new("t"))
        .unwrap()
    else {
        panic!("nothing to claim");
    };
    for (kind, payload) in [
        (
            EventKind::AgentStarted,
            json!({"session_id": run.id().as_str()}),
        ),
        (EventKind::ReviseRequested, json!({"attempt": 1})),
        (EventKind::ReviseRequested, json!({"attempt": 2})),
        (
            EventKind::ReviseUnsent,
            json!({"attempt": 2, "error": "no session"}),
        ),
        (EventKind::ReviseRequested, json!({"attempt": 3})),
        (EventKind::ReviseRequested, json!({"attempt": 4})),
        (
            EventKind::ReviseUnsent,
            json!({"attempt": 4, "error": "no session"}),
        ),
    ] {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
    let opened: Vec<(Value, Value)> = queue
        .show(TaskId::new(1))
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.kind.as_str() == "session_opened")
        .map(|e| (e.payload["kind"].clone(), e.payload["attempt"].clone()))
        .collect();
    let span = |kind: &str, attempt: i64| (json!(kind), json!(attempt));
    assert_eq!(
        opened,
        [
            span("worker", 1),
            span("revise", 1),
            span("revise", 2),
            span("revise", 1),
            span("revise", 3),
            span("revise", 4),
            span("revise", 3),
        ]
    );
}
