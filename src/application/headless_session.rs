//! The session wrapper of a headless worker (ADR-t813-1 decisions 1 to 3):
//! instead of one interactive agent, it runs one non-interactive call per
//! turn in the run's workspace. The first turn starts the session named by
//! the run's id with the task's prompt; every later one resumes it with the
//! prompt of the next request the supervisor wrote to the run's `turns/`
//! directory ([`crate::domain::turn`]), where it would have typed into an
//! interactive session. The provider's [`TurnReader`] reads each turn's
//! output; the wrapper stops a turn that falls silent, runs past its limit,
//! starts otherwise than asked or says its provider cannot be used, records
//! `turn_started` and `turn_finished`, and writes the run's idle marker
//! when the turn's process ended. The exit request ends the wrapper (and
//! stops a turn that runs). A turn that failed or was stopped ends the
//! session, so that the run goes to its recovery job as a run that failed;
//! one that failed at the provider's login or usage limit does not, and
//! the supervisor holds the queue for a person as it does for an
//! interactive session. A turn whose agent did not start is one too
//! (ADR-t813-2): the supervisor then moves the run to the other provider,
//! and the wrapper runs the next request as the first turn of a new session
//! of that provider in the same worktree (the run's `actual_provider` says
//! which, read before every turn).

use crate::domain::EventKind;
use anyhow::{Context, Result};
use serde_json::json;
use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
    thread,
    time::Instant,
};

use super::{
    AgentProvider, ProcessControl, Queue, RunFiles, Spawned, Spawner, TurnReader,
    actor_executor::{
        ActorExecutionSpec, ActorExecutor, ActorProgram, HostActorExecutor, SessionAgent,
        WorkspaceAccess,
    },
};
use crate::domain::{
    ActorContext, Provider, TaskRun, event_kind,
    provider_switch::{since_switch, switches},
    stall::StallConfig,
    tokens::TokenUsage,
    turn::{
        LIMITS_FILE, TurnFailure, TurnLimits, TurnOutcome, TurnRequest, TurnResult, TurnSession,
        TurnSignal, exit_path, idle_marker, output_path, pending, request_path, session_name,
        taken_path, turns_dir,
    },
    worker_model::WorkerSession,
};

/// What the wrapper of a headless worker works with.
pub(super) struct Turns<'a> {
    pub(super) queue: &'a mut dyn Queue,
    pub(super) db: &'a Path,
    pub(super) run: &'a TaskRun,
    /// The agent of the run's provider when the wrapper started.
    pub(super) provider: &'a dyn AgentProvider,
    /// The headless agent of the other provider, which the run's turns go
    /// to once the supervisor moved it there (ADR-t813-2); `None` when the
    /// binary has none.
    pub(super) other: Option<&'a dyn AgentProvider>,
    pub(super) spawner: &'a dyn Spawner,
    /// Lists the turn's descendants and signals them ([`stop_turn`]).
    pub(super) processes: &'a dyn ProcessControl,
    pub(super) files: &'a dyn RunFiles,
    pub(super) pid: u32,
    /// The wrapper of a `needs_session` run's resume: it waits for the
    /// supervisor's request instead of starting with the task's prompt.
    pub(super) resume: bool,
}

/// The session of the provider a run is on, as its events since it last
/// moved to that provider say (ADR-t813-2 decision 4).
struct Agent {
    provider: Provider,
    /// A turn of it had its model answer: there is a conversation to
    /// resume.
    created: bool,
    /// The session the agent said it started (Codex's thread), the last
    /// recorded.
    identified: Option<String>,
    /// The name of its session for a provider that takes one (Claude): the
    /// run's id until the run first switched, a name of its own after.
    name: String,
}

impl Agent {
    fn new(run: &TaskRun, provider: Provider, events: &[crate::domain::RunEvent]) -> Self {
        let current = since_switch(events);
        Self {
            provider,
            created: current.iter().any(|e| {
                e.kind == event_kind::TURN_FINISHED && e.payload["session_created"] == true
            }),
            identified: current
                .iter()
                .rfind(|e| e.kind == event_kind::TURN_SESSION_IDENTIFIED)
                .and_then(|e| e.payload["session_id"].as_str().map(str::to_owned)),
            name: session_name(run.id().as_str(), switches(events)),
        }
    }
}

