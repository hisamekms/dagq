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
    assert_eq!(held[0]["disk"]["reserve"], 5 * GIB);
    assert_eq!(held[0]["disk"]["volume"], 0);
    assert_eq!(held[0]["disk"]["reserve_share"], 0.05);
    assert_eq!(held[0]["disk"]["expected"], Value::Null);
    assert_eq!(held[0]["disk"]["sample_count"], 0);
    assert_eq!(held[0]["disk"]["moving"], 0);
    assert_eq!(held[0]["disk"]["remaining"], 0);
    assert!(
        held[0]["message"]
            .as_str()
            .unwrap()
            .contains("reserve 5.0 GiB")
    );
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
    assert_eq!(held[0]["disk"]["reserve"], 5 * GIB);
    assert_eq!(held[0]["disk"]["moving"], 0);
    assert_eq!(held[0]["disk"]["claim_expected"], 0);
    assert!(held[0]["message"].as_str().unwrap().contains("moving 0"));
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

/// The expectation of the queue's one run size sample: 10 GiB.
const EXPECTED: u64 = 10 * GIB;
/// Room above the reserve (5 GiB) for one expectation and a half: with
/// nothing moving one run is claimed; with one moving a second goes only
/// once the first's size is measured at the expectation.
const ONE_AND_A_HALF: u64 = 5 * GIB + EXPECTED + EXPECTED / 2;

/// A first task lands with no disk read, and its run gets a run size
/// sample of [`EXPECTED`]; two more tasks are ready, the turn of the first
/// claimed held until `act` says so. Returns the two tasks.
fn one_sample_and_two_ready(
    wiring: &Wiring,
    released: std::sync::mpsc::Receiver<()>,
) -> (crate::domain::TaskId, crate::domain::TaskId) {
    let first = wiring.ready_task(&[]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let run = queue.show(first).unwrap().runs[0].id().clone();
    queue
        .record_runtime_event(
            &run,
            crate::domain::EventKind::BuildOutputsRemoved,
            serde_json::json!({"bytes": EXPECTED}),
        )
        .unwrap();
    wiring.free.store(ONE_AND_A_HALF, Ordering::SeqCst);
    wiring.agent.hold_next_turn(released);
    (wiring.ready_task(&[]), wiring.ready_task(&[]))
}

/// Whether `task`'s first run is running.
fn running(queue: &mut SqliteQueue, task: crate::domain::TaskId) -> bool {
    queue
        .show(task)
        .unwrap()
        .runs
        .first()
        .is_some_and(|run| run.status() == RunStatus::Running)
}

/// Whether `task` has a run.
fn claimed(queue: &mut SqliteQueue, task: crate::domain::TaskId) -> bool {
    !queue.show(task).unwrap().runs.is_empty()
}

/// ADR-t4542-1: the size of the run moving, measured off the loop at the
/// expectation, leaves it nothing to take yet: while its turn is held,
/// the second task is claimed beside it, which room for one expectation
/// above the reserve holds.
#[test]
fn a_moving_runs_measured_size_lets_the_next_claim_go_beside_it() {
    let mut wiring = Wiring::new(
        "a_moving_runs_measured_size_lets_the_next_claim_go_beside_it",
        Script {
            receipts: vec![succeeded, succeeded, succeeded],
            verdicts: vec![
                verdict("pass", "meets the acceptance"),
                verdict("pass", "meets the acceptance"),
                verdict("pass", "meets the acceptance"),
            ],
        },
    );
    wiring.moving_sizes = std::sync::Arc::new(|_: &std::path::Path| Ok(Some(EXPECTED)));
    let (release, released) = std::sync::mpsc::channel();
    let (second, third) = one_sample_and_two_ready(&wiring, released);
    let mut beside = false;
    wiring.supervise_once_while(|queue| {
        let (held, other) = if running(queue, second) {
            (second, third)
        } else if running(queue, third) {
            (third, second)
        } else {
            return false;
        };
        if !claimed(queue, other) {
            return false;
        }
        beside = running(queue, held);
        let _ = release.send(());
        true
    });
    assert!(beside);
    let mut queue = wiring.queue();
    for task in [second, third] {
        assert_eq!(
            queue.show(task).unwrap().runs[0].status(),
            RunStatus::Integrated
        );
    }
}

/// ADR-t4542-1: a moving run whose size cannot be measured counts the
/// expectation whole: while its turn is held, the second task is not
/// claimed and `claim_held` names the reserve and the expectation twice
/// (the new run's and the moving run's remaining). Once the first run
/// landed, the second is claimed and lands.
#[test]
fn a_moving_run_not_measured_counts_the_expectation_whole_and_holds_the_next_claim() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut wiring = Wiring::new(
        "a_moving_run_not_measured_counts_the_expectation_whole_and_holds_the_next_claim",
        Script {
            receipts: vec![succeeded, succeeded, succeeded],
            verdicts: vec![
                verdict("pass", "meets the acceptance"),
                verdict("pass", "meets the acceptance"),
                verdict("pass", "meets the acceptance"),
            ],
        },
    );
    let measured = calls.clone();
    wiring.moving_sizes = std::sync::Arc::new(move |_: &std::path::Path| {
        measured.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::from_raw_os_error(libc::EMFILE))
    });
    let (release, released) = std::sync::mpsc::channel();
    let (second, third) = one_sample_and_two_ready(&wiring, released);
    let mut after = None;
    let mut alone = false;
    wiring.supervise_once_while(|queue| {
        let (held, other) = if running(queue, second) {
            (second, third)
        } else if running(queue, third) {
            (third, second)
        } else {
            return false;
        };
        if calls.load(Ordering::SeqCst) == 0 || queue_payloads(queue, "claim_held").is_empty() {
            return false;
        }
        // Some passes after the measure failed, the claim still waits.
        let passes = *after.get_or_insert(wiring.passes());
        if wiring.passes() < passes + 5 {
            return false;
        }
        alone = running(queue, held) && !claimed(queue, other);
        let _ = release.send(());
        true
    });
    assert!(alone);
    let mut queue = wiring.queue();
    let held = queue_payloads(&queue, "claim_held");
    assert!(
        held.iter().all(|hold| hold["reason"] == "disk_space"),
        "{held:?}"
    );
    assert_eq!(
        held[0]["threshold"],
        serde_json::json!((5 * GIB + 2 * EXPECTED) as f64)
    );
    let disk = &held[0]["disk"];
    assert_eq!(disk["expected"], EXPECTED);
    assert_eq!(disk["sample_count"], 1);
    assert_eq!(disk["moving"], 1);
    assert_eq!(disk["remaining"], EXPECTED);
    assert_eq!(disk["measured"], 0);
    assert_eq!(disk["unmeasured"], 1);
    assert_eq!(disk["landing_waiting"], false);
    assert_eq!(disk["landing_expected"], 0);
    assert_eq!(disk["claims_wait_reason"], Value::Null);
    let message = held[0]["message"].as_str().unwrap();
    assert!(
        message.contains("expected 10.0 GiB (samples 1)"),
        "{message}"
    );
    assert!(
        message.contains("remaining 10.0 GiB (measured 0, unmeasured 1)"),
        "{message}"
    );
    for task in [second, third] {
        assert_eq!(
            queue.show(task).unwrap().runs[0].status(),
            RunStatus::Integrated
        );
    }
}
