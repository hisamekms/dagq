//! The fixture of the runtime tests (`tests/runtime_*.rs`): the stub agent
//! and workspace, the supervisor options and the helpers several of them use.
#![allow(dead_code, unused_imports)]

use crate::common;
pub mod headless;
pub use crate::common::{Bounded, WithoutActor};
pub use anyhow::{Result, bail, ensure};
use dagq::domain::LeaseToken;
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
    let result = Command::new("git")
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
/// timed while it is held (task 324).
pub struct Fixture {
    pub db: PathBuf,
    pub dir: TempDir,
    pub _test: common::Waiting,
}

impl Fixture {
    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        kill_stubs(&self.db);
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
        command.stdin(Stdio::null());
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
    (
        Fixture {
            db: db.clone(),
            dir,
            _test: common::test(),
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
            worker_mode: None,
        })
        .unwrap();
    queue
        .transition(task.id(), TaskAction::BypassReview)
        .unwrap();
    task.id()
}

/// Shell prelude for the fake agent: `receipt COMMIT [RUN_ID]` writes an
/// atomically renamed receipt claiming success with evidence on every check,
/// `idle` mimics Claude's Stop hook (`idle_bg` with background work still
/// running, `idle_bg_done` once it ended, as Claude Code 2.1.281 writes
/// `background_tasks`), and `await_exit` blocks until the test
/// workspace delivers the supervisor's exit request.
pub const AGENT_PRELUDE: &str = concat!(
    watchdog!(),
    r#"
test -f seed.txt || exit 99
printf 'fixture log\n' > "$LOG"
receipt() {
  printf '{"run_id":"%s","result":"succeeded","commit":"%s","tests":{"status":"passed","evidence_or_reason":"ran"},"e2e":{"status":"not_applicable","evidence_or_reason":"no e2e surface"},"subagent_review":{"status":"passed","evidence_or_reason":"reviewed"},"summary":"done"}' "${2:-$RUN_ID}" "$1" > "$RECEIPT.tmp"
  mv "$RECEIPT.tmp" "$RECEIPT"
}
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
await_exit() { while [ ! -f "$EXIT" ]; do sleep 0.05; done; }
commit() { printf 'change by %s\n' "$RUN_ID" > change.txt && git add change.txt && git commit -q -m "$1"; }
"#
);

pub const VALID_AGENT: &str = "commit work; receipt \"$(git rev-parse HEAD)\"";

/// The first workspace the test backend hands out; see `workspace_id`.
pub const WORKSPACE_ID: &str = "01234567-89ab-4def-8123-000000000000";

pub fn workspace_id(n: usize) -> String {
    format!("01234567-89ab-4def-8123-{n:012x}")
}

/// Shell prelude for a resumed session: `await_message` blocks until the
/// supervisor's resolution request arrived (the test backend writes it to
/// `$MESSAGE`) and sets `$MAIN` to the main it names; `receipt` / `idle` /
/// `await_exit` are the worker's.
pub const RESUME_PRELUDE: &str = concat!(
    watchdog!(),
    r#"
receipt() {
  printf '{"run_id":"%s","result":"%s","commit":"%s","tests":{"status":"passed","evidence_or_reason":"reran after the rebase"},"e2e":{"status":"not_applicable","evidence_or_reason":"no e2e surface"},"subagent_review":{"status":"not_applicable","evidence_or_reason":"resumed session"},"summary":"%s"}' "$RUN_ID" "${2:-succeeded}" "$1" "${3:-resolved}" > "$RECEIPT.tmp"
  mv "$RECEIPT.tmp" "$RECEIPT"
}
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
await_exit() { while [ ! -f "$EXIT" ]; do sleep 0.05; done; }
await_message() {
  while [ ! -f "$MESSAGE" ]; do sleep 0.05; done
  MAIN=$(sed -n 's/.*main is now \([0-9a-f]*\) .*/\1/p' "$MESSAGE" | head -n 1)
}
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
);

