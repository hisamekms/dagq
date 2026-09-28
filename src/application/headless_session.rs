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
//! interactive session.

use crate::domain::EventKind;
use anyhow::{Context, Result};
use serde_json::json;
use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use super::{
    AgentProvider, Queue, RunFiles, Spawned, Spawner, TurnReader,
    actor_executor::{
        ActorExecutionSpec, ActorExecutor, ActorProgram, HostActorExecutor, SessionAgent,
        WorkspaceAccess,
    },
};
use crate::domain::{
    ActorContext, TaskRun, event_kind,
    stall::StallConfig,
    tokens::TokenUsage,
    turn::{
        LIMITS_FILE, TurnFailure, TurnLimits, TurnOutcome, TurnRequest, TurnResult, TurnSignal,
        exit_path, idle_marker, output_path, pending, request_path, taken_path, turns_dir,
    },
    worker_model::WorkerSession,
};

/// What the wrapper of a headless worker works with.
pub(super) struct Turns<'a> {
    pub(super) queue: &'a mut dyn Queue,
    pub(super) db: &'a Path,
    pub(super) run: &'a TaskRun,
    pub(super) provider: &'a dyn AgentProvider,
    pub(super) spawner: &'a dyn Spawner,
    pub(super) files: &'a dyn RunFiles,
    pub(super) pid: u32,
    /// The wrapper of a `needs_session` run's resume: it waits for the
    /// supervisor's request instead of starting with the task's prompt.
    pub(super) resume: bool,
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

