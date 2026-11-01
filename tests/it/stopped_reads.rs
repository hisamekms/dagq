//! The readers of every event of the queue, stopped until the ledger's
//! readers take their place (ADR-t3745-1, docs/design/measurement.md
//! "並走の比較と照合"): each entry, on the command line and on the queue
//! service's side, refuses before it reads, naming why and what brings it
//! back, and the supervisor's `all_events` is refused by the service.

use crate::common;
use crate::common::service::{call, queue, start};
use common::cli::refused;

use std::sync::Arc;

use dagq::{
    application::supervisor_service::ServiceCaller,
    domain::{LeaseToken, queue_service::UseCase},
    infrastructure::queue_service::SocketCaller,
};
use serde_json::json;

/// `text` is the refusal of `entry`: why it is stopped and what brings it
/// back (`restored`).
fn assert_stopped(text: &str, entry: &str, restored: &str) {
    assert!(text.contains(&format!("{entry} is stopped")), "{text}");
    assert!(
        text.contains("until the ledger's readers take its place"),
        "{text}"
    );
    assert!(text.contains(restored), "{text}");
}

/// Each stopped command exits non-zero with the refusal as `{"error"}`.
#[test]
fn each_stopped_command_is_refused_naming_why_and_what_brings_it_back() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    common::template::queue(&db);
    assert_stopped(
        &refused(&db, &["stats", "--ledger"]),
        "stats --ledger",
        "removed once stats reads the ledger",
    );
}

/// The queue service refuses each stopped read and command it is sent
/// directly, before it reads.
#[test]
fn the_service_refuses_each_stopped_read_it_is_sent() {
    let queue = queue();
    start(&queue);
    let response = call(&queue, None, UseCase::Stats, json!({"ledger": true}));
    assert_eq!(response["ok"], false, "{response}");
    assert_stopped(
        response["error"]["message"].as_str().unwrap(),
        "stats --ledger",
        "removed once stats reads the ledger",
    );
}

/// The supervisor's `all_events` is refused by the service, which answers
/// no event of the queue in its place.
#[test]
fn the_service_refuses_the_supervisors_all_events() {
    let queue = queue();
    let served = common::service::Served::start(&queue.db);
    let pid = 4244;
    let caller: Arc<dyn ServiceCaller> = Arc::new(SocketCaller::new(&queue.db));
    caller
        .sign_in(&LeaseToken::new("supervisor-of-all-events"), pid)
        .unwrap();
    let error = caller
        .call(UseCase::Supervisor, json!({"op": "all_events", "args": []}))
        .unwrap_err();
    assert_stopped(&format!("{error:#}"), "all_events", "read no whole log");
    caller.sign_out();
    drop(served);
}
