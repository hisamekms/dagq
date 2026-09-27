//! The detection of a new dagq release on crates.io (ADR-t618-1 decisions
//! 1 to 3): which build looks, the host's `[update]` settings, the reading
//! of the sparse index, when a look is due and how `status` reads the
//! result. The ask and the job that installs a release are not here.

use serde::Serialize;
use serde_json::{Value, json};

use super::RunEvent;
pub use super::event_kind::{RELEASE_CHECK_FAILED, RELEASE_CHECKED};

/// The queue events of a look, newest of either decides when the next is
/// due.
pub const RELEASE_CHECK_KINDS: [&str; 2] = [RELEASE_CHECKED, RELEASE_CHECK_FAILED];

/// dagq's file in crates.io's sparse index (`<first 2>/<next 2>/<name>`).
pub const INDEX_URL: &str = "https://index.crates.io/da/gq/dagq";

/// `check_interval_secs`' default: once a day.
pub const DEFAULT_CHECK_INTERVAL_SECS: u64 = 86_400;

/// `release` of `[update]`: ask a person (the default), install without
/// asking, or do not look.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseMode {
    #[default]
    Ask,
    Auto,
    Off,
}

impl ReleaseMode {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "ask" => Some(Self::Ask),
            "auto" => Some(Self::Auto),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

/// The host's `[update]` (`host.toml`), a value of the wrong shape taken
/// as its default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleaseUpdateConfig {
    pub release: ReleaseMode,
    pub check_interval_secs: u64,
}

impl Default for ReleaseUpdateConfig {
    fn default() -> Self {
        Self {
            release: ReleaseMode::Ask,
            check_interval_secs: DEFAULT_CHECK_INTERVAL_SECS,
        }
    }
}

/// Whether `build` (a build identifier) is a release: `X.Y.Z` with no
/// pre-release and no build metadata. Only a release build looks.
pub fn is_release_build(build: &str) -> bool {
    !build.contains(['-', '+']) && parse_version(build).is_some()
}

/// `X.Y.Z` as numbers; `None` for anything else (a pre-release, build
/// metadata, fewer or more parts).
fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

/// Whether release `latest` is newer than `current` (both `X.Y.Z`).
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// The newest release in the text of the sparse index's file: one JSON
/// object a line with `vers` and `yanked`; the yanked versions and the
/// pre-releases are left out. `Err` when no line is an object with `vers`
/// (the text is not the index); `Ok(None)` when every version is left out.
pub fn latest_release(index: &str) -> Result<Option<String>, String> {
    let mut entries = 0;
    let mut latest: Option<((u64, u64, u64), String)> = None;
    for line in index.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(vers) = entry["vers"].as_str() else {
            continue;
        };
        entries += 1;
        if entry["yanked"].as_bool() == Some(true) {
            continue;
        }
        // Build metadata does not order versions; a pre-release is left out.
        let release = vers.split('+').next().unwrap_or(vers);
        let Some(parsed) = parse_version(release) else {
            continue;
        };
        if latest.as_ref().is_none_or(|(best, _)| parsed > *best) {
            latest = Some((parsed, release.to_owned()));
        }
    }
    if entries == 0 {
        return Err("the index has no version (not a sparse index file)".to_owned());
    }
    Ok(latest.map(|(_, version)| version))
}

/// Whether a look is due at `now`: none recorded (`last`, the queue's
/// newest `release_checked` / `release_check_failed`, by any supervisor),
/// or the last one is `interval_secs` old by its `checked_at` (unix
/// seconds). On a supervisor's first pass a
/// `release_checked` of another build than `current` is looked at again,
/// so a binary just installed learns where it stands.
pub fn check_due(
    last: Option<&RunEvent>,
    now: i64,
    interval_secs: u64,
    first_pass: bool,
    current: &str,
) -> bool {
    let Some(last) = last else {
        return true;
    };
    if first_pass
        && last.kind == RELEASE_CHECKED
        && last.payload["current"].as_str() != Some(current)
    {
        return true;
    }
    // The supervisor's clock at the look (`checked_at`), not the row's.
    let Some(at) = last.payload["checked_at"].as_i64() else {
        return true;
    };
    let interval = i64::try_from(interval_secs).unwrap_or(i64::MAX);
    now.saturating_sub(at) >= interval
}

