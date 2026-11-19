//! The free disk space wired from the supervisor's reading to its holds
//! and its landings (ADR-t4473-1): below the reserve no run is claimed
//! (`claim_held`) and no landing starts (`landing_held`); above it, with no
//! run moving, a waiting landing starts before a new claim, and a queue
//! with no run size sample yet claims one run at a time. A relaxation of
//! the needs (ADR-t4727-1) is read from its newest record on the next
//! pass, and one past its time is recorded expired once. Which needs and
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
    let held = queue_payloads(&queue, "claim_held");
    assert_eq!(
        held.iter()
            .filter(|hold| hold["reason"] == "disk_space")
            .count(),
        1
    );
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

/// The startup reader wires the fixed method through check_disk into
/// the hold event and its log breakdown even before the first sample.
#[test]
fn fixed_disk_configuration_reaches_the_supervisors_hold_without_samples() {
    let wiring = Wiring::new(
        "fixed_disk_configuration_reaches_the_supervisors_hold_without_samples",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    )
    .with_repository_startup_file(std::sync::Arc::new(|| {
        let config = crate::infrastructure::run_env::parse_config(
            "[disk]\nestimate = \"fixed\"\nrun_size = \"20GiB\"\nlanding_size = \"30GiB\"\n",
        )?;
        Ok(
            crate::application::supervise::repository_config::StartupRules {
                stall: config.stall,
                disk: config.disk,
                resume: config.resume,
                review_jobs: config.review_jobs,
            },
        )
    }));
    wiring.free.store(SHORT, Ordering::SeqCst);
    let task = wiring.ready_task(&[]);
    wiring.supervise_once_while_stoppable(|queue, stop| {
        if queue_payloads(queue, "claim_held").is_empty() {
            return false;
        }
        stop();
        true
    });
    let mut queue = wiring.queue();
    let held = queue_payloads(&queue, "claim_held");
    assert_eq!(held.len(), 1);
    assert_eq!(held[0]["disk"]["estimate"], "fixed");
    assert_eq!(held[0]["disk"]["expected"], 20 * GIB);
    assert_eq!(held[0]["disk"]["integrator_expected"], 30 * GIB);
    assert_eq!(held[0]["disk"]["workers"], 0);
    assert_eq!(held[0]["disk"]["waiting"], 0);
    assert_eq!(held[0]["disk"]["integrators"], 0);
    assert_eq!(held[0]["disk"]["sample_count"], 0);
    assert!(
        held[0]["message"]
            .as_str()
            .unwrap()
            .contains("expected 20.0 GiB (fixed)")
    );
    assert!(queue.show(task).unwrap().runs.is_empty());
}

/// Startup's fixed estimate and the off-loop measurement reach the event
/// writer on a moving pass. Further passes write nothing more for this
/// run in the same supervisor process.
#[test]
fn a_moving_run_exceeding_the_fixed_expectation_is_recorded_once() {
    let mut wiring = Wiring::new(
        "a_moving_run_exceeding_the_fixed_expectation_is_recorded_once",
        Script {
            receipts: vec![succeeded],
            verdicts: vec![verdict("pass", "meets the acceptance")],
        },
    )
    .with_tick(std::time::Duration::from_millis(1))
    .with_repository_startup_file(std::sync::Arc::new(|| {
        let config = crate::infrastructure::run_env::parse_config(
            "[disk]\nestimate = \"fixed\"\nrun_size = \"20GiB\"\n",
        )?;
        Ok(
            crate::application::supervise::repository_config::StartupRules {
                stall: config.stall,
                disk: config.disk,
                resume: config.resume,
                review_jobs: config.review_jobs,
            },
        )
    }));
    wiring.free.store(ROOM, Ordering::SeqCst);
    // Only the worktree contributes; neither TMPDIR exists in this probe.
    wiring.moving_sizes = std::sync::Arc::new(move |path: &std::path::Path| {
        if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("worktree"))
        {
            Ok(Some(21 * GIB))
        } else {
            Ok(None)
        }
    });
    let (release, released) = std::sync::mpsc::channel();
    wiring.agent.hold_next_turn(released);
    let task = wiring.ready_task(&[]);
    let mut observed_pass = None;
    let mut released = false;
    wiring.supervise_once_while_stoppable(|queue, stop| {
        let events = queue_payloads(queue, "run_size_exceeded_expected");
        if events.is_empty() {
            return false;
        }
        assert_eq!(events.len(), 1);
        if released {
            if queue.show(task).unwrap().runs[0].status() == RunStatus::Integrated {
                stop();
                return true;
            }
            return false;
        }
        assert!(running(queue, task));
        let pass = *observed_pass.get_or_insert(wiring.passes());
        if wiring.passes() < pass + 5 {
            return false;
        }
        release.send(()).unwrap();
        released = true;
        false
    });
    assert!(observed_pass.is_some());
    let mut queue = wiring.queue();
    let detail = queue.show(task).unwrap();
    let events = queue_payloads(&queue, "run_size_exceeded_expected");
    assert_eq!(
        events,
        vec![serde_json::json!({
            "run_id": detail.runs[0].id(),
            "task_id": task,
            "bytes": 21 * GIB,
            "expected": 20 * GIB,
            "estimate": "fixed",
        })]
    );
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
}

