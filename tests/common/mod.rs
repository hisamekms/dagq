//! Time limits for the waits of the integration tests (task 324).
//!
//! A test that waits on a thread, a child process or a stub agent can wait
//! forever when the condition never holds: `JoinHandle::join`,
//! `Child::wait` and `Command::output` have no deadline of their own. Such a
//! wait is wrapped in [`within`]; a monitor thread checks every open wait,
//! and one past its limit ends the whole test process with a failure that
//! names the test and what it waited for, rather than leaving
//! `cargo test | tail` hanging. Every test binary that uses it exits then,
//! since the stuck thread cannot be stopped from outside.
//!
//! Exiting that way skips every `Drop`, so what a guard would clean up
//! outside the process (a supervisor the e2e tests started) is registered with [`on_timeout`] too: the monitor runs
//! those hooks, each within its own limit, before it exits (task 440).
//!
//! A binary may opt into deadlines of its own with [`deadlines`] (the e2e's
//! do): a deadline of the whole process from that call, every test's
//! ([`test_within`]) ending before it, every wait and polling loop
//! ([`deadline`]) of a test ending before the test's, and an overall limit
//! of the cleanup, past which the hooks not run yet are left and named.
//! Without it (`tests/it`) a wait has its own limit only and the cleanup
//! runs every hook within its own. What is decided, the deadlines and the
//! cut of the cleanup, is [`dagq::domain::e2e_deadline`]'s.
#![allow(dead_code)]

pub mod actor;
pub mod attributes;
pub mod cli;
pub mod install;
pub mod lifecycle;
pub mod queue;
pub mod report;
pub mod service;
pub mod setup_review;
pub mod template;
pub mod toolchain;

pub use actor::WithoutActor;

