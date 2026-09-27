//! Runtime tests: the forecast snapshots the supervisor records
//! (ADR-0070 decision 3).
use crate::runtime_support;

use runtime_support::*;

fn snapshots(db: &Path) -> Vec<Value> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "forecast_recorded")
        .map(|event| event.payload)
        .collect()
}

fn ids(snapshot: &Value, key: &str) -> Vec<i64> {
    snapshot[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_i64().unwrap())
        .collect()
}

fn triggers(snapshot: &Value) -> Vec<String> {
    snapshot["triggers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|trigger| trigger["trigger"].as_str().unwrap().to_owned())
        .collect()
}

fn options(host_config: Option<PathBuf>) -> SuperviseOptions {
    SuperviseOptions {
        forecast_snapshots: true,
        forecast_check: Duration::ZERO,
        host_config,
        ..supervise_options(1, true)
    }
}

fn supervise_forecasting(db: &Path, repo: &Path, reviewer: &TestReviewer, host: &Path) -> Value {
    let backend = TestWorkspace::new(db, false, IDLE_AGENT);
    let _waiting = common_within();
    let outcome = runtime::supervise_with_reviewer(
        db,
        repo,
        &backend,
        &claude_stub(db),
        reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options(Some(host.to_path_buf())),
    )
    .unwrap();
    backend.join();
    outcome
}

fn common_within() -> crate::common::Waiting {
    crate::common::within(crate::common::STEP_LIMIT, "supervise to return")
}

/// The first snapshot is the day's, with every open task and goal in one
/// event; the landing of the first task moves the second (it gets a time
/// from the landed run) and is recorded, the landing of the last moves
/// nothing and is not. The next supervisor's start is a change mark.
#[test]
fn snapshots_are_recorded_at_the_day_a_moving_landing_and_a_start() {
    let (dir, repo, db) = fixture();
    let host = dir.path().join("no host-wide file.toml");
    let goal = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        let goal = queue
            .add_goal(NewGoal {
                title: "forecast".into(),
                description: "d".into(),
                acceptance: "a".into(),
                constraints: "c".into(),
                doc: None,
                draft: false,
            })
            .unwrap()
            .id();
        queue.set_goal(TaskId::new(1), Some(goal)).unwrap();
        let second = add_ready_task(&mut queue, "second task", &[TaskId::new(1)]);
        queue.set_goal(second, Some(goal)).unwrap();
        goal
    };
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_forecasting(&db, &repo, &reviewer, &host);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 2, "{outcome}");

    let recorded = snapshots(&db);
    assert_eq!(recorded.len(), 2, "{recorded:#?}");
    // The day's snapshot: nothing landed yet, so no time for anything.
    assert_eq!(triggers(&recorded[0]), ["daily"]);
    assert_eq!(ids(&recorded[0], "tasks"), [1, 2]);
    assert_eq!(ids(&recorded[0], "goals"), [goal.as_i64()]);
    assert!(recorded[0]["tasks"][0]["p50_secs"].is_null());
    assert!(recorded[0]["supervisor"].is_string());
    assert_eq!(recorded[0]["method"], 1);
    assert!(recorded[0]["assumptions"]["parallel"].is_number());
    assert!(recorded[0]["triggers_through"].as_i64().unwrap() > 0);
    // The first landing gave the second task a time.
    assert_eq!(triggers(&recorded[1]), ["landing"]);
    assert_eq!(recorded[1]["triggers"][0]["task_id"], 1);
    assert_eq!(ids(&recorded[1], "tasks"), [2]);
    assert!(recorded[1]["tasks"][0]["p50_secs"].is_number());
    let moved: Vec<(String, i64)> = recorded[1]["moved"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["target"].as_str().unwrap().to_owned(),
                m["id"].as_i64().unwrap(),
            )
        })
        .collect();
    assert!(moved.contains(&("task".into(), 2)), "{moved:?}");
    // The landing of the last task moved no p50 past the thresholds.
    let integrated = SqliteQueue::open(&db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "run_integrated")
        .count();
    assert_eq!(integrated, 2);

    // A new supervisor's start is a change mark: one more snapshot, with
    // the open goal and no task.
    let outcome = supervise_forecasting(&db, &repo, &reviewer, &host);
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    let recorded = snapshots(&db);
    assert_eq!(recorded.len(), 3, "{recorded:#?}");
    assert!(
        recorded[2]["triggers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["trigger"] == "mark" && t["kind"] == "supervisor_started"),
        "{}",
        recorded[2]["triggers"]
    );
    assert_eq!(ids(&recorded[2], "tasks"), Vec::<i64>::new());
    assert_eq!(ids(&recorded[2], "goals"), [goal.as_i64()]);
}

/// A snapshot that cannot be computed (the host's settings do not read)
/// is logged and stops neither the claim nor the landing; without the
/// setting nothing is recorded.
#[test]
fn a_failed_snapshot_stops_no_landing_and_off_records_nothing() {
    let (dir, repo, db) = fixture();
    let broken = dir.path().join("host.toml");
    fs::write(&broken, "[kpi\nnot toml").unwrap();
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_forecasting(&db, &repo, &reviewer, &broken);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert!(snapshots(&db).is_empty());

    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_ready_task(&mut queue, "another task", &[]);
    }
    let outcome = supervise_reviewed(
        &db,
        &repo,
        &TestWorkspace::new(&db, false, IDLE_AGENT),
        &reviewer,
    );
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert!(snapshots(&db).is_empty());
}
