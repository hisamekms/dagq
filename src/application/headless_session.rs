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
//!
//! A headless planner of the runtime's (ADR-t1394-2 decision 2) runs on
//! the same turns in its planner directory: its initial prompt is the
//! first turn, and the revise, the answer of its `planner_question` and the
//! exit come as requests; its turns are events of the queue that name it
//! by `planner_id`, and it stays on Claude ([`TurnOwner::Planner`]).

use crate::domain::EventKind;
use anyhow::{Context, Result};
use serde_json::json;
use std::{
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use super::{
    AgentProvider, Clock, ProcessControl, Queue, RunFiles, SccacheServer, Spawned, Spawner,
    TurnReader, TurnTarget,
    actor_executor::{
        ActorExecutionSpec, ActorExecutor, ActorProgram, HostActorExecutor, SessionAgent,
        WorkspaceAccess,
    },
};
use crate::domain::{
    ActorContext, ActorRole, PlannerId, Provider, RunEvent, TaskRun, event_kind,
    provider_switch::{since_switch, switches},
    sccache::SccacheTarget,
    stall::StallConfig,
    tokens::{ExecutionTokens, ModelTokens, TokenSource, TokenUsage},
    turn::{
        LIMITS_FILE, TurnFailure, TurnLimits, TurnOutcome, TurnRequest, TurnResult, TurnSession,
        TurnSignal, commands_path, counted_rollout_turns, exit_path, idle_marker, output_path,
        pending, renew_wall_marker, request_path, request_to_take, session_name, taken_path,
        thread_total_own, turn_own_cost, turn_own_models, turns_dir,
    },
    worker_model::WorkerSession,
};

/// Unix milliseconds on `clock`: when the wrapper read the lines of a
/// turn's output it reads next ([`TurnReader::stamp`]).
fn now_millis(clock: &dyn Clock) -> i64 {
    clock
        .system_time()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_millis()).ok())
        .unwrap_or(0)
}

/// Unix seconds on `clock`, the `at` of `sccache_wrapper_removed`.
fn unix_secs(clock: &dyn Clock) -> u64 {
    clock
        .system_time()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Whose turns [`Turns`] drives.
pub(super) enum TurnOwner<'a> {
    /// A headless worker's run.
    Run(&'a TaskRun),
    /// A headless planner of the runtime's (ADR-t1394-2 decision 2): its
    /// directory (prompt, requests, turns, idle marker), the checkout it
    /// works in, the plugin directory it loads, the model and effort its
    /// opener chose, and the name of its session.
    Planner {
        id: PlannerId,
        dir: &'a Path,
        cwd: &'a Path,
        plugin_dir: Option<&'a Path>,
        model: Option<(&'a str, &'a str)>,
        session: String,
    },
}

/// What the wrapper of a headless session works with.
pub(super) struct Turns<'a> {
    pub(super) queue: &'a mut dyn Queue,
    pub(super) db: &'a Path,
    pub(super) owner: TurnOwner<'a>,
    /// The agent of the run's provider when the wrapper started.
    pub(super) provider: &'a dyn AgentProvider,
    /// The headless agent of the other provider, which the run's turns go
    /// to once the supervisor moved it there (ADR-t813-2); `None` when the
    /// binary has none.
    pub(super) other: Option<&'a dyn AgentProvider>,
    pub(super) spawner: &'a dyn Spawner,
    /// The queue service's socket and the worker's token, which each turn
    /// is given instead of the queue's path (goal 82's stage (3)).
    /// `None` for a planner, whose `dagq` opens the queue itself.
    pub(super) queue_service: Option<&'a dyn super::queue_service::ServiceAccess>,
    /// Lists the turn's descendants and signals them ([`stop_turn`]).
    pub(super) processes: &'a dyn ProcessControl,
    pub(super) files: &'a dyn RunFiles,
    pub(super) pid: u32,
    /// The wrapper of a `needs_session` run's resume: it waits for the
    /// supervisor's request instead of starting with the task's prompt.
    pub(super) resume: bool,
    /// The sccache its environment names as `RUSTC_WRAPPER`, and how its
    /// server is looked at and the guard made: a turn on either provider
    /// runs without `RUSTC_WRAPPER` unless the server listens just before
    /// it starts, and through the guard when it does (ADR-t2086-1). `None`
    /// when the wrapper names no sccache.
    pub(super) sccache: Option<(&'a SccacheTarget, &'a dyn SccacheServer)>,
    /// The wrapper runs in the background (ADR-t1404-1 decision 6): its
    /// stdout is the session's log, not a terminal, and takes the `[dagq]`
    /// summary of the turns all the same, for `run log` / `planner log`.
    pub(super) background: bool,
    /// The clock the turns' stamps and the recorded times are read on.
    pub(super) clock: &'a dyn Clock,
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
    fn new(name: String, provider: Provider, events: &[RunEvent]) -> Self {
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
            name,
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
#[derive(Debug, PartialEq, Eq)]
struct Stop {
    outcome: TurnOutcome,
    failure: Option<TurnFailure>,
    why: String,
}

/// The first unusable observation wins over exit and time limits. The caller
/// reads the clock and files; this decision only sees their values.
fn stop_decision(
    observed: Option<Stop>,
    exit_requested: bool,
    heartbeats: bool,
    since_output: Duration,
    elapsed: Duration,
    limits: TurnLimits,
) -> Option<Stop> {
    observed.or_else(|| {
        if exit_requested {
            Some(Stop {
                outcome: TurnOutcome::Stopped,
                failure: None,
                why: "the supervisor asked the session to exit".to_owned(),
            })
        } else if heartbeats && since_output >= limits.silence() {
            Some(Stop {
                outcome: TurnOutcome::Silent,
                failure: None,
                why: format!("no output for {}s", limits.silence_secs),
            })
        } else if elapsed >= limits.limit() {
            Some(Stop {
                outcome: TurnOutcome::TimedOut,
                failure: None,
                why: format!("the turn ran past {}s", limits.limit_secs),
            })
        } else {
            None
        }
    })
}

/// A signal's stop, keeping the first one in output order.
fn observed_stop(
    previous: Option<Stop>,
    signal: &TurnSignal,
    expected: Option<&str>,
) -> Option<Stop> {
    previous.or_else(|| match signal {
        TurnSignal::Started {
            permission_mode, ..
        } if expected.is_some_and(|expected| permission_mode.as_deref() != Some(expected)) => {
            Some(Stop {
                outcome: TurnOutcome::LaunchMismatch,
                failure: None,
                why: format!(
                    "the agent started in permission mode {} instead of {}",
                    permission_mode.as_deref().unwrap_or("(none)"),
                    expected.unwrap()
                ),
            })
        }
        TurnSignal::Unusable(failure, message) => Some(Stop {
            outcome: TurnOutcome::Failed,
            failure: Some(*failure),
            why: message.clone(),
        }),
        _ => None,
    })
}

/// Stop a turn that still runs: SIGKILL to its process group and to each
/// of its descendants by pid. A provider may run a command in a group of
/// its own (Codex does: its pgid is the command's pid), which a signal to
/// the turn's group does not reach; the descendants are listed before the
/// group is killed, as a descendant whose parent was killed has 1 for a
/// parent and is no longer found (task 1085). Claude Code starts the shell
/// of each Bash tool call in a group of its own too, so the turn's group
/// holds `claude` alone. SIGKILL, not SIGINT: the descendants are killed by
/// pid, so nothing is left for the provider to tidy, and a provider that
/// does not answer is not waited for.
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
        if let Some(at) = self.partial.iter().rposition(|b| *b == b'\n') {
            lines.extend(
                self.partial[..at]
                    .split(|b| *b == b'\n')
                    .map(|line| String::from_utf8_lossy(line).into_owned()),
            );
            self.partial.drain(..=at);
        }
        // A worker can append indefinitely without a newline. Keep
        // that from growing the wrapper without bound.
        if self.partial.len() > 64 * 1024 * 1024 {
            tracing::warn!(path = %path.display(), "discard turn output with an overlong line");
            self.partial.clear();
        }
        if rest && !self.partial.is_empty() {
            lines.push(String::from_utf8_lossy(&std::mem::take(&mut self.partial)).into_owned());
        }
        lines
    }
}

