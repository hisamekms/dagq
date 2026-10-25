//! The executor of a worker set to the container ([Container runtime],
//! ADR-t2113-3 decision 1, ADR-t2114-2 decision 1): each turn of a run
//! whose worker works in a clone runs in a container of the dagq machine
//! instead of a process of the host ([`PodmanActorExecutor`]), through the
//! [`RunContainers`] port, which makes the machine, the image, the run's
//! relay, its build output and its queue's worker cache ready, hands the run token over as a file
//! of the run directory, starts `podman`, starts the container's sccache
//! server and makes its guard at the supervisor's call when `[run.env]`
//! names sccache
//! ([`ContainerServer`](crate::domain::sccache::ContainerServer)) and
//! removes a run's container.
//!
//! The session wrapper stays on the host (the control side): it reads the
//! turn's output and the run's files as for a turn on the host, and the
//! container's launcher (`dagq relay turn`) writes the run's bundle and
//! state at the turn's end. Whatever stops a run's session stops its
//! container too ([`RunContainers::remove`]).
//!
//! The verification of `integrate` set to the container runs its commands
//! in containers through the [`VerificationContainers`] port.
//!
//! [Container runtime]: ../../docs/design/container-runtime.md

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::json;

use super::actor_executor::{
    ActorExecutionSpec, ActorExecutor, ActorHandle, ActorProgram, EnforcementLevel,
    ExecutorBackend, SessionAgent,
};
use super::queue_service::ServiceAccess;
use super::{AgentProvider, CommandSpec, Spawner, Streams, TurnTarget};
use crate::domain::actor::ACTOR_ENV;
use crate::domain::local_checks;
use crate::domain::podman_machine::QueueKey;
use crate::domain::queue_service::{CLIENT_ENV, CREDENTIAL_FILE_ENV, Principal};
use crate::domain::run_container::{
    AGENT, ContainerLimits, HOME_DIR, WorkerContainer, launcher_command, worker_run_args,
};
use crate::domain::sccache::{
    CONTAINER_GATE_VAR, CONTAINER_TURN_VAR, ContainerGate, ContainerServer, ERROR_LOG_VAR,
    GUARD_PROGRAM_VAR, GateClaim, REFUSED_ERROR_LOG, WRAPPER_VAR, container_gate,
};
use crate::domain::{ActorRole, CommitSha, EventKind, Provider, RunId, TaskRun};

use crate::domain::machine_memory::{MemoryAsk, MemoryNote};

/// The run's event of `note`, told of the wait for `ask`'s memory
/// ([`EventKind::ContainerMemoryWaiting`] and the two after it), with
/// `fields` (the turn's number) besides: the fields of the ask, why it
/// waits, and how long it waited when it ended; `None` for a try it waits
/// on, which is not recorded.
pub fn memory_note_event(
    ask: &MemoryAsk,
    note: MemoryNote<'_>,
    fields: serde_json::Value,
) -> Option<(EventKind, serde_json::Value)> {
    let mut payload = json!({
        "purpose": ask.purpose.as_str(),
        "container": ask.container,
        "memory_mib": ask.memory_mib,
        "since": ask.since,
        "pid": ask.pid,
    });
    let kind = match note {
        MemoryNote::Waiting { why } => {
            payload["why"] = json!(why);
            EventKind::ContainerMemoryWaiting
        }
        MemoryNote::Long { why } => {
            payload["why"] = json!(why);
            EventKind::ContainerMemoryWaitLong
        }
        MemoryNote::Ended {
            outcome,
            waited_secs,
        } => {
            payload["outcome"] = json!(outcome);
            payload["waited_secs"] = json!(waited_secs);
            EventKind::ContainerMemoryWaitEnded
        }
        MemoryNote::Still => return None,
    };
    if let (Some(payload), serde_json::Value::Object(fields)) = (payload.as_object_mut(), fields) {
        payload.extend(fields);
    }
    Some((kind, payload))
}

/// What a turn's container runs with, once [`RunContainers::prepare`] made
/// its parts ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRun {
    /// The queue's name among the machine's containers and volumes.
    pub queue: QueueKey,
    /// The image to run.
    pub image: String,
    /// The directory in the container the run's build output volume is
    /// mounted at (`[container.build_output]` of `dagq.toml`); `None`
    /// mounts none.
    pub build_output: Option<String>,
    /// The directories in the container the queue's worker cache is
    /// mounted at (`[container.worker_cache]` of `dagq.toml`); empty
    /// mounts none.
    pub worker_cache: Vec<String>,
    /// `[run.env]` of the main checkout's `dagq.toml`, for the run.
    pub run_env: Vec<(String, String)>,
    /// The limits of the container (`host.toml`).
    pub limits: ContainerLimits,
}

/// A run's turn container that runs: its name, its ID, which no other
/// container of the run has (each turn's is made anew), and the sccache
/// gate and the turn's token its wrapper gave it ([`CONTAINER_GATE_VAR`],
/// [`CONTAINER_TURN_VAR`]), `None` for a turn whose server is not to be
/// started. All of it is what podman says the container was started with,
/// which nothing in the container changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningContainer {
    pub name: String,
    pub id: String,
    pub gate: Option<String>,
    pub turn: Option<String>,
    /// The container's environment.
    pub env: Vec<(String, String)>,
    /// The directories in the container the queue's worker cache volumes
    /// are mounted at.
    pub worker_cache: Vec<String>,
}

impl RunningContainer {
    /// The plan of the container's sccache server: the one it was started
    /// with ([`ContainerServer::of_container`]), never one `dagq.toml`
    /// makes when the supervisor answers, nor one of the run directory,
    /// which the worker writes.
    pub fn sccache_plan(&self) -> ContainerServer {
        ContainerServer::of_container(&self.env, &self.worker_cache)
    }
}