use dagq::domain::e2e_deadline::{self, HookOutcome};
use std::{
    cell::Cell,
    collections::HashMap,
    io::Write,
    process::{self, Command, ExitStatus, Output},
    sync::{
        LazyLock, Mutex, MutexGuard, OnceLock, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

/// A whole test: the fixtures open one for the test that uses them. Well
/// above the slowest test's minute or so under `cargo llvm-cov`'s load.
pub const TEST_LIMIT: Duration = Duration::from_secs(600);

/// A single step inside a test: a thread, a session or a process that is
/// expected to be done by then.
pub const STEP_LIMIT: Duration = Duration::from_secs(300);

/// The exit code of a timed-out run, the one a failed test binary has.
pub const TIMED_OUT: i32 = 101;

struct Open {
    test: String,
    what: String,
    started: Instant,
    /// Its own limit, as asked for.
    own: Duration,
    /// When it is late, since [`ORIGIN`]: `own` from `started`, or before
    /// the deadline of what encloses it.
    deadline: Duration,
}

impl Open {
    /// How long it may take: its own limit, or less when cut to end before
    /// what encloses it.
    fn limit(&self) -> Duration {
        self.deadline.saturating_sub(since_origin(self.started))
    }
}

/// What the deadlines are measured from.
static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);

fn since_origin(at: Instant) -> Duration {
    at.saturating_duration_since(*ORIGIN)
}

/// The deadlines a binary opted into with [`deadlines`].
struct Deadlines {
    /// The process's, since [`ORIGIN`].
    run_end: Duration,
    /// The overall limit of the cleanup past a timeout.
    cleanup: Duration,
}

static DEADLINES: OnceLock<Deadlines> = OnceLock::new();

thread_local! {
    /// The deadline of the test the thread runs, since [`ORIGIN`], while
    /// its [`TestWait`] is held.
    static TEST_END: Cell<Option<Duration>> = const { Cell::new(None) };
}

/// The deadline of what encloses a wait the calling thread opens: its
/// test's, or else the process's, when the binary opted in.
fn enclosing() -> Option<Duration> {
    TEST_END
        .get()
        .or_else(|| DEADLINES.get().map(|deadlines| deadlines.run_end))
}

static OPEN: LazyLock<Mutex<HashMap<u64, Open>>> = LazyLock::new(|| {
    thread::Builder::new()
        .name("test deadline monitor".into())
        .spawn(monitor)
        .unwrap();
    Mutex::new(HashMap::new())
});
static NEXT: AtomicU64 = AtomicU64::new(0);
/// Set once a wait is past its limit, while the monitor runs the hooks.
static TIMING_OUT: AtomicBool = AtomicBool::new(false);
/// The name of the threads the hooks run on.
const CLEANUP_THREAD: &str = "timeout cleanup";

struct Hook {
    what: String,
    limit: Duration,
    run: Box<dyn FnOnce() + Send>,
}

static HOOKS: LazyLock<Mutex<HashMap<u64, Hook>>> = LazyLock::new(Default::default);

fn hooks() -> MutexGuard<'static, HashMap<u64, Hook>> {
    HOOKS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A cleanup [`on_timeout`] registered; dropping it, once the guard it
/// stands in for has cleaned up by itself, unregisters it.
#[must_use = "the cleanup is registered only while this is held"]
pub struct Cleanup(u64);
impl Drop for Cleanup {
    fn drop(&mut self) {
        hooks().remove(&self.0);
    }
}

/// Have `hook` run if a wait times out while the returned [`Cleanup`] is
/// held, before the process exits. Hooks run newest first, as drops would,
/// each on its own thread; one still running after `limit` is reported and
/// left behind, so a stuck hook cannot keep the process from exiting.
pub fn on_timeout(
    limit: Duration,
    what: impl Into<String>,
    hook: impl FnOnce() + Send + 'static,
) -> Cleanup {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    hooks().insert(
        id,
        Hook {
            what: what.into(),
            limit,
            run: Box::new(hook),
        },
    );
    Cleanup(id)
}

/// Run every registered hook, newest first, each within its limit, and
/// within the overall limit of the cleanup when the binary opted into one
/// ([`deadlines`]): past it the hooks not run yet are left, and what ended,
/// what was left running and what was not run are named.
fn run_hooks() {
    let started = Instant::now();
    let mut cleanup = e2e_deadline::Cleanup::new(DEADLINES.get().map(|d| d.cleanup));
    let mut pending: Vec<_> = hooks().drain().collect();
    pending.sort_by_key(|(id, _)| std::cmp::Reverse(*id));
    for (_, hook) in pending {
        let Some(wait) = cleanup.wait_for(&hook.what, hook.limit, started.elapsed()) else {
            continue;
        };
        report(&format!("cleaning up before the exit: {}\n", hook.what));
        let (done, finished) = mpsc::channel();
        let run = hook.run;
        let spawned = thread::Builder::new()
            .name(CLEANUP_THREAD.into())
            .spawn(move || {
                run();
                let _ = done.send(());
            });
        let outcome = match spawned {
            Err(error) => HookOutcome::CouldNotStart(error.to_string()),
            Ok(_) => match finished.recv_timeout(wait) {
                Ok(()) => HookOutcome::Done,
                Err(mpsc::RecvTimeoutError::Timeout) => HookOutcome::DidNotFinish(wait),
                Err(mpsc::RecvTimeoutError::Disconnected) => HookOutcome::Panicked,
            },
        };
        report(&cleanup.ended(&hook.what, hook.limit, outcome));
    }
    if let Some(summary) = cleanup.summary() {
        report(&summary);
    }
}

/// Straight to the process's stderr: the test harness captures only
/// `print!`/`eprint!`, and the process ends before it would print.
fn report(text: &str) {
    let _ = std::io::stderr().lock().write_all(text.as_bytes());
}

fn open() -> MutexGuard<'static, HashMap<u64, Open>> {
    OPEN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An open wait; dropping it, when the wait returned or panicked, closes it.
#[must_use = "the wait is timed only while this is held"]
pub struct Waiting(u64);
impl Drop for Waiting {
    fn drop(&mut self) {
        // Once the monitor is cleaning up for a timeout, a test thread
        // whose wait ends meanwhile (the cleanup may be what ends it) stops
        // here: were it to go on and finish the last test, the harness
        // would exit with its own code before the monitor exits 101.
        if TIMING_OUT.load(Ordering::SeqCst) && thread::current().name() != Some(CLEANUP_THREAD) {
            loop {
                thread::park();
            }
        }
        open().remove(&self.0);
    }
}

/// Time what the calling thread waits for from now, `what` naming the
/// condition ("the supervisor thread to return"): within `limit`, cut to
/// end before its test's or the process's deadline when the binary opted
/// into [`deadlines`].
pub fn within(limit: Duration, what: impl Into<String>) -> Waiting {
    LazyLock::force(&ORIGIN);
    let started = Instant::now();
    let deadline = e2e_deadline::nested_deadline(
        since_origin(started),
        limit,
        enclosing(),
        e2e_deadline::WAIT_MARGIN,
    );
    let test = thread::current()
        .name()
        .unwrap_or("(on a thread the test started)")
        .to_owned();
    Waiting(open_wait(test, what.into(), started, limit, deadline))
}

fn open_wait(
    test: String,
    what: String,
    started: Instant,
    own: Duration,
    deadline: Duration,
) -> u64 {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    open().insert(
        id,
        Open {
            test,
            what,
            started,
            own,
            deadline,
        },
    );
    id
}

/// The whole test the calling thread runs, for a fixture to hold.
pub fn test() -> Waiting {
    within(TEST_LIMIT, "the test to finish")
}

/// Opt this test binary into the deadlines of the e2e (the first call sets
/// them; a later one changes nothing): the process's, `run_limit` from now,
/// past which it reports every open wait, cleans up and exits like a wait
/// past its limit; and `cleanup`, the overall limit of that cleanup.
pub fn deadlines(run_limit: Duration, cleanup: Duration) {
    LazyLock::force(&ORIGIN);
    let started = Instant::now();
    let mut first = false;
    let set = DEADLINES.get_or_init(|| {
        first = true;
        Deadlines {
            run_end: since_origin(started) + run_limit,
            cleanup,
        }
    });
    if first {
        // Never closed: it ends with the process.
        open_wait(
            "(the e2e test binary)".to_owned(),
            "the e2e process to end by its deadline".to_owned(),
            started,
            run_limit,
            set.run_end,
        );
    }
}

/// [`test`] within `limit` for a binary that opted into [`deadlines`]:
/// the test ends before the process's deadline however late it started,
/// and what it waits for on this thread ([`within`], [`deadline`]) ends
/// before its own deadline, so a wait that stops is named first.
pub fn test_within(limit: Duration) -> TestWait {
    let waiting = within(limit, "the test to finish");
    let end = open()[&waiting.0].deadline;
    let enclosing = TEST_END.replace(Some(end));
    TestWait(waiting, enclosing)
}

/// The whole test [`test_within`] timed; dropping it closes it.
#[must_use = "the test is timed only while this is held"]
pub struct TestWait(Waiting, Option<Duration>);
impl Drop for TestWait {
    fn drop(&mut self) {
        TEST_END.set(self.1);
    }
}

/// `thread::spawn` for a thread of the test the calling thread runs: what
/// it waits for ([`within`], [`deadline`]) ends before that test's
/// deadline, as on the test's own thread.
pub fn spawn_in_test<T: Send + 'static>(
    run: impl FnOnce() -> T + Send + 'static,
) -> thread::JoinHandle<T> {
    let end = TEST_END.get();
    thread::spawn(move || {
        TEST_END.set(end);
        run()
    })
}