/// How a turn ended.
struct Turn {
    outcome: TurnOutcome,
    failure: Option<TurnFailure>,
    result: TurnResult,
}

/// Why the wrapper stopped a turn that still ran.
struct Stop {
    outcome: TurnOutcome,
    failure: Option<TurnFailure>,
    why: String,
}

/// Stop a turn that still runs: SIGKILL to its process group and to each
/// of its descendants by pid. A provider may run a command in a group of
/// its own (Codex does: its pgid is the command's pid), which a signal to
/// the turn's group does not reach; the descendants are listed before the
/// group is killed, as a descendant whose parent was killed has 1 for a
/// parent and is no longer found (task 1085).
fn stop_turn(processes: &dyn ProcessControl, child: &mut dyn Spawned) -> Result<()> {
    let descendants = processes.descendants(child.id());
    child.kill_group()?;
    for pid in descendants {
        // One that ended since it was listed has nothing left to stop.
        let _ = processes.kill(pid);
    }
    Ok(())
}

/// Whether starting a turn failed because its executable could not be run
/// (not found, not executable), rather than for anything else the wrapper
/// met on the way (the run's settings, its worktree).
fn executable_unrunnable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            )
        })
    })
}

/// The lines of a file another process appends to, read as they come.
#[derive(Default)]
struct Tail {
    offset: u64,
    partial: Vec<u8>,
}

impl Tail {
    /// The complete lines written since the last read; with `rest`, also
    /// a last line without its line break.
    fn read(&mut self, files: &dyn RunFiles, path: &Path, rest: bool) -> Vec<String> {
        if let Ok(bytes) = files.read_from(path, self.offset) {
            self.offset += bytes.len() as u64;
            self.partial.extend_from_slice(&bytes);
        }
        let mut lines = Vec::new();
        while let Some(at) = self.partial.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=at).collect();
            lines.push(String::from_utf8_lossy(&line[..at]).into_owned());
        }
        if rest && !self.partial.is_empty() {
            lines.push(String::from_utf8_lossy(&std::mem::take(&mut self.partial)).into_owned());
        }
        lines
    }
}

/// A line for the workspace's terminal, which shows what the turns do;
/// nothing when the wrapper has no terminal.
fn say(text: &str) {
    if std::io::stdout().is_terminal() {
        println!("[dagq] {text}");
    }
}