/// `status`'s `release_update`: the host's `mode`, the `latest` release
/// last read, when the last look was (`checked_at`) and the `state`:
/// `off`, `not_release` (no build that looks: no live supervisor, nor this
/// binary, is a release), `unchecked`, `check_failed` (the newest look
/// failed; `error`), `update_available` or `up_to_date`.
pub fn status(
    config: &ReleaseUpdateConfig,
    release_build: bool,
    last_checked: Option<&RunEvent>,
    last: Option<&RunEvent>,
) -> Value {
    let latest = last_checked.and_then(|event| event.payload["latest"].as_str());
    let current = last_checked.and_then(|event| event.payload["current"].as_str());
    let state = if config.release == ReleaseMode::Off {
        "off"
    } else if !release_build {
        "not_release"
    } else {
        match last {
            None => "unchecked",
            Some(event) if event.kind == RELEASE_CHECK_FAILED => "check_failed",
            Some(_) => match (latest, current) {
                (Some(latest), Some(current)) if is_newer(latest, current) => "update_available",
                _ => "up_to_date",
            },
        }
    };
    let mut value = json!({
        "mode": config.release,
        "latest": latest,
        "current": current,
        "checked_at": last.and_then(|event| event.payload["checked_at"].as_i64()),
        "state": state,
    });
    if let Some(event) = last.filter(|event| event.kind == RELEASE_CHECK_FAILED) {
        value["error"] = event.payload["error"].clone();
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;

    fn event(kind: &str, at: i64, mut payload: Value) -> RunEvent {
        payload["checked_at"] = json!(at);
        RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    #[test]
    fn only_a_plain_version_is_a_release_build() {
        assert!(is_release_build("0.4.0"));
        assert!(!is_release_build("0.4.0-dev+abc"));
        assert!(!is_release_build("0.4.0-dev+unknown"));
        assert!(!is_release_build("0.4.0+abc"));
        assert!(!is_release_build("0.4"));
        assert!(!is_release_build("0.4.0.1"));
    }

    #[test]
    fn the_latest_release_skips_yanked_and_pre_releases() {
        let index = [
            r#"{"name":"dagq","vers":"0.2.0","yanked":false}"#,
            r#"{"name":"dagq","vers":"0.10.0","yanked":true}"#,
            r#"{"name":"dagq","vers":"0.3.0","yanked":false}"#,
            r#"{"name":"dagq","vers":"1.0.0-rc.1","yanked":false}"#,
            "not json",
            r#"{"name":"dagq","vers":"0.3.1+build","yanked":false}"#,
        ]
        .join("\n");
        assert_eq!(latest_release(&index).unwrap().as_deref(), Some("0.3.1"));
    }

    #[test]
    fn an_index_of_no_version_is_an_error_and_all_yanked_is_none() {
        assert!(latest_release("<html>").is_err());
        assert!(latest_release("").is_err());
        assert_eq!(
            latest_release(r#"{"vers":"0.1.0","yanked":true}"#).unwrap(),
            None
        );
    }

    #[test]
    fn newer_compares_numbers_not_text() {
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.9.0", "0.10.0"));
        assert!(!is_newer("0.3.0", "0.3.0"));
        assert!(!is_newer("x", "0.3.0"));
    }

    #[test]
    fn a_look_is_due_without_one_or_after_the_interval() {
        assert!(check_due(None, 100, 50, false, "0.3.0"));
        let last = event(RELEASE_CHECKED, 100, json!({"current": "0.3.0"}));
        assert!(!check_due(Some(&last), 149, 50, false, "0.3.0"));
        assert!(!check_due(Some(&last), 149, 50, true, "0.3.0"));
        assert!(check_due(Some(&last), 150, 50, false, "0.3.0"));
        // A binary just installed looks again on its first pass only.
        assert!(check_due(Some(&last), 101, 50, true, "0.4.0"));
        assert!(!check_due(Some(&last), 101, 50, false, "0.4.0"));
        let failed = event(RELEASE_CHECK_FAILED, 100, json!({"error": "x"}));
        assert!(!check_due(Some(&failed), 101, 50, true, "0.4.0"));
    }

    #[test]
    fn the_status_reads_the_state_of_the_last_look() {
        let config = ReleaseUpdateConfig::default();
        let checked = event(
            RELEASE_CHECKED,
            10,
            json!({"latest": "0.4.0", "current": "0.3.0"}),
        );
        let failed = event(RELEASE_CHECK_FAILED, 20, json!({"error": "timeout"}));
        assert_eq!(status(&config, true, None, None)["state"], "unchecked");
        let available = status(&config, true, Some(&checked), Some(&checked));
        assert_eq!(available["state"], "update_available");
        assert_eq!(available["mode"], "ask");
        assert_eq!(available["latest"], "0.4.0");
        assert_eq!(available["checked_at"], 10);
        let failing = status(&config, true, Some(&checked), Some(&failed));
        assert_eq!(failing["state"], "check_failed");
        assert_eq!(failing["error"], "timeout");
        assert_eq!(failing["latest"], "0.4.0");
        assert_eq!(failing["checked_at"], 20);
        let same = event(
            RELEASE_CHECKED,
            10,
            json!({"latest": "0.3.0", "current": "0.3.0"}),
        );
        assert_eq!(
            status(&config, true, Some(&same), Some(&same))["state"],
            "up_to_date"
        );
        assert_eq!(status(&config, false, None, None)["state"], "not_release");
        let off = ReleaseUpdateConfig {
            release: ReleaseMode::Off,
            ..config
        };
        assert_eq!(status(&off, true, None, None)["state"], "off");
        assert_eq!(status(&off, true, None, None)["mode"], "off");
    }

    #[test]
    fn the_modes_parse_by_name() {
        assert_eq!(ReleaseMode::parse("ask"), Some(ReleaseMode::Ask));
        assert_eq!(ReleaseMode::parse("auto"), Some(ReleaseMode::Auto));
        assert_eq!(ReleaseMode::parse("off"), Some(ReleaseMode::Off));
        assert_eq!(ReleaseMode::parse("never"), None);
    }
}
