//! The fixture and the fakes for launchd and process signals for the
//! lifecycle tests (`tests/lifecycle_*.rs`).

use super::Bounded;
use dagq::domain::LeaseToken;
use dagq::infrastructure::git_binary::git_executable;

use anyhow::{Result, bail};
use dagq::{
    VERSION,
    application::{AgentState, LaunchAgent, ProcessControl},
    domain::{SupervisorMode, recovery::ProcessInfo},
    infrastructure::{location::QueueLocation, sqlite::SqliteQueue},
    lifecycle::{self, DownOptions, UpEnvironment, UpOptions},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    thread,
    time::Duration,
};
use tempfile::TempDir;

pub fn git(repo: &Path, args: &[&str]) {
    let result = Command::new(git_executable().expect("git executable"))
        .arg("-C")
        .arg(repo)
        .args(args)
        .bounded_output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

/// Disposable repository, queue, home and stub `claude`, all under one temp dir.
pub struct Fixture {
    pub _dir: TempDir,
    pub repo: PathBuf,
    pub location: QueueLocation,
    pub environment: UpEnvironment,
    pub options: UpOptions,
    /// Times the test while held (task 324).
    pub _test: super::Waiting,
}

pub fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("my repo");
    fs::create_dir(&repo).unwrap();
    super::template::repository(&repo, "fixture\n");
    let db = dir.path().join("queue's dir").join("queue.db");
    let home = dir.path().join("home");
    let location = QueueLocation::explicit_in(&db, &home);
    location.prepare().unwrap();
    super::template::queue(&db);
    let claude = dir.path().join("claude-stub");
    // `plugin list --json` prints `<stub>.plugins` (see `list_plugins`),
    // and fails when there is none.
    crate::common::template::script(
        &claude,
        "#!/bin/sh\nif [ \"$1\" = plugin ]; then printf '%s\\n' \"$*\" > \"$0.plugin-args\"; \
pwd >> \"$0.plugin-args\"; exec cat \"$0.plugins\"; fi\nprintf 'claude-stub 0.0.0\\n'\n",
    );

    // Only passed on to the supervisor's command line: no cmux is there.
    let cmux = dir.path().join("cmux-absent");

    // Claude Code has accepted the folder trust dialog at the repository root.
    let claude_config = dir.path().join("claude.json");
    let root = repo.canonicalize().unwrap();
    fs::write(
        &claude_config,
        json!({"projects": {root.to_str().unwrap(): {"hasTrustDialogAccepted": true}}}).to_string(),
    )
    .unwrap();
    Fixture {
        repo,
        location,
        _test: super::test(),
        environment: UpEnvironment {
            role: None,
            queue: None,
            path: "/usr/bin:/bin:/home/u/.local/bin".into(),
            config_home: None,
            current_exe: "/opt/bin/dagq".into(),
            claude_config: Some(claude_config),
            user_config: None,
            restart: false,
        },
        options: UpOptions {
            no_claude: false,
            parallel: Some(2),
            max_waiting: None,
            runtime_planners: None,
            max_load: None,
            in_cmux: false,
            no_wait: false,
            plugin_dir: Some(dir.path().to_path_buf()),
            cmux,
            claude,
            codex: "/opt/bin/codex".into(),
            startup_timeout: Duration::from_secs(5),
            handoff_timeout: Duration::from_secs(5),
            auto_update: false,
            // The queue service's start has its own tests (queue_service).
            queue_service: false,
            poll: Duration::from_millis(20),
        },
        _dir: dir,
    }
}

/// Have the fixture's `claude` print `listed` for `plugin list --json`.
pub fn list_plugins(fixture: &Fixture, listed: &str) {
    let mut path = fixture.options.claude.clone().into_os_string();
    path.push(".plugins");
    fs::write(path, listed).unwrap();
}

/// Records installs and uninstalls; an install registers a supervisor with
/// this process's PID, the way a started `supervise` would within seconds.
pub struct FakeLaunchd {
    pub db: PathBuf,
    pub registers_on_install: bool,
    /// An install makes every registration already there heartbeat again
    /// first, the way an alive but silent supervisor can come back while
    /// `up` waits for the one it started.
    pub heartbeats_existing_on_install: bool,
    pub loaded: Mutex<bool>,
    /// The agent's process while loaded, as `launchctl print` would show it.
    pub agent_pid: Mutex<Option<u32>>,
    pub installs: Mutex<Vec<(String, PathBuf, String)>>,
    pub uninstalls: Mutex<Vec<(String, PathBuf)>>,
}

impl FakeLaunchd {
    pub fn new(db: &Path) -> Self {
        Self {
            db: db.into(),
            registers_on_install: true,
            heartbeats_existing_on_install: false,
            loaded: Mutex::new(false),
            agent_pid: Mutex::new(None),
            installs: Mutex::new(Vec::new()),
            uninstalls: Mutex::new(Vec::new()),
        }
    }
    pub fn load(&self, pid: Option<u32>) {
        *self.loaded.lock().unwrap() = true;
        *self.agent_pid.lock().unwrap() = pid;
    }
}

