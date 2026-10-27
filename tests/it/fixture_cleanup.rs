//! What the queue fixture leaves in the temporary directory: nothing after
//! its drop, though a session wrapper's thread writes into it as its stub
//! is killed, and what a killed test process left is swept by a later
//! fixture.
use crate::common::cleanup::{
    OWNER_FILE, claim_fixture_dir, owned_temp_dir_in, sweep_abandoned_fixtures_in,
};
use crate::common::{self, Bounded, SHORT_TEMP_ROOT, set_mode};
use crate::runtime_support::{fixture, fixture_dropped, joined, wrapper_running, wrappers_running};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime},
};

/// A wrapper's thread that writes its run's idle marker, its directories
/// made again, once the fixture's drop killed the stubs: the drop waits for
/// it before the directory goes, so the directory does not come back.
#[test]
fn a_wrapper_writing_as_its_fixture_drops_leaves_no_directory() {
    let (dir, _repo, db) = fixture();
    let path = dir.path().to_owned();
    let marker = path.join("runs").join("run").join("idle.json");
    let running = wrapper_running(&db);
    let wrapper = {
        let (db, marker) = (db.clone(), marker.clone());
        thread::spawn(move || {
            let _running = running;
            let waited = Instant::now();
            while !fixture_dropped(&db) {
                assert!(waited.elapsed() < Duration::from_secs(30), "no drop");
                thread::sleep(Duration::from_millis(10));
            }
            // After the stubs are killed, as a wrapper records the turn.
            thread::sleep(Duration::from_millis(100));
            fs::create_dir_all(marker.parent().unwrap()).unwrap();
            fs::write(&marker, "{}").unwrap();
        })
    };
    drop(dir);
    assert_eq!(wrappers_running(&db), 0);
    joined(wrapper, "the wrapper to write its marker");
    assert!(!path.exists(), "{} is left", path.display());
}

/// Only when
/// [`a_wrapper_past_the_drops_limit_fails_the_test_and_leaves_its_directory_whole`]
/// runs it: a fixture dropped with a session wrapper's thread that never
/// ends, its directory named in the file the environment names.
#[test]
#[ignore = "run by a_wrapper_past_the_drops_limit_fails_the_test_and_leaves_its_directory_whole"]
fn wrapper_limit_probe() {
    let Some(seen) = std::env::var_os("DAGQ_WRAPPER_LIMIT_PROBE") else {
        return;
    };
    let (mut dir, _repo, db) = fixture();
    dir.wrappers_limit = Duration::from_millis(300);
    fs::write(seen, dir.path().as_os_str().as_encoded_bytes()).unwrap();
    let running = wrapper_running(&db);
    // Ends with the process: the drop's timeout exits it.
    thread::spawn(move || {
        let _running = running;
        loop {
            thread::park();
        }
    });
    drop(dir);
    panic!("the drop went on with a session wrapper running");
}

/// A session wrapper's thread still running past the drop's limit fails
/// the test binary, naming the wait, as any wait past its limit does, and
/// neither the drop nor the timeout's cleanup removes the directory under
/// it: the directory is left whole, its owner lock freed by the exit, and
/// the sweep of a later fixture takes it.
#[test]
fn a_wrapper_past_the_drops_limit_fails_the_test_and_leaves_its_directory_whole() {
    let seen = tempfile::tempdir().unwrap();
    let seen = seen.path().join("fixture");
    let started = Instant::now();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "fixture_cleanup::wrapper_limit_probe",
            "--ignored",
            "--test-threads",
            "2",
        ])
        .env("DAGQ_WRAPPER_LIMIT_PROBE", &seen)
        .bounded_output()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(30));
    assert_eq!(output.status.code(), Some(common::TIMED_OUT), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "test fixture_cleanup::wrapper_limit_probe timed out: the session wrappers on "
        ) && stderr.contains("to end after the fixture's drop did not happen within 300ms"),
        "{stderr}"
    );
    assert!(!stderr.contains("the drop went on"), "{stderr}");
    let dir = PathBuf::from(fs::read_to_string(&seen).unwrap());
    assert!(dir.join("queue's data.db").is_file(), "{stderr}");
    let owner = fs::File::open(dir.join(OWNER_FILE)).unwrap();
    assert!(owner.try_lock().is_ok(), "the owner lock is held");
    drop(owner);
    sweep_abandoned_fixtures_in(dir.parent().unwrap());
    assert!(!dir.exists(), "{} is left", dir.display());
}

