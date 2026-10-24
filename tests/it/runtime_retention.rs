//! Runtime tests: the sweep's removal of what ended planners, plan reviews
//! and observations left past their retention, and of the old backups
//! (ADR-t697-1).
use crate::runtime_support;
use dagq::{
    application::{
        planner::{PlanningDirs, remove_expired_dirs},
        retention::{RetentionDirs, remove_expired},
    },
    domain::{PlannerId, PlannerOrigin, retention::RetentionConfig},
    infrastructure::{
        adapters::SystemProcesses,
        clock::SystemClock,
        location::{plan_reviews_dir, planners_dir},
        observer::observer_dir,
        run_files::LocalRunFiles,
    },
};
use std::time::{SystemTime, UNIX_EPOCH};

use runtime_support::*;

const DAY: i64 = 86_400;

fn now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

/// Set the time `path` was last written to `at` (UNIX seconds).
fn written_at(path: &Path, at: i64) {
    let at = UNIX_EPOCH + Duration::from_secs(u64::try_from(at).unwrap());
    fs::File::open(path).unwrap().set_modified(at).unwrap();
}

/// A pid no process runs.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    {
        let _waiting = crate::common::within(crate::common::STEP_LIMIT, "`true` to exit");
        child.wait().unwrap();
    }
    pid
}

/// A planner of the runtime's in a workspace `workspace`, its wrapper the
/// process `pid` and its agent `agent`, and a file in its directory;
/// closed at `closed_at` with its exit recorded when given one.
fn planner(
    db: &Path,
    workspace: &str,
    (pid, agent): (u32, u32),
    closed_at: Option<i64>,
) -> (PlannerId, PathBuf) {
    let queue = SqliteQueue::open(db).unwrap();
    let planner = queue.open_planner(PlannerOrigin::Runtime, None).unwrap();
    queue
        .planner_workspace_created(planner.id, workspace)
        .unwrap();
    queue.register_planner_wrapper(planner.id, pid).unwrap();
    queue
        .register_planner_agent(planner.id, pid, agent)
        .unwrap();
    if let Some(closed_at) = closed_at {
        queue.planner_exited(planner.id, pid, 0).unwrap();
        queue.close_planner(planner.id, None).unwrap();
        Connection::open(db)
            .unwrap()
            .execute(
                "UPDATE planners SET closed_at=?2 WHERE id=?1",
                [planner.id.as_i64(), closed_at],
            )
            .unwrap();
    }
    let dir = planners_dir(db).join(planner.id.to_string());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("prompt.txt"), "prompt").unwrap();
    (planner.id, dir)
}

/// A plan review row finished at `finished_at` (`None`: still running)
/// and its directory with a file.
fn plan_review(db: &Path, finished_at: Option<i64>) -> PathBuf {
    let sql = Connection::open(db).unwrap();
    sql.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    sql.execute(
        "INSERT INTO plan_reviews(proposal_id, attempt, supervisor_token, started_at, finished_at, outcome)
         VALUES (1, 1, 'gone', ?1, ?2, CASE WHEN ?2 IS NULL THEN NULL ELSE 'succeeded' END)",
        rusqlite::params![finished_at.unwrap_or(now()) - 60, finished_at],
    )
    .unwrap();
    let dir = plan_reviews_dir(db).join(sql.last_insert_rowid().to_string());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("prompt.txt"), "prompt").unwrap();
    dir
}

/// An observation's directory under `observer/`, its file and itself
/// last written at `at`.
fn observation(db: &Path, at: i64) -> PathBuf {
    let dir = observer_dir(db).join(at.to_string());
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("input.json");
    fs::write(&file, "{}").unwrap();
    written_at(&file, at);
    written_at(&dir, at);
    dir
}

/// Record the start of the observation in `dir` by the process `pid`,
/// with a two days' timeout, and its finish when `finished`.
fn record_observation(db: &Path, dir: &Path, pid: u32, finished: bool) {
    use dagq::domain::EventKind;
    let queue = SqliteQueue::open(db).unwrap();
    let payload = json!({"mode": "hourly", "dir": dir, "pid": pid, "timeout_secs": 172_800});
    queue
        .record_queue_event(EventKind::ObserveStarted, payload)
        .unwrap();
    if finished {
        queue
            .record_queue_event(
                EventKind::ObserveFinished,
                json!({"outcome": "ok", "dir": dir}),
            )
            .unwrap();
    }
}