/// The containers of the runs of one queue in the dagq machine.
pub trait RunContainers: Send + Sync {
    /// Make what a turn of `run` needs ready: the dagq machine running
    /// with the run as a user, the image built, the run's relay open, its
    /// build output volume made (when `dagq.toml` names its directory) and
    /// the queue's worker cache volumes made (one for each directory
    /// `dagq.toml` lists).
    /// `Err` says why the turn cannot run in the container; it is not run
    /// anywhere else (fail closed).
    fn prepare(&self, run: &TaskRun) -> Result<PreparedRun>;
    /// Write the token of `credential` (the worker's token file of the
    /// queue service) to the run directory `run_dir` as
    /// [`TOKEN_FILE`](crate::domain::run_container::TOKEN_FILE) with mode
    /// 0600, following no symlink, and make the agent's home there; the
    /// token file's path.
    fn prepare_run_dir(&self, run_dir: &Path, credential: &Path) -> Result<PathBuf>;
    /// `podman --connection dagq` with `args`.
    fn command(&self, args: &[String]) -> CommandSpec;
    /// Stop and remove the container of the run `run`, and forget the
    /// memory its turn reserved ([`Self::reserve_memory`]); nothing when
    /// there is none.
    fn remove(&self, run: &RunId) -> Result<()>;
    /// Reserve the dagq machine's memory for the container of `run`'s turn
    /// (`prepared`'s limit), waiting while it does not fit (ADR-t3163-1):
    /// after [`Self::prepare`] made the machine run with the run among its
    /// users, before the container starts. `notes` is told of a wait as it
    /// goes ([`MemoryNote`]), for the run's records, and `stop` is read
    /// after each try: why the turn is not to start (its run's stop, a
    /// hold or a replanning), which ends the wait and forgets what it
    /// reserved ([`super::podman_machine::MemoryWaitStopped`]). `Err` says
    /// why the turn cannot run in the container (its limit never fits the
    /// machine, the machine's records cannot be written, it was stopped);
    /// it runs nowhere else.
    fn reserve_memory(
        &self,
        run: &TaskRun,
        prepared: &PreparedRun,
        stop: &dyn Fn() -> Option<String>,
        notes: &mut dyn FnMut(&MemoryAsk, MemoryNote<'_>),
    ) -> Result<()> {
        let _ = (prepared, stop, notes);
        bail!(
            "this process reserves no memory of the dagq machine for run {}",
            run.id()
        )
    }
    /// Forget what [`Self::reserve_memory`] reserved for `run`'s turn, if
    /// anything: a turn whose container did not start.
    fn release_memory(&self, run: &RunId) -> Result<()> {
        let _ = run;
        Ok(())
    }
    /// Record that the container of `run`'s turn was started on its
    /// reservation: from then on the reservation ends with the container
    /// ([`crate::domain::machine_memory::reservation_held`]).
    fn memory_started(&self, run: &RunId) -> Result<()> {
        let _ = run;
        Ok(())
    }
    /// Run a facts-only turn of `run` in its container and wait for its
    /// end: no agent, no run token (a copy left in the run directory is
    /// removed first) and no path to the queue broker (no relay is opened,
    /// only the clone and the run directory are mounted), its executor's
    /// git guarded while it writes the run branch's bundle, the clone's
    /// work and the run's state anew
    /// ([`facts_command`](crate::domain::run_container::facts_command),
    /// ADR-t2978-1).
    /// `Err` says why it did not run or end (the machine or the image
    /// cannot be made ready, a container of the run is there already, it
    /// exited with an error or past its limit); none runs on the host
    /// instead.
    fn facts_turn(&self, run: &TaskRun) -> Result<()>;
    /// Remove the sccache gates and the requests earlier turns left in the
    /// run directory `run_dir` ([`container_gate`] and
    /// [`container_request`](crate::domain::sccache::container_request)),
    /// a link there removed, never followed, and give the token of the turn
    /// about to start, which no earlier turn had ([`CONTAINER_TURN_VAR`]):
    /// what the wrapper does before a turn whose container's server is to
    /// be started.
    fn clear_sccache(&self, run_dir: &Path) -> Result<String> {
        let _ = run_dir;
        Ok("turn".to_owned())
    }
    /// The names of the requests the launchers of turns left in `run_dir`
    /// for their server (regular files, a link not followed), sorted.
    fn sccache_requests(&self, run_dir: &Path) -> Result<Vec<String>> {
        let _ = run_dir;
        Ok(Vec::new())
    }
    /// Remove the requests `names` in `run_dir`, once answered; one not
    /// there is so already.
    fn take_sccache_requests(&self, run_dir: &Path, names: &[String]) -> Result<()> {
        let _ = (run_dir, names);
        Ok(())
    }
    /// The turn container of `run` when it runs; `None` when none runs,
    /// and `Err` when that cannot be told.
    fn running_container(&self, run: &RunId) -> Result<Option<RunningContainer>> {
        let _ = run;
        Ok(None)
    }
    /// Start the sccache server in the running container of the ID
    /// `container` (ADR-t2114-4 decision 1): `podman exec` `program
    /// --start-server` in it with `env` (the server's own environment);
    /// by its ID, so that no later turn's container of the same name is
    /// reached. `Err` says why no server was started.
    fn start_sccache(
        &self,
        container: &str,
        program: &str,
        env: &[(String, String)],
    ) -> Result<()> {
        let _ = (program, env);
        bail!("this process starts no sccache server in the container {container}")
    }
    /// Make the guard of the running container of the ID `container` in
    /// its agent's home under `run_dir`
    /// ([`container_guard`](crate::domain::sccache::container_guard)):
    /// `podman exec` `dagq relay guard` in it, a link to the container's
    /// own `dagq`.
    fn make_sccache_guard(&self, container: &str, run_dir: &Path) -> Result<()> {
        let _ = run_dir;
        bail!("this process makes no sccache guard in the container {container}")
    }
    /// Write `gate` as the sccache gate `name` (a turn's,
    /// [`container_gate`]) of `run_dir` only where none is there, whole (a
    /// file of its own linked to the name), following no link; the text of
    /// the one there first otherwise, read through a descriptor that
    /// follows no link and does not block.
    fn claim_sccache_gate(&self, run_dir: &Path, name: &str, gate: &ContainerGate) -> GateClaim {
        let _ = (run_dir, name, gate);
        GateClaim::Failed("this process writes no sccache gate".to_owned())
    }
    /// The text of the sccache gate `name` in `run_dir`, `None` when there
    /// is none, read as [`Self::claim_sccache_gate`] reads one.
    fn read_sccache_gate(&self, run_dir: &Path, name: &str) -> Result<Option<String>> {
        let _ = (run_dir, name);
        Ok(None)
    }
}

/// One verification of `integrate` in the container, once
/// [`VerificationContainers::prepare`] made its parts ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedVerification {
    /// The verification's ID: its container's and build output's name.
    pub id: String,
    pub queue: QueueKey,
    pub image: String,
    /// The verification's directory: its clone
    /// ([`VERIFICATION_CLONE`](crate::domain::run_container::VERIFICATION_CLONE))
    /// and its token's copy.
    pub dir: PathBuf,
    /// Where the preparation command writes the materials, in the
    /// verification's directory
    /// ([`STAGED_MATERIALS`](crate::domain::run_container::STAGED_MATERIALS)),
    /// made empty.
    pub staged: PathBuf,
    /// The materials' copy in the run directory
    /// ([`MATERIALS_DIR`](crate::domain::run_container::MATERIALS_DIR)),
    /// which [`VerificationContainers::publish_materials`] makes and the
    /// container mounts.
    pub materials: PathBuf,
    /// The directory in the container the verification's build output
    /// volume is mounted at; `None` mounts none.
    pub build_output: Option<String>,
    pub limits: ContainerLimits,
}

