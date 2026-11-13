//! The answers of `approve_landing` asks wired from the answer to what the
//! supervisor does with the run: a pass applies the answer whatever the
//! slots and closes the ask, a `land` queues the run, which lands once
//! there is room, before new claims, and in the order of the approvals; a
//! `retry_review` reviews a failed review's run again; a supervisor that
//! drains applies the answers but starts no new work (ADR-t3330-1). Which
//! answer applies, and when an approved run lands, is
//! `supervise::execution::landing`'s judgment, tested there.

use std::sync::mpsc;

use serde_json::{Value, json};

use super::{
    agent::{Script, succeeded, succeeded_at_the_head, verdict},
    harness::Wiring,
};
use crate::{
    application::TaskReads,
    domain::{Ask, AskId, AskKind, RunStatus, TaskDetail, TaskId, TaskStatus},
    infrastructure::sqlite::SqliteQueue,
};

/// The payloads of `detail`'s events of `kind`, oldest first.
fn payloads<'a>(detail: &'a TaskDetail, kind: &str) -> Vec<&'a Value> {
    detail
        .events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| &event.payload)
        .collect()
}

/// The kinds of `detail`'s events, oldest first: what an assertion shows.
fn kinds(detail: &TaskDetail) -> Vec<&str> {
    detail
        .events
        .iter()
        .map(|event| event.kind.as_str())
        .collect()
}

/// The ID of the first event of `kind` of `task`.
fn first_event(queue: &mut SqliteQueue, task: TaskId, kind: &str) -> i64 {
    queue
        .show(task)
        .unwrap()
        .events
        .iter()
        .find(|event| event.kind.as_str() == kind)
        .unwrap_or_else(|| panic!("task {task} has no {kind}"))
        .id
        .as_i64()
}

/// What a review job prints that holds no verdict: the review fails.
fn no_verdict() -> Value {
    json!("no verdict here")
}

/// A review's `concern`, which asks a person to land.
fn concern() -> Value {
    verdict("concern", "a concern")
}

/// The `approve_landing` ask of `task`'s first run.
fn landing_ask(queue: &mut SqliteQueue, task: TaskId) -> Ask {
    let run = queue.show(task).unwrap().runs[0].id().clone();
    let asks = queue.asks(Default::default()).unwrap();
    asks.iter()
        .find(|ask| ask.kind == AskKind::ApproveLanding && ask.run_id.as_ref() == Some(&run))
        .unwrap_or_else(|| panic!("no approve_landing ask of task {task}: {asks:?}"))
        .clone()
}

/// The `runtime_delivers` of the answer of ask `ask` of `task`.
fn delivers(queue: &mut SqliteQueue, task: TaskId, ask: AskId) -> Value {
    queue
        .show(task)
        .unwrap()
        .events
        .into_iter()
        .find(|event| {
            event.kind.as_str() == "ask_answered" && event.payload["ask_id"] == json!(ask)
        })
        .expect("the answer is recorded")
        .payload["runtime_delivers"]
        .clone()
}

/// The `next` of the attention of ask `ask` in `status`.
fn ask_next(status: &Value, ask: AskId) -> Value {
    status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["ask_id"] == json!(ask))
        .unwrap_or_else(|| panic!("{status}"))["next"]
        .clone()
}

/// Whether `task`'s first run is at work in its turn.
fn running(queue: &mut SqliteQueue, task: TaskId) -> bool {
    queue
        .show(task)
        .unwrap()
        .runs
        .first()
        .is_some_and(|run| run.status() == RunStatus::Running)
}