impl<'a> Turns<'a> {
    /// Run the session's turns until the exit request or a turn that ends
    /// it; the wrapper's exit code: 0 after the exit request, 1 after a
    /// turn that failed or was stopped. `child_may_be_alive` is set while a
    /// turn's process may run.
    pub(super) fn drive(mut self, child_may_be_alive: &mut bool) -> Result<i32> {
        let run_dir = PathBuf::from(self.run.run_dir().context("missing run directory")?);
        // The supervisor cleared what an earlier session left (its exit
        // request, its untaken requests) before it opened this workspace.
        self.files.create_dir_all(&turns_dir(&run_dir))?;
        let events = self.queue.run_events(self.run.id())?;
        let mut turn = events
            .iter()
            .filter(|e| e.kind == event_kind::TURN_STARTED)
            .count() as u64;
        // The session of the provider the run is on: what it did since the
        // run last moved to it (ADR-t813-2).
        let mut on = Agent::new(self.run, self.run.actual_provider(), &events);
        let mut task_prompt = self.files.read_to_string(&run_dir.join("prompt.txt"))?;
        let mut first = (!self.resume).then(|| task_prompt.clone());
        let mut registered = false;
        // The request of a turn that resumed a thread its agent does not
        // have, run once more as a new session.
        let mut again: Option<(String, Option<TurnRequest>)> = None;
        loop {
            let (prompt, request) = match again.take().or_else(|| first.take().map(|p| (p, None))) {
                Some(next) => next,
                None => match self.next_request(&run_dir)? {
                    Some(request) => (request.prompt.clone(), Some(request)),
                    None => {
                        say("exit requested; the session ends");
                        return Ok(0);
                    }
                },
            };
            // The supervisor moved the run to the other provider: this turn
            // is the first of a new session there, in the same worktree.
            let provider = self.queue.run(self.run.id())?.actual_provider();
            if provider != on.provider {
                on = Agent::new(self.run, provider, &self.queue.run_events(self.run.id())?);
                // The supervisor wrote the task's prompt again for this
                // provider's worker.
                task_prompt = self.files.read_to_string(&run_dir.join("prompt.txt"))?;
                say(&format!(
                    "the run moved to {}; a new session starts",
                    provider.as_str()
                ));
            }
            let agent = self.agent(on.provider)?;
            // A session is resumed once its model answered or the agent
            // keeps it (a turn that failed before an answer may have left
            // it); an agent that names its own resumes the one it said it
            // started. One that never was starts with the task's prompt,
            // the request after it.
            let from_output = agent.turn_session_from_output();
            let resume = if from_output {
                on.identified.clone()
            } else {
                (on.created || agent.turn_session_exists(self.run, &on.name))
                    .then(|| on.name.clone())
            };
            let asked = prompt.clone();
            let prompt = match &request {
                Some(_) if resume.is_none() => format!("{task_prompt}\n\n{prompt}"),
                _ => prompt,
            };
            turn += 1;
            // Each turn is a process of its own: it starts with the session
            // recorded last (the claim's, a resume's or a revise's, raised
            // after a failure the task caused; ADR-0079 decisions 3 and 5).
            let session = WorkerSession::current(&self.queue.run_events(self.run.id())?);
            let ended = self.turn(
                &run_dir,
                turn,
                &prompt,
                resume.as_deref(),
                &mut on,
                request.as_ref(),
                &session,
                &mut registered,
                child_may_be_alive,
            )?;
            on.created |= ended.result.session_created;
            if let Some(missing) = resume.filter(|_| from_output && ended.result.session_missing) {
                // The agent kept no thread of that id (the turn that named
                // it ended before it was saved): forget it and start anew.
                self.queue.record_runtime_event(
                    self.run.id(),
                    EventKind::TurnSessionIdentified,
                    json!({
                        "turn": turn,
                        "session_id": null,
                        "missing": missing,
                        "provider": on.provider,
                    }),
                )?;
                say(&format!("session {missing} is gone; a new one starts"));
                on.identified = None;
                again = Some((asked, request));
                continue;
            }
            if !ended.outcome.goes_on(ended.failure) {
                say(&format!(
                    "turn {turn} {}; the session ends",
                    ended.outcome.as_str()
                ));
                return Ok(if ended.outcome == TurnOutcome::Stopped {
                    0
                } else {
                    1
                });
            }
        }
    }

