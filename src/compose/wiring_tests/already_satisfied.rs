//! A worker's `already_satisfied` receipt (ADR-t1818-1) wired from the
//! claim to the person's ask and on to the close: the claim is validated
//! without a commit, the task's verification runs on a snapshot of main's
//! head, the review judges the claim on it, the supervisor asks to close,
//! and the `close` answer completes the task without a commit so that the
//! task after it is claimed and lands.

use std::sync::mpsc;

use serde_json::json;

use super::{
    agent::{Script, already_satisfied, login_wall, succeeded, verdict},
    harness::Wiring,
};
use crate::{
    application::TaskReads,
    domain::{Answerer, AskKind, RunStatus, TaskDetail, TaskStatus},
};

/// The `requested_by` of the actor of each of `detail`'s events of `kind`.
fn requested_by(detail: &TaskDetail, kind: &str) -> Vec<Option<String>> {
    detail
        .events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| {
            event
                .actor
                .as_ref()
                .and_then(|actor| actor.requested_by.clone())
        })
        .collect()
}

/// What a replanning's park leaves of `task`'s claim: the operation
/// parked, the run waiting for a session without a lease, no close, the
/// task in progress and `dependent` not claimed.
fn assert_parked_unclosed(
    wiring: &Wiring,
    op: &crate::domain::task_replan::ReplanOperation,
    task: crate::domain::TaskId,
    dependent: crate::domain::TaskId,
) {
    let mut queue = wiring.queue();
    let operation = queue.replan_operation(op.id).unwrap();
    assert_eq!(
        operation.status,
        crate::domain::task_replan::ReplanStatus::Parked,
        "{:?}",
        operation.status_reason
    );
    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.task.status(), TaskStatus::InProgress, "{kinds:?}");
    assert_eq!(
        detail.runs[0].status(),
        RunStatus::NeedsSession,
        "{kinds:?}"
    );
    assert_eq!(payloads(&detail, "replan_parked").len(), 1, "{kinds:?}");
    assert!(payloads(&detail, "run_satisfied_closed").is_empty());
    assert!(queue.run_lease(detail.runs[0].id()).unwrap().is_none());
    assert!(queue.show(dependent).unwrap().runs.is_empty());
}

/// The entries of `status`'s attention that name `key` with `value`.
fn attention<'a>(
    status: &'a serde_json::Value,
    key: &str,
    value: &serde_json::Value,
) -> Vec<&'a serde_json::Value> {
    status["attention"]
        .as_array()
        .expect("the status has its attention")
        .iter()
        .filter(|entry| entry[key] == *value)
        .collect()
}

/// Whether the queue has an `approve_close` ask, open or not.
fn asked_to_close(wiring: &Wiring) -> bool {
    wiring
        .queue()
        .asks(Default::default())
        .unwrap()
        .iter()
        .any(|ask| ask.kind == AskKind::ApproveClose)
}

/// The payloads of `detail`'s events of `kind`, oldest first.
fn payloads<'a>(detail: &'a TaskDetail, kind: &str) -> Vec<&'a serde_json::Value> {
    detail
        .events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| &event.payload)
        .collect()
}

