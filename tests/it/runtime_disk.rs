//! Runtime tests: holding claims and landings while the free disk space is
//! short of what a run needs, cleaning for room, and telling the inbox once
//! (task 377).
use crate::runtime_support;
use dagq::domain::EventKind;

use dagq::domain::{Ask, AskReason, disk::DiskConfig};
use runtime_support::*;
use std::sync::atomic::AtomicBool;

const GIB: u64 = 1 << 30;

/// Needs of a gibibyte for a claim and a landing, with no build measured.
fn gibibyte_needed() -> Option<DiskConfig> {
    Some(DiskConfig {
        min_free_bytes: Some(GIB),
        ..DiskConfig::default()
    })
}

fn queue_events(db: &Path, kind: &str) -> Vec<Value> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == kind)
        .map(|event| event.payload)
        .collect()
}

/// The queue's `cost` asks about the disk, closed ones too.
fn disk_asks(db: &Path) -> Vec<Ask> {
    SqliteQueue::open(db)
        .unwrap()
        .asks(AskQuery {
            all: true,
            ..AskQuery::default()
        })
        .unwrap()
        .into_iter()
        .filter(|ask| {
            ask.kind == AskKind::QueueHold
                && ask.reason_category == AskReason::Cost
                && ask.subject.as_deref() == Some("disk")
        })
        .collect()
}

/// The free space the first test's supervisor reads.
static SHORT: AtomicBool = AtomicBool::new(true);

fn short_disk(_: &Path) -> Option<u64> {
    Some(if SHORT.load(Ordering::SeqCst) {
        GIB / 2
    } else {
        4 * GIB
    })
}

/// Short of the free space a claim needs, with nothing to clean, the ready
/// task is not claimed: `claim_held` (reason `disk_space`) is recorded
/// once and `status` / `stats` show it; the inbox gets one `cost` ask
/// about the disk. The supervisor applies its answers: `done` while the
/// disk is still short asks again after another cleanup; `wait` does not.
/// Once there is room the task is claimed and `claim_resumed` recorded.
/// The ask's text and when it is opened, joined or closed are the unit
/// tests' of `supervise::disk::disk_ask_step`, `asks_again_on` and
/// `disk_question`.
#[test]
fn no_run_is_claimed_while_the_disk_is_short_and_the_inbox_is_told_once() {
    let (_dir, repo, db) = fixture();
    SHORT.store(true, Ordering::SeqCst);
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        disk: gibibyte_needed(),
        free_space: short_disk,
        stop: stop.clone(),
        ..supervise_options(2, false)
    };
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(&db, Duration::from_secs(30), |_| disk_asks(&db).len() == 1);
    await_passes(&passes, SOME_PASSES);
    let held = queue_events(&db, "claim_held");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["reason"], "disk_space");
    assert_eq!(held[0]["value"], json!((GIB / 2) as f64));
    assert_eq!(held[0]["threshold"], json!(GIB as f64));
    assert!(
        held[0]["message"]
            .as_str()
            .unwrap()
            .contains("0.5 GiB of the queue's directory is below the 1.0 GiB a new run needs"),
        "{held:?}"
    );
    // Nothing to clean: no repair, and the one ask.
    assert!(queue_events(&db, "auto_repaired").is_empty());
    let asks = disk_asks(&db);
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = &asks[0];
    assert!(ask.is_open());
    assert_eq!(ask.options, ["done", "wait"]);
    assert!(ask.affected.is_empty());
    assert_eq!(ask.task_id, None);
    assert!(ask.question.contains("0.5 GiB free"), "{}", ask.question);
    assert!(!ask.question.contains("Affected:"));
    {
        let mut queue = SqliteQueue::open(&db).unwrap();
        let detail = queue.show(TaskId::new(1)).unwrap();
        assert!(detail.runs.is_empty());
    }
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        status["supervisors"][0]["claim_hold"]["reason"], "disk_space",
        "{status}"
    );
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["claim_holds"]["held"]["reason"], "disk_space");
    assert_eq!(stats["landing_holds"]["held"], Value::Null, "{stats}");

    // A person says the disk is freed while it is not: the answer is
    // applied, and after another cleanup a new ask is opened.
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "done")
        .unwrap();
    wait_until(&db, Duration::from_secs(30), |_| disk_asks(&db).len() == 2);
    let asks = disk_asks(&db);
    assert!(asks[0].closed_at.is_some());
    assert!(asks[1].is_open());
    // `wait`: closed, and not asked again while the disk stays short.
    SqliteQueue::open(&db)
        .unwrap()
        .answer(asks[1].id, "wait")
        .unwrap();
    wait_until(&db, Duration::from_secs(30), |_| {
        disk_asks(&db).iter().all(|ask| ask.closed_at.is_some())
    });
    await_passes(&passes, SOME_PASSES);
    assert_eq!(disk_asks(&db).len(), 2);
    assert_eq!(queue_events(&db, "claim_held").len(), 1);

    // There is room again: the task is claimed and runs to its rest.
    SHORT.store(false, Ordering::SeqCst);
    wait_until(&db, Duration::from_secs(60), |queue| {
        queue
            .show(TaskId::new(1))
            .unwrap()
            .runs
            .first()
            .is_some_and(|run| queue.run_lease(run.id()).unwrap().is_none())
    });
    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to drain").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let resumed = queue_events(&db, "claim_resumed");
    assert_eq!(resumed.len(), 1, "{resumed:?}");
    assert_eq!(resumed[0]["reason"], "disk_space");
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(
        stats["claim_holds"]["by_reason"]["disk_space"]["count"], 1,
        "{stats}"
    );
    assert_eq!(stats["claim_holds"]["held"], Value::Null);
    assert_eq!(disk_asks(&db).len(), 2);
}

