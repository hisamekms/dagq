//! Runtime tests: the hold of a task that ends (ADR-t1879-1). The
//! transaction that completes or cancels a task closes its open hold and
//! records `task_hold_ended` with the end (`completed` or `canceled`), and
//! discards every result the hold kept (`hold_result_discarded`,
//! `task_ended`), a kept answer's ask closed. A hold left open on a task
//! that ended before is closed the same way by a supervisor's pass, the
//! task unchanged; a result kept for a task that ended after its hold's
//! release is discarded as `task_ended` when the pass settles it, its ask
//! closed with that reason; and a task moved on again after its end is not
//! held.
use crate::common::cli::invoke_with;
use crate::runtime_support;
use dagq::application::Landing;
use dagq::domain::{
    AskReason, EventKind, HEARTBEAT_TIMEOUT_SECS, LeaseToken, RunEvent, task_hold::DiscardReason,
};

use runtime_support::*;

/// `dagq` as a person at a plain terminal; its JSON, or a panic.
fn as_user(db: &Path, args: &[&str]) -> Value {
    let output = invoke_with(&[], db, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn hold(db: &Path, target: &str) -> Value {
    as_user(db, &["hold", target, "--reason", "the login expired"])
}

/// The events of `task`, in order.
fn task_events(db: &Path, task: TaskId) -> Vec<RunEvent> {
    SqliteQueue::open(db).unwrap().show(task).unwrap().events
}

/// Check that `task`'s hold was closed by its end `reason` in the
/// transaction that ended it: no hold open, the row released, and its
/// `task_hold_ended` written right after the `task_status_changed` that
/// ended it (nothing of another transaction in between).
fn assert_ended(db: &Path, task: TaskId, reason: &str) -> Value {
    let queue = SqliteQueue::open(db).unwrap();
    assert!(queue.task_hold(task).unwrap().is_none());
    assert!(queue.hold_guard(task).unwrap().is_none());
    let events = task_events(db, task);
    let ended = events
        .iter()
        .position(|e| e.kind == EventKind::TaskHoldEnded)
        .expect("no task_hold_ended");
    let changed = events
        .iter()
        .rposition(|e| e.kind == EventKind::TaskStatusChanged && e.payload["to"] == reason)
        .expect("no task_status_changed");
    assert!(changed < ended, "{events:?}");
    assert!(
        events[changed + 1..ended]
            .iter()
            .all(|e| e.kind != EventKind::TaskStatusChanged),
        "{events:?}"
    );
    let payload = events[ended].payload.clone();
    assert_eq!(payload["reason"], reason);
    assert!(payload["hold_id"].as_i64().is_some(), "{payload}");
    let released: Option<i64> = Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT released_at FROM task_holds WHERE id=?1",
            [payload["hold_id"].as_i64()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(released.is_some());
    payload
}

/// An interrupted run of task 1, made through the store with no session,
/// on a queue whose clock is ahead of the claim's real time.
fn interrupted(repo: &Path, db: &Path) -> (SqliteQueue, TaskRun) {
    let run = provision_under(repo, db, "first");
    let clock = ManualClock::at(unix_second_now() + 10 * HEARTBEAT_TIMEOUT_SECS);
    let mut queue = SqliteQueue::open(db).unwrap().with_generators(Generators {
        clock: Arc::new(clock),
        ids: Arc::new(clock::UuidGenerator),
    });
    let run = queue.recover_run(run.id(), 0, json!({})).unwrap();
    assert_eq!(run.status(), RunStatus::Interrupted);
    (queue, run)
}

/// The recovery job's `decide` ask on `run`, answered `answer`.
fn answered_decide(queue: &mut SqliteQueue, run: &TaskRun, answer: &str) -> AskId {
    let ask = queue
        .ask(NewAsk {
            kind: AskKind::Decide,
            task_id: None,
            run_id: Some(run.id().clone()),
            question: "Retry it?".into(),
            options: vec!["retry".into(), "resume".into(), "cancel".into()],
            asked_by: dagq::application::TRIAGE_ASKER.into(),
            reason_category: AskReason::RecoveryFailed,
            topics: Vec::new(),
            recommendation: None,
            confidence: None,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask
        .id;
    queue.answer(ask, answer).unwrap();
    ask
}

/// Whether ask `id` is closed.
fn ask_closed(queue: &SqliteQueue, id: AskId) -> bool {
    queue
        .asks(AskQuery {
            all: true,
            ..Default::default()
        })
        .unwrap()
        .iter()
        .any(|a| a.id == id && a.closed_at.is_some())
}

/// Check that ask `id` of `run` was closed for its kept answer's discard
/// with the task's end: closed, and its `ask_closed` records reason
/// `task_ended` and that the answer was not applied.
fn assert_ask_discarded(queue: &SqliteQueue, run: &TaskRun, id: AskId) {
    assert!(ask_closed(queue, id));
    let closes: Vec<_> = queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::AskClosed && e.payload["ask_id"] == json!(id))
        .collect();
    assert_eq!(closes.len(), 1, "{closes:?}");
    let payload = &closes[0].payload;
    assert_eq!(payload["reason"], DiscardReason::TaskEnded.as_str());
    assert_eq!(payload["discarded"], true);
    assert!(
        payload["note"]
            .as_str()
            .is_some_and(|note| note.contains("canceled")),
        "{payload}"
    );
}

/// A held ready task canceled by a person: the cancel closes the hold
/// (`canceled`). Moved on again after its end, the task takes no hold
/// from before: nothing guards it and the claim's line has it.
#[test]
fn a_held_task_canceled_closes_its_hold_and_is_not_held_when_it_moves_on_again() {
    let (_dir, _repo, db) = fixture();
    hold(&db, "1");
    as_user(&db, &["cancel", "1", "--reason", "not_worth"]);
    assert_ended(&db, TaskId::new(1), "canceled");
    // A ready task again (as a store write moves it), not held.
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE tasks SET status='ready' WHERE id=1", [])
        .unwrap();
    let queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.task_hold(TaskId::new(1)).unwrap().is_none());
    assert!(queue.held_claims().unwrap().is_empty());
    assert!(queue.task_holds().unwrap().is_empty());
}

/// A held task canceled as a duplicate of another closes its hold too.
#[test]
fn a_held_task_canceled_as_a_duplicate_closes_its_hold() {
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let second = add_ready_task(&mut queue, "second", &[]);
    hold(&db, &second.to_string());
    as_user(&db, &["cancel", &second.to_string(), "--duplicate-of", "1"]);
    assert_ended(&db, second, "canceled");
    // The other task's hold is its own: none was there.
    assert!(queue.task_hold(TaskId::new(1)).unwrap().is_none());
}

/// A task held while its run lands (the landing goes on, the hold
/// settling) completes with it: the completion closes the hold
/// (`completed`), and the run's events end the hold's interval.
#[test]
fn a_task_held_while_its_run_lands_closes_its_hold_as_it_completes() {
    let (_dir, repo, db, run) = awaiting_run_owned();
    let main = git_out(&repo, &["rev-parse", "main"]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .begin_integration(run.id(), &LeaseToken::new("integrator"), &sha(&main))
        .unwrap();
    assert_eq!(hold(&db, "1")["state"], "settling");
    let commit = sha(&main);
    let (task, _) = queue
        .finish_integration(
            run.id(),
            &LeaseToken::new("integrator"),
            &Landing {
                commit: commit.clone(),
                source_commit: commit.clone(),
                main_before: commit,
                history_ref: String::new(),
                message: String::new(),
                verification_skipped: false,
            },
            "",
        )
        .unwrap();
    assert_eq!(task.status(), TaskStatus::Completed);
    let ended = assert_ended(&db, TaskId::new(1), "completed");
    assert!(ended["settled_secs"].is_null(), "{ended}");
    // The run's record of it ends the hold's interval.
    let events = queue.run_events(run.id()).unwrap();
    assert!(events.iter().any(|e| e.kind == EventKind::TaskHoldEnded));
    assert!(queue.settling_holds().unwrap().is_empty());
}

/// The results a hold kept for a task that ends are discarded with the
/// end (`task_ended`), a kept answer's ask closed: none is left for the
/// supervisor to settle.
#[test]
fn the_results_a_hold_kept_are_discarded_when_the_task_ends() {
    let (_dir, repo, db) = fixture();
    let (mut queue, run) = interrupted(&repo, &db);
    let ask = answered_decide(&mut queue, &run, "retry");
    hold(&db, "1");
    assert!(
        queue
            .keep_triage_answer(run.id(), ask, "retry", "a person chose retry")
            .unwrap()
    );
    assert!(
        queue
            .keep_live_verdict(run.id(), json!({"verdict": "repair"}))
            .unwrap()
    );
    assert!(!ask_closed(&queue, ask));
    as_user(&db, &["cancel", "1", "--reason", "not_worth"]);
    assert_ended(&db, TaskId::new(1), "canceled");
    let events = queue.run_events(run.id()).unwrap();
    let kept: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::HoldResultKept)
        .map(|e| json!(e.id))
        .collect();
    let discarded: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::HoldResultDiscarded)
        .collect();
    assert_eq!(kept.len(), 2);
    assert_eq!(
        discarded
            .iter()
            .map(|e| e.payload["kept_id"].clone())
            .collect::<Vec<_>>(),
        kept
    );
    for discard in &discarded {
        assert_eq!(discard.payload["reason"], DiscardReason::TaskEnded.as_str());
        assert_eq!(discard.payload["superseded"], true);
    }
    assert_ask_discarded(&queue, &run, ask);
    assert!(queue.released_kept_results().unwrap().is_empty());
}

/// A queue whose task ended before its end closed holds: the supervisor's
/// pass closes the hold left open and discards what it kept, as the end
/// would have, and leaves the task as it is; the next pass finds none.
#[test]
fn a_hold_left_open_on_a_task_that_ended_is_closed_without_changing_the_task() {
    let (_dir, repo, db) = fixture();
    let (mut queue, run) = interrupted(&repo, &db);
    let ask = answered_decide(&mut queue, &run, "retry");
    hold(&db, "1");
    assert!(
        queue
            .keep_triage_answer(run.id(), ask, "retry", "a person chose retry")
            .unwrap()
    );
    // The task ended with its hold open, as a cancel before the end closed
    // holds left it.
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE tasks SET status='canceled', updated_at='2026-01-01T00:00:00Z' WHERE id=1",
            [],
        )
        .unwrap();
    assert!(queue.task_hold(TaskId::new(1)).unwrap().is_some());
    let before = queue.show(TaskId::new(1)).unwrap().task;
    assert_eq!(
        queue.end_holds_of_ended_tasks().unwrap(),
        vec![TaskId::new(1)]
    );
    assert!(queue.task_hold(TaskId::new(1)).unwrap().is_none());
    let after = queue.show(TaskId::new(1)).unwrap().task;
    assert_eq!(after.status(), TaskStatus::Canceled);
    assert_eq!(after.updated_at(), before.updated_at());
    let ended = task_events(&db, TaskId::new(1))
        .into_iter()
        .rfind(|e| e.kind == EventKind::TaskHoldEnded)
        .unwrap();
    assert_eq!(ended.payload["reason"], "canceled");
    let discarded = queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .rfind(|e| e.kind == EventKind::HoldResultDiscarded)
        .unwrap();
    assert_eq!(discarded.payload["reason"], "task_ended");
    assert_ask_discarded(&queue, &run, ask);
    assert!(queue.released_kept_results().unwrap().is_empty());
    assert!(queue.end_holds_of_ended_tasks().unwrap().is_empty());
}

/// A kept `cancel` answer applied at the release ends the task: the
/// cancel's end finds that result settled as applied and does not discard
/// it too, and the hold it closes is already released, so no
/// `task_hold_ended` is written.
#[test]
fn a_kept_cancel_answer_applied_at_the_release_is_applied_once() {
    let (_dir, repo, db) = fixture();
    let (mut queue, run) = interrupted(&repo, &db);
    let ask = answered_decide(&mut queue, &run, "cancel");
    hold(&db, "1");
    assert!(
        queue
            .keep_triage_answer(run.id(), ask, "cancel", "a person chose cancel")
            .unwrap()
    );
    as_user(&db, &["release", "1"]);
    let released = queue.released_kept_results().unwrap();
    assert_eq!(released.len(), 1);
    assert!(matches!(
        queue.settle_kept_result(released[0]).unwrap(),
        dagq::application::KeptOutcome::Applied(_)
    ));
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Canceled
    );
    let events = queue.run_events(run.id()).unwrap();
    let settled: Vec<_> = events
        .iter()
        .filter(|e| {
            e.kind == EventKind::HoldResultApplied || e.kind == EventKind::HoldResultDiscarded
        })
        .collect();
    assert_eq!(settled.len(), 1, "{settled:?}");
    assert_eq!(settled[0].kind, EventKind::HoldResultApplied);
    assert!(
        task_events(&db, TaskId::new(1))
            .iter()
            .all(|e| e.kind != EventKind::TaskHoldEnded)
    );
    assert!(ask_closed(&queue, ask));
}