/// The containers of the verifications of `integrate` set to the container
/// (ADR-t2113-3 decision 1, ADR-t2114-4 decision 4, ADR-t2173-1 decisions
/// 1 and 4, ADR-t2179-1 decisions 7 and 8): what makes a verification's
/// parts ready, starts its commands' containers and removes what it left.
pub trait VerificationContainers: Send + Sync {
    /// Make the parts of a verification of `run`'s commit `commit` ready:
    /// the dagq machine, the image, the run's relay and the verification's
    /// own build output volume (when `dagq.toml` names its directory); a
    /// new directory of its own under the queue's verification directory,
    /// holding a clone of `commit` the control side makes with its git
    /// guarded, a copy of the verification's token `token` and an empty
    /// directory for the materials. A path of the verification's
    /// directory that is there already, or a parent that is a symlink or no
    /// directory, is refused. `Err` says why no command runs; nothing runs
    /// on the host instead.
    fn prepare(
        &self,
        run: &TaskRun,
        commit: &CommitSha,
        token: &Path,
    ) -> Result<PreparedVerification>;
    /// Copy the staged materials of `verification` into the run
    /// directory's materials, made anew: through the run directory's
    /// descriptors, removing what is there first and following no symlink
    /// the worker put there, so no file outside the run directory is
    /// written.
    fn publish_materials(&self, verification: &PreparedVerification) -> Result<()>;
    /// The shell's command line of `podman --connection dagq` with `args`.
    fn command_line(&self, args: &[String]) -> String;
    /// Stop and remove the container of `verification`, if it is there.
    fn remove_container(&self, verification: &PreparedVerification) -> Result<()>;
    /// Reserve the dagq machine's memory for `verification` of `run`
    /// (its limit), waiting in the host-wide order while it does not fit
    /// (ADR-t3163-1 decision 4): before its first command, held through
    /// every command to [`Self::finish`]. `notes` is told of a wait as it
    /// goes. `Err` says why no command runs (its limit never fits, the
    /// machine's records cannot be written); none runs on the host
    /// instead.
    fn reserve_memory(
        &self,
        run: &TaskRun,
        verification: &PreparedVerification,
        notes: &mut dyn FnMut(&MemoryAsk, MemoryNote<'_>),
    ) -> Result<()> {
        let _ = (verification, notes);
        bail!(
            "this process reserves no memory of the dagq machine for the verification of run {}",
            run.id()
        )
    }
    /// Remove what `verification` left: its container, its build output
    /// volume and its directory, without git and following no symlink, and
    /// forget the memory it reserved.
    fn finish(&self, verification: &PreparedVerification) -> Result<()>;
    /// Remove, as [`Self::finish`] does, what the verifications of `run`
    /// an earlier process made left (its supervisor died while they ran):
    /// those whose directory is still in the queue's verification
    /// directory. The IDs of those removed.
    fn remove_left(&self, run: &RunId) -> Result<Vec<String>>;
}

/// The verification containers of a queue that has none: every verification
/// set to the container is refused.
pub struct NoVerificationContainers;

impl VerificationContainers for NoVerificationContainers {
    fn prepare(&self, _: &TaskRun, _: &CommitSha, _: &Path) -> Result<PreparedVerification> {
        bail!("this process runs no verification in a container")
    }
    fn publish_materials(&self, _: &PreparedVerification) -> Result<()> {
        bail!("this process runs no verification in a container")
    }
    fn command_line(&self, _: &[String]) -> String {
        String::new()
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

/// The shell's command line that runs `command` of `run` with `env` in the
/// container of `verification`: `dagq relay verify` in a container
/// [`verification_run_args`](crate::domain::run_container::verification_run_args)
/// hardens, its environment naming the materials, the run and the
/// verification's home besides `env`. What `integrate`'s verification and
/// the landing recheck of a run in a container run through `podman` on
/// the host.
pub fn verification_line(
    containers: &dyn VerificationContainers,
    verification: &PreparedVerification,
    run: &RunId,
    command: &str,
    env: &[(String, String)],
) -> Result<String> {
    use crate::domain::run_container::{
        MATERIALS_ENV, VerificationContainer, verification_command, verification_run_args,
    };
    let mut env = verification_env(env);
    env.extend([
        (
            MATERIALS_ENV.to_owned(),
            super::path_text(&verification.materials)?,
        ),
        (
            crate::domain::actor::RUN_ID_ENV.to_owned(),
            run.as_str().to_owned(),
        ),
        (
            "HOME".to_owned(),
            super::path_text(&verification.dir.join(HOME_DIR))?,
        ),
    ]);
    let command = verification_command(&verification.dir, command);
    let args = verification_run_args(&VerificationContainer {
        queue: &verification.queue,
        run: run.as_str(),
        verification: &verification.id,
        image: &verification.image,
        dir: &verification.dir,
        materials: &verification.materials,
        build_output: verification.build_output.as_deref(),
        limits: verification.limits,
        env: &env,
        command: &command,
    })
    .map_err(|why| anyhow!("the verification's container cannot be made: {why}"))?;
    Ok(containers.command_line(&args))
}

/// `env` without what names a worker's local checks' directory: a
/// verification's results are the runtime's own record, and it is never
/// told where the worker's are (ADR-t2589-1 decision 2).
fn verification_env(env: &[(String, String)]) -> Vec<(String, String)> {
    env.iter()
        .filter(|(key, _)| key != local_checks::DIR_ENV)
        .cloned()
        .collect()
}

/// Starts a worker's turns in their container. Only a Claude worker's
/// turn of a run in a clone is started: any other program, role or
/// provider is refused, and never started on the host instead.
pub struct PodmanActorExecutor<'a> {
    queue: &'a Path,
    provider: &'a dyn AgentProvider,
    spawner: &'a dyn Spawner,
    service: &'a dyn ServiceAccess,
    containers: &'a dyn RunContainers,
    sccache: Option<SccacheRecords<'a>>,
    memory: Option<MemoryRecords<'a>>,
}

/// Where the executor records the wait of a turn's container for the dagq
/// machine's memory, and the turn it is of.
#[derive(Clone, Copy)]
pub struct MemoryRecords<'a> {
    pub events: &'a dyn super::actor_executor::ExecutorEvents,
    pub turn: u64,
    /// What shows the turn's wrapper lives while it waits: its heartbeat.
    pub beat: &'a dyn Fn(),
    /// Why the turn is not to start after all, read after each try at the
    /// memory, the one that reserved it included: its run's exit request,
    /// a hold or a replanning that fences its task.
    pub stop: &'a dyn Fn() -> Option<String>,
}

/// Forgets the memory reserved for a run's turn whose container did not
/// start, unless it started.
struct Reserved<'a> {
    containers: &'a dyn RunContainers,
    run: &'a RunId,
    started: bool,
}

