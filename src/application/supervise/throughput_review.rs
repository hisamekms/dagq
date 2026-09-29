//! The throughput review's timer (ADR-t996-1): once the last whole hour,
//! yesterday or the ISO week before this one has no
//! `throughput_review_finished` in the queue (whichever supervisor ran it),
//! the supervisor starts `throughput-review` for it as a child process,
//! one at a time and outside the run slots, as it starts the observer. The
//! command judges the hour, runs the job, saves the review and tells the
//! inbox; its events are the record. A review that failed is not started
//! again for its period, and neither a failure nor a review in progress
//! holds a claim or a landing.

use super::*;
use crate::domain::event_kind::{THROUGHPUT_REVIEW_FINISHED, THROUGHPUT_REVIEW_STARTED};
use crate::domain::throughput_review::{
    HISTORY_EVENTS, RUNNING_MS, ReviewMode, children_finished, reviewed, running, window,
};

/// The newest finishes a supervisor that exec'd reads for the reviews it
/// has to reap.
const HANDED_OVER_EVENTS: usize = 10;

/// The review running now and the periods this process started.
#[derive(Default)]
pub(super) struct ThroughputReviewWatch {
    job: Option<(ReviewMode, String, Box<dyn Spawned>)>,
    /// So a review that dies before it records its finish is not started
    /// again on every pass.
    launched: Vec<(ReviewMode, String)>,
    /// When this process first looked (unix milliseconds): after an exec,
    /// the reviews the process before left running finish from then on.
    first_pass_ms: Option<i64>,
    /// The handed-over reviews this process reaped already, and the
    /// reviews it started itself, which it waits on.
    reaped: Vec<u32>,
}

impl ThroughputReviewWatch {
    pub(super) const fn running(&self) -> bool {
        self.job.is_some()
    }
}