/// A line for the workspace's terminal, which shows what the turns do, or
/// for the log of a wrapper in the `background`; nothing otherwise (no
/// terminal).
fn say(background: bool, text: &str) {
    // The summary is a convenience: a write that fails (a full disk under
    // the log) loses the line, never the session.
    if background || std::io::stdout().is_terminal() {
        let _ = writeln!(std::io::stdout(), "[dagq] {text}");
    }
}

/// What the turns of [`Turns`]' owner are read and recorded through.
impl<'a> Turns<'a> {
    /// The directory of its prompt, requests and turns: the run's, or the
    /// planner's.
    fn dir(&self) -> Result<PathBuf> {
        match &self.owner {
            TurnOwner::Run(run) => Ok(PathBuf::from(
                run.run_dir().context("missing run directory")?,
            )),
            TurnOwner::Planner { dir, .. } => Ok(dir.to_path_buf()),
        }
    }

    /// Where its turns work: the run's worktree, or the planner's checkout.
    fn cwd(&self) -> Result<PathBuf> {
        match &self.owner {
            TurnOwner::Run(run) => Ok(PathBuf::from(
                run.worktree_path().context("missing worktree")?,
            )),
            TurnOwner::Planner { cwd, .. } => Ok(cwd.to_path_buf()),
        }
    }

    /// Its events: the run's, or the planner's turns.
    fn events(&self) -> Result<Vec<RunEvent>> {
        match &self.owner {
            TurnOwner::Run(run) => self.queue.run_events(run.id()),
            TurnOwner::Planner { id, .. } => self.queue.planner_turn_events(*id),
        }
    }

    /// The provider it started on: the run's, or Claude for a planner,
    /// which never moves (ADR-t1394-2 decision 5).
    fn start_provider(&self) -> Provider {
        match &self.owner {
            TurnOwner::Run(run) => run.actual_provider(),
            TurnOwner::Planner { .. } => Provider::Claude,
        }
    }

    /// The provider it is on now: the run's, read again (the supervisor
    /// may have moved it, ADR-t813-2).
    fn provider_now(&self) -> Result<Provider> {
        match &self.owner {
            TurnOwner::Run(run) => Ok(self.queue.run(run.id())?.actual_provider()),
            TurnOwner::Planner { .. } => Ok(Provider::Claude),
        }
    }

    /// The name of its session for a provider that takes one: the run's
    /// ([`session_name`]), or the planner's own.
    fn session_name(&self, events: &[RunEvent]) -> String {
        match &self.owner {
            TurnOwner::Run(run) => session_name(run.id().as_str(), switches(events)),
            TurnOwner::Planner { session, .. } => session.clone(),
        }
    }

    /// How the owner is named in the logs.
    fn label(&self) -> String {
        match &self.owner {
            TurnOwner::Run(run) => format!("run {}", run.id()),
            TurnOwner::Planner { id, .. } => format!("planner {id}"),
        }
    }

    /// Record `kind` with `payload`: on the run, or as an event of the
    /// queue that names the planner (`planner_id`).
    fn record(&mut self, kind: EventKind, mut payload: serde_json::Value) -> Result<()> {
        match &self.owner {
            TurnOwner::Run(run) => self.queue.record_runtime_event(run.id(), kind, payload),
            TurnOwner::Planner { id, .. } => {
                payload["planner_id"] = json!(id);
                self.queue.record_queue_event(kind, payload).map(|_| ())
            }
        }
    }

