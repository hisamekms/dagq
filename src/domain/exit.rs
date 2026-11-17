//! The retries of a `/exit` a session held back (ADR-0047 decision 25):
//! how many and how far apart, as `[exit]` of `dagq.toml` sets them. Since
//! task 1437 retired the interactive worker, `[exit]` is still checked when
//! `dagq.toml` is parsed (old files keep working) and nothing reads it.

use std::time::Duration;

/// The `reason` of `exit_requested`, serialized by [`Self::payload`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// `resume_timeout`: a resume turn reached its configured time limit.
    ResumeTimeout,
    /// `revise_deadline`: a revise/conflict turn reached its time limit.
    ReviseDeadline,
    /// `idle_unresolved`: idle without a receipt resolving the request.
    IdleUnresolved,
    /// `idle_after_receipt`: idle after the rewritten receipt.
    IdleAfterReceipt,
    /// `heartbeat_expired`: the wrapper stopped heartbeating.
    HeartbeatExpired,
    /// `turn_ended`: exit after the turn/stage ended for another reason.
    TurnEnded,
}

impl ExitReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ResumeTimeout => "resume_timeout",
            Self::ReviseDeadline => "revise_deadline",
            Self::IdleUnresolved => "idle_unresolved",
            Self::IdleAfterReceipt => "idle_after_receipt",
            Self::HeartbeatExpired => "heartbeat_expired",
            Self::TurnEnded => "turn_ended",
        }
    }

    /// Add the reason without changing the caller's existing payload keys.
    pub fn payload(self, mut payload: serde_json::Value) -> serde_json::Value {
        payload["reason"] = self.as_str().into();
        payload
    }
}

/// Whether the last started turn stopped at a recorded time limit.
/// Event ids delimit turns, never elapsed time. Legacy exit requests with
/// no reason are not evidence; a later turn start clears earlier limits.
pub fn last_turn_time_limited(events: &[crate::domain::views::RunEvent]) -> bool {
    use crate::domain::EventKind;
    let start = events
        .iter()
        .filter(|e| e.kind == EventKind::TurnStarted)
        .map(|e| e.id)
        .max();
    events
        .iter()
        .filter(|e| start.is_none_or(|start| e.id > start))
        .any(|e| {
            (e.kind == EventKind::ExitRequested
                && matches!(
                    e.payload["reason"].as_str(),
                    Some("resume_timeout" | "revise_deadline")
                ))
                || (e.kind == EventKind::TurnFinished && e.payload["outcome"] == "timed_out")
        })
}

/// The retries of a `/exit` after its timeout, by default.
pub const EXIT_RETRIES: usize = 3;

/// The wait after each retry before the next (or before the retries are
/// used up), by default: 30, 60 and 120 seconds.
pub const EXIT_RETRY_INTERVALS_SECS: [u64; 3] = [30, 60, 120];

/// `[exit]` of `dagq.toml` (ADR-0047 decision 25): the retries of a `/exit`
/// the session held back past its timeout (or that never reached it), and
/// the wait after each one. Since task 1437 it is only parsed and checked:
/// nothing retries a `/exit` any more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitConfig {
    /// The retries after the timeout; 0 retried none and, once they were
    /// used up, did not close the workspace to go on to land.
    pub retries: usize,
    /// The wait after retry `n` is `intervals[n - 1]`; past the list, its
    /// last value.
    pub intervals: Vec<Duration>,
}

impl ExitConfig {
    /// The keys `[exit]` may set.
    pub const KEYS: [&'static str; 2] = ["retries", "retry_intervals_secs"];

    /// The wait after retry `attempt` (1 for the first) before the next
    /// step: the interval of that retry, the last one past the list, the
    /// default's first for an empty list.
    pub fn interval(&self, attempt: usize) -> Duration {
        self.intervals
            .get(attempt.saturating_sub(1))
            .or_else(|| self.intervals.last())
            .copied()
            .unwrap_or(Duration::from_secs(EXIT_RETRY_INTERVALS_SECS[0]))
    }
}

impl Default for ExitConfig {
    fn default() -> Self {
        Self {
            retries: EXIT_RETRIES,
            intervals: EXIT_RETRY_INTERVALS_SECS
                .iter()
                .map(|secs| Duration::from_secs(*secs))
                .collect(),
        }
    }
}

/// `exit_retried`'s `cause`: the session held the `/exit` back past its
/// timeout.
pub const CAUSE_EXIT_TIMEOUT: &str = "exit_timeout";

/// `exit_retried`'s `cause`: cmux timed out on the `/exit` without it
/// reaching the session (`exit_unsent`).
pub const CAUSE_BACKEND_TIMEOUT: &str = "backend_timeout";

/// What a retry read on the session's screen, as `exit_retried`'s `screen`
/// names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitScreen {
    /// The input box is drawn and takes input, with no dialog: `/exit` is
    /// typed again.
    InputReady,
    /// `/exit` is still in the input box: Enter alone is sent again.
    InputPending,
    /// A dialog is up: a known one is answered by its rule (ADR-0047
    /// decision 29), any other gets nothing.
    Dialog,
    /// Neither a ready input box nor a dialog (the session is busy):
    /// nothing is sent.
    NotReady,
    /// The screen could not be read: nothing is sent.
    Unreadable,
}

