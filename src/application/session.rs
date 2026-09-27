//! The session wrapper (`session`): the process cmux starts in a run's
//! workspace. It waits for the supervisor to record the workspace,
//! registers itself under the run's lease, starts the agent through the
//! [`Spawner`] with the run's prompt, heartbeats while the agent runs and
//! records its exit (ADR-0007). `resume` reopens the session of a
//! `needs_session` run instead (ADR-0019).

use crate::domain::LeaseToken;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

use super::{AgentProvider, Queue, RunFiles, Spawner, Streams};
use crate::domain::{ReasonCode, RunId, TaskRun, worker_model::WorkerSession};

/// How long the wrapper waits for the supervisor to record the workspace
/// cmux started it in.
const WORKSPACE_REGISTRATION: Duration = Duration::from_secs(45);

/// How many times the wrapper tries to record its exit, and the pause
/// before the first retry (doubled before each later one): a transient
/// failure such as SQLITE_BUSY past the busy timeout must not lose the
/// `session_exited` the supervisor waits for. Every error is retried (the
/// application cannot tell a transient one), and with the 5 s busy
/// timeout each attempt may take, the attempts stay well inside the 30 s
/// the supervisor gives a silent wrapper (about 15.6 s at most).
const EXIT_RECORD_ATTEMPTS: u32 = 3;
const EXIT_RECORD_BACKOFF: Duration = Duration::from_millis(200);

/// What the wrapper works with: the queue, the agent, how it is started,
/// the run's files, and this process's pid.
pub struct Session<'a> {
    pub queue: &'a mut dyn Queue,
    pub provider: &'a dyn AgentProvider,
    pub spawner: &'a dyn Spawner,
    pub files: &'a dyn RunFiles,
    pub pid: u32,
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
        provider,
        spawner,
        files,
        pid,
    } = ctx;
    let started = Instant::now();
    // cmux may start this command before its create response reaches
    // supervisor. A resumed run keeps the workspace of its first session.
    let run = loop {
        let run = queue.run(id)?;
        if run.workspace_id().is_some() {
            break run;
        }
        ensure!(
            started.elapsed() < WORKSPACE_REGISTRATION,
            "workspace registration timed out"
        );
        thread::sleep(Duration::from_millis(100));
    };
    if resume {
        queue.register_resume_wrapper(id, token, pid)?;
    } else {
        queue.register_wrapper(id, token, pid)?;
    }
    let mut child_may_be_alive = false;
    let result = drive_agent(
        queue,
        &run,
        provider,
        spawner,
        files,
        pid,
        resume,
        &mut child_may_be_alive,
    );
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
    run: &TaskRun,
    provider: &dyn AgentProvider,
    spawner: &dyn Spawner,
    files: &dyn RunFiles,
    pid: u32,
    resume: bool,
    child_may_be_alive: &mut bool,
) -> Result<i32> {
    let mut command = if resume {
        provider.resume_command(run)?
    } else {
        let prompt_path =
            Path::new(run.run_dir().context("missing run directory")?).join("prompt.txt");
        provider.command(run, &files.read_to_string(&prompt_path)?)?
    };
    // The model and effort the claim chose (ADR-0079 decision 3); a resume
    // keeps them.
    let session = WorkerSession::of_run(&queue.run_events(run.id())?);
    provider.select_model(&mut command, &session.model, &session.effort);
    let mut child = spawner
        .spawn(&command, Streams::Inherit)
        .context("launch agent")?;
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
