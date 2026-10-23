//! The session wrapper (`session`): the process cmux starts in a run's
//! workspace. It waits for the supervisor to record the workspace,
//! registers itself under the run's lease, starts the agent through the
//! [`Spawner`] with the run's prompt, heartbeats while the agent runs and
//! records its exit (ADR-0007). `resume` reopens the session of a
//! `needs_session` run instead (ADR-0019). A headless worker's wrapper
//! runs its turns instead of one agent (ADR-t813-1, `headless_session`).
//! A wrapper the supervisor started in the background (ADR-t1404-1) has no
//! workspace: it waits for the supervisor's record of its start instead.
//!
//! The runtime starts every wrapper, a worker's and a planner's, in the
//! background only (ADR-t1433-3). The entry that is not in the background
//! (it checks for a terminal, refuses a pipe and waits for the workspace's
//! record) is still accepted for the argv of an older supervisor, and is
//! taken only when `dagq session` is typed without the background flag.
//! On either entry, a wrapper refused its registration logs that and ends
//! with an error, without closing a workspace it may run in or calling
//! cmux ([`wrapper_refused`]).

use crate::domain::LeaseToken;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

use super::headless_session::Turns;
use super::{AgentProvider, ProcessControl, Queue, RunFiles, Spawner};
use crate::domain::queue_service::UseCase;
use crate::domain::session_step::{StepTarget, TargetKind};
use crate::domain::sessions::SessionHook;
use crate::domain::task_replan::Fence;
use crate::domain::{
    Capability, EventKind, PlannerId, PlannerSession, Reason, ReasonCode, Resource, RunEvent,
    RunId, TaskId, TaskRun,
};
use tracing::warn;

/// How long the wrapper waits for the supervisor to record the workspace
/// cmux started it in, or its own start in the background.
const WORKSPACE_REGISTRATION: Duration = Duration::from_secs(45);

/// Where a session wrapper was started: in a cmux workspace, which the
/// run records before the wrapper registers, or in the background without
/// one (ADR-t1404-1), where the supervisor records the wrapper's start
/// (`wrapper_launched`) instead. The supervisor tells a background wrapper
/// so with [`BACKGROUND_FLAG`](crate::domain::background_wrapper::BACKGROUND_FLAG).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WrapperStart {
    #[default]
    Workspace,
    Background,
}

impl WrapperStart {
    /// The start the wrapper's `--background` flag names.
    pub const fn of_flag(background: bool) -> Self {
        if background {
            Self::Background
        } else {
            Self::Workspace
        }
    }

    /// Wait, checking every 100 ms up to [`WORKSPACE_REGISTRATION`], until
    /// `ready` (which reads the queue) says what the wrapper waits for is
    /// recorded: a workspace, or its own start in the background.
    #[expect(
        clippy::disallowed_methods,
        reason = "L5: the wrapper's thin wait bounds its polling of the queue; `ready` decides"
    )]
    pub fn wait(self, mut ready: impl FnMut() -> Result<bool>) -> Result<()> {
        let started = Instant::now();
        loop {
            if ready()? {
                return Ok(());
            }
            ensure!(
                !super::waited_out(started.elapsed(), WORKSPACE_REGISTRATION),
                match self {
                    Self::Workspace => "workspace registration timed out",
                    Self::Background =>
                        "the supervisor recorded no start of this background wrapper in time",
                }
            );
            thread::sleep(Duration::from_millis(100));
        }
    }
}

/// How many times the wrapper tries to record its exit, and the pause
/// before the first retry (doubled before each later one): a transient
/// failure such as SQLITE_BUSY past the busy timeout must not lose the
/// `session_exited` the supervisor waits for. Every error is retried (the
/// application cannot tell a transient one), and with the 5 s busy
/// timeout each attempt may take, the attempts stay well inside the 30 s
/// the supervisor gives a silent wrapper (about 15.6 s at most).
const EXIT_RECORD_ATTEMPTS: u32 = 3;
const EXIT_RECORD_BACKOFF: Duration = Duration::from_millis(200);

/// A wrapper refused its session (`session` names it) with `error`: the
/// refusal is logged and `error` returned. The wrapper ends without
/// touching a workspace it may run in, which is left to whatever opened
/// it.
pub(crate) fn wrapper_refused(session: &str, error: anyhow::Error) -> anyhow::Error {
    warn!(session, error = %format_args!("{error:#}"), "{session} refused its wrapper: {error:#}");
    error
}

