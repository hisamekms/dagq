//! Only the queue service's process opens the queue's DB (ADR-t3102-2
//! decision 1): a `dagq` of a worker, a job or the observer with no
//! service's socket, and a job command no supervisor started, are refused
//! and open no queue. The rest of the processes reach it through the
//! service: the host's CLI (`host_commands`, `host_control`), the
//! supervisor (`supervisor_service`) and the session wrappers
//! (`session_wrapper_service`).

use crate::common;
use common::cli::{invoke_unserved, queue};
use serde_json::Value;
use std::fs;

/// What a refused command printed.
fn refusal(output: &std::process::Output, args: &[&str]) -> Value {
    assert!(!output.status.success(), "{args:?} was allowed");
    serde_json::from_slice(&output.stderr).unwrap()
}

#[test]
fn a_dagq_of_no_service_s_principal_is_refused_and_opens_no_queue() {
    let (_dir, db) = queue();
    // No SQLite opens it: a process that opened the queue would fail on
    // the file, not refuse. Which commands are refused is `unserved`'s,
    // unit-tested in src/main.rs: here a worker's read, and a job command
    // no supervisor started, which passes the operation's authorization
    // first.
    fs::write(&db, b"not a queue").unwrap();
    let worker = [
        ("DAGQ_ROLE", "worker"),
        ("DAGQ_RUN_ID", "00000000-0000-4000-8000-000000000001"),
        ("DAGQ_TASK_ID", "1"),
    ];
    for (env, args) in [
        (&worker[..], &["show", "1"][..]),
        (&[("DAGQ_ROLE", "supervisor")], &["observe", "--dry-run"]),
    ] {
        let error = refusal(&invoke_unserved(env, &db, args), args);
        let message = error["error"].as_str().unwrap();
        assert!(
            message.contains("reaches the queue only through its queue service")
                && message.contains("opens no queue"),
            "{env:?} {args:?}: {message}"
        );
    }
    assert_eq!(fs::read(&db).unwrap(), b"not a queue");
}
