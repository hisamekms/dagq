//! The observer job the supervisor starts on its timer, on the provider
//! its route gives (ADR-t1063-1 decisions 1, 4 and 5, ADR-t1222-1, task
//! 1223): a Codex one that found Codex unusable publishes its finish, has
//! Codex held by 実行と着地, which owns the holds (ADR-t1545-1 decision 2),
//! and starts again on the other provider, or, under `--no-claude`,
//! records why. With `[provider_fallback] jobs` off it starts again on the
//! provider it could not use once that provider's hold ends, a Claude one
//! too (ADR-t1857-1).

use super::*;
use crate::domain::actor_model::{JobStartRoute, records_unusable};
use crate::domain::event_kind::OBSERVE_FINISHED;
use crate::domain::queue_hold::HoldJob;
use crate::domain::throughput_review::{ReviewMode, UnusableFinish};

/// The newest observations a supervisor reads for the finish of the one it
/// waited on.
const FINISH_EVENTS: usize = 10;

/// An observation this process started and waits on.
pub(super) struct ObserverJob {
    pub(super) mode: ObserveMode,
    pub(super) child: Box<dyn Spawned>,
    /// Whether a finish that says its provider could not be used has
    /// that provider held and makes the observation due again
    /// ([`retries_unusable`]).
    pub(super) retries_unusable: bool,
    /// The newest event when it started: its finish comes after.
    pub(super) mark: EventId,
}

/// Whether the supervisor reads `provider_unusable` off the finish of a job
/// on its timer (an observation or a throughput review) that ran on
/// `provider` for a role that names its provider (`switchable`), when some
/// provider could run it (`unavailable` false): what the job records
/// ([`records_unusable`]), which then starts again elsewhere or, with
/// `[provider_fallback] jobs` off, on the same provider once its hold ends
/// (ADR-t1063-1 decision 4, ADR-t1857-1).
pub(super) const fn retries_unusable(
    provider: Provider,
    switchable: bool,
    fallback: bool,
    unavailable: bool,
) -> bool {
    !unavailable && records_unusable(provider, switchable, fallback)
}

/// The job on the timer a finish that says its provider could not be used
/// ended: what is due again once that provider is held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TimerJob {
    /// The observation of this mode, due again.
    Observer(ObserveMode),
    /// The throughput review of this mode and period, due again.
    Review(ReviewMode, String),
    /// A review the process before an exec left running: its period has
    /// no start of this process to clear.
    HandedOver,
}

/// The finish of a job on the timer that says its provider could not be
/// used, as 観測と分析 publishes it to 実行と着地, which holds that provider
/// (its `hold_timer_jobs_unusable`): the finish read
/// ([`UnusableFinish`]), its provider's words (`output.out` in its
/// directory), the job as the hold ask names it, the job as the log names
/// it, and what is due again once held.
pub(super) struct UnusableTimerJob {
    pub(super) finish: UnusableFinish,
    pub(super) output: String,
    pub(super) hold: HoldJob,
    pub(super) what: String,
    pub(super) job: TimerJob,
}

/// The finish of the observation of `mode` started after `mark` among
/// `finished` (newest first), read as one whose provider could not be
/// used, when it says so (`provider_unusable`).
fn unusable_finish(
    finished: &[RunEvent],
    mark: EventId,
    mode: ObserveMode,
) -> Option<UnusableFinish> {
    let finish = finished
        .iter()
        .find(|event| event.id > mark && event.payload["mode"] == mode.as_str())?;
    UnusableFinish::of(finish)
}

/// `finish` of `job` (`what` in the log) with its provider's words, to
/// publish: `output.out` in its directory, empty when unreadable.
pub(super) fn unusable_timer_job(
    files: &dyn RunFiles,
    finish: UnusableFinish,
    hold: HoldJob,
    what: String,
    job: TimerJob,
) -> UnusableTimerJob {
    let output = finish
        .dir
        .as_deref()
        .and_then(|dir| {
            files
                .read_to_string(&Path::new(dir).join("output.out"))
                .ok()
        })
        .unwrap_or_default();
    UnusableTimerJob {
        finish,
        output,
        hold,
        what,
        job,
    }
}

