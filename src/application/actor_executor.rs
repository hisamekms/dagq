//! The one way the runtime starts an AI actor (goal 55, ADR-t728-1): an
//! [`ActorExecutor`] takes an [`ActorExecutionSpec`] (the actor with its
//! role, run and task, the capabilities its role is granted, the workspace
//! it may touch, its resource limits and what to start) and starts it. The
//! worker and its resume, every planner, the inbox, the review, recovery,
//! plan review and goal review jobs and the observer are all started here,
//! and the environment ([`actor_env`]) of each role is made in this one
//! place. What a role's agent is given beyond it (Claude Code's settings,
//! hooks and `permissions.deny`, or none) is the provider's implementation's
//! to decide (ADR-t1063-1 decision 3): a headless job names only its intent
//! ([`JobAccess`]).
//!
//! [`HostActorExecutor`] is the only backend: it starts the actor as a
//! process of this user on this host, through the [`AgentProvider`] (Claude
//! Code) and the cmux [`WorkspaceBackend`]. On a host the spec is advisory
//! (ADR-t728-1 decision 6): the capabilities, the workspace access and the
//! limits are recorded and checked for consistency, but nothing isolates the
//! process, which can reach whatever this user can. A sandboxed backend
//! (goal 38) would enforce the same spec. Nothing here names such a
//! backend's details.
//!
//! What starts outside the executor is not an AI actor: the supervisor's own
//! workspace (`up --in-cmux`), the session wrappers (which then start their
//! agent here), the `observe` command the supervisor runs (whose agent
//! starts here), and the stub agents of the tests.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use tracing::warn;

use super::queue_service::ServiceAccess;
use super::{
    AgentProvider, CommandSpec, PlannerCommand, Spawned, Spawner, Streams, TurnTarget,
    WorkspaceBackend, WorkspaceTags,
    lifecycle::{PLANNER_ID_ENV, PLANNER_ORIGIN_ENV, QUEUE_ENV, SESSION_KIND_ENV},
    naming::shell_join,
    path_text,
};
use crate::domain::{
    ActorContext, ActorRole, PlannerId, PlannerOrigin, RunId, TaskId, TaskRun, TrustLevel,
    actor::{ACTOR_ENV, ACTOR_ID_ENV, ROLE_ENV, RUN_ID_ENV, TASK_ID_ENV},
    actor_model::ActorLaunch,
    authorization::{Capability, grants},
    headless_job::JobAccess,
    queue_service::{CLIENT_ENV, CREDENTIAL_FILE_ENV, Principal, SOCKET_ENV, client_role},
    sessions::{self, LAUNCH_ENV},
};

pub use super::execution::{EnforcementLevel, ExecutionConfig, ExecutorBackend};

/// The part of the file system an actor works on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceAccess {
    /// It edits this checkout or worktree: a worker in its run's worktree,
    /// the inbox and a planner in the repository's checkout.
    Write(PathBuf),
    /// It only reads this checkout or worktree: the review in the run's
    /// worktree (its tools are read-only), the plan and goal reviews in the
    /// checkout.
    Read(PathBuf),
    /// It works in a directory of its own and reads the queue through the
    /// CLI: the recovery job and the observer.
    Scratch(PathBuf),
}

impl WorkspaceAccess {
    pub fn path(&self) -> &Path {
        match self {
            Self::Write(path) | Self::Read(path) | Self::Scratch(path) => path,
        }
    }
}

/// What an actor may use. On a host the caller's own loop keeps it (the
/// supervisor's timers, the observer's deadline); a sandbox would hold it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceLimits {
    /// How long it may run before it is given up; `None` for a session a
    /// person or the supervisor ends.
    pub timeout: Option<Duration>,
}

/// The agent a session wrapper starts in its workspace.
pub enum SessionAgent<'a> {
    /// A run's worker with its prompt.
    Worker { run: &'a TaskRun, prompt: &'a str },
    /// The same session reopened for a `needs_session` run (ADR-0019).
    Resume { run: &'a TaskRun },
    /// One turn of a headless worker (ADR-t813-1): `prompt` starting a
    /// session or going on with one, as `session` says, its output to
    /// `stdout` and `stderr` rather than the wrapper's terminal, without
    /// the variables `without_env` names (`RUSTC_WRAPPER` for a sandboxed
    /// turn whose sccache server was not confirmed, ADR-t1215-1).
    Turn {
        run: &'a TaskRun,
        prompt: &'a str,
        session: crate::domain::turn::TurnSession<'a>,
        stdout: &'a Path,
        stderr: &'a Path,
        without_env: &'a [&'a str],
    },
    /// A planner (ADR-0041).
    Planner(PlannerCommand<'a>),
    /// One turn of a headless planner of the runtime's (ADR-t1394-2
    /// decision 2), in `target` (the planner's directory and checkout), as
    /// a worker's [`SessionAgent::Turn`] is in its run.
    PlannerTurn {
        target: TurnTarget<'a>,
        prompt: &'a str,
        session: crate::domain::turn::TurnSession<'a>,
        stdout: &'a Path,
        stderr: &'a Path,
    },
}

/// What a workspace not tied to a run runs.
pub enum WorkspaceCommand<'a> {
    /// A session wrapper (a planner's `planner-session`), which starts its
    /// agent through the executor in turn.
    Wrapper(String),
    /// The agent itself with `prompt` as its first message, loading
    /// `plugin_dir`: the inbox.
    Agent {
        prompt: String,
        plugin_dir: Option<&'a Path>,
    },
}

/// What a headless job runs.
pub enum HeadlessProgram<'a> {
    /// The review of an accepted run (ADR-0027), allowed what `access`
    /// says.
    Review {
        run: &'a TaskRun,
        prompt: &'a str,
        access: JobAccess,
        /// The review's required subagents (ADR-t1453-1), which the
        /// provider hands its job; none for a review that requires none,
        /// whose command is as before.
        subagents: &'a [crate::domain::review_subagents::AgentDefinition],
    },
    /// A job in `cwd` allowed what `access` says beyond what needs no
    /// permission (ADR-t1063-1 decision 2): the recovery job, the plan and
    /// goal reviews, the throughput review, the observer. The provider
    /// turns the intent into its own mechanism.
    Job {
        cwd: &'a Path,
        prompt: &'a str,
        access: JobAccess,
    },
}

/// What to start for the actor.
pub enum ActorProgram<'a> {
    /// The session wrapper `wrapper` of a run's worker (or of its resume
    /// or reopening), started without a workspace as a process detached
    /// from the supervisor, in the run's worktree, with the repository's
    /// `[run.env]` after the runtime's own names and its output in `log`
    /// (ADR-t1404-1 decisions 1 and 5, ADR-t1433-3 decision 1); its handle
    /// is what the run records as its session.
    RunSession {
        run: &'a TaskRun,
        wrapper: String,
        run_env: Vec<(String, String)>,
        log: &'a Path,
    },
    /// A workspace of its own: the inbox, a planner. `planner` names the
    /// planner and its origin, `launch` the model its agent starts with.
    NamedWorkspace {
        name: &'a str,
        cwd: &'a Path,
        command: WorkspaceCommand<'a>,
        planner: Option<(PlannerOrigin, PlannerId)>,
        launch: Option<&'a ActorLaunch>,
        description: Option<String>,
        group: Option<String>,
        /// The log a headless planner's wrapper writes to when it is
        /// started without a workspace (ADR-t1404-1 decision 8): a process
        /// detached from the supervisor with the workspace's environment,
        /// whose handle is the workspace's ID.
        background: Option<&'a Path>,
    },
    /// The agent of a session wrapper, with the terminal of its workspace,
    /// and the model and effort it was opened with.
    SessionAgent {
        agent: SessionAgent<'a>,
        model: Option<(&'a str, &'a str)>,
    },
    /// A headless job: its session id when known ahead (ADR-0048 decision
    /// 4), its model, whether it loads no MCP server, the environment
    /// beside the actor's (the repository's `[run.env]`, the observer's
    /// `PATH`), the variables it does not inherit (`RUSTC_WRAPPER` for a
    /// sandboxed job whose sccache server was not confirmed, ADR-t1215-1)
    /// and where its output goes.
    Headless {
        program: HeadlessProgram<'a>,
        session_id: Option<&'a str>,
        launch: Option<&'a ActorLaunch>,
        without_mcp: bool,
        env: Vec<(String, String)>,
        without_env: &'a [&'a str],
        streams: Streams<'a>,
    },
}

