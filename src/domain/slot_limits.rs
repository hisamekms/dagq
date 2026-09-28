//! How many runs a supervisor executes at once (`parallel`), keeps
//! waiting for a person outside its slots (`max_waiting`, ADR-0062
//! decision 7) and how many planners its runtime opens at once
//! (`runtime_planners`, ADR-0041 decision 12, task 941), and where each
//! value comes from (task 698): the flag (`supervise --parallel` /
//! `--max-waiting` / `--runtime-planners`), else `[supervisor]` of the
//! main checkout's `dagq.toml`, else the default.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::waiting::DEFAULT_MAX_WAITING;

/// The default of `parallel`.
pub const DEFAULT_PARALLEL: usize = 4;

/// The default of `runtime_planners`.
pub const DEFAULT_RUNTIME_PLANNERS: usize = 1;

/// The queue event recorded when a supervisor starts using values of
/// `[supervisor]` it read again that differ from those it used.
pub const SUPERVISOR_CONFIG_CHANGED: &str =
    crate::domain::event_kind::EventKind::SupervisorConfigChanged.as_str();

/// Where a value in use comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SettingSource {
    /// Given on the command line.
    #[serde(rename = "flag")]
    Flag,
    /// `[supervisor]` of the main checkout's `dagq.toml`.
    #[serde(rename = "dagq.toml")]
    File,
    /// Neither: the default.
    #[serde(rename = "default")]
    Default,
}

impl SettingSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::File => "dagq.toml",
            Self::Default => "default",
        }
    }

    /// The source a registration records, `None` for an unknown text.
    pub fn parse(text: &str) -> Option<Self> {
        [Self::Flag, Self::File, Self::Default]
            .into_iter()
            .find(|source| source.as_str() == text)
    }
}

/// `[supervisor]` of `dagq.toml`: each key it sets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SupervisorConfig {
    /// At least 1.
    pub parallel: Option<usize>,
    /// 0 keeps every run in its slot.
    pub max_waiting: Option<usize>,
    /// At least 1.
    pub runtime_planners: Option<usize>,
}

impl SupervisorConfig {
    /// The keys `[supervisor]` may set.
    pub const KEYS: [&'static str; 3] = ["parallel", "max_waiting", "runtime_planners"];
}

/// The flags given to `supervise`; `None` is not given.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SlotFlags {
    pub parallel: Option<usize>,
    pub max_waiting: Option<usize>,
    pub runtime_planners: Option<usize>,
}

impl SlotFlags {
    /// All given: the file is never read for them.
    pub const fn complete(self) -> bool {
        self.parallel.is_some() && self.max_waiting.is_some() && self.runtime_planners.is_some()
    }
}

/// One value in use and where it comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Setting {
    pub value: usize,
    pub source: SettingSource,
}

impl Setting {
    /// The flag over the file over the default.
    pub fn resolve(flag: Option<usize>, file: Option<usize>, default: usize) -> Self {
        match (flag, file) {
            (Some(value), _) => Self {
                value,
                source: SettingSource::Flag,
            },
            (None, Some(value)) => Self {
                value,
                source: SettingSource::File,
            },
            (None, None) => Self {
                value: default,
                source: SettingSource::Default,
            },
        }
    }
}

/// `parallel`, `max_waiting` and `runtime_planners` in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SlotLimits {
    pub parallel: Setting,
    pub max_waiting: Setting,
    pub runtime_planners: Setting,
}

impl SlotLimits {
    /// The flags over `[supervisor]` (none for no file or no table) over
    /// the defaults.
    pub fn resolve(flags: SlotFlags, file: SupervisorConfig) -> Self {
        Self {
            parallel: Setting::resolve(flags.parallel, file.parallel, DEFAULT_PARALLEL),
            max_waiting: Setting::resolve(flags.max_waiting, file.max_waiting, DEFAULT_MAX_WAITING),
            runtime_planners: Setting::resolve(
                flags.runtime_planners,
                file.runtime_planners,
                DEFAULT_RUNTIME_PLANNERS,
            ),
        }
    }

    fn json(self) -> Value {
        json!({
            "parallel": self.parallel.value,
            "parallel_source": self.parallel.source,
            "max_waiting": self.max_waiting.value,
            "max_waiting_source": self.max_waiting.source,
            "runtime_planners": self.runtime_planners.value,
            "runtime_planners_source": self.runtime_planners.source,
        })
    }
}

