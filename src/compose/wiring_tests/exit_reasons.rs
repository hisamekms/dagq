//! Exit causes reach the recorded event through each supervisor writer.
//! The decisions themselves are tested in domain/application; these use
//! the production supervisor with the wiring harness's memdb and ports.

use std::{sync::mpsc, time::Duration};

use serde_json::{Value, json};

use super::{
    agent::{Script, no_receipt, succeeded, verdict},
    harness::{Wiring, connect},
};
use crate::{
    application::TaskReads,
    domain::{AskKind, EventKind, TaskDetail, TaskId},
};

/// Make the live fake wrapper's heartbeat old during `turn`, and drain
/// once the supervisor records its exit request. The harness bounds the
/// wait and joins the supervisor and wrapper before returning.
fn lose_heartbeat(wiring: &Wiring, task: TaskId, turn: usize, release: mpsc::Sender<()>) -> Value {
    let mut release = Some(release);
    let mut recorded = None;
    let outcome = wiring.supervise_once_while_stoppable(|queue, stop| {
        let detail = queue.show(task).unwrap();
        let Some(start) = detail
            .events
            .iter()
            .filter(|event| event.kind == EventKind::TurnStarted)
            .nth(turn - 1)
        else {
            return false;
        };
        if let Some(exit) = detail
            .events
            .iter()
            .find(|event| event.id > start.id && event.kind == EventKind::ExitRequested)
        {
            recorded = Some(exit.payload.clone());
            // The wrapper may already have stopped the held agent after
            // reading /exit; then its receiver has gone away.
            let _ = release.take().unwrap().send(());
            stop();
            return true;
        }
        // The fake process still lives. Only the recorded heartbeat is
        // late, so this reaches the single /exit rather than dead-process
        // recovery. No elapsed wall time is needed to cause the silence.
        connect(&wiring.db)
            .execute(
                "UPDATE run_processes SET heartbeat_at = 1 WHERE run_id = ?1 AND role = 'wrapper' AND exited_at IS NULL",
                [detail.runs[0].id()],
            )
            .unwrap();
        false
    });
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    recorded.expect("the silent wrapper was asked to exit")
}

fn exits(detail: &TaskDetail) -> Vec<&crate::domain::RunEvent> {
    detail
        .events
        .iter()
        .filter(|event| event.kind == EventKind::ExitRequested)
        .collect()
}

#[test]
fn a_workers_lost_heartbeat_records_its_exit_reason() {
    let wiring = Wiring::new(
        "a_workers_lost_heartbeat_records_its_exit_reason",
        Script {
            receipts: vec![no_receipt],
            verdicts: vec![],
        },
    )
    .with_tick(Duration::from_millis(1));
    let task = wiring.ready_task(&["true"]);
    let (release, held) = mpsc::channel();
    wiring.agent.hold_next_turn(held);
    let exit = lose_heartbeat(&wiring, task, 1, release);
    assert_eq!(exit["reason"], "heartbeat_expired");
    assert!(exit["workspace_id"].is_string(), "{exit}");
    assert!(exit["timeout_secs"].is_u64(), "{exit}");
    assert_eq!(exit.as_object().unwrap().len(), 3, "{exit}");
}

#[test]
fn a_resumes_lost_heartbeat_records_its_exit_reason() {
    let wiring = Wiring::new(
        "a_resumes_lost_heartbeat_records_its_exit_reason",
        Script {
            receipts: vec![succeeded, no_receipt],
            verdicts: vec![verdict("concern", "a person decides")],
        },
    )
    .with_tick(Duration::from_millis(1));
    let task = wiring.ready_task(&["true"]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let ask = queue
        .asks(Default::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.kind == AskKind::ApproveLanding && ask.closed_at.is_none())
        .expect("the review asked a person");
    queue.answer(ask.id, "send_back: fix it").unwrap();
    let (release, held) = mpsc::channel();
    wiring.agent.hold_next_turn(held);
    let exit = lose_heartbeat(&wiring, task, 2, release);
    assert_eq!(exit["reason"], "heartbeat_expired");
    assert!(exit["workspace_id"].is_string(), "{exit}");
    assert_eq!(exit["resume_attempt"], 1, "{exit}");
    assert_eq!(exit.as_object().unwrap().len(), 3, "{exit}");
}

#[test]
fn a_revises_lost_heartbeat_reaches_the_exit_watch_record() {
    let wiring = Wiring::new(
        "a_revises_lost_heartbeat_reaches_the_exit_watch_record",
        Script {
            receipts: vec![succeeded, no_receipt],
            verdicts: vec![verdict("revise", "fix the loop")],
        },
    )
    .with_tick(Duration::from_millis(1));
    let task = wiring.ready_task(&["true"]);
    let (release, held) = mpsc::channel();
    let agent = wiring.agent.clone();
    wiring
        .agent
        .on_review(Box::new(move || agent.hold_next_turn(held)));
    let exit = lose_heartbeat(&wiring, task, 2, release);
    assert_eq!(exit["reason"], "heartbeat_expired");
    assert!(exit["workspace_id"].is_string(), "{exit}");
    assert_eq!(exit.as_object().unwrap().len(), 2, "{exit}");
}

#[test]
fn a_finished_turn_records_turn_ended_when_the_supervisor_exits_its_session() {
    let wiring = Wiring::new(
        "a_finished_turn_records_turn_ended_when_the_supervisor_exits_its_session",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![verdict("pass", "fine")],
        },
    )
    .with_tick(Duration::from_millis(1));
    let task = wiring.ready_task(&["true"]);
    let outcome = wiring.supervise_once_while_stoppable(|queue, stop| {
        if exits(&queue.show(task).unwrap()).is_empty() {
            return false;
        }
        stop();
        true
    });
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = wiring.queue().show(task).unwrap();
    let recorded = exits(&detail);
    let [exit] = recorded.as_slice() else {
        panic!("expected one recorded exit request: {recorded:?}");
    };
    let finished = detail
        .events
        .iter()
        .find(|event| event.kind == EventKind::TurnFinished)
        .expect("the worker's turn finished");
    assert!(finished.id < exit.id);
    assert_eq!(finished.payload["outcome"], "succeeded");
    assert_eq!(exit.payload["reason"], "turn_ended");
    assert!(exit.payload["workspace_id"].is_string());
    assert_eq!(exit.payload.as_object().unwrap().len(), 2);
}