impl Drop for Reserved<'_> {
    fn drop(&mut self) {
        if !self.started
            && let Err(error) = self.containers.release_memory(self.run)
        {
            tracing::warn!(run_id = %self.run, error = %format_args!("{error:#}"), "the memory reserved for run {}'s container could not be released: {error:#}", self.run);
        }
    }
}

/// Where the executor records an agent's run without `RUSTC_WRAPPER`, and
/// what the record says of the turn.
#[derive(Clone, Copy)]
pub struct SccacheRecords<'a> {
    pub events: &'a dyn super::actor_executor::ExecutorEvents,
    /// When the turn starts, in Unix seconds.
    pub at: i64,
    pub turn: u64,
}

impl<'a> PodmanActorExecutor<'a> {
    /// The executor of the queue at `queue`, whose agent's command
    /// `provider` makes and whose `podman` `spawner` starts.
    pub fn new(
        queue: &'a Path,
        provider: &'a dyn AgentProvider,
        spawner: &'a dyn Spawner,
        service: &'a dyn ServiceAccess,
        containers: &'a dyn RunContainers,
    ) -> Self {
        Self {
            queue,
            provider,
            spawner,
            service,
            containers,
            sccache: None,
            memory: None,
        }
    }

    /// Record the wait of the turn's container for the dagq machine's
    /// memory in `records`; without, it is only logged.
    pub fn with_memory_records(mut self, records: MemoryRecords<'a>) -> Self {
        self.memory = Some(records);
        self
    }

    fn memory_note(&self, run: &RunId, ask: &MemoryAsk, note: MemoryNote<'_>) {
        if note != MemoryNote::Still {
            tracing::info!(run_id = %run, "run {run}: its turn's container and the dagq machine's memory: {note:?}");
        }
        let Some(records) = &self.memory else {
            return;
        };
        let Some((kind, payload)) = memory_note_event(ask, note, json!({"turn": records.turn}))
        else {
            (records.beat)();
            return;
        };
        if let Err(error) = records.events.record_agent_event(Some(run), kind, payload) {
            tracing::warn!(run_id = %run, error = %format_args!("{error:#}"), "the wait of run {run}'s container for the machine's memory could not be recorded: {error:#}");
        }
    }