/// The deadline of a polling loop of the calling thread that may go on for
/// `limit` from now: [`deadline_after`] now.
pub fn deadline(limit: Duration) -> Instant {
    deadline_after(Instant::now(), limit)
}

/// The deadline of a polling loop that may go on for `limit` from `start`:
/// cut to end before its test's or the process's deadline when the binary
/// opted into [`deadlines`], with time left for the loop's own failure to
/// be the one named. On a thread the test started, only one started with
/// [`spawn_in_test`] (or a deadline taken on the test's thread) ends before
/// the test's.
pub fn deadline_after(start: Instant, limit: Duration) -> Instant {
    LazyLock::force(&ORIGIN);
    let end = e2e_deadline::nested_deadline(
        since_origin(start),
        limit,
        enclosing(),
        e2e_deadline::LOOP_MARGIN,
    );
    *ORIGIN + end
}

/// `Command::output` and `Command::status` timed with [`STEP_LIMIT`]: the
/// test's child processes (the `dagq` binary, git, the hooks' shells).
pub trait Bounded {
    fn bounded_output(&mut self) -> std::io::Result<Output>;
    fn bounded_status(&mut self) -> std::io::Result<ExitStatus>;
}
impl Bounded for Command {
    fn bounded_output(&mut self) -> std::io::Result<Output> {
        let _waiting = within(STEP_LIMIT, exits(self));
        self.output()
    }
    fn bounded_status(&mut self) -> std::io::Result<ExitStatus> {
        let _waiting = within(STEP_LIMIT, exits(self));
        self.status()
    }
}

fn exits(command: &Command) -> String {
    let mut line = command.get_program().to_string_lossy().into_owned();
    for arg in command.get_args() {
        line.push(' ');
        line += &arg.to_string_lossy();
    }
    format!("`{line}` to exit")
}

fn monitor() {
    loop {
        thread::sleep(Duration::from_millis(250));
        let now = since_origin(Instant::now());
        let late = late_report(&open(), now);
        let Some(late) = late else {
            continue;
        };
        report(&late);
        // The lock on the open waits is released: a hook may time its own
        // commands with [`within`].
        TIMING_OUT.store(true, Ordering::SeqCst);
        run_hooks();
        process::exit(TIMED_OUT);
    }
}