/// A worker script that commits, leaves build outputs in its worktree and
/// fails: its receipt names the base commit, which validation refuses
/// (a headless turn that writes its receipt and then exits non-zero may be
/// validated before its failure is seen).
const BUILDING_AGENT: &str = "commit work; mkdir -p target/debug; \
     head -c 65536 /dev/zero > target/debug/big; receipt \"$BASE\"";

/// The queue the third test's supervisor reads, and whether its disk was freed.
static LANDING_DB: Mutex<Option<PathBuf>> = Mutex::new(None);
static FREED: AtomicBool = AtomicBool::new(false);

/// Short once a run waits to land, until [`FREED`].
fn short_at_landing(_: &Path) -> Option<u64> {
    if FREED.load(Ordering::SeqCst) {
        return Some(4 * GIB);
    }
    let db = LANDING_DB
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()?;
    let queued = SqliteQueue::open(&db)
        .ok()?
        .latest_event_of("landing_queued")
        .ok()?
        .is_some();
    Some(if queued { GIB / 2 } else { 4 * GIB })
}

/// A run that passed its review but finds the disk short of what its
/// verification needs does not start landing: it stays awaiting
/// integration, `landing_held` is recorded and shown by `status` and
/// `stats`, and it joins the disk ask. Once there is room it lands,
/// `landing_resumed` is recorded and the runtime closes the ask.
#[test]
fn a_landing_waits_for_room_before_its_verification() {
    let (_dir, repo, db) = fixture();
    FREED.store(false, Ordering::SeqCst);
    *LANDING_DB.lock().unwrap() = Some(db.clone());
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let reviewer = Arc::new(TestReviewer::new(&[verdict(
        "pass",
        &[],
        "meets the acceptance",
    )]));
    let options = SuperviseOptions {
        disk: gibibyte_needed(),
        free_space: short_at_landing,
        ..supervise_options(1, true)
    };
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || {
            let _waiting = crate::common::within(crate::common::STEP_LIMIT, "supervise to return");
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
        })
    };
    wait_until(&db, Duration::from_secs(60), |_| {
        !queue_events(&db, "landing_held").is_empty()
            && disk_asks(&db).iter().any(|ask| !ask.affected.is_empty())
    });
    await_passes(&passes, SOME_PASSES);
    let run = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert!(queue_events(&db, "integration_started").is_empty());
    let held = queue_events(&db, "landing_held");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["reason"], "disk_space");
    assert!(
        held[0]["message"]
            .as_str()
            .unwrap()
            .contains("no run lands"),
        "{held:?}"
    );
    let asks = disk_asks(&db);
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].affected, [run.id().as_str()]);
    assert!(asks[0].question.contains("Affected: run "));
    // It holds no session, as a login's ask does.
    assert!(
        SqliteQueue::open(&db)
            .unwrap()
            .hold_of(run.id())
            .unwrap()
            .is_none()
    );
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        status["supervisors"][0]["landing_hold"]["reason"], "disk_space",
        "{status}"
    );
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["landing_holds"]["held"]["reason"], "disk_space");

    FREED.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to land the run").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = SqliteQueue::open(&db).unwrap().run(run.id()).unwrap();
    assert_eq!(run.status(), RunStatus::Integrated);
    let resumed = queue_events(&db, "landing_resumed");
    assert_eq!(resumed.len(), 1, "{resumed:?}");
    assert_eq!(resumed[0]["reason"], "disk_space");
    let asks = disk_asks(&db);
    assert!(asks[0].closed_at.is_some(), "{asks:?}");
    assert_eq!(asks[0].answered_by.as_deref(), Some("runtime"));
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["landing_holds"]["count"], 1, "{stats}");
    assert_eq!(stats["landing_holds"]["held"], Value::Null);
    assert!(
        runtime::status(&db).unwrap()["supervisors"]
            .as_array()
            .unwrap()
            .iter()
            .all(|supervisor| supervisor.get("landing_hold").is_none())
    );
}

