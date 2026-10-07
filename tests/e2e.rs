//! End-to-end happy paths through the real binary, real Git, real cmux and
//! real launchd, from `add` to the squash landing by `integrate`, from
//! `up` to `down`, and from a killed supervisor to the adoption of its run.
//! Claude is replaced by a stub script whose worker turns do what the
//! prompt asks: change, commit, write the receipt; a later turn of the
//! session takes a person's answer or the supervisor's resolution request as
//! its prompt and resolves the conflict. Requires a running cmux, so it is ignored by
//! default: `cargo test --locked --test e2e -- --ignored --nocapture`.
//!
//! The launchd `up` / `down` test is temporarily off even under `--ignored`:
//! no project runs the launchd mode now, and without a cmux socket password
//! its preflight always stops `up`. It returns at once, printing why, unless
//! `DAGQ_E2E_LAUNCHD=1` is set; the in-cmux `up` / `down` test is out for
//! now under ADR-t1582-1 (see the last paragraph).
//!
//! ADR-t1582-1 keeps one case, and the helpers only it uses, out under `#[cfg(any())]`.
#[path = "e2e/broker.rs"]
mod broker;
#[path = "e2e/cleanup.rs"]
mod cleanup;
mod common;
#[path = "e2e/headless.rs"]
mod headless;
#[path = "e2e/other_repository.rs"]
mod other_repository;
#[path = "e2e/stub.rs"]
mod stub;

use cleanup::{
    GroupGuard, WorkspaceGuard, claim_fixture_dir, cmux_retrying, listed_group,
    sweep_abandoned_fixtures, workspace_listed,
};
#[cfg(any())] // Goes with task 1443 (ADR-t1582-1).
use cleanup::{
    cmux_attempt, listed_workspace, try_listed_workspace, wait_for_listed, wait_until_not_listed,
};
use common::{Bounded, Cleanup, Waiting, WithoutActor};
use dagq::application::ProcessControl;
use dagq::domain::background_wrapper::BackgroundHandle;
use dagq::infrastructure::adapters::SystemProcesses;
use dagq::infrastructure::git_binary::git_executable;
use serde_json::{Value, json};
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_dagq");
/// The version the binary under test records on its registration.
const VERSION: &str = dagq::VERSION;
/// How long a test waits for a supervisor to take a run through a pass
/// (claim, workspace, session, receipt, review, exit request, landing). A pass
/// took 26 to 90 s at load average 20 to 40 in the auto-update's e2e gate
/// (2026-09-29 and 30, task 1008), and a `/exit` cmux lost takes the
/// supervisor's 120 s exit-request timeout and a retry on top of that: the
/// step limit covers both, so a wait fails only when the pass stops.
const SUPERVISE_TIMEOUT: Duration = common::STEP_LIMIT;
/// Passed to every `supervise` the tests start: the load of the host (other
/// e2e tests running in parallel, other runs) must not hold back the claims
/// a test waits for.
const NO_LOAD_HOLD: [&str; 2] = ["--max-load", "0"];
/// A whole e2e test, which the fixture holds: well above the longest test's
/// own deadlines (a few supervise passes of [`SUPERVISE_TIMEOUT`] each), so
/// those fail first with their own message. The waits inside a test are
/// timed with [`common::STEP_LIMIT`].
const TEST_LIMIT: Duration = Duration::from_secs(1800);
/// How long the timeout cleanup of a fixture's cmux group or of a supervisor
/// the test started may take before the test binary exits without it.
const CLEANUP_LIMIT: Duration = Duration::from_secs(60);
/// How long a test polls for a state it waits to reach (a status, a
/// planner's state, a pin in cmux's listing, a process's exit, a landing):
/// one value for every such wait, long enough for a loaded host (load avg
/// 10 and more, task 641), so a wait fails only when the state never comes.
/// The poll ends as soon as the state holds.
pub(crate) const WAIT_LIMIT: Duration = common::STEP_LIMIT;

const E2E_DAGQ_TOML: &str =
    "[run.env]\nE2E_SHARED = '${DAGQ_QUEUE_DIR}/shared'\nE2E_RUN_DIR = '${DAGQ_RUN_DIR}'\n";

/// A verification command that records the `[run.env]` it ran with in the
/// run directory it names, and fails without it.
const VERIFY_RUN_ENV: &str =
    r#"printf 'verify env: %s\n' "$E2E_SHARED" >> "${E2E_RUN_DIR:?}/verify-env.txt""#;

fn cmux_executable() -> PathBuf {
    env::var_os("DAGQ_E2E_CMUX")
        .map(PathBuf::from)
        .unwrap_or_else(|| "cmux".into())
}

/// Fail loudly, never skip, when cmux is missing: the test would prove nothing.
fn preflight(cmux: &Path) -> String {
    let hint = "the e2e test needs a running cmux; put cmux on PATH or set DAGQ_E2E_CMUX";
    let pong = |output: &std::process::Output| {
        output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "PONG"
    };
    let ping = cmux_retrying(cmux, &["ping"], pong)
        .unwrap_or_else(|error| panic!("cannot run {}: {error}; {hint}", cmux.display()));
    assert!(
        pong(&ping),
        "cmux ping failed ({}): {}{}; {hint}",
        ping.status,
        String::from_utf8_lossy(&ping.stdout),
        String::from_utf8_lossy(&ping.stderr)
    );
    let version = Command::new(cmux)
        .arg("--version")
        .bounded_output()
        .unwrap();
    String::from_utf8_lossy(&version.stdout).trim().to_owned()
}

fn git(repo: &Path, args: &[&str]) -> String {
    let result = Command::new(git_executable().expect("git executable"))
        .arg("-C")
        .arg(repo)
        .args(args)
        .bounded_output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}

/// Record in `home`'s Claude Code config that the folder trust dialog was
/// accepted at `repo`, as `up` requires before it starts anything.
fn trust_repository(home: &Path, repo: &Path) {
    let root = repo.canonicalize().unwrap();
    fs::write(
        home.join(".claude.json"),
        json!({"projects": {root.to_str().unwrap(): {"hasTrustDialogAccepted": true}}}).to_string(),
    )
    .unwrap();
}

/// The queue is resolved the way a user's shell would: from the repository as
/// the working directory, with `XDG_DATA_HOME` pointed at the disposable
/// directory instead of the developer's real data home.
struct Env {
    repo: PathBuf,
    data_home: PathBuf,
}

fn dagq(env: &Env, args: &[&str]) -> Value {
    dagq_with(env, &[], args)
}

