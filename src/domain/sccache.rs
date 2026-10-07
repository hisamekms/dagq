//! The sccache server of a `[run.env]` whose `RUSTC_WRAPPER` is sccache
//! (ADR-t1215-1): the supervisor, outside any sandbox, starts and keeps
//! it; no other process the runtime gives `[run.env]` may (ADR-t2086-1),
//! sandboxed or not: each is refused the server's start, one whose server
//! could not be confirmed just before it starts runs without
//! `RUSTC_WRAPPER`, and one whose server was confirmed compiles through
//! the guard, which runs the compiler itself should the server stop after
//! the look ([`GuardLook`]).

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

/// Process identity: `pid` plus `started_at` distinguishes reused PIDs.
/// `started_at` is the host's C-locale `ps lstart` text, not a queue timestamp.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ServerProcess {
    pub pid: u32,
    pub started_at: String,
    pub parent_pid: u32,
    pub command: String,
    /// True only when the observed parent command establishes confinement.
    /// Unknown when it does not: a reparented daemon may keep its sandbox.
    pub sandboxed: Option<bool>,
    /// The start in Unix seconds, read from the process's elapsed time, to
    /// compare with a start record's `at` when the record has no
    /// `started_at` ([`started_near`]). Not part of the identity, and not
    /// written to events: `started_at` stays the identity's start.
    #[serde(skip)]
    pub started_unix: Option<i64>,
}

/// How far, in seconds either way, a server's start may be from a start
/// record's `at` and still be that record's start. The supervisor stamps
/// `at` just after `--start-server` returns (a fresh start: the server
/// started up to the host adapter's 30 s start timeout plus about 2 s of
/// reading the listener's pid before), or just before it stops the old
/// server (a restart: the new one starts up to the stop's 5 s query plus
/// its 30 s wait for the port after), and `ps etime` is whole seconds.
/// 120 s covers those 35 s with room for a slow host, while a PID reused
/// by another server on the same port within two minutes of the record is
/// too unlikely to tell apart.
pub const START_RECORD_WINDOW_SECS: i64 = 120;

/// Whether `process` started within [`START_RECORD_WINDOW_SECS`] of `at`
/// (Unix seconds). An unknown start time never matches.
pub fn started_near(process: &ServerProcess, at: i64) -> bool {
    process
        .started_unix
        .is_some_and(|start| (start - at).abs() <= START_RECORD_WINDOW_SECS)
}

/// Lifetime compile counters returned by sccache, not per-observation deltas.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ServerStats {
    pub requests: u64,
    pub failures: u64,
    pub compilations: u64,
}

impl ServerStats {
    pub fn failure_ratio(self) -> f64 {
        if self.requests == 0 {
            0.0
        } else {
            self.failures as f64 / self.requests as f64
        }
    }
}

/// Accumulate consecutive failure-only deltas. A first observation is a
/// baseline, never evidence that a previously healthy server is broken.
/// Three new failures without a success mark the server unhealthy. Success,
/// a changed identity or decreased counters reset the streak; idle samples
/// add nothing. No compile probe runs to obtain these counters.
#[derive(Default)]
pub struct FailureWatch {
    previous: Option<(ServerProcess, ServerStats)>,
    failures: u64,
}
impl FailureWatch {
    pub fn observe(&mut self, process: &ServerProcess, stats: ServerStats) -> bool {
        if let Some((old_process, old)) = &self.previous {
            if old_process.pid != process.pid
                || old_process.started_at != process.started_at
                || stats.requests < old.requests
                || stats.failures < old.failures
                || stats.compilations < old.compilations
            {
                self.failures = 0;
            } else {
                let requests = stats.requests - old.requests;
                let failures = stats.failures - old.failures;
                if stats.compilations > old.compilations || requests > failures {
                    self.failures = 0;
                } else {
                    self.failures = self.failures.saturating_add(failures);
                }
            }
        }
        self.previous = Some((process.clone(), stats));
        self.failures >= 3
    }
}