impl ExitScreen {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InputReady => "input_ready",
            Self::InputPending => "input_pending",
            Self::Dialog => "dialog",
            Self::NotReady => "not_ready",
            Self::Unreadable => "unreadable",
        }
    }

    /// The screen `exit_retried` named `name`.
    pub fn parse(name: &str) -> Option<Self> {
        [
            Self::InputReady,
            Self::InputPending,
            Self::Dialog,
            Self::NotReady,
            Self::Unreadable,
        ]
        .into_iter()
        .find(|screen| screen.as_str() == name)
    }

    /// Classify a screen read: a dialog first (nothing but a known
    /// dialog's keys go over it), then `/exit` left in the input box, then
    /// a ready input box.
    pub const fn of(dialog: bool, pending: bool, ready: bool) -> Self {
        if dialog {
            Self::Dialog
        } else if pending {
            Self::InputPending
        } else if ready {
            Self::InputReady
        } else {
            Self::NotReady
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_reasons_preserve_existing_keys_and_distinguish_causes() {
        for (reason, name) in [
            (ExitReason::ResumeTimeout, "resume_timeout"),
            (ExitReason::ReviseDeadline, "revise_deadline"),
            (ExitReason::IdleUnresolved, "idle_unresolved"),
            (ExitReason::IdleAfterReceipt, "idle_after_receipt"),
            (ExitReason::HeartbeatExpired, "heartbeat_expired"),
            (ExitReason::TurnEnded, "turn_ended"),
        ] {
            for old in [
                serde_json::json!({"workspace_id":"w"}),
                serde_json::json!({"workspace_id":"w", "resume_attempt":2}),
                serde_json::json!({"workspace_id":"w", "timeout_secs":30}),
            ] {
                let mut actual = reason.payload(old.clone());
                assert_eq!(
                    actual.as_object_mut().unwrap().remove("reason"),
                    Some(name.into())
                );
                assert_eq!(actual, old);
            }
        }
    }

    fn event(
        id: i64,
        kind: crate::domain::EventKind,
        payload: serde_json::Value,
    ) -> crate::domain::views::RunEvent {
        crate::domain::views::RunEvent {
            id: crate::domain::EventId::new(id),
            kind: kind.into(),
            payload,
            task_id: None,
            goal_id: None,
            run_id: None,
            actor: None,
            created_at: String::new(),
        }
    }

    #[test]
    fn last_turn_limits_require_explicit_records_and_clear_at_the_next_start() {
        use crate::domain::EventKind::*;
        use serde_json::json;
        for (kind, payload, limited) in [
            (ExitRequested, json!({"reason":"resume_timeout"}), true),
            (ExitRequested, json!({"reason":"revise_deadline"}), true),
            (TurnFinished, json!({"outcome":"timed_out"}), true),
            (ExitRequested, json!({"reason":"idle_unresolved"}), false),
            (ExitRequested, json!({"reason":"idle_after_receipt"}), false),
            (ExitRequested, json!({"reason":"heartbeat_expired"}), false),
            (ExitRequested, json!({"reason":"turn_ended"}), false),
            (ExitRequested, json!({"timeout_secs":3600}), false),
            (ExitRequested, json!({"reason":"unknown"}), false),
            (TurnFinished, json!({"outcome":"stopped"}), false),
        ] {
            let mut events = vec![event(2, kind, payload), event(1, TurnStarted, json!({}))];
            assert_eq!(last_turn_time_limited(&events), limited, "{events:?}");
            // Finishing after the supervisor's exit does not hide its reason.
            events.push(event(3, TurnFinished, json!({"outcome":"stopped"})));
            assert_eq!(last_turn_time_limited(&events), limited);
            events.push(event(4, TurnStarted, json!({})));
            assert!(!last_turn_time_limited(&events));
            events.push(event(5, TurnFinished, json!({"outcome":"completed"})));
            assert!(!last_turn_time_limited(&events));
        }
        assert!(!last_turn_time_limited(&[]));
    }

    #[test]
    fn default_is_three_retries_thirty_sixty_and_one_hundred_twenty_seconds_apart() {
        let config = ExitConfig::default();
        assert_eq!(config.retries, 3);
        assert_eq!(config.interval(1), Duration::from_secs(30));
        assert_eq!(config.interval(2), Duration::from_secs(60));
        assert_eq!(config.interval(3), Duration::from_secs(120));
        // Past the list, its last value.
        assert_eq!(config.interval(4), Duration::from_secs(120));
        let empty = ExitConfig {
            retries: 1,
            intervals: Vec::new(),
        };
        assert_eq!(empty.interval(1), Duration::from_secs(30));
    }

    #[test]
    fn a_dialog_outranks_the_input_box() {
        assert_eq!(ExitScreen::of(true, true, true), ExitScreen::Dialog);
        assert_eq!(ExitScreen::of(false, true, true), ExitScreen::InputPending);
        assert_eq!(ExitScreen::of(false, false, true), ExitScreen::InputReady);
        assert_eq!(ExitScreen::of(false, false, false), ExitScreen::NotReady);
        for (screen, name) in [
            (ExitScreen::InputReady, "input_ready"),
            (ExitScreen::InputPending, "input_pending"),
            (ExitScreen::Dialog, "dialog"),
            (ExitScreen::NotReady, "not_ready"),
            (ExitScreen::Unreadable, "unreadable"),
        ] {
            assert_eq!(screen.as_str(), name);
            assert_eq!(ExitScreen::parse(name), Some(screen));
        }
        assert_eq!(ExitScreen::parse("other"), None);
    }
}
