//! Runtime tests: deferring the claim of a task whose files meet a run in
//! flight on a conflict hotspot (ADR-0069).
use crate::runtime_support;
use dagq::domain::EventKind;

use dagq::{application::DraftPlannerStore, domain::stats::ConflictConfig};
use runtime_support::*;
use std::sync::atomic::{AtomicBool, Ordering};

const HOT: &str = "docs/hot.md";

fn add_task(queue: &mut SqliteQueue, title: &str, paths: &[&str], priority: Priority) -> TaskId {
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: Vec::new(),
            // The stand-in worker commits change.txt.
            paths: paths
                .iter()
                .map(|path| (*path).to_owned())
                .chain(["change.txt".to_owned()])
                .collect(),
            priority: Some(priority),
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    task.id()
}

fn events(db: &Path, kind: &str) -> Vec<(Option<TaskId>, Value)> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == kind)
        .map(|event| (event.task_id, event.payload))
        .collect()
}

fn runs_of(db: &Path, task: TaskId) -> usize {
    SqliteQueue::open(db)
        .unwrap()
        .show(task)
        .unwrap()
        .runs
        .len()
}

fn options(defer_max_secs: i64) -> SuperviseOptions {
    SuperviseOptions {
        conflicts: Some(ConflictConfig {
            defer_max_secs,
            ..ConflictConfig::default()
        }),
        ..supervise_options(3, true)
    }
}