/// A `land` answer, with a reason, given while the only slot runs another
/// task is applied on the next pass: the ask closes, `landing_queued`
/// (`via: approve`) is recorded and the run waits unleased, `status`
/// saying it is queued to land. Once the slot is free, it lands before
/// the next task is claimed.
#[test]
fn a_land_answer_is_applied_while_the_slot_is_taken_and_lands_before_a_new_claim() {
    const LAND_WITH_A_REASON: &str = "land: review 3, the findings are docs only";
    let wiring = Wiring::new(
        "a_land_answer_is_applied_while_the_slot_is_taken_and_lands_before_a_new_claim",
        Script {
            receipts: vec![succeeded, succeeded, succeeded],
            verdicts: vec![concern(), verdict("pass", "fine")],
        },
    )
    .with_parallel(1);
    let first = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let ask = landing_ask(&mut queue, first);

    let second = wiring.ready_task(&["true"]);
    let third = wiring.ready_task(&["true"]);
    // Task 2's turn holds the only slot until the answer is applied.
    let (release, released) = mpsc::channel();
    wiring.agent.hold_next_turn(released);
    let mut answered = false;
    let mut observed = None;
    wiring.supervise_once_while(|queue| {
        if !answered {
            if running(queue, second) {
                use crate::application::queue_commands::{AnswerParams, QueueCommand};
                use crate::domain::authorization::{AnswerPolicy, DenyReason};
                use crate::domain::{ActorContext, AuthorizationError};
                // Direct DB dispatch receives an ordinary actor, with no
                // deployment metadata supplied by the caller.
                let mut direct = wiring.queue().with_actor(ActorContext::user());
                let command = QueueCommand::Answer(AnswerParams {
                    original_text: None,
                    id: ask.id.as_i64(),
                    text: LAND_WITH_A_REASON.into(),
                });
                let before = direct.read_ask(ask.id).unwrap();
                for policy in [
                    AnswerPolicy::new(Some(true), 100),
                    AnswerPolicy::new(None, 100),
                ] {
                    let error = crate::compose::wiring_run_command_with_policy(
                        &mut direct,
                        std::path::Path::new("/wiring/queue"),
                        &command,
                        &policy,
                    )
                    .unwrap_err();
                    assert_eq!(
                        error.downcast_ref::<AuthorizationError>().unwrap().reason,
                        DenyReason::RecentLogin
                    );
                    let after = direct.read_ask(ask.id).unwrap();
                    assert_eq!(after.answer, before.answer);
                    assert_eq!(after.closed_at, before.closed_at);
                }
                crate::compose::wiring_run_command_with_policy(
                    &mut direct,
                    std::path::Path::new("/wiring/queue"),
                    &command,
                    &AnswerPolicy::new(Some(false), 100),
                )
                .unwrap();
                answered = true;
            }
            return false;
        }
        if queue.read_ask(ask.id).unwrap().closed_at.is_none() {
            return false;
        }
        let detail = queue.show(first).unwrap();
        let queued: Vec<Value> = payloads(&detail, "landing_queued")
            .into_iter()
            .cloned()
            .collect();
        observed = Some((
            queue.show(second).unwrap().runs[0].status(),
            detail.runs[0].status(),
            queued,
            queue.run_lease(detail.runs[0].id()).unwrap().is_some(),
            wiring.status(),
        ));
        let _ = release.send(());
        true
    });
    let (second_status, first_status, queued, leased, status) =
        observed.expect("the answer was applied");
    assert_eq!(second_status, RunStatus::Running);
    assert_eq!(first_status, RunStatus::AwaitingIntegration);
    assert_eq!(queued, [json!({"via": "approve", "ask_id": ask.id})]);
    assert!(!leased);
    let entry = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["task_id"] == first.as_i64())
        .unwrap_or_else(|| panic!("{status}"));
    assert_eq!(entry["next"], "queued to land (runtime)");
    assert_eq!(entry["kind"], "landing_queued");
    assert_eq!(delivers(&mut queue, first, ask.id), true);
    assert_eq!(
        queue.read_ask(ask.id).unwrap().answer.as_deref(),
        Some(LAND_WITH_A_REASON)
    );

    for task in [first, second, third] {
        let detail = queue.show(task).unwrap();
        assert_eq!(
            detail.runs[0].status(),
            RunStatus::Integrated,
            "{task} {:?}",
            kinds(&detail)
        );
        assert_eq!(payloads(&detail, "integration_started").len(), 1, "{task}");
    }
    let approved = payloads(&queue.show(first).unwrap(), "integration_approved")
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(approved.len(), 1);
    assert_eq!(approved[0]["ask_id"], ask.id.as_i64());
    // Task 2 held the slot and landed first; the approved run landed
    // before task 3 was claimed.
    assert!(
        first_event(&mut queue, second, "run_integrated")
            < first_event(&mut queue, first, "integration_started")
    );
    assert!(
        first_event(&mut queue, first, "run_integrated")
            < first_event(&mut queue, third, "run_claimed")
    );
    assert!(
        queue
            .asks(Default::default())
            .unwrap()
            .iter()
            .all(|ask| ask.closed_at.is_some())
    );
    assert_eq!(wiring.agent.review_prompts().len(), 3);
}