    /// Record the agent's run without `RUSTC_WRAPPER` of a turn whose
    /// `[run.env]` keeps the container's server from starting in
    /// `records`; without, it is only logged.
    pub fn with_sccache_records(mut self, records: SccacheRecords<'a>) -> Self {
        self.sccache = Some(records);
        self
    }

    fn wrapper_removed(&self, run: &RunId, port: u16, why: &str) {
        match &self.sccache {
            Some(records) => super::sccache::record_wrapper_removed(
                records.events,
                run,
                records.at,
                "wrapper",
                port,
                why,
                json!({"turn": records.turn}),
            ),
            None => {
                tracing::warn!(run_id = %run, "run {run}: its turn's container runs without {WRAPPER_VAR}: {why}")
            }
        }
    }
}

impl ActorExecutor for PodmanActorExecutor<'_> {
    fn backend(&self) -> ExecutorBackend {
        ExecutorBackend::Container
    }

    fn enforcement(&self) -> EnforcementLevel {
        EnforcementLevel::Sandbox
    }

    fn spawn(&self, spec: ActorExecutionSpec<'_>) -> Result<ActorHandle> {
        spec.check()?;
        ensure!(
            spec.role() == ActorRole::Worker,
            "only a worker's turn runs in the container, not {}",
            spec.role().as_str()
        );
        // The queue broker has no destination of OpenAI's (ADR-t2114-1
        // decision 2).
        ensure!(
            spec.program.provider() == Provider::Claude,
            "a Codex worker does not run in the container: the queue broker has no destination of its provider"
        );
        let ActorExecutionSpec { actor, program, .. } = spec;
        let ActorProgram::SessionAgent {
            agent:
                SessionAgent::Turn {
                    run,
                    prompt,
                    session,
                    stdout,
                    stderr,
                    ..
                },
            model,
        } = program
        else {
            bail!("only a worker's turn runs in the container");
        };
        ensure!(
            run.in_container(),
            "run {} works in a worktree of the host, not a clone: its turn does not run in the container",
            run.id()
        );
        let run_dir = Path::new(run.run_dir().context("missing run directory")?);
        let clone = Path::new(run.worktree_path().context("missing clone")?);
        let branch = run.branch().context("missing run branch")?;
        let target = TurnTarget::of_run(run)?;
        let mut agent = self.provider.turn_command(&target, prompt, session)?;
        if let Some((model, effort)) = model {
            self.provider.select_model(&mut agent, model, effort);
        }
        // The container's failures are this executor's, never the
        // provider's: no cause of theirs is kept as an `io::Error`, which
        // the wrapper would take for an agent that cannot be run.
        let prepared = self
            .containers
            .prepare(run)
            .map_err(|error| anyhow!("make the run's container ready: {error:#}"))?;
        // Before the container starts, outside the turn's time: the turn is
        // recorded as started once its container is (ADR-t3163-1 decision
        // 4).
        let go_on = || None;
        let stop = self
            .memory
            .as_ref()
            .map_or(&go_on as &dyn Fn() -> Option<String>, |records| {
                records.stop
            });
        self.containers
            .reserve_memory(run, &prepared, stop, &mut |ask, note| {
                self.memory_note(run.id(), ask, note);
            })
            .map_err(|error| {
                match error.downcast::<super::podman_machine::MemoryWaitStopped>() {
                    Ok(stopped) => anyhow::Error::new(stopped),
                    Err(error) => {
                        anyhow!(
                            "reserve the dagq machine's memory for the run's container: {error:#}"
                        )
                    }
                }
            })?;
        let mut reserved = Reserved {
            containers: self.containers,
            run: run.id(),
            started: false,
        };
        // The token the supervisor issued at the claim or the resume; one
        // is issued only when there is none.
        let credential = match self.service.credential(self.queue, actor.actor_id()) {
            Some(file) => file,
            None => self.service.issue(self.queue, &Principal::of(&actor))?,
        };
        let token = self
            .containers
            .prepare_run_dir(run_dir, &credential)
            .map_err(|error| anyhow!("hand the run token to the container: {error:#}"))?;
        let mut plan = ContainerServer::of(&prepared.run_env, &prepared.worker_cache);
        // A gate or request an earlier turn left is not this turn's.
        let mut turn = String::new();
        if let ContainerServer::Start { target, .. } = &plan {
            match self.containers.clear_sccache(run_dir) {
                Ok(token) => turn = token,
                Err(error) => {
                    plan = ContainerServer::Refused {
                        target: target.clone(),
                        why: format!(
                            "the sccache gate an earlier turn left could not be removed: {error:#}"
                        ),
                    };
                }
            }
        }
        let mut env = turn_variables(&prepared.run_env, &agent, &plan);
        env.extend(sccache_variables(&plan, run_dir, &turn)?);
        env.extend(actor.env());
        env.push(super::actor_executor::local_checks_env(run)?);
        env.push((
            CREDENTIAL_FILE_ENV.to_owned(),
            token
                .to_str()
                .context("the token's path is not UTF-8")?
                .to_owned(),
        ));
        env.push((
            "HOME".to_owned(),
            run_dir
                .join(HOME_DIR)
                .to_str()
                .context("the run directory is not UTF-8")?
                .to_owned(),
        ));
        let mut agent_args = vec![AGENT.to_owned()];
        for arg in agent.get_args() {
            agent_args.push(
                arg.to_str()
                    .context("an argument of the agent is not UTF-8")?
                    .to_owned(),
            );
        }
        let command = launcher_command(
            run_dir,
            clone,
            run.base_commit().as_str(),
            branch,
            &agent_args,
        );
        let args = worker_run_args(&WorkerContainer {
            queue: &prepared.queue,
            run: run.id().as_str(),
            image: &prepared.image,
            clone,
            run_dir,
            build_output: prepared.build_output.as_deref(),
            worker_cache: &prepared.worker_cache,
            limits: prepared.limits,
            env: &env,
            unset_env: unset_variables(&plan),
            command: &command,
        })
        .map_err(|why| anyhow!("the run's container cannot be made: {why}"))?;
        let mut podman = self.containers.command(&args);
        // Never in the clone: the control side starts no program there
        // (ADR-t2179-1 decision 1).
        podman.current_dir(run_dir).new_session();
        // The agent's prompt is its standard input, never an argument: the
        // container's launcher hands podman's on to the agent.
        if let Some(prompt) = agent.get_stdin() {
            podman.stdin(prompt);
        }
        // A podman that cannot be started is this executor's failure, not
        // the provider's (whose agent never started on the host).
        let child = self
            .spawner
            .spawn(&podman, Streams::Files { stdout, stderr })
            .map_err(|error| anyhow!("start the run's container with podman: {error:#}"))?;
        // Held by the container now: the turn's end removes both.
        reserved.started = true;
        if let Err(error) = self.containers.memory_started(run.id()) {
            tracing::warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "the start of run {}'s container could not be recorded with its memory: {error:#}", run.id());
        }
        // A server to start is the supervisor's, at the launcher's request
        // (ADR-t2114-4 decision 1).
        if let ContainerServer::Refused { target, why } = &plan {
            self.wrapper_removed(run.id(), target.port, why);
        }
        Ok(ActorHandle::Process(child))
    }
}

