//! The throughput review's timer (ADR-t996-1): once the last whole hour,
//! yesterday or the ISO week before this one has no
//! `throughput_review_finished` in the queue (whichever supervisor ran it),
//! the supervisor starts `throughput-review` for it as a child process,
//! one at a time and outside the run slots, as it starts the observer. The
//! command judges the hour, runs the job, saves the review and tells the
//! inbox; its events are the record. A review that failed is not started
//! again for its period, but for one whose Codex could not be used
//! (`provider_unusable`, task 1220): Codex is held and the period starts
//! again on the other provider. Neither a failure nor a review in progress
//! holds a claim or a landing.

use super::*;
use crate::domain::actor_model::{ActorLaunch, JobRoute, ModelRole, job_route};
use crate::domain::event_kind::{THROUGHPUT_REVIEW_FINISHED, THROUGHPUT_REVIEW_STARTED};
use crate::domain::provider_switch::SwitchReason;
use crate::domain::throughput_review::{
    HISTORY_EVENTS, RUNNING_MS, ReviewMode, children_finished, reviewed, running, window,
};

/// The newest finishes a supervisor that exec'd reads for the reviews it
/// has to reap.
const HANDED_OVER_EVENTS: usize = 10;

/// A review this process started and waits on.
struct ReviewJob {
    mode: ReviewMode,
    period: String,
    child: Box<dyn Spawned>,
    /// Whether it runs on Codex for a role that names its provider: a
    /// finish that says Codex could not be used holds Codex and starts the
    /// period again (ADR-t1063-1 decision 4).
    switchable_codex: bool,
}

/// How the due review starts (ADR-t1063-1 decisions 1, 4 and 5,
/// ADR-t1204-1).
enum ReviewRoute {
    /// On this launch; `true` when `[roles.throughput_review]` names its
    /// provider.
    Start(ActorLaunch, bool),
    /// Under `--no-claude`, no provider can run it: the command records
    /// why for the period that needs a review, starting no agent.
    Unavailable(ActorLaunch, String),
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

    /// Where the due review goes, or `None` while it waits. A role that
    /// names no provider runs on Claude as before: it waits while the
    /// queue's hold ask holds Claude, and does not start under
    /// `--no-claude` (ADR-t1204-1 decision 2). One that names its provider
    /// starts there when it can be used, else on the other provider when
    /// that one can be, else waits, or, under `--no-claude`, records why
    /// (a Codex review never moves to Claude then).
    fn throughput_review_route(&self) -> Option<ReviewRoute> {
        let role = ModelRole::ThroughputReview;
        let models = self.role_models(role);
        let launch = models.launch(role);
        if !models.switchable(role) {
            return (!self.no_claude && self.queue_hold.is_none())
                .then_some(ReviewRoute::Start(launch, false));
        }
        match job_route(&launch, true, |provider| self.job_unusable(provider)) {
            JobRoute::Start(launch) => Some(ReviewRoute::Start(launch, true)),
            JobRoute::Wait { .. } if self.no_claude => {
                let codex = self
                    .job_unusable(Provider::Codex)
                    .map_or("unknown", SwitchReason::as_str);
                Some(ReviewRoute::Unavailable(
                    launch,
                    format!(
                        "provider_disabled: Claude is disabled by --no-claude and codex cannot be used ({codex}); handle this role manually"
                    ),
                ))
            }
            JobRoute::Wait { provider, reason } => {
                tracing::debug!(
                    "the throughput review waits: {} cannot be used ({}), nor can the other provider",
                    provider.as_str(),
                    reason.as_str()
                );
                None
            }
        }
    }

    /// Reap the review once it exited; when `start` and none runs, start
    /// the one due. A failure to start is logged and not retried for that
    /// period in this process.
    pub(super) fn throughput_review_pass(&mut self, options: &LoopSettings, start: bool) {
        self.reap_handed_over_reviews(options);
        if let Some(job) = self.throughput_review.job.as_mut() {
            let (mode, period) = (job.mode, &job.period);
            match job.child.try_wait() {
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
            if let Some(job) = self.throughput_review.job.take()
                && job.switchable_codex
            {
                self.codex_review_unusable(&job);
            }
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
        let Some(route) = self.throughput_review_route() else {
            return;
        };
        let (launch, switchable, unavailable) = match route {
            ReviewRoute::Start(launch, switchable) => (launch, switchable, None),
            ReviewRoute::Unavailable(launch, why) => (launch, false, Some(why)),
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
                    switchable_codex: switchable
                        && unavailable.is_none()
                        && launch.provider == Provider::Codex,
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
                // One that found Codex unusable holds it here, as for a
                // review this process started, so its period, due again,
                // goes to the other provider rather than to Codex once more.
                if let Some(finish) = finished
                    .iter()
                    .find(|event| event.payload["pid"].as_u64() == Some(u64::from(pid)))
                {
                    self.hold_codex_for(finish);
                }
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

    /// After a Codex review of a role that names its provider exited: when
    /// its finish says Codex could not be used (`provider_unusable`), hold
    /// Codex as a worker's or another job's failure does, and let the
    /// period be due again, so that it starts on the other provider (or,
    /// under `--no-claude`, records why) (ADR-t1063-1 decisions 4 and 5).
    /// A hold that cannot be written keeps the period as started, so it
    /// is not started again on Codex at once.
    fn codex_review_unusable(&mut self, job: &ReviewJob) {
        let finished = match self
            .queue
            .latest_events_of(THROUGHPUT_REVIEW_FINISHED, HANDED_OVER_EVENTS)
        {
            Ok(finished) => finished,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the throughput review's finish could not be read: {error:#}");
                return;
            }
        };
        let pid = u64::from(job.child.id());
        let Some(finish) = finished.iter().find(|event| {
            event.payload["mode"] == job.mode.as_str()
                && event.payload["period"] == job.period.as_str()
                && event.payload["pid"].as_u64() == Some(pid)
        }) else {
            return;
        };
        if self.hold_codex_for(finish) {
            self.throughput_review
                .launched
                .retain(|(mode, period)| !(*mode == job.mode && *period == job.period));
        }
    }

    /// Hold Codex when the review `finish` ended says Codex could not be
    /// used (`provider_unusable`), as a worker's or another job's failure
    /// does; whether it is held now. Its period is due again then, and
    /// starts on the other provider (or, under `--no-claude`, records
    /// why).
    fn hold_codex_for(&mut self, finish: &crate::domain::RunEvent) -> bool {
        let unusable = &finish.payload["provider_unusable"];
        let Some(reason) = unusable["reason"]
            .as_str()
            .filter(|_| unusable["provider"] == Provider::Codex.as_str())
            .and_then(|reason| reason.parse::<SwitchReason>().ok())
        else {
            return false;
        };
        let (mode, period) = (
            finish.payload["mode"].as_str().unwrap_or_default(),
            finish.payload["period"].as_str().unwrap_or_default(),
        );
        // Codex's own words may say when a usage limit resets.
        let output = finish.payload["dir"]
            .as_str()
            .and_then(|dir| {
                self.files
                    .read_to_string(&Path::new(dir).join("output.out"))
                    .ok()
            })
            .unwrap_or_default();
        let said = format!(
            "{}\n{output}",
            finish.payload["error"].as_str().unwrap_or_default()
        );
        match self.hold_provider(Provider::Codex, reason, None, &said) {
            Ok(()) => {
                info!(
                    "throughput review ({mode} {period}): codex cannot be used ({}); the period starts again on the other provider",
                    reason.as_str()
                );
                true
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "codex could not be held after the throughput review failed: {error:#}");
                false
            }
        }
    }
}
