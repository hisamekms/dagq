//! The observer job the supervisor starts on its timer, on the provider
//! its route gives (ADR-t1063-1 decisions 1, 4 and 5, ADR-t1222-1, task
//! 1223): a Codex one that found Codex unusable holds Codex and starts
//! again on the other provider, or, under `--no-claude`, records why. With
//! `[provider_fallback] jobs` off it starts again on the provider it could
//! not use once that provider's hold ends, a Claude one too (ADR-t1857-1).

use super::*;
use crate::domain::actor_model::records_unusable;
use crate::domain::event_kind::OBSERVE_FINISHED;
use crate::domain::provider_switch::SwitchReason;
use serde_json::Value;

/// The newest observations a supervisor reads for the finish of the one it
/// waited on.
const FINISH_EVENTS: usize = 10;

/// An observation this process started and waits on.
pub(super) struct ObserverJob {
    pub(super) mode: ObserveMode,
    pub(super) child: Box<dyn Spawned>,
    /// Whether a finish that says its provider could not be used holds
    /// that provider and makes the observation due again
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

/// The provider and reason of a finish's `provider_unusable` (an
/// `observe_finished` or a `throughput_review_finished`), when it has one
/// with a provider and a reason known.
pub(super) fn finish_unusable(payload: &Value) -> Option<(Provider, SwitchReason)> {
    let unusable = &payload["provider_unusable"];
    let provider = unusable["provider"].as_str()?.parse::<Provider>().ok()?;
    let reason = unusable["reason"].as_str()?.parse::<SwitchReason>().ok()?;
    Some((provider, reason))
}

/// What the finish of a job on its timer says about the provider it could
/// not use, read here so that the hold ([`Supervisor::hold_unusable`])
/// takes plain values: the provider and reason, the job's error and its
/// provider's words (`output.out` in its directory).
pub(super) struct UnusableFinish {
    pub(super) unusable: (Provider, SwitchReason),
    pub(super) error: String,
    pub(super) output: String,
}

/// The finish of the observation of `mode` started after `mark` among
/// `finished` (newest first), and the provider and reason it says could not
/// be used, when it says so (`provider_unusable`).
fn unusable_finish(
    finished: &[RunEvent],
    mark: EventId,
    mode: ObserveMode,
) -> Option<(&RunEvent, Provider, SwitchReason)> {
    let finish = finished
        .iter()
        .find(|event| event.id > mark && event.payload["mode"] == mode.as_str())?;
    let (provider, reason) = finish_unusable(&finish.payload)?;
    Some((finish, provider, reason))
}

impl Supervisor<'_> {
    /// The provider `finish` (an `observe_finished` or a
    /// `throughput_review_finished`) says could not be used, with its error
    /// and its provider's words, when it says so.
    pub(super) fn unusable_of(&self, finish: &RunEvent) -> Option<UnusableFinish> {
        let unusable = finish_unusable(&finish.payload)?;
        let error = finish.payload["error"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let output = finish.payload["dir"]
            .as_str()
            .and_then(|dir| {
                self.files
                    .read_to_string(&Path::new(dir).join("output.out"))
                    .ok()
            })
            .unwrap_or_default();
        Some(UnusableFinish {
            unusable,
            error,
            output,
        })
    }

    /// Hold the provider `finish` says could not be used
    /// ([`Self::unusable_of`], [`Self::hold_unusable`]); the provider held,
    /// when it is held now.
    pub(super) fn hold_finish_unusable(
        &mut self,
        finish: &RunEvent,
        job: &HoldJob,
        what: &str,
    ) -> Option<Provider> {
        let read = self.unusable_of(finish)?;
        self.hold_unusable(read.unusable, (&read.error, &read.output), job, what)
    }

    /// After an observation whose finish is read for `provider_unusable`
    /// ([`ObserverJob::retries_unusable`]) exited: when its finish says its
    /// provider could not be used, hold that provider as a worker's or
    /// another job's failure does ([`Self::hold_unusable`]: Codex's
    /// [`crate::domain::provider_switch::ProviderHold`], Claude's hold ask
    /// or, for an agent that did not start, its `ProviderHold`), and make
    /// the observation due again, so that it starts on the other provider
    /// (or, under `--no-claude`, records why), or, with `[provider_fallback]
    /// jobs` off, on the same provider once its hold ends (ADR-t1857-1). A
    /// hold that cannot be written leaves the observation to its interval,
    /// so it is not started again at once.
    pub(super) fn observer_unusable(&mut self, job: &ObserverJob) {
        let finished = match self.queue.latest_events_of(OBSERVE_FINISHED, FINISH_EVENTS) {
            Ok(finished) => finished,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the observer's finish could not be read: {error:#}");
                return;
            }
        };
        let Some((finish, _, _)) = unusable_finish(&finished, job.mark, job.mode) else {
            return;
        };
        let finish = finish.clone();
        let what = format!("observer ({})", job.mode.as_str());
        if self
            .hold_finish_unusable(&finish, &HoldJob::Observer, &what)
            .is_some()
        {
            self.observers_launched
                .retain(|(launched, _)| *launched != job.mode);
            self.observer_again = Some(job.mode);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (event, provider, reason) =
            unusable_finish(&finished, EventId::new(5), ObserveMode::Daily).unwrap();
        assert_eq!(
            (event.id, provider, reason),
            (EventId::new(7), Provider::Codex, SwitchReason::UsageLimit)
        );
        // Claude's, which only a fallback turned off records.
        let claude = serde_json::json!({"mode": "daily", "outcome": "failed",
            "provider_unusable": {"provider": "claude", "reason": "launch_failed"}});
        let (_, provider, reason) =
            unusable_finish(&[finish(8, claude)], EventId::new(5), ObserveMode::Daily).unwrap();
        assert_eq!(
            (provider, reason),
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
