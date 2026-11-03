//! A person's decisions on a run go into the handoff prompt of a new
//! session of the same run (ADR-t4027-1): a person answered the run's
//! worker_question and sent its landing back, and the resume took both to
//! the same session; after the next review a
//! person sent it back again, past `[fresh_session]`'s threshold, so that
//! resume starts a new session (`session_renewed`) whose handoff holds the
//! earlier send_back's reason and the answer, both delivered to the
//! earlier session, as the person wrote them.

use std::path::Path;

use serde_json::{Value, json};

use super::{
    agent::{Script, succeeded},
    harness::Wiring,
};
use crate::{
    application::{RunFiles, TaskReads},
    domain::{AskKind, EventKind, RunStatus, turn::turns_dir},
};

/// The person's words of the first `send_back`, delivered to the session.
const REASON: &str = "keep the old flag: the review misread ADR-t2946-1";

/// The person's words of the second `send_back`, which the new session's
/// request carries.
const LATER: &str = "name the old flag in the ADR's consequences";

/// The person's answer to the run's worker_question.
const ANSWER: &str = "use the second line of the table, as the ADR says";

/// The prompts of the requests written for the headless session of the
/// run in `run_dir`, waiting or taken, in no order.
fn requests(wiring: &Wiring, run_dir: &Path) -> Vec<String> {
    wiring
        .files
        .read_dir(&turns_dir(run_dir))
        .unwrap()
        .iter()
        .filter_map(|path| wiring.files.read_to_string(path).ok())
        .filter_map(|text| serde_json::from_str::<Value>(&text).ok())
        .filter_map(|request| request["prompt"].as_str().map(str::to_owned))
        .collect()
}

/// The open `approve_landing` ask of the queue.
fn landing_ask(queue: &mut crate::infrastructure::sqlite::SqliteQueue) -> crate::domain::Ask {
    queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.kind == AskKind::ApproveLanding && ask.answer.is_none())
        .expect("an open approve_landing ask")
}

#[test]
fn a_new_session_of_a_run_carries_the_persons_send_back_and_answer() {
    let concern =
        json!({"verdict": "concern", "reasons": ["a person looks"], "summary": "a person looks"});
    let wiring = Wiring::new(
        "a_new_session_of_a_run_carries_the_persons_send_back_and_answer",
        Script {
            receipts: vec![succeeded; 6],
            verdicts: vec![concern.clone(), concern.clone(), concern],
        },
    )
    .with_fresh_session(100);
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.runs.len(), 1, "{kinds:?}");
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration, "{kinds:?}");

    // The run's worker_question, answered and delivered to its session.
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
    // Answered while the run waits to land: the resume of the first
    // send_back delivers it to the same session (`ask_delivered`).
    queue.answer(question.id, ANSWER).unwrap();

    // The first send_back, below the threshold: the same session takes it.
    let first = landing_ask(&mut queue);
    queue
        .answer(first.id, &format!("send_back: {REASON}"))
        .unwrap();
    wiring.supervise_once();
    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert!(
        detail
            .events
            .iter()
            .any(|e| e.kind == EventKind::ResumeRequestSent),
        "the first send_back reached the session: {kinds:?}"
    );
    assert!(
        detail
            .events
            .iter()
            .any(|e| e.kind == EventKind::AskDelivered
                && e.payload["ask_id"] == question.id.as_i64()),
        "the answer reached the session: {kinds:?}"
    );
    assert!(
        !detail
            .events
            .iter()
            .any(|e| e.kind == EventKind::SessionRenewed),
        "{kinds:?}"
    );

    // The next review asks again; the turn's context was past the
    // threshold, and the person sends it back again.
    queue
        .record_runtime_event(
            run.id(),
            EventKind::TurnFinished,
            json!({"turn": 2, "peak_context": 500}),
        )
        .unwrap();
    let second = landing_ask(&mut queue);
    assert_ne!(second.id, first.id);
    queue
        .answer(second.id, &format!("send_back: {LATER}"))
        .unwrap();
    wiring.supervise_once();

    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.runs.len(), 1, "the same run: {kinds:?}");
    assert!(
        detail
            .events
            .iter()
            .any(|e| e.kind == EventKind::SessionRenewed),
        "{kinds:?}"
    );
    let handoffs: Vec<String> = requests(&wiring, Path::new(run.run_dir().unwrap()))
        .into_iter()
        .filter(|prompt| prompt.starts_with("dagq: this run's worker goes on in a new session"))
        .collect();
    assert_eq!(handoffs.len(), 1, "{kinds:?}");
    let handoff = &handoffs[0];
    let section = handoff
        .split_once("Decisions a person made on this run")
        .unwrap_or_else(|| panic!("no decisions' section: {handoff}"))
        .1
        .split_once("The earlier session would have been sent this next")
        .unwrap()
        .0;
    assert!(
        section.contains(&format!("- ask {} (approve_landing, ", first.id.as_i64()))
            && section
                .contains("sent to the earlier session), the landing's send_back, its reason:\n")
            && section.contains(REASON),
        "{section}"
    );
    assert!(
        section.contains(&format!(
            "- ask {} (worker_question, ",
            question.id.as_i64()
        )) && section.contains(&format!(
            "sent to the earlier session), the answer to the worker's question:\n{ANSWER}\n"
        )),
        "{section}"
    );
    // The second send_back is the request's, written once.
    assert!(!section.contains(LATER), "{section}");
    assert_eq!(handoff.matches(LATER).count(), 1, "{handoff}");
}