/// Answer the request the launcher of `run`'s turn container left for its
/// sccache server, as the supervisor that leases `run` does (ADR-t2114-4
/// decision 1), recording on `queue` at `at` (Unix seconds).
///
/// Only the request of a running container its wrapper gave this run's
/// gate of its turn, both named by that turn's token ([`container_gate`],
/// [`container_request`]), is served, and once: one already recorded (by
/// its ID) is not started again, whatever the worker leaves in its run
/// directory. With no gate there, it starts the server and makes the guard
/// in the container by its ID with the plan the container was started with
/// ([`RunningContainer::sccache_plan`]: its port, its cache directory in
/// its worker cache mounts or refused), writes its verdict as that turn's gate
/// where none is while the same container still runs, and records the
/// start or its failure and, as [`removed_after`] says of the gate that is
/// the turn's, the agent's run without `RUSTC_WRAPPER`. A gate there that
/// nothing records yet (the launcher's timed out one, or one an earlier
/// process wrote) is recorded as it says. Once no container runs, only the
/// launcher's timed out gate of a turn that ended is left to record, once
/// by the gate's own token and of one turn a pass; while one runs, another
/// turn's request is only taken away. The requests listed before the
/// container is looked at are taken
/// away once answered, and no other. What it writes and takes is the
/// answered turn's alone, so a next turn's container that replaces it at
/// any point (its gate and request named by its own token) is that turn's
/// to answer. An error leaves the requests for the next pass.
pub fn serve_sccache_request<Q: super::RunLog + ?Sized>(
    containers: &dyn RunContainers,
    queue: &Q,
    run: &TaskRun,
    at: i64,
) -> Result<()> {
    use crate::domain::sccache::{
        DEFAULT_PORT, RequestStep, container_request, container_verdict, removed_after,
        request_turn,
    };
    let run_dir = Path::new(run.run_dir().context("missing run directory")?);
    // Listed before the container is looked at: each is of the container
    // found then or of a turn that ended before it.
    let requests = containers.sccache_requests(run_dir)?;
    let take = || containers.take_sccache_requests(run_dir, &requests);
    let events = queue.run_events(run.id())?;
    let recorded = |key: &str, value: &str| {
        events.iter().any(|event| {
            [
                EventKind::SccacheContainerServerStarted,
                EventKind::SccacheContainerServerStartFailed,
                EventKind::SccacheWrapperRemoved,
            ]
            .iter()
            .any(|kind| event.kind == *kind)
                && event.payload[key] == value
        })
    };
    // The port is the one the turn's container was started with; once it
    // ended, nothing trusted says it, and none is recorded.
    let removed = |port: Option<u16>, why: &str, mut fields: serde_json::Value| {
        if port.is_none() {
            fields["port"] = serde_json::Value::Null;
        }
        super::sccache::record_wrapper_removed(
            queue,
            run.id(),
            at,
            "supervisor",
            port.unwrap_or(DEFAULT_PORT),
            why,
            fields,
        );
    };
    let running = containers.running_container(run.id())?;
    // The running container's turn and its gate, when its wrapper asked for
    // a server; a request in any other is the worker's.
    let own = match &running {
        Some(RunningContainer {
            gate: Some(path),
            turn: Some(turn),
            ..
        }) => match container_gate(turn) {
            Some(gate) if *path == super::path_text(&run_dir.join(&gate))? => {
                Some((turn.as_str(), gate))
            }
            _ => None,
        },
        _ => None,
    };
    let Some(container) = &running else {
        // The turn ended: what its launcher wrote alone is left to record,
        // once by the gate's own token, and of one turn a pass (no other
        // turn's container can write in the run directory once the turn's
        // ended).
        for turn in requests.iter().filter_map(|name| request_turn(name)) {
            let Some(gate) = container_gate(turn) else {
                continue;
            };
            if let Some(text) = containers.read_sccache_gate(run_dir, &gate)?
                && let Some(ContainerGate::TimedOut(why)) = ContainerGate::parse(&text)
                && !recorded("gate", &text)
            {
                removed(None, &why, json!({"container_id": null, "gate": text}));
                break;
            }
        }
        return take();
    };
    let Some((turn, gate)) = own else {
        return take();
    };
    // One served already is served; its own request not left yet is
    // answered once it is.
    if recorded("container_id", &container.id)
        || !container_request(turn).is_some_and(|own| requests.contains(&own))
    {
        return take();
    }
    let same = || -> Result<bool> {
        Ok(containers
            .running_container(run.id())?
            .is_some_and(|now| now.id == container.id))
    };
    let fields = json!({"container_id": container.id});
    // The plan the container was started with, whatever `dagq.toml` says
    // by now (ADR-t2114-4 decisions 1 and 3).
    let plan = container.sccache_plan();
    let port = match &plan {
        ContainerServer::Start { target, .. } | ContainerServer::Refused { target, .. } => {
            target.port
        }
        ContainerServer::NotNamed => DEFAULT_PORT,
    };
    match RequestStep::of(containers.read_sccache_gate(run_dir, &gate)?.as_deref()) {
        RequestStep::Wait => return Ok(()),
        RequestStep::Record(found) => {
            let mut fields = fields.clone();
            fields["gate"] = json!(found.text());
            match &found {
                ContainerGate::Started => super::sccache::record_container_server(
                    queue,
                    run.id(),
                    at,
                    port,
                    &container.name,
                    Ok(()),
                    json!({"container_id": container.id, "by": "supervisor"}),
                ),
                ContainerGate::Failed(why) | ContainerGate::TimedOut(why) => {
                    removed(Some(port), why, fields);
                }
            }
        }
        RequestStep::Start => {
            let (started, guard) = match plan {
                ContainerServer::Start { target, env } => {
                    let started = containers
                        .start_sccache(&container.id, &target.program, &env)
                        .map_err(|error| format!("{error:#}"));
                    let guard = match &started {
                        Ok(()) => containers
                            .make_sccache_guard(&container.id, run_dir)
                            .map_err(|error| format!("{error:#}")),
                        Err(_) => Ok(()),
                    };
                    (started, guard)
                }
                ContainerServer::Refused { why, .. } => (Err(why), Ok(())),
                ContainerServer::NotNamed => (
                    Err("the container was started naming no sccache to start".to_owned()),
                    Ok(()),
                ),
            };
            // A container that ended meanwhile has no launcher left to
            // answer. One that ends after this look is answered in its own
            // turn's gate, which no later turn's launcher reads.
            if !same()? {
                return take();
            }
            let verdict = container_verdict(&started, &guard);
            let claim = containers.claim_sccache_gate(run_dir, &gate, &verdict);
            super::sccache::record_container_server(
                queue,
                run.id(),
                at,
                port,
                &container.name,
                started,
                json!({"container_id": container.id, "by": "supervisor"}),
            );
            if let Some(why) = removed_after(&verdict, &claim) {
                let mut fields = fields;
                // The launcher's gate is recorded by its own token, as
                // when no answer comes before its container ends.
                if let GateClaim::Lost(text) = &claim {
                    fields["gate"] = json!(text);
                }
                removed(Some(port), &why, fields);
            }
        }
    }
    take()
}

