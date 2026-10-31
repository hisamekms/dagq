//! The daily KPI report (ADR-0051 decision 20): on the first pass after the
//! host's local midnight, a job thread writes the reports the queue does
//! not record as written yet (the 7 days before today and the ISO week
//! before this one, [`report::write_due`]) under `<queue dir>/reports/`
//! and records each as `report_written`, so the next process and the other
//! supervisors do not write it again. It takes no run slot and uses no
//! LLM; the loop does not wait for it but like the observer, before it
//! ends. A failure is logged and tried again after [`RETRY`]; it stops no
//! claim nor landing. Only a supervisor still claiming starts the job; one
//! draining, stopping or waiting for a handoff only reaps the job running.
//! A job an exec cuts short leaves the reports it did not record to the
//! next process. After the reports the same job records the targets'
//! breaches that started and ended and makes the messages of the host's
//! push command (ADR-0051 decisions 18 and 23), which [`super::push`]
//! sends.
//!
//! The job is stopped now ([`STOPPED`]): it reads every event of the queue.

use super::*;
use crate::application::push::{self, PushOutcome, PushRequest, PushTarget};
use crate::application::report::{self, ReportSetup, Written};
use crate::domain::kpi::push::{PushConfig, PushMessage};

/// How long after a failed report job the reports are looked for again.
const RETRY: Duration = Duration::from_secs(600);

/// The report job is stopped (request 190, decision (1) of ADR-t3745-1):
/// the reports ([`report::write_due`]), the judgment of the targets'
/// breaches ([`push::check_breaches`]) and the push's summary read every
/// event of the queue. No job starts, so nothing of the queue is read and
/// no failure, attention, ask nor retry follows; the log says so once a
/// process. The breaches open stay open: a target not judged is not
/// resolved. Task 1669 brings the job back from the ledger and removes
/// this with the branch it stops.
const STOPPED: bool = true;

/// What the supervisor writes the reports with.
#[derive(Clone)]
pub struct ReportPort {
    /// The host's time zone at a unix second, seconds east of UTC.
    pub utc_offset: fn(i64) -> i64,
    /// Where and how the reports are made at a unix second: the `[kpi]`
    /// settings and the retention are read again each time.
    pub setup: Arc<dyn Fn(i64) -> Result<ReportSetup> + Send + Sync>,
    /// The host's `[push]`, read again each time; `None` pushes nothing.
    pub push_config: Arc<dyn Fn() -> Result<Option<PushConfig>> + Send + Sync>,
    /// Runs the push command once.
    pub run_push: fn(&PushRequest) -> PushOutcome,
    /// The delays before the second and the third attempt of a message.
    pub push_retry: [Duration; 2],
    /// The queue's name and database the messages are about.
    pub push_target: PushTarget,
}

/// What one report job did: the reports written, and the messages for the
/// push command with the `[push]` they go by.
pub(in crate::application::supervise) struct ReportJob {
    written: Vec<Written>,
    push: Option<(PushConfig, Vec<PushMessage>)>,
}

/// The report job running now, and the local day the reports were last
/// found written.
#[derive(Default)]
pub(in crate::application::supervise) struct ReportWatch {
    job: Option<(i64, thread::JoinHandle<Result<ReportJob>>)>,
    checked: Option<i64>,
    failed: Option<Instant>,
    /// The log said this process starts no job ([`STOPPED`]).
    told_stopped: bool,
}

impl ReportWatch {
    pub(in crate::application::supervise) const fn running(&self) -> bool {
        self.job.is_some()
    }

    /// Start `job` for the local `day`, unless [`STOPPED`]: then `job` is
    /// not run, and the first time the log says why.
    fn start(&mut self, day: i64, job: impl FnOnce() -> Result<ReportJob> + Send + 'static) {
        if STOPPED {
            if !self.told_stopped {
                info!(
                    "the KPI reports, the targets' breaches and the push's summary are stopped: they read every event of the queue, and come back once they read the ledger"
                );
                self.told_stopped = true;
            }
            return;
        }
        self.job = Some((day, spawn_traced(job)));
    }
}

