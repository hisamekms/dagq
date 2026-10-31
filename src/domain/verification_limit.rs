//! The time limit of one verification command a project declares in
//! `dagq.toml` (`[verification] command_timeout_secs`): how long each
//! command of `integrate`'s verification (a task's `--verify` and the
//! `[landing_verification]` command alike), of the landing recheck and of
//! a verification in a container may run before it is stopped and
//! recorded as a `timeout` failure. How long a project's verification
//! takes depends on its language and the shape of its tests, so the limit
//! is the project's; the runtime keeps only [`DEFAULT_COMMAND_TIMEOUT_SECS`],
//! which is the same for every language (ADR-t2268-1). The release of a
//! dead landing waits as long ([`super::landing_release::grace_secs`]).

use std::time::Duration;

/// The table of `dagq.toml` that declares the limit.
///
/// ```toml
/// [verification]
/// command_timeout_secs = 5400
/// ```
pub const TABLE: &str = "verification";

/// The key of [`VerificationConfig::command_timeout_secs`].
pub const COMMAND_TIMEOUT_SECS: &str = "command_timeout_secs";

/// The keys [`TABLE`] takes.
pub const KEYS: [&str; 1] = [COMMAND_TIMEOUT_SECS];

/// The limit without a declaration: an hour, about twice a full run of a
/// large project's tests with room for a loaded host, while a timed-out
/// run holds its slot no longer than that.
pub const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 60 * 60;

/// The largest limit a project may declare: a week. A command running
/// longer holds its slot for good, and the deadline of a larger limit
/// would not fit the clock.
pub const MAX_COMMAND_TIMEOUT_SECS: u64 = 7 * 24 * 60 * 60;

/// `[verification]` of `dagq.toml`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VerificationConfig {
    /// `command_timeout_secs`: the limit of one verification command, in
    /// whole seconds above 0 and at most [`MAX_COMMAND_TIMEOUT_SECS`]
    /// (`0`, a negative value, a fraction or a string is an error of
    /// `dagq.toml`). `None` (no key, no table) leaves the runtime's limit:
    /// [`DEFAULT_COMMAND_TIMEOUT_SECS`], or a shorter one tests set.
    pub command_timeout_secs: Option<u64>,
}

impl VerificationConfig {
    /// The limit of one verification command: the declared one, else
    /// `default`.
    pub fn command_timeout(&self, default: Duration) -> Duration {
        self.command_timeout_secs
            .map_or(default, Duration::from_secs)
    }
}

/// `command_timeout_secs` as written (already a positive whole number),
/// refused past [`MAX_COMMAND_TIMEOUT_SECS`].
pub fn checked_timeout_secs(secs: i64) -> Result<u64, String> {
    match u64::try_from(secs) {
        Ok(secs) if (1..=MAX_COMMAND_TIMEOUT_SECS).contains(&secs) => Ok(secs),
        _ => Err(format!(
            "must be at most {MAX_COMMAND_TIMEOUT_SECS} seconds (a week), not {secs}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without a declaration the runtime's limit holds; a declared one
    /// replaces it, shorter or longer.
    #[test]
    fn the_declared_limit_replaces_the_default() {
        let default = Duration::from_secs(DEFAULT_COMMAND_TIMEOUT_SECS);
        assert_eq!(default, Duration::from_secs(3600));
        assert_eq!(
            VerificationConfig::default().command_timeout(default),
            default
        );
        let declared = VerificationConfig {
            command_timeout_secs: Some(5400),
        };
        assert_eq!(declared.command_timeout(default), Duration::from_secs(5400));
        let short = VerificationConfig {
            command_timeout_secs: Some(1),
        };
        assert_eq!(short.command_timeout(default), Duration::from_secs(1));
    }

    /// A limit past a week is refused, and says what the bound is.
    #[test]
    fn a_limit_past_a_week_is_refused() {
        assert_eq!(checked_timeout_secs(1), Ok(1));
        assert_eq!(
            checked_timeout_secs(MAX_COMMAND_TIMEOUT_SECS as i64),
            Ok(MAX_COMMAND_TIMEOUT_SECS)
        );
        let error = checked_timeout_secs(MAX_COMMAND_TIMEOUT_SECS as i64 + 1).unwrap_err();
        assert_eq!(error, "must be at most 604800 seconds (a week), not 604801");
    }
}