/// `land` answers are applied while a `[run.env]` program is missing, but
/// nothing lands until it is found; then the approved runs land in the
/// order they were approved, each once, under a supervisor other than the
/// ones that applied the answers, without asking or reviewing again.
#[test]
fn approved_runs_wait_for_the_landing_and_land_oldest_approval_first_once() {
    let wiring = Wiring::new(
        "approved_runs_wait_for_the_landing_and_land_oldest_approval_first_once",
        Script {
            receipts: vec![succeeded, succeeded],
            verdicts: vec![concern()],
        },
    );
    let first = wiring.ready_task(&["true"]);
    let second = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let (first_ask, second_ask) = (
        landing_ask(&mut queue, first).id,
        landing_ask(&mut queue, second).id,
    );

    wiring.verifier.set_program_missing(true);
    // Task 2's run is approved first, then task 1's, each by a supervisor
    // of its own.
    for ask in [second_ask, first_ask] {
        queue.answer(ask, "land").unwrap();
        wiring.supervise_once();
        assert!(queue.read_ask(ask).unwrap().closed_at.is_some(), "{ask}");
    }
    for task in [first, second] {
        let detail = queue.show(task).unwrap();
        assert_eq!(
            detail.runs[0].status(),
            RunStatus::AwaitingIntegration,
            "{:?}",
            kinds(&detail)
        );
        assert!(queue.run_lease(detail.runs[0].id()).unwrap().is_none());
        assert!(payloads(&detail, "integration_started").is_empty());
    }

    wiring.verifier.set_program_missing(false);
    wiring.supervise_once();
    for task in [first, second] {
        let detail = queue.show(task).unwrap();
        assert_eq!(
            detail.runs[0].status(),
            RunStatus::Integrated,
            "{task} {:?}",
            kinds(&detail)
        );
        assert_eq!(payloads(&detail, "integration_started").len(), 1);
        assert_eq!(payloads(&detail, "integration_approved").len(), 1);
        assert_eq!(payloads(&detail, "ask_opened").len(), 1);
    }
    assert!(
        first_event(&mut queue, second, "run_integrated")
            < first_event(&mut queue, first, "integration_started")
    );
    assert_eq!(wiring.agent.review_prompts().len(), 2);
}

