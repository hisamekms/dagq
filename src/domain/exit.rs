//! The retries of a `/exit` a session held back (ADR-0047 decision 25):
//! how many and how far apart, as `[exit]` of `dagq.toml` sets them.

use std::time::Duration;

/// The retries of a `/exit` after its timeout, by default.
pub const EXIT_RETRIES: usize = 3;

/// The wait after each retry before the next (or before the retries are
/// used up), by default: 30, 60 and 120 seconds.
pub const EXIT_RETRY_INTERVALS_SECS: [u64; 3] = [30, 60, 120];

/// `[exit]` of `dagq.toml` (ADR-0047 decision 25): the retries of a `/exit`
/// the session held back past its timeout (or that never reached it), and
/// the wait after each one. `retries = 0` makes none: the session's
/// `stuck_exit` path follows the timeout at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitConfig {
    /// The retries after the timeout.
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