/// A ready task whose worker claims its base meets it, its two commands
/// passing and its review passing: the supervisor claims it, starts its
/// worker's session, validates the receipt as `already_satisfied`, runs
/// both commands on a snapshot of main's head, has the review judge the
/// claim there and asks a person to close it, the run left validating
/// without a lease.
#[test]
fn a_satisfied_claim_is_verified_on_main_and_asked_to_close() {
    let wiring = Wiring::new(
        "a_satisfied_claim_is_verified_on_main_and_asked_to_close",
        Script {
            receipts: vec![already_satisfied],
            verdicts: vec![verdict("pass", "fine")],
        },
    );
    let task = wiring.ready_task(&["test -f seed.txt", "test ! -f broken.txt"]);
    wiring.supervise_once();

    let head = wiring.git.main();
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail
        .events
        .iter()
        .map(|event| event.kind.as_str())
        .collect();
    assert_eq!(detail.runs.len(), 1, "{kinds:?}");
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Validating, "{kinds:?}");
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert_eq!(payloads(&detail, "run_claimed").len(), 1, "{kinds:?}");
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 1, "{kinds:?}");
    assert_eq!(validated[0]["already_satisfied"], true);
    assert_eq!(validated[0]["status"], "validating");
    assert_eq!(
        payloads(&detail, "run_satisfied_snapshot"),
        [&json!({"head": head})]
    );
    assert_eq!(
        payloads(&detail, "run_satisfied_verified"),
        [&json!({"head": head, "passed": true, "commands": 2})]
    );
    // The verification ran in the run's scratch checkout of main's head.
    let ran = wiring.verifier.ran();
    assert_eq!(
        ran.iter()
            .map(|(command, _)| command.as_str())
            .collect::<Vec<_>>(),
        ["test -f seed.txt", "test ! -f broken.txt"]
    );
    let checked_out = wiring.git.checked_out();
    assert!(
        !checked_out.is_empty() && checked_out.iter().all(|(_, commit)| *commit == head),
        "{checked_out:?}"
    );
    assert!(
        ran.iter().all(|(_, cwd)| *cwd == checked_out[0].0),
        "{ran:?} {checked_out:?}"
    );
    // One turn of the worker, whose receipt was observed, and one job of the
    // review, whose pass is not `high` and so asks.
    assert_eq!(
        wiring.agent.started(),
        ["wiring-worker-turn", "wiring-review"]
    );
    assert_eq!(payloads(&detail, "receipt_observed").len(), 1, "{kinds:?}");
    assert_eq!(
        payloads(&detail, "run_satisfied_review_job_started").len(),
        1,
        "{kinds:?}"
    );
    let judged = payloads(&detail, "run_satisfied_review_job_finished");
    assert_eq!(judged.len(), 1, "{kinds:?}");
    assert_eq!(judged[0]["verdict"], "pass");
    let judged = payloads(&detail, "run_satisfied_judged");
    assert_eq!(judged.len(), 1, "{kinds:?}");
    assert_eq!(judged[0]["step"], "ask");
    assert_eq!(judged[0]["head"], json!(head));
    // The judgment ran in a scratch checkout of main's head, not the run's
    // worktree, and its event and the ask's are the job's.
    let reviewed_in = wiring.agent.reviewed_in();
    assert_eq!(reviewed_in.len(), 1, "{reviewed_in:?}");
    assert!(
        checked_out.contains(&(reviewed_in[0].clone(), head.clone())),
        "{reviewed_in:?} {checked_out:?}"
    );
    assert_ne!(
        Some(reviewed_in[0].as_path()),
        run.worktree_path().map(std::path::Path::new)
    );
    let job = Some(format!("review-job:{}:satisfied-1", run.id()));
    for kind in ["run_satisfied_judged", "ask_opened"] {
        assert_eq!(
            requested_by(&detail, kind),
            std::slice::from_ref(&job),
            "{kind}"
        );
    }
    // The wrapper was asked to exit, and its session closed.
    assert_eq!(payloads(&detail, "session_closed").len(), 1, "{kinds:?}");
    assert_eq!(payloads(&detail, "lease_released").len(), 1, "{kinds:?}");

    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::ApproveClose);
    assert_eq!(asks[0].options, ["close", "retry", "cancel"]);
    assert!(
        asks[0].question.contains(head.as_str()),
        "{}",
        asks[0].question
    );
}

