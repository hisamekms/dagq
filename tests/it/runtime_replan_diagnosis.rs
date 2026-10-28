//! Runtime tests: the runtime's detection of a task whose reviews keep
//! failing and the read-only diagnosis of its replanning operation
//! (docs/plans/task-replanning.md, "入口と診断" and "回数・抑制・上限と観測").
//! The run's reviews end in a `concern` of low confidence that asks a
//! person, the last of the reviews that did not pass: under the default
//! threshold the run is sent back twice first, under `non_pass_reviews = 1`
//! that one review is enough. The task becomes a candidate, the diagnosis
//! job (the test reviewer's recovery job) prints its verdict, and the
//! supervisor goes on, parks the run for the replanning or asks a person.
//! Which verdict does what is the unit tests' (`domain::replan_detect`);
//! these keep the wiring, the restarts and the CLI's counts.
use crate::runtime_review::revising_agent;
use crate::runtime_support;

use dagq::domain::replan_detect::{self, Detection, ReplanConfig};
use dagq::domain::task_replan::{DiagnosisChoice, ReplanOperation, ReplanOrigin, ReplanStatus};
use dagq::domain::{AskKind, AskReason};
use runtime_support::*;

/// Turn the detection on in the repository's `dagq.toml`, with `extra`
/// keys of `[replan]`.
fn enable_replan(repo: &Path, extra: &str) {
    fs::write(
        repo.join("dagq.toml"),
        format!("[replan]\nenabled = true\n{extra}"),
    )
    .unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-q", "-m", "detect long-running tasks"]);
}

/// A diagnosis job's script: its verdict of `choice` for `causes`.
fn diagnosis(choice: &str, causes: &[&str], confidence: &str) -> String {
    let json = json!({"choice": choice, "causes": causes, "confidence": confidence,
                      "reason": "the findings come back on two independent parts",
                      "progress": "two findings fixed, one left", "remaining": 1});
    format!("printf '%s\\n' '{json}'")
}

/// The `concern` of low confidence that asks a person.
fn asking_concern() -> String {
    let concern = json!({"verdict": "concern", "reasons": ["the mechanism again"],
                         "summary": "judged", "recommendation": "send_back",
                         "confidence": "low", "reason_category": null});
    format!("printf '%s\\n' '{concern}'")
}

/// Reviews that send the run back twice, then the `concern` that asks a
/// person: three reviews that did not pass. Each diagnosis job runs the
/// next of `diagnoses`.
fn reviews_with(diagnoses: &[String]) -> TestReviewer {
    TestReviewer::new(&[
        verdict("revise", &["the mechanism is not fixed"], "not yet"),
        verdict("revise", &["the mechanism is still not fixed"], "not yet"),
        asking_concern(),
    ])
    .with_triages(diagnoses)
}