/// An observation's directory last written at `at` whose start and finish
/// are recorded, by a process that is gone.
fn ended_observation(db: &Path, at: i64) -> PathBuf {
    let dir = observation(db, at);
    record_observation(db, &dir, dead_pid(), true);
    dir
}

/// The copies a breaking `migrate` made, by name, and a file of another
/// name, in `backups/` beside the queue.
fn backups(db: &Path, names: &[&str]) -> PathBuf {
    let dir = db.parent().unwrap().join("backups");
    fs::create_dir_all(&dir).unwrap();
    for name in names.iter().chain(&["notes.txt"]) {
        fs::write(dir.join(name), "copy").unwrap();
    }
    dir
}

/// Goal 54 (4) with the defaults (7 days, 2 backups): the supervisor's
/// sweep removes the directory of a planner closed 8 days ago none of
/// whose processes runs, of a plan review finished 8 days ago and of an
/// observation finished and last written 8 days ago, and the oldest
/// backup past the newest two. A planner closed 8 days ago with its exit
/// recorded and its heartbeat stale but its wrapper's process still
/// running (its runner may go, its directory not), a planner closed a day
/// ago, one still open, a plan review finished a day ago or still running,
/// an observation of a day ago, one of 8 days ago no record names, the
/// observer's cursor and a file in `backups/` of another name stay.
#[test]
fn the_sweep_removes_the_directories_past_their_retention_and_keeps_two_backups() {
    let (_dir, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let now = now();
    let dead = dead_pid();
    let alive = std::process::id();
    let (_, old_planner) = planner(&db, "W-OLD", (dead, dead), Some(now - 8 * DAY));
    let (running_id, running_planner) =
        planner(&db, "W-RUNNING", (alive, dead), Some(now - 8 * DAY));
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE planners SET heartbeat_at=1 WHERE id=?1",
            [running_id.as_i64()],
        )
        .unwrap();
    let (_, recent_planner) = planner(&db, "W-RECENT", (dead, dead), Some(now - DAY));
    let (_, open_planner) = planner(&db, "W-OPEN", (alive, alive), None);
    let old_review = plan_review(&db, Some(now - 8 * DAY));
    let recent_review = plan_review(&db, Some(now - DAY));
    let running_review = plan_review(&db, None);
    let old_observation = ended_observation(&db, now - 8 * DAY);
    // No record names it: kept however old (ADR-t697-1 decision 2).
    let unrecorded_observation = observation(&db, now - 8 * DAY - 1);
    let recent_observation = observation(&db, now - DAY);
    let cursor = observer_dir(&db).join("cursor");
    fs::write(&cursor, "12\n").unwrap();
    written_at(&cursor, now - 30 * DAY);
    let backups = backups(
        &db,
        &[
            "queue-40-100.sqlite3",
            "queue-41-200.sqlite3",
            "queue-41-200-1.sqlite3",
        ],
    );

    let backend = TestWorkspace::new(&db, false, "exit 0");
    backend.listed.lock().unwrap().push("w-open".into());
    // The supervisor's services stop with this test (common::service).
    let dagq = crate::common::service::owned_executable(db.parent().unwrap());
    supervise_retrying_by(&db, &repo, &backend, &supervise_options_sweeping(), &dagq).unwrap();

    for gone in [&old_planner, &old_review, &old_observation] {
        assert!(!gone.exists(), "{}", gone.display());
    }
    for kept in [
        &running_planner,
        &recent_planner,
        &open_planner,
        &recent_review,
        &running_review,
        &recent_observation,
        &unrecorded_observation,
    ] {
        assert!(kept.is_dir(), "{}", kept.display());
    }
    assert!(cursor.is_file());
    let mut left: Vec<String> = fs::read_dir(&backups)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(
        left,
        [
            "notes.txt",
            "queue-41-200-1.sqlite3",
            "queue-41-200.sqlite3"
        ]
    );
    // The planners' rows stay.
    assert_eq!(
        SqliteQueue::open(&db)
            .unwrap()
            .planners(true)
            .unwrap()
            .len(),
        4
    );
}

