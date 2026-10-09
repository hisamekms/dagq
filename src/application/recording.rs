//! Every failed call of the host's sessions on record (task 109):
//! [`RecordingBackend`] wraps a [`WorkspaceBackend`] (cmux) and
//! [`RecordingSessions`] a [`SessionWrappers`] (the background wrappers),
//! and both write `backend_call_failed` with the load the call failed
//! under. A call that timed out is made again after a
//! backoff when making it again is safe (task 326).

use crate::domain::LeaseToken;
use anyhow::Result;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, thread, time::Duration};

use super::{QueueOpener, RecordingQueue, SessionWrappers, WorkspaceBackend, WorkspaceTags};
use crate::domain::background_wrapper::{StopRoute, WrapperStop};
use crate::domain::{EventKind, Reason, ReasonCode, RunId};

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

/// A failed backend call, handed back by [`RecordingBackend`] so that
/// whoever records the error later can tell a cmux failure from others
/// ([`reason_of_error`]). It prints exactly as the error it wraps, with or
/// without `{:#}`, and its sources are that error's.
#[derive(Debug)]
pub struct BackendFailure {
    pub op: String,
    /// The failed call is known to have left nothing behind (a read), so
    /// it could be made again.
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

fn timed_out(error: &anyhow::Error) -> bool {
    ReasonCode::of_backend_error(&format!("{error:#}")) == ReasonCode::BackendTimeout
}

/// The payload of `wrapper_stopped` (task 1657): the stopped wrapper's
/// `handle` (`workspace_id`) and `pid`, how it ended (`signal`), the
/// SIGKILLs sent to what it started (`children_killed`), whether a turn
/// it left after it died was killed (`left_turn_killed`), and the path of
/// the runtime that stopped it (`route`).
pub fn wrapper_stopped_payload(
    handle: &str,
    stop: WrapperStop,
    route: StopRoute,
    left_turn_killed: bool,
) -> Value {
    json!({
        "workspace_id": handle,
        "pid": crate::domain::background_wrapper::BackgroundHandle::parse(handle).map(|h| h.pid),
        "signal": stop.signal.as_str(),
        "children_killed": stop.children_killed,
        "left_turn_killed": left_turn_killed,
        "route": route.as_str(),
    })
}

/// The limits of one call of a port: how long it may run, how many times
/// in all a call that timed out is made when that is safe, and the backoff
/// before the first retry.
#[derive(Debug, Clone, Copy)]
struct Limits {
    timeout: Duration,
    attempts: u32,
    backoff: Duration,
}

/// Where [`RecordingBackend`] and [`RecordingSessions`] record a failed
/// call: through its own connection `queues` opens for each, as the ports
/// recording reaches, with the slots of `token` (`None` reports every lease
/// and supervisor) and the 1-minute load average (`None` where it cannot be
/// read).
struct Recorder {
    queues: Arc<dyn QueueOpener<dyn RecordingQueue + Send>>,
    token: Option<LeaseToken>,
    load_average: fn() -> Option<f64>,
}

/// The connections of the whole queue an opener gives, as the ports
/// recording reaches: what [`RecordingBackend`], which is given the whole
/// queue, records through.
struct AsRecordingQueue(Arc<dyn QueueOpener>);

impl QueueOpener<dyn RecordingQueue + Send> for AsRecordingQueue {
    fn open(&self) -> Result<Box<dyn RecordingQueue + Send>> {
        Ok(self.0.open()?)
    }
}

impl Recorder {
    /// A call that is made once.
    fn recorded<T>(
        &self,
        limits: Limits,
        op: &str,
        workspace_id: Option<&str>,
        run_id: Option<&RunId>,
        result: Result<T>,
    ) -> Result<T> {
        result.map_err(|error| {
            let _ = self.record(
                limits,
                op,
                workspace_id,
                run_id,
                &format!("{error:#}"),
                Attempt::ONLY,
            );
            BackendFailure::wrap(op, false, error)
        })
    }

