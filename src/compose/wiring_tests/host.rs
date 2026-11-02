//! The host of a wiring test: its processes, the verification's commands,
//! and the ports of what it has none of (containers, telemetry sinks,
//! program reviews), each a fake that answers in memory.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Result, bail};

use crate::{
    application::{
        Clock, ContainerRuntime, Exit, ProcessControl, ReviewProgramBackend, RunFiles,
        TelemetrySink, TelemetrySinks, Verifier,
        memory_files::MemoryFiles,
        queue_broker::{TokenFor, TokenIssue, TokenSweep, VerificationTokens},
        review_programs::SnapshotProgram,
        run_container::{PreparedRun, PreparedVerification, RunContainers, VerificationContainers},
    },
    domain::{
        CommitSha, RunId, TaskRun, container_runtime::Availability,
        container_runtime::RuntimeTarget, run_env::RunEnvCheck, session_step::StepTarget,
    },
};

use crate::application::CommandSpec;

/// The processes of a test: the ones it started, each with the start the
/// system would show for it, alive until it ends one. No signal reaches
/// any process of the host. A signal only takes its pid off the living:
/// the thread a wrapper runs on ends at its exit request or its lost
/// lease, not at a signal.
#[derive(Default)]
pub(super) struct FakeProcesses {
    live: Mutex<HashMap<u32, String>>,
    /// The pid the next process gets.
    next: Mutex<u32>,
}

impl FakeProcesses {
    /// The processes of a test, `pid` (the supervisor's) alive among them.
    pub(super) fn with(pid: u32) -> Self {
        let processes = Self::default();
        processes.lock().insert(pid, format!("start of {pid}"));
        // Far from any pid of the host, which none of these is.
        *processes.next.lock().unwrap() = 4_000_000;
        processes
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u32, String>> {
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A process started now: its pid and its start.
    pub(super) fn start(&self) -> (u32, String) {
        let pid = {
            let mut next = self
                .next
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *next += 1;
            *next
        };
        let start = format!("start of {pid}");
        self.lock().insert(pid, start.clone());
        (pid, start)
    }

    /// Process `pid` ended.
    pub(super) fn end(&self, pid: u32) {
        self.lock().remove(&pid);
    }
}

impl ProcessControl for FakeProcesses {
    fn alive(&self, pid: u32) -> bool {
        self.lock().contains_key(&pid)
    }

    fn terminate(&self, pid: u32) -> Result<()> {
        self.end(pid);
        Ok(())
    }

    fn interrupt(&self, pid: u32) -> Result<()> {
        self.end(pid);
        Ok(())
    }

    fn kill(&self, pid: u32) -> Result<()> {
        self.end(pid);
        Ok(())
    }

    fn kill_group(&self, leader: u32) -> Result<()> {
        self.end(leader);
        Ok(())
    }

    /// None of a test's processes has a child.
    fn list(&self) -> Result<Vec<crate::domain::recovery::ProcessInfo>> {
        Ok(Vec::new())
    }

    fn start_identity(&self, pid: u32) -> Option<String> {
        self.lock().get(&pid).cloned()
    }

    fn session_start(&self, pid: u32) -> Option<String> {
        self.start_identity(pid)
    }
}

/// How one verification command ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    Passes,
    Fails,
}

/// The verification of a test: each command it is asked to run is
/// recorded with where it ran and ends as the test says (passing unless
/// named), its log written empty; the repository's `dagq.toml` is the
/// defaults' but for its `[roles.<role>]` and `[worker.trial]`, which the
/// checkout's `dagq.toml` a test sets gives ([`FakeVerifier::set_config`]), and the review's
/// agents and programs, read from the text a review reads off the landing
/// branch.
pub(super) struct FakeVerifier {
    files: std::sync::Arc<MemoryFiles>,
    failing: Vec<String>,
    ran: Mutex<Vec<(String, PathBuf)>>,
    config: Mutex<String>,
}

impl FakeVerifier {
    /// A verification where `failing` fail and every other command passes.
    pub(super) fn new(files: std::sync::Arc<MemoryFiles>, failing: &[&str]) -> Self {
        Self {
            files,
            failing: failing
                .iter()
                .map(|command| (*command).to_owned())
                .collect(),
            ran: Mutex::default(),
            config: Mutex::default(),
        }
    }

