use crate::common;

use common::cli::*;
use common::{Bounded, WithoutActor};

use std::{
    path::Path,
    process::{Command, Output},
};

use dagq::infrastructure::sqlite::SqliteQueue;
use serde_json::Value;

#[test]
fn version_works_outside_a_repository_and_without_a_queue() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .arg("--version")
        .current_dir(dir.path())
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        format!("{} {}", env!("CARGO_PKG_NAME"), dagq::VERSION)
    );
}

/// `--version` names the build (ADR-0045 decision 2): a development version
/// carries the commit it was built from, and a release its version alone.
#[test]
fn version_is_the_build_identifier() {
    let package = env!("CARGO_PKG_VERSION");
    if dagq::build_id::is_prerelease(package) {
        let metadata = dagq::VERSION
            .strip_prefix(&format!("{package}+"))
            .unwrap_or_else(|| panic!("{} lacks build metadata", dagq::VERSION));
        let commit = metadata.strip_suffix(".dirty").unwrap_or(metadata);
        assert!(
            commit == dagq::build_id::UNKNOWN_COMMIT
                || (commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit())),
            "{}",
            dagq::VERSION
        );
    } else {
        assert_eq!(dagq::VERSION, package);
    }
    // The rule the build script embeds, over the same root. `.dirty` is
    // not compared: an edit after the build marks the tree dirty without a
    // rebuild.
    let expected = dagq::build_id::compute(
        env!("CARGO_PKG_NAME"),
        package,
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
    .identifier;
    let clean = |id: &str| id.strip_suffix(".dirty").unwrap_or(id).to_owned();
    assert_eq!(clean(dagq::VERSION), clean(&expected));
}

/// Runs `binary` (a copy of this one, as `claim` leaves a run's wrapper in
/// `runs/<id>/runner`) against `db`.
fn run_copy(binary: &Path, db: &Path, args: &[&str]) -> Output {
    Command::new(binary)
        .without_actor_env()
        .arg("--db")
        .arg(db)
        .args(args)
        .bounded_output()
        .unwrap()
}