    /// `call`, a read that leaves nothing behind, made again after a
    /// backoff (doubled each time) up to the port's attempts in all while
    /// it fails with a timeout. Every failed attempt is recorded with its
    /// number and the backoff that followed it.
    fn retried<T>(
        &self,
        limits: Limits,
        op: &str,
        workspace_id: &str,
        mut call: impl FnMut() -> Result<T>,
    ) -> Result<T> {
        let attempts = limits.attempts.max(1);
        let mut backoff = limits.backoff;
        let mut number = 1;
        loop {
            let error = match call() {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            let timed_out = timed_out(&error);
            let retry = timed_out && number < attempts;
            if retry {
                thread::sleep(backoff);
            }
            let attempt = Attempt {
                number,
                of: attempts,
                retry_after: retry.then_some(backoff),
            };
            let _ = self.record(
                limits,
                op,
                Some(workspace_id),
                None,
                &format!("{error:#}"),
                attempt,
            );
            if !retry {
                return Err(BackendFailure::wrap(op, timed_out, error));
            }
            backoff = backoff.saturating_mul(2);
            number += 1;
        }
    }

    /// Record `wrapper_stopped` with `payload` on the run whose session
    /// `handle` is, else as the queue's own event.
    fn record_stop(&self, payload: Value, handle: &str) -> Result<()> {
        let queue = self.queues.open()?;
        match queue.run_in_workspace(handle)? {
            Some(run) => queue.record_runtime_event(&run, EventKind::WrapperStopped, payload),
            None => queue
                .record_queue_event(EventKind::WrapperStopped, payload)
                .map(drop),
        }
    }

    fn record(
        &self,
        limits: Limits,
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
                limits.timeout,
                error,
                (self.load_average)(),
                slots,
                parallel,
                attempt,
            ),
        )
    }
}

/// A [`WorkspaceBackend`] that records every failed or timed-out call as
/// `backend_call_failed` before handing the error back unchanged, so the
/// queue keeps how often cmux fails and under what load (task 109). The
/// record is made here, in the application layer, and not in the cmux
/// adapter (ADR-0013). A call on a workspace a run recorded is recorded on
/// that run; one that belongs to no run (`up`'s workspaces, the queue's
/// group, `down`'s close) without one. `token` is the supervisor whose
/// slots are reported; `None` (`up`, `down`) reports every lease and
/// supervisor. A record that cannot be written is dropped: it must never
/// hide the backend's error.
pub struct RecordingBackend<'a> {
    inner: &'a dyn WorkspaceBackend,
    recorder: Recorder,
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
            recorder: Recorder {
                queues: Arc::new(AsRecordingQueue(queues)),
                token,
                load_average,
            },
        }
    }

    fn limits(&self) -> Limits {
        Limits {
            timeout: self.inner.call_timeout(),
            attempts: self.inner.call_attempts(),
            backoff: self.inner.retry_backoff(),
        }
    }

    fn recorded<T>(&self, op: &str, workspace_id: Option<&str>, result: Result<T>) -> Result<T> {
        self.recorder
            .recorded(self.limits(), op, workspace_id, None, result)
    }

    fn retried<T>(
        &self,
        op: &str,
        workspace_id: &str,
        call: impl FnMut() -> Result<T>,
    ) -> Result<T> {
        self.recorder.retried(self.limits(), op, workspace_id, call)
    }
}