pub const DETECTED: &str = crate::domain::event_kind::EventKind::SccacheServerDetected.as_str();
pub const UNHEALTHY: &str = crate::domain::event_kind::EventKind::SccacheServerUnhealthy.as_str();
pub const RESTART_FAILED: &str =
    crate::domain::event_kind::EventKind::SccacheServerRestartFailed.as_str();
pub const HEALTH_KINDS: &[&str] = &[SCCACHE_SERVER_STARTED, DETECTED, UNHEALTHY, RESTART_FAILED];

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
    /// Before a worker's session opens, on either provider.
    BeforeWorker,
    /// Before a run's `needs_session` resume opens, on either provider.
    BeforeResume,
    /// Before the run's review job starts, on either provider.
    BeforeReview,
    /// Before a landing of the supervisor's runs its verification commands.
    BeforeIntegrate,
    /// Before the landing recheck runs its command.
    BeforeRecheck,
    /// Before the e2e of a run starts.
    BeforeE2e,
}

impl CheckReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Missing => "missing",
            Self::BeforeWorker => "before_worker",
            Self::BeforeResume => "before_resume",
            Self::BeforeReview => "before_review",
            Self::BeforeIntegrate => "before_integrate",
            Self::BeforeRecheck => "before_recheck",
            Self::BeforeE2e => "before_e2e",
        }
    }
}

/// What the supervisor's look before a process given `[run.env]` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerCheck {
    /// `[run.env]` names no sccache: nothing to do.
    NotConfigured,
    /// The server listens on `port`.
    Running { port: u16 },
    /// No server could be confirmed (none listens, none could be started,
    /// or the look failed): the turn or job runs without `RUSTC_WRAPPER`.
    Unconfirmed { port: u16, why: String },
}

/// The file name of the guard (ADR-t2086-1): a link to the `dagq` binary
/// that a process given `[run.env]` is given as `RUSTC_WRAPPER` in place
/// of sccache. `dagq` invoked by this name is the guard, not the CLI.
pub const GUARD_NAME: &str = "dagq-rustc-wrapper";
/// The variable that tells the guard the sccache `[run.env]` names.
pub const GUARD_PROGRAM_VAR: &str = "DAGQ_SCCACHE_PROGRAM";
/// The variable sccache's server, and nothing else of sccache, opens
/// first: its log, before it binds the port or leaves the client.
pub const ERROR_LOG_VAR: &str = "SCCACHE_ERROR_LOG";
/// A log no process can open (`/dev/null` is no directory), so a server
/// started with it exits before it listens: what every process the
/// runtime gives `[run.env]` naming sccache is given as [`ERROR_LOG_VAR`]
/// (ADR-t2086-1), so that no sccache it runs (its client re-executes
/// itself as the server, with its environment) starts one. The
/// supervisor's own start of the server is never given it.
pub const REFUSED_ERROR_LOG: &str = "/dev/null/dagq-refuses-the-sccache-server";

/// Whether the guard's argv\[0\] names it ([`GUARD_NAME`]).
pub fn invoked_as_guard(argv0: &std::ffi::OsStr) -> bool {
    Path::new(argv0).file_name() == Some(std::ffi::OsStr::new(GUARD_NAME))
}

/// The variables a process of `target` runs with beside its `[run.env]`:
/// always the [`REFUSED_ERROR_LOG`], and, with `guard` (the link to the
/// guard, made when its server was confirmed), the guard as
/// `RUSTC_WRAPPER` and the sccache it compiles through. Without `guard`
/// the caller takes `RUSTC_WRAPPER` out.
pub fn guarded_env(target: &SccacheTarget, guard: Option<&str>) -> Vec<(String, String)> {
    let mut env = vec![(ERROR_LOG_VAR.to_owned(), REFUSED_ERROR_LOG.to_owned())];
    if let Some(guard) = guard {
        env.push((WRAPPER_VAR.to_owned(), guard.to_owned()));
        env.push((GUARD_PROGRAM_VAR.to_owned(), target.program.clone()));
    }
    env
}

