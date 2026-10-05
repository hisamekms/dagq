//! The worker's model and effort (ADR-0079 decisions 3 and 4): given
//! explicitly at every start, recorded at the claim, and with
//! `[worker.trial]` on, alternated between the control and the treatment
//! for the mechanical tasks of the lower third.
use crate::common;
use crate::runtime_support;
use dagq::domain::EventKind;
use dagq::domain::LeaseToken;

use dagq::domain::{ClaimOutcome, worker_model::WorkerTrial};
use runtime_support::*;

/// Record plan review's prediction for `task` the way `finish_plan_review`
/// does.
fn predict(db: &Path, task: i64, nature: &str, tokens: u64) {
    Connection::open(db)
        .unwrap()
        .execute(
            "INSERT INTO run_events(task_id, kind, payload) VALUES (?1, 'task_weight_predicted', ?2)",
            rusqlite::params![
                task,
                json!({"proposal_id": 1, "plan_review_id": 1, "attempt": 1,
                       "prediction": {"size": "S", "nature": nature, "uncertainty": 0.2,
                                      "expected_output_tokens": tokens, "rework_probability": 0.1,
                                      "reason": "r"},
                       "model": "claude-opus-5-5", "effort": "medium"})
                .to_string()
            ],
        )
        .unwrap();
}

fn claimed(queue: &mut SqliteQueue, task: i64) -> Value {
    let detail = queue.show(TaskId::new(task)).unwrap();
    payloads(&detail, "run_claimed")[0].clone()
}

fn session_of(claimed: &Value) -> (Value, Value, Value) {
    (
        claimed["model"].clone(),
        claimed["effort"].clone(),
        claimed["group"].clone(),
    )
}

fn opus() -> (Value, Value, Value) {
    (json!("claude-opus-5-5"), json!("medium"), Value::Null)
}

/// Without `[worker.trial]` every worker starts at Opus 5.5 medium, given
/// explicitly, and its claim records them without a group.
#[test]
fn workers_start_at_opus_medium_by_default() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(session_of(&claimed(&mut queue, 1)), opus());
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(
        turn_models(&db),
        [
            json!({"run_id": run.id(), "resume": false, "model": "claude-opus-5-5", "effort": "medium"})
        ]
    );
}

/// With the trial on, the two mechanical tasks of the lower third take the
/// control and then the treatment, started at Sonnet 5 medium; the others
/// stay at the default outside any group.
#[test]
fn the_trial_alternates_the_mechanical_tasks_of_the_lower_third() {
    let (_dir, repo, db) = fixture();
    fs::write(
        repo.join("dagq.toml"),
        "[worker.trial]\nenabled = true\nwindow = 3\n",
    )
    .unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-m", "trial"]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    for title in ["light", "heavy", "judgment"] {
        add_ready_task(&mut queue, title, &[]);
    }
    predict(&db, 1, "mechanical", 1_000);
    predict(&db, 2, "mechanical", 1_000);
    predict(&db, 3, "mechanical", 900_000);
    predict(&db, 4, "design_judgment", 900_000);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let first = claimed(&mut queue, 1);
    assert_eq!(
        session_of(&first),
        (json!("claude-opus-5-5"), json!("medium"), json!("control"))
    );
    assert_eq!(first["trial_percentile"], 16.7);
    let second = claimed(&mut queue, 2);
    assert_eq!(
        session_of(&second),
        (
            json!("claude-sonnet-5"),
            json!("medium"),
            json!("treatment")
        )
    );
    // Above the lower third, and not mechanical.
    let heavy = claimed(&mut queue, 3);
    assert_eq!(session_of(&heavy), opus());
    assert_eq!(heavy["trial_percentile"], 83.3);
    assert_eq!(session_of(&claimed(&mut queue, 4)), opus());
    // The wrapper starts each session with what its claim chose.
    let mut run_of = |task: i64| {
        queue.show(TaskId::new(task)).unwrap().runs[0]
            .id()
            .as_str()
            .to_owned()
    };
    let started: HashMap<String, Value> = turn_models(&db)
        .into_iter()
        .map(|line| (line["run_id"].as_str().unwrap().to_owned(), line))
        .collect();
    assert_eq!(started.len(), 4);
    assert_eq!(started[&run_of(1)]["model"], "claude-opus-5-5");
    assert_eq!(started[&run_of(2)]["model"], "claude-sonnet-5");
    assert_eq!(started[&run_of(2)]["effort"], "medium");
    assert_eq!(started[&run_of(3)]["model"], "claude-opus-5-5");
}