pub struct TestProvider {
    pub script: String,
    /// The queue, for a script that runs `$DAGQ --db "$DB" ...` as a worker would.
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
    /// Kept in [`session_models`] rather than passed on: `/bin/sh` takes
    /// no model.
    fn select_model(&self, command: &mut CommandSpec, model: &str, effort: &str) {
        use std::io::Write as _;
        let run_id = command
            .get_envs()
            .find(|(key, _)| *key == "RUN_ID")
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned());
        let resume = command
            .get_args()
            .any(|arg| arg.to_string_lossy().starts_with(RESUME_PRELUDE));
        let line = json!({"run_id": run_id, "resume": resume, "model": model, "effort": effort});
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(session_models_path(&self.db))
            .unwrap();
        writeln!(file, "{line}").unwrap();
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
        // Background work is stopped before the receipt, or /exit stalls.
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

fn session_models_path(db: &Path) -> PathBuf {
    db.with_file_name("session-models.jsonl")
}

/// The model and effort each session of [`TestProvider`] was started with
/// (ADR-0079 decision 3), in the order they started: `run_id`, `resume`,
/// `model`, `effort`.
pub fn session_models(db: &Path) -> Vec<Value> {
    fs::read_to_string(session_models_path(db))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Claude Code's empty input box, the screen `capture` returns unless a
/// test sets another: the session takes input, and what the supervisor
/// typed left the box.
pub const READY_SCREEN: &str = "\
⏺ Done.

──────────────────────────────────────────────────────────────────────
❯ 
──────────────────────────────────────────────────────────────────────
  ? for shortcuts
";

/// [`READY_SCREEN`] once the session took a text: at work on it.
pub const WORKING_SCREEN: &str = "\
⏺ Done.

✻ Working… (3s · esc to interrupt)

──────────────────────────────────────────────────────────────────────
❯ 
──────────────────────────────────────────────────────────────────────
  ? for shortcuts
";

/// Claude Code's input box still holding `text` after its Enter.
pub fn pending_screen(text: &str) -> String {
    format!(
        "⏺ Done.\n\n{rule}\n❯ {}\n{rule}\n  ? for shortcuts\n",
        dagq::infrastructure::adapters::single_line(text),
        rule = "─".repeat(70)
    )
}

/// The runner's shell line before Claude Code draws its input box.
pub const BOOT_SCREEN: &str =
    "worktree on dagq/run\n❯ '/run/runner' '--db' '/queue.db' 'session' '--resume'\n";

/// The test backend delivers an exit request as a file the fake agent polls for.
pub fn exit_request_path(run_dir: &str) -> PathBuf {
    Path::new(run_dir).join("exit-requested")
}

/// ... and the text typed into a resumed session the same way.
pub fn resume_message_path(run_dir: &str) -> PathBuf {
    Path::new(run_dir).join("resume-message")
}

/// One session the test backend started, keyed by its workspace id.
pub struct TestSession {
    pub run_id: RunId,
    pub run_dir: String,
    pub worker: Option<thread::JoinHandle<Result<Value>>>,
}

/// Starts the session wrapper on a thread per workspace, with the agent
/// script chosen per task, and records exits and closes per workspace.
pub struct TestWorkspace {
    pub db: PathBuf,
    pub fail: bool,
    pub close_fail: bool,
    pub script: String,
    pub scripts: Mutex<HashMap<TaskId, String>>,
    pub exit_timeout: Duration,
    pub registration_timeout: Duration,
    /// `create` opens the workspace but starts no session, so its wrapper
    /// never registers.
    pub no_session: bool,
    pub resume_timeout: Duration,
    /// `send_exit` returns only after the wrapper recorded its exit, as a
    /// slow `cmux send` does when the session exits on the first keystroke.
    pub exit_returns_after_session: bool,
    pub prompt_wait: Duration,
    /// What `capture` returns, and how often it was asked.
    pub screen: Mutex<String>,
    pub captures: AtomicUsize,
    /// `send_enter` calls: Enters sent again after a submit (task 285).
    pub enters: AtomicUsize,
    /// This many Enters leave a typed text in the input box (the screen
    /// shows it there until the last one), as a long paste does.
    pub swallowed_enters: AtomicUsize,
    /// This many texts are typed but never reach the session, as one typed
    /// before Claude Code's input box is drawn.
    pub dropped_texts: AtomicUsize,
    pub exits_sent: AtomicUsize,
    pub sessions: Mutex<Vec<(String, TestSession)>>,
    pub closed: Mutex<Vec<String>>,
    /// `notify` calls as (title, body, workspace); the supervisor sends
    /// none, `ask` one per new ask (ADR-0022).
    pub notifications: Mutex<Vec<(String, String, Option<String>)>>,
    /// The tags each run workspace was opened with.
    pub tags: Mutex<Vec<WorkspaceTags>>,
    /// Every `ensure_group` call, as (external ID, name).
    pub groups: Mutex<Vec<(String, String)>>,
    /// `workspace-group create` fails.
    pub group_fails: bool,
    /// `send_exit` delivers the request but reports a timeout, the way
    /// `cmux send` does when cmux answers too late under load.
    pub send_times_out: bool,
    /// Resumed-session script per task; a resume of any other task fails.
    pub resume_scripts: Mutex<HashMap<TaskId, String>>,
    /// `create_resume` calls: the workspace name and the command.
    pub resumes: Mutex<Vec<(String, String)>>,
    /// The tags of every `create_resume` call.
    pub resume_tags: Mutex<Vec<WorkspaceTags>>,
    /// `send_text` calls: the workspace and the text.
    pub texts: Mutex<Vec<(String, String)>>,
    /// The inputs that switched a live session's model or effort
    /// (`/model`, `/effort`; ADR-0079 decision 5): the workspace and the
    /// input. The session takes them as the agent does, without working.
    pub switches: Mutex<Vec<(String, String)>>,
    /// A switch input fails to be typed, the other texts going on.
    pub switch_fails: bool,
    /// A switch input stays in the input box, whatever Enter is sent.
    pub switch_stuck: bool,
    /// `send_text` records the call and then fails, as a `cmux send` to a
    /// workspace that went away does.
    pub text_fails: bool,
    /// A `send_text` of an answer to an ask returns only in a later second
    /// than the session's turn on it ended: once the session wrote
    /// `answered` next to its receipt, and past the next second, as a
    /// `cmux send` that is slow under load does (task 971).
    pub answer_send_outlasts_turn: bool,
    /// `exists` fails, as `cmux workspace list` does when cmux is gone.
    pub exists_fails: bool,
    /// Workspaces cmux lists although this backend did not open them (a
    /// run's workspace from an earlier supervisor), until they are closed.
    pub listed: Mutex<Vec<String>>,
    /// Workspaces cmux does not list for now although they are open.
    pub hidden: Mutex<Vec<String>>,
    /// This many captures time out, as `cmux read-screen` does under load.
    pub capture_timeouts: AtomicUsize,
    /// This many `send_exit` calls time out before the `/exit` reaches the
    /// session (task 354).
    pub exit_unsent: AtomicUsize,
    /// `close` ends the session in the workspace, as closing a cmux
    /// workspace kills its terminal, instead of requiring it gone.
    pub close_ends_session: bool,
    /// `close` times out and leaves the workspace and its session as they
    /// are, as cmux does under load.
    pub close_times_out: bool,
    /// `send_key` calls: the keys a known dialog was answered with. Enter on
    /// the "Background work is running" screen lets a held session exit,
    /// and Escape closes the Settings panel.
    pub keys: Mutex<Vec<String>>,
    /// The screen the next `send_text` leaves, once: a dialog that came up
    /// over the text the supervisor typed (task 480).
    pub screen_after_text: Mutex<Option<String>>,
    /// The screen the first capture after the next `send_text` leaves, once:
    /// that capture (the submit's confirmation) still shows the text taken,
    /// and a later one (the send's start check) shows this.
    pub screen_after_confirm: Mutex<Option<String>>,
    /// [`Self::screen_after_confirm`] armed by a `send_text` or, from
    /// [`Self::screen_after_exit`], by a delivered `send_exit`.
    armed_screen: Mutex<Option<String>>,
    /// The screen the first capture after the next delivered `send_exit`
    /// leaves, once: that capture (the submit's confirmation) still shows
    /// the screen the `/exit` reached, and every later one (an exit retry's)
    /// shows this, a dialog that came up after it, whatever the load.
    pub screen_after_exit: Mutex<Option<String>>,
    /// The `claude` a headless run's wrapper calls for its turns
    /// ([`headless_claude`]); a headless run fails its wrapper without one.
    pub headless: Option<PathBuf>,
    /// Wait for this per-turn marker before the wrapper starts its timers.
    pub headless_ready: Option<PathBuf>,
    /// The `codex` a headless Codex run's wrapper calls for its turns
    /// ([`headless_codex`]).
    pub codex: Option<PathBuf>,
}

impl TestWorkspace {
    pub fn new(db: &Path, fail: bool, script: &str) -> Self {
        Self {
            db: db.into(),
            fail,
            close_fail: false,
            script: script.into(),
            scripts: Mutex::new(HashMap::new()),
            exit_timeout: Duration::from_secs(120),
            registration_timeout: Duration::from_secs(45),
            no_session: false,
            resume_timeout: Duration::from_secs(120),
            exit_returns_after_session: false,
            prompt_wait: Duration::from_secs(90),
            screen: Mutex::new(READY_SCREEN.into()),
            captures: AtomicUsize::new(0),
            enters: AtomicUsize::new(0),
            swallowed_enters: AtomicUsize::new(0),
            dropped_texts: AtomicUsize::new(0),
            exits_sent: AtomicUsize::new(0),
            sessions: Mutex::new(Vec::new()),
            closed: Mutex::new(Vec::new()),
            notifications: Mutex::new(Vec::new()),
            tags: Mutex::new(Vec::new()),
            groups: Mutex::new(Vec::new()),
            group_fails: false,
            send_times_out: false,
            answer_send_outlasts_turn: false,
            resume_scripts: Mutex::new(HashMap::new()),
            resumes: Mutex::new(Vec::new()),
            resume_tags: Mutex::new(Vec::new()),
            texts: Mutex::new(Vec::new()),
            switches: Mutex::new(Vec::new()),
            switch_fails: false,
            switch_stuck: false,
            text_fails: false,
            exists_fails: false,
            listed: Mutex::new(Vec::new()),
            hidden: Mutex::new(Vec::new()),
            capture_timeouts: AtomicUsize::new(0),
            exit_unsent: AtomicUsize::new(0),
            close_ends_session: false,
            close_times_out: false,
            keys: Mutex::new(Vec::new()),
            screen_after_text: Mutex::new(None),
            screen_after_confirm: Mutex::new(None),
            armed_screen: Mutex::new(None),
            screen_after_exit: Mutex::new(None),
            headless: None,
            headless_ready: None,
            codex: None,
        }
    }
    /// Let cmux list `workspace` as if an earlier supervisor opened it.
    pub fn list(&self, workspace: &str) {
        self.listed.lock().unwrap().push(workspace.into());
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
    pub fn switches(&self) -> Vec<(String, String)> {
        self.switches.lock().unwrap().clone()
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
    pub fn join(&self) {
        let workers: Vec<_> = self
            .sessions
            .lock()
            .unwrap()
            .iter_mut()
            .filter_map(|(id, s)| s.worker.take().map(|worker| (id.clone(), worker)))
            .collect();
        for (id, worker) in workers {
            joined(
                worker,
                format!("the session wrapper of workspace {id} to return (its stub agent to exit)"),
            )
            .unwrap();
        }
    }
    pub fn session_run_dir(&self, workspace_id: &str) -> String {
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _)| id == workspace_id)
            .map(|(_, s)| s.run_dir.clone())
            .expect("workspace was created")
    }
}

impl WorkspaceBackend for TestWorkspace {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn preflight_detached(&self, _: &SupervisorEnvironment) -> Result<()> {
        unreachable!("only up preflights the detached connection")
    }
    fn create(
        &self,
        task: &Task,
        run: &TaskRun,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        assert_eq!(task.id(), run.task_id());
        self.tags.lock().unwrap().push(tags.clone());
        assert!(
            Path::new(run.worktree_path().unwrap())
                .join("seed.txt")
                .exists()
        );
        assert!(command.contains("'\"'\"'")); // Database path contains an apostrophe.
        if self.fail {
            bail!("injected workspace creation failure");
        }
        let token: String = Connection::open(&self.db)?.query_row(
            "SELECT token FROM run_leases WHERE run_id=?1",
            [&run.id()],
            |r| r.get(0),
        )?;
        let db = self.db.clone();
        let id = run.id().clone();
        let script = self
            .scripts
            .lock()
            .unwrap()
            .get(&run.task_id())
            .cloned()
            .unwrap_or_else(|| self.script.clone());
        let mut sessions = self.sessions.lock().unwrap();
        let workspace = workspace_id(sessions.len());
        if self.no_session {
            sessions.push((
                workspace.clone(),
                TestSession {
                    run_id: run.id().clone(),
                    run_dir: run.run_dir().unwrap().to_owned(),
                    worker: None,
                },
            ));
            return Ok(workspace);
        }
        let headless = headless_provider(run, self.headless.as_deref(), self.codex.as_deref());
        let ready = self.headless_ready.clone();
        let worker = thread::spawn(move || {
            let spawner = StubSpawner { db: db.clone() };
            if let Some((provider, other)) = headless {
                let spawner = headless::ReadySpawner {
                    inner: spawner,
                    ready,
                };
                return runtime::session_with_providers(
                    &db,
                    &id,
                    &LeaseToken::new(&token),
                    &provider,
                    Some(&other),
                    &spawner,
                    false,
                );
            }
            let provider = TestProvider {
                script,
                db: db.clone(),
            };
            runtime::session_with_provider(&db, &id, &LeaseToken::new(&token), &provider, &spawner)
        });
        sessions.push((
            workspace.clone(),
            TestSession {
                run_id: run.id().clone(),
                run_dir: run.run_dir().unwrap().to_owned(),
                worker: Some(worker),
            },
        ));
        Ok(workspace)
    }
    fn create_resume(
        &self,
        task: &Task,
        run: &TaskRun,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        assert_eq!(task.id(), run.task_id());
        assert!(command.ends_with(" '--resume'"), "{command}");
        // The worker's env (ADR-0026) and `run <run-id> resume` (ADR-0028).
        assert!(
            tags.env
                .iter()
                .any(|(k, v)| k == "DAGQ_ROLE" && v == "worker"),
            "{:?}",
            tags.env
        );
        assert!(tags.env.iter().any(|(k, _)| k == "DAGQ_QUEUE"));
        // The same actor as the run's worker (ADR-t728-1 decision 4).
        for (name, value) in [
            ("DAGQ_ACTOR_ID", format!("worker:{}", run.id())),
            ("DAGQ_RUN_ID", run.id().to_string()),
            ("DAGQ_TASK_ID", run.task_id().to_string()),
        ] {
            assert!(
                tags.env.iter().any(|(k, v)| k == name && *v == value),
                "{name}={value} in {:?}",
                tags.env
            );
        }
        assert_eq!(
            tags.description.as_deref(),
            Some(format!("run {} resume", run.id()).as_str())
        );
        let headless = headless_provider(run, self.headless.as_deref(), self.codex.as_deref());
        let script = match &headless {
            Some(_) => String::new(),
            None => self
                .resume_scripts
                .lock()
                .unwrap()
                .get(&run.task_id())
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no resume script for task {}", run.task_id()))?,
        };
        let token: String = Connection::open(&self.db)?.query_row(
            "SELECT token FROM run_leases WHERE run_id=?1",
            [&run.id()],
            |r| r.get(0),
        )?;
        let run_dir = run.run_dir().unwrap().to_owned();
        // The worker's session left its exit request and any earlier
        // resume its message behind.
        let _ = fs::remove_file(exit_request_path(&run_dir));
        let _ = fs::remove_file(resume_message_path(&run_dir));
        self.resumes.lock().unwrap().push((
            dagq::infrastructure::adapters::run_workspace_name(task, run)?,
            command.into(),
        ));
        self.resume_tags.lock().unwrap().push(tags.clone());
        let db = self.db.clone();
        let id = run.id().clone();
        let mut sessions = self.sessions.lock().unwrap();
        let workspace = workspace_id(sessions.len());
        let ready = self.headless_ready.clone();
        let worker = thread::spawn(move || {
            let spawner = StubSpawner { db: db.clone() };
            if let Some((provider, other)) = headless {
                let spawner = headless::ReadySpawner {
                    inner: spawner,
                    ready,
                };
                return runtime::session_with_providers(
                    &db,
                    &id,
                    &LeaseToken::new(&token),
                    &provider,
                    Some(&other),
                    &spawner,
                    true,
                );
            }
            let provider = TestProvider {
                script,
                db: db.clone(),
            };
            runtime::resume_session_with_provider(
                &db,
                &id,
                &LeaseToken::new(&token),
                &provider,
                &spawner,
            )
        });
        sessions.push((
            workspace.clone(),
            TestSession {
                run_id: run.id().clone(),
                run_dir,
                worker: Some(worker),
            },
        ));
        Ok(workspace)
    }
    fn send_text(&self, workspace_id: &str, text: &str) -> Result<()> {
        if text.starts_with("/model ") || text.starts_with("/effort ") {
            self.switches
                .lock()
                .unwrap()
                .push((workspace_id.into(), text.into()));
            if self.switch_fails || self.text_fails {
                bail!("injected cmux send failure");
            }
            if self.switch_stuck {
                *self.screen.lock().unwrap() = pending_screen(text);
            }
            return Ok(());
        }
        self.texts
            .lock()
            .unwrap()
            .push((workspace_id.into(), text.into()));
        if self.text_fails {
            bail!("injected cmux send failure");
        }
        if self.swallowed_enters.load(Ordering::SeqCst) > 0 {
            *self.screen.lock().unwrap() = pending_screen(text);
        }
        if self
            .dropped_texts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Ok(());
        }
        // The session got it and works on it.
        let mut screen = self.screen.lock().unwrap();
        if *screen == READY_SCREEN {
            *screen = WORKING_SCREEN.into();
        }
        if let Some(after) = self.screen_after_text.lock().unwrap().take() {
            *screen = after;
        }
        if let Some(after) = self.screen_after_confirm.lock().unwrap().take() {
            *self.armed_screen.lock().unwrap() = Some(after);
        }
        drop(screen);
        let run_dir = self.session_run_dir(workspace_id);
        let path = resume_message_path(&run_dir);
        fs::write(path.with_extension("tmp"), text)?;
        fs::rename(path.with_extension("tmp"), path)?;
        if self.answer_send_outlasts_turn && text.starts_with("answer to ask") {
            let answered = Path::new(&run_dir).join("answered");
            let deadline = Instant::now() + Duration::from_secs(30);
            while !answered.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            let second = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            while SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() <= second {
                thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(())
    }
    fn send_enter(&self, _: &str) -> Result<()> {
        self.enters.fetch_add(1, Ordering::SeqCst);
        if self
            .swallowed_enters
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            == Ok(1)
        {
            *self.screen.lock().unwrap() = READY_SCREEN.into();
        }
        Ok(())
    }
    fn send_key(&self, workspace_id: &str, key: &str) -> Result<()> {
        self.keys.lock().unwrap().push(key.into());
        let mut screen = self.screen.lock().unwrap();
        match key {
            "enter" if screen.contains("Background work is running") => {
                *screen = READY_SCREEN.into();
                release_held_session(&self.session_run_dir(workspace_id));
            }
            "escape" if screen.contains("Settings:") => *screen = WORK_SCREEN.into(),
            _ => (),
        }
        Ok(())
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
        if self
            .capture_timeouts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            bail!("cmux read-screen failed: Error: Command timed out");
        }
        let mut screen = self.screen.lock().unwrap();
        let read = screen.clone();
        if let Some(after) = self.armed_screen.lock().unwrap().take() {
            *screen = after;
        }
        Ok(read)
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
        // before the supervisor gives up the workspace. A workspace this
        // backend did not create (an orphan's) has no session here.
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
                    "session did not end with its workspace"
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
        if let Some(run_id) = &run_id {
            let (exited, pid): (bool, u32) = connection.query_row(
                "SELECT exited_at IS NOT NULL, pid FROM run_processes WHERE run_id=?1 AND role='wrapper'",
                [run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert!(exited || !pid_alive(pid));
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
            bail!("injected workspace close failure");
        }
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
        self.exits_sent.fetch_add(1, Ordering::SeqCst);
        if self
            .exit_unsent
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            bail!("\"cmux\" send did not finish within 30s");
        }
        let run_dir = self.session_run_dir(workspace_id);
        fs::write(exit_request_path(&run_dir), "")?;
        if let Some(after) = self.screen_after_exit.lock().unwrap().take() {
            *self.armed_screen.lock().unwrap() = Some(after);
        }
        if self.send_times_out {
            // The /exit got there: the transcript shows it.
            *self.screen.lock().unwrap() = format!("❯ /exit\n{READY_SCREEN}");
            bail!("\"cmux\" send did not finish within 30s");
        }
        if self.exit_returns_after_session {
            let run_id = self
                .sessions
                .lock()
                .unwrap()
                .iter()
                .find(|(id, _)| id == workspace_id)
                .map(|(_, s)| s.run_id.clone())
                .expect("workspace was created");
            let connection = Connection::open(&self.db)?;
            let started = Instant::now();
            while !connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM run_processes WHERE run_id=?1 AND role='wrapper' AND exited_at IS NOT NULL)",
                [&run_id],
                |r| r.get::<_, bool>(0),
            )? {
                ensure!(
                    started.elapsed() < Duration::from_secs(30),
                    "session did not exit"
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(())
    }
    fn exit_timeout(&self) -> Duration {
        self.exit_timeout
    }
    fn registration_timeout(&self) -> Duration {
        self.registration_timeout
    }
    fn prompt_wait(&self) -> Duration {
        self.prompt_wait
    }
    // A workspace is listed from its creation until it is closed, as cmux
    // does; one this backend never opened is not.
    fn exists(&self, workspace_id: &str) -> Result<bool> {
        ensure!(!self.exists_fails, "injected workspace list failure");
        Ok(self
            .listed_workspace_ids()?
            .iter()
            .any(|listed| listed == workspace_id))
    }
    fn listed_workspace_ids(&self) -> Result<Vec<String>> {
        ensure!(!self.exists_fails, "injected workspace list failure");
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
        if self.group_fails {
            bail!("workspace-group create failed")
        }
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
/// supervisor moves it there (ADR-t813-2). `None` for an interactive run.
fn headless_provider(
    run: &TaskRun,
    claude: Option<&Path>,
    codex: Option<&Path>,
) -> Option<(HeadlessProvider, HeadlessProvider)> {
    (run.worker_mode() == dagq::domain::worker::WorkerMode::Headless).then(|| {
        let provider = run.actual_provider();
        (
            headless_of(provider, claude, codex),
            headless_of(provider.other(), claude, codex),
        )
    })
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
        run: &TaskRun,
        prompt: &str,
        session: dagq::domain::turn::TurnSession<'_>,
    ) -> Result<CommandSpec> {
        self.agent.turn_command(run, prompt, session)
    }
    fn turn_reader(&self) -> Result<Box<dyn dagq::application::TurnReader>> {
        self.agent.turn_reader()
    }
    fn turn_permission_mode(&self) -> Option<&'static str> {
        self.agent.turn_permission_mode()
    }
    fn turn_session_from_output(&self) -> bool {
        self.agent.turn_session_from_output()
    }
}

/// A stub `claude` for headless turns (ADR-t813-1), in `dir`: it takes
/// `claude -p`'s arguments, appends `<start|resume> <session> <prompt's
/// first line>` to `stub-calls.log` in the run directory (`--add-dir`),
/// prints `system/init` in stream-json with the permission mode it was
/// given (or `$PERMISSION_SAID`), then sources `turn.sh` next to it (see
/// [`set_turns`]) and prints a result unless the turn did. `$TURN` is the
/// turn's number in the run, `$PROMPT` its prompt, `$MODE` `start` or
/// `resume`, `$SESSION` the session id. The turn's helpers: `say TEXT`,
/// `result [TEXT]` (with `$DENIALS` as its `permission_denials` and
/// `$COST`, 0.01 unless the turn sets it, as its `total_cost_usd`),
/// `denied` (three refusals), `fail TEXT` (an error result, exit 1),
/// `commit MESSAGE`, `receipt COMMIT [RESULT] [EVIDENCE]`, `ask QUESTION`
/// (its notification goes to `true`, not the host's cmux: a real `cmux
/// notify` that is slow under load would be a quiet process of the turn
/// for the `idle_process` watch).
pub fn headless_claude(dir: &Path, db: &Path) -> PathBuf {
    let stub = dir.join("claude-headless");
    let script = format!(
        r#"#!/bin/sh
MODE=; SESSION=; RUN_DIR=; PERMISSION=; PROMPT=
ARGS="$*"
while [ $# -gt 0 ]; do
  case "$1" in
    --session-id) MODE=start; SESSION=$2; shift 2 ;;
    --resume) MODE=resume; SESSION=$2; shift 2 ;;
    --add-dir) RUN_DIR=$2; shift 2 ;;
    --permission-mode) PERMISSION=$2; shift 2 ;;
    --output-format|--debug-file|--settings|--model|--effort) shift 2 ;;
    --) PROMPT=$2; shift 2 ;;
    *) shift ;;
  esac
