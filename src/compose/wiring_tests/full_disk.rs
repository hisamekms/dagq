//! A worker's session that died on a full disk wired from its end to its
//! landing (goal 260): the supervisor neither starts the run's recovery job
//! nor opens a `decide` ask; it leaves the run resting while the free space
//! holds the claims, and the pass of a later supervisor that finds room
//! resumes the same run, which lands.

use std::sync::atomic::Ordering;

use serde_json::Value;

use super::{
    agent::{Script, died_on_a_full_disk, succeeded, verdict},
    harness::Wiring,
};
use crate::{
    application::{AskQuery, TaskReads},
    domain::{AskKind, RunStatus, TaskDetail, TaskStatus},
};

/// The payloads of `detail`'s events of `kind`, oldest first.
fn payloads(detail: &TaskDetail, kind: &str) -> Vec<Value> {
    detail
        .events
        .iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| event.payload.clone())
        .collect()
}

/// The kinds of `detail`'s events, oldest first.
fn kinds(detail: &TaskDetail) -> Vec<&str> {
    detail.events.iter().map(|e| e.kind.as_str()).collect()
}

/// The disk fills while the worker's turn runs and its session dies on
/// it: across the passes of a supervisor that finds no room the run rests
/// `failed` with no recovery job and no ask; the first pass of the next
/// supervisor, with room again, resumes it (`auto_repaired` with `repair:
/// full_disk_resume` and `triage_finished` by the runtime, both naming the
/// full disk), and the same run lands.
#[test]
fn a_session_dead_on_a_full_disk_resumes_once_there_is_room_and_lands() {
    let wiring = Wiring::new(
        "a_session_dead_on_a_full_disk_resumes_once_there_is_room_and_lands",
        Script {
            receipts: vec![died_on_a_full_disk, succeeded],
            verdicts: vec![verdict("pass", "meets the acceptance")],
        },
    );
    let free = wiring.free.clone();
    wiring
        .agent
        .on_next_turn(Box::new(move || free.store(0, Ordering::SeqCst)));
    let base = wiring.git.main();
    let task = wiring.ready_task(&[]);
    wiring.supervise_once();

    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    assert_eq!(detail.runs.len(), 1, "{:?}", kinds(&detail));
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Failed, "{:?}", kinds(&detail));
    for kind in [
        "recovery_requested",
        "triage_started",
        "triage_finished",
        "auto_repaired",
        "resume_started",
    ] {
        assert!(
            payloads(&detail, kind).is_empty(),
            "{kind}: {:?}",
            kinds(&detail)
        );
    }
    let decide = |queue: &mut crate::infrastructure::sqlite::SqliteQueue| {
        queue
            .asks(AskQuery {
                all: true,
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .filter(|ask| ask.kind == AskKind::Decide)
            .count()
    };
    assert_eq!(decide(&mut queue), 0);
    assert!(wiring.agent.recovery_prompts().is_empty());

    wiring.free.store(u64::MAX, Ordering::SeqCst);
    wiring.supervise_once();

    let detail = queue.show(task).unwrap();
    let all = kinds(&detail);
    assert_eq!(detail.runs.len(), 1, "{all:?}");
    assert_eq!(detail.runs[0].id(), run.id());
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated, "{all:?}");
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_ne!(wiring.git.main(), base);
    let repaired = payloads(&detail, "auto_repaired");
    let repaired: Vec<&Value> = repaired
        .iter()
        .filter(|p| p["repair"] == "full_disk_resume")
        .collect();
    assert_eq!(repaired.len(), 1, "{all:?}");
    assert_eq!(repaired[0]["layer"], "runtime");
    assert_eq!(repaired[0]["alert"], "failed");
    assert_eq!(repaired[0]["conditions"]["cause"], "disk_full");
    let evidence = repaired[0]["conditions"]["evidence"].as_str().unwrap();
    assert!(evidence.contains("No space left on device"), "{evidence}");
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished.len(), 1, "{all:?}");
    assert_eq!(finished[0]["by"], "runtime");
    assert_eq!(finished[0]["action"], "resume");
    assert_eq!(finished[0]["cause"], "disk_full");
    assert_eq!(finished[0]["evidence"], evidence);
    for kind in ["recovery_requested", "triage_started"] {
        assert!(payloads(&detail, kind).is_empty(), "{kind}: {all:?}");
    }
    let at = |kind: &str| all.iter().position(|k| *k == kind).unwrap();
    assert!(at("triage_finished") < at("resume_started"), "{all:?}");
    assert_eq!(decide(&mut queue), 0);
    assert!(wiring.agent.recovery_prompts().is_empty());
    let prompts = wiring.agent.turn_prompts();
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    assert!(
        prompts[1].contains("ended on a full disk"),
        "{}",
        prompts[1]
    );
}
