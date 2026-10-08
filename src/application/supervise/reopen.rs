//! A headless run's session lost while it waits for a person (task 1372):
//! its wrapper recorded its exit, or stopped heartbeating with its process
//! gone, while the run waited for the answer of its `worker_question` (or
//! of its `stalled` ask) and had no receipt. Nothing is lost with it: the
//! provider keeps the session, the worktree and the asks stay. So the
//! runtime opens a new workspace whose wrapper waits for the supervisor's
//! request and resumes the same session ([`crate::application::session`]'s
//! resume mode), and the wait goes on (ADR-0047's first layer, recorded as
//! `auto_repaired`). Attempts in a row are bounded: past
//! [`REOPEN_ATTEMPTS`], the run goes the way of a session that ended, to
//! its recovery job.

use super::*;
use crate::domain::{EventKind, RunEvent, RunProcess};

/// How many times in a row (with no turn of the session between them) the
/// supervisor opens a lost session again before it gives the run up.
pub(super) const REOPEN_ATTEMPTS: usize = 3;

/// The `repair` of the `auto_repaired` a reopened session records.
pub(super) const REOPEN_REPAIR: &str = "headless_session_reopened";

/// The `cause` of a `session_reopen_failed` whose wrapper could not be
/// started (or recorded): an attempt of its own.
const OPEN_FAILED: &str = "open_failed";

/// The `cause` of a `session_reopen_failed` whose previous wrapper could
/// not be stopped (or not be told to run or not), so no new one was
/// started beside it: an attempt of its own.
const CLOSE_FAILED: &str = "close_failed";

/// The `cause` of a `session_reopen_failed` whose wrapper, started by an
/// attempt, did not register: part of that attempt.
const NOT_REGISTERED: &str = "registration_timeout";

/// The reopen of one waiting run's lost session.
#[derive(Debug, Default)]
pub(super) struct ReopenWatch {
    /// When a workspace was opened whose wrapper has not registered yet.
    opened: Option<Instant>,
    /// When this process last made an attempt: the next one waits
    /// [`SessionWrappers::reopen_interval`] after it.
    last_attempt: Option<Instant>,
    /// The exit code the lost wrapper recorded (`None` for one that died):
    /// the session ends with it once no attempt is left.
    exit_code: Option<i32>,
    /// No attempt is left: a session whose processes an attempt forgot is
    /// ended by its watch as one that exited.
    gave_up: bool,
    /// Since when the lost wrapper's turn outlived it: the session is
    /// opened again once that turn ended, within the turn's limit.
    agent_since: Option<Instant>,
}

impl ReopenWatch {
    /// Whether no workspace an attempt opened waits for its wrapper: the
    /// reopen is over once the run's wrapper lives in its slot.
    pub(super) fn settled(&self) -> bool {
        self.opened.is_none()
    }

    /// The wrapper of the workspace an attempt opened registered.
    pub(super) fn registered(&mut self) {
        self.opened = None;
    }

    /// The exit code the session ends with when no attempt is left and no
    /// wrapper row holds one: the lost wrapper's, else 1 (it died).
    pub(super) fn lost_exit(&self) -> Option<i32> {
        self.gave_up.then_some(self.exit_code.unwrap_or(1))
    }
}

/// What an attempt records of the session it opens again.
struct LostSession<'a> {
    attempt: usize,
    /// The workspace the lost session (or the previous attempt) had.
    previous: &'a str,
    /// `exited`, `died` or `not_registered`.
    cause: &'a str,
    /// The lost wrapper's row; `None` after an attempt forgot it.
    wrapper: Option<&'a RunProcess>,
    open_asks: &'a [AskId],
}

/// What the wait does with its lost session.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Reopen {
    /// A session is being opened again, or will be after the interval:
    /// the wait goes on.
    Waiting,
    /// The session is not opened again: the wait ends as the session's end.
    GiveUp,
}

/// The attempts to open the run's lost session again since the session
/// last started a turn: each records either the `auto_repaired` of its
/// reopen or a `session_reopen_failed` of its own (not the
/// [`NOT_REGISTERED`] of a reopen's wrapper).
pub(super) fn attempts_in_a_row(events: &[RunEvent]) -> usize {
    let since = events
        .iter()
        .rposition(|e| e.kind == event_kind::TURN_STARTED)
        .map_or(0, |at| at + 1);
    events[since..]
        .iter()
        .filter(|e| {
            (e.kind == event_kind::AUTO_REPAIRED && e.payload["repair"] == REOPEN_REPAIR)
                || (e.kind == event_kind::SESSION_REOPEN_FAILED
                    && e.payload["cause"] != NOT_REGISTERED)
        })
        .count()
}

