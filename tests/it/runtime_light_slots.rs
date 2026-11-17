//! Runtime tests: landing-turn waits leave worker capacity for any change.
//! Historical light-room deferrals remain readable.
use crate::runtime_support;
use dagq::application::DraftPlannerStore;
use dagq::domain::EventKind;

use dagq::domain::{ExecutionClass, LeaseToken, Priority, change::TaskChange};
use runtime_support::*;
use std::sync::atomic::{AtomicBool, AtomicU64};

/// Each worker writes a file of its own, so the runs never conflict.
const OWN_FILE_AGENT: &str = "printf 'by %s\\n' \"$RUN_ID\" > \"file-$RUN_ID.txt\" && git add -A && git commit -q -m work; receipt \"$(git rev-parse HEAD)\"";

fn add_task(db: &Path, title: &str, change: &str, paths: &[&str], priority: Priority) -> TaskId {
    let task = add_draft(
        db,
        title,
        change,
        paths,
        priority,
        ExecutionClass::Implementation,
    );
    ready(db, task);
    task
}

fn ready(db: &Path, task: TaskId) {
    SqliteQueue::open(db)
        .unwrap()
        .transition(task, TaskAction::BypassReview)
        .unwrap();
}

/// A draft task of `class`: its ID comes now, its wait starts when it is
/// made ready.
fn add_draft(
    db: &Path,
    title: &str,
    change: &str,
    paths: &[&str],
    priority: Priority,
    execution_class: ExecutionClass,
) -> TaskId {
    let mut queue = SqliteQueue::open(db).unwrap();
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: Vec::new(),
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            priority: Some(priority),
            change: Some(change.parse::<TaskChange>().unwrap()),
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            wait_for_build: false,
            execution_class,
        })
        .unwrap();
    task.id()
}

/// The tasks of the queue's `run_claimed` events, oldest first, with the
/// events' payloads.
fn claims(db: &Path) -> Vec<(TaskId, Value)> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == EventKind::RunClaimed)
        .map(|event| (event.task_id.unwrap(), event.payload))
        .collect()
}

fn landing_queued(db: &Path, task: TaskId) -> bool {
    SqliteQueue::open(db)
        .unwrap()
        .show(task)
        .unwrap()
        .events
        .iter()
        .any(|event| event.kind == EventKind::LandingQueued)
}