/// `dagq status` once `condition` holds of it: what `status` reports of
/// processes that just exited or were just killed is waited for rather than
/// judged on the first look. Past the deadline the last status is returned,
/// for the caller's assertions to show.
fn status_when(env: &Env, condition: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        let status = dagq(env, &["status"]);
        if condition(&status) || Instant::now() >= deadline {
            return status;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn dagq_with(env: &Env, extra: &[(&str, &Path)], args: &[&str]) -> Value {
    checked(args, dagq_output(env, extra, args))
}

/// [`dagq_with`] for a command that opens workspaces (`up`): every
/// workspace its output names goes into `guard` before anything about the
/// output is checked, so a failing command or assertion still has them
/// closed when the test ends.
fn dagq_opening(
    env: &Env,
    extra: &[(&str, &Path)],
    args: &[&str],
    guard: &mut WorkspaceGuard,
) -> Value {
    let output = dagq_output(env, extra, args);
    guard.record_opened(&output.stdout);
    checked(args, output)
}

fn dagq_output(env: &Env, extra: &[(&str, &Path)], args: &[&str]) -> std::process::Output {
    let mut command = Command::new(BIN);
    // A person's commands, not the actor's of the session running the tests.
    command.without_actor_env();
    command
        .current_dir(&env.repo)
        .env("XDG_DATA_HOME", &env.data_home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .args(args);
    for (key, value) in extra {
        command.env(key, value);
    }
    command.bounded_output().unwrap()
}

/// [`dagq`] with its stdout and stderr in files of `env`'s data home
/// rather than pipes, for a thread that runs commands while the test spawns
/// long-lived processes (the queue service, the supervisor). On macOS a
/// pipe becomes close-on-exec only after it is made, so a process spawned
/// by another thread in between inherits its write end and keeps
/// `Command::output` waiting for an end of file until that process exits.
/// A file has no end to wait for.
fn dagq_in_files(env: &Env, args: &[&str]) -> Value {
    let stdout = tempfile::tempfile_in(&env.data_home).unwrap();
    let stderr = tempfile::tempfile_in(&env.data_home).unwrap();
    let mut command = Command::new(BIN);
    command.without_actor_env();
    let status = command
        .current_dir(&env.repo)
        .env("XDG_DATA_HOME", &env.data_home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .args(args)
        .stdin(Stdio::null())
        .stdout(stdout.try_clone().unwrap())
        .stderr(stderr.try_clone().unwrap())
        .bounded_status()
        .unwrap();
    let read = |mut file: fs::File| {
        use std::io::Seek;
        file.rewind().unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        bytes
    };
    checked(
        args,
        std::process::Output {
            status,
            stdout: read(stdout),
            stderr: read(stderr),
        },
    )
}

fn checked(args: &[&str], output: std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "dagq {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The workspace is pinned, has the sidebar color `color` (cmux lists a
/// named color by its hex value) and carries the status pill `pill` as
/// `cmux list-status` prints it. cmux answers `workspace-action` before its
/// listing shows the change: with other e2e tests driving cmux, a pin sent
/// right after an unpin is listed as `pinned: false` for up to ~0.5s (task
/// 1120), so the look is waited for, not read once. A failed listing or
/// `list-status` (cmux's `Command timed out` under load) is "not yet" too.
#[cfg(any())] // Goes with task 1443 (ADR-t1582-1).
fn assert_look(cmux: &Path, id: &str, color: &str, pill: &str) {
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        let listed = match try_listed_workspace(cmux, id) {
            Ok(Some(listed)) => Ok(listed),
            Ok(None) => Err("not listed".to_owned()),
            Err(error) => Err(format!("listing failed: {error:#}")),
        };
        let status = cmux_attempt(cmux, &["list-status", "--workspace", id]);
        if let (Ok(listed), Ok(status)) = (&listed, &status)
            && listed["pinned"] == true
            && listed["custom_color"] == color
            && status.lines().any(|line| line == pill)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "workspace {id} never got its look: {listed:?}\n{status:?}"
        );
        thread::sleep(Duration::from_millis(200));
    }
}

/// `cmux workspace env <id> --json`: the environment the workspace was
/// created with.
#[cfg(any())] // Goes with task 1443 (ADR-t1582-1).
fn workspace_env(cmux: &Path, id: &str) -> Value {
    let output = Command::new(cmux)
        .args(["workspace", "env", id, "--json"])
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "cmux workspace env {id}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<Value>(&output.stdout).unwrap()["env"].clone()
}

/// Kills a still-running supervisor when an assertion fails mid-run, and
/// when a wait times out and the test binary exits without unwinding.
struct ChildGuard(Child, Option<Cleanup>);

impl ChildGuard {
    fn new(child: Child) -> Self {
        let pid = child.id();
        let cleanup = common::on_timeout(CLEANUP_LIMIT, format!("kill process {pid}"), move || {
            // SAFETY: kill(2) takes no pointers. The hook is unregistered
            // once the test reaps the child or the guard drops.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        });
        Self(child, Some(cleanup))
    }

    /// The child was waited for: its pid is no longer ours to kill on a
    /// timeout.
    fn reaped(&mut self) {
        self.1 = None;
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// Join a thread the test started, within [`common::STEP_LIMIT`].
fn joined<T>(handle: thread::JoinHandle<T>, what: &str) -> T {
    let _waiting = common::within(common::STEP_LIMIT, format!("{what} to return"));
    handle.join().unwrap()
}

fn reader(mut source: impl Read + Send + 'static) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut text = String::new();
        source.read_to_string(&mut text).unwrap();
        text
    })
}

/// The directory name of every fixture's disposable repository. The runtime
/// names a queue's workspace group and titles after the repository's
/// directory (`[dagq-e2e]`, `[dagq-e2e]worker#…`), so a group an e2e leaves
/// in cmux says where it came from (task 710; the manual smoke's
/// repositories are named in docs/design/manual-smoke.md).
const E2E_REPO_NAME: &str = "dagq-e2e";

/// Disposable repository, queue and stub agent, all outside this repository.
struct Fixture {
    /// Declared first so that it drops first, while the run directories
    /// its wrappers run from are still there.
    wrappers: WrapperGuard,
    group: GroupGuard,
    _dir: tempfile::TempDir,
    cmux: PathBuf,
    repo: PathBuf,
    stub: PathBuf,
    base: String,
    db: PathBuf,
    env: Env,
    /// Held for the fixture's lifetime, and dropped after `_dir` is gone;
    /// see [`OWNER_FILE`].
    _owner: fs::File,
    _test: Waiting,
}

fn fixture() -> Fixture {
    fixture_on("main", &[])
}

/// [`fixture`] whose repository's default branch is `branch` and whose seed
/// commit also has `files` (path, content) besides `seed.txt` and
/// `dagq.toml`.
fn fixture_on(branch: &str, files: &[(&str, &str)]) -> Fixture {
    let test = common::within(TEST_LIMIT, "the test to finish");
    let cmux = cmux_executable();
    let cmux_version = preflight(&cmux);
    eprintln!("cmux: {cmux_version}");
    sweep_abandoned_fixtures(&cmux);
    let dir = tempfile::tempdir().unwrap();
    let owner = claim_fixture_dir(dir.path());
    let repo = dir.path().join(E2E_REPO_NAME);
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", branch]);
    git(&repo, &["config", "user.name", "e2e"]);
    git(&repo, &["config", "user.email", "e2e@example.invalid"]);
    fs::write(repo.join("seed.txt"), "fixture\n").unwrap();
    // ADR-0023 decision 3: every run gets this env in its workspace and
    // its verification commands.
    fs::write(repo.join("dagq.toml"), E2E_DAGQ_TOML).unwrap();
    for (path, content) in files {
        let path = repo.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "seed"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let stub = dir.path().join("claude-stub");
    fs::write(&stub, stub::STUB).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    let env = Env {
        repo: repo.clone(),
        data_home: dir.path().join("data"),
    };
    let init = dagq(&env, &["init"]);
    assert_eq!(
        init["schema_version"],
        dagq::infrastructure::sqlite::SqliteQueue::SCHEMA_VERSION
    );
    let db = PathBuf::from(init["db"].as_str().unwrap());
    assert!(db.starts_with(env.data_home.join("dagq")));
    assert_eq!(dagq(&env, &["locate"])["db_exists"], true);
    Fixture {
        wrappers: WrapperGuard::default(),
        // A repository queue's directory is named after the queue hash,
        // which is the external ID of its workspace group.
        group: GroupGuard::new(
            cmux.clone(),
            db.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned(),
        ),
        _dir: dir,
        cmux,
        repo,
        stub,
        base,
        db,
        env,
        _owner: owner,
        _test: test,
    }
}

impl Fixture {
    fn group(&self) -> Option<Value> {
        listed_group(&self.cmux, &self.group.external_id)
    }
}

/// Register a ready task whose acceptance the stub agent satisfies.
fn add_ready_task(env: &Env, title: &str, dependencies: &[&str]) -> String {
    add_ready_task_verifying(env, title, dependencies, &[])
}

fn add_ready_task_verifying(
    env: &Env,
    title: &str,
    dependencies: &[&str],
    verify: &[&str],
) -> String {
    add_ready_task_described(
        env,
        title,
        "Add e2e.txt to the worktree",
        dependencies,
        verify,
    )
}

fn add_ready_task_described(
    env: &Env,
    title: &str,
    description: &str,
    dependencies: &[&str],
    verify: &[&str],
) -> String {
    // The stub worker plays Claude's headless turns, the default worker.
    let mut args = vec![
        "add",
        title,
        "--description",
        description,
        "--acceptance",
        "e2e.txt is committed and seed.txt still exists",
        "--verify",
        "test -f seed.txt",
        "--verify",
        "test -f e2e.txt",
    ];
    for command in verify {
        args.extend(["--verify", command]);
    }
    for dependency in dependencies {
        args.extend(["--depends-on", dependency]);
    }
    let id = dagq(env, &args)["id"].to_string();
    assert_eq!(
        dagq(env, &["ready", &id, "--bypass-review"])["status"],
        "ready"
    );
    id
}

/// What one `supervise` pass produced, plus what the test observed while it ran.
struct Pass {
    outcome: Value,
    stderr: String,
    /// The background wrapper's handle per task, in the order they were
    /// first seen: a run opens no workspace (ADR-t1433-3).
    sessions: Vec<(String, String)>,
    /// Whether every wrapper ran at one moment; for one task this is
    /// simply "it ran".
    running_together: bool,
}

/// The handle `id` a run recorded as its session: a run's session wrapper
/// starts in the background, never in a workspace (ADR-t1433-3).
pub(crate) fn wrapper_handle(id: &str) -> BackgroundHandle {
    BackgroundHandle::parse(id)
        .unwrap_or_else(|| panic!("the run's session {id} is not a background wrapper's handle"))
}

/// Whether the background wrapper `id` runs: its pid shows the start the
/// supervisor recorded for it (ADR-t1404-1 decision 2).
pub(crate) fn wrapper_running(id: &str) -> bool {
    let handle = wrapper_handle(id);
    handle.is(
        handle.pid,
        SystemProcesses.start_identity(handle.pid).as_deref(),
    )
}

/// Wait until the background wrapper `id` has ended: the runtime has it end
/// after its session and stops what is left of it (ADR-t1404-1 decision 3).
pub(crate) fn wait_until_wrapper_gone(id: &str) {
    let deadline = Instant::now() + WAIT_LIMIT;
    while wrapper_running(id) {
        assert!(
            Instant::now() < deadline,
            "the background wrapper {id} still runs {WAIT_LIMIT:?} after its session ended"
        );
        thread::sleep(Duration::from_millis(200));
    }
}

/// Stops, when the test ends, the background wrappers of the runs it saw
/// that still run: a run opens no workspace for [`WorkspaceGuard`] to
/// close (ADR-t1433-3). Each recorded wrapper is also stopped when a wait
/// times out and the test binary exits without unwinding (the hook of
/// [`common::on_timeout`], as for the supervisor child and the group).
#[derive(Default)]
pub(crate) struct WrapperGuard(std::sync::Mutex<Vec<(String, Cleanup)>>);

impl WrapperGuard {
    /// Stop the background wrapper `id` when the test ends or times out;
    /// an ID that is no background wrapper's handle is left alone.
    pub(crate) fn record(&self, id: &str) {
        if BackgroundHandle::parse(id).is_none() {
            return;
        }
        let mut ids = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !ids.iter().any(|(known, _)| known == id) {
            let owned = id.to_owned();
            let cleanup = common::on_timeout(
                CLEANUP_LIMIT,
                format!("stop the background wrapper {id}"),
                move || stop_wrapper(&owned),
            );
            ids.push((id.to_owned(), cleanup));
        }
    }
}

impl Drop for WrapperGuard {
    fn drop(&mut self) {
        let ids = std::mem::take(self.0.get_mut().unwrap_or_else(|e| e.into_inner()));
        for (id, _cleanup) in &ids {
            stop_wrapper(id);
        }
    }
}

/// Stop the background wrapper `id` while it still runs: SIGTERM first,
/// which the wrapper passes on to the groups of its turns, then SIGKILL to
/// its group and itself after 3 seconds.
fn stop_wrapper(id: &str) {
    if !wrapper_running(id) {
        return;
    }
    eprintln!("stopping the background wrapper {id} the test left running");
    let pid = wrapper_handle(id).pid as libc::pid_t;
    // SAFETY: kill(2) takes no pointers; the pid shows the start the
    // supervisor recorded for this wrapper.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(3);
    while wrapper_running(id) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    if wrapper_running(id) {
        // SAFETY: as above; the wrapper leads its own group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

/// Stops the queue's service [`supervise_once`] started when it goes.
struct ServiceGuard<'a>(&'a Env);

impl Drop for ServiceGuard<'_> {
    fn drop(&mut self) {
        let _ = dagq_output(self.0, &[], &["service", "stop"]);
    }
}

/// Run `supervise --once` with the given extra arguments and watch the runs of
/// `tasks` until it exits: the handles of their background wrappers must
/// appear in the queue and the wrappers run before the sessions end. The
/// stub agents of the pass keep their sessions until the test has seen that
/// (the `watching` and `listed` files in the run env's `E2E_SHARED`), so it
/// does not depend on how fast the host is (task 641).
fn supervise_once(fixture: &Fixture, extra: &[&str], tasks: &[&str]) -> Pass {
    let shared = fixture.db.with_file_name("shared");
    fs::create_dir_all(&shared).unwrap();
    let watching = shared.join("watching");
    let listed = shared.join("listed");
    let _ = fs::remove_file(&listed);
    fs::write(&watching, "").unwrap();
    // The queue's service, which `up` starts and a worker's `dagq` goes to
    // in client mode (goal 82's stage (3)); a one-shot `supervise` keeps
    // none of its own. It is stopped when the pass ends.
    dagq(
        &fixture.env,
        &["service", "start", "--cmux", fixture.cmux.to_str().unwrap()],
    );
    let _service = ServiceGuard(&fixture.env);
    let started = Instant::now();
    let mut child = ChildGuard::new(
        Command::new(BIN)
            // A person's supervisor, not the session's running the tests.
            .without_actor_env()
            .current_dir(&fixture.repo)
            .env("XDG_DATA_HOME", &fixture.env.data_home)
            .arg("supervise")
            .arg("--once")
            .args(NO_LOAD_HOLD)
            .args(extra)
            .arg("--cmux")
            .arg(&fixture.cmux)
            .arg("--claude")
            .arg(&fixture.stub)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = reader(child.0.stdout.take().unwrap());
    let stderr = reader(child.0.stderr.take().unwrap());
    let mut sessions: Vec<(String, String)> = Vec::new();
    let mut running_together = false;
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            child.reaped();
            break status;
        }
        if started.elapsed() >= SUPERVISE_TIMEOUT {
            let _ = child.0.kill();
            let _ = child.0.wait();
            child.reaped();
            let stderr = joined(stderr, "the supervisor's stderr reader");
            panic!("supervise did not finish within {SUPERVISE_TIMEOUT:?}; its stderr:\n{stderr}");
        }
        for task in tasks {
            if sessions.iter().any(|(t, _)| t == task) {
                continue;
            }
            let detail = dagq(&fixture.env, &["show", task, "--full"]);
            if let Some(id) = detail["runs"]
                .as_array()
                .unwrap()
                .last()
                .and_then(|r| r["workspace_id"].as_str())
            {
                wrapper_handle(id);
                fixture.wrappers.record(id);
                eprintln!(
                    "task {task} background wrapper {id} registered after {:?}",
                    started.elapsed()
                );
                sessions.push((task.to_string(), id.to_owned()));
            }
        }
        if sessions.len() == tasks.len() && !running_together {
            running_together = sessions.iter().all(|(_, id)| wrapper_running(id));
            if running_together {
                fs::write(&listed, "").unwrap();
            }
        }
        thread::sleep(Duration::from_millis(200));
    };
    let supervise_took = started.elapsed();
    fs::remove_file(&watching).unwrap();
    let stdout = joined(stdout, "the supervisor's stdout reader");
    let stderr = joined(stderr, "the supervisor's stderr reader");
    eprintln!("supervise finished in {supervise_took:?}\n{stderr}");
    assert!(status.success(), "supervise failed ({status}): {stderr}");
    let outcome: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(outcome["outcome"], "finished", "{outcome}");
    Pass {
        outcome,
        stderr,
        sessions,
        running_together,
    }
}

#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn happy_path_runs_a_stub_agent_through_cmux_and_lands_on_main() {
    let fixture = fixture();
    let Fixture {
        repo,
        base,
        db,
        env,
        ..
    } = &fixture;
    // The stub reviewer passes a task that says E2E-REVIEW-PASS.
    let task_id = add_ready_task_described(
        env,
        "e2e stub task",
        "Add e2e.txt to the worktree. E2E-REVIEW-PASS",
        &[],
        &[VERIFY_RUN_ENV],
    );
    assert_eq!(
        dagq(env, &["candidates"])["candidates"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // The supervisor pushes what it lands to the repository's bare origin.
    let origin = repo.parent().unwrap().join("origin.git");
    git(
        repo.parent().unwrap(),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    git(repo, &["remote", "add", "origin", origin.to_str().unwrap()]);

    let pass = supervise_once(&fixture, &[], &[&task_id]);
    let workspace = pass.sessions[0].1.clone();
    assert!(
        pass.running_together,
        "the background wrapper {workspace} was never seen running"
    );
    let outcome = &pass.outcome;
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 1, "{outcome}");
    assert_eq!(outcome["errors"], Value::Array(vec![]), "{outcome}");
    // Accepted, reviewed and landed by the supervisor alone (ADR-0027).
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(outcome["runs"][0]["worker_mode"], "headless", "{outcome}");
    let stderr = &pass.stderr;
    let base = base.as_str();
    let repo = repo.as_path();
    let db = db.as_path();

    let detail = dagq(env, &["show", &task_id, "--full"]);
    assert_eq!(detail["task"]["status"], "completed");
    let runs = detail["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    let run_id = run["id"].as_str().unwrap();
    assert_eq!(run["status"], "integrated");
    assert_eq!(run["workspace_id"], workspace.as_str());
    assert_eq!(run["base_commit"], base);
    assert!(run["last_error"].is_null());
    // A run opens no workspace: its session is a background wrapper
    // (`wrapper_handle` above) whose output goes to the run directory's
    // `session.log` (ADR-t1433-3).
    assert_eq!(run["branch"], format!("dagq/{run_id}"));
    let run_dir = Path::new(run["run_dir"].as_str().unwrap());
    assert!(run["workspace_closed_at"].is_number(), "{run}");
    wait_until_wrapper_gone(&workspace);
    assert!(stderr.contains("review 1: pass"), "{stderr}");
    assert!(stderr.contains("integrated"), "{stderr}");

    // The run lives next to the queue.
    assert_eq!(
        run_dir,
        db.canonicalize()
            .unwrap()
            .with_file_name("runs")
            .join(run_id)
    );
    let worktree = Path::new(run["worktree_path"].as_str().unwrap());
    assert_eq!(worktree, run_dir.join("worktree"));
    // The run's own history is kept under its ref; the landing is one
    // squash commit on main with its tree, pushed to origin.
    let head = git(repo, &["rev-parse", &format!("refs/dagq/runs/{run_id}")]);
    assert_ne!(head, base);
    let main = git(repo, &["rev-parse", "main"]);
    assert_ne!(main, head);
    assert_eq!(run["result_commit"], main.as_str());
    assert_eq!(git(&origin, &["rev-parse", "main"]), main);
    assert_eq!(git(repo, &["rev-parse", "main^"]), base);
    assert_eq!(
        git(repo, &["rev-parse", "main^{tree}"]),
        git(repo, &["rev-parse", &format!("{head}^{{tree}}")])
    );
    assert_eq!(
        git(repo, &["log", "-1", "--format=%B", "main"]),
        format!("e2e stub task\n\nadded e2e.txt\n\nDagq-Task: {task_id}\nDagq-Run: {run_id}")
    );
    assert_eq!(git(repo, &["status", "--porcelain"]), ""); // The checkout moved with main.
    assert_eq!(
        fs::read_to_string(repo.join("e2e.txt")).unwrap(),
        format!("written by the stub agent for {run_id}\n")
    );
    assert!(!worktree.exists(), "landed worktree was not removed");
    assert_eq!(
        git(repo, &["branch", "--list", &format!("dagq/{run_id}")]),
        ""
    );

    let receipt: Value =
        serde_json::from_str(&fs::read_to_string(run["receipt_path"].as_str().unwrap()).unwrap())
            .unwrap();
    assert_eq!(receipt["run_id"], run_id);
    assert_eq!(receipt["commit"], head.as_str());
    // The worker's turn is `claude -p --output-format stream-json` under
    // the session wrapper (ADR-t813-1), with the run directory's hook-less
    // settings.
    let log = fs::read_to_string(run["log_path"].as_str().unwrap()).unwrap();
    assert!(
        log.contains(&format!(
            "turn argv: -p --output-format stream-json --session-id {run_id} --resume  --permission-mode auto --add-dir {run_dir} --settings {run_dir}/claude-headless-settings.json --model claude-opus-5-5",
            run_dir = run_dir.display()
        )),
        "{log}"
    );
    // The turn's output is kept; the exit was the exit request in
    // `turns/`, not a `/exit` typed into the terminal.
    let output = fs::read_to_string(run_dir.join("turns/turn-000001.jsonl")).unwrap();
    assert!(output.contains("\"type\":\"result\""), "{output}");
    assert!(run_dir.join("turns/exit").exists());
    // The model and effort were given explicitly (ADR-0079 decision 3).
    assert!(
        log.contains("model: claude-opus-5-5 effort: medium"),
        "{log}"
    );
    // The worker's variables in the background wrapper's environment
    // reached the agent, and the wrapper gave the agent the queue
    // service's socket rather than the queue's path (goal 82's stage (3)).
    let socket = dagq::infrastructure::queue_service::socket_path(
        fixture.db.canonicalize().unwrap().parent().unwrap(),
    );
    assert!(
        log.contains(&format!(
            "env: DAGQ_ROLE=worker DAGQ_QUEUE= DAGQ_SERVICE_SOCKET={}",
            socket.display()
        )),
        "{log}"
    );
    // dagq.toml's [run.env], expanded, reached the agent's shell; the
    // verification commands get it at the landing, the only place they run.
    let shared = db.canonicalize().unwrap().with_file_name("shared");
    assert!(
        log.contains(&format!(
            "run env: E2E_SHARED={} E2E_RUN_DIR={}",
            shared.display(),
            run_dir.display()
        )),
        "{log}"
    );
    assert_eq!(
        fs::read_to_string(run_dir.join("verify-env.txt")).unwrap(),
        format!("verify env: {}\n", shared.display())
    );
    // The headless review ran `claude -p` with the run directory's hook-less
    // settings and read review.md there.
    let review_log = fs::read_to_string(run_dir.join("claude-review.log")).unwrap();
    assert!(
        review_log.contains(&format!(
            "argv: -p --debug-file {run_dir}/claude-review.log --add-dir {run_dir} --settings {run_dir}/claude-review-settings.json",
            run_dir = run_dir.display()
        )),
        "{review_log}"
    );
    assert!(
        review_log.contains(&format!("review: {}/review.md", run_dir.display())),
        "{review_log}"
    );
    // With a session id of its own, recorded in `review_started` and in the
    // review's span (ADR-0048).
    let review_session = review_log
        .lines()
        .find_map(|line| line.strip_prefix("session: "))
        .unwrap_or_default()
        .to_owned();
    assert!(
        !review_session.is_empty() && review_session != run_id,
        "{review_log}"
    );
    // A run opens no workspace, so the supervisor asks cmux for no
    // workspace group of the queue (ADR-t1433-3).
    assert!(
        fixture.group().is_none(),
        "a run made the queue's workspace group"
    );
    // The wrapper's output went to the run directory's log, which
    // `run log` reads.
    let launched = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "wrapper_launched")
        .expect("the background wrapper's start is recorded");
    assert_eq!(launched["payload"]["workspace_id"], workspace.as_str());
    assert_eq!(
        Path::new(launched["payload"]["log"].as_str().unwrap()),
        run_dir.join("session.log")
    );

    let events = detail["events"].as_array().unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    let position = |kind: &str| {
        kinds
            .iter()
            .position(|k| *k == kind)
            .unwrap_or_else(|| panic!("missing {kind} in {kinds:?}"))
    };
    // The session stays open through validation and the review; the exit
    // is requested on the passing verdict, and the landing follows the
    // close.
    let order = [
        "lease_acquired",
        "worktree_created",
        "workspace_created",
        "wrapper_started",
        "agent_started",
        "turn_started",
        "turn_finished",
        "session_idle_observed",
        "supervision_finished",
        "validation_finished",
        "review_started",
        "review_finished",
        "exit_requested",
        "session_exited",
        "workspace_closed",
        "landing_queued",
        "integration_started",
        "integration_rebased",
        "run_integrated",
        "worktree_removed",
        "push_finished",
    ];
    for pair in order.windows(2) {
        assert!(
            position(pair[0]) < position(pair[1]),
            "{} before {}: {kinds:?}",
            pair[0],
            pair[1]
        );
    }
    // The receipt is seen while the turn runs or right after it, but
    // before its idle marker ends the session's watch.
    assert!(position("receipt_observed") < position("session_idle_observed"));
    let event = |kind: &str| events.iter().find(|e| e["kind"] == kind).unwrap();
    let turn = &event("turn_finished")["payload"];
    assert_eq!(turn["outcome"], "succeeded", "{turn}");
    assert_eq!(turn["session_id"], run_id);
    assert_eq!(turn["usage"]["input_tokens"], 11);
    assert_eq!(turn["cost_usd"], 0.02);
    assert_eq!(
        event("workspace_created")["payload"]["workspace_id"],
        workspace.as_str()
    );
    assert_eq!(
        event("session_idle_observed")["payload"]["session_id"],
        run_id
    );
    assert_eq!(
        event("supervision_finished")["payload"],
        serde_json::json!({"status": "validating", "exit_code": null, "session_live": true})
    );
    assert_eq!(
        event("review_started")["payload"]["session_live"],
        true,
        "the session must be alive during the review"
    );
    let review = &event("review_finished")["payload"];
    assert_eq!(review["verdict"], "pass");
    assert_eq!(review["attempt"], 1);
    assert_eq!(
        review["summary"],
        "the stub reviewer found e2e.txt committed"
    );
    assert_eq!(
        event("exit_requested")["payload"]["workspace_id"],
        workspace.as_str()
    );
    assert!(!kinds.contains(&"exit_request_timed_out"), "{kinds:?}");
    assert!(!kinds.contains(&"integration_approved"), "{kinds:?}");
    assert_eq!(event("session_exited")["payload"]["exit_code"], 0);
    let finished = event("validation_finished");
    assert_eq!(finished["payload"]["status"], "awaiting_integration");
    assert_eq!(finished["payload"]["result_commit"], head.as_str());
    assert_eq!(finished["payload"]["receipt"]["summary"], "added e2e.txt");
    // The verification commands ran once, after the rebase, with the run env.
    let verifications: Vec<&Value> = events
        .iter()
        .filter(|e| e["kind"] == "verification_command")
        .collect();
    assert_eq!(verifications.len(), 3);
    assert!(
        verifications
            .iter()
            .all(|e| e["payload"]["exit_code"] == 0 && e["payload"]["phase"] == "integration")
    );
    assert!(!kinds.contains(&"cleanup_failed"), "{kinds:?}");

    let processes = detail["processes"].as_array().unwrap();
    assert_eq!(processes.len(), 2);
    assert!(processes.iter().all(|p| p["exit_code"] == 0));

    assert_eq!(
        dagq(env, &["candidates"])["candidates"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let status = dagq(env, &["status"]);
    assert_eq!(status["supervisors"], Value::Array(vec![]), "{status}");
    assert_eq!(status["runs"], Value::Array(vec![]), "{status}");
    // Only the stopped `--once` supervisor is left; no run waits for anyone.
    assert!(
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["run_id"].is_null()),
        "{status}"
    );
    assert_eq!(
        dagq(env, &["integrate", "--next"])["outcome"],
        "no_run_awaiting"
    );
}

/// The stub worker asks a `worker_question` (its task says `E2E-ASK`) and
/// ends its turn; the run waits out of its slot, and once the ask is
/// answered the supervisor sends the answer to the same session as the
/// prompt of its next turn (ADR-t813-1), closes the ask, and the worker
/// commits it with its change, which is reviewed and lands.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn a_worker_question_is_answered_as_the_next_turn_and_the_run_lands() {
    let fixture = fixture();
    let Fixture { repo, env, .. } = &fixture;
    let task_id = dagq(
        env,
        &[
            "add",
            "e2e asking task",
            "--description",
            "E2E-ASK: ask which word goes into answer.txt, then add e2e.txt. E2E-REVIEW-PASS",
            "--acceptance",
            "answer.txt holds the answer and e2e.txt is committed",
            "--verify",
            "test -f answer.txt",
            "--verify",
            "test -f e2e.txt",
        ],
    )["id"]
        .to_string();
    assert_eq!(
        dagq(env, &["ready", &task_id, "--bypass-review"])["status"],
        "ready"
    );
    // Answers the ask as the inbox would, once the worker registered it
    // and the run waits for it out of its slot: a wait starts only from an
    // open ask, so an answer before the supervisor's tick saw it would
    // leave no wait to check.
    let answerer = {
        let env = Env {
            repo: env.repo.clone(),
            data_home: env.data_home.clone(),
        };
        let task_id = task_id.clone();
        thread::spawn(move || {
            let started = Instant::now();
            loop {
                assert!(
                    started.elapsed() < SUPERVISE_TIMEOUT,
                    "the worker asked nothing, or its run never waited"
                );
                // Its commands write to files: the main thread spawns the
                // service and the supervisor meanwhile ([`dagq_in_files`]).
                let asks = dagq_in_files(&env, &["asks", "--open"]);
                let waiting = || {
                    dagq_in_files(&env, &["show", &task_id, "--full"])["events"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|e| e["kind"] == "run_waiting_started")
                };
                if let Some(ask) = asks["asks"].as_array().unwrap().first()
                    && waiting()
                {
                    assert_eq!(ask["kind"], "worker_question", "{ask}");
                    assert_eq!(ask["asked_by"], "worker", "{ask}");
                    let id = ask["id"].to_string();
                    dagq_in_files(&env, &["answer", &id, "--text", "blue"]);
                    return ask["id"].as_i64().unwrap();
                }
                thread::sleep(Duration::from_millis(200));
            }
        })
    };
    let pass = supervise_once(&fixture, &[], &[&task_id]);
    let ask_id = joined(answerer, "the answering thread");
    assert_eq!(
        pass.outcome["errors"],
        Value::Array(vec![]),
        "{}",
        pass.outcome
    );
    assert_eq!(
        pass.outcome["runs"][0]["status"], "integrated",
        "{}",
        pass.outcome
    );
    let detail = dagq(env, &["show", &task_id, "--full"]);
    assert_eq!(detail["task"]["status"], "completed", "{detail}");
    // The answer landed on main with the change of the same run.
    assert_eq!(
        fs::read_to_string(repo.join("answer.txt")).unwrap(),
        format!("answer to ask {ask_id}: blue\n")
    );
    assert!(repo.join("e2e.txt").is_file());
    let events = detail["events"].as_array().unwrap();
    let of = |kind: &str| -> Vec<&Value> { events.iter().filter(|e| e["kind"] == kind).collect() };
    // Two turns of one session: the ask's, then the answer's.
    let turns = of("turn_finished");
    assert_eq!(turns.len(), 2, "{detail}");
    let run_id = detail["runs"][0]["id"].as_str().unwrap();
    assert!(
        turns.iter().all(|t| t["payload"]["session_id"] == run_id),
        "{turns:?}"
    );
    let requested = of("turn_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(
        requested[0]["payload"]["what"],
        format!("answer of ask {ask_id}")
    );
    let delivered = of("ask_delivered");
    assert_eq!(delivered.len(), 1, "{detail}");
    assert_eq!(delivered[0]["payload"]["ask_id"], ask_id);
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    let position = |kind: &str| {
        kinds
            .iter()
            .position(|k| *k == kind)
            .unwrap_or_else(|| panic!("missing {kind} in {kinds:?}"))
    };
    // The run waited out of its slot until the answer, which went to its
    // turns, and was then reviewed and landed.
    for pair in [
        "run_waiting_started",
        "run_waiting_ended",
        "ask_delivered",
        "review_finished",
        "run_integrated",
    ]
    .windows(2)
    {
        assert!(
            position(pair[0]) < position(pair[1]),
            "{} before {}: {kinds:?}",
            pair[0],
            pair[1]
        );
    }
    let asks = dagq(env, &["asks", "--all"]);
    assert!(asks["asks"][0]["closed_at"].is_number(), "{asks}");
    let open = dagq(env, &["asks"]);
    assert_eq!(open["asks"], json!([]), "{open}");
}

/// Two independent tasks run in two background wrappers at once; the task
/// that depends on one of them waits for its integration and then starts
/// from the main that contains it.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn two_independent_tasks_run_concurrently_and_a_dependent_follows_integration() {
    let fixture = fixture();
    let Fixture {
        repo, base, env, ..
    } = &fixture;
    let first = add_ready_task(env, "e2e first", &[]);
    let second = add_ready_task(env, "e2e second", &[]);
    let third = add_ready_task(env, "e2e dependent", &[&first]);
    assert_eq!(
        dagq(env, &["candidates"])["candidates"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let pass = supervise_once(&fixture, &["--parallel", "2"], &[&first, &second]);
    assert!(
        pass.running_together,
        "both wrappers were never running at the same time: {:?}",
        pass.sessions
    );
    assert_ne!(pass.sessions[0].1, pass.sessions[1].1);
    let outcome = &pass.outcome;
    assert_eq!(outcome["errors"], Value::Array(vec![]), "{outcome}");
    let runs = outcome["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2, "{outcome}");
    assert!(
        runs.iter().all(|r| r["status"] == "awaiting_integration"),
        "{outcome}"
    );
    for task in [&first, &second] {
        let detail = dagq(env, &["show", task, "--full"]);
        assert_eq!(detail["task"]["status"], "in_progress");
        let run = &detail["runs"][0];
        assert_eq!(run["status"], "awaiting_integration");
        assert_eq!(run["base_commit"], base.as_str());
        assert!(run["workspace_closed_at"].is_number(), "{run}");
        assert!(run["last_error"].is_null(), "{run}");
        let worktree = Path::new(run["worktree_path"].as_str().unwrap());
        assert_eq!(git(worktree, &["status", "--porcelain"]), "");
    }
    // The dependent never started: awaiting integration is not completion.
    let detail = dagq(env, &["show", &third, "--full"]);
    assert_eq!(detail["task"]["status"], "ready");
    assert_eq!(detail["runs"], Value::Array(vec![]));
    assert_eq!(
        dagq(env, &["candidates"])["candidates"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    // `doctor` still lists the two waiting runs (task 1520), but neither
    // holds a lease nor is left for `recover`: the pass let go of both.
    let doctor = dagq(env, &["doctor", "--full"]);
    let mut listed: Vec<&str> = doctor["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            assert_eq!(r["status"], "awaiting_integration", "{doctor}");
            assert!(r["lease"].is_null(), "{doctor}");
            assert_eq!(r["recoverable"], false, "{doctor}");
            r["run_id"].as_str().unwrap()
        })
        .collect();
    listed.sort_unstable();
    let mut waiting = Vec::new();
    for task in [&first, &second] {
        let detail = dagq(env, &["show", task, "--full"]);
        waiting.push(detail["runs"][0]["id"].as_str().unwrap().to_owned());
    }
    waiting.sort_unstable();
    assert_eq!(listed, waiting, "{doctor}");

    // Land the first task; the dependent becomes claimable from the landed main.
    let first_run = dagq(env, &["show", &first, "--full"])["runs"][0].clone();
    let first_commit = first_run["result_commit"].as_str().unwrap().to_owned();
    assert_eq!(dagq(env, &["integrate", &first])["outcome"], "integrated");
    let first_landed = git(repo, &["rev-parse", "main"]);
    assert_ne!(first_landed, first_commit);
    assert_eq!(git(repo, &["rev-parse", "main^"]), base.as_str());
    assert_eq!(
        dagq(env, &["candidates"])["candidates"][0]["id"].to_string(),
        third
    );
    let pass = supervise_once(&fixture, &["--parallel", "2"], &[&third]);
    // The same pass rechecks the second task's waiting run against the main
    // the direct integrate moved (ADR-t1310-1): it rewrote the same file as
    // the first, so the recheck resumes its session without waiting for its
    // approve_landing ask, and the stub resolves the conflict on top of main.
    let outcome = &pass.outcome;
    assert_eq!(outcome["errors"], Value::Array(vec![]), "{outcome}");
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 2, "{outcome}");
    let run = dagq(env, &["show", &third, "--full"])["runs"][0].clone();
    assert_eq!(run["status"], "awaiting_integration", "{run}");
    assert_eq!(run["base_commit"], first_landed.as_str());
    assert_eq!(git(repo, &["rev-parse", "main"]), first_landed);
    assert_eq!(dagq(env, &["status"])["supervisors"], Value::Array(vec![]));
    let detail = dagq(env, &["show", &second, "--full"]);
    let run = &detail["runs"][0];
    assert_eq!(run["status"], "awaiting_integration", "{detail}");
    let recheck = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "landing_recheck_failed")
        .unwrap();
    assert_eq!(recheck["payload"]["code"], "rebase_conflict", "{recheck}");
    assert_eq!(recheck["payload"]["action"], "resumed", "{recheck}");
    assert_eq!(
        recheck["payload"]["main"],
        first_landed.as_str(),
        "{recheck}"
    );
    let worktree = Path::new(run["worktree_path"].as_str().unwrap());
    assert_eq!(
        git(worktree, &["rev-parse", "HEAD^"]),
        first_landed,
        "the resumed session rebased onto the landed main"
    );
    let run_dir = Path::new(run["run_dir"].as_str().unwrap());
    let seen = fs::read_to_string(run_dir.join("resume-request-seen.txt")).unwrap();
    assert!(seen.contains("landing recheck"), "{seen}");
    assert!(
        seen.contains(&format!("main is now {first_landed} ")),
        "{seen}"
    );
    assert!(seen.contains("task 1: e2e first"), "{seen}");
    let resumed = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "resume_finished")
        .unwrap();
    if let Some(id) = resumed["payload"]["workspace_id"].as_str() {
        fixture.wrappers.record(id);
    }
    assert_eq!(resumed["payload"]["outcome"], "resolved", "{resumed}");

    // The merge queue is FIFO by validation time: --next takes the second
    // task first, which the recheck's resume already put on the first
    // landing. The next --next takes the dependent: it rewrote the same
    // file on the first landing, so the runtime cannot rebase it and parks
    // it for a session.
    let next = dagq(env, &["integrate", "--next"]);
    assert_eq!(next["outcome"], "integrated", "{next}");
    assert_eq!(next["task"]["id"].to_string(), second);
    let second_landed = git(repo, &["rev-parse", "main"]);
    assert_eq!(git(repo, &["rev-parse", "main^"]), first_landed);
    let parked = dagq(env, &["integrate", "--next"]);
    assert_eq!(parked["outcome"], "needs_session", "{parked}");
    assert_eq!(parked["run"]["task_id"].to_string(), third);
    assert!(
        parked["reason"]
            .as_str()
            .unwrap()
            .contains("conflicted in e2e.txt"),
        "{parked}"
    );

    // The integrate call approved it, so the next supervisor pass resumes
    // its session (the stub plays Claude), which resolves the conflict on
    // top of main and rewrites the receipt; the runtime then lands it.
    let run = dagq(env, &["show", &third, "--full"])["runs"][0].clone();
    assert_eq!(run["status"], "needs_session");
    let worktree = Path::new(run["worktree_path"].as_str().unwrap());
    assert_eq!(
        git(worktree, &["rev-parse", "HEAD"]),
        run["result_commit"].as_str().unwrap()
    );
    assert_eq!(
        dagq(env, &["integrate", "--next"])["outcome"],
        "no_run_awaiting"
    );
    let status = dagq(env, &["status"]);
    let attention = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["run_id"] == run["id"])
        .cloned()
        .unwrap();
    assert_eq!(attention["next"], "resuming (runtime)", "{status}");
    let pass = supervise_once(&fixture, &["--parallel", "2"], &[]);
    let outcome = &pass.outcome;
    assert_eq!(outcome["errors"], Value::Array(vec![]), "{outcome}");
    assert!(
        pass.stderr.contains("resolution request sent"),
        "{}",
        pass.stderr
    );
    let detail = dagq(env, &["show", &third, "--full"]);
    let run = &detail["runs"][0];
    assert_eq!(run["status"], "integrated", "{detail}");
    let run_dir = Path::new(run["run_dir"].as_str().unwrap());
    let seen = fs::read_to_string(run_dir.join("resume-request-seen.txt")).unwrap();
    assert!(
        seen.contains(&format!("main is now {second_landed} ")),
        "{seen}"
    );
    assert!(seen.contains("task 2: e2e second"), "{seen}");
    let kinds: Vec<&str> = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    for kind in [
        "integration_approved",
        "resume_started",
        "resume_finished",
        "run_integrated",
    ] {
        assert!(kinds.contains(&kind), "{kind} missing: {kinds:?}");
    }
    let finished = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "resume_finished")
        .unwrap();
    if let Some(id) = finished["payload"]["workspace_id"].as_str() {
        fixture.wrappers.record(id);
    }
    assert_eq!(finished["payload"]["outcome"], "resolved", "{finished}");
    assert_eq!(finished["payload"]["workspace_closed"], true, "{finished}");
    let resume_workspace = finished["payload"]["workspace_id"].as_str().unwrap();
    wait_until_wrapper_gone(resume_workspace);
    assert_eq!(git(repo, &["rev-parse", "main^"]), second_landed);
    assert_eq!(
        git(repo, &["rev-list", "--count", &format!("{base}..main")]),
        "3"
    );
    assert_eq!(
        fs::read_to_string(repo.join("e2e.txt")).unwrap(),
        format!(
            "resolved by the session for {}\n",
            run["id"].as_str().unwrap()
        )
    );
    for task in [&first, &second, &third] {
        let detail = dagq(env, &["show", task, "--full"]);
        assert_eq!(detail["task"]["status"], "completed", "{task}");
        assert!(
            !Path::new(detail["runs"][0]["worktree_path"].as_str().unwrap()).exists(),
            "{task}"
        );
    }
    assert_eq!(
        git(repo, &["for-each-ref", "refs/dagq/runs/"])
            .lines()
            .count(),
        3
    );
}

/// The supervisor is killed while the stub worker is running (the task 15
/// incident: a binary update or a `kill` took the resident supervisor with
/// it). The wrapper keeps heartbeating in its cmux workspace and the stub
/// writes its receipt regardless. The next `supervise --once` adopts the
/// run from the dead supervisor's stale lease (ADR-0012), requests the
/// session's exit once, validates it, and `integrate` lands it: nothing is redone.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn killed_supervisor_run_is_adopted_by_the_next_supervisor_and_lands() {
    let fixture = fixture();
    let Fixture { repo, env, .. } = &fixture;
    // The worker holds until the supervisor is killed: a stub that wrote
    // its receipt at once left the run `running` only for the moments
    // before its validation, which a loaded host's slower polls of `show`
    // missed, and the test then waited for a state that had passed
    // (task 1008).
    let task_id = add_ready_task_described(
        env,
        "e2e adopted task",
        "Add e2e.txt to the worktree. E2E-HOLD",
        &[],
        &[],
    );

    // A resident supervisor starts the run; it is killed once the worker runs.
    let mut victim = ChildGuard::new(
        Command::new(BIN)
            // A person's supervisor, not the session's running the tests.
            .without_actor_env()
            .current_dir(&fixture.repo)
            .env("XDG_DATA_HOME", &fixture.env.data_home)
            .args(["supervise", "--parallel", "1"])
            .args(NO_LOAD_HOLD)
            .arg("--cmux")
            .arg(&fixture.cmux)
            .arg("--claude")
            .arg(&fixture.stub)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let victim_stderr = reader(victim.0.stderr.take().unwrap());
    let victim_pid = victim.0.id();
    let started = Instant::now();
    let mut victim_stderr = Some(victim_stderr);
    let run = loop {
        let exited = victim.0.try_wait().unwrap().is_some();
        if exited || started.elapsed() >= SUPERVISE_TIMEOUT {
            let _ = victim.0.kill();
            let _ = victim.0.wait();
            victim.reaped();
            let log = joined(
                victim_stderr.take().unwrap(),
                "the supervisor's stderr reader",
            );
            let runs = dagq(env, &["show", &task_id, "--full"])["runs"].clone();
            panic!(
                "the worker did not start within {SUPERVISE_TIMEOUT:?} (the supervisor exited: {exited}); runs: {runs}\nsupervisor stderr:\n{log}"
            );
        }
        let detail = dagq(env, &["show", &task_id, "--full"]);
        if let Some(run) = detail["runs"].as_array().unwrap().last()
            && run["status"] == "running"
        {
            break run.clone();
        }
        thread::sleep(Duration::from_millis(200));
    };
    let victim_stderr = victim_stderr.take().unwrap();
    let run_id = run["id"].as_str().unwrap().to_owned();
    let workspace = run["workspace_id"].as_str().unwrap().to_owned();
    fixture.wrappers.record(&workspace);
    eprintln!(
        "worker of run {run_id} started after {:?}; killing supervisor {victim_pid}",
        started.elapsed()
    );
    victim.0.kill().unwrap();
    victim.0.wait().unwrap();
    victim.reaped();
    eprintln!(
        "killed supervisor stderr:\n{}",
        joined(victim_stderr, "the killed supervisor's stderr reader")
    );
    assert!(!pid_alive(victim_pid));
    // The worker finishes its receipt with no supervisor watching it.
    fs::write(Path::new(run["run_dir"].as_str().unwrap()).join("go"), "").unwrap();

    // What the inbox sees before anyone adopts: the registration and
    // the lease are stale by pid, the wrapper is alive, and the run keeps going.
    let status = status_when(env, |status| {
        status["supervisors"]
            .as_array()
            .is_some_and(|s| s.len() == 1 && s[0]["stale"] == true)
            && status["runs"][0]["lease"]["alive"] == false
    });
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 1, "{status}");
    assert_eq!(supervisors[0]["pid"], victim_pid);
    assert_eq!(supervisors[0]["alive"], false);
    assert_eq!(supervisors[0]["stale"], true);
    assert_eq!(
        supervisors[0]["run_ids"],
        Value::Array(vec![Value::String(run_id.clone())])
    );
    assert_eq!(status["runs"][0]["run_id"], run_id.as_str());
    assert_eq!(status["runs"][0]["lease"]["pid"], victim_pid);
    assert_eq!(status["runs"][0]["lease"]["alive"], false);
    let doctor = dagq(env, &["doctor", "--full"]);
    assert_eq!(doctor["runs"][0]["recoverable"], false, "{doctor}");
    let wrapper = doctor["runs"][0]["processes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["role"] == "wrapper")
        .unwrap()
        .clone();
    assert_eq!(wrapper["alive"], true, "{doctor}");
    assert!(wrapper_running(&workspace));

    // The next supervisor adopts the run instead of leaving it to recover.
    let pass = supervise_once(&fixture, &["--parallel", "1"], &[&task_id]);
    let outcome = &pass.outcome;
    assert_eq!(outcome["errors"], Value::Array(vec![]), "{outcome}");
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 1, "{outcome}");
    assert_eq!(outcome["runs"][0]["id"], run_id.as_str());
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    assert!(
        pass.stderr
            .contains(&format!("run {run_id} adopted from supervisor")),
        "{}",
        pass.stderr
    );
    assert_eq!(pass.sessions, vec![(task_id.clone(), workspace.clone())]);

    let detail = dagq(env, &["show", &task_id, "--full"]);
    assert_eq!(detail["runs"].as_array().unwrap().len(), 1); // Not rerun.
    let run = &detail["runs"][0];
    assert_eq!(run["status"], "awaiting_integration");
    assert_eq!(run["workspace_id"], workspace.as_str());
    assert!(run["last_error"].is_null(), "{run}");
    assert!(run["workspace_closed_at"].is_number(), "{run}");
    wait_until_wrapper_gone(&workspace);
    let events = detail["events"].as_array().unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    let count = |kind: &str| kinds.iter().filter(|k| **k == kind).count();
    assert_eq!(count("run_adopted"), 1, "{kinds:?}");
    assert_eq!(count("exit_requested"), 1, "{kinds:?}");
    assert_eq!(count("session_exited"), 1, "{kinds:?}");
    assert_eq!(count("validation_finished"), 1, "{kinds:?}");
    assert!(!kinds.contains(&"run_recovered"), "{kinds:?}");
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    let adopted = events.iter().find(|e| e["kind"] == "run_adopted").unwrap();
    assert_eq!(adopted["payload"]["previous_pid"], victim_pid);
    assert_eq!(adopted["payload"]["wrapper"]["pid"], wrapper["pid"]);
    assert_eq!(adopted["payload"]["wrapper"]["alive"], true);
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(
        position("agent_started") < position("run_adopted"),
        "{kinds:?}"
    );
    assert!(
        position("run_adopted") < position("exit_requested"),
        "{kinds:?}"
    );
    // `exit_requested` is recorded before the exit request is sent, so a session that
    // exits before the send returns still lands after it.
    assert!(
        position("exit_requested") < position("session_exited"),
        "{kinds:?}"
    );
    // The adopted run waits to land with no lease left; `status` still lists
    // it as the latest awaiting run of its in-progress task (task 1520).
    let released = |status: &Value| {
        status["runs"].as_array().is_some_and(|runs| {
            runs.len() == 1
                && runs[0]["run_id"] == run_id.as_str()
                && runs[0]["status"] == "awaiting_integration"
                && runs[0]["lease"].is_null()
        })
    };
    // The adopter deregistered on exit; the killed one's row stays for `up` to prune.
    let status = status_when(env, |status| {
        status["supervisors"].as_array().is_some_and(|s| {
            s.len() == 1 && s[0]["pid"] == victim_pid && s[0]["run_ids"] == Value::Array(vec![])
        }) && released(status)
    });
    let supervisors = status["supervisors"].as_array().unwrap();
    let diagnosis = format!(
        "victim pid {victim_pid}; supervisors {supervisors:#?}; adopter stderr:\n{}",
        pass.stderr
    );
    assert_eq!(supervisors.len(), 1, "{diagnosis}");
    assert_eq!(supervisors[0]["pid"], victim_pid, "{diagnosis}");
    assert_eq!(
        supervisors[0]["run_ids"],
        Value::Array(vec![]),
        "{diagnosis}"
    );
    assert!(released(&status), "{status}");

    let integrated = dagq(env, &["integrate", &task_id]);
    assert_eq!(integrated["outcome"], "integrated", "{integrated}");
    assert_eq!(integrated["task"]["status"], "completed");
    // No origin in this repository: the push is skipped and the landing stands.
    assert_eq!(integrated["push"]["outcome"], "skipped", "{integrated}");
    assert_eq!(
        integrated["push"]["reason"],
        "the repository has no remote origin"
    );
    assert_eq!(
        fs::read_to_string(repo.join("e2e.txt")).unwrap(),
        format!("written by the stub agent for {run_id}\n")
    );
    assert_eq!(
        git(
            repo,
            &["rev-list", "--count", &format!("{}..main", fixture.base)]
        ),
        "1"
    );
}

/// Unloads the LaunchAgent the test bootstrapped if it is still there when
/// the test ends, so a failed assertion does not leave a supervisor
/// restarting forever against a deleted queue.
struct AgentGuard {
    label: String,
    plist: PathBuf,
}

impl AgentGuard {
    fn loaded(&self) -> bool {
        Command::new("launchctl")
            .args(["print", &format!("gui/{}/{}", uid(), self.label)])
            .bounded_output()
            .unwrap()
            .status
            .success()
    }
}

impl Drop for AgentGuard {
    fn drop(&mut self) {
        if self.loaded() {
            let output = Command::new("launchctl")
                .args(["bootout", &format!("gui/{}/{}", uid(), self.label)])
                .bounded_output()
                .unwrap();
            eprintln!(
                "booted out {} ({}): {}",
                self.label,
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        if self.plist.exists() {
            let _ = fs::remove_file(&self.plist);
            eprintln!("removed {}", self.plist.display());
        }
    }
}

/// The records of a JSON Lines log; each line must be one JSON object.
fn log_records(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect()
}

fn log_messages(path: &Path) -> Vec<String> {
    log_records(path)
        .iter()
        .map(|record| record["message"].as_str().unwrap().to_owned())
        .collect()
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .bounded_output()
        .is_ok_and(|output| output.status.success())
}

/// Whether the launchd `up` / `down` e2e runs. It is temporarily off unless
/// `DAGQ_E2E_LAUNCHD=1`: no project runs the launchd mode now, and without a
/// cmux socket password `up`'s preflight always stops it, which would fail
/// every `--ignored` run. To bring it back, drop this check.
fn launchd_e2e_enabled() -> bool {
    if env::var("DAGQ_E2E_LAUNCHD").as_deref() == Ok("1") {
        return true;
    }
    eprintln!(
        "skipping the launchd up/down e2e: the launchd mode is temporarily out of the \
         default e2e because no project runs it now and, without a cmux socket password, \
         its preflight always stops `up`; set DAGQ_E2E_LAUNCHD=1 to run it"
    );
    false
}

/// `up` bootstraps the supervisor as a LaunchAgent of the real launchd and
/// opens the inbox workspace in the real cmux; `status` lists the
/// supervisor through its registration; `down --wait` unloads the agent
/// and returns once the supervisor has drained and deregistered.
///
/// Runs only with `DAGQ_E2E_LAUNCHD=1`; see [`launchd_e2e_enabled`].
#[test]
#[ignore = "needs a running cmux and launchd; run with --ignored and DAGQ_E2E_LAUNCHD=1"]
fn up_starts_a_launchd_supervisor_that_status_lists_and_down_wait_stops_it() {
    if !launchd_e2e_enabled() {
        return;
    }
    let fixture = fixture();
    let Fixture {
        cmux,
        repo,
        stub,
        db,
        env,
        ..
    } = &fixture;
    // The agent's plist goes under a disposable HOME, not the developer's.
    let home = fixture._dir.path().join("home");
    fs::create_dir(&home).unwrap();
    trust_repository(&home, &env.repo);
    let located = dagq_with(env, &[("HOME", home.as_path())], &["locate"]);
    let label = located["label"].as_str().unwrap().to_owned();
    let plist = PathBuf::from(located["launch_agent"].as_str().unwrap());
    assert!(plist.starts_with(&home));
    let log_dir = PathBuf::from(located["log_dir"].as_str().unwrap());
    let agent = AgentGuard {
        label: label.clone(),
        plist: plist.clone(),
    };
    let mut workspaces = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };

    let up_args = [
        "up",
        "--parallel",
        "2",
        "--cmux",
        cmux.to_str().unwrap(),
        "--claude",
        stub.to_str().unwrap(),
        // The load of a busy host would hold every claim (task 680).
        NO_LOAD_HOLD[0],
        NO_LOAD_HOLD[1],
    ];
    let started = Instant::now();
    // A supervisor that never registers is diagnosed from launchd.log,
    // which `up` names in its error; show it before failing.
    let output = dagq_output(env, &[("HOME", home.as_path())], &up_args);
    workspaces.record_opened(&output.stdout);
    if !output.status.success() {
        let launchd_log = log_dir.join("launchd.log");
        eprintln!(
            "launchd.log:\n{}",
            fs::read_to_string(&launchd_log).unwrap_or_else(|_| "(not written)".into())
        );
        panic!("dagq up: {}", String::from_utf8_lossy(&output.stderr));
    }
    let first: Value = serde_json::from_slice(&output.stdout).unwrap();
    eprintln!("up took {:?}: {first}", started.elapsed());
    assert_eq!(first["supervisor"]["outcome"], "started", "{first}");
    let pid = u32::try_from(first["supervisor"]["pid"].as_u64().unwrap()).unwrap();
    assert!(pid_alive(pid));
    assert_eq!(first["supervisor"]["plist"], plist.to_str().unwrap());
    assert_eq!(first["supervisor"]["log_dir"], log_dir.to_str().unwrap());
    assert_eq!(first["pruned_supervisors"], Value::Array(vec![]));
    let repo_name = repo.file_name().unwrap().to_str().unwrap();
    // The inbox is the one session `up` opens (ADR-0041 decision 6).
    assert_eq!(first.get("planner"), None, "{first}");
    let sessions: Vec<String> = ["inbox"]
        .into_iter()
        .map(|key| {
            assert_eq!(first[key]["outcome"], "created", "{first}");
            assert_eq!(first[key]["name"], format!("[{repo_name}]{key}"));
            let id = first[key]["workspace_id"].as_str().unwrap().to_owned();
            uuid::Uuid::parse_str(&id).expect("workspace id is a UUID");
            assert!(workspace_listed(cmux, &id));
            id
        })
        .collect();
    // launchd knows the agent, and the plist is what `up` described.
    assert!(
        agent.loaded(),
        "launchctl print gui/{}/{label} failed",
        uid()
    );
    let contents = fs::read_to_string(&plist).unwrap();
    assert!(contents.contains(&format!("<string>{label}</string>")));
    assert!(contents.contains("<string>supervise</string>"));
    assert!(contents.contains(&format!("<string>{}</string>", db.display())));
    assert!(contents.contains("<key>KeepAlive</key>\n\t<true/>"));
    assert!(
        contents.contains("\t\t<string>--max-load</string>\n\t\t<string>0</string>\n"),
        "{contents}"
    );
    // The launchd-run supervisor wrote its own log.
    let logs: Vec<PathBuf> = fs::read_dir(&log_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with(&format!("-{pid}.jsonl"))
        })
        .collect();
    assert_eq!(logs.len(), 1, "{logs:?}");
    let log = log_messages(&logs[0]);
    assert!(
        log.iter().any(|m| m.contains(&format!(
            "started: version {VERSION}, pid {pid}, parallel 2"
        ))),
        "{log:?}"
    );

    let status = dagq(env, &["status"]);
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 1, "{status}");
    assert_eq!(supervisors[0]["pid"], pid);
    assert_eq!(supervisors[0]["registered"], true);
    assert_eq!(supervisors[0]["alive"], true);
    assert_eq!(supervisors[0]["stale"], false);
    assert_eq!(supervisors[0]["parallel"], 2);
    // The supervisor recorded the build it runs, which is what the next
    // `up` compares itself against (ADR-0014).
    assert_eq!(supervisors[0]["binary_version"], VERSION, "{status}");
    assert_eq!(status["runs"], Value::Array(vec![]));

    // Idempotent: nothing is started or opened twice.
    let second = dagq_opening(env, &[("HOME", home.as_path())], &up_args, &mut workspaces);
    assert_eq!(second["supervisor"]["outcome"], "reused", "{second}");
    assert_eq!(second["supervisor"]["version"], VERSION, "{second}");
    assert_eq!(second["supervisor"]["pid"], pid);
    for (key, id) in ["inbox"].into_iter().zip(&sessions) {
        assert_eq!(second[key]["outcome"], "reused", "{second}");
        assert_eq!(second[key]["workspace_id"], id.as_str());
    }
    assert_eq!(second["pruned_supervisors"], Value::Array(vec![]));

    let started = Instant::now();
    let down = dagq_with(env, &[("HOME", home.as_path())], &["down", "--wait"]);
    eprintln!("down --wait took {:?}: {down}", started.elapsed());
    assert_eq!(down["outcome"], "stopped", "{down}");
    assert_eq!(down["pid"], pid);
    assert_eq!(down["launch_agent_unloaded"], true);
    assert!(!pid_alive(pid), "supervisor {pid} is still alive");
    assert!(!agent.loaded(), "the agent is still loaded");
    assert!(!plist.exists(), "the plist was not removed");
    let status = dagq(env, &["status"]);
    assert_eq!(status["supervisors"], Value::Array(vec![]), "{status}");
    let log = log_messages(&logs[0]);
    assert!(
        log.iter()
            .any(|m| m.contains("exiting: {\"errors\":[],\"outcome\":\"stopped\"")),
        "{log:?}"
    );
    // The inbox workspace is left open by `down`; the guard closes it.
    for id in &sessions {
        assert!(workspace_listed(cmux, id));
    }
    let again = dagq_with(env, &[("HOME", home.as_path())], &["down"]);
    assert_eq!(again["outcome"], "not_running", "{again}");
}

/// `up --in-cmux` needs no socket password: the supervisor runs inside a
/// cmux workspace of its own, so it is a child of a cmux terminal like any
/// other client. Nothing about launchd is touched, `status` reports the
/// mode, and `down --wait` interrupts the supervisor and closes the
/// workspace once it has drained.
// Out until task 1443 deletes this case and this cfg (ADR-t1582-1).
#[cfg(any())]
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn up_in_cmux_starts_a_supervisor_in_a_workspace_that_down_wait_stops_and_closes() {
    let fixture = fixture();
    let Fixture {
        cmux,
        repo,
        stub,
        env,
        ..
    } = &fixture;
    // A disposable HOME, so a stray plist could only land there; none should.
    let home = fixture._dir.path().join("home");
    fs::create_dir(&home).unwrap();
    trust_repository(&home, &env.repo);
    let located = dagq_with(env, &[("HOME", home.as_path())], &["locate"]);
    let plist = PathBuf::from(located["launch_agent"].as_str().unwrap());
    let label = located["label"].as_str().unwrap().to_owned();
    let log_dir = PathBuf::from(located["log_dir"].as_str().unwrap());
    let mut workspaces = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };

    let up_args = [
        "up",
        "--in-cmux",
        "--parallel",
        "2",
        "--cmux",
        cmux.to_str().unwrap(),
        "--claude",
        stub.to_str().unwrap(),
        // The load of a busy host would hold every claim (task 680).
        NO_LOAD_HOLD[0],
        NO_LOAD_HOLD[1],
    ];
    let started = Instant::now();
    let first = dagq_opening(env, &[("HOME", home.as_path())], &up_args, &mut workspaces);
    eprintln!("up --in-cmux took {:?}: {first}", started.elapsed());
    assert_eq!(first["supervisor"]["outcome"], "started", "{first}");
    assert_eq!(first["supervisor"]["mode"], "in_cmux");
    assert_eq!(first["supervisor"]["plist"], Value::Null);
    let repo_name = repo.file_name().unwrap().to_str().unwrap();
    assert_eq!(repo_name, E2E_REPO_NAME);
    assert_eq!(
        first["supervisor"]["name"],
        format!("[{repo_name}]supervisor")
    );
    let supervisor_workspace = first["supervisor"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    uuid::Uuid::parse_str(&supervisor_workspace).expect("workspace id is a UUID");
    assert!(workspace_listed(cmux, &supervisor_workspace));
    let pid = u32::try_from(first["supervisor"]["pid"].as_u64().unwrap()).unwrap();
    assert!(pid_alive(pid));
    let ps = Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .bounded_output()
        .unwrap();
    let command = String::from_utf8_lossy(&ps.stdout);
    assert!(command.contains(" --max-load 0"), "{command}");
    assert_eq!(first["inbox"]["outcome"], "created", "{first}");
    let inbox = first["inbox"]["workspace_id"].as_str().unwrap().to_owned();
    // `up` opens no planner (ADR-0041 decision 6); the runtime does.
    assert_eq!(first.get("planner"), None, "{first}");

    // Every workspace carries its role and the queue in its own
    // environment, and all joined the queue's group (ADR-0026).
    let db = fixture.db.canonicalize().unwrap();
    for (id, role) in [(&supervisor_workspace, "supervisor"), (&inbox, "inbox")] {
        let env = workspace_env(cmux, id);
        assert_eq!(env["DAGQ_ROLE"], role, "{env}");
        assert_eq!(env["DAGQ_QUEUE"], db.to_str().unwrap(), "{env}");
    }
    let group = fixture.group().expect("the queue's workspace group exists");
    assert_eq!(group["name"], "[dagq-e2e]", "{group}");
    let members: Vec<String> = group["member_workspace_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_ascii_lowercase())
        .collect();
    for id in [&supervisor_workspace, &inbox] {
        assert!(members.contains(&id.to_ascii_lowercase()), "{group}");
    }
    assert_eq!(
        listed_workspace(cmux, &inbox).unwrap()["description"],
        format!("dagq role=inbox queue={}", fixture.group.external_id)
    );
    // The inbox is Amber, pinned with its role's pill (ADR-0031).
    assert_look(cmux, &inbox, "#7D6608", "dagq_role=inbox icon=tray");
    assert_eq!(
        listed_workspace(cmux, &supervisor_workspace).unwrap()["pinned"],
        false
    );
    // A person renames the inbox workspace; `up` still knows it.
    let rename = Command::new(cmux)
        .args(["workspace", "rename", &inbox, "--title", "renamed by hand"])
        .bounded_output()
        .unwrap();
    assert!(rename.status.success(), "{rename:?}");

    // launchd knows nothing about this queue, and no plist was written.
    assert!(!plist.exists(), "{} exists", plist.display());
    assert!(
        !Command::new("launchctl")
            .args(["print", &format!("gui/{}/{label}", uid())])
            .bounded_output()
            .unwrap()
            .status
            .success(),
        "launchd has an agent for {label}"
    );
    // The supervisor in the workspace writes its JSON Lines log to the
    // queue's log directory, exactly as the launchd-run one does
    // (ADR-0033): the file outlives the workspace.
    let logs: Vec<PathBuf> = fs::read_dir(&log_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_str().unwrap();
            name.starts_with("supervise-") && name.ends_with(&format!("-{pid}.jsonl"))
        })
        .collect();
    assert_eq!(logs.len(), 1, "{logs:?} in {}", log_dir.display());
    let supervisor_log = logs[0].clone();
    assert!(
        log_messages(&supervisor_log)
            .iter()
            .any(|m| m.contains(&format!(
                "started: version {VERSION}, pid {pid}, parallel 2"
            )))
    );

    // The in-cmux supervisor lands a task, and its integrate progress and
    // the failed push of main (origin does not exist) are records in that
    // file, not only lines on the workspace's screen.
    let task_id = add_ready_task_described(
        env,
        "logged landing",
        "Add e2e.txt to the worktree. E2E-REVIEW-PASS",
        &[],
        &[],
    );
    let missing_origin = repo.parent().unwrap().join("missing-origin.git");
    git(
        repo,
        &["remote", "add", "origin", missing_origin.to_str().unwrap()],
    );
    let deadline = Instant::now() + WAIT_LIMIT;
    let run_id = loop {
        let detail = dagq(env, &["show", &task_id, "--full"]);
        if let Some(id) = detail["runs"][0]["workspace_id"].as_str() {
            workspaces.record(id);
        }
        if detail["task"]["status"] == "completed" {
            break detail["runs"][0]["id"].as_str().unwrap().to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "task {task_id} did not land: {detail}\n{:?}",
            log_messages(&supervisor_log)
        );
        thread::sleep(Duration::from_millis(500));
    };
    // main is pushed after the task completes; wait for its record.
    let pushed = format!("run {run_id}: push of the landing branch failed");
    let records = loop {
        let records = log_records(&supervisor_log);
        if records
            .iter()
            .any(|r| r["message"].as_str().unwrap().contains(&pushed))
        {
            break records;
        }
        assert!(Instant::now() < deadline, "no push record: {records:#?}");
        thread::sleep(Duration::from_millis(200));
    };
    let find = |needle: &str| {
        records
            .iter()
            .find(|r| r["message"].as_str().unwrap().contains(needle))
            .unwrap_or_else(|| panic!("no record with {needle:?} in {records:#?}"))
            .clone()
    };
    let integrating = find(&format!(
        "run {run_id} integrating task {task_id} onto main"
    ));
    assert_eq!(integrating["level"], "INFO");
    assert_eq!(integrating["target"], "dagq::application::integrate");
    assert_eq!(integrating["fields"]["op"], "integrate");
    assert_eq!(integrating["fields"]["run_id"], run_id.as_str());
    assert_eq!(integrating["fields"]["task_id"], task_id.as_str());
    assert_eq!(integrating["spans"][0]["name"], "integrate");
    let landed = find(&format!("task {task_id} landed as"));
    assert_eq!(landed["fields"]["run_id"], run_id.as_str());
    let push = find(&pushed);
    assert_eq!(push["level"], "WARN");
    assert_eq!(push["fields"]["op"], "push");
    assert!(push["fields"]["error"].as_str().is_some(), "{push}");
    eprintln!("in-cmux supervisor log {}:", supervisor_log.display());
    for record in [&integrating, &landed, &push] {
        eprintln!("{record}");
    }

    let status = dagq(env, &["status"]);
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 1, "{status}");
    assert_eq!(supervisors[0]["pid"], pid);
    assert_eq!(supervisors[0]["mode"], "in_cmux");
    assert_eq!(
        supervisors[0]["workspace_id"],
        supervisor_workspace.as_str()
    );
    assert_eq!(supervisors[0]["stale"], false);
    let doctor = dagq(env, &["doctor", "--full"]);
    assert_eq!(doctor["supervisors"][0]["mode"], "in_cmux", "{doctor}");
    assert_eq!(
        doctor["supervisors"][0]["binary_version"], VERSION,
        "{doctor}"
    );

    // Idempotent: the live supervisor is of this binary's own version, so
    // it is reused with its mode and workspace and nothing is replaced.
    let second = dagq_opening(env, &[("HOME", home.as_path())], &up_args, &mut workspaces);
    assert_eq!(second["supervisor"]["outcome"], "reused", "{second}");
    assert_eq!(second["supervisor"]["version"], VERSION, "{second}");
    assert_eq!(second["supervisor"]["mode"], "in_cmux");
    assert_eq!(
        second["supervisor"]["workspace_id"],
        supervisor_workspace.as_str()
    );
    assert_eq!(second["inbox"]["outcome"], "reused", "{second}");
    assert_eq!(second["inbox"]["workspace_id"], inbox.as_str());
    // The look is put back on a reused workspace that lost it.
    let unpin = Command::new(cmux)
        .args([
            "workspace-action",
            "--action",
            "unpin",
            "--workspace",
            &inbox,
        ])
        .bounded_output()
        .unwrap();
    assert!(unpin.status.success(), "{unpin:?}");
    // The inbox has lost its pin before `up` runs, as cmux lists it.
    wait_for_listed(cmux, &inbox, "the unpin never showed up", |listed| {
        listed["pinned"] == false
    });
    let third = dagq_opening(env, &[("HOME", home.as_path())], &up_args, &mut workspaces);
    assert_eq!(third["inbox"]["outcome"], "reused", "{third}");
    assert_eq!(third["warnings"], serde_json::json!([]), "{third}");
    assert_look(cmux, &inbox, "#7D6608", "dagq_role=inbox icon=tray");

    // cmux refuses to close a pinned workspace; dagq's close unpins it
    // first, so `down` closes the supervisor's workspace even when a person
    // pinned it.
    let pin = Command::new(cmux)
        .args([
            "workspace-action",
            "--action",
            "pin",
            "--workspace",
            &supervisor_workspace,
        ])
        .bounded_output()
        .unwrap();
    assert!(pin.status.success(), "{pin:?}");
    wait_for_listed(
        cmux,
        &supervisor_workspace,
        "the pin never showed up",
        |listed| listed["pinned"] == true,
    );
    let refused = Command::new(cmux)
        .args(["workspace", "close", &supervisor_workspace])
        .bounded_output()
        .unwrap();
    eprintln!("cmux workspace close of a pinned workspace: {refused:?}");
    assert!(!refused.status.success(), "{refused:?}");

    let started = Instant::now();
    let down = dagq_with(env, &[("HOME", home.as_path())], &["down", "--wait"]);
    eprintln!("down --wait took {:?}: {down}", started.elapsed());
    assert_eq!(down["outcome"], "stopped", "{down}");
    assert_eq!(down["pid"], pid);
    assert_eq!(down["launch_agent_unloaded"], false);
    assert_eq!(
        down["supervisor_workspaces"],
        serde_json::json!([{"workspace_id": supervisor_workspace, "outcome": "closed"}]),
        "{down}"
    );
    assert!(!pid_alive(pid), "supervisor {pid} is still alive");
    wait_until_not_listed(cmux, &supervisor_workspace);
    assert_eq!(dagq(env, &["status"])["supervisors"], Value::Array(vec![]));
    // The inbox workspace is left open by `down`; the guard closes it.
    assert!(workspace_listed(cmux, &inbox));
}

/// `install` hands a supervisor over to the new binary while its worker
/// still works (ADR-0045 decision 10): the supervisor process execs the
/// installed file under its own pid and token, the session in its cmux
/// workspace is not touched, and the continued supervisor watches the run
/// through its receipt, review, exit request and landing on main. `--rollback`
/// hands it over again to the binary the install kept.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn install_hands_the_supervisor_over_while_a_session_works_and_the_run_lands() {
    let fixture = fixture();
    let Fixture {
        cmux, repo, env, ..
    } = &fixture;
    let task_id = add_ready_task_described(
        env,
        "e2e handoff task",
        "Add e2e.txt to the worktree. E2E-HOLD E2E-REVIEW-PASS",
        &[],
        &[],
    );
    // The fixed binary the supervisor runs and `install` replaces.
    let fixed = fixture._dir.path().join("bin").join("dagq");
    fs::create_dir_all(fixed.parent().unwrap()).unwrap();
    fs::copy(BIN, &fixed).unwrap();
    let fixed_text = fixed.to_str().unwrap();
    let mut supervisor = ChildGuard::new(
        Command::new(&fixed)
            // A person's supervisor, not the session's running the tests.
            .without_actor_env()
            .current_dir(repo)
            .env("XDG_DATA_HOME", &env.data_home)
            .args(["supervise", "--parallel", "1", "--observe-interval", "0"])
            .args(NO_LOAD_HOLD)
            .arg("--cmux")
            .arg(cmux)
            .arg("--claude")
            .arg(&fixture.stub)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = reader(supervisor.0.stderr.take().unwrap());
    let pid = supervisor.0.id();
    let started = Instant::now();
    let run = loop {
        assert!(
            supervisor.0.try_wait().unwrap().is_none(),
            "the supervisor exited before the worker started"
        );
        assert!(
            started.elapsed() < SUPERVISE_TIMEOUT,
            "the worker did not start within {SUPERVISE_TIMEOUT:?}"
        );
        let detail = dagq(env, &["show", &task_id, "--full"]);
        if let Some(run) = detail["runs"].as_array().unwrap().last()
            && run["status"] == "running"
        {
            break run.clone();
        }
        thread::sleep(Duration::from_millis(200));
    };
    let run_id = run["id"].as_str().unwrap().to_owned();
    let workspace = run["workspace_id"].as_str().unwrap().to_owned();
    fixture.wrappers.record(&workspace);
    let run_dir = PathBuf::from(run["run_dir"].as_str().unwrap());

    let installed = dagq(
        env,
        &[
            "install",
            "--from",
            BIN,
            "--to",
            fixed_text,
            "--handoff-timeout",
            "120",
        ],
    );
    eprintln!("install: {installed}");
    assert_eq!(installed["outcome"], "installed", "{installed}");
    assert_eq!(installed["version"], VERSION);
    assert_eq!(installed["supervisors"][0]["pid"], pid, "{installed}");
    let token = installed["supervisors"][0]["token"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(fixed.with_file_name("dagq.previous").is_file());
    // The same process, the same registration; the session goes on.
    assert!(supervisor.0.try_wait().unwrap().is_none());
    let status = dagq(env, &["status"]);
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 1, "{status}");
    assert_eq!(supervisors[0]["pid"], pid);
    assert_eq!(supervisors[0]["binary_version"], VERSION);
    assert_eq!(status["runs"][0]["run_id"], run_id.as_str(), "{status}");
    assert_eq!(status["runs"][0]["status"], "running", "{status}");
    assert!(wrapper_running(&workspace));

    // Let the worker finish: the continued supervisor lands the run.
    fs::write(run_dir.join("go"), "").unwrap();
    let mut stderr = Some(stderr);
    let landed = loop {
        if started.elapsed() >= SUPERVISE_TIMEOUT * 2 {
            let _ = supervisor.0.kill();
            let log = joined(stderr.take().unwrap(), "the supervisor's stderr reader");
            panic!("the run did not land; supervisor stderr:\n{log}");
        }
        let detail = dagq(env, &["show", &task_id, "--full"]);
        if detail["task"]["status"] == "completed" {
            break detail;
        }
        thread::sleep(Duration::from_millis(300));
    };
    let events = landed["events"].as_array().unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    let count = |kind: &str| kinds.iter().filter(|k| **k == kind).count();
    assert_eq!(count("supervisor_handed_off"), 1, "{kinds:?}");
    assert_eq!(count("run_adopted"), 0, "{kinds:?}");
    assert_eq!(count("exit_requested"), 1, "{kinds:?}");
    assert_eq!(count("lease_acquired"), 1, "{kinds:?}");
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("supervisor_handed_off") < position("receipt_observed"));
    assert_eq!(landed["runs"].as_array().unwrap().len(), 1);
    assert_eq!(landed["runs"][0]["status"], "integrated");
    assert_eq!(
        git(repo, &["show", "main:e2e.txt"]),
        format!("written by the stub agent for {run_id}")
    );

    // Back to the binary the install kept, the same way.
    let rolled = dagq(
        env,
        &[
            "install",
            "--rollback",
            "--to",
            fixed_text,
            "--handoff-timeout",
            "120",
        ],
    );
    assert_eq!(rolled["supervisors"][0]["pid"], pid, "{rolled}");
    assert_eq!(rolled["supervisors"][0]["token"], token.as_str());
    assert!(supervisor.0.try_wait().unwrap().is_none());

    // SIGINT drains the continued supervisor like any other.
    unsafe { libc::kill(pid as i32, libc::SIGINT) };
    let deadline = Instant::now() + WAIT_LIMIT;
    let exit = loop {
        if let Some(exit) = supervisor.0.try_wait().unwrap() {
            supervisor.reaped();
            break exit;
        }
        assert!(Instant::now() < deadline, "the supervisor did not stop");
        thread::sleep(Duration::from_millis(200));
    };
    let stderr = joined(stderr.take().unwrap(), "the supervisor's stderr reader");
    eprintln!("supervisor stderr:\n{stderr}");
    assert!(exit.success(), "{exit}");
    assert_eq!(
        stderr
            .matches(&format!("supervisor {token} handed off: version {VERSION}"))
            .count(),
        2
    );
    assert!(!stderr.contains("could not exec"), "{stderr}");
    assert!(
        dagq(env, &["status"])["supervisors"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

/// `supervise --auto-update` (ADR-0045 decision 17) with a real cmux worker
/// at work: a commit that changes the runtime lands on main, the supervisor's
/// job builds it in the queue's own checkout (a stub build copies the
/// binary under test), puts it in place and hands the supervisor over under
/// its pid and token, and the worker's run goes on and lands.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn auto_update_hands_the_supervisor_over_while_a_session_works_and_the_run_lands() {
    let fixture = fixture();
    let Fixture {
        cmux,
        repo,
        env,
        db,
        ..
    } = &fixture;
    let task_id = add_ready_task_described(
        env,
        "e2e auto-update task",
        "Add e2e.txt to the worktree. E2E-HOLD E2E-REVIEW-PASS",
        &[],
        &[],
    );
    // The automatic update builds only dagq's source (ADR-t614-1).
    fs::write(repo.join("Cargo.toml"), "[package]\nname = \"dagq\"\n").unwrap();
    git(repo, &["add", "Cargo.toml"]);
    git(repo, &["commit", "-q", "-m", "dagq's manifest"]);
    let fixed = fixture._dir.path().join("bin").join("dagq");
    fs::create_dir_all(fixed.parent().unwrap()).unwrap();
    fs::copy(BIN, &fixed).unwrap();
    let build = format!(
        "mkdir -p \"$CARGO_TARGET_DIR/release\" && cp {} \"$CARGO_TARGET_DIR/release/dagq\"",
        common::shell_path(BIN)
    );
    let mut supervisor = ChildGuard::new(
        Command::new(&fixed)
            // A person's supervisor, not the session's running the tests.
            .without_actor_env()
            .current_dir(repo)
            .env("XDG_DATA_HOME", &env.data_home)
            .args(["supervise", "--parallel", "1", "--observe-interval", "0"])
            .args(NO_LOAD_HOLD)
            .arg("--cmux")
            .arg(cmux)
            .arg("--claude")
            .arg(&fixture.stub)
            .args(["--auto-update", "--update-interval", "1"])
            .arg("--update-build-command")
            .arg(&build)
            // The e2e gate (ADR-t963-1) passes without running the e2e
            // again inside this one.
            .args(["--update-e2e-command", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = reader(supervisor.0.stderr.take().unwrap());
    let pid = supervisor.0.id();
    let updates = || {
        dagq::infrastructure::sqlite::SqliteQueue::open(db)
            .unwrap()
            .update_events(100)
            .unwrap()
    };
    let installed = |sha: &str| {
        updates()
            .iter()
            .any(|u| u.kind == "update_installed" && u.payload["commit"] == sha)
    };
    let started = Instant::now();
    let run = loop {
        assert!(
            supervisor.0.try_wait().unwrap().is_none(),
            "the supervisor exited before the worker started"
        );
        assert!(
            started.elapsed() < SUPERVISE_TIMEOUT,
            "the worker did not start within {SUPERVISE_TIMEOUT:?}"
        );
        let detail = dagq(env, &["show", &task_id, "--full"]);
        if let Some(run) = detail["runs"].as_array().unwrap().last()
            && run["status"] == "running"
        {
            break run.clone();
        }
        thread::sleep(Duration::from_millis(200));
    };
    let run_id = run["id"].as_str().unwrap().to_owned();
    let workspace = run["workspace_id"].as_str().unwrap().to_owned();
    fixture.wrappers.record(&workspace);
    let run_dir = PathBuf::from(run["run_dir"].as_str().unwrap());

    // A change of the runtime lands on main while the worker works.
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src").join("lib.rs"), "// landed\n").unwrap();
    git(repo, &["add", "src/lib.rs"]);
    git(repo, &["commit", "-q", "-m", "runtime change"]);
    let landed = git(repo, &["rev-parse", "main"]).trim().to_owned();
    let mut stderr = Some(stderr);
    while !installed(&landed) {
        if started.elapsed() >= SUPERVISE_TIMEOUT {
            let _ = supervisor.0.kill();
            let log = joined(stderr.take().unwrap(), "the supervisor's stderr reader");
            panic!(
                "{landed} was not installed: {:?}\nsupervisor stderr:\n{log}",
                updates()
            );
        }
        thread::sleep(Duration::from_millis(300));
    }
    assert!(fixed.with_file_name("dagq.previous").is_file());
    assert!(supervisor.0.try_wait().unwrap().is_none());
    let status = dagq(env, &["status"]);
    assert_eq!(status["auto_update"]["state"], "installed", "{status}");
    assert_eq!(status["auto_update"]["commit"], landed.as_str(), "{status}");
    let supervisors = status["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 1, "{status}");
    assert_eq!(supervisors[0]["pid"], pid);
    assert_eq!(supervisors[0]["auto_update"], true);
    assert_eq!(status["runs"][0]["run_id"], run_id.as_str(), "{status}");
    assert_eq!(status["runs"][0]["status"], "running", "{status}");
    assert!(wrapper_running(&workspace));

    // The worker finishes; the continued supervisor lands its run.
    fs::write(run_dir.join("go"), "").unwrap();
    let detail = loop {
        if started.elapsed() >= SUPERVISE_TIMEOUT * 2 {
            let _ = supervisor.0.kill();
            let log = joined(stderr.take().unwrap(), "the supervisor's stderr reader");
            panic!("the run did not land; supervisor stderr:\n{log}");
        }
        let detail = dagq(env, &["show", &task_id, "--full"]);
        if detail["task"]["status"] == "completed" {
            break detail;
        }
        thread::sleep(Duration::from_millis(300));
    };
    let kinds: Vec<&str> = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"supervisor_handed_off"), "{kinds:?}");
    assert!(!kinds.contains(&"run_adopted"), "{kinds:?}");
    assert_eq!(detail["runs"][0]["status"], "integrated");
    // The run's timeline shows the update that happened while it worked
    // (ADR-0073 decision 17, task 496).
    let timeline = dagq(env, &["timeline", &run_id]);
    assert!(
        timeline["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "update_installed" && e["commit"] == landed.as_str()),
        "{timeline}"
    );

    unsafe { libc::kill(pid as i32, libc::SIGINT) };
    let deadline = Instant::now() + WAIT_LIMIT;
    let exit = loop {
        if let Some(exit) = supervisor.0.try_wait().unwrap() {
            supervisor.reaped();
            break exit;
        }
        assert!(Instant::now() < deadline, "the supervisor did not stop");
        thread::sleep(Duration::from_millis(200));
    };
    let stderr = joined(stderr.take().unwrap(), "the supervisor's stderr reader");
    assert!(exit.success(), "{exit}\n{stderr}");
    assert!(!stderr.contains("could not exec"), "{stderr}");
}
