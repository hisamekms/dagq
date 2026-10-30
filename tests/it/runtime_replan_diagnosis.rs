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

/// The events of the queue as `events --full` shows them, with their
/// actors.
fn events_full(db: &Path) -> Vec<Value> {
    common_ok(
        db,
        &[
            "events", "--after", "0", "--all", "--full", "--limit", "10000",
        ],
    )["events"]
        .as_array()
        .unwrap()
        .clone()
}

/// The actor id of diagnosis job `attempt` of `run`.
fn diagnosis_job(run: &RunId, attempt: u64) -> String {
    format!("recovery-job:{run}:replan_diagnosis:{attempt}")
}

/// What the supervisor wrote applying the verdict of diagnosis job `job`:
/// each event's kind and, for a `replan_status_changed`, the status it
/// moved to. Every one of them, and every `replan_diagnosed` of its
/// attempt, is the supervisor's with the job as `requested_by`.
fn applied_for(events: &[Value], job: &str, attempt: u64) -> Vec<String> {
    for event in events
        .iter()
        .filter(|e| e["kind"] == "replan_diagnosed" && e["payload"]["attempt"] == json!(attempt))
    {
        assert_eq!(event["actor"]["requested_by"], json!(job), "{event}");
    }
    events
        .iter()
        .filter(|e| e["actor"]["requested_by"] == json!(job))
        .map(|e| {
            assert_eq!(e["actor"]["role"], "supervisor", "{e}");
            let kind = e["kind"].as_str().unwrap();
            match e["payload"]["to"].as_str() {
                Some(to) if kind == "replan_status_changed" => format!("{kind} {to}"),
                _ => kind.to_owned(),
            }
        })
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
    // The verdict is applied as the supervisor's at the job's request.
    let job = diagnosis_job(detail.runs[0].id(), 1);
    assert_eq!(
        applied_for(&events_full(&db), &job, 1),
        ["replan_status_changed continuing", "replan_diagnosed"]
    );
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
    let job = diagnosis_job(detail.runs[0].id(), 1);
    assert_eq!(
        applied_for(&events_full(&db), &job, 1),
        [
            "request_recorded",
            "replan_status_changed pause_requested",
            "replan_diagnosed"
        ]
    );
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

/// Acceptance (the event's actor of an `ask` verdict): a diagnosis job
/// that chooses `ask` itself opens the person's `decide` ask, and the
/// claim, the ask and its `replan_diagnosed` are the supervisor's at the
/// job's request.
#[test]
fn a_diagnosis_that_asks_opens_the_ask_at_its_jobs_request() {
    let (_dir, _repo, db, _backend, _reviewer) =
        one_review(true, &[diagnosis("ask", &["oversized_unit"], "high")]);
    let op = operation(&db);
    assert_eq!(op.status, ReplanStatus::Diagnosing, "{op:?}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let diagnosed = payloads(&detail, "replan_diagnosed");
    assert_eq!(diagnosed.len(), 1, "{diagnosed:?}");
    assert_eq!(diagnosed[0]["outcome"], "ask", "{diagnosed:?}");
    let asked: Vec<_> = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .filter(|ask| ask.asked_by == replan_detect::DIAGNOSIS_ASKER)
        .collect();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0].recommendation, None);
    let job = diagnosis_job(detail.runs[0].id(), 1);
    assert_eq!(
        applied_for(&events_full(&db), &job, 1),
        [
            "replan_status_changed diagnosing",
            "ask_opened",
            "replan_diagnosed"
        ]
    );
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
    // The stale verdict's record and the ask, with the claim it makes, are
    // written at the request of the job that gave each.
    let run = detail.runs[0].id().clone();
    let events = events_full(&db);
    assert_eq!(
        applied_for(&events, &diagnosis_job(&run, 1), 1),
        ["replan_diagnosed"]
    );
    assert_eq!(
        applied_for(&events, &diagnosis_job(&run, 2), 2),
        [
            "replan_status_changed diagnosing",
            "ask_opened",
            "replan_diagnosed"
        ]
    );
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
    // The answer is applied as any other answer is: the supervisor's,
    // asked by no job.
    let events = events_full(&db);
    let moved = events
        .iter()
        .find(|e| e["kind"] == "replan_status_changed" && e["payload"]["to"] == "pause_requested")
        .unwrap();
    assert_eq!(moved["actor"]["role"], "supervisor", "{moved}");
    assert_eq!(moved["actor"]["requested_by"], Value::Null, "{moved}");
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
            .contains("the diagnosis jobs failed: none started"),
        "{}",
        ask.question
    );
}