/// The claim itself: a task keeps its group on a later claim, a task
/// without a prediction or with too few others stays outside, and the
/// trial off leaves every claim at the default.
#[test]
fn claims_choose_the_session_in_their_transaction() {
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    for title in ["b", "c", "d"] {
        add_ready_task(&mut queue, title, &[]);
    }
    let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
    let on = WorkerTrial {
        enabled: true,
        window: 2,
    };
    let claim = |queue: &mut SqliteQueue, task: i64, trial: &WorkerTrial| {
        let outcome = queue
            .claim_for_supervisor_in_order(
                &base,
                &LeaseToken::new("t"),
                &[TaskId::new(task)],
                None,
                trial,
                &dagq::domain::provider_switch::WorkerRoute::direct(
                    &dagq::domain::worker::Worker::ALL,
                ),
            )
            .unwrap();
        let ClaimOutcome::Claimed { run } = outcome else {
            panic!("nothing to claim");
        };
        *run
    };
    // Task 1 has too few others to be ranked among.
    predict(&db, 1, "mechanical", 10);
    predict(&db, 2, "mechanical", 500);
    claim(&mut queue, 1, &on);
    assert_eq!(session_of(&claimed(&mut queue, 1)), opus());
    assert_eq!(claimed(&mut queue, 1).get("trial_percentile"), None);
    // Task 3 has none of its own.
    claim(&mut queue, 3, &on);
    assert_eq!(session_of(&claimed(&mut queue, 3)), opus());
    // The trial off: task 4 would be a subject, but stays outside.
    predict(&db, 4, "mechanical", 1);
    claim(&mut queue, 4, &WorkerTrial::default());
    assert_eq!(session_of(&claimed(&mut queue, 4)), opus());
    // Task 2 is ranked among tasks 4 and 1 now: 500 is above 10 and 1.
    claim(&mut queue, 2, &on);
    let payload = claimed(&mut queue, 2);
    assert_eq!(session_of(&payload), opus());
    assert_eq!(payload["trial_percentile"], 100.0);
}

/// `stats` puts the runs of each group side by side: their model, their
/// speed and their task-caused rework.
#[test]
fn stats_compare_the_groups() {
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "b", &[]);
    add_ready_task(&mut queue, "c", &[]);
    let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
    for (task, tokens) in [(1, 10), (2, 10), (3, 900)] {
        predict(&db, task, "mechanical", tokens);
    }
    let on = WorkerTrial {
        enabled: true,
        window: 2,
    };
    for task in 1..=3 {
        let ClaimOutcome::Claimed { run } = queue
            .claim_for_supervisor_in_order(
                &base,
                &LeaseToken::new("t"),
                &[TaskId::new(task)],
                None,
                &on,
                &dagq::domain::provider_switch::WorkerRoute::direct(
                    &dagq::domain::worker::Worker::ALL,
                ),
            )
            .unwrap()
        else {
            panic!("nothing to claim");
        };
        let mut events = vec![
            (EventKind::ReceiptObserved, json!({})),
            (
                EventKind::ValidationFinished,
                json!({"status": "awaiting_integration"}),
            ),
        ];
        // The treatment was sent back to revise: task-caused rework.
        if task == 2 {
            events.push((EventKind::ReviseRequested, json!({"attempt": 1})));
        }
        events.push((EventKind::RunIntegrated, json!({"status": "integrated"})));
        for (kind, payload) in events {
            queue.record_runtime_event(run.id(), kind, payload).unwrap();
        }
    }
    let output = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .arg("--db")
        .arg(&db)
        .args(["stats", "--full", "--cmux", "/usr/bin/true"])
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stats: Value = serde_json::from_slice(&output.stdout).unwrap();
    let groups = stats["trial_groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "{groups:?}");
    assert_eq!(groups[0]["group"], "control");
    assert_eq!(groups[0]["sessions"], json!({"claude-opus-5-5/medium": 1}));
    assert_eq!(groups[0]["runs"], 1);
    assert_eq!(groups[0]["tasks"], 1);
    assert_eq!(groups[0]["task_rework"], 0);
    assert_eq!(groups[0]["task_rework_rate"], 0.0);
    assert_eq!(groups[0]["lead_time"]["count"], 1);
    assert_eq!(groups[0]["work"]["count"], 1);
    assert_eq!(groups[1]["group"], "treatment");
    assert_eq!(groups[1]["sessions"], json!({"claude-sonnet-5/medium": 1}));
    assert_eq!(groups[1]["task_rework"], 1);
    assert_eq!(groups[1]["task_rework_rate"], 100.0);
    // Each run reads its session next to its prediction; task 3 is outside.
    let runs = stats["runs"].as_array().unwrap();
    let of = |task: i64| runs.iter().find(|run| run["task_id"] == task).unwrap();
    assert_eq!(of(2)["worker_model"], "claude-sonnet-5");
    assert_eq!(of(2)["trial_group"], "treatment");
    assert_eq!(of(3)["worker_model"], "claude-opus-5-5");
    assert_eq!(of(3)["worker_effort"], "medium");
    assert_eq!(of(3)["trial_group"], Value::Null);
}

