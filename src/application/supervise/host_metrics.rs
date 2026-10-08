//! The host's load, recorded continuously (task 516): every
//! [`HostMetricsPort::interval`], draining or not, a job thread samples
//! the host and appends a row to `<queue dir>/host/metrics-YYYYMMDD.csv`,
//! then removes the files past their retention
//! ([`crate::domain::host_metrics`]). Nothing is recorded as an event
//! (ADR-0040 decision 5). A failed or panicking sample is logged (once
//! until one succeeds again) and stops nothing; one job runs at a time, so
//! a slow tool delays the next sample instead of piling them up.

use super::*;

/// What the supervisor records the host's load with.
#[derive(Clone)]
pub struct HostMetricsPort {
    /// How often a sample is taken.
    pub interval: Duration,
    /// Takes one sample at the unix second given and records it; returns
    /// the files the retention removed.
    pub record: Arc<dyn Fn(i64) -> Result<Vec<PathBuf>> + Send + Sync>,
}

/// The sample job running now and when the last one started.
#[derive(Default)]
pub(super) struct HostMetricsWatch {
    job: Option<thread::JoinHandle<Result<Vec<PathBuf>>>>,
    started: Option<Instant>,
    failing: bool,
}

impl HostMetricsWatch {
    pub(super) const fn running(&self) -> bool {
        self.job.is_some()
    }
}

impl HostMetricsWatch {
    /// Reap the sample job once it ended; start one when the interval
    /// passed since the last one started, unless `handing_off`: a handoff
    /// waits for the job and none starts once it is asked for, so a slow
    /// sample cannot put the exec off. `now` reads the injected clock's unix
    /// second, `at` its monotonic time.
    pub(super) fn pass(
        &mut self,
        port: Option<&HostMetricsPort>,
        handing_off: bool,
        now: impl FnOnce() -> i64,
        at: Instant,
    ) {
        let Some(port) = port.cloned() else {
            return;
        };
        if let Some(job) = self.job.take() {
            if !job.is_finished() {
                self.job = Some(job);
                return;
            }
            self.reap(job);
        }
        if handing_off
            || !sample_due(
                self.started
                    .map(|started| at.saturating_duration_since(started)),
                port.interval,
            )
        {
            return;
        }
        self.started = Some(at);
        let now = now();
        self.job = Some(spawn_traced(move || (port.record)(now)));
    }

    /// Wait for the sample job still running, as the loop ends.
    pub(super) fn finish(&mut self) {
        if let Some(job) = self.job.take() {
            self.reap(job);
        }
    }

    fn reap(&mut self, job: thread::JoinHandle<Result<Vec<PathBuf>>>) {
        let failure = match job.join() {
            Ok(Ok(removed)) => {
                for path in removed {
                    info!(
                        "host metrics past their retention removed: {}",
                        path.display()
                    );
                }
                if self.failing {
                    info!("the host's load is recorded again");
                }
                self.failing = false;
                return;
            }
            Ok(Err(error)) => format!("{error:#}"),
            Err(_) => "the sample job panicked".to_owned(),
        };
        if !self.failing {
            warn!(error = %failure, "the host's load could not be recorded: {failure}");
        }
        self.failing = true;
    }
}

/// Whether a sample is due `since_started` the last one started (`None`:
/// none yet), taken every `interval`.
fn sample_due(since_started: Option<Duration>, interval: Duration) -> bool {
    since_started.is_none_or(|since| since >= interval)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first pass samples, and the next one once the interval passed
    /// since the last sample started: not 1 ms before.
    #[test]
    fn a_sample_is_due_first_and_then_once_the_interval_passed() {
        let interval = Duration::from_secs(60);
        assert!(sample_due(None, interval));
        assert!(!sample_due(Some(Duration::ZERO), interval));
        assert!(!sample_due(
            Some(interval - Duration::from_millis(1)),
            interval
        ));
        assert!(sample_due(Some(interval), interval));
    }
}