/// The fixture with the hot file committed and three conflicts on it,
/// which make it an alert of `conflict_hotspots` with the default
/// thresholds; a draft task carries them.
fn hot_fixture() -> (Fixture, PathBuf, PathBuf) {
    let (dir, repo, db) = fixture();
    fs::create_dir(repo.join("docs")).unwrap();
    fs::write(repo.join(HOT), "hot\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "hot file"]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let old = queue
        .add(NewTask {
            title: "landed long ago".into(),
            description: "d".into(),
            acceptance: "a".into(),
            verification_commands: Vec::new(),
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Some(Priority::Normal),
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap()
        .id();
    for main in ["m1", "m2", "m3"] {
        queue
            .record_task_event(
                old,
                EventKind::IntegrationDeferred,
                json!({"conflicts": [HOT], "main": main}),
            )
            .unwrap();
    }
    (dir, repo, db)
}

/// A task whose declared paths meet a run in flight on a hotspot is not
/// claimed, and the next candidate that does not meet is; the deferral is
/// recorded once with the files and the run in the way, and `status` and
/// `stats` show it. A task of interrupt priority is claimed over the same
/// files, and the deferred task is claimed once its deferral has lasted
/// `defer_max_secs`, which holds across supervisors.
#[test]
fn a_task_meeting_a_run_on_a_hotspot_waits_and_the_next_one_is_claimed() {
    let (_dir, repo, db) = hot_fixture();
    let (hot, near, apart) = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        // Task 1 (the fixture's) declares no paths and has no related
        // landing: nothing is expected of it.
        let hot = add_task(&mut queue, "edits the hot file", &[HOT], Priority::Normal);
        let near = add_task(&mut queue, "edits the docs", &[HOT], Priority::Normal);
        let apart = add_task(
            &mut queue,
            "edits elsewhere",
            &["other.txt"],
            Priority::Normal,
        );
        (hot, near, apart)
    };
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(
        stats["conflict_hotspots"]["files"][0]["alert"], true,
        "{stats}"
    );

    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &options(3600)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(runs_of(&db, TaskId::new(1)), 1);
    assert_eq!(runs_of(&db, hot), 1);
    assert_eq!(runs_of(&db, apart), 1, "the next candidate is claimed");
    assert_eq!(runs_of(&db, near), 0, "the task meeting the hot run waits");
    let deferred = events(&db, "claim_deferred");
    assert_eq!(deferred.len(), 1, "{deferred:?}");
    let (task, payload) = &deferred[0];
    assert_eq!(*task, Some(near));
    assert_eq!(payload["reason"], "hot_files");
    assert_eq!(payload["files"], json!([HOT]));
    assert_eq!(payload["runs"].as_array().unwrap().len(), 1, "{payload}");
    assert_eq!(payload["runs"][0]["task_id"], json!(hot));
    assert_eq!(payload["max_secs"], 3600);
    assert!(payload["message"].as_str().unwrap().contains(HOT));

    let status = runtime::status(&db).unwrap();
    let open = status["claim_deferrals"].as_array().unwrap();
    assert_eq!(open.len(), 1, "{status}");
    assert_eq!(open[0]["task_id"], json!(near));
    assert_eq!(open[0]["files"], json!([HOT]));
    assert!(open[0]["since"].is_string());
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    let deferrals = &stats["claim_deferrals"];
    assert_eq!(deferrals["count"], 1, "{deferrals}");
    assert_eq!(deferrals["by_file"][HOT], 1);
    assert_eq!(deferrals["deferred"][0]["task_id"], json!(near));

    // An interrupt over the same file is claimed; the deferred task still
    // waits and is not recorded again.
    let interrupt = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        add_task(&mut queue, "stops the line", &[HOT], Priority::Interrupt)
    };
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &options(3600)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(runs_of(&db, interrupt), 1, "an interrupt is not deferred");
    assert_eq!(runs_of(&db, near), 0);
    assert_eq!(events(&db, "claim_deferred").len(), 1);
    assert!(events(&db, "claim_deferral_ended").is_empty());

    // Past the limit, counted from the first deferral, the task is claimed:
    // the limit of 1 is past from the second after the deferral's.
    let deferred_at = SqliteQueue::open(&db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .find(|event| event.kind == "claim_deferred")
        .and_then(|event| dagq::domain::stats::timestamp_millis(&event.created_at))
        .unwrap();
    await_second_after(deferred_at.div_euclid(1000));
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &options(1)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(runs_of(&db, near), 1, "an expired deferral is claimed");
    let ended = events(&db, "claim_deferral_ended");
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(ended[0].0, Some(near));
    assert_eq!(ended[0].1["why"], "expired");
    assert!(ended[0].1["deferred_secs"].as_i64().unwrap() >= 1);
    assert_eq!(events(&db, "claim_deferred").len(), 1);
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["claim_deferrals"], json!([]), "{status}");
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["claim_deferrals"]["by_end"]["expired"]["count"], 1);
    assert_eq!(stats["claim_deferrals"]["deferred"], json!([]));
}

/// A run in the way that only waits for a person's answer (an open
/// `approve_landing` and no lease) holds the task back within
/// `waiting_owner_grace_secs`, as any run in flight; past it, the task is
/// claimed before `defer_max_secs` and the deferral ends as
/// `owner_waiting` (ADR-t1484-1).
#[test]
fn a_run_waiting_for_its_owner_past_the_grace_lets_the_deferred_task_go() {
    let (_dir, repo, db) = hot_fixture();
    let (hot, near) = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        let hot = add_task(&mut queue, "edits the hot file", &[HOT], Priority::Normal);
        let near = add_task(&mut queue, "edits the docs", &[HOT], Priority::Normal);
        (hot, near)
    };
    let with_grace = |grace: i64| SuperviseOptions {
        conflicts: Some(ConflictConfig {
            defer_max_secs: 3600,
            waiting_owner_grace_secs: grace,
            ..ConflictConfig::default()
        }),
        ..supervise_options(3, true)
    };
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &with_grace(600)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(runs_of(&db, hot), 1);
    assert_eq!(runs_of(&db, near), 0, "the task meeting the hot run waits");

    // The run in the way rests on a person's answer.
    let asked = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        let run = queue.show(hot).unwrap().runs[0].id().clone();
        assert!(queue.run_lease(&run).unwrap().is_none());
        queue
            .ask(NewAsk {
                recommendation: None,
                confidence: None,
                topics: Vec::new(),
                kind: AskKind::ApproveLanding,
                task_id: None,
                run_id: Some(run),
                question: "land it?".into(),
                options: vec!["land".into(), "send_back".into(), "cancel".into()],
                asked_by: "supervisor".into(),
                reason_category: dagq::domain::AskReason::Scope,
                finding_id: None,
                request_id: None,
            })
            .unwrap()
            .ask
    };
    // Within the grace, the task still waits.
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &with_grace(600)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(runs_of(&db, near), 0, "within the grace the task waits");
    assert!(events(&db, "claim_deferral_ended").is_empty());

    // Past the grace, it is claimed long before the limit.
    await_second_after(asked.created_at);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &with_grace(1)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(runs_of(&db, near), 1, "the owner's wait no longer holds it");
    let ended = events(&db, "claim_deferral_ended");
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(ended[0].0, Some(near));
    assert_eq!(ended[0].1["why"], "owner_waiting");
    assert_eq!(events(&db, "claim_deferred").len(), 1);
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(
        stats["claim_deferrals"]["by_end"]["owner_waiting"]["count"], 1,
        "{stats}"
    );
}