/// Task 892: a run on Codex takes no group and no turn of the trial, its
/// claim names no Claude model (the step is `ladder_model`, and why no
/// model is known is `model_unknown`), and `stats` leaves it out of the
/// groups and of the host's Claude version.
#[test]
fn a_codex_run_is_outside_the_trial() {
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let codex = queue
        .add(NewTask {
            title: "b".into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["true".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: Some(dagq::domain::Provider::Codex),
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            wait_for_build: false,
        })
        .unwrap();
    queue
        .transition(codex.id(), TaskAction::BypassReview)
        .unwrap();
    add_ready_task(&mut queue, "c", &[]);
    add_ready_task(&mut queue, "d", &[]);
    let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
    for (task, tokens) in [(1, 10), (2, 10), (3, 10), (4, 900)] {
        predict(&db, task, "mechanical", tokens);
    }
    let on = WorkerTrial {
        enabled: true,
        window: 2,
    };
    for task in 1..=3 {
        let ClaimOutcome::Claimed { run } = queue
            .claim_for_supervisor_in_order(
                &base,
                &LeaseToken::new("t"),
                &[TaskId::new(task)],
                Some(&json!({"claude_version": "2.1.0", "codex_version": "0.46.0"})),
                &on,
                &dagq::domain::provider_switch::WorkerRoute::direct(
                    &dagq::domain::worker::Worker::ALL,
                ),
            )
            .unwrap()
        else {
            panic!("nothing to claim");
        };
        assert_eq!(run.task_id(), TaskId::new(task));
        queue
            .record_runtime_event(
                run.id(),
                EventKind::RunIntegrated,
                json!({"status": "integrated"}),
            )
            .unwrap();
    }
    assert_eq!(
        session_of(&claimed(&mut queue, 1)),
        (json!("claude-opus-5-5"), json!("medium"), json!("control"))
    );
    let on_codex = claimed(&mut queue, 2);
    assert_eq!(on_codex["provider"], "codex");
    assert_eq!(
        session_of(&on_codex),
        (Value::Null, json!("medium"), Value::Null)
    );
    assert_eq!(on_codex["ladder_model"], "claude-opus-5-5");
    assert_eq!(
        on_codex["model_unknown"],
        dagq::domain::worker_model::CODEX_MODEL_UNKNOWN
    );
    assert_eq!(on_codex.get("trial_percentile"), None);
    // The Codex run took no turn: the next subject is the treatment.
    assert_eq!(
        session_of(&claimed(&mut queue, 3)),
        (
            json!("claude-sonnet-5"),
            json!("medium"),
            json!("treatment")
        )
    );
    let stats = common::cli::ok(&db, &["stats", "--full"]);
    let groups = stats["trial_groups"].as_array().unwrap();
    let runs: Vec<Value> = groups.iter().map(|group| group["runs"].clone()).collect();
    assert_eq!(runs, [json!(1), json!(1)], "{groups:?}");
    let rows = stats["runs"].as_array().unwrap();
    let of = |task: i64| rows.iter().find(|run| run["task_id"] == task).unwrap();
    assert_eq!(of(2)["worker_model"], Value::Null);
    assert_eq!(of(2)["trial_group"], Value::Null);
    assert_eq!(of(2)["claude_version"], Value::Null);
    assert_eq!(of(1)["claude_version"], "2.1.0");
    assert_eq!(of(1)["worker_model"], "claude-opus-5-5");
}