impl Turns<'_> {
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
        let mut created = events
            .iter()
            .any(|e| e.kind == event_kind::TURN_FINISHED && e.payload["session_created"] == true);
        let mut turn = events
            .iter()
            .filter(|e| e.kind == event_kind::TURN_STARTED)
            .count() as u64;
        // The session an agent that names its own (Codex's thread) said it
        // started, the last one recorded.
        let from_output = self.provider.turn_session_from_output();
        let mut identified = events
            .iter()
            .rfind(|e| e.kind == event_kind::TURN_SESSION_IDENTIFIED)
            .and_then(|e| e.payload["session_id"].as_str().map(str::to_owned));
        let task_prompt = self.files.read_to_string(&run_dir.join("prompt.txt"))?;
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
            // A session is resumed once its model answered or the agent
            // keeps it (a turn that failed before an answer may have left
            // it); an agent that names its own resumes the one it said it
            // started. One that never was starts with the task's prompt,
            // the request after it.
            let resume = if from_output {
                identified.clone()
            } else {
                (created || self.provider.turn_session_exists(self.run))
                    .then(|| self.run.id().to_string())
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
                &mut identified,
                request.as_ref(),
                &session,
                &mut registered,
                child_may_be_alive,
            )?;
            created |= ended.result.session_created;
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
                        "provider": self.run.actual_provider(),
                    }),
                )?;
                say(&format!("session {missing} is gone; a new one starts"));
                identified = None;
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

    /// Run turn `turn` with `prompt`, resuming the session `resume`, and
    /// record it; `identified` is the session the agent last said it
    /// started. A turn that may still run when an error ends the wrapper
    /// is stopped with its group first.
    #[allow(clippy::too_many_arguments)]
    fn turn(
        &mut self,
        run_dir: &Path,
        turn: u64,
        prompt: &str,
        resume: Option<&str>,
        identified: &mut Option<String>,
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
        let mut reader = self.provider.turn_reader()?;
        let mut child = HostActorExecutor::new(self.db)
            .with_provider(self.provider)
            .with_spawner(self.spawner)
            .spawn(ActorExecutionSpec::new(
                ActorContext::worker(run.id(), run.task_id()),
                WorkspaceAccess::Write(PathBuf::from(
                    run.worktree_path().context("missing worktree")?,
                )),
                ActorProgram::SessionAgent {
                    agent: SessionAgent::Turn {
                        run,
                        prompt,
                        resume,
                        stdout: &stdout,
                        stderr: &stderr,
                    },
                    model: Some((&session.model, &session.effort)),
                },
            ))?
            .process()?;
        *child_may_be_alive = true;
        let ended = self.follow_turn(
            run_dir,
            turn,
            resume,
            identified,
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
                if child.kill_group().is_ok() && child.wait().is_ok() {
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
        identified: &mut Option<String>,
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
        }
        let what = request.map_or("the task's prompt", |r| r.what.as_str());
        self.queue.record_runtime_event(
            run.id(),
            EventKind::TurnStarted,
            json!({
                "turn": turn,
                "resume": resume.is_some(),
                "request": request.map(|r| r.seq),
                "what": what,
                "pid": child.id(),
                // An agent that names its own session names a new one in
                // its output.
                "session_id": resume.map(str::to_owned).or_else(|| {
                    (!self.provider.turn_session_from_output()).then(|| run.id().to_string())
                }),
                "silence_secs": limits.silence_secs,
                "limit_secs": limits.limit_secs,
            }),
        )?;
        say(&format!("turn {turn} started: {what}"));
        let (exit, stop, mut tail) =
            self.follow(run_dir, turn, identified, child, reader, stdout, limits)?;
        // What it wrote after the last look.
        for line in tail.read(self.files, stdout, true) {
            for signal in reader.line(&line) {
                if let TurnSignal::Started {
                    session_id: Some(id),
                    ..
                } = signal
                {
                    self.identify(turn, identified, &id)?;
                }
            }
        }
        let stderr_text = self.files.read_to_string(stderr).unwrap_or_default();
        let result = reader.finish(exit.as_ref(), &stderr_text);
        let (outcome, failure) = match &stop {
            Some(stop) => (stop.outcome, stop.failure.or(result.failure)),
            None if result.is_error => (TurnOutcome::Failed, result.failure),
            None => (TurnOutcome::Succeeded, None),
        };
        let exit_code = exit.as_ref().and_then(|exit| exit.code);
        let (tokens, tokens_total) = self.turn_tokens(&result)?;
        self.queue.record_runtime_event(
            run.id(),
            EventKind::TurnFinished,
            json!({
                "turn": turn,
                "outcome": outcome,
                "failure": failure,
                "stopped": stop.as_ref().map(|stop| stop.why.as_str()),
                "exit_code": exit_code,
                "message": result.message,
                "session_id": result.session_id,
                "session_created": result.session_created,
                "num_turns": result.num_turns,
                "duration_ms": result.duration_ms,
                "cost_usd": result.cost_usd,
                "usage": result.usage,
                "provider": run.actual_provider(),
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
    /// names its own and `id` is not the one `identified` last: the next
    /// turns resume it (Codex's thread, ADR-t813-1).
    fn identify(&mut self, turn: u64, identified: &mut Option<String>, id: &str) -> Result<()> {
        if !self.provider.turn_session_from_output() || identified.as_deref() == Some(id) {
            return Ok(());
        }
        self.queue.record_runtime_event(
            self.run.id(),
            EventKind::TurnSessionIdentified,
            json!({
                "turn": turn,
                "session_id": id,
                "provider": self.run.actual_provider(),
            }),
        )?;
        *identified = Some(id.to_owned());
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
        identified: &mut Option<String>,
        child: &mut dyn Spawned,
        reader: &mut dyn TurnReader,
        stdout: &Path,
        limits: TurnLimits,
    ) -> Result<(Option<super::Exit>, Option<Stop>, Tail)> {
        let started = Instant::now();
        let mut last_output = Instant::now();
        let mut tail = Tail::default();
        let silence = Duration::from_secs(u64::try_from(limits.silence_secs).unwrap_or(0));
        let limit = Duration::from_secs(u64::try_from(limits.limit_secs).unwrap_or(0));
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
                                self.identify(turn, identified, &id)?;
                            }
                            if let Some(expected) = self.provider.turn_permission_mode()
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
                // as the worker is told.
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
                child.kill_group()?;
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
