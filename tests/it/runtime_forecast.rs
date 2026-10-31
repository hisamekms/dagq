//! Runtime tests: the forecast snapshots the supervisor records
//! (ADR-0070 decision 3).
use crate::runtime_support;
use dagq::domain::EventKind;

use runtime_support::*;

fn snapshots(db: &Path) -> Vec<Value> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == EventKind::ForecastRecorded)
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
        // The forecast draws its samples from the ledger.
        ledger: Some(runtime::LedgerOptions {
            interval: Duration::ZERO,
        }),
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
/// event, before anything landed. Each later one was kept by a trigger
/// (a landing kept only when it moved a p50, its samples read from the
/// ledger the supervisor builds once a run's push ended). The next
/// supervisor's start is a change mark.
#[test]
fn snapshots_are_recorded_at_the_day_a_moving_landing_and_a_start() {
    let (dir, repo, db) = fixture();
    let host = dir.path().join("no host-wide file.toml");
    let goal = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        let goal = queue
            .add_goal(NewGoal {
                priority: Default::default(),
                title: "forecast".into(),
                description: "d".into(),
                acceptance: "a".into(),
                constraints: "c".into(),
                doc: None,
                draft: false,
                tags: Vec::new(),
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
    assert!(!recorded.is_empty(), "{recorded:#?}");
    // The day's snapshot: nothing landed yet, so no time for anything.
    assert_eq!(triggers(&recorded[0]), ["daily"]);
    assert_eq!(ids(&recorded[0], "tasks"), [1, 2]);
    assert_eq!(ids(&recorded[0], "goals"), [goal.as_i64()]);
    assert!(recorded[0]["tasks"][0]["p50_secs"].is_null());
    assert!(recorded[0]["supervisor"].is_string());
    assert_eq!(recorded[0]["method"], 3);
    assert!(recorded[0]["assumptions"]["parallel"].is_number());
    assert!(recorded[0]["triggers_through"].as_i64().unwrap() > 0);
    // A landing's snapshot is kept only for what it moved.
    for later in &recorded[1..] {
        assert_eq!(triggers(later), ["landing"], "{later}");
        assert!(!later["moved"].as_array().unwrap().is_empty(), "{later}");
    }
    let integrated = SqliteQueue::open(&db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == EventKind::RunIntegrated)
        .count();
    assert_eq!(integrated, 2);
    let before = recorded.len();

    // A new supervisor's start is a change mark: one more snapshot, with
    // the open goal and no task.
    let outcome = supervise_forecasting(&db, &repo, &reviewer, &host);
    assert_eq!(outcome["runs"], json!([]), "{outcome}");
    let recorded = snapshots(&db);
    assert_eq!(recorded.len(), before + 1, "{recorded:#?}");
    let started = &recorded[before];
    assert!(
        started["triggers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["trigger"] == "mark" && t["kind"] == "supervisor_started"),
        "{}",
        started["triggers"]
    );
    assert_eq!(ids(started, "tasks"), Vec::<i64>::new());
    assert_eq!(ids(started, "goals"), [goal.as_i64()]);
    assert!(
        started["assumptions"]["samples"]["all"].is_number(),
        "{started}"
    );
}

/// `kpi` scores the snapshots against the finishes (ADR-0070 decision
/// 4). `kpi` reads every event of the queue and is stopped until it reads
/// the ledger (ADR-t3745-1): task 1669 brings this back and removes this
/// cfg.
#[cfg(any())]
#[test]
fn kpi_scores_the_snapshots_against_the_finishes() {
    let (dir, repo, db) = fixture();
    let host = dir.path().join("no host-wide file.toml");
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    supervise_forecasting(&db, &repo, &reviewer, &host);
    // `kpi` scores the snapshots against the finishes (ADR-0070 decision
    // 4): the landing's snapshot of task 2 is the one sample; the day's
    // gave neither task a p50, so both of its rows are left out.
    let config = dir.path().join("config");
    fs::create_dir_all(&config).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command.without_actor_env().env("XDG_CONFIG_HOME", &config);
    crate::common::cli::reach(&mut command, &db, &["kpi"]);
    let output = command
        .arg("--db")
        .arg(&db)
        .args(["kpi", "--last", "1"])
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let today = &report["periods"][0];
    let forecast = &today["details"]["forecast"];
    assert_eq!(forecast["samples"], 1, "{forecast}");
    assert_eq!(forecast["excluded"]["unforecast"], 2, "{forecast}");
    let kpis = &today["kpis"];
    assert_eq!(kpis["forecast.p50_error"]["all"]["n"], 1, "{kpis}");
    assert_eq!(kpis["forecast.p50_error"]["target=task"]["n"], 1);
    assert_eq!(kpis["forecast.p50_error"]["marks=0"]["n"], 1);
    assert_eq!(kpis["forecast.p90_hit_rate"]["all"]["n"], 1);
    assert!(kpis["forecast.late_rate"]["all"]["value"].is_number());
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

/// Task 1519: the snapshot reads the runs as `dagq forecast` does. A run
/// sent back by its review (`awaiting_integration`) and one waiting for a
/// session and a person (`needs_session`) are recorded in `wait_to_land`,
/// the second waiting; a failed run's task waits for a slot.
#[test]
fn a_snapshot_records_runs_under_revise_and_resume_in_their_phase() {
    use dagq::{
        application::forecast::{SnapshotOutcome, pending, record_snapshot},
        domain::run_phase::{Attempt, AttemptKind, Phase, PhaseChange},
        domain::{ClaimOutcome, EventKind, LeaseToken},
        infrastructure::adapters::SystemProcesses,
    };
    let moved = |phase: Phase, attempt: Attempt| {
        (
            EventKind::RunPhaseChanged,
            PhaseChange::new(phase, attempt, "test").payload(),
        )
    };
    let revise = Attempt::of(AttemptKind::Revise, 1);
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "resumed", &[]);
    add_ready_task(&mut queue, "failed", &[]);
    let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
    let mut claim = |status: &str, steps: &[(EventKind, Value)]| {
        let ClaimOutcome::Claimed { run } = queue
            .claim_for_supervisor(&base, &LeaseToken::new("t"))
            .unwrap()
        else {
            panic!("nothing to claim");
        };
        for (kind, payload) in [
            (EventKind::ReceiptObserved, json!({})),
            (
                EventKind::ValidationFinished,
                json!({"status": "awaiting_integration"}),
            ),
        ]
        .iter()
        .chain(steps)
        {
            queue
                .record_runtime_event(run.id(), *kind, payload.clone())
                .unwrap();
        }
        Connection::open(&db)
            .unwrap()
            .execute(
                "UPDATE task_runs SET status=?1 WHERE id=?2",
                rusqlite::params![status, run.id().as_str()],
            )
            .unwrap();
    };
    claim(
        "awaiting_integration",
        &[
            moved(Phase::Validating, Attempt::FIRST),
            moved(Phase::Review, Attempt::FIRST),
            moved(Phase::Revise, revise),
        ],
    );
    claim(
        "needs_session",
        &[
            moved(Phase::Validating, Attempt::FIRST),
            moved(Phase::NeedsSession, Attempt::FIRST),
            moved(Phase::Waiting { in_slot: false }, Attempt::FIRST),
        ],
    );
    claim("failed", &[]);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let found = pending(&queue, now, |_| 0, None).unwrap().unwrap();
    let outcome = record_snapshot(
        &queue,
        &queue,
        &SystemProcesses,
        &LeaseToken::new("t"),
        now,
        5,
        &found,
    )
    .unwrap();
    assert!(
        matches!(outcome, SnapshotOutcome::Recorded { tasks: 3, .. }),
        "{outcome:?}"
    );
    let recorded = snapshots(&db);
    assert_eq!(recorded.len(), 1, "{recorded:#?}");
    let rows: Vec<(i64, Value, Value)> = recorded[0]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["id"].as_i64().unwrap(),
                row["phase"].clone(),
                row["waiting"].clone(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            (1, json!("revise"), json!(false)),
            (2, json!("waiting"), json!(true)),
            (3, Value::Null, json!(false)),
        ]
    );
}