impl ActorProgram<'_> {
    fn provider(&self) -> crate::domain::Provider {
        use crate::domain::Provider;
        match self {
            Self::RunSession { run, .. } => run.actual_provider(),
            Self::SessionAgent { agent, .. } => match agent {
                SessionAgent::Worker { run, .. }
                | SessionAgent::Resume { run }
                | SessionAgent::Turn { run, .. } => run.actual_provider(),
                SessionAgent::Planner(_) | SessionAgent::PlannerTurn { .. } => Provider::Claude,
            },
            Self::NamedWorkspace { launch, .. } | Self::Headless { launch, .. } => {
                launch.map_or(Provider::Claude, |launch| launch.provider)
            }
        }
    }

    /// The roles this program is started for: any other is refused.
    fn roles(&self) -> &'static [ActorRole] {
        match self {
            Self::RunSession { .. }
            | Self::SessionAgent {
                agent:
                    SessionAgent::Worker { .. }
                    | SessionAgent::Resume { .. }
                    | SessionAgent::Turn { .. },
                ..
            } => &[ActorRole::Worker],
            Self::SessionAgent {
                agent: SessionAgent::Planner(_) | SessionAgent::PlannerTurn { .. },
                ..
            }
            | Self::NamedWorkspace {
                command: WorkspaceCommand::Wrapper(_),
                ..
            } => &[ActorRole::Planner],
            Self::NamedWorkspace {
                command: WorkspaceCommand::Agent { .. },
                ..
            } => &[ActorRole::Inbox],
            Self::Headless {
                program: HeadlessProgram::Review { .. },
                ..
            } => &[ActorRole::ReviewJob],
            Self::Headless {
                program: HeadlessProgram::Job { .. },
                ..
            } => &[
                ActorRole::RecoveryJob,
                ActorRole::PlanReviewJob,
                ActorRole::GoalReviewJob,
                ActorRole::ThroughputReviewJob,
                ActorRole::Observer,
            ],
        }
    }

    fn run(&self) -> Option<&TaskRun> {
        match self {
            Self::RunSession { run, .. }
            | Self::SessionAgent {
                agent:
                    SessionAgent::Worker { run, .. }
                    | SessionAgent::Resume { run }
                    | SessionAgent::Turn { run, .. },
                ..
            }
            | Self::Headless {
                program: HeadlessProgram::Review { run, .. },
                ..
            } => Some(run),
            _ => None,
        }
    }
}

/// An AI actor to start: who it is, what its role may do, where it works,
/// what it may use and what to start.
pub struct ActorExecutionSpec<'a> {
    pub actor: ActorContext,
    /// What the role is granted ([`grants`]): recorded with the start, and
    /// what a sandboxed backend would hold it to.
    pub capabilities: &'static [Capability],
    pub workspace: WorkspaceAccess,
    pub resources: ResourceLimits,
    pub program: ActorProgram<'a>,
}

impl<'a> ActorExecutionSpec<'a> {
    /// `actor` starting `program` in `workspace`, with its role's grants
    /// and no limit.
    pub fn new(actor: ActorContext, workspace: WorkspaceAccess, program: ActorProgram<'a>) -> Self {
        Self {
            capabilities: grants(actor.role()),
            actor,
            workspace,
            resources: ResourceLimits::default(),
            program,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.resources.timeout = Some(timeout);
        self
    }

    pub fn role(&self) -> ActorRole {
        self.actor.role()
    }

    /// The run it acts on: the actor's, or the one its program is of.
    pub fn run_id(&self) -> Option<&RunId> {
        self.actor
            .run_id()
            .or_else(|| self.program.run().map(TaskRun::id))
    }

    pub fn task_id(&self) -> Option<TaskId> {
        self.actor
            .task_id()
            .or_else(|| self.program.run().map(TaskRun::task_id))
    }

    /// Refuse a spec that does not hold together (fail closed): an actor
    /// that is not an AI actor, a program not of its role, capabilities
    /// other than its role's, or a worker acting on another run than the
    /// one it is started for.
    pub fn check(&self) -> Result<()> {
        let role = self.role();
        ensure!(
            role.trust() == TrustLevel::UntrustedAgent,
            "{} is not an AI actor the executor starts",
            role.as_str()
        );
        ensure!(
            self.program.roles().contains(&role),
            "{} cannot be started as this program",
            role.as_str()
        );
        ensure!(
            self.capabilities == grants(role),
            "the capabilities of {} are not its role's",
            self.actor.actor_id()
        );
        if let (Some(own), Some(run)) = (self.actor.run_id(), self.program.run()) {
            ensure!(
                own == run.id(),
                "{} is started for run {}",
                self.actor.actor_id(),
                run.id()
            );
        }
        Ok(())
    }
}

/// What the executor started.
pub enum ActorHandle {
    /// The stable ID of the workspace it runs in.
    Workspace(String),
    /// The process it is.
    Process(Box<dyn Spawned>),
}

impl ActorHandle {
    pub fn workspace(self) -> Result<String> {
        match self {
            Self::Workspace(id) => Ok(id),
            Self::Process(_) => bail!("the actor was started as a process, not in a workspace"),
        }
    }

    pub fn process(self) -> Result<Box<dyn Spawned>> {
        match self {
            Self::Process(process) => Ok(process),
            Self::Workspace(_) => bail!("the actor was started in a workspace, not as a process"),
        }
    }
}

/// Starts AI actors. Every start goes through [`ActorExecutionSpec::check`]
/// first.
pub trait ActorExecutor {
    fn backend(&self) -> ExecutorBackend;
    fn enforcement(&self) -> EnforcementLevel;
    fn spawn(&self, spec: ActorExecutionSpec<'_>) -> Result<ActorHandle>;
}

/// The environment of `actor` on the queue at `queue` (ADR-0026,
/// ADR-t728-1 decision 4): its role, the queue (not for a worker or a job,
/// whose `dagq` runs in client mode: [`client_role`]), its id, its run and task,
/// the kind of span a session's hook records (the inbox's, a planner's by
/// its origin), a planner's origin and id, and the model it starts with.
/// Every workspace and headless job of an actor gets exactly this, and the
/// in-cmux supervisor's workspace the same shape.
pub fn actor_env(
    queue: &Path,
    actor: &ActorContext,
    planner: Option<(PlannerOrigin, PlannerId)>,
    launch: Option<&ActorLaunch>,
) -> Result<Vec<(String, String)>> {
    let mut env = Vec::new();
    if let Some(role) = actor.role().env_value() {
        env.push((ROLE_ENV.to_owned(), role.to_owned()));
    }
    // A worker or a job is not given the queue's path: its `dagq` goes to
    // the queue service ([`client_env`], goal 82's stage (3)).
    if !client_role(actor.role()) {
        env.push((QUEUE_ENV.to_owned(), path_text(queue)?));
    }
    env.push((ACTOR_ID_ENV.to_owned(), actor.actor_id().to_owned()));
    if let Some(run) = actor.run_id() {
        env.push((RUN_ID_ENV.to_owned(), run.to_string()));
    }
    if let Some(task) = actor.task_id() {
        env.push((TASK_ID_ENV.to_owned(), task.to_string()));
    }
    let kind = match actor.role() {
        ActorRole::Inbox => Some(sessions::INBOX),
        ActorRole::Planner => Some(match planner {
            Some((PlannerOrigin::Runtime, _)) => sessions::RUNTIME_PLANNER,
            _ => sessions::PLANNER,
        }),
        _ => None,
    };
    if let Some(kind) = kind {
        env.push((SESSION_KIND_ENV.to_owned(), kind.to_owned()));
    }
    if let Some((origin, id)) = planner {
        env.push((PLANNER_ORIGIN_ENV.to_owned(), origin.as_str().to_owned()));
        env.push((PLANNER_ID_ENV.to_owned(), id.to_string()));
    }
    if let Some(launch) = launch {
        env.push((LAUNCH_ENV.to_owned(), launch.to_value().to_string()));
    }
    Ok(env)
}

/// Starts AI actors on this host, as processes of this user (advisory):
/// workspaces through cmux, agents through the provider and the spawner.
/// Each part is given where the caller has it, and a program that needs a
/// part the executor was not given is refused.
pub struct HostActorExecutor<'a> {
    queue: &'a Path,
    workspaces: Option<&'a dyn WorkspaceBackend>,
    provider: Option<&'a dyn AgentProvider>,
    spawner: Option<&'a dyn Spawner>,
    service: Option<&'a dyn ServiceAccess>,
    config: ExecutionConfig,
    no_claude: bool,
}

impl<'a> HostActorExecutor<'a> {
    /// An executor for the actors of the queue at `queue`, with no part yet
    /// and every actor on the host.
    pub fn new(queue: &'a Path) -> Self {
        Self {
            queue,
            workspaces: None,
            provider: None,
            spawner: None,
            service: None,
            config: ExecutionConfig::default(),
            no_claude: false,
        }
    }

    /// Refuse Claude before opening a workspace or constructing an agent command.
    pub fn with_no_claude(mut self, no_claude: bool) -> Self {
        self.no_claude = no_claude;
        self
    }

    /// The backend of each actor as `config` names it: an actor it puts on
    /// another backend is refused.
    pub fn with_config(mut self, config: ExecutionConfig) -> Self {
        self.config = config;
        self
    }

    /// Workspaces open through `workspaces`.
    pub fn with_workspaces(mut self, workspaces: &'a dyn WorkspaceBackend) -> Self {
        self.workspaces = Some(workspaces);
        self
    }

    /// Agents are made by `provider`.
    pub fn with_provider(mut self, provider: &'a dyn AgentProvider) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Processes start through `spawner`.
    pub fn with_spawner(mut self, spawner: &'a dyn Spawner) -> Self {
        self.spawner = Some(spawner);
        self
    }

    /// The tokens and the socket of the queue service, which a worker and
    /// a job are given instead of the queue's path (goal 82's stage (3)).
    pub fn with_queue_service(mut self, service: &'a dyn ServiceAccess) -> Self {
        self.service = Some(service);
        self
    }

    /// The broker's client a worker is given (ADR-t827-1 decisions 5 and
    /// 7): the `dagq-broker-client` next to `dagq` (this process's own
    /// binary), and only when `version` reads dagq's build from it. A
    /// missing client or one of another build is a structured
    /// [`super::broker::BrokerFailure`] (`client_missing`,
    /// `version_mismatch`), and the worker gets no broker tools.
    pub fn broker_client(
        &self,
        dagq: &Path,
        version: &dyn Fn(&Path) -> std::result::Result<String, String>,
    ) -> super::broker::BrokerResult<std::path::PathBuf> {
        super::broker::resolve_client(dagq, crate::VERSION, version)
    }

    fn workspaces(&self) -> Result<&'a dyn WorkspaceBackend> {
        self.workspaces.context("this executor opens no workspace")
    }