/// What the wrapper works with: the queue, the agent, how it is started,
/// the run's files, this process's pid, and where it was started.
pub struct Session<'a> {
    pub queue: &'a mut dyn WrapperStore,
    /// The queue's database, which the executor starts the agent on.
    pub db: &'a Path,
    pub provider: &'a dyn AgentProvider,
    /// The headless agent of the other provider, for a headless run the
    /// supervisor moves there (ADR-t813-2); `None` when there is none.
    pub other: Option<&'a dyn AgentProvider>,
    pub spawner: &'a dyn Spawner,
    /// The queue service's socket and the worker's token, which the agent
    /// is given instead of the queue's path (goal 82's stage (3)).
    pub queue_service: &'a dyn super::queue_service::ServiceAccess,
    /// Lists and signals processes: a headless turn is stopped with its
    /// descendants, which may run outside its group.
    pub processes: &'a dyn ProcessControl,
    pub files: &'a dyn RunFiles,
    pub pid: u32,
    /// Where the supervisor started this wrapper.
    pub start: WrapperStart,
    /// The sccache the wrapper's environment names as `RUSTC_WRAPPER` and
    /// how its server is looked at before each turn (ADR-t2086-1);
    /// `None` names none.
    pub sccache: Option<(
        crate::domain::sccache::SccacheTarget,
        &'a dyn super::SccacheServer,
    )>,
    /// The clock the wrapper's turns read their times on.
    pub clock: &'a dyn super::Clock,
    /// Reads where `dagq.toml` puts the worker, before each turn (goal 38).
    pub runtimes: &'a super::headless_session::RuntimesFile,
    /// Reads the profiles the main checkout's `dagq.toml` declares, before
    /// each turn, which the turn's output is read with.
    pub toolchains: &'a super::headless_session::ToolchainsFile,
    /// The containers the turns of a run in a clone run in (goal 38).
    pub containers: &'a dyn super::run_container::RunContainers,
    /// Where the wrapper sends the steps of its turns, with the run's
    /// telemetry token (ADR-t1662-3 decision 1).
    pub telemetry: &'a dyn super::TelemetrySinks,
}

/// Wrap the agent of run `id` under the lease `token` until it exits; the
/// agent's exit code. A failure is recorded as the run's runtime error,
/// and the wrapper's exit too unless the agent may still be running.
pub fn run_session(
    ctx: Session<'_>,
    id: &RunId,
    token: &LeaseToken,
    resume: bool,
) -> Result<Value> {
    let Session {
        queue,
        db,
        provider,
        other,
        spawner,
        queue_service,
        processes,
        files,
        pid,
        start,
        sccache,
        clock,
        runtimes,
        toolchains,
        containers,
        telemetry,
    } = ctx;
    // A background wrapper finds its start among the run's records by its
    // pid and the start the system shows for it (ADR-t1404-1 decision 2).
    let own_start = match start {
        WrapperStart::Background => processes.start_identity(pid),
        WrapperStart::Workspace => None,
    };
    let run = match register(queue, id, token, pid, resume, start, own_start.as_deref()) {
        Ok(run) => run,
        Err(error) => return Err(wrapper_refused(&format!("run {id}"), error)),
    };
    let mut child_may_be_alive = false;
    // A headless worker runs one call per turn (ADR-t813-1).
    let result = Turns {
        queue: &mut *queue,
        db,
        owner: super::headless_session::TurnOwner::Run(&run),
        provider,
        other,
        spawner,
        queue_service: Some(queue_service),
        processes,
        files,
        pid,
        resume,
        sccache: sccache.as_ref().map(|(target, server)| (target, *server)),
        background: start == WrapperStart::Background,
        clock,
        runtimes: Some(runtimes),
        toolchains: Some(toolchains),
        containers: Some(containers),
        telemetry: Some(telemetry),
        steps: Default::default(),
    }
    .drive(&mut child_may_be_alive);
    match result {
        Ok(code) => {
            record_exit(queue, id, pid, code)?;
            Ok(json!({"run_id": id, "exit_code": code}))
        }
        Err(error) => {
            let _ = queue.record_runtime_error(
                id,
                &format!("{error:#}"),
                &ReasonCode::WrapperFailed.into(),
            );
            if !child_may_be_alive {
                let _ = record_exit(queue, id, pid, 127);
            }
            Err(error)
        }
    }
}

/// Wait for the supervisor to record the run's workspace (or, in the
/// background, this wrapper's start) and register this wrapper under the
/// run's lease; the run.
fn register(
    queue: &mut dyn WrapperStore,
    id: &RunId,
    token: &LeaseToken,
    pid: u32,
    resume: bool,
    start: WrapperStart,
    own_start: Option<&str>,
) -> Result<TaskRun> {
    // cmux may start this command before its create response reaches
    // supervisor. A resumed run keeps the workspace of its first session.
    // A background wrapper's start is recorded after the run records its
    // handle, for each session anew.
    start.wait(|| match start {
        WrapperStart::Workspace => Ok(queue.run(id)?.workspace_id().is_some()),
        WrapperStart::Background => Ok(crate::domain::background_wrapper::launched_as(
            &queue.run_events(id)?,
            pid,
            own_start,
        )),
    })?;
    let run = queue.run(id)?;
    if resume {
        queue.register_resume_wrapper(id, token, pid)?;
    } else {
        queue.register_wrapper(id, token, pid)?;
    }
    Ok(run)
}

/// Record the wrapper's exit with `code`, retrying a failed attempt up to
/// [`EXIT_RECORD_ATTEMPTS`] in all; the last error once they are used up.
fn record_exit(queue: &mut dyn WrapperStore, id: &RunId, pid: u32, code: i32) -> Result<()> {
    retry_exit_record(id, EXIT_RECORD_ATTEMPTS, EXIT_RECORD_BACKOFF, || {
        queue.wrapper_exited(id, pid, code)
    })
}