/// The disk's hold a supervisor recorded reaches `status` as its
/// `claim_hold` (the need's breakdown with it) while that supervisor runs,
/// and, once it has stood 30 minutes with a ready task to claim, the
/// inbox's attention; `candidates`' `held` names it. A hold whose supervisor
/// stopped holds nothing, and neither shows once the claims resume.
#[test]
fn a_standing_disk_hold_shows_in_status_and_after_30_minutes_in_the_inboxs_attention() {
    use std::time::{Duration, SystemTime};

    use crate::domain::{EventKind, LeaseToken, SessionRole};
    let wiring = Wiring::new(
        "a_standing_disk_hold_shows_in_status_and_after_30_minutes_in_the_inboxs_attention",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    );
    wiring.free.store(SHORT, Ordering::SeqCst);
    let task = wiring.ready_task(&[]);
    wiring.supervise_once();
    let mut queue = wiring.queue();
    let held = queue_payloads(&queue, "claim_held");
    assert_eq!(held.len(), 1, "{held:?}");
    let token = LeaseToken::new(held[0]["supervisor"].as_str().unwrap());
    // The events carry the queue's own time; a person reads the queue so
    // long after the hold started.
    let since = queue
        .all_events()
        .unwrap()
        .into_iter()
        .find(|event| event.kind == EventKind::ClaimHeld)
        .unwrap()
        .created_at;
    let since = crate::domain::stats::timestamp_millis(&since).unwrap();
    let later = |minutes: u64| {
        SystemTime::UNIX_EPOCH
            + Duration::from_millis(u64::try_from(since).unwrap())
            + Duration::from_secs(minutes * 60)
    };
    let told = |status: &Value| {
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["kind"] == "claim_held")
            .cloned()
            .collect::<Vec<_>>()
    };
    // The `--once` supervisor stopped: its hold holds nothing.
    let status = wiring.status();
    assert_eq!(status["claim_hold"], Value::Null, "{status}");
    assert!(told(&status).is_empty());

    // Still running ten minutes on: shown, not yet the inbox's.
    wiring.register_at(&token, later(10));
    let status = wiring.status_at(later(10), Some(SessionRole::Inbox));
    let hold = &status["claim_hold"];
    assert_eq!(hold["reason"], "disk_space", "{status}");
    assert_eq!(hold["value"], serde_json::json!(SHORT as f64));
    assert_eq!(hold["threshold"], serde_json::json!((5 * GIB) as f64));
    assert_eq!(hold["disk"]["reserve"], 5 * GIB);
    assert_eq!(hold["supervisor"], token.as_str());
    assert!(hold["since"].is_string());
    assert!(
        hold["message"]
            .as_str()
            .unwrap()
            .contains("reserve 5.0 GiB")
    );
    assert!(told(&status).is_empty(), "{status}");

    // Past thirty minutes (the clock reads whole seconds), the task still
    // ready: the inbox's attention, told with the need, not asked.
    wiring.register_at(&token, later(31));
    let status = wiring.status_at(later(31), Some(SessionRole::Inbox));
    let items = told(&status);
    assert_eq!(items.len(), 1, "{status}");
    assert_eq!(items[0]["next"], "tell the person to free the disk");
    assert_eq!(
        items[0]["last_error"],
        "no new run claimed for 30 min: free 4.0 GiB below the 5.0 GiB a new run needs \
         (reserve 5.0 GiB, new run W 0.0 GiB, unused reservations U 0.0 GiB (0 moving runs), \
         empty INTEGRATOR slot I 0.0 GiB); ready tasks waiting: 1"
    );
    // Told, not asked: the only ask is the disk's own, opened by its own
    // condition (nothing moves below the reserve), not by this attention.
    let asks = status["asks"].as_array().unwrap();
    assert!(
        asks.iter().all(|ask| ask["kind"] == "queue_hold"),
        "{status}"
    );
    assert!(items[0]["ask_id"].is_null());
    // A planner's status has no attention.
    let planner = wiring.status_at(later(31), Some(SessionRole::Planner));
    assert!(told(&planner).is_empty());
    let shown = wiring.claim_holds_at(later(31));
    assert_eq!(shown[0]["reason"], "claim_held", "{shown:?}");
    assert_eq!(shown[0]["record"]["reason"], "disk_space");
    assert!(queue.show(task).unwrap().runs.is_empty());

    // The claims resume: neither shows any more.
    queue
        .record_queue_event(
            EventKind::ClaimResumed,
            serde_json::json!({"reason": "disk_space", "supervisor": token}),
        )
        .unwrap();
    let status = wiring.status_at(later(31), Some(SessionRole::Inbox));
    assert_eq!(status["claim_hold"], Value::Null);
    assert!(told(&status).is_empty());
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
    )
    .with_tick(std::time::Duration::from_millis(1))
    .with_continuous_passes();
    wiring.free.store(ROOM, Ordering::SeqCst);
    let free = wiring.free.clone();
    wiring
        .agent
        .on_next_turn(Box::new(move || free.store(SHORT, Ordering::SeqCst)));
    let task = wiring.ready_task(&[]);
    let free = wiring.free.clone();
    // The queued run has handed back its lease. Keep the supervisor
    // running to observe the disk recovery after this test restores room.
    let mut awaiting = None;
    wiring.supervise_once_while_stoppable(|queue, stop| {
        let detail = queue.show(task).unwrap();
        let Some(run) = detail.runs.first() else {
            return false;
        };
        if run.status() == RunStatus::Integrated {
            stop();
            return true;
        }
        if awaiting.is_none() && !queue_payloads(queue, "landing_held").is_empty() {
            awaiting = Some((run.status(), queue.run_lease(run.id()).unwrap().is_some()));
            free.store(ROOM, Ordering::SeqCst);
        }
        false
    });

    assert_eq!(awaiting, Some((RunStatus::AwaitingIntegration, false)));
    let mut queue = wiring.queue();
    let held = queue_payloads(&queue, "landing_held");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["reason"], "disk_space");
    let candidates = held[0]["landing_candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0]["allowed"], false);
    assert_eq!(candidates[0]["needed"], 5 * GIB);
    assert_eq!(candidates[0]["waiting"], true);
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

/// A resolved approved resume waits for landing in needs_session. Its
/// slot must still enter the shared disk decision: it holds below the
/// reserve, then advances with exactly the reserve and no other active run.
#[test]
fn a_resolved_approved_resume_is_a_disk_candidate_while_still_needs_session() {
    let wiring = Wiring::new(
        "a_resolved_approved_resume_is_a_disk_candidate_while_still_needs_session",
        Script {
            receipts: vec![succeeded, succeeded],
            verdicts: vec![verdict("concern", "a person must approve")],
        },
    )
    .with_tick(std::time::Duration::from_millis(1));
    wiring
        .git
        .commit_files(&[("change.txt", "original")], &["change.txt"]);
    let task = wiring.ready_task(&[]);
    wiring.supervise_once();
    wiring.git.conflict_at_next_rebase("change.txt", "resolved");
    let parked = wiring.integrate_outcome(task);
    assert_eq!(parked["outcome"], "needs_session", "{parked}");
    let run = wiring.queue().show(task).unwrap().runs[0].id().clone();

    // The queued run owns no worker slot, so observe successive passes
    // until its landing finishes rather than relying on --once draining it.
    let wiring = wiring.with_continuous_passes();
    wiring.free.store(ROOM, Ordering::SeqCst);
    let free = wiring.free.clone();
    wiring.agent.on_next_turn(Box::new(move || {
        free.store(SHORT, Ordering::SeqCst);
    }));
    let mut held_status = None;
    wiring.supervise_once_while_stoppable(|queue, stop| {
        if held_status.is_none() && !queue_payloads(queue, "landing_held").is_empty() {
            held_status = Some(queue.show(task).unwrap().runs[0].status());
            wiring.free.store(5 * GIB, Ordering::SeqCst);
        }
        let detail = queue.show(task).unwrap();
        let resumed = detail
            .events
            .iter()
            .find(|e| e.kind.as_str() == "resume_finished");
        if resumed.is_some_and(|resumed| {
            detail
                .events
                .iter()
                .any(|e| e.id > resumed.id && e.kind.as_str() == "integration_compared")
        }) {
            stop();
            return true;
        }
        false
    });
    assert_eq!(held_status, Some(RunStatus::NeedsSession));
    let mut queue = wiring.queue();
    let held = queue_payloads(&queue, "landing_held");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["disk"]["workers"], 0);
    assert_eq!(held[0]["disk"]["waiting"], 1);
    assert_eq!(held[0]["disk"]["integrators"], 0);
    assert_eq!(
        held[0]["landing_candidates"],
        serde_json::json!([{"run": run, "needed": 5 * GIB,
                           "allowed": false, "waiting": true}])
    );
    assert_eq!(queue_payloads(&queue, "landing_resumed").len(), 1);
    let detail = queue.show(task).unwrap();
    let resumed: Vec<_> = detail
        .events
        .iter()
        .filter(|e| e.kind.as_str() == "resume_finished")
        .collect();
    assert_eq!(resumed.len(), 1, "{detail:?}");
    assert_eq!(resumed[0].payload["approved"], true);
    assert_eq!(resumed[0].payload["outcome"], "resolved");
    let started = detail
        .events
        .iter()
        .rev()
        .find(|e| e.kind.as_str() == "integration_started")
        .unwrap();
    assert!(resumed[0].id < started.id);
    // The resolved diff still needs the person's approval again. Reaching
    // that comparison proves the disk gate let its landing proceed.
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    let compared = detail
        .events
        .iter()
        .rev()
        .find(|e| e.kind.as_str() == "integration_compared")
        .unwrap();
    assert_eq!(compared.payload["matched"], false);
    assert_eq!(compared.payload["reapproval"], "integrate");
    wiring.integrate(task);
    assert_eq!(
        queue.show(task).unwrap().runs[0].status(),
        RunStatus::Integrated
    );
}

