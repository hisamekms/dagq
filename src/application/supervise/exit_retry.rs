//! The retries of a `/exit` the session held back past its timeout, or
//! that never reached it (ADR-0047 decision 25): [`ExitRetry`]. Each retry
//! reads the screen first and sends only what it shows to be safe: the
//! keys of a known dialog by its rule (decision 29), Enter alone for a
//! `/exit` left in the input box (decision 31), `/exit` again only into an
//! input box that is drawn empty with no dialog up, and nothing over any
//! other dialog or into a screen that cannot be read. The retries are
//! `[exit]` of `dagq.toml` ([`ExitConfig`]): how many, and the wait after
//! each before the next step. Each is recorded as `exit_retried`; a session
//! that exits during them as `auto_repaired` (`repair: exit_retry`).

use super::*;
use crate::domain::exit::{CAUSE_BACKEND_TIMEOUT, CAUSE_EXIT_TIMEOUT, ExitScreen};
use crate::domain::{EventKind, RunEvent};

/// Where the retries of a session's latest `/exit` stand. They start at
/// its timeout ([`ExitRetry::start`]); the first is made at once, and each
/// next one the retry's interval after the one before.
#[derive(Debug, Default)]
pub(super) struct ExitRetry {
    /// The retries made (also by a previous supervisor).
    pub(super) attempts: usize,
    /// When the timeout was recorded, or the last retry made; `None`
    /// before the timeout.
    pub(super) last: Option<Instant>,
    /// The `/exit` never reached the session ([`CAUSE_BACKEND_TIMEOUT`]),
    /// rather than being held back past its timeout
    /// ([`CAUSE_EXIT_TIMEOUT`]).
    pub(super) unsent: bool,
    /// What the last retry read on the screen.
    pub(super) screen: Option<ExitScreen>,
    /// A retry typed `/exit` into the session.
    pub(super) typed: bool,
    /// The retries are used up (or none were to be made): the session's
    /// `stuck_exit` path goes on.
    pub(super) done: bool,
}

/// What [`ExitRetry::poll`] left to its watch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RetryStep {
    /// A retry was made or is waited for.
    Waiting,
    /// The retries were used up at this poll, `retried` when any was made
    /// (not with `retries = 0` or a headless session): the watch decides
    /// once what follows.
    UsedUp { retried: bool },
    /// Used up at an earlier poll.
    Over,
}

impl ExitRetry {
    /// The `/exit` timed out (or never got there, `cause`
    /// [`CAUSE_BACKEND_TIMEOUT`]): the retries start now. A request whose
    /// retries started already keeps them: a recovery job's repair that
    /// gave the session the exit timeout again does not retry again
    /// (ADR-0047 decision 38, at most `[exit] retries` per stage).
    pub(super) fn start(&mut self, cause: &'static str) {
        if self.last.is_none() && !self.done {
            self.last = Some(Instant::now());
            self.unsent = cause == CAUSE_BACKEND_TIMEOUT;
        }
    }

