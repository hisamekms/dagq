//! The session wrapper (`session`): the process cmux starts in a run's
//! workspace. It waits for the supervisor to record the workspace,
//! registers itself under the run's lease, starts the agent through the
//! [`Spawner`] with the run's prompt, heartbeats while the agent runs and
//! records its exit (ADR-0007). `resume` reopens the session of a
//! `needs_session` run instead (ADR-0019). A headless worker's wrapper
//! runs its turns instead of one agent (ADR-t813-1, `headless_session`).
//! A wrapper the supervisor started in the background (ADR-t1404-1) has no
//! workspace: it waits for the supervisor's record of its start instead.

use crate::domain::LeaseToken;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use super::headless_session::Turns;
use super::{
    AgentProvider, ProcessControl, Queue, RunFiles, Spawner, WorkspaceBackend,
    actor_executor::{
        ActorExecutionSpec, ActorExecutor, ActorProgram, HostActorExecutor, SessionAgent,
        WorkspaceAccess,
    },
};
use crate::domain::{
    ActorContext, ReasonCode, RunId, TaskRun, run::run_workspaces, worker::WorkerMode,
    worker_model::WorkerSession,
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
    pub fn wait(self, mut ready: impl FnMut() -> Result<bool>) -> Result<()> {
        let started = Instant::now();
        loop {
            if ready()? {
                return Ok(());
            }
            ensure!(
                started.elapsed() < WORKSPACE_REGISTRATION,
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

/// The cmux workspace a session wrapper runs in (`CMUX_WORKSPACE_ID`) and
/// the backend that closes it, for a wrapper refused its session (task
/// 806).
pub struct OwnWorkspace<'a> {
    pub backend: &'a dyn WorkspaceBackend,
    pub id: String,
}

/// A wrapper refused its session (`session` names it) with `error`: the
/// refusal is logged, and the wrapper's own workspace is closed when
/// `recorded` says nothing records it for the session. Such a workspace is
/// one cmux made although the create reported failing (a create that
/// timed out, task 806), or opened for a session that ended before its
/// wrapper started: nothing would ever find it to close it. A workspace
/// the session records is left to whatever ends the session, and one
/// whose record cannot be read is left too. The error says what became of
/// the workspace.
pub(crate) fn wrapper_refused(
    own: Option<&OwnWorkspace<'_>>,
    recorded: impl FnOnce(&str) -> Result<bool>,
    session: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    let Some(own) = own else {
        warn!(session, error = %format_args!("{error:#}"), "{session} refused its wrapper, which knows no workspace of its own: {error:#}");
        return error;
    };
    let workspace = own.id.as_str();
    match recorded(workspace) {
        Ok(true) => {
            warn!(session, workspace_id = workspace, error = %format_args!("{error:#}"), "{session} refused its wrapper; its workspace {workspace} is the session's and is left open: {error:#}");
            error
        }
        Err(read) => {
            warn!(session, workspace_id = workspace, error = %format_args!("{error:#}"), "{session} refused its wrapper; whether it records workspace {workspace} could not be read ({read:#}), so it is left open: {error:#}");
            error
        }
        Ok(false) => match own.backend.close(workspace) {
            Ok(()) => {
                warn!(session, workspace_id = workspace, error = %format_args!("{error:#}"), "{session} refused its wrapper; closed its workspace {workspace}, which nothing records: {error:#}");
                error.context(format!(
                    "{session} refused this wrapper; its workspace {workspace}, which nothing records, was closed"
                ))
            }
            Err(close) => {
                warn!(session, workspace_id = workspace, error = %format_args!("{error:#}"), "{session} refused its wrapper; its workspace {workspace}, which nothing records, could not be closed ({close:#}): {error:#}");
                error.context(format!(
                    "{session} refused this wrapper; its workspace {workspace}, which nothing records, could not be closed: {close:#}"
                ))
            }
        },
    }
}

/// What the wrapper works with: the queue, the agent, how it is started,
/// the run's files, this process's pid, and the workspace it runs in.
pub struct Session<'a> {
    pub queue: &'a mut dyn Queue,
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
    /// Closed when the run refuses this wrapper and records no such
    /// workspace ([`wrapper_refused`]); `None` outside cmux and in the
    /// background.
    pub own_workspace: Option<OwnWorkspace<'a>>,
    /// Where the supervisor started this wrapper.
    pub start: WrapperStart,
    /// The sccache the wrapper's environment names as `RUSTC_WRAPPER` and
    /// how its server is looked at before a Codex turn (ADR-t1215-1);
    /// `None` names none.
    pub sccache: Option<(
        crate::domain::sccache::SccacheTarget,
        &'a dyn super::SccacheServer,
    )>,
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
        own_workspace,
        start,
        sccache,
    } = ctx;
    // A background wrapper finds its start among the run's records by its
    // pid and the start the system shows for it (ADR-t1404-1 decision 2).
    let own_start = match start {
        WrapperStart::Background => processes.start_identity(pid),
        WrapperStart::Workspace => None,
    };
    let run = match register(queue, id, token, pid, resume, start, own_start.as_deref()) {
        Ok(run) => run,
        Err(error) => {
            let recorded = |workspace: &str| -> Result<bool> {
                let run = queue.run(id)?;
                let events = queue.run_events(id)?;
                Ok(run_workspaces(&run, &events)
                    .iter()
                    .any(|w| w.workspace_id == workspace))
            };
            return Err(wrapper_refused(
                own_workspace.as_ref(),
                recorded,
                &format!("run {id}"),
                error,
            ));
        }
    };
    let mut child_may_be_alive = false;
    // A headless worker runs one call per turn (ADR-t813-1).
    let result = if run.worker_mode() == WorkerMode::Headless {
        Turns {
            queue: &mut *queue,
            db,
            run: &run,
            provider,
            other,
            spawner,
            queue_service,
            processes,
            files,
            pid,
            resume,
            sccache: sccache.as_ref().map(|(target, server)| (target, *server)),
        }
        .drive(&mut child_may_be_alive)
    } else {
        drive_agent(
            queue,
            db,
            &run,
            provider,
            spawner,
            queue_service,
            files,
            pid,
            resume,
            &mut child_may_be_alive,
        )
    };
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
    queue: &mut dyn Queue,
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
fn record_exit(queue: &mut dyn Queue, id: &RunId, pid: u32, code: i32) -> Result<()> {
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

#[allow(clippy::too_many_arguments)]
fn drive_agent(
    queue: &mut dyn Queue,
    db: &Path,
    run: &TaskRun,
    provider: &dyn AgentProvider,
    spawner: &dyn Spawner,
    queue_service: &dyn super::queue_service::ServiceAccess,
    files: &dyn RunFiles,
    pid: u32,
    resume: bool,
    child_may_be_alive: &mut bool,
) -> Result<i32> {
    let prompt = if resume {
        None
    } else {
        let prompt_path =
            Path::new(run.run_dir().context("missing run directory")?).join("prompt.txt");
        Some(files.read_to_string(&prompt_path)?)
    };
    let agent = match &prompt {
        None => SessionAgent::Resume { run },
        Some(prompt) => SessionAgent::Worker { run, prompt },
    };
    // The model and effort the claim chose (ADR-0079 decision 3), or the
    // ones the resume recorded, raised after a failure the task caused
    // (decision 5).
    let session = WorkerSession::current(&queue.run_events(run.id())?);
    let mut child = HostActorExecutor::new(db)
        .with_provider(provider)
        .with_spawner(spawner)
        .with_queue_service(queue_service)
        .spawn(ActorExecutionSpec::new(
            ActorContext::worker(run.id(), run.task_id()),
            WorkspaceAccess::Write(PathBuf::from(
                run.worktree_path().context("missing worktree")?,
            )),
            ActorProgram::SessionAgent {
                agent,
                model: Some((&session.model, &session.effort)),
            },
        ))?
        .process()?;
    *child_may_be_alive = true;
    let registered = if resume {
        queue.register_resume_agent(run.id(), pid, child.id())
    } else {
        queue.register_agent(run.id(), pid, child.id())
    };
    if let Err(error) = registered {
        let _ = child.kill();
        if child.wait().is_ok() {
            *child_may_be_alive = false;
        }
        return Err(error);
    }
    loop {
        if let Some(status) = child.try_wait()? {
            *child_may_be_alive = false;
            return Ok(status.code.unwrap_or(128));
        }
        if let Err(error) = queue.heartbeat_wrapper(run.id(), pid) {
            // Keep owning/waiting on the existing child even during a DB outage.
            tracing::warn!(
                run_id = %run.id(),
                error = %format_args!("{error:#}"),
                "wrapper heartbeat failed: {error:#}"
            );
        }
        thread::sleep(provider.wait_interval());
    }
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
