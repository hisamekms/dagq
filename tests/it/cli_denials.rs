//! What the host's `dagq` refuses itself, before a command the queue
//! service does not answer, is sent to the service as the refused
//! caller's principal and recorded there as its own `authorization_denied`
//! (ADR-t3102-1 decision 1, ADR-t3102-2 decision 1): the CLI never opens
//! the queue for it. A caller the service does not authenticate is
//! recorded as unauthenticated, a refusal named for another role is
//! refused, and with no service that answers the refusal stays
//! unrecorded and is still a refusal (ADR-t3102-3 decision 2).

use crate::common;
use common::cli::invoke_with;
use common::service::{call, events, queue, start, token};
use dagq::domain::queue_service::{CREDENTIAL_FILE_ENV, Principal, UseCase};
use dagq::infrastructure::queue_service as service;
use serde_json::{Value, json};

/// The error JSON a refused command printed.
fn refusal(output: &std::process::Output) -> Value {
    assert!(!output.status.success(), "allowed");
    serde_json::from_slice(&output.stderr).unwrap()
}

/// The reasons and use cases of the queue's
/// `queue_service_unauthenticated`, in order.
fn unauthenticated(db: &std::path::Path) -> Vec<(Value, Value)> {
    events(db, "queue_service_unauthenticated")
        .iter()
        .map(|event| {
            (
                event["payload"]["use_case"].clone(),
                event["payload"]["reason"].clone(),
            )
        })
        .collect()
}

/// Every event of the queue, read by the test as a library.
fn event_count(db: &std::path::Path) -> usize {
    dagq::infrastructure::sqlite::SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .len()
}

/// Acceptance (2) and (3): a planner's and a worker's refusals on the host
/// are recorded by the service as theirs, the planner's actor and the
/// worker's run named; an actor named with no token is recorded as
/// unauthenticated, and nothing as `authorization_denied` for it.
#[test]
fn a_refusal_is_recorded_by_the_service_as_the_refused_principal() {
    let queue = queue();
    start(&queue);
    let task = queue.run.task_id().to_string();
    let planner = open_planner(&queue.db);
    let planner_file = service::issue(queue.dir(), &Principal::planner(planner), 1)
        .unwrap()
        .file;
    let planner_actor = format!("planner:{planner}");
    let as_planner = [
        ("DAGQ_ROLE", "planner"),
        ("DAGQ_ACTOR_ID", planner_actor.as_str()),
        (CREDENTIAL_FILE_ENV, planner_file.to_str().unwrap()),
    ];
    let refused = refusal(&invoke_with(&as_planner, &queue.db, &["integrate", &task]));
    assert_eq!(refused["denied"]["capability"], "landing.request");
    let worker_file = service::issue(
        queue.dir(),
        &Principal::worker(queue.run.id(), queue.run.task_id()),
        1,
    )
    .unwrap()
    .file;
    let as_worker = [
        ("DAGQ_ROLE", "worker"),
        ("DAGQ_ACTOR_ID", "worker:named"),
        (CREDENTIAL_FILE_ENV, worker_file.to_str().unwrap()),
    ];
    let refused = refusal(&invoke_with(&as_worker, &queue.db, &["up"]));
    assert_eq!(refused["denied"]["capability"], "service.lifecycle");

    let denied = events(&queue.db, "authorization_denied");
    let recorded: Vec<(Value, Value, Value)> = denied
        .iter()
        .map(|event| {
            (
                event["actor"]["role"].clone(),
                event["actor"]["id"].clone(),
                event["payload"]["capability"].clone(),
            )
        })
        .collect();
    assert_eq!(
        recorded,
        [
            (
                json!("planner"),
                json!(planner_actor),
                json!("landing.request")
            ),
            (
                json!("worker"),
                json!(format!("worker:{}", queue.run.id())),
                json!("service.lifecycle")
            ),
        ]
    );
    assert_eq!(
        denied[0]["payload"]["resource"],
        json!({"kind": "task", "id": queue.run.task_id(), "status": null})
    );
    assert_eq!(denied[0]["payload"]["reason"], "not granted");

    // An observer is named with no token: the service records the
    // request as unauthenticated, and no refusal as the observer's.
    let refused = refusal(&invoke_with(
        &[("DAGQ_ROLE", "observer")],
        &queue.db,
        &["down"],
    ));
    assert_eq!(refused["error"], "observer may not change queue state");
    assert_eq!(
        unauthenticated(&queue.db),
        [(json!("denial_record"), json!("missing_token"))]
    );
    assert_eq!(events(&queue.db, "authorization_denied").len(), 2);
}

