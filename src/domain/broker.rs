//! The resource broker's settings and its attention ([Broker] mode and
//! settings, ADR-t827-4 decision 4): the repository's policy is `[broker]`
//! of `dagq.toml` ([`BrokerConfig`]: the mode, what `process.exec` may run
//! and the server's limits), the host's circumstances are `[broker]` of
//! `host.toml` ([`HostBroker`]: podman, the machine's and the container's
//! resources, the port). `host.toml` can lower the mode to `disabled` and
//! never raise it ([`resolve_mode`]).
//!
//! The supervisor's broker events are queue events: a broker that failed
//! to start or stays unhealthy is the attention `broker_unhealthy` for the
//! inbox until the broker runs again ([`attention_stands`]).
//!
//! [Broker]: ../../docs/design/broker.md

use serde::Serialize;

/// Whether and how runs use the broker (ADR-t827-4 decision 3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrokerMode {
    /// No broker: nothing calls podman (the default, and a queue without
    /// `[broker]`).
    #[default]
    Disabled,
    /// The supervisor keeps a broker; a worker may use it, and runs go on
    /// without it.
    Preferred,
    /// Refused until Phase 2 enforces it: never silently `preferred`.
    Required,
}

impl BrokerMode {
    pub const ALL: [Self; 3] = [Self::Disabled, Self::Preferred, Self::Required];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Preferred => "preferred",
            Self::Required => "required",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.as_str() == text)
    }

    /// Why a mode cannot run now: `required` is Phase 2's.
    pub fn unsupported(self) -> Option<String> {
        (self == Self::Required).then(|| {
            "[broker] mode = \"required\" of dagq.toml is not implemented yet (Phase 2 enforces it); \
use \"preferred\" or \"disabled\""
                .to_owned()
        })
    }
}

/// The defaults of the server's limits, as `dagq-broker serve` has them.
pub const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 60;
pub const DEFAULT_EXEC_MAX_TIMEOUT_SECS: u64 = 300;
pub const DEFAULT_OUTPUT_LIMIT_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_FS_LIMIT_BYTES: u64 = 4 * 1024 * 1024;

/// `[broker]` of `dagq.toml`: the repository's policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrokerConfig {
    pub mode: BrokerMode,
    /// The basenames `process.exec` may run; empty refuses every exec.
    pub exec_allow: Vec<String>,
    /// The env names a request may pass to its process.
    pub exec_env: Vec<String>,
    pub exec_timeout_secs: u64,
    pub exec_max_timeout_secs: u64,
    pub output_limit_bytes: u64,
    pub fs_limit_bytes: u64,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            mode: BrokerMode::Disabled,
            exec_allow: Vec::new(),
            exec_env: Vec::new(),
            exec_timeout_secs: DEFAULT_EXEC_TIMEOUT_SECS,
            exec_max_timeout_secs: DEFAULT_EXEC_MAX_TIMEOUT_SECS,
            output_limit_bytes: DEFAULT_OUTPUT_LIMIT_BYTES,
            fs_limit_bytes: DEFAULT_FS_LIMIT_BYTES,
        }
    }
}

impl BrokerConfig {
    /// The keys of the table.
    pub const KEYS: [&'static str; 7] = [
        "mode",
        "exec_allow",
        "exec_env",
        "exec_timeout_secs",
        "exec_max_timeout_secs",
        "output_limit_bytes",
        "fs_limit_bytes",
    ];

    /// Whether the limits hold together: the default timeout within the
    /// maximum, as `dagq-broker serve` requires.
    pub fn check(&self) -> Result<(), String> {
        if self.exec_timeout_secs > self.exec_max_timeout_secs {
            return Err(format!(
                "exec_timeout_secs ({}) is above exec_max_timeout_secs ({})",
                self.exec_timeout_secs, self.exec_max_timeout_secs
            ));
        }
        Ok(())
    }