/// The claim of [`a_satisfied_claim_is_verified_on_main_and_asked_to_close`]
/// with a task that depends on it: the dependent waits for the ask, and a
/// person's `close` completes the claimed task without a commit or a second
/// verification (main did not move), closes the ask, and frees the
/// dependent, which is claimed on main's head, lands one commit on main and
/// pushes it.
#[test]
fn a_close_answer_completes_the_claim_and_its_dependent_lands() {
    let wiring = Wiring::new(
        "a_close_answer_completes_the_claim_and_its_dependent_lands",
        Script {
            receipts: vec![already_satisfied, succeeded],
            verdicts: vec![verdict("pass", "fine")],
        },
    );
    let task = wiring.ready_task(&["test -f seed.txt", "test ! -f broken.txt"]);
    let dependent = wiring.ready_task_after(&["test -f seed.txt"], &[task]);
    wiring.supervise_once();

    let head = wiring.git.main();
    let mut queue = wiring.queue();
    assert!(queue.show(dependent).unwrap().runs.is_empty());
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::ApproveClose);
    let ask = asks[0].id;
    // The claim resting on its ask is no orphan for a person's recover,
    // and the attention is its ask, not the unleased validating run.
    let claim = queue.show(task).unwrap().runs[0].id().clone();
    let refused = wiring.recover(&claim).unwrap_err().to_string();
    assert!(refused.contains("approve_close"), "{refused}");
    let status = wiring.status();
    assert!(
        attention(&status, "run_id", &json!(claim))
            .iter()
            .all(|entry| !entry["ask_id"].is_null()),
        "{status}"
    );
    assert_eq!(
        attention(&status, "ask_id", &json!(ask)).len(),
        1,
        "{status}"
    );
    queue
        .answer_as(ask, "close", Answerer::PERSON)
        .expect("answer the close");
    // The answer is the supervisor's to apply, not the inbox's to read.
    let status = wiring.status();
    let answered = attention(&status, "ask_id", &json!(ask));
    assert_eq!(answered.len(), 1, "{status}");
    assert_eq!(
        answered[0]["next"],
        format!("applying the answer of ask {ask} (runtime)"),
        "{status}"
    );
    wiring.supervise_once();

    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail
        .events
        .iter()
        .map(|event| event.kind.as_str())
        .collect();
    assert_eq!(detail.task.status(), TaskStatus::Completed, "{kinds:?}");
    assert_eq!(detail.runs.len(), 1, "{kinds:?}");
    assert_eq!(detail.runs[0].status(), RunStatus::Succeeded, "{kinds:?}");
    assert_eq!(detail.runs[0].result_commit(), None);
    let closed = payloads(&detail, "run_satisfied_closed");
    assert_eq!(closed.len(), 1, "{kinds:?}");
    assert_eq!(closed[0]["head"], json!(head));
    assert_eq!(closed[0]["ask_id"], json!(ask));
    assert_eq!(closed[0]["rechecked"], false);
    assert_eq!(payloads(&detail, "run_satisfied_verified").len(), 1);
    assert!(payloads(&detail, "run_integrated").is_empty(), "{kinds:?}");
    assert!(queue.read_ask(ask).unwrap().closed_at.is_some());

    let after = queue.show(dependent).unwrap();
    let kinds: Vec<&str> = after
        .events
        .iter()
        .map(|event| event.kind.as_str())
        .collect();
    assert_eq!(after.task.status(), TaskStatus::Completed, "{kinds:?}");
    assert_eq!(after.runs.len(), 1, "{kinds:?}");
    let run = &after.runs[0];
    assert_eq!(run.status(), RunStatus::Integrated, "{kinds:?}");
    assert_eq!(run.base_commit(), &head);
    // One squash commit on the head the claim was closed on, pushed.
    let landed = wiring.git.main();
    assert_eq!(wiring.git.main_line(), [head.clone(), landed.clone()]);
    assert_eq!(run.result_commit(), Some(&landed));
    assert_eq!(wiring.git.pushed(), Some(landed.clone()));
    let integrated = payloads(&after, "run_integrated");
    assert_eq!(integrated.len(), 1, "{kinds:?}");
    assert_eq!(integrated[0]["commit"], json!(landed));
    assert_eq!(integrated[0]["main_before"], json!(head));
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert!(queue.asks(Default::default()).unwrap().is_empty());
}

