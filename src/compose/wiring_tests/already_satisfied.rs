//! A worker's `already_satisfied` receipt (ADR-t1818-1) wired from the
//! claim to the person's ask: the claim is validated without a commit, the
//! task's verification runs on a snapshot of main's head, the review judges
//! the claim on it, and the supervisor asks to close.

use serde_json::json;

use super::{
    agent::{Script, already_satisfied, verdict},
    harness::Wiring,
};
use crate::{
    application::TaskReads,
    domain::{AskKind, RunStatus, TaskDetail},
};

/// The payloads of `detail`'s events of `kind`, oldest first.
fn payloads<'a>(detail: &'a TaskDetail, kind: &str) -> Vec<&'a serde_json::Value> {
    detail
        .events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| &event.payload)
        .collect()
}

/// A ready task whose worker claims its base meets it, its two commands
/// passing and its review passing: the supervisor claims it, starts its
/// worker's session, validates the receipt as `already_satisfied`, runs
/// both commands on a snapshot of main's head, has the review judge the
/// claim there and asks a person to close it, the run left validating
/// without a lease.
#[test]
fn a_satisfied_claim_is_verified_on_main_and_asked_to_close() {
    let wiring = Wiring::new(
        "a_satisfied_claim_is_verified_on_main_and_asked_to_close",
        Script {
            receipts: vec![already_satisfied],
            verdicts: vec![verdict("pass", "fine")],
        },
    );
    let task = wiring.ready_task(&["test -f seed.txt", "test ! -f broken.txt"]);
    wiring.supervise_once();

    let head = wiring.git.main();
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail
        .events
        .iter()
        .map(|event| event.kind.as_str())
        .collect();
    assert_eq!(detail.runs.len(), 1, "{kinds:?}");
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Validating, "{kinds:?}");
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert_eq!(payloads(&detail, "run_claimed").len(), 1, "{kinds:?}");
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 1, "{kinds:?}");
    assert_eq!(validated[0]["already_satisfied"], true);
    assert_eq!(validated[0]["status"], "validating");
    assert_eq!(
        payloads(&detail, "run_satisfied_snapshot"),
        [&json!({"head": head})]
    );
    assert_eq!(
        payloads(&detail, "run_satisfied_verified"),
        [&json!({"head": head, "passed": true, "commands": 2})]
    );
    // The verification ran in the run's scratch checkout of main's head.
    let ran = wiring.verifier.ran();
    assert_eq!(
        ran.iter()
            .map(|(command, _)| command.as_str())
            .collect::<Vec<_>>(),
        ["test -f seed.txt", "test ! -f broken.txt"]
    );
    let checked_out = wiring.git.checked_out();
    assert!(
        !checked_out.is_empty() && checked_out.iter().all(|(_, commit)| *commit == head),
        "{checked_out:?}"
    );
    assert!(
        ran.iter().all(|(_, cwd)| *cwd == checked_out[0].0),
        "{ran:?} {checked_out:?}"
    );
    // One turn of the worker, whose receipt was observed, and one job of the
    // review, whose pass is not `high` and so asks.
    assert_eq!(
        wiring.agent.started(),
        ["wiring-worker-turn", "wiring-review"]
    );
    assert_eq!(payloads(&detail, "receipt_observed").len(), 1, "{kinds:?}");
    assert_eq!(
        payloads(&detail, "run_satisfied_review_job_started").len(),
        1,
        "{kinds:?}"
    );
    let judged = payloads(&detail, "run_satisfied_review_job_finished");
    assert_eq!(judged.len(), 1, "{kinds:?}");
    assert_eq!(judged[0]["verdict"], "pass");
    let judged = payloads(&detail, "run_satisfied_judged");
    assert_eq!(judged.len(), 1, "{kinds:?}");
    assert_eq!(judged[0]["step"], "ask");
    assert_eq!(judged[0]["head"], json!(head));
    // The wrapper was asked to exit, and its session closed.
    assert_eq!(payloads(&detail, "session_closed").len(), 1, "{kinds:?}");
    assert_eq!(payloads(&detail, "lease_released").len(), 1, "{kinds:?}");

    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::ApproveClose);
    assert_eq!(asks[0].options, ["close", "retry", "cancel"]);
    assert!(
        asks[0].question.contains(head.as_str()),
        "{}",
        asks[0].question
    );
}
