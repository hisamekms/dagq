//! `dagq forecast` (ADR-0070 decisions 1 and 2) on a real queue: the open
//! tasks and goals with their p50 and p90, what it assumed, the narrowing
//! by task and goal, and that it records nothing.
use dagq::domain::LeaseToken;
use std::{path::Path, process::Command};

use dagq::{
    domain::{ClaimOutcome, CommitSha, TaskRun},
    infrastructure::sqlite::SqliteQueue,
};
use serde_json::{Value, json};

use crate::common::{Bounded, cli::*};

/// `dagq forecast` as `role`, with the host-wide `host.toml` read from
/// `config` rather than the home of the person running the tests.
fn forecast(role: Option<&str>, db: &Path, config: &Path, args: &[&str]) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command
        .env("XDG_CONFIG_HOME", config)
        .env_remove("DAGQ_ROLE");
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

/// One runtime task of a goal lands; a docs task of the goal is in flight,
/// a third waits for it, and a task outside the goal waits for a slot.
#[test]
fn forecast_prints_the_open_tasks_and_goals_with_what_it_assumed() {
    let (dir, db) = queue();
    let config = dir.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    let goal = ok(&db, &["goal", "add", "grouped"])["id"].to_string();
    ok(
        &db,
        &["add", "landed", "--kind", "runtime", "--goal", &goal],
    );
    ok(&db, &["ready", "1", "--bypass-review"]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let landed = claim(&mut queue);
    for (kind, payload) in [
        ("receipt_observed", json!({})),
        (
            "validation_finished",
            json!({"status": "awaiting_integration"}),
        ),
        ("run_integrated", json!({"status": "integrated"})),
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
    ok(&db, &["add", "running", "--kind", "docs", "--goal", &goal]);
    ok(&db, &["ready", "2", "--bypass-review"]);
    claim(&mut queue);
    ok(&db, &["add", "after", "--goal", &goal, "--depends-on", "2"]);
    ok(&db, &["add", "outside", "--priority", "low"]);
    ok(&db, &["add", "unplanned", "--goal", &goal]);
    ok(&db, &["ready", "3", "--bypass-review"]);
    ok(&db, &["ready", "4", "--bypass-review"]);
    let events = queue.all_events().unwrap().len();

    let all = forecast(None, &db, &config, &["--parallel", "2", "--trials", "20"]);
    assert_eq!(all["method"], 1);
    assert_eq!(all["trials"], 20);
    assert!(all["seed"].is_u64());
    let assumptions = &all["assumptions"];
    assert_eq!(assumptions["parallel"], 2);
    assert_eq!(assumptions["min_samples"], 5);
    assert_eq!(assumptions["samples"]["all"], 1);
    assert_eq!(assumptions["samples"]["kinds"]["runtime"]["runs"], 1);
    assert_eq!(
        assumptions["samples"]["kinds"]["docs"],
        json!({"runs": 0, "distribution": "all"})
    );
    assert_eq!(assumptions["substituted"], json!(["docs", "unknown"]));
    // The landed task and the draft are left out.
    assert_eq!(ids(&all["tasks"]), [2, 3, 4]);
    let running = &all["tasks"][0];
    assert_eq!(running["phase"], "work");
    assert_eq!(running["waiting"], false);
    assert_eq!(running["distribution"], "all");
    assert_eq!(running["kind"], "docs");
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