    /// The checkout's `dagq.toml` is `text` from now on.
    pub(super) fn set_config(&self, text: &str) {
        text.clone_into(
            &mut self
                .config
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }

    /// The commands run, each with its directory, in order.
    pub(super) fn ran(&self) -> Vec<(String, PathBuf)> {
        self.ran
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn outcome(&self, command: &str) -> Outcome {
        if self.failing.iter().any(|failing| failing == command) {
            Outcome::Fails
        } else {
            Outcome::Passes
        }
    }
}

/// The exit of a process that ended with `code`.
pub(super) fn exit(code: i32) -> Exit {
    Exit {
        success: code == 0,
        code: Some(code),
        signal: None,
        description: format!("exit status: {code}"),
    }
}

impl Verifier for FakeVerifier {
    fn run_env(&self, _: &Path) -> Result<Vec<(String, String)>> {
        Ok(Vec::new())
    }

    fn role_models(&self) -> Result<crate::domain::actor_model::RoleModels> {
        let config = self
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Ok(crate::infrastructure::run_env::parse_config(&config)?.roles)
    }

    fn worker_trial(&self) -> Result<crate::domain::worker_model::WorkerTrial> {
        let config = self
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Ok(crate::infrastructure::run_env::parse_config(&config)?.worker_trial)
    }

    fn review_subagents_in(
        &self,
        text: &str,
    ) -> Result<crate::domain::review_subagents::ReviewAgents> {
        Ok(crate::infrastructure::run_env::parse_config(text)?.review_agents())
    }

    fn review_programs_in(
        &self,
        text: &str,
    ) -> Result<crate::domain::review_programs::ReviewPrograms> {
        Ok(crate::infrastructure::run_env::parse_config(text)?.review_programs())
    }

    fn run_env_programs(&self, _: Option<&Path>) -> Result<RunEnvCheck> {
        Ok(RunEnvCheck::default())
    }

    fn run_env_table(&self) -> Result<Option<Vec<(String, String)>>> {
        Ok(None)
    }

    fn run_env_salt(&self) -> Result<String> {
        Ok(String::new())
    }

    fn run_to_log(
        &self,
        command: &str,
        cwd: &Path,
        _: &[(String, String)],
        log: &Path,
    ) -> Result<Exit> {
        self.ran
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((command.to_owned(), cwd.to_owned()));
        self.files.write(log, b"")?;
        Ok(match self.outcome(command) {
            Outcome::Passes => exit(0),
            Outcome::Fails => exit(1),
        })
    }
}

/// The host of a test has no container runtime, no container, no telemetry
/// sink and no program review: a target set to a container waits, a stop
/// removes nothing, and nothing is sent.
pub(super) struct Nothing;

impl ContainerRuntime for Nothing {
    fn availability(&self, _: RuntimeTarget) -> Availability {
        Availability::Unavailable("a wiring test has no container runtime".to_owned())
    }
}

impl RunContainers for Nothing {
    fn prepare(&self, run: &TaskRun) -> Result<PreparedRun> {
        bail!(
            "a wiring test runs no turn of run {} in a container",
            run.id()
        )
    }

    fn prepare_run_dir(&self, run_dir: &Path, _: &Path) -> Result<PathBuf> {
        bail!(
            "a wiring test prepares no container for {}",
            run_dir.display()
        )
    }

    fn command(&self, args: &[String]) -> CommandSpec {
        let mut command = CommandSpec::new("wiring-no-podman");
        command.args(args);
        command
    }

    fn remove(&self, _: &RunId) -> Result<()> {
        Ok(())
    }

    fn facts_turn(&self, run: &TaskRun) -> Result<()> {
        bail!(
            "a wiring test runs no facts-only turn of run {} in a container",
            run.id()
        )
    }
}

impl VerificationContainers for Nothing {
    fn prepare(&self, run: &TaskRun, _: &CommitSha, _: &Path) -> Result<PreparedVerification> {
        bail!("a wiring test verifies run {} in no container", run.id())
    }

    fn publish_materials(&self, _: &PreparedVerification) -> Result<()> {
        bail!("a wiring test verifies in no container")
    }

    fn command_line(&self, args: &[String]) -> String {
        args.join(" ")
    }

    fn remove_container(&self, _: &PreparedVerification) -> Result<()> {
        Ok(())
    }

    fn finish(&self, _: &PreparedVerification) -> Result<()> {
        Ok(())
    }

    fn remove_left(&self, _: &RunId) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

/// None issued: a verification in a container is the only holder.
impl VerificationTokens for Nothing {
    fn issue(&self, run: &RunId, _: TokenFor) -> Result<PathBuf> {
        bail!("a wiring test verifies run {run} in no container")
    }

    fn revoke(&self, _: &RunId, _: TokenFor) -> Result<bool> {
        Ok(false)
    }

    fn issued(&self, _: &RunId, _: TokenFor) -> Result<Option<TokenIssue>> {
        Ok(None)
    }

    fn revoke_issued(&self, _: &RunId, _: TokenFor, _: &TokenIssue) -> Result<bool> {
        Ok(false)
    }

    fn sweep(&self, _: &mut dyn FnMut(&RunId, TokenFor) -> Result<bool>) -> Result<TokenSweep> {
        Ok(TokenSweep::default())
    }
}

impl TelemetrySinks for Nothing {
    fn sink(&self, _: &Path, _: &StepTarget) -> Option<Box<dyn TelemetrySink>> {
        None
    }

    fn digest_key(&self, _: &Path) -> Option<crate::domain::command_shape::DigestKey> {
        None
    }

    fn steps_file(&self, _: &Path, _: &StepTarget) -> Option<PathBuf> {
        None
    }
}

impl ReviewProgramBackend for Nothing {
    fn command(
        &self,
        program: &SnapshotProgram,
        _: &Path,
        _: &Path,
        _: &Path,
    ) -> Result<CommandSpec> {
        bail!(
            "a wiring test runs no program review ({})",
            program.program.name
        )
    }
}

/// A clock that does not move: what the queue records and the supervisor
/// reads as now stays the same, so no lease nor timeout of the test runs
/// out while the production's own waits (its ticks, the watch of a job)
/// pass in real time.
pub(super) struct StoppedClock {
    at: SystemTime,
    monotonic: Instant,
}

impl StoppedClock {
    /// Stopped at `at`, its monotonic clock at `monotonic`.
    pub(super) fn new(at: SystemTime, monotonic: Instant) -> Self {
        Self { at, monotonic }
    }
}

impl Clock for StoppedClock {
    fn system_time(&self) -> SystemTime {
        self.at
    }

    fn monotonic(&self) -> Instant {
        self.monotonic
    }
}

/// The unix second a test's clock stops at: 2026-11-01T00:00:00Z.
pub(super) const STOPPED_AT: Duration = Duration::from_secs(1_793_491_200);
