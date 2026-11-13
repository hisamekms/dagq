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
/// named), with scripted exits and logs when supplied; otherwise its log
/// is empty. The repository's `dagq.toml` is the
/// defaults' but for its `[roles.<role>]` and `[worker.trial]`, which the
/// checkout's `dagq.toml` a test sets gives ([`FakeVerifier::set_config`]), and the review's
/// agents, programs, code paths and revise limit, read from the text a
/// review reads off the landing branch.
pub(super) struct FakeVerifier {
    files: std::sync::Arc<MemoryFiles>,
    failing: Vec<String>,
    /// Commands that fail the next time each runs, then pass
    /// ([`FakeVerifier::fail_once`]).
    failing_once: Mutex<Vec<String>>,
    results: Mutex<HashMap<String, std::collections::VecDeque<(i32, String)>>>,
    ran: Mutex<Vec<(String, PathBuf)>>,
    config: Mutex<String>,
    /// Whether `dagq.toml` sets the verification to the container.
    in_container: std::sync::atomic::AtomicBool,
    /// What runs as the command it names starts, once.
    on_command: Mutex<Option<(String, Hook)>>,
    /// Whether the program `[run.env]` names (`RUSTC_WRAPPER`) cannot be
    /// executed; every program resolves by default.
    program_missing: std::sync::atomic::AtomicBool,
}

/// What a fake does once, at a point of the test's choosing.
pub(super) type Hook = Box<dyn FnOnce() + Send>;