/// A landing-turn wait leaves room for the heavy interrupt first. Once
/// that run queues too, the excess over parallel supplies backpressure.
#[test]
fn the_landing_queue_leaves_room_for_the_heavy_interrupt() {
    let (_dir, repo, db, blocker) = awaiting_run();
    // Another landing holds the single integration slot.
    let mut queue = SqliteQueue::open(&db).unwrap();
    let main = git_out(&repo, &["rev-parse", "main"]);
    queue
        .begin_integration(blocker.id(), &LeaseToken::new("by-hand"), &sha(&main))
        .unwrap();
    fs::write(
        repo.join("dagq.toml"),
        "[tasks]\nchanges = [\"feature\", \"docs\"]\n[supervisor]\nlight_changes = [\"docs\"]\n",
    )
    .unwrap();
    let first = add_task(&db, "first", "feature", &["file-*"], Priority::Normal);

    let backend = Arc::new(TestWorkspace::new(&db, false, OWN_FILE_AGENT));
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        ..supervise_options(1, false)
    };
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
        })
    };
    wait_until(&db, Duration::from_secs(120), |_| {
        landing_queued(&db, first)
    });
    await_passes(&passes, SOME_PASSES);

    let heavy = add_task(&db, "heavy", "feature", &["file-*"], Priority::Interrupt);
    let unscoped = add_task(&db, "unscoped", "docs", &[], Priority::High);
    let light = add_task(&db, "light", "docs", &["file-*"], Priority::Normal);
    // The fixture's run of task 1 was the first claim.
    wait_until(&db, Duration::from_secs(60), |_| claims(&db).len() == 3);
    let claimed = claims(&db);
    assert_eq!(claimed[1].0, first, "{claimed:?}");
    assert!(claimed[1].1.get("light_room").is_none(), "{claimed:?}");
    assert_eq!(claimed[2].0, heavy, "{claimed:?}");
    assert!(claimed[2].1.get("light_room").is_none(), "{claimed:?}");
    // The queued run leaves worker capacity.
    assert_eq!(claimed[2].1["slots"], 0, "{claimed:?}");
    // The table read at the start is no change.
    assert!(
        !SqliteQueue::open(&db)
            .unwrap()
            .all_events()
            .unwrap()
            .iter()
            .any(|event| event.kind == EventKind::SupervisorConfigChanged)
    );

    let status = runtime::status(&db).unwrap();
    let supervisor_status = status["supervisors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["registered"] == true && entry["alive"] == true)
        .unwrap()
        .clone();
    assert_eq!(
        supervisor_status["slots"]["landing_queue"], 1,
        "{supervisor_status}"
    );
    assert_eq!(supervisor_status["slots"]["used"], 1, "{supervisor_status}");
    let first_run = SqliteQueue::open(&db).unwrap().show(first).unwrap().runs[0]
        .id()
        .to_string();
    let listed = status["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|run| run["run_id"] == first_run.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("{status}"));
    assert_eq!(listed["progress"]["slot"], "landing_queue", "{listed}");

    // The queued excess holds further claims.
    await_passes(&passes, SOME_PASSES);
    let mut queue = SqliteQueue::open(&db).unwrap();
    for task in [light, unscoped] {
        assert_eq!(
            queue.show(task).unwrap().task.status(),
            TaskStatus::Ready,
            "{task}"
        );
    }
    assert_eq!(claims(&db).len(), 3);

    // Drain: the other landing gives its slot back, and the two runs land.
    stop.store(true, Ordering::SeqCst);
    queue
        .abort_integration(
            blocker.id(),
            &LeaseToken::new("by-hand"),
            "awaiting_integration",
            "given back",
            &dagq::domain::Reason::new(ReasonCode::Other),
        )
        .unwrap();
    let outcome = joined(supervisor, "the supervisor to drain").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(claims(&db).len(), 3);
}

/// The kinds of the task's events, oldest first, with their ids.
fn task_events(db: &Path, task: TaskId) -> Vec<(i64, String)> {
    SqliteQueue::open(db)
        .unwrap()
        .show(task)
        .unwrap()
        .events
        .iter()
        .map(|event| (event.id.as_i64(), event.kind.to_string()))
        .collect()
}

/// A parked run resumes in the worker capacity the landing queue leaves,
/// ahead of a new claim at the same priority.
#[test]
fn a_parked_run_waits_for_a_normal_slot_while_the_landing_queue_fills_it() {
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    let setup = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &setup).unwrap();
    setup.join();
    let runs: Vec<TaskRun> = [1, 2]
        .into_iter()
        .map(|id| queue.show(TaskId::new(id)).unwrap().runs[0].clone())
        .collect();
    for run in &runs {
        assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    }
    let (blocker, parked) = (&runs[0], &runs[1]);
    // Another landing holds the single integration slot.
    let main = git_out(&repo, &["rev-parse", "main"]);
    queue
        .begin_integration(blocker.id(), &LeaseToken::new("by-hand"), &sha(&main))
        .unwrap();
    fs::write(
        repo.join("dagq.toml"),
        "[tasks]\nchanges = [\"feature\", \"docs\"]\n[supervisor]\nlight_changes = [\"docs\"]\n",
    )
    .unwrap();
    let first = add_task(&db, "first", "feature", &["file-*"], Priority::Normal);

    let backend = Arc::new(TestWorkspace::new(&db, false, OWN_FILE_AGENT));
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        ..supervise_options(1, false)
    };
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
        })
    };
    wait_until(&db, Duration::from_secs(120), |_| {
        landing_queued(&db, first)
    });
    await_passes(&passes, SOME_PASSES);

    queue
        .set_priority(TaskId::new(2), Some(Priority::Interrupt))
        .unwrap();
    // The landing queue leaves worker capacity for the parked run.
    queue
        .park_rechecked(parked.id(), None, "parked for the test", json!({}))
        .unwrap()
        .unwrap();
    let heavy = add_task(&db, "heavy", "feature", &["file-*"], Priority::Interrupt);
    wait_until(&db, Duration::from_secs(120), |_| {
        task_events(&db, TaskId::new(2))
            .iter()
            .any(|(_, kind)| kind == "resume_started")
    });
    // Once the resumed turn releases worker capacity, the next claim may
    // start even while the first run still waits in the landing lane.
    wait_until(&db, Duration::from_secs(120), |_| claims(&db).len() == 4);
    stop.store(true, Ordering::SeqCst);
    queue
        .abort_integration(
            blocker.id(),
            &LeaseToken::new("by-hand"),
            "awaiting_integration",
            "given back",
            &dagq::domain::Reason::new(ReasonCode::Other),
        )
        .unwrap();
    let resumed = task_events(&db, TaskId::new(2))
        .into_iter()
        .find(|(_, kind)| kind == "resume_started")
        .unwrap()
        .0;
    // No claim was made from the freed slot before the resume.
    let heavy_claims: Vec<i64> = task_events(&db, heavy)
        .into_iter()
        .filter(|(_, kind)| kind == "run_claimed")
        .map(|(id, _)| id)
        .collect();
    assert_eq!(heavy_claims.len(), 1, "{heavy_claims:?}");
    assert!(
        heavy_claims.iter().all(|id| *id > resumed),
        "{heavy_claims:?}"
    );
    assert_eq!(claims(&db).len(), 4);
    let outcome = joined(supervisor, "the supervisor to drain").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
}

