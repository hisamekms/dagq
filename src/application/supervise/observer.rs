//! The observer job the supervisor starts on its timer, on the provider
//! its route gives (ADR-t1063-1 decisions 1, 4 and 5, ADR-t1222-1, task
//! 1223): a Codex one that found Codex unusable holds Codex and starts
//! again on the other provider, or, under `--no-claude`, records why.

use super::*;
use crate::domain::event_kind::OBSERVE_FINISHED;
use crate::domain::provider_switch::SwitchReason;

/// The newest observations a supervisor reads for the finish of the one it
/// waited on.
const FINISH_EVENTS: usize = 10;

/// An observation this process started and waits on.
pub(super) struct ObserverJob {
    pub(super) mode: ObserveMode,
    pub(super) child: Box<dyn Spawned>,
    /// Whether it runs on Codex for a role that names its provider: a
    /// finish that says Codex could not be used holds Codex and makes the
    /// observation due again.
    pub(super) switchable_codex: bool,
    /// The newest event when it started: its finish comes after.
    pub(super) mark: EventId,
}

/// The finish of the observation of `mode` started after `mark` among
/// `finished` (newest first), and the reason it says Codex could not be
/// used, when it says so (`provider_unusable` of `codex`).
fn codex_unusable(
    finished: &[RunEvent],
    mark: EventId,
    mode: ObserveMode,
) -> Option<(&RunEvent, SwitchReason)> {
    let finish = finished
        .iter()
        .find(|event| event.id > mark && event.payload["mode"] == mode.as_str())?;
    let unusable = &finish.payload["provider_unusable"];
    let reason = unusable["reason"]
        .as_str()
        .filter(|_| unusable["provider"] == Provider::Codex.as_str())?
        .parse::<SwitchReason>()
        .ok()?;
    Some((finish, reason))
}

impl Supervisor<'_> {
    /// After a Codex observation of a role that names its provider exited:
    /// when its finish says Codex could not be used (`provider_unusable`),
    /// hold Codex as a worker's or another job's failure does, and make
    /// the observation due again, so that it starts on the other provider
    /// (or, under `--no-claude`, records why). A hold that cannot be
    /// written leaves the observation to its interval, so it is not
    /// started again on Codex at once.
    pub(super) fn codex_observer_unusable(&mut self, job: &ObserverJob) {
        let finished = match self.queue.latest_events_of(OBSERVE_FINISHED, FINISH_EVENTS) {
            Ok(finished) => finished,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the observer's finish could not be read: {error:#}");
                return;
            }
        };
        let Some((finish, reason)) = codex_unusable(&finished, job.mark, job.mode) else {
            return;
        };
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
                    "observer ({}): codex cannot be used ({}); the observation starts again on the other provider",
                    job.mode.as_str(),
                    reason.as_str()
                );
                self.observers_launched
                    .retain(|(launched, _)| *launched != job.mode);
                self.observer_again = Some(job.mode);
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "codex could not be held after the observer failed: {error:#}");
            }
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
    /// that says Codex could not be used, with a reason it knows, holds
    /// Codex (task 1223).
    #[test]
    fn the_finish_past_the_mark_says_whether_codex_cannot_be_used() {
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
        let (event, reason) =
            codex_unusable(&finished, EventId::new(5), ObserveMode::Daily).unwrap();
        assert_eq!(
            (event.id, reason),
            (EventId::new(7), SwitchReason::UsageLimit)
        );
        // The hourly one past the mark succeeded; the older failure is
        // another observation's.
        assert!(codex_unusable(&finished, EventId::new(5), ObserveMode::Hourly).is_none());
        // Its finish not recorded yet past the mark.
        assert!(codex_unusable(&finished, EventId::new(7), ObserveMode::Daily).is_none());
        for payload in [
            serde_json::json!({"mode": "daily", "outcome": "failed"}),
            serde_json::json!({"mode": "daily", "provider_unusable": {"provider": "claude", "reason": "usage_limit"}}),
            serde_json::json!({"mode": "daily", "provider_unusable": {"provider": "codex", "reason": "no_such_reason"}}),
        ] {
            assert!(
                codex_unusable(
                    &[finish(8, payload.clone())],
                    EventId::new(5),
                    ObserveMode::Daily
                )
                .is_none(),
                "{payload}"
            );
        }
    }
}
