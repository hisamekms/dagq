//! Every failed cmux call on record (task 109): [`RecordingBackend`] wraps
//! a [`WorkspaceBackend`] and writes `backend_call_failed` with the load
//! the call failed under. A call that timed out is made again after a
//! backoff when making it again is safe (task 326).

use crate::domain::LeaseToken;
use anyhow::Result;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, thread, time::Duration};

use super::{QueueOpener, SupervisorEnvironment, WorkspaceBackend, WorkspaceTags};
use crate::domain::{Reason, ReasonCode, RunId};

/// `backend_call_failed` keeps this many leading characters of the error.
pub const BACKEND_ERROR_CHARS: usize = 300;

/// Which attempt of a call failed: `number` of at most `of`, and the
/// backoff before the next one, `None` when the call is not made again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attempt {
    pub number: u32,
    pub of: u32,
    pub retry_after: Option<Duration>,
}

impl Attempt {
    /// The only attempt of a call that is never made again.
    pub const ONLY: Self = Self {
        number: 1,
        of: 1,
        retry_after: None,
    };
}

/// The payload of `backend_call_failed`: the call (`op`, the workspace it
/// was for, the backend's per-call timeout), its error cut to
/// [`BACKEND_ERROR_CHARS`] characters, the load it failed under — the
/// 1-minute load average (null when unavailable), the slots held and the
/// `parallel` offered (null without a supervisor) — and which attempt it
/// was (`attempt` of `max_attempts`, with `retry_after_ms`, the backoff
/// before the next one, null when there is none; task 326). `code` is
/// `backend_timeout` or `backend_failed` (ADR-0034).
#[allow(clippy::too_many_arguments)]
pub fn backend_failure_payload(
    op: &str,
    workspace_id: Option<&str>,
    timeout: Duration,
    error: &str,
    load_avg: Option<f64>,
    slots: i64,
    parallel: Option<i64>,
    attempt: Attempt,
) -> Value {
    json!({
        "code": ReasonCode::of_backend_error(error),
        "op": op,
        "workspace_id": workspace_id,
        "timeout_secs": timeout.as_secs(),
        "error": error.chars().take(BACKEND_ERROR_CHARS).collect::<String>(),
        "load_avg": load_avg,
        "slots": slots,
        "parallel": parallel,
        "attempt": attempt.number,
        "max_attempts": attempt.of,
        "retry_after_ms": attempt.retry_after.map(|backoff| backoff.as_millis() as u64),
    })
}

/// Whether the last [`TEXT_TAIL_LINES`] lines of `screen` show any trace
/// of `text`: the head of its first non-blank line as it is typed (tabs as
/// spaces, up to a backslash), or the `[Pasted text` Claude Code folds a
/// long paste into. Only the last lines are read, so that an earlier copy
/// of the same text in the scrollback does not count. A text with no head
/// to look for counts as shown, so that it is never typed twice on a guess.
pub fn text_on_screen(screen: &str, text: &str) -> bool {
    let head: String = text
        .split(['\n', '\r'])
        .map(|line| line.replace('\t', " "))
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim()
        .chars()
        .take_while(|c| *c != '\\')
        .take(TEXT_HEAD_CHARS)
        .collect();
    let head = head.trim_end();
    let lines: Vec<&str> = screen.lines().collect();
    let tail = lines[lines.len().saturating_sub(TEXT_TAIL_LINES)..].join("\n");
    head.is_empty() || tail.contains(head) || tail.contains("[Pasted text")
}

/// [`text_on_screen`] looks for this many leading characters of a text,
/// few enough to fit on the first line it wraps to.
const TEXT_HEAD_CHARS: usize = 24;

/// [`text_on_screen`] reads this many last lines of the screen: the input
/// box and what was submitted last.
const TEXT_TAIL_LINES: usize = 30;

/// A failed backend call, handed back by [`RecordingBackend`] so that
/// whoever records the error later can tell a cmux failure from others
/// ([`reason_of_error`]). It prints exactly as the error it wraps, with or
/// without `{:#}`, and its sources are that error's.
#[derive(Debug)]
pub struct BackendFailure {
    pub op: String,
    /// The failed call is known to have left nothing behind (a read, or a
    /// text the screen shows no trace of), so it could be made again.
    pub effect_free: bool,
    error: anyhow::Error,
}