/// The variables a turn's container is given beside the actor's and the
/// token file's: `[run.env]`, then what the agent's command sets; none
/// naming the actor, the queue service or the home, which the executor
/// sets itself, nor Claude Code's configuration directory, which stays the
/// home's for the control side to find the sessions there
/// ([`crate::domain::run_container::CLAUDE_DIR`]). A `[run.env]` naming
/// sccache (`plan` is not [`ContainerServer::NotNamed`]) is given without
/// `RUSTC_WRAPPER` and `SCCACHE_ERROR_LOG`, which [`sccache_variables`] and
/// the launcher decide (ADR-t2114-4 decisions 2 and 3); any other is given
/// as it is.
fn turn_variables(
    run_env: &[(String, String)],
    agent: &CommandSpec,
    plan: &ContainerServer,
) -> Vec<(String, String)> {
    let sccache = !matches!(plan, ContainerServer::NotNamed);
    let reserved = |key: &str| {
        (sccache && (key == WRAPPER_VAR || key == ERROR_LOG_VAR))
            || ACTOR_ENV.contains(&key)
            || CLIENT_ENV.contains(&key)
            || key == "HOME"
            || key == "CLAUDE_CONFIG_DIR"
            || key == local_checks::DIR_ENV
    };
    let from_agent = agent
        .get_envs()
        .filter_map(|(key, value)| Some((key.to_str()?.to_owned(), value?.to_str()?.to_owned())));
    run_env
        .iter()
        .cloned()
        .chain(from_agent)
        .filter(|(key, _)| !reserved(key))
        .collect()
}

/// The variables a turn's container is started without, whatever its
/// image sets: for a `plan` the supervisor handles (a server to start or
/// one refused), `RUSTC_WRAPPER`, which the launcher gives the agent only
/// as the guard behind a started gate (ADR-t2114-4 decisions 2 and 3), so
/// no wrapper the image names reaches the agent when no server was
/// started or no guard made; for a `[run.env]` naming no sccache, none.
fn unset_variables(plan: &ContainerServer) -> &'static [&'static str] {
    match plan {
        ContainerServer::NotNamed => &[],
        ContainerServer::Start { .. } | ContainerServer::Refused { .. } => &[WRAPPER_VAR],
    }
}