    /// The agent of `provider`: the one the wrapper started with, or the
    /// other provider's.
    fn agent(&self, provider: Provider) -> Result<&'a dyn AgentProvider> {
        if provider == self.run.actual_provider() {
            return Ok(self.provider);
        }
        self.other.with_context(|| {
            format!(
                "run {} moved to {}, which this wrapper has no headless agent of",
                self.run.id(),
                provider.as_str()
            )
        })
    }

    /// Wait for the supervisor's next request, heartbeating, and take it;
    /// `None` once the exit is requested.
    fn next_request(&mut self, run_dir: &Path) -> Result<Option<TurnRequest>> {
        loop {
            if self.files.exists(&exit_path(run_dir)) {
                return Ok(None);
            }
            let names: Vec<String> = self
                .files
                .read_dir(&turns_dir(run_dir))?
                .iter()
                .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
                .collect();
            if let Some(&seq) = pending(names.iter().map(String::as_str)).first() {
                let path = request_path(run_dir, seq);
                let request: TurnRequest = serde_json::from_str(&self.files.read_to_string(&path)?)
                    .with_context(|| format!("read request {}", path.display()))?;
                self.files.rename(&path, &taken_path(run_dir, seq))?;
                return Ok(Some(request));
            }
            self.heartbeat();
            thread::sleep(self.provider.wait_interval());
        }
    }

    /// Heartbeat the wrapper; a failure (the queue busy) is only logged,
    /// as the interactive wrapper does.
    fn heartbeat(&mut self) {
        if let Err(error) = self.queue.heartbeat_wrapper(self.run.id(), self.pid) {
            tracing::warn!(
                run_id = %self.run.id(),
                error = %format_args!("{error:#}"),
                "wrapper heartbeat failed: {error:#}"
            );
        }
    }

    /// Run turn `turn` with `prompt` on the agent `on` is of, resuming the
    /// session `resume` (or starting one), and record it. A turn that may
    /// still run when an error ends the wrapper is stopped with its group
    /// first; one whose agent could not be started is recorded as failed
    /// to start (`launch`).
    #[allow(clippy::too_many_arguments)]
    fn turn(
        &mut self,
        run_dir: &Path,
        turn: u64,
        prompt: &str,
        resume: Option<&str>,
        on: &mut Agent,
        request: Option<&TurnRequest>,
        session: &WorkerSession,
        registered: &mut bool,
        child_may_be_alive: &mut bool,
    ) -> Result<Turn> {
        let run = self.run;
        let limits = TurnLimits::parse_or(
            self.files
                .read_to_string(&turns_dir(run_dir).join(LIMITS_FILE))
                .ok()
                .as_deref(),
            StallConfig::default().turn_limits(),
        );
        let stdout = output_path(run_dir, turn, "jsonl");
        let stderr = output_path(run_dir, turn, "err");
        let agent = self.agent(on.provider)?;
        let mut reader = agent.turn_reader()?;
        let worktree = PathBuf::from(run.worktree_path().context("missing worktree")?);
        let spawned = HostActorExecutor::new(self.db)
            .with_provider(agent)
            .with_spawner(self.spawner)
            .spawn(ActorExecutionSpec::new(
                ActorContext::worker(run.id(), run.task_id()),
                WorkspaceAccess::Write(worktree),
                ActorProgram::SessionAgent {
                    agent: SessionAgent::Turn {
                        run,
                        prompt,
                        session: match resume {
                            Some(id) => TurnSession::Resume(id),
                            None => TurnSession::New(&on.name),
                        },
                        stdout: &stdout,
                        stderr: &stderr,
                    },
                    model: Some((&session.model, &session.effort)),
                },
            ))
            .and_then(|handle| handle.process());
        let mut child = match spawned {
            Ok(child) => child,
            // The agent's executable cannot be run: its provider cannot be
            // used (ADR-t813-2 decision 2). Any other error is the
            // wrapper's, as before.
            Err(error) if executable_unrunnable(&error) => {
                return self.launch_failed(run_dir, turn, resume, on, request, &error);
            }
            Err(error) => return Err(error),
        };
        *child_may_be_alive = true;
        let ended = self.follow_turn(
            run_dir,
            turn,
            resume,
            on,
            request,
            registered,
            &mut *child,
            &mut *reader,
            (&stdout, &stderr),
            limits,
        );
        match ended {
            Ok(ended) => {
                *child_may_be_alive = false;
                Ok(ended)
            }
            Err(error) => {
                // Nothing else stops it once the wrapper is gone.
                if stop_turn(self.processes, &mut *child).is_ok() && child.wait().is_ok() {
                    *child_may_be_alive = false;
                }
                Err(error)
            }
        }
    }

    /// Register, record and follow the started turn `turn` until it ended
    /// (or was stopped), then record how and write the idle marker.
    #[allow(clippy::too_many_arguments)]
    fn follow_turn(
        &mut self,
        run_dir: &Path,
        turn: u64,
        resume: Option<&str>,
        on: &mut Agent,
        request: Option<&TurnRequest>,
        registered: &mut bool,
        child: &mut dyn Spawned,
        reader: &mut dyn TurnReader,
        (stdout, stderr): (&Path, &Path),
        limits: TurnLimits,
    ) -> Result<Turn> {
        let run = self.run;
        if !*registered {
            let registration = if self.resume {
                self.queue
                    .register_resume_agent(run.id(), self.pid, child.id())
            } else {
                self.queue.register_agent(run.id(), self.pid, child.id())
            };
            registration?;
            *registered = true;
        } else {
            // Every later turn is a process of its own: it is the run's
            // agent while it runs, so the supervisor's process watches see
            // it and its helpers, not the turn that ended.
            self.queue
                .register_turn_agent(run.id(), self.pid, child.id())?;
        }
        self.started(turn, resume, on, request, Some(child.id()), limits)?;
        let (exit, stop, mut tail) =
            self.follow(run_dir, turn, on, child, reader, stdout, limits)?;
        // What it wrote after the last look.
        for line in tail.read(self.files, stdout, true) {
            for signal in reader.line(&line) {
                if let TurnSignal::Started {
                    session_id: Some(id),
                    ..
                } = signal
                {
                    self.identify(turn, on, &id)?;
                }
            }
        }
        let stderr_text = self.files.read_to_string(stderr).unwrap_or_default();
        let mut result = reader.finish(exit.as_ref(), &stderr_text);
        // An agent that exited in failure (not by a signal) with nothing on
        // its output, and that its reader could tell nothing of, did not
        // start: its provider cannot be used (ADR-t813-2 decision 2).
        if stop.is_none()
            && tail.offset == 0
            && exit
                .as_ref()
                .is_some_and(|exit| !exit.success && exit.code.is_some())
            && !result.session_missing
            && matches!(result.failure, None | Some(TurnFailure::Other))
        {
            result.is_error = true;
            result.failure = Some(TurnFailure::Launch);
        }
        let (outcome, failure) = match &stop {
            Some(stop) => (stop.outcome, stop.failure.or(result.failure)),
            None if result.is_error => (TurnOutcome::Failed, result.failure),
            None => (TurnOutcome::Succeeded, None),
        };
        let exit_code = exit.as_ref().and_then(|exit| exit.code);
        let stopped = stop.as_ref().map(|stop| stop.why.as_str());
        self.finished(turn, outcome, failure, stopped, exit_code, result)
    }

    /// Record that turn `turn` started (its agent's `pid`, none when it
    /// could not be started).
    fn started(
        &mut self,
        turn: u64,
        resume: Option<&str>,
        on: &Agent,
        request: Option<&TurnRequest>,
        pid: Option<u32>,
        limits: TurnLimits,
    ) -> Result<()> {
        let what = request.map_or("the task's prompt", |r| r.what.as_str());
        let from_output = self.agent(on.provider)?.turn_session_from_output();
        self.queue.record_runtime_event(
            self.run.id(),
            EventKind::TurnStarted,
            json!({
                "turn": turn,
                "resume": resume.is_some(),
                "request": request.map(|r| r.seq),
                "what": what,
                "pid": pid,
                "provider": on.provider,
                // An agent that names its own session names a new one in
                // its output.
                "session_id": resume
                    .map(str::to_owned)
                    .or_else(|| (!from_output).then(|| on.name.clone())),
                "silence_secs": limits.silence_secs,
                "limit_secs": limits.limit_secs,
            }),
        )?;
        say(&format!("turn {turn} started: {what}"));
        Ok(())
    }

    /// Turn `turn`'s agent could not be started (`error`): recorded as a
    /// turn that started and failed to start (`launch`), with the idle
    /// marker, so that the supervisor moves the run to the other provider
    /// (ADR-t813-2 decision 2).
    fn launch_failed(
        &mut self,
        run_dir: &Path,
        turn: u64,
        resume: Option<&str>,
        on: &Agent,
        request: Option<&TurnRequest>,
        error: &anyhow::Error,
    ) -> Result<Turn> {
        let limits = TurnLimits::parse_or(
            self.files
                .read_to_string(&turns_dir(run_dir).join(LIMITS_FILE))
                .ok()
                .as_deref(),
            StallConfig::default().turn_limits(),
        );
        self.started(turn, resume, on, request, None, limits)?;
        let result = TurnResult {
            is_error: true,
            failure: Some(TurnFailure::Launch),
            message: Some(format!("the agent could not be started: {error:#}")),
            model_unknown: Some("the agent did not start".to_owned()),
            ..TurnResult::default()
        };
        self.finished(
            turn,
            TurnOutcome::Failed,
            Some(TurnFailure::Launch),
            None,
            None,
            result,
        )
    }

    /// Record how turn `turn` ended and write the idle marker.
    fn finished(
        &mut self,
        turn: u64,
        outcome: TurnOutcome,
        failure: Option<TurnFailure>,
        stopped: Option<&str>,
        exit_code: Option<i32>,
        result: TurnResult,
    ) -> Result<Turn> {
        let run = self.run;
        let (tokens, tokens_total) = self.turn_tokens(&result)?;
        // The provider the turn ran on: the wrapper's copy of the run may
        // predate a switch (ADR-t813-2).
        let provider = self.queue.run(run.id())?.actual_provider();
        self.queue.record_runtime_event(
            run.id(),
            EventKind::TurnFinished,
            json!({
                "turn": turn,
                "outcome": outcome,
                "failure": failure,
                "stopped": stopped,
                "exit_code": exit_code,
                "message": result.message,
                "session_id": result.session_id,
                "session_created": result.session_created,
                "num_turns": result.num_turns,
                "duration_ms": result.duration_ms,
                "cost_usd": result.cost_usd,
                "usage": result.usage,
                "provider": provider,
                // The model the agent ran the turn on, as it says (Codex's
                // is not the claim's Claude model), else why it is not
                // known.
                "model": result.model,
                "model_unknown": result.model_unknown,
                // The runtime's kinds of token, which the span sums
                // (ADR-t813-2 decision 7).
                "tokens": tokens.as_ref().map(TokenUsage::payload),
                "tokens_total": tokens_total.as_ref().map(TokenUsage::payload),
                "permission_denials": result.permission_denials.len(),
                "denied_tools": result.permission_denials,
            }),
        )?;
        // The idle marker the supervisor's watches read, written only once
        // the turn is recorded.
        let marker = run.idle_marker_path()?;
        let tmp = marker.with_extension("json.tmp");
        self.files.write(
            &tmp,
            idle_marker(run.id().as_str(), turn, outcome, &result)
                .to_string()
                .as_bytes(),
        )?;
        self.files.rename(&tmp, &marker)?;
        say(&format!(
            "turn {turn} {}{}{}",
            outcome.as_str(),
            failure.map_or(String::new(), |f| format!(" ({})", f.as_str())),
            result
                .message
                .as_deref()
                .map_or(String::new(), |m| format!(": {m}"))
        ));
        Ok(Turn {
            outcome,
            failure,
            result,
        })
    }

    /// The turn's own tokens, and the session's running total when the
    /// provider gives that instead (Codex): the total less the one the
    /// session's last turn recorded as `tokens_total`, the whole total for
    /// a session's first (ADR-t813-2 decision 7).
    fn turn_tokens(
        &mut self,
        result: &TurnResult,
    ) -> Result<(Option<TokenUsage>, Option<TokenUsage>)> {
        let Some(total) = result.tokens.clone().filter(|_| result.tokens_cumulative) else {
            return Ok((result.tokens.clone(), None));
        };
        let events = self.queue.run_events(self.run.id())?;
        let earlier = events
            .iter()
            .rev()
            .filter(|e| {
                e.kind == event_kind::TURN_FINISHED
                    && result.session_id.is_some()
                    && e.payload["session_id"].as_str() == result.session_id.as_deref()
            })
            .find_map(|e| TokenUsage::from_payload(&e.payload["tokens_total"]));
        let own = earlier.map_or_else(
            || TokenUsage {
                messages: 1,
                ..total.clone()
            },
            |earlier| total.since(&earlier),
        );
        Ok((Some(own), Some(total)))
    }

    /// Record the session `id` the agent said turn `turn` runs in, when it
    /// names its own and `id` is not the one it said last: the next turns
    /// resume it (Codex's thread, ADR-t813-1).
    fn identify(&mut self, turn: u64, on: &mut Agent, id: &str) -> Result<()> {
        if !self.agent(on.provider)?.turn_session_from_output()
            || on.identified.as_deref() == Some(id)
        {
            return Ok(());
        }
        self.queue.record_runtime_event(
            self.run.id(),
            EventKind::TurnSessionIdentified,
            json!({
                "turn": turn,
                "session_id": id,
                "provider": on.provider,
            }),
        )?;
        on.identified = Some(id.to_owned());
        Ok(())
    }

    /// Follow the turn's process until it exits or the wrapper stops it:
    /// its exit (`None` when stopped), why it was stopped, and the tail of
    /// its stdout read so far.
    #[allow(clippy::too_many_arguments)]
    fn follow(
        &mut self,
        run_dir: &Path,
        turn: u64,
        on: &mut Agent,
        child: &mut dyn Spawned,
        reader: &mut dyn TurnReader,
        stdout: &Path,
        limits: TurnLimits,
    ) -> Result<(Option<super::Exit>, Option<Stop>, Tail)> {
        let started = Instant::now();
        let mut last_output = Instant::now();
        let mut tail = Tail::default();
        let silence = limits.silence();
        let limit = limits.limit();
        let mut stop: Option<Stop> = None;
        loop {
            let lines = tail.read(self.files, stdout, false);
            if !lines.is_empty() {
                last_output = Instant::now();
            }
            for line in lines {
                for signal in reader.line(&line) {
                    match signal {
                        TurnSignal::Started {
                            session_id,
                            permission_mode,
                            ..
                        } => {
                            if let Some(id) = session_id {
                                self.identify(turn, on, &id)?;
                            }
                            if let Some(expected) = self.agent(on.provider)?.turn_permission_mode()
                                && permission_mode.as_deref() != Some(expected)
                                && stop.is_none()
                            {
                                stop = Some(Stop {
                                    outcome: TurnOutcome::LaunchMismatch,
                                    failure: None,
                                    why: format!(
                                        "the agent started in permission mode {} instead of {expected}",
                                        permission_mode.as_deref().unwrap_or("(none)")
                                    ),
                                });
                            }
                        }
                        TurnSignal::Unusable(failure, message) => {
                            if stop.is_none() {
                                stop = Some(Stop {
                                    outcome: TurnOutcome::Failed,
                                    failure: Some(failure),
                                    why: message,
                                });
                            }
                        }
                        TurnSignal::Said(text) => say(&text),
                        TurnSignal::Tool(tool) => say(&format!("→ {tool}")),
                    }
                }
            }
            if stop.is_none()
                && let Some(exit) = child.try_wait()?
            {
                // What the turn left running in its group ends with it,
                // as the worker is told. What it left outside its group is
                // no longer found as its descendant (its parent is 1 now)
                // and is left running (headless-worker.md).
                let _ = child.kill_group();
                return Ok((Some(exit), None, tail));
            }
            if stop.is_none() {
                stop = if self.files.exists(&exit_path(run_dir)) {
                    Some(Stop {
                        outcome: TurnOutcome::Stopped,
                        failure: None,
                        why: "the supervisor asked the session to exit".to_owned(),
                    })
                } else if reader.heartbeats() && last_output.elapsed() >= silence {
                    Some(Stop {
                        outcome: TurnOutcome::Silent,
                        failure: None,
                        why: format!("no output for {}s", limits.silence_secs),
                    })
                } else if started.elapsed() >= limit {
                    Some(Stop {
                        outcome: TurnOutcome::TimedOut,
                        failure: None,
                        why: format!("the turn ran past {}s", limits.limit_secs),
                    })
                } else {
                    None
                };
            }
            if let Some(stop) = stop {
                say(&format!("stopping the turn: {}", stop.why));
                stop_turn(self.processes, child)?;
                let _ = child.wait();
                return Ok((None, Some(stop), tail));
            }
            self.heartbeat();
            thread::sleep(self.provider.wait_interval());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::memory_files::MemoryFiles;

    #[test]
    fn only_an_executable_that_cannot_be_run_is_a_start_that_failed() {
        let missing = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::NotFound))
            .context("spawn /nonexistent/codex");
        assert!(executable_unrunnable(&missing));
        let denied =
            anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(executable_unrunnable(&denied));
        assert!(!executable_unrunnable(&anyhow::anyhow!("missing worktree")));
        let full = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::StorageFull));
        assert!(!executable_unrunnable(&full));
    }

    #[test]
    fn a_tail_reads_whole_lines_as_they_come() {
        let files = MemoryFiles::default();
        let path = Path::new("/r/out");
        let mut tail = Tail::default();
        assert!(tail.read(&files, path, false).is_empty());
        files.write(path, b"one\ntw").unwrap();
        assert_eq!(tail.read(&files, path, false), ["one"]);
        files.write(path, b"one\ntwo\nthr").unwrap();
        assert_eq!(tail.read(&files, path, false), ["two"]);
        assert_eq!(tail.read(&files, path, true), ["thr"]);
        assert!(tail.read(&files, path, true).is_empty());
    }
}