/// The report of a wait past its deadline at `now`, the first to come
/// when more are, and of every other open wait, if one is past it.
fn late_report(open: &HashMap<u64, Open>, now: Duration) -> Option<String> {
    let id = e2e_deadline::first_late(open.iter().map(|(id, wait)| (*id, wait.deadline)), now)?;
    let late = &open[&id];
    let mut report = format!(
        "\ntest {} timed out: {} did not happen within {:?}\n",
        late.test,
        late.what,
        late.limit()
    );
    if late.limit() < late.own {
        report += &format!(
            "  (its own limit {:?} was cut to end before its test's or the process's deadline)\n",
            late.own
        );
    }
    let mut others: Vec<_> = open
        .iter()
        .filter(|(other, _)| **other != id)
        .map(|(_, wait)| wait)
        .collect();
    others.sort_by_key(|wait| wait.started);
    for wait in others {
        report += &format!(
            "  also waiting: test {} for {} (for {:?})\n",
            wait.test,
            wait.what,
            wait.started.elapsed()
        );
    }
    Some(report)
}

/// A child the test started that would not exit by itself (a `watch
/// --until-attention`): killed by its handle when the guard drops (the test
/// passed or panicked) and, through [`on_timeout`], by its pid when a wait
/// times out and the process exits without the drops.
pub struct KillOnDrop {
    child: Option<process::Child>,
    _cleanup: Cleanup,
}

impl KillOnDrop {
    pub fn new(child: process::Child, what: impl Into<String>) -> Self {
        let pid = child.id().to_string();
        let cleanup = on_timeout(STEP_LIMIT, what, move || {
            let _ = Command::new("kill").args(["-KILL", &pid]).status();
        });
        Self {
            child: Some(child),
            _cleanup: cleanup,
        }
    }

    pub fn child(&mut self) -> &mut process::Child {
        self.child.as_mut().expect("the child is still held")
    }

    /// Waits for the child and its output; a timeout still kills it.
    pub fn wait_with_output(mut self) -> std::io::Result<Output> {
        let child = self.child.take().expect("the child is still held");
        child.wait_with_output()
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// `path` quoted for a string a shell runs (a stub's script, a task's
/// verification command, a wrapper): the runtime fixtures name their queue
/// and repository with an apostrophe (`queue's data.db`, `repo's
/// directory`), so a path from them put in as `'{}'` closes the quote early
/// and the shell stops at a syntax error (task 1364).
pub fn shell_path(path: impl AsRef<std::path::Path>) -> String {
    dagq::infrastructure::adapters::shell_quote(path.as_ref().to_str().unwrap())
}

/// A temporary directory in `/tmp`, not in `$TMPDIR`, for a fixture that
/// must be short and under no `runs/` directory. A worker's turns, the
/// tests it runs in them included, have their run directory's `tmp`
/// (`<queue>/runs/<run>/tmp`) as their `TMPDIR`. Under it, a fixture's queue
/// is taken for a run directory's files (no link followed, no directory's
/// time read). A socket's path there may pass the `sun_path` limit (104
/// bytes on macOS), and the queue's socket then moves out of its
/// `service/`. A run's worktree there is too long to name its Claude Code
/// scratchpad (200 characters at most).
pub fn short_tempdir() -> tempfile::TempDir {
    tempfile::tempdir_in("/tmp").unwrap()
}

/// Set the permission bits of `path` to `mode`: a test makes a file or a
/// directory of the queue unreadable or unwritable to inject a failure of
/// the host, and gives the bits back once the failure is to be gone.
pub fn set_mode(path: impl AsRef<std::path::Path>, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// The shell function `await_file PATH`, for a shell the test starts (a
/// stub's script, a task's verification, a reviewer's or job's script) to
/// wait for the file at the absolute `PATH` the test writes: it waits while
/// the test is there to write it, and exits the shell with 1 once the
/// file's directory is gone (the test's `TempDir` was dropped) or the
/// test process is (a timeout's `process::exit` skips the drops): the one
/// a headless stub's `STUB_TEST_PID` names, or else the shell's parent. A
/// bare `while [ ! -f PATH ]` left the shell looping after a test that
/// failed before writing the file (task 1580). A literal, for the stubs'
/// preludes to `concat!`.
#[macro_export]
macro_rules! await_file_fn {
    () => {
        r#"await_file() { while [ ! -f "$1" ]; do [ -d "${1%/*}" ] && kill -0 "${STUB_TEST_PID:-$PPID}" 2>/dev/null || exit 1; sleep 0.05; done; }"#
    };
}

/// [`await_file_fn`]'s function.
pub const AWAIT_FILE: &str = await_file_fn!();

/// A shell's wait for the file `word` names (a shell word: a quoted path,
/// [`shell_path`], or one of the script's variables, `"$EXIT.go"`) that
/// stops with the test: [`AWAIT_FILE`] defined and called, for a script
/// without a prelude that defines it.
pub fn await_file(word: &str) -> String {
    format!("{AWAIT_FILE}; await_file {word}")
}

/// [`await_file`] for `path`.
pub fn await_path(path: impl AsRef<std::path::Path>) -> String {
    await_file(&shell_path(path))
}
