//! The runtime operations are authorized in the application before they
//! run (ADR-t728-1 decision 5, task 734): workers, the headless jobs and
//! the observer land, recover and run the service not at all, a worker
//! runs and records its own run's session only, and the user, the inbox
//! and the planner keep the operations they run today.

use crate::common;

use common::cli::*;

use serde_json::Value;
use std::path::Path;

const WORKER: [(&str, &str); 4] = [
    ("DAGQ_ROLE", "worker"),
    ("DAGQ_ACTOR_ID", "worker:r1"),
    ("DAGQ_RUN_ID", "r1"),
    ("DAGQ_TASK_ID", "1"),
];

/// The error JSON `args` is refused with, as `env`.
fn denied_as(env: &[(&str, &str)], db: &Path, args: &[&str]) -> Value {
    let output = invoke_with(env, db, args);
    assert!(!output.status.success(), "{env:?} {args:?} was allowed");
    serde_json::from_slice(&output.stderr).unwrap()
}

/// The error JSON of `args` as `env`, which the authorizer let through.
fn failed_past_authorization(env: &[(&str, &str)], db: &Path, args: &[&str]) -> Value {
    let output = invoke_with(env, db, args);
    let error: Value = serde_json::from_slice(&output.stderr).unwrap_or(Value::Null);
    assert!(error.get("denied").is_none(), "{env:?} {args:?}: {error}");
    error
}

fn denials(db: &Path) -> Vec<Value> {
    ok(
        db,
        &[
            "events",
            "--after",
            "0",
            "--all",
            "--full",
            "--kind",
            "authorization_denied",
        ],
    )["events"]
        .as_array()
        .unwrap()
        .clone()
}

/// What the untrusted actors may not run, and the capability each needs.
const OPERATIONS: [(&[&str], &str); 7] = [
    (&["integrate", "1"], "landing.request"),
    (&["integrate", "--next"], "landing.request"),
    (&["recover", "r1"], "run.recover"),
    (&["install", "--from", "/nonexistent"], "service.install"),
    (&["up"], "service.lifecycle"),
    (&["down"], "service.lifecycle"),
    (&["migrate"], "queue.admin"),
];