/// Call `record` until it succeeds or `attempts` calls failed, sleeping
/// `backoff` (doubled each time) between them. Every failure is logged,
/// and the last one says the exit was not recorded.
fn retry_exit_record(
    id: &RunId,
    attempts: u32,
    backoff: Duration,
    mut record: impl FnMut() -> Result<()>,
) -> Result<()> {
    let mut pause = backoff;
    for attempt in 1.. {
        let Err(error) = record() else {
            return Ok(());
        };
        if attempt >= attempts {
            tracing::warn!(
                run_id = %id,
                attempts,
                error = %format_args!("{error:#}"),
                "wrapper exit not recorded after {attempts} attempts: {error:#}"
            );
            return Err(error.context(format!("record wrapper exit after {attempts} attempts")));
        }
        tracing::warn!(
            run_id = %id,
            attempt,
            error = %format_args!("{error:#}"),
            "recording wrapper exit failed, retrying: {error:#}"
        );
        thread::sleep(pause);
        pause = pause.saturating_mul(2);
    }
    unreachable!("the loop returns by the last attempt")
}

// --- What a session wrapper reads and records (ADR-t3102-1 decision 4) ------

/// The queue as a session wrapper and its hook reach it: the reads it
/// waits and starts its turns on, its registration, heartbeats, agents and
/// exit, and its records, a run's or a planner's (ADR-t3102-3 decision 3).
/// The wrapper process reaches it through the queue service as the
/// wrapper's principal ([`SessionOp`]); a test's wrapper in a thread, on
/// the queue itself ([`Queue`] has it). Each method is one transaction.
pub trait WrapperStore {
    fn run(&self, id: &RunId) -> Result<TaskRun>;
    fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>>;
    /// What fences `task`'s run at its turn's boundary now.
    fn turn_fence(&self, task: TaskId) -> Result<Option<Fence>>;
    fn planner(&self, id: PlannerId) -> Result<PlannerSession>;
    fn planner_turn_events(&self, id: PlannerId) -> Result<Vec<RunEvent>>;
    /// The Git common directory the queue is bound to, if any.
    fn repository_binding(&self) -> Result<Option<String>>;
    fn register_wrapper(&mut self, id: &RunId, token: &LeaseToken, pid: u32) -> Result<()>;
    fn register_resume_wrapper(&mut self, id: &RunId, token: &LeaseToken, pid: u32) -> Result<()>;
    fn register_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32) -> Result<()>;
    fn register_resume_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32)
    -> Result<()>;
    fn register_turn_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32) -> Result<()>;
    fn heartbeat_wrapper(&self, id: &RunId, pid: u32) -> Result<()>;
    fn wrapper_exited(&mut self, id: &RunId, pid: u32, exit_code: i32) -> Result<()>;
    fn record_runtime_error(&mut self, id: &RunId, message: &str, reason: &Reason) -> Result<()>;
    fn register_planner_wrapper(&self, id: PlannerId, pid: u32) -> Result<()>;
    fn register_planner_agent(&self, id: PlannerId, wrapper_pid: u32, agent: u32) -> Result<()>;
    fn heartbeat_planner(&self, id: PlannerId, wrapper_pid: u32) -> Result<()>;
    fn planner_exited(&self, id: PlannerId, wrapper_pid: u32, exit_code: i32) -> Result<()>;
    fn record_runtime_event(&self, id: &RunId, kind: EventKind, payload: Value) -> Result<()>;
    /// Record an event of the queue itself, on no task, goal or run: a
    /// planner's wrapper's.
    fn record_queue_event(&self, kind: EventKind, payload: Value) -> Result<()>;
    /// Record what a planner's hook reported, as the hook did on the queue
    /// ([`super::SessionRegistry::record_session_hook`]).
    fn record_session_hook(&self, hook: &SessionHook) -> Result<Value>;
    /// The store as the executor's record of a relocated agent takes it.
    fn as_events(&self) -> &dyn super::actor_executor::ExecutorEvents;
}