impl LaunchAgent for FakeLaunchd {
    fn install(&self, label: &str, path: &Path, contents: &str) -> Result<()> {
        self.installs
            .lock()
            .unwrap()
            .push((label.into(), path.into(), contents.into()));
        self.load(Some(std::process::id()));
        let mut queue = SqliteQueue::open(&self.db)?;
        if self.heartbeats_existing_on_install {
            for registration in queue.supervisors()? {
                queue.heartbeat(&registration.token)?;
            }
        }
        if self.registers_on_install {
            queue.register_supervisor(
                &LeaseToken::new(uuid::Uuid::new_v4().to_string()),
                std::process::id(),
                2,
                VERSION,
            )?;
        }
        Ok(())
    }
    fn uninstall(&self, label: &str, path: &Path) -> Result<AgentState> {
        self.uninstalls
            .lock()
            .unwrap()
            .push((label.into(), path.into()));
        let loaded = std::mem::replace(&mut *self.loaded.lock().unwrap(), false);
        let pid = self.agent_pid.lock().unwrap().take();
        Ok(AgentState {
            loaded,
            pid: if loaded { pid } else { None },
        })
    }
}

/// Liveness by an explicit dead set, which only the test writes: a signal
/// never moves a PID into it, the way a real signal does not make the
/// process disappear by the next syscall.
#[derive(Default)]
pub struct FakeProcesses {
    pub dead: Mutex<HashSet<u32>>,
    pub terminated: Mutex<Vec<u32>>,
    pub interrupted: Mutex<Vec<u32>>,
    pub killed: Mutex<Vec<u32>>,
    /// This user's processes as `list` gives them; `None` fails the
    /// listing, the way a process control that cannot list does.
    pub listed: Mutex<Option<Vec<ProcessInfo>>>,
    /// How many times each pid was asked whether it lives: a test waits
    /// for the looks of a handoff and its watch instead of a fixed sleep
    /// (task 1048).
    pub looks: Mutex<HashMap<u32, usize>>,
}

impl FakeProcesses {
    /// How many times `pid` was asked whether it lives.
    pub fn looks_at(&self, pid: u32) -> usize {
        self.looks.lock().unwrap().get(&pid).copied().unwrap_or(0)
    }
}

impl ProcessControl for FakeProcesses {
    fn alive(&self, pid: u32) -> bool {
        *self.looks.lock().unwrap().entry(pid).or_default() += 1;
        !self.dead.lock().unwrap().contains(&pid)
    }
    fn terminate(&self, pid: u32) -> Result<()> {
        self.terminated.lock().unwrap().push(pid);
        Ok(())
    }
    fn interrupt(&self, pid: u32) -> Result<()> {
        self.interrupted.lock().unwrap().push(pid);
        Ok(())
    }
    // SIGKILL returns before the target is reaped, so `kill -0` still
    // succeeds right after it; the fake does not pretend otherwise.
    fn kill(&self, pid: u32) -> Result<()> {
        self.killed.lock().unwrap().push(pid);
        Ok(())
    }
    fn list(&self) -> Result<Vec<ProcessInfo>> {
        match &*self.listed.lock().unwrap() {
            Some(listed) => Ok(listed.clone()),
            None => bail!("the fake lists no processes"),
        }
    }
}

pub fn up(fixture: &Fixture, launchd: &FakeLaunchd, processes: &FakeProcesses) -> Value {
    lifecycle::up(
        &fixture.location,
        &fixture.repo,
        launchd,
        processes,
        &fixture.environment,
        &fixture.options,
    )
    .unwrap()
}

pub fn try_up(
    fixture: &Fixture,
    launchd: &FakeLaunchd,
    processes: &FakeProcesses,
) -> Result<Value> {
    lifecycle::up(
        &fixture.location,
        &fixture.repo,
        launchd,
        processes,
        &fixture.environment,
        &fixture.options,
    )
}

/// Poll `condition` until it holds. On the deadline the supervisor's pid
/// is marked dead before the panic: `up`'s drain loop is otherwise
/// unbounded, so a helper thread that merely panicked would leave the main
/// thread waiting for a registration nothing is going to remove, and
/// `thread::scope` would never join.
pub fn wait_until(processes: &FakeProcesses, pid: u32, condition: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !condition() {
        if std::time::Instant::now() >= deadline {
            processes.dead.lock().unwrap().insert(pid);
            panic!("the condition never held; released the drain so the test can fail");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// The mode recorded on a registration, read straight from the queue.
pub fn remaining_mode(queue: &SqliteQueue, token: &str) -> Option<SupervisorMode> {
    queue
        .supervisors()
        .unwrap()
        .into_iter()
        .find(|registration| registration.token == token)
        .and_then(|registration| registration.mode)
}

pub fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// The `backend_call_failed` events of the fixture's queue, oldest first.
pub fn backend_failures(fixture: &Fixture) -> Vec<dagq::domain::RunEvent> {
    SqliteQueue::open(&fixture.location.db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "backend_call_failed")
        .collect()
}

pub fn down(
    fixture: &Fixture,
    launchd: &FakeLaunchd,
    processes: &FakeProcesses,
    wait: bool,
    force: bool,
) -> Value {
    lifecycle::down(
        &fixture.location,
        launchd,
        processes,
        &DownOptions {
            wait,
            force,
            poll: Duration::from_millis(20),
        },
    )
    .unwrap()
}

/// A registration of an older build that takes a handoff (ADR-0045
/// decision 10), with a run in flight under its token.
pub fn handoff_supervisor(fixture: &Fixture, token: &str, mode: SupervisorMode) -> SqliteQueue {
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    queue
        .register_supervisor(&LeaseToken::new(token), std::process::id(), 4, "0.0.1")
        .unwrap();
    queue.accept_handoff(&LeaseToken::new(token)).unwrap();
    queue
        .set_supervisor_mode(&LeaseToken::new(token), mode, None)
        .unwrap();
    queue
}
