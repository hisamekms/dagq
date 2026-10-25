//! Only the queue's binary changes its schema (ADR-t3102-2 decisions 2 and
//! 3): a development build and a run's `runner` copy refuse `migrate`,
//! `up` and the writes before anything changes, and `install` lets the build it puts in place
//! migrate. Each test makes its own throwaway queue and the two binaries:
//! the queue's (a copy of the test's `dagq` that `init` names) and another.

use crate::common;
use common::lifecycle::*;
use common::{Bounded, WithoutActor};

use dagq::infrastructure::{
    queue_binary::{self, GRANT_FILE, QueueBinary},
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
        // `ready --inherit` moves a run's branch, so it opens the DB itself.
        let error = error_of(&run(
            binary,
            &db,
            &["ready", "1", "--inherit", "--reason", "r"],
        ));
        assert!(
            error.contains("run `dagq migrate` with the queue's binary"),
            "{error}"
        );
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

/// `install` migrates the queue with the build it puts in place before it
/// is there, by a grant for that `migrate` alone, and then names it (at
/// the path it replaced) the queue's binary.
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
    // The install names the path it puts the binary at.
    let installed = copy_of_dagq(&dir.path().join("installed").join("dagq"));
    let output = run(
        &fixed,
        &db,
        &[
            "install",
            "--from",
            next.to_str().unwrap(),
            "--to",
            installed.to_str().unwrap(),
        ],
    );
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
    assert_eq!(
        queue_binary::read(&db).unwrap(),
        Some(QueueBinary::new(&installed))
    );
}

/// `up` of a binary that is not the queue's applies no migration and stops
/// before it starts anything.
#[test]
fn up_of_another_binary_applies_no_migration() {
    let fixture = fixture();
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
}
