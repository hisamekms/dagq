//! `dagq forecast` (ADR-0070 decisions 1 and 2) on a real queue: the open
//! tasks and goals with their p50 and p90, what it assumed, the narrowing
//! by task and goal, and that it records nothing.
use dagq::domain::EventKind;
use dagq::domain::LeaseToken;
use std::{path::Path, process::Command};

use dagq::{
    domain::{ClaimOutcome, CommitSha, TaskRun},
    infrastructure::sqlite::SqliteQueue,
};
use serde_json::{Value, json};

use crate::common::{Bounded, WithoutActor, cli::*};

/// `dagq forecast` as `role`, with the host-wide `host.toml` read from
/// `config` rather than the home of the person running the tests.
fn forecast(role: Option<&str>, db: &Path, config: &Path, args: &[&str]) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command.without_actor_env();
    command.env("XDG_CONFIG_HOME", config);
    if let Some(role) = role {
        command.env("DAGQ_ROLE", role);
    }
    let output = command
        .arg("--db")
        .arg(db)
        .arg("forecast")
        .args(args)
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn claim(queue: &mut SqliteQueue) -> TaskRun {
    let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&base, &LeaseToken::new("t"))
        .unwrap()
    else {
        panic!("nothing to claim");
    };
    *run
}

fn ids(list: &Value) -> Vec<i64> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_i64().unwrap())
        .collect()
}

/// One feature task of a goal lands; a docs task of the goal is in flight,
/// a third waits for it, and a task outside the goal waits for a slot.
#[test]
fn forecast_prints_the_open_tasks_and_goals_with_what_it_assumed() {
    let (dir, db) = queue();
    let config = dir.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    let goal = ok(&db, &["goal", "add", "grouped"])["id"].to_string();
    ok(
        &db,
        &["add", "landed", "--change", "feature", "--goal", &goal],
    );
    ok(&db, &["ready", "1", "--bypass-review"]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let landed = claim(&mut queue);
    for (kind, payload) in [
        (EventKind::ReceiptObserved, json!({})),
        (
            EventKind::ValidationFinished,
            json!({"status": "awaiting_integration"}),
        ),
        (EventKind::RunIntegrated, json!({"status": "integrated"})),
    ] {
        queue
            .record_runtime_event(landed.id(), kind, payload)
            .unwrap();
    }
    // The landing's bookkeeping, which these events do not do.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute("UPDATE tasks SET status='completed' WHERE id=1", [])
        .unwrap();
    ok(
        &db,
        &["add", "running", "--change", "docs", "--goal", &goal],
    );
    ok(&db, &["ready", "2", "--bypass-review"]);
    claim(&mut queue);
    ok(&db, &["add", "after", "--goal", &goal, "--depends-on", "2"]);
    ok(&db, &["add", "outside", "--priority", "low"]);
    ok(&db, &["add", "unplanned", "--goal", &goal]);
    ok(&db, &["ready", "3", "--bypass-review"]);
    ok(&db, &["ready", "4", "--bypass-review"]);
    let events = queue.all_events().unwrap().len();

    let all = forecast(None, &db, &config, &["--parallel", "2", "--trials", "20"]);
    assert_eq!(all["method"], 2);
    assert_eq!(all["trials"], 20);
    assert!(all["seed"].is_u64());
    let assumptions = &all["assumptions"];
    assert_eq!(assumptions["parallel"], 2);
    assert_eq!(assumptions["min_samples"], 5);
    assert_eq!(assumptions["samples"]["all"], 1);
    assert_eq!(assumptions["samples"]["changes"]["feature"]["runs"], 1);
    assert_eq!(
        assumptions["samples"]["changes"]["docs"],
        json!({"runs": 0, "distribution": "all"})
    );
    assert_eq!(assumptions["substituted"], json!(["docs", "unknown"]));
    // The landed task and the draft are left out.
    assert_eq!(ids(&all["tasks"]), [2, 3, 4]);
    let running = &all["tasks"][0];
    assert_eq!(running["phase"], "work");
    assert_eq!(running["waiting"], false);
    assert_eq!(running["distribution"], "all");
    assert_eq!(running["change"], "docs");
    for task in all["tasks"].as_array().unwrap() {
        assert!(task["p50"].is_string(), "{task}");
        assert!(task["p90_secs"].as_i64().unwrap() >= task["p50_secs"].as_i64().unwrap());
    }
    let goals = all["goals"].as_array().unwrap();
    assert_eq!(ids(&all["goals"]), [goal.parse::<i64>().unwrap()]);
    assert_eq!(goals[0]["open_tasks"], 2);
    assert_eq!(goals[0]["unplanned_tasks"], 1);
    // It cannot close before its unplanned task lands.
    assert_eq!(goals[0]["p50"], Value::Null);
    assert_eq!(goals[0]["reason"], "blocked");

    // The same moment gives the same forecast; the observer may read it.
    let task = forecast(
        Some("observer"),
        &db,
        &config,
        &["--task", "4", "--parallel", "2", "--trials", "20"],
    );
    assert_eq!(ids(&task["tasks"]), [4]);
    assert!(task["goals"].as_array().unwrap().is_empty());
    let in_goal = forecast(None, &db, &config, &["--goal", &goal, "--parallel", "0"]);
    assert_eq!(ids(&in_goal["tasks"]), [2, 3]);
    assert_eq!(in_goal["trials"], 1000);
    // No slot: only the run in flight finishes.
    assert!(in_goal["tasks"][0]["p50"].is_string());
    assert_eq!(in_goal["tasks"][1]["reason"], "no_slots");
    assert_eq!(in_goal["goals"][0]["reason"], "blocked");
    // Without a supervisor there is no slot either.
    let live = forecast(None, &db, &config, &["--trials", "5"]);
    assert_eq!(live["assumptions"]["parallel"], 0);

    assert_eq!(queue.all_events().unwrap().len(), events, "records nothing");
}

/// Set run `run`'s status as the runtime would have left it.
fn set_run_status(db: &Path, run: &TaskRun, status: &str) {
    rusqlite::Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status=?1 WHERE id=?2",
            rusqlite::params![status, run.id().as_str()],
        )
        .unwrap();
}

