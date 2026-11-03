//! A person's `send_back` that never reached a run's session goes into the
//! prompt of the next run of its task (ADR-t4018-1): the run's resumes were
//! used up, or its last resume failed before its request was sent, the
//! recovery job retried the task carrying the branch over, and the new
//! run's `prompt.txt` holds the reason as the person wrote it. A decision
//! too long for the section is cut there, and the read the prompt names
//! for it prints it whole.

use std::path::Path;

use serde_json::{Value, json};

use super::{
    agent::{Script, succeeded},
    harness::Wiring,
};
use crate::{
    application::{RunFiles, TaskReads},
    domain::{AskKind, EventKind, RunStatus, TaskDetail, TaskId},
    infrastructure::sqlite::SqliteQueue,
};

/// The person's words of the `send_back`.
const REASON: &str = "keep the old flag: the review misread ADR-t2946-1, and the earlier answer of ask 1420 still holds";

/// A review that asks a person to land (a concern), then the recovery
/// job's verdict: carry the branch over to a new run.
fn verdicts() -> Vec<Value> {
    vec![
        json!({"verdict": "concern", "reasons": ["a person looks"], "summary": "a person looks"}),
        json!({"verdict": "repair", "confidence": "high",
               "diagnosis": "the person's send_back reached no session; retry carrying the branch over",
               "actions": [{"action": "retry_inherit"}]}),
        json!({"verdict": "concern", "reasons": ["a person looks"], "summary": "a person looks"}),
    ]
}

/// The payloads of `detail`'s events of `kind`, oldest first.
fn payloads<'a>(detail: &'a TaskDetail, kind: &str) -> Vec<&'a Value> {
    detail
        .events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| &event.payload)
        .collect()
}

/// `count` resumes the run had before, each counted: what the supervisor
/// would have recorded had it resumed the run so many times.
fn resumed_before(queue: &mut SqliteQueue, run: &crate::domain::RunId, count: usize) {
    for attempt in 1..=count {
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

/// Run the task to its first `approve_landing` ask, give its run
/// `resumes` counted resumes before, and answer the ask `send_back` with
/// [`REASON`]: the task, the first run and the ask.
fn sent_back(wiring: &Wiring, resumes: usize) -> (TaskId, crate::domain::RunId, i64) {
    sent_back_with(wiring, resumes, REASON)
}

/// [`sent_back`] with `reason` as the person's words.
fn sent_back_with(
    wiring: &Wiring,
    resumes: usize,
    reason: &str,
) -> (TaskId, crate::domain::RunId, i64) {
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.runs.len(), 1, "{kinds:?}");
    let run = detail.runs[0].id().clone();
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::AwaitingIntegration,
        "{kinds:?}"
    );
    let asks = queue.asks(Default::default()).unwrap();
    let ask = asks
        .iter()
        .find(|ask| ask.kind == AskKind::ApproveLanding)
        .unwrap_or_else(|| panic!("no approve_landing ask: {asks:?}"));
    resumed_before(&mut queue, &run, resumes);
    queue
        .answer(ask.id, &format!("send_back: {reason}"))
        .unwrap();
    (task, run, ask.id.as_i64())
}

/// After the first run went to the recovery job and the job carried its
/// branch over: the new run's `prompt.txt` holds [`REASON`] whole with
/// its ask, the job's reason why the run was carried over, and the read of
/// `landing_decided` the prompt names prints the whole reason.
fn assert_handed_over(wiring: &Wiring, task: TaskId, first: &crate::domain::RunId, ask: i64) {
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.runs.len(), 2, "{kinds:?}");
    assert_eq!(detail.runs[0].id(), first);
    // No request ever took the send_back to the first run's session.
    assert!(
        payloads(&detail, "resume_request_sent").is_empty(),
        "{kinds:?}"
    );
    let retried: Vec<&Value> = payloads(&detail, "triage_finished")
        .into_iter()
        .filter(|p| p["action"] == "retry_inherit")
        .collect();
    assert_eq!(retried.len(), 1, "{kinds:?}");
    let second = &detail.runs[1];
    let prompt = wiring
        .files
        .read_to_string(&Path::new(second.run_dir().unwrap()).join("prompt.txt"))
        .unwrap();
    assert!(prompt.contains(REASON), "{prompt}");
    assert!(
        prompt.contains(&format!("- ask {ask} (approve_landing, ")),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("Decisions a person made on run {first}")),
        "{prompt}"
    );
    assert!(
        prompt.contains("the recovery job retried its task carrying its work over (the person's send_back reached no session"),
        "{prompt}"
    );
    // What the prompt names to read a cut decision with prints it whole.
    let read = crate::application::queue_reads::observation::events(
        &queue,
        &crate::application::queue_reads::EventsRead {
            after: 0,
            limit: 100,
            all: false,
            full: true,
            run: None,
            task: Some(task.as_i64()),
            goal: None,
            kind: vec!["landing_decided".into()],
            since: None,
            until: None,
        },
    )
    .unwrap();
    assert!(read.to_string().contains(REASON), "{read}");
}