impl Supervisor<'_> {
    /// The review due at `now`, `offset` seconds east of UTC: the hour
    /// first (its period passes soonest), then the day, then the week, whose
    /// latest finished period has no finish recorded and no start of the
    /// last [`crate::domain::throughput_review::RUNNING_MS`] (one a handoff
    /// left running, or another supervisor's).
    fn due_throughput_review(&self, offset: i64, now: i64) -> Result<Option<(ReviewMode, String)>> {
        let offset_ms = offset * 1000;
        let finished = self
            .queue
            .latest_events_of(THROUGHPUT_REVIEW_FINISHED, HISTORY_EVENTS)?;
        let started = self
            .queue
            .latest_events_of(THROUGHPUT_REVIEW_STARTED, HISTORY_EVENTS)?;
        for mode in [ReviewMode::Hourly, ReviewMode::Daily, ReviewMode::Weekly] {
            let label = window(mode, now * 1000, offset_ms).label;
            let launched = self
                .throughput_review
                .launched
                .iter()
                .any(|(was, period)| *was == mode && *period == label);
            if !launched
                && !reviewed(&finished, mode, &label)
                && !running(&started, mode, &label, now * 1000)
            {
                return Ok(Some((mode, label)));
            }
        }
        Ok(None)
    }

    /// Reap the review once it exited; when `start` and none runs, start
    /// the one due. A failure to start is logged and not retried for that
    /// period in this process.
    pub(super) fn throughput_review_pass(&mut self, options: &LoopSettings, start: bool) {
        self.reap_handed_over_reviews(options);
        if let Some((mode, period, child)) = self.throughput_review.job.as_mut() {
            match child.try_wait() {
                Ok(None) => return,
                Ok(Some(status)) => {
                    info!(
                        "throughput review ({} {period}) exited: {status}",
                        mode.as_str()
                    );
                }
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "throughput review ({} {period}) could not be waited for: {error:#}", mode.as_str());
                }
            }
            self.throughput_review.job = None;
        }
        if !start || !options.throughput_review {
            return;
        }
        let now = self.generators.clock.now();
        let offset = (options.utc_offset)(now);
        let (mode, period) = match self.due_throughput_review(offset, now) {
            Ok(Some(due)) => due,
            Ok(None) => return,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "throughput review schedule could not be read: {error:#}");
                return;
            }
        };
        self.throughput_review
            .launched
            .retain(|(was, _)| *was != mode);
        self.throughput_review.launched.push((mode, period.clone()));
        let mut command = CommandSpec::new(&self.layout.runner);
        command
            .arg("--db")
            .arg(&self.layout.db)
            .arg("throughput-review")
            .arg("--mode")
            .arg(mode.as_str())
            .arg("--at")
            .arg(now.to_string())
            .arg("--utc-offset")
            .arg(offset.to_string())
            .arg("--claude")
            .arg(&self.layout.claude)
            .current_dir(&self.layout.repo_root);
        for name in &self.layout.observer_env_remove {
            command.env_remove(name);
        }
        // The command is the supervisor's; its agent is the
        // throughput-review-job (ADR-t996-1 decision 4).
        command.envs(self.layout.supervisor_actor().env());
        match self.spawner.spawn(&command, Streams::Null) {
            Ok(child) => {
                info!(
                    "throughput review ({} {period}) started: pid {}",
                    mode.as_str(),
                    child.id()
                );
                // This process waits on its own reviews: none is reaped as
                // handed over.
                self.throughput_review.reaped.push(child.id());
                self.throughput_review.job = Some((mode, period, child));
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "throughput review ({} {period}) could not start: {error:#}", mode.as_str());
            }
        }
    }

    /// Reap the reviews the process before an exec left running (a handoff
    /// does not stop them): they are still children of this pid, and with
    /// no one waiting on them would stay zombies until the supervisor
    /// exits. Their finish names their pid; a zombie keeps its pid, so it
    /// is reaped once and nothing else. Only a supervisor that took over by
    /// an exec looks, for [`RUNNING_MS`] after its first pass, by when such
    /// a review finished or was killed at its timeout.
    fn reap_handed_over_reviews(&mut self, options: &LoopSettings) {
        if options.handoff_token.is_none() {
            return;
        }
        let now_ms = self.generators.clock.now() * 1000;
        let first = *self.throughput_review.first_pass_ms.get_or_insert(now_ms);
        if now_ms - first > RUNNING_MS {
            return;
        }
        let finished = match self
            .queue
            .latest_events_of(THROUGHPUT_REVIEW_FINISHED, HANDED_OVER_EVENTS)
        {
            Ok(finished) => finished,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the throughput reviews to reap could not be read: {error:#}");
                return;
            }
        };
        // A review may finish just before the exec, with nobody left to
        // wait on it.
        for pid in children_finished(&finished, self.layout.pid, first - 60_000) {
            if !self.throughput_review.reaped.contains(&pid) {
                self.processes.reap(pid);
                self.throughput_review.reaped.push(pid);
                info!("throughput review handed over by the exec reaped: pid {pid}");
            }
        }
    }

    /// Kill the review still running and the processes it started, so none
    /// outlives this supervisor; `why` ends the log line. Its period has no
    /// finish, so a supervisor starts it again once its start is older than
    /// `RUNNING_MS`. A handoff does not stop it: the review goes on under
    /// the exec'd process, records its own finish, and its start keeps the
    /// next process from starting it again.
    pub(super) fn stop_throughput_review(&mut self, why: &str) {
        let Some((mode, period, mut child)) = self.throughput_review.job.take() else {
            return;
        };
        let descendants = self.processes.descendants(child.id());
        let _ = child.kill();
        let _ = child.wait();
        for pid in &descendants {
            let _ = self.processes.kill(*pid);
        }
        info!(
            "throughput review ({} {period}) stopped {why}: pid {} and {} descendant(s) killed",
            mode.as_str(),
            child.id(),
            descendants.len()
        );
    }
}
