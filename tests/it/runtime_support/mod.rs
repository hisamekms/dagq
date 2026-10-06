//! The fixture of the runtime tests (`tests/runtime_*.rs`): the stub agent
//! and workspace, the supervisor options and the helpers several of them use.
#![allow(dead_code, unused_imports)]

use crate::common;
use dagq::infrastructure::git_binary::git_executable;
pub mod background_wrappers;
pub mod headless;
mod reviewer;
pub use headless::{CODEX_HOME, set_codex_model};
pub use reviewer::*;
mod thread_stacks;
pub use crate::common::{Bounded, WithoutActor, shell_path};
pub use anyhow::{Result, bail, ensure};
use dagq::domain::LeaseToken;
use dagq::domain::background_wrapper::HeadlessWrapper;
pub use dagq::domain::headless_job::JobAccess;
pub use dagq::{
    VERSION,
    application::{
        AgentProvider, Clock, CommandSpec, Exit, Generators, IdGenerator, MainRemote, Spawned,
        Spawner, Streams, SupervisorEnvironment, TaskStore, WorkspaceBackend, WorkspaceTags,
        dependency_graph,
    },
    domain::{
        AskId, AskKind, CommitSha, EventId, EvidenceCheck, GoalEdit, GoalId, MAX_RESUME_ATTEMPTS,
        NewAsk, NewGoal, NewTask, Priority, ReasonCode, RunId, RunStatus, SessionRole, Task,
        TaskAction, TaskId, TaskRun, TaskStatus,
        resume::CONFLICT_ONLY_RESUME_LIMIT,
        search::{SearchKind, SearchQuery, SearchRef},
    },
    infrastructure::{
        adapters::{GitRepository, shell_join, workspace_handle},
        asks::AskQuery,
        clock::{self, SystemClock},
        location::QueueLocation,
        process,
        run_files::LocalRunFiles,
        sqlite::SqliteQueue,
        telemetry::Telemetry,
    },
    runtime::{self, IntegrateTarget, SuperviseOptions},
};
use dagq::{application::ProcessControl, infrastructure::adapters::SystemProcesses};
pub use rusqlite::Connection;
use rusqlite::OptionalExtension;
pub use serde_json::{Value, json};
pub use std::{
    collections::HashMap,
    fs,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, LazyLock, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
pub use tempfile::TempDir;

/// A full Git object ID as the runtime takes it.
pub fn sha(commit: &str) -> CommitSha {
    CommitSha::try_from(commit).unwrap()
}

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

/// The first lines of every stub agent: a stub whose test process is gone
/// (killed, or ended while the session still ran) kills its own process
/// group, the one [`StubSpawner`] made, with every child in it.
macro_rules! watchdog {
    () => {
        r#"
( while kill -0 $$ 2>/dev/null; do kill -0 "$PPID" 2>/dev/null || kill -s KILL -- -$$; sleep 0.2; done ) &
"#
    };
}

/// A test's directory with its repository and queue. Dropping it, when the
/// test returns or panics, kills every stub agent started on its queue with
/// all their children, so none outlives the test (task 317). The test is
/// timed while it is held (task 324), and a timeout, whose `process::exit`
/// skips the drop, kills them and removes the directory through
/// [`common::on_timeout`] (task 1580), which ends the shells' waits of
/// [`common::await_file`] whose parent is not the test.
pub struct Fixture {
    pub db: PathBuf,
    pub dir: TempDir,
    pub _test: common::Waiting,
    pub _on_timeout: common::Cleanup,
}

impl Fixture {
    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        kill_stubs(&self.db);
        common::service::unserve(&self.db);
    }
}

/// The process groups of the stub agents started per queue; a queue whose
/// fixture was dropped maps to `None` and starts no more.
pub static STUBS: LazyLock<Mutex<HashMap<PathBuf, Option<Vec<u32>>>>> =
    LazyLock::new(Default::default);

pub fn stubs() -> MutexGuard<'static, HashMap<PathBuf, Option<Vec<u32>>>> {
    STUBS.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn kill_stubs(db: &Path) {
    let groups = stubs().insert(db.into(), None).flatten();
    for group in groups.unwrap_or_default() {
        // SAFETY: kill(2) takes no pointer; a negative pid names the group.
        unsafe { libc::kill(-(group as libc::pid_t), libc::SIGKILL) };
    }
}

/// Starts the stub agents of the sessions on `db`, as `LocalSpawner` would
/// start Claude, but each in a process group of its own, which [`Fixture`]
/// kills, and with no stream of the test process: a stub left running does
/// not hold the pipe of `cargo test | grep` open.
pub struct StubSpawner {
    pub db: PathBuf,
}

impl Spawner for StubSpawner {
    fn spawn(&self, spec: &CommandSpec, streams: Streams<'_>) -> Result<Box<dyn Spawned>> {
        // A worker's `dagq` goes to the queue's service, which the
        // supervisor keeps running in production (goal 82's stage (3)).
        if spec
            .get_envs()
            .any(|(key, value)| key == "DAGQ_SERVICE_SOCKET" && value.is_some())
        {
            common::service::serve_with(&self.db, &fake_cmux_dir(&self.db).join("cmux"));
        }
        let mut stubs = stubs();
        let Some(groups) = stubs.entry(self.db.clone()).or_insert(Some(Vec::new())) else {
            bail!("the test's fixture is gone");
        };
        let mut command = process::command(spec);
        // The spec's own actor (a worker's, a job's), not the tests' one.
        command.without_actor_env();
        // A stub's `dagq` that names no `--cmux` finds the fake first: before
        // the spec's PATH (the last it sets, as `process::command` applies
        // them), the tests' own when it sets none, or alone when it removes it.
        let path = match spec.get_envs().filter(|(key, _)| *key == "PATH").last() {
            Some((_, value)) => value.map(ToOwned::to_owned).unwrap_or_default(),
            None => std::env::var_os("PATH").unwrap_or_default(),
        };
        command.env(
            "PATH",
            std::env::join_paths(
                std::iter::once(fake_cmux_dir(&self.db)).chain(std::env::split_paths(&path)),
            )?,
        );
        // A headless job's prompt, or nothing (task 1560).
        command.stdin(process::stdin(spec)?);
        match streams {
            // The agent's terminal.
            Streams::Inherit => {
                command.stdout(Stdio::null()).stderr(Stdio::null());
            }
            // A headless turn's output (ADR-t813-1).
            Streams::Files { stdout, stderr } => {
                command
                    .stdout(fs::File::create(stdout)?)
                    .stderr(fs::File::create(stderr)?);
            }
            other => panic!("an agent's streams go to its terminal or its files: {other:?}"),
        }
        // A command in a session of its own leads its group already.
        if !spec.get_new_session() {
            command.process_group(0);
        }
        let child = command.spawn()?;
        groups.push(child.id());
        Ok(Box::new(Stub(child)))
    }
}

/// The directory of a fake `cmux` next to the queue at `db`, which appends
/// the arguments of each call to `calls` there and exits at once: what the
/// stub agents' `dagq` (a notifying `ask`, a `stats`) resolves as `cmux`
/// when it names none, so no test reaches the host's cmux or its inbox
/// (task 1128).
pub fn fake_cmux_dir(db: &Path) -> PathBuf {
    let dir = db.parent().unwrap().join("fake-cmux");
    let stub = dir.join("cmux");
    if !stub.exists() {
        fs::create_dir_all(&dir).unwrap();
        // Written aside and renamed, so a stub started at the same time
        // never runs half a script.
        let aside = dir.join(format!("cmux.{:?}", thread::current().id()));
        crate::common::template::script(
            &aside,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"${0%/*}/calls\"\n",
        );

        fs::rename(&aside, &stub).unwrap();
    }
    dir
}

/// The host's processes as a supervisor in production sees them: a test's
/// sessions run inside the test process, which is also the supervisor, so
/// its stub agents (those [`StubSpawner`] started for `db`) are listed as
/// no child of it, like an agent under a wrapper in a terminal of its own.
pub struct DetachedStubs {
    pub db: PathBuf,
}

impl ProcessControl for DetachedStubs {
    fn alive(&self, pid: u32) -> bool {
        SystemProcesses.alive(pid)
    }
    fn terminate(&self, pid: u32) -> Result<()> {
        SystemProcesses.terminate(pid)
    }
    fn interrupt(&self, pid: u32) -> Result<()> {
        SystemProcesses.interrupt(pid)
    }
    fn kill(&self, pid: u32) -> Result<()> {
        SystemProcesses.kill(pid)
    }
    fn reap(&self, pid: u32) {
        SystemProcesses.reap(pid)
    }
    fn list(&self) -> Result<Vec<dagq::domain::recovery::ProcessInfo>> {
        let agents = stubs().get(&self.db).cloned().flatten().unwrap_or_default();
        let mut all = SystemProcesses.list()?;
        for process in &mut all {
            if agents.contains(&process.pid) {
                process.ppid = 1;
            }
        }
        Ok(all)
    }
    fn start_identity(&self, pid: u32) -> Option<String> {
        SystemProcesses.start_identity(pid)
    }
    fn started_at(&self, pid: u32) -> Option<i64> {
        SystemProcesses.started_at(pid)
    }
    fn descendants(&self, pid: u32) -> Vec<u32> {
        SystemProcesses.descendants(pid)
    }
}

pub struct Stub(pub Child);

impl Spawned for Stub {
    fn id(&self) -> u32 {
        self.0.id()
    }
    fn try_wait(&mut self) -> Result<Option<Exit>> {
        Ok(self.0.try_wait()?.map(process::exit))
    }
    fn kill(&mut self) -> Result<()> {
        Ok(self.0.kill()?)
    }
    fn wait(&mut self) -> Result<Exit> {
        Ok(process::exit(self.0.wait()?))
    }
    /// Every stub leads its own group.
    fn kill_group(&mut self) -> Result<()> {
        // SAFETY: kill(2) takes no pointer; a negative pid names the group.
        unsafe { libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL) };
        Ok(())
    }
}