/// Run the task in the way with `agent`, which fails its validation, then
/// a pass with a stand-in that works: the fixture, the queue and the task
/// that was deferred behind the failed run.
fn after_a_failed_run_in_the_way(agent: &str) -> (Fixture, PathBuf, TaskId) {
    let (dir, repo, db) = hot_fixture();
    let (hot, near) = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        let hot = add_task(&mut queue, "edits the hot file", &[HOT], Priority::Normal);
        let near = add_task(&mut queue, "edits the docs", &[HOT], Priority::Normal);
        (hot, near)
    };
    let backend = TestWorkspace::new(&db, false, agent);
    let outcome = supervise_with(&db, &repo, &backend, &options(3600)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = SqliteQueue::open(&db).unwrap().show(hot).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::Failed);
    assert_eq!(events(&db, "claim_deferred").len(), 1);
    // The stats window starts at the first finished run: the conflicts are
    // recorded again so the file stays a hotspot.
    let mut queue = SqliteQueue::open(&db).unwrap();
    for main in ["m4", "m5", "m6"] {
        queue
            .record_task_event(
                TaskId::new(2),
                EventKind::IntegrationDeferred,
                json!({"conflicts": [HOT], "main": main}),
            )
            .unwrap();
    }
    drop(queue);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let outcome = supervise_with(&db, &repo, &backend, &options(3600)).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    (dir, db, near)
}

/// A run in the way that failed with no commit of its own (its receipt
/// names its base) holds the task back no longer: the task is claimed at
/// once, without the grace or the limit, and the deferral ends as
/// `no_commit` (ADR-t1634-1).
#[test]
fn a_failed_run_with_no_commit_of_its_own_lets_the_deferred_task_go() {
    let (_dir, db, near) = after_a_failed_run_in_the_way("receipt \"$BASE\"");
    assert_eq!(runs_of(&db, near), 1, "the failed run holds nothing");
    let ended = events(&db, "claim_deferral_ended");
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(ended[0].0, Some(near));
    assert_eq!(ended[0].1["why"], "no_commit");
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(
        stats["claim_deferrals"]["by_end"]["no_commit"]["count"], 1,
        "{stats}"
    );
}

/// A run in the way that failed with a commit of its own still holds the
/// task back (ADR-t1634-1).
#[test]
fn a_failed_run_with_a_commit_of_its_own_still_holds_the_task_back() {
    let (_dir, db, near) = after_a_failed_run_in_the_way("commit work; receipt \"$BASE\"");
    assert_eq!(runs_of(&db, near), 0, "a commit of its own is in the way");
    let ended = events(&db, "claim_deferral_ended");
    assert!(ended.is_empty(), "{ended:?}");
}