impl ObservationState {
    /// Reap the report job once it ended; start one when the local day
    /// changed since the reports were last found written.
    pub(in crate::application::supervise) fn report_pass(
        &mut self,
        env: &mut ObservationEnv<'_>,
        start: bool,
    ) {
        let Some(port) = self.reports.clone() else {
            return;
        };
        if let Some((day, job)) = self.report.job.take() {
            if !job.is_finished() {
                self.report.job = Some((day, job));
                return;
            }
            match job.join() {
                Ok(Ok(done)) => {
                    for report in &done.written {
                        info!(
                            "KPI report of {} written: {}",
                            report.label,
                            report.html.display()
                        );
                    }
                    self.report.checked = Some(day);
                    self.report.failed = None;
                    if let Some((config, messages)) = done.push {
                        self.queue_pushes(config, messages, env.generators.clock.monotonic());
                    }
                }
                Ok(Err(error)) => {
                    warn!(error = %format_args!("{error:#}"), "the KPI reports could not be written: {error:#}");
                    self.report.failed = Some(env.generators.clock.monotonic());
                }
                Err(_) => {
                    warn!("the KPI report job panicked");
                    self.report.failed = Some(env.generators.clock.monotonic());
                }
            }
        }
        let since_failed = self.report.failed.map(|at| env.generators.clock.since(at));
        if !start_due(start, since_failed) {
            return;
        }
        let now = env.generators.clock.now();
        let day = report::local_day(now, (port.utc_offset)(now));
        if self.report.checked == Some(day) {
            return;
        }
        let queues = self.queues.clone();
        let files = env.files.clone();
        let token = env.token.clone();
        self.report.start(day, move || -> Result<ReportJob> {
            let setup = (port.setup)(now)?;
            let queue = queues.open()?;
            let written = report::write_due(&*queue, &*files, &setup, now, &token)?;
            // A broken [push] stops no report nor breach record.
            let config = (port.push_config)().unwrap_or_else(|error| {
                warn!(error = %format_args!("{error:#}"), "the [push] of host.toml could not be read: {error:#}");
                None
            });
            // The reports are recorded already: a failure from here on is
            // logged and does not fail the job, which would not write them
            // again.
            let push = push::check_breaches(&*queue, &setup, config.as_ref(), now)
                .and_then(|breaches| {
                    config
                        .map(|config| {
                            push::messages(&*queue, &config, &port.push_target, &breaches, &written)
                                .map(|messages| (config, messages))
                        })
                        .transpose()
                })
                .unwrap_or_else(|error| {
                    warn!(error = %format_args!("{error:#}"), "the KPI breaches or push messages could not be made: {error:#}");
                    None
                });
            Ok(ReportJob {
                written: written.into_iter().map(|(written, _)| written).collect(),
                push,
            })
        });
    }
}

/// Whether a report job may start now, with `start`: [`RETRY`] after a
/// failure, given as the time since.
fn start_due(start: bool, since_failed: Option<Duration>) -> bool {
    start && since_failed.is_none_or(|since| since >= RETRY)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report job starts again at the retry after a failure, not 1 ms
    /// before; never without `start`.
    #[test]
    fn a_report_starts_again_once_the_retry_passed() {
        let ms = Duration::from_millis(1);
        assert!(start_due(true, None));
        assert!(!start_due(false, None));
        assert!(!start_due(true, Some(RETRY - ms)));
        assert!(start_due(true, Some(RETRY)));
    }

    /// While stopped, no pass runs the job that reads every event nor
    /// records a breach resolved: nothing runs, none is due again after a
    /// failure, and the log is told once.
    #[test]
    fn a_stopped_report_job_reads_nothing_and_leaves_the_breaches_open() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let ran = Arc::new(AtomicUsize::new(0));
        let mut watch = ReportWatch::default();
        for day in [1, 1, 2] {
            let ran = ran.clone();
            watch.start(day, move || {
                ran.fetch_add(1, Ordering::SeqCst);
                panic!("the report job reads every event and judges the breaches")
            });
            assert!(!watch.running());
        }
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        assert_eq!(watch.checked, None);
        assert_eq!(watch.failed, None);
        assert!(watch.told_stopped);
    }
}