impl WorkspaceBackend for RecordingBackend<'_> {
    fn preflight(&self) -> Result<()> {
        self.inner.preflight()
    }
    fn close(&self, workspace_id: &str) -> Result<()> {
        let result = self.inner.close(workspace_id);
        self.recorded("close", Some(workspace_id), result)
    }
    fn set_color(&self, workspace_id: &str, color: &str) -> Result<()> {
        let result = self.inner.set_color(workspace_id, color);
        self.recorded("set_color", Some(workspace_id), result)
    }
    fn set_status(&self, workspace_id: &str, key: &str, value: &str, icon: &str) -> Result<()> {
        let result = self.inner.set_status(workspace_id, key, value, icon);
        self.recorded("set_status", Some(workspace_id), result)
    }
    fn pin(&self, workspace_id: &str) -> Result<()> {
        let result = self.inner.pin(workspace_id);
        self.recorded("pin", Some(workspace_id), result)
    }
    fn exists(&self, workspace_id: &str) -> Result<bool> {
        self.retried("exists", workspace_id, || self.inner.exists(workspace_id))
    }
    fn create_named(
        &self,
        name: &str,
        cwd: &Path,
        command: &str,
        tags: &WorkspaceTags,
    ) -> Result<String> {
        let result = self.inner.create_named(name, cwd, command, tags);
        self.recorded("create_named", None, result)
    }
    fn ensure_group(&self, external_id: &str, name: &str) -> Result<String> {
        let result = self.inner.ensure_group(external_id, name);
        self.recorded("ensure_group", None, result)
    }
    fn notify(&self, title: &str, body: &str, workspace: Option<&str>) -> Result<()> {
        let result = self.inner.notify(title, body, workspace);
        self.recorded("notify", workspace, result)
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
}

/// The [`SessionWrappers`] of the supervisor and the runtime's planners,
/// recording every failed or timed-out call as `backend_call_failed` (on
/// the run whose session it is, or the run a start's environment names) and
/// every stop that tells how it ended as `wrapper_stopped`, before handing
/// back what the port returned, as [`RecordingBackend`] does for cmux.
/// `token` is the supervisor whose slots are reported.
pub struct RecordingSessions<'a> {
    inner: &'a dyn SessionWrappers,
    recorder: Recorder,
    /// Stops the turns background wrappers that died left running
    /// ([`Self::stopping_left_turns`]); `None` leaves them.
    left_turns: Option<Arc<dyn super::ProcessControl + Send + Sync>>,
}

impl<'a> RecordingSessions<'a> {
    /// `inner`, recording its failures through a connection `queues`
    /// opens for each, which reaches only the ports recording takes.
    pub fn over(
        inner: &'a dyn SessionWrappers,
        queues: Arc<dyn QueueOpener<dyn RecordingQueue + Send>>,
        token: Option<LeaseToken>,
        load_average: fn() -> Option<f64>,
    ) -> Self {
        Self {
            inner,
            recorder: Recorder {
                queues,
                token,
                load_average,
            },
            left_turns: None,
        }
    }

    /// `self`, telling and stopping through `processes` the turn a
    /// background session (ADR-t1404-1) left when its wrapper died: such a
    /// session `exists` while its recorded turn runs, and its stop stops
    /// that turn after the wrapper (decision 3).
    pub fn stopping_left_turns(
        mut self,
        processes: Arc<dyn super::ProcessControl + Send + Sync>,
    ) -> Self {
        self.left_turns = Some(processes);
        self
    }

    fn limits(&self) -> Limits {
        Limits {
            timeout: self.inner.call_timeout(),
            attempts: self.inner.call_attempts(),
            backoff: self.inner.retry_backoff(),
        }
    }

    /// The turn the background session `handle` left running, with the
    /// process control that stops it; `None` for an ID that is not a
    /// wrapper's handle, without [`Self::stopping_left_turns`], or when
    /// none runs.
    fn left_turn(
        &self,
        handle: &str,
    ) -> Option<(
        &dyn super::ProcessControl,
        crate::domain::background_wrapper::BackgroundHandle,
    )> {
        if !crate::domain::background_wrapper::is_background(handle) {
            return None;
        }
        let processes = self.left_turns.as_deref()?;
        let queue = self.recorder.queues.open().ok()?;
        // A run's session, else a headless planner's (ADR-t1394-2).
        let turn = match queue.run_in_workspace(handle).ok()? {
            Some(run) => super::supervise::left_turn(&*queue, processes, &run, handle)?,
            None => super::supervise::left_planner_turn(&*queue, processes, handle)?,
        };
        Some((processes as &dyn super::ProcessControl, turn))
    }
}