/// A wrapper's thread between turns ends at the exit request the fixture's
/// drop writes into its run directory, before the directory goes.
#[test]
fn a_wrapper_between_turns_takes_the_exit_request_of_its_fixtures_drop() {
    let (dir, _repo, db) = fixture();
    let path = dir.path().to_owned();
    let turns = path.join("runs").join("run").join("turns");
    fs::create_dir_all(&turns).unwrap();
    let running = wrapper_running(&db);
    let wrapper = {
        let exit = dagq::domain::turn::exit_path(turns.parent().unwrap());
        thread::spawn(move || {
            let _running = running;
            let waited = Instant::now();
            while !exit.exists() {
                if waited.elapsed() > Duration::from_secs(30) {
                    return false;
                }
                thread::sleep(Duration::from_millis(10));
            }
            true
        })
    };
    let dropped = Instant::now();
    drop(dir);
    assert!(dropped.elapsed() < Duration::from_secs(10));
    assert!(
        joined(wrapper, "the wrapper to see its exit request"),
        "the wrapper saw no exit request"
    );
    assert!(!path.exists(), "{} is left", path.display());
}

/// The exit requests of the fixture's drop change nothing out of the
/// fixture through a link a test left in a run directory: an `exit` that
/// is a link is replaced, not written through, and a `turns` or a run that
/// is a link is left alone.
#[test]
fn the_drops_exit_requests_write_through_no_link_out_of_the_fixture() {
    use dagq::domain::turn::exit_path;
    use std::os::unix::fs::symlink;
    let outside = tempfile::tempdir().unwrap();
    let kept = outside.path().join("kept");
    fs::write(&kept, "kept\n").unwrap();
    let (turns, run) = (outside.path().join("turns"), outside.path().join("run"));
    fs::create_dir(&turns).unwrap();
    fs::create_dir(&run).unwrap();
    let (dir, _repo, _db) = fixture();
    let runs = dir.path().join("runs");
    fs::create_dir_all(runs.join("exit-linked/turns")).unwrap();
    symlink(&kept, exit_path(&runs.join("exit-linked"))).unwrap();
    fs::create_dir(runs.join("turns-linked")).unwrap();
    symlink(&turns, runs.join("turns-linked/turns")).unwrap();
    symlink(&run, runs.join("run-linked")).unwrap();

    let path = dir.path().to_owned();
    drop(dir);

    assert!(!path.exists(), "{} is left", path.display());
    assert_eq!(fs::read_to_string(&kept).unwrap(), "kept\n");
    for linked in [&turns, &run] {
        let entries: Vec<_> = fs::read_dir(linked).unwrap().flatten().collect();
        assert!(entries.is_empty(), "{} has {entries:?}", linked.display());
    }
}

/// The runtime's fixture is in `/tmp` and holds its directory's owner lock
/// while it lives, which a kill of its test frees for a later sweep.
#[test]
fn the_runtime_fixture_holds_its_directorys_owner_lock() {
    let (dir, _repo, _db) = fixture();
    assert!(
        dir.path().starts_with(SHORT_TEMP_ROOT),
        "{}",
        dir.path().display()
    );
    let owner = fs::File::open(dir.path().join(OWNER_FILE)).unwrap();
    assert!(owner.try_lock().is_err(), "the owner lock is free");
}

/// The first owned directory of a process in a root sweeps what a dead
/// test left there first, and a later sweep leaves the owned one while its
/// owner holds the lock and takes it once the lock is freed, as a kill of
/// its test frees it.
#[test]
fn an_owned_directory_is_swept_only_once_its_owner_is_gone() {
    let root = tempfile::tempdir().unwrap();
    let dead = root.path().join(".tmpdead");
    fs::create_dir(&dead).unwrap();
    drop(claim_fixture_dir(&dead));

    let (dir, owner) = owned_temp_dir_in(root.path());
    assert!(!dead.exists(), "{} is left", dead.display());
    assert!(dir.path().starts_with(root.path()));
    sweep_abandoned_fixtures_in(root.path());
    assert!(dir.path().exists(), "{} is swept", dir.path().display());
    drop(owner);
    sweep_abandoned_fixtures_in(root.path());
    assert!(!dir.path().exists(), "{} is left", dir.path().display());
}

/// `dir`'s modification time an hour and a half ago.
fn age(dir: &Path) {
    fs::File::open(dir)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(90 * 60))
        .unwrap();
}

