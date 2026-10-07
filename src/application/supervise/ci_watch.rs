//! The watch of the landing branch's CI (ADR-t1920-1, the design's CI
//! watch). Each pass reads `[ci_watch]` of the main checkout's
//! `dagq.toml` again; with the table, a check is due at the first pass of
//! the process and every `interval_secs` after, draining and handing off
//! too, and runs on a job thread ([`crate::application::ci_watch::check`]).
//! While its answer says the means to read GitHub are missing, and until
//! this process has its first answer, no task is claimed, no resume
//! starts and no run starts its landing, as for a missing program of
//! `[run.env]` (ADR-0049 decision 9). A passing failure is retried at the
//! next interval, and recorded as `ci_check_failed` once the same one
//! repeats [`crate::domain::ci_watch::CI_WATCH_FAILURE_LIMIT`] times.

use super::*;
use crate::application::ci_watch::{CheckOutcome, CiSource};
use crate::domain::ci_watch::{
    CI_WATCH_ACCESS_KINDS, CI_WATCH_HOLD, CiWatchConfig, FailureStreak, JobsUnread,
    available_after_failure, check_due, hold_reason,
};

/// Reads `[ci_watch]` of the main checkout's `dagq.toml` (`None` without
/// the table).
pub type CiWatchFile = Arc<dyn Fn() -> Result<Option<CiWatchConfig>> + Send + Sync>;

/// Makes the source a check reads GitHub through, for `[ci_watch]` and
/// the watched branch.
pub type CiSourceMaker =
    Arc<dyn Fn(&CiWatchConfig, &str) -> Result<Arc<dyn CiSource>> + Send + Sync>;

/// What the supervisor watches the CI with (`SuperviseOptions::ci_watch`);
/// `None` watches nothing (`--once`, the tests that do not ask).
#[derive(Clone)]
pub struct CiWatchPort {
    pub file: CiWatchFile,
    pub source: CiSourceMaker,
    /// The time between checks instead of `interval_secs` (on the
    /// injected clock's monotonic time); tests set it.
    pub interval: Option<Duration>,
}

/// The watch between passes.
#[derive(Default)]
pub(super) struct CiWatchState {
    /// `[ci_watch]` as last read.
    config: Option<CiWatchConfig>,
    /// The error the last read failed with, warned of once.
    error: Option<String>,
    job: Option<thread::JoinHandle<Result<CheckOutcome>>>,
    /// When the last check started (the injected clock's monotonic time).
    started: Option<Instant>,
    /// Whether the means were there at the last answer; `None` before
    /// this process's first one (or without the table).
    available: Option<bool>,
    failures: FailureStreak,
    /// The success run whose jobs the last check could not read, which the
    /// next check counts on from (ADR-t2034-1 decision 5).
    jobs_unread: JobsUnread,
    /// The hold's reason this process last recorded or found recorded
    /// (`Some(None)`: none); `None` before its first look at the queue.
    recorded: Option<Option<&'static str>>,
}

impl CiWatchState {
    pub(super) const fn running(&self) -> bool {
        self.job.is_some()
    }
}