/// Whether `pid` runs: neither gone nor a zombie.
pub fn running(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .bounded_output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    !stat.trim().is_empty() && !stat.trim().starts_with('Z')
}

pub fn fixture() -> (Fixture, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo's directory");
    fs::create_dir(&repo).unwrap();
    common::template::repository(&repo, "fixture\n");
    let db = dir.path().join("queue's data.db");
    let mut queue = common::template::queue(&db);
    add_ready_task(&mut queue, "test task", &[]);
    let on_timeout = {
        let (db, path) = (db.clone(), dir.path().to_owned());
        common::on_timeout(
            Duration::from_secs(20),
            format!("kill the stub agents of {} and remove it", db.display()),
            move || {
                kill_stubs(&db);
                let _ = fs::remove_dir_all(path);
            },
        )
    };
    (
        Fixture {
            db: db.clone(),
            dir,
            _test: common::test(),
            _on_timeout: on_timeout,
        },
        repo,
        db,
    )
}

pub fn add_ready_task(queue: &mut SqliteQueue, title: &str, dependencies: &[TaskId]) -> TaskId {
    let task = queue
        .add(NewTask {
            title: title.into(),
            description: "small change".into(),
            acceptance: "works".into(),
            verification_commands: vec!["test -f seed.txt".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            dependencies: dependencies.to_vec(),
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            wait_for_build: false,
        })
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    task.id()
}

/// The fake worker's own helpers, the same in an interactive session and in
/// a headless turn ([`TURN_PRELUDE`], goal 92): `receipt COMMIT [RUN_ID]`
/// writes an atomically renamed receipt claiming success with evidence on
/// every check, and `commit MESSAGE` rewrites `change.txt` and commits it.
macro_rules! worker_helpers {
    () => {
        r#"
test -f seed.txt || exit 99
receipt() {
  printf '{"run_id":"%s","result":"succeeded","commit":"%s","tests":{"status":"passed","evidence_or_reason":"ran"},"e2e":{"status":"not_applicable","evidence_or_reason":"no e2e surface"},"subagent_review":{"status":"passed","evidence_or_reason":"reviewed"},"summary":"done"}' "${2:-$RUN_ID}" "$1" > "$RECEIPT.tmp"
  mv "$RECEIPT.tmp" "$RECEIPT"
}
commit() { printf 'change by %s\n' "$RUN_ID" > change.txt && git add change.txt && git commit -q -m "$1"; }
"#
    };
}

/// The resumed worker's `receipt COMMIT [RESULT] [SUMMARY]`, the same in an
/// interactive resumed session and a headless resume turn.
macro_rules! resume_receipt {
    () => {
        r#"
receipt() {
  printf '{"run_id":"%s","result":"%s","commit":"%s","tests":{"status":"passed","evidence_or_reason":"reran after the rebase"},"e2e":{"status":"not_applicable","evidence_or_reason":"no e2e surface"},"subagent_review":{"status":"not_applicable","evidence_or_reason":"resumed session"},"summary":"%s"}' "$RUN_ID" "${2:-succeeded}" "$1" "${3:-resolved}" > "$RECEIPT.tmp"
  mv "$RECEIPT.tmp" "$RECEIPT"
}
"#
    };
}

/// The resumed worker's `resolve`, which rebases onto `$MAIN` (see
/// [`RESUME_PRELUDE`]).
macro_rules! resolve_helpers {
    () => {
        r#"
# The supervisor polls `git status` in the worktree. It runs with
# GIT_OPTIONAL_LOCKS=0 now, but a git that took index.lock for a moment
# there made a session's `git add` or `git commit` fail (and failed tests
# under load), so the scripts still guard against the lock. `resolve` therefore looks at the rebase after every step, as a
# person would: it resolves the conflict while the file or the index needs
# it, skips a pick that is already in HEAD, and continues until the rebase
# is over. A command that only found the lock is retried.
unlocked() {
  locked=0
  until out=$("$@" 2>&1); do
    case $out in *index.lock*) ;; *) return 1 ;; esac
    locked=$((locked + 1))
    [ "$locked" -lt 100 ] || return 1
    sleep 0.05
  done
}
resolve() {
  unlocked git rebase -q "$MAIN" && return
  steps=0
  while [ -d "$(git rev-parse --git-path rebase-merge)" ]; do
    steps=$((steps + 1))
    [ "$steps" -lt 100 ] || return 1
    if [ "$(cat change.txt)" != 'resolved by the resumed session' ] || [ -n "$(git ls-files -u)" ]; then
      printf 'resolved by the resumed session\n' > change.txt
      git add change.txt >/dev/null 2>&1
    elif git diff --cached --quiet HEAD; then
      git rebase --skip >/dev/null 2>&1
    else
      GIT_EDITOR=true git rebase --continue >/dev/null 2>&1
    fi || sleep 0.05
  done
}
"#
    };
}

/// Shell prelude for the fake agent: the [`worker_helpers`] (`receipt`,
/// `commit`), `idle` mimics Claude's Stop hook (`idle_bg` with background
/// work still running, `idle_bg_done` once it ended, as Claude Code
/// 2.1.281 writes `background_tasks`), and `await_exit` blocks until the
/// test workspace delivers the supervisor's exit request.
pub const AGENT_PRELUDE: &str = concat!(
    watchdog!(),
    crate::await_file_fn!(),
    worker_helpers!(),
    r#"
printf 'fixture log\n' > "$LOG"
idle() {
  printf '{"session_id":"%s","hook_event_name":"Stop","stop_hook_active":false}' "$RUN_ID" > "$IDLE.tmp"
  mv "$IDLE.tmp" "$IDLE"
}
idle_bg() {
  printf '{"session_id":"%s","hook_event_name":"Stop","stop_hook_active":false,"background_tasks":[{"id":"b1","type":"shell","status":"running","description":"cargo test","command":"cargo test"}]}' "$RUN_ID" > "$IDLE.tmp"
  mv "$IDLE.tmp" "$IDLE"
}
idle_bg_done() {
  printf '{"session_id":"%s","hook_event_name":"Stop","stop_hook_active":false,"background_tasks":[]}' "$RUN_ID" > "$IDLE.tmp"
  mv "$IDLE.tmp" "$IDLE"
}
await_exit() { await_file "$EXIT"; }
"#
);

pub const VALID_AGENT: &str = "commit work; receipt \"$(git rev-parse HEAD)\"";

/// The handle of the background wrapper the run recorded as its first
/// session (ADR-t1404-1), the fixture's default (task 1439): a pid and a
/// start, not a workspace.
pub fn background_session(run: &TaskRun) -> String {
    let session = run.workspace_id().expect("the run's session").to_owned();
    assert!(
        dagq::domain::background_wrapper::is_background(&session),
        "{session}"
    );
    session
}

/// Shell prelude for a resumed session: `await_message` blocks until the
/// supervisor's resolution request arrived (the test backend writes it to
/// `$MESSAGE`) and sets `$MAIN` to the main it names; `receipt` / `idle` /
/// `await_exit` are the worker's.
pub const RESUME_PRELUDE: &str = concat!(
    watchdog!(),
    crate::await_file_fn!(),
    resume_receipt!(),
    r#"
idle() {
  printf '{"session_id":"%s","hook_event_name":"Stop","stop_hook_active":false}' "$RUN_ID" > "$IDLE.tmp"
  mv "$IDLE.tmp" "$IDLE"
}
idle_bg() {
  printf '{"session_id":"%s","hook_event_name":"Stop","stop_hook_active":false,"background_tasks":[{"id":"b1","type":"shell","status":"running","description":"cargo test","command":"cargo test"}]}' "$RUN_ID" > "$IDLE.tmp"
  mv "$IDLE.tmp" "$IDLE"
}
idle_bg_done() {
  printf '{"session_id":"%s","hook_event_name":"Stop","stop_hook_active":false,"background_tasks":[]}' "$RUN_ID" > "$IDLE.tmp"
  mv "$IDLE.tmp" "$IDLE"
}
await_exit() { await_file "$EXIT"; }
await_message() {
  await_file "$MESSAGE"
  MAIN=$(sed -n 's/.*main is now \([0-9a-f]*\) .*/\1/p' "$MESSAGE" | head -n 1)
}
"#,
    resolve_helpers!()
);

mod turns;
pub use turns::*;
mod headless_stubs;
pub use headless_stubs::*;
mod lost_lease;
pub use lost_lease::*;
pub mod planner_prompt_bytes;
pub mod planner_turns;

pub struct TestProvider {
    pub script: String,
    /// The queue, which a script reads as `$DB` (the test's own look at
    /// it); its `$DAGQ ...` goes to the queue service, as a worker's does.
    pub db: PathBuf,
}

