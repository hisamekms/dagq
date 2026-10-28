//! Only the queue's binary changes its schema (ADR-t3102-2 decisions 2 and
//! 3): a development build and a run's `runner` copy refuse `migrate`,
//! `up` and the writes before anything changes, and `install` lets the build it puts in place
//! migrate. Each test makes its own throwaway queue and the two binaries:
//! the queue's (a copy of the test's `dagq` that `init` names) and another.

use crate::common;
use common::lifecycle::*;
use common::{Bounded, WithoutActor};

use common::service::OwnedByTest;
use dagq::application::queue_service::QueueServiceControl;
use dagq::infrastructure::{
    queue_binary::{self, GRANT_FILE, QueueBinary},
    queue_service::{self as service, SystemQueueService},
    sqlite::SqliteQueue,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn run(binary: &Path, db: &Path, args: &[&str]) -> Output {
    Command::new(binary)
        .without_actor_env()
        .arg("--db")
        .arg(db)
        .args(args)
        .bounded_output()
        .unwrap()
}

fn error_of(output: &Output) -> String {
    assert!(!output.status.success(), "succeeded");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn user_version(db: &Path) -> i64 {
    rusqlite::Connection::open(db)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

/// A copy of the test's `dagq` at `path`.
fn copy_of_dagq(path: &Path) -> PathBuf {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_dagq"), path).unwrap();
    path.canonicalize().unwrap()
}

/// Replace the queue at `db` with one at schema `version`.
fn old_queue(db: &Path, version: usize) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", db.display()));
    }
    let raw = rusqlite::Connection::open(db).unwrap();
    for migration in &dagq::infrastructure::schema::MIGRATIONS[..version] {
        raw.execute_batch(migration).unwrap();
    }
    raw.execute_batch(&format!(
        "PRAGMA application_id = 1129599281; PRAGMA user_version = {version};"
    ))
    .unwrap();
}

#[test]
fn only_the_queues_binary_migrates_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let fixed = copy_of_dagq(&dir.path().join("bin").join("dagq"));
    let development = Path::new(env!("CARGO_BIN_EXE_dagq"));
    let db = dir.path().join("queue").join("queue.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();

    // `init` names the binary that created the queue.
    let output = run(&fixed, &db, &["init"]);
    assert!(output.status.success(), "{}", error_of(&output));
    assert_eq!(
        queue_binary::read(&db).unwrap(),
        Some(QueueBinary::new(&fixed))
    );
    // `init` of the queue there names no other binary.
    let output = run(development, &db, &["init"]);
    assert!(output.status.success(), "{}", error_of(&output));
    assert_eq!(queue_binary::read(&db).unwrap().unwrap().path, fixed);

    // Schema 23, which this binary migrates with breaking and compatible
    // migrations.
    old_queue(&db, 23);
    // A development build and a run's runner copy refuse to migrate, and
    // a write points at the queue's binary; the schema stays.
    let runner = copy_of_dagq(&dir.path().join("runs/run-1/runner"));
    for binary in [development, runner.as_path()] {
        let error = error_of(&run(binary, &db, &["migrate"]));
        assert!(error.contains("is not the queue's binary"), "{error}");
        assert!(error.contains(&fixed.display().to_string()), "{error}");
        // `integrate` sends its steps to the queue's service and opens no
        // DB, so with no service it neither migrates nor reads the queue.
        let error = error_of(&run(binary, &db, &["integrate", "1"]));
        assert!(error.contains("unreachable"), "{error}");
        assert_eq!(user_version(&db), 23);
        // Reading the schema is not refused.
        let check = run(binary, &db, &["migrate", "--check"]);
        assert!(check.status.success(), "{}", error_of(&check));
    }
    // The queue's binary migrates it.
    let output = run(&fixed, &db, &["migrate"]);
    assert!(output.status.success(), "{}", error_of(&output));
    assert_eq!(user_version(&db), SqliteQueue::SCHEMA_VERSION);
    // With nothing pending, another binary's `migrate` changes nothing and
    // is not refused.
    let output = run(development, &db, &["migrate"]);
    assert!(output.status.success(), "{}", error_of(&output));
}

/// `install` has the queue migrated by the service of the build it puts in
/// place before it is there, by a grant for that start alone, and then
/// names it (at the path it replaced) the queue's binary.
#[test]
fn install_lets_its_new_build_migrate_and_names_it_the_queues_binary() {
    let dir = tempfile::tempdir().unwrap();
    let fixed = copy_of_dagq(&dir.path().join("bin").join("dagq"));
    let next = copy_of_dagq(&dir.path().join("update/target/release/dagq"));
    let db = dir.path().join("queue").join("queue.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    // The queue at the last breaking migration, so that what is pending
    // is compatible, as an automatic update's install finds it.
    let migrations = dagq::infrastructure::schema::MIGRATIONS;
    let last_breaking = (1..=migrations.len())
        .rev()
        .find(|&version| !dagq::infrastructure::schema::is_compatible(migrations[version - 1]))
        .unwrap();
    old_queue(&db, last_breaking);
    let pending = last_breaking < migrations.len();
    queue_binary::write(&db, &QueueBinary::new(&fixed)).unwrap();
    // The new build is refused on its own.
    if pending {
        let error = error_of(&run(&next, &db, &["migrate"]));
        assert!(error.contains("is not the queue's binary"), "{error}");
    }
    // The install names the path it puts the binary at. The services it
    // starts stop with this test.
    let installed = copy_of_dagq(&dir.path().join("installed").join("dagq"));
    let _service = StopService(db.clone());
    let output = Command::new(&fixed)
        .without_actor_env()
        .owned_by_test()
        .arg("--db")
        .arg(&db)
        .args([
            "install",
            "--from",
            next.to_str().unwrap(),
            "--to",
            installed.to_str().unwrap(),
        ])
        .bounded_output()
        .unwrap();
    assert!(output.status.success(), "{}", error_of(&output));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // Nothing is pending when the last migration is a breaking one.
    if pending {
        assert_eq!(
            report["migrated"]["schema_version"],
            SqliteQueue::SCHEMA_VERSION,
            "{report}"
        );
    }
    assert_eq!(user_version(&db), SqliteQueue::SCHEMA_VERSION);
    assert!(!db.with_file_name(GRANT_FILE).exists());
    // The new build's service applied them before it accepted, and the
    // service of the binary in place took over after.
    if pending {
        let log = fs::read_to_string(service::log_path(db.parent().unwrap())).unwrap();
        assert!(log.contains("applied the migrations of its build"), "{log}");
    }
    assert_eq!(
        queue_binary::read(&db).unwrap(),
        Some(QueueBinary::new(&installed))
    );
}

/// `up` of a binary that is not the queue's applies no migration and stops
/// before it starts anything: the service it starts refuses before it takes
/// over, and accepts nothing.
#[test]
fn up_of_another_binary_applies_no_migration() {
    let mut fixture = fixture();
    let _service = serve_on_up(&mut fixture);
    let db = &fixture.location.db;
    queue_binary::write(db, &QueueBinary::new(Path::new("/elsewhere/dagq"))).unwrap();
    let pending = SqliteQueue::SCHEMA_VERSION - 1;
    rusqlite::Connection::open(db)
        .unwrap()
        .execute_batch(&format!("PRAGMA user_version = {pending};"))
        .unwrap();
    let launchd = FakeLaunchd::new(db);
    let processes = FakeProcesses::default();
    let error = format!("{:#}", try_up(&fixture, &launchd, &processes).unwrap_err());
    assert!(error.contains("is not the queue's binary"), "{error}");
    assert_eq!(user_version(db), pending);
    assert!(launchd.installs.lock().unwrap().is_empty());
    assert_eq!(
        dagq::infrastructure::queue_service::probe(db.parent().unwrap()).state,
        dagq::domain::queue_service::ServiceState::Stopped
    );
}

/// The version of this binary's last breaking migration.
fn last_breaking() -> usize {
    let migrations = dagq::infrastructure::schema::MIGRATIONS;
    (1..=migrations.len())
        .rev()
        .find(|&version| !dagq::infrastructure::schema::is_compatible(migrations[version - 1]))
        .unwrap()
}

/// A queue at schema `version`, in a directory this test owns, with the
/// control of its service started from the tests' binary, stopped with the
/// test.
fn throwaway_queue(
    version: usize,
) -> (
    tempfile::TempDir,
    fs::File,
    PathBuf,
    SystemQueueService,
    StopService,
) {
    let (dir, owner) = common::cleanup::owned_temp_dir();
    let db = dir.path().join("queue").join("queue.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    old_queue(&db, version);
    let control = SystemQueueService::new(
        &db,
        &common::service::owned_executable(db.parent().unwrap()),
    );
    let service = StopService(db.clone());
    (dir, owner, db, control, service)
}

/// [`throwaway_queue`] before its last breaking migration, and that
/// migration's version.
fn queue_before_the_last_breaking() -> (
    tempfile::TempDir,
    fs::File,
    PathBuf,
    i64,
    SystemQueueService,
    StopService,
) {
    let last_breaking = last_breaking();
    let (dir, owner, db, control, service) = throwaway_queue(last_breaking - 1);
    (dir, owner, db, last_breaking as i64, control, service)
}

/// [`QueueServiceControl::replace`] bounded by the test's wait: once the
/// service takes over it is waited for as long as it lives.
fn replace(
    control: &SystemQueueService,
    breaking: bool,
    reuse: bool,
) -> anyhow::Result<dagq::application::queue_service::Replaced> {
    let timeout = std::time::Duration::from_secs(30);
    let _waiting = common::within(timeout * 2, "the replacement's service to start");
    control.replace(breaking, reuse, timeout)
}

fn stopped(db: &Path) {
    assert_eq!(
        service::probe(db.parent().unwrap()).state,
        dagq::domain::queue_service::ServiceState::Stopped
    );
}

/// After the drain of an `install --allow-breaking`, the new build's
/// service applies a breaking migration only on a queue nothing uses: with
/// a live supervisor registered it applies nothing, copies nothing and
/// accepts nothing, and the service `up` starts refuses a breaking one
/// outright. A supervisor that registers on another connection while the
/// service looks, before its migration's transaction, is found by the
/// check in that transaction, before the copy (ADR-t3102-2 decision 2).
#[test]
fn a_replacement_s_service_applies_a_breaking_migration_only_on_an_unused_queue() {
    let _test = common::test();
    let (_dir, _owner, db, last_breaking, control, _service) = queue_before_the_last_breaking();
    let before = last_breaking - 1;
    let raw = rusqlite::Connection::open(&db).unwrap();
    raw.execute(
        "INSERT INTO supervisors(token, pid, parallel) VALUES ('live', ?1, 1)",
        [std::process::id()],
    )
    .unwrap();
    let in_use = format!("breaking migration(s) {last_breaking} while the queue is in use by");

    let error = format!("{:#}", replace(&control, true, false).unwrap_err());
    assert!(
        error.contains(&format!("{in_use} supervisor live")),
        "{error}"
    );
    assert_eq!(user_version(&db), before);
    assert!(!db.with_file_name("backups").exists());
    stopped(&db);
    // Not after a drain: refused as breaking, whoever uses the queue.
    let error = format!("{:#}", replace(&control, false, true).unwrap_err());
    assert!(error.contains("install --allow-breaking"), "{error}");
    assert_eq!(user_version(&db), before);
    stopped(&db);

    // A supervisor registering while the service takes over: the service
    // found the queue unused before, and its transaction waits for the
    // registration's, then finds it.
    raw.execute("DELETE FROM supervisors", []).unwrap();
    raw.execute_batch("BEGIN IMMEDIATE").unwrap();
    raw.execute(
        "INSERT INTO supervisors(token, pid, parallel) VALUES ('late', ?1, 1)",
        [std::process::id()],
    )
    .unwrap();
    let error = std::thread::scope(|scope| {
        let replacing = scope.spawn(|| replace(&control, true, false));
        let service_dir = db
            .parent()
            .unwrap()
            .join(dagq::domain::queue_service::SERVICE_DIR);
        let _waiting = common::within(std::time::Duration::from_secs(30), "the take-over");
        while !fs::read_dir(&service_dir).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("replacement-")
                    && fs::read_to_string(entry.path())
                        .is_ok_and(|text| text.contains("taking_over"))
            })
        }) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        raw.execute_batch("COMMIT").unwrap();
        format!("{:#}", replacing.join().unwrap().unwrap_err())
    });
    assert!(
        error.contains(&format!("{in_use} supervisor late")),
        "{error}"
    );
    assert_eq!(user_version(&db), before);
    assert!(!db.with_file_name("backups").exists());
    stopped(&db);

    // The supervisor gone: copied, then migrated.
    raw.execute("DELETE FROM supervisors", []).unwrap();
    let replaced = replace(&control, true, false).unwrap();
    let migrated = replaced.migrated.unwrap();
    assert_eq!(
        migrated["schema_version"],
        SqliteQueue::SCHEMA_VERSION,
        "{migrated}"
    );
    let backup = PathBuf::from(migrated["backup"].as_str().unwrap());
    assert_eq!(user_version(&backup), before);
    assert_eq!(user_version(&db), SqliteQueue::SCHEMA_VERSION);
    assert!(replaced.service.current(), "{:?}", replaced.service);
}