impl FakeVerifier {
    /// A verification where `failing` fail and every other command passes.
    pub(super) fn new(files: std::sync::Arc<MemoryFiles>, failing: &[&str]) -> Self {
        Self {
            files,
            failing: failing
                .iter()
                .map(|command| (*command).to_owned())
                .collect(),
            failing_once: Mutex::default(),
            results: Mutex::default(),
            ran: Mutex::default(),
            config: Mutex::default(),
            in_container: std::sync::atomic::AtomicBool::new(false),
            on_command: Mutex::default(),
            program_missing: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(super) fn results(&self, command: &str, results: &[(i32, &str)]) {
        self.results.lock().unwrap().insert(
            command.into(),
            results.iter().map(|(c, s)| (*c, (*s).into())).collect(),
        );
    }

    /// The program `[run.env]` names cannot be executed from now on when
    /// `missing`, wherever it is looked up from (main's checkout or a run's
    /// directory), and resolves again otherwise.
    pub(super) fn set_program_missing(&self, missing: bool) {
        self.program_missing
            .store(missing, std::sync::atomic::Ordering::SeqCst);
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

    /// `dagq.toml` sets the verification to the container (which a test's
    /// host cannot use) when `container`, to the host otherwise.
    pub(super) fn set_in_container(&self, container: bool) {
        self.in_container
            .store(container, std::sync::atomic::Ordering::SeqCst);
    }

    /// `command` fails the next time it runs and passes after, as a
    /// landing's verification that a resumed session's fix makes pass.
    pub(super) fn fail_once(&self, command: &str) {
        self.failing_once
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(command.to_owned());
    }

    /// Run `hook` as `command` starts, before it ends, once.
    pub(super) fn on_command(&self, command: &str, hook: Hook) {
        *self
            .on_command
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((command.to_owned(), hook));
    }

    /// The commands run, each with its directory, in order.
    pub(super) fn ran(&self) -> Vec<(String, PathBuf)> {
        self.ran
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn outcome(&self, command: &str) -> Outcome {
        let mut once = self
            .failing_once
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(at) = once.iter().position(|failing| failing == command) {
            once.remove(at);
            return Outcome::Fails;
        }
        drop(once);
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

    fn eval_config(&self) -> Result<crate::domain::agent_eval::round::EvalConfig> {
        let config = self
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Ok(crate::infrastructure::run_env::parse_config(&config)?.eval)
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

    fn review_code_paths_in(
        &self,
        text: &str,
    ) -> Result<Option<crate::domain::review_reason::CodePaths>> {
        Ok(crate::infrastructure::run_env::parse_config(text)?.review_code)
    }

    fn review_revise_limit_in(&self, text: &str) -> Result<Option<crate::domain::ReviseLimit>> {
        Ok(crate::infrastructure::run_env::parse_config(text)?.review_revise_limit)
    }

    /// No `[run.env]` program to check, or one (`RUSTC_WRAPPER`) that
    /// resolves unless the test made it missing
    /// ([`FakeVerifier::set_program_missing`]).
    fn run_env_programs(&self, _: Option<&Path>) -> Result<RunEnvCheck> {
        if !self
            .program_missing
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(RunEnvCheck::default());
        }
        Ok(RunEnvCheck {
            config: true,
            path: "/wiring/bin".into(),
            programs: vec![crate::domain::run_env::RunEnvProgram {
                variable: "RUSTC_WRAPPER".into(),
                value: "/wiring/bin/sccache".into(),
                resolved: None,
            }],
        })
    }

    fn run_env_table(&self) -> Result<Option<Vec<(String, String)>>> {
        Ok(None)
    }

    fn run_env_salt(&self) -> Result<String> {
        Ok(String::new())
    }

    fn recheck_config(&self) -> Result<crate::domain::recheck::RecheckConfig> {
        Ok(crate::infrastructure::run_env::parse_config(&self.config.lock().unwrap())?.recheck)
    }

    fn toolchains(&self) -> Result<crate::domain::toolchain::Toolchains> {
        Ok(crate::domain::toolchain::Toolchains::this_repository())
    }

    fn runtimes(&self) -> Result<crate::domain::container_runtime::RuntimeConfig> {
        let mut runtimes = crate::domain::container_runtime::RuntimeConfig::default();
        if self.in_container.load(std::sync::atomic::Ordering::SeqCst) {
            runtimes.verification = crate::domain::container_runtime::Runtime::Container;
        }
        Ok(runtimes)
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
        let hook = {
            let mut on = self
                .on_command
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match on.take() {
                Some((named, hook)) if named == command => Some(hook),
                other => {
                    *on = other;
                    None
                }
            }
        };
        if let Some(hook) = hook {
            hook();
        }
        if let Some((code, output)) = self
            .results
            .lock()
            .unwrap()
            .get_mut(command)
            .and_then(|r| r.pop_front())
        {
            self.files.write(log, output.as_bytes())?;
            return Ok(exit(code));
        }
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

/// The verifications' tokens of a test, in memory: each issue a number of
/// its own, its value a file name nowhere on the host.
#[derive(Default)]
pub(super) struct MemoryVerificationTokens {
    issued: Mutex<Vec<(RunId, TokenFor, TokenIssue)>>,
    made: Mutex<u64>,
}

impl MemoryVerificationTokens {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(RunId, TokenFor, TokenIssue)>> {
        self.issued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn take(&self, run: &RunId, of: TokenFor, issue: Option<&TokenIssue>) -> bool {
        let mut issued = self.lock();
        let before = issued.len();
        issued.retain(|(at, kind, made)| {
            !(at == run && *kind == of && issue.is_none_or(|issue| issue == made))
        });
        issued.len() != before
    }
}

impl VerificationTokens for MemoryVerificationTokens {
    fn issue(&self, run: &RunId, of: TokenFor) -> Result<PathBuf> {
        self.take(run, of, None);
        let made = {
            let mut made = self
                .made
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *made += 1;
            *made
        };
        self.lock()
            .push((run.clone(), of, TokenIssue(format!("token {made}"))));
        Ok(PathBuf::from(format!("/wiring/tokens/{run}-{made}")))
    }

    fn revoke(&self, run: &RunId, of: TokenFor) -> Result<bool> {
        Ok(self.take(run, of, None))
    }

    fn issued(&self, run: &RunId, of: TokenFor) -> Result<Option<TokenIssue>> {
        Ok(self
            .lock()
            .iter()
            .find(|(at, kind, _)| at == run && *kind == of)
            .map(|(_, _, issue)| issue.clone()))
    }

    fn revoke_issued(&self, run: &RunId, of: TokenFor, issue: &TokenIssue) -> Result<bool> {
        Ok(self.take(run, of, Some(issue)))
    }

    fn sweep(&self, keep: &mut dyn FnMut(&RunId, TokenFor) -> Result<bool>) -> Result<TokenSweep> {
        let mut sweep = TokenSweep::default();
        let held: Vec<(RunId, TokenFor)> = self
            .lock()
            .iter()
            .map(|(run, of, _)| (run.clone(), *of))
            .collect();
        for (run, of) in held {
            match keep(&run, of) {
                Ok(true) => {}
                Ok(false) => {
                    self.take(&run, of, None);
                    sweep.revoked.push((run, of));
                }
                Err(error) => sweep.failures.push(format!("{error:#}")),
            }
        }
        Ok(sweep)
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

/// Program reviews the fake agent runs ([`super::agent::PROGRAM`]): in the
/// tree they are given, named by their program.
pub(super) struct FakePrograms;

impl ReviewProgramBackend for FakePrograms {
    fn command(
        &self,
        program: &SnapshotProgram,
        worktree: &Path,
        _: &Path,
        _: &Path,
    ) -> Result<CommandSpec> {
        let mut command = CommandSpec::new(super::agent::PROGRAM);
        command.current_dir(worktree).arg(&program.program.name);
        Ok(command)
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

/// Stateful verification port shared by the wiring's supervisor and direct
/// integrate. Disabled by default, preserving held-runtime fixtures.
pub(super) struct FakeVerificationContainers {
    pub enabled: std::sync::atomic::AtomicBool,
    pub state: Mutex<crate::domain::run_container::oom::OomState>,
    pub reads: Mutex<Vec<String>>,
}
impl Default for FakeVerificationContainers {
    fn default() -> Self {
        Self {
            enabled: false.into(),
            state: Mutex::new(crate::domain::run_container::oom::OomState::Absent),
            reads: Mutex::default(),
        }
    }
}
impl ContainerRuntime for FakeVerificationContainers {
    fn availability(&self, target: RuntimeTarget) -> Availability {
        if self.enabled.load(std::sync::atomic::Ordering::SeqCst) {
            Availability::Available
        } else {
            Nothing.availability(target)
        }
    }
}
impl VerificationContainers for FakeVerificationContainers {
    fn prepare(&self, run: &TaskRun, _: &CommitSha, _: &Path) -> Result<PreparedVerification> {
        anyhow::ensure!(
            self.enabled.load(std::sync::atomic::Ordering::SeqCst),
            "no verification container"
        );
        let dir = Path::new("/wiring/verification").join(run.id().as_str());
        Ok(PreparedVerification {
            id: uuid::Uuid::new_v4().to_string(),
            queue: crate::domain::podman_machine::QueueKey::new("bbbbbbbbbbbbbbbb").unwrap(),
            image: "fake".into(),
            staged: dir.join("staged"),
            materials: dir.join("materials"),
            dir,
            build_output: None,
            limits: Default::default(),
        })
    }
    fn publish_materials(&self, _: &PreparedVerification) -> Result<()> {
        Ok(())
    }
    fn command_line(&self, args: &[String]) -> String {
        args.last().unwrap().clone()
    }
    fn remove_container(&self, _: &PreparedVerification) -> Result<()> {
        Ok(())
    }
    fn reserve_memory(
        &self,
        _: &TaskRun,
        _: &PreparedVerification,
        _: &mut dyn FnMut(
            &crate::domain::machine_memory::MemoryAsk,
            crate::domain::machine_memory::MemoryNote<'_>,
        ),
    ) -> Result<()> {
        Ok(())
    }
    fn finish(&self, _: &PreparedVerification) -> Result<()> {
        Ok(())
    }
    fn remove_left(&self, _: &RunId) -> Result<Vec<String>> {
        Ok(vec![])
    }
    fn read_oom(
        &self,
        container: &str,
        _: i64,
        memory_mib: u64,
    ) -> crate::domain::run_container::oom::OomObservation {
        self.reads.lock().unwrap().push(container.into());
        crate::domain::run_container::oom::OomObservation {
            state: self.state.lock().unwrap().clone(),
            container: container.into(),
            memory_mib,
            machine_memory_mib: Some(8192),
        }
    }
}