/// The payload of [`SUPERVISOR_CONFIG_CHANGED`] for a change from `from`
/// to `to`, `None` when nothing changed.
pub fn slot_limits_change(from: SlotLimits, to: SlotLimits) -> Option<Value> {
    (from != to).then(|| json!({"from": from.json(), "to": to.json()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_wins_over_the_file_over_the_default() {
        let file = SupervisorConfig {
            parallel: Some(3),
            max_waiting: Some(0),
            runtime_planners: Some(2),
        };
        let all = SlotFlags {
            parallel: Some(2),
            max_waiting: Some(1),
            runtime_planners: Some(3),
        };
        let limits = SlotLimits::resolve(all, file);
        assert_eq!(limits.parallel.value, 2);
        assert_eq!(limits.parallel.source, SettingSource::Flag);
        assert_eq!(limits.max_waiting.value, 1);
        assert_eq!(
            (
                limits.runtime_planners.value,
                limits.runtime_planners.source
            ),
            (3, SettingSource::Flag)
        );
        assert!(all.complete());
        assert!(
            !SlotFlags {
                runtime_planners: None,
                ..all
            }
            .complete()
        );

        let limits = SlotLimits::resolve(SlotFlags::default(), file);
        assert_eq!(
            (limits.parallel.value, limits.parallel.source),
            (3, SettingSource::File)
        );
        assert_eq!(
            (limits.max_waiting.value, limits.max_waiting.source),
            (0, SettingSource::File)
        );
        assert_eq!(
            (
                limits.runtime_planners.value,
                limits.runtime_planners.source
            ),
            (2, SettingSource::File)
        );

        let limits = SlotLimits::resolve(SlotFlags::default(), SupervisorConfig::default());
        assert_eq!(
            (limits.parallel.value, limits.parallel.source),
            (DEFAULT_PARALLEL, SettingSource::Default)
        );
        assert_eq!(
            (limits.max_waiting.value, limits.max_waiting.source),
            (DEFAULT_MAX_WAITING, SettingSource::Default)
        );
        assert_eq!(
            (
                limits.runtime_planners.value,
                limits.runtime_planners.source
            ),
            (DEFAULT_RUNTIME_PLANNERS, SettingSource::Default)
        );
        assert!(!SlotFlags::default().complete());
    }

    #[test]
    fn sources_round_trip_and_changes_are_reported() {
        for source in [
            SettingSource::Flag,
            SettingSource::File,
            SettingSource::Default,
        ] {
            assert_eq!(SettingSource::parse(source.as_str()), Some(source));
            assert_eq!(
                serde_json::to_value(source).unwrap(),
                json!(source.as_str())
            );
        }
        assert_eq!(SettingSource::parse("elsewhere"), None);
        let from = SlotLimits::resolve(SlotFlags::default(), SupervisorConfig::default());
        assert_eq!(slot_limits_change(from, from), None);
        let to = SlotLimits::resolve(
            SlotFlags::default(),
            SupervisorConfig {
                parallel: Some(2),
                max_waiting: None,
                runtime_planners: Some(2),
            },
        );
        let payload = slot_limits_change(from, to).unwrap();
        assert_eq!(payload["from"]["parallel"], json!(4));
        assert_eq!(payload["to"]["parallel"], json!(2));
        assert_eq!(payload["to"]["parallel_source"], json!("dagq.toml"));
        assert_eq!(payload["to"]["max_waiting_source"], json!("default"));
        assert_eq!(payload["from"]["runtime_planners"], json!(1));
        assert_eq!(payload["to"]["runtime_planners"], json!(2));
        assert_eq!(payload["to"]["runtime_planners_source"], json!("dagq.toml"));
    }
}

#[cfg(test)]
mod registration_tests {
    use super::SettingSource;
    use crate::domain::{LeaseToken, SupervisorRegistration};

    #[test]
    fn only_values_from_flags_are_passed_to_a_restart() {
        let registration =
            |parallel_source, max_waiting, max_waiting_source| SupervisorRegistration {
                token: LeaseToken::new("t"),
                pid: 1,
                parallel: 3,
                started_at: 0,
                heartbeat_at: 0,
                mode: None,
                workspace_id: None,
                binary_version: None,
                handoff_accepted: false,
                handoff_binary: None,
                auto_update: false,
                max_waiting,
                parallel_source,
                max_waiting_source,
                runtime_planners: None,
                runtime_planners_source: None,
                providers: None,
            };
        assert_eq!(
            registration(
                Some(SettingSource::Flag),
                Some(2),
                Some(SettingSource::Flag)
            )
            .flag_arguments(),
            ["--parallel", "3", "--max-waiting", "2"]
        );
        // An older binary's registration: its values were given.
        assert_eq!(
            registration(None, Some(4), None).flag_arguments(),
            ["--parallel", "3", "--max-waiting", "4"]
        );
        assert_eq!(
            registration(None, None, None).flag_arguments(),
            ["--parallel", "3"]
        );
        assert!(
            registration(
                Some(SettingSource::File),
                Some(2),
                Some(SettingSource::Default)
            )
            .flag_arguments()
            .is_empty()
        );
        let with_planners = |runtime_planners_source| SupervisorRegistration {
            runtime_planners: Some(2),
            runtime_planners_source,
            ..registration(
                Some(SettingSource::File),
                Some(4),
                Some(SettingSource::Default),
            )
        };
        assert_eq!(
            with_planners(Some(SettingSource::Flag)).flag_arguments(),
            ["--runtime-planners", "2"]
        );
        assert!(
            with_planners(Some(SettingSource::File))
                .flag_arguments()
                .is_empty()
        );
        assert!(
            with_planners(Some(SettingSource::Default))
                .flag_arguments()
                .is_empty()
        );
    }
}
