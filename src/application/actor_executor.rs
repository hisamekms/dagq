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
//! Code), the background session wrappers ([`SessionWrappers`]) and the
//! inbox's cmux [`WorkspaceBackend`]. On a host the spec is advisory
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

use super::queue_service::ServiceAccess;
use super::{
    AgentProvider, CommandSpec, PlannerCommand, RunLog, SessionWrappers, Spawned, Spawner, Streams,
    TurnTarget, WorkspaceBackend, WorkspaceTags,
    lifecycle::{PLANNER_ID_ENV, PLANNER_ORIGIN_ENV, QUEUE_ENV, SESSION_KIND_ENV},
    naming::shell_join,
    path_text,
};
use crate::domain::{
    ActorContext, ActorRole, EventKind, PlannerId, PlannerOrigin, RunId, TaskId, TaskRun,
    TrustLevel,
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
    /// One turn of a headless worker (ADR-t813-1): `prompt` starting a
    /// session or going on with one, as `session` says, its output to
    /// `stdout` and `stderr` rather than the wrapper's terminal, without
    /// the variables `without_env` names (`RUSTC_WRAPPER` for a turn whose
    /// sccache server was not confirmed) and with those of `with_env` (the
    /// refusal of the sccache server's start and its guard, ADR-t2086-1).
    Turn {
        run: &'a TaskRun,
        prompt: &'a str,
        session: crate::domain::turn::TurnSession<'a>,
        stdout: &'a Path,
        stderr: &'a Path,
        without_env: &'a [&'a str],
        with_env: &'a [(String, String)],
    },
    /// A planner in a terminal (ADR-0041): only a person's, opened before
    /// `dagq plan` was abolished (ADR-t1394-1). The runtime's planners run
    /// headless only (ADR-t1433-2 decision 3), each turn a
    /// [`SessionAgent::PlannerTurn`].
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
    /// A workspace of its own: the inbox. `launch` names the model its
    /// agent starts with.
    NamedWorkspace {
        name: &'a str,
        cwd: &'a Path,
        command: WorkspaceCommand<'a>,
        launch: Option<&'a ActorLaunch>,
        description: Option<String>,
        group: Option<String>,
    },
    /// The session wrapper `wrapper` (`planner-session --headless`) of a
    /// planner of the runtime's, which runs its agent one call per turn
    /// (ADR-t1394-2 decision 2): started without a workspace as a process
    /// detached from the supervisor, in `cwd`, with the planner's
    /// variables (`planner` names it and its origin, `launch` the model its
    /// agent starts with) and its output in `log` (ADR-t1404-1 decision 8,
    /// ADR-t1433-2 decision 3). Its handle is what the planner records as
    /// its session.
    PlannerSession {
        cwd: &'a Path,
        wrapper: String,
        planner: (PlannerOrigin, PlannerId),
        launch: Option<&'a ActorLaunch>,
        log: &'a Path,
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
    /// job whose sccache server was not confirmed, ADR-t2086-1) and where
    /// its output goes.
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
                SessionAgent::Turn { run, .. } => run.actual_provider(),
                SessionAgent::Planner(_) | SessionAgent::PlannerTurn { .. } => Provider::Claude,
            },
            Self::NamedWorkspace { launch, .. }
            | Self::PlannerSession { launch, .. }
            | Self::Headless { launch, .. } => {
                launch.map_or(Provider::Claude, |launch| launch.provider)
            }
        }
    }

    /// The roles this program is started for: any other is refused.
    fn roles(&self) -> &'static [ActorRole] {
        match self {
            Self::RunSession { .. }
            | Self::SessionAgent {
                agent: SessionAgent::Turn { .. },
                ..
            } => &[ActorRole::Worker],
            Self::SessionAgent {
                agent: SessionAgent::Planner(_) | SessionAgent::PlannerTurn { .. },
                ..
            }
            | Self::PlannerSession { .. } => &[ActorRole::Planner],
            Self::NamedWorkspace { .. } => &[ActorRole::Inbox],
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
                agent: SessionAgent::Turn { run, .. },
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
/// the inbox's workspace through cmux, the session wrappers in the
/// background, agents through the provider and the spawner.
/// Each part is given where the caller has it, and a program that needs a
/// part the executor was not given is refused.
pub struct HostActorExecutor<'a> {
    queue: &'a Path,
    workspaces: Option<&'a dyn WorkspaceBackend>,
    sessions: Option<&'a dyn SessionWrappers>,
    provider: Option<&'a dyn AgentProvider>,
    spawner: Option<&'a dyn Spawner>,
    service: Option<&'a dyn ServiceAccess>,
    events: Option<&'a dyn RunLog>,
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
            sessions: None,
            provider: None,
            spawner: None,
            service: None,
            events: None,
            config: ExecutionConfig::default(),
            no_claude: false,
        }
    }

    /// An agent whose executable was found again at its start
    /// ([`AgentProvider::relocated_executable`]) is recorded on `events`,
    /// on its run when it has one (ADR-t2079-1); without it, only logged.
    pub fn with_events(mut self, events: &'a dyn RunLog) -> Self {
        self.events = Some(events);
        self
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

    /// Session wrappers start in the background through `sessions`.
    pub fn with_sessions(mut self, sessions: &'a dyn SessionWrappers) -> Self {
        self.sessions = Some(sessions);
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

    fn workspaces(&self) -> Result<&'a dyn WorkspaceBackend> {
        self.workspaces.context("this executor opens no workspace")
    }

    fn sessions(&self) -> Result<&'a dyn SessionWrappers> {
        self.sessions
            .context("this executor starts no background session wrapper")
    }

    fn provider(&self) -> Result<&'a dyn AgentProvider> {
        self.provider.context("this executor has no agent provider")
    }

    fn spawner(&self) -> Result<&'a dyn Spawner> {
        self.spawner.context("this executor starts no process")
    }

    /// Spawn `command`, an agent of `provider` for `actor`; when nothing
    /// was found to execute and the provider finds its executable again by
    /// its name (the path it was given is gone, ADR-t2079-1), once more
    /// with that one, which is recorded. Any other failure, and one the
    /// name does not mend, is the spawn's as before.
    fn spawn_agent(
        &self,
        provider: &dyn AgentProvider,
        actor: &ActorContext,
        command: &mut CommandSpec,
        streams: Streams<'_>,
    ) -> Result<Box<dyn Spawned>> {
        let error = match self.spawner()?.spawn(command, streams) {
            Ok(child) => return Ok(child),
            Err(error) => error,
        };
        let not_found = error.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
        });
        let from = PathBuf::from(command.get_program());
        let Some(to) = not_found
            .then(|| provider.relocated_executable())
            .flatten()
            .filter(|to| *to != from)
        else {
            return Err(error);
        };
        command.set_program(&to);
        let child = self.spawner()?.spawn(command, streams)?;
        self.record_relocation(actor, &from, &to);
        Ok(child)
    }

    /// Log and record that `actor`'s agent was started by `to`, found
    /// again by its provider's name, as `from` was gone (ADR-t2079-1).
    fn record_relocation(&self, actor: &ActorContext, from: &Path, to: &Path) {
        tracing::warn!(
            actor_id = actor.actor_id(),
            from = %from.display(),
            to = %to.display(),
            "{} is gone; the agent of {} starts with {}, found again by its provider's name",
            from.display(),
            actor.actor_id(),
            to.display()
        );
        let Some(events) = self.events else {
            return;
        };
        let payload = serde_json::json!({
            "from": from.to_string_lossy(),
            "to": to.to_string_lossy(),
            "actor_id": actor.actor_id(),
            "role": actor.role().as_str(),
        });
        let recorded = match actor.run_id() {
            Some(run) => {
                events.record_runtime_event(run, EventKind::ProviderExecutableRelocated, payload)
            }
            None => events
                .record_queue_event(EventKind::ProviderExecutableRelocated, payload)
                .map(|_| ()),
        };
        if let Err(error) = recorded {
            tracing::warn!(error = %format_args!("{error:#}"), "provider_executable_relocated could not be recorded: {error:#}");
        }
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
                let sessions = self.sessions()?;
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
                Ok(ActorHandle::Workspace(sessions.launch_background(
                    workspace.path(),
                    &wrapper,
                    &env,
                    log,
                )?))
            }
            ActorProgram::NamedWorkspace {
                name,
                cwd,
                command: WorkspaceCommand::Agent { prompt, plugin_dir },
                launch,
                description,
                group,
            } => {
                let command = command_line(&self.provider()?.inbox_command(
                    &prompt,
                    plugin_dir,
                    self.queue.parent().unwrap_or(Path::new(".")),
                )?)?;
                let tags = WorkspaceTags {
                    env: actor_env(self.queue, &actor, None, launch)?,
                    description,
                    group,
                };
                Ok(ActorHandle::Workspace(
                    self.workspaces()?
                        .create_named(name, cwd, &command, &tags)?,
                ))
            }
            ActorProgram::PlannerSession {
                cwd,
                wrapper,
                planner,
                launch,
                log,
            } => {
                let env = actor_env(self.queue, &actor, Some(planner), launch)?;
                Ok(ActorHandle::Workspace(
                    self.sessions()?
                        .launch_background(cwd, &wrapper, &env, log)?,
                ))
            }
            ActorProgram::SessionAgent { agent, model } => {
                let provider = self.provider()?;
                let mut streams = Streams::Inherit;
                let mut without: &[&str] = &[];
                let mut with: &[(String, String)] = &[];
                let mut command = match agent {
                    SessionAgent::Turn {
                        run,
                        prompt,
                        session,
                        stdout,
                        stderr,
                        without_env,
                        with_env,
                    } => {
                        streams = Streams::Files { stdout, stderr };
                        without = without_env;
                        with = with_env;
                        let target = TurnTarget::of_run(run)?;
                        provider.turn_command(&target, prompt, session)?
                    }
                    SessionAgent::Planner(planner) => provider.planner_command(&planner)?,
                    SessionAgent::PlannerTurn {
                        target,
                        prompt,
                        session,
                        stdout,
                        stderr,
                    } => {
                        streams = Streams::Files { stdout, stderr };
                        provider.turn_command(&target, prompt, session)?
                    }
                };
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
                command.envs(with.iter().map(|(key, value)| (key, value)));
                for name in without {
                    command.env_remove(name);
                }
                let child = self
                    .spawn_agent(provider, &actor, &mut command, streams)
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
                self.spawner()?;
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
                let child = self.spawn_agent(provider, &actor, &mut command, streams)?;
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

    impl AgentProvider for Fake {
        fn preflight(&self) -> Result<()> {
            Ok(())
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
        /// `turn <session>`.
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
            let _ = target;
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

    impl SessionWrappers for Fake {
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
        fn stop_background(
            &self,
            _: &str,
            _: crate::domain::background_wrapper::StopRoute,
        ) -> Result<Option<crate::domain::background_wrapper::WrapperStop>> {
            unreachable!()
        }
        fn exists(&self, _: &str) -> Result<bool> {
            unreachable!()
        }
    }

    impl WorkspaceBackend for Fake {
        fn preflight(&self) -> Result<()> {
            Ok(())
        }
        fn preflight_detached(&self, _: &SupervisorEnvironment) -> Result<()> {
            Ok(())
        }
        fn close(&self, _: &str) -> Result<()> {
            unreachable!()
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
        fn exists(&self, _: &str) -> Result<bool> {
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
            Ok("w-named".into())
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

    /// Run `r1` provisioned in `run_dir`, which a worker's turn needs.
    fn provisioned_run(run_dir: &Path) -> TaskRun {
        let text = |name: &str| run_dir.join(name).to_string_lossy().into_owned();
        crate::domain::run::start_provisioning(
            claimed_run("r1"),
            &crate::domain::RunPlan {
                repo_path: "/repo".into(),
                run_dir: text(""),
                branch: "dagq/r1".into(),
                worktree_path: text("worktree"),
                receipt_path: text("receipt.json"),
                log_path: text("log"),
            },
        )
        .unwrap()
    }

    /// A worker's turn of `run` in `session`, its output to fixed files.
    fn turn<'a>(
        run: &'a TaskRun,
        session: crate::domain::turn::TurnSession<'a>,
    ) -> SessionAgent<'a> {
        SessionAgent::Turn {
            run,
            prompt: "work",
            session,
            stdout: Path::new("/r/turn.out"),
            stderr: Path::new("/r/turn.err"),
            without_env: &[],
            with_env: &[],
        }
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
                    agent: turn(&run, crate::domain::turn::TurnSession::Resume("s")),
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
        let run_dir = tempfile::tempdir().unwrap();
        let run = provisioned_run(run_dir.path());
        let fake = Fake::default();
        let executor = HostActorExecutor::new(Path::new("/q/queue.db"))
            .with_sessions(&fake)
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
                    agent: turn(&run, crate::domain::turn::TurnSession::Resume("s")),
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
        assert!(
            error
                .to_string()
                .contains("starts no background session wrapper")
        );
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
            .with_sessions(&fake)
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

    /// A planner of the runtime's opens no workspace (ADR-t1433-2
    /// decision 3): its wrapper starts in the background in the checkout
    /// with the planner's variables and its log, and its handle is the
    /// background wrapper's.
    #[test]
    fn a_planners_wrapper_starts_in_the_background_with_the_planner_env() {
        let fake = Fake::default();
        let handle = HostActorExecutor::new(Path::new("/q/queue.db"))
            .with_sessions(&fake)
            .spawn(ActorExecutionSpec::new(
                ActorContext::instance(ActorRole::Planner, 4),
                WorkspaceAccess::Write("/repo".into()),
                ActorProgram::PlannerSession {
                    cwd: Path::new("/repo"),
                    wrapper: "planner-session --headless --background".into(),
                    planner: (PlannerOrigin::Runtime, PlannerId::new(4)),
                    launch: None,
                    log: Path::new("/q/planners/4/session.log"),
                },
            ))
            .unwrap();
        assert_eq!(handle.workspace().unwrap(), "background:7:start");
        let made = fake.workspaces.lock().unwrap();
        assert_eq!(made.len(), 1);
        assert_eq!(made[0].0, "background /repo /q/planners/4/session.log");
        assert_eq!(made[0].1, "planner-session --headless --background");
        assert_eq!(
            made[0].2.env,
            pairs(&[
                ("DAGQ_ROLE", "planner"),
                ("DAGQ_QUEUE", "/q/queue.db"),
                ("DAGQ_ACTOR_ID", "planner:4"),
                ("DAGQ_SESSION_KIND", "runtime_planner"),
                ("DAGQ_PLANNER_ORIGIN", "runtime"),
                ("DAGQ_PLANNER_ID", "4"),
            ])
        );
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
                    launch: None,
                    description: Some("d".into()),
                    group: None,
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
        let run_dir = tempfile::tempdir().unwrap();
        let run = provisioned_run(run_dir.path());
        let fake = Fake::default();
        let executor = HostActorExecutor::new(Path::new("/q/queue.db"))
            .with_provider(&fake)
            .with_spawner(&fake)
            .with_queue_service(&fake);
        for agent in [
            turn(&run, crate::domain::turn::TurnSession::New("s")),
            turn(&run, crate::domain::turn::TurnSession::Resume("s")),
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
        // The turn of a run the claim issued no token for issues one,
        // once; the next turn takes the one there is.
        assert_eq!(*fake.issued.lock().unwrap(), ["worker:r1"]);
        assert!(fake.revoked.lock().unwrap().is_empty());
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
                    agent: turn(&run, crate::domain::turn::TurnSession::Resume("s")),
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

    /// A provider whose executable `/gone/claude` is not there, and which
    /// finds `found` by its name; the spawner finds nothing at
    /// `/gone/claude` and records every program it was asked to start.
    struct Moved {
        found: Option<PathBuf>,
        started: Mutex<Vec<PathBuf>>,
    }

    impl AgentProvider for Moved {
        fn preflight(&self) -> Result<()> {
            Ok(())
        }
        fn relocated_executable(&self) -> Option<PathBuf> {
            self.found.clone()
        }
        fn review_command(&self, _: &TaskRun, _: &str, _: JobAccess) -> Result<CommandSpec> {
            unreachable!()
        }
        fn headless_command(&self, _: &Path, _: &str, _: JobAccess) -> Result<CommandSpec> {
            Ok(CommandSpec::new("/gone/claude"))
        }
        fn turn_command(
            &self,
            _: &TurnTarget<'_>,
            _: &str,
            _: crate::domain::turn::TurnSession<'_>,
        ) -> Result<CommandSpec> {
            Ok(CommandSpec::new("/gone/claude"))
        }
    }

    impl Spawner for Moved {
        fn spawn(&self, command: &CommandSpec, _: Streams<'_>) -> Result<Box<dyn Spawned>> {
            let program = PathBuf::from(command.get_program());
            self.started.lock().unwrap().push(program.clone());
            if program == Path::new("/gone/claude") {
                return Err(anyhow::Error::new(std::io::Error::from(
                    std::io::ErrorKind::NotFound,
                )));
            }
            Ok(Box::new(Child))
        }
    }

    /// ADR-t2079-1: an agent whose executable is gone at its start (a
    /// planner's turn, a job) starts once more with the one its provider
    /// finds by its name; with none found, the start fails with the error
    /// it met, as before, after one attempt.
    #[test]
    fn an_agent_whose_executable_is_gone_starts_with_the_one_found_by_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let service = Fake::default();
        let moved = Moved {
            found: Some("/bin/claude-new".into()),
            started: Mutex::default(),
        };
        let executor = HostActorExecutor::new(dir.path())
            .with_provider(&moved)
            .with_spawner(&moved)
            .with_queue_service(&service);
        let turn = |executor: &HostActorExecutor<'_>| {
            executor.spawn(ActorExecutionSpec::new(
                ActorContext::instance(ActorRole::Planner, 4),
                WorkspaceAccess::Write(dir.path().to_path_buf()),
                ActorProgram::SessionAgent {
                    agent: SessionAgent::PlannerTurn {
                        target: TurnTarget {
                            role: ActorRole::Planner,
                            dir: dir.path(),
                            cwd: dir.path(),
                            debug_log: None,
                            plugin_dir: None,
                        },
                        prompt: "p",
                        session: crate::domain::turn::TurnSession::New("s"),
                        stdout: dir.path(),
                        stderr: dir.path(),
                    },
                    model: None,
                },
            ))
        };
        turn(&executor).unwrap();
        let job = executor
            .spawn(ActorExecutionSpec::new(
                ActorContext::instance(ActorRole::ThroughputReviewJob, "daily:1"),
                WorkspaceAccess::Scratch(dir.path().to_path_buf()),
                job(dir.path()),
            ))
            .unwrap();
        drop(job);
        assert_eq!(
            *moved.started.lock().unwrap(),
            [
                "/gone/claude",
                "/bin/claude-new",
                "/gone/claude",
                "/bin/claude-new"
            ]
            .map(PathBuf::from)
        );

        let lost = Moved {
            found: None,
            started: Mutex::default(),
        };
        let executor = HostActorExecutor::new(dir.path())
            .with_provider(&lost)
            .with_spawner(&lost);
        let error = turn(&executor).err().unwrap();
        assert!(
            error.chain().any(|cause| cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)),
            "{error:#}"
        );
        assert_eq!(format!("{error}"), "launch agent");
        assert_eq!(
            *lost.started.lock().unwrap(),
            [PathBuf::from("/gone/claude")]
        );
    }
}