    fn provider(&self) -> Result<&'a dyn AgentProvider> {
        self.provider.context("this executor has no agent provider")
    }

    fn spawner(&self) -> Result<&'a dyn Spawner> {
        self.spawner.context("this executor starts no process")
    }

    fn service(&self) -> Result<&'a dyn ServiceAccess> {
        self.service.context(
            "this executor has no queue service to give the actor (fail closed: it is never \
             given the queue's path instead)",
        )
    }

    /// What the agent of `actor`, a [`client_role`], is given for its
    /// `dagq` to run in client mode (ADR-t1233-1 decision 7, ADR-t1233-4
    /// decision 4): the service's socket and the file of its token. `issue`
    /// issues a new token (a job's, at its start); otherwise the one the
    /// supervisor issued at the claim or the resume is handed over, and
    /// one is issued only when there is none (a wrapper started without
    /// the claim's). The socket is returned for the provider to let its
    /// sandbox reach it.
    fn client_env(
        &self,
        actor: &ActorContext,
        issue: bool,
    ) -> Result<(PathBuf, Vec<(String, String)>)> {
        let service = self.service()?;
        let existing = if issue {
            None
        } else {
            service.credential(self.queue, actor.actor_id())
        };
        let credential = match existing {
            Some(file) => file,
            None => service.issue(self.queue, &Principal::of(actor))?,
        };
        let socket = service.socket(self.queue);
        let env = vec![
            (SOCKET_ENV.to_owned(), path_text(&socket)?),
            (CREDENTIAL_FILE_ENV.to_owned(), path_text(&credential)?),
        ];
        Ok((socket, env))
    }
}

/// A create cmux reported failed may have made the workspace all the same
/// (a create that timed out while cmux went on, task 806). Nothing records
/// its UUID and its wrapper is refused, so it would be left open: the
/// workspaces listed with `description`, which names only this planner's
/// workspace, are closed, and the create's `error` says what
/// became of them. A listing that fails leaves the error as it was; the
/// wrapper closes its own workspace when refused.
fn close_unrecorded(
    cmux: &dyn WorkspaceBackend,
    description: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    let found = match cmux.workspaces_described(description) {
        Ok(found) => found,
        Err(list) => {
            warn!(description, error = %format_args!("{list:#}"), "workspaces described {description:?} could not be listed after a failed create: {list:#}");
            return error;
        }
    };
    if found.is_empty() {
        return error;
    }
    let closed: Vec<String> = found
        .into_iter()
        .map(|id| match cmux.close(&id) {
            Ok(()) => {
                warn!(workspace_id = %id, description, "closed workspace {id} cmux made although its create failed");
                format!("{id} was closed")
            }
            Err(close) => format!("{id} could not be closed: {close:#}"),
        })
        .collect();
    error.context(format!(
        "cmux made the workspace described {description:?} although the create failed; {}",
        closed.join(", ")
    ))
}

impl ActorExecutor for HostActorExecutor<'_> {
    fn backend(&self) -> ExecutorBackend {
        ExecutorBackend::Host
    }

    fn enforcement(&self) -> EnforcementLevel {
        EnforcementLevel::Advisory
    }

    fn spawn(&self, spec: ActorExecutionSpec<'_>) -> Result<ActorHandle> {
        spec.check()?;
        anyhow::ensure!(
            !self.no_claude || spec.program.provider() != crate::domain::Provider::Claude,
            "provider_disabled: Claude is disabled by --no-claude; handle this role manually"
        );
        // An actor configured for another backend is refused, never started
        // on the host instead (fail closed).
        let backend = self.config.backend_of(spec.role());
        if backend != self.backend() {
            backend.ensure_implemented()?;
            bail!(
                "{} is configured for the {} backend, not {}",
                spec.role().as_str(),
                backend.as_str(),
                self.backend().as_str()
            );
        }
        let ActorExecutionSpec {
            actor,
            program,
            workspace,
            ..
        } = spec;
        match program {
            ActorProgram::RunSession {
                run: _,
                wrapper,
                run_env,
                log,
            } => {
                let cmux = self.workspaces()?;
                // The worker's token, issued at the claim and the resume
                // (ADR-t1233-4 decision 4); the wrapper hands its file to
                // the agent. The wrapper itself opens the queue: it gets
                // neither the token nor the socket.
                self.service()?
                    .issue(self.queue, &Principal::of(&actor))
                    .context("issue the worker's token for the queue service")?;
                let mut env = actor_env(self.queue, &actor, None, None)?;
                env.extend(run_env);
                // In the run's worktree, which the worker writes.
                Ok(ActorHandle::Workspace(cmux.launch_background(
                    workspace.path(),
                    &wrapper,
                    &env,
                    log,
                )?))
            }
            ActorProgram::NamedWorkspace {
                name,
                cwd,
                command,
                planner,
                launch,
                description,
                group,
                background,
            } => {
                let command = match command {
                    WorkspaceCommand::Wrapper(command) => command,
                    WorkspaceCommand::Agent { prompt, plugin_dir } => {
                        command_line(&self.provider()?.inbox_command(
                            &prompt,
                            plugin_dir,
                            self.queue.parent().unwrap_or(Path::new(".")),
                        )?)?
                    }
                };
                let env = actor_env(self.queue, &actor, planner, launch)?;
                let cmux = self.workspaces()?;
                if let Some(log) = background {
                    return Ok(ActorHandle::Workspace(
                        cmux.launch_background(cwd, &command, &env, log)?,
                    ));
                }
                let tags = WorkspaceTags {
                    env,
                    description,
                    group,
                };
                let id = cmux
                    .create_named(name, cwd, &command, &tags)
                    .map_err(|error| match (planner, &tags.description) {
                        // Only a planner's description names one workspace
                        // (`planner=<id>`); the inbox's is the queue's.
                        (Some(_), Some(description)) => close_unrecorded(cmux, description, error),
                        _ => error,
                    })?;
                Ok(ActorHandle::Workspace(id))
            }
            ActorProgram::SessionAgent { agent, model } => {
                let provider = self.provider()?;
                let mut streams = Streams::Inherit;
                let mut without: &[&str] = &[];
                // The resource broker's tools, as the supervisor left them
                // in the run's dir (ADR-t827-4 decision 1, ADR-t838-1): a
                // `required` run without them is refused here, before any
                // command of the agent is made.
                let broker = match &agent {
                    SessionAgent::Worker { run, .. }
                    | SessionAgent::Resume { run }
                    | SessionAgent::Turn { run, .. } => super::broker_run::worker_broker(run)?,
                    SessionAgent::Planner(_) | SessionAgent::PlannerTurn { .. } => {
                        super::broker_run::WorkerBroker::None
                    }
                };
                let (mut command, worker) = match agent {
                    SessionAgent::Worker { run, .. } | SessionAgent::Resume { run }
                        if matches!(broker, super::broker_run::WorkerBroker::Required(_)) =>
                    {
                        return Err(super::broker_run::BrokerRequiredRefused(format!(
                            "[broker] mode = \"required\" runs run {}'s worker in headless turns \
only; an interactive session has no settings that refuse the built-in tools",
                            run.id()
                        ))
                        .into());
                    }
                    SessionAgent::Worker { run, prompt } => {
                        (provider.command(run, prompt)?, Some(run))
                    }
                    SessionAgent::Resume { run } => (provider.resume_command(run)?, Some(run)),
                    SessionAgent::Turn {
                        run,
                        prompt,
                        session,
                        stdout,
                        stderr,
                        without_env,
                    } => {
                        streams = Streams::Files { stdout, stderr };
                        without = without_env;
                        let mut target = TurnTarget::of_run(run)?;
                        if let super::broker_run::WorkerBroker::Required(config) = &broker {
                            target.broker_required = Some(config);
                        }
                        (provider.turn_command(&target, prompt, session)?, Some(run))
                    }
                    SessionAgent::Planner(planner) => (provider.planner_command(&planner)?, None),
                    SessionAgent::PlannerTurn {
                        target,
                        prompt,
                        session,
                        stdout,
                        stderr,
                    } => {
                        streams = Streams::Files { stdout, stderr };
                        (provider.turn_command(&target, prompt, session)?, None)
                    }
                };
                // With `preferred`, the tools beside the built-in ones when
                // the supervisor issued the run's token; a run without them
                // starts as before. `required`'s are the turn's own.
                if let (Some(_), super::broker_run::WorkerBroker::Preferred(config)) =
                    (worker, &broker)
                {
                    provider.broker_tools(&mut command, config);
                }
                if let Some((model, effort)) = model {
                    provider.select_model(&mut command, model, effort);
                }
                // The workspace's environment already makes the wrapper's
                // children this actor; set again, the agent is it whatever
                // the wrapper inherited.
                command.envs(actor.env());
                if client_role(actor.role()) {
                    // The worker's `dagq` goes to the queue service, never
                    // to the queue's path, which a workspace an older
                    // binary opened may still name (goal 82's stage (3)).
                    let (socket, env) = self.client_env(&actor, false)?;
                    command.env_remove(QUEUE_ENV).envs(env);
                    provider.reach_queue_service(&mut command, &socket);
                } else {
                    for name in CLIENT_ENV {
                        command.env_remove(name);
                    }
                }
                for name in without {
                    command.env_remove(name);
                }
                let child = self
                    .spawner()?
                    .spawn(&command, streams)
                    .context("launch agent")?;
                Ok(ActorHandle::Process(child))
            }
            ActorProgram::Headless {
                program,
                session_id,
                launch,
                without_mcp,
                env,
                without_env,
                streams,
            } => {
                let provider = self.provider()?;
                let spawner = self.spawner()?;
                let mut command = match program {
                    HeadlessProgram::Review {
                        run,
                        prompt,
                        access,
                        subagents,
                    } => {
                        let mut command = provider.review_command(run, prompt, access)?;
                        if !subagents.is_empty() {
                            provider.review_subagents(&mut command, subagents)?;
                        }
                        command
                    }
                    HeadlessProgram::Job {
                        cwd,
                        prompt,
                        access,
                    } => provider.headless_command(cwd, prompt, access)?,
                };
                if let Some(session_id) = session_id {
                    provider.assign_session_id(&mut command, session_id);
                }
                if let Some(launch) = launch {
                    provider.apply_launch(&mut command, launch);
                }
                if without_mcp {
                    provider.without_mcp(&mut command);
                }
                // The caller's variables first, then the actor's, which
                // nothing the repository sets replaces. The actor variables
                // the job's actor does not set are not inherited from the
                // starter (a worker's or a planner's session), so the job
                // is not taken for its run, task or planner (task 902).
                let mut actor_env = actor_env(self.queue, &actor, None, None)?;
                for name in ACTOR_ENV {
                    if !actor_env.iter().any(|(key, _)| key == name) {
                        command.env_remove(name);
                    }
                }
                // A job's `dagq` goes to the queue service with a token of
                // its own, which ends with it (goal 82's stage (3)).
                let client = client_role(actor.role());
                if client {
                    let (socket, env) = self.client_env(&actor, true)?;
                    command.env_remove(QUEUE_ENV);
                    provider.reach_queue_service(&mut command, &socket);
                    actor_env.extend(env);
                }
                command.envs(env).envs(actor_env);
                for name in without_env {
                    command.env_remove(name);
                }
                let child = spawner.spawn(&command, streams)?;
                let child = if client {
                    self.service()?
                        .revoke_on_exit(self.queue, actor.actor_id(), child)
                } else {
                    child
                };
                Ok(ActorHandle::Process(child))
            }
        }
    }
}