    /// The flags of `dagq-broker serve` that differ from its defaults.
    pub fn serve_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        let mut number = |flag: &str, value: u64, default: u64| {
            if value != default {
                args.extend([flag.to_owned(), value.to_string()]);
            }
        };
        number(
            "--exec-timeout-secs",
            self.exec_timeout_secs,
            DEFAULT_EXEC_TIMEOUT_SECS,
        );
        number(
            "--exec-max-timeout-secs",
            self.exec_max_timeout_secs,
            DEFAULT_EXEC_MAX_TIMEOUT_SECS,
        );
        number(
            "--output-limit-bytes",
            self.output_limit_bytes,
            DEFAULT_OUTPUT_LIMIT_BYTES,
        );
        number(
            "--fs-limit-bytes",
            self.fs_limit_bytes,
            DEFAULT_FS_LIMIT_BYTES,
        );
        for name in &self.exec_allow {
            args.extend(["--exec-allow".to_owned(), name.clone()]);
        }
        for name in &self.exec_env {
            args.extend(["--exec-env".to_owned(), name.clone()]);
        }
        args
    }
}

/// `[broker]` of `host.toml`: the host's circumstances. Every value is an
/// override; `None` keeps the default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HostBroker {
    /// `mode = "disabled"`: this host does not run the broker. The only
    /// mode the host can set; it cannot raise the repository's.
    pub disabled: bool,
    /// The podman executable; `None` is `podman` on `PATH`.
    pub podman: Option<String>,
    pub machine_cpus: Option<u32>,
    pub machine_memory_mib: Option<u32>,
    pub machine_disk_gib: Option<u32>,
    pub container_memory: Option<String>,
    pub container_cpus: Option<String>,
    pub container_pids: Option<u32>,
    /// The port on `127.0.0.1`; `Some(0)` or `None` picks a free one (the
    /// one used before, when there is one).
    pub port: Option<u16>,
}

impl HostBroker {
    pub const KEYS: [&'static str; 9] = [
        "mode",
        "podman",
        "machine_cpus",
        "machine_memory_mib",
        "machine_disk_gib",
        "container_memory",
        "container_cpus",
        "container_pids",
        "port",
    ];

    /// The port asked for, `0` being none.
    pub fn fixed_port(&self) -> Option<u16> {
        self.port.filter(|port| *port != 0)
    }
}

/// The mode in force: the repository's, lowered to `disabled` when the
/// host says so. The host never raises it.
pub fn resolve_mode(repository: BrokerMode, host: &HostBroker) -> BrokerMode {
    if host.disabled {
        BrokerMode::Disabled
    } else {
        repository
    }
}

/// The broker could not start, or stays unhealthy after its restart
/// (`reason`: a failure's code such as `machine_busy`, or `unhealthy`):
/// the inbox's attention, next `dagq broker status`.
pub const BROKER_UNHEALTHY: &str = crate::domain::event_kind::EventKind::BrokerUnhealthy.as_str();
/// The health answered again after a [`BROKER_UNHEALTHY`].
pub const BROKER_HEALTHY: &str = crate::domain::event_kind::EventKind::BrokerHealthy.as_str();
/// The supervisor made the broker run (`port`, `build`, `image`).
pub const BROKER_STARTED: &str = crate::domain::event_kind::EventKind::BrokerStarted.as_str();
/// `down`, or a supervisor at the end of the drain `down` asked for,
/// stopped the broker (`container_stopped`, `machine_stopped`, `by`).
pub const BROKER_STOPPED: &str = crate::domain::event_kind::EventKind::BrokerStopped.as_str();
/// `down` asked the supervisors it signalled (`supervisors`, their tokens)
/// to stop the broker once their drain ends; `up`'s replacement and an exec
/// do not ask, so the broker keeps running through them.
pub const BROKER_STOP_REQUESTED: &str =
    crate::domain::event_kind::EventKind::BrokerStopRequested.as_str();
/// The supervisor built the broker's image (`build`, `duration_ms`).
pub const BROKER_IMAGE_BUILT: &str =
    crate::domain::event_kind::EventKind::BrokerImageBuilt.as_str();
/// The `repair` of the `auto_repaired` of a container restarted after its
/// health failed three times in a row (ADR-t827-3 decision 3).
pub const BROKER_RESTART: &str = "broker_restart";
/// The kinds whose latest says whether the attention stands.
pub const BROKER_ATTENTION_KINDS: [&str; 4] = [
    BROKER_UNHEALTHY,
    BROKER_HEALTHY,
    BROKER_STARTED,
    BROKER_STOPPED,
];

/// Whether the attention stands: the latest of
/// [`BROKER_ATTENTION_KINDS`] is [`BROKER_UNHEALTHY`].
pub fn attention_stands(latest: Option<&str>) -> bool {
    latest == Some(BROKER_UNHEALTHY)
}

