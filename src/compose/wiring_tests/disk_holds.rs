//! The free disk space wired from the supervisor's reading to its holds
//! and its landings (ADR-t4473-1): below the reserve no run is claimed
//! (`claim_held`) and no landing starts (`landing_held`); above it, with no
//! run moving, a waiting landing starts before a new claim, and a queue
//! with no run size sample yet claims one run at a time. Which needs and
//! how many claims each reading gives are the unit tests of
//! `domain::disk`.

use std::sync::atomic::Ordering;

use serde_json::Value;

use super::{
    agent::{Script, succeeded, verdict},
    harness::Wiring,
};
use crate::{
    application::TaskReads,
    domain::{RunStatus, TaskDetail},
    infrastructure::sqlite::SqliteQueue,
};

const GIB: u64 = 1 << 30;
/// Below the reserve: 5 GiB, its least, as the supervisors read no
/// volume's size.
const SHORT: u64 = 4 * GIB;
/// Far above the reserve, read: the disk's rule applies.
const ROOM: u64 = 100 * GIB;

/// The payloads of the queue's events of `kind`, oldest first.
fn queue_payloads(queue: &SqliteQueue, kind: &str) -> Vec<Value> {
    queue
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind.as_str() == kind)
        .map(|event| event.payload)
        .collect()
}

/// The id of `detail`'s first event of `kind`.
fn first_id(detail: &TaskDetail, kind: &str) -> i64 {
    detail
        .events
        .iter()
        .find(|event| event.kind.as_str() == kind)
        .map(|event| event.id.as_i64())
        .unwrap_or_else(|| panic!("no {kind}"))
}

/// Below the reserve the two ready tasks are not claimed: `claim_held`
/// (`disk_space`, the reserve its threshold). With room again (the next
/// supervisor), and no run size sample yet, the first is claimed and the second only once the first
/// no longer moves: after it landed, its landing having gone before the
/// second's claim. Both land.
#[test]
fn below_the_reserve_no_run_is_claimed_and_with_room_a_new_queue_claims_one_at_a_time() {
    let wiring = Wiring::new(
        "below_the_reserve_no_run_is_claimed_and_with_room_a_new_queue_claims_one_at_a_time",
        Script {
            receipts: vec![succeeded, succeeded],
            verdicts: vec![
                verdict("pass", "meets the acceptance"),
                verdict("pass", "meets the acceptance"),
            ],
        },
    );
    wiring.free.store(SHORT, Ordering::SeqCst);
    let first = wiring.ready_task(&[]);
    let second = wiring.ready_task(&[]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let held = queue_payloads(&queue, "claim_held");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["reason"], "disk_space");
    assert_eq!(held[0]["value"], serde_json::json!(SHORT as f64));
    assert_eq!(held[0]["threshold"], serde_json::json!((5 * GIB) as f64));
    assert!(queue.show(first).unwrap().runs.is_empty());

    wiring.free.store(ROOM, Ordering::SeqCst);
    wiring.supervise_once();
    assert_eq!(queue_payloads(&queue, "claim_held").len(), 1);
    let first = queue.show(first).unwrap();
    let second = queue.show(second).unwrap();
    for detail in [&first, &second] {
        assert_eq!(detail.runs.len(), 1);
        assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    }
    // One at a time: the second is claimed only after the first landed.
    let (earlier, later) = if first_id(&first, "run_claimed") < first_id(&second, "run_claimed") {
        (&first, &second)
    } else {
        (&second, &first)
    };
    assert!(
        first_id(earlier, "run_integrated") < first_id(later, "run_claimed"),
        "{:?}",
        later
            .events
            .iter()
            .map(|e| e.kind.as_str())
            .collect::<Vec<_>>()
    );
}

/// A run that waits to land while the free space is below the reserve does
/// not start its landing: `landing_held` (`disk_space`), the run awaiting
/// integration. Once there is room it lands (`landing_resumed`).
#[test]
fn below_the_reserve_a_landing_waits_and_with_room_it_lands() {
    let wiring = Wiring::new(
        "below_the_reserve_a_landing_waits_and_with_room_it_lands",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![verdict("pass", "meets the acceptance")],
        },
    );
    wiring.free.store(ROOM, Ordering::SeqCst);
    let free = wiring.free.clone();
    wiring
        .agent
        .on_next_turn(Box::new(move || free.store(SHORT, Ordering::SeqCst)));
    let task = wiring.ready_task(&[]);
    let free = wiring.free.clone();
    let mut awaiting = None;
    wiring.supervise_once_while(|queue| {
        if queue_payloads(queue, "landing_held").is_empty() {
            return false;
        }
        awaiting = queue
            .show(task)
            .unwrap()
            .runs
            .first()
            .map(|run| run.status());
        free.store(ROOM, Ordering::SeqCst);
        true
    });

    assert_eq!(awaiting, Some(RunStatus::AwaitingIntegration));
    let mut queue = wiring.queue();
    let held = queue_payloads(&queue, "landing_held");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["reason"], "disk_space");
    assert_eq!(held[0]["threshold"], serde_json::json!((5 * GIB) as f64));
    assert_eq!(queue_payloads(&queue, "landing_resumed").len(), 1);
    let detail = queue.show(task).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    let held_at = queue
        .all_events()
        .unwrap()
        .into_iter()
        .find(|event| event.kind.as_str() == "landing_held")
        .unwrap()
        .id
        .as_i64();
    assert!(held_at < first_id(&detail, "integration_started"));
}