impl AgentProvider for TestProvider {
    fn resume_command(&self, run: &TaskRun) -> Result<CommandSpec> {
        let run_dir = run.run_dir().unwrap();
        let mut command = CommandSpec::new("/bin/sh");
        command
            .current_dir(run.worktree_path().unwrap())
            .env("RUN_ID", run.id().as_str())
            .env("RECEIPT", run.receipt_path().unwrap())
            .env("IDLE", run.idle_marker_path().unwrap())
            .env("EXIT", exit_request_path(run_dir))
            .env("MESSAGE", resume_message_path(run_dir))
            .env("DAGQ", env!("CARGO_BIN_EXE_dagq"))
            .env("DB", &self.db)
            .arg("-c")
            .arg(format!("{RESUME_PRELUDE}\n{}", self.script));
        worker_env(&mut command, run);
        Ok(command)
    }
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn wait_interval(&self) -> Duration {
        TEST_TICK
    }
    fn review_command(&self, _: &TaskRun, _: &str, _: JobAccess) -> Result<CommandSpec> {
        unreachable!("sessions do not review")
    }
    // `headless_command` keeps the default refusal: a run's provider has no
    // headless job, which the observer test relies on.
    fn command(&self, run: &TaskRun, prompt: &str) -> Result<CommandSpec> {
        assert!(prompt.contains("Acceptance criteria:"));
        assert!(
            prompt.contains(
                "Verification commands (integrate runs them once after rebasing onto main;"
            )
        );
        // Every context section is present whether or not it has entries.
        assert!(prompt.contains("Goal"));
        assert!(prompt.contains("Context"));
        assert!(prompt.contains("Predecessor tasks"));
        assert!(prompt.contains("Sibling tasks in progress"));
        // A question goes to the queue as an ask, not to the terminal.
        assert!(prompt.contains(&format!(
            "`dagq ask --run {} --kind worker_question --because scope --topic <code> --question '...'`",
            run.id()
        )));
        // Background work is stopped before the receipt.
        assert!(prompt.contains(runtime::STOP_BACKGROUND), "{prompt}");
        let mut command = CommandSpec::new("/bin/sh");
        command
            .current_dir(run.worktree_path().unwrap())
            .env("RUN_ID", run.id().as_str())
            .env("RECEIPT", run.receipt_path().unwrap())
            .env("LOG", run.log_path().unwrap())
            .env("BASE", run.base_commit().as_str())
            .env("IDLE", run.idle_marker_path().unwrap())
            .env("EXIT", exit_request_path(run.run_dir().unwrap()))
            .env("MESSAGE", resume_message_path(run.run_dir().unwrap()))
            .env("DAGQ", env!("CARGO_BIN_EXE_dagq"))
            .env("DB", &self.db)
            .arg("-c")
            .arg(format!("{AGENT_PRELUDE}\n{}", self.script));
        worker_env(&mut command, run);
        Ok(command)
    }
}

/// The fake agent runs as the run's worker, as a real session does, rather
/// than as whatever actor runs the tests: its `$DAGQ ask --run` is its own
/// run's (task 733).
fn worker_env(command: &mut CommandSpec, run: &TaskRun) {
    for (name, value) in dagq::domain::ActorContext::worker(run.id(), run.task_id()).env() {
        command.env(name, value);
    }
}

/// The test backend delivers an exit request as a file the fake agent polls for.
pub fn exit_request_path(run_dir: &str) -> PathBuf {
    Path::new(run_dir).join("exit-requested")
}

/// ... and the text typed into a resumed session the same way.
pub fn resume_message_path(run_dir: &str) -> PathBuf {
    Path::new(run_dir).join("resume-message")
}

/// One session the test backend started, keyed by its background handle.
pub struct TestSession {
    pub run_id: RunId,
    pub run_dir: String,
    pub worker: Option<thread::JoinHandle<Result<Value>>>,
}

/// Starts each run's session wrapper on a thread as a background session
/// with a process of its own ([`headless::launch_background`]; a run opens
/// no workspace, ADR-t1433-3), with the agent script chosen per task, and
/// records the stops per handle. Planner and inbox workspaces are only
/// listed and closed.
pub struct TestWorkspace {
    pub db: PathBuf,
    pub fail: bool,
    /// The tasks whose background launch fails as with `fail` (task 1482).
    pub fail_tasks: Mutex<Vec<TaskId>>,
    pub close_fail: bool,
    pub script: String,
    pub scripts: Mutex<HashMap<TaskId, String>>,
    pub exit_timeout: Duration,
    pub registration_timeout: Duration,
    /// How long after an attempt to reopen a headless run's lost session
    /// the supervisor makes the next one (task 1372).
    pub reopen_interval: Duration,
    /// The background launch of a first session starts no wrapper, so it
    /// never registers.
    pub no_session: bool,
    /// The background launch of a resume starts no wrapper, so it never
    /// registers (a reopen whose wrapper does not start).
    pub resume_no_session: bool,
    pub resume_timeout: Duration,
    /// How often supervision attempted a screen read through this backend
    /// (a worker has no screen, so none should).
    pub captures: AtomicUsize,
    pub sessions: Mutex<Vec<(String, TestSession)>>,
    pub closed: Mutex<Vec<String>>,
    /// `notify` calls as (title, body, workspace); the supervisor sends
    /// none, `ask` one per new ask (ADR-0022).
    pub notifications: Mutex<Vec<(String, String, Option<String>)>>,
    /// Every `ensure_group` call, as (external ID, name).
    pub groups: Mutex<Vec<(String, String)>>,
    /// Resumed-session script per task; a resume of any other task fails.
    pub resume_scripts: Mutex<HashMap<TaskId, String>>,
    /// `launch_background` calls (ADR-t1404-1): the directory, the command
    /// and the environment of each ([`headless::launch_background`]).
    pub launched: Mutex<Vec<headless::Launch>>,
    /// The processes of the background sessions `no_session` or
    /// `resume_no_session` started without a wrapper, by handle, until
    /// their close stops them as it stops a background wrapper.
    pub stands: Mutex<Vec<(String, headless::Stand)>>,
    /// `send_text` calls: the session and the text.
    pub texts: Mutex<Vec<(String, String)>>,
    /// `exists` (and the listing) fails, as asking about a session can.
    pub exists_fails: bool,
    /// Sessions the backend reports open although it did not start them (a
    /// run's wrapper from an earlier supervisor, a workspace from before
    /// ADR-t1433-3, a planner's workspace), until they are stopped.
    pub listed: Mutex<Vec<String>>,
    /// Sessions the backend reports gone for now although they run.
    pub hidden: Mutex<Vec<String>>,
    /// `close` ends the session, as stopping a background wrapper does,
    /// instead of requiring it gone.
    pub close_ends_session: bool,
    /// `close` times out and leaves the session as it is.
    pub close_times_out: bool,
    /// The `claude` a headless run's wrapper calls for its turns
    /// ([`headless_claude`]); a headless run fails its wrapper without one.
    pub headless: Option<PathBuf>,
    /// Wait for this per-turn marker before the wrapper starts its timers.
    pub headless_ready: Option<PathBuf>,
    /// The `codex` a headless Codex run's wrapper calls for its turns
    /// ([`headless_codex`]).
    pub codex: Option<PathBuf>,
    /// The sccache a headless run's wrapper takes its environment to name
    /// as `RUSTC_WRAPPER` (ADR-t1215-1); `None` names none.
    pub sccache: Option<dagq::domain::sccache::SccacheTarget>,
    /// With `sccache`: what the turns inherit as if the wrapper's
    /// environment held it (the wrapper's `[run.env]` in production),
    /// set before each turn's own variables ([`headless::InheritingSpawner`]).
    pub inherited_env: Vec<(String, String)>,
}

impl TestWorkspace {
    pub fn new(db: &Path, fail: bool, script: &str) -> Self {
        Self {
            db: db.into(),
            fail,
            fail_tasks: Mutex::new(Vec::new()),
            close_fail: false,
            script: script.into(),
            scripts: Mutex::new(HashMap::new()),
            exit_timeout: Duration::from_secs(120),
            registration_timeout: Duration::from_secs(45),
            reopen_interval: Duration::from_secs(60),
            no_session: false,
            resume_no_session: false,
            resume_timeout: Duration::from_secs(120),
            captures: AtomicUsize::new(0),
            sessions: Mutex::new(Vec::new()),
            closed: Mutex::new(Vec::new()),
            notifications: Mutex::new(Vec::new()),
            groups: Mutex::new(Vec::new()),
            resume_scripts: Mutex::new(HashMap::new()),
            launched: Mutex::new(Vec::new()),
            stands: Mutex::new(Vec::new()),
            texts: Mutex::new(Vec::new()),
            exists_fails: false,
            listed: Mutex::new(Vec::new()),
            hidden: Mutex::new(Vec::new()),
            close_ends_session: false,
            close_times_out: false,
            headless: None,
            headless_ready: None,
            codex: None,
            sccache: None,
            inherited_env: Vec::new(),
        }
    }
    /// Report `session` open as if an earlier supervisor started it.
    pub fn list(&self, session: &str) {
        self.listed.lock().unwrap().push(session.into());
    }
    /// Resumed-session script for one task.
    pub fn resume_script_for(&self, task_id: i64, script: &str) {
        self.resume_scripts
            .lock()
            .unwrap()
            .insert(TaskId::new(task_id), script.into());
    }
    pub fn texts(&self) -> Vec<(String, String)> {
        self.texts.lock().unwrap().clone()
    }

    /// Agent script for one task; other tasks use the default script.
    pub fn script_for(&self, task_id: i64, script: &str) {
        self.scripts
            .lock()
            .unwrap()
            .insert(TaskId::new(task_id), script.into());
    }
    pub fn closed(&self) -> Vec<String> {
        self.closed.lock().unwrap().clone()
    }
    /// Wait for every session wrapper started so far to return successfully.
    /// A wrapper's error comes with the queue's events, which tell what the
    /// supervisor did to the run meanwhile (task 1274).
    pub fn join(&self) {
        let workers: Vec<_> = self
            .sessions
            .lock()
            .unwrap()
            .iter_mut()
            .filter_map(|(id, s)| s.worker.take().map(|worker| (id.clone(), worker)))
            .collect();
        for (id, worker) in workers {
            let returned = joined(
                worker,
                format!("the session wrapper of {id} to return (its stub agent to exit)"),
            );
            if let Err(error) = returned {
                print_queue_events(&self.db);
                panic!("the session wrapper of {id} failed: {error:#}");
            }
        }
    }
    pub fn session_run_dir(&self, workspace_id: &str) -> String {
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _)| id == workspace_id)
            .map(|(_, s)| s.run_dir.clone())
            .expect("the session was started")
    }
}