/// The load average [`changing`] reads, as `f64` bits.
static LOAD: AtomicU64 = AtomicU64::new(0);

fn changing() -> Option<f64> {
    Some(f64::from_bits(LOAD.load(Ordering::SeqCst)))
}

fn set_load(value: f64) {
    LOAD.store(value.to_bits(), Ordering::SeqCst);
}

/// [`OWN_FILE_AGENT`] that writes a Spike's result too.
fn own_file_spike_agent() -> String {
    format!(
        "{RECEIPT_SPIKE}printf 'by %s\\n' \"$RUN_ID\" > \"file-$RUN_ID.txt\" && git add -A && git commit -q -m work; receipt_spike \"$(git rev-parse HEAD)\" holds"
    )
}

/// Aging compares all candidate changes in the worker capacity the landing
/// queue leaves; candidates shows no new light-room deferral.
#[test]
fn in_the_light_room_the_aging_compares_the_light_tasks_as_candidates_show() {
    let (_dir, repo, db, blocker) = awaiting_run();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let main = git_out(&repo, &["rev-parse", "main"]);
    queue
        .begin_integration(blocker.id(), &LeaseToken::new("by-hand"), &sha(&main))
        .unwrap();
    fs::write(
        repo.join("dagq.toml"),
        "[tasks]\nchanges = [\"feature\", \"docs\"]\n[supervisor]\nlight_changes = [\"docs\"]\nclass_aging = 1\n",
    )
    .unwrap();
    let first = add_task(&db, "first", "feature", &["file-*"], Priority::Normal);
    set_load(1.0);

    let backend = Arc::new(TestWorkspace::new(&db, false, &own_file_spike_agent()));
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        max_load: Some(16.0),
        load_average: changing,
        claim_spacing: Some(0),
        ..supervise_options(1, false)
    };
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
        })
    };
    wait_until(&db, Duration::from_secs(120), |_| {
        landing_queued(&db, first)
    });
    // The claims wait while the tasks line up.
    set_load(40.0);
    await_passes(&passes, SOME_PASSES);
    let heavy = add_task(&db, "heavy", "feature", &["file-*"], Priority::Normal);
    let fresh = add_draft(
        &db,
        "light implementation",
        "docs",
        &["file-*"],
        Priority::Normal,
        ExecutionClass::Implementation,
    );
    let spikes: Vec<TaskId> = (0..2)
        .map(|index| {
            let spike = add_draft(
                &db,
                &format!("light spike {index}"),
                "docs",
                &["file-*"],
                Priority::Normal,
                ExecutionClass::Spike,
            );
            ready(&db, spike);
            spike
        })
        .collect();
    thread::sleep(Duration::from_millis(1100));
    ready(&db, fresh);
    set_load(1.0);
    // The oldest implementation is now a candidate too: it goes first.
    wait_until(&db, Duration::from_secs(60), |_| claims(&db).len() == 3);
    let claimed = claims(&db);
    assert_eq!(claimed[2].0, heavy, "{claimed:?}");
    assert!(claimed[2].1.get("light_room").is_none(), "{claimed:?}");
    assert_eq!(claimed[2].1["aged"], false, "{claimed:?}");

    let view = crate::common::cli::ok(&db, &["candidates"]);
    let ids: Vec<i64> = view["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| task["id"].as_i64().unwrap())
        .collect();
    assert_eq!(
        ids,
        [spikes[0].as_i64(), spikes[1].as_i64(), fresh.as_i64()],
        "{view}"
    );
    assert_eq!(
        view["aged"],
        json!([spikes[0].as_i64(), spikes[1].as_i64()]),
        "{view}"
    );
    let deferred: Vec<(i64, String)> = view["deferred"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["task_id"].as_i64().unwrap(),
                d["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(deferred.is_empty(), "{view}");

    stop.store(true, Ordering::SeqCst);
    queue
        .abort_integration(
            blocker.id(),
            &LeaseToken::new("by-hand"),
            "awaiting_integration",
            "given back",
            &dagq::domain::Reason::new(ReasonCode::Other),
        )
        .unwrap();
    let outcome = joined(supervisor, "the supervisor to drain").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
}

/// A load under any `--max-load`: the claim spacing is in effect, nothing
/// else holds the claims.
fn low_load() -> Option<f64> {
    Some(1.0)
}

/// The payloads of the queue's events of `kind` on `task`, oldest first.
fn events_of(db: &Path, task: TaskId, kind: EventKind) -> Vec<Value> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == kind && event.task_id == Some(task))
        .map(|event| event.payload)
        .collect()
}