/// The claim of [`a_satisfied_claim_is_verified_on_main_and_asked_to_close`]
/// answered while a supervisor runs another task, whose worker's turn holds
/// a slot, after main moved on: that running supervisor applies the
/// `close`, which does not close on the old snapshot but verifies again on
/// main's new head in the free slot, recorded with the ask, and closes on
/// that before the other task's turn ends; the other task then lands on the
/// new head.
#[test]
fn a_close_answered_after_main_moved_verifies_the_new_head_first() {
    let wiring = Wiring::new(
        "a_close_answered_after_main_moved_verifies_the_new_head_first",
        Script {
            receipts: vec![already_satisfied, succeeded],
            verdicts: vec![verdict("pass", "fine")],
        },
    );
    let task = wiring.ready_task(&["test -f seed.txt", "test ! -f broken.txt"]);
    wiring.supervise_once();
    let first = wiring.git.main();
    let mut queue = wiring.queue();
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].id;
    // Another task's worker holds its slot in its turn until the claim is
    // closed (or the test's deadline passes).
    let other = wiring.ready_task(&["test -f seed.txt"]);
    let (release, released) = mpsc::channel();
    wiring.agent.hold_next_turn(released);
    let mut moved = None;
    let mut other_at_close = None;
    wiring.supervise_once_while(|queue| {
        if moved.is_none() {
            let running = queue
                .show(other)
                .unwrap()
                .runs
                .first()
                .is_some_and(|run| run.status() == RunStatus::Running);
            if running {
                moved = Some(wiring.git.move_main());
                queue
                    .answer_as(ask, "close", Answerer::PERSON)
                    .expect("answer the close");
            }
            return false;
        }
        if queue.show(task).unwrap().task.status() != TaskStatus::Completed {
            return false;
        }
        other_at_close = Some(queue.show(other).unwrap().runs[0].status());
        let _ = release.send(());
        true
    });
    let moved = moved.expect("the other task's run started");
    assert_ne!(moved, first);
    // The close was applied while the other task's run held its slot.
    assert_eq!(other_at_close, Some(RunStatus::Running));

    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail
        .events
        .iter()
        .map(|event| event.kind.as_str())
        .collect();
    assert_eq!(detail.task.status(), TaskStatus::Completed, "{kinds:?}");
    assert_eq!(detail.runs[0].result_commit(), None);
    assert_eq!(
        payloads(&detail, "run_satisfied_snapshot"),
        [
            &json!({"head": first}),
            &json!({"head": moved, "ask_id": ask})
        ]
    );
    let verified = payloads(&detail, "run_satisfied_verified");
    assert_eq!(verified.len(), 2, "{kinds:?}");
    assert_eq!(verified[1]["head"], json!(moved));
    assert_eq!(verified[1]["passed"], true);
    let closed = payloads(&detail, "run_satisfied_closed");
    assert_eq!(closed.len(), 1, "{kinds:?}");
    assert_eq!(closed[0]["head"], json!(moved));
    assert_eq!(closed[0]["ask_id"], json!(ask));
    assert_eq!(closed[0]["rechecked"], true);
    // Each verification ran on a checkout of the head it names.
    let heads: Vec<_> = wiring
        .git
        .checked_out()
        .into_iter()
        .map(|(_, commit)| commit)
        .collect();
    assert!(
        heads.contains(&first) && heads.contains(&moved),
        "{heads:?}"
    );
    assert!(queue.read_ask(ask).unwrap().closed_at.is_some());

    // The other task landed one commit on the new head, the close none.
    let after = queue.show(other).unwrap();
    let kinds: Vec<&str> = after
        .events
        .iter()
        .map(|event| event.kind.as_str())
        .collect();
    assert_eq!(after.runs[0].status(), RunStatus::Integrated, "{kinds:?}");
    let landed = wiring.git.main();
    assert_eq!(
        wiring.git.main_line(),
        [first, moved.clone(), landed.clone()]
    );
    let integrated = payloads(&after, "run_integrated");
    assert_eq!(integrated.len(), 1, "{kinds:?}");
    assert_eq!(integrated[0]["main_before"], json!(moved));
    assert_eq!(integrated[0]["commit"], json!(landed));
}

