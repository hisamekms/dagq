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
            priority,
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(worker_mode()),
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
            priority: Priority::Normal,
            change: None,
            dependencies: Vec::new(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(worker_mode()),
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
        let near = add_task(&mut queue, "edits the docs", &["docs/**"], Priority::Normal);
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
    let near = add("edits the docs", &["docs/**"]);
    wait_until(&db, limit, |_| runs_of(&db, near) == 1);
    assert!(events(&db, "claim_deferred").is_empty());
    assert!(events(&db, "conflicts_config_changed").is_empty());

    // Three conflicts make it a hotspot: the next task over it waits.
    conflicts("hotspot_conflicts = 3\ndefer_max_secs = 7200\n");
    wait_until(&db, limit, |_| {
        events(&db, "conflicts_config_changed").len() == 1
    });
    let later = add("edits the docs later", &["docs/**"]);
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
    let last = add("edits the docs last", &["docs/**"]);
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
