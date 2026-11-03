//! The next run of a task gets a person's decisions on the run before it
//! whether they reached that run's session or not, and however the next
//! run was started (ADR-t4018-1, ADR-t4027-1): (i) an answer to the run's
//! worker_question delivered to its session goes into the prompt of the
//! run the recovery job's `retry_inherit` starts, and (ii) a `send_back` to
//! a run with no resume left goes into the prompt of the run a person's
//! bare `ready` starts, no retry of the earlier run recorded.

use std::path::Path;

use serde_json::{Value, json};

use super::{
    agent::{Script, succeeded},
    harness::Wiring,
};
use crate::{
    application::{RunFiles, TaskReads},
    domain::{AskKind, EventKind, RunStatus, TaskAction, TaskDetail},
    infrastructure::sqlite::SqliteQueue,
};

/// The person's answer to the run's worker_question.
const ANSWER: &str = "use the second line of the table, as ADR-t2946-1 says";

/// The person's words of the `send_back` that reached no session.
const REASON: &str = "keep the old flag: the review misread ADR-t2946-1";

/// A review that asks a person to land.
fn concern() -> Value {
    json!({"verdict": "concern", "reasons": ["a person looks"], "summary": "a person looks"})
}

/// The kinds of `detail`'s events, for a failed assertion to show.
fn kinds(detail: &TaskDetail) -> Vec<&str> {
    detail.events.iter().map(|e| e.kind.as_str()).collect()
}

/// The open `approve_landing` ask of the queue.
fn landing_ask(queue: &mut SqliteQueue) -> crate::domain::Ask {
    queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.kind == AskKind::ApproveLanding && ask.answer.is_none())
        .expect("an open approve_landing ask")
}

/// Use up the resumes of `run`: what the supervisor would have recorded had
/// it resumed the run so many times.
fn use_up_resumes(queue: &mut SqliteQueue, run: &crate::domain::RunId) {
    for attempt in 1..=crate::domain::MAX_RESUME_ATTEMPTS {
        queue
            .record_runtime_event(
                run,
                EventKind::ResumeStarted,
                json!({"attempt": attempt, "counted": true}),
            )
            .unwrap();
        queue
            .record_runtime_event(
                run,
                EventKind::ResumeFinished,
                json!({"attempt": attempt, "outcome": "resolved", "status": "awaiting_integration"}),
            )
            .unwrap();
    }
}

/// The `prompt.txt` of `detail`'s run at `at`.
fn prompt_of(wiring: &Wiring, detail: &TaskDetail, at: usize) -> String {
    wiring
        .files
        .read_to_string(&Path::new(detail.runs[at].run_dir().unwrap()).join("prompt.txt"))
        .unwrap()
}

