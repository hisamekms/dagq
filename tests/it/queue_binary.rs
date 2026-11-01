//! Only the queue's binary starts its service, which alone changes its
//! schema (ADR-t3102-2 decisions 1 to 3): a development build and a run's
//! `runner` copy refuse `service start`, `service serve` and `up` before
//! anything changes, and `install` lets the build it puts in place
//! migrate. Each test makes its own throwaway queue and the two binaries:
//! the queue's (a copy of the test's `dagq` that `init` names) and another.

use crate::common;
use common::lifecycle::*;
use common::{Bounded, WithoutActor};

use common::queue::user_version;
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
        .owned_by_test()
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

/// The queue's binary alone starts its service, which alone migrates it
/// (ADR-t3102-2 decisions 1 to 3): a development build and a run's runner
/// copy refuse `service start` and `service serve` naming the queue's
/// binary, and start nothing; with no service, `migrate`, `doctor` and
/// `init` of the queue fail as unreachable and `migrate --check` leaves the
/// schema to the replacement start, all opening no DB; with the queue's
/// binary's service, they are its use cases.
#[test]
fn only_the_queues_binary_migrates_the_queue() {
    let (dir, _owner) = common::cleanup::owned_temp_dir();
    let fixed = copy_of_dagq(&dir.path().join("bin").join("dagq"));
    let development = Path::new(env!("CARGO_BIN_EXE_dagq"));
    let db = dir.path().join("queue").join("queue.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let _service = StopService(db.clone());

    // `init` names the binary that created the queue.
    let output = run(&fixed, &db, &["init"]);
    assert!(output.status.success(), "{}", error_of(&output));
    assert_eq!(
        queue_binary::read(&db).unwrap(),
        Some(QueueBinary::new(&fixed))
    );

    // Schema 23, which this binary migrates with breaking and compatible
    // migrations.
    old_queue(&db, 23);
    let runner = copy_of_dagq(&dir.path().join("runs/run-1/runner"));
    for binary in [development, runner.as_path()] {
        // Not the queue's binary: no service of it starts, by hand or as
        // `service serve`.
        for args in [&["service", "start"][..], &["service", "serve"]] {
            let error = error_of(&run(binary, &db, args));
            assert!(error.contains("is not the queue's binary"), "{error}");
            assert!(error.contains(&fixed.display().to_string()), "{error}");
            stopped(&db);
        }
        // With no service, nothing is migrated, read or initialized here:
        // the DB is not opened instead.
        for args in [&["migrate"][..], &["doctor"], &["init"]] {
            let error = error_of(&run(binary, &db, args));
            assert!(error.contains("unreachable"), "{args:?}: {error}");
        }
        // Nor is it checked: the check leaves the schema to the
        // replacement start of its build's service.
        let output = run(binary, &db, &["migrate", "--check"]);
        assert!(output.status.success(), "{}", error_of(&output));
        let check: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(check["unanswered"], true, "{check}");
        let reason = check["reason"].as_str().unwrap();
        assert!(
            reason.contains(dagq::domain::queue_service::SCHEMA_UNANSWERED),
            "{reason}"
        );
        assert!(reason.contains("unreachable"), "{reason}");
        assert_eq!(user_version(&db), 23);
        assert_eq!(queue_binary::read(&db).unwrap().unwrap().path, fixed);
    }

    // The queue's binary's service answers the others' requests. It starts
    // on a queue its build would migrate, and its start is recorded by
    // none, as the queue does not open as it is.
    let error = error_of(&run(&fixed, &db, &["service", "start"]));
    assert!(error.contains("run `dagq migrate`"), "{error}");
    assert_eq!(
        service::probe(db.parent().unwrap()).state,
        dagq::domain::queue_service::ServiceState::Running
    );
    let check: serde_json::Value =
        serde_json::from_slice(&run(&runner, &db, &["migrate", "--check"]).stdout).unwrap();
    assert_eq!(check["schema_version"], 23, "{check}");
    assert_eq!(check["opens"], false, "{check}");
    let doctor: serde_json::Value =
        serde_json::from_slice(&run(development, &db, &["doctor"]).stdout).unwrap();
    assert_eq!(doctor["schema"], check, "{doctor}");
    let output = run(development, &db, &["migrate"]);
    assert!(output.status.success(), "{}", error_of(&output));
    assert_eq!(user_version(&db), SqliteQueue::SCHEMA_VERSION);
    // Its service applied them, not the command's process.
    let log = fs::read_to_string(service::log_path(db.parent().unwrap())).unwrap();
    assert!(log.contains("at a migrate's request"), "{log}");
    let output = run(development, &db, &["init"]);
    assert!(output.status.success(), "{}", error_of(&output));
    assert_eq!(queue_binary::read(&db).unwrap().unwrap().path, fixed);
}

/// A queue that names no binary is the first starter's: once a binary
/// started its service, another one's start is refused and starts nothing,
/// the running service left as it was. While its service runs, a queue
/// with no record is not taken over by a replacement no install grants.
#[test]
fn a_queue_with_no_record_is_named_by_the_binary_that_first_starts_its_service() {
    let (dir, _owner) = common::cleanup::owned_temp_dir();
    let first = copy_of_dagq(&dir.path().join("bin").join("dagq"));
    let other = copy_of_dagq(&dir.path().join("dev").join("dagq"));
    let db = dir.path().join("queue").join("queue.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let _service = StopService(db.clone());
    old_queue(&db, SqliteQueue::SCHEMA_VERSION as usize);
    assert_eq!(queue_binary::read(&db).unwrap(), None);
    let output = Command::new(&first)
        .without_actor_env()
        .owned_by_test()
        .arg("--db")
        .arg(&db)
        .args(["service", "start"])
        .bounded_output()
        .unwrap();
    assert!(output.status.success(), "{}", error_of(&output));
    assert_eq!(
        queue_binary::read(&db).unwrap(),
        Some(QueueBinary::new(&first))
    );
    let running = service::probe(db.parent().unwrap());
    // With the record gone and the service running, no start of another
    // build stops it to name itself: only an install's grant would.
    fs::remove_file(db.parent().unwrap().join(queue_binary::RECORD_FILE)).unwrap();
    let report = dir.path().join("replacement.json");
    let error = error_of(&run(
        &other,
        &db,
        &[
            "service",
            "serve",
            "--replacement",
            report.to_str().unwrap(),
        ],
    ));
    assert!(error.contains("names no binary yet"), "{error}");
    assert_eq!(service::probe(db.parent().unwrap()).pid, running.pid);
    assert_eq!(queue_binary::read(&db).unwrap(), None);
    queue_binary::write(&db, &QueueBinary::new(&first)).unwrap();
    let error = error_of(&run(&other, &db, &["service", "serve"]));
    assert!(error.contains("is not the queue's binary"), "{error}");
    assert_eq!(service::probe(db.parent().unwrap()).pid, running.pid);
    // A supervisor's watch of the other binary starts none either, with
    // the service gone.
    StopService(db.clone()).stop();
    stopped(&db);
    let error = format!(
        "{:#}",
        SystemQueueService::new(&db, &other)
            .start(std::time::Duration::from_secs(10))
            .unwrap_err()
    );
    assert!(error.contains("is not the queue's binary"), "{error}");
    stopped(&db);
}

/// `install` has the queue migrated by the service of the build it puts in
/// place before it is there, by a grant for that start alone, and then
/// names it (at the path it replaced) the queue's binary.
#[test]
fn install_lets_its_new_build_migrate_and_names_it_the_queues_binary() {
    let (dir, _owner) = common::cleanup::owned_temp_dir();
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
        let error = error_of(&run(&next, &db, &["service", "start"]));
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

/// A queue service of a build before the `migrate` use case on the socket
/// of the queue at `db`, as the one of the fixed binary in production then
/// answered: `hello` with its build, and every other request with the
/// `bad_request` of a request it does not read, in `words`. It stops and
/// takes its socket away when it goes.
struct ServiceBeforeMigrate {
    socket: PathBuf,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ServiceBeforeMigrate {
    fn start(db: &Path, words: &'static str) -> Self {
        use std::io::{BufRead, Write};
        use std::sync::atomic::Ordering;
        let socket = service::socket_path(db.parent().unwrap());
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let Ok((stream, _)) = listener.accept() else {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                // A caller that sends nothing does not hold the stop.
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut line = String::new();
                let _ = std::io::BufReader::new(&stream).read_line(&mut line);
                let request: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
                let answer = if request["use_case"] == "hello" {
                    serde_json::json!({
                        "api_version": 2, "min_api_version": 1, "ok": true,
                        "result": {
                            "service": "dagq-queue-service", "build": "0.0.0-before-migrate",
                            "pid": 1, "api_version": 2, "min_api_version": 1,
                        },
                    })
                } else {
                    serde_json::json!({
                        "api_version": 2, "min_api_version": 1, "ok": false,
                        "error": {"code": "bad_request", "message": words},
                    })
                };
                let _ = writeln!(&stream, "{answer}");
            }
        });
        Self {
            socket,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for ServiceBeforeMigrate {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _waiting = common::within(common::STEP_LIMIT, "the stand-in service to end");
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.socket);
    }
}

/// Where the judgment of the fixed binary of the build before goes on what
/// `migrate --check` of the new build printed.
#[derive(Debug, PartialEq, Eq)]
enum JudgmentBefore {
    /// The check failed: the update job fails at its `check`, and the
    /// install replaces nothing.
    CheckFailed,
    /// A breaking migration is pending: the update awaits approval.
    Breaking,
    /// The queue refuses the new build.
    Refused,
    /// Nothing is pending: the install talks to the running service as it
    /// is, of the build before.
    RunningService,
    /// The install holds the service and starts the new build's as a
    /// replacement (`hold_service`, then `serve(source, db, breaking)`).
    ServeNew { breaking: bool },
}

/// The judgment of the fixed binary in production when `migrate --check`
/// of a later build first refused to read a service before `migrate`,
/// copied from commit 47dc8b16d: its `LocalBinaries::schema`
/// (src/infrastructure/binaries.rs) reads only the exit status, `pending`
/// (an `i64` `version` and a `bool` `compatible` each) and `opens`; its
/// update job's `put_in_place` (src/application/update.rs) fails at `check`
/// on a failure and awaits approval on a breaking migration, and its
/// `install` (src/application/install.rs, after `binaries.schema`) refuses
/// a queue that does not open with nothing pending, keeps the running
/// service when nothing is pending, and else holds it and serves the new
/// build with `breaking` false.
fn judgment_before(output: &Output) -> JudgmentBefore {
    if !output.status.success() {
        return JudgmentBefore::CheckFailed;
    }
    let Ok(report) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return JudgmentBefore::CheckFailed;
    };
    let Some(listed) = report["pending"].as_array() else {
        return JudgmentBefore::CheckFailed;
    };
    let mut pending = Vec::new();
    for migration in listed {
        let Some(version) = migration["version"].as_i64() else {
            return JudgmentBefore::CheckFailed;
        };
        pending.push((version, migration["compatible"] == true));
    }
    let opens = report["opens"] == true;
    if pending.iter().any(|(_, compatible)| !compatible) {
        return JudgmentBefore::Breaking;
    }
    if !(opens || !pending.is_empty()) {
        return JudgmentBefore::Refused;
    }
    if pending.is_empty() {
        JudgmentBefore::RunningService
    } else {
        JudgmentBefore::ServeNew { breaking: false }
    }
}

/// `migrate --check` of a new build, with the queue's service one of a
/// build before the `migrate` use case (answering it with the words it
/// answers in production), opens no DB and ends with 0 and the answer that
/// sends the judgment of the fixed binary of the build before to the
/// replacement start of the new build's service, which applies the
/// compatible migrations. How such a service is told by its `hello` is the
/// unit test's of `domain::queue_service`, and that the replacement start
/// refuses a breaking migration is
/// `a_replacement_s_service_applies_a_breaking_migration_only_on_an_unused_queue`'s.
#[test]
fn a_new_build_s_check_leaves_a_service_before_migrate_to_the_replacement_start() {
    use std::os::unix::fs::PermissionsExt;
    let _test = common::test();
    let version = last_breaking();
    let (_dir, _owner, db, control, _service) = throwaway_queue(version);
    let before = ServiceBeforeMigrate::start(
        &db,
        "the request does not read: unknown variant `migrate`, expected one of `hello`",
    );
    // The DB does not open meanwhile: the check opens none.
    fs::set_permissions(&db, fs::Permissions::from_mode(0o000)).unwrap();
    let output = run(
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &db,
        &["migrate", "--check"],
    );
    fs::set_permissions(&db, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(output.status.success(), "{}", error_of(&output));
    let check: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(check["unanswered"], true, "{check}");
    assert!(
        check["reason"]
            .as_str()
            .unwrap()
            .contains(dagq::domain::queue_service::SCHEMA_UNANSWERED),
        "{check}"
    );
    assert_eq!(
        judgment_before(&output),
        JudgmentBefore::ServeNew { breaking: false },
        "{check}"
    );
    drop(before);
    assert_eq!(user_version(&db), version as i64);
    // The replacement start that judgment makes.
    let replaced = replace(&control, false, true).unwrap();
    assert!(replaced.service.current(), "{:?}", replaced.service);
    assert_eq!(user_version(&db), SqliteQueue::SCHEMA_VERSION);
}
