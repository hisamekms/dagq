//! The daily KPI report (ADR-0051 decision 20): on the first pass after the
//! host's local midnight, a job thread writes the reports the queue does
//! not record as written yet (the 7 days before today and the ISO week
//! before this one, [`report::write_due`]) under `<queue dir>/reports/`
//! and records each as `report_written`, so the next process and the other
//! supervisors do not write it again. It takes no run slot and uses no
//! LLM; the loop does not wait for it but like the observer, before it
//! ends. A failure is logged and tried again after [`RETRY`]; it stops no
//! claim nor landing. After the reports the same job records the targets'
//! breaches that started and ended and makes the messages of the host's
//! push command (ADR-0051 decisions 18 and 23), which [`super::push`]
//! sends.

use super::*;
use crate::application::push::{self, PushOutcome, PushRequest, PushTarget};
use crate::application::report::{self, ReportSetup, Written};
use crate::domain::kpi::push::{PushConfig, PushMessage};

/// How long after a failed report job the reports are looked for again.
const RETRY: Duration = Duration::from_secs(600);

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
pub(super) struct ReportJob {
    written: Vec<Written>,
    push: Option<(PushConfig, Vec<PushMessage>)>,
}

/// The report job running now, and the local day the reports were last
/// found written.
#[derive(Default)]
pub(super) struct ReportWatch {
    job: Option<(i64, thread::JoinHandle<Result<ReportJob>>)>,
    checked: Option<i64>,
    failed: Option<Instant>,
}

impl ReportWatch {
    pub(super) const fn running(&self) -> bool {
        self.job.is_some()
    }
}

impl ObservationState {
    /// Reap the report job once it ended; start one when the local day
    /// changed since the reports were last found written.
    pub(super) fn report_pass(&mut self, env: &mut PassEnv<'_>, start: bool) {
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
                        self.queue_pushes(config, messages);
                    }
                }
                Ok(Err(error)) => {
                    warn!(error = %format_args!("{error:#}"), "the KPI reports could not be written: {error:#}");
                    self.report.failed = Some(Instant::now());
                }
                Err(_) => {
                    warn!("the KPI report job panicked");
                    self.report.failed = Some(Instant::now());
                }
            }
        }
        if !start
            || self
                .report
                .failed
                .is_some_and(|failed| failed.elapsed() < RETRY)
        {
            return;
        }
        let now = env.generators.clock.now();
        let day = report::local_day(now, (port.utc_offset)(now));
        if self.report.checked == Some(day) {
            return;
        }
        let queues = env.queues.clone();
        let files = env.files.clone();
        let token = env.token.clone();
        let job = spawn_traced(move || -> Result<ReportJob> {
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
        self.report.job = Some((day, job));
    }
}