#[test]
fn migrate_is_explicit_and_older_binaries_keep_working_within_the_floor() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    let raw = rusqlite::Connection::open(&db).unwrap();
    let version = || -> i64 {
        raw.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    };
    // A queue left at schema 23 by the binary before the floor table.
    for migration in &dagq::infrastructure::schema::MIGRATIONS[..23] {
        raw.execute_batch(migration).unwrap();
    }
    raw.execute_batch("PRAGMA application_id = 1129599281; PRAGMA user_version = 23;")
        .unwrap();
    raw.execute_batch(
        "INSERT INTO tasks(title,description,acceptance,verification_commands,status)
         VALUES ('old task','','','[]','draft');",
    )
    .unwrap();
    // Commands that change the queue need `migrate` first, and so do the
    // ones that only read: the queue's service reads the queue as it is,
    // never a copy migrated in memory (ADR-t3102-2 decision 4).
    for args in [
        &["add", "new task"][..],
        &["ready", "1"],
        &["init"],
        &["list"],
        &["show", "1"],
        &["status"],
    ] {
        let error = refused(&db, args);
        assert!(error.contains("run `dagq migrate`"), "{args:?}: {error}");
    }
    assert_eq!(version(), 23);
    let check = ok(&db, &["migrate", "--check"]);
    // `doctor` reports the schema as `migrate --check` does next to its
    // runs and supervisors (ADR-0045 decision 5).
    let doctor = ok(&db, &["doctor"]);
    assert_eq!(doctor["schema"], check);
    assert!(
        doctor["error"]
            .as_str()
            .is_some_and(|error| error.contains("run `dagq migrate`")),
        "{doctor}"
    );
    assert_eq!(check["schema_version"], 23);
    assert_eq!(check["binary_schema_version"], SqliteQueue::SCHEMA_VERSION);
    assert_eq!(check["opens"], false);
    // Every migration after 23, up to the last one the binary lists, with
    // its declaration (ADR-0067 decision 4).
    let migrations = dagq::infrastructure::schema::MIGRATIONS;
    let pending: Vec<Value> = (24..=SqliteQueue::SCHEMA_VERSION)
        .map(|version| {
            serde_json::json!({
                "version": version,
                "compatible": dagq::infrastructure::schema::is_compatible(
                    migrations[usize::try_from(version - 1).unwrap()]
                ),
            })
        })
        .collect();
    assert_eq!(check["pending"], Value::Array(pending));
    assert_eq!(
        check["pending"].as_array().unwrap()[..3],
        serde_json::json!([
            {"version": 24, "compatible": false},
            {"version": 25, "compatible": false},
            {"version": 26, "compatible": true}
        ])
        .as_array()
        .unwrap()[..]
    );
    assert_eq!(version(), 23);
    let migrated = ok(&db, &["migrate"]);
    assert_eq!(migrated["previous_version"], 23);
    assert_eq!(migrated["schema_version"], SqliteQueue::SCHEMA_VERSION);
    assert_eq!(
        migrated["floor"],
        dagq::infrastructure::schema::floor_for(SqliteQueue::SCHEMA_VERSION)
    );
    assert_eq!(migrated["commit_messages_filled"], 0);
    let backup = migrated["backup"].as_str().unwrap();
    assert!(Path::new(backup).starts_with(dir.path().canonicalize().unwrap().join("backups")));
    assert_eq!(version(), SqliteQueue::SCHEMA_VERSION);
    let doctor = ok(&db, &["doctor"]);
    assert_eq!(doctor["schema"]["opens"], true);
    assert_eq!(doctor["schema"]["pending"], serde_json::json!([]));
    ok(&db, &["add", "task one"]);

    // A later binary's compatible migration: this binary, and the wrapper
    // copy of it a running run uses, now are the older binaries.
    let runner = dir.path().join("runs/run-1/runner");
    std::fs::create_dir_all(runner.parent().unwrap()).unwrap();
    crate::common::template::dagq(&runner);
    raw.execute_batch(&format!(
        "ALTER TABLE tasks ADD COLUMN future_hint TEXT;
         CREATE TABLE future_things (id INTEGER PRIMARY KEY);
         PRAGMA user_version = {};",
        SqliteQueue::SCHEMA_VERSION + 1
    ))
    .unwrap();
    for args in [
        &["add", "task two"][..],
        &["show", "2"],
        &["list"],
        &["status"],
        &["doctor"],
        &["migrate"],
    ] {
        let output = run_copy(&runner, &db, args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(ok(&db, &["list"])["total"], 3);
    assert_eq!(version(), SqliteQueue::SCHEMA_VERSION + 1);

    // A later breaking migration raises the floor: the older binaries stop
    // with the reason, and leave the queue alone.
    raw.execute_batch(&format!(
        "UPDATE schema_floor SET floor = {0}; PRAGMA user_version = {0};",
        SqliteQueue::SCHEMA_VERSION + 2
    ))
    .unwrap();
    for args in [&["list"][..], &["migrate"]] {
        let output = run_copy(&runner, &db, args);
        assert!(!output.status.success(), "{args:?} succeeded");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        let error = error["error"].as_str().unwrap();
        assert!(
            error.contains(&format!(
                "unsupported queue schema version {0}: the queue refuses binaries older than schema {0}",
                SqliteQueue::SCHEMA_VERSION + 2
            )),
            "{args:?}: {error}"
        );
    }
    // The wrapper copy opens no queue: it reaches its run only through the
    // queue service, which judges it by its API version (ADR-t3102-2
    // decision 4), so with no service (nor its token) it stops on that and
    // not on the schema.
    let output = run_copy(
        &runner,
        &db,
        &[
            "session",
            "--run",
            "run-1",
            "--lease",
            "token",
            "--claude",
            "/bin/false",
            "--background",
        ],
    );
    assert!(!output.status.success(), "the wrapper copy succeeded");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    let error = error["error"].as_str().unwrap();
    assert!(error.contains("service"), "{error}");
    assert!(!error.contains("schema"), "{error}");
    // `doctor` still reports the schema that refuses the binary, and why.
    let output = run_copy(&runner, &db, &["doctor"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doctor: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        doctor["schema"]["schema_version"],
        SqliteQueue::SCHEMA_VERSION + 2
    );
    assert_eq!(doctor["schema"]["floor"], SqliteQueue::SCHEMA_VERSION + 2);
    assert_eq!(doctor["schema"]["opens"], false);
    assert!(doctor.get("runs").is_none());
    assert!(
        doctor["error"]
            .as_str()
            .unwrap()
            .contains("unsupported queue schema version"),
        "{doctor}"
    );
    assert_eq!(version(), SqliteQueue::SCHEMA_VERSION + 2);
}

/// Only when [`a_wait_past_its_limit_fails_with_the_test_and_the_condition`]
/// runs it: a wait that never ends, timed with a short limit.
#[test]
#[ignore = "run by a_wait_past_its_limit_fails_with_the_test_and_the_condition"]
fn deadline_probe() {
    if std::env::var_os("DAGQ_DEADLINE_PROBE").is_none() {
        return;
    }
    let _waiting = common::within(
        std::time::Duration::from_millis(300),
        "the probe's condition to hold",
    );
    loop {
        std::thread::park();
    }
}

/// A wait past its limit ends the test binary as a failure, naming the test
/// and the condition it waited for (task 324), instead of hanging.
#[test]
fn a_wait_past_its_limit_fails_with_the_test_and_the_condition() {
    let started = std::time::Instant::now();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "cli_version::deadline_probe",
            "--ignored",
            "--test-threads",
            "2",
        ])
        .env("DAGQ_DEADLINE_PROBE", "1")
        .bounded_output()
        .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
    assert_eq!(output.status.code(), Some(common::TIMED_OUT), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "test cli_version::deadline_probe timed out: the probe's condition to hold did not happen within 300ms"
        ),
        "{stderr}"
    );
}