/// The sweep takes a directory whose owner is gone (with a directory in it
/// made unwritable) and the old shapes of the fixtures without an owner (a
/// `tests/it` queue fixture's, a container queue's, what a late idle marker
/// made again), and leaves an owned one (an older e2e's lock included), a
/// young unmarked one and a directory of another kind.
#[test]
fn the_sweep_takes_only_fixture_directories_their_owners_left() {
    let root = tempfile::tempdir().unwrap();
    let make = |name: &str| {
        let dir = root.path().join(name);
        fs::create_dir(&dir).unwrap();
        dir
    };
    let dead = make(".tmpdead");
    drop(claim_fixture_dir(&dead));
    fs::write(dead.join("queue's data.db"), "").unwrap();
    // A directory its killed test made unwritable to inject a failure.
    let unwritable = dead.join("runs");
    fs::create_dir(&unwritable).unwrap();
    fs::write(unwritable.join("idle.json"), "{}").unwrap();
    set_mode(&unwritable, 0o500);
    let live = make(".tmplive");
    let _owner = claim_fixture_dir(&live);
    let queue = make(".tmpqueue");
    fs::create_dir(queue.join("repo's directory")).unwrap();
    fs::write(queue.join("queue's data.db"), "").unwrap();
    age(&queue);
    let idle = make(".tmpidle");
    fs::create_dir_all(idle.join("runs/a")).unwrap();
    fs::write(idle.join("runs/a/idle.json"), "{}").unwrap();
    age(&idle);
    let young = make(".tmpyoung");
    fs::create_dir_all(young.join("runs/a")).unwrap();
    fs::write(young.join("runs/a/idle.json"), "{}").unwrap();
    let container = make(".tmpcontainer");
    for sub in ["repo", "data/dagq/q", "podman"] {
        fs::create_dir_all(container.join(sub)).unwrap();
    }
    age(&container);
    // An older binary's e2e fixture, its owner gone or alive.
    let legacy = make(".tmplegacy");
    fs::write(legacy.join("e2e-owner"), "1\n").unwrap();
    let legacy_live = make(".tmplegacylive");
    let held = fs::File::create(legacy_live.join("e2e-owner")).unwrap();
    held.lock().unwrap();
    age(&legacy_live);
    let other = make(".tmpother");
    fs::create_dir_all(other.join("runs/a")).unwrap();
    fs::write(other.join("runs/a/idle.json"), "{}").unwrap();
    fs::write(other.join("runs/a/receipt.json"), "{}").unwrap();
    fs::write(other.join("q.db"), "").unwrap();
    age(&other);

    sweep_abandoned_fixtures_in(root.path());

    for gone in [&dead, &queue, &idle, &container, &legacy] {
        assert!(!gone.exists(), "{} is left", gone.display());
    }
    for kept in [&live, &young, &other, &legacy_live] {
        assert!(kept.exists(), "{} is swept", kept.display());
    }
}

/// The sweep follows no link out of its root: a link with an old
/// container queue's shape at its end, a fixture's shape made of links, and
/// an owner file that is a link are neither taken nor written through.
#[test]
fn the_sweep_follows_no_link_out_of_its_root() {
    let (root, outside) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let target = outside.path().join("container");
    for sub in ["repo", "data", "podman"] {
        fs::create_dir_all(target.join(sub)).unwrap();
    }
    age(&target);
    let link = root.path().join(".tmplink");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let linked = root.path().join(".tmplinked");
    fs::create_dir(&linked).unwrap();
    for sub in ["repo", "data", "podman"] {
        std::os::unix::fs::symlink(target.join(sub), linked.join(sub)).unwrap();
    }
    age(&linked);
    let made = outside.path().join("made");
    let dangling = root.path().join(".tmpdangling");
    for sub in ["repo", "data", "podman"] {
        fs::create_dir_all(dangling.join(sub)).unwrap();
    }
    std::os::unix::fs::symlink(&made, dangling.join(OWNER_FILE)).unwrap();
    age(&dangling);

    sweep_abandoned_fixtures_in(root.path());

    for kept in [&target.join("repo"), &link, &linked, &dangling] {
        assert!(kept.exists(), "{} is swept", kept.display());
    }
    assert!(
        !target.join(OWNER_FILE).exists(),
        "an owner file made through the link"
    );
    assert!(
        fs::symlink_metadata(&made).is_err(),
        "{} is made",
        made.display()
    );
}