/// The broker's health as `status` and `doctor` show it, from the latest
/// of [`BROKER_ATTENTION_KINDS`] (`kind` and `payload`, recorded at `at`):
/// `healthy` after a start or a health that answered again, `unhealthy`
/// with the `reason`, `stopped`, or `unknown` when no supervisor recorded
/// any. It reads the records only: no podman, no request to the broker.
pub fn health_report(latest: Option<(&str, &serde_json::Value, &str)>) -> serde_json::Value {
    let Some((kind, payload, at)) = latest else {
        return serde_json::json!({"state": "unknown", "reason": null, "at": null});
    };
    let state = match kind {
        BROKER_STARTED | BROKER_HEALTHY => "healthy",
        BROKER_UNHEALTHY => "unhealthy",
        BROKER_STOPPED => "stopped",
        _ => "unknown",
    };
    serde_json::json!({
        "state": state,
        "reason": (kind == BROKER_UNHEALTHY).then(|| payload["reason"].clone()),
        "at": at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_health_reads_the_latest_broker_event() {
        use serde_json::json;
        assert_eq!(health_report(None)["state"], "unknown");
        let started = health_report(Some((BROKER_STARTED, &json!({"port": 1}), "t")));
        assert_eq!(
            started,
            json!({"state": "healthy", "reason": null, "at": "t"})
        );
        assert_eq!(
            health_report(Some((BROKER_HEALTHY, &json!({}), "t")))["state"],
            "healthy"
        );
        let unhealthy = health_report(Some((
            BROKER_UNHEALTHY,
            &json!({"reason": "machine_busy"}),
            "t",
        )));
        assert_eq!(
            unhealthy,
            json!({"state": "unhealthy", "reason": "machine_busy", "at": "t"})
        );
        assert_eq!(
            health_report(Some((BROKER_STOPPED, &json!({}), "t")))["state"],
            "stopped"
        );
        assert_eq!(
            health_report(Some(("other", &json!({}), "t")))["state"],
            "unknown"
        );
    }

    #[test]
    fn the_host_lowers_the_mode_and_never_raises_it() {
        let lowered = HostBroker {
            disabled: true,
            ..HostBroker::default()
        };
        let silent = HostBroker::default();
        for mode in BrokerMode::ALL {
            assert_eq!(resolve_mode(mode, &lowered), BrokerMode::Disabled);
            assert_eq!(resolve_mode(mode, &silent), mode);
        }
        assert_eq!(BrokerMode::default(), BrokerMode::Disabled);
        assert_eq!(BrokerMode::parse("preferred"), Some(BrokerMode::Preferred));
        assert_eq!(BrokerMode::parse("on"), None);
        assert!(BrokerMode::Required.unsupported().is_some());
        assert!(BrokerMode::Preferred.unsupported().is_none());
    }

    #[test]
    fn serve_gets_only_the_limits_that_differ_from_its_defaults() {
        assert!(BrokerConfig::default().serve_args().is_empty());
        let config = BrokerConfig {
            exec_allow: vec!["ls".into(), "cat".into()],
            exec_env: vec!["LANG".into()],
            exec_timeout_secs: 30,
            fs_limit_bytes: 1024,
            ..BrokerConfig::default()
        };
        assert_eq!(
            config.serve_args(),
            [
                "--exec-timeout-secs",
                "30",
                "--fs-limit-bytes",
                "1024",
                "--exec-allow",
                "ls",
                "--exec-allow",
                "cat",
                "--exec-env",
                "LANG"
            ]
        );
        assert!(config.check().is_ok());
        let wrong = BrokerConfig {
            exec_timeout_secs: 301,
            ..BrokerConfig::default()
        };
        assert!(wrong.check().unwrap_err().contains("exec_max_timeout_secs"));
    }

    #[test]
    fn the_attention_stands_until_the_broker_runs_again() {
        assert!(attention_stands(Some(BROKER_UNHEALTHY)));
        for latest in [
            None,
            Some(BROKER_HEALTHY),
            Some(BROKER_STARTED),
            Some(BROKER_STOPPED),
        ] {
            assert!(!attention_stands(latest));
        }
        assert_eq!(
            HostBroker {
                port: Some(0),
                ..HostBroker::default()
            }
            .fixed_port(),
            None
        );
        assert_eq!(
            HostBroker {
                port: Some(9000),
                ..HostBroker::default()
            }
            .fixed_port(),
            Some(9000)
        );
    }
}