/// Whether the workspace an attempt opened at `opened` still waits for its
/// wrapper at `now`: within the registration `timeout`, unless a wrapper
/// row of the run shows it `lost` (the new wrapper registered and was lost
/// in turn). Past it, nothing registered and the next attempt closes it.
pub(super) fn awaits_registration(
    now: Instant,
    opened: Instant,
    lost: bool,
    timeout: Duration,
) -> bool {
    !lost && !passed(now, opened, timeout)
}

/// What the reopen does at `now` with a turn that has outlived its lost
/// wrapper since `since`: it waits for the turn to end within the turn's
/// `limit` (as the wrapper would have), and gives the session up at it.
pub(super) fn turn_outlived(now: Instant, since: Instant, limit: Duration) -> Reopen {
    if passed(now, since, limit) {
        Reopen::GiveUp
    } else {
        Reopen::Waiting
    }
}

/// Whether the next attempt waits at `now` for the reopen `interval` since
/// this process's `last_attempt`; the first attempt does not wait.
pub(super) fn attempt_waits(
    now: Instant,
    last_attempt: Option<Instant>,
    interval: Duration,
) -> bool {
    last_attempt.is_some_and(|at| !passed(now, at, interval))
}

impl Supervisor<'_> {
    /// The waiting run's session is gone (its wrapper `lost`, or no wrapper
    /// row since an attempt forgot it): open it again when the run is
    /// headless, waits in its first session with no receipt, holds an
    /// unclosed `worker_question` or `stalled` ask, its last turn's process
    /// is gone, and attempts in a row remain. A run given up has the
    /// workspace of an attempt whose wrapper never registered closed.
    pub(super) fn reopen_lost_session(
        &mut self,
        slot: &mut Slot,
        processes: &[RunProcess],
        lost: Option<&RunProcess>,
    ) -> Result<Reopen> {
        let run = slot.run.clone();
        let Phase::Session(watch) = &mut slot.phase else {
            return Ok(Reopen::GiveUp);
        };
        let known = self.reopens.contains_key(run.id());
        let reopen = self.reopens.entry(run.id().clone()).or_default();
        if let Some(lost) = lost {
            reopen.exit_code = lost.exit_code;
        } else if !known {
            // No wrapper row and nothing of this process's: another process
            // (before a handoff or an adoption) forgot the processes for an
            // attempt whose wrapper may still be starting. It gets the
            // registration's time before the next attempt closes it.
            reopen.opened = Some(self.generators.clock.monotonic());
            return Ok(Reopen::Waiting);
        }
        let outcome = self.reopen_or_not(&run, watch, processes, lost)?;
        if let Reopen::GiveUp = outcome {
            if lost.is_none() {
                // The workspace of an attempt whose wrapper never
                // registered: nothing may start in it later.
                let workspace = watch.workspace.clone();
                self.close_lost_workspace(&run, &workspace, None);
            }
            if let Some(reopen) = self.reopens.get_mut(run.id()) {
                reopen.opened = None;
                reopen.gave_up = true;
            }
        }
        Ok(outcome)
    }

    /// [`Self::reopen_lost_session`] for a headless first session.
    fn reopen_or_not(
        &mut self,
        run: &TaskRun,
        watch: &mut SessionWatch,
        processes: &[RunProcess],
        lost: Option<&RunProcess>,
    ) -> Result<Reopen> {
        let now = self.generators.clock.monotonic();
        let reopen = self.reopens.entry(run.id().clone()).or_default();
        if let Some(opened) = reopen.opened {
            if awaits_registration(
                now,
                opened,
                lost.is_some(),
                self.sessions.registration_timeout(),
            ) {
                return Ok(Reopen::Waiting);
            }
            reopen.opened = None;
            if lost.is_none() {
                // Nothing registered in the workspace the attempt opened;
                // the next attempt closes it.
                self.reopen_failed(run, &watch.workspace, NOT_REGISTERED, None)?;
            }
        }
        if watch.receipt_seen || self.files.is_file(&watch.receipt_path) {
            return Ok(Reopen::GiveUp);
        }
        let waits = self.queue.has_unclosed_worker_question(run.id())?
            || self.queue.has_unclosed_ask(run.id(), AskKind::Stalled)?;
        if !waits {
            return Ok(Reopen::GiveUp);
        }
        // A turn that outlived its wrapper still works in the worktree (a
        // wrapper lost in the middle of a turn): no second session beside
        // it. The session is opened again once the turn ended, which the
        // turn's limit bounds as the wrapper would have; past it, the run
        // goes to its recovery job, which may stop the turn's processes.
        let reopen = self.reopens.entry(run.id().clone()).or_default();
        if let Some(agent) = processes
            .iter()
            .find(|p| p.role == "agent" && self.processes.alive(p.pid))
        {
            let limit = self.stall.threshold("turn_limit_secs").unwrap_or_default();
            let since = match reopen.agent_since {
                Some(since) => since,
                None => {
                    info!(run_id = %run.id(), "run {} lost its wrapper while its turn (pid {}) still runs; waiting for the turn to end before opening the session again", run.id(), agent.pid);
                    *reopen.agent_since.insert(now)
                }
            };
            let outcome = turn_outlived(now, since, limit);
            if let Reopen::GiveUp = outcome {
                warn!(run_id = %run.id(), "the turn (pid {}) of run {} outlived its lost wrapper past the turn's limit; giving the session up", agent.pid, run.id());
            }
            return Ok(outcome);
        }
        reopen.agent_since = None;
        let attempts = attempts_in_a_row(&self.queue.run_events(run.id())?);
        if attempts >= REOPEN_ATTEMPTS {
            warn!(run_id = %run.id(), "the lost session of {} was opened again {attempts} times in a row; giving it up", run.id());
            return Ok(Reopen::GiveUp);
        }
        let reopen = self.reopens.entry(run.id().clone()).or_default();
        if attempt_waits(now, reopen.last_attempt, self.sessions.reopen_interval()) {
            return Ok(Reopen::Waiting);
        }
        reopen.last_attempt = Some(now);
        let attempt = attempts + 1;
        let given_up = || {
            if attempt >= REOPEN_ATTEMPTS {
                Reopen::GiveUp
            } else {
                Reopen::Waiting
            }
        };
        let previous = watch.workspace.clone();
        // What is left of the lost session (or of an attempt whose wrapper
        // never registered) is closed first: no two wrappers take the
        // run's requests, so nothing opens beside one that may live.
        if !self.close_lost_workspace(run, &previous, Some(attempt)) {
            self.reopen_failed(run, &previous, CLOSE_FAILED, None)?;
            return Ok(given_up());
        }
        let cause = match lost {
            Some(wrapper) if wrapper.exited_at.is_some() => "exited",
            Some(_) => "died",
            None => "not_registered",
        };
        let open_asks: Vec<AskId> = self
            .queue
            .unclosed_run_asks(run.id())?
            .into_iter()
            .filter(|ask| matches!(ask.kind, AskKind::WorkerQuestion | AskKind::Stalled))
            .map(|ask| ask.id)
            .collect();
        let lost_session = LostSession {
            attempt,
            previous: &previous,
            cause,
            wrapper: lost,
            open_asks: &open_asks,
        };
        match self.open_session_again(run, &lost_session) {
            Ok(workspace) => {
                info!(run_id = %run.id(), "run {} lost its session ({cause}) while it waited; opened it again in workspace {workspace} (attempt {attempt})", run.id());
                // The registration's time runs from the open, not from
                // before it.
                let opened = self.generators.clock.monotonic();
                watch.workspace = workspace;
                watch.startup = opened;
                watch.silent = false;
                if let Some(reopen) = self.reopens.get_mut(run.id()) {
                    reopen.opened = Some(opened);
                }
                Ok(Reopen::Waiting)
            }
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: its lost session could not be opened again (attempt {attempt}): {error:#}", run.id());
                self.reopen_failed(run, &previous, OPEN_FAILED, Some(&error))?;
                Ok(given_up())
            }
        }
    }

    /// Stop the wrapper `workspace` of the run's lost session while it
    /// runs, and record `workspace_closed` (with the `attempt` it is
    /// stopped for); whether nothing of it is left running. A workspace
    /// of a session from before ADR-t1433-3 is left to a person
    /// ([`Self::run_session_open`]).
    fn close_lost_workspace(
        &mut self,
        run: &TaskRun,
        workspace: &str,
        attempt: Option<usize>,
    ) -> bool {
        match self.run_session_open(workspace) {
            Ok(true) => match stop_session(self.sessions, workspace, StopRoute::Reopen) {
                Ok(()) => {
                    if let Err(error) = self.queue.record_workspace_closed(
                        run.id(),
                        workspace,
                        json!({"by": "supervisor", "reopen": attempt}),
                    ) {
                        warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: the close of workspace {workspace} could not be recorded: {error:#}", run.id());
                    }
                    true
                }
                Err(error) => {
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: the wrapper {workspace} of its lost session could not be stopped: {error:#}", run.id());
                    false
                }
            },
            Ok(false) => true,
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: whether the wrapper {workspace} runs could not be told: {error:#}", run.id());
                false
            }
        }
    }

    /// Forget the lost session's processes and start a background wrapper
    /// that resumes the run's session on the supervisor's request, with
    /// the worker's env, `[run.env]` and a fresh runtime snapshot; record
    /// its handle as the run's session and the repair. The new handle.
    fn open_session_again(&mut self, run: &TaskRun, lost: &LostSession<'_>) -> Result<String> {
        // Only the lost wrapper's row (or none) is forgotten: one that
        // registered meanwhile refuses the attempt.
        self.queue
            .clear_lost_session(run.id(), &self.token, lost.wrapper.map(|w| w.pid))?;
        // A turn its wrapper never saw end (the wrapper was lost in the
        // middle of it) ended with its process: its idle marker says so, so
        // that the reopened session is between turns and takes the answer
        // as its next turn.
        let lost_turn = self.end_lost_turn(run)?;
        let run_dir = PathBuf::from(run.run_dir().context("missing run directory")?);
        self.files
            .copy(&self.layout.runner, &run_dir.join(RUN_RUNNER_FILE))
            .context("snapshot runtime binary")?;
        self.prepare_turns(run, &run_dir)?;
        self.broker_grant_or_refuse(run)?;
        let run_env = self.verifier.run_env(&run_dir)?;
        self.warn_ignored_wrapper_setting();
        let log = self.session_log(&run_dir, Some(lost.attempt), true);
        let command = background::wrapper_command(vec![
            path_text(&run_dir.join(RUN_RUNNER_FILE))?,
            "--db".into(),
            path_text(&self.layout.db)?,
            "session".into(),
            "--run".into(),
            run.id().to_string(),
            "--lease".into(),
            self.token.to_string(),
            "--claude".into(),
            path_text(&self.layout.claude)?,
            "--codex".into(),
            path_text(&self.layout.codex)?,
            "--resume".into(),
        ]);
        let workspace = self
            .actors()
            .spawn(ActorExecutionSpec::new(
                ActorContext::worker(run.id(), run.task_id()),
                WorkspaceAccess::Write(PathBuf::from(
                    run.worktree_path().context("missing worktree")?,
                )),
                ActorProgram::RunSession {
                    run,
                    wrapper: command,
                    run_env,
                    log: &log,
                },
            ))?
            .workspace()?;
        let repaired = json!({
            "layer": "runtime",
            "repair": REOPEN_REPAIR,
            "conditions": {
                "worker_mode": "headless",
                "waiting": true,
                "receipt": false,
                "open_asks": lost.open_asks,
                "lost_turn": lost_turn,
                "attempt": lost.attempt,
                "attempts": REOPEN_ATTEMPTS,
            },
            "detail": {
                "workspace_id": workspace,
                "previous_workspace": lost.previous,
                "cause": lost.cause,
                "exit_code": lost.wrapper.and_then(|w| w.exit_code),
            },
        });
        let attempt_no = u64::try_from(lost.attempt).unwrap_or(u64::MAX);
        if let Err(error) =
            self.queue
                .session_reopened(run.id(), &self.token, &workspace, attempt_no, repaired)
        {
            // Unrecorded, nothing would find the wrapper to stop it.
            return Err(match stop_session(self.sessions, &workspace, StopRoute::Reopen) {
                Ok(()) => error.context(format!(
                    "the reopened background wrapper {workspace} of run {} could not be recorded and was stopped",
                    run.id()
                )),
                Err(stop) => error.context(format!(
                    "the reopened background wrapper {workspace} of run {} could not be recorded, and stopping it failed: {stop:#}",
                    run.id()
                )),
            });
        }
        self.record_launch(run, &workspace, &log, None)?;
        Ok(workspace)
    }

    /// The number of the run's last turn when it started and its wrapper
    /// recorded no end of it, after writing its idle marker (outcome
    /// `succeeded`: the session goes on); `None` when the last turn ended.
    fn end_lost_turn(&mut self, run: &TaskRun) -> Result<Option<u64>> {
        let events = self.queue.run_events(run.id())?;
        let Some(turn) = events
            .iter()
            .rfind(|e| e.kind == event_kind::TURN_STARTED)
            .and_then(|e| e.payload["turn"].as_u64())
        else {
            return Ok(None);
        };
        let finished = events.iter().any(|e| {
            e.kind == event_kind::TURN_FINISHED && e.payload["turn"].as_u64() == Some(turn)
        });
        if finished {
            return Ok(None);
        }
        let marker = run.idle_marker_path()?;
        let tmp = marker.with_extension("json.tmp");
        let content = crate::domain::turn::idle_marker(
            run.id().as_str(),
            turn,
            crate::domain::turn::TurnOutcome::Succeeded,
            &crate::domain::turn::TurnResult::default(),
        );
        self.files.write(&tmp, content.to_string().as_bytes())?;
        self.files.rename(&tmp, &marker)?;
        info!(run_id = %run.id(), "turn {turn} of run {} ended with its lost wrapper; its idle marker is written", run.id());
        Ok(Some(turn))
    }

    fn reopen_failed(
        &mut self,
        run: &TaskRun,
        workspace: &str,
        cause: &str,
        error: Option<&anyhow::Error>,
    ) -> Result<()> {
        // An attempt's own failure is counted with the attempts; a wrapper
        // that did not register belongs to the attempt that opened it.
        let attempt = attempts_in_a_row(&self.queue.run_events(run.id())?)
            + usize::from(cause != NOT_REGISTERED);
        self.queue.record_runtime_event(
            run.id(),
            EventKind::SessionReopenFailed,
            json!({
                "attempt": attempt,
                "attempts": REOPEN_ATTEMPTS,
                "cause": cause,
                "workspace_id": workspace,
                "error": error.map(|e| format!("{e:#}")),
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;

    const MS: Duration = Duration::from_millis(1);

    /// The workspace an attempt opened waits for its wrapper until the
    /// registration timeout, not at it; a lost wrapper row ends the wait at
    /// once.
    #[test]
    fn an_opened_workspace_waits_for_its_wrapper_until_the_registration_timeout() {
        let opened = Instant::now();
        let timeout = Duration::from_secs(60);
        assert!(awaits_registration(
            opened + timeout - MS,
            opened,
            false,
            timeout
        ));
        assert!(!awaits_registration(
            opened + timeout,
            opened,
            false,
            timeout
        ));
        assert!(!awaits_registration(opened, opened, true, timeout));
    }

    /// A turn that outlived its lost wrapper is waited for until the turn's
    /// limit and given up at it; a limit of zero gives it up at once.
    #[test]
    fn a_turn_that_outlived_its_wrapper_is_given_up_at_the_turn_limit() {
        let since = Instant::now();
        let limit = Duration::from_secs(3600);
        assert_eq!(
            turn_outlived(since + limit - MS, since, limit),
            Reopen::Waiting
        );
        assert_eq!(turn_outlived(since + limit, since, limit), Reopen::GiveUp);
        assert_eq!(turn_outlived(since, since, Duration::ZERO), Reopen::GiveUp);
    }

    /// The first attempt does not wait; the next waits for the reopen
    /// interval since the last one, and goes at it.
    #[test]
    fn the_next_attempt_waits_for_the_reopen_interval() {
        let last = Instant::now();
        let interval = Duration::from_secs(10);
        assert!(!attempt_waits(last + interval * 100, None, interval));
        assert!(attempt_waits(last + interval - MS, Some(last), interval));
        assert!(!attempt_waits(last + interval, Some(last), interval));
    }

    fn event(kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: "2026-10-02T00:00:00Z".into(),
            actor: None,
        }
    }

    /// The attempts in a row are the reopens and the workspaces that could
    /// not be opened since the session's last turn; a wrapper that did not
    /// register in a workspace an attempt opened is no attempt of its own.
    #[test]
    fn the_attempts_in_a_row_restart_with_a_turn() {
        let reopened = || event("auto_repaired", json!({"repair": REOPEN_REPAIR}));
        let failed = |cause: &str| event("session_reopen_failed", json!({"cause": cause}));
        let events = vec![
            reopened(),
            event("turn_started", json!({})),
            reopened(),
            failed(NOT_REGISTERED),
            failed(OPEN_FAILED),
            failed(CLOSE_FAILED),
            event("auto_repaired", json!({"repair": "exit_forced_close"})),
        ];
        assert_eq!(attempts_in_a_row(&events), 3);
        assert_eq!(attempts_in_a_row(&events[..2]), 0);
        assert_eq!(attempts_in_a_row(&[]), 0);
    }
}