impl WorkspaceBackend for TestWorkspace {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn preflight_detached(&self, _: &SupervisorEnvironment) -> Result<()> {
        unreachable!("only up preflights the detached connection")
    }
    fn launch_background(
        &self,
        cwd: &Path,
        command: &str,
        env: &[(String, String)],
        log: &Path,
    ) -> Result<String> {
        headless::launch_background(self, cwd, command, env, log)
    }
    fn send_text(&self, workspace_id: &str, text: &str) -> Result<()> {
        self.texts
            .lock()
            .unwrap()
            .push((workspace_id.to_owned(), text.to_owned()));
        bail!("a worker has no interactive input: {workspace_id}, {text}")
    }
    fn send_enter(&self, _: &str) -> Result<()> {
        bail!("a worker has no interactive input")
    }
    fn send_key(&self, workspace_id: &str, key: &str) -> Result<()> {
        bail!("a worker has no interactive input: {workspace_id}, {key}")
    }
    fn resume_prompt_delay(&self) -> Duration {
        Duration::ZERO
    }
    fn submit_check_interval(&self) -> Duration {
        Duration::from_millis(10)
    }
    fn resume_timeout(&self) -> Duration {
        self.resume_timeout
    }
    fn capture(&self, _: &str) -> Result<String> {
        self.captures.fetch_add(1, Ordering::SeqCst);
        bail!("a worker has no screen")
    }
    fn retry_backoff(&self) -> Duration {
        Duration::from_millis(10)
    }
    fn close(&self, workspace_id: &str) -> Result<()> {
        ensure!(
            !self.close_times_out,
            "cmux close-workspace failed: Command timed out"
        );
        // The session must have exited (or died, its wrapper's pid gone)
        // before the supervisor stops its handle. A handle this backend did
        // not start (an orphan's) has no session here.
        let run_id = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _)| id == workspace_id)
            .map(|(_, s)| s.run_id.clone());
        let connection = Connection::open(&self.db)?;
        if self.close_ends_session
            && let Some(run_id) = &run_id
        {
            fs::write(exit_request_path(&self.session_run_dir(workspace_id)), "")?;
            let started = Instant::now();
            while !connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM run_processes WHERE run_id=?1 AND role='wrapper' AND exited_at IS NOT NULL)",
                [run_id],
                |r| r.get::<_, bool>(0),
            )? {
                ensure!(
                    started.elapsed() < Duration::from_secs(30),
                    "session did not end with its wrapper's stop"
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
        if let Some(run_id) = &run_id {
            // A reopen's session whose wrapper never registered has no row.
            let row: Option<(bool, u32)> = connection
                .query_row(
                    "SELECT exited_at IS NOT NULL, pid FROM run_processes WHERE run_id=?1 AND role='wrapper'",
                    [run_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            assert!(row.is_none_or(|(exited, pid)| exited || !pid_alive(pid)));
        } else {
            let live: Vec<u32> = connection
                .prepare(
                    "SELECT p.pid FROM run_processes p JOIN task_runs r ON r.id=p.run_id
                     WHERE r.workspace_id=?1 AND p.role='wrapper' AND p.exited_at IS NULL",
                )?
                .query_map([workspace_id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            assert!(live.into_iter().all(|pid| !pid_alive(pid)));
        }
        if self.close_fail {
            bail!("injected session stop failure");
        }
        // A background session without a wrapper is stopped.
        self.stands
            .lock()
            .unwrap()
            .retain(|(handle, _)| handle != workspace_id);
        self.closed.lock().unwrap().push(workspace_id.into());
        Ok(())
    }

    fn set_color(&self, _: &str, _: &str) -> Result<()> {
        unreachable!("only up colors a workspace")
    }
    fn set_status(&self, _: &str, _: &str, _: &str, _: &str) -> Result<()> {
        unreachable!("only up puts a status pill on a workspace")
    }
    fn pin(&self, _: &str) -> Result<()> {
        unreachable!("only up pins a workspace")
    }
    fn send_exit(&self, workspace_id: &str) -> Result<()> {
        bail!("a worker uses an exit request file: {workspace_id}")
    }
    fn exit_timeout(&self) -> Duration {
        self.exit_timeout
    }
    fn registration_timeout(&self) -> Duration {
        self.registration_timeout
    }
    fn reopen_interval(&self) -> Duration {
        self.reopen_interval
    }
    // A session is open from its start until it is stopped, as a background
    // wrapper runs until its stop; one this backend never started is not,
    // unless `list` names it.
    fn exists(&self, workspace_id: &str) -> Result<bool> {
        ensure!(!self.exists_fails, "injected session list failure");
        Ok(self
            .listed_workspace_ids()?
            .iter()
            .any(|listed| listed == workspace_id))
    }
    fn listed_workspace_ids(&self) -> Result<Vec<String>> {
        ensure!(!self.exists_fails, "injected session list failure");
        let closed = self.closed();
        let mut listed: Vec<String> = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, _)| id.clone())
            .collect();
        listed.extend(self.listed.lock().unwrap().iter().cloned());
        let hidden = self.hidden.lock().unwrap();
        listed.retain(|id| !closed.contains(id) && !hidden.contains(id));
        Ok(listed)
    }
    fn create_named(&self, _: &str, _: &Path, _: &str, _: &WorkspaceTags) -> Result<String> {
        bail!("not used by the supervisor")
    }
    fn ensure_group(&self, external_id: &str, name: &str) -> Result<String> {
        self.groups
            .lock()
            .unwrap()
            .push((external_id.into(), name.into()));
        Ok(format!("group-{external_id}"))
    }
    fn notify(&self, title: &str, body: &str, workspace: Option<&str>) -> Result<()> {
        self.notifications.lock().unwrap().push((
            title.into(),
            body.into(),
            workspace.map(Into::into),
        ));
        Ok(())
    }
}

/// The providers of a headless run's wrapper (ADR-t813-1): the run's
/// provider's own turns and reader (Claude Code's calling the stub `claude`
/// at `claude`, Codex's the stub `codex` at `codex`), with the test tick,
/// and the other provider's, which the run's turns go to once the
/// supervisor moves it there (ADR-t813-2). Every worker run is headless
/// (task 1437); the run must be.
pub(crate) fn headless_provider(
    run: &TaskRun,
    claude: Option<&Path>,
    codex: Option<&Path>,
) -> (HeadlessProvider, HeadlessProvider) {
    // Claim and resume start every worker run headless (task 1437).
    assert_eq!(
        run.worker_mode(),
        dagq::domain::worker::WorkerMode::Headless,
        "a worker session of {}",
        run.id()
    );
    let provider = run.actual_provider();
    (
        headless_of(provider, claude, codex),
        headless_of(provider.other(), claude, codex),
    )
}

/// `provider`'s headless turns with its stub.
fn headless_of(
    provider: dagq::domain::Provider,
    claude: Option<&Path>,
    codex: Option<&Path>,
) -> HeadlessProvider {
    {
        let agent: Box<dyn AgentProvider + Send> = match provider {
            dagq::domain::Provider::Codex => {
                let executable = codex
                    .unwrap_or(Path::new("/nonexistent/headless-codex"))
                    .to_owned();
                Box::new(dagq::infrastructure::codex::Codex {
                    // The stub's rollouts, never the person's `~/.codex`.
                    home: executable.parent().map(|dir| dir.join(CODEX_HOME)),
                    executable,
                })
            }
            dagq::domain::Provider::Claude => {
                Box::new(dagq::infrastructure::adapters::ClaudeCode {
                    executable: claude
                        .unwrap_or(Path::new("/nonexistent/headless-claude"))
                        .to_owned(),
                })
            }
        };
        HeadlessProvider { agent }
    }
}

/// A provider's headless turns (the real `turn_command` and reader) with
/// the stub `claude` of [`headless_claude`] or `codex` of
/// [`headless_codex`]; a turn's model and effort go on its command line as
/// they would.
pub struct HeadlessProvider {
    pub agent: Box<dyn AgentProvider + Send>,
}

impl AgentProvider for HeadlessProvider {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn command(&self, _: &TaskRun, _: &str) -> Result<CommandSpec> {
        bail!("a headless worker starts no interactive session")
    }
    fn resume_command(&self, _: &TaskRun) -> Result<CommandSpec> {
        bail!("a headless worker starts no interactive session")
    }
    fn review_command(&self, _: &TaskRun, _: &str, _: JobAccess) -> Result<CommandSpec> {
        unreachable!("sessions do not review")
    }
    fn wait_interval(&self) -> Duration {
        TEST_TICK
    }
    fn select_model(&self, command: &mut CommandSpec, model: &str, effort: &str) {
        self.agent.select_model(command, model, effort);
    }
    fn turn_command(
        &self,
        target: &dagq::application::TurnTarget<'_>,
        prompt: &str,
        session: dagq::domain::turn::TurnSession<'_>,
    ) -> Result<CommandSpec> {
        self.agent.turn_command(target, prompt, session)
    }
    fn turn_reader(&self) -> Result<Box<dyn dagq::application::TurnReader>> {
        self.agent.turn_reader()
    }
    fn turn_permission_mode(&self, broker_required: bool) -> Option<&'static str> {
        self.agent.turn_permission_mode(broker_required)
    }
    fn turn_session_from_output(&self) -> bool {
        self.agent.turn_session_from_output()
    }
}

/// The arguments of each call the stub of [`headless_codex`] got for `run`,
/// each ended by `|`.
pub fn stub_args(run: &TaskRun) -> Vec<String> {
    fs::read_to_string(Path::new(run.run_dir().unwrap()).join("stub-args.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// What the stub of [`headless_claude`] in `dir` does in a turn: a shell
/// script, usually a `case "$TURN"`.
pub fn set_turns(dir: &Path, script: &str) {
    fs::write(dir.join("turn.sh"), script).unwrap();
}

/// The calls the stub of [`headless_claude`] got for `run`, one line each:
/// `<start|resume> <session> <prompt's first line>`.
pub fn stub_calls(run: &TaskRun) -> Vec<String> {
    fs::read_to_string(Path::new(run.run_dir().unwrap()).join("stub-calls.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// /bin/sh --version is not portable; a tiny standalone provider preflight stub.
pub fn claude_stub(db: &Path) -> PathBuf {
    let stub = db.parent().unwrap().join("claude-stub");
    // A live session's recovery job (its prompt on stdin in print mode)
    // escalates (so the alert's ask opens); anything else prints no
    // verdict.
    crate::common::template::script(
        &stub,
        format!(
            "#!/bin/sh\nPROMPT=\"$*\"; [ \"$1\" = -p ] && PROMPT=$(cat)\ncase \"$PROMPT\" in *\"{LIVE_RECOVERY}\"*) printf '%s\\n' '{ESCALATE}' ;; *) printf 'test provider\\n' ;; esac\n"
        ),
    );

    stub
}

/// The supervisor's pass and idle intervals and the wrapper's wait interval
/// in these tests: short, so a run's steps follow each other without a
/// second's pause. A test waits some of them at every step of a run: 20 ms
/// instead of 50 shortened most runtime tests (task 567).
pub const TEST_TICK: Duration = Duration::from_millis(20);

/// How many passes of the supervisor a check that nothing happens (past a
/// threshold, or at all) waits for with [`await_passes`]: the fixed sleeps
/// it replaced let the 20 ms tick run some dozens of passes unloaded, and a
/// handful under load (task 1046).
pub const SOME_PASSES: u64 = 5;

/// Wait for `n` whole passes of the supervisor whose
/// [`SuperviseOptions::passes`] `passes` is, all started after this call: a
/// check that nothing happens past a threshold waits out the threshold and
/// then these passes, instead of a fixed sleep well past it (task 1046).
pub fn await_passes(passes: &AtomicU64, n: u64) {
    // The pass in progress now may have looked before the call: the next
    // `n` start after it, and the start of the one after them shows the
    // last of them ended.
    let from = passes.load(Ordering::SeqCst);
    let target = from + n + 1;
    let started = Instant::now();
    loop {
        let now = passes.load(Ordering::SeqCst);
        if now >= target {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the supervisor made {} of the {n} passes waited for in 60 seconds",
            now.saturating_sub(from + 1)
        );
        thread::sleep(Duration::from_millis(5));
    }
}

/// The wall clock's unix second, as the queue's times and the markers'
/// seconds count it.
pub fn unix_second_now() -> i64 {
    unix_second(SystemTime::now())
}

/// The unix second of `at`.
pub fn unix_second(at: SystemTime) -> i64 {
    i64::try_from(at.duration_since(UNIX_EPOCH).unwrap().as_secs()).unwrap()
}

/// The unix second `path` was last written in.
pub fn modified_second(path: &Path) -> i64 {
    unix_second(fs::metadata(path).unwrap().modified().unwrap())
}

/// The unix time in milliseconds `path` was last written at.
pub fn modified_millis(path: &Path) -> i64 {
    let modified = fs::metadata(path).unwrap().modified().unwrap();
    i64::try_from(modified.duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap()
}

/// Wait until `path` is written after `at_ms` (unix milliseconds), e.g. a
/// session's idle marker after its question: a check that nothing happens
/// while it is idle starts from that idle (task 1075).
pub fn await_written_after(path: &Path, at_ms: i64) {
    let started = Instant::now();
    while fs::metadata(path).is_err() || modified_millis(path) <= at_ms {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{} was not written after {at_ms} in 30 seconds",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// When the first `kind` event of `detail` was recorded, in unix
/// milliseconds.
pub fn first_event_millis(detail: &dagq::domain::TaskDetail, kind: &str) -> i64 {
    detail
        .events
        .iter()
        .find(|event| event.kind == kind)
        .and_then(|event| dagq::domain::stats::timestamp_millis(&event.created_at))
        .unwrap_or_else(|| panic!("no {kind} event"))
}

/// Wait until the wall clock is past unix second `second`, so what follows
/// falls in a later second than it: instead of a fixed 1.1 s sleep, which
/// waits a whole second also when the boundary is a few ms away (task
/// 1075).
pub fn await_second_after(second: i64) {
    let started = Instant::now();
    while unix_second_now() <= second {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the clock did not pass unix second {second} in 5 seconds"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// Supervisor options with the test tick and the [`SteadyClock`].
pub fn supervise_options(parallel: usize, once: bool) -> SuperviseOptions {
    let base = SuperviseOptions::new(parallel, once);
    SuperviseOptions {
        tick: TEST_TICK,
        idle_poll: TEST_TICK,
        generators: Generators {
            clock: Arc::new(SteadyClock(
                SystemTime::now(),
                Instant::now(),
                MonotonicAhead::default(),
            )),
            ..clock::system()
        },
        // A development build never looks for a release (ADR-t618-1), so
        // no test reaches crates.io even when Cargo.toml names a release.
        release_current: Some("0.0.0-dev+test".to_owned()),
        // No Codex worker unless a test gives its stub: the host's `codex`
        // is not these tests'.
        codex: PathBuf::from("/nonexistent/codex"),
        // An update's or a release's job that starts a supervisor again
        // does not reach the host's cmux (task 1128).
        update: dagq::application::supervise::UpdateSettings {
            cmux: Some(PathBuf::from("/usr/bin/true")),
            ..base.update.clone()
        },
        ..base
    }
}

/// The Claude Code scratchpad of the session run in `worktree` under
/// `root` (task 1100): the path with each character but an ASCII letter or
/// digit turned into `-`, written here without the runtime's function.
pub fn scratchpad_of(root: &Path, worktree: &str) -> PathBuf {
    root.join(
        worktree
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>(),
    )
}

/// The clock of one supervisor in these tests: the wall clock when its
/// options were made, advanced by the monotonic clock. The supervisor's
/// heartbeat thread waits its 2 seconds on the monotonic clock, which stops
/// while the host sleeps (`CLOCK_UPTIME_RAW` on macOS), while the wall
/// clock jumps by the time asleep; a jump past `HEARTBEAT_TIMEOUT_SECS`
/// made the loop's next lease-checked write fail with "run lease is missing
/// or stale" before the heartbeat caught up. This clock does not jump, and
/// agrees with the wall clock again for the next supervisor, so the times
/// a test writes with `unixepoch()` before it supervises still hold.
/// Its monotonic clock, which the supervisor's waits (registration, resume,
/// exit, reopen) are measured on, is the host's moved on by the
/// [`MonotonicAhead`] a test sets (task 1557).
pub struct SteadyClock(pub SystemTime, pub Instant, pub MonotonicAhead);

impl Clock for SteadyClock {
    fn system_time(&self) -> SystemTime {
        self.0 + self.1.elapsed()
    }

    fn monotonic(&self) -> Instant {
        Instant::now() + self.2.get()
    }
}

/// How far a [`SteadyClock`]'s monotonic clock is ahead of the host's: a
/// test moves it on to run a supervisor's wait out at once, instead of
/// sleeping through it. Shared by the clock and the test.
#[derive(Clone, Default)]
pub struct MonotonicAhead(Arc<AtomicU64>);

impl MonotonicAhead {
    /// Move the clock on by `by` from where it is.
    pub fn by(&self, by: Duration) {
        let ms = u64::try_from(by.as_millis()).unwrap();
        self.0.fetch_add(ms, Ordering::SeqCst);
    }

    fn get(&self) -> Duration {
        Duration::from_millis(self.0.load(Ordering::SeqCst))
    }
}

/// [`supervise_options`] whose clock's monotonic time the returned
/// [`MonotonicAhead`] moves on.
pub fn supervise_options_ahead(parallel: usize, once: bool) -> (SuperviseOptions, MonotonicAhead) {
    let ahead = MonotonicAhead::default();
    let mut options = supervise_options(parallel, once);
    options.generators.clock = Arc::new(SteadyClock(
        SystemTime::now(),
        Instant::now(),
        ahead.clone(),
    ));
    (options, ahead)
}

/// One pass of the parallel supervisor: claim whatever is ready, finish it, exit.
pub fn supervise(db: &Path, repo: &Path, backend: &TestWorkspace) -> Result<Value> {
    supervise_with(db, repo, backend, &supervise_options(4, true))
}

pub fn supervise_with(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    options: &SuperviseOptions,
) -> Result<Value> {
    let mut options = options.clone();
    options.retry_unreadable_review = false;
    supervise_retrying_with(db, repo, backend, &options)
}

/// Keep the production retry for tests of unreadable reviews and their asks.
/// Other tests use [`supervise`] / [`supervise_with`] to skip that extra job.
pub fn supervise_retrying(db: &Path, repo: &Path, backend: &TestWorkspace) -> Result<Value> {
    supervise_retrying_with(db, repo, backend, &supervise_options(4, true))
}

/// Like [`supervise_with`], retaining the caller's review retry policy.
pub fn supervise_retrying_with(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    options: &SuperviseOptions,
) -> Result<Value> {
    let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
    let _diagnostics = supervise_diagnostics(db);
    runtime::supervise(
        db,
        repo,
        backend,
        &claude_stub(db),
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        options,
    )
}

/// Keep stacks in a separate hook: a stuck SQLite read or stub registry
/// lock must not prevent sampling the supervisor. Hooks run newest first,
/// before the fixture kills its stubs.
fn supervise_diagnostics(db: &Path) -> (common::Cleanup, common::Cleanup) {
    let db = db.to_owned();
    let events = common::on_timeout(
        Duration::from_secs(10),
        format!("print the events of the queue {}", db.display()),
        move || print_queue_events(&db),
    );
    let stacks = common::on_timeout(
        Duration::from_secs(20),
        "print the test process's thread stacks",
        thread_stacks::print,
    );
    (events, stacks)
}

/// How many of the latest events [`print_queue_events`] prints.
const EVENTS_PRINTED: i64 = 200;

/// Write the queue's latest events and its unclosed asks to the process's
/// stderr, oldest first, for a supervise that timed out. It runs on the
/// timeout monitor's cleanup thread while the supervisor may still write,
/// so it only reads; what cannot be read is said instead.
pub fn print_queue_events(db: &Path) {
    use std::io::Write as _;
    let read = || -> rusqlite::Result<Vec<String>> {
        let connection =
            Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(Duration::from_secs(2))?;
        let mut lines: Vec<String> = connection
            .prepare(
                "SELECT id, created_at, task_id, run_id, kind, substr(payload, 1, 300)
                 FROM run_events ORDER BY id DESC LIMIT ?1",
            )?
            .query_map([EVENTS_PRINTED], |r| {
                Ok(format!(
                    "  event {} {} task {:?} run {:?} {} {}",
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        lines.reverse();
        lines.extend(
            connection
                .prepare("SELECT id, kind, run_id FROM asks WHERE closed_at IS NULL ORDER BY id")?
                .query_map([], |r| {
                    Ok(format!(
                        "  unclosed ask {} {} run {:?}",
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
        Ok(lines)
    };
    let text = match read() {
        Ok(lines) => format!(
            "the latest events of the queue {} (oldest first):\n{}\n",
            db.display(),
            lines.join("\n")
        ),
        Err(error) => format!(
            "the events of the queue {} could not be read: {error}\n",
            db.display()
        ),
    };
    let _ = std::io::stderr().lock().write_all(text.as_bytes());
    print_stub_processes(db);
}

/// The processes left in the groups of the stub agents started on `db`,
/// so that a stub that stopped making progress shows where it is.
fn print_stub_processes(db: &Path) {
    use std::io::Write as _;
    let groups: Vec<String> = stubs()
        .get(db)
        .cloned()
        .flatten()
        .unwrap_or_default()
        .iter()
        .map(u32::to_string)
        .collect();
    // `ps` returns at once; the bounded output would wait on the limit
    // this runs past.
    let listed = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,pgid=,stat=,etime=,command="])
        .output();
    let text = match listed {
        Ok(out) => {
            let lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|line| {
                    line.split_whitespace()
                        .nth(2)
                        .is_some_and(|pgid| groups.iter().any(|g| g == pgid))
                })
                .map(|line| format!("  {}", line.chars().take(300).collect::<String>()))
                .collect();
            format!(
                "the processes of the stub groups {groups:?}:\n{}\n",
                lines.join("\n")
            )
        }
        Err(error) => format!("the stub processes could not be listed: {error}\n"),
    };
    let _ = std::io::stderr().lock().write_all(text.as_bytes());
}

/// The asks other than `approve_landing`: those the supervisor opens for a
/// run whose stand-in review printed no verdict (task 328) go with every
/// run these tests leave awaiting integration.
pub fn other_asks(queue: &mut SqliteQueue, all: bool) -> Vec<dagq::domain::Ask> {
    queue
        .asks(AskQuery {
            all,
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .filter(|ask| ask.kind != AskKind::ApproveLanding)
        .collect()
}

/// Wait for the supervisor thread and its sessions and check it recorded no
/// error; a wait past its limit prints the queue's events first, so that
/// where the run stalled can be read from the failure (task 679).
pub fn finished(
    db: &Path,
    backend: &TestWorkspace,
    supervisor: thread::JoinHandle<Result<Value>>,
) -> Value {
    let _diagnostics = supervise_diagnostics(db);
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    outcome
}

/// Join `thread`, failing the test with `what` if it has not returned
/// within [`common::STEP_LIMIT`]; a panic in it fails the test as is.
pub fn joined<T>(thread: thread::JoinHandle<T>, what: impl Into<String>) -> T {
    let _waiting = common::within(common::STEP_LIMIT, what);
    thread
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// Poll the queue until `condition` holds or the deadline passes; a
/// timeout names the caller's line.
#[track_caller]
pub fn wait_until(
    db: &Path,
    timeout: Duration,
    mut condition: impl FnMut(&mut SqliteQueue) -> bool,
) {
    let started = Instant::now();
    let mut queue = SqliteQueue::open(db).unwrap();
    while !condition(&mut queue) {
        if started.elapsed() >= timeout {
            print_wait_diagnostics(db);
            panic!("condition not met within {timeout:?}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Print the queue's events and the test process's thread stacks before a
/// wait gives up, as a supervise past its limit does: a supervisor held in
/// a call (as one in a read of a pipe another process inherited was, task
/// 1017) shows where, which the timeout alone does not. The stacks come
/// first, as in [`supervise_diagnostics`]: a stuck read or stub lock must
/// not keep the supervisor from being sampled.
pub fn print_wait_diagnostics(db: &Path) {
    thread_stacks::print();
    print_queue_events(db);
}

/// Run one fake agent script through supervise and return the task detail.
pub fn run_agent(script: &str) -> (Fixture, PathBuf, dagq::domain::TaskDetail) {
    run_agent_with(script, false)
}

pub fn run_agent_with(
    script: &str,
    close_fail: bool,
) -> (Fixture, PathBuf, dagq::domain::TaskDetail) {
    run_agent_with_review_retry(script, close_fail, false)
}

fn run_agent_with_review_retry(
    script: &str,
    close_fail: bool,
    retry: bool,
) -> (Fixture, PathBuf, dagq::domain::TaskDetail) {
    warm_dagq();
    let (dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, script);
    backend.close_fail = close_fail;
    let mut options = supervise_options(4, true);
    options.retry_unreadable_review = retry;
    let outcome = supervise_retrying_with(&db, &repo, &backend, &options).unwrap();
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 1);
    assert_eq!(outcome["errors"], json!([]));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    let run = &detail.runs[0];
    // The worker the test chose (headless always non-interactive).
    assert_eq!(
        run.worker_mode(),
        dagq::domain::worker::WorkerMode::Headless
    );
    assert_eq!(outcome["runs"][0]["id"], json!(run.id()));
    // Runs live in `runs/` next to the (canonicalized) database, worktree inside.
    let run_dir = db
        .canonicalize()
        .unwrap()
        .with_file_name("runs")
        .join(run.id().as_str());
    assert_eq!(Path::new(run.run_dir().unwrap()), run_dir);
    assert_eq!(
        Path::new(run.worktree_path().unwrap()),
        run_dir.join("worktree")
    );
    // Every outcome keeps the worktree; only an accepted run closes its
    // session, a background wrapper's (task 1439).
    assert!(Path::new(run.worktree_path().unwrap()).exists());
    let session = background_session(run);
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    if run.status() == RunStatus::AwaitingIntegration && !close_fail {
        assert_eq!(backend.closed(), vec![session.clone()]);
        assert!(run.workspace_closed_at().is_some());
        assert!(kinds.contains(&"workspace_closed"));
    } else {
        assert!(backend.closed().is_empty());
        assert!(run.workspace_closed_at().is_none());
        assert!(!kinds.contains(&"workspace_closed"));
    }
    assert_eq!(kinds.contains(&"cleanup_failed"), close_fail);
    // A run at rest is reported through `watch`, not a notification
    // (ADR-0022); only the `approve_landing` ask of an accepted run whose
    // stand-in review printed no verdict notifies (task 328).
    assert_eq!(
        backend.notifications.lock().unwrap().len(),
        usize::from(run.status() == RunStatus::AwaitingIntegration),
        "{:?}",
        backend.notifications.lock().unwrap()
    );
    assert!(
        backend
            .notifications
            .lock()
            .unwrap()
            .iter()
            .all(|n| n.0.ends_with("approve_landing"))
    );
    // The run's wrapper ran in the background (task 1439): no workspace and
    // no group; it started in the run's worktree with its output in the
    // run dir, its environment carrying its role, actor id, run and task
    // (ADR-t728-1 decision 4), not the queue's path (its wrapper is named
    // the queue, and its worker the queue service: goal 82's stage (3)).
    // The handle's pid is the start the supervisor recorded and the
    // wrapper that registered and heartbeat under it.
    assert!(backend.groups.lock().unwrap().is_empty());
    let launched = backend.launched.lock().unwrap().clone();
    assert_eq!(launched.len(), 1, "{launched:?}");
    assert_eq!(
        launched[0].env,
        vec![
            ("DAGQ_ROLE".to_owned(), "worker".to_owned()),
            ("DAGQ_ACTOR_ID".to_owned(), format!("worker:{}", run.id())),
            ("DAGQ_RUN_ID".to_owned(), run.id().to_string()),
            ("DAGQ_TASK_ID".to_owned(), run.task_id().to_string()),
        ]
    );
    assert_eq!(launched[0].cwd, Path::new(run.worktree_path().unwrap()));
    assert_eq!(launched[0].log, run_dir.join("session.log"));
    let pid = dagq::domain::background_wrapper::BackgroundHandle::parse(&session)
        .unwrap()
        .pid;
    let launches = payloads(&detail, "wrapper_launched");
    assert_eq!(launches.len(), 1, "{launches:?}");
    assert_eq!(launches[0]["pid"], json!(pid));
    let (wrapper, heartbeat): (u32, i64) = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT pid, heartbeat_at FROM run_processes WHERE run_id=?1 AND role='wrapper'",
            [run.id()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(wrapper, pid);
    assert!(heartbeat > 0);
    // The run came to rest: its lease is gone, and the task still owns it.
    assert!(kinds.contains(&"lease_acquired"));
    assert!(kinds.contains(&"lease_released"));
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(queue.candidates().unwrap().is_empty());
    (dir, db, detail)
}

/// The prompt the run's agent was started with, as `provision` wrote it.
pub fn read_prompt(run: &TaskRun) -> String {
    fs::read_to_string(Path::new(run.run_dir().unwrap()).join("prompt.txt")).unwrap()
}

pub fn rejection_reason(detail: &dagq::domain::TaskDetail) -> String {
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Failed);
    let event = detail
        .events
        .iter()
        .find(|e| e.kind == "validation_finished")
        .unwrap();
    assert_eq!(event.payload["status"], "failed");
    assert_eq!(event.payload["accepted"], false);
    let reason = event.payload["reason"].as_str().unwrap().to_owned();
    assert_eq!(run.last_error(), Some(reason.as_str()));
    reason
}

pub fn event_kinds(detail: &dagq::domain::TaskDetail) -> Vec<&str> {
    detail.events.iter().map(|e| e.kind.as_str()).collect()
}

/// Blocks a fake turn until `$EXIT.held` exists, which no test writes: a
/// turn that does not end when its session is asked to exit, so its
/// wrapper outlives the exit request until its session is stopped (its
/// workspace closed, or the fixture ended).
pub const HOLD: &str = "await_file \"$EXIT.held\"";

/// Fake agent whose turn works (no receipt) until the test writes
/// `$EXIT.go`, then commits and writes its receipt like `VALID_AGENT`.
pub const GATED_AGENT: &str =
    "await_file \"$EXIT.go\"; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit";

/// The attention entries of `status` for one ask.
pub fn ask_attention(status: &Value, ask_id: AskId) -> Vec<Value> {
    status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["ask_id"] == ask_id.as_i64())
        .cloned()
        .collect()
}

/// The `backend_call_failed` events of a task, oldest first.
pub fn backend_failures(detail: &dagq::domain::TaskDetail) -> Vec<&dagq::domain::RunEvent> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == "backend_call_failed")
        .collect()
}

/// Whether a process with `pid` exists.
pub fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .bounded_status()
        .unwrap()
        .success()
}

/// A `sleep 60` with no stream of the test process: one a failing test
/// leaves behind does not hold the pipe of `cargo test | grep` open.
pub fn sleeper() -> Child {
    Command::new("sleep")
        .arg("60")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// A PID that certainly belonged to a process that has already exited.
pub fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// Register a run the way `supervise` does under `token`, with the given PIDs
/// as wrapper (started in the background) and agent, but with no supervisor
/// loop watching it. Returns the running run.
pub fn orphan_run(repo: &Path, db: &Path, token: &str, wrapper: u32, agent: u32) -> TaskRun {
    use dagq::{
        domain::ClaimOutcome,
        infrastructure::{
            adapters::{GitRepository, path_text},
            runtime_store::RunPlan,
        },
    };
    let repository = GitRepository::inspect(repo).unwrap();
    let mut queue = SqliteQueue::open(db).unwrap();
    queue
        .bind_repository(&path_text(&repository.common_dir).unwrap())
        .unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&repository.main_head().unwrap(), &LeaseToken::new(token))
        .unwrap()
    else {
        panic!()
    };
    let run_dir = dagq::infrastructure::location::runs_dir(db).join(run.id().as_str());
    fs::create_dir_all(&run_dir).unwrap();
    queue
        .plan_run(
            run.id(),
            &LeaseToken::new(token),
            &RunPlan {
                repo_path: path_text(&repository.root).unwrap(),
                run_dir: path_text(&run_dir).unwrap(),
                branch: format!("dagq/{}", run.id()),
                worktree_path: path_text(&run_dir.join("worktree")).unwrap(),
                receipt_path: path_text(&run_dir.join("receipt.json")).unwrap(),
                log_path: path_text(&run_dir.join("claude.debug.log")).unwrap(),
            },
        )
        .unwrap();
    let run = queue.run(run.id()).unwrap();
    repository.create_worktree(&run).unwrap();
    // Its session is a background wrapper's (task 1439): the handle names
    // the wrapper pid with its start, which a dead pid has none of.
    let start = SystemProcesses
        .start_identity(wrapper)
        .unwrap_or_else(|| "Thu Jan  1 00:00:00 1970".to_owned());
    let handle = dagq::domain::background_wrapper::BackgroundHandle::new(wrapper, &start);
    record_background_start(&mut queue, &run, token, &handle.to_string());
    queue
        .register_wrapper(run.id(), &LeaseToken::new(token), wrapper)
        .unwrap();
    queue.register_agent(run.id(), wrapper, agent).unwrap();
    let run = queue.run(run.id()).unwrap();
    assert_eq!(run.status(), RunStatus::Running);
    run
}

pub fn git_out(repo: &Path, args: &[&str]) -> String {
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

/// `dagq stats --full` of the queue at `db`, as the CLI prints it.
pub fn stats_full(db: &Path) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .arg("--db")
        .arg(db)
        .args(["stats", "--full", "--cmux", "/usr/bin/true"])
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// `integrate` as the CLI runs it without `--no-push`: pushing through the
/// real Git adapter, which finds no origin in the fixtures.
pub fn integrate(db: &Path, task_id: i64, repo: &Path) -> Result<Value> {
    let remote = GitRepository::inspect(repo).ok();
    runtime::integrate(
        db,
        IntegrateTarget::Task(TaskId::new(task_id)),
        repo,
        remote.as_ref().map(|r| r as &dyn MainRemote),
    )
}

pub fn integrate_next(db: &Path, repo: &Path) -> Value {
    let remote = GitRepository::inspect(repo).unwrap();
    runtime::integrate(db, IntegrateTarget::Next, repo, Some(&remote)).unwrap()
}

pub fn events_of(db: &Path, run_id: &RunId, kind: &str) -> Vec<Value> {
    SqliteQueue::open(db)
        .unwrap()
        .run_events(run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.payload)
        .collect()
}

/// Rewrite the run's receipt the way a resumed session would after its work.
pub fn write_receipt(run: &TaskRun, commit: &str, result: &str, summary: &str) {
    write_receipt_json(run, session_receipt(run, commit, result, summary));
}

/// A session's receipt for `commit`, with the evidence it would give.
pub fn session_receipt(run: &TaskRun, commit: &str, result: &str, summary: &str) -> Value {
    json!({
        "run_id": run.id(), "result": result, "commit": commit,
        "tests": {"status": "passed", "evidence_or_reason": "reran"},
        "e2e": {"status": "not_applicable", "evidence_or_reason": "none"},
        "subagent_review": {"status": "not_applicable", "evidence_or_reason": "session"},
        "summary": summary,
    })
}

pub fn write_receipt_json(run: &TaskRun, receipt: Value) {
    let path = Path::new(run.receipt_path().unwrap());
    fs::write(path.with_extension("tmp"), receipt.to_string()).unwrap();
    fs::rename(path.with_extension("tmp"), path).unwrap();
}

/// The payloads of the `verification_command` events the landing recorded
/// (`phase: integration`), oldest first. Validation runs no verification
/// command (ADR-0023 decision 1).
pub fn integration_verifications(detail: &dagq::domain::TaskDetail) -> Vec<&Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == "verification_command" && e.payload["phase"] == "integration")
        .map(|e| &e.payload)
        .collect()
}

/// The `integration_receipt` events of the task's run, oldest first.
pub fn integration_receipts(detail: &dagq::domain::TaskDetail) -> Vec<&Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == "integration_receipt")
        .map(|e| &e.payload)
        .collect()
}

/// Assert that `main` is linear, `commits` long on top of `seed`, and that
/// its head carries the landing of `run` with the same tree as `source`.
pub fn assert_landed(repo: &Path, run: &TaskRun, task_title: &str, expected_parent: &str) {
    let main = git_out(repo, &["rev-parse", "main"]);
    assert_eq!(run.status(), RunStatus::Integrated);
    assert_eq!(
        run.result_commit().map(CommitSha::as_str),
        Some(main.as_str())
    );
    assert_eq!(git_out(repo, &["rev-parse", "main^"]), expected_parent);
    assert_eq!(
        git_out(repo, &["rev-list", "--parents", "-1", "main"])
            .split(' ')
            .count(),
        2
    );
    let history = format!("refs/dagq/runs/{}", run.id());
    let source = git_out(repo, &["rev-parse", &history]);
    assert_eq!(
        git_out(repo, &["rev-parse", "main^{tree}"]),
        git_out(repo, &["rev-parse", &format!("{source}^{{tree}}")])
    );
    let message = git_out(repo, &["log", "-1", "--format=%B", "main"]);
    assert!(message.starts_with(task_title), "{message}");
    assert!(message.contains("\n\nDagq-Task: "), "{message}");
    assert!(
        message.ends_with(&format!("Dagq-Run: {}", run.id())),
        "{message}"
    );
    // Worktree and branch are gone; the run's history stays under the ref.
    assert!(!Path::new(run.worktree_path().unwrap()).exists());
    assert!(!git_out(repo, &["branch", "--list", run.branch().unwrap()]).contains("dagq/"));
}

/// Start the first exec of the `dagq` binary in this test process on a
/// thread of its own, once, and return at once (task 1416). On the
/// development host the first exec of the binary from each test process
/// that cargo started waits about 0.7 s unloaded for macOS's XProtect to
/// scan it, and any other exec of it meanwhile waits for the same scan;
/// nextest runs every test in a process of its own, so every test whose
/// stub session starts the queue service paid it there. Started as a
/// fixture begins, the scan overlaps the template copies and the
/// supervisor's claim and provisioning. `--version` reads no queue and
/// writes nothing, so what the tests read is the same.
pub fn warm_dagq() {
    static WARM: std::sync::Once = std::sync::Once::new();
    WARM.call_once(|| {
        thread::spawn(|| {
            let _ = Command::new(env!("CARGO_BIN_EXE_dagq"))
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        });
    });
}

/// A validated run plus a ready dependent task, before any landing.
pub fn awaiting_run() -> (Fixture, PathBuf, PathBuf, TaskRun) {
    awaiting_run_with_review_retry(false)
}

/// An awaiting run made with the production unreadable-review retry.
pub fn awaiting_run_retrying() -> (Fixture, PathBuf, PathBuf, TaskRun) {
    awaiting_run_with_review_retry(true)
}

fn awaiting_run_with_review_retry(retry: bool) -> (Fixture, PathBuf, PathBuf, TaskRun) {
    let (dir, db, detail) = run_agent_with_review_retry(VALID_AGENT, false, retry);
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let dependent = queue
        .add(NewTask {
            title: "dependent".into(),
            description: String::new(),
            acceptance: String::new(),
            verification_commands: vec![],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            dependencies: vec![TaskId::new(1)],
            goal_dependencies: Vec::new(),
            goal_id: None,
            context: String::new(),
            provider: None,
            worker_mode: Some(dagq::domain::worker::WorkerMode::Headless),
            wait_for_build: false,
        })
        .unwrap();
    queue
        .transition(dependent.id(), TaskAction::BypassReview)
        .unwrap();
    assert!(queue.candidates().unwrap().is_empty());
    let repo = dir.path().join("repo's directory");
    (dir, repo, db, run)
}

/// Two tasks rewrite `change.txt`: the first lands, and a person's
/// `integrate` of the second (its approval) conflicts and parks it as
/// `needs_session`. Returns the parked run and the landed main.
pub fn parked_conflict(repo: &Path, db: &Path, backend: &TestWorkspace) -> (TaskRun, String) {
    warm_dagq();
    let mut queue = SqliteQueue::open(db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    supervise(db, repo, backend).unwrap();
    backend.join();
    assert_eq!(integrate(db, 1, repo).unwrap()["outcome"], "integrated");
    let first_landed = git_out(repo, &["rev-parse", "main"]);
    let parked = integrate(db, 2, repo).unwrap();
    assert_eq!(parked["outcome"], "needs_session", "{parked}");
    let run = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::NeedsSession);
    (run, first_landed)
}

/// Make the conflict that parked the run count toward the resume attempts,
/// as a failed verification would (ADR-0047 decision 24: a resume of a run
/// parked only by a conflict after its landing was approved is not one of
/// them), for tests of what happens once the attempts are used up.
pub fn count_resumes_of_parked(db: &Path) {
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE run_events SET payload=json_set(payload,'$.code','verification_failed')
             WHERE kind='integration_deferred'",
            [],
        )
        .unwrap();
}

pub fn payloads<'a>(detail: &'a dagq::domain::TaskDetail, kind: &str) -> Vec<&'a Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == kind)
        .map(|e| &e.payload)
        .collect()
}

pub const IDLE_AGENT: &str = "commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit";

/// Provision and start a run under `token` exactly as `supervise` does
/// (claim, plan, prompt, worktree, the session's wrapper started in the
/// background by the test backend, its handle and start recorded: task
/// 1439), but with no loop or heartbeat behind the token: the state a
/// supervisor leaves when it is killed after the session started. Returns
/// the `running` run once the wrapper registered its agent.
pub fn start_run_under_dead_supervisor(
    repo: &Path,
    db: &Path,
    backend: &TestWorkspace,
    token: &str,
) -> TaskRun {
    let mut queue = SqliteQueue::open(db).unwrap();
    let run = provision_under(repo, db, token);
    let handle = backend
        .launch_background(
            Path::new(run.worktree_path().unwrap()),
            &shell_join(&[
                "runner".into(),
                "session".into(),
                "--run".into(),
                run.id().to_string(),
                "--lease".into(),
                token.into(),
                "--background".into(),
            ]),
            &[],
            &Path::new(run.run_dir().unwrap()).join("session.log"),
        )
        .unwrap();
    record_background_start(&mut queue, &run, token, &handle);
    wait_until(db, Duration::from_secs(10), |queue| {
        queue.run(run.id()).unwrap().status() == RunStatus::Running
    });
    queue.run(run.id()).unwrap()
}

/// Record the background wrapper `handle` as the session of `run` under
/// `token`, and its start, as the supervisor records them
/// (`workspace_created`, then `wrapper_launched`): the wrapper registers
/// once it finds its start.
pub fn record_background_start(queue: &mut SqliteQueue, run: &TaskRun, token: &str, handle: &str) {
    let parsed = dagq::domain::background_wrapper::BackgroundHandle::parse(handle).unwrap();
    queue
        .workspace_created(run.id(), &LeaseToken::new(token), handle)
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            dagq::domain::EventKind::WrapperLaunched,
            json!({
                "pid": parsed.pid,
                "start": parsed.start,
                "workspace_id": handle,
                "log": Path::new(run.run_dir().unwrap()).join("session.log").to_string_lossy(),
            }),
        )
        .unwrap();
}

/// Claim the next task under `token` and provision its run as `supervise`
/// does but for its session: plan, run directory, prompt and worktree.
pub fn provision_under(repo: &Path, db: &Path, token: &str) -> TaskRun {
    use dagq::{
        domain::ClaimOutcome,
        infrastructure::{
            adapters::{GitRepository, path_text},
            location::runs_dir,
            runtime_store::RunPlan,
        },
    };
    let repository = GitRepository::inspect(repo).unwrap();
    let mut queue = SqliteQueue::open(db).unwrap();
    queue
        .bind_repository(&path_text(&repository.common_dir).unwrap())
        .unwrap();
    // Any worker, as a supervisor with every adapter claims (a headless
    // task's too).
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor_in_order(
            &repository.main_head().unwrap(),
            &LeaseToken::new(token),
            &[],
            None,
            &Default::default(),
            &dagq::domain::provider_switch::WorkerRoute::direct(&dagq::domain::worker::Worker::ALL),
        )
        .unwrap()
    else {
        panic!("no candidate to claim")
    };
    let run_dir = runs_dir(&db.canonicalize().unwrap()).join(run.id().as_str());
    queue
        .plan_run(
            run.id(),
            &LeaseToken::new(token),
            &RunPlan {
                repo_path: path_text(&repository.root).unwrap(),
                run_dir: path_text(&run_dir).unwrap(),
                branch: format!("dagq/{}", run.id()),
                worktree_path: path_text(&run_dir.join("worktree")).unwrap(),
                receipt_path: path_text(&run_dir.join("receipt.json")).unwrap(),
                log_path: path_text(&run_dir.join("claude.debug.log")).unwrap(),
            },
        )
        .unwrap();
    fs::create_dir_all(&run_dir).unwrap();
    let run = queue.run(run.id()).unwrap();
    let task = queue.show(run.task_id()).unwrap().task;
    fs::write(
        run_dir.join("prompt.txt"),
        runtime::prompt(&task, &run, None, &[], &[], &[], None, &[]).unwrap(),
    )
    .unwrap();
    repository.create_worktree(&run).unwrap();
    run
}

/// Age the lease of `run` so it is stale by heartbeat while its pid (this
/// test process) is alive, like a supervisor that stopped heartbeating.
pub fn age_lease(db: &Path, run: &TaskRun, seconds: i64) {
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET heartbeat_at=unixepoch()-?2 WHERE run_id=?1",
            rusqlite::params![run.id(), seconds],
        )
        .unwrap();
}

pub fn adoption_events(detail: &dagq::domain::TaskDetail) -> Vec<&Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind == "run_adopted")
        .map(|e| &e.payload)
        .collect()
}

pub fn supervisor_token_of(db: &Path, run: &TaskRun) -> String {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT supervisor_token FROM task_runs WHERE id=?1",
            [&run.id()],
            |r| r.get(0),
        )
        .unwrap()
}

/// The run's own attention; an ask about the run (with `ask_id`) is not.
pub fn run_attention_of<'a>(status: &'a Value, run_id: &RunId) -> Option<&'a Value> {
    status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["run_id"] == run_id.as_str() && a["ask_id"].is_null())
}

pub fn supervise_reviewed(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    reviewer: &TestReviewer,
) -> Value {
    supervise_reviewed_with(db, repo, backend, reviewer, &supervise_options(4, true))
}

/// [`supervise_reviewed`] with `options`.
pub fn supervise_reviewed_with(
    db: &Path,
    repo: &Path,
    backend: &TestWorkspace,
    reviewer: &TestReviewer,
    options: &SuperviseOptions,
) -> Value {
    let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
    let _diagnostics = supervise_diagnostics(db);
    let outcome = runtime::supervise_with_reviewer(
        db,
        repo,
        backend,
        &claude_stub(db),
        reviewer,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        options,
    )
    .unwrap();
    backend.join();
    outcome
}

/// The position of the first event of `kind`, which must exist.
pub fn position(kinds: &[&str], kind: &str) -> usize {
    kinds
        .iter()
        .position(|k| *k == kind)
        .unwrap_or_else(|| panic!("no {kind} in {kinds:?}"))
}

pub fn assert_landed_run(run: &TaskRun, repo: &Path, base: &str) {
    assert_landed(repo, run, "test task", base);
}

/// What a live session's recovery prompt says, for the stand-ins whose
/// default job for it escalates.
pub const LIVE_RECOVERY: &str = "is still running. The supervisor raised";

/// The default verdict of a live session's recovery job in these tests.
pub const ESCALATE: &str = r#"{"verdict": "escalate", "confidence": "high", "diagnosis": "the test provider does not repair"}"#;

/// A recovery job's script that prints `verdict` (no apostrophes in the
/// texts: the script quotes the JSON with them).
pub fn recovery(verdict: Value) -> String {
    format!("printf '%s\\n' '{verdict}'")
}

/// A recovery job's script whose verdict is a `repair` of high confidence
/// with the one `action`.
pub fn repair(action: Value, diagnosis: &str) -> String {
    recovery(json!({
        "verdict": "repair",
        "confidence": "high",
        "diagnosis": diagnosis,
        "actions": [action],
    }))
}

pub fn stalled_asks(queue: &SqliteQueue) -> Vec<dagq::domain::Ask> {
    queue
        .asks(AskQuery {
            all: true,
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .filter(|ask| ask.kind == AskKind::Stalled)
        .collect()
}

/// Open the queue's hold ask of `reason` (and `subject`) with no run in
/// it, as the runtime does when Claude's login or its usage limit stops
/// the work: Claude is held (ADR-0047 decision 42, ADR-t813-2).
pub fn open_hold_ask(
    db: &Path,
    reason: dagq::domain::AskReason,
    subject: Option<&str>,
) -> dagq::domain::Ask {
    SqliteQueue::open(db)
        .unwrap()
        .hold(dagq::domain::NewHold {
            reason_category: reason,
            subject: subject.map(str::to_owned),
            run_id: None,
            job: None,
            question: "the login ran out".into(),
            options: dagq::domain::HOLD_OPTIONS
                .iter()
                .map(|o| (*o).to_owned())
                .collect(),
            asked_by: "supervisor".into(),
        })
        .unwrap()
        .ask
}