/// The expectation of the queue's one run size sample: 10 GiB.
const EXPECTED: u64 = 10 * GIB;
/// Room above the reserve (5 GiB) for two expectations and a half: with
/// nothing moving one run is claimed; with one moving a second goes only
/// once the first's size is measured at the expectation.
const ONE_AND_A_HALF: u64 = 5 * GIB + 2 * EXPECTED + EXPECTED / 2;

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
/// the second task is claimed beside it, which room for W plus the empty integration slot above the reserve holds.
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
    // The worktree alone is the expectation (no TMPDIR there): the run
    // stage peaks the first run leaves are W's samples too.
    wiring.moving_sizes = std::sync::Arc::new(|path: &std::path::Path| {
        Ok(path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("worktree"))
            .then_some(EXPECTED))
    });
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
/// claimed and `claim_held` names the reserve and three expectations
/// (new W, remaining W and the empty integration slot). Once the first run
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
    let (release, released) = std::sync::mpsc::channel();
    let (second, third) = one_sample_and_two_ready(&wiring, released);
    let ended = std::path::PathBuf::from(
        wiring.queue().all_runs().unwrap()[0]
            .worktree_path()
            .unwrap(),
    );
    let ended_walked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saw_ended = ended_walked.clone();
    let measured = calls.clone();
    wiring.moving_sizes = std::sync::Arc::new(move |path: &std::path::Path| {
        if path == ended {
            saw_ended.store(true, Ordering::SeqCst);
        }
        measured.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::from_raw_os_error(libc::EMFILE))
    });
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
    assert!(
        !ended_walked.load(Ordering::SeqCst),
        "an ended run has no disk stage to measure"
    );
    let mut queue = wiring.queue();
    // Ordering waits may follow this hold. Its measurements describe
    // only the wait for capacity while the first worker was moving.
    let held = queue_payloads(&queue, "claim_held")
        .into_iter()
        .find(|hold| hold["reason"] == "disk_space")
        .expect("a hold while the worker was moving");
    assert_eq!(
        held["threshold"],
        serde_json::json!((5 * GIB + 3 * EXPECTED) as f64)
    );
    let disk = &held["disk"];
    assert_eq!(disk["expected"], EXPECTED);
    assert_eq!(disk["sample_count"], 1);
    assert_eq!(disk["moving"], 1);
    assert_eq!(disk["remaining"], EXPECTED);
    assert_eq!(disk["measured"], 0);
    assert_eq!(disk["unmeasured"], 1);
    assert_eq!(disk["landing_waiting"], false);
    assert_eq!(disk["landing_expected"], EXPECTED);
    assert_eq!(disk["workers"], 1);
    assert_eq!(disk["waiting"], 0);
    assert_eq!(disk["integrators"], 0);
    assert_eq!(disk["integrator_expected"], EXPECTED);
    assert!(
        held["message"].as_str().unwrap().contains(
            "stages worker 1, waiting 0, integrator 0; W 10.0 GiB, I 10.0 GiB, U 10.0 GiB"
        )
    );
    assert_eq!(disk["claims_wait_reason"], Value::Null);
    let message = held["message"].as_str().unwrap();
    assert!(
        message.contains("expected 10.0 GiB (recent; samples 1)"),
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

/// A relaxation of the needs recorded `ago` before the test's clock, for
/// an hour: `expected_bytes` and `no_remaining` as given.
fn relax(wiring: &Wiring, expected: Option<u64>, no_remaining: bool, ago: u64) -> i64 {
    wiring.queue_event_ago(
        "disk_relaxed",
        &serde_json::json!({
            "expected_bytes": expected, "no_landing_expectation": false,
            "no_remaining": no_remaining, "for_secs": 3600, "reason": "emergency",
        }),
        std::time::Duration::from_secs(ago),
    )
}

/// ADR-t4727-1: the supervisor reads a relaxation on its next pass, with no
/// restart. Under one that overrides the expectation alone (9 GiB), the
/// moving run not measured counts the overridden W, so the claim
/// is held, and `claim_held` names the relaxation; a relaxation recorded
/// while it is held that counts the moving runs' remaining 0 lets the
/// second task be claimed beside the first, which the room above the
/// reserve for one expectation holds.
#[test]
fn a_relaxation_recorded_while_a_claim_is_held_lets_it_go_on_the_next_pass() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut wiring = Wiring::new(
        "a_relaxation_recorded_while_a_claim_is_held_lets_it_go_on_the_next_pass",
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
    let first_relax = relax(&wiring, Some(9 * GIB), false, 0);
    let mut relaxed = false;
    let mut beside = false;
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
        if !relaxed {
            assert!(!claimed(queue, other));
            relax(&wiring, None, true, 0);
            relaxed = true;
            return false;
        }
        if !claimed(queue, other) {
            return false;
        }
        beside = running(queue, held);
        let _ = release.send(());
        true
    });
    assert!(beside);
    let mut queue = wiring.queue();
    // The first hold is the one the second relaxation ended; a later pass,
    // with a run moving and another waiting to land, may hold again.
    let held = queue_payloads(&queue, "claim_held");
    assert!(!held.is_empty());
    assert_eq!(
        held[0]["threshold"],
        serde_json::json!((5 * GIB + 27 * GIB) as f64)
    );
    let disk = &held[0]["disk"];
    assert_eq!(disk["expected"], 9 * GIB);
    assert_eq!(disk["relaxed"]["expected_bytes"], 9 * GIB);
    assert_eq!(disk["relaxed"]["no_remaining"], false);
    assert_eq!(disk["relaxed"]["event_id"], first_relax);
    let message = held[0]["message"].as_str().unwrap();
    assert!(message.contains("relaxed until"), "{message}");
    assert!(message.contains("the reserve is kept"), "{message}");
    assert!(!queue_payloads(&queue, "claim_resumed").is_empty());
    assert!(queue_payloads(&queue, "disk_relax_expired").is_empty());
    for task in [second, third] {
        assert_eq!(
            queue.show(task).unwrap().runs[0].status(),
            RunStatus::Integrated
        );
    }
}