/// A failed review's ask offers `retry_review` (ADR-t2521-1): the runtime
/// takes the answer (`runtime_delivers`, `applying the answer`), closes
/// the ask, records `review_retry_queued` once, and reviews the run again
/// as the next attempt on the receipt it has, without resuming the session
/// or asking for a receipt; the passing review lands it. A later
/// supervisor starts no review again. (The harness's supervisor does not
/// run an unreadable review once more, so each attempt is one job.)
#[test]
fn a_retry_review_answer_reviews_a_failed_review_s_run_again_without_a_resume() {
    let wiring = Wiring::new(
        "a_retry_review_answer_reviews_a_failed_review_s_run_again_without_a_resume",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![no_verdict(), verdict("pass", "fine")],
        },
    );
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let ask = landing_ask(&mut queue, task);
    assert!(ask.options.iter().any(|o| o == "retry_review"), "{ask:?}");
    assert!(
        ask.question.contains(
            "retry_review: run the review again on the same run and receipt (no change, no resume of the session)"
        ),
        "{}",
        ask.question
    );

    queue.answer(ask.id, "retry_review").unwrap();
    assert_eq!(delivers(&mut queue, task, ask.id), true);
    assert_eq!(
        ask_next(&wiring.status(), ask.id),
        format!("applying the answer of ask {} (runtime)", ask.id)
    );
    wiring.supervise_once();
    // Another supervisor's passes start no review again.
    wiring.supervise_once();

    let detail = queue.show(task).unwrap();
    assert_eq!(detail.runs.len(), 1);
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::Integrated,
        "{:?}",
        kinds(&detail)
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert_eq!(
        payloads(&detail, "review_retry_queued"),
        [&json!({"ask_id": ask.id, "answer": "retry_review"})]
    );
    let attempts: Vec<&Value> = payloads(&detail, "review_started")
        .into_iter()
        .map(|payload| &payload["attempt"])
        .collect();
    assert_eq!(attempts, [&json!(1), &json!(2)]);
    assert!(payloads(&detail, "resume_started").is_empty());
    assert!(payloads(&detail, "landing_decided").is_empty());
    assert!(payloads(&detail, "review_outcome").is_empty());
    assert_eq!(payloads(&detail, "receipt_observed").len(), 1);
    assert_eq!(wiring.agent.review_prompts().len(), 2);
}

/// `cancel: <reason>` chooses `cancel` as the bare option does: the run
/// fails, its task is canceled and the ask closes, the answer with its
/// reason recorded on `landing_decided`. `retry_review` is no answer of
/// the ask of a verdict (a `concern`): the runtime does not take it
/// (`runtime_delivers: false`, `read the answer`), and the ask stays open
/// with no review started again.
#[test]
fn a_cancel_with_a_reason_is_applied_and_a_concerns_retry_review_is_left_to_the_inbox() {
    let wiring = Wiring::new(
        "a_cancel_with_a_reason_is_applied_and_a_concerns_retry_review_is_left_to_the_inbox",
        Script {
            receipts: vec![succeeded, succeeded],
            verdicts: vec![concern()],
        },
    );
    let canceled = wiring.ready_task(&["true"]);
    let retried = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let cancel_ask = landing_ask(&mut queue, canceled);
    let retry_ask = landing_ask(&mut queue, retried);
    assert!(
        !retry_ask.options.iter().any(|o| o == "retry_review"),
        "{retry_ask:?}"
    );

    queue.answer(cancel_ask.id, "cancel: not needed").unwrap();
    queue.answer(retry_ask.id, "retry_review").unwrap();
    assert_eq!(delivers(&mut queue, canceled, cancel_ask.id), true);
    assert_eq!(delivers(&mut queue, retried, retry_ask.id), false);
    assert_eq!(
        ask_next(&wiring.status(), retry_ask.id),
        format!("read the answer of ask {} and close it", retry_ask.id)
    );
    let reviews = wiring.agent.review_prompts().len();
    wiring.supervise_once();

    let detail = queue.show(canceled).unwrap();
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::Failed,
        "{:?}",
        kinds(&detail)
    );
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    assert!(queue.read_ask(cancel_ask.id).unwrap().closed_at.is_some());
    let decided = payloads(&detail, "landing_decided");
    assert_eq!(decided.len(), 1, "{decided:?}");
    assert_eq!(decided[0]["answer"], "cancel: not needed", "{}", decided[0]);
    assert!(payloads(&detail, "integration_approved").is_empty());

    let detail = queue.show(retried).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(queue.read_ask(retry_ask.id).unwrap().closed_at.is_none());
    assert!(payloads(&detail, "review_retry_queued").is_empty());
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert_eq!(wiring.agent.review_prompts().len(), reviews);
}