/// A kept `cancel` answer that the answers' path applies after the
/// release, before a pass settled it, is settled as applied in the same
/// transaction: the cancel's end does not discard it nor close its ask as
/// discarded, and no pass settles it again.
#[test]
fn a_kept_cancel_answer_the_answers_apply_after_the_release_is_settled_as_applied() {
    let (_dir, repo, db) = fixture();
    let (mut queue, run) = interrupted(&repo, &db);
    let ask = answered_decide(&mut queue, &run, "cancel");
    hold(&db, "1");
    assert!(
        queue
            .keep_triage_answer(run.id(), ask, "cancel", "a person chose cancel")
            .unwrap()
    );
    as_user(&db, &["release", "1"]);
    let kept = queue.released_kept_results().unwrap();
    assert_eq!(kept.len(), 1);
    queue
        .decide_triage(run.id(), ask, "cancel", "a person chose cancel")
        .unwrap();
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Canceled
    );
    let events = queue.run_events(run.id()).unwrap();
    let settled: Vec<_> = events
        .iter()
        .filter(|e| {
            e.kind == EventKind::HoldResultApplied || e.kind == EventKind::HoldResultDiscarded
        })
        .collect();
    assert_eq!(settled.len(), 1, "{settled:?}");
    assert_eq!(settled[0].kind, EventKind::HoldResultApplied);
    assert_eq!(settled[0].payload["kept_id"], json!(kept[0]));
    assert!(ask_closed(&queue, ask));
    assert!(
        events
            .iter()
            .all(|e| e.payload["discarded"] != true && e.kind != EventKind::AskClosed),
        "{events:?}"
    );
    assert!(queue.released_kept_results().unwrap().is_empty());
    assert!(matches!(
        queue.settle_kept_result(kept[0]).unwrap(),
        dagq::application::KeptOutcome::Untouched
    ));
}

