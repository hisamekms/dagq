//! Runtime tests: a supervisor's `parallel` and `max_waiting` from its
//! flags, else `[supervisor]` of the main checkout's `dagq.toml`, else 4
//! (task 698), and its `runtime_planners` likewise, else 1 (task 941),
//! read again each pass, and shown with where each comes from in `status`
//! and `doctor`.
use crate::runtime_support;

use dagq::domain::slot_limits::SettingSource;
use runtime_support::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// Write `dagq.toml` whole by a rename: the supervisor reads it every pass.
fn write_config(repo: &Path, text: &str) {
    let staged = repo.join("dagq.toml.new");
    fs::write(&staged, text).unwrap();
    fs::rename(&staged, repo.join("dagq.toml")).unwrap();
}

fn events(db: &Path, kind: &str) -> Vec<Value> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == kind)
        .map(|event| event.payload)
        .collect()
}

/// The one registration's `(parallel, its source, max_waiting, its
/// source)`, once it has one.
fn registered(db: &Path) -> (u32, &'static str, Option<u32>, &'static str) {
    let registration = SqliteQueue::open(db)
        .unwrap()
        .supervisors()
        .unwrap()
        .remove(0);
    let source = |source: Option<SettingSource>| source.map_or("none", SettingSource::as_str);
    (
        registration.parallel,
        source(registration.parallel_source),
        registration.max_waiting,
        source(registration.max_waiting_source),
    )
}