/// Acceptance (3): the service records a refusal only as the caller's
/// own: one named for another role is refused as `authorization_denied`
/// (recorded as the caller's), and one the policy would not make is not
/// recorded.
#[test]
fn a_refusal_of_another_role_is_refused_and_recorded_as_the_caller_s() {
    let queue = queue();
    start(&queue);
    let worker = token(
        &queue,
        &Principal::worker(queue.run.id(), queue.run.task_id()),
    );
    let denial = |role: &str, capability: &str| {
        json!({"event": "authorization_denied", "role": role, "capability": capability,
               "reason": "not granted", "resource": {"kind": "queue"}})
    };
    let crossed = call(
        &queue,
        Some(&worker),
        UseCase::DenialRecord,
        denial("observer", "queue.watch"),
    );
    assert_eq!(
        crossed["error"]["code"], "authorization_denied",
        "{crossed}"
    );
    let denied = events(&queue.db, "authorization_denied");
    assert_eq!(denied.len(), 1);
    assert_eq!(denied[0]["actor"]["role"], "worker");
    assert_eq!(denied[0]["payload"]["role"], "worker");
    assert_eq!(denied[0]["payload"]["resource"]["kind"], "unresolved");

    let granted = call(
        &queue,
        Some(&worker),
        UseCase::DenialRecord,
        denial("worker", "queue.read"),
    );
    assert_eq!(granted["error"]["code"], "bad_request", "{granted}");
    let own = call(
        &queue,
        Some(&worker),
        UseCase::DenialRecord,
        denial("worker", "queue.watch"),
    );
    assert_eq!(own["ok"], true, "{own}");
    let denied = events(&queue.db, "authorization_denied");
    assert_eq!(denied.len(), 2);
    assert_eq!(denied[1]["payload"]["capability"], "queue.watch");
    assert_eq!(
        denied[1]["actor"]["id"],
        format!("worker:{}", queue.run.id())
    );
}

/// Acceptance (4): with the service stopped, a refusal is still returned,
/// and nothing is written to the queue in its place.
#[test]
fn a_refusal_with_no_service_is_returned_and_left_unrecorded() {
    let queue = queue();
    start(&queue);
    let planner = open_planner(&queue.db);
    let file = service::issue(queue.dir(), &Principal::planner(planner), 1)
        .unwrap()
        .file;
    assert!(
        common::cli::invoke(&queue.db, &["service", "stop"])
            .status
            .success()
    );
    let before = event_count(&queue.db);
    let actor = format!("planner:{planner}");
    let refused = refusal(&invoke_with(
        &[
            ("DAGQ_ROLE", "planner"),
            ("DAGQ_ACTOR_ID", actor.as_str()),
            (CREDENTIAL_FILE_ENV, file.to_str().unwrap()),
        ],
        &queue.db,
        &["supervise", "--once"],
    ));
    assert_eq!(refused["denied"]["capability"], "scheduler.supervise");
    assert_eq!(event_count(&queue.db), before);
}

/// A planner of the runtime's opened on the queue at `db` for the test.
fn open_planner(db: &std::path::Path) -> i64 {
    dagq::infrastructure::sqlite::SqliteQueue::open(db)
        .unwrap()
        .open_planner(dagq::domain::PlannerOrigin::Runtime, None)
        .unwrap()
        .id
        .as_i64()
}