#[test]
fn workers_jobs_and_the_observer_land_recover_and_run_the_service_not_at_all() {
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    let before = ok(&db, &["show", "1"])["task"].clone();
    let mut actors: Vec<Vec<(&str, &str)>> = vec![WORKER.to_vec()];
    for role in [
        "review-job",
        "recovery-job",
        "plan-review-job",
        "goal-review-job",
        "throughput-review-job",
        "observer",
    ] {
        actors.push(vec![("DAGQ_ROLE", role)]);
    }
    let mut refused = 0;
    for env in &actors {
        for (args, capability) in OPERATIONS {
            let error = denied_as(env, &db, args);
            assert_eq!(
                error["denied"]["capability"], capability,
                "{env:?} {args:?}"
            );
            assert_eq!(error["denied"]["reason"], "not granted", "{env:?} {args:?}");
            refused += 1;
        }
    }
    let error = denied_as(&WORKER, &db, &["integrate", "1"]);
    assert_eq!(
        error["error"],
        "worker may not landing.request (not granted)"
    );
    let error = denied_as(&[("DAGQ_ROLE", "observer")], &db, &["up"]);
    assert_eq!(error["error"], "observer may not change queue state");
    let error = denied_as(&[("DAGQ_ROLE", "review-job")], &db, &["down"]);
    assert_eq!(error["error"], "reviewer may not change queue state");
    refused += 3;

    // Each refusal is on the queue, as the refused actor, before the
    // operation did anything: the task is as it was and no supervisor
    // came up.
    let recorded = denials(&db);
    assert_eq!(recorded.len(), refused);
    assert_eq!(recorded[0]["actor"]["role"], "worker");
    assert_eq!(recorded[0]["actor"]["id"], "worker:r1");
    assert_eq!(recorded[0]["payload"]["capability"], "landing.request");
    assert_eq!(
        recorded[0]["payload"]["resource"],
        serde_json::json!({"kind": "task", "id": 1, "status": null})
    );
    assert_eq!(ok(&db, &["show", "1"])["task"], before);
    assert!(
        ok(&db, &["status"])["supervisors"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_worker_runs_and_records_the_session_of_its_own_run_only() {
    let (_dir, db) = queue();
    for args in [
        &["session-event", "open", "--run", "r2"][..],
        &[
            "session", "--run", "r2", "--lease", "t", "--claude", "claude",
        ],
        &[
            "session",
            "--run",
            "not a run",
            "--lease",
            "t",
            "--claude",
            "claude",
        ],
    ] {
        let error = denied_as(&WORKER, &db, args);
        assert_eq!(
            error["error"],
            format!(
                "worker may not {} (not on this resource)",
                error["denied"]["capability"].as_str().unwrap()
            ),
            "{args:?}"
        );
    }
    // A worker that names its session an inbox's would record the inbox's
    // span: the queue's, not its run's.
    let mut as_inbox = WORKER.to_vec();
    as_inbox.push(("DAGQ_SESSION_KIND", "inbox"));
    let error = denied_as(&as_inbox, &db, &["session-event", "open", "--run", "r1"]);
    assert_eq!(error["denied"]["capability"], "session.record");
    // Nor a planner's wrapper.
    let error = denied_as(
        &WORKER,
        &db,
        &["planner-session", "--planner", "1", "--claude", "claude"],
    );
    assert_eq!(error["denied"]["capability"], "session.run");
    let recorded = denials(&db);
    assert_eq!(recorded.len(), 5);
    assert_eq!(
        recorded[0]["payload"]["resource"],
        serde_json::json!({"kind": "run", "id": "r2", "task": null})
    );
    assert_eq!(recorded[0]["actor"]["id"], "worker:r1");

    // Its own run's passes the authorizer (and then wants the hook's input
    // or a run the queue has).
    failed_past_authorization(&WORKER, &db, &["session-event", "open"]);
    failed_past_authorization(&WORKER, &db, &["session-event", "open", "--run", "r1"]);
    failed_past_authorization(
        &WORKER,
        &db,
        &[
            "session", "--run", "r1", "--lease", "t", "--claude", "claude",
        ],
    );
    assert_eq!(denials(&db).len(), 5);
}

#[test]
fn the_user_the_inbox_and_the_planner_keep_their_operations() {
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    let planner = [("DAGQ_ROLE", "planner"), ("DAGQ_ACTOR_ID", "planner:3")];
    let inbox = [("DAGQ_ROLE", "inbox")];
    // `down` with no supervisor and `migrate --check` succeed.
    for env in [&[][..], &inbox[..], &planner[..]] {
        let output = invoke_with(env, &db, &["down"]);
        assert!(
            output.status.success(),
            "{env:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = invoke_with(env, &db, &["migrate", "--check"]);
        assert!(output.status.success(), "{env:?}");
        // `install` is theirs at a person's word; this one has no binary.
        failed_past_authorization(env, &db, &["install", "--from", "/nonexistent"]);
    }
    // The user and the inbox land and recover by hand (dagq-recover); the
    // run is not there, the authority is.
    for env in [&[][..], &inbox[..]] {
        failed_past_authorization(env, &db, &["recover", "r9"]);
        failed_past_authorization(env, &db, &["review", "7"]);
    }
    // The planner lands, recovers and supervises nothing (ADR-t728-1
    // decision 7).
    for (args, capability) in [
        (&["integrate", "1"][..], "landing.request"),
        (&["recover", "r1"], "run.recover"),
        (&["review", "1"], "review.prepare"),
        (&["supervise", "--once"], "scheduler.supervise"),
        (&["observe", "--dry-run"], "observe.run"),
    ] {
        let error = denied_as(&planner, &db, args);
        assert_eq!(error["denied"]["capability"], capability, "{args:?}");
    }
    // A planner's wrapper and span are its own planner's.
    let error = denied_as(
        &planner,
        &db,
        &["planner-session", "--planner", "4", "--claude", "claude"],
    );
    assert_eq!(error["denied"]["reason"], "not on this resource");
    let mut other = planner.to_vec();
    other.push(("DAGQ_PLANNER_ID", "4"));
    let error = denied_as(&other, &db, &["session-event", "open"]);
    assert_eq!(error["denied"]["capability"], "session.record");
    let mut own = planner.to_vec();
    own.push(("DAGQ_PLANNER_ID", "3"));
    failed_past_authorization(&own, &db, &["session-event", "open"]);
    failed_past_authorization(&inbox, &db, &["session-event", "open"]);
    // A planner workspace opened before `DAGQ_ACTOR_ID` still records its
    // span, as the queue's.
    failed_past_authorization(
        &[("DAGQ_ROLE", "planner"), ("DAGQ_PLANNER_ID", "4")],
        &db,
        &["session-event", "open"],
    );
    assert_eq!(denials(&db).len(), 7);
}