done
DAGQ={dagq}
DB={db}
RECEIPT="$RUN_DIR/receipt.json"
printf '%s %s %s\n' "$MODE" "$SESSION" "$(printf '%s\n' "$PROMPT" | head -n 1 | cut -c1-80)" >> "$RUN_DIR/stub-calls.log"
printf '%s\n' "$ARGS" | head -n 1 >> "$RUN_DIR/stub-args.log"
TURN=$(wc -l < "$RUN_DIR/stub-calls.log" | tr -d ' ')
DENIALS=
RESULTED=
COST=0.01
say() {{ printf '{{"type":"assistant","message":{{"model":"stub","content":[{{"type":"text","text":"%s"}}]}}}}\n' "$1"; }}
result() {{
  printf '{{"type":"result","subtype":"success","is_error":false,"num_turns":2,"duration_ms":5,"total_cost_usd":%s,"session_id":"%s","result":"%s","usage":{{"input_tokens":7,"output_tokens":3}},"permission_denials":[%s]}}\n' "$COST" "$SESSION" "${{1:-done}}" "$DENIALS"
  RESULTED=1
}}
denied() {{ DENIALS='{{"tool_name":"Bash","tool_use_id":"t1","tool_input":{{}}}},{{"tool_name":"Bash","tool_use_id":"t2","tool_input":{{}}}},{{"tool_name":"Edit","tool_use_id":"t3","tool_input":{{}}}}'; }}
fail() {{
  printf '{{"type":"result","subtype":"success","is_error":true,"api_error_status":null,"session_id":"%s","result":"%s"}}\n' "$SESSION" "$1"
  exit 1
}}
commit() {{ printf 'change by %s turn %s\n' "$SESSION" "$TURN" >> change.txt && git add change.txt && git commit -q -m "$1"; }}
receipt() {{
  printf '{{"run_id":"%s","result":"%s","commit":"%s","tests":{{"status":"passed","evidence_or_reason":"ran"}},"e2e":{{"status":"%s","evidence_or_reason":"stub e2e"}},"subagent_review":{{"status":"passed","evidence_or_reason":"reviewed"}},"summary":"turn %s"}}' "${{DAGQ_RUN_ID:-$SESSION}}" "${{2:-succeeded}}" "$1" "${{3:-not_applicable}}" "$TURN" > "$RECEIPT.tmp"
  mv "$RECEIPT.tmp" "$RECEIPT"
}}
ask() {{ "$DAGQ" --db "$DB" ask --run "${{DAGQ_RUN_ID:-$SESSION}}" --kind worker_question --because scope --topic acceptance_conflict --question "$1" --cmux true >/dev/null; }}
printf '{{"type":"system","subtype":"init","session_id":"%s","model":"stub","permissionMode":"%s"}}\n' "$SESSION" "${{PERMISSION_SAID:-$PERMISSION}}"
. {turns}
[ -n "$RESULTED" ] || result
"#,
        dagq = shell_join(&[env!("CARGO_BIN_EXE_dagq").to_owned()]),
        db = "\"$STUB_DB\"",
        turns = "\"${0%/*}/turn.sh\"",
    );
    crate::common::template::script_env(&stub, script, &[("STUB_DB", db.to_str().unwrap())]);

    set_turns(dir, "say working");
    stub
}