/// A `high` pass on the snapshot main is still at closes the claim with no
/// ask, on the judgment's word, and its dependent is claimed and lands.
#[test]
fn a_high_pass_on_the_snapshot_closes_the_claim_with_no_ask() {
    let wiring = Wiring::new(
        "a_high_pass_on_the_snapshot_closes_the_claim_with_no_ask",
        Script {
            receipts: vec![already_satisfied, succeeded],
            verdicts: vec![
                json!({"verdict": "pass", "reasons": ["seed.txt is there"],
                       "summary": "main meets it", "confidence": "high",
                       "recommendation": "close"}),
                verdict("pass", "fine"),
            ],
        },
    );
    let task = wiring.ready_task(&["test -f seed.txt"]);
    let dependent = wiring.ready_task_after(&["test -f seed.txt"], &[task]);
    let head = wiring.git.main();
    wiring.supervise_once();

    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail
        .events
        .iter()
        .map(|event| event.kind.as_str())
        .collect();
    assert_eq!(detail.task.status(), TaskStatus::Completed, "{kinds:?}");
    assert_eq!(detail.runs[0].result_commit(), None);
    assert!(!kinds.contains(&"ask_opened"), "{kinds:?}");
    let closed = payloads(&detail, "run_satisfied_closed");
    assert_eq!(closed.len(), 1, "{kinds:?}");
    assert_eq!(closed[0]["head"], json!(head));
    assert_eq!(closed[0]["closed_by"], "judgment");
    assert_eq!(closed[0]["confidence"], "high");
    let job = Some(format!("review-job:{}:satisfied-1", detail.runs[0].id()));
    for kind in ["run_satisfied_judged", "run_satisfied_closed"] {
        assert_eq!(
            requested_by(&detail, kind),
            std::slice::from_ref(&job),
            "{kind}"
        );
    }
    let after = queue.show(dependent).unwrap();
    assert_eq!(after.runs.len(), 1);
    assert_eq!(after.runs[0].status(), RunStatus::Integrated);
    assert_eq!(wiring.git.main_line().len(), 2);
    assert_eq!(wiring.git.pushed(), Some(wiring.git.main()));
    assert!(queue.asks(Default::default()).unwrap().is_empty());
}