/// A `retry_review` answered while the queue's hold ask holds Claude for
/// its usage limit is applied, and the review it queues waits held (no
/// second `review_started`) until the hold is answered; then it starts
/// once and the run lands, its session never resumed.
#[test]
fn a_retried_review_waits_for_the_held_provider() {
    let wiring = Wiring::new(
        "a_retried_review_waits_for_the_held_provider",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![no_verdict(), verdict("pass", "fine")],
        },
    );
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let ask = landing_ask(&mut queue, task);
    let hold = crate::application::AskStore::hold(
        &mut queue,
        crate::domain::NewHold {
            reason_category: crate::domain::AskReason::Cost,
            subject: Some(crate::domain::queue_hold::USAGE_LIMIT_SUBJECT.to_owned()),
            run_id: None,
            job: None,
            question: "the usage limit".into(),
            options: crate::domain::HOLD_OPTIONS
                .iter()
                .map(|option| (*option).to_owned())
                .collect(),
            asked_by: "supervisor".into(),
        },
    )
    .unwrap()
    .ask;
    queue.answer(ask.id, "retry_review").unwrap();

    // Some passes after the answer is applied, the retried review still
    // waits; then the hold is answered.
    let mut applied_at = None;
    let mut held = None;
    wiring.supervise_once_while(|queue| {
        if queue.read_ask(ask.id).unwrap().closed_at.is_none() {
            return false;
        }
        let applied_at = *applied_at.get_or_insert(wiring.passes());
        if wiring.passes() < applied_at + 5 {
            return false;
        }
        let detail = queue.show(task).unwrap();
        held = Some((
            payloads(&detail, "review_retry_queued").len(),
            payloads(&detail, "review_started").len(),
            payloads(&detail, "review_failed").len(),
        ));
        queue.answer(hold.id, "done").unwrap();
        true
    });
    assert_eq!(held, Some((1, 1, 1)));
    let detail = queue.show(task).unwrap();
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::Integrated,
        "{:?}",
        kinds(&detail)
    );
    assert_eq!(payloads(&detail, "review_started").len(), 2);
    assert!(payloads(&detail, "resume_started").is_empty());
    assert_eq!(wiring.agent.review_prompts().len(), 2);
}

/// Commit on main `[review.subagents.design]`, which reviews
/// `change.txt` (every range's change in the test's repository), and its
/// definition; no program review runs.
fn design_reviews_the_change(wiring: &Wiring) {
    const CONFIG: &str = "[review.subagents.design]\npaths = [\"change.txt\"]\n\
[review.programs.\"builtin:secrets\"]\nenabled = false\n";
    wiring.git.commit_files(
        &[
            ("dagq.toml", CONFIG),
            (
                ".dagq/agents/design/AGENT.md",
                "---\ndescription: design checks\n---\nCheck the design.\n",
            ),
        ],
        &["change.txt"],
    );
    wiring.verifier.set_config(CONFIG);
}

/// The one open ask of the queue, an `approve_landing`.
fn open_landing_ask(queue: &mut SqliteQueue) -> Ask {
    let open: Vec<Ask> = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .filter(|ask| ask.closed_at.is_none())
        .collect();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].kind, AskKind::ApproveLanding);
    open[0].clone()
}

/// The reason a bare `send_back` to ask `ask` parks the run with: the
/// subagent's finding the ask showed, under its name.
fn design_sent_back(ask: AskId) -> String {
    format!("the review's findings were sent back by ask {ask}: design: add a unit test")
}

