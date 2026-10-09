//! Runtime tests: a Spike's run reports its result in the receipt
//! (ADR-t1487-1 decision 3). The supervisor's wiring of one case: a
//! receipt without it parks the run as `needs_session`
//! (`spike_result_missing`) and the resume that writes it, a negative
//! verdict included, brings the run on. Which receipts lack it, for every
//! verdict and for an implementation's run, is judged by the unit tests of
//! `domain::validation` and `domain::execution_class`, and the landing's
//! check of a rewritten receipt by `application::integrate`'s.
use crate::runtime_support;

use dagq::domain::ExecutionClass;
use runtime_support::*;

/// A fixture whose only ready task is of `class`.
fn class_fixture(class: ExecutionClass) -> (Fixture, PathBuf, PathBuf) {
    let (dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let task = queue
        .add(NewTask {
            title: "does the premise hold".into(),
            description: "measure it, at most one hour".into(),
            acceptance: "a verdict with grounds".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            wait_for_build: false,
            execution_class: class,
        })
        .unwrap();
    assert_eq!(task.id(), TaskId::new(2));
    assert_eq!(task.execution_class(), class);
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    (dir, repo, db)
}

/// A receipt function for scripts: `receipt_spike COMMIT VERDICT` claims
/// success with a complete `spike_result` of `VERDICT`, or none when
/// `VERDICT` is empty.
const RECEIPT_SPIKE: &str = r#"receipt_spike() {
  if [ -n "$2" ]; then
    result=',"spike_result":{"verdict":"'"$2"'","grounds":"measured twice","evidence":"notes.txt","conditions":{"commit":"'"$1"'","tools":"sh","provider":"claude headless worker","environment":"test host"},"spent":"5 minutes of 60"}'
  else
    result=''
  fi
  printf '{"run_id":"%s","result":"succeeded","commit":"%s","tests":{"status":"passed","evidence_or_reason":"ran"},"e2e":{"status":"not_applicable","evidence_or_reason":"the runtime runs the e2e"},"subagent_review":{"status":"passed","evidence_or_reason":"reviewed"},"summary":"done"%s}' "$RUN_ID" "$1" "$result" > "$RECEIPT.tmp"
  mv "$RECEIPT.tmp" "$RECEIPT"
}
"#;

/// A Spike's receipt without its result parks the run as `needs_session`
/// with the code `spike_result_missing`; the resume asks for it, and the
/// rewritten receipt with a `does_not_hold` verdict (a result, not a gap)
/// brings the run to `awaiting_integration`.
#[test]
fn a_spike_without_its_result_waits_for_a_session_that_writes_it() {
    let (_dir, repo, db) = class_fixture(ExecutionClass::Spike);
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!("{RECEIPT_SPIKE}commit work; receipt_spike \"$(git rev-parse HEAD)\" ''"),
    );
    backend.resume_script_for(
        2,
        &format!(
            "{RECEIPT_SPIKE}await_message; receipt_spike \"$(git rev-parse HEAD)\" does_not_hold; idle; await_exit"
        ),
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    let run = &detail.runs[0];
    // The worker was told how to write the result.
    let prompt = read_prompt(run);
    assert!(prompt.contains("Spike: this task is a Spike"), "{prompt}");
    assert!(prompt.contains("\"spike_result\":{\"verdict\""), "{prompt}");
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 2);
    assert_eq!(validated[0]["status"], "needs_session");
    assert_eq!(validated[0]["code"], "spike_result_missing");
    assert_eq!(validated[0]["reason"], "spike result missing: spike_result");
    assert_eq!(
        validated[0]["spike_result_missing"],
        json!(["spike_result"])
    );
    assert!(validated[0].get("evidence_missing").is_none());
    assert_eq!(validated[1]["status"], "awaiting_integration");
    assert_eq!(
        validated[1]["receipt"]["spike_result"]["verdict"],
        "does_not_hold"
    );
    assert_eq!(
        payloads(&detail, "evidence_missing"),
        [&json!({
            "code": "spike_result_missing",
            "checks": [],
            "spike_result": ["spike_result"],
            "reason": "spike result missing: spike_result",
        })]
    );
    let text = &session_texts(run)[0];
    assert!(text.contains("or, for a Spike, its spike_result"), "{text}");
    assert!(
        text.contains("Reason: spike result missing: spike_result"),
        "{text}"
    );
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
}