/// Supervisor options that sweep on every pass.
fn supervise_options_sweeping() -> SuperviseOptions {
    SuperviseOptions {
        sweep_interval: Duration::ZERO,
        retry_unreadable_review: false,
        ..supervise_options(4, true)
    }
}

/// Both parts of the retention, as the sweep runs them: 計画管理's
/// directories of its planners and plan reviews, then host運用's of the
/// observations and the backups. What went, sorted, and each part's
/// failure.
fn remove_all(db: &Path, config: &RetentionConfig) -> (Vec<PathBuf>, Vec<String>) {
    let queue = SqliteQueue::open(db).unwrap();
    let planning = remove_expired_dirs(
        &queue.planners(true).unwrap(),
        &queue,
        &SystemProcesses,
        &LocalRunFiles,
        now(),
        PlanningDirs {
            planners: &planners_dir(db),
            plan_reviews: &plan_reviews_dir(db),
        },
        config,
    );
    let host = remove_expired(
        &queue,
        &SystemProcesses,
        &LocalRunFiles,
        &SystemClock,
        &RetentionDirs::of(db, &LocalRunFiles),
        config,
    );
    let mut removed = planning.removed;
    removed.extend(host.removed);
    removed.sort();
    let failures = [planning.failure, host.failure]
        .into_iter()
        .flatten()
        .map(|error| format!("{error:#}"))
        .collect();
    (removed, failures)
}

/// `[retention]` sets the days and the copies: with a day and one copy, a
/// planner closed two days ago goes and the newest copy alone stays. A
/// closed planner whose background wrapper alone, or whose agent alone,
/// still runs keeps its directory however old its close, and a second
/// removal finds nothing more.
#[test]
fn the_retention_follows_its_days_and_backups_and_a_live_wrapper_keeps_its_planner() {
    let (_dir, _repo, db) = fixture();
    let now = now();
    let (dead, alive) = (dead_pid(), std::process::id());
    let (_, closed) = planner(&db, "W-CLOSED", (dead, dead), Some(now - 2 * DAY));
    // Its wrapper is gone but its agent still runs.
    let (_, agent) = planner(&db, "W-AGENT", (dead, alive), Some(now - 30 * DAY));
    // Its agent is gone; its wrapper, in the background, still runs.
    let (live_id, live) = planner(&db, "W-LIVE", (alive, dead), None);
    let queue = SqliteQueue::open(&db).unwrap();
    queue.close_planner(live_id, None).unwrap();
    // A background wrapper: this process, with the start it shows.
    let handle = dagq::domain::background_wrapper::BackgroundHandle::new(
        std::process::id(),
        &dagq::application::ProcessControl::start_identity(&SystemProcesses, std::process::id())
            .unwrap(),
    );
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE planners SET closed_at=?2, workspace_id=?3 WHERE id=?1",
            rusqlite::params![live_id.as_i64(), now - 30 * DAY, handle.to_string()],
        )
        .unwrap();
    let recent = ended_observation(&db, now - 2 * DAY);
    let backups = backups(&db, &["queue-40-100.sqlite3", "queue-41-200.sqlite3"]);
    let config = RetentionConfig {
        days: 1,
        backups: 1,
    };

    let (removed, failures) = remove_all(&db, &config);
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(removed.len(), 3, "{removed:?}");
    assert!(!closed.exists());
    assert!(!recent.exists());
    assert!(live.join("prompt.txt").is_file());
    assert!(agent.join("prompt.txt").is_file());
    assert!(!backups.join("queue-40-100.sqlite3").exists());
    assert!(backups.join("queue-41-200.sqlite3").is_file());
    let again = remove_all(&db, &config);
    assert!(again.0.is_empty() && again.1.is_empty(), "{again:?}");
}