/// A task whose one review, the `concern` that asks a person, makes it a
/// candidate under `non_pass_reviews = 1` (or detects nothing with the
/// detection off, `enabled` false), diagnosed by `diagnoses` in turn,
/// supervised once.
fn one_review(
    enabled: bool,
    diagnoses: &[String],
) -> (Fixture, PathBuf, PathBuf, TestWorkspace, TestReviewer) {
    let (dir, repo, db) = fixture();
    fs::write(
        repo.join("dagq.toml"),
        format!("[replan]\nenabled = {enabled}\nnon_pass_reviews = 1\n"),
    )
    .unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-q", "-m", "detect long-running tasks"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[asking_concern()]).with_triages(diagnoses);
    let outcome = supervise_reviewed_owned(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    (dir, repo, db, backend, reviewer)
}

/// The task's one replanning operation.
fn operation(db: &Path) -> ReplanOperation {
    let ops = SqliteQueue::open(db)
        .unwrap()
        .replan_operations(true)
        .unwrap();
    assert_eq!(ops.len(), 1, "{ops:?}");
    ops[0].clone()
}

/// The ids of the task's events of `kind`.
fn ids_of(detail: &dagq::domain::TaskDetail, kind: &str) -> Vec<i64> {
    detail
        .events
        .iter()
        .filter(|e| e.kind.as_str() == kind)
        .map(|e| e.id.as_i64())
        .collect()
}

/// Acceptance (the threshold reached, the window's evidence, the existing
/// review limit kept, a restart): the third review that did not pass makes
/// the task a candidate of origin `runtime` on those reviews, its
/// diagnosis job reads them and goes on, and the person's `approve_landing`
/// the review opened stays theirs. Supervising again (a restart) neither
/// detects nor diagnoses the same window again, and the CLI's counts are
/// those the detection read.
#[test]
fn a_task_whose_reviews_keep_failing_is_diagnosed_once_and_goes_on() {
    let (_dir, repo, db) = fixture();
    enable_replan(&repo, "");
    let backend = TestWorkspace::new(&db, false, &revising_agent(2));
    let reviewer = reviews_with(&[diagnosis("continue", &["local_defect"], "high")]);
    let outcome = supervise_reviewed_owned(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let reviews = ids_of(&detail, "review_finished");
    assert_eq!(reviews.len(), 3);
    let op = operation(&db);
    assert_eq!(op.origin, ReplanOrigin::Runtime);
    assert_eq!(op.status, ReplanStatus::Continuing, "{op:?}");
    let evidence: Vec<i64> = op.diagnosis.evidence.iter().map(|id| id.as_i64()).collect();
    assert_eq!(evidence, reviews);
    assert_eq!(op.diagnosis.choice, Some(DiagnosisChoice::Continue));
    assert_eq!(
        op.diagnosis.decided_by.as_deref(),
        Some(replan_detect::DECIDED_BY_RUNTIME)
    );
    assert_eq!(op.diagnosis.remaining, Some(1));
    assert!(op.diagnosis.window_end.unwrap().as_i64() > *reviews.last().unwrap());
    let started = payloads(&detail, "replan_diagnosis_started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["attempt"], 1);
    let diagnosed = payloads(&detail, "replan_diagnosed");
    assert_eq!(diagnosed.len(), 1);
    assert_eq!(diagnosed[0]["outcome"], "continue", "{}", diagnosed[0]);
    // The job read the window's findings and the turns by cause.
    let prompts = reviewer.triage_prompts();
    assert_eq!(prompts.len(), 1);
    for part in [
        "replanning diagnosis",
        "the mechanism is still not fixed",
        "3 non-pass reviews",
        "by cause: initial 1, review 2",
    ] {
        assert!(prompts[0].0.contains(part), "{part} in {}", prompts[0].0);
    }
    // The review's own ask is the person's still.
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::ApproveLanding);

    // A restart reads the same records: nothing is detected twice.
    let outcome = supervise_reviewed_owned(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(operation(&db).id, op.id);
    assert_eq!(payloads(&detail, "replan_diagnosis_started").len(), 1);
    let config = ReplanConfig {
        enabled: true,
        ..ReplanConfig::default()
    };
    let events = queue.task_events(TaskId::new(1)).unwrap();
    let Detection::Below { window } =
        replan_detect::detect(&config, &events, std::slice::from_ref(&op))
    else {
        panic!("the diagnosed window is a candidate again");
    };
    assert!(window.after_diagnosis);

    // The CLI counts as the detection does.
    let counted = replan_detect::count_turns(&events);
    let run = detail.runs[0].id().to_string();
    let timeline = common_ok(&db, &["timeline", &run]);
    assert_eq!(timeline["turns"], json!(counted.total), "{timeline}");
    let steps: Vec<&str> = timeline["replans"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["kind"].as_str().unwrap())
        .collect();
    assert!(steps.starts_with(&["replan_requested"]), "{steps:?}");
    assert!(steps.contains(&"replan_diagnosed"), "{steps:?}");
    let status = common_ok(&db, &["status"]);
    let counts = &status["replans"]["counts"];
    assert_eq!(counts["by_origin"]["runtime"], 1, "{counts}");
    assert_eq!(counts["by_choice"]["continue"], 1, "{counts}");
    assert_eq!(counts["fenced"], 0, "{counts}");
    let stats = common_ok(&db, &["stats", "--full"]);
    let replans = &stats["replans"];
    assert_eq!(replans["opened"]["runtime"], 1, "{replans}");
    assert_eq!(replans["diagnosed"]["continue"], 1, "{replans}");
    assert_eq!(replans["turns"]["non_pass"], 3, "{replans}");
    assert_eq!(replans["turns"]["by_cause"], json!(counted.total.by_cause));
}

/// Acceptance (the formal replanning, the existing review limit): a sure
/// replanning for a cause of the unit of work moves the operation past the
/// pause's fence with a planning request, and the park takes it from
/// there: the run waiting for the person's `approve_landing` is parked
/// with its work saved and that ask closed, and the operation is with the
/// planner of its request.
#[test]
fn a_diagnosed_replanning_parks_the_run_and_closes_the_reviews_ask() {
    let (_dir, repo, db, _backend, _reviewer) =
        one_review(true, &[diagnosis("replan", &["oversized_unit"], "high")]);
    let op = operation(&db);
    assert!(op.status.hands_to_planner(), "{op:?}");
    assert!(op.snapshot_id.is_some());
    assert!(op.request_id.is_some(), "the planner's request is recorded");
    assert_eq!(op.diagnosis.choice, Some(DiagnosisChoice::Replan));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    let diagnosed = payloads(&detail, "replan_diagnosed");
    assert_eq!(diagnosed[0]["outcome"], "park", "{}", diagnosed[0]);
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    let status = common_ok(&db, &["status"]);
    let open = &status["replans"]["open"][0];
    assert_eq!(open["guards"], true, "{open}");
    assert_eq!(open["lineage_auto_parks"], 1, "{open}");
    assert_eq!(open["turns"]["non_pass"], 1, "{open}");
    assert_eq!(status["replans"]["counts"]["fenced"], 1);
    assert_eq!(status["replans"]["detection"]["non_pass_reviews"], 1);

    // `[replan]` edited into one that cannot be read: a supervisor keeps
    // the settings it read last, so `status` and `doctor` show neither
    // settings nor a window, but the error; the turns are counted still.
    fs::write(repo.join("dagq.toml"), "[replan]\nenabled = 3\n").unwrap();
    for command in ["status", "doctor"] {
        let replans = &common_ok(&db, &[command])["replans"];
        assert_eq!(replans["detection"], Value::Null, "{command}: {replans}");
        assert!(
            replans["detection_error"]
                .as_str()
                .is_some_and(|error| error.contains("keeps the settings it read last")),
            "{command}: {replans}"
        );
        assert_eq!(replans["open"][0]["window"], Value::Null, "{command}");
        assert_eq!(replans["open"][0]["turns"]["non_pass"], 1, "{command}");
    }
}

/// Acceptance (external causes are not split): a replanning only for
/// conflicts goes on instead.
#[test]
fn a_replanning_only_for_external_causes_goes_on() {
    let (_dir, _repo, db, _backend, _reviewer) =
        one_review(true, &[diagnosis("replan", &["conflict"], "high")]);
    let op = operation(&db);
    assert_eq!(op.status, ReplanStatus::Continuing, "{op:?}");
    assert!(
        op.status_reason
            .as_deref()
            .unwrap()
            .contains("a smaller unit of work would meet as well (conflict)"),
        "{op:?}"
    );
    // The runtime's choice is recorded, and it parked nothing.
    assert_eq!(op.diagnosis.choice, Some(DiagnosisChoice::Continue));
    let lineage = SqliteQueue::open(&db)
        .unwrap()
        .replan_lineage(TaskId::new(1))
        .unwrap();
    assert_eq!(replan_detect::auto_parks(&lineage), 0);
}

/// Acceptance (a person's decision, a stale verdict): a diagnosis of low
/// confidence asks a person in a `decide` ask about the task (not holding
/// its run), recommending its choice, and the person's `replan` parks the
/// run, recorded as the person's decision. A verdict whose run's HEAD
/// moved while its job ran opens no ask: it is discarded and the next job
/// diagnoses the run again.
#[test]
fn an_unsure_diagnosis_asks_a_person_whose_replan_parks_the_run() {
    let unsure = diagnosis("replan", &["oversized_unit"], "low");
    // The job runs in the run directory's `replan_diagnosis/`, beside the
    // run's worktree, whose HEAD it moves before it gives its verdict.
    let moving = format!(
        "for worktree in ../worktree-*/; do git -C \"$worktree\" -c user.name=t -c user.email=t@example.com commit -q --allow-empty -m moved || exit 1; done && {unsure}"
    );
    let (_dir, repo, db, backend, reviewer) = one_review(true, &[moving, unsure]);
    let op = operation(&db);
    assert_eq!(op.status, ReplanStatus::Diagnosing, "{op:?}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "replan_diagnosis_started").len(), 2);
    let failed = payloads(&detail, "replan_diagnosis_failed");
    assert!(failed.is_empty(), "{failed:?}");
    let diagnosed = payloads(&detail, "replan_diagnosed");
    assert_eq!(diagnosed.len(), 2, "{diagnosed:?}");
    assert_eq!(diagnosed[0]["outcome"], "discarded", "{diagnosed:?}");
    assert!(
        diagnosed[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("HEAD moved")),
        "{diagnosed:?}"
    );
    assert!(diagnosed[0]["ask_id"].is_null(), "{diagnosed:?}");
    assert_eq!(diagnosed[1]["outcome"], "ask", "{diagnosed:?}");
    let asked: Vec<_> = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .filter(|ask| ask.asked_by == replan_detect::DIAGNOSIS_ASKER)
        .collect();
    assert_eq!(asked.len(), 1, "the stale verdict opened none: {asked:?}");
    let ask = asked[0].clone();
    assert_eq!(ask.kind, AskKind::Decide);
    assert_eq!(ask.task_id, Some(TaskId::new(1)));
    assert_eq!(ask.run_id, None, "the run is not held by it");
    assert_eq!(ask.options, ["continue", "replan"]);
    assert_eq!(ask.recommendation.as_deref(), Some("replan"));
    assert_eq!(ask.reason_category, AskReason::Scope);
    assert!(
        ask.question
            .contains("the diagnosis is not sure of its choice"),
        "{}",
        ask.question
    );
    queue.answer(ask.id, "replan").unwrap();
    let outcome = supervise_reviewed_owned(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let op = operation(&db);
    assert!(op.status.hands_to_planner(), "{op:?}");
    assert!(op.snapshot_id.is_some());
    assert_eq!(
        op.diagnosis.decided_by.as_deref(),
        Some(replan_detect::DECIDED_BY_PERSON)
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let detail = queue.show(TaskId::new(1)).unwrap();
    let answered = payloads(&detail, "ask_answered");
    assert!(
        answered
            .iter()
            .any(|a| a["ask_id"] == ask.id.as_i64() && a["runtime_delivers"] == true),
        "{answered:?}"
    );
}

/// Acceptance (a job that fails): a diagnosis job that prints no verdict
/// is started again up to its attempts, then a person is asked.
#[test]
fn a_diagnosis_that_never_gives_a_verdict_asks_a_person_after_its_attempts() {
    let unreadable = "printf 'no verdict here\\n'".to_owned();
    let (_dir, _repo, db, _backend, _reviewer) =
        one_review(true, &[unreadable.clone(), unreadable.clone(), unreadable]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "replan_diagnosis_started").len(), 3);
    assert_eq!(payloads(&detail, "replan_diagnosis_failed").len(), 3);
    let op = operation(&db);
    assert_eq!(op.status, ReplanStatus::Diagnosing);
    let ask = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.asked_by == replan_detect::DIAGNOSIS_ASKER)
        .expect("the diagnosis's ask");
    assert_eq!(ask.reason_category, AskReason::RecoveryFailed);
    assert!(
        ask.question
            .contains("the diagnosis job did not give a verdict"),
        "{}",
        ask.question
    );
}

/// Acceptance (disabled, the job's workspace): with `enabled = false`
/// nothing is detected and no diagnosis starts; the review's ask is the
/// person's as before. Once the detection is on, a run with no run
/// directory gets no diagnosis job. Nothing is written to the main
/// checkout, no provider starts, and a person decides in the diagnosis's
/// ask instead.
#[test]
fn a_run_without_a_run_directory_is_not_diagnosed_in_the_main_checkout() {
    let (_dir, repo, db, backend, off) = one_review(false, &[]);
    {
        let queue = SqliteQueue::open(&db).unwrap();
        assert!(queue.replan_operations(true).unwrap().is_empty());
        assert!(off.triage_prompts().is_empty());
        let asks = queue.asks(Default::default()).unwrap();
        assert_eq!(asks.len(), 1, "{asks:?}");
        assert_eq!(asks[0].kind, AskKind::ApproveLanding);
    }
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute("UPDATE task_runs SET run_dir=NULL WHERE task_id=1", [])
        .unwrap();
    enable_replan(&repo, "non_pass_reviews = 1\n");
    let reviewer = TestReviewer::new(&[asking_concern()]).with_triages(&[diagnosis(
        "continue",
        &["local_defect"],
        "high",
    )]);
    supervise_reviewed_owned(&db, &repo, &backend, &reviewer);
    assert!(
        !repo.join("replan_diagnosis").exists(),
        "nothing is written to the main checkout"
    );
    assert!(reviewer.triage_prompts().is_empty(), "no job started");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(payloads(&detail, "replan_diagnosis_started").is_empty());
    assert_eq!(operation(&db).status, ReplanStatus::Diagnosing);
    let ask = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.asked_by == replan_detect::DIAGNOSIS_ASKER)
        .expect("the diagnosis's ask");
    assert_eq!(ask.reason_category, AskReason::RecoveryFailed);
    assert!(
        ask.question.contains("the run has no run directory"),
        "{}",
        ask.question
    );
}

fn common_ok(db: &Path, args: &[&str]) -> Value {
    crate::common::cli::ok(db, args)
}