impl Supervisor<'_> {
    /// Whether the claims, the resumes and the landings wait for the
    /// means to read the CI: the table is set and this process has no
    /// answer yet or the last one found them missing.
    pub(super) fn ci_watch_held(&self) -> bool {
        hold_reason(self.ci.config.is_some(), self.ci.available).is_some()
    }

    /// Whether the last answer found the means to read the CI missing (not
    /// merely none yet): what a drain hands a run back to a person for.
    pub(super) fn ci_watch_unreadable(&self) -> bool {
        self.ci.config.is_some() && self.ci.available == Some(false)
    }

    /// Read `[ci_watch]` again, reap the check that ended and start the one
    /// that is due, then record where the hold for the watch changed.
    pub(super) fn ci_watch_pass(&mut self) {
        let Some(port) = self.ci_watch_port.clone() else {
            return;
        };
        self.ci_watch_step(&port);
        self.record_ci_watch_hold();
    }

    /// Record `ci_watch_held` / `ci_watch_resumed` when the hold's reason
    /// ([`hold_reason`]) differs from the one this process last recorded,
    /// against its own latest on the queue; a pass that changes nothing
    /// reads nothing.
    fn record_ci_watch_hold(&mut self) {
        let reason = hold_reason(self.ci.config.is_some(), self.ci.available);
        if self.ci.recorded == Some(reason) {
            return;
        }
        let workflow = self
            .ci
            .config
            .as_ref()
            .map(|config| config.workflow.clone());
        let hold = reason.map(|reason| (reason, json!({"workflow": workflow})));
        if self.record_own_hold(CI_WATCH_HOLD, hold) {
            self.ci.recorded = Some(reason);
        }
    }

    fn ci_watch_step(&mut self, port: &CiWatchPort) {
        match (port.file)() {
            Ok(config) => {
                if config != self.ci.config {
                    match &config {
                        Some(config) => info!(
                            "[ci_watch] of dagq.toml watches {} every {}s",
                            config.workflow, config.interval_secs
                        ),
                        None => info!("[ci_watch] of dagq.toml is gone: the CI is not watched"),
                    }
                    // A new table is checked at once.
                    self.ci.started = None;
                    if config.is_none() {
                        self.ci.available = None;
                    }
                }
                self.ci.config = config;
                self.ci.error = None;
            }
            Err(error) => {
                let message = format!("{error:#}");
                if self.ci.error.as_ref() != Some(&message) {
                    warn!(error = %message, "[ci_watch] of dagq.toml not read: {message}; keeping the table in use");
                    self.ci.error = Some(message);
                }
            }
        }
        self.reap_ci_check();
        let Some(config) = self.ci.config.clone() else {
            return;
        };
        let interval = port.interval.unwrap_or_else(|| config.interval());
        let now = self.generators.clock.monotonic();
        let since_start = self
            .ci
            .started
            .map(|started| now.saturating_duration_since(started));
        if self.ci.job.is_some() || !check_due(since_start, interval) {
            return;
        }
        let branch = match &config.branch {
            Some(branch) => branch.clone(),
            None => match self.repository.landing_branch() {
                Ok(branch) => branch.name,
                // The landing branch's own hold covers it.
                Err(_) => return,
            },
        };
        let source = match (port.source)(&config, &branch) {
            Ok(source) => source,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the CI watch could not start: {error:#}");
                self.ci.started = Some(now);
                return;
            }
        };
        self.ci.started = Some(now);
        let queues = self.queues.clone();
        let token = self.token.clone();
        let build = self.layout.version.clone();
        let jobs_unread = self.ci.jobs_unread.clone();
        self.ci.job = Some(spawn_traced(move || {
            let queue = queues.open()?;
            crate::application::ci_watch::check(
                &*queue,
                &*source,
                &config,
                &branch,
                &token,
                &build,
                &jobs_unread,
            )
        }));
    }

    /// Take the answer of the check that ended.
    fn reap_ci_check(&mut self) {
        let Some(job) = self.ci.job.take() else {
            return;
        };
        if !job.is_finished() {
            self.ci.job = Some(job);
            return;
        }
        let result = job
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("the CI check panicked")));
        match result {
            Ok(outcome) => {
                self.ci.failures.succeed();
                match (self.ci.available, outcome.available) {
                    (Some(true) | None, false) => warn!(
                        "the CI cannot be read: no task is claimed and no run lands until it can (ADR-t1920-1)"
                    ),
                    (Some(false), true) => {
                        info!("the CI can be read again; claiming and landing resume")
                    }
                    _ => {}
                }
                self.ci.available = Some(outcome.available);
                self.ci.jobs_unread = outcome.jobs_unread;
                if outcome.recorded > 0 {
                    info!("the CI watch recorded {} run(s)", outcome.recorded);
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                warn!(error = %message, "the CI check failed; it is tried again at the next interval: {message}");
                // A passing failure holds nothing: the queue's last answer
                // stands (the check may have recorded the means' return
                // before it failed).
                if let Ok(last) = self.queue.latest_queue_event(&CI_WATCH_ACCESS_KINDS) {
                    self.ci.available = Some(available_after_failure(
                        last.as_ref().map(|event| event.kind.as_str()),
                    ));
                }
                if let Some(mut payload) =
                    self.ci.failures.fail(&message, self.generators.clock.now())
                {
                    payload["supervisor"] = json!(self.token);
                    if let Err(error) = self
                        .queue
                        .record_queue_event(EventKind::CiCheckFailed, payload)
                    {
                        warn!(error = %format_args!("{error:#}"), "ci_check_failed could not be recorded: {error:#}");
                    }
                }
            }
        }
    }
}
