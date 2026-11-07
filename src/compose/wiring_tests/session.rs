//! The session wrappers of a wiring test: a background start runs the
//! production wrapper ([`crate::application::session::run_session`]) of the
//! run its command names on a thread of the test, as a process of
//! [`FakeProcesses`] whose pid and start make its handle; the thread is
//! joined by the test ([`FakeSessions::join_by`]).

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde_json::Value;

use super::{
    agent::FakeAgent,
    host::{FakeProcesses, Nothing},
    service::MemoryTokens,
};
use crate::{
    application::{
        Clock, ProcessControl, RunFiles, SessionWrappers,
        memory_files::MemoryFiles,
        session::{Session, WrapperStart, run_session},
    },
    domain::{
        LeaseToken, RunId,
        actor::{ActorContext, ActorRole},
        background_wrapper::{BackgroundHandle, StopRoute, WrapperStop, start_token},
        turn::exit_path,
    },
    infrastructure::sqlite::SqliteQueue,
};

/// How often a test's wrapper looks again for the record of its start: the
/// test's own, shorter than the production's
/// [`REGISTRATION_POLL`](crate::application::session::REGISTRATION_POLL).
/// Each look opens the queue, so a much shorter wait takes from the
/// supervisor's passes the CPU they need.
const REGISTRATION_POLL: Duration = Duration::from_millis(50);

/// A wrapper started on a thread: its handle and run directory, and its
/// thread with the channel that says it returned.
struct Wrapper {
    handle: String,
    run_dir: PathBuf,
    thread: JoinHandle<Result<Value>>,
    returned: mpsc::Receiver<()>,
}

/// The wrappers of a test, and what they run with.
pub(super) struct FakeSessions {
    pub(super) db: PathBuf,
    pub(super) agent: Arc<FakeAgent>,
    pub(super) tokens: Arc<MemoryTokens>,
    pub(super) processes: Arc<FakeProcesses>,
    pub(super) files: Arc<MemoryFiles>,
    pub(super) clock: Arc<dyn Clock>,
    wrappers: Mutex<Vec<Wrapper>>,
    /// What lets the next wrapper started (not a resume's) go on to its
    /// session, once: it waits, started and not yet registered, until it
    /// says so or is dropped (by the test's deadline at the latest).
    launch_gate: Mutex<Option<mpsc::Receiver<()>>>,
    /// Whether a resume's wrapper fails to start, as a launch that the
    /// host refused.
    pub(super) fail_resumes: std::sync::atomic::AtomicBool,
    /// Whether a new run's wrapper fails to start, as a launch that the
    /// host refused.
    pub(super) fail_claims: std::sync::atomic::AtomicBool,
}

impl FakeSessions {
    pub(super) fn new(
        db: PathBuf,
        agent: Arc<FakeAgent>,
        tokens: Arc<MemoryTokens>,
        processes: Arc<FakeProcesses>,
        files: Arc<MemoryFiles>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            db,
            agent,
            tokens,
            processes,
            files,
            clock,
            wrappers: Mutex::default(),
            launch_gate: Mutex::default(),
            fail_resumes: std::sync::atomic::AtomicBool::new(false),
            fail_claims: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Hold the next wrapper started for a run's first session before it
    /// runs: it waits until `go` says so, as a wrapper slowed between its
    /// launch and its first turn.
    pub(super) fn hold_next_launch(&self, go: mpsc::Receiver<()>) {
        *self
            .launch_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(go);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Wrapper>> {
        self.wrappers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Join every wrapper started, each asked to exit, by `deadline`: what
    /// each returned. One that has not returned by then is named with the
    /// test's `name` in the panic.
    pub(super) fn join_by(&self, name: &str, deadline: Instant) -> Vec<Result<Value>> {
        let wrappers = std::mem::take(&mut *self.lock());
        // Every wrapper is asked to exit first, and each is waited for
        // before any that did not return fails the test.
        for wrapper in &wrappers {
            let _ = self.files.write(&exit_path(&wrapper.run_dir), b"");
        }
        let mut stuck = Vec::new();
        let mut returned = Vec::new();
        for wrapper in wrappers {
            let left = deadline.saturating_duration_since(Instant::now());
            if wrapper.returned.recv_timeout(left).is_err() {
                stuck.push(wrapper.handle);
                continue;
            }
            returned.push(wrapper.thread.join().unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "the session wrapper {} panicked",
                    wrapper.handle
                ))
            }));
        }
        assert!(
            stuck.is_empty(),
            "{name}: the session wrappers {stuck:?} did not return by the test's deadline"
        );
        returned
    }
}