impl SessionWrappers for RecordingSessions<'_> {
    /// A run's wrapper that cannot be started is recorded as a failed call
    /// on the run its environment names; a planner's, whose environment
    /// names no run, is not recorded.
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
            Some(run) => {
                self.recorder
                    .recorded(self.limits(), "launch_background", None, Some(&run), result)
            }
            None => result,
        }
    }
    /// The wrapper is stopped, then the turn a wrapper that died left
    /// running, and the stop is recorded as `wrapper_stopped` with `route`
    /// on the run whose session it is, or as the queue's own event for a
    /// planner's ([`wrapper_stopped_payload`], task 1657). A failed stop
    /// of the wrapper is a failed `close` and records no `wrapper_stopped`;
    /// a left turn that cannot be stopped fails the `close` after the
    /// wrapper's stop is recorded. A record that cannot be written is
    /// dropped.
    fn stop_background(&self, handle: &str, route: StopRoute) -> Result<Option<WrapperStop>> {
        let limits = self.limits();
        let result = self.inner.stop_background(handle, route);
        let stop = self
            .recorder
            .recorded(limits, "close", Some(handle), None, result)?;
        let left_turn = self.left_turn(handle);
        let left_turn_killed = left_turn.is_some();
        let left = match left_turn {
            Some((processes, turn)) => super::supervise::stop_left_turn(processes, &turn),
            None => Ok(()),
        };
        // The wrapper's stop is recorded even when the turn it left could
        // not be stopped, whose failure is then the close's.
        if let Some(stop) = stop {
            let _ = self.recorder.record_stop(
                wrapper_stopped_payload(handle, stop, route, left_turn_killed),
                handle,
            );
        }
        self.recorder
            .recorded(limits, "close", Some(handle), None, left)?;
        Ok(stop)
    }
    /// A background session whose wrapper is gone still exists while the
    /// turn it left runs, so that whatever stops it stops that turn.
    fn exists(&self, handle: &str) -> Result<bool> {
        let exists = self.recorder.retried(self.limits(), "exists", handle, || {
            self.inner.exists(handle)
        })?;
        Ok(exists || self.left_turn(handle).is_some())
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
    fn resume_timeout(&self) -> Duration {
        self.inner.resume_timeout()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, anyhow, bail};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A cmux and a port of background wrappers whose first `failures`
    /// calls fail with `error`, counting every call it gets. Only `exists`,
    /// `close` and the start of a wrapper (which it refuses) are made
    /// through it.
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
        fn exists(&self, _: &str) -> Result<bool> {
            self.call().map(|()| true)
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

    impl SessionWrappers for Backend {
        fn launch_background(
            &self,
            _: &Path,
            _: &str,
            _: &[(String, String)],
            _: &Path,
        ) -> Result<String> {
            bail!("this test starts no background wrapper")
        }
        fn stop_background(&self, _: &str, _: StopRoute) -> Result<Option<WrapperStop>> {
            unimplemented!()
        }
        fn exists(&self, _: &str) -> Result<bool> {
            self.call().map(|()| true)
        }
        /// No sleep between attempts in a unit test.
        fn retry_backoff(&self) -> Duration {
            Duration::ZERO
        }
    }

    /// No queue: the failures [`RecordingBackend`] records are dropped, so
    /// that only what it hands back and how often it calls are checked.
    struct NoQueue;

    impl<P: ?Sized> QueueOpener<P> for NoQueue {
        fn open(&self) -> Result<Box<P>> {
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
    }

    /// `wrapper_stopped` names the handle, its pid, how the wrapper ended,
    /// the SIGKILLs sent to what it started, a left turn killed and the
    /// route (task 1657).
    #[test]
    fn a_wrapper_stop_is_recorded_with_its_signal_kills_and_route() {
        use crate::domain::background_wrapper::{StopSignal, WrapperStop};
        let stop = WrapperStop {
            signal: StopSignal::Kill,
            children_killed: 2,
        };
        assert_eq!(
            wrapper_stopped_payload("background:7:start", stop, StopRoute::AfterReview, true),
            json!({
                "workspace_id": "background:7:start",
                "pid": 7,
                "signal": "sigkill",
                "children_killed": 2,
                "left_turn_killed": true,
                "route": "after_review",
            })
        );
        let gone = WrapperStop {
            signal: StopSignal::Gone,
            children_killed: 0,
        };
        let payload = wrapper_stopped_payload("WS-1", gone, StopRoute::Close, false);
        assert_eq!(payload["pid"], Value::Null);
        assert_eq!(payload["signal"], "gone");
        assert_eq!(payload["route"], "close");
    }

    /// A run's wrapper that cannot be started is handed back as a failed
    /// `launch_background` of the run its environment names (recorded on
    /// it), as the workspace's `create` was before ADR-t1433-3; a
    /// planner's, whose environment names no run, is handed back as it
    /// failed and is not recorded.
    #[test]
    fn a_failed_background_launch_is_a_backend_failure_only_for_a_run() {
        let backend = Backend::failing("unused", 0);
        let recording = RecordingSessions::over(&backend, Arc::new(NoQueue), None, || None);
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

    /// The recording of the session wrappers records each failed attempt
    /// of a session's call on the run whose session it is, and a failed
    /// start on the run its environment names, with the slots of its
    /// supervisor, through only the ports it takes (no SQLite).
    #[test]
    fn a_failed_session_call_is_recorded_on_its_run_with_its_supervisors_slots() {
        use crate::application::port_fakes::SessionRecords;
        const TIMEOUT: &str = "background wrapper check failed: Command timed out";
        let records = SessionRecords {
            runs: [("background:7:start".to_owned(), RunId::new("r1").unwrap())].into(),
            slots: (2, Some(4)),
            ..SessionRecords::default()
        };
        let token = LeaseToken::new("supervisor");
        let backend = Backend::failing(TIMEOUT, 1);
        let recording = RecordingSessions::over(
            &backend,
            Arc::new(records.clone()),
            Some(token.clone()),
            || Some(1.5),
        );
        assert!(SessionWrappers::exists(&recording, "background:7:start").unwrap());
        let env = [(crate::domain::actor::RUN_ID_ENV.to_owned(), "r2".to_owned())];
        recording
            .launch_background(Path::new("/w"), "wrapper", &env, Path::new("/r/log"))
            .unwrap_err();

        let failures = records.failures.lock().unwrap();
        let runs: Vec<_> = failures.iter().map(|(run, _)| run.clone()).collect();
        assert_eq!(
            runs,
            [
                Some(RunId::new("r1").unwrap()),
                Some(RunId::new("r2").unwrap())
            ]
        );
        let (_, exists) = &failures[0];
        assert_eq!(exists["op"], "exists");
        assert_eq!(exists["code"], "backend_timeout");
        assert_eq!(exists["workspace_id"], "background:7:start");
        assert_eq!(
            (&exists["attempt"], &exists["max_attempts"]),
            (&json!(1), &json!(3))
        );
        assert_eq!(
            (&exists["slots"], &exists["parallel"]),
            (&json!(2), &json!(4))
        );
        assert_eq!(exists["load_avg"], 1.5);
        assert_eq!(failures[1].1["op"], "launch_background");
        assert_eq!(
            *records.slots_of.lock().unwrap(),
            [Some(token.clone()), Some(token)]
        );
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
}
