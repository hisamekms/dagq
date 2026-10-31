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

/// Each stopped command exits non-zero with the refusal as `{"error"}`,
/// and records nothing: `mark` and `mark --retract` write no mark.
#[test]
fn each_stopped_command_is_refused_naming_why_and_what_brings_it_back() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    common::template::queue(&db);
    for (args, entry, restored) in [
        (
            &["stats"][..],
            "stats",
            "the ledger's reader of stats, kpi and report",
        ),
        (
            &["stats", "--full"],
            "stats",
            "the ledger's reader of stats, kpi and report",
        ),
        (
            &["stats", "--ledger"],
            "stats --ledger",
            "removed once stats reads the ledger",
        ),
        (
            &["kpi"],
            "kpi",
            "the ledger's reader of stats, kpi and report",
        ),
        (
            &["kpi", "--compare", "1"],
            "kpi",
            "the ledger's reader of stats, kpi and report",
        ),
        (
            &["report"],
            "report",
            "the ledger's reader of stats, kpi and report",
        ),
        (
            &["report", "--print", "json"],
            "report",
            "the ledger's reader of stats, kpi and report",
        ),
        (&["marks"], "marks", "a read of the marks"),
        (&["mark", "parallel 3→2"], "mark", "a read of the marks"),
        (&["mark", "--retract", "1"], "mark", "a read of the marks"),
    ] {
        assert_stopped(&refused(&db, args), entry, restored);
    }
    let marks = common::cli::ok(
        &db,
        &[
            "events",
            "--all",
            "--kind",
            "mark_recorded",
            "--kind",
            "mark_retracted",
        ],
    );
    assert_eq!(marks["events"], json!([]), "{marks}");
}

/// The queue service refuses each stopped read and command it is sent
/// directly, `stats_inputs` and `report_inputs` too, before it reads.
#[test]
fn the_service_refuses_each_stopped_read_it_is_sent() {
    let queue = queue();
    start(&queue);
    for (use_case, params, entry, restored) in [
        (
            UseCase::Stats,
            json!({}),
            "stats",
            "the ledger's reader of stats, kpi and report",
        ),
        (
            UseCase::Stats,
            json!({"ledger": true}),
            "stats --ledger",
            "removed once stats reads the ledger",
        ),
        (
            UseCase::StatsInputs,
            json!({}),
            "stats_inputs",
            "the ledger's reader of stats, kpi and report",
        ),
        (
            UseCase::Kpi,
            json!({}),
            "kpi",
            "the ledger's reader of stats, kpi and report",
        ),
        (
            UseCase::ReportInputs,
            json!({"period": "day"}),
            "report",
            "the ledger's reader of stats, kpi and report",
        ),
        (
            UseCase::Report,
            json!({"period": "day"}),
            "report",
            "the ledger's reader of stats, kpi and report",
        ),
        (UseCase::Marks, json!({}), "marks", "a read of the marks"),
        (
            UseCase::Mark,
            json!({"label": "x"}),
            "mark",
            "a read of the marks",
        ),
    ] {
        let response = call(&queue, None, use_case, params.clone());
        assert_eq!(response["ok"], false, "{use_case:?} {params}: {response}");
        assert_stopped(
            response["error"]["message"].as_str().unwrap(),
            entry,
            restored,
        );
    }
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