/// The variables a turn's container is given about sccache by `plan`
/// (ADR-t2114-4 decisions 1 to 3): for a server the supervisor starts,
/// the sccache the guard compiles through, the token `turn` and the gate
/// file in `run_dir` it names, which the launcher waits for before the
/// agent and then gives the agent the refused `SCCACHE_ERROR_LOG` and,
/// behind a started gate, the guard; for one refused,
/// the refused `SCCACHE_ERROR_LOG` alone (no `RUSTC_WRAPPER`); for none,
/// nothing.
fn sccache_variables(
    plan: &ContainerServer,
    run_dir: &Path,
    turn: &str,
) -> Result<Vec<(String, String)>> {
    Ok(match plan {
        ContainerServer::NotNamed => Vec::new(),
        ContainerServer::Start { target, .. } => vec![
            (GUARD_PROGRAM_VAR.to_owned(), target.program.clone()),
            (
                CONTAINER_GATE_VAR.to_owned(),
                super::path_text(&run_dir.join(
                    container_gate(turn).context("the turn's token names no sccache gate")?,
                ))?,
            ),
            (CONTAINER_TURN_VAR.to_owned(), turn.to_owned()),
        ],
        ContainerServer::Refused { .. } => {
            vec![(ERROR_LOG_VAR.to_owned(), REFUSED_ERROR_LOG.to_owned())]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verification_is_not_told_the_local_checks_directory() {
        let env = vec![
            ("CARGO_TARGET_DIR".to_owned(), "/build".to_owned()),
            (
                "DAGQ_LOCAL_CHECKS_DIR".to_owned(),
                "/q/runs/r1/local-checks".to_owned(),
            ),
        ];
        assert_eq!(verification_env(&env), env[..1]);
    }

    #[test]
    fn a_turn_s_variables_leave_the_sccache_wrapper_and_the_actor_s_names_out() {
        let run_env = vec![
            ("RUSTC_WRAPPER".to_owned(), "sccache".to_owned()),
            ("SCCACHE_ERROR_LOG".to_owned(), "/tmp/log".to_owned()),
            ("CARGO_TARGET_DIR".to_owned(), "/build".to_owned()),
            ("DAGQ_RUN_ID".to_owned(), "other".to_owned()),
            ("DAGQ_SERVICE_SOCKET".to_owned(), "/x".to_owned()),
            ("DAGQ_LOCAL_CHECKS_DIR".to_owned(), "/elsewhere".to_owned()),
            ("HOME".to_owned(), "/Users/me".to_owned()),
            (
                "CLAUDE_CONFIG_DIR".to_owned(),
                "/Users/me/.claude".to_owned(),
            ),
        ];
        let mut agent = CommandSpec::new("claude");
        agent.env("CLAUDE_X", "1").env_remove("GONE");
        let plan = ContainerServer::of(&run_env, &[]);
        assert!(matches!(plan, ContainerServer::Refused { .. }), "{plan:?}");
        assert_eq!(
            turn_variables(&run_env, &agent, &plan),
            [
                ("CARGO_TARGET_DIR".to_owned(), "/build".to_owned()),
                ("CLAUDE_X".to_owned(), "1".to_owned()),
            ]
        );
    }

    /// A `[run.env]` naming no sccache is given as it is, its own wrapper
    /// and log too, and nothing about sccache is added or unset (ADR-t2257-1
    /// decision 5); one naming sccache whose server is started is told the
    /// gate and the sccache, one refused only the refused log, and both
    /// are started without the image's wrapper.
    #[test]
    fn only_a_run_env_naming_sccache_has_its_wrapper_handled() {
        let run_env = vec![
            ("RUSTC_WRAPPER".to_owned(), "cachepot".to_owned()),
            ("SCCACHE_ERROR_LOG".to_owned(), "/tmp/log".to_owned()),
        ];
        let plan = ContainerServer::of(&run_env, &["/cache".to_owned()]);
        assert_eq!(plan, ContainerServer::NotNamed);
        let agent = CommandSpec::new("claude");
        assert_eq!(turn_variables(&run_env, &agent, &plan), run_env);
        let run_dir = Path::new("/q/runs/r1");
        assert!(sccache_variables(&plan, run_dir, "t1").unwrap().is_empty());
        assert!(unset_variables(&plan).is_empty());

        let named = vec![
            ("RUSTC_WRAPPER".to_owned(), "/usr/bin/sccache".to_owned()),
            ("SCCACHE_DIR".to_owned(), "/cache/sccache".to_owned()),
        ];
        let plan = ContainerServer::of(&named, &["/cache".to_owned()]);
        assert_eq!(
            sccache_variables(&plan, run_dir, "t1").unwrap(),
            [
                (
                    "DAGQ_SCCACHE_PROGRAM".to_owned(),
                    "/usr/bin/sccache".to_owned()
                ),
                (
                    "DAGQ_SCCACHE_GATE".to_owned(),
                    "/q/runs/r1/sccache-gate.t1".to_owned()
                ),
                ("DAGQ_SCCACHE_TURN".to_owned(), "t1".to_owned()),
            ]
        );
        // The image's wrapper never reaches the agent of a turn the
        // supervisor handles, a server started or refused.
        assert_eq!(unset_variables(&plan), ["RUSTC_WRAPPER"]);
        let refused = ContainerServer::of(&named, &[]);
        assert_eq!(unset_variables(&refused), ["RUSTC_WRAPPER"]);
        assert_eq!(
            sccache_variables(&refused, run_dir, "t1").unwrap(),
            [("SCCACHE_ERROR_LOG".to_owned(), REFUSED_ERROR_LOG.to_owned())]
        );
    }
}
