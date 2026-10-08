//! The throughput review's timer (ADR-t996-1): once the last whole hour,
//! yesterday or the ISO week before this one has no
//! `throughput_review_finished` in the queue (whichever supervisor ran it),
//! the supervisor starts `throughput-review` for it as a child process,
//! one at a time and outside the run slots, as it starts the observer. The
//! command judges the hour, runs the job, saves the review and tells the
//! inbox; its events are the record. A review that failed is not started
//! again for its period, but for one whose Codex could not be used
//! (`provider_unusable`, task 1220): Codex is held and the period starts
//! again on the other provider, or, with `[provider_fallback] jobs` off,
//! on Codex once its hold ends; with the fallback off a Claude one of a
//! role that names Claude holds Claude and starts again on Claude once
//! Claude's hold ends (ADR-t1857-1). Neither a failure nor a review in
//! progress holds a claim or a landing.

use super::*;
use crate::domain::actor_model::{JobStartRoute, ModelRole, UnnamedWithoutClaude, job_start_route};
use crate::domain::event_kind::{THROUGHPUT_REVIEW_FINISHED, THROUGHPUT_REVIEW_STARTED};
use crate::domain::queue_hold::HoldJob;
use crate::domain::throughput_review::{
    HISTORY_EVENTS, RUNNING_MS, ReviewMode, UnusableFinish, children_finished, reviewed, running,
    window,
};

/// The newest finishes a supervisor that exec'd reads for the reviews it
/// has to reap.
const HANDED_OVER_EVENTS: usize = 10;

/// A review this process started and waits on.
struct ReviewJob {
    mode: ReviewMode,
    period: String,
    child: Box<dyn Spawned>,
    /// Whether a finish that says its provider could not be used holds that
    /// provider and starts the period again ([`observer::retries_unusable`]:
    /// a Codex one of a role that names its provider, ADR-t1063-1 decision
    /// 4, and a Claude one too with `[provider_fallback] jobs` off,
    /// ADR-t1857-1).
    retries_unusable: bool,
}