/// The value of `flag` among the words of a wrapper's command.
fn after<'a>(words: &[&'a str], flag: &str) -> Option<&'a str> {
    let at = words.iter().position(|word| *word == flag)?;
    words.get(at + 1).copied()
}

impl SessionWrappers for FakeSessions {
    /// The wrapper of the run the command names (`--run`, `--lease`,
    /// `--resume`), on a thread as a new process.
    fn launch_background(
        &self,
        _: &std::path::Path,
        command: &str,
        _: &[(String, String)],
        _: &std::path::Path,
    ) -> Result<String> {
        let words: Vec<&str> = command
            .split(' ')
            .map(|word| word.trim_matches('\''))
            .collect();
        let id = RunId::new(after(&words, "--run").context("the wrapper names no run")?)?;
        let token =
            LeaseToken::new(after(&words, "--lease").context("the wrapper names no lease")?);
        let resume = words.contains(&"--resume");
        if !resume && self.fail_claims.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("the wiring test refuses to start the wrapper of run {id}");
        }
        if resume && self.fail_resumes.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("the wiring test refuses to start the wrapper of run {id}'s resume");
        }
        let (pid, start) = self.processes.start();
        let handle = BackgroundHandle {
            pid,
            start: start_token(&start),
        }
        .to_string();
        let run_dir = PathBuf::from(
            crate::application::RunReads::run(&SqliteQueue::open(&self.db)?, &id)?
                .run_dir()
                .context("the run has no directory")?,
        );
        let (db, agent, tokens, processes, files, clock) = (
            self.db.clone(),
            self.agent.clone(),
            self.tokens.clone(),
            self.processes.clone(),
            self.files.clone(),
            self.clock.clone(),
        );
        let gate = if resume {
            None
        } else {
            self.launch_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
        };
        let (returned, returns) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name(format!("wrapper {id}"))
            .spawn(move || {
                if let Some(gate) = gate {
                    let _ = gate.recv_timeout(super::harness::LIMIT);
                }
                let returned_value = wrap(
                    &db, &id, &token, resume, pid, &agent, &tokens, &processes, &files, &*clock,
                );
                // Its process ends with it.
                processes.end(pid);
                let _ = returned.send(());
                returned_value
            })?;
        self.lock().push(Wrapper {
            handle: handle.clone(),
            run_dir,
            thread,
            returned: returns,
        });
        Ok(handle)
    }

    /// The wrapper asked to exit, as the production stop asks it; its
    /// thread ends at its next look.
    fn stop_background(&self, handle: &str, _: StopRoute) -> Result<Option<WrapperStop>> {
        let run_dir = self
            .lock()
            .iter()
            .find(|wrapper| wrapper.handle == handle)
            .map(|wrapper| wrapper.run_dir.clone());
        if let Some(run_dir) = run_dir {
            self.files.write(&exit_path(&run_dir), b"")?;
        }
        Ok(None)
    }

    /// The fake knows all process identities, so absence is definitive.
    fn wrapper_exists(&self, handle: &str) -> Result<bool> {
        self.exists(handle)
    }

    /// Whether the process of `handle` lives with the start it names.
    fn exists(&self, handle: &str) -> Result<bool> {
        Ok(BackgroundHandle::parse(handle).is_some_and(|handle| {
            handle.is(
                handle.pid,
                self.processes.start_identity(handle.pid).as_deref(),
            )
        }))
    }
}

/// The production wrapper of run `id` under `token`, as process `pid`, on
/// the queue at `db` as the wrapper writes it.
#[allow(clippy::too_many_arguments)]
fn wrap(
    db: &std::path::Path,
    id: &RunId,
    token: &LeaseToken,
    resume: bool,
    pid: u32,
    agent: &FakeAgent,
    tokens: &MemoryTokens,
    processes: &FakeProcesses,
    files: &MemoryFiles,
    clock: &dyn Clock,
) -> Result<Value> {
    // The wrapper's events are its own, not the worker's (ADR-t728-1).
    let mut queue =
        SqliteQueue::open(db)?.with_actor(ActorContext::instance(ActorRole::Wrapper, id));
    let runtimes = || Ok(Default::default());
    let toolchains = crate::domain::toolchain::Toolchains::none;
    run_session(
        Session {
            queue: &mut queue,
            db,
            provider: agent,
            other: None,
            spawner: agent,
            queue_service: tokens,
            processes,
            files,
            pid,
            start: WrapperStart::Background,
            registration_poll: REGISTRATION_POLL,
            sccache: None,
            clock,
            runtimes: &runtimes,
            toolchains: &toolchains,
            containers: &Nothing,
            telemetry: &Nothing,
        },
        id,
        token,
        resume,
    )
}