/// ADR-t4727-1: a relaxation past its time (recorded two hours ago for
/// one) relaxes nothing: the first pass records `disk_relax_expired` of it
/// once, however many passes follow, and the claim beside the moving run
/// not measured is held as with none (the reserve and the expectation
/// three times) until the first run landed.
#[test]
fn a_relaxation_past_its_time_is_recorded_expired_once_and_relaxes_nothing() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut wiring = Wiring::new(
        "a_relaxation_past_its_time_is_recorded_expired_once_and_relaxes_nothing",
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
    let relaxation = relax(&wiring, None, true, 2 * 3600);
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
    let expired = queue_payloads(&queue, "disk_relax_expired");
    assert_eq!(expired.len(), 1, "{expired:?}");
    assert_eq!(expired[0]["relaxed"], relaxation);
    let held = queue_payloads(&queue, "claim_held");
    assert_eq!(
        held[0]["threshold"],
        serde_json::json!((5 * GIB + 3 * EXPECTED) as f64)
    );
    assert!(held[0]["disk"].get("relaxed").is_none(), "{held:?}");
    for task in [second, third] {
        assert_eq!(
            queue.show(task).unwrap().runs[0].status(),
            RunStatus::Integrated
        );
    }
}

/// Status wires the newest queue relaxation into both its public view
/// and one inbox notice. An unrelated newer event does not mask it, a
/// replacement wins, and clear never revives an older relaxation.
#[test]
fn status_and_attention_share_the_newest_relaxation_and_clear_hides_it() {
    use crate::domain::{EventKind, SessionRole};
    let wiring = Wiring::new(
        "status_and_attention_share_the_newest_relaxation_and_clear_hides_it",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    );
    let first = relax(&wiring, Some(GIB), false, 0);
    let newest = relax(&wiring, Some(2 * GIB), true, 0);
    let queue = wiring.queue();
    queue
        .record_queue_event(EventKind::ClaimResumed, serde_json::json!({}))
        .unwrap();
    let notice = |status: &Value| {
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["kind"] == "disk_relaxed")
            .cloned()
            .collect::<Vec<_>>()
    };
    let shown = wiring.status();
    assert_ne!(first, newest);
    assert_eq!(shown["disk_relax"]["event_id"], newest);
    assert_eq!(shown["disk_relax"]["expected_bytes"], 2 * GIB);
    assert_eq!(shown["disk_relax"]["no_remaining"], true);
    assert_eq!(shown["disk_relax"]["reason"], "emergency");
    // Every role gets the active record, only the inbox gets attention.
    for role in [
        SessionRole::Inbox,
        SessionRole::Planner,
        SessionRole::Worker,
        SessionRole::Supervisor,
        SessionRole::Observer,
    ] {
        let status = wiring.status_at(
            std::time::SystemTime::UNIX_EPOCH + super::host::STOPPED_AT,
            Some(role),
        );
        assert_eq!(status["disk_relax"], shown["disk_relax"]);
        let items = notice(&status);
        assert_eq!(items.len(), usize::from(role == SessionRole::Inbox));
        if role == SessionRole::Inbox {
            assert_eq!(
                items[0]["next"],
                "report the disk relaxation and its expiry"
            );
            assert!(
                items[0]["last_error"]
                    .as_str()
                    .unwrap()
                    .contains(shown["disk_relax"]["until"].as_str().unwrap())
            );
            assert!(items[0]["ask_id"].is_null());
        }
        assert!(status["asks"].as_array().unwrap().is_empty());
    }
    // Deadline is judged by the read, with no expiry event needed.
    let expired = wiring.status_at(
        std::time::SystemTime::UNIX_EPOCH
            + super::host::STOPPED_AT
            + std::time::Duration::from_secs(3600),
        Some(SessionRole::Inbox),
    );
    assert!(expired["disk_relax"].is_null());
    assert!(notice(&expired).is_empty());
    queue
        .record_queue_event(
            EventKind::DiskRelaxCleared,
            serde_json::json!({"relaxed": newest, "reason": "space freed"}),
        )
        .unwrap();
    let cleared = wiring.status();
    assert!(cleared["disk_relax"].is_null());
    assert!(notice(&cleared).is_empty());
    assert!(cleared["asks"].as_array().unwrap().is_empty());
}
