//! The e2e gate on this machine (`infrastructure::e2e_gate`) at its process
//! and directory boundaries: the e2e it starts with a cleared environment,
//! its rerun of the failed tests by name, the host's lock, and the cleanup
//! of what the e2e and earlier gates left. What it decides (which tests to
//! rerun, which failed again, what the e2e is given, the command) is in
//! the module's unit tests.
use crate::common;
use dagq::application::install::E2eSettings;
use dagq::domain::e2e_quarantine::{self, QuarantineFile};
use dagq::infrastructure::e2e_gate;
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

fn settings(dir: &Path, command: &str) -> E2eSettings {
    E2eSettings {
        command: Some(command.to_owned()),
        timeout: Duration::from_secs(60),
        run_env_root: Some(dir.join("repo")),
        queue_dir: Some(dir.join("queue")),
        scratch: dir.join("scratch"),
        log: dir.join("logs").join("e2e.log"),
        utc_offset_secs: 0,
        lock: None,
        git_env: Vec::new(),
        ended: None,
    }
}

fn subdirectories(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default()
}

/// The names a shell adds to the environment it was given.
const SHELL_OWN: &[&str] = &["PWD", "SHLVL", "_", "OLDPWD"];

/// The gate runs the whole e2e with no test left out (ADR-t2159-1
/// decision 7), from a cleared environment given back only the passed
/// names, the `[run.env]` and its own; a failed run names its failed tests,
/// which run once more by name in a `TMPDIR` and log of their own
/// (ADR-t1165-1), and the checkout's marks are read. Each run's `TMPDIR`
/// is removed after it, and the log names what was given without values.
#[test]
fn the_gate_runs_the_e2e_with_the_narrowed_env_and_reruns_its_failed_tests_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    fs::create_dir_all(dir.join("repo")).unwrap();
    fs::write(
        dir.join("repo").join("dagq.toml"),
        "[run.env]\nGATE_ENV = '${DAGQ_QUEUE_DIR}/x'\n",
    )
    .unwrap();
    let checkout = dir.join("checkout");
    fs::create_dir_all(checkout.join(".config")).unwrap();
    fs::write(
        checkout.join(e2e_quarantine::FILE),
        "[[test]]\nname = \"e2e::b\"\nreason = \"flaky\"\ntask = 1120\nuntil = 2999-12-31\n",
    )
    .unwrap();
    let received = dir.join("received");
    let received = common::shell_path(&received);
    let settings = settings(
        dir,
        &format!(
            "/usr/bin/env >> {received}; echo ---- >> {received}; \
             mkdir -p \"$TMPDIR/.tmpA/data/dagq/q1\"; \
             if [ -n \"${{DAGQ_E2E_RERUN:-}}\" ]; then echo \"rerun [$DAGQ_E2E_RERUN]\"; \
             echo 'test e2e::a ... ok'; echo 'test e2e::b ... FAILED'; exit 101; fi; \
             echo 'test e2e::a ... FAILED'; echo 'test e2e::b ... FAILED'; echo 'test e2e::c ... ok'; \
             echo 'test result: FAILED. 1 passed; 2 failed'; exit 101"
        ),
    );
    let target = dir.join("target");
    let outcome = e2e_gate::run(&checkout, Some(&target), &settings).unwrap();
    assert!(!outcome.passed && !outcome.timed_out, "{outcome:?}");
    assert_eq!(outcome.failed_tests, ["e2e::a", "e2e::b"]);
    // A [run.env] that names no sccache is given as it is.
    assert_eq!(outcome.sccache_wrapper_removed, None);
    assert_eq!(outcome.cleanup["removed"], true, "{outcome:?}");
    let rerun = outcome.rerun.clone().expect("the failed tests are rerun");
    assert_eq!(rerun.tests, ["e2e::a", "e2e::b"]);
    assert_eq!(rerun.failed, ["e2e::b"]);
    assert!(!rerun.timed_out && rerun.error.is_none(), "{rerun:?}");
    assert_eq!(rerun.cleanup["removed"], true, "{rerun:?}");
    assert!(subdirectories(&settings.scratch).is_empty());
    let QuarantineFile::Marks(marks) = &outcome.quarantine else {
        panic!("{:?}", outcome.quarantine);
    };
    assert_eq!(marks[0].name, "e2e::b");

    let received = fs::read_to_string(dir.join("received")).unwrap();
    let passes: Vec<&str> = received.split("----\n").filter(|p| !p.is_empty()).collect();
    assert_eq!(passes.len(), 2, "{received}");
    let queue_value = format!("GATE_ENV={}/x", dir.join("queue").display());
    for (pass, given) in passes.iter().enumerate() {
        for line in given.lines() {
            let name = line.split('=').next().unwrap();
            assert!(
                e2e_gate::passed(name)
                    || SHELL_OWN.contains(&name)
                    || [
                        "TMPDIR",
                        "CARGO_TARGET_DIR",
                        "GATE_ENV",
                        e2e_gate::RERUN_ENV
                    ]
                    .contains(&name),
                "{name} was given: {given}"
            );
        }
        assert!(given.lines().any(|l| l == queue_value), "{given}");
        assert!(
            given
                .lines()
                .any(|l| l == format!("CARGO_TARGET_DIR={}", target.display())),
            "{given}"
        );
        assert!(
            given.lines().any(|l| l.starts_with("TMPDIR=")
                && Path::new(&l["TMPDIR=".len()..]).starts_with(&settings.scratch)),
            "{given}"
        );
        assert!(!given.contains("DAGQ_E2E_SKIP"), "{given}");
        assert_eq!(
            given
                .lines()
                .any(|l| l == format!("{}=e2e::a e2e::b", e2e_gate::RERUN_ENV)),
            pass == 1,
            "{given}"
        );
    }

    let first = fs::read_to_string(&settings.log).unwrap();
    let again = fs::read_to_string(settings.rerun_log()).unwrap();
    assert!(!first.contains("rerun ["), "{first}");
    assert!(again.contains("rerun [e2e::a e2e::b]"), "{again}");
    assert!(
        again.contains("== the rerun by name of the e2e tests that failed in")
            && again.contains(".rerun-"),
        "{again}"
    );
    for log in [first, again] {
        let names = log
            .lines()
            .find_map(|line| line.strip_prefix("== the env passed (names only): "))
            .unwrap_or_else(|| panic!("{log}"));
        let names: Vec<&str> = names.split(' ').collect();
        for name in ["CARGO_TARGET_DIR", "GATE_ENV", "PATH", "TMPDIR"] {
            assert!(names.contains(&name), "{name}: {log}");
        }
        assert!(!log.contains(&queue_value), "{log}");
    }
}