/// The observation due at `now` (unix seconds; `at` the monotonic time
/// the launches are judged on), if any: one due again whatever the queue
/// says, else the daily one when `daily` and it has not run for 24 hours,
/// else the hourly one when `interval` passed since the last one started
/// or finished (`last`, from the queue, whichever supervisor ran it) and
/// since this process last launched it (`launched`).
pub(super) fn due_observation(
    again: Option<ObserveMode>,
    interval: Duration,
    daily: bool,
    launched: &[(ObserveMode, Instant)],
    now: i64,
    at: Instant,
    mut last: impl FnMut(ObserveMode) -> Result<Option<i64>>,
) -> Result<Option<ObserveMode>> {
    if interval.is_zero() {
        return Ok(None);
    }
    let mut modes = vec![(ObserveMode::Hourly, i64::try_from(interval.as_secs())?)];
    if daily {
        modes.insert(0, (ObserveMode::Daily, DAILY_WINDOW_SECS));
    }
    if let Some(mode) = again {
        return Ok(Some(mode));
    }
    for (mode, every) in modes {
        let recorded = last(mode)?.is_some_and(|last| now - last < every);
        let launched = launched.iter().any(|(launched, when)| {
            *launched == mode
                && at.saturating_duration_since(*when).as_secs() < every.unsigned_abs()
        });
        if !recorded && !launched {
            return Ok(Some(mode));
        }
    }
    Ok(None)
}