/// What the look just before a process the runtime gives `[run.env]`
/// found (ADR-t2086-1), for every such process alike: a worker's turn and
/// resume and the run's review on either provider, integrate's
/// verification commands, the landing recheck and the e2e.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardLook {
    /// Nothing looks at the server here (no sccache in `[run.env]`, or no
    /// port to look with): `[run.env]` is given as it is.
    NotConfigured,
    /// The server listened and the guard was made at this path.
    Guard(String),
    /// The server on `port` could not be confirmed, or the guard not
    /// made: the process runs without `RUSTC_WRAPPER`, which its caller
    /// records as `sccache_wrapper_removed` with `why` as its `reason`.
    Unconfirmed { port: u16, why: String },
}

impl GuardLook {
    /// The variables a process of `target` runs without and with beside
    /// its `[run.env]`: refused the server's start unless nothing looked,
    /// through the guard when there is one, else without `RUSTC_WRAPPER`.
    pub fn vars(&self, target: &SccacheTarget) -> (&'static [&'static str], Vec<(String, String)>) {
        match self {
            Self::NotConfigured => (&[], Vec::new()),
            Self::Guard(guard) => (&[], guarded_env(target, Some(guard))),
            Self::Unconfirmed { .. } => (&[WRAPPER_VAR], guarded_env(target, None)),
        }
    }

    /// [`Self::vars`] put into `env` (a `[run.env]`), which names the
    /// sccache it is about; one that names none is left alone. Returns the
    /// variables the process must not inherit either.
    pub fn apply(&self, env: &mut Vec<(String, String)>) -> &'static [&'static str] {
        let Some(target) = SccacheTarget::of_pairs(env) else {
            return &[];
        };
        let (without, with) = self.vars(&target);
        env.retain(|(key, _)| !without.contains(&key.as_str()));
        for (key, value) in with {
            env.retain(|(name, _)| *name != key);
            env.push((key, value));
        }
        without
    }

    /// The port and why, when the process runs without `RUSTC_WRAPPER`.
    pub fn removed(&self) -> Option<(u16, &str)> {
        match self {
            Self::Unconfirmed { port, why } => Some((*port, why)),
            _ => None,
        }
    }
}

/// What the guard runs a compile with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardStep {
    /// The compiler itself: no server to reach, or the sccache client
    /// failed (its start of the server refused). Uncached but correct.
    Compiler,
    /// sccache, which compiles through the server that listens.
    Sccache,
}

impl GuardStep {
    /// The first step: sccache only when the guard knows it and its server
    /// listens now. A server that stops after this look is the one case
    /// where the client tries to start one, and the start is refused.
    pub fn first(program_known: bool, listening: bool) -> Self {
        if program_known && listening {
            Self::Sccache
        } else {
            Self::Compiler
        }
    }