/// A stop asked before the pass (a draining supervisor) applies no
/// `close` answered after main moved: it starts no verification again, the
/// ask stays unclosed and the run validating, for the next supervisor.
#[test]
fn a_draining_supervisor_verifies_no_claim_for_its_close_answer() {
    let wiring = Wiring::new(
        "a_draining_supervisor_verifies_no_claim_for_its_close_answer",
        Script {
            receipts: vec![already_satisfied],
            verdicts: vec![verdict("pass", "fine")],
        },
    );
    let task = wiring.ready_task(&["test -f seed.txt"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].id;
    wiring.git.move_main();
    queue
        .answer_as(ask, "close", Answerer::PERSON)
        .expect("answer the close");
    wiring.supervise_draining();

    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(detail.runs[0].status(), RunStatus::Validating, "{kinds:?}");
    assert_eq!(
        payloads(&detail, "run_satisfied_snapshot").len(),
        1,
        "{kinds:?}"
    );
    assert_eq!(
        payloads(&detail, "run_satisfied_verified").len(),
        1,
        "{kinds:?}"
    );
    assert!(payloads(&detail, "run_satisfied_closed").is_empty());
    assert!(queue.read_ask(ask).unwrap().closed_at.is_none());
}

/// A claim resting unleased on its close question is parked where it
/// rests once a replanning of its run asks to pause: a `close` answered
/// after that fence neither closes the task nor frees its dependent, the
/// park closes the ask, and the close's own transaction refuses a task
/// being replanned.
#[test]
fn a_claim_waiting_for_its_close_answer_is_parked_and_the_answer_does_not_close_it() {
    let wiring = Wiring::new(
        "a_claim_waiting_for_its_close_answer_is_parked_and_the_answer_does_not_close_it",
        Script {
            receipts: vec![already_satisfied],
            verdicts: vec![verdict("pass", "fine")],
        },
    );
    let task = wiring.ready_task(&["test -f seed.txt"]);
    let dependent = wiring.ready_task_after(&["test -f seed.txt"], &[task]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].id;
    let run = queue.show(task).unwrap().runs[0].id().clone();
    let op = wiring.pause_replan(&run);
    queue
        .answer_as(ask, "close", Answerer::PERSON)
        .expect("answer the close");
    wiring.supervise_once();

    assert_parked_unclosed(&wiring, &op, task, dependent);
    assert!(queue.read_ask(ask).unwrap().closed_at.is_some());
    let refused =
        crate::application::RunTransitions::close_satisfied(&mut queue, &run, None, json!({}))
            .unwrap_err()
            .to_string();
    assert!(refused.contains("being replanned"), "{refused}");
}

/// A replanning asked to pause while the claim's verification runs lets
/// the verification end, but its result opens no `approve_close` ask: it
/// is withheld (`replan_result_withheld` of the verification, passed), the
/// run is parked out of its slot and its task stays in progress.
#[test]
fn a_claim_whose_verification_runs_at_the_fence_is_parked_without_its_ask() {
    let wiring = Wiring::new(
        "a_claim_whose_verification_runs_at_the_fence_is_parked_without_its_ask",
        Script {
            receipts: vec![already_satisfied],
            verdicts: vec![verdict("pass", "fine")],
        },
    );
    let task = wiring.ready_task(&["test -f seed.txt", "test ! -f broken.txt"]);
    let dependent = wiring.ready_task_after(&["test -f seed.txt"], &[task]);
    let paused = std::sync::Arc::new(std::sync::Mutex::new(None));
    {
        let (db, generators, paused) = (wiring.db.clone(), wiring.generators(), paused.clone());
        wiring.verifier.on_command(
            "test ! -f broken.txt",
            Box::new(move || {
                let mut queue = crate::infrastructure::sqlite::SqliteQueue::open(&db).unwrap();
                let run = queue.show(task).unwrap().runs[0].id().clone();
                *paused.lock().unwrap() =
                    Some(super::harness::pause_replan(&db, &generators, &run));
            }),
        );
    }
    wiring.supervise_once();

    let op = paused
        .lock()
        .unwrap()
        .clone()
        .expect("the verification ran");
    assert_parked_unclosed(&wiring, &op, task, dependent);
    let detail = wiring.queue().show(task).unwrap();
    let withheld = payloads(&detail, "replan_result_withheld");
    assert_eq!(withheld.len(), 1, "{withheld:?}");
    assert_eq!(withheld[0]["what"], "satisfied_verification");
    assert_eq!(withheld[0]["result"]["passed"], true);
    assert!(payloads(&detail, "run_satisfied_verified").is_empty());
    assert!(!asked_to_close(&wiring));
}

/// A replanning asked to pause while the review judges the claim lets the
/// judgment job end and be reaped (not stopped), but its `high` pass
/// neither closes the task nor asks: it is withheld under the operation,
/// and the run is parked.
#[test]
fn a_claim_whose_judgment_runs_at_the_fence_is_parked_without_its_ask() {
    let wiring = Wiring::new(
        "a_claim_whose_judgment_runs_at_the_fence_is_parked_without_its_ask",
        Script {
            receipts: vec![already_satisfied],
            verdicts: vec![json!({"verdict": "pass", "reasons": [], "summary": "met",
                                  "confidence": "high"})],
        },
    );
    let task = wiring.ready_task(&["test -f seed.txt"]);
    let dependent = wiring.ready_task_after(&["test -f seed.txt"], &[task]);
    let paused = std::sync::Arc::new(std::sync::Mutex::new(None));
    {
        let (db, generators, paused) = (wiring.db.clone(), wiring.generators(), paused.clone());
        wiring.agent.on_review(Box::new(move || {
            let mut queue = crate::infrastructure::sqlite::SqliteQueue::open(&db).unwrap();
            let run = queue.show(task).unwrap().runs[0].id().clone();
            *paused.lock().unwrap() = Some(super::harness::pause_replan(&db, &generators, &run));
        }));
    }
    wiring.supervise_once();

    let op = paused.lock().unwrap().clone().expect("the judgment ran");
    assert_parked_unclosed(&wiring, &op, task, dependent);
    let detail = wiring.queue().show(task).unwrap();
    let ended = payloads(&detail, "run_satisfied_review_job_finished");
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(ended[0]["outcome"], "answered");
    assert!(payloads(&detail, "headless_job_stopped").is_empty());
    let withheld = payloads(&detail, "replan_result_withheld");
    assert_eq!(withheld.len(), 1, "{withheld:?}");
    assert_eq!(withheld[0]["what"], "satisfied_judgment");
    assert_eq!(withheld[0]["operation_id"], json!(op.id));
    assert_eq!(withheld[0]["result"]["verdict"], "pass");
    assert!(payloads(&detail, "run_satisfied_judged").is_empty());
    assert!(!asked_to_close(&wiring));
}

/// The verification set to a container the host cannot use: the claim
/// rests accepted and unleased with nothing verified on the host (no
/// snapshot, no command, no checkout), and is no orphan for `recover`.
/// Back on the host it is verified and asked. A `close` answered while the
/// container is set again (main having moved) waits too, and is applied,
/// verifying on the new head, once the host is back.
#[test]
fn a_verification_set_to_a_held_container_runs_nothing_on_the_host() {
    let wiring = Wiring::new(
        "a_verification_set_to_a_held_container_runs_nothing_on_the_host",
        Script {
            receipts: vec![already_satisfied],
            verdicts: vec![verdict("pass", "fine")],
        },
    );
    let task = wiring.ready_task(&["test -f seed.txt"]);
    wiring.verifier.set_in_container(true);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Validating, "{kinds:?}");
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    assert!(
        payloads(&detail, "run_satisfied_snapshot").is_empty(),
        "{kinds:?}"
    );
    assert!(queue.asks(Default::default()).unwrap().is_empty());
    assert!(wiring.verifier.ran().is_empty());
    assert!(wiring.git.checked_out().is_empty());
    let refused = wiring.recover(run.id()).unwrap_err().to_string();
    assert!(refused.contains("already_satisfied"), "{refused}");
    // The queue's attention says why: the container, not the resting run.
    let status = wiring.status();
    assert!(
        !attention(&status, "kind", &json!("container_runtime_unavailable")).is_empty(),
        "{status}"
    );
    assert!(
        attention(&status, "run_id", &json!(run.id())).is_empty(),
        "{status}"
    );

    wiring.verifier.set_in_container(false);
    wiring.supervise_once();
    let detail = queue.show(task).unwrap();
    assert_eq!(payloads(&detail, "run_satisfied_snapshot").len(), 1);
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].id;

    wiring.git.move_main();
    wiring.verifier.set_in_container(true);
    queue
        .answer_as(ask, "close", Answerer::PERSON)
        .expect("answer the close");
    wiring.supervise_once();
    let detail = queue.show(task).unwrap();
    assert_eq!(payloads(&detail, "run_satisfied_snapshot").len(), 1);
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert!(queue.read_ask(ask).unwrap().closed_at.is_none());

    wiring.verifier.set_in_container(false);
    wiring.supervise_once();
    let detail = queue.show(task).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(
        payloads(&detail, "run_satisfied_closed")[0]["rechecked"],
        true
    );
}