impl BackendFailure {
    fn wrap(op: &str, effect_free: bool, error: anyhow::Error) -> anyhow::Error {
        anyhow::Error::new(Self {
            op: op.to_owned(),
            effect_free,
            error,
        })
    }

    fn timed_out(&self) -> bool {
        ReasonCode::of_backend_error(&format!("{:#}", self.error)) == ReasonCode::BackendTimeout
    }

    /// `backend_timeout` or `backend_failed`, with the call's `op`.
    pub fn reason(&self) -> Reason {
        Reason::new(ReasonCode::of_backend_error(&format!("{:#}", self.error)))
            .with("op", self.op.as_str())
    }
}

impl std::fmt::Display for BackendFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}

impl std::error::Error for BackendFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source()
    }
}

/// The reason code of an error a run step failed with: that of a failed
/// cmux call anywhere in its chain, else `fallback`.
pub fn reason_of_error(error: &anyhow::Error, fallback: ReasonCode) -> Reason {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<BackendFailure>())
        .map_or_else(|| Reason::new(fallback), BackendFailure::reason)
}

/// Whether `error` is a cmux call that timed out without it being known
/// whether it took effect: a send that may have reached the session, whose
/// screen then tells whether it did (task 285), rather than be sent again.
pub fn timed_out_maybe_sent(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<BackendFailure>())
        .is_some_and(|failure| failure.timed_out() && !failure.effect_free)
}

/// Whether `error` is a `/exit` that timed out on every attempt with the
/// screen showing each time that it did not get there (task 354): the
/// session was not asked to exit.
pub fn exit_unsent(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<BackendFailure>())
        .is_some_and(|failure| {
            failure.op == "send_exit" && failure.timed_out() && failure.effect_free
        })
}

fn timed_out(error: &anyhow::Error) -> bool {
    ReasonCode::of_backend_error(&format!("{error:#}")) == ReasonCode::BackendTimeout
}

/// A [`WorkspaceBackend`] that records every failed or timed-out call as
/// `backend_call_failed` before handing the error back unchanged, so the
/// queue keeps how often cmux fails and under what load (task 109). The
/// record is made here, in the application layer, and not in the cmux
/// adapter (ADR-0013). A call made for a run (`create`, or any call on a
/// workspace a run opened) is recorded on that run; one that belongs to no
/// run (`up`'s workspaces, the queue's group, `down`'s close) without one.
/// `token` is the supervisor whose slots are reported; `None` (`up`,
/// `down`) reports every lease and supervisor. The record is written
/// through its own connection, and a record that cannot be written is
/// dropped: it must never hide the backend's error.
pub struct RecordingBackend<'a> {
    inner: &'a dyn WorkspaceBackend,
    queues: Arc<dyn QueueOpener>,
    token: Option<LeaseToken>,
    /// The 1-minute load average, `None` where it cannot be read.
    load_average: fn() -> Option<f64>,
    /// Stops the turns background wrappers that died left running
    /// ([`Self::stopping_left_turns`]); `None` leaves them.
    left_turns: Option<Arc<dyn super::ProcessControl + Send + Sync>>,
}

impl<'a> RecordingBackend<'a> {
    /// `inner`, recording its failures through a connection `queues`
    /// opens for each.
    pub fn over(
        inner: &'a dyn WorkspaceBackend,
        queues: Arc<dyn QueueOpener>,
        token: Option<LeaseToken>,
        load_average: fn() -> Option<f64>,
    ) -> Self {
        Self {
            inner,
            queues,
            token,
            load_average,
            left_turns: None,
        }
    }

    /// `self`, telling and stopping through `processes` the turn a
    /// background session (ADR-t1404-1) left when its wrapper died: such a
    /// session `exists` while its recorded turn runs, and its `close`
    /// stops that turn after the wrapper (decision 3).
    pub fn stopping_left_turns(
        mut self,
        processes: Arc<dyn super::ProcessControl + Send + Sync>,
    ) -> Self {
        self.left_turns = Some(processes);
        self
    }

