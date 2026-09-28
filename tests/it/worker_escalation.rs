//! The raise of a worker session after a failure the task caused (ADR-0079
//! decision 5): a failed verification or a concern a person sent back
//! resumes one step higher, a conflict, a kill, `evidence_missing` and
//! `scope_violation` do not; the step stays with the task's later runs, and
//! `stats` counts the raises and whether the raised resumes resolved.
use crate::runtime_support;
use dagq::domain::EventKind;
use dagq::domain::LeaseToken;

use dagq::domain::{ClaimOutcome, worker_model::WorkerTrial};
use runtime_support::*;

/// Park task 2's run for a conflict, record `park` after it (when given),
/// and begin its resume: the `resume_started` it records.
fn resume_started_after(park: Option<(EventKind, Value)>, verification: bool) -> Value {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    if verification {
        count_resumes_of_parked(&db);
    }
    let mut queue = SqliteQueue::open(&db).unwrap();
    if let Some((kind, payload)) = park {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
    queue
        .begin_resume(
            run.id(),
            &LeaseToken::new("t"),
            &sha(&first_landed),
            None,
            Default::default(),
        )
        .unwrap()
        .unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    payloads(&detail, "resume_started")[0].clone()
}

/// The model and effort `started` recorded.
fn session(started: &Value) -> (Value, Value) {
    (started["model"].clone(), started["effort"].clone())
}

fn opus(effort: &str) -> (Value, Value) {
    (json!("claude-opus-5-5"), json!(effort))
}

fn assert_kept(started: &Value) {
    assert_eq!(session(started), opus("medium"), "{started}");
    assert!(started.get("escalated_from").is_none(), "{started}");
    assert!(started.get("escalation_reason").is_none(), "{started}");
}

fn assert_raised(started: &Value, reason: &str) {
    assert_eq!(session(started), opus("high"), "{started}");
    assert_eq!(
        started["escalated_from"],
        json!({"model": "claude-opus-5-5", "effort": "medium"})
    );
    assert_eq!(started["escalation_reason"], reason);
}

#[test]
fn a_conflict_resume_keeps_the_step() {
    assert_kept(&resume_started_after(None, false));
}

#[test]
fn a_failed_verification_resumes_one_step_higher() {
    assert_raised(&resume_started_after(None, true), "verification_failed");
}

#[test]
fn a_concern_sent_back_resumes_one_step_higher() {
    let started = resume_started_after(
        Some((
            EventKind::LandingDecided,
            json!({"status": "needs_session", "reason": "sent back", "code": "sent_back"}),
        )),
        false,
    );
    assert_raised(&started, "sent_back");
}

#[test]
fn missing_evidence_and_a_scope_violation_keep_the_step() {
    for (kind, code) in [
        (EventKind::EvidenceMissing, "evidence_missing"),
        (EventKind::ScopeViolation, "scope_violation"),
    ] {
        let started = resume_started_after(
            Some((
                kind,
                json!({"status": "needs_session", "reason": code, "code": code}),
            )),
            true,
        );
        assert_kept(&started);
    }
}

/// A killed session fails its run; the recovery job sends it back to a
/// session of its own, which keeps the step.
#[test]
fn a_kill_keeps_the_step() {
    let started = resume_started_after(
        Some((
            EventKind::TriageFinished,
            json!({"status": "needs_session", "action": "resume", "reason": "killed", "code": "triage_resume"}),
        )),
        true,
    );
    assert_kept(&started);
}

/// A run parked for a failed verification is resumed by the supervisor at
/// Opus high: the wrapper starts the session with it, the run lands, and
/// `stats` reads the raise and that the raised resume resolved.
#[test]
fn a_raised_resume_starts_the_session_higher_and_stats_reads_it() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    count_resumes_of_parked(&db);
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    let resumed: Vec<Value> = session_models(&db)
        .into_iter()
        .filter(|model| model["run_id"] == run.id().as_str() && model["resume"] == true)
        .collect();
    assert_eq!(
        resumed,
        [json!({"run_id": run.id(), "resume": true, "model": "claude-opus-5-5", "effort": "high"})]
    );

    let stats = stats_full(&db);
    let escalations = &stats["escalations"];
    assert_eq!(escalations["count"], 1, "{escalations}");
    assert_eq!(escalations["by_reason"], json!({"verification_failed": 1}));
    assert_eq!(
        escalations["by_step"],
        json!({"claude-opus-5-5/medium -> claude-opus-5-5/high": 1})
    );
    assert_eq!(escalations["resumes"]["attempts"], 1);
    assert_eq!(escalations["resumes"]["resolved"], 1);
    assert_eq!(escalations["revises"], 0);
    let runs = stats["runs"].as_array().unwrap();
    let second = runs.iter().find(|r| r["task_id"] == 2).unwrap();
    let attempt = &second["resume_attempts"][0];
    assert_eq!(attempt["escalated_from"], "claude-opus-5-5/medium");
    assert_eq!(attempt["escalated_to"], "claude-opus-5-5/high");
    assert_eq!(attempt["resolved"], true);
}

