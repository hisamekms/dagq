//! The shells a test starts end with the test (task 1580): a wait of
//! [`common::await_file`] once the test's directory or the shell's parent
//! is gone, and a headless stub `claude` with its turn's loops and children
//! once its fixture drops or its parent (the test process) is gone. Before,
//! such shells outlived failed and timed-out tests by hours with init as
//! their parent.
use crate::common;
use crate::runtime_support;

use runtime_support::*;

/// How long a shell that should end may take: many of its 0.05 s and 0.2 s
/// ticks, under the load of a whole run.
const ENDS_WITHIN: Duration = Duration::from_secs(20);

/// Kills the process group led by `0`, and `0` itself, if it still runs
/// when dropped, so that a test that fails leaves nothing behind.
struct Leftover(u32);

impl Drop for Leftover {
    fn drop(&mut self) {
        // Only while it runs: a pid that ended may be another's by now.
        if running(self.0) {
            // SAFETY: kill(2) takes no pointer; a negative pid names the
            // group the test's stub leads.
            unsafe {
                libc::kill(-(self.0 as libc::pid_t), libc::SIGKILL);
                libc::kill(self.0 as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

/// Wait until `pid` no longer runs, failing past [`ENDS_WITHIN`].
fn ends(pid: u32, what: &str) {
    let begun = Instant::now();
    while running(pid) {
        assert!(begun.elapsed() < ENDS_WITHIN, "{what} {pid} lives on");
        thread::sleep(Duration::from_millis(50));
    }
}

/// The pids a turn of [`waiting_turn`] wrote: the stub's and its `sleep`'s.
fn turn_pids(file: &Path) -> (u32, u32) {
    let begun = Instant::now();
    while !file.exists() {
        assert!(
            begun.elapsed() < common::STEP_LIMIT,
            "no {}",
            file.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
    let text = fs::read_to_string(file).unwrap();
    let (stub, sleep) = text.trim().split_once(' ').unwrap();
    (stub.parse().unwrap(), sleep.parse().unwrap())
}

/// A turn that leaves a child, writes its own and the child's pid to
/// `$RUN_DIR/pids`, and loops for good, as the turns that outlived their
/// tests did.
const WAITING_TURN: &str = r#"sleep 300 &
printf '%s %s\n' $$ $! > "$RUN_DIR/pids.tmp"; mv "$RUN_DIR/pids.tmp" "$RUN_DIR/pids"
while :; do sleep 0.05; done"#;

#[test]
fn a_file_wait_returns_once_the_file_is_there() {
    let dir = tempfile::tempdir().unwrap();
    let release = dir.path().join("queue's data.release");
    let mut wait = common::KillOnDrop::new(
        Command::new("/bin/sh")
            .args(["-c", &common::await_path(&release)])
            .spawn()
            .unwrap(),
        "the wait for the release",
    );
    fs::write(&release, "").unwrap();
    let _waiting = common::within(common::STEP_LIMIT, "the wait to return");
    assert!(wait.child().wait().unwrap().success());
}

/// The wait of a verification that a test failed to release (it ended
/// before it wrote the file) ends with the test's directory.
#[test]
fn a_file_wait_ends_when_the_tests_directory_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let release = dir.path().join("queue's data.release");
    let mut wait = common::KillOnDrop::new(
        Command::new("/bin/sh")
            .args(["-c", &common::await_path(&release)])
            .spawn()
            .unwrap(),
        "the wait for the release",
    );
    thread::sleep(Duration::from_millis(200));
    assert!(wait.child().try_wait().unwrap().is_none(), "it waits");
    drop(dir);
    let begun = Instant::now();
    let status = loop {
        if let Some(status) = wait.child().try_wait().unwrap() {
            break status;
        }
        assert!(begun.elapsed() < ENDS_WITHIN, "the wait lives on");
        thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(1));
}

/// The wait of a test process that ended without its drops (a timeout's
/// `process::exit`) ends with its parent, though the directory stays.
#[test]
fn a_file_wait_ends_when_its_parent_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let release = dir.path().join("release");
    let pid_file = dir.path().join("wait.pid");
    // The parent starts the wait and ends at once.
    let status = Command::new("/bin/sh")
        .args([
            "-c",
            "/bin/sh -c \"$1\" </dev/null >/dev/null 2>&1 & echo $! > \"$2\"",
            "sh",
            &common::await_path(&release),
            pid_file.to_str().unwrap(),
        ])
        .bounded_status()
        .unwrap();
    assert!(status.success());
    let pid: u32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _leftover = Leftover(pid);
    ends(pid, "the orphaned wait");
    assert!(dir.path().exists() && !release.exists());
}

/// Acceptance (2) of task 1580: a headless turn that waits for good, with
/// a child, dies with the test's fixture.
#[test]
fn a_waiting_headless_turn_and_its_child_die_with_the_fixture() {
    let (fixture, _repo, db) = fixture();
    let stub = headless_claude(fixture.path(), &db);
    set_turns(fixture.path(), WAITING_TURN);
    let run_dir = fixture.path().join("run");
    fs::create_dir(&run_dir).unwrap();
    let mut spec = CommandSpec::new(&stub);
    spec.args(["-p", "--session-id", "s1", "--add-dir"])
        .arg(&run_dir)
        .args(["--", "work"]);
    let (stdout, stderr) = (run_dir.join("stdout"), run_dir.join("stderr"));
    let child = StubSpawner { db: db.clone() }
        .spawn(
            &spec,
            Streams::Files {
                stdout: &stdout,
                stderr: &stderr,
            },
        )
        .unwrap();
    let (stub_pid, sleep) = turn_pids(&run_dir.join("pids"));
    assert_eq!(stub_pid, child.id());
    let _leftovers = (Leftover(stub_pid), Leftover(sleep));
    assert!(running(stub_pid) && running(sleep));
    drop(fixture);
    // The stub is this process's child: reaped here once killed.
    let begun = Instant::now();
    loop {
        // SAFETY: waitpid(2) with a null status pointer writes nothing.
        let reaped =
            unsafe { libc::waitpid(stub_pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) };
        if reaped == stub_pid as libc::pid_t {
            break;
        }
        assert!(begun.elapsed() < ENDS_WITHIN, "stub {stub_pid} lives on");
        thread::sleep(Duration::from_millis(20));
    }
    ends(sleep, "the turn's sleep");
}

/// A headless turn whose test process ended without the drops (on a
/// timeout's `process::exit`, or killed) kills its group, with the turn's
/// loop and its child, though its directory and its parent stay. A process
/// of the test's stands in for the test process.
#[test]
fn a_waiting_headless_turn_and_its_child_die_with_their_test_process() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue's data.db");
    let mut stand_in = common::KillOnDrop::new(
        Command::new("sleep").arg("300").spawn().unwrap(),
        "the stand-in of the test process",
    );
    let stub = headless_claude_of(dir.path(), &db, stand_in.child().id());
    set_turns(dir.path(), WAITING_TURN);
    let run_dir = dir.path().join("run");
    fs::create_dir(&run_dir).unwrap();
    let mut spec = CommandSpec::new(&stub);
    spec.args(["-p", "--session-id", "s1", "--add-dir"])
        .arg(&run_dir)
        .args(["--", "work"]);
    let (stdout, stderr) = (run_dir.join("stdout"), run_dir.join("stderr"));
    let fixture = fixture();
    let child = StubSpawner {
        db: fixture.0.db.clone(),
    }
    .spawn(
        &spec,
        Streams::Files {
            stdout: &stdout,
            stderr: &stderr,
        },
    )
    .unwrap();
    let (stub_pid, sleep) = turn_pids(&run_dir.join("pids"));
    assert_eq!(stub_pid, child.id());
    let _leftovers = (Leftover(stub_pid), Leftover(sleep));
    thread::sleep(Duration::from_millis(500));
    assert!(running(stub_pid) && running(sleep), "the turn waits");
    drop(stand_in);
    ends(sleep, "the turn's sleep");
    let begun = Instant::now();
    loop {
        // SAFETY: waitpid(2) with a null status pointer writes nothing.
        let reaped =
            unsafe { libc::waitpid(stub_pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) };
        if reaped == stub_pid as libc::pid_t {
            break;
        }
        assert!(begun.elapsed() < ENDS_WITHIN, "stub {stub_pid} lives on");
        thread::sleep(Duration::from_millis(20));
    }
    assert!(run_dir.exists());
}