    /// The turn the background session `workspace_id` left running, with
    /// the process control that stops it; `None` for a workspace, without
    /// [`Self::stopping_left_turns`], or when none runs.
    fn left_turn(
        &self,
        workspace_id: &str,
    ) -> Option<(
        &dyn super::ProcessControl,
        crate::domain::background_wrapper::BackgroundHandle,
    )> {
        if !crate::domain::background_wrapper::is_background(workspace_id) {
            return None;
        }
        let processes = self.left_turns.as_deref()?;
        let queue = self.queues.open().ok()?;
        // A run's session, else a headless planner's (ADR-t1394-2).
        let turn = match queue.run_in_workspace(workspace_id).ok()? {
            Some(run) => super::supervise::left_turn(&*queue, processes, &run, workspace_id)?,
            None => super::supervise::left_planner_turn(&*queue, processes, workspace_id)?,
        };
        Some((processes as &dyn super::ProcessControl, turn))
    }

    /// A call that is made once.
    fn recorded<T>(
        &self,
        op: &str,
        workspace_id: Option<&str>,
        run_id: Option<&RunId>,
        result: Result<T>,
    ) -> Result<T> {
        result.map_err(|error| {
            let _ = self.record(
                op,
                workspace_id,
                run_id,
                &format!("{error:#}"),
                Attempt::ONLY,
            );
            BackendFailure::wrap(op, false, error)
        })
    }