    /// The idle marker the supervisor's watches read: the run's, or the
    /// planner's.
    fn idle_marker_path(&self) -> Result<PathBuf> {
        match &self.owner {
            TurnOwner::Run(run) => Ok(run.idle_marker_path()?),
            TurnOwner::Planner { dir, .. } => Ok(super::planner_idle_marker(dir)),
        }
    }

    /// The session the idle marker names.
    fn marker_session(&self) -> String {
        match &self.owner {
            TurnOwner::Run(run) => run.id().to_string(),
            TurnOwner::Planner { session, .. } => session.clone(),
        }
    }

    /// The model and effort turn's agent starts with: the session the run
    /// recorded last (the claim's, a resume's or a revise's, raised after a
    /// failure the task caused; ADR-0079 decisions 3 and 5), or the
    /// planner's opener's.
    fn model(&self) -> Result<Option<(String, String)>> {
        match &self.owner {
            TurnOwner::Run(_) => {
                let session = WorkerSession::current(&self.events()?);
                Ok(Some((session.model, session.effort)))
            }
            TurnOwner::Planner { model, .. } => {
                Ok(model.map(|(model, effort)| (model.to_owned(), effort.to_owned())))
            }
        }
    }
}

impl<'a> Turns<'a> {
    /// Run the session's turns until the exit request or a turn that ends
    /// it; the wrapper's exit code: 0 after the exit request, 1 after a
    /// turn that failed or was stopped. `child_may_be_alive` is set while a
    /// turn's process may run.
    pub(super) fn drive(mut self, child_may_be_alive: &mut bool) -> Result<i32> {
        let run_dir = self.dir()?;
        // The supervisor cleared what an earlier session left (its exit
        // request, its untaken requests) before it opened this workspace.
        self.files.create_dir_all(&turns_dir(&run_dir))?;
        let events = self.events()?;
        let mut turn = events
            .iter()
            .filter(|e| e.kind == event_kind::TURN_STARTED)
            .count() as u64;
        // The session of the provider the run is on: what it did since the
        // run last moved to it (ADR-t813-2).
        let mut on = Agent::new(self.session_name(&events), self.start_provider(), &events);
        let mut task_prompt = self.files.read_to_string(&run_dir.join("prompt.txt"))?;
        let mut first = (!self.resume).then(|| task_prompt.clone());
        let mut registered = false;
        // The request of a turn that resumed a thread its agent does not
        // have, run once more as a new session.
        let mut again: Option<(String, Option<TurnRequest>)> = None;
        // The last turn met the provider's wall: a planner's wrapper waits
        // for the `provider retry` before the requests that wait behind it.
        let mut walled = false;
        loop {
            let (prompt, request) = match again.take().or_else(|| first.take().map(|p| (p, None))) {
                Some(next) => next,
                None => match self.next_request(&run_dir, walled)? {
                    Some(request) => (request.prompt.clone(), Some(request)),
                    None => {
                        say(self.background, "exit requested; the session ends");
                        return Ok(0);
                    }
                },
            };
            // The supervisor moved the run to the other provider: this turn
            // is the first of a new session there, in the same worktree.
            let provider = self.provider_now()?;
            if provider != on.provider {
                let events = self.events()?;
                on = Agent::new(self.session_name(&events), provider, &events);
                // The supervisor wrote the task's prompt again for this
                // provider's worker.
                task_prompt = self.files.read_to_string(&run_dir.join("prompt.txt"))?;
                say(
                    self.background,
                    &format!(
                        "the run moved to {}; a new session starts",
                        provider.as_str()
                    ),
                );
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
                (on.created || agent.turn_session_exists(&self.cwd()?, &on.name))
                    .then(|| on.name.clone())
            };
            let asked = prompt.clone();
            let prompt = match &request {
                Some(_) if resume.is_none() => format!("{task_prompt}\n\n{prompt}"),
                _ => prompt,
            };
            turn += 1;
            // Each turn is a process of its own, started with the model
            // recorded last ([`Self::model`]).
            let session = self.model()?;
            let ended = self.turn(
                &run_dir,
                turn,
                &prompt,
                resume.as_deref(),
                &mut on,
                request.as_ref(),
                session,
                &mut registered,
                child_may_be_alive,
            )?;
            on.created |= ended.result.session_created;
            if let Some(missing) = resume.filter(|_| from_output && ended.result.session_missing) {
                // The agent kept no thread of that id (the turn that named
                // it ended before it was saved): forget it and start anew.
                self.record(
                    EventKind::TurnSessionIdentified,
                    json!({
                        "turn": turn,
                        "session_id": null,
                        "missing": missing,
                        "provider": on.provider,
                    }),
                )?;
                say(
                    self.background,
                    &format!("session {missing} is gone; a new one starts"),
                );
                on.identified = None;
                again = Some((asked, request));
                continue;
            }
            walled = ended.outcome == TurnOutcome::Failed
                && ended.failure.is_some_and(TurnFailure::at_provider_wall);
            if !ended.outcome.goes_on(ended.failure) {
                say(
                    self.background,
                    &format!("turn {turn} {}; the session ends", ended.outcome.as_str()),
                );
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
        if provider == self.start_provider() {
            return Ok(self.provider);
        }
        self.other.with_context(|| {
            format!(
                "{} moved to {}, which this wrapper has no headless agent of",
                self.label(),
                provider.as_str()
            )
        })
    }

    /// Wait for the supervisor's next request, heartbeating, and take it;
    /// `None` once the exit is requested. A planner's wrapper whose last
    /// turn met the provider's wall (`walled`) takes the `provider retry`
    /// first and leaves the requests before it waiting ([`request_to_take`]):
    /// the planner stays idle at the wall for the supervisor's hold, and
    /// the failed turn's request is made again before them (task 1596). A
    /// worker's takes them in order, as before: its supervisor tends the
    /// wall by the run's events rather than by its idleness.
    fn next_request(&mut self, run_dir: &Path, walled: bool) -> Result<Option<TurnRequest>> {
        let after_wall = walled && matches!(self.owner, TurnOwner::Planner { .. });
        loop {
            if self.files.is_file(&exit_path(run_dir)) {
                return Ok(None);
            }
            let names: Vec<String> = self
                .files
                .read_dir(&turns_dir(run_dir))?
                .iter()
                .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
                .collect();
            let mut waiting = Vec::new();
            for seq in pending(names.iter().map(String::as_str)) {
                let path = request_path(run_dir, seq);
                let request: TurnRequest = serde_json::from_str(&self.files.read_to_string(&path)?)
                    .with_context(|| format!("read request {}", path.display()))?;
                waiting.push(request);
                if !after_wall {
                    break;
                }
            }
            let next =
                request_to_take(waiting.iter().map(|r| (r.seq, r.what.as_str())), after_wall);
            if after_wall && next.is_none() {
                self.renew_wall_marker(run_dir);
            }
            if let Some(request) = next.and_then(|seq| waiting.into_iter().find(|r| r.seq == seq)) {
                let seq = request.seq;
                let path = request_path(run_dir, seq);
                // Stamped before it is taken: a planner's idle marker of the
                // turn before is no idle with this one (a request written
                // while that turn ran), not even between the two.
                if matches!(self.owner, TurnOwner::Planner { .. })
                    && let Err(error) = super::screen_idle::record_supervisor_input(
                        self.files,
                        &super::planner_idle_marker(run_dir),
                    )
                {
                    tracing::warn!(
                        "the input stamp of request {seq} could not be written: {error}"
                    );
                }
                self.files.rename(&path, &taken_path(run_dir, seq))?;
                return Ok(Some(request));
            }
            self.heartbeat();
            thread::sleep(self.provider.wait_interval());
        }
    }

    /// Write a planner's idle marker of the turn that met the provider's
    /// wall again when an input stamped after it left it stale while no
    /// `provider retry` waits ([`renew_wall_marker`]): the planner is idle
    /// at the wall again, for the supervisor's `tend_planner_walls`. A
    /// failure is only logged; the next look tries again.
    fn renew_wall_marker(&self, run_dir: &Path) {
        let marker = super::planner_idle_marker(run_dir);
        let Ok(Some((modified, bytes))) = self.files.read_stamped(&marker) else {
            return;
        };
        let last_input = super::screen_idle::last_input(self.files, &marker, std::time::UNIX_EPOCH);
        if !renew_wall_marker(false, Some(modified), last_input) {
            return;
        }
        let tmp = marker.with_extension("json.tmp");
        if let Err(error) = self
            .files
            .write(&tmp, &bytes)
            .and_then(|()| self.files.rename(&tmp, &marker))
        {
            tracing::warn!("the idle marker at the wall could not be renewed: {error}");
        }
    }

    /// The variables turn `turn` runs without and with, on either provider
    /// (ADR-t2086-1): no sccache a turn runs may start the server, which
    /// would keep a Codex turn's sandbox (ADR-t1215-1) and is the
    /// supervisor's to start for any. The turn is given the refusal of the
    /// server's start, and its server is looked at just before it starts
    /// (a connect to the port, which starts nothing). One whose server
    /// listens compiles through the guard made in `run_dir`, which runs the
    /// compiler itself should the server stop during the turn
    /// (ADR-t2086-1); one whose server does not, or whose guard could not
    /// be made, runs without `RUSTC_WRAPPER` (its build is uncached but
    /// correct), recorded as `sccache_wrapper_removed`.
    fn sccache_turn(
        &mut self,
        run_dir: &Path,
        turn: u64,
    ) -> (&'static [&'static str], Vec<(String, String)>) {
        let Some((target, server)) = self.sccache else {
            return (&[], Vec::new());
        };
        let TurnOwner::Run(run) = self.owner else {
            return (&[], Vec::new());
        };
        let look = turn_look(target, server, run_dir);
        if let Some((port, why)) = look.removed() {
            crate::application::sccache::record_wrapper_removed(
                &*self.queue,
                run.id(),
                i64::try_from(unix_secs(self.clock)).unwrap_or(i64::MAX),
                "wrapper",
                port,
                why,
                json!({"turn": turn}),
            );
        }
        look.vars(target)
    }

    /// Heartbeat the wrapper; a failure (the queue busy) is only logged,
    /// as the interactive wrapper does.
    fn heartbeat(&mut self) {
        let beat = match &self.owner {
            TurnOwner::Run(run) => self.queue.heartbeat_wrapper(run.id(), self.pid),
            TurnOwner::Planner { id, .. } => self.queue.heartbeat_planner(*id, self.pid),
        };
        if let Err(error) = beat {
            tracing::warn!(
                owner = %self.label(),
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
        model: Option<(String, String)>,
        registered: &mut bool,
        child_may_be_alive: &mut bool,
    ) -> Result<Turn> {
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
        let cwd = self.cwd()?;
        let (without_env, with_env) = self.sccache_turn(run_dir, turn);
        let session = match resume {
            Some(id) => TurnSession::Resume(id),
            None => TurnSession::New(&on.name),
        };
        let debug_log = run_dir.join(super::planner::PLANNER_DEBUG_LOG);
        let (actor, agent_program) = match &self.owner {
            TurnOwner::Run(run) => (
                ActorContext::worker(run.id(), run.task_id()),
                SessionAgent::Turn {
                    run,
                    prompt,
                    session,
                    stdout: &stdout,
                    stderr: &stderr,
                    without_env,
                    with_env: &with_env,
                },
            ),
            TurnOwner::Planner { id, plugin_dir, .. } => (
                ActorContext::instance(ActorRole::Planner, *id),
                SessionAgent::PlannerTurn {
                    target: TurnTarget {
                        role: ActorRole::Planner,
                        dir: run_dir,
                        cwd: &cwd,
                        debug_log: Some(&debug_log),
                        plugin_dir: *plugin_dir,
                        broker_required: None,
                    },
                    prompt,
                    session,
                    stdout: &stdout,
                    stderr: &stderr,
                },
            ),
        };
        let events: &dyn super::RunLog = &*self.queue;
        let mut executor = HostActorExecutor::new(self.db)
            .with_provider(agent)
            .with_spawner(self.spawner)
            .with_events(events);
        if let Some(service) = self.queue_service {
            executor = executor.with_queue_service(service);
        }
        let spawned = executor
            .spawn(ActorExecutionSpec::new(
                actor,
                WorkspaceAccess::Write(cwd.clone()),
                ActorProgram::SessionAgent {
                    agent: agent_program,
                    model: model
                        .as_ref()
                        .map(|(model, effort)| (model.as_str(), effort.as_str())),
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
        match &self.owner {
            TurnOwner::Run(run) => {
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
                    // Every later turn is a process of its own: it is the
                    // run's agent while it runs, so the supervisor's process
                    // watches see it and its helpers, not the turn that
                    // ended.
                    self.queue
                        .register_turn_agent(run.id(), self.pid, child.id())?;
                }
            }
            // A planner's agent is its turn while it runs.
            TurnOwner::Planner { id, .. } => {
                self.queue
                    .register_planner_agent(*id, self.pid, child.id())?;
                *registered = true;
            }
        }
        self.started(turn, resume, on, request, Some(child.id()), limits)?;
        let (exit, stop, mut tail) =
            self.follow(run_dir, turn, on, child, reader, stdout, limits)?;
        // What it wrote after the last look.
        reader.stamp(now_millis(self.clock));
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

    /// Write the `commands` of a run's Codex turn `turn` to its file
    /// ([`commands_path`]) before its end is recorded, for its session's
    /// work breakdown: Codex has no transcript to read them from. A turn
    /// that ran none (or could not start) gets an empty one, which tells
    /// it from a turn whose file was never written. Failing to write it is
    /// only logged: the span's close then records no breakdown and says
    /// why.
    fn write_commands(&self, turn: u64, commands: Vec<crate::domain::turn::TurnCommand>) {
        let written = self.dir().and_then(|run_dir| {
            let path = commands_path(&run_dir, turn);
            let tmp = path.with_extension("jsonl.tmp");
            let mut text = String::new();
            for command in &commands {
                text.push_str(&serde_json::to_string(command)?);
                text.push('\n');
            }
            self.files.write(&tmp, text.as_bytes())?;
            self.files.rename(&tmp, &path)?;
            Ok(())
        });
        if let Err(error) = written {
            tracing::warn!("the commands of turn {turn} could not be written: {error:#}");
        }
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
        let start = pid
            .and_then(|pid| self.processes.start_identity(pid))
            .map(|start| crate::domain::background_wrapper::start_token(&start));
        // A planner's turn names the model and effort its opener started
        // it with, which its session span keeps (ADR-t1394-2 decision 4).
        let launch = match &self.owner {
            TurnOwner::Planner { .. } => self
                .model()?
                .map(|(model, effort)| json!({"model": model, "effort": effort})),
            TurnOwner::Run(_) => None,
        };
        let mut payload = json!({
            "turn": turn,
            "resume": resume.is_some(),
            "request": request.map(|r| r.seq),
            "what": what,
            "pid": pid,
            // The process's start, which tells it from another that
            // takes its pid: a background session's stop finds a turn
            // its dead wrapper left by it (ADR-t1404-1 decision 3).
            "start": start,
            "provider": on.provider,
            // An agent that names its own session names a new one in
            // its output.
            "session_id": resume
                .map(str::to_owned)
                .or_else(|| (!from_output).then(|| on.name.clone())),
            "silence_secs": limits.silence_secs,
            "limit_secs": limits.limit_secs,
        });
        if let Some(launch) = launch {
            payload["launch"] = launch;
        }
        self.record(EventKind::TurnStarted, payload)?;
        say(self.background, &format!("turn {turn} started: {what}"));
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
        mut result: TurnResult,
    ) -> Result<Turn> {
        let (cost, session_cost) = self.turn_cost(turn, &result)?;
        result.cost_usd = cost;
        let (tokens, tokens_total, total_by_model) =
            self.turn_tokens(turn, &result, cost, session_cost)?;
        // The provider the turn ran on: the wrapper's copy of the run may
        // predate a switch (ADR-t813-2).
        let provider = self.provider_now()?;
        // A reader that read commands (Codex's) writes them whatever the
        // run is on now, and a Codex turn that could not start an empty
        // file.
        if (result.commands.is_some() || provider == Provider::Codex)
            && matches!(self.owner, TurnOwner::Run(_))
        {
            self.write_commands(turn, result.commands.take().unwrap_or_default());
        }
        let mut payload = json!({
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
            "tokens_total": tokens_total.as_ref().map(TokenUsage::payload),
            "permission_denials": result.permission_denials.len(),
            "denied_tools": result.permission_denials,
        });
        // The runtime's kinds of token, which the span sums (ADR-t813-2
        // decision 7), in the form of every Execution (ADR-t1486-1).
        tokens.record(&mut payload);
        // The session's running totals the turn's cost and tokens were
        // taken from, which the session's next turn takes its own from
        // (task 1199).
        if let Some(total) = session_cost {
            payload["session_cost_usd"] = json!(total);
        }
        if !total_by_model.is_empty() {
            payload["tokens_total_by_model"] =
                total_by_model.iter().map(ModelTokens::payload).collect();
        }
        self.record(EventKind::TurnFinished, payload)?;
        // The idle marker the supervisor's watches read, written only once
        // the turn is recorded.
        let marker = self.idle_marker_path()?;
        let tmp = marker.with_extension("json.tmp");
        self.files.write(
            &tmp,
            idle_marker(&self.marker_session(), turn, outcome, &result)
                .to_string()
                .as_bytes(),
        )?;
        self.files.rename(&tmp, &marker)?;
        say(
            self.background,
            &format!(
                "turn {turn} {}{}{}",
                outcome.as_str(),
                failure.map_or(String::new(), |f| format!(" ({})", f.as_str())),
                result
                    .message
                    .as_deref()
                    .map_or(String::new(), |m| format!(": {m}"))
            ),
        );
        Ok(Turn {
            outcome,
            failure,
            result,
        })
    }

    /// The turn's own cost, and the session's running total when the
    /// provider gives that instead (Claude's `total_cost_usd`, task 1199):
    /// what the turn added to the total the session's last turn recorded
    /// ([`turn_own_cost`]); the whole total without a session id.
    fn turn_cost(&mut self, turn: u64, result: &TurnResult) -> Result<(Option<f64>, Option<f64>)> {
        let Some(total) = result.cost_usd.filter(|_| result.cost_cumulative) else {
            return Ok((result.cost_usd, None));
        };
        let Some(session) = result.session_id.as_deref() else {
            return Ok((Some(total), Some(total)));
        };
        let events = self.events()?;
        Ok((
            Some(turn_own_cost(&events, turn, session, total)),
            Some(total),
        ))
    }

    /// The turn's own tokens as its `turn_finished` records them, with its
    /// own cost `cost` when the provider's was the session's total
    /// (`session_cost`); and the session's running totals, overall and per
    /// model, when the provider gives those instead. Claude's
    /// `modelUsage` (ADR-t1486-1): what the turn added to the totals the
    /// session's last turn recorded ([`turn_own_models`]). Codex's
    /// rollouts: their records of the turn's root turns but those the
    /// thread's earlier turns counted ([`counted_rollout_turns`]), with the
    /// thread's total kept as `tokens_total`. Codex's thread total when the
    /// rollouts cannot be counted: the total less the one the session's
    /// last turn recorded as `tokens_total`, the whole total for a
    /// session's first (ADR-t813-2 decision 7).
    fn turn_tokens(
        &mut self,
        turn: u64,
        result: &TurnResult,
        cost: Option<f64>,
        session_cost: Option<f64>,
    ) -> Result<(ExecutionTokens, Option<TokenUsage>, Vec<ModelTokens>)> {
        let mut own = ExecutionTokens {
            tokens: result.tokens.clone(),
            by_model: result.tokens_by_model.clone(),
            source: result.tokens_source,
            reason: result.tokens_reason,
            children: result.children,
            turns: Vec::new(),
            context: result.context.clone(),
        };
        if let Some(rollout) = &result.rollout {
            let counted = match result.session_id.as_deref() {
                Some(session) => counted_rollout_turns(&self.events()?, session),
                None => Vec::new(),
            };
            return Ok((rollout.tokens(&counted), result.tokens.clone(), Vec::new()));
        }
        let Some(mut total) = result.tokens.clone().filter(|_| result.tokens_cumulative) else {
            if let Some(tokens) = own.tokens.as_mut().filter(|_| session_cost.is_some()) {
                tokens.cost_usd = cost;
            }
            return Ok((own, None, Vec::new()));
        };
        if result.tokens_source == Some(TokenSource::ModelUsage) {
            let models = match result.session_id.as_deref() {
                Some(session) => {
                    turn_own_models(&self.events()?, turn, session, &result.tokens_by_model)
                }
                None => result.tokens_by_model.clone(),
            };
            own.tokens = Some(ModelTokens::total(&models, cost));
            own.by_model = models;
            total.cost_usd = session_cost.or(total.cost_usd);
            return Ok((own, Some(total), result.tokens_by_model.clone()));
        }
        own.tokens = Some(match result.session_id.as_deref() {
            Some(session) => thread_total_own(&self.events()?, session, &total),
            None => TokenUsage {
                messages: 1,
                ..total.clone()
            },
        });
        Ok((own, Some(total), Vec::new()))
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
        self.record(
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
        let mut stop: Option<Stop> = None;
        // A `required` run's turn started in its own permission mode
        // (ADR-t838-1), as the executor found the run when it started it.
        let broker_required = match &self.owner {
            TurnOwner::Run(run) => matches!(
                super::broker_run::worker_broker(run),
                Ok(super::broker_run::WorkerBroker::Required(_))
            ),
            TurnOwner::Planner { .. } => false,
        };
        loop {
            reader.stamp(now_millis(self.clock));
            let lines = tail.read(self.files, stdout, false);
            if !lines.is_empty() {
                last_output = Instant::now();
            }
            for line in lines {
                for signal in reader.line(&line) {
                    // Identification precedes acting on an unusable observation.
                    if let TurnSignal::Started {
                        session_id: Some(id),
                        ..
                    } = &signal
                    {
                        self.identify(turn, on, id)?;
                    }
                    let expected = if matches!(signal, TurnSignal::Started { .. }) {
                        self.agent(on.provider)?
                            .turn_permission_mode(broker_required)
                    } else {
                        None
                    };
                    stop = observed_stop(stop, &signal, expected);
                    match signal {
                        TurnSignal::Started { .. } | TurnSignal::Unusable(..) => {}
                        TurnSignal::Said(text) => say(self.background, &text),
                        TurnSignal::Tool(tool) => say(self.background, &format!("→ {tool}")),
                    }
                }
            }
            if stop.is_none()
                && let Some(exit) = child.try_wait()?
            {
                // What the turn left running in its group ends with it,
                // as the worker is told. What it left outside its group is
                // left running, on purpose: its parent is 1 now, so it is
                // not told from another process by kinship; pids listed
                // earlier in the turn may belong to another process by now;
                // and before a turn ends Codex waits for its commands and
                // Claude stops its background tasks, so only what the agent
                // detached (`nohup … &`) is left. The recovery job's `stop_processes` stops it by the
                // run's worktree.
                let _ = child.kill_group();
                return Ok((Some(exit), None, tail));
            }
            stop = if stop.is_some() {
                stop_decision(stop, false, false, Duration::ZERO, Duration::ZERO, limits)
            } else {
                stop_decision(
                    None,
                    self.files.is_file(&exit_path(run_dir)),
                    reader.heartbeats(),
                    last_output.elapsed(),
                    started.elapsed(),
                    limits,
                )
            };
            if let Some(stop) = stop {
                say(self.background, &format!("stopping the turn: {}", stop.why));
                stop_turn(self.processes, child)?;
                let _ = child.wait();
                return Ok((None, Some(stop), tail));
            }
            self.heartbeat();
            thread::sleep(self.provider.wait_interval());
        }
    }
}

/// The look just before a run's turn on either provider starts, a new
/// turn or one that resumes the session alike (ADR-t2086-1): the server
/// of `target` looked at through `server` (which starts nothing) and the
/// guard made in `run_dir`.
fn turn_look(
    target: &SccacheTarget,
    server: &dyn SccacheServer,
    run_dir: &Path,
) -> crate::domain::sccache::GuardLook {
    crate::application::sccache::look(server, target, run_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::memory_files::MemoryFiles;

    fn limits() -> TurnLimits {
        TurnLimits {
            silence_secs: 1,
            limit_secs: 2,
            silence_ms: Some(500),
            limit_ms: Some(1500),
        }
    }

    #[test]
    fn silence_and_limit_use_elapsed_values_at_the_exact_threshold() {
        let limits = limits();
        let judge = |heartbeat, quiet, elapsed| {
            stop_decision(
                None,
                false,
                heartbeat,
                Duration::from_millis(quiet),
                Duration::from_millis(elapsed),
                limits,
            )
        };
        assert_eq!(judge(true, 499, 1499), None);
        assert_eq!(judge(true, 0, 0), None);
        assert_eq!(
            judge(true, 500, 1499),
            Some(Stop {
                outcome: TurnOutcome::Silent,
                failure: None,
                why: "no output for 1s".into(),
            })
        );
        // Output resets silence, but never the turn's total running time.
        assert_eq!(
            judge(true, 0, 1500),
            Some(Stop {
                outcome: TurnOutcome::TimedOut,
                failure: None,
                why: "the turn ran past 2s".into(),
            })
        );
        assert_eq!(judge(false, 9999, 1499), None);
        assert_eq!(
            judge(false, 9999, 1500).unwrap().outcome,
            TurnOutcome::TimedOut
        );
        assert_eq!(judge(true, 500, 1500).unwrap().outcome, TurnOutcome::Silent);
        assert_eq!(judge(true, 501, 1501).unwrap().outcome, TurnOutcome::Silent);
        // The former integration case's millisecond limit still records
        // its rounded whole seconds, without waiting for a real clock.
        let short = TurnLimits {
            silence_ms: Some(800),
            limit_ms: Some(500),
            limit_secs: 1,
            ..limits
        };
        assert_eq!(
            stop_decision(
                None,
                false,
                true,
                Duration::from_millis(499),
                Duration::from_millis(499),
                short
            ),
            None
        );
        assert_eq!(
            stop_decision(
                None,
                false,
                true,
                Duration::from_millis(0),
                Duration::from_millis(500),
                short
            ),
            Some(Stop {
                outcome: TurnOutcome::TimedOut,
                failure: None,
                why: "the turn ran past 1s".into(),
            })
        );
        assert_eq!(
            stop_decision(
                None,
                false,
                true,
                Duration::from_millis(800),
                Duration::from_millis(800),
                short
            ),
            Some(Stop {
                outcome: TurnOutcome::Silent,
                failure: None,
                why: "no output for 1s".into(),
            })
        );
        // The seconds fallback has the same inclusive threshold.
        let seconds = TurnLimits {
            silence_ms: None,
            limit_ms: None,
            ..limits
        };
        assert_eq!(
            stop_decision(
                None,
                false,
                true,
                Duration::from_millis(999),
                Duration::from_millis(1999),
                seconds
            ),
            None
        );
        assert_eq!(
            stop_decision(
                None,
                false,
                true,
                Duration::from_secs(1),
                Duration::from_secs(2),
                seconds
            )
            .unwrap()
            .outcome,
            TurnOutcome::Silent
        );
        assert_eq!(
            stop_decision(
                None,
                false,
                false,
                Duration::ZERO,
                Duration::from_secs(2),
                seconds
            )
            .unwrap()
            .outcome,
            TurnOutcome::TimedOut
        );
    }

    #[test]
    fn exit_wins_over_time_limits_and_the_first_observation_wins_over_exit() {
        let elapsed = Duration::from_secs(10);
        assert_eq!(
            stop_decision(None, true, true, elapsed, elapsed, limits()),
            Some(Stop {
                outcome: TurnOutcome::Stopped,
                failure: None,
                why: "the supervisor asked the session to exit".into(),
            })
        );
        for (outcome, failure) in [
            (TurnOutcome::LaunchMismatch, None),
            (TurnOutcome::Failed, Some(TurnFailure::Authentication)),
            (TurnOutcome::Failed, Some(TurnFailure::UsageLimit)),
        ] {
            let observed = Stop {
                outcome,
                failure,
                why: "first observation".into(),
            };
            assert_eq!(
                stop_decision(Some(observed), true, true, elapsed, elapsed, limits()),
                Some(Stop {
                    outcome,
                    failure,
                    why: "first observation".into(),
                })
            );
        }
    }

    #[test]
    fn permission_modes_must_match_only_when_the_provider_requires_one() {
        let started = |mode: Option<&str>| TurnSignal::Started {
            session_id: Some("s".into()),
            model: None,
            permission_mode: mode.map(str::to_owned),
        };
        assert_eq!(
            observed_stop(None, &started(Some("auto")), Some("auto")),
            None
        );
        for mode in [None, Some("default"), Some("auto")] {
            assert_eq!(observed_stop(None, &started(mode), None), None);
        }
        for (mode, said) in [(Some("default"), "default"), (None, "(none)")] {
            assert_eq!(
                observed_stop(None, &started(mode), Some("auto")),
                Some(Stop {
                    outcome: TurnOutcome::LaunchMismatch,
                    failure: None,
                    why: format!("the agent started in permission mode {said} instead of auto"),
                })
            );
        }
    }

    #[test]
    fn unusable_signals_keep_their_failure_and_message_in_output_order() {
        for failure in [
            TurnFailure::Authentication,
            TurnFailure::UsageLimit,
            TurnFailure::Launch,
            TurnFailure::Model,
            TurnFailure::Sandbox,
        ] {
            assert_eq!(
                observed_stop(
                    None,
                    &TurnSignal::Unusable(failure, "provider said why".into()),
                    None
                ),
                Some(Stop {
                    outcome: TurnOutcome::Failed,
                    failure: Some(failure),
                    why: "provider said why".into(),
                })
            );
        }
        let mismatch = TurnSignal::Started {
            session_id: None,
            model: None,
            permission_mode: None,
        };
        let unusable = TurnSignal::Unusable(TurnFailure::Authentication, "login failed".into());
        let first = observed_stop(None, &mismatch, Some("auto"));
        assert_eq!(
            observed_stop(first, &unusable, None).unwrap().outcome,
            TurnOutcome::LaunchMismatch
        );
        let first = observed_stop(None, &unusable, None);
        assert_eq!(
            observed_stop(first, &mismatch, Some("auto")).unwrap().why,
            "login failed"
        );
        for signal in [
            TurnSignal::Said("output".into()),
            TurnSignal::Tool("Bash".into()),
        ] {
            assert_eq!(observed_stop(None, &signal, Some("auto")), None);
        }
    }

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

    /// A clock stopped at a fixed time.
    struct At(std::time::SystemTime);

    impl Clock for At {
        fn system_time(&self) -> std::time::SystemTime {
            self.0
        }

        fn monotonic(&self) -> std::time::Instant {
            std::time::Instant::now()
        }
    }

    /// The turns' stamps (milliseconds) and the `at` of
    /// `sccache_wrapper_removed` (seconds) come from the injected clock,
    /// not the wall clock (architecture.md L4).
    #[test]
    fn stamps_and_the_sccache_removal_time_come_from_the_injected_clock() {
        let clock = At(std::time::UNIX_EPOCH + Duration::from_millis(1_700_000_000_123));
        assert_eq!(now_millis(&clock), 1_700_000_000_123);
        assert_eq!(unix_secs(&clock), 1_700_000_000);
        // A clock before the epoch reads as zero, as before.
        let before = At(std::time::UNIX_EPOCH - Duration::from_secs(1));
        assert_eq!(now_millis(&before), 0);
        assert_eq!(unix_secs(&before), 0);
    }

    #[test]
    fn a_turn_is_refused_the_servers_start_and_guarded_only_when_it_listens() {
        use crate::application::sccache::LookedAt;
        use crate::domain::sccache::{GuardLook, REFUSED_ERROR_LOG};
        let target = SccacheTarget {
            program: "/opt/bin/sccache".into(),
            port: 4300,
        };
        let run_dir = Path::new("/q/runs/r");
        let refused = ("SCCACHE_ERROR_LOG".to_owned(), REFUSED_ERROR_LOG.to_owned());
        // Nothing about the turn's provider is asked: a Claude turn and a
        // Codex turn, new or resumed, are given the same.
        let look = turn_look(&target, &LookedAt::new(Ok(true), true), run_dir);
        let (without, with) = look.vars(&target);
        assert_eq!(
            look,
            GuardLook::Guard("/q/runs/r/dagq-rustc-wrapper".into())
        );
        assert!(without.is_empty());
        assert_eq!(
            with,
            [
                refused.clone(),
                (
                    "RUSTC_WRAPPER".to_owned(),
                    "/q/runs/r/dagq-rustc-wrapper".to_owned()
                ),
                (
                    "DAGQ_SCCACHE_PROGRAM".to_owned(),
                    "/opt/bin/sccache".to_owned()
                ),
            ]
        );
        for server in [
            LookedAt::new(Ok(false), true),
            LookedAt::new(Err("refused"), true),
            LookedAt::new(Ok(true), false),
        ] {
            let look = turn_look(&target, &server, run_dir);
            let (without, with) = look.vars(&target);
            assert_eq!(look.removed().map(|(port, _)| port), Some(4300));
            assert_eq!(without, ["RUSTC_WRAPPER"]);
            assert_eq!(with, std::slice::from_ref(&refused));
        }
    }
}
