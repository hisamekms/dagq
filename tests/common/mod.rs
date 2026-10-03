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
//! outside the process (the e2e tests' cmux workspace group, a supervisor
//! they started) is registered with [`on_timeout`] too: the monitor runs
//! those hooks, each within its own limit, before it exits (task 440).
#![allow(dead_code)]

pub mod actor;
pub mod cli;
pub mod lifecycle;
pub mod queue;
pub mod service;
pub mod template;

pub use actor::WithoutActor;

use std::{
    collections::HashMap,
    io::Write,
    process::{self, Command, ExitStatus, Output},
    sync::{
        LazyLock, Mutex, MutexGuard, PoisonError,
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
    limit: Duration,
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

/// Run every registered hook, newest first, each within its limit.
fn run_hooks() {
    let mut pending: Vec<_> = hooks().drain().collect();
    pending.sort_by_key(|(id, _)| std::cmp::Reverse(*id));
    for (_, hook) in pending {
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
            Err(error) => format!("could not start: {error}"),
            Ok(_) => match finished.recv_timeout(hook.limit) {
                Ok(()) => "done".to_owned(),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    format!("did not finish within {:?}", hook.limit)
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => "panicked".to_owned(),
            },
        };
        report(&format!("  cleanup {} {outcome}\n", hook.what));
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
/// condition ("the supervisor thread to return").
pub fn within(limit: Duration, what: impl Into<String>) -> Waiting {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let test = thread::current()
        .name()
        .unwrap_or("(on a thread the test started)")
        .to_owned();
    open().insert(
        id,
        Open {
            test,
            what: what.into(),
            started: Instant::now(),
            limit,
        },
    );
    Waiting(id)
}

/// The whole test the calling thread runs, for a fixture to hold.
pub fn test() -> Waiting {
    within(TEST_LIMIT, "the test to finish")
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
        let late = late_report(&open());
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

/// The report of a wait past its limit, and of every other open wait, if
/// one is past its limit.
fn late_report(open: &HashMap<u64, Open>) -> Option<String> {
    let late = open
        .values()
        .find(|wait| wait.started.elapsed() >= wait.limit)?;
    let mut report = format!(
        "\ntest {} timed out: {} did not happen within {:?}\n",
        late.test, late.what, late.limit
    );
    let mut others: Vec<_> = open
        .values()
        .filter(|wait| !std::ptr::eq(*wait, late))
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