/// A review whose subagent sends the run back under a pass with no reason
/// of its own asks a person past the run's revises. A bare `send_back` to
/// that ask, given while the run waits, names the subagent's finding the
/// ask showed, under its name, as `landing_decided`'s reason and the
/// prompt of the resume it starts; the review after that resume passes
/// and the run lands. Which findings a send_back names is
/// `supervise::execution::landing`'s.
#[test]
fn a_bare_send_back_of_a_subagents_send_back_carries_its_findings() {
    let wiring = Wiring::new(
        "a_bare_send_back_of_a_subagents_send_back_carries_its_findings",
        Script {
            receipts: vec![
                succeeded,
                succeeded_at_the_head,
                succeeded_at_the_head,
                succeeded_at_the_head,
            ],
            verdicts: vec![verdict("pass", "looks fine")],
        },
    );
    design_reviews_the_change(&wiring);
    // `design` sends the run back on its first three reviews (the last
    // past the run's revises, a person's ask), then passes.
    wiring.agent.review_agent_says(
        "design",
        &[
            ("revise", &["add a unit test"]),
            ("revise", &["add a unit test"]),
            ("revise", &["add a unit test"]),
            ("pass", &[]),
        ],
    );
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let reviewed = queue.show(task).unwrap();
    let finished = payloads(&reviewed, "review_finished");
    assert_eq!(finished.len(), 3, "{finished:?} {:?}", kinds(&reviewed));
    assert_eq!(finished[2]["verdict"], "pass");
    assert_eq!(finished[2]["reasons"], json!([]));
    assert_eq!(finished[2]["route"]["destination"], "send_back");
    let ask = open_landing_ask(&mut queue);
    assert!(
        ask.question.contains("\n- design: add a unit test"),
        "{}",
        ask.question
    );

    queue.answer(ask.id, "send_back").unwrap();
    wiring.supervise_once();
    let detail = queue.show(task).unwrap();
    let decided = payloads(&detail, "landing_decided");
    assert_eq!(decided.len(), 1, "{decided:?}");
    let reason = design_sent_back(ask.id);
    assert_eq!(decided[0]["reason"], reason);
    let prompts = wiring.agent.turn_prompts();
    assert!(
        prompts.iter().any(|prompt| prompt.contains(&reason)),
        "{prompts:?}"
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::Integrated,
        "{:?}",
        kinds(&detail)
    );
}

/// A bare `send_back` given while the landing recheck parks the waiting
/// run for a conflict (the supervisor stands still where the recheck found
/// it: another's landing moved main) applies once the run waits
/// again: `landing_decided` names the review's findings and the ask, the
/// resume's prompt holds them, and the ask closes. That an answer waits
/// while the run is not awaiting integration is
/// `supervise::execution::landing::landing_answer_to_apply`'s.
#[test]
fn a_bare_send_back_given_while_the_recheck_parks_the_run_applies_once_it_waits_again() {
    let landing_ask_id = std::sync::Arc::new(std::sync::Mutex::new(None::<AskId>));
    let answered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let wiring = Wiring::new(
        "a_bare_send_back_given_while_the_recheck_parks_the_run_applies_once_it_waits_again",
        Script {
            receipts: vec![
                succeeded,
                succeeded_at_the_head,
                succeeded_at_the_head,
                succeeded_at_the_head,
            ],
            verdicts: vec![
                json!({"verdict": "concern", "reasons": ["add a unit test"], "summary": "a concern"}),
                verdict("pass", "fine"),
            ],
        },
    );
    // Answer the ask as the recheck parks the run, once.
    let wiring = {
        let (db, ask, answered) = (wiring.db.clone(), landing_ask_id.clone(), answered.clone());
        let generators = wiring.generators();
        wiring.with_pauses(std::sync::Arc::new(move |at, _| {
            let ask = *ask
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if at == crate::application::supervise::PausePoint::RecheckFailure
                && let Some(ask) = ask
                && !answered.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                SqliteQueue::open(&db)
                    .unwrap()
                    .with_generators(generators.clone())
                    .answer(ask, "send_back")
                    .unwrap();
            }
        }))
    };
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let ask = landing_ask(&mut queue, task);
    *landing_ask_id
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ask.id);

    // Main moves on by another's landing, and the recheck finds the
    // waiting run conflicting with it.
    wiring.git.conflict_once_main_moves(&["change.txt"]);
    wiring.git.move_main();
    wiring.supervise_once();
    assert!(answered.load(std::sync::atomic::Ordering::SeqCst));
    // A later pass applies it if the first supervisor ended before it.
    wiring.supervise_once();

    let detail = queue.show(task).unwrap();
    assert_eq!(
        payloads(&detail, "landing_recheck_failed").len(),
        1,
        "{:?}",
        kinds(&detail)
    );
    let decided = payloads(&detail, "landing_decided");
    assert_eq!(decided.len(), 1, "{decided:?} {:?}", kinds(&detail));
    let reason = format!(
        "the review's findings were sent back by ask {}: add a unit test",
        ask.id
    );
    assert_eq!(decided[0]["reason"], reason);
    let prompts = wiring.agent.turn_prompts();
    assert!(
        prompts.iter().any(|prompt| prompt.contains(&reason)),
        "{prompts:?}"
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
}

