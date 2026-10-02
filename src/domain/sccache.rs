//! The sccache server of a `[run.env]` whose `RUSTC_WRAPPER` is sccache
//! (ADR-t1215-1): the supervisor, outside any sandbox, starts and keeps
//! it; a process the runtime runs in a sandbox (a Codex worker's turn, a
//! Codex job given `[run.env]`) must not, so a sandboxed turn whose server
//! could not be confirmed just before it starts runs without
//! `RUSTC_WRAPPER`.

use std::path::Path;

/// The variable that names cargo's wrapper of rustc.
pub const WRAPPER_VAR: &str = "RUSTC_WRAPPER";
/// sccache's server port variable, and its default.
pub const PORT_VAR: &str = "SCCACHE_SERVER_PORT";
pub const DEFAULT_PORT: u16 = 4226;
/// The idle timeout the supervisor starts the server with: none, so that it
/// never stops on idle and is next started by a process in a sandbox.
pub const IDLE_TIMEOUT_VAR: &str = "SCCACHE_IDLE_TIMEOUT";
pub const IDLE_TIMEOUT: &str = "0";

pub const SCCACHE_SERVER_STARTED: &str =
    crate::domain::event_kind::EventKind::SccacheServerStarted.as_str();
pub const SCCACHE_SERVER_START_FAILED: &str =
    crate::domain::event_kind::EventKind::SccacheServerStartFailed.as_str();
pub const SCCACHE_WRAPPER_REMOVED: &str =
    crate::domain::event_kind::EventKind::SccacheWrapperRemoved.as_str();

/// The sccache a `[run.env]` names as cargo's wrapper, and the port of its
/// server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SccacheTarget {
    /// The value of `RUSTC_WRAPPER` as written (a name or a path).
    pub program: String,
    pub port: u16,
}

impl SccacheTarget {
    /// The target of an environment whose `RUSTC_WRAPPER` is sccache (its
    /// basename is `sccache`); `None` for any other wrapper or none. The
    /// port is `SCCACHE_SERVER_PORT` when it is a port, else sccache's
    /// default.
    pub fn of<'a>(env: impl IntoIterator<Item = (&'a str, &'a str)>) -> Option<Self> {
        let mut program = None;
        let mut port = None;
        for (key, value) in env {
            match key {
                WRAPPER_VAR => program = Some(value),
                PORT_VAR => port = Some(value),
                _ => {}
            }
        }
        let program = program?;
        if Path::new(program).file_name()? != "sccache" {
            return None;
        }
        Some(Self {
            program: program.to_owned(),
            port: port
                .and_then(|port| port.trim().parse().ok())
                .filter(|port| *port != 0)
                .unwrap_or(DEFAULT_PORT),
        })
    }

    /// [`Self::of`] owned pairs.
    pub fn of_pairs(env: &[(String, String)]) -> Option<Self> {
        Self::of(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
    }
}

/// Why the supervisor looked at the server (the `reason` of its events).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckReason {
    /// The supervisor's first look after it started (or was handed off).
    Startup,
    /// A later pass found no server.
    Missing,
    /// Before a Codex worker's workspace opens.
    BeforeWorker,
    /// Before a Codex run's `needs_session` resume opens.
    BeforeResume,
    /// Before a Codex job given `[run.env]` (the run's review) starts.
    BeforeReview,
}

impl CheckReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Missing => "missing",
            Self::BeforeWorker => "before_worker",
            Self::BeforeResume => "before_resume",
            Self::BeforeReview => "before_review",
        }
    }
}

/// What a look before a sandboxed turn or job found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerCheck {
    /// `[run.env]` names no sccache: nothing to do.
    NotConfigured,
    /// The server listens.
    Running,
    /// No server could be confirmed (none listens, none could be started,
    /// or the look failed): the turn or job runs without `RUSTC_WRAPPER`.
    Unconfirmed { port: u16, why: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(env: &[(&str, &str)]) -> Option<SccacheTarget> {
        SccacheTarget::of(env.iter().copied())
    }

    #[test]
    fn only_a_wrapper_named_sccache_is_a_target() {
        assert_eq!(target(&[]), None);
        assert_eq!(target(&[("RUSTC_WRAPPER", "ccache")]), None);
        assert_eq!(target(&[("RUSTC_WRAPPER", "")]), None);
        assert_eq!(target(&[("RUSTC_WRAPPER", "/opt/sccache-wrapper")]), None);
        assert_eq!(target(&[("CARGO_BUILD_JOBS", "4")]), None);
        assert_eq!(
            target(&[("RUSTC_WRAPPER", "sccache")]),
            Some(SccacheTarget {
                program: "sccache".into(),
                port: DEFAULT_PORT
            })
        );
        assert_eq!(
            target(&[
                ("SCCACHE_SERVER_PORT", "4300"),
                ("RUSTC_WRAPPER", "/home/me/.local/bin/sccache"),
            ]),
            Some(SccacheTarget {
                program: "/home/me/.local/bin/sccache".into(),
                port: 4300
            })
        );
        // A port sccache would not take is its default.
        for port in ["0", "x", "70000"] {
            assert_eq!(
                target(&[("RUSTC_WRAPPER", "sccache"), ("SCCACHE_SERVER_PORT", port)])
                    .unwrap()
                    .port,
                DEFAULT_PORT
            );
        }
        assert_eq!(
            SccacheTarget::of_pairs(&[("RUSTC_WRAPPER".into(), "sccache".into())])
                .unwrap()
                .program,
            "sccache"
        );
    }

    #[test]
    fn the_reasons_have_their_names() {
        let names: Vec<&str> = [
            CheckReason::Startup,
            CheckReason::Missing,
            CheckReason::BeforeWorker,
            CheckReason::BeforeResume,
            CheckReason::BeforeReview,
        ]
        .iter()
        .map(|reason| reason.as_str())
        .collect();
        assert_eq!(
            names,
            [
                "startup",
                "missing",
                "before_worker",
                "before_resume",
                "before_review"
            ]
        );
        assert_eq!(SCCACHE_SERVER_STARTED, "sccache_server_started");
        assert_eq!(SCCACHE_SERVER_START_FAILED, "sccache_server_start_failed");
        assert_eq!(SCCACHE_WRAPPER_REMOVED, "sccache_wrapper_removed");
    }
}
