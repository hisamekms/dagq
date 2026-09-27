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

impl Supervisor<'_> {
    /// Reap the sample job once it ended; start one when the interval
    /// passed since the last one started.
    pub(super) fn host_metrics_pass(&mut self) {
        let Some(port) = self.host_metrics_port.clone() else {
            return;
        };
        if let Some(job) = self.host_metrics.job.take() {
            if !job.is_finished() {
                self.host_metrics.job = Some(job);
                return;
            }
            self.reap_host_metrics(job);
        }
        // A handoff waits for the job: none starts once it is asked for,
        // so a slow sample cannot put the exec off.
        if self.handoff.is_some() {
            return;
        }
        if self
            .host_metrics
            .started
            .is_some_and(|started| started.elapsed() < port.interval)
        {
            return;
        }
        self.host_metrics.started = Some(Instant::now());
        let now = self.generators.clock.now();
        self.host_metrics.job = Some(spawn_traced(move || (port.record)(now)));
    }

    /// Wait for the sample job still running, as the loop ends.
    pub(super) fn finish_host_metrics(&mut self) {
        if let Some(job) = self.host_metrics.job.take() {
            self.reap_host_metrics(job);
        }
    }

    fn reap_host_metrics(&mut self, job: thread::JoinHandle<Result<Vec<PathBuf>>>) {
        let failure = match job.join() {
            Ok(Ok(removed)) => {
                for path in removed {
                    info!(
                        "host metrics past their retention removed: {}",
                        path.display()
                    );
                }
                if self.host_metrics.failing {
                    info!("the host's load is recorded again");
                }
                self.host_metrics.failing = false;
                return;
            }
            Ok(Err(error)) => format!("{error:#}"),
            Err(_) => "the sample job panicked".to_owned(),
        };
        if !self.host_metrics.failing {
            warn!(error = %failure, "the host's load could not be recorded: {failure}");
        }
        self.host_metrics.failing = true;
    }
}