fn record(queue: &SqliteQueue, run: &TaskRun, steps: &[(EventKind, Value)]) {
    for (kind, payload) in steps {
        queue
            .record_runtime_event(run.id(), *kind, payload.clone())
            .unwrap();
    }
}

/// Task 1519: the latest run of an `in_progress` task is in flight
/// through its review and revise (`awaiting_integration`), its resume
/// (`needs_session`) and its wait to land, in the phase its events put it
/// in and waiting while it waits for a person; a failed run's retry and
/// an earlier failed run of a task are not, and a ready task still waits
/// for a slot.
#[test]
fn forecast_keeps_runs_under_review_revise_and_resume_in_flight() {
    let (dir, db) = queue();
    let config = dir.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    for title in ["revised", "resumed", "retried", "landing", "ready"] {
        ok(&db, &["add", title]);
    }
    for id in ["1", "2", "3"] {
        ok(&db, &["ready", id, "--bypass-review"]);
    }
    let mut queue = SqliteQueue::open(&db).unwrap();
    let validated = [
        (EventKind::ReceiptObserved, json!({})),
        (
            EventKind::ValidationFinished,
            json!({"status": "awaiting_integration"}),
        ),
    ];
    // 1: sent back by its review, worked again and validated again.
    let revised = claim(&mut queue);
    assert_eq!(revised.task_id().as_i64(), 1);
    record(&queue, &revised, &validated);
    record(
        &queue,
        &revised,
        &[
            (EventKind::ReviewStarted, json!({})),
            (EventKind::ReviewFinished, json!({"verdict": "revise"})),
            (EventKind::ResumeStarted, json!({})),
        ],
    );
    record(&queue, &revised, &validated);
    set_run_status(&db, &revised, "awaiting_integration");
    // 2: waits for a session to resume it, and for a person meanwhile.
    let resumed = claim(&mut queue);
    record(&queue, &resumed, &validated);
    record(
        &queue,
        &resumed,
        &[(
            EventKind::RunWaitingStarted,
            json!({"phase": "resume", "status": "needs_session"}),
        )],
    );
    set_run_status(&db, &resumed, "needs_session");
    // 3: an earlier run left in an in-flight status is not the latest;
    // the latest failed, and the retry waits for a slot.
    let older = claim(&mut queue);
    record(&queue, &older, &validated);
    set_run_status(&db, &older, "failed");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute("UPDATE tasks SET status='ready' WHERE id=3", [])
        .unwrap();
    let failed = claim(&mut queue);
    assert_eq!(failed.task_id().as_i64(), 3);
    record(&queue, &failed, &validated);
    set_run_status(&db, &failed, "failed");
    // Not a state the claim leaves, but one that tells the latest run
    // from any run in flight.
    set_run_status(&db, &older, "awaiting_integration");
    ok(&db, &["ready", "4", "--bypass-review"]);
    // 4: an earlier run failed in its work; the latest waits to land.
    let earlier = claim(&mut queue);
    set_run_status(&db, &earlier, "failed");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute("UPDATE tasks SET status='ready' WHERE id=4", [])
        .unwrap();
    let landing = claim(&mut queue);
    assert_eq!(landing.task_id().as_i64(), 4);
    record(&queue, &landing, &validated);
    record(&queue, &landing, &[(EventKind::RunE2eStarted, json!({}))]);
    set_run_status(&db, &landing, "awaiting_integration");
    ok(&db, &["ready", "5", "--bypass-review"]);

    let all = forecast(None, &db, &config, &["--parallel", "1", "--trials", "5"]);
    let tasks = all["tasks"].as_array().unwrap();
    let row = |id: i64| tasks.iter().find(|task| task["id"] == id).unwrap();
    assert_eq!(ids(&all["tasks"]), [1, 2, 3, 4, 5]);
    assert_eq!(
        (&row(1)["phase"], &row(1)["waiting"]),
        (&json!("wait_to_land"), &json!(false))
    );
    assert_eq!(
        (&row(2)["phase"], &row(2)["waiting"]),
        (&json!("wait_to_land"), &json!(true))
    );
    assert_eq!(
        (&row(3)["phase"], &row(3)["waiting"]),
        (&Value::Null, &json!(false))
    );
    assert_eq!(
        (&row(4)["phase"], &row(4)["waiting"]),
        (&json!("wait_to_land"), &json!(false))
    );
    assert_eq!(
        (&row(5)["phase"], &row(5)["waiting"]),
        (&Value::Null, &json!(false))
    );
}