/// A raised step stays with the task: its retry is claimed at it, in the
/// group the claim chose, and says it inherited it.
#[test]
fn a_retry_inherits_the_raised_step() {
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
    let claim = |queue: &mut SqliteQueue| {
        let ClaimOutcome::Claimed { run } = queue
            .claim_for_supervisor_in_order(
                &base,
                &LeaseToken::new("t"),
                &[TaskId::new(1)],
                None,
                &WorkerTrial::default(),
                &dagq::domain::worker::Worker::ALL,
            )
            .unwrap()
        else {
            panic!("nothing to claim");
        };
        run
    };
    let first = claim(&mut queue);
    let connection = Connection::open(&db).unwrap();
    connection
        .execute(
            "INSERT INTO run_events(task_id, run_id, kind, payload) VALUES (1, ?1, 'resume_started', ?2)",
            rusqlite::params![
                first.id(),
                json!({"attempt": 1, "model": "claude-opus-5-5", "effort": "xhigh", "group": null,
                       "escalated_from": {"model": "claude-opus-5-5", "effort": "high"},
                       "escalation_reason": "verification_failed"})
                .to_string()
            ],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE task_runs SET status='failed' WHERE id=?1",
            [first.id()],
        )
        .unwrap();
    connection
        .execute("UPDATE tasks SET status='ready' WHERE id=1", [])
        .unwrap();
    let second = claim(&mut queue);
    let claimed = queue.run_events(second.id()).unwrap();
    let claimed = &claimed
        .iter()
        .find(|e| e.kind == "run_claimed")
        .unwrap()
        .payload;
    assert_eq!(
        (&claimed["model"], &claimed["effort"], &claimed["group"]),
        (&json!("claude-opus-5-5"), &json!("xhigh"), &Value::Null)
    );
    assert_eq!(claimed["escalation_inherited"], true);
    // The first claim inherited nothing.
    let first_claimed = queue.run_events(first.id()).unwrap();
    assert!(
        first_claimed
            .iter()
            .find(|e| e.kind == "run_claimed")
            .unwrap()
            .payload
            .get("escalation_inherited")
            .is_none()
    );
}

/// The worker goes idle after its receipt; when a text arrives in its
/// terminal it appends a line, commits, rewrites the receipt and goes idle
/// again, once (`runtime_review`'s agent for one revise).
fn revising_agent() -> String {
    "commit work; receipt \"$(git rev-parse HEAD)\"; idle; \
     while [ ! -f \"$MESSAGE\" ]; do sleep 0.1; done; rm \"$MESSAGE\"; \
     printf 'fix 1\\n' >> change.txt; git commit -q -am \"fix 1\"; \
     receipt \"$(git rev-parse HEAD)\"; idle; await_exit"
        .to_owned()
}