/// A breaking migration that fails is rolled back whole: the queue keeps
/// its schema and rows, and the service accepts nothing.
#[test]
fn a_replacement_s_failed_breaking_migration_leaves_the_queue_as_it_was() {
    let _test = common::test();
    let (_dir, _owner, db, last_breaking, control, _service) = queue_before_the_last_breaking();
    let before = last_breaking - 1;
    // A dependency on tasks that are not there: the migrations' check of
    // the foreign keys before the commit fails.
    let raw = rusqlite::Connection::open(&db).unwrap();
    raw.execute_batch(
        "PRAGMA foreign_keys = OFF;
         INSERT INTO task_dependencies(task_id, predecessor_id) VALUES (998, 999);",
    )
    .unwrap();
    let error = format!("{:#}", replace(&control, true, false).unwrap_err());
    assert!(error.contains("foreign key"), "{error}");
    assert_eq!(user_version(&db), before);
    let kept: i64 = raw
        .query_row("SELECT count(*) FROM task_dependencies", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(kept, 1);
    stopped(&db);
}

/// The report the replacement's service in `db`'s service directory tells
/// now, if any.
fn replacement_state(db: &Path) -> Option<String> {
    let dir = db
        .parent()
        .unwrap()
        .join(dagq::domain::queue_service::SERVICE_DIR);
    fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
        let name = entry.file_name().to_string_lossy().into_owned();
        (name.starts_with("replacement-") && name.ends_with(".json"))
            .then(|| fs::read_to_string(entry.path()).ok())
            .flatten()
    })
}