/// Only when [`a_timed_out_wait_runs_the_cleanups_before_the_exit`] runs
/// it: a wait that never ends, with cleanups registered for its timeout.
#[test]
#[ignore = "run by a_timed_out_wait_runs_the_cleanups_before_the_exit"]
fn deadline_cleanup_probe() {
    let Some(dir) = std::env::var_os("DAGQ_DEADLINE_CLEANUP_PROBE") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let cleaned = dir.join("cleaned");
    let _cleanup = common::on_timeout(common::STEP_LIMIT, "the probe's marker", move || {
        std::fs::write(cleaned, "cleaned\n").unwrap()
    });
    let unregistered = dir.join("unregistered");
    drop(common::on_timeout(
        common::STEP_LIMIT,
        "an unregistered cleanup",
        move || std::fs::write(unregistered, "ran\n").unwrap(),
    ));
    // Newest first: this one runs, and is given up on, before the marker.
    let _stuck = common::on_timeout(
        std::time::Duration::from_millis(300),
        "a stuck cleanup",
        || loop {
            std::thread::park();
        },
    );
    let _waiting = common::within(
        std::time::Duration::from_millis(300),
        "the probe's condition to hold",
    );
    loop {
        std::thread::park();
    }
}

/// A wait past its limit runs the cleanups registered for it before the
/// test binary exits, newest first and each within its own limit, so one
/// that is stuck does not keep the binary from exiting (task 440).
#[test]
fn a_timed_out_wait_runs_the_cleanups_before_the_exit() {
    let dir = tempfile::tempdir().unwrap();
    let started = std::time::Instant::now();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "cli_version::deadline_cleanup_probe",
            "--ignored",
            "--test-threads",
            "2",
        ])
        .env("DAGQ_DEADLINE_CLEANUP_PROBE", dir.path())
        .bounded_output()
        .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
    assert_eq!(output.status.code(), Some(common::TIMED_OUT), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let at = |line: &str| {
        stderr
            .find(line)
            .unwrap_or_else(|| panic!("no {line:?} in {stderr}"))
    };
    let timed_out = at("test cli_version::deadline_cleanup_probe timed out: \
         the probe's condition to hold did not happen within 300ms");
    let stuck = at("cleaning up before the exit: a stuck cleanup\n");
    let given_up = at("cleanup a stuck cleanup did not finish within 300ms\n");
    let marker = at("cleaning up before the exit: the probe's marker\n");
    let done = at("cleanup the probe's marker done\n");
    assert!(timed_out < stuck && stuck < given_up && given_up < marker && marker < done);
    assert!(!stderr.contains("an unregistered cleanup"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("cleaned")).unwrap(),
        "cleaned\n"
    );
    assert!(!dir.path().join("unregistered").exists());
}

/// The hidden flags a `supervise` of these tests runs with (task 1048): it
/// passes every 100ms instead of every 1s (a job, the throughput review
/// among them, is active) or 2s (idle), so it takes a handoff sooner, and
/// heartbeats every 100ms instead of 2s, so the watch after a handoff sees
/// the next heartbeat (unix seconds) at the next second.
const FAST_SUPERVISOR: [&str; 6] = [
    "--tick-ms",
    "100",
    "--idle-poll-ms",
    "100",
    "--heartbeat-interval-ms",
    "100",
];