/// A supervisor that drains for a stop applies the `land`, `send_back`,
/// `cancel` and `retry_review` answers of runs nobody leases on its passes
/// and closes their asks (ADR-t3330-1), but starts no new work: no claim,
/// no resume of the run sent back, no review of the run queued to be
/// reviewed again, and, with no run of its own left, no landing of the
/// approved run, which stays queued.
#[test]
fn a_draining_supervisor_applies_the_landing_answers_and_starts_no_new_work() {
    // One slot: the tasks are reviewed one after another, task 4's review
    // failing.
    let wiring = Wiring::new(
        "a_draining_supervisor_applies_the_landing_answers_and_starts_no_new_work",
        Script {
            receipts: vec![succeeded, succeeded, succeeded, succeeded],
            verdicts: vec![concern(), concern(), concern(), no_verdict()],
        },
    )
    .with_parallel(1);
    let tasks: Vec<TaskId> = (0..4).map(|_| wiring.ready_task(&["true"])).collect();
    wiring.supervise_once();
    let reviews = wiring.agent.review_prompts().len();
    let mut queue = wiring.queue();
    let answers: Vec<(AskId, &str)> = tasks
        .iter()
        .zip(["land", "send_back", "cancel", "retry_review"])
        .map(|(task, answer)| (landing_ask(&mut queue, *task).id, answer))
        .collect();
    for (ask, answer) in &answers {
        queue.answer(*ask, answer).unwrap();
    }
    let ready = wiring.ready_task(&["true"]);

    // The drain has room for every run: what it starts no slot keeps.
    wiring.set_parallel(4);
    wiring.supervise_draining();
    for (ask, _) in &answers {
        assert!(queue.read_ask(*ask).unwrap().closed_at.is_some(), "{ask}");
    }
    let landed = queue.show(tasks[0]).unwrap();
    assert_eq!(
        landed.runs[0].status(),
        RunStatus::AwaitingIntegration,
        "{:?}",
        kinds(&landed)
    );
    assert!(queue.run_lease(landed.runs[0].id()).unwrap().is_none());
    assert_eq!(payloads(&landed, "integration_approved").len(), 1);
    assert_eq!(payloads(&landed, "landing_queued").len(), 1);
    assert!(payloads(&landed, "integration_started").is_empty());
    let sent_back = queue.show(tasks[1]).unwrap();
    assert_eq!(sent_back.runs[0].status(), RunStatus::NeedsSession);
    assert_eq!(payloads(&sent_back, "landing_decided").len(), 1);
    assert!(payloads(&sent_back, "resume_started").is_empty());
    let canceled = queue.show(tasks[2]).unwrap();
    assert_eq!(canceled.runs[0].status(), RunStatus::Failed);
    assert_eq!(canceled.task.status(), TaskStatus::Canceled);
    let retried = queue.show(tasks[3]).unwrap();
    assert_eq!(payloads(&retried, "review_retry_queued").len(), 1);
    assert_eq!(payloads(&retried, "review_started").len(), 1);
    assert!(queue.show(ready).unwrap().runs.is_empty());
    assert_eq!(wiring.agent.review_prompts().len(), reviews);
}

