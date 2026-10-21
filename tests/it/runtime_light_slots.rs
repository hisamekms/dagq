//! Runtime tests: a run that waits only for its landing turn leaves room
//! for a task of `[supervisor] light_changes` that declares its `--paths`,
//! and only for one (ADR-t1591-1).
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

/// With `light_changes = ["docs"]` and one slot, a run whose review passed
/// waits for its landing turn behind another landing: the room it leaves
/// claims the `docs` task that declares its paths, and neither the
/// `feature` task (interrupt) nor the `docs` task without paths, which wait
/// for a slot with the landing queue in it. `status` shows the landing
/// queue out of the slots, and the light claim says so.
#[test]
fn the_landing_queue_leaves_room_for_light_tasks_only() {
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

    let heavy = add_task(&db, "heavy", "feature", &["src/**"], Priority::Interrupt);
    let unscoped = add_task(&db, "unscoped", "docs", &[], Priority::High);
    let light = add_task(&db, "light", "docs", &["file-*"], Priority::Normal);
    // The fixture's run of task 1 was the first claim.
    wait_until(&db, Duration::from_secs(60), |_| claims(&db).len() == 3);
    let claimed = claims(&db);
    assert_eq!(claimed[1].0, first, "{claimed:?}");
    assert!(claimed[1].1.get("light_room").is_none(), "{claimed:?}");
    assert_eq!(claimed[2].0, light, "{claimed:?}");
    assert_eq!(claimed[2].1["light_room"], true, "{claimed:?}");
    // It counted the run in the landing queue among the slots.
    assert_eq!(claimed[2].1["slots"], 1, "{claimed:?}");
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

    // Further passes claim neither the heavy task nor the one without paths.
    await_passes(&passes, SOME_PASSES);
    let mut queue = SqliteQueue::open(&db).unwrap();
    for task in [heavy, unscoped] {
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

/// A parked `needs_session` run waits for a normal slot: while the landing
/// queue fills `parallel`, it is not resumed and only the light task is
/// claimed; once the slots free, it is resumed ahead of a new claim of the
/// same effective priority (ADR-t1850-1): its task, set to interrupt while
/// in progress, goes before the interrupt one.
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

    // The run parks while the landing queue fills the slot.
    queue
        .park_rechecked(parked.id(), None, "parked for the test", json!({}))
        .unwrap()
        .unwrap();
    let heavy = add_task(&db, "heavy", "feature", &["src/**"], Priority::Interrupt);
    let light = add_task(&db, "light", "docs", &["file-*"], Priority::Normal);
    // Tasks 1 and 2, then `first`, then the light one.
    wait_until(&db, Duration::from_secs(60), |_| claims(&db).len() == 4);
    let claimed = claims(&db);
    assert_eq!(claimed[3].0, light, "{claimed:?}");
    assert_eq!(claimed[3].1["light_room"], true, "{claimed:?}");
    await_passes(&passes, SOME_PASSES);
    let kinds = task_events(&db, TaskId::new(2));
    assert!(
        !kinds.iter().any(|(_, kind)| kind == "resume_started"),
        "{kinds:?}"
    );
    assert_eq!(
        queue.run(parked.id()).unwrap().status(),
        RunStatus::NeedsSession
    );
    assert_eq!(queue.show(heavy).unwrap().task.status(), TaskStatus::Ready);

    // The other landing gives its slot back: `first` and the light run
    // land, and the freed slot resumes the parked run, raised to the heavy
    // task's priority, before the claim.
    queue
        .set_priority(TaskId::new(2), Some(Priority::Interrupt))
        .unwrap();
    queue
        .abort_integration(
            blocker.id(),
            &LeaseToken::new("by-hand"),
            "awaiting_integration",
            "given back",
            &dagq::domain::Reason::new(ReasonCode::Other),
        )
        .unwrap();
    wait_until(&db, Duration::from_secs(120), |_| {
        task_events(&db, TaskId::new(2))
            .iter()
            .any(|(_, kind)| kind == "resume_started")
    });
    stop.store(true, Ordering::SeqCst);
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

/// In the room the landing queue leaves, the aging compares only the light
/// tasks (ADR-t1487-2 decision 3, ADR-t1489-1), and `candidates` shows the
/// same order: a heavy implementation that waited longest is deferred for
/// the light room, so the light Spikes past `class_aging` go before a
/// fresh light implementation of the same priority. The claim records
/// that the first Spike aged, and `candidates` then shows the second one
/// first, aged, with the heavy task in `deferred` as `light_room`.
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
    let heavy = add_task(&db, "heavy", "feature", &["src/**"], Priority::Normal);
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
    // The fixture's run of task 1, then `first`, then the light room's.
    wait_until(&db, Duration::from_secs(60), |_| claims(&db).len() == 3);
    let claimed = claims(&db);
    assert_eq!(claimed[2].0, spikes[0], "{claimed:?}");
    assert_eq!(claimed[2].1["light_room"], true, "{claimed:?}");
    assert_eq!(claimed[2].1["aged"], true, "{claimed:?}");

    let view = crate::common::cli::ok(&db, &["candidates"]);
    let ids: Vec<i64> = view["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| task["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [spikes[1].as_i64(), fresh.as_i64()], "{view}");
    assert_eq!(view["aged"], json!([spikes[1].as_i64()]), "{view}");
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
    assert_eq!(
        deferred,
        [(heavy.as_i64(), "light_room".to_owned())],
        "{view}"
    );

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

/// The landing queue's room and a free normal slot taking turns, here by
/// `parallel` going from 1 to 2 and back while the claim spacing holds
/// every claim: the heavy task's `light_room` deferral stays one record,
/// so `stats` counts one deferral and `status` shows the same one
/// throughout, while `candidates` shows the task deferred in the light
/// room and first in the order over the free slot, as the claim would
/// take it. With the spacing gone, a free slot claims it as shown, while
/// it is still recorded, and only then does the record end.
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
    // From here the spacing holds every claim, in either room.
    set_supervisor(&repo, &db, &passes, 1, 3600);
    // Heavy for its change: its paths are those its worker writes.
    let heavy = add_task(&db, "heavy", "feature", &["file-*"], Priority::Normal);
    wait_until(&db, Duration::from_secs(60), |_| {
        !events_of(&db, heavy, EventKind::ClaimDeferred).is_empty()
    });
    let deferred = events_of(&db, heavy, EventKind::ClaimDeferred);
    assert_eq!(deferred[0]["reason"], "light_room", "{deferred:?}");
    let open = |db: &Path| -> Vec<(Value, Value)> {
        runtime::status(db).unwrap()["claim_deferrals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|open| (open["task_id"].clone(), open["since"].clone()))
            .collect()
    };
    let before = open(&db);
    assert_eq!(before.len(), 1, "{before:?}");
    assert_eq!(before[0].0, json!(heavy), "{before:?}");

    let in_light_room = (Vec::new(), vec![(heavy.as_i64(), "light_room".to_owned())]);
    assert_eq!(shown(&db), in_light_room);
    // Another supervisor's records of its slots, many more than the
    // newest few of the queue: the light room's supervisor is read by its
    // own newest one, however old.
    let mut queue = SqliteQueue::open(&db).unwrap();
    for _ in 0..300 {
        for kind in [EventKind::SlotsFullStarted, EventKind::SlotsFullEnded] {
            queue
                .record_queue_event(kind, json!({"supervisor": "elsewhere", "class": "light"}))
                .unwrap();
        }
    }
    assert_eq!(shown(&db), in_light_room);

    // A free normal slot and the light room, by turns.
    for _ in 0..2 {
        set_supervisor(&repo, &db, &passes, 2, 3600);
        assert_eq!(shown(&db), (vec![heavy.as_i64()], Vec::new()));
        assert_eq!(open(&db), before);
        set_supervisor(&repo, &db, &passes, 1, 3600);
        assert_eq!(shown(&db), in_light_room);
    }
    assert_eq!(events_of(&db, heavy, EventKind::ClaimDeferred).len(), 1);
    assert!(events_of(&db, heavy, EventKind::ClaimDeferralEnded).is_empty());
    assert_eq!(open(&db), before);
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["claim_deferrals"]["count"], 1, "{stats}");
    assert_eq!(claims(&db).len(), 2);

    // A free slot with no spacing claims the heavy task, still recorded:
    // its record ends as it leaves the candidates.
    set_supervisor(&repo, &db, &passes, 2, 0);
    wait_until(&db, Duration::from_secs(60), |_| claims(&db).len() == 3);
    // The claim takes it first, as `candidates` showed over the free slot.
    let claimed = claims(&db);
    assert_eq!(claimed[2].0, heavy, "{claimed:?}");
    assert!(claimed[2].1.get("light_room").is_none(), "{claimed:?}");
    wait_until(&db, Duration::from_secs(60), |_| {
        !events_of(&db, heavy, EventKind::ClaimDeferralEnded).is_empty()
    });
    let ended = events_of(&db, heavy, EventKind::ClaimDeferralEnded);
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(ended[0]["why"], "not_candidate", "{ended:?}");
    assert_eq!(events_of(&db, heavy, EventKind::ClaimDeferred).len(), 1);
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["claim_deferrals"]["count"], 1, "{stats}");

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
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["claim_deferrals"]["count"], 1, "{stats}");
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