/// A removal that fails does not hide the others: with `backups/` read-only
/// the old copy stays and its failure is returned with the observation's
/// directory the same call removed, and 計画管理's part removes the closed
/// planner's directory on its own.
#[test]
fn a_failed_removal_returns_with_what_the_same_sweep_removed() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, _repo, db) = fixture();
    let now = now();
    let dead = dead_pid();
    let (_, closed) = planner(&db, "W-CLOSED", (dead, dead), Some(now - 8 * DAY));
    let old = ended_observation(&db, now - 8 * DAY);
    let backups = backups(
        &db,
        &[
            "queue-40-100.sqlite3",
            "queue-41-200.sqlite3",
            "queue-41-300.sqlite3",
        ],
    );
    fs::set_permissions(&backups, fs::Permissions::from_mode(0o555)).unwrap();

    let (removed, failures) = remove_all(&db, &RetentionConfig::default());
    fs::set_permissions(&backups, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("queue-40-100.sqlite3"), "{failures:?}");
    assert!(backups.join("queue-40-100.sqlite3").is_file());
    let mut expected = vec![closed.clone(), old.clone()];
    expected.sort();
    assert_eq!(removed, expected);
    assert!(!closed.exists() && !old.exists());
}

/// An observation not over by the queue's records keeps its directory
/// however old its last write (ADR-t697-1 decision 2): with `[retention]`
/// of a day, one started with a two days' timeout (`observe --timeout
/// 172800`) whose process still runs and that recorded no finish stays
/// after two quiet days, and so does one no record names, while one
/// finished whose process is gone goes. The other cases of the records are
/// domain::retention's unit test.
#[test]
fn an_observation_not_over_by_its_records_keeps_its_directory_however_old() {
    let (_dir, _repo, db) = fixture();
    let now = now();
    let running = observation(&db, now - 2 * DAY);
    record_observation(&db, &running, std::process::id(), false);
    let unrecorded = observation(&db, now - 2 * DAY - 1);
    let over = ended_observation(&db, now - 2 * DAY - 3);
    let config = RetentionConfig {
        days: 1,
        ..RetentionConfig::default()
    };

    let swept = remove_expired(
        &SqliteQueue::open(&db).unwrap(),
        &SystemProcesses,
        &LocalRunFiles,
        &SystemClock,
        &RetentionDirs::of(&db, &LocalRunFiles),
        &config,
    );
    assert!(swept.failure.is_none(), "{:?}", swept.failure);
    assert_eq!(swept.removed, std::slice::from_ref(&over));
    assert!(running.join("input.json").is_file());
    assert!(unrecorded.join("input.json").is_file());
}

/// The retention reads the observations' records a range of events of
/// their kinds at a time (`events_of_between`, one indexed range per
/// kind): the events of the kinds asked for in the range come oldest first
/// and at most the limit, a kind named twice once, and those of other
/// kinds and outside the range are left out.
#[test]
fn the_events_of_kinds_in_a_range_come_oldest_first_up_to_the_limit() {
    use dagq::{application::EventStore, domain::EventKind};
    let (_dir, _repo, db) = fixture();
    let queue = SqliteQueue::open(&db).unwrap();
    let record = |kind: EventKind| queue.record_queue_event(kind, json!({})).unwrap();
    let before = record(EventKind::ObserveStarted);
    let a = record(EventKind::ObserveStarted);
    record(EventKind::CiWatchHeld);
    let b = record(EventKind::ObserveFinished);
    let c = record(EventKind::ObserveStarted);
    let upto = record(EventKind::ObserveFinished);
    record(EventKind::ObserveStarted);
    let kinds = [
        EventKind::ObserveStarted.as_str(),
        EventKind::ObserveFinished.as_str(),
        EventKind::ObserveStarted.as_str(),
    ];
    let ids = |limit| {
        EventStore::events_of_between(&queue, &kinds, before, upto, limit)
            .unwrap()
            .into_iter()
            .map(|event| event.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(10), [a, b, c, upto]);
    assert_eq!(ids(3), [a, b, c]);
}