/// The home of the stub `codex` of [`headless_codex`], next to it: its
/// rollouts are under `sessions`.
pub const CODEX_HOME: &str = "codex-home";

/// Have the stub `codex` in `dir` write each turn's `model` to its
/// thread's rollout, as Codex does, from now on.
pub fn set_codex_model(dir: &Path, model: &str) {
    fs::write(dir.join("codex-model"), model).unwrap();
}

/// A stub `codex` for headless turns (ADR-t813-1, ADR-t813-3), in `dir`: it
/// takes `codex exec --json -C <worktree> -c … -- <prompt>` and `codex exec
/// resume --json -c … -- <thread> <prompt>`, finds the run directory among
/// the `-c` writable roots, appends `<start|resume> <thread> <prompt's
/// first line>` to `stub-calls.log` there and the arguments of the call
/// (one line each) to `stub-args.log`, prints `thread.started` (a new
/// thread `codex-thread-<turn>` when it starts, the one it resumes
/// otherwise) and `turn.started`, then sources `turn.sh` next to it (see
/// [`set_turns`]) and prints `turn.completed` unless the turn ended. `$TURN`
/// is the turn's number in the run, `$PROMPT` its prompt, `$MODE` `start`
/// or `resume`, `$THREAD` the thread. The turn's helpers: `say TEXT`,
/// `result` (`turn.completed` with its usage), `error TEXT` (an `error`
/// event), `fail TEXT` (`turn.failed`, exit 1), `refused COMMAND` (a
/// command the sandbox refused), `commit MESSAGE`, `receipt COMMIT
/// [RESULT] [EVIDENCE]`, `ask QUESTION`.
pub fn headless_codex(dir: &Path, db: &Path) -> PathBuf {
    let stub = dir.join("codex-headless");
    let script = format!(
        r#"#!/bin/sh
MODE=start; THREAD=; PROMPT=; ROOTS=
ARGS=
for arg in "$@"; do ARGS="$ARGS $arg|"; done
[ "$1" = --version ] && {{ echo "codex-cli 0.46.0"; exit 0; }}
[ "$1" = exec ] || {{ echo "not exec: $*" >&2; exit 2; }}
shift
if [ "$1" = resume ]; then MODE=resume; shift; fi
while [ $# -gt 0 ]; do
  case "$1" in
    -c) case "$2" in sandbox_workspace_write.writable_roots=*) ROOTS=$2 ;; esac; shift 2 ;;
    -C|-m) shift 2 ;;
    --) shift; break ;;
    *) shift ;;
  esac