/// (i) A person answered the run's worker_question and the resume of a
/// first `send_back` delivered the answer to its session; the run then
/// failed (its resumes used up at a second `send_back`) and the recovery
/// job retried it carrying its branch over: the new run's prompt holds the
/// answer whole, marked as sent to the earlier session.
#[test]
fn an_answer_delivered_to_a_run_reaches_the_prompt_of_its_retry_inherit() {
    let wiring = Wiring::new(
        "an_answer_delivered_to_a_run_reaches_the_prompt_of_its_retry_inherit",
        Script {
            receipts: vec![succeeded; 4],
            verdicts: vec![
                concern(),
                concern(),
                json!({"verdict": "repair", "confidence": "high",
                       "diagnosis": "the person's send_back reached no session; retry carrying the branch over",
                       "actions": [{"action": "retry_inherit"}]}),
                concern(),
            ],
        },
    );
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    assert_eq!(detail.runs.len(), 1, "{:?}", kinds(&detail));
    let run = detail.runs[0].clone();
    assert_eq!(
        run.status(),
        RunStatus::AwaitingIntegration,
        "{:?}",
        kinds(&detail)
    );
    let question = queue
        .ask(crate::domain::NewAsk {
            recommendation: None,
            confidence: None,
            topics: vec!["adr_conflict".into()],
            kind: AskKind::WorkerQuestion,
            task_id: Some(task),
            run_id: Some(run.id().clone()),
            question: "Which line of the table holds?".into(),
            options: vec![],
            asked_by: "worker".into(),
            reason_category: crate::domain::AskReason::Scope,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask;
    queue.answer(question.id, ANSWER).unwrap();
    let first = landing_ask(&mut queue);
    queue
        .answer(first.id, "send_back: name the flag in the ADR")
        .unwrap();
    wiring.supervise_once();
    let detail = queue.show(task).unwrap();
    assert!(
        detail
            .events
            .iter()
            .any(|e| e.kind == EventKind::AskDelivered
                && e.payload["ask_id"] == question.id.as_i64()),
        "the answer reached the session: {:?}",
        kinds(&detail)
    );

    // The next review asks again; with the resumes used up the second
    // send_back resumes no session, and the recovery job carries the branch
    // over to a new run.
    let second = landing_ask(&mut queue);
    assert_ne!(second.id, first.id);
    use_up_resumes(&mut queue, run.id());
    queue
        .answer(second.id, &format!("send_back: {REASON}"))
        .unwrap();
    wiring.supervise_once();

    let detail = queue.show(task).unwrap();
    assert_eq!(detail.runs.len(), 2, "{:?}", kinds(&detail));
    assert!(
        detail
            .events
            .iter()
            .any(|e| e.kind == EventKind::TriageFinished && e.payload["action"] == "retry_inherit"),
        "{:?}",
        kinds(&detail)
    );
    let prompt = prompt_of(&wiring, &detail, 1);
    assert!(
        prompt.contains(&format!(
            "- ask {} (worker_question, ",
            question.id.as_i64()
        )) && prompt.contains(&format!(
            "sent to the earlier session), the answer to the worker's question:\n{ANSWER}\n"
        )),
        "{prompt}"
    );
    assert!(prompt.contains(REASON), "{prompt}");
    // The send_back no session took comes before the delivered answer.
    assert!(
        prompt.find(REASON).unwrap() < prompt.find(ANSWER).unwrap(),
        "{prompt}"
    );
}

/// (ii) A person sent back a run whose resumes were used up; its recovery
/// job left it to a person (it gave no verdict it could read), and a person
/// started the task again with a bare `ready`, no retry of the run
/// recorded: the new run's prompt holds the send_back's reason whole.
#[test]
fn a_send_back_to_a_used_up_run_reaches_the_prompt_of_a_bare_ready() {
    let wiring = Wiring::new(
        "a_send_back_to_a_used_up_run_reaches_the_prompt_of_a_bare_ready",
        Script {
            receipts: vec![succeeded, succeeded],
            verdicts: vec![concern(), json!({"verdict": "unreadable"}), concern()],
        },
    );
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let first = detail.runs[0].id().clone();
    let ask = landing_ask(&mut queue);
    use_up_resumes(&mut queue, &first);
    queue
        .answer(ask.id, &format!("send_back: {REASON}"))
        .unwrap();
    wiring.supervise_once();
    let detail = queue.show(task).unwrap();
    assert_eq!(detail.runs.len(), 1, "{:?}", kinds(&detail));
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::Failed,
        "{:?}",
        kinds(&detail)
    );
    assert!(
        !detail.events.iter().any(|e| matches!(
            e.kind.known(),
            Some(EventKind::TriageFinished | EventKind::TriageDecided)
        ) && (e.payload["action"] == "retry"
            || e.payload["action"] == "retry_inherit"
            || e.payload["answer"] == "retry")),
        "no retry of the run: {:?}",
        kinds(&detail)
    );

    crate::application::TaskStore::transition(&mut queue, task, TaskAction::Ready)
        .expect("a person's bare ready");
    wiring.supervise_once();
    let detail = queue.show(task).unwrap();
    assert_eq!(detail.runs.len(), 2, "{:?}", kinds(&detail));
    let prompt = prompt_of(&wiring, &detail, 1);
    assert!(
        prompt.contains(&format!("Decisions a person made on run {first}")),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("- ask {} (approve_landing, ", ask.id.as_i64()))
            && prompt.contains("never sent to a session), the landing's send_back, its reason:\n")
            && prompt.contains(REASON),
        "{prompt}"
    );
}