fn half_a_gibibyte(_: &Path) -> Option<u64> {
    Some(GIB / 2)
}

fn two_gibibytes(_: &Path) -> Option<u64> {
    Some(2 * GIB)
}

/// A person's `integrate` with `disk` and the free space `free_space` reads.
fn integrate_on(
    db: &Path,
    repo: &Path,
    disk: Option<DiskConfig>,
    free_space: fn(&Path) -> Option<u64>,
) -> anyhow::Result<Value> {
    runtime::OneShot {
        disk,
        free_space,
        ..runtime::OneShot::system()
    }
    .integrate(db, IntegrateTarget::Task(TaskId::new(1)), repo, None)
}

/// The scratchpad whose presence makes the scratchpad test's disk short.
static SCRATCHPAD: Mutex<Option<PathBuf>> = Mutex::new(None);

fn short_while_scratchpad(_: &Path) -> Option<u64> {
    let left = SCRATCHPAD.lock().unwrap_or_else(PoisonError::into_inner);
    Some(if left.as_ref().is_some_and(|dir| dir.exists()) {
        1
    } else {
        4 * GIB
    })
}

/// Short of the room a claim needs (twice the largest build of the recent
/// runs), the supervisor cleans what an ended run left: task 1100, the
/// Claude Code scratchpad of a run whose task was canceled goes with its
/// worktree and its run `TMPDIR`, and `auto_repaired` (`disk_cleanup`)
/// counts their bytes. With room after it, the next task is claimed
/// without a hold or an ask. That a run's size adds the largest
/// scratchpad and run `TMPDIR` to the largest build is the unit tests' of
/// `domain::disk::run_size` and `DiskConfig::needs`, read from the queue
/// as in `a_persons_integrate_counts_the_scratchpads_in_a_runs_size`.
#[test]
fn a_cleanup_for_room_removes_the_scratchpads_and_claims_without_holding() {
    const SCRATCH: usize = 1 << 20;
    let (dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, BUILDING_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let first = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        queue
            .transition(TaskId::new(1), TaskAction::Cancel)
            .unwrap();
        add_ready_task(&mut queue, "second task", &[]);
        queue.show(TaskId::new(1)).unwrap().runs[0].clone()
    };
    let built = queue_events(&db, "build_outputs_removed");
    assert_eq!(built.len(), 1, "{built:?}");
    let built = built[0]["bytes"].as_u64().unwrap();
    let root = dir.path().join("claude-tmp");
    let scratchpad = scratchpad_of(&root, first.worktree_path().unwrap());
    fs::create_dir_all(scratchpad.join("session/scratchpad")).unwrap();
    fs::write(
        scratchpad.join("session/scratchpad/big"),
        vec![0u8; SCRATCH],
    )
    .unwrap();
    *SCRATCHPAD.lock().unwrap() = Some(scratchpad.clone());
    // Task 1290: the temporary files directory of its Codex turns, in its
    // run directory, goes with them and counts.
    let run_dir = PathBuf::from(first.run_dir().unwrap());
    let tmp = run_dir.join("tmp");
    fs::create_dir_all(tmp.join("t")).unwrap();
    fs::write(tmp.join("t/big"), vec![0u8; SCRATCH / 4]).unwrap();
    let options = SuperviseOptions {
        free_space: short_while_scratchpad,
        scratchpad_roots: Some(vec![root.clone()]),
        ..supervise_options(1, true)
    };
    supervise_with(&db, &repo, &backend, &options).unwrap();
    backend.join();
    assert!(!scratchpad.exists());
    let removed = queue_events(&db, "scratchpad_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    let scratched = removed[0]["bytes"].as_u64().unwrap();
    assert!(scratched >= SCRATCH as u64, "{scratched}");
    assert!(!tmp.exists());
    assert!(run_dir.join("receipt.json").is_file());
    let tmps = queue_events(&db, "run_tmp_removed");
    assert_eq!(tmps.len(), 1, "{tmps:?}");
    let tmp_bytes = tmps[0]["bytes"].as_u64().unwrap();
    assert!(tmp_bytes >= (SCRATCH / 4) as u64, "{tmp_bytes}");
    let worktree = queue_events(&db, "worktree_removed");
    assert_eq!(worktree.len(), 1, "{worktree:?}");
    let repaired = queue_events(&db, "auto_repaired");
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert_eq!(repaired[0]["repair"], "disk_cleanup");
    assert_eq!(
        repaired[0]["bytes"].as_u64().unwrap(),
        worktree[0]["bytes"].as_u64().unwrap() + scratched + tmp_bytes
    );
    assert_eq!(repaired[0]["detail"]["runs"], json!([first.id().as_str()]));
    assert_eq!(repaired[0]["layer"], "runtime");
    assert_eq!(repaired[0]["conditions"]["free_bytes"], 1);
    let needed = repaired[0]["conditions"]["needed_bytes"].as_u64().unwrap();
    assert_eq!(needed, 2 * built);
    assert!(queue_events(&db, "claim_held").is_empty());
    assert!(disk_asks(&db).is_empty());
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(queue.show(TaskId::new(2)).unwrap().runs.len(), 1);
    // `stats` counts the repair on no task.
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(
        stats["auto_repairs"]["by_layer"]["runtime"]["by_repair"]["disk_cleanup"], 1,
        "{stats}"
    );
}

/// A person's `integrate` (task 638) reads the free space and the recent
/// runs' sizes from the queue before it approves the run: short of the
/// landing threshold it fails and leaves the run as it was, with no
/// `integration_approved`; with room it lands. Task 1100: the largest
/// `scratchpad_removed` (and, task 1290, `run_tmp_removed`) adds to the
/// largest `build_outputs_removed`. The threshold, the message and the
/// cases with no threshold or no reading are the unit test
/// `integrate::tests::a_persons_integrate_refuses_below_the_landing_threshold_only`.
#[test]
fn a_persons_integrate_counts_the_scratchpads_in_a_runs_size() {
    let (_dir, repo, db, run) = awaiting_run();
    let queue = SqliteQueue::open(&db).unwrap();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::BuildOutputsRemoved,
            json!({"bytes": GIB / 4}),
        )
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::ScratchpadRemoved,
            json!({"bytes": GIB / 4}),
        )
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::RunTmpRemoved,
            json!({"bytes": GIB / 8}),
        )
        .unwrap();
    // 0.625 GiB times 1.5 is above the half a gibibyte free; the build
    // alone (0.375 GiB) would not be.
    let error = format!(
        "{:#}",
        integrate_on(&db, &repo, Some(DiskConfig::default()), half_a_gibibyte).unwrap_err()
    );
    assert!(error.contains("below the 0.9 GiB"), "{error}");
    assert!(error.contains("0.6 GiB, times"), "{error}");
    assert!(
        error.contains("the largest build outputs plus the largest Claude Code scratchpad and the largest run TMPDIR"),
        "{error}"
    );
    assert!(events_of(&db, run.id(), "integration_approved").is_empty());
    let now = SqliteQueue::open(&db).unwrap().run(run.id()).unwrap();
    assert_eq!(now.status(), RunStatus::AwaitingIntegration);
    let outcome = integrate_on(&db, &repo, Some(DiskConfig::default()), two_gibibytes).unwrap();
    assert_eq!(outcome["outcome"], "integrated", "{outcome}");
    assert_eq!(events_of(&db, run.id(), "integration_approved").len(), 1);
}