done
if [ "$MODE" = resume ]; then THREAD=$1; PROMPT=$2; else PROMPT=$1; fi
RUN_DIR=$(printf '%s' "${{ROOTS#*=}}" | tr -d '[]' | tr ',' '
' | sed -n 5p | tr -d '"')
DAGQ={dagq}
DB={db}
RECEIPT="$RUN_DIR/receipt.json"
TURN=$(( $(cat "$RUN_DIR/stub-calls.log" 2>/dev/null | wc -l) + 1 ))
[ -n "$THREAD" ] || THREAD="codex-thread-$TURN"
printf '%s %s %s
' "$MODE" "$THREAD" "$(printf '%s
' "$PROMPT" | head -n 1 | cut -c1-80)" >> "$RUN_DIR/stub-calls.log"
printf '%s
' "$ARGS" >> "$RUN_DIR/stub-args.log"
ENDED=
say() {{ printf '{{"type":"item.completed","item":{{"id":"m%s","type":"agent_message","text":"%s"}}}}
' "$TURN" "$1"; }}
result() {{
  printf '{{"type":"turn.completed","usage":{{"input_tokens":%s,"cached_input_tokens":%s,"output_tokens":%s,"reasoning_output_tokens":%s}}}}
' $((11 * TURN)) $((4 * TURN)) $((5 * TURN)) $((2 * TURN))
  ENDED=1
}}
error() {{ printf '{{"type":"error","message":"%s"}}
' "$1"; }}
fail() {{
  printf '{{"type":"error","message":"%s"}}
{{"type":"turn.failed","error":{{"message":"%s"}}}}
' "$1" "$1"
  exit 1
}}
refused() {{ printf '{{"type":"item.completed","item":{{"id":"c%s","type":"command_execution","command":"%s","exit_code":1,"aggregated_output":"%s: Operation not permitted","status":"failed"}}}}
' "$TURN" "$1" "$1"; }}
commit() {{ printf 'change by %s turn %s
' "$THREAD" "$TURN" >> change.txt && git add change.txt && git commit -q -m "$1"; }}
receipt() {{
  printf '{{"run_id":"%s","result":"%s","commit":"%s","tests":{{"status":"passed","evidence_or_reason":"ran"}},"e2e":{{"status":"%s","evidence_or_reason":"stub e2e"}},"subagent_review":{{"status":"passed","evidence_or_reason":"reviewed"}},"summary":"turn %s"}}' "$DAGQ_RUN_ID" "${{2:-succeeded}}" "$1" "${{3:-not_applicable}}" "$TURN" > "$RECEIPT.tmp"
  mv "$RECEIPT.tmp" "$RECEIPT"
}}
ask() {{ "$DAGQ" --db "$DB" ask --run "$DAGQ_RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question "$1" --cmux true >/dev/null; }}
echo "Reading additional input from stdin..." >&2
printf '{{"type":"thread.started","thread_id":"%s"}}
{{"type":"turn.started"}}
' "$THREAD"
# The model goes to the thread's rollout only, as Codex writes it.
if [ -f {model} ]; then
  SESSIONS={home}/sessions
  ROLLOUT=$(ls "$SESSIONS"/*/*/*/rollout-*-"$THREAD".jsonl 2>/dev/null | head -n 1)
  if [ -z "$ROLLOUT" ]; then
    mkdir -p "$SESSIONS/$(date +%Y/%m/%d)"
    ROLLOUT="$SESSIONS/$(date +%Y/%m/%d)/rollout-2026-09-29T00-00-00-$THREAD.jsonl"
    printf '{{"type":"session_meta","payload":{{"id":"%s"}}}}
' "$THREAD" > "$ROLLOUT"
  fi
  printf '{{"timestamp":"%s","type":"turn_context","payload":{{"model":"%s","effort":"medium"}}}}
' "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" "$(cat {model})" >> "$ROLLOUT"
fi
. {turns}
[ -n "$ENDED" ] || result
"#,
        dagq = shell_join(&[env!("CARGO_BIN_EXE_dagq").to_owned()]),
        db = "\"$STUB_DB\"",
        turns = "\"${0%/*}/turn.sh\"",
        model = "\"${0%/*}/codex-model\"",
        home = "\"${0%/*}/codex-home\"",
    );
    crate::common::template::script_env(&stub, script, &[("STUB_DB", db.to_str().unwrap())]);

    set_turns(dir, "say working");
    stub
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
    // A live session's recovery job escalates (so the alert's ask opens);
    // anything else prints no verdict.
    crate::common::template::script(
        &stub,
        format!(
            "#!/bin/sh\ncase \"$*\" in *\"{LIVE_RECOVERY}\"*) printf '%s\\n' '{ESCALATE}' ;; *) printf 'test provider\\n' ;; esac\n"
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

/// Supervisor options with the test tick and the [`SteadyClock`].
pub fn supervise_options(parallel: usize, once: bool) -> SuperviseOptions {
    let base = SuperviseOptions::new(parallel, once);
    SuperviseOptions {
        tick: TEST_TICK,
        idle_poll: TEST_TICK,
        generators: Generators {
            clock: Arc::new(SteadyClock(SystemTime::now(), Instant::now())),
            ..clock::system()
        },
        // A development build never looks for a release (ADR-t618-1), so
        // no test reaches crates.io even when Cargo.toml names a release.
        release_current: Some("0.0.0-dev+test".to_owned()),
        // No Codex worker unless a test gives its stub: the host's `codex`
        // is not these tests'.
        codex: PathBuf::from("/nonexistent/codex"),
        // No retries of a held /exit (ADR-0047 decision 25) unless a test
        // asks for them: a test's exit timeout goes on to the stuck_exit
        // path at once, as it did before the retries.
        exit: Some(dagq::domain::exit::ExitConfig {
            retries: 0,
            intervals: Vec::new(),
        }),
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
pub struct SteadyClock(pub SystemTime, pub Instant);

impl Clock for SteadyClock {
    fn system_time(&self) -> SystemTime {
        self.0 + self.1.elapsed()
    }
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
    let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
    // A supervise past its limit shows what the queue recorded up to then,
    // so that where it waited can be read from the failure (task 770).
    let _dump = common::on_timeout(
        Duration::from_secs(10),
        format!("print the events of the queue {}", db.display()),
        {
            let db = db.to_owned();
            move || print_queue_events(&db)
        },
    );
    runtime::supervise(
        db,
        repo,
        backend,
        &claude_stub(db),
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        options,
    )
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
    let _dump = common::on_timeout(
        Duration::from_secs(10),
        format!("print the events of the queue {}", db.display()),
        {
            let db = db.to_owned();
            move || print_queue_events(&db)
        },
    );
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
        assert!(
            started.elapsed() < timeout,
            "condition not met within {timeout:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Run one fake agent script through supervise and return the task detail.
pub fn run_agent(script: &str) -> (Fixture, PathBuf, dagq::domain::TaskDetail) {
    run_agent_with(script, false)
}

pub fn run_agent_with(
    script: &str,
    close_fail: bool,
) -> (Fixture, PathBuf, dagq::domain::TaskDetail) {
    let (dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, script);
    backend.close_fail = close_fail;
    let outcome = supervise(&db, &repo, &backend).unwrap();
    // These scripts exit on their own, like a person's /exit; nothing was requested.
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    backend.join();
    assert_eq!(outcome["outcome"], "finished");
    assert_eq!(outcome["runs"].as_array().unwrap().len(), 1);
    assert_eq!(outcome["errors"], json!([]));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    let run = &detail.runs[0];
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
    // Every outcome keeps the worktree; only an accepted run closes its workspace.
    assert!(Path::new(run.worktree_path().unwrap()).exists());
    assert_eq!(run.workspace_id(), Some(WORKSPACE_ID));
    let kinds: Vec<&str> = detail.events.iter().map(|e| e.kind.as_str()).collect();
    if run.status() == RunStatus::AwaitingIntegration && !close_fail {
        assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
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
    // The run workspace carries its role, queue, actor id, run and task in
    // its environment (ADR-t728-1 decision 4), a description naming the run
    // and task, and the queue's group.
    let canonical = db.canonicalize().unwrap();
    let hash = QueueLocation::explicit(&canonical).hash();
    assert_eq!(
        *backend.tags.lock().unwrap(),
        vec![WorkspaceTags {
            env: vec![
                ("DAGQ_ROLE".into(), "worker".into()),
                ("DAGQ_QUEUE".into(), canonical.to_str().unwrap().into()),
                ("DAGQ_ACTOR_ID".into(), format!("worker:{}", run.id())),
                ("DAGQ_RUN_ID".into(), run.id().to_string()),
                ("DAGQ_TASK_ID".into(), run.task_id().to_string()),
            ],
            description: Some(format!(
                "dagq role=worker queue={hash} run={} task={}",
                run.id(),
                run.task_id()
            )),
            group: Some(format!("group-{hash}")),
        }]
    );
    assert_eq!(backend.groups.lock().unwrap().len(), 1);
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

/// Fake agent that ignores the supervisor's `/exit` (as when a dialog holds
/// it back) and ends only once the test writes `$EXIT.held`, the way a person
/// would answer the dialog and exit.
/// Blocks a fake session until the test calls `release_held_session`.
pub const HOLD: &str = "while [ ! -f \"$EXIT.held\" ]; do sleep 0.05; done";

pub const HELD_AGENT: &str = "commit work; receipt \"$(git rev-parse HEAD)\"; idle; while [ ! -f \"$EXIT.held\" ]; do sleep 0.05; done";

pub fn release_held_session(run_dir: &str) {
    fs::write(Path::new(run_dir).join("exit-requested.held"), "").unwrap();
}

/// A screen of ordinary work.
pub const WORK_SCREEN: &str =
    "⏺ Bash(cargo test)\n  ⎿  test result: ok\n\n│ ❯ \n  ? for shortcuts\n";

/// Fake agent that works (no receipt, no idle marker) until the test writes
/// `$EXIT.go`, then finishes like `VALID_AGENT` and waits for `/exit`.
pub const PROMPTED_AGENT: &str = "while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit";

/// A session stopped at a login that ran out (task 266).
pub const LOGIN_SCREEN: &str = "\
⏺ Bash(cargo test)
  ⎿  API Error: 401 {\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"OAuth token has expired.\"}} · Please run /login

│ ❯ 
  ? for shortcuts
";

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
/// as wrapper and agent, but with no supervisor loop watching it. Returns the
/// running run.
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
    queue
        .workspace_created(
            run.id(),
            &LeaseToken::new(token),
            &format!("ws-{}", run.task_id()),
        )
        .unwrap();
    queue
        .register_wrapper(run.id(), &LeaseToken::new(token), wrapper)
        .unwrap();
    queue.register_agent(run.id(), wrapper, agent).unwrap();
    let run = queue.run(run.id()).unwrap();
    assert_eq!(run.status(), RunStatus::Running);
    run
}

pub fn git_out(repo: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
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

/// A validated run plus a ready dependent task, before any landing.
pub fn awaiting_run() -> (Fixture, PathBuf, PathBuf, TaskRun) {
    let (dir, db, detail) = run_agent(VALID_AGENT);
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
            worker_mode: None,
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
/// (claim, plan, prompt, worktree, workspace with the wrapper thread of the
/// test backend), but with no loop or heartbeat behind the token: the state
/// a supervisor leaves when it is killed after the session started. Returns
/// the `running` run once the wrapper registered its agent.
pub fn start_run_under_dead_supervisor(
    repo: &Path,
    db: &Path,
    backend: &TestWorkspace,
    token: &str,
) -> TaskRun {
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
        runtime::prompt(&task, &run, None, &[], &[], &[], None, &[], &[]).unwrap(),
    )
    .unwrap();
    repository.create_worktree(&run).unwrap();
    let command = shell_join(&[
        "runner".into(),
        "--db".into(),
        path_text(db).unwrap(),
        "session".into(),
    ]);
    let workspace = backend
        .create(&task, &run, &command, &WorkspaceTags::default())
        .unwrap();
    queue
        .workspace_created(run.id(), &LeaseToken::new(token), &workspace)
        .unwrap();
    wait_until(db, Duration::from_secs(10), |queue| {
        queue.run(run.id()).unwrap().status() == RunStatus::Running
    });
    queue.run(run.id()).unwrap()
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

/// Stands in for the headless reviewer (ADR-0027): each review runs the
/// next script with `/bin/sh -c` in the worktree (the last one repeats) and
/// records its prompt; `timeout` is the review timeout.
pub struct TestReviewer {
    pub scripts: Mutex<Vec<String>>,
    pub prompts: Mutex<Vec<String>>,
    pub timeout: Duration,
    /// Scripts of the headless recovery jobs, one per job in order; without
    /// one left, a job cannot start.
    pub triages: Mutex<Vec<String>>,
    /// The recovery job prompts and the directories they ran in.
    pub triage_prompts: Mutex<Vec<(String, PathBuf)>>,
    /// The model and effort each job was given (ADR-0079 decision 7), in
    /// order; a job started as before gives none.
    pub models: Mutex<Vec<(String, String)>>,
}

impl TestReviewer {
    pub fn new(scripts: &[String]) -> Self {
        Self {
            scripts: Mutex::new(scripts.to_vec()),
            prompts: Mutex::new(Vec::new()),
            timeout: Duration::from_secs(60),
            triages: Mutex::new(Vec::new()),
            triage_prompts: Mutex::new(Vec::new()),
            models: Mutex::new(Vec::new()),
        }
    }
    pub fn models(&self) -> Vec<(String, String)> {
        self.models.lock().unwrap().clone()
    }
    pub fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
    pub fn with_triages(self, scripts: &[String]) -> Self {
        *self.triages.lock().unwrap() = scripts.to_vec();
        self
    }
    pub fn triage_prompts(&self) -> Vec<(String, PathBuf)> {
        self.triage_prompts.lock().unwrap().clone()
    }
}

impl AgentProvider for TestReviewer {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn command(&self, _: &TaskRun, _: &str) -> Result<CommandSpec> {
        unreachable!("the reviewer starts no session")
    }
    fn resume_command(&self, _: &TaskRun) -> Result<CommandSpec> {
        unreachable!("the reviewer starts no session")
    }
    // A run that fails under these tests is recovered by this provider too:
    // with no script left, a live session's recovery job escalates, and one
    // for a run that ended cannot start (it waits to be recovered by hand).
    // A goal whose tasks all landed is not reviewed by this provider: its
    // goal review cannot start, and the goal stays open (tests/it/goal_review.rs
    // plays the goal review).
    fn headless_command(&self, cwd: &Path, prompt: &str, access: JobAccess) -> Result<CommandSpec> {
        ensure!(
            !prompt.starts_with("You are the goal review"),
            "the test reviewer runs no goal review"
        );
        assert_eq!(access, runtime::TRIAGE_ACCESS);
        let mut triages = self.triages.lock().unwrap();
        if triages.is_empty() && prompt.contains(LIVE_RECOVERY) {
            triages.push(format!("printf '%s\\n' '{ESCALATE}'"));
        }
        ensure!(
            !triages.is_empty(),
            "the test reviewer has no recovery job left"
        );
        self.triage_prompts
            .lock()
            .unwrap()
            .push((prompt.into(), cwd.into()));
        let mut command = CommandSpec::new("/bin/sh");
        command.current_dir(cwd).arg("-c").arg(triages.remove(0));
        Ok(command)
    }
    fn review_command(
        &self,
        run: &TaskRun,
        prompt: &str,
        access: JobAccess,
    ) -> Result<CommandSpec> {
        assert_eq!(access, runtime::REVIEW_ACCESS);
        self.prompts.lock().unwrap().push(prompt.into());
        let mut scripts = self.scripts.lock().unwrap();
        let script = if scripts.len() > 1 {
            scripts.remove(0)
        } else {
            scripts[0].clone()
        };
        ensure!(
            script != UNSTARTABLE_REVIEW,
            "the test reviewer cannot start this review"
        );
        let mut command = CommandSpec::new("/bin/sh");
        command
            .current_dir(run.worktree_path().unwrap())
            .arg("-c")
            .arg(script);
        Ok(command)
    }
    fn review_timeout(&self) -> Duration {
        self.timeout
    }
    fn select_model(&self, _: &mut CommandSpec, model: &str, effort: &str) {
        self.models
            .lock()
            .unwrap()
            .push((model.into(), effort.into()));
    }
}

/// A reviewer script whose review cannot start: `review_command` fails, so
/// no job runs and writes `review-N.out` / `.err`.
pub const UNSTARTABLE_REVIEW: &str = "<unstartable review>";

/// A reviewer script that prints the verdict JSON.
pub fn verdict(decision: &str, reasons: &[&str], summary: &str) -> String {
    let json = json!({"verdict": decision, "reasons": reasons, "summary": summary});
    format!("printf '%s\\n' '{json}'")
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