impl<T: Queue> WrapperStore for T {
    fn run(&self, id: &RunId) -> Result<TaskRun> {
        super::RunReads::run(self, id)
    }
    fn run_events(&self, id: &RunId) -> Result<Vec<RunEvent>> {
        super::EventStore::run_events(self, id)
    }
    fn turn_fence(&self, task: TaskId) -> Result<Option<Fence>> {
        super::RunTransitions::turn_fence(self, task)
    }
    fn planner(&self, id: PlannerId) -> Result<PlannerSession> {
        super::SessionRegistry::planner(self, id)
    }
    fn planner_turn_events(&self, id: PlannerId) -> Result<Vec<RunEvent>> {
        super::SessionRegistry::planner_turn_events(self, id)
    }
    fn repository_binding(&self) -> Result<Option<String>> {
        super::SupervisorReads::repository_binding(self)
    }
    fn register_wrapper(&mut self, id: &RunId, token: &LeaseToken, pid: u32) -> Result<()> {
        super::RunCoordination::register_wrapper(self, id, token, pid)
    }
    fn register_resume_wrapper(&mut self, id: &RunId, token: &LeaseToken, pid: u32) -> Result<()> {
        super::RunCoordination::register_resume_wrapper(self, id, token, pid)
    }
    fn register_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32) -> Result<()> {
        super::RunCoordination::register_agent(self, id, wrapper_pid, agent_pid)
    }
    fn register_resume_agent(
        &mut self,
        id: &RunId,
        wrapper_pid: u32,
        agent_pid: u32,
    ) -> Result<()> {
        super::RunCoordination::register_resume_agent(self, id, wrapper_pid, agent_pid)
    }
    fn register_turn_agent(&mut self, id: &RunId, wrapper_pid: u32, agent_pid: u32) -> Result<()> {
        super::RunCoordination::register_turn_agent(self, id, wrapper_pid, agent_pid)
    }
    fn heartbeat_wrapper(&self, id: &RunId, pid: u32) -> Result<()> {
        super::RunCoordination::heartbeat_wrapper(self, id, pid)
    }
    fn wrapper_exited(&mut self, id: &RunId, pid: u32, exit_code: i32) -> Result<()> {
        super::RunCoordination::wrapper_exited(self, id, pid, exit_code)
    }
    fn record_runtime_error(&mut self, id: &RunId, message: &str, reason: &Reason) -> Result<()> {
        super::RunTransitions::record_runtime_error(self, id, message, reason)
    }
    fn register_planner_wrapper(&self, id: PlannerId, pid: u32) -> Result<()> {
        super::SessionRegistry::register_planner_wrapper(self, id, pid)
    }
    fn register_planner_agent(&self, id: PlannerId, wrapper_pid: u32, agent: u32) -> Result<()> {
        super::SessionRegistry::register_planner_agent(self, id, wrapper_pid, agent)
    }
    fn heartbeat_planner(&self, id: PlannerId, wrapper_pid: u32) -> Result<()> {
        super::SessionRegistry::heartbeat_planner(self, id, wrapper_pid)
    }
    fn planner_exited(&self, id: PlannerId, wrapper_pid: u32, exit_code: i32) -> Result<()> {
        super::SessionRegistry::planner_exited(self, id, wrapper_pid, exit_code)
    }
    fn record_runtime_event(&self, id: &RunId, kind: EventKind, payload: Value) -> Result<()> {
        super::EventStore::record_runtime_event(self, id, kind, payload)
    }
    fn record_queue_event(&self, kind: EventKind, payload: Value) -> Result<()> {
        super::EventStore::record_queue_event(self, kind, payload).map(drop)
    }
    fn record_session_hook(&self, hook: &SessionHook) -> Result<Value> {
        super::SessionRegistry::record_session_hook(self, hook)
    }
    fn as_events(&self) -> &dyn super::actor_executor::ExecutorEvents {
        self
    }
}

/// How a run's agent registers: its first session's, its resume's, or a
/// later turn's ([`WrapperStore::register_agent`] and the two after it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStart {
    First,
    Resume,
    Turn,
}

/// A [`Reason`] as the service's params carry it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasonParams {
    pub code: crate::domain::ReasonCode,
    #[serde(default)]
    pub detail: serde_json::Map<String, Value>,
}

/// One operation of a session wrapper (or its hook) the queue service does
/// as the wrapper's principal: one [`WrapperStore`] method, one
/// transaction (ADR-t3102-3 decisions 3 and 4). Its use case is
/// [`UseCase::SessionRun`] for the reads and the session's lifecycle and
/// [`UseCase::SessionRecord`] for the records ([`Self::use_case`]); the
/// policy is asked for what [`Self::request`] names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionOp {
    Run {
        run: RunId,
    },
    RunEvents {
        run: RunId,
    },
    TurnFence {
        task: TaskId,
    },
    Planner {
        planner: PlannerId,
    },
    PlannerTurnEvents {
        planner: PlannerId,
    },
    RepositoryBinding,
    RegisterWrapper {
        run: RunId,
        lease: LeaseToken,
        pid: u32,
        resume: bool,
    },
    RegisterAgent {
        run: RunId,
        wrapper_pid: u32,
        agent_pid: u32,
        start: AgentStart,
    },
    HeartbeatWrapper {
        run: RunId,
        pid: u32,
    },
    WrapperExited {
        run: RunId,
        pid: u32,
        exit_code: i32,
    },
    RuntimeError {
        run: RunId,
        message: String,
        reason: ReasonParams,
    },
    RegisterPlannerWrapper {
        planner: PlannerId,
        pid: u32,
    },
    RegisterPlannerAgent {
        planner: PlannerId,
        wrapper_pid: u32,
        agent_pid: u32,
    },
    HeartbeatPlanner {
        planner: PlannerId,
        wrapper_pid: u32,
    },
    PlannerExited {
        planner: PlannerId,
        wrapper_pid: u32,
        exit_code: i32,
    },
    RuntimeEvent {
        run: RunId,
        kind: String,
        payload: Value,
    },
    /// An event of the queue a wrapper of `owner` records.
    QueueEvent {
        owner: StepTarget,
        kind: String,
        payload: Value,
    },
    Hook {
        hook: SessionHook,
    },
}

impl SessionOp {
    /// The use case that carries the operation.
    pub const fn use_case(&self) -> UseCase {
        match self {
            Self::RuntimeEvent { .. } | Self::QueueEvent { .. } | Self::Hook { .. } => {
                UseCase::SessionRecord
            }
            _ => UseCase::SessionRun,
        }
    }