    /// `call`, made again after a backoff (doubled each time) up to the
    /// backend's `call_attempts` in all while it fails with a timeout that
    /// `effect_free` says left nothing behind. `effect_free` is asked after
    /// the backoff, right before the call is made again, so that a call
    /// that got through late, during the backoff, is not made twice (task
    /// 354); after the last attempt it is asked at once. Every failed
    /// attempt is recorded with its number and the backoff that followed
    /// it.
    fn retried<T>(
        &self,
        op: &str,
        workspace_id: &str,
        mut call: impl FnMut() -> Result<T>,
        effect_free: impl Fn() -> bool,
    ) -> Result<T> {
        let attempts = self.inner.call_attempts().max(1);
        let mut backoff = self.inner.retry_backoff();
        let mut number = 1;
        loop {
            let error = match call() {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            let timed_out = timed_out(&error);
            let waited = timed_out && number < attempts;
            if waited {
                thread::sleep(backoff);
            }
            let effect_free = timed_out && effect_free();
            let retry = waited && effect_free;
            let attempt = Attempt {
                number,
                of: attempts,
                retry_after: retry.then_some(backoff),
            };
            let _ = self.record(op, Some(workspace_id), None, &format!("{error:#}"), attempt);
            if !retry {
                return Err(BackendFailure::wrap(op, effect_free, error));
            }
            backoff = backoff.saturating_mul(2);
            number += 1;
        }
    }

    fn record(
        &self,
        op: &str,
        workspace_id: Option<&str>,
        run_id: Option<&RunId>,
        error: &str,
        attempt: Attempt,
    ) -> Result<()> {
        let queue = self.queues.open()?;
        let run_id = match (run_id, workspace_id) {
            (Some(run_id), _) => Some(run_id.clone()),
            (None, Some(workspace_id)) => queue.run_in_workspace(workspace_id)?,
            (None, None) => None,
        };
        let (slots, parallel) = queue.backend_slots(self.token.as_ref())?;
        queue.record_backend_failure(
            run_id.as_ref(),
            backend_failure_payload(
                op,
                workspace_id,
                self.inner.call_timeout(),
                error,
                (self.load_average)(),
                slots,
                parallel,
                attempt,
            ),
        )
    }
}

impl WorkspaceBackend for RecordingBackend<'_> {
    fn preflight(&self) -> Result<()> {
        self.inner.preflight()
    }
    fn preflight_detached(&self, environment: &SupervisorEnvironment) -> Result<()> {
        self.inner.preflight_detached(environment)
    }
    /// A run's wrapper that cannot be started is recorded as a failed call
    /// on the run its environment names, as the workspace's create was
    /// before ADR-t1433-3; a planner's, whose environment names no run, is
    /// not recorded, as before.
    fn launch_background(
        &self,
        cwd: &std::path::Path,
        command: &str,
        env: &[(String, String)],
        log: &std::path::Path,
    ) -> Result<String> {
        let run = env
            .iter()
            .find(|(name, _)| name == crate::domain::actor::RUN_ID_ENV)
            .and_then(|(_, id)| RunId::new(id.as_str()).ok());
        let result = self.inner.launch_background(cwd, command, env, log);
        match run {
            Some(run) => self.recorded("launch_background", None, Some(&run), result),
            None => result,
        }
    }
    /// A text that timed out is typed again only while the screen shows no
    /// trace of it: one that got there is left to the submit check (task
    /// 285), and one whose screen cannot be read is not guessed at.
    fn send_text(&self, workspace_id: &str, text: &str) -> Result<()> {
        self.retried(
            "send_text",
            workspace_id,
            || self.inner.send_text(workspace_id, text),
            // One read: a screen that does not answer at once is not
            // waited for, and the text is not typed again.
            || {
                let screen = self.inner.capture(workspace_id);
                self.recorded("capture", Some(workspace_id), None, screen)
                    .is_ok_and(|screen| !text_on_screen(&screen, text))
            },
        )
    }
    fn send_enter(&self, workspace_id: &str) -> Result<()> {
        let result = self.inner.send_enter(workspace_id);
        self.recorded("send_enter", Some(workspace_id), None, result)
    }
    fn send_key(&self, workspace_id: &str, key: &str) -> Result<()> {
        let result = self.inner.send_key(workspace_id, key);
        self.recorded("send_key", Some(workspace_id), None, result)
    }
    fn capture(&self, workspace_id: &str) -> Result<String> {
        self.retried(
            "capture",
            workspace_id,
            || self.inner.capture(workspace_id),
            || true,
        )
    }
    fn close(&self, workspace_id: &str) -> Result<()> {
        let result = self.inner.close(workspace_id);
        self.recorded("close", Some(workspace_id), None, result)?;
        match self.left_turn(workspace_id) {
            Some((processes, turn)) => {
                let result = super::supervise::stop_left_turn(processes, &turn);
                self.recorded("close", Some(workspace_id), None, result)
            }
            None => Ok(()),
        }
    }
    fn set_color(&self, workspace_id: &str, color: &str) -> Result<()> {
        let result = self.inner.set_color(workspace_id, color);
        self.recorded("set_color", Some(workspace_id), None, result)
    }
    fn set_status(&self, workspace_id: &str, key: &str, value: &str, icon: &str) -> Result<()> {
        let result = self.inner.set_status(workspace_id, key, value, icon);
        self.recorded("set_status", Some(workspace_id), None, result)
    }
    fn pin(&self, workspace_id: &str) -> Result<()> {
        let result = self.inner.pin(workspace_id);
        self.recorded("pin", Some(workspace_id), None, result)
    }
    /// Never made again: a second `/exit` could pick a dialog's option.
    fn send_exit(&self, workspace_id: &str) -> Result<()> {
        let result = self.inner.send_exit(workspace_id);
        self.recorded("send_exit", Some(workspace_id), None, result)
    }
    /// A `/exit` that timed out is typed again only while the screen, read
    /// once, shows it did not get there (`unsent`: the input box drawn, no
    /// dialog, no trace of it), so it is never typed twice into a session
    /// or a dialog (task 354). One that timed out on every attempt fails
    /// as effect-free ([`exit_unsent`]).
    fn send_exit_when(&self, workspace_id: &str, unsent: &dyn Fn(&str) -> bool) -> Result<()> {
        self.retried(
            "send_exit",
            workspace_id,
            || self.inner.send_exit(workspace_id),
            || {
                let screen = self.inner.capture(workspace_id);
                self.recorded("capture", Some(workspace_id), None, screen)
                    .is_ok_and(|screen| unsent(&screen))
            },
        )
    }
    /// A background session whose wrapper is gone still exists while the
    /// turn it left runs, so that whatever closes it stops that turn.
    fn exists(&self, workspace_id: &str) -> Result<bool> {
        let exists = self.retried(
            "exists",
            workspace_id,
            || self.inner.exists(workspace_id),
            || true,
        )?;
        Ok(exists || self.left_turn(workspace_id).is_some())
    }
    fn listed_workspace_ids(&self) -> Result<Vec<String>> {
        let result = self.inner.listed_workspace_ids();
        self.recorded("listed_workspace_ids", None, None, result)
    }
    fn workspaces_described(&self, description: &str) -> Result<Vec<String>> {
        let result = self.inner.workspaces_described(description);
        self.recorded("workspaces_described", None, None, result)
    }
    fn create_named(
        &self,
        name: &str,
        cwd: &Path,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        let result = self.inner.create_named(name, cwd, command, tags);
        self.recorded("create_named", None, None, result)
    }
    fn ensure_group(&self, external_id: &str, name: &str) -> Result<String> {
        let result = self.inner.ensure_group(external_id, name);
        self.recorded("ensure_group", None, None, result)
    }
    fn notify(&self, title: &str, body: &str, workspace: Option<&str>) -> Result<()> {
        let result = self.inner.notify(title, body, workspace);
        self.recorded("notify", workspace, None, result)
    }
    fn call_timeout(&self) -> Duration {
        self.inner.call_timeout()
    }
    fn call_attempts(&self) -> u32 {
        self.inner.call_attempts()
    }
    fn retry_backoff(&self) -> Duration {
        self.inner.retry_backoff()
    }
    fn exit_timeout(&self) -> Duration {
        self.inner.exit_timeout()
    }
    fn registration_timeout(&self) -> Duration {
        self.inner.registration_timeout()
    }
    fn reopen_interval(&self) -> Duration {
        self.inner.reopen_interval()
    }
    fn resume_prompt_delay(&self) -> Duration {
        self.inner.resume_prompt_delay()
    }
    fn resume_timeout(&self) -> Duration {
        self.inner.resume_timeout()
    }
    fn submit_check_interval(&self) -> Duration {
        self.inner.submit_check_interval()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::Queue;
    use anyhow::{Context, anyhow, bail};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A cmux whose first `failures` calls fail with `error`, counting
    /// every call it gets. Only `exists` and `close` are made through it.
    struct Backend {
        error: &'static str,
        failures: AtomicUsize,
        calls: AtomicUsize,
    }

    impl Backend {
        fn failing(error: &'static str, failures: usize) -> Self {
            Self {
                error,
                failures: AtomicUsize::new(failures),
                calls: AtomicUsize::new(0),
            }
        }

        fn call(&self) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self
                .failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            {
                Ok(_) => bail!("{}", self.error),
                Err(_) => Ok(()),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl WorkspaceBackend for Backend {
        fn preflight(&self) -> Result<()> {
            unimplemented!()
        }
        fn preflight_detached(&self, _: &SupervisorEnvironment) -> Result<()> {
            unimplemented!()
        }
        fn send_text(&self, _: &str, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn send_enter(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn capture(&self, _: &str) -> Result<String> {
            unimplemented!()
        }
        fn close(&self, _: &str) -> Result<()> {
            self.call()
        }
        fn set_color(&self, _: &str, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn set_status(&self, _: &str, _: &str, _: &str, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn pin(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn send_exit(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn exists(&self, _: &str) -> Result<bool> {
            self.call().map(|()| true)
        }
        fn listed_workspace_ids(&self) -> Result<Vec<String>> {
            unimplemented!()
        }
        fn create_named(&self, _: &str, _: &Path, _: &str, _: &WorkspaceTags) -> Result<String> {
            unimplemented!()
        }
        fn ensure_group(&self, _: &str, _: &str) -> Result<String> {
            unimplemented!()
        }
        fn notify(&self, _: &str, _: &str, _: Option<&str>) -> Result<()> {
            unimplemented!()
        }
        /// No sleep between attempts in a unit test.
        fn retry_backoff(&self) -> Duration {
            Duration::ZERO
        }
    }

    /// No queue: the failures [`RecordingBackend`] records are dropped, so
    /// that only what it hands back and how often it calls are checked.
    struct NoQueue;

    impl QueueOpener for NoQueue {
        fn open(&self) -> Result<Box<dyn Queue + Send>> {
            bail!("no queue")
        }
    }

    fn recording(backend: &Backend) -> RecordingBackend<'_> {
        RecordingBackend::over(backend, Arc::new(NoQueue), None, || None)
    }

    /// The [`BackendFailure`] `error` wraps.
    fn failure(error: &anyhow::Error) -> &BackendFailure {
        error.downcast_ref::<BackendFailure>().unwrap()
    }

    /// `exists`, a call that leaves nothing behind, is made again while it
    /// times out, up to the backend's `call_attempts` in all, and fails as
    /// effect-free with `backend_timeout` and its op once they run out; a
    /// failure that is not a timeout is never made again. Moved from the
    /// interactive
    /// `failed_backend_calls_are_recorded_with_the_load_and_counted_by_stats`
    /// that task 1437 deleted.
    #[test]
    fn an_effect_free_call_that_timed_out_is_made_again_until_its_attempts_run_out() {
        const TIMEOUT: &str = "cmux list-workspaces failed: Command timed out";
        let backend = Backend::failing(TIMEOUT, 2);
        assert!(recording(&backend).exists("ws").unwrap());
        assert_eq!(backend.calls(), 3);

        let backend = Backend::failing(TIMEOUT, 3);
        let error = recording(&backend).exists("ws").unwrap_err();
        assert_eq!(backend.calls(), 3);
        assert_eq!(failure(&error).op, "exists");
        assert!(failure(&error).effect_free);
        let reason = reason_of_error(&error, ReasonCode::Other);
        assert_eq!(reason.code, ReasonCode::BackendTimeout);
        assert_eq!(reason.detail["op"], "exists");
        assert!(!timed_out_maybe_sent(&error));

        let backend = Backend::failing("injected workspace list failure", 1);
        let error = recording(&backend).exists("ws").unwrap_err();
        assert_eq!(backend.calls(), 1);
        assert!(!failure(&error).effect_free);
        assert_eq!(
            reason_of_error(&error, ReasonCode::Other).code,
            ReasonCode::BackendFailed
        );
    }

    /// `close` is made once, timed out or not, and its failure is handed
    /// back with its code and op, which `cleanup_failed` carries
    /// ([`reason_of_error`]); a close that timed out may have taken effect.
    /// Moved from the interactive
    /// `failed_backend_calls_are_recorded_with_the_load_and_counted_by_stats`
    /// that task 1437 deleted.
    #[test]
    fn a_failed_close_is_made_once_and_handed_back_with_its_code_and_op() {
        let backend = Backend::failing("injected workspace close failure", 1);
        let error = recording(&backend).close("ws").unwrap_err();
        assert_eq!(backend.calls(), 1);
        assert_eq!(format!("{error:#}"), "injected workspace close failure");
        let reason = reason_of_error(&error, ReasonCode::Other);
        assert_eq!(reason.code, ReasonCode::BackendFailed);
        assert_eq!(reason.detail["op"], "close");

        let backend = Backend::failing("cmux close-workspace failed: Command timed out", 1);
        let error = recording(&backend).close("ws").unwrap_err();
        assert_eq!(backend.calls(), 1);
        assert_eq!(
            reason_of_error(&error, ReasonCode::Other).code,
            ReasonCode::BackendTimeout
        );
        assert!(timed_out_maybe_sent(&error));
    }

    /// A run's wrapper that cannot be started is handed back as a failed
    /// `launch_background` of the run its environment names (recorded on
    /// it), as the workspace's `create` was before ADR-t1433-3; a
    /// planner's, whose environment names no run, is handed back as it
    /// failed and is not recorded.
    #[test]
    fn a_failed_background_launch_is_a_backend_failure_only_for_a_run() {
        let backend = Backend::failing("unused", 0);
        let recording = recording(&backend);
        let launch = |env: &[(&str, &str)]| {
            let env: Vec<(String, String)> = env
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect();
            recording
                .launch_background(
                    Path::new("/w"),
                    "wrapper",
                    &env,
                    Path::new("/r/session.log"),
                )
                .unwrap_err()
        };
        let error = launch(&[
            ("DAGQ_ROLE", "worker"),
            (crate::domain::actor::RUN_ID_ENV, "r1"),
        ]);
        assert_eq!(failure(&error).op, "launch_background");
        assert!(!failure(&error).effect_free);
        assert_eq!(
            reason_of_error(&error, ReasonCode::Other).code,
            ReasonCode::BackendFailed
        );
        let error = launch(&[("DAGQ_ROLE", "planner")]);
        assert!(error.downcast_ref::<BackendFailure>().is_none());
        assert!(
            format!("{error:#}").contains("starts no background wrapper"),
            "{error:#}"
        );
        assert_eq!(backend.calls(), 0);
    }

    #[test]
    fn a_backend_failure_prints_as_the_error_it_wraps_and_classifies_it() {
        let inner = || anyhow!("Command timed out").context("cmux capture-pane failed");
        let wrapped = BackendFailure::wrap("capture", true, inner());
        assert_eq!(format!("{wrapped:#}"), format!("{:#}", inner()));
        assert_eq!(format!("{wrapped}"), format!("{}", inner()));
        let outer = Err::<(), _>(wrapped)
            .context("run could not be watched")
            .unwrap_err();
        assert_eq!(
            format!("{outer:#}"),
            "run could not be watched: cmux capture-pane failed: Command timed out"
        );
        let reason = reason_of_error(&outer, ReasonCode::Other);
        assert_eq!(reason.code, ReasonCode::BackendTimeout);
        assert_eq!(reason.detail["op"], "capture");
        let failed = BackendFailure::wrap("close", false, anyhow!("workspace not found"));
        assert_eq!(
            reason_of_error(&failed, ReasonCode::Other).code,
            ReasonCode::BackendFailed
        );
        assert_eq!(
            reason_of_error(&anyhow!("git failed"), ReasonCode::Other),
            Reason::new(ReasonCode::Other)
        );
    }

    #[test]
    fn a_backend_failure_payload_carries_its_code() {
        let payload = backend_failure_payload(
            "send_exit",
            Some("ws"),
            Duration::from_secs(30),
            "\"cmux\" send did not finish within 30s",
            None,
            1,
            Some(4),
            Attempt::ONLY,
        );
        assert_eq!(payload["code"], "backend_timeout");
        assert_eq!(payload["op"], "send_exit");
        assert_eq!(
            (
                &payload["attempt"],
                &payload["max_attempts"],
                &payload["retry_after_ms"]
            ),
            (&json!(1), &json!(1), &Value::Null)
        );
        let retried = Attempt {
            number: 2,
            of: 3,
            retry_after: Some(Duration::from_secs(4)),
        };
        let payload = backend_failure_payload(
            "capture",
            Some("ws"),
            Duration::from_secs(30),
            "Command timed out",
            Some(91.5),
            4,
            Some(4),
            retried,
        );
        assert_eq!(payload["attempt"], 2);
        assert_eq!(payload["max_attempts"], 3);
        assert_eq!(payload["retry_after_ms"], 4000);
    }

    #[test]
    fn a_text_is_on_the_screen_by_the_head_of_its_first_line() {
        let text = "answer to ask 12: rebase onto main and run the tests again\nthen commit";
        assert!(text_on_screen("> answer to ask 12: rebase onto ma", text));
        assert!(!text_on_screen("> ready", text));
        // Up to a backslash, which is typed otherwise, and tabs as spaces.
        assert!(text_on_screen("> see C:", "see C:\\path"));
        assert!(text_on_screen("> a b", "\n\na\tb"));
        // A long paste folded in the input box got there.
        assert!(text_on_screen("> [Pasted text #1 +3 lines]", text));
        // A copy far up the scrollback is not this one.
        let old = format!("> {text}\n{}> ready", "work\n".repeat(40));
        assert!(!text_on_screen(&old, text));
        // Nothing to look for is never typed again on a guess.
        assert!(text_on_screen("", "  "));
    }
}