/// A supervisor reads `[conflicts]` of the main checkout's `dagq.toml`
/// again every pass (ADR-0080): a lower `hotspot_conflicts` makes the hot
/// file a hotspot and a task over it waits, with the file's limit; an
/// invalid table keeps the values in use; a shorter `defer_max_secs` ends
/// the deferrals in place. Each change is recorded once.
#[test]
fn a_changed_conflicts_table_is_read_again_without_a_restart() {
    let (_dir, repo, db) = hot_fixture();
    let config = repo.join("dagq.toml");
    // Written whole by a rename; changes take effect after two reads.
    let conflicts = |table: &str| {
        let staged = repo.join("dagq.toml.new");
        fs::write(&staged, format!("[conflicts]\n{table}")).unwrap();
        fs::rename(&staged, &config).unwrap();
    };
    // Four conflicts needed: the hot file is no hotspot.
    conflicts("hotspot_conflicts = 4\ndefer_max_secs = 7200\n");
    let add = |title: &str, paths: &[&str]| {
        add_task(
            &mut SqliteQueue::open(&db).unwrap(),
            title,
            paths,
            Priority::Normal,
        )
    };
    let hot = add("edits the hot file", &[HOT]);
    let backend = Arc::new(TestWorkspace::new(&db, false, VALID_AGENT));
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        ..supervise_options(8, false)
    };
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    let limit = Duration::from_secs(60);
    wait_until(&db, limit, |_| runs_of(&db, hot) == 1);
    let near = add("edits the docs", &[HOT]);
    wait_until(&db, limit, |_| runs_of(&db, near) == 1);
    assert!(events(&db, "claim_deferred").is_empty());
    assert!(events(&db, "conflicts_config_changed").is_empty());

    // Three conflicts make it a hotspot: the next task over it waits.
    conflicts("hotspot_conflicts = 3\ndefer_max_secs = 7200\n");
    wait_until(&db, limit, |_| {
        events(&db, "conflicts_config_changed").len() == 1
    });
    let later = add("edits the docs later", &[HOT]);
    wait_until(&db, limit, |_| events(&db, "claim_deferred").len() == 1);
    let deferred = events(&db, "claim_deferred");
    assert_eq!(deferred[0].0, Some(later));
    assert_eq!(deferred[0].1["max_secs"], 7200, "{deferred:?}");

    // An invalid table keeps the values in use, not the defaults (claims
    // wait meanwhile: the landing branch is read from the same file, so
    // the runs in flight rest first, their reviews and landings done).
    wait_until(&db, limit, |queue| {
        [TaskId::new(1), hot, near].iter().all(|&task| {
            queue
                .show(task)
                .unwrap()
                .runs
                .first()
                .is_some_and(|run| queue.run_lease(run.id()).unwrap().is_none())
        })
    });
    conflicts("hotspot_conflicts = 3\ndefer_max_secs = 0\n");
    let last = add("edits the docs last", &[HOT]);
    wait_until(&db, limit, |_| {
        events(&db, "candidates_sampled")
            .iter()
            .any(|(_, sample)| sample["candidates"] == 2)
    });
    await_passes(&passes, SOME_PASSES);
    assert_eq!(runs_of(&db, last), 0);
    assert_eq!(events(&db, "conflicts_config_changed").len(), 1);
    // Valid again with the values in use: no change, and the task waits
    // with the limit kept.
    conflicts("hotspot_conflicts = 3\ndefer_max_secs = 7200\n");
    wait_until(&db, limit, |_| events(&db, "claim_deferred").len() == 2);
    let deferred = events(&db, "claim_deferred");
    assert_eq!(deferred[1].0, Some(last));
    assert_eq!(deferred[1].1["max_secs"], 7200, "{deferred:?}");
    assert_eq!(runs_of(&db, later), 0);
    assert_eq!(events(&db, "conflicts_config_changed").len(), 1);

    // A shorter limit ends the deferrals in place, counted from their
    // start.
    conflicts("hotspot_conflicts = 3\ndefer_max_secs = 1\n");
    wait_until(&db, limit, |_| {
        runs_of(&db, later) == 1 && runs_of(&db, last) == 1
    });
    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to drain").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let changed = events(&db, "conflicts_config_changed");
    assert_eq!(changed.len(), 2, "{changed:?}");
    let (task, first) = &changed[0];
    assert_eq!(*task, None);
    assert_eq!(first["from"]["hotspot_conflicts"], 4, "{first}");
    assert_eq!(first["to"]["hotspot_conflicts"], 3);
    assert_eq!(first["source"], "file");
    assert!(first["supervisor"].is_string());
    assert_eq!(changed[1].1["from"]["defer_max_secs"], 7200);
    assert_eq!(changed[1].1["to"]["defer_max_secs"], 1);
    let ended = events(&db, "claim_deferral_ended");
    assert_eq!(ended.len(), 2, "{ended:?}");
    assert!(ended.iter().all(|(_, payload)| payload["why"] == "expired"));
}

