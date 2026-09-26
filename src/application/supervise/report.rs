//! The daily KPI report (ADR-0051 decision 20): on the first pass after the
//! host's local midnight, a job thread writes the reports the queue does
//! not record as written yet (the 7 days before today and the ISO week
//! before this one, [`report::write_due`]) under `<queue dir>/reports/`
//! and records each as `report_written`, so the next process and the other
//! supervisors do not write it again. It takes no run slot and uses no
//! LLM; the loop does not wait for it but like the observer, before it
//! ends. A failure is logged and tried again after [`RETRY`]; it stops no
//! claim nor landing.

use super::*;
use crate::application::report::{self, ReportSetup, Written};

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
}

/// The report job running now, and the local day the reports were last
/// found written.
#[derive(Default)]
pub(super) struct ReportWatch {
    job: Option<(i64, thread::JoinHandle<Result<Vec<Written>>>)>,
    checked: Option<i64>,
    failed: Option<Instant>,
}

impl ReportWatch {
    pub(super) const fn running(&self) -> bool {
        self.job.is_some()
    }
}

impl Supervisor<'_> {
    /// Reap the report job once it ended; start one when the local day
    /// changed since the reports were last found written.
    pub(super) fn report_pass(&mut self, start: bool) {
        let Some(port) = self.reports.clone() else {
            return;
        };
        if let Some((day, job)) = self.report.job.take() {
            if !job.is_finished() {
                self.report.job = Some((day, job));
                return;
            }
            match job.join() {
                Ok(Ok(written)) => {
                    for report in &written {
                        info!(
                            "KPI report of {} written: {}",
                            report.label,
                            report.html.display()
                        );
                    }
                    self.report.checked = Some(day);
                    self.report.failed = None;
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
        let now = self.generators.clock.now();
        let day = report::local_day(now, (port.utc_offset)(now));
        if self.report.checked == Some(day) {
            return;
        }
        let queues = self.queues.clone();
        let files = self.files.clone();
        let token = self.token.clone();
        let job = spawn_traced(move || -> Result<Vec<Written>> {
            let setup = (port.setup)(now)?;
            let queue = queues.open()?;
            report::write_due(&*queue, &*files, &setup, now, &token)
        });
        self.report.job = Some((day, job));
    }
}