    /// Why the `/exit` is retried, as `exit_retried`'s `cause` names it.
    pub(super) const fn cause(&self) -> &'static str {
        if self.unsent {
            CAUSE_BACKEND_TIMEOUT
        } else {
            CAUSE_EXIT_TIMEOUT
        }
    }

    /// The retries of the run's latest `/exit` as its events hold them, for
    /// a supervisor that took the run over (an adoption, a handoff): the
    /// retries recorded are not made again, and the next waits from the
    /// last one's time (`since` turns an event into an [`Instant`]). None
    /// before the request's timeout.
    pub(super) fn adopt(history: &RunHistory<'_>, since: impl Fn(&RunEvent) -> Instant) -> Self {
        let Some(timeout) = history.latest_exit_timeout() else {
            return Self::default();
        };
        let retries = history.latest_exit_retries();
        let last = retries.last().copied();
        // The previous supervisor used them up and went on to the
        // `stuck_exit` recovery job or ask: not retried, nor repaired, again.
        let used_up = history.events().iter().any(|e| {
            e.id > timeout.id
                && ((e.kind == event_kind::RECOVERY_REQUESTED
                    && e.payload["alert"] == RecoveryAlert::StuckExit.as_str())
                    || (e.kind == event_kind::ASK_OPENED && e.payload["kind"] == "stuck_exit"))
        });
        Self {
            attempts: retries.len(),
            last: Some(since(last.unwrap_or(timeout))),
            unsent: timeout.payload["unsent"] == true,
            screen: last.and_then(|e| e.payload["screen"].as_str().and_then(ExitScreen::parse)),
            typed: retries.iter().any(|e| e.payload["send"] == "exit"),
            done: used_up,
        }
    }

    /// Make the next retry when it is due, or say that they are used up.
    /// `exit_typed` is whether the supervisor typed the `/exit` (for the
    /// "Background work is running" dialog's condition). A headless session
    /// has no screen to read and its exit request stays written: it gets no
    /// retry, as with `retries = 0`.
    pub(super) fn poll(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        exit_typed: bool,
    ) -> Result<RetryStep> {
        if self.done {
            return Ok(RetryStep::Over);
        }
        let retries = sv.exit_config.retries;
        let Some(last) = self.last.filter(|_| retries > 0 && !headless(run)) else {
            self.done = true;
            return Ok(RetryStep::UsedUp { retried: false });
        };
        if self.attempts > 0 && last.elapsed() < sv.exit_config.interval(self.attempts) {
            return Ok(RetryStep::Waiting);
        }
        if self.attempts >= retries {
            self.done = true;
            return Ok(RetryStep::UsedUp { retried: true });
        }
        self.attempts += 1;
        self.retry(sv, run, workspace, exit_typed || self.typed)?;
        self.last = Some(Instant::now());
        Ok(RetryStep::Waiting)
    }

    /// One retry: read the screen, record `exit_retried` with what it
    /// allows (`send`), then send that. The record goes first, as
    /// `exit_requested` does: the session may exit on the send before it
    /// returns, and a supervisor that takes the run over must not send it
    /// again. A send that fails is logged; a known dialog's own events say
    /// whether its rule sent its keys.
    fn retry(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        exit_typed: bool,
    ) -> Result<()> {
        let (screen, text) = match sv.cmux.capture(workspace) {
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "screen of {} could not be read for a retry of its /exit: {error:#}", run.id());
                (ExitScreen::Unreadable, None)
            }
            Ok(text) => {
                let dialog = sv.signals.detect_prompt(&text).is_some()
                    || sv.signals.known_dialog(&text).is_some();
                let screen = ExitScreen::of(
                    dialog,
                    sv.signals.input_pending(&text, "/exit"),
                    sv.signals.input_ready(&text),
                );
                (screen, Some(text))
            }
        };
        let send = match (screen, &text) {
            (ExitScreen::Dialog, Some(text)) if sv.signals.known_dialog(text).is_some() => "keys",
            (ExitScreen::InputPending, _) => "enter",
            (ExitScreen::InputReady, _) => "exit",
            _ => "nothing",
        };
        self.screen = Some(screen);
        sv.queue.record_runtime_event(
            run.id(),
            EventKind::ExitRetried,
            json!({
                "attempt": self.attempts,
                "retries": sv.exit_config.retries,
                "cause": self.cause(),
                "screen": screen.as_str(),
                "send": send,
                "next_secs": sv.exit_config.interval(self.attempts).as_secs(),
                "workspace_id": workspace,
                "excerpt": text.as_deref().map(|text| sv.signals.screen_excerpt(text)),
            }),
        )?;
        info!(run_id = %run.id(), "session of {} still holds its /exit ({}): retry {} of {} read {} and sends {send}", run.id(), self.cause(), self.attempts, sv.exit_config.retries, screen.as_str());
        match send {
            "keys" => {
                let text = text.unwrap_or_default();
                answer_known_dialog(sv, run, workspace, &text, exit_typed, None)?;
            }
            "enter" => {
                if let Err(error) = sv.cmux.send_enter(workspace) {
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "Enter could not be sent again for the /exit of {}: {error:#}", run.id());
                }
            }
            "exit" => match submit(sv, run, workspace, Input::Exit, "/exit") {
                Ok(Submission::Unsent) => {
                    warn!(run_id = %run.id(), "/exit retried for {} timed out in cmux without reaching the session", run.id());
                }
                Ok(_) => self.typed = true,
                Err(error) => {
                    warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "/exit could not be typed again into {}: {error:#}", run.id());
                }
            },
            _ => (),
        }
        Ok(())
    }

    /// The session exited: when it did during the retries, they repaired
    /// the `/exit` (ADR-0047 decision 38), recorded as `auto_repaired`
    /// (`repair: exit_retry`). The session is gone, so a record that fails
    /// is only noted.
    pub(super) fn exited(&self, sv: &mut Supervisor<'_>, run: &TaskRun, workspace: &str) {
        if self.attempts == 0 || self.done {
            return;
        }
        let payload = json!({
            "layer": "runtime",
            "repair": EXIT_RETRY,
            "conditions": {
                "attempts": self.attempts,
                "cause": self.cause(),
                "screen": self.screen.map(ExitScreen::as_str),
            },
            "detail": {"workspace_id": workspace},
        });
        if let Err(error) =
            sv.queue
                .record_runtime_event(run.id(), EventKind::AutoRepaired, payload)
        {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "auto_repaired of {} could not be recorded: {error:#}", run.id());
        }
        info!(run_id = %run.id(), "session of {} exited after {} retries of its /exit", run.id(), self.attempts);
    }
}

/// `auto_repaired`'s `repair` of a `/exit` a retry got through.
pub(super) const EXIT_RETRY: &str = "exit_retry";