/// Two e2e that take the same host lock (ADR-t1233-2 decision 4) run one
/// after the other: the second waits for the first, and says how long it
/// waited; the lock is a file at the path given.
#[test]
fn e2e_that_take_the_host_lock_run_one_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let busy = common::shell_path(dir.join("busy"));
    let order = common::shell_path(dir.join("order"));
    let command = format!(
        "if mkdir {busy}; then echo start >> {order}; sleep 1.5; echo end >> {order}; \
         rmdir {busy}; else echo overlap >> {order}; fi"
    );
    let lock = dir.join("data").join("e2e.lock");
    let gates: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|name| {
            let mut settings = settings(dir, &command);
            settings.lock = Some(lock.clone());
            settings.scratch = dir.join(format!("scratch-{name}"));
            settings.log = dir.join(format!("{name}.log"));
            settings.run_env_root = None;
            thread::spawn(move || e2e_gate::run(Path::new("/"), None, &settings).unwrap())
        })
        .collect();
    let outcomes: Vec<_> = {
        let _wait = common::within(Duration::from_secs(60), "the two gates to end");
        gates.into_iter().map(|gate| gate.join().unwrap()).collect()
    };
    assert!(
        outcomes.iter().all(|outcome| outcome.passed
            && !outcome.timed_out
            && outcome.failed_tests.is_empty()
            && outcome.rerun.is_none()),
        "{outcomes:?}"
    );
    assert_eq!(
        fs::read_to_string(dir.join("order")).unwrap(),
        "start\nend\nstart\nend\n"
    );
    assert!(
        outcomes.iter().map(|outcome| outcome.lock_wait_secs).max() >= Some(1),
        "{outcomes:?}"
    );
    assert!(lock.exists());
}

/// An earlier gate's directory is cleaned up before the e2e unless its
/// gate still runs: one of another live process, or one of this process
/// whose gate has not ended.
#[test]
fn a_gate_cleans_up_what_a_gone_gate_left_but_not_a_running_one() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let settings = settings(dir, "exit 0");
    // No pid is this large; this test's own is alive.
    let gone = settings.scratch.join("1-4000000000");
    // The parent of this test's process is alive; a root of this
    // process's own is in use only while its gate runs.
    let parent = std::os::unix::process::parent_id();
    let running = settings.scratch.join(format!("1-{parent}"));
    let own_done = settings.scratch.join(format!("2-{}", std::process::id()));
    fs::create_dir_all(&own_done).unwrap();
    fs::create_dir_all(gone.join(".tmpA/data/dagq/q1")).unwrap();
    fs::create_dir_all(&running).unwrap();
    assert!(e2e_gate::run(dir, None, &settings).unwrap().passed);
    assert!(!gone.exists());
    assert!(running.exists());
    assert!(!own_done.exists());
}

/// The gate's cleanup stops the processes whose command line names a path
/// inside the e2e's `TMPDIR` (found by listing the processes), then removes
/// that directory.
#[test]
fn the_e2e_gates_cleanup_stops_what_runs_inside_its_root_and_removes_it() {
    use common::Bounded;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("scratch").join("1-4000000000");
    fs::create_dir_all(root.join(".tmpA/data/dagq/q1")).unwrap();
    let ready = root.join("ready");
    let sleeper = root.join("sleeper");
    // Orphaned on purpose: should the cleanup miss it, it ends by itself
    // once its root is gone (the cleanup's removal or the `TempDir`'s), the
    // test process is gone, or after at most 120 rounds of a second.
    fs::write(
        &sleeper,
        format!(
            "touch {ready}; i=0; while [ \"$i\" -lt 120 ] && [ -d {root} ] && kill -0 {pid} 2>/dev/null; \
             do sleep 1; i=$((i + 1)); done",
            ready = common::shell_path(&ready),
            root = common::shell_path(&root),
            pid = std::process::id()
        ),
    )
    .unwrap();
    // Detached, so that init reaps it once stopped.
    let started = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "/bin/sh {} </dev/null >/dev/null 2>&1 & echo $!",
            common::shell_path(&sleeper)
        ))
        .bounded_output()
        .unwrap();
    let pid: u32 = String::from_utf8_lossy(&started.stdout)
        .trim()
        .parse()
        .unwrap();
    {
        let _wait = common::within(Duration::from_secs(20), "the sleeper to start");
        while !ready.exists() {
            thread::sleep(Duration::from_millis(20));
        }
    }
    let cleanup = e2e_gate::clean_up(&root);
    assert_eq!(cleanup["processes"], json!([pid]), "{cleanup}");
    assert_eq!(cleanup["removed"], true, "{cleanup}");
    assert_eq!(cleanup["errors"], json!([]), "{cleanup}");
    assert!(!root.exists());
}