/// (i) A run whose resumes were used up is sent back by a person's answer:
/// no session takes it, the recovery job carries the branch over, and the
/// new run's prompt holds the person's reason.
#[test]
fn a_send_back_to_a_run_with_no_resume_left_reaches_the_next_runs_prompt() {
    let wiring = Wiring::new(
        "a_send_back_to_a_run_with_no_resume_left_reaches_the_next_runs_prompt",
        Script {
            receipts: vec![succeeded, succeeded],
            verdicts: verdicts(),
        },
    );
    let (task, first, ask) = sent_back(&wiring, crate::domain::MAX_RESUME_ATTEMPTS);
    wiring.supervise_once();
    let detail = wiring.queue().show(task).unwrap();
    assert!(
        payloads(&detail, "resume_started").len() == crate::domain::MAX_RESUME_ATTEMPTS,
        "no resume started after the send_back"
    );
    assert_handed_over(&wiring, task, &first, ask);
}

/// (ii) The resume after a person's `send_back` started (`resume_started`)
/// but its wrapper failed to start, so its request was never sent; the run
/// used its last resume and the recovery job carried its branch over: the
/// new run's prompt holds the person's reason all the same.
#[test]
fn a_send_back_whose_resume_failed_before_its_request_reaches_the_next_runs_prompt() {
    let wiring = Wiring::new(
        "a_send_back_whose_resume_failed_before_its_request_reaches_the_next_runs_prompt",
        Script {
            receipts: vec![succeeded, succeeded],
            verdicts: verdicts(),
        },
    );
    let (task, first, ask) = sent_back(&wiring, crate::domain::MAX_RESUME_ATTEMPTS - 1);
    wiring.fail_resume_launches();
    let outcome = wiring.supervise_once_reporting();
    let errors = outcome["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{outcome}");
    assert!(
        errors[0]["message"]
            .as_str()
            .unwrap()
            .contains("could not be resumed"),
        "{outcome}"
    );
    // The next pass finds its resumes used up and recovers it.
    wiring.supervise_once();
    let detail = wiring.queue().show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        payloads(&detail, "resume_started").len(),
        crate::domain::MAX_RESUME_ATTEMPTS,
        "the resume after the send_back started: {kinds:?}"
    );
    assert_handed_over(&wiring, task, &first, ask);
}

/// (iii) The run that carried the first one's branch over failed before its
/// session took its prompt in (its wrapper could not start), and a person
/// answered the recovery's ask `retry`: the next run's prompt holds both the
/// first run's `send_back` reason and that answer, each with its ask, and
/// the read of the asks prints the answer whole.
#[test]
fn decisions_pass_over_a_run_that_failed_before_its_session_took_them_in() {
    let wiring = Wiring::new(
        "decisions_pass_over_a_run_that_failed_before_its_session_took_them_in",
        Script {
            receipts: vec![succeeded, succeeded],
            verdicts: vec![
                json!({"verdict": "concern", "reasons": ["a person looks"], "summary": "a person looks"}),
                json!({"verdict": "repair", "confidence": "high",
                       "diagnosis": "the person's send_back reached no session; retry carrying the branch over",
                       "actions": [{"action": "retry_inherit"}]}),
                json!({"verdict": "escalate", "confidence": "high",
                       "diagnosis": "the run's wrapper could not start",
                       "question": "retry the task?"}),
                json!({"verdict": "concern", "reasons": ["a person looks"], "summary": "a person looks"}),
            ],
        },
    );
    let (task, first, ask) = sent_back(&wiring, crate::domain::MAX_RESUME_ATTEMPTS);
    wiring.fail_claim_launches(true);
    let outcome = wiring.supervise_once_reporting();
    assert!(
        outcome["errors"]
            .to_string()
            .contains("refuses to start the wrapper"),
        "{outcome}"
    );
    wiring.fail_claim_launches(false);
    // The next pass hands the failed run to the recovery job, which asks.
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.runs.len(), 2, "{kinds:?}");
    let second = detail.runs[1].id().clone();
    let decide = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.kind == AskKind::Decide && ask.run_id.as_ref() == Some(&second))
        .unwrap_or_else(|| panic!("no decide ask on run {second}: {kinds:?}"));
    const ANSWER: &str = "retry";
    queue.answer(decide.id, ANSWER).unwrap();
    wiring.supervise_once();

    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.runs.len(), 3, "{kinds:?}");
    let third = &detail.runs[2];
    let prompt = wiring
        .files
        .read_to_string(&Path::new(third.run_dir().unwrap()).join("prompt.txt"))
        .unwrap();
    assert!(prompt.contains(REASON), "{prompt}");
    assert!(
        prompt.contains(&format!("- ask {ask} (approve_landing, ")),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("run {first}), the landing's send_back")),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("- ask {} (decide, ", decide.id)),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!(
            "run {second}), the answer to the decide ask:\n{ANSWER}\n"
        )),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("Decisions a person made on run {second}")),
        "{prompt}"
    );
    // The read of the asks a worker may run prints the answer whole.
    let asks = crate::application::queue_reads::shared::asks(
        &queue,
        &crate::application::queue_reads::AsksRead {
            open: false,
            role: None,
            all: true,
        },
    )
    .unwrap();
    assert!(
        asks["asks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == decide.id.as_i64() && a["answer"] == ANSWER),
        "{asks}"
    );
}