    /// After sccache exited: whether the compiler runs instead. Only when
    /// sccache failed and said its own error (`sccache: error:`, as its
    /// client reports a failed connect or start); a compile the compiler
    /// failed is the compiler's exit, said by the compiler.
    pub fn after_sccache(success: bool, stderr: &[u8]) -> Option<Self> {
        let client_failed = !success
            && stderr
                .split(|byte| *byte == b'\n')
                .any(|line| line.starts_with(b"sccache: error:"));
        client_failed.then_some(Self::Compiler)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(env: &[(&str, &str)]) -> Option<SccacheTarget> {
        SccacheTarget::of(env.iter().copied())
    }

    #[test]
    fn failures_need_new_samples_and_reset_on_success_identity_or_zeroed_stats() {
        let mut process = ServerProcess {
            pid: 1,
            started_at: "first".into(),
            parent_pid: 0,
            command: "sccache".into(),
            sandboxed: None,
            started_unix: None,
        };
        let stats = |requests, failures, compilations| ServerStats {
            requests,
            failures,
            compilations,
        };
        let mut watch = FailureWatch::default();
        assert!(!watch.observe(&process, stats(7, 7, 0))); // historical failures are baseline
        assert!(!watch.observe(&process, stats(7, 7, 0))); // idle is no evidence
        assert!(!watch.observe(&process, stats(9, 9, 0)));
        assert!(!watch.observe(&process, stats(10, 9, 1))); // success clears the streak
        assert!(!watch.observe(&process, stats(12, 11, 1)));
        assert!(watch.observe(&process, stats(13, 12, 1)));
        process.started_at = "reused pid".into();
        assert!(!watch.observe(&process, stats(13, 12, 1)));
        assert!(!watch.observe(&process, stats(0, 0, 0))); // --zero-stats
        assert!(watch.observe(&process, stats(3, 3, 0)));
        assert_eq!(stats(0, 0, 0).failure_ratio(), 0.0);
        assert_eq!(stats(7, 7, 0).failure_ratio(), 1.0);
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
    fn a_sandboxed_env_refuses_the_servers_start_and_guards_a_confirmed_server() {
        let target = SccacheTarget {
            program: "/opt/bin/sccache".into(),
            port: 4300,
        };
        let refused = (ERROR_LOG_VAR.to_owned(), REFUSED_ERROR_LOG.to_owned());
        assert_eq!(guarded_env(&target, None), std::slice::from_ref(&refused));
        assert_eq!(
            guarded_env(&target, Some("/q/runs/r/dagq-rustc-wrapper")),
            [
                refused,
                (
                    "RUSTC_WRAPPER".to_owned(),
                    "/q/runs/r/dagq-rustc-wrapper".to_owned()
                ),
                (
                    "DAGQ_SCCACHE_PROGRAM".to_owned(),
                    "/opt/bin/sccache".to_owned()
                ),
            ]
        );
        // The log is no file anyone can open.
        assert!(REFUSED_ERROR_LOG.starts_with("/dev/null/"));
    }

    #[test]
    fn every_look_refuses_the_start_and_only_a_confirmed_one_keeps_a_wrapper() {
        let run_env = || {
            vec![
                ("RUSTC_WRAPPER".to_owned(), "/opt/bin/sccache".to_owned()),
                ("SCCACHE_SERVER_PORT".to_owned(), "4300".to_owned()),
                ("CARGO_BUILD_JOBS".to_owned(), "4".to_owned()),
            ]
        };
        let refused = (ERROR_LOG_VAR.to_owned(), REFUSED_ERROR_LOG.to_owned());
        // Confirmed: through the guard, refused the start.
        let mut env = run_env();
        let guard = GuardLook::Guard("/q/runs/r/dagq-rustc-wrapper".into());
        assert!(guard.apply(&mut env).is_empty());
        assert_eq!(
            env,
            [
                ("SCCACHE_SERVER_PORT".to_owned(), "4300".to_owned()),
                ("CARGO_BUILD_JOBS".to_owned(), "4".to_owned()),
                refused.clone(),
                (
                    "RUSTC_WRAPPER".to_owned(),
                    "/q/runs/r/dagq-rustc-wrapper".to_owned()
                ),
                (
                    "DAGQ_SCCACHE_PROGRAM".to_owned(),
                    "/opt/bin/sccache".to_owned()
                ),
            ]
        );
        assert_eq!(guard.removed(), None);
        // Unconfirmed: no wrapper (nor an inherited one), refused the start.
        let mut env = run_env();
        let unconfirmed = GuardLook::Unconfirmed {
            port: 4300,
            why: "no sccache server listens on port 4300".into(),
        };
        assert_eq!(unconfirmed.apply(&mut env), ["RUSTC_WRAPPER"]);
        assert_eq!(
            env,
            [
                ("SCCACHE_SERVER_PORT".to_owned(), "4300".to_owned()),
                ("CARGO_BUILD_JOBS".to_owned(), "4".to_owned()),
                refused,
            ]
        );
        assert_eq!(
            unconfirmed.removed(),
            Some((4300, "no sccache server listens on port 4300"))
        );
        // Nothing looked, or no sccache named: as it is.
        let mut env = run_env();
        assert!(GuardLook::NotConfigured.apply(&mut env).is_empty());
        assert_eq!(env, run_env());
        let mut other = vec![("RUSTC_WRAPPER".to_owned(), "ccache".to_owned())];
        assert!(unconfirmed.apply(&mut other).is_empty());
        assert_eq!(other, [("RUSTC_WRAPPER".to_owned(), "ccache".to_owned())]);
    }

    #[test]
    fn the_guard_is_known_by_its_name_only() {
        use std::ffi::OsStr;
        assert!(invoked_as_guard(OsStr::new("dagq-rustc-wrapper")));
        assert!(invoked_as_guard(OsStr::new("/q/runs/r/dagq-rustc-wrapper")));
        assert!(!invoked_as_guard(OsStr::new("dagq")));
        assert!(!invoked_as_guard(OsStr::new("/usr/local/bin/dagq")));
        assert!(!invoked_as_guard(OsStr::new("dagq-rustc-wrapper/dagq")));
        assert!(!invoked_as_guard(OsStr::new("")));
    }

    #[test]
    fn the_guard_compiles_through_sccache_only_while_its_server_listens() {
        assert_eq!(GuardStep::first(true, true), GuardStep::Sccache);
        assert_eq!(GuardStep::first(true, false), GuardStep::Compiler);
        assert_eq!(GuardStep::first(false, true), GuardStep::Compiler);
        assert_eq!(GuardStep::first(false, false), GuardStep::Compiler);
    }

    #[test]
    fn the_compiler_runs_again_only_after_the_clients_own_failure() {
        // The start of a server was refused: the client's error.
        let refused = b"sccache: error: Timed out waiting for server startup. Maybe the remote service is unreachable?\nRun with SCCACHE_LOG=debug SCCACHE_NO_DAEMON=1 to get more information\n";
        assert_eq!(
            GuardStep::after_sccache(false, refused),
            Some(GuardStep::Compiler)
        );
        assert_eq!(
            GuardStep::after_sccache(
                false,
                b"warning: x\nsccache: error: Server startup failed: x\n"
            ),
            Some(GuardStep::Compiler)
        );
        // The compiler's own failure, through the server, is the compile's.
        assert_eq!(
            GuardStep::after_sccache(false, b"error[E0425]: cannot find value `x`\n"),
            None
        );
        // A line that only mentions sccache is not its error.
        assert_eq!(
            GuardStep::after_sccache(false, b"error: see sccache: error: in the log\n"),
            None
        );
        assert_eq!(GuardStep::after_sccache(true, refused), None);
        assert_eq!(GuardStep::after_sccache(false, b""), None);
    }

    #[test]
    fn the_reasons_have_their_names() {
        let names: Vec<&str> = [
            CheckReason::Startup,
            CheckReason::Missing,
            CheckReason::BeforeWorker,
            CheckReason::BeforeResume,
            CheckReason::BeforeReview,
            CheckReason::BeforeIntegrate,
            CheckReason::BeforeRecheck,
            CheckReason::BeforeE2e,
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
                "before_review",
                "before_integrate",
                "before_recheck",
                "before_e2e"
            ]
        );
        assert_eq!(SCCACHE_SERVER_STARTED, "sccache_server_started");
        assert_eq!(SCCACHE_SERVER_START_FAILED, "sccache_server_start_failed");
        assert_eq!(SCCACHE_WRAPPER_REMOVED, "sccache_wrapper_removed");
    }
}