/// A live session that cannot be switched up before a revise goes on as it
/// was: the revise is sent and records why it was not raised (ADR-0079
/// decision 5).
#[test]
fn a_revise_whose_session_cannot_be_switched_goes_on_unraised() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let mut backend = TestWorkspace::new(&db, false, &revising_agent());
    backend.switch_fails = true;
    let reviewer = TestReviewer::new(&[
        verdict("revise", &["add a line to change.txt"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    let requested = payloads(&detail, "revise_requested");
    assert_eq!(
        (&requested[0]["model"], &requested[0]["effort"]),
        (&json!("claude-opus-5-5"), &json!("medium"))
    );
    assert!(
        requested[0].get("escalated_from").is_none(),
        "{}",
        requested[0]
    );
    let skipped = &requested[0]["escalation_skipped"];
    assert_eq!(
        (&skipped["model"], &skipped["effort"], &skipped["reason"]),
        (&json!("claude-opus-5-5"), &json!("high"), &json!("revise"))
    );
    assert!(
        skipped["why"]
            .as_str()
            .unwrap()
            .contains("`/effort high` could not be typed"),
        "{skipped}"
    );
    assert_eq!(backend.texts().len(), 1);
    let escalations = &stats_full(&db)["escalations"];
    assert_eq!(
        (&escalations["count"], &escalations["revises_not_switched"]),
        (&json!(0), &json!(1)),
        "{escalations}"
    );
}

/// A switch input left in the input box leaves the session unsettled: the
/// revise is not typed over it, and a person is asked as for a revise that
/// could not be sent (ADR-0079 decision 5).
#[test]
fn a_switch_left_in_the_input_box_asks_a_person_without_typing_the_revise() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.switch_stuck = true;
    let reviewer = TestReviewer::new(&[verdict("revise", &["add a line"], "one gap")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(payloads(&detail, "revise_requested").is_empty());
    assert!(backend.texts().is_empty(), "{:?}", backend.texts());
    let switches: Vec<String> = backend.switches().into_iter().map(|(_, s)| s).collect();
    assert_eq!(switches, ["/effort high"]);
    let unconfirmed = payloads(&detail, "submit_unconfirmed");
    assert_eq!(unconfirmed[0]["what"], "model switch");
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].kind, dagq::domain::AskKind::ApproveLanding);
    assert!(
        asks[0]
            .question
            .contains("could not be switched to claude-opus-5-5/high before revise 1"),
        "{}",
        asks[0].question
    );
}

/// The one revise of `runtime_review`'s revise test switched the live
/// session from Opus medium to high before the request, and `stats`
/// counts it.
pub(crate) fn assert_raised_by_revise(requested: &Value, backend: &TestWorkspace, db: &Path) {
    assert_eq!(session(requested), opus("high"), "{requested}");
    assert_eq!(
        requested["escalated_from"],
        json!({"model": "claude-opus-5-5", "effort": "medium"})
    );
    assert_eq!(requested["escalation_reason"], "revise");
    assert_eq!(
        backend.switches(),
        [(WORKSPACE_ID.to_owned(), "/effort high".to_owned())]
    );
    let escalations = &stats_full(db)["escalations"];
    assert_eq!(escalations["count"], 1, "{escalations}");
    assert_eq!(escalations["revises"], 1);
    assert_eq!(escalations["by_reason"], json!({"revise": 1}));
}

/// Each of two revises raised the live session a step: high, then xhigh.
pub(crate) fn assert_revises_raised(detail: &dagq::domain::TaskDetail, backend: &TestWorkspace) {
    let efforts: Vec<&Value> = payloads(detail, "revise_requested")
        .iter()
        .map(|p| &p["effort"])
        .collect();
    assert_eq!(efforts, [&json!("high"), &json!("xhigh")]);
    let switches: Vec<String> = backend.switches().into_iter().map(|(_, s)| s).collect();
    assert_eq!(switches, ["/effort high", "/effort xhigh"]);
}
