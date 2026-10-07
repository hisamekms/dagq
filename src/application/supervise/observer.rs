//! The observer job the supervisor starts on its timer, on the provider
//! its route gives (ADR-t1063-1 decisions 1, 4 and 5, ADR-t1222-1, task
//! 1223): a Codex one that found Codex unusable publishes its finish, has
//! Codex held by 実行と着地, which owns the holds (ADR-t1545-1 decision 2),
//! and starts again on the other provider, or, under `--no-claude`,
//! records why. With `[provider_fallback] jobs` off it starts again on the
//! provider it could not use once that provider's hold ends, a Claude one
//! too (ADR-t1857-1).

use super::*;
use crate::domain::actor_model::records_unusable;
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
/// (`Supervisor::hold_timer_jobs_unusable`): the finish read
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

impl Supervisor<'_> {
    /// `finish` of `job` (`what` in the log) with its provider's words, to
    /// publish: `output.out` in its directory, empty when unreadable.
    pub(super) fn unusable_timer_job(
        &self,
        finish: UnusableFinish,
        hold: HoldJob,
        what: String,
        job: TimerJob,
    ) -> UnusableTimerJob {
        let output = finish
            .dir
            .as_deref()
            .and_then(|dir| {
                self.files
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

    /// After an observation whose finish is read for `provider_unusable`
    /// ([`ObserverJob::retries_unusable`]) exited: its finish, when it says
    /// its provider could not be used, for 実行と着地 to hold that provider
    /// as a worker's or another job's failure does (Codex's
    /// [`crate::domain::provider_switch::ProviderHold`], Claude's hold ask
    /// or, for an agent that did not start, its `ProviderHold`). Once it is
    /// held the observation is due again ([`Self::timer_job_due_again`]).
    pub(super) fn observer_unusable(&self, job: &ObserverJob) -> Option<UnusableTimerJob> {
        let finished = match self.queue.latest_events_of(OBSERVE_FINISHED, FINISH_EVENTS) {
            Ok(finished) => finished,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the observer's finish could not be read: {error:#}");
                return None;
            }
        };
        let finish = unusable_finish(&finished, job.mark, job.mode)?;
        Some(self.unusable_timer_job(
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