    /// What the policy is asked for the operation: a read of the queue,
    /// `session.run` on the run or the planner of the session's lifecycle,
    /// or `session.record` on that of a record (a hook's of the inbox, on
    /// the queue).
    pub fn request(&self) -> (Capability, Resource) {
        let on_target = |target: &StepTarget| match target.kind {
            TargetKind::Run => target.run_id().map_or(Resource::Unresolved, Resource::run),
            TargetKind::Planner => target.id.parse().map_or(Resource::Unresolved, |id| {
                Resource::Planner(PlannerId::new(id))
            }),
            _ => Resource::Unresolved,
        };
        match self {
            Self::Run { .. }
            | Self::RunEvents { .. }
            | Self::TurnFence { .. }
            | Self::Planner { .. }
            | Self::PlannerTurnEvents { .. }
            | Self::RepositoryBinding => (Capability::QueueRead, Resource::Queue),
            Self::RegisterWrapper { run, .. }
            | Self::RegisterAgent { run, .. }
            | Self::HeartbeatWrapper { run, .. }
            | Self::WrapperExited { run, .. }
            | Self::RuntimeError { run, .. } => {
                (Capability::SessionRun, Resource::run(run.clone()))
            }
            Self::RegisterPlannerWrapper { planner, .. }
            | Self::RegisterPlannerAgent { planner, .. }
            | Self::HeartbeatPlanner { planner, .. }
            | Self::PlannerExited { planner, .. } => {
                (Capability::SessionRun, Resource::Planner(*planner))
            }
            Self::RuntimeEvent { run, .. } => {
                (Capability::SessionRecord, Resource::run(run.clone()))
            }
            Self::QueueEvent { owner, .. } => (Capability::SessionRecord, on_target(owner)),
            Self::Hook { hook } => (
                Capability::SessionRecord,
                match (hook.kind, hook.planner_id) {
                    (crate::domain::sessions::INBOX, _) => Resource::Queue,
                    (_, Some(id)) => Resource::Planner(PlannerId::new(id)),
                    (_, None) => Resource::Unresolved,
                },
            ),
        }
    }
}

/// An event kind the params name, or why it is none this binary records.
fn event_kind(kind: &str) -> Result<EventKind> {
    EventKind::from_name(kind).with_context(|| format!("{kind:?} is no event kind of this binary"))
}

/// Do `op` on `store`, already authorized: its answer, the method's value
/// as JSON under `value` (a unit's is `null`).
pub fn apply(store: &mut dyn WrapperStore, op: &SessionOp) -> Result<Value> {
    let value = match op {
        SessionOp::Run { run } => serde_json::to_value(store.run(run)?)?,
        SessionOp::RunEvents { run } => serde_json::to_value(store.run_events(run)?)?,
        SessionOp::TurnFence { task } => serde_json::to_value(store.turn_fence(*task)?)?,
        SessionOp::Planner { planner } => serde_json::to_value(store.planner(*planner)?)?,
        SessionOp::PlannerTurnEvents { planner } => {
            serde_json::to_value(store.planner_turn_events(*planner)?)?
        }
        SessionOp::RepositoryBinding => serde_json::to_value(store.repository_binding()?)?,
        SessionOp::RegisterWrapper {
            run,
            lease,
            pid,
            resume,
        } => {
            if *resume {
                store.register_resume_wrapper(run, lease, *pid)?;
            } else {
                store.register_wrapper(run, lease, *pid)?;
            }
            Value::Null
        }
        SessionOp::RegisterAgent {
            run,
            wrapper_pid,
            agent_pid,
            start,
        } => {
            match start {
                AgentStart::First => store.register_agent(run, *wrapper_pid, *agent_pid)?,
                AgentStart::Resume => store.register_resume_agent(run, *wrapper_pid, *agent_pid)?,
                AgentStart::Turn => store.register_turn_agent(run, *wrapper_pid, *agent_pid)?,
            }
            Value::Null
        }
        SessionOp::HeartbeatWrapper { run, pid } => {
            store.heartbeat_wrapper(run, *pid)?;
            Value::Null
        }
        SessionOp::WrapperExited {
            run,
            pid,
            exit_code,
        } => {
            store.wrapper_exited(run, *pid, *exit_code)?;
            Value::Null
        }
        SessionOp::RuntimeError {
            run,
            message,
            reason,
        } => {
            let reason = Reason {
                code: reason.code,
                detail: reason.detail.clone(),
            };
            store.record_runtime_error(run, message, &reason)?;
            Value::Null
        }
        SessionOp::RegisterPlannerWrapper { planner, pid } => {
            store.register_planner_wrapper(*planner, *pid)?;
            Value::Null
        }
        SessionOp::RegisterPlannerAgent {
            planner,
            wrapper_pid,
            agent_pid,
        } => {
            store.register_planner_agent(*planner, *wrapper_pid, *agent_pid)?;
            Value::Null
        }
        SessionOp::HeartbeatPlanner {
            planner,
            wrapper_pid,
        } => {
            store.heartbeat_planner(*planner, *wrapper_pid)?;
            Value::Null
        }
        SessionOp::PlannerExited {
            planner,
            wrapper_pid,
            exit_code,
        } => {
            store.planner_exited(*planner, *wrapper_pid, *exit_code)?;
            Value::Null
        }
        SessionOp::RuntimeEvent { run, kind, payload } => {
            store.record_runtime_event(run, event_kind(kind)?, payload.clone())?;
            Value::Null
        }
        SessionOp::QueueEvent {
            owner,
            kind,
            payload,
        } => {
            // A planner's wrapper's events of the queue name its planner,
            // which its turns are read back by; no other wrapper's are.
            ensure!(
                owner.kind == TargetKind::Planner
                    && payload["planner_id"].as_i64() == owner.id.parse::<i64>().ok(),
                "a session wrapper records events of the queue only for its planner, named \
                 as their planner_id"
            );
            store.record_queue_event(event_kind(kind)?, payload.clone())?;
            Value::Null
        }
        SessionOp::Hook { hook } => store.record_session_hook(hook)?,
    };
    Ok(json!({ "value": value }))
}