/// The queue's `supervisor_config_changed` events.
fn config_changes(db: &Path) -> usize {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .iter()
        .filter(|event| event.kind == EventKind::SupervisorConfigChanged)
        .count()
}

/// `[supervisor]` with `parallel` and `claim_spacing`, other than in use,
/// written and then read by the supervisor: the passes after the call see
/// it.
fn set_supervisor(repo: &Path, db: &Path, passes: &AtomicU64, parallel: usize, spacing: usize) {
    let changes = config_changes(db);
    fs::write(
        repo.join("dagq.toml"),
        format!(
            "[tasks]\nchanges = [\"feature\", \"docs\"]\n[supervisor]\nlight_changes = [\"docs\"]\nparallel = {parallel}\nclaim_spacing = {spacing}\n"
        ),
    )
    .unwrap();
    wait_until(db, Duration::from_secs(60), |_| {
        config_changes(db) > changes
    });
    await_passes(passes, 2);
}

/// `candidates`' order and its deferred tasks with their reasons.
fn shown(db: &Path) -> (Vec<i64>, Vec<(i64, String)>) {
    let view = crate::common::cli::ok(db, &["candidates"]);
    let ids = view["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| task["id"].as_i64().unwrap())
        .collect();
    let deferred = view["deferred"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["task_id"].as_i64().unwrap(),
                d["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    (ids, deferred)
}

/// Claim spacing still gates heavy tasks beside the landing queue. Changing
/// parallel creates no light-room deferrals, and removing spacing claims
/// the heavy task in normal order.
#[test]
fn a_light_room_deferral_stays_one_record_while_a_free_slot_comes_and_goes() {
    let (_dir, repo, db, blocker) = awaiting_run();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let main = git_out(&repo, &["rev-parse", "main"]);
    queue
        .begin_integration(blocker.id(), &LeaseToken::new("by-hand"), &sha(&main))
        .unwrap();
    fs::write(
        repo.join("dagq.toml"),
        "[tasks]\nchanges = [\"feature\", \"docs\"]\n[supervisor]\nlight_changes = [\"docs\"]\nparallel = 1\nclaim_spacing = 0\n",
    )
    .unwrap();
    let first = add_task(&db, "first", "feature", &["file-*"], Priority::Normal);

    let backend = Arc::new(TestWorkspace::new(&db, false, OWN_FILE_AGENT));
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        parallel: None,
        claim_spacing: None,
        max_load: Some(16.0),
        load_average: low_load,
        ..supervise_options(1, false)
    };
    let passes = options.passes.clone();
    let dagq = crate::common::service::owned_executable(db.parent().unwrap());
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &reviewer,
                &dagq,
                &options,
            )
        })
    };
    wait_until(&db, Duration::from_secs(120), |_| {
        landing_queued(&db, first)
    });
    await_passes(&passes, SOME_PASSES);
    // From here the spacing holds every claim.
    set_supervisor(&repo, &db, &passes, 1, 3600);
    // Heavy for its change: its paths are those its worker writes.
    let heavy = add_task(&db, "heavy", "feature", &["file-*"], Priority::Normal);
    await_passes(&passes, SOME_PASSES);
    assert!(events_of(&db, heavy, EventKind::ClaimDeferred).is_empty());
    let in_worker_room = (vec![heavy.as_i64()], Vec::new());
    assert_eq!(shown(&db), in_worker_room);
    assert_eq!(claims(&db).len(), 2);
    set_supervisor(&repo, &db, &passes, 2, 3600);
    assert_eq!(shown(&db), in_worker_room);
    set_supervisor(&repo, &db, &passes, 1, 3600);
    assert_eq!(shown(&db), in_worker_room);
    // A worker slot with no spacing claims the heavy task.
    set_supervisor(&repo, &db, &passes, 2, 0);
    wait_until(&db, Duration::from_secs(60), |_| claims(&db).len() == 3);
    // The claim takes it first, as `candidates` showed over the free slot.
    let claimed = claims(&db);
    assert_eq!(claimed[2].0, heavy, "{claimed:?}");
    assert!(claimed[2].1.get("light_room").is_none(), "{claimed:?}");
    assert!(events_of(&db, heavy, EventKind::ClaimDeferred).is_empty());
    assert!(events_of(&db, heavy, EventKind::ClaimDeferralEnded).is_empty());

    stop.store(true, Ordering::SeqCst);
    queue
        .abort_integration(
            blocker.id(),
            &LeaseToken::new("by-hand"),
            "awaiting_integration",
            "given back",
            &dagq::domain::Reason::new(ReasonCode::Other),
        )
        .unwrap();
    let outcome = joined(supervisor, "the supervisor to drain").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
}

