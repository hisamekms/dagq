//! Session wrappers a test backend starts in the background (ADR-t1404-1
//! decision 8), each a real process: the plan review's backend runs a
//! headless planner's wrapper this way (`planner_headless`), or, parked,
//! a process that only stands for the wrapper while the test plays the
//! planner itself.
use anyhow::Result;
use dagq::application::ProcessControl;
use dagq::domain::background_wrapper::BackgroundHandle;
use dagq::infrastructure::adapters::SystemProcesses;
use std::{
    fs,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
};

/// One wrapper started in the background: its handle, its command line,
/// its environment and its log.
pub type BackgroundLaunch = (String, String, Vec<(String, String)>, PathBuf);

/// The wrappers a backend started, by handle; what still runs when they
/// are dropped (a test that failed) is stopped with what it started.
#[derive(Default)]
pub struct BackgroundWrappers {
    launched: Mutex<Vec<BackgroundLaunch>>,
    children: Mutex<Vec<(String, Child)>>,
    /// Run, in place of each command (still recorded as launched), a
    /// process that waits while the process of this pid lives: its handle
    /// is real, and the test registers the planner and writes its marker
    /// and turns itself.
    parked: Option<u32>,
    /// The `PATH` each wrapper is started with instead of this process's.
    pub path: Option<String>,
    /// A file removed just before each wrapper starts: the version of
    /// Claude Code an update removes after the supervisor started.
    pub remove_before_launch: Option<PathBuf>,
}

impl BackgroundWrappers {
    /// Wrappers that never run their command and wait while the test's
    /// process lives: see [`Self::parked`].
    pub fn parked() -> Self {
        Self::parked_while(std::process::id())
    }

    /// Wrappers that never run their command and wait while the process
    /// `pid` lives, at most 600 seconds. A test that timed out or was
    /// killed skips the `Drop` that stops them (the timeout's
    /// `process::exit`), so they end by themselves once it is gone
    /// (docs/development/testing.md「待ちの上限」).
    fn parked_while(pid: u32) -> Self {
        Self {
            launched: Mutex::default(),
            children: Mutex::default(),
            parked: Some(pid),
            path: None,
            remove_before_launch: None,
        }
    }

    /// Start `command` as a process of its own in `cwd`, with `env` over
    /// this process's environment less its dagq and cmux variables (as the
    /// cmux adapter starts it) and its output in `log`; its handle is its
    /// pid and start.
    pub fn launch(
        &self,
        cwd: &Path,
        command: &str,
        env: &[(String, String)],
        log: &Path,
    ) -> Result<String> {
        let mut process = Command::new("/bin/sh");
        if let Some(pid) = self.parked {
            process.arg("-c").arg(format!(
                "n=0; while [ \"$n\" -lt 600 ] && kill -0 {pid} 2>/dev/null; do sleep 1; n=$((n + 1)); done"
            ));
        } else {
            process.arg("-c").arg(format!("exec {command}"));
        }
        for (name, _) in std::env::vars() {
            if name.starts_with("DAGQ_") || name.starts_with("CMUX_") {
                process.env_remove(name);
            }
        }
        if let Some(path) = &self.path {
            process.env("PATH", path);
        }
        if let Some(gone) = &self.remove_before_launch {
            let _ = fs::remove_file(gone);
        }
        let output = fs::File::create(log)?;
        let child = process
            .envs(env.iter().map(|(k, v)| (k, v)))
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output)
            .process_group(0)
            .spawn()?;
        let start = SystemProcesses
            .start_identity(child.id())
            .expect("the wrapper's start");
        let handle = BackgroundHandle::new(child.id(), &start).to_string();
        self.launched.lock().unwrap().push((
            handle.clone(),
            command.to_owned(),
            env.to_vec(),
            log.to_owned(),
        ));
        self.children.lock().unwrap().push((handle.clone(), child));
        Ok(handle)
    }

    /// Every wrapper started, in order.
    pub fn launched(&self) -> Vec<BackgroundLaunch> {
        self.launched.lock().unwrap().clone()
    }

    /// Whether the wrapper `handle` still runs (its process reaped once it
    /// ended).
    pub fn runs(&self, handle: &str) -> bool {
        self.children
            .lock()
            .unwrap()
            .iter_mut()
            .any(|(id, child)| id == handle && matches!(child.try_wait(), Ok(None)))
    }

    /// Stop the wrapper `handle` and what it started, if it still runs.
    pub fn stop(&self, handle: &str) {
        for (id, child) in self.children.lock().unwrap().iter_mut() {
            if id == handle {
                stop(child);
            }
        }
    }
}

impl Drop for BackgroundWrappers {
    fn drop(&mut self) {
        for (_, child) in self.children.lock().unwrap().iter_mut() {
            stop(child);
        }
    }
}

/// `child` and its descendants, killed while it runs, and reaped.
fn stop(child: &mut Child) {
    if matches!(child.try_wait(), Ok(None)) {
        for pid in SystemProcesses.descendants(child.id()) {
            let _ = SystemProcesses.kill(pid);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[test]
fn a_parked_wrapper_waits_while_its_test_lives_and_ends_once_it_is_gone() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut gone = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    gone.wait().unwrap();
    let orphaned = BackgroundWrappers::parked_while(gone.id());
    let left = orphaned
        .launch(
            dir.path(),
            "dagq session",
            &[],
            &dir.path().join("left.log"),
        )
        .unwrap();
    let live = BackgroundWrappers::parked();
    let parked = live
        .launch(
            dir.path(),
            "dagq session",
            &[],
            &dir.path().join("live.log"),
        )
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while orphaned.runs(&left) {
        assert!(
            std::time::Instant::now() < deadline,
            "a parked wrapper whose test is gone still runs after 10 seconds"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // Past a turn of its loop, the wrapper of a live test still waits.
    std::thread::sleep(std::time::Duration::from_millis(1200));
    assert!(live.runs(&parked));
    live.stop(&parked);
    assert!(!live.runs(&parked));
}