/// A queue and a repository with one commit to supervise, and a stub of
/// claude beside them.
struct Supervised {
    /// Stop the supporting service before deleting its queue directory.
    _service: common::service::DirectlyServed,
    dir: tempfile::TempDir,
    db: std::path::PathBuf,
    repo: std::path::PathBuf,
    claude: std::path::PathBuf,
    codex: std::path::PathBuf,
    auto_update: bool,
}

fn supervised() -> Supervised {
    supervised_with_binary(Path::new(env!("CARGO_BIN_EXE_dagq")), || {})
}

fn supervised_with_binary(binary: &Path, ready: impl FnOnce()) -> Supervised {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue").join("queue.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    drop(common::template::queue(&db));
    ready();
    let (service, repo, claude, codex) = std::thread::scope(|scope| {
        let service =
            scope.spawn(|| common::service::DirectlyServed::start_with_binary(&db, binary));
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        common::template::repository(&repo, "seed");
        let stub = |name: &str, text: &str| {
            let path = dir.path().join(name);
            crate::common::template::script(&path, format!("#!/bin/sh\nprintf '{text}\\n'\n"));
            path
        };
        let claude = stub("claude", "stub 1.0");
        let codex = stub("codex", "codex-cli 0.200.0");
        (service.join().unwrap(), repo, claude, codex)
    });
    Supervised {
        _service: service,
        db,
        repo,
        claude,
        codex,
        auto_update: false,
        dir,
    }
}

impl Supervised {
    /// `binary`'s `supervise` of the queue, stopping with `owner`
    /// (`dagq::domain::queue_service::OWNER_PID_ENV`) in place of this
    /// test's process.
    fn supervise(&self, binary: &Path, owner: u32) -> common::KillOnDrop {
        common::KillOnDrop::new(
            Command::new(binary)
                .without_actor_env()
                .env(
                    dagq::domain::queue_service::OWNER_PID_ENV,
                    owner.to_string(),
                )
                .arg("--db")
                .arg(&self.db)
                .args(["supervise", "--observe-interval", "0", "--repo"])
                .arg(&self.repo)
                .arg("--claude")
                .arg(&self.claude)
                .arg("--codex")
                .arg(&self.codex)
                .args(self.auto_update.then_some("--auto-update"))
                // This fixture exercises process ownership and exec, without
                // starting unrelated observation, review or report work.
                .args([
                    "--observe-daily",
                    "false",
                    "--throughput-review",
                    "false",
                    "--report-daily",
                    "false",
                    "--forecast-snapshots",
                    "false",
                ])
                .args(FAST_SUPERVISOR)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
            "the supervisor",
        )
    }

    fn registered(&self) -> Vec<dagq::domain::SupervisorRegistration> {
        SqliteQueue::open(&self.db).unwrap().supervisors().unwrap()
    }