/// The directory of a planner's directory `dir` its hook leaves its reports
/// in for its wrapper to relay ([`spool_hook`], [`relay_hooks`]).
pub const HOOK_SPOOL_DIR: &str = "session-hooks";

/// Leave `hook`, a planner's hook's report, in its planner's directory
/// `dir` for the planner's wrapper to record as its own principal
/// (ADR-t3102-1 decision 4): the hook runs as the planner's agent, whose
/// policy records no session. Written whole under a name of its own,
/// `stamp` (the time and the process) first, so the wrapper relays the
/// reports in the order they came. The file it left.
pub fn spool_hook(
    files: &dyn RunFiles,
    dir: &Path,
    hook: &SessionHook,
    stamp: &str,
) -> Result<std::path::PathBuf> {
    let spool = dir.join(HOOK_SPOOL_DIR);
    files.create_dir_all(&spool)?;
    let path = spool.join(format!("{stamp}.json"));
    let partial = spool.join(format!("{stamp}.partial"));
    files.write(&partial, &serde_json::to_vec(hook)?)?;
    files
        .rename(&partial, &path)
        .with_context(|| format!("leave the hook's report at {}", path.display()))?;
    Ok(path)
}

/// Record, in the order they came, the reports the hook of `planner` left
/// in its directory `dir` ([`spool_hook`]), each removed once it is
/// recorded; how many were. The first that cannot be recorded stops the
/// relay, and it and the later ones are relayed at the next call. One that
/// does not read, or names another planner, is warned of and removed.
pub fn relay_hooks(
    files: &dyn RunFiles,
    dir: &Path,
    planner: PlannerId,
    store: &dyn WrapperStore,
) -> usize {
    let spool = dir.join(HOOK_SPOOL_DIR);
    let Ok(mut reports) = files.read_dir(&spool) else {
        return 0;
    };
    reports.retain(|path| path.extension().is_some_and(|ext| ext == "json"));
    reports.sort();
    let mut relayed = 0;
    for report in reports {
        let hook = files
            .read_to_string(&report)
            .map_err(anyhow::Error::from)
            .and_then(|text| Ok(serde_json::from_str::<SessionHook>(&text)?));
        let hook = hook.and_then(|hook| {
            ensure!(
                hook.planner_id == Some(planner.as_i64()),
                "it names another planner than {planner}"
            );
            Ok(hook)
        });
        let hook = match hook {
            Ok(hook) => hook,
            Err(error) => {
                warn!(report = %report.display(), error = %format_args!("{error:#}"), "the hook's report {} does not read and is dropped: {error:#}", report.display());
                let _ = files.remove_file(&report);
                continue;
            }
        };
        if let Err(error) = store.record_session_hook(&hook) {
            warn!(report = %report.display(), error = %format_args!("{error:#}"), "the hook's report {} could not be recorded yet: {error:#}", report.display());
            break;
        }
        let _ = files.remove_file(&report);
        relayed += 1;
    }
    relayed
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::sync::{Arc, Mutex};
    use tracing::{Event, Subscriber, field::Field, field::Visit};
    use tracing_subscriber::{Layer, layer::Context as LayerContext, prelude::*};

    /// Collects the `message` of every event.
    #[derive(Clone, Default)]
    struct Messages(Arc<Mutex<Vec<String>>>);

    impl<S: Subscriber> Layer<S> for Messages {
        fn on_event(&self, event: &Event<'_>, _: LayerContext<'_, S>) {
            struct Message(String);
            impl Visit for Message {
                fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                    if field.name() == "message" {
                        self.0 = format!("{value:?}");
                    }
                }
            }
            let mut message = Message(String::new());
            event.record(&mut message);
            self.0.lock().unwrap().push(message.0);
        }
    }

    fn logged<T>(work: impl FnOnce() -> T) -> (T, Vec<String>) {
        let messages = Messages::default();
        let subscriber = tracing_subscriber::registry().with(messages.clone());
        let result = tracing::subscriber::with_default(subscriber, work);
        let lines = messages.0.lock().unwrap().clone();
        (result, lines)
    }

    /// A store that records the hooks it is given, and fails the one
    /// whose session is `fail`; every other method is not called.
    #[derive(Default)]
    struct Hooks {
        recorded: std::cell::RefCell<Vec<String>>,
        fail: Option<&'static str>,
    }

    impl WrapperStore for Hooks {
        fn run(&self, _: &RunId) -> Result<TaskRun> {
            unreachable!()
        }
        fn run_events(&self, _: &RunId) -> Result<Vec<RunEvent>> {
            unreachable!()
        }
        fn turn_fence(&self, _: TaskId) -> Result<Option<Fence>> {
            unreachable!()
        }
        fn planner(&self, _: PlannerId) -> Result<PlannerSession> {
            unreachable!()
        }
        fn planner_turn_events(&self, _: PlannerId) -> Result<Vec<RunEvent>> {
            unreachable!()
        }
        fn repository_binding(&self) -> Result<Option<String>> {
            unreachable!()
        }
        fn register_wrapper(&mut self, _: &RunId, _: &LeaseToken, _: u32) -> Result<()> {
            unreachable!()
        }
        fn register_resume_wrapper(&mut self, _: &RunId, _: &LeaseToken, _: u32) -> Result<()> {
            unreachable!()
        }
        fn register_agent(&mut self, _: &RunId, _: u32, _: u32) -> Result<()> {
            unreachable!()
        }
        fn register_resume_agent(&mut self, _: &RunId, _: u32, _: u32) -> Result<()> {
            unreachable!()
        }
        fn register_turn_agent(&mut self, _: &RunId, _: u32, _: u32) -> Result<()> {
            unreachable!()
        }
        fn heartbeat_wrapper(&self, _: &RunId, _: u32) -> Result<()> {
            unreachable!()
        }
        fn wrapper_exited(&mut self, _: &RunId, _: u32, _: i32) -> Result<()> {
            unreachable!()
        }
        fn record_runtime_error(&mut self, _: &RunId, _: &str, _: &Reason) -> Result<()> {
            unreachable!()
        }
        fn register_planner_wrapper(&self, _: PlannerId, _: u32) -> Result<()> {
            unreachable!()
        }
        fn register_planner_agent(&self, _: PlannerId, _: u32, _: u32) -> Result<()> {
            unreachable!()
        }
        fn heartbeat_planner(&self, _: PlannerId, _: u32) -> Result<()> {
            unreachable!()
        }
        fn planner_exited(&self, _: PlannerId, _: u32, _: i32) -> Result<()> {
            unreachable!()
        }
        fn record_runtime_event(&self, _: &RunId, _: EventKind, _: Value) -> Result<()> {
            unreachable!()
        }
        fn record_queue_event(&self, _: EventKind, _: Value) -> Result<()> {
            unreachable!()
        }
        fn record_session_hook(&self, hook: &SessionHook) -> Result<Value> {
            if self.fail == Some(hook.session_id.as_str()) {
                return Err(anyhow!("the queue service is unreachable"));
            }
            self.recorded.borrow_mut().push(hook.session_id.clone());
            Ok(json!({}))
        }
        fn as_events(&self) -> &dyn crate::application::actor_executor::ExecutorEvents {
            unreachable!()
        }
    }

    fn hook(session: &str) -> SessionHook {
        SessionHook::from_hook(
            "open",
            &json!({"session_id": session, "source": "startup"}),
            |name| match name {
                "DAGQ_ROLE" => Some("planner".into()),
                "DAGQ_PLANNER_ORIGIN" => Some("runtime".into()),
                "DAGQ_PLANNER_ID" => Some("3".into()),
                _ => None,
            },
        )
        .unwrap()
        .unwrap()
    }

    /// ADR-t3102-1 decision 4: a planner's hook leaves its reports in the
    /// planner's directory, and its wrapper records them in the order they
    /// came, each once; one it cannot record yet stops the relay until the
    /// next, and one that does not read is dropped.
    #[test]
    fn a_planners_hook_is_relayed_by_its_wrapper_in_order() {
        let files = crate::application::memory_files::MemoryFiles::default();
        let dir = Path::new("/q/planners/3");
        for (stamp, session) in [("02", "b"), ("01", "a"), ("03", "c")] {
            let left = spool_hook(&files, dir, &hook(session), stamp).unwrap();
            assert_eq!(left, dir.join(HOOK_SPOOL_DIR).join(format!("{stamp}.json")));
        }
        files
            .write(&dir.join(HOOK_SPOOL_DIR).join("00.json"), b"not a hook")
            .unwrap();
        let store = Hooks {
            fail: Some("b"),
            ..Hooks::default()
        };
        assert_eq!(relay_hooks(&files, dir, PlannerId::new(3), &store), 1);
        assert_eq!(*store.recorded.borrow(), ["a"]);
        let store = Hooks::default();
        assert_eq!(relay_hooks(&files, dir, PlannerId::new(3), &store), 2);
        assert_eq!(*store.recorded.borrow(), ["b", "c"]);
        assert_eq!(relay_hooks(&files, dir, PlannerId::new(3), &store), 0);
        assert!(
            files
                .read_dir(&dir.join(HOOK_SPOOL_DIR))
                .unwrap()
                .is_empty()
        );
        // A planner with no report left relays nothing.
        assert_eq!(
            relay_hooks(
                &files,
                Path::new("/q/planners/4"),
                PlannerId::new(4),
                &store
            ),
            0
        );
        // A report that names another planner is dropped, not recorded.
        spool_hook(&files, dir, &hook("d"), "04").unwrap();
        assert_eq!(relay_hooks(&files, dir, PlannerId::new(5), &store), 0);
        assert!(
            files
                .read_dir(&dir.join(HOOK_SPOOL_DIR))
                .unwrap()
                .is_empty()
        );
        assert_eq!(*store.recorded.borrow(), ["b", "c"]);
    }

    /// ADR-t3102-3 decision 3: each operation reads back as written, goes
    /// with its own use case, and asks the policy for its own run or
    /// planner (a read, of the queue).
    #[test]
    fn a_session_op_names_its_use_case_and_what_the_policy_is_asked() {
        let run = RunId::new("r1").unwrap();
        let planner = Resource::Planner(PlannerId::new(3));
        for (op, use_case, request) in [
            (
                SessionOp::Run { run: run.clone() },
                UseCase::SessionRun,
                (Capability::QueueRead, Resource::Queue),
            ),
            (
                SessionOp::RegisterWrapper {
                    run: run.clone(),
                    lease: LeaseToken::new("l"),
                    pid: 7,
                    resume: true,
                },
                UseCase::SessionRun,
                (Capability::SessionRun, Resource::run(run.clone())),
            ),
            (
                SessionOp::RuntimeError {
                    run: run.clone(),
                    message: "m".into(),
                    reason: ReasonParams {
                        code: ReasonCode::WrapperFailed,
                        detail: serde_json::Map::new(),
                    },
                },
                UseCase::SessionRun,
                (Capability::SessionRun, Resource::run(run.clone())),
            ),
            (
                SessionOp::HeartbeatPlanner {
                    planner: PlannerId::new(3),
                    wrapper_pid: 7,
                },
                UseCase::SessionRun,
                (Capability::SessionRun, planner.clone()),
            ),
            (
                SessionOp::RuntimeEvent {
                    run: run.clone(),
                    kind: "turn_started".into(),
                    payload: json!({}),
                },
                UseCase::SessionRecord,
                (Capability::SessionRecord, Resource::run(run.clone())),
            ),
            (
                SessionOp::QueueEvent {
                    owner: StepTarget::of(TargetKind::Planner, 3),
                    kind: "turn_started".into(),
                    payload: json!({}),
                },
                UseCase::SessionRecord,
                (Capability::SessionRecord, planner.clone()),
            ),
            (
                SessionOp::Hook { hook: hook("s") },
                UseCase::SessionRecord,
                (Capability::SessionRecord, planner.clone()),
            ),
        ] {
            assert_eq!(op.use_case(), use_case, "{op:?}");
            assert_eq!(op.request(), request, "{op:?}");
            let written = serde_json::to_value(&op).unwrap();
            assert_eq!(serde_json::from_value::<SessionOp>(written).unwrap(), op);
        }
        let mut inbox = hook("s");
        inbox.kind = crate::domain::sessions::INBOX;
        assert_eq!(
            SessionOp::Hook { hook: inbox }.request(),
            (Capability::SessionRecord, Resource::Queue)
        );
        assert!(
            serde_json::from_value::<SessionOp>(json!({"op": "run", "run": "r1", "x": 1})).is_err()
        );
        assert!(
            serde_json::from_value::<SessionOp>(json!({"op": "hook", "hook": {
                "event": {"hook": "start", "source": "startup"}, "kind": "worker",
                "session_id": "s"}}))
            .is_err()
        );
        // An event kind this binary does not know is refused, not written.
        let mut store = Hooks::default();
        assert!(
            apply(
                &mut store,
                &SessionOp::QueueEvent {
                    owner: StepTarget::of(TargetKind::Planner, 3),
                    kind: "no_such_kind".into(),
                    payload: json!({}),
                },
            )
            .is_err()
        );
        // Nor one of the queue that names no planner, or another one.
        for payload in [json!({}), json!({"planner_id": 4})] {
            assert!(
                apply(
                    &mut store,
                    &SessionOp::QueueEvent {
                        owner: StepTarget::of(TargetKind::Planner, 3),
                        kind: "turn_started".into(),
                        payload,
                    },
                )
                .is_err()
            );
        }
        let answer = apply(&mut store, &SessionOp::Hook { hook: hook("s") }).unwrap();
        assert_eq!(answer, json!({"value": {}}));
    }

    #[test]
    fn a_wrapper_waits_until_what_it_waits_for_is_recorded() {
        let mut reads = 0;
        WrapperStart::Background
            .wait(|| {
                reads += 1;
                Ok(reads == 3)
            })
            .unwrap();
        assert_eq!(reads, 3);
        assert!(
            WrapperStart::Workspace
                .wait(|| Err(anyhow!("queue gone")))
                .is_err()
        );
        assert_eq!(WrapperStart::of_flag(true), WrapperStart::Background);
        assert_eq!(WrapperStart::of_flag(false), WrapperStart::Workspace);
    }

    fn run_id() -> RunId {
        RunId::try_from("3aa21145-c873-4cec-aee3-ee7f07f52e4a").unwrap()
    }

    #[test]
    fn a_transient_failure_to_record_the_exit_is_retried() {
        let mut calls = 0;
        let (result, lines) = logged(|| {
            retry_exit_record(&run_id(), 5, Duration::ZERO, || {
                calls += 1;
                if calls < 3 {
                    Err(anyhow!("database is locked"))
                } else {
                    Ok(())
                }
            })
        });
        result.unwrap();
        assert_eq!(calls, 3);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].contains("retrying: database is locked"),
            "{lines:?}"
        );
    }

    #[test]
    fn the_exit_left_unrecorded_after_the_last_attempt_is_logged() {
        let mut calls = 0;
        let (result, lines) = logged(|| {
            retry_exit_record(&run_id(), 3, Duration::ZERO, || {
                calls += 1;
                Err(anyhow!("database is locked"))
            })
        });
        let error = result.unwrap_err();
        assert_eq!(calls, 3);
        assert!(
            format!("{error:#}").contains("after 3 attempts: database is locked"),
            "{error:#}"
        );
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[2].contains("wrapper exit not recorded after 3 attempts: database is locked"),
            "{lines:?}"
        );
    }
}
