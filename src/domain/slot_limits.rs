//! How many runs a supervisor executes at once (`parallel`), keeps
//! waiting for a person outside its slots (`max_waiting`, ADR-0062
//! decision 7) and how many planners its runtime opens at once
//! (`runtime_planners`, ADR-0041 decision 12, task 941), how long it
//! spaces new claims while the load hold is on (`claim_spacing`,
//! ADR-t1479-1), and where each value comes from (task 698): the flag (`supervise --parallel` /
//! `--max-waiting` / `--runtime-planners`), else `[supervisor]` of the
//! main checkout's `dagq.toml`, else the default.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::claim_spacing::DEFAULT_CLAIM_SPACING_SECS;
use super::light_slots::LightChanges;
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupervisorConfig {
    /// At least 1.
    pub parallel: Option<usize>,
    /// 0 keeps every run in its slot.
    pub max_waiting: Option<usize>,
    /// At least 1.
    pub runtime_planners: Option<usize>,
    /// Seconds; 0 spaces no claim.
    pub claim_spacing: Option<usize>,
    /// The changes claimed in the room the landing queue leaves
    /// (ADR-t1591-1); none claims nothing there. It has no flag.
    pub light_changes: Option<LightChanges>,
}

impl SupervisorConfig {
    /// The keys `[supervisor]` may set.
    pub const KEYS: [&'static str; 5] = [
        "parallel",
        "max_waiting",
        "runtime_planners",
        "claim_spacing",
        "light_changes",
    ];

    /// `light_changes`, empty when not set.
    pub fn light_changes(&self) -> LightChanges {
        self.light_changes.clone().unwrap_or_default()
    }
}

/// The flags given to `supervise`; `None` is not given. `claim_spacing`
/// has no flag of the CLI: only a caller of the library gives it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SlotFlags {
    pub parallel: Option<usize>,
    pub max_waiting: Option<usize>,
    pub runtime_planners: Option<usize>,
    pub claim_spacing: Option<usize>,
}

impl SlotFlags {
    /// All given: the file is never read for them.
    pub const fn complete(self) -> bool {
        self.parallel.is_some()
            && self.max_waiting.is_some()
            && self.runtime_planners.is_some()
            && self.claim_spacing.is_some()
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

/// `parallel`, `max_waiting`, `runtime_planners` and `claim_spacing` (in
/// seconds) in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SlotLimits {
    pub parallel: Setting,
    pub max_waiting: Setting,
    pub runtime_planners: Setting,
    pub claim_spacing: Setting,
}

impl SlotLimits {
    /// The flags over `[supervisor]` (none for no file or no table) over
    /// the defaults.
    pub fn resolve(flags: SlotFlags, file: &SupervisorConfig) -> Self {
        Self {
            parallel: Setting::resolve(flags.parallel, file.parallel, DEFAULT_PARALLEL),
            max_waiting: Setting::resolve(flags.max_waiting, file.max_waiting, DEFAULT_MAX_WAITING),
            runtime_planners: Setting::resolve(
                flags.runtime_planners,
                file.runtime_planners,
                DEFAULT_RUNTIME_PLANNERS,
            ),
            claim_spacing: Setting::resolve(
                flags.claim_spacing,
                file.claim_spacing,
                DEFAULT_CLAIM_SPACING_SECS,
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
            "claim_spacing": self.claim_spacing.value,
            "claim_spacing_source": self.claim_spacing.source,
        })
    }
}

/// The payload of [`SUPERVISOR_CONFIG_CHANGED`] for a change from `from`
/// to `to`, `None` when nothing changed: the slot limits and
/// `light_changes` (ADR-t1591-1), by the names of the changes.
pub fn supervisor_config_change(
    from: (SlotLimits, &LightChanges),
    to: (SlotLimits, &LightChanges),
) -> Option<Value> {
    let names = |light: &LightChanges| -> Vec<String> {
        light
            .values()
            .iter()
            .map(|c| c.as_str().to_owned())
            .collect()
    };
    (from.0 != to.0 || from.1 != to.1).then(|| {
        let mut from_json = from.0.json();
        from_json["light_changes"] = json!(names(from.1));
        let mut to_json = to.0.json();
        to_json["light_changes"] = json!(names(to.1));
        json!({"from": from_json, "to": to_json})
    })
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
            claim_spacing: Some(60),
            light_changes: None,
        };
        let all = SlotFlags {
            parallel: Some(2),
            max_waiting: Some(1),
            runtime_planners: Some(3),
            claim_spacing: Some(0),
        };
        let limits = SlotLimits::resolve(all, &file);
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
        assert_eq!(
            (limits.claim_spacing.value, limits.claim_spacing.source),
            (0, SettingSource::Flag)
        );
        assert!(all.complete());
        assert!(
            !SlotFlags {
                runtime_planners: None,
                ..all
            }
            .complete()
        );
        // The CLI never gives `claim_spacing`: the file is read for it.
        assert!(
            !SlotFlags {
                claim_spacing: None,
                ..all
            }
            .complete()
        );

        let limits = SlotLimits::resolve(SlotFlags::default(), &file);
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
        assert_eq!(
            (limits.claim_spacing.value, limits.claim_spacing.source),
            (60, SettingSource::File)
        );

        let limits = SlotLimits::resolve(SlotFlags::default(), &SupervisorConfig::default());
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
        assert_eq!(
            (limits.claim_spacing.value, limits.claim_spacing.source),
            (180, SettingSource::Default)
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
        let from = SlotLimits::resolve(SlotFlags::default(), &SupervisorConfig::default());
        let none = LightChanges::default();
        assert_eq!(supervisor_config_change((from, &none), (from, &none)), None);
        let to = SlotLimits::resolve(
            SlotFlags::default(),
            &SupervisorConfig {
                parallel: Some(2),
                max_waiting: None,
                runtime_planners: Some(2),
                claim_spacing: Some(0),
                light_changes: None,
            },
        );
        let payload = supervisor_config_change((from, &none), (to, &none)).unwrap();
        assert_eq!(payload["from"]["parallel"], json!(4));
        assert_eq!(payload["to"]["parallel"], json!(2));
        assert_eq!(payload["to"]["parallel_source"], json!("dagq.toml"));
        assert_eq!(payload["to"]["max_waiting_source"], json!("default"));
        assert_eq!(payload["from"]["runtime_planners"], json!(1));
        assert_eq!(payload["to"]["runtime_planners"], json!(2));
        assert_eq!(payload["to"]["runtime_planners_source"], json!("dagq.toml"));
        assert_eq!(payload["from"]["claim_spacing"], json!(180));
        assert_eq!(payload["from"]["claim_spacing_source"], json!("default"));
        assert_eq!(payload["to"]["claim_spacing"], json!(0));
        assert_eq!(payload["to"]["claim_spacing_source"], json!("dagq.toml"));
        assert_eq!(payload["to"]["light_changes"], json!([]));
        // Only `light_changes` changed: reported all the same.
        let docs = LightChanges::new(
            vec!["docs".parse().unwrap()],
            Some(&crate::domain::change::ChangeSet::new(vec!["docs".parse().unwrap()]).unwrap()),
        )
        .unwrap();
        let payload = supervisor_config_change((from, &none), (from, &docs)).unwrap();
        assert_eq!(payload["from"]["light_changes"], json!([]));
        assert_eq!(payload["to"]["light_changes"], json!(["docs"]));
        assert_eq!(payload["to"]["parallel"], json!(4));
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
                claim_spacing: None,
                claim_spacing_source: None,
                max_load: None,
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