/// The claim's judgment job meets a login: the job joins the
/// authentication `queue_hold` of the review and ends `held`, and while
/// the ask is open the claim is neither judged nor asked to close. Once a
/// person answers `done`, the hold is applied to that job and the same
/// judgment (one start, on the same head) runs its job again as attempt 1,
/// not as a run once more; its `low` pass asks to close.
#[test]
fn a_judgment_stopped_at_a_login_waits_for_the_hold_and_runs_again_on_its_head_after_done() {
    let wiring = Wiring::new(
        "a_judgment_stopped_at_a_login_waits_for_the_hold_and_runs_again_on_its_head_after_done",
        Script {
            receipts: vec![already_satisfied],
            verdicts: vec![
                login_wall(),
                json!({"verdict": "pass", "reasons": ["seed.txt is there"],
                       "summary": "met", "confidence": "low", "recommendation": "close"}),
            ],
        },
    );
    let task = wiring.ready_task(&["test -f seed.txt"]);
    let head = wiring.git.main();
    // What the claim shows while the hold is open, then the hold answered.
    let mut seen = None;
    wiring.supervise_once_while(|queue| {
        let Some(hold) = queue
            .asks(crate::application::AskQuery {
                open: true,
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .find(|ask| ask.kind == AskKind::QueueHold)
        else {
            return false;
        };
        let detail = queue.show(task).unwrap();
        // The job's finish is written after its `auth_required`.
        if payloads(&detail, "run_satisfied_review_job_finished").is_empty() {
            return false;
        }
        let asked_to_close = queue
            .asks(Default::default())
            .unwrap()
            .iter()
            .any(|ask| ask.kind == AskKind::ApproveClose);
        queue
            .answer_as(hold.id, "done", Answerer::PERSON)
            .expect("answer the hold");
        seen = Some((hold, detail, asked_to_close));
        true
    });

    let (hold, detail, asked) = seen.expect("the login held the review");
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    let run = detail.runs[0].clone();
    assert_eq!(
        hold.reason_category,
        crate::domain::AskReason::Authentication
    );
    assert_eq!(
        hold.affected,
        [crate::domain::queue_hold::HoldJob::Review(run.id().clone()).entry()]
    );
    let required = payloads(&detail, "auth_required");
    assert_eq!(required.len(), 1, "{kinds:?}");
    assert_eq!(required[0]["job"], "review");
    let held = payloads(&detail, "run_satisfied_review_job_finished");
    assert_eq!(held.len(), 1, "{kinds:?}");
    assert_eq!(held[0]["outcome"], "held");
    assert!(held[0]["runs_again"].is_null(), "{}", held[0]);
    // Held while the ask is open: no judgment and no ask of the claim.
    assert!(payloads(&detail, "run_satisfied_judged").is_empty());
    assert!(!asked);

    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    let outcomes: Vec<_> = payloads(&detail, "run_satisfied_review_job_finished")
        .into_iter()
        .map(|p| p["outcome"].clone())
        .collect();
    assert_eq!(outcomes, [json!("held"), json!("answered")], "{kinds:?}");
    let begun = payloads(&detail, "run_satisfied_review_started");
    assert_eq!(begun.len(), 1, "{begun:?}");
    assert_eq!(begun[0]["head"], json!(head));
    let started: Vec<_> = payloads(&detail, "run_satisfied_review_job_started")
        .into_iter()
        .map(|p| (p["attempt"].clone(), p["again"].clone()))
        .collect();
    assert_eq!(
        started,
        [(json!(1), json!(false)), (json!(1), json!(false))]
    );
    let prompts = wiring.agent.review_prompts();
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    for prompt in &prompts {
        assert!(
            prompt.contains(&format!("main at {head} (the snapshot)")),
            "{prompt}"
        );
    }
    let applied: Vec<_> = crate::application::EventStore::all_events(&queue)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind.as_str() == "queue_hold_applied")
        .map(|e| e.payload)
        .collect();
    assert_eq!(applied.len(), 1, "{applied:?}");
    assert_eq!(applied[0]["jobs"], json!(hold.affected), "{applied:?}");
    let judged = payloads(&detail, "run_satisfied_judged");
    assert_eq!(judged.len(), 1, "{kinds:?}");
    assert_eq!(judged[0]["step"], "ask");
    assert_eq!(judged[0]["head"], json!(head));
    let close: Vec<_> = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .filter(|ask| ask.kind == AskKind::ApproveClose)
        .collect();
    assert_eq!(close.len(), 1, "{close:?}");
    assert_eq!(close[0].reason_category, crate::domain::AskReason::Scope);
    assert_eq!(close[0].recommendation.as_deref(), Some("close"));
    assert!(queue.run_lease(run.id()).unwrap().is_none());
}