/// A supervisor that takes over a light room's record from one that is
/// gone, its heavy slots left recorded full: over this supervisor's free
/// slot the record stays one, and `candidates` shows the task where this
/// supervisor's claim takes it, not as the gone one left it.
#[test]
fn a_supervisor_that_takes_over_a_light_room_deferral_shows_it_by_its_own_free_slot() {
    let (_dir, repo, db, blocker) = awaiting_run();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let main = git_out(&repo, &["rev-parse", "main"]);
    queue
        .begin_integration(blocker.id(), &LeaseToken::new("by-hand"), &sha(&main))
        .unwrap();
    // Two free slots, and the claim spacing after the fixture's claim
    // holds every claim.
    fs::write(
        repo.join("dagq.toml"),
        "[tasks]\nchanges = [\"feature\", \"docs\"]\n[supervisor]\nlight_changes = [\"docs\"]\nparallel = 2\nclaim_spacing = 3600\n",
    )
    .unwrap();
    let heavy = add_task(&db, "heavy", "feature", &["file-*"], Priority::Normal);
    // What a supervisor that is gone left: its heavy slots full, and the
    // heavy task deferred for its light room.
    let gone = LeaseToken::new("gone");
    queue
        .record_queue_event(
            EventKind::SlotsFullStarted,
            json!({"supervisor": "gone", "class": "heavy", "running": 1, "slots": 1}),
        )
        .unwrap();
    let (kind, payload) = dagq::domain::claim_order::light_room_deferred(&gone);
    queue.record_task_event(heavy, kind, payload).unwrap();
    let deferred = (Vec::new(), vec![(heavy.as_i64(), "light_room".to_owned())]);
    // With no supervisor alive, the record holds as recorded.
    assert_eq!(shown(&db), deferred);

    let backend = Arc::new(TestWorkspace::new(&db, false, OWN_FILE_AGENT));
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        parallel: None,
        claim_spacing: None,
        max_load: Some(16.0),
        load_average: low_load,
        ..supervise_options(1, false)
    };
    let passes = options.passes.clone();
    let dagq = crate::common::service::owned_executable(db.parent().unwrap());
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &reviewer,
                &dagq,
                &options,
            )
        })
    };
    wait_until(&db, Duration::from_secs(60), |queue| {
        queue.supervisors().unwrap().len() == 1
    });
    await_passes(&passes, SOME_PASSES);
    assert_eq!(shown(&db), (vec![heavy.as_i64()], Vec::new()));
    assert_eq!(events_of(&db, heavy, EventKind::ClaimDeferred).len(), 1);
    assert!(events_of(&db, heavy, EventKind::ClaimDeferralEnded).is_empty());
    {
        let stats = crate::common::ledger::stats(&db, &Default::default()).unwrap();
        assert_eq!(stats["claim_deferrals"]["count"], 1, "{stats}");
    }
    assert_eq!(claims(&db).len(), 1);

    stop.store(true, Ordering::SeqCst);
    queue
        .abort_integration(
            blocker.id(),
            &LeaseToken::new("by-hand"),
            "awaiting_integration",
            "given back",
            &dagq::domain::Reason::new(ReasonCode::Other),
        )
        .unwrap();
    let outcome = joined(supervisor, "the supervisor to drain").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
}