/// The review running now and the periods this process started.
#[derive(Default)]
pub(super) struct ThroughputReviewWatch {
    job: Option<ReviewJob>,
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
                && !running(&started, &finished, mode, &label, now * 1000)
            {
                return Ok(Some((mode, label)));
            }
        }
        Ok(None)
    }

    /// Where the due job of `role` (the throughput review, the observer)
    /// goes, or `None` while it waits ([`job_start_route`] with
    /// `[provider_fallback] jobs`): a role that names no provider does not
    /// start under `--no-claude` (ADR-t1204-1 decision 2).
    pub(super) fn job_start_route(&self, role: ModelRole) -> Option<JobStartRoute> {
        self.start_route(role, UnnamedWithoutClaude::Wait)
    }

    /// Where the due job of `role` goes, from `[roles.<role>]` as it reads
    /// now, the queue's hold ask, `[provider_fallback] jobs` and why each
    /// provider cannot be used ([`job_start_route`]), or `None` while it
    /// waits, saying why in the debug log.
    pub(super) fn start_route(
        &self,
        role: ModelRole,
        unnamed: UnnamedWithoutClaude,
    ) -> Option<JobStartRoute> {
        let models = self.role_models(role);
        job_start_route(
            models.launch(role),
            models.switchable(role),
            self.no_claude,
            self.queue_hold.is_some(),
            self.fallback.jobs,
            unnamed,
            |provider| self.job_unusable(provider),
        )
        .inspect_err(|why| {
            if let Some(why) = why {
                tracing::debug!("the {} job waits: {why}", role.as_str());
            }
        })
        .ok()
    }

    /// Reap the review once it exited, and those an exec handed over: the
    /// finishes among them that say their provider could not be used, for
    /// 実行と着地 to hold that provider before the next start
    /// ([`Self::start_throughput_review`]).
    pub(super) fn reap_throughput_reviews(
        &mut self,
        options: &LoopSettings,
    ) -> Vec<observer::UnusableTimerJob> {
        let mut unusable = self.reap_handed_over_reviews(options);
        if let Some(job) = self.throughput_review.job.as_mut() {
            let (mode, period) = (job.mode, &job.period);
            match job.child.try_wait() {
                Ok(None) => return unusable,
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
            if let Some(job) = self.throughput_review.job.take()
                && job.retries_unusable
            {
                unusable.extend(self.review_unusable(&job));
            }
        }
        unusable
    }

    /// When `start` and none runs, start the review due. A failure to
    /// start is logged and not retried for that period in this process.
    pub(super) fn start_throughput_review(&mut self, options: &LoopSettings, start: bool) {
        if !start || !options.throughput_review || self.throughput_review.running() {
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
        let Some(route) = self.job_start_route(ModelRole::ThroughputReview) else {
            return;
        };
        let (launch, switchable, unavailable) = match route {
            JobStartRoute::Start(launch, switchable) => (launch, switchable, None),
            JobStartRoute::Unavailable(launch, why) => (launch, false, Some(why)),
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
            .arg("--codex")
            .arg(&self.layout.codex)
            .arg("--launch")
            .arg(launch.to_value().to_string())
            .current_dir(&self.layout.repo_root);
        if let Some(home) = &self.layout.codex_home {
            command.arg("--codex-home").arg(home);
        }
        if switchable {
            command.arg("--switchable");
            if !self.fallback.jobs {
                command.arg("--no-provider-fallback");
            }
        }
        if let Some(why) = &unavailable {
            command.arg("--unavailable").arg(why);
        }
        for name in &self.layout.observer_env_remove {
            command.env_remove(name);
        }
        // The command is the supervisor's; its agent is the
        // throughput-review-job (ADR-t996-1 decision 4).
        command.envs(self.layout.supervisor_actor().env());
        match self.spawner.spawn(&command, Streams::Null) {
            Ok(child) => {
                info!(
                    "throughput review ({} {period}) started on {}: pid {}",
                    mode.as_str(),
                    launch.provider.as_str(),
                    child.id()
                );
                // This process waits on its own reviews: none is reaped as
                // handed over.
                self.throughput_review.reaped.push(child.id());
                self.throughput_review.job = Some(ReviewJob {
                    mode,
                    period,
                    child,
                    retries_unusable: observer::retries_unusable(
                        launch.provider,
                        switchable,
                        self.fallback.jobs,
                        unavailable.is_some(),
                    ),
                });
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
    fn reap_handed_over_reviews(
        &mut self,
        options: &LoopSettings,
    ) -> Vec<observer::UnusableTimerJob> {
        let mut unusable = Vec::new();
        if options.handoff_token.is_none() {
            return unusable;
        }
        let now_ms = self.generators.clock.now() * 1000;
        let first = *self.throughput_review.first_pass_ms.get_or_insert(now_ms);
        if now_ms - first > RUNNING_MS {
            return unusable;
        }
        let finished = match self
            .queue
            .latest_events_of(THROUGHPUT_REVIEW_FINISHED, HANDED_OVER_EVENTS)
        {
            Ok(finished) => finished,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the throughput reviews to reap could not be read: {error:#}");
                return unusable;
            }
        };
        // A review may finish just before the exec, with nobody left to
        // wait on it.
        for pid in children_finished(&finished, self.layout.pid, first - 60_000) {
            if !self.throughput_review.reaped.contains(&pid) {
                self.processes.reap(pid);
                self.throughput_review.reaped.push(pid);
                info!("throughput review handed over by the exec reaped: pid {pid}");
                // One that found its provider unusable has it held, as for
                // a review this process started, so its period, due again,
                // goes to the other provider (or waits for this one) rather
                // than to it once more.
                if let Some(finish) = finished
                    .iter()
                    .find(|event| event.payload["pid"].as_u64() == Some(u64::from(pid)))
                    && let Some(read) = UnusableFinish::of(finish)
                {
                    unusable.push(self.unusable_timer_job(
                        read,
                        HoldJob::ThroughputReview,
                        review_entry(finish),
                        observer::TimerJob::HandedOver,
                    ));
                }
            }
        }
        unusable
    }

    /// Kill the review still running and the processes it started, so none
    /// outlives this supervisor; `why` ends the log line. Its period has no
    /// finish, so a supervisor starts it again once its start is older than
    /// `RUNNING_MS`. A handoff does not stop it: the review goes on under
    /// the exec'd process, records its own finish, and its start keeps the
    /// next process from starting it again.
    pub(super) fn stop_throughput_review(&mut self, why: &str) {
        let Some(ReviewJob {
            mode,
            period,
            mut child,
            ..
        }) = self.throughput_review.job.take()
        else {
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

    /// After a review whose finish is read for `provider_unusable`
    /// ([`ReviewJob::retries_unusable`]) exited: its finish, when it says
    /// its provider could not be used, for 実行と着地 to hold that provider
    /// as a worker's or another job's failure does; once it is held the
    /// period is due again ([`Self::review_due_again`]; ADR-t1063-1
    /// decisions 4 and 5, ADR-t1857-1).
    fn review_unusable(&self, job: &ReviewJob) -> Option<observer::UnusableTimerJob> {
        let finished = match self
            .queue
            .latest_events_of(THROUGHPUT_REVIEW_FINISHED, HANDED_OVER_EVENTS)
        {
            Ok(finished) => finished,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the throughput review's finish could not be read: {error:#}");
                return None;
            }
        };
        let pid = u64::from(job.child.id());
        let finish = finished.iter().find(|event| {
            event.payload["mode"] == job.mode.as_str()
                && event.payload["period"] == job.period.as_str()
                && event.payload["pid"].as_u64() == Some(pid)
        })?;
        Some(self.unusable_timer_job(
            UnusableFinish::of(finish)?,
            HoldJob::ThroughputReview,
            review_entry(finish),
            observer::TimerJob::Review(job.mode, job.period.clone()),
        ))
    }

    /// Let the period of `mode`, whose review's provider is held now, be
    /// due again, so that it starts on the other provider (or, under
    /// `--no-claude`, records why), or, with `[provider_fallback] jobs`
    /// off, on the same provider once its hold ends. A review whose hold
    /// could not be written keeps its period as started, so it is not
    /// started again at once.
    pub(super) fn review_due_again(&mut self, mode: ReviewMode, period: &str) {
        self.throughput_review
            .launched
            .retain(|(was, was_period)| !(*was == mode && was_period == period));
    }
}

/// The review `finish` ended, as the log names it.
fn review_entry(finish: &crate::domain::RunEvent) -> String {
    format!(
        "throughput review ({} {})",
        finish.payload["mode"].as_str().unwrap_or_default(),
        finish.payload["period"].as_str().unwrap_or_default()
    )
}