/// A supervisor of a build before the hold knows none: its watch replaces
/// the service a replacement started, of another build to it, as soon as it
/// answers, and a service it starts may take the queue's lock while the
/// replacement's service waits to take over. The replacement goes through
/// all the same: what it started applied the compatible migrations before
/// it accepted, and a service in its way while it takes over is stopped
/// (ADR-t3102-2 decisions 2 and 7). The watch stands in here: it starts a
/// service of the tests' binary, which a look tells from the replacement's
/// by its pid.
#[test]
fn a_replacement_goes_through_the_watch_of_a_supervisor_before_the_hold() {
    let _test = common::test();
    let last_breaking = last_breaking();
    if last_breaking == dagq::infrastructure::schema::MIGRATIONS.len() {
        return;
    }
    let (_dir, _owner, db, control, _service) = throwaway_queue(last_breaking);
    // Killed when dropped, the test passed or panicked, and by its pid when
    // a wait times out.
    let serve = || {
        let child = Command::new(env!("CARGO_BIN_EXE_dagq"))
            .without_actor_env()
            .owned_by_test()
            .arg("--db")
            .arg(&db)
            .args(["service", "serve"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        common::KillOnDrop::new(child, "a queue service the test started")
    };
    // Each kills and reaps what it holds within the step's limit.
    let killed = |children: &mut Vec<common::KillOnDrop>| {
        let _waiting = common::within(common::STEP_LIMIT, "the services the test started to end");
        children.clear();
    };
    // The service running before: the one the replacement takes over from.
    let mut started = vec![serve()];
    {
        let _waiting = common::within(std::time::Duration::from_secs(10), "the service to answer");
        while service::probe(db.parent().unwrap()).state
            != dagq::domain::queue_service::ServiceState::Running
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    let done = std::sync::atomic::AtomicBool::new(false);
    let (replaced, watched) = std::thread::scope(|scope| {
        let watch = scope.spawn(|| {
            let (mut intruded, mut replaced) = (false, false);
            let mut own = Vec::new();
            while !done.load(std::sync::atomic::Ordering::SeqCst) {
                let state = replacement_state(&db).unwrap_or_default();
                if state.contains("taking_over") && !intruded {
                    // Started again while the one before stops.
                    intruded = true;
                    own.push(serve());
                } else if state.contains("accepting") && !replaced {
                    // Of another build to it: replaced at once. The one it
                    // started while the replacement took over is gone or
                    // still waits for the lock; it goes first, so that it
                    // does not take the lock this stop lets go.
                    let found = service::probe(db.parent().unwrap());
                    if found.state == dagq::domain::queue_service::ServiceState::Running {
                        replaced = true;
                        killed(&mut own);
                        let _ = control.stop(std::time::Duration::from_secs(10));
                        own.push(serve());
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            (own, intruded || replaced)
        });
        let replaced = replace(&control, false, true);
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        let _waiting = common::within(common::STEP_LIMIT, "the stand-in watch to end");
        (replaced, watch.join().unwrap())
    });
    let (own, _) = watched;
    started.extend(own);
    let replaced = replaced.unwrap();
    assert_eq!(
        replaced.migrated.as_ref().unwrap()["schema_version"],
        SqliteQueue::SCHEMA_VERSION,
        "{replaced:?}"
    );
    assert_eq!(user_version(&db), SqliteQueue::SCHEMA_VERSION);
    killed(&mut started);
    let _ = control.stop(std::time::Duration::from_secs(10));
}