/// Acceptance (an apply that fails): a verdict whose apply ends in an
/// error (here the queue's write of the ask the verdict asks for fails,
/// while the events can be written) is recorded as a failed job with its
/// verdict, not discarded, and once the failures reach the limit a person
/// is asked, told that the verdicts could not be applied, instead of a job
/// started again.
#[test]
fn a_verdict_whose_apply_keeps_failing_asks_a_person_after_its_attempts() {
    let (_dir, repo, db) = fixture();
    enable_replan(&repo, "non_pass_reviews = 1\n");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_verdict_asks BEFORE INSERT ON asks
             WHEN NEW.asked_by = 'replan-diagnosis' AND NEW.reason_category = 'scope'
             BEGIN SELECT RAISE(ABORT, 'ask write failed'); END",
        )
        .unwrap();
    let asking = diagnosis("ask", &["oversized_unit"], "high");
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[asking_concern()]).with_triages(&[
        asking.clone(),
        asking.clone(),
        asking.clone(),
        asking,
    ]);
    let outcome = supervise_reviewed_owned(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(reviewer.triage_prompts().len(), 3, "no job past the limit");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let started = payloads(&detail, "replan_diagnosis_started");
    let failed = payloads(&detail, "replan_diagnosis_failed");
    assert_eq!((started.len(), failed.len()), (3, 3), "{failed:?}");
    for (start, failure) in started.iter().zip(&failed) {
        assert_eq!(failure["started_generation"], start["generation"]);
        assert_eq!(failure["verdict"]["choice"], "ask", "{failure}");
        assert!(
            failure["error"]
                .as_str()
                .unwrap()
                .contains("ask write failed"),
            "{failure}"
        );
    }
    assert!(
        payloads(&detail, "replan_diagnosed")
            .iter()
            .all(|d| d["outcome"] != "discarded"),
        "no verdict discarded"
    );
    assert!(queue.running_diagnosis_owners().unwrap().is_empty());
    let ask = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.asked_by == replan_detect::DIAGNOSIS_ASKER)
        .expect("the diagnosis's ask");
    assert_eq!(ask.reason_category, AskReason::RecoveryFailed);
    assert!(
        ask.question
            .contains("job 3 gave a verdict (ask) that could not be applied"),
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

/// `dagq hold` or `dagq release` of task 1 as a person at a plain
/// terminal.
fn as_person(db: &Path, args: &[&str]) {
    let output = crate::common::cli::invoke_with(&[], db, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The outcomes of the task's `replan_diagnosed`, in order.
fn outcomes(db: &Path) -> Vec<String> {
    let detail = SqliteQueue::open(db).unwrap().show(TaskId::new(1)).unwrap();
    payloads(&detail, "replan_diagnosed")
        .iter()
        .map(|p| p["outcome"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// Acceptance (the hold before the start): a task held before its
/// diagnosis gets no job: no provider runs, no `replan_diagnosis_started`
/// is written and no ask opens, the hold recorded as what keeps it. Once
/// it is released, the next supervisor diagnoses it.
#[test]
fn a_held_task_gets_no_diagnosis_job_until_its_release() {
    let (_dir, repo, db, backend, _off) = one_review(false, &[]);
    as_person(&db, &["hold", "1", "--reason", "the login expired"]);
    enable_replan(&repo, "non_pass_reviews = 1\n");
    let reviewer = TestReviewer::new(&[asking_concern()]).with_triages(&[diagnosis(
        "continue",
        &["local_defect"],
        "high",
    )]);
    let outcome = supervise_reviewed_owned(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(reviewer.triage_prompts().is_empty(), "no job started");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(payloads(&detail, "replan_diagnosis_started").is_empty());
    assert_eq!(operation(&db).status, ReplanStatus::Candidate);
    assert!(
        queue
            .asks(Default::default())
            .unwrap()
            .iter()
            .all(|ask| ask.asked_by != replan_detect::DIAGNOSIS_ASKER)
    );
    let deferred = payloads(&detail, "hold_deferred");
    assert!(
        deferred.iter().any(|d| d["step"] == "recover"),
        "{deferred:?}"
    );

    as_person(&db, &["release", "1"]);
    let outcome = supervise_reviewed_owned(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(reviewer.triage_prompts().len(), 1);
    assert_eq!(operation(&db).status, ReplanStatus::Continuing);
    assert_eq!(outcomes(&db), ["continue"]);
}

/// Acceptance (a hold after the start): a task held while its diagnosis
/// job runs (the job's script holds it under the store, as
/// `runtime_task_hold::the_supervisor_keeps_a_verdict_whose_task_was_held_while_the_job_ran`
/// does) gets the verdict kept, neither applied nor asked about: the
/// operation stays diagnosing. Once the hold is released, the verdict is
/// applied on its premise checked again (the run rested meanwhile), and no
/// second job runs.
#[test]
fn a_verdict_reaching_a_task_held_meanwhile_is_kept_and_applied_after_the_release() {
    let (_dir, repo, db, backend, _off) = one_review(false, &[]);
    enable_replan(&repo, "non_pass_reviews = 1\n");
    let hold_now = format!(
        "sqlite3 -cmd '.timeout 5000' {} {} || exit 97",
        shell_path(&db),
        dagq::infrastructure::adapters::shell_quote(
            "INSERT INTO task_holds(task_id, reason, by_role, by_id, created_at, settled_at)
             VALUES (1, 'held while the diagnosis ran', 'user', 'user', 0, 0);"
        )
    );
    let job = format!(
        "{hold_now} && {}",
        diagnosis("continue", &["local_defect"], "high")
    );
    let reviewer = TestReviewer::new(&[asking_concern()]).with_triages(&[job]);
    let outcome = supervise_reviewed_owned(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let op = operation(&db);
    assert_eq!(op.status, ReplanStatus::Diagnosing, "{op:?}");
    assert_eq!(op.diagnosis.choice, None, "nothing applied");
    assert_eq!(outcomes(&db), [replan_detect::HELD_OUTCOME]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kept = detail
        .events
        .iter()
        .find(|e| e.kind.as_str() == "replan_diagnosed")
        .unwrap()
        .clone();
    assert_eq!(
        kept.payload["verdict"]["choice"], "continue",
        "{}",
        kept.payload
    );
    assert_eq!(
        kept.payload["generation"], op.generation,
        "{}",
        kept.payload
    );
    assert!(
        queue
            .asks(Default::default())
            .unwrap()
            .iter()
            .all(|ask| ask.asked_by != replan_detect::DIAGNOSIS_ASKER)
    );

    as_person(&db, &["release", "1"]);
    let outcome = supervise_reviewed_owned(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let op = operation(&db);
    assert_eq!(op.status, ReplanStatus::Continuing, "{op:?}");
    assert_eq!(op.diagnosis.choice, Some(DiagnosisChoice::Continue));
    assert_eq!(outcomes(&db), [replan_detect::HELD_OUTCOME, "continue"]);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let applied = &payloads(&detail, "replan_diagnosed")[1];
    assert_eq!(applied["kept_id"], kept.id.as_i64(), "{applied}");
    assert_eq!(payloads(&detail, "replan_diagnosis_started").len(), 1);
}

/// Acceptance (a person's answer under a hold): the answer to the
/// diagnosis's `decide` ask given while the task is held is not applied:
/// the ask stays answered and open, the operation diagnosing. Once the
/// hold is released, the answer is applied as the person's decision.
#[test]
fn an_answer_to_the_diagnosis_of_a_held_task_is_applied_after_the_release() {
    let unsure = diagnosis("replan", &["oversized_unit"], "low");
    let (_dir, repo, db, backend, reviewer) = one_review(true, &[unsure]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.asked_by == replan_detect::DIAGNOSIS_ASKER)
        .expect("the diagnosis's ask");
    as_person(&db, &["hold", "1", "--reason", "the login expired"]);
    queue.answer(ask.id, "continue").unwrap();
    let outcome = supervise_reviewed_owned(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(operation(&db).status, ReplanStatus::Diagnosing);
    let read = queue.read_ask(ask.id).unwrap();
    assert!(
        read.answered_at.is_some() && read.closed_at.is_none(),
        "{read:?}"
    );

    as_person(&db, &["release", "1"]);
    let outcome = supervise_reviewed_owned(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let op = operation(&db);
    assert_eq!(op.status, ReplanStatus::Continuing, "{op:?}");
    assert_eq!(
        op.diagnosis.decided_by.as_deref(),
        Some(replan_detect::DECIDED_BY_PERSON)
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
}

/// Another supervisor of `pid`, not registered: its token and its
/// process's start.
fn stand_in(pid: u32) -> replan_detect::JobOwner {
    replan_detect::JobOwner {
        pid,
        token: Some("other".into()),
        process_start: dagq::application::ProcessControl::start_identity(
            &dagq::infrastructure::adapters::SystemProcesses,
            pid,
        ),
    }
}

/// Acceptance (two supervisors, an owner gone): a diagnosis job another
/// supervisor alive started is left to it, so no second job starts in the
/// queue; once that supervisor is gone, the next one claims the operation
/// again and its job, the next attempt, gives the verdict, the job of the
/// supervisor that is gone counted as no failure.
#[test]
fn a_diagnosis_another_supervisor_runs_is_left_to_it_and_taken_over_once_it_is_gone() {
    let (_dir, repo, db, backend, _off) = one_review(false, &[]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].id().clone();
    let op = queue
        .record_replan(
            &dagq::domain::task_replan::NewReplan {
                run: run.clone(),
                reason: "the reviews keep failing".into(),
                evidence: Vec::new(),
            },
            ReplanOrigin::Runtime,
            "supervisor",
            "supervisor",
        )
        .unwrap()
        .operation;
    let mut other = crate::common::KillOnDrop::new(
        std::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .unwrap(),
        "the other supervisor's stand-in",
    );
    // Not registered, it is told by its pid and its process's start.
    let claimed = queue
        .start_replan_diagnosis(
            (op.id, op.generation),
            "its diagnosis 1 starts",
            &run,
            (json!({"attempt": 1}), &stand_in(other.child().id()), &[]),
        )
        .unwrap();
    enable_replan(&repo, "non_pass_reviews = 1\n");
    let reviewer = TestReviewer::new(&[asking_concern()]).with_triages(&[diagnosis(
        "continue",
        &["local_defect"],
        "high",
    )]);
    let outcome = supervise_reviewed_owned(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(reviewer.triage_prompts().is_empty(), "no second job");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "replan_diagnosis_started").len(), 1);
    let held = operation(&db);
    assert_eq!(
        (held.status, held.generation),
        (ReplanStatus::Diagnosing, claimed.generation)
    );

    drop(other);
    let outcome = supervise_reviewed_owned(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(reviewer.triage_prompts().len(), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let started = payloads(&detail, "replan_diagnosis_started");
    assert_eq!(started.len(), 2, "{started:?}");
    assert_eq!(started[1]["attempt"], 2);
    assert_eq!(started[1]["generation"], claimed.generation + 1);
    assert!(payloads(&detail, "replan_diagnosis_failed").is_empty());
    let diagnosed = payloads(&detail, "replan_diagnosed");
    assert_eq!(diagnosed.len(), 1, "{diagnosed:?}");
    assert_eq!(diagnosed[0]["outcome"], "continue", "{}", diagnosed[0]);
    assert_eq!(operation(&db).status, ReplanStatus::Continuing);
}

/// Acceptance (one job queue-wide, the provider once): while a supervisor
/// alive runs the diagnosis job of another operation, closed since (the
/// run went on without it), a supervisor's check of its own operation lets
/// the start go on, and the queue's transaction of the start refuses it:
/// no provider starts, the operation is not claimed and no start of it is
/// recorded.
#[test]
fn a_start_the_queues_transaction_refuses_starts_no_provider() {
    use dagq::domain::task_replan::{NewReplan, ReplanMove};
    let (_dir, repo, db, backend, _off) = one_review(false, &[]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].id().clone();
    let new = NewReplan {
        run: run.clone(),
        reason: "the reviews keep failing".into(),
        evidence: Vec::new(),
    };
    let first = queue
        .record_replan(&new, ReplanOrigin::Runtime, "supervisor", "supervisor")
        .unwrap()
        .operation;
    let mut other = crate::common::KillOnDrop::new(
        std::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .unwrap(),
        "the other supervisor's stand-in",
    );
    let owner = stand_in(other.child().id());
    let claimed = queue
        .start_replan_diagnosis(
            (first.id, first.generation),
            "its diagnosis 1 starts",
            &run,
            (json!({"attempt": 1}), &owner, &[]),
        )
        .unwrap();
    queue
        .move_replan(
            first.id,
            claimed.generation,
            &ReplanMove {
                to: ReplanStatus::Continuing,
                reason: "the run went on during its diagnosis".into(),
                snapshot_id: None,
                diagnosis: None,
            },
        )
        .unwrap();
    let second = queue
        .record_replan(&new, ReplanOrigin::Runtime, "supervisor", "supervisor")
        .unwrap()
        .operation;
    assert_ne!(second.id, first.id);
    enable_replan(&repo, "non_pass_reviews = 1\n");
    let reviewer = TestReviewer::new(&[asking_concern()]).with_triages(&[diagnosis(
        "continue",
        &["local_defect"],
        "high",
    )]);
    let outcome = supervise_reviewed_owned(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(reviewer.triage_prompts().is_empty(), "no provider started");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let started = payloads(&detail, "replan_diagnosis_started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["operation_id"], first.id.as_i64());
    let unclaimed = queue.replan_operation(second.id).unwrap();
    assert_eq!(
        (unclaimed.status, unclaimed.generation),
        (ReplanStatus::Candidate, second.generation)
    );
    let owners: Vec<_> = queue
        .running_diagnosis_owners()
        .unwrap()
        .into_iter()
        .map(|o| o.owner)
        .collect();
    assert_eq!(owners, [owner]);
    drop(other);
}

/// Acceptance (a start that fails): a diagnosis job whose provider's
/// process does not start after its start was recorded ends as failed,
/// tied to that start, so no job holds the queue; the next pass claims the
/// operation again and its job, the next attempt, gives the verdict, the
/// provider started once and one failure counted.
#[test]
fn a_diagnosis_job_that_does_not_start_ends_failed_and_the_next_attempt_diagnoses() {
    let (_dir, _repo, db, _backend, reviewer) = one_review(
        true,
        &[
            UNSTARTABLE_JOB.to_owned(),
            diagnosis("continue", &["local_defect"], "high"),
        ],
    );
    assert_eq!(reviewer.triage_prompts().len(), 1, "one provider started");
    let queue = SqliteQueue::open(&db).unwrap();
    let events = queue.task_events(TaskId::new(1)).unwrap();
    let of = |kind: &str| -> Vec<&Value> {
        events
            .iter()
            .filter(|e| e.kind.as_str() == kind)
            .map(|e| &e.payload)
            .collect()
    };
    let started = of("replan_diagnosis_started");
    assert_eq!(started.len(), 2, "{started:?}");
    assert_eq!(
        (started[0]["attempt"].clone(), started[1]["attempt"].clone()),
        (json!(1), json!(2))
    );
    let failed = of("replan_diagnosis_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["started_generation"], started[0]["generation"]);
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap()
            .contains("the diagnosis job could not start"),
        "{}",
        failed[0]
    );
    let diagnosed = of("replan_diagnosed");
    assert_eq!(diagnosed.len(), 1, "{diagnosed:?}");
    assert_eq!(diagnosed[0]["outcome"], "continue", "{}", diagnosed[0]);
    assert_eq!(diagnosed[0]["started_generation"], started[1]["generation"]);
    assert!(queue.running_diagnosis_owners().unwrap().is_empty());
    assert_eq!(operation(&db).status, ReplanStatus::Continuing);
}

/// Acceptance (one settlement of one verdict, on the supervisor's path): a
/// park the queue refuses at the lineage's limit turns into an ask written
/// under the job's generation, as the move was. Here, once the verdict is
/// decided on (a park), the lineage reaches its limit and another
/// supervisor records the failure of the same verdict, claiming the
/// operation again: the park and then its ask are refused, the verdict is
/// discarded and no ask opens, the failure counts once, and the next job
/// goes on from there.
#[test]
fn a_refused_parks_ask_after_another_supervisors_failure_of_its_verdict_opens_none() {
    let (_dir, repo, db) = fixture();
    enable_replan(&repo, "non_pass_reviews = 1\nmax_auto_parks = 1\n");
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[asking_concern()]).with_triages(&[
        diagnosis("replan", &["oversized_unit"], "high"),
        diagnosis("continue", &["local_defect"], "high"),
    ]);
    let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let at_decision = db.clone();
    let mut options = supervise_options(4, true);
    options.pauses = Some(runtime::PausesPort(Arc::new(move |at, run| {
        if at != runtime::PausePoint::VerdictDecided || reached.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut queue = SqliteQueue::open(&at_decision).unwrap();
        let open = queue.replan_operations(false).unwrap();
        assert_eq!(open.len(), 1, "{open:?}");
        let op = &open[0];
        assert_eq!(op.status, ReplanStatus::Diagnosing, "{op:?}");
        // An earlier park of the lineage without a person ended meanwhile.
        let conn = Connection::open(&at_decision).unwrap();
        conn.busy_timeout(crate::common::STEP_LIMIT).unwrap();
        conn.execute(
            "INSERT INTO replan_operations(task_id, run_id, goal_id, origin, status, reason,
                   conditions, task_version, goal_acceptance_version, diagnosis, requested_by,
                   requested_by_id, created_at, updated_at, closed_at)
                 SELECT task_id, run_id, goal_id, origin, 'withdrawn', 'an earlier park',
                   conditions, task_version, goal_acceptance_version,
                   '{\"evidence\":[],\"choice\":\"replan\",\"decided_by\":\"runtime\"}',
                   requested_by, requested_by_id, created_at, updated_at, updated_at
                 FROM replan_operations WHERE id=?1",
            [op.id.as_i64()],
        )
        .unwrap();
        // Another supervisor fails the same verdict under the job's
        // generation.
        queue
            .fail_replan_diagnosis(
                (op.id, op.generation),
                "the apply of its diagnosis's verdict failed",
                run.unwrap(),
                json!({"operation_id": op.id, "attempt": 1,
                       "started_generation": op.generation,
                       "verdict": {"choice": "replan"},
                       "error": "the verdict could not be applied: another supervisor's"}),
            )
            .unwrap();
    })));
    let outcome = supervise_reviewed_owned_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(
        !queue
            .asks(Default::default())
            .unwrap()
            .iter()
            .any(|ask| ask.asked_by == replan_detect::DIAGNOSIS_ASKER),
        "the refused park's ask opened"
    );
    let detail = queue.show(TaskId::new(1)).unwrap();
    let started = payloads(&detail, "replan_diagnosis_started");
    let failed = payloads(&detail, "replan_diagnosis_failed");
    assert_eq!((started.len(), failed.len()), (2, 1), "{failed:?}");
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap()
            .contains("another supervisor's"),
        "{}",
        failed[0]
    );
    let diagnosed = payloads(&detail, "replan_diagnosed");
    let outcomes: Vec<(&Value, &Value)> = diagnosed
        .iter()
        .map(|d| (&d["attempt"], &d["outcome"]))
        .collect();
    assert_eq!(
        outcomes,
        [
            (&json!(1), &json!("discarded")),
            (&json!(2), &json!("continue"))
        ],
        "{diagnosed:?}"
    );
    // The refused park went on to the ask at the lineage's limit, which
    // the queue refused under the job's generation.
    assert_eq!(diagnosed[0]["why"], "park_limit", "{}", diagnosed[0]);
    assert!(
        diagnosed[0]["reason"]
            .as_str()
            .unwrap()
            .contains("generation"),
        "{}",
        diagnosed[0]
    );
    assert_eq!(reviewer.triage_prompts().len(), 2);
    let op = queue
        .replan_operations(true)
        .unwrap()
        .into_iter()
        .find(|op| op.status != ReplanStatus::Withdrawn)
        .unwrap();
    assert_eq!(
        replan_detect::failed_jobs(&queue.task_events(TaskId::new(1)).unwrap(), op.id),
        1
    );
    assert_eq!(op.status, ReplanStatus::Continuing);
}

fn common_ok(db: &Path, args: &[&str]) -> Value {
    crate::common::cli::ok(db, args)
}
