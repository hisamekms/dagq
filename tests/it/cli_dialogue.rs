//! The asks, answers, notes, marks and findings are authorized in the
//! application before they change the queue (ADR-t728-1 decision 5,
//! ADR-t728-3, task 733): a worker asks on its own run only, only the user
//! and the inbox answer, the answer records whose authority it carries
//! (the user's own or the inbox's delegated one) and whether it approves,
//! and the observer still records findings and raises blocked asks.

use crate::common;

use common::cli::*;

use serde_json::{Value, json};
use std::path::Path;

fn allowed_as(env: &[(&str, &str)], db: &Path, args: &[&str]) -> Value {
    let output = invoke_with(env, db, args);
    assert!(
        output.status.success(),
        "{env:?} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The error JSON `args` is refused with.
fn denied_as(env: &[(&str, &str)], db: &Path, args: &[&str]) -> Value {
    let output = invoke_with(env, db, args);
    assert!(!output.status.success(), "{env:?} {args:?} was allowed");
    serde_json::from_slice(&output.stderr).unwrap()
}

fn events(db: &Path, kind: &str) -> Vec<Value> {
    ok(
        db,
        &["events", "--after", "0", "--all", "--full", "--kind", kind],
    )["events"]
        .as_array()
        .unwrap()
        .clone()
}

fn ask(kind: &str, target: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = [
        "ask",
        "--kind",
        kind,
        "--because",
        "scope",
        "--question",
        "which?",
        "--option",
        "retry",
        "--option",
        "cancel",
    ]
    .map(str::to_owned)
    .to_vec();
    args.extend(target.iter().map(|arg| (*arg).to_owned()));
    args
}

fn strs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

const WORKER: [(&str, &str); 4] = [
    ("DAGQ_ROLE", "worker"),
    ("DAGQ_ACTOR_ID", "worker:r1"),
    ("DAGQ_RUN_ID", "r1"),
    ("DAGQ_TASK_ID", "1"),
];

#[test]
fn a_worker_asks_on_its_own_run_and_task_only() {
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    ok(&db, &["add", "second"]);
    for (args, reason) in [
        (
            ask("worker_question", &["--run", "r2"]),
            "not on this resource",
        ),
        (
            ask("worker_question", &["--task", "2"]),
            "not on this resource",
        ),
        (ask("decide", &["--run", "r1"]), "not of this kind"),
        (ask("approve_landing", &["--task", "1"]), "not of this kind"),
    ] {
        let error = denied_as(&WORKER, &db, &strs(&args));
        assert_eq!(
            error["error"],
            format!("worker may not ask.open ({reason})"),
            "{args:?}"
        );
        assert_eq!(error["denied"]["capability"], "ask.open");
    }
    assert!(
        ok(&db, &["asks", "--all"])["asks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let denied = events(&db, "authorization_denied");
    assert_eq!(denied.len(), 4);
    assert_eq!(
        denied[0]["payload"]["resource"],
        json!({"kind": "new_ask", "ask_kind": "worker_question", "run": "r2", "task": null})
    );
    assert_eq!(denied[0]["actor"]["id"], "worker:r1");
    // Its own task it asks about.
    let asked = allowed_as(
        &WORKER,
        &db,
        &strs(&ask("worker_question", &["--task", "1"])),
    );
    assert_eq!(asked["asked_by"], "worker");
}

#[test]
fn workers_jobs_the_observer_and_the_planner_neither_answer_nor_close() {
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    let open = ok(&db, &strs(&ask("decide", &["--task", "1"])))["id"].to_string();
    let answered = ok(&db, &strs(&ask("approve_landing", &["--task", "1"])))["id"].to_string();
    ok(&db, &["answer", &answered, "--text", "retry"]);
    let mut actors: Vec<Vec<(&str, &str)>> = vec![
        WORKER.to_vec(),
        vec![("DAGQ_ROLE", "planner"), ("DAGQ_ACTOR_ID", "planner:1")],
    ];
    for role in [
        "review-job",
        "recovery-job",
        "plan-review-job",
        "goal-review-job",
        "observer",
    ] {
        actors.push(vec![("DAGQ_ROLE", role)]);
    }
    let mut refused = 0;
    for env in &actors {
        for (args, capability) in [
            (vec!["answer", &open, "--text", "retry"], "ask.answer"),
            (vec!["ask", "close", &answered], "ask.close"),
        ] {
            let error = denied_as(env, &db, &args);
            assert_eq!(error["denied"]["capability"], capability, "{env:?}");
            assert_eq!(error["denied"]["reason"], "not granted", "{env:?}");
            refused += 1;
        }
    }
    let asks = ok(&db, &["asks", "--all"])["asks"].clone();
    assert!(asks[0]["answer"].is_null(), "{asks}");
    assert!(asks[1]["closed_at"].is_null(), "{asks}");
    let denied = events(&db, "authorization_denied");
    assert_eq!(denied.len(), refused);
    // Refused before the ask was read.
    assert_eq!(
        denied[0]["payload"]["resource"],
        json!({"kind": "ask", "id": 1, "run": null})
    );
}

#[test]
fn the_user_and_the_inbox_answer_and_the_record_tells_them_apart() {
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    let inbox = [("DAGQ_ROLE", "inbox"), ("DAGQ_ACTOR_ID", "inbox")];
    let by_user = ok(&db, &strs(&ask("decide", &["--task", "1"])))["id"].to_string();
    let by_inbox = ok(&db, &strs(&ask("approve_landing", &["--task", "1"])))["id"].to_string();
    let question = allowed_as(
        &inbox,
        &db,
        &strs(&ask("worker_question", &["--task", "1"])),
    );
    let plain = question["id"].to_string();

    let user_answer = ok(&db, &["answer", &by_user, "--text", "retry"]);
    let inbox_answer = allowed_as(&inbox, &db, &["answer", &by_inbox, "--text", "cancel"]);
    let plain_answer = allowed_as(&inbox, &db, &["answer", &plain, "--text", "go on"]);
    // The row: `answered_by` and `asked_by` keep their values; the
    // authority and the approval are new.
    assert_eq!(user_answer["answered_by"], "person");
    assert_eq!(user_answer["answer_authority"], "user");
    assert_eq!(user_answer["answer_approval"], true);
    assert_eq!(user_answer["asked_by"], "human");
    assert_eq!(inbox_answer["answered_by"], "inbox");
    assert_eq!(inbox_answer["answer_authority"], "delegated");
    assert_eq!(inbox_answer["answer_approval"], true);
    assert_eq!(plain_answer["answer_authority"], "delegated");
    assert_eq!(plain_answer["answer_approval"], false);
    assert_eq!(question["asked_by"], "inbox");

    let answered = events(&db, "ask_answered");
    let of = |id: &str| {
        answered
            .iter()
            .find(|event| event["payload"]["ask_id"] == id.parse::<i64>().unwrap())
            .unwrap()
            .clone()
    };
    let user = of(&by_user);
    assert_eq!(user["payload"]["authority"], "user");
    assert_eq!(user["payload"]["approval"], true);
    assert_eq!(user["payload"]["answered_by"], "person");
    assert_eq!(user["actor"], json!({"role": "user", "id": "user"}));
    let delegated = of(&by_inbox);
    assert_eq!(delegated["payload"]["authority"], "delegated");
    assert_eq!(delegated["payload"]["approval"], true);
    assert_eq!(delegated["actor"], json!({"role": "inbox", "id": "inbox"}));
    assert_eq!(of(&plain)["payload"]["approval"], false);
    // The inbox closes what it read.
    allowed_as(&inbox, &db, &["ask", "close", &by_inbox]);
    assert!(events(&db, "authorization_denied").is_empty());
}

#[test]
fn the_observer_still_records_findings_and_raises_blocked_asks() {
    let (_dir, db) = queue();
    ok(&db, &["add", "first"]);
    let observer = [("DAGQ_ROLE", "observer"), ("DAGQ_ACTOR_ID", "observer:s1")];
    let finding = allowed_as(
        &observer,
        &db,
        &[
            "finding",
            "record",
            "--kind",
            "stall",
            "--queue",
            "--summary",
            "waits",
        ],
    );
    assert_eq!(finding["recorded_by"], "observer");
    let id = finding["id"].to_string();
    let blocked = allowed_as(&observer, &db, &strs(&ask("blocked", &["--finding", &id])));
    assert_eq!(blocked["asked_by"], "observer");
    assert_eq!(blocked["created"], true);
    allowed_as(
        &observer,
        &db,
        &["finding", "resolve", &id, "--reason", "gone"],
    );
    // Not another kind, a note, a mark or a dismissal.
    for args in [
        strs(&ask("decide", &["--task", "1"])),
        vec!["note", "--task", "1", "--text", "x"],
        vec!["mark", "label"],
        vec!["finding", "dismiss", &id, "--reason", "no"],
    ] {
        let error = denied_as(&observer, &db, &args);
        assert_eq!(
            error["error"], "observer may not change queue state",
            "{args:?}"
        );
    }
    // A person answers the observer's blocked ask with `dismiss`, an
    // approval that the runtime applies to the finding.
    let answered = ok(
        &db,
        &["answer", &blocked["id"].to_string(), "--text", "dismiss"],
    );
    assert_eq!(answered["answer_approval"], true);
    assert_eq!(answered["answer_authority"], "user");
}