    fn wait_registered(&self) {
        let started = std::time::Instant::now();
        while !self
            .registered()
            .first()
            .is_some_and(|r| r.handoff_accepted)
        {
            assert!(
                started.elapsed().as_secs() < 30,
                "the supervisor never registered to accept a handoff"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// Kill `owner` with SIGKILL and reap it, then wait for the supervisor
    /// to stop by itself: it leaves no registration and records the
    /// owner's going as the reason of its stop.
    fn kill_owner_and_see_the_supervisor_stop(
        &self,
        owner: &mut common::service::StandInOwner,
        supervisor: &mut common::KillOnDrop,
    ) {
        owner.kill();
        let exit = {
            let _waiting = common::within(
                std::time::Duration::from_secs(60),
                "the supervisor to stop once its owner is gone",
            );
            supervisor.child().wait().unwrap()
        };
        assert!(exit.success(), "{exit:?}");
        assert!(self.registered().is_empty());
        let draining = common::service::events(&self.db, "supervisor_draining");
        assert_eq!(draining.len(), 1, "{draining:?}");
        assert_eq!(
            draining[0]["payload"]["reason"], "owner_gone",
            "{draining:?}"
        );
        let stopped = common::service::events(&self.db, "supervisor_stopped");
        assert_eq!(stopped.len(), 1, "{stopped:?}");
        assert_eq!(stopped[0]["payload"]["reason"], "owner_gone", "{stopped:?}");
        assert_eq!(stopped[0]["payload"]["outcome"], "stopped", "{stopped:?}");
    }
}

/// A supervisor given an owner (a test's process) stops by itself once
/// the owner is gone, however it ended (here a SIGKILL, which skips every
/// drop of the test): it drains, leaves no registration, and records the
/// owner's going apart from a stop request. One whose owner is gone
/// already at its start registers nothing.
#[test]
fn a_supervisor_stops_by_itself_once_its_owner_is_gone() {
    let fixture = supervised();
    let binary = Path::new(env!("CARGO_BIN_EXE_dagq"));

    let mut gone = common::service::StandInOwner::start();
    let pid = gone.pid();
    gone.kill();
    let mut early = fixture.supervise(binary, pid);
    let exit = {
        let _waiting = common::within(common::STEP_LIMIT, "the supervisor of a gone owner");
        early.child().wait().unwrap()
    };
    assert!(exit.success(), "{exit:?}");
    assert!(fixture.registered().is_empty());
    assert!(common::service::events(&fixture.db, "supervisor_started").is_empty());

    let mut owner = common::service::StandInOwner::start();
    let mut supervisor = fixture.supervise(binary, owner.pid());
    fixture.wait_registered();
    assert_eq!(fixture.registered()[0].pid, supervisor.child().id());
    fixture.kill_owner_and_see_the_supervisor_stop(&mut owner, &mut supervisor);
}

/// A supervisor handed over to another binary by `install` (the exec of
/// the automatic update's handoff too) keeps its owner, since the exec
/// keeps its environment, and still stops once the owner is gone. The
/// file adapter then restores the kept inode without a second exec.
#[test]
fn install_execs_a_running_supervisor_under_its_pid_and_keeps_its_owner() {
    let templates = tempfile::tempdir().unwrap();
    let templates_path = templates.path().to_owned();
    common::template::with_root(templates.path(), || {
        let phase = std::time::Instant::now();
        // Every run starts with a fresh, unexecuted file and private
        // templates. No previous Cargo call can warm this image for us.
        let image_dir = tempfile::tempdir().unwrap();
        let source = image_dir.path().join("dagq");
        let measured = |path: &Path| {
            ring::digest::digest(&ring::digest::SHA256, &std::fs::read(path).unwrap())
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        std::thread::scope(|scope| {
            let image = scope.spawn(|| {
                std::fs::copy(env!("CARGO_BIN_EXE_dagq"), &source).unwrap();
                // Keep the real executable code and strip only symbol/debug data.
                // Stripping and its first exec are both inside this cold test.
                let stripped = Command::new("strip").arg(&source).bounded_output().unwrap();
                assert!(stripped.status.success(), "{stripped:?}");
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o555)).unwrap();
                measured(&source)
            });
            let mut expected_digest = String::new();
            let mut fixture =
                supervised_with_binary(&source, || expected_digest = image.join().unwrap());
            eprintln!(
                "install boundary: cold fixture {:?}, image {} bytes",
                phase.elapsed(),
                std::fs::metadata(&source).unwrap().len()
            );
            // This repository is not dagq's source, so no update job starts; the
            // registration's enabled bit must still cross the exec.
            fixture.auto_update = true;
            let fixed = fixture.dir.path().join("bin").join("dagq");
            std::fs::create_dir_all(fixed.parent().unwrap()).unwrap();
            // A small predecessor reports its own version and launches the real
            // supervisor under the same PID. The incoming file is the actual build.
            let old_version = "0.0.0-install-fixture";
            common::template::script(
                &fixed,
                format!(
                    "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'dagq {old_version}'; else exec {} \"$@\"; fi\n",
                    common::shell_path(&source)
                ),
            );
            use std::os::unix::fs::MetadataExt;
            let original_inode = std::fs::metadata(&fixed).unwrap().ino();
            let mut owner = common::service::StandInOwner::start();
            let mut supervisor = fixture.supervise(&fixed, owner.pid());
            fixture.wait_registered();
            eprintln!("install boundary: registered {:?}", phase.elapsed());
            let before = fixture.registered().remove(0);
            assert_eq!(before.binary_version.as_deref(), Some(dagq::VERSION));
            assert!(before.auto_update);
            let token = before.token.clone();
            assert_ne!(std::fs::metadata(&source).unwrap().ino(), original_inode);
            let invoke_install = |args: &[&str]| {
                Command::new(&source)
                    .without_actor_env()
                    .env(
                        dagq::domain::queue_service::OWNER_PID_ENV,
                        std::process::id().to_string(),
                    )
                    .env(
                        "XDG_CONFIG_HOME",
                        fixture.db.parent().unwrap().join("config"),
                    )
                    .arg("--db")
                    .arg(&fixture.db)
                    .args(args)
                    .bounded_output()
                    .unwrap()
            };
            // Exercise the CLI selection and composition of Source::Rollback before
            // any replacement: no second successful install/exec is needed here.
            let missing =
                invoke_install(&["install", "--rollback", "--to", fixed.to_str().unwrap()]);
            assert!(!missing.status.success());
            assert!(
                String::from_utf8_lossy(&missing.stderr).contains("no previous binary"),
                "{missing:?}"
            );
            eprintln!("install boundary: rollback refused {:?}", phase.elapsed());
            let output = invoke_install(&[
                "install",
                "--from",
                source.to_str().unwrap(),
                "--to",
                fixed.to_str().unwrap(),
                "--handoff-timeout",
                "60",
                "--poll-ms",
                "20",
                "--binary-fixture",
                source.to_str().unwrap(),
            ]);
            assert!(output.status.success(), "{output:?}");
            let report: Value = serde_json::from_slice(&output.stdout).unwrap();
            eprintln!("install boundary: installed {:?}", phase.elapsed());
            assert_eq!(report["outcome"], "installed", "{report}");
            assert_eq!(report["version"], dagq::VERSION);
            assert_eq!(report["previous_version"], old_version);
            assert_eq!(
                report["previous"],
                fixed.with_file_name("dagq.previous").to_str().unwrap()
            );
            assert_eq!(report["migrated"], Value::Null);
            assert_eq!(report["supervisors"][0]["token"], token.as_str());
            assert_eq!(report["supervisors"][0]["pid"], supervisor.child().id());
            let started = std::time::Instant::now();
            loop {
                let handed = common::service::events(&fixture.db, "supervisor_started");
                let ready = fixture
                    .registered()
                    .first()
                    .is_some_and(|r| r.handoff_accepted && r.handoff_binary.is_none());
                if ready
                    && handed
                        .iter()
                        .any(|event| event["payload"]["handoff"] == true)
                {
                    break;
                }
                assert!(
                    started.elapsed() < common::STEP_LIMIT,
                    "the installed binary never took the handoff: {handed:?}"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let registration = fixture.registered().remove(0);
            assert!(registration.auto_update);
            assert_eq!(registration.token, token);
            assert_eq!(registration.pid, supervisor.child().id());
            assert_eq!(registration.handoff_binary, None);
            assert_eq!(registration.binary_version.as_deref(), Some(dagq::VERSION));
            let previous = fixed.with_file_name("dagq.previous");
            assert_eq!(std::fs::metadata(&previous).unwrap().ino(), original_inode);
            assert_ne!(std::fs::metadata(&fixed).unwrap().ino(), original_inode);
            assert_eq!(
                std::fs::metadata(&fixed).unwrap().ino(),
                std::fs::metadata(&source).unwrap().ino()
            );
            // Compare the reported facts with independent SHA-256 measurements of
            // the files, including the distinct predecessor's actual version.
            assert_eq!(report["digest"], expected_digest);
            assert_eq!(report["replaced_binary"]["digest"], measured(&previous));
            assert_eq!(report["replaced_binary"]["version"], old_version);
            assert!(supervisor.child().try_wait().unwrap().is_none());
            fixture.kill_owner_and_see_the_supervisor_stop(&mut owner, &mut supervisor);
            eprintln!("install boundary: stopped {:?}", phase.elapsed());
            dagq::application::install::Binaries::restore(
                &dagq::infrastructure::binaries::LocalBinaries,
                &fixed,
            )
            .unwrap();
            assert_eq!(std::fs::metadata(&fixed).unwrap().ino(), original_inode);
            assert!(!previous.exists());
            let fixture_dir = fixture.dir.path().to_owned();
            drop(fixture);
            assert!(
                !fixture_dir.exists(),
                "install images must leave with the fixture"
            );
            drop(image_dir);
            assert!(
                !source.exists(),
                "the cold executable must not outlive its fixture"
            );
        });
    });
    drop(templates);
    assert!(!templates_path.exists());
}