/// `command` as the one line a workspace runs: its program and arguments,
/// quoted. The workspace gives the directory and the environment.
pub fn command_line(command: &CommandSpec) -> Result<String> {
    let mut argv = vec![path_text(Path::new(command.get_program()))?];
    for arg in command.get_args() {
        argv.push(
            arg.to_str()
                .context("an argument of the agent is not UTF-8")?
                .to_owned(),
        );
    }
    Ok(shell_join(&argv))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::{Exit, SupervisorEnvironment};
    use crate::domain::{CommitSha, NewTask, Task, task};
    use std::sync::Mutex;

    fn claimed_run(id: &str) -> TaskRun {
        let ready = Task::new(
            TaskId::new(3),
            NewTask {
                title: "t".into(),
                description: String::new(),
                acceptance: String::new(),
                verification_commands: Vec::new(),
                dependencies: Vec::new(),
                goal_dependencies: Vec::new(),
                goal_id: None,
                context: String::new(),
                required_evidence: Vec::new(),
                paths: Vec::new(),
                priority: Default::default(),
                change: None,
                provider: None,
                worker_mode: None,
                wait_for_build: false,
            },
            "now".into(),
        )
        .unwrap();
        let ready = task::transition(ready, task::TaskAction::BypassReview, false).unwrap();
        let claimed = task::claim(ready).unwrap();
        let base = CommitSha::parse("0123456789abcdef0123456789abcdef01234567", "base").unwrap();
        TaskRun::new(RunId::new(id).unwrap(), &claimed, &base, "t0".into()).unwrap()
    }

    /// Records what it was asked to make and start.
    #[derive(Default)]
    struct Fake {
        workspaces: Mutex<Vec<(String, String, WorkspaceTags)>>,
        spawned: Mutex<Vec<CommandSpec>>,
        /// Every create reports failing although cmux makes the workspace
        /// (a create that timed out, task 806).
        create_times_out: bool,
        /// The workspaces cmux lists, as (ID, description).
        listed: Mutex<Vec<(String, String)>>,
        closed: Mutex<Vec<String>>,
        /// The actors a token was issued for, in order.
        issued: Mutex<Vec<String>>,
        /// The actors whose token a job's end revokes, in order.
        revoked: Mutex<Vec<String>>,
    }

    /// The queue service's socket and token files at fixed paths under
    /// `/q/service`, by actor id.
    impl ServiceAccess for Fake {
        fn socket(&self, _: &Path) -> PathBuf {
            "/q/service/queue.sock".into()
        }
        fn issue(&self, _: &Path, principal: &Principal) -> Result<PathBuf> {
            self.issued.lock().unwrap().push(principal.actor_id.clone());
            Ok(format!("/q/service/credentials/{}", principal.actor_id).into())
        }
        fn credential(&self, _: &Path, actor_id: &str) -> Option<PathBuf> {
            self.issued
                .lock()
                .unwrap()
                .iter()
                .any(|issued| issued == actor_id)
                .then(|| format!("/q/service/credentials/{actor_id}").into())
        }
        fn revoke_on_exit(
            &self,
            _: &Path,
            actor_id: &str,
            child: Box<dyn Spawned>,
        ) -> Box<dyn Spawned> {
            self.revoked.lock().unwrap().push(actor_id.to_owned());
            child
        }
    }

    impl Fake {
        /// Make the workspace `id` the way cmux does, and fail the create
        /// when it times out.
        fn made(&self, id: &str, tags: &WorkspaceTags) -> Result<String> {
            if !self.create_times_out {
                return Ok(id.into());
            }
            let mut listed = self.listed.lock().unwrap();
            let made = format!("{id}-{}", listed.len());
            listed.push((made, tags.description.clone().unwrap_or_default()));
            bail!("Error: Command timed out")
        }
    }

    impl AgentProvider for Fake {
        fn preflight(&self) -> Result<()> {
            Ok(())
        }
        fn command(&self, run: &TaskRun, prompt: &str) -> Result<CommandSpec> {
            let mut command = CommandSpec::new("agent");
            command.arg(run.id().as_str()).arg(prompt);
            Ok(command)
        }
        fn resume_command(&self, run: &TaskRun) -> Result<CommandSpec> {
            let mut command = CommandSpec::new("agent");
            command.arg("--resume").arg(run.id().as_str());
            Ok(command)
        }
        fn review_command(
            &self,
            run: &TaskRun,
            prompt: &str,
            access: JobAccess,
        ) -> Result<CommandSpec> {
            let mut command = CommandSpec::new("review");
            command
                .arg(run.id().as_str())
                .arg(access.as_str())
                .arg(prompt);
            Ok(command)
        }
        fn headless_command(
            &self,
            cwd: &Path,
            prompt: &str,
            access: JobAccess,
        ) -> Result<CommandSpec> {
            let mut command = CommandSpec::new("job");
            command.current_dir(cwd).arg(access.as_str()).arg(prompt);
            Ok(command)
        }
        fn inbox_command(
            &self,
            prompt: &str,
            plugin_dir: Option<&Path>,
            _queue_dir: &Path,
        ) -> Result<CommandSpec> {
            let mut command = CommandSpec::new("/opt/claude");
            if let Some(dir) = plugin_dir {
                command.arg("--plugin-dir").arg(dir);
            }
            command.arg("--").arg(prompt);
            Ok(command)
        }
        fn select_model(&self, command: &mut CommandSpec, model: &str, effort: &str) {
            command.option_args(["--model", model, "--effort", effort]);
        }
        fn assign_session_id(&self, command: &mut CommandSpec, session_id: &str) {
            command.option_args(["--session-id", session_id]);
        }
        fn without_mcp(&self, command: &mut CommandSpec) {
            command.option_args(["--strict-mcp-config"]);
        }
        fn broker_tools(&self, command: &mut CommandSpec, config: &Path) -> bool {
            command.option_args([std::ffi::OsStr::new("--mcp-config"), config.as_os_str()]);
            true
        }
        /// `turn <session> [--required <config>]`: what the turn's target
        /// asked of the broker.
        fn turn_command(
            &self,
            target: &TurnTarget<'_>,
            _: &str,
            session: crate::domain::turn::TurnSession<'_>,
        ) -> Result<CommandSpec> {
            let mut command = CommandSpec::new("turn");
            command.arg(match session {
                crate::domain::turn::TurnSession::New(name)
                | crate::domain::turn::TurnSession::Resume(name) => name,
            });
            if let Some(config) = target.broker_required {
                command.arg("--required").arg(config);
            }
            Ok(command)
        }
    }

    struct Child;

    impl Spawned for Child {
        fn id(&self) -> u32 {
            7
        }
        fn try_wait(&mut self) -> Result<Option<Exit>> {
            Ok(None)
        }
        fn kill(&mut self) -> Result<()> {
            Ok(())
        }
        fn wait(&mut self) -> Result<Exit> {
            unreachable!()
        }
    }

    impl Spawner for Fake {
        fn spawn(&self, command: &CommandSpec, _: Streams<'_>) -> Result<Box<dyn Spawned>> {
            self.spawned.lock().unwrap().push(command.clone());
            Ok(Box::new(Child))
        }
    }

    impl WorkspaceBackend for Fake {
        fn preflight(&self) -> Result<()> {
            Ok(())
        }
        fn preflight_detached(&self, _: &SupervisorEnvironment) -> Result<()> {
            Ok(())
        }
        fn launch_background(
            &self,
            cwd: &Path,
            command: &str,
            env: &[(String, String)],
            log: &Path,
        ) -> Result<String> {
            self.workspaces.lock().unwrap().push((
                format!("background {} {}", cwd.display(), log.display()),
                command.to_owned(),
                WorkspaceTags {
                    env: env.to_vec(),
                    ..WorkspaceTags::default()
                },
            ));
            Ok("background:7:start".into())
        }
        fn send_text(&self, _: &str, _: &str) -> Result<()> {
            unreachable!()
        }
        fn send_enter(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn capture(&self, _: &str) -> Result<String> {
            unreachable!()
        }
        fn close(&self, id: &str) -> Result<()> {
            let mut listed = self.listed.lock().unwrap();
            let before = listed.len();
            listed.retain(|(listed, _)| listed != id);
            ensure!(listed.len() < before, "no such workspace: {id}");
            self.closed.lock().unwrap().push(id.to_owned());
            Ok(())
        }
        fn workspaces_described(&self, description: &str) -> Result<Vec<String>> {
            Ok(self
                .listed
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, listed)| listed == description)
                .map(|(id, _)| id.clone())
                .collect())
        }
        fn set_color(&self, _: &str, _: &str) -> Result<()> {
            unreachable!()
        }
        fn set_status(&self, _: &str, _: &str, _: &str, _: &str) -> Result<()> {
            unreachable!()
        }
        fn pin(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn send_exit(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn exists(&self, _: &str) -> Result<bool> {
            unreachable!()
        }
        fn listed_workspace_ids(&self) -> Result<Vec<String>> {
            unreachable!()
        }
        fn create_named(
            &self,
            name: &str,
            _: &Path,
            command: &str,
            tags: &WorkspaceTags,
        ) -> Result<String> {
            self.workspaces.lock().unwrap().push((
                name.to_owned(),
                command.to_owned(),
                tags.clone(),
            ));
            self.made("w-named", tags)
        }
        fn ensure_group(&self, _: &str, _: &str) -> Result<String> {
            unreachable!()
        }
        fn notify(&self, _: &str, _: &str, _: Option<&str>) -> Result<()> {
            unreachable!()
        }
    }

    /// The variables `command` sets, in order.
    fn env_of(command: &CommandSpec) -> Vec<(String, String)> {
        command
            .get_envs()
            .filter_map(|(key, value)| {
                Some((
                    key.to_string_lossy().into_owned(),
                    value?.to_string_lossy().into_owned(),
                ))
            })
            .collect()
    }

    /// The variables `command` does not inherit, in order.
    fn removed_of(command: &CommandSpec) -> Vec<String> {
        command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect()
    }

    fn pairs(env: &[(&str, &str)]) -> Vec<(String, String)> {
        env.iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn job(cwd: &Path) -> ActorProgram<'_> {
        ActorProgram::Headless {
            program: HeadlessProgram::Job {
                cwd,
                prompt: "p",
                access: JobAccess::QueueCli,
            },
            session_id: None,
            launch: None,
            without_mcp: false,
            without_env: &[],
            env: Vec::new(),
            streams: Streams::Null,
        }
    }

    #[test]
    fn no_claude_refuses_every_headless_role_before_accessing_a_provider() {
        let cwd = Path::new("/work");
        let executor = HostActorExecutor::new(Path::new("/queue.db")).with_no_claude(true);
        for role in [
            ActorRole::RecoveryJob,
            ActorRole::PlanReviewJob,
            ActorRole::GoalReviewJob,
            ActorRole::ThroughputReviewJob,
            ActorRole::Observer,
        ] {
            let error = executor
                .spawn(ActorExecutionSpec::new(
                    ActorContext::instance(role, "test"),
                    WorkspaceAccess::Scratch(cwd.into()),
                    job(cwd),
                ))
                .err()
                .unwrap();
            assert!(
                error.to_string().contains("provider_disabled"),
                "{role:?}: {error}"
            );
        }
    }

    #[test]
    fn the_host_backend_is_advisory() {
        let executor = HostActorExecutor::new(Path::new("/q/queue.db"));
        assert_eq!(executor.backend().as_str(), "host");
        assert_eq!(executor.enforcement().as_str(), "advisory");
    }

    #[test]
    fn the_environment_names_the_actor_its_queue_and_its_session() {
        let queue = Path::new("/q/queue.db");
        let run = RunId::new("r1").unwrap();
        assert_eq!(
            actor_env(
                queue,
                &ActorContext::worker(&run, TaskId::new(3)),
                None,
                None
            )
            .unwrap(),
            // No queue path: its `dagq` runs in client mode (goal 82's
            // stage (3)).
            pairs(&[
                ("DAGQ_ROLE", "worker"),
                ("DAGQ_ACTOR_ID", "worker:r1"),
                ("DAGQ_RUN_ID", "r1"),
                ("DAGQ_TASK_ID", "3"),
            ])
        );
        assert_eq!(
            actor_env(
                queue,
                &ActorContext::new(ActorRole::Inbox, "inbox"),
                None,
                None
            )
            .unwrap(),
            pairs(&[
                ("DAGQ_ROLE", "inbox"),
                ("DAGQ_QUEUE", "/q/queue.db"),
                ("DAGQ_ACTOR_ID", "inbox"),
                ("DAGQ_SESSION_KIND", "inbox"),
            ])
        );
        let planner = ActorContext::instance(ActorRole::Planner, 4);
        let launch = ActorLaunch::default_of(crate::domain::actor_model::ModelRole::Planner);
        let env = actor_env(
            queue,
            &planner,
            Some((PlannerOrigin::Runtime, PlannerId::new(4))),
            Some(&launch),
        )
        .unwrap();
        assert_eq!(
            env[..6],
            pairs(&[
                ("DAGQ_ROLE", "planner"),
                ("DAGQ_QUEUE", "/q/queue.db"),
                ("DAGQ_ACTOR_ID", "planner:4"),
                ("DAGQ_SESSION_KIND", "runtime_planner"),
                ("DAGQ_PLANNER_ORIGIN", "runtime"),
                ("DAGQ_PLANNER_ID", "4"),
            ])[..]
        );
        assert_eq!(env[6].0, "DAGQ_LAUNCH");
        assert_eq!(
            actor_env(queue, &ActorContext::review_job(&run, 2), None, None).unwrap(),
            pairs(&[
                ("DAGQ_ROLE", "review-job"),
                ("DAGQ_ACTOR_ID", "review-job:r1:2"),
            ])
        );
    }

    #[test]
    fn a_spec_that_does_not_hold_together_is_refused() {
        let fake = Fake::default();
        let executor = HostActorExecutor::new(Path::new("/q/queue.db"))
            .with_provider(&fake)
            .with_spawner(&fake)
            .with_queue_service(&fake);
        let dir = Path::new("/q/job");
        // Not an AI actor.
        for actor in [
            ActorContext::user(),
            ActorContext::instance(ActorRole::Supervisor, 1),
            ActorContext::new(ActorRole::Integrator, "integrator"),
        ] {
            let error = executor
                .spawn(ActorExecutionSpec::new(
                    actor,
                    WorkspaceAccess::Scratch(dir.into()),
                    job(dir),
                ))
                .err()
                .unwrap();
            assert!(error.to_string().contains("is not an AI actor"), "{error}");
        }
        // A program of another role.
        let error = executor
            .spawn(ActorExecutionSpec::new(
                ActorContext::instance(ActorRole::Planner, 1),
                WorkspaceAccess::Scratch(dir.into()),
                job(dir),
            ))
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("cannot be started as this program")
        );
        // Capabilities beyond its role's.
        let mut spec = ActorExecutionSpec::new(
            ActorContext::instance(ActorRole::Observer, "s1"),
            WorkspaceAccess::Scratch(dir.into()),
            job(dir),
        );
        spec.capabilities = grants(ActorRole::User);
        let error = executor.spawn(spec).err().unwrap();
        assert!(error.to_string().contains("are not its role's"));
        // A worker of another run.
        let run = claimed_run("r2");
        let error = executor
            .spawn(ActorExecutionSpec::new(
                ActorContext::worker(&RunId::new("r1").unwrap(), TaskId::new(3)),
                WorkspaceAccess::Write("/w".into()),
                ActorProgram::SessionAgent {
                    agent: SessionAgent::Resume { run: &run },
                    model: None,
                },
            ))
            .err()
            .unwrap();
        assert!(
            error.to_string().contains("is started for run r2"),
            "{error}"
        );
        assert!(fake.spawned.lock().unwrap().is_empty());
    }

    /// A worker or a job is never started without the queue service to give
    /// it, and never given the queue's path instead (goal 82's stage (3)).
    #[test]
    fn a_worker_or_a_job_without_the_queue_service_is_not_started() {
        let run = claimed_run("r1");
        let fake = Fake::default();
        let executor = HostActorExecutor::new(Path::new("/q/queue.db"))
            .with_workspaces(&fake)
            .with_provider(&fake)
            .with_spawner(&fake);
        let dir = Path::new("/q/job");
        let worker = || ActorContext::worker(run.id(), run.task_id());
        for spec in [
            ActorExecutionSpec::new(
                worker(),
                WorkspaceAccess::Write("/w".into()),
                ActorProgram::RunSession {
                    run: &run,
                    wrapper: "wrapper".into(),
                    run_env: Vec::new(),
                    log: Path::new("/r/session.log"),
                },
            ),
            ActorExecutionSpec::new(
                worker(),
                WorkspaceAccess::Write("/w".into()),
                ActorProgram::SessionAgent {
                    agent: SessionAgent::Resume { run: &run },
                    model: None,
                },
            ),
            ActorExecutionSpec::new(
                ActorContext::instance(ActorRole::Observer, "s1"),
                WorkspaceAccess::Scratch(dir.into()),
                job(dir),
            ),
        ] {
            let error = executor.spawn(spec).err().unwrap();
            assert!(error.to_string().contains("no queue service"), "{error}");
        }
        assert!(fake.workspaces.lock().unwrap().is_empty());
        assert!(fake.spawned.lock().unwrap().is_empty());
    }

    #[test]
    fn a_program_needs_the_part_that_starts_it() {
        let run = claimed_run("r1");
        let executor = HostActorExecutor::new(Path::new("/q/queue.db"));
        let spec = ActorExecutionSpec::new(
            ActorContext::worker(run.id(), run.task_id()),
            WorkspaceAccess::Write("/w".into()),
            ActorProgram::RunSession {
                run: &run,
                wrapper: "wrapper".into(),
                run_env: Vec::new(),
                log: Path::new("/r/session.log"),
            },
        );
        assert_eq!(spec.run_id(), Some(run.id()));
        assert_eq!(spec.task_id(), Some(TaskId::new(3)));
        assert_eq!(spec.workspace.path(), Path::new("/w"));
        let error = executor.spawn(spec).err().unwrap();
        assert!(error.to_string().contains("opens no workspace"));
        let dir = Path::new("/q/job");
        let error = executor
            .spawn(ActorExecutionSpec::new(
                ActorContext::instance(ActorRole::Observer, "s1"),
                WorkspaceAccess::Scratch(dir.into()),
                job(dir),
            ))
            .err()
            .unwrap();
        assert!(error.to_string().contains("no agent provider"));
        let fake = Fake::default();
        let error = HostActorExecutor::new(Path::new("/q/queue.db"))
            .with_provider(&fake)
            .spawn(ActorExecutionSpec::new(
                ActorContext::instance(ActorRole::Observer, "s1"),
                WorkspaceAccess::Scratch(dir.into()),
                job(dir),
            ))
            .err()
            .unwrap();
        assert!(error.to_string().contains("starts no process"));
    }

    /// A run's session opens no workspace (ADR-t1433-3): its wrapper
    /// starts in the background in the run's worktree with the worker's
    /// variables and `[run.env]` after them, and the claim and the resume
    /// each issue the worker's token.
    #[test]
    fn a_run_session_starts_its_wrapper_in_the_background_with_the_worker_env() {
        let run = claimed_run("r1");
        let fake = Fake::default();
        let executor = HostActorExecutor::new(Path::new("/q/queue.db"))
            .with_workspaces(&fake)
            .with_queue_service(&fake);
        for log in ["/r/session.log", "/r/session-resume-1.log"] {
            let handle = executor
                .spawn(ActorExecutionSpec::new(
                    ActorContext::worker(run.id(), run.task_id()),
                    WorkspaceAccess::Write("/w".into()),
                    ActorProgram::RunSession {
                        run: &run,
                        wrapper: "wrapper --background".into(),
                        run_env: pairs(&[("SHARED", "x")]),
                        log: Path::new(log),
                    },
                ))
                .unwrap();
            assert_eq!(handle.workspace().unwrap(), "background:7:start");
        }
        let made = fake.workspaces.lock().unwrap();
        assert_eq!(made.len(), 2);
        assert_eq!(made[0].0, "background /w /r/session.log");
        assert_eq!(made[1].0, "background /w /r/session-resume-1.log");
        for (_, command, tags) in made.iter() {
            assert_eq!(command, "wrapper --background");
            assert_eq!(
                tags.env,
                pairs(&[
                    ("DAGQ_ROLE", "worker"),
                    ("DAGQ_ACTOR_ID", "worker:r1"),
                    ("DAGQ_RUN_ID", "r1"),
                    ("DAGQ_TASK_ID", "3"),
                    ("SHARED", "x"),
                ])
            );
        }
        assert_eq!(*fake.issued.lock().unwrap(), ["worker:r1", "worker:r1"]);
    }

    /// Task 806: a create that reports failing although cmux made the
    /// workspace (a create that timed out) closes the workspaces listed
    /// with the description of the planner's workspace, and the error says
    /// so; another workspace, and the inbox's, whose description is the
    /// queue's, are left open.
    #[test]
    fn a_workspace_cmux_made_although_its_create_failed_is_closed() {
        let fake = Fake {
            create_times_out: true,
            ..Fake::default()
        };
        fake.listed
            .lock()
            .unwrap()
            .push(("other".into(), "dagq role=worker run=r2".into()));
        let executor = HostActorExecutor::new(Path::new("/q/queue.db"))
            .with_workspaces(&fake)
            .with_provider(&fake)
            .with_queue_service(&fake);
        let planner = |planner: Option<(PlannerOrigin, PlannerId)>, description: &str| {
            executor.spawn(ActorExecutionSpec::new(
                match planner {
                    Some(_) => ActorContext::instance(ActorRole::Planner, 4),
                    None => ActorContext::new(ActorRole::Inbox, "inbox"),
                },
                WorkspaceAccess::Write("/repo".into()),
                ActorProgram::NamedWorkspace {
                    name: "[repo]planner#4",
                    cwd: Path::new("/repo"),
                    command: match planner {
                        Some(_) => WorkspaceCommand::Wrapper("wrapper".into()),
                        None => WorkspaceCommand::Agent {
                            prompt: "p".into(),
                            plugin_dir: None,
                        },
                    },
                    planner,
                    launch: None,
                    description: Some(description.into()),
                    group: None,
                    background: None,
                },
            ))
        };
        let error = planner(
            Some((PlannerOrigin::Runtime, PlannerId::new(4))),
            "dagq role=planner planner=4",
        )
        .err()
        .unwrap();
        assert!(format!("{error:#}").contains("was closed"), "{error:#}");
        let error = planner(None, "dagq role=inbox").err().unwrap();
        assert!(!format!("{error:#}").contains("closed"), "{error:#}");
        assert_eq!(*fake.closed.lock().unwrap(), ["w-named-1"]);
        let listed: Vec<String> = fake
            .listed
            .lock()
            .unwrap()
            .iter()
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(listed, ["other", "w-named-1"]);
    }

    #[test]
    fn the_inbox_runs_its_agent_as_the_command_of_its_workspace() {
        let fake = Fake::default();
        let id = HostActorExecutor::new(Path::new("/q/queue.db"))
            .with_workspaces(&fake)
            .with_provider(&fake)
            .spawn(ActorExecutionSpec::new(
                ActorContext::new(ActorRole::Inbox, "inbox"),
                WorkspaceAccess::Write("/repo".into()),
                ActorProgram::NamedWorkspace {
                    name: "[repo]inbox",
                    cwd: Path::new("/repo"),
                    command: WorkspaceCommand::Agent {
                        prompt: "You are the inbox".into(),
                        plugin_dir: Some(Path::new("/p")),
                    },
                    planner: None,
                    launch: None,
                    description: Some("d".into()),
                    group: None,
                    background: None,
                },
            ))
            .unwrap()
            .workspace()
            .unwrap();
        assert_eq!(id, "w-named");
        let made = fake.workspaces.lock().unwrap();
        assert_eq!(made[0].0, "[repo]inbox");
        assert_eq!(
            made[0].1,
            "'/opt/claude' '--plugin-dir' '/p' '--' 'You are the inbox'"
        );
        assert_eq!(made[0].2.env[0], ("DAGQ_ROLE".into(), "inbox".into()));
    }

    #[test]
    fn a_headless_job_gets_its_options_then_its_environment() {
        let fake = Fake::default();
        let launch = ActorLaunch::default_of(crate::domain::actor_model::ModelRole::Observer);
        let dir = Path::new("/q/job");
        let child = HostActorExecutor::new(Path::new("/q/queue.db"))
            .with_provider(&fake)
            .with_spawner(&fake)
            .with_queue_service(&fake)
            .spawn(
                ActorExecutionSpec::new(
                    ActorContext::instance(ActorRole::Observer, "s1"),
                    WorkspaceAccess::Scratch(dir.into()),
                    ActorProgram::Headless {
                        program: HeadlessProgram::Job {
                            cwd: dir,
                            prompt: "observe",
                            access: JobAccess::QueueCli,
                        },
                        session_id: Some("s1"),
                        launch: Some(&launch),
                        without_mcp: true,
                        without_env: &[],
                        // The caller's own names never replace the actor's.
                        env: pairs(&[("PATH", "/bin"), ("DAGQ_ROLE", "user")]),
                        streams: Streams::Null,
                    },
                )
                .with_timeout(Duration::from_secs(5)),
            )
            .unwrap()
            .process()
            .unwrap();
        assert_eq!(child.id(), 7);
        let spawned = fake.spawned.lock().unwrap();
        let command = &spawned[0];
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--session-id".to_owned()), "{args:?}");
        assert!(args.contains(&"--strict-mcp-config".to_owned()), "{args:?}");
        // The provider gets the job's intent, not a provider's tool names.
        assert!(args.contains(&"queue_cli".to_owned()), "{args:?}");
        assert_eq!(command.get_current_dir(), Some(dir));
        let env = env_of(command);
        assert_eq!(
            env,
            pairs(&[
                ("PATH", "/bin"),
                ("DAGQ_ROLE", "user"),
                ("DAGQ_ROLE", "observer"),
                ("DAGQ_ACTOR_ID", "observer:s1"),
                ("DAGQ_SERVICE_SOCKET", "/q/service/queue.sock"),
                (
                    "DAGQ_SERVICE_CREDENTIAL_FILE",
                    "/q/service/credentials/observer:s1"
                ),
            ])
        );
        // Its own token, which its end revokes.
        assert_eq!(*fake.issued.lock().unwrap(), ["observer:s1"]);
        assert_eq!(*fake.revoked.lock().unwrap(), ["observer:s1"]);
        // The actor variables the observer does not set are not inherited
        // from the starter's session (task 902), and removed before the
        // variables the command sets.
        assert_eq!(
            removed_of(command),
            [
                "DAGQ_RUN_ID",
                "DAGQ_TASK_ID",
                "DAGQ_SESSION_KIND",
                "DAGQ_PLANNER_ID",
                "DAGQ_PLANNER_ORIGIN",
                "DAGQ_QUEUE",
            ]
        );
        let removes = command.get_envs().position(|(_, value)| value.is_none());
        let sets = command.get_envs().position(|(_, value)| value.is_some());
        assert!(removes < sets);
    }

    /// Every headless job starts without the actor variables its actor does
    /// not set, and a job with a run gets its run and task (task 902).
    #[test]
    fn a_headless_job_inherits_no_actor_it_is_not() {
        let run = claimed_run("r1");
        let fake = Fake::default();
        let executor = HostActorExecutor::new(Path::new("/q/queue.db"))
            .with_provider(&fake)
            .with_spawner(&fake)
            .with_queue_service(&fake);
        let dir = Path::new("/q/job");
        let jobs = [
            ActorContext::recovery_job(run.id(), "failed", 1),
            ActorContext::plan_review_job(4, 1),
            ActorContext::goal_review_job(5, 1),
            ActorContext::instance(ActorRole::Observer, "s1"),
        ];
        for actor in jobs {
            executor
                .spawn(ActorExecutionSpec::new(
                    actor,
                    WorkspaceAccess::Scratch(dir.into()),
                    job(dir),
                ))
                .unwrap();
        }
        let review = |actor: ActorContext| {
            executor
                .spawn(ActorExecutionSpec::new(
                    actor,
                    WorkspaceAccess::Read(dir.into()),
                    ActorProgram::Headless {
                        program: HeadlessProgram::Review {
                            run: &run,
                            prompt: "review",
                            access: JobAccess::ReadFiles,
                            subagents: &[],
                        },
                        session_id: None,
                        launch: None,
                        without_mcp: false,
                        without_env: &[],
                        env: Vec::new(),
                        streams: Streams::Null,
                    },
                ))
                .unwrap();
        };
        review(ActorContext::review_job(run.id(), 1));
        review(ActorContext::review_job(run.id(), 2).with_run(run.id().clone(), run.task_id()));
        let spawned = fake.spawned.lock().unwrap();
        assert_eq!(spawned.len(), 6);
        let removed: Vec<_> = ACTOR_ENV[2..].iter().chain(&["DAGQ_QUEUE"]).collect();
        for command in &spawned[..5] {
            assert_eq!(
                removed_of(command).iter().collect::<Vec<_>>(),
                removed,
                "{:?}",
                command.get_program()
            );
            assert!(
                !env_of(command)
                    .iter()
                    .any(|(key, _)| ACTOR_ENV[2..].contains(&key.as_str()))
            );
        }
        let with_run = &spawned[5];
        assert_eq!(
            removed_of(with_run),
            ACTOR_ENV[4..]
                .iter()
                .chain(&["DAGQ_QUEUE"])
                .copied()
                .collect::<Vec<_>>()
        );
        let env = env_of(with_run);
        assert!(
            env.contains(&("DAGQ_RUN_ID".into(), "r1".into())),
            "{env:?}"
        );
        assert!(
            env.contains(&("DAGQ_TASK_ID".into(), "3".into())),
            "{env:?}"
        );
    }

    /// The actor variables a workspace names are its actor's, and the
    /// workspace removes none: cmux gives it the env (task 902).
    #[test]
    fn the_actor_env_names_every_variable_actor_env_sets() {
        let run = claimed_run("r1");
        let queue = Path::new("/q/queue.db");
        let planner = actor_env(
            queue,
            &ActorContext::instance(ActorRole::Planner, 4),
            Some((PlannerOrigin::Runtime, PlannerId::new(4))),
            None,
        )
        .unwrap();
        let worker = actor_env(
            queue,
            &ActorContext::worker(run.id(), run.task_id()),
            None,
            None,
        )
        .unwrap();
        let mut named: Vec<_> = planner
            .iter()
            .chain(&worker)
            .map(|(key, _)| key.as_str())
            .filter(|key| *key != "DAGQ_QUEUE")
            .collect();
        named.sort_unstable();
        named.dedup();
        let mut all = ACTOR_ENV.to_vec();
        all.sort_unstable();
        assert_eq!(named, all);
    }

    #[test]
    fn the_session_agent_is_its_actor_with_its_model() {
        let run = claimed_run("r1");
        let fake = Fake::default();
        let executor = HostActorExecutor::new(Path::new("/q/queue.db"))
            .with_provider(&fake)
            .with_spawner(&fake)
            .with_queue_service(&fake);
        for agent in [
            SessionAgent::Worker {
                run: &run,
                prompt: "work",
            },
            SessionAgent::Resume { run: &run },
        ] {
            executor
                .spawn(ActorExecutionSpec::new(
                    ActorContext::worker(run.id(), run.task_id()),
                    WorkspaceAccess::Write("/w".into()),
                    ActorProgram::SessionAgent {
                        agent,
                        model: Some(("opus", "high")),
                    },
                ))
                .unwrap();
        }
        for command in fake.spawned.lock().unwrap().iter() {
            let args: Vec<_> = command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            assert!(args.contains(&"opus".to_owned()), "{args:?}");
            // The worker's `dagq` goes to the queue service with the
            // run's token, never to the queue's path.
            assert_eq!(
                env_of(command),
                pairs(&[
                    ("DAGQ_ROLE", "worker"),
                    ("DAGQ_ACTOR_ID", "worker:r1"),
                    ("DAGQ_RUN_ID", "r1"),
                    ("DAGQ_TASK_ID", "3"),
                    ("DAGQ_SERVICE_SOCKET", "/q/service/queue.sock"),
                    (
                        "DAGQ_SERVICE_CREDENTIAL_FILE",
                        "/q/service/credentials/worker:r1"
                    ),
                ])
            );
            assert_eq!(removed_of(command), ["DAGQ_QUEUE"]);
        }
        // The wrapper of a run the claim issued no token for issues one,
        // once; the resume takes the one there is.
        assert_eq!(*fake.issued.lock().unwrap(), ["worker:r1"]);
        assert!(fake.revoked.lock().unwrap().is_empty());
    }

    /// The broker's tools (`preferred`) reach a worker, its resume and its
    /// turns only through the MCP configuration the supervisor wrote in the
    /// run's dir; without it (`disabled`) the command and the environment
    /// are the provider's own, as before.
    #[test]
    fn a_worker_gets_the_brokers_tools_only_with_its_mcp_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let run = claimed_run("r1");
        let run_dir = dir.path().join("r1");
        let text = |name: &str| run_dir.join(name).to_string_lossy().into_owned();
        let run = crate::domain::run::start_provisioning(
            run,
            &crate::domain::RunPlan {
                repo_path: "/repo".into(),
                run_dir: text(""),
                branch: "dagq/r1".into(),
                worktree_path: text("worktree"),
                receipt_path: text("receipt.json"),
                log_path: text("log"),
            },
        )
        .unwrap();
        let spawn = || {
            let fake = Fake::default();
            let executor = HostActorExecutor::new(Path::new("/q/queue.db"))
                .with_provider(&fake)
                .with_spawner(&fake)
                .with_queue_service(&fake);
            for agent in [
                SessionAgent::Worker {
                    run: &run,
                    prompt: "work",
                },
                SessionAgent::Resume { run: &run },
            ] {
                executor
                    .spawn(ActorExecutionSpec::new(
                        ActorContext::worker(run.id(), run.task_id()),
                        WorkspaceAccess::Write("/w".into()),
                        ActorProgram::SessionAgent { agent, model: None },
                    ))
                    .unwrap();
            }
            fake.spawned
                .lock()
                .unwrap()
                .iter()
                .map(|command| {
                    let args: Vec<String> = command
                        .get_args()
                        .map(|arg| arg.to_string_lossy().into_owned())
                        .collect();
                    (args, env_of(command))
                })
                .collect::<Vec<_>>()
        };
        let without = spawn();
        assert_eq!(without[0].0, ["r1", "work"]);
        assert_eq!(without[1].0, ["--resume", "r1"]);
        let config = super::super::broker_run::mcp_config_path(&run_dir);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "{}").unwrap();
        let with = spawn();
        let config = config.to_string_lossy().into_owned();
        assert_eq!(with[0].0, ["r1", "work", "--mcp-config", config.as_str()]);
        assert_eq!(
            with[1].0,
            ["--resume", "r1", "--mcp-config", config.as_str()]
        );
        // The environment is the same either way: no token, no URL.
        assert_eq!(with[0].1, without[0].1);
        assert_eq!(with[1].1, without[1].1);
    }

    /// `required` (ADR-t838-1): a run the supervisor marked `required`
    /// without its MCP configuration is refused before any command of its
    /// agent is made, so nothing starts with the built-in tools; with the
    /// configuration, its turns (new and resumed) are made for the broker
    /// (`TurnTarget::broker_required`) and not given `preferred`'s
    /// `--mcp-config` beside the built-in tools. An interactive worker or
    /// resume of a `required` run is refused. Without the mark nothing
    /// changes.
    #[test]
    fn a_required_run_starts_only_with_the_brokers_tools() {
        use crate::application::broker_run::{
            BrokerRequiredRefused, mcp_config_path, required_path,
        };
        use crate::domain::turn::TurnSession;
        let dir = tempfile::tempdir().unwrap();
        let run = claimed_run("r1");
        let run_dir = dir.path().join("r1");
        let text = |name: &str| run_dir.join(name).to_string_lossy().into_owned();
        let run = crate::domain::run::start_provisioning(
            run,
            &crate::domain::RunPlan {
                repo_path: "/repo".into(),
                run_dir: text(""),
                branch: "dagq/r1".into(),
                worktree_path: text("worktree"),
                receipt_path: text("receipt.json"),
                log_path: text("log"),
            },
        )
        .unwrap();
        let (stdout, stderr) = (run_dir.join("out"), run_dir.join("err"));
        // Each agent's spawn: the arguments started, or the refusal.
        let spawn = || {
            let fake = Fake::default();
            let executor = HostActorExecutor::new(Path::new("/q/queue.db"))
                .with_provider(&fake)
                .with_spawner(&fake)
                .with_queue_service(&fake);
            let agents = [
                SessionAgent::Turn {
                    run: &run,
                    prompt: "work",
                    session: TurnSession::New("s"),
                    stdout: &stdout,
                    stderr: &stderr,
                    without_env: &[],
                },
                SessionAgent::Turn {
                    run: &run,
                    prompt: "go on",
                    session: TurnSession::Resume("s"),
                    stdout: &stdout,
                    stderr: &stderr,
                    without_env: &[],
                },
                SessionAgent::Worker {
                    run: &run,
                    prompt: "work",
                },
                SessionAgent::Resume { run: &run },
            ];
            let outcomes: Vec<std::result::Result<(), bool>> = agents
                .into_iter()
                .map(|agent| {
                    executor
                        .spawn(ActorExecutionSpec::new(
                            ActorContext::worker(run.id(), run.task_id()),
                            WorkspaceAccess::Write("/w".into()),
                            ActorProgram::SessionAgent { agent, model: None },
                        ))
                        .map(drop)
                        .map_err(|error| BrokerRequiredRefused::is(&error))
                })
                .collect();
            let spawned: Vec<Vec<String>> = fake
                .spawned
                .lock()
                .unwrap()
                .iter()
                .map(|command| {
                    std::iter::once(command.get_program())
                        .chain(command.get_args())
                        .map(|arg| arg.to_string_lossy().into_owned())
                        .collect()
                })
                .collect();
            (outcomes, spawned)
        };
        std::fs::create_dir_all(&run_dir).unwrap();
        let (outcomes, before) = spawn();
        assert_eq!(outcomes, [Ok(()), Ok(()), Ok(()), Ok(())]);
        assert_eq!(before[0], ["turn", "s"]);

        // Marked, without the configuration: nothing is made or started.
        std::fs::write(required_path(&run_dir), "").unwrap();
        let (outcomes, spawned) = spawn();
        assert_eq!(outcomes, [Err(true), Err(true), Err(true), Err(true)]);
        assert!(spawned.is_empty(), "{spawned:?}");

        // With it: the turns are the broker's, the sessions refused.
        let config = mcp_config_path(&run_dir);
        let config_dir = config.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(&config, "{}").unwrap();
        let config = config.to_string_lossy().into_owned();
        let (outcomes, spawned) = spawn();
        assert_eq!(outcomes, [Ok(()), Ok(()), Err(true), Err(true)]);
        assert_eq!(
            spawned,
            [
                ["turn", "s", "--required", config.as_str()],
                ["turn", "s", "--required", config.as_str()],
            ]
        );

        // The mark gone (`preferred`): the configuration beside the
        // built-in tools, as before.
        std::fs::remove_file(required_path(&run_dir)).unwrap();
        let (outcomes, spawned) = spawn();
        assert_eq!(outcomes, [Ok(()), Ok(()), Ok(()), Ok(())]);
        assert_eq!(spawned[0], ["turn", "s", "--mcp-config", config.as_str()]);
        assert_eq!(
            spawned[3],
            ["agent", "--resume", "r1", "--mcp-config", config.as_str()]
        );

        // Whatever is at the mark's path marks the run (a mark the
        // supervisor could not write as a file): without the configuration
        // nothing starts.
        std::fs::create_dir(required_path(&run_dir)).unwrap();
        std::fs::remove_dir_all(config_dir).unwrap();
        let (outcomes, spawned) = spawn();
        assert_eq!(outcomes, [Err(true), Err(true), Err(true), Err(true)]);
        assert!(spawned.is_empty(), "{spawned:?}");
    }

    #[test]
    fn an_actor_configured_for_podman_is_refused_not_run_on_the_host() {
        let run = claimed_run("r1");
        let fake = Fake::default();
        let executor = HostActorExecutor::new(Path::new("/q/queue.db"))
            .with_provider(&fake)
            .with_spawner(&fake)
            .with_queue_service(&fake)
            .with_config(ExecutionConfig {
                backend: ExecutorBackend::Host,
                actors: vec![(ActorRole::Worker, ExecutorBackend::Podman)],
            });
        let error = executor
            .spawn(ActorExecutionSpec::new(
                ActorContext::worker(run.id(), run.task_id()),
                WorkspaceAccess::Write("/w".into()),
                ActorProgram::SessionAgent {
                    agent: SessionAgent::Resume { run: &run },
                    model: None,
                },
            ))
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("podman"), "{error}");
        assert!(error.contains("not implemented"), "{error}");
        assert!(fake.spawned.lock().unwrap().is_empty());
        // The roles it does not name still run on the host.
        let dir = tempfile::tempdir().unwrap();
        executor
            .spawn(ActorExecutionSpec::new(
                ActorContext::new(ActorRole::Observer, "observer"),
                WorkspaceAccess::Scratch(dir.path().into()),
                job(dir.path()),
            ))
            .unwrap();
        assert_eq!(fake.spawned.lock().unwrap().len(), 1);
    }

    /// A worker is given only the client next to dagq that names dagq's
    /// build: a missing one or one of another build is refused with its
    /// code, never handed over (ADR-t827-1 decision 7).
    #[test]
    fn a_worker_gets_only_the_client_of_dagqs_build() {
        use crate::application::broker::FailureCode;
        let dir = tempfile::tempdir().unwrap();
        let dagq = dir.path().join("dagq");
        std::fs::write(&dagq, "").unwrap();
        let executor = HostActorExecutor::new(dir.path());
        let ours = |_: &Path| Ok::<_, String>(crate::VERSION.to_owned());
        let error = executor.broker_client(&dagq, &ours).unwrap_err();
        assert_eq!(error.code, FailureCode::ClientMissing);

        let client = dir.path().join("dagq-broker-client");
        std::fs::write(&client, "").unwrap();
        assert_eq!(executor.broker_client(&dagq, &ours).unwrap(), client);

        let other = |_: &Path| Ok::<_, String>("0.0.1-dev+other".to_owned());
        let error = executor.broker_client(&dagq, &other).unwrap_err();
        assert_eq!(error.code, FailureCode::VersionMismatch);
        assert!(
            error.message.contains("0.0.1-dev+other") && error.message.contains(crate::VERSION),
            "{error}"
        );
        let silent = |_: &Path| Err::<String, _>("exited with 1".to_owned());
        let error = executor.broker_client(&dagq, &silent).unwrap_err();
        assert_eq!(error.code, FailureCode::VersionMismatch);
    }
}