/// Supervise in the background with `parallel` and `max_waiting` as the
/// flags; the stop switch and the thread.
fn start(
    db: &Path,
    repo: &Path,
    parallel: Option<usize>,
    max_waiting: Option<usize>,
) -> (
    Arc<AtomicBool>,
    Arc<TestWorkspace>,
    thread::JoinHandle<Result<Value>>,
) {
    let backend = Arc::new(TestWorkspace::new(db, false, VALID_AGENT));
    let stop = Arc::new(AtomicBool::new(false));
    let options = SuperviseOptions {
        stop: stop.clone(),
        parallel,
        max_waiting,
        ..supervise_options(4, false)
    };
    let supervisor = {
        let (db, repo, backend) = (db.to_path_buf(), repo.to_path_buf(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    wait_until(db, Duration::from_secs(30), |queue| {
        queue.supervisors().unwrap().len() == 1
    });
    (stop, backend, supervisor)
}

fn finish(
    stop: &AtomicBool,
    backend: &TestWorkspace,
    supervisor: thread::JoinHandle<Result<Value>>,
) {
    stop.store(true, Ordering::SeqCst);
    let outcome = joined(supervisor, "the supervisor to drain").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
}

/// Without flags, `[supervisor]` sets both values, `status` and `doctor`
/// say they come from `dagq.toml`, and a claim uses its `parallel`. A
/// changed table takes effect without a restart and is recorded once; an
/// invalid one keeps the values in use; a table taken out goes back to
/// the defaults.
#[test]
fn the_supervisor_table_sets_parallel_and_max_waiting_and_is_read_again() {
    let (_dir, repo, db) = fixture();
    write_config(&repo, "[supervisor]\nparallel = 1\nmax_waiting = 0\n");
    let (stop, backend, supervisor) = start(&db, &repo, None, None);
    assert_eq!(registered(&db), (1, "dagq.toml", Some(0), "dagq.toml"));
    for report in [
        runtime::status(&db).unwrap(),
        runtime::doctor(&db, false).unwrap(),
    ] {
        let entry = &report["supervisors"][0];
        assert_eq!(entry["parallel"], 1, "{entry}");
        assert_eq!(entry["parallel_source"], "dagq.toml", "{entry}");
        assert_eq!(entry["max_waiting"], 0, "{entry}");
        assert_eq!(entry["max_waiting_source"], "dagq.toml", "{entry}");
    }
    let status = runtime::status(&db).unwrap();
    let entry = &status["supervisors"][0];
    assert_eq!(entry["slots"]["parallel"], 1, "{entry}");
    assert_eq!(entry["slots"]["source"], "dagq.toml", "{entry}");
    assert_eq!(entry["waiting"]["limit"], 0, "{entry}");
    assert_eq!(entry["waiting"]["source"], "dagq.toml", "{entry}");
    // A claim is made under the table's `parallel`.
    let task = add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "first", &[]);
    wait_until(&db, Duration::from_secs(60), |queue| {
        !queue.show(task).unwrap().runs.is_empty()
    });
    let claimed = events(&db, "run_claimed");
    assert_eq!(claimed[0]["parallel"], 1, "{claimed:?}");
    assert!(events(&db, "supervisor_config_changed").is_empty());

    // Changed: in use from the next pass, on the registration, recorded.
    write_config(&repo, "[supervisor]\nparallel = 2\nmax_waiting = 3\n");
    wait_until(&db, Duration::from_secs(30), |_| {
        events(&db, "supervisor_config_changed").len() == 1
    });
    assert_eq!(registered(&db), (2, "dagq.toml", Some(3), "dagq.toml"));
    let changed = &events(&db, "supervisor_config_changed")[0];
    assert_eq!(changed["from"]["parallel"], 1, "{changed}");
    assert_eq!(changed["to"]["parallel"], 2, "{changed}");
    assert_eq!(changed["from"]["max_waiting"], 0, "{changed}");
    assert_eq!(changed["to"]["max_waiting"], 3, "{changed}");
    assert!(changed["supervisor"].is_string(), "{changed}");

    // Invalid: the values in use stay, nothing is recorded.
    write_config(&repo, "[supervisor]\nparallel = 0\n");
    thread::sleep(TEST_TICK * 10);
    assert_eq!(registered(&db), (2, "dagq.toml", Some(3), "dagq.toml"));
    assert_eq!(events(&db, "supervisor_config_changed").len(), 1);

    // Taken out: the defaults.
    write_config(&repo, "[stall]\n");
    wait_until(&db, Duration::from_secs(30), |_| {
        events(&db, "supervisor_config_changed").len() == 2
    });
    assert_eq!(registered(&db), (4, "default", Some(4), "default"));
    finish(&stop, &backend, supervisor);
}

/// A flag wins over the table and is never read again for; the value the
/// flags leave follows the table. With neither, both are 4.
#[test]
fn a_flag_wins_over_the_table_and_the_default_is_four() {
    let (_dir, repo, db) = fixture();
    write_config(&repo, "[supervisor]\nparallel = 1\nmax_waiting = 2\n");
    let (stop, backend, supervisor) = start(&db, &repo, Some(3), None);
    assert_eq!(registered(&db), (3, "flag", Some(2), "dagq.toml"));
    write_config(&repo, "[supervisor]\nparallel = 2\nmax_waiting = 2\n");
    thread::sleep(TEST_TICK * 10);
    assert_eq!(registered(&db), (3, "flag", Some(2), "dagq.toml"));
    assert!(events(&db, "supervisor_config_changed").is_empty());
    finish(&stop, &backend, supervisor);

    // Both flags: the table is not used.
    let (stop, backend, supervisor) = start(&db, &repo, Some(2), Some(1));
    assert_eq!(registered(&db), (2, "flag", Some(1), "flag"));
    let doctor = runtime::doctor(&db, false).unwrap();
    assert_eq!(doctor["supervisors"][0]["parallel_source"], "flag");
    assert_eq!(doctor["supervisors"][0]["max_waiting_source"], "flag");
    finish(&stop, &backend, supervisor);

    // Neither flag nor file.
    fs::remove_file(repo.join("dagq.toml")).unwrap();
    let (stop, backend, supervisor) = start(&db, &repo, None, None);
    assert_eq!(registered(&db), (4, "default", Some(4), "default"));
    finish(&stop, &backend, supervisor);
}

/// The one registration's `(runtime_planners, its source)`.
fn registered_planners(db: &Path) -> (Option<u32>, &'static str) {
    let registration = SqliteQueue::open(db)
        .unwrap()
        .supervisors()
        .unwrap()
        .remove(0);
    (
        registration.runtime_planners,
        registration
            .runtime_planners_source
            .map_or("none", SettingSource::as_str),
    )
}

/// Without the flag, `[supervisor] runtime_planners` sets the limit on
/// the runtime's planners, shown in `status` and `doctor` with its source;
/// a change takes effect without a restart and is recorded; an invalid
/// value keeps the one in use; a table taken out goes back to 1 (task
/// 941).
#[test]
fn the_supervisor_table_sets_runtime_planners_and_is_read_again() {
    let (_dir, repo, db) = fixture();
    write_config(&repo, "[supervisor]\nruntime_planners = 2\n");
    let (stop, backend, supervisor) = start(&db, &repo, Some(1), Some(0));
    assert_eq!(registered_planners(&db), (Some(2), "dagq.toml"));
    for report in [
        runtime::status(&db).unwrap(),
        runtime::doctor(&db, false).unwrap(),
    ] {
        let entry = &report["supervisors"][0];
        assert_eq!(entry["runtime_planners"], 2, "{entry}");
        assert_eq!(entry["runtime_planners_source"], "dagq.toml", "{entry}");
    }

    write_config(&repo, "[supervisor]\nruntime_planners = 3\n");
    wait_until(&db, Duration::from_secs(30), |_| {
        events(&db, "supervisor_config_changed").len() == 1
    });
    assert_eq!(registered_planners(&db), (Some(3), "dagq.toml"));
    let changed = &events(&db, "supervisor_config_changed")[0];
    assert_eq!(changed["from"]["runtime_planners"], 2, "{changed}");
    assert_eq!(changed["to"]["runtime_planners"], 3, "{changed}");
    assert_eq!(
        changed["to"]["runtime_planners_source"], "dagq.toml",
        "{changed}"
    );
    // The flags are not read again for.
    assert_eq!(registered(&db), (1, "flag", Some(0), "flag"));

    write_config(&repo, "[supervisor]\nruntime_planners = 0\n");
    thread::sleep(TEST_TICK * 10);
    assert_eq!(registered_planners(&db), (Some(3), "dagq.toml"));
    assert_eq!(events(&db, "supervisor_config_changed").len(), 1);

    write_config(&repo, "[stall]\n");
    wait_until(&db, Duration::from_secs(30), |_| {
        events(&db, "supervisor_config_changed").len() == 2
    });
    assert_eq!(registered_planners(&db), (Some(1), "default"));
    finish(&stop, &backend, supervisor);
}