/// Drive file writes at the claim-load check, after this pass read conflicts
/// and before the next pass. No sleeps or races with the config reader: the
/// callback runs on the supervisor thread, and this queue has no ready task.
#[test]
fn transient_conflicts_tables_are_ignored_and_stable_changes_are_confirmed() {
    use std::cell::RefCell;

    struct Script {
        config: PathBuf,
        db: PathBuf,
        stop: Arc<AtomicBool>,
        step: usize,
    }
    thread_local! {
        static SCRIPT: RefCell<Option<Script>> = const { RefCell::new(None) };
    }
    const ORIGINAL: &str = "[conflicts]\nhotspot_conflicts = 4\ndefer_max_secs = 7200\n";
    const PARTIAL: &str = "[conflicts]\nhotspot_conflicts = 4\n";
    const CHANGED: &str = "[conflicts]\nhotspot_conflicts = 5\ndefer_max_secs = 8000\n";
    fn after_read() -> Option<f64> {
        SCRIPT.with(|script| {
            let mut script = script.borrow_mut();
            let script = script.as_mut().unwrap();
            // Each pair is the number of applied changes *this* pass and
            // the file the next pass will read. Empty files, missing tables,
            // and a parseable partial table all go through the real loader.
            let steps = [
                (0, ""),
                (0, ORIGINAL),
                (0, PARTIAL),
                (0, ORIGINAL),
                (0, "[run.env]\n"),
                (0, "[run.env]\n"),
                (1, "[run.env]\n"),
                (1, CHANGED),
                (1, CHANGED),
                (2, CHANGED),
                (2, CHANGED),
            ];
            let (count, next) = steps[script.step];
            let changed = events(&script.db, "conflicts_config_changed");
            assert_eq!(
                changed.len(),
                count,
                "pass {}: {changed:?}",
                script.step + 1
            );
            if count >= 1 {
                assert_eq!(changed[0].1["from"]["defer_max_secs"], 7200);
                assert_eq!(changed[0].1["to"], json!(ConflictConfig::default()));
            }
            if count == 2 {
                assert_eq!(changed[1].1["from"], json!(ConflictConfig::default()));
                assert_eq!(changed[1].1["to"]["hotspot_conflicts"], 5);
                assert_eq!(changed[1].1["to"]["defer_max_secs"], 8000);
            }
            fs::write(&script.config, next).unwrap();
            script.step += 1;
            if script.step == steps.len() {
                script.stop.store(true, Ordering::SeqCst);
            }
        });
        Some(0.0)
    }

    let (_dir, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let config = repo.join("dagq.toml");
    fs::write(&config, ORIGINAL).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    SCRIPT.with(|script| {
        *script.borrow_mut() = Some(Script {
            config,
            db: db.clone(),
            stop: stop.clone(),
            step: 0,
        });
    });
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let options = SuperviseOptions {
        stop,
        load_average: after_read,
        ..supervise_options(1, false)
    };
    let outcome = supervise_with(&db, &repo, &backend, &options).unwrap();
    backend.join();
    SCRIPT.with(|script| assert_eq!(script.borrow_mut().take().unwrap().step, 11));
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(events(&db, "conflicts_config_changed").len(), 2);
}

type Step = Box<dyn FnOnce()>;

thread_local! {
    static STEPS: std::cell::RefCell<std::collections::VecDeque<Step>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
    static STOP: std::cell::RefCell<Option<Arc<AtomicBool>>> =
        const { std::cell::RefCell::new(None) };
}

/// The claim-load check of each pass, after it read `[conflicts]`: runs
/// the next step, and stops the supervisor after the last.
fn next_step() -> Option<f64> {
    if let Some(step) = STEPS.with(|steps| steps.borrow_mut().pop_front()) {
        step();
    }
    if STEPS.with(|steps| steps.borrow().is_empty()) {
        STOP.with(|stop| {
            stop.borrow()
                .as_ref()
                .unwrap()
                .store(true, Ordering::SeqCst)
        });
    }
    Some(0.0)
}

/// Run a supervisor over the queue with no ready task, one step a pass.
fn supervise_steps(db: &Path, repo: &Path, steps: Vec<Step>) {
    supervise_steps_with(db, repo, steps, |options| options);
}

/// [`supervise_steps`] with the options `adjust` gives.
fn supervise_steps_with(
    db: &Path,
    repo: &Path,
    steps: Vec<Step>,
    adjust: impl FnOnce(SuperviseOptions) -> SuperviseOptions,
) {
    let stop = Arc::new(AtomicBool::new(false));
    STEPS.with(|queued| *queued.borrow_mut() = steps.into());
    STOP.with(|held| *held.borrow_mut() = Some(stop.clone()));
    let backend = TestWorkspace::new(db, false, VALID_AGENT);
    let options = adjust(SuperviseOptions {
        stop,
        load_average: next_step,
        ..supervise_options(1, false)
    });
    let outcome = supervise_with(db, repo, &backend, &options).unwrap();
    backend.join();
    assert!(STEPS.with(|steps| steps.borrow().is_empty()));
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
}

/// A supervisor started with values other than the latest
/// `conflicts_config_changed`'s `to` records them (ADR-t775-1): after X -> Y
/// and a restart with X, the latest event moves to X, so a change to Y is
/// recorded again. A restart with the latest `to` records nothing, and a
/// change another supervisor recorded is not recorded twice.
#[test]
fn values_started_with_are_recorded_against_the_latest_change() {
    const X: &str = "[conflicts]\nhotspot_conflicts = 4\n";
    const Y: &str = "[conflicts]\nhotspot_conflicts = 5\n";
    let (_dir, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let config = repo.join("dagq.toml");
    // After this pass, `count` changes are recorded; then `next` is written.
    let step = |count: usize, next: Option<&'static str>| -> Step {
        let (db, config) = (db.clone(), config.clone());
        Box::new(move || {
            let changed = events(&db, "conflicts_config_changed");
            assert_eq!(changed.len(), count, "{changed:?}");
            if let Some(next) = next {
                fs::write(&config, next).unwrap();
            }
        })
    };
    let hotspot_conflicts = |payload: &Value, key: &str| payload[key]["hotspot_conflicts"].clone();

    // X -> Y, recorded; X written back while no supervisor runs.
    fs::write(&config, X).unwrap();
    supervise_steps(
        &db,
        &repo,
        vec![step(0, Some(Y)), step(0, None), step(1, Some(X))],
    );
    // Started with X: Y -> X from the start, and Y is a change again.
    supervise_steps(
        &db,
        &repo,
        vec![step(2, Some(Y)), step(2, None), step(3, None)],
    );
    let changed = events(&db, "conflicts_config_changed");
    assert_eq!(hotspot_conflicts(&changed[1].1, "from"), 5);
    assert_eq!(hotspot_conflicts(&changed[1].1, "to"), 4);
    assert_eq!(changed[1].1["source"], "start");
    assert!(changed[1].1["supervisor"].is_string());
    assert_eq!(hotspot_conflicts(&changed[2].1, "from"), 4);
    assert_eq!(hotspot_conflicts(&changed[2].1, "to"), 5);
    assert_eq!(changed[2].1["source"], "file");

    // Started with Y, the latest `to`: nothing. Another supervisor records
    // Y -> X; this one, reading X, does not record it again.
    let other = {
        let db = db.clone();
        Box::new(move || {
            SqliteQueue::open(&db)
                .unwrap()
                .record_queue_event(
                    EventKind::ConflictsConfigChanged,
                    json!({
                        "from": ConflictConfig { hotspot_conflicts: 5, ..ConflictConfig::default() },
                        "to": ConflictConfig { hotspot_conflicts: 4, ..ConflictConfig::default() },
                        "source": "file",
                        "supervisor": "another",
                    }),
                )
                .unwrap();
        }) as Step
    };
    supervise_steps(
        &db,
        &repo,
        vec![
            step(3, None),
            other,
            step(4, Some(X)),
            step(4, None),
            step(4, None),
        ],
    );
    // Started with X, the latest `to` again: nothing.
    supervise_steps(&db, &repo, vec![step(4, None)]);
    assert_eq!(events(&db, "conflicts_config_changed").len(), 4);
}

/// An invalid `[conflicts]` at the start is warned of once (ADR-t775-1):
/// the start's warn, and not again by the reads of each pass that meet the
/// same error. The whole `dagq.toml` is parsed for the landing branch
/// before `[conflicts]` is read, so a file invalid at both stops the start
/// there; the reader stands in for a file that turned invalid in between.
/// The defaults it leaves are not recorded as a change.
#[test]
fn an_invalid_conflicts_table_at_the_start_is_warned_of_once() {
    use dagq::infrastructure::telemetry::Telemetry;
    const NOT_READ: &str = "[conflicts] of dagq.toml not read";
    fn invalid(_: &Path) -> Result<Option<ConflictConfig>> {
        bail!("dagq.toml:2: value of hotspot_conflicts: must be a positive number, not 0")
    }
    let (_dir, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let (telemetry, captured) = Telemetry::capture();
    // As in lifecycle_up: a second dispatcher makes every callsite ask the
    // thread's default, whichever test reached it first.
    let _second = tracing::Dispatch::new(tracing_subscriber::registry());
    tracing::callsite::rebuild_interest_cache();
    let steps: Vec<Step> = (0..3).map(|_| Box::new(|| ()) as Step).collect();
    telemetry.in_scope(|| {
        supervise_steps_with(&db, &repo, steps, |options| SuperviseOptions {
            load_conflicts: invalid,
            ..options
        })
    });
    let warned: Vec<Value> = captured
        .records()
        .into_iter()
        .filter(|record| {
            record["message"]
                .as_str()
                .is_some_and(|message| message.starts_with(NOT_READ))
        })
        .collect();
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(
        warned[0]["message"]
            .as_str()
            .unwrap()
            .ends_with("using the defaults"),
        "{warned:?}"
    );
    assert!(events(&db, "conflicts_config_changed").is_empty());
}

fn high_load() -> Option<f64> {
    Some(40.0)
}

/// `candidates` shows the supervisor's last judgment of the deferrals
/// (ADR-t1992-1): while every slot is taken, and then while the claims are
/// held for the load, the supervisor does not judge the claims, so the
/// deferral it recorded stays open past `defer_max_secs` and `candidates`
/// keeps the task in `deferred`, closing and adding none; `held` names the
/// hold and the supervisor's absence.
#[test]
fn a_deferral_stays_shown_while_the_supervisor_does_not_judge_the_claims() {
    let (_dir, repo, db) = hot_fixture();
    let (hot, near, apart) = {
        let mut queue = SqliteQueue::open(&db).unwrap();
        let hot = add_task(&mut queue, "edits the hot file", &[HOT], Priority::Normal);
        let near = add_task(&mut queue, "edits the docs", &[HOT], Priority::Normal);
        let apart = add_task(
            &mut queue,
            "edits elsewhere",
            &["other.txt"],
            Priority::Normal,
        );
        (hot, near, apart)
    };
    let shown = |db: &Path| crate::common::cli::ok(db, &["candidates"]);
    let deferred = |view: &Value| -> Vec<Value> {
        view["deferred"]
            .as_array()
            .unwrap()
            .iter()
            .map(|deferral| deferral["task_id"].clone())
            .collect()
    };
    let held = |view: &Value| -> Vec<Value> {
        view["held"]
            .as_array()
            .unwrap()
            .iter()
            .map(|held| held["reason"].clone())
            .collect()
    };

    // Three slots, three sessions that work until the test lets them go:
    // task 1, the hot task and the one apart take every slot, and the task
    // meeting the hot run is deferred.
    let backend = Arc::new(TestWorkspace::new(&db, false, GATED_AGENT));
    let working = options(1);
    let passes = working.passes.clone();
    let supervisor = {
        let (db, repo, backend, working) =
            (db.clone(), repo.clone(), backend.clone(), working.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &working))
    };
    wait_until(&db, Duration::from_secs(60), |_| {
        events(&db, "claim_deferred").len() == 1
            && [TaskId::new(1), hot, apart]
                .iter()
                .all(|task| runs_of(&db, *task) == 1)
    });
    let deferred_at = SqliteQueue::open(&db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .find(|event| event.kind == "claim_deferred")
        .and_then(|event| dagq::domain::stats::timestamp_millis(&event.created_at))
        .unwrap();
    // Past the limit, more passes with no free slot: no judgment.
    await_second_after(deferred_at.div_euclid(1000) + 1);
    await_passes(&passes, SOME_PASSES);
    assert!(events(&db, "claim_deferral_ended").is_empty());
    assert_eq!(runs_of(&db, near), 0);
    let view = shown(&db);
    assert_eq!(deferred(&view), [json!(near)], "{view}");
    assert!(view["candidates"].as_array().unwrap().is_empty(), "{view}");
    assert!(!held(&view).contains(&json!("no_supervisor")), "{view}");
    assert_eq!(view["deferred"][0]["reason"], "hot_files");
    assert_eq!(view["deferred"][0]["files"], json!([HOT]));

    // Let the sessions finish and the supervisor drain.
    working.stop.store(true, Ordering::SeqCst);
    for task in [TaskId::new(1), hot, apart] {
        let run = SqliteQueue::open(&db).unwrap().show(task).unwrap().runs[0].clone();
        fs::write(
            Path::new(run.run_dir().unwrap()).join("exit-requested.go"),
            "",
        )
        .unwrap();
    }
    joined(supervisor, "the supervisor thread to drain").unwrap();
    backend.join();

    // A pass with the claims held for the load does not judge them either.
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let held_pass = SuperviseOptions {
        max_load: Some(16.0),
        load_average: high_load,
        ..options(1)
    };
    let outcome = supervise_with(&db, &repo, &backend, &held_pass).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(!events(&db, "claim_held").is_empty());
    assert!(events(&db, "claim_deferral_ended").is_empty());
    assert_eq!(runs_of(&db, near), 0);
    let view = shown(&db);
    assert_eq!(deferred(&view), [json!(near)], "{view}");
    // The hold's supervisor is gone: its record holds nothing now.
    assert_eq!(held(&view), [json!("no_supervisor")], "{view}");
    assert_eq!(
        crate::common::cli::ok(&db, &["graph"])["deferred"],
        view["deferred"]
    );
}