/// A `land` answer given while a supervisor drains for a stop, its own run
/// still at work, is applied and the approved run lands in that drain,
/// before the drain ends with its own run (ADR-t3330-1). The approved run
/// is not reviewed again.
#[test]
fn a_drain_lands_an_approved_run_while_its_own_run_keeps_it_going() {
    let wiring = Wiring::new(
        "a_drain_lands_an_approved_run_while_its_own_run_keeps_it_going",
        Script {
            receipts: vec![succeeded, succeeded],
            verdicts: vec![concern(), verdict("pass", "fine")],
        },
    );
    let approved = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let ask = landing_ask(&mut queue, approved);

    let own = wiring.ready_task(&["true"]);
    let (release, released) = mpsc::channel();
    wiring.agent.hold_next_turn(released);
    let mut stopped = false;
    let mut answered = false;
    let mut at_landing = None;
    let outcome = wiring.supervise_once_while_stoppable(|queue, stop| {
        if !stopped {
            if running(queue, own) {
                stop();
                stopped = true;
            }
            return false;
        }
        if !answered {
            // Answered once the drain is recorded: no pass at work applies it.
            let draining = queue
                .all_events()
                .unwrap()
                .iter()
                .any(|event| event.kind.as_str() == "supervisor_draining");
            if draining {
                queue.answer(ask.id, "land").unwrap();
                answered = true;
            }
            return false;
        }
        if queue.show(approved).unwrap().runs[0].status() != RunStatus::Integrated {
            return false;
        }
        // Landed while the drain went on for its own run.
        at_landing = Some((
            running(queue, own),
            queue.read_ask(ask.id).unwrap().closed_at.is_some(),
        ));
        let _ = release.send(());
        true
    });
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(at_landing, Some((true, true)));
    let detail = queue.show(approved).unwrap();
    assert_eq!(payloads(&detail, "integration_approved").len(), 1);
    assert_eq!(payloads(&detail, "integration_started").len(), 1);
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    let own = queue.show(own).unwrap();
    assert_eq!(
        own.runs[0].status(),
        RunStatus::Integrated,
        "{:?}",
        kinds(&own)
    );
    assert_eq!(wiring.agent.review_prompts().len(), 2);
}

/// A drain with no run of its own applies a `land` answer and ends without
/// landing it; the next supervisor lands the approved run from its events,
/// without reviewing or asking again (ADR-t3330-1).
#[test]
fn a_drain_with_no_run_of_its_own_leaves_the_approved_run_to_the_next_supervisor() {
    let wiring = Wiring::new(
        "a_drain_with_no_run_of_its_own_leaves_the_approved_run_to_the_next_supervisor",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![concern()],
        },
    );
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let ask = landing_ask(&mut queue, task);
    queue.answer(ask.id, "land").unwrap();

    wiring.supervise_draining();
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let detail = queue.show(task).unwrap();
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::AwaitingIntegration,
        "{:?}",
        kinds(&detail)
    );
    assert_eq!(payloads(&detail, "landing_queued").len(), 1);
    assert!(payloads(&detail, "integration_started").is_empty());

    let base = wiring.git.main();
    wiring.supervise_once();
    let detail = queue.show(task).unwrap();
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::Integrated,
        "{:?}",
        kinds(&detail)
    );
    assert_ne!(wiring.git.main(), base);
    assert_eq!(payloads(&detail, "integration_approved").len(), 1);
    assert_eq!(payloads(&detail, "integration_started").len(), 1);
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert_eq!(payloads(&detail, "ask_opened").len(), 1);
    assert_eq!(wiring.agent.review_prompts().len(), 1);
}