/// The command the prompt names to read the whole text of `ask`'s decision
/// cut by its limit: what follows `ask <id>: ` between backticks.
fn read_command_for(prompt: &str, ask: i64) -> &str {
    let named = format!("ask {ask}: `");
    let from = prompt
        .find(&named)
        .unwrap_or_else(|| panic!("the prompt names no read for ask {ask}: {prompt}"))
        + named.len();
    let to = prompt[from..].find('`').unwrap() + from;
    &prompt[from..to]
}

/// What the read `command` prints, run as the worker would: `dagq events
/// --full --task <ID> --kind <KIND>` read off the queue.
fn printed_by(queue: &SqliteQueue, command: &str) -> Value {
    let words: Vec<&str> = command.split(' ').collect();
    let ["dagq", "events", "--full", "--task", task, "--kind", kind] = words[..] else {
        panic!("not a read of a task's events of a kind: {command}");
    };
    crate::application::queue_reads::observation::events(
        queue,
        &crate::application::queue_reads::EventsRead {
            after: 0,
            limit: 100,
            all: false,
            full: true,
            run: None,
            task: Some(task.parse().unwrap()),
            goal: None,
            kind: vec![kind.to_owned()],
            since: None,
            until: None,
        },
    )
    .unwrap()
}

/// A person's `send_back` reason and answer to the run's worker_question,
/// each longer than one decision's limit in the section, never reached the
/// run's session: the new run's prompt cuts both, naming each one's ask
/// and the read of the event that holds it, and that read prints each text
/// whole, to its end.
#[test]
fn decisions_cut_by_their_limit_are_printed_whole_by_the_read_the_prompt_names() {
    let wiring = Wiring::new(
        "decisions_cut_by_their_limit_are_printed_whole_by_the_read_the_prompt_names",
        Script {
            receipts: vec![succeeded, succeeded],
            verdicts: verdicts(),
        },
    );
    let limit = crate::application::prompt::WORKER_DECISION_BYTES;
    let reason = format!(
        "{}the send_back reason ends here",
        "keep the old flag, as the earlier answer of ask 1420 said. ".repeat(limit / 40)
    );
    let answer = format!(
        "{}the answer ends here",
        "use the second line of the table, as ADR-t2946-1 says. ".repeat(limit / 40)
    );
    assert!(reason.len() > limit && answer.len() > limit);
    let (task, first, landing) =
        sent_back_with(&wiring, crate::domain::MAX_RESUME_ATTEMPTS, &reason);
    let mut queue = wiring.queue();
    let question = queue
        .ask(crate::domain::NewAsk {
            recommendation: None,
            confidence: None,
            topics: vec!["adr_conflict".into()],
            kind: AskKind::WorkerQuestion,
            task_id: Some(task),
            run_id: Some(first.clone()),
            question: "Which line of the table holds?".into(),
            options: vec![],
            asked_by: "worker".into(),
            reason_category: crate::domain::AskReason::Scope,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask;
    queue.answer(question.id, &answer).unwrap();
    wiring.supervise_once();

    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.runs.len(), 2, "{kinds:?}");
    assert!(payloads(&detail, "ask_delivered").is_empty(), "{kinds:?}");
    let prompt = wiring
        .files
        .read_to_string(&Path::new(detail.runs[1].run_dir().unwrap()).join("prompt.txt"))
        .unwrap();
    let question = question.id.as_i64();
    for (ask, kind, text, end, event) in [
        (
            landing,
            "approve_landing",
            &reason,
            "the send_back reason ends here",
            "landing_decided",
        ),
        (
            question,
            "worker_question",
            &answer,
            "the answer ends here",
            "ask_answered",
        ),
    ] {
        assert!(
            prompt.contains(&format!("- ask {ask} ({kind}, ")),
            "{prompt}"
        );
        // Its start is in the prompt, its end is cut.
        assert!(prompt.contains(&text[..limit / 2]), "{prompt}");
        assert!(!prompt.contains(end), "ask {ask} was not cut: {prompt}");
        let command = read_command_for(&prompt, ask);
        assert_eq!(
            command,
            format!("dagq events --full --task {task} --kind {event}")
        );
        let printed = printed_by(&queue, command);
        assert!(
            printed.to_string().contains(text.as_str()),
            "`{command}` does not print ask {ask}'s whole text: {printed}"
        );
    }
}