/// A person's `cancel` answer to the recovery job's ask of a held task's
/// run is refused while the hold is open: the task stays in progress and
/// held (the answer is kept and applied at the release, above).
#[test]
fn a_recovery_jobs_cancel_answer_is_refused_while_the_task_is_held() {
    let (_dir, repo, db) = fixture();
    let (mut queue, run) = interrupted(&repo, &db);
    let ask = answered_decide(&mut queue, &run, "cancel");
    hold(&db, "1");
    let refused = queue
        .decide_triage(run.id(), ask, "cancel", "a person chose cancel")
        .unwrap_err();
    assert!(refused.to_string().contains("is held"), "{refused}");
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::InProgress
    );
    assert!(queue.task_hold(TaskId::new(1)).unwrap().is_some());
}

/// A person's `cancel` answer to the landing's ask of a held task's run
/// fails the run and cancels the task as the landing answers apply it,
/// and the cancel closes the hold (`canceled`).
#[test]
fn a_landings_cancel_answer_closes_the_hold_of_the_task() {
    use dagq::application::TaskStore;
    let (_dir, _repo, db, run) = awaiting_run_owned();
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(hold(&db, "1")["state"], "held");
    let ask = queue
        .ask(NewAsk {
            kind: AskKind::ApproveLanding,
            task_id: None,
            run_id: Some(run.id().clone()),
            question: "Land it?".into(),
            options: vec!["land".into(), "send_back".into(), "cancel".into()],
            asked_by: "runtime".into(),
            reason_category: AskReason::Scope,
            topics: Vec::new(),
            recommendation: None,
            confidence: None,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask
        .id;
    queue.answer(ask, "cancel").unwrap();
    queue
        .decide_landing(
            run.id(),
            RunStatus::Failed,
            "canceled by the answer",
            json!({"code": "cancelled", "ask_id": ask, "answer": "cancel"}),
        )
        .unwrap();
    TaskStore::cancel(
        &mut queue,
        TaskId::new(1),
        dagq::domain::cancel_reason::NewCancel::answered(AskKind::ApproveLanding, ask),
    )
    .unwrap();
    assert_ended(&db, TaskId::new(1), "canceled");
}

/// A queue from before a task's end closed holds: a kept answer, its hold
/// released, and the task canceled before the supervisor settled it. No
/// hold is left open for the pass to close, so the settle discards the
/// answer as `task_ended` and closes its ask with that reason.
#[test]
fn a_kept_answer_of_a_task_that_ended_after_its_release_is_discarded_with_its_ask() {
    let (_dir, repo, db) = fixture();
    let (mut queue, run) = interrupted(&repo, &db);
    let ask = answered_decide(&mut queue, &run, "retry");
    hold(&db, "1");
    assert!(
        queue
            .keep_triage_answer(run.id(), ask, "retry", "a person chose retry")
            .unwrap()
    );
    as_user(&db, &["release", "1"]);
    // Canceled with the kept answer pending, as a cancel before the end
    // discarded what a hold kept left it.
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE tasks SET status='canceled' WHERE id=1", [])
        .unwrap();
    assert!(queue.task_hold(TaskId::new(1)).unwrap().is_none());
    assert!(queue.end_holds_of_ended_tasks().unwrap().is_empty());
    let released = queue.released_kept_results().unwrap();
    assert_eq!(released.len(), 1);
    assert!(!ask_closed(&queue, ask));
    assert!(matches!(
        queue.settle_kept_result(released[0]).unwrap(),
        dagq::application::KeptOutcome::Discarded(DiscardReason::TaskEnded)
    ));
    let discarded = queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .rfind(|e| e.kind == EventKind::HoldResultDiscarded)
        .unwrap();
    assert_eq!(discarded.payload["kept_id"], json!(released[0]));
    assert_eq!(discarded.payload["reason"], "task_ended");
    assert_ask_discarded(&queue, &run, ask);
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Canceled
    );
    assert!(queue.released_kept_results().unwrap().is_empty());
}
