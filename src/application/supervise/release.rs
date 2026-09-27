//! The supervisor's look for a new dagq release (ADR-t618-1 decisions 1 to
//! 3, [`crate::application::release_update`]): on its first pass and then
//! every [`RELEASE_LOOK`], a supervisor of a release build reads the
//! host's `[update]` and, unless it is off, starts a job thread that,
//! when a look is due for the queue, reads the index and records the
//! result. The loop does not
//! wait for it but, like the report job, before it ends or execs. Nothing
//! is claimed or held on it; a failure is only logged and recorded.

use super::*;
use crate::application::release_update::{self, ReleaseIndex};
use crate::domain::release_update::{ReleaseMode, ReleaseUpdateConfig, is_release_build};

/// How often the supervisor looks whether a look at the index is due.
pub const RELEASE_LOOK: Duration = Duration::from_secs(60);

/// What the supervisor looks for a release with.
#[derive(Clone)]
pub struct ReleasePort {
    /// The host's `[update]`, read again at each look.
    pub config: Arc<dyn Fn() -> ReleaseUpdateConfig + Send + Sync>,
    /// Reads crates.io's sparse index.
    pub index: Arc<dyn ReleaseIndex>,
    /// The supervisor's build identifier as the look takes it.
    pub current: String,
}

type ReleaseJob = thread::JoinHandle<Result<Option<(&'static str, Value)>>>;

/// The look running now and when the last one started.
#[derive(Default)]
pub(super) struct ReleaseWatch {
    job: Option<ReleaseJob>,
    last: Option<Instant>,
}

impl ReleaseWatch {
    pub(super) const fn running(&self) -> bool {
        self.job.is_some()
    }
}

impl Supervisor<'_> {
    /// Reap the look once it ended; start one on the first pass and every
    /// [`RELEASE_LOOK`] after, when `start`.
    pub(super) fn release_pass(&mut self, start: bool) {
        let Some(port) = self.release_port.clone() else {
            return;
        };
        if let Some(job) = self.release.job.take() {
            if !job.is_finished() {
                self.release.job = Some(job);
                return;
            }
            match job.join() {
                Ok(Ok(Some((kind, payload)))) => {
                    info!("release check: {kind} {payload}");
                }
                Ok(Ok(None)) => {}
                Ok(Err(error)) => {
                    warn!(error = %format_args!("{error:#}"), "release check: {error:#}");
                }
                Err(_) => warn!("the release check panicked"),
            }
        }
        // Only a release build looks (ADR-t618-1 decision 1).
        if !start || !is_release_build(&port.current) {
            return;
        }
        let first_pass = self.release.last.is_none();
        if self
            .release
            .last
            .is_some_and(|last| last.elapsed() < RELEASE_LOOK)
        {
            return;
        }
        self.release.last = Some(Instant::now());
        let config = (port.config)();
        if config.release == ReleaseMode::Off {
            return;
        }
        let now = self.generators.clock.now();
        let queues = self.queues.clone();
        let token = self.token.clone();
        self.release.job = Some(spawn_traced(move || {
            let queue = queues.open()?;
            release_update::check(
                &*queue,
                &*port.index,
                &config,
                &port.current,
                now,
                first_pass,
                &token,
            )
        }));
    }
}