impl ObservationState {
    /// After an observation whose finish is read for `provider_unusable`
    /// ([`ObserverJob::retries_unusable`]) exited: its finish, when it says
    /// its provider could not be used, for 実行と着地 to hold that provider
    /// as a worker's or another job's failure does (Codex's
    /// [`crate::domain::provider_switch::ProviderHold`], Claude's hold ask
    /// or, for an agent that did not start, its `ProviderHold`). Once it is
    /// held the observation is due again ([`Self::timer_job_due_again`]).
    fn observer_unusable(env: &PassEnv<'_>, job: &ObserverJob) -> Option<UnusableTimerJob> {
        let finished = match env.queue.latest_events_of(OBSERVE_FINISHED, FINISH_EVENTS) {
            Ok(finished) => finished,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the observer's finish could not be read: {error:#}");
                return None;
            }
        };
        let finish = unusable_finish(&finished, job.mark, job.mode)?;
        Some(unusable_timer_job(
            &**env.files,
            finish,
            HoldJob::Observer,
            format!("observer ({})", job.mode.as_str()),
            TimerJob::Observer(job.mode),
        ))
    }

    /// Make `job`, whose provider is held now, due again, so that it starts
    /// on the other provider (or, under `--no-claude`, records why), or,
    /// with `[provider_fallback] jobs` off, on the same provider once its
    /// hold ends (ADR-t1857-1). A job whose hold could not be written is
    /// not given here: it is left to its interval, so it is not started
    /// again at once.
    pub(super) fn timer_job_due_again(&mut self, job: TimerJob) {
        match job {
            TimerJob::Observer(mode) => {
                self.observers_launched
                    .retain(|(launched, _)| *launched != mode);
                self.observer_again = Some(mode);
            }
            TimerJob::Review(mode, period) => self.review_due_again(mode, &period),
            TimerJob::HandedOver => {}
        }
    }

    /// The observation due now, if none runs ([`due_observation`]).
    pub(super) fn observer_due(
        &self,
        env: &PassEnv<'_>,
        options: &LoopSettings,
    ) -> Result<Option<ObserveMode>> {
        if self.observer.is_some() || options.observe_interval.is_zero() {
            return Ok(None);
        }
        due_observation(
            self.observer_again,
            options.observe_interval,
            options.observe_daily,
            &self.observers_launched,
            env.generators.clock.now(),
            Instant::now(),
            |mode| env.queue.last_observe(mode.as_str()),
        )
    }

    /// Launch `dagq observe` for `mode` on `route` as a child process; its
    /// finish is the first `observe_finished` past `mark`. It takes no run
    /// slot. A failure to launch is logged and retried after the interval.
    /// `fallback_jobs` is `[provider_fallback] jobs`.
    pub(super) fn start_observer(
        &mut self,
        env: &PassEnv<'_>,
        mode: ObserveMode,
        route: JobStartRoute,
        mark: EventId,
        fallback_jobs: bool,
    ) {
        let (launch, switchable, unavailable) = match route {
            JobStartRoute::Start(launch, switchable) => (launch, switchable, None),
            JobStartRoute::Unavailable(launch, why) => (launch, false, Some(why)),
        };
        self.observer_again = None;
        self.observers_launched
            .retain(|(launched, _)| *launched != mode);
        self.observers_launched.push((mode, Instant::now()));
        let layout = env.layout;
        let mut command = CommandSpec::new(&layout.runner);
        command
            .arg("--db")
            .arg(&layout.db)
            .arg("observe")
            .arg("--claude")
            .arg(&layout.claude)
            .arg("--codex")
            .arg(&layout.codex)
            .arg("--launch")
            .arg(launch.to_value().to_string())
            .current_dir(&layout.repo_root);
        if let Some(home) = &layout.codex_home {
            command.arg("--codex-home").arg(home);
        }
        if switchable {
            command.arg("--switchable");
            if !fallback_jobs {
                command.arg("--no-provider-fallback");
            }
        }
        if let Some(why) = &unavailable {
            command.arg("--unavailable").arg(why);
        }
        for name in &layout.observer_env_remove {
            command.env_remove(name);
        }
        // The observe command is the supervisor's; its agent is the
        // observer (ADR-t728-1 decision 4).
        command.envs(layout.supervisor_actor().env());
        if mode == ObserveMode::Daily {
            command.arg("--daily");
        }
        match env.spawner.spawn(&command, Streams::Null) {
            Ok(child) => {
                info!(
                    "observer ({}) started on {}: pid {}",
                    mode.as_str(),
                    launch.provider.as_str(),
                    child.id()
                );
                self.observer = Some(ObserverJob {
                    mode,
                    child,
                    retries_unusable: retries_unusable(
                        launch.provider,
                        switchable,
                        fallback_jobs,
                        unavailable.is_some(),
                    ),
                    mark,
                });
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "observer ({}) could not start: {error:#}", mode.as_str())
            }
        }
    }

    /// Kill the observer still running and the processes it started (its
    /// agent and that agent's Bash), so none outlives this supervisor or
    /// runs on unwatched after its exec; `why` ends the log line.
    pub(super) fn stop_observer(&mut self, env: &PassEnv<'_>, why: &str) {
        let Some(ObserverJob {
            mode, mut child, ..
        }) = self.observer.take()
        else {
            return;
        };
        // Listed before the kill: once `observe` is gone, its agent is no
        // longer its descendant.
        let descendants = env.processes.descendants(child.id());
        let _ = child.kill();
        let _ = child.wait();
        for pid in &descendants {
            let _ = env.processes.kill(*pid);
        }
        info!(
            "observer ({}) stopped {why}: pid {} and {} descendant(s) killed",
            mode.as_str(),
            child.id(),
            descendants.len()
        );
    }

    /// Reap the observer once it exited; its own `observe_finished` is the
    /// record. The finish of one that found its provider unusable, for
    /// 実行と着地 to hold that provider ([`Self::observer_unusable`]); it
    /// is due again once held ([`Self::timer_job_due_again`]).
    pub(super) fn poll_observer(&mut self, env: &PassEnv<'_>) -> Option<UnusableTimerJob> {
        let job = self.observer.as_mut()?;
        let mode = job.mode;
        match job.child.try_wait() {
            Ok(None) => return None,
            Ok(Some(status)) => {
                info!("observer ({}) exited: {status}", mode.as_str());
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "observer ({}) could not be waited for: {error:#}", mode.as_str());
            }
        }
        let job = self.observer.take()?;
        if !job.retries_unusable {
            return None;
        }
        Self::observer_unusable(env, &job)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::provider_switch::SwitchReason;

    fn finish(id: i64, payload: serde_json::Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: OBSERVE_FINISHED.to_owned(),
            payload,
            created_at: "2026-10-05T00:00:00Z".to_owned(),
            actor: None,
        }
    }

    /// The finish read is the newest of the mode past the mark; only one
    /// that says its provider could not be used, with a provider and a
    /// reason it knows, holds it (task 1223, ADR-t1857-1).
    #[test]
    fn the_finish_past_the_mark_says_whether_its_provider_cannot_be_used() {
        let unusable = serde_json::json!({"provider": "codex", "reason": "usage_limit"});
        let finished = [
            finish(
                9,
                serde_json::json!({"mode": "hourly", "outcome": "succeeded"}),
            ),
            finish(
                7,
                serde_json::json!({"mode": "daily", "outcome": "failed",
                                         "provider_unusable": unusable}),
            ),
            finish(
                3,
                serde_json::json!({"mode": "hourly", "outcome": "failed",
                                         "provider_unusable": unusable}),
            ),
        ];
        let read = unusable_finish(&finished, EventId::new(5), ObserveMode::Daily).unwrap();
        assert_eq!(
            (read.event, read.provider, read.reason),
            (EventId::new(7), Provider::Codex, SwitchReason::UsageLimit)
        );
        // Claude's, which only a fallback turned off records.
        let claude = serde_json::json!({"mode": "daily", "outcome": "failed",
            "provider_unusable": {"provider": "claude", "reason": "launch_failed"}});
        let read =
            unusable_finish(&[finish(8, claude)], EventId::new(5), ObserveMode::Daily).unwrap();
        assert_eq!(
            (read.provider, read.reason),
            (Provider::Claude, SwitchReason::LaunchFailed)
        );
        // The hourly one past the mark succeeded; the older failure is
        // another observation's.
        assert!(unusable_finish(&finished, EventId::new(5), ObserveMode::Hourly).is_none());
        // Its finish not recorded yet past the mark.
        assert!(unusable_finish(&finished, EventId::new(7), ObserveMode::Daily).is_none());
        for payload in [
            serde_json::json!({"mode": "daily", "outcome": "failed"}),
            serde_json::json!({"mode": "daily", "provider_unusable": {"provider": "gemini", "reason": "usage_limit"}}),
            serde_json::json!({"mode": "daily", "provider_unusable": {"provider": "codex", "reason": "no_such_reason"}}),
        ] {
            assert!(
                unusable_finish(
                    &[finish(8, payload.clone())],
                    EventId::new(5),
                    ObserveMode::Daily
                )
                .is_none(),
                "{payload}"
            );
        }
    }

    /// The observation due is judged on the time and the queue's last
    /// observations given as values: one due again first, then the daily
    /// one, then the hourly one once its interval passed both since the
    /// queue's last one and since this process launched it (not 1 s
    /// before), and none with a zero interval.
    #[test]
    fn the_observation_due_follows_the_time_and_the_last_observations_given() {
        let hour = Duration::from_secs(3600);
        let at = Instant::now();
        let now = 1_000_000;
        let none = |_: ObserveMode| Ok(None);
        let due = |again,
                   daily,
                   launched: &[(ObserveMode, Instant)],
                   last: &dyn Fn(ObserveMode) -> Option<i64>| {
            due_observation(again, hour, daily, launched, now, at, |mode| Ok(last(mode))).unwrap()
        };
        assert_eq!(
            due_observation(None, Duration::ZERO, true, &[], now, at, none).unwrap(),
            None
        );
        assert_eq!(
            due(Some(ObserveMode::Hourly), true, &[], &|_| None),
            Some(ObserveMode::Hourly)
        );
        assert_eq!(due(None, true, &[], &|_| None), Some(ObserveMode::Daily));
        let daily_done = |mode| (mode == ObserveMode::Daily).then_some(now - 60);
        assert_eq!(due(None, true, &[], &daily_done), Some(ObserveMode::Hourly));
        // The queue's last hourly one 1 s before the interval ends, then at it.
        let hourly_at =
            |secs: i64| move |mode| Some(now - if mode == ObserveMode::Daily { 60 } else { secs });
        assert_eq!(due(None, true, &[], &hourly_at(3599)), None);
        assert_eq!(
            due(None, true, &[], &hourly_at(3600)),
            Some(ObserveMode::Hourly)
        );
        // Launched by this process within the interval, recorded or not.
        let launched = [(
            ObserveMode::Hourly,
            at.checked_sub(Duration::from_secs(3599)).unwrap(),
        )];
        assert_eq!(due(None, false, &launched, &|_| None), None);
        let launched = [(ObserveMode::Hourly, at.checked_sub(hour).unwrap())];
        assert_eq!(
            due(None, false, &launched, &|_| None),
            Some(ObserveMode::Hourly)
        );
    }

    /// Which timer jobs' finishes are read for `provider_unusable`: a Codex
    /// one of a role that names its provider, on or off, and a Claude one
    /// only with the fallback off; never one that no provider could run or
    /// whose role names none (ADR-t1063-1 decision 4, ADR-t1857-1).
    #[test]
    fn a_timer_job_is_retried_on_its_provider_only_as_its_role_and_fallback_say() {
        for fallback in [true, false] {
            assert!(retries_unusable(Provider::Codex, true, fallback, false));
            assert!(!retries_unusable(Provider::Codex, true, fallback, true));
            assert!(!retries_unusable(Provider::Codex, false, fallback, false));
            assert!(!retries_unusable(Provider::Claude, false, fallback, false));
        }
        assert!(!retries_unusable(Provider::Claude, true, true, false));
        assert!(retries_unusable(Provider::Claude, true, false, false));
        assert!(!retries_unusable(Provider::Claude, true, false, true));
    }
}
